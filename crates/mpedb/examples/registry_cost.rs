//! #124: **what does a compile cost as a function of registry size?**
//!
//! `mem_shapes`'s `registry` shape (#123) reported held bytes for ONE compile
//! after N plans were published. This tool sweeps N and reports the curve —
//! held bytes AND compile latency — including a post-eviction point, which is
//! the one that says whether the cost is a ratchet (paid over everything ever
//! registered) or merely proportional to the live registry.
//!
//! ```text
//! registry_cost                       # default sweep: 0 1000 4096 4400
//! registry_cost 0 1000 4096 4200      # explicit N's
//! REGCOST_VIA=prepare registry_cost   # compile + PUBLISH, not compile alone
//! ```
//!
//! One process per point would be cleaner for `RssAnon`, but `held` is a live
//! counter that is re-armed per point, so the sweep is honest in one process.
//!
//! # What it found, and what the fix did
//!
//! Two costs, both O(everything ever registered), on this box (release):
//!
//! ```text
//!   COMPILE  (prepare_detached: bind + plan, no registry write)
//!   N        held B      median µs      after #124: held B   median µs
//!   0             1519         6.7                    1519         6.9
//!   1000       297_227       237.9                    1519         7.4
//!   4096     1_215_587      1056.3                    1519         7.5
//!   4400*    1_164_417       979.6                    1519         7.5
//!
//!   PUBLISH  (prepare: the same compile, plus the registry insert)
//!   N        held B      median µs      after #124: held B   median µs
//!   0             3273        28.1                    3273        19.5
//!   1000       299_766       160.7                    3545        22.6
//!   4096     1_218_126       568.8                    3783†       23.4
//!   4400*    1_166_956       542.7                    3783        24.3
//!
//!   * 4400 published = 3841 live: eviction has fired once (it drops
//!     EVICT_BATCH=256 of MAX=4096). The cost barely moves, which is the
//!     point — evicting 6% of the registry buys back 6% of a cost that
//!     should never have been proportional to it at all.
//!   † the ONE insert in 256 that trips the authoritative walk still holds
//!     ~1.2 MB; it is amortised out of the median and is the design.
//! ```
//!
//! The slope was **297 B held and 0.24 µs per previously-registered plan**, on
//! every `query()` and every `prepare()`, forever. Three sources, all of them
//! walking the whole sys keyspace to find a handful of records that share it
//! with the plan registry: `load_policy_catalog`, `load_view_catalog` (both per
//! compile), and `evict_if_full` (per publication of a new statement text).
//! The first two are now prefix-bounded; the third reads an 8-byte counter and
//! only ranks the registry when that counter reaches the cap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use mpedb::{Config, Database};

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(LIVE.fetch_add(l.size(), Relaxed) + l.size(), Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            PEAK.fetch_max(LIVE.fetch_add(l.size(), Relaxed) + l.size(), Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size(), Relaxed);
            PEAK.fetch_max(LIVE.fetch_add(new, Relaxed) + new, Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

fn arm() -> usize {
    let live = LIVE.load(Relaxed);
    PEAK.store(live, Relaxed);
    live
}

fn tmp_path(tag: &str) -> String {
    let dir = ["/mnt/xfs/mpedb-scratch", "/mnt/ext4/mpedb-scratch", "/tmp"]
        .into_iter()
        .find(|d| std::fs::create_dir_all(d).is_ok())
        .unwrap_or("/tmp");
    let p = format!("{dir}/regcost-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&p);
    p
}

fn schema_toml(path: &str) -> String {
    format!(
        "[database]\npath = \"{path}\"\nsize_mb = 512\nmax_readers = 8\n\n\
         [[table]]\nname = \"src\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n"
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let points: Vec<usize> = if args.is_empty() {
        vec![0, 1000, 4096, 4400]
    } else {
        args.iter().map(|a| a.parse().expect("N")).collect()
    };
    let policies = std::env::var("REGCOST_POLICIES").is_ok();
    let views: usize = std::env::var("REGCOST_VIEWS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);

    println!("n_published held_bytes us_median us_p90");

    for &n in &points {
        let path = tmp_path(&format!("{n}"));
        let cfg = Config::from_toml_str(&schema_toml(&path)).unwrap();
        let db = Database::open_with_config(cfg).unwrap();

        if policies {
            db.query("ALTER TABLE src ENABLE ROW LEVEL SECURITY", &[]).unwrap();
        }
        for v in 0..views {
            db.query(&format!("CREATE VIEW v{v} AS SELECT id FROM src"), &[]).unwrap();
        }
        for k in 0..n {
            db.prepare(&format!("SELECT id, a, t FROM src WHERE id = {k}")).unwrap();
        }

        // Held bytes for ONE compile of a statement never seen before.
        // `prepare_detached` is `prepare` minus the registry write, so this is
        // compilation alone and never grows the registry underneath the sweep.
        let probe = |i: usize| format!("SELECT id FROM src WHERE a = {i} /* probe */");
        // `detached` (default) = compile only (`prepare_detached` opens no write
        // txn). `prepare` = compile + publish, which also runs `evict_if_full`.
        let publish = std::env::var("REGCOST_VIA").is_ok_and(|v| v == "prepare");
        let one = |sql: &str| {
            if publish {
                std::hint::black_box(db.prepare(sql).unwrap().0[0]);
            } else {
                std::hint::black_box(db.prepare_detached(sql).unwrap());
            }
        };
        let live_before = arm();
        one(&probe(999_999));
        let held = PEAK.load(Relaxed).saturating_sub(live_before);

        // Latency: 200 distinct fresh statements, median + p90.
        let mut us: Vec<u128> = Vec::with_capacity(200);
        for i in 0..200 {
            let sql = probe(i);
            let t0 = std::time::Instant::now();
            one(&sql);
            us.push(t0.elapsed().as_nanos());
        }
        us.sort_unstable();
        let med = us[us.len() / 2] as f64 / 1000.0;
        let p90 = us[us.len() * 9 / 10] as f64 / 1000.0;

        println!("{n} {held} {med:.1} {p90:.1}");

        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
    }
}
