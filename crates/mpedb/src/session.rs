//! Session context — the serverless "principal" (DESIGN-MULTIDB.md §2).
//!
//! A [`Session`] is a bag of caller-set variables read by `current_setting()`
//! in SQL. It is the serverless analogue of a SQL session's `SET` variables and
//! the input to row-level-security policies (Phase 4).
//!
//! ## Trust (read this)
//!
//! The context is **asserted by the caller and authenticated against nothing.**
//! mpedb cannot tell a server-verified identity from attacker-controlled input,
//! so a value here MUST be derived from a server-side-verified session, never
//! from raw client input (§6.2). Setting the wrong `app.tenant` reads *and
//! writes* another tenant's rows with no hostility. See also the pooling-bleed
//! footgun (§2.5): prefer a fresh `Session` (or [`Session::reset`]) per
//! principal over a reused long-lived bag.

use mpedb_sql::CompiledPlan;
use mpedb_types::{Error, Result, Value};
use std::collections::BTreeMap;

/// Caller-asserted session context: `current_setting('key')` resolves against it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    ctx: BTreeMap<String, Value>,
}

impl Session {
    /// An empty context — the default for [`Database::execute`](crate::Database::execute)
    /// and friends. A plan that references `current_setting()` fails closed
    /// against an empty session (missing key is a hard error).
    pub fn empty() -> Session {
        Session::default()
    }

    /// Set a context variable (mirrors SQL `SET app.tenant = …`). Chainable.
    pub fn set(&mut self, key: impl Into<String>, value: Value) -> &mut Session {
        self.ctx.insert(key.into(), value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.ctx.get(key)
    }

    /// Remove one key, returning its previous value.
    pub fn unset(&mut self, key: &str) -> Option<Value> {
        self.ctx.remove(key)
    }

    /// Clear all context — call between principals on a reused `Session` to
    /// avoid the pooling-bleed footgun (§2.5).
    pub fn reset(&mut self) {
        self.ctx.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.ctx.is_empty()
    }
}

/// Build the executor's full parameter array: the caller's params followed by
/// the plan's reserved session-context slots, resolved from `session` by key.
///
/// Fail-closed (DESIGN-MULTIDB.md §2.3/§2.4): a missing key, a NULL value, or a
/// type mismatch is a hard error — never a silently-empty result. Only the
/// `n_user` caller params are accepted; a caller cannot supply a value for the
/// reserved tail (that is where §2.1's "refuse caller-supplied context" is
/// enforced), because any excess is reported as a wrong parameter count.
pub(crate) fn resolve_params(
    plan: &CompiledPlan,
    user_params: &[Value],
    session: &Session,
) -> Result<Vec<Value>> {
    let total = plan.n_params as usize;
    let n_ctx = plan.context_keys.len();
    let n_user = total - n_ctx;
    if user_params.len() != n_user {
        return Err(Error::WrongParamCount {
            expected: n_user,
            got: user_params.len(),
        });
    }
    if n_ctx == 0 {
        return Ok(user_params.to_vec());
    }
    let mut full = Vec::with_capacity(total);
    full.extend_from_slice(user_params);
    for (p, key) in plan.context_keys.iter().enumerate() {
        let value = session.get(key).ok_or_else(|| {
            Error::Bind(format!(
                "session context '{key}' is required by the statement but is not set"
            ))
        })?;
        if value.is_null() {
            return Err(Error::TypeMismatch(format!(
                "session context '{key}' is NULL; a concrete value is required"
            )));
        }
        if let Some(t) = plan.param_types[n_user + p] {
            if !value.fits(t) {
                return Err(Error::TypeMismatch(format!(
                    "session context '{key}' is {}, statement requires {t}",
                    value.type_name()
                )));
            }
        }
        full.push(value.clone());
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::Session;
    use crate::{Database, ExecResult};
    use mpedb_types::{Config, Value};

    fn db(tag: &str) -> Database {
        let path = format!("/dev/shm/mpedb-sess-{tag}-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"note\"\n  type = \"text\"\n  nullable = true"
        ))
        .unwrap();
        Database::open_with_config(cfg).unwrap()
    }

    fn sess(tenant: i64) -> Session {
        let mut s = Session::empty();
        s.set("app.tenant", Value::Int(tenant));
        s
    }

    fn rows(r: ExecResult) -> Vec<Vec<Value>> {
        match r {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn current_setting_filters_by_session() {
        let db = db("filter");
        for (id, t) in [(1, 1), (2, 1), (3, 2)] {
            db.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
        let sql = "SELECT id FROM orders WHERE tenant = current_setting('app.tenant')";
        // tenant 1 sees 2 rows, tenant 2 sees 1 — one plan, different sessions.
        assert_eq!(rows(db.query_ctx(&sess(1), sql, &[]).unwrap()).len(), 2);
        assert_eq!(rows(db.query_ctx(&sess(2), sql, &[]).unwrap()).len(), 1);
        // The plan hash is identical regardless of the session's values.
        let h1 = db.prepare(sql).unwrap();
        let h2 = db.prepare(sql).unwrap();
        assert_eq!(h1, h2);
        // execute_ctx by hash routes context the same way.
        assert_eq!(rows(db.execute_ctx(&sess(2), &h1, &[]).unwrap()).len(), 1);
    }

    #[test]
    fn context_mixes_with_user_params() {
        let db = db("mix");
        for (id, t) in [(1, 7), (2, 7), (3, 7)] {
            db.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
        // $1 is a user param, current_setting is a reserved slot after it.
        let sql = "SELECT id FROM orders WHERE tenant = current_setting('app.tenant') AND id >= $1";
        let got = rows(db.query_ctx(&sess(7), sql, &[Value::Int(2)]).unwrap());
        assert_eq!(got.len(), 2); // ids 2,3
        // Wrong user-param count (context slot is NOT caller-suppliable).
        assert!(matches!(
            db.query_ctx(&sess(7), sql, &[Value::Int(2), Value::Int(7)]),
            Err(mpedb_types::Error::WrongParamCount { expected: 1, got: 2 })
        ));
    }

    #[test]
    fn fail_closed_missing_null_and_wrong_type() {
        let db = db("failclosed");
        db.query("INSERT INTO orders (id, tenant, note) VALUES (1, 1, NULL)", &[]).unwrap();
        let sql = "SELECT id FROM orders WHERE tenant = current_setting('app.tenant')";
        // missing key
        assert!(matches!(
            db.query_ctx(&Session::empty(), sql, &[]),
            Err(mpedb_types::Error::Bind(_))
        ));
        // NULL value
        let mut s = Session::empty();
        s.set("app.tenant", Value::Null);
        assert!(matches!(
            db.query_ctx(&s, sql, &[]),
            Err(mpedb_types::Error::TypeMismatch(_))
        ));
        // wrong type (text where int required)
        let mut s = Session::empty();
        s.set("app.tenant", Value::Text("x".into()));
        assert!(matches!(
            db.query_ctx(&s, sql, &[]),
            Err(mpedb_types::Error::TypeMismatch(_))
        ));
    }

    #[test]
    fn untyped_context_ref_is_rejected_at_prepare() {
        let db = db("untyped");
        // current_setting in a projection with nothing to infer a type from.
        assert!(matches!(
            db.prepare("SELECT current_setting('x') FROM orders"),
            Err(mpedb_types::Error::Bind(_))
        ));
    }

    #[test]
    fn begin_as_snapshots_context_for_writes() {
        let db = db("beginas");
        db.query("INSERT INTO orders (id, tenant, note) VALUES (1, 5, NULL)", &[]).unwrap();
        db.query("INSERT INTO orders (id, tenant, note) VALUES (2, 6, NULL)", &[]).unwrap();
        let mut s = Session::empty();
        s.set("app.tenant", Value::Int(5));
        let mut w = db.begin_as(&s).unwrap();
        // Mutating the caller's Session after begin_as must NOT bleed in.
        s.set("app.tenant", Value::Int(6));
        let affected = w
            .query(
                "UPDATE orders SET note = 'seen' WHERE tenant = current_setting('app.tenant')",
                &[],
            )
            .unwrap();
        assert_eq!(affected, ExecResult::Affected(1)); // only tenant 5 (snapshot)
        w.commit().unwrap();
        db.verify().unwrap();
    }
}
