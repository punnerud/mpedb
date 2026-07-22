//! Partial indexes (P1 / design/DESIGN-WORKLOAD-INDEXES.md §5).
//!
//! `CREATE INDEX … WHERE <predicate>` is parsed, stored on `IndexDef`, and
//! survives the schema wire (canonical-bytes v10). §5.5 v1: the planner may
//! now PICK a partial for access, but only when the query predicate provably
//! ENTAILS the index predicate — exact atom match plus the `IS NOT NULL`
//! weakenings, and nothing else. Everything below is differentialled against
//! the BUNDLED sqlite (`tests/sqlite_oracle`, 3.45), which enforces real
//! partial membership: if the implication test ever over-claimed, mpedb would
//! return FEWER rows than sqlite and these tests would say so.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/mnt/ext4").is_dir() {
        PathBuf::from("/mnt/ext4")
    } else if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-partial-ix-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        path,
    )
}

#[test]
fn create_index_where_parses_and_builds() {
    let (db, path) = open();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[])
        .unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (1, 10, 'x')", &[])
        .unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (2, NULL, 'y')", &[])
        .unwrap();
    // Non-unique partial: create succeeds and stores the predicate.
    db.query("CREATE INDEX ix_a ON t (a) WHERE a IS NOT NULL", &[])
        .unwrap();
    // SELECT answers correctly. `a = 10` entails `a IS NOT NULL`, so this one
    // now rides the index (§5.5 row 2) — the answer is what is asserted here.
    let res = db.query("SELECT id FROM t WHERE a = 10", &[]).unwrap();
    match res {
        mpedb::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Predicate is in the live schema.
    let sch = db.schema();
    let t = sch.tables.iter().find(|t| t.name == "t").expect("table t");
    let ix = t
        .indexes
        .iter()
        .find(|ix| ix.predicate.is_some())
        .expect("partial index");
    assert!(
        ix.predicate
            .as_ref()
            .unwrap()
            .to_ascii_uppercase()
            .contains("IS NOT NULL"),
        "{:?}",
        ix.predicate
    );
    // UNIQUE partial is refused until membership evaluation ships.
    let err = db
        .query("CREATE UNIQUE INDEX ux ON t (b) WHERE b IS NOT NULL", &[])
        .unwrap_err();
    assert!(
        format!("{err}").contains("UNIQUE INDEX") || format!("{err}").contains("partial"),
        "{err}"
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn create_index_where_empty_table_like_cpython() {
    let (db, path) = open();
    db.query("CREATE TABLE test (t TEXT)", &[]).unwrap();
    // CPython shape: empty table + WHERE with a function call spelling.
    // (Host UDF not registered — predicate text is stored; build is empty.)
    db.query("CREATE INDEX t ON test(t) WHERE t IS NOT NULL", &[])
        .unwrap();
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------- §5.5 v1 --

/// The one table every implication case runs over, in BOTH engines.
/// `del` is the Django soft-delete shape (`WHERE deleted_at IS NULL`);
/// `st` is the literal-equality shape.
const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT, del INT, st TEXT)";
const ROWS: &[&str] = &[
    "INSERT INTO t (id, a, b, del, st) VALUES (1, 10, 'x', NULL, 'live')",
    "INSERT INTO t (id, a, b, del, st) VALUES (2, NULL, 'y', NULL, 'live')",
    "INSERT INTO t (id, a, b, del, st) VALUES (3, 10, 'z', 1, 'gone')",
    "INSERT INTO t (id, a, b, del, st) VALUES (4, 20, 'w', NULL, 'gone')",
    "INSERT INTO t (id, a, b, del, st) VALUES (5, 20, NULL, 7, 'live')",
    "INSERT INTO t (id, a, b, del, st) VALUES (6, 30, 'v', NULL, NULL)",
];

fn explain(db: &Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}"), &[]).unwrap() {
        ExecResult::Explain(text) => text,
        other => panic!("expected explain, got {other:?}"),
    }
}

/// mpedb's rows for `sql`, rendered the way the oracle renders sqlite's:
/// one line per row, columns joined by `|`, NULL as the empty string.
fn mp(db: &Database, sql: &str) -> Vec<String> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Int(i) => i.to_string(),
                        Value::Text(s) => s.clone(),
                        Value::Bool(b) => (*b as i64).to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("expected rows for `{sql}`, got {other:?}"),
    }
}

/// The same script against the bundled sqlite.
fn sq(index_ddl: &str, sql: &str) -> Vec<String> {
    let mut script = String::new();
    script.push_str(DDL);
    script.push(';');
    for r in ROWS {
        script.push_str(r);
        script.push(';');
    }
    script.push_str(index_ddl);
    script.push(';');
    script.push_str(sql);
    script.push(';');
    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Build the fixture in mpedb with `index_ddl`, then for each `(sql, uses_ix)`
/// assert (a) mpedb's answer equals the BUNDLED sqlite's, and (b) the access
/// path is/is not the partial index — so a silently-declined implication shows
/// up as a test failure rather than as invisible lost performance.
fn battery(tag: &str, index_ddl: &str, cases: &[(&str, bool)]) {
    let (db, path) = open();
    db.query(DDL, &[]).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    db.query(index_ddl, &[]).unwrap();
    for (sql, uses_ix) in cases {
        let want = sq(index_ddl, sql);
        let got = mp(&db, sql);
        assert_eq!(got, want, "[{tag}] answer differs from sqlite for `{sql}`");
        let plan = explain(&db, sql);
        let claims = plan.contains("via index");
        assert_eq!(
            claims, *uses_ix,
            "[{tag}] expected index-use={uses_ix} for `{sql}`, plan was:\n{plan}"
        );
    }
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// Row 2 of the lattice: the `IS NOT NULL` predicate and its weakenings.
/// Every non-`≠` comparison against a non-NULL literal entails it, because
/// such a comparison is 3-valued NULL — never TRUE — on a NULL column.
#[test]
fn is_not_null_predicate_and_its_weakenings() {
    battery(
        "isnotnull",
        "CREATE INDEX ix ON t (a) WHERE a IS NOT NULL",
        &[
            // The implication holds here, but `IS NOT NULL` pins no key part,
            // so there is no Point/Range to build — mpedb has no whole-index
            // scan access path. Implication is necessary, never sufficient.
            ("SELECT id FROM t WHERE a IS NOT NULL ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 ORDER BY id", true),
            ("SELECT id, typeof(a) FROM t WHERE a = 20 ORDER BY id", true),
            ("SELECT id FROM t WHERE a > 10 ORDER BY id", true),
            ("SELECT id FROM t WHERE a >= 20 AND b IS NOT NULL ORDER BY id", true),
            // `≠` is deliberately outside v1 even though it would be sound.
            ("SELECT id FROM t WHERE a <> 10 ORDER BY id", false),
            // Nothing about `a` at all: no evidence, so no index.
            ("SELECT id FROM t WHERE b = 'x' ORDER BY id", false),
            // The OPPOSITE claim must never reach the index.
            ("SELECT id FROM t WHERE a IS NULL ORDER BY id", false),
        ],
    );
}

/// The Django soft-delete shape: the predicate names a column the index does
/// NOT cover, so the entailing conjunct stays in the residual filter.
#[test]
fn is_null_predicate_on_an_uncovered_column() {
    battery(
        "softdelete",
        "CREATE INDEX ix ON t (a) WHERE del IS NULL",
        &[
            ("SELECT id FROM t WHERE a = 10 AND del IS NULL ORDER BY id", true),
            ("SELECT id FROM t WHERE a = 20 AND del IS NULL ORDER BY id", true),
            ("SELECT id FROM t WHERE a > 10 AND del IS NULL ORDER BY id", true),
            (
                "SELECT id, typeof(del) FROM t WHERE a = 10 AND del IS NULL AND b = 'x' ORDER BY id",
                true,
            ),
            // Same query WITHOUT the entailing conjunct: FullScan, and the row
            // set is strictly larger — which is exactly what the index would
            // have hidden had the implication test been sloppy.
            ("SELECT id FROM t WHERE a = 10 ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 AND del IS NOT NULL ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 AND del = 1 ORDER BY id", false),
        ],
    );
}

/// Row 3: exact atom match on an equality against a literal, in both operand
/// orders — and NO range subsumption (`a = 30` really does imply `a > 20`,
/// and v1 still declines: that row of the lattice is v2).
#[test]
fn literal_equality_predicate_matches_exactly_or_declines() {
    battery(
        "eqlit",
        "CREATE INDEX ix ON t (a) WHERE st = 'live'",
        &[
            ("SELECT id FROM t WHERE a = 10 AND st = 'live' ORDER BY id", true),
            ("SELECT id FROM t WHERE a = 20 AND 'live' = st ORDER BY id", true),
            ("SELECT id FROM t WHERE a = 10 AND st = 'gone' ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 AND st IS NOT NULL ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 ORDER BY id", false),
        ],
    );
}

/// A conjunction predicate needs EVERY atom entailed, and a predicate outside
/// the v1 vocabulary (here: a disjunction) is never usable at all — not even
/// by a query that repeats it verbatim.
#[test]
fn conjunctions_need_every_atom_and_junk_predicates_are_refused() {
    battery(
        "conj",
        "CREATE INDEX ix ON t (a) WHERE del IS NULL AND st = 'live'",
        &[
            (
                "SELECT id FROM t WHERE a = 10 AND del IS NULL AND st = 'live' ORDER BY id",
                true,
            ),
            ("SELECT id FROM t WHERE a = 10 AND del IS NULL ORDER BY id", false),
            ("SELECT id FROM t WHERE a = 10 AND st = 'live' ORDER BY id", false),
        ],
    );
    battery(
        "junk",
        "CREATE INDEX ix ON t (a) WHERE del IS NULL OR st = 'live'",
        &[
            (
                "SELECT id FROM t WHERE a = 10 AND (del IS NULL OR st = 'live') ORDER BY id",
                false,
            ),
            ("SELECT id FROM t WHERE a = 10 AND del IS NULL ORDER BY id", false),
        ],
    );
}

/// A bound PARAMETER is not evidence: the compiler does not know `$1`, so
/// `WHERE st = $1` cannot prove `WHERE st = 'live'` (§5.5, "the parameter
/// problem"). The answer must still be sqlite's.
#[test]
fn a_parameter_is_not_evidence() {
    let (db, path) = open();
    db.query(DDL, &[]).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    let ddl = "CREATE INDEX ix ON t (a) WHERE st = 'live'";
    db.query(ddl, &[]).unwrap();
    let sql = "SELECT id FROM t WHERE a = 10 AND st = $1 ORDER BY id";
    let plan = explain(&db, sql);
    assert!(
        !plan.contains("via index"),
        "a parameter must not prove a literal predicate:\n{plan}"
    );
    let got = match db
        .query(sql, &[Value::Text("live".into())])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(
        got,
        vec![vec![Value::Int(1)]],
        "same answer sqlite gives for st = 'live'"
    );
    assert_eq!(
        sq(ddl, "SELECT id FROM t WHERE a = 10 AND st = 'live' ORDER BY id"),
        vec!["1".to_string()]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A partial index is never the inner probe of a nested loop: the ON
/// equalities are all `extract_join_access` sees, and the WHERE that would
/// prove membership is not in scope there.
#[test]
fn a_partial_index_is_never_a_join_inner_probe() {
    let (db, path) = open();
    db.query(DDL, &[]).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    db.query("CREATE TABLE o (id INTEGER PRIMARY KEY, k INT)", &[])
        .unwrap();
    db.query("INSERT INTO o (id, k) VALUES (1, 10), (2, 20)", &[])
        .unwrap();
    db.query("CREATE INDEX ix ON t (a) WHERE del IS NULL", &[])
        .unwrap();
    let sql = "SELECT o.id, t.id FROM o JOIN t ON t.a = o.k WHERE t.del IS NULL ORDER BY o.id, t.id";
    let plan = explain(&db, sql);
    assert!(
        !plan.contains("via index"),
        "the partial must not be a join inner probe:\n{plan}"
    );
    assert_eq!(mp(&db, sql), vec!["1|1", "2|4"]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// UPDATE and DELETE take the same access path, so they take the same
/// implication test. The row COUNTS are what a wrong probe would corrupt.
#[test]
fn update_and_delete_ride_the_same_implication() {
    let (db, path) = open();
    db.query(DDL, &[]).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    db.query("CREATE INDEX ix ON t (a) WHERE del IS NULL", &[])
        .unwrap();
    let plan = explain(&db, "UPDATE t SET b = 'q' WHERE a = 20 AND del IS NULL");
    assert!(plan.contains("via index"), "{plan}");
    match db
        .query("UPDATE t SET b = 'q' WHERE a = 20 AND del IS NULL", &[])
        .unwrap()
    {
        ExecResult::Affected(n) => assert_eq!(n, 1, "only row 4 is live with a = 20"),
        other => panic!("expected affected, got {other:?}"),
    }
    match db
        .query("DELETE FROM t WHERE a = 10 AND del IS NULL", &[])
        .unwrap()
    {
        ExecResult::Affected(n) => assert_eq!(n, 1, "only row 1 is live with a = 10"),
        other => panic!("expected affected, got {other:?}"),
    }
    assert_eq!(mp(&db, "SELECT id FROM t ORDER BY id"), ["2", "3", "4", "5", "6"]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
