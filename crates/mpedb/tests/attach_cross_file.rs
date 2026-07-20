//! #51: `ATTACH DATABASE` + cross-file SELECT, differentially against the
//! bundled sqlite oracle (which ATTACHes real files the same way).
//!
//! Two layers:
//! - a DIFFERENTIAL battery: the identical two/three-file fixture is built on
//!   both engines from the same DDL+INSERT text, and every enabled cross-file
//!   query shape must produce identical rows;
//! - SEMANTICS probes: the resolution rules the resolver claims to implement
//!   (main-first shadowing, attach-order, alias hiding, error identities) are
//!   asserted against the oracle so a rusqlite bump that moves them fails
//!   loudly here, not silently in the resolver.
//!
//! What v1 REFUSES BY NAME is asserted refused (never answered differently):
//! writes/DDL touching an attached db, ATTACH of a missing file / `:memory:`
//! / bound params, cross-file statements inside an open transaction, and
//! same-named unaliased tables from two databases.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> String {
    if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".into()
    } else {
        std::env::temp_dir().to_string_lossy().into_owned()
    }
}

/// The whole fixture: one main mpedb + two attachable member files, plus the
/// oracle-side sqlite twin files, all deleted on drop.
struct Fix {
    main: Database,
    files: Vec<String>,
    /// sqlite twin files for the oracle scripts (removed per run).
    oracle_other: String,
    oracle_third: String,
    other_path: String,
    third_path: String,
}
impl Deref for Fix {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.main
    }
}
impl Drop for Fix {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
            let _ = std::fs::remove_file(format!("{f}-wal"));
        }
    }
}

const MAIN_DDL: &[&str] = &[
    "CREATE TABLE t (a INTEGER PRIMARY KEY, tag TEXT)",
    "CREATE TABLE s (id INTEGER PRIMARY KEY, val INT)",
];
const MAIN_ROWS: &[&str] = &[
    "INSERT INTO t (a, tag) VALUES (1, 'main-1')",
    "INSERT INTO t (a, tag) VALUES (2, 'main-2')",
    "INSERT INTO t (a, tag) VALUES (3, 'main-3')",
    "INSERT INTO s (id, val) VALUES (1, 100)",
];
/// `other`: u (data + NULLs), plus a `t` that SHADOW-tests against main's.
const OTHER_DDL: &[&str] = &[
    "CREATE TABLE u (x INTEGER PRIMARY KEY, y INT, z TEXT)",
    "CREATE TABLE t (a INTEGER PRIMARY KEY, tag TEXT)",
];
const OTHER_ROWS: &[&str] = &[
    "INSERT INTO u (x, y, z) VALUES (1, 10, 'one')",
    "INSERT INTO u (x, y, z) VALUES (2, 20, NULL)",
    "INSERT INTO u (x, y, z) VALUES (3, 20, 'three')",
    "INSERT INTO u (x, y, z) VALUES (4, NULL, 'four')",
    "INSERT INTO t (a, tag) VALUES (99, 'other-99')",
];
/// `third`: its own `u` (attach-order test) and `w`.
const THIRD_DDL: &[&str] = &[
    "CREATE TABLE u (x INTEGER PRIMARY KEY, y INT)",
    "CREATE TABLE w (k INTEGER PRIMARY KEY, note TEXT)",
];
const THIRD_ROWS: &[&str] = &[
    "INSERT INTO u (x, y) VALUES (7, 70)",
    "INSERT INTO w (k, note) VALUES (1, 'w-one')",
    "INSERT INTO w (k, note) VALUES (2, 'w-two')",
];

fn seed_db(path: &str, ddl: &[&str], rows: &[&str]) -> Database {
    let _ = std::fs::remove_file(path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in ddl.iter().chain(rows) {
        db.query(stmt, &[]).unwrap();
    }
    db
}

/// Build the three mpedb files and ATTACH `other` (and optionally `third`)
/// on the main handle — through the actual statement path.
fn fix(tag: &str, attach_third: bool) -> Fix {
    let dir = scratch_dir();
    let id = format!("{}-{}", std::process::id(), UNIQ.fetch_add(1, Ordering::Relaxed));
    let main_path = format!("{dir}/mpedb-att-{tag}-{id}-main.mpedb");
    let other_path = format!("{dir}/mpedb-att-{tag}-{id}-other.mpedb");
    let third_path = format!("{dir}/mpedb-att-{tag}-{id}-third.mpedb");
    let oracle_other = format!("{dir}/mpedb-att-{tag}-{id}-oracle-other.db");
    let oracle_third = format!("{dir}/mpedb-att-{tag}-{id}-oracle-third.db");

    let main = seed_db(&main_path, MAIN_DDL, MAIN_ROWS);
    drop(seed_db(&other_path, OTHER_DDL, OTHER_ROWS));
    drop(seed_db(&third_path, THIRD_DDL, THIRD_ROWS));

    main.query(&format!("ATTACH DATABASE '{other_path}' AS other"), &[])
        .unwrap();
    if attach_third {
        main.query(&format!("ATTACH DATABASE '{third_path}' AS third"), &[])
            .unwrap();
    }
    Fix {
        main,
        files: vec![
            main_path,
            other_path.clone(),
            third_path.clone(),
            oracle_other.clone(),
            oracle_third.clone(),
        ],
        oracle_other,
        oracle_third,
        other_path,
        third_path,
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// The oracle side: in-memory main, ATTACH the twin files, build everything
/// from the SAME DDL/INSERT text, run the query.
fn sqlite_rows(f: &Fix, query: &str) -> Vec<Vec<String>> {
    let _ = std::fs::remove_file(&f.oracle_other);
    let _ = std::fs::remove_file(&f.oracle_third);
    let mut script = String::new();
    for stmt in MAIN_DDL.iter().chain(MAIN_ROWS) {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(&format!("ATTACH '{}' AS other;\n", f.oracle_other));
    for stmt in OTHER_DDL.iter().chain(OTHER_ROWS) {
        // Build the attached file's content via qualified DDL/INSERTs.
        script.push_str(&qualify(stmt, "other"));
        script.push_str(";\n");
    }
    script.push_str(&format!("ATTACH '{}' AS third;\n", f.oracle_third));
    for stmt in THIRD_DDL.iter().chain(THIRD_ROWS) {
        script.push_str(&qualify(stmt, "third"));
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Point a fixture DDL/INSERT statement at an attached db on the oracle.
fn qualify(stmt: &str, db: &str) -> String {
    stmt.replacen("CREATE TABLE ", &format!("CREATE TABLE {db}."), 1)
        .replacen("INSERT INTO ", &format!("INSERT INTO {db}."), 1)
}

fn cell_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s.parse::<i64>().map(|y| y == *i).unwrap_or(false),
        Value::Float(x) => match s.parse::<f64>() {
            Ok(y) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            Err(_) => false,
        },
        Value::Bool(b) => s == if *b { "1" } else { "0" },
        Value::Text(t) => s == t,
        other => panic!("unexpected value in cross-file result: {other:?}"),
    }
}

fn assert_same(f: &Fix, sql: &str) {
    let ours = mpedb_rows(f, sql);
    let theirs = sqlite_rows(f, sql);
    assert_eq!(
        ours.len(),
        theirs.len(),
        "row count differs for `{sql}`:\n mpedb={ours:?}\n sqlite={theirs:?}"
    );
    for (i, (m, s)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(m.len(), s.len(), "arity differs row {i} for `{sql}`");
        for (mv, sv) in m.iter().zip(s) {
            assert!(
                cell_matches(mv, sv),
                "cell differs for `{sql}` row {i}: mpedb={mv:?} sqlite={sv:?}\n\
                 full mpedb={ours:?}\n full sqlite={theirs:?}"
            );
        }
    }
}

// =========================================================================
// The differential battery — every enabled cross-file query shape.
// =========================================================================

#[test]
fn differential_cross_file_selects() {
    let f = fix("diff", true);
    for sql in [
        // qualified single-table
        "SELECT * FROM other.u ORDER BY x",
        "SELECT y FROM other.u WHERE x = 2",
        "SELECT z FROM other.u WHERE y = 20 ORDER BY x",
        "SELECT x FROM other.u ORDER BY x LIMIT 2 OFFSET 1",
        "SELECT x FROM other.u ORDER BY y DESC, x ASC",
        // main. qualification and mixing
        "SELECT tag FROM main.t ORDER BY a",
        "SELECT t.a, u.y FROM main.t JOIN other.u ON u.x = t.a ORDER BY t.a",
        // 3-part column names (unaliased entries)
        "SELECT other.u.y FROM other.u ORDER BY other.u.x DESC",
        "SELECT main.t.tag FROM main.t ORDER BY main.t.a",
        // bare-name resolution: u only lives in attached dbs (other first)
        "SELECT * FROM u ORDER BY x",
        // bare-name shadowing: t is in main AND other → main wins
        "SELECT tag FROM t ORDER BY a",
        // aggregates over an attached table
        "SELECT count(*), sum(y) FROM other.u",
        "SELECT y, count(*) FROM other.u GROUP BY y ORDER BY y",
        "SELECT max(x) - min(x) FROM other.u",
        // joins: inner, left, comma/cartesian, three files
        "SELECT t.a, u.y FROM t LEFT JOIN other.u u ON u.x = t.a + 1 ORDER BY t.a",
        "SELECT * FROM main.t, other.u ORDER BY a, x",
        "SELECT t.a, o.y, w.note FROM t JOIN other.u o ON o.x = t.a \
         JOIN third.w w ON w.k = t.a ORDER BY t.a",
        // subqueries across files
        "SELECT a FROM t WHERE a IN (SELECT x FROM other.u) ORDER BY a",
        "SELECT (SELECT max(x) FROM other.u), a FROM t ORDER BY a",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM other.u WHERE u.x = t.a) ORDER BY a",
        // compound across files
        "SELECT a FROM t UNION SELECT x FROM other.u ORDER BY 1",
        "SELECT a FROM t INTERSECT SELECT x FROM other.u ORDER BY 1",
        // aliased attached tables, incl. a cross-file self-join
        "SELECT z1.y, z2.y FROM other.u z1 JOIN other.u z2 ON z2.x = z1.x + 1 \
         ORDER BY z1.x",
        // same-named tables via aliases
        "SELECT m.tag, o.tag FROM main.t m, other.t o ORDER BY m.a",
        // DISTINCT + expression projection
        "SELECT DISTINCT y FROM other.u ORDER BY y",
        "SELECT x + y FROM other.u WHERE y IS NOT NULL ORDER BY x",
        // third-db bare name (only in third)
        "SELECT note FROM w ORDER BY k",
    ] {
        assert_same(&f, sql);
    }
}

/// The prepared-plan hot path: prepare once, execute repeatedly; per-file
/// snapshots are taken fresh per execution, so a write to the attached file
/// through an independent handle is visible on the next execute.
#[test]
fn prepared_cross_plan_executes_and_sees_fresh_member_snapshots() {
    let f = fix("prep", false);
    let h = f.prepare("SELECT count(*) FROM other.u").unwrap();
    let n = |r: ExecResult| match r {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        o => panic!("{o:?}"),
    };
    assert_eq!(n(f.execute(&h, &[]).unwrap()), Value::Int(4));
    // Write through an independent handle on the attached FILE (cross-file
    // writes through the attach are refused; direct handles are the v1 way).
    let side = Database::open_from_file(std::path::Path::new(&f.other_path)).unwrap();
    side.query("INSERT INTO u (x, y, z) VALUES (9, 90, 'nine')", &[])
        .unwrap();
    assert_eq!(n(f.execute(&h, &[]).unwrap()), Value::Int(5));
}

/// Parameters flow into cross plans exactly as into ordinary ones.
#[test]
fn cross_plan_with_parameters() {
    let f = fix("params", false);
    let h = f
        .prepare("SELECT y FROM other.u WHERE x = $1")
        .unwrap();
    match f.execute(&h, &[Value::Int(2)]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(20)]]),
        o => panic!("{o:?}"),
    }
}

// =========================================================================
// Semantics probes: the derived rules, pinned against the oracle.
// =========================================================================

/// Alias hiding (probes P5/P5b): with `FROM other.u AS z`, both `other.u.y`
/// and `u.y` are invalid on BOTH engines.
#[test]
fn alias_hides_db_qualified_names_like_sqlite() {
    let f = fix("alias", false);
    for sql in [
        "SELECT other.u.y FROM other.u AS z",
        "SELECT u.y FROM other.u AS z",
    ] {
        let script = format!(
            "ATTACH '{}' AS other; CREATE TABLE other.u (x INTEGER PRIMARY KEY, y INT); {sql};",
            f.oracle_other
        );
        let _ = std::fs::remove_file(&f.oracle_other);
        assert!(
            sqlite_oracle::try_script_stdout(&script, "NULL").is_err(),
            "oracle accepted `{sql}` — the P5 rule moved"
        );
        assert!(f.query(sql, &[]).is_err(), "mpedb accepted `{sql}`");
    }
}

/// Unknown database qualifier: sqlite says `no such table: nope.t`; so do we.
#[test]
fn unknown_db_matches_sqlite_error_identity() {
    let f = fix("nodb", false);
    let e = f.query("SELECT * FROM nope.t", &[]).unwrap_err();
    assert!(e.to_string().contains("no such table: nope.t"), "{e}");
}

/// ATTACH/DETACH statement error identities (probes P7/P8).
#[test]
fn attach_detach_error_identities() {
    let f = fix("errs", false);
    // duplicate name (P8)
    let e = f
        .query(&format!("ATTACH '{}' AS other", f.third_path), &[])
        .unwrap_err();
    assert!(e.to_string().contains("database other is already in use"), "{e}");
    // reserved names
    let e = f
        .query(&format!("ATTACH '{}' AS main", f.third_path), &[])
        .unwrap_err();
    assert!(e.to_string().contains("database main is already in use"), "{e}");
    // detach unknown (P7)
    let e = f.query("DETACH nosuch", &[]).unwrap_err();
    assert!(e.to_string().contains("no such database: nosuch"), "{e}");
    // detach main (P7b)
    let e = f.query("DETACH main", &[]).unwrap_err();
    assert!(e.to_string().contains("cannot detach database main"), "{e}");
    // same FILE under a second name is allowed (P8b)
    f.query(&format!("ATTACH '{}' AS other2", f.other_path), &[])
        .unwrap();
    assert_same_counts(&f, "SELECT count(*) FROM other.u", "SELECT count(*) FROM other2.u");
}

fn assert_same_counts(f: &Fix, a: &str, b: &str) {
    assert_eq!(mpedb_rows(f, a), mpedb_rows(f, b));
}

/// DETACH really detaches: names stop resolving, and a prepared cross plan
/// fails closed with PlanInvalidated instead of reading a stale member list.
#[test]
fn detach_invalidates_names_and_cached_plans() {
    let f = fix("detach", false);
    let h = f.prepare("SELECT count(*) FROM other.u").unwrap();
    f.execute(&h, &[]).unwrap();
    f.query("DETACH DATABASE other", &[]).unwrap();
    // The name is gone.
    let e = f.query("SELECT * FROM other.u", &[]).unwrap_err();
    assert!(e.to_string().contains("no such table: other.u"), "{e}");
    // The cached plan fails closed: DETACH clears the cross cache, so the
    // hash is unknown (re-prepare); a racing clone would hit the epoch check
    // and report PlanInvalidated — both are the fail-closed re-prepare path.
    assert!(matches!(
        f.execute(&h, &[]),
        Err(mpedb::Error::PlanInvalidated) | Err(mpedb::Error::UnknownPlan(_))
    ));
    // Re-attach under the same name works and requires a fresh prepare.
    f.query(&format!("ATTACH '{}' AS other", f.other_path), &[])
        .unwrap();
    assert_eq!(
        mpedb_rows(&f, "SELECT count(*) FROM other.u"),
        vec![vec![Value::Int(4)]]
    );
}

/// Member-side DDL (through any handle) invalidates cached cross plans via
/// the per-member schema-gen check under the execution pin.
#[test]
fn member_ddl_invalidates_cached_cross_plans() {
    let f = fix("gen", false);
    let h = f.prepare("SELECT count(*) FROM other.u").unwrap();
    f.execute(&h, &[]).unwrap();
    let side = Database::open_from_file(std::path::Path::new(&f.other_path)).unwrap();
    side.query("CREATE TABLE fresh (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    assert!(matches!(
        f.execute(&h, &[]),
        Err(mpedb::Error::PlanInvalidated)
    ));
    // A re-prepare against the new member schema works.
    let h2 = f.prepare("SELECT count(*) FROM other.u").unwrap();
    f.execute(&h2, &[]).unwrap();
}

// =========================================================================
// The v1 refusal set — refused BY NAME, never answered differently.
// =========================================================================

#[test]
fn writes_and_ddl_to_attached_refuse_by_name() {
    let f = fix("refuse", false);
    for (sql, needle) in [
        ("INSERT INTO other.u (x, y) VALUES (100, 1)", "cross-file writes"),
        ("UPDATE other.u SET y = 0 WHERE x = 1", "cross-file writes"),
        ("DELETE FROM other.u", "cross-file writes"),
        // bare name resolving to an attached table
        ("INSERT INTO u (x, y) VALUES (100, 1)", "cross-file writes"),
        // main write READING an attached table
        (
            "INSERT INTO s (id, val) SELECT x, y FROM other.u",
            "cross-file writes",
        ),
        ("CREATE TABLE other.w2 (q INT)", "DDL on an attached database"),
        ("DROP TABLE other.u", "DDL on an attached database"),
        ("CREATE INDEX other.i ON u (y)", "DDL on an attached database"),
    ] {
        let e = f.query(sql, &[]).unwrap_err();
        assert!(
            e.to_string().contains(needle),
            "`{sql}` should refuse with `{needle}`, got: {e}"
        );
    }
    // Nothing above wrote anything.
    assert_eq!(
        mpedb_rows(&f, "SELECT count(*) FROM other.u"),
        vec![vec![Value::Int(4)]]
    );
    // Writes to MAIN keep working with attachments present.
    f.query("INSERT INTO t (a, tag) VALUES (50, 'still-works')", &[])
        .unwrap();
    f.query("INSERT INTO main.t (a, tag) VALUES (51, 'qualified')", &[])
        .unwrap();
    assert_eq!(
        mpedb_rows(&f, "SELECT count(*) FROM main.t"),
        vec![vec![Value::Int(5)]]
    );
}

#[test]
fn attach_shapes_refused_by_name() {
    let f = fix("shapes", false);
    for (sql, needle) in [
        (
            "ATTACH '/nonexistent/nowhere.mpedb' AS ghost".to_string(),
            "does not exist",
        ),
        ("ATTACH ':memory:' AS mem".to_string(), ":memory:"),
        ("ATTACH ? AS pdb".to_string(), "bound parameter"),
    ] {
        let e = f.query(&sql, &[]).unwrap_err();
        assert!(
            e.to_string().contains(needle),
            "`{sql}` should refuse with `{needle}`, got: {e}"
        );
    }
}

#[test]
fn cross_file_inside_open_transaction_refuses_by_name() {
    let f = fix("txn", false);
    let mut s = f.begin().unwrap();
    let e = s.query("SELECT * FROM other.u", &[]).unwrap_err();
    assert!(
        e.to_string().contains("open write transaction"),
        "in-txn cross SELECT: {e}"
    );
    let e = s
        .query(&format!("ATTACH '{}' AS t3", f.third_path), &[])
        .unwrap_err();
    assert!(
        e.to_string().contains("open transaction"),
        "in-txn ATTACH: {e}"
    );
    // The txn itself is fine and main-only statements still run in it.
    s.query("INSERT INTO t (a, tag) VALUES (60, 'in-txn')", &[])
        .unwrap();
    s.commit().unwrap();
}

/// Same-named unaliased tables from two databases: refused with a nameable
/// rule (sqlite resolves per-reference; one row-name cannot mean two tables
/// in the rewrite, and silently picking one would be a wrong answer).
#[test]
fn same_name_two_dbs_without_aliases_refuses() {
    let f = fix("dupname", false);
    let e = f
        .query("SELECT * FROM main.t, other.t", &[])
        .unwrap_err();
    assert!(e.to_string().contains("add AS aliases"), "{e}");
}

/// RLS on any involved member refuses cross-file reads (policies are
/// per-file state the merged plan cannot validate).
#[test]
fn rls_policies_refuse_cross_file() {
    let f = fix("rls", false);
    // A policy on MAIN refuses main-side cross statements.
    f.query(
        "CREATE POLICY p ON t FOR SELECT USING (a > 0)",
        &[],
    )
    .unwrap();
    let e = f.query("SELECT * FROM other.u", &[]).unwrap_err();
    assert!(
        e.to_string().contains("RLS"),
        "policy-bearing main should refuse cross-file: {e}"
    );
    // Main-only statements are unaffected.
    f.query("SELECT * FROM t", &[]).unwrap();
}
