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
/// Body-OWNED subplans (design/DESIGN-DERIVED-TABLES.md §5.5, PLAN_FORMAT 52).
///
/// The rank-1 Django shape is a body that PROJECTS a correlated `EXISTS` (or
/// scalar subquery) under an alias, consumed by the outer query — the
/// `.annotate(Exists(...))` + `.aggregate()`/`.filter()` pair. The `EXISTS`
/// correlates to the BODY's row, so the fill has to happen while the body
/// materialises; by the time the outer runs, the alias is just a column.
///
/// Every one is diffed cell-for-cell against the bundled oracle, including the
/// shapes where a mis-filled slot would produce a plausible-but-wrong answer
/// (all-true / all-false / first-row-repeated), which is why the fixtures have
/// rows on both sides of every predicate.
#[test]
fn body_owned_subplans_match_sqlite() {
    let d = db();
    let queries = [
        // ---- THE Django shape: correlated EXISTS projected, then filtered --
        "SELECT count(*) FROM (SELECT t.id, EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub WHERE f",
        "SELECT count(*) FROM (SELECT t.id, EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub WHERE NOT f",
        "SELECT id, f FROM (SELECT t.id AS id, EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub ORDER BY id",
        // …and the same with the EXISTS negated inside the body.
        "SELECT count(*) FROM (SELECT t.id, NOT EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub WHERE f",
        // ---- a correlated SCALAR subquery projected under an alias ---------
        "SELECT id, m FROM (SELECT t.id AS id, (SELECT max(u.b) FROM u WHERE u.b = t.a) AS m \
         FROM t) sub ORDER BY id",
        "SELECT count(m) FROM (SELECT (SELECT max(u.b) FROM u WHERE u.b = t.a) AS m FROM t) sub",
        "SELECT sum(m) FROM (SELECT (SELECT max(u.b) FROM u WHERE u.b = t.a) AS m FROM t) sub",
        // ---- the body's lift in its own WHERE, correlated and not ----------
        "SELECT count(*) FROM (SELECT a FROM t WHERE a IN (SELECT b FROM u) GROUP BY a) sub",
        "SELECT * FROM (SELECT a FROM t WHERE a IN (SELECT b FROM u) GROUP BY a) sub ORDER BY a",
        "SELECT count(*) FROM (SELECT t.id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a)) sub",
        "SELECT count(*) FROM (SELECT t.id FROM t WHERE t.a = (SELECT max(b) FROM u)) sub",
        // ---- UNCORRELATED lift in the body: filled ONCE, before the body ---
        "SELECT * FROM (SELECT a, (SELECT count(*) FROM u) AS n FROM t GROUP BY a) sub ORDER BY a",
        // ---- both kinds in ONE body, so the two fill phases must not collide
        "SELECT * FROM (SELECT t.id AS id, (SELECT count(*) FROM u) AS n, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f FROM t) sub ORDER BY id",
        // ---- the body also aggregates/groups/distincts around its lift -----
        "SELECT * FROM (SELECT DISTINCT EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f FROM t) sub \
         ORDER BY f",
        "SELECT f, count(*) FROM (SELECT EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f FROM t) sub \
         GROUP BY f ORDER BY f",
        // ---- a lift NESTED inside the body's own lift (#73 §3) -------------
        "SELECT count(*) FROM (SELECT t.id FROM t WHERE t.a IN \
         (SELECT b FROM u WHERE b = (SELECT max(b) FROM u))) sub",
        // ---- the outer still aggregates/orders/limits over the result ------
        "SELECT max(id) FROM (SELECT t.id AS id, EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub WHERE f",
        "SELECT id FROM (SELECT t.id AS id, EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f \
         FROM t) sub WHERE f ORDER BY id DESC LIMIT 2",
    ];
    for q in queries {
        check(&d, q);
    }
}

/// The `C-API-COMPAT.md` run-4 category this closes, in Django's own wire
/// shapes: "derived body has an aliased/renamed column" (7 statements) —
/// every one of which "projects a correlated scalar/`EXISTS`/window under an
/// alias and is consumed by an outer aggregate `FILTER`/argument".
///
/// `FILTER` and the aggregate ARGUMENT are the positions that made the old
/// row's diagnosis ("a projection remap converts it into correlated subquery
/// in an aggregate argument") true; with body ownership the correlation is
/// resolved one level down and the outer sees a plain materialised column, so
/// those positions are ordinary again. Diffed against the oracle.
#[test]
fn the_django_annotate_exists_then_aggregate_shapes_match_sqlite() {
    let d = db();
    for q in [
        // .annotate(Exists(...)).aggregate(Count(...))
        "SELECT count(*) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub WHERE has_thing",
        // …consumed by an aggregate FILTER instead of a WHERE.
        "SELECT count(*) FILTER (WHERE has_thing) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub",
        "SELECT count(*) FILTER (WHERE NOT has_thing) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub",
        // …consumed as the aggregate ARGUMENT. (`sum()` of a BOOL is a
        // separate, PRE-EXISTING refusal — arithmetic on mpedb's first-class
        // bool is rigid, the documented boundary of the int/bool bridge — so
        // the aggregate-argument position is exercised with `max`, which is
        // class-ordered rather than arithmetic.)
        "SELECT max(has_thing), max(id) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub",
        // …grouped BY the projected correlated value.
        "SELECT has_thing, count(*) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub \
         GROUP BY has_thing ORDER BY has_thing",
        // A correlated SCALAR annotation, same two consumers.
        "SELECT count(*) FROM (SELECT t.id AS id, \
         (SELECT count(*) FROM u WHERE u.b = t.a) AS n FROM t) sub WHERE n > 0",
        "SELECT sum(n) FROM (SELECT (SELECT count(*) FROM u WHERE u.b = t.a) AS n FROM t) sub",
        // The body ALSO DISTINCTs/joins around its lift — the combination the
        // run-4 table filed under "JOIN"/"DISTINCT" and root-caused to the
        // same thing.
        "SELECT count(*) FROM (SELECT DISTINCT t.a AS a, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing FROM t) sub WHERE has_thing",
        "SELECT count(*) FROM (SELECT t.id AS id, \
         EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS has_thing \
         FROM t JOIN u ON u.id = t.id) sub WHERE has_thing",
        // NOT here, and deliberately: a body that GROUPS and projects the
        // correlated value without making it a key —
        // `SELECT t.a, EXISTS(…) FROM t GROUP BY t.a` — is the "correlated
        // subquery in a grouped SELECT-list expression that is not itself a
        // GROUP BY key" straggler the run-4 table already names. That is a
        // genuine per-GROUP hole (no single row's correlation applies to a
        // collapsed group), refused by name, and body ownership neither
        // creates nor closes it.
    ] {
        check(&d, q);
    }
}

/// A body-owned lift reserves a parameter slot, so the CALLER's parameter count
/// must not move — the accounting bug this would otherwise be
/// (`n_subplan_slots`) shows up as "expected 2 parameters, got 1".
#[test]
fn body_owned_subplans_do_not_shift_the_caller_parameter_count() {
    let d = db();
    let sql = "SELECT count(*) FROM (SELECT t.id, \
               EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f FROM t WHERE t.id > $1) sub WHERE f";
    // Zero user params on one side of the reserved region, one on the other.
    let rows = mpedb_rows(&d, "SELECT count(*) FROM (SELECT t.id, \
        EXISTS (SELECT 1 FROM u WHERE u.b = t.a) AS f FROM t) sub WHERE f")
        .unwrap();
    assert_eq!(rows.len(), 1);
    let h = d.prepare(sql).unwrap();
    // Expectations taken from the oracle, not by hand.
    for (arg, want) in [(0i64, "4"), (3, "1"), (99, "0")] {
        match d.execute(&h, &[Value::Int(arg)]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                assert_eq!(render(rows[0][0].clone()), want, "for $1 = {arg}")
            }
            other => panic!("expected rows, got {other:?}"),
        }
    }
    // Too few / too many parameters must still be caught, at the CALLER's
    // arity — not at `n_params`, which the reserved slot inflated.
    assert!(d.execute(&h, &[]).is_err(), "one user parameter is required");
    assert!(
        d.execute(&h, &[Value::Int(1), Value::Int(2)]).is_err(),
        "only one user parameter exists"
    );
}

#[test]
fn refusals_and_error_parity() {
    let d = db();
    // Correlated derived table (LATERAL): the body references a table that is
    // not in ITS scope. sqlite refuses too ("no such column") — error parity.
    check(&d, "SELECT * FROM (SELECT count(*) AS n FROM u WHERE u.b = t.a) s JOIN t ON 1");
    // Re-referencing the derived alias as a join operand: sqlite "no such
    // table: d" — error parity.
    check(&d, "SELECT * FROM (SELECT a FROM t GROUP BY a) d JOIN d ON 1");
    // A subquery inside the body used to be a stage-1 refusal; the body now
    // OWNS its lifts (format 52, §5.5), so this ANSWERS and is diffed above in
    // `body_owned_subplans_match_sqlite`. A lift in a COMPOUND body was refused
    // one stage longer — until the ARMS owned theirs (format 56, §5.6). Both
    // answer now, and are diffed against sqlite.
    check(
        &d,
        "SELECT count(*) FROM (SELECT a FROM t WHERE a IN (SELECT b FROM u) \
         UNION SELECT id FROM u) sub",
    );
    check(
        &d,
        "SELECT count(*) FROM (SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) \
         UNION ALL SELECT id FROM u WHERE b IS NULL) sub",
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
    // A derived table as a compound arm materialises (format 58) — answered.
    d.query(
        "SELECT count(*) FROM (SELECT a FROM t GROUP BY a) x \
         UNION SELECT count(*) FROM u",
        &[],
    )
    .expect("nested derived compound arm");
    // A filtering consumer over a nested derived is still not a compound-arm
    // materialise — refuse by name.
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
    // Nested derived inside a lifted subquery body (not a compound arm).
    let e = d
        .query(
            "SELECT id FROM t WHERE EXISTS \
             (SELECT 1 FROM (SELECT b FROM u GROUP BY b) x WHERE x.b IN (SELECT a FROM t))",
            &[],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("outermost FROM"),
        "expected the nested-derived refusal, got: {e}"
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

    // The #101 memory-proportional twin: the HELD materialized set is checked
    // against `max_join_cells`, attributed to the derived table by name.
    let d = open("[runtime]\nmax_join_cells = 10\n\n");
    let e = d
        // The LIMIT makes the body non-flattenable, so it materializes.
        .query("SELECT count(*) FROM (SELECT a, s FROM t LIMIT 100) big", &[])
        .unwrap_err(); // 6 rows × 2 cells = 12 > 10
    let msg = e.to_string();
    assert!(msg.contains("max_join_cells"), "msg should name the knob: {msg}");
    assert!(msg.contains("derived table"), "msg should attribute the holder: {msg}");
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
