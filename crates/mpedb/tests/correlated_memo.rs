//! Correlated subqueries with repeated correlation values — the case where the
//! per-row inner-subplan memoization (MPEE "buy the inner cells once, stream the
//! probes") actually fires. The memoized result MUST equal per-row re-execution.
//! Cross-checked against sqlite 3.45.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-corrmemo-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n\n[[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n",
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn ints(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn correlated_subqueries_with_repeated_keys_match_sqlite() {
    let (db, path) = open();
    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, g INT)", &[]).unwrap();
    db.query("CREATE TABLE b (bid INTEGER PRIMARY KEY, g INT, v INT)", &[]).unwrap();
    // a.g has repeats (10 thrice, 20 twice, 30 once) — every repeat is a memo hit.
    db.query("INSERT INTO a (id, g) VALUES (1,10),(2,10),(3,20),(4,20),(5,30),(6,10)", &[]).unwrap();
    db.query("INSERT INTO b (bid, g, v) VALUES (1,10,100),(2,10,101),(3,20,200)", &[]).unwrap();

    // Correlated EXISTS: g=10 and g=20 have b-rows, g=30 does not.
    let got = ints(db.query(
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.g = a.g) ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(6)]]);

    // Correlated scalar subquery (count): (1,2)(2,2)(3,1)(4,1)(5,0)(6,2).
    let got = ints(db.query(
        "SELECT id, (SELECT count(*) FROM b WHERE b.g = a.g) FROM a ORDER BY id",
        &[],
    ).unwrap());
    let want: Vec<i64> = vec![2, 2, 1, 1, 0, 2];
    let counts: Vec<i64> = got.iter().map(|r| match r[1] { Value::Int(n) => n, ref o => panic!("{o:?}") }).collect();
    assert_eq!(counts, want);

    // Correlated IN: b.v>100 → g in {10,20}; a.g in {10,20} → id 1,2,3,4,6.
    let got = ints(db.query(
        "SELECT id FROM a WHERE a.g IN (SELECT g FROM b WHERE b.v > 100) ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(6)]]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A/B timing: many outer rows sharing few correlation keys is where memoizing
/// the inner subplan turns O(N_outer × M_inner) into O(distinct_keys × M_inner).
/// Ignored by default (timing, not correctness); run with
/// `cargo test -p mpedb --test correlated_memo -- --ignored --nocapture`.
#[test]
#[ignore]
fn memo_speedup_on_repeated_correlation_keys() {
    use std::time::Instant;
    let (db, path) = open();
    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, g INT)", &[]).unwrap();
    db.query("CREATE TABLE b (bid INTEGER PRIMARY KEY, g INT, v INT)", &[]).unwrap();
    // 4000 outer rows, only 5 distinct correlation values → 800× reuse.
    for id in 0..4000 {
        db.query(&format!("INSERT INTO a (id, g) VALUES ({id}, {})", id % 5), &[]).unwrap();
    }
    // 400 inner rows so each inner scan is non-trivial.
    for bid in 0..400 {
        db.query(&format!("INSERT INTO b (bid, g, v) VALUES ({bid}, {}, {bid})", bid % 5), &[]).unwrap();
    }
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.g = a.g AND b.v > 10)";

    let time_it = || {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            db.query(sql, &[]).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        best
    };

    std::env::set_var("MPEDB_NO_SUBPLAN_MEMO", "1");
    let off = time_it();
    std::env::remove_var("MPEDB_NO_SUBPLAN_MEMO");
    let on = time_it();
    println!(
        "correlated EXISTS, 4000 outer × 5 distinct keys: memo OFF {off:.2} ms, ON {on:.2} ms, {:.1}× faster",
        off / on
    );
    let _ = std::fs::remove_file(&path);
}

/// Timing for the EXISTS consumer-cap (LIMIT 1): DISTINCT outer keys (so the
/// memoization never reuses) each matching MANY inner rows. Capping the inner
/// at one row stops the scan at the first match instead of gathering every
/// match. Prints an absolute time; compare against a build with the cap
/// disabled for the A/B. Ignored by default. See P73 for the full session.
#[test]
#[ignore]
fn exists_cap_absolute_time() {
    use std::time::Instant;
    let (db, path) = open();
    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, g INT)", &[]).unwrap();
    db.query("CREATE TABLE b (bid INTEGER PRIMARY KEY, g INT)", &[]).unwrap();
    // 100 DISTINCT outer keys (g = 0..99) — no memo reuse.
    for id in 0..100 {
        db.query(&format!("INSERT INTO a (id, g) VALUES ({id}, {id})"), &[]).unwrap();
    }
    // 30000 inner rows, g = bid % 100 → each outer key matches 300 inner rows,
    // interleaved so the first match is found early but gathering all is costly.
    for bid in 0..30000 {
        db.query(&format!("INSERT INTO b (bid, g) VALUES ({bid}, {})", bid % 100), &[]).unwrap();
    }
    let sql = "SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.g = a.g)";
    // count(*) over the outer would hit the aggregate+correlated refusal, so
    // materialize the ids instead.
    let sql = if db.query(sql, &[]).is_err() {
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.g = a.g)"
    } else {
        sql
    };
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        db.query(sql, &[]).unwrap();
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("EXISTS cap, 100 distinct outer × 300 inner matches each: {best:.2} ms");
    let _ = std::fs::remove_file(&path);
}
