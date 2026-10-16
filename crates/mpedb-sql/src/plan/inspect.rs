//! [`CompiledPlan`] introspection: decltypes, target table, hash, host/spell-call walkers.

use super::*;

impl CompiledPlan {
    pub fn validate_enum_inputs(&self, params: &[Value]) -> mpedb_types::Result<()> {
        let projection = |xs: &[Projection]| -> mpedb_types::Result<()> {
            for p in xs {
                if let Projection::Expr { program, .. } | Projection::SetReturning { program, .. } = p {
                    program.validate_enum_inputs(params)?;
                }
            }
            Ok(())
        };
        match &self.stmt {
            PlanStmt::Select(s) => {
                if let Some(p) = &s.filter { p.validate_enum_inputs(params)?; }
                projection(&s.projection)?;
            }
            PlanStmt::Update { filter, set, returning, .. } => {
                if let Some(p) = filter { p.validate_enum_inputs(params)?; }
                for (_,p) in set { p.validate_enum_inputs(params)?; }
                if let Some(xs) = returning { projection(xs)?; }
            }
            PlanStmt::Delete { filter, returning, .. } => {
                if let Some(p) = filter { p.validate_enum_inputs(params)?; }
                if let Some(xs) = returning { projection(xs)?; }
            }
            PlanStmt::Insert { returning: Some(xs), .. } => projection(xs)?,
            _ => {}
        }
        Ok(())
    }

    /// Describe the columns of an ordinary view/query without reading rows.
    /// Reshaped aggregate/window/derived outputs are left undescribed here.
    pub fn output_pg_columns(&self, schema: &Schema) -> Vec<PgOutputColumn> {
        let PlanStmt::Select(sp) = &self.stmt else {
            return Vec::new();
        };
        if !sp.joins.is_empty() || sp.aggregate.is_some() || !sp.windows.is_empty() {
            return Vec::new();
        }
        sp.projection
            .iter()
            .take(sp.projection.len().saturating_sub(sp.order_junk as usize))
            .map(|p| {
                let source = match p {
                    Projection::Column(n) => schema
                        .table(sp.table)
                        .and_then(|t| t.columns.get(*n as usize)),
                    Projection::Expr { program, .. } => match program.instrs.as_slice() {
                        [Instr::PushCol(n)] => schema
                            .table(sp.table)
                            .and_then(|t| t.columns.get(*n as usize)),
                        _ => None,
                    },
                    _ => None,
                };
                let name = match p {
                    Projection::Column(_) => source.map(|c| c.name.clone()).unwrap_or_default(),
                    Projection::Expr { name, .. } | Projection::SetReturning { name, .. } => {
                        name.clone()
                    }
                };
                PgOutputColumn {
                    name,
                    type_oid: projection_pg_oid(
                        p,
                        sp.table,
                        schema,
                        &self.subplans,
                        self.subplan_base(),
                    ),
                    type_mod: source
                        .and_then(|c| c.decltype())
                        .and_then(mpedb_types::pgtype::type_modifier)
                        .unwrap_or(-1),
                }
            })
            .collect()
    }

    /// PostgreSQL output OIDs, including RETURNING and explicit strict casts.
    /// This is separate from sqlite decltypes: a computed cast has a PG type
    /// even though sqlite correctly reports no declared column type for it.
    pub fn output_pg_oids(&self, schema: &Schema) -> Vec<Option<u32>> {
        let (table, projection) = match &self.stmt {
            PlanStmt::Select(sp)
                if sp.joins.is_empty() && sp.aggregate.is_none() && sp.windows.is_empty() =>
            {
                (
                    sp.table,
                    &sp.projection[..sp.projection.len().saturating_sub(sp.order_junk as usize)],
                )
            }
            PlanStmt::Insert {
                table,
                returning: Some(p),
                ..
            }
            | PlanStmt::Update {
                table,
                returning: Some(p),
                ..
            }
            | PlanStmt::Delete {
                table,
                returning: Some(p),
                ..
            } => (*table, p.as_slice()),
            _ => return Vec::new(),
        };
        projection
            .iter()
            .map(|p| projection_pg_oid(p, table, schema, &self.subplans, self.subplan_base()))
            .collect()
    }

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

    /// Per output column: the base TABLE the column comes from
    /// (libsqlite3 `sqlite3_column_table_name`), or `None` where sqlite also
    /// reports NULL. Same rule as [`Self::output_decltypes`] and derived from
    /// the same plan-level source mapping, not from names: only an output
    /// column that is a bare reference to a real base-table column has a
    /// source table. Anything computed — an expression, an aggregate, a
    /// window — has none, and a join reshapes the tuple so `Projection::Column`
    /// no longer indexes one table's row. A non-SELECT yields an empty vec,
    /// which the caller reads as "all NULL".
    pub fn output_table_names(&self, schema: &Schema) -> Vec<Option<String>> {
        let PlanStmt::Select(sp) = &self.stmt else {
            return Vec::new();
        };
        let out_n = sp.projection.len().saturating_sub(sp.order_junk as usize);
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
                // The column must really exist on the table: an out-of-range
                // index is not a source, and must not borrow the table's name.
                Projection::Column(n) => table
                    .columns
                    .get(*n as usize)
                    .map(|_| table.name.to_string()),
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

/// Follow a scalar subquery's result slot back to its actual output. The slot's
/// storage type cannot distinguish timestamp/timestamptz or uuid/bytea.
fn projection_pg_oid(
    projection: &Projection,
    table: u32,
    schema: &Schema,
    subplans: &[SubPlan],
    sub_base: u16,
) -> Option<u32> {
    match projection {
        Projection::Column(n) => schema
            .table(table)
            .and_then(|t| t.columns.get(*n as usize))
            .and_then(|c| c.pg_enum.as_ref().map(|e| e.oid)
                .or_else(|| c.decltype().and_then(mpedb_types::pgtype::by_name).map(|t| t.oid))),
        Projection::Expr { program, .. } => {
            if let Some(Instr::CastPg(oid)) = program.instrs.last() {
                return Some(*oid);
            }
            let [Instr::PushParam(slot)] = program.instrs.as_slice() else {
                return match program.instrs.as_slice() {
                    [Instr::PushCol(n)] => projection_pg_oid(&Projection::Column(*n), table, schema, subplans, sub_base),
                    [Instr::PushConst(n)] => program.consts.get(*n as usize).and_then(Value::column_type).map(mpedb_types::pgtype::default_oid),
                    _ => None,
                };
            };
            let sub = subplans.get(slot.checked_sub(sub_base)? as usize)?;
            if sub.kind != SubPlanKind::Scalar {
                return None;
            }
            let SubBody::Select(sp) = &sub.body else {
                return None;
            };
            if !sp.joins.is_empty() || sp.aggregate.is_some() || !sp.windows.is_empty() {
                return None;
            }
            projection_pg_oid(
                sp.projection.first()?,
                sp.table,
                schema,
                &sub.subplans,
                sub.sub_base,
            )
        }
        Projection::SetReturning { .. } => None,
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
