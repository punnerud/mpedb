//! **Where does mpedb's peak memory actually go, per query shape?** (#123 step 1)
//!
//! Nobody had profiled this. The memory numbers that existed were the freelist
//! high-water leak (fixed, #37), the join-cell counter (`examples/mpee_memory`)
//! and the blob path (`examples/blob_paths`) — three points, no map. Before
//! designing a streaming executor it is worth knowing which shapes actually
//! hold bytes, and whether what they hold is O(result) or O(1). This repo has
//! been burned by skipping that step exactly once and wrote it down
//! (`INNOVATIONS.md` §9.2, the locality sort measured at 4.23 vs 4.26).
//!
//! # The headline metric is `held`, not `rss`
//!
//! `held` is the peak of a **live-bytes counter in a wrapping global
//! allocator**: every `alloc` adds, every `dealloc` subtracts, and the peak is
//! `fetch_max` on the running sum. That is precisely "how many heap bytes did
//! the engine hold SIMULTANEOUSLY", which is the number a streaming design
//! would move, and it is deterministic — same shape, same rows, same answer on
//! every machine, no allocator quantisation, no sampling race, no mmap'd file
//! pages folded in.
//!
//! `rss` is peak **`RssAnon`** from `/proc/self/status`, sampled by a
//! non-allocating poller thread, reported beside it as corroboration.
//! `RssAnon` and not `VmHWM`: on a database that mmaps its file, `VmHWM`
//! charges the whole touched mapping and measures the *file*, not the engine
//! (`README.md` says the same about the writer-process comparison). Even
//! `RssAnon` over-reports what is HELD, because glibc's malloc does not return
//! freed arenas to the kernel — so a shape with heavy churn and a small live
//! set looks large in `RssAnon` and small in `held`. `held` is the truth for
//! the design question; `rss` is the truth for the OOM killer.
//!
//! `churn` (cumulative bytes ever allocated) is printed too. `churn / held`
//! separates the two failure modes: a big `held` is what streaming fixes, a
//! big `churn` with a small `held` is an allocator problem, not a memory one.
//!
//! Both counters are **reset immediately before the measured statement**, after
//! the fixture is built and populated, so the number is the statement's own
//! marginal footprint and not the loader's.
//!
//! # Usage
//!
//! ```text
//! mem_shapes <shape> <rows>          MEM_VIA=execute|query|prepare|compile
//!
//!   pkpoint       SELECT 6 cols WHERE id = 1            -- the O(1) floor
//!   select_limit  SELECT 6 cols LIMIT 10                -- the O(1) control
//!   count         SELECT count(*)                       -- NOT O(1): see below
//!   select        SELECT 6 cols, no ORDER BY            -- ExecResult::Rows
//!   stream        the same SQL via Database::stream_query
//!   select_sorted the same SELECT + ORDER BY a          -- cannot stream today
//!   insert_select INSERT INTO dst SELECT * FROM src
//!   agg_many      GROUP BY g       (rows groups)
//!   agg_few       GROUP BY g10     (10 groups)          -- O(groups) or O(input)?
//!   window        row_number() OVER (PARTITION BY g10 ORDER BY a)
//!   rcte          WITH RECURSIVE ... LIMIT rows
//!   join_held     join whose inner is fully held, ZERO output rows
//!   join_rows     the same join, `rows` output rows
//!   update        UPDATE src SET b = b + 1
//!   delete        DELETE FROM src WHERE b >= 0
//!   blob          one INSERT of a `rows`-KiB blob via a $1 param
//!   blob_streamed the same bytes via WriteSession::insert_streaming
//!   registry      publish `rows` plans, then MEASURE A COMPILE (MEM_VIA=compile)
//! ```
//!
//! Run one shape per process: `RssAnon` peaks do not reset, and a fresh
//! process is the only honest floor. A shell loop over the table above is the
//! whole driver.
//!
//! # Results
//!
//! Measured on this box at 10 k / 40 k / 160 k rows; `B/row` is the 40 k→160 k
//! slope, so every fixed cost cancels. Full table and analysis in
//! `design/DESIGN-STREAM-EXEC.md` §2.
//!
//! **This table is the BASELINE — the state that motivated the work, not the
//! state today.** It is kept as measured rather than refreshed in place,
//! because it is the evidence the design was argued from; overwriting it would
//! delete the before-number that makes the after-number mean anything. See
//! "What has changed since" at the end.
//!
//! ```text
//!   shape          held@160k    B/row   out@160k   what is held
//!   agg_many       167875378   1049.4     160000   input + group map + 3 spines
//!   window          78340330    489.8     160000   base + projected + 5 side vecs
//!   join_rows       65482467    409.6     160000   held inner + product
//!   insert_select   57111901    357.1     160000   source set + built target set
//!   update/delete   57111901    357.1     160000   full old-row set
//!   select_sorted   54660353    341.8     160000   full set, sorted in place
//!   select          50820659    317.8     160000   ExecResult::Rows
//!   count           50822241    317.8          1   <- the whole input, for one int
//!   agg_few         50826842    317.8         10   <- the whole input, for ten rows
//!   join_held       30182303    188.8          0   the inner relation alone
//!   rcte            29114511    182.0     160000   fixpoint result (1 column)
//!   blob 64MiB      67110007   1.00x payload       one encoded row image
//!   blob_streamed        4537      0.0          1   <- flat at 4/16/64 MiB
//!   stream              61801    0.002     160000   <- flat; 822x under `select`
//!   select_limit         3437      0.0         10
//!   pkpoint               618      0.0          1
//! ```
//!
//! `rss_anon / held` lands between 0.95 and 1.11 on every shape above 10 MB —
//! two independent instruments agreeing, which is the only real evidence that
//! neither is measuring itself.
//!
//! Three things worth keeping:
//!
//! 1. **Aggregation materialised its whole input.** `SELECT count(*)` held
//!    50.8 MB to produce one integer — `exec/aggregate.rs` gathered in full and
//!    said "Unbounded on purpose". Ranked by bytes held it was joint 8th;
//!    ranked by bytes held per byte of ANSWER it was first by orders of
//!    magnitude, which is what made it the first thing to fix rather than the
//!    biggest row in the table.
//! 2. **`INSERT … SELECT` costs 12% over a plain SELECT, not 100%.** The double
//!    materialisation is real in the code, but `for srow in src` consumes the
//!    source by value, so only the two SPINES are simultaneously full-length —
//!    a 39 B/row delta, not 318.
//! 3. **mpedb already streams, twice, and both are flat.** `stream_query` at
//!    61.8 KB over 160 k rows and `insert_streaming` at 4537 B over 64 MiB. The
//!    gap is the row pipeline in between, not the idea.
//!
//! # What has changed since (re-run this probe to refresh)
//!
//! Finding 1 is fixed: the aggregate is a fold, so it no longer holds its input
//! (`design/DESIGN-STREAM-EXEC.md` §5.1). The invariant is asserted in
//! `tests/agg_stream_mem.rs` rather than left to this probe.
//!
//! ```text
//!   shape        held@160k before   after      note
//!   count             50822241      79526      639x, and 32% FASTER
//!   agg_few           50826842      84110      604x
//!   agg_many         167875378  117132646      1.43x — output is genuinely O(n)
//! ```
//!
//! Three baseline rows are DELIBERATELY still true and are the next target
//! (#125): `join_rows`, an aggregate over a join, and an aggregate under a
//! correlated `FILTER` did not move at all. The fold streams, but it is still
//! handed WIDE rows — nothing tells the join which columns anything downstream
//! reads. That is a width problem, not a streaming one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use mpedb::{Config, Database, ExecResult, Value};

// ------------------------------------------------------------- the allocator

/// Wraps the system allocator with a live-bytes counter. The peak of that
/// counter is the metric this whole example exists to produce: bytes held
/// *simultaneously*, which is what a streaming executor would reduce.
///
/// `Relaxed` throughout: the measurement is a running sum with no ordering
/// relationship to anything else, and the only other thread (the `RssAnon`
/// poller) never allocates. `fetch_max` makes the peak correct even if that
/// ever stops being true.
struct Tracking;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CHURN: AtomicUsize = AtomicUsize::new(0);

fn note_alloc(n: usize) {
    CHURN.fetch_add(n, Relaxed);
    let live = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(live, Relaxed);
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if q.is_null() {
            return q;
        }
        // A grow-in-place still moves the live total; charge the delta the same
        // way either way so `held` counts the new size and not both sizes.
        LIVE.fetch_sub(l.size(), Relaxed);
        note_alloc(new);
        q
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// Arm the measurement: the peak restarts from whatever is live right now, so
/// the reported figure is the statement's marginal hold, not the loader's.
fn arm() -> usize {
    let live = LIVE.load(Relaxed);
    PEAK.store(live, Relaxed);
    CHURN.store(0, Relaxed);
    live
}

// ------------------------------------------------------------------ RssAnon

/// Peak `RssAnon`, in bytes, sampled by a thread that **never allocates** —
/// it holds the `/proc/self/status` fd open and `pread`s into a stack buffer,
/// because a `read_to_string` in the sampler would show up in `held` and the
/// two metrics would measure each other.
#[cfg(target_os = "linux")]
struct RssPoller {
    stop: std::sync::Arc<AtomicBool>,
    peak: std::sync::Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
fn rss_anon_bytes(fd: libc::c_int, buf: &mut [u8; 4096]) -> usize {
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len() - 1, 0) };
    if n <= 0 {
        return 0;
    }
    let s = &buf[..n as usize];
    // Hand-rolled scan for "RssAnon:" — no `str::from_utf8`, no split, no
    // allocation anywhere on this path.
    let needle = b"RssAnon:";
    let mut i = 0;
    while i + needle.len() < s.len() {
        if &s[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < s.len() && (s[j] == b' ' || s[j] == b'\t') {
                j += 1;
            }
            let mut kb = 0usize;
            while j < s.len() && s[j].is_ascii_digit() {
                kb = kb * 10 + (s[j] - b'0') as usize;
                j += 1;
            }
            return kb * 1024;
        }
        i += 1;
    }
    0
}

#[cfg(target_os = "linux")]
impl RssPoller {
    /// Spawned BEFORE `arm()`, so the thread's own setup allocations land in
    /// the fixture's account rather than the statement's.
    fn start() -> RssPoller {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let peak = std::sync::Arc::new(AtomicUsize::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let handle = std::thread::spawn(move || {
            let path = c"/proc/self/status";
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
            if fd < 0 {
                return;
            }
            let mut buf = [0u8; 4096];
            while !s.load(Relaxed) {
                p.fetch_max(rss_anon_bytes(fd, &mut buf), Relaxed);
                unsafe { libc::usleep(200) };
            }
            p.fetch_max(rss_anon_bytes(fd, &mut buf), Relaxed);
            unsafe { libc::close(fd) };
        });
        RssPoller { stop, peak, handle: Some(handle) }
    }

    fn finish(mut self) -> usize {
        self.stop.store(true, Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak.load(Relaxed)
    }
}

#[cfg(not(target_os = "linux"))]
struct RssPoller;
#[cfg(not(target_os = "linux"))]
impl RssPoller {
    fn start() -> RssPoller {
        RssPoller
    }
    fn finish(self) -> usize {
        0
    }
}

// ----------------------------------------------------------------- fixtures

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
    // Scratch on a real filesystem with room: /dev/shm would charge the
    // fixture's pages to this box's RAM and to nothing in this report.
    let dir = ["/mnt/xfs/mpedb-scratch", "/mnt/ext4/mpedb-scratch", "/tmp"]
        .into_iter()
        .find(|d| std::fs::create_dir_all(d).is_ok())
        .unwrap_or("/tmp");
    let p = format!("{dir}/mem-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&p);
    p
}

/// `src` is the workhorse: a PK, a unique group key, a 10-way group key, two
/// payload ints and one ~24-byte text — wide enough that a materialised row is
/// a real row and not a `Vec` of one `i64`.
///
/// `dst` mirrors it (for `INSERT … SELECT`), `small`/`dim` are the join pair,
/// `blobs` is the control.
fn schema_toml(path: &str, size_mb: usize) -> String {
    format!(
        "[database]\npath = \"{path}\"\nsize_mb = {size_mb}\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = 0\n\n\
         [[table]]\nname = \"src\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g10\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"b\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"dst\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g10\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"b\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"small\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\n\n\
         [[table]]\nname = \"dim\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"label\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"blobs\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"data\"\ntype = \"blob\"\n"
    )
}

/// Rough file sizing: ~64 B/row in `src` plus the same again if a shape copies
/// it, plus slack for the freelist and the blob control. Over-sizing costs
/// address space, not RSS — the pages are never touched.
fn size_mb_for(rows: usize) -> usize {
    256 + rows / 4000
}

fn open(rows: usize) -> Tmp {
    let path = tmp_path(&format!("{rows}"));
    let cfg = Config::from_toml_str(&schema_toml(&path, size_mb_for(rows))).unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    Tmp { db, path }
}

/// Bulk-load through multi-row `VALUES`, 500 at a time, **inside a
/// `WriteSession`**. All of this happens before `arm()`, so its own cost is
/// outside every number reported — but the choice of loader is still
/// load-bearing, and getting it wrong cost this file an afternoon:
///
/// The first version used `Database::query` per chunk. Every chunk is a
/// DISTINCT statement text (the literals differ), so every chunk **published a
/// distinct plan to the shared registry** — 200 plans of ~74 KB each for a
/// 100 k-row load, because a 500-row literal `VALUES` plan carries 3000
/// constants. `compile_maybe_explain` then did two full `sys_scan`s (policies
/// and views), and the registry lives in that same sys keyspace — so every
/// later compile materialised all 14.9 MB of them. It looked exactly like
/// "compiling a PK point lookup costs 149 bytes per row of the table", which is
/// false. `WriteSession::query` compiles into the local cache and never
/// publishes, so the registry stays empty and the shapes measure themselves.
///
/// **That scan is gone (#124: those loads are prefix-bounded now, and
/// publication reads a counter instead of ranking the registry), so the loader
/// choice no longer changes any number here.** It stays a `WriteSession`
/// anyway: it is faster, and a fixture that cannot contaminate the measurement
/// is worth more than one that merely does not today.
/// (The underlying effect is real and is measured on its own by the `registry`
/// shape — it is just not proportional to table rows.)
fn load_chunks(db: &Database, rows: usize, mut sql: impl FnMut(usize, usize) -> String) {
    let mut i = 0;
    while i < rows {
        // One transaction per 20 chunks: long enough to amortise the commit,
        // short enough that the write txn's own COW set stays bounded.
        let stop = (i + 10_000).min(rows);
        let mut w = db.begin().unwrap();
        while i < stop {
            let end = (i + 500).min(stop);
            w.query(&sql(i, end), &[]).unwrap();
            i = end;
        }
        w.commit().unwrap();
    }
}

fn load_src(db: &Database, rows: usize) {
    load_chunks(db, rows, |i, end| {
        let vals: Vec<String> = (i..end)
            .map(|k| format!("({k}, {k}, {}, {k}, {k}, 'payload text row {k}')", k % 10))
            .collect();
        format!("INSERT INTO src (id, g, g10, a, b, t) VALUES {}", vals.join(", "))
    });
}

fn load_join(db: &Database, rows: usize, matching: bool) {
    let vals: Vec<String> = (0..10).map(|k| format!("({k}, {k})")).collect();
    db.query(&format!("INSERT INTO small (id, k) VALUES {}", vals.join(", ")), &[]).unwrap();
    // `join_held`: every `dim.k` is outside `small.k`'s range, so the ON clause
    // rejects every candidate and the result is EMPTY — the peak is then the
    // held inner side alone, with no output set mixed into it.
    load_chunks(db, rows, move |i, end| {
        let vals: Vec<String> = (i..end)
            .map(|k| {
                let key = if matching { k % 10 } else { k + 1_000_000 };
                format!("({k}, {key}, 'dimension label {k}')")
            })
            .collect();
        format!("INSERT INTO dim (id, k, label) VALUES {}", vals.join(", "))
    });
}

const SELECT_COLS: &str = "SELECT id, g, g10, a, b, t FROM src";

/// Build the fixture for a shape and hand back the statement to measure.
/// `rows` means rows in `src`/`dim` for every shape but `blob`, where it is
/// KiB of payload.
fn fixture(shape: &str, rows: usize) -> (Tmp, String) {
    let t = open(rows);
    let sql = match shape {
        // The floor: a PK point lookup over the same populated table. Whatever
        // this holds is what OPENING A READ costs, not what the shape costs,
        // and it must be subtracted before any shape is called linear.
        "pkpoint" => {
            load_src(&t.db, rows);
            format!("{SELECT_COLS} WHERE id = 1")
        }
        "count" => {
            load_src(&t.db, rows);
            "SELECT count(*) FROM src".to_string()
        }
        "select" | "stream" => {
            load_src(&t.db, rows);
            SELECT_COLS.to_string()
        }
        // The genuine O(1) control: `LIMIT` without `ORDER BY` is the one
        // pushdown the executor has (`scan_rows_capped` breaks at the cap), so
        // this shape must stay flat as `rows` grows or the harness is lying.
        "select_limit" => {
            load_src(&t.db, rows);
            format!("{SELECT_COLS} LIMIT 10")
        }
        "select_sorted" => {
            load_src(&t.db, rows);
            format!("{SELECT_COLS} ORDER BY a")
        }
        "insert_select" => {
            load_src(&t.db, rows);
            "INSERT INTO dst (id, g, g10, a, b, t) SELECT id, g, g10, a, b, t FROM src".to_string()
        }
        "agg_many" => {
            load_src(&t.db, rows);
            "SELECT g, sum(a), count(*) FROM src GROUP BY g".to_string()
        }
        "agg_few" => {
            load_src(&t.db, rows);
            "SELECT g10, sum(a), count(*) FROM src GROUP BY g10".to_string()
        }
        "window" => {
            load_src(&t.db, rows);
            "SELECT id, row_number() OVER (PARTITION BY g10 ORDER BY a) FROM src".to_string()
        }
        "rcte" => {
            format!(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) \
                 SELECT x FROM c LIMIT {rows}"
            )
        }
        "join_held" => {
            load_join(&t.db, rows, false);
            "SELECT small.id, dim.label FROM small, dim WHERE small.k = dim.k".to_string()
        }
        "join_rows" => {
            load_join(&t.db, rows, true);
            "SELECT small.id, dim.label FROM small, dim WHERE small.k = dim.k".to_string()
        }
        "update" => {
            load_src(&t.db, rows);
            "UPDATE src SET b = b + 1".to_string()
        }
        "delete" => {
            load_src(&t.db, rows);
            "DELETE FROM src WHERE b >= 0".to_string()
        }
        "blob" => "INSERT INTO blobs (id, data) VALUES (1, $1)".to_string(),
        // The control's control: `WriteSession::insert_streaming` over a
        // `ReaderBlobSource`, which pulls one page at a time. If the ordinary
        // param path holds 1.0x the payload and this holds O(1), then the
        // resident copy is the CALLER'S `Value::Blob`, not the engine — which
        // is the claim `examples/blob_stream` and the `ReaderBlobSource` docs
        // already make, here measured rather than asserted.
        "blob_streamed" => String::new(),
        // Not a query shape — a property of COMPILATION. Publish `rows`
        // distinct plans to the shared registry, then measure what compiling
        // ONE trivial statement costs afterwards. This used to be O(bytes ever
        // registered) — 297 B held and 0.24 µs per previously-registered plan —
        // because `compile_maybe_explain` ran two full `sys_scan`s and the
        // registry lives in that keyspace. #124 made both loads prefix-bounded;
        // this shape now reads ~3.1 KB at rows=4096, flat in `rows`. See
        // `examples/registry_cost` for the sweep. Run it with `MEM_VIA=compile`.
        "registry" => {
            for k in 0..rows {
                t.db.prepare(&format!("SELECT id FROM src WHERE id = {k}")).unwrap();
            }
            format!("{SELECT_COLS} WHERE id = 1")
        }
        other => panic!("unknown shape `{other}`"),
    };
    (t, sql)
}

// -------------------------------------------------------------- measurement

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: mem_shapes <shape> <rows>   (see the module docs for shapes)");
        std::process::exit(2);
    }
    let shape = args[1].clone();
    let rows: usize = args[2].parse().expect("rows must be a number");

    let (t, sql) = fixture(&shape, rows);

    // The blob control's payload is built BEFORE arming: the bytes the caller
    // already holds are the caller's, and the question is what the WRITE PATH
    // adds on top of them. Charging the payload here would guarantee the
    // control ranks first and would measure nothing about mpedb.
    let params: Vec<Value> = if shape == "blob" || shape == "blob_streamed" {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        vec![Value::Blob(
            (0..rows * 1024)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect(),
        )]
    } else {
        Vec::new()
    };

    // **Prepare outside the window, then measure `execute(hash, params)`** —
    // the documented hot path, "zero parsing". This is not a nicety: the first
    // draft measured `Database::query(sql, …)`, which recompiles the SQL and
    // re-`register`s the plan on every call. A bare `WHERE id = 1` PK point
    // lookup measured **149 bytes per row of the whole table** through `query`
    // and ~0 through `execute`. Left in, it would have put a false linear floor
    // under every shape in this file.
    //
    // The slope was never O(table): it was O(registry), because this file's own
    // loader published one plan per 500-row chunk (see `load_chunks`) and
    // compilation scanned the whole sys keyspace. #124 removed the scan and the
    // slope with it. `execute(hash, params)` is still what belongs in the
    // window — it is the documented hot path — but that is now a statement
    // about which API is being profiled, not a workaround.
    let streamed_blob = shape == "blob_streamed";
    let hash = if streamed_blob {
        // No SQL: `insert_streaming` is a typed engine call, not a plan.
        t.db.prepare("SELECT id FROM blobs WHERE id = 0").unwrap()
    } else {
        t.db.prepare(&sql).unwrap()
    };

    let plan_line = match t.db.query(&format!("EXPLAIN {sql}"), &params) {
        Ok(ExecResult::Explain(e)) => e.lines().next().unwrap_or("").trim().to_string(),
        _ => String::new(),
    };

    let poller = RssPoller::start();
    let rss_before = {
        // One reading before arming, so the delta is the statement's.
        #[cfg(target_os = "linux")]
        {
            let mut buf = [0u8; 4096];
            let fd = unsafe { libc::open(c"/proc/self/status".as_ptr(), libc::O_RDONLY) };
            let v = if fd >= 0 { rss_anon_bytes(fd, &mut buf) } else { 0 };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
            v
        }
        #[cfg(not(target_os = "linux"))]
        0usize
    };

    let live_before = arm();
    let t0 = std::time::Instant::now();

    // `MEM_VIA` selects WHICH ENTRY POINT is measured. The default is the hot
    // path; the other two exist because the difference between them turned out
    // to be the largest single number in this file.
    //
    //   execute  (default)  execute(hash, params) — zero parsing
    //   query               Database::query(sql, params) — recompiles AND
    //                       re-`register`s the plan on every call
    //   prepare             Database::prepare(sql) alone, on a statement never
    //                       seen before, so the registry write is not elided
    let via = std::env::var("MEM_VIA").unwrap_or_else(|_| "execute".into());

    let out_rows: usize = if streamed_blob {
        let Value::Blob(bytes) = &params[0] else { unreachable!() };
        let n = bytes.len();
        let mut src = mpedb::ReaderBlobSource::new(&bytes[..], n);
        let mut w = t.db.begin().unwrap();
        w.insert_streaming("blobs", &[Value::Int(1), Value::Blob(Vec::new())], 1, &mut src)
            .unwrap();
        w.commit().unwrap();
        1
    } else if via == "compile" {
        // `prepare_detached` is `prepare` MINUS the registry write: it never
        // opens a write transaction. The difference between this and
        // `via=prepare` is exactly what publishing a plan costs.
        let fresh = format!("{sql} /* {} */", std::process::id());
        std::hint::black_box(t.db.prepare_detached(&fresh).unwrap());
        0
    } else if via == "prepare" {
        // A statement text that cannot already be in the registry.
        let fresh = format!("{sql} /* {} */", std::process::id());
        std::hint::black_box(t.db.prepare(&fresh).unwrap());
        0
    } else if via == "query" {
        match t.db.query(&sql, &params).unwrap() {
            ExecResult::Rows { rows, .. } => {
                let n = rows.len();
                std::hint::black_box(&rows);
                n
            }
            ExecResult::Affected(n) => n as usize,
            ExecResult::Explain(_) => 0,
        }
    } else if shape == "stream" {
        // Draining a stream and counting is the honest comparison to
        // materialising it: the consumer keeps nothing either way.
        let mut s = t.db.stream_query(&hash, &params).unwrap();
        let mut n = 0usize;
        while let Some(r) = s.next().unwrap() {
            std::hint::black_box(r);
            n += 1;
        }
        n
    } else {
        match t.db.execute(&hash, &params).unwrap() {
            ExecResult::Rows { rows, .. } => {
                let n = rows.len();
                // Read the peak while the result is STILL ALIVE — the whole
                // point is what the statement holds at its high-water mark, and
                // dropping first would still be caught by PEAK, but this makes
                // the ordering explicit rather than incidental.
                std::hint::black_box(&rows);
                n
            }
            ExecResult::Affected(n) => n as usize,
            ExecResult::Explain(_) => 0,
        }
    };

    let micros = t0.elapsed().as_micros();
    let held = PEAK.load(Relaxed).saturating_sub(live_before);
    let churn = CHURN.load(Relaxed);
    let rss_peak = poller.finish();
    let rss = rss_peak.saturating_sub(rss_before);

    let per_row = if out_rows > 0 { held as f64 / out_rows as f64 } else { 0.0 };
    println!(
        "shape={shape} via={via} rows={rows} out_rows={out_rows} held={held} b_per_out_row={per_row:.1} \
         churn={churn} rss_anon={rss} rss_anon_peak={rss_peak} us={micros} plan={plan_line:?}"
    );
}
