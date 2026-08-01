use super::*;
use super::helpers::*;

/// Refusal stubs must leave the refusal ON THE HANDLE (backup: the
/// destination), or CPython raises bare SystemError.
#[test]
fn refusal_stubs_set_handle_error() {
    unsafe {
        let dst = open_memory();
        let src = open_memory();
        let main = cs("main");
        // The backup API is REAL now; what still refuses is a schema name mpedb
        // has no equivalent for, and the refusal must land on the DESTINATION.
        // (`temp` is no longer one of those — see
        // `backup_of_the_temp_schema_is_an_empty_database`.)
        let other = cs("attached_db");
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, other.as_ptr()).is_null());
        let msg = CStr::from_ptr(sqlite3_errmsg(dst)).to_string_lossy().into_owned();
        assert_eq!(msg, "unknown database attached_db", "backup errmsg {msg:?}");

        // Incremental blob I/O is REAL now (see the blob_* tests below); what
        // this asserts is only that a FAILING open still leaves its reason on
        // the handle rather than a bare non-OK code.
        let mut blob: *mut c_void = ptr::null_mut();
        let t = cs("t");
        assert_eq!(
            sqlite3_blob_open(src, main.as_ptr(), t.as_ptr(), t.as_ptr(), 1, 0, &mut blob),
            SQLITE_ERROR
        );
        assert!(blob.is_null(), "a failed open must leave *ppBlob NULL");
        let msg = CStr::from_ptr(sqlite3_errmsg(src)).to_string_lossy().into_owned();
        assert_eq!(msg, "no such table: main.t", "blob errmsg {msg:?}");
        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// Every `blob_open` refusal shape, with sqlite's exact code and message.
#[test]
fn blob_open_error_shapes_match_sqlite() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB, c TEXT, d REAL, e INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "create index idx_c on t(c)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'00010203', 'hello', 1.5, 42)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t (a, b) values (3, NULL)"), SQLITE_OK);

        // Unknown database/table/column, and a missing row.
        assert_eq!(
            blob_open_err(db, "nope", "t", "b", 1, 0),
            (SQLITE_ERROR, "no such table: nope.t".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "nope", "b", 1, 0),
            (SQLITE_ERROR, "no such table: main.nope".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "nope", 1, 0),
            (SQLITE_ERROR, "no such column: \"nope\"".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "b", 999, 0),
            (SQLITE_ERROR, "no such rowid: 999".into())
        );
        // A value that is not text/blob names its own type, NULL included.
        assert_eq!(
            blob_open_err(db, "main", "t", "e", 1, 0),
            (SQLITE_ERROR, "cannot open value of type integer".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "d", 1, 0),
            (SQLITE_ERROR, "cannot open value of type real".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "b", 3, 0),
            (SQLITE_ERROR, "cannot open value of type null".into())
        );
        // Read-WRITE on an indexed or primary-key column is refused; the same
        // column opens fine read-ONLY.
        assert_eq!(
            blob_open_err(db, "main", "t", "c", 1, 1),
            (SQLITE_ERROR, "cannot open indexed column for writing".into())
        );
        let ro = blob_open_ok(db, "t", "c", 1, 0);
        assert_eq!(sqlite3_blob_bytes(ro), 5); // 'hello' — TEXT opens as bytes
        assert_eq!(sqlite3_blob_close(ro), SQLITE_OK);

        // The schema check precedes the row fetch: a missing row still reports
        // the indexed-column refusal (sqlite's order).
        assert_eq!(
            blob_open_err(db, "main", "t", "c", 999, 1),
            (SQLITE_ERROR, "cannot open indexed column for writing".into())
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The size is FIXED at open: out-of-range reads/writes are SQLITE_ERROR and a
/// write can never grow the value. A zero-length I/O at the very end is OK.
#[test]
fn blob_io_bounds_are_the_open_time_size() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'00010203040506070809')"), SQLITE_OK);
        let b = blob_open_ok(db, "t", "b", 1, 1);
        assert_eq!(sqlite3_blob_bytes(b), 10);

        // In range.
        let (rc, got) = blob_read_vec(b, 4, 6);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(got, vec![0x06, 0x07, 0x08, 0x09]);

        // One byte past the end, a negative count, a negative offset.
        for (n, off) in [(4, 7), (11, 0), (-1, 0), (1, -1), (1, 10)] {
            let mut buf = [0u8; 16];
            assert_eq!(
                sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, n, off),
                SQLITE_ERROR,
                "read(n={n}, off={off}) must be out of range"
            );
            assert_eq!(errmsg(db), "SQL logic error");
        }
        // A ZERO-length read at exactly the end is fine (sqlite: probed).
        let mut none = [0u8; 1];
        assert_eq!(sqlite3_blob_read(b, none.as_mut_ptr() as *mut c_void, 0, 10), SQLITE_OK);

        // Writes obey the same bounds and never grow the value.
        let data = [0xAAu8, 0xBB];
        assert_eq!(
            sqlite3_blob_write(b, data.as_ptr() as *const c_void, 2, 9),
            SQLITE_ERROR,
            "a write may not extend the blob"
        );
        assert_eq!(sqlite3_blob_write(b, data.as_ptr() as *const c_void, 2, 8), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(b), 10, "size is fixed at open");
        let (rc, got) = blob_read_vec(b, 2, 8);
        assert_eq!((rc, got), (SQLITE_OK, vec![0xAA, 0xBB]), "own write reads back");
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // …and it landed in the row, with the length unchanged.
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("select hex(b) from t where a = 1");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 0), "0001020304050607AABB");
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// A read-only handle refuses `blob_write` with SQLITE_READONLY, and the
/// refusal does not kill the handle (reads keep working).
#[test]
fn blob_write_on_readonly_handle_is_readonly() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);
        let b = blob_open_ok(db, "t", "b", 1, 0);
        let d = [0x99u8];
        assert_eq!(sqlite3_blob_write(b, d.as_ptr() as *const c_void, 1, 0), SQLITE_READONLY);
        assert_eq!(errmsg(db), "attempt to write a readonly database");
        // Still alive.
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The expiry rule under mpedb's MVCC (see `blob.rs`): a row modified or
/// deleted under a handle makes every later call SQLITE_ABORT, permanently —
/// `reopen` on a dead handle stays ABORT, exactly as sqlite does.
#[test]
fn blob_handle_expires_when_its_row_changes() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB, c TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102', 'x'), (2, x'0304', 'y')"), SQLITE_OK);

        // A write to a DIFFERENT row leaves the handle alone.
        let b = blob_open_ok(db, "t", "b", 1, 1);
        assert_eq!(exec(db, "update t set c = 'zz' where a = 2"), SQLITE_OK);
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));

        // A write to ANOTHER COLUMN of the handle's own row expires it.
        assert_eq!(exec(db, "update t set c = 'changed' where a = 1"), SQLITE_OK);
        let (rc, _) = blob_read_vec(b, 1, 0);
        assert_eq!(rc, SQLITE_ABORT);
        assert_eq!(errmsg(db), "query aborted");
        // Dead is dead: bytes reads 0, write and reopen both stay ABORT.
        assert_eq!(sqlite3_blob_bytes(b), 0);
        let d = [0u8];
        assert_eq!(sqlite3_blob_write(b, d.as_ptr() as *const c_void, 1, 0), SQLITE_ABORT);
        assert_eq!(sqlite3_blob_reopen(b, 2), SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // DELETE expires a handle too.
        let b2 = blob_open_ok(db, "t", "b", 2, 0);
        assert_eq!(exec(db, "delete from t where a = 2"), SQLITE_OK);
        assert_eq!(blob_read_vec(b2, 1, 0).0, SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(b2), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// A write THROUGH a blob handle expires nothing — not the writer, and not a
/// same-row sibling handle on this connection (sqlite parity, probed).
#[test]
fn blob_write_does_not_expire_sibling_handles() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);
        let a = blob_open_ok(db, "t", "b", 1, 1);
        let b = blob_open_ok(db, "t", "b", 1, 1);
        let d = [0x51u8];
        assert_eq!(sqlite3_blob_write(a, d.as_ptr() as *const c_void, 1, 0), SQLITE_OK);
        // Both handles are alive, and the sibling sees the new byte.
        assert_eq!(blob_read_vec(a, 1, 0), (SQLITE_OK, vec![0x51]));
        assert_eq!(blob_read_vec(b, 1, 0), (SQLITE_OK, vec![0x51]));
        assert_eq!(sqlite3_blob_close(a), SQLITE_OK);
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// `sqlite3_blob_reopen` moves a live handle to another row of the same
/// table/column, re-reading the size; a FAILED reopen kills the handle.
#[test]
fn blob_reopen_moves_the_row_and_a_failure_kills_the_handle() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(
            exec(db, "insert into t values (1, x'0102'), (2, x'030405'), (4, NULL)"),
            SQLITE_OK
        );
        let h = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_blob_bytes(h), 2);
        assert_eq!(sqlite3_blob_reopen(h, 2), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(h), 3, "size follows the new row");
        assert_eq!(blob_read_vec(h, 3, 0), (SQLITE_OK, vec![0x03, 0x04, 0x05]));

        // Reopen onto a missing rowid: SQLITE_ERROR with the open-shape
        // message, and the handle is finalized (everything after is ABORT).
        assert_eq!(sqlite3_blob_reopen(h, 777), SQLITE_ERROR);
        assert_eq!(errmsg(db), "no such rowid: 777");
        assert_eq!(blob_read_vec(h, 1, 0).0, SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(h), SQLITE_OK);

        // Reopen onto a NULL value fails the same way.
        let h2 = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_blob_reopen(h2, 4), SQLITE_ERROR);
        assert_eq!(errmsg(db), "cannot open value of type null");
        assert_eq!(sqlite3_blob_close(h2), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The two closes differ when a blob handle is still open, and the shim
/// follows sqlite on both (probed on 3.45.1): `sqlite3_close` refuses with
/// SQLITE_BUSY, while `sqlite3_close_v2` succeeds and leaves a ZOMBIE — the
/// connection stays alive for the handle, which keeps working, and is freed by
/// the last `sqlite3_blob_close`.
#[test]
fn close_with_an_open_blob_handle_is_busy_but_close_v2_zombies() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);

        // v1: refused, and the connection is untouched — the handle and the
        // connection both still work.
        let b = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_close(db), SQLITE_BUSY);
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // v2 with a live handle: OK, and the handle keeps reading afterwards.
        let b2 = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_close_v2(db), SQLITE_OK);
        assert_eq!(
            blob_read_vec(b2, 2, 0),
            (SQLITE_OK, vec![0x01, 0x02]),
            "a zombie connection still serves its outstanding blob handle"
        );
        // This free is the connection's too; ASAN/valgrind would catch a leak
        // or a double free here.
        assert_eq!(sqlite3_blob_close(b2), SQLITE_OK);
    }
}


/// A blob write that would leave a TEXT value invalid UTF-8 is refused rather
/// than silently restyling the cell as a blob: mpedb text is strictly UTF-8,
/// so this is a documented divergence from sqlite (which stores raw bytes).
#[test]
fn blob_write_refuses_to_break_a_text_value() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, c TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, 'hello')"), SQLITE_OK);
        let h = blob_open_ok(db, "t", "c", 1, 1);
        // ASCII in, ASCII out: fine.
        let ok = *b"H";
        assert_eq!(sqlite3_blob_write(h, ok.as_ptr() as *const c_void, 1, 0), SQLITE_OK);
        // A lone continuation byte is not valid UTF-8: refused, value intact.
        let bad = [0xFFu8];
        assert_eq!(sqlite3_blob_write(h, bad.as_ptr() as *const c_void, 1, 0), SQLITE_ERROR);
        assert!(errmsg(db).contains("UTF-8"), "errmsg {:?}", errmsg(db));
        assert_eq!(blob_read_vec(h, 5, 0), (SQLITE_OK, b"Hello".to_vec()));
        assert_eq!(sqlite3_blob_close(h), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// `sqlite3_bind_zeroblob` binds N zero bytes, and refuses past
/// SQLITE_MAX_LENGTH with SQLITE_TOOBIG (sqlite's cap; mpedb materializes the
/// zeros, so the cap is also the memory guard).
#[test]
fn bind_zeroblob_binds_zeros_and_caps() {
    unsafe {
        let db = open_memory();
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("select ?, typeof(?)");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_bind_zeroblob(st, 1, 5), SQLITE_OK);
        // A negative length is an empty blob, as sqlite does.
        assert_eq!(sqlite3_bind_zeroblob(st, 2, -3), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(st, 0), SQLITE_BLOB);
        assert_eq!(sqlite3_column_bytes(st, 0), 5);
        let p = sqlite3_column_blob(st, 0) as *const u8;
        assert_eq!(std::slice::from_raw_parts(p, 5), &[0u8; 5]);
        assert_eq!(col_text(st, 1), "blob");
        // Past the cap.
        assert_eq!(sqlite3_bind_zeroblob(st, 1, 1_000_000_001), SQLITE_TOOBIG);
        assert_eq!(sqlite3_bind_zeroblob64(st, 1, 1_000_000_001), SQLITE_TOOBIG);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
