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

/// Render a user enum type as an SQL identifier, preserving quoted names.
pub fn enum_type_name(e: &mpedb_types::pgtype::PgEnum) -> String {
    let plain = crate::token::tokenize_dialect(&e.name, Dialect::Postgres).ok()
        .is_some_and(|tokens| tokens.len() == 1 && matches!(&tokens[0].tok, crate::token::Tok::Ident(n) if n == &e.name))
        && e.name == e.name.to_ascii_lowercase();
    if plain { e.name.clone() } else { format!("\"{}\"", e.name.replace('"', "\"\"")) }
}

/// Stored view metadata supplied by the database owning its definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogView {
    pub name: String,
    pub definition: String,
    /// None when the current compiler cannot describe the definition.
    pub columns: Option<Vec<crate::PgOutputColumn>>,
}

/// Catalog rows for stored views; no view is executed to construct these.
pub fn view_catalog_rows(name: &str, views: &[CatalogView], temporary: bool) -> Result<Vec<Vec<Value>>> {
    catalog::view_rows(name, views, temporary)
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

/// Catalog rows belonging to the connection-private temporary schema.
pub fn temporary_catalog_rows(name: &str, schema: &Schema) -> Option<Vec<Vec<Value>>> {
    catalog::temporary_rows(name, schema)
}

/// Recognize only a standalone SELECT nextval(literal-or-parameter), never a
/// larger expression whose filtering or evaluation count would be discarded.
pub fn serial_nextval_call(sql: &str, params: &[Value]) -> Option<(String, String)> {
    use crate::ast::{Expr, Stmt};
    if !sql.as_bytes().windows(7).any(|s| s.eq_ignore_ascii_case(b"nextval")) { return None; }
    let (stmt, _, _, ctes) = crate::parser::parse_statement_ctes_dialect(
        sql, &[], &[], &crate::binder::OpSet::default(), Dialect::Postgres,
    ).ok()?;
    if !ctes.is_empty() { return None; }
    let Stmt::Select(s) = stmt else { return None; };
    if s.table.is_some() || s.from_derived.is_some() || s.from_series.is_some() || !s.joins.is_empty()
        || s.where_clause.is_some() || !s.group_by.is_empty() || s.having.is_some()
        || !s.order_by.is_empty() || s.limit.is_some() || s.offset.is_some() || s.distinct {
        return None;
    }
    let items = s.items?;
    let [(Expr::Func(name, args), alias)] = items.as_slice() else { return None; };
    if !name.eq_ignore_ascii_case("nextval") || args.len() != 1 { return None; }
    let mut argument = &args[0];
    if let Expr::Cast(inner, ty) = argument {
        if !ty.eq_ignore_ascii_case("regclass") { return None; }
        argument = inner;
    }
    let value = match argument {
        Expr::Lit(v) => v,
        Expr::Param(n) => params.get(*n as usize)?,
        _ => return None,
    };
    let Value::Text(sequence) = value else { return None; };
    Some((sequence.clone(), alias.clone().unwrap_or_else(|| "nextval".into())))
}

/// Session functions must not discard a FROM, filter, or surrounding expression.
/// Only literal and bound arguments can be evaluated outside the SQL engine.
pub fn session_config_call(sql: &str, params: &[Value]) -> Option<(Vec<Value>, String)> {
    use crate::ast::{Expr, Stmt};
    if !sql
        .as_bytes()
        .windows(10)
        .any(|s| s.eq_ignore_ascii_case(b"set_config"))
    {
        return None;
    }
    let (stmt, _, _, ctes) = crate::parser::parse_statement_ctes_dialect(
        sql,
        &[],
        &[],
        &crate::binder::OpSet::default(),
        Dialect::Postgres,
    )
    .ok()?;
    if !ctes.is_empty() {
        return None;
    }
    let Stmt::Select(s) = stmt else {
        return None;
    };
    if s.table.is_some()
        || s.from_derived.is_some()
        || s.from_series.is_some()
        || !s.joins.is_empty()
        || s.where_clause.is_some()
        || !s.group_by.is_empty()
        || s.having.is_some()
        || !s.order_by.is_empty()
        || s.limit.is_some()
        || s.offset.is_some()
        || s.distinct
    {
        return None;
    }
    let items = s.items?;
    let [(Expr::Func(name, args), alias)] = items.as_slice() else {
        return None;
    };
    if !name.eq_ignore_ascii_case("set_config") {
        return None;
    }
    let values = args
        .iter()
        .map(|arg| match arg {
            Expr::Lit(v) => Some(v.clone()),
            Expr::Param(n) => params.get(*n as usize).cloned(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((values, alias.clone().unwrap_or_else(|| "set_config".into())))
}

/// A regclass-style name, respecting quoted identifiers and schema qualification.
pub fn serial_regclass_name(name: &str) -> Option<(Option<String>, String)> {
    use crate::token::{tokenize_dialect, Tok};
    let tokens = tokenize_dialect(name, Dialect::Postgres).ok()?;
    let ident = |t: &Tok| match t {
        Tok::Ident(n) => Some(n.to_ascii_lowercase()),
        Tok::QuotedIdent(n, true) => Some(n.clone()),
        _ => None,
    };
    match tokens.as_slice() {
        [name] => Some((None, ident(&name.tok)?)),
        [schema, dot, name] if dot.tok == Tok::Dot => Some((Some(ident(&schema.tok)?), ident(&name.tok)?)),
        _ => None,
    }
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
/// `pg_description` rows for the comments mpedb has stored.
///
/// Separate from [`catalog_rows`] because a comment is NOT in the schema: it is
/// a sys-keyspace record, and only the caller holding the database can read it
/// (`Database::list_comments`). Pass those pairs here and this addresses them
/// the way PostgreSQL does — `objsubid` is the attnum for a column comment and
/// `0` for a table's.
#[cfg(feature = "pg-dialect")]
pub fn description_rows(schema: &Schema, comments: &[(String, String)]) -> Vec<Vec<Value>> {
    super::catalog::pgcat::description_rows(schema, comments)
}

/// Without the feature there is no catalog to describe.
#[cfg(not(feature = "pg-dialect"))]
pub fn description_rows(_schema: &Schema, _comments: &[(String, String)]) -> Vec<Vec<Value>> {
    Vec::new()
}

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
