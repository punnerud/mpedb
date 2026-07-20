//! v2 delta-overlay differential: the merged view (overlay shadows base,
//! tombstones suppress) against hand-computed expectations over a real sqlite
//! file, plus the LOCKED-mode contract (a foreign sqlite writer gets
//! SQLITE_BUSY while the overlay is open) and the divergence refusal.

use mpedb::{LockMode, ReconcilePolicy, SqliteOverlay, Value};
use rusqlite::Connection;

/// The base .db plus its overlay siblings, REMOVED on drop. The overlay file
/// alone is 64 MB, and before this guard every test run left a full set
/// behind — measured twice at 23-25 GB of /tmp, enough to fill / and fail
/// unrelated workspace runs with ENOSPC.
struct OvlPaths(std::path::PathBuf);

impl std::ops::Deref for OvlPaths {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for OvlPaths {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for OvlPaths {
    fn drop(&mut self) {
        for suffix in ["", ".overlay.mpedb", ".overlay.probe", "-journal", "-wal"] {
            let _ = std::fs::remove_file(format!("{}{}", self.0.display(), suffix));
        }
    }
}

fn setup(tag: &str) -> OvlPaths {
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
    OvlPaths(p)
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
    let mut ovl = SqliteOverlay::open(&p).unwrap();

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
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("INSERT INTO users (id, name, age) VALUES (777, 'varig', 1)", &[]).unwrap();
        ovl.query("DELETE FROM users WHERE id = 3", &[]).unwrap();
    } // drop releases the SHARED lock

    // Reopen: the stored settled stamp still matches — deltas are live.
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
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
fn text_pk_without_rowid_merges_like_any_other_shape() {
    let p = OvlPaths(
        std::env::temp_dir()
            .join("mpedb-overlay-tests")
            .join(format!("ovl-textpk-{}.db", std::process::id())),
    );
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for suffix in ["", ".overlay.mpedb", ".overlay.probe"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = DELETE;
             CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;
             INSERT INTO kv VALUES ('alpha',1),('beta',2),('gamma',3),('delta',4);",
        )
        .unwrap();
    }
    let mut ovl = SqliteOverlay::open(&p).unwrap();

    // Point probe + full scan straight through.
    let got = rows(ovl.query("SELECT v FROM kv WHERE k = 'beta'", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2)]]);
    let got = rows(ovl.query("SELECT k FROM kv ORDER BY k", &[]).unwrap());
    let names: Vec<&str> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["alpha", "beta", "delta", "gamma"], "BINARY order");

    // Deltas of every kind on a text PK, merged per key.
    ovl.query("UPDATE kv SET v = 22 WHERE k = 'beta'", &[]).unwrap();
    ovl.query("DELETE FROM kv WHERE k = 'gamma'", &[]).unwrap();
    ovl.query("INSERT INTO kv (k, v) VALUES ('epsilon', 5)", &[]).unwrap();
    let got = rows(ovl.query("SELECT v FROM kv WHERE k = 'beta'", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(22)]]);
    let got = rows(ovl.query("SELECT count(*) FROM kv", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(4)]]);
    // Range over the merge: text keycode bounds both directions.
    let got = rows(
        ovl.query("SELECT k FROM kv WHERE k > 'alpha' AND k <= 'epsilon' ORDER BY k", &[])
            .unwrap(),
    );
    let names: Vec<&str> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["beta", "delta", "epsilon"]);

    // The base is untouched until checkpoint.
    drop(ovl);
    let c = Connection::open(&p).unwrap();
    let v: i64 = c.query_row("SELECT v FROM kv WHERE k = 'beta'", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 2);

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn reconcile_resolves_conflicts_by_named_policy() {
    let p = setup("reconcile");
    // Session 1 (LOCKED): three deltas — update, insert, delete — plus a
    // second update to the same PK (the FIRST capture must be what survives).
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("UPDATE users SET name = 'vaar' WHERE id = 10", &[]).unwrap();
        ovl.query("UPDATE users SET age = 1 WHERE id = 10", &[]).unwrap();
        ovl.query("INSERT INTO users (id, name, age) VALUES (100, 'ny', 1)", &[]).unwrap();
        ovl.query("DELETE FROM users WHERE id = 20", &[]).unwrap();
    }
    // Foreign writer: touches id=10 (CONFLICT with our update) and id=30
    // (no delta of ours — not a conflict at all).
    {
        let c = Connection::open(&p).unwrap();
        c.execute("UPDATE users SET name = 'deres' WHERE id = 10", []).unwrap();
        c.execute("UPDATE users SET name = 'ren-fremmed' WHERE id = 30", []).unwrap();
    }
    // Plain open still refuses by name.
    let Err(err) = SqliteOverlay::open(&p) else {
        panic!("divergence with unpushed deltas must refuse without a policy");
    };
    assert!(format!("{err}").contains("reconcile"), "{err}");

    // THEIRS: the conflicted delta drops; the provably-untouched two stay.
    {
        let mut ovl =
            SqliteOverlay::open_with_options(&p, LockMode::Locked, Some(ReconcilePolicy::Theirs))
                .unwrap();
        let got = rows(ovl.query("SELECT name FROM users WHERE id = 10", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Text("deres".into())]], "theirs won id=10");
        let got = rows(ovl.query("SELECT name FROM users WHERE id = 30", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Text("ren-fremmed".into())]]);
        let got = rows(ovl.query("SELECT name FROM users WHERE id = 100", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Text("ny".into())]], "our insert survived");
        let got = rows(ovl.query("SELECT count(*) FROM users WHERE id = 20", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Int(0)]], "our tombstone survived");
    }

    // OURS, fresh scenario on the same base: conflict again, keep ours,
    // checkpoint — OUR value lands in the base over theirs.
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("UPDATE users SET name = 'vaar2' WHERE id = 11", &[]).unwrap();
    }
    {
        let c = Connection::open(&p).unwrap();
        c.execute("UPDATE users SET name = 'deres2' WHERE id = 11", []).unwrap();
    }
    {
        let mut ovl =
            SqliteOverlay::open_with_options(&p, LockMode::Locked, Some(ReconcilePolicy::Ours))
                .unwrap();
        let got = rows(ovl.query("SELECT name FROM users WHERE id = 11", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Text("vaar2".into())]], "ours kept");
        // A reconciled handle checkpoints normally (feature-gated builds
        // exercise this in the checkpoint suite; here we just verify the
        // handle serves).
        let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
        assert_eq!(got, vec![vec![Value::Int(100)]]);
    }

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn optimistic_mode_adopts_foreign_writes_and_guards_deltas() {
    let p = setup("optimistic");
    let mut ovl = SqliteOverlay::open_with_mode(&p, LockMode::Optimistic).unwrap();

    // Plain read through a bracket.
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(100)]]);

    // A foreign writer commits BETWEEN our statements (no standing lock to
    // stop it. With an empty overlay the next bracket adopts the moved base
    // in place — that is the whole point of the mode.
    {
        let c = Connection::open(&p).unwrap();
        c.execute("INSERT INTO users VALUES (500, 'fremmed', 9)", []).unwrap();
    }
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 500", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("fremmed".into())]]);

    // Our own writes land in the overlay and merge as usual.
    ovl.query("INSERT INTO users (id, name, age) VALUES (600, 'egen', 1)", &[]).unwrap();
    let got = rows(ovl.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(102)]]);

    // With UNPUSHED deltas, a foreign commit is genuine divergence: the
    // next statement refuses by name instead of merging against a moved
    // base (busy ≠ divergence; this is the stamp, not a lock).
    {
        let c = Connection::open(&p).unwrap();
        c.execute("INSERT INTO users VALUES (700, 'kollisjon', 9)", []).unwrap();
    }
    let err = ovl.query("SELECT count(*) FROM users", &[]).unwrap_err();
    assert!(format!("{err}").contains("unpushed deltas"), "{err}");

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn offline_mode_serves_overlay_and_names_every_fall_through() {
    let p = setup("offline");
    // Seed the overlay in a LOCKED session, no checkpoint.
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("UPDATE users SET name = 'lokal' WHERE id = 5", &[]).unwrap();
    }
    let mut ovl = SqliteOverlay::open_with_mode(&p, LockMode::Offline).unwrap();

    // Overlay-resident point read and update: no base needed, they work.
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 5", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("lokal".into())]]);
    ovl.query("UPDATE users SET age = 50 WHERE id = 5", &[]).unwrap();

    // Everything needing the base refuses BY NAME: scans, fall-through
    // misses, and insert's merged uniqueness probe.
    for sql in [
        "SELECT count(*) FROM users",
        "SELECT name FROM users WHERE id = 7",
        "INSERT INTO users (id, name, age) VALUES (900, 'x', 1)",
    ] {
        let err = ovl.query(sql, &[]).unwrap_err();
        assert!(format!("{err}").contains("unlocked-offline"), "{sql}: {err}");
    }

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn co_attached_handles_share_one_overlay() {
    let p = setup("coattach");
    // Two handles = two file descriptions = the multi-process shape (OFD
    // locks are per-description; the overlay engine is multi-process by
    // construction). Two SHAREDs coexist.
    let mut a = SqliteOverlay::open(&p).unwrap();
    let mut b = SqliteOverlay::open(&p).unwrap();

    a.query("INSERT INTO users (id, name, age) VALUES (800, 'fra-a', 1)", &[]).unwrap();
    let got = rows(b.query("SELECT name FROM users WHERE id = 800", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("fra-a".into())]], "b sees a's delta");
    let got = rows(b.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(101)]]);

    b.query("UPDATE users SET name = 'fra-b' WHERE id = 800", &[]).unwrap();
    let got = rows(a.query("SELECT name FROM users WHERE id = 800", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("fra-b".into())]], "a sees b's update");

    drop(a);
    let got = rows(b.query("SELECT count(*) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(101)]], "b keeps serving after a closes");

    drop(b);
    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn locked_mode_gives_foreign_writers_sqlite_busy() {
    let p = setup("busy");
    let mut ovl = SqliteOverlay::open(&p).unwrap();

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

// ------------------------------------------- base schema fidelity (#B) -----

/// A base table's DECLARED TYPE reaches the overlay as its sqlite AFFINITY, so
/// a value written through the overlay is converted exactly as sqlite converts
/// it. This was a wrong answer, not a lost annotation: an `int` column took
/// `'1.50'` and returned `1.50`/`text` where sqlite returns `1.5`/`real`, and
/// where mpedb's own native path correctly REFUSES.
///
/// The storage type stays `Any` deliberately — a sqlite file is not rigid, so a
/// column declared `int` may genuinely hold `'abc'`, and declaring the overlay
/// column `Int64` would make mpedb refuse to read rows sqlite happily holds.
/// What sqlite guarantees is the CONVERSION, and that is what now survives.
#[test]
fn declared_affinity_survives_into_the_overlay() {
    let p = setup("affinity");
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "CREATE TABLE aff (id INTEGER PRIMARY KEY, num decimal(10,2), i int, \
         s varchar(10), r double precision, none_)",
    )
    .unwrap();
    drop(c);

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    ovl.query(
        "INSERT INTO aff (id, num, i, s, r, none_) VALUES (1, '1.50', '1.50', '1.50', '12', '1.50')",
        &[],
    )
    .unwrap();
    let got = rows(
        ovl.query(
            "SELECT typeof(num), typeof(i), typeof(s), typeof(r), typeof(none_) FROM aff",
            &[],
        )
        .unwrap(),
    );
    // sqlite3 3.45.1 on the identical script: real|real|text|real|text.
    assert_eq!(
        got,
        vec![vec![
            Value::Text("real".into()),
            Value::Text("real".into()),
            Value::Text("text".into()),
            Value::Text("real".into()),
            // No declared type at all: BLOB affinity, stored verbatim.
            Value::Text("text".into()),
        ]]
    );
    let got = rows(ovl.query("SELECT num, i, r FROM aff", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Float(1.5), Value::Float(1.5), Value::Float(12.0)]]);
}

/// A base table with no `INTEGER PRIMARY KEY` gets a SYNTHESIZED `rowid`, and
/// it must be HIDDEN — #94's rule on the native path. Leaving it visible made
/// `SELECT *` return one column MORE than sqlite does: wrong result arity.
#[test]
fn a_synthesized_rowid_is_hidden_from_select_star() {
    let p = setup("rowid-hidden");
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "CREATE TABLE norowid (a TEXT, b INT);
         INSERT INTO norowid VALUES ('x', 1), ('y', 2);
         CREATE TABLE textpk (k TEXT PRIMARY KEY, n INT) WITHOUT ROWID;
         INSERT INTO textpk VALUES ('a', 1);",
    )
    .unwrap();
    drop(c);

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    let got = rows(ovl.query("SELECT * FROM norowid ORDER BY a", &[]).unwrap());
    assert_eq!(
        got,
        vec![
            vec![Value::Text("x".into()), Value::Int(1)],
            vec![Value::Text("y".into()), Value::Int(2)],
        ],
        "SELECT * must return the base's 2 columns, not 3"
    );
    // …and the hidden rowid is still addressable by name, as sqlite's is.
    let got = rows(ovl.query("SELECT rowid FROM norowid ORDER BY rowid", &[]).unwrap());
    assert_eq!(ints(&got), vec![1, 2]);
    // A WITHOUT ROWID table has a real declared PK and nothing synthesized.
    let got = rows(ovl.query("SELECT * FROM textpk", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("a".into()), Value::Int(1)]]);
}

/// `NOT NULL` and `DEFAULT` survive the base→overlay translation. Dropping them
/// stored a NULL where sqlite stores the default, and accepted a row sqlite
/// refuses — and the second one only surfaced later, as a checkpoint failure on
/// an unrelated statement.
#[test]
fn not_null_and_default_survive_into_the_overlay() {
    let p = setup("constraints");
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL DEFAULT 'z', \
         n INT DEFAULT 7, d decimal(10,2) DEFAULT '1.50', q TEXT DEFAULT 'NOT NULL', \
         e TEXT DEFAULT 'a''b')",
    )
    .unwrap();
    drop(c);

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    // The DEFAULT fills an omitted column — sqlite stores 'z', 7 and the REAL
    // 1.5 (the default takes the column's store-time affinity too).
    ovl.query("INSERT INTO t (id) VALUES (2)", &[]).unwrap();
    let got = rows(ovl.query("SELECT v, n, d, typeof(d), q, e FROM t", &[]).unwrap());
    assert_eq!(
        got,
        vec![vec![
            Value::Text("z".into()),
            Value::Int(7),
            Value::Float(1.5),
            Value::Text("real".into()),
            // A `NOT NULL` inside a string default is a string, not a
            // constraint — `q` stays nullable and keeps its text.
            Value::Text("NOT NULL".into()),
            // A DOUBLED quote is one escaped quote inside the literal, not its
            // end — reading it as the end gave the default `a`.
            Value::Text("a'b".into()),
        ]]
    );
    // An explicit NULL into the NOT NULL column is refused, as sqlite refuses it.
    let err = ovl.query("INSERT INTO t (id, v) VALUES (4, NULL)", &[]).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("null"), "{err}");
    // …and so is an UPDATE that would introduce one.
    let err = ovl.query("UPDATE t SET v = NULL WHERE id = 2", &[]).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("null"), "{err}");
    // The refused rows left nothing behind.
    let got = rows(ovl.query("SELECT count(*) FROM t", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)]]);
}

/// What mpedb cannot carry is refused BY NAME rather than silently dropped. A
/// dropped CHECK let a row the base itself rejects into the overlay, and the
/// error then surfaced on an unrelated statement at checkpoint time; a DEFAULT
/// mpedb cannot evaluate would store NULL where sqlite stores a value.
///
/// The overlay is strict about unattachable tables — it refuses to open at all
/// rather than serve a database with silently unwritable tables — so the
/// refusal is the open error, and it must NAME the table and the reason.
#[test]
fn unrepresentable_constraints_are_refused_by_name() {
    for (tag, ddl, want) in [
        // A CHECK that COMPILES no longer refuses (see the enforcement tests);
        // one using a sqlite function mpedb lacks still must — naming the
        // function, so the caller knows exactly what to remove or wait for.
        (
            "fnchk",
            "CREATE TABLE chk (id INTEGER PRIMARY KEY, v TEXT CHECK (glob('a*', v)))",
            "glob",
        ),
        // `length()` pins its argument to text at compile time and an attached
        // column is `Any` (a sqlite column can hold any type), so this CHECK
        // does not compile TODAY — the refusal must say which function and why
        // rather than hide the table silently.
        (
            "lenchk",
            "CREATE TABLE chk (id INTEGER PRIMARY KEY, v TEXT CHECK (length(v) < 10))",
            "length",
        ),
        (
            "dyndefault",
            "CREATE TABLE chk (id INTEGER PRIMARY KEY, t TEXT DEFAULT CURRENT_TIMESTAMP)",
            "DEFAULT",
        ),
        (
            "generated",
            "CREATE TABLE chk (id INTEGER PRIMARY KEY, a INT, b INT GENERATED ALWAYS AS (a+1))",
            "GENERATED",
        ),
    ] {
        let p = setup(tag);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(ddl).unwrap();
        drop(c);
        let msg = match SqliteOverlay::open(&p) {
            Ok(_) => panic!("`{tag}` must be refused, not silently half-enforced"),
            Err(e) => format!("{e}"),
        };
        assert!(msg.contains("chk"), "must name the table: {msg}");
        assert!(msg.contains(want), "must name the reason ({want}): {msg}");
    }
    // A LITERAL default is representable and does not trip the refusal.
    let p = setup("litdefault");
    let c = Connection::open(&p).unwrap();
    c.execute_batch("CREATE TABLE ok (id INTEGER PRIMARY KEY, v TEXT DEFAULT 'z', n INT DEFAULT -1)")
        .unwrap();
    drop(c);
    let mut ovl = SqliteOverlay::open(&p).unwrap();
    ovl.query("INSERT INTO ok (id) VALUES (1)", &[]).unwrap();
    let got = rows(ovl.query("SELECT v, n FROM ok", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("z".into()), Value::Int(-1)]]);
}

/// Task #102's acceptance shape: a base whose table carries a CHECK now
/// ATTACHES (it used to refuse by name), and the overlay's write path rejects
/// exactly at INSERT/UPDATE time with sqlite's semantics — only FALSE rejects;
/// TRUE and NULL pass — instead of letting the row in and failing the
/// checkpoint later on an unrelated statement.
#[test]
fn base_checks_compile_and_enforce_at_insert() {
    let p = setup("chk102");
    let c = Connection::open(&p).unwrap();
    c.execute_batch("CREATE TABLE t102 (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0))")
        .unwrap();
    drop(c);

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    let err = ovl.query("INSERT INTO t102 (id, age) VALUES (2, -1)", &[]).unwrap_err();
    assert!(format!("{err}").contains("CHECK"), "{err}");
    assert!(format!("{err}").contains("age >= 0"), "the error names the expression: {err}");
    ovl.query("INSERT INTO t102 (id, age) VALUES (2, 1)", &[]).unwrap();
    // NULL result = PASS (sqlite: only FALSE rejects).
    ovl.query("INSERT INTO t102 (id, age) VALUES (3, NULL)", &[]).unwrap();
    // UPDATE re-evaluates the CHECK against the NEW row.
    let err = ovl.query("UPDATE t102 SET age = -5 WHERE id = 2", &[]).unwrap_err();
    assert!(format!("{err}").contains("CHECK"), "{err}");
    ovl.query("UPDATE t102 SET age = 7 WHERE id = 2", &[]).unwrap();
    // DELETE writes a tombstone (PK + NULLs) — the CHECK must not block it.
    ovl.query("DELETE FROM t102 WHERE id = 3", &[]).unwrap();
    // The refused rows left nothing behind; the accepted update landed.
    let got = rows(ovl.query("SELECT id, age FROM t102", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2), Value::Int(7)]]);
}

/// The enforcement differential: the SAME write statements against the same
/// CHECK-constrained schema, through the overlay on one file and through the
/// sqlite library on a twin — accept/reject must match statement for
/// statement, and the surviving rows must be identical. Covers multi-column
/// CHECKs, quoted identifiers, `typeof()`/`json_valid()`, integer truthiness
/// (`CHECK (n)`), NULL-passes, and sqlite's cross-type comparison (a text
/// value in an INTEGER-affinity column compares greater than any number).
///
/// (`length()` is NOT here: it pins its argument to text at compile time, and
/// an attached column is `Any`, so a base CHECK using it stays a per-table
/// named refusal — see `unrepresentable_constraints_are_refused_by_name`.)
#[test]
fn base_checks_reject_exactly_what_sqlite_rejects() {
    let ddl = "CREATE TABLE d (id INTEGER PRIMARY KEY, \
                 a INTEGER CHECK (\"a\" >= 0), \
                 b INTEGER, \
                 t BLOB CHECK (typeof(t) <> 'text' OR t IS NULL), \
                 j TEXT CHECK ((JSON_VALID(\"j\") OR \"j\" IS NULL)), \
                 n INTEGER CHECK (n), \
                 CHECK (a <= b OR b IS NULL))";
    let p = setup("chkdiff");
    let twin = setup("chkdiff-twin");
    for f in [&p, &twin] {
        let c = Connection::open(f).unwrap();
        c.execute_batch(ddl).unwrap();
        drop(c);
    }

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    let lib = Connection::open(&twin).unwrap();
    for stmt in [
        "INSERT INTO d VALUES (1, 1, 2, 5, '{\"x\": 1}', 5)",
        "INSERT INTO d VALUES (2, -1, 2, 5, NULL, 5)",      // a >= 0 is FALSE
        "INSERT INTO d VALUES (3, NULL, NULL, NULL, NULL, NULL)", // every CHECK is NULL: pass
        "INSERT INTO d VALUES (4, 3, 1, NULL, NULL, 1)",    // a <= b is FALSE
        "INSERT INTO d VALUES (5, 1, NULL, NULL, NULL, 1)", // b IS NULL arm: pass
        "INSERT INTO d VALUES (6, 1, 2, 'txt', NULL, 1)",   // typeof(t) = 'text'
        "INSERT INTO d VALUES (7, 1, 2, NULL, 'not json', 1)", // json_valid = 0
        "INSERT INTO d VALUES (8, 1, 2, NULL, NULL, 0)",    // CHECK (n): 0 is falsy
        "INSERT INTO d VALUES (9, 1, 2, NULL, NULL, 7)",    // CHECK (n): 7 is truthy
        // TEXT in the INTEGER-affinity column: sqlite orders text above every
        // number, so 'abc' >= 0 is TRUE — a pass, not a reject.
        "INSERT INTO d VALUES (10, 'abc', NULL, NULL, NULL, 1)",
        "UPDATE d SET a = -5 WHERE id = 1",
        "UPDATE d SET a = 0 WHERE id = 1",
        "UPDATE d SET t = 'lengthy' WHERE id = 1",
        "DELETE FROM d WHERE id = 3",
    ] {
        let ours = ovl.query(stmt, &[]);
        let theirs = lib.execute(stmt, []);
        assert_eq!(
            ours.is_ok(),
            theirs.is_ok(),
            "`{stmt}`: overlay {ours:?} vs sqlite {theirs:?}"
        );
    }
    // Same survivors, cell for cell (quote() canonicalizes both sides).
    let sel = "SELECT id, quote(a), quote(b), quote(t), quote(j), quote(n) FROM d ORDER BY id";
    let ours: Vec<(i64, String, String, String, String, String)> = rows(ovl.query(sel, &[]).unwrap())
        .into_iter()
        .map(|r| {
            let s = |v: &Value| match v {
                Value::Text(s) => s.clone(),
                other => panic!("quote() must answer text, got {other:?}"),
            };
            let id = match r[0] {
                Value::Int(i) => i,
                ref other => panic!("id must be int, got {other:?}"),
            };
            (id, s(&r[1]), s(&r[2]), s(&r[3]), s(&r[4]), s(&r[5]))
        })
        .collect();
    let theirs: Vec<(i64, String, String, String, String, String)> = lib
        .prepare(sel)
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    assert_eq!(ours, theirs);
}

/// The blocker this task exists for: Django emits a CHECK on every
/// PositiveIntegerField and JSONField (the exact wire shapes from
/// C-API-COMPAT.md), so a real Django base must attach with those CHECKs LIVE.
#[test]
fn django_schema_attaches_with_live_checks() {
    let p = setup("django-chk");
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "CREATE TABLE \"app_item\" (\
           \"id\" integer NOT NULL PRIMARY KEY AUTOINCREMENT, \
           \"age\" integer unsigned NOT NULL CHECK (\"age\" >= 0), \
           \"data\" text NOT NULL CHECK ((JSON_VALID(\"data\") OR \"data\" IS NULL)))",
    )
    .unwrap();
    drop(c);

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    ovl.query("INSERT INTO app_item (id, age, data) VALUES (1, 30, '{\"k\": [1, 2]}')", &[])
        .unwrap();
    let err = ovl
        .query("INSERT INTO app_item (id, age, data) VALUES (2, -1, '{}')", &[])
        .unwrap_err();
    assert!(format!("{err}").contains("CHECK"), "PositiveIntegerField: {err}");
    let err = ovl
        .query("INSERT INTO app_item (id, age, data) VALUES (2, 1, 'not json')", &[])
        .unwrap_err();
    assert!(format!("{err}").contains("CHECK"), "JSONField: {err}");
    let got = rows(ovl.query("SELECT age, data FROM app_item", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(30), Value::Text("{\"k\": [1, 2]}".into())]]);
}
