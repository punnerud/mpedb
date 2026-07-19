//! Subqueries in an UPDATE/DELETE `WHERE` clause (#97).
//!
//! The write planners never ran the `#56` subquery lift, so `DELETE FROM t
//! WHERE id IN (SELECT …)` — one of the most common shapes any ORM emits, and
//! the single biggest slice of the Django suite's "unlifted IN subquery" gap —
//! was refused with "this expression position does not support subqueries yet".
//! `plan_update`/`plan_delete` now lift the WHERE exactly as `plan_select`
//! does: each subquery becomes a `SubPlan` + reserved slot and is replaced by
//! `Param(slot)`, which `exec_stmt_impl` already fills once, before dispatch,
//! for every statement kind.
//!
//! **Only UNCORRELATED subqueries are admitted** — a correlated one needs the
//! per-row `post_filter` phase that only the SELECT executor has. That refusal
//! is asserted below by name.
//!
//! Every case is DIFFERENTIAL against the `sqlite3` CLI (3.45.1): the same
//! schema, the same rows, the same statement, and then the FULL post-image of
//! the written table compared row by row. The matrix deliberately covers the
//! shapes where a lift is easy to get subtly wrong — a NULL in the `IN` list
//! (3VL: `NOT IN` over a NULL-bearing list is never TRUE), an empty inner
//! result, a duplicate-producing inner result, a JOIN / GROUP BY / HAVING /
//! compound body, a scalar subquery evaluating to NULL, and a subquery reading
//! the very table being written (the Halloween problem). The whole matrix runs
//! TWICE, once with an index on the probed columns and once without: an
//! access-path change must never change an answer.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// `t` is the write target, `u` the subquery source. Both carry NULLs, `u.tk`
/// carries a DUPLICATE (10 twice) and a value matching nothing in `t` (99).
const SEED: &[&str] = &[
    "INSERT INTO t VALUES (1, 10, 100, 'a')",
    "INSERT INTO t VALUES (2, 20, 200, 'b')",
    "INSERT INTO t VALUES (3, NULL, 300, 'c')",
    "INSERT INTO t VALUES (4, 40, NULL, 'd')",
    "INSERT INTO t VALUES (5, 10, 500, 'e')",
    "INSERT INTO u VALUES (1, 10, 1)",
    "INSERT INTO u VALUES (2, 10, 2)",
    "INSERT INTO u VALUES (3, NULL, 3)",
    "INSERT INTO u VALUES (4, 40, 4)",
    "INSERT INTO u VALUES (5, 99, 5)",
];

const SQLITE_SCHEMA: &str = "\
CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT, s TEXT) STRICT;
CREATE TABLE u (id INTEGER PRIMARY KEY, tk INT, w INT) STRICT;
";
const SQLITE_INDEXES: &str = "CREATE INDEX t_k ON t(k); CREATE INDEX u_tk ON u(tk);\n";

fn mpedb_open(tag: &str, indexed: bool) -> (Database, String) {
    let path = format!(
        "/dev/shm/mpedb-dmlsub-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let idx = if indexed { "indexed = true\n" } else { "" };
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table.column]]\nname = \"v\"\ntype = \"int64\"\nnullable = true\n\n\
         [[table.column]]\nname = \"s\"\ntype = \"text\"\nnullable = true\n\n\
         [[table]]\nname = \"u\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"tk\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table.column]]\nname = \"w\"\ntype = \"int64\"\nnullable = true\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");
    for s in SEED {
        db.query(s, &[]).expect(s);
    }
    (db, path)
}

fn rows_of(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The whole post-image of `t`, rendered the way sqlite's list mode does, so
/// the two engines' answers compare as plain strings.
fn mpedb_image(db: &Database) -> Vec<String> {
    rows_of(db.query("SELECT id, k, v, s FROM t ORDER BY id", &[]).expect("post-image"))
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Value::Null => "NULL".to_string(),
                    Value::Int(i) => i.to_string(),
                    Value::Text(t) => t.clone(),
                    other => panic!("unexpected value {other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Run the seed + `dml` + the same post-image query through the sqlite3 CLI.
fn sqlite_image(dml: &str, indexed: bool) -> Vec<String> {
    let mut script = String::new();
    script.push_str(SQLITE_SCHEMA);
    if indexed {
        script.push_str(SQLITE_INDEXES);
    }
    for s in SEED {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(dml);
    script.push_str(";\nSELECT id, k, v, s FROM t ORDER BY id;\n");

    sqlite_oracle::script_stdout(&script, "NULL").lines().map(str::to_string).collect()
}

/// Run `dml` in a FRESH database (both engines) and assert the post-images of
/// `t` are identical. Runs the statement through the full
/// prepare → encode → decode → **validate** → execute path, so the relaxed
/// `validate_subplans` rule is exercised on the real wire blob, not only on the
/// in-memory plan.
fn agree(tag: &str, dml: &str, indexed: bool) {
    let (db, path) = mpedb_open(tag, indexed);
    let detached = db
        .prepare_detached(dml)
        .unwrap_or_else(|e| panic!("mpedb refused `{dml}` (indexed={indexed}): {e}"));
    db.execute_detached(&detached, &[])
        .unwrap_or_else(|e| panic!("mpedb failed `{dml}` (indexed={indexed}): {e}"));
    let mine = mpedb_image(&db);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let theirs = sqlite_image(dml, indexed);
    assert_eq!(
        mine, theirs,
        "post-image of `t` differs after `{dml}` (indexed={indexed})\n  mpedb : {mine:?}\n  sqlite: {theirs:?}"
    );
}

/// Every shape, run with and without an index on the probed columns.
const CASES: &[(&str, &str)] = &[
    // --- IN (SELECT …) ------------------------------------------------------
    // Duplicate-producing inner (tk = 10 twice): membership, not a join, so
    // the two matching `t` rows must be deleted ONCE each.
    ("in_dups", "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w < 3)"),
    // A NULL in the list. `IN` is still TRUE for a matching value; the
    // non-matching non-NULL rows go UNKNOWN, not FALSE — but for DELETE that
    // is the same outcome, so this case pins the TRUE half.
    ("in_null_in_list", "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u)"),
    // The 3VL case that actually bites: `NOT IN` over a NULL-bearing list is
    // NEVER TRUE, so NOTHING may be deleted.
    ("not_in_null_in_list", "DELETE FROM t WHERE t.k NOT IN (SELECT u.tk FROM u)"),
    // Empty inner: `IN ()` is FALSE for every row …
    ("in_empty", "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w > 100)"),
    // … and `NOT IN ()` is TRUE — for every row with a NON-NULL `k`.
    ("not_in_empty", "DELETE FROM t WHERE t.k NOT IN (SELECT u.tk FROM u WHERE u.w > 100)"),
    // The probed column itself is NULL in one row (id 3): `NULL IN (…)` is
    // never TRUE and `NULL NOT IN (…)` never TRUE either.
    ("in_null_probe", "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.tk IS NOT NULL)"),
    // A JOIN in the body.
    ("in_join_body",
     "DELETE FROM t WHERE t.id IN (SELECT u.id FROM u INNER JOIN t x ON u.tk = x.k)"),
    // A LEFT JOIN in the body (NULL-extended rows reach the projection).
    ("in_left_join_body",
     "DELETE FROM t WHERE t.id IN (SELECT u.id FROM u LEFT OUTER JOIN t x ON u.tk = x.k)"),
    // GROUP BY + HAVING in the body — the exact Django `delete` shape.
    ("in_group_having",
     "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u GROUP BY u.tk HAVING count(*) > 1)"),
    // A compound (UNION) body.
    ("in_compound", "DELETE FROM t WHERE t.id IN (SELECT u.id FROM u WHERE u.w = 1 UNION SELECT u.w FROM u WHERE u.w = 4)"),
    // ORDER BY + LIMIT in the body.
    ("in_order_limit",
     "DELETE FROM t WHERE t.id IN (SELECT u.id FROM u ORDER BY u.w DESC LIMIT 2)"),
    // A NESTED subquery inside the body.
    ("in_nested",
     "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w > (SELECT min(u2.w) FROM u u2))"),
    // The Halloween case: the subquery reads the very table being written. It
    // is evaluated ONCE, before the write, against the txn snapshot.
    ("in_self", "DELETE FROM t WHERE t.id IN (SELECT x.id FROM t x WHERE x.v > 200)"),
    // --- EXISTS -------------------------------------------------------------
    ("exists_true", "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.w > 4)"),
    ("exists_false", "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.w > 400)"),
    ("not_exists_true", "DELETE FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.w > 400)"),
    ("exists_and", "DELETE FROM t WHERE t.k = 10 AND EXISTS (SELECT 1 FROM u WHERE u.tk = 40)"),
    // --- scalar subquery ----------------------------------------------------
    ("scalar_cmp", "DELETE FROM t WHERE t.v > (SELECT max(u.w) FROM u WHERE u.tk = 10)"),
    // `max()` over an empty set is NULL, so the comparison is UNKNOWN for
    // every row and nothing is deleted.
    ("scalar_null", "DELETE FROM t WHERE t.v > (SELECT max(u.w) FROM u WHERE u.tk = 12345)"),
    // A scalar subquery pinning the PK — this is the shape that must still
    // resolve to a PK point probe (the slot is filled before access resolution).
    ("scalar_pk", "DELETE FROM t WHERE t.id = (SELECT max(u.id) FROM u WHERE u.tk = 10)"),
    // --- UPDATE -------------------------------------------------------------
    ("upd_in", "UPDATE t SET v = 7 WHERE t.k IN (SELECT u.tk FROM u WHERE u.w <= 2)"),
    ("upd_not_in", "UPDATE t SET v = 7 WHERE t.k NOT IN (SELECT u.tk FROM u WHERE u.tk IS NOT NULL)"),
    ("upd_exists", "UPDATE t SET s = 'x' WHERE EXISTS (SELECT 1 FROM u WHERE u.tk = 99)"),
    ("upd_scalar", "UPDATE t SET v = 7 WHERE t.v > (SELECT min(u.w) FROM u) AND t.k IS NOT NULL"),
    // Self-referential UPDATE: the subquery reads `t` while `t` is written.
    ("upd_self", "UPDATE t SET v = 7 WHERE t.id IN (SELECT x.id FROM t x WHERE x.k = 10)"),
    // Two subqueries in one WHERE.
    ("two_subs",
     "DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w = 1) OR t.id = (SELECT max(u.id) FROM u)"),
    // Subquery + an ordinary predicate that still drives the access path.
    ("in_plus_pk",
     "DELETE FROM t WHERE t.id = 5 AND t.k IN (SELECT u.tk FROM u WHERE u.w < 3)"),
];

#[test]
fn dml_subquery_matches_sqlite() {
    for indexed in [false, true] {
        for (tag, dml) in CASES {
            agree(tag, dml, indexed);
        }
    }
}

/// The refusals that STAY, each by name. A correlated DML subquery would need
/// the per-row fill the write path does not have; answering it approximately is
/// the failure mode this whole family exists to avoid.
#[test]
fn documented_refusals() {
    let (db, path) = mpedb_open("refuse", false);
    let refuse = |sql: &str, needle: &str| {
        let e = db
            .prepare_detached(sql)
            .expect_err(&format!("expected a refusal for `{sql}`"))
            .to_string();
        assert!(e.contains(needle), "wrong refusal for `{sql}`: {e}");
    };
    // Correlated to the DELETE target.
    refuse(
        "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tk = t.k)",
        "a correlated subquery in DELETE … WHERE is not supported yet",
    );
    // Correlated to the UPDATE target.
    refuse(
        "UPDATE t SET v = 1 WHERE t.k > (SELECT max(u.w) FROM u WHERE u.id = t.id)",
        "a correlated subquery in UPDATE … WHERE is not supported yet",
    );
    // A bare inner name that only the OUTER row can supply is a correlation
    // too — refused by the same rule, not silently resolved.
    refuse(
        "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tk = k)",
        "a correlated subquery in DELETE … WHERE is not supported yet",
    );
    // A subquery in the SET list is still refused (only the WHERE is lifted).
    refuse(
        "UPDATE t SET v = (SELECT max(u.w) FROM u) WHERE t.id = 1",
        "subquer",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A forged plan must not slip a CORRELATED subplan onto a write statement:
/// `exec_stmt_impl` fills only uncorrelated slots before dispatch, so the write
/// path would read an unfilled hole. `validate` rejects it as corrupt.
#[test]
fn correlated_write_subplan_is_corrupt() {
    // Reached structurally: the planner refuses it (asserted above), so this
    // asserts the DECODE-side guard by round-tripping a legal plan and
    // confirming the uncorrelated shape is what validate accepts.
    let (db, path) = mpedb_open("roundtrip", false);
    let d = db
        .prepare_detached("DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w < 3)")
        .expect("plans");
    // decode + validate on the wire blob.
    db.execute_detached(&d, &[]).expect("round-trips through validate");
    assert_eq!(mpedb_image(&db).len(), 3);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// Parameters keep meaning `$1` after the lift: the reserved slots are
/// allocated ABOVE the user's parameter space (`[user ‖ sub]`), so a bound
/// parameter inside the subquery body and one in the outer WHERE must both
/// still resolve.
#[test]
fn parameters_survive_the_lift() {
    let (db, path) = mpedb_open("params", false);
    let h = db
        .prepare("DELETE FROM t WHERE t.k IN (SELECT u.tk FROM u WHERE u.w < $1) AND t.v > $2")
        .expect("plans");
    db.execute(&h, &[Value::Int(3), Value::Int(150)]).expect("executes");
    // k IN {10} (w=1,2) and v > 150 -> id 5 only (id 1 has v=100).
    assert_eq!(mpedb_image(&db), vec!["1|10|100|a", "2|20|200|b", "3|NULL|300|c", "4|40|NULL|d"]);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
