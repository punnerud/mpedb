//! v2 delta-overlay differential: the merged view (overlay shadows base,
//! tombstones suppress) against hand-computed expectations over a real sqlite
//! file, plus the LOCKED-mode contract (a foreign sqlite writer gets
//! SQLITE_BUSY while the overlay is open) and the divergence refusal.

use mpedb::{SqliteOverlay, Value};
use rusqlite::Connection;

fn setup(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-overlay-tests")
        .join(format!("ovl-{tag}-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for suffix in ["", ".overlay.mpedb", ".overlay.probe"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);",
    )
    .unwrap();
    for i in 0..100i64 {
        c.execute(
            "INSERT INTO users VALUES (?, ?, ?)",
            rusqlite::params![i, format!("u{i}"), 20 + i % 50],
        )
        .unwrap();
    }
    drop(c);
    p
}

fn rows(r: mpedb::ExecResult) -> Vec<Vec<Value>> {
    match r {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        })
        .collect()
}

#[test]
fn merged_view_reads_writes_and_tombstones() {
    let p = setup("merge");
    let ovl = SqliteOverlay::open(&p).unwrap();

    // Pure read-through: nothing in the overlay yet.
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(100)]]);
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 42", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("u42".into())]]);

    // INSERT lands in the overlay, reads see the merged view.
    ovl.query("INSERT INTO users (id, name, age) VALUES (1000, 'ny', 33)", &[]).unwrap();
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(101)]]);
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 1000", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("ny".into())]]);

    // UPDATE of a base row: the overlay image shadows — exactly one row for
    // that PK in a scan, with the new value.
    ovl.query("UPDATE users SET name = 'endret' WHERE id = 42", &[]).unwrap();
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 42", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("endret".into())]]);
    let got = rows(ovl.query("SELECT count(*) FROM users WHERE id = 42", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)]], "shadow must not duplicate");
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(101)]]);

    // DELETE of a base row: tombstone suppresses it everywhere.
    ovl.query("DELETE FROM users WHERE id = 7", &[]).unwrap();
    let got = rows(ovl.query("SELECT count(*) FROM users WHERE id = 7", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(0)]]);
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(100)]]);

    // Range scan through the merge: ordered, shadowed, tombstone-free.
    let got = rows(
        ovl.query("SELECT id FROM users WHERE id >= 5 AND id <= 10 ORDER BY id", &[]).unwrap(),
    );
    assert_eq!(ints(&got), vec![5, 6, 8, 9, 10]);

    // Uniqueness is over the MERGED view: a live base PK collides…
    let err = ovl.query("INSERT INTO users (id, name, age) VALUES (50, 'dup', 1)", &[]).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("primary key"), "{err}");
    // …a live overlay PK collides…
    let err =
        ovl.query("INSERT INTO users (id, name, age) VALUES (1000, 'dup', 1)", &[]).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("primary key"), "{err}");
    // …and a tombstoned PK is free again.
    ovl.query("INSERT INTO users (id, name, age) VALUES (7, 'gjenbrukt', 2)", &[]).unwrap();
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 7", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("gjenbrukt".into())]]);

    // UPDATE over a range spanning base and overlay rows.
    ovl.query("UPDATE users SET age = 99 WHERE id >= 98 AND id <= 1000", &[]).unwrap();
    let got = rows(
        ovl.query(
            "SELECT count(*) FROM users WHERE CAST(age AS INTEGER) = 99",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(3)]]); // 98, 99, 1000

    // The base FILE is untouched by all of the above.
    drop(ovl);
    let lib = Connection::open(&p).unwrap();
    let n: i64 = lib.query_row("SELECT count(*) FROM users", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 100, "deltas must never leak into the base before checkpoint");
    let name: String =
        lib.query_row("SELECT name FROM users WHERE id = 42", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "u42");

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn deltas_survive_reopen_and_divergence_is_refused() {
    let p = setup("reopen");
    {
        let ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("INSERT INTO users (id, name, age) VALUES (777, 'varig', 1)", &[]).unwrap();
        ovl.query("DELETE FROM users WHERE id = 3", &[]).unwrap();
    } // drop releases the SHARED lock

    // Reopen: the stored settled stamp still matches — deltas are live.
    {
        let ovl = SqliteOverlay::open(&p).unwrap();
        let got = rows(ovl.query("SELECT name FROM users WHERE id = 777", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Text("varig".into())]]);
        let got = rows(ovl.query("SELECT count(*) FROM users WHERE id = 3", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Int(0)]]);
    }

    // A foreign writer moves the base in the unlocked window…
    {
        let c = Connection::open(&p).unwrap();
        c.execute("INSERT INTO users VALUES (500, 'fremmed', 9)", []).unwrap();
    }
    // …and reopen refuses by name instead of merging against a moved base.
    let Err(err) = SqliteOverlay::open(&p) else {
        panic!("open against a moved base must refuse");
    };
    let msg = format!("{err}");
    assert!(msg.contains("changed since"), "{msg}");

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn locked_mode_gives_foreign_writers_sqlite_busy() {
    let p = setup("busy");
    let ovl = SqliteOverlay::open(&p).unwrap();

    // A foreign sqlite writer must experience the held SHARED as plain BUSY
    // (OFD locks conflict with the library's classic locks even in-process).
    let c = Connection::open(&p).unwrap();
    c.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
    let err = c.execute("INSERT INTO users VALUES (600, 'blokkert', 9)", []).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("locked") || msg.contains("busy"),
        "expected SQLITE_BUSY, got: {msg}"
    );

    // Foreign READERS coexist untouched the whole time.
    let n: i64 = c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 100);

    // And the failed foreign attempt cost the overlay nothing.
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(100)]]);

    drop(ovl);
    // With the overlay closed, the same writer succeeds.
    c.execute("INSERT INTO users VALUES (600, 'fri', 9)", []).unwrap();

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}
