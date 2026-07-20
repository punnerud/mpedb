//! **The memory claim of #123 §5.1, as an assertion rather than a benchmark.**
//!
//! `crates/mpedb/examples/mem_shapes.rs` measured it: at 160 000 rows
//! `SELECT count(*)` held **50.8 MB to produce one integer**, and
//! `GROUP BY` over ten groups held the same 50.8 MB for ten rows — 317.8 bytes
//! per input row, for an answer that is O(1). "A measured improvement nobody
//! asserts will regress", so the invariant lives here, in the test suite, and
//! not only in a design document.
//!
//! # What is measured
//!
//! `held` — the peak of a live-bytes counter in a wrapping [`GlobalAlloc`]:
//! every `alloc` adds, every `dealloc` subtracts, peak by `fetch_max`. That is
//! "how many heap bytes did the engine hold SIMULTANEOUSLY", which is exactly
//! the quantity streaming moves, and it is deterministic — no allocator
//! quantisation, no sampling race, no mmap'd file pages. Identical instrument
//! to `mem_shapes.rs`, so the numbers here and the numbers in
//! `design/DESIGN-STREAM-EXEC.md` §2.1 are comparable.
//!
//! # What is asserted
//!
//! The **slope**, not the level. Each shape is run at `SMALL` and at
//! `BIG = 16 × SMALL` rows, and the assertion is that
//! `(held_big − held_small) / (BIG − SMALL)` — bytes held per additional input
//! row — is under [`FLAT_SLOPE`] bytes, against a measured 317.8 before. A
//! slope test cancels every fixed cost (the open, the plan, the reader slot),
//! which is what makes it robust enough to keep in CI on a machine nobody has
//! profiled.
//!
//! [`materialising_select_is_the_control`] asserts the *opposite* of the same
//! quantity for a plain `SELECT`: its slope must be LARGE. Without that, a
//! harness that silently measured nothing would pass every assertion in this
//! file. `mem_shapes.rs` made the same argument with its four flat O(1) shapes.
//!
//! # Why the tests here serialize themselves
//!
//! The counter is process-global and this binary installs a `#[global_allocator]`
//! over it, so a second test allocating concurrently is charged into the same
//! peak. Every test takes [`MEASURING`] for its whole body, which makes the
//! numbers correct under the default parallel harness — no `--test-threads 1`
//! for anyone who just runs `cargo test`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use mpedb::{Config, Database, ExecResult, Value};

// ------------------------------------------------------------- the allocator

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
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let d = new - l.size();
                PEAK.fetch_max(LIVE.fetch_add(d, Relaxed) + d, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            PEAK.fetch_max(LIVE.fetch_add(l.size(), Relaxed) + l.size(), Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// One measurement at a time: the peak counter is process-global, so two
/// concurrent tests would each be charged the other's allocations. Taken for
/// the whole body of every test in this file, fixture loading included.
static MEASURING: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASURING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset the peak to the CURRENT live total, immediately before the measured
/// statement, so the figure is that statement's marginal hold and not the
/// fixture's.
fn arm() -> usize {
    let live = LIVE.load(Relaxed);
    PEAK.store(live, Relaxed);
    live
}

fn held_since(live_before: usize) -> usize {
    PEAK.load(Relaxed).saturating_sub(live_before)
}

// ----------------------------------------------------------------- fixture

const SMALL: usize = 1_000;
const BIG: usize = 16_000;

/// Bytes of simultaneous hold per ADDITIONAL input row that still counts as
/// flat. The materialising path measured **317.8** on the six-column fixture
/// in `mem_shapes.rs` (and measures ~300 here — see the control); a fold holds
/// nothing per row, so anything in single digits is the streaming regime with
/// room to spare for allocator behaviour that is not this crate's.
const FLAT_SLOPE: f64 = 8.0;

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

fn scratch_dir() -> &'static str {
    for d in ["/mnt/xfs/mpedb-scratch", "/mnt/ext4/mpedb-scratch"] {
        if std::fs::create_dir_all(d).is_ok() {
            return d;
        }
    }
    if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    }
}

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The `mem_shapes.rs` fixture, six columns wide: five `int64` and one
/// ~21-character text, ~61 bytes of user payload per row. The path carries a
/// counter as well as the pid: several tests in this binary build the same row
/// count, and a shared path would collide on the PK.
fn load(rows: usize) -> Tmp {
    let path = format!(
        "{}/mpedb-aggmem-{}-{rows}-{}.mpedb",
        scratch_dir(),
        std::process::id(),
        UNIQ.fetch_add(1, Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let size_mb = 64 + rows / 4_000 * 8;
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = {size_mb}\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = 0\n\n\
         [[table]]\nname = \"src\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g10\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"b\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    // Inside a WriteSession, and NOT via `Database::query` per chunk: every
    // distinct statement text publishes a distinct plan to the shared registry,
    // and `mem_shapes.rs` §1.1 records what that did to its first measurement.
    let mut w = db.begin().unwrap();
    let mut i = 0;
    while i < rows {
        let end = (i + 500).min(rows);
        let vals: Vec<String> = (i..end)
            .map(|k| format!("({k}, {k}, {}, {k}, {k}, 'payload text row {k}')", k % 10))
            .collect();
        w.query(
            &format!("INSERT INTO src (id, g, g10, a, b, t) VALUES {}", vals.join(", ")),
            &[],
        )
        .unwrap();
        i = end;
    }
    w.commit().unwrap();
    Tmp { db, path }
}

/// Peak simultaneous hold of ONE `execute(hash, params)` — the documented hot
/// path. Measuring `query(sql, …)` instead would recompile and re-register the
/// plan inside the window, which is O(the sys keyspace) and would put a false
/// linear floor under every number (`mem_shapes.rs` §1.1, correction 1).
fn held(t: &Tmp, sql: &str) -> (usize, usize) {
    let hash = t.db.prepare(sql).unwrap();
    let before = arm();
    let res = t.db.execute(&hash, &[]).unwrap();
    let peak = held_since(before);
    let out = match res {
        ExecResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    };
    (peak, out)
}

/// Bytes held per additional input row, between `SMALL` and `BIG`.
fn slope(small: usize, big: usize) -> f64 {
    (big as f64 - small as f64) / (BIG - SMALL) as f64
}

#[test]
fn aggregate_hold_is_flat_in_the_input_size() {
    let _m = measuring();
    let s = load(SMALL);
    let b = load(BIG);

    // (label, sql, expected output rows at BIG)
    let shapes: &[(&str, &str, usize)] = &[
        // The headline: one integer out, the whole table in.
        ("count", "SELECT count(*) FROM src", 1),
        // The same, through a residual filter pushed into the streamed scan.
        ("count_where", "SELECT count(*) FROM src WHERE b >= 0", 1),
        // A PK RANGE, so the resume bound has a `hi` to respect too.
        ("count_range", "SELECT count(*) FROM src WHERE id >= 10", 1),
        // Every fold, at once.
        (
            "folds",
            "SELECT count(*), count(a), sum(a), avg(a), min(a), max(a), total(a) FROM src",
            1,
        ),
        // GROUP BY over ten groups: 50.8 MB for ten rows, before.
        ("agg_few", "SELECT g10, sum(a), count(*) FROM src GROUP BY g10", 10),
        ("agg_few_having", "SELECT g10, count(*) FROM src GROUP BY g10 HAVING count(*) > 1", 10),
        // A BOUNDED distinct set: `count(DISTINCT …)` is O(distinct) by nature,
        // which is 10 here — but it used to hold the ROWS as well, and that half
        // is what streaming removes.
        ("count_distinct", "SELECT count(DISTINCT g10) FROM src", 1),
        // The bare-column witness (sqlite's lowest-rowid pick) keeps one row per
        // group, not one row per input row.
        ("bare_cols", "SELECT t, max(a) FROM src", 1),
    ];

    let mut report = String::new();
    for (label, sql, want_out) in shapes {
        let (hs, _) = held(&s, sql);
        let (hb, ob) = held(&b, sql);
        let m = slope(hs, hb);
        report.push_str(&format!(
            "  {label:<16} held@{SMALL}={hs:>9}  held@{BIG}={hb:>9}  B/row={m:>8.2}\n"
        ));
        assert_eq!(ob, *want_out, "`{sql}` produced {ob} rows, expected {want_out}");
        assert!(
            m < FLAT_SLOPE,
            "`{label}` holds {m:.1} bytes per input row — a fold must be O(groups), \
             not O(rows). 16x the rows moved the peak from {hs} to {hb}.\n{report}"
        );
    }
    eprintln!("aggregate hold (streaming fold):\n{report}");
}

/// The control, and the reason to believe the assertions above.
///
/// A plain `SELECT` of the same six columns materialises `ExecResult::Rows` by
/// construction — its answer IS O(n) — so its slope must be LARGE. If this
/// assertion ever fails the instrument has stopped measuring, and every "flat"
/// verdict above is vacuous.
#[test]
fn materialising_select_is_the_control() {
    let _m = measuring();
    let s = load(SMALL);
    let b = load(BIG);
    let sql = "SELECT id, g, g10, a, b, t FROM src";
    let (hs, _) = held(&s, sql);
    let (hb, _) = held(&b, sql);
    let m = slope(hs, hb);
    eprintln!("control (materialising SELECT): held@{SMALL}={hs} held@{BIG}={hb} B/row={m:.2}");
    assert!(
        m > 100.0,
        "the control must be visibly O(n) or the harness is measuring nothing: \
         {m:.1} B/row ({hs} -> {hb})"
    );
}

/// `agg_many` — one group per input row — is the shape #123 §4.4 says
/// streaming does NOT fix: the group map is O(groups) = O(rows) and no chunk
/// size changes that. Asserted so nobody reads the two tests above as a claim
/// about all aggregates. What streaming removed here is only the input's own
/// residency, measured at ~30% of the 1049 B/row this shape held before.
#[test]
fn unbounded_group_by_is_still_linear_and_says_so() {
    let _m = measuring();
    let s = load(SMALL);
    let b = load(BIG);
    let sql = "SELECT g, sum(a), count(*) FROM src GROUP BY g";
    let (hs, _) = held(&s, sql);
    let (hb, ob) = held(&b, sql);
    let m = slope(hs, hb);
    eprintln!("agg_many (one group per row): held@{SMALL}={hs} held@{BIG}={hb} B/row={m:.2}");
    assert_eq!(ob, BIG, "one group per row");
    assert!(
        m > FLAT_SLOPE,
        "an unbounded GROUP BY holds O(groups); if this ever went flat the group \
         map stopped being held and the result is wrong: {m:.1} B/row"
    );
    // …but it must have SHED the input. 1049.4 B/row was the measured figure
    // with the input materialised too; the group map and the output alone are
    // well under that.
    assert!(
        m < 900.0,
        "the input should no longer be held on top of the group map: {m:.1} B/row \
         against 1049.4 before"
    );
}

/// A sanity check that the numbers above are about the right query: the
/// streamed fold and the materialising fold must agree to the value.
#[test]
fn streamed_and_materialised_folds_agree() {
    let _m = measuring();
    let b = load(SMALL);
    let read = match b.db.query("SELECT count(*), sum(a), min(a), max(a) FROM src", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    // The same statement inside a write session takes the materialising path
    // (`TxnCtx::scans_incrementally` is false there).
    let mut w = b.db.begin().unwrap();
    let written = match w.query("SELECT count(*), sum(a), min(a), max(a) FROM src", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    w.rollback();
    assert_eq!(read, written);
    assert_eq!(
        read,
        vec![vec![
            Value::Int(SMALL as i64),
            Value::Int((SMALL as i64 - 1) * SMALL as i64 / 2),
            Value::Int(0),
            Value::Int(SMALL as i64 - 1),
        ]]
    );
}
