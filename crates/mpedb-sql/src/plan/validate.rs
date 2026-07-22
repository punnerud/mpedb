use super::*;
use super::decode::corrupt;

impl CompiledPlan {
    /// Semantic re-validation against the schema: index/column/parameter
    /// bounds, PK shapes, typed constants, and footprint consistency
    /// (recomputed from scratch and compared, so a forged footprint in an
    /// otherwise well-formed blob is rejected).
    pub(crate) fn validate(&self, schema: &Schema) -> Result<()> {
        let ptypes = &self.param_types;
        // ONE tree budget for the whole plan — statement-level lifts, a derived
        // body's owned lifts and every compound arm's owned lifts draw from it,
        // exactly as the decoder does, so ownership can never buy a bigger
        // forest than the flat format allowed.
        let mut budget = MAX_SUBPLANS;
        match &self.stmt {
            PlanStmt::Select(sp) => self.validate_select(sp, schema, ptypes)?,
            PlanStmt::Compound(c) => self.validate_compound(
                c,
                schema,
                ptypes,
                self.n_user_params() as usize,
                &mut budget,
            )?,
            PlanStmt::RecursiveCte(rc) => self.validate_recursive_cte(rc, schema, ptypes)?,
            PlanStmt::Derived(dp) => {
                // Statement-level lifts are filled against the OUTER row of a
                // top-level derived, which has no base-table correlation — empty
                // is required. Nested Derived arms of a compound are validated
                // via validate_compound and do not take this path.
                if !self.subplans.is_empty() {
                    return Err(corrupt("derived-table statement with lifted subqueries"));
                }
                self.validate_derived(dp, schema, ptypes, &mut budget)?;
            }
            _other => self.validate_rest(schema)?,
        }
        if !self.subplans.is_empty() {
            self.validate_subplans(schema, &mut budget)?;
        } else if let PlanStmt::Select(sp) = &self.stmt {
            if sp.post_filter.is_some() {
                return Err(corrupt("post-filter without subplans"));
            }
        }
        // Footprint consistency for EVERY statement kind: recomputed from
        // scratch and compared, so a forged footprint in an otherwise
        // well-formed blob is rejected.
        let recomputed = planner::compute_footprint(&self.stmt, &self.subplans, schema)?;
        if recomputed != self.footprint {
            return Err(corrupt("plan footprint does not match its statement"));
        }
        Ok(())
    }

    /// Re-validate a compound `SELECT … UNION/… …` against `ptypes` — the arm
    /// count, the op count, that no arm smuggles its own ORDER BY/LIMIT or a
    /// post-filter, that every arm agrees on the output arity, and that the
    /// compound ORDER BY names an output column. Shared between a top-level
    /// compound statement and a compound subquery body (format 31), so the two
    /// can never drift.
    ///
    /// `level_base` is where THIS compound's parameter level ends and its arms'
    /// OWNED reserved slots begin (format 56) — the statement's user-param count
    /// at the top, `sub_base` inside a lifted subquery, `body_sub_base` inside a
    /// derived table. Pinning it here is what stops a forged `arm_sub_base` from
    /// pointing the arms' fill at live user slots.
    fn validate_compound(
        &self,
        c: &CompoundPlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
        level_base: usize,
        budget: &mut usize,
    ) -> Result<()> {
        if !(2..=MAX_COMPOUND_ARMS).contains(&c.arms.len()) {
            return Err(corrupt("compound arm count out of range"));
        }
        if c.ops.len() != c.arms.len() - 1 {
            return Err(corrupt("compound op count does not match arm count"));
        }
        // The arms' OWNED lifts (format 56). Either no list at all, or exactly
        // one per arm, based where this level's parameters end.
        if !c.arm_subplans.is_empty() && c.arm_subplans.len() != c.arms.len() {
            return Err(corrupt("compound arm-subplan list count does not match arm count"));
        }
        if c.arm_sub_base as usize != level_base {
            return Err(corrupt("compound arm-subplan base does not match its parameter level"));
        }
        if level_base > ptypes.len() {
            return Err(corrupt("compound arm-subplan base out of range"));
        }
        // The arms' parameter space: this level's, then one slot per arm lift in
        // layout order, typed by the lift's own declared `slot_type` (so a lift
        // used as a key part still type-checks, and a forged `param_types` entry
        // cannot re-type a reserved slot).
        let mut types: Vec<Option<ColumnType>> = ptypes.to_vec();
        if !c.arm_subplans.is_empty() {
            types.truncate(level_base);
            for arm in &c.arm_subplans {
                if arm.len() > MAX_SUBPLANS {
                    return Err(corrupt("too many subplans in one compound arm"));
                }
                for s in arm {
                    types.push(s.slot_type);
                }
            }
        }
        // The gather-side slot discipline runs over the WHOLE arm-lift region
        // for EVERY arm, not just over the arm's own slice: a correlated slot of
        // ANY arm is an unfilled hole while another arm gathers, so reading one
        // across arms is exactly as illegal as reading one's own.
        let all_correlated: Vec<bool> = c
            .arm_subplans
            .iter()
            .flat_map(|a| a.iter().map(|s| !s.outer_args.is_empty()))
            .collect();
        let arity = c.arms[0].output_arity();
        for (k, arm) in c.arms.iter().enumerate() {
            let out = arm.output_select();
            // The compound owns ORDER BY/LIMIT — SQL cannot express them per arm,
            // so an arm carrying its own is forged. And with no junk,
            // `projection.len()` IS the output arity, which the set ops and the
            // compound sort both index.
            if !out.order_by.is_empty()
                || out.order_junk != 0
                || out.limit.is_some()
                || out.offset.is_some()
            {
                return Err(corrupt("compound arm carries its own ORDER BY/LIMIT"));
            }
            // A `post_filter` is applied only on the per-row path, which an arm
            // takes only when it OWNS a correlated lift — the same rule the
            // subplan level follows. Without one it would be silently ignored,
            // so its presence is forgery.
            let lifts = c.arm_lifts(k);
            if out.post_filter.is_some() && !lifts.iter().any(|s| !s.outer_args.is_empty()) {
                return Err(corrupt("compound arm carries a post-filter"));
            }
            if arm.output_arity() != arity {
                return Err(corrupt("compound arms disagree on output arity"));
            }
            match arm {
                crate::plan::CompoundArm::Select(sp) => {
                    self.validate_select(sp, schema, &types)?;
                    self.check_correlated_slots(sp, level_base, &all_correlated)?;
                }
                crate::plan::CompoundArm::Derived(dp) => {
                    // Nested derived as a compound arm: validate the derived
                    // plan against the same param level; body slots start at
                    // body_sub_base (the arm's planning base).
                    self.validate_derived(dp, schema, &types, budget)?;
                }
            }
        }
        // Each Select arm's lifts, against the ARM's row and the ARM's reserved
        // base — the same recursion the statement level and the derived body use.
        // Derived arms own their lifts inside the DerivedPlan (validated above).
        for (k, lifts) in c.arm_subplans.iter().enumerate() {
            if lifts.is_empty() {
                continue;
            }
            let base_k = c.arm_base(k) as usize;
            let arm = c
                .arms
                .get(k)
                .ok_or_else(|| corrupt("compound arm-subplan list longer than the arms"))?;
            let arm_outer = self.select_row_types(arm.output_select(), schema)?;
            let arm_ptypes = &types[..base_k.min(types.len())];
            for s in lifts {
                self.validate_subplan_rec(schema, s, arm_ptypes, &arm_outer, budget)?;
            }
        }
        for (i, _, _) in &c.order_by {
            if *i as usize >= arity {
                return Err(corrupt("compound order-by column out of range"));
            }
        }
        Ok(())
    }

    /// Re-validate a recursive CTE and its §3 restrictions (design/
    /// DESIGN-CTE-RECURSIVE.md §3) so a hand-crafted plan cannot smuggle an
    /// illegal recursive reference past the binder. The three components are
    /// then checked as ordinary selects (`CTE_TABLE` resolves through
    /// `validate_select`'s CTE-aware `get_table`).
    fn validate_recursive_cte(
        &self,
        rc: &RecursiveCtePlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        // How many times a select's FROM/JOIN operands name the working table.
        let reads_cte = |sp: &SelectPlan| -> usize {
            (sp.table == super::CTE_TABLE) as usize
                + sp.joins.iter().filter(|j| j.table == super::CTE_TABLE).count()
        };
        let arity = rc.columns.len();
        if arity == 0 || arity != rc.col_types.len() {
            return Err(corrupt("recursive CTE column/type arity mismatch"));
        }
        // Stage 1 carries no lifted subqueries and no correlated post-filters —
        // the parameter layout is `[user]` only.
        if !self.subplans.is_empty() {
            return Err(corrupt("recursive CTE with lifted subqueries"));
        }
        for sp in [&rc.anchor, &rc.recursive, &rc.outer] {
            if sp.post_filter.is_some() {
                return Err(corrupt("recursive CTE component carries a post-filter"));
            }
        }
        // The anchor is non-recursive: it must NOT reference the working table,
        // and its projection arity fixes the CTE's shape.
        if reads_cte(&rc.anchor) != 0 {
            return Err(corrupt("recursive CTE anchor references the working table"));
        }
        if rc.anchor.projection.len() != arity {
            return Err(corrupt("recursive CTE anchor arity does not match the column list"));
        }
        // The recursive term references the working table EXACTLY once, as a
        // FROM/JOIN operand, and only through an INNER join — never a
        // null-extended (LEFT/FULL) side (§3).
        if reads_cte(&rc.recursive) != 1 {
            return Err(corrupt(
                "recursive CTE term must reference the working table exactly once",
            ));
        }
        for j in &rc.recursive.joins {
            if j.table == super::CTE_TABLE && j.kind != JoinKind::Inner {
                return Err(corrupt(
                    "recursive CTE reference on the null-extended side of an outer join",
                ));
            }
        }
        // No aggregate / GROUP BY / DISTINCT / window in the recursive term (§3).
        if rc.recursive.aggregate.is_some()
            || rc.recursive.distinct
            || !rc.recursive.windows.is_empty()
        {
            return Err(corrupt(
                "recursive CTE term uses an aggregate/GROUP BY/DISTINCT/window",
            ));
        }
        if rc.recursive.projection.len() != arity {
            return Err(corrupt("recursive CTE term arity does not match the column list"));
        }
        // Structural validation of each component (widths, access paths, program
        // bounds). CTE_TABLE resolves through the CTE-aware `get_table`.
        self.validate_select(&rc.anchor, schema, ptypes)?;
        self.validate_select(&rc.recursive, schema, ptypes)?;
        self.validate_select(&rc.outer, schema, ptypes)?;
        Ok(())
    }

    /// Re-validate a MATERIALIZED derived table (design/DESIGN-DERIVED-TABLES.md
    /// §5) so a hand-crafted blob cannot smuggle an illegal shape past the
    /// planner: the column list matches the body's output arity, the body never
    /// references the working table (a derived table cannot see itself), and the
    /// outer reads it exactly once. The components are then checked as ordinary
    /// selects (`CTE_TABLE` resolves through `validate_select`'s CTE-aware
    /// `get_table`).
    fn validate_derived(
        &self,
        dp: &DerivedPlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
        budget: &mut usize,
    ) -> Result<()> {
        // How many times a select's FROM/JOIN operands name the working table.
        let reads_cte = |sp: &SelectPlan| -> usize {
            (sp.table == super::CTE_TABLE) as usize
                + sp.joins.iter().filter(|j| j.table == super::CTE_TABLE).count()
        };
        let arity = dp.columns.len();
        if arity == 0 || arity != dp.col_types.len() {
            return Err(corrupt("derived-table column/type arity mismatch"));
        }
        if dp.body.output_arity() != arity {
            return Err(corrupt("derived-table body arity does not match the column list"));
        }
        // The STATEMENT-level list stays empty only for a top-level
        // PlanStmt::Derived. Nested Derived as a compound arm is validated with
        // the parent's subplans still present — the empty check is only for the
        // top-level node (caller: validate_stmt).
        //
        // The OUTER never has a per-row fill phase (it scans a materialised
        // set), so a post-filter there is forgery. A SELECT body's is governed
        // by `validate_body_subplans`; a COMPOUND body's arms by
        // `validate_compound`, which allows one exactly where the arm owns a
        // correlated lift.
        if dp.outer.post_filter.is_some() {
            return Err(corrupt("derived-table outer carries a post-filter"));
        }
        if let SubBody::Select(sp) = &dp.body {
            if sp.post_filter.is_some() && !dp.body_subplans.iter().any(|s| !s.outer_args.is_empty())
            {
                return Err(corrupt("derived-table body carries a post-filter"));
            }
        }
        // The body cannot reference the working table it DEFINES.
        let body_reads_cte = match &dp.body {
            SubBody::Select(sp) => reads_cte(sp),
            SubBody::Compound(c) => c
                .arms
                .iter()
                .map(|a| match a {
                    crate::plan::CompoundArm::Select(sp) => reads_cte(sp),
                    // A nested derived arm defines its own working table; its
                    // outer may read CTE_TABLE, which is that arm's, not this
                    // derived's — still refuse if the body Select of the nest
                    // somehow names this level's CTE (planner never produces it).
                    crate::plan::CompoundArm::Derived(inner) => {
                        reads_cte(&inner.outer)
                            + match &inner.body {
                                SubBody::Select(sp) => reads_cte(sp),
                                SubBody::Compound(c2) => c2
                                    .arms
                                    .iter()
                                    .map(|a2| reads_cte(a2.output_select()))
                                    .sum::<usize>(),
                            }
                    }
                })
                .sum(),
        };
        // For a compound body's nested Derived arms, CTE_TABLE in the arm
        // outer is the nested derived's own working table — not this dp's.
        // Only refuse if a plain Select arm (or nested body select) names it.
        // Re-check with a simpler rule: only Select arms of this body.
        let plain_body_cte = match &dp.body {
            SubBody::Select(sp) => reads_cte(sp),
            SubBody::Compound(c) => c
                .arms
                .iter()
                .filter_map(|a| a.as_select())
                .map(reads_cte)
                .sum(),
        };
        let _ = body_reads_cte;
        if plain_body_cte != 0 {
            return Err(corrupt("derived-table body references the working table"));
        }
        // The outer reads the materialized rows exactly once (its FROM, or —
        // after the RIGHT-join rewrite — one join operand).
        if reads_cte(&dp.outer) != 1 {
            return Err(corrupt(
                "derived-table outer must reference the working table exactly once",
            ));
        }
        // Structural validation of each component (widths, access paths, program
        // bounds). The outer binds CTE_TABLE to THIS derived's working table —
        // including when this Derived is a nested compound arm (format 58).
        match &dp.body {
            SubBody::Select(sp) => self.validate_select_cte(sp, schema, ptypes, None)?,
            // A COMPOUND body owns its lifts one level further down, per ARM
            // (format 56); its reserved region starts where the body's own
            // would have.
            SubBody::Compound(c) => self.validate_compound(
                c,
                schema,
                ptypes,
                dp.body_sub_base as usize,
                budget,
            )?,
        }
        let working = dp.derived_def();
        self.validate_select_cte(&dp.outer, schema, ptypes, Some(&working))?;
        self.validate_body_subplans(dp, schema, budget)?;
        Ok(())
    }

    /// Re-validate the BODY's own lifted subqueries (format 52) — the exact
    /// checks [`validate_subplans`](Self::validate_subplans) makes at the
    /// statement level, but against the BODY's row and the BODY's reserved-slot
    /// base, because that is what the executor fills them from.
    ///
    /// Everything the two levels share is shared as CODE
    /// (`check_slot_discipline`, `validate_subplan_rec`), so a hand-crafted
    /// blob cannot get a weaker check by putting its lifts on the body.
    fn validate_body_subplans(
        &self,
        dp: &DerivedPlan,
        schema: &Schema,
        budget: &mut usize,
    ) -> Result<()> {
        // Reserved slots the body owns: its own lifts, or — for a compound body
        // — its arms' (format 56, checked by `validate_compound`).
        let arm_slots = match &dp.body {
            SubBody::Compound(c) => {
                (c.n_arm_slots() + c.n_derived_body_slots()) as usize
            }
            SubBody::Select(_) => 0,
        };
        let base = dp.body_sub_base as usize;
        let end = base + dp.body_subplans.len() + arm_slots;
        // Top-level Derived fills [user ‖ body slots] to n_params. Nested
        // Derived as a compound arm occupies a MID slice of a wider buffer —
        // only require the region fits, do not demand it is the whole tail.
        if end > self.n_params as usize {
            return Err(corrupt(
                "derived-table body subplan slots out of the reserved parameter region",
            ));
        }
        if matches!(self.stmt, PlanStmt::Derived(_))
            && end != self.n_params as usize
        {
            return Err(corrupt(
                "derived-table body subplan slots do not fill the reserved parameter region",
            ));
        }
        if dp.body_subplans.is_empty() {
            return Ok(());
        }
        // Only a plain SELECT body has the per-row fill phase a correlated lift
        // needs; a compound body's lifts belong to its ARMS, never to the body.
        let body = dp.body.as_select().ok_or_else(|| {
            corrupt("derived-table subplans on a compound body")
        })?;
        if dp.body_subplans.len() > MAX_SUBPLANS {
            return Err(corrupt("too many derived-table body subplans"));
        }
        // `[user ‖ body subplans]`, with nothing after: the context region must
        // be empty (a derived table refuses `current_setting()` and `'now'` in
        // both components).
        if !self.context_keys.is_empty() {
            return Err(corrupt("derived-table statement with reserved context slots"));
        }
        // The OUTER row a body lift correlates to is the BODY's base row —
        // `[table0 ‖ … ‖ tableN]` — NOT the outer statement's materialised
        // tuple. Getting this wrong is exactly how a forged `outer_arg` would
        // read the wrong column, so it is bounds-checked against the real one.
        let outer_types = self.select_row_types(body, schema)?;
        self.check_slot_discipline(body, base, &dp.body_subplans)?;
        let user_ptypes = &self.param_types[..base];
        for s in &dp.body_subplans {
            self.validate_subplan_rec(schema, s, user_ptypes, &outer_types, budget)?;
        }
        Ok(())
    }

    /// Everything `validate` checks about one SELECT — shared verbatim between
    /// a top-level SELECT and each compound arm, so the two can never drift.
    fn validate_select(
        &self,
        sp: &SelectPlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        // Default working-table binding from the statement node; nested
        // Derived arms pass theirs via `validate_select_cte`.
        let cte_td: Option<TableDef> = match &self.stmt {
            PlanStmt::RecursiveCte(rc) => Some(rc.cte_def()),
            PlanStmt::Derived(dp) => Some(dp.derived_def()),
            _ => None,
        };
        self.validate_select_cte(sp, schema, ptypes, cte_td.as_ref())
    }

    fn validate_select_cte(
        &self,
        sp: &SelectPlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
        cte_td: Option<&TableDef>,
    ) -> Result<()> {
        let get_table = |id: u32| -> Result<&TableDef> {
            if id == super::CTE_TABLE {
                return cte_td
                    .ok_or_else(|| corrupt("CTE working table outside a recursive CTE / derived table"));
            }
            schema
                .table(id)
                .ok_or_else(|| corrupt(format!("table id {id} out of range")))
        };
        {
            {
                let SelectPlan {
                    table,
                    access,
                    joins,
                    joined_filter,
                    filter,
                    projection,
                    order_by,
                    order_over,
                    aggregate,
                    distinct,
                    order_junk,
                    ..
                } = sp;
                // The DUAL sentinel (FROM-less SELECT) is legal ONLY in its
                // narrowest shape: no joins (nothing to join a non-table to)
                // and a full "scan" (no columns exist to probe). Everything
                // below then bounds widths against the zero-column def.
                let t = if *table == super::DUAL_TABLE {
                    if !joins.is_empty() {
                        return Err(corrupt("joins on a FROM-less select"));
                    }
                    if !matches!(access, AccessPath::FullScan) {
                        return Err(corrupt("keyed access on a FROM-less select"));
                    }
                    super::dual_def()
                } else if *table == super::CTE_TABLE {
                    // The recursive CTE's working table: no PK and no indexes, so
                    // the ONLY sound access over it is a FullScan (the executor
                    // reads it from an in-memory row set, never through a key
                    // tree). Joins ARE allowed — the recursive term may join it
                    // with a base table.
                    if !matches!(access, AccessPath::FullScan) {
                        return Err(corrupt("keyed access on a recursive CTE working table"));
                    }
                    get_table(*table)?
                } else {
                    get_table(*table)?
                };
                // Junk columns are sort-only and get trimmed, so they must not
                // be able to (a) eat the whole output, (b) survive a DISTINCT —
                // where they would dedup on a value the caller never sees — or
                // (c) exist where nothing sorts the projection.
                let junk = *order_junk as usize;
                if junk > 0 {
                    if *order_over != OrderOver::Projection {
                        return Err(corrupt("order-junk columns without a projection sort"));
                    }
                    if *distinct {
                        return Err(corrupt("order-junk columns under DISTINCT"));
                    }
                    if junk >= projection.len() {
                        return Err(corrupt("order-junk columns leave no output"));
                    }
                }
                self.check_access(access, t, None, ptypes)?;
                // With a join the "base row" IS the joined row, so every width
                // below moves. Getting this wrong is not cosmetic: a program
                // bounded against the outer's width alone could not name the
                // inner's columns at all, and one bounded against nothing could
                // read past the tuple.
                if joins.len() > MAX_JOINS {
                    return Err(corrupt("too many joins in plan"));
                }
                // Width accumulates left to right: join `k`'s `on` runs over
                // `[table0 ‖ … ‖ table_{k+1}]`, so its bound grows as we go. Each
                // join's POLICY runs over its OWN row alone (the whole point of
                // it being separate), so it is bounded by that one table's width.
                // A self-join (same table id twice) is legal since #44 — tables
                // are addressed by alias, and the plan carries slots, not names.
                let mut acc_width = t.columns.len();
                // The accumulated tuple's column TYPES, for OuterCol parts:
                // join `k`'s access resolves against the tuple built BEFORE
                // its own table joins in.
                let mut acc_types: Vec<ColumnType> =
                    t.columns.iter().map(|c| c.ty).collect();
                for j in joins {
                    let jt = get_table(j.table)?;
                    // The working table as a join operand is FullScan-only, for
                    // the same reason as in the outer position — no key tree.
                    if j.table == super::CTE_TABLE && !matches!(j.access, AccessPath::FullScan) {
                        return Err(corrupt("keyed access on a recursive CTE working table"));
                    }
                    // FULL needs the inner side enumerated and held: single
                    // join, FullScan access — the executor's unmatched-inner
                    // sweep is built on exactly that.
                    // FULL is allowed at any chain position (the left-deep
                    // gather composes it correctly wherever it sits), but its
                    // inner side must be a held FullScan: the executor's
                    // unmatched-inner sweep needs every inner row enumerated, so
                    // a keyed FULL inner is refused as forged (the planner never
                    // emits one — it forces FullScan for FULL).
                    if j.kind == JoinKind::Full && !matches!(j.access, AccessPath::FullScan) {
                        return Err(corrupt("FULL join with a keyed inner access"));
                    }
                    self.check_access(&j.access, jt, Some(&acc_types), ptypes)?;
                    if let Some(p) = &j.policy {
                        self.check_program(p, jt, ptypes)?;
                    }
                    acc_width += jt.columns.len();
                    acc_types.extend(jt.columns.iter().map(|c| c.ty));
                    self.check_program_width(&j.on, acc_width, ptypes)?;
                }
                let base_width = acc_width; // the full joined row
                if let Some(jf) = joined_filter {
                    if joins.is_empty() {
                        return Err(corrupt("joined filter without a join"));
                    }
                    self.check_program_width(jf, base_width, ptypes)?;
                }
                if let Some(pf) = &sp.post_filter {
                    // Over the base (joined) row; it may read correlated
                    // subplan slots — the per-program discipline for the
                    // GATHER-side programs is enforced in `validate`.
                    self.check_program_width(pf, base_width, ptypes)?;
                }
                // The sort key is an index into whichever tuple `order_over`
                // names, and those have different widths. Bounding it against
                // the wrong one is not a style point: too LOOSE lets a hostile
                // plan index past the tuple, and too TIGHT is worse than it
                // sounds — `cmp_rows` skips a key it cannot fetch, so an
                // out-of-range index silently drops the sort rather than
                // failing, and the caller gets an unordered answer to an
                // ORDER BY query.
                let order_width = |projection_len: usize, grouped: Option<usize>| match order_over {
                    OrderOver::BaseRow => base_width,
                    OrderOver::Grouped => grouped.unwrap_or(0),
                    OrderOver::Projection => projection_len,
                };
                if let Some(f) = filter {
                    // The OUTER's policy/residual, over the outer row alone.
                    self.check_program(f, t, ptypes)?;
                }
                // Windows and aggregation are mutually exclusive (stage 1): the
                // window phase runs over base rows, the aggregate over grouped
                // tuples — one tuple model per plan. A blob claiming both is
                // forged (the planner refuses the SQL in-process).
                if aggregate.is_some() && !sp.windows.is_empty() {
                    return Err(corrupt("windows together with an aggregate"));
                }
                if let Some(a) = aggregate {
                    // GROUP BY columns and aggregate ARGUMENTS index the BASE
                    // row — which for a join is the JOINED row, hence
                    // `base_width` and not the outer table's; HAVING and the
                    // projection index the GROUPED tuple `[keys ‖ aggs]`, which
                    // is a different width again. Checking either against the
                    // wrong one would let a hostile plan read past its row — so
                    // they are bounded separately.
                    for k in &a.group_by {
                        match k {
                            GroupKey::Col(c) => {
                                if *c as usize >= base_width {
                                    return Err(corrupt("GROUP BY column out of range"));
                                }
                            }
                            GroupKey::Expr(p) => {
                                self.check_program_width(p, base_width, ptypes)?
                            }
                        }
                    }
                    for c in &a.aggs {
                        if let Some(p) = &c.arg {
                            self.check_program_width(p, base_width, ptypes)?;
                        }
                        // FILTER (WHERE …) (format 38): a predicate over the same
                        // base row as the argument, so bound it by `base_width`.
                        if let Some(p) = &c.filter {
                            self.check_program_width(p, base_width, ptypes)?;
                        }
                        // Host-aggregate arguments after the first (format 51):
                        // the same base row `arg` is evaluated over.
                        for p in &c.extra_args {
                            self.check_program_width(p, base_width, ptypes)?;
                        }
                    }
                    // sqlite bare columns (format 30) extend the grouped tuple to
                    // `[keys ‖ aggs ‖ bare_cols]`. Each is a BASE-row column, so
                    // bound it by `base_width` — the executor never indexes the row
                    // past this, so the bound is the whole safety obligation here.
                    // The WITNESS row a bare column is read from is inferred at exec
                    // from the aggregate set (single min/max → the extremum's row;
                    // otherwise → the group's lowest-rowid row via the min-PK
                    // witness), and both readers are memory-safe for any aggregate
                    // set, so no min/max shape is required of a decoded plan. The
                    // never-a-wrong-answer gate for the lowest-rowid case (single
                    // INTEGER-PK table, no join) lives in the planner, which is the
                    // only producer of a legitimately compiled plan (COMPAT.md).
                    for &c in &a.bare_cols {
                        if c as usize >= base_width {
                            return Err(corrupt("bare column out of the base row"));
                        }
                    }
                    // Aggregate-over-index-tree (format 59): re-prove the whole
                    // admission against the live schema, so a forged/stale plan
                    // fails closed instead of folding the wrong tree. Shape
                    // first (single filterless group over one plain table),
                    // then `agg_servable_by_index` — the same closed rule the
                    // planner chose by.
                    if let Some(ix_no) = a.over_index {
                        if !joins.is_empty()
                            || !matches!(sp.access, AccessPath::FullScan)
                            || filter.is_some()
                            || sp.post_filter.is_some()
                            || !a.group_by.is_empty()
                            || !a.bare_cols.is_empty()
                            || a.aggs.is_empty()
                        {
                            return Err(corrupt(
                                "aggregate-over-index on a shape it cannot serve",
                            ));
                        }
                        if t.kind != mpedb_types::TableKind::Standard {
                            return Err(corrupt("aggregate-over-index on a non-standard table"));
                        }
                        let ix = ix_no
                            .checked_sub(1)
                            .and_then(|k| t.indexes.get(k as usize))
                            .ok_or_else(|| corrupt("aggregate-over-index names no index"))?;
                        if ix_no > 63 || ix.predicate.is_some() {
                            return Err(corrupt(
                                "aggregate-over-index on a partial or out-of-bitmap index",
                            ));
                        }
                        if !super::agg_set_servable_by_index(t, ix, &a.aggs) {
                            return Err(corrupt(
                                "aggregate-over-index with an aggregate set the index cannot serve",
                            ));
                        }
                    }
                    let out_width = a.group_by.len() + a.aggs.len() + a.bare_cols.len();
                    if out_width == 0 {
                        return Err(corrupt("aggregation with no groups and no aggregates"));
                    }
                    if let Some(h) = &a.having {
                        self.check_program_width(h, out_width, ptypes)?;
                    }
                    for p in projection {
                        match p {
                            Projection::Column(i) => {
                                if *i as usize >= out_width {
                                    return Err(corrupt(
                                        "projection column out of the grouped tuple",
                                    ));
                                }
                            }
                            Projection::Expr { program, .. } => {
                                self.check_program_width(program, out_width, ptypes)?
                            }
                        }
                    }
                    let w = order_width(projection.len(), Some(out_width));
                    for (c, _, _) in order_by {
                        if *c as usize >= w {
                            return Err(corrupt("order-by column out of range"));
                        }
                    }
                    return Ok(());
                }
                // Window functions widen the tuple the projection sees: each
                // appends one result column at slot `base_width + k`. The
                // window's OWN sub-programs (arg, PARTITION BY, ORDER BY) read
                // the base row, so they bound by `base_width`; the projection and
                // ORDER BY may reach the window slots, so they bound by
                // `proj_width` — exactly as the aggregate branch widens the
                // projection to the grouped tuple's width.
                let win = &sp.windows;
                if !win.is_empty() {
                    if win.len() > super::MAX_WINDOWS {
                        return Err(corrupt("too many windows in plan"));
                    }
                    for w in win {
                        use super::WindowFunc as WF;
                        if w.distinct {
                            return Err(corrupt("DISTINCT window aggregate is not supported"));
                        }
                        // A ranking/distribution function has no argument (the row
                        // is the input; ntile's bucket count rides in its tag). An
                        // aggregate window MAY carry one (`sum(x)`) or not
                        // (`count(*)`). Every value/offset function REQUIRES its
                        // value `expr`.
                        let is_ranking = matches!(
                            w.func,
                            WF::RowNumber
                                | WF::Rank
                                | WF::DenseRank
                                | WF::Ntile(_)
                                | WF::PercentRank
                                | WF::CumeDist
                        );
                        let is_value = matches!(
                            w.func,
                            WF::Lag(_)
                                | WF::Lead(_)
                                | WF::FirstValue
                                | WF::LastValue
                                | WF::NthValue(_)
                        );
                        if w.arg.is_some() && is_ranking {
                            return Err(corrupt("ranking window function carries an argument"));
                        }
                        if w.arg.is_none() && is_value {
                            return Err(corrupt("value window function requires an argument"));
                        }
                        // Format 55: the host window aggregate's NAME and its
                        // tag must agree in both directions, so a plan can
                        // neither name a host function it does not call nor
                        // call one it does not name.
                        if matches!(w.func, WF::Host) != w.host.is_some() {
                            return Err(corrupt(
                                "window host name present without the host tag (or vice versa)",
                            ));
                        }
                        if matches!(w.func, WF::Host) && w.arg.is_none() {
                            return Err(corrupt("host window aggregate requires an argument"));
                        }
                        if let WF::NthValue(n) = w.func {
                            if n < 1 {
                                return Err(corrupt("nth_value n must be a positive integer"));
                            }
                        }
                        if let WF::Ntile(n) = w.func {
                            if n < 1 {
                                return Err(corrupt("ntile bucket count must be a positive integer"));
                            }
                        }
                        // `default` is a lag/lead-only field (the out-of-range
                        // value); anything else carrying one is malformed.
                        if w.default.is_some() && !matches!(w.func, WF::Lag(_) | WF::Lead(_)) {
                            return Err(corrupt("only lag/lead carry a default expression"));
                        }
                        // Explicit frame legality — the same rule set the planner
                        // and decoder apply, re-checked here on the semantic pass.
                        if let Some(f) = &w.frame {
                            f.check(w.func, !w.order_by.is_empty()).map_err(corrupt)?;
                        }
                        if let Some(a) = &w.arg {
                            self.check_program_width(a, base_width, ptypes)?;
                        }
                        if let Some(d) = &w.default {
                            self.check_program_width(d, base_width, ptypes)?;
                        }
                        for p in &w.partition_by {
                            self.check_program_width(p, base_width, ptypes)?;
                        }
                        for (p, _) in &w.order_by {
                            self.check_program_width(p, base_width, ptypes)?;
                        }
                    }
                }
                let proj_width = base_width + win.len();
                for p in projection {
                    match p {
                        Projection::Column(i) => {
                            if *i as usize >= proj_width {
                                return Err(corrupt("projection column out of range"));
                            }
                        }
                        Projection::Expr { program, .. } => {
                            self.check_program_width(program, proj_width, ptypes)?
                        }
                    }
                }
                // A plain Select has no grouped tuple, so `OrderOver::Grouped`
                // here is itself a malformed plan rather than a width question.
                if *order_over == OrderOver::Grouped {
                    return Err(corrupt("order-over grouped without an aggregate"));
                }
                // A windowed plan sorts the PROJECTION (the window results live
                // there), so its ORDER BY indexes the projection whatever
                // `order_over` claims — bound it that way.
                let w = if win.is_empty() {
                    order_width(projection.len(), None)
                } else {
                    projection.len()
                };
                for (c, _, _) in order_by {
                    if *c as usize >= w {
                        return Err(corrupt("order-by column out of range"));
                    }
                }
            }
        }
        Ok(())
    }

    /// The DML/txn arms of `validate` — split from the SELECT/compound arms
    /// only so `validate_select` can be shared with compound arms.
    fn validate_rest(&self, schema: &Schema) -> Result<()> {
        let ptypes = &self.param_types;
        let get_table = |id: u32| {
            schema
                .table(id)
                .ok_or_else(|| corrupt(format!("table id {id} out of range")))
        };
        match &self.stmt {
            PlanStmt::Select(_)
            | PlanStmt::Compound(_)
            | PlanStmt::RecursiveCte(_)
            | PlanStmt::Derived(_) => {
                unreachable!("handled by validate")
            }
            PlanStmt::Insert {
                table,
                rows,
                from_select,
                with_check,
                on_conflict,
                returning,
            } => {
                let t = get_table(*table)?;
                // A DO UPDATE's SET/WHERE run over [existing ‖ proposed], so
                // their column indices legitimately reach 2n-1. check_program
                // only knows about n, hence the dedicated check.
                match on_conflict {
                    // Replace carries no payload — the executor derives the
                    // unique-index set from the live TableDef — so there is
                    // nothing plan-level to validate.
                    PlanOnConflict::Error
                    | PlanOnConflict::DoNothing
                    | PlanOnConflict::Replace => {}
                    PlanOnConflict::DoUpdate {
                        target,
                        probe,
                        set,
                        filter,
                    } => {
                        if target.is_empty() {
                            return Err(corrupt("ON CONFLICT DO UPDATE with no target"));
                        }
                        for c in target {
                            if *c as usize >= t.columns.len() {
                                return Err(corrupt("conflict-target column out of range"));
                            }
                        }
                        // Recompute the probe from the target and demand a
                        // match. A blob claiming "target (email), probe pk"
                        // would upsert the WRONG ROW — found by pk, reported as
                        // if found by email — which is a silent wrong answer,
                        // not a crash.
                        if *probe != crate::planner::conflict_probe(t, target) {
                            return Err(corrupt("conflict probe does not match the target"));
                        }
                        for (c, p) in set {
                            if *c as usize >= t.columns.len() {
                                return Err(corrupt("ON CONFLICT SET column out of range"));
                            }
                            self.check_doubled_program(p, t, ptypes)?;
                        }
                        if let Some(f) = filter {
                            self.check_doubled_program(f, t, ptypes)?;
                        }
                    }
                }
                if let Some(r) = returning {
                    self.check_projection(r, t, ptypes)?;
                }
                if let Some(sel) = from_select {
                    if !rows.is_empty() {
                        return Err(corrupt("INSERT has both VALUES rows and a SELECT source"));
                    }
                    // The embedded source query is re-validated in full.
                    self.validate_select(&sel.plan, schema, ptypes)?;
                    let width = sel.plan.projection.len();
                    if sel.col_map.len() != t.columns.len() {
                        return Err(corrupt("INSERT … SELECT col_map width mismatch"));
                    }
                    for (ci, m) in sel.col_map.iter().enumerate() {
                        match m {
                            Some(i) => {
                                if *i as usize >= width {
                                    return Err(corrupt("INSERT … SELECT col_map index out of range"));
                                }
                            }
                            None => {
                                let col = &t.columns[ci];
                                // The INTEGER PRIMARY KEY rowid alias auto-assigns
                                // when omitted, so it is exempt from the NOT-NULL rule.
                                if !col.nullable
                                    && col.default.is_none()
                                    && t.rowid_alias_col() != Some(ci as u16)
                                {
                                    return Err(corrupt(
                                        "INSERT … SELECT omits a NOT NULL column without a default",
                                    ));
                                }
                            }
                        }
                    }
                } else if rows.is_empty() {
                    return Err(corrupt("INSERT plan with no rows"));
                }
                if let Some(w) = with_check {
                    self.check_program(w, t, ptypes)?;
                }
                for row in rows {
                    if row.len() != t.columns.len() {
                        return Err(corrupt("INSERT row width mismatch"));
                    }
                    for (ci, src) in row.iter().enumerate() {
                        let col = &t.columns[ci];
                        match src {
                            InsertSource::Expr(prog) => {
                                // Dual-row (width 0): no base columns, only
                                // constants / params / builtins.
                                self.check_program_width(prog, 0, ptypes)?;
                            }
                            InsertSource::Param(i) => {
                                if *i >= self.n_params {
                                    return Err(corrupt("param index out of range"));
                                }
                                if self.param_types[*i as usize] != Some(col.ty) {
                                    return Err(corrupt("insert param type mismatch"));
                                }
                            }
                            InsertSource::Const(i) => {
                                let v = self
                                    .consts
                                    .get(*i as usize)
                                    .ok_or_else(|| corrupt("const index out of range"))?;
                                if !v.fits(col.ty) {
                                    return Err(corrupt("insert const type mismatch"));
                                }
                                if v.is_null() && !col.nullable {
                                    return Err(corrupt("NULL insert into NOT NULL column"));
                                }
                            }
                            InsertSource::Default => {
                                // Default on the rowid-alias PK column is the
                                // auto-assign marker (resolved to max(rowid)+1 at
                                // execution), so it is exempt from the NOT-NULL rule.
                                if !col.nullable
                                    && col.default.is_none()
                                    && t.rowid_alias_col() != Some(ci as u16)
                                {
                                    return Err(corrupt(
                                        "DEFAULT insert into NOT NULL column without default",
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            PlanStmt::Update {
                table,
                access,
                filter,
                set,
                with_check,
                returning,
            } => {
                let t = get_table(*table)?;
                if let Some(r) = returning {
                    self.check_projection(r, t, ptypes)?;
                }
                self.check_access(access, t, None, ptypes)?;
                if let Some(f) = filter {
                    self.check_program(f, t, ptypes)?;
                }
                if let Some(w) = with_check {
                    self.check_program(w, t, ptypes)?;
                }
                if set.is_empty() {
                    return Err(corrupt("UPDATE plan with empty SET"));
                }
                let mut seen = vec![false; t.columns.len()];
                for (c, program) in set {
                    let ci = *c as usize;
                    if ci >= t.columns.len() {
                        return Err(corrupt("SET column out of range"));
                    }
                    if t.is_pk_column(*c) {
                        return Err(corrupt("UPDATE plan sets a primary key column"));
                    }
                    if seen[ci] {
                        return Err(corrupt("duplicate SET column"));
                    }
                    seen[ci] = true;
                    self.check_program(program, t, ptypes)?;
                }
            }
            PlanStmt::Delete {
                table,
                access,
                filter,
                returning,
            } => {
                let t = get_table(*table)?;
                self.check_access(access, t, None, ptypes)?;
                if let Some(r) = returning {
                    self.check_projection(r, t, ptypes)?;
                }
                if let Some(f) = filter {
                    self.check_program(f, t, ptypes)?;
                }
            }
            PlanStmt::Begin
            | PlanStmt::Commit
            | PlanStmt::Rollback
            | PlanStmt::Savepoint(_)
            | PlanStmt::Release(_)
            | PlanStmt::RollbackTo(_) => {}
        }
        Ok(())
    }

    /// The subplan table's own rules (#56). The load-bearing one is the SLOT
    /// DISCIPLINE: a CORRELATED subplan's result slot is filled per outer row
    /// by the executor's post-phase, so a gather-side program (access parts,
    /// filter, join on/policy, joined_filter, aggregate args/HAVING) reading
    /// it would read an unfilled hole. Uncorrelated slots are filled once
    /// before access resolution and are legal everywhere.
    fn validate_subplans(&self, schema: &Schema, budget: &mut usize) -> Result<()> {
        // A SELECT owns an outer ROW that a subplan may correlate to. An
        // UPDATE/DELETE (#97) does not: `exec_stmt_impl` fills its reserved
        // slots ONCE before dispatch and the write path has no per-row fill
        // phase at all, so a correlated slot there would be an unfilled hole.
        // Requiring every top-level subplan uncorrelated makes the whole
        // gather-side slot discipline vacuous — there is no correlated slot to
        // leak into `access`/`filter` — so it is checked here instead of via
        // `check_slot_discipline`, which needs a `SelectPlan`. (An INSERT's
        // subplans belong to its source SELECT, whose param space is merged
        // rather than reserved; that shape stays refused.)
        let outer = match &self.stmt {
            PlanStmt::Select(outer) => Some(outer),
            PlanStmt::Update { .. } | PlanStmt::Delete { .. } => {
                if self.subplans.iter().any(|s| !s.outer_args.is_empty()) {
                    return Err(corrupt("correlated subplan on an UPDATE/DELETE"));
                }
                None
            }
            // A compound's lifts belong to its ARMS (format 56), not to the
            // statement: the statement-level fill happens once, before dispatch,
            // against an outer row a compound does not have. The list must
            // therefore be EMPTY — exactly the rule a derived plan follows.
            PlanStmt::Compound(_) => {
                return Err(corrupt("compound statement with statement-level subplans"));
            }
            _ => return Err(corrupt("subplans on a non-SELECT statement")),
        };
        if self.subplans.len() > MAX_SUBPLANS {
            return Err(corrupt("too many subplans in plan"));
        }
        let n_ctx = self.context_keys.len();
        if self.subplans.len() + n_ctx > self.n_params as usize {
            return Err(corrupt("more reserved slots than parameters"));
        }
        let sub_base = self.subplan_base() as usize;
        let get_table = |id: u32| {
            schema
                .table(id)
                .ok_or_else(|| corrupt(format!("table id {id} out of range")))
        };
        // The outer base row: `[table0 ‖ … ‖ tableN]` types, for outer_args.
        // EMPTY for a write statement — nothing may correlate to it, and the
        // bounds check below turns a forged `outer_arg` into `corrupt`.
        let mut outer_types: Vec<ColumnType> = Vec::new();
        if let Some(outer) = outer {
            for id in std::iter::once(outer.table).chain(outer.joins.iter().map(|j| j.table)) {
                // A FROM-less outer contributes zero columns — nothing can
                // correlate against it (outer_args bounds-check against an
                // empty tuple below, so a forged arg still fails).
                if id == super::DUAL_TABLE {
                    continue;
                }
                outer_types.extend(get_table(id)?.columns.iter().map(|c| c.ty));
            }

            // The gather-side slot discipline (a correlated result slot may not be
            // read by any gather-side program) applies at THIS level's `sub_base` and,
            // recursively, at each nested subplan's own `sub_base` — factored into one
            // helper (`check_slot_discipline`) so the top and nested levels (#73 §3
            // stage 2) cannot drift.
            self.check_slot_discipline(outer, sub_base, &self.subplans)?;
        }

        // Each top subplan (and, recursively, its own nested lifts — #73 §3) is
        // validated against ITS level's parameter space and outer row. The
        // budget bounds the whole tree at `MAX_SUBPLANS`, matching the decoder.
        let user_ptypes = &self.param_types[..sub_base];
        for s in &self.subplans {
            self.validate_subplan_rec(schema, s, user_ptypes, &outer_types, budget)?;
        }
        Ok(())
    }

    /// The base-row column types of one SELECT (`[table0 ‖ … ‖ tableN]`), used
    /// as the OUTER row a subplan's `outer_args` index into. FROM-less (DUAL)
    /// tables contribute nothing.
    fn select_row_types(&self, sp: &SelectPlan, schema: &Schema) -> Result<Vec<ColumnType>> {
        let mut types = Vec::new();
        for id in std::iter::once(sp.table).chain(sp.joins.iter().map(|j| j.table)) {
            if id == super::DUAL_TABLE {
                continue;
            }
            let t = schema
                .table(id)
                .ok_or_else(|| corrupt(format!("table id {id} out of range")))?;
            types.extend(t.columns.iter().map(|c| c.ty));
        }
        Ok(types)
    }

    /// The gather-side SLOT DISCIPLINE for one level of subplans. A CORRELATED
    /// result slot (at `base + i`, for a subplan with non-empty `outer_args`) is
    /// filled PER ROW after the gather, so no gather-side program of `sp` — the
    /// access parts, `filter`, `joined_filter`, a join's `on`/`policy`, or (for
    /// an aggregate, #73 §1.2c) the group keys / aggregate args / HAVING / grouped
    /// projection — may read it. `post_filter`, an aggregate's `FILTER (WHERE …)`
    /// (evaluated per row inside the aggregate loop against that row's filled
    /// scratch) and a non-aggregate projection are the only readers of a
    /// correlated slot. Applied at the top level (`base =
    /// subplan_base`) and, by `validate_subplan_rec`, at each nested subplan's own
    /// `base = sub_base`, so every level (#73 §3 stage 2) is checked identically.
    fn check_slot_discipline(
        &self,
        sp: &SelectPlan,
        base: usize,
        subplans: &[SubPlan],
    ) -> Result<()> {
        let correlated: Vec<bool> = subplans.iter().map(|s| !s.outer_args.is_empty()).collect();
        self.check_correlated_slots(sp, base, &correlated)
    }

    /// The same rule, expressed over the correlation FLAGS of a contiguous
    /// reserved region rather than over one owner's list — so a compound can
    /// check every arm against the WHOLE arm-lift region (a slot owned by
    /// another arm is just as unfilled). The single implementation both entry
    /// points share: no level can get a weaker check by owning its lifts.
    fn check_correlated_slots(
        &self,
        sp: &SelectPlan,
        base: usize,
        correlated: &[bool],
    ) -> Result<()> {
        let n = correlated.len();
        let gather_ok = |p: &ExprProgram| -> Result<()> {
            for i in &p.instrs {
                if let Instr::PushParam(pi) | Instr::InParam(pi) = *i {
                    let pi = pi as usize;
                    if (base..base + n).contains(&pi) && correlated[pi - base] {
                        return Err(corrupt(
                            "gather-side program reads a correlated subplan slot",
                        ));
                    }
                }
            }
            Ok(())
        };
        let key_parts_ok = |a: &AccessPath| -> Result<()> {
            let mut check = |p: &KeyPart| -> Result<()> {
                if let KeyPart::Param(i) = p {
                    let i = *i as usize;
                    if (base..base + n).contains(&i) && correlated[i - base] {
                        return Err(corrupt("access path reads a correlated subplan slot"));
                    }
                }
                Ok(())
            };
            match a {
                AccessPath::FullScan => Ok(()),
                AccessPath::PkPoint(parts) => parts.iter().try_for_each(&mut check),
                AccessPath::PkRange { lo, hi } => [lo, hi]
                    .into_iter()
                    .flatten()
                    .flat_map(|b| b.parts.iter())
                    .try_for_each(&mut check),
                AccessPath::IndexPoint { parts, .. } => parts.iter().try_for_each(&mut check),
                AccessPath::IndexRange { lo, hi, .. } => [lo, hi]
                    .into_iter()
                    .flatten()
                    .flat_map(|b| b.parts.iter())
                    .try_for_each(&mut check),
                // An FtsScan carries a literal query tree, no key parts, so it
                // can never read a correlated subplan slot.
                AccessPath::FtsScan { .. } => Ok(()),
            }
        };
        key_parts_ok(&sp.access)?;
        if let Some(f) = &sp.filter {
            gather_ok(f)?;
        }
        if let Some(f) = &sp.joined_filter {
            gather_ok(f)?;
        }
        for j in &sp.joins {
            key_parts_ok(&j.access)?;
            gather_ok(&j.on)?;
            if let Some(p) = &j.policy {
                gather_ok(p)?;
            }
        }
        if let Some(_agg) = &sp.aggregate {
            // NOT `agg.group_by`, NOT `a.arg`, NOT `a.filter` (#97). All three
            // are evaluated PER ROW inside `exec_aggregate`'s row loop, after
            // the per-row correlated fill, against THAT row's scratch parameter
            // vector — exactly like `post_filter`, and unlike everything else
            // here. So a correlated slot IS meaningful in each and must not be
            // rejected. (Rejecting `a.filter` was not merely conservative: the
            // executor used to evaluate the filter against the pre-fill
            // `params`, read NULL, and DROP the row — a wrong answer for both
            // `EXISTS` and `NOT EXISTS`.)
            //
            // HAVING and the grouped PROJECTION also read correlated slots now:
            // the executor keeps the first base-row param scratch per group and
            // evaluates those programs against it (Django OuterRef on a group
            // key; matches sqlite bare-column pick). No gather-side refusal.
            let _ = _agg;
        }
        Ok(())
    }

    /// Validate one subplan and, recursively, its nested lifts (#73 §3 stage 2).
    ///
    /// `base_ptypes` are the parameter types of the slots BELOW this subplan's
    /// reserved region — i.e. `[user ‖ … ‖ this subplan's-parent correlation]`,
    /// of width `parent.sub_base`. `parent_outer_types` is the enclosing plan's
    /// base row, which this subplan's `outer_args` index into. The subplan's own
    /// inner parameter space is then `base ‖ its correlation ‖ its children`, and
    /// a nested child inherits `base ‖ its correlation` as ITS base.
    fn validate_subplan_rec(
        &self,
        schema: &Schema,
        s: &SubPlan,
        base_ptypes: &[Option<ColumnType>],
        parent_outer_types: &[ColumnType],
        budget: &mut usize,
    ) -> Result<()> {
        if *budget == 0 {
            return Err(corrupt("too many subplans in plan"));
        }
        *budget -= 1;

        // `sub_base` locates the children slots: the level's param prefix, plus
        // — for a subplan lifted from a LATER compound arm (format 49) — the
        // reserved-slot GAP of the preceding arms' subplans (those slots exist
        // in the statement layout but are invisible to this arm's inner, which
        // never reads them; the executor pads them NULL), plus this subplan's
        // own correlation args. A prefix below the level's width would make the
        // executor fill children over live slots; a gap above `MAX_SUBPLANS`
        // has no honest producer and would only inflate the pad allocation.
        let prefix = (s.sub_base as usize)
            .checked_sub(s.outer_args.len())
            .ok_or_else(|| corrupt("subplan sub_base inconsistent with its correlation args"))?;
        if prefix < base_ptypes.len() || prefix - base_ptypes.len() > MAX_SUBPLANS {
            return Err(corrupt("subplan sub_base inconsistent with its correlation args"));
        }
        for &a in &s.outer_args {
            if a as usize >= parent_outer_types.len() {
                return Err(corrupt("subplan correlation arg out of the outer row"));
            }
        }
        // (#97) A CORRELATED `List` subplan used to be `corrupt` here, mirroring
        // the planner's "rewrite as EXISTS" refusal. Both are gone: the kind
        // changes only what `subplan_value` reduces the rows to, and the per-row
        // fill is kind-agnostic. The rule that actually protects the slot is
        // `check_slot_discipline`, which already counts `Instr::InParam` as a
        // read — so a correlated List slot reaching a GATHER-side program is
        // still rejected, while `post_filter` (its one legal reader) is not.
        // A scalar subquery IS one value; EXISTS ignores its projection.
        if s.kind != SubPlanKind::Exists && s.body.output_arity() != 1 {
            return Err(corrupt("scalar subplan must output exactly one column"));
        }
        // The inner parameter space: base ‖ (compound-arm gap, untyped — the
        // inner never reads those slots and the executor pads them NULL) ‖ this
        // subplan's correlation ‖ its children results. A correlation slot has
        // the OUTER column's type; a child result slot carries the child's
        // declared `slot_type` (so a child used as a key part still
        // type-checks).
        let mut inner_types: Vec<Option<ColumnType>> = base_ptypes.to_vec();
        inner_types.resize(prefix, None);
        inner_types.extend(s.outer_args.iter().map(|&a| Some(parent_outer_types[a as usize])));
        // (`inner_types.len()` is now `s.sub_base`.)
        for c in &s.subplans {
            inner_types.push(c.slot_type);
        }
        match &s.body {
            // A compound body (format 31) is always UNCORRELATED and carries no
            // nested lifts — the planner produces it only for an uncorrelated
            // `IN`/scalar/`EXISTS`, and a subquery inside a compound arm is still
            // refused. A forged one with correlation args or children would let a
            // gather-side slot discipline go unchecked, so refuse both.
            SubBody::Compound(c) => {
                // A compound body's lifts belong to its ARMS (format 56), never
                // to this subplan: `SubPlan::subplans` are filled per row of a
                // SELECT body, and a compound has no such phase. Correlation,
                // on the other hand, IS this subplan's: the region is filled
                // once per outer row BEFORE the compound runs, so every arm
                // reads it as an ordinary parameter.
                if !s.subplans.is_empty() {
                    return Err(corrupt("compound subplan with nested lifts"));
                }
                self.validate_compound(c, schema, &inner_types, s.sub_base as usize, budget)?;
            }
            SubBody::Select(sp) => {
                // A `post_filter` is applied per row only when this subplan HAS
                // children (the executor's leaf path runs the plain `exec_select`,
                // which ignores it) — so a post-filter with no nested subplans
                // would be silently dropped. Refuse it, mirroring the top-level
                // "post-filter without subplans" rule. WITH children (#73 §3 stage
                // 2), the post-filter rides the per-row fill of the correlated
                // ones; the gather-side discipline for its slots is enforced by
                // `check_slot_discipline` below.
                if sp.post_filter.is_some() && s.subplans.is_empty() {
                    return Err(corrupt("subplan post-filter without nested subplans"));
                }
                // #73 §3: a nested lift MAY correlate to its IMMEDIATE parent
                // (stage 2) or, via a TRANSIT arg carried by an intervening level,
                // to a MIDDLE or OUTER scope (stage 3). Either way its `outer_args`
                // name slots of THIS subplan's parent row and are bounds-checked
                // against `sp`'s row in the recursion below.
                self.validate_select(sp, schema, &inner_types)?;
                // This subplan's OWN children live at `s.sub_base + i`: enforce the
                // same gather-side slot discipline one level down.
                self.check_slot_discipline(sp, s.sub_base as usize, &s.subplans)?;

                // Recurse: a nested child's base prefix is `[user ‖ … ‖ this
                // correlation]` (width `s.sub_base`), and its outer row is sp's row.
                let child_base = &inner_types[..s.sub_base as usize];
                let child_outer = self.select_row_types(sp, schema)?;
                for c in &s.subplans {
                    self.validate_subplan_rec(schema, c, child_base, &child_outer, budget)?;
                }
            }
        }
        Ok(())
    }

    fn check_program(
        &self,
        p: &ExprProgram,
        t: &TableDef,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        // Stack discipline and const-pool indices were proven by
        // ExprProgram::new/decode; column and parameter indices are ours.
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= t.columns.len() => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi as usize >= ptypes.len() => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Validate a `RETURNING` projection: column indices in range, and any
    /// expression's own indices too.
    fn check_projection(
        &self,
        proj: &[Projection],
        t: &TableDef,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        for p in proj {
            match p {
                Projection::Column(i) => {
                    if *i as usize >= t.columns.len() {
                        return Err(corrupt("RETURNING column out of range"));
                    }
                }
                Projection::Expr { program, .. } => self.check_program(program, t, ptypes)?,
            }
        }
        Ok(())
    }

    /// Bound a program's column indices by an arbitrary tuple width — for the
    /// GROUPED tuple `[keys ‖ aggs]`, which is not a table's row.
    fn check_program_width(
        &self,
        p: &ExprProgram,
        width: usize,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= width => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi as usize >= ptypes.len() => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A `DO UPDATE` SET/WHERE program runs over the EXISTING row concatenated
    /// with the PROPOSED one, so `Col(n + i)` is `excluded.<col i>` and is
    /// legal. `check_program` would reject those as out of range, so the bound
    /// here is 2n — but still a bound: a hostile plan must not read past the
    /// doubled row either.
    fn check_doubled_program(
        &self,
        p: &ExprProgram,
        t: &TableDef,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        let limit = t.columns.len() * 2;
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= limit => {
                    return Err(corrupt("ON CONFLICT expression column out of range"));
                }
                Instr::PushParam(pi) if pi as usize >= ptypes.len() => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A key part must reference a valid param/const, and a const must be a
    /// non-NULL value of the key column's exact type. `outer` is the
    /// accumulated outer tuple's column types when the part sits inside a
    /// join's access path — the only place `OuterCol` is legal; a
    /// statement-level path (outer = None) carrying one is corrupt.
    fn check_key_part(
        &self,
        p: &KeyPart,
        ty: ColumnType,
        outer: Option<&[ColumnType]>,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        match p {
            KeyPart::Param(i) => {
                let Some(pt) = ptypes.get(*i as usize) else {
                    return Err(corrupt("key param out of range"));
                };
                // None is allowed: inequality ClassCmp leaves the param free so
                // a float bind against an int key still compares (Numeric
                // affinity at residual time). A pinned type must still match.
                if let Some(t) = pt {
                    if *t != ty {
                        return Err(corrupt("key param type mismatch"));
                    }
                }
            }
            KeyPart::Const(i) => {
                let v = self
                    .consts
                    .get(*i as usize)
                    .ok_or_else(|| corrupt("key const out of range"))?;
                if v.is_null() || !v.fits(ty) {
                    return Err(corrupt("key const type mismatch"));
                }
            }
            KeyPart::OuterCol(i) => {
                let Some(cols) = outer else {
                    return Err(corrupt("outer-column key part outside a join"));
                };
                let Some(&oty) = cols.get(*i as usize) else {
                    return Err(corrupt("outer-column key part out of range"));
                };
                if oty != ty {
                    return Err(corrupt("outer-column key part type mismatch"));
                }
            }
        }
        Ok(())
    }

    fn check_access(
        &self,
        a: &AccessPath,
        t: &TableDef,
        outer: Option<&[ColumnType]>,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        match a {
            AccessPath::FullScan => Ok(()),
            AccessPath::PkPoint(parts) => {
                if parts.len() != t.primary_key.len() {
                    return Err(corrupt("PkPoint part count != PK column count"));
                }
                for (part, &pk_col) in parts.iter().zip(&t.primary_key) {
                    self.check_key_part(part, t.columns[pk_col as usize].ty, outer, ptypes)?;
                }
                Ok(())
            }
            AccessPath::PkRange { lo, hi } => {
                if lo.is_none() && hi.is_none() {
                    return Err(corrupt("PkRange with no bounds"));
                }
                let first_ty = t.columns[t.primary_key[0] as usize].ty;
                for bound in [lo, hi].into_iter().flatten() {
                    if bound.parts.len() != 1 {
                        return Err(corrupt("Phase 1 PkRange bound must have exactly one part"));
                    }
                    self.check_key_part(&bound.parts[0], first_ty, outer, ptypes)?;
                }
                Ok(())
            }
            AccessPath::IndexPoint { index_no, parts } => {
                let no = *index_no as usize;
                if no == 0 || no > t.indexes.len() || no > 63 {
                    return Err(corrupt("index_no out of range"));
                }
                let ix = &t.indexes[no - 1];
                // Parts cover a non-empty PREFIX of the index's columns, in
                // key order, each typed as its column (#55).
                if parts.is_empty() || parts.len() > ix.columns.len() {
                    return Err(corrupt("IndexPoint parts do not fit the index"));
                }
                for (part, &col) in parts.iter().zip(&ix.columns) {
                    self.check_key_part(part, t.columns[col as usize].ty, outer, ptypes)?;
                }
                Ok(())
            }
            AccessPath::IndexRange { index_no, lo, hi } => {
                let no = *index_no as usize;
                if no == 0 || no > t.indexes.len() || no > 63 {
                    return Err(corrupt("index_no out of range"));
                }
                if lo.is_none() && hi.is_none() {
                    return Err(corrupt("IndexRange with no bounds"));
                }
                // Phase-1 rule, same as PkRange: bounds over the FIRST index
                // column only — valid for composite unchanged (the first
                // column's encoding is a key prefix).
                let col = t.indexes[no - 1].columns[0];
                let ty = t.columns[col as usize].ty;
                for bound in [lo, hi].into_iter().flatten() {
                    if bound.parts.len() != 1 {
                        return Err(corrupt("IndexRange bound must have exactly one part"));
                    }
                    self.check_key_part(&bound.parts[0], ty, outer, ptypes)?;
                }
                Ok(())
            }
            AccessPath::FtsScan { query } => {
                // An FtsScan is legal ONLY against an FTS table (a forged plan
                // pointing it at an ordinary table would have the executor probe
                // a nonexistent inverted-index tree). Every term's column ordinal
                // must be a real FTS content column.
                if !t.kind.is_fts() {
                    return Err(corrupt("FtsScan access on a non-FTS table"));
                }
                let ncols = t.fts_content_columns().len() as u16;
                validate_fts_query(query, ncols)?;
                Ok(())
            }
        }
    }
}

/// Recursively check a compiled FTS query: non-empty terms, in-range column
/// ordinals. Depth is bounded by the decoder ([`MAX_FTS_DEPTH`]).
fn validate_fts_query(q: &FtsQuery, ncols: u16) -> Result<()> {
    match q {
        FtsQuery::Term(t) => {
            if t.token.is_empty() {
                return Err(corrupt("empty FTS term"));
            }
            for &c in &t.columns {
                if c >= ncols {
                    return Err(corrupt("FTS term column ordinal out of range"));
                }
            }
            Ok(())
        }
        FtsQuery::And(a, b) | FtsQuery::Or(a, b) | FtsQuery::AndNot(a, b) => {
            validate_fts_query(a, ncols)?;
            validate_fts_query(b, ncols)
        }
    }
}
