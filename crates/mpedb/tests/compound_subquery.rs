//! Compound (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) bodies in a LIFTED
//! subquery position (#56/format 31): `x IN (SELECT … UNION …)`, a scalar
//! `(SELECT … UNION … LIMIT 1)`, and `EXISTS (SELECT … UNION …)`. The subquery
//! lift now accepts a whole compound body (uncorrelated) and carries it in the
//! `SubPlan` as a `CompoundPlan`; the executor runs it exactly like a top-level
//! compound and reduces the rows to the consumer's value / list / existence.
//!
//! Every expected value is cross-checked against the `sqlite3` CLI (3.45) at test
//! time, so correct rows + ordering + dedup are pinned to the reference engine.
//! (A derived-table compound `FROM (SELECT … UNION …)` is a separate, deferred
//! feature — derived tables are flattened onto their base, which a compound
//! cannot be — and is NOT exercised here.)

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database so a panicking test does not leak a `/dev/shm` file.
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

/// Two tables of plain integers so mpedb's rigid typing and sqlite's loose
/// typing agree cell-for-cell. `t.a`, `u.b` are nullable to exercise the IN/3VL
/// path (a NULL probe key, a NULL in the membership list).
const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "a"
  type = "int64"
  nullable = true

[[table]]
name = "u"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "b"
  type = "int64"
  nullable = true
"#;

fn insert_statements() -> Vec<&'static str> {
    vec![
        "INSERT INTO t (id, a) VALUES (1,10),(2,20),(3,30),(4,NULL),(5,40)",
        "INSERT INTO u (id, b) VALUES (1,20),(2,30),(3,40),(4,NULL)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-compound-sub-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering, matching the `sqlite3` CLI default "list" mode:
/// NULL as empty, integers verbatim. (Every query below outputs only integers.)
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in compound-subquery test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run schema + data + one query through the `sqlite3` CLI and parse its default
/// list-mode output into rows.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER);\n\
         CREATE TABLE u (id INTEGER PRIMARY KEY, b INTEGER);\n",
    );
    for stmt in insert_statements() {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// A battery of compound-bodied subqueries in every lift position, over each set
/// operator — each must match sqlite 3.45 cell-for-cell.
#[test]
fn compound_subquery_bodies_match_sqlite_3_45() {
    let d = db();
    let queries = [
        // ---- IN (SELECT … UNION/… …) ---------------------------------------
        // UNION and UNION ALL give the SAME membership set (dedup is irrelevant
        // to `IN`); both must agree with sqlite.
        "SELECT id FROM t WHERE a IN (SELECT a FROM t UNION SELECT b FROM u) ORDER BY id",
        "SELECT id FROM t WHERE a IN (SELECT a FROM t UNION ALL SELECT b FROM u) ORDER BY id",
        // INTERSECT: only values present in BOTH arms — {20,30,40}.
        "SELECT id FROM t WHERE a IN (SELECT a FROM t INTERSECT SELECT b FROM u) ORDER BY id",
        // EXCEPT: values in the left arm but not the right — {10}.
        "SELECT id FROM t WHERE a IN (SELECT a FROM t EXCEPT SELECT b FROM u) ORDER BY id",
        // NOT IN over a compound whose result includes a NULL (u.b has one) — the
        // 3VL empty-answer rule: NOT IN a list with NULL is never TRUE.
        "SELECT id FROM t WHERE a NOT IN (SELECT b FROM u UNION SELECT a FROM t) ORDER BY id",
        // NOT IN over a NULL-free compound — a genuine complement.
        "SELECT id FROM t \
         WHERE a NOT IN (SELECT b FROM u WHERE b IS NOT NULL EXCEPT SELECT a FROM t WHERE a > 25) \
         ORDER BY id",
        // A compound body with its OWN ORDER BY / LIMIT (bound to the compound).
        "SELECT id FROM t WHERE a IN (SELECT a FROM t UNION SELECT b FROM u ORDER BY 1 LIMIT 2) ORDER BY id",
        // The lifted LHS is itself an expression, and the body reads a param-free
        // filtered union.
        "SELECT id FROM t \
         WHERE a + 0 IN (SELECT a FROM t WHERE a < 25 UNION SELECT b FROM u WHERE b > 25) ORDER BY id",

        // ---- scalar (SELECT … UNION/… … LIMIT 1) ---------------------------
        // A FROM-less projection scalar: min of the NULL-free union = 10.
        "SELECT (SELECT a FROM t WHERE a IS NOT NULL UNION SELECT b FROM u WHERE b IS NOT NULL \
         ORDER BY 1 LIMIT 1)",
        // A scalar in WHERE feeding a PK-shaped comparison.
        "SELECT id FROM t \
         WHERE a = (SELECT b FROM u UNION SELECT 999 ORDER BY 1 DESC LIMIT 1) ORDER BY id",
        // INTERSECT scalar: the single shared-and-largest value.
        "SELECT (SELECT a FROM t INTERSECT SELECT b FROM u ORDER BY 1 DESC LIMIT 1)",

        // ---- EXISTS (SELECT … UNION/… …) -----------------------------------
        // The right arm matches ⇒ every outer row.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT 1 FROM u WHERE b > 100 UNION SELECT 1 FROM t WHERE a = 20) ORDER BY id",
        // Both arms empty ⇒ no outer rows.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT b FROM u WHERE b > 1000 UNION SELECT a FROM t WHERE a > 1000) ORDER BY id",
        // NOT EXISTS over an empty compound ⇒ every outer row.
        "SELECT id FROM t \
         WHERE NOT EXISTS (SELECT b FROM u WHERE b > 1000 INTERSECT SELECT a FROM t) ORDER BY id",
        // EXCEPT inside EXISTS: non-empty ⇒ every outer row.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT a FROM t EXCEPT SELECT b FROM u) ORDER BY id",

        // ---- a compound body NESTED inside a plain-select subquery ----------
        "SELECT id FROM t WHERE a IN \
         (SELECT b FROM u WHERE b IN (SELECT a FROM t UNION SELECT b FROM u)) ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// Direct `Value` assertions on the canonical shapes, so the behavior is pinned
/// independently of the string cross-check.
#[test]
fn compound_subquery_direct_values() {
    let d = db();
    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };
    // min over the NULL-free union {10,20,30,40} = 10.
    assert_eq!(
        one("SELECT (SELECT a FROM t WHERE a IS NOT NULL UNION SELECT b FROM u WHERE b IS NOT NULL \
             ORDER BY 1 LIMIT 1)"),
        Value::Int(10)
    );
    // EXISTS over a non-empty INTERSECT ({20,30,40}) is TRUE.
    assert_eq!(
        one("SELECT EXISTS (SELECT a FROM t INTERSECT SELECT b FROM u)"),
        Value::Bool(true)
    );
    // EXISTS over an empty EXCEPT is FALSE: an empty left arm minus anything is
    // still empty.
    assert_eq!(
        one("SELECT EXISTS (SELECT b FROM u WHERE b > 1000 EXCEPT SELECT a FROM t)"),
        Value::Bool(false)
    );
}

/// UNCORRELATED subqueries inside compound ARMS (the 520-record corpus gap):
/// each arm's lifted subplans take the reserved slots after the preceding
/// arms', numbered against the final statement layout at plan time and filled
/// once before dispatch — exactly like a single SELECT's. Cross-checked
/// against the bundled sqlite; the corpus shape (two views with IN-subquery
/// bodies under UNION / UNION ALL) is the last case.
#[test]
fn subquery_in_compound_arms_matches_sqlite() {
    let d = db();
    let queries = [
        // One arm with a subquery.
        "SELECT id FROM t WHERE a IN (SELECT b FROM u) UNION ALL SELECT id FROM u ORDER BY 1",
        // Both arms with subqueries (slot offsets in play).
        "SELECT id FROM t WHERE a IN (SELECT b FROM u) \
         UNION SELECT id FROM u WHERE b IN (SELECT a FROM t) ORDER BY 1",
        // Scalar + EXISTS subqueries across three arms and mixed set ops.
        "SELECT id FROM t WHERE a = (SELECT max(b) FROM u) \
         UNION ALL SELECT id FROM u WHERE EXISTS (SELECT 1 FROM t WHERE a = 10) \
         EXCEPT SELECT id FROM t WHERE a IN (SELECT b FROM u WHERE b IS NULL) ORDER BY 1",
        // NOT IN with a NULL-bearing membership list in an arm (3VL).
        "SELECT id FROM t WHERE a NOT IN (SELECT b FROM u) UNION SELECT 77 ORDER BY 1",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    // The corpus shape: views whose bodies carry IN-subqueries, referenced in
    // UNION / UNION ALL arms (the flatten splices the subquery into the arm).
    d.query("CREATE VIEW v1 AS SELECT id, a FROM t WHERE a IN (SELECT b FROM u)", &[])
        .unwrap();
    d.query("CREATE VIEW v2 AS SELECT id, a FROM t WHERE a NOT IN (SELECT b FROM u)", &[])
        .unwrap();
    let with_views = |q: &str| {
        format!(
            "CREATE VIEW v1 AS SELECT id, a FROM t WHERE a IN (SELECT b FROM u);\n\
             CREATE VIEW v2 AS SELECT id, a FROM t WHERE a NOT IN (SELECT b FROM u);\n{q}"
        )
    };
    for q in [
        "SELECT id, a FROM v1 UNION ALL SELECT id, a FROM v2 ORDER BY 1, 2",
        "SELECT id, a FROM v1 UNION SELECT id, a FROM v2 ORDER BY 1, 2",
    ] {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(&with_views(q)), "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// CORRELATED subqueries inside compound ARMS (format 56). The arm OWNS the
/// lift, so it is filled per ARM row by the same `exec_select_leveled`
/// discipline the top level and a derived body use — the arm's row is the only
/// row such a correlation can name, which is why hoisting it onto the statement
/// could never work. Every shape is matched against the bundled sqlite.
#[test]
fn correlated_subquery_in_compound_arms_matches_sqlite() {
    let d = db();
    let queries = [
        // The canonical shape: a correlated EXISTS in one arm.
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         UNION SELECT id FROM u ORDER BY 1",
        // NOT EXISTS — the negated 3VL side.
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         UNION ALL SELECT id FROM u WHERE b IS NULL ORDER BY 1",
        // BOTH arms correlated: each numbers its own reserved slots after the
        // preceding arm's, and each fills only its own.
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         UNION SELECT id FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.a = u.b) ORDER BY 1",
        // A correlated SCALAR subquery in the projection of an arm. Two output
        // columns so a NULL row still renders as a non-empty line in both arms.
        "SELECT id, (SELECT max(b) FROM u WHERE u.b = t.a) FROM t \
         UNION SELECT id, b FROM u ORDER BY 1, 2",
        // A correlated IN, and an uncorrelated one in the other arm (mixed
        // fill phases in one statement).
        "SELECT id FROM t WHERE a IN (SELECT b FROM u WHERE u.id = t.id) \
         UNION ALL SELECT id FROM u WHERE b IN (SELECT a FROM t) ORDER BY 1",
        // EXCEPT / INTERSECT over a correlated arm.
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         EXCEPT SELECT id FROM u WHERE b = 20 ORDER BY 1",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         INTERSECT SELECT id FROM u ORDER BY 1",
        // An AGGREGATE arm over a correlated filter (the per-row fill runs
        // between the gather and the grouping).
        "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         UNION ALL SELECT count(*) FROM u ORDER BY 1",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// A compound SUBQUERY BODY whose arms reference the ENCLOSING row (format 56).
/// The correlation region belongs to the SUBPLAN — filled once per outer row
/// before the compound runs — so every arm reads it as an ordinary parameter.
/// This is the `no table named V0 in this statement` refusal, closed.
#[test]
fn correlated_compound_subquery_body_matches_sqlite() {
    let d = db();
    let queries = [
        // EXISTS over a correlated compound: both arms name the outer row.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a UNION SELECT 1 FROM u WHERE u.id = t.id) \
         ORDER BY id",
        // Only ONE arm correlates; the other is a constant.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT b FROM u WHERE u.b = t.a UNION SELECT 999) ORDER BY id",
        // INTERSECT: the outer row must be in both arms.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT u.b FROM u WHERE u.b = t.a INTERSECT SELECT a FROM t) ORDER BY id",
        // EXCEPT under NOT EXISTS.
        "SELECT id FROM t \
         WHERE NOT EXISTS (SELECT u.b FROM u WHERE u.b = t.a EXCEPT SELECT 20) ORDER BY id",
        // A correlated IN whose membership list is a compound.
        "SELECT id FROM t \
         WHERE a IN (SELECT b FROM u WHERE u.id = t.id UNION SELECT a FROM t WHERE a = 40) \
         ORDER BY id",
        // A correlated SCALAR compound (LIMIT 1 makes it single-valued).
        "SELECT id, (SELECT b FROM u WHERE u.b = t.a UNION SELECT 0 ORDER BY 1 DESC LIMIT 1) \
         FROM t ORDER BY id",
        // The SAME outer column named by both arms — one shared correlation
        // slot, by the arg dedup.
        "SELECT id FROM t \
         WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a UNION ALL SELECT 1 FROM u WHERE u.b = t.a + 10) \
         ORDER BY id",
        // A compound subquery body NESTED inside a plain-select subquery, whose
        // arms reach OUT to the outermost row (a transit correlation).
        "SELECT id FROM t WHERE EXISTS (\
           SELECT 1 FROM u WHERE EXISTS (SELECT 1 FROM u u2 WHERE u2.b = t.a UNION SELECT 1 FROM t WHERE t.id = u.id)\
         ) ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// The refusals that must stay refusals (never a WRONG answer): a compound
/// subquery BODY cannot itself contain a subquery (its arms' slots would
/// collide with the outer lift's), and a scalar/IN compound must project
/// exactly one column. These are clean errors, not silent misreads.
#[test]
fn compound_subquery_refusals() {
    let d = db();
    // A subquery inside a compound SUBQUERY BODY used to be refused (the arms'
    // slots would have collided with the outer lift's). The arms now OWN their
    // lifts and number them after this subplan's correlation region, so it
    // answers — matched against sqlite.
    for q in [
        "SELECT id FROM t WHERE a IN \
         (SELECT a FROM t WHERE a IN (SELECT b FROM u) UNION SELECT b FROM u) ORDER BY id",
        "SELECT id FROM t WHERE EXISTS \
         (SELECT 1 FROM u WHERE u.b = t.a AND u.b IN (SELECT a FROM t) \
          UNION SELECT 1 FROM u WHERE u.id = (SELECT max(id) FROM t)) ORDER BY id",
    ] {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    // A scalar compound projecting two columns is refused.
    assert!(d
        .query("SELECT (SELECT id, a FROM t UNION SELECT id, b FROM u LIMIT 1)", &[])
        .is_err());
    // An IN compound projecting two columns is refused.
    assert!(d
        .query(
            "SELECT id FROM t WHERE a IN (SELECT id, a FROM t UNION SELECT id, b FROM u)",
            &[]
        )
        .is_err());
    // A COMPOUND body in a DERIVED-TABLE FROM source is now MATERIALIZED
    // (design/DESIGN-DERIVED-TABLES.md §5, format 49): the body runs once and
    // the outer scans the row set — checked against sqlite in
    // derived_materialize.rs. Here: it answers, with the UNION-deduped rows.
    {
        let got = mpedb_rows(
            &d,
            "SELECT x.a FROM (SELECT a FROM t UNION SELECT b FROM u) x \
             WHERE x.a IS NOT NULL ORDER BY x.a",
        );
        assert_eq!(
            got,
            sqlite_rows(
                "SELECT x.a FROM (SELECT a FROM t UNION SELECT b FROM u) x \
                 WHERE x.a IS NOT NULL ORDER BY x.a"
            )
        );
    }
    assert!(d
        .query("SELECT * FROM (SELECT a FROM t WHERE a > 15) x ORDER BY a", &[])
        .is_ok());
    // A CORRELATED compound subquery (an arm references the outer row) is
    // ANSWERED as of format 56 — see `correlated_compound_subquery_body_*`.
    // What stays refused here is `current_setting()` inside one: its reserved
    // slot would have to be reconciled across the arms AND the correlation
    // region, which this stage does not do.
    {
        let q = "SELECT id FROM t WHERE EXISTS \
                 (SELECT 1 FROM u WHERE u.b = t.a UNION SELECT 1 FROM u WHERE u.b > 100) \
                 ORDER BY id";
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q));
    }
}
