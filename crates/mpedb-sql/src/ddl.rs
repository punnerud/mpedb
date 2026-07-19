//! Row-level-security DDL statements (design/DESIGN-MULTIDB.md §3.1). These do not
//! compile to a [`CompiledPlan`](crate::CompiledPlan) — they mutate the catalog
//! sys-keyspace — so the facade intercepts them before planning and applies
//! them via its policy-store API. `USING`/`WITH CHECK` predicates are captured
//! as SOURCE text (re-bound by the planner per statement, §3.2).

use mpedb_types::{Collation, ColumnType, DefaultExpr, PolicyCmd};

/// One column of a `CREATE TABLE` (#47 stage 2) or `ALTER TABLE ADD COLUMN`.
/// The declared type is sqlite's whole vocabulary — any word(s) with an
/// optional size (`varchar(100)`, `bigint`, `double precision`, `decimal(10,2)`,
/// or nothing at all) — folded to a rigid [`ColumnType`] by
/// [`ColumnType::from_declared`]. Constraints are the useful subset: `NOT
/// NULL`, `UNIQUE`, `PRIMARY KEY`, `COLLATE`. `CHECK`/`REFERENCES` are named
/// refusals for now; `DEFAULT <const>` is parsed for ADD COLUMN (below) and
/// still refused by name in `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateColumnSpec {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub unique: bool,
    pub pk: bool,
    /// `COLLATE <name>` declared on the column (BINARY/NOCASE/RTRIM). The parser
    /// resolves the name (an unknown one is a clean error); the facade carries it
    /// onto the [`ColumnDef`](mpedb_types::ColumnDef). [`Collation::Binary`] when
    /// none was written.
    pub collation: Collation,
    /// `DEFAULT <const>` on `ALTER TABLE ADD COLUMN` — a LITERAL constant
    /// (integer/float/string/blob/bool/NULL or a signed number), folded at
    /// parse time (sqlite refuses a non-constant ADD-COLUMN default). Always a
    /// `DefaultExpr::Const`; the facade type-checks it against `ty` and, when
    /// non-NULL, fills existing rows with it. `None` when no `DEFAULT` clause
    /// (and always `None` on the `CREATE TABLE` path, which refuses DEFAULT).
    pub default: Option<DefaultExpr>,
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

/// `CREATE VIRTUAL TABLE [IF NOT EXISTS] <name> USING fts5(<col>, …
/// [, tokenize='unicode61'|'ascii'])` (design/DESIGN-FTS.md §1). Applied by the
/// facade like `CREATE TABLE`, but builds an FTS content + inverted-index table
/// (`TableKind::Fts`) with an auto `rowid` INTEGER primary key and the declared
/// columns as tokenized TEXT content.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateVirtualTableSpec {
    pub name: String,
    pub columns: Vec<String>,
    pub tokenizer: mpedb_types::Tokenizer,
    pub if_not_exists: bool,
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

/// When a trigger fires relative to the row operation (DESIGN-TRIGGERS §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
}

/// Which row event a trigger watches. `Update { of }` names the columns whose
/// change arms the trigger (`UPDATE OF a, b`); an empty list means any column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update { of: Vec<String> },
    Delete,
}

/// `CREATE TRIGGER [IF NOT EXISTS] <name> {BEFORE|AFTER}
///    {INSERT|UPDATE [OF cols]|DELETE} ON <table> [FOR EACH ROW]
///    [WHEN (<cond>)] BEGIN <stmt>; END` (DESIGN-TRIGGERS §1-2).
///
/// The `WHEN` predicate and the `BEGIN … END` body are captured as SOURCE text
/// (like `CREATE VIEW`'s SELECT and a policy predicate), re-compiled against the
/// live schema at catalog-load time. `EXECUTE PROCEDURE` (PySpell) bodies are a
/// named parse refusal until DESIGN-TRIGGERS stage 5, so the body is always SQL.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTriggerSpec {
    pub name: String,
    pub timing: TriggerTiming,
    pub event: TriggerEvent,
    pub table: String,
    /// Captured `WHEN (…)` predicate source, if any.
    pub when_src: Option<String>,
    /// Captured `BEGIN … END` body source — one or more `;`-separated
    /// INSERT/UPDATE/DELETE statements, split and compiled at apply/load time.
    pub body_sql: String,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DdlStmt {
    CreateTable(CreateTableSpec),
    /// `CREATE VIRTUAL TABLE … USING fts5(…)` (design/DESIGN-FTS.md §1).
    CreateVirtualTable(CreateVirtualTableSpec),
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
    /// `ALTER TABLE <t> ADD [COLUMN] <name> <type> [NOT NULL] [DEFAULT <const>]`
    /// (#47 stage 5) — appends a column. Existing rows are rewritten with the
    /// new column set to the `DEFAULT <const>` (or NULL when there is none;
    /// mpedb's row image is schema-driven, so a widen needs a rewrite). A
    /// non-NULL default makes `NOT NULL` legal and is persisted for later
    /// INSERTs. The facade refuses UNIQUE / PRIMARY KEY on ADD and NOT NULL
    /// without a non-NULL default — matching sqlite.
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
    /// `CREATE VIEW [IF NOT EXISTS] <name> AS <select>` (#73). The SELECT is
    /// captured as source text and re-parsed + flattened into referencing
    /// queries (design/DESIGN-VIEW.md). Applied by the facade as a catalog mutation.
    CreateView { name: String, select_sql: String, if_not_exists: bool },
    /// `DROP VIEW [IF EXISTS] <name>` (#73).
    DropView { name: String, if_exists: bool },
    CreatePolicy(CreatePolicySpec),
    DropPolicy { table: String, name: String },
    AlterRls { table: String, action: RlsAction },
    /// `CREATE TRIGGER …` (DESIGN-TRIGGERS). Stored as a sys-keyspace catalog
    /// record (`trigger/<name>`), never a plan — like `CREATE VIEW`.
    CreateTrigger(CreateTriggerSpec),
    /// `DROP TRIGGER [IF EXISTS] <name>` (DESIGN-TRIGGERS).
    DropTrigger { name: String, if_exists: bool },
    /// `ANALYZE [<name>]` — sqlite gathers optimizer statistics here; mpedb's
    /// planner is rule-based (PK > unique > non-unique index > scan) and keeps no
    /// statistics, so there is nothing to gather. Accepted as a no-op success so
    /// tools/migrations that emit ANALYZE do not break. The optional target name
    /// (a table, index, or schema) is captured and ignored — it is NOT required
    /// to exist (leniency is never a wrong answer, and matches sqlite's success).
    Analyze { name: Option<String> },
    /// `REINDEX [<name>]` — sqlite rebuilds indexes; mpedb maintains every index
    /// eagerly on each write, so there is never a stale index to rebuild.
    /// Accepted as a no-op success. The optional target is captured and ignored:
    /// a table name and an index name are indistinguishable at parse time (mpedb
    /// does not persist index names — indexes are positional), so accepting
    /// leniently is the safe choice and never a wrong answer.
    Reindex { target: Option<String> },
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
        // An unrecognized type word is LEGAL, as in sqlite, and means NUMERIC
        // affinity → `Any`. (It used to be a parse error.)
        match parse_ddl("ALTER TABLE t ADD COLUMN c BOGUS").unwrap().unwrap() {
            DdlStmt::AlterAddColumn { column, .. } => assert_eq!(column.ty, ColumnType::Any),
            other => panic!("{other:?}"),
        }
        // A TYPELESS ADD COLUMN is valid too, sqlite-style: `Any` (no affinity).
        match parse_ddl("ALTER TABLE t ADD COLUMN c").unwrap().unwrap() {
            DdlStmt::AlterAddColumn { column, .. } => {
                assert_eq!(column.name, "c");
                assert_eq!(column.ty, ColumnType::Any);
            }
            other => panic!("expected typeless AlterAddColumn, got {other:?}"),
        }
    }

    #[test]
    fn alter_add_column_default_literals() {
        use mpedb_types::Value;
        let default = |sql: &str| match parse_ddl(sql).unwrap().unwrap() {
            DdlStmt::AlterAddColumn { column, .. } => column.default,
            other => panic!("{other:?}"),
        };
        // Integer / signed / float / string / bool / NULL literals fold to Const.
        assert_eq!(
            default("ALTER TABLE t ADD c INT NOT NULL DEFAULT 5"),
            Some(DefaultExpr::Const(Value::Int(5)))
        );
        assert_eq!(
            default("ALTER TABLE t ADD c INT DEFAULT -7"),
            Some(DefaultExpr::Const(Value::Int(-7)))
        );
        assert_eq!(
            default("ALTER TABLE t ADD c REAL DEFAULT 1.5"),
            Some(DefaultExpr::Const(Value::Float(1.5)))
        );
        assert_eq!(
            default("ALTER TABLE t ADD c TEXT DEFAULT 'x'"),
            Some(DefaultExpr::Const(Value::Text("x".into())))
        );
        assert_eq!(
            default("ALTER TABLE t ADD c BOOL DEFAULT true"),
            Some(DefaultExpr::Const(Value::Bool(true)))
        );
        assert_eq!(
            default("ALTER TABLE t ADD c INT DEFAULT NULL"),
            Some(DefaultExpr::Const(Value::Null))
        );
        // No DEFAULT clause → None.
        assert_eq!(default("ALTER TABLE t ADD c INT"), None);
        // A non-constant default (parenthesized expr, function, column ref,
        // CURRENT_*) is refused at parse time, matching sqlite.
        assert!(parse_ddl("ALTER TABLE t ADD c INT DEFAULT (1+2)").is_err());
        assert!(parse_ddl("ALTER TABLE t ADD c INT DEFAULT abs(-5)").is_err());
        assert!(parse_ddl("ALTER TABLE t ADD c INT DEFAULT other").is_err());
        assert!(parse_ddl("ALTER TABLE t ADD c TEXT DEFAULT current_timestamp").is_err());
        assert!(parse_ddl("ALTER TABLE t ADD c INT DEFAULT").is_err());
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
    fn create_trigger_after_insert_parses() {
        let ddl = parse_ddl(
            "CREATE TRIGGER audit_ins AFTER INSERT ON orders FOR EACH ROW \
             BEGIN INSERT INTO audit (oid) VALUES (NEW.id); END",
        )
        .unwrap()
        .unwrap();
        match ddl {
            DdlStmt::CreateTrigger(s) => {
                assert_eq!(s.name, "audit_ins");
                assert_eq!(s.timing, TriggerTiming::After);
                assert_eq!(s.event, TriggerEvent::Insert);
                assert_eq!(s.table, "orders");
                assert!(s.when_src.is_none());
                assert!(!s.if_not_exists);
                assert_eq!(s.body_sql, "INSERT INTO audit (oid) VALUES (NEW.id)");
            }
            other => panic!("expected CreateTrigger, got {other:?}"),
        }
    }

    #[test]
    fn create_trigger_when_and_if_not_exists_and_case_body() {
        let ddl = parse_ddl(
            "CREATE TRIGGER IF NOT EXISTS t AFTER INSERT ON orders WHEN (NEW.total > 100) \
             BEGIN INSERT INTO big (v) VALUES (CASE WHEN NEW.total > 200 THEN 2 ELSE 1 END); END",
        )
        .unwrap()
        .unwrap();
        match ddl {
            DdlStmt::CreateTrigger(s) => {
                assert!(s.if_not_exists);
                assert_eq!(s.when_src.as_deref(), Some("NEW.total > 100"));
                // The CASE … END inside the body must NOT end the BEGIN … END capture.
                assert_eq!(
                    s.body_sql,
                    "INSERT INTO big (v) VALUES (CASE WHEN NEW.total > 200 THEN 2 ELSE 1 END)"
                );
            }
            other => panic!("expected CreateTrigger, got {other:?}"),
        }
    }

    #[test]
    fn trigger_update_of_and_drop_parse() {
        match parse_ddl(
            "CREATE TRIGGER t BEFORE UPDATE OF a, b ON orders BEGIN DELETE FROM log; END",
        )
        .unwrap()
        .unwrap()
        {
            DdlStmt::CreateTrigger(s) => {
                assert_eq!(s.timing, TriggerTiming::Before);
                assert_eq!(s.event, TriggerEvent::Update { of: vec!["a".into(), "b".into()] });
            }
            other => panic!("expected CreateTrigger, got {other:?}"),
        }
        assert_eq!(
            parse_ddl("DROP TRIGGER audit_ins").unwrap().unwrap(),
            DdlStmt::DropTrigger { name: "audit_ins".into(), if_exists: false }
        );
        assert_eq!(
            parse_ddl("DROP TRIGGER IF EXISTS audit_ins").unwrap().unwrap(),
            DdlStmt::DropTrigger { name: "audit_ins".into(), if_exists: true }
        );
    }

    #[test]
    fn trigger_named_refusals_parse_errors() {
        assert!(parse_ddl("CREATE TRIGGER t INSTEAD OF INSERT ON v BEGIN DELETE FROM x; END").is_err());
        assert!(parse_ddl(
            "CREATE TRIGGER t AFTER INSERT ON o FOR EACH STATEMENT BEGIN DELETE FROM x; END"
        )
        .is_err());
        assert!(parse_ddl("CREATE TRIGGER t AFTER INSERT ON o EXECUTE PROCEDURE p(NEW.id)").is_err());
        assert!(parse_ddl("CREATE TRIGGER t AFTER INSERT ON o BEGIN INSERT INTO x VALUES (1)").is_err()); // no END
    }

    #[test]
    fn analyze_and_reindex_parse() {
        // Bare and named forms; trailing `;` tolerated by parse_ddl.
        assert_eq!(parse_ddl("ANALYZE").unwrap().unwrap(), DdlStmt::Analyze { name: None });
        assert_eq!(
            parse_ddl("ANALYZE orders").unwrap().unwrap(),
            DdlStmt::Analyze { name: Some("orders".into()) }
        );
        assert_eq!(parse_ddl("analyze;").unwrap().unwrap(), DdlStmt::Analyze { name: None });
        assert_eq!(parse_ddl("REINDEX").unwrap().unwrap(), DdlStmt::Reindex { target: None });
        assert_eq!(
            parse_ddl("REINDEX orders").unwrap().unwrap(),
            DdlStmt::Reindex { target: Some("orders".into()) }
        );
        // A column named `analyze`/`reindex` still parses as ordinary SQL — the
        // DDL words are positional identifiers, not reserved keywords.
        assert_eq!(parse_ddl("SELECT analyze FROM t").unwrap(), None);
        assert_eq!(parse_ddl("INSERT INTO t (reindex) VALUES (1)").unwrap(), None);
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

    /// sqlite's declared-type vocabulary: any word(s), with an optional size.
    /// The expectations here are pinned against the real `sqlite3` binary in
    /// `crates/mpedb/tests/django_parse_gaps.rs`.
    #[test]
    fn create_table_sqlite_declared_types() {
        let s = create_table(
            "CREATE TABLE t (id integer NOT NULL PRIMARY KEY, name varchar(100) NOT NULL, \
             code char(1) NULL, big bigint, small smallint, pos integer unsigned, \
             amount double precision, price decimal(10, 2), made datetime, day date, \
             data BLOB, huge \"unsigned big int\", weird nosuchtype, bare)",
        );
        use ColumnType::*;
        let got: Vec<ColumnType> = s.columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            got,
            vec![
                Int64,   // integer
                Text,    // varchar(100)
                Text,    // char(1)
                Int64,   // bigint
                Int64,   // smallint
                Int64,   // integer unsigned
                Float64, // double precision
                Any,     // decimal(10, 2)  → NUMERIC affinity
                Any,     // datetime        → NUMERIC affinity
                Any,     // date            → NUMERIC affinity
                Blob,    // BLOB
                Int64,   // "unsigned big int" (quoted words are type words)
                Any,     // an unknown name is legal in sqlite and means NUMERIC
                Any,     // no declared type at all
            ]
        );
        // The size is consumed and dropped — mpedb has no width-limited types,
        // so honouring `varchar(1)` as a limit would reject rows sqlite stores.
        assert_eq!(create_table("CREATE TABLE t (a varchar(1))").columns[0].ty, Text);
        // A malformed size is still a parse error.
        assert!(parse_ddl("CREATE TABLE t (a varchar(100)").is_err());
        assert!(parse_ddl("CREATE TABLE t (a varchar(x))").is_err());
        // `ADD COLUMN` uses the identical grammar.
        match parse_ddl("ALTER TABLE t ADD COLUMN c varchar(100)").unwrap().unwrap() {
            DdlStmt::AlterAddColumn { column, .. } => assert_eq!(column.ty, ColumnType::Text),
            other => panic!("{other:?}"),
        }
        match parse_ddl("ALTER TABLE t ADD COLUMN c double precision DEFAULT 1.5")
            .unwrap()
            .unwrap()
        {
            DdlStmt::AlterAddColumn { column, .. } => {
                assert_eq!(column.ty, ColumnType::Float64);
                assert_eq!(column.default, Some(DefaultExpr::Const(mpedb_types::Value::Float(1.5))));
            }
            other => panic!("{other:?}"),
        }
        // A constraint word is never eaten as a type word — the column below is
        // typeless with a PRIMARY KEY, not a column of type `primary`.
        let s = create_table("CREATE TABLE t (a PRIMARY KEY, b UNIQUE, c COLLATE NOCASE)");
        assert!(s.columns.iter().all(|c| c.ty == ColumnType::Any));
        assert!(s.columns[0].pk && s.columns[1].unique);
    }

    /// `AUTOINCREMENT` refuses BY NAME, in every position it can be written,
    /// and the message says what cannot be promised. `PRIMARY KEY ASC|DESC` —
    /// the same production — is accepted and its direction dropped, as in
    /// sqlite.
    #[test]
    fn autoincrement_refuses_by_name_everywhere() {
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY ASC AUTOINCREMENT, x INT)",
            "CREATE TABLE t (id integer AUTOINCREMENT PRIMARY KEY)",
            "ALTER TABLE t ADD COLUMN id INTEGER PRIMARY KEY AUTOINCREMENT",
        ] {
            let e = parse_ddl(sql).unwrap_err().to_string();
            assert!(e.contains("AUTOINCREMENT"), "{sql}: {e}");
            assert!(e.contains("reused"), "the refusal must say WHY — {sql}: {e}");
        }
        // Without the keyword the same column definitions are fine, direction
        // and all.
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY ASC)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY DESC, x INT)",
        ] {
            let s = create_table(sql);
            assert!(s.columns[0].pk, "{sql}");
            assert_eq!(s.columns[0].ty, ColumnType::Int64, "{sql}");
        }
    }

    #[test]
    fn create_table_malformed_and_unsupported_refuse() {
        assert!(parse_ddl("CREATE TABLE t (id INT PRIMARY KEY,)").is_err()); // trailing comma → empty col
        assert!(parse_ddl("CREATE TABLE t (id INT DEFAULT 0)").is_err()); // DEFAULT unsupported
        assert!(parse_ddl("CREATE TABLE t (id INT CHECK (id > 0))").is_err()); // CHECK unsupported
        assert!(parse_ddl("CREATE TABLE t (id INT REFERENCES o(id))").is_err()); // FK unsupported
        assert!(parse_ddl("CREATE TABLE t id INT)").is_err()); // missing (
        assert!(parse_ddl("CREATE TABLE t (id INT").is_err()); // missing )
    }
}
