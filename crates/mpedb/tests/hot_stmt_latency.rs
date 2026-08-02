//! N2 instrument: per-statement latency, layer-separated. NOT a correctness
//! test — an `#[ignore]`d manual-timing harness (repo convention; no
//! criterion dep). Release builds ONLY:
//!
//!     cargo test --release -p mpedb --test hot_stmt_latency -- --ignored --nocapture
//!
//! Debug numbers are meaningless here twice over: unoptimized code, and
//! `verify_plan_memo` recompiles every memo hit in debug builds (also under
//! MPEDB_VERIFY_PLAN_MEMO=1 — don't set it for timing runs).
//!
//! Cells, each after one warm-up call:
//!   a   session INSERT ×N inside one txn      (the Django shape)
//!   a2  session point-SELECT ×N inside one txn (same path, no write-apply)
//!   b   autocommit point-SELECT ×N             (the #166 memo baseline)
//!   b2  autocommit INSERT ×N                   (REPORTED ONLY — contains a
//!                                              commit per op, not comparable)
//!   sp  savepoint cycle ×N inside one txn      (SAVEPOINT/INSERT/ROLLBACK
//!                                              TO/RELEASE — 4 stmts/cycle)
//! (a2)−(b) = session-path overhead; (a)−(a2) = write-apply cost.

use mpedb::{Config, Database, ExecResult, Value};
use std::time::Instant;

const N: usize = 10_000;
const ROUNDS: usize = 3;

fn memdb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 128\nmax_readers = 8\n\n\
                [[table]]\nname = \"users\"\nprimary_key = [\"id\"]\n\
                [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
                [[table.column]]\nname = \"email\"\ntype = \"text\"\n\
                [[table.column]]\nname = \"age\"\ntype = \"int64\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn ns_per_op(total_ns: u128, ops: usize) -> u128 {
    total_ns / ops as u128
}

fn report(cell: &str, round: usize, ns: u128) {
    eprintln!("cell={cell} round={round} ns_per_op={ns}");
}

fn seed(db: &Database, rows: i64) {
    let mut s = db.begin().unwrap();
    for i in 0..rows {
        s.query(
            "INSERT INTO users VALUES ($1, $2, $3)",
            &[Value::Int(i), Value::Text(format!("u{i}@x.no")), Value::Int(i % 90)],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

fn assert_point(res: ExecResult) {
    match res {
        ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        _ => panic!("point select must return rows"),
    }
}

#[test]
#[ignore]
fn hot_stmt_latency() {
    if cfg!(debug_assertions) {
        eprintln!("hot_stmt_latency: debug build — numbers would lie (verify_plan_memo recompiles every hit); run with --release");
        return;
    }
    if std::env::var_os("MPEDB_VERIFY_PLAN_MEMO").is_some() {
        eprintln!("hot_stmt_latency: MPEDB_VERIFY_PLAN_MEMO is set — unset it for timing runs");
        return;
    }

    let db = memdb();
    seed(&db, 1_000);

    for round in 0..ROUNDS {
        // (a) session INSERT ×N inside one txn, rolled back so every round
        // sees the same table size.
        {
            let mut s = db.begin().unwrap();
            s.query(
                "INSERT INTO users VALUES ($1, $2, $3)",
                &[Value::Int(1_000_000), Value::Text("w@x.no".into()), Value::Int(1)],
            )
            .unwrap();
            let t = Instant::now();
            for i in 0..N {
                s.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[
                        Value::Int(2_000_000 + (round * N + i) as i64),
                        Value::Text("v@x.no".into()),
                        Value::Int(7),
                    ],
                )
                .unwrap();
            }
            report("a_session_insert_txn", round, ns_per_op(t.elapsed().as_nanos(), N));
            s.rollback();
        }

        // (a2) session point-SELECT ×N inside one txn.
        {
            let mut s = db.begin().unwrap();
            assert_point(s.query("SELECT email FROM users WHERE id = $1", &[Value::Int(1)]).unwrap());
            let t = Instant::now();
            for i in 0..N {
                let r = s
                    .query("SELECT email FROM users WHERE id = $1", &[Value::Int((i % 1000) as i64)])
                    .unwrap();
                assert_point(r);
            }
            report("a2_session_select_txn", round, ns_per_op(t.elapsed().as_nanos(), N));
            s.rollback();
        }

        // (b) autocommit point-SELECT ×N — the #166 baseline.
        {
            assert_point(db.query("SELECT email FROM users WHERE id = $1", &[Value::Int(1)]).unwrap());
            let t = Instant::now();
            for i in 0..N {
                let r = db
                    .query("SELECT email FROM users WHERE id = $1", &[Value::Int((i % 1000) as i64)])
                    .unwrap();
                assert_point(r);
            }
            report("b_auto_select", round, ns_per_op(t.elapsed().as_nanos(), N));
        }

        // (b2) autocommit INSERT — reported only; each op contains a commit.
        {
            let base = 3_000_000 + (round * N) as i64;
            db.query(
                "INSERT INTO users VALUES ($1, $2, $3)",
                &[Value::Int(base - 1), Value::Text("w@x.no".into()), Value::Int(1)],
            )
            .unwrap();
            let t = Instant::now();
            for i in 0..N {
                db.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[Value::Int(base + i as i64), Value::Text("v@x.no".into()), Value::Int(7)],
                )
                .unwrap();
            }
            report("b2_auto_insert_NOT_COMPARABLE", round, ns_per_op(t.elapsed().as_nanos(), N));
            db.query("DELETE FROM users WHERE id >= $1", &[Value::Int(3_000_000)]).unwrap();
        }

        // (sp) savepoint cycle ×N/4 inside one txn (4 statements per cycle,
        // reported per STATEMENT so it lands on the same axis).
        {
            let cycles = N / 4;
            let mut s = db.begin().unwrap();
            s.query("SAVEPOINT w", &[]).unwrap();
            s.query("RELEASE SAVEPOINT w", &[]).unwrap();
            let t = Instant::now();
            for k in 0..cycles {
                s.query("SAVEPOINT sp", &[]).unwrap();
                s.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[Value::Int(4_000_000 + k as i64), Value::Text("s@x.no".into()), Value::Int(3)],
                )
                .unwrap();
                s.query("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
                s.query("RELEASE SAVEPOINT sp", &[]).unwrap();
            }
            report("sp_savepoint_cycle_stmt", round, ns_per_op(t.elapsed().as_nanos(), cycles * 4));
            s.rollback();
        }

        // (spu) savepoint cycle with UNIQUE names — Django's actual shape
        // (`s<pid>_x<n>` per use). Pre-N2 every op compiled (full prelude),
        // remembered (memo CLOCK churn against the hot texts) and grew the
        // hash cache without bound.
        {
            let cycles = N / 4;
            let mut s = db.begin().unwrap();
            s.query("SAVEPOINT w0", &[]).unwrap();
            s.query("RELEASE SAVEPOINT w0", &[]).unwrap();
            let t = Instant::now();
            for k in 0..cycles {
                let n = round * cycles + k;
                s.query(&format!("SAVEPOINT s{n}"), &[]).unwrap();
                s.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[Value::Int(5_000_000 + n as i64), Value::Text("u@x.no".into()), Value::Int(2)],
                )
                .unwrap();
                s.query(&format!("ROLLBACK TO SAVEPOINT s{n}"), &[]).unwrap();
                s.query(&format!("RELEASE SAVEPOINT s{n}"), &[]).unwrap();
            }
            report("spu_savepoint_unique_stmt", round, ns_per_op(t.elapsed().as_nanos(), cycles * 4));
            s.rollback();
        }

        // (spl) savepoint cycle over a LARGE dirty set — Django's fixture
        // shape (setUpTestData in the outer transaction, then a savepoint
        // per test). Pre-N2-fase-4 every SAVEPOINT eagerly copied the whole
        // dirty set (and every ROLLBACK TO cloned it again), so this cell
        // scaled with fixture size instead of with what the test touched.
        {
            let cycles = 500;
            let mut s = db.begin().unwrap();
            for i in 0..2000 {
                s.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[Value::Int(6_000_000 + i), Value::Text("f@x.no".into()), Value::Int(1)],
                )
                .unwrap();
            }
            s.query("SAVEPOINT w1", &[]).unwrap();
            s.query("RELEASE SAVEPOINT w1", &[]).unwrap();
            let t = Instant::now();
            for k in 0..cycles {
                s.query("SAVEPOINT sp", &[]).unwrap();
                s.query(
                    "INSERT INTO users VALUES ($1, $2, $3)",
                    &[Value::Int(7_000_000 + k as i64), Value::Text("s@x.no".into()), Value::Int(3)],
                )
                .unwrap();
                s.query("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
                s.query("RELEASE SAVEPOINT sp", &[]).unwrap();
            }
            report("spl_savepoint_bigdirty_stmt", round, ns_per_op(t.elapsed().as_nanos(), cycles * 4));
            s.rollback();
        }
    }
    db.verify().unwrap();
}
