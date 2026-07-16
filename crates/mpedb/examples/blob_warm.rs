//! Is mpedb's large-blob write cost a ONE-TIME page-fault cost, or per write?
//!
//! `examples/blob_paths` says the destination mapping's page faults are ~94% of
//! a cold blob write (64 MiB: 78 ms cold, 4.6 ms once the pages are faulted —
//! 17x). If that carries into the engine, then mpedb-bench's blob cells — which
//! seed a FRESH database per cell — only ever measure first-touch, and the
//! steady-state cost of a long-lived process is a different, much smaller number.
//!
//! This writes the same blob repeatedly, deleting between rounds so the freelist
//! hands back pages that are already faulted in. Round 1 pays the faults; if the
//! later rounds are much faster, the bench's number is a cold-start artefact and
//! the "mpedb is 2x off raw std::fs on blobs" framing is measuring the wrong
//! thing.
//!
//! Usage: `blob_warm <dir> [mib] [rounds]`

use mpedb::{params, Config, Database};

fn cfg(path: &std::path::Path, size_mb: u64) -> Config {
    Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 16
durability = "none"

[[table]]
name = "blobs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "blob"
  nullable = false
"#,
        path.display()
    ))
    .unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(a.get(1).cloned().unwrap_or("/tmp/bw".into()));
    let mib: usize = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(16);
    let rounds: usize = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(6);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blob.mpedb");
    let _ = std::fs::remove_file(&path);

    let n = mib * 1024 * 1024;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let payload: Vec<u8> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();

    // Room for a couple of copies of the blob plus the tree.
    let db = Database::open_with_config(cfg(&path, (mib as u64 * 4).max(64))).unwrap();
    let ins = db.prepare("INSERT INTO blobs (id, v) VALUES ($1, $2)").unwrap();
    let del = db.prepare("DELETE FROM blobs WHERE id = $1").unwrap();

    println!("{mib} MiB blob, {rounds} rounds, delete between (pages come back via the freelist)");
    println!("{:>5} {:>9} {:>11} {:>9} {:>9}", "round", "ms", "MiB/s", "params", "execute");
    for r in 0..rounds {
        // Time the API copy apart from the engine. `IntoValue for &[u8]` does
        // `to_vec()` — a full copy of the payload before the engine sees a byte —
        // and folding it into the "engine" number would blame the engine for the
        // caller's malloc.
        let t0 = std::time::Instant::now();
        let p = params![r as i64, payload.as_slice()];
        let t_params = t0.elapsed();
        let t1 = std::time::Instant::now();
        db.execute(&ins, &p).unwrap();
        let t_exec = t1.elapsed();
        let d = t0.elapsed();
        println!(
            "{:>5} {:>9.2} {:>11.1} {:>9.2} {:>9.2}{}",
            r,
            d.as_secs_f64() * 1e3,
            mib as f64 / d.as_secs_f64(),
            t_params.as_secs_f64() * 1e3,
            t_exec.as_secs_f64() * 1e3,
            if r == 0 { "   <- pays the page faults" } else { "" }
        );
        // free the pages so the next round draws them back already faulted
        db.execute(&del, &params![r as i64]).unwrap();
    }
    // #40: where did the per-page time go? These must add up to the rounds above.
    mpedb_core::engine::leakstat::dump("blob");
    drop(db);
    let _ = std::fs::remove_file(&path);
}
