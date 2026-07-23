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
//!
//! ## v2 (#116): constraints, not refusals
//!
//! v1 REFUSED to reorder wherever it could not prove commutativity outright.
//! In vehicle routing the same situations are *constraints* — a time window, a
//! forbidden turn — and a solver prices them and searches the feasible region
//! rather than giving up. v2 converts three of v1's refusals
//! (design/DESIGN-MPEE-SOLVER.md §7):
//!
//! - a **LEFT JOIN is a BARRIER**, not a veto. Its inner side keeps its exact
//!   position, so the set of tables preceding it — and therefore what it
//!   NULL-extends — is untouched; each maximal INNER run between barriers is
//!   an independent sub-problem the same solver orders, because
//!   `(A ⋈ B) ⟕ C ≡ (B ⋈ A) ⟕ C`. FULL still refuses: #65 disables predicate
//!   pushdown entirely under FULL, so the ON→WHERE move the rewrite relies on
//!   is not available there.
//! - a **correlated lifted subplan** is REMAPPED, not refused. Its `outer_args`
//!   are base-row slots of the joined tuple in the TEXTUAL order, so the
//!   permutation's slot map is applied to them ([`Solved::slot_map`]).
//! - a **residual conjunct's placement** is PRICED. #65 evaluates a conjunct at
//!   the step that places its LAST table, so *when* a filter runs is a
//!   consequence of the order — a choice, and therefore a cost term
//!   (`Cost::residual_late`).
//!
//! RLS stays refused, deliberately: a reorder changes which pairs a predicate
//! is evaluated over and mpedb RAISES on arithmetic overflow, so under a policy
//! scope a raise is an information channel (§7).
//!
//! ## Ping-pong: the solver steers which cost input is bought (§9.5)
//!
//! Cost inputs are bought on DEMAND, never up front. `row_count` is read
//! through a memoizing [`Cell`], and an unbought table prices at [`UNBOUGHT`]
//! — a valid LOWER bound, because every cost term is monotone non-decreasing in
//! a table's bucket. The solver therefore runs as branch-and-bound: propose an order
//! under the optimistic bound, buy only the counts that proposal's cost
//! actually depends on, re-solve. When a proposal's own cost is fully bought,
//! its estimate is exact while every rejected candidate's estimate was a lower
//! bound — so it is optimal over everything the search explored, and the rounds
//! stop. That is Morten's *"valg av N kan gjøres med MPEE-styring"*: the solver
//! picks which N to examine next instead of enumerating them all.

use super::*;
use std::cell::Cell;

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
    /// **v2 (#116) — residual PLACEMENT priced.** #65 evaluates a conjunct at
    /// the step that places its LAST table, so a conjunct's position is a
    /// consequence of the order, not a fixed property of the text. Every
    /// conjunct is charged the position at which it becomes evaluable: a filter
    /// that runs at step 1 shrinks every stage after it, one that runs at step
    /// 7 shrinks nothing. Purely structural — it reads no statistics — and it
    /// sits LAST in the lexicographic order, so it can only break ties among
    /// candidates the first three terms rate identically. That is exactly the
    /// v1 population that fell back to "whatever the textual order said".
    residual_late: u32,
}

impl Cost {
    fn add(self, o: Cost) -> Cost {
        Cost {
            worst_log: self.worst_log.saturating_add(o.worst_log),
            cartesian: self.cartesian.saturating_add(o.cartesian),
            late_unconstrained: self.late_unconstrained.saturating_add(o.late_unconstrained),
            residual_late: self.residual_late.saturating_add(o.residual_late),
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

/// How many propose → probe → re-solve rounds the compile-time ping-pong runs
/// before it gives up on being frugal, buys every cost input and solves once
/// eagerly (design/DESIGN-MPEE-SOLVER.md §9.5). A function of the STATEMENT,
/// never of the catalog, so the algorithm stays the same in every process.
const PING_PONG_ROUNDS: usize = 3;

/// What a magnitude that has not been bought yet is priced at (§9.5).
///
/// `magnitude(n) = 1` for a one-row table, so this is a genuine LOWER bound for
/// every table that holds at least one row, and the branch-and-bound in
/// [`Problem::solve`] is exact. The one exception is an EMPTY table
/// (`magnitude(0) = 0`), where the bound is one too high and the search could
/// in principle prune the true optimum — a query joining an empty table returns
/// no rows and terminates immediately whatever the order, so what is at stake
/// there is nothing. The row SET is never affected: reordering an INNER chain
/// preserves it exactly.
///
/// `0` would be the unconditionally safe value, and it is also useless: with
/// every unbought table at `0` the leading cost term is `0` for every candidate
/// and the first round decides on tie-breakers alone — which then sends the
/// solver off buying counts for an order it is about to discard. At `1` the
/// leading term of an all-unbought round IS the count of un-probed steps, so
/// the first proposal is the one whose cost the solver can most cheaply
/// certify. That is what makes the ping-pong converge in two rounds instead of
/// walking the chain.
const UNBOUGHT: u32 = 1;

/// One table position in the scope being solved.
struct Node<'a> {
    def: &'a TableDef,
    /// log2 magnitude of the table's row count — **bought lazily** (§9.5). The
    /// solver asks; only then does the cost side pay for the catalog read.
    /// `None` prices as [`UNBOUGHT`], a genuine lower bound, which is what
    /// makes the branch-and-bound rounds in [`Problem::solve`] sound.
    bucket: Cell<Option<u32>>,
    /// Per secondary index (position i = index_no i+1): the NDV bucket from
    /// the cost source, filled by the SAME purchase as `bucket` so the two are
    /// never inconsistent. Unbought ⇒ no discount, which keeps the unbought
    /// price of [`UNBOUGHT`] a lower bound exactly as before (an NDV discount
    /// only ever LOWERS a bought cost, and the unbought price is already the
    /// floor).
    ndv: std::cell::OnceCell<Vec<Option<u32>>>,
    /// Equality pins on this table's columns: `(column index, mask of tables
    /// the pinning expression needs placed first)`. A constant pin has mask 0.
    pins: Vec<(usize, u32)>,
    /// The FULL table mask of every conjunct that mentions this table. A
    /// conjunct is resolved — evaluated, per #65 — at the step that places its
    /// LAST table, i.e. when `mask & !(placed | 1<<t) == 0`. Multi-table
    /// entries are what make a step non-cartesian; a `1<<t` entry is a
    /// single-table filter, which constrains the table wherever it is placed
    /// but (unless it yields a KNOWN access path) bounds nothing.
    conj: Vec<u32>,
    /// `conj` contains this table alone: a constant anchor, a LIKE, …
    self_filter: bool,
    /// Tables this one shares a conjunct with.
    adj: u32,
    /// **v2 barrier (#116).** This table is the NULL-extended inner side of a
    /// LEFT join: its position is FIXED, so the set of tables that precede it
    /// — and therefore exactly what it preserves and what it NULL-extends — is
    /// the one the user wrote. Everything between two barriers is a free INNER
    /// run the solver orders as an independent sub-problem.
    barrier: bool,
}

struct Problem<'a> {
    n: usize,
    nodes: Vec<Node<'a>>,
    /// Table ids, parallel to `nodes` — the key the cost side is bought with.
    ids: Vec<u32>,
    row_count: RowCountFn<'a>,
    /// How many `row_count` reads the solver actually paid for. The ping-pong
    /// metric: `probes < n` is the search steering which N to examine.
    probes: Cell<usize>,
}

impl<'a> Problem<'a> {
    /// Buy one table's magnitude bucket, once (§9.5). Nothing else in the
    /// solver ever calls `row_count`.
    fn buy(&self, t: usize) {
        if self.nodes[t].bucket.get().is_none() {
            let node = &self.nodes[t];
            node.bucket.set(Some(magnitude((self.row_count.row_count)(self.ids[t]))));
            // One purchase buys the table's whole statistics row: the count
            // and every index's NDV bucket. Splitting them would let the two
            // disagree mid-solve, and an NDV read is the same cheap catalog
            // get the count is.
            let _ = node.ndv.set(
                (0..node.def.indexes.len().min(63))
                    .map(|i| (self.row_count.index_ndv_bucket)(self.ids[t], i as u32 + 1))
                    .collect(),
            );
            self.probes.set(self.probes.get() + 1);
        }
    }

    /// The magnitude the cost model currently believes. An unbought table
    /// prices at [`UNBOUGHT`] — a LOWER bound on any non-empty table's
    /// magnitude, never an invented estimate — which is what keeps every
    /// intermediate round's cost admissible and the branch-and-bound sound.
    fn bucket(&self, t: usize) -> u32 {
        self.nodes[t].bucket.get().unwrap_or(UNBOUGHT)
    }

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
        node.def.indexes.iter().take(63).any(|ix| {
            // A PARTIAL unique index does not determine at most one row of the
            // TABLE — only of its members — and `extract_join_access` will not
            // pick one anyway, so it must not make a table look "known" here
            // either (the cost model must describe the access the planner
            // actually emits). CREATE INDEX refuses UNIQUE … WHERE today, so
            // this changes no plan; it keeps the two in step when it stops.
            ix.unique
                && ix.predicate.is_none()
                && !ix.columns.is_empty()
                && ix.columns.iter().all(|&c| pinned(c))
        })
    }

    /// The NDV discount for entering `t` with `placed` done (stage A of
    /// design/DESIGN-MPEE-GENERAL.md §4): the largest NDV bucket over `t`'s
    /// non-partial indexes whose EVERY column is equality-pinned by constants,
    /// params, or placed tables. An equality on such an index cannot match
    /// more than its largest key group, so `bucket(rows) − bucket(ndv)` is
    /// still a worst-case-shaped bound — exact for uniform keys, optimistic
    /// only under skew, and log2 granularity absorbs skew up to 2× per bucket
    /// (the caveat is DESIGN-MPEE-GENERAL §8). Zero when the table's stats
    /// are unbought (the UNBOUGHT floor is already the lower bound) or the
    /// index was never analyzed.
    fn ndv_discount(&self, placed: u32, t: usize) -> u32 {
        let node = &self.nodes[t];
        let Some(ndv) = node.ndv.get() else { return 0 };
        let pinned = |col: u16| {
            node.pins
                .iter()
                .any(|&(c, need)| c == col as usize && need & !placed == 0)
        };
        node.def
            .indexes
            .iter()
            .take(63)
            .zip(ndv)
            .filter(|(ix, _)| {
                // Partial: entry count covers members only, and membership is
                // not establishable here — the same refusal known() makes.
                ix.predicate.is_none()
                    && !ix.columns.is_empty()
                    && ix.columns.iter().all(|&c| pinned(c))
            })
            .filter_map(|(_, b)| *b)
            .max()
            .unwrap_or(0)
    }

    /// The cost of putting `t` at position `pos` given the `placed` set.
    ///
    /// Depends only on `(placed, t, pos)` — never on the ORDER `placed` was
    /// built in — which is what makes the subset DP in [`Self::dp`] correct.
    fn step(&self, placed: u32, t: usize, pos: usize) -> Cost {
        let node = &self.nodes[t];
        let known = self.known(placed, t);
        // Everything this table's conjuncts still need, once it is in.
        let after = !(placed | (1 << t));
        let linked = node.conj.iter().any(|&m| m.count_ones() > 1 && m & after == 0);
        let constrained = linked || node.self_filter || known;
        // v2: every conjunct RESOLVED at this step is charged its position.
        // Each conjunct is charged exactly once over a full order — at the step
        // that places its last table — so this is a genuine sum over conjuncts.
        let resolved = node.conj.iter().filter(|&&m| m & after == 0).count() as u32;
        Cost {
            // A fully-pinned analyzed index tightens the worst case without
            // changing its nature: floored at 1 so a discount can never claim
            // the KNOWN (single-row) certainty only known() may grant.
            worst_log: if known {
                0
            } else {
                let b = self.bucket(t);
                let d = self.ndv_discount(placed, t);
                if d == 0 || b == 0 { b } else { b.saturating_sub(d).max(1) }
            },
            cartesian: u32::from(pos > 0 && !linked),
            late_unconstrained: if constrained { 0 } else { (self.n - pos) as u32 },
            residual_late: resolved.saturating_mul(pos as u32),
        }
    }

    /// Cost of ordering the tables of one segment, given the fixed `prefix`
    /// already placed before it and the position `base` the segment starts at.
    fn order_cost(&self, prefix: u32, base: usize, order: &[usize]) -> Cost {
        let mut placed = prefix;
        let mut cost = Cost::default();
        for (i, &t) in order.iter().enumerate() {
            cost = cost.add(self.step(placed, t, base + i));
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
    /// `seeds` restricts which tables may occupy the segment's first position —
    /// the extremal sampling of `extremes()`. `seeds = univ` = every table =
    /// the exhaustive search.
    ///
    /// `prefix` is the (fixed) set already placed before this segment, `univ`
    /// the segment's own tables and `base` the position its first table takes.
    /// With no barrier in the chain this is `(0, full, 0)` and the whole thing
    /// is v1's DP verbatim.
    fn dp(&self, prefix: u32, univ: u32, base: usize, seeds: u32) -> Option<Vec<usize>> {
        let m = univ.count_ones() as usize;
        let mut levels: Vec<BTreeMap<u32, (Cost, u8)>> = vec![BTreeMap::new(); m];
        for t in 0..self.n {
            if seeds & univ & (1 << t) == 0 {
                continue;
            }
            levels[0].insert(1u32 << t, (self.step(prefix, t, base), t as u8));
        }
        for k in 0..m - 1 {
            if levels[k].len() > MAX_STATES {
                return None;
            }
            let cur = std::mem::take(&mut levels[k]);
            for (&mask, &(cost, _)) in &cur {
                let placed = prefix | mask;
                // Collapse + stream (design/DESIGN-MPEE-SOLVER.md §4): expand
                // only along the join graph's frontier once the scope is too
                // wide for exhaustive search. A subgraph attached through few
                // edges can then only appear as a connected prefix, so the
                // state count follows the graph's decomposition instead of 2^n.
                let mut cand =
                    if m <= DP_FULL_MAX { univ & !mask } else { self.frontier(placed) & univ };
                if cand == 0 {
                    cand = univ & !mask;
                }
                for t in 0..self.n {
                    if cand & (1 << t) == 0 {
                        continue;
                    }
                    let nc = cost.add(self.step(placed, t, base + k + 1));
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
        // Reconstruct backwards from the full segment.
        let mut order = vec![0usize; m];
        let mut mask = univ;
        for k in (0..m).rev() {
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
    fn extremes(&self, prefix: u32, univ: u32) -> u32 {
        let mut m = 0u32;
        for t in 0..self.n {
            if univ & (1 << t) != 0 && self.known(prefix, t) {
                m |= 1 << t;
            }
        }
        // The size extremes read whatever magnitudes have been BOUGHT so far
        // and nothing more (§9.5). In the first ping-pong round every bucket is
        // still at the flat [`UNBOUGHT`] floor, so the size extremes cannot
        // separate anything and the seeds are purely structural — the anchors and the
        // hub; each later round sharpens them with the counts the previous
        // round's decision turned out to depend on. Seed selection never buys.
        let by = |f: &dyn Fn(usize) -> u32, max: bool| -> Option<usize> {
            let mut best: Option<usize> = None;
            for t in 0..self.n {
                if univ & (1 << t) == 0 {
                    continue;
                }
                match best {
                    Some(b) if !(if max { f(t) > f(b) } else { f(t) < f(b) }) => {}
                    _ => best = Some(t),
                }
            }
            best
        };
        if let Some(t) = by(&|t| self.bucket(t), false) {
            m |= 1 << t;
        }
        if let Some(t) = by(&|t| self.bucket(t), true) {
            m |= 1 << t;
        }
        if let Some(t) = by(&|t| self.nodes[t].adj.count_ones(), true) {
            m |= 1 << t;
        }
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
    fn search(&self, prefix: u32, univ: u32, base: usize) -> Vec<usize> {
        let m = univ.count_ones() as usize;
        if m <= DP_FULL_MAX {
            // Small enough to be exhaustive: extremal sampling would only be a
            // way of not looking at everything, and here we can afford to.
            if let Some(o) = self.dp(prefix, univ, base, univ) {
                return o;
            }
        }
        let ex = self.extremes(prefix, univ);
        let mut best: Option<(Cost, Vec<usize>)> = None;
        let consider = |o: Vec<usize>, best: &mut Option<(Cost, Vec<usize>)>| {
            let c = self.order_cost(prefix, base, &o);
            if best.as_ref().is_none_or(|(bc, _)| c < *bc) {
                *best = Some((c, o));
            }
        };
        for seeds in [ex, ex | (self.frontier(prefix | ex) & univ), univ] {
            let before = best.as_ref().map(|(c, _)| *c);
            if let Some(o) = self.dp(prefix, univ, base, seeds) {
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
                consider(self.greedy_from(prefix, univ, base, t), &mut best);
            }
        }
        best.map(|(_, o)| o).unwrap_or_else(|| {
            let seed = (0..self.n).find(|t| univ & (1 << t) != 0).unwrap_or(0);
            self.greedy_from(prefix, univ, base, seed)
        })
    }

    /// Greedy completion from a fixed seed: place tables one at a time, always
    /// taking the cheapest frontier candidate under the SAME scoring function.
    /// O(n^2), fully deterministic, and the guaranteed floor when every DP
    /// round blew the state cap.
    fn greedy_from(&self, prefix: u32, univ: u32, base: usize, seed: usize) -> Vec<usize> {
        let m = univ.count_ones() as usize;
        let mut placed = prefix | (1u32 << seed);
        let mut order = vec![seed];
        for i in 1..m {
            let mut cand = self.frontier(placed) & univ;
            if cand == 0 {
                cand = univ & !placed;
            }
            let mut best: Option<(Cost, usize)> = None;
            for t in 0..self.n {
                if cand & (1 << t) == 0 {
                    continue;
                }
                let c = self.step(placed, t, base + i);
                if best.is_none_or(|(bc, _)| c < bc) {
                    best = Some((c, t));
                }
            }
            // `cand` is non-empty by construction (it falls back to every
            // unplaced table), but a solver must never be the thing that
            // panics a query: bail to whatever order has been built plus the
            // rest in textual order.
            let Some((_, t)) = best else {
                order.extend((0..self.n).filter(|t| univ & (1 << t) != 0 && placed & (1 << t) == 0));
                return order;
            };
            order.push(t);
            placed |= 1 << t;
        }
        order
    }

    /// The chain, segment by segment. A BARRIER table (a LEFT join's inner
    /// side) occupies its own textual position and nothing else moves across
    /// it; each maximal run of free positions between two barriers is solved
    /// as an independent sub-problem.
    ///
    /// Segment-local optimisation is GLOBALLY optimal here: a segment's
    /// internal order cannot change the SET of tables any later segment sees
    /// (only the order within its own position range), and `step`'s cost
    /// depends on the placed SET, not on how it was built (§3). So the
    /// segments do not interact and solving them left to right is exact.
    fn solve_chain(&self) -> Vec<usize> {
        let mut order: Vec<usize> = Vec::with_capacity(self.n);
        let mut placed = 0u32;
        let mut p = 0usize;
        while p < self.n {
            if self.nodes[p].barrier {
                order.push(p);
                placed |= 1 << p;
                p += 1;
                continue;
            }
            let mut univ = 0u32;
            let base = p;
            while p < self.n && !self.nodes[p].barrier {
                univ |= 1 << p;
                p += 1;
            }
            if univ.count_ones() == 1 {
                order.push(base);
                placed |= 1 << base;
                continue;
            }
            for t in self.search(placed, univ, base) {
                order.push(t);
                placed |= 1 << t;
            }
        }
        order
    }

    /// **Ping-pong (§9.5).** Propose under the optimistic [`UNBOUGHT`] bound,
    /// buy only the counts the proposal's own cost depends on, re-solve. Stop
    /// when the proposal is fully bought.
    ///
    /// Soundness: every cost term is monotone non-decreasing in a table's
    /// bucket, so an unbought table priced at [`UNBOUGHT`] makes every
    /// candidate's cost a LOWER bound.
    /// When the winner `O` has all of its own contributors bought, `cost(O)` is
    /// exact while every rejected candidate `P` satisfied
    /// `cost_est(P) ≥ cost_est(O) = cost_true(O)` and `cost_true(P) ≥
    /// cost_est(P)`. So `O` is optimal over everything the search explored —
    /// the same guarantee the eager v1 gave, for fewer cost reads.
    ///
    /// Termination and compile-cost bound: each round that does not stop buys
    /// at least one previously unbought count, and the rounds are capped at
    /// [`PING_PONG_ROUNDS`]. A chain that has not settled by then buys
    /// EVERYTHING and solves once more — which is exactly v1's eager solve, so
    /// the fallback is bit-identical to the pre-#116 behaviour and the whole
    /// mechanism is bounded at `PING_PONG_ROUNDS + 1` searches.
    fn solve(&self) -> Vec<usize> {
        let mut order = self.solve_chain();
        for _ in 0..PING_PONG_ROUNDS {
            // A position's bucket enters the cost only when the step is NOT
            // KNOWN — a PK probe is one row by proof and never reads a count.
            let owed = self.owed(&order);
            if owed.is_empty() {
                return order;
            }
            for t in owed {
                self.buy(t);
            }
            order = self.solve_chain();
        }
        if self.owed(&order).is_empty() {
            return order;
        }
        for t in 0..self.n {
            self.buy(t);
        }
        self.solve_chain()
    }

    /// The tables whose magnitude this order's cost depends on and that have
    /// not been paid for yet.
    fn owed(&self, order: &[usize]) -> Vec<usize> {
        let mut placed = 0u32;
        let mut owed = Vec::new();
        for &t in order {
            if !self.known(placed, t) && self.nodes[t].bucket.get().is_none() {
                owed.push(t);
            }
            placed |= 1 << t;
        }
        owed
    }

    /// Adopt `chosen` only if it is STRICTLY better than what the user wrote.
    ///
    /// The textual order's cost starts as a LOWER bound (its unbought tables
    /// price at [`UNBOUGHT`]) and is refined ONE count at a time, stopping the instant the
    /// comparison is decided. Buying can only raise the textual cost, so the
    /// first `chosen < textual` that appears is final — and in a chain the
    /// solver rescues, that happens after two or three counts rather than after
    /// all of them. This is the branch-and-bound bound doing the steering: the
    /// solver's current best decides which N is worth examining next.
    ///
    /// The comparison is against the OPTIMISTIC textual cost throughout, which
    /// is what makes it sound: `chosen`'s own contributors are all bought by
    /// [`Self::solve`], so `cost_true(chosen) = cost_est(chosen) <
    /// cost_est(textual) ≤ cost_true(textual)`. Declining is always safe — it
    /// keeps the order the user wrote.
    fn beats_textual(&self, chosen: &[usize]) -> bool {
        let textual: Vec<usize> = (0..self.n).collect();
        for t in self.owed(&textual) {
            if self.chain_cost(chosen) < self.chain_cost(&textual) {
                return true;
            }
            self.buy(t);
        }
        self.chain_cost(chosen) < self.chain_cost(&textual)
    }

    /// The cost of the whole chain under a candidate order — the comparison
    /// against the user's textual order.
    fn chain_cost(&self, order: &[usize]) -> Cost {
        self.order_cost(0, 0, order)
    }
}

/// `MPEDB_NO_MPEE=1` leaves every join chain in the user's textual order —
/// the pre-#114 behaviour, in the SAME binary.
///
/// Without this, the only way to A/B the solver was to build the old planner in
/// a second worktree, and two builds have already been the source of one false
/// A/B in this repo (see `engine/commit.rs`). The switch exists so a claim
/// about what the solver buys — wall clock, peak RSS, live join cells — is a
/// paired measurement of one binary rather than a comparison of two.
///
/// `=1` and nothing else, so a script that spells "off" as `=0` cannot silently
/// select the arm it meant to exclude.
pub(super) fn disabled() -> bool {
    match crate::planner::mpee_override() {
        Some(off) => off,
        None => {
            static OFF: std::sync::LazyLock<bool> =
                std::sync::LazyLock::new(|| std::env::var("MPEDB_NO_MPEE").is_ok_and(|v| v == "1"));
            *OFF
        }
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

/// What a successful solve produces.
pub(super) struct Solved {
    /// The rewritten statement, in the chosen order.
    pub stmt: ast::SelectStmt,
    /// **Old base-row slot → new base-row slot.** Subqueries are lifted BEFORE
    /// the join dispatch, so a correlated subplan's `outer_args` name slots of
    /// the joined tuple in the TEXTUAL order. Applying this map to them is what
    /// turns v1's correlated-subplan REFUSAL into a constraint the solver
    /// satisfies (design/DESIGN-MPEE-SOLVER.md §7, and the wrong answer
    /// `crates/mpedb/tests/agg_filter.rs` caught during v1's development).
    pub slot_map: Vec<u16>,
}

impl Solved {
    /// Rewrite every lifted subplan's correlation args through the permutation.
    /// Only the TOP level is ours: a nested lift's `outer_args` name slots of
    /// ITS parent's row, which this reorder does not touch.
    pub fn remap(&self, mut subplans: Vec<SubPlan>) -> Vec<SubPlan> {
        for p in &mut subplans {
            for a in &mut p.outer_args {
                if let Some(&to) = self.slot_map.get(*a as usize) {
                    *a = to;
                }
            }
        }
        subplans
    }
}

/// Solve this scope's join order and, if a strictly better one exists, return
/// the rewritten statement. `Err` = keep the textual order verbatim.
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
) -> std::result::Result<Solved, Skip> {
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
    // v2: LEFT is a CONSTRAINT, not a veto — position `k+1` becomes a barrier
    // below. FULL still refuses: #65 disables WHERE pushdown entirely when any
    // FULL is in the chain, so the `INNER JOIN … ON p` ≡ `CROSS JOIN … WHERE p`
    // move this rewrite is built on has no way back to a per-step ON there.
    // RIGHT was already rewritten to LEFT (or refused) before planning.
    // USING / NATURAL still refuse: their desugar picks the LEFTMOST occurrence
    // of a shared column as the coalesce representative, which a reorder moves.
    let mut barrier = vec![false; n];
    for (k, j) in s.joins.iter().enumerate() {
        if j.natural || !j.using.is_empty() {
            return Err(Skip::Ineligible);
        }
        match j.kind {
            ast::JoinKind::Inner => {}
            ast::JoinKind::Left => barrier[k + 1] = true,
            ast::JoinKind::Right | ast::JoinKind::Full => return Err(Skip::Ineligible),
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
    // `owner` = `Some(k)` for a conjunct of a BARRIER (LEFT) join's ON, which
    // stays attached to that join and therefore constrains ONLY its own
    // NULL-extended table `k`; `None` for everything the rewrite moves into the
    // WHERE, which #65 then places at the step of its last table.
    let mut conjuncts: Vec<(BExpr, Option<usize>)> = Vec::new();
    for (k, j) in s.joins.iter().enumerate() {
        let scope = Scope::joined_named(named[..=k + 1].to_vec()).map_err(|_| Skip::Unbindable)?;
        binder = binder.rescope(scope);
        let on = binder.bind_predicate(&j.on).map_err(|_| Skip::Unbindable)?;
        let mut split = Vec::new();
        split_and(on, &mut split);
        let owner = barrier[k + 1].then_some(k + 1);
        conjuncts.extend(split.into_iter().map(|c| (c, owner)));
    }
    if let Some(w) = &s.where_clause {
        let w = binder.bind_predicate(w).map_err(|_| Skip::Unbindable)?;
        let mut split = Vec::new();
        split_and(w, &mut split);
        conjuncts.extend(split.into_iter().map(|c| (c, None)));
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

    // A table's SEGMENT: barriers are singletons at their own textual position,
    // and each maximal free run between two of them is one segment. Free tables
    // never leave their segment, so the segment index alone decides which table
    // of a conjunct is the LAST one placed — which is where #65 evaluates it.
    let mut seg = vec![0usize; n];
    {
        let mut cur = 0usize;
        for p in 0..n {
            if barrier[p] {
                if p > 0 {
                    cur += 1;
                }
                seg[p] = cur;
                cur += 1;
            } else {
                seg[p] = cur;
            }
        }
    }
    // Does this conjunct land in the gather at all? A conjunct whose LAST table
    // is a LEFT join's inner side filters the already-NULL-extended row and
    // stays in `joined_filter` (#65's rule, `planner/join.rs`), so it restricts
    // no step and must not be priced as if it did.
    let lands_on_barrier = |m: u32| -> bool {
        let last = (0..n).filter(|&t| m & (1 << t) != 0).max_by_key(|&t| seg[t]);
        last.is_some_and(|t| barrier[t])
    };

    let mut nodes: Vec<Node> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| Node {
            def: d,
            // Lazily bought (§9.5) — the solver asks before the cost side pays.
            bucket: Cell::new(None),
            ndv: std::cell::OnceCell::new(),
            pins: Vec::new(),
            conj: Vec::new(),
            self_filter: false,
            adj: 0,
            barrier: barrier[i],
        })
        .collect();
    for (c, owner) in &conjuncts {
        let Some(m) = mask_of(c) else { return Err(Skip::Unbindable) };
        if m == 0 {
            continue; // column-free: placed at the outer, orders nothing
        }
        // Adjacency is search GUIDANCE only (which states the frontier DP
        // expands to), never a cost claim, so it is recorded for every conjunct.
        for (t, node) in nodes.iter_mut().enumerate() {
            if m & (1 << t) != 0 {
                node.adj |= m & !(1 << t);
            }
        }
        // Which table this conjunct is allowed to constrain.
        //   - a barrier's own ON: only its NULL-extended table. Under a LEFT
        //     join every preserved-side row survives whatever the ON says, so
        //     crediting the preserved side with it would be a false constraint.
        //   - a moved conjunct landing after a barrier: nothing at all.
        //   - otherwise: every table it mentions, as v1 did.
        let constrains: u32 = match owner {
            Some(k) => m & (1 << k),
            None if lands_on_barrier(m) => 0,
            None => m,
        };
        if constrains == 0 {
            continue;
        }
        for (t, node) in nodes.iter_mut().enumerate() {
            if constrains & (1 << t) != 0 {
                node.conj.push(m);
                if m == 1 << t {
                    node.self_filter = true;
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
            if constrains & (1 << t) == 0 {
                continue;
            }
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

    let p = Problem { n, nodes, ids, row_count, probes: Cell::new(0) };
    let chosen = p.solve();
    if !p.beats_textual(&chosen) {
        return Err(Skip::NoGain);
    }
    // Old base-row slot -> new one, for the lifted correlated subplans.
    let mut new_base = vec![0usize; n];
    let mut acc2 = 0usize;
    for &t in &chosen {
        new_base[t] = acc2;
        acc2 += defs[t].columns.len();
    }
    let mut slot_map = vec![0u16; acc];
    for (t, d) in defs.iter().enumerate() {
        for c in 0..d.columns.len() {
            slot_map[base[t] + c] = (new_base[t] + c) as u16;
        }
    }
    Ok(Solved { stmt: rewrite(s, &chosen, &named, &barrier), slot_map })
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
            BExpr::Coalesce(args)
            | BExpr::Call(_, args)
            | BExpr::CallColl(_, args, _)
            | BExpr::HostCall { args, .. }
            | BExpr::SpellCall { args, .. } => {
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
///
/// A BARRIER position (a LEFT join) keeps its table, its kind and its ON
/// verbatim — moving a LEFT join's ON into the WHERE would turn "does this row
/// match" into "does this row survive", a different query. Everything else in
/// the chain is an `INNER JOIN … ON true` whose predicate went to the WHERE.
fn rewrite(
    s: &ast::SelectStmt,
    order: &[usize],
    named: &[(String, &TableDef)],
    barrier: &[bool],
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
    for (k, j) in s.joins.iter().enumerate() {
        // A barrier's ON stays on its join; only INNER ONs move.
        if barrier[k + 1] {
            continue;
        }
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
        .enumerate()
        .map(|(p, &i)| {
            let (t, a) = entry(i);
            // `p + 1` is the emitted position; barriers were pinned to their
            // textual position by the solver, so `i == p + 1` holds there.
            let (kind, on) = if barrier[p + 1] {
                (s.joins[i - 1].kind, s.joins[i - 1].on.clone())
            } else {
                (ast::JoinKind::Inner, ast::Expr::Lit(Value::Bool(true)))
            };
            ast::JoinClause { table: t, alias: a, kind, on, using: Vec::new(), natural: false }
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
        let a = Cost { worst_log: 10, cartesian: 0, late_unconstrained: 99, residual_late: 99 };
        let b = Cost { worst_log: 11, cartesian: 0, late_unconstrained: 0, residual_late: 0 };
        assert!(a < b, "the worst-case product dominates");
        let c = Cost { worst_log: 10, cartesian: 5, late_unconstrained: 0, residual_late: 0 };
        assert!(a < c, "with equal worst case, fewer cartesian steps win");
        // v2: residual placement is the LAST word — it may only break ties the
        // first three terms leave open, never overturn one of them.
        let early = Cost { worst_log: 4, cartesian: 0, late_unconstrained: 0, residual_late: 3 };
        let late = Cost { worst_log: 4, cartesian: 0, late_unconstrained: 0, residual_late: 9 };
        assert!(early < late, "an equally-safe order that filters earlier wins");
        let riskier = Cost { worst_log: 5, cartesian: 0, late_unconstrained: 0, residual_late: 0 };
        assert!(late < riskier, "no amount of early filtering buys a worse worst case");
    }

    /// `n` tables `tK(a int64 PRIMARY KEY, b int64)` — the `select5` chain
    /// shape, small enough to build without an engine.
    fn chain_schema(n: usize) -> Schema {
        let c = |name: &str, nullable: bool| mpedb_types::ColumnDef {
            generated: None,
            decl: None,
            name: name.into(),
            ty: ColumnType::Int64,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: mpedb_types::Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ColumnType::Int64),
        };
        let tables = (1..=n)
            .map(|k| TableDef {
                id: 0,
                name: format!("t{k}"),
                columns: vec![c("a", false), c("b", true)],
                primary_key: vec![0],
                indexes: vec![],
                dead: false,
                implicit_rowid: false,
                kind: mpedb_types::TableKind::Standard,
            })
            .collect();
        Schema::new(tables).unwrap()
    }

    /// Compile `sql` and report how many DISTINCT tables the compile actually
    /// read a row count for.
    fn probes(sql: &str, schema: &Schema, n: usize) -> usize {
        let seen = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let f = |tid: u32| -> u64 {
            seen.borrow_mut().insert(tid);
            10
        };
        let cs = crate::CostSource { row_count: &f, index_ndv_bucket: &|_, _| None };
        crate::prepare_with_row_counts(sql, schema, &cs).expect("compiles");
        let hit = seen.borrow().len();
        assert!(hit <= n, "cannot probe more tables than the scope has");
        hit
    }

    /// **The ping-pong, measured** (design/DESIGN-MPEE-SOLVER.md §9.5). The
    /// scrambled chain with a late constant anchor is decided by STRUCTURE —
    /// which order makes every step a PK probe — and a PK probe is one row by
    /// proof, so its magnitude is never worth buying. v1 bought every table's
    /// count before the search started; v2 lets the search say which ones it
    /// needs, and the answer is a small constant instead of `n`.
    #[test]
    fn the_solver_buys_only_the_counts_its_decision_rests_on() {
        for n in [6usize, 10, 17] {
            let schema = chain_schema(n);
            let from: Vec<String> = (1..=n)
                .step_by(2)
                .chain((2..=n).step_by(2))
                .map(|k| format!("t{k}"))
                .collect();
            let mut w: Vec<String> =
                (1..n).map(|k| format!("t{k}.b = t{}.a", k + 1)).collect();
            w.push(format!("t{n}.a = 4"));
            let sql =
                format!("SELECT t1.a FROM {} WHERE {}", from.join(", "), w.join(" AND "));
            // Measured: exactly ONE, at n = 6, 10 and 17 alike — the scan the
            // chain starts from. Every other position is a PK probe, and a
            // probe's cost is a proof, not a statistic.
            assert_eq!(
                probes(&sql, &schema, n),
                1,
                "an {n}-table chain decided by structure should cost ONE row count"
            );
        }
    }

    /// **Stage A (design/DESIGN-MPEE-GENERAL.md §4): the NDV discount flips a
    /// star schema to dimension-first.** The measured failure this encodes
    /// (BENCHMARKS-OLAP.md): without per-index NDV, driving the 2M-row fact
    /// table and probing the dimension prices at bucket(2M) = 21, while
    /// dimension-first prices at bucket(5k) + bucket(2M) = 34 — so the solver
    /// scanned the fact table and discarded 89% of it after the join. With
    /// NDV, the dimension's filtered entry prices at bucket(5k) − bucket(8)
    /// and the fact's indexed join equality at bucket(2M) − bucket(5k):
    /// 9 + 8 = 17 < 21, and the dimension drives. Same query, same schema,
    /// same solver — the only change is which statistics exist, which is the
    /// whole claim of the CostSource seam.
    #[test]
    fn ndv_flips_a_star_to_dimension_first() {
        let c = |name: &str, ty: ColumnType, indexed: bool| mpedb_types::ColumnDef {
            generated: None,
            decl: None,
            name: name.into(),
            ty,
            nullable: true,
            unique: false,
            indexed,
            default: None,
            check: None,
            collation: mpedb_types::Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ty),
        };
        let key = |name: &str| mpedb_types::ColumnDef {
            nullable: false,
            ..c(name, ColumnType::Int64, false)
        };
        let schema = Schema::new(vec![
            TableDef {
                id: 0,
                name: "fact".into(),
                columns: vec![
                    key("id"),
                    c("product_id", ColumnType::Int64, true),
                    c("amount", ColumnType::Float64, false),
                ],
                primary_key: vec![0],
                indexes: vec![],
                dead: false,
                implicit_rowid: false,
                kind: mpedb_types::TableKind::Standard,
            },
            TableDef {
                id: 0,
                name: "product".into(),
                columns: vec![key("id"), c("category", ColumnType::Text, true)],
                primary_key: vec![0],
                indexes: vec![],
                dead: false,
                implicit_rowid: false,
                kind: mpedb_types::TableKind::Standard,
            },
        ])
        .unwrap();
        let sql = "SELECT f.amount FROM fact f, product p                    WHERE f.product_id = p.id AND p.category = 'tools'";
        let rows = |tid: u32| -> u64 { if tid == 0 { 2_000_000 } else { 5_000 } };

        // Unanalyzed: byte-identical to pre-stage-A behavior — fact drives.
        let blind = crate::CostSource { row_count: &rows, index_ndv_bucket: &|_, _| None };
        let plan = crate::prepare_with_row_counts(sql, &schema, &blind).unwrap();
        assert!(
            plan.explain(&schema).contains("join order: fact"),
            "without NDV the worst case is all that exists, and the fact drives"
        );

        // Analyzed: category NDV 8 (bucket 4), fact.product_id NDV 5000
        // (bucket 13) — the dimension drives.
        let ndv = |tid: u32, ixno: u32| -> Option<u32> {
            match (tid, ixno) {
                (0, 1) => Some(13), // fact.product_id
                (1, 1) => Some(4),  // product.category
                _ => None,
            }
        };
        let seen = crate::CostSource { row_count: &rows, index_ndv_bucket: &ndv };
        let plan = crate::prepare_with_row_counts(sql, &schema, &seen).unwrap();
        let explain = plan.explain(&schema);
        assert!(
            explain.contains("join order: product"),
            "with NDV the dimension must drive; got:
{explain}"
        );
        // And the fact must be entered through its join-key index, not scanned
        // — the access path the discount priced must be the one emitted.
        assert!(
            explain.contains("IndexPoint") || explain.contains("index"),
            "the fact side must be an index probe; got:
{explain}"
        );
    }

    /// The corollary: a chain with NO keyed access anywhere is decided by size
    /// alone, so the same mechanism pays for what it genuinely needs. This is
    /// the honest other half of the measurement above — laziness is a property
    /// of the QUESTION, not a trick that always wins.
    #[test]
    fn a_size_decided_chain_still_pays_for_its_sizes() {
        let schema = chain_schema(4);
        // Every conjunct joins two NON-key columns, so no step is ever KNOWN.
        let sql = "SELECT t1.a FROM t1, t2, t3, t4 \
                   WHERE t1.b = t2.b AND t2.b = t3.b AND t3.b = t4.b";
        assert!(probes(sql, &schema, 4) >= 3, "a size decision must buy sizes");
    }
}
