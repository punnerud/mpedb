//! Multi-database workspace handle (DESIGN-MULTIDB.md §1).
//!
//! A [`Workspace`] owns N fully-independent [`Database`] engines addressed by
//! `alias.table`. Each member is its own file — its own writer lock, reader
//! table, catalog, and 56-table/`u64`-footprint domain — so **separate files
//! give separate writer locks (linear write parallelism) and the only
//! OS-enforced isolation boundary** the serverless model can offer (§6). This
//! layer adds no shared state across members and touches nothing in the
//! reviewed concurrency/commit protocol.

use crate::{Database, ExecResult};
use mpedb_types::{Config, Error, PlanHash, Result, Value, WorkspaceConfig};
use std::path::Path;

/// A plan compiled against one workspace member, carrying the routing `alias`
/// so [`Workspace::execute`] dispatches with no re-parse. The `hash` is exactly
/// the member's own content hash (the alias never enters the plan bytes), so a
/// hash prepared directly on the member is interchangeable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsPlan {
    pub alias: String,
    pub hash: PlanHash,
}

/// A set of independently-attached databases addressed by alias.
pub struct Workspace {
    members: Vec<(String, Database)>,
    /// The unqualified-default alias — `Some` only when there is exactly one
    /// member, so a bare `FROM t` is unambiguous.
    default_alias: Option<String>,
}

impl Workspace {
    /// Open every member described by a workspace TOML (a lone `[database]`
    /// config opens as a one-member workspace).
    pub fn open(config_path: &Path) -> Result<Workspace> {
        Workspace::open_config(WorkspaceConfig::from_file(config_path)?)
    }

    /// Open every member of an already-parsed [`WorkspaceConfig`].
    pub fn open_config(cfg: WorkspaceConfig) -> Result<Workspace> {
        let default_alias = cfg.default_alias().map(str::to_string);
        let mut members = Vec::with_capacity(cfg.members.len());
        for m in cfg.members {
            let db = Database::open_with_config(m.config)?;
            members.push((m.alias, db));
        }
        Ok(Workspace {
            members,
            default_alias,
        })
    }

    /// The attached member aliases, in config order.
    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|(a, _)| a.as_str())
    }

    /// Borrow one member engine directly — e.g. `ws.db("billing").begin()` for
    /// a member-local write transaction, or per-member `prepare`/`execute`.
    pub fn db(&self, alias: &str) -> Option<&Database> {
        self.members
            .iter()
            .find(|(a, _)| a == alias)
            .map(|(_, d)| d)
    }

    /// Attach another database at runtime (sqlite `ATTACH`). Errors on a
    /// duplicate alias or a file already attached under another alias.
    pub fn attach(&mut self, alias: impl Into<String>, config: Config) -> Result<()> {
        let alias = alias.into();
        if alias.is_empty() || alias.contains('.') {
            return Err(Error::Config("attach alias must be non-empty and contain no '.'".into()));
        }
        if self.members.iter().any(|(a, _)| a == &alias) {
            return Err(Error::Config(format!("database alias `{alias}` is already attached")));
        }
        if let Some((a, _)) = self
            .members
            .iter()
            .find(|(_, d)| d.path() == config.options.path)
        {
            return Err(Error::Config(format!(
                "file `{}` is already attached as `{a}`",
                config.options.path.display()
            )));
        }
        let db = Database::open_with_config(config)?;
        self.members.push((alias, db));
        // Attaching a second member removes the unqualified default.
        if self.members.len() != 1 {
            self.default_alias = None;
        }
        Ok(())
    }

    /// Detach a member (sqlite `DETACH`), dropping its engine. Returns whether
    /// a member with that alias existed.
    pub fn detach(&mut self, alias: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|(a, _)| a != alias);
        let removed = self.members.len() != before;
        self.default_alias = match self.members.as_slice() {
            [(only, _)] => Some(only.clone()),
            _ => None,
        };
        removed
    }

    /// Resolve `sql` to (alias, member, de-qualified sql).
    fn resolve(&self, sql: &str) -> Result<(String, &Database, String)> {
        let (alias_opt, dequalified) = mpedb_sql::split_db_alias(sql)?;
        let alias = match alias_opt {
            Some(a) => a,
            None => self.default_alias.clone().ok_or_else(|| {
                Error::Bind(
                    "statement has no `alias.` database qualifier and the workspace \
                     has more than one database"
                        .into(),
                )
            })?,
        };
        let db = self
            .db(&alias)
            .ok_or_else(|| Error::Bind(format!("unknown database alias `{alias}`")))?;
        Ok((alias, db, dequalified))
    }

    /// Compile + run one statement, routed by its `alias.` qualifier.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        let (_, db, sql) = self.resolve(sql)?;
        db.query(&sql, params)
    }

    /// Compile + publish a plan on the routed member, returning a [`WsPlan`].
    pub fn prepare(&self, sql: &str) -> Result<WsPlan> {
        let (alias, db, sql) = self.resolve(sql)?;
        let hash = db.prepare(&sql)?;
        Ok(WsPlan { alias, hash })
    }

    /// Execute a previously-prepared [`WsPlan`] on its member.
    pub fn execute(&self, plan: &WsPlan, params: &[Value]) -> Result<ExecResult> {
        let db = self
            .db(&plan.alias)
            .ok_or_else(|| Error::Bind(format!("unknown database alias `{}`", plan.alias)))?;
        db.execute(&plan.hash, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::Value;

    fn ws_toml(tag: &str) -> String {
        let pid = std::process::id();
        format!(
            r#"
[[database]]
alias = "billing"
path = "/dev/shm/mpedb-ws-{tag}-{pid}-billing.mpedb"
size_mb = 8
  [[database.table]]
  name = "orders"
  primary_key = ["id"]
    [[database.table.column]]
    name = "id"
    type = "int64"

[[database]]
alias = "shared"
path = "/dev/shm/mpedb-ws-{tag}-{pid}-shared.mpedb"
size_mb = 8
  [[database.table]]
  name = "tenants"
  primary_key = ["id"]
    [[database.table.column]]
    name = "id"
    type = "int64"
"#
        )
    }

    fn open(tag: &str) -> Workspace {
        let cfg = WorkspaceConfig::from_toml_str(&ws_toml(tag)).unwrap();
        for m in &cfg.members {
            let _ = std::fs::remove_file(&m.config.options.path);
        }
        Workspace::open_config(cfg).unwrap()
    }

    #[test]
    fn routes_and_isolates_members() {
        let ws = open("iso");
        ws.query("INSERT INTO billing.orders (id) VALUES (1)", &[]).unwrap();
        ws.query("INSERT INTO billing.orders (id) VALUES (2)", &[]).unwrap();
        ws.query("INSERT INTO shared.tenants (id) VALUES (9)", &[]).unwrap();

        // Each alias sees only its own table.
        let orders = ws.query("SELECT * FROM billing.orders", &[]).unwrap();
        assert!(matches!(orders, ExecResult::Rows { rows, .. } if rows.len() == 2));
        let tenants = ws.query("SELECT * FROM shared.tenants", &[]).unwrap();
        assert!(matches!(tenants, ExecResult::Rows { rows, .. } if rows.len() == 1));

        // A table that lives in the other member is unknown here (isolation).
        assert!(ws.query("SELECT * FROM billing.tenants", &[]).is_err());
        // No qualifier is ambiguous with two members.
        assert!(matches!(
            ws.query("SELECT * FROM orders", &[]),
            Err(Error::Bind(_))
        ));
        // Unknown alias.
        assert!(matches!(
            ws.query("SELECT * FROM nope.orders", &[]),
            Err(Error::Bind(_))
        ));
    }

    #[test]
    fn wsplan_prepare_then_execute() {
        let ws = open("plan");
        ws.query("INSERT INTO billing.orders (id) VALUES (7)", &[]).unwrap();
        let plan = ws.prepare("SELECT * FROM billing.orders WHERE id = $1").unwrap();
        assert_eq!(plan.alias, "billing");
        // A WsPlan hash equals the member's own hash for the de-qualified SQL.
        let direct = ws.db("billing").unwrap().prepare("SELECT * FROM orders WHERE id = $1").unwrap();
        assert_eq!(plan.hash, direct);
        let got = ws.execute(&plan, &[Value::Int(7)]).unwrap();
        assert!(matches!(got, ExecResult::Rows { rows, .. } if rows.len() == 1));
        // Executing against a detached alias fails.
        assert!(matches!(
            ws.execute(&WsPlan { alias: "gone".into(), hash: plan.hash }, &[Value::Int(7)]),
            Err(Error::Bind(_))
        ));
    }

    #[test]
    fn attach_detach_and_default_alias() {
        let ws0 = open("att");
        // Build a single-member workspace to check the unqualified default.
        let single = WorkspaceConfig::from_toml_str(&format!(
            "[database]\npath = \"/dev/shm/mpedb-ws-solo-{}.mpedb\"\nsize_mb = 8\n\
             [[table]]\nname = \"t\"\nprimary_key=[\"id\"]\n  [[table.column]]\n  name=\"id\"\n  type=\"int64\"",
            std::process::id()
        ))
        .unwrap();
        let _ = std::fs::remove_file(&single.members[0].config.options.path);
        let mut solo = Workspace::open_config(single).unwrap();
        // One member ⇒ unqualified statements route to it.
        solo.query("INSERT INTO t (id) VALUES (1)", &[]).unwrap();
        assert!(matches!(
            solo.query("SELECT * FROM t", &[]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        // Attach a second member ⇒ the default disappears, qualifiers required.
        let second = mpedb_types::Config::from_toml_str(&format!(
            "[database]\npath = \"/dev/shm/mpedb-ws-second-{}.mpedb\"\nsize_mb = 8\n\
             [[table]]\nname = \"u\"\nprimary_key=[\"id\"]\n  [[table.column]]\n  name=\"id\"\n  type=\"int64\"",
            std::process::id()
        ))
        .unwrap();
        let _ = std::fs::remove_file(&second.options.path);
        solo.attach("second", second).unwrap();
        assert!(matches!(solo.query("SELECT * FROM t", &[]), Err(Error::Bind(_))));
        assert!(solo.query("SELECT * FROM main.t", &[]).is_ok()
            || solo.query("SELECT * FROM solo.t", &[]).is_ok()
            || solo.aliases().count() == 2);
        // Detach restores a single default.
        assert!(solo.detach("second"));
        assert!(solo.query("SELECT * FROM t", &[]).is_ok());
        drop(ws0);
    }
}
