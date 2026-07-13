//! Row-level-security DDL statements (DESIGN-MULTIDB.md §3.1). These do not
//! compile to a [`CompiledPlan`](crate::CompiledPlan) — they mutate the catalog
//! sys-keyspace — so the facade intercepts them before planning and applies
//! them via its policy-store API. `USING`/`WITH CHECK` predicates are captured
//! as SOURCE text (re-bound by the planner per statement, §3.2).

use mpedb_types::PolicyCmd;

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
    fn malformed_ddl_errors() {
        assert!(parse_ddl("CREATE POLICY p ON orders").is_err()); // no USING/CHECK
        assert!(parse_ddl("CREATE POLICY p ON orders USING (").is_err()); // unbalanced
        assert!(parse_ddl("ALTER TABLE orders ENABLE").is_err()); // missing ROW LEVEL SECURITY
        assert!(parse_ddl("CREATE POLICY p ON orders FOR BOGUS USING (id = 1)").is_err());
    }
}
