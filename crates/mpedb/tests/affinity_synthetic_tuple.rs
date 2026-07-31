//! A column's DECLARED affinity, carried into every SYNTHETIC tuple mpedb
//! builds — the grouped tuple, a materialized derived table's working table,
//! and a recursive CTE's working table.
//!
//! It was not carried, and that was a WRONG ANSWER rather than a refusal.
//! `decimal(10,2)` is `(ColumnType::Any, Affinity::Numeric)` — the affinity
//! comes from the DECLARED type name, and `Affinity::implied_by` cannot
//! reproduce it (`Any → Blob`). So the synthetic tuples got `Blob`, and
//! `HAVING price > '50'` compared REAL against TEXT by storage class and was
//! FALSE for every row, while the byte-identical `WHERE price > '50'` over the
//! base table answered correctly.
//!
//! Django binds `Decimal` as TEXT (`register_adapter(decimal.Decimal, str)`),
//! so this is not an exotic shape: it is what every `.filter(price__gt=…)` on a
//! `DecimalField` over a grouped query does.
//!
//! The rule, MEASURED against sqlite 3.45 at both ends before it was written:
//! only a bare COLUMN carries an affinity. `HAVING max(price) > '50'` and
//! `HAVING price + 0 > '50'` are BOTH empty in sqlite too — an aggregate result
//! and a computed expression genuinely have none. Getting that half wrong would
//! have traded one wrong answer for another.

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
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

/// `price` is `decimal(10,2)` — the declared name that yields NUMERIC affinity
/// over an `Any` type, which is the whole point. `code` is TEXT (the control:
/// its affinity is TEXT and must NOT start numerifying), `n` a plain INTEGER.
const CREATE: &str = "CREATE TABLE t (\
    id INTEGER PRIMARY KEY, \
    price decimal(10,2), \
    code TEXT, \
    n INTEGER)";

const ROWS: &[(i64, f64, &str, i64)] = &[
    (1, 30.00, "30", 2),
    (2, 23.09, "23.09", 1),
    (3, 29.69, "29.69", 1),
    (4, 29.69, "29.69", 3),
    (5, 82.80, "82.80", 2),
    (6, 75.00, "75", 1),
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, p, c, n)| {
            format!("INSERT INTO t (id, price, code, n) VALUES ({id}, {p}, '{c}', {n})")
        })
        .collect()
}

fn db() -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-affsyn-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 16\ndurability = \"none\"\n");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query(CREATE, &[]).unwrap();
    for s in insert_statements() {
        db.query(&s, &[]).unwrap();
    }
    Tmp { db, path }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Float(f) => {
            // sqlite prints a REAL with `%!.15g` — SIGNIFICANT digits, always
            // with a decimal point. `{f:.15}` is 15 DECIMAL PLACES, which
            // renders 82.8 as 82.799999999999997 and fails a comparison the
            // engine got right. Rust's `{}` is the shortest round-tripping
            // form and agrees with %g for every value here.
            let s = format!("{f}");
            if s.contains(['.', 'e', 'N', 'i']) { s } else { format!("{s}.0") }
        }
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb refused `{sql}`: {e}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = format!("{CREATE};\n");
    for s in insert_statements() {
        script.push_str(&s);
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

fn same(d: &Database, sql: &str) {
    assert_eq!(mpedb_rows(d, sql), sqlite_rows(sql), "mismatch on `{sql}`");
}

/// The GROUPED tuple: a group key that is a bare column keeps its affinity, so
/// HAVING compares the way the identical WHERE does.
#[test]
fn a_group_key_keeps_its_declared_affinity() {
    let d = db();
    for q in [
        // The control: the same predicate over the BASE row always worked.
        "SELECT id FROM t WHERE price > '50' ORDER BY id",
        // …and now over the grouped tuple.
        "SELECT id FROM t GROUP BY id, price HAVING price > '50' ORDER BY id",
        "SELECT id FROM t GROUP BY id, price HAVING price = '29.69' ORDER BY id",
        "SELECT id FROM t GROUP BY id, price HAVING price <= '30' ORDER BY id",
        "SELECT n, count(*) FROM t GROUP BY n, price HAVING price > '50' ORDER BY n",
        // The key in the PROJECTION as well as in HAVING.
        "SELECT price, count(*) FROM t GROUP BY price HAVING price > '25' ORDER BY price",
        // A TEXT key must NOT start numerifying — its affinity is TEXT, and
        // sqlite compares it as text. This is the control that a blanket
        // "numerify everything" fix would break.
        "SELECT id FROM t GROUP BY id, code HAVING code > '50' ORDER BY id",
        "SELECT id FROM t GROUP BY id, code HAVING code = '30' ORDER BY id",
        // (An INTEGER key against a text literal — `n > '1'` — is the
        // int64-vs-text COMPARISON rule, a different gap; it is pinned there.)
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// What must NOT gain an affinity: an aggregate RESULT and a COMPUTED key.
/// Both are empty in sqlite, and the fix has to leave them that way.
#[test]
fn an_aggregate_result_and_a_computed_key_carry_none() {
    let d = db();
    for q in [
        "SELECT id FROM t GROUP BY id HAVING max(price) > '50' ORDER BY id",
        "SELECT id FROM t GROUP BY id HAVING min(price) = '30' ORDER BY id",
        "SELECT id FROM t GROUP BY id HAVING sum(price) > '50' ORDER BY id",
        // (`count(*) > '0'` belongs to the int64-vs-text comparison rule, not
        // to this one — it is pinned where that rule lives.)
        // (`price + 0 > '50'` would show the same thing, but it trips the
        // int64-vs-text comparison refusal first — pinned with that rule.)
        // A TEXT aggregate over a TEXT column still compares as text.
        "SELECT id FROM t GROUP BY id HAVING max(code) > '50' ORDER BY id",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// A MATERIALIZED derived table's working table. `DISTINCT` is what stops the
/// body from being flattened — a flattened body reads the base column directly
/// and never lost the affinity, which is why this needs the non-flattenable
/// shape to reproduce at all.
#[test]
fn a_materialized_derived_column_keeps_its_affinity() {
    let d = db();
    for q in [
        "SELECT count(*) FROM (SELECT DISTINCT price AS x FROM t) s WHERE x > '50'",
        "SELECT x FROM (SELECT DISTINCT price AS x FROM t) s WHERE x > '50' ORDER BY x",
        // Django's `.distinct().aggregate(...)` shape, verbatim in structure.
        "SELECT avg(CASE WHEN x = '29.69' THEN y END) \
         FROM (SELECT DISTINCT price AS x, n AS y FROM t) s",
        "SELECT sum(CASE WHEN x > '50' THEN 1 ELSE 0 END) \
         FROM (SELECT DISTINCT price AS x FROM t) s",
        // GROUP BY is the other non-flattenable body.
        "SELECT count(*) FROM (SELECT price AS x, count(*) AS c FROM t GROUP BY price) s \
         WHERE x > '50'",
        // The TEXT control, through the same materialization.
        "SELECT count(*) FROM (SELECT DISTINCT code AS x FROM t) s WHERE x > '50'",
        // A COMPUTED body column carries none, in both engines.
        "SELECT count(*) FROM (SELECT DISTINCT price + 0 AS x FROM t) s WHERE x > '50'",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// A RECURSIVE CTE's working table takes its columns from the ANCHOR, so it
/// carries the anchor's affinities by the same rule. Sharing the walk with the
/// derived path is what keeps these two from drifting.
#[test]
fn a_recursive_cte_column_keeps_its_affinity() {
    let d = db();
    for q in [
        "WITH RECURSIVE r(i, p) AS (\
           SELECT id, price FROM t WHERE id = 1 \
           UNION ALL \
           SELECT r.i + 1, t.price FROM r JOIN t ON t.id = r.i + 1 WHERE r.i < 6) \
         SELECT i FROM r WHERE p > '50' ORDER BY i",
        "WITH RECURSIVE r(i, p) AS (\
           SELECT id, price FROM t WHERE id = 1 \
           UNION ALL \
           SELECT r.i + 1, t.price FROM r JOIN t ON t.id = r.i + 1 WHERE r.i < 6) \
         SELECT count(*) FROM r WHERE p = '29.69'",
        // The TEXT control through the same working table.
        "WITH RECURSIVE r(i, c) AS (\
           SELECT id, code FROM t WHERE id = 1 \
           UNION ALL \
           SELECT r.i + 1, t.code FROM r JOIN t ON t.id = r.i + 1 WHERE r.i < 6) \
         SELECT i FROM r WHERE c > '50' ORDER BY i",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// The plan carries the affinities on the wire, so a plan that round-trips
/// through encode + decode + validate answers the same. This is what the
/// format-68 bump buys: recomputing them after decode would let the
/// compile-time and validate-time working-table defs drift.
#[test]
fn the_affinities_survive_a_plan_round_trip() {
    let d = db();
    for sql in [
        "SELECT count(*) FROM (SELECT DISTINCT price AS x FROM t) s WHERE x > '50'",
        "WITH RECURSIVE r(i, p) AS (\
           SELECT id, price FROM t WHERE id = 1 \
           UNION ALL \
           SELECT r.i + 1, t.price FROM r JOIN t ON t.id = r.i + 1 WHERE r.i < 6) \
         SELECT count(*) FROM r WHERE p > '50'",
    ] {
        let detached = d.prepare_detached(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        // `execute_detached` goes through decode + validate on the wire blob,
        // not the in-process plan cache.
        let got = match d.execute_detached(&detached, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.iter().map(|r| r.iter().map(render).collect::<Vec<_>>()).collect::<Vec<_>>()
            }
            other => panic!("expected rows, got {other:?}"),
        };
        assert_eq!(got, sqlite_rows(sql), "after a round trip: `{sql}`");
    }
    d.verify().unwrap();
}
