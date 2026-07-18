use super::*;
use super::decode::corrupt;

impl CompiledPlan {
    /// Semantic re-validation against the schema: index/column/parameter
    /// bounds, PK shapes, typed constants, and footprint consistency
    /// (recomputed from scratch and compared, so a forged footprint in an
    /// otherwise well-formed blob is rejected).
    pub(crate) fn validate(&self, schema: &Schema) -> Result<()> {
        let ptypes = &self.param_types;
        match &self.stmt {
            PlanStmt::Select(sp) => self.validate_select(sp, schema, ptypes)?,
            PlanStmt::Compound(c) => {
                if !(2..=MAX_COMPOUND_ARMS).contains(&c.arms.len()) {
                    return Err(corrupt("compound arm count out of range"));
                }
                if c.ops.len() != c.arms.len() - 1 {
                    return Err(corrupt("compound op count does not match arm count"));
                }
                let arity = c.arms[0].projection.len();
                for arm in &c.arms {
                    // The compound owns ORDER BY/LIMIT — SQL cannot express
                    // them per arm, so an arm carrying its own is forged. And
                    // with no junk, `projection.len()` IS the output arity,
                    // which the set ops and the compound sort both index.
                    if !arm.order_by.is_empty()
                        || arm.order_junk != 0
                        || arm.limit.is_some()
                        || arm.offset.is_some()
                    {
                        return Err(corrupt("compound arm carries its own ORDER BY/LIMIT"));
                    }
                    // Arms run through the plain executor, which never fills
                    // correlated slots — a post-filter there would be
                    // silently ignored, so its presence is forgery.
                    if arm.post_filter.is_some() {
                        return Err(corrupt("compound arm carries a post-filter"));
                    }
                    if arm.projection.len() != arity {
                        return Err(corrupt("compound arms disagree on output arity"));
                    }
                    self.validate_select(arm, schema, ptypes)?;
                }
                for (i, _) in &c.order_by {
                    if *i as usize >= arity {
                        return Err(corrupt("compound order-by column out of range"));
                    }
                }
            }
            _other => self.validate_rest(schema)?,
        }
        if !self.subplans.is_empty() {
            self.validate_subplans(schema)?;
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

    /// Everything `validate` checks about one SELECT — shared verbatim between
    /// a top-level SELECT and each compound arm, so the two can never drift.
    fn validate_select(
        &self,
        sp: &SelectPlan,
        schema: &Schema,
        ptypes: &[Option<ColumnType>],
    ) -> Result<()> {
        let get_table = |id: u32| {
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
                    // FULL needs the inner side enumerated and held: single
                    // join, FullScan access — the executor's unmatched-inner
                    // sweep is built on exactly that.
                    if j.kind == JoinKind::Full {
                        if joins.len() != 1 {
                            return Err(corrupt("FULL join in a multi-join chain"));
                        }
                        if !matches!(j.access, AccessPath::FullScan) {
                            return Err(corrupt("FULL join with a keyed inner access"));
                        }
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
                    }
                    let out_width = a.group_by.len() + a.aggs.len();
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
                    for (c, _) in order_by {
                        if *c as usize >= w {
                            return Err(corrupt("order-by column out of range"));
                        }
                    }
                    return Ok(());
                }
                for p in projection {
                    match p {
                        Projection::Column(i) => {
                            if *i as usize >= base_width {
                                return Err(corrupt("projection column out of range"));
                            }
                        }
                        Projection::Expr { program, .. } => {
                            self.check_program_width(program, base_width, ptypes)?
                        }
                    }
                }
                // A plain Select has no grouped tuple, so `OrderOver::Grouped`
                // here is itself a malformed plan rather than a width question.
                if *order_over == OrderOver::Grouped {
                    return Err(corrupt("order-over grouped without an aggregate"));
                }
                let w = order_width(projection.len(), None);
                for (c, _) in order_by {
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
            PlanStmt::Select(_) | PlanStmt::Compound(_) => {
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
                    PlanOnConflict::Error | PlanOnConflict::DoNothing => {}
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
                                if !col.nullable && col.default.is_none() {
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
                                if !col.nullable && col.default.is_none() {
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
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {}
        }
        Ok(())
    }

    /// The subplan table's own rules (#56). The load-bearing one is the SLOT
    /// DISCIPLINE: a CORRELATED subplan's result slot is filled per outer row
    /// by the executor's post-phase, so a gather-side program (access parts,
    /// filter, join on/policy, joined_filter, aggregate args/HAVING) reading
    /// it would read an unfilled hole. Uncorrelated slots are filled once
    /// before access resolution and are legal everywhere.
    fn validate_subplans(&self, schema: &Schema) -> Result<()> {
        let PlanStmt::Select(outer) = &self.stmt else {
            return Err(corrupt("subplans on a non-SELECT statement"));
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
        let mut outer_types: Vec<ColumnType> = Vec::new();
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

        // Each top subplan (and, recursively, its own nested lifts — #73 §3) is
        // validated against ITS level's parameter space and outer row. The
        // budget bounds the whole tree at `MAX_SUBPLANS`, matching the decoder.
        let user_ptypes = &self.param_types[..sub_base];
        let mut budget = MAX_SUBPLANS;
        for s in &self.subplans {
            self.validate_subplan_rec(schema, s, user_ptypes, &outer_types, &mut budget)?;
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
    /// projection — may read it. `post_filter` and a non-aggregate projection are
    /// the only readers of a correlated slot. Applied at the top level (`base =
    /// subplan_base`) and, by `validate_subplan_rec`, at each nested subplan's own
    /// `base = sub_base`, so every level (#73 §3 stage 2) is checked identically.
    fn check_slot_discipline(
        &self,
        sp: &SelectPlan,
        base: usize,
        subplans: &[SubPlan],
    ) -> Result<()> {
        let correlated: Vec<bool> = subplans.iter().map(|s| !s.outer_args.is_empty()).collect();
        let n = subplans.len();
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
        if let Some(agg) = &sp.aggregate {
            for k in &agg.group_by {
                if let GroupKey::Expr(p) = k {
                    gather_ok(p)?;
                }
            }
            for a in &agg.aggs {
                if let Some(p) = &a.arg {
                    gather_ok(p)?;
                }
            }
            if let Some(h) = &agg.having {
                gather_ok(h)?;
            }
            for p in &sp.projection {
                if let Projection::Expr { program, .. } = p {
                    gather_ok(program)?;
                }
            }
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

        // `sub_base` locates the children slots; it must be exactly the level's
        // param prefix plus this subplan's own correlation args, or the executor
        // would fill children into the wrong indices.
        if s.sub_base as usize != base_ptypes.len() + s.outer_args.len() {
            return Err(corrupt("subplan sub_base inconsistent with its correlation args"));
        }
        for &a in &s.outer_args {
            if a as usize >= parent_outer_types.len() {
                return Err(corrupt("subplan correlation arg out of the outer row"));
            }
        }
        // A scalar subquery IS one value; EXISTS ignores its projection.
        if s.kind == SubPlanKind::List && !s.outer_args.is_empty() {
            return Err(corrupt("correlated IN-list subplan"));
        }
        if s.kind != SubPlanKind::Exists
            && s.plan.projection.len() - s.plan.order_junk as usize != 1
        {
            return Err(corrupt("scalar subplan must output exactly one column"));
        }
        // A `post_filter` is applied per row only when this subplan HAS children
        // (the executor's leaf path runs the plain `exec_select`, which ignores
        // it) — so a post-filter with no nested subplans would be silently
        // dropped. Refuse it, mirroring the top-level "post-filter without
        // subplans" rule. WITH children (#73 §3 stage 2), the post-filter rides
        // the per-row fill of the correlated ones; the gather-side discipline for
        // its slots is enforced by `check_slot_discipline` below.
        if s.plan.post_filter.is_some() && s.subplans.is_empty() {
            return Err(corrupt("subplan post-filter without nested subplans"));
        }
        // #73 §3 stage 2: a nested lift MAY correlate to its IMMEDIATE parent —
        // this subplan's row — which the executor fills per parent row. Its
        // `outer_args` are bounds-checked against `s.plan`'s row in the recursion
        // below; correlation to a MIDDLE/OUTER scope is unrepresentable (an
        // `outer_arg` names only the immediate parent), so nothing here can encode
        // it. (Stage 1's blanket refusal of a correlated child is retired.)
        // The inner parameter space: base ‖ this subplan's correlation ‖ its
        // children results. A correlation slot has the OUTER column's type; a
        // child result slot carries the child's declared `slot_type` (so a child
        // used as a key part still type-checks).
        let mut inner_types: Vec<Option<ColumnType>> = base_ptypes.to_vec();
        inner_types.extend(s.outer_args.iter().map(|&a| Some(parent_outer_types[a as usize])));
        // (`inner_types.len()` is now `s.sub_base`.)
        for c in &s.subplans {
            inner_types.push(c.slot_type);
        }
        self.validate_select(&s.plan, schema, &inner_types)?;
        // This subplan's OWN children live at `s.sub_base + i`: enforce the same
        // gather-side slot discipline one level down.
        self.check_slot_discipline(&s.plan, s.sub_base as usize, &s.subplans)?;

        // Recurse: a nested child's base prefix is `[user ‖ … ‖ this
        // correlation]` (width `s.sub_base`), and its outer row is s.plan's row.
        let child_base = &inner_types[..s.sub_base as usize];
        let child_outer = self.select_row_types(&s.plan, schema)?;
        for c in &s.subplans {
            self.validate_subplan_rec(schema, c, child_base, &child_outer, budget)?;
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
                if *pt != Some(ty) {
                    return Err(corrupt("key param type mismatch"));
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
        }
    }
}
