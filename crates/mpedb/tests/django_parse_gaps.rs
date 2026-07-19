//! The parse-level gaps that stopped Django's ORM before a single query ran,
//! differential-tested against the `sqlite3` CLI.
//!
//! Each of these was a hard failure on the Django measurement, and each is a
//! case where mpedb REFUSED text sqlite accepts — so "closed" means mpedb and
//! sqlite3 return the SAME answer, not merely that mpedb stopped erroring.
//!
//!   1. `"t"."c"` — a quoted identifier as the qualifier of a dotted reference
//!      (already on main; the regression coverage lives in the sql crate).
//!   2. sqlite's declared-type vocabulary in `CREATE TABLE`
//!      (`varchar(100)`, `bigint`, `datetime`, `double precision`, …).
//!   3. `AUTOINCREMENT` — a deliberate refusal, see
//!      `autoincrement_refuses_by_name`. It is the one gap that cannot be
//!      closed without lying about the never-reuse guarantee.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-djgaps-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// One value as the sqlite3 CLI prints it (default `-separator |`, NULL as the
/// empty string).
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            // The CLI prints a whole float as `1.0`, Rust as `1`.
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i32).to_string(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb `{sql}` failed: {e}"),
    }
}

fn mpedb_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let t = open();
    for s in setup {
        t.db.query(s, &[]).unwrap_or_else(|e| panic!("mpedb setup `{s}` failed: {e}"));
    }
    mpedb_rows(&t.db, query)
}

/// Run a script through the `sqlite3` CLI. Returns `Err(stderr)` when sqlite
/// itself refused something — the callers that expect agreement assert success,
/// the ones probing sqlite's own limits read the error.
fn sqlite_try(setup: &[&str], query: &str) -> Result<Vec<Vec<String>>, String> {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI must be on PATH for this cross-check");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() || !stderr.is_empty() {
        return Err(format!("{stderr}\nscript:\n{script}"));
    }
    Ok(String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect())
}

fn sqlite_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    sqlite_try(setup, query).unwrap_or_else(|e| panic!("sqlite3 failed: {e}"))
}

fn assert_same(setup: &[&str], query: &str) {
    let m = mpedb_state(setup, query);
    let s = sqlite_state(setup, query);
    assert_eq!(m, s, "mpedb vs sqlite3 diverged for:\n{setup:?}\n{query}");
}

// ---------------------------------------------------------------- item 2 ----

/// Every declared type name Django's schema editor emits, plus the corners of
/// sqlite's affinity rule, checked against the REAL sqlite3 binary rather than
/// against a second copy of the algorithm.
///
/// The probe is `typeof()` after inserting a value of the class mpedb chose:
/// mpedb's rigid column type must agree with the storage class sqlite lands on
/// for that same value. That is the honest statement of the mapping — sqlite
/// converts per affinity, mpedb refuses a mismatch, and the two must agree on
/// what a well-typed client's value becomes.
#[test]
fn declared_types_agree_with_sqlite_affinity() {
    // (declared type, a value literal of the class mpedb maps it to, the
    // storage class sqlite3 must report for that value).
    let cases: &[(&str, &str, &str)] = &[
        ("integer", "3", "integer"),
        ("int", "3", "integer"),
        ("bigint", "3", "integer"),
        ("smallint", "3", "integer"),
        ("tinyint", "3", "integer"),
        ("int(8)", "3", "integer"),
        ("integer unsigned", "3", "integer"),
        ("unsigned big int", "3", "integer"),
        ("int2", "3", "integer"),
        ("floating point", "3", "integer"), // `INT` wins even inside `point`
        ("real", "1.5", "real"),
        ("float", "1.5", "real"),
        ("double", "1.5", "real"),
        ("double precision", "1.5", "real"),
        ("text", "'x'", "text"),
        ("varchar(100)", "'x'", "text"),
        ("varchar", "'x'", "text"),
        ("char(1)", "'x'", "text"),
        ("nchar(55)", "'x'", "text"),
        ("native character(70)", "'x'", "text"),
        ("clob", "'x'", "text"),
        ("blob", "x'00ff'", "blob"),
        // NUMERIC affinity → mpedb's per-value `Any` column: sqlite keeps a
        // number a number and a non-numeric string a string, and so does `Any`.
        ("decimal(10,2)", "1.5", "real"),
        ("numeric", "3", "integer"),
        ("date", "'2024-01-01'", "text"),
        ("datetime", "'2024-01-01 00:00:00'", "text"),
        ("varbinary", "3", "integer"),
        ("nosuchtype", "3", "integer"),
    ];
    for (decl, lit, want_class) in cases {
        let create = format!("CREATE TABLE t (id INTEGER PRIMARY KEY, x {decl})");
        let insert = format!("INSERT INTO t (id, x) VALUES (1, {lit})");
        let setup: &[&str] = &[&create, &insert];
        // sqlite's storage class for the value is what we claim to match.
        let s = sqlite_state(setup, "SELECT typeof(x) FROM t");
        assert_eq!(s, vec![vec![want_class.to_string()]], "sqlite3 on `{decl}` / `{lit}`");
        // …and the value itself must come back identical from both engines.
        // (Through `hex()` for blobs — the sqlite3 CLI writes raw bytes.)
        let proj = if *want_class == "blob" { "hex(x)" } else { "x" };
        assert_same(setup, &format!("SELECT id, {proj} FROM t ORDER BY id"));
    }
}

/// A typeless column, and a whole Django-shaped `CREATE TABLE` in sqlite's
/// vocabulary: it must not merely parse — the same INSERTs must produce the
/// same rows, including aggregates over them.
#[test]
fn a_django_shaped_create_table_agrees_with_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE \"app_author\" (\"id\" integer NOT NULL PRIMARY KEY, \
          \"name\" varchar(100) NOT NULL, \"code\" char(1) NULL, \
          \"rank\" bigint NULL, \"score\" double precision NULL, \
          \"created\" datetime NULL, \"data\" BLOB NULL, \"loose\")",
        "INSERT INTO \"app_author\" (\"id\", \"name\", \"code\", \"rank\", \"score\", \
          \"created\", \"loose\") VALUES (1, 'ann', 'a', 100, 1.5, '2024-01-01', 'txt')",
        "INSERT INTO \"app_author\" (\"id\", \"name\", \"code\", \"rank\", \"score\", \
          \"created\", \"loose\") VALUES (2, 'bob', NULL, -3, -0.25, '2024-02-02', 42)",
        "INSERT INTO \"app_author\" (\"id\", \"name\") VALUES (3, 'cyd')",
    ];
    assert_same(
        setup,
        "SELECT \"id\", \"name\", \"code\", \"rank\", \"score\", \"created\", \"loose\" \
         FROM \"app_author\" ORDER BY \"id\"",
    );
    assert_same(setup, "SELECT SUM(\"rank\"), COUNT(\"code\") FROM \"app_author\"");
    assert_same(
        setup,
        "SELECT \"name\" FROM \"app_author\" WHERE \"rank\" > -10 ORDER BY \"name\"",
    );
}

/// `ALTER TABLE … ADD COLUMN` speaks the identical vocabulary — `varchar(100)`
/// must not mean one thing in a CREATE and another in an ADD.
#[test]
fn add_column_speaks_the_same_type_vocabulary() {
    let setup: &[&str] = &[
        "CREATE TABLE t (id integer PRIMARY KEY)",
        "INSERT INTO t (id) VALUES (1)",
        "ALTER TABLE t ADD COLUMN note varchar(100)",
        "ALTER TABLE t ADD COLUMN amount double precision",
        "ALTER TABLE t ADD COLUMN n bigint DEFAULT 7",
        "INSERT INTO t (id, note, amount) VALUES (2, 'x', 1.5)",
    ];
    assert_same(setup, "SELECT id, note, amount, n FROM t ORDER BY id");
}

/// The size in `varchar(100)` is DROPPED, not enforced. sqlite ignores it too,
/// so enforcing it would reject rows sqlite stores — a wrong answer, not a
/// stricter schema.
#[test]
fn a_declared_size_is_not_a_length_limit_in_either_engine() {
    let setup: &[&str] = &[
        "CREATE TABLE t (id integer PRIMARY KEY, s varchar(1))",
        "INSERT INTO t (id, s) VALUES (1, 'much longer than one')",
    ];
    assert_same(setup, "SELECT id, s FROM t");
}

// ---------------------------------------------------------------- item 3 ----

/// `AUTOINCREMENT` refuses BY NAME.
///
/// sqlite's guarantee is that a rowid is never REUSED after a delete, held up by
/// a persisted per-table counter (`sqlite_sequence`). mpedb reads the current
/// maximum out of the PK tree and keeps no counter, so it cannot make that
/// promise. Accepting the keyword and quietly reusing ids would be wrong DATA
/// for the caller who asked for it — the one outcome worse than either
/// alternative.
///
/// The differential below is what makes the refusal honest rather than lazy: it
/// shows mpedb's plain `INTEGER PRIMARY KEY` matching sqlite's plain
/// (non-AUTOINCREMENT) rowid exactly — INCLUDING the reuse after deleting the
/// top row, which is precisely the behaviour AUTOINCREMENT would have to change.
#[test]
fn autoincrement_refuses_by_name() {
    let t = open();
    let e = t
        .db
        .query("CREATE TABLE ai (id INTEGER PRIMARY KEY AUTOINCREMENT, x INT)", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("AUTOINCREMENT"), "{e}");
    assert!(e.contains("reused"), "the refusal must say WHY: {e}");
    // sqlite ACCEPTS it — the gap is real, and this pins that it is a gap and
    // not a shared refusal.
    assert!(sqlite_try(&["CREATE TABLE ai (id INTEGER PRIMARY KEY AUTOINCREMENT)"], "SELECT 1")
        .is_ok());

    // Without the keyword: identical to sqlite, ids auto-assigned AND reused
    // after the top row is deleted.
    let setup: &[&str] = &[
        "CREATE TABLE ai (id INTEGER PRIMARY KEY, x INT)",
        "INSERT INTO ai (x) VALUES (1)",
        "INSERT INTO ai (x) VALUES (2)",
        "INSERT INTO ai (x) VALUES (3)",
        "DELETE FROM ai WHERE id = 3",
        "INSERT INTO ai (x) VALUES (4)",
    ];
    assert_same(setup, "SELECT id, x FROM ai ORDER BY id");
    // Said out loud: the id that came back is 3, the one AUTOINCREMENT forbids.
    assert_eq!(
        mpedb_state(setup, "SELECT MAX(id) FROM ai"),
        vec![vec!["3".to_string()]]
    );

    // `PRIMARY KEY ASC|DESC` — the same production — is accepted, direction
    // dropped, exactly as sqlite does with it.
    assert_same(
        &[
            "CREATE TABLE d (id INTEGER PRIMARY KEY ASC, x INT)",
            "INSERT INTO d (x) VALUES (7)",
        ],
        "SELECT id, x FROM d",
    );
}

// -------------------------------------------------------------- deviations ---

/// The rigid half of the mapping, stated as a REFUSAL and pinned as such.
///
/// sqlite converts at store time and keeps whatever survives: text `'abc'` into
/// an `integer` column stays text. mpedb's `integer` column is rigid and refuses
/// it. That is narrower than sqlite, never a different answer — and it is the
/// same rigidity a config-declared `type = "int64"` column has always had.
#[test]
fn a_rigid_column_refuses_what_sqlite_would_coerce() {
    let t = open();
    t.db.query("CREATE TABLE r (id integer PRIMARY KEY, n bigint, s varchar(10))", &[])
        .unwrap();
    for bad in [
        "INSERT INTO r (id, n) VALUES (1, 'abc')",
        "INSERT INTO r (id, s) VALUES (2, 5)",
    ] {
        assert!(t.db.query(bad, &[]).is_err(), "must refuse, not coerce: {bad}");
    }
    // sqlite accepts both — this is the documented narrowing, asserted so it
    // cannot silently become a coercion later.
    assert_eq!(
        sqlite_state(
            &[
                "CREATE TABLE r (id integer PRIMARY KEY, n bigint, s varchar(10))",
                "INSERT INTO r (id, n) VALUES (1, 'abc')",
                "INSERT INTO r (id, s) VALUES (2, 5)",
            ],
            "SELECT typeof(n), typeof(s) FROM r ORDER BY id",
        ),
        vec![
            vec!["text".to_string(), "null".to_string()],
            vec!["null".to_string(), "text".to_string()],
        ]
    );
}
