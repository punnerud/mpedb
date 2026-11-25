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
    let col = |name: &str| ColumnDef { generated: None, default_text: None, decl: None,
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

/// The Python frontend's sugar against a real database: a general `for`
/// over a materialized result inside a WRITE procedure (where the streaming
/// `db.rows` cursor is not allowed), `range`, and `sql.NAME(...)` linked at
/// define time like any embedded statement.
#[test]
fn python_general_for_and_sql_functions_run_against_a_real_database() {
    let (cfg, _guard) = test_config("pysugar");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100), (2, 250), (3, 40)]);
    let engine = ProcEngine::new(&db);

    engine
        .define(
            "
def add_interest(percent):
    changed = 0
    for row in db.query('SELECT id, balance FROM accounts ORDER BY id'):
        if row[1] < 50:
            continue
        db.execute('UPDATE accounts SET balance = $1 WHERE id = $2', [row[1] + row[1] * percent // 100, row[0]])
        changed += 1
    return changed
",
            Lang::Python,
        )
        .unwrap();
    assert_eq!(
        engine.call("add_interest", &[Value::Int(10)]).unwrap(),
        ProcValue::Scalar(Value::Int(2))
    );
    assert_eq!((balance(&db, 1), balance(&db, 2), balance(&db, 3)), (110, 275, 40));

    engine
        .define(
            "
def label(id):
    rows = db.query('SELECT balance FROM accounts WHERE id = $1', [id])
    size = 'none' if len(rows) == 0 else 'small' if rows[0][0] < 100 else 'large'
    stars = ''
    for i in range(3):
        stars = stars + '*'
    return sql.upper(size) + stars + sql.substr('abcdef', 2, 3)
",
            Lang::Python,
        )
        .unwrap();
    let text = |id: i64| engine.call("label", &[Value::Int(id)]).unwrap();
    assert_eq!(text(2), ProcValue::Scalar(Value::Text("LARGE***bcd".into())));
    assert_eq!(text(3), ProcValue::Scalar(Value::Text("SMALL***bcd".into())));
    assert_eq!(text(9), ProcValue::Scalar(Value::Text("NONE***bcd".into())));

    // A string literal reaches SQL as a literal, which `'now'` requires.
    engine
        .define("def today_len():\n    return sql.length(sql.date('now'))", Lang::Python)
        .unwrap();
    assert_eq!(
        engine.call("today_len", &[]).unwrap(),
        ProcValue::Scalar(Value::Int(10))
    );

    // An unknown SQL function is refused when the procedure is defined.
    let e = engine
        .define("def bad(a):\n    return sql.no_such_function(a)", Lang::Python)
        .unwrap_err();
    assert!(e.to_string().contains("no_such_function"), "{e}");
}

/// Built lists and dicts against a real database: a write procedure
/// collects what it changed and returns it as named fields.
#[test]
fn python_lists_and_dicts_run_against_a_real_database() {
    let (cfg, _guard) = test_config("pycontainers");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100), (2, 250), (3, 40)]);
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "
def raise_small(limit, by):
    changed = []
    skipped = {}
    for row in db.query('SELECT id, balance FROM accounts ORDER BY id'):
        if row[1] >= limit:
            skipped = {**skipped, sql.format('id%d', row[0]): row[1]}
            continue
        db.execute('UPDATE accounts SET balance = $1 WHERE id = $2', [row[1] + by, row[0]])
        changed = [*changed, {'id': row[0], 'before': row[1], 'after': row[1] + by}]
    return {'changed': changed, 'skipped': skipped, 'any': len(changed) > 0 and 'id2' in skipped}
",
            Lang::Python,
        )
        .unwrap();
    let scalar = |i: i64| ProcValue::Scalar(Value::Int(i));
    let row = |id: i64, before: i64, after: i64| {
        ProcValue::Map(vec![
            ("id".into(), scalar(id)),
            ("before".into(), scalar(before)),
            ("after".into(), scalar(after)),
        ])
    };
    assert_eq!(
        engine.call("raise_small", &[Value::Int(200), Value::Int(5)]).unwrap(),
        ProcValue::Map(vec![
            ("changed".into(), ProcValue::List(vec![row(1, 100, 105), row(3, 40, 45)])),
            ("skipped".into(), ProcValue::Map(vec![("id2".into(), scalar(250))])),
            ("any".into(), ProcValue::Scalar(Value::Bool(true))),
        ])
    );
    assert_eq!((balance(&db, 1), balance(&db, 2), balance(&db, 3)), (105, 250, 45));
}

/// Procedures calling procedures: bound by content hash when the caller is
/// defined, run inside the caller's transaction and budgets.
#[test]
fn python_procedures_call_each_other() {
    let (cfg, _guard) = test_config("pycalls");
    let db = Database::open_with_config(cfg).unwrap();
    seed_accounts(&db, &[(1, 100), (2, 10)]);
    let engine = ProcEngine::new(&db);
    let def = |src: &str| engine.define(src, Lang::Python);
    let int = |i: i64| ProcValue::Scalar(Value::Int(i));

    def("def bal(id):\n    rows = db.query('SELECT balance FROM accounts WHERE id = $1', [id])\n    return rows[0][0] if len(rows) == 1 else None").unwrap();
    def("def move(a, b, amount):\n    if bal(a) is None or bal(b) is None or bal(a) < amount:\n        return 1 // 0\n    db.execute('UPDATE accounts SET balance = balance - $1 WHERE id = $2', [amount, a])\n    db.execute('UPDATE accounts SET balance = balance + $1 WHERE id = $2', [amount, b])\n    return {'from': bal(a), 'to': bal(b)}").unwrap();
    def("def twice(a, b, amount):\n    first = move(a, b, amount)\n    second = move(a, b, amount)\n    return [first, second, describe({'n': 2, 'last': second})]").unwrap_err(); // describe is not defined yet
    def("def describe(d):\n    return sql.format('%d moves, %d left', d['n'], d['last']['from'])").unwrap();
    def("def twice(a, b, amount):\n    first = move(a, b, amount)\n    second = move(a, b, amount)\n    return [first, second, describe({'n': 2, 'last': second})]").unwrap();

    // A caller of a writer is itself a writer; a reader stays a reader.
    assert!(engine.info("twice").unwrap().writes);
    assert!(engine.info("move").unwrap().writes);
    assert!(!engine.info("describe").unwrap().writes);

    let pair = |from: i64, to: i64| ProcValue::Map(vec![("from".into(), int(from)), ("to".into(), int(to))]);
    assert_eq!(
        engine.call("twice", &[Value::Int(1), Value::Int(2), Value::Int(30)]).unwrap(),
        ProcValue::List(vec![
            pair(70, 40), // the callee saw the caller's uncommitted writes
            pair(40, 70),
            ProcValue::Scalar(Value::Text("2 moves, 40 left".into())),
        ])
    );
    assert_eq!((balance(&db, 1), balance(&db, 2)), (40, 70));

    // An error in a callee rolls back what the whole call did.
    assert!(engine.call("twice", &[Value::Int(1), Value::Int(2), Value::Int(25)]).is_err());
    assert_eq!((balance(&db, 1), balance(&db, 2)), (40, 70));

    // Redefining a callee does not change a caller until it is defined again.
    def("def describe(d):\n    return 'changed'").unwrap();
    let last = |v: ProcValue| match v {
        ProcValue::List(mut items) => items.pop().unwrap(),
        other => other,
    };
    assert_eq!(
        last(engine.call("twice", &[Value::Int(2), Value::Int(1), Value::Int(1)]).unwrap()),
        ProcValue::Scalar(Value::Text("2 moves, 68 left".into()))
    );
    def("def twice(a, b, amount):\n    first = move(a, b, amount)\n    second = move(a, b, amount)\n    return [first, second, describe({'n': 2, 'last': second})]").unwrap();
    assert_eq!(
        last(engine.call("twice", &[Value::Int(2), Value::Int(1), Value::Int(1)]).unwrap()),
        ProcValue::Scalar(Value::Text("changed".into()))
    );

    // What is refused when a procedure is defined.
    let refused = |src: &str, needle: &str| {
        let e = def(src).unwrap_err();
        assert!(e.to_string().contains(needle), "{src}: {e}");
    };
    refused("def a(x):\n    return missing(x)", "is not a defined procedure");
    refused("def a(x):\n    return bal(x, 1)", "takes 1 argument(s) but 2 were passed");
    refused("def a(x):\n    return a(x)", "cannot call itself");
    refused("def a(x):\n    return bal(id=x)", "keyword arguments");
    def("def scan():\n    n = 0\n    for r in db.rows('SELECT id FROM accounts'):\n        n += 1\n    return n").unwrap();
    refused("def a(x):\n    db.execute('DELETE FROM accounts WHERE id = $1', [x])\n    return scan()", "cursors are allowed only when nothing in the call writes");
    refused("def a(x):\n    return [scan(), move(1, 2, x)]", "cursors are allowed only when nothing in the call writes");
    // A reader may call the cursor procedure.
    def("def count_twice():\n    return scan() + scan()").unwrap();
    assert_eq!(engine.call("count_twice", &[]).unwrap(), int(4));

    // Cursors a called procedure opened do not pile up across calls, also
    // when it returns from inside its loop.
    def("def first_id():\n    for r in db.rows('SELECT id FROM accounts ORDER BY id'):\n        return r[0]\n    return None").unwrap();
    def("def many():\n    t = 0\n    for i in range(500):\n        t += scan() + first_id()\n    return t").unwrap();
    assert_eq!(engine.call("many", &[]).unwrap(), int(1500));
    // A cursor cannot leave the procedure that opened it.
    refused("def leak():\n    return db.rows('SELECT id FROM accounts')", "may only be used as the iterable");

    // The chain of calls has a depth limit, checked when defining.
    def("def d0(x):\n    return x + 1").unwrap();
    let max = mpedb_spell::interp::MAX_CALL_DEPTH;
    for level in 1..=max + 1 {
        let src = format!("def d{level}(x):\n    return d{}(x) + 1", level - 1);
        if level <= max {
            def(&src).unwrap();
        } else {
            refused(&src, "deep");
        }
    }
    assert_eq!(engine.call(&format!("d{max}"), &[Value::Int(0)]).unwrap(), int(max as i64 + 1));

    // The instruction budget is one budget for the whole call.
    def("def spin(n):\n    i = 0\n    while i < n:\n        i += 1\n    return i").unwrap();
    def("def spin_many(n):\n    t = 0\n    for i in range(100):\n        t += spin(n)\n    return t").unwrap();
    assert_eq!(engine.call("spin_many", &[Value::Int(10)]).unwrap(), int(1000));
    let e = engine.call("spin_many", &[Value::Int(100_000)]).unwrap_err();
    assert!(e.to_string().contains("instruction budget"), "{e}");
}
