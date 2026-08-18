//! The session's `pg_catalog` / `information_schema`, materialised on demand.
//!
//! # Why materialised, and why in a separate database
//!
//! The rows themselves come from `mpedb_sql::pg::api` — that module knows what
//! `pg_class` contains. What it cannot decide is how a client's query REACHES
//! them, and that turned out to be the whole design question:
//!
//! - As CTEs: works for one relation, and fails the moment two are joined,
//!   because a CTE in join position is *spliced* onto its body's base table and
//!   a `UNION ALL` body has none. Measured, not assumed.
//! - As real tables in the USER's database: joins work, but every database
//!   grows twenty tables it never asked for, visible in its own catalog, in its
//!   own dumps, and in its own file.
//! - As real tables in a **session-private in-memory database**: joins work,
//!   the user's file is untouched, and the whole thing evaporates when the
//!   connection ends. This is what mpedb-pg does.
//!
//! # The honest limit
//!
//! A statement that names BOTH a catalog relation and a user table cannot run:
//! they live in different databases. That is a NAMED refusal rather than a
//! wrong answer, and it costs nothing real — `psql \d`, SQLAlchemy's reflection
//! and Django's introspection all query the catalog alone. `SELECT * FROM
//! users, pg_class` is not a query anyone writes.
//!
//! # Freshness
//!
//! The catalog is rebuilt when the user database's schema generation moves, so
//! a `CREATE TABLE` on one connection is visible to the next `\d` on another.
//! Nothing is cached across the change.

use mpedb::{Config, Database, Value};
use mpedb_types::Schema;
use mpedb_types::ColumnType;

/// A session's private catalog database.
pub struct SessionCatalog {
    db: Option<Database>,
    /// The schema generation the materialised rows were built from. A change
    /// means every relation is stale — mpedb has no per-table catalog version,
    /// and inventing one to save a rebuild of a few hundred rows would be the
    /// wrong trade.
    built_for_gen: u64,
}

impl SessionCatalog {
    pub fn new() -> SessionCatalog {
        SessionCatalog {
            db: None,
            built_for_gen: u64::MAX,
        }
    }

    /// Ensure the catalog reflects `schema`, building or rebuilding as needed,
    /// and return the database to run the client's statement against.
    ///
    /// Building is LAZY: an application that never introspects never pays for
    /// twenty tables it does not read. That matters here more than it would in
    /// a resident server, because mpedb-pg is one process per connection — the
    /// cost would be paid at every connect.
    /// `comments` are the sys-keyspace COMMENT records
    /// (`Database::list_comments`), which `pg_description` is built from: they
    /// are not in the schema, so the caller — which holds the user database —
    /// has to hand them over. A DDL commit bumps `schema_gen`, and a
    /// `COMMENT ON` bumps it too, so the same cache key covers both.
    pub fn ensure(
        &mut self,
        schema: &Schema,
        schema_gen: u64,
        comments: &[(String, String)],
    ) -> Result<&Database, String> {
        if self.db.is_none() || self.built_for_gen != schema_gen {
            self.db = Some(build(schema, comments)?);
            self.built_for_gen = schema_gen;
        }
        Ok(self.db.as_ref().expect("just built"))
    }
}

impl Default for SessionCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// The name a catalog relation gets as a real table.
///
/// Qualified names are flattened with an underscore because mpedb has one
/// namespace: `information_schema.columns` becomes `information_schema_columns`.
/// The statement is rewritten to match (see [`rewrite`]), so the client never
/// sees this spelling.
pub fn table_name(schema: &str, name: &str) -> String {
    if schema == "pg_catalog" {
        // pg_catalog names are already globally unique and already carry their
        // prefix; renaming them would only make the rewrite harder to read.
        name.to_string()
    } else {
        format!("{schema}_{name}")
    }
}

fn build(schema: &Schema, comments: &[(String, String)]) -> Result<Database, String> {
    // A ZERO-table seed is legal (CLAUDE.md): the tables arrive via live DDL
    // below, which is exactly the shape this needs — the relation set is not
    // known until `catalog_relations()` is walked.
    let cfg = Config::from_toml_str(
        "[database]\npath = \":memory:\"\nsize_mb = 64\ndurability = \"none\"\n",
    )
    .map_err(|e| format!("catalog config: {e}"))?;
    let db = Database::open_in_memory(cfg).map_err(|e| format!("catalog database: {e}"))?;

    for rel in mpedb_sql::pg::api::catalog_relations() {
        let table = table_name(rel.schema, rel.name);
        let cols: Vec<String> = rel
            .columns
            .iter()
            .map(|(n, t)| format!("{} {}", quote_ident(n), ddl_type(*t)))
            .collect();
        // A relation with an `oid` column MUST declare it as the primary key.
        // Not a stylistic choice: mpedb reserves `oid` as an alias for the
        // implicit rowid (#94), so a table without a declared PK that also has
        // a column called `oid` is refused by name. Declaring it is also
        // correct — `oid` is the natural key of every catalog that has one, and
        // it is what pg_index/pg_constraint join against.
        //
        // Relations without an `oid` (pg_attribute, pg_index, the whole of
        // information_schema) take the implicit rowid: their natural keys are
        // composite, and declaring the wrong one would refuse legal rows.
        let pk = if rel.columns.iter().any(|(n, _)| *n == "oid") {
            ", PRIMARY KEY (\"oid\")"
        } else {
            ""
        };
        let ddl = format!(
            "CREATE TABLE {} ({}{pk})",
            quote_ident(&table),
            cols.join(", ")
        );
        db.query(&ddl, &[])
            .map_err(|e| format!("catalog table {table}: {e}"))?;

        // `pg_description` is the one relation whose rows do not come from the
        // schema: a comment is a sys-keyspace record the caller read for us.
        let rows = if rel.name == "pg_description" {
            mpedb_sql::pg::api::description_rows(schema, comments)
        } else {
            match mpedb_sql::pg::api::catalog_rows(
                &format!("{}.{}", rel.schema, rel.name),
                schema,
            ) {
                Some(r) => r,
                None => continue,
            }
        };
        if rows.is_empty() {
            continue;
        }
        let placeholders = (1..=rel.columns.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let insert = format!(
            "INSERT INTO {} VALUES ({placeholders})",
            quote_ident(&table)
        );
        let mut w = db
            .begin()
            .map_err(|e| format!("catalog write for {table}: {e}"))?;
        for row in &rows {
            w.query(&insert, row)
                .map_err(|e| format!("catalog row for {table}: {e}"))?;
        }
        w.commit()
            .map_err(|e| format!("catalog commit for {table}: {e}"))?;
    }
    // The DDL and the inserts above are mpedb's OWN SQL and are written in the
    // sqlite dialect deliberately. The client's statements are not — they are
    // PostgreSQL, `!~` and `pg_catalog.f()` and all — so the dialect flips only
    // once the catalog is populated.
    //
    // Missing this is not a subtle failure: `psql \d` filters with
    // `n.nspname !~ '^pg_toast'`, which under the sqlite dialect is a parse
    // error pointing at a byte offset in a query the user never wrote.
    let mut db = db;
    db.set_dialect(mpedb_types::Dialect::Postgres);
    Ok(db)
}

fn ddl_type(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Int64 => "BIGINT",
        ColumnType::Float64 => "DOUBLE",
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Text => "TEXT",
        ColumnType::Blob => "BLOB",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Date => "DATE",
        ColumnType::Time => "TIME",
        ColumnType::Numeric => "NUMERIC",
        ColumnType::Any => "ANY",
    }
}

/// mpedb's identifiers are case-preserving and compared case-insensitively, but
/// a catalog column called `end` or `order` would still collide with a keyword.
/// Quoting unconditionally is cheaper than maintaining a keyword list that has
/// to track the grammar.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Rewrite a statement's catalog references to the materialised table names.
///
/// Only the qualified forms need rewriting (`information_schema.columns` →
/// `information_schema_columns`); a bare `pg_class` already IS the table name.
/// The rewrite is textual but anchored on the references the tokenizer found,
/// so a string literal is never touched.
pub fn rewrite(sql: &str, refs: &[(&'static str, &'static str)]) -> String {
    let mut out = sql.to_string();
    for (schema, name) in refs {
        if *schema == "pg_catalog" {
            // `pg_catalog.pg_class` → `pg_class`; the bare form needs nothing.
            out = replace_ident_ci(&out, &format!("{schema}.{name}"), name);
        } else {
            out = replace_ident_ci(&out, &format!("{schema}.{name}"), &table_name(schema, name));
        }
    }
    out
}

/// Case-insensitive replacement of a dotted identifier, respecting identifier
/// boundaries so `information_schema.tables_extra` is not mangled by a rule for
/// `information_schema.tables`.
fn replace_ident_ci(hay: &str, needle: &str, with: &str) -> String {
    let lower_hay = hay.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = String::with_capacity(hay.len());
    let mut at = 0usize;
    while let Some(p) = lower_hay[at..].find(&lower_needle) {
        let start = at + p;
        let end = start + needle.len();
        let before_ok = start == 0 || !ident(lower_hay.as_bytes()[start - 1]);
        let after_ok = end >= hay.len() || !ident(lower_hay.as_bytes()[end]);
        out.push_str(&hay[at..start]);
        if before_ok && after_ok {
            out.push_str(with);
        } else {
            out.push_str(&hay[start..end]);
        }
        at = end;
    }
    out.push_str(&hay[at..]);
    out
}

/// What a statement's catalog references mean for routing.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// No catalog relation named — the user's database answers it.
    User,
    /// Only catalog relations — the session catalog answers it.
    Catalog(Vec<(&'static str, &'static str)>),
}

/// Decide where a statement runs.
///
/// A statement naming a catalog relation goes to the catalog database. Whether
/// it ALSO names a user table cannot be decided here without binding it — so it
/// is not guessed: the statement is sent to the catalog, and an unknown table
/// there produces the ordinary `42P01`, which is the truthful answer (that
/// table does not exist in the relation set this query is being answered from).
pub fn route(sql: &str) -> Route {
    let refs = mpedb_sql::pg::api::catalog_references(sql);
    if refs.is_empty() {
        Route::User
    } else {
        Route::Catalog(refs)
    }
}

/// Values are inserted through the ordinary parameter path, so this only exists
/// to keep the type visible to callers building rows by hand in tests.
pub type Row = Vec<Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_with_no_catalog_reference_routes_to_the_user_database() {
        assert_eq!(route("SELECT * FROM users"), Route::User);
        assert_eq!(route("INSERT INTO t VALUES (1)"), Route::User);
        // A literal must not route: this is the sqlite_sequence bug class.
        assert_eq!(route("SELECT 'pg_class'"), Route::User);
    }

    #[test]
    fn a_catalog_statement_routes_to_the_catalog_with_every_relation_named() {
        let Route::Catalog(refs) = route("SELECT relname FROM pg_class") else {
            panic!("should route to catalog")
        };
        assert_eq!(refs, vec![("pg_catalog", "pg_class")]);
    }

    #[test]
    fn qualified_information_schema_names_are_rewritten_and_bare_pg_names_are_not() {
        let refs = mpedb_sql::pg::api::catalog_references(
            "SELECT * FROM information_schema.columns WHERE table_name = 'users'",
        );
        let got = rewrite(
            "SELECT * FROM information_schema.columns WHERE table_name = 'users'",
            &refs,
        );
        assert_eq!(
            got,
            "SELECT * FROM information_schema_columns WHERE table_name = 'users'"
        );

        let refs = mpedb_sql::pg::api::catalog_references("SELECT * FROM pg_catalog.pg_class");
        assert_eq!(
            rewrite("SELECT * FROM pg_catalog.pg_class", &refs),
            "SELECT * FROM pg_class"
        );
        // A bare pg_ name is already the table name.
        let refs = mpedb_sql::pg::api::catalog_references("SELECT * FROM pg_class");
        assert_eq!(rewrite("SELECT * FROM pg_class", &refs), "SELECT * FROM pg_class");
    }

    #[test]
    fn the_rewrite_respects_identifier_boundaries() {
        // A rule for `information_schema.tables` must not mangle
        // `information_schema.tables_extra`.
        let got = replace_ident_ci(
            "SELECT * FROM information_schema.tables_extra",
            "information_schema.tables",
            "information_schema_tables",
        );
        assert_eq!(got, "SELECT * FROM information_schema.tables_extra");
    }

    #[test]
    fn the_rewrite_is_case_insensitive_like_every_other_identifier_match() {
        let got = replace_ident_ci(
            "SELECT * FROM INFORMATION_SCHEMA.COLUMNS",
            "information_schema.columns",
            "information_schema_columns",
        );
        assert_eq!(got, "SELECT * FROM information_schema_columns");
    }

    /// The load-bearing one: the whole reason the catalog is materialised
    /// rather than spliced is that clients JOIN several relations, which CTEs
    /// cannot do here. If this passes, the design works.
    #[test]
    fn several_catalog_relations_can_be_joined_in_one_query() {
        let schema = crate::testutil::sample_schema();
        let mut cat = SessionCatalog::new();
        let db = cat.ensure(&schema, 1, &[]).expect("catalog builds");
        let got = db
            .query(
                "SELECT c.relname, n.nspname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'r' ORDER BY c.relname",
                &[],
            )
            .expect("the join must plan and run");
        let mpedb::ExecResult::Rows { rows, .. } = got else {
            panic!("expected rows")
        };
        let names: Vec<String> = rows
            .iter()
            .map(|r| match &r[0] {
                Value::Text(s) => s.clone(),
                v => panic!("{v:?}"),
            })
            .collect();
        assert_eq!(names, vec!["memberships", "users"]);
        for r in &rows {
            assert_eq!(r[1], Value::Text("public".into()));
        }
    }

    #[test]
    fn a_three_way_join_of_the_shape_psql_backslash_d_sends_also_works() {
        let schema = crate::testutil::sample_schema();
        let mut cat = SessionCatalog::new();
        let db = cat.ensure(&schema, 1, &[]).expect("catalog builds");
        let got = db
            .query(
                "SELECT a.attname FROM pg_class c \
                 JOIN pg_attribute a ON a.attrelid = c.oid \
                 WHERE c.relname = 'users' AND a.attnum > 0 \
                 ORDER BY a.attnum",
                &[],
            )
            .expect("three-way catalog query");
        let mpedb::ExecResult::Rows { rows, .. } = got else {
            panic!("expected rows")
        };
        let names: Vec<String> = rows
            .iter()
            .map(|r| match &r[0] {
                Value::Text(s) => s.clone(),
                v => panic!("{v:?}"),
            })
            .collect();
        assert_eq!(names, vec!["id", "email", "nick"]);
    }

    #[test]
    fn the_catalog_is_rebuilt_when_the_schema_generation_moves() {
        let schema = crate::testutil::sample_schema();
        let mut cat = SessionCatalog::new();
        cat.ensure(&schema, 1, &[]).unwrap();
        assert_eq!(cat.built_for_gen, 1);
        // A DDL on another connection bumps the generation; the next statement
        // here must not answer from the old rows.
        cat.ensure(&schema, 2, &[]).unwrap();
        assert_eq!(cat.built_for_gen, 2);
    }

    #[test]
    fn information_schema_relations_are_queryable_under_their_flattened_names() {
        let schema = crate::testutil::sample_schema();
        let mut cat = SessionCatalog::new();
        let db = cat.ensure(&schema, 1, &[]).unwrap();
        let got = db
            .query(
                "SELECT table_name FROM information_schema_tables ORDER BY table_name",
                &[],
            )
            .expect("information_schema query");
        let mpedb::ExecResult::Rows { rows, .. } = got else {
            panic!()
        };
        assert_eq!(rows.len(), 2);
    }
}
