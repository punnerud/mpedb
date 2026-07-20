//! **The width claim of #125, as an assertion rather than a benchmark.**
//!
//! `design/DESIGN-STREAM-EXEC.md` §9.1 measured two shapes that the streaming
//! aggregate left completely flat — an aggregate over a JOIN at **753.4 B per
//! input row**, and an aggregate under a correlated `FILTER` at **566.6** —
//! and §9.3 explained why: those paths materialise, and the fold is still
//! handed WIDE rows. Nothing told the join which columns anything downstream
//! reads.
//!
//! `mpedb_sql::row_prune` now says so. This file is the invariant that keeps
//! it saying so.
//!
//! # The instrument
//!
//! Identical to `tests/agg_stream_mem.rs` and `examples/mem_shapes.rs`, on
//! purpose, so the numbers here are comparable to the ones in the design
//! document: `held` is the peak of a live-bytes counter in a wrapping
//! [`GlobalAlloc`] — every `alloc` adds, every `dealloc` subtracts, peak by
//! `fetch_max`. That is "how many heap bytes did the engine hold
//! SIMULTANEOUSLY", which is exactly the quantity column pruning moves, and it
//! is deterministic: no allocator quantisation, no sampling race, no mmap'd
//! file pages.
//!
//! # What is asserted
//!
//! The **slope**, not the level — bytes held per additional input row, between
//! `SMALL` and `BIG = 16 × SMALL`. A slope cancels every fixed cost (the open,
//! the plan, the reader slot), which is what makes it robust on a machine
//! nobody has profiled.
//!
//! Each shape carries the figure it was measured at, before and after, and a
//! ceiling with room for allocator behaviour that is not this crate's.
//! [`the_unpruned_join_is_the_control`] asserts the OPPOSITE for a join that
//! projects everything: its slope must stay LARGE. Without that, a harness
//! that silently measured nothing would pass every assertion in this file.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use mpedb::{Config, Database, ExecResult};

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
            if new >= l.size() {
                let d = new - l.size();
                PEAK.fetch_max(LIVE.fetch_add(d, Relaxed) + d, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// One measurement at a time: the peak counter is process-global, so two
/// concurrent tests would each be charged the other's allocations.
static MEASURING: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASURING.lock().unwrap_or_else(|e| e.into_inner())
}

fn arm() -> usize {
    let live = LIVE.load(Relaxed);
    PEAK.store(live, Relaxed);
    live
}

// ----------------------------------------------------------------- fixture

const SMALL: usize = 1_000;
const BIG: usize = 16_000;

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

/// `mem_shapes.rs`'s join pair, plus its six-column workhorse.
///
/// - `src` — the wide single table (`id, g, g10, a, b, t`), for the correlated
///   shape.
/// - `small` (10 rows) × `dim` (`rows` rows) — the join pair, with every
///   `dim.k` matching a `small.k`, so the product is `rows` rows of
///   `[small ‖ dim]` = 5 slots and one ~19-character `dim.label`. That is the
///   relation `SELECT count(*)` over a join holds today.
fn load(rows: usize) -> Tmp {
    let path = format!(
        "{}/mpedb-prunemem-{}-{rows}-{}.mpedb",
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
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"small\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\n\n\
         [[table]]\nname = \"dim\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"label\"\ntype = \"text\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let mut w = db.begin().unwrap();
    let vals: Vec<String> = (0..10).map(|k| format!("({k}, {k})")).collect();
    w.query(&format!("INSERT INTO small (id, k) VALUES {}", vals.join(", ")), &[]).unwrap();
    let mut i = 0;
    while i < rows {
        let end = (i + 500).min(rows);
        let s: Vec<String> = (i..end)
            .map(|k| format!("({k}, {k}, {}, {k}, {k}, 'payload text row {k}')", k % 10))
            .collect();
        w.query(&format!("INSERT INTO src (id, g, g10, a, b, t) VALUES {}", s.join(", ")), &[])
            .unwrap();
        let d: Vec<String> = (i..end)
            .map(|k| format!("({k}, {}, 'dimension label {k}')", k % 10))
            .collect();
        w.query(&format!("INSERT INTO dim (id, k, label) VALUES {}", d.join(", ")), &[]).unwrap();
        i = end;
    }
    w.commit().unwrap();
    Tmp { db, path }
}

/// Peak simultaneous hold of ONE `execute(hash, params)` — the documented hot
/// path. Measuring `query(sql, …)` instead would recompile and re-register the
/// plan inside the window (`mem_shapes.rs` §1.1, correction 1).
fn held(t: &Tmp, sql: &str) -> (usize, usize) {
    let hash = t.db.prepare(sql).unwrap();
    let before = arm();
    let res = t.db.execute(&hash, &[]).unwrap();
    let peak = PEAK.load(Relaxed).saturating_sub(before);
    let out = match res {
        ExecResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    };
    (peak, out)
}

fn slope(small: usize, big: usize) -> f64 {
    (big as f64 - small as f64) / (BIG - SMALL) as f64
}

// ---------------------------------------------------------------- the claims

/// `(label, sql, expected output rows, B/row BEFORE #125, ceiling)`
///
/// The "before" column was measured on THIS fixture with THIS instrument, by
/// making `row_prune` answer `None` unconditionally and re-running — not
/// copied from the design document, which measured a fixture this file does
/// not reproduce column for column. The one exception is
/// `count_over_wide_join`, which lands on **753.4** to the decimal and is
/// therefore the §9.1 row itself.
///
/// The ceilings sit ~15-25% above the measured figure: enough room for
/// allocator behaviour that is not this crate's, tight enough that losing a
/// stage of the mask fails the test rather than merely embarrassing it.
const SHAPES: &[(&str, &str, usize, f64, f64)] = &[
    // ---- the two shapes DESIGN-STREAM-EXEC §9.1 recorded as UNCHANGED ------
    //
    // An aggregate over a join, on the six-column table self-joined by its
    // primary key: a 12-slot product, one row per input row. `count(*)`
    // observes NO column of it, so the last stage's tuple is EMPTY and the
    // outer relation keeps only the ON's key — one slot of six.
    //
    // 753.4 -> 112.8 B/row, and 753.4 is §9.1's own figure.
    ("count_over_wide_join", "SELECT count(*) FROM src a JOIN src b ON a.id = b.id", 1, 753.4, 170.0),
    // The narrow pair (`small` × `dim`), where the product is 5 slots to begin
    // with and the held inner side is what dominates. 378.0 -> 145.2.
    ("count_over_join", "SELECT count(*) FROM small, dim WHERE small.k = dim.k", 1, 378.0, 200.0),
    // The same join aggregated over a column: the argument pins one more slot,
    // and the text label still goes. 378.0 -> 241.2.
    ("sum_over_join", "SELECT sum(dim.k) FROM small, dim WHERE small.k = dim.k", 1, 378.0, 300.0),
    // An aggregate under a correlated FILTER — §9.1's other flat row.
    // `correlated_survivors` keeps a per-row scratch beside every gathered row,
    // so the whole six-column input is resident; the correlation names ONE
    // column and the aggregate none. 406.6 -> 225.2.
    //
    // (§9.1 reports 566.6 for "count(*) FILTER (correlated EXISTS)" over a
    // fixture whose exact query this file could not reconstruct; 406.6 is what
    // the same SHAPE measures here, unpruned, and is the honest baseline for
    // the ratio.)
    (
        "correlated_filter",
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM small WHERE small.k = src.g10)) \
         FROM src",
        1,
        406.6,
        290.0,
    ),
    // A correlated WHERE residual — the `post_filter` — over the same input.
    (
        "correlated_where",
        "SELECT count(*) FROM src WHERE EXISTS (SELECT 1 FROM small WHERE small.k = src.g10)",
        1,
        406.6,
        290.0,
    ),
    // ---- and the shape that shows where this technique STOPS ---------------
    //
    // A join projecting two of five slots, one of them the LAST. 378.0 -> 346.0,
    // a 9% win and no more: the plan's column indices are absolute in the
    // joined tuple, so a slot below the highest observed one can only be NULLed
    // out, not removed. Positional pruning frees PAYLOAD (the text that is no
    // longer carried) and the TAIL; compacting the middle would mean remapping
    // every index in the plan, which is a different change. This row is here so
    // that limit is measured rather than assumed.
    (
        "narrow_join_projection",
        "SELECT small.id, dim.label FROM small, dim WHERE small.k = dim.k",
        BIG,
        378.0,
        365.0,
    ),
];

#[test]
fn the_output_requirement_bounds_what_the_pipeline_holds() {
    let _m = measuring();
    let s = load(SMALL);
    let b = load(BIG);

    let mut report = String::new();
    for (label, sql, want_out, before, ceiling) in SHAPES {
        let (hs, _) = held(&s, sql);
        let (hb, ob) = held(&b, sql);
        let m = slope(hs, hb);
        report.push_str(&format!(
            "  {label:<24} held@{SMALL}={hs:>9}  held@{BIG}={hb:>9}  B/row={m:>8.1}  \
             (before {before:.1})\n"
        ));
        assert_eq!(ob, *want_out, "`{sql}` produced {ob} rows, expected {want_out}");
        assert!(
            m < *ceiling,
            "`{label}` holds {m:.1} bytes per input row, over the {ceiling:.1} ceiling \
             (it held {before:.1} before #125). 16x the rows moved the peak from {hs} to \
             {hb}.\n{report}"
        );
    }
    eprintln!("width pruning (bytes held per input row):\n{report}");
}

/// The control, and the reason to believe the assertions above.
///
/// `SELECT *` over the same join observes EVERY column, so `row_prune` answers
/// `None` and the executor's path is byte-for-byte what it was. Its slope must
/// therefore stay large — if this ever goes flat the instrument has stopped
/// measuring and every verdict above is vacuous.
#[test]
fn the_unpruned_join_is_the_control() {
    let _m = measuring();
    let s = load(SMALL);
    let b = load(BIG);
    let sql = "SELECT small.id, small.k, dim.id, dim.k, dim.label FROM small, dim \
               WHERE small.k = dim.k";
    let (hs, _) = held(&s, sql);
    let (hb, ob) = held(&b, sql);
    let m = slope(hs, hb);
    eprintln!("control (fully projected join): held@{SMALL}={hs} held@{BIG}={hb} B/row={m:.1}");
    assert_eq!(ob, BIG);
    assert!(
        m > 300.0,
        "the control must be visibly O(n·width) or the harness is measuring nothing: \
         {m:.1} B/row ({hs} -> {hb})"
    );
}

/// Pruning is about bytes, not about answers: every shape above must give the
/// same result as the same statement with nothing pruned. `SELECT *`-ing the
/// join and counting in the harness is the unpruned spelling of
/// `count_over_join`, and the two must agree exactly.
#[test]
fn the_pruned_and_unpruned_spellings_agree() {
    let _m = measuring();
    let t = load(SMALL);
    let pruned = match t
        .db
        .query("SELECT count(*) FROM small, dim WHERE small.k = dim.k", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
    .into_iter()
    .next()
    .unwrap();
    let full = match t
        .db
        .query(
            "SELECT small.id, small.k, dim.id, dim.k, dim.label FROM small, dim \
             WHERE small.k = dim.k",
            &[],
        )
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(pruned, vec![mpedb::Value::Int(full as i64)]);
}
