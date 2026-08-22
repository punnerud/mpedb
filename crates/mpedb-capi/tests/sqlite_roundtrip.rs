//! Opening a real sqlite file, writing to it, and checking point back out.
//!
//! This path had NO Rust test: it was covered only by the PHP CI job, and that
//! job cannot be started from a branch (GitHub refuses `workflow_dispatch` for
//! a workflow absent from the default branch), so the first time it ran it
//! found a bug that had been sitting in a finished-looking commit. Hence this
//! file — the round trip is checkable without PHP, and without a real sqlite,
//! because `mpedb-sqlitefmt` both writes the source and reads the result.

use mpedb_sqlite3::*;
use mpedb_sqlitefmt::{write_image, ImageIndex, ImageTable, SqliteFile, Value as FmtValue};
use std::ffi::CString;
use std::os::raw::c_int;
use std::ptr;

mod common;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// A sqlite file with one table, three rows and one index — no PRIMARY KEY,
/// which is the shape that makes mpedb synthesize a hidden rowid on import.
fn write_source(path: &str) {
    let rows: Vec<Vec<FmtValue>> = [(1i64, "a"), (2, "b"), (3, "c")]
        .iter()
        .map(|(i, s)| vec![FmtValue::Int(*i), FmtValue::Text((*s).into())])
        .collect();
    let img = write_image(
        &[ImageTable {
            name: "p".into(),
            sql: "CREATE TABLE p (id INTEGER, name TEXT)".into(),
            rows,
            indexes: vec![ImageIndex {
                name: "p_id".into(),
                sql: "CREATE INDEX p_id ON p (id)".into(),
                columns: vec![0],
            }],
        }],
        4096,
    )
    .expect("write the source image");
    std::fs::write(path, img).expect("write the source file");
}

unsafe fn exec(db: *mut Sqlite3, sql: &str) {
    let s = cs(sql);
    let rc = sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "exec `{sql}`");
}

/// One integer from a one-row query.
unsafe fn scalar(db: *mut Sqlite3, sql: &str, col: c_int) -> i64 {
    let mut st: *mut Stmt = ptr::null_mut();
    let s = cs(sql);
    let rc = sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "prepare `{sql}`");
    assert_eq!(sqlite3_step(st), SQLITE_ROW, "step `{sql}`");
    let v = sqlite3_column_int64(st, col);
    sqlite3_finalize(st);
    v
}

/// The whole cycle: import, write, check point, and read the file back.
///
/// The assertion that matters is the SCHEMA. A table imported without a
/// primary key gets a synthesized trailing `rowid`, and it used to be written
/// into the checkpointed file as a real column — a third column whose every
/// value was NULL, because the rows come from `SELECT *`, which expands over
/// the visible columns only. Stock sqlite reported it as four
/// `NULL value in p.rowid` lines from `PRAGMA integrity_check` once the column
/// was also marked NOT NULL; before that the file was equally wrong and
/// silent about it.
#[test]
fn a_checkpoint_writes_the_schema_it_imported() {
    let dir = common::scratch_base_str();
    let src = format!("{}/capi-roundtrip-{}.db", dir.trim_end_matches('/'), std::process::id());
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(format!("{src}.mpedb"));
    write_source(&src);

    unsafe {
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let name = cs(&src);
        assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK, "open the sqlite file");

        assert_eq!(scalar(db, "SELECT COUNT(*) FROM p", 0), 3, "the imported rows");
        exec(db, "INSERT INTO p VALUES (4,'d')");
        // Column 2 of `PRAGMA wal_checkpoint` is the row count written.
        assert_eq!(scalar(db, "PRAGMA wal_checkpoint", 2), 4, "rows check pointed");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }

    let f = SqliteFile::open(std::path::Path::new(&src)).expect("reopen the checkpointed file");
    let tables = f.tables().expect("read its schema");
    assert_eq!(tables.len(), 1, "one table");
    let t = &tables[0];
    assert_eq!(
        t.columns,
        vec!["id".to_string(), "name".to_string()],
        "the synthesized rowid must NOT be written as a column"
    );

    let mut got = Vec::new();
    f.scan_table(t, &mut |_rowid, vals| {
        got.push(vals);
        Ok(())
    })
    .expect("scan the rows");
    assert_eq!(got.len(), 4, "three imported rows plus the one written");
    for r in &got {
        assert_eq!(r.len(), 2, "each row carries exactly the declared columns: {r:?}");
    }

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(format!("{src}.mpedb"));
}
