use super::*;

/// Live-cell accounting for a nested-loop join's materialized intermediate
/// product (the #74 budget's memory-proportional twin, `[runtime]
/// max_join_cells`). `live` counts the `Value` cells currently HELD by
/// [`gather_joined`] — the accumulated tuple set, the held inner side, and
/// the next stage being built — so the counter tracks the join's resident
/// footprint, which the work-row meter (a count of rows READ) cannot see: a
/// 17-way cross join materializes gigabytes while still far under the 10^9
/// work-row default. Deterministic — a pure function of data and plan — so
/// the trip point is reproducible on every machine. `budget == 0` is the
/// unlimited sentinel.
///
/// Shared with the streaming aggregate (#123 §4.3), where the input is no
/// longer held at all but the GROUP MAP still is: an aggregate's irreducible
/// state is O(groups), no chunk size makes it smaller, and the same counter
/// with the same knob is what bounds it.
pub(super) struct JoinCells {
    budget: u64,
    live: u64,
}

impl JoinCells {
    pub(super) fn new(budget: u64) -> JoinCells {
        JoinCells { budget, live: 0 }
    }

    /// Charge `n` newly held cells; [`Error::RuntimeBudget`] once the live
    /// total crosses the budget. `which` is evaluated only on the error path.
    #[inline]
    pub(super) fn charge(&mut self, n: u64, which: impl FnOnce() -> String) -> Result<()> {
        self.live = self.live.saturating_add(n);
        if self.budget != 0 && self.live > self.budget {
            return Err(Error::RuntimeBudget {
                kind: mpedb_types::BudgetKind::JoinCells,
                limit: self.budget,
                used: self.live,
                which: which(),
            });
        }
        Ok(())
    }

    /// Return `n` cells whose rows were dropped (a superseded accumulator
    /// stage, a released held inner side).
    fn release(&mut self, n: u64) {
        self.live = self.live.saturating_sub(n);
    }
}

/// The clean out-of-memory error for the join accumulator's own allocations:
/// the bulk reservations in [`gather_joined`] are fallible, so under a memory
/// rlimit / cgroup cap an unbounded (`max_join_cells = 0`) join fails with an
/// [`Error`] instead of aborting the host process. Best-effort — a small
/// allocation elsewhere at the very wall can still abort; the deterministic
/// cell budget is the primary guard.
fn join_oom() -> Error {
    Error::OutOfMemory { what: "a nested-loop join's intermediate rows" }
}

/// Can a join budget of `cells` plausibly be allocated by this process?
/// ~40 B resident per cell (the calibration constant from
/// [`mpedb_types::config::DEFAULT_MAX_JOIN_CELLS`]'s measurement), compared
/// against the tighter of the address-space rlimit and, on Linux,
/// `MemAvailable`. Falls back to "fits" when nothing is readable — that is
/// today's behaviour, so an exotic platform loses nothing. The 3/4 factor
/// leaves room for the transient candidate row and the engine's own maps.
fn budget_fits_in_memory(cells: u64) -> bool {
    let need = cells.saturating_mul(40);
    let mut bound = u64::MAX;
    // wasm32 has no `getrlimit`, but it does have a HARD address-space
    // ceiling that plays the same role: a 32-bit wasm memory can never exceed
    // 4 GiB. Using it keeps this function doing what it says — comparing the
    // need against the address space — rather than falling back to
    // "unbounded", which would be the one wrong answer in the one environment
    // whose address space is smallest.
    #[cfg(target_arch = "wasm32")]
    {
        bound = bound.min(4 << 30);
    }
    #[cfg(not(target_arch = "wasm32"))]
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_AS, &mut rl) == 0 && rl.rlim_cur != libc::RLIM_INFINITY {
            bound = bound.min(rl.rlim_cur as u64);
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
        {
            bound = bound.min(kb.saturating_mul(1024));
        }
    }
    bound == u64::MAX || need <= bound / 4 * 3
}

/// Build one joined row of exactly `cap` values.
///
/// `outer` lands at slot 0 and `inner` at `inner_at`, with NULL padding in
/// between and after. Both sides may be SHORTER than the region they occupy:
/// #125 narrows a held tuple to the slots a later stage can observe, and the
/// dropped tail then reads as the NULL padding it already was for a LEFT
/// join's unmatched inner side. Positions are therefore fixed by `inner_at`
/// and `cap` — the plan's column indices are absolute in this tuple and
/// nothing may shift them — which is also why the LEFT extension
/// (`inner = &[]`) and the FULL sweep (`outer = &[]`) are this same call.
///
/// Two regimes, chosen by the caller from the cell budget:
///
/// - **finite budget** (the default): the deterministic cell cap is the
///   guard, so the build is the plain `with_capacity` + `extend_from_slice`
///   it always was — the O(n·m) candidate loop pays nothing for the budget
///   machinery beyond one predicted branch.
/// - **`fallible` (budget `0` = unlimited)**: every allocation the row makes
///   is fallible — the spine via `try_reserve_exact`, and each text/blob
///   payload via [`try_clone_value`] (at the memory wall those per-value
///   clones are exactly what an infallible `extend_from_slice` aborts on;
///   observed: a 26-byte `String` clone). Scalars are cloned inline (their
///   `Clone` cannot allocate); only heap-carrying values take the outlined
///   fallible path, which never inlines (`List` makes it recursive).
// inline(always): this is the body of the O(n·m) candidate loop — as a mere
// hint the call stayed outlined and cost ~1-2 ns per candidate (measurable
// on a 400M-candidate join).
#[inline(always)]
fn build_joined_row(
    fallible: bool,
    cap: usize,
    outer: &[Value],
    inner: &[Value],
    inner_at: usize,
) -> Result<Vec<Value>> {
    let mut joined;
    if fallible {
        joined = Vec::new();
        joined.try_reserve_exact(cap).map_err(|_| join_oom())?;
        push_clones(&mut joined, outer)?;
        joined.resize(inner_at, Value::Null);
        push_clones(&mut joined, inner)?;
    } else {
        joined = Vec::with_capacity(cap);
        joined.extend_from_slice(outer);
        joined.resize(inner_at, Value::Null);
        joined.extend_from_slice(inner);
    }
    joined.resize(cap, Value::Null);
    Ok(joined)
}

/// [`build_joined_row`] into an EXISTING buffer, reusing its allocation.
///
/// The inner candidate loop evaluates the ON over `[outer ‖ inner]` and, for a
/// selective join, DISCARDS most candidates. Building a fresh `Vec` per
/// candidate — the pre-`4471128` join path — allocated and freed one heap
/// buffer per considered pair, the dominant cost in the `gather_joined`
/// profile (malloc/free + drop chains). Filling a reused buffer instead pays
/// one allocation across a run of rejected candidates; a KEPT candidate
/// `mem::take`s the buffer (moved, never cloned — exactly the old cost), so
/// this is strictly ≤ the old allocation count in every case: a cross join
/// where every candidate is kept reallocates each time as before, a selective
/// join shares one buffer across its rejects.
#[inline]
fn fill_joined_row(
    buf: &mut Vec<Value>,
    fallible: bool,
    cap: usize,
    outer: &[Value],
    inner: &[Value],
    inner_at: usize,
) -> Result<()> {
    buf.clear();
    if fallible {
        buf.try_reserve(cap).map_err(|_| join_oom())?;
        push_clones(buf, outer)?;
        buf.resize(inner_at, Value::Null);
        push_clones(buf, inner)?;
    } else {
        buf.reserve(cap);
        buf.extend_from_slice(outer);
        buf.resize(inner_at, Value::Null);
        buf.extend_from_slice(inner);
    }
    buf.resize(cap, Value::Null);
    Ok(())
}

/// `extend_from_slice`, made fallible per value: only heap-carrying values take
/// the outlined [`try_clone_value`] path, so a scalar row pays one predicted
/// branch.
#[inline(always)]
fn push_clones(out: &mut Vec<Value>, src: &[Value]) -> Result<()> {
    for v in src {
        match v {
            Value::Null
            | Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::Timestamp(_) => out.push(v.clone()),
            heap => out.push(try_clone_value(heap)?),
        }
    }
    Ok(())
}

/// Rebuild `row` carrying only what a later stage can observe (#125).
///
/// `mask` is in THIS row's own coordinates — a stage mask for an accumulated
/// tuple, an inner mask for a held inner relation — and `trim` is how many
/// leading slots to keep.
///
/// Slots at or above `trim` are DROPPED and holes below it become
/// `Value::Null`. Truncating is the safer half of that pair and gets used
/// wherever it can: a consumer this analysis missed reads out of bounds and
/// `Instr::PushCol` answers with `Error::Internal`, where a NULLed hole would
/// answer with a wrong value. The rebuild is a fresh exact-size `Vec` rather
/// than `truncate`, which frees nothing — the capacity IS the memory this is
/// trying not to hold.
fn narrow_row(row: Vec<Value>, mask: &Mask, trim: usize, fallible: bool) -> Result<Vec<Value>> {
    let trim = trim.min(row.len());
    let mut out = Vec::new();
    if fallible {
        out.try_reserve_exact(trim).map_err(|_| join_oom())?;
    } else {
        out.reserve_exact(trim);
    }
    for (i, v) in row.into_iter().enumerate() {
        if i >= trim {
            break;
        }
        out.push(if mask.observes(i) { v } else { Value::Null });
    }
    Ok(out)
}

/// [`narrow_row`] over a whole materialized relation, in place. A no-op — not
/// even a rebuild — when the mask keeps everything.
pub(super) fn narrow_rows(rows: &mut Vec<Vec<Value>>, mask: &Mask) -> Result<()> {
    if !mask.prunes() {
        return Ok(());
    }
    let trim = mask.trim();
    let mut out = Vec::new();
    out.try_reserve_exact(rows.len()).map_err(|_| join_oom())?;
    for row in std::mem::take(rows) {
        out.push(narrow_row(row, mask, trim, true)?);
    }
    *rows = out;
    Ok(())
}

/// The DECODE-level keep-vector for a batched scan (#125's scan half): the
/// stage mask's observable slots, WIDENED by every column the residual
/// `filter` reads — the residual runs over the pruned row inside
/// `scan_rows_pruned`, unlike the materializing gathers, where it sees full
/// width — then trimmed of its dead tail. `None` when nothing would be
/// pruned, so the caller can tell "decode everything" apart at a glance.
pub(super) fn scan_keep(
    mask: &Mask,
    filter: Option<&ExprProgram>,
    width: usize,
) -> Option<Vec<bool>> {
    let mut keep: Vec<bool> = (0..width).map(|i| mask.observes(i)).collect();
    if let Some(f) = filter {
        // `PushCol` is the IR's only column read — the same single-opcode
        // fact `mpedb_sql::row_prune` leans on.
        for instr in &f.instrs {
            if let mpedb_types::Instr::PushCol(i) = instr {
                if let Some(s) = keep.get_mut(*i as usize) {
                    *s = true;
                }
            }
        }
    }
    let trim = keep.iter().rposition(|k| *k).map_or(0, |p| p + 1);
    keep.truncate(trim);
    if keep.len() == width && keep.iter().all(|k| *k) {
        return None;
    }
    Some(keep)
}

/// [`gather_rows`] that narrows as it reads (#125).
///
/// Narrowing a relation the gather already materialized still pays for the
/// wide version once — the peak is then the WIDE set, and every column pruning
/// buys is invisible at the high-water mark. Measured: an aggregate over a
/// six-column self-join went 753.4 -> 326.0 B/row that way, and the 326 was
/// exactly the outer relation's own full-width gather.
///
/// So where the access path can be drawn in [`BatchScan`]-sized pieces, it is:
/// each batch arrives ALREADY PRUNED — the scan decodes only the observed
/// columns (plus what the residual filter reads, which a per-batch
/// [`narrow_row`] pass then drops again) — and the wide relation never
/// exists, not even one row of it. Same rows, same order, same work-row
/// charges as a single-pass gather — #123 §9.2 argues that at length and
/// `tests/agg_stream.rs::c_invariance` re-runs a whole differential battery
/// at four batch sizes to keep it true.
///
/// Everything else (an index or FTS access, a write context, a mask that keeps
/// everything) falls back to gather-then-narrow, which is the same answer at
/// the old peak.
#[allow(clippy::too_many_arguments)]
pub(super) fn gather_narrowed(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    t: &TableDef,
    mask: &Mask,
) -> Result<Vec<Vec<Value>>> {
    if !mask.prunes() {
        return gather_rows(ctx, table, access, filter, plan, params, None);
    }
    let width = t.columns.len();
    let keep = scan_keep(mask, filter, width);
    if let Some(mut scan) =
        BatchScan::open(&*ctx, table, access, plan, params, t, width, keep.clone())?
    {
        let trim = mask.trim();
        // Does the scan mask keep anything the TARGET mask drops (a
        // filter-only column, per `scan_keep`'s widening)? Only then does the
        // batch still need the narrowing pass.
        let renarrow = keep
            .as_deref()
            .is_none_or(|k| k.iter().enumerate().any(|(i, on)| *on && !mask.observes(i)));
        let mut out = Vec::new();
        loop {
            // Draw a pruned batch, re-narrow it if the filter widened it,
            // drop it. The batch is the only extra residency that ever exists.
            let batch = scan.next(ctx, filter, params)?;
            if batch.is_empty() {
                break;
            }
            out.try_reserve(batch.len()).map_err(|_| join_oom())?;
            for row in batch {
                out.push(if renarrow {
                    narrow_row(row, mask, trim, true)?
                } else {
                    row
                });
            }
        }
        return Ok(out);
    }
    let mut rows = gather_rows(ctx, table, access, filter, plan, params, None)?;
    narrow_rows(&mut rows, mask)?;
    Ok(rows)
}

fn try_clone_value(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Text(s) => {
            let mut t = String::new();
            t.try_reserve_exact(s.len()).map_err(|_| join_oom())?;
            t.push_str(s);
            Value::Text(t)
        }
        Value::Blob(b) => {
            let mut c = Vec::new();
            c.try_reserve_exact(b.len()).map_err(|_| join_oom())?;
            c.extend_from_slice(b);
            Value::Blob(c)
        }
        Value::List(xs) => {
            let mut c = Vec::new();
            c.try_reserve_exact(xs.len()).map_err(|_| join_oom())?;
            for x in xs {
                c.push(try_clone_value(x)?);
            }
            Value::List(c)
        }
        // Null/Int/Float/Bool/Timestamp carry no heap: Clone cannot allocate.
        other => other.clone(),
    })
}

/// `INNER JOIN`, as a nested loop over the outer scan.
///
/// The order of the four tests is the security contract, not an implementation
/// detail — see [`mpedb_sql::Join::policy`]. Each table's RLS `USING` runs over
/// ITS OWN row, before anything that can see both:
///
/// mpedb's expressions raise on arithmetic overflow, and a raise is
/// observable. An `ON a.x * b.secret` that overflows, evaluated before b's
/// policy, would report the existence of a row the policy hides — the row
/// never comes back, but the error says it was there. (Division by zero is
/// NOT such a case: like sqlite it yields NULL, which just fails to match.)
/// Filtering first is what makes the policy a filter rather than a suggestion.
///
/// Cost: the inner side is read ONCE and held, so this is O(n+m) reads and
/// O(n·m) `on` evaluations, with the inner side resident. No predicate is
/// pushed into either scan yet — every conjunct of the user's WHERE waits for
/// the joined row — so both sides are full scans unless a POLICY pins a key.
/// `EXPLAIN` says so rather than leaving it to be found on a big table.
/// Does this access path reference the outer row (`KeyPart::OuterCol`)?
/// If so it is the index nested-loop form, resolved per outer row.
fn access_has_outer(a: &AccessPath) -> bool {
    let outer = |p: &KeyPart| matches!(p, KeyPart::OuterCol(_));
    let bound_outer = |b: &Option<KeyBound>| {
        b.as_ref().is_some_and(|b| b.parts.iter().any(outer))
    };
    match a {
        AccessPath::PkPoint(parts) => parts.iter().any(outer),
        AccessPath::IndexPoint { parts, .. } => parts.iter().any(outer),
        AccessPath::PkRange { lo, hi } | AccessPath::IndexRange { lo, hi, .. } => {
            bound_outer(lo) || bound_outer(hi)
        }
        // An FtsScan carries a literal query tree with no key parts, so it never
        // references the outer row (and MATCH is single-table only — it never
        // reaches a join inner side).
        AccessPath::FullScan | AccessPath::FtsScan { .. } => false,
    }
}

/// Fetch one join step's candidate rows for ONE outer row — the index nested
/// loop. The join's POLICY runs here, over each fetched inner row alone,
/// BEFORE the residual ON can raise on it: the same RLS ordering contract as
/// the held path, where `gather_rows` applies it as the fetch filter.
fn fetch_inner(
    ctx: &mut dyn TxnCtx,
    join: &Join,
    plan: &CompiledPlan,
    params: &[Value],
    outer: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mut rows = match &join.access {
        AccessPath::PkPoint(parts) => {
            let mut pk = Vec::with_capacity(parts.len());
            let mut any_null = false;
            for p in parts {
                let v = resolve_part_outer(p, plan, params, outer)?;
                if v.is_null() {
                    // `inner.pk = NULL` is UNKNOWN: no candidates (and for a
                    // LEFT join, that means NULL-extension — SQL's answer).
                    any_null = true;
                    break;
                }
                pk.push(v);
            }
            if any_null {
                Vec::new()
            } else {
                ctx.get_by_pk(join.table, &pk)?.into_iter().collect()
            }
        }
        AccessPath::IndexPoint { index_no, parts } => {
            let mut vals = Vec::with_capacity(parts.len());
            let mut any_null = false;
            for p in parts {
                let v = resolve_part_outer(p, plan, params, outer)?;
                if v.is_null() {
                    any_null = true; // `col = NULL` is UNKNOWN: no candidates
                    break;
                }
                vals.push(v);
            }
            if any_null {
                Vec::new()
            } else {
                ctx.scan_by_index(join.table, *index_no, &vals)?
            }
        }
        _ => return Err(internal("unparametrized access in index nested loop")),
    };
    if let Some(p) = &join.policy {
        let mut stack = Vec::with_capacity(p.max_stack());
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if p.eval_filter_host(&mut stack, &row, params, ctx.host_fns())? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn gather_joined(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    outer_table: u32,
    outer_access: &AccessPath,
    outer_policy: Option<&ExprProgram>,
    joins: &[Join],
    joined_filter: Option<&ExprProgram>,
    // #125: which slots of the accumulated tuple a later stage can observe.
    // `None` = carry everything, which is what every path did before.
    prune: Option<&RowPrune>,
) -> Result<Vec<Vec<Value>>> {
    // Left-deep nested loop. Start with the outer's rows (its policy applies
    // through the access path), then fold in each join: gather that table's
    // rows — its policy runs over its OWN row, BEFORE any ON can raise on
    // it — and keep the pairs its ON accepts. Join `k`'s ON sees the row
    // accumulated so far, `[table0 ‖ … ‖ table_k]`, which is exactly the tuple
    // the planner bound and width-checked it against.
    let outer_def = table_def(schema, plan, outer_table)?;
    let mut acc = match prune {
        // The outer relation is held for the whole nested loop, so it is
        // narrowed AS IT IS READ rather than after — see `gather_narrowed`.
        Some(p) => gather_narrowed(
            ctx,
            outer_table,
            outer_access,
            outer_policy,
            plan,
            params,
            &outer_def,
            p.stage(0),
        )?,
        None => gather_rows(ctx, outer_table, outer_access, outer_policy, plan, params, None)?,
    };
    let mut stack = Vec::new();
    // The width of the tuple accumulated BEFORE each join — what a FULL
    // join's unmatched-inner sweep NULL-extends on the left. Tracked from the
    // schema rather than read off `acc`, which may hold no rows.
    let mut acc_width = outer_def.columns.len();
    // Live-cell budget on what this join HOLDS ([`JoinCells`]): seeded with
    // the outer rows, charged per retained joined row, released when a stage
    // is superseded. The work-row charge below bounds the O(n·m) candidates
    // CONSIDERED; this bounds the product RETAINED — the one that eats
    // memory when a late constant anchor leaves every earlier step a cross
    // join (select5's `join-17-4`).
    //
    // **Charged on the LOGICAL width, before #125 narrows anything.** The
    // budget is a deterministic tripwire whose trip point is a tested contract
    // (`tests/runtime_budget.rs`, `tests/mpee_solver.rs`): which statements it
    // refuses must not depend on how wide the executor happens to carry a row.
    // Column pruning is a width optimisation that changes nothing observable,
    // and *which queries are refused* is observable. So the counter keeps
    // pricing the product the join logically forms; residency is now strictly
    // below what it says.
    let mut cells = JoinCells::new(ctx.join_cells_budget());
    cells.charge((acc.len() * acc_width) as u64, || {
        format!("rows held by a join over \"{}\"", table_name(schema, outer_table))
    })?;
    // With a FINITE budget the deterministic cap above is the guard and the
    // row build stays on the plain infallible path it always was; the
    // explicit `max_join_cells = 0` opt-out pays for per-allocation
    // fallibility (see `build_joined_row`).
    //
    // …unless the budget CANNOT FIT: the cap only guards if it trips before
    // the memory wall, and a budget of 2^28 cells ≈ 11 GB resident never
    // trips inside a 3 GB rlimit — measured: `select5`'s `join-17-4` died on
    // SIGABRT with the default config, exactly the host-killing failure this
    // budget exists to prevent. So when the budget's byte-equivalent exceeds
    // what this process can plausibly allocate (rlimit and, on Linux,
    // MemAvailable), the row build goes fallible too: answers are unchanged
    // (fallibility only changes the failure MODE at the wall, abort → clean
    // Error::OutOfMemory), the deterministic trip point is unchanged, and a
    // healthy box still pays nothing. One probe per gather, not per row.
    let fallible = cells.budget == 0 || !budget_fits_in_memory(cells.budget);
    for (ji, join) in joins.iter().enumerate() {
        let inner_def = table_def(schema, plan, join.table)?;
        let inner_width = inner_def.columns.len();
        let next_width = acc_width + inner_width;
        // What THIS stage's retained rows must keep — `stage(ji + 1)`, the
        // suffix union from here to the output. `None` whenever narrowing would
        // rebuild each row into the row it already is, so a statement that reads
        // everything pays nothing at all for this.
        let narrow = prune.map(|p| p.stage(ji + 1)).filter(|m| m.prunes());
        let next_trim = narrow.map_or(next_width, Mask::trim);
        let join_tbl = join.table; // for the #74 attribution closures
        // An access with no OuterCol parts is resolved once: read the inner
        // side once and hold it (the pre-#49 execution — keeping it is what
        // stops an ON without equality from regressing to O(n·m) READS). One
        // WITH OuterCol parts is the index nested loop, fetched per outer row.
        let held: Option<Vec<Vec<Value>>> = if access_has_outer(&join.access) {
            None
        } else {
            // The held inner side is resident for the whole outer loop — the
            // 188.8 B/row `join_held` measures — so it is narrowed too, by its
            // OWN mask: this narrowing happens BEFORE the ON runs against these
            // rows, so it keeps what the ON reads as well as what survives to
            // the output. The row ORDER is untouched, so a FULL join's
            // `inner_matched` still indexes it.
            let h = match prune {
                Some(p) => gather_narrowed(
                    ctx,
                    join.table,
                    &join.access,
                    join.policy.as_ref(),
                    plan,
                    params,
                    &inner_def,
                    p.inner(ji),
                )?,
                None => gather_rows(
                    ctx,
                    join.table,
                    &join.access,
                    join.policy.as_ref(),
                    plan,
                    params,
                    None,
                )?,
            };
            cells.charge((h.len() * inner_width) as u64, || {
                format!("nested-loop join with \"{}\"", table_name(schema, join_tbl))
            })?;
            Some(h)
        };
        // FULL: which held inner rows matched at least one outer row.
        // validate pinned FULL to a single, held (FullScan) join, so `held`
        // is always Some when this is.
        let mut inner_matched: Option<Vec<bool>> = if join.kind == JoinKind::Full {
            Some(vec![false; held.as_ref().map_or(0, |h| h.len())])
        } else {
            None
        };
        let mut next = Vec::new();
        // One candidate buffer reused across the whole nested loop: a rejected
        // pair leaves it filled for the next `fill_joined_row`; a kept pair
        // `mem::take`s it (moved into `next`, no clone) and the next fill
        // reallocates. See `fill_joined_row`.
        let mut cand: Vec<Value> = Vec::new();
        for a in &acc {
            let fetched;
            let candidates: &[Vec<Value>] = match &held {
                Some(rows) => rows,
                None => {
                    fetched = fetch_inner(ctx, join, plan, params, a)?;
                    &fetched
                }
            };
            let mut matched = false;
            for (ci, i) in candidates.iter().enumerate() {
                // #74: one work-row per inner candidate considered. This is the
                // O(n·m) cost of a cross join — a held inner side is read once
                // (charged m by the scan layer) but paired against every outer
                // row here, so the product must be counted at the pairing.
                ctx.charge_work(1, &|| {
                    format!("nested-loop join with \"{}\"", table_name(schema, join_tbl))
                })?;
                // The candidate is built at FULL width: the ON was bound
                // against the whole tuple `[outer ‖ inner]` and may read any
                // slot of it. Only what SURVIVES is narrowed — the candidate
                // itself is transient and never counted as held.
                fill_joined_row(&mut cand, fallible, next_width, a, i, acc_width)?;
                if join.on.eval_filter_host(&mut stack, &cand, params, ctx.host_fns())? {
                    matched = true;
                    if let Some(m) = &mut inner_matched {
                        m[ci] = true;
                    }
                    // The memory charge, per RETAINED row: candidates that the
                    // ON rejects were transient, but this one is now held.
                    cells.charge(next_width as u64, || {
                        format!("nested-loop join with \"{}\"", table_name(schema, join_tbl))
                    })?;
                    next.try_reserve(1).map_err(|_| join_oom())?;
                    // Kept: MOVE the buffer out (no clone); `cand` is empty for
                    // the next fill, which reallocates — exactly the old
                    // per-kept-row allocation.
                    let joined = std::mem::take(&mut cand);
                    next.push(match narrow {
                        None => joined,
                        Some(m) => narrow_row(joined, m, next_trim, fallible)?,
                    });
                }
            }
            // LEFT/FULL: no match → ONE row with the inner side NULL-extended.
            // The ON is never evaluated over this row — it exists BECAUSE
            // nothing matched — so it cannot raise on it, and a policy-hidden
            // inner row reads as ABSENT (the outer row survives,
            // NULL-extended, never carrying the hidden row's values).
            if !matched && matches!(join.kind, JoinKind::Left | JoinKind::Full) {
                let joined = build_joined_row(fallible, next_width, a, &[], acc_width)?;
                cells.charge(next_width as u64, || {
                    format!("nested-loop join with \"{}\"", table_name(schema, join_tbl))
                })?;
                next.try_reserve(1).map_err(|_| join_oom())?;
                next.push(match narrow {
                    None => joined,
                    Some(m) => narrow_row(joined, m, next_trim, fallible)?,
                });
            }
        }
        // FULL's other half: inner rows NO outer row matched, NULL-extended
        // on the OUTER side. Same raise contract — their ON never ran true,
        // and a policy-hidden OUTER row was never in `acc` to begin with.
        if let (Some(m), Some(h)) = (&inner_matched, &held) {
            for (ci, i) in h.iter().enumerate() {
                if !m[ci] {
                    let joined = build_joined_row(fallible, next_width, &[], i, acc_width)?;
                    cells.charge(next_width as u64, || {
                        format!("nested-loop join with \"{}\"", table_name(schema, join_tbl))
                    })?;
                    next.try_reserve(1).map_err(|_| join_oom())?;
                    next.push(match narrow {
                        None => joined,
                        Some(m) => narrow_row(joined, m, next_trim, fallible)?,
                    });
                }
            }
        }
        // This stage's inputs are dropped here: `acc` is superseded by `next`
        // and `held` goes out of scope. Return their cells so `live` keeps
        // tracking what the join actually holds.
        let dropped = (acc.len() * acc_width) as u64
            + held.as_ref().map_or(0, |h| (h.len() * inner_width) as u64);
        acc_width += inner_width;
        acc = next;
        cells.release(dropped);
    }
    // WHERE runs once, over the full joined row — after every ON and every
    // per-table policy, because it can raise and a raise is observable.
    // Survivors are MOVED, not cloned, so no new cells are charged; the
    // survivor vec's own spine is the one bulk allocation, made fallible.
    if let Some(f) = joined_filter {
        let mut kept = Vec::new();
        kept.try_reserve_exact(acc.len()).map_err(|_| join_oom())?;
        for row in acc {
            if f.eval_filter_host(&mut stack, &row, params, ctx.host_fns())? {
                kept.push(row);
            }
        }
        acc = kept;
    }
    Ok(acc)
}

pub(crate) fn resolve_part(part: &KeyPart, plan: &CompiledPlan, params: &[Value]) -> Result<Value> {
    Ok(match part {
        KeyPart::Param(i) => params
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("key param"))?,
        KeyPart::Const(i) => plan
            .consts
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("key const"))?,
        // Only legal inside a join's access path, where the outer row exists;
        // validate refuses it anywhere else, so reaching this is an exec bug.
        KeyPart::OuterCol(_) => return Err(internal("outer-column key part outside a join")),
    })
}

/// [`resolve_part`] with the accumulated outer row in scope — the index
/// nested-loop form, where `OuterCol(i)` is slot `i` of that row.
fn resolve_part_outer(
    part: &KeyPart,
    plan: &CompiledPlan,
    params: &[Value],
    outer: &[Value],
) -> Result<Value> {
    match part {
        KeyPart::OuterCol(i) => outer
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("outer key column out of row bounds")),
        other => resolve_part(other, plan, params),
    }
}

/// Fetch the candidate rows for an access path and apply the residual filter.
pub(super) fn gather_rows(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    cap: Option<usize>,
) -> Result<Vec<Vec<Value>>> {
    // FROM-less SELECT: the "table" is the DUAL sentinel — ONE synthetic
    // empty row, never a TxnCtx call (there is nothing to read). The filter
    // still runs (`SELECT 3 WHERE 1=0` is zero rows), over a width-0 row
    // whose programs can only read consts and params — validate enforced
    // that. Every select path funnels through here, so aggregates and
    // subplans over the dual row need no cases of their own.
    if table == mpedb_sql::DUAL_TABLE {
        let mut rows = vec![Vec::new()];
        if let Some(f) = filter {
            let mut stack = Vec::with_capacity(f.max_stack());
            if !f.eval_filter_host(&mut stack, &rows[0], params, ctx.host_fns())? {
                rows.clear();
            }
        }
        if cap == Some(0) {
            rows.clear();
        }
        return Ok(rows);
    }
    // Scan paths push the filter AND the cap down into the (possibly
    // streaming) scan. Point and index-equality paths gather their matches —
    // one row for a PK/unique probe, every equal row for a non-unique index —
    // and filter here (no cap pushdown; the caller's skip/take still bounds
    // what is returned).
    let mut rows = match access {
        AccessPath::PkPoint(parts) => {
            let mut pk = Vec::with_capacity(parts.len());
            for p in parts {
                pk.push(resolve_part(p, plan, params)?);
            }
            // A NULL PK part can never match a stored row (PK columns are NOT
            // NULL); get_by_pk simply misses — SQL's `pk = NULL` is UNKNOWN.
            ctx.get_by_pk(table, &pk)?.into_iter().collect()
        }
        AccessPath::PkRange { lo, hi } => {
            return match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                // A NULL bound makes the range predicate UNKNOWN for every
                // row: no matches.
                None => Ok(Vec::new()),
                Some((lo_k, hi_k)) => ctx.scan_rows_capped(
                    table,
                    lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    filter.map(|f| (f, params)),
                    cap,
                ),
            };
        }
        AccessPath::IndexPoint { index_no, parts } => {
            let mut vals = Vec::with_capacity(parts.len());
            let mut any_null = false;
            for p in parts {
                let v = resolve_part(p, plan, params)?;
                if v.is_null() {
                    // `col = NULL` is UNKNOWN; any-NULL rows are not indexed.
                    any_null = true;
                    break;
                }
                vals.push(v);
            }
            if any_null {
                Vec::new()
            } else {
                // N rows equal on the covered prefix; the engine takes the
                // exact-get fast path when a UNIQUE index is covered full
                // width.
                ctx.scan_by_index(table, *index_no, &vals)?
            }
        }
        AccessPath::IndexRange { index_no, lo, hi } => {
            match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                // A NULL bound makes the range predicate UNKNOWN: no matches.
                None => Vec::new(),
                // The same prefix-bound construction as a composite-PK range
                // works over the index tree: both the unique (`value`) and the
                // non-unique (`value ‖ pk`) key layouts start with the encoded
                // value, and `prefix_hi` clears every continuation.
                Some((lo_k, hi_k)) => {
                    let lo_b = lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc));
                    let hi_b = hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc));
                    // An index range fetches ONE ROW PER ENTRY, which beats a
                    // scan only while the range stays small. The planner cannot
                    // know the fraction (no histograms), so the engine prices
                    // it: `scan_by_index_range_adaptive` counts the range's
                    // KEYS up to the switch point and scans the table instead
                    // when the range is too wide to be worth a descent per row.
                    //
                    // Held back under a `cap`: this path does not push the
                    // LIMIT down (it materializes the range and the caller
                    // truncates — a measured hole, `SELECT … WHERE day_id >=
                    // 1000 LIMIT 5` costs the whole range), and switching the
                    // access path under a LIMIT would also change WHICH rows a
                    // LIMIT with no ORDER BY returns. Both belong to the
                    // pushdown fix, not to this one.
                    match cap.is_none().then(|| {
                        ctx.scan_by_index_range_adaptive(table, *index_no, lo_b, hi_b)
                    }) {
                        Some(r) => match r? {
                            Some(rows) => rows,
                            None => ctx.scan_by_index_range(table, *index_no, lo_b, hi_b)?,
                        },
                        None => ctx.scan_by_index_range(table, *index_no, lo_b, hi_b)?,
                    }
                }
            }
        }
        AccessPath::FullScan => {
            return ctx.scan_rows_capped(table, None, None, filter.map(|f| (f, params)), cap);
        }
        AccessPath::FtsScan { query } => {
            // Posting-list set algebra → matching rowids in ascending order
            // (design/DESIGN-FTS.md §4); fetch each row by its rowid PK. The
            // residual WHERE / RLS policy is applied by the shared filter loop
            // below, exactly as for a point/index path.
            let rowids = super::fts::evaluate(ctx, table, query)?;
            let mut out = Vec::with_capacity(rowids.len());
            for id in rowids {
                if let Some(row) = ctx.get_by_pk(table, &[Value::Int(id)])? {
                    out.push(row);
                }
            }
            out
        }
    };
    if let Some(f) = filter {
        let mut stack = Vec::with_capacity(f.max_stack());
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if f.eval_filter_host(&mut stack, &row, params, ctx.host_fns())? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok(rows)
}

/// Rows drawn per B+tree visit by a [`BatchScan`] — the same constant
/// `stream.rs::BATCH` uses, and for the same reason: small enough that the
/// working set is trivial, large enough that the per-batch tree re-descent
/// amortizes to noise (`stream_query` measures FASTER than the materializing
/// path at 160 k rows, design/DESIGN-STREAM-EXEC.md §6).
///
/// **Deliberately not budget-derived.** DESIGN-STREAM-EXEC §4.2 proposes
/// `C = clamp(1, budget_cells / (W·S), 65536)`, which is right for a stage
/// whose throughput improves with a bigger chunk. A FOLD is not such a stage:
/// its hold is O(groups) whichever way the input arrives, so every cell above
/// the re-descent amortization point buys nothing but peak. With the default
/// `max_join_cells = 268 435 456` that formula clamps to 65 536 rows ≈ 20 MB
/// held for `SELECT count(*)`, against 61.8 KB for the same scan through
/// `stream_query`. The budget is still consulted — as a DIVISOR below, so a
/// deliberately tiny budget still shrinks the batch, and as the §4.3 tripwire
/// on the group map, which is the state a chunk size genuinely cannot move.
const FOLD_BATCH: usize = 256;

/// `MPEDB_FOLD_BATCH=<n>` forces the fold's batch size, for A/B measurement
/// and for the C-invariance suite (`MPEDB_NO_SUBPLAN_MEMO` is the precedent).
///
/// It exists because the budget cannot serve as the knob here: `max_join_cells`
/// is ALSO the group-map tripwire (§4.3), so setting it low enough to force a
/// one-row batch refuses the statement instead of chunking it, and the
/// invariant that needs testing — "same rows, same order, same errors, for
/// every C" (§4.2) — is then untestable at the interesting values of C. Read
/// once per process; `0` or unparseable means "no override".
fn batch_override() -> Option<usize> {
    static OVERRIDE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("MPEDB_FOLD_BATCH")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
    })
}

/// Batch size for a fold over rows `width` cells wide under `budget` live
/// cells (`0` = unlimited): at most [`FOLD_BATCH`], at least one row (a
/// budget too small to hold a single row must still make progress — it is not
/// a new refusal), and proportional to the budget in between.
fn fold_batch(budget: u64, width: usize) -> usize {
    if let Some(n) = batch_override() {
        return n;
    }
    if budget == 0 {
        return FOLD_BATCH;
    }
    let w = width.max(1) as u64;
    (budget / w).clamp(1, FOLD_BATCH as u64) as usize
}

/// A resumable, PK-ordered, batched scan: the streaming half of #123 §5.1.
///
/// Each [`next`](BatchScan::next) draws at most `batch` FILTERED rows and
/// remembers where to resume — the last row's encoded PK, exclusive, which is
/// exactly the B+tree key it is stored under. The caller folds the batch and
/// drops it, so peak hold is O(batch) instead of O(matched rows). This is the
/// same resume-by-encoded-PK loop `stream.rs::refill` runs, lifted so the
/// aggregate can share it.
///
/// **Not observable.** The rows, their order, the residual filter applied to
/// them and the #74 work-row charges are identical to a single-pass
/// `gather_rows` — the work meter charges once per row visited inside
/// `RowCursor::next`, and re-descending the tree visits each row exactly once
/// either way. `batch` is config-derived and a statement's result may not
/// depend on config; `tests/agg_stream.rs` asserts that twice — the whole
/// differential battery re-run at `MPEDB_FOLD_BATCH ∈ {1, 2, 7, 256}`, and the
/// same battery answered identically under
/// `max_join_cells ∈ {0, 512, 4096, 268 435 456}`.
pub(super) struct BatchScan {
    table: u32,
    /// Lower bound of the NEXT batch, resumed strictly after the last row
    /// handed out — the raw storage key `scan_rows_pruned` hands back, so no
    /// re-encoding and no requirement that the PK survive the column mask.
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    /// Decode-time column mask ([`scan_keep`]): `None` decodes full rows.
    keep: Option<Vec<bool>>,
    batch: usize,
    done: bool,
}

impl BatchScan {
    /// Open a batched scan over `access`, or `Ok(None)` when this shape cannot
    /// be streamed and the caller must take its materializing path:
    ///
    /// - the context's `scan_rows_capped` is a materialize-then-truncate
    ///   (`!scans_incrementally`) — batching it would be quadratic;
    /// - the access path is not PK-ordered. `PkPoint` needs no streaming (one
    ///   row); `IndexPoint`/`IndexRange`/`FtsScan` resolve through a set of
    ///   rowids or an index tree, and a streaming index cursor is deferred to
    ///   #48 (design/DESIGN-STREAM-EXEC.md §7.1);
    /// - the "table" is the DUAL or working-table sentinel, which has no
    ///   B+tree and no PK to resume from.
    ///
    /// `keep` is the decode-time column mask the batches are drawn under
    /// ([`scan_keep`] — it must already cover what `filter` reads); `None`
    /// decodes full rows, which is byte-identical to the pre-#125 scan.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open(
        ctx: &dyn TxnCtx,
        table: u32,
        access: &AccessPath,
        plan: &CompiledPlan,
        params: &[Value],
        t: &TableDef,
        width: usize,
        keep: Option<Vec<bool>>,
    ) -> Result<Option<BatchScan>> {
        if !ctx.scans_incrementally()
            || table == mpedb_sql::DUAL_TABLE
            || table == mpedb_sql::CTE_TABLE
            || t.primary_key.is_empty()
        {
            return Ok(None);
        }
        let (lo, hi, empty) = match access {
            AccessPath::FullScan => (None, None, false),
            AccessPath::PkRange { lo, hi } => {
                match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                    // A NULL bound makes the range predicate UNKNOWN for every
                    // row: a valid scan that is born exhausted, NOT a refusal
                    // to stream (which would re-materialize for nothing).
                    None => (None, None, true),
                    Some((l, h)) => (l, h, false),
                }
            }
            AccessPath::PkPoint(_)
            | AccessPath::IndexPoint { .. }
            | AccessPath::IndexRange { .. }
            | AccessPath::FtsScan { .. } => return Ok(None),
        };
        Ok(Some(BatchScan {
            table,
            lo,
            hi,
            keep,
            batch: fold_batch(ctx.join_cells_budget(), width),
            done: empty,
        }))
    }

    /// A [`BatchScan`] over ONE MORSEL of an already-resolved PK range — the
    /// parallel fold's worker input (`exec/parallel.rs`). The caller resolved
    /// the plan's bounds once ([`range_bounds`]) and cut them at structural
    /// partition keys; each worker then drains its contiguous piece with
    /// exactly the serial scan's batching, masking, filtering and #74 charges
    /// — the same rows the serial scan would hand this piece, in the same
    /// order, through the same code.
    pub(super) fn open_partition(
        ctx: &dyn TxnCtx,
        table: u32,
        lo: Option<RawBound>,
        hi: Option<RawBound>,
        width: usize,
        keep: Option<Vec<bool>>,
    ) -> BatchScan {
        BatchScan {
            table,
            lo,
            hi,
            keep,
            batch: fold_batch(ctx.join_cells_budget(), width),
            done: false,
        }
    }

    /// Has this scan run out of rows? (Set by the batch that came up short.)
    pub(super) fn exhausted(&self) -> bool {
        self.done
    }

    /// The UNVISITED remainder of this scan: the bounds a fresh scan would
    /// need to fold exactly the rows this one has not handed out yet. Only
    /// meaningful while `!exhausted()`; that is precisely when the adaptive
    /// fold hands its tail to the morsel queue.
    pub(super) fn remainder(&self) -> (Option<RawBound>, Option<RawBound>) {
        (self.lo.clone(), self.hi.clone())
    }

    /// The next batch of at most `self.batch` filtered rows; empty when the
    /// scan is exhausted.
    pub(super) fn next(
        &mut self,
        ctx: &mut dyn TxnCtx,
        filter: Option<&ExprProgram>,
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        if self.done {
            return Ok(Vec::new());
        }
        let (rows, resume) = ctx.scan_rows_pruned(
            self.table,
            self.lo.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
            self.hi.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
            filter.map(|f| (f, params)),
            self.batch,
            self.keep.as_deref(),
        )?;
        // Short of the cap means the cursor ran out, not that the filter got
        // picky: `scan_rows_pruned` counts KEPT rows and keeps pulling past
        // rejected ones, so it only returns early when the range is done.
        if rows.len() < self.batch {
            self.done = true;
            return Ok(rows);
        }
        // At the cap the scan hands back the last kept row's raw key: resume
        // strictly after it.
        let key = resume.ok_or_else(|| internal("capped batch without a resume key"))?;
        self.lo = Some((key, false));
        Ok(rows)
    }
}

pub(crate) type RawBound = (Vec<u8>, bool);

/// Raw encoded-key bounds for a Phase-1 PK range (bounds are over the FIRST
/// PK column only), with prefix semantics for composite PKs:
///
/// - `enc(v)`       = `keycode::encode_key(&[v])` — a strict prefix of every
///   composite key whose first column equals `v`.
/// - `prefix_hi(v)` = `enc(v) ++ [0xFF]` — greater than every key whose first
///   column equals `v` (continuation tags are 0x00/0x01 < 0xFF) and less than
///   the encoding of any larger first-column value.
///
/// lo inclusive → (enc(v), true); lo exclusive → (prefix_hi(v), true);
/// hi inclusive → (prefix_hi(v), false); hi exclusive → (enc(v), false).
/// Single-column PKs get identical results by the same construction.
///
/// Returns `Ok(None)` when a bound resolves to NULL (empty result).
pub(crate) fn range_bounds(
    lo: Option<&KeyBound>,
    hi: Option<&KeyBound>,
    plan: &CompiledPlan,
    params: &[Value],
) -> Result<Option<(Option<RawBound>, Option<RawBound>)>> {
    let resolve = |b: &KeyBound| -> Result<Option<Value>> {
        let part = b.parts.first().ok_or_else(|| internal("range bound"))?;
        let v = resolve_part(part, plan, params)?;
        Ok(if v.is_null() { None } else { Some(v) })
    };
    let lo_k = match lo {
        None => None,
        Some(b) => match resolve(b)? {
            None => return Ok(None),
            Some(v) => Some(if b.inclusive {
                (enc1(&v), true)
            } else {
                (prefix_hi(&v), true)
            }),
        },
    };
    let hi_k = match hi {
        None => None,
        Some(b) => match resolve(b)? {
            None => return Ok(None),
            Some(v) => Some(if b.inclusive {
                (prefix_hi(&v), false)
            } else {
                (enc1(&v), false)
            }),
        },
    };
    Ok(Some((lo_k, hi_k)))
}

fn enc1(v: &Value) -> Vec<u8> {
    keycode::encode_key(std::slice::from_ref(v))
}

fn prefix_hi(v: &Value) -> Vec<u8> {
    let mut k = enc1(v);
    k.push(0xFF);
    k
}

/// ORDER BY over full table rows: `Value::sql_cmp` per column with NULLS
/// FIRST ascending; descending columns reverse their comparison (NULLS LAST).
/// Stable, so ties keep scan (PK) order.
/// Top-K variant of [`gather_rows`] for `ORDER BY … LIMIT`: scan paths route
/// through the bounded-heap [`TxnCtx::scan_rows_topk`]; point paths return
/// their at-most-one matching row (trivially the top-K).
#[allow(clippy::too_many_arguments)]
pub(super) fn gather_topk(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    order_by: &[(u16, SortDir, OrderColl)],
    keep: usize,
) -> Result<Vec<Vec<Value>>> {
    check_order_colls(order_by, ctx.host_colls())?;
    match access {
        AccessPath::PkRange { lo, hi } => {
            match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                None => Ok(Vec::new()),
                Some((lo_k, hi_k)) => ctx.scan_rows_topk(
                    table,
                    lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    filter.map(|f| (f, params)),
                    order_by,
                    keep,
                ),
            }
        }
        AccessPath::FullScan => {
            ctx.scan_rows_topk(table, None, None, filter.map(|f| (f, params)), order_by, keep)
        }
        // Point/index paths gather their matches — at most one for PK/unique,
        // every equal/in-range row for a non-unique index — then sort and cap.
        // These materialize all matches before truncating; a streaming index
        // cursor is deliberately deferred (#48) until a real workload shows
        // the cost.
        AccessPath::PkPoint(_)
        | AccessPath::IndexPoint { .. }
        | AccessPath::IndexRange { .. }
        | AccessPath::FtsScan { .. } => {
            let mut r = gather_rows(ctx, table, access, filter, plan, params, None)?;
            sort_rows(&mut r, order_by, ctx.host_colls());
            r.truncate(keep);
            Ok(r)
        }
    }
}

pub(super) fn sort_rows(
    rows: &mut [Vec<Value>],
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) {
    rows.sort_by(|a, b| cmp_rows(a, b, order_by, colls));
}

/// Every HOST collating sequence an `ORDER BY` names must be registered on the
/// connection running the sort. Checked ONCE, before any comparison, because a
/// `sort_by` comparator has nowhere to report a failure — and answering
/// "peers", or falling back to BINARY, would return rows in a silently wrong
/// order. sqlite's own message for the miss is reproduced verbatim, because
/// consumers (CPython's `test_deregister_collation`) assert on it.
pub(super) fn check_order_colls(
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) -> Result<()> {
    for (_, _, c) in order_by {
        if let Some(name) = c.host() {
            if !colls.is_some_and(|t| t.has(name)) {
                return Err(Error::Unsupported(format!(
                    "no such collation sequence: {name}"
                )));
            }
        }
    }
    Ok(())
}

/// Total sort order over two rows for an `ORDER BY` spec (column index,
/// direction + NULL placement, collation). Shared by [`sort_rows`] and the
/// streaming top-K heap. The [`Collation`] is applied to text keys and is
/// [`Collation::Binary`] (bytewise) for a plain `ORDER BY`.
///
/// The NULL placement is decided BEFORE the direction is applied, and is NOT
/// reversed by `DESC`: `NULLS FIRST` means first in the delivered order either
/// way. That is the whole content of the explicit clause — sqlite's DEFAULTS
/// (first for ASC, last for DESC) are what `SortDir::dir` already resolved to,
/// so a plain `ORDER BY x` and a plain `ORDER BY x DESC` come out here exactly
/// as they did before the clause existed.
pub(super) fn cmp_rows(
    a: &[Value],
    b: &[Value],
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) -> Ordering {
    for (col, dir, coll) in order_by {
        let (col, dir) = (*col, *dir);
        let (Some(x), Some(y)) = (a.get(col as usize), b.get(col as usize)) else {
            continue;
        };
        match (x.is_null(), y.is_null()) {
            // Two NULLs are peers on this key; fall through to the next one.
            (true, true) => continue,
            (true, false) => {
                return if dir.nulls_first { Ordering::Less } else { Ordering::Greater }
            }
            (false, true) => {
                return if dir.nulls_first { Ordering::Greater } else { Ordering::Less }
            }
            (false, false) => {}
        }
        let ord = cmp_order(x, y, coll, colls);
        if ord != Ordering::Equal {
            return if dir.desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}

/// Order two NON-NULL values (the NULL cases are settled by the caller's
/// placement rule before this is reached).
fn cmp_order(
    a: &Value,
    b: &Value,
    coll: &OrderColl,
    colls: Option<&dyn HostColls>,
) -> Ordering {
    // A HOST collating sequence orders TEXT against TEXT; every other pair is
    // settled by storage class, exactly as sqlite does (the callback is only
    // ever consulted for two text values). `check_order_colls` has already
    // guaranteed the name resolves, so a miss here is unreachable.
    let coll = match coll {
        OrderColl::Native(c) => *c,
        OrderColl::Host(name) => {
            return match (a, b, colls) {
                (Value::Text(x), Value::Text(y), Some(t)) => t.compare(name, x, y),
                _ => cmp_order(a, b, &OrderColl::Native(Collation::Binary), None),
            }
        }
    };
    // `sort_cmp`, not `sql_cmp`: an `any` column really can hold a number AND a
    // string, and sqlite orders those by storage class (NULL < numbers < text <
    // blob). `sql_cmp` refuses that pair, and turning the refusal into `Equal`
    // — as this did — is not an order: it left `ORDER BY` over such a column
    // returning rows in an arbitrary sequence.
    match a.sort_cmp(b, coll) {
        Some(ord) => ord,
        // A pair `sort_cmp` will not rank even by storage class (mpedb's own
        // Bool/Timestamp against another class): peers, as before.
        None => Ordering::Equal,
    }
}
