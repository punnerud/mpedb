//! Oracle bootstrap into MPEdb. No Oracle-to-PostgreSQL conversion takes place:
//! original types and DDL are retained beside the lossless MPEdb carriers.
use crate::{
    import::{self, ImportOptions, ImportReport, TableImportStat},
    oracle::{self, OracleTable},
    oracle_cursor::{OracleIdentity, OraclePosition},
    oracle_source::{OracleSource, OracleStateIdentity},
    state,
};
use mpedb::Database;
use mpedb_types::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
const META: &[u8] = b"ora/bootstrap";
const CHUNK: usize = 65536;
const LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleBootstrap {
    pub version: u32,
    pub identity: OracleIdentity,
    pub tables: Vec<OracleTable>,
    pub cursor: Vec<u8>,
    pub mirror_id: String,
    pub epoch: u64,
    pub state_identity: OracleStateIdentity,
}
impl OracleBootstrap {
    fn validate(&self) -> Result<()> {
        if self.version != 2
            || self.state_identity.object_id == 0
            || self.state_identity.data_object_id == 0
            || self.epoch != 1
            || self.tables.is_empty()
            || self.mirror_id.len() != 32
        {
            return Err(Error::Corrupt("invalid Oracle bootstrap manifest".into()));
        }
        OraclePosition::decode(&self.cursor, &self.identity, &self.tables)?;
        Ok(())
    }
}
fn chunk_key(n: usize) -> Vec<u8> {
    format!("ora/bootstrap/{n:08x}").into_bytes()
}
/// Read and verify source metadata without contacting Oracle. No passwords or
/// connect descriptors are persisted in this manifest.
pub fn load_bootstrap(db: &Database) -> Result<OracleBootstrap> {
    let header = db
        .sys_record_get(state::MIR_NS, META)?
        .ok_or_else(|| Error::Config("Oracle bootstrap metadata is missing".into()))?;
    if header.len() != 41 || header[0] != 1 {
        return Err(Error::Corrupt("invalid Oracle bootstrap header".into()));
    }
    let length = u64::from_be_bytes(header[1..9].try_into().unwrap());
    if length == 0 || length > LIMIT as u64 {
        return Err(Error::Corrupt(
            "Oracle bootstrap metadata length exceeds bounds".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    for n in 0..(length as usize).div_ceil(CHUNK) {
        let chunk = db
            .sys_record_get(state::MIR_NS, &chunk_key(n))?
            .ok_or_else(|| Error::Corrupt("Oracle bootstrap chunk is missing".into()))?;
        let expected = CHUNK.min(length as usize - bytes.len());
        if chunk.len() != expected {
            return Err(Error::Corrupt(
                "invalid Oracle bootstrap chunk length".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if blake3::hash(&bytes).as_bytes() != &header[9..] {
        return Err(Error::Corrupt(
            "Oracle bootstrap metadata hash mismatch".into(),
        ));
    }
    let metadata: OracleBootstrap = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Corrupt("invalid Oracle bootstrap metadata encoding".into()))?;
    metadata.validate()?;
    let cfg = db
        .sys_record_get(state::MIR_NS, state::KEY_CFG)?
        .ok_or_else(|| Error::Corrupt("Oracle mirror config is missing".into()))?;
    let cfg = state::MirrorConfig::decode(&cfg)?;
    if cfg.source_kind != state::SourceKind::Oracle
        || mirror_name(cfg.mirror_id) != metadata.mirror_id
        || oracle::build_schema(&metadata.tables)?.hash() != db.schema().hash()
    {
        return Err(Error::Corrupt(
            "Oracle bootstrap does not match its MPEdb carrier".into(),
        ));
    }
    Ok(metadata)
}
fn mirror_name(id: [u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Import one consistent Oracle SCN into a new file. Authority metadata and
/// cursor are published atomically only after every row has been imported.
/// Bootstrap undo/DDL failures publish no mirror and never fall back to NOW.
pub fn import_oracle(
    source: &mut OracleSource,
    dest: &Path,
    opts: &ImportOptions,
) -> Result<(Database, ImportReport)> {
    if dest.exists() {
        return Err(Error::Config(
            "Oracle import destination already exists".into(),
        ));
    }
    if opts.batch_rows == 0 || opts.adapt.is_some() {
        return Err(Error::Config(
            "Oracle import requires positive batch_rows and lossless carriers".into(),
        ));
    }
    let mirror_id = mirror_name(import::mirror_id_for(dest));
    // Verify ownership/shape before excluding our table. A business-table name
    // collision is an error, never an excuse to silently skip its contents.
    source.ensure_source_state(&mirror_id, 1, "source")?;
    let mut exclude = opts.exclude.clone();
    exclude.push("MPEDB_MIRROR_STATE".into());
    let tables = source.introspect(opts.include.as_deref(), &exclude)?;
    if tables.is_empty() {
        return Err(Error::Config(
            "no mirrorable Oracle tables in source".into(),
        ));
    }
    let snapshot = source.snapshot_for_mirror(&tables, &mirror_id)?;
    if snapshot.source_state != Some((1, "source".into())) {
        return Err(Error::Config(
            "Oracle import requires an initial source-authoritative epoch".into(),
        ));
    }
    let metadata = OracleBootstrap {
        version: 2,
        cursor: snapshot.position()?.encode(),
        identity: snapshot.identity,
        tables: snapshot.tables,
        mirror_id,
        epoch: 1,
        state_identity: snapshot.source_state_identity.ok_or_else(|| {
            Error::Corrupt("Oracle bootstrap has no control-table identity".into())
        })?,
    };
    metadata.validate()?;
    let bytes = serde_json::to_vec(&metadata)
        .map_err(|_| Error::Config("Oracle metadata serialization failed".into()))?;
    if bytes.len() > LIMIT {
        return Err(Error::Unsupported(
            "Oracle bootstrap metadata exceeds 64 MiB".into(),
        ));
    }
    let schema = oracle::build_schema(&metadata.tables)?;
    let maps = metadata
        .tables
        .iter()
        .enumerate()
        .map(|(id, t)| {
            let columns = t
                .columns
                .iter()
                .map(|c| {
                    Ok(state::ColumnMap {
                        source_name: c.name.clone(),
                        source_type: c.declared_type(),
                        not_null: c.not_null,
                        generated: c.generated,
                        identity: c.identity,
                        unique: false,
                        mapped: c
                            .mapped()
                            .ok_or_else(|| Error::Unsupported("unmapped Oracle type".into()))?,
                        policy: c
                            .policy()
                            .ok_or_else(|| Error::Unsupported("unmapped Oracle type".into()))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((
                id as u32,
                state::TableMap {
                    source_name: t.name.clone(),
                    columns,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let db = import::create_mirror_db(
        dest,
        schema.clone(),
        opts.size_bytes,
        opts.durability,
        mpedb_types::Dialect::Postgres,
    )?;
    let mut report = ImportReport::default();
    for (id, rows) in snapshot.rows.into_iter().enumerate() {
        let count = rows.len() as u64;
        let mut batch = Vec::new();
        for row in rows {
            batch.push(row);
            if batch.len() >= opts.batch_rows {
                import::flush_batch(&db, id as u32, &metadata.tables[id].pk, &mut batch)?;
            }
        }
        if !batch.is_empty() {
            import::flush_batch(&db, id as u32, &metadata.tables[id].pk, &mut batch)?;
        }
        report.tables.push(TableImportStat {
            name: metadata.tables[id].name.clone(),
            table_id: id as u32,
            rows: count,
        });
    }
    import::publish_mirror_state_with(&db, &schema, state::SourceKind::Oracle, &maps, |s| {
        let mut header = vec![1];
        header.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        header.extend_from_slice(blake3::hash(&bytes).as_bytes());
        for (n, chunk) in bytes.chunks(CHUNK).enumerate() {
            s.sys_record_put(state::MIR_NS, &chunk_key(n), chunk)?;
        }
        s.sys_record_put(state::MIR_NS, META, &header)?;
        s.sys_record_put(state::MIR_NS, state::KEY_CUR, &metadata.cursor)
    })?;
    Ok((db, report))
}
