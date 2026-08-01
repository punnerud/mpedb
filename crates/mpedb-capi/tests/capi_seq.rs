//! The synthesised `sqlite_sequence` READ arm (plan §4 step 1), driven through
//! the exported C API exactly as a consumer would.
//!
//! The truth table was measured on stock 3.45.1 and is pinned differentially
//! in `scratchpad` history and in `mpedb/tests/rowid_alias.rs` (the engine
//! half). This file pins the three pieces only the SHIM can get wrong:
//!
//! * existence mirrors the `sqlite_master` LISTING rule — before any
//!   AUTOINCREMENT table, `no such table: sqlite_sequence`, exactly stock;
//! * the mini-evaluator answers a PROJECTED, FILTERED read — `SELECT seq …
//!   WHERE name == ?` is one INTEGER column, `==` included (sqlite treats the
//!   doubled spelling as `=`);
//! * the read goes through the OPEN txn — an id the txn just handed out is
//!   visible before COMMIT (a fresh read snapshot cannot see it).

use mpedb_sqlite3::*;
use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

mod common;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open_memory() -> *mut Sqlite3 {
    let mut db: *mut Sqlite3 = ptr::null_mut();
    let name = cs(":memory:");
    assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut Sqlite3, sql: &str) {
    let s = cs(sql);
    let rc = sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "exec `{sql}`: {}", errmsg(db));
}

unsafe fn errmsg(db: *mut Sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db) as *const c_char).to_str().unwrap().to_string()
}

/// Run a query expected to yield rows; return (column count, rows as i64 of
/// column `col`).
unsafe fn int_col(db: *mut Sqlite3, sql: &str, col: c_int) -> (c_int, Vec<i64>) {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare `{sql}`: {}",
        errmsg(db)
    );
    let mut out = Vec::new();
    let mut ncols = 0;
    while sqlite3_step(st) == SQLITE_ROW {
        ncols = sqlite3_column_count(st);
        out.push(sqlite3_column_int64(st, col));
    }
    sqlite3_finalize(st);
    (ncols, out)
}

#[test]
fn sqlite_sequence_reads_like_stock() {
    unsafe {
        let db = open_memory();

        // Before any AUTOINCREMENT table: stock's exact refusal, not [].
        let s = cs("SELECT * FROM sqlite_sequence");
        let mut st: *mut Stmt = ptr::null_mut();
        let mut rc = sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut());
        if rc == SQLITE_OK {
            rc = sqlite3_step(st);
            sqlite3_finalize(st);
        }
        assert_ne!(rc, SQLITE_ROW, "a fresh database has no sqlite_sequence");
        assert_ne!(rc, SQLITE_DONE, "a fresh database has no sqlite_sequence");
        assert!(
            errmsg(db).contains("no such table: sqlite_sequence"),
            "got: {}",
            errmsg(db)
        );

        // Created but no id handed out: the table EXISTS and is empty.
        exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)");
        let (_, rows) = int_col(db, "SELECT * FROM sqlite_sequence", 1);
        assert_eq!(rows, Vec::<i64>::new());

        // Explicit ids move the counter (never down): 9 then 5 records 9.
        exec(db, "INSERT INTO t VALUES (9, 'a')");
        exec(db, "INSERT INTO t VALUES (5, 'b')");
        let (ncols, rows) = int_col(db, "SELECT seq FROM sqlite_sequence WHERE name == 't'", 0);
        assert_eq!((ncols, rows), (1, vec![9]), "projected + `==`-filtered read");

        // Mid-txn: the id this OPEN txn handed out is already visible.
        exec(db, "BEGIN");
        exec(db, "INSERT INTO t VALUES (NULL, 'c')");
        let (_, rows) = int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 't'", 0);
        assert_eq!(rows, vec![10], "the txn's own auto-assign must be visible");
        exec(db, "COMMIT");

        // A filter that misses answers zero rows, not an error.
        let (_, rows) = int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 'zz'", 0);
        assert_eq!(rows, Vec::<i64>::new());

        sqlite3_close(db);
    }
}

/// The WRITE arm (plan §4 step 2): stock keeps `sqlite_sequence` as an
/// ordinary table, and the consumer forms map onto the catalog's counters —
/// every fact below is the measured stock 3.45.1 answer. Django's flush
/// reset is the governing consumer: it used to RAISE `no such table` here.
#[test]
fn sqlite_sequence_writes_move_the_counters() {
    unsafe {
        let db = open_memory();
        exec(db, "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)");
        exec(db, "CREATE TABLE b (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)");
        for t in ["a", "b"] {
            for _ in 0..3 {
                exec(db, &format!("INSERT INTO {t} (v) VALUES ('x')"));
            }
            exec(db, &format!("DELETE FROM {t}"));
        }

        // Django's flush form: both counters reset in one statement, the
        // changed-row count is stock's (2), and the next ids restart at 1.
        exec(db, "UPDATE sqlite_sequence SET seq = 0 WHERE name IN ('a', 'b')");
        assert_eq!(sqlite3_changes(db), 2);
        exec(db, "INSERT INTO a (v) VALUES ('p')");
        assert_eq!(int_col(db, "SELECT max(id) FROM a", 0).1, vec![1]);

        // A WHERE that matches nothing is a SILENT no-op with 0 changes —
        // sqlite_sequence is just a table, no name validation.
        exec(db, "UPDATE sqlite_sequence SET seq = 9 WHERE name = 'nope'");
        assert_eq!(sqlite3_changes(db), 0);

        // Seek forward; the next allocation is seq + 1.
        exec(db, "UPDATE sqlite_sequence SET seq = 5 WHERE name = 'b'");
        exec(db, "INSERT INTO b (v) VALUES ('p')");
        assert_eq!(int_col(db, "SELECT max(id) FROM b", 0).1, vec![6]);

        // A LOW seq is stored verbatim (the read shows it, like stock) and
        // allocation silently corrects past the live max — never a collision.
        exec(db, "UPDATE sqlite_sequence SET seq = 1 WHERE name = 'b'");
        assert_eq!(int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 'b'", 0).1, vec![1]);
        exec(db, "INSERT INTO b (v) VALUES ('q')");
        assert_eq!(int_col(db, "SELECT max(id) FROM b", 0).1, vec![7]);

        // DELETE removes the record, which resets the sequence; the row is
        // recreated at the next allocation, exactly stock's lifecycle.
        exec(db, "DELETE FROM sqlite_sequence WHERE name = 'a'");
        assert_eq!(sqlite3_changes(db), 1);
        assert_eq!(int_col(db, "SELECT count(*) FROM sqlite_sequence WHERE name = 'a'", 0).1, vec![0]);
        exec(db, "DELETE FROM a");
        exec(db, "INSERT INTO a (v) VALUES ('r')");
        assert_eq!(int_col(db, "SELECT max(id) FROM a", 0).1, vec![1]);

        // Pre-seeding a VIRGIN table's counter via INSERT is honored.
        exec(db, "CREATE TABLE c2 (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)");
        exec(db, "INSERT INTO sqlite_sequence (name, seq) VALUES ('c2', 50)");
        exec(db, "INSERT INTO c2 (v) VALUES ('p')");
        assert_eq!(int_col(db, "SELECT max(id) FROM c2", 0).1, vec![51]);

        // INSERT for a name that already has a row: stock's allocation-inert
        // duplicate — counted as 1 change, counter unchanged.
        exec(db, "INSERT INTO sqlite_sequence VALUES ('b', 99)");
        assert_eq!(sqlite3_changes(db), 1);
        assert_eq!(int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 'b'", 0).1, vec![7]);

        // The write rides the OPEN transaction: visible inside, gone on
        // rollback — a flush's resets are atomic with its deletes.
        exec(db, "BEGIN");
        exec(db, "UPDATE sqlite_sequence SET seq = 200 WHERE name = 'b'");
        assert_eq!(int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 'b'", 0).1, vec![200]);
        exec(db, "ROLLBACK");
        assert_eq!(int_col(db, "SELECT seq FROM sqlite_sequence WHERE name = 'b'", 0).1, vec![7]);

        // Named refusals — never an approximation: renames manufacture the
        // duplicates stock then ignores, and an orphan row has nowhere to
        // live in a catalog-backed table.
        for bad in [
            "UPDATE sqlite_sequence SET name = 'z' WHERE name = 'b'",
            "INSERT INTO sqlite_sequence VALUES ('unknown_table', 5)",
            "UPDATE sqlite_sequence SET seq = NULL WHERE name = 'b'",
        ] {
            let s = cs(bad);
            let rc = sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
            assert_ne!(rc, SQLITE_OK, "must refuse: {bad}");
        }

        sqlite3_close(db);

        // Existence mirrors the listing rule: no AUTOINCREMENT table, no
        // sqlite_sequence — writes refuse like reads do, stock's words.
        let db = open_memory();
        exec(db, "CREATE TABLE plain (i INTEGER)");
        let s = cs("UPDATE sqlite_sequence SET seq = 0");
        let rc = sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
        assert_ne!(rc, SQLITE_OK);
        assert!(errmsg(db).contains("no such table: sqlite_sequence"), "{}", errmsg(db));
        sqlite3_close(db);
    }
}
