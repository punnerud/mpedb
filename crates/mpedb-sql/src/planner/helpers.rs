//! Freestanding planner helpers: index numbering, conflict probes, subplan
//! accounting, LIMIT-param typing (moved verbatim from `planner/mod.rs`).

use super::*;

/// Canonical secondary-index numbering helper (design/DESIGN.md §4.4): index 0 is
/// the PK tree; the returned vector lists the column index of secondary
/// index 1, 2, ... — columns with `unique = true` OR `indexed = true`, in
/// declaration order, skipping a column that is by itself the entire primary
/// key. UNIQUE index trees are keyed `value → pk`; non-unique ones use the
/// composite key `(value ‖ pk) → pk` (unique by construction).
pub fn secondary_indexes(table: &TableDef) -> Vec<Option<u16>> {
    // `TableDef.indexes` is the single source of index numbering
    // (DESIGN-SCHEMA-V2): index_no = position + 1, 0 = the PK tree. Each
    // entry is `Some(column)` for a single-column index — the only shape the
    // planner exploits until #55 — or `None` for a composite entry, which
    // KEEPS its index_no (numbering must stay aligned with the engine's
    // trees) but is never offered as an access path.
    table
        .indexes
        .iter()
        .map(|ix| match ix.columns[..] {
            // An EXPRESSION index (v13) is never an access path: its key is a
            // computed value, and matching a query's expression against a
            // stored one is a problem this planner does not solve. Offering it
            // would not be a missed optimisation, it would be a WRONG answer —
            // the key it holds is not the column's value.
            _ if !ix.exprs.is_empty() => None,
            [c] => Some(c),
            _ => None,
        })
        .collect()
}

/// How `ON CONFLICT (<target>) DO UPDATE` must find the conflicting row.
///
/// The single source of truth for both the planner (which records it) and
/// `CompiledPlan::validate` (which recomputes it and demands a match). A blob
/// claiming "target (email), probe pk" would find a row by PK and report it as
/// the email conflict — the wrong row, silently.
///
/// `None` = the target is neither the PK nor a single secondary UNIQUE column,
/// so there is no key to probe by.
pub(crate) fn conflict_probe_opt(table: &TableDef, target: &[u16]) -> Option<ConflictProbe> {
    if target == table.primary_key {
        return Some(ConflictProbe::Pk);
    }
    // A UNIQUE index whose column SET equals the target set can witness the
    // conflict (#55: composite targets included — order-insensitive, as in
    // PostgreSQL, which matches targets against unique indexes by column
    // set). A non-unique index cannot: nothing stops several rows from
    // sharing the values, so there is no single row to have conflicted
    // with — PG rejects the same shape at prepare.
    let mut want: Vec<u16> = target.to_vec();
    want.sort_unstable();
    let ino = table.indexes.iter().position(|ix| {
        // …and an expression index cannot serve a conflict target either: the
        // target names COLUMNS, and this index does not key by them.
        // A BUILDING unique index cannot witness a conflict: the row it would
        // have collided with may be ahead of the backfill and have no entry
        // yet. The probe would find nothing, ON CONFLICT would take the
        // insert branch, and a DUPLICATE row would land — the constraint
        // violated by the statement meant to respect it.
        if !ix.unique
            || !ix.usable_for_access()
            || ix.columns.len() != want.len()
            || ix.predicate.is_some()
            || !ix.exprs.is_empty()
        {
            return false;
        }
        let mut cols = ix.columns.clone();
        cols.sort_unstable();
        cols == want
    })?;
    Some(ConflictProbe::Index(ino as u32 + 1))
}

/// The validate-side view: a target that resolves to nothing is corrupt, and
/// `Pk` is the safe thing to compare an unresolvable one against (it will not
/// match a real `Index` plan).
pub(crate) fn conflict_probe(table: &TableDef, target: &[u16]) -> ConflictProbe {
    conflict_probe_opt(table, target).unwrap_or(ConflictProbe::Pk)
}

pub(super) fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Count every subplan in the tree — the top-level lifts plus, recursively,
/// each subplan's own nested lifts (#73 §3).
pub(super) fn total_subplans(subs: &[SubPlan]) -> usize {
    subs.iter()
        .map(|s| {
            1 + total_subplans(&s.subplans)
                + match &s.body {
                    // A compound body's arms own their lifts (format 56); they
                    // are part of THIS plan's tree and count against the same
                    // ceiling the decoder enforces.
                    SubBody::Compound(c) => compound_subplan_total(c),
                    SubBody::Select(_) => 0,
                    SubBody::Derived(dp) => derived_subplan_total(dp),
                }
        })
        .sum()
}

/// Every lift a materialized derived table owns, transitively: its body's own
/// list plus whatever its body owns in turn.
fn derived_subplan_total(dp: &crate::plan::DerivedPlan) -> usize {
    total_subplans(&dp.body_subplans)
        + match &dp.body {
            SubBody::Compound(c) => compound_subplan_total(c),
            SubBody::Select(_) => 0,
            SubBody::Derived(inner) => derived_subplan_total(inner),
        }
}

/// Every lift a compound's arms own, transitively.
fn compound_subplan_total(c: &CompoundPlan) -> usize {
    c.arm_subplans.iter().map(|a| total_subplans(a)).sum()
}

/// Lifts a statement's COMPONENTS own (never on the statement-level list): a
/// materialized derived table's body (format 52) and a compound's arms
/// (format 56), at every depth.
pub(super) fn owned_subplan_total(stmt: &PlanStmt) -> usize {
    match stmt {
        PlanStmt::Compound(c) => compound_subplan_total(c),
        PlanStmt::Derived(dp) => derived_subplan_total(dp),
        _ => 0,
    }
}

/// Hook for aggregate + correlated-slot discipline. All positions are legal
/// now: per-row (WHERE → `post_filter`, GROUP BY key, aggregate arg, FILTER)
/// fill via `row_params`; per-group (HAVING, non-key SELECT-list) use the
/// group's first base-row param scratch (sqlite bare-column convention; Django
/// OuterRef on a group key is constant within the group). Kept so a future
/// tightening has a single call site in select/join planners.
pub(super) fn reject_correlated_in_aggregate(
    sp: &SelectPlan,
    _sub_base: u16,
    _correlated: &[bool],
) -> Result<()> {
    let _ = sp;
    Ok(())
}


/// Every `LIMIT ?` / `OFFSET ?` parameter in the compiled tree types as
/// int64 — registered HERE, at the one chokepoint every statement passes,
/// instead of in each planning path. The binder never sees LIMIT (it flows
/// parser → planner as data, not as an expression), so this is its
/// `unify_param`: an untyped slot becomes int64, an int64 slot is confirmed,
/// and any other type is a loud conflict (`$n` can name one parameter twice).
pub(super) fn register_limit_params(
    stmt: &PlanStmt,
    subplans: &[SubPlan],
    ptypes: &mut [Option<ColumnType>],
) -> Result<()> {
    fn one(v: Option<LimitVal>, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        let Some(LimitVal::Param(i)) = v else { return Ok(()) };
        match ptypes.get(i as usize) {
            Some(None) => ptypes[i as usize] = Some(ColumnType::Int64),
            Some(Some(ColumnType::Int64)) => {}
            Some(Some(other)) => {
                return Err(bind_err(format!(
                    "parameter ${} is used both as {other:?} and as a \
                     LIMIT/OFFSET value (an integer)",
                    i + 1
                )))
            }
            // Parser bookkeeping caps every LIMIT param below n_user_params;
            // an index past the table is a planner bug, not user error.
            None => return Err(bind_err(format!("LIMIT parameter ${} out of range", i + 1))),
        }
        Ok(())
    }
    /// A window value function's integer argument, when it is a parameter.
    /// Same integer typing as LIMIT: the slot is Int64, so a caller binding
    /// text or a float is refused by the ordinary parameter check rather than
    /// coerced — which is precisely the per-row guessing the offset rules
    /// forbid, only moved to bind time.
    fn win(f: WindowFunc, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        let arg = match f {
            WindowFunc::Lag(v) | WindowFunc::Lead(v) | WindowFunc::NthValue(v)
            | WindowFunc::Ntile(v) => v,
            _ => return Ok(()),
        };
        let Some(i) = arg.param() else { return Ok(()) };
        match ptypes.get(i as usize) {
            Some(None) => ptypes[i as usize] = Some(ColumnType::Int64),
            Some(Some(ColumnType::Int64)) => {}
            Some(Some(other)) => {
                return Err(bind_err(format!(
                    "parameter ${} is used both as {other:?} and as a window \
                     function's integer argument",
                    i + 1
                )))
            }
            None => {
                return Err(bind_err(format!(
                    "window function parameter ${} out of range",
                    i + 1
                )))
            }
        }
        Ok(())
    }
    fn sel(sp: &SelectPlan, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        for w in &sp.windows {
            win(w.func, ptypes)?;
        }
        one(sp.limit, ptypes)?;
        one(sp.offset, ptypes)
    }
    fn comp(cp: &CompoundPlan, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        one(cp.limit, ptypes)?;
        one(cp.offset, ptypes)?;
        for arm in &cp.arms {
            match arm {
                CompoundArm::Select(sp) => sel(sp, ptypes)?,
                CompoundArm::Derived(dp) => derived(dp, ptypes)?,
            }
        }
        for list in &cp.arm_subplans {
            for sub in list {
                subplan(sub, ptypes)?;
            }
        }
        Ok(())
    }
    fn derived(dp: &DerivedPlan, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        match &dp.body {
            SubBody::Select(ref sp) => sel(sp, ptypes)?,
            SubBody::Compound(ref cp) => comp(cp, ptypes)?,
            SubBody::Derived(ref inner) => derived(inner, ptypes)?,
        }
        for sub in &dp.body_subplans {
            subplan(sub, ptypes)?;
        }
        sel(&dp.outer, ptypes)
    }
    fn subplan(sub: &SubPlan, ptypes: &mut [Option<ColumnType>]) -> Result<()> {
        match &sub.body {
            SubBody::Select(sp) => sel(sp, ptypes)?,
            SubBody::Compound(cp) => comp(cp, ptypes)?,
            SubBody::Derived(dp) => derived(dp, ptypes)?,
        }
        for child in &sub.subplans {
            subplan(child, ptypes)?;
        }
        Ok(())
    }
    match stmt {
        PlanStmt::Select(sp) => sel(sp, ptypes)?,
        PlanStmt::Compound(cp) => comp(cp, ptypes)?,
        PlanStmt::Derived(dp) => derived(dp, ptypes)?,
        PlanStmt::RecursiveCte(rc) => {
            sel(&rc.anchor, ptypes)?;
            sel(&rc.recursive, ptypes)?;
            sel(&rc.outer, ptypes)?;
        }
        PlanStmt::Insert { from_select: Some(fs), .. } => sel(fs.plan.output_select(), ptypes)?,
        _ => {}
    }
    for sub in subplans {
        subplan(sub, ptypes)?;
    }
    Ok(())
}

