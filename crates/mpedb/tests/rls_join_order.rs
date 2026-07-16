//! #46b: the RLS-over-join ORDERING contract, mutation-tested on the raise
//! path. Each table's policy runs over its OWN row BEFORE the join's ON (or
//! its residual) can raise on it — because mpedb expressions can raise
//! (division by zero) and a raise is observable, so the wrong order reports
//! the existence of rows the policy hides. These tests fail if anyone
//! reorders policy evaluation after ON — in EITHER execution form (held
//! full scan, or the #49 index nested loop) — or lets a LEFT join leak a
//! policy-hidden row's values instead of NULL-extending.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

/// emp(eid, x, did?) joined against dept(did, val) — dept row 200 has
/// `val = 0`, the divide-by-zero landmine a policy is about to hide.
fn db(tag: &str) -> Tmp {
    let path = format!("/dev/shm/mpedb-rlsjoin-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"emp\"\nprimary_key = [\"eid\"]\n  \
         [[table.column]]\n  name = \"eid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"did\"\n  type = \"int64\"\n  nullable = true\n\
         [[table]]\nname = \"dept\"\nprimary_key = [\"did\"]\n  \
         [[table.column]]\n  name = \"did\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"val\"\n  type = \"int64\""
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    let ins_e = db.prepare("INSERT INTO emp (eid, x, did) VALUES ($1, $2, $3)").unwrap();
    db.execute(&ins_e, &params![1i64, 10i64, 100i64]).unwrap();
    db.execute(&ins_e, &params![2i64, 10i64, 200i64]).unwrap();
    db.execute(&ins_e, &[Value::Int(3), Value::Int(10), Value::Null]).unwrap();
    let ins_d = db.prepare("INSERT INTO dept (did, val) VALUES ($1, $2)").unwrap();
    db.execute(&ins_d, &params![100i64, 10i64]).unwrap();
    db.execute(&ins_d, &params![200i64, 0i64]).unwrap(); // the landmine
    Tmp { db, path }
}

fn hide_zero_depts(d: &Database) {
    d.query("CREATE POLICY hide_zero ON dept FOR ALL USING (val > 0)", &[]).unwrap();
    d.query("ALTER TABLE dept ENABLE ROW LEVEL SECURITY", &[]).unwrap();
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// HELD PATH (no ON equality → the inner side is read once, policy as the
/// fetch filter): an ON that divides by the hidden row's zero must never see
/// it. Reorder policy after ON and this query raises division-by-zero —
/// reporting that a hidden row exists without returning it.
#[test]
fn held_path_policy_runs_before_on_can_raise() {
    let d = db("held");
    hide_zero_depts(&d);
    // No equality conjunct → no pushdown. Every visible dept has val=10, so
    // x/val = 1 pairs every employee with dept 100. dept 200 (val=0) is
    // policy-hidden and the division never happens.
    let got = rows(
        d.query(
            "SELECT emp.eid FROM emp JOIN dept ON emp.x / dept.val = 1 ORDER BY emp.eid",
            &[],
        )
        .expect("the hidden zero row must never reach the ON"),
    );
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]);
}

/// INDEX NESTED LOOP (#49): the equality is consumed into a per-outer-row
/// fetch, and the POLICY must filter the fetched row BEFORE the residual ON
/// runs. emp 2's fetch finds dept 200 by PK — policy hides it, so the
/// residual `x / val = 1` never divides by its zero and the row is simply
/// unmatched. Run the residual first and this raises.
#[test]
fn index_nested_loop_policy_runs_before_residual_on() {
    let d = db("inl");
    hide_zero_depts(&d);
    let got = rows(
        d.query(
            "SELECT emp.eid FROM emp JOIN dept ON dept.did = emp.did AND emp.x / dept.val = 1 \
             ORDER BY emp.eid",
            &[],
        )
        .expect("the policy must filter the fetched row before the residual ON"),
    );
    assert_eq!(got, vec![vec![Value::Int(1)]], "only emp 1's dept is visible AND passes");
}

/// LEFT JOIN semantics of a hidden row: policy-hidden = ABSENT. The outer row
/// SURVIVES, NULL-extended — it is neither dropped (that would be INNER over
/// a filter) nor allowed to carry the hidden row's values (that would leak).
#[test]
fn left_join_null_extends_over_policy_hidden_inner() {
    let d = db("left");
    hide_zero_depts(&d);
    let got = rows(
        d.query(
            "SELECT emp.eid, dept.val FROM emp LEFT JOIN dept ON dept.did = emp.did \
             ORDER BY emp.eid",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(10)], // visible dept
            vec![Value::Int(2), Value::Null],    // hidden dept: absent, NOT val=0
            vec![Value::Int(3), Value::Null],    // NULL did: matches nothing
        ]
    );
}

/// WHERE over a NULL-extended slot is UNKNOWN, never a raise: `100 / NULL`
/// short-circuits to NULL before any division-by-zero check, and NULL is not
/// TRUE — so the extended rows are filtered, silently and correctly.
#[test]
fn where_over_null_extended_slot_is_unknown_not_raise() {
    let d = db("wnull");
    hide_zero_depts(&d);
    let got = rows(
        d.query(
            "SELECT emp.eid FROM emp LEFT JOIN dept ON dept.did = emp.did \
             WHERE 100 / dept.val = 10 ORDER BY emp.eid",
            &[],
        )
        .expect("dividing by a NULL-extended slot is UNKNOWN, not an error"),
    );
    assert_eq!(got, vec![vec![Value::Int(1)]]);
}

/// The asymmetry: a policy on the OUTER table removes its rows entirely —
/// LEFT preserves unmatched OUTER rows, never policy-hidden ones.
#[test]
fn left_join_policy_on_outer_removes_rows() {
    let d = db("outer");
    d.query("CREATE POLICY only_low ON emp FOR ALL USING (eid < 3)", &[]).unwrap();
    d.query("ALTER TABLE emp ENABLE ROW LEVEL SECURITY", &[]).unwrap();
    let got = rows(
        d.query(
            "SELECT emp.eid FROM emp LEFT JOIN dept ON dept.did = emp.did ORDER BY emp.eid",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}
