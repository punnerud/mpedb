//! PostgreSQL source adapter — introspection, type mapping, and PK policy
//! (DESIGN-MIRROR §4.3/§4.4/§4.5). Uses the sync `postgres` client. This stage
//! (M4.1) turns a live PostgreSQL `public` schema into an mpedb [`Schema`] plus
//! per-table source metadata; rows/pull come in later stages.

use crate::state::MapPolicy;
use mpedb_types::{ColumnDef, ColumnType, Error, Result, Schema, TableDef};
use postgres::Client;

fn pgerr(context: &str, e: postgres::Error) -> Error {
    Error::Config(format!("postgres {context}: {e}"))
}

/// A PostgreSQL column as introspected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgColumn {
    pub name: String,
    /// The BASE type name (`int8`, `text`, `timestamptz`, `numeric`, …) — what
    /// the mpedb mapping and the [`MapPolicy`] verdict key off.
    pub pg_type: String,
    /// The DECLARED type with typmod (`numeric(10,2)`, `varchar(64)`) — what an
    /// export must recreate. Distinct from `pg_type` on purpose: recording only
    /// the base type silently widens every constrained column on the way home.
    pub declared_type: String,
    pub not_null: bool,
    /// GENERATED column: mirror its value, never write it back.
    pub generated: bool,
    /// GENERATED … AS IDENTITY (needs OVERRIDING SYSTEM VALUE on push).
    pub identity: bool,
    /// mpedb type, or None if the PG type is not representable (→ reject).
    pub mapped: Option<ColumnType>,
    /// Single-column UNIQUE (not the PK) → mpedb unique index.
    pub unique: bool,
}

/// A PostgreSQL table as introspected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgTable {
    pub oid: u32,
    pub name: String,
    /// `relreplident`: 'd' default, 'n' nothing, 'f' full, 'i' index.
    pub replica_identity: char,
    pub columns: Vec<PgColumn>,
    /// Column indices forming the PK, in PK order.
    pub pk: Vec<usize>,
}

/// Map a PostgreSQL base type name to an mpedb column type (DESIGN-MIRROR §4.5).
/// `None` ⇒ not representable (caller rejects the table or marks it pull_only).
/// numeric → Text (lossless round-trip); uuid → Blob(16); json/jsonb → Text.
pub fn map_pg_type(typname: &str) -> Option<ColumnType> {
    Some(match typname {
        "int2" | "int4" | "int8" => ColumnType::Int64,
        "float4" | "float8" => ColumnType::Float64,
        "bool" => ColumnType::Bool,
        "text" | "varchar" | "bpchar" | "name" | "citext" => ColumnType::Text,
        "bytea" => ColumnType::Blob,
        // timestamptz/timestamp store int64 micros; date/time handled at import
        "timestamptz" | "timestamp" | "date" => ColumnType::Timestamp,
        "time" => ColumnType::Int64,
        "numeric" => ColumnType::Text, // lossless as canonical text
        "uuid" => ColumnType::Blob,    // 16 bytes
        "json" | "jsonb" => ColumnType::Text,
        _ => return None,
    })
}

/// How faithfully `map_pg_type` carried this PG type into mpedb — recorded per
/// column so a later export/pre-flight can tell "can go home" from "already
/// lost" (DESIGN-MIRROR §2, [`MapPolicy`]).
///
/// This lives beside `map_pg_type` on purpose: the two must not drift. A type
/// added there without a verdict here would silently record `Exact`.
pub fn pg_map_policy(typname: &str, mapped: ColumnType) -> MapPolicy {
    match typname {
        // same width, same semantics, both directions
        "int8" | "float8" | "bool" | "text" | "bytea" | "timestamptz" | "timestamp" => {
            MapPolicy::Exact
        }
        // mpedb's type is WIDER than the source column: import was lossless, but
        // a LOCAL write can now exceed what the source accepts. This is the
        // class that fails at the target's INSERT — `int4` holding 2147483648
        // is exactly the error the PG fidelity work hit — so the pre-flight
        // keys off it.
        "int2" | "int4" | "float4" | "varchar" | "bpchar" | "citext" | "name" | "date" | "time" => {
            MapPolicy::Widened
        }
        // preserved through a canonical text/byte form
        "numeric" | "json" | "jsonb" => MapPolicy::ViaText,
        "uuid" => MapPolicy::ViaText,
        // Unknown types are rejected by map_pg_type before we get here; if that
        // ever changes, do not claim fidelity we have not reasoned about.
        _ => {
            let _ = mapped;
            MapPolicy::LossyAtImport
        }
    }
}

/// Introspect the `public` schema, restricted to the given scope.
pub fn introspect(
    client: &mut Client,
    include: Option<&[String]>,
    exclude: &[String],
) -> Result<Vec<PgTable>> {
    let table_rows = client
        .query(
            "SELECT c.oid, c.relname, c.relreplident::text \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r','p') AND n.nspname = 'public' \
               AND c.relname NOT LIKE '\\_mpedb\\_%' \
             ORDER BY c.relname",
            &[],
        )
        .map_err(|e| pgerr("table list", e))?;

    let mut out = Vec::new();
    for row in table_rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let relident: String = row.get(2);
        if let Some(inc) = include {
            if !inc.iter().any(|n| n == &name) {
                continue;
            }
        }
        if exclude.iter().any(|n| n == &name) {
            continue;
        }
        out.push(introspect_table(client, oid, name, relident.chars().next().unwrap_or('d'))?);
    }
    Ok(out)
}

fn introspect_table(
    client: &mut Client,
    oid: u32,
    name: String,
    replica_identity: char,
) -> Result<PgTable> {
    // Columns in attnum order. TWO type strings, and both are needed:
    //   typname            — the BASE type ("numeric", "varchar"), which drives
    //                        the mpedb mapping and the policy verdict;
    //   format_type(...)   — the DECLARED type WITH typmod ("numeric(10,2)",
    //                        "varchar(64)"), which is what an export has to
    //                        recreate. Recording only the base type would
    //                        silently widen every constrained column on the way
    //                        home — a schema that looks right and is not.
    let col_rows = client
        .query(
            "SELECT a.attname, t.typname, a.attnotnull, \
                    a.attidentity::text, a.attgenerated::text, \
                    format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .map_err(|e| pgerr("columns", e))?;

    // PK column names in key order
    let pk_names: Vec<String> = client
        .query(
            "SELECT a.attname \
             FROM pg_constraint con \
             JOIN unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
             WHERE con.conrelid = $1 AND con.contype = 'p' \
             ORDER BY k.ord",
            &[&oid],
        )
        .map_err(|e| pgerr("primary key", e))?
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    // single-column UNIQUE constraint columns
    let unique_names: Vec<String> = client
        .query(
            "SELECT a.attname \
             FROM pg_constraint con \
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[1] \
             WHERE con.conrelid = $1 AND con.contype = 'u' \
               AND array_length(con.conkey, 1) = 1",
            &[&oid],
        )
        .map_err(|e| pgerr("unique", e))?
        .into_iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    let mut columns = Vec::new();
    for row in col_rows {
        let cname: String = row.get(0);
        let typname: String = row.get(1);
        let not_null: bool = row.get(2);
        let attidentity: String = row.get(3);
        let attgenerated: String = row.get(4);
        let declared_type: String = row.get(5);
        let is_pk = pk_names.iter().any(|p| p == &cname);
        columns.push(PgColumn {
            mapped: map_pg_type(&typname),
            unique: !is_pk && unique_names.iter().any(|u| u == &cname),
            name: cname,
            pg_type: typname,
            declared_type,
            not_null,
            generated: !attgenerated.is_empty(),
            identity: !attidentity.is_empty(),
        });
    }
    let pk = pk_names
        .iter()
        .map(|n| columns.iter().position(|c| &c.name == n).unwrap())
        .collect();

    Ok(PgTable {
        oid,
        name,
        replica_identity,
        columns,
        pk,
    })
}

/// Build an mpedb [`TableDef`] from a PostgreSQL table (§4.4). Rejects
/// unmirrorable shapes (no PK, unmappable type, invalid identifier) clearly.
pub fn to_table_def(src: &PgTable) -> Result<TableDef> {
    if !ident_ok(&src.name) {
        return Err(Error::Unsupported(format!(
            "table `{}` is not a valid mpedb identifier (mangling not yet supported)",
            src.name
        )));
    }
    if src.pk.is_empty() {
        return Err(Error::Unsupported(format!(
            "table `{}` has no primary key; mirror mode requires one",
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
        let ty = c.mapped.ok_or_else(|| {
            Error::Unsupported(format!(
                "column `{}.{}` has PostgreSQL type `{}` which mpedb cannot represent",
                src.name, c.name, c.pg_type
            ))
        })?;
        let is_pk = src.pk.contains(&i);
        columns.push(ColumnDef {
            name: c.name.clone(),
            ty,
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
            affinity: mpedb_types::Affinity::implied_by(ty),
        });
    }
    Ok(TableDef {
        id: 0, // assigned by Schema::new in build_schema
        name: src.name.clone(),
        columns,
        primary_key: src.pk.iter().map(|&i| i as u16).collect(),
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    })
}

/// Build the full mpedb schema from introspected PostgreSQL tables.
pub fn build_schema(tables: &[PgTable]) -> Result<Schema> {
    Schema::new(tables.iter().map(to_table_def).collect::<Result<Vec<_>>>()?)
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
    fn type_mapping() {
        assert_eq!(map_pg_type("int8"), Some(ColumnType::Int64));
        assert_eq!(map_pg_type("int4"), Some(ColumnType::Int64));
        assert_eq!(map_pg_type("float8"), Some(ColumnType::Float64));
        assert_eq!(map_pg_type("bool"), Some(ColumnType::Bool));
        assert_eq!(map_pg_type("text"), Some(ColumnType::Text));
        assert_eq!(map_pg_type("varchar"), Some(ColumnType::Text));
        assert_eq!(map_pg_type("bytea"), Some(ColumnType::Blob));
        assert_eq!(map_pg_type("timestamptz"), Some(ColumnType::Timestamp));
        assert_eq!(map_pg_type("date"), Some(ColumnType::Timestamp));
        assert_eq!(map_pg_type("numeric"), Some(ColumnType::Text));
        assert_eq!(map_pg_type("uuid"), Some(ColumnType::Blob));
        assert_eq!(map_pg_type("jsonb"), Some(ColumnType::Text));
        // unrepresentable → None
        assert_eq!(map_pg_type("_int4"), None); // array
        assert_eq!(map_pg_type("point"), None);
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn introspect_live_pg() {
        let pg = crate::pg_harness::ThrowawayPg::start();
        let mut c = pg.client();
        c.batch_execute(
            "CREATE TABLE users(
                 id bigint PRIMARY KEY,
                 email text NOT NULL UNIQUE,
                 age int,
                 active boolean,
                 balance double precision,
                 created_at timestamptz);
             CREATE TABLE orders(oid bigint PRIMARY KEY, amount numeric);",
        )
        .unwrap();

        let tables = introspect(&mut c, None, &[]).unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].name, "orders");
        assert_eq!(tables[1].name, "users");

        let users = &tables[1];
        assert_eq!(users.pk.len(), 1);
        assert_eq!(users.columns[0].name, "id");
        assert_eq!(users.columns[0].mapped, Some(ColumnType::Int64));
        assert_eq!(users.columns[1].name, "email");
        assert!(users.columns[1].unique);
        assert_eq!(users.columns[3].mapped, Some(ColumnType::Bool));
        assert_eq!(users.columns[5].mapped, Some(ColumnType::Timestamp));

        let schema = build_schema(&tables).unwrap();
        assert_eq!(schema.tables.len(), 2);
        // orders.amount numeric → Text
        let orders = schema.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.columns[1].ty, ColumnType::Text);
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn no_pk_and_unmappable_type_rejected() {
        let pg = crate::pg_harness::ThrowawayPg::start();
        let mut c = pg.client();
        c.batch_execute("CREATE TABLE t(a int, b text);").unwrap(); // no PK
        assert!(build_schema(&introspect(&mut c, None, &[]).unwrap()).is_err());

        c.batch_execute("DROP TABLE t; CREATE TABLE t(id bigint PRIMARY KEY, tags int[]);")
            .unwrap();
        // array type is unrepresentable → to_table_def rejects
        assert!(build_schema(&introspect(&mut c, None, &[]).unwrap()).is_err());
    }
}
