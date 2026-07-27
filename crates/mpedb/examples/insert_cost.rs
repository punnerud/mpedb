//! Where do the microseconds in one insert actually go? (#164 follow-on)
//!
//! #164 spent a day on the writer lock and then measured it: the whole
//! acquire/release pair is 10 ns on Linux, 455 ns on macOS, 847 ns on Windows,
//! against ~13 µs for one uncontended insert. So the lock is between 0.1 % and
//! 7 % of an insert, and the other 93–99.9 % has never been looked at.
//!
//! This splits that remainder along the two seams the engine's own design
//! draws, so the answer is a share of a known total rather than a profile
//! nobody can act on:
//!
//! 1. **`query(sql)` vs `execute(hash)`** — the facade's whole premise is
//!    "SQL compiles once to a content-hashed plan; `execute(hash, params)` is
//!    the hot path with zero parsing". `query` takes the compile-or-look-up
//!    road every call: schema gate, policy catalog, view catalog, a read pin
//!    for the planner's row counts, tunables, cost policy, model. The
//!    difference between the two is what a caller pays for not preparing.
//! 2. **One row per transaction vs N** — every autocommit commit does the
//!    catalog writeback, the freelist fixpoint, the meta flip and the notify
//!    publish. Batching amortises all of it, and the ratio says how much of an
//!    insert is per-ROW work versus per-COMMIT work.
//!
//! Non-durable throughout (`durability = none`), so no fsync is in any number
//! here and the ring is off — this measures the direct path deliberately.
//!
//! ```text
//! cargo run --release -p mpedb --example insert_cost -- --rows 20000 --dir /tmp/x
//! ```
//!
//! # What it found, and what fixing it did
//!
//! Seam 1 was 71 % of an insert: `query(sql)` re-derived, on every single call,
//! a plan it had already derived — the facade's caches are all keyed by the
//! hash of the FINISHED plan, so reaching one costs the compile it would save.
//! #166 added a SQL-text memo in front of them. Three runs each on an idle
//! box, 20k rows, same binary except for the change:
//!
//! ```text
//!   arm                     before    after
//!   query(sql)              13.80     5.41 us    <- 2.55x
//!   execute(hash)            3.97     4.02 us    control
//!   10 rows / txn            1.59     1.61 us    control
//!   100 rows / txn           1.29     1.27 us    control
//! ```
//!
//! The three control arms are why the first row is readable at all: they are
//! arms this change does not touch, and they did not move. Run this isolated —
//! a first attempt measured under corpus load and moved ALL FOUR arms up
//! proportionally, which is the signature of a busy box and not of a fix.
//!
//! Seam 2 is untouched and now dominates: batching still buys 3.4x over the
//! best single-row number, so per-COMMIT work (freelist fixpoint, catalog
//! writeback, meta flip, notify publish) is what an insert is mostly made of
//! once the frontend is gone.

use std::path::PathBuf;
use std::time::Instant;

use mpedb::{Config, Database, Value};

const SCHEMA: &str = "\n[[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\
     \n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
     \n  [[table.column]]\n  name = \"s\"\n  type = \"text\"\n";

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

fn open(dir: &std::path::Path, tag: &str) -> (Database, PathBuf) {
    let path = dir.join(format!("insertcost-{tag}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 256\nmax_readers = 16\n\
         durability = \"none\"\n{SCHEMA}",
        mpedb::toml_escape(&path.display().to_string())
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, path)
}

fn us_per(t: Instant, n: u64) -> f64 {
    t.elapsed().as_secs_f64() * 1e6 / n as f64
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: u64 = arg(&args, "--rows", "20000").parse().expect("--rows");
    let dir = PathBuf::from(arg(&args, "--dir", "."));
    std::fs::create_dir_all(&dir).expect("--dir");

    const SQL: &str = "INSERT INTO t (id, s) VALUES (?, ?)";
    let mut out: Vec<(&str, f64)> = Vec::new();

    // A: the shape `runner_noise` measures — SQL text every call.
    {
        let (db, p) = open(&dir, "query");
        let t = Instant::now();
        for i in 0..rows as i64 {
            db.query(SQL, &[Value::Int(i), Value::Text(format!("v{i}"))]).unwrap();
        }
        out.push(("query(sql) — compile-or-look-up per call", us_per(t, rows)));
        drop(db);
        let _ = std::fs::remove_file(&p);
    }

    // B: the hot path the design is built around.
    {
        let (db, p) = open(&dir, "execute");
        let h = db.prepare(SQL).unwrap();
        let t = Instant::now();
        for i in 0..rows as i64 {
            db.execute(&h, &[Value::Int(i), Value::Text(format!("v{i}"))]).unwrap();
        }
        out.push(("execute(hash) — zero parsing, still one txn/row", us_per(t, rows)));
        drop(db);
        let _ = std::fs::remove_file(&p);
    }

    // B2: the shape the C-API shim actually runs (#168). With a transaction
    // open, `sqlite3_step` routes to `WriteSession::query(text, params)`
    // (capi/src/lib.rs) — SQL text, inside a txn. That is every ORM, and it is
    // a different code path from arm A: it compiles against the SESSION's
    // schema view, so #166's memo did not reach it.
    {
        let (db, p) = open(&dir, "txnquery");
        let t = Instant::now();
        let mut i = 0i64;
        for _ in 0..(rows / 10) {
            let mut s = db.begin().unwrap();
            for _ in 0..10 {
                s.query(SQL, &[Value::Int(i), Value::Text(format!("v{i}"))]).unwrap();
                i += 1;
            }
            s.commit().unwrap();
        }
        out.push((
            "session.query(sql) — 10/txn, the shim's shape",
            us_per(t, (rows / 10) * 10),
        ));
        drop(db);
        let _ = std::fs::remove_file(&p);
    }

    // C+D: the same hot path with the commit amortised over a batch.
    for batch in [10u64, 100] {
        let (db, p) = open(&dir, &format!("batch{batch}"));
        let h = db.prepare(SQL).unwrap();
        let t = Instant::now();
        let mut i = 0i64;
        for _ in 0..(rows / batch) {
            let mut s = db.begin().unwrap();
            for _ in 0..batch {
                s.execute(&h, &[Value::Int(i), Value::Text(format!("v{i}"))]).unwrap();
                i += 1;
            }
            s.commit().unwrap();
        }
        out.push((
            if batch == 10 { "  ...10 rows per transaction" } else { "  ...100 rows per transaction" },
            us_per(t, (rows / batch) * batch),
        ));
        drop(db);
        let _ = std::fs::remove_file(&p);
    }

    println!("one insert, non-durable, single writer — {rows} rows each\n");
    let base = out[0].1;
    for (label, us) in &out {
        println!("  {label:<46} {us:>7.2} us   ({:.0}% of the first)", 100.0 * us / base);
    }
    println!(
        "\n  for scale, the whole writer-lock acquire+release pair measured by\n\
         `mpedb-core --example lock_cost`: 0.01 us on Linux, 0.46 on macOS,\n\
         0.85 on Windows. The lock is not where this time goes."
    );
}
