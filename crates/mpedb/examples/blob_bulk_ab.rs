//! Paired A/B for the extent path (DESIGN-BLOBEXTENT §13.5) — the 4 KiB bulk
//! cell, threshold OFF vs ON, same binary, ABAB interleave. On the dev box
//! this is DIRECTIONAL (CV ~9%); the verdict cell runs paired on the Pi and
//! absolute on M3/EPYC.
//!
//! `cargo run --release -p mpedb --example blob_bulk_ab [reps] [mib]`

use mpedb::{params, Database};
use std::time::Instant;


const BATCH: usize = 256;

fn run_arm(dir: &str, tag: &str, threshold: bool, mib: usize, blob: usize) -> f64 {
    let path = format!("{dir}/ab-{tag}.mpedb");
    let cfg_path = format!("{dir}/ab-{tag}.toml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let extent = if threshold { "extent_threshold_kb = 2\n" } else { "" };
    std::fs::write(
        &cfg_path,
        format!(
            "[database]\npath = \"{path}\"\nsize_mb = {}\ndurability = \"none\"\n{extent}\n\
             [[table]]\nname = \"blobs\"\nprimary_key = [\"id\"]\n\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\n  \
             [[table.column]]\n  name = \"data\"\n  type = \"blob\"\n",
            mib * 3 / 2 + 64
        ),
    )
    .unwrap();
    let db = Database::open(std::path::Path::new(&cfg_path)).unwrap();
    let ins = db.prepare("INSERT INTO blobs (id, data) VALUES ($1, $2)").unwrap();
    let n_rows = mib * (1 << 20) / blob;
    let payload = vec![0xABu8; blob];

    let t0 = Instant::now();
    let mut id = 0i64;
    while (id as usize) < n_rows {
        let mut s = db.begin().unwrap();
        for _ in 0..BATCH.min(n_rows - id as usize) {
            s.execute(&ins, &params![id, payload.clone()]).unwrap();
            id += 1;
        }
        s.commit().unwrap();
    }
    let secs = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&cfg_path);
    (mib as f64) / secs
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let reps: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let mib: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let blob: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4096);
    // /dev/shm on Linux; the temp dir elsewhere (macOS: APFS — a DISK cell,
    // not a memory cell; compare ratios, never absolutes, across the two).
    let fallback = std::env::temp_dir().display().to_string();
    let dir: &str = args
        .get(4)
        .map(|s| s.as_str())
        .unwrap_or(if std::path::Path::new("/dev/shm").is_dir() {
            "/dev/shm"
        } else {
            &fallback
        });
    let mut off = Vec::new();
    let mut on = Vec::new();
    // ABAB pairing: adjacent arms see the same machine state.
    for r in 0..reps {
        off.push(run_arm(dir, "off", false, mib, blob));
        on.push(run_arm(dir, "on", true, mib, blob));
        println!(
            "rep {r}: off={:7.1} MiB/s   on={:7.1} MiB/s   ratio={:.3}",
            off[r],
            on[r],
            on[r] / off[r]
        );
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (m_off, m_on) = (med(&mut off), med(&mut on));
    println!(
        "median: off={m_off:.1} MiB/s  on={m_on:.1} MiB/s  ratio={:.3}  ({mib} MiB of {blob}-byte blobs, batch {BATCH}, none, dir={dir})",
        m_on / m_off
    );
}
