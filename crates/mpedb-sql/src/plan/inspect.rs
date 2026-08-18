//! [`CompiledPlan`] introspection: decltypes, target table, hash, host/spell-call walkers.

use super::*;

impl CompiledPlan {
    /// Per output column: the declared type name to report as its `decltype`
    /// (libsqlite3 `sqlite3_column_decltype`), or `None` where sqlite also
    /// reports NULL. A `decltype` exists ONLY for an output column that is a
    /// bare reference to a real base-table column — the plan-derived source
    /// mapping, not a name heuristic. Anything computed (an expression, an
    /// aggregate, a window, a join column, a typeless `ANY` column) has no
    /// declared type ⇒ `None`. A non-SELECT (or RETURNING) yields an empty vec,
    /// which the caller reads as "all NULL".
    pub fn output_decltypes(&self, schema: &Schema) -> Vec<Option<String>> {
        let PlanStmt::Select(sp) = &self.stmt else {
            return Vec::new();
        };
        // Real OUTPUT columns exclude the trailing sort-only "junk" projections.
        let out_n = sp.projection.len().saturating_sub(sp.order_junk as usize);
        // Only a single-table scan with no aggregate/window keeps the projection
        // over the BASE ROW, so `Projection::Column(n)` indexes table column `n`.
        // Joins/aggregates/windows reshape the tuple — no base-column source.
        if !sp.joins.is_empty() || sp.aggregate.is_some() || !sp.windows.is_empty() {
            return vec![None; out_n];
        }
        let Some(table) = schema.table(sp.table) else {
            return vec![None; out_n];
        };
        sp.projection
            .iter()
            .take(out_n)
            .map(|p| match p {
                Projection::Column(n) => table
                    .columns
                    .get(*n as usize)
                    .and_then(|c| c.decltype().map(str::to_string)),
                // Neither has a declared type: one is computed, and the
                // other is computed AND expands the row.
                Projection::Expr { .. } | Projection::SetReturning { .. } => None,
            })
            .collect()
    }

    /// The table this plan targets (for RLS policy-epoch validation), if any.
    pub fn target_table(&self) -> Option<u32> {
        match &self.stmt {
            PlanStmt::Select(SelectPlan { table, .. })
            | PlanStmt::Insert { table, .. }
            | PlanStmt::Update { table, .. }
            | PlanStmt::Delete { table, .. } => Some(*table),
            // A compound has no SINGLE target; staleness is covered by the
            // per-arm entries in `policies`, which stamp every table read.
            PlanStmt::Compound(_) => None,
            // A recursive CTE reads several base tables (across anchor /
            // recursive / outer); like a compound it has no single target, and
            // `policies` stamps each real table read.
            PlanStmt::RecursiveCte(_) => None,
            // A derived-table statement likewise reads several tables (body +
            // outer); `policies` stamps each real table read.
            PlanStmt::Derived(_) => None,
            PlanStmt::Begin
            | PlanStmt::Commit
            | PlanStmt::Rollback
            | PlanStmt::Savepoint(_)
            | PlanStmt::Release(_)
            | PlanStmt::RollbackTo(_) => None,
        }
    }

    /// Content hash: blake3(canonical bytes ‖ schema_hash ‖ FORMAT_VERSION).
    pub fn hash(&self) -> PlanHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.encode());
        hasher.update(&self.schema_hash);
        hasher.update(&FORMAT_VERSION.to_le_bytes());
        PlanHash(*hasher.finalize().as_bytes())
    }

    /// Does this plan call a HOST-registered UDF — a scalar (`Instr::HostCall`)
    /// or, since stage 2, an aggregate (`AggTarget::Host`),
    /// design/DESIGN-UDF.md? Such a plan is valid ONLY for
    /// the connection that registered the function, so the facade must NOT
    /// publish it to the shared content-hashed `plan/<hash>` registry — it
    /// compiles-and-executes it locally each time instead. Computed by scanning
    /// every embedded [`ExprProgram`], so a plan decoded from the registry (which
    /// by this very rule can never carry a host call) correctly reports `false`.
    pub fn contains_host_call(&self) -> bool {
        stmt_has_host_call(&self.stmt) || self.subplans.iter().any(subplan_has_host_call)
    }

    /// Does any expression anywhere in this plan call a STORED function
    /// ([`Instr::SpellCall`])? Unlike a host call this does NOT bar registry
    /// publication — the definition lives in the file — it decides whether an
    /// execution needs the spell table in scope. The walk mirrors the host
    /// walker's structure but checks only PROGRAMS: spells exist solely as
    /// scalar calls (no spell aggregates/collations/windows). If a future
    /// plan node is added to one walker and missed here, the failure mode is
    /// a clean runtime "not in scope" refusal, never a wrong answer.
    pub fn contains_spell_call(&self) -> bool {
        stmt_has_spell_call(&self.stmt) || self.subplans.iter().any(subplan_has_spell_call)
    }

    /// True when this WRITE statement carries a `RETURNING` clause, i.e. its
    /// result is `Rows`, not `Affected`. Such a plan must never be enqueued
    /// on the intent ring: a ring result slot carries only an affected count
    /// (design/DESIGN.md §5.3), so a leader executing it as a FOREIGN intent
    /// has nowhere to post the rows (`execute_prepared` refuses with "write
    /// plan returned rows"). The facade keeps these on the direct
    /// writer-lock path, where the owner executes its own statement and
    /// keeps the full result.
    pub fn has_returning(&self) -> bool {
        matches!(
            &self.stmt,
            PlanStmt::Insert { returning: Some(_), .. }
                | PlanStmt::Update { returning: Some(_), .. }
                | PlanStmt::Delete { returning: Some(_), .. }
        )
    }
}

/// `Instr::HostCall` anywhere in an optional program.
fn opt_prog_host_call(p: &Option<ExprProgram>) -> bool {
    p.as_ref().is_some_and(ExprProgram::has_host_call)
}

/// `Instr::HostCall` anywhere in a projection list (SELECT list / RETURNING).
fn projection_host_call(proj: &[Projection]) -> bool {
    proj.iter().any(|p| match p {
        Projection::Column(_) => false,
        Projection::Expr { program, .. } | Projection::SetReturning { program, .. } => {
            program.has_host_call()
        }
    })
}

fn select_has_host_call(sp: &SelectPlan) -> bool {
    opt_prog_host_call(&sp.filter)
        || opt_prog_host_call(&sp.joined_filter)
        || opt_prog_host_call(&sp.post_filter)
        || sp.joins.iter().any(|j| {
            j.on.has_host_call() || j.policy.as_ref().is_some_and(ExprProgram::has_host_call)
        })
        || projection_host_call(&sp.projection)
        || sp.aggregate.as_ref().is_some_and(|a| {
            a.group_by.iter().any(|k| matches!(k, GroupKey::Expr(p) if p.has_host_call()))
                || a.aggs.iter().any(|c| {
                    // The aggregate ITSELF may be host-registered (stage 2), not
                    // just its argument/filter expressions — same
                    // one-connection-only rule, same registry gate.
                    c.func.host().is_some()
                        || opt_prog_host_call(&c.arg)
                        || opt_prog_host_call(&c.filter)
                        || c.extra_args.iter().any(ExprProgram::has_host_call)
                })
                || opt_prog_host_call(&a.having)
        })
        // A HOST collating sequence is resolved through the connection's
        // registry at sort time, so a plan naming one is connection-local for
        // the same reason a host function call is (design/DESIGN-UDF.md stage 3).
        || sp.order_by.iter().any(|(_, _, c)| c.host().is_some())
        || sp.windows.iter().any(|w| {
            // The window function ITSELF may be host-registered (format 55),
            // not just its sub-expressions — same one-connection-only rule.
            w.host.is_some()
                || opt_prog_host_call(&w.arg)
                || w.partition_by.iter().any(ExprProgram::has_host_call)
                || w.order_by.iter().any(|(p, _)| p.has_host_call())
                || opt_prog_host_call(&w.default)
        })
}

/// Does any part of a materialized derived plan call a HOST function? Its
/// outer, its body (to any nesting), and the lifts it owns.
fn derived_has_host_call(dp: &DerivedPlan) -> bool {
    select_has_host_call(&dp.outer)
        || subbody_has_host_call(&dp.body)
        || dp.body_subplans.iter().any(|s| subbody_has_host_call(&s.body))
}

fn compound_has_host_call(c: &CompoundPlan) -> bool {
    c.arms.iter().any(|a| match a {
        CompoundArm::Select(sp) => select_has_host_call(sp),
        CompoundArm::Derived(dp) => derived_has_host_call(dp),
    })
        // The compound's OWN `ORDER BY` may name a host collating sequence —
        // and it is the only one the arms cannot carry, since the trailing
        // ORDER BY belongs to the whole compound.
        || c.order_by.iter().any(|(_, _, coll)| coll.host().is_some())
}

fn subbody_has_host_call(b: &SubBody) -> bool {
    match b {
        SubBody::Select(sp) => select_has_host_call(sp),
        SubBody::Compound(c) => compound_has_host_call(c),
        SubBody::Derived(dp) => derived_has_host_call(dp),
    }
}

fn subplan_has_host_call(s: &SubPlan) -> bool {
    subbody_has_host_call(&s.body) || s.subplans.iter().any(subplan_has_host_call)
}

fn stmt_has_host_call(stmt: &PlanStmt) -> bool {
    match stmt {
        PlanStmt::Select(sp) => select_has_host_call(sp),
        PlanStmt::Compound(c) => compound_has_host_call(c),
        PlanStmt::RecursiveCte(rc) => {
            select_has_host_call(&rc.anchor)
                || select_has_host_call(&rc.recursive)
                || select_has_host_call(&rc.outer)
        }
        PlanStmt::Derived(dp) => {
            subbody_has_host_call(&dp.body)
                || select_has_host_call(&dp.outer)
                // A body-owned lift can call a host UDF too (format 52).
                // Missing it would publish a connection-local plan to the
                // SHARED registry — the leak this gate exists to close.
                || dp.body_subplans.iter().any(subplan_has_host_call)
        }
        PlanStmt::Insert {
            rows,
            from_select,
            with_check,
            on_conflict,
            returning,
            ..
        } => {
            // `rows` is load-bearing here, not completeness for its own sake:
            // this predicate decides BOTH whether the executor gets the host
            // table AND whether the plan may enter the shared registry. A
            // `VALUES (myfn(1))` cell that this walk did not see would be a
            // connection-local call published for every process to execute.
            rows.iter()
                .flatten()
                .any(|s| matches!(s, InsertSource::Expr(p) if p.has_host_call()))
                || from_select
                    .as_ref()
                    .is_some_and(|s| select_has_host_call(s.plan.output_select()))
                || opt_prog_host_call(with_check)
                || returning.as_ref().is_some_and(|r| projection_host_call(r))
                || on_conflict_host_call(on_conflict)
        }
        PlanStmt::Update {
            filter,
            set,
            with_check,
            returning,
            ..
        } => {
            opt_prog_host_call(filter)
                || set.iter().any(|(_, p)| p.has_host_call())
                || opt_prog_host_call(with_check)
                || returning.as_ref().is_some_and(|r| projection_host_call(r))
        }
        PlanStmt::Delete { filter, returning, .. } => {
            opt_prog_host_call(filter)
                || returning.as_ref().is_some_and(|r| projection_host_call(r))
        }
        PlanStmt::Begin
        | PlanStmt::Commit
        | PlanStmt::Rollback
        | PlanStmt::Savepoint(_)
        | PlanStmt::Release(_)
        | PlanStmt::RollbackTo(_) => false,
    }
}

fn opt_prog_spell_call(p: &Option<ExprProgram>) -> bool {
    p.as_ref().is_some_and(ExprProgram::has_spell_call)
}

fn projection_spell_call(proj: &[Projection]) -> bool {
    proj.iter().any(|p| match p {
        Projection::Column(_) => false,
        Projection::Expr { program, .. } | Projection::SetReturning { program, .. } => {
            program.has_spell_call()
        }
    })
}

fn select_has_spell_call(sp: &SelectPlan) -> bool {
    opt_prog_spell_call(&sp.filter)
        || opt_prog_spell_call(&sp.joined_filter)
        || opt_prog_spell_call(&sp.post_filter)
        || sp
            .joins
            .iter()
            .any(|j| j.on.has_spell_call() || j.policy.as_ref().is_some_and(ExprProgram::has_spell_call))
        || projection_spell_call(&sp.projection)
        || sp.aggregate.as_ref().is_some_and(|a| {
            a.group_by.iter().any(|k| matches!(k, GroupKey::Expr(p) if p.has_spell_call()))
                || a.aggs.iter().any(|c| {
                    opt_prog_spell_call(&c.arg)
                        || opt_prog_spell_call(&c.filter)
                        || c.extra_args.iter().any(ExprProgram::has_spell_call)
                })
                || opt_prog_spell_call(&a.having)
        })
        || sp.windows.iter().any(|w| {
            opt_prog_spell_call(&w.arg)
                || w.partition_by.iter().any(ExprProgram::has_spell_call)
                || w.order_by.iter().any(|(p, _)| p.has_spell_call())
                || opt_prog_spell_call(&w.default)
        })
}

fn compound_has_spell_call(c: &CompoundPlan) -> bool {
    c.arms.iter().any(|a| match a {
        CompoundArm::Select(sp) => select_has_spell_call(sp),
        CompoundArm::Derived(dp) => derived_has_spell_call(dp),
    })
}

/// The spell twin of [`derived_has_host_call`].
fn derived_has_spell_call(dp: &DerivedPlan) -> bool {
    select_has_spell_call(&dp.outer)
        || subbody_has_spell_call(&dp.body)
        || dp.body_subplans.iter().any(subplan_has_spell_call)
}

fn subbody_has_spell_call(b: &SubBody) -> bool {
    match b {
        SubBody::Select(sp) => select_has_spell_call(sp),
        SubBody::Compound(c) => compound_has_spell_call(c),
        SubBody::Derived(dp) => derived_has_spell_call(dp),
    }
}

fn subplan_has_spell_call(s: &SubPlan) -> bool {
    subbody_has_spell_call(&s.body) || s.subplans.iter().any(subplan_has_spell_call)
}

fn stmt_has_spell_call(stmt: &PlanStmt) -> bool {
    match stmt {
        PlanStmt::Select(sp) => select_has_spell_call(sp),
        PlanStmt::Compound(c) => compound_has_spell_call(c),
        PlanStmt::RecursiveCte(rc) => {
            select_has_spell_call(&rc.anchor)
                || select_has_spell_call(&rc.recursive)
                || select_has_spell_call(&rc.outer)
        }
        PlanStmt::Derived(dp) => {
            subbody_has_spell_call(&dp.body)
                || select_has_spell_call(&dp.outer)
                || dp.body_subplans.iter().any(subplan_has_spell_call)
        }
        PlanStmt::Insert { from_select, with_check, on_conflict, returning, .. } => {
            from_select
                .as_ref()
                .is_some_and(|s| select_has_spell_call(s.plan.output_select()))
                || opt_prog_spell_call(with_check)
                || returning.as_ref().is_some_and(|r| projection_spell_call(r))
                || match on_conflict {
                    PlanOnConflict::Error | PlanOnConflict::DoNothing | PlanOnConflict::Replace => {
                        false
                    }
                    PlanOnConflict::DoUpdate { set, filter, .. } => {
                        set.iter().any(|(_, p)| p.has_spell_call())
                            || opt_prog_spell_call(filter)
                    }
                }
        }
        PlanStmt::Update { filter, set, with_check, returning, .. } => {
            opt_prog_spell_call(filter)
                || set.iter().any(|(_, p)| p.has_spell_call())
                || opt_prog_spell_call(with_check)
                || returning.as_ref().is_some_and(|r| projection_spell_call(r))
        }
        PlanStmt::Delete { filter, returning, .. } => {
            opt_prog_spell_call(filter)
                || returning.as_ref().is_some_and(|r| projection_spell_call(r))
        }
        _ => false,
    }
}

fn on_conflict_host_call(oc: &PlanOnConflict) -> bool {
    match oc {
        PlanOnConflict::Error | PlanOnConflict::DoNothing | PlanOnConflict::Replace => false,
        PlanOnConflict::DoUpdate { set, filter, .. } => {
            set.iter().any(|(_, p)| p.has_host_call()) || opt_prog_host_call(filter)
        }
    }
}
