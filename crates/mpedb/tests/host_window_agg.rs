//! HOST window aggregates — sqlite's `create_window_function`
//! (design/DESIGN-UDF.md stage 4, PLAN_FORMAT 55).
//!
//! A host aggregate registered with the WINDOW protocol supplies `xValue` (the
//! current frame's value, without consuming the state) and `xInverse` (undo one
//! row that has left the frame) on top of `xStep`/`xFinal`. That pair is what
//! lets a MOVING frame slide instead of being re-aggregated, and it is the whole
//! reason sqlite makes it a separate registration.
//!
//! What is pinned here:
//!   - the ANSWERS match a re-aggregation of each frame, for a moving ROWS
//!     frame, the default frame, and PARTITION BY;
//!   - the CALL SEQUENCE is sqlite's — `inverse` really is invoked as the left
//!     edge advances, and `finish` runs once per partition — because a consumer's
//!     callbacks (CPython's, for one) are written expecting exactly that;
//!   - an aggregate registered WITHOUT the window protocol is refused BY NAME
//!     under `OVER`, never answered with a whole-partition value;
//!   - an error out of `step`/`value`/`inverse` propagates, and an error out of
//!     `finish` does NOT (sqlite does not propagate a finalizer failure out of
//!     `sqlite3_step`).

use mpedb::{Config, Database, ExecResult, Value};
use mpedb_types::HostAggState;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Tmp {
    db: Database,
    path: String,
}
impl std::ops::Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-hwagg-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g\"\ntype = \"text\"\n\
         [[table.column]]\nname = \"y\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

fn ints(r: &ExecResult) -> Vec<i64> {
    match r {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row.last() {
                Some(Value::Int(i)) => *i,
                other => panic!("expected an integer, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The reference implementation from sqlite's own documentation of
/// `create_window_function`: a running integer sum that can also subtract.
/// Every call is appended to a shared journal so the SEQUENCE can be asserted.
struct SumInt {
    total: i64,
    log: Arc<Mutex<Vec<String>>>,
}

impl HostAggState for SumInt {
    fn step(&mut self, args: &[Value]) -> mpedb::Result<()> {
        if let Some(Value::Int(v)) = args.first() {
            self.total += v;
        }
        self.log.lock().unwrap().push(format!("step {:?}", args.first()));
        Ok(())
    }
    fn inverse(&mut self, args: &[Value]) -> mpedb::Result<()> {
        if let Some(Value::Int(v)) = args.first() {
            self.total -= v;
        }
        self.log.lock().unwrap().push(format!("inverse {:?}", args.first()));
        Ok(())
    }
    fn value(&mut self) -> mpedb::Result<Value> {
        self.log.lock().unwrap().push("value".into());
        Ok(Value::Int(self.total))
    }
    fn finish(self: Box<Self>) -> mpedb::Result<Value> {
        self.log.lock().unwrap().push("finish".into());
        Ok(Value::Int(self.total))
    }
}

fn register_sumint(db: &Database) -> Arc<Mutex<Vec<String>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    db.register_host_window_aggregate("sumint", 1, move || {
        Box::new(SumInt { total: 0, log: l.clone() })
    });
    log
}

fn seed(db: &Database, rows: &[(i64, &str, i64)]) {
    for (id, g, y) in rows {
        db.query(&format!("INSERT INTO t (id, g, y) VALUES ({id}, '{g}', {y})"), &[])
            .unwrap();
    }
}

/// sqlite's own worked example for `create_window_function`, answer for answer:
/// `sumint(y) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)`.
#[test]
fn a_moving_rows_frame_slides_and_answers_like_a_re_aggregation() {
    let db = db();
    let log = register_sumint(&db);
    seed(&db, &[(1, "a", 4), (2, "b", 5), (3, "c", 3), (4, "d", 8), (5, "e", 1)]);

    let r = db
        .query(
            "SELECT id, sumint(y) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM t ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(ints(&r), [9, 12, 16, 12, 9]);

    // The SEQUENCE, not just the answers: the left edge really does retract
    // through `inverse`, and the partition is finalized exactly once.
    let log = log.lock().unwrap();
    assert_eq!(log.iter().filter(|e| e.starts_with("step")).count(), 5);
    assert_eq!(log.iter().filter(|e| e.starts_with("inverse")).count(), 3);
    assert_eq!(log.iter().filter(|e| *e == "value").count(), 5);
    assert_eq!(log.iter().filter(|e| *e == "finish").count(), 1);
}

/// The DEFAULT frame (no explicit frame) is cumulative through the end of each
/// peer group, exactly as a built-in aggregate window is — and needs no
/// `inverse`, because the left edge never moves.
#[test]
fn the_default_frame_is_cumulative_and_never_inverses() {
    let db = db();
    let log = register_sumint(&db);
    seed(&db, &[(1, "a", 4), (2, "a", 5), (3, "a", 3)]);

    let r = db
        .query("SELECT id, sumint(y) OVER (ORDER BY id) FROM t ORDER BY id", &[])
        .unwrap();
    assert_eq!(ints(&r), [4, 9, 12]);
    assert_eq!(
        log.lock().unwrap().iter().filter(|e| e.starts_with("inverse")).count(),
        0
    );

    // With no ORDER BY the whole partition is one frame.
    let r = db.query("SELECT id, sumint(y) OVER () FROM t ORDER BY id", &[]).unwrap();
    assert_eq!(ints(&r), [12, 12, 12]);
}

/// PARTITION BY restarts the accumulation — one state, and one `finish`, per
/// partition.
#[test]
fn each_partition_gets_its_own_state() {
    let db = db();
    let log = register_sumint(&db);
    seed(&db, &[(1, "a", 4), (2, "a", 5), (3, "b", 3), (4, "b", 8)]);

    let r = db
        .query(
            "SELECT id, sumint(y) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(ints(&r), [4, 9, 3, 11]);
    assert_eq!(log.lock().unwrap().iter().filter(|e| *e == "finish").count(), 2);
}

/// An aggregate registered WITHOUT the window protocol cannot take `OVER`. The
/// refusal is at PREPARE and names the reason — answering it with a
/// whole-partition value under a bounded frame would be a wrong answer.
#[test]
fn a_plain_host_aggregate_is_refused_under_over() {
    struct Plain(i64);
    impl HostAggState for Plain {
        fn step(&mut self, args: &[Value]) -> mpedb::Result<()> {
            if let Some(Value::Int(v)) = args.first() {
                self.0 += v;
            }
            Ok(())
        }
        fn finish(self: Box<Self>) -> mpedb::Result<Value> {
            Ok(Value::Int(self.0))
        }
    }
    let db = db();
    db.register_host_aggregate("plainsum", 1, || Box::new(Plain(0)));
    seed(&db, &[(1, "a", 4), (2, "a", 5)]);

    // Grouped, it works.
    let r = db.query("SELECT plainsum(y) FROM t", &[]).unwrap();
    assert_eq!(ints(&r), [9]);

    let e = db
        .query("SELECT plainsum(y) OVER (ORDER BY id) FROM t", &[])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("cannot be used with OVER") && e.contains("create_window_function"),
        "refusal should name the missing registration: {e}"
    );
}

/// `step`/`value`/`inverse` errors reach the caller; a `finish` error does not —
/// sqlite does not propagate a finalizer failure out of `sqlite3_step`.
#[test]
fn errors_propagate_from_the_row_callbacks_but_not_from_finish() {
    struct Failing(&'static str);
    fn boom(which: &str) -> mpedb::Error {
        mpedb::Error::Unsupported(format!("{which} exploded"))
    }
    impl HostAggState for Failing {
        fn step(&mut self, _args: &[Value]) -> mpedb::Result<()> {
            if self.0 == "step" {
                return Err(boom("step"));
            }
            Ok(())
        }
        fn inverse(&mut self, _args: &[Value]) -> mpedb::Result<()> {
            if self.0 == "inverse" {
                return Err(boom("inverse"));
            }
            Ok(())
        }
        fn value(&mut self) -> mpedb::Result<Value> {
            if self.0 == "value" {
                return Err(boom("value"));
            }
            Ok(Value::Int(0))
        }
        fn finish(self: Box<Self>) -> mpedb::Result<Value> {
            Err(boom("finish"))
        }
    }
    let db = db();
    seed(&db, &[(1, "a", 4), (2, "a", 5), (3, "a", 6)]);
    let q = "SELECT f(y) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
             FROM t ORDER BY id";
    for which in ["step", "value", "inverse"] {
        db.register_host_window_aggregate("f", 1, move || Box::new(Failing(which)));
        let e = db.query(q, &[]).unwrap_err().to_string();
        assert!(e.contains(&format!("{which} exploded")), "{which}: {e}");
    }
    // Only `finish` fails now: the statement still succeeds.
    db.register_host_window_aggregate("f", 1, || Box::new(Failing("none")));
    assert_eq!(ints(&db.query(q, &[]).unwrap()), [0, 0, 0]);
}

/// A plan naming a host window aggregate is CONNECTION-LOCAL: its callbacks
/// live in THIS connection's registry, so it must never reach the shared
/// content-hashed `plan/<hash>` registry, where another attacher would execute
/// it with no such function registered. Asserted where it is observable — a
/// SECOND connection to the same file, next to an ordinary window plan that
/// does get published and does run there.
#[test]
fn a_host_window_plan_never_reaches_the_shared_registry() {
    let db = db();
    register_sumint(&db);
    seed(&db, &[(1, "a", 4)]);

    let shared = db.prepare("SELECT sum(y) OVER (ORDER BY id) FROM t").unwrap();
    let local = db.prepare("SELECT sumint(y) OVER (ORDER BY id) FROM t").unwrap();
    assert_eq!(ints(&db.execute(&local, &[]).unwrap()), [4]);

    let other = Database::open_from_file(std::path::Path::new(&db.path)).unwrap();
    assert_eq!(ints(&other.execute(&shared, &[]).unwrap()), [4]);
    let e = other.execute(&local, &[]).unwrap_err();
    assert!(
        !matches!(e, mpedb::Error::Io(_)),
        "expected a missing-plan error, got {e}"
    );
}
