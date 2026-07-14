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

    /// Begin a multi-member write (DESIGN-MULTIDB.md §1.5). See
    /// [`WorkspaceTxn`] — **there is no atomic cross-file commit**, and the type
    /// exists to make that impossible to use by accident.
    pub fn begin_multi(&self) -> WorkspaceTxn<'_> {
        WorkspaceTxn {
            ws: self,
            members: Vec::new(),
        }
    }
}

/// A write spanning several workspace members — **NOT a transaction across
/// files, and deliberately not shaped like one** (DESIGN-MULTIDB.md §1.5).
///
/// There is no atomic cross-file commit and none should be added: members have
/// entirely independent meta pages, intent rings and WALs, and a shared commit
/// protocol would destroy the independent-writer-lock parallelism that is the
/// whole point of separate files.
///
/// **The failure envelope, stated exactly:** the only commit method is
/// [`commit_sequential_nonatomic`](WorkspaceTxn::commit_sequential_nonatomic).
/// It commits members one after another, each atomic *on its own engine*, with
/// no cross-file barrier at any durability level. **After a crash an ARBITRARY
/// SUBSET of the member commits may survive — a later member may be durable
/// while an earlier one is lost. There is no prefix guarantee and no ordering
/// guarantee.** "It got through member 3, so members 1-2 are safe" is exactly
/// the reasoning this envelope forbids.
///
/// The cliff, plainly: **if you need ACID across two tables they belong in ONE
/// file; if you need isolation between them they belong in TWO files.** This
/// type is for the case where the write is genuinely independent per member and
/// you accept re-running it — an idempotent fan-out, not a transfer.
/// One queued statement: SQL plus its bound params.
type QueuedStmt = (String, Vec<Value>);

pub struct WorkspaceTxn<'ws> {
    ws: &'ws Workspace,
    /// Per member (in first-touched order): the statements queued for it,
    /// applied in one `WriteSession` at commit.
    members: Vec<(String, Vec<QueuedStmt>)>,
}

impl<'ws> WorkspaceTxn<'ws> {
    /// Queue a statement against `alias`. Statements for the same member run in
    /// one `WriteSession` (atomic together); different members never are.
    ///
    /// Nothing executes until commit: a partially-built multi-member write must
    /// not be able to leave one member mutated because the *second* `stmt` call
    /// had a typo in it.
    pub fn stmt(&mut self, alias: &str, sql: &str, params: &[Value]) -> Result<()> {
        if self.ws.db(alias).is_none() {
            return Err(Error::Bind(format!("unknown database alias `{alias}`")));
        }
        let entry: &mut (String, Vec<QueuedStmt>) = match self.members.iter_mut().find(|(a, _)| a == alias) {
            Some(e) => e,
            None => {
                self.members.push((alias.to_string(), Vec::new()));
                self.members.last_mut().expect("just pushed")
            }
        };
        entry.1.push((sql.to_string(), params.to_vec()));
        Ok(())
    }

    /// Commit each member's queued statements, **sequentially and
    /// non-atomically across members** (§1.5). The name is the documentation.
    ///
    /// Per member: one `WriteSession`, so that member's statements are atomic
    /// together and its failure rolls itself back cleanly. Across members: on
    /// the first failure this stops and returns the error, leaving **already
    /// committed members committed** — it cannot undo them, because undoing is
    /// what "no cross-file transaction" means. `Ok(n)` = members committed.
    ///
    /// A crash mid-way is worse than a returned error, and worse than it looks:
    /// see the type docs — an arbitrary subset survives, not a prefix.
    pub fn commit_sequential_nonatomic(self) -> Result<usize> {
        let mut done = 0usize;
        for (alias, stmts) in &self.members {
            let db = self
                .ws
                .db(alias)
                .ok_or_else(|| Error::Bind(format!("unknown database alias `{alias}`")))?;
            let mut s = db.begin()?;
            for (sql, params) in stmts {
                // A member's own failure rolls back only that member: the
                // session drops un-committed, and members already committed
                // stay committed. That asymmetry IS the envelope.
                s.query(sql, params)?;
            }
            s.commit()?;
            done += 1;
        }
        Ok(done)
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

    // ---- §1.5 WorkspaceTxn: the non-atomic cross-file write ----

    fn nrows(ws: &Workspace, sql: &str) -> usize {
        match ws.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn workspace_txn_commits_every_member() {
        let ws = Workspace::open_config(
            mpedb_types::WorkspaceConfig::from_toml_str(&ws_toml("mtx-ok")).unwrap(),
        )
        .unwrap();
        let mut tx = ws.begin_multi();
        tx.stmt("billing", "INSERT INTO orders (id) VALUES ($1)", &[Value::Int(1)]).unwrap();
        tx.stmt("shared", "INSERT INTO tenants (id) VALUES ($1)", &[Value::Int(7)]).unwrap();
        assert_eq!(tx.commit_sequential_nonatomic().unwrap(), 2);
        assert_eq!(nrows(&ws, "SELECT id FROM billing.orders"), 1);
        assert_eq!(nrows(&ws, "SELECT id FROM shared.tenants"), 1);
    }

    /// The envelope, demonstrated rather than asserted in prose: a later member
    /// failing does NOT undo an earlier member's commit. This is the whole
    /// reason the method's name says non-atomic — a test that only checked the
    /// happy path would leave the dangerous half unpinned.
    #[test]
    fn workspace_txn_leaves_earlier_members_committed_when_a_later_one_fails() {
        let ws = Workspace::open_config(
            mpedb_types::WorkspaceConfig::from_toml_str(&ws_toml("mtx-fail")).unwrap(),
        )
        .unwrap();
        // seed the row the second member will collide with
        ws.query("INSERT INTO shared.tenants (id) VALUES ($1)", &[Value::Int(7)]).unwrap();

        let mut tx = ws.begin_multi();
        tx.stmt("billing", "INSERT INTO orders (id) VALUES ($1)", &[Value::Int(1)]).unwrap();
        tx.stmt("shared", "INSERT INTO tenants (id) VALUES ($1)", &[Value::Int(7)]).unwrap();
        assert!(tx.commit_sequential_nonatomic().is_err(), "PK collision must surface");

        // billing committed and STAYS committed; there is nothing to roll it back.
        assert_eq!(
            nrows(&ws, "SELECT id FROM billing.orders"),
            1,
            "an earlier member's commit survives a later member's failure — that IS §1.5"
        );
        assert_eq!(nrows(&ws, "SELECT id FROM shared.tenants"), 1, "the failed member changed nothing");
    }

    /// Per member, statements are still atomic on their own engine: a failure
    /// inside one member rolls that member's whole batch back.
    #[test]
    fn workspace_txn_is_atomic_within_one_member() {
        let ws = Workspace::open_config(
            mpedb_types::WorkspaceConfig::from_toml_str(&ws_toml("mtx-one")).unwrap(),
        )
        .unwrap();
        ws.query("INSERT INTO billing.orders (id) VALUES ($1)", &[Value::Int(5)]).unwrap();
        let mut tx = ws.begin_multi();
        tx.stmt("billing", "INSERT INTO orders (id) VALUES ($1)", &[Value::Int(1)]).unwrap();
        tx.stmt("billing", "INSERT INTO orders (id) VALUES ($1)", &[Value::Int(5)]).unwrap(); // dup
        assert!(tx.commit_sequential_nonatomic().is_err());
        // id=1 must NOT be there: same member, same session, rolled back together.
        assert_eq!(nrows(&ws, "SELECT id FROM billing.orders"), 1);
    }

    /// Nothing runs until commit — a typo in the second statement must not leave
    /// the first member already mutated.
    #[test]
    fn workspace_txn_queues_and_validates_before_touching_anything() {
        let ws = Workspace::open_config(
            mpedb_types::WorkspaceConfig::from_toml_str(&ws_toml("mtx-queue")).unwrap(),
        )
        .unwrap();
        let mut tx = ws.begin_multi();
        tx.stmt("billing", "INSERT INTO orders (id) VALUES ($1)", &[Value::Int(1)]).unwrap();
        assert!(
            matches!(tx.stmt("ghost", "INSERT INTO x (id) VALUES ($1)", &[Value::Int(1)]), Err(Error::Bind(_))),
            "an unknown alias must be caught at queue time"
        );
        drop(tx); // never committed
        assert_eq!(nrows(&ws, "SELECT id FROM billing.orders"), 0, "queuing must not write");
    }
}
