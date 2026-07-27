//! Can a SHARED CI runner resolve an mpedb-vs-sqlite3 ratio, and to what
//! precision? — the instrument probe, not a benchmark.
//!
//! ## Why this exists, and what it deliberately is not
//!
//! `benchmarks/` publishes absolute numbers with the hardware named, because
//! an absolute number without its machine is noise with a decimal point. A
//! GitHub runner is a shared VM with an unknown neighbour, so it can never
//! produce one of those. But that is an argument about ABSOLUTE numbers, and
//! a paired design asks a different question: run the two arms seconds apart
//! on the SAME machine, form the ratio INSIDE a repetition, and most of what
//! makes the host untrustworthy cancels.
//!
//! That is this project's own method (`mpedb-bench --h2h`), and the reason it
//! cannot simply be pointed at Windows is mechanical: `mpedb-bench` pulls
//! PostgreSQL and Turso, whose build scripts do not cross to that target.
//!
//! **So this measures the instrument before anyone trusts it.** Its primary
//! output is not the ratio — it is the ratio's coefficient of variation, and
//! from that the smallest effect the runner can tell from noise. That is what
//! made the Raspberry Pi this project's A/B instrument: CV 1.6 % against the
//! dev box's 9.0 %, measured rather than assumed. If the CV here comes back
//! large, the honest answer to "can CI benchmark this?" is *no, and here is
//! the number*.
//!
//! ## What it measures, and the two things it deliberately does not
//!
//! Contended point-inserts: `--writers` threads, each with its own connection
//! to one file, each inserting into its own key range. Same shape as the
//! published contended cell, so the ratio is comparable in KIND (never in
//! magnitude) to the one in `benchmarks/head-to-head.md`.
//!
//! Non-durable on both sides — mpedb `durability = none`, sqlite
//! `synchronous = OFF` + WAL. Like-for-like (#122), and chosen because the
//! durable classes are exactly the ones a virtualised CI disk cannot speak
//! for: in those, fsync cost dominates, and this disk's fsync is not any real
//! machine's. A durable ratio from here would be precise and meaningless.
//!
//! The other thing pairing does not fix is **core count**. mpedb's advantage
//! in this cell grows with the number of concurrent writers; a 2–4 vCPU runner
//! caps that below where the interesting difference appears. A ratio measured
//! at `--writers 4` on such a host is a lower bound on nothing in particular —
//! it says the mechanism works there, not how well it scales.
//!
//! ## Usage
//!
//! ```text
//! cargo run --release -p mpedb --example runner_noise -- \
//!     --reps 10 --writers 4 --secs 2 --dir /tmp/x
//! ```
//!
//! Arms alternate order every repetition, so a systematic "the first arm runs
//! on a colder cache" effect cancels instead of accumulating into the ratio.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mpedb::{Config, Database, Value};

/// One arm's throughput for one repetition.
struct Arm {
    label: &'static str,
    ops_per_s: f64,
}

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let reps: usize = arg(&args, "--reps", "10").parse().expect("--reps");
    let writers: usize = arg(&args, "--writers", "4").parse().expect("--writers");
    let secs: u64 = arg(&args, "--secs", "2").parse().expect("--secs");
    let dir = PathBuf::from(arg(&args, "--dir", "."));
    std::fs::create_dir_all(&dir).expect("--dir");

    println!(
        "runner-noise probe: {reps} paired reps, {writers} writer threads, {secs}s per arm\n\
         non-durable on both sides (mpedb none / sqlite synchronous=OFF+WAL)\n"
    );

    let mut ratios = Vec::with_capacity(reps);
    let mut mp = Vec::with_capacity(reps);
    let mut sq = Vec::with_capacity(reps);

    for rep in 0..reps {
        // Alternate which arm goes first: a fixed order would fold "the second
        // arm always runs on a warmer page cache" straight into the ratio.
        let (a, b) = if rep % 2 == 0 {
            (run_mpedb(&dir, rep, writers, secs), run_sqlite(&dir, rep, writers, secs))
        } else {
            let s = run_sqlite(&dir, rep, writers, secs);
            (run_mpedb(&dir, rep, writers, secs), s)
        };
        let r = a.ops_per_s / b.ops_per_s;
        println!(
            "  rep {rep:2}: {} {:>10.0} op/s   {} {:>10.0} op/s   ratio {:.3}",
            a.label, a.ops_per_s, b.label, b.ops_per_s, r
        );
        mp.push(a.ops_per_s);
        sq.push(b.ops_per_s);
        ratios.push(r);
    }

    let (rm, rcv) = mean_cv(&ratios);
    let (_, mcv) = mean_cv(&mp);
    let (_, scv) = mean_cv(&sq);
    ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let med = ratios[ratios.len() / 2];

    // The 95% half-width on the MEAN ratio, the number that says what this run
    // can and cannot claim. 1.96/sqrt(n) is the normal approximation; with ten
    // reps it is optimistic by a few percent, which is stated rather than
    // hidden behind a t-table nobody will check.
    let half = 1.96 * rcv / (ratios.len() as f64).sqrt();

    // THE DIAGNOSTIC, and the reason this probe is worth more than a table of
    // ratios. If the two arms fluctuate together — the same neighbour, the same
    // thermal moment — the ratio's CV lands BELOW either arm's, and pairing is
    // doing what it is supposed to. If they fluctuate independently, the ratio's
    // CV is the quadrature sum sqrt(a^2 + b^2), which is strictly LARGER than
    // either. So comparing observed against that sum says, in one number,
    // whether pairing bought anything on this host at this granularity —
    // instead of leaving it to be assumed because the design has a good name.
    let independent = (mcv * mcv + scv * scv).sqrt();
    println!(
        "\nratio (mpedb / sqlite3)  median {med:.3}  mean {rm:.3}\n\
         CV: ratio {:.1}%   mpedb arm {:.1}%   sqlite arm {:.1}%\n\
         quadrature sum if the arms were INDEPENDENT: {:.1}%",
        rcv * 100.0,
        mcv * 100.0,
        scv * 100.0,
        independent * 100.0
    );
    println!(
        "\nWHAT THIS RUN CAN RESOLVE: +/-{:.1}% on the mean ratio (95%, normal\n\
         approximation, {} reps). A difference smaller than that is not a\n\
         difference this host measured.",
        half * 100.0,
        ratios.len()
    );
    // 0.85/1.15 rather than a strict comparison: with ten reps a CV estimate
    // carries roughly +/-25% of itself, so calling a 3% gap a result would be
    // reading the noise in the noise.
    if rcv < independent * 0.85 {
        println!(
            "\nPAIRING IS WORKING: the ratio is quieter than independent arms\n\
             would give, so the two are sharing a disturbance and it cancels.\n\
             The interval above is the honest precision of this host."
        );
    } else if rcv > independent * 1.15 {
        println!(
            "\nPAIRING IS WORSE THAN INDEPENDENT — something is ANTI-correlated\n\
             between the arms (one arm leaving the machine in a state the next\n\
             one inherits). Interleave them more finely before believing a ratio."
        );
    } else {
        println!(
            "\nPAIRING BOUGHT NOTHING: the ratio's spread is what independent\n\
             arms would give. Running them seconds apart is too coarse for the\n\
             disturbance to be shared, so the interval above comes from\n\
             AVERAGING, not from the paired design. More reps still narrow it;\n\
             calling it 'paired' does not."
        );
    }
}

fn mean_cv(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    if n < 2.0 || mean == 0.0 {
        return (mean, 0.0);
    }
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, var.sqrt() / mean)
}

const SCHEMA: &str = "\n[[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\
     \n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
     \n  [[table.column]]\n  name = \"s\"\n  type = \"text\"\n";

fn run_mpedb(dir: &std::path::Path, rep: usize, writers: usize, secs: u64) -> Arm {
    let path = dir.join(format!("noise-{rep}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 256\nmax_readers = 32\n\
         durability = \"none\"\n{SCHEMA}",
        // ESCAPED: a Windows path is TOML escapes otherwise (#159), and this
        // example exists to be RUN on Windows.
        mpedb::toml_escape(&path.display().to_string())
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let ops = timed(writers, secs, |w| {
        let db = &db;
        move |i: i64| {
            let id = w as i64 * 10_000_000 + i;
            db.query(
                "INSERT INTO t (id, s) VALUES (?, ?)",
                &[Value::Int(id), Value::Text(format!("v{id}"))],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        }
    });
    drop(db);
    let _ = std::fs::remove_file(&path);
    Arm { label: "mpedb  ", ops_per_s: ops }
}

fn run_sqlite(dir: &std::path::Path, rep: usize, writers: usize, secs: u64) -> Arm {
    let path = dir.join(format!("noise-{rep}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF;
             CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT) STRICT;",
        )
        .unwrap();
    }
    let ops = timed(writers, secs, |w| {
        // One connection per writer, as the published contended cell does.
        // `busy_timeout` is not a courtesy here: without it every writer but
        // one returns SQLITE_BUSY immediately and the arm measures error
        // handling. That sqlite NEEDS it and mpedb does not is the qualitative
        // half of this comparison, and it does not show up in the ratio.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::from_secs(60)).unwrap();
        conn.execute_batch("PRAGMA synchronous = OFF;").unwrap();
        move |i: i64| {
            let id = w as i64 * 10_000_000 + i;
            conn.execute("INSERT INTO t (id, s) VALUES (?1, ?2)", (id, format!("v{id}")))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    });
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Arm { label: "sqlite3", ops_per_s: ops }
}

/// Run `writers` threads for `secs` seconds and return total ops/s.
///
/// `make` is called ONCE PER THREAD, inside the thread, so per-connection
/// setup is not serialised into the measured window.
fn timed<F, G>(writers: usize, secs: u64, make: F) -> f64
where
    F: Fn(usize) -> G + Sync,
    G: FnMut(i64) -> Result<(), String>,
{
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let total: u64 = std::thread::scope(|s| {
        let stop = &stop;
        let make = &make;
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                s.spawn(move || {
                    let mut op = make(w);
                    let mut n = 0i64;
                    while !stop.load(Ordering::Relaxed) {
                        // A failed insert still costs the work that produced
                        // the failure, so it counts. Silently skipping them
                        // would let an arm "win" by failing faster.
                        let _ = op(n);
                        n += 1;
                    }
                    n as u64
                })
            })
            .collect();
        std::thread::sleep(Duration::from_secs(secs));
        stop.store(true, Ordering::Relaxed);
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });
    total as f64 / start.elapsed().as_secs_f64()
}
