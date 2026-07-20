//! Unquoted (and quoted) SQL identifiers fold ASCII case, as sqlite does —
//! differential against the BUNDLED oracle (sqlite 3.45.0, `sqlite_oracle`).
//!
//! The gap this closes was found by CPython's own suite: `CREATE TABLE t`
//! followed by a reference to `T` answered `unknown table T`. It is the kind of
//! gap that fires wherever a consumer round-trips a name through different
//! casing, which ORMs and dump/restore paths do constantly.
//!
//! # What the oracle actually says (measured, not remembered)
//!
//! Two of the three rules contradict the plausible guess, so every one of them
//! is asserted here against the oracle rather than against a belief:
//!
//! 1. **Quoting does NOT protect case.** `"T"`, `[T]`, `` `T` `` and bare `T`
//!    all name the same thing as `t`; `CREATE TABLE t("a" INT, "A" INT)` is
//!    `duplicate column name`. Quoting buys spellings a bare word cannot have,
//!    never case sensitivity. (`quoted_spellings_all_fold`.)
//! 2. **Folding is ASCII-ONLY.** `Æ`/`æ`, `k`/`KELVIN SIGN` and `i`/`İ` stay
//!    DISTINCT identifiers. Reaching for Rust's Unicode-aware `to_lowercase()`
//!    would silently merge names sqlite keeps apart — a wrong answer, not an
//!    error. (`non_ascii_names_stay_distinct`.)
//! 3. **Names are reported in their DECLARED spelling**, not the folded one and
//!    not the query's: `CREATE TABLE t(Abc INT)` then `SELECT ABC FROM T`
//!    labels the column `Abc`. Folding a stored name would relabel every result
//!    column. (`reported_names_keep_the_declared_spelling`.)
//!
//! # The hazard
//!
//! Widening turns refusals into answers, and a wrong one is worse than the
//! refusal it replaced. The one place that genuinely fired here is ORDER BY:
//! an output alias outranks a base column of the same name, the planner
//! implements that with an EXACT alias comparison, and once column lookup
//! folded, `SELECT a AS b FROM t ORDER BY B` stopped matching the alias and
//! silently sorted by the base column `b`. Measured before the fix: mpedb
//! `2,1` against sqlite's `1,2`. `order_by_alias_outranks_base_column` is the
//! guard, and `parser::select::align_order_by_alias_case` is the fix.
//!
//! `COLLATE NOCASE` is about VALUES, not identifiers, and is a different
//! mechanism; `collate_nocase_is_unrelated_to_identifier_folding` pins that the
//! two do not route through each other.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, std::path::PathBuf) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-identcase-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // A one-table seed the battery never uses: every table under test is made
    // with live `CREATE TABLE`, so the DDL path folds too.
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n",
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config"))
        .expect("open");
    (db, path)
}

/// One statement's rows, rendered like the oracle's list mode (`|`-joined, NULL
/// as `NULL`), or `Err(message)`.
fn one(db: &Database, sql: &str) -> Result<Vec<String>, String> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => Ok(rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => "NULL".to_owned(),
                        Value::Int(i) => i.to_string(),
                        // mpedb has a real bool type; sqlite prints 1/0. That
                        // difference is the type model, not identifier casing.
                        Value::Bool(b) => u8::from(*b).to_string(),
                        Value::Float(f) => format!("{f}"),
                        Value::Text(s) => s.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()),
        Ok(_) => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// The output column labels for `sql`, or `Err`.
fn labels(db: &Database, sql: &str) -> Result<Vec<String>, String> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { columns, .. }) => Ok(columns),
        Ok(other) => Err(format!("not rows: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// The setup statements as ONE `;`-terminated script for the oracle. mpedb's
/// `query` takes a single statement, so the suite holds them unterminated and
/// they get their separators only here.
fn join_stmts(setup: &[&str]) -> String {
    setup.iter().map(|s| format!("{};", s.trim_end_matches(';'))).collect::<Vec<_>>().join("\n")
}

/// Run `setup` (each statement must succeed) then `query` on a fresh mpedb, and
/// the same script on the oracle, and require them to **agree or refuse, never
/// differ**: if mpedb answers, the answer must equal sqlite's; if mpedb
/// refuses, sqlite must have done something mpedb structurally cannot, which
/// the caller states by passing `may_refuse = true`.
#[track_caller]
fn agree(setup: &[&str], query: &str, may_refuse: bool) {
    let script = format!("{}\n{query};", join_stmts(setup));
    let want: Vec<String> = sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(str::to_owned)
        .collect();

    let (db, path) = open();
    for s in setup {
        let s = s.trim_end_matches(';');
        if let Err(e) = db.query(s, &[]) {
            let _ = std::fs::remove_file(&path);
            panic!("setup failed on `{s}`: {e}\n(sqlite ran the same script fine)");
        }
    }
    let got = one(&db, query);
    drop(db);
    let _ = std::fs::remove_file(&path);

    match got {
        Ok(rows) => assert_eq!(rows, want, "\nquery: {query}\nsetup: {setup:?}"),
        Err(e) => assert!(
            may_refuse,
            "mpedb REFUSED a query sqlite answers {want:?}: {e}\nquery: {query}\nsetup: {setup:?}"
        ),
    }
}

/// Both engines must REJECT `script` (mpedb's refusal is the point, so no
/// `may_refuse` escape hatch here).
#[track_caller]
fn both_reject(setup: &[&str], bad: &str) {
    let script = format!("{}\n{bad};", join_stmts(setup));
    assert!(
        sqlite_oracle::try_script_stdout(&script, "NULL").is_err(),
        "sqlite ACCEPTED what this test claims both reject: {bad}"
    );
    let (db, path) = open();
    for s in setup {
        db.query(s.trim_end_matches(';'), &[]).expect("setup");
    }
    let got = db.query(bad, &[]);
    drop(db);
    let _ = std::fs::remove_file(&path);
    assert!(got.is_err(), "mpedb ACCEPTED `{bad}`, sqlite rejects it");
}

// ---------------------------------------------------------------- table names

#[test]
fn table_names_fold_ascii_case() {
    let mk = ["CREATE TABLE MyTab(a INT)", "INSERT INTO mytab VALUES(1)"];
    agree(&mk, "SELECT count(*) FROM MYTAB", false);
    agree(&mk, "SELECT count(*) FROM MyTab", false);
    agree(&mk, "SELECT count(*) FROM mYtAb", false);
    // …and through every DML verb, not just SELECT.
    agree(
        &["CREATE TABLE t(Abc INT)", "INSERT INTO T(ABC) VALUES(1)", "UPDATE T SET ABC=5"],
        "SELECT abc FROM t",
        false,
    );
    agree(
        &["CREATE TABLE t(Abc INT)", "INSERT INTO t VALUES(1)", "DELETE FROM T WHERE ABC=1"],
        "SELECT count(*) FROM t",
        false,
    );
}

#[test]
fn quoted_spellings_all_fold() {
    // The measured surprise: quoting does NOT make a name case-sensitive, in
    // EITHER direction. All four spellings name one table.
    let mk = ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(7)"];
    agree(&mk, "SELECT a FROM \"T\"", false);
    agree(&mk, "SELECT a FROM [T]", false);
    agree(&mk, "SELECT a FROM `T`", false);
    // Declared QUOTED, referenced bare and differently cased.
    let mkq = ["CREATE TABLE \"MiXeD\"(\"Abc\" INT)", "INSERT INTO mixed(abc) VALUES(1)"];
    agree(&mkq, "SELECT ABC FROM MIXED", false);
    agree(&mkq, "SELECT \"abc\" FROM \"mixed\"", false);
}

#[test]
fn duplicate_table_names_modulo_case_are_refused() {
    // sqlite: `table T already exists`. If this were allowed, the two tables
    // would be indistinguishable to every lookup — a wrong answer waiting.
    both_reject(&["CREATE TABLE t(a INT)"], "CREATE TABLE T(b INT)");
    both_reject(&["CREATE TABLE t(a INT)"], "CREATE TABLE \"T\"(b INT)");
}

// --------------------------------------------------------------- column names

#[test]
fn column_names_fold_ascii_case() {
    let mk = ["CREATE TABLE t(Abc INT, dEf INT)", "INSERT INTO t(ABC, DEF) VALUES(1, 2)"];
    agree(&mk, "SELECT abc, def FROM t", false);
    agree(&mk, "SELECT t.ABC FROM t", false);
    agree(&mk, "SELECT x.aBc FROM t AS X", false);
    agree(&mk, "SELECT count(*) FROM t WHERE ABC=1 AND DeF=2", false);
    agree(&mk, "SELECT ABC, count(*) FROM t GROUP BY abc HAVING ABC=1", false);
    agree(&mk, "SELECT DISTINCT ABC FROM t", false);
}

#[test]
fn duplicate_columns_modulo_case_are_refused() {
    // sqlite: `duplicate column name: A` — and it says so for the QUOTED
    // spelling too, which is the same measurement as `quoted_spellings_all_fold`
    // seen from the declaration side.
    both_reject(&[], "CREATE TABLE t(a INT, A INT)");
    both_reject(&[], "CREATE TABLE t(\"a\" INT, \"A\" INT)");
    both_reject(&[], "CREATE TABLE t(a INT, \"A\" INT)");
}

#[test]
fn unique_and_primary_key_see_one_column_not_two() {
    // The UNIQUE/PK interaction: with a single folded column there is a single
    // index, so the second insert collides exactly as sqlite says it does.
    both_reject(
        &["CREATE TABLE t(a INT UNIQUE)", "INSERT INTO t VALUES(1)"],
        "INSERT INTO t VALUES(1)",
    );
    // A PK declared in one case and referenced in another is still ONE key.
    agree(
        &["CREATE TABLE t(Abc INTEGER PRIMARY KEY)", "INSERT INTO T(ABC) VALUES(1)"],
        "SELECT ABC FROM T WHERE abc=1",
        false,
    );
    both_reject(
        &["CREATE TABLE t(Abc INTEGER PRIMARY KEY)", "INSERT INTO t VALUES(1)"],
        "INSERT INTO T(ABC) VALUES(1)",
    );
}

// ------------------------------------------------------------------- non-ASCII

#[test]
fn non_ascii_names_stay_distinct() {
    // THE bug this test exists to prevent: a Unicode-aware fold would merge
    // these, and mpedb would answer the wrong row instead of erroring.
    // `Æ` (U+00C6) vs `æ` (U+00E6) — sqlite keeps them apart, so both can be
    // declared side by side and each keeps its own value.
    let mk = ["CREATE TABLE t(\u{e6} INT, \u{c6} INT)", "INSERT INTO t VALUES(1, 2)"];
    agree(&mk, "SELECT \u{e6}, \u{c6} FROM t", false);
    // Two TABLES differing only by a non-ASCII case pair coexist.
    let two = [
        "CREATE TABLE \u{e6}(x INT)",
        "CREATE TABLE \u{c6}(x INT)",
        "INSERT INTO \u{e6} VALUES(1)",
        "INSERT INTO \u{c6} VALUES(2)",
    ];
    agree(&two, "SELECT x FROM \u{e6}", false);
    agree(&two, "SELECT x FROM \u{c6}", false);
    // The two classic Unicode-fold traps: KELVIN SIGN folds to `k` and `İ`
    // folds to `i` under `to_lowercase()`, but NOT under sqlite's rule.
    both_reject(&["CREATE TABLE k(a INT)"], "SELECT * FROM \u{212a}");
    both_reject(&["CREATE TABLE ti(a INT)"], "SELECT * FROM t\u{130}");
}

// ---------------------------------------------------------- reported spellings

#[test]
fn reported_names_keep_the_declared_spelling() {
    let (db, path) = open();
    db.query("CREATE TABLE MiXeD(Abc INT, dEf INT)", &[]).unwrap();
    db.query("INSERT INTO mixed VALUES(1,2)", &[]).unwrap();

    // sqlite reports the DECLARED spelling for a bare column reference, no
    // matter how the query spelled it — verified against the oracle right here
    // so the expectation cannot drift from belief.
    let want = sqlite_oracle::script_stdout_headers(
        "CREATE TABLE MiXeD(Abc INT, dEf INT); INSERT INTO mixed VALUES(1,2); \
         SELECT ABC, DEF FROM MIXED;",
        "NULL",
    );
    let want: Vec<String> =
        want.lines().next().unwrap().split('|').map(str::to_owned).collect();
    assert_eq!(want, vec!["Abc", "dEf"], "oracle's own column labels");
    assert_eq!(labels(&db, "SELECT ABC, DEF FROM MIXED").unwrap(), want);
    // `SELECT *` likewise.
    assert_eq!(labels(&db, "SELECT * FROM MIXED").unwrap(), want);
    // An explicit alias is reported VERBATIM, in the alias's own spelling.
    assert_eq!(
        labels(&db, "SELECT ABC AS WeIrD FROM mixed").unwrap(),
        vec!["WeIrD".to_owned()]
    );
    // Error messages quote what the USER wrote, so a typo is findable.
    let err = db.query("SELECT NoSuchCol FROM MIXED", &[]).unwrap_err().to_string();
    assert!(err.contains("NoSuchCol"), "error lost the user's spelling: {err}");
    let err = db.query("SELECT 1 FROM NoSuchTbl", &[]).unwrap_err().to_string();
    assert!(err.contains("NoSuchTbl"), "error lost the user's spelling: {err}");

    drop(db);
    let _ = std::fs::remove_file(&path);
}

// -------------------------------------------------------------------- aliases

#[test]
fn table_aliases_fold_ascii_case() {
    let mk = ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)"];
    agree(&mk, "SELECT X.a FROM t AS x", false);
    agree(&mk, "SELECT \"X\".a FROM t AS \"x\"", false);
}

#[test]
fn order_by_alias_outranks_base_column() {
    // THE regression guard for the one wrong answer this change could create.
    // `b` is both an output alias (for `a`) and a real column. sqlite resolves
    // the OUTPUT name first, case-insensitively, so all four spellings sort by
    // `a` (1,2) and none by the base `b` (which would give 2,1).
    let mk = ["CREATE TABLE t(a INT, b INT)", "INSERT INTO t VALUES(1,20)", "INSERT INTO t VALUES(2,10)"];
    agree(&mk, "SELECT a AS b FROM t ORDER BY b", false);
    agree(&mk, "SELECT a AS b FROM t ORDER BY B", false);
    agree(&mk, "SELECT a AS b FROM t ORDER BY \"B\"", false);
    agree(&mk, "SELECT DISTINCT a AS b FROM t ORDER BY B", false);
    agree(&mk, "SELECT a AS b FROM t ORDER BY B DESC", false);
    // …but a QUALIFIED name or an EXPRESSION names the BASE column even when an
    // alias shadows it (measured), so those must still give 2,1.
    agree(&mk, "SELECT a AS b FROM t ORDER BY t.B", false);
    agree(&mk, "SELECT a AS b FROM t ORDER BY B+0", false);
    // An alias naming a DIFFERENT column still wins over that column.
    agree(&mk, "SELECT b AS A FROM t ORDER BY a", false);
    // GROUP BY and WHERE do NOT see aliases — they resolve to the base column.
    agree(&mk, "SELECT a AS b, count(*) FROM t GROUP BY B ORDER BY 1", false);
    agree(&mk, "SELECT a AS b FROM t WHERE B=10", false);
}

// ----------------------------------------------------------------------- CTEs

#[test]
fn cte_names_and_columns_fold_ascii_case() {
    // Table-backed CTE bodies throughout: a FROM-less body is a SEPARATE and
    // unrelated engine gap ("cannot be flattened"), and using one here would
    // refuse for a reason that has nothing to do with casing, hiding whatever
    // the case fold actually does.
    let mk = ["CREATE TABLE t(Abc INT)", "INSERT INTO t VALUES(1)", "INSERT INTO t VALUES(2)"];
    agree(&mk, "WITH Cte AS (SELECT ABC FROM T) SELECT abc FROM CTE ORDER BY 1", false);
    agree(&mk, "WITH \"Cte\" AS (SELECT Abc FROM t) SELECT Abc FROM cte ORDER BY 1", false);
    // An explicit CTE column list: mpedb supports it only on the RECURSIVE
    // form, so the plain one may refuse (an unrelated parser gap, not casing).
    agree(&mk, "WITH c(Ab) AS (SELECT Abc FROM t) SELECT AB FROM C ORDER BY 1", true);
    // A RECURSIVE CTE's name is matched exactly by
    // `planner/mod.rs::resolve_table_cte` (~:679) — for the self-reference AND
    // the outer one — and `planner/` is owned by another agent here. So a
    // recursive CTE referenced in a different case REFUSES (`unknown table r`)
    // rather than differing. Its COLUMN names fold normally.
    agree(
        &[],
        "WITH RECURSIVE R(Nn) AS (SELECT 1 UNION ALL SELECT NN+1 FROM r WHERE nn<3) \
         SELECT nN FROM R ORDER BY 1",
        true,
    );
    agree(
        &[],
        "WITH RECURSIVE r(Nn) AS (SELECT 1 UNION ALL SELECT NN+1 FROM r WHERE nn<3) \
         SELECT nN FROM r ORDER BY 1",
        false,
    );
    // A CTE shadows a same-named table case-insensitively: `FROM t` in the
    // outer query must find the CTE `T` (row 9), not the base table (1 and 2).
    let two = [
        "CREATE TABLE t(Abc INT)",
        "CREATE TABLE u(Abc INT)",
        "INSERT INTO t VALUES(1)",
        "INSERT INTO t VALUES(2)",
        "INSERT INTO u VALUES(9)",
    ];
    agree(&two, "WITH T AS (SELECT Abc FROM U) SELECT abc FROM t", false);
    // The shadowing reaches INSIDE the CTE's own body too, which is how sqlite
    // arrives at `circular reference: T` here rather than reading the table.
    // Both engines must refuse — resolving it to the base table would be a
    // silent wrong answer dressed up as a convenience.
    both_reject(&mk, "WITH T AS (SELECT Abc FROM t WHERE ABC=2) SELECT abc FROM t");
}

// ---------------------------------------------------------------------- views

#[test]
fn view_names_fold_ascii_case() {
    let (db, path) = open();
    db.query("CREATE TABLE t(a INT)", &[]).unwrap();
    db.query("INSERT INTO t VALUES(1)", &[]).unwrap();
    db.query("CREATE VIEW MyView AS SELECT a FROM t", &[]).unwrap();
    // Referenced in another case.
    assert_eq!(one(&db, "SELECT a FROM MYVIEW").unwrap(), vec!["1".to_owned()]);
    assert_eq!(one(&db, "SELECT a FROM myview").unwrap(), vec!["1".to_owned()]);
    // A view name colliding with a table modulo case is refused, as sqlite does
    // (`table T already exists`).
    assert!(db.query("CREATE VIEW T AS SELECT 1 AS a", &[]).is_err());
    assert!(db.query("CREATE VIEW MYVIEW AS SELECT 1 AS a", &[]).is_err());
    // DROP finds it in yet another case, and really drops it.
    db.query("DROP VIEW myVIEW", &[]).unwrap();
    assert!(db.query("SELECT a FROM MyView", &[]).is_err());
    drop(db);
    let _ = std::fs::remove_file(&path);

    // The oracle agrees on the whole shape.
    assert!(sqlite_oracle::try_script_stdout(
        "CREATE TABLE t(a INT); CREATE VIEW MyView AS SELECT a FROM t; \
         SELECT count(*) FROM MYVIEW; DROP VIEW myVIEW; SELECT * FROM MyView;",
        "NULL",
    )
    .is_err_and(|e| e.contains("MyView")));
}

// -------------------------------------------------------------------- indexes

#[test]
fn index_names_and_columns_fold_ascii_case() {
    // The INDEXED COLUMNS resolve case-insensitively (this is the part mpedb
    // stores), and an index created in one case is dropped in another.
    let (db, path) = open();
    db.query("CREATE TABLE t(Abc INT, d INT)", &[]).unwrap();
    db.query("CREATE INDEX MixedIx ON T(ABC)", &[]).unwrap();
    db.query("INSERT INTO t VALUES(1,2)", &[]).unwrap();
    assert_eq!(one(&db, "SELECT d FROM t WHERE ABC=1").unwrap(), vec!["2".to_owned()]);
    drop(db);
    let _ = std::fs::remove_file(&path);

    // sqlite: the index NAME folds too (a second `IX` is `already exists`) and
    // is stored verbatim. mpedb does not persist index names at all (they are
    // parsed and dropped; identity is the index SHAPE), so there is nothing for
    // case folding to get wrong here — recorded, not asserted against mpedb.
    assert!(sqlite_oracle::try_script_stdout(
        "CREATE TABLE t(a INT); CREATE INDEX Ix ON t(a); CREATE INDEX IX ON t(a);",
        "NULL",
    )
    .is_err());
    assert_eq!(
        sqlite_oracle::script_stdout(
            "CREATE TABLE t(a INT); CREATE INDEX MixedIx ON t(a); \
             SELECT name FROM sqlite_master WHERE type='index';",
            "NULL",
        ),
        "MixedIx\n",
        "sqlite stores the index name VERBATIM"
    );
}

// ------------------------------------------------------------------- triggers

#[test]
fn trigger_names_and_bodies_fold_ascii_case() {
    let (db, path) = open();
    db.query("CREATE TABLE t(a INT)", &[]).unwrap();
    db.query("CREATE TABLE log(v INT)", &[]).unwrap();
    // Table name, column name and trigger name all spelled differently from
    // their declarations.
    db.query(
        "CREATE TRIGGER Tr AFTER INSERT ON T BEGIN INSERT INTO LOG VALUES(NEW.A); END",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO t VALUES(5)", &[]).unwrap();
    assert_eq!(one(&db, "SELECT v FROM log").unwrap(), vec!["5".to_owned()]);
    // A duplicate trigger name modulo case is refused; DROP finds it folded.
    assert!(db
        .query("CREATE TRIGGER TR AFTER INSERT ON t BEGIN INSERT INTO log VALUES(1); END", &[])
        .is_err());
    db.query("DROP TRIGGER tR", &[]).unwrap();
    db.query("INSERT INTO t VALUES(6)", &[]).unwrap();
    assert_eq!(one(&db, "SELECT count(*) FROM log").unwrap(), vec!["1".to_owned()]);
    drop(db);
    let _ = std::fs::remove_file(&path);

    // Same shape on the oracle.
    assert_eq!(
        sqlite_oracle::script_stdout(
            "CREATE TABLE t(a INT); CREATE TABLE log(v INT); \
             CREATE TRIGGER Tr AFTER INSERT ON T BEGIN INSERT INTO LOG VALUES(NEW.A); END; \
             INSERT INTO t VALUES(5); SELECT v FROM log;",
            "NULL",
        ),
        "5\n"
    );
}

// ------------------------------------------------------ joins: USING / NATURAL

#[test]
fn join_column_matching_folds_ascii_case() {
    let mk = [
        "CREATE TABLE a(Id INT, x INT)",
        "CREATE TABLE b(ID INT, y INT)",
        "INSERT INTO a VALUES(1,10)",
        "INSERT INTO b VALUES(1,20)",
    ];
    agree(&mk, "SELECT x, y FROM a JOIN b USING(id)", true);
    agree(&mk, "SELECT x, y FROM a NATURAL JOIN b", true);
    agree(&mk, "SELECT A.x, B.y FROM a JOIN b ON A.ID = B.id", false);
}

#[test]
fn ambiguous_modulo_case_is_refused() {
    // Two tables whose columns differ only by case make a bare reference
    // ambiguous in sqlite. Answering it from one side would be a coin flip.
    both_reject(
        &[
            "CREATE TABLE a(x INT)",
            "CREATE TABLE b(X INT)",
            "INSERT INTO a VALUES(1)",
            "INSERT INTO b VALUES(2)",
        ],
        "SELECT x FROM a, b",
    );
}

// ------------------------------------------------- functions / types / collate

#[test]
fn function_and_type_names_were_already_case_insensitive() {
    // Not changed by this work — pinned so a later "cleanup" cannot regress it.
    agree(&[], "SELECT AbS(-1), LENGTH('ab')", false);
    agree(&["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)"], "SELECT CoUnT(*) FROM T", false);
    agree(&[], "SELECT typeof(CAST('12' AS InTeGeR))", false);
}

#[test]
fn collate_nocase_is_unrelated_to_identifier_folding() {
    // NOCASE is about VALUES. It is spelled case-insensitively (a name), but
    // what it does is compare STRINGS — and, separately measured, it is
    // ASCII-only too, so 'æ' and 'Æ' are NOT equal under it. If the identifier
    // fold ever routed through value comparison, this row would flip.
    agree(&[], "SELECT 'a' = 'A' COLLATE NoCase", true);
    agree(&[], "SELECT '\u{e6}' = '\u{c6}' COLLATE NOCASE", true);
    // A column whose COLLATE is declared in mixed case still compares NOCASE.
    agree(
        &["CREATE TABLE t(a TEXT COLLATE NoCase)", "INSERT INTO t VALUES('AB')"],
        "SELECT count(*) FROM t WHERE a='ab'",
        true,
    );
    // …while an identifier that differs only by non-ASCII case does NOT match,
    // even next to a NOCASE column — the two mechanisms stay separate.
    both_reject(
        &["CREATE TABLE t(\u{e6} TEXT COLLATE NOCASE)"],
        "SELECT \u{c6} FROM t",
    );
}

// ------------------------------------------------------------------ DML extras

#[test]
fn insert_update_and_upsert_column_lists_fold() {
    agree(
        &["CREATE TABLE t(Abc INT, dEf INT)", "INSERT INTO T(ABC,DEF) VALUES(1,2)"],
        "SELECT abc, def FROM t",
        false,
    );
    // `excluded.<col>` folds. The ON CONFLICT *target* and the DO UPDATE SET
    // column list still do NOT: those two lookups are spelled inline in
    // `planner/mod.rs` (`unknown conflict-target column`, ~:726, and
    // `unknown column … in DO UPDATE SET`, ~:762) rather than going through
    // `TableDef::column_index`, and `planner/` is owned by another agent in
    // this working tree. The result is a REFUSAL where sqlite answers — inside
    // the law (agree or refuse, never differ), and a two-line follow-up.
    // Written exactly-cased so the rest of the upsert path is still covered.
    agree(
        &[
            "CREATE TABLE t(Abc INTEGER PRIMARY KEY, dEf INT)",
            "INSERT INTO t VALUES(1,1)",
            "INSERT INTO t VALUES(1,2) ON CONFLICT(Abc) DO UPDATE SET dEf=EXCLUDED.DEF",
        ],
        "SELECT abc, def FROM t",
        true,
    );
}

/// The known planner-side residuals: mpedb REFUSES, sqlite answers. Pinned so
/// the gap is visible and so a later fix has a test that flips loudly rather
/// than silently. Each is a refusal, never a differing answer.
#[test]
fn planner_owned_residuals_refuse_rather_than_differ() {
    let mk = [
        "CREATE TABLE t(Abc INTEGER PRIMARY KEY, dEf INT)",
        "INSERT INTO t VALUES(1,1)",
    ];
    let (db, path) = open();
    for s in mk {
        db.query(s, &[]).expect("setup");
    }
    for bad in [
        // ON CONFLICT target, planner/mod.rs ~:726
        "INSERT INTO t VALUES(1,2) ON CONFLICT(ABC) DO UPDATE SET dEf=2",
        // DO UPDATE SET column list, planner/mod.rs ~:762
        "INSERT INTO t VALUES(1,2) ON CONFLICT(Abc) DO UPDATE SET DEF=2",
        // RETURNING column list, planner/mod.rs ~:840
        "INSERT INTO t VALUES(9,9) RETURNING ABC",
    ] {
        let got = db.query(bad, &[]);
        assert!(
            got.is_err(),
            "`{bad}` now WORKS — good: fold the matching planner/ site and move \
             this case into the positive battery above"
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
