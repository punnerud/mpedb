use super::*;
use super::decode::corrupt;

impl CompiledPlan {
    /// Semantic re-validation against the schema: index/column/parameter
    /// bounds, PK shapes, typed constants, and footprint consistency
    /// (recomputed from scratch and compared, so a forged footprint in an
    /// otherwise well-formed blob is rejected).
    pub(crate) fn validate(&self, schema: &Schema) -> Result<()> {
        match &self.stmt {
            PlanStmt::Select(sp) => self.validate_select(sp, schema)?,
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
                    if arm.projection.len() != arity {
                        return Err(corrupt("compound arms disagree on output arity"));
                    }
                    self.validate_select(arm, schema)?;
                }
                for (i, _) in &c.order_by {
                    if *i as usize >= arity {
                        return Err(corrupt("compound order-by column out of range"));
                    }
                }
            }
            _other => self.validate_rest(schema)?,
        }
        // Footprint consistency for EVERY statement kind: recomputed from
        // scratch and compared, so a forged footprint in an otherwise
        // well-formed blob is rejected.
        let recomputed = planner::compute_footprint(&self.stmt, schema)?;
        if recomputed != self.footprint {
            return Err(corrupt("plan footprint does not match its statement"));
        }
        Ok(())
    }

    /// Everything `validate` checks about one SELECT — shared verbatim between
    /// a top-level SELECT and each compound arm, so the two can never drift.
    fn validate_select(&self, sp: &SelectPlan, schema: &Schema) -> Result<()> {
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
                let t = get_table(*table)?;
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
                self.check_access(access, t, None)?;
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
                    self.check_access(&j.access, jt, Some(&acc_types))?;
                    if let Some(p) = &j.policy {
                        self.check_program(p, jt)?;
                    }
                    acc_width += jt.columns.len();
                    acc_types.extend(jt.columns.iter().map(|c| c.ty));
                    self.check_program_width(&j.on, acc_width)?;
                }
                let base_width = acc_width; // the full joined row
                if let Some(jf) = joined_filter {
                    if joins.is_empty() {
                        return Err(corrupt("joined filter without a join"));
                    }
                    self.check_program_width(jf, base_width)?;
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
                    self.check_program(f, t)?;
                }
                if let Some(a) = aggregate {
                    // GROUP BY columns and aggregate ARGUMENTS index the BASE
                    // row — which for a join is the JOINED row, hence
                    // `base_width` and not the outer table's; HAVING and the
                    // projection index the GROUPED tuple `[keys ‖ aggs]`, which
                    // is a different width again. Checking either against the
                    // wrong one would let a hostile plan read past its row — so
                    // they are bounded separately.
                    for c in &a.group_by {
                        if *c as usize >= base_width {
                            return Err(corrupt("GROUP BY column out of range"));
                        }
                    }
                    for c in &a.aggs {
                        if let Some(p) = &c.arg {
                            self.check_program_width(p, base_width)?;
                        }
                    }
                    let out_width = a.group_by.len() + a.aggs.len();
                    if out_width == 0 {
                        return Err(corrupt("aggregation with no groups and no aggregates"));
                    }
                    if let Some(h) = &a.having {
                        self.check_program_width(h, out_width)?;
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
                                self.check_program_width(program, out_width)?
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
                            self.check_program_width(program, base_width)?
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
                            self.check_doubled_program(p, t)?;
                        }
                        if let Some(f) = filter {
                            self.check_doubled_program(f, t)?;
                        }
                    }
                }
                if let Some(r) = returning {
                    self.check_projection(r, t)?;
                }
                if rows.is_empty() {
                    return Err(corrupt("INSERT plan with no rows"));
                }
                if let Some(w) = with_check {
                    self.check_program(w, t)?;
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
                    self.check_projection(r, t)?;
                }
                self.check_access(access, t, None)?;
                if let Some(f) = filter {
                    self.check_program(f, t)?;
                }
                if let Some(w) = with_check {
                    self.check_program(w, t)?;
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
                    self.check_program(program, t)?;
                }
            }
            PlanStmt::Delete {
                table,
                access,
                filter,
                returning,
            } => {
                let t = get_table(*table)?;
                self.check_access(access, t, None)?;
                if let Some(r) = returning {
                    self.check_projection(r, t)?;
                }
                if let Some(f) = filter {
                    self.check_program(f, t)?;
                }
            }
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {}
        }
        Ok(())
    }

    fn check_program(&self, p: &ExprProgram, t: &TableDef) -> Result<()> {
        // Stack discipline and const-pool indices were proven by
        // ExprProgram::new/decode; column and parameter indices are ours.
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= t.columns.len() => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Validate a `RETURNING` projection: column indices in range, and any
    /// expression's own indices too.
    fn check_projection(&self, proj: &[Projection], t: &TableDef) -> Result<()> {
        for p in proj {
            match p {
                Projection::Column(i) => {
                    if *i as usize >= t.columns.len() {
                        return Err(corrupt("RETURNING column out of range"));
                    }
                }
                Projection::Expr { program, .. } => self.check_program(program, t)?,
            }
        }
        Ok(())
    }

    /// Bound a program's column indices by an arbitrary tuple width — for the
    /// GROUPED tuple `[keys ‖ aggs]`, which is not a table's row.
    fn check_program_width(&self, p: &ExprProgram, width: usize) -> Result<()> {
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= width => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
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
    fn check_doubled_program(&self, p: &ExprProgram, t: &TableDef) -> Result<()> {
        let limit = t.columns.len() * 2;
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= limit => {
                    return Err(corrupt("ON CONFLICT expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
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
    ) -> Result<()> {
        match p {
            KeyPart::Param(i) => {
                if *i >= self.n_params {
                    return Err(corrupt("key param out of range"));
                }
                if self.param_types[*i as usize] != Some(ty) {
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
    ) -> Result<()> {
        match a {
            AccessPath::FullScan => Ok(()),
            AccessPath::PkPoint(parts) => {
                if parts.len() != t.primary_key.len() {
                    return Err(corrupt("PkPoint part count != PK column count"));
                }
                for (part, &pk_col) in parts.iter().zip(&t.primary_key) {
                    self.check_key_part(part, t.columns[pk_col as usize].ty, outer)?;
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
                    self.check_key_part(&bound.parts[0], first_ty, outer)?;
                }
                Ok(())
            }
            AccessPath::IndexPoint { index_no, part } => {
                let sec = crate::planner::secondary_indexes(t);
                let no = *index_no as usize;
                if no == 0 || no > sec.len() || no > 63 {
                    return Err(corrupt("index_no out of range"));
                }
                let col = sec[no - 1];
                self.check_key_part(part, t.columns[col as usize].ty, outer)
            }
            AccessPath::IndexRange { index_no, lo, hi } => {
                let sec = crate::planner::secondary_indexes(t);
                let no = *index_no as usize;
                if no == 0 || no > sec.len() || no > 63 {
                    return Err(corrupt("index_no out of range"));
                }
                if lo.is_none() && hi.is_none() {
                    return Err(corrupt("IndexRange with no bounds"));
                }
                let ty = t.columns[sec[no - 1] as usize].ty;
                for bound in [lo, hi].into_iter().flatten() {
                    if bound.parts.len() != 1 {
                        return Err(corrupt("IndexRange bound must have exactly one part"));
                    }
                    self.check_key_part(&bound.parts[0], ty, outer)?;
                }
                Ok(())
            }
        }
    }
}
