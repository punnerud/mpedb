//! Shape-admission rules: aggregate-over-index (format 59/60) and the parallel-fold gate.

use super::*;

/// May aggregate `call` be served by index `ix` of table `t` (the
/// aggregate-over-index-tree admission rule, format 59)? The SINGLE source for
/// planner and validate, so the producer and the re-validator cannot drift.
///
/// - `count(*)` (no argument): every indexed column must be schema-NOT-NULL,
///   so the entry count IS the row count.
/// - `f(col)` for native `count`/`sum`/`avg`/`total`/`min`/`max`: the argument
///   must be the bare LEADING index column and every NON-leading column
///   schema-NOT-NULL (otherwise a row with `col` non-NULL but a trailing
///   column NULL would be missing from the tree — the membership rule would
///   overshoot the aggregate's NULL-skip).
/// - `sum`/`avg`/`total` decode the value from the KEY, so the leading column
///   must be plain-keyed numeric (`int64`/`float64` — exact keycode round-trip
///   modulo the canonical float image, which every SQL comparison calls equal).
/// - `min`/`max` re-fetch the boundary ROW, so any type serves EXCEPT a
///   non-BINARY collation (the tree orders folded text; the fold's
///   `min_max_prefers` orders raw bytes — agree or refuse, never differ) and
///   `any` (class-keyed; refused with the same reasoning, v1).
/// - DISTINCT, FILTER, host aggregates, extra args, `group_concat`: refused —
///   they need dedup, other columns, callbacks, or scan-order-dependent
///   results the index order would silently change.
pub fn agg_servable_by_index(
    t: &mpedb_types::TableDef,
    ix: &mpedb_types::IndexDef,
    call: &AggCall,
) -> bool {
    use mpedb_types::AggFn as F;
    if call.distinct || call.filter.is_some() || !call.extra_args.is_empty() {
        return false;
    }
    let Some(f) = call.func.native() else {
        return false; // host aggregate: a live callback, never an index fold
    };
    let not_null = |c: u16| {
        t.columns
            .get(c as usize)
            .is_some_and(|col| !col.nullable)
    };
    let Some(arg) = &call.arg else {
        // count(*): counts NULL rows too, so the tree must omit nothing.
        return f == F::Count
            && !ix.columns.is_empty()
            // An expression key part has no column whose NOT NULL could make the
            // count exact, and its ordinal panics `not_null`.
            && !ix.has_expression_part()
            && ix.columns.iter().all(|&c| not_null(c));
    };
    // The argument must be the bare leading index column…
    let lead = match ix.columns.first() {
        Some(&c) => c,
        None => return false,
    };
    if arg.instrs.as_slice() != [mpedb_types::Instr::PushCol(lead)] {
        return false;
    }
    // …and the trailing columns NOT NULL, so membership == "arg is non-NULL".
    if !ix.columns[1..].iter().all(|&c| not_null(c)) {
        return false;
    }
    let Some(col) = t.columns.get(lead as usize) else {
        return false;
    };
    match f {
        F::Count => true, // membership only — no value is ever decoded
        F::Sum | F::Avg | F::Total => {
            matches!(col.ty, ColumnType::Int64 | ColumnType::Float64)
        }
        // The argument's collation (format 60) must BE the tree's fold: the
        // boundary probe finds the extremum by KEY order, which is the fold's
        // comparison exactly when the two collations agree — a NOCASE tree
        // serves `min(nc)` (the fold compares NOCASE since format 60) but not
        // `min(nc COLLATE BINARY)`. A typeless (`any`) column stays refused:
        // its class-keyed image is not the fold's comparison for text.
        F::Min | F::Max => col.ty != ColumnType::Any && call.coll == col.collation,
        // Both build an ORDERED accumulation in SCAN order, and an index
        // walk would reorder it — a different array, not a faster one.
        F::GroupConcat | F::ArrayAgg => false,
    }
}

/// The SET-level admission for aggregate-over-index: every call individually
/// servable ([`agg_servable_by_index`]) PLUS the collated-lead mix guard.
///
/// A tree whose leading key column folds text (non-BINARY collation) stores
/// FOLDED keys, so the executor's mixed "index-tree scan" mode — which decodes
/// each leading value once and feeds every accumulator — would hand a min/max
/// a folded spelling (`'b'` for a stored `'B'`) that may match no row. Such a
/// tree may serve an all-`count` set (membership only, no key decoded) or an
/// all-min/max set (boundary probes, the value re-fetched FROM THE ROW), and
/// nothing mixed. Shared by the planner (which chooses) and `validate` (which
/// re-proves against the live schema), so the two cannot drift.
pub fn agg_set_servable_by_index(
    t: &mpedb_types::TableDef,
    ix: &mpedb_types::IndexDef,
    aggs: &[AggCall],
) -> bool {
    use mpedb_types::AggFn as F;
    if !aggs.iter().all(|c| agg_servable_by_index(t, ix, c)) {
        return false;
    }
    let lead_coll = ix
        .columns
        .first()
        .and_then(|&c| t.columns.get(c as usize))
        .map(|c| c.collation)
        .unwrap_or(mpedb_types::Collation::Binary);
    if lead_coll == mpedb_types::Collation::Binary {
        return true;
    }
    aggs.iter().all(|c| c.func.native() == Some(F::Count))
        || aggs
            .iter()
            .all(|c| matches!(c.func.native(), Some(F::Min | F::Max)))
}

/// May this SELECT's aggregate fold be split across worker threads
/// (design/DESIGN-PARALLEL-READ.md §8 — the parallel fold's SHAPE gate)? The
/// single source for the executor's attempt and for EXPLAIN's claim, so the
/// two cannot drift. **Static eligibility only**: whether any worker actually
/// engages is decided at run time by the data (§8 replaced the compile-time
/// row-estimate gate with adaptive scheduling), and additionally requires a
/// pinned-snapshot read context with no correlated machinery in scope.
///
/// Everything admitted here is admitted BECAUSE its partition-merge is proven
/// order-identical to the serial fold — values, ties, spellings, raises:
///
/// - **count(\*) / count(x)** — addition; the NULL-skip is per row.
/// - **min / max over a bare non-`any` column** — the strict-beat compare is a
///   TOTAL order within every rigid column class ([`mpedb_types::Value::sort_cmp`]:
///   floats by IEEE total order including NaN, text under its collation), so
///   merging segment extrema in key order reproduces the serial
///   first-strict-beat witness exactly. An `any` column can hold Bool or
///   Timestamp beside another class — `sort_cmp` calls those peers,
///   incomparability breaks the first-beat associativity argument, and the
///   shape is refused rather than reasoned about. A COMPUTED argument's
///   per-row class is not schema-pinned → refused.
/// - **sum over a bare `int64` column** — folded per segment as an i128
///   prefix-sum monoid `(Σ, max-prefix, min-prefix)`; the serial i64 fold
///   raises iff SOME true prefix sum leaves i64 range, that predicate composes
///   exactly across ordered contiguous segments, and i128 cannot itself
///   overflow at any reachable row count. sqlite raises on INTERMEDIATE
///   overflow even when the total fits (probed on the bundled 3.45: `[MAX, 1,
///   -2]` errors while the same multiset as `[1, -2, MAX]` completes), mpedb's
///   serial fold has the same rule, and the monoid reproduces it in BOTH
///   directions — so no raise-frequency divergence exists and no RLS carve-out
///   is needed.
///
/// Refused, and why (the honest list — these change ANSWERS, not speed):
/// GROUP BY (v1 scope: the per-worker maps and their shared cell budget are a
/// separate step), float `sum`, every `avg`/`total` (f64 accumulation is
/// non-associative: partitioned low bits would differ from the serial — i.e.
/// the oracle's — answer, and the doctrine is bit-exact), `group_concat`
/// (order IS the answer), DISTINCT (dedup sets span partitions), host
/// aggregates (opaque state, no merge), bare columns (the witness rule's
/// all-NULL / all-filtered corners track the LATEST row — merging needs state
/// the witness does not carry), any host-called per-row program (a host
/// callback is not known thread-safe), index and point access paths, joins,
/// windows, correlated subplans.
pub fn parallel_fold_shape(p: &SelectPlan, schema: &mpedb_types::Schema) -> bool {
    use mpedb_types::AggFn as F;
    let Some(agg) = &p.aggregate else {
        return false;
    };
    let Some(t) = schema.table(p.table) else {
        return false;
    };
    if !p.joins.is_empty()
        || p.post_filter.is_some()
        || !p.windows.is_empty()
        || agg.over_index.is_some()
        || !agg.group_by.is_empty()
        || !agg.bare_cols.is_empty()
        || agg.aggs.is_empty()
        || p.table == DUAL_TABLE
        || p.table == CTE_TABLE
        || t.primary_key.is_empty()
        || !matches!(p.access, AccessPath::FullScan | AccessPath::PkRange { .. })
    {
        return false;
    }
    // Per-row programs must be host-free: workers run without the host
    // registries in scope, and a host callback is not known thread-safe.
    // (HAVING and the projection run at finish, on the calling thread, with
    // the full context — they are free to call hosts.)
    let host_free = |prog: &Option<ExprProgram>| prog.as_ref().is_none_or(|f| !f.has_host_call());
    if !host_free(&p.filter) {
        return false;
    }
    // The bare-column argument a call reads, when it is exactly that shape.
    let bare_col = |c: &AggCall| match c.arg.as_ref().map(|p| p.instrs.as_slice()) {
        Some([mpedb_types::Instr::PushCol(i)]) => t.columns.get(*i as usize),
        _ => None,
    };
    agg.aggs.iter().all(|c| {
        if c.distinct || !c.extra_args.is_empty() || !host_free(&c.filter) {
            return false;
        }
        match c.func.native() {
            Some(F::Count) => host_free(&c.arg),
            Some(F::Min | F::Max) => {
                bare_col(c).is_some_and(|col| col.ty != mpedb_types::ColumnType::Any)
            }
            Some(F::Sum) => bare_col(c).is_some_and(|col| col.ty == mpedb_types::ColumnType::Int64),
            // avg/total/group_concat/host: order-dependent or opaque.
            _ => false,
        }
    })
}
