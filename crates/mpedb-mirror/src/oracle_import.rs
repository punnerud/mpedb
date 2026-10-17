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
mod staging;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_ledger: Option<crate::oracle_source::OracleOriginLedgerIdentity>,
}
impl OracleBootstrap {
    fn validate(&self) -> Result<()> {
        if self.version != 3
            || self.state_identity.incarnation == [0; 16]
            || self.state_identity.object_id == 0
            || self.state_identity.data_object_id == 0
            || self.epoch != 1
            || self.tables.is_empty()
            || self.mirror_id.len() != 32
        {
            return Err(Error::Corrupt("invalid Oracle bootstrap manifest".into()));
        }
        if self.origin_ledger.as_ref().is_some_and(|ledger| {
            ledger.object_id == 0 || ledger.data_object_id == 0 || ledger.schema_ddl.is_empty()
        }) {
            return Err(Error::Corrupt(
                "invalid Oracle bootstrap origin ledger".into(),
            ));
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
    let expected = oracle::build_schema(&metadata.tables)?;
    if cfg.scope != (0..expected.tables.len() as u32).collect::<Vec<_>>() {
        return Err(Error::Corrupt(
            "Oracle bootstrap table-id scope changed".into(),
        ));
    }
    // rRETL lineage, maps and local tables may extend the MPEdb schema. Only
    // the pinned carrier scope belongs to this source; its ids and full table
    // definitions must still match exactly.
    let bundle = db.schema();
    let carriers = cfg
        .scope
        .iter()
        .map(|&id| {
            let table = bundle
                .tables
                .get(id as usize)
                .ok_or_else(|| Error::Corrupt("Oracle carrier table is missing".into()))?;
            if table.id != id || table.dead {
                return Err(Error::Corrupt(
                    "Oracle carrier table identity changed".into(),
                ));
            }
            Ok(table.clone())
        })
        .collect::<Result<Vec<_>>>()?;
    let actual = mpedb_types::Schema::new(carriers)?;
    if cfg.source_kind != state::SourceKind::Oracle
        || mirror_name(cfg.mirror_id) != metadata.mirror_id
        || expected.hash() != actual.hash()
    {
        return Err(Error::Corrupt(
            "Oracle bootstrap does not match its MPEdb carrier".into(),
        ));
    }
    Ok(metadata)
}
/// Durable per-FILE identity for outbound request generations. Unlike a path
/// or transaction counter, this changes on every fresh import and survives move.
pub fn load_file_incarnation(db: &Database) -> Result<[u8; 16]> {
    let bytes = db
        .sys_record_get(state::MIR_NS, b"ora/file-incarnation")?
        .ok_or_else(|| {
            Error::Unsupported(
                "Oracle mirror has no file incarnation; reimport before enabling write-back".into(),
            )
        })?;
    let id: [u8; 16] = bytes
        .try_into()
        .map_err(|_| Error::Corrupt("invalid Oracle file incarnation length".into()))?;
    if id == [0; 16] {
        return Err(Error::Corrupt("zero Oracle file incarnation".into()));
    }
    Ok(id)
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
    match dest.symlink_metadata() {
        Ok(_) => {
            return Err(Error::Config(
                "Oracle import destination already exists".into(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
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
    let mut ledger = source.existing_write_ledger()?;
    if ledger.is_some() {
        exclude.push("MPEDB_WRITE_RECEIPTS".into());
    }
    let mut origins = source.existing_origin_ledger()?;
    if origins.is_some() {
        exclude.push("MPEDB_WRITE_ORIGINS".into());
    }
    let mut tables = source.introspect(opts.include.as_deref(), &exclude)?;
    if tables.iter().any(|t| t.name == "MPEDB_WRITE_ORIGINS") {
        origins = Some(source.prepare_origin_ledger()?);
        tables.retain(|t| t.name != "MPEDB_WRITE_ORIGINS");
    }
    if tables.iter().any(|t| t.name == "MPEDB_WRITE_RECEIPTS") {
        // A concurrent controller may have installed tracking after the first
        // lookup. Verify ownership before removing it from application scope.
        ledger = Some(source.prepare_write_ledger()?);
        tables.retain(|t| t.name != "MPEDB_WRITE_RECEIPTS");
    }
    if tables.is_empty() {
        return Err(Error::Config(
            "no mirrorable Oracle tables in source".into(),
        ));
    }
    let tables = oracle::carrier_tables(&tables)?
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let schema = oracle::build_schema(&tables)?;
    let nonce = source.import_incarnation()?;
    let stage = staging::Staging::new(dest, nonce)?;
    let db = import::create_mirror_db(
        &stage.path(),
        schema.clone(),
        opts.size_bytes,
        opts.durability,
        mpedb_types::Dialect::Postgres,
    )?;
    let mut report = ImportReport {
        tables: tables
            .iter()
            .enumerate()
            .map(|(id, t)| TableImportStat {
                name: t.name.clone(),
                table_id: id as u32,
                rows: 0,
            })
            .collect(),
        ..Default::default()
    };
    let types = schema
        .tables
        .iter()
        .map(|t| t.columns.iter().map(|c| c.ty).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut current = None;
    let snapshot = source.stream_mirror_snapshot(
        &tables,
        &mirror_id,
        ledger.as_ref(),
        origins.as_ref(),
        &mut |id, row| {
            if current != Some(id) {
                if let Some(previous) = current {
                    if !batch.is_empty() {
                        import::flush_batch(
                            &db,
                            previous,
                            &tables[previous as usize].pk,
                            &mut batch,
                        )?;
                    }
                }
                current = Some(id);
                batch_bytes = 0;
            }
            let size = mpedb_core::row::encoded_len(&row, &types[id as usize]);
            if !batch.is_empty()
                && (batch.len() >= opts.batch_rows
                    || batch_bytes.saturating_add(size) > 8 * 1024 * 1024)
            {
                import::flush_batch(&db, id, &tables[id as usize].pk, &mut batch)?;
                batch_bytes = 0;
            }
            batch_bytes = batch_bytes.saturating_add(size);
            batch.push(row);
            report.tables[id as usize].rows += 1;
            Ok(())
        },
    )?;
    if let Some(id) = current {
        if !batch.is_empty() {
            import::flush_batch(&db, id, &tables[id as usize].pk, &mut batch)?;
        }
    }
    if snapshot.source_state != Some((1, "source".into())) {
        return Err(Error::Config(
            "Oracle import requires an initial source-authoritative epoch".into(),
        ));
    }
    let metadata = OracleBootstrap {
        version: 3,
        origin_ledger: origins,
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
    import::publish_mirror_state_with(&db, &schema, state::SourceKind::Oracle, &maps, |s| {
        let mut header = vec![1];
        header.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        header.extend_from_slice(blake3::hash(&bytes).as_bytes());
        for (n, chunk) in bytes.chunks(CHUNK).enumerate() {
            s.sys_record_put(state::MIR_NS, &chunk_key(n), chunk)?;
        }
        s.sys_record_put(state::MIR_NS, META, &header)?;
        s.sys_record_put(state::MIR_NS, b"ora/file-incarnation", &nonce)?;
        let cfg = s
            .sys_record_get(state::MIR_NS, state::KEY_CFG)?
            .ok_or_else(|| Error::Corrupt("Oracle import config publication is missing".into()))?;
        let mut cfg = state::MirrorConfig::decode(&cfg)?;
        cfg.mirror_id = import::mirror_id_for(dest);
        s.sys_record_put(state::MIR_NS, state::KEY_CFG, &cfg.encode())?;
        s.sys_record_put(state::MIR_NS, state::KEY_CUR, &metadata.cursor)
    })?;
    drop(db);
    stage.publish()?;
    let db = Database::open_from_file(dest)?;
    // The trusted applier exemption is per connection, not persisted tuning.
    db.set_fk_enforced(false);
    Ok((db, report))
}
