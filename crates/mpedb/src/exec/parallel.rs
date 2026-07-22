//! **Adaptive intra-query read parallelism** for the aggregate fold —
//! design/DESIGN-PARALLEL-READ.md §8's morsel scheduling, built.
//!
//! # The shape of the decision
//!
//! There is **no compile-time gate and no row estimate**. The calling thread is
//! worker 0: it starts folding the statement's lowest key range immediately,
//! through the ordinary serial code, and a short query finishes there having
//! engaged nothing at all (the cost is one row counter and one comparison per
//! batch). Only a scan that proves long at run time — [`PROBE_ROWS`] rows
//! folded and still not exhausted — hands its REMAINING key range to a morsel
//! queue, spawns helpers, and keeps pulling morsels itself. The data decides,
//! after the fact, on evidence.
//!
//! That ordering is what makes this safe to leave on. A gate ("parallel iff
//! estimated rows ≥ N") is a prediction, and it predicts exactly the quantity
//! the planner refuses to guess — the UNKNOWN selectivity class. A mispredicted
//! gate either eats the full serial cost on a query it called small or pays the
//! thread-startup tax on one it called big. The probe cannot mispredict: by the
//! time any thread is spawned, [`PROBE_ROWS`] rows of evidence say the scan is
//! long, and the spawn cost is already a rounding error against the work
//! ALREADY done.
//!
//! Helpers only ever do work worker 0 would have done anyway (morsels are
//! disjoint), so there is nothing wasted and nothing to cancel. The rejected
//! alternative — run serial and parallel and race — burns 2× CPU to one winner;
//! in a single-process engine idle cores are free, but **mpedb's whole
//! differentiator is many processes on one file**, so a racing query steals
//! cores from the other processes' requests.
//!
//! # Why every observable is the serial fold's
//!
//! - **Same snapshot, one pin.** Workers share the statement's own [`ReadTxn`]
//!   ([`TxnCtx::snapshot_txn`]): same `txn_id`, same meta, same tree roots,
//!   ZERO extra reader slots — the same-snapshot guarantee is structural, not
//!   protocol-negotiated. Scoped threads bound every worker by the leader's
//!   borrow, so a worker cannot outlive the pin.
//! - **Same rows, same per-row semantics.** The remainder is cut at B+tree
//!   separator keys ([`ReadTxn::partition_range`] — deterministic for a
//!   snapshot, no row visit, no charge), and each worker drains its contiguous
//!   morsel through the SERIAL machinery itself (`BatchScan` + the one
//!   [`Folder`] row body, or the fused `fold_range_column` loop), so masking,
//!   residual filtering, 3VL, collations and NULL rules cannot drift. There is
//!   still no second row-processing implementation.
//! - **Same meter.** Workers charge the statement's own `WorkMeter`; a
//!   completed parallel fold has charged exactly the serial total, in a
//!   different interleaving.
//! - **Merged in key order.** Morsels are merged by index, and the leader's
//!   probe prefix is the first segment — so every first-wins rule (a min/max
//!   tie's spelling, an integer sum's overflow point) reproduces the serial
//!   answer.
//! - **Any mid-flight error abandons.** A budget refusal, a per-row raise,
//!   eviction, a defensive invariant break: the coordinator rewinds the meter
//!   to its pre-hand-off checkpoint and answers `false`, and the CALLER simply
//!   keeps folding the remainder serially from where its own scan stood, into
//!   the accumulators it never gave up. The statement then charges the serial
//!   total in the serial order and trips at the serial row — the #74 refusal
//!   point does not move.
//!
//! # The mpedb-specific cost: fan-out width must be budgeted
//!
//! DuckDB never thinks about this; we must. Helper count is bounded by the
//! `[runtime] max_query_threads` knob, by the free cores, by the parallel
//! workers this PROCESS already has in flight, and by the reader census on the
//! file — other live pins are other requests, and a greedy analytical query
//! must not starve them. A helper that sees another engagement denied for want
//! of budget ([`PRESSURE`]) finishes its current morsel and yields back.
//! Correctness never depends on a helper running at all: the leader drains
//! whatever the queue still holds.

use super::aggregate::{mint_accum, Acc, Folder, PartAcc};
use super::gather::BatchScan;
use super::*;
use mpedb_core::{FoldOpts, ReadTxn};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomOrd};

/// Statements whose parallel fold ENGAGED (helpers actually spawned) in this
/// process — the one deliberate observable ([`crate::parallel_folds_engaged`]).
/// It exists because the design makes everything ELSE indistinguishable: the
/// invariance batteries need to assert that the path they exercise really ran.
/// Monotone; never reset.
pub(crate) static ENGAGED: AtomicU64 = AtomicU64::new(0);

/// Helper threads this process has in flight across ALL statements. The budget
/// is per process, not per query: eight concurrent aggregates must not each
/// claim the machine.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Bumped whenever an engagement wanted helpers and the budget had none left.
/// Running helpers watch it and wind down at their next morsel boundary, which
/// is how the budget is handed back to a waiting query without preemption.
static PRESSURE: AtomicU64 = AtomicU64::new(0);

/// Rows the leader folds before it will consider handing off the remainder.
///
/// The floor is set by what a hand-off costs (a structural cut of the
/// remainder, a thread spawn per helper, one merge: tens of microseconds) and
/// the ceiling by Amdahl (this prefix is serial). 32 768 rows is ~5 ms of the
/// heavier folds on this box: three orders of magnitude above the hand-off
/// cost, and 3 % of a 1 M-row scan. `MPEDB_PAR_PROBE_ROWS=<n>` overrides it —
/// the differential batteries set 1 so a small fixture exercises the workers
/// (the `MPEDB_FOLD_BATCH` precedent).
fn probe_rows() -> u64 {
    static OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("MPEDB_PAR_PROBE_ROWS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(32_768)
    })
}

/// Morsels per worker. Four is §2's number: enough that a slow core's morsel
/// is stolen rather than gating the wall clock, few enough that the per-morsel
/// tree descent stays noise.
const MORSELS_PER_WORKER: usize = 4;

/// Cores available to this process, cached (the query resolves it only at a
/// hand-off, but a hand-off is on the hot path of a long scan).
fn cores() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

/// The statement's parallel eligibility, decided ONCE before the fold starts.
/// Building it must stay nearly free — it is on the path of every aggregate,
/// including the ones that will never engage — so it reads a bool, a virtual
/// call and a `u32`, and nothing else. Every expensive question (cores, the
/// reader census, the tree's cut points) waits for the probe to prove the scan
/// long.
pub(super) struct ParPlan {
    /// Rows the leader folds before considering a hand-off.
    probe: u64,
    /// The meter reading when the statement's fold began — the baseline of the
    /// "have I folded enough to hand off?" test on the general path, where the
    /// batch loop counts KEPT rows but the meter counts VISITED ones (a
    /// selective residual must not hide a long scan).
    start_used: u64,
}

impl ParPlan {
    /// The leader's probe cap for a fused fold.
    pub(super) fn probe_cap(&self) -> u64 {
        self.probe
    }

    /// Has the leader folded enough to consider handing off the remainder?
    pub(super) fn probe_reached(&self, ctx: &dyn TxnCtx) -> bool {
        ctx.snapshot_txn()
            .is_some_and(|t| t.work_used().saturating_sub(self.start_used) >= self.probe)
    }
}

/// Is this statement eligible to hand a long tail to worker threads? `None`
/// keeps every serial path byte-identical to what it was.
pub(super) fn admit(ctx: &dyn TxnCtx, parallel_shape: bool) -> Option<ParPlan> {
    if !parallel_shape {
        return None;
    }
    let txn = ctx.snapshot_txn()?;
    if txn.max_query_threads() == 1 {
        return None; // configured serial
    }
    Some(ParPlan { probe: probe_rows(), start_used: txn.work_used() })
}

/// How many helpers may this engagement spawn — the whole §8 budget: the
/// configured ceiling, the cores, this process's in-flight helpers, and the
/// file's reader census (other live pins are other requests, possibly other
/// PROCESSES, and they are why an embedded engine may not simply take the
/// machine). Reserves what it returns; the caller releases it.
fn reserve_helpers(txn: &ReadTxn<'_>, morsels: usize) -> usize {
    let knob = txn.max_query_threads();
    let workers = match knob {
        // Auto: the M3 curve's knee is at 8 (n=8→11 bought 17 % for 37 % more
        // threads), and an embedded library should not commandeer every core
        // of its host by default.
        0 => cores().min(8),
        n => n as usize,
    };
    // Other readers on the file: their requests want cores too. Our own pin is
    // one of them, hence the -1.
    let others = txn.live_readers().saturating_sub(1) as usize;
    let want = workers
        .saturating_sub(1)
        .min(morsels.saturating_sub(1))
        .min(cores().saturating_sub(1 + others));
    if want == 0 {
        return 0;
    }
    let cap = cores().saturating_sub(1);
    let mut cur = ACTIVE.load(AtomOrd::Relaxed);
    loop {
        let take = want.min(cap.saturating_sub(cur));
        if take == 0 {
            // Someone else has the machine. Say so: their helpers wind down at
            // their next morsel boundary, and the next statement gets a turn.
            PRESSURE.fetch_add(1, AtomOrd::Relaxed);
            return 0;
        }
        match ACTIVE.compare_exchange_weak(cur, cur + take, AtomOrd::Relaxed, AtomOrd::Relaxed) {
            Ok(_) => return take,
            Err(c) => cur = c,
        }
    }
}

/// One morsel's bounds.
type Range = (Option<RawBound>, Option<RawBound>);

/// Cut `(lo, hi)` at the tree's separator keys into contiguous, collectively
/// exhaustive pieces, in key order.
fn ranges_from(lo: Option<RawBound>, hi: Option<RawBound>, points: &[Vec<u8>]) -> Vec<Range> {
    let mut out = Vec::with_capacity(points.len() + 1);
    let mut cur_lo = lo;
    for p in points {
        out.push((cur_lo.take(), Some((p.clone(), false))));
        cur_lo = Some((p.clone(), true));
    }
    out.push((cur_lo, hi));
    out
}

/// The work-stealing queue: morsels in key order, pulled by index.
struct Queue {
    ranges: Vec<Range>,
    next: AtomicUsize,
    /// Set when the coordinator has already failed — running workers stop
    /// pulling instead of finishing a fold nobody will read.
    abort: AtomicBool,
}

impl Queue {
    fn take(&self) -> Option<(usize, &Range)> {
        if self.abort.load(AtomOrd::Relaxed) {
            return None;
        }
        let i = self.next.fetch_add(1, AtomOrd::Relaxed);
        self.ranges.get(i).map(|r| (i, r))
    }
}

/// One morsel's result: its index (= key order) and its accumulators, or
/// `None` for a morsel that held no rows (it merges as the identity).
type Part = (usize, Option<Vec<PartAcc>>);

/// Run the morsel queue over `drain`, on the calling thread plus `helpers`
/// spawned ones, and merge the partials into `accs` in KEY ORDER.
///
/// `Ok(true)` — merged, `accs` is now the whole scan's. `Ok(false)` — nothing
/// was merged, the meter is rewound, and `accs` is untouched: the caller folds
/// the remainder serially, as if the hand-off had never been attempted.
fn run<D>(txn: &ReadTxn<'_>, accs: &mut [Acc], ranges: Vec<Range>, drain: D) -> Result<bool>
where
    D: Fn(&Range) -> Result<Option<Vec<PartAcc>>> + Sync,
{
    let helpers = reserve_helpers(txn, ranges.len());
    if helpers == 0 {
        return Ok(false); // no budget: the caller stays serial, having lost nothing
    }
    // Panic-safe release: a panic inside the fold unwinds through
    // `thread::scope`, and a leaked reservation would silently shrink this
    // process's parallelism for the rest of its life.
    struct Reserved(usize);
    impl Drop for Reserved {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(self.0, AtomOrd::Relaxed);
        }
    }
    let _reserved = Reserved(helpers);
    ENGAGED.fetch_add(1, AtomOrd::Relaxed);
    // Everything past this point may abandon; the rewind target is the meter
    // AS THE LEADER LEFT IT, so the serial continuation charges from there.
    let checkpoint = txn.work_checkpoint();
    let queue = Queue { ranges, next: AtomicUsize::new(0), abort: AtomicBool::new(false) };
    let pressure0 = PRESSURE.load(AtomOrd::Relaxed);

    let worker = |yielding: bool| -> Result<Vec<Part>> {
        let mut out = Vec::new();
        while let Some((i, r)) = queue.take() {
            match drain(r) {
                Ok(p) => out.push((i, p)),
                Err(e) => {
                    queue.abort.store(true, AtomOrd::Relaxed);
                    return Err(e);
                }
            }
            // A helper hands the machine back the moment another statement was
            // denied budget. The leader never yields: it owns the queue's
            // completion, so correctness never depends on scheduling.
            if yielding && PRESSURE.load(AtomOrd::Relaxed) != pressure0 {
                break;
            }
        }
        Ok(out)
    };

    let collected: Result<Vec<Part>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..helpers).map(|_| s.spawn(|| worker(true))).collect();
        // The leader drains too — and drains LAST-resort: whatever the helpers
        // did not take, it takes.
        let mut all = worker(false);
        for h in handles {
            match h.join() {
                Err(_) => all = Err(internal("a parallel fold worker panicked")),
                Ok(Err(e)) => {
                    if all.is_ok() {
                        all = Err(e);
                    }
                }
                Ok(Ok(part)) => {
                    if let Ok(v) = &mut all {
                        v.extend(part);
                    }
                }
            }
        }
        all
    });

    let mut parts = match collected {
        Ok(p) => p,
        Err(_) => {
            txn.work_rewind(checkpoint);
            return Ok(false);
        }
    };
    // Key order is morsel order: sort by index and fold each morsel into the
    // leader's prefix, left to right.
    parts.sort_by_key(|(i, _)| *i);
    if parts.len() != queue.ranges.len() {
        // Defensive: a morsel that neither thread reported cannot be merged
        // into a complete answer. Abandon rather than answer from a subset.
        txn.work_rewind(checkpoint);
        return Ok(false);
    }
    for (_, part) in parts {
        let Some(part) = part else { continue };
        if part.len() != accs.len() {
            txn.work_rewind(checkpoint);
            return Ok(false);
        }
        for (a, b) in accs.iter_mut().zip(part) {
            if a.merge_part(b).is_err() {
                txn.work_rewind(checkpoint);
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Cut `(lo, hi)` into morsels, or `None` when the tree offers no cut inside
/// it — a remainder living in one leaf is not worth a thread, and saying so
/// structurally needs no row estimate.
fn morsels(
    txn: &ReadTxn<'_>,
    table: u32,
    lo: &Option<RawBound>,
    hi: &Option<RawBound>,
) -> Option<Vec<Range>> {
    let workers = match txn.max_query_threads() {
        0 => cores().min(8),
        n => n as usize,
    };
    let want = workers.max(2) * MORSELS_PER_WORKER;
    let points = txn
        .partition_range(
            table,
            lo.as_ref().map(|(k, _)| k.as_slice()),
            hi.as_ref().map(|(k, _)| k.as_slice()),
            want,
        )
        .ok()?;
    if points.is_empty() {
        return None;
    }
    Some(ranges_from(lo.clone(), hi.clone(), &points))
}

/// Hand the remainder of a FUSED fold (`ReadTxn::fold_range_column`: one bare
/// column, no residual, no grouping) to the morsel queue. `Ok(false)` = the
/// caller drains `(lo, hi)` serially itself.
pub(super) fn tail_fused(
    ctx: &dyn TxnCtx,
    accs: &mut [Acc],
    table: u32,
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    col: u16,
    agg: &Aggregation,
) -> Result<bool> {
    let Some(txn) = ctx.snapshot_txn() else {
        return Ok(false);
    };
    let Some(ranges) = morsels(txn, table, &lo, &hi) else {
        return Ok(false);
    };
    let has_arg: Vec<bool> = agg.aggs.iter().map(|c| c.arg.is_some()).collect();
    run(txn, accs, ranges, |(rlo, rhi)| {
        // Host registries are deliberately absent (the gate proved every
        // per-row program host-free), and `par_sum` mints the segment monoid
        // for integer `sum`. Everything else is the serial fused loop.
        let mut a = agg
            .aggs
            .iter()
            .map(|c| mint_accum(c, None, true))
            .collect::<Result<Vec<Acc>>>()?;
        txn.fold_range_column(
            table,
            rlo.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            rhi.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            col,
            FoldOpts::worker(),
            &mut |v| {
                for (acc, has) in a.iter_mut().zip(&has_arg) {
                    acc.push(if *has { Some(v) } else { None })?;
                }
                Ok(())
            },
        )?;
        a.into_iter().map(PartAcc::demote).collect::<Result<Vec<_>>>().map(Some)
    })
}

/// Hand the remainder of the GENERAL fold (`BatchScan` + [`Folder`]: a
/// residual, a computed argument, a per-aggregate FILTER) to the morsel queue.
/// `Ok(false)` = the caller's own scan keeps going, serially, from where it
/// stands.
#[allow(clippy::too_many_arguments)]
pub(super) fn tail_general(
    ctx: &dyn TxnCtx,
    accs: &mut [Acc],
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    t: &TableDef,
    table: u32,
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    filter: Option<&ExprProgram>,
    agg: &Aggregation,
    keep: Option<&[bool]>,
    width: usize,
) -> Result<bool> {
    let Some(txn) = ctx.snapshot_txn() else {
        return Ok(false);
    };
    let Some(ranges) = morsels(txn, table, &lo, &hi) else {
        return Ok(false);
    };
    run(txn, accs, ranges, |(rlo, rhi)| {
        let mut wctx = ReadCtx(txn, None, None, None, ChargeMode::Batched);
        let mut folder = Folder::new(&wctx, schema, plan, t, table, &[], agg);
        folder.parallelize();
        let mut scan = BatchScan::open_partition(
            &wctx,
            table,
            rlo.clone(),
            rhi.clone(),
            width,
            keep.map(|k| k.to_vec()),
        );
        loop {
            let batch = scan.next(&mut wctx, filter, params)?;
            if batch.is_empty() {
                break;
            }
            for row in &batch {
                folder.push(&wctx, row, params)?;
            }
        }
        match folder.into_single_group() {
            None => Ok(None),
            Some(a) => a.into_iter().map(PartAcc::demote).collect::<Result<Vec<_>>>().map(Some),
        }
    })
}
