use super::*;
use super::helpers::*;

/// The three corners CPython's suite does not reach:
/// 1. a SUCCESSFUL `OR ROLLBACK` leaves the transaction alone;
/// 2. a conflicting one discards work done EARLIER in the same transaction;
/// 3. a NON-constraint failure of the same statement does not roll back —
///    sqlite's action fires on conflict resolution, not on any error.
#[test]
fn insert_or_rollback_aborts_the_transaction_only_on_a_conflict() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE)"), SQLITE_OK);

        // (1) No conflict: the row stands and so does the transaction.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (1, 'a')"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES (2, 'b')"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0, "still inside the transaction");
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 2);

        // (2) A conflict discards the whole transaction, including the row
        // inserted by an EARLIER statement in it.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (3, 'c')"), SQLITE_OK);
        assert_eq!(
            exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES (4, 'a')"),
            SQLITE_CONSTRAINT
        );
        assert_eq!(sqlite3_get_autocommit(db), 1, "the transaction is gone");
        // Row 3 never happened; the pre-transaction rows are intact.
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 2);

        // (3) A type error is not a conflict: the transaction survives it,
        // exactly as `OR ABORT` would.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (5, 'e')"), SQLITE_OK);
        assert_ne!(exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES ('x', 'f')"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0, "a non-conflict error keeps the transaction");
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 3);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// Comments are the parser's business now, at every position — not just the
/// leading one the shim strips. A `;` or a parameter marker inside a comment
/// must not be seen by the statement splitter or the bind-parameter scanner
/// either, and `==` is `=`.
#[test]
fn interior_comments_and_eq_alias_reach_the_engine() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, v) VALUES (1, 10), (2, 20)"), SQLITE_OK);

        assert_eq!(scalar_count(db, "SELECT v FROM t WHERE id == 1"), 10);
        assert_eq!(scalar_count(db, "SELECT v FROM t -- trailing comment"), 10);
        assert_eq!(scalar_count(db, "SELECT /* inline */ v FROM t WHERE id = 2"), 20);
        // A `;` inside a comment is not a statement boundary.
        assert_eq!(scalar_count(db, "SELECT v FROM t WHERE id = 1 -- ; SELECT 99"), 10);
        // A `?` inside a comment is not a bound parameter.
        let s = cs("SELECT v FROM t WHERE id = ? /* not a ? here */");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_bind_parameter_count(st), 1);
        assert_eq!(sqlite3_bind_int(st, 1, 2), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 20);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // An unquoted identifier may carry bytes >= 0x80.
        let s = cs("SELECT 1 AS \u{00ff}");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(
            CStr::from_ptr(sqlite3_column_name(st, 0)).to_str().unwrap(),
            "\u{00ff}"
        );
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The action stream for the write statements CPython's own authorizer tests
/// never reach, and the DENY message shape for each: a denied column read
/// names the object ("access to t.c is prohibited"), everything else is
/// sqlite's generic "not authorized". Both carry SQLITE_AUTH.
#[test]
fn authorizer_sees_writes_and_ddl_and_denies_with_sqlites_two_messages() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, a, b) VALUES (1, 2, 'x')"), SQLITE_OK);
        set_auth(db);

        // INSERT names its table; nothing is read.
        auth_reset(None);
        assert_eq!(exec(db, "INSERT INTO t (id, a, b) VALUES (2, 3, 'y')"), SQLITE_OK);
        assert_eq!(auth_log(), [(18, "t".to_string(), String::new())]);

        // UPDATE names the ASSIGNED column, and reads the ones it consults.
        auth_reset(None);
        assert_eq!(exec(db, "UPDATE t SET b = 'z' WHERE a = 3"), SQLITE_OK);
        let log = auth_log();
        assert!(log.contains(&(23, "t".into(), "b".into())), "{log:?}");
        assert!(!log.contains(&(23, "t".into(), "a".into())), "a is read, not written: {log:?}");
        assert!(log.contains(&(20, "t".into(), "a".into())), "{log:?}");

        // DELETE names its table.
        auth_reset(None);
        assert_eq!(exec(db, "DELETE FROM t WHERE id = 2"), SQLITE_OK);
        assert!(auth_log().contains(&(9, "t".into(), String::new())));

        // DDL and transaction control are described too.
        auth_reset(None);
        assert_eq!(exec(db, "CREATE TABLE u (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(auth_log(), [(2, "u".to_string(), String::new())]);
        auth_reset(None);
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(auth_log(), [(22, "BEGIN".to_string(), String::new())]);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);

        // DENY on a column read: sqlite's object-naming message.
        auth_reset(Some((20, SQLITE_DENY)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "access to t.b is prohibited");

        // DENY on anything else: the generic message.
        auth_reset(Some((18, SQLITE_DENY)));
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (9)"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "not authorized");
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t WHERE id = 9"), 0);

        // A verdict outside {OK, DENY, IGNORE} is sqlite's malfunction.
        auth_reset(Some((21, 42)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_ERROR);
        assert_eq!(errmsg(db), "authorizer malfunction");

        // SQLITE_IGNORE means "read this column as NULL"; mpedb has no plan
        // rewrite for that, so it refuses rather than handing back the value
        // the callback asked to hide.
        auth_reset(Some((20, SQLITE_IGNORE)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_ERROR);
        assert!(errmsg(db).contains("SQLITE_IGNORE"), "{}", errmsg(db));
        assert!(errmsg(db).contains("NULL"), "{}", errmsg(db));

        // Clearing restores the ungated connection, and the callback stops
        // being consulted at all.
        assert_eq!(sqlite3_set_authorizer(db, ptr::null_mut(), ptr::null_mut()), SQLITE_OK);
        auth_reset(Some((20, SQLITE_DENY)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_OK);
        assert!(auth_log().is_empty());

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
