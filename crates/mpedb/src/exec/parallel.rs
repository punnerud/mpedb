//! The partitioned parallel aggregate fold — design/DESIGN-PARALLEL-READ.md's
//! BUILD verdict, scoped to the aggregates whose partition-merge is PROVEN
//! order-identical to the serial fold (`mpedb_sql::parallel_fold_shape` is the
//! single shape gate; its doc carries the per-aggregate proofs and the honest
//! refusal list — float sum/avg, total, group_concat, DISTINCT, host and
//! bare-column witnesses stay serial because their answers are order-dependent;
//! integer avg parallelizes only inside the merge-checked f64-exactness window).
//!
//! # How the observable contract is kept EXACTLY serial
//!
//! - **Same snapshot, one pin.** Workers share the statement's own
//!   [`ReadTxn`] (`TxnCtx::snapshot_txn`): same `txn_id`, same meta, same
//!   tree roots, zero extra reader slots — the same-snapshot guarantee is
//!   structural, not protocol-negotiated. Scoped threads bound every worker
//!   by the leader's borrow, so a worker can never outlive the pin.
//! - **Same rows, same per-row semantics.** The PK range is cut at B+tree
//!   separator keys ([`ReadTxn::partition_range`] — deterministic for a
//!   snapshot, no row visits, no charges); each worker drains its contiguous
//!   piece through the SERIAL machinery itself ([`BatchScan`] + the one
//!   [`Folder`] row body, or the fused [`ReadTxn::fold_range_column`] loop),
//!   so masking, residual filtering, 3VL, collations and NULL rules cannot
//!   drift — there is still no second row-processing implementation.
//! - **Same meter.** Workers charge the statement's own [`WorkMeter`]
//!   (atomic adds): a completed parallel fold has charged exactly the serial
//!   total, in a different interleaving.
//! - **Any mid-flight error abandons to serial.** Budget refusals (work-rows
//!   OR the shared group-map cell counter), per-row program raises, snapshot
//!   eviction, defensive invariant breaks — the coordinator rewinds the meter
//!   to its pre-attempt checkpoint and returns `None`, and `exec_aggregate`
//!   falls through to the serial paths as if the attempt never happened. The
//!   serial re-run then produces the authentic outcome: the same refusal at
//!   the same deterministic `used`, the same error text, the same
//!   raise-vs-budget precedence the serial scan order dictates. Bounded: the
//!   abandoned attempt did at most O(budget + a batch per worker) work.
//! - **Merge-time raises are proven, not re-run.** The one error the merge
//!   itself can produce — integer `sum` overflow, detected by the
//!   [`ParSum`] prefix monoid at finish — fires iff the serial fold's does
//!   (proof at [`ParSum`]), with the same payload-free error; no re-run
//!   needed, and none is done.
//!
//! Engagement (all must hold, or the statement folds serially, as today):
//! shape gate ∥ snapshot context ∥ `max_query_threads` resolves ≥ 2 ∥
//! estimated input ≥ ~100k rows (`MPEDB_PAR_MIN_ROWS` overrides, for the
//! invariance batteries) ∥ the tree yields at least one in-range cut.

use super::aggregate::{mint_accum, Acc, Folder, Group, SendAcc, SendGroup};
use super::gather::{scan_keep, BatchScan, JoinCells, RawBound};
use super::*;
use mpedb_core::ReadTxn;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// Statements whose parallel fold ENGAGED (workers actually spawned) in this
/// process — the one deliberate observable ([`crate::parallel_folds_engaged`]).
/// It exists because the design makes everything ELSE indistinguishable: the
/// invariance batteries need to assert the path they exercise really ran.
pub(crate) static ENGAGED: AtomicU64 = AtomicU64::new(0);

/// The engagement threshold, in estimated input rows. ~100k is where the M3
/// measurement crossed 2× (10k measured BELOW 1× at high thread counts);
/// below it the spawn/merge floor eats the win and the gate keeps the
/// statement serial. `MPEDB_PAR_MIN_ROWS=<n>` overrides — the differential
/// batteries force tiny thresholds to run the parallel path on small
/// fixtures (`MPEDB_FOLD_BATCH` is the precedent).
fn par_min_rows() -> u64 {
    static OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("MPEDB_PAR_MIN_ROWS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(100_000)
    })
}

/// Resolve the `[runtime] max_query_threads` knob: `0` = auto, `min(cores,
/// 8)`. Capped at 8 by default on purpose: the M3 curve shows the knee
/// (n=8→n=11 bought count 4.63×→5.43× — 17% for 37% more threads), an
/// embedded library should not commandeer every core of its host by default,
/// and each worker multiplies a wide GROUP BY's transient map residency.
/// On-by-default is justified the only way it can be: the answer — values,
/// ties, spellings, raises, refusal points — is PROVEN identical at every
/// thread count, so the knob is observable as wall time alone (PostgreSQL
/// ships parallel query on by default on the same grounds).
fn effective_threads(raw: u32) -> usize {
    match raw {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8),
        n => n as usize,
    }
}

/// The single bare column a FUSED worker decodes, when the statement is the
/// fused shape (ungrouped, filterless, every aggregate reading one shared
/// bare column or `count(*)`) — the parallel twin of `try_fused_fold`'s
/// admission, erring toward the general body: a `None` here only costs the
/// fused fast path, never correctness.
fn fused_col(agg: &Aggregation, filter: Option<&ExprProgram>, t: &TableDef) -> Option<u16> {
    if filter.is_some() || !agg.group_by.is_empty() || !agg.bare_cols.is_empty() {
        return None;
    }
    let mut col: Option<u16> = None;
    for c in &agg.aggs {
        if c.filter.is_some() || !c.extra_args.is_empty() || c.func.native().is_none() {
            return None;
        }
        match c.arg.as_ref().map(|p| p.instrs.as_slice()) {
            None => {}
            Some([mpedb_types::Instr::PushCol(i)]) => match col {
                None => col = Some(*i),
                Some(j) if j == *i => {}
                Some(_) => return None,
            },
            Some(_) => return None,
        }
    }
    col.filter(|c| (*c as usize) < t.columns.len())
}

/// One worker's contiguous piece of the resolved PK range.
type Range = (Option<RawBound>, Option<RawBound>);

/// Cut the resolved range at the tree's separator keys into at most `k`
/// contiguous, collectively exhaustive pieces, in key order.
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

/// Attempt the partitioned fold. `Ok(Some(groups))` is the MERGED group map,
/// byte-equivalent to the serial fold's (adopted by `exec_aggregate`'s shared
/// finish code); `Ok(None)` means "fold serially" — not engaged, or engaged
/// and abandoned with the meter rewound. `Err` is returned ONLY for the
/// proven merge-time raise (integer-sum overflow surfaces from the shared
/// finish code, not here) — everything else abandons instead, so the serial
/// re-run owns every ambiguous outcome.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_parallel_fold(
    ctx: &dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    t: &TableDef,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    agg: &Aggregation,
    prune: Option<&RowPrune>,
    parallel_shape: bool,
    width: usize,
) -> Result<Option<BTreeMap<Vec<u8>, Group>>> {
    if !parallel_shape || !ctx.scans_incrementally() {
        return Ok(None);
    }
    let Some(txn) = ctx.snapshot_txn() else {
        return Ok(None);
    };
    let threads = effective_threads(txn.max_query_threads());
    if threads < 2 {
        return Ok(None);
    }
    // Resolve the plan's bounds ONCE, exactly as `BatchScan::open` would; a
    // NULL bound is a born-empty scan the serial path answers for free.
    let (lo, hi) = match access {
        AccessPath::FullScan => (None, None),
        AccessPath::PkRange { lo, hi } => {
            match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                None => return Ok(None),
                Some(b) => b,
            }
        }
        _ => return Ok(None),
    };
    let threshold = par_min_rows();
    let (mut points, est) = txn.partition_range(
        table,
        lo.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        hi.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        threads,
    )?;
    if est < threshold || points.is_empty() {
        return Ok(None);
    }
    // Workers scale with the estimate — half a threshold each is the floor —
    // and never exceed the cuts the tree offered.
    let k = threads
        .min((est / (threshold / 2).max(1)).max(2) as usize)
        .min(points.len() + 1);
    if points.len() > k - 1 {
        let s = points.len();
        points = (1..k).map(|i| points[i * s / k].clone()).collect();
        points.dedup();
    }
    let ranges = ranges_from(lo, hi, &points);

    ENGAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Everything after this point may abandon: checkpoint the meter first.
    let checkpoint = txn.work_checkpoint();
    let cells_budget = ctx.join_cells_budget();
    let shared_cells = Arc::new(AtomicU64::new(0));
    let keep = prune.and_then(|p| scan_keep(p.stage(0), filter, width));
    let fused = fused_col(agg, filter, t);

    let folded: Result<Vec<BTreeMap<Vec<u8>, SendGroup>>> = std::thread::scope(|s| {
        let handles: Vec<_> = ranges
            .into_iter()
            .map(|(rlo, rhi)| {
                let keep = keep.clone();
                let shared_cells = shared_cells.clone();
                s.spawn(move || -> Result<BTreeMap<Vec<u8>, SendGroup>> {
                    match fused {
                        Some(col) => {
                            worker_fused(txn, table, rlo, rhi, col, agg, cells_budget, &shared_cells)
                        }
                        None => worker_fold(
                            txn, plan, params, schema, t, table, rlo, rhi, filter, agg, keep,
                            width, cells_budget, &shared_cells,
                        ),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| internal("parallel fold worker panicked"))?
            })
            .collect()
    });
    let folded = match folded {
        Ok(v) => v,
        Err(_) => {
            // Abandon: rewind the meter and let the serial paths re-run the
            // statement from the same starting charge — the authentic serial
            // outcome (success, budget refusal, or raise) follows.
            txn.work_rewind(checkpoint);
            return Ok(None);
        }
    };

    // Merge in partition (key) order. On a shared group the EARLIER
    // partition's key spelling and scratch stand (its rows came first in scan
    // order — the serial first-row pick); accumulators combine under each
    // aggregate's proven merge. A merge error abandons like a worker's.
    let promote = |(keys, accs, scratch): SendGroup| -> Group {
        (
            keys,
            accs.into_iter().map(SendAcc::promote).collect(),
            None, // the witness slot the gate proved absent
            scratch,
        )
    };
    let mut it = folded.into_iter();
    let mut merged: BTreeMap<Vec<u8>, Group> = it
        .next()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, g)| (k, promote(g)))
        .collect();
    for m in it {
        for (key, g) in m {
            match merged.entry(key) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(promote(g));
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let dst = e.get_mut();
                    for (a, b) in dst.1.iter_mut().zip(g.1) {
                        if a.merge_ordered(b.promote()).is_err() {
                            txn.work_rewind(checkpoint);
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
    // Integer `avg` is proven bit-identical only inside the f64-exactness
    // window ([`super::aggregate::ParSum::exact_window`]); an escaped merged
    // state has NO provable value, so it must abandon HERE — after adoption
    // the shared finish code could no longer fall back. Rare by construction
    // (a prefix beyond ±2⁵³ needs ~9·10¹⁵ of accumulated magnitude), and the
    // cost is one wasted parallel scan, bounded by 1/k of the serial time.
    if merged
        .values()
        .any(|g| g.1.iter().any(|a| a.avg_escaped()))
    {
        txn.work_rewind(checkpoint);
        return Ok(None);
    }
    Ok(Some(merged))
}

/// The general worker: the serial streaming fold verbatim — its own
/// [`ReadCtx`] over the SHARED txn, a [`BatchScan`] over its piece, and the
/// one [`Folder`] row body — with the two parallel twists ([`Folder::parallelize`]):
/// group-map cells charge the statement-wide shared counter, and integer
/// `sum` folds the [`super::aggregate::ParSum`] segment monoid.
#[allow(clippy::too_many_arguments)]
fn worker_fold(
    txn: &ReadTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    t: &TableDef,
    table: u32,
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    filter: Option<&ExprProgram>,
    agg: &Aggregation,
    keep: Option<Vec<bool>>,
    width: usize,
    cells_budget: u64,
    shared_cells: &Arc<AtomicU64>,
) -> Result<BTreeMap<Vec<u8>, SendGroup>> {
    // Host registries deliberately absent: the gate proved every per-row
    // program host-free, so this context resolves identically to the serial
    // one — and a forged plan fails closed with the "not in scope" refusal.
    let mut wctx = ReadCtx(txn, None, None, None);
    let mut folder = Folder::new(&wctx, schema, plan, t, table, &[], agg);
    folder.parallelize(JoinCells::new_shared(cells_budget, shared_cells.clone()));
    let mut scan = BatchScan::open_partition(&wctx, table, lo, hi, width, keep);
    loop {
        let batch = scan.next(&mut wctx, filter, params)?;
        if batch.is_empty() {
            break;
        }
        for row in &batch {
            folder.push(&wctx, row, params)?;
        }
    }
    // Surrender the map in its Send image; a host accumulator (impossible
    // past the gate) refuses here and the coordinator abandons to serial.
    folder
        .into_groups()
        .into_iter()
        .map(|(k, (keys, accs, witness, scratch))| {
            if witness.is_some() {
                return Err(internal("a witness reached the parallel fold"));
            }
            let accs = accs
                .into_iter()
                .map(SendAcc::demote)
                .collect::<Result<Vec<SendAcc>>>()?;
            Ok((k, (keys, accs, scratch)))
        })
        .collect()
}

/// The fused worker: the decode-to-accumulator loop (`try_fused_fold`'s
/// input plumbing) over one piece — no row spine, no map. Hands back the one
/// (empty-key) group the serial fused path injects; a piece with no rows
/// still returns its never-pushed accumulators, which merge as the identity.
#[allow(clippy::too_many_arguments)]
fn worker_fused(
    txn: &ReadTxn<'_>,
    table: u32,
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    col: u16,
    agg: &Aggregation,
    cells_budget: u64,
    shared_cells: &Arc<AtomicU64>,
) -> Result<BTreeMap<Vec<u8>, SendGroup>> {
    // The serial fused path charges the injected group's cells; each worker
    // charges its own copy against the SHARED counter, so a budget too small
    // for the accumulators refuses here too (then serially, authentically).
    let mut cells = JoinCells::new_shared(cells_budget, shared_cells.clone());
    cells.charge(agg.aggs.len() as u64, || {
        "the group map of a parallel aggregate".into()
    })?;
    let mut accs = agg
        .aggs
        .iter()
        .map(|c| mint_accum(c, None, true))
        .collect::<Result<Vec<Acc>>>()?;
    let has_arg: Vec<bool> = agg.aggs.iter().map(|c| c.arg.is_some()).collect();
    txn.fold_range_column(
        table,
        lo.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        hi.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        col,
        &mut |v| {
            for (a, has) in accs.iter_mut().zip(&has_arg) {
                a.push(if *has { Some(v) } else { None })?;
            }
            Ok(())
        },
    )?;
    let accs = accs
        .into_iter()
        .map(SendAcc::demote)
        .collect::<Result<Vec<SendAcc>>>()?;
    let mut out = BTreeMap::new();
    out.insert(Vec::new(), (Vec::new(), accs, None));
    Ok(out)
}
