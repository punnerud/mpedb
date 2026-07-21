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
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

/// Run a script through the bundled sqlite. Returns `Err(message)` when sqlite
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
    let stdout = sqlite_oracle::try_script_stdout(&script, "")
        .map_err(|e| format!("{e}\nscript:\n{script}"))?;
    Ok(stdout
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
    let mut with_sel: Vec<&str> = setup.to_vec();
    with_sel.push("INSERT INTO d (id, i) SELECT id + 10, 77 FROM d WHERE id = 1");
    assert_same(&with_sel, "SELECT id, i, r, s, z, nd, nn FROM d ORDER BY id");

    // …and through a FROM-less source. This used to report `Corrupt("table id
    // 4294967295 out of range")`: the INSERT footprint inserted the source's
    // table id unconditionally, and a FROM-less SELECT's "table" is the DUAL
    // sentinel (u32::MAX), which the sparse TableSet rightly rejects. The
    // SELECT footprint arm always guarded the sentinel; the INSERT arm now
    // does too — a FROM-less source reads no catalog table at all.
    let mut fromless: Vec<&str> = setup.to_vec();
    fromless.push("INSERT INTO d (id, i) SELECT 20, 77");
    assert_same(&fromless, "SELECT id, i, r, s, z, nd, nn FROM d ORDER BY id");
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
/// A DDL-declared column APPLIES sqlite's store affinity (#113,
/// `declared_int_real_text_columns_convert_like_sqlite_or_refuse` above), so
/// what is left here is the residue the conversion cannot land inside the rigid
/// type: text `'abc'` into an `integer` column stays text in sqlite, and mpedb
/// refuses it. Narrower than sqlite, never a different answer — and a
/// CONFIG-declared `type = "int64"` / `type = "text"` column refuses BOTH,
/// because there rigidity is the contract and no affinity is applied at all.
#[test]
fn a_rigid_column_refuses_what_sqlite_would_coerce() {
    let t = open();
    t.db.query("CREATE TABLE r (id integer PRIMARY KEY, n bigint, s varchar(10))", &[])
        .unwrap();
    // The conversion runs and leaves a text: an int column cannot hold it.
    assert!(t.db.query("INSERT INTO r (id, n) VALUES (1, 'abc')", &[]).is_err());
    // …while the value sqlite's TEXT affinity DOES convert now agrees with it.
    t.db.query("INSERT INTO r (id, s) VALUES (2, 5)", &[]).unwrap();
    // sqlite accepts both — the narrowing is the first row only.
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
    assert_eq!(
        mpedb_rows(&t.db, "SELECT typeof(s), s FROM r WHERE id = 2"),
        vec![vec!["text".to_string(), "5".to_string()]]
    );

    // The PROVENANCE half: the identical types declared in a TOML config keep
    // the rigid refusal, because that is what a rigid schema is for.
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-djgaps-cfg-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"c\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"n\"\ntype = \"int64\"\nnullable = true\n\
         [[table.column]]\nname = \"s\"\ntype = \"text\"\nnullable = true\n"
    );
    let cfg = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for bad in [
        "INSERT INTO c (id, n) VALUES (1, 'abc')",
        "INSERT INTO c (id, s) VALUES (2, 5)",
        "INSERT INTO c (id, n) VALUES (3, '12')",
    ] {
        assert!(cfg.query(bad, &[]).is_err(), "a config column must stay rigid: {bad}");
    }
    drop(cfg);
    let _ = std::fs::remove_file(&path);
}

/// Two Django run-2 gap rows — "`FILTER (WHERE …)` parse: `expected )`" (3 tests)
/// and "`expected )` after IN/EXISTS subquery" (3 tests) — are NOT gaps in
/// `FILTER`, `IN` or `EXISTS`. They are `LIKE … ESCAPE` and
/// `ORDER BY … NULLS FIRST/LAST` (unsupported, already their own gap row)
/// mis-reported at the token where the ENCLOSING construct next demanded a `)`.
///
/// Neither word is reserved, so the parser walked past the unsupported clause
/// and only noticed at the paren, blaming the wrong clause.
///
/// BOTH clauses now WORK (tests/like_escape.rs and tests/order_by_nulls.rs pin
/// their corners against the sqlite3 binary), so these statements are checked
/// here for what they were really about: the clause parses in each enclosing
/// construct — FILTER, IN-subquery, EXISTS-subquery — and the whole statement
/// agrees with sqlite.
#[test]
fn the_filter_and_subquery_paren_errors_are_really_like_escape_and_nulls_ordering() {
    let t = open();
    t.db.query("CREATE TABLE t (id integer PRIMARY KEY, x integer, tag varchar(20))", &[])
        .unwrap();
    t.db.query("CREATE TABLE u (uid integer PRIMARY KEY, x integer, tag varchar(20))", &[])
        .unwrap();

    // `LIKE … ESCAPE` in each of the three positions that used to blame a paren
    // — parses, runs, and matches sqlite row for row.
    let esc_setup = [
        "CREATE TABLE t (id integer PRIMARY KEY, x integer, tag varchar(20))",
        "CREATE TABLE u (uid integer PRIMARY KEY, x integer, tag varchar(20))",
        "INSERT INTO t (id, x, tag) VALUES (1, 1, 'ab')",
        "INSERT INTO t (id, x, tag) VALUES (2, 2, 'a%b')",
        "INSERT INTO t (id, x, tag) VALUES (3, 3, 'zz')",
        "INSERT INTO u (uid, x, tag) VALUES (1, 1, 'a%b')",
        "INSERT INTO u (uid, x, tag) VALUES (2, 2, 'ab')",
    ];
    for sql in [
        r#"SELECT count(*) FILTER (WHERE "t"."tag" LIKE 'a\%%' ESCAPE '\') FROM "t""#,
        r#"SELECT count(*) FILTER (WHERE "t"."tag" NOT LIKE 'a\%%' ESCAPE '\') FROM "t""#,
        r#"SELECT id FROM "t" WHERE "t"."x" IN (SELECT U0."x" FROM "u" U0 WHERE U0."tag" LIKE 'a\%%' ESCAPE '\') ORDER BY id"#,
        r#"SELECT id FROM "t" WHERE EXISTS(SELECT 1 FROM "u" U0 WHERE U0."tag" LIKE 'a\%%' ESCAPE '\') ORDER BY id"#,
        r#"SELECT id FROM "t" WHERE "t"."tag" LIKE 'a\%%' ESCAPE '\' ORDER BY id"#,
    ] {
        assert_same(&esc_setup, sql);
    }

    // `ORDER BY … NULLS FIRST/LAST` — likewise, including inside a subquery,
    // which is where the unconsumed `NULLS` used to resurface as a paren
    // complaint. tests/order_by_nulls.rs pins the placement itself; here the
    // point is that the ENCLOSING construct parses and the answer is sqlite's.
    let nulls_setup = [
        "CREATE TABLE t (id integer PRIMARY KEY, x integer, tag varchar(20))",
        "CREATE TABLE u (uid integer PRIMARY KEY, x integer, tag varchar(20))",
        "INSERT INTO t (id, x, tag) VALUES (1, 1, 'ab')",
        "INSERT INTO t (id, x, tag) VALUES (2, 2, 'zz')",
        "INSERT INTO t (id, x, tag) VALUES (3, NULL, 'qq')",
        "INSERT INTO u (uid, x, tag) VALUES (1, 1, 'ab')",
        "INSERT INTO u (uid, x, tag) VALUES (2, NULL, 'cd')",
    ];
    for sql in [
        r#"SELECT id FROM "t" WHERE "t"."x" IN (SELECT U0."x" FROM "u" U0 ORDER BY U0."x" ASC NULLS LAST) ORDER BY id"#,
        r#"SELECT id FROM "t" WHERE EXISTS(SELECT 1 FROM "u" U0 ORDER BY U0."x" NULLS LAST LIMIT 1) ORDER BY id"#,
        r#"SELECT id FROM "t" ORDER BY "t"."x" DESC NULLS FIRST, id"#,
        r#"SELECT id FROM "t" ORDER BY "t"."x" ASC NULLS LAST, id"#,
    ] {
        assert_same(&nulls_setup, sql);
    }

    // The enclosing constructs are NOT the gap: the identical statements with
    // the tail removed all run, and agree with sqlite.
    for sql in [
        r#"SELECT count(*) FILTER (WHERE "t"."tag" LIKE 'a%') FROM "t""#,
        r#"SELECT id FROM "t" WHERE "t"."x" IN (SELECT U0."x" FROM "u" U0 WHERE U0."tag" LIKE 'a%')"#,
        r#"SELECT id FROM "t" WHERE EXISTS(SELECT 1 FROM "u" U0 WHERE U0."tag" LIKE 'a%')"#,
        r#"SELECT id FROM "t" WHERE "t"."x" IN (SELECT U0."x" FROM "u" U0 ORDER BY U0."x" ASC)"#,
        r#"SELECT id FROM "t" WHERE EXISTS(SELECT 1 FROM "u" U0 ORDER BY U0."x" LIMIT 1)"#,
    ] {
        assert!(t.db.query(sql, &[]).is_ok(), "must parse and run: {sql}");
    }
    let setup = [
        "CREATE TABLE t (id integer PRIMARY KEY, x integer, tag varchar(20))",
        "CREATE TABLE u (uid integer PRIMARY KEY, x integer, tag varchar(20))",
        "INSERT INTO t (id, x, tag) VALUES (1, 1, 'ab')",
        "INSERT INTO t (id, x, tag) VALUES (2, 2, 'zz')",
        "INSERT INTO u (uid, x, tag) VALUES (1, 1, 'ab')",
    ];
    assert_same(
        &setup,
        r#"SELECT count(*) FILTER (WHERE "t"."tag" LIKE 'a%') FROM "t""#,
    );
    assert_same(
        &setup,
        r#"SELECT id FROM "t" WHERE EXISTS(SELECT 1 FROM "u" U0 WHERE U0."x" = "t"."x") ORDER BY id"#,
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

/// The INTEGER / REAL / TEXT affinities on a **DDL-declared** column: mpedb
/// APPLIES the conversion (task #113) and stays rigid about the result, so the
/// contract is *agree or refuse, never differ*. Driven over the same value
/// battery as the NUMERIC case, against the bundled oracle:
///
/// * whenever mpedb accepts the value, its `typeof()` AND its text must equal
///   sqlite's — that is the wrong-answer guard;
/// * whenever mpedb refuses, sqlite must have stored a value OUTSIDE the rigid
///   column's own storage class — that is the proof the refusal is the stated
///   narrowing and not a missing conversion.
#[test]
fn declared_int_real_text_columns_convert_like_sqlite_or_refuse() {
    for (decl, rigid_class) in
        [("bigint", "integer"), ("double precision", "real"), ("varchar(10)", "text")]
    {
        for lit in numeric_affinity_cases() {
            let create = format!("CREATE TABLE t (id INTEGER PRIMARY KEY, v {decl})");
            let insert = format!("INSERT INTO t (id, v) VALUES (1, {lit})");
            let setup: &[&str] = &[&create, &insert];
            let query = "SELECT typeof(v), CAST(v AS TEXT) FROM t";
            let want = sqlite_state(setup, query);
            let t = open();
            let mut refused = false;
            for s in setup {
                if t.db.query(s, &[]).is_err() {
                    refused = true;
                    break;
                }
            }
            if refused {
                // A refusal is only honest if sqlite kept the value in a class
                // this rigid column cannot hold. NULL is always holdable, so a
                // refusal there would be a bug.
                let got = &want[0][0];
                assert_ne!(got, rigid_class, "{decl} refused {lit}, but sqlite stored a {got}");
                assert_ne!(got, "null", "{decl} refused {lit}, which sqlite stored as NULL");
            } else {
                assert_eq!(mpedb_rows(&t.db, query), want, "{decl} / {lit}");
            }
        }
    }
}

/// A DDL-declared `blob` is sqlite's BLOB affinity: it converts NOTHING and
/// holds every class per value, which is why #113 made it the typeless column
/// rather than a rigid `Blob`. `UPDATE t SET b='aaaa'` really does leave
/// `typeof(b) = 'text'` — the shape three of CPython's `BlobTests` use.
#[test]
fn a_declared_blob_column_is_sqlites_typeless_column() {
    for lit in numeric_affinity_cases() {
        let create = "CREATE TABLE t (id INTEGER PRIMARY KEY, b blob)".to_string();
        let insert = format!("INSERT INTO t (id, b) VALUES (1, {lit})");
        let setup: &[&str] = &[&create, &insert];
        assert_same(setup, "SELECT typeof(b), CAST(b AS TEXT) FROM t");
    }
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, b blob)",
            "INSERT INTO t (id, b) VALUES (1, x'0102')",
            "UPDATE t SET b = 'aaaa' WHERE id = 1",
        ],
        "SELECT typeof(b), b, length(b), hex(b) FROM t",
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

    // The fourth query, which used to be the refusal: COMPARISON affinity now
    // converts the literal to a number BEFORE comparing, so `[2]` — not the
    // `[1, 2]` an unconverted class comparison would say.
    assert_same(setup, "SELECT id FROM t WHERE price < '40.0' ORDER BY id");
    assert_eq!(sqlite_state(setup, "SELECT id FROM t WHERE price < '40.0'"), vec![vec!["2"]]);
    // Against a NUMBER there is no affinity to apply and mpedb agrees.
    assert_same(setup, "SELECT id FROM t WHERE price < 40 ORDER BY id");
    // The whole operator family, both operand orders, and a text the affinity
    // CANNOT convert (which is then compared by storage class: every number
    // sorts below every text).
    assert_same(setup, "SELECT id FROM t WHERE price <= '35' ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE price = '1000' ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE '40.0' > price ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE price < 'abc' ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE price > 'abc' ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE price BETWEEN '30' AND '40' ORDER BY id");
    assert_same(setup, "SELECT id FROM t WHERE price < NULL ORDER BY id");
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

// ------------------------------------------------- negative LIMIT/OFFSET ----

/// sqlite reads a NEGATIVE `LIMIT` as "no limit" — the idiom Django emits for
/// every open-ended slice `qs[5:]` (`LIMIT -1 OFFSET 5`), because SQL has no
/// way to spell OFFSET without LIMIT. A negative `OFFSET` skips nothing.
/// mpedb refused both with a parse error; now it answers sqlite's rows.
#[test]
fn a_negative_limit_means_no_limit_like_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')",
    ];
    // Django's own shape: an open-ended slice.
    assert_same(setup, "SELECT id FROM t ORDER BY name ASC LIMIT -1 OFFSET 2");
    assert_same(setup, "SELECT id FROM t ORDER BY id LIMIT -1");
    assert_same(setup, "SELECT id FROM t ORDER BY id LIMIT -5");
    // A negative OFFSET skips nothing; `-0` is plain zero, not "no bound".
    assert_same(setup, "SELECT id FROM t ORDER BY id LIMIT 3 OFFSET -1");
    assert_same(setup, "SELECT id FROM t ORDER BY id LIMIT -0");
    // Inside a derived table and a subquery, where the bound drives planning.
    assert_same(setup, "SELECT COUNT(*) FROM (SELECT id FROM t ORDER BY id LIMIT -1 OFFSET 3) s");
    assert_same(setup, "SELECT id FROM t WHERE id IN (SELECT id FROM t LIMIT -1) ORDER BY id");
    // A LIMIT still binds to the whole compound, so a negative one before a
    // set operator is refused exactly as a positive one is (sqlite: "LIMIT
    // clause should come after UNION not before").
    let t = open();
    for s in setup {
        t.db.query(s, &[]).unwrap();
    }
    let e = t.db.query("SELECT id FROM t LIMIT -1 UNION SELECT id FROM t", &[]).unwrap_err();
    assert!(format!("{e}").contains("apply to the whole compound"), "{e}");
    assert!(sqlite_try(setup, "SELECT id FROM t LIMIT -1 UNION SELECT id FROM t").is_err());
}

// ------------------------------------------- expressions in INSERT VALUES ----

/// `INSERT … VALUES (<expression>)`. sqlite evaluates a VALUES row over no
/// row, which is the same thing a FROM-less `SELECT` is, and Django writes it
/// for every `RETURNING` insert of a database function. mpedb refused
/// ("INSERT values must be literals or parameters"); it now plans the row as
/// `INSERT … SELECT` and answers sqlite's rows.
#[test]
fn an_expression_in_insert_values_agrees_with_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE n (id INTEGER PRIMARY KEY, num INTEGER, v TEXT)",
        "INSERT INTO n VALUES (1, 10, 'Aa'), (2, 20, 'Bb')",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, name TEXT, num INTEGER)",
        // A function call, a scalar subquery, and arithmetic over one.
        "INSERT INTO c (id, name) VALUES (1, LOWER('ABC'))",
        "INSERT INTO c (id, num) VALUES (2, (SELECT MAX(num) FROM n))",
        "INSERT INTO c (id, num) VALUES (3, (SELECT MAX(num) + 5 FROM n))",
        "INSERT INTO c (id, name) VALUES (4, LOWER((SELECT v FROM n WHERE id = 2)))",
    ];
    assert_same(setup, "SELECT id, name, num FROM c ORDER BY id");
    assert_same(setup, "SELECT id, typeof(name), typeof(num) FROM c ORDER BY id");
    // RETURNING sees the computed value, not the expression.
    assert_same(
        &setup[..3],
        "INSERT INTO c (id, name) VALUES (9, UPPER('xy')) RETURNING id, name",
    );

    // Multi-row VALUES with an expression (PLAN_FORMAT 57): each cell that is
    // not a bare literal/param is a dual-row program. Django bulk_create.
    let t = open();
    for s in &setup[..3] {
        t.db.query(s, &[]).unwrap();
    }
    t.db
        .query(
            "INSERT INTO c (id, name) VALUES (7, 'x'), (8, LOWER('Y'))",
            &[],
        )
        .unwrap();
    let res = t
        .db
        .query("SELECT id, name FROM c WHERE id IN (7, 8) ORDER BY id", &[])
        .unwrap();
    match res {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int(7), Value::Text("x".into())],
                    vec![Value::Int(8), Value::Text("y".into())],
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------ the time keywords ---------

/// `CURRENT_TIMESTAMP` / `CURRENT_DATE` / `CURRENT_TIME` in EXPRESSION
/// position. sqlite defines each as the corresponding function of `'now'`, so
/// mpedb desugars it there; the DDL `DEFAULT` position keeps its separate
/// refusal (a stored default is a constant, not a per-row call).
#[test]
fn the_time_keywords_are_their_now_functions_like_sqlite() {
    let setup: &[&str] = &["CREATE TABLE st (id INTEGER PRIMARY KEY, opening TEXT)"];
    // Value-identical to the function form — sqlite's own definition, so the
    // equality holds in both engines without reading a clock.
    assert_same(
        setup,
        "SELECT current_timestamp = datetime('now'), current_date = date('now'), \
         current_time = time('now')",
    );
    assert_same(
        setup,
        "SELECT length(CURRENT_TIMESTAMP), length(CURRENT_DATE), length(CURRENT_TIME)",
    );
    // Django's shape: parenthesized, inside COALESCE, over an empty aggregate.
    assert_same(
        setup,
        "SELECT COALESCE(MAX(st.opening), (CURRENT_TIMESTAMP)) = datetime('now') FROM st WHERE 0 = 1",
    );
    // A QUOTED one is still a column name, as for every word with a meaning.
    assert_same(
        &["CREATE TABLE k (id INTEGER PRIMARY KEY, current_date TEXT)", "INSERT INTO k VALUES (1, 'x')"],
        "SELECT \"current_date\" FROM k",
    );
}

// ------------------------------------------------ `$` in an identifier ------

/// sqlite's `IdChar` includes `$`, so `crafted_alia$` is one identifier —
/// which Django's alias generator really emits. mpedb's parameter scanner ate
/// it. `$` still cannot START a token, so `$1` is still a parameter.
#[test]
fn a_dollar_continues_an_identifier_like_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE bk (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE au (id INTEGER PRIMARY KEY)",
        "INSERT INTO bk VALUES (1, 'x')",
        "INSERT INTO au VALUES (1)",
    ];
    assert_same(
        setup,
        "SELECT bk.name, crafted_alia$.id FROM bk \
         LEFT OUTER JOIN au crafted_alia$ ON (bk.id = crafted_alia$.id) ORDER BY bk.id",
    );
    assert_same(setup, "SELECT 1 AS a$b");
}

// ------------------------------------------------ compound arm typing -------

/// A compound column that is DYNAMICALLY typed in one arm (`any` — a typeless
/// column, a host UDF, a per-row CASE) and concrete in another. sqlite has no
/// static column type for a compound at all: every row keeps its own arm's
/// storage class, which is what `any` says. Two DIFFERENT concrete types still
/// refuse.
#[test]
fn a_typeless_compound_arm_unifies_like_sqlite() {
    let setup: &[&str] = &[
        // `datetime` is NUMERIC affinity → a typeless (`any`) column.
        "CREATE TABLE art (id INTEGER PRIMARY KEY, name TEXT, created datetime)",
        "CREATE TABLE rn (id INTEGER PRIMARY KEY, name TEXT, ord INTEGER)",
        "INSERT INTO art VALUES (1, 'a', '2010-01-01 00:00:00')",
        "INSERT INTO rn VALUES (1, 'b', 3)",
    ];
    // Django's shape: an `any` column against a `text`-typed expression.
    assert_same(
        setup,
        "SELECT art.name, art.created FROM art \
         UNION SELECT rn.name, strftime('%Y-%m-%d', '1991-10-10') FROM rn ORDER BY 1",
    );
    assert_same(
        setup,
        "SELECT typeof(x) FROM (SELECT created AS x FROM art UNION ALL SELECT 'zz' FROM rn) ORDER BY 1",
    );
    // Two concrete types still refuse — there the arms really disagree.
    let t = open();
    for s in setup {
        t.db.query(s, &[]).unwrap();
    }
    let e = t.db.query("SELECT name FROM art UNION SELECT ord FROM rn", &[]).unwrap_err();
    assert!(format!("{e}").contains("in one arm and"), "{e}");
}

// ------------------------------------------- comparison affinity of a CAST --

/// sqlite's comparison affinity applies when EITHER operand carries one, and a
/// `CAST` carries its target's. Django writes `CAST(<agg> AS NUMERIC) > ?` for
/// every `DecimalField` aggregate and binds the bound as TEXT, so without this
/// the pair met as int-vs-text and was refused.
#[test]
fn a_cast_carries_comparison_affinity_like_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE s (id INTEGER PRIMARY KEY, qty decimal(10,2))",
        "INSERT INTO s VALUES (1, 3), (2, 4), (3, 9)",
    ];
    // NUMERIC affinity converts the text operand; without the CAST neither
    // side carries an affinity and sqlite compares by storage class — the two
    // answers DIFFER, which is exactly why the rule has to be the rule.
    assert_same(setup, "SELECT CAST(SUM(qty) AS NUMERIC) > '0' FROM s");
    assert_same(setup, "SELECT SUM(qty) > '0' FROM s");
    assert_same(setup, "SELECT CAST(SUM(qty) AS NUMERIC) = '16' FROM s");
    assert_same(setup, "SELECT CAST(SUM(qty) AS NUMERIC) < 'abc' FROM s");
    assert_same(setup, "SELECT typeof(CAST(SUM(qty) AS NUMERIC)) FROM s");
    // A CAST to TEXT carries TEXT affinity, which is NOT applied against a
    // typeless operand (sqlite's rule: only numeric affinity crosses).
    assert_same(setup, "SELECT CAST(qty AS TEXT) = '3' FROM s ORDER BY id");
}

// ------------------------------------------ uncorrelated subquery in HAVING --

/// A subquery in `HAVING`. An UNCORRELATED one is filled once, before
/// dispatch, so the grouped predicate reads an ordinary parameter; a
/// CORRELATED one is still refused by name, because its slot would hold
/// whatever the last base row put there.
#[test]
fn an_uncorrelated_subquery_in_having_agrees_with_sqlite() {
    let setup: &[&str] = &[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, dept TEXT, g INTEGER)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER)",
        "INSERT INTO a VALUES (1, 'x', 10), (2, 'x', 20), (3, 'y', 10)",
        "INSERT INTO b VALUES (1, 10), (2, 10), (3, 30)",
    ];
    assert_same(
        setup,
        "SELECT a.dept, COUNT(*) FROM a GROUP BY a.dept \
         HAVING COUNT(*) > (SELECT COUNT(*) FROM b WHERE b.k = 30) ORDER BY 1",
    );
    // Django's shape: an `IN (SELECT …)` inside an OR, over a joined group.
    assert_same(
        setup,
        "SELECT a.dept, COUNT(*) FROM a GROUP BY a.dept \
         HAVING (COUNT(*) > 5 OR a.dept IN (SELECT 'x' FROM b)) ORDER BY 1",
    );
    // Correlated HAVING: first-row scratch per group (Django OuterRef).
    assert_same(
        setup,
        "SELECT a.dept FROM a GROUP BY a.dept \
         HAVING (SELECT COUNT(*) FROM b WHERE b.k = a.g) > 0 ORDER BY 1",
    );
}

// ------------------------------------ a typeless argument to a scalar fn ----

/// A DYNAMICALLY typed (`any`) argument to a built-in scalar. Its storage
/// class is not known until the row is read, so the type check belongs at the
/// row, not at compile time — Django's `CAST(FLOOR("price") AS NUMERIC)` over a
/// `decimal` column was refused whole.
///
/// Admitting it is what exposed `round()`: sqlite's `round` ALWAYS answers a
/// REAL, clamps a negative digit count to 0, and rounds through the decimal
/// rendering. mpedb returned the integer unchanged, rounded to TENS on a
/// negative count, and multiplied — three wrong answers, all fixed here and all
/// checked on VALUE and `typeof()`.
#[test]
fn a_typeless_argument_to_a_numeric_function_agrees_with_sqlite() {
    let setup: &[&str] = &[
        // `decimal(10,2)` is NUMERIC affinity → typeless: row 1 stores a REAL,
        // row 2 an INTEGER, so one column meets both classes.
        "CREATE TABLE n (id INTEGER PRIMARY KEY, price decimal(10,2))",
        "INSERT INTO n VALUES (1, 30.5), (2, 7), (3, NULL)",
    ];
    assert_same(
        setup,
        "SELECT id, abs(price), ceil(price), floor(price), trunc(price), round(price) \
         FROM n ORDER BY id",
    );
    assert_same(
        setup,
        "SELECT id, typeof(abs(price)), typeof(ceil(price)), typeof(floor(price)), \
         typeof(round(price)) FROM n ORDER BY id",
    );
    // Django's shape.
    assert_same(setup, "SELECT id, (CAST(FLOOR(price) AS NUMERIC)) FROM n ORDER BY id");
    assert_same(
        setup,
        "SELECT id, typeof(CAST(FLOOR(price) AS NUMERIC)) FROM n ORDER BY id",
    );
    // `round()` in its own right — the three corrections, on value and type.
    assert_same(setup, "SELECT round(7), typeof(round(7)), round(7, 2), round(-7)");
    assert_same(setup, "SELECT round(1234.5678, -2), round(1234.5678, 2), round(2.675, 2)");
    assert_same(setup, "SELECT round(1.005, 2), round(123.456, 40), round(-2.5), round(2.5)");
    // (`round(1e17)` agrees too, but this file's `render` prints a big float
    // as Rust does and sqlite as `1.0e+17`, so it cannot be compared here.)
    assert_same(setup, "SELECT typeof(round(1e17)), round(1e17) = 1e17");
    assert_same(setup, "SELECT round(NULL), round(1.5, NULL), typeof(round(NULL))");
}
