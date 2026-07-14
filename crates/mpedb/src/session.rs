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

    /// Bind a **membership set** for `col IN (current_setting(key))`
    /// (DESIGN-MULTIDB.md §2.6) — e.g. the orgs this principal belongs to.
    ///
    /// The arity lives here, in the data: one compiled plan serves a caller in
    /// one org and a caller in fifty, because the list never enters the plan
    /// bytes or its hash (§4.1).
    ///
    /// An EMPTY set is legal and means "belongs to nothing": `IN ()` is FALSE,
    /// so every row is denied — cleanly, not as UNKNOWN. Nested lists are
    /// rejected: a membership set is flat by construction, and nothing
    /// downstream should have to reason about lists of lists.
    pub fn set_list(
        &mut self,
        key: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Result<&mut Session> {
        let items: Vec<Value> = values.into_iter().collect();
        if items.iter().any(|v| matches!(v, Value::List(_))) {
            return Err(Error::TypeMismatch(
                "a session context list must be flat: nested lists are not supported".into(),
            ));
        }
        self.ctx.insert(key.into(), Value::List(items));
        Ok(self)
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
    use mpedb_types::{Config, Error, Value};

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

    // ---- §2.6 `col IN (current_setting('k'))` end to end ----

    fn seed_orgs(db: &Database) {
        for (id, t) in [(1, 10), (2, 20), (3, 30), (4, 40)] {
            db.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
    }

    fn ids(r: ExecResult) -> Vec<i64> {
        match r {
            ExecResult::Rows { rows, .. } => rows
                .iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => panic!("expected int id"),
                })
                .collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn in_context_list_filters_by_membership() {
        let db = db("in-basic");
        seed_orgs(&db);
        let sql = "SELECT id FROM orders WHERE tenant IN (current_setting('app.orgs')) ORDER BY id";

        let mut s = Session::empty();
        s.set_list("app.orgs", [Value::Int(10), Value::Int(30)]).unwrap();
        assert_eq!(ids(db.query_ctx(&s, sql, &[]).unwrap()), vec![1, 3]);

        // a different membership set, SAME statement
        let mut s2 = Session::empty();
        s2.set_list("app.orgs", [Value::Int(40)]).unwrap();
        assert_eq!(ids(db.query_ctx(&s2, sql, &[]).unwrap()), vec![4]);

        // empty set = belongs to nothing = denies cleanly
        let mut s3 = Session::empty();
        s3.set_list("app.orgs", []).unwrap();
        assert!(ids(db.query_ctx(&s3, sql, &[]).unwrap()).is_empty());

        let _ = std::fs::remove_file(db.path());
    }

    /// THE design property (§4.1/§2.6): arity lives in the data, so one
    /// content-hashed plan serves a caller in one org and a caller in fifty.
    /// If the list ever leaked into the plan bytes this would mint a plan per
    /// distinct membership set — the exact explosion §2.6 exists to avoid.
    #[test]
    fn one_plan_serves_every_membership_set() {
        let db = db("in-oneplan");
        seed_orgs(&db);
        let sql = "SELECT id FROM orders WHERE tenant IN (current_setting('app.orgs'))";
        let h1 = db.prepare(sql).unwrap();
        let h2 = db.prepare(sql).unwrap();
        assert_eq!(h1, h2);

        let mut small = Session::empty();
        small.set_list("app.orgs", [Value::Int(10)]).unwrap();
        let mut big = Session::empty();
        big.set_list("app.orgs", (0..50).map(Value::Int)).unwrap();

        // both execute the SAME prepared hash
        assert_eq!(ids(db.execute_ctx(&small, &h1, &[]).unwrap()), vec![1]);
        assert_eq!(ids(db.execute_ctx(&big, &h1, &[]).unwrap()), vec![1, 2, 3, 4]);
        let _ = std::fs::remove_file(db.path());
    }

    /// 3VL reaches the query layer: a NULL in the set means "maybe", and a
    /// filter needs exactly TRUE, so non-matching rows stay hidden rather than
    /// being reported as definitely-absent.
    #[test]
    fn in_context_null_element_denies_non_matching_rows() {
        let db = db("in-null");
        seed_orgs(&db);
        let sql = "SELECT id FROM orders WHERE tenant IN (current_setting('app.orgs')) ORDER BY id";
        let mut s = Session::empty();
        s.set_list("app.orgs", [Value::Int(10), Value::Null]).unwrap();
        // 10 matches outright; the rest are UNKNOWN (the NULL might have been
        // them) and UNKNOWN is not visible.
        assert_eq!(ids(db.query_ctx(&s, sql, &[]).unwrap()), vec![1]);
        let _ = std::fs::remove_file(db.path());
    }

    #[test]
    fn in_context_missing_key_is_a_hard_error() {
        let db = db("in-missing");
        seed_orgs(&db);
        let r = db.query_ctx(
            &Session::empty(),
            "SELECT id FROM orders WHERE tenant IN (current_setting('app.orgs'))",
            &[],
        );
        assert!(matches!(r, Err(Error::Bind(_))), "got {r:?}");
        let _ = std::fs::remove_file(db.path());
    }

    /// A scalar where a list belongs is a hard error, not a silent deny — a
    /// silent deny would look exactly like "this principal owns nothing".
    #[test]
    fn in_context_with_a_scalar_value_errors() {
        let db = db("in-scalar");
        seed_orgs(&db);
        let mut s = Session::empty();
        s.set("app.orgs", Value::Int(10));
        let r = db.query_ctx(
            &s,
            "SELECT id FROM orders WHERE tenant IN (current_setting('app.orgs'))",
            &[],
        );
        assert!(matches!(r, Err(Error::TypeMismatch(_))), "got {r:?}");
        let _ = std::fs::remove_file(db.path());
    }

    /// One context slot cannot be both a scalar and a list: it would make the
    /// same key mean two things in one statement.
    #[test]
    fn a_key_used_as_both_scalar_and_list_is_rejected_at_prepare() {
        let db = db("in-mixed");
        let r = db.prepare(
            "SELECT id FROM orders WHERE tenant IN (current_setting('k')) AND id = current_setting('k')",
        );
        assert!(matches!(&r, Err(Error::Bind(m)) if m.contains("one or the other")), "got {r:?}");
        let _ = std::fs::remove_file(db.path());
    }

    /// Until task #21 lands general IN, the unsupported form must say so.
    #[test]
    fn literal_in_list_is_rejected_with_a_useful_message() {
        let db = db("in-literal");
        let r = db.prepare("SELECT id FROM orders WHERE tenant IN (1, 2)");
        assert!(
            matches!(&r, Err(Error::Parse { msg, .. }) if msg.contains("current_setting")),
            "got {r:?}"
        );
        let _ = std::fs::remove_file(db.path());
    }

    #[test]
    fn set_list_rejects_nesting() {
        let mut s = Session::empty();
        let r = s.set_list("k", [Value::List(vec![Value::Int(1)])]);
        assert!(matches!(r, Err(Error::TypeMismatch(_))), "got {r:?}");
    }
}
