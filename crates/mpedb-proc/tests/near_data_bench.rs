//! The near-data benchmark: does running the analysis NEXT TO the data
//! (a stored procedure) beat query-then-analyze-in-client, in time AND
//! memory — and does streaming beat materializing?
//!
//! Three ways to compute `sum(v)` over a 200,000-row table
//! `t(id int64 PK, v int64)`:
//!
//! 1. **client-python** — `mpedb-py`: `db.execute(hash)` materializes the
//!    whole result as a Python `list[tuple]`, then a Python loop sums it.
//!    Requires `python3.12` and a built module
//!    (`cargo build --release -p mpedb-py`; the test picks up
//!    `target/release/libmpedb_py.so`, or set `MPEDB_PY_MODULE` to a
//!    ready-made `mpedb.so`). Skipped loudly if unavailable.
//! 2. **proc-materializing** — Python-subset proc: `db.query` builds the
//!    full interpreter list-of-tuples, a `while` loop sums it. Needs a
//!    raised instruction budget (the default 1M dies at ~62k rows — by
//!    design).
//! 3. **proc-streaming** — Python-subset proc: `for row in db.rows(...)`
//!    pulls one row at a time (facade `RowStream`, O(BATCH)=O(256 rows)
//!    interpreter memory).
//!
//! Time is wall-clock per analysis (warmup + 5 runs, min and mean
//! reported). Interpreter/client PEAK MEMORY is measured with a counting
//! global allocator (phases 2/3: exact heap high-water delta around one
//! call, mmap'd database pages excluded — they are shared and identical
//! across phases) and with `tracemalloc` (phase 1: traced Python
//! allocation peak, i.e. the rows list) plus `ru_maxrss`.
//!
//! Run: `cargo test -p mpedb-proc --release --test near_data_bench -- --ignored --nocapture`
//! (numbers from a debug build are meaningless).

use mpedb::{params, Config, Database};
use mpedb_proc::{Lang, ProcEngine, ProcValue, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const ROWS: i64 = 200_000;
const RUNS: usize = 5;

// ------------------------------------------------------- counting allocator

/// Global allocator wrapper tracking live heap bytes and the high-water
/// mark. `peak_delta` = how much higher the heap peaked during a closure
/// than where it started — the honest "peak interpreter memory" number.
struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delegates to System; the bookkeeping is atomic and lock-free.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            bump(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            bump(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = System.realloc(p, l, new_size);
        if !q.is_null() {
            if new_size >= l.size() {
                bump(new_size - l.size());
            } else {
                CUR.fetch_sub(l.size() - new_size, Ordering::Relaxed);
            }
        }
        q
    }
}

fn bump(n: usize) {
    let c = CUR.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(c, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f`, returning (result, peak heap growth in bytes during the call).
fn with_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let start = CUR.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(start))
}

// ------------------------------------------------------------------ helpers

fn human(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

fn int(v: &ProcValue) -> i64 {
    match v {
        ProcValue::Scalar(Value::Int(i)) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

/// (min ms, mean ms) over RUNS timed calls of `f` (after one warmup).
fn time_runs(mut f: impl FnMut()) -> (f64, f64) {
    f(); // warmup
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t0 = Instant::now();
        f();
        times.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    (min, mean)
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("scratch"));
    }
}

// ---------------------------------------------------------------- the bench

#[test]
#[ignore = "benchmark; run --release with --nocapture"]
fn near_data_three_way() {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let db_path = dir.join(format!("mpedb-neardata-{}.mpedb", std::process::id()));
    let _guard = FileGuard(db_path.clone());
    let _ = std::fs::remove_file(&db_path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 128
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        db_path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();

    // Seed 200k rows: v = id % 1000 (sum fits comfortably in i64).
    let ins = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)").unwrap();
    let mut session = db.begin().unwrap();
    for id in 0..ROWS {
        session.execute(&ins, &params![id, id % 1000]).unwrap();
    }
    session.commit().unwrap();
    let expected: i64 = (0..ROWS).map(|i| i % 1000).sum();

    let mut engine = ProcEngine::new(&db);
    // The materializing loop needs ~16 instrs/row (3.2M); give both procs
    // the same raised budget so the comparison is about execution, not
    // budget shape. Rows budget: default-sized, far above 200k.
    engine.set_budget(1_000_000_000, 10_000, 10_000_000);
    engine
        .define(
            "
def sum_mat():
    rows = db.query(\"SELECT id, v FROM t\")
    s = 0
    i = 0
    n = len(rows)
    while i < n:
        s = s + rows[i][1]
        i = i + 1
    return s
",
            Lang::Python,
        )
        .unwrap();
    engine
        .define(
            "
def sum_stream():
    s = 0
    for row in db.rows(\"SELECT id, v FROM t\"):
        s = s + row[1]
    return s
",
            Lang::Python,
        )
        .unwrap();

    // Phase 2: proc, materializing db.query.
    let ((), mat_peak) = with_peak(|| {
        assert_eq!(int(&engine.call("sum_mat", &[]).unwrap()), expected);
    });
    let (mat_min, mat_mean) = time_runs(|| {
        assert_eq!(int(&engine.call("sum_mat", &[]).unwrap()), expected);
    });

    // Phase 3: proc, streaming db.rows.
    let ((), stream_peak) = with_peak(|| {
        assert_eq!(int(&engine.call("sum_stream", &[]).unwrap()), expected);
    });
    let (stream_min, stream_mean) = time_runs(|| {
        assert_eq!(int(&engine.call("sum_stream", &[]).unwrap()), expected);
    });

    // Phase 1: client python via mpedb-py (skipped loudly if unavailable).
    let py = python_phase(&db_path, &toml, expected);

    println!();
    println!(
        "near-data benchmark: sum(v) over {ROWS} rows, {RUNS} runs (+1 warmup) each"
    );
    println!(
        "| variant            | ms/analysis (min) | ms (mean) | peak interpreter/client memory |"
    );
    println!(
        "|--------------------|-------------------|-----------|--------------------------------|"
    );
    if let Some((py_min, py_mean, traced, maxrss_kb)) = py {
        println!(
            "| client-python      | {py_min:>17.1} | {py_mean:>9.1} | {} tracemalloc rows-list peak (process ru_maxrss {} MiB) |",
            human(traced),
            maxrss_kb / 1024
        );
    } else {
        println!("| client-python      |            (skipped: python3.12 or mpedb.so unavailable) |");
    }
    println!(
        "| proc-materializing | {mat_min:>17.1} | {mat_mean:>9.1} | {} heap peak delta |",
        human(mat_peak)
    );
    println!(
        "| proc-streaming     | {stream_min:>17.1} | {stream_mean:>9.1} | {} heap peak delta |",
        human(stream_peak)
    );
    println!();

    // The hypothesis-shaped assertions, kept loose enough to not flake:
    // streaming must be O(1)-ish in the result size — at least 50x below
    // materializing (measured: ~3 orders of magnitude).
    assert!(
        stream_peak * 50 < mat_peak.max(1),
        "streaming peak {} not clearly below materializing peak {}",
        human(stream_peak),
        human(mat_peak)
    );
    db.verify().unwrap();
}

/// Run the client-python phase in a subprocess; returns
/// (min ms, mean ms, tracemalloc peak bytes, ru_maxrss KiB) or None if the
/// environment lacks python3.12 / the module.
fn python_phase(
    db_path: &Path,
    toml: &str,
    expected: i64,
) -> Option<(f64, f64, usize, usize)> {
    // Locate a built module: MPEDB_PY_MODULE=<path to .so>, else the
    // workspace's target/release/libmpedb_py.so.
    let module = std::env::var("MPEDB_PY_MODULE").map(PathBuf::from).ok().or_else(|| {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/libmpedb_py.so");
        p.exists().then_some(p)
    });
    let Some(module) = module else {
        eprintln!(
            "SKIPPING client-python phase: no mpedb python module found \
             (build with `cargo build --release -p mpedb-py` or set MPEDB_PY_MODULE)"
        );
        return None;
    };
    // Stage <scratch>/mpedb.so + config so `import mpedb` resolves.
    let scratch = db_path.with_extension("scratch");
    std::fs::create_dir_all(&scratch).ok()?;
    std::fs::copy(&module, scratch.join("mpedb.so")).ok()?;
    let cfg = scratch.join("db.toml");
    std::fs::write(&cfg, toml).ok()?;

    let script = format!(
        r#"
import sys, time, resource, tracemalloc
sys.path.insert(0, {moddir:?})
import mpedb
db = mpedb.Database({cfg:?})
h = db.prepare("SELECT id, v FROM t")

def analysis():
    rows = db.execute(h)
    s = 0
    for r in rows:
        s += r[1]
    return s

assert analysis() == {expected}  # warmup + correctness
times = []
for _ in range({runs}):
    t0 = time.perf_counter()
    s = analysis()
    times.append((time.perf_counter() - t0) * 1000.0)
    assert s == {expected}

tracemalloc.start()
analysis()
_cur, peak = tracemalloc.get_traced_memory()
tracemalloc.stop()

print("MS", " ".join(f"{{t:.3f}}" for t in times))
print("TRACED_PEAK", peak)
print("MAXRSS_KB", resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
"#,
        moddir = scratch.display().to_string(),
        cfg = cfg.display().to_string(),
        runs = RUNS,
        expected = expected,
    );
    let out = std::process::Command::new("python3.12")
        .arg("-c")
        .arg(&script)
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "SKIPPING client-python phase: script failed\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            return None;
        }
        Err(e) => {
            eprintln!("SKIPPING client-python phase: cannot run python3.12: {e}");
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut times: Vec<f64> = Vec::new();
    let mut traced = 0usize;
    let mut maxrss = 0usize;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("MS ") {
            times = rest
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
        } else if let Some(rest) = line.strip_prefix("TRACED_PEAK ") {
            traced = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("MAXRSS_KB ") {
            maxrss = rest.trim().parse().unwrap_or(0);
        }
    }
    if times.is_empty() {
        eprintln!("SKIPPING client-python phase: unparseable output:\n{stdout}");
        return None;
    }
    let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    Some((min, mean, traced, maxrss))
}
