//! The public face of the PostgreSQL dialect, for `mpedb-pg`.
//!
//! Everything else under `pg/` is `pub(crate)`: it is compiler internals. This
//! module is the narrow surface the wire protocol needs, and it is narrow on
//! purpose — a wire server that could reach into the binder would grow a second
//! implementation of things the compiler already decides.

use crate::pg::catalog;
use mpedb_types::schema::Schema;
use mpedb_types::{ColumnType, Dialect, Result, Value};

/// A synthesised catalog relation, as the wire server sees it.
pub struct CatalogRelation {
    /// `pg_catalog` or `information_schema`.
    pub schema: &'static str,
    /// Unqualified relation name.
    pub name: &'static str,
    /// Columns in ordinal order.
    pub columns: &'static [(&'static str, ColumnType)],
}

/// Every relation mpedb synthesises, in a stable order.
pub fn catalog_relations() -> Vec<CatalogRelation> {
    catalog::relations()
        .iter()
        .map(|r| CatalogRelation {
            schema: r.schema.as_str(),
            name: r.name,
            columns: r.columns,
        })
        .collect()
}

/// The rows a relation has for a given schema, or `None` if the name is not a
/// catalog relation.
pub fn catalog_rows(qualified_or_bare: &str, schema: &Schema) -> Option<Vec<Vec<Value>>> {
    catalog::lookup(qualified_or_bare).map(|r| (r.rows)(schema))
}

/// Which catalog relations a statement NAMES, as `(schema, name)` pairs.
///
/// Identifier positions only — the statement is tokenised, so a string literal
/// containing `pg_class` does not count. That distinction is not pedantry: the
/// C-API shim learned it the hard way on `SELECT name FROM sqlite_master WHERE
/// … AND NOT name='sqlite_sequence'`, where matching the literal answered the
/// query with the wrong table and Django silently created a migration table
/// twice.
///
/// A statement that does not parse is not an error here — it returns nothing
/// and the ordinary compile path produces the real diagnostic.
pub fn catalog_references(sql: &str) -> Vec<(&'static str, &'static str)> {
    use crate::token::{tokenize_dialect, Tok};
    let Ok(toks) = tokenize_dialect(sql, Dialect::Postgres) else {
        return Vec::new();
    };
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        // A qualified name is three tokens: ident `.` ident.
        let (name, next) = match (&toks[i].tok, toks.get(i + 1).map(|t| &t.tok), toks.get(i + 2).map(|t| &t.tok)) {
            (Tok::Ident(q), Some(Tok::Dot), Some(Tok::Ident(b))) => (format!("{q}.{b}"), i + 3),
            (Tok::Ident(b), _, _) => (b.clone(), i + 1),
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(rel) = catalog::lookup(&name) {
            let pair = (rel.schema.as_str(), rel.name);
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
        i = next;
    }
    out
}

/// Resolve a PostgreSQL type name to an mpedb column type, or refuse by name.
pub fn column_type_of(decl: &str) -> Result<ColumnType> {
    crate::pg::types::column_type(decl)
}

/// The PostgreSQL version mpedb reports compatibility with — the version the
/// differential suite measures against, so raising it means re-measuring.
pub fn compat_pg_version() -> &'static str {
    crate::pg::funcs::COMPAT_PG_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_pg_catalog_name_is_found_and_a_bare_information_schema_name_is_not() {
        // Matching PostgreSQL: pg_catalog is on the implicit search_path and
        // information_schema is not, so a user table called `tables` is safe.
        assert_eq!(catalog_references("SELECT * FROM pg_class"), vec![("pg_catalog", "pg_class")]);
        assert!(catalog_references("SELECT * FROM tables").is_empty());
        assert_eq!(
            catalog_references("SELECT * FROM information_schema.tables"),
            vec![("information_schema", "tables")]
        );
    }

    #[test]
    fn a_string_literal_naming_a_catalog_relation_does_not_count() {
        // The exact bug class the C-API shim hit with sqlite_sequence: matching
        // the literal answered the query with the wrong table, silently.
        assert!(catalog_references("SELECT 'pg_class' AS x").is_empty());
        assert!(catalog_references("SELECT * FROM t WHERE name = 'pg_class'").is_empty());
    }

    #[test]
    fn a_join_across_several_catalog_relations_reports_all_of_them() {
        // This is the shape `psql \\d` sends, and it is the reason the rows must
        // be materialised as real tables rather than spliced in as CTEs.
        let got = catalog_references(
            "SELECT c.relname, a.attname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = c.oid",
        );
        assert!(got.contains(&("pg_catalog", "pg_class")), "{got:?}");
        assert!(got.contains(&("pg_catalog", "pg_namespace")), "{got:?}");
        assert!(got.contains(&("pg_catalog", "pg_attribute")), "{got:?}");
        assert_eq!(got.len(), 3, "each relation once: {got:?}");
    }

    #[test]
    fn a_qualified_name_does_not_also_match_its_bare_tail_twice() {
        let got = catalog_references("SELECT * FROM pg_catalog.pg_class");
        assert_eq!(got, vec![("pg_catalog", "pg_class")]);
    }

    #[test]
    fn every_relation_can_be_asked_for_its_rows_by_name() {
        let schema = Schema::new(Vec::new()).unwrap();
        for rel in catalog_relations() {
            let qualified = format!("{}.{}", rel.schema, rel.name);
            assert!(
                catalog_rows(&qualified, &schema).is_some(),
                "{qualified} has no rows function"
            );
        }
        assert!(catalog_rows("not_a_catalog_relation", &schema).is_none());
    }
}
