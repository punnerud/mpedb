use super::*;
use super::helpers::*;

// ---------------------------------------------------------- index catalog (#119)

/// Every row of `sql` as `"<typename>:<text>"`, so a test asserts the value AND
/// the storage class — a NULL `sqlite_master.sql` is what tells a consumer
/// "constraint index", and `''` would be a different answer, not a near one.
unsafe fn typed_rows(db: *mut Sqlite3, sql: &str) -> Vec<String> {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare {sql}: {}",
        errmsg(db)
    );
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let mut cells = Vec::new();
        for i in 0..sqlite3_column_count(st) {
            let ty = sqlite3_column_type(st, i);
            let tyname = match ty {
                SQLITE_NULL => "null",
                SQLITE_INTEGER => "int",
                SQLITE_FLOAT => "real",
                SQLITE_TEXT => "text",
                _ => "blob",
            };
            let p = sqlite3_column_text(st, i);
            let txt = if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string()
            };
            cells.push(format!("{tyname}:{txt}"));
        }
        out.push(cells.join("|"));
    }
    sqlite3_finalize(st);
    out
}

/// `sqlite_master` carries INDEX rows, with the caller's own `CREATE INDEX`
/// text — the half of the catalog that used to be missing entirely, and the
/// reason a dump replayed into a schema without its indexes.
///
/// Every expectation is the bundled 3.45.0 oracle's own answer to the same
/// script (`crates/mpedb/tests/sqlite_oracle`), including the two rules that
/// are NOT guessable and differ from `CREATE TABLE`:
///
/// * a `CREATE INDEX`'s stored text runs to just before the `;`, so trailing
///   whitespace and interior comments survive where a table's do not;
/// * `UNIQUE` belongs to the head sqlite rebuilds, so it comes back uppercased.
///
/// A constraint index (`UNIQUE`/`PRIMARY KEY` inside the `CREATE TABLE`) is
/// named `sqlite_autoindex_<table>_<k>` and its `sql` is NULL, exactly as
/// sqlite reports it.
#[test]
fn sqlite_master_lists_indexes_with_the_callers_own_text() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE INDEX ix_b ON t(b)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE UNIQUE INDEX ux_a ON t(a)"), SQLITE_OK, "{}", errmsg(db));
        // Head normalized and uppercased, tail verbatim; `IF NOT EXISTS` and a
        // `main.` qualifier are dropped.
        assert_eq!(
            exec(db, "create   unique index  if not exists  main.\"quoted idx\" on t ( b )   ;  -- trail"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );

        assert_eq!(
            typed_rows(db, "SELECT type, name, tbl_name, sql FROM sqlite_master"),
            [
                "text:table|text:t|text:t|text:CREATE TABLE t (a INTEGER, b TEXT)",
                "text:index|text:ix_b|text:t|text:CREATE INDEX ix_b ON t(b)",
                "text:index|text:ux_a|text:t|text:CREATE UNIQUE INDEX ux_a ON t(a)",
                // Three trailing spaces: sqlite keeps everything up to the `;`.
                "text:index|text:quoted idx|text:t|text:CREATE UNIQUE INDEX \"quoted idx\" on t ( b )   ",
            ]
        );

        // A UNIQUE column constraint is an index sqlite NAMES but gives no
        // statement text — and the NULL is load-bearing (Django's
        // `get_constraints` does `if not sql: continue`).
        assert_eq!(exec(db, "CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE)"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "SELECT name, sql FROM sqlite_master WHERE tbl_name = 'u'"),
            [
                "text:u|text:CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE)",
                "text:sqlite_autoindex_u_1|null:",
            ]
        );
        // CPython's iterdump second pass filters on exactly that NULL.
        assert_eq!(
            collect_text_col(
                db,
                "SELECT name FROM sqlite_master WHERE sql NOT NULL AND type IN ('index','trigger','view')"
            ),
            ["ix_b", "ux_a", "quoted idx"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// `PRAGMA index_list` reports the REAL index name, the right `origin`, and the
/// `partial` bit — and reports newest-first, as sqlite does. `PRAGMA index_info`
/// exists at all, which is the third call in Django's `get_constraints` chain.
///
/// Oracle (3.45.0) for the same script, verbatim:
/// ```text
/// --index_list u--   0|part|0|c|1   1|spaced|0|c|0   2|sqlite_autoindex_u_1|1|u|0
/// --index_info ix_b--   0|1|b
/// ```
/// Before this, EVERY entry came back as `sqlite_autoindex_<t>_<k>` with origin
/// `c`: a fabricated name for a real `CREATE INDEX`, which then resolved to no
/// `sqlite_master` row at all.
#[test]
fn pragma_index_list_and_index_info_match_sqlites_shape() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE, c TEXT)"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(exec(db, "CREATE INDEX spaced ON u(c)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(
            exec(db, "CREATE INDEX part ON u(c) WHERE c IS NOT NULL"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(
            typed_rows(db, "PRAGMA index_list(u)"),
            [
                "int:0|text:part|int:0|text:c|int:1",
                "int:1|text:spaced|int:0|text:c|int:0",
                "int:2|text:sqlite_autoindex_u_1|int:1|text:u|int:0",
            ]
        );
        // seqno | cid | name, with `cid` in `table_info`'s numbering.
        assert_eq!(typed_rows(db, "PRAGMA index_info(spaced)"), ["int:0|int:2|text:c"]);
        assert_eq!(
            typed_rows(db, "PRAGMA index_info(sqlite_autoindex_u_1)"),
            ["int:0|int:1|text:b"]
        );
        // An unknown index is zero rows, not an error (sqlite's answer).
        assert!(typed_rows(db, "PRAGMA index_info(nope)").is_empty());

        // An `INTEGER PRIMARY KEY` is a rowid alias: sqlite builds NO index for
        // it, and neither does this — but a TEXT or composite PK does get one.
        assert_eq!(exec(db, "CREATE TABLE v (a TEXT PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "PRAGMA index_list(v)"),
            ["int:0|text:sqlite_autoindex_v_1|int:1|text:pk|int:0"]
        );
        assert_eq!(exec(db, "CREATE TABLE w (a INTEGER, b TEXT, PRIMARY KEY (a, b))"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "PRAGMA index_info(sqlite_autoindex_w_1)"),
            ["int:0|int:0|text:a", "int:1|int:1|text:b"]
        );
        // A table with a HIDDEN rowid (#94: no declared PK) has no PK index at
        // all — emitting one would advertise a column `SELECT *` cannot see.
        assert_eq!(exec(db, "CREATE TABLE h (a INTEGER, b TEXT)"), SQLITE_OK);
        assert!(typed_rows(db, "PRAGMA index_list(h)").is_empty());
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Django's `get_constraints` reaches `sqlite_master` ONLY through bound
/// parameters (`WHERE type='table' and name=%s`, then `type='index' AND
/// name=%s`). The shim's mini-evaluator used to read a literal or refuse, so
/// the whole method raised `unsupported` on the very first query.
#[test]
fn sqlite_master_where_accepts_a_bound_parameter() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX ix ON t(a)"), SQLITE_OK);

        let bound = |sql: &str, v: &str| -> Vec<String> {
            let s = cs(sql);
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "{}",
                errmsg(db)
            );
            let val = cs(v);
            assert_eq!(
                sqlite3_bind_text(st, 1, val.as_ptr(), -1, sqlite_transient()),
                SQLITE_OK
            );
            let mut out = Vec::new();
            loop {
                let rc = sqlite3_step(st);
                if rc == SQLITE_ROW {
                    let ty = sqlite3_column_type(st, 0);
                    let p = sqlite3_column_text(st, 0);
                    out.push(if ty == SQLITE_NULL {
                        "null:".to_string()
                    } else {
                        format!(
                            "text:{}",
                            CStr::from_ptr(p as *const c_char).to_str().unwrap()
                        )
                    });
                    continue;
                }
                assert_eq!(rc, SQLITE_DONE, "step {sql}: {}", errmsg(db));
                break;
            }
            sqlite3_finalize(st);
            out
        };

        assert_eq!(
            bound("SELECT sql FROM sqlite_master WHERE type='table' and name=?", "t"),
            ["text:CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"]
        );
        assert_eq!(
            bound("SELECT sql FROM sqlite_master WHERE type='index' AND name=?", "ix"),
            ["text:CREATE INDEX ix ON t(a)"]
        );
        // The constraint index resolves too — to a NULL, which is the answer
        // Django reads as "inline constraint, already parsed".
        assert_eq!(
            bound(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?",
                "sqlite_autoindex_t_1"
            ),
            ["null:"]
        );
        // Named and numbered bindings go through the same rewrite.
        assert_eq!(bound("SELECT name FROM sqlite_master WHERE name = :n", "ix"), ["text:ix"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// A dropped table takes its index NAMES with it: a table re-created with the
/// same shape must not inherit a `CREATE INDEX` text nobody wrote for it.
#[test]
fn dropping_a_table_forgets_its_index_names() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX ix_b ON t(b)"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["ix_b"]
        );
        assert_eq!(exec(db, "DROP TABLE t"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"), SQLITE_OK);
        // The UNIQUE constraint's index has the SAME shape the dropped
        // `ix_b`... it does not: `unique` differs. Assert the name anyway —
        // a leaked record would have surfaced as `ix_b`.
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["sqlite_autoindex_t_1"]
        );
        assert_eq!(exec(db, "DROP TABLE t"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX other ON t(b)"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["other"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Read-your-own-writes at the METADATA level: every introspection surface sees
/// a table the previous statement committed, with nothing in between — on the
/// connection that created it AND on a sibling connection to the same file.
///
/// `Database::schema()` hands back this process's cached bundle, which a DDL
/// commit does not touch; only `schema_gen` in the flipping meta moves. Every
/// surface here therefore has to consult it. The SQL paths do that through the
/// facade's own `gate_cache_on_schema`; `sqlite_master`/`PRAGMA` do it in
/// `exec_one_inner`; `sqlite3_blob_open` bypasses SQL entirely and used to
/// answer `no such table` for a table that demonstrably existed, until some
/// other statement on the handle happened to reload the bundle.
#[test]
fn introspection_sees_what_the_previous_statement_committed() {
    unsafe {
        // Same connection, no intervening statement.
        let db = open_memory();
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE fresh (a INTEGER, s TEXT)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type='table'"),
            ["fresh"]
        );
        assert_eq!(collect_text_col(db, "PRAGMA table_info(fresh)"), ["0", "1"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // Two connections on one file: B has never seen the table.
        let path = format!("{}/mpedb-capi-fresh-{}.db", scratch_dir(), std::process::id());
        let _ = std::fs::remove_file(&path);
        let mut a: *mut Sqlite3 = ptr::null_mut();
        let mut b: *mut Sqlite3 = ptr::null_mut();
        let p = cs(&path);
        assert_eq!(sqlite3_open(p.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(p.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "CREATE TABLE seed (x INTEGER)"), SQLITE_OK, "{}", errmsg(a));
        // B's FIRST look already sees it.
        assert_eq!(collect_text_col(b, "SELECT name FROM sqlite_master"), ["seed"]);

        assert_eq!(exec(a, "CREATE INDEX ix_x ON seed(x)"), SQLITE_OK, "{}", errmsg(a));
        assert_eq!(collect_text_col(b, "PRAGMA index_list(seed)"), ["0"]);
        assert_eq!(collect_col_text(b, "PRAGMA index_list(seed)", 1), ["ix_x"]);

        // Incremental blob I/O on a table+row B has never touched.
        assert_eq!(
            exec(a, "CREATE TABLE bl (id INTEGER PRIMARY KEY, d BLOB)"),
            SQLITE_OK,
            "{}",
            errmsg(a)
        );
        assert_eq!(exec(a, "INSERT INTO bl VALUES (1, x'0011223344')"), SQLITE_OK, "{}", errmsg(a));
        let mut blob: *mut c_void = ptr::null_mut();
        let (m, t, col) = (cs("main"), cs("bl"), cs("d"));
        assert_eq!(
            sqlite3_blob_open(b, m.as_ptr(), t.as_ptr(), col.as_ptr(), 1, 0, &mut blob),
            SQLITE_OK,
            "blob_open on a sibling connection: {}",
            errmsg(b)
        );
        assert_eq!(sqlite3_blob_bytes(blob), 5);
        let (rc, bytes) = blob_read_vec(blob, 5, 0);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(bytes, vec![0x00, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(sqlite3_blob_close(blob), SQLITE_OK);

        assert_eq!(sqlite3_close(a), SQLITE_OK);
        assert_eq!(sqlite3_close(b), SQLITE_OK);
        let _ = std::fs::remove_file(&path);
    }
}

/// `ALTER TABLE … RENAME TO` CARRIES the verbatim `CREATE` text, retargeted —
/// for the table and for every index over it, exactly as sqlite does.
///
/// The records are keyed by table name, so a rename used to orphan them and
/// `sqlite_master` fell back to reconstructing from the live schema. That loses
/// everything the catalog does not keep verbatim: here, a column's declared
/// `COLLATE nocase` came back as `COLLATE NOCASE`, and Django's
/// `assertColumnCollation` — a plain string compare against the token in
/// `sqlite_master.sql` — failed on the case alone.
///
/// It is not an exotic path: Django implements nearly every `AlterField` on
/// sqlite by building `new__<table>` and renaming it into place.
#[test]
fn rename_carries_the_verbatim_ddl_like_sqlite() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE a (x TEXT COLLATE nocase, y int)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "CREATE INDEX ix ON a(y)"), SQLITE_OK);
        assert_eq!(exec(db, "ALTER TABLE a RENAME TO b"), SQLITE_OK);

        // Both texts are what the 3.45.1 binary produces for this script.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'b'"),
            ["CREATE TABLE \"b\" (x TEXT COLLATE nocase, y int)"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'ix'"),
            ["CREATE INDEX ix ON \"b\"(y)"]
        );
        // The index's TABLE attribution moved too, so `index_list` still finds
        // it under the new name and reports it as `CREATE INDEX`-origin.
        assert_eq!(collect_text_col(db, "SELECT tbl_name FROM sqlite_master WHERE name = 'ix'"), ["b"]);

        // The old name answers nothing, and a NEW table taking it does not
        // inherit the moved text.
        assert!(collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'a'").is_empty());
        assert_eq!(exec(db, "CREATE TABLE a (z int)"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'a'"),
            ["CREATE TABLE a (z int)"]
        );

        // A SELF-REFERENCE is the one shape the retarget refuses: the same
        // token could be a column of that name, so rather than emit text
        // pointing at a table that no longer exists, it falls back to the
        // reconstruction — which is correct, just not verbatim.
        assert_eq!(
            exec(db, "CREATE TABLE p (id INTEGER PRIMARY KEY, up INT REFERENCES p(id))"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "ALTER TABLE p RENAME TO q"), SQLITE_OK);
        let sql = collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'q'");
        assert!(sql[0].contains("\"q\"") && !sql[0].contains("REFERENCES p"), "{sql:?}");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
