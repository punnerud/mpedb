//! mpedb-bench — honest head-to-head: mpedb vs SQLite vs PostgreSQL.
//!
//! Run: `cargo run --release -p mpedb-bench`
//! Flags: `--out FILE` (report destination; default is derived from the
//! machine, so a second host cannot silently overwrite the first host's
//! numbers), `--quick` (short cells, report not written),
//!        `--only <substr>` (run only engines whose key matches: mpedb,
//!        sqlite, postgres).
//!
//! Progress goes to stderr; the final report goes to stdout and (full runs)
//! to crates/mpedb-bench/RESULTS-<machine>.md.

mod bulk;
mod dur_compare;
mod eng_mpedb;
mod eng_pg;
mod eng_sqlite;
mod engines;
mod report;
mod util;
mod workloads;

use std::path::PathBuf;
use std::time::Instant;

use eng_mpedb::MpedbEngine;
use eng_pg::{PgEngine, PgServer};
use eng_sqlite::{SqliteEngine, SqliteMode};
use engines::Engine;
use report::{CellRow, Report};
use util::{cpu_model, fs_type, host_slug, mem_total, os_release, rustc_version, today_utc, BResult};
use workloads::{run_workload, RunCfg, ALL_WORKLOADS};

const ENGINE_KEYS: [&str; 3] = ["mpedb", "sqlite", "postgres"];

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Disk-backed scratch next to the build output (the workspace `target/`
/// directory lives on the real disk here — verified and printed at run time).
fn disk_scratch(pid: u32) -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("mpedb-bench-scratch-{pid}"))
}

fn pg_version_string() -> String {
    std::process::Command::new("/usr/lib/postgresql/16/bin/postgres")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "postgres binary not found".into())
}

fn engine_label(key: &str) -> String {
    match key {
        "mpedb" => format!("mpedb {}", env!("CARGO_PKG_VERSION")),
        "sqlite" => format!("SQLite {}", rusqlite::version()),
        _ => {
            // "postgres (PostgreSQL) 16.14 (Ubuntu ...)" → "PostgreSQL 16.14"
            let v = pg_version_string();
            v.split_whitespace()
                .nth(2)
                .map_or_else(|| "PostgreSQL 16".into(), |n| format!("PostgreSQL {n}"))
        }
    }
}

fn config_label(key: &str, durable: bool) -> String {
    match (key, durable) {
        ("mpedb", false) => "tmpfs, durability=none".into(),
        ("mpedb", true) => "disk, durability=commit".into(),
        ("sqlite", false) => "tmpfs, sync=OFF+MEMORY".into(),
        ("sqlite", true) => "disk, sync=FULL+WAL".into(),
        (_, false) => "tmpfs, fsync=off+sc=off".into(),
        (_, true) => "disk, fsync=on+sc=on".into(),
    }
}

fn build_engine(
    key: &str,
    durable: bool,
    tmpfs_base: &std::path::Path,
    disk_base: &std::path::Path,
) -> BResult<Box<dyn Engine>> {
    let medium = if durable { disk_base } else { tmpfs_base };
    match key {
        "mpedb" => Ok(Box::new(MpedbEngine::new(
            medium.join("mpedb"),
            if durable { "commit" } else { "none" },
        )?)),
        "sqlite" => Ok(Box::new(SqliteEngine::new(
            medium.join("sqlite"),
            if durable {
                SqliteMode::CommitClass
            } else {
                SqliteMode::NoneClass
            },
        )?)),
        _ => {
            // Data dir follows the medium; the unix SOCKET always sits on
            // tmpfs (short path — 107-byte sun_path limit; carries no data).
            let datadir = medium.join("pgdata");
            let sockdir = tmpfs_base.join(if durable { "pgsock-c" } else { "pgsock-n" });
            let server = PgServer::start(datadir, sockdir, durable)?;
            Ok(Box::new(PgEngine::new(server)?))
        }
    }
}

/// The bulk MB/s section (`--io`): the raw-Rust baseline first, then each engine
/// against it. Both media, both durability classes. Failures are reported, never
/// silently dropped — a missing cell would read as "not measured", not "broken".
fn run_bulk(
    tmpfs_base: &std::path::Path,
    disk_base: &std::path::Path,
    quick: bool,
    only: &Option<String>,
) -> Vec<bulk::BulkRow> {
    let bcfg = if quick { bulk::BulkCfg::quick() } else { bulk::BulkCfg::full() };
    let mut rows: Vec<bulk::BulkRow> = Vec::new();
    eprintln!(
        "=== bulk MB/s ({:.0} MiB logical payload per cell, {} B values) ===",
        bcfg.total_mib, bcfg.value_bytes
    );

    for (durable, class, medium) in [
        (false, "none-class", tmpfs_base),
        (true, "commit-class", disk_base),
    ] {
        // the baseline first: everything below is read as a fraction of it
        eprint!("  {:<28} ", format!("raw std::fs ({class})"));
        match bulk::raw_baseline(&medium.join("raw"), &bcfg, durable) {
            Ok((w, r)) => {
                eprintln!("write {w:>8.1} MiB/s  read {r:>8.1} MiB/s");
                rows.push(bulk::BulkRow {
                    engine: "raw std::fs (baseline)".into(),
                    config: if durable {
                        "write + fsync barrier".into()
                    } else {
                        "write, no fsync".into()
                    },
                    class,
                    logical_mib: bcfg.total_mib as f64,
                    write_mibs: w,
                    scan_mibs: r,
                    is_baseline: true,
                });
            }
            Err(e) => eprintln!("FAILED: {e}"),
        }

        let want = |k: &str| only.as_ref().is_none_or(|f| k.contains(f.as_str()));
        if want("mpedb") {
            let d = if durable { "commit" } else { "none" };
            eprint!("  {:<28} ", format!("mpedb durability={d}"));
            match bulk::mpedb_bulk(&medium.join("bulk-mpedb"), d, &bcfg) {
                Ok(r) => {
                    eprintln!(
                        "write {:>8.1} MiB/s  scan {:>8.1} MiB/s",
                        r.write_mibs, r.scan_mibs
                    );
                    rows.push(r);
                }
                Err(e) => eprintln!("FAILED: {e}"),
            }
        }
        if want("sqlite") {
            eprint!("  {:<28} ", "sqlite");
            match bulk::sqlite_bulk(&medium.join("bulk-sqlite"), durable, &bcfg) {
                Ok(r) => {
                    eprintln!(
                        "write {:>8.1} MiB/s  scan {:>8.1} MiB/s",
                        r.write_mibs, r.scan_mibs
                    );
                    rows.push(r);
                }
                Err(e) => eprintln!("FAILED: {e}"),
            }
        }
        if want("postgres") {
            eprint!("  {:<28} ", "postgres");
            let datadir = medium.join("bulk-pgdata");
            let sockdir = tmpfs_base.join(if durable { "bpgsock-c" } else { "bpgsock-n" });
            match PgServer::start(datadir, sockdir, durable).and_then(|srv| {
                let mut c = postgres::Client::connect(&srv.conn_str(), postgres::NoTls)?;
                let label = if durable {
                    "disk, fsync=on+sc=on"
                } else {
                    "tmpfs, fsync=off+sc=off"
                };
                let r = bulk::pg_bulk(&mut c, label, class, &bcfg);
                drop(c);
                // keep the server alive until the client is done, then stop it
                drop(srv);
                r
            }) {
                Ok(r) => {
                    eprintln!(
                        "write {:>8.1} MiB/s  scan {:>8.1} MiB/s",
                        r.write_mibs, r.scan_mibs
                    );
                    rows.push(r);
                }
                Err(e) => eprintln!("FAILED: {e}"),
            }
        }
    }
    rows
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    // Bulk MB/s is off by default: it moves hundreds of MB per cell, which is
    // minutes of extra runtime and a lot of disk churn for a suite most runs
    // want for its ops/s cells.
    let io = args.iter().any(|a| a == "--io");
    let only: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--only")
        .map(|w| w[1].clone());
    // The RAM-backed medium for none-class cells. Linux has /dev/shm; macOS has
    // no tmpfs, so a run there must point this at a RAM disk (see README) —
    // otherwise "none-class" would silently measure a page-cached APFS file and
    // would not be comparable to a Linux run.
    let tmpfs_arg: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--tmpfs")
        .map(|w| w[1].clone());
    // Where to write the report. RESULTS.md is a SINGLE-MACHINE file — it says
    // so in its own first line — and this used to overwrite it unconditionally.
    // So running the suite on a second machine did not add a second set of
    // numbers, it DELETED the first, and the loss was silent. A second host
    // needs its own file.
    let out_arg: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| w[1].clone());
    for (i, a) in args.iter().enumerate() {
        let known = a == "--quick"
            || a == "--io"
            || a == "--only"
            || a == "--tmpfs"
            || a == "--out"
            || (i > 0
                && (args[i - 1] == "--only"
                    || args[i - 1] == "--tmpfs"
                    || args[i - 1] == "--out"));
        if !known {
            eprintln!(
                "usage: mpedb-bench [--quick] [--io] [--only mpedb|sqlite|postgres] \
                 [--tmpfs DIR] [--out FILE]"
            );
            std::process::exit(2);
        }
    }
    let cfg = if quick { RunCfg::quick() } else { RunCfg::full() };

    let pid = std::process::id();
    let tmpfs_root = tmpfs_arg.unwrap_or_else(|| "/dev/shm".to_string());
    let tmpfs_base = PathBuf::from(format!("{tmpfs_root}/mpedb-bench-{pid}"));
    let disk_base = disk_scratch(pid);
    for d in [&tmpfs_base, &disk_base] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("cannot create scratch dir {}: {e}", d.display());
            std::process::exit(1);
        }
    }
    let _guards = (DirGuard(tmpfs_base.clone()), DirGuard(disk_base.clone()));

    let tmpfs_ty = fs_type(&tmpfs_base);
    let disk_ty = fs_type(&disk_base);
    if tmpfs_ty == "?" {
        // No /proc/mounts (macOS): we cannot verify the medium. Say so rather
        // than claim "expected tmpfs" — a silently page-cached file would make
        // none-class numbers incomparable to a Linux run.
        eprintln!(
            "NOTE: cannot verify the filesystem of {} on this platform — ensure it is \
             RAM-backed (macOS: `--tmpfs /Volumes/<ramdisk>`), or none-class numbers are \
             not comparable to a tmpfs run.",
            tmpfs_base.display()
        );
    } else if tmpfs_ty != "tmpfs" {
        eprintln!("WARNING: {} is {tmpfs_ty}, expected tmpfs", tmpfs_base.display());
    }
    if disk_ty == "tmpfs" {
        eprintln!(
            "WARNING: disk scratch {} is on tmpfs — 'disk' cells are not disk-backed!",
            disk_base.display()
        );
    }

    let info_lines = vec![
        format!("Date: {} (UTC)", today_utc()),
        format!(
            "CPU: {} — {} cores; RAM: {}; OS: {}",
            cpu_model(),
            std::thread::available_parallelism().map_or(0, |n| n.get()),
            mem_total(),
            os_release()
        ),
        format!(
            "Media: tmpfs = {} ({tmpfs_ty}); disk = {} ({disk_ty})",
            tmpfs_base.display(),
            disk_base.display()
        ),
        format!(
            "mpedb {} (this workspace, embedded, one shared Database handle across threads)",
            env!("CARGO_PKG_VERSION")
        ),
        format!(
            "SQLite {} (rusqlite 0.31 `bundled` — system libsqlite3 has no dev \
             symlink/header, so linking it fails; STRICT table, one connection per thread)",
            rusqlite::version()
        ),
        format!(
            "{} (throwaway cluster: initdb --auth=trust --locale=C, pg_ctl, unix socket, \
             `postgres` crate 0.19, one client per thread)",
            pg_version_string()
        ),
        rustc_version() + " (--release, lto=thin)",
        format!(
            "Workload sizing: point cells self-calibrate to ~{:.1} s; timed cells {:.1} s; \
             {} seeded rows per cell",
            cfg.target_s, cfg.timed_s, cfg.seed_rows
        ),
    ];

    let mut cells: Vec<CellRow> = Vec::new();
    for durable in [false, true] {
        let class = if durable { "commit-class" } else { "none-class" };
        for key in ENGINE_KEYS {
            if let Some(f) = &only {
                if !key.contains(f.as_str()) {
                    continue;
                }
            }
            let elabel = engine_label(key);
            let clabel = config_label(key, durable);
            eprintln!("=== {elabel} — {clabel} ({class}) ===");
            match build_engine(key, durable, &tmpfs_base, &disk_base) {
                Err(e) => {
                    eprintln!("  engine unavailable: {e}");
                    for w in ALL_WORKLOADS {
                        cells.push(CellRow {
                            engine: elabel.clone(),
                            config: clabel.clone(),
                            class,
                            workload: w,
                            outcome: Err(format!("engine unavailable: {e}")),
                        });
                    }
                }
                Ok(mut engine) => {
                    for w in ALL_WORKLOADS {
                        eprint!("  {:<18} ", w.name());
                        let t = Instant::now();
                        let outcome = run_workload(engine.as_mut(), w, &cfg)
                            .map_err(|e| e.to_string());
                        match &outcome {
                            Ok(_) => eprintln!("done in {:>5.1} s", t.elapsed().as_secs_f64()),
                            Err(e) => eprintln!("FAILED: {e}"),
                        }
                        cells.push(CellRow {
                            engine: elabel.clone(),
                            config: clabel.clone(),
                            class,
                            workload: w,
                            outcome,
                        });
                    }
                }
            }
        }
    }

    let bulk_rows = if io {
        run_bulk(&tmpfs_base, &disk_base, quick, &only)
    } else {
        Vec::new()
    };

    // Focused single-client durable point-insert, by durability class (§5.4).
    // Its own engine instances on real disk; PostgreSQL sockets on tmpfs.
    eprintln!("=== single-client durable point-insert, by class (§5.4) ===");
    let labels = dur_compare::Labels {
        mpedb: engine_label("mpedb"),
        sqlite: engine_label("sqlite"),
        pg: engine_label("postgres"),
    };
    let dur_rows = dur_compare::run(&disk_base, &tmpfs_base, &cfg, &only, &labels);

    let mut extra_caveats = Vec::new();
    let retries = eng_mpedb::spurious_corrupt_retries();
    if retries > 0 {
        extra_caveats.push(format!(
            "mpedb spurious-Corrupt reader retries observed THIS run: {retries} \
             (see the engine-race caveat above; retry time is included in read latency)"
        ));
    }
    let report = Report {
        info_lines,
        cells,
        dur_rows,
        quick_mode: quick,
        extra_caveats,
        bulk_rows,
    };
    println!("{}", report.to_text());

    if quick {
        eprintln!("(quick mode: no report written)");
    } else {
        let path = match &out_arg {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("RESULTS-{}.md", host_slug())),
        };
        match std::fs::write(&path, report.to_markdown()) {
            Ok(()) => eprintln!("wrote {}", path.display()),
            Err(e) => {
                eprintln!("failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
}
