//! A derived table in a NESTED position, reached through the PASS-THROUGH
//! wrapper (#112, design/DESIGN-DERIVED-TABLES.md §5.7).
//!
//! sqlite has no parenthesized compound operand, so every generator that needs
//! one writes `SELECT * FROM ( <body> )` instead — Django's `SQLCompiler` does
//! it for a nested combinator, and its `subquery`-wrapping path writes the
//! projection-restricting cousin `SELECT sq.a, sq.b FROM ( <body> ) sq`. Both
//! wrappers are IDENTITIES over the body's rows, so `crate::view` removes them
//! before planning and the body lands in a position mpedb already represents.
//!
//! The hazard this file exists to police is that widening a refusal can create
//! a WRONG ANSWER. Every shape newly accepted here is checked cell-for-cell —
//! value AND `typeof()` — against the BUNDLED sqlite oracle (3.45.0), and every
//! shape whose removal would NOT be an identity (a body with its own
//! ORDER BY/LIMIT spliced into a compound arm, a non-associative operator
//! nesting, a hidden column the outer names) is asserted to still REFUSE.
//! Narrower than sqlite is fine; different is never.

use mpedb::{Config, Database, Error, ExecResult, Value};
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
  [[table.column]]
  name = "s"
  type = "text"
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
        "INSERT INTO t (id, a, s) VALUES (1,10,'x'),(2,20,'y'),(3,20,'x'),(4,NULL,'z'),(5,30,NULL),(6,10,'y')",
        "INSERT INTO u (id, b) VALUES (1,10),(2,20),(3,NULL),(4,99)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-derived-nested-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering matching the sqlite CLI list mode.
const NULLV: &str = "<NULL>";

fn render(v: Value) -> String {
    match v {
        Value::Null => NULLV.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        other => panic!("unexpected value in derived-nested test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Result<Vec<Vec<String>>, Error> {
    match db.query(sql, &[])? {
        ExecResult::Rows { rows, .. } => Ok(rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect()),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

fn oracle_script(query: &str) -> String {
    let mut script = String::from(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, s TEXT);\n\
         CREATE TABLE u (id INTEGER PRIMARY KEY, b INTEGER);\n",
    );
    for stmt in insert_statements() {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    script
}

fn sqlite_rows(query: &str) -> Result<Vec<Vec<String>>, String> {
    Ok(sqlite_oracle::try_script_stdout(&oracle_script(query), NULLV)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect())
}

/// Both engines answer identically. Used for every shape this change newly
/// accepts — the answer must BE sqlite's, not merely exist.
fn same(db: &Database, sql: &str) {
    let ours = mpedb_rows(db, sql).unwrap_or_else(|e| panic!("mpedb refused `{sql}`: {e}"));
    let theirs = sqlite_rows(sql).unwrap_or_else(|e| panic!("sqlite refused `{sql}`: {e}"));
    assert_eq!(ours, theirs, "row mismatch on `{sql}`");
}

/// Both engines refuse. Used where the wrapper's removal would NOT be an
/// identity and where sqlite's own answer is an error too.
fn both_refuse(db: &Database, sql: &str) {
    let ours = mpedb_rows(db, sql);
    let theirs = sqlite_rows(sql);
    match (ours, theirs) {
        (Err(_), Err(_)) => {}
        (Ok(a), Err(e)) => panic!("sqlite errors ({e}) but mpedb answers {a:?} on `{sql}`"),
        (Err(e), Ok(b)) => panic!("mpedb errors ({e}) but sqlite answers {b:?} on `{sql}`"),
        (Ok(a), Ok(b)) => panic!("both answered ({a:?} / {b:?}) on `{sql}`"),
    }
}

/// mpedb refuses BY NAME where sqlite answers: a narrower engine, never a
/// different one. Also asserts sqlite really does answer, so the case does not
/// silently stop being a gap.
fn refused_narrower(db: &Database, sql: &str, needle: &str) {
    let e = match mpedb_rows(db, sql) {
        Err(e) => e,
        Ok(rows) => panic!("expected a refusal on `{sql}`, got {rows:?}"),
    };
    assert!(
        e.to_string().contains(needle),
        "expected a refusal mentioning `{needle}` on `{sql}`, got: {e}"
    );
    sqlite_rows(sql).unwrap_or_else(|e| panic!("sqlite no longer answers `{sql}`: {e}"));
}

// --------------------------------------------------- a pass-through arm -----

/// `SELECT * FROM (<body>)` as a compound arm — Django's `test_union_nested`
/// shape and its `UNION ALL`/`INTERSECT` siblings. The wrapper is dropped and
/// the inner chain splices into the enclosing one.
#[test]
fn passthrough_compound_arm_matches_sqlite() {
    let d = db();
    for q in [
        // The Django shape verbatim: a compound arm holding a compound.
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM t UNION SELECT id FROM u) ORDER BY 1",
        "SELECT id FROM t UNION ALL SELECT * FROM (SELECT id FROM t UNION ALL SELECT id FROM u) ORDER BY 1",
        "SELECT id FROM t INTERSECT SELECT * FROM (SELECT id FROM u INTERSECT SELECT id FROM t) ORDER BY 1",
        // A three-deep nest, and one with more arms around it.
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM u UNION SELECT * FROM (SELECT id FROM t UNION SELECT 99)) ORDER BY 1",
        "SELECT 1 UNION SELECT * FROM (SELECT id FROM t UNION SELECT id FROM u) UNION SELECT 100 ORDER BY 1",
        // A PLAIN (non-compound) non-flattenable body in an arm: aggregate,
        // GROUP BY, DISTINCT, join — each previously refused there.
        "SELECT count(*) FROM u UNION SELECT * FROM (SELECT count(*) FROM t) ORDER BY 1",
        "SELECT id FROM u UNION SELECT * FROM (SELECT a FROM t GROUP BY a) ORDER BY 1",
        "SELECT id FROM u UNION ALL SELECT * FROM (SELECT DISTINCT a FROM t) ORDER BY 1",
        "SELECT id FROM u EXCEPT SELECT * FROM (SELECT t.id FROM t JOIN u ON u.id = t.id WHERE u.b > 10) ORDER BY 1",
        // ARM 0 is exact for ANY operator pair — a compound chain is already
        // evaluated left-associatively, so the flat chain brackets identically.
        "SELECT * FROM (SELECT id FROM t UNION ALL SELECT id FROM u) EXCEPT SELECT id FROM u ORDER BY 1",
        "SELECT * FROM (SELECT id FROM t EXCEPT SELECT id FROM u) UNION SELECT 42 ORDER BY 1",
        "SELECT * FROM (SELECT id FROM t INTERSECT SELECT id FROM u) UNION ALL SELECT 42 ORDER BY 1",
        "SELECT * FROM (SELECT a FROM t GROUP BY a) UNION SELECT id FROM u ORDER BY 1",
        // The compound's ORDER BY resolves against ARM 0's output names, which
        // the splice must leave alone.
        "SELECT * FROM (SELECT a AS x FROM t GROUP BY a) UNION SELECT id FROM u ORDER BY x",
        "SELECT * FROM (SELECT a AS x FROM t UNION SELECT b FROM u) UNION SELECT 42 ORDER BY x DESC",
        // …and the compound's own ORDER BY / LIMIT still binds to the WHOLE
        // chain, not to a spliced-in arm.
        "SELECT * FROM (SELECT id FROM t UNION SELECT id FROM u) UNION SELECT 0 ORDER BY 1 DESC LIMIT 3",
        // Text and NULL columns, so the set operators' NULL-equality and the
        // storage classes ride through the splice too.
        "SELECT s FROM t UNION SELECT * FROM (SELECT s FROM t UNION SELECT NULL) ORDER BY 1",
        "SELECT typeof(a) FROM t UNION SELECT * FROM (SELECT typeof(b) FROM u) ORDER BY 1",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// Non-associative / non-spliceable nests that used to refuse are now answered
/// by materialising the nested derived arm (PLAN_FORMAT 58). Differential vs
/// sqlite — never a wrong answer, never a silent identity rewrite.
#[test]
fn non_associative_arm_nesting_matches_sqlite() {
    let d = db();
    for q in [
        // EXCEPT is not associative: `A \ (B \ C)` ≠ `(A \ B) \ C`.
        "SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM t EXCEPT SELECT id FROM u) ORDER BY 1",
        // Django's `test_qs_with_subcompound_qs`: `A EXCEPT (B INTERSECT C)`.
        "SELECT count(*) FROM (SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM t INTERSECT SELECT id FROM u WHERE b > 10)) sub",
        // A MIXED chain: `A ∪ (B ⊎ C)` vs `(A ∪ B) ⊎ C`.
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM u UNION ALL SELECT id FROM u) ORDER BY 1",
        "SELECT id FROM t UNION ALL SELECT * FROM (SELECT id FROM u UNION SELECT id FROM u) ORDER BY 1",
        "SELECT id FROM t INTERSECT SELECT * FROM (SELECT id FROM u UNION SELECT id FROM t) ORDER BY 1",
        // Nested derived with its own ORDER BY / LIMIT on the body (not the arm wrapper).
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM u ORDER BY id LIMIT 1) ORDER BY 1",
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM u LIMIT 1 OFFSET 1) ORDER BY 1",
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM u UNION SELECT id FROM t LIMIT 2) ORDER BY 1",
        // Non-passthrough wrappers: real nested derived (project / filter / DISTINCT).
        "SELECT id FROM t UNION SELECT x FROM (SELECT a AS x FROM t GROUP BY a) w ORDER BY 1",
        "SELECT id FROM t UNION SELECT * FROM (SELECT a FROM t GROUP BY a) w WHERE a > 10 ORDER BY 1",
        "SELECT id FROM t UNION SELECT DISTINCT * FROM (SELECT a FROM t GROUP BY a) w ORDER BY 1",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

// ------------------------------------ a projection-restricting wrapper ------

/// Django's `subquery` wrapper: `SELECT sq.a, sq.b FROM (<body>) sq` as a
/// derived table's BODY — `test_distinct_ordered_sliced_subquery_aggregation`.
/// The middle SELECT only drops columns, so the outer reads the inner body
/// directly.
#[test]
fn projection_passthrough_body_matches_sqlite() {
    let d = db();
    for q in [
        // The Django shape verbatim: DISTINCT + join + ORDER BY + LIMIT inside,
        // a column-dropping wrapper, `count(*)` outside.
        "SELECT count(*) FROM (SELECT sq.c1, sq.c2 FROM (SELECT DISTINCT t.id AS c1, t.a AS c2, u.b FROM t LEFT JOIN u ON u.id = t.id ORDER BY u.b LIMIT 3) sq) sq2",
        // The same, reading the surviving columns rather than counting them.
        "SELECT c1, c2 FROM (SELECT sq.c1, sq.c2 FROM (SELECT DISTINCT t.id AS c1, t.a AS c2, u.b FROM t LEFT JOIN u ON u.id = t.id ORDER BY u.b LIMIT 3) sq) sq2 ORDER BY 1",
        // The VALUE and the TYPE, because a wrapper removal that changed the
        // storage class would still agree on the value.
        "SELECT typeof(c1), typeof(c2) FROM (SELECT sq.c1, sq.c2 FROM (SELECT DISTINCT id AS c1, s AS c2 FROM t ORDER BY id LIMIT 4) sq) o ORDER BY 1, 2",
        "SELECT c2, typeof(c2) FROM (SELECT sq.c2 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o ORDER BY 1",
        // A pure `SELECT *` wrapper (no column dropped) around every
        // non-flattenable body kind.
        "SELECT count(*) FROM (SELECT * FROM (SELECT a, count(*) AS n FROM t GROUP BY a) i) o",
        "SELECT * FROM (SELECT * FROM (SELECT DISTINCT a FROM t) i) o ORDER BY 1",
        "SELECT * FROM (SELECT * FROM (SELECT a FROM t ORDER BY a DESC LIMIT 2) i) o ORDER BY 1",
        "SELECT * FROM (SELECT * FROM (SELECT id FROM t UNION SELECT id FROM u) i) o ORDER BY 1",
        // A REORDERING wrapper: the output tuple must keep the WRAPPER's order.
        "SELECT * FROM (SELECT sq.c2, sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o ORDER BY 1, 2",
        "SELECT * FROM (SELECT i.n, i.a FROM (SELECT a, count(*) AS n FROM t GROUP BY a) i) o ORDER BY 1, 2",
        // Three levels of wrapper.
        "SELECT count(*) FROM (SELECT o.c1 FROM (SELECT i.c1, i.c2 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) i) o) x",
        // A compound body under the wrapper.
        "SELECT count(*) FROM (SELECT i.id FROM (SELECT id, a FROM t UNION SELECT id, b FROM u) i) o",
        // The wrapper's columns are what the outer WHERE / GROUP BY / ORDER BY
        // see, unqualified and qualified alike.
        "SELECT c2, count(*) FROM (SELECT sq.c1, sq.c2 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o WHERE o.c1 > 2 GROUP BY c2 ORDER BY 1",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// A column the wrapper HID must stay hidden. Collapsing the wrapper makes the
/// inner body's whole projection reachable, so naming a dropped column would
/// turn sqlite's "no such column" into an answer — the exact widening-into-a-
/// wrong-answer this pass must not do. The rewrite is declined instead, and
/// both engines error.
#[test]
fn hidden_columns_stay_hidden() {
    let d = db();
    for q in [
        "SELECT o.c2 FROM (SELECT sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o",
        "SELECT c2 FROM (SELECT sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o",
        "SELECT c1 FROM (SELECT sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o WHERE c2 > 1",
        "SELECT c1 FROM (SELECT sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o ORDER BY c2",
        "SELECT count(*) FROM (SELECT sq.c1 FROM (SELECT DISTINCT id AS c1, a AS c2 FROM t) sq) o GROUP BY c2",
    ] {
        both_refuse(&d, q);
    }
    d.verify().unwrap();
}

// ----------------------------------- a pass-through in a subquery body ------

/// `IN (SELECT * FROM (<body>))` / `EXISTS (SELECT * FROM (<body>))`: the
/// wrapper is dropped and the body becomes the subquery itself, which mpedb
/// already represents (a plain SELECT or a whole compound).
#[test]
fn passthrough_subquery_body_matches_sqlite() {
    let d = db();
    for q in [
        "SELECT id FROM t WHERE a IN (SELECT * FROM (SELECT DISTINCT b FROM u WHERE b IS NOT NULL)) ORDER BY id",
        "SELECT id FROM t WHERE a IN (SELECT * FROM (SELECT b FROM u UNION SELECT a FROM t WHERE a > 25)) ORDER BY id",
        "SELECT id FROM t WHERE EXISTS (SELECT * FROM (SELECT count(*) FROM u WHERE b > 50)) ORDER BY id",
        "SELECT (SELECT * FROM (SELECT max(b) FROM u)) AS m, typeof((SELECT * FROM (SELECT max(b) FROM u)))",
        // The body's own ORDER BY + LIMIT belongs to the subquery, so it is
        // kept — unlike in a compound arm, where it would rebind.
        "SELECT id FROM t WHERE a IN (SELECT * FROM (SELECT b FROM u ORDER BY b DESC LIMIT 2)) ORDER BY id",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

// --------------------------------------------- the boundary that stays ------

/// The nested positions the wrapper rewrite does NOT reach keep their refusal
/// by name (design/DESIGN-DERIVED-TABLES.md §5.7): a derived table whose
/// consumer is not a pass-through still needs a real `SelectPlan`-owned derived
/// source, which is a plan-format and executor change.
#[test]
fn genuinely_nested_derived_still_refuses() {
    let d = db();
    let nested = "only supported in a statement's outermost FROM";
    for q in [
        // A filtering (not pass-through) consumer inside a subquery body.
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM (SELECT b FROM u GROUP BY b) x WHERE x.b > 10)",
        // A filtering consumer inside a derived body.
        "SELECT count(*) FROM (SELECT x FROM (SELECT a AS x FROM t) i GROUP BY x) o",
    ] {
        refused_narrower(&d, q, nested);
    }
    d.verify().unwrap();
}

/// Django `test_distinct_ordered_sliced_subquery`: a projection-restricting
/// wrapper as the whole IN-subquery body collapses onto the inner DISTINCT /
/// ORDER BY / LIMIT select (selected columns first; DISTINCT still sees the
/// full inner projection via trailing junk).
#[test]
fn projection_passthrough_subquery_body_matches_sqlite() {
    let d = db();
    for q in [
        "SELECT s FROM t WHERE id IN (SELECT sq.id FROM (SELECT DISTINCT id, a FROM t ORDER BY a LIMIT 2) sq) ORDER BY 1",
        "SELECT s FROM t WHERE id IN (SELECT sq.id FROM (SELECT DISTINCT id, a FROM t ORDER BY a DESC LIMIT 3) sq) ORDER BY 1",
        "SELECT id FROM t WHERE a IN (SELECT x.b FROM (SELECT DISTINCT b, id FROM u ORDER BY id LIMIT 2) x) ORDER BY 1",
        // Reorder + drop.
        "SELECT s FROM t WHERE id IN (SELECT sq.a FROM (SELECT DISTINCT id AS a, s AS b FROM t ORDER BY b LIMIT 2) sq) ORDER BY 1",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// Django `test_qs_with_subcompound_qs`: nested set-op derived arm materialises
/// (format 58). Answers match sqlite, including the parentheses that
/// left-associative splice would get wrong.
#[test]
fn except_intersect_nest_matches_sqlite() {
    let d = db();
    for q in [
        "SELECT count(*) FROM (SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM t INTERSECT SELECT id FROM u WHERE b > 10)) sub",
        "SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM t INTERSECT SELECT id FROM u) ORDER BY 1",
        "SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM u INTERSECT SELECT id FROM t WHERE a IS NOT NULL) ORDER BY 1",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}
