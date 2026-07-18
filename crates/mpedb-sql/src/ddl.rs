//! Row-level-security DDL statements (DESIGN-MULTIDB.md §3.1). These do not
//! compile to a [`CompiledPlan`](crate::CompiledPlan) — they mutate the catalog
//! sys-keyspace — so the facade intercepts them before planning and applies
//! them via its policy-store API. `USING`/`WITH CHECK` predicates are captured
//! as SOURCE text (re-bound by the planner per statement, §3.2).

use mpedb_types::{ColumnType, PolicyCmd};

/// One column of a `CREATE TABLE` (#47 stage 2). Types are the config's
/// names (`int64`/`int`/`integer`, `text`, `real`, `bool`, `blob`,
/// `timestamp`, `any`), constraints the useful subset: `NOT NULL`,
/// `UNIQUE`, `PRIMARY KEY`. `DEFAULT`/`CHECK` are named refusals for now.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateColumnSpec {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub unique: bool,
    pub pk: bool,
}

/// `CREATE TABLE <name> (col TYPE [cons…], …[, PRIMARY KEY (a, b)]
/// [, UNIQUE (a, b)]…)` — applied by the facade as a catalog mutation
/// under the writer lock, never compiled to a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableSpec {
    pub name: String,
    pub columns: Vec<CreateColumnSpec>,
    /// Table-level `PRIMARY KEY (…)`; empty when a column carries the
    /// inline `PRIMARY KEY` flag instead.
    pub table_pk: Vec<String>,
    /// Table-level `UNIQUE (…)` groups — composite unique indexes (#55).
    pub uniques: Vec<Vec<String>>,
}

/// `CREATE POLICY <name> ON <table> [AS PERMISSIVE|RESTRICTIVE]
///   [FOR ALL|SELECT|INSERT|UPDATE|DELETE] USING (<expr>) [WITH CHECK (<expr>)]`
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePolicySpec {
    pub name: String,
    pub table: String,
    pub command: PolicyCmd,
    pub permissive: bool,
    pub using_src: Option<String>,
    pub check_src: Option<String>,
}

/// `ALTER TABLE <t> {ENABLE | FORCE | DISABLE} ROW LEVEL SECURITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlsAction {
    Enable { force: bool },
    Disable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DdlStmt {
    CreateTable(CreateTableSpec),
    /// `DROP TABLE [IF EXISTS] <name>` (#47 stage 4) — applied by the facade as
    /// a catalog mutation: the slot is tombstoned in place and its id is never
    /// reused (DESIGN-DROP-TABLE §0). `if_exists` suppresses the missing-table
    /// error, matching sqlite/PG.
    DropTable { name: String, if_exists: bool },
    /// `ALTER TABLE <t> RENAME TO <new>` (#47 stage 5) — pure schema metadata,
    /// no data rewrite (same id, same trees). Applied by the facade.
    AlterRenameTable { table: String, new_name: String },
    /// `ALTER TABLE <t> RENAME [COLUMN] <old> TO <new>` — pure schema metadata
    /// (column position/type unchanged, so no row is touched).
    AlterRenameColumn { table: String, column: String, new_name: String },
    /// `ALTER TABLE <t> ADD [COLUMN] <name> <type> [NULL]` (#47 stage 5) —
    /// appends a column. Existing rows are rewritten with the new column NULL
    /// (mpedb's row image is schema-driven, so a widen needs a rewrite). v1
    /// accepts only a nullable column (the facade refuses NOT NULL / UNIQUE /
    /// PRIMARY KEY on ADD — no DEFAULT fill and no online index build yet).
    AlterAddColumn { table: String, column: CreateColumnSpec },
    /// `ALTER TABLE <t> DROP [COLUMN] <name>` (#47 stage 5) — removes a column
    /// and rewrites existing rows without it. The facade/schema refuse dropping
    /// a PK column, an indexed column, or the last column (no online index
    /// rebuild, and a table needs its key).
    AlterDropColumn { table: String, column: String },
    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] <name> ON <table> (<col> [ASC|DESC],
    /// …)` — builds a secondary index over the existing rows. The index name is
    /// accepted but not persisted (mpedb indexes are positional); ASC/DESC per
    /// column is accepted and ignored (indexes are ascending, used for
    /// equality/range lookups only).
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        if_not_exists: bool,
    },
    CreatePolicy(CreatePolicySpec),
    DropPolicy { table: String, name: String },
    AlterRls { table: String, action: RlsAction },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_ddl;

    #[test]
    fn ordinary_sql_is_not_ddl() {
        assert_eq!(parse_ddl("SELECT * FROM orders").unwrap(), None);
        assert_eq!(parse_ddl("INSERT INTO t (id) VALUES (1)").unwrap(), None);
        // A quoted identifier "create" would be a column, not DDL — but bare
        // leading create/drop/alter route to DDL.
    }

    #[test]
    fn create_policy_captures_source_verbatim() {
        let ddl = parse_ddl(
            "CREATE POLICY tenant_iso ON orders AS RESTRICTIVE FOR UPDATE \
             USING ( tenant = current_setting('app.tenant') ) \
             WITH CHECK (tenant = current_setting('app.tenant') AND id < 100)",
        )
        .unwrap()
        .unwrap();
        match ddl {
            DdlStmt::CreatePolicy(s) => {
                assert_eq!(s.name, "tenant_iso");
                assert_eq!(s.table, "orders");
                assert!(!s.permissive);
                assert_eq!(s.command, PolicyCmd::Update);
                // Balanced source captured and trimmed, inner parens preserved.
                assert_eq!(s.using_src.as_deref(), Some("tenant = current_setting('app.tenant')"));
                assert_eq!(
                    s.check_src.as_deref(),
                    Some("tenant = current_setting('app.tenant') AND id < 100")
                );
            }
            other => panic!("expected CreatePolicy, got {other:?}"),
        }
    }

    #[test]
    fn alter_and_drop_parse() {
        assert_eq!(
            parse_ddl("ALTER TABLE orders ENABLE ROW LEVEL SECURITY").unwrap().unwrap(),
            DdlStmt::AlterRls { table: "orders".into(), action: RlsAction::Enable { force: false } }
        );
        assert_eq!(
            parse_ddl("ALTER TABLE orders FORCE ROW LEVEL SECURITY").unwrap().unwrap(),
            DdlStmt::AlterRls { table: "orders".into(), action: RlsAction::Enable { force: true } }
        );
        assert_eq!(
            parse_ddl("DROP POLICY p ON orders").unwrap().unwrap(),
            DdlStmt::DropPolicy { table: "orders".into(), name: "p".into() }
        );
    }

    #[test]
    fn alter_rename_parses() {
        assert_eq!(
            parse_ddl("ALTER TABLE orders RENAME TO invoices").unwrap().unwrap(),
            DdlStmt::AlterRenameTable { table: "orders".into(), new_name: "invoices".into() }
        );
        // `RENAME COLUMN a TO b` and the bare `RENAME a TO b` are equivalent.
        let with_kw =
            parse_ddl("ALTER TABLE orders RENAME COLUMN qty TO amount").unwrap().unwrap();
        let bare = parse_ddl("ALTER TABLE orders RENAME qty TO amount").unwrap().unwrap();
        assert_eq!(with_kw, bare);
        assert_eq!(
            with_kw,
            DdlStmt::AlterRenameColumn {
                table: "orders".into(),
                column: "qty".into(),
                new_name: "amount".into(),
            }
        );
        // The RLS ALTER forms still parse (RENAME branches off first).
        assert_eq!(
            parse_ddl("ALTER TABLE orders ENABLE ROW LEVEL SECURITY").unwrap().unwrap(),
            DdlStmt::AlterRls { table: "orders".into(), action: RlsAction::Enable { force: false } }
        );
        // Malformed RENAME COLUMN (no TO) errors.
        assert!(parse_ddl("ALTER TABLE orders RENAME COLUMN qty amount").is_err());
    }

    #[test]
    fn alter_add_column_parses() {
        // COLUMN keyword optional; type required; NULL/NOT NULL/UNIQUE captured.
        let with_kw = parse_ddl("ALTER TABLE t ADD COLUMN note TEXT").unwrap().unwrap();
        let bare = parse_ddl("ALTER TABLE t ADD note TEXT").unwrap().unwrap();
        assert_eq!(with_kw, bare);
        match with_kw {
            DdlStmt::AlterAddColumn { table, column } => {
                assert_eq!(table, "t");
                assert_eq!(column.name, "note");
                assert_eq!(column.ty, ColumnType::Text);
                assert!(!column.not_null && !column.unique && !column.pk);
            }
            other => panic!("expected AlterAddColumn, got {other:?}"),
        }
        // NOT NULL is captured (the facade, not the parser, refuses it on ADD).
        match parse_ddl("ALTER TABLE t ADD x INT NOT NULL").unwrap().unwrap() {
            DdlStmt::AlterAddColumn { column, .. } => assert!(column.not_null),
            other => panic!("{other:?}"),
        }
        // Unknown type / missing type error.
        assert!(parse_ddl("ALTER TABLE t ADD COLUMN c BOGUS").is_err());
        assert!(parse_ddl("ALTER TABLE t ADD COLUMN c").is_err());
    }

    #[test]
    fn alter_drop_column_parses() {
        // COLUMN keyword optional.
        let with_kw = parse_ddl("ALTER TABLE t DROP COLUMN a").unwrap().unwrap();
        let bare = parse_ddl("ALTER TABLE t DROP a").unwrap().unwrap();
        assert_eq!(with_kw, bare);
        assert_eq!(
            with_kw,
            DdlStmt::AlterDropColumn { table: "t".into(), column: "a".into() }
        );
    }

    #[test]
    fn create_index_parses() {
        assert_eq!(
            parse_ddl("CREATE INDEX ix ON t (a)").unwrap().unwrap(),
            DdlStmt::CreateIndex {
                name: "ix".into(),
                table: "t".into(),
                columns: vec!["a".into()],
                unique: false,
                if_not_exists: false,
            }
        );
        // UNIQUE, IF NOT EXISTS, composite, and per-column ASC/DESC (ignored).
        assert_eq!(
            parse_ddl("CREATE UNIQUE INDEX IF NOT EXISTS ix ON t (a, b DESC, c ASC)")
                .unwrap()
                .unwrap(),
            DdlStmt::CreateIndex {
                name: "ix".into(),
                table: "t".into(),
                columns: vec!["a".into(), "b".into(), "c".into()],
                unique: true,
                if_not_exists: true,
            }
        );
    }

    #[test]
    fn drop_table_parses() {
        assert_eq!(
            parse_ddl("DROP TABLE orders").unwrap().unwrap(),
            DdlStmt::DropTable { name: "orders".into(), if_exists: false }
        );
        assert_eq!(
            parse_ddl("DROP TABLE IF EXISTS orders").unwrap().unwrap(),
            DdlStmt::DropTable { name: "orders".into(), if_exists: true }
        );
    }

    #[test]
    fn malformed_ddl_errors() {
        assert!(parse_ddl("CREATE POLICY p ON orders").is_err()); // no USING/CHECK
        assert!(parse_ddl("CREATE POLICY p ON orders USING (").is_err()); // unbalanced
        assert!(parse_ddl("ALTER TABLE orders ENABLE").is_err()); // missing ROW LEVEL SECURITY
        assert!(parse_ddl("CREATE POLICY p ON orders FOR BOGUS USING (id = 1)").is_err());
    }

    fn create_table(sql: &str) -> CreateTableSpec {
        match parse_ddl(sql).unwrap().unwrap() {
            DdlStmt::CreateTable(s) => s,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn create_table_inline_pk_and_constraints() {
        let s = create_table(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, \
             age INT NULL)",
        );
        assert_eq!(s.name, "users");
        assert_eq!(s.columns.len(), 3);
        assert_eq!(s.columns[0].name, "id");
        assert_eq!(s.columns[0].ty, ColumnType::Int64);
        assert!(s.columns[0].pk);
        assert_eq!(s.columns[1].name, "email");
        assert_eq!(s.columns[1].ty, ColumnType::Text);
        assert!(s.columns[1].not_null && s.columns[1].unique);
        assert!(!s.columns[2].not_null); // explicit NULL
        assert!(s.table_pk.is_empty());
        assert!(s.uniques.is_empty());
    }

    #[test]
    fn create_table_composite_key_and_unique_groups() {
        let s = create_table(
            "CREATE TABLE lines (order_id INT, line_no INT, sku TEXT, qty INT, \
             PRIMARY KEY (order_id, line_no), UNIQUE (order_id, sku))",
        );
        assert_eq!(s.columns.len(), 4);
        assert!(s.columns.iter().all(|c| !c.pk));
        assert_eq!(s.table_pk, vec!["order_id", "line_no"]);
        assert_eq!(s.uniques, vec![vec!["order_id".to_string(), "sku".to_string()]]);
    }

    #[test]
    fn create_table_every_type_word() {
        let s = create_table(
            "CREATE TABLE t (a INT PRIMARY KEY, b int64, c integer, d text, e string, \
             f real, g float, h bool, i boolean, j blob, k bytes, l timestamp, m any)",
        );
        use ColumnType::*;
        let got: Vec<ColumnType> = s.columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            got,
            vec![Int64, Int64, Int64, Text, Text, Float64, Float64, Bool, Bool, Blob, Blob,
                 Timestamp, Any]
        );
    }

    #[test]
    fn create_table_malformed_and_unsupported_refuse() {
        assert!(parse_ddl("CREATE TABLE t (id INT PRIMARY KEY,)").is_err()); // trailing comma → empty col
        assert!(parse_ddl("CREATE TABLE t (id BOGUSTYPE)").is_err()); // unknown type
        assert!(parse_ddl("CREATE TABLE t (id INT DEFAULT 0)").is_err()); // DEFAULT unsupported
        assert!(parse_ddl("CREATE TABLE t (id INT CHECK (id > 0))").is_err()); // CHECK unsupported
        assert!(parse_ddl("CREATE TABLE t (id INT REFERENCES o(id))").is_err()); // FK unsupported
        assert!(parse_ddl("CREATE TABLE t id INT)").is_err()); // missing (
        assert!(parse_ddl("CREATE TABLE t (id INT").is_err()); // missing )
    }
}
