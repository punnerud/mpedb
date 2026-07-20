//! The durable head-to-head, **paired and interleaved** (#122).
//!
//! The full suite runs each engine's cells back to back, which is fine for a
//! matrix but not for a comparison that a reader will quote as a ratio: the
//! arms are minutes apart and this box drifts. This mode instead builds every
//! durable arm ONCE and then walks them round-robin, `reps` times, so each
//! repetition contains one measurement of every arm within seconds of the
//! others. Ratios are formed **inside a repetition** and the spread across
//! repetitions is reported next to the median — the method BENCHMARKS.md
//! requires of anything it publishes.
//!
//! The arms, all durable-on-ack, all on the same real disk:
//!
//! - **mpedb `durability = commit`** — the mapped-page barrier: msync the
//!   dirty span, then msync the meta. design/DESIGN.md §4.1's two-flush floor.
//! - **mpedb `durability = wal`** — the log: one `pwrite` + one `fdatasync`.
//!   This is the like-for-like comparand for the other two engines, and it was
//!   missing from the published table.
//! - **SQLite `synchronous = FULL` + WAL** — log-based, one flush.
//! - **PostgreSQL `synchronous_commit = on`** — log-based, one flush.
//!
//! Each point-insert cell is also bracketed by the kernel's **device
//! flush counters** (`/proc/diskstats`), so the table reports barriers per
//! commit and microseconds per barrier next to the latency. That is what
//! separates "this engine issues more flushes" from "this engine's flushes
//! are slower", which no engine-level timer can do.

use std::path::Path;

use crate::eng_mpedb::MpedbEngine;
use crate::eng_pg::{PgEngine, PgServer};
use crate::eng_sqlite::{SqliteEngine, SqliteMode};
use crate::engines::Engine;
use crate::util::{block_device_name, block_device_of, flush_stat, median, BResult, FlushStat};
use crate::workloads::{measure_contended, measure_single_insert, RunCfg};

/// Pre-reserved mpedb file size for this mode. Two mpedb arms are open at
/// once; the suite's 1 GiB default would put 2 GiB of mapping under a 3 GB
/// `ulimit -v` before anything else is allocated.
const SIZE_MB: u64 = 256;

struct Arm {
    /// Short label used in every table this mode prints.
    label: &'static str,
    engine: Box<dyn Engine>,
}

/// One arm's numbers from one repetition.
#[derive(Clone, Copy, Default)]
struct Sample {
    ops_s: f64,
    p50_us: f64,
    p99_us: f64,
    /// device cache-flush requests per committed insert
    flush_per_op: f64,
    /// mean microseconds the device spent per flush request
    us_per_flush: f64,
}

fn build_arms(disk_base: &Path, tmpfs_base: &Path) -> BResult<Vec<Arm>> {
    let mut arms: Vec<Arm> = Vec::new();
    arms.push(Arm {
        label: "mpedb commit",
        engine: Box::new(MpedbEngine::new_sized(
            disk_base.join("h2h-mpedb-commit"),
            "commit",
            SIZE_MB,
        )?),
    });
    arms.push(Arm {
        label: "mpedb wal",
        engine: Box::new(MpedbEngine::new_sized(
            disk_base.join("h2h-mpedb-wal"),
            "wal",
            SIZE_MB,
        )?),
    });
    arms.push(Arm {
        label: "SQLite FULL+WAL",
        engine: Box::new(SqliteEngine::new(
            disk_base.join("h2h-sqlite"),
            SqliteMode::CommitClass,
        )?),
    });
    let srv = PgServer::start_general(
        disk_base.join("h2h-pgdata"),
        tmpfs_base.join("h2h-pgsock"),
        "on",
        "on",
    )?;
    arms.push(Arm {
        label: "PostgreSQL sc=on",
        engine: Box::new(PgEngine::new(srv)?),
    });
    Ok(arms)
}

/// Run one point-insert cell with the device flush counters bracketed around
/// it. The counters are host-wide, so this mode requires an otherwise idle
/// disk — which is also what any absolute number here requires.
fn point_insert_sample(arm: &mut Arm, cfg: &RunCfg, dev: Option<(u32, u32)>) -> BResult<Sample> {
    arm.engine.reset_and_seed(cfg.seed_rows)?;
    let mut conn = arm.engine.conn()?;
    let before = dev.and_then(flush_stat);
    let s = measure_single_insert(&mut *conn, cfg)?;
    let after = dev.and_then(flush_stat);
    let d = match (before, after) {
        (Some(b), Some(a)) => a.since(b),
        _ => FlushStat::default(),
    };
    let ops = s.ops.max(1) as f64;
    Ok(Sample {
        ops_s: s.ops_per_s(),
        p50_us: s.p50_us as f64,
        p99_us: s.p99_us as f64,
        flush_per_op: d.ios as f64 / ops,
        us_per_flush: if d.ios > 0 {
            d.ticks_ms as f64 * 1000.0 / d.ios as f64
        } else {
            0.0
        },
    })
}

fn fmt_spread(v: &[f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("{lo:.0}-{hi:.0}")
}

fn print_cell(title: &str, labels: &[&str], per_arm: &[Vec<f64>], unit: &str, ref_ix: usize) {
    println!("\n{title}");
    println!(
        "  {:<18} {:>10} {:>14} {:>10} {:>16}",
        "arm",
        format!("median {unit}"),
        "spread (min-max)",
        "n",
        "median x mpedb commit"
    );
    let base: Vec<f64> = per_arm[ref_ix].clone();
    for (i, label) in labels.iter().enumerate() {
        let v = &per_arm[i];
        // Paired ratio: formed INSIDE each repetition, then the median of the
        // ratios — not the ratio of the medians, which would let drift between
        // repetitions leak into the comparison.
        let ratios: Vec<f64> = v
            .iter()
            .zip(base.iter())
            .filter(|(_, b)| **b > 0.0)
            .map(|(x, b)| x / b)
            .collect();
        let ratio = format!(
            "{:.2}x [{:.2}-{:.2}]",
            median(&ratios),
            ratios.iter().copied().fold(f64::INFINITY, f64::min),
            ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        );
        println!(
            "  {:<18} {:>10.0} {:>14} {:>10} {:>16}",
            label,
            median(v),
            fmt_spread(v),
            v.len(),
            ratio
        );
    }
}

/// Run `reps` interleaved repetitions and print the tables. `disk_base` must
/// be real disk.
pub fn run(disk_base: &Path, tmpfs_base: &Path, cfg: &RunCfg, reps: usize) -> BResult<()> {
    let dev = block_device_of(disk_base);
    match dev.and_then(block_device_name) {
        Some(n) => eprintln!(
            "[h2h] disk {} on /dev/{n}; device flush counters from /proc/diskstats",
            disk_base.display()
        ),
        None => eprintln!(
            "[h2h] disk {}; NO device flush counters on this platform",
            disk_base.display()
        ),
    }
    let mut arms = build_arms(disk_base, tmpfs_base)?;
    let labels: Vec<&str> = arms.iter().map(|a| a.label).collect();

    let n = arms.len();
    let mut ops = vec![vec![]; n];
    let mut p50 = vec![vec![]; n];
    let mut p99 = vec![vec![]; n];
    let mut fpo = vec![vec![]; n];
    let mut upf = vec![vec![]; n];
    let mut cont = vec![vec![]; n];

    for rep in 0..reps {
        for (i, arm) in arms.iter_mut().enumerate() {
            eprint!("[h2h] rep {}/{reps} point-insert {:<18} ", rep + 1, arm.label);
            let s = point_insert_sample(arm, cfg, dev)?;
            eprintln!(
                "{:>8.0} ops/s  p50 {:>6.0} p99 {:>7.0} us  {:>5.2} flush/op  {:>6.0} us/flush",
                s.ops_s, s.p50_us, s.p99_us, s.flush_per_op, s.us_per_flush
            );
            ops[i].push(s.ops_s);
            p50[i].push(s.p50_us);
            p99[i].push(s.p99_us);
            fpo[i].push(s.flush_per_op);
            upf[i].push(s.us_per_flush);
        }
        for (i, arm) in arms.iter_mut().enumerate() {
            eprint!("[h2h] rep {}/{reps} contended-4  {:<18} ", rep + 1, arm.label);
            arm.engine.reset_and_seed(cfg.seed_rows)?;
            let s = measure_contended(&*arm.engine, cfg)?;
            eprintln!("{:>8.0} ops/s", s.ops_per_s());
            cont[i].push(s.ops_per_s());
        }
    }

    println!("\n=== durable head-to-head, paired and interleaved ({reps} repetitions) ===");
    print_cell("point-insert, 1 client (ops/s)", &labels, &ops, "ops/s", 0);
    print_cell("point-insert, 1 client (p50 us)", &labels, &p50, "us", 0);
    print_cell("point-insert, 1 client (p99 us)", &labels, &p99, "us", 0);
    // ops/s is 1/MEAN latency; p50 is the typical commit. Printing both, plus
    // the ratio between them, separates "every commit is slower" from "the
    // tail is heavier" — two different defects that a single ops/s cell
    // reports identically.
    println!("\n  mean latency (= 1/ops per s) vs p50, medians across reps");
    println!("  {:<18} {:>10} {:>10} {:>10}", "arm", "mean us", "p50 us", "mean/p50");
    for (i, label) in labels.iter().enumerate() {
        let mean: Vec<f64> = ops[i].iter().map(|r| if *r > 0.0 { 1e6 / r } else { 0.0 }).collect();
        let (m, p) = (median(&mean), median(&p50[i]));
        println!(
            "  {:<18} {:>10.0} {:>10.0} {:>10.2}",
            label,
            m,
            p,
            if p > 0.0 { m / p } else { 0.0 }
        );
    }
    print_cell("contended writes, 4 threads (ops/s)", &labels, &cont, "ops/s", 0);
    println!("\ndevice cache flushes (kernel counters, per committed insert)");
    println!("  {:<18} {:>14} {:>16}", "arm", "median flush/op", "median us/flush");
    for (i, label) in labels.iter().enumerate() {
        println!(
            "  {:<18} {:>14.2} {:>16.0}",
            label,
            median(&fpo[i]),
            median(&upf[i])
        );
    }
    Ok(())
}
