//! What does the MPEE join-order solver buy in MEMORY?
//!
//! The wall-clock case for #114 is well measured (`INNOVATIONS.md` §4.8,
//! `design/DESIGN-MPEE-SOLVER.md` §10). The memory case was an anecdote — "it
//! used to OOM and now it doesn't" — until this probe. The claim under test is
//! structural: a bad join order carries an intermediate that is the PRODUCT of
//! everything placed so far, a good one carries one row per step, so the
//! solver's memory effect should be super-linear in the width of the chain.
//!
//! Both arms are the SAME BINARY: `MPEDB_NO_MPEE=1` leaves every chain in the
//! user's textual order (the pre-#114 behaviour). Two builds have already been
//! the source of one false A/B in this repo, so there is exactly one here.
//! `crates/mpedb/tests/mpee_solver.rs::the_mpee_kill_switch_selects_the_textual_order`
//! asserts the switch actually selects the arm it claims to, in whichever arm
//! the harness set it — run it in BOTH arms before believing any number below.
//!
//! **The headline metric is `cells`, not `rss`.** `max_join_cells` counts the
//! `Value` cells a join HOLDS (`exec/gather.rs`), and it is a pure function of
//! data and plan — so the peak is a property of the ENGINE, not of the machine,
//! the allocator or the timer. This probe recovers it exactly by bisecting the
//! budget: the smallest budget under which the statement passes IS the peak,
//! because `charge` trips on `live > budget`. RSS is reported beside it as
//! corroboration only; it is noisy, it includes the mmap'd file pages, and it
//! is quantised by the allocator.
//!
//! ```text
//! usage: mpee_memory <mode> <shape> [arg]
//!
//!   mode   cells    bisect [runtime] max_join_cells -> exact peak live cells
//!          rss      run once unlimited; peak RSS via getrusage(RUSAGE_SELF)
//!          base     build the fixture, run NO join; the arm-independent floor
//!          compile  median plan-compile time (the solver's OWN cost), [arg] reps
//!
//!   shape  chainN   N tables chained by PK equalities, FROM scrambled
//!                   odds-then-evens, the only constant anchor written LAST
//!                   (the `mpee_solver.rs` harness, = select5's `join-17-4`)
//!          j17      `join-17-4` verbatim: 17 tables, 3 columns each
//!          plainN   an ORDINARY 3-table join, N rows, already in a good
//!                   textual order — the control group
//!
//!   arg    cells/rss: the cap, in cells (default 64,000,000 ~ 2.5 GB at the
//!          40 B/cell calibration). A shape whose peak exceeds the cap reports
//!          a THRESHOLD (`>cap`), not a ratio — that is a different kind of
//!          result and the report must not conflate them.
//! ```
//!
//! Every mode prints `rows=` and `digest=` so the driver can assert the two
//! arms return the SAME ANSWER. Reordering preserves the result set exactly;
//! an arm that disagreed would outrank every number here.

use mpedb::{Config, Database, ExecResult, Value};

/// Default bisection ceiling: 64 M cells ~ 2.5 GB resident at the 40 B/cell
/// calibration constant. Above this the OFF arm is not measured, it is simply
/// reported as exceeding the cap.
const DEFAULT_CAP: u64 = 64_000_000;

// ------------------------------------------------------------------ fixtures

struct Tmp {
    db: Database,
    path: String,
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn tmp_path(tag: &str) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let p = format!("{dir}/mpedb-mpee-mem-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&p);
    p
}

/// `n` tables `tK(a int64 PK, b int64)`, 10 rows each — the `select5` shape,
/// identical to `tests/mpee_solver.rs::open_chain`.
fn open_chain(n: usize, cells: u64) -> Tmp {
    let path = tmp_path(&format!("chain{n}"));
    let mut toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = {cells}\n"
    );
    for k in 1..=n {
        toml.push_str(&format!(
            "\n[[table]]\nname = \"t{k}\"\nprimary_key = [\"a\"]\n\
             \x20 [[table.column]]\n  name = \"a\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"b\"\n  type = \"int64\"\n"
        ));
    }
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for k in 1..=n {
        let vals: Vec<String> = (1..=10).map(|i| format!("({i}, {})", i % 10 + 1)).collect();
        db.query(&format!("INSERT INTO t{k} (a, b) VALUES {}", vals.join(", ")), &[]).unwrap();
    }
    Tmp { db, path }
}

fn chain_where(n: usize, anchor: i64) -> String {
    let mut conj: Vec<String> = (1..n).map(|k| format!("t{k}.b = t{}.a", k + 1)).collect();
    conj.push(format!("t{n}.a = {anchor}"));
    conj.join(" AND ")
}

/// FROM odds-then-evens: consecutive entries are NOT adjacent in the join
/// path, so in the written order every step but the first two is a product.
fn scrambled_sql(n: usize, anchor: i64) -> String {
    let mut from: Vec<String> = (1..=n).step_by(2).map(|k| format!("t{k}")).collect();
    from.extend((2..=n).step_by(2).map(|k| format!("t{k}")));
    format!("SELECT t1.a FROM {} WHERE {}", from.join(", "), chain_where(n, anchor))
}

const J17: [u32; 17] = [1, 4, 6, 9, 10, 14, 24, 25, 27, 38, 47, 53, 54, 56, 58, 61, 63];

const J17_SQL: &str =
    "SELECT x24,x6,x53,x1,x54,x61,x58,x63,x56,x47,x27,x38,x4,x25,x9,x14,x10 \
     FROM t9,t56,t53,t61,t54,t1,t27,t4,t38,t14,t63,t10,t25,t24,t47,t58,t6 \
     WHERE b61=a38 AND a54=b6 AND a9=b14 AND b53=a14 AND a1=b4 AND b10=a25 \
     AND a53=b63 AND a10=b9 AND b25=a6 AND b27=a47 AND b1=a58 AND a24=b54 \
     AND a63=b58 AND a61=b24 AND b47=a56 AND a38=9 AND b56=a4";

fn open_j17(cells: u64) -> Tmp {
    let path = tmp_path("j17");
    let mut toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = {cells}\n"
    );
    for t in J17 {
        toml.push_str(&format!(
            "\n[[table]]\nname = \"t{t}\"\nprimary_key = [\"a{t}\"]\n\
             \x20 [[table.column]]\n  name = \"a{t}\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"b{t}\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"x{t}\"\n  type = \"text\"\n"
        ));
    }
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for t in J17 {
        let vals: Vec<String> =
            (1..=10).map(|i| format!("({i}, {}, 'table t{t} row {i}')", i % 10 + 1)).collect();
        db.query(&format!("INSERT INTO t{t} (a{t}, b{t}, x{t}) VALUES {}", vals.join(", ")), &[])
            .unwrap();
    }
    Tmp { db, path }
}

/// The CONTROL GROUP: an ordinary 3-table join in a sensible textual order —
/// a fact table filtered to a narrow range, joined to two dimensions by their
/// primary keys. Nothing here is pathological, and the solver should have
/// little or nothing to add. A technique that only pays on adversarial shapes
/// has to say so, which needs this arm measured, not assumed.
fn open_plain(rows: usize, cells: u64) -> Tmp {
    let path = tmp_path(&format!("plain{rows}"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = {cells}\n\n\
         [[table]]\nname = \"fact\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"dim1\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"dim2\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"amt\"\ntype = \"int64\"\n\n\
         [[table]]\nname = \"d1\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"label\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"d2\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"label\"\ntype = \"text\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let dims = (rows / 10).max(2);
    for chunk in (0..rows).collect::<Vec<_>>().chunks(200) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|&i| format!("({}, {}, {}, {})", i + 1, i % dims + 1, i % (dims / 2 + 1) + 1, i))
            .collect();
        db.query(&format!("INSERT INTO fact (id, dim1, dim2, amt) VALUES {}", vals.join(", ")), &[])
            .unwrap();
    }
    for (t, n) in [("d1", dims), ("d2", dims / 2 + 1)] {
        for chunk in (0..n).collect::<Vec<_>>().chunks(200) {
            let vals: Vec<String> =
                chunk.iter().map(|&i| format!("({}, 'label {i}')", i + 1)).collect();
            db.query(&format!("INSERT INTO {t} (id, label) VALUES {}", vals.join(", ")), &[])
                .unwrap();
        }
    }
    Tmp { db, path }
}

const PLAIN_SQL: &str = "SELECT fact.amt, d1.label, d2.label FROM fact, d1, d2 \
                         WHERE fact.dim1 = d1.id AND fact.dim2 = d2.id AND fact.id < 50";

/// Which fixture + which statement, for a shape name.
fn fixture(shape: &str, cells: u64) -> (Tmp, String) {
    if let Some(n) = shape.strip_prefix("chain") {
        let n: usize = n.parse().expect("chainN: N must be a number");
        (open_chain(n, cells), scrambled_sql(n, 4))
    } else if shape == "j17" {
        (open_j17(cells), J17_SQL.to_string())
    } else if let Some(n) = shape.strip_prefix("plain") {
        let n: usize = n.parse().expect("plainN: N must be a number");
        (open_plain(n, cells), PLAIN_SQL.to_string())
    } else {
        panic!("unknown shape `{shape}`");
    }
}

// -------------------------------------------------------------- measurement

/// Peak resident set size of THIS process, in bytes. `ru_maxrss` is bytes on
/// Darwin and KiB on Linux — normalised here, and the unit is stated in the
/// output so a report can never guess wrong.
fn peak_rss_bytes() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let raw = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") { raw } else { raw * 1024 }
}

/// FNV-1a over a canonical rendering of the result set. The point is only that
/// two arms agree, so the encoding just has to be injective enough and stable.
fn digest(rows: &[Vec<Value>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for &x in b {
            h ^= x as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    for r in rows {
        for v in r {
            match v {
                Value::Null => eat(b"N"),
                Value::Int(i) => {
                    eat(b"I");
                    eat(&i.to_le_bytes());
                }
                Value::Float(f) => {
                    eat(b"F");
                    eat(&f.to_bits().to_le_bytes());
                }
                Value::Bool(b) => eat(if *b { b"T" } else { b"f" }),
                Value::Text(t) => {
                    eat(b"S");
                    eat(t.as_bytes());
                }
                other => {
                    eat(b"?");
                    eat(format!("{other:?}").as_bytes());
                }
            }
            eat(b"\x1f");
        }
        eat(b"\x1e");
    }
    h
}

enum Run {
    Ok { rows: usize, digest: u64, micros: u128 },
    /// The join-cells budget tripped: `live` exceeded the cap.
    OverBudget,
    Other(String),
}

fn run(db: &Database, sql: &str) -> Run {
    let t0 = std::time::Instant::now();
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => Run::Ok {
            rows: rows.len(),
            digest: digest(&rows),
            micros: t0.elapsed().as_micros(),
        },
        Err(mpedb::Error::RuntimeBudget { kind: mpedb::BudgetKind::JoinCells, .. }) => {
            Run::OverBudget
        }
        other => Run::Other(format!("{other:?}")),
    }
}

/// The join order this arm actually chose, from EXPLAIN — printed with every
/// measurement so the arm is self-evident in the log, not merely asserted in a
/// test that ran at some other time.
fn order_line(db: &Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}"), &[]) {
        Ok(ExecResult::Explain(t)) => t
            .lines()
            .find(|l| l.trim_start().starts_with("join order:"))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "join order: <none>".to_string()),
        other => format!("<explain failed: {other:?}>"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: mpee_memory <cells|rss|base|compile> <shape> [arg]");
        std::process::exit(2);
    }
    let (mode, shape) = (args[1].as_str(), args[2].as_str());
    let arm = if std::env::var("MPEDB_NO_MPEE").as_deref() == Ok("1") { "off" } else { "on" };
    let arg: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_CAP);

    match mode {
        // Deterministic peak of the live-cell counter — the headline metric.
        // The exact peak of `JoinCells::live`, by bisection on the budget:
        // `charge` trips on `live > budget`, so the SMALLEST budget under which
        // the statement completes IS the peak. `max_join_cells = 0` is the
        // unlimited sentinel, so the search never probes 0. Failing probes are
        // cheap — they abort the moment the product crosses — which is what
        // makes this affordable on the arm whose peak is large.
        "cells" => {
            let cap = arg;
            // Probe the cap first: a shape whose peak is above it is a
            // THRESHOLD result, not a ratio, and is reported as such.
            let (t, sql) = fixture(shape, cap);
            let order = order_line(&t.db, &sql);
            let top = run(&t.db, &sql);
            let (rows, dig) = match top {
                Run::Ok { rows, digest, .. } => (rows, digest),
                Run::OverBudget => {
                    println!(
                        "mode=cells arm={arm} shape={shape} peak_cells=>{cap} \
                         rows=NA digest=NA order={order:?}"
                    );
                    return;
                }
                Run::Other(e) => panic!("{e}"),
            };
            drop(t);
            let (mut lo, mut hi) = (0u64, cap);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                let (t, sql) = fixture(shape, mid);
                match run(&t.db, &sql) {
                    Run::Ok { .. } => hi = mid,
                    Run::OverBudget => lo = mid,
                    Run::Other(e) => panic!("unexpected failure at budget {mid}: {e}"),
                }
            }
            println!(
                "mode=cells arm={arm} shape={shape} peak_cells={hi} \
                 rows={rows} digest={dig:016x} order={order:?}"
            );
        }
        // Peak RSS of the whole process, with the join run once. Corroboration
        // for `cells`: noisy, allocator-quantised, and it includes the mmap'd
        // file pages — subtract `base` for the join's own marginal footprint.
        "rss" => {
            let (t, sql) = fixture(shape, arg);
            let order = order_line(&t.db, &sql);
            let r = run(&t.db, &sql);
            let rss = peak_rss_bytes();
            match r {
                Run::Ok { rows, digest, micros } => println!(
                    "mode=rss arm={arm} shape={shape} rss_bytes={rss} us={micros} \
                     rows={rows} digest={digest:016x} order={order:?}"
                ),
                Run::OverBudget => println!(
                    "mode=rss arm={arm} shape={shape} rss_bytes={rss} us=NA \
                     rows=OVER_BUDGET digest=NA order={order:?}"
                ),
                Run::Other(e) => panic!("{e}"),
            }
        }
        // The arm-independent floor: the same fixture, built and populated,
        // with NO join run. Peak RSS is monotone within a process, so the
        // floor cannot be taken in the same process as the join.
        "base" => {
            // `_t` and not `_`: the fixture must stay ALIVE (and its file
            // mapped) until the reading, or the floor is not the floor.
            let (_t, _sql) = fixture(shape, arg);
            let rss = peak_rss_bytes();
            println!("mode=base arm={arm} shape={shape} rss_bytes={rss} rows=NA digest=NA");
        }
        // The solver's OWN cost: plan compilation, which is where the search
        // runs. Paid once per distinct statement because plans are
        // content-hashed and cached — but "once" still has to be small.
        "compile" => {
            let reps = args.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(50);
            let (t, _) = fixture(shape, 0);
            let mut us = Vec::with_capacity(reps);
            for i in 0..reps {
                // Vary the anchor constant so every rep compiles a DISTINCT
                // plan: an identical statement would be answered by the
                // content-hashed plan cache and measure nothing.
                let sql = if let Some(n) = shape.strip_prefix("chain") {
                    scrambled_sql(n.parse().unwrap(), (i % 10 + 1) as i64)
                } else if shape == "j17" {
                    J17_SQL.replace("a38=9", &format!("a38={}", i % 10 + 1))
                } else {
                    PLAIN_SQL.replace("< 50", &format!("< {}", 50 + i))
                };
                let t0 = std::time::Instant::now();
                let h = t.db.prepare(&sql).unwrap();
                us.push(t0.elapsed().as_nanos() as u64);
                std::hint::black_box(h);
            }
            us.sort_unstable();
            println!(
                "mode=compile arm={arm} shape={shape} reps={reps} \
                 p50_ns={} p10_ns={} p90_ns={} min_ns={} max_ns={}",
                us[reps / 2],
                us[reps / 10],
                us[reps * 9 / 10],
                us[0],
                us[reps - 1]
            );
        }
        other => panic!("unknown mode `{other}`"),
    }
}
