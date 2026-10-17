//! End-to-end tests: procedures against a real shared-memory database file.

use mpedb::{params, Config, Database, Error, ExecResult, Value};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn test_config(name: &str) -> (Config, FileGuard) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-proc-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 64

[[table]]
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false
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

fn seed_accounts(db: &Database, rows: &[(i64, i64)]) {
    for &(id, bal) in rows {
        db.query(
            "INSERT INTO accounts (id, balance) VALUES ($1, $2)",
            &params![id, bal],
        )
        .unwrap();
    }
}

fn balance(db: &Database, id: i64) -> i64 {
    match db
        .query(
            "SELECT balance FROM accounts WHERE id = $1",
            &params![id],
        )
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(v) => v,
            ref v => panic!("balance is {v:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }
}

fn int(v: &ProcValue) -> i64 {
    match v {
        ProcValue::Scalar(Value::Int(i)) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

/// transfer(src, dst, amount): applies both UPDATEs *first*, then checks the
/// resulting balance inside the same transaction (the proc must see its own
/// writes) and aborts via a runtime error when it went negative — proving
/// that an already-applied write is rolled back.
const TRANSFER_PY: &str = r#"
def transfer(src, dst, amount):
    db.execute("UPDATE accounts SET balance = balance - $2 WHERE id = $1", [src, amount])
    db.execute("UPDATE accounts SET balance = balance + $2 WHERE id = $1", [dst, amount])
    rows = db.query("SELECT balance FROM accounts WHERE id = $1", [src])
    if len(rows) == 0 or rows[0][0] < 0:
        return 1 // 0
    return rows[0][0]
"#;

const TRANSFER_RS: &str = r#"
fn transfer(src: i64, dst: i64, amount: i64) -> i64 {
    db.execute("UPDATE accounts SET balance = balance - $2 WHERE id = $1", &[src, amount]);
    db.execute("UPDATE accounts SET balance = balance + $2 WHERE id = $1", &[dst, amount]);
    let rows = db.query("SELECT balance FROM accounts WHERE id = $1", &[src]);
    if rows.len() == 0 || rows[0][0] < 0 {
        return 1 / 0;
    }
    rows[0][0]
}
"#;

#[test]
fn transfer_is_atomic_python() {
    let (cfg, _g) = test_config("transfer-py");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100), (2, 10)]);
    let engine = ProcEngine::new(&db);
    let h = engine.define(TRANSFER_PY, Lang::Python).unwrap();

    // Happy path: returns the new source balance.
    let v = engine.call("transfer", &params![1, 2, 30]).unwrap();
    assert_eq!(int(&v), 70);
    assert_eq!(balance(&db, 1), 70);
    assert_eq!(balance(&db, 2), 40);

    // Insufficient balance: the first UPDATE was applied inside the txn,
    // the error rolls the whole call back — nothing changed.
    let e = engine.call("transfer", &params![1, 2, 1000]).unwrap_err();
    assert!(matches!(e, Error::DivisionByZero), "{e}");
    assert_eq!(balance(&db, 1), 70);
    assert_eq!(balance(&db, 2), 40);

    // Call by hash works too.
    let v = engine.call(&h.to_string(), &params![2, 1, 5]).unwrap();
    assert_eq!(int(&v), 35);
    db.verify().unwrap();
}

#[test]
fn transfer_python_and_rust_behave_identically() {
    let (cfg, _g) = test_config("transfer-both");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100), (2, 100)]);
    let engine = ProcEngine::new(&db);
    let h_py = engine.define(TRANSFER_PY, Lang::Python).unwrap();
    // Same name in Rust would rebind it; give the Rust twin its own name.
    let rs_src = TRANSFER_RS.replace("fn transfer", "fn transfer_rs");
    let h_rs = engine.define(&rs_src, Lang::Rust).unwrap();
    assert_ne!(h_py, h_rs);

    let v = engine.call("transfer", &params![1, 2, 25]).unwrap();
    assert_eq!(int(&v), 75);
    let v = engine.call("transfer_rs", &params![2, 1, 25]).unwrap();
    assert_eq!(int(&v), 100); // 125 - 25
    assert_eq!(balance(&db, 1), 100);
    assert_eq!(balance(&db, 2), 100);

    // Both fail identically and atomically.
    for name in ["transfer", "transfer_rs"] {
        let e = engine.call(name, &params![1, 2, 500]).unwrap_err();
        assert!(matches!(e, Error::DivisionByZero), "{name}: {e}");
    }
    assert_eq!(balance(&db, 1), 100);
    assert_eq!(balance(&db, 2), 100);
}

#[test]
fn cross_handle_call_by_hash_only() {
    let (cfg, _g) = test_config("cross");
    // "Process A": define, then drop every handle.
    let hash = {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        seed_accounts(&db, &[(1, 50), (2, 0)]);
        let engine = ProcEngine::new(&db);
        engine.define(TRANSFER_PY, Lang::Python).unwrap()
    };
    // "Process B": a fresh Database instance that never saw the source.
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    let v = engine.call(&hash.to_string(), &params![1, 2, 20]).unwrap();
    assert_eq!(int(&v), 30);
    // …and by name.
    let v = engine.call("transfer", &params![1, 2, 10]).unwrap();
    assert_eq!(int(&v), 20);
    assert_eq!(balance(&db, 2), 30);

    // The catalog lists it.
    let procs = engine.list().unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].name, "transfer");
    assert_eq!(procs[0].hash, hash);
    assert_eq!(procs[0].argc, 3);
    assert!(procs[0].writes);
}

#[test]
fn define_is_idempotent_and_redefinition_rebinds_the_name() {
    let (cfg, _g) = test_config("idem");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 5)]);
    let engine = ProcEngine::new(&db);

    let v1 = "def get(a):\n    return db.query(\"SELECT balance FROM accounts WHERE id = $1\", [a])[0][0]\n";
    let h1 = engine.define(v1, Lang::Python).unwrap();
    let h1_again = engine.define(v1, Lang::Python).unwrap();
    assert_eq!(h1, h1_again);

    // New body, same name: the name follows the new definition…
    let v2 = "def get(a):\n    return db.query(\"SELECT balance FROM accounts WHERE id = $1\", [a])[0][0] + 1000\n";
    let h2 = engine.define(v2, Lang::Python).unwrap();
    assert_ne!(h1, h2);
    assert_eq!(int(&engine.call("get", &params![1]).unwrap()), 1005);
    // …while the old hash stays callable (content-addressed, immutable).
    assert_eq!(int(&engine.call(&h1.to_string(), &params![1]).unwrap()), 5);
}

#[test]
fn budget_exhaustion_commits_nothing_both_frontends() {
    let (cfg, _g) = test_config("budget");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100)]);
    let mut engine = ProcEngine::new(&db);
    engine.set_budget(50_000, 100, 1_000);

    // Write first, then spin: the write must not survive the budget kill.
    let py = "
def spin_py(a):
    db.execute(\"UPDATE accounts SET balance = 0 WHERE id = $1\", [a])
    while True:
        pass
";
    let rs = "
fn spin_rs(a: i64) {
    db.execute(\"UPDATE accounts SET balance = 0 WHERE id = $1\", &[a]);
    while true { }
}
";
    engine.define(py, Lang::Python).unwrap();
    engine.define(rs, Lang::Rust).unwrap();
    for name in ["spin_py", "spin_rs"] {
        let e = engine.call(name, &params![1]).unwrap_err();
        assert!(
            e.to_string().contains("instruction budget"),
            "{name}: {e}"
        );
        assert_eq!(balance(&db, 1), 100, "{name} leaked a write");
    }
    // Pure read-only spin dies cleanly too.
    engine
        .define("def spin_ro():\n    while True:\n        pass", Lang::Python)
        .unwrap();
    let e = engine.call("spin_ro", &params![]).unwrap_err();
    assert!(e.to_string().contains("instruction budget"), "{e}");
    db.verify().unwrap();
}

#[test]
fn wrong_arity_unknown_name_and_bad_args() {
    let (cfg, _g) = test_config("errors");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define("def two(a, b):\n    return a", Lang::Python)
        .unwrap();
    assert!(matches!(
        engine.call("two", &params![1]),
        Err(Error::WrongParamCount {
            expected: 2,
            got: 1
        })
    ));
    let e = engine.call("nosuch", &params![]).unwrap_err();
    assert!(e.to_string().contains("unknown procedure"), "{e}");
    let e = engine.call(&"0".repeat(64), &params![]).unwrap_err();
    assert!(e.to_string().contains("unknown procedure"), "{e}");
}

#[test]
fn define_rejects_sql_mismatches_with_location() {
    let (cfg, _g) = test_config("sqlcheck");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);

    // DML through db.query
    let e = engine
        .define(
            "def f(a):\n    return db.query(\"DELETE FROM accounts WHERE id = $1\", [a])",
            Lang::Python,
        )
        .unwrap_err();
    assert!(e.to_string().contains("read-only SELECT"), "{e}");
    assert!(e.to_string().contains("line 2"), "{e}");

    // SELECT through db.execute
    let e = engine
        .define(
            "def f(a):\n    return db.execute(\"SELECT * FROM accounts WHERE id = $1\", [a])",
            Lang::Python,
        )
        .unwrap_err();
    assert!(e.to_string().contains("requires DML"), "{e}");

    // Transaction control inside a proc
    let e = engine
        .define("def f():\n    db.execute(\"COMMIT\")\n    return 1", Lang::Python)
        .unwrap_err();
    assert!(e.to_string().contains("already is one transaction"), "{e}");

    // Bad SQL: located compile error
    let e = engine
        .define(
            "def f():\n    return db.query(\"SELECT nope FROM missing\")",
            Lang::Python,
        )
        .unwrap_err();
    assert!(e.to_string().contains("embedded SQL failed to compile"), "{e}");

    // Parameter-count mismatch between SQL and the argument list
    let e = engine
        .define(
            "def f(a):\n    return db.query(\"SELECT * FROM accounts WHERE id = $1 AND balance > $2\", [a])",
            Lang::Python,
        )
        .unwrap_err();
    assert!(e.to_string().contains("2 parameter(s) but 1"), "{e}");
}

#[test]
fn read_only_procs_run_without_the_writer_lock() {
    let (cfg, _g) = test_config("rolock");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 42)]);
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "def get(a):\n    return db.query(\"SELECT balance FROM accounts WHERE id = $1\", [a])[0][0]",
            Lang::Python,
        )
        .unwrap();
    engine
        .define(
            "def wipe(a):\n    return db.execute(\"UPDATE accounts SET balance = 0 WHERE id = $1\", [a])",
            Lang::Python,
        )
        .unwrap();

    // Hold the single writer lock on this thread…
    let session = db.begin().unwrap();
    // …a read-only proc still runs (it never touches the writer lock)…
    let v = engine.call("get", &params![1]).unwrap();
    assert_eq!(int(&v), 42);
    // …while a writing proc errors instead of deadlocking (ERRORCHECK).
    assert!(engine.call("wipe", &params![1]).is_err());
    drop(session);
    // Lock released: the writing proc works again.
    assert_eq!(int(&engine.call("wipe", &params![1]).unwrap()), 1);
}

#[test]
fn returning_rows_yields_lists_of_tuples() {
    let (cfg, _g) = test_config("rows");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 10), (2, 20)]);
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "def all_rows():\n    return db.query(\"SELECT id, balance FROM accounts\")",
            Lang::Python,
        )
        .unwrap();
    let v = engine.call("all_rows", &params![]).unwrap();
    let ProcValue::List(rows) = v else {
        panic!("expected a list, got {v:?}");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        ProcValue::Tuple(vec![
            ProcValue::Scalar(Value::Int(1)),
            ProcValue::Scalar(Value::Int(10)),
        ])
    );
}

/// A proc whose embedded plan was compiled against a schema that later
/// changed must surface `PlanInvalidated` (the §7.2 healing path), not act
/// on the stale plan. The registry record is forged through a raw engine
/// attachment, exactly like the facade's own corruption tests.
#[test]
fn stale_plan_surfaces_plan_invalidated() {
    use mpedb_types::{ColumnDef, ColumnType, Schema, TableDef};

    let (cfg, _g) = test_config("stale");
    let db = Database::open_with_config(cfg.clone()).unwrap();
    seed_accounts(&db, &[(1, 9)]);
    let engine = ProcEngine::new(&db);
    let sql = "SELECT balance FROM accounts WHERE id = $1";
    engine
        .define(
            &format!("def get(a):\n    return db.query(\"{sql}\", [a])[0][0]"),
            Lang::Python,
        )
        .unwrap();
    assert_eq!(int(&engine.call("get", &params![1]).unwrap()), 9);

    // The plan hash embedded in the proc (recomputable: same SQL, schema).
    let h = mpedb_sql::prepare(sql, &db.schema()).unwrap().hash();

    // A *different* schema whose shape still fits: same table, extra column.
    let col = |name: &str| ColumnDef { pg_enum: None, generated: None, default_text: None, decl: None,
        name: name.into(),
        ty: ColumnType::Int64,
        nullable: false,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: mpedb_types::Collation::Binary,
        affinity: mpedb_types::Affinity::Integer,
    };
    let other_schema = Schema::new(vec![TableDef {
        id: 0,
        name: "accounts".into(),
        columns: vec![col("id"), col("balance"), col("extra")],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        implicit_rowid: false, autoincrement: false,
        kind: mpedb_types::TableKind::Standard,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    let foreign = mpedb_sql::prepare(sql, &other_schema).unwrap();

    // Forge the registry record under the proc's plan hash: the facade
    // exposes no raw registry writes, so attach a second engine directly.
    // Layout: sql_len ‖ sql ‖ blob_len ‖ blob ‖ dialect ‖ last_used. The
    // dialect byte must be the one THIS process compiles under (0 = sqlite,
    // the default), or the load path refuses on the dialect before it ever
    // reaches the schema check this test is about.
    let blob = foreign.encode();
    let mut rec = Vec::new();
    rec.extend_from_slice(&(sql.len() as u32).to_le_bytes());
    rec.extend_from_slice(sql.as_bytes());
    rec.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    rec.extend_from_slice(&blob);
    rec.push(0);
    rec.extend_from_slice(&1u64.to_le_bytes());
    let mut key = b"plan/".to_vec();
    key.extend_from_slice(&h.0);
    let raw = mpedb_core::Engine::open(&cfg, vec![vec![None, None]]).unwrap();
    let mut w = raw.begin_write().unwrap();
    w.sys_put(&key, &rec).unwrap();
    w.commit().unwrap();

    // A fresh handle (cold plan cache, cold proc cache — this exercises the
    // name→blob→decode load path) must hit the forged record.
    let db2 = Database::open_with_config(cfg).unwrap();
    let engine2 = ProcEngine::new(&db2);
    assert!(matches!(
        engine2.call("get", &params![1]),
        Err(Error::PlanInvalidated)
    ));
}

// ------------------------------------------------------------------ cursors

/// Seed `n` accounts with balance = 2*id, in multi-row batches (autocommit
/// per statement; the row count deliberately spans several of the facade
/// stream's internal refill batches).
fn seed_many(db: &Database, n: i64) {
    let mut id = 0;
    while id < n {
        let hi = (id + 100).min(n);
        let values: Vec<String> = (id..hi).map(|i| format!("({i}, {})", 2 * i)).collect();
        db.query(
            &format!(
                "INSERT INTO accounts (id, balance) VALUES {}",
                values.join(", ")
            ),
            &params![],
        )
        .unwrap();
        id = hi;
    }
}

/// End-to-end streaming: Python `for row in db.rows(...)` and the Rust
/// while-cursor form run real engine scans through `Database::stream_query`
/// — multiple refill batches, a parameterized range predicate — and must
/// agree with the materialized `db.query` aggregate.
#[test]
fn cursor_procs_stream_real_scans() {
    let (cfg, _g) = test_config("cursor-e2e");
    let db = Database::open_with_config(cfg).unwrap();
    seed_many(&db, 600);
    let engine = ProcEngine::new(&db);

    engine
        .define(
            "
def sum_py(lo):
    s = 0
    for row in db.rows(\"SELECT id, balance FROM accounts WHERE id >= $1\", [lo]):
        s = s + row[1]
    return s
",
            Lang::Python,
        )
        .unwrap();
    engine
        .define(
            "
fn sum_rs(lo: i64) -> i64 {
    let mut s = 0;
    let c = db.rows(\"SELECT id, balance FROM accounts WHERE id >= $1\", &[lo]);
    while db.cursor_next(c) {
        s += db.cursor_col(c, 1);
    }
    s
}
",
            Lang::Rust,
        )
        .unwrap();

    for lo in [0i64, 100, 599, 600] {
        let want: i64 = (lo..600).map(|i| 2 * i).sum();
        assert_eq!(int(&engine.call("sum_py", &params![lo]).unwrap()), want);
        assert_eq!(int(&engine.call("sum_rs", &params![lo]).unwrap()), want);
    }
    db.verify().unwrap();
}

/// Sorted and LIMIT/OFFSET plans keep exact `db.query` semantics through a
/// cursor (the stream materializes internally for ORDER BY — documented —
/// and pushes LIMIT down for plain scans).
#[test]
fn cursor_procs_respect_order_by_and_limit() {
    let (cfg, _g) = test_config("cursor-orderby");
    let db = Database::open_with_config(cfg).unwrap();
    seed_many(&db, 600);
    let engine = ProcEngine::new(&db);

    engine
        .define(
            "
def first_desc():
    for row in db.rows(\"SELECT id FROM accounts ORDER BY id DESC LIMIT 3 OFFSET 1\"):
        return row[0]
    return -1
",
            Lang::Python,
        )
        .unwrap();
    assert_eq!(int(&engine.call("first_desc", &params![]).unwrap()), 598);

    engine
        .define(
            "
def count_limited(lo):
    n = 0
    for row in db.rows(\"SELECT id FROM accounts WHERE id >= $1 LIMIT 5\", [lo]):
        n = n + 1
    return n
",
            Lang::Python,
        )
        .unwrap();
    assert_eq!(int(&engine.call("count_limited", &params![10]).unwrap()), 5);
    assert_eq!(int(&engine.call("count_limited", &params![598]).unwrap()), 2);
    db.verify().unwrap();
}

/// The v1 rule end-to-end: a procedure that both writes and opens a cursor
/// is rejected at define time with a located error naming the rule.
#[test]
fn cursor_in_write_proc_is_rejected_at_define() {
    let (cfg, _g) = test_config("cursor-write");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    let e = engine
        .define(
            "
def bad(a):
    db.execute(\"DELETE FROM accounts WHERE id = $1\", [a])
    s = 0
    for row in db.rows(\"SELECT id FROM accounts\"):
        s = s + row[0]
    return s
",
            Lang::Python,
        )
        .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("read-only") && msg.contains("line"),
        "expected the located v1-rule error, got: {msg}"
    );
}

/// The row budget is enforced against real scans, and a budget kill leaves
/// the database untouched and verifiable.
#[test]
fn cursor_row_budget_e2e() {
    let (cfg, _g) = test_config("cursor-budget");
    let db = Database::open_with_config(cfg).unwrap();
    seed_many(&db, 600);
    let mut engine = ProcEngine::new(&db);
    engine.set_budget(1_000_000, 10_000, 100);

    engine
        .define(
            "
def scan_all():
    n = 0
    for row in db.rows(\"SELECT id FROM accounts\"):
        n = n + 1
    return n
",
            Lang::Python,
        )
        .unwrap();
    let e = engine.call("scan_all", &params![]).unwrap_err();
    assert!(e.to_string().contains("row budget"), "{e}");
    // Raising the budget makes the same proc complete.
    engine.set_budget(1_000_000, 10_000, 601);
    assert_eq!(int(&engine.call("scan_all", &params![]).unwrap()), 600);
    db.verify().unwrap();
}

/// Streaming keeps full SELECT semantics: expression projections evaluate
/// per pulled row, PK-point plans work through a cursor (internally
/// materialized — at most one row), and a NULL range bound yields a stream
/// that is born exhausted (SQL UNKNOWN, not an error).
#[test]
fn cursor_procs_cover_projection_point_and_null_bounds() {
    let (cfg, _g) = test_config("cursor-edges");
    let db = Database::open_with_config(cfg).unwrap();
    seed_many(&db, 10);
    let engine = ProcEngine::new(&db);

    engine
        .define(
            "
def edges(lo):
    s = 0
    for row in db.rows(\"SELECT id, balance + 1 FROM accounts WHERE id >= $1\", [lo]):
        s = s + row[1]
    for row in db.rows(\"SELECT balance FROM accounts WHERE id = $1\", [3]):
        s = s + row[0] * 1000
    return s
",
            Lang::Python,
        )
        .unwrap();
    // sum(2i + 1 for i in 5..10) = 2*(5+..+9) + 5 = 75; + balance(3)=6 * 1000.
    assert_eq!(int(&engine.call("edges", &params![5]).unwrap()), 6075);
    // NULL lower bound: the range predicate is UNKNOWN everywhere -> only
    // the point-lookup contribution remains.
    assert_eq!(
        int(&engine.call("edges", &params![Value::Null]).unwrap()),
        6000
    );
    db.verify().unwrap();
}

#[test]
fn plsql_routine_is_persisted_validated_and_callable_after_reopen() {
    let (cfg, _guard) = test_config("plsql-persist");
    let db = Database::open_with_config(cfg.clone()).unwrap();
    let source = "CREATE OR REPLACE FUNCTION choose_value(p NUMBER, yes BOOLEAN) RETURN NUMBER IS BEGIN IF yes THEN RETURN p; ELSE RETURN 9007199254740992.125; END IF; END choose_value;";
    let engine = ProcEngine::new(&db);
    let hash = engine.define(source, Lang::PlSql).unwrap();
    assert_eq!(engine.info(&hash.to_string()).unwrap().name, "CHOOSE_VALUE");
    assert_eq!(engine.call("CHOOSE_VALUE", &[Value::Int(42), Value::Bool(true)]).unwrap(), ProcValue::Scalar(Value::Numeric("42".into())));
    assert_eq!(engine.define(source, Lang::PlSql).unwrap(), hash);
    drop(engine);
    drop(db);
    let reopened = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&reopened);
    assert_eq!(engine.call(&hash.to_string(), &[Value::Null, Value::Null]).unwrap(), ProcValue::Scalar(Value::Numeric("9007199254740992.125".into())));
    assert!(engine.call("CHOOSE_VALUE", &[Value::Int(1), Value::Int(1)]).is_err());
}

#[test]
fn plsql_out_parameters_survive_storage_and_copy_out_only_on_success() {
    let (cfg, _guard) = test_config("plsql-out");
    let db = Database::open_with_config(cfg.clone()).unwrap();
    let source = "CREATE PROCEDURE choose_out(input NUMBER, result OUT NUMBER, flag IN OUT BOOLEAN) IS BEGIN result := input; flag := NOT flag; END;";
    let hash = ProcEngine::new(&db).define(source, Lang::PlSql).unwrap();
    drop(db);
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    assert_eq!(engine.call(&hash.to_string(), &[Value::Int(42), Value::Null, Value::Bool(false)]).unwrap(), ProcValue::Tuple(vec![
        ProcValue::Scalar(Value::Numeric("42".into())),
        ProcValue::Scalar(Value::Numeric("42".into())),
        ProcValue::Scalar(Value::Bool(true)),
    ]));
    let args = [Value::Int(42), Value::Null, Value::Int(1)];
    assert!(engine.call(&hash.to_string(), &args).is_err());
    assert_eq!(args, [Value::Int(42), Value::Null, Value::Int(1)]);
}

#[test]
fn plsql_arithmetic_updates_in_out_values_without_binary_float_rounding() {
    let (cfg, _guard) = test_config("plsql-arithmetic");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    let source = "CREATE PROCEDURE adjust(amount IN OUT NUMBER, result OUT NUMBER) IS BEGIN amount := amount + 0.1 + 0.2; result := amount / 2; END;";
    let hash = engine.define(source, Lang::PlSql).unwrap();
    assert_eq!(engine.call(&hash.to_string(), &[Value::Numeric("9007199254740992.1".into()), Value::Null]).unwrap(), ProcValue::Tuple(vec![
        ProcValue::Scalar(Value::Numeric("9007199254740992.4".into())),
        ProcValue::Scalar(Value::Numeric("4503599627370496.2".into())),
    ]));
}

#[test]
fn plsql_literal_dynamic_dml_is_positional_persistent_and_atomic() {
    let (cfg, _guard) = test_config("plsql-dynamic-dml");
    let db=Database::open_with_config(cfg.clone()).unwrap();
    db.query("CREATE TABLE native_ledger (id NUMERIC PRIMARY KEY, amount NUMERIC, note TEXT)",&[]).unwrap();
    let source="CREATE PROCEDURE store_row(p_id NUMBER, p_amount NUMBER, p_note VARCHAR2) IS BEGIN EXECUTE IMMEDIATE q'[INSERT INTO native_ledger VALUES(:x,:x,:x)]' USING IN p_id,p_amount,p_note; END;";
    let engine=ProcEngine::new(&db);
    let hash=engine.define(source,Lang::PlSql).unwrap();
    assert!(engine.info(&hash.to_string()).unwrap().writes);
    engine.call("STORE_ROW",&[Value::Int(1),Value::Numeric("9007199254740992.125".into()),Value::Text("ø".into())]).unwrap();
    let expected=vec![vec![Value::Numeric("1".into()),Value::Numeric("9007199254740992.125".into()),Value::Text("ø".into())]];
    assert!(matches!(db.query("SELECT * FROM native_ledger",&[]).unwrap(),ExecResult::Rows{rows,..} if rows==expected));
    engine.define("CREATE PROCEDURE break_after_write IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount=0'; n:=1/0; END;",Lang::PlSql).unwrap();
    assert!(engine.call("BREAK_AFTER_WRITE",&[]).is_err());
    assert!(matches!(db.query("SELECT * FROM native_ledger",&[]).unwrap(),ExecResult::Rows{rows,..} if rows==expected),"failure must roll back the already executed DML");
    assert!(db.create_function(mpedb::spellfn::SpellLang::PlSql,"CREATE FUNCTION bad_scalar RETURN NUMBER IS BEGIN EXECUTE IMMEDIATE 'DELETE FROM native_ledger'; RETURN 1; END;").unwrap_err().to_string().contains("cannot run SQL"));
    drop(engine);drop(db);
    let db=Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db).call(&hash.to_string(),&[Value::Int(2),Value::Null,Value::Null]).unwrap();
    assert!(matches!(db.query("SELECT amount,note FROM native_ledger WHERE id=$1",&[Value::Numeric("2".into())]).unwrap(),ExecResult::Rows{rows,..} if rows==vec![vec![Value::Null,Value::Null]]));
}

#[test]
fn plsql_dynamic_dml_rejects_unimplemented_forms_before_publication() {
    let (cfg, _guard)=test_config("plsql-dynamic-refusals");
    let db=Database::open_with_config(cfg).unwrap();
    let engine=ProcEngine::new(&db);
    for body in [
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=:x' USING NULL;",
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=:x' USING OUT n;",
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=:x' USING IN OUT n;",
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=:x';",
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=1' USING n;",
        "EXECUTE IMMEDIATE sql_text;",
        "EXECUTE IMMEDIATE 'SELECT balance FROM accounts' BULK COLLECT INTO n;",
        "EXECUTE IMMEDIATE 'CREATE TABLE x(id NUMBER)';",
        "EXECUTE IMMEDIATE 'UPDATE accounts SET balance=1; DELETE FROM accounts';",
    ] {
        let source=format!("CREATE PROCEDURE refused IS n NUMBER; sql_text VARCHAR2(100); BEGIN {body} END;");
        assert!(engine.define(&source,Lang::PlSql).is_err(),"{body}");
        assert!(engine.info("REFUSED").is_err(),"a rejected routine must not publish");
    }
}

#[test]
fn plsql_dynamic_select_into_checks_cardinality_types_and_transaction_visibility() {
    let (cfg,_guard)=test_config("plsql-select-into");
    let db=Database::open_with_config(cfg).unwrap();seed_accounts(&db,&[(1,50),(2,20)]);
    let engine=ProcEngine::new(&db);
    engine.define("CREATE FUNCTION get_balance(p PLS_INTEGER) RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT balance FROM accounts WHERE id=:key' INTO n USING p; RETURN n; END;",Lang::PlSql).unwrap();
    assert_eq!(engine.call("GET_BALANCE",&[Value::Int(1)]).unwrap(),ProcValue::Scalar(Value::Numeric("50".into())));
    assert!(matches!(engine.call("GET_BALANCE",&[Value::Int(99)]),Err(Error::Oracle{code:1403,..})));
    engine.define("CREATE FUNCTION adjust_read(p PLS_INTEGER,delta PLS_INTEGER) RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE accounts SET balance=balance+:x WHERE id=:x' USING delta,p; EXECUTE IMMEDIATE 'SELECT balance FROM accounts WHERE id=:x' INTO n USING p; RETURN n; END;",Lang::PlSql).unwrap();
    assert_eq!(engine.call("ADJUST_READ",&[Value::Int(1),Value::Int(10)]).unwrap(),ProcValue::Scalar(Value::Numeric("60".into())));
    engine.define("CREATE FUNCTION too_many RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE accounts SET balance=0'; EXECUTE IMMEDIATE 'SELECT balance FROM accounts' INTO n; RETURN n; END;",Lang::PlSql).unwrap();
    assert!(matches!(engine.call("TOO_MANY",&[]),Err(Error::Oracle{code:1422,..})));
    assert_eq!(balance(&db,1),60);assert_eq!(balance(&db,2),20);
    engine.define("CREATE PROCEDURE fetch_pair(a OUT NUMBER,b OUT NUMBER) IS BEGIN EXECUTE IMMEDIATE 'SELECT balance,id FROM accounts WHERE id=1' INTO a,b; END;",Lang::PlSql).unwrap();
    assert_eq!(engine.call("FETCH_PAIR",&[Value::Null,Value::Null]).unwrap(),ProcValue::Tuple(vec![ProcValue::Scalar(Value::Numeric("60".into())),ProcValue::Scalar(Value::Numeric("1".into()))]));
    for source in [
        "CREATE PROCEDURE bad(p IN NUMBER) IS BEGIN EXECUTE IMMEDIATE 'SELECT 1' INTO p; END;",
        "CREATE PROCEDURE bad IS n NUMBER; m NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT 1' INTO n,m; END;",
        "CREATE PROCEDURE bad IS n CONSTANT NUMBER:=0; BEGIN EXECUTE IMMEDIATE 'SELECT 1' INTO n; END;",
    ] {assert!(engine.define(source,Lang::PlSql).is_err(),"{source}");}
}

#[test]
fn plsql_caught_statement_failure_rolls_back_only_that_statement() {
    let (cfg,_guard)=test_config("plsql-caught-statement");
    let db=Database::open_with_config(cfg).unwrap();seed_accounts(&db,&[(1,50),(2,20)]);
    let engine=ProcEngine::new(&db);
    engine.define("CREATE PROCEDURE handle_duplicate IS BEGIN EXECUTE IMMEDIATE 'UPDATE accounts SET balance=balance+10 WHERE id=1'; BEGIN EXECUTE IMMEDIATE 'INSERT INTO accounts(id,balance) VALUES(3,30),(1,999)'; EXCEPTION WHEN DUP_VAL_ON_INDEX THEN NULL; END; EXECUTE IMMEDIATE 'UPDATE accounts SET balance=balance+1 WHERE id=2'; END;",Lang::PlSql).unwrap();
    engine.call("HANDLE_DUPLICATE",&[]).unwrap();
    assert_eq!(balance(&db,1),60);assert_eq!(balance(&db,2),21);
    assert!(matches!(db.query("SELECT id FROM accounts WHERE id=3",&[]).unwrap(),ExecResult::Rows{rows,..} if rows.is_empty()),"first row of the failed multi-row INSERT must also roll back");
    engine.define("CREATE FUNCTION missing_default(p PLS_INTEGER) RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT balance FROM accounts WHERE id=:p' INTO n USING p; RETURN n; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 123; END;",Lang::PlSql).unwrap();
    assert_eq!(engine.call("MISSING_DEFAULT",&[Value::Int(99)]).unwrap(),ProcValue::Scalar(Value::Numeric("123".into())));
    engine.define("CREATE PROCEDURE rethrow_duplicate IS BEGIN BEGIN EXECUTE IMMEDIATE 'INSERT INTO accounts VALUES(4,40),(1,0)'; EXCEPTION WHEN DUP_VAL_ON_INDEX THEN RAISE; END; END;",Lang::PlSql).unwrap();
    assert!(matches!(engine.call("RETHROW_DUPLICATE",&[]),Err(Error::Oracle{code:1,..})));
    assert!(matches!(db.query("SELECT id FROM accounts WHERE id=4",&[]).unwrap(),ExecResult::Rows{rows,..} if rows.is_empty()));
}

#[test]
fn plsql_dynamic_returning_assigns_null_and_rolls_back_the_statement() {
    let (cfg, _guard) = test_config("plsql-dynamic-returning");
    let db = Database::open_with_config(cfg).unwrap();
    db.query(
        "CREATE TABLE native_ledger (id INTEGER PRIMARY KEY, amount INTEGER, note TEXT)",
        &[],
    )
    .unwrap();
    db.query(
        "INSERT INTO native_ledger VALUES (1, 7, 'seed'), (2, 8, NULL)",
        &[],
    )
    .unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "CREATE FUNCTION take_note(p_id PLS_INTEGER) RETURN VARCHAR2 IS v VARCHAR2(20) := 'old'; incoming VARCHAR2(20); BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET note = :1 WHERE id = :2 RETURNING note INTO :3' USING incoming, p_id, OUT v; RETURN v; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("TAKE_NOTE", &[Value::Int(1)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    assert!(matches!(
        db.query("SELECT note FROM native_ledger WHERE id = 1", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Null]]
    ));
    engine
        .define(
            "CREATE FUNCTION take_amount(p_id PLS_INTEGER) RETURN NUMBER IS v NUMBER := 1; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = :1 WHERE id = :2 RETURNING amount INTO :3' USING 4, p_id, OUT v; RETURN v; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("TAKE_AMOUNT", &[Value::Int(1)]).unwrap(),
        ProcValue::Scalar(Value::Numeric("4".into()))
    );
    engine
        .define(
            "CREATE FUNCTION miss_keeps(p PLS_INTEGER) RETURN NUMBER IS v NUMBER := 7; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 5 WHERE id = 1'; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING p, OUT v; RETURN 0; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN v; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MISS_KEEPS", &[Value::Int(99)]).unwrap(),
        ProcValue::Scalar(Value::Numeric("7".into()))
    );
    assert!(matches!(
        db.query("SELECT amount FROM native_ledger WHERE id = 1", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Int(5)]]
    ));
    engine
        .define(
            "CREATE FUNCTION many_keeps RETURN NUMBER IS v NUMBER := 7; BEGIN EXECUTE IMMEDIATE 'INSERT INTO native_ledger VALUES (3, 3, ''keep'')'; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = amount + 1 WHERE id < 3 RETURNING amount INTO :1' USING OUT v; RETURN 0; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN v; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MANY_KEEPS", &[]).unwrap(),
        ProcValue::Scalar(Value::Numeric("7".into()))
    );
    assert!(matches!(
        db.query(
            "SELECT id, amount FROM native_ledger ORDER BY id",
            &[],
        )
        .unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![
            vec![Value::Int(1), Value::Int(5)],
            vec![Value::Int(2), Value::Int(8)],
            vec![Value::Int(3), Value::Int(3)],
        ]
    ));
}

#[test]
fn plsql_static_returning_assigns_null_and_rolls_back_the_statement() {
    let (cfg, _guard) = test_config("plsql-static-returning");
    let db = Database::open_with_config(cfg).unwrap();
    db.query(
        "CREATE TABLE native_ledger (id INTEGER PRIMARY KEY, amount INTEGER, note TEXT)",
        &[],
    )
    .unwrap();
    db.query(
        "INSERT INTO native_ledger VALUES (1, 7, 'seed'), (2, 8, NULL)",
        &[],
    )
    .unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "CREATE FUNCTION take_note(p_id PLS_INTEGER) RETURN VARCHAR2 IS v VARCHAR2(20) := 'old'; incoming VARCHAR2(20); BEGIN UPDATE native_ledger SET note = take_note.incoming WHERE id = take_note.p_id RETURNING note INTO v; IF SQL%FOUND AND NOT SQL%NOTFOUND AND SQL%ROWCOUNT = 1 THEN RETURN v; ELSE RETURN 'bad'; END IF; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("TAKE_NOTE", &[Value::Int(1)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    assert!(matches!(
        db.query("SELECT note FROM native_ledger WHERE id = 1", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Null]]
    ));
    engine
        .define(
            "CREATE FUNCTION take_amount(p_id PLS_INTEGER, p_amt PLS_INTEGER) RETURN NUMBER IS v NUMBER := 1; BEGIN UPDATE native_ledger SET amount = take_amount.p_amt WHERE id = take_amount.p_id RETURNING amount INTO v; RETURN v; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine
            .call("TAKE_AMOUNT", &[Value::Int(1), Value::Int(4)])
            .unwrap(),
        ProcValue::Scalar(Value::Numeric("4".into()))
    );
    engine
        .define(
            "CREATE FUNCTION miss_keeps(p PLS_INTEGER) RETURN NUMBER IS v NUMBER := 7; one PLS_INTEGER := 1; five PLS_INTEGER := 5; nine PLS_INTEGER := 9; BEGIN UPDATE native_ledger SET amount = miss_keeps.five WHERE id = miss_keeps.one; BEGIN UPDATE native_ledger SET amount = miss_keeps.nine WHERE id = miss_keeps.p RETURNING amount INTO v; RETURN 0; EXCEPTION WHEN NO_DATA_FOUND THEN IF SQL%NOTFOUND AND SQL%ROWCOUNT = 0 THEN RETURN v; ELSE RETURN -1; END IF; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MISS_KEEPS", &[Value::Int(99)]).unwrap(),
        ProcValue::Scalar(Value::Numeric("7".into()))
    );
    assert!(matches!(
        db.query("SELECT amount FROM native_ledger WHERE id = 1", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Int(5)]]
    ));
    engine
        .define(
            "CREATE FUNCTION many_keeps RETURN NUMBER IS v NUMBER := 7; next_id PLS_INTEGER := 3; next_amt PLS_INTEGER := 3; lim PLS_INTEGER := 3; one PLS_INTEGER := 1; BEGIN INSERT INTO native_ledger VALUES (many_keeps.next_id, many_keeps.next_amt, 'keep'); BEGIN UPDATE native_ledger SET amount = amount + many_keeps.one WHERE id < many_keeps.lim RETURNING amount INTO v; RETURN 0; EXCEPTION WHEN TOO_MANY_ROWS THEN IF SQL%FOUND AND NOT SQL%NOTFOUND AND SQL%ROWCOUNT = 1 THEN RETURN v; ELSE RETURN -1; END IF; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MANY_KEEPS", &[]).unwrap(),
        ProcValue::Scalar(Value::Numeric("7".into()))
    );
    assert!(matches!(
        db.query("SELECT id, amount, note FROM native_ledger ORDER BY id", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![
            vec![Value::Int(1), Value::Int(5), Value::Null],
            vec![Value::Int(2), Value::Int(8), Value::Null],
            vec![Value::Int(3), Value::Int(3), Value::Text("keep".into())],
        ]
    ));
}

#[test]
fn plsql_sql_attributes_see_a_missing_returning_row() {
    let (cfg, _guard) = test_config("plsql-sql-attr");
    let db = Database::open_with_config(cfg).unwrap();
    db.query(
        "CREATE TABLE native_ledger (id INTEGER PRIMARY KEY, amount INTEGER, note TEXT)",
        &[],
    )
    .unwrap();
    db.query(
        "INSERT INTO native_ledger VALUES (1, 7, 'seed'), (2, 8, NULL)",
        &[],
    )
    .unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "CREATE FUNCTION miss_rowcount(p PLS_INTEGER) RETURN PLS_INTEGER IS v NUMBER := 7; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 5 WHERE id = 1'; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING p, OUT v; RETURN -1; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%ROWCOUNT; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MISS_ROWCOUNT", &[Value::Int(99)]).unwrap(),
        ProcValue::Scalar(Value::Int(0))
    );
    assert!(matches!(
        db.query("SELECT amount FROM native_ledger WHERE id = 1", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Int(5)]]
    ));
    engine
        .define(
            "CREATE FUNCTION miss_notfound(p PLS_INTEGER) RETURN PLS_INTEGER IS v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING p, OUT v; RETURN 0; EXCEPTION WHEN NO_DATA_FOUND THEN IF SQL%NOTFOUND AND NOT SQL%FOUND AND NOT SQL%ISOPEN AND v = 7 THEN RETURN 1; ELSE RETURN 0; END IF; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MISS_NOTFOUND", &[Value::Int(99)]).unwrap(),
        ProcValue::Scalar(Value::Int(1))
    );
    engine
        .define(
            "CREATE FUNCTION many_rowcount RETURN PLS_INTEGER IS v NUMBER := 7; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 4'; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = amount + 1 WHERE id < 3 RETURNING amount INTO :1' USING OUT v; RETURN -1; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%ROWCOUNT; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MANY_ROWCOUNT", &[]).unwrap(),
        ProcValue::Scalar(Value::Int(1))
    );
    assert!(matches!(
        db.query("SELECT id, amount FROM native_ledger ORDER BY id", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![
            vec![Value::Int(1), Value::Int(4)],
            vec![Value::Int(2), Value::Int(4)],
        ]
    ));
    engine
        .define(
            "CREATE FUNCTION many_found RETURN PLS_INTEGER IS v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = amount + 1 WHERE id < 3 RETURNING amount INTO :1' USING OUT v; RETURN 0; EXCEPTION WHEN TOO_MANY_ROWS THEN IF SQL%FOUND AND NOT SQL%NOTFOUND AND NOT SQL%ISOPEN AND v = 7 THEN RETURN 1; ELSE RETURN 0; END IF; END; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("MANY_FOUND", &[]).unwrap(),
        ProcValue::Scalar(Value::Int(1))
    );
    assert!(matches!(
        db.query("SELECT id, amount FROM native_ledger ORDER BY id", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![
            vec![Value::Int(1), Value::Int(4)],
            vec![Value::Int(2), Value::Int(4)],
        ]
    ));
    engine
        .define(
            "CREATE FUNCTION hit_rowcount(p PLS_INTEGER) RETURN PLS_INTEGER IS v NUMBER := 1; BEGIN EXECUTE IMMEDIATE 'UPDATE native_ledger SET amount = 6 WHERE id = :1 RETURNING amount INTO :2' USING p, OUT v; RETURN SQL%ROWCOUNT; END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("HIT_ROWCOUNT", &[Value::Int(2)]).unwrap(),
        ProcValue::Scalar(Value::Int(1))
    );
    assert!(matches!(
        db.query("SELECT amount FROM native_ledger WHERE id = 2", &[]).unwrap(),
        ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Int(6)]]
    ));
}

#[test]
fn plsql_others_cannot_hide_engine_budgets_or_unsupported_execution() {
    let (cfg,_guard)=test_config("plsql-handler-budget");
    let db=Database::open_with_config(cfg).unwrap();seed_accounts(&db,&[(1,50)]);
    let mut engine=ProcEngine::new(&db);
    engine.define("CREATE FUNCTION spin_after_write RETURN NUMBER IS BEGIN EXECUTE IMMEDIATE 'UPDATE accounts SET balance=0'; LOOP NULL; END LOOP; EXCEPTION WHEN OTHERS THEN RETURN 999; END;",Lang::PlSql).unwrap();
    engine.set_budget(100,100,100);
    let error=engine.call("SPIN_AFTER_WRITE",&[]).unwrap_err().to_string();
    assert!(error.contains("instruction budget"),"{error}");
    assert_eq!(balance(&db,1),50);
}

#[test]
fn plsql_package_variable_starts_null_and_survives_the_session() {
    let (cfg, _guard) = test_config("plsql-package");
    let db = Database::open_with_config(cfg).unwrap();
    let source = r#"
CREATE OR REPLACE PACKAGE BODY ledger AS
  n PLS_INTEGER := NULL;
  FUNCTION touch(v PLS_INTEGER) RETURN PLS_INTEGER IS
    previous PLS_INTEGER;
  BEGIN
    previous := n;
    n := v;
    IF v IS NULL THEN
      RAISE NO_DATA_FOUND;
    END IF;
    RETURN previous;
  END touch;
END ledger;
"#;
    let engine = ProcEngine::new(&db);
    engine.define(source, Lang::PlSql).unwrap();
    assert_eq!(
        engine.call("LEDGER.TOUCH", &[Value::Int(4)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    assert_eq!(
        engine.call("LEDGER.TOUCH", &[Value::Int(9)]).unwrap(),
        ProcValue::Scalar(Value::Int(4))
    );
    let err = engine.call("LEDGER.TOUCH", &[Value::Null]).unwrap_err().to_string();
    assert!(err.contains("ORA-01403"), "{err}");
    assert_eq!(
        engine.call("LEDGER.TOUCH", &[Value::Int(1)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    let other = ProcEngine::new(&db);
    assert_eq!(
        other.call("LEDGER.TOUCH", &[Value::Int(2)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    assert_eq!(
        engine.call("LEDGER.TOUCH", &[Value::Int(3)]).unwrap(),
        ProcValue::Scalar(Value::Int(1))
    );
    engine.define(source, Lang::PlSql).unwrap();
    assert_eq!(
        engine.call("LEDGER.TOUCH", &[Value::Int(5)]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
}

#[test]
fn plsql_anchored_type_keeps_null_and_inherits_not_null() {
    let (cfg, _guard) = test_config("plsql-anchored-type");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "CREATE FUNCTION see_type(v VARCHAR2) RETURN VARCHAR2 IS \
             a VARCHAR2(4) := 'kept'; \
             n a%TYPE; \
             BEGIN \
               IF v IS NULL THEN \
                 IF n IS NULL THEN n := NULL; END IF; \
                 RETURN n; \
               END IF; \
               n := v; \
               RETURN n; \
             END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("SEE_TYPE", &[Value::Null]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    assert_eq!(
        engine.call("SEE_TYPE", &[Value::Text("ok".into())]).unwrap(),
        ProcValue::Scalar(Value::Text("ok".into()))
    );
    let err = engine
        .call("SEE_TYPE", &[Value::Text("toolong".into())])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("character string buffer too small"),
        "{err}"
    );
    let refused = engine
        .define(
            "CREATE FUNCTION bad RETURN VARCHAR2 IS \
             a VARCHAR2(4) NOT NULL := 'kept'; \
             n a%TYPE; \
             BEGIN RETURN n; END;",
            Lang::PlSql,
        )
        .unwrap_err()
        .to_string();
    assert!(refused.contains("inherits NOT NULL"), "{refused}");
}
