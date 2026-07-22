use super::*;
use mpedb_types::{HostAggState, HostAggs};

/// One aggregate's running state over one group: a built-in [`Accum`], or a HOST
/// aggregate's `xStep`/`xFinal` accumulator (design/DESIGN-UDF.md stage 2).
///
/// The two differ on NULL, deliberately. A built-in SKIPS NULL arguments
/// ([`Accum::push`] is where that rule lives); sqlite hands a USER aggregate
/// every row, NULLs included, and lets `xStep` decide — Django's `StdDevPop`
/// depends on it. DISTINCT and `FILTER (WHERE …)` are applied identically to
/// both: the filter by the caller before `push`, the dedup here.
enum Acc {
    Native(Accum),
    Host {
        state: Box<dyn HostAggState>,
        /// `f(DISTINCT x)`: values already stepped in this group, keyed by their
        /// storage-class grouping encoding — the same mechanism
        /// [`Accum::new_distinct`] uses.
        /// `None` for a non-DISTINCT call, so the common case pays nothing.
        seen: Option<std::collections::BTreeSet<Vec<u8>>>,
        /// The argument column's declared collation, folded into the dedup key
        /// exactly as [`Accum`] does for a native aggregate.
        coll: Collation,
    },
}

impl Acc {
    /// Feed one row. `None` is `count(*)`'s "the row itself" — never produced for
    /// a host aggregate (the plan decoder rejects a host call with no argument).
    fn push(&mut self, v: Option<&Value>) -> Result<()> {
        self.push_n(v, &[])
    }

    /// Feed one row, with a host aggregate's arguments AFTER the first.
    /// `extra` is always empty for a built-in (mpedb has no multi-argument
    /// native aggregate), and the DISTINCT dedup key stays the FIRST argument —
    /// which is the only argument a DISTINCT call may have (the parser refuses
    /// `f(DISTINCT a, b)`).
    fn push_n(&mut self, v: Option<&Value>, extra: &[Value]) -> Result<()> {
        match self {
            Acc::Native(a) => a.push(v),
            Acc::Host { state, seen, coll } => {
                let mut all: Vec<Value>;
                let args = match v {
                    Some(v) if extra.is_empty() => std::slice::from_ref(v),
                    Some(v) => {
                        all = Vec::with_capacity(1 + extra.len());
                        all.push(v.clone());
                        all.extend_from_slice(extra);
                        &all[..]
                    }
                    None => &[][..],
                };
                if let Some(seen) = seen {
                    if !seen.insert(keycode::encode_group_key(args, std::slice::from_ref(coll))) {
                        return Ok(());
                    }
                }
                state.step(args)
            }
        }
    }

    fn finish(self) -> Result<Value> {
        match self {
            Acc::Native(a) => Ok(a.finish()),
            // sqlite frees the aggregate context right after `xFinal`; consuming
            // the boxed state here is that free.
            Acc::Host { state, .. } => state.finish(),
        }
    }
}

/// Mint a fresh accumulator for one aggregate over one group. A host aggregate
/// needs the connection's factory table, so this is where a plan naming one
/// executed OUT of scope (the write path, the streaming/overlay read paths)
/// surfaces a clean error instead of a wrong answer.
///
/// `a.coll` — the ARGUMENT's collating sequence, binder-computed (format 60) —
/// reaches both collation-sensitive accumulations: the DISTINCT dedup fold and
/// the MIN/MAX compare (`tests/agg_collate.rs`; under BINARY a NOCASE column
/// holding 'a','B' answered `min='B'`, a live wrong answer).
fn new_accum(a: &AggCall, host: Option<&dyn HostAggs>) -> Result<Acc> {
    let coll = a.coll;
    let Some(name) = a.func.host() else {
        let f = a.func.native().ok_or_else(|| internal("aggregate with no target"))?;
        return Ok(Acc::Native(if a.distinct {
            Accum::new_distinct_collated(f, coll)
        } else {
            Accum::new_collated(f, coll)
        }));
    };
    let host = host.ok_or_else(|| {
        Error::Unsupported(format!(
            "host aggregate {name}() is not in scope for this execution"
        ))
    })?;
    // The plan's shape IS the arity: a host call always carries its first
    // argument (decode enforces it) plus however many `extra_args` it was
    // written with, so the registry lookup asks for exactly that count.
    let argc = a.arg.is_some() as i32 + a.extra_args.len() as i32;
    Ok(Acc::Host {
        state: host.create(name, argc)?,
        seen: a.distinct.then(std::collections::BTreeSet::new),
        coll,
    })
}

/// The bare-column witness row a group carries (sqlite's "bare columns", see
/// [`exec_aggregate`]), in one of two shapes chosen by the aggregate set.
enum Witness {
    // #87: `extreme` is the running min/max (None until the first non-NULL
    // arg); `bare` is the extremum row's values for `agg.bare_cols`.
    MinMax { extreme: Option<Value>, bare: Vec<Value> },
    // sqlite's arbitrary pick: `pk` is the encoded PK of the lowest-rowid row
    // seen so far (empty = no row yet); `bare` is that row's bare values.
    MinRowid { pk: Vec<u8>, bare: Vec<Value> },
}

/// One group's state: `[group key values]`, one accumulator per aggregate,
/// the optional bare-column witness, and the **first** per-row param scratch
/// seen for this group (correlated slots filled). HAVING and a SELECT-list
/// expression that is not a group key evaluate against that scratch so a
/// correlated subquery keyed by the group (Django `OuterRef("pk")`) is correct.
/// When correlation varies inside the group, the first row wins — matching
/// sqlite's bare-column pick under PK/scan order. O(1) in input rows (#123 §5.1).
type Group = (Vec<Value>, Vec<Acc>, Option<Witness>, Option<Vec<Value>>);

/// The fold: everything an aggregate holds between rows.
///
/// **This is the single row-processing body**, deliberately. #123 §6 names "a
/// second code path per shape" as the real cost of streaming — `stream.rs`
/// exists because of the first one, and `tests/stream_correctness.rs` exists
/// because that path silently returned wrong answers. So the streaming input
/// and the materializing input differ ONLY in where the rows come from; both
/// call [`Folder::push`], one row at a time, in scan order. There is no second
/// implementation of grouping, of the FILTER clause, or of the witness rule to
/// drift.
struct Folder<'a> {
    agg: &'a Aggregation,
    t: &'a TableDef,
    schema: &'a Schema,
    table: u32,
    group_collations: Vec<Collation>,
    has_bare: bool,
    /// The single min/max aggregate that governs the bare-column witness, if
    /// the aggregate set has exactly one (see [`exec_aggregate`]).
    mm: Option<(&'a AggCall, mpedb_types::AggFn)>,
    /// Groups, keyed by the memcmp-ordered keycode of the group columns — so
    /// they come out in a deterministic (and sqlite-matching) order for free.
    groups: std::collections::BTreeMap<Vec<u8>, Group>,
    /// Reused expression-eval stack: one live fold evaluates its per-row
    /// programs thousands of times, and `eval_host`'s fresh stack was a
    /// measurable slice of the per-row constant (examples/agg_prof.rs).
    stack: Vec<Value>,
    /// Reused group-key encode buffer — the per-row `Vec<u8>` the BTreeMap is
    /// probed with; an OWNED copy is cloned off it only when a new group is
    /// born.
    key_buf: Vec<u8>,
    /// Computed `GroupKey::Expr` values for the CURRENT row, stashed during
    /// encoding so a new group can take them without re-evaluating.
    expr_keys: Vec<Value>,
    /// Per aggregate: `Some(c)` when its argument program is the single
    /// instruction `PushCol(c)` — `sum(a)`, `min(x)`, the overwhelmingly
    /// common shapes — so the fold reads the column BY REFERENCE instead of
    /// running the interpreter and cloning the value per row. `None` runs the
    /// program exactly as before.
    fast_args: Vec<Option<u16>>,
    /// #123 §4.3: the input is no longer held, but the GROUP MAP is, and it is
    /// O(distinct keys) — no chunk size makes it smaller. So it takes the
    /// tripwire the join's intermediate product already has, on the same
    /// `[runtime] max_join_cells` knob. Charged once per group created, which
    /// is why an unbounded `GROUP BY` is *governed* rather than silently
    /// growing until the OOM killer arrives.
    cells: super::gather::JoinCells,
}

impl<'a> Folder<'a> {
    fn new(
        ctx: &dyn TxnCtx,
        schema: &'a Schema,
        plan: &CompiledPlan,
        t: &'a TableDef,
        table: u32,
        joins: &[Join],
        agg: &'a Aggregation,
    ) -> Folder<'a> {
        // sqlite "bare columns" (COMPAT.md): each group also carries the values
        // of `agg.bare_cols` from a WITNESS row. Which row is sqlite's — and so
        // mpedb's — is inferable from the aggregate set (no plan byte, so
        // PLAN_FORMAT stays put):
        //   * EXACTLY ONE min()/max() (even alongside count/sum/avg) → that
        //     extremum's row, sqlite's documented rule (#87, verified:
        //     `min(x), count(*)` follows the min);
        //   * ZERO min()/max() → sqlite's "arbitrary" pick, which is
        //     deterministic: the group's LOWEST-ROWID row. mpedb identifies a
        //     single-table row by its PK, so it tracks the minimum PK per group
        //     and takes that row. The planner only emits this shape over a
        //     single INTEGER-PK table (`rowid_pick_ok`), where PK == sqlite's
        //     rowid, so the pick matches sqlite EXACTLY.
        // In the ≥2 min/max case sqlite follows the LAST min/max — an
        // order-dependent, undocumented pick the planner refuses, with ONE
        // carve-out: when that last min/max has a non-NULL CONSTANT argument and
        // no FILTER it only "improves" on the group's first row, so sqlite's pick
        // IS the lowest-rowid row and the planner admits it under
        // `rowid_pick_ok`. Such plans land in the min-PK branch below (`mm` is
        // None for anything but exactly one min/max), which is exactly the right
        // witness; a forged plan falls into the same safe branch.
        //
        // A HOST aggregate is never a min/max: `AggTarget::native()` is None for
        // one, so it can neither govern the witness nor be miscounted here.
        let mm: Option<(&AggCall, mpedb_types::AggFn)> = {
            let mut it = agg.aggs.iter().filter_map(|c| match c.func.native() {
                Some(f @ (mpedb_types::AggFn::Min | mpedb_types::AggFn::Max)) => Some((c, f)),
                _ => None,
            });
            match (it.next(), it.next()) {
                // Exactly one min/max → its witness row governs the bare columns.
                (Some(c), None) => Some(c),
                // Zero (→ lowest rowid) or two-plus (planner-refused) → min-PK.
                _ => None,
            }
        };
        // The collation each GROUP BY key groups under: a bare NOCASE/RTRIM
        // column collapses case-/space-variants into ONE group (sqlite parity);
        // a computed key is BINARY. Folded before encoding, so `'abc'` and
        // `'ABC'` (NOCASE) hash to the same bucket — the equality half of the
        // column's declared collation.
        let base_colls = super::base_row_collations(schema, plan, table, joins);
        let group_collations: Vec<Collation> = agg
            .group_by
            .iter()
            .map(|k| match k {
                GroupKey::Col(c) => {
                    base_colls.get(*c as usize).copied().unwrap_or(Collation::Binary)
                }
                GroupKey::Expr(_) => Collation::Binary,
            })
            .collect();
        Folder {
            agg,
            t,
            schema,
            table,
            has_bare: !agg.bare_cols.is_empty(),
            group_collations,
            mm,
            fast_args: agg
                .aggs
                .iter()
                .map(|c| match c.arg.as_ref().map(|p| p.instrs.as_slice()) {
                    Some([mpedb_types::Instr::PushCol(i)]) => Some(*i),
                    _ => None,
                })
                .collect(),
            groups: Default::default(),
            stack: Vec::new(),
            key_buf: Vec::new(),
            expr_keys: Vec::new(),
            cells: super::gather::JoinCells::new(ctx.join_cells_budget()),
        }
    }

    /// Fold ONE base row into the group state.
    ///
    /// `row_params` is the parameter vector for this row's per-row programs:
    /// the correlated scratch when the plan has correlated subplans, otherwise
    /// `params` itself. `ctx` is borrowed SHARED — the fold reads host UDF /
    /// aggregate registries and nothing else, which is exactly what lets the
    /// caller alternate between pulling a batch (`&mut ctx`) and folding it.
    fn push(&mut self, ctx: &dyn TxnCtx, row: &[Value], row_params: &[Value]) -> Result<()> {
        let host = ctx.host_fns();
        // The grouping key is sqlite's storage-class key, NOT the on-disk one:
        // over a typeless column `1` and `1.0` are ONE group and the text `'1'`
        // another. Its byte order is also sqlite's class order, so this
        // `BTreeMap` still iterates groups the way sqlite emits them.
        //
        // Encoded STRAIGHT off the row into a reused buffer — the per-row
        // `Vec<Value>` of cloned key values only ever fed this encoder, so a
        // hot group now costs one encode and one map probe, no allocation.
        // `GroupKey::Expr` values are computed once and stashed for the
        // new-group path below.
        self.key_buf.clear();
        self.expr_keys.clear();
        for (i, k) in self.agg.group_by.iter().enumerate() {
            let coll =
                self.group_collations.get(i).copied().unwrap_or(Collation::Binary);
            match k {
                GroupKey::Col(c) => keycode::encode_group_value(
                    &mut self.key_buf,
                    row.get(*c as usize).unwrap_or(&Value::Null),
                    coll,
                ),
                // A computed key — `GROUP BY a+1` — evaluated over the base row.
                GroupKey::Expr(p) => {
                    let v = p.eval_with_stack_host(&mut self.stack, row, row_params, host)?;
                    keycode::encode_group_value(&mut self.key_buf, &v, coll);
                    self.expr_keys.push(v);
                }
            }
        }
        // Not `or_insert_with`: minting a HOST accumulator can FAIL (an
        // out-of-scope or unregistered aggregate), and a closure cannot carry
        // that error out. ONE probe per row — a `get_mut`-then-insert split
        // was measurably slower on a 10k-group map — at the price of cloning
        // the (small, often empty) encoded key per row; `BTreeMap` has no
        // borrowed-key entry API to avoid that with.
        let entry = match self.groups.entry(self.key_buf.clone()) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                // Materialize the key tuple this group owns: column keys
                // cloned off the row, computed keys taken from the stash.
                let mut expr_it = self.expr_keys.drain(..);
                let key_vals: Vec<Value> = self
                    .agg
                    .group_by
                    .iter()
                    .map(|k| match k {
                        GroupKey::Col(c) => row.get(*c as usize).cloned().unwrap_or(Value::Null),
                        GroupKey::Expr(_) => expr_it.next().unwrap_or(Value::Null),
                    })
                    .collect();
                drop(expr_it);
                let witness = if !self.has_bare {
                    None
                } else if self.mm.is_some() {
                    Some(Witness::MinMax { extreme: None, bare: Vec::new() })
                } else {
                    Some(Witness::MinRowid { pk: Vec::new(), bare: Vec::new() })
                };
                let accs = self
                    .agg
                    .aggs
                    .iter()
                    // The factory table for host aggregates is consulted per
                    // GROUP (one fresh accumulator each), not per row.
                    .map(|c| new_accum(c, ctx.host_aggs()))
                    .collect::<Result<Vec<Acc>>>()?;
                // One group's resident cells: its key tuple, its accumulators,
                // and its witness row. Charged before the group exists, so an
                // unbounded GROUP BY trips the knob instead of the OOM killer.
                let (schema, table) = (self.schema, self.table);
                self.cells.charge(
                    (key_vals.len() + accs.len() + self.agg.bare_cols.len()) as u64,
                    || {
                        format!(
                            "the group map of an aggregate over \"{}\"",
                            super::table_name(schema, table)
                        )
                    },
                )?;
                e.insert((key_vals, accs, witness, None))
            }
        };
        // First-row correlation scratch for this group (sqlite bare-column
        // convention). Django OuterRef on a group key is constant within the
        // group, so first vs last is the same; when correlation varies, this
        // matches sqlite's pick under PK/scan order.
        if entry.3.is_none() {
            entry.3 = Some(row_params.to_vec());
        }
        for (i, call) in self.agg.aggs.iter().enumerate() {
            // `agg(x) FILTER (WHERE cond)`: this row feeds THIS aggregate only
            // when `cond` is TRUE over the base row (3VL — NULL/FALSE skip). The
            // filter is per-aggregate, so two aggregates in one SELECT can have
            // different filters (or none). For DISTINCT this runs BEFORE the
            // dedupe inside `Accum` — filter first, then dedupe.
            if let Some(f) = &call.filter {
                if !f.eval_filter_host(&mut self.stack, row, row_params, host)? {
                    continue;
                }
            }
            match (&call.arg, self.fast_args[i]) {
                // count(*): the ROW is the input, so nothing is evaluated and
                // NULL cannot arise.
                (None, _) => entry.1[i].push(None)?,
                // A bare-column argument, read by reference — same value, same
                // out-of-bounds refusal, no interpreter and no clone.
                (Some(_), Some(c)) if call.extra_args.is_empty() => {
                    let v = row.get(c as usize).ok_or_else(|| {
                        Error::Internal(format!("column index {c} out of row bounds"))
                    })?;
                    entry.1[i].push(Some(v))?;
                }
                (Some(p), _) => {
                    let v = p.eval_with_stack_host(&mut self.stack, row, row_params, host)?;
                    // A host aggregate's arguments after the first, evaluated
                    // over the same base row and in the same order. Empty for
                    // every built-in, so this allocates nothing for them.
                    let mut extra = Vec::with_capacity(call.extra_args.len());
                    for p in &call.extra_args {
                        extra.push(p.eval_with_stack_host(&mut self.stack, row, row_params, host)?);
                    }
                    entry.1[i].push_n(Some(&v), &extra)?;
                }
            }
        }
        // Update the bare-column witness, reproducing sqlite's rule EXACTLY.
        if let Some(w) = entry.2.as_mut() {
            let capture = || -> Vec<Value> {
                self.agg
                    .bare_cols
                    .iter()
                    .map(|&c| row.get(c as usize).cloned().unwrap_or(Value::Null))
                    .collect()
            };
            match w {
                // The bare columns come from the row that achieved the min/max; on
                // a tie the FIRST such row wins (`min_max_prefers` replaces only on
                // a strict beat); and until any non-NULL extremum is seen the
                // witness tracks the LATEST row, so an all-NULL-argument group
                // takes its bare values from its last row — differential-tested
                // against sqlite 3.45.
                Witness::MinMax { extreme, bare } => {
                    let (mm, mm_fn) =
                        self.mm.expect("MinMax witness implies a single min/max aggregate");
                    // A FILTER on the governing min/max restricts BOTH the
                    // extremum AND the witness row to the rows it accepts
                    // (verified vs sqlite 3.45: the bare column follows the
                    // FILTERED extremum). A rejected row contributes nothing —
                    // except that when the filter rejects EVERY row in the group,
                    // sqlite falls back to the group's FIRST row for the bare
                    // values, so seed `bare` from the first row while it is still
                    // empty. With no filter this is byte-identical to before
                    // (`passes` is always true, and the extreme=None branch already
                    // captured every row, so the last row wins as documented).
                    let passes = match &mm.filter {
                        Some(fp) => {
                            fp.eval_filter_host(&mut self.stack, row, row_params, host)?
                        }
                        None => true,
                    };
                    if !passes {
                        if bare.is_empty() {
                            *bare = capture();
                        }
                    } else {
                        let v = match &mm.arg {
                            Some(p) => {
                                p.eval_with_stack_host(&mut self.stack, row, row_params, host)?
                            }
                            None => Value::Null,
                        };
                        match extreme {
                            None => {
                                *bare = capture();
                                if !v.is_null() {
                                    *extreme = Some(v);
                                }
                            }
                            Some(e) => {
                                // The ARGUMENT's collation (format 60): the
                                // witness follows the same extremum the
                                // accumulator keeps, tie rule included.
                                if !v.is_null() && mm_fn.min_max_prefers(e, &v, mm.coll)? {
                                    *extreme = Some(v);
                                    *bare = capture();
                                }
                            }
                        }
                    }
                }
                // sqlite's "arbitrary" pick is the group's lowest-rowid row. The
                // row's PK identifies it; over a single INTEGER-PK table (the only
                // shape the planner emits this for) the PK IS sqlite's rowid, so
                // the smallest encoded PK is sqlite's pick. Encoding gives memcmp
                // order = ascending PK, so tracking the running minimum matches
                // sqlite even when the scan is NOT PK-ordered (an index or
                // descending-range access path).
                Witness::MinRowid { pk, bare } => {
                    let pk_vals: Vec<Value> = self
                        .t
                        .primary_key
                        .iter()
                        .map(|&c| row.get(c as usize).cloned().unwrap_or(Value::Null))
                        .collect();
                    let this = keycode::encode_key(&pk_vals);
                    if pk.is_empty() || this < *pk {
                        *pk = this;
                        *bare = capture();
                    }
                }
            }
        }
        Ok(())
    }
}

/// The `count(*)`-only fast path (#126): `Some(n)` when every aggregate is a
/// bare `count(*)` and the input is a FILTERLESS streaming-shape scan, so the
/// row count IS the key count of the range and the context may count
/// leaf-wholesale ([`TxnCtx::count_rows_range`]) without materializing a row.
/// `None` falls through to the fold, which stays the semantics of record.
///
/// The guards, each load-bearing:
/// - **no residual `filter`** — DESIGN-MULTIDB §4: the residual is where the
///   planner AND-folded the row policy, and a policy-bearing scan must test
///   every row;
/// - **every aggregate is `count(*)`** with no argument, DISTINCT, FILTER or
///   host extra args — anything else needs column values;
/// - **no GROUP BY, no bare columns** — grouping and the witness need values;
/// - the same streaming-shape checks as [`gather::BatchScan::open`] (an
///   incremental context, a real table, a FullScan/PkRange access).
///
/// The #74 charges are the drain-scan's exactly (`ReadTxn::count_range`), so
/// the budget contract does not move by a row.
#[allow(clippy::too_many_arguments)]
fn try_count_only(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    plan: &CompiledPlan,
    params: &[Value],
    t: &TableDef,
    filter: Option<&ExprProgram>,
    agg: &Aggregation,
) -> Result<Option<u64>> {
    let plain_count = |c: &AggCall| {
        c.arg.is_none()
            && !c.distinct
            && c.filter.is_none()
            && c.extra_args.is_empty()
            && c.func.native() == Some(mpedb_types::AggFn::Count)
    };
    if filter.is_some()
        || !agg.group_by.is_empty()
        || !agg.bare_cols.is_empty()
        || agg.aggs.is_empty()
        || !agg.aggs.iter().all(plain_count)
        || !ctx.scans_incrementally()
        || table == mpedb_sql::DUAL_TABLE
        || table == mpedb_sql::CTE_TABLE
        || t.primary_key.is_empty()
    {
        return Ok(None);
    }
    let (lo, hi) = match access {
        AccessPath::FullScan => (None, None),
        AccessPath::PkRange { lo, hi } => {
            match gather::range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                // A NULL bound makes the range predicate UNKNOWN for every
                // row: zero rows, counted here exactly as the born-exhausted
                // scan would have.
                None => return Ok(Some(0)),
                Some((l, h)) => (l, h),
            }
        }
        AccessPath::PkPoint(_)
        | AccessPath::IndexPoint { .. }
        | AccessPath::IndexRange { .. }
        | AccessPath::FtsScan { .. } => return Ok(None),
    };
    ctx.count_rows_range(
        table,
        lo.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
        hi.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
    )
}

/// The aggregate-over-index-tree path (format 59): `Some(accs)` — one FINISHED
/// set of accumulators for the statement's single group — when the plan
/// carries `over_index` and this context can serve it; `None` falls back to
/// the row fold, which stays the semantics of record.
///
/// Sound by the membership rule: a row with ANY NULL indexed column has no
/// entry, and the value aggregates skip NULLs — the tree omits exactly the
/// rows the fold would have ignored. `count(*)` is only planned onto an
/// all-NOT-NULL index (entry count == row count). Three modes, cheapest first:
///
/// - **wholesale count** — every aggregate is `count(*)` or `count(lead)`:
///   the entry count IS each answer; no key is ever read.
/// - **boundary probes** — every aggregate is `min(lead)`/`max(lead)`:
///   O(log n) per distinct extremum; the value is re-fetched from the ROW
///   (bit-exact — a stored `-0.0` whose key image is canonicalized comes back
///   signed), and "first entry of the run" reproduces the fold's
///   first-strict-beat tie rule. An empty tree probes to `None` and the
///   never-pushed accumulator finishes to NULL, exactly the fold's answer.
/// - **index-tree scan** — mixes with `sum`/`avg`/`total`: every decoded
///   leading value feeds the SAME [`Accum`] the row fold uses, so overflow
///   (`sum` int64 — a raise in both engines), avg's divide-by-non-NULL-count
///   and the empty-input NULLs cannot drift. Fold order is KEY order, not PK
///   order — the same order sqlite folds in when it picks the same index, and
///   the only way a partial `sum` overflow can differ from the table fold
///   (documented, differential-tested). Float keys decode to the canonical
///   member of their slot (`-0.0` → `0.0`, one NaN image) — equal under every
///   SQL comparison to what the row holds.
///
/// #74 charges are the engine methods' — per entry visited (scan/count), one
/// per probed row — deterministic per snapshot and documented there. For
/// `count(*)` the charge equals the table drain's exactly (entry count ==
/// row count on an all-NOT-NULL index).
fn try_agg_index(
    ctx: &mut dyn TxnCtx,
    table: u32,
    agg: &Aggregation,
    filter: Option<&ExprProgram>,
) -> Result<Option<Vec<Acc>>> {
    let Some(ix) = agg.over_index else {
        return Ok(None);
    };
    if !ctx.agg_over_index_supported() {
        return Ok(None); // row-fold fallback: same answer, one semantics
    }
    // Shape re-guards (validate enforces these against the schema already;
    // re-checking here costs nothing and keeps this path locally provable).
    if filter.is_some() || !agg.group_by.is_empty() || !agg.bare_cols.is_empty()
        || agg.aggs.is_empty()
    {
        return Ok(None);
    }
    use mpedb_types::AggFn as F;
    let native: Vec<F> = agg
        .aggs
        .iter()
        .filter_map(|c| c.func.native())
        .collect();
    if native.len() != agg.aggs.len() {
        return Ok(None); // a host aggregate never rides the index
    }
    // Mode 1: every aggregate is a count — `count(*)` or `count(lead)`, both
    // of which the entry count answers (the planner admitted them onto this
    // tree under exactly that rule).
    if native.iter().all(|f| *f == F::Count) {
        let n = ctx.count_index_entries(table, ix)?;
        let accs = agg
            .aggs
            .iter()
            .map(|_| {
                let mut a = Accum::new(F::Count);
                a.add_rows(n);
                Acc::Native(a)
            })
            .collect();
        return Ok(Some(accs));
    }
    // Mode 2: min/max only — boundary probes, one per distinct direction.
    if native.iter().all(|f| matches!(f, F::Min | F::Max)) {
        let mut min_row: Option<Option<Vec<Value>>> = None; // memoized probes
        let mut max_row: Option<Option<Vec<Value>>> = None;
        let mut accs = Vec::with_capacity(agg.aggs.len());
        for (call, f) in agg.aggs.iter().zip(&native) {
            let row = match f {
                F::Min => {
                    if min_row.is_none() {
                        min_row = Some(ctx.index_boundary_row(table, ix, false)?);
                    }
                    min_row.as_ref().expect("just filled")
                }
                _ => {
                    if max_row.is_none() {
                        max_row = Some(ctx.index_boundary_row(table, ix, true)?);
                    }
                    max_row.as_ref().expect("just filled")
                }
            };
            let mut a = Accum::new(*f);
            if let Some(row) = row {
                // The argument is the bare leading column (the admission
                // rule); read it off the probed row and push ONCE — a
                // one-value min/max finishes to that value, an empty tree
                // pushes nothing and finishes to NULL.
                let col = match call.arg.as_ref().map(|p| p.instrs.as_slice()) {
                    Some([mpedb_types::Instr::PushCol(c)]) => *c as usize,
                    _ => return Ok(None), // forged plan: fall back to the fold
                };
                let v = row.get(col).cloned().unwrap_or(Value::Null);
                a.push(Some(&v))?;
            }
            accs.push(Acc::Native(a));
        }
        return Ok(Some(accs));
    }
    // Mode 3: the index-tree scan. Every decoded leading value feeds every
    // accumulator — `count(lead)` counts it, `sum`/`avg`/`total` accumulate,
    // a mixed-in `min`/`max` takes the running extremum (same result as the
    // probe, already paid for by the scan).
    let mut accs: Vec<Accum> = native.iter().map(|f| Accum::new(*f)).collect();
    ctx.fold_index_leading(table, ix, &mut |v| {
        for a in &mut accs {
            a.push(Some(&v))?;
        }
        Ok(())
    })?;
    Ok(Some(accs.into_iter().map(Acc::Native).collect()))
}

/// `GROUP BY` / aggregates / `HAVING`.
///
/// **The first line is the invariant.** DESIGN-MULTIDB §4: aggregation must
/// consume rows only AFTER the merged `(WHERE ∧ effective-policy)` predicate.
/// `gather_rows` applies exactly that — the access path plus `filter`, which is
/// where the planner AND-folded the policy — so accumulating over its output
/// satisfies §4 by construction. Reading the raw scan instead would make
/// `count(*)` report rows the caller cannot see, and a count leaks existence
/// whether or not the rows come back. §4 calls it "a natural mistake, since some
/// policy conjuncts land in the residual"; the only defence is to never hold the
/// unfiltered stream. **The streaming path below obeys the same rule for the
/// same reason**: `BatchScan` pushes `filter` down into the scan, so the fold
/// never sees a row the merged predicate rejected.
///
/// The other trap: **LIMIT applies to GROUPS, not rows.** The non-aggregate path
/// bounds `gather_rows` by offset+limit, which would be silently wrong here —
/// `LIMIT 1` on a grouped query means one group, not one input row. So this
/// consumes the whole input and bounds at the end.
///
/// # Memory (#123 §5.1)
///
/// **An aggregate is a fold**, so its state is O(groups) and never O(rows).
/// This used to gather the entire input first and say so ("Unbounded on
/// purpose"): `SELECT count(*)` over 160 000 rows held 50.8 MB to produce one
/// integer, the worst held-to-answer ratio in the whole measurement
/// (design/DESIGN-STREAM-EXEC.md §2.1). It now drains the input in
/// [`BatchScan`]-sized batches and folds each batch into the accumulators
/// before drawing the next, so a bounded-group aggregate holds kilobytes
/// regardless of table size — asserted, not benchmarked, in
/// `tests/agg_stream.rs`.
///
/// Four shapes keep the materializing path, and each is a shape where it buys
/// nothing:
///
/// - **an aggregate over a JOIN** — the joined tuple stream is the join's
///   accumulated product, which `gather_joined` holds anyway (its own
///   `max_join_cells` budget governs it);
/// - **a correlated subplan or a correlated WHERE residual** — those run
///   `correlated_survivors` over the gathered set, keeping a per-row scratch
///   beside each row;
/// - **a non-PK-ordered access path** (index / FTS / point), which has no
///   resume key until #48;
/// - **a context whose `scan_rows_capped` materializes anyway** (every write
///   context) — see [`TxnCtx::scans_incrementally`].
///
/// What is left holding memory afterwards is exactly what a fold cannot shed:
/// the group map (O(distinct keys), now charged against `max_join_cells`), the
/// per-aggregate `DISTINCT` sets (O(distinct arg values) per aggregate per
/// group), and the grouped output. `count(DISTINCT x)` therefore still holds
/// its dedup set — but it no longer holds the ROWS as well, which was the
/// larger half.
#[allow(clippy::too_many_arguments)]
pub(super) fn exec_aggregate(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    t: &TableDef,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    joins: &[Join],
    joined_filter: Option<&ExprProgram>,
    agg: &Aggregation,
    projection: &[Projection],
    order_by: &[(u16, SortDir, mpedb_types::OrderColl)],
    order_over: OrderOver,
    order_junk: u16,
    limit: Option<u64>,
    offset: Option<u64>,
    distinct: bool,
    // First reserved result slot of THIS level (`subplan_base` at the top,
    // `sub.sub_base` for a nested aggregate subplan) — where the per-row
    // correlated fill writes. Unused when `correlated` is empty and
    // `post_filter` is `None`.
    base: usize,
    // #73 §1: the correlated subplans (per-row filled) and the correlated WHERE
    // residual. Empty/`None` for a plain aggregate, which behaves exactly as
    // before.
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
    // #125: which slots of the gathered tuple this aggregate can observe.
    // `count(*)` observes none of them, so its input is folded from EMPTY rows.
    prune: Option<&RowPrune>,
) -> Result<ExecResult> {
    let mut folder = Folder::new(&*ctx, schema, plan, t, table, joins, agg);

    // The base row's width in cells: what one batch of the streaming drain
    // holds, and the divisor that sizes it.
    let width = t.columns.len();
    // STREAMING FOLD (#123 §5.1). Only for the shapes where the input is a
    // plain PK-ordered scan of one table with no correlated machinery — see the
    // doc comment's list. `BatchScan::open` answers `None` for everything else,
    // and the materializing path below is then EXACTLY the code that ran
    // before, feeding the same `Folder::push`.
    let streamed = if joins.is_empty() && correlated.is_empty() && post_filter.is_none() {
        if let Some(accs) = try_agg_index(ctx, table, agg, filter)? {
            // Aggregate-over-index-tree (format 59): the tree answered and no
            // base row was ever materialized. Inject the one group the fold
            // would have built — same (empty) key, same group-map cell charge —
            // and let the shared finish code below do everything observable
            // (HAVING, projection, ORDER BY, LIMIT).
            folder.cells.charge(agg.aggs.len() as u64, || {
                format!(
                    "the group map of an aggregate over \"{}\"",
                    super::table_name(schema, table)
                )
            })?;
            folder.groups.insert(Vec::new(), (Vec::new(), accs, None, None));
            true
        } else if let Some(n) = try_count_only(ctx, table, access, plan, params, t, filter, agg)? {
            // `count(*)`-only over a filterless scan: the engine counted KEYS
            // leaf-wholesale and no row was ever materialized. Inject the one
            // group the fold would have built — same group key (empty), same
            // accumulators, same group-map cell charge — and let the shared
            // finish code below do everything observable.
            folder.cells.charge(agg.aggs.len() as u64, || {
                format!(
                    "the group map of an aggregate over \"{}\"",
                    super::table_name(schema, table)
                )
            })?;
            let accs = agg
                .aggs
                .iter()
                .map(|_| {
                    let mut a = Accum::new(mpedb_types::AggFn::Count);
                    a.add_rows(n);
                    Acc::Native(a)
                })
                .collect();
            folder.groups.insert(Vec::new(), (Vec::new(), accs, None, None));
            true
        } else {
            // #125's scan half: the fold observes `prune.stage(0)` (group
            // keys, aggregate arguments and their FILTERs, the witness PK) and
            // the residual reads its own columns — everything else is never
            // DECODED. `scan_keep` is that union; `None` keeps the scan
            // byte-identical to the unpruned one.
            let keep = prune.and_then(|p| gather::scan_keep(p.stage(0), filter, width));
            match gather::BatchScan::open(&*ctx, table, access, plan, params, t, width, keep)? {
                Some(mut scan) => {
                    loop {
                        // Draw a batch (`&mut ctx`), fold it (`&ctx`), drop it.
                        // The batch is the only input residency that ever exists.
                        let batch = scan.next(ctx, filter, params)?;
                        if batch.is_empty() {
                            break;
                        }
                        for row in &batch {
                            folder.push(&*ctx, row, params)?;
                        }
                    }
                    true
                }
                None => false,
            }
        }
    } else {
        false
    };

    if !streamed {
        // Unbounded on purpose: see the LIMIT note above. Over a join the row
        // being aggregated is the JOINED row — same rule, wider row.
        let rows = match joins.is_empty() {
            // #125: the fold reads only what `agg`/`group_by`/`FILTER` name
            // (plus the PK, for the bare-column witness), so everything else is
            // dropped as the set is read. This is the materializing path — a
            // WRITE context, or a correlated aggregate whose per-row scratch the
            // streaming fold cannot carry — and it is exactly where the input IS
            // held.
            true => match prune {
                Some(p) => gather::gather_narrowed(
                    ctx, table, access, filter, plan, params, t, p.stage(0),
                )?,
                None => gather_rows(ctx, table, access, filter, plan, params, None)?,
            },
            false => gather_joined(
                ctx, plan, params, schema, table, access, filter, joins, joined_filter, prune,
            )?,
        };

        // #73 §1: aggregate over a correlated filter. Fill each correlated slot per
        // gathered row and apply the correlated WHERE residual BEFORE grouping, so
        // accumulation still consumes only the full `(WHERE ∧ policy)` set
        // (DESIGN-MULTIDB §4 — the same ordering the plain gather guarantees). The
        // shared `correlated_survivors` keeps this byte-identical to the
        // non-aggregate correlated path, memo included.
        //
        // Each survivor keeps its SCRATCH — the `[user ‖ subplan results]` vector
        // with this row's correlated slots filled. `Folder::push` evaluates the
        // PER-ROW programs (`FILTER (WHERE …)`, an aggregate argument, a computed
        // GROUP BY key) against that scratch, not against `params`: `params` still
        // holds NULL in every correlated slot, and a filter that reads one would
        // evaluate to NULL and — 3VL — DROP the row rather than test it. That is the
        // wrong answer `count(*) FILTER (WHERE EXISTS (…correlated…))` used to give,
        // 0 for both the positive and the negated form. The GROUPED programs (HAVING,
        // non-key projection) read the group's first-row scratch after collapse
        // (sqlite bare-column convention; Django OuterRef on a group key).
        let (rows, scratches): (Vec<Vec<Value>>, Option<Vec<Vec<Value>>>) =
            if correlated.is_empty() && post_filter.is_none() {
                (rows, None)
            } else {
                let survivors = correlated_survivors(
                    ctx, schema, plan, params, base, rows, correlated, post_filter,
                )?;
                let mut kept = Vec::with_capacity(survivors.len());
                let mut scratch = Vec::with_capacity(survivors.len());
                for (row, s) in survivors {
                    kept.push(row);
                    scratch.push(s);
                }
                (kept, Some(scratch))
            };

        for (ri, row) in rows.iter().enumerate() {
            // The parameter vector for THIS row's per-row programs: the correlated
            // scratch when this plan has correlated subplans, otherwise `params`
            // itself (no clone, no correlated slot to fill). The two agree on every
            // user and uncorrelated-subplan slot — the scratch is a copy of `params`
            // with only the correlated slots overwritten — so this is a no-op except
            // exactly where a correlated slot is read.
            let row_params: &[Value] = match &scratches {
                Some(s) => &s[ri],
                None => params,
            };
            folder.push(&*ctx, row, row_params)?;
        }
    }

    let Folder { groups, .. } = folder;

    // (grouped tuple, first-row param scratch for correlated HAVING/projection)
    let mut out: Vec<(Vec<Value>, Option<Vec<Value>>)> = Vec::new();
    // `SELECT count(*) FROM t` over an EMPTY table must return one row (0), not
    // zero rows — there is one group when there is nothing to group by. With a
    // GROUP BY, an empty input means no groups at all.
    if groups.is_empty() && agg.group_by.is_empty() {
        // An EMPTY group still FINISHES a fresh accumulator — for a host
        // aggregate that is `xFinal` on a never-stepped (NULL) aggregate
        // context, which is exactly sqlite's rule and why Django's
        // `STDDEV_POP` over no rows is NULL rather than an error.
        let accs = agg
            .aggs
            .iter()
            .map(|c| new_accum(c, ctx.host_aggs()))
            .collect::<Result<Vec<Acc>>>()?;
        let mut tuple: Vec<Value> =
            accs.into_iter().map(Acc::finish).collect::<Result<_>>()?;
        // No rows means no witness row, so a bare column is NULL — sqlite:
        // `SELECT name, max(x) FROM empty` yields one row `(NULL, NULL)`.
        tuple.resize(tuple.len() + agg.bare_cols.len(), Value::Null);
        out.push((tuple, None));
    }
    for (_, (keys, accs, witness, scratch)) in groups {
        let mut tuple = keys;
        for a in accs {
            tuple.push(a.finish()?);
        }
        // Every group has at least one row, so the witness `bare` is populated;
        // extend the grouped tuple to `[keys ‖ aggs ‖ bare]`.
        if let Some(w) = witness {
            let bare = match w {
                Witness::MinMax { bare, .. } | Witness::MinRowid { bare, .. } => bare,
            };
            tuple.extend(bare);
        }
        out.push((tuple, scratch));
    }

    // HAVING — over the GROUPED tuple, which is why it can see aggregates and
    // WHERE cannot. Correlated slots come from the group's first base-row scratch.
    if let Some(h) = &agg.having {
        let mut keep = Vec::with_capacity(out.len());
        for (tuple, scratch) in out {
            let eval_params: &[Value] = scratch.as_deref().unwrap_or(params);
            if h.eval_filter_host(&mut Vec::new(), &tuple, eval_params, ctx.host_fns())? {
                keep.push((tuple, scratch));
            }
        }
        out = keep;
    }

    // Sort the GROUPED tuple only when the indices refer to it; otherwise the
    // sort waits for the projection below. Scratches stay zipped with tuples.
    if order_over == OrderOver::Grouped && !order_by.is_empty() {
        super::gather::check_order_colls(order_by, ctx.host_colls())?;
        let colls = ctx.host_colls();
        out.sort_by(|(a, _), (b, _)| super::gather::cmp_rows(a, b, order_by, colls));
    }

    let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    let mut projected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (tuple, scratch) in out {
        let eval_params: &[Value] = scratch.as_deref().unwrap_or(params);
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => tuple
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("grouped projection column"))?,
                Projection::Expr { program, .. } => {
                    program.eval_host(&tuple, eval_params, ctx.host_fns())?
                }
            });
        }
        // `SELECT DISTINCT dept, count(*) … GROUP BY dept` — the groups are
        // already distinct by key, but the PROJECTION need not be (two groups
        // can share a count), so this still has work to do.
        if distinct && !seen.insert(keycode::encode_group_key(&orow, &[])) {
            continue;
        }
        projected.push(orow);
    }
    if order_over == OrderOver::Projection {
        super::gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut projected, order_by, ctx.host_colls());
    }
    // Sort-only columns come off after the sort, exactly as in the plain path —
    // `ORDER BY count(*) + 1` computes a column nobody asked to see.
    if order_junk > 0 {
        let keep = projection.len() - order_junk as usize;
        for row in &mut projected {
            row.truncate(keep);
        }
    }
    let projected: Vec<Vec<Value>> = projected.into_iter().skip(skip).take(take).collect();
    let columns = projection
        .iter()
        .take(projection.len() - order_junk as usize)
        .map(|p| match p {
            Projection::Column(i) => t
                .columns
                .get(*i as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col{i}")),
            Projection::Expr { name, .. } => name.clone(),
        })
        .collect();
    Ok(ExecResult::Rows {
        columns,
        rows: projected,
    })
}
