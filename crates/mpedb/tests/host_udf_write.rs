//! Host UDFs (scalar + aggregate) on the WRITE path — design/DESIGN-UDF.md.
//!
//! CPython's `sqlite3` opens an implicit transaction on the first DML, so in
//! real Django use almost every UDF call after the first INSERT runs inside an
//! open write transaction. These tests pin that shape: the same closures must
//! resolve from a `WriteSession`, from autocommit DML (values / WHERE /
//! RETURNING), and a plan naming a host UDF must still never reach the shared
//! `plan/<hash>` registry.

use mpedb::{Config, Database, Error, ExecResult, HostAggState, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn test_db(name: &str) -> (Database, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-udfw-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 16

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "n"
  type = "int64"
  nullable = true

  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
"#,
        path.display()
    );
    let cfg = Config::from_toml_str(&toml).expect("config");
    let db = Database::open_with_config(cfg).expect("open");
    (db, FileGuard(path))
}

/// `plus1(x) = x + 1`, the stand-in for a Django scalar UDF.
fn register_plus1(db: &Database) {
    db.register_host_function("plus1", 1, |args: &[Value]| match args[0] {
        Value::Int(i) => Ok(Value::Int(i + 1)),
        Value::Null => Ok(Value::Null),
        _ => Err(Error::TypeMismatch("plus1 wants an int".into())),
    });
}

/// A host aggregate: sum of arguments + 100, so it cannot be confused with the
/// built-in `sum`.
struct SumPlus(i64);
impl HostAggState for SumPlus {
    fn step(&mut self, args: &[Value]) -> Result<(), Error> {
        if let Some(Value::Int(i)) = args.first() {
            self.0 += i;
        }
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<Value, Error> {
        Ok(Value::Int(self.0 + 100))
    }
}

fn register_sumplus(db: &Database) {
    db.register_host_aggregate("sumplus", 1, || Box::new(SumPlus(0)));
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

// ---------------------------------------------------------------- scalars

/// The CPython shape: INSERT (opens the implicit transaction), then a UDF call
/// with NO intervening commit.
#[test]
fn scalar_udf_resolves_inside_an_open_transaction() {
    let (db, _g) = test_db("scalar-txn");
    register_plus1(&db);
    let mut s = db.begin().expect("begin");
    s.query("INSERT INTO t (id, n) VALUES (1, 10)", &[])
        .expect("insert");
    // read inside the open write transaction
    let r = rows(s.query("SELECT plus1(n) FROM t", &[]).expect("select"));
    assert_eq!(r, vec![vec![Value::Int(11)]]);
    s.commit().expect("commit");
}

/// A UDF in a write statement's source rows / SET / WHERE / RETURNING, inside
/// a session.
#[test]
fn scalar_udf_in_dml_inside_a_session() {
    let (db, _g) = test_db("scalar-dml-txn");
    register_plus1(&db);
    let mut s = db.begin().expect("begin");
    // INSERT ... SELECT: the UDF runs in the row-producing side of a write
    // statement (`INSERT ... VALUES (<expr>)` takes only literals/parameters —
    // a general limit of the INSERT surface, not a UDF one; see
    // `insert_values_expression_refusal_is_general`).
    s.query("INSERT INTO t (id, n) VALUES (1, 41)", &[]).unwrap();
    s.query("INSERT INTO t (id, n) SELECT plus1(id), plus1(n) FROM t", &[])
        .expect("insert ... select with udf");
    assert_eq!(
        rows(s.query("SELECT n FROM t WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::Int(42)]]
    );
    s.query("DELETE FROM t WHERE id = 2", &[]).unwrap();
    s.query("UPDATE t SET n = 42 WHERE id = 1", &[]).unwrap();
    // SET expression + WHERE
    s.query("UPDATE t SET n = plus1(n) WHERE plus1(id) = 2", &[])
        .expect("update with udf");
    assert_eq!(
        rows(s.query("SELECT n FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int(43)]]
    );
    // RETURNING
    let r = rows(
        s.query("DELETE FROM t WHERE plus1(id) = 2 RETURNING plus1(n)", &[])
            .expect("delete returning udf"),
    );
    assert_eq!(r, vec![vec![Value::Int(44)]]);
    s.commit().expect("commit");
}

/// The same, on the AUTOCOMMIT write path (no session): `Database::query`
/// routes DML through the writer lock / ring leader.
#[test]
fn scalar_udf_in_autocommit_dml() {
    let (db, _g) = test_db("scalar-dml-auto");
    register_plus1(&db);
    db.query("INSERT INTO t (id, n) VALUES (0, 41)", &[]).unwrap();
    db.query("INSERT INTO t (id, n) SELECT plus1(id), plus1(n) FROM t", &[])
        .expect("insert ... select with udf");
    db.query("DELETE FROM t WHERE id = 0", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int(42)]]
    );
    db.query("UPDATE t SET n = plus1(n) WHERE plus1(id) = 2", &[])
        .expect("update with udf");
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int(43)]]
    );
    let r = rows(
        db.query("DELETE FROM t WHERE id = 1 RETURNING plus1(n)", &[])
            .expect("delete returning udf"),
    );
    assert_eq!(r, vec![vec![Value::Int(44)]]);
}

/// `execute(hash, …)` — the prepared-plan path — inside a session.
#[test]
fn scalar_udf_via_prepared_hash_in_session() {
    let (db, _g) = test_db("scalar-hash");
    register_plus1(&db);
    let h = db.prepare("SELECT plus1(n) FROM t").expect("prepare");
    db.query("INSERT INTO t (id, n) VALUES (1, 5)", &[]).unwrap();
    let mut s = db.begin().expect("begin");
    s.query("INSERT INTO t (id, n) VALUES (2, 6)", &[]).unwrap();
    let r = rows(s.execute(&h, &[]).expect("execute by hash in session"));
    assert_eq!(r, vec![vec![Value::Int(6)], vec![Value::Int(7)]]);
    s.commit().unwrap();
}

// -------------------------------------------------------------- aggregates

#[test]
fn aggregate_udf_resolves_inside_an_open_transaction() {
    let (db, _g) = test_db("agg-txn");
    register_sumplus(&db);
    let mut s = db.begin().expect("begin");
    s.query("INSERT INTO t (id, n) VALUES (1, 10), (2, 20)", &[])
        .expect("insert");
    let r = rows(s.query("SELECT sumplus(n) FROM t", &[]).expect("agg"));
    assert_eq!(r, vec![vec![Value::Int(130)]]);
    // grouped, and each group gets its own state
    let r = rows(
        s.query("SELECT id, sumplus(n) FROM t GROUP BY id ORDER BY id", &[])
            .expect("grouped agg"),
    );
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(110)],
            vec![Value::Int(2), Value::Int(120)]
        ]
    );
    s.commit().expect("commit");
}

/// A host aggregate feeding a write statement: `INSERT ... SELECT sumplus(...)`
/// runs the aggregate on the WRITE path.
#[test]
fn aggregate_udf_feeding_a_write_statement() {
    let (db, _g) = test_db("agg-dml");
    register_sumplus(&db);
    db.query("INSERT INTO t (id, n) VALUES (1, 10), (2, 20)", &[])
        .unwrap();
    db.query("INSERT INTO t (id, n) SELECT 3, sumplus(n) FROM t", &[])
        .expect("insert from a host aggregate");
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Int(130)]]
    );
    // and the same inside an open transaction
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t (id, n) VALUES (4, 1)", &[]).unwrap();
    s.query("INSERT INTO t (id, n) SELECT 5, sumplus(n) FROM t WHERE id < 4", &[])
        .expect("insert from a host aggregate in a session");
    assert_eq!(
        rows(s.query("SELECT n FROM t WHERE id = 5", &[]).unwrap()),
        vec![vec![Value::Int(260)]]
    );
    s.commit().unwrap();
}

// ------------------------------------------------- plan-registry containment

/// A plan containing a `HostCall` must NEVER be published to the shared
/// content-hashed registry — from the write path either. Verified by asking a
/// SECOND handle (its own local cache empty, no UDFs registered) to execute the
/// hash: it must fail with `UnknownPlan`, which is only possible if the plan
/// was never stored.
#[test]
fn host_call_plans_never_reach_the_shared_registry() {
    let (db, g) = test_db("registry");
    register_plus1(&db);
    register_sumplus(&db);

    // (a) autocommit DML with a UDF in its source rows
    db.query("INSERT INTO t (id, n) VALUES (1, 41)", &[]).unwrap();
    db.query("INSERT INTO t (id, n) SELECT plus1(id), plus1(n) FROM t", &[])
        .unwrap();
    // (b) autocommit DML with a UDF in its WHERE
    db.query("UPDATE t SET n = 7 WHERE plus1(id) = 2", &[]).unwrap();
    // (c) the same inside a session
    {
        let mut s = db.begin().unwrap();
        s.query("UPDATE t SET n = plus1(n) WHERE id = 1", &[])
            .unwrap();
        s.query("SELECT sumplus(n) FROM t", &[]).unwrap();
        s.commit().unwrap();
    }
    // (d) a prepared read
    let h_read = db.prepare("SELECT plus1(n) FROM t").unwrap();

    // Everything a UDF-bearing plan could have been stored under:
    let hashes = vec![
        h_read,
        db.prepare_detached("INSERT INTO t (id, n) SELECT plus1(id), plus1(n) FROM t")
            .unwrap()
            .hash,
        db.prepare_detached("UPDATE t SET n = plus1(n) WHERE id = 1")
            .unwrap()
            .hash,
        db.prepare_detached("DELETE FROM t WHERE id = 1 RETURNING plus1(n)")
            .unwrap()
            .hash,
        db.prepare_detached("UPDATE t SET n = 7 WHERE plus1(id) = 2")
            .unwrap()
            .hash,
        db.prepare_detached("SELECT sumplus(n) FROM t").unwrap().hash,
    ];

    // A fresh handle onto the same file: empty local cache, no UDFs. Any of
    // those hashes resolving would mean the plan was published.
    let db2 = Database::open_from_file(&g.0).expect("second handle");
    for h in &hashes {
        match db2.execute(h, &[]) {
            Err(Error::UnknownPlan(_)) => {}
            other => panic!("host-call plan {h} leaked into the shared registry: {other:?}"),
        }
    }
    drop(db2);

    // A plan WITHOUT a host call still publishes, so the test above is not
    // vacuous.
    let plain = db.prepare("SELECT n FROM t").unwrap();
    let db3 = Database::open_from_file(&g.0).expect("third handle");
    assert!(
        db3.execute(&plain, &[]).is_ok(),
        "a UDF-free plan must still be shared"
    );
}

// ------------------------------------------------------- clean out-of-scope

/// A context that genuinely cannot carry host closures must refuse with a
/// message that names the limit — never `internal error (bug in mpedb)`.
#[test]
fn out_of_scope_refusal_is_clean_not_internal() {
    let (db, _g) = test_db("out-of-scope");
    register_plus1(&db);
    db.query("INSERT INTO t (id, n) VALUES (1, 1)", &[]).unwrap();
    let h = db.prepare("SELECT plus1(n) FROM t").unwrap();

    // The STREAMING read path (`stream_query`) carries no host closures at
    // all, so it must refuse cleanly.
    let e = db
        .stream_query(&h, &[])
        .and_then(|mut s| {
            while s.next()?.is_some() {}
            Ok(())
        })
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        !e.contains("internal error"),
        "out-of-scope host UDF must not report an engine bug: {e}"
    );
    assert!(
        e.contains("plus1") && e.contains("not in scope"),
        "refusal must name the limit, got: {e}"
    );
    let _ = h;
}


// ------------------------------------------------------------ safety: panic

/// A host UDF is arbitrary CALLER code running inside the write transaction,
/// under the single writer lock. A panic in it must not unwind through the
/// engine: it becomes an ordinary statement error, the transaction stays the
/// executor's to roll back, and the connection stays usable.
#[test]
fn a_panicking_udf_fails_the_statement_and_leaves_the_db_usable() {
    let (db, _g) = test_db("panic");
    db.register_host_function("boom", 1, |_args: &[Value]| -> Result<Value, Error> {
        panic!("host UDF exploded");
    });
    db.query("INSERT INTO t (id, n) VALUES (1, 1), (2, 2)", &[])
        .unwrap();

    // autocommit write path
    let e = db
        .query("UPDATE t SET n = boom(n) WHERE id = 1", &[])
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unsupported(m) if m.contains("panicked")),
        "a panicking UDF must surface as a statement error, got {e:?}"
    );
    // nothing was written, and the writer lock was released cleanly — the very
    // next write proves both.
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int(1)]]
    );
    db.query("UPDATE t SET n = 9 WHERE id = 1", &[])
        .expect("the writer lock must not be poisoned by a UDF panic");

    // inside an open transaction: the session survives, rolls back its own
    // statement's failure, and can still commit its earlier work
    let mut s = db.begin().expect("begin after a panic");
    s.query("INSERT INTO t (id, n) VALUES (3, 3)", &[]).unwrap();
    let e = s.query("SELECT boom(n) FROM t", &[]).unwrap_err();
    assert!(
        matches!(&e, Error::Unsupported(m) if m.contains("panicked")),
        "got {e:?}"
    );
    s.commit().expect("a read that panicked must not poison the session");
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Int(3)]]
    );
    db.verify().expect("page accounting after a UDF panic");
}

/// The aggregate twin: a panic in `xStep`/`xFinal` is a statement error too.
#[test]
fn a_panicking_aggregate_fails_the_statement() {
    struct Boom;
    impl HostAggState for Boom {
        fn step(&mut self, _args: &[Value]) -> Result<(), Error> {
            panic!("xStep exploded");
        }
        fn finish(self: Box<Self>) -> Result<Value, Error> {
            Ok(Value::Null)
        }
    }
    let (db, _g) = test_db("agg-panic");
    db.register_host_aggregate("boomagg", 1, || Box::new(Boom));
    db.query("INSERT INTO t (id, n) VALUES (1, 1)", &[]).unwrap();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t (id, n) VALUES (2, 2)", &[]).unwrap();
    let e = s.query("SELECT boomagg(n) FROM t", &[]).unwrap_err();
    assert!(
        matches!(&e, Error::Unsupported(m) if m.contains("panicked")),
        "got {e:?}"
    );
    s.commit().unwrap();
    db.verify().expect("page accounting after an aggregate panic");
}

// ------------------------------------------------------ safety: reentrancy

/// A host UDF sees ONLY its arguments (design/DESIGN-UDF.md): it is handed
/// evaluated `Value`s and hands one back, never the transaction, the snapshot,
/// or a table handle. The only way it could reach the database is by capturing
/// a `Database` of its own — and from the write path that hits the ERRORCHECK
/// writer lock, which ERRORS rather than deadlocking or letting a UDF mutate
/// the database behind the executor's back.
#[test]
fn a_udf_that_reenters_the_database_errors_rather_than_deadlocking() {
    use std::sync::Arc;
    let (db, _g) = test_db("reenter");
    let db = Arc::new(db);
    let weak = Arc::downgrade(&db);
    db.register_host_function("reenter", 1, move |args: &[Value]| {
        let d = weak.upgrade().expect("db alive");
        // A nested write from inside the write path: must not hang.
        match d.query("INSERT INTO t (id, n) VALUES (99, 99)", &[]) {
            Ok(_) => Ok(Value::Int(-1)),
            // The engine refuses a nested write transaction from the lock's
            // own owner ("writer lock re-entered by its owner") instead of
            // deadlocking. The UDF swallows it and passes its argument through.
            Err(_) => Ok(args[0].clone()),
        }
    });
    db.query("INSERT INTO t (id, n) VALUES (1, 5)", &[]).unwrap();
    // Terminates (no deadlock) with the argument passed through: the nested
    // write was REFUSED, so the UDF never mutated anything.
    let r = db
        .query("UPDATE t SET n = reenter(n) WHERE id = 1 RETURNING n", &[])
        .expect("a reentrant UDF must not hang the writer");
    assert_eq!(rows(r), vec![vec![Value::Int(5)]]);
    assert!(
        rows(db.query("SELECT id FROM t WHERE id = 99", &[]).unwrap()).is_empty(),
        "a UDF must not have mutated the database from inside the write path"
    );
    db.verify().expect("page accounting after a reentrant UDF");
}

// ------------------------------------------------ documented remaining limit

/// `INSERT ... VALUES (<expression>)` takes only literals and parameters — for
/// EVERY function, not just host UDFs. The refusal names that limit, and
/// `INSERT ... SELECT` is the working form.
#[test]
fn insert_values_expression_refusal_is_general() {
    let (db, _g) = test_db("insert-values");
    register_plus1(&db);
    let native = db
        .query("INSERT INTO t (id, n) VALUES (1, abs(-5))", &[])
        .unwrap_err();
    let host = db
        .query("INSERT INTO t (id, n) VALUES (1, plus1(4))", &[])
        .unwrap_err();
    for e in [&native, &host] {
        assert!(
            matches!(e, Error::Bind(m) if m.contains("literals or parameters")),
            "expected the general INSERT-VALUES limit, got {e:?}"
        );
    }
    // the working form
    db.query("INSERT INTO t (id, n) VALUES (1, 4)", &[]).unwrap();
    db.query("INSERT INTO t (id, n) SELECT 2, plus1(n) FROM t WHERE id = 1", &[])
        .expect("INSERT ... SELECT carries expressions, UDFs included");
    assert_eq!(
        rows(db.query("SELECT n FROM t WHERE id = 2", &[]).unwrap()),
        vec![vec![Value::Int(5)]]
    );
}

// ------------------------------------------- the durable / group-commit path

/// With `durability = "commit"` the autocommit write path routes through the
/// Phase-2 group-commit leader (`ring_exec`). A host-call plan must (a) still
/// resolve its closures there and (b) never be enqueued on the intent ring —
/// the ring is cross-process and loads plans BY HASH from the shared registry,
/// where a host-call plan deliberately never appears.
#[test]
fn write_path_udf_under_durability_commit() {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-udfw-durable-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 16
durability = "commit"

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "n"
  type = "int64"
  nullable = true
"#,
        path.display()
    );
    let _g = FileGuard(path.clone());
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config"))
        .expect("open durable");
    register_plus1(&db);
    register_sumplus(&db);

    db.query("INSERT INTO t (id, n) VALUES (1, 10)", &[]).unwrap();
    // leader executes our own statement with the closures in scope
    db.query("UPDATE t SET n = plus1(n) WHERE plus1(id) = 2", &[])
        .expect("udf write under durability=commit");
    let r = rows(
        db.query("DELETE FROM t WHERE id = 1 RETURNING plus1(n)", &[])
            .expect("udf RETURNING under durability=commit"),
    );
    assert_eq!(r, vec![vec![Value::Int(12)]]);

    // still never published
    let h = db
        .prepare_detached("UPDATE t SET n = plus1(n) WHERE plus1(id) = 2")
        .unwrap()
        .hash;
    let db2 = Database::open_from_file(&path).expect("second handle");
    assert!(
        matches!(db2.execute(&h, &[]), Err(Error::UnknownPlan(_))),
        "a host-call plan must not be in the shared registry"
    );
    drop(db2);
    db.verify().expect("page accounting");
}
