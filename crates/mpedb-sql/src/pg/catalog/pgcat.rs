//! The `pg_catalog` relations mpedb synthesises.
//!
//! # How the column lists were chosen
//!
//! Not by copying PostgreSQL's `pg_type.dat` wholesale, and not by guessing.
//! Each relation carries the columns real clients SELECT by name — `psql \d`,
//! SQLAlchemy's PG reflection, Django's introspection, psycopg's type loading —
//! plus the ones a `SELECT *` would make conspicuous by their absence.
//!
//! A column mpedb has no concept for is still PRESENT, carrying PostgreSQL's
//! own "none" value (`0` for an absent OID, `false`, `''`, or the value every
//! stock database shows). That is deliberate: an omitted column makes the
//! client's query fail to BIND, which reads as "mpedb is broken", whereas a
//! column reporting `0` reads as "there is no tablespace here", which is true.

use super::{
    attrdef_oid, column_type_len, column_type_name, column_type_oid, constraint_oid, index_oid,
    has_declared_pk, int, live_tables, reltype_oid, table_oid, text, visible_columns,
    CatalogRelation,
    CatalogSchema::{InformationSchema, PgCatalog},
    INFORMATION_SCHEMA_NS_OID, PG_CATALOG_NS_OID, PUBLIC_NS_OID,
};
use mpedb_types::schema::Schema;
use mpedb_types::{ColumnType as C, Value};

/// Every relation, in both namespaces. `information_schema`'s rows live in
/// `super::infoschema` but are listed here so there is ONE table of what mpedb
/// claims to have.
pub(crate) const RELATIONS: &[CatalogRelation] = &[
    CatalogRelation {
        name: "pg_namespace",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("nspname", C::Text),
            ("nspowner", C::Int64),
            ("nspacl", C::Text),
        ],
        rows: pg_namespace,
    },
    CatalogRelation {
        name: "pg_class",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("relname", C::Text),
            ("relnamespace", C::Int64),
            ("reltype", C::Int64),
            ("reloftype", C::Int64),
            ("relowner", C::Int64),
            ("relam", C::Int64),
            ("relfilenode", C::Int64),
            ("reltablespace", C::Int64),
            ("relpages", C::Int64),
            ("reltuples", C::Float64),
            ("relallvisible", C::Int64),
            ("reltoastrelid", C::Int64),
            ("relhasindex", C::Bool),
            ("relisshared", C::Bool),
            ("relpersistence", C::Text),
            ("relkind", C::Text),
            ("relnatts", C::Int64),
            ("relchecks", C::Int64),
            ("relhasrules", C::Bool),
            ("relhastriggers", C::Bool),
            ("relhassubclass", C::Bool),
            ("relrowsecurity", C::Bool),
            ("relforcerowsecurity", C::Bool),
            ("relispopulated", C::Bool),
            ("relreplident", C::Text),
            ("relispartition", C::Bool),
            ("relrewrite", C::Int64),
            ("relacl", C::Text),
            ("reloptions", C::Text),
        ],
        rows: pg_class,
    },
    CatalogRelation {
        name: "pg_attribute",
        schema: PgCatalog,
        columns: &[
            ("attrelid", C::Int64),
            ("attname", C::Text),
            ("atttypid", C::Int64),
            ("attstattarget", C::Int64),
            ("attlen", C::Int64),
            ("attnum", C::Int64),
            ("attndims", C::Int64),
            ("attcacheoff", C::Int64),
            ("atttypmod", C::Int64),
            ("attbyval", C::Bool),
            ("attstorage", C::Text),
            ("attalign", C::Text),
            ("attnotnull", C::Bool),
            ("atthasdef", C::Bool),
            ("atthasmissing", C::Bool),
            ("attidentity", C::Text),
            ("attgenerated", C::Text),
            ("attisdropped", C::Bool),
            ("attislocal", C::Bool),
            ("attinhcount", C::Int64),
            ("attcollation", C::Int64),
        ],
        rows: pg_attribute,
    },
    CatalogRelation {
        name: "pg_type",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("typname", C::Text),
            ("typnamespace", C::Int64),
            ("typowner", C::Int64),
            ("typlen", C::Int64),
            ("typbyval", C::Bool),
            ("typtype", C::Text),
            ("typcategory", C::Text),
            ("typispreferred", C::Bool),
            ("typisdefined", C::Bool),
            ("typdelim", C::Text),
            ("typrelid", C::Int64),
            ("typelem", C::Int64),
            ("typarray", C::Int64),
            ("typnotnull", C::Bool),
            ("typbasetype", C::Int64),
            ("typtypmod", C::Int64),
            ("typndims", C::Int64),
            ("typcollation", C::Int64),
            // A DOMAIN's default expression, as text. mpedb has no domains, so
            // it is always NULL — but reflection SELECTs it by name, and a
            // missing column fails the whole query rather than the one field.
            ("typdefault", C::Text),
        ],
        rows: pg_type,
    },
    CatalogRelation {
        name: "pg_index",
        schema: PgCatalog,
        columns: &[
            ("indexrelid", C::Int64),
            ("indrelid", C::Int64),
            ("indnatts", C::Int64),
            ("indnkeyatts", C::Int64),
            ("indisunique", C::Bool),
            ("indisprimary", C::Bool),
            ("indisexclusion", C::Bool),
            ("indimmediate", C::Bool),
            ("indisclustered", C::Bool),
            ("indisvalid", C::Bool),
            ("indcheckxmin", C::Bool),
            ("indisready", C::Bool),
            ("indislive", C::Bool),
            ("indisreplident", C::Bool),
            // PostgreSQL 15's `NULLS NOT DISTINCT` on a unique index. mpedb has
            // no such option, so it is always false — but reflection SELECTs it
            // by name and a missing column fails the whole query.
            ("indnullsnotdistinct", C::Bool),
            ("indkey", C::Text),
            ("indcollation", C::Text),
            ("indclass", C::Text),
            ("indoption", C::Text),
            ("indexprs", C::Text),
            ("indpred", C::Text),
        ],
        rows: pg_index,
    },
    CatalogRelation {
        name: "pg_constraint",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("conname", C::Text),
            ("connamespace", C::Int64),
            ("contype", C::Text),
            ("condeferrable", C::Bool),
            ("condeferred", C::Bool),
            ("convalidated", C::Bool),
            ("conrelid", C::Int64),
            ("contypid", C::Int64),
            ("conindid", C::Int64),
            ("conparentid", C::Int64),
            ("confrelid", C::Int64),
            ("confupdtype", C::Text),
            ("confdeltype", C::Text),
            ("confmatchtype", C::Text),
            ("conislocal", C::Bool),
            ("coninhcount", C::Int64),
            ("connoinherit", C::Bool),
            ("conkey", C::Text),
            ("confkey", C::Text),
            ("conbin", C::Text),
            // NOT a PostgreSQL column: it is what `pg_get_constraintdef(oid)`
            // returns, precomputed. That function needs the SCHEMA to answer —
            // a `ScalarFn` has none — so the PG surface binds the call to this
            // sibling column instead (`PgFunc::SiblingColumn`, pg/funcs.rs).
            // Reflection parses this text with a REGEX, so it is not cosmetic:
            // it is where foreign keys, unique and check constraints are read
            // from.
            ("condef", C::Text),
        ],
        rows: pg_constraint,
    },
    CatalogRelation {
        name: "pg_database",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("datname", C::Text),
            ("datdba", C::Int64),
            ("encoding", C::Int64),
            ("datcollate", C::Text),
            ("datctype", C::Text),
            ("datistemplate", C::Bool),
            ("datallowconn", C::Bool),
            ("datconnlimit", C::Int64),
            ("dattablespace", C::Int64),
            ("datacl", C::Text),
        ],
        rows: pg_database,
    },
    CatalogRelation {
        name: "pg_roles",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("rolname", C::Text),
            ("rolsuper", C::Bool),
            ("rolinherit", C::Bool),
            ("rolcreaterole", C::Bool),
            ("rolcreatedb", C::Bool),
            ("rolcanlogin", C::Bool),
            ("rolreplication", C::Bool),
            ("rolconnlimit", C::Int64),
            ("rolbypassrls", C::Bool),
        ],
        rows: pg_roles,
    },
    CatalogRelation {
        name: "pg_am",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("amname", C::Text),
            ("amhandler", C::Int64),
            ("amtype", C::Text),
        ],
        rows: pg_am,
    },
    // Two relations mpedb has NO objects for, and that is exactly why they
    // exist here. An ORM's reflection joins them unconditionally, so a MISSING
    // relation is an error the client cannot interpret, while an EMPTY one is
    // the truthful answer: this database has no sequences and no operator
    // classes, so the join contributes no rows. Measured: `pg_sequence` was 89
    // driver errors in SQLAlchemy's suite and `pg_opclass` another 54, both
    // inside column- and index-reflection queries that ask for nothing else
    // mpedb lacks.
    CatalogRelation {
        name: "pg_sequence",
        schema: PgCatalog,
        columns: &[
            ("seqrelid", C::Int64),
            ("seqtypid", C::Int64),
            ("seqstart", C::Int64),
            ("seqincrement", C::Int64),
            ("seqmax", C::Int64),
            ("seqmin", C::Int64),
            ("seqcache", C::Int64),
            ("seqcycle", C::Bool),
        ],
        rows: empty,
    },
    CatalogRelation {
        // Same reasoning as `pg_sequence`/`pg_opclass`: mpedb has one
        // collation family (`crate::Collation`) and no catalog objects for it,
        // so the honest row set is empty. Reflection LEFT JOINs it to find a
        // column's non-default collation; no rows means "every column uses the
        // default", which is true here.
        name: "pg_collation",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("collname", C::Text),
            ("collnamespace", C::Int64),
            ("collowner", C::Int64),
            ("collprovider", C::Text),
            ("collisdeterministic", C::Bool),
            ("collencoding", C::Int64),
            ("collcollate", C::Text),
            ("collctype", C::Text),
        ],
        rows: empty,
    },
    CatalogRelation {
        // ENUM labels. mpedb has no `CREATE TYPE … AS ENUM`, so this is always
        // EMPTY — but reflection JOINs it to read a column's enum labels, and
        // a missing relation fails the whole query where an empty one just
        // contributes no rows.
        name: "pg_enum",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("enumtypid", C::Int64),
            ("enumsortorder", C::Float64),
            ("enumlabel", C::Text),
        ],
        rows: empty,
    },
    CatalogRelation {
        name: "pg_opclass",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("opcmethod", C::Int64),
            ("opcname", C::Text),
            ("opcnamespace", C::Int64),
            ("opcowner", C::Int64),
            ("opcfamily", C::Int64),
            ("opcintype", C::Int64),
            ("opcdefault", C::Bool),
            ("opckeytype", C::Int64),
        ],
        rows: empty,
    },
    CatalogRelation {
        name: "pg_description",
        schema: PgCatalog,
        columns: &[
            ("objoid", C::Int64),
            ("classoid", C::Int64),
            ("objsubid", C::Int64),
            ("description", C::Text),
        ],
        rows: empty,
    },
    CatalogRelation {
        name: "pg_attrdef",
        schema: PgCatalog,
        columns: &[
            ("oid", C::Int64),
            ("adrelid", C::Int64),
            ("adnum", C::Int64),
            ("adbin", C::Text),
        ],
        rows: pg_attrdef,
    },
    CatalogRelation {
        name: "pg_tables",
        schema: PgCatalog,
        columns: &[
            ("schemaname", C::Text),
            ("tablename", C::Text),
            ("tableowner", C::Text),
            ("tablespace", C::Text),
            ("hasindexes", C::Bool),
            ("hasrules", C::Bool),
            ("hastriggers", C::Bool),
            ("rowsecurity", C::Bool),
        ],
        rows: pg_tables,
    },
    CatalogRelation {
        name: "pg_indexes",
        schema: PgCatalog,
        columns: &[
            ("schemaname", C::Text),
            ("tablename", C::Text),
            ("indexname", C::Text),
            ("tablespace", C::Text),
            ("indexdef", C::Text),
        ],
        rows: pg_indexes,
    },
    CatalogRelation {
        name: "pg_views",
        schema: PgCatalog,
        columns: &[
            ("schemaname", C::Text),
            ("viewname", C::Text),
            ("viewowner", C::Text),
            ("definition", C::Text),
        ],
        rows: empty,
    },
    CatalogRelation {
        name: "pg_matviews",
        schema: PgCatalog,
        columns: &[
            ("schemaname", C::Text),
            ("matviewname", C::Text),
            ("matviewowner", C::Text),
            ("definition", C::Text),
        ],
        rows: empty,
    },
    // ---- information_schema (rows in `super::infoschema`) -------------------
    CatalogRelation {
        name: "tables",
        schema: InformationSchema,
        columns: super::infoschema::TABLES_COLUMNS,
        rows: super::infoschema::tables,
    },
    CatalogRelation {
        name: "columns",
        schema: InformationSchema,
        columns: super::infoschema::COLUMNS_COLUMNS,
        rows: super::infoschema::columns,
    },
    CatalogRelation {
        name: "schemata",
        schema: InformationSchema,
        columns: super::infoschema::SCHEMATA_COLUMNS,
        rows: super::infoschema::schemata,
    },
    CatalogRelation {
        name: "table_constraints",
        schema: InformationSchema,
        columns: super::infoschema::TABLE_CONSTRAINTS_COLUMNS,
        rows: super::infoschema::table_constraints,
    },
    CatalogRelation {
        name: "key_column_usage",
        schema: InformationSchema,
        columns: super::infoschema::KEY_COLUMN_USAGE_COLUMNS,
        rows: super::infoschema::key_column_usage,
    },
    CatalogRelation {
        name: "referential_constraints",
        schema: InformationSchema,
        columns: super::infoschema::REFERENTIAL_CONSTRAINTS_COLUMNS,
        rows: super::infoschema::referential_constraints,
    },
];

/// The owner OID every object reports.
///
/// mpedb has no roles — the OS owns the file, which is the actual security
/// boundary (DESIGN-MULTIDB's trust box says so in as many words). One role
/// exists so that joins to `pg_roles` resolve rather than dropping every row.
pub(crate) const OWNER_OID: i64 = 10;
/// The one role's name, and the `user` every connection reports.
pub(crate) const OWNER_NAME: &str = "mpedb";

fn empty(_: &Schema) -> Vec<Vec<Value>> {
    Vec::new()
}

fn pg_namespace(_: &Schema) -> Vec<Vec<Value>> {
    [
        (i64::from(PUBLIC_NS_OID), "public"),
        (i64::from(PG_CATALOG_NS_OID), "pg_catalog"),
        (i64::from(INFORMATION_SCHEMA_NS_OID), "information_schema"),
    ]
    .into_iter()
    .map(|(oid, name)| vec![int(oid), text(name), int(OWNER_OID), Value::Null])
    .collect()
}

fn pg_class(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        let has_index = !t.indexes.is_empty() || has_declared_pk(t);
        out.push(vec![
            int(table_oid(t.id)),
            text(&t.name),
            int(i64::from(PUBLIC_NS_OID)),
            int(reltype_oid(t.id)),
            int(0),
            int(OWNER_OID),
            // relam: 2 is PostgreSQL's heap access method. mpedb has exactly
            // one storage shape, so reporting heap is accurate in the only
            // sense the column can be read.
            int(2),
            int(table_oid(t.id)),
            int(0),
            int(0),
            // reltuples: -1 is PostgreSQL's "never analysed". Reporting a
            // fabricated row count here would feed a client's own planner.
            Value::Float(-1.0),
            int(0),
            int(0),
            Value::Bool(has_index),
            Value::Bool(false),
            // relpersistence 'p' = permanent.
            text("p"),
            // relkind 'r' = ordinary table.
            text("r"),
            int(visible_columns(t).len() as i64),
            int(
                visible_columns(t)
                    .iter()
                    .filter(|c| c.check.is_some())
                    .count() as i64,
            ),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            // relreplident 'd' = default (the primary key).
            text("d"),
            Value::Bool(false),
            int(0),
            Value::Null,
            Value::Null,
        ]);
        // Indexes are relations too, and `psql \d` reaches them through
        // pg_class → pg_index → pg_class. Omitting them makes every index
        // invisible even though pg_index has the row.
        for (n, name) in index_names(t) {
            out.push(vec![
                int(index_oid(t.id, n)),
                text(name),
                int(i64::from(PUBLIC_NS_OID)),
                int(0),
                int(0),
                int(OWNER_OID),
                // 403 = btree, which is what mpedb's indexes are.
                int(403),
                int(index_oid(t.id, n)),
                int(0),
                int(0),
                Value::Float(-1.0),
                int(0),
                int(0),
                Value::Bool(false),
                Value::Bool(false),
                text("p"),
                // relkind 'i' = index.
                text("i"),
                int(index_width(t, n) as i64),
                int(0),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                text("n"),
                Value::Bool(false),
                int(0),
                Value::Null,
                Value::Null,
            ]);
        }
    }
    out
}

/// Index number → reported name, for every index on a table.
///
/// Index 0 is the PK tree (CLAUDE.md's numbering: `index_no = position + 1`
/// for the entries in `TableDef::indexes`). A flag-derived index never had a
/// name, so it gets PostgreSQL's own generated shape — `<table>_<col>_idx` —
/// which is what a client would see had the index been created there.
fn index_names(t: &mpedb_types::TableDef) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    if has_declared_pk(t) {
        // In PostgreSQL a PRIMARY KEY constraint and its backing index share ONE
        // name, so the declared one — when there is one — has to be reported for
        // both or a client sees two unrelated objects.
        out.push((
            0,
            t.pk_name
                .clone()
                .unwrap_or_else(|| format!("{}_pkey", t.name)),
        ));
    }
    for (i, idx) in t.indexes.iter().enumerate() {
        let n = i as u32 + 1;
        let name = idx.name.clone().unwrap_or_else(|| {
            // `filter_map` already drops the expression sentinel (there is no
            // column at u16::MAX), so a generated name for an expression index
            // simply omits that part rather than panicking.
            let cols: Vec<&str> = idx
                .columns
                .iter()
                .filter_map(|&c| t.columns.get(c as usize))
                .map(|c| c.name.as_str())
                .collect();
            format!(
                "{}_{}_{}",
                t.name,
                cols.join("_"),
                if idx.unique { "key" } else { "idx" }
            )
        });
        out.push((n, name));
    }
    out
}

fn index_width(t: &mpedb_types::TableDef, index_no: u32) -> usize {
    if index_no == 0 {
        t.primary_key.len()
    } else {
        t.indexes
            .get(index_no as usize - 1)
            .map(|i| i.columns.len())
            .unwrap_or(0)
    }
}

fn pg_attribute(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (i, c) in visible_columns(t).iter().enumerate() {
            let oid = column_type_oid(c.ty);
            out.push(vec![
                int(table_oid(t.id)),
                text(&c.name),
                int(oid),
                int(-1),
                int(column_type_len(c.ty)),
                // attnum is 1-BASED. Zero would mean "whole row" and negative
                // numbers are system columns; an off-by-one here makes every
                // client read the wrong column for a constraint's conkey.
                int(i as i64 + 1),
                int(0),
                int(-1),
                int(-1),
                Value::Bool(matches!(
                    c.ty,
                    mpedb_types::ColumnType::Int64
                        | mpedb_types::ColumnType::Float64
                        | mpedb_types::ColumnType::Bool
                        | mpedb_types::ColumnType::Timestamp
                )),
                text(storage_for(c.ty)),
                text(align_for(c.ty)),
                Value::Bool(!c.nullable),
                // `atthasdef` — whether pg_attrdef has a row for this column.
                // The rowid alias has no DECLARED default and one REPORTED one
                // (see `rowid_alias_default`), so both have to be counted here
                // or the client never reads the row that exists.
                Value::Bool(c.default.is_some() || rowid_alias_default(t, i).is_some()),
                Value::Bool(false),
                // `attidentity` — whether the column is an IDENTITY column,
                // PostgreSQL 10's `GENERATED … AS IDENTITY`. mpedb's generated
                // key is the ROWID ALIAS (#94), a single-column INTEGER PRIMARY
                // KEY that auto-assigns `max+1`, and that is reported as a
                // SERIAL — a sequence-backed DEFAULT — not as an identity.
                //
                // The distinction is not cosmetic. An identity column is
                // reflected by reading the SEQUENCE OBJECT behind it
                // (`pg_get_serial_sequence` → `pg_sequence.seqstart` and the
                // rest), and mpedb has no sequence objects, so that read comes
                // back empty and the column reflects as NOT autoincrementing —
                // measured, with `attidentity = 'd'` set. A serial is reflected
                // from the DEFAULT TEXT alone, which mpedb can state truthfully.
                // SQLAlchemy's own comment in that query says the same thing
                // from the other side: `attidentity != ''` is what keeps a
                // serial from being mistaken for an identity.
                text(""),
                text(if c.generated.is_some() { "s" } else { "" }),
                Value::Bool(false),
                Value::Bool(true),
                int(0),
                // Text collates bytewise here (mpedb's memcmp order, sqlite's
                // BINARY); 950 is PostgreSQL's "C" collation OID. Anything else
                // would promise an ordering mpedb does not implement.
                int(if c.ty == mpedb_types::ColumnType::Text {
                    950
                } else {
                    0
                }),
            ]);
        }
    }
    out
}

/// The DEFAULT a rowid-alias column REPORTS: PostgreSQL's `serial` shape.
///
/// A single-column INTEGER PRIMARY KEY auto-assigns `max+1` (#94). PostgreSQL
/// spells that a `serial`, which is not a type but a DEFAULT reading from a
/// sequence, and reflection recognises the column as auto-assigning by matching
/// `nextval\('…'\)` against this text. mpedb has no sequence OBJECT — `nextval()`
/// refuses by name — so nothing here can be followed to one; the sequence name
/// is PostgreSQL's derived one (`<table>_<column>_seq`), which is the name a
/// client would find had the column been declared `SERIAL`.
///
/// `None` for every other column, including a composite PK and a non-integer
/// one: neither auto-assigns, and saying they did would make a client omit the
/// value on INSERT and get a NOT NULL violation.
fn rowid_alias_default(t: &mpedb_types::TableDef, i: usize) -> Option<String> {
    let c = t.columns.get(i)?;
    if c.ty != mpedb_types::ColumnType::Int64 || t.primary_key.as_slice() != [i as u16] {
        return None;
    }
    // A DECLARED default wins: the column supplies its value from that, and
    // reporting a sequence over the top would name a source that is not used.
    if c.default.is_some() {
        return None;
    }
    Some(format!(
        "nextval('{}_{}_seq'::regclass)",
        t.name, c.name
    ))
}

fn storage_for(ty: mpedb_types::ColumnType) -> &'static str {
    match ty {
        mpedb_types::ColumnType::Text | mpedb_types::ColumnType::Blob => "x",
        mpedb_types::ColumnType::Any => "x",
        _ => "p",
    }
}

fn align_for(ty: mpedb_types::ColumnType) -> &'static str {
    match ty {
        mpedb_types::ColumnType::Text | mpedb_types::ColumnType::Blob => "i",
        mpedb_types::ColumnType::Any => "i",
        _ => "d",
    }
}

fn pg_type(_: &Schema) -> Vec<Vec<Value>> {
    mpedb_types::pgtype::TYPES
        .iter()
        .map(|t| {
            vec![
                int(i64::from(t.oid)),
                text(t.name),
                int(i64::from(PG_CATALOG_NS_OID)),
                int(OWNER_OID),
                int(i64::from(t.typlen)),
                Value::Bool(t.typlen > 0 && t.typlen <= 8),
                // 'b' = base type. Every type mpedb reports is one.
                text("b"),
                text(category_for(t.name)),
                Value::Bool(matches!(t.name, "int4" | "float8" | "text" | "timestamptz")),
                Value::Bool(true),
                text(","),
                int(0),
                int(0),
                int(0),
                Value::Bool(false),
                int(0),
                int(-1),
                int(0),
                int(if t.name == "text" { 100 } else { 0 }),
                // typdefault: a domain-only field, and mpedb has no domains.
                Value::Null,
            ]
        })
        .collect()
}

/// PostgreSQL's `typcategory` — a single letter psycopg and friends switch on
/// when they choose a Python adapter.
fn category_for(name: &str) -> &'static str {
    match name {
        "bool" => "B",
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "oid" => "N",
        "char" | "name" | "text" | "bpchar" | "varchar" => "S",
        "date" | "time" | "timestamp" | "timestamptz" | "timetz" | "interval" => "D",
        _ => "U",
    }
}

fn pg_index(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (n, _) in index_names(t) {
            let (cols, unique, primary): (Vec<u16>, bool, bool) = if n == 0 {
                (t.primary_key.clone(), true, true)
            } else {
                let idx = &t.indexes[n as usize - 1];
                (idx.columns.clone(), idx.unique, false)
            };
            // indkey is an int2vector: PostgreSQL renders it as SPACE-separated
            // 1-based attnums, and clients parse exactly that. Rendering it
            // comma-separated would parse as a single number in most of them.
            //
            // Stored as that TEXT rather than as a `Value::List`, because the
            // catalog is MATERIALIZED into a real mpedb table and a list has no
            // row encoding — `Any` excludes it by name. `unnest(indkey)` still
            // works: the set-returning projection parses this external form,
            // which is the one thing that knows what it means.
            //
            // PostgreSQL's own convention: a `0` in int2vector `indkey` means
            // "this key part is an EXPRESSION — see indexprs", and every client
            // that reads indkey knows it. mpedb spells the same thing
            // `INDEX_EXPR_COL` (`u16::MAX`), and `c + 1` on that overflows.
            let indkey = cols
                .iter()
                .map(|&c| {
                    if c == mpedb_types::schema::INDEX_EXPR_COL {
                        "0".to_string()
                    } else {
                        (c + 1).to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let zeros = cols.iter().map(|_| "0").collect::<Vec<_>>().join(" ");
            out.push(vec![
                int(index_oid(t.id, n)),
                int(table_oid(t.id)),
                int(cols.len() as i64),
                int(cols.len() as i64),
                Value::Bool(unique),
                Value::Bool(primary),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                // indnullsnotdistinct: mpedb's unique index treats NULLs as
                // distinct, which is the SQL default and PostgreSQL's.
                Value::Bool(false),
                text(indkey),
                text(zeros.clone()),
                text(zeros.clone()),
                text(zeros),
                Value::Null,
                // A partial index's predicate is the source text mpedb stores.
                match n {
                    0 => Value::Null,
                    _ => match &t.indexes[n as usize - 1].predicate {
                        Some(p) => text(p),
                        None => Value::Null,
                    },
                },
            ]);
        }
    }
    out
}

/// `pg_description` rows from the comments mpedb has STORED.
///
/// Not a `CatalogRelation::rows` function, because those see only the schema
/// and a comment does not live there — it is a sys-keyspace record, read by
/// `Database::list_comments`. So the caller passes them in and this maps them
/// onto PostgreSQL's `(objoid, classoid, objsubid)` addressing.
///
/// `objsubid` is the ATTNUM for a column comment and `0` for a table's, which
/// is exactly how reflection tells the two apart in one join.
/// The value of a named column in a row of relation `rel`.
///
/// The position comes from the relation's OWN column list rather than from a
/// literal index: these row vectors are positional, and a column inserted in the
/// middle would otherwise make every reader below it quietly read its neighbour.
fn column_of(rel: &str, col: &str, row: &[Value]) -> Option<Value> {
    let rel = super::lookup(rel)?;
    let i = rel.columns.iter().position(|(c, _)| *c == col)?;
    row.get(i).cloned()
}

pub fn description_rows(schema: &Schema, comments: &[(String, String)]) -> Vec<Vec<Value>> {
    // The class a comment's object belongs to: `pg_class` for a table or one of
    // its columns, `pg_constraint` for a constraint.
    const PG_CLASS_OID: i64 = 1259;
    const PG_CONSTRAINT_OID: i64 = 2606;
    let mut out = Vec::new();
    // Built once, and ONLY if a constraint comment is present. Reading the OIDs
    // out of the rows `pg_constraint` itself produces is what keeps the two from
    // drifting: the numbering is positional (`next_oid` walks pk, then uniques,
    // then foreign keys, then checks), so a second implementation of that walk
    // would hang a comment on a different constraint the first time either side
    // grew a case.
    let mut con_rows: Option<Vec<Vec<Value>>> = None;
    for (subkey, body) in comments {
        // `t/<table>`, `c/<table>.<column>`, `k/<table>.<constraint>` — the
        // spellings `Database::comment_key` writes.
        let (kind, rest) = match subkey.split_once('/') {
            Some(p) => p,
            None => continue,
        };
        let (table, sub) = match kind {
            "t" => (rest, None),
            "c" | "k" => match rest.split_once('.') {
                Some((t, c)) => (t, Some(c)),
                None => continue,
            },
            _ => continue,
        };
        let Some(t) = live_tables(schema).find(|t| t.name.eq_ignore_ascii_case(table))
        else {
            continue;
        };
        let objsubid = match (kind, sub) {
            ("c", Some(col)) => {
                // 1-based attnum, and a column that no longer exists simply has
                // no row — a DROP COLUMN leaves the comment behind in the sys
                // keyspace and reflection must not invent an attnum for it.
                match t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(col)) {
                    Some(i) => i as i64 + 1,
                    None => continue,
                }
            }
            // A CONSTRAINT comment addresses the CONSTRAINT, so it is keyed by
            // that object's OID and class — not by the table's, which would
            // report a foreign key's comment as the table's own.
            ("k", Some(name)) => {
                let rows = con_rows.get_or_insert_with(|| pg_constraint(schema));
                let want_rel = Value::Int(table_oid(t.id));
                let hit = rows.iter().find(|r| {
                    column_of("pg_constraint", "conrelid", r) == Some(want_rel.clone())
                        && matches!(
                            column_of("pg_constraint", "conname", r),
                            Some(Value::Text(ref n)) if n.eq_ignore_ascii_case(name)
                        )
                });
                // A comment on a constraint that no longer exists has no row,
                // for the same reason a dropped column's does not: the sys
                // keyspace keeps the comment, and inventing an OID for it would
                // attach the text to whatever now occupies that slot.
                let Some(row) = hit else { continue };
                let Some(Value::Int(oid)) = column_of("pg_constraint", "oid", row) else {
                    continue;
                };
                out.push(vec![
                    int(oid),
                    int(PG_CONSTRAINT_OID),
                    int(0),
                    text(body.clone()),
                ]);
                continue;
            }
            _ => 0,
        };
        out.push(vec![
            int(table_oid(t.id)),
            int(PG_CLASS_OID),
            int(objsubid),
            text(body.clone()),
        ]);
    }
    out
}

fn pg_constraint(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        // One counter per table: every constraint on it takes the next slot in
        // that table's OID block, so no two can collide and none can reach into
        // another table's range.
        let mut nth = 0usize;
        // Same `INDEX_EXPR_COL` rule as `indkey`: 0 is PostgreSQL's "expression".
        let attnums = |cols: &[u16]| {
            format!(
                "{{{}}}",
                cols.iter()
                    .map(|&c| {
                        if c == mpedb_types::schema::INDEX_EXPR_COL {
                            "0".to_string()
                        } else {
                            (c + 1).to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        // The column list a constraint definition renders, quoted PG's way.
        let collist = |cols: &[u16]| {
            cols.iter()
                .map(|&c| match t.columns.get(c as usize) {
                    Some(col) => quote_ident(&col.name),
                    None => "?column?".to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        if has_declared_pk(t) {
            out.push(constraint_row(
                next_oid(t.id, &mut nth),
                t.pk_name
                    .clone()
                    .unwrap_or_else(|| format!("{}_pkey", t.name)),
                "p",
                table_oid(t.id),
                index_oid(t.id, 0),
                0,
                attnums(&t.primary_key),
                String::new(),
                false,
                Value::Null,
                format!("PRIMARY KEY ({})", collist(&t.primary_key)),
            ));
        }
        let names = index_names(t);
        for (i, idx) in t.indexes.iter().enumerate() {
            // A unique index is a CONSTRAINT only if one was declared.
            // PostgreSQL reports a bare `CREATE UNIQUE INDEX` in `pg_index`
            // alone, and a client that reads it out of here instead re-creates
            // the index as a `UniqueConstraint` — the same enforcement under a
            // different object, with nothing in the output to say so.
            if !idx.unique || !idx.from_constraint {
                continue;
            }
            let n = i as u32 + 1;
            out.push(constraint_row(
                next_oid(t.id, &mut nth),
                // The SAME name the backing index reports: in PostgreSQL a
                // unique constraint and its index share one name, and that
                // shared name is how a client pairs the two (SQLAlchemy's
                // `duplicates_index`). Two spellings would report the pair as
                // two unrelated objects.
                names
                    .iter()
                    .find(|(no, _)| *no == n)
                    .map(|(_, nm)| nm.clone())
                    .unwrap_or_else(|| format!("{}_{}_key", t.name, n)),
                "u",
                table_oid(t.id),
                index_oid(t.id, n),
                0,
                attnums(&idx.columns),
                String::new(),
                false,
                Value::Null,
                format!("UNIQUE ({})", collist(&idx.columns)),
            ));
        }
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            // The parent's OID has to be looked up by NAME: a FK records the
            // referenced table's name, not its id, because the parent may not
            // exist when the child is declared.
            let parent_def =
                live_tables(schema).find(|p| mpedb_types::ident::ident_eq(&p.name, &fk.parent));
            let parent = parent_def.map(|p| table_oid(p.id)).unwrap_or(0);
            // `REFERENCES t` with no column list means the parent's PRIMARY KEY
            // — resolved HERE because the parent exists by now, even though it
            // did not when the child was declared (ForeignKeyDef's own docs).
            let parent_cols: Vec<String> = if !fk.parent_columns.is_empty() {
                fk.parent_columns.iter().map(|c| quote_ident(c)).collect()
            } else {
                match parent_def {
                    Some(p) => p
                        .primary_key
                        .iter()
                        .filter_map(|&c| p.columns.get(c as usize))
                        .map(|c| quote_ident(&c.name))
                        .collect(),
                    None => Vec::new(),
                }
            };
            // `confkey` is the parent's column ORDINALS, which needs the same
            // resolution: a name list has to be looked up in the parent.
            let confkey = match parent_def {
                Some(p) if !fk.parent_columns.is_empty() => {
                    let ords: Vec<u16> = fk
                        .parent_columns
                        .iter()
                        .filter_map(|name| {
                            p.columns
                                .iter()
                                .position(|c| mpedb_types::ident::ident_eq(&c.name, name))
                                .map(|i| i as u16)
                        })
                        .collect();
                    if ords.len() == fk.parent_columns.len() {
                        attnums(&ords)
                    } else {
                        String::new()
                    }
                }
                Some(p) => attnums(&p.primary_key),
                None => String::new(),
            };
            out.push(constraint_row(
                next_oid(t.id, &mut nth),
                fk.name
                    .clone()
                    .unwrap_or_else(|| format!("{}_fk_{i}", t.name)),
                "f",
                table_oid(t.id),
                0,
                parent,
                attnums(&fk.columns),
                confkey,
                fk.deferred,
                Value::Null,
                fk_def(fk, &collist(&fk.columns), &parent_cols),
            ));
        }
        for (i, c) in t.columns.iter().enumerate() {
            if let Some(check) = &c.check {
                out.push(constraint_row(
                    next_oid(t.id, &mut nth),
                    format!("{}_{}_check", t.name, c.name),
                    "c",
                    table_oid(t.id),
                    0,
                    0,
                    format!("{{{}}}", i + 1),
                    String::new(),
                    false,
                    text(check),
                    // PostgreSQL renders a CHECK with the expression in its own
                    // parentheses inside the constraint's — SQLAlchemy's
                    // `CHECK \((.+)\)` strips one level and keeps the rest, so
                    // one pair of parentheses too few loses the outer operator.
                    format!("CHECK (({check}))"),
                ));
            }
        }
    }
    out
}

fn next_oid(table_id: u32, nth: &mut usize) -> i64 {
    let oid = constraint_oid(table_id, *nth);
    *nth += 1;
    oid
}

#[allow(clippy::too_many_arguments)]
fn constraint_row(
    oid: i64,
    name: String,
    contype: &str,
    conrelid: i64,
    conindid: i64,
    confrelid: i64,
    conkey: String,
    confkey: String,
    deferred: bool,
    conbin: Value,
    condef: String,
) -> Vec<Value> {
    vec![
        int(oid),
        text(name),
        int(i64::from(PUBLIC_NS_OID)),
        text(contype),
        Value::Bool(deferred),
        Value::Bool(deferred),
        Value::Bool(true),
        int(conrelid),
        int(0),
        int(conindid),
        int(0),
        int(confrelid),
        // 'a' = NO ACTION, PostgreSQL's default for both.
        text("a"),
        text("a"),
        // 's' = MATCH SIMPLE.
        text("s"),
        Value::Bool(true),
        int(0),
        Value::Bool(false),
        text(conkey),
        if confkey.is_empty() {
            Value::Null
        } else {
            text(confkey)
        },
        conbin,
        text(condef),
    ]
}

/// PostgreSQL's `pg_get_constraintdef` text for a FOREIGN KEY.
///
/// The shape is fixed by what reads it: SQLAlchemy matches
/// `FOREIGN KEY \((.*?)\) REFERENCES (?:(t)\.)?(t)\((cols)\)` and then optional
/// MATCH / ON UPDATE / ON DELETE / DEFERRABLE clauses, each separated by a
/// SINGLE space. The schema qualifier is left off deliberately — mpedb has one
/// namespace and PostgreSQL itself omits it when the schema is on the
/// search_path, which is the case reflection is written for.
fn fk_def(fk: &mpedb_types::schema::ForeignKeyDef, child_cols: &str, parent_cols: &[String]) -> String {
    use mpedb_types::schema::FkAction;
    let action = |a: FkAction| match a {
        FkAction::NoAction => None,
        FkAction::Restrict => Some("RESTRICT"),
        FkAction::Cascade => Some("CASCADE"),
        FkAction::SetNull => Some("SET NULL"),
        FkAction::SetDefault => Some("SET DEFAULT"),
    };
    let mut def = format!(
        "FOREIGN KEY ({child_cols}) REFERENCES {}({})",
        quote_ident(&fk.parent),
        parent_cols.join(", ")
    );
    // PostgreSQL prints ON UPDATE before ON DELETE, and prints neither when the
    // action is NO ACTION (the default).
    if let Some(a) = action(fk.on_update) {
        def.push_str(&format!(" ON UPDATE {a}"));
    }
    if let Some(a) = action(fk.on_delete) {
        def.push_str(&format!(" ON DELETE {a}"));
    }
    if fk.deferred {
        def.push_str(" DEFERRABLE INITIALLY DEFERRED");
    }
    def
}

/// Quote an identifier the way PostgreSQL renders one inside a definition:
/// bare when it is already a lowercase unquoted identifier, double-quoted (with
/// embedded quotes doubled) otherwise.
///
/// PostgreSQL also quotes a bare identifier that collides with a RESERVED WORD;
/// this does not, because the keyword set lives in the tokenizer and duplicating
/// it here would be a second copy to keep in step. The consumer unquotes either
/// way, so the difference is textual only.
fn quote_ident(name: &str) -> String {
    let bare = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn pg_database(_: &Schema) -> Vec<Vec<Value>> {
    vec![vec![
        int(16400),
        text("mpedb"),
        int(OWNER_OID),
        // 6 = UTF8 in PostgreSQL's encoding table. mpedb's Text is UTF-8 by
        // construction, so this is the only honest answer.
        int(6),
        // C collation: mpedb compares text bytewise. Claiming a locale would
        // promise an ordering the engine does not implement.
        text("C"),
        text("C"),
        Value::Bool(false),
        Value::Bool(true),
        int(-1),
        int(1663),
        Value::Null,
    ]]
}

fn pg_roles(_: &Schema) -> Vec<Vec<Value>> {
    vec![vec![
        int(OWNER_OID),
        text(OWNER_NAME),
        Value::Bool(true),
        Value::Bool(true),
        Value::Bool(true),
        Value::Bool(true),
        Value::Bool(true),
        Value::Bool(false),
        int(-1),
        Value::Bool(true),
    ]]
}

fn pg_am(_: &Schema) -> Vec<Vec<Value>> {
    vec![
        vec![int(2), text("heap"), int(0), text("t")],
        vec![int(403), text("btree"), int(0), text("i")],
    ]
}

fn pg_attrdef(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (i, c) in visible_columns(t).iter().enumerate() {
            // Either a DECLARED default or the rowid alias's reported serial —
            // never both, and `rowid_alias_default` is what decides.
            let adbin = match (&c.default, rowid_alias_default(t, i)) {
                (Some(_), _) => match &c.default_text {
                    Some(t) => text(t),
                    None => Value::Null,
                },
                (None, Some(seq)) => text(seq),
                (None, None) => continue,
            };
            out.push(vec![
                int(attrdef_oid(t.id, i)),
                int(table_oid(t.id)),
                int(i as i64 + 1),
                // adbin is a parse tree in PostgreSQL and unreadable to clients
                // anyway; they call pg_get_expr() on it. mpedb keeps the DDL
                // TEXT the default was written with, which is what that call
                // would have returned.
                adbin,
            ]);
        }
    }
    out
}

fn pg_tables(schema: &Schema) -> Vec<Vec<Value>> {
    live_tables(schema)
        .map(|t| {
            vec![
                text("public"),
                text(&t.name),
                text(OWNER_NAME),
                Value::Null,
                Value::Bool(!t.indexes.is_empty() || has_declared_pk(t)),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
            ]
        })
        .collect()
}

fn pg_indexes(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (n, name) in index_names(t) {
            let (cols, unique) = if n == 0 {
                (t.primary_key.clone(), true)
            } else {
                let idx = &t.indexes[n as usize - 1];
                (idx.columns.clone(), idx.unique)
            };
            let colnames: Vec<&str> = cols
                .iter()
                .filter_map(|&c| t.columns.get(c as usize))
                .map(|c| c.name.as_str())
                .collect();
            out.push(vec![
                text("public"),
                text(&t.name),
                text(&name),
                Value::Null,
                text(format!(
                    "CREATE {}INDEX {name} ON public.{} USING btree ({})",
                    if unique { "UNIQUE " } else { "" },
                    t.name,
                    colnames.join(", ")
                )),
            ]);
        }
    }
    out
}

/// Type helpers re-exported for `infoschema`, which reports the same types
/// under the SQL standard's names.
pub(crate) fn sql_type_name(ty: mpedb_types::ColumnType) -> &'static str {
    // information_schema uses the SQL standard's spellings, not typname.
    match column_type_name(ty) {
        "int8" => "bigint",
        "int4" => "integer",
        "int2" => "smallint",
        "float8" => "double precision",
        "float4" => "real",
        "bool" => "boolean",
        "bytea" => "bytea",
        "timestamptz" => "timestamp with time zone",
        "timestamp" => "timestamp without time zone",
        "numeric" => "numeric",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{oid_of, sample};
    use super::*;
    use crate::pg::catalog::lookup;

    fn rows_of(name: &str) -> Vec<Vec<Value>> {
        let rel = lookup(name).unwrap_or_else(|| panic!("no relation {name}"));
        (rel.rows)(&sample())
    }

    fn col(name: &str, col: &str, row: &[Value]) -> Value {
        super::column_of(name, col, row)
            .unwrap_or_else(|| panic!("{name} has no column {col}"))
    }

    #[test]
    fn pg_class_reports_tables_and_their_indexes_as_separate_relations() {
        let rows = rows_of("pg_class");
        let kinds: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                let Value::Text(n) = col("pg_class", "relname", r) else {
                    panic!()
                };
                let Value::Text(k) = col("pg_class", "relkind", r) else {
                    panic!()
                };
                (n, k)
            })
            .collect();
        assert!(kinds.contains(&("users".into(), "r".into())));
        assert!(kinds.contains(&("memberships".into(), "r".into())));
        // `psql \d` reaches an index through pg_class, so a pg_index row with
        // no pg_class row beside it is an index nothing can see.
        assert!(kinds.contains(&("users_pkey".into(), "i".into())));
        assert!(kinds.contains(&("memberships_pkey".into(), "i".into())));
    }

    #[test]
    fn attnum_is_one_based() {
        // Off by one here silently reads the wrong column for every conkey and
        // indkey a client resolves.
        let rows = rows_of("pg_attribute");
        let users = oid_of(&sample(), "users");
        let mut nums: Vec<i64> = rows
            .iter()
            .filter(|r| col("pg_attribute", "attrelid", r) == Value::Int(users))
            .map(|r| match col("pg_attribute", "attnum", r) {
                Value::Int(n) => n,
                v => panic!("{v:?}"),
            })
            .collect();
        nums.sort_unstable();
        assert_eq!(nums, vec![1, 2, 3]);
    }

    #[test]
    fn attnotnull_is_the_inverse_of_nullable() {
        let rows = rows_of("pg_attribute");
        let find = |name: &str| {
            rows.iter()
                .find(|r| col("pg_attribute", "attname", r) == Value::Text(name.into()))
                .cloned()
                .unwrap()
        };
        assert_eq!(
            col("pg_attribute", "attnotnull", &find("id")),
            Value::Bool(true)
        );
        // `nick` is nullable in the sample; reporting NOT NULL for it would
        // make a client reject a legal insert before it ever reaches mpedb.
        assert_eq!(
            col("pg_attribute", "attnotnull", &find("nick")),
            Value::Bool(false)
        );
    }

    #[test]
    fn indkey_is_space_separated_one_based_attnums() {
        // PostgreSQL renders int2vector space-separated and every client parses
        // exactly that. Commas would read as one number.
        let rows = rows_of("pg_index");
        let memberships = oid_of(&sample(), "memberships");
        let pk = rows
            .iter()
            .find(|r| {
                col("pg_index", "indrelid", r) == Value::Int(memberships)
                    && col("pg_index", "indisprimary", r) == Value::Bool(true)
            })
            .unwrap();
        assert_eq!(col("pg_index", "indkey", pk), Value::Text("1 2".into()));
        assert_eq!(col("pg_index", "indnatts", pk), Value::Int(2));
    }

    #[test]
    fn a_composite_primary_key_produces_one_constraint_naming_both_columns() {
        let rows = rows_of("pg_constraint");
        let pk = rows
            .iter()
            .find(|r| col("pg_constraint", "conname", r) == Value::Text("memberships_pkey".into()))
            .unwrap();
        assert_eq!(col("pg_constraint", "contype", pk), Value::Text("p".into()));
        assert_eq!(col("pg_constraint", "conkey", pk), Value::Text("{1,2}".into()));
    }

    #[test]
    fn a_unique_column_becomes_a_unique_constraint_not_a_primary_key() {
        let rows = rows_of("pg_constraint");
        let types: Vec<String> = rows
            .iter()
            .filter(|r| {
                col("pg_constraint", "conrelid", r) == Value::Int(oid_of(&sample(), "users"))
            })
            .map(|r| match col("pg_constraint", "contype", r) {
                Value::Text(t) => t,
                v => panic!("{v:?}"),
            })
            .collect();
        assert!(types.contains(&"p".to_string()));
        assert!(types.contains(&"u".to_string()), "{types:?}");
    }

    #[test]
    fn pg_type_carries_the_oids_clients_decode_by() {
        let rows = rows_of("pg_type");
        let by_name = |n: &str| {
            rows.iter()
                .find(|r| col("pg_type", "typname", r) == Value::Text(n.into()))
                .cloned()
                .unwrap()
        };
        assert_eq!(col("pg_type", "oid", &by_name("int8")), Value::Int(20));
        assert_eq!(col("pg_type", "oid", &by_name("text")), Value::Int(25));
        // typcategory is what psycopg switches on to pick an adapter.
        assert_eq!(
            col("pg_type", "typcategory", &by_name("int8")),
            Value::Text("N".into())
        );
        assert_eq!(
            col("pg_type", "typcategory", &by_name("text")),
            Value::Text("S".into())
        );
    }

    #[test]
    fn reltuples_is_minus_one_rather_than_an_invented_row_count() {
        // -1 is PostgreSQL's "never analysed". A fabricated number here would
        // feed the CLIENT's planner, which is a wrong answer with no error.
        for r in rows_of("pg_class") {
            assert_eq!(col("pg_class", "reltuples", &r), Value::Float(-1.0));
        }
    }

    #[test]
    fn the_three_namespaces_exist_so_joins_to_them_resolve() {
        let names: Vec<Value> = rows_of("pg_namespace")
            .iter()
            .map(|r| col("pg_namespace", "nspname", r))
            .collect();
        assert!(names.contains(&Value::Text("public".into())));
        assert!(names.contains(&Value::Text("pg_catalog".into())));
        assert!(names.contains(&Value::Text("information_schema".into())));
    }

    #[test]
    fn a_foreign_key_renders_the_definition_reflection_parses() {
        // The minimal repro for the whole `condef` column: a table name that
        // needs quoting, which is what SQLAlchemy's BizarroCharacterTest uses.
        // Every character of this text is load-bearing — reflection reads
        // foreign keys by REGEX over it, so an extra space or a missing paren
        // is a key that reflects as none.
        use mpedb_types::schema::{ForeignKeyDef, TableDef};
        let child = TableDef {
            foreign_keys: vec![ForeignKeyDef {
                columns: vec![1],
                parent: "(2)".into(),
                parent_columns: vec!["(3)".into()],
                on_delete: mpedb_types::schema::FkAction::Cascade,
                on_update: mpedb_types::schema::FkAction::NoAction,
                deferred: false,
                name: Some("fk_other".into()),
            }],
            ..super::super::tests::table_for_test(1, "other", vec!["id", "ref"], vec![0])
        };
        let parent = super::super::tests::table_for_test(0, "(2)", vec!["(3)"], vec![0]);
        let schema = Schema::new(vec![parent, child]).expect("valid");
        let rows = pg_constraint(&schema);
        let defs: Vec<String> = rows
            .iter()
            .map(|r| match col("pg_constraint", "condef", r) {
                Value::Text(s) => s,
                v => panic!("{v:?}"),
            })
            .collect();
        assert!(
            defs.contains(&"FOREIGN KEY (ref) REFERENCES \"(2)\"(\"(3)\") ON DELETE CASCADE".into()),
            "{defs:?}"
        );
        // The parent's own key still renders, and the child's, so a client that
        // asks for all constraints on one table gets both kinds.
        assert!(defs.contains(&"PRIMARY KEY (\"(3)\")".into()), "{defs:?}");
        assert!(defs.contains(&"PRIMARY KEY (id)".into()), "{defs:?}");
    }
}
