//! WINDOW functions over a GROUPED result — the window phase running AFTER
//! `GROUP BY`/`HAVING`, over the grouped rows.
//!
//! `SELECT dept, count(*), rank() OVER (ORDER BY count(*) DESC) … GROUP BY dept`
//! was refused as "window functions together with GROUP BY / aggregates in one
//! SELECT". The two are not alternatives: SQL evaluates windows after
//! aggregation, so the window's own clauses resolve against the GROUPED tuple
//! `[keys ‖ aggs ‖ bare]` — `PARTITION BY dept` names a group key and
//! `ORDER BY count(*)` names an aggregate slot.
//!
//! Everything here is checked cell-for-cell against sqlite 3.45, because the
//! PHASE ORDER is exactly where an engine gets it wrong and still looks
//! plausible: HAVING must run BEFORE the window (a filtered-out group is not in
//! the partition), and LIMIT must run AFTER it (`LIMIT 1` must not shrink the
//! partition the window ran over).

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

/// `dept` a text group key with NULLs, `reg` a second key (for a two-key
/// GROUP BY under a PARTITION BY), `sal` a nullable INT to aggregate — NULLs so
/// `max(sal)` can be NULL and the window then orders by a NULL.
const CREATE: &str =
    "CREATE TABLE e (id INTEGER PRIMARY KEY, dept TEXT, reg TEXT, sal INTEGER)";

/// One seed row: `(id, dept, reg, sal)`.
type Row = (i64, Option<&'static str>, Option<&'static str>, Option<i64>);

const ROWS: &[Row] = &[
    (1, Some("acct"), Some("N"), Some(10)),
    (2, Some("acct"), Some("N"), Some(20)),
    (3, Some("mgmt"), Some("S"), Some(30)),
    (4, Some("mgmt"), Some("S"), Some(30)),
    (5, Some("mgmt"), Some("N"), Some(5)),
    (6, Some("ops"), Some("S"), None),
    (7, Some("ops"), None, Some(7)),
    (8, None, Some("N"), Some(1)),
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, d, r, s)| {
            let q = |x: &Option<&str>| x.map_or("NULL".into(), |v| format!("'{v}'"));
            let n = |x: &Option<i64>| x.map_or("NULL".to_string(), |v| v.to_string());
            format!(
                "INSERT INTO e (id, dept, reg, sal) VALUES ({id}, {}, {}, {})",
                q(d),
                q(r),
                n(s)
            )
        })
        .collect()
}

fn db() -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-wovergrp-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\ndurability = \"none\"\n"
    );
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
            // The sqlite CLI prints a REAL with a decimal point always present
            // — `1.0`, never `1` — which is how a cume_dist of exactly 1
            // otherwise read as a mismatch that was purely this helper's.
            let s = format!("{f:.15}");
            let s = s.trim_end_matches('0');
            if s.ends_with('.') { format!("{s}0") } else { s.to_string() }
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

/// The window's own clauses resolve against the GROUPED tuple: a group key, an
/// aggregate slot, or an expression over them.
#[test]
fn window_clauses_read_the_grouped_tuple() {
    let d = db();
    for q in [
        "SELECT dept, count(*), rank() OVER (ORDER BY count(*) DESC) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, sum(sal), row_number() OVER (ORDER BY sum(sal)) FROM e GROUP BY dept ORDER BY dept",
        // An aggregate named ONLY inside the OVER clause — it still needs a
        // grouped-tuple slot of its own.
        "SELECT dept, rank() OVER (ORDER BY max(sal)) FROM e GROUP BY dept ORDER BY dept",
        // PARTITION BY a group key, and by a second key.
        "SELECT dept, reg, count(*), rank() OVER (PARTITION BY reg ORDER BY count(*) DESC) \
         FROM e GROUP BY dept, reg ORDER BY dept, reg",
        "SELECT dept, count(*), rank() OVER (PARTITION BY dept) FROM e GROUP BY dept ORDER BY dept",
        // An aggregate OF an aggregate: the inner is the grouping one.
        "SELECT dept, sum(sal), sum(sum(sal)) OVER () FROM e GROUP BY dept ORDER BY dept",
        // A window over a GROUP BY EXPRESSION key.
        "SELECT sal / 10, count(*), rank() OVER (ORDER BY sal / 10) FROM e GROUP BY sal / 10 ORDER BY 1",
        // No GROUP BY at all — one group, and the window runs over that one row.
        "SELECT count(*), rank() OVER (ORDER BY count(*)) FROM e",
        "SELECT sum(sal), sum(sal) + count(*) OVER () FROM e",
        // Two windows with an identical spec share one slot; both must answer.
        "SELECT dept, rank() OVER (ORDER BY count(*)), rank() OVER (ORDER BY count(*)) \
         FROM e GROUP BY dept ORDER BY dept",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// The PHASE ORDER, which is the whole correctness question: HAVING before the
/// window, ORDER BY and LIMIT after it.
#[test]
fn having_runs_before_the_window_and_limit_after() {
    let d = db();
    for q in [
        // A group HAVING removes is not in the partition — the counts and ranks
        // below are over the SURVIVORS.
        "SELECT dept, count(*), rank() OVER (ORDER BY count(*)) FROM e GROUP BY dept \
         HAVING count(*) > 1 ORDER BY dept",
        "SELECT dept, sum(sal), count(*) OVER () FROM e GROUP BY dept \
         HAVING sum(sal) IS NOT NULL ORDER BY dept",
        // LIMIT must NOT shrink the partition: `count(*) OVER ()` is the number
        // of GROUPS, not the number of rows returned.
        "SELECT dept, count(*) OVER () FROM e GROUP BY dept ORDER BY dept LIMIT 2",
        "SELECT dept, sum(sum(sal)) OVER () FROM e GROUP BY dept ORDER BY dept LIMIT 1",
        "SELECT dept, count(*) OVER () FROM e GROUP BY dept ORDER BY dept LIMIT 2 OFFSET 1",
        // ORDER BY a window result, by ordinal and through an alias.
        "SELECT dept, rank() OVER (ORDER BY sum(sal) DESC) AS r FROM e GROUP BY dept ORDER BY r, dept",
        "SELECT dept, rank() OVER (ORDER BY sum(sal) DESC) FROM e GROUP BY dept ORDER BY 2, 1",
        // DISTINCT over the projection, AFTER the window. This is a SECOND plan
        // construction in the aggregate planner, and it dropped the windows —
        // the projection then read a slot the tuple no longer had.
        "SELECT DISTINCT count(*) OVER () FROM e GROUP BY dept",
        "SELECT DISTINCT dept, count(*) OVER () FROM e GROUP BY dept ORDER BY dept",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// Frames, exclusions and the value/offset functions all work over the grouped
/// rows — the phase is the same one, given a different tuple.
#[test]
fn frames_and_value_functions_over_groups() {
    let d = db();
    for q in [
        "SELECT dept, sum(sal), sum(sum(sal)) OVER (ORDER BY dept ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
         FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, count(*), group_concat(dept) OVER (ORDER BY count(*) \
         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, count(*) OVER (ORDER BY dept \
         ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) \
         FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, count(*), sum(count(*)) OVER (ORDER BY count(*) \
         RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, sum(sal), lag(sum(sal)) OVER (ORDER BY dept) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, first_value(dept) OVER (ORDER BY count(*) DESC) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, ntile(2) OVER (ORDER BY count(*)) FROM e GROUP BY dept ORDER BY dept",
        "SELECT dept, max(sal), cume_dist() OVER (ORDER BY max(sal)) FROM e GROUP BY dept ORDER BY dept",
    ] {
        same(&d, q);
    }
    d.verify().unwrap();
}

/// An EMPTY result and a JOIN — the two shapes where the grouped tuple is built
/// differently and the window phase must still see the right rows.
#[test]
fn empty_groups_and_joins() {
    let d = db();
    d.query("CREATE TABLE hq (dept TEXT PRIMARY KEY, city TEXT)", &[]).unwrap();
    for s in [
        "INSERT INTO hq (dept, city) VALUES ('acct', 'Oslo')",
        "INSERT INTO hq (dept, city) VALUES ('mgmt', 'Bergen')",
        "INSERT INTO hq (dept, city) VALUES ('ops', 'Tromso')",
    ] {
        d.query(s, &[]).unwrap();
    }
    // The oracle needs the same extra table, so these are checked directly
    // against a script that creates both.
    let mut script = format!("{CREATE};\nCREATE TABLE hq (dept TEXT PRIMARY KEY, city TEXT);\n");
    for s in insert_statements() {
        script.push_str(&s);
        script.push_str(";\n");
    }
    for s in [
        "INSERT INTO hq (dept, city) VALUES ('acct', 'Oslo')",
        "INSERT INTO hq (dept, city) VALUES ('mgmt', 'Bergen')",
        "INSERT INTO hq (dept, city) VALUES ('ops', 'Tromso')",
    ] {
        script.push_str(s);
        script.push_str(";\n");
    }
    for q in [
        // No group survives the WHERE: zero rows, and a window over zero rows.
        "SELECT dept, count(*), rank() OVER (ORDER BY count(*)) FROM e WHERE sal > 1000 \
         GROUP BY dept ORDER BY dept",
        // …but a groupless aggregate still yields ONE row, and the window runs
        // over that one.
        "SELECT count(*), rank() OVER () FROM e WHERE sal > 1000",
        // A grouped, windowed JOIN.
        "SELECT e.dept, count(*), rank() OVER (ORDER BY count(*) DESC) \
         FROM e JOIN hq ON hq.dept = e.dept GROUP BY e.dept ORDER BY e.dept",
        "SELECT hq.city, sum(e.sal), sum(sum(e.sal)) OVER () \
         FROM e JOIN hq ON hq.dept = e.dept GROUP BY hq.city ORDER BY hq.city",
    ] {
        let mut sc = script.clone();
        sc.push_str(q);
        sc.push_str(";\n");
        let want: Vec<Vec<String>> = sqlite_oracle::script_stdout(&sc, "")
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('|').map(str::to_string).collect())
            .collect();
        assert_eq!(mpedb_rows(&d, q), want, "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// A window may still not appear in WHERE — that refusal is about WHERE, not
/// about aggregation, and it must not have been loosened by accident.
#[test]
fn a_window_in_where_is_still_refused() {
    let d = db();
    for q in [
        "SELECT dept FROM e WHERE row_number() OVER (ORDER BY dept) = 1 GROUP BY dept",
        "SELECT dept, count(*) FROM e GROUP BY dept HAVING rank() OVER (ORDER BY count(*)) = 1",
    ] {
        assert!(
            d.query(q, &[]).is_err(),
            "expected a clean refusal for `{q}`, but it was accepted"
        );
    }
    d.verify().unwrap();
}
