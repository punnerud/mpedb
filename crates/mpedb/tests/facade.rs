//! End-to-end tests for the mpedb facade: SQL in, plan hashes out, execution
//! against a real shared-memory engine file.

use mpedb::{params, Config, Database, Error, ExecResult, PlanHash, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn test_config(name: &str, size_mb: u64) -> (Config, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-facade-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 64

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

  [[table.column]]
  name = "age"
  type = "int64"
  check = "age >= 0 AND age < 200"

  [[table.column]]
  name = "created"
  type = "timestamp"
  default = "now()"

[[table]]
name = "t2"
primary_key = ["a", "b"]

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"

  [[table.column]]
  name = "val"
  type = "float64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), FileGuard(path))
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn affected(res: ExecResult) -> u64 {
    match res {
        ExecResult::Affected(n) => n,
        other => panic!("expected affected count, got {other:?}"),
    }
}

/// Single-int-column result as a plain Vec<i64>.
fn ids(res: ExecResult) -> Vec<i64> {
    rows(res)
        .into_iter()
        .map(|r| match &r[..] {
            [Value::Int(i)] => *i,
            other => panic!("expected one int column, got {other:?}"),
        })
        .collect()
}

fn seed_users(db: &Database) {
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    for (id, email, age) in [
        (1i64, "a@x.no", Some(30i64)),
        (2, "b@x.no", Some(17)),
        (3, "c@x.no", None),
        (4, "d@x.no", Some(55)),
    ] {
        let age = age.map(Value::Int).unwrap_or(Value::Null);
        assert_eq!(affected(db.execute(&ins, &params![id, email, age]).unwrap()), 1);
    }
}

#[test]
fn crud_end_to_end() {
    let (cfg, _g) = test_config("crud", 8);
    let db = Database::open_with_config(cfg).unwrap();
    seed_users(&db);
    db.verify().unwrap();

    // -- SELECT: PK point ---------------------------------------------------
    let sel = db
        .prepare("SELECT id, email, age FROM users WHERE id = $1")
        .unwrap();
    match db.execute(&sel, &params![2]).unwrap() {
        ExecResult::Rows { columns, rows } => {
            assert_eq!(columns, ["id", "email", "age"]);
            assert_eq!(
                rows,
                vec![vec![Value::Int(2), Value::Text("b@x.no".into()), Value::Int(17)]]
            );
        }
        other => panic!("{other:?}"),
    }
    // point miss
    assert!(rows(db.execute(&sel, &params![99]).unwrap()).is_empty());

    // -- SELECT: PK range, exclusive lo / inclusive hi ----------------------
    let rng = db
        .prepare("SELECT id FROM users WHERE id > $1 AND id <= $2")
        .unwrap();
    assert_eq!(ids(db.execute(&rng, &params![1, 3]).unwrap()), [2, 3]);
    assert_eq!(ids(db.execute(&rng, &params![0, 99]).unwrap()), [1, 2, 3, 4]);
    // inclusive lo / exclusive hi
    assert_eq!(
        ids(db.query("SELECT id FROM users WHERE id >= 2 AND id < 4", &params![]).unwrap()),
        [2, 3]
    );

    // -- SELECT: secondary unique index point --------------------------------
    let by_email = db.prepare("SELECT id FROM users WHERE email = $1").unwrap();
    assert_eq!(ids(db.execute(&by_email, &params!["c@x.no"]).unwrap()), [3]);
    assert!(ids(db.execute(&by_email, &params!["nobody@x.no"]).unwrap()).is_empty());

    // -- SELECT: full scan with residual WHERE -------------------------------
    // NULL age (id 3) is UNKNOWN for `age >= 30` and must be excluded.
    assert_eq!(
        ids(db.query("SELECT id FROM users WHERE age >= 30", &params![]).unwrap()),
        [1, 4]
    );

    // -- ORDER BY: ASC is NULLS FIRST; DESC reverses (NULLS LAST) ------------
    assert_eq!(
        ids(db.query("SELECT id FROM users ORDER BY age", &params![]).unwrap()),
        [3, 2, 1, 4]
    );
    assert_eq!(
        ids(db.query("SELECT id FROM users ORDER BY age DESC", &params![]).unwrap()),
        [4, 1, 2, 3]
    );

    // -- LIMIT / OFFSET (applied after the sort) ------------------------------
    assert_eq!(
        ids(db
            .query("SELECT id FROM users ORDER BY age LIMIT 2 OFFSET 1", &params![])
            .unwrap()),
        [2, 1]
    );
    assert_eq!(
        ids(db
            .query("SELECT id FROM users ORDER BY id LIMIT 10 OFFSET 3", &params![])
            .unwrap()),
        [4]
    );

    // -- computed projection --------------------------------------------------
    match db.query("SELECT age + 1 FROM users WHERE id = 1", &params![]).unwrap() {
        ExecResult::Rows { columns, rows } => {
            assert_eq!(columns, ["age + 1"]);
            assert_eq!(rows, vec![vec![Value::Int(31)]]);
        }
        other => panic!("{other:?}"),
    }

    // -- UPDATE: set expression evaluates against the OLD row -----------------
    let upd = db
        .prepare("UPDATE users SET age = age + 1 WHERE age IS NOT NULL")
        .unwrap();
    assert_eq!(affected(db.execute(&upd, &params![]).unwrap()), 3);
    assert_eq!(
        rows(db.query("SELECT age FROM users ORDER BY id", &params![]).unwrap()),
        vec![
            vec![Value::Int(31)],
            vec![Value::Int(18)],
            vec![Value::Null],
            vec![Value::Int(56)],
        ]
    );
    db.verify().unwrap();

    // UPDATE via PK point and via index point
    assert_eq!(
        affected(
            db.query("UPDATE users SET email = $1 WHERE id = $2", &params!["b2@x.no", 2])
                .unwrap()
        ),
        1
    );
    assert_eq!(ids(db.execute(&by_email, &params!["b2@x.no"]).unwrap()), [2]);
    assert!(ids(db.execute(&by_email, &params!["b@x.no"]).unwrap()).is_empty());
    // update matching nothing
    assert_eq!(
        affected(db.query("UPDATE users SET age = 1 WHERE id = 99", &params![]).unwrap()),
        0
    );

    // -- DELETE with predicate -------------------------------------------------
    let del = db.prepare("DELETE FROM users WHERE age > $1").unwrap();
    assert_eq!(affected(db.execute(&del, &params![40]).unwrap()), 1); // id 4
    assert_eq!(
        ids(db.query("SELECT id FROM users", &params![]).unwrap()),
        [1, 2, 3]
    );
    // delete range
    assert_eq!(
        affected(db.query("DELETE FROM users WHERE id >= 2 AND id <= 3", &params![]).unwrap()),
        2
    );
    assert_eq!(ids(db.query("SELECT id FROM users", &params![]).unwrap()), [1]);

    db.verify().unwrap();
}

#[test]
fn constraint_violations_surface_precisely() {
    let (cfg, _g) = test_config("constraints", 8);
    let db = Database::open_with_config(cfg).unwrap();
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    db.execute(&ins, &params![1, "a@x.no", 30]).unwrap();

    // duplicate PK
    assert!(matches!(
        db.execute(&ins, &params![1, "other@x.no", 1]),
        Err(Error::PrimaryKeyViolation { table }) if table == "users"
    ));
    // duplicate UNIQUE email
    assert!(matches!(
        db.execute(&ins, &params![2, "a@x.no", 1]),
        Err(Error::UniqueViolation { table, constraint })
            if table == "users" && constraint == "email"
    ));
    // CHECK violation
    assert!(matches!(
        db.execute(&ins, &params![2, "b@x.no", 500]),
        Err(Error::CheckViolation { table, column, .. })
            if table == "users" && column == "age"
    ));
    // NULL into NOT NULL
    assert!(matches!(
        db.execute(&ins, &params![2, Value::Null, 1]),
        Err(Error::NotNullViolation { table, column })
            if table == "users" && column == "email"
    ));
    // wrong param type (facade-level bind check, before any engine work)
    assert!(matches!(
        db.execute(&ins, &params!["not an int", "b@x.no", 1]),
        Err(Error::TypeMismatch(_))
    ));
    // wrong param count
    assert!(matches!(
        db.execute(&ins, &params![2, "b@x.no"]),
        Err(Error::WrongParamCount { expected: 3, got: 2 })
    ));

    // Every failed autocommit DML aborted cleanly: no partial writes.
    assert_eq!(ids(db.query("SELECT id FROM users", &params![]).unwrap()), [1]);
    db.verify().unwrap();
}

#[test]
fn plan_hash_protocol_across_instances() {
    let (cfg, _g) = test_config("planhash", 8);
    let db = Database::open_with_config(cfg.clone()).unwrap();
    seed_users(&db);

    // Formatting-insensitive hashing: same statement, same hash.
    let h1 = db.prepare("SELECT * FROM users WHERE id = $1").unwrap();
    let h2 = db.prepare("select   *\n  FROM users\twhere id = ?").unwrap();
    assert_eq!(h1, h2);
    // prepare is idempotent
    assert_eq!(db.prepare("SELECT * FROM users WHERE id = $1").unwrap(), h1);

    // A SECOND handle that never prepared executes by hash via the registry.
    let db2 = Database::open_with_config(cfg.clone()).unwrap();
    let got = rows(db2.execute(&h1, &params![2]).unwrap());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Int(2));
    assert_eq!(got[0][1], Value::Text("b@x.no".into()));

    // A third handle loads the plan from inside a WriteSession (through the
    // session's own transaction — no nested locking).
    let db3 = Database::open_with_config(cfg.clone()).unwrap();
    let mut s = db3.begin().unwrap();
    assert_eq!(rows(s.execute(&h1, &params![3]).unwrap()).len(), 1);
    s.rollback();

    // DML by hash from a fresh handle, autocommit routing by footprint.
    let hd = db.prepare("DELETE FROM users WHERE id = $1").unwrap();
    let db4 = Database::open_with_config(cfg).unwrap();
    assert_eq!(affected(db4.execute(&hd, &params![4]).unwrap()), 1);
    assert_eq!(
        ids(db.query("SELECT id FROM users", &params![]).unwrap()),
        [1, 2, 3]
    );

    // Unknown hash is a clean, retryable error.
    let bogus = PlanHash([0xAB; 32]);
    assert!(matches!(
        db2.execute(&bogus, &params![]),
        Err(Error::UnknownPlan(h)) if h == bogus
    ));

    db.verify().unwrap();
}

#[test]
fn explain_renders_and_does_not_execute() {
    let (cfg, _g) = test_config("explain", 8);
    let db = Database::open_with_config(cfg).unwrap();

    match db
        .query("EXPLAIN SELECT * FROM users WHERE id = $1", &params![])
        .unwrap()
    {
        ExecResult::Explain(text) => {
            assert!(text.contains("Select users"), "{text}");
            assert!(text.contains("PkPoint"), "{text}");
            assert!(text.contains("read_only=true"), "{text}");
        }
        other => panic!("{other:?}"),
    }

    // EXPLAIN of DML must not execute the statement.
    match db
        .query(
            "EXPLAIN INSERT INTO users (id, email) VALUES (99, 'x@x.no')",
            &params![],
        )
        .unwrap()
    {
        ExecResult::Explain(text) => assert!(text.contains("Insert users"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert!(ids(db.query("SELECT id FROM users", &params![]).unwrap()).is_empty());
    db.verify().unwrap();
}

#[test]
fn write_session_isolation_commit_and_rollback() {
    let (cfg, _g) = test_config("session", 8);
    let db = Database::open_with_config(cfg).unwrap();
    // Prepare shared plans BEFORE opening the session (prepare may take the
    // writer lock, which the session will hold).
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    let count_sel = db.prepare("SELECT id FROM users").unwrap();

    // --- rollback discards ---------------------------------------------------
    let mut s = db.begin().unwrap();
    assert_eq!(affected(s.execute(&ins, &params![1, "a@x.no", 20]).unwrap()), 1);
    // The session sees its own uncommitted writes, via SQL and via hash.
    assert_eq!(
        ids(s.query("SELECT id FROM users WHERE id = 1", &params![]).unwrap()),
        [1]
    );
    assert_eq!(ids(s.execute(&count_sel, &params![]).unwrap()), [1]);
    // A second begin() on the same thread errors instead of deadlocking
    // (ERRORCHECK writer mutex).
    assert!(db.begin().is_err());
    s.rollback();
    assert!(ids(db.execute(&count_sel, &params![]).unwrap()).is_empty());

    // --- multi-statement commit persists atomically ---------------------------
    let mut s = db.begin().unwrap();
    s.execute(&ins, &params![1, "a@x.no", 20]).unwrap();
    s.execute(&ins, &params![2, "b@x.no", 40]).unwrap();
    assert_eq!(
        affected(s.query("UPDATE users SET age = age * 2 WHERE id = 2", &params![]).unwrap()),
        1
    );
    // constraint failures inside the session surface but do not kill it
    assert!(matches!(
        s.execute(&ins, &params![1, "dup@x.no", 1]),
        Err(Error::PrimaryKeyViolation { .. })
    ));
    // EXPLAIN inside a session renders without executing
    assert!(matches!(
        s.query("EXPLAIN DELETE FROM users", &params![]).unwrap(),
        ExecResult::Explain(_)
    ));
    s.commit().unwrap();

    assert_eq!(
        rows(db.query("SELECT id, age FROM users ORDER BY id", &params![]).unwrap()),
        vec![
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(2), Value::Int(80)],
        ]
    );

    // --- drop without commit also rolls back ----------------------------------
    {
        let mut s = db.begin().unwrap();
        s.execute(&ins, &params![9, "z@x.no", 1]).unwrap();
        // dropped here
    }
    assert_eq!(ids(db.execute(&count_sel, &params![]).unwrap()), [1, 2]);

    db.verify().unwrap();
}

#[test]
fn partial_statement_poisons_session_and_commit_refuses() {
    let (cfg, _g) = test_config("poison", 8);
    let db = Database::open_with_config(cfg).unwrap();
    let count_sel = db.prepare("SELECT id FROM users").unwrap();
    let multi_dup =
        "INSERT INTO users (id, email) VALUES (1, 'a@x.no'), (2, 'b@x.no'), (1, 'dup@x.no')";

    // --- multi-row INSERT failing on row 3 inside a session --------------
    let mut s = db.begin().unwrap();
    assert!(matches!(
        s.query(multi_dup, &params![]),
        Err(Error::PrimaryKeyViolation { .. })
    ));
    // Rows 1 and 2 were applied before the failure: the session is poisoned.
    assert!(matches!(
        s.execute(&count_sel, &params![]),
        Err(Error::Unsupported(m)) if m.contains("poisoned")
    ));
    assert!(matches!(
        s.query("SELECT id FROM users", &params![]),
        Err(Error::Unsupported(m)) if m.contains("poisoned")
    ));
    // commit refuses and rolls back instead of persisting the torn prefix.
    assert!(matches!(
        s.commit(),
        Err(Error::Unsupported(m)) if m.contains("poisoned")
    ));
    assert!(ids(db.execute(&count_sel, &params![]).unwrap()).is_empty());

    // --- same statement via autocommit: error, NOTHING persisted ----------
    assert!(matches!(
        db.query(multi_dup, &params![]),
        Err(Error::PrimaryKeyViolation { .. })
    ));
    assert!(ids(db.execute(&count_sel, &params![]).unwrap()).is_empty());
    db.verify().unwrap();

    // --- a failing single-row INSERT does NOT poison ----------------------
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    let mut s = db.begin().unwrap();
    assert_eq!(affected(s.execute(&ins, &params![1, "a@x.no", 20]).unwrap()), 1);
    assert!(matches!(
        s.execute(&ins, &params![1, "dup@x.no", 1]),
        Err(Error::PrimaryKeyViolation { .. })
    ));
    // Still usable and committable: the failed insert had zero side effects.
    assert_eq!(affected(s.execute(&ins, &params![2, "b@x.no", 150]).unwrap()), 1);
    s.commit().unwrap();
    assert_eq!(ids(db.execute(&count_sel, &params![]).unwrap()), [1, 2]);

    // --- multi-row UPDATE failing mid-loop also poisons --------------------
    // ages are (20, 150); age = age + 100 passes CHECK for id 1 (120) and
    // violates it for id 2 (250) AFTER id 1 was already updated.
    let mut s = db.begin().unwrap();
    assert!(matches!(
        s.query("UPDATE users SET age = age + 100", &params![]),
        Err(Error::CheckViolation { .. })
    ));
    assert!(matches!(
        s.commit(),
        Err(Error::Unsupported(m)) if m.contains("poisoned")
    ));
    assert_eq!(
        rows(db.query("SELECT age FROM users ORDER BY id", &params![]).unwrap()),
        vec![vec![Value::Int(20)], vec![Value::Int(150)]]
    );

    // --- rollback of a poisoned session works normally ---------------------
    let mut s = db.begin().unwrap();
    assert!(s
        .query(
            "INSERT INTO users (id, email) VALUES (3, 'c@x.no'), (1, 'dup@x.no')",
            &params![]
        )
        .is_err());
    s.rollback();
    assert_eq!(ids(db.execute(&count_sel, &params![]).unwrap()), [1, 2]);

    db.verify().unwrap();
}

#[test]
fn cold_cache_read_only_execute_never_touches_writer_lock() {
    let (cfg, _g) = test_config("coldread", 8);
    let db = Database::open_with_config(cfg.clone()).unwrap();
    seed_users(&db);
    let sel = db.prepare("SELECT id FROM users WHERE id = $1").unwrap();

    // Hold the single (process-shared) writer lock via an open session.
    let mut s = db.begin().unwrap();
    assert_eq!(
        affected(
            s.query("INSERT INTO users (id, email) VALUES (100, 'w@x.no')", &params![])
                .unwrap()
        ),
        1
    );

    // A fresh handle with a COLD local cache executes the read-only plan
    // from another thread: it loads the plan from the registry and must
    // complete while the writer lock is held (read-only routing invariant),
    // not block behind the session.
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let db2 = Database::open_with_config(cfg).unwrap();
        let res = db2.execute(&sel, &params![1]);
        let _ = tx.send(());
        res
    });
    let completed = rx.recv_timeout(std::time::Duration::from_secs(10));
    // Unblock the worker regardless, so a regression fails the assert below
    // instead of hanging the whole test binary.
    s.rollback();
    assert!(
        completed.is_ok(),
        "cold-cache read-only execute blocked behind the writer lock"
    );
    let got = ids(worker.join().unwrap().unwrap());
    assert_eq!(got, [1]);
    db.verify().unwrap();
}

#[test]
fn now_default_is_recent_and_shared_within_one_insert() {
    let (cfg, _g) = test_config("nowdefault", 8);
    let db = Database::open_with_config(cfg).unwrap();

    db.query(
        "INSERT INTO users (id, email) VALUES (1, 'a@x.no'), (2, 'b@x.no')",
        &params![],
    )
    .unwrap();

    let got = rows(
        db.query("SELECT created, age FROM users ORDER BY id", &params![])
            .unwrap(),
    );
    assert_eq!(got.len(), 2);
    let (Value::Timestamp(t1), Value::Timestamp(t2)) = (&got[0][0], &got[1][0]) else {
        panic!("expected timestamps, got {got:?}");
    };
    // now() is bound once per execute(): identical across the rows of one
    // multi-row INSERT.
    assert_eq!(t1, t2);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64;
    assert!((now - t1).abs() < 60_000_000, "timestamp not recent: {t1}");
    // Omitted nullable column without default -> NULL.
    assert_eq!(got[0][1], Value::Null);

    // A second INSERT later gets its own (>=) timestamp.
    db.query("INSERT INTO users (id, email) VALUES (3, 'c@x.no')", &params![])
        .unwrap();
    let got = rows(db.query("SELECT created FROM users WHERE id = 3", &params![]).unwrap());
    let Value::Timestamp(t3) = got[0][0] else { panic!() };
    assert!(t3 >= *t1);

    db.verify().unwrap();
}

#[test]
fn composite_pk_range_uses_prefix_semantics() {
    let (cfg, _g) = test_config("composite", 8);
    let db = Database::open_with_config(cfg).unwrap();
    let ins = db
        .prepare("INSERT INTO t2 (a, b, val) VALUES ($1, $2, $3)")
        .unwrap();
    for (a, b, v) in [
        (1i64, "a", Value::Float(1.0)),
        (1, "b", Value::Float(1.5)),
        (1, "zzz", Value::Null),
        (2, "a", Value::Float(2.0)),
        (2, "m", Value::Float(2.5)),
        (3, "x", Value::Float(3.0)),
    ] {
        db.execute(&ins, &params![a, b, v]).unwrap();
    }

    let pairs = |sql: &str, ps: &[Value]| -> Vec<(i64, String)> {
        rows(db.query(sql, ps).unwrap())
            .into_iter()
            .map(|r| match &r[..] {
                [Value::Int(a), Value::Text(b)] => (*a, b.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let p = |a: i64, b: &str| (a, b.to_string());

    // Exclusive lo: NO (1, b) row may leak in, however large its b — this is
    // exactly the prefix_hi construction.
    assert_eq!(
        pairs("SELECT a, b FROM t2 WHERE a > 1 AND a <= 2", &params![]),
        [p(2, "a"), p(2, "m")]
    );
    // Inclusive lo keeps the whole a=2 group.
    assert_eq!(
        pairs("SELECT a, b FROM t2 WHERE a >= 2", &params![]),
        [p(2, "a"), p(2, "m"), p(3, "x")]
    );
    // Exclusive hi cuts the whole a=2 group.
    assert_eq!(
        pairs("SELECT a, b FROM t2 WHERE a < 2", &params![]),
        [p(1, "a"), p(1, "b"), p(1, "zzz")]
    );
    // Inclusive hi keeps it.
    assert_eq!(
        pairs("SELECT a, b FROM t2 WHERE a <= 2", &params![]),
        [p(1, "a"), p(1, "b"), p(1, "zzz"), p(2, "a"), p(2, "m")]
    );
    assert_eq!(
        pairs("SELECT a, b FROM t2 WHERE a > 1 AND a < 3", &params![]),
        [p(2, "a"), p(2, "m")]
    );

    // Composite PK point.
    assert_eq!(
        rows(db.query("SELECT val FROM t2 WHERE a = 1 AND b = 'b'", &params![]).unwrap()),
        vec![vec![Value::Float(1.5)]]
    );
    // Range DML honors the same bounds.
    assert_eq!(
        affected(db.query("DELETE FROM t2 WHERE a > 1 AND a <= 2", &params![]).unwrap()),
        2
    );
    assert_eq!(
        pairs("SELECT a, b FROM t2", &params![]),
        [p(1, "a"), p(1, "b"), p(1, "zzz"), p(3, "x")]
    );

    db.verify().unwrap();
}

#[test]
fn null_range_parameter_matches_nothing() {
    let (cfg, _g) = test_config("nullrange", 8);
    let db = Database::open_with_config(cfg).unwrap();
    seed_users(&db);

    // `id > NULL` is UNKNOWN for every row: empty result, not a full scan.
    let rng = db.prepare("SELECT id FROM users WHERE id > $1").unwrap();
    assert!(ids(db.execute(&rng, &params![Value::Null]).unwrap()).is_empty());
    // Same for DML.
    let del = db.prepare("DELETE FROM users WHERE id < $1").unwrap();
    assert_eq!(affected(db.execute(&del, &params![Value::Null]).unwrap()), 0);
    // PK point with NULL: no match (never an error).
    let sel = db.prepare("SELECT id FROM users WHERE id = $1").unwrap();
    assert!(ids(db.execute(&sel, &params![Value::Null]).unwrap()).is_empty());
    // Index point with NULL: UNIQUE columns may hold NULLs but `= NULL` never
    // matches them.
    let by_email = db.prepare("SELECT id FROM users WHERE email = $1").unwrap();
    assert!(ids(db.execute(&by_email, &params![Value::Null]).unwrap()).is_empty());

    assert_eq!(ids(db.query("SELECT id FROM users", &params![]).unwrap()), [1, 2, 3, 4]);
    db.verify().unwrap();
}

#[test]
fn transaction_control_statements_are_rejected() {
    let (cfg, _g) = test_config("txnctl", 8);
    let db = Database::open_with_config(cfg).unwrap();

    let hb = db.prepare("BEGIN").unwrap();
    assert!(matches!(db.execute(&hb, &params![]), Err(Error::Unsupported(_))));
    assert!(matches!(
        db.query("COMMIT", &params![]),
        Err(Error::Unsupported(_))
    ));

    let mut s = db.begin().unwrap();
    assert!(matches!(s.execute(&hb, &params![]), Err(Error::Unsupported(_))));
    assert!(matches!(
        s.query("ROLLBACK", &params![]),
        Err(Error::Unsupported(_))
    ));
    s.rollback();
    db.verify().unwrap();
}

#[test]
fn open_from_toml_file_on_disk() {
    let (cfg, _g) = test_config("openfile", 8);
    // Write the same config out as a file and open through Database::open.
    let toml_path = _g.0.with_extension("toml");
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 64

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

  [[table.column]]
  name = "age"
  type = "int64"
  check = "age >= 0 AND age < 200"

  [[table.column]]
  name = "created"
  type = "timestamp"
  default = "now()"

[[table]]
name = "t2"
primary_key = ["a", "b"]

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"

  [[table.column]]
  name = "val"
  type = "float64"
"#,
        cfg.options.path.display()
    );
    std::fs::write(&toml_path, toml).unwrap();
    let db = Database::open(&toml_path).unwrap();
    assert_eq!(db.schema().tables.len(), 2);
    db.query("INSERT INTO users (id, email) VALUES (1, 'a@x.no')", &params![])
        .unwrap();
    assert_eq!(ids(db.query("SELECT id FROM users", &params![]).unwrap()), [1]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&toml_path);
}

#[test]
fn params_macro_conversions() {
    let v = params![1i64, 2i32, "text", String::from("owned"), 3.5f64, true,
        vec![1u8, 2], &b"raw"[..], Value::Null, Some(7i64), None::<i64>];
    assert_eq!(
        v,
        vec![
            Value::Int(1),
            Value::Int(2),
            Value::Text("text".into()),
            Value::Text("owned".into()),
            Value::Float(3.5),
            Value::Bool(true),
            Value::Blob(vec![1, 2]),
            Value::Blob(b"raw".to_vec()),
            Value::Null,
            Value::Int(7),
            Value::Null,
        ]
    );
    let empty = params![];
    assert!(empty.is_empty());
}

#[test]
fn group_commit_under_thread_contention() {
    // durability=commit is what routes contended writes through the intent
    // ring (group commit); on tmpfs the msyncs are cheap enough for a test.
    let (mut cfg, _g) = test_config("groupcommit", 16);
    cfg.options.durability = mpedb::Durability::Commit;
    let db = std::sync::Arc::new(Database::open_with_config(cfg).unwrap());
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();

    // 8 threads × 50 distinct autocommit inserts: under contention these ride
    // the intent ring and group-commit; every row must land exactly once.
    let mut handles = Vec::new();
    for t in 0..8i64 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50i64 {
                let id = t * 1000 + i;
                let r = db
                    .execute(&ins, &params![id, format!("u{id}@x.no"), 30])
                    .unwrap();
                assert!(matches!(r, ExecResult::Affected(1)), "got {r:?} for id {id}");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let ExecResult::Rows { rows, .. } = db
        .query("SELECT id FROM users", &params![])
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.len(), 400, "every contended insert must land exactly once");
    db.verify().unwrap();

    // per-intent errors keep their precision through the ring: race duplicate
    // ids from many threads — exactly one winner per id, losers get
    // PrimaryKeyViolation, and the batch commits around them.
    let mut handles = Vec::new();
    let wins = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    for _ in 0..8 {
        let db = db.clone();
        let wins = wins.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..40i64 {
                let id = 90_000 + i;
                match db.execute(&ins, &params![id, format!("dup{id}@x.no"), 1]) {
                    Ok(_) => {
                        wins.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(Error::PrimaryKeyViolation { .. })
                    | Err(Error::UniqueViolation { .. }) => {}
                    Err(e) => panic!("unexpected error through the ring: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        wins.load(std::sync::atomic::Ordering::Relaxed),
        40,
        "exactly one winner per contested id"
    );
    db.verify().unwrap();
}

#[test]
fn group_commit_under_thread_contention_wal_mode() {
    // durability=wal routes contended writes through the intent ring exactly
    // like commit mode (one record + one fdatasync per batch); every insert
    // must land exactly once and per-intent errors keep their precision.
    let (mut cfg, guard) = test_config("groupcommit-wal", 16);
    cfg.options.durability = mpedb::Durability::Wal;
    let wal = {
        let mut os = cfg.options.path.as_os_str().to_owned();
        os.push("-wal");
        PathBuf::from(os)
    };
    let db = std::sync::Arc::new(Database::open_with_config(cfg).unwrap());
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();

    let mut handles = Vec::new();
    let wins = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    for t in 0..8i64 {
        let db = db.clone();
        let wins = wins.clone();
        handles.push(std::thread::spawn(move || {
            // 40 distinct ids + 10 contested ones per thread
            for i in 0..40i64 {
                let id = t * 1000 + i;
                let r = db
                    .execute(&ins, &params![id, format!("u{id}@x.no"), 30])
                    .unwrap();
                assert!(matches!(r, ExecResult::Affected(1)), "got {r:?} for id {id}");
            }
            for i in 0..10i64 {
                let id = 90_000 + i;
                match db.execute(&ins, &params![id, format!("dup{id}@x.no"), 1]) {
                    Ok(_) => {
                        wins.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(Error::PrimaryKeyViolation { .. })
                    | Err(Error::UniqueViolation { .. }) => {}
                    Err(e) => panic!("unexpected error through the ring: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        ids(db.query("SELECT id FROM users WHERE id < 90000", &params![]).unwrap()).len(),
        320,
        "every contended insert must land exactly once"
    );
    assert_eq!(wins.load(std::sync::atomic::Ordering::Relaxed), 10);
    db.verify().unwrap();
    drop(db);
    drop(guard);
    let _ = std::fs::remove_file(&wal);
}

#[test]
fn streaming_topk_matches_full_sort() {
    // ORDER BY … LIMIT must return exactly the full-sort prefix (the bounded
    // top-K heap and the reference sort must agree), including DESC, NULLS
    // FIRST/LAST, ties, and OFFSET.
    let (cfg, _g) = test_config("topk", 16);
    let db = Database::open_with_config(cfg).unwrap();
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    // ages with duplicates and NULLs; ids are the tiebreaker via scan order.
    let ages = [50i64, 20, 50, 90, 20, 70, 50, 10, 90, 30];
    {
        let mut s = db.begin().unwrap();
        for (i, a) in ages.iter().enumerate() {
            let id = i as i64;
            if id % 4 == 0 {
                // NULL age on every 4th row
                s.query(
                    &format!("INSERT INTO users (id, email) VALUES ({id}, 'u{id}@x.no')"),
                    &params![],
                )
                .unwrap();
            } else {
                s.execute(&ins, &params![id, format!("u{id}@x.no"), *a]).unwrap();
            }
        }
        s.commit().unwrap();
    }

    // Reference: full result, sorted client-side the same way, then sliced.
    let full = rows(db.query("SELECT id, age FROM users", &params![]).unwrap());
    let reference = |desc: bool, limit: usize, offset: usize| -> Vec<Vec<Value>> {
        let mut v = full.clone();
        v.sort_by(|a, b| {
            let cmp = match (&a[1], &b[1]) {
                (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
                (Value::Null, _) => std::cmp::Ordering::Less, // NULLS FIRST asc
                (_, Value::Null) => std::cmp::Ordering::Greater,
                (Value::Int(x), Value::Int(y)) => x.cmp(y),
                _ => std::cmp::Ordering::Equal,
            };
            let cmp = if desc { cmp.reverse() } else { cmp };
            // ties broken by scan order (id ascending) — stable sort preserves it
            cmp
        });
        v.into_iter().skip(offset).take(limit).collect()
    };

    for (desc, limit, offset) in [
        (false, 3, 0),
        (false, 5, 2),
        (true, 4, 0),
        (true, 3, 5),
        (false, 100, 0), // limit beyond end
        (false, 1, 9),
        (true, 2, 8),
    ] {
        let dir = if desc { "DESC" } else { "ASC" };
        let sql = format!("SELECT id, age FROM users ORDER BY age {dir} LIMIT {limit} OFFSET {offset}");
        let got = rows(db.query(&sql, &params![]).unwrap());
        assert_eq!(
            got,
            reference(desc, limit, offset),
            "mismatch for {sql}"
        );
    }
    db.verify().unwrap();
}
