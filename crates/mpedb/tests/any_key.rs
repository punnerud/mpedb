//! A TYPELESS (`any`) column as a PRIMARY KEY / index key.
//!
//! `Schema::validate` used to refuse this on the grounds that "a key is
//! memcmp-ordered and `any` has no order across types". mpedb now has that
//! order — `Value::sort_cmp` is sqlite's storage-class order and
//! `keycode::KeySpec { class: true }` is that order as BYTES — so such a key is
//! allowed, and the tree is keyed by storage class rather than by mpedb type.
//! That is what unblocks Django's `queries` label, which dies at
//! `DateTimeField(primary_key=True)`.
//!
//! **What is allowed is the STORAGE, not the access path.** The planner never
//! probes an `any` key column (`planner::access`), so every predicate over one
//! stays a residual filter over a full scan. Two reasons, both wrong-answer
//! class: a probe would skip sqlite's *comparison affinity* on the bound, and
//! mpedb's own `Bool`/`Timestamp` have no storage class, so `sort_cmp` calls
//! them peers where a key must rank them. The `plan_never_probes_a_typeless_key`
//! test below is what holds that line.
//!
//! Everything semantic here is DIFFERENTIALLY verified against the `sqlite3`
//! CLI 3.45: mpedb runs the program, sqlite runs the identical DDL + INSERTs +
//! query, and the two outputs must match exactly.

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
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn db(tag: &str) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-anykey-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // A throwaway seed table satisfies the config schema; every table under
    // test is CREATEd at runtime so its declared types run the real DDL path.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// sqlite3's `.mode list` rendering of one value. Only classes with an
/// unambiguous textual form are ever projected by the batteries below — a raw
/// REAL or BLOB is compared through `typeof`/`hex` instead, so this never has to
/// guess at `%!.15g`.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("battery projected a value with no stable text form: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb refused `{sql}`: {e}"),
    }
}

/// Run `setup` (DDL + INSERTs, one statement per line, no trailing `;`) plus
/// `query` through the sqlite3 CLI and parse `.mode list` output. Statements
/// that sqlite REJECTS are expected to be rejected — `setup` here only ever
/// carries the ones both engines accept.
fn sqlite_rows(setup: &[String], query: &str) -> Vec<Vec<String>> {
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
        .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 failed on `{query}`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Run every statement through both engines and return the ones BOTH accepted,
/// asserting statement by statement that they agreed on accept-vs-reject. This
/// is the uniqueness cross-check: which values collide in an `any` key must be
/// exactly the values sqlite's `=` calls equal.
fn seed_both(d: &Database, stmts: &[&str]) -> Vec<String> {
    let mut accepted: Vec<String> = Vec::new();
    for s in stmts {
        let mine = d.query(s, &[]).is_ok();
        // Replay everything accepted so far, then this statement, in a fresh
        // sqlite process; a non-zero exit means sqlite rejected it.
        let mut script = String::new();
        for a in &accepted {
            script.push_str(a);
            script.push_str(";\n");
        }
        script.push_str(s);
        script.push_str(";\n");
        let mut child = Command::new("sqlite3")
            .arg(":memory:")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let theirs = child.wait_with_output().unwrap().status.success();
        assert_eq!(
            mine, theirs,
            "accept/reject disagreement on `{s}` (mpedb ok={mine}, sqlite ok={theirs})"
        );
        if mine {
            accepted.push((*s).to_string());
        }
    }
    accepted
}

fn compare(d: &Database, setup: &[String], queries: &[&str]) {
    for q in queries {
        assert_eq!(
            mpedb_rows(d, q),
            sqlite_rows(setup, q),
            "mpedb and sqlite3 3.45 disagree on `{q}`"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. The Django shape: `DateTimeField(primary_key=True)`.
// ---------------------------------------------------------------------------

/// `datetime` is sqlite's NUMERIC affinity — the class that is not a single
/// storage class, and therefore mpedb's `any`. Django writes ISO *strings* into
/// it, but the affinity means a numeric-looking one arrives as a number, so one
/// such column really does hold two classes at once.
#[test]
fn datetime_primary_key_matches_sqlite() {
    let d = db("dt");
    let stmts = [
        "CREATE TABLE dj (dt datetime NOT NULL PRIMARY KEY, name TEXT)",
        "INSERT INTO dj VALUES ('2020-01-01 10:00:00', 'a')",
        "INSERT INTO dj VALUES ('2021-06-06 00:00:00', 'b')",
        "INSERT INTO dj VALUES ('2019-12-31 23:59:59', 'c')",
        // NUMERIC affinity converts these on the way in: the integer 5, the
        // real 5.5 — so the column holds numbers next to its strings.
        "INSERT INTO dj VALUES (5, 'five')",
        "INSERT INTO dj VALUES ('5.5', 'fivefive')",
        "INSERT INTO dj VALUES (-3, 'minus')",
        // ...and these must COLLIDE with what is already there, exactly where
        // sqlite's `=` does: 5.0 is the integer 5, '5' numerifies to it too.
        "INSERT INTO dj VALUES (5.0, 'dup-real')",
        "INSERT INTO dj VALUES ('5', 'dup-text')",
        "INSERT INTO dj VALUES ('2020-01-01 10:00:00', 'dup-iso')",
        // A text the affinity cannot convert stays TEXT.
        "INSERT INTO dj VALUES ('abc', 'abc')",
    ];
    let setup = seed_both(&d, &stmts);
    compare(
        &d,
        &setup,
        &[
            // Tree order (the sort is elided over a PK-ordered scan) must be
            // sqlite's ORDER BY order over a mixed column.
            "SELECT typeof(dt), name FROM dj ORDER BY dt",
            "SELECT typeof(dt), name FROM dj ORDER BY dt DESC",
            // ...and the same rows through the EXPLICIT sorter: a two-key
            // ORDER BY is longer than the single-column PK, so the elision does
            // not fire and `cmp_rows`/`Value::sort_cmp` decides the order
            // instead of the tree. `name` never breaks a tie (the PK is
            // unique), so this must be the SAME sequence as the elided form —
            // that equality is the tree-order-vs-sorter-order agreement.
            "SELECT typeof(dt), name FROM dj ORDER BY dt, name",
            "SELECT typeof(dt), name FROM dj ORDER BY dt DESC, name DESC",
            "SELECT count(*) FROM dj",
            // Point lookups, string and numeric.
            "SELECT name FROM dj WHERE dt = '2021-06-06 00:00:00'",
            "SELECT name FROM dj WHERE dt = 5",
            "SELECT name FROM dj WHERE dt = 5.0",
            "SELECT name FROM dj WHERE dt = '5'",
            "SELECT name FROM dj WHERE dt = 'abc'",
            // Ranges whose bound is in a DIFFERENT class from most stored values.
            "SELECT name FROM dj WHERE dt > 4 ORDER BY name",
            "SELECT name FROM dj WHERE dt < 4 ORDER BY name",
            "SELECT name FROM dj WHERE dt >= '2020-01-01 10:00:00' ORDER BY name",
            "SELECT name FROM dj WHERE dt < '2020-01-01 10:00:00' ORDER BY name",
            "SELECT name FROM dj WHERE dt BETWEEN 0 AND 'zzz' ORDER BY name",
            "SELECT name FROM dj WHERE dt BETWEEN '2019' AND '2021' ORDER BY name",
            "SELECT typeof(min(dt)), typeof(max(dt)) FROM dj",
        ],
    );
}

// ---------------------------------------------------------------------------
// 2. The harshest shape: a column with NO declared type at all.
// ---------------------------------------------------------------------------

/// sqlite's no-affinity column converts NOTHING, so one PRIMARY KEY holds an
/// integer, a real, a text and a blob simultaneously — including the three pairs
/// the type-keyed encoder used to get wrong:
///
/// - `1` vs `1.0` (it split them; sqlite has one key),
/// - `0` vs `-0.0` (same),
/// - `'1'` vs `x'31'` (it ALIASED them — identical payload bytes, only the type
///   differs, and the type is not in the on-disk encoding — so an INSERT would
///   have overwritten an unrelated row),
/// - `9007199254740992.0` vs `9007199254740993`, where any `as f64` cast loses
///   the distinction.
#[test]
fn typeless_primary_key_matches_sqlite() {
    let d = db("nt");
    let stmts = [
        "CREATE TABLE nt (k PRIMARY KEY, tag TEXT)",
        "INSERT INTO nt VALUES (1, 'int1')",
        "INSERT INTO nt VALUES ('1', 'text1')",
        "INSERT INTO nt VALUES (x'31', 'blob31')",
        "INSERT INTO nt VALUES (-0.0, 'negzero')",
        "INSERT INTO nt VALUES (2.5, 'r2.5')",
        "INSERT INTO nt VALUES (-7, 'im7')",
        "INSERT INTO nt VALUES ('abc', 'tabc')",
        "INSERT INTO nt VALUES (x'00ff', 'blob00ff')",
        "INSERT INTO nt VALUES (9007199254740993, 'i2p53p1')",
        "INSERT INTO nt VALUES (9007199254740992.0, 'r2p53')",
        // Collisions, each verified against sqlite by `seed_both`.
        "INSERT INTO nt VALUES (1.0, 'dup-1.0')",
        "INSERT INTO nt VALUES (0, 'dup-0')",
        "INSERT INTO nt VALUES (0.0, 'dup-0.0')",
        "INSERT INTO nt VALUES ('abc', 'dup-abc')",
        "INSERT INTO nt VALUES (x'31', 'dup-blob31')",
    ];
    let setup = seed_both(&d, &stmts);
    compare(
        &d,
        &setup,
        &[
            "SELECT typeof(k), tag FROM nt ORDER BY k",
            "SELECT typeof(k), tag FROM nt ORDER BY k DESC",
            // The explicit sorter (two keys > a one-column PK ⇒ no elision).
            "SELECT typeof(k), tag FROM nt ORDER BY k, tag",
            "SELECT typeof(k), tag FROM nt ORDER BY k DESC, tag DESC",
            "SELECT count(*) FROM nt",
            "SELECT tag FROM nt WHERE k = 1",
            "SELECT tag FROM nt WHERE k = 1.0",
            "SELECT tag FROM nt WHERE k = '1'",
            "SELECT tag FROM nt WHERE k = x'31'",
            "SELECT tag FROM nt WHERE k = 0",
            "SELECT tag FROM nt WHERE k = -0.0",
            "SELECT tag FROM nt WHERE k = 9007199254740993",
            "SELECT tag FROM nt WHERE k = 9007199254740992.0",
            // Range bounds in every class against values in every class.
            "SELECT tag FROM nt WHERE k > 0 ORDER BY tag",
            "SELECT tag FROM nt WHERE k < 0 ORDER BY tag",
            "SELECT tag FROM nt WHERE k >= '1' ORDER BY tag",
            "SELECT tag FROM nt WHERE k < 'abc' ORDER BY tag",
            "SELECT tag FROM nt WHERE k > x'00' ORDER BY tag",
            "SELECT tag FROM nt WHERE k BETWEEN 1 AND 'abc' ORDER BY tag",
            "SELECT tag FROM nt WHERE k BETWEEN -1 AND 3 ORDER BY tag",
            "SELECT tag FROM nt WHERE k BETWEEN 9007199254740992.0 AND 9007199254740993 ORDER BY tag",
            "SELECT typeof(min(k)), typeof(max(k)) FROM nt",
            "SELECT count(*) FROM nt WHERE k > 9007199254740992.0",
        ],
    );
}

/// DML through a typeless PK: the rows an UPDATE/DELETE touches must be exactly
/// the rows the same WHERE selects. A key encoding that disagreed with the
/// filter would delete the wrong row — the failure mode the old refusal was
/// really guarding.
#[test]
fn dml_through_a_typeless_primary_key_matches_sqlite() {
    let d = db("dml");
    let stmts = [
        "CREATE TABLE nt (k PRIMARY KEY, tag TEXT)",
        "INSERT INTO nt VALUES (1, 'int1')",
        "INSERT INTO nt VALUES ('1', 'text1')",
        "INSERT INTO nt VALUES (x'31', 'blob31')",
        "INSERT INTO nt VALUES (2.5, 'r2.5')",
        "INSERT INTO nt VALUES (-7, 'im7')",
        "INSERT INTO nt VALUES ('abc', 'tabc')",
        "UPDATE nt SET tag = 'U' WHERE k = 1",
        "UPDATE nt SET tag = 'V' WHERE k = '1'",
        "UPDATE nt SET tag = 'W' WHERE k > 2",
        "DELETE FROM nt WHERE k = x'31'",
        "DELETE FROM nt WHERE k < 0",
    ];
    let setup = seed_both(&d, &stmts);
    compare(
        &d,
        &setup,
        &[
            "SELECT typeof(k), tag FROM nt ORDER BY k",
            "SELECT count(*) FROM nt",
        ],
    );
}

// ---------------------------------------------------------------------------
// 3. Indexes over a typeless column — and the agreement that licenses them.
// ---------------------------------------------------------------------------

const IX_ROWS: &[&str] = &[
    "INSERT INTO ix VALUES (1, 1)",
    "INSERT INTO ix VALUES (2, '1')",
    "INSERT INTO ix VALUES (3, x'31')",
    "INSERT INTO ix VALUES (4, NULL)",
    "INSERT INTO ix VALUES (5, 1.0)",
    "INSERT INTO ix VALUES (6, -0.0)",
    "INSERT INTO ix VALUES (7, 'abc')",
    "INSERT INTO ix VALUES (8, 2.5)",
    "INSERT INTO ix VALUES (9, NULL)",
    "INSERT INTO ix VALUES (10, 9007199254740993)",
    "INSERT INTO ix VALUES (11, 9007199254740992.0)",
    "INSERT INTO ix VALUES (12, x'00ff')",
];

const IX_QUERIES: &[&str] = &[
    "SELECT id, typeof(v) FROM ix ORDER BY v, id",
    "SELECT id, typeof(v) FROM ix ORDER BY v DESC, id",
    "SELECT id FROM ix WHERE v = 1 ORDER BY id",
    "SELECT id FROM ix WHERE v = '1' ORDER BY id",
    "SELECT id FROM ix WHERE v = x'31' ORDER BY id",
    "SELECT id FROM ix WHERE v IS NULL ORDER BY id",
    "SELECT id FROM ix WHERE v > 0 ORDER BY id",
    "SELECT id FROM ix WHERE v < 'abc' ORDER BY id",
    "SELECT id FROM ix WHERE v BETWEEN 1 AND 'z' ORDER BY id",
    "SELECT id FROM ix WHERE v > 9007199254740992.0 ORDER BY id",
    "SELECT count(*) FROM ix",
    "SELECT typeof(min(v)), typeof(max(v)) FROM ix",
];

/// **The index-vs-full-scan agreement, stated as a test.** The same data and
/// the same predicates, once with a secondary index over the typeless column
/// and once without: every answer must be identical, and both must be sqlite's
/// — which for sqlite genuinely IS an index scan, since sqlite will use it.
///
/// mpedb keeps the index maintained and never probes it, so this is a check
/// that maintaining it changes nothing an observer can see. It is the property
/// that would break first if the planner ever started probing such a key
/// without the affinity work that a probe needs.
#[test]
fn a_typeless_index_never_changes_an_answer() {
    let create = "CREATE TABLE ix (id INTEGER PRIMARY KEY, v)";
    let mut with_ix: Vec<&str> = vec![create, "CREATE INDEX ixv ON ix (v)"];
    with_ix.extend_from_slice(IX_ROWS);
    let mut without: Vec<&str> = vec![create];
    without.extend_from_slice(IX_ROWS);

    let indexed = db("ixyes");
    let setup = seed_both(&indexed, &with_ix);
    let plain = db("ixno");
    let plain_setup = seed_both(&plain, &without);

    for q in IX_QUERIES {
        let a = mpedb_rows(&indexed, q);
        let b = mpedb_rows(&plain, q);
        assert_eq!(a, b, "the index changed mpedb's answer to `{q}`");
        assert_eq!(a, sqlite_rows(&setup, q), "mpedb vs sqlite3 on `{q}` (indexed)");
        assert_eq!(b, sqlite_rows(&plain_setup, q), "mpedb vs sqlite3 on `{q}` (no index)");
    }
}

/// A UNIQUE index over a typeless column: which values collide must be exactly
/// the values sqlite's `=` calls equal — `1`/`1.0`, `0`/`-0.0`, but NOT `'1'`
/// against `x'31'`, and never two NULLs (an any-NULL row has no index entry, in
/// both engines).
#[test]
fn a_typeless_unique_index_collides_exactly_where_sqlite_does() {
    let d = db("uq");
    let stmts = [
        "CREATE TABLE ix (id INTEGER PRIMARY KEY, v)",
        "CREATE UNIQUE INDEX ixv ON ix (v)",
        "INSERT INTO ix VALUES (1, 1)",
        "INSERT INTO ix VALUES (2, '1')",
        "INSERT INTO ix VALUES (3, x'31')",
        "INSERT INTO ix VALUES (4, NULL)",
        "INSERT INTO ix VALUES (5, NULL)", // two NULLs are not a collision
        "INSERT INTO ix VALUES (6, 1.0)",  // collides with row 1
        "INSERT INTO ix VALUES (7, 0)",
        "INSERT INTO ix VALUES (8, -0.0)", // collides with row 7
        "INSERT INTO ix VALUES (9, 9007199254740992.0)",
        "INSERT INTO ix VALUES (10, 9007199254740993)", // does NOT collide
        "INSERT INTO ix VALUES (11, 'abc')",
        "INSERT INTO ix VALUES (12, 'abc')", // collides
        "UPDATE ix SET v = 1 WHERE id = 11", // collides with row 1
        "UPDATE ix SET v = 42 WHERE id = 11",
    ];
    let setup = seed_both(&d, &stmts);
    compare(
        &d,
        &setup,
        &[
            "SELECT id, typeof(v) FROM ix ORDER BY id",
            "SELECT id FROM ix WHERE v = 1 ORDER BY id",
            "SELECT count(*) FROM ix",
        ],
    );
}

// ---------------------------------------------------------------------------
// 4. The guard that makes all of the above provable.
// ---------------------------------------------------------------------------

/// **Load-bearing.** Allowing an `any` key column is allowing the STORAGE. The
/// planner must never turn a predicate over one into a key probe — not a
/// `PkPoint`, not a `PkRange`, not an `IndexPoint`/`IndexRange` — because a raw
/// bound skips sqlite's comparison affinity and because `Bool`/`Timestamp` have
/// no storage class to rank against. That is also what keeps the
/// comparison-affinity rule's own proof intact: a `ClassCmp` can only ever
/// rewrite a residual filter, since it is never an access path.
///
/// If this test ever fails, the batteries above stop being evidence for
/// anything: they would be exercising a probe whose bound was never converted.
#[test]
fn plan_never_probes_a_typeless_key() {
    let d = db("plan");
    for s in [
        "CREATE TABLE nt (k PRIMARY KEY, tag TEXT)",
        "CREATE TABLE dj (dt datetime NOT NULL PRIMARY KEY, name TEXT)",
        "CREATE TABLE ix (id INTEGER PRIMARY KEY, v, w INTEGER)",
        "CREATE INDEX ixv ON ix (v)",
        "CREATE UNIQUE INDEX ixvu ON ix (w)",
        "CREATE TABLE comp (a, b INTEGER, tag TEXT, PRIMARY KEY (a, b))",
    ] {
        d.query(s, &[]).unwrap();
    }
    let probing = ["PkPoint", "PkRange", "IndexPoint", "IndexRange"];
    for q in [
        "SELECT tag FROM nt WHERE k = 1",
        "SELECT tag FROM nt WHERE k = 'x'",
        "SELECT tag FROM nt WHERE k > 1",
        "SELECT tag FROM nt WHERE k BETWEEN 1 AND 5",
        "SELECT name FROM dj WHERE dt = '2020-01-01'",
        "SELECT name FROM dj WHERE dt < '2020-01-01'",
        "SELECT id FROM ix WHERE v = 1",
        "SELECT id FROM ix WHERE v > 1",
        "SELECT tag FROM comp WHERE a = 1 AND b = 2",
        "SELECT tag FROM comp WHERE a = 1",
        "SELECT tag FROM comp WHERE a > 1",
        "DELETE FROM nt WHERE k = 1",
        "UPDATE nt SET tag = 'x' WHERE k = 1",
        // A join whose inner side would otherwise probe the typeless key.
        "SELECT nt.tag FROM ix JOIN nt ON nt.k = ix.v",
        "SELECT nt.tag FROM ix JOIN nt ON nt.k = ix.w",
    ] {
        let plan = match d.query(&format!("EXPLAIN {q}"), &[]).unwrap() {
            ExecResult::Explain(t) => t,
            other => panic!("EXPLAIN {q} -> {other:?}"),
        };
        for p in probing {
            assert!(
                !plan.contains(p),
                "`{q}` planned a {p} over a typeless key column:\n{plan}"
            );
        }
    }

    // The two ORDER BY forms the differential batteries lean on really are two
    // different code paths: a single-key ORDER BY over the PK is satisfied by
    // TREE order and carries no sort, while a two-key one is longer than the PK
    // and goes through `cmp_rows`/`Value::sort_cmp`. If this ever stops being
    // true the batteries quietly stop comparing the tree against the sorter.
    let explain = |q: &str| match d.query(&format!("EXPLAIN {q}"), &[]).unwrap() {
        ExecResult::Explain(t) => t,
        other => panic!("EXPLAIN {q} -> {other:?}"),
    };
    assert!(
        !explain("SELECT tag FROM nt ORDER BY k").contains("order by"),
        "the PK-order elision no longer fires over a typeless key"
    );
    assert!(
        explain("SELECT tag FROM nt ORDER BY k, tag").contains("order by"),
        "the two-key ORDER BY no longer reaches the explicit sorter"
    );

    // The control: the SAME shapes over rigidly typed key columns still probe,
    // so the guard is narrow rather than an accidental blanket disable.
    for (q, want) in [
        ("SELECT v FROM ix WHERE id = 1", "PkPoint"),
        ("SELECT v FROM ix WHERE id > 1", "PkRange"),
        ("SELECT id FROM ix WHERE w = 2", "IndexPoint"),
    ] {
        let plan = match d.query(&format!("EXPLAIN {q}"), &[]).unwrap() {
            ExecResult::Explain(t) => t,
            other => panic!("EXPLAIN {q} -> {other:?}"),
        };
        assert!(plan.contains(want), "`{q}` lost its {want}:\n{plan}");
    }
}

/// **fix(wrong answer), found while auditing every key built over a typeless
/// column.** The correlated-subquery memo cached on `keycode::encode_key` — the
/// ORDERED encoding, which drops the mpedb type. Over an `any` column that
/// collides the text `'1'` with the blob `x'31'` and the integer `0` with the
/// real `0.0`, so the cache served one value's subquery result for the other:
///
/// ```text
/// SELECT id, (SELECT typeof(o.v) FROM m) FROM o ORDER BY id
///   sqlite: text · blob · integer · real
///   mpedb:  text · TEXT · integer · INTEGER
/// ```
///
/// A cache key must be INJECTIVE, which neither the ordered key (drops the
/// type) nor the grouping key (folds `1` and `1.0` on purpose) is;
/// `keycode::encode_key_exact` is.
#[test]
fn a_correlated_memo_over_a_typeless_column_matches_sqlite() {
    let d = db("memo");
    let stmts = [
        "CREATE TABLE o (id INTEGER PRIMARY KEY, v)",
        "INSERT INTO o VALUES (1, '1')",
        "INSERT INTO o VALUES (2, x'31')",
        "INSERT INTO o VALUES (3, 0)",
        "INSERT INTO o VALUES (4, 0.0)",
        "INSERT INTO o VALUES (5, 1)",
        "INSERT INTO o VALUES (6, 1.0)",
        "INSERT INTO o VALUES (7, NULL)",
        "CREATE TABLE m (id INTEGER PRIMARY KEY, w TEXT)",
        "INSERT INTO m VALUES (1, 'zz')",
    ];
    let setup = seed_both(&d, &stmts);
    compare(
        &d,
        &setup,
        &[
            "SELECT id, (SELECT typeof(o.v) FROM m) FROM o ORDER BY id",
            "SELECT id, (SELECT typeof(o.v) || '/' || m.w FROM m) FROM o ORDER BY id",
        ],
    );
}

/// The typeless key survives a reopen: the on-disk encoding is stable, and the
/// schema (which now permits such a key) round-trips through canonical bytes.
#[test]
fn a_typeless_key_survives_reopen() {
    let path = format!(
        "{}/mpedb-anykey-reopen-{}-{}.mpedb",
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for s in [
        "CREATE TABLE nt (k PRIMARY KEY, tag TEXT)",
        "INSERT INTO nt VALUES (1, 'int1')",
        "INSERT INTO nt VALUES ('1', 'text1')",
        "INSERT INTO nt VALUES (x'31', 'blob31')",
        "INSERT INTO nt VALUES ('abc', 'tabc')",
    ] {
        d.query(s, &[]).unwrap();
    }
    let before = mpedb_rows(&d, "SELECT typeof(k), tag FROM nt ORDER BY k");
    drop(d); // the FILE stays: reopening the same bytes is the point
    let db2 = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    assert_eq!(before, mpedb_rows(&db2, "SELECT typeof(k), tag FROM nt ORDER BY k"));
    // A duplicate still collides after the reopen — the tree really is keyed by
    // storage class, not by whatever the writer happened to hold in memory.
    assert!(db2.query("INSERT INTO nt VALUES (1.0, 'dup')", &[]).is_err());
    assert!(db2.query("INSERT INTO nt VALUES (2, 'new')", &[]).is_ok());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}
