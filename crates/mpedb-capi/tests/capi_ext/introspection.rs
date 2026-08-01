use super::*;
use super::helpers::*;

/// The hidden implicit rowid (#94) must be invisible to INTROSPECTION, not
/// just to `SELECT *`: `PRAGMA table_info` listed it and the reconstructed
/// `sqlite_master.sql` declared it, so a consumer that rebuilds a schema or a
/// column list from either (CPython's `iterdump`) got a column the caller
/// never wrote — and a dump that replayed as a different table.
#[test]
fn introspection_hides_the_implicit_rowid() {
    unsafe {
        let db = open_memory();
        // No PRIMARY KEY: the engine synthesizes a hidden rowid.
        assert_eq!(exec(db, "CREATE TABLE \"alpha\" (\"one\")"), SQLITE_OK);
        // With one: nothing is hidden and the PK is real.
        assert_eq!(exec(db, "CREATE TABLE beta (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);

        // One row (cid 0) — the hidden rowid is not a seventh column.
        assert_eq!(collect_text_col(db, "PRAGMA table_info(\"alpha\")"), ["0"]);
        assert_eq!(collect_text_col(db, "PRAGMA table_info(beta)"), ["0", "1"]);

        // Both come back as the caller wrote them (the verbatim record), which
        // is what sqlite hands back and never mentions a rowid either.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'alpha'"),
            ["CREATE TABLE \"alpha\" (\"one\")"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'beta'"),
            ["CREATE TABLE beta (id INTEGER PRIMARY KEY, v TEXT)"]
        );

        // A RENAME carries the recorded text over, retargeted — which for this
        // already-quoted statement is character-for-character what the
        // reconstruction would have produced anyway. Either way the hidden
        // rowid must stay elided, which is the path this test is about.
        // (`rename_carries_the_verbatim_ddl_like_sqlite` covers the case where
        // the two DIFFER.)
        assert_eq!(exec(db, "ALTER TABLE \"alpha\" RENAME TO alpha2"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'alpha2'"),
            ["CREATE TABLE \"alpha2\" (\"one\")"]
        );
        assert_eq!(collect_text_col(db, "PRAGMA table_info(alpha2)"), ["0"]);
        // Same for a shape change under an unchanged name: `beta` grows a
        // column, and the recorded text no longer describes it.
        assert_eq!(exec(db, "ALTER TABLE beta ADD COLUMN w TEXT"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'beta'"),
            ["CREATE TABLE \"beta\" (\"id\" INTEGER NOT NULL, \"v\" TEXT, \"w\" TEXT, PRIMARY KEY (\"id\"))"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// `sqlite_master.sql` is the caller's OWN `CREATE TABLE`, byte for byte —
/// sqlite stores the statement text, and consumers diff against it (CPython's
/// `test_dump_custom_row_factory` asserts `iterdump()` re-emits
/// `CREATE TABLE test(t);` exactly). mpedb's catalog keeps the resolved schema
/// rather than the bytes, so the shim files the text in the catalog's
/// sys-keyspace and hands it back — but ONLY while it still describes this
/// exact shape, because an almost-right `CREATE TABLE` replays as a DIFFERENT
/// table.
#[test]
fn sqlite_master_returns_the_verbatim_create_table() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE test(t);"), SQLITE_OK);
        // The trailing `;` is not part of what sqlite stores.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'test'"),
            ["CREATE TABLE test(t)"]
        );

        // sqlite does NOT store the raw bytes: it rebuilds the head as the
        // literal `CREATE TABLE ` and keeps the text from the NAME token on.
        // So the head is uppercased and re-spaced while the tail is verbatim,
        // and a trailing comment (not a token) is dropped. All four verified
        // against sqlite 3.45 — see `introspect::ddl_verbatim`.
        assert_eq!(
            exec(db, "-- lead\n  create   table   spaced ( a  int ) ; -- trail"),
            SQLITE_OK
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'spaced'"),
            ["CREATE TABLE spaced ( a  int )"]
        );
        // A `;` inside a string literal is not a terminator, and the text runs
        // to the last real token past it.
        assert_eq!(exec(db, "CREATE TABLE semi (a TEXT DEFAULT 'x;y')"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'semi'"),
            ["CREATE TABLE semi (a TEXT DEFAULT 'x;y')"]
        );

        // A DROP forgets the text: a table recreated by some other route (or
        // in another process) must not inherit the old spelling.
        assert_eq!(exec(db, "DROP TABLE test"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE \"test\" (\"t\")"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'test'"),
            ["CREATE TABLE \"test\" (\"t\")"]
        );

        // A `CREATE TABLE` inside an open transaction IS recorded: the shim
        // fingerprints against the WriteSession schema and the sys-record
        // rides the same txn (CPython `test_table_dump` / iterdump mid-session).
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE intxn (a int NOT NULL, b TEXT)"), SQLITE_OK);
        // Visible mid-transaction with the caller's own text.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'intxn'"),
            ["CREATE TABLE intxn (a int NOT NULL, b TEXT)"]
        );
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'intxn'"),
            ["CREATE TABLE intxn (a int NOT NULL, b TEXT)"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// VIEW/TRIGGER `sqlite_master.sql` is also the caller's own text (CPython
/// `test_table_dump` asserts spelling of both, not a reconstruction).
#[test]
fn sqlite_master_returns_verbatim_create_view_and_trigger() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t1(id integer primary key, t1_i1 integer, i2 integer)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "CREATE TABLE t2(id integer primary key, t2_i1 integer, t2_i2 integer)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(
                db,
                "CREATE TRIGGER trigger_1 update of t1_i1 on t1 begin \
                 update t2 set t2_i1 = new.t1_i1 where t2_i1 = old.t1_i1; end;"
            ),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "CREATE VIEW v1 as select * from t1 left join t2 using (id);"),
            SQLITE_OK
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'v1'"),
            ["CREATE VIEW v1 as select * from t1 left join t2 using (id)"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'trigger_1'"),
            ["CREATE TRIGGER trigger_1 update of t1_i1 on t1 begin \
              update t2 set t2_i1 = new.t1_i1 where t2_i1 = old.t1_i1; end"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// CPython `test_table_dump` / `iterdump`: (1) `PRAGMA table_info("quoted""table")`
/// must un-escape doubled quotes; (2) mid-transaction `CREATE TABLE` (Python's
/// default isolation starts a txn on INSERT) must still answer `table_info`
/// from the open WriteSession so the dump does not emit bare `VALUES()`.
#[test]
fn table_info_unescapes_quotes_and_sees_mid_txn_creates() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, r#"CREATE TABLE "quoted""table"("quoted""field" text)"#),
            SQLITE_OK
        );
        assert_eq!(
            collect_pragma_col_names(db, r#""quoted""table""#),
            [r#"quoted"field"#],
            "doubled quotes in table_info arg must resolve"
        );
        // Mid-txn create after an INSERT (mirrors CPython isolation).
        assert_eq!(exec(db, "CREATE TABLE seed(x)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO seed VALUES (1)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE later(y TEXT)"), SQLITE_OK);
        let cols = collect_pragma_col_names(db, "later");
        assert_eq!(cols, ["y"], "mid-txn table_info saw {cols:?}");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The `sqlite_master` mini-evaluator has to survive the shapes real
/// consumers write, not just single-line ones: CPython's `iterdump` breaks its
/// query across lines, quotes every identifier, uses `==`, and tests
/// `"sql" NOT NULL`.
#[test]
fn sqlite_master_evaluator_takes_the_iterdump_query_shape() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE beta (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE alpha (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        let q = "
        SELECT \"name\"
        FROM \"sqlite_master\"
            WHERE \"sql\" NOT NULL AND
            \"type\" == 'table'
            ORDER BY \"name\"
        ";
        assert_eq!(collect_text_col(db, q), ["alpha", "beta"]);
        // Descending, and on a column other than `name`.
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master ORDER BY \"name\" DESC"),
            ["beta", "alpha"]
        );
        // `IS NULL` is the other half of the NULL test, and matches nothing:
        // every row this shim emits carries its DDL.
        assert!(collect_text_col(db, "SELECT name FROM sqlite_master WHERE sql IS NULL").is_empty());
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
                .len(),
            2
        );
        // A shape it cannot evaluate REFUSES rather than answering wrongly.
        let s = cs("SELECT name FROM sqlite_master ORDER BY nosuchcol");
        let mut st: *mut Stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut());
        if rc == SQLITE_OK {
            assert_ne!(sqlite3_step(st), SQLITE_ROW, "an unevaluable ORDER BY must not answer");
            sqlite3_finalize(st);
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// A consumer could not create a TRIGGER through this API at all: the
/// statement splitter cut the body at its first `;`, so `execute` reported
/// "you can only execute one statement at a time". End to end, through
/// prepare/step, including the trigger actually firing.
#[test]
fn a_trigger_can_be_created_and_fires() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t1 (a INTEGER PRIMARY KEY, b INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t2 (a INTEGER PRIMARY KEY, b INTEGER)"), SQLITE_OK);

        // No BEFORE/AFTER word: sqlite's documented default is BEFORE.
        let ddl = "CREATE TRIGGER tr UPDATE OF b ON t1 \
                   BEGIN UPDATE t2 SET b = new.b WHERE a = old.a; END;";
        let s = cs(ddl);
        let mut st: *mut Stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, &mut tail), SQLITE_OK);
        // The whole trigger is ONE statement: nothing is left over.
        assert!(tail.is_null() || CStr::from_ptr(tail).to_bytes().is_empty(), "tail must be empty");
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        assert_eq!(exec(db, "INSERT INTO t2 (a, b) VALUES (1, 0)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t1 (a, b) VALUES (1, 0)"), SQLITE_OK);
        assert_eq!(exec(db, "UPDATE t1 SET b = 42 WHERE a = 1"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT b FROM t2 WHERE a = 1"), 42);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
