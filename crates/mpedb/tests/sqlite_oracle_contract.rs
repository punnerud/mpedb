//! Pins the rendering contract of the bundled-sqlite oracle
//! (`tests/sqlite_oracle/mod.rs`): list-mode shape, NULL sentinels, sqlite's
//! own REAL→TEXT conversion, error text, lenient mode — and the known
//! version-wobble cases, hardcoded to the answers of the PINNED sqlite
//! (3.45.0). If a Cargo.toml bump of rusqlite changes any of these, this
//! test is the reviewable behavioural diff.
//!
//! Nothing here shells out: the whole point is that no `sqlite3` binary is
//! needed on any machine. The one `#[ignore]`d test at the bottom compares
//! the oracle against the AMBIENT CLI — useful when auditing an upgrade,
//! version-dependent by design, never part of the default battery.

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

use sqlite_oracle::{script_stdout, script_stdout_lenient, try_script_stdout};

#[test]
fn the_pinned_version_is_what_cargo_toml_says() {
    // Not an upgrade blocker — a tripwire that the "bundled" feature is
    // actually in effect (a system-linked rusqlite would float with the OS).
    assert_eq!(sqlite_oracle::version(), "3.45.0");
}

#[test]
fn list_mode_shape_matches_the_cli() {
    // Rows newline-terminated, columns '|'-joined, headers off.
    assert_eq!(
        script_stdout("SELECT 1, 'a', NULL; SELECT 2, 'b|c', '';", ""),
        "1|a|\n2|b|c|\n"
    );
    // The nullvalue sentinel is caller-chosen, exactly like `-nullvalue`.
    assert_eq!(script_stdout("SELECT NULL;", "NULL"), "NULL\n");
    assert_eq!(script_stdout("SELECT NULL;", "<NULL>"), "<NULL>\n");
    // Setup statements print nothing; only row-returning statements do.
    assert_eq!(
        script_stdout(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\n\
             INSERT INTO t VALUES (1, 'x');\n\
             INSERT INTO t VALUES (2, NULL);\n\
             SELECT a, b FROM t ORDER BY a;",
            ""
        ),
        "1|x\n2|\n"
    );
}

#[test]
fn reals_render_through_sqlites_own_conversion() {
    // These are sqlite3_column_text's answers — the CLI prints the same.
    let cases = [
        ("SELECT 1.5;", "1.5"),
        ("SELECT 3.0;", "3.0"),
        ("SELECT 1.0/3.0;", "0.333333333333333"),
        ("SELECT 1e20;", "1.0e+20"),
        ("SELECT 1e-15;", "1.0e-15"),
        ("SELECT -0.0;", "0.0"),   // sqlite drops the sign of negative zero
        ("SELECT 0.0/-1;", "0.0"), // …even when computed (CLI 3.45.1 agrees)
        ("SELECT 0.1;", "0.1"),
        ("SELECT 9e999;", "Inf"),
        ("SELECT -9e999;", "-Inf"),
        ("SELECT CAST('nan' AS REAL);", "0.0"), // unparseable text casts to 0.0
    ];
    for (sql, want) in cases {
        assert_eq!(script_stdout(sql, ""), format!("{want}\n"), "for {sql}");
    }
    // Integers are integers, not floats.
    assert_eq!(script_stdout("SELECT 2+2, -9223372036854775808;", ""), "4|-9223372036854775808\n");
}

/// The wobble that motivated the whole conversion: sqlite changed `%f`
/// rounding after 3.45, so the same `printf('%.0f', …)` differs between the
/// dev box (3.45.1) and the M3 (3.51.0). 3.45 rounds an exact half DOWN
/// (0.5→"0", 1.5→"1", 2.5→"2" — verified against both bundled 3.45.0 and
/// the CLI 3.45.1, which agree on every case here). The bundled oracle —
/// mpedb's actual target — answers with 3.45 semantics on every machine.
#[test]
fn printf_percent_f_rounding_is_pinned_to_3_45() {
    assert_eq!(script_stdout("SELECT printf('%.0f', 0.5);", ""), "0\n");
    assert_eq!(script_stdout("SELECT printf('%.0f', 1.5);", ""), "1\n");
    assert_eq!(script_stdout("SELECT printf('%.0f', 2.5);", ""), "2\n");
    assert_eq!(script_stdout("SELECT printf('%.1f', 0.25);", ""), "0.3\n");
}

/// The CLI prints C strings, so an embedded NUL truncates the cell. The value
/// underneath is real — `printf('%c', NULL)` is a ONE-BYTE `\0` string in
/// sqlite (`hex()` proves it) — but every differential expectation was written
/// against the CLI's truncated rendering, so the oracle reproduces it.
/// (mpedb's own `printf('%c', NULL)` is a genuinely empty string; the
/// difference is invisible through this rendering, exactly as it was through
/// the subprocess.)
#[test]
fn embedded_nul_truncates_like_the_clis_c_strings() {
    assert_eq!(script_stdout("SELECT printf('%c', NULL);", ""), "\n");
    assert_eq!(script_stdout("SELECT hex(printf('%c', NULL));", ""), "00\n");
    assert_eq!(
        script_stdout("SELECT 'a' || printf('%c', NULL) || 'b';", ""),
        "a\n"
    );
}

#[test]
fn error_paths_carry_sqlites_message_and_lenient_mode_continues() {
    // Fail-fast: first error, sqlite's own text.
    let e = try_script_stdout("CREATE TABLE t(a INTEGER PRIMARY KEY); SAVEPOINT s; ROLLBACK TO nope;", "")
        .unwrap_err();
    assert!(e.contains("no such savepoint: nope"), "got: {e}");
    let e = try_script_stdout(
        "CREATE TABLE t(a INTEGER PRIMARY KEY);\n\
         INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);",
        "",
    )
    .unwrap_err();
    assert!(e.contains("UNIQUE constraint failed"), "got: {e}");

    // Lenient: the failed INSERT is skipped, the script keeps going — the
    // CLI's default batch behaviour.
    assert_eq!(
        script_stdout_lenient(
            "CREATE TABLE t(a INTEGER PRIMARY KEY);\n\
             INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\n\
             INSERT INTO t VALUES (2);\nSELECT a FROM t ORDER BY a;",
            ""
        ),
        "1\n2\n"
    );
}

#[test]
fn foreign_keys_default_off_like_stock_sqlite() {
    // libsqlite3-sys compiles the bundled library with
    // -DSQLITE_DEFAULT_FOREIGN_KEYS=1; the oracle resets it to the stock
    // default the CLI (and mpedb's dialect contract) assume. A dangling
    // child row must go in.
    assert_eq!(
        script_stdout(
            "CREATE TABLE p (id INTEGER PRIMARY KEY);\n\
             CREATE TABLE ch (id INTEGER PRIMARY KEY, p_id INT REFERENCES p(id));\n\
             INSERT INTO ch VALUES (1, 99);\nSELECT id, p_id FROM ch;",
            ""
        ),
        "1|99\n"
    );
}

#[test]
fn the_math_and_fts5_surfaces_exist() {
    // Math functions come from LIBSQLITE3_FLAGS in .cargo/config.toml.
    assert_eq!(script_stdout("SELECT log2(8), trunc(-1.5);", ""), "3.0|-1.0\n");
    // FTS5 is part of libsqlite3-sys's bundled flag set.
    assert_eq!(
        script_stdout(
            "CREATE VIRTUAL TABLE ft USING fts5(a);\n\
             INSERT INTO ft(rowid, a) VALUES (1, 'apple pie');\n\
             SELECT rowid FROM ft WHERE ft MATCH 'apple';",
            ""
        ),
        "1\n"
    );
}

/// Upgrade-audit aid, NOT part of the battery: sweep a rendering corpus
/// through the oracle and the ambient `sqlite3` CLI and diff them. Run it
/// when bumping the pinned rusqlite to see the behavioural delta against a
/// known CLI. Version-dependent by nature, hence ignored.
#[test]
#[ignore = "compares against the ambient sqlite3 CLI; version-dependent by design"]
fn parity_with_the_ambient_cli() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let script = "CREATE TABLE t(a INTEGER PRIMARY KEY, x REAL, s TEXT);\n\
         INSERT INTO t VALUES (1, 0.5, 'a'), (2, -0.0, NULL), (3, 1e20, 'b|c'), (4, NULL, '');\n\
         SELECT a, x, s FROM t ORDER BY a;\n\
         SELECT 1.0/3.0, 2.0/3.0, 1e-15, 123456.789, 9999999.5;\n\
         SELECT printf('%.0f|%.0f|%.0f', 0.5, 1.5, 2.5);\n\
         SELECT quote(s), typeof(x) FROM t ORDER BY a;\n\
         SELECT sum(x), avg(x), total(x) FROM t;\n";
    let ours = script_stdout(script, "NULL");

    let mut child = Command::new("sqlite3")
        .args(["-batch", "-noheader", "-nullvalue", "NULL", ":memory:"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ambient sqlite3 CLI on PATH");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let theirs = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        ours, theirs,
        "bundled {} vs ambient CLI disagree",
        sqlite_oracle::version()
    );
}
