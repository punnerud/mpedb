//! Rendering: the same report as an aligned text table (stdout) and as
//! markdown (RESULTS.md). The honesty notes are part of BOTH outputs by
//! construction.

use crate::dur_compare::{DurRow, DEFERRED, DURABLE_ON_ACK};
use crate::util::LatStats;
use crate::workloads::{CellResult, Workload, ALL_WORKLOADS};

/// Honesty requirements — printed in the output header and in RESULTS.md.
pub const HONESTY_NOTES: &[&str] = &[
    "Class comparisons ONLY. \"none-class\" = no fsync guarantees (data may be lost on OS \
     crash / power loss); \"commit-class\" = durable on ack. Never compare a none-class \
     number with a commit-class number.",
    "PostgreSQL has no true none-mode: it ALWAYS writes WAL; fsync=off/synchronous_commit=off \
     only stop waiting for it. Its none-class cells therefore do strictly more work than the \
     embedded engines' none-class cells. That asymmetry is inherent, and reported as such.",
    "SQLite's none-class (journal_mode=MEMORY, synchronous=OFF) also gives up rollback \
     safety: a crash mid-write can corrupt the database file. mpedb durability=none remains \
     process-crash-safe (COW pages + atomic meta flip); reboot durability is absent in both. \
     The none-class cells are comparable on durability, NOT identical on crash safety.",
    "mpedb and SQLite are embedded — an operation is a function call in the same process. \
     PostgreSQL is client/server — every operation pays a unix-socket round-trip plus \
     protocol encode/decode. A real architectural difference (not benchmark unfairness), \
     and it dominates point-op latency on this 2-core machine.",
    "Single machine, 2 cores, 7.6 GiB RAM; every engine built/run with --release \
     (debug assertions off). Contended cells intentionally run more threads than cores.",
    "No cherry-picking: every cell is reported, including those mpedb loses.",
];

pub struct CellRow {
    pub engine: String,
    /// e.g. "tmpfs, durability=none" / "disk, sync=FULL+WAL"
    pub config: String,
    pub class: &'static str,
    pub workload: Workload,
    pub outcome: Result<CellResult, String>,
}

pub struct Report {
    /// Machine / versions / date bullet lines (no leading dash).
    pub info_lines: Vec<String>,
    pub cells: Vec<CellRow>,
    /// Single-client durable point-insert, by durability class (§5.4).
    pub dur_rows: Vec<DurRow>,
    pub quick_mode: bool,
    /// Run-specific caveat lines appended after the static caveats
    /// (e.g. the spurious-Corrupt retry count observed this run).
    pub extra_caveats: Vec<String>,
}

fn fmt_rate(s: &LatStats) -> String {
    format!("{:.0}", s.ops_per_s())
}

fn stat_cols(s: &LatStats) -> [String; 4] {
    [
        s.ops.to_string(),
        fmt_rate(s),
        s.p50_us.to_string(),
        s.p99_us.to_string(),
    ]
}

/// Render one table. `md` selects markdown pipe syntax vs aligned text.
fn render_table(headers: &[&str], rows: &[Vec<String>], md: bool) -> String {
    let mut out = String::new();
    if md {
        out.push_str(&format!("| {} |\n", headers.join(" | ")));
        out.push_str(&format!(
            "|{}\n",
            headers.iter().map(|_| "---|").collect::<String>()
        ));
        for r in rows {
            out.push_str(&format!("| {} |\n", r.join(" | ")));
        }
        return out;
    }
    let cols = headers.len();
    let mut width = vec![0usize; cols];
    for (i, h) in headers.iter().enumerate() {
        width[i] = h.len();
    }
    for r in rows {
        for (i, v) in r.iter().enumerate() {
            width[i] = width[i].max(v.len());
        }
    }
    let line = |vals: &[String]| -> String {
        let mut s = String::new();
        for (i, v) in vals.iter().enumerate() {
            // left-align the first two (labels), right-align numbers
            if i < 2 {
                s.push_str(&format!("{:<w$}  ", v, w = width[i]));
            } else {
                s.push_str(&format!("{:>w$}  ", v, w = width[i]));
            }
        }
        s.trim_end().to_string()
    };
    let hdr: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    out.push_str(&line(&hdr));
    out.push('\n');
    out.push_str(&"-".repeat(width.iter().sum::<usize>() + 2 * (cols - 1)));
    out.push('\n');
    for r in rows {
        out.push_str(&line(r));
        out.push('\n');
    }
    out
}

impl Report {
    fn workload_section(&self, w: Workload, md: bool) -> String {
        let mut out = String::new();
        let title = format!("{} — {}", w.name(), w.describe());
        if md {
            out.push_str(&format!("### {title}\n\n"));
        } else {
            out.push_str(&format!("## {title}\n\n"));
        }

        let cells: Vec<&CellRow> = self.cells.iter().filter(|c| c.workload == w).collect();
        let mut failures: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();

        if w == Workload::ReadWhileWrite {
            let headers = [
                "engine",
                "config (class)",
                "read ops/s",
                "r p50 µs",
                "r p99 µs",
                "write ops/s",
                "w p50 µs",
                "w p99 µs",
            ];
            for c in &cells {
                match &c.outcome {
                    Ok(res) => {
                        let (r, wr) = (res.reads.as_ref(), res.writes.as_ref());
                        let g = |o: Option<&LatStats>, f: &dyn Fn(&LatStats) -> String| {
                            o.map_or_else(|| "-".into(), f)
                        };
                        rows.push(vec![
                            c.engine.clone(),
                            format!("{} ({})", c.config, c.class),
                            g(r, &fmt_rate),
                            g(r, &|s| s.p50_us.to_string()),
                            g(r, &|s| s.p99_us.to_string()),
                            g(wr, &fmt_rate),
                            g(wr, &|s| s.p50_us.to_string()),
                            g(wr, &|s| s.p99_us.to_string()),
                        ]);
                    }
                    Err(e) => failures.push(format!("{} ({}): {e}", c.engine, c.config)),
                }
            }
            out.push_str(&render_table(&headers, &rows, md));
        } else {
            let headers = [
                "engine",
                "config (class)",
                "ops",
                "ops/s",
                "p50 µs",
                "p99 µs",
            ];
            for c in &cells {
                match &c.outcome {
                    Ok(res) => {
                        let s = res
                            .writes
                            .as_ref()
                            .or(res.reads.as_ref())
                            .expect("point cell has exactly one stat");
                        let [ops, rate, p50, p99] = stat_cols(s);
                        rows.push(vec![
                            c.engine.clone(),
                            format!("{} ({})", c.config, c.class),
                            ops,
                            rate,
                            p50,
                            p99,
                        ]);
                    }
                    Err(e) => failures.push(format!("{} ({}): {e}", c.engine, c.config)),
                }
            }
            out.push_str(&render_table(&headers, &rows, md));
        }
        for f in &failures {
            out.push_str(&format!("\nFAILED — {f}\n"));
        }
        out.push('\n');
        out
    }

    /// The by-class single-client durable point-insert comparison (§5.4).
    /// Two tables — compare WITHIN a class only, never across.
    fn dur_section(&self, md: bool) -> String {
        if self.dur_rows.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str(if md {
            "## Single-client durable point-insert — by durability class\n\n"
        } else {
            "# Single-client durable point-insert — by durability class\n\n"
        });
        out.push_str(
            "One sequential writer, real disk. This is the head-to-head for \
             durable single-client INSERTs. Compare WITHIN a class only — the two \
             classes make different promises.\n\n",
        );
        for (class, blurb) in [
            (
                DURABLE_ON_ACK,
                "a commit is power-loss-durable the instant it returns — one fsync per \
                 commit (mpedb wal / SQLite synchronous=FULL / PostgreSQL sc=on). Batching \
                 amortizes that fsync across many rows.",
            ),
            (
                DEFERRED,
                "crash-consistent immediately, but power loss may lose a bounded recent \
                 window — fsync is coalesced, not per commit (mpedb async / SQLite \
                 synchronous=NORMAL / PostgreSQL sc=off). Weaker than durable-on-ack; never \
                 call it durable-on-ack.",
            ),
        ] {
            out.push_str(&if md {
                format!("### {class} — {blurb}\n\n")
            } else {
                format!("## {class}\n  ({blurb})\n\n")
            });
            let headers = ["engine", "config", "note", "ops/s", "p50 µs", "p99 µs"];
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            for r in self.dur_rows.iter().filter(|r| r.class == class) {
                match &r.outcome {
                    Ok(s) => rows.push(vec![
                        r.engine.clone(),
                        r.config.clone(),
                        r.note.clone(),
                        fmt_rate(s),
                        s.p50_us.to_string(),
                        s.p99_us.to_string(),
                    ]),
                    Err(e) => failures.push(format!("{} ({}): {e}", r.engine, r.config)),
                }
            }
            out.push_str(&render_table(&headers, &rows, md));
            for f in &failures {
                out.push_str(&format!("\nFAILED — {f}\n"));
            }
            out.push('\n');
        }
        out
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("mpedb-bench — mpedb vs SQLite vs PostgreSQL (this machine, head-to-head)\n");
        out.push_str("==========================================================================\n\n");
        if self.quick_mode {
            out.push_str("!! QUICK MODE: shortened cells; numbers are NOT for comparison !!\n\n");
        }
        for l in &self.info_lines {
            out.push_str(&format!("  {l}\n"));
        }
        out.push_str("\nHonesty notes (read before comparing anything):\n");
        for n in HONESTY_NOTES {
            out.push_str(&format!("  * {n}\n"));
        }
        out.push('\n');
        for w in ALL_WORKLOADS {
            out.push_str(&self.workload_section(w, false));
        }
        out.push_str(&self.dur_section(false));
        out.push_str("Caveats:\n");
        for l in CAVEATS_MD.lines() {
            out.push_str(&format!("  {l}\n"));
        }
        for c in &self.extra_caveats {
            out.push_str(&format!("  - {c}\n"));
        }
        out
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# mpedb vs SQLite vs PostgreSQL — head-to-head on this machine\n\n");
        if self.quick_mode {
            out.push_str("**QUICK MODE — shortened cells; numbers are NOT for comparison.**\n\n");
        }
        out.push_str(
            "Generated by `cargo run --release -p mpedb-bench`. \
             All cells measured on the machine below in one run.\n\n",
        );
        out.push_str("## Machine, versions, date\n\n");
        for l in &self.info_lines {
            out.push_str(&format!("- {l}\n"));
        }
        out.push_str("\n## Honesty notes — read before comparing anything\n\n");
        for n in HONESTY_NOTES {
            out.push_str(&format!("- {n}\n"));
        }
        out.push_str("\n## Results\n\n");
        out.push_str(
            "Latencies are per-operation microseconds measured around each call \
             (p50/p99 over every operation in the cell). ops/s = ops / wall time. \
             Point cells are self-calibrated to run ~2-10 s; timed cells run a fixed \
             5 s. Each cell starts from a freshly seeded 50,000-row table.\n\n",
        );
        for w in ALL_WORKLOADS {
            out.push_str(&self.workload_section(w, true));
        }
        out.push_str(&self.dur_section(true));
        out.push_str("## Caveats\n\n");
        out.push_str(CAVEATS_MD);
        for c in &self.extra_caveats {
            out.push_str(&format!("- {c}\n"));
        }
        out
    }
}

pub const CAVEATS_MD: &str = "\
- One run, one machine; no confidence intervals. Treat small (<20%) differences as noise.
- HOST LOAD DOMINATES ABSOLUTE NUMBERS — never read a run-to-run delta as a code \
change without checking the unchanged engines. Measured on this shared 2-core VM: \
between two runs on 2026-07-14 (10:40 and 11:56) every cell rose 15-84% — mpedb \
none-class point-insert 165k->302k ops/s — but SQLite (+19% median) and PostgreSQL \
(+35% median) rose by the same order with byte-identical binaries, so it was host \
load, not code. SQLite and PostgreSQL are the control group in every run: if mpedb \
moves and they do not, that is a code signal; if all three move together, it is the \
host. Compare RATIOS across runs, absolutes only within one run.
- The 2-core box runs benchmark threads, the engine, and (for PostgreSQL) server \
processes simultaneously; contended cells oversubscribe the CPU on purpose.
- SQLite runs the bundled 3.45.0 build (the system libsqlite3 lacks the dev symlink \
and header needed to link it). Compiled by the same rustc/cc toolchain as everything else.
- SQLite none-class uses a rollback journal in memory, so concurrent access serializes \
at the database level (readers block the writer and vice versa; 60 s busy_timeout). \
WAL mode (commit-class) allows readers concurrent with the writer.
- PostgreSQL none-class keeps its WAL and full client/server stack (see honesty notes); \
its data dir sits on /dev/shm. The unix socket for BOTH PostgreSQL configs lives on \
/dev/shm (sockets carry no data; the datadir location is what differs).
- mpedb commit-class engages the intent-ring group commit only under contention \
(DESIGN.md §5.3); single-client durable inserts pay one msync each, serialized.
- DURABILITY CLASS TABLE (§5.4): the durable-on-ack class (mpedb wal / SQLite FULL / \
PostgreSQL sc=on) acks only after the commit is power-loss-durable. The \
crash-consistent-deferred class (mpedb async / SQLite NORMAL / PostgreSQL sc=off) acks \
BEFORE the fsync — always crash-consistent (a torn tail truncates whole commits), but a \
power failure may lose a bounded recent window of acked commits. NEVER compare a \
deferred number against a durable-on-ack number. mpedb `async` bounds the window by a \
flush interval (default 10 ms, MPEDB_WAL_FLUSH_MS); PostgreSQL sc=off by \
wal_writer_delay; SQLite NORMAL flushes at checkpoint.
- mpedb wal records are LEAN (DESIGN.md §5.4.1): only the touched COW pages are logged, \
and each B+tree node's unused free space is elided (stored as prefix+suffix, zero-filled \
on replay — proven byte-safe against btree.rs). MPEDB_WAL_FULL_PAGES=1 disables it for \
A/B; lean cut the per-commit fdatasync payload and measured ~1.15-1.2x single-client \
insert throughput on this host.
- Seeding is batched (one transaction / COPY) and unmeasured; measured ops always go \
through prepared statements / precompiled plans.
- KNOWN mpedb ENGINE RACE found by this benchmark (durability=commit only): a reader \
that loads the `durable_txn` gate and is then descheduled while two durable commits \
land gets a spurious `Corrupt(\"no valid meta page\")` from `newest_meta` — both \
checksum-valid meta slots are newer than its stale gate. The database is not corrupt; \
re-reading succeeds. The benchmark adapter retries such reads (bounded at 100), counts \
them, and INCLUDES the retry time in the measured latency. Fix belongs in \
`mpedb-core::shm::newest_meta` (reload the monotone gate and retry).
";
