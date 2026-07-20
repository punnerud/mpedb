//! MPEE — the plan solver (task #114, design/DESIGN-MPEE-SOLVER.md).
//!
//! One mechanism, invoked at EVERY level of the compile recursion (the top
//! SELECT, each lifted subquery body, each derived body, each compound arm,
//! each recursive-CTE component — everything routes through
//! [`super::join::plan_join_select`]), that chooses the join order for that
//! scope. The access path per position then falls out of the order: mpedb's
//! #49/#55 equality consumption turns "this table is entered with its PK
//! pinned by a table already placed" into a probe.
//!
//! ## Cost, and what it refuses to invent
//!
//! `row_count` says how many rows EXIST, never how many a predicate LETS
//! THROUGH. Predicates are therefore classified, and the class decides what
//! may be assumed:
//!
//! - **KNOWN** — a full PK equality, or a full-width probe of a UNIQUE index:
//!   exactly one row.
//! - **BOUNDED** — a non-unique index equality, a constant anchor on a non-key
//!   column, any equality linking this table to an already-placed one: bounded
//!   above by `row_count(t)` and by nothing tighter.
//! - **UNKNOWN** — `LIKE '%x%'`, `f(col) > 0`, an `any`-column comparison, a
//!   bound-parameter range: NOTHING is known, and no constant is invented.
//!
//! BOUNDED and UNKNOWN are priced identically, at the full row count. The
//! solver therefore optimises the WORST case rather than the expected one —
//! for the UNKNOWN class that is the only defensible objective, and it is the
//! reason `select5.test`'s `join-17-4` is solvable at all: its problem is not
//! a slightly-off estimate, it is that the textual order's worst case is
//! astronomical while another order's is finite REGARDLESS of the data.
//!
//! The only statistic consulted is the catalog's exact `row_count`, and only
//! through a magnitude bucket (§`magnitude`), so a table must DOUBLE before
//! any cost can move. See design/DESIGN-MPEE-SOLVER.md §6 for why that is a
//! stability property and not a safety one: plan bytes are content-hashed, so
//! a different choice is a different hash by construction and the same hash
//! can never name two plans.

use super::*;

/// Coarse table-size input: `⌈log2⌉`-ish magnitude of a row count.
///
/// `0 → 0`, `1 → 1`, `2..3 → 2`, `4..7 → 3`, … Costs are sums of these, i.e.
/// logs of a product. Quantizing here is what makes the chosen plan stable
/// across commits (design/DESIGN-MPEE-SOLVER.md §6): a table has to double
/// before any comparison can flip.
pub(crate) fn magnitude(n: u64) -> u32 {
    64 - n.leading_zeros()
}

/// The lexicographic cost of one candidate left-deep order, summed over its
/// positions. Field ORDER is the comparison order — `derive(Ord)` on a struct
/// is lexicographic by declaration, which is exactly what is wanted here.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
struct Cost {
    /// log2 of the worst-case product: 0 for a KNOWN step, `bucket(t)`
    /// otherwise. The same quantity `crates/mpedb/src/risk.rs` computes for
    /// the #74 budget — used to CHOOSE a plan here rather than only to warn.
    worst_log: u32,
    /// Steps with no predicate at all linking them to the already-placed set.
    /// A cartesian step multiplies the intermediate by the whole table with
    /// CERTAINTY; a linked step multiplies it by AT MOST the whole table. Same
    /// upper bound, categorically different risk — and purely structural, so
    /// this term reads no statistics whatsoever.
    cartesian: u32,
    /// `(n - i)` for a position `i` whose table has no constraint on it at all.
    /// An unconstrained table inflates its own stage and every stage after it,
    /// so it is charged once per remaining step — this is what pushes
    /// certainly-full scans to the end of the chain.
    late_unconstrained: u32,
}

impl Cost {
    fn add(self, o: Cost) -> Cost {
        Cost {
            worst_log: self.worst_log.saturating_add(o.worst_log),
            cartesian: self.cartesian.saturating_add(o.cartesian),
            late_unconstrained: self.late_unconstrained.saturating_add(o.late_unconstrained),
        }
    }
}

/// Above this many tables the DP stops considering cartesian-first orders and
/// expands only along the join graph's frontier (design/DESIGN-MPEE-SOLVER.md
/// §4). `2^12 * 12 ≈ 49k` transitions is the exhaustive budget.
const DP_FULL_MAX: usize = 12;

/// Hard cap on live DP states. Exceeding it falls back to a greedy pass with
/// the identical scoring function, so compile time stays predictable. Both
/// constants are functions of the STATEMENT, never of the catalog — which is
/// what keeps the algorithm choice deterministic across processes.
const MAX_STATES: usize = 20_000;

/// Widest chain the solver will look at. Bounded by the `u32` state mask (31)
/// and set above the plan format's own `MAX_JOINS = 16` (17 tables), so the
/// solver never becomes the reason a statement is refused. `select5.test`
/// carries comma joins up to 64 tables wide; those are refused by plan
/// validation, and the solver simply declines to look at them.
const MAX_SOLVE: usize = 24;

/// One table position in the scope being solved.
struct Node<'a> {
    def: &'a TableDef,
    /// log2 magnitude of the table's row count.
    bucket: u32,
    /// Equality pins on this table's columns: `(column index, mask of tables
    /// the pinning expression needs placed first)`. A constant pin has mask 0.
    pins: Vec<(usize, u32)>,
    /// For every conjunct that mentions this table AND at least one other:
    /// the mask of the OTHER tables it needs. Empty ⇒ this table is linked to
    /// nothing and every step that introduces it is a cartesian product.
    links: Vec<u32>,
    /// A conjunct mentioning ONLY this table (a constant anchor, a LIKE, …).
    /// It constrains the table wherever it is placed, so it never makes a step
    /// cartesian, but — unless it produces a KNOWN access path — it bounds
    /// nothing.
    self_filter: bool,
    /// Tables this one shares a conjunct with.
    adj: u32,
}

struct Problem<'a> {
    n: usize,
    nodes: Vec<Node<'a>>,
}

impl<'a> Problem<'a> {
    /// Is entering `t` with `placed` already done a KNOWN (single-row) step?
    /// Mirrors `extract_join_access`'s preference: full PK first, then a
    /// full-width UNIQUE index.
    fn known(&self, placed: u32, t: usize) -> bool {
        let node = &self.nodes[t];
        let pinned = |col: u16| {
            node.pins
                .iter()
                .any(|&(c, need)| c == col as usize && need & !placed == 0)
        };
        let pk = &node.def.primary_key;
        if !pk.is_empty() && pk.iter().all(|&c| pinned(c)) {
            return true;
        }
        node.def
            .indexes
            .iter()
            .take(63)
            .any(|ix| ix.unique && !ix.columns.is_empty() && ix.columns.iter().all(|&c| pinned(c)))
    }

    /// The cost of putting `t` at position `pos` given the `placed` set.
    fn step(&self, placed: u32, t: usize, pos: usize) -> Cost {
        let node = &self.nodes[t];
        let known = self.known(placed, t);
        let linked = node.links.iter().any(|&m| m & !placed == 0);
        let constrained = linked || node.self_filter || known;
        Cost {
            worst_log: if known { 0 } else { node.bucket },
            cartesian: u32::from(pos > 0 && !linked),
            late_unconstrained: if constrained { 0 } else { (self.n - pos) as u32 },
        }
    }

    fn order_cost(&self, order: &[usize]) -> Cost {
        let mut placed = 0u32;
        let mut cost = Cost::default();
        for (pos, &t) in order.iter().enumerate() {
            cost = cost.add(self.step(placed, t, pos));
            placed |= 1 << t;
        }
        cost
    }

    /// Tables adjacent to the placed set and not yet placed.
    fn frontier(&self, placed: u32) -> u32 {
        let mut f = 0u32;
        for t in 0..self.n {
            if placed & (1 << t) != 0 {
                f |= self.nodes[t].adj;
            }
        }
        f & !placed & ((1u32 << self.n) - 1)
    }

    /// Dynamic program over subsets, level by level in increasing population
    /// count. Legal because a step's cost depends only on `(placed, t, pos)`
    /// and `pos = popcount(placed)` — so the cost of a SET is independent of
    /// the order the set was built in, and the state needs no "last table".
    ///
    /// `BTreeMap` (not `HashMap`) because iteration order is part of the
    /// tie-breaking and must be byte-identical in every process.
    ///
    /// `seeds` restricts which tables may occupy position 0 — the extremal
    /// sampling of `extremes()`. `full & seeds` = every table = the exhaustive
    /// search.
    fn dp(&self, seeds: u32) -> Option<Vec<usize>> {
        let full = (1u32 << self.n) - 1;
        let mut levels: Vec<BTreeMap<u32, (Cost, u8)>> = vec![BTreeMap::new(); self.n];
        for t in 0..self.n {
            if seeds & (1 << t) == 0 {
                continue;
            }
            levels[0].insert(1u32 << t, (self.step(0, t, 0), t as u8));
        }
        for k in 0..self.n - 1 {
            if levels[k].len() > MAX_STATES {
                return None;
            }
            let cur = std::mem::take(&mut levels[k]);
            for (&mask, &(cost, _)) in &cur {
                // Collapse + stream (design/DESIGN-MPEE-SOLVER.md §4): expand
                // only along the join graph's frontier once the scope is too
                // wide for exhaustive search. A subgraph attached through few
                // edges can then only appear as a connected prefix, so the
                // state count follows the graph's decomposition instead of 2^n.
                let mut cand = if self.n <= DP_FULL_MAX { full & !mask } else { self.frontier(mask) };
                if cand == 0 {
                    cand = full & !mask;
                }
                for t in 0..self.n {
                    if cand & (1 << t) == 0 {
                        continue;
                    }
                    let nc = cost.add(self.step(mask, t, k + 1));
                    let nm = mask | (1 << t);
                    match levels[k + 1].get(&nm) {
                        // Strictly-better only: on a tie the first insertion
                        // wins, and insertions run in ascending (mask, t)
                        // order, so the result is deterministic.
                        Some(&(old, _)) if old <= nc => {}
                        _ => {
                            levels[k + 1].insert(nm, (nc, t as u8));
                        }
                    }
                }
            }
            levels[k] = cur;
        }
        // Reconstruct backwards from the full set.
        let mut order = vec![0usize; self.n];
        let mut mask = full;
        for k in (0..self.n).rev() {
            let &(_, last) = levels[k].get(&mask)?;
            order[k] = last as usize;
            mask &= !(1u32 << last);
        }
        Some(order)
    }

    /// **Extremal analysis** (Morten's city analogy: sample the compass
    /// extremes first and the 4×4 among them already exposes the main roads
    /// and junctions). The query-graph analogues, all read straight off the
    /// problem and all deterministic:
    ///
    /// - every table that is already KNOWN with nothing placed — i.e. carries a
    ///   CONSTANT anchor pinning its whole PK or a whole UNIQUE index. This is
    ///   the strongest restriction a table can have, and it is exactly what
    ///   `select5.test`'s `join-17-4` buries in its 16th conjunct;
    /// - the smallest table (you want it early) and the largest (you want it
    ///   late, or probed rather than scanned);
    /// - the hub: the highest-degree node in the join graph.
    ///
    /// See design/DESIGN-MPEE-SOLVER.md §4.1 for where the road analogy stops
    /// transferring — a left-deep order has a START but no END, so extremal
    /// sampling here degenerates to *seed selection plus hub identification*
    /// rather than bracketing a route.
    fn extremes(&self) -> u32 {
        let mut m = 0u32;
        for t in 0..self.n {
            if self.known(0, t) {
                m |= 1 << t;
            }
        }
        let by = |f: &dyn Fn(&Node) -> u32, max: bool| -> usize {
            let mut best = 0usize;
            for t in 1..self.n {
                let (a, b) = (f(&self.nodes[t]), f(&self.nodes[best]));
                if if max { a > b } else { a < b } {
                    best = t;
                }
            }
            best
        };
        m |= 1 << by(&|nd| nd.bucket, false);
        m |= 1 << by(&|nd| nd.bucket, true);
        m |= 1 << by(&|nd| nd.adj.count_ones(), true);
        m
    }

    /// Progressive refinement: solve from the extremal seeds, then widen the
    /// seed set by the nodes "between" them (their graph frontier), then to
    /// everything — stopping the moment a round does not improve the decision.
    ///
    /// Stopping rule and worst-case bound: at most three rounds, each bounded
    /// by `MAX_STATES`; a round that blows the cap contributes nothing and the
    /// best order found so far stands. On a dense graph where refinement never
    /// converges this degenerates to the extremal-seeded greedy — still a
    /// *valid* plan, since ordering an INNER chain never changes the answer,
    /// only possibly a non-optimal one.
    fn search(&self) -> Vec<usize> {
        if self.n <= DP_FULL_MAX {
            // Small enough to be exhaustive: extremal sampling would only be a
            // way of not looking at everything, and here we can afford to.
            if let Some(o) = self.dp((1u32 << self.n) - 1) {
                return o;
            }
        }
        let full = (1u32 << self.n) - 1;
        let ex = self.extremes();
        let mut best: Option<(Cost, Vec<usize>)> = None;
        let consider = |o: Vec<usize>, best: &mut Option<(Cost, Vec<usize>)>| {
            let c = self.order_cost(&o);
            if best.as_ref().is_none_or(|(bc, _)| c < *bc) {
                *best = Some((c, o));
            }
        };
        for seeds in [ex, ex | self.frontier(ex), full] {
            let before = best.as_ref().map(|(c, _)| *c);
            if let Some(o) = self.dp(seeds) {
                consider(o, &mut best);
            }
            // A round that did not move the decision ends the refinement —
            // widening further can only cost more search for the same answer.
            if before.is_some() && before == best.as_ref().map(|(c, _)| *c) {
                break;
            }
        }
        for t in 0..self.n {
            if ex & (1 << t) != 0 {
                consider(self.greedy_from(t), &mut best);
            }
        }
        best.map(|(_, o)| o).unwrap_or_else(|| self.greedy_from(0))
    }

    /// Greedy completion from a fixed seed: place tables one at a time, always
    /// taking the cheapest frontier candidate under the SAME scoring function.
    /// O(n^2), fully deterministic, and the guaranteed floor when every DP
    /// round blew the state cap.
    fn greedy_from(&self, seed: usize) -> Vec<usize> {
        let full = (1u32 << self.n) - 1;
        let mut placed = 1u32 << seed;
        let mut order = vec![seed];
        for pos in 1..self.n {
            let mut cand = self.frontier(placed);
            if cand == 0 {
                cand = full & !placed;
            }
            let mut best: Option<(Cost, usize)> = None;
            for t in 0..self.n {
                if cand & (1 << t) == 0 {
                    continue;
                }
                let c = self.step(placed, t, pos);
                if best.is_none_or(|(bc, _)| c < bc) {
                    best = Some((c, t));
                }
            }
            // `cand` is non-empty by construction (it falls back to every
            // unplaced table), but a solver must never be the thing that
            // panics a query: bail to whatever order has been built plus the
            // rest in textual order.
            let Some((_, t)) = best else {
                order.extend((0..self.n).filter(|t| placed & (1 << t) == 0));
                return order;
            };
            order.push(t);
            placed |= 1 << t;
        }
        order
    }
}

/// Why the chain was left alone, for the `MPEDB_EXPLAIN_MPEE` trace and for
/// the unit tests. Never surfaced to users.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Skip {
    /// Fewer than two tables, or more than the DP can address.
    Size,
    /// A LEFT/FULL/RIGHT join, USING/NATURAL, a CTE working table, or an RLS
    /// policy on some table in the chain — see DESIGN-MPEE-SOLVER.md §7.
    Ineligible,
    /// The probe bind failed; the normal path will report the real error.
    Unbindable,
    /// The solver found nothing strictly better than what the user wrote.
    NoGain,
}

/// Solve this scope's join order and, if a strictly better one exists, return
/// the rewritten statement. `None` = keep the textual order verbatim.
#[allow(clippy::too_many_arguments)]
pub(super) fn reorder<'s>(
    s: &ast::SelectStmt,
    schema: &'s Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    slot_types: &[Ty],
    cte: Option<CteRef<'s>>,
    row_count: RowCountFn<'_>,
) -> std::result::Result<ast::SelectStmt, Skip> {
    let n = s.joins.len() + 1;
    // Masks are `u32`, so 31 is the hard ceiling; the plan format caps a chain
    // at `MAX_JOINS = 16` joins (17 tables) anyway, and a wider one is refused
    // downstream — solving it would be wasted work. `MAX_SOLVE` sits above the
    // format cap so the solver is never the thing that refuses.
    if !(2..=MAX_SOLVE).contains(&n) {
        return Err(Skip::Size);
    }
    // ---- eligibility (design/DESIGN-MPEE-SOLVER.md §7) ----
    if s.from_derived.is_some() {
        return Err(Skip::Ineligible);
    }
    for j in &s.joins {
        if j.kind != ast::JoinKind::Inner || j.natural || !j.using.is_empty() {
            return Err(Skip::Ineligible);
        }
    }
    let outer_name = s.table.as_deref().ok_or(Skip::Ineligible)?;
    let mut ids = Vec::with_capacity(n);
    let mut defs: Vec<&TableDef> = Vec::with_capacity(n);
    let mut named: Vec<(String, &TableDef)> = Vec::with_capacity(n);
    {
        let (id, def) = resolve_table_cte(schema, cte, outer_name).map_err(|_| Skip::Unbindable)?;
        ids.push(id);
        defs.push(def);
        named.push((s.alias.clone().unwrap_or_else(|| def.name.clone()), def));
    }
    for j in &s.joins {
        let (id, def) = resolve_table_cte(schema, cte, &j.table).map_err(|_| Skip::Unbindable)?;
        ids.push(id);
        defs.push(def);
        named.push((j.alias.clone().unwrap_or_else(|| def.name.clone()), def));
    }
    // The recursive-CTE working table has no key trees at all, and a table
    // under an RLS policy carries an evaluation-order contract this v1
    // declines to reason about under reordering.
    for &id in &ids {
        if id == CTE_TABLE
            || id == crate::plan::DUAL_TABLE
            || catalog.get(id).is_some()
            || catalog.requires_policy(id)
        {
            return Err(Skip::Ineligible);
        }
    }

    // ---- probe bind ----
    // Each ON binds over its OWN left-deep prefix scope, so a forward
    // reference stays refused exactly as it is today; left-deep prefixes share
    // slot numbering, so the resulting slots are directly comparable with the
    // WHERE's, which binds over the full joined scope.
    let eff_params = n_params + slot_types.len() as u16;
    let mut binder = Binder::with_scope(
        Scope::single_named(named[0].0.clone(), defs[0]),
        eff_params,
        true,
    );
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }
    let mut conjuncts: Vec<BExpr> = Vec::new();
    for (k, j) in s.joins.iter().enumerate() {
        let scope = Scope::joined_named(named[..=k + 1].to_vec()).map_err(|_| Skip::Unbindable)?;
        binder = binder.rescope(scope);
        let on = binder.bind_predicate(&j.on).map_err(|_| Skip::Unbindable)?;
        split_and(on, &mut conjuncts);
    }
    if let Some(w) = &s.where_clause {
        let w = binder.bind_predicate(w).map_err(|_| Skip::Unbindable)?;
        split_and(w, &mut conjuncts);
    }

    // ---- build the problem ----
    // Slot -> table index, from the cumulative widths of the textual order.
    let mut base = Vec::with_capacity(n + 1);
    let mut acc = 0usize;
    for d in &defs {
        base.push(acc);
        acc += d.columns.len();
    }
    base.push(acc);
    let table_of = |slot: u16| -> Option<usize> {
        let s = slot as usize;
        (0..n).find(|&i| s >= base[i] && s < base[i + 1])
    };
    let mask_of = |e: &BExpr| -> Option<u32> {
        let mut m = 0u32;
        for c in cols_of(e) {
            m |= 1 << table_of(c)?;
        }
        Some(m)
    };

    let mut nodes: Vec<Node> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| Node {
            def: d,
            bucket: magnitude(row_count(ids[i])),
            pins: Vec::new(),
            links: Vec::new(),
            self_filter: false,
            adj: 0,
        })
        .collect();
    for c in &conjuncts {
        let Some(m) = mask_of(c) else { return Err(Skip::Unbindable) };
        if m == 0 {
            continue; // column-free: placed at the outer, orders nothing
        }
        if m.count_ones() == 1 {
            nodes[m.trailing_zeros() as usize].self_filter = true;
        } else {
            for (t, node) in nodes.iter_mut().enumerate() {
                if m & (1 << t) != 0 {
                    node.links.push(m & !(1 << t));
                    node.adj |= m & !(1 << t);
                }
            }
        }
        // Equality pins: `<col of t> = <expr not mentioning t>`. Mirrors
        // `extract_join_access`'s guards — an `any` column can never be
        // pinned, a cross-type equality encodes differently, and a NULL or
        // out-of-type constant would never make a key.
        let BExpr::Binary(BinOp::Eq, l, r) = c else { continue };
        for (a, b) in [(&**l, &**r), (&**r, &**l)] {
            let BExpr::Col(slot) = a else { continue };
            let Some(t) = table_of(*slot) else { continue };
            let col = *slot as usize - base[t];
            let ty = nodes[t].def.columns[col].ty;
            if ty == ColumnType::Any {
                continue;
            }
            let Some(need) = mask_of(b) else { continue };
            if need & (1 << t) != 0 {
                continue; // self-referential: not a probe key
            }
            let ok = match b {
                BExpr::Col(o) => {
                    let ot = table_of(*o).map(|ot| (ot, *o as usize - base[ot]));
                    ot.is_some_and(|(ot, oc)| nodes[ot].def.columns[oc].ty == ty)
                }
                BExpr::Const(v) => !v.is_null() && v.fits(ty),
                BExpr::Param(_) => true,
                _ => false,
            };
            if ok {
                nodes[t].pins.push((col, need));
            }
        }
    }

    let p = Problem { n, nodes };
    let chosen = p.search();
    let textual: Vec<usize> = (0..n).collect();
    if p.order_cost(&chosen) >= p.order_cost(&textual) {
        return Err(Skip::NoGain);
    }
    Ok(rewrite(s, &chosen, &named))
}

/// Every column slot an expression reads (the set form of `max_col`).
fn cols_of(e: &BExpr) -> Vec<u16> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            BExpr::Col(c) => out.push(*c),
            BExpr::Unary(_, a)
            | BExpr::Like(a, _, _, _)
            | BExpr::Glob(a, _)
            | BExpr::Regexp(a, _)
            | BExpr::Cast(a, _)
            | BExpr::InParam(a, _) => stack.push(a),
            BExpr::Binary(_, a, b)
            | BExpr::IsDistinct(a, b, _)
            | BExpr::CollateCmp(_, a, b, _)
            | BExpr::RegexpDyn(a, b)
            | BExpr::LikeDyn(a, b, _, _)
            | BExpr::GlobDyn(a, b)
            | BExpr::ClassCmp(_, a, b, _, _) => {
                stack.push(a);
                stack.push(b);
            }
            BExpr::InList(a, list) | BExpr::InListColl(a, list, _) => {
                stack.push(a);
                stack.extend(list.iter());
            }
            BExpr::Case(arms, else_) => {
                for (c, r) in arms {
                    stack.push(c);
                    stack.push(r);
                }
                if let Some(e) = else_ {
                    stack.push(e);
                }
            }
            BExpr::Coalesce(args) | BExpr::Call(_, args) | BExpr::HostCall { args, .. } => {
                stack.extend(args.iter())
            }
            BExpr::Const(_) | BExpr::Param(_) => {}
        }
    }
    out
}

/// Emit the reordered statement. `INNER JOIN … ON p` ≡ `CROSS JOIN … WHERE p`,
/// so every original ON conjunct moves into the WHERE and mpedb's #65 pushdown
/// puts it back at the earliest step where all its slots are bound — which is
/// exactly the index nested-loop candidate the ON used to be. `SELECT *` is
/// pinned to the ORIGINAL table order first, so output column order never
/// moves (the same trick `rewrite_right_join` uses).
fn rewrite(
    s: &ast::SelectStmt,
    order: &[usize],
    named: &[(String, &TableDef)],
) -> ast::SelectStmt {
    let entry = |i: usize| -> (String, Option<String>) {
        if i == 0 {
            (s.table.clone().expect("join on a FROM-less SELECT"), s.alias.clone())
        } else {
            (s.joins[i - 1].table.clone(), s.joins[i - 1].alias.clone())
        }
    };
    let items = match &s.items {
        Some(items) => Some(items.clone()),
        None => {
            // VISIBLE columns only — a hidden implicit rowid (#94) is never in
            // a `SELECT *`.
            let mut out: Vec<(ast::Expr, Option<String>)> = Vec::new();
            for (name, def) in named {
                for c in def.visible_columns() {
                    out.push((ast::Expr::Qualified(name.clone(), c.name.clone()), None));
                }
            }
            Some(out)
        }
    };
    // ON conjuncts in textual order, then the original WHERE — the relative
    // order every ON had against the WHERE is preserved.
    let mut pred: Option<ast::Expr> = None;
    let and = |a: Option<ast::Expr>, b: ast::Expr| -> Option<ast::Expr> {
        Some(match a {
            None => b,
            Some(a) => ast::Expr::Binary(ast::BinOp::And, Box::new(a), Box::new(b)),
        })
    };
    for j in &s.joins {
        if !matches!(&j.on, ast::Expr::Lit(Value::Bool(true))) {
            pred = and(pred, j.on.clone());
        }
    }
    if let Some(w) = &s.where_clause {
        pred = and(pred, w.clone());
    }

    let (table, alias) = entry(order[0]);
    let joins = order[1..]
        .iter()
        .map(|&i| {
            let (t, a) = entry(i);
            ast::JoinClause {
                table: t,
                alias: a,
                kind: ast::JoinKind::Inner,
                on: ast::Expr::Lit(Value::Bool(true)),
                using: Vec::new(),
                natural: false,
            }
        })
        .collect();
    ast::SelectStmt {
        table: Some(table),
        alias,
        joins,
        items,
        where_clause: pred,
        ..s.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnitude_buckets_are_log2ish() {
        assert_eq!(magnitude(0), 0);
        assert_eq!(magnitude(1), 1);
        assert_eq!(magnitude(2), 2);
        assert_eq!(magnitude(3), 2);
        assert_eq!(magnitude(4), 3);
        assert_eq!(magnitude(10), 4);
        assert_eq!(magnitude(1_000_000), 20);
        // A table has to DOUBLE before the bucket moves — the plan-stability
        // property design/DESIGN-MPEE-SOLVER.md §6 depends on.
        assert_eq!(magnitude(1000), magnitude(1023));
        assert_ne!(magnitude(1023), magnitude(1024));
    }

    #[test]
    fn cost_orders_lexicographically() {
        let a = Cost { worst_log: 10, cartesian: 0, late_unconstrained: 99 };
        let b = Cost { worst_log: 11, cartesian: 0, late_unconstrained: 0 };
        assert!(a < b, "the worst-case product dominates");
        let c = Cost { worst_log: 10, cartesian: 5, late_unconstrained: 0 };
        assert!(a < c, "with equal worst case, fewer cartesian steps win");
    }
}
