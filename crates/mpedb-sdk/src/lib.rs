//! mpedb-sdk — a thin, dependency-light caching client session for mpedb.
//!
//! The pitch: *the user sends just SQL; the SDK optimizes against mpedb.*
//! Instead of every process pushing its compiled plans into the database's
//! shared registry (design/DESIGN.md §7.2), a [`Session`] holds them **client-side**
//! as [`DetachedPlan`]s (Morten's detached-plan model): it compiles each
//! distinct SQL string exactly once via [`Database::prepare_detached`], caches
//! the plan locally, and thereafter executes by
//! [`Database::execute_detached`] — no SQL re-parsing, and no registry write
//! per statement.
//!
//! Two properties fall out for free:
//!
//! - **No shared-memory bloat from plans.** A read-mostly fleet of 1000
//!   processes each running the same handful of statements keeps its plans in
//!   its own address space, not in the file.
//! - **Self-healing across schema changes.** If the schema changed under a
//!   long-lived session, [`Database::execute_detached`] returns
//!   [`Error::PlanInvalidated`]; [`Session::run`] transparently re-prepares
//!   from the cached SQL and retries **once**, so callers never see the stale
//!   plan.
//!
//! ```no_run
//! use mpedb::{params, Config, Database};
//! use mpedb_sdk::Session;
//!
//! let db = Database::open_with_config(Config::from_toml_str("...").unwrap()).unwrap();
//! let sess = Session::new(&db);
//! // First call compiles + caches; every later call reuses the cached plan.
//! for id in 0..1000 {
//!     let _ = sess.run("SELECT * FROM users WHERE id = $1", &params![id]).unwrap();
//! }
//! assert_eq!(sess.cached_plans(), 1);
//! ```
//!
//! The crate depends on `mpedb` alone.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use mpedb::{Database, DetachedPlan, Error, ExecResult, Result, Value};

const POISON: &str = "mpedb-sdk session cache lock poisoned";

/// A caching client session over one [`Database`].
///
/// Holds a local map from SQL text to its compiled [`DetachedPlan`]. Cheap to
/// create and `Send + Sync` — share one behind an `&`/`Arc` across threads;
/// the cache is guarded by an `RwLock`, so concurrent `run`s of already-cached
/// statements proceed without contention.
pub struct Session<'db> {
    db: &'db Database,
    plans: RwLock<HashMap<String, Arc<DetachedPlan>>>,
}

impl<'db> Session<'db> {
    /// Create a session over `db`. Compiles nothing yet.
    pub fn new(db: &'db Database) -> Session<'db> {
        Session {
            db,
            plans: RwLock::new(HashMap::new()),
        }
    }

    /// Run `sql` with `params`.
    ///
    /// The first time a given SQL string is seen this compiles it once
    /// ([`Database::prepare_detached`]) and caches the plan locally; every
    /// later call for the same text skips parsing entirely and goes straight
    /// to [`Database::execute_detached`]. On [`Error::PlanInvalidated`] (the
    /// schema changed under us) the cached plan is transparently re-prepared
    /// from its carried SQL and the statement is retried **once**; a second
    /// invalidation surfaces to the caller (it means the schema is changing
    /// faster than we can adapt, or the re-prepare itself failed).
    pub fn run(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        let plan = self.plan_for(sql)?;
        match self.db.execute_detached(&plan, params) {
            Err(Error::PlanInvalidated) => {
                // Stale across a schema change: recompile from SQL and retry
                // once. `prepare_detached` uses the *current* schema, so the
                // fresh plan cannot be invalidated for the same reason.
                let fresh = self.reprepare(sql)?;
                self.db.execute_detached(&fresh, params)
            }
            other => other,
        }
    }

    /// How many distinct SQL statements are currently cached (i.e. compiled
    /// exactly once each).
    pub fn cached_plans(&self) -> usize {
        self.plans.read().expect(POISON).len()
    }

    /// The cached plan for `sql`, compiling and caching it on first use.
    fn plan_for(&self, sql: &str) -> Result<Arc<DetachedPlan>> {
        if let Some(p) = self.plans.read().expect(POISON).get(sql) {
            return Ok(p.clone());
        }
        // Miss: compile (no lock held). A racing thread that compiled the same
        // SQL first is fine — the plan is content-identical, so `or_insert`
        // keeps whichever landed and both callers proceed with an equal plan.
        let plan = Arc::new(self.db.prepare_detached(sql)?);
        Ok(self
            .plans
            .write()
            .expect(POISON)
            .entry(sql.to_owned())
            .or_insert(plan)
            .clone())
    }

    /// Force a recompile of `sql` and overwrite the cache entry.
    fn reprepare(&self, sql: &str) -> Result<Arc<DetachedPlan>> {
        let plan = Arc::new(self.db.prepare_detached(sql)?);
        self.plans
            .write()
            .expect(POISON)
            .insert(sql.to_owned(), plan.clone());
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb::{params, Config, PlanHash};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQ: AtomicU64 = AtomicU64::new(0);

    fn shm_dir() -> PathBuf {
        if Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        }
    }

    fn db_path(name: &str) -> PathBuf {
        shm_dir().join(format!(
            "mpedb-sdk-{name}-{}-{}.mpedb",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// A config with `users(id int64 pk, email text unique not null)` plus an
    /// optional extra nullable column, so two variants share the `SELECT/INSERT
    /// by id` SQL but hash to different schemas.
    fn config(path: &Path, extra_col: bool) -> Config {
        let extra = if extra_col {
            "\n  [[table.column]]\n  name = \"note\"\n  type = \"text\"\n"
        } else {
            ""
        };
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = 16
max_readers = 32

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true
{extra}"#,
            path.display()
        );
        Config::from_toml_str(&toml).unwrap()
    }

    struct FileGuard(PathBuf);
    impl Drop for FileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn same_sql_compiles_once_over_1000_runs() {
        let path = db_path("once");
        let _ = std::fs::remove_file(&path);
        let _g = FileGuard(path.clone());
        let db = Database::open_with_config(config(&path, false)).unwrap();
        let sess = Session::new(&db);

        // Seed one row through the session's own write path.
        assert_eq!(
            sess.run("INSERT INTO users (id, email) VALUES (1, 'a@x')", &params![])
                .unwrap(),
            ExecResult::Affected(1)
        );
        // 1000 identical point reads — compiled exactly once.
        for _ in 0..1000 {
            match sess
                .run("SELECT id, email FROM users WHERE id = $1", &params![1])
                .unwrap()
            {
                ExecResult::Rows { rows, .. } => {
                    assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("a@x".into())]]);
                }
                other => panic!("{other:?}"),
            }
        }
        // INSERT + SELECT = two distinct statements, each cached once.
        assert_eq!(sess.cached_plans(), 2);
        db.verify().unwrap();
    }

    #[test]
    fn mixed_sql_caches_each_distinct_statement() {
        let path = db_path("mixed");
        let _ = std::fs::remove_file(&path);
        let _g = FileGuard(path.clone());
        let db = Database::open_with_config(config(&path, false)).unwrap();
        let sess = Session::new(&db);

        for i in 1..=20 {
            sess.run(
                "INSERT INTO users (id, email) VALUES ($1, $2)",
                &params![i, format!("u{i}@x")],
            )
            .unwrap();
        }
        // A few distinct SELECT shapes, each run several times.
        for _ in 0..5 {
            let n = match sess.run("SELECT id FROM users", &params![]).unwrap() {
                ExecResult::Rows { rows, .. } => rows.len(),
                other => panic!("{other:?}"),
            };
            assert_eq!(n, 20);
            assert!(matches!(
                sess.run("SELECT id FROM users WHERE id = $1", &params![7])
                    .unwrap(),
                ExecResult::Rows { rows, .. } if rows.len() == 1
            ));
            assert!(matches!(
                sess.run("SELECT id FROM users WHERE id > $1", &params![10])
                    .unwrap(),
                ExecResult::Rows { rows, .. } if rows.len() == 10
            ));
        }
        // exactly 4 distinct statements: 1 INSERT + 3 SELECT shapes.
        assert_eq!(sess.cached_plans(), 4);
        db.verify().unwrap();
    }

    #[test]
    fn schema_change_is_healed_by_auto_retry() {
        // Two databases on separate files: `db_a` has an extra column so a
        // plan it produces is invalid against `db_b`'s schema — the exact
        // situation of a session whose cached plan predates a schema migration.
        let path_a = db_path("schema-a");
        let path_b = db_path("schema-b");
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let _ga = FileGuard(path_a.clone());
        let _gb = FileGuard(path_b.clone());
        let db_a = Database::open_with_config(config(&path_a, true)).unwrap();
        let db_b = Database::open_with_config(config(&path_b, false)).unwrap();

        let sql = "SELECT id FROM users WHERE id = $1";
        let stale = db_a.prepare_detached(sql).unwrap();

        // A session over db_b, whose cache we pre-load with the stale plan
        // (compiled against db_a's schema) to simulate the schema having
        // changed under a live session.
        let sess = Session::new(&db_b);
        sess.plans
            .write()
            .unwrap()
            .insert(sql.to_owned(), Arc::new(stale.clone()));

        db_b.query("INSERT INTO users (id, email) VALUES (3, 'c@x')", &params![])
            .unwrap();

        // Sanity: the stale plan really is invalid against db_b.
        assert!(matches!(
            db_b.execute_detached(&stale, &params![3]),
            Err(Error::PlanInvalidated)
        ));

        // run() must transparently heal: re-prepare from the cached SQL, retry.
        assert!(matches!(
            sess.run(sql, &params![3]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        // The cache now holds the healed plan (valid for db_b, different hash).
        let healed: PlanHash = sess.plans.read().unwrap().get(sql).unwrap().hash;
        assert_ne!(healed, stale.hash);
        // ...and a subsequent run needs no retry.
        assert!(matches!(
            sess.run(sql, &params![3]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        db_b.verify().unwrap();
    }
}
