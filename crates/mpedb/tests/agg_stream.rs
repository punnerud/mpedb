//! The STREAMING aggregate (#123 §5.1) — differential, at every batch size.
//!
//! `exec/aggregate.rs` no longer gathers its input: a plain PK-ordered scan is
//! drained in batches and folded into the accumulators, so a bounded-group
//! aggregate holds O(groups) instead of O(input rows). `agg_stream_mem.rs`
//! asserts the memory claim; this file asserts the only thing that makes the
//! memory claim worth having — **the answers did not change**.
//!
//! Two properties, one battery:
//!
//! 1. **Differential.** Every shape is checked against the BUNDLED sqlite
//!    oracle (3.45.0, compiled in — never the ambient binary), comparing the
//!    VALUE *and* `typeof()` of every aggregate. Aggregation is where type
//!    promotion lives and where memory is the least trustworthy guide:
//!    `sum` of integers is an integer but `avg` of integers is a real, `sum`
//!    over an empty set is NULL while `count` is 0, every aggregate but
//!    `count(*)` skips NULLs, and a per-row mix of int and real promotes.
//!    None of that is asserted from memory; it is asserted from the oracle.
//!
//! 2. **`C`-invariance** (design/DESIGN-STREAM-EXEC.md §4.2). The batch size is
//!    config-derived and a statement's result may not depend on config, so the
//!    battery is re-run under `MPEDB_FOLD_BATCH ∈ {1, 2, 7, 256}` — every one
//!    of them against the same oracle, which is a stronger statement than
//!    "the four agree with each other". A one-row batch means a fresh B+tree
//!    descent per row and a resume bound per row; a 256-row batch is one
//!    descent for the whole 24-row fixture. `group_concat`, the bare-column
//!    witness and `min`/`max` tie-breaking are all order-sensitive, so a
//!    resume bound that skipped or repeated a row would show up here.
//!
//! The fixture is 24 rows on purpose: `1`, `2` and `7` all divide it unevenly,
//! so the last batch is short and the resume bound is exercised at a boundary
//! that is not the end of the table.

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

// ------------------------------------------------------------------ fixture

const CREATE: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, x INTEGER, y REAL, s TEXT)",
    // Never populated: the empty-set rules (count → 0, everything else → NULL)
    // are a different code path (the synthetic empty group).
    "CREATE TABLE e (id INTEGER PRIMARY KEY, x INTEGER)",
    // The correlated / join partner. `ref` matches some `t.id`s, one twice,
    // and one dangling — so a correlated EXISTS is not accidentally 1:1.
    "CREATE TABLE c (cid INTEGER PRIMARY KEY, ref INTEGER)",
];

/// 24 rows: NULLs in every nullable column at different strides, duplicate
/// `x` values (so `DISTINCT` and min/max ties matter), negative values, and a
/// bounded group key `g` alongside the unbounded `id`.
fn rows() -> Vec<String> {
    let mut out = Vec::new();
    for i in 1i64..=24 {
        let g = i % 4;
        let x = if i % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", (i * 13) % 17 - 8)
        };
        let y = if i % 5 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", i as f64 * 1.5 - 3.0)
        };
        let s = if i % 6 == 0 {
            "NULL".to_string()
        } else {
            format!("'s{}'", i % 5)
        };
        out.push(format!("INSERT INTO t (id, g, x, y, s) VALUES ({i}, {g}, {x}, {y}, {s})"));
    }
    for (cid, r) in [(1i64, 1i64), (2, 2), (3, 4), (4, 4), (5, 99), (6, 13)] {
        out.push(format!("INSERT INTO c (cid, ref) VALUES ({cid}, {r})"));
    }
    out
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-agg-stream-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    open_at(&path, "")
}

/// Open with an optional extra config fragment (used to vary
/// `[runtime] max_join_cells`, which is both the fold's batch divisor and the
/// group-map tripwire).
fn open_at(path: &str, extra: &str) -> Tmp {
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n{extra}\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in CREATE {
        db.query(stmt, &[]).unwrap();
    }
    for stmt in rows() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path: path.to_string() }
}

// -------------------------------------------------------------- the oracle

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for stmt in CREATE {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    for stmt in rows() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// One mpedb value against sqlite's rendered cell. Integers and text (which is
/// what every `typeof()` column is) compare EXACTLY; floats within a relative
/// tolerance, since sqlite renders ~15 significant digits.
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
        other => panic!("unexpected value type in an aggregate result: {other:?}"),
    }
}

fn agree(db: &Database, query: &str) {
    let batch = std::env::var("MPEDB_FOLD_BATCH").unwrap_or_else(|_| "default".into());
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(
        m.len(),
        s.len(),
        "[C={batch}] row count differs for `{query}`: mpedb {m:?} vs sqlite {s:?}"
    );
    for (mr, sr) in m.iter().zip(&s) {
        assert_eq!(
            mr.len(),
            sr.len(),
            "[C={batch}] column count differs for `{query}`: mpedb {mr:?} vs sqlite {sr:?}"
        );
        for (mv, sv) in mr.iter().zip(sr) {
            assert!(
                cell_matches(mv, sv),
                "[C={batch}] cell mismatch for `{query}`: mpedb {mv:?} vs sqlite {sv:?}\n  \
                 mpedb row {mr:?}\n  sqlite row {sr:?}"
            );
        }
    }
}

// -------------------------------------------------------------- the battery

/// Every aggregate shape the streaming fold touches, and every shape it
/// deliberately does NOT (join, correlated FILTER, point/index access) so a
/// dispatch mistake that routes one of those into the fold shows up as a wrong
/// answer rather than as a silent memory win.
const BATTERY: &[&str] = &[
    // --- the folds themselves, with their promotion rules ------------------
    "SELECT count(*), typeof(count(*)) FROM t",
    "SELECT count(x), typeof(count(x)) FROM t",
    "SELECT sum(x), typeof(sum(x)) FROM t",
    "SELECT avg(x), typeof(avg(x)) FROM t",
    "SELECT min(x), typeof(min(x)), max(x), typeof(max(x)) FROM t",
    "SELECT sum(y), typeof(sum(y)), avg(y), typeof(avg(y)) FROM t",
    "SELECT min(y), typeof(min(y)), max(y), typeof(max(y)) FROM t",
    "SELECT min(s), typeof(min(s)), max(s), typeof(max(s)) FROM t",
    "SELECT total(x), typeof(total(x)) FROM t",
    // Order-sensitive across batch boundaries: group_concat joins in SCAN
    // order, so a resume bound off by one row is visible here and nowhere else.
    "SELECT group_concat(s) FROM t",
    "SELECT group_concat(x) FROM t",
    // --- the empty set: count is 0, everything else is NULL ----------------
    "SELECT count(*), typeof(count(*)), count(x), sum(x), typeof(sum(x)) FROM e",
    "SELECT avg(x), typeof(avg(x)), min(x), max(x), total(x), typeof(total(x)) FROM e",
    "SELECT group_concat(x), typeof(group_concat(x)) FROM e",
    // An empty set reached by a FILTER rather than by an empty table.
    "SELECT count(*), sum(x), typeof(sum(x)) FROM t WHERE x > 1000",
    // --- mixed int/real per row: promotion inside one aggregate ------------
    "SELECT sum(CASE WHEN id % 2 = 0 THEN x ELSE y END), \
            typeof(sum(CASE WHEN id % 2 = 0 THEN x ELSE y END)) FROM t",
    "SELECT avg(CASE WHEN id % 2 = 0 THEN x ELSE y END), \
            min(CASE WHEN id % 2 = 0 THEN x ELSE y END), \
            typeof(min(CASE WHEN id % 2 = 0 THEN x ELSE y END)) FROM t",
    // --- DISTINCT aggregates: O(distinct), NOT O(rows) ---------------------
    "SELECT count(DISTINCT x), typeof(count(DISTINCT x)) FROM t",
    "SELECT sum(DISTINCT x), typeof(sum(DISTINCT x)), avg(DISTINCT x), \
            typeof(avg(DISTINCT x)) FROM t",
    "SELECT count(DISTINCT s), group_concat(DISTINCT s) FROM t",
    // --- GROUP BY, bounded --------------------------------------------------
    "SELECT g, count(*), sum(x), typeof(sum(x)), avg(x), typeof(avg(x)), \
            min(x), max(x), group_concat(s) FROM t GROUP BY g ORDER BY g",
    "SELECT g, count(DISTINCT x) FROM t GROUP BY g ORDER BY g",
    // --- GROUP BY, one group per row (the state is genuinely O(input)) -----
    "SELECT id, count(*), sum(x), typeof(sum(x)) FROM t GROUP BY id ORDER BY id",
    // --- GROUP BY over a computed key and over text ------------------------
    "SELECT x % 3, count(*), sum(id) FROM t GROUP BY x % 3 ORDER BY 1",
    "SELECT s, count(*), sum(x) FROM t GROUP BY s ORDER BY s",
    // --- HAVING (filters AFTER the fold) -----------------------------------
    "SELECT g, count(*) FROM t GROUP BY g HAVING count(*) > 5 ORDER BY g",
    "SELECT g, sum(x) FROM t GROUP BY g HAVING sum(x) IS NULL OR sum(x) < 0 ORDER BY g",
    // --- ORDER BY / LIMIT / DISTINCT over the grouped output ---------------
    "SELECT g, count(*) FROM t GROUP BY g ORDER BY count(*) DESC, g",
    "SELECT g, count(*) FROM t GROUP BY g ORDER BY count(*) DESC, g LIMIT 2",
    "SELECT g, count(*) FROM t GROUP BY g ORDER BY g LIMIT 2 OFFSET 1",
    "SELECT DISTINCT count(*) FROM t GROUP BY g",
    // --- WHERE pushed into the streamed scan -------------------------------
    "SELECT count(*), sum(x), typeof(sum(x)) FROM t WHERE x > 0",
    "SELECT count(*), min(id), max(id) FROM t WHERE s IS NOT NULL",
    "SELECT g, count(*) FROM t WHERE x IS NOT NULL GROUP BY g ORDER BY g",
    // --- PkRange: the resume bound must respect BOTH ends ------------------
    "SELECT count(*), sum(x), min(id), max(id) FROM t WHERE id > 5 AND id <= 19",
    "SELECT count(*), sum(id) FROM t WHERE id >= 20",
    "SELECT count(*), sum(id) FROM t WHERE id < 4",
    // --- PkPoint / no-match: NOT streamed, must still be right -------------
    "SELECT count(*), sum(x), typeof(sum(x)) FROM t WHERE id = 7",
    "SELECT count(*), sum(x), typeof(sum(x)) FROM t WHERE id = 999",
    // --- sqlite bare columns: the witness row, across batch boundaries -----
    "SELECT s, max(x) FROM t",
    "SELECT s, min(x) FROM t",
    "SELECT g, s, max(x) FROM t GROUP BY g ORDER BY g",
    "SELECT g, s, min(x) FROM t GROUP BY g ORDER BY g",
    // Zero min/max → sqlite's "arbitrary" pick, which is the lowest rowid.
    "SELECT g, s, count(*) FROM t GROUP BY g ORDER BY g",
    // --- FILTER (WHERE …) --------------------------------------------------
    "SELECT count(*) FILTER (WHERE x > 0), sum(x) FILTER (WHERE g = 1), \
            typeof(sum(x) FILTER (WHERE g = 1)) FROM t",
    "SELECT g, count(*) FILTER (WHERE x IS NULL) FROM t GROUP BY g ORDER BY g",
    // A filter that rejects EVERY row: the empty-group value again.
    "SELECT count(*) FILTER (WHERE x > 1000), sum(x) FILTER (WHERE x > 1000) FROM t",
    // --- shapes that keep the materializing path ---------------------------
    // Correlated subquery inside FILTER (#73 §1).
    "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) FROM t",
    "SELECT g, count(*) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) \
            FROM t GROUP BY g ORDER BY g",
    // Aggregate over a join.
    "SELECT count(*), sum(t.x), typeof(sum(t.x)) FROM t JOIN c ON c.ref = t.id",
    "SELECT t.g, count(*) FROM t JOIN c ON c.ref = t.id GROUP BY t.g ORDER BY t.g",
    // Aggregate over a LEFT join (NULL-extended rows are counted by count(*)
    // and skipped by count(col) — the classic).
    "SELECT count(*), count(c.cid) FROM t LEFT JOIN c ON c.ref = t.id",
];

/// The battery, at whatever batch size `MPEDB_FOLD_BATCH` names (unset =
/// production default). Re-run at four sizes by [`c_invariance`].
#[test]
fn differential_battery() {
    let db = db();
    for q in BATTERY {
        agree(&db, q);
    }
    db.verify().unwrap();
}

/// §4.2's test obligation: `C` must not be observable. Re-runs
/// [`differential_battery`] in a child process per batch size, because the
/// override is read once per process (and because `set_var` is not sound to
/// race against other tests).
///
/// Not `max_join_cells`: that knob is ALSO the group-map tripwire, so the
/// values of it that force a one-row batch refuse the grouped statements
/// instead of chunking them. [`budget_does_not_change_answers`] varies the
/// knob itself, over the range where both roles are satisfiable.
#[test]
fn c_invariance() {
    if std::env::var_os("MPEDB_FOLD_BATCH").is_some() {
        return; // child: `differential_battery` is doing the work
    }
    let exe = std::env::current_exe().expect("test executable path");
    for c in ["1", "2", "7", "256"] {
        let out = std::process::Command::new(&exe)
            .args(["--exact", "differential_battery", "--test-threads", "1"])
            .env("MPEDB_FOLD_BATCH", c)
            .output()
            .expect("re-run the battery");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the differential battery failed at MPEDB_FOLD_BATCH={c}:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A filter that matched nothing also exits 0, and would make this test
        // pass while asserting nothing at all.
        assert!(
            stdout.contains("1 passed"),
            "the child at MPEDB_FOLD_BATCH={c} did not RUN the battery:\n{stdout}"
        );
    }
}

/// The same invariance through the knob that ships: `max_join_cells` divides
/// down to the batch size (§4.2), so four different budgets must produce four
/// identical answers. The values are all large enough that the group map fits
/// — the tripwire's own behaviour is [`group_map_is_governed_by_the_budget`].
#[test]
fn budget_does_not_change_answers() {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let mut baseline: Option<Vec<Vec<Vec<Value>>>> = None;
    for (i, cells) in [0u64, 512, 4096, 268_435_456].into_iter().enumerate() {
        let path = format!(
            "{dir}/mpedb-agg-budget-{}-{}-{i}.mpedb",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        );
        let _ = std::fs::remove_file(&path);
        let db = open_at(
            &path,
            &format!("\n[runtime]\nmax_work_rows = 0\nmax_join_cells = {cells}\n"),
        );
        let got: Vec<Vec<Vec<Value>>> = BATTERY.iter().map(|q| mpedb_rows(&db, q)).collect();
        match &baseline {
            None => baseline = Some(got),
            Some(want) => {
                for (q, (a, b)) in BATTERY.iter().zip(want.iter().zip(&got)) {
                    assert_eq!(a, b, "max_join_cells = {cells} changed the answer to `{q}`");
                }
            }
        }
    }
}

/// #123 §4.3: the input is no longer held, but the GROUP MAP is — it is
/// O(distinct keys) and no chunk size makes it smaller — so it takes the
/// tripwire the join's intermediate product already has, on the same knob.
/// Without this an unbounded `GROUP BY` would be the one shape that got
/// *less* governed by streaming, which is the wrong direction.
#[test]
fn group_map_is_governed_by_the_budget() {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-agg-groupcap-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // 24 groups × (1 key + 1 accumulator) = 48 cells; 20 is not enough.
    let db = open_at(&path, "\n[runtime]\nmax_work_rows = 0\nmax_join_cells = 20\n");
    let err = db
        .query("SELECT id, count(*) FROM t GROUP BY id", &[])
        .expect_err("an unbounded GROUP BY must trip the cell budget, not grow silently");
    let msg = err.to_string();
    assert!(
        msg.contains("max_join_cells"),
        "the error should name the knob to raise: {msg}"
    );
    assert!(
        msg.contains("group map"),
        "the error should say it was the GROUP MAP and not the input — a user who \
         raises the knob after a group-map trip is doing the right thing: {msg}"
    );
    // The SAME budget runs a bounded aggregate fine: what is governed is the
    // irreducible state, not the input, which is exactly the §4.3 change of
    // meaning. 4 groups × 2 cells = 8 ≤ 20, over the same 24 input rows.
    assert_eq!(
        mpedb_rows(&db, "SELECT count(*) FROM t"),
        vec![vec![Value::Int(24)]],
        "a scalar aggregate over 24 rows must not trip a 20-cell budget"
    );
    let g = mpedb_rows(&db, "SELECT g, count(*) FROM t GROUP BY g ORDER BY g");
    assert_eq!(g.len(), 4, "four bounded groups fit: {g:?}");
}

/// `sum()` over integers that overflow: whatever sqlite does, mpedb must do —
/// and the streaming fold must not change WHEN it happens. Taken from the
/// oracle rather than from memory, because the two plausible answers (raise,
/// or promote to real) are both defensible and only one is sqlite's.
#[test]
fn integer_overflow_in_sum_matches_the_oracle() {
    let script = "CREATE TABLE o (id INTEGER PRIMARY KEY, v INTEGER);\n\
                  INSERT INTO o VALUES (1, 9223372036854775807);\n\
                  INSERT INTO o VALUES (2, 9223372036854775807);\n\
                  SELECT sum(v), typeof(sum(v)) FROM o;\n";
    let oracle = sqlite_oracle::try_script_stdout(script, "NULL");

    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-agg-ovf-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query("CREATE TABLE o (id INTEGER PRIMARY KEY, v INTEGER)", &[]).unwrap();
    db.query("INSERT INTO o VALUES (1, 9223372036854775807)", &[]).unwrap();
    db.query("INSERT INTO o VALUES (2, 9223372036854775807)", &[]).unwrap();
    let got = db.query("SELECT sum(v), typeof(sum(v)) FROM o", &[]);

    match (oracle, got) {
        (Err(oe), Err(me)) => {
            // Both refuse. The messages need not be byte-equal (they are not
            // elsewhere either), but both must say "overflow".
            let (oe, me) = (oe.to_lowercase(), me.to_string().to_lowercase());
            assert!(oe.contains("overflow"), "sqlite: {oe}");
            assert!(me.contains("overflow"), "mpedb: {me}");
        }
        (Ok(o), Ok(ExecResult::Rows { rows, .. })) => {
            let s: Vec<&str> = o.trim().split('|').collect();
            assert_eq!(rows.len(), 1, "one row: {rows:?}");
            for (mv, sv) in rows[0].iter().zip(&s) {
                assert!(cell_matches(mv, sv), "overflow sum: mpedb {mv:?} vs sqlite {sv:?}");
            }
        }
        (o, m) => panic!(
            "sqlite and mpedb disagree on whether sum() overflow is an error:\n  \
             sqlite: {o:?}\n  mpedb:  {m:?}"
        ),
    }
    let _ = std::fs::remove_file(&path);
}

/// The streaming fold runs on a READ context. Inside a `WriteSession` the
/// context's `scan_rows_capped` materializes anyway
/// (`TxnCtx::scans_incrementally` is false), so the aggregate takes the
/// unchanged path — and must give the unchanged answer, INCLUDING over the
/// session's own uncommitted writes, which no snapshot-based stream could see.
#[test]
fn write_session_aggregates_see_uncommitted_rows() {
    let db = db();
    let mut w = db.begin().unwrap();
    assert_eq!(
        match w.query("SELECT count(*), sum(x) FROM t", &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        },
        mpedb_rows(&db, "SELECT count(*), sum(x) FROM t")
    );
    w.query("INSERT INTO t (id, g, x, y, s) VALUES (100, 0, 1000, 1.0, 'z')", &[])
        .unwrap();
    let inside = match w.query("SELECT count(*), max(x) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        inside,
        vec![vec![Value::Int(25), Value::Int(1000)]],
        "an aggregate inside a write session must see the session's own writes"
    );
    w.rollback();
    assert_eq!(
        mpedb_rows(&db, "SELECT count(*), max(x) FROM t"),
        vec![vec![Value::Int(24), Value::Int(8)]]
    );
}
