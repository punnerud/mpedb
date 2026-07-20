//! sqlite source adapter — introspection, type mapping, and PK policy
//! (DESIGN-MIRROR §4.2/§4.4/§4.5). Uses rusqlite 0.31 (bundled SQLite 3.45).
//!
//! This stage (M2.2) turns a live sqlite schema into an mpedb [`Schema`] plus
//! the per-table source metadata the importer/adapter needs. It does not read
//! rows yet (that is import, M2.3).

use crate::state::MapPolicy;
use mpedb_types::{ColumnDef, ColumnType, Error, Result, Schema, TableDef};
use rusqlite::Connection;

fn sqlerr(context: &str, e: rusqlite::Error) -> Error {
    Error::Config(format!("sqlite {context}: {e}"))
}

/// A source column as introspected, with its chosen mpedb type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceColumn {
    pub name: String,
    pub declared_type: String,
    pub not_null: bool,
    /// STORED/VIRTUAL generated column: mirror its value, never write it back.
    pub generated: bool,
    pub mapped: ColumnType,
    /// Single-column UNIQUE (not the PK) — becomes an mpedb unique index.
    pub unique: bool,
}

/// A source table as introspected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceTable {
    pub name: String,
    pub without_rowid: bool,
    pub strict: bool,
    pub columns: Vec<SourceColumn>,
    /// Column indices forming the PK, in PK order.
    pub pk: Vec<usize>,
}

/// Map a sqlite declared type to an mpedb column type (DESIGN-MIRROR §4.5):
/// mpedb-specific declared-type sniffing (BOOL, DATE/TIME) runs BEFORE sqlite's
/// affinity rules (datatype3.html §3.1). NUMERIC affinity maps to Float64 by
/// default; the 2^53 precision guard is applied per-row at import time.
pub fn map_sqlite_type(declared: &str) -> ColumnType {
    let up = declared.to_ascii_uppercase();
    // sniff first
    if up.contains("BOOL") {
        return ColumnType::Bool;
    }
    if up.contains("DATE") || up.contains("TIME") {
        return ColumnType::Timestamp;
    }
    // affinity rules, in precedence order
    if up.contains("INT") {
        return ColumnType::Int64;
    }
    if up.contains("CHAR") || up.contains("CLOB") || up.contains("TEXT") {
        return ColumnType::Text;
    }
    if up.is_empty() || up.contains("BLOB") {
        return ColumnType::Blob;
    }
    if up.contains("REAL") || up.contains("FLOA") || up.contains("DOUB") {
        return ColumnType::Float64;
    }
    ColumnType::Float64 // NUMERIC affinity
}

/// How faithfully `map_sqlite_type` carried this declared type into mpedb
/// (DESIGN-MIRROR §2, [`MapPolicy`]). Kept beside the mapping so the two cannot
/// drift.
///
/// sqlite makes this weaker than the PG twin on purpose, and the record should
/// say so rather than flatter us: a declared type in sqlite is an *affinity*,
/// not a constraint — any row may hold any type — so "Exact" here means "the
/// declared type maps cleanly", NOT "every value obeys it". Per-row drift is
/// precisely what the pre-flight has to find; the schema only says what SHOULD
/// be there.
pub fn sqlite_map_policy(declared: &str) -> MapPolicy {
    let up = declared.to_ascii_uppercase();
    if up.contains("BOOL") {
        // sqlite has no bool: values are 0/1 by convention and nothing enforces
        // it. Round-trips as INTEGER, but a stray 'yes' is a per-row problem.
        return MapPolicy::Exact;
    }
    if up.contains("DATE") || up.contains("TIME") {
        // the storage convention (seconds vs millis vs ISO text) is a CONFIG
        // guess, not a fact — if it was wrong, every value is already wrong.
        return MapPolicy::ViaText;
    }
    if up.contains("INT") {
        return MapPolicy::Exact; // sqlite ints are exactly i64
    }
    if up.contains("CHAR") || up.contains("CLOB") || up.contains("TEXT") {
        return MapPolicy::Exact;
    }
    if up.is_empty() || up.contains("BLOB") {
        return MapPolicy::Exact;
    }
    if up.contains("REAL") || up.contains("FLOA") || up.contains("DOUB") {
        return MapPolicy::Exact;
    }
    // NUMERIC affinity → Float64: an integer beyond 2^53 has ALREADY lost
    // precision by the time it reaches mpedb. The column is recoverable; the
    // digits are not.
    MapPolicy::LossyAtImport
}

/// Introspect the `main` schema, restricted to the given scope. `include`
/// (if Some) is an allow-list of table names; `exclude` is always removed.
/// sqlite internal tables (`sqlite_%`) and the mirror's own objects
/// (`_mpedb_%`) are never mirrored.
pub fn introspect(
    conn: &Connection,
    include: Option<&[String]>,
    exclude: &[String],
) -> Result<Vec<SourceTable>> {
    let names = table_names(conn)?;
    let mut out = Vec::new();
    for name in names {
        if let Some(inc) = include {
            if !inc.iter().any(|n| n == &name) {
                continue;
            }
        }
        if exclude.iter().any(|n| n == &name) {
            continue;
        }
        out.push(introspect_table(conn, &name)?);
    }
    Ok(out)
}

fn table_names(conn: &Connection) -> Result<Vec<String>> {
    // pragma_table_list is a table-valued function (SQLite 3.37+); type='table'
    // excludes views/virtual/shadow objects.
    let mut stmt = conn
        .prepare(
            "SELECT name FROM pragma_table_list \
             WHERE type='table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
               AND name NOT LIKE '\\_mpedb\\_%' ESCAPE '\\' \
             ORDER BY name",
        )
        .map_err(|e| sqlerr("table_list", e))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| sqlerr("table_list", e))?;
    let mut names = Vec::new();
    for r in rows {
        names.push(r.map_err(|e| sqlerr("table_list", e))?);
    }
    Ok(names)
}

fn introspect_table(conn: &Connection, name: &str) -> Result<SourceTable> {
    // table_list flags for this table
    let (without_rowid, strict) = conn
        .query_row(
            "SELECT wr, strict FROM pragma_table_list WHERE name=?1 AND type='table'",
            [name],
            |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)? != 0)),
        )
        .map_err(|e| sqlerr("table_list flags", e))?;

    // columns via table_xinfo: cid,name,type,notnull,dflt_value,pk,hidden
    let mut stmt = conn
        .prepare(
            "SELECT name, type, \"notnull\", pk, hidden \
             FROM pragma_table_xinfo(?1) ORDER BY cid",
        )
        .map_err(|e| sqlerr("table_xinfo", e))?;
    let rows = stmt
        .query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,      // name
                r.get::<_, String>(1)?,      // declared type
                r.get::<_, i64>(2)? != 0,    // not null
                r.get::<_, i64>(3)?,         // pk position (0 = not pk)
                r.get::<_, i64>(4)?,         // hidden (0 normal, 2/3 generated)
            ))
        })
        .map_err(|e| sqlerr("table_xinfo", e))?;

    let unique_cols = single_column_unique_names(conn, name)?;

    let mut columns = Vec::new();
    let mut pk_positions: Vec<(i64, usize)> = Vec::new();
    for r in rows {
        let (cname, dtype, not_null, pk_pos, hidden) = r.map_err(|e| sqlerr("table_xinfo", e))?;
        if hidden == 1 {
            continue; // hidden virtual-table column; not mirrorable
        }
        let generated = hidden == 2 || hidden == 3;
        let idx = columns.len();
        if pk_pos > 0 {
            pk_positions.push((pk_pos, idx));
        }
        let unique = unique_cols.iter().any(|u| u == &cname) && pk_pos == 0;
        columns.push(SourceColumn {
            mapped: map_sqlite_type(&dtype),
            name: cname,
            declared_type: dtype,
            not_null,
            generated,
            unique,
        });
    }
    pk_positions.sort_by_key(|&(pos, _)| pos);
    let pk = pk_positions.into_iter().map(|(_, idx)| idx).collect();

    Ok(SourceTable {
        name: name.to_string(),
        without_rowid,
        strict,
        columns,
        pk,
    })
}

/// Names of columns that are the sole key of a non-partial UNIQUE index — the
/// only unique shape mpedb secondary indexes can represent (§4.1).
fn single_column_unique_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut idx_stmt = conn
        .prepare(
            "SELECT name FROM pragma_index_list(?1) \
             WHERE \"unique\"=1 AND partial=0 AND origin IN ('u','c')",
        )
        .map_err(|e| sqlerr("index_list", e))?;
    let idx_names: Vec<String> = idx_stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .map_err(|e| sqlerr("index_list", e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| sqlerr("index_list", e))?;

    let mut out = Vec::new();
    for idx in idx_names {
        // key columns of the index (key=1), skipping rowid/expression (cid<0)
        let mut col_stmt = conn
            .prepare("SELECT name, cid FROM pragma_index_xinfo(?1) WHERE key=1")
            .map_err(|e| sqlerr("index_xinfo", e))?;
        let cols: Vec<(Option<String>, i64)> = col_stmt
            .query_map([&idx], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| sqlerr("index_xinfo", e))?
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| sqlerr("index_xinfo", e))?;
        if cols.len() == 1 {
            if let (Some(cname), cid) = &cols[0] {
                if *cid >= 0 {
                    out.push(cname.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Build an mpedb [`TableDef`] from a source table, applying PK policy
/// (DESIGN-MIRROR §4.4). Rejects unmirrorable shapes with a clear error.
pub fn to_table_def(src: &SourceTable) -> Result<TableDef> {
    if !ident_ok(&src.name) {
        return Err(Error::Unsupported(format!(
            "table name `{}` is not a valid mpedb identifier (mangling not yet supported)",
            src.name
        )));
    }
    if src.pk.is_empty() {
        return Err(Error::Unsupported(format!(
            "table `{}` has no declared primary key; mirror mode requires one \
             (implicit rowid is rejected — VACUUM renumbers it)",
            src.name
        )));
    }
    let mut columns = Vec::with_capacity(src.columns.len());
    for (i, c) in src.columns.iter().enumerate() {
        if !ident_ok(&c.name) {
            return Err(Error::Unsupported(format!(
                "column `{}.{}` is not a valid mpedb identifier",
                src.name, c.name
            )));
        }
        let is_pk = src.pk.contains(&i);
        columns.push(ColumnDef { decl: None,
            name: c.name.clone(),
            ty: c.mapped,
            // PK columns are NOT NULL in mpedb regardless of sqlite's flag
            // (INTEGER PRIMARY KEY is implicitly non-null anyway).
            nullable: !is_pk && !c.not_null,
            unique: c.unique,
            indexed: false,
            default: None,
            check: None,
            // Mirror does not yet carry a source column collation (BINARY).
            collation: mpedb_types::Collation::Binary,
            // Mirror maps every source column to a RIGID mpedb type (never
            // `any`), and a rigid column enforces rather than converts — so the
            // affinity is the one its type implies. Import stays strict-reject.
            affinity: mpedb_types::Affinity::implied_by(c.mapped),
        });
    }
    let primary_key = src.pk.iter().map(|&i| i as u16).collect();
    Ok(TableDef {
        id: 0, // assigned by Schema::new in build_schema
        name: src.name.clone(),
        columns,
        primary_key,
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    })
}

/// Build the full mpedb schema from introspected source tables.
pub fn build_schema(tables: &[SourceTable]) -> Result<Schema> {
    let defs = tables.iter().map(to_table_def).collect::<Result<Vec<_>>>()?;
    Schema::new(defs)
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with("__mpedb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_mapping_sniff_and_affinity() {
        assert_eq!(map_sqlite_type("INTEGER"), ColumnType::Int64);
        assert_eq!(map_sqlite_type("BIGINT"), ColumnType::Int64);
        assert_eq!(map_sqlite_type("TEXT"), ColumnType::Text);
        assert_eq!(map_sqlite_type("VARCHAR(20)"), ColumnType::Text);
        assert_eq!(map_sqlite_type("BLOB"), ColumnType::Blob);
        assert_eq!(map_sqlite_type(""), ColumnType::Blob);
        assert_eq!(map_sqlite_type("REAL"), ColumnType::Float64);
        assert_eq!(map_sqlite_type("DOUBLE"), ColumnType::Float64);
        assert_eq!(map_sqlite_type("NUMERIC"), ColumnType::Float64);
        assert_eq!(map_sqlite_type("DECIMAL(10,2)"), ColumnType::Float64);
        // sniff wins over affinity
        assert_eq!(map_sqlite_type("BOOLEAN"), ColumnType::Bool);
        assert_eq!(map_sqlite_type("DATETIME"), ColumnType::Timestamp);
        assert_eq!(map_sqlite_type("TIMESTAMP"), ColumnType::Timestamp);
        assert_eq!(map_sqlite_type("DATE"), ColumnType::Timestamp);
    }

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn introspect_maps_tables_pk_and_unique() {
        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE users(
                 id INTEGER PRIMARY KEY,
                 email TEXT NOT NULL UNIQUE,
                 age INTEGER,
                 active BOOLEAN,
                 created_at DATETIME);
             CREATE TABLE orders(oid INTEGER PRIMARY KEY, amount REAL);
             CREATE VIEW v AS SELECT 1;",
        )
        .unwrap();

        let tables = introspect(&conn, None, &[]).unwrap();
        // view excluded; both real tables present, sorted by name
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].name, "orders");
        assert_eq!(tables[1].name, "users");

        let users = &tables[1];
        assert_eq!(users.pk, vec![0]); // id
        let names: Vec<_> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "email", "age", "active", "created_at"]);
        assert_eq!(users.columns[0].mapped, ColumnType::Int64);
        assert_eq!(users.columns[1].mapped, ColumnType::Text);
        assert!(users.columns[1].unique, "email is a single-column UNIQUE");
        assert!(!users.columns[0].unique, "PK column is not flagged unique");
        assert_eq!(users.columns[3].mapped, ColumnType::Bool);
        assert_eq!(users.columns[4].mapped, ColumnType::Timestamp);

        // scope selection
        let only = introspect(&conn, Some(&["users".into()]), &[]).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].name, "users");
        let excl = introspect(&conn, None, &["users".into()]).unwrap();
        assert_eq!(excl.len(), 1);
        assert_eq!(excl[0].name, "orders");
    }

    #[test]
    fn build_schema_from_introspection() {
        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, v REAL);",
        )
        .unwrap();
        let tables = introspect(&conn, None, &[]).unwrap();
        let schema = build_schema(&tables).unwrap();
        assert_eq!(schema.tables.len(), 1);
        let t = &schema.tables[0];
        assert_eq!(t.name, "t");
        assert_eq!(t.primary_key, vec![0]);
        assert_eq!(t.columns[0].ty, ColumnType::Int64);
        assert!(!t.columns[0].nullable); // PK forced NOT NULL
        assert!(t.columns[1].unique);
        assert_eq!(t.columns[2].ty, ColumnType::Float64);
    }

    #[test]
    fn no_pk_table_is_rejected() {
        let conn = mem();
        conn.execute_batch("CREATE TABLE t(a INTEGER, b TEXT);").unwrap();
        let tables = introspect(&conn, None, &[]).unwrap();
        assert!(build_schema(&tables).is_err());
    }

    #[test]
    fn composite_pk_is_supported() {
        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE t(a INTEGER, b INTEGER, v TEXT, PRIMARY KEY(a, b));",
        )
        .unwrap();
        let tables = introspect(&conn, None, &[]).unwrap();
        assert_eq!(tables[0].pk, vec![0, 1]);
        let schema = build_schema(&tables).unwrap();
        assert_eq!(schema.tables[0].primary_key, vec![0, 1]);
    }
}
