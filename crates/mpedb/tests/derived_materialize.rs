//! MATERIALIZED derived tables (#74 Stage A, design/DESIGN-DERIVED-TABLES.md
//! §5): every body kind the Stage-B flattener refuses — aggregate, GROUP
//! BY/HAVING, DISTINCT, join, renamed/qualified projections, ORDER BY+LIMIT,
//! window, and compound (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) — now runs
//! by executing the body EXACTLY ONCE into an in-memory row set the outer
//! query scans (`PlanStmt::Derived`, the recursive-CTE working-table
//! primitive, PLAN_FORMAT 49).
//!
//! Every query is checked cell-for-cell against the BUNDLED sqlite oracle
//! (3.45.0), including the error cases (both engines must refuse). The #74
//! budget test lives here too: a huge body under a tiny `max_work_rows` must
//! trip `Error::RuntimeBudget` with the `derived table "<alias>"` attribution.

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

/// `t` and `u`: integers with NULLs (join-column NULL semantics), plus a text
/// column so renamed/text projections are exercised.
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

fn open(extra_toml: &str) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-derived-mat-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n{extra_toml}{SCHEMA}"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

fn db() -> Tmp {
    open("")
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
            // Only used for avg() outputs below; sqlite prints 15.0 as "15.0".
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        other => panic!("unexpected value in derived-materialize test: {other:?}"),
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

/// Differential check: both engines answer identically, or BOTH refuse.
/// A shape only one engine answers is exactly the divergence this feature is
/// forbidden to introduce.
fn check(db: &Database, sql: &str) {
    let ours = mpedb_rows(db, sql);
    let theirs = sqlite_rows(sql);
    match (ours, theirs) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "row mismatch on `{sql}`"),
        (Err(_), Err(_)) => {}
        (Ok(a), Err(e)) => panic!("sqlite errors ({e}) but mpedb answers {a:?} on `{sql}`"),
        (Err(e), Ok(b)) => panic!("mpedb errors ({e}) but sqlite answers {b:?} on `{sql}`"),
    }
}

/// Every previously refused body kind, cross-checked cell-for-cell.
#[test]
fn materialized_bodies_match_sqlite() {
    let d = db();
    let queries = [
        // ---- the Django shape: aggregate over a grouped body ---------------
        "SELECT count(*) FROM (SELECT a, count(*) FROM t GROUP BY a) sub",
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) subquery",
        "SELECT sum(n) FROM (SELECT a, count(*) AS n FROM t GROUP BY a) sub",
        // ---- aggregate body -------------------------------------------------
        "SELECT * FROM (SELECT count(*) AS n, sum(a) AS tot FROM t) sub",
        "SELECT n + 1 FROM (SELECT count(*) AS n FROM t) sub",
        "SELECT * FROM (SELECT avg(a) AS m FROM t) sub",
        // ---- GROUP BY / HAVING body ----------------------------------------
        "SELECT * FROM (SELECT a, count(*) AS n FROM t GROUP BY a HAVING count(*) > 1) sub ORDER BY a",
        "SELECT n, count(*) FROM (SELECT a, count(*) AS n FROM t GROUP BY a) sub GROUP BY n ORDER BY n",
        // ---- DISTINCT body --------------------------------------------------
        "SELECT count(*) FROM (SELECT DISTINCT a FROM t) sub",
        "SELECT * FROM (SELECT DISTINCT a, s FROM t) sub ORDER BY a, s",
        // ---- renamed / aliased / computed projections ----------------------
        "SELECT x FROM (SELECT a AS x, s AS y FROM t) sub WHERE y = 'x' ORDER BY x",
        "SELECT sub.x FROM (SELECT a AS x FROM t GROUP BY a) sub WHERE sub.x > 10 ORDER BY sub.x",
        "SELECT dbl FROM (SELECT a * 2 AS dbl FROM t WHERE a IS NOT NULL) sub ORDER BY dbl",
        // ---- join body (qualified projections under aliases) ---------------
        "SELECT count(*) FROM (SELECT t.a AS ta, u.b AS ub FROM t JOIN u ON u.id = t.id) sub \
         WHERE ub IS NOT NULL",
        "SELECT ta, ub FROM (SELECT t.a AS ta, u.b AS ub FROM t JOIN u ON u.id = t.id) sub \
         ORDER BY ta, ub",
        "SELECT count(*) FROM (SELECT t.id FROM t LEFT JOIN u ON u.b = t.a WHERE u.id IS NULL) sub",
        // ---- ORDER BY + LIMIT body (the limit binds INSIDE the body) -------
        "SELECT * FROM (SELECT a FROM t WHERE a IS NOT NULL ORDER BY a DESC, id LIMIT 3) sub ORDER BY a",
        "SELECT count(*) FROM (SELECT a FROM t ORDER BY id LIMIT 2 OFFSET 1) sub",
        "SELECT min(a), max(a) FROM (SELECT a FROM t WHERE a IS NOT NULL ORDER BY a LIMIT 4) sub",
        // ---- window body ----------------------------------------------------
        "SELECT count(*) FROM (SELECT a, row_number() OVER (ORDER BY id) AS rn FROM t) sub WHERE rn > 2",
        "SELECT a, rn FROM (SELECT a, row_number() OVER (ORDER BY id) AS rn FROM t) sub \
         WHERE rn <= 3 ORDER BY rn",
        // ---- compound bodies, every set operator ---------------------------
        "SELECT count(*) FROM (SELECT a FROM t UNION SELECT b FROM u) sub",
        "SELECT count(*) FROM (SELECT a FROM t UNION ALL SELECT b FROM u) sub",
        "SELECT count(*) FROM (SELECT a FROM t INTERSECT SELECT b FROM u) sub",
        "SELECT count(*) FROM (SELECT a FROM t EXCEPT SELECT b FROM u) sub",
        "SELECT * FROM (SELECT a FROM t UNION SELECT b FROM u) sub ORDER BY 1",
        "SELECT a FROM (SELECT a FROM t UNION SELECT b FROM u ORDER BY 1 LIMIT 3) sub ORDER BY a",
        // The first arm names the output (sqlite's rule): reference it.
        "SELECT x FROM (SELECT a AS x FROM t UNION SELECT b FROM u) sub WHERE x IS NOT NULL ORDER BY x",
        // ---- duplicates preserved (a derived table is a BAG) ---------------
        "SELECT count(*) FROM (SELECT a FROM t UNION ALL SELECT a FROM t) sub",
        "SELECT count(*) FROM (SELECT a FROM t UNION ALL SELECT a FROM t) sub WHERE a = 20",
        // ---- empty body -----------------------------------------------------
        "SELECT count(*) FROM (SELECT a FROM t WHERE a > 1000 GROUP BY a) sub",
        "SELECT * FROM (SELECT DISTINCT a FROM t WHERE a > 1000) sub",
        "SELECT count(*) FROM (SELECT a FROM t WHERE a > 1000 UNION SELECT b FROM u WHERE b > 1000) sub",
        // ---- the derived table joined against a real table ------------------
        "SELECT count(*) FROM (SELECT a, count(*) AS n FROM t GROUP BY a) d JOIN u ON u.b = d.a",
        "SELECT d.a, d.n, u.id FROM (SELECT a, count(*) AS n FROM t GROUP BY a) d \
         JOIN u ON u.b = d.a ORDER BY d.a, u.id",
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) d LEFT JOIN u ON u.b = d.a",
        // RIGHT JOIN puts the derived table on the preserved-right side (the
        // planner rewrites it to a swapped LEFT — the sentinel moves into the
        // join chain).
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) d RIGHT JOIN u ON u.b = d.a",
        "SELECT u.id, d.a FROM (SELECT a FROM t GROUP BY a) d RIGHT JOIN u ON u.b = d.a \
         ORDER BY u.id",
        // NULLs in the join column: a NULL key matches nothing.
        "SELECT count(*) FROM (SELECT a FROM t) d JOIN u ON u.b = d.a",
        "SELECT count(*) FROM (SELECT b FROM u) d JOIN t ON t.a = d.b",
        // Comma-join (cartesian) with a real table.
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) d, u",
        // ---- outer shapes over the materialized set -------------------------
        "SELECT x, count(*) FROM (SELECT a AS x FROM t UNION ALL SELECT b FROM u) sub \
         GROUP BY x ORDER BY x",
        "SELECT DISTINCT n FROM (SELECT a, count(*) AS n FROM t GROUP BY a) sub ORDER BY n",
        "SELECT * FROM (SELECT a, count(*) AS n FROM t GROUP BY a) sub ORDER BY n DESC, a LIMIT 2",
        "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY id) AS rn FROM t) sub",
        // ---- alias-less derived table (sqlite allows it) --------------------
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a)",
        "SELECT * FROM (SELECT a FROM t UNION SELECT b FROM u) ORDER BY 1",
        "SELECT count(*) FROM (SELECT DISTINCT a FROM t)",
        // ---- nested: a SIMPLE inner (flattened) inside a complex outer ------
        "SELECT count(*) FROM (SELECT a FROM (SELECT a FROM t WHERE a > 0) i GROUP BY a) o",
        // ---- star over duplicate short names from a join body ---------------
        "SELECT count(*) FROM (SELECT t.id, u.id FROM t JOIN u ON u.id = t.id) sub",
    ];
    for q in queries {
        check(&d, q);
    }
    d.verify().unwrap();
}

/// Shapes where BOTH engines must refuse (never a silent one-sided answer),
/// plus mpedb-only refusals that must stay clean errors.
#[test]
fn refusals_and_error_parity() {
    let d = db();
    // Correlated derived table (LATERAL): the body references a table that is
    // not in ITS scope. sqlite refuses too ("no such column") — error parity.
    check(&d, "SELECT * FROM (SELECT count(*) AS n FROM u WHERE u.b = t.a) s JOIN t ON 1");
    // Re-referencing the derived alias as a join operand: sqlite "no such
    // table: d" — error parity.
    check(&d, "SELECT * FROM (SELECT a FROM t GROUP BY a) d JOIN d ON 1");
    // mpedb-only stage-1 refusals (sqlite answers): must be clean errors.
    // A subquery inside the body.
    let e = d
        .query(
            "SELECT count(*) FROM (SELECT a FROM t WHERE a IN (SELECT b FROM u) GROUP BY a) sub",
            &[],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("subquery in the derived-table body"),
        "unexpected refusal text: {e}"
    );
    // A subquery in the outer statement.
    let e = d
        .query(
            "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) sub \
             WHERE sub.a IN (SELECT b FROM u)",
            &[],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("subquery in the outer statement"),
        "unexpected refusal text: {e}"
    );
    // A derived table in a nested position (a compound arm).
    let e = d
        .query(
            "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) x \
             UNION SELECT count(*) FROM u",
            &[],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("outermost FROM"),
        "unexpected refusal text: {e}"
    );
    // A NON-flattenable derived body nested inside another derived body.
    let e = d
        .query(
            "SELECT count(*) FROM (SELECT x FROM (SELECT a AS x FROM t) i GROUP BY x) o",
            &[],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("outermost FROM"),
        "unexpected refusal text: {e}"
    );
    d.verify().unwrap();
}

/// Parameters inside the body: uncorrelated (a param is a constant per
/// execute), so they are legal — direct value assertions.
#[test]
fn params_in_body() {
    let d = db();
    let r = d
        .query(
            "SELECT count(*) FROM (SELECT a FROM t WHERE a > $1 GROUP BY a) sub",
            &[Value::Int(10)],
        )
        .unwrap();
    match r {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]), // a=20,30
        other => panic!("{other:?}"),
    }
    // The same param used in body AND outer.
    let r = d
        .query(
            "SELECT count(*) FROM (SELECT a, count(*) AS n FROM t WHERE a >= $1 GROUP BY a) sub \
             WHERE sub.a > $1",
            &[Value::Int(10)],
        )
        .unwrap();
    match r {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]), // a=20,30
        other => panic!("{other:?}"),
    }
    d.verify().unwrap();
}

/// The #74 budget: a large materialized body under a tiny `max_work_rows` must
/// trip `Error::RuntimeBudget`, attributed to the derived table by name, and
/// the message must name the knob.
#[test]
fn budget_trips_on_runaway_body() {
    let d = open("[runtime]\nmax_work_rows = 20\n\n");
    // 6 × 4 = 24 cartesian rows materialized > 20 budget.
    let e = d
        .query(
            "SELECT count(*) FROM (SELECT t.id FROM t JOIN u ON 1 ORDER BY t.id LIMIT 100) big",
            &[],
        )
        .unwrap_err();
    let msg = e.to_string();
    assert!(
        matches!(e, Error::RuntimeBudget { .. }),
        "expected RuntimeBudget, got {e:?}"
    );
    assert!(msg.contains("max_work_rows"), "msg should name the knob: {msg}");
    // A small body under the same budget still answers.
    let r = d
        .query("SELECT count(*) FROM (SELECT a FROM t GROUP BY a) sub", &[])
        .unwrap();
    match r {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(4)]]),
        other => panic!("{other:?}"),
    }
    d.verify().unwrap();
}

/// The materialized statement round-trips through prepare/execute (the shared
/// registry path exercises encode → decode → validate), and EXPLAIN renders
/// the two components.
#[test]
fn prepared_roundtrip_and_explain() {
    let d = db();
    let h = d
        .prepare("SELECT count(*) FROM (SELECT a, count(*) AS n FROM t GROUP BY a) sub WHERE sub.n > 1")
        .unwrap();
    let r = d.execute(&h, &[]).unwrap();
    match r {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]), // a=10,20
        other => panic!("{other:?}"),
    }
    d.verify().unwrap();
}
