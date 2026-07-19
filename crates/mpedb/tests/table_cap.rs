//! The table-count ceiling (design/DESIGN-TABLE-CAP.md, PLAN_FORMAT 41).
//!
//! `MAX_TABLES` was 128 with an 8-slot system reserve — 120 usable, counting
//! LIFETIME creates because a dropped id is never reused. That is the ceiling
//! Django's `queries` (493 tests) and `backends` labels died on. Footprints and
//! the CDC capture config are now sparse `TableSet`s instead of per-table
//! bitmaps, so the id space is bounded only by cost: 4096 slots, 4088 live.
//!
//! The property these tests exist for is NOT "more tables fit". It is that a
//! table whose id sits **above every retired bitmap boundary** (64, 120, 128)
//! is a first-class table: its plans compile, its footprint names *it* and not
//! some folded alias, it joins correctly with tables on the other side of the
//! boundary, and every answer matches sqlite. A footprint that silently misses
//! or aliases a table is a concurrency/mirror correctness bug, not a cosmetic
//! one — so the check is differential, not self-referential.

use mpedb::{Config, Database, ExecResult, Value};

/// Tables created by the wide-schema fixture. Comfortably past the retired
/// 120-usable ceiling, and past 128 (the old `u128` bitmap width) so ids on
/// both sides of every old boundary are exercised.
const N_TABLES: u32 = 200;

fn config(tag: &str) -> (Config, std::path::PathBuf) {
    let path = std::path::PathBuf::from(format!("/dev/shm/mpedb-tablecap-{tag}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 128
durability = "none"

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => *n,
            other => panic!("expected an int, got {other:?} for {sql}"),
        },
        other => panic!("expected rows, got {other:?} for {sql}"),
    }
}

/// Render a result set as sorted `|`-joined strings so mpedb and sqlite are
/// compared on VALUES, not on row order.
fn rows_of(db: &Database, sql: &str) -> Vec<String> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Null => "NULL".to_string(),
                            Value::Int(n) => n.to_string(),
                            Value::Text(s) => s.clone(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            out.sort();
            out
        }
        other => panic!("expected rows, got {other:?} for {sql}"),
    }
}

fn sqlite_rows(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut st = conn.prepare(sql).unwrap();
    let ncols = st.column_count();
    let mut out: Vec<String> = st
        .query_map([], |row| {
            let mut parts = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let v: rusqlite::types::Value = row.get(i)?;
                parts.push(match v {
                    rusqlite::types::Value::Null => "NULL".to_string(),
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    rusqlite::types::Value::Text(s) => s,
                    other => format!("{other:?}"),
                });
            }
            Ok(parts.join("|"))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out.sort();
    out
}

/// `CREATE TABLE t{k} (id INTEGER PRIMARY KEY, v INTEGER, tag TEXT)` for
/// k in 0..N_TABLES, then three rows each whose `v` encodes the table number —
/// so a join that silently reads the WRONG table produces different values,
/// not merely a different row count.
fn build(db: &Database, conn: &rusqlite::Connection) {
    for k in 0..N_TABLES {
        let ddl = format!(
            "CREATE TABLE t{k} (id INTEGER PRIMARY KEY, v INTEGER NOT NULL, tag TEXT NOT NULL)"
        );
        db.query(&ddl, &[]).unwrap_or_else(|e| panic!("create t{k}: {e}"));
        conn.execute_batch(&format!("{ddl};")).unwrap();
        for r in 1..=3i64 {
            let dml = format!(
                "INSERT INTO t{k} (id, v, tag) VALUES ({r}, {}, 't{k}r{r}')",
                k as i64 * 1000 + r
            );
            db.query(&dml, &[]).unwrap_or_else(|e| panic!("insert t{k}: {e}"));
            conn.execute_batch(&format!("{dml};")).unwrap();
        }
    }
}

#[test]
fn wide_schema_joins_above_the_old_cap_match_sqlite() {
    let (cfg, path) = config("wide");
    let db = Database::open_with_config(cfg).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    build(&db, &conn);

    // The seed table plus 200 user tables is past every retired ceiling.
    assert!(N_TABLES > 128, "the fixture must clear the old u128 width");

    let mut queries: Vec<String> = Vec::new();
    // Single-table reads at, on both sides of, and far past every old boundary.
    for k in [0u32, 55, 63, 64, 119, 120, 127, 128, 129, 150, 199] {
        queries.push(format!("SELECT id, v, tag FROM t{k} ORDER BY id"));
        queries.push(format!("SELECT count(*), sum(v) FROM t{k}"));
        queries.push(format!("SELECT v FROM t{k} WHERE id = 2"));
    }
    // Two-table joins. The pairs deliberately mix: both below the old cap,
    // both above it, and one on each side — the last is what a folded or
    // truncated table bit would get wrong while the others still looked fine.
    for (a, b) in [
        (0u32, 1u32),
        (5, 150),
        (63, 64),
        (119, 120),
        (127, 128),
        (130, 199),
        (140, 141),
        (64, 192),
    ] {
        queries.push(format!(
            "SELECT t{a}.id, t{a}.v, t{b}.v, t{b}.tag FROM t{a} JOIN t{b} ON t{a}.id = t{b}.id \
             ORDER BY t{a}.id"
        ));
        queries.push(format!(
            "SELECT count(*) FROM t{a} JOIN t{b} ON t{a}.id = t{b}.id WHERE t{b}.v > {}",
            b as i64 * 1000
        ));
        queries.push(format!(
            "SELECT t{a}.tag FROM t{a} WHERE t{a}.id IN (SELECT id FROM t{b} WHERE v > 0) \
             ORDER BY t{a}.tag"
        ));
    }
    // Three-table joins spanning the boundary in both directions.
    for (a, b, c) in [(1u32, 130u32, 199u32), (150, 60, 190), (198, 199, 0)] {
        queries.push(format!(
            "SELECT t{a}.v, t{b}.v, t{c}.v FROM t{a} \
             JOIN t{b} ON t{a}.id = t{b}.id JOIN t{c} ON t{b}.id = t{c}.id ORDER BY t{a}.v"
        ));
    }
    // Compounds over high ids (the compound arm's footprint is a UNION of the
    // arms' read sets — the set-union path, not the single-insert path).
    queries.push(
        "SELECT v FROM t150 UNION SELECT v FROM t151 UNION SELECT v FROM t199 ORDER BY v".into(),
    );
    queries.push("SELECT tag FROM t199 EXCEPT SELECT tag FROM t198 ORDER BY tag".into());

    let mut checked = 0usize;
    let mut wrong = Vec::new();
    for q in &queries {
        let mine = rows_of(&db, q);
        let theirs = sqlite_rows(&conn, q);
        if mine != theirs {
            wrong.push(format!("{q}\n  mpedb : {mine:?}\n  sqlite: {theirs:?}"));
        }
        checked += 1;
    }
    assert!(wrong.is_empty(), "{} wrong of {checked}:\n{}", wrong.len(), wrong.join("\n"));
    assert!(checked >= 60, "too few comparisons ({checked})");

    // WRITES to a high-id table land in that table and nowhere else. This is
    // the property a folded/truncated write-set bit would break silently.
    for k in [0u32, 64, 128, 199] {
        db.query(&format!("UPDATE t{k} SET v = v + 7 WHERE id = 1"), &[]).unwrap();
        conn.execute_batch(&format!("UPDATE t{k} SET v = v + 7 WHERE id = 1;")).unwrap();
    }
    for k in 0..N_TABLES {
        let q = format!("SELECT id, v FROM t{k} ORDER BY id");
        assert_eq!(rows_of(&db, &q), sqlite_rows(&conn, &q), "after updates: t{k}");
    }
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn footprint_names_the_high_table_id_it_touches() {
    // EXPLAIN renders the footprint's table sets. A plan over a high-id table
    // must name THAT id — under the retired bitmap this was a bit position, and
    // any fold or truncation showed up as the wrong (or a missing) table.
    let (cfg, path) = config("footprint");
    let db = Database::open_with_config(cfg).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    build(&db, &conn);

    // Table ids are assigned in creation order after the seed table, so t{k}
    // has id k+1. Confirm through EXPLAIN rather than assuming.
    for k in [0u32, 64, 128, 199] {
        let id = k + 1;
        let ExecResult::Explain(text) =
            db.query(&format!("EXPLAIN SELECT v FROM t{k} WHERE id = 1"), &[]).unwrap()
        else {
            panic!("expected an explain");
        };
        assert!(
            text.contains(&format!("tables_read=[{id}]")),
            "t{k} (id {id}) footprint wrong:\n{text}"
        );
        assert!(text.contains("tables_written=[]"), "read plan claims writes:\n{text}");

        let ExecResult::Explain(text) =
            db.query(&format!("EXPLAIN UPDATE t{k} SET v = 1 WHERE id = 1"), &[]).unwrap()
        else {
            panic!("expected an explain");
        };
        assert!(
            text.contains(&format!("tables_written=[{id}]")),
            "t{k} (id {id}) write footprint wrong:\n{text}"
        );
    }
    // A join's read set carries BOTH ids, ascending — the canonical order the
    // encoding depends on, even though the SQL names the higher one first.
    let ExecResult::Explain(text) = db
        .query("EXPLAIN SELECT t199.v FROM t199 JOIN t5 ON t199.id = t5.id", &[])
        .unwrap()
    else {
        panic!("expected an explain");
    };
    assert!(text.contains("tables_read=[6,200]"), "join footprint wrong:\n{text}");

    // Row counts are unaffected by any of this; a sanity anchor.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t199"), 3);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
