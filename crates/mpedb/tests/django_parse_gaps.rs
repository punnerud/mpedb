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
//!   4. `DEFAULT` / `CHECK` / `REFERENCES` in `CREATE TABLE`. The first two are
//!      ENFORCED; `REFERENCES` is parsed and not enforced, which is exactly
//!      sqlite's own default (`PRAGMA foreign_keys = OFF`) and is asserted as
//!      such rather than assumed.
//!   5. Named constraints (`CONSTRAINT c UNIQUE (a, b)`), at table and column
//!      level. The name is dropped; the constraint is not.

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

// ---------------------------------------------------------------- item 4 ----

/// `DEFAULT` is not decoration: it changes what an INSERT stores, and it must
/// store what sqlite stores — including through an INSERT that names only some
/// columns, and through `INSERT … SELECT`.
#[test]
fn column_defaults_store_what_sqlite_stores() {
    let setup: &[&str] = &[
        "CREATE TABLE d (id INTEGER PRIMARY KEY, i INT DEFAULT 42, \
          r REAL DEFAULT 1.5, s TEXT DEFAULT 'hi', z INT DEFAULT -7, \
          nd INT DEFAULT NULL, nn INT NOT NULL DEFAULT 9)",
        "INSERT INTO d (id) VALUES (1)",
        "INSERT INTO d (id, i, s) VALUES (2, 0, '')",
        "INSERT INTO d (id, i, r, s, z, nd, nn) VALUES (3, 1, 2.5, 'x', 8, 4, 5)",
    ];
    assert_same(setup, "SELECT id, i, r, s, z, nd, nn FROM d ORDER BY id");
    // A NOT NULL column WITH a default may legally be omitted, in both engines.
    assert_same(setup, "SELECT SUM(nn), SUM(i) FROM d");

    // `INSERT … SELECT` takes the defaults for the columns it does not name.
    // (Through a real FROM: a FROM-less `INSERT … SELECT 4, 77` hits an
    // unrelated pre-existing bug — `Corrupt("table id 4294967295 out of
    // range")`, the dual-table sentinel leaking into the insert path.)
    let mut with_sel: Vec<&str> = setup.to_vec();
    with_sel.push("INSERT INTO d (id, i) SELECT id + 10, 77 FROM d WHERE id = 1");
    assert_same(&with_sel, "SELECT id, i, r, s, z, nd, nn FROM d ORDER BY id");
}

/// A `DEFAULT` whose type does not match the declared column type fails the
/// `CREATE TABLE`, not the first INSERT.
#[test]
fn a_mistyped_default_fails_the_create() {
    let t = open();
    let e = t
        .db
        .query("CREATE TABLE bad (id INTEGER PRIMARY KEY, n INT DEFAULT 'nope')", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("DEFAULT"), "{e}");
    assert!(t.db.query("SELECT * FROM bad", &[]).is_err(), "table must not exist");
}

/// A `CHECK` that arrived through `CREATE TABLE` must actually FIRE. A
/// constraint that is stored, reported by the catalog and never enforced is
/// strictly worse than one that was refused.
#[test]
fn create_table_checks_are_enforced() {
    let t = open();
    t.db.query(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pos INT CHECK (pos >= 0), \
          lo INT, hi INT, CONSTRAINT ord CHECK (hi > lo))",
        &[],
    )
    .unwrap();
    t.db.query("INSERT INTO c (id, pos, lo, hi) VALUES (1, 0, 1, 2)", &[]).unwrap();
    for (sql, what) in [
        ("INSERT INTO c (id, pos, lo, hi) VALUES (2, -1, 1, 2)", "column CHECK"),
        ("INSERT INTO c (id, pos, lo, hi) VALUES (3, 0, 5, 5)", "table CHECK"),
    ] {
        let e = t.db.query(sql, &[]).unwrap_err().to_string();
        assert!(e.contains("CHECK"), "{what} must fire: {e}");
    }
    // An UPDATE into violation is refused too — the constraint is on the ROW,
    // not on the INSERT statement.
    let e = t.db.query("UPDATE c SET hi = 0 WHERE id = 1", &[]).unwrap_err().to_string();
    assert!(e.contains("CHECK"), "CHECK must fire on UPDATE: {e}");
    // …and only the legal row is there.
    assert_eq!(mpedb_rows(&t.db, "SELECT COUNT(*) FROM c"), vec![vec!["1".to_string()]]);

    // sqlite agrees on which rows are legal.
    assert_same(
        &[
            "CREATE TABLE c (id INTEGER PRIMARY KEY, pos INT CHECK (pos >= 0), \
              lo INT, hi INT, CONSTRAINT ord CHECK (hi > lo))",
            "INSERT INTO c (id, pos, lo, hi) VALUES (1, 0, 1, 2)",
            "INSERT INTO c (id, pos, lo, hi) VALUES (4, 3, 1, 9)",
        ],
        "SELECT id, pos, lo, hi FROM c ORDER BY id",
    );
}

/// A CHECK naming a column that does not exist fails the `CREATE TABLE`, rather
/// than landing in the catalog as a constraint that can never be loaded.
#[test]
fn an_uncompilable_check_fails_the_create() {
    let t = open();
    let e = t
        .db
        .query("CREATE TABLE bad (id INTEGER PRIMARY KEY, x INT CHECK (nosuch > 0))", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("nosuch"), "{e}");
    assert!(t.db.query("SELECT * FROM bad", &[]).is_err(), "table must not exist");
}

/// A CHECK created by DDL is enforced by a SECOND handle that attaches
/// afterwards — the programs are recompiled from the CATALOG's schema, not
/// carried over from the config's seed. Without this, whether a constraint fires
/// would depend on which process wrote it.
#[test]
fn a_ddl_check_is_enforced_by_a_later_attach() {
    let t = open();
    t.db.query("CREATE TABLE c2 (id INTEGER PRIMARY KEY, pos INT CHECK (pos >= 0))", &[])
        .unwrap();
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n",
        t.path
    );
    let db2 = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let e = db2.query("INSERT INTO c2 (id, pos) VALUES (1, -1)", &[]).unwrap_err().to_string();
    assert!(e.contains("CHECK"), "a second attach must enforce the CHECK: {e}");
    db2.query("INSERT INTO c2 (id, pos) VALUES (1, 1)", &[]).unwrap();
}

/// …and by the SAME transaction that created the table. This is the sharpest
/// case: the rows written alongside a `CREATE TABLE … CHECK (…)` are exactly the
/// ones an empty program slot would let through.
#[test]
fn a_check_fires_in_the_transaction_that_created_the_table() {
    let t = open();
    let mut s = t.db.begin().unwrap();
    s.query("CREATE TABLE c3 (id INTEGER PRIMARY KEY, pos INT CHECK (pos >= 0))", &[])
        .unwrap();
    s.query("INSERT INTO c3 (id, pos) VALUES (1, 5)", &[]).unwrap();
    let e = s.query("INSERT INTO c3 (id, pos) VALUES (2, -5)", &[]).unwrap_err().to_string();
    assert!(e.contains("CHECK"), "in-transaction CHECK must fire: {e}");
    s.commit().unwrap();
    assert_eq!(mpedb_rows(&t.db, "SELECT COUNT(*) FROM c3"), vec![vec!["1".to_string()]]);
}

/// `REFERENCES` is PARSED and NOT ENFORCED — which is exactly what sqlite does
/// under its default `PRAGMA foreign_keys = OFF`. Asserted rather than assumed:
/// the dangling child row goes in and the `ON DELETE CASCADE` does not fire, in
/// BOTH engines.
#[test]
fn references_is_parsed_and_not_enforced_like_sqlite_default() {
    let setup: &[&str] = &[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE ch (id INTEGER PRIMARY KEY, \
          p_id INT REFERENCES p (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED)",
        "INSERT INTO p (id) VALUES (1)",
        // 99 is not in `p` — sqlite (foreign_keys = OFF) accepts it, so mpedb must.
        "INSERT INTO ch (id, p_id) VALUES (1, 99)",
        "INSERT INTO ch (id, p_id) VALUES (2, 1)",
        "DELETE FROM p WHERE id = 1",
    ];
    // No cascade happened in either engine: both child rows survive their parent.
    assert_same(setup, "SELECT id, p_id FROM ch ORDER BY id");
    assert_same(setup, "SELECT COUNT(*) FROM p");
    // The table-level FOREIGN KEY spelling is equally inert.
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE ch (id INTEGER PRIMARY KEY, p_id INT, \
              CONSTRAINT fk FOREIGN KEY (p_id) REFERENCES p (id) ON DELETE CASCADE)",
            "INSERT INTO ch (id, p_id) VALUES (1, 404)",
        ],
        "SELECT id, p_id FROM ch",
    );
}

/// The whole Django-shaped `CREATE TABLE`: sqlite's type vocabulary, `DEFAULT`,
/// a column `CHECK`, a column `REFERENCES`, and named table constraints, all at
/// once — and the same INSERTs must produce the same rows.
#[test]
fn a_django_create_table_with_defaults_checks_and_fks_agrees_with_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE \"t\" (\
           \"id\" integer NOT NULL PRIMARY KEY, \
           \"name\" varchar(100) NOT NULL, \
           \"code\" char(1) NULL, \
           \"big\" bigint NULL, \
           \"pos\" integer unsigned NOT NULL CHECK (\"pos\" >= 0), \
           \"amount\" double precision NULL, \
           \"note\" text NOT NULL DEFAULT 'none', \
           \"n\" integer NOT NULL DEFAULT 7, \
           \"other_id\" integer NULL REFERENCES \"t\" (\"id\") \
             DEFERRABLE INITIALLY DEFERRED, \
           CONSTRAINT \"t_name_code_uniq\" UNIQUE (\"name\", \"code\"), \
           CONSTRAINT \"t_big_ck\" CHECK (\"big\" IS NULL OR \"big\" > 0))",
        "INSERT INTO \"t\" (\"id\", \"name\", \"code\", \"big\", \"pos\", \"amount\", \
           \"other_id\") VALUES (1, 'a', 'x', 100, 0, 1.5, NULL)",
        "INSERT INTO \"t\" (\"id\", \"name\", \"code\", \"big\", \"pos\", \"amount\", \
           \"other_id\") VALUES (2, 'b', 'y', 200, 9, -0.25, 1)",
    ];
    assert_same(
        setup,
        "SELECT \"id\", \"name\", \"code\", \"big\", \"pos\", \"amount\", \"note\", \"n\", \
         \"other_id\" FROM \"t\" ORDER BY \"id\"",
    );
    assert_same(setup, "SELECT SUM(\"big\"), COUNT(\"other_id\") FROM \"t\"");
    // …and the constraints in it are live in mpedb.
    let t = open();
    for s in setup {
        t.db.query(s, &[]).unwrap();
    }
    for (bad, what) in [
        (
            "INSERT INTO \"t\" (\"id\", \"name\", \"code\", \"pos\") VALUES (3, 'c', 'z', -1)",
            "column CHECK",
        ),
        (
            "INSERT INTO \"t\" (\"id\", \"name\", \"code\", \"big\", \"pos\") \
             VALUES (4, 'd', 'w', -5, 0)",
            "named table CHECK",
        ),
        (
            "INSERT INTO \"t\" (\"id\", \"name\", \"code\", \"pos\") VALUES (5, 'a', 'x', 0)",
            "named UNIQUE",
        ),
    ] {
        assert!(t.db.query(bad, &[]).is_err(), "{what} must still constrain");
    }
}

// ---------------------------------------------------------------- item 5 ----

/// Named table constraints: the NAME is accepted and dropped, the CONSTRAINT is
/// real. Django names every one it emits, so this is the difference between
/// `migrate` running and not — but a name that only appeared in error messages
/// must not become a name that turns a constraint off.
#[test]
fn named_table_constraints_still_constrain() {
    let t = open();
    t.db.query(
        "CREATE TABLE n (id INTEGER PRIMARY KEY, a INT, b INT, \
          CONSTRAINT n_ab_uniq UNIQUE (a, b), CONSTRAINT n_a_ck CHECK (a >= 0))",
        &[],
    )
    .unwrap();
    t.db.query("INSERT INTO n (id, a, b) VALUES (1, 1, 2)", &[]).unwrap();
    t.db.query("INSERT INTO n (id, a, b) VALUES (2, 1, 3)", &[]).unwrap();
    // The named UNIQUE rejects the duplicate pair…
    assert!(t.db.query("INSERT INTO n (id, a, b) VALUES (3, 1, 2)", &[]).is_err());
    // …and the named CHECK rejects the negative.
    assert!(t.db.query("INSERT INTO n (id, a, b) VALUES (4, -1, 9)", &[]).is_err());

    // `CONSTRAINT <name> PRIMARY KEY (…)` is the key, not decoration —
    // including the composite form Django writes for a through-table.
    t.db.query("CREATE TABLE n2 (a INT, b INT, CONSTRAINT n2_pk PRIMARY KEY (a, b))", &[])
        .unwrap();
    t.db.query("INSERT INTO n2 (a, b) VALUES (1, 1)", &[]).unwrap();
    t.db.query("INSERT INTO n2 (a, b) VALUES (1, 2)", &[]).unwrap();
    assert!(t.db.query("INSERT INTO n2 (a, b) VALUES (1, 1)", &[]).is_err());

    // Naming a constraint changes no answer: sqlite agrees on the rows.
    assert_same(
        &[
            "CREATE TABLE n (id INTEGER PRIMARY KEY, a INT, b INT, \
              CONSTRAINT n_ab_uniq UNIQUE (a, b), CONSTRAINT n_a_ck CHECK (a >= 0))",
            "INSERT INTO n (id, a, b) VALUES (1, 1, 2)",
            "INSERT INTO n (id, a, b) VALUES (2, 1, 3)",
        ],
        "SELECT id, a, b FROM n ORDER BY id",
    );
}

/// Column-level `CONSTRAINT <name>` in front of each constraint word, which is
/// the other half of sqlite's grammar and the shape a Django `unique=True` +
/// `db_check` field pair lands in.
#[test]
fn named_column_constraints_still_constrain() {
    let t = open();
    t.db.query(
        "CREATE TABLE m (id INTEGER PRIMARY KEY, \
          email TEXT CONSTRAINT m_email_nn NOT NULL CONSTRAINT m_email_uq UNIQUE, \
          pos INT CONSTRAINT m_pos_ck CHECK (pos >= 0) CONSTRAINT m_pos_df DEFAULT 3)",
        &[],
    )
    .unwrap();
    t.db.query("INSERT INTO m (id, email) VALUES (1, 'a@b')", &[]).unwrap();
    assert!(t.db.query("INSERT INTO m (id, email) VALUES (2, 'a@b')", &[]).is_err());
    assert!(t.db.query("INSERT INTO m (id) VALUES (3)", &[]).is_err());
    assert!(t
        .db
        .query("INSERT INTO m (id, email, pos) VALUES (4, 'c@d', -1)", &[])
        .is_err());
    // The named DEFAULT applied to row 1.
    assert_eq!(
        mpedb_rows(&t.db, "SELECT id, email, pos FROM m ORDER BY id"),
        vec![vec!["1".to_string(), "a@b".to_string(), "3".to_string()]]
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

// ------------------------------------------------------- store affinity ----

/// sqlite's **store-time NUMERIC affinity**, differentially, value by value.
///
/// This is the bug this section exists for: `decimal(10,2)` and a column with
/// NO declared type are BOTH `ColumnType::Any`, and sqlite treats them
/// OPPOSITELY — the first converts `'1.50'` to the real `1.5`, the second keeps
/// the text. Collapsing them onto one behaviour traded a clean refusal for a
/// WRONG ANSWER (`1.50`/`text` where sqlite says `1.5`/`real`), which is the one
/// trade this project never makes.
///
/// The projection is `typeof(v)` and `CAST(v AS TEXT)`: the storage class AND
/// the exact bytes, so a conversion that lands on the right class with the
/// wrong value (a `f64` rounding `'9007199254740993'` down, say) still fails.
fn numeric_affinity_cases() -> Vec<&'static str> {
    vec![
        // Lossless and reversible → converted.
        "'1.50'",
        "'0012'",
        "'1e3'",
        "'1E2'",
        "'1.5e2'",
        "'  7  '",
        "'+5'",
        "'-0'",
        "'5.'",
        "'.5'",
        "'-1.50'",
        "'0012.5'",
        "'1.0'",
        "'3.0'",
        "'0.0'",
        "'-00.00'",
        "'00'",
        "'1e-2'",
        "'0.1'",
        // Exact past 2^53 — an f64 would round this to ...992.
        "'9007199254740993'",
        // The i64 extremes: the pure-integer path keeps them, the real path
        // must not (sqlite ticket #3922).
        "'9223372036854775807'",
        "'-9223372036854775808'",
        "'9223372036854775808'",
        "'-9223372036854775809'",
        // Too big for an integer → real.
        "'99999999999999999999'",
        "'1e18'",
        "'1e19'",
        "'1e400'",
        "'0.30000000000000004'",
        // NOT losslessly numeric → stays TEXT.
        "'abc'",
        "''",
        "'  '",
        "'12abc'",
        "'0x10'",
        "'1_000'",
        "'1,5'",
        "'inf'",
        "'Infinity'",
        "'nan'",
        "'true'",
        "'2024-01-01'",
        "'2024-01-01 00:00:00'",
        // Non-text inputs: integers pass through, reals collapse when integral,
        // blobs are never parsed (unlike CAST), NULL stays NULL.
        "7",
        "1.5",
        "1.0",
        "-0.0",
        "1e100",
        "x'6162'",
        "NULL",
    ]
}

#[test]
fn numeric_affinity_converts_on_store_exactly_as_sqlite_does() {
    // Every NUMERIC-affinity spelling Django and the sqlite type vocabulary
    // produce. They must behave identically to each other AND to sqlite.
    for decl in ["numeric", "decimal(10,2)", "datetime", "date", "nosuchtype"] {
        for lit in numeric_affinity_cases() {
            let create = format!("CREATE TABLE t (id INTEGER PRIMARY KEY, v {decl})");
            let insert = format!("INSERT INTO t (id, v) VALUES (1, {lit})");
            let setup: &[&str] = &[&create, &insert];
            assert_same(setup, "SELECT typeof(v), CAST(v AS TEXT) FROM t");
        }
    }
}

/// The regression guard the fix must not trip: a column with NO declared type
/// is sqlite's BLOB ("NONE") affinity and stores **verbatim**. Same values, same
/// comparison, opposite expectation — which is exactly why one mpedb type could
/// not serve both.
#[test]
fn a_typeless_column_still_stores_verbatim_like_sqlite() {
    for lit in numeric_affinity_cases() {
        let create = "CREATE TABLE t (id INTEGER PRIMARY KEY, v)".to_string();
        let insert = format!("INSERT INTO t (id, v) VALUES (1, {lit})");
        let setup: &[&str] = &[&create, &insert];
        assert_same(setup, "SELECT typeof(v), CAST(v AS TEXT) FROM t");
    }
    // …and it really is the opposite behaviour, not a coincidence of the
    // sample: the same literal lands in two different storage classes.
    let m = mpedb_state(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n numeric, b)",
            "INSERT INTO t (id, n, b) VALUES (1, '1.50', '1.50')",
        ],
        "SELECT typeof(n), typeof(b) FROM t",
    );
    assert_eq!(m, vec![vec!["real".to_string(), "text".to_string()]]);
}

/// The other three affinities are RIGID columns in mpedb, and rigid means the
/// value is refused rather than converted. Pinned as a refusal so it can never
/// quietly become a coercion — and asserted against what sqlite would have
/// stored, so the narrowing stays visible.
#[test]
fn text_integer_real_blob_affinities_still_refuse_rather_than_convert() {
    let t = open();
    t.db.query(
        "CREATE TABLE r (id integer PRIMARY KEY, i bigint, f double precision, \
         s varchar(10), b blob)",
        &[],
    )
    .unwrap();
    for bad in [
        "INSERT INTO r (id, i) VALUES (1, '1.50')",
        "INSERT INTO r (id, f) VALUES (2, '1.50')",
        "INSERT INTO r (id, s) VALUES (3, 1.5)",
        "INSERT INTO r (id, b) VALUES (4, '1.50')",
    ] {
        assert!(t.db.query(bad, &[]).is_err(), "must refuse, not coerce: {bad}");
    }
    // What sqlite does with those same four, recorded so the gap is a stated
    // narrowing rather than an assumption.
    assert_eq!(
        sqlite_state(
            &[
                "CREATE TABLE r (id integer PRIMARY KEY, i bigint, f double precision, \
                 s varchar(10), b blob)",
                "INSERT INTO r (id, i) VALUES (1, '1.50')",
                "INSERT INTO r (id, f) VALUES (2, '1.50')",
                "INSERT INTO r (id, s) VALUES (3, 1.5)",
                "INSERT INTO r (id, b) VALUES (4, '1.50')",
            ],
            "SELECT typeof(i), typeof(f), typeof(s), typeof(b) FROM r ORDER BY id",
        ),
        vec![
            vec!["real", "null", "null", "null"],
            vec!["null", "real", "null", "null"],
            vec!["null", "null", "text", "null"],
            vec!["null", "null", "null", "text"],
        ]
        .into_iter()
        .map(|r| r.into_iter().map(str::to_string).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
}

/// Affinity is applied on EVERY way a value enters the column, not just
/// `INSERT … VALUES`: UPDATE, DEFAULT, `INSERT … SELECT`, and a bound
/// parameter. sqlite converts in all four, and a path that skipped one would
/// leave the same wrong answer behind a different statement.
#[test]
fn store_affinity_applies_on_update_default_and_insert_select() {
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v numeric)",
            "INSERT INTO t (id) VALUES (1)",
            "UPDATE t SET v = '1.50' WHERE id = 1",
        ],
        "SELECT typeof(v), CAST(v AS TEXT) FROM t",
    );
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v numeric DEFAULT '1.50')",
            "INSERT INTO t (id) VALUES (1)",
        ],
        "SELECT typeof(v), CAST(v AS TEXT) FROM t",
    );
    assert_same(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, v varchar(10))",
            "INSERT INTO s (id, v) VALUES (1, '1.50')",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v numeric)",
            "INSERT INTO t (id, v) SELECT id, v FROM s",
        ],
        "SELECT typeof(v), CAST(v AS TEXT) FROM t",
    );
    // A bound parameter takes the same conversion — this is the Django shape:
    // the ORM binds a Decimal as a STRING.
    let t = open();
    t.db.query("CREATE TABLE p (id INTEGER PRIMARY KEY, v numeric)", &[]).unwrap();
    t.db.query("INSERT INTO p (id, v) VALUES (1, $1)", &[Value::Text("1.50".into())])
        .unwrap();
    assert_eq!(
        mpedb_rows(&t.db, "SELECT typeof(v), CAST(v AS TEXT) FROM p"),
        vec![vec!["real".to_string(), "1.5".to_string()]]
    );
}

// ---------------------------------------------- the four Django queries ----

/// The measured Django regression, end to end. A `decimal(10,2)` column bound
/// as a STRING (which is what the ORM does) got stored as text, so the value,
/// the ordering and the extremum were all sqlite's answers' opposites.
///
/// Three of the four now AGREE with sqlite. The fourth — comparing the column
/// against a text literal — needs COMPARISON affinity (sqlite converts the
/// literal to a number BEFORE comparing) and is a clean refusal until that
/// lands, asserted here as such so it cannot quietly become a guess.
#[test]
fn the_django_decimal_shape_agrees_with_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE t (id integer NOT NULL PRIMARY KEY, price decimal(10,2) NOT NULL)",
        "INSERT INTO t (id, price) VALUES (1, '1000')",
        "INSERT INTO t (id, price) VALUES (2, '35')",
    ];
    assert_same(setup, "SELECT id, price, typeof(price) FROM t ORDER BY id");
    assert_same(setup, "SELECT id FROM t ORDER BY price");
    assert_same(setup, "SELECT MAX(price), MIN(price) FROM t");
    // …and with a mixed magnitude, where the column really does hold an INTEGER
    // and a REAL at once — the case a lexicographic order got backwards.
    let mixed: &[&str] = &[
        "CREATE TABLE t (id integer NOT NULL PRIMARY KEY, price decimal(10,2) NOT NULL)",
        "INSERT INTO t (id, price) VALUES (1, '1000')",
        "INSERT INTO t (id, price) VALUES (2, '35')",
        "INSERT INTO t (id, price) VALUES (3, '9.99')",
        "INSERT INTO t (id, price) VALUES (4, '200.5')",
    ];
    assert_same(mixed, "SELECT id FROM t ORDER BY price");
    assert_same(mixed, "SELECT MAX(price), MIN(price), COUNT(DISTINCT price) FROM t");
    assert_same(mixed, "SELECT id FROM t ORDER BY price DESC LIMIT 2");

    // The refusal. sqlite answers `[2]`; mpedb refuses rather than answering
    // `[1, 2]`, which is what an unconverted comparison would say.
    let t = open();
    for s in setup {
        t.db.query(s, &[]).unwrap();
    }
    let err = t.db.query("SELECT id FROM t WHERE price < '40.0'", &[]).unwrap_err();
    assert!(format!("{err}").contains("compare"), "{err}");
    assert_eq!(sqlite_state(setup, "SELECT id FROM t WHERE price < '40.0'"), vec![vec!["2"]]);
    // Against a NUMBER there is no affinity to apply and mpedb agrees.
    assert_same(setup, "SELECT id FROM t WHERE price < 40 ORDER BY id");
}

/// A NUMERIC column that holds a value sqlite could NOT convert keeps it as
/// text, so the column holds several storage classes at once — and sqlite orders
/// those `NULL < numbers < text < blob`. Every sort context has to follow that:
/// treating the pair as "equal" (which is what refusing inside a comparator
/// amounts to) is not an order at all.
#[test]
fn mixed_storage_classes_sort_in_sqlites_class_order() {
    let setup: &[&str] = &[
        "CREATE TABLE m (id INTEGER PRIMARY KEY, v NUMERIC)",
        "INSERT INTO m VALUES (1, 'abc')",
        "INSERT INTO m VALUES (2, '10')",
        "INSERT INTO m VALUES (3, NULL)",
        "INSERT INTO m VALUES (4, '2.5')",
        "INSERT INTO m VALUES (5, 'zz')",
    ];
    assert_same(setup, "SELECT id FROM m ORDER BY v, id");
    assert_same(setup, "SELECT id FROM m ORDER BY v DESC, id");
    assert_same(setup, "SELECT MAX(v), MIN(v) FROM m");
    assert_same(setup, "SELECT typeof(MAX(v)), typeof(MIN(v)) FROM m");
    // The same over a TYPELESS column, which stores every one of those verbatim.
    let loose: &[&str] = &[
        "CREATE TABLE m (id INTEGER PRIMARY KEY, v)",
        "INSERT INTO m VALUES (1, 'abc')",
        "INSERT INTO m VALUES (2, 10)",
        "INSERT INTO m VALUES (3, NULL)",
        "INSERT INTO m VALUES (4, 2.5)",
        "INSERT INTO m VALUES (5, x'00')",
    ];
    assert_same(loose, "SELECT id FROM m ORDER BY v, id");
    assert_same(loose, "SELECT id FROM m ORDER BY v DESC, id");
}
