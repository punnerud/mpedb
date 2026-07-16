//! Export a mirrored `.mpedb` into a fresh PostgreSQL schema — the piece that
//! makes `sqlite3 → mpedb → PostgreSQL` an actual migration rather than a
//! diagram.
//!
//! Two things make this more than "CREATE TABLE + INSERT":
//!
//! **Provenance-faithful DDL — but only within one dialect.** mpedb's type set
//! is deliberately small (six types), so a naive reverse mapping flattens the
//! source: an `int4` and an `int8` are both `Int64` on the way in, and both
//! would come back out as `bigint`; a `varchar(20)` would come back as `text`.
//! Anything narrow when it left is silently widened when it returns. So for a
//! **PostgreSQL-sourced** mirror the recorded declared type — *with* its
//! typmod — is emitted verbatim, and PG -> mpedb -> PG reproduces the schema
//! exactly.
//!
//! For a **sqlite-sourced** mirror the recorded types are deliberately NOT
//! reused, even though they look like valid PG types. That is the trap: the
//! words collide across dialects but the meanings do not. sqlite's `INTEGER` is
//! 64-bit where PG's `integer` is int4, and sqlite's `REAL` is a double where
//! PG's `real` is single precision. Emitting them verbatim produced a target
//! that rejected any value above 2^31 and silently rounded every float to ~7
//! digits. A sqlite source therefore gets the generic widest mapping, which is
//! the only honest reading of an affinity.
//!
//! **Pre-flight before the first write.** PostgreSQL is strict where mpedb (and
//! especially sqlite) is not: a value that fits `Int64` need not fit `int4`, and
//! a text value holding a NUL byte is legal in mpedb and rejected outright by
//! PG. Discovering that on row 400k of a load, half-written, is the worst
//! possible time. [`export_pg`] therefore runs [`crate::preflight`] first and
//! refuses to start if anything would fail — the whole point of recording the
//! source schema in the first place.

use std::path::Path;

use mpedb::Database;
use mpedb_core::Engine;
use mpedb_types::{ColumnType, Error, Result, Schema, Value};
use postgres::Client;

use crate::export::ExportStat;
use crate::state::{self, SourceKind, TableMap};

/// Summary of a PostgreSQL export.
#[derive(Clone, Debug, Default)]
pub struct PgExportReport {
    pub tables: Vec<ExportStat>,
    /// Tables emitted with generic (widened) types rather than the source's own.
    /// Either the mirror carried no `mir/map` record, or it did but the source
    /// was not PostgreSQL — sqlite's type names are affinities and mean
    /// different things, so reusing them here would corrupt rather than
    /// preserve (see the module header).
    pub widened: Vec<String>,
}

impl PgExportReport {
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// The generic mpedb → PostgreSQL type mapping, used only when a column has no
/// recorded source type. Every choice here is the *widest* safe option, because
/// without provenance we do not know what the value came from and a too-narrow
/// guess would reject good data.
fn generic_pg_type(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Int64 => "bigint",
        ColumnType::Float64 => "double precision",
        ColumnType::Bool => "boolean",
        ColumnType::Text => "text",
        ColumnType::Blob => "bytea",
        ColumnType::Timestamp => "timestamptz",
        // Unreachable via `mirror push`: preflight refuses an `any` column against
        // a PostgreSQL target (FindingKind::AnyColumn) before any DDL is
        // generated. If it ever gets here, the check was bypassed — emit
        // something PostgreSQL will REJECT rather than silently pick a type and
        // make the schema depend on today's data.
        ColumnType::Any => "\"any\" -- mpedb: no PostgreSQL equivalent; see preflight",
    }
}

/// A typed NULL to bind; PG infers the column type from the target.
static NULL_I64: Option<i64> = None;

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `postgres::Error` Display is "db error" — the real message is in the source
/// chain (see `pg_adapter::pgerr`, same reason).
fn pgerr(ctx: &str, e: postgres::Error) -> Error {
    use std::error::Error as _;
    let mut msg = format!("{ctx}: {e}");
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    Error::Config(msg)
}

/// Options for [`export_pg`].
pub struct PgExportOptions {
    /// Target PG schema; created if absent.
    pub schema: String,
    /// Skip the pre-flight gate. Off by default, and there is no CLI surface
    /// for turning it on lightly: the gate is the feature.
    pub skip_preflight: bool,
}

impl Default for PgExportOptions {
    fn default() -> Self {
        PgExportOptions {
            schema: "public".into(),
            skip_preflight: false,
        }
    }
}

/// Column DDL for one column, preferring the recorded source type.
fn column_ddl(
    name: &str,
    ct: ColumnType,
    nullable: bool,
    unique: bool,
    map: Option<&TableMap>,
    kind: SourceKind,
) -> String {
    let ty = declared_pg_type(name, ct, map, kind)
        .unwrap_or_else(|| generic_pg_type(ct).to_string());
    let mut def = format!("{} {}", q(name), ty);
    if !nullable {
        def.push_str(" NOT NULL");
    }
    if unique {
        def.push_str(" UNIQUE");
    }
    def
}

/// The PG type to emit for a column: the recorded source type only when the
/// source WAS PostgreSQL. See the module header for why a sqlite `INTEGER` must
/// not be echoed into a PG `integer`.
fn declared_pg_type(
    name: &str,
    _ct: ColumnType,
    map: Option<&TableMap>,
    kind: SourceKind,
) -> Option<String> {
    if kind != SourceKind::Postgres {
        return None;
    }
    // Match provenance by source column name; a mirror's column names are the
    // source's, so this is exact rather than positional (scope filters and
    // future column drops make position unreliable).
    map.and_then(|m| m.columns.iter().find(|c| c.source_name == name))
        .map(|c| c.source_type.clone())
}

/// Bind an mpedb value as a PG parameter.
///
/// `Timestamp` is micros-since-epoch in mpedb; hand PG an interval-from-epoch
/// expression rather than a formatted string so no timezone parsing is involved
/// on either side.
enum PgParam {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Blob(Vec<u8>),
    /// micros since the unix epoch
    Micros(i64),
}

fn to_param(v: &Value) -> PgParam {
    match v {
        Value::Null => PgParam::Null,
        Value::Int(i) => PgParam::Int(*i),
        Value::Float(f) => PgParam::Float(*f),
        Value::Bool(b) => PgParam::Bool(*b),
        Value::Text(s) => PgParam::Text(s.clone()),
        Value::Blob(b) => PgParam::Blob(b.clone()),
        Value::Timestamp(us) => PgParam::Micros(*us),
        // A context list (§2.6) is param-only and cannot be stored, so nothing
        // read out of a column is one.
        Value::List(_) => PgParam::Null,
    }
}

/// Export the mirror at `mpedb_path` into `client`'s database.
pub fn export_pg(
    mpedb_path: &Path,
    client: &mut Client,
    opts: &PgExportOptions,
) -> Result<PgExportReport> {
    // Gate BEFORE any DDL or data: refusing to start beats a half-loaded target.
    // The Database handle exists only for this — the export itself reads through
    // Engine so it can stream rather than materialise a table at a time.
    if !opts.skip_preflight {
        let db = Database::open_from_file(mpedb_path)?;
        let pf = crate::preflight(&db)?;
        if pf.would_fail() {
            // LossyColumn is a judgement call, not a rejection — same rule the
            // report itself uses, so the gate and the report never disagree.
            let bad: Vec<&crate::Finding> = pf
                .findings
                .iter()
                .filter(|f| f.kind != crate::FindingKind::LossyColumn)
                .collect();
            let n = bad.len();
            let mut detail = String::new();
            for f in bad.iter().take(10) {
                detail.push_str(&format!("\n  {}.{}: {}", f.table, f.column, f.detail));
            }
            return Err(Error::Config(format!(
                "refusing to export: {n} value(s) would be rejected by the target \
                 schema.{detail}\nRun `mpedb mirror preflight --db <file>` for the full \
                 report, and `--adapt` at import time to coerce what can be coerced."
            )));
        }
    }

    let eng = Engine::open_from_file(mpedb_path)?;
    let r = eng.begin_read()?;
    let schema: Schema = r.stored_schema()?;
    let src_kind = match r.sys_get(&state::sys_subkey(state::KEY_CFG))? {
        Some(raw) => state::MirrorConfig::decode(&raw)?.source_kind,
        None => return Err(Error::Config("not a mirror (no mir/cfg record)".into())),
    };
    let mut report = PgExportReport::default();

    client
        .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", q(&opts.schema)))
        .map_err(|e| pgerr("create schema", e))?;

    // One transaction for the whole export: a failed export must leave no
    // partial schema behind for someone to mistake for a finished one. PG's
    // transactional DDL makes this actually possible, unlike most targets.
    let mut tx = client.transaction().map_err(|e| pgerr("begin", e))?;

    for (table_id, t) in schema.tables.iter().enumerate() {
        let map: Option<TableMap> = r
            .sys_get(&state::sys_subkey(&state::map_key(table_id as u32)))?
            .map(|raw| TableMap::decode(&raw))
            .transpose()?;
        // "no usable provenance" is not the same as "no provenance record": a
        // sqlite mirror HAS a record, but its types are not PG's to reuse.
        if map.is_none() || src_kind != SourceKind::Postgres {
            report.widened.push(t.name.clone());
        }

        let col_defs: Vec<String> = t
            .columns
            .iter()
            .map(|c| column_ddl(&c.name, c.ty, c.nullable, c.unique, map.as_ref(), src_kind))
            .collect();
        let pk_cols = t
            .primary_key
            .iter()
            .map(|&i| q(&t.columns[i as usize].name))
            .collect::<Vec<_>>()
            .join(", ");
        let qname = format!("{}.{}", q(&opts.schema), q(&t.name));
        let create = format!(
            "CREATE TABLE {qname} ({}, PRIMARY KEY ({pk_cols}))",
            col_defs.join(", ")
        );
        tx.batch_execute(&create)
            .map_err(|e| pgerr(&format!("create `{}`", t.name), e))?;

        // Bind + cast. Faithful narrow DDL forces this: rust-postgres sends an
        // i64 as int8 and PG will not implicitly narrow int8 -> int4, so a
        // provenance-recreated int4 column rejects every row. An explicit cast
        // to the column's own declared type is the fix, and it is safe here
        // precisely because preflight already proved every value fits that type
        // -- without that gate this cast would be silently truncating instead.
        // Timestamp is micros-since-epoch in mpedb; let PG build the instant so
        // no timezone text parsing is involved on either side.
        let mut vals = Vec::with_capacity(t.columns.len());
        for (i, c) in t.columns.iter().enumerate() {
            let n = i + 1;
            let declared = declared_pg_type(&c.name, c.ty, map.as_ref(), src_kind);
            vals.push(match c.ty {
                ColumnType::Timestamp => format!(
                    "(timestamptz 'epoch' + (${n}::bigint) * interval '1 microsecond')"
                ),
                // The cast must go through the WIRE type first. `$1::int4`
                // alone makes PG infer the parameter itself as int4, so the
                // client still sends an i64 into an int4 slot and fails
                // identically -- the cast has to name what rust-postgres
                // actually serializes before narrowing from it.
                _ => match &declared {
                    Some(ty) => format!("(${n}::{})::{ty}", generic_pg_type(c.ty)),
                    None => format!("${n}"),
                },
            });
        }
        let insert = format!("INSERT INTO {qname} VALUES ({})", vals.join(", "));
        let stmt = tx
            .prepare(&insert)
            .map_err(|e| pgerr(&format!("prepare insert `{}`", t.name), e))?;

        let mut cur = r.scan(table_id as u32, None, None)?;
        let mut n = 0u64;
        while let Some(row) = cur.next()? {
            let params: Vec<PgParam> = row.iter().map(to_param).collect();
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = params
                .iter()
                .map(|p| -> &(dyn postgres::types::ToSql + Sync) {
                    match p {
                        PgParam::Null => &NULL_I64 as &(dyn postgres::types::ToSql + Sync),
                        PgParam::Int(i) => i,
                        PgParam::Float(f) => f,
                        PgParam::Bool(b) => b,
                        PgParam::Text(s) => s,
                        PgParam::Blob(b) => b,
                        PgParam::Micros(m) => m,
                    }
                })
                .collect();
            tx.execute(&stmt, &refs)
                .map_err(|e| pgerr(&format!("insert into `{}`", t.name), e))?;
            n += 1;
        }
        report.tables.push(ExportStat {
            name: t.name.clone(),
            rows: n,
        });
    }

    tx.commit().map_err(|e| pgerr("commit", e))?;
    r.finish()?;
    Ok(report)
}
