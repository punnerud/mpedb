//! Instrument the high-water leak instead of guessing at it.
//!
//! `mpedb-core/tests/high_water_leak.rs` characterises the symptom — 4+ writers
//! churning a 1000-key table grow the high-water linearly until the file fills —
//! and lists the questions to answer BEFORE touching the reclamation path. This
//! answers them. Every 200 ms, from a parent that only observes:
//!
//!   txn        how fast the writers are going
//!   high_water pages ever allocated. THE LEAK, if it climbs
//!   bound      `oldest_pinned_cache` — the reclamation gate
//!   lag        txn - bound. If this grows, the gate is falling behind
//!   free_ents  freelist entries. If this grows, freeing outruns reclaiming
//!
//! and, once the writers are gone, the freelist's shape: how many entries, how
//! many pages they hold, and how OLD each is — which tells apart "entries are
//! stuck" from "entries are churn". An aggregate counter cannot.
//!
//! Five hypotheses have died on this instrument; the module docs of
//! `mpedb-core/tests/high_water_leak.rs` list them so number six is cheaper.
//!
//! Usage: `leak_probe <dir> <writers> <secs>`. Build with `--features leakstat`
//! for the per-branch alloc counters (off by default — they put an atomic on
//! the page-allocation path); without it the table and freelist shape still
//! work and only the `leakstat[...]` line reads zero.

use mpedb::{params, Config, Database};

const KEYSPACE: u64 = 1000;

fn cfg(path: &std::path::Path) -> Config {
    Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 64
durability = "none"

[[table]]
name = "items"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"
"#,
        path.display()
    ))
    .unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let writers: usize = a[2].parse().unwrap();
    let secs: u64 = a[3].parse().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("leak.mpedb");
    let _ = std::fs::remove_file(&path);

    {
        let db = Database::open_with_config(cfg(&path)).unwrap();
        let ins = db
            .prepare("INSERT INTO items (id, a, b) VALUES ($1, $2, $3)")
            .unwrap();
        let mut s = db.begin().unwrap();
        for i in (0..KEYSPACE as i64).step_by(2) {
            s.execute(&ins, &params![i, i, "seed"]).unwrap();
        }
        s.commit().unwrap();
    }

    let mut pids = Vec::new();
    for k in 0..writers {
        // SAFETY: forked from a single-threaded parent; the child only opens the
        // database and leaves via _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            child(&path, k as u64, secs);
            if k == 0 {
                mpedb_core::engine::leakstat::dump("writer0");
            }
            unsafe { libc::_exit(0) };
        }
        pids.push(pid);
    }

    let db = Database::open_with_config(cfg(&path)).unwrap();
    println!("   t |        txn | high_water |      bound |        lag | free_ents");
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < secs {
        std::thread::sleep(std::time::Duration::from_millis(200));
        match db.leak_counters() {
            Ok((txn, hw, bound, ents)) => println!(
                "{:4.1} | {txn:10} | {hw:10} | {bound:10} | {:10} | {ents:9}",
                t0.elapsed().as_secs_f64(),
                txn.saturating_sub(bound)
            ),
            Err(e) => println!("{:4.1} | probe: {e}", t0.elapsed().as_secs_f64()),
        }
    }
    for p in pids {
        let mut st = 0i32;
        unsafe { libc::waitpid(p, &mut st, 0) };
    }

    // The writers are gone: the freelist is now quiescent, so its shape is a
    // clean read. Age each entry against the final txn — a "stuck" entry is
    // one far below the bound (long reclaimable) that nobody ever drained.
    if let Ok(((txn, hw, bound), ents)) = db.freelist_shape() {
        let pages: usize = ents.iter().map(|e| e.1).sum();
        println!("\nfinal: txn={txn} high_water={hw} bound={bound}");
        println!("freelist: {} entries holding {pages} pages", ents.len());
        let mut buckets = [0usize; 6]; // age in txns: <10, <1e2, <1e3, <1e4, <1e5, rest
        for &(t, _) in &ents {
            let age = txn.saturating_sub(t);
            let b = match age {
                0..=9 => 0,
                10..=99 => 1,
                100..=999 => 2,
                1_000..=9_999 => 3,
                10_000..=99_999 => 4,
                _ => 5,
            };
            buckets[b] += 1;
        }
        println!(
            "age(txns): <10:{} <100:{} <1k:{} <10k:{} <100k:{} older:{}",
            buckets[0], buckets[1], buckets[2], buckets[3], buckets[4], buckets[5]
        );
        for &(t, n) in ents.iter().take(8) {
            println!("  oldest: txn={t} (age {}) pages={n}", txn.saturating_sub(t));
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn child(path: &std::path::Path, k: u64, secs: u64) {
    let Ok(db) = Database::open_with_config(cfg(path)) else {
        return;
    };
    let ins = db
        .prepare("INSERT INTO items (id, a, b) VALUES ($1, $2, $3)")
        .unwrap();
    let upd = db.prepare("UPDATE items SET a = $1 WHERE id = $2").unwrap();
    let del = db.prepare("DELETE FROM items WHERE id = $1").unwrap();
    let sel = db.prepare("SELECT id, a, b FROM items WHERE id = $1").unwrap();
    let mut x = k.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < secs {
        let key = (next() % KEYSPACE) as i64;
        let _ = match next() % 10 {
            0..=2 => db.execute(&ins, &params![key, key * 7, "mixed"]),
            3..=5 => db.execute(&upd, &params![(next() % (1 << 20)) as i64, key]),
            6..=8 => db.execute(&sel, &params![key]),
            _ => db.execute(&del, &params![key]),
        };
    }
}
