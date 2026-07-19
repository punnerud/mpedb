//! The code from `GUIDE.md`, compiled and run.
//!
//! Every Rust snippet in the guide exists here first. Documentation that is
//! not executed rots quietly — it keeps compiling in a reader's head and
//! nowhere else — and this project has already shipped a README claiming a
//! surface the binary did not have.
//!
//! When the guide changes, change this; when this fails, the guide is wrong.

use mpedb::{params, Config, Database, ExecResult};
use mpedb_types::Value;

/// One throwaway database per test, removed on drop.
struct Tmp {
    db: Database,
    path: String,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

impl std::ops::Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

/// The schema from the guide's Quickstart, verbatim.
const GUIDE_CONFIG: &str = r#"
[database]
size_mb = 64
max_readers = 128
durability = "wal"

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
  nullable = true
  check = "age >= 0 AND age < 150"
"#;

/// `/dev/shm` when present (fast tmpfs, mpedb's habitat), else the platform
/// temp dir — keeps the scratch path portable to macOS, where `/dev/shm` does
/// not exist (#66).
fn scratch_path(name: String) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    dir.join(name).to_string_lossy().into_owned()
}

fn open(tag: &str) -> Tmp {
    let path = scratch_path(format!("mpedb-guide-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!("[database]\npath = \"{path}\"\n{}", GUIDE_CONFIG.replacen("\n[database]\n", "\n", 1));
    let cfg = Config::from_toml_str(&toml).unwrap_or_else(|e| panic!("guide config rejected: {e}"));
    let db = Database::open_with_config(cfg).unwrap();
    Tmp { db, path }
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// GUIDE.md § Quickstart.
#[test]
fn quickstart() {
    let db = open("quickstart");

    // Write.
    let n = db.query(
        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
        &params![1, "ada@example.com", 36],
    );
    assert!(matches!(n, Ok(ExecResult::Affected(1))));

    // Read.
    let r = db
        .query("SELECT email, age FROM users WHERE id = $1", &params![1])
        .unwrap();
    assert_eq!(
        rows(r),
        vec![vec![Value::Text("ada@example.com".into()), Value::Int(36)]]
    );

    // The hot path: compile once, execute by hash forever.
    let h = db.prepare("SELECT email FROM users WHERE id = $1").unwrap();
    let r = db.execute(&h, &params![1]).unwrap();
    assert_eq!(rows(r), vec![vec![Value::Text("ada@example.com".into())]]);
}

/// GUIDE.md § What the schema buys you — each of these is an ERROR here and
/// silently accepted (or coerced) by sqlite3 without STRICT.
#[test]
fn the_schema_refuses_what_sqlite_would_take() {
    let db = open("rigid");
    db.query(
        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
        &params![1, "ada@example.com", 36],
    )
    .unwrap();

    // A string in an integer column.
    assert!(db
        .query(
            "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
            &params![2, "b@example.com", "not a number"],
        )
        .is_err());

    // NOT NULL.
    assert!(db
        .query("INSERT INTO users (id, age) VALUES ($1, $2)", &params![3, 20])
        .is_err());

    // UNIQUE.
    assert!(db
        .query(
            "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
            &params![4, "ada@example.com", 20],
        )
        .is_err());

    // CHECK.
    assert!(db
        .query(
            "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
            &params![5, "c@example.com", 200],
        )
        .is_err());

    // Every one of those left the table alone.
    assert_eq!(rows(db.query("SELECT id FROM users", &[]).unwrap()).len(), 1);
}

/// GUIDE.md § Transactions.
#[test]
fn transactions_are_all_or_nothing() {
    let db = open("txn");

    let mut tx = db.begin().unwrap();
    tx.query(
        "INSERT INTO users (id, email) VALUES ($1, $2)",
        &params![1, "a@example.com"],
    )
    .unwrap();
    tx.query(
        "INSERT INTO users (id, email) VALUES ($1, $2)",
        &params![2, "b@example.com"],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(rows(db.query("SELECT id FROM users", &[]).unwrap()).len(), 2);

    // Rollback: dropping without commit throws the work away.
    let mut tx = db.begin().unwrap();
    tx.query(
        "INSERT INTO users (id, email) VALUES ($1, $2)",
        &params![3, "c@example.com"],
    )
    .unwrap();
    tx.rollback();
    assert_eq!(
        rows(db.query("SELECT id FROM users", &[]).unwrap()).len(),
        2,
        "the rolled-back row must be gone"
    );
}

/// GUIDE.md § Upsert — the ON CONFLICT forms, including on a UNIQUE column
/// that is not the primary key.
#[test]
fn upsert() {
    let db = open("upsert");
    db.query(
        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
        &params![1, "ada@example.com", 36],
    )
    .unwrap();

    // On the primary key.
    db.query(
        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET age = excluded.age",
        &params![1, "ada@example.com", 37],
    )
    .unwrap();
    assert_eq!(
        rows(db.query("SELECT age FROM users WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Int(37)]]
    );

    // On a UNIQUE column. The proposed id 99 never enters — the row that owns
    // the email is the one updated.
    let r = db
        .query(
            "INSERT INTO users (id, email, age) VALUES ($1, $2, $3) \
             ON CONFLICT (email) DO UPDATE SET age = users.age + 1 RETURNING id, age",
            &params![99, "ada@example.com", 0],
        )
        .unwrap();
    assert_eq!(rows(r), vec![vec![Value::Int(1), Value::Int(38)]]);

    // DO NOTHING.
    db.query(
        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        &params![1, "z@example.com", 1],
    )
    .unwrap();
    assert_eq!(rows(db.query("SELECT id FROM users", &[]).unwrap()).len(), 1);
}

/// GUIDE.md § Reading the plan.
#[test]
fn explain_says_what_it_will_do() {
    let db = open("explain");
    match db
        .query("EXPLAIN SELECT email FROM users WHERE id = $1", &[])
        .unwrap()
    {
        ExecResult::Explain(text) => {
            // The guide prints this transcript, so compare it EXACTLY. A test
            // that only checks `contains("PkPoint")` would let the guide show
            // a rendering the binary never produced — which is what happened:
            // the first draft printed the Debug form, `PkPoint([Param(0)])`,
            // in the document whose premise is that it is executed.
            assert_eq!(
                text.trim_end(),
                "Select users\n  \
                 access: PkPoint(id = $1)\n  \
                 project: email\n  \
                 footprint: read_only=true tables_read=0x1 tables_written=0x0 \
                 indexes_used=0x1 key=Point",
                "GUIDE.md's EXPLAIN transcript is stale"
            );
        }
        other => panic!("expected an explain, got {other:?}"),
    }
    match db.query("EXPLAIN SELECT email FROM users", &[]).unwrap() {
        ExecResult::Explain(text) => assert!(text.contains("FullScan"), "{text}"),
        other => panic!("expected an explain, got {other:?}"),
    }
}

const SHOP_CONFIG: &str = r#"
[[table]]
name = "items"
primary_key = ["iid"]

  [[table.column]]
  name = "iid"
  type = "int64"

  [[table.column]]
  name = "oid"
  type = "int64"

  [[table.column]]
  name = "qty"
  type = "int64"

[[table]]
name = "orders"
primary_key = ["oid"]

  [[table.column]]
  name = "oid"
  type = "int64"

  [[table.column]]
  name = "customer"
  type = "text"
"#;

fn open_shop(tag: &str) -> Tmp {
    let path = scratch_path(format!("mpedb-guide-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n{SHOP_CONFIG}"
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    Tmp { db, path }
}

/// GUIDE.md § Aggregates and joins.
#[test]
fn aggregates_and_joins() {
    let db = open_shop("join");
    for (oid, c) in [(1, "ada"), (2, "bob"), (3, "nobody")] {
        db.query(
            "INSERT INTO orders (oid, customer) VALUES ($1, $2)",
            &params![oid, c],
        )
        .unwrap();
    }
    for (iid, oid, qty) in [(10, 1, 2), (11, 1, 3), (12, 2, 5)] {
        db.query(
            "INSERT INTO items (iid, oid, qty) VALUES ($1, $2, $3)",
            &params![iid, oid, qty],
        )
        .unwrap();
    }

    // Aggregates.
    let r = db
        .query("SELECT count(*), sum(qty), avg(qty) FROM items", &[])
        .unwrap();
    assert_eq!(
        rows(r),
        vec![vec![Value::Int(3), Value::Int(10), Value::Float(10.0 / 3.0)]]
    );

    // GROUP BY / HAVING.
    let r = db
        .query(
            "SELECT oid, count(*) FROM items GROUP BY oid HAVING count(*) > 1 ORDER BY oid",
            &[],
        )
        .unwrap();
    assert_eq!(rows(r), vec![vec![Value::Int(1), Value::Int(2)]]);

    // A join. Order 3 has no items, so it is not in the answer at all — an
    // INNER JOIN emits a row only where both sides match.
    let r = db
        .query(
            "SELECT orders.customer, sum(items.qty) FROM items \
             JOIN orders ON items.oid = orders.oid \
             GROUP BY orders.customer ORDER BY orders.customer",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows(r),
        vec![
            vec![Value::Text("ada".into()), Value::Int(5)],
            vec![Value::Text("bob".into()), Value::Int(5)],
        ]
    );

    // DISTINCT.
    let r = db
        .query("SELECT DISTINCT oid FROM items ORDER BY oid", &[])
        .unwrap();
    assert_eq!(rows(r), vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // LEFT JOIN keeps the row with no match and NULL-extends the inner side:
    // order 3 ("nobody") comes back with a NULL quantity instead of vanishing.
    let r = db
        .query(
            "SELECT orders.customer, items.qty FROM orders \
             LEFT JOIN items ON items.oid = orders.oid \
             WHERE orders.oid = 3",
            &[],
        )
        .unwrap();
    assert_eq!(rows(r), vec![vec![Value::Text("nobody".into()), Value::Null]]);

    // A chain with aliases — the third table is the same physical table again
    // (a self-join): every other item in the same order.
    let r = db
        .query(
            "SELECT a.iid, b.iid FROM items a \
             JOIN orders o ON a.oid = o.oid \
             JOIN items b ON b.oid = o.oid AND b.iid > a.iid",
            &[],
        )
        .unwrap();
    assert_eq!(rows(r), vec![vec![Value::Int(10), Value::Int(11)]]);
}

/// GUIDE.md § Large values: stream them.
#[test]
fn stream_a_file_in() {
    let path = scratch_path(format!("mpedb-guide-blob-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\n\
         [[table]]\nname = \"files\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"data\"\n  type = \"blob\""
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let db = Tmp { db, path };

    // A stand-in for "a big file on disk" — the memory ceiling is proven at
    // 8 MiB in tests/insert_file.rs; the guide only shows the call shape.
    let src = scratch_path(format!("mpedb-guide-blob-src-{}", std::process::id()));
    std::fs::write(&src, vec![7u8; 128 * 1024]).unwrap();

    // The guide's snippet: the `&[][..]` placeholder marks the streamed
    // column; the file's bytes never sit in memory at once.
    let mut s = db.begin().unwrap();
    s.insert_file("files", &params![1i64, &[][..]], 1, &src).unwrap();
    s.commit().unwrap();

    let r = rows(db.query("SELECT data FROM files WHERE id = 1", &[]).unwrap());
    match &r[0][0] {
        Value::Blob(b) => assert_eq!(b.len(), 128 * 1024),
        other => panic!("expected the blob back, got {other:?}"),
    }
    let _ = std::fs::remove_file(&src);
}

/// GUIDE.md § Coming from sqlite3 — the differences that will bite, each one
/// executed rather than asserted in prose.
#[test]
fn the_sqlite_differences_that_bite() {
    let db = open_shop("diffs");
    db.query(
        "INSERT INTO orders (oid, customer) VALUES ($1, $2)",
        &params![1, "ada"],
    )
    .unwrap();

    // 1. DDL is live: CREATE/DROP TABLE, ALTER RENAME, ALTER ADD COLUMN
    //    (nullable). A new table takes the next free id and nothing renumbers.
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (1, 'x')", &[]).unwrap();
    db.query("ALTER TABLE t ADD COLUMN note TEXT", &[]).unwrap();
    db.query("ALTER TABLE t RENAME TO t2", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT v FROM t2 WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("x".into())]]
    );
    db.query("DROP TABLE t2", &[]).unwrap();
    // …a PK-less CREATE now gets sqlite's hidden rowid (#94), so it is accepted;
    // but the changes that need a default fill / row rewrite still refuse:
    // NOT NULL on ADD, and DROP COLUMN.
    db.query("CREATE TABLE u (id INTEGER)", &[]).unwrap();
    assert!(db.query("ALTER TABLE orders ADD COLUMN x INT NOT NULL", &[]).is_err());

    // 2. Division by zero yields NULL, matching sqlite. (Also a FROM-less
    // SELECT — one synthetic row, so the division is reached and evaluated.)
    assert_eq!(
        rows(db.query("SELECT 1 / 0", &[]).unwrap()),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(db.query("SELECT 3 + 5", &[]).unwrap()),
        vec![vec![Value::Int(8)]],
        "FROM-less SELECT evaluates over one synthetic row"
    );

    // 6. CASE/COALESCE arms cannot mix int64 and float64 — sqlite types the
    // winning arm per row, rigid typing cannot. The CAST in the message works.
    let err = db.query("SELECT coalesce(30, 1.5)", &[]).unwrap_err();
    assert!(format!("{err}").contains("CAST"), "{err}");
    assert!(db.query("SELECT coalesce(CAST(30 AS REAL), 1.5)", &[]).is_ok());

    // 3. Every join kind works two-table (RIGHT plans as a swapped LEFT;
    // FULL NULL-extends both sides); only a RIGHT/FULL inside a multi-join
    // CHAIN is refused, with the message saying the manual fix.
    assert!(db
        .query(
            "SELECT orders.oid FROM items RIGHT JOIN orders ON items.oid = orders.oid",
            &[],
        )
        .is_ok(), "two-table RIGHT JOIN works");
    assert!(db
        .query(
            "SELECT orders.oid FROM items FULL OUTER JOIN orders ON items.oid = orders.oid",
            &[],
        )
        .is_ok(), "two-table FULL JOIN works");
    let err = db
        .query(
            "SELECT o.oid FROM items i JOIN orders o ON i.oid = o.oid \
             RIGHT JOIN items x ON x.oid = o.oid",
            &[],
        )
        .unwrap_err();
    assert!(format!("{err}").contains("multi-join chain"), "{err}");
    assert!(db
        .query("SELECT orders.oid FROM items CROSS JOIN orders", &[])
        .is_ok(), "CROSS JOIN is the cartesian product, like the comma-join");
    assert!(db
        .query("SELECT oid FROM orders UNION SELECT oid FROM items", &[])
        .is_ok(), "compound set ops work");
    assert!(db
        .query(
            "SELECT customer FROM orders WHERE oid = (SELECT max(oid) FROM orders)",
            &[],
        )
        .is_ok(), "scalar subqueries work");
    // >1 row from a scalar subquery is an ERROR (PG's rule) — sqlite would
    // silently take the first row. Two rows make it observable.
    db.query(
        "INSERT INTO orders (oid, customer) VALUES ($1, $2)",
        &params![2, "bob"],
    )
    .unwrap();
    assert!(db
        .query("SELECT customer FROM orders WHERE oid = (SELECT oid FROM orders)", &[])
        .is_err(), "a multi-row scalar subquery must error");

    // 4. ORDER BY must name something the query outputs.
    assert!(db
        .query("SELECT customer FROM orders ORDER BY oid + 1", &[])
        .is_ok(), "a sort-only column is fine");
    assert!(db
        .query("SELECT DISTINCT customer FROM orders ORDER BY oid", &[])
        .is_err(), "under DISTINCT it must be selected");
}
