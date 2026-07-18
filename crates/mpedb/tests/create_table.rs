//! #47 stage 2/3: `CREATE TABLE` end to end — the created table is usable
//! (PK point/scan, composite unique, NOT NULL), existing tables keep their
//! ids and data (append-only DDL, the whole point of stable ids), the
//! change persists across reopen (file-authoritative schema), and a second
//! process sees the new table on its next statement (schema-gen reload).

use mpedb::{params, Config, Database, Error, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn config(name: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-createtable-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // Seed with ONE table (`users`, id 0). Everything else is created live.
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 32

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "name"
  type = "text"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match &rows(db.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

#[test]
fn create_use_and_persist() {
    let (cfg, path) = config("basic");
    {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        db.query("INSERT INTO users (id, name) VALUES (1, 'a')", &[]).unwrap();

        // Create a table, then use it: insert, PK point lookup, scan, aggregate.
        assert!(matches!(
            db.query(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INT NOT NULL, \
                 note TEXT)",
                &[],
            )
            .unwrap(),
            ExecResult::Affected(0)
        ));
        for (id, bal) in [(1, 100), (2, 200), (3, 300)] {
            db.query(
                "INSERT INTO accounts (id, balance, note) VALUES ($1, $2, 'x')",
                &params![id, bal],
            )
            .unwrap();
        }
        // PK point lookup.
        let got = rows(db.query("SELECT balance FROM accounts WHERE id = 2", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Int(200)]]);
        // Aggregate over a scan.
        assert_eq!(scalar_i64(&db, "SELECT sum(balance) FROM accounts"), 600);
        // NOT NULL is enforced on the created column (a statically-NULL
        // insert is caught at bind time; a runtime NULL would be
        // NotNullViolation — either way it refuses).
        let err = db
            .query("INSERT INTO accounts (id, balance, note) VALUES (4, NULL, 'y')", &[])
            .unwrap_err();
        assert!(
            matches!(err, Error::NotNullViolation { .. } | Error::Bind(_)),
            "{err}"
        );

        // The pre-existing table is untouched — same data, still works.
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 1);
        db.query("INSERT INTO users (id, name) VALUES (2, 'b')", &[]).unwrap();
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 2);
        db.verify().unwrap();
    }

    // Reopen with the ORIGINAL config (its schema still hash-matches the
    // frozen SEED): the created table is read back from the catalog.
    {
        let db = Database::open_with_config(cfg).unwrap();
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM accounts"), 3);
        assert_eq!(scalar_i64(&db, "SELECT balance FROM accounts WHERE id = 3"), 300);
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 2);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn existing_table_ids_are_stable_when_a_name_sorts_before() {
    // The regression the whole format change exists to prevent: creating a
    // table whose name sorts BEFORE an existing one must NOT renumber the
    // existing table (which would point its catalog tree at the wrong data).
    let (cfg, path) = config("stable-ids");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("INSERT INTO users (id, name) VALUES (7, 'seven')", &[]).unwrap();

    // `aaa` sorts before `users` alphabetically — under v1 this renumbered.
    db.query("CREATE TABLE aaa (k INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    db.query("INSERT INTO aaa (k, v) VALUES (1, 'one')", &[]).unwrap();

    // `users` still reads its own rows (id 0 unchanged), and a fresh write
    // lands in the right tree.
    assert_eq!(
        rows(db.query("SELECT name FROM users WHERE id = 7", &[]).unwrap()),
        vec![vec![Value::Text("seven".into())]]
    );
    db.query("INSERT INTO users (id, name) VALUES (8, 'eight')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 2);
    // …and the new table reads its own.
    assert_eq!(
        rows(db.query("SELECT v FROM aaa WHERE k = 1", &[]).unwrap()),
        vec![vec![Value::Text("one".into())]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn composite_pk_and_unique_in_created_table() {
    let (cfg, path) = config("composite");
    let db = Database::open_with_config(cfg).unwrap();
    db.query(
        "CREATE TABLE lines (oid INT, lno INT, sku TEXT, PRIMARY KEY (oid, lno), \
         UNIQUE (oid, sku))",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO lines (oid, lno, sku) VALUES (1, 1, 'a')", &[]).unwrap();
    db.query("INSERT INTO lines (oid, lno, sku) VALUES (1, 2, 'b')", &[]).unwrap();
    // Composite PK point lookup.
    assert_eq!(
        rows(db.query("SELECT sku FROM lines WHERE oid = 1 AND lno = 2", &[]).unwrap()),
        vec![vec![Value::Text("b".into())]]
    );
    // Composite UNIQUE enforces over the set: (1,'a') again refuses.
    let err = db
        .query("INSERT INTO lines (oid, lno, sku) VALUES (1, 9, 'a')", &[])
        .unwrap_err();
    assert!(matches!(err, Error::UniqueViolation { .. }), "{err}");
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn create_table_refusals() {
    let (cfg, path) = config("refuse");
    let db = Database::open_with_config(cfg).unwrap();

    // No PK.
    assert!(db.query("CREATE TABLE t (a INT, b TEXT)", &[]).is_err());
    // Duplicate name (a seed table).
    assert!(db.query("CREATE TABLE users (id INT PRIMARY KEY)", &[]).is_err());
    // Two PK forms at once.
    assert!(db
        .query("CREATE TABLE t (a INT PRIMARY KEY, b INT, PRIMARY KEY (b))", &[])
        .is_err());
    // Unknown key column.
    assert!(db
        .query("CREATE TABLE t (a INT, PRIMARY KEY (nope))", &[])
        .is_err());
    // Two columns each marked inline PRIMARY KEY: a typo, not a composite —
    // sqlite/PG both refuse (a composite key must be `PRIMARY KEY (a, b)`).
    let err = db
        .query("CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)", &[])
        .unwrap_err();
    assert!(format!("{err}").contains("more than one"), "{err}");
    // The intended composite form is accepted, and a row differing only in
    // the second key column is distinct (proving the composite key applies).
    db.query(
        "CREATE TABLE comp (a INT, b INT, PRIMARY KEY (a, b))",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO comp (a, b) VALUES (1, 1)", &[]).unwrap();
    db.query("INSERT INTO comp (a, b) VALUES (1, 2)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM comp"), 2);

    // After the refusals, a valid create still works (no half-applied state).
    db.query("CREATE TABLE ok (id INT PRIMARY KEY)", &[]).unwrap();
    db.query("INSERT INTO ok (id) VALUES (5)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM ok"), 1);
    // A second `users` really was rejected — the original is intact.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 0);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn second_process_sees_the_new_table() {
    // Two handles on the SAME file = the multi-process shape. B caches the
    // schema at open; A creates a table; B must see it on its next statement
    // (schema-gen reload), and B's writes to the OLD table still land right.
    let (cfg, path) = config("multiproc");
    let a = Database::open_with_config(cfg.clone()).unwrap();
    let b = Database::open_with_config(cfg).unwrap();

    // B warms its cached schema (gen 0) by touching the seed table.
    a.query("INSERT INTO users (id, name) VALUES (1, 'a')", &[]).unwrap();
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM users"), 1);

    // A creates a table (bumps schema_gen).
    a.query("CREATE TABLE ledger (id INTEGER PRIMARY KEY, amt INT NOT NULL)", &[]).unwrap();
    a.query("INSERT INTO ledger (id, amt) VALUES (1, 42)", &[]).unwrap();

    // B — still holding the stale schema — must pick up `ledger` on its next
    // query (refresh-before-compile), both read and write.
    assert_eq!(scalar_i64(&b, "SELECT amt FROM ledger WHERE id = 1"), 42);
    b.query("INSERT INTO ledger (id, amt) VALUES (2, 99)", &[]).unwrap();
    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM ledger"), 2);

    // …and B's writes to the pre-existing table still target the right tree.
    b.query("INSERT INTO users (id, name) VALUES (2, 'b')", &[]).unwrap();
    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM users"), 2);
    a.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn typeless_column_accepts_any_scalar_like_sqlite() {
    // A column with no declared type is sqlite's no-affinity column; mpedb maps
    // it to `Any` (the loose-type escape hatch), so it stores any scalar as-is.
    // Requires an explicit PK (mpedb has no implicit rowid yet).
    let (cfg, path) = config("typeless");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE tl (id INTEGER PRIMARY KEY, data)", &[]).unwrap();
    db.query("INSERT INTO tl VALUES (1, 'text')", &[]).unwrap();
    db.query("INSERT INTO tl VALUES (2, 42)", &[]).unwrap();
    db.query("INSERT INTO tl VALUES (3, 3.5)", &[]).unwrap();
    db.query("INSERT INTO tl VALUES (4, NULL)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM tl"), 4);
    // Each value comes back with its original dynamic type.
    let r = rows(db.query("SELECT data FROM tl ORDER BY id", &[]).unwrap());
    assert_eq!(r[0][0], Value::Text("text".into()));
    assert_eq!(r[1][0], Value::Int(42));
    assert_eq!(r[2][0], Value::Float(3.5));
    assert_eq!(r[3][0], Value::Null);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
