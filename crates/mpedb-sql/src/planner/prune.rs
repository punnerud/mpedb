//! **The output requirement, made to constrain the row pipeline** (#125).
//!
//! The MPEE solver prices what a statement must *read* — row counts, predicate
//! classes, cartesian risk. It prices nothing about what the statement must
//! *produce*. `SELECT count(*)` produces one integer; the join underneath it
//! nevertheless materialises every column of every table it touches, because
//! nothing in the pipeline ever told it which columns anything downstream can
//! observe. This module computes that set.
//!
//! # Scope: WIDTH only, and that is the whole safety argument
//!
//! Dropping a **column** changes nothing observable: it is a width
//! optimisation, safe by construction as long as the observable set is a
//! superset of what the executor actually reads. Dropping a **join**, a
//! `DISTINCT` or a `GROUP BY` changes which **rows** exist — those are
//! rewrites, they can produce wrong answers, and they deliberately do not
//! share this code path (see `design/DESIGN-STREAM-EXEC.md` §10).
//!
//! So the only way this can be wrong is by MISSING a consumer. Three things
//! guard against that:
//!
//! 1. [`row_prune`] destructures [`SelectPlan`] **exhaustively**, by name. A
//!    field added to the plan is a compile error here, not a silent hole —
//!    which is precisely the failure mode `crate::plan` documents for
//!    `mpedb::access`'s report (it drops `windows` and `returning` through a
//!    `..` pattern and still calls itself exact).
//! 2. Pruning **truncates** wherever it can and only NULLs interior holes.
//!    A forgotten consumer above the trim point reads out of bounds, and
//!    [`mpedb_types::Instr::PushCol`] answers an out-of-range slot with
//!    `Error::Internal` — a loud failure, not a wrong answer.
//! 3. Every shape is differential-tested against the bundled sqlite oracle on
//!    value *and* `typeof()` (`tests/prune_width.rs`).
//!
//! # What "observable" means here
//!
//! The executor's row pipeline for one `SELECT` is a left-deep nested loop:
//! stage 0 is the outer table's base row, and stage *k* is
//! `[stage k-1 ‖ joins[k-1]'s table]`. Each stage has its OWN observable set,
//! computed backwards from the output:
//!
//! ```text
//!   mask[m]  = what the OUTPUT reads                       (m = the last stage)
//!   mask[k]  = mask[k+1] restricted to this stage's width
//!            ∪ join k's ON, restricted to this stage's width
//!            ∪ join k's access path (its `KeyPart::OuterCol`s)
//! ```
//!
//! One mask for every stage would be simpler and is what the first version
//! did — and it is a **materially weaker** answer. A column that only join 1's
//! `ON` reads would then pin the FINAL tuple too, so `SELECT count(*)` over a
//! join retained a 4-wide row where it should retain an empty one. Computing
//! the suffix union per stage is what makes "the delivery is one integer" reach
//! all the way down: the last stage of a `count(*)` observes nothing, and its
//! product is a set of zero-width rows.
//!
//! The HELD inner side of a join needs a third set, not either of those: it is
//! narrowed before the `ON` runs against it, so it keeps `mask[k+1]`'s inner
//! half *plus* whatever join `k`'s own `ON` reads there.
//!
//! Deliberately NOT in any set, each for a reason that is a fact about the
//! executor rather than an optimism:
//!
//! - `filter` (the base-row residual) and `Join::policy` — both are applied
//!   *inside* the gather, over the unpruned row, before anything is narrowed.
//! - `having`, and an aggregate's `projection`/`order_by` — those index the
//!   GROUPED tuple `[keys ‖ aggs ‖ bare_cols]`, a different tuple whose every
//!   input is already collected via `group_by`/`aggs`/`bare_cols`.
//! - `limit`/`offset` — constants.
//! - `distinct` — it deduplicates the projection, already collected.
//!
//! And two that ARE in the set although no expression names them:
//!
//! - the **primary key of the outer table**, whenever the plan aggregates:
//!   sqlite's bare-column witness picks a group's lowest-rowid row and reads
//!   the PK off the base row to do it (`exec/aggregate.rs`, `Witness::MinRowid`),
//!   with a silent `unwrap_or(Value::Null)`.
//! - each correlated subplan's `outer_args`, which name base-row slots filled
//!   per row by `correlated_survivors`.

use crate::plan::{
    AccessPath, GroupKey, OrderOver, Projection, SelectPlan,
};
use mpedb_types::{ExprProgram, Instr, KeyBound, KeyPart};

/// One tuple's observable slots: `keep[i]` is true when some later stage can
/// read slot `i`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mask {
    keep: Vec<bool>,
}

impl Mask {
    /// Can anything read slot `i`?
    ///
    /// `true` for anything past the mask: an index this analysis never saw is
    /// one it cannot claim is dead, and the safe direction is to keep it.
    #[inline]
    pub fn observes(&self, i: usize) -> bool {
        self.keep.get(i).copied().unwrap_or(true)
    }

    /// How many leading slots the tuple must keep — one past the highest
    /// observable slot, or `0` when nothing is observable at all. Zero is the
    /// interesting case and not a degenerate one: `SELECT count(*)` over a join
    /// retains a set of EMPTY rows, which is all a row count needs.
    pub fn trim(&self) -> usize {
        self.keep.iter().rposition(|k| *k).map_or(0, |p| p + 1)
    }

    /// The tuple's logical width — what it would be with nothing dropped.
    pub fn width(&self) -> usize {
        self.keep.len()
    }

    /// Does narrowing this tuple change anything? When it does not, the
    /// executor keeps the row it already built and pays nothing at all.
    pub fn prunes(&self) -> bool {
        self.keep.iter().any(|k| !k)
    }
}

/// Which slots of each stage of one `SELECT`'s row pipeline a later stage can
/// observe, and therefore which ones the executor may stop carrying. Built by
/// [`row_prune`]; applied by `mpedb::exec::gather`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPrune {
    /// `stage[k]` describes the tuple accumulated through join `k-1`; `stage[0]`
    /// is the outer table's base row and `stage[joins.len()]` is the output of
    /// the whole pipeline.
    stage: Vec<Mask>,
    /// `inner[j]` describes join `j`'s HELD inner relation, in that relation's
    /// OWN coordinates. Wider than `stage[j+1]`'s inner half by join `j`'s `ON`,
    /// which is evaluated against the held rows after they are narrowed.
    inner: Vec<Mask>,
}

impl RowPrune {
    /// The mask for the tuple accumulated through `k` joins (`k = 0` is the
    /// base row). Out of range yields an empty mask, which claims nothing.
    pub fn stage(&self, k: usize) -> &Mask {
        self.stage.get(k).unwrap_or(EMPTY)
    }

    /// The mask for join `j`'s held inner relation, in the inner table's own
    /// column coordinates.
    pub fn inner(&self, j: usize) -> &Mask {
        self.inner.get(j).unwrap_or(EMPTY)
    }
}

/// The mask that claims nothing: `observes` is `true` everywhere past its
/// (zero) length, so an out-of-range stage keeps every column.
static EMPTY: &Mask = &Mask { keep: Vec::new() };

fn set(keep: &mut [bool], i: u16) {
    if let Some(s) = keep.get_mut(i as usize) {
        *s = true;
    }
}

/// Every column a compiled expression reads. `PushCol` is the only
/// column-naming instruction in the IR — the program is a flat stack machine,
/// so there is no nesting to recurse through and no second opcode to forget.
fn prog_cols(p: &ExprProgram, keep: &mut [bool]) {
    for instr in &p.instrs {
        if let Instr::PushCol(i) = instr {
            set(keep, *i);
        }
    }
}

fn bound_cols(b: &Option<KeyBound>, keep: &mut [bool]) {
    if let Some(b) = b {
        for p in &b.parts {
            if let KeyPart::OuterCol(i) = p {
                set(keep, *i);
            }
        }
    }
}

/// The outer-row slots an index nested loop resolves its key from
/// (`KeyPart::OuterCol`). A `Param`/`Const` part reads no row.
fn access_outer_cols(a: &AccessPath, keep: &mut [bool]) {
    match a {
        AccessPath::PkPoint(parts) | AccessPath::IndexPoint { parts, .. } => {
            for p in parts {
                if let KeyPart::OuterCol(i) = p {
                    set(keep, *i);
                }
            }
        }
        AccessPath::PkRange { lo, hi } | AccessPath::IndexRange { lo, hi, .. } => {
            bound_cols(lo, keep);
            bound_cols(hi, keep);
        }
        // A full scan has no key, and an FTS query tree carries literal terms
        // only — neither can name the outer row.
        AccessPath::FullScan | AccessPath::FtsScan { .. } => {}
    }
}

/// The per-stage observable-column masks for one `SELECT`'s row pipeline, or
/// `None` when there is nothing to gain (every slot is read) or nothing safe
/// to say.
///
/// - `widths[0]` is the outer table's column count and `widths[j + 1]` is
///   `joins[j]`'s table's — so `widths.len() == joins.len() + 1`.
/// - `outer_pk` is the outer table's primary-key column indices — pinned
///   whenever the plan aggregates, because sqlite's bare-column witness reads
///   them off the base row without going through an expression.
/// - `correlated_args` is the union of this level's correlated subplans'
///   `outer_args`: base-row slots the per-row correlation fill reads.
///
/// # Refusals
///
/// A plan carrying **window functions** is refused outright. Its projection
/// indexes the *extended* tuple `[base ‖ window results]`, and the window
/// phase (`exec/window.rs`) reads partition/order/frame values through several
/// side vectors that this analysis does not model. Windows are a separate
/// executor; pruning them is separate work.
///
/// A `widths` that does not describe this plan's joins is refused too — the
/// whole analysis is about absolute slot positions, so a layout it cannot
/// reconstruct is one it must not prune.
pub fn row_prune(
    sp: &SelectPlan,
    widths: &[usize],
    outer_pk: &[u16],
    correlated_args: &[u16],
) -> Option<RowPrune> {
    // Exhaustive by name: a new field on `SelectPlan` breaks this build rather
    // than quietly becoming a column nobody knows is read. That is the whole
    // guard against the one failure mode this module has.
    let SelectPlan {
        table: _,
        // The OUTER access path resolves from params and consts only — validate
        // refuses `OuterCol` outside a join — so it names no slot of a row.
        access: _,
        joins,
        // Applied INSIDE the gather, over the full-width row, before anything
        // here narrows it.
        filter: _,
        joined_filter,
        post_filter,
        projection,
        order_by,
        order_over,
        limit: _,
        offset: _,
        aggregate,
        // Junk columns are trailing PROJECTION entries; they are covered by
        // walking `projection` in full.
        order_junk: _,
        // DISTINCT deduplicates the projection, which is already collected.
        distinct: _,
        windows,
    } = sp;

    if !windows.is_empty() || widths.len() != joins.len() + 1 {
        return None;
    }
    // Cumulative stage widths: `cum[k]` is the width of the tuple accumulated
    // through `k` joins, so `cum[0]` is the base row and `cum[m]` the output.
    let mut cum = Vec::with_capacity(widths.len());
    let mut total = 0usize;
    for w in widths {
        total += w;
        cum.push(total);
    }
    if total == 0 {
        return None;
    }

    // ---- the OUTPUT requirement: what the last stage's tuple must carry ----
    let mut out = vec![false; total];
    if let Some(f) = joined_filter {
        prog_cols(f, &mut out);
    }
    if let Some(f) = post_filter {
        prog_cols(f, &mut out);
    }
    // An aggregate's projection indexes the GROUPED tuple, not this one.
    if aggregate.is_none() {
        for p in projection {
            match p {
                Projection::Column(i) => set(&mut out, *i),
                Projection::Expr { program, .. } => prog_cols(program, &mut out),
            }
        }
    }
    if let Some(agg) = aggregate {
        for k in &agg.group_by {
            match k {
                GroupKey::Col(i) => set(&mut out, *i),
                GroupKey::Expr(p) => prog_cols(p, &mut out),
            }
        }
        for a in &agg.aggs {
            // `arg` is `None` for `count(*)` — the ROW is the argument, and a
            // row with no columns is still a row.
            if let Some(p) = &a.arg {
                prog_cols(p, &mut out);
            }
            if let Some(p) = &a.filter {
                prog_cols(p, &mut out);
            }
            for p in &a.extra_args {
                prog_cols(p, &mut out);
            }
        }
        for &c in &agg.bare_cols {
            set(&mut out, c);
        }
        // The bare-column witness: with no min/max to govern it the executor
        // picks the group's lowest-rowid row, reading the PK straight off the
        // base row. It is the one base-row read no expression names.
        for &c in outer_pk {
            set(&mut out, c);
        }
    }
    // ORDER BY over the base/joined tuple — the only `order_over` whose indices
    // address the tuple these masks describe. `cmp_rows` SKIPS a key it cannot
    // find rather than failing, so a missed sort key would silently reorder.
    if *order_over == OrderOver::BaseRow {
        for (i, _, _) in order_by {
            set(&mut out, *i);
        }
    }
    for &c in correlated_args {
        set(&mut out, c);
    }

    // ---- walk the stages BACKWARDS, accumulating the suffix union ----------
    //
    // `acc` is always "everything the stages at or after this one read". Going
    // backwards is what keeps join 1's ON from pinning the OUTPUT tuple: its
    // columns enter the running set only once the walk reaches the stage that
    // feeds it.
    let m = joins.len();
    let mut acc = out;
    let mut stage: Vec<Mask> = vec![Mask::default(); m + 1];
    let mut inner: Vec<Mask> = vec![Mask::default(); m];
    stage[m] = Mask { keep: acc.clone() };
    for j in (0..m).rev() {
        // Join `j`'s HELD inner side is narrowed BEFORE its ON runs against it,
        // so it keeps what stage j+1 keeps there PLUS what the ON reads there.
        let mut with_on = acc.clone();
        prog_cols(&joins[j].on, &mut with_on);
        inner[j] = Mask { keep: with_on[cum[j]..cum[j + 1]].to_vec() };
        // …and stage `j` itself keeps the same union, restricted to its width,
        // plus the outer slots the inner's access path resolves its key from.
        acc = with_on;
        access_outer_cols(&joins[j].access, &mut acc);
        acc.truncate(cum[j]);
        stage[j] = Mask { keep: acc.clone() };
    }

    // Nothing to gain anywhere: narrowing would rebuild each row to produce the
    // row it already is.
    if !stage.iter().any(Mask::prunes) && !inner.iter().any(Mask::prunes) {
        return None;
    }
    Some(RowPrune { stage, inner })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(bits: &[bool]) -> Mask {
        Mask { keep: bits.to_vec() }
    }

    #[test]
    fn trim_is_one_past_the_last_observable_slot() {
        assert_eq!(mask(&[false, true, false, false]).trim(), 2);
        assert_eq!(mask(&[true, false]).trim(), 1);
    }

    /// `SELECT count(*)`: nothing is observable, so the retained rows are
    /// EMPTY. That is the shape the whole exercise exists to produce.
    #[test]
    fn nothing_observable_trims_to_zero() {
        let p = mask(&[false, false, false]);
        assert_eq!(p.trim(), 0);
        assert!(p.prunes());
    }

    /// Past the mask nothing is claimed: a slot this analysis does not
    /// describe reads as observable, which is the safe direction.
    #[test]
    fn beyond_the_mask_keeps_everything() {
        assert!(mask(&[false, true]).observes(9));
        assert!(EMPTY.observes(0));
        assert_eq!(EMPTY.trim(), 0);
        assert!(!EMPTY.prunes());
    }

    #[test]
    fn prunes_reports_interior_holes_too() {
        let p = mask(&[false, true]);
        assert_eq!(p.trim(), 2);
        assert!(p.prunes(), "slot 0 is a hole even though the width is unchanged");
        assert!(!mask(&[true, true]).prunes());
    }

    /// An out-of-range stage or join index must claim nothing rather than
    /// panic — the executor indexes these by position.
    #[test]
    fn out_of_range_stages_claim_nothing() {
        let p = RowPrune { stage: vec![mask(&[false])], inner: Vec::new() };
        assert!(p.stage(7).observes(3));
        assert!(p.inner(0).observes(0));
    }
}
