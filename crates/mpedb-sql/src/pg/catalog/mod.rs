//! `pg_catalog` and `information_schema`, generated from the live [`Schema`].
//!
//! # What this module is, and what it deliberately is not
//!
//! It produces **rows**, not SQL. Every relation here is a
//! [`CatalogRelation`]: a name, a column list with mpedb types, and a function
//! that turns the current schema into `Vec<Vec<Value>>`. Nothing in this file
//! knows how those rows will be reached.
//!
//! That factoring is not fastidiousness — it is forced by a measured limit.
//! The obvious route (define each relation as a CTE over `SELECT … UNION ALL
//! SELECT …` and let the ordinary planner run the client's query) works for a
//! single relation and **fails the moment two of them are joined**:
//!
//! ```text
//! WITH c(a) AS (SELECT 1 UNION ALL SELECT 3),
//!      d(b) AS (SELECT 2 UNION ALL SELECT 4)
//! SELECT c.a, d.b FROM c JOIN d ON c.a = d.b
//!   → bind error: CTE `d` body is not a simple SELECT
//! ```
//!
//! The reason is structural rather than incidental: a CTE in join position is
//! *spliced* — the join is rewritten to read the body's BASE TABLE under the
//! reference alias, with the body's WHERE merged into the ON (`view.rs`
//! `flatten_cte_join` / `splice_join_body`). A `UNION ALL` body has no single
//! base table to splice, so there is nothing for the rewrite to aim at.
//! Widening that is real materialization of a subquery in join position, not a
//! contained change.
//!
//! And joins are exactly what catalog clients send. `psql \d` joins `pg_class`
//! to `pg_namespace` to `pg_attribute` to `pg_type`; every ORM's reflection
//! does the same. So the rows have to arrive as ORDINARY TABLES the ordinary
//! planner can join, which means someone has to materialize them — see
//! `mpedb-pg`'s session catalog. This module is the half that says what the
//! rows ARE, so the CTE route (fine for one relation) and the materialized
//! route cannot disagree about `pg_class.relkind`.
//!
//! # Fidelity
//!
//! These relations report what mpedb actually has. Where PostgreSQL has a
//! concept mpedb does not — tablespaces, access methods, inheritance, multiple
//! schemas — the column is present (clients select it by name) and carries
//! PostgreSQL's own value for "none": `0` for an absent OID, `''`, or the
//! default every stock database shows. A column is never omitted, because a
//! missing column is a query that fails to bind rather than a row that reads
//! honestly.

pub(crate) mod infoschema;
pub(crate) mod pgcat;

use mpedb_types::schema::Schema;
use mpedb_types::{ColumnType, Value};

/// One synthesised catalog relation.
pub(crate) struct CatalogRelation {
    /// Unqualified name, as `pg_catalog` / `information_schema` spell it.
    pub name: &'static str,
    /// Which namespace it belongs to — the qualified spelling clients use.
    pub schema: CatalogSchema,
    /// Columns in ordinal order: `(name, type)`.
    pub columns: &'static [(&'static str, ColumnType)],
    /// Build the rows for a given schema. Row width must equal
    /// `columns.len()`; [`check`] enforces that in tests.
    pub rows: fn(&Schema) -> Vec<Vec<Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogSchema {
    PgCatalog,
    InformationSchema,
}

impl CatalogSchema {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CatalogSchema::PgCatalog => "pg_catalog",
            CatalogSchema::InformationSchema => "information_schema",
        }
    }
}

/// Every relation mpedb synthesises.
pub(crate) fn relations() -> &'static [CatalogRelation] {
    pgcat::RELATIONS
}

/// Look a relation up by an unqualified or qualified name.
///
/// Unqualified resolution is what makes `SELECT * FROM pg_class` work, and it
/// matches PostgreSQL: `pg_catalog` is implicitly first on `search_path`.
/// `information_schema` is NOT implicitly on the path in PostgreSQL, so it
/// resolves only when written qualified — reproducing that keeps a user table
/// called `tables` from being shadowed.
pub(crate) fn lookup(name: &str) -> Option<&'static CatalogRelation> {
    let lower = name.to_ascii_lowercase();
    let (qual, bare) = match lower.split_once('.') {
        Some((q, b)) => (Some(q.to_string()), b.to_string()),
        None => (None, lower),
    };
    relations().iter().find(|r| {
        r.name == bare
            && match qual.as_deref() {
                Some(q) => q == r.schema.as_str(),
                None => r.schema == CatalogSchema::PgCatalog,
            }
    })
}

// ---------------------------------------------------------------- shared bits

/// The OID mpedb reports for the `public` namespace.
///
/// PostgreSQL's own value in every stock database. Clients do compare against
/// it, and a database that reports something else looks like a system catalog
/// to reflection code that special-cases the system namespaces by OID.
pub(crate) const PUBLIC_NS_OID: u32 = 2200;
/// PostgreSQL's `pg_catalog` namespace OID.
pub(crate) const PG_CATALOG_NS_OID: u32 = 11;
/// PostgreSQL's `information_schema` namespace OID — assigned at initdb rather
/// than pinned in the catalog, but stable in practice and never compared to.
pub(crate) const INFORMATION_SCHEMA_NS_OID: u32 = 13000;

/// Per-table OID space.
///
/// Every object belonging to one table — the table, its rowtype, its indexes,
/// its constraints, its column defaults — is allocated inside a 1024-wide block
/// derived from the table's STABLE id (DESIGN-SCHEMA-V2). Stable rather than
/// positional so an OID survives a DROP of an unrelated table: a client that
/// cached `attrelid` between two queries would otherwise read another table's
/// columns.
///
/// The block width is what makes the sub-ranges below safe. An earlier layout
/// spaced tables 8 apart and put constraints at `+1000`, which reached straight
/// into a LATER table's block — two different objects sharing one OID. Since
/// `pg_class` is keyed on `oid`, that surfaces as an insert failure rather than
/// as a wrong answer; it could as easily have gone the other way.
const OID_BLOCK: i64 = 1024;
/// PostgreSQL's `FirstNormalObjectId`. Reflection code tells user objects from
/// system ones by comparing against it, so an OID below it makes a user table
/// invisible to every tool that filters on that.
const FIRST_NORMAL_OID: i64 = 16384;
const OFF_RELTYPE: i64 = 1;
const OFF_INDEX: i64 = 16;
const OFF_CONSTRAINT: i64 = 256;
const OFF_ATTRDEF: i64 = 512;

/// The OID mpedb gives a user table.
pub(crate) fn table_oid(table_id: u32) -> i64 {
    FIRST_NORMAL_OID + i64::from(table_id) * OID_BLOCK
}

/// The OID of a table's implicit rowtype (`pg_class.reltype`).
pub(crate) fn reltype_oid(table_id: u32) -> i64 {
    table_oid(table_id) + OFF_RELTYPE
}

/// The OID of index number `index_no` on a table (0 = the PK tree).
pub(crate) fn index_oid(table_id: u32, index_no: u32) -> i64 {
    table_oid(table_id) + OFF_INDEX + i64::from(index_no)
}

/// The OID of the `n`th constraint on a table.
pub(crate) fn constraint_oid(table_id: u32, n: usize) -> i64 {
    table_oid(table_id) + OFF_CONSTRAINT + n as i64
}

/// The OID of the default expression on the `n`th column of a table.
pub(crate) fn attrdef_oid(table_id: u32, n: usize) -> i64 {
    table_oid(table_id) + OFF_ATTRDEF + n as i64
}

/// Live (non-tombstone, non-FTS-shadow) tables, with their ids.
///
/// A dropped table leaves a DEAD slot that keeps its id so `position == id`
/// stays dense. Reporting one would give clients a table with no name and no
/// columns, so they are skipped here — which is also what a dropped table
/// looks like in PostgreSQL: gone.
pub(crate) fn live_tables(schema: &Schema) -> impl Iterator<Item = &mpedb_types::TableDef> {
    schema.tables.iter().filter(|t| !t.dead && !t.name.is_empty())
}

/// The columns a CLIENT can see: every declared column except the IMPLICIT
/// ROWID.
///
/// A table declared without a PRIMARY KEY gets one anyway — a real trailing
/// column named `rowid` carrying the key (#94) — because the engine keys every
/// table by its PK tree. Nothing else exposes it: `SELECT *` does not expand to
/// it, and sqlite hides its own rowid from `PRAGMA table_info` the same way. The
/// catalog was the one surface that did, which made a two-column table reflect
/// as three columns with a primary key its author never wrote.
///
/// The rowid is appended LAST, so dropping it leaves every other column's
/// 1-based `attnum` exactly where it was.
pub(crate) fn visible_columns(t: &mpedb_types::TableDef) -> &[mpedb_types::ColumnDef] {
    if t.implicit_rowid && !t.columns.is_empty() {
        &t.columns[..t.columns.len() - 1]
    } else {
        &t.columns
    }
}

/// Whether the table has a PRIMARY KEY a client should be told about.
///
/// False for an implicit rowid: that key is mpedb's storage decision, not
/// something the author declared, and reporting it makes a reflecting client
/// re-create the table WITH a primary key on a column that does not exist for it.
pub(crate) fn has_declared_pk(t: &mpedb_types::TableDef) -> bool {
    !t.primary_key.is_empty() && !t.implicit_rowid
}

/// The PG type OID a column reports.
pub(crate) fn column_type_oid(ty: ColumnType) -> i64 {
    i64::from(mpedb_types::pgtype::default_oid(ty))
}

/// `pg_attribute.attlen` for a column — PostgreSQL's `typlen`.
pub(crate) fn column_type_len(ty: ColumnType) -> i64 {
    mpedb_types::pgtype::by_oid(mpedb_types::pgtype::default_oid(ty))
        .map(|t| i64::from(t.typlen))
        .unwrap_or(-1)
}

/// The type NAME a column reports, in PostgreSQL's vocabulary.
pub(crate) fn column_type_name(ty: ColumnType) -> &'static str {
    mpedb_types::pgtype::by_oid(mpedb_types::pgtype::default_oid(ty))
        .map(|t| t.name)
        .unwrap_or("text")
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

fn int(i: impl Into<i64>) -> Value {
    Value::Int(i.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::schema::TableKind;
    use mpedb_types::value::{Affinity, Collation};
    use mpedb_types::{ColumnDef, TableDef};

    /// One column. `ColumnDef` has no constructor and twelve fields; spelling
    /// them out at every call site is what makes schema tests unreadable.
    fn c(name: &str, ty: ColumnType, nullable: bool, unique: bool, indexed: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            ty,
            nullable,
            unique,
            indexed,
            default: None,
            default_text: None,
            decl: None,
            check: None,
            collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
            generated: None,
        }
    }

    fn table(id: u32, name: &str, columns: Vec<ColumnDef>, pk: Vec<u16>) -> TableDef {
        TableDef {
            id,
            name: name.into(),
            columns,
            primary_key: pk,
            indexes: vec![],
            dead: false, pk_name: None,
            kind: TableKind::Standard,
            implicit_rowid: false,
            autoincrement: false,
            foreign_keys: Vec::new(),
        }
    }

    /// A table whose columns are all `Int64` and named as given — enough for a
    /// constraint test, where the column TYPE is not what is under test.
    pub(crate) fn table_for_test(
        id: u32,
        name: &str,
        cols: Vec<&str>,
        pk: Vec<u16>,
    ) -> TableDef {
        table(
            id,
            name,
            cols.into_iter()
                .map(|n| c(n, ColumnType::Int64, false, false, false))
                .collect(),
            pk,
        )
    }

    /// A schema with the shapes that break naive catalog code: a COMPOSITE
    /// primary key (so `indkey` and `ordinal_position` have something to get
    /// wrong), a nullable column, a unique column and a non-unique index.
    /// The OID of a table BY NAME. `Schema::new` sorts and renumbers, so
    /// `table_oid(0)` is whichever table sorts first — an assumption that reads
    /// as correct right up until a table is renamed.
    pub(crate) fn oid_of(schema: &Schema, name: &str) -> i64 {
        let t = live_tables(schema)
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no table {name}"));
        table_oid(t.id)
    }

    pub(crate) fn sample() -> Schema {
        Schema::new(vec![
            table(
                0,
                "users",
                vec![
                    c("id", ColumnType::Int64, false, false, false),
                    c("email", ColumnType::Text, false, true, false),
                    c("nick", ColumnType::Text, true, false, true),
                ],
                vec![0],
            ),
            table(
                1,
                "memberships",
                vec![
                    c("user_id", ColumnType::Int64, false, false, false),
                    c("group_id", ColumnType::Int64, false, false, false),
                    c("joined", ColumnType::Timestamp, true, false, false),
                ],
                vec![0, 1],
            ),
        ])
        .expect("sample schema is valid")
    }

    #[test]
    fn every_relation_produces_rows_as_wide_as_its_column_list() {
        // The failure this catches is nasty in exactly the way that matters: a
        // column added to the list without a value added to the row generator
        // shifts EVERY later column by one, so `relname` reads as the OID and
        // nothing errors.
        let schema = sample();
        for rel in relations() {
            for (i, row) in (rel.rows)(&schema).iter().enumerate() {
                assert_eq!(
                    row.len(),
                    rel.columns.len(),
                    "{}.{} row {i} has {} values for {} columns",
                    rel.schema.as_str(),
                    rel.name,
                    row.len(),
                    rel.columns.len()
                );
            }
        }
    }

    #[test]
    fn every_value_fits_the_column_type_it_is_declared_under() {
        // These rows are going into REAL rigid tables. A Text where the column
        // says Int64 would fail at insert time, in the wire layer, on a query
        // the user did not write — so catch it here instead.
        let schema = sample();
        for rel in relations() {
            for row in (rel.rows)(&schema) {
                for (v, (cname, cty)) in row.iter().zip(rel.columns) {
                    assert!(
                        v.fits(*cty),
                        "{}.{}.{cname}: {v:?} does not fit {cty:?}",
                        rel.schema.as_str(),
                        rel.name
                    );
                }
            }
        }
    }

    /// Every OID a catalog relation reports must be unique ACROSS relations
    /// that share the `oid` column space — pg_class, pg_type, pg_constraint,
    /// pg_attrdef, pg_namespace, pg_am. In PostgreSQL these are different
    /// catalogs and may repeat; here they must not, because `pg_class.oid` and
    /// `pg_index.indexrelid` are joined and a repeat would silently join the
    /// wrong pair.
    ///
    /// This is the test that would have caught the first OID layout, where
    /// constraints sat at `table_oid + 1000` and reached into the NEXT table's
    /// block.
    #[test]
    fn object_oids_never_collide_across_the_relations_that_join_on_them() {
        let mut schema = sample();
        // Widen the sample: a table with several indexes is exactly the shape
        // that overran an 8-wide block.
        for t in schema.tables.iter_mut() {
            t.indexes = (0..6)
                .map(|_| mpedb_types::IndexDef {
                    columns: vec![0],
                    unique: false,
                    predicate: None,
                    name: None,
                    exprs: vec![None],
                    collations: vec![None],
                    from_constraint: false,
                })
                .collect();
        }
        let mut seen: std::collections::BTreeMap<i64, String> = std::collections::BTreeMap::new();
        let mut claim = |oid: i64, what: String| {
            if let Some(prev) = seen.insert(oid, what.clone()) {
                panic!("OID {oid} claimed by both `{prev}` and `{what}`");
            }
        };
        for t in live_tables(&schema) {
            claim(table_oid(t.id), format!("table {}", t.name));
            claim(reltype_oid(t.id), format!("rowtype {}", t.name));
            for n in 0..=t.indexes.len() as u32 {
                claim(index_oid(t.id, n), format!("index {n} of {}", t.name));
            }
            for n in 0..t.indexes.len() + t.foreign_keys.len() + t.columns.len() + 1 {
                claim(constraint_oid(t.id, n), format!("constraint {n} of {}", t.name));
            }
            for n in 0..t.columns.len() {
                claim(attrdef_oid(t.id, n), format!("attrdef {n} of {}", t.name));
            }
        }
        // …and none of them may land on a built-in type's OID either, since
        // pg_type shares the space.
        for ty in mpedb_types::pgtype::TYPES {
            assert!(
                !seen.contains_key(&i64::from(ty.oid)),
                "user object collides with built-in type {}",
                ty.name
            );
        }
    }

    #[test]
    fn relation_names_are_unique_within_a_schema() {
        let mut seen = std::collections::BTreeSet::new();
        for rel in relations() {
            assert!(
                seen.insert((rel.schema.as_str(), rel.name)),
                "duplicate relation {}.{}",
                rel.schema.as_str(),
                rel.name
            );
        }
    }

    #[test]
    fn unqualified_lookup_finds_pg_catalog_but_not_information_schema() {
        // PostgreSQL puts pg_catalog on the implicit search_path and does NOT
        // put information_schema there. Reproducing that is what stops a user
        // table named `tables` or `columns` from being shadowed by a catalog
        // relation it has nothing to do with.
        assert!(lookup("pg_class").is_some());
        assert!(lookup("PG_CLASS").is_some());
        assert!(lookup("pg_catalog.pg_class").is_some());
        assert!(lookup("tables").is_none());
        assert!(lookup("information_schema.tables").is_some());
        assert!(lookup("columns").is_none());
    }

    #[test]
    fn user_table_oids_are_above_postgresqls_system_boundary_and_distinct() {
        // Reflection code tells user objects from system ones by comparing
        // against FirstNormalObjectId (16384). An OID below it makes a user
        // table invisible to every tool that filters on that.
        let schema = sample();
        let mut seen = std::collections::BTreeSet::new();
        for t in live_tables(&schema) {
            let oid = table_oid(t.id);
            assert!(oid >= 16384, "{} got OID {oid}", t.name);
            assert!(seen.insert(oid), "duplicate OID {oid}");
            // The rowtype and the PK index must not collide with it or with
            // each other.
            assert!(seen.insert(reltype_oid(t.id)));
            assert!(seen.insert(index_oid(t.id, 0)));
        }
    }

    #[test]
    fn dropped_and_unnamed_tables_are_not_reported() {
        let mut schema = sample();
        let mut dead = table(2, "", vec![], vec![]);
        dead.dead = true;
        schema.tables.push(dead);
        let names: Vec<_> = live_tables(&schema).map(|t| t.name.as_str()).collect();
        // `Schema::new` SORTS tables by name and renumbers ids so `position ==
        // id` (DESIGN-SCHEMA-V2). The order here is that sorted order, not the
        // order they were handed in — which is why every test below resolves an
        // id through `oid_of` rather than assuming the input order.
        assert_eq!(names, vec!["memberships", "users"]);
    }
}
