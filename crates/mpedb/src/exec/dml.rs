use super::*;

pub(super) fn exec_stmt_rest(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &WriteRules,
    depth: u32,
) -> Result<ExecResult> {
    match &plan.stmt {
        PlanStmt::Select(_)
        | PlanStmt::Compound(_)
        | PlanStmt::RecursiveCte(_)
        | PlanStmt::Derived(_) => {
            unreachable!("handled by exec_stmt_impl")
        }
        PlanStmt::Insert {
            table,
            rows,
            from_select,
            with_check,
            on_conflict,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Bind-time `now()`: captured exactly once per execute() call so
            // every DEFAULT now() in a multi-row INSERT gets the same value
            // (reviewed determinism requirement).
            let now = now_micros();
            // Materialize the rows to insert. INSERT … SELECT reads its source
            // FULLY first (so `INSERT INTO t SELECT … FROM t` reads the
            // pre-insert state — sqlite's semantics), then inserts; each source
            // tuple maps to the target columns via `col_map`, omitted columns
            // taking their DEFAULT / NULL.
            let built_rows: Vec<std::borrow::Cow<[Value]>> = if let Some(sel) = from_select {
                let src = match exec_select(ctx, schema, plan, params, &sel.plan)? {
                    ExecResult::Rows { rows, .. } => rows,
                    _ => return Err(internal("INSERT … SELECT source produced no row set")),
                };
                // The connection's host-registered UDFs, for a DEFAULT that
                // calls one (`fold_default_expr`). Taken AFTER the source
                // select, which needs `ctx` mutably.
                let host = ctx.host_fns();
                let mut built = Vec::with_capacity(src.len());
                for srow in src {
                    let mut row = Vec::with_capacity(t.columns.len());
                    for (ci, col) in t.columns.iter().enumerate() {
                        row.push(match sel.col_map[ci] {
                            Some(si) => coerce_insert_value(
                                srow.get(si as usize).cloned().unwrap_or(Value::Null),
                                col.ty,
                            ),
                            None => default_cell(col.default.as_ref(), now, host)?,
                        });
                    }
                    built.push(std::borrow::Cow::Owned(row));
                }
                built
            } else {
                let host = ctx.host_fns();
                let mut built = Vec::with_capacity(rows.len());
                for row_spec in rows {
                    built.push(build_insert_row(&t, plan, params, row_spec, now, host)?);
                }
                built
            };
            // `applied` = rows fully inserted before the current one.
            let mut written = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            // INTEGER PRIMARY KEY rowid alias (sqlite): a NULL value in the PK
            // column — from an omitted column, an explicit NULL, or a NULL param
            // — auto-assigns `max(rowid)+1`. Resolved here, per row and in order,
            // AFTER earlier rows in the same statement have been inserted, so
            // `INSERT INTO t VALUES(NULL),(NULL)` yields consecutive ids.
            let rowid_col = t.rowid_alias_col();
            // sqlite's STORE-TIME AFFINITY, applied before anything else looks
            // at the row: `'1.50'` into a `decimal(10,2)` column IS the real
            // 1.5, so RLS, triggers, CHECK, uniqueness, the index keys and
            // RETURNING must all see 1.5 and `typeof()` must say `real`. Guarded
            // by `converts_on_store` so a table with no such column never leaves
            // the borrowed zero-copy row (#40).
            let converts = t.converts_on_store();
            let generates = t.has_generated();
            for (applied, mut row) in built_rows.into_iter().enumerate() {
                // The per-ROW guard matters as much as the per-table one now
                // that a shim `text` column carries TEXT affinity (#113): most
                // rows are already in their columns' classes and stay borrowed.
                if converts && t.needs_store_affinity(&row) {
                    t.apply_store_affinity(row.to_mut());
                }
                // GENERATED ALWAYS AS (…): computed HERE, before anything else
                // looks at the row — so RLS WITH CHECK, the BEFORE triggers, the
                // OR REPLACE conflict probes, the index keys, CHECK/NOT NULL and
                // RETURNING all see the value the engine will store. The rowid
                // alias is resolved just below, so a generated column reading it
                // is recomputed there.
                if generates {
                    if let Err(e) = t.apply_generated(row.to_mut(), &[]) {
                        *partial = applied > 0;
                        return Err(e);
                    }
                }
                if let Some(rc) = rowid_col {
                    if row.get(rc as usize).is_some_and(|v| v.is_null()) {
                        let next = ctx.next_rowid(*table, rc)?;
                        row.to_mut()[rc as usize] = Value::Int(next);
                        // The auto-assigned rowid is an input a generated column
                        // may read (`b AS (id * 2)`), and it did not exist on the
                        // pass above. Recompute — `apply_generated` is idempotent.
                        if generates {
                            if let Err(e) = t.apply_generated(row.to_mut(), &[]) {
                                *partial = applied > 0;
                                return Err(e);
                            }
                        }
                    } else if let Some(Value::Int(id)) = row.get(rc as usize) {
                        // An EXPLICIT id through the alias: an AUTOINCREMENT
                        // table must remember it so it is never handed out
                        // again (a no-op for plain tables — the flag is the
                        // first check inside).
                        ctx.note_rowid(*table, *id)?;
                    }
                }
                // RLS WITH CHECK on the new row (before the engine's PK/unique
                // pre-checks): NULL and FALSE both reject (§3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = applied > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = applied > 0;
                            return Err(e);
                        }
                    }
                }
                // BEFORE INSERT FOR EACH ROW triggers fire before the row is
                // written (DESIGN-TRIGGERS §4.1), NEW = the row about to be
                // inserted (read-only). A failing body may already have written
                // to other tables on the shared txn, so it poisons the statement.
                match fire_insert(ctx, schema, &triggers.before_insert, *table, &row, triggers, depth)
                {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): skip this row's insert and all its
                    // remaining trigger work, silently (sqlite semantics).
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                // INSERT OR REPLACE: delete every existing row the proposed row
                // would collide with — on the PK AND on each secondary UNIQUE
                // index — so the insert below cannot trip a uniqueness
                // constraint (sqlite's delete-on-any-unique semantics). All
                // probes read BEFORE any delete; victims are de-duplicated so a
                // row conflicting on several constraints is removed once. A NULL
                // in a probed key means no entry and no conflict (UNIQUE and the
                // rowid-alias auto-assign both permit it), so it is skipped.
                // FOREIGN KEY, child side: the row must name a parent that
                // exists. Runs AFTER the BEFORE triggers (a body may supply the
                // parent) and BEFORE the row lands, so a violation leaves
                // nothing behind — sqlite's order.
                if let Some(g) = &triggers.fks {
                    if g.has_outgoing(*table) {
                        let mut held = Vec::new();
                        if let Err(e) = crate::fk::check_child(ctx, schema, *table, &row, &mut held)
                        {
                            *partial = applied > 0;
                            return Err(e);
                        }
                        push_fk_deferred(held);
                    }
                }
                if matches!(on_conflict, PlanOnConflict::Replace) {
                    let mut victims: Vec<Vec<Value>> = Vec::new();
                    let pk_of = |r: &[Value]| -> Vec<Value> {
                        t.primary_key.iter().map(|&c| r[c as usize].clone()).collect()
                    };
                    let pk = pk_of(&row);
                    if !pk.iter().any(|v| v.is_null()) {
                        if let Some(existing) = ctx.get_by_pk(*table, &pk)? {
                            victims.push(pk_of(&existing));
                        }
                    }
                    // The secondary-UNIQUE victims come from the CONTEXT, not
                    // from a loop here (#169): the native store keeps those
                    // constraints in the schema and probes their indexes, the
                    // sqlite-backed ones keep them off the schema on purpose
                    // and answer from their own list. Same semantics, one
                    // question.
                    victims.extend(ctx.unique_victims(*table, &t, &row)?);
                    let mut deleted: Vec<Vec<Value>> = Vec::new();
                    for v in victims {
                        if deleted.contains(&v) {
                            continue;
                        }
                        // A REPLACE victim is a DELETE as far as foreign keys
                        // are concerned — sqlite fires ON DELETE actions for it
                        // too. The pre-image has to be read before it goes.
                        if let Some(g) = &triggers.fks {
                            if g.has_incoming(&t.name) {
                                if let Some(old) = ctx.get_by_pk(*table, &v)? {
                                    let mut held = Vec::new();
                                    let r = crate::fk::on_parent_change(
                                        ctx, schema, g, *table, &old, None, &mut held,
                                        crate::fk::Phase::Guard, 0,
                                    )
                                    .and_then(|()| {
                                        ctx.delete_by_pk(*table, &v)?;
                                        crate::fk::on_parent_change(
                                            ctx, schema, g, *table, &old, None, &mut held,
                                            crate::fk::Phase::Act, 0,
                                        )
                                    });
                                    if let Err(e) = r {
                                        *partial = true;
                                        return Err(e);
                                    }
                                    push_fk_deferred(held);
                                    deleted.push(v);
                                    continue;
                                }
                            }
                        }
                        ctx.delete_by_pk(*table, &v)?;
                        deleted.push(v);
                    }
                }
                match ctx.insert_row(*table, &row) {
                    Ok(()) => {
                        written += 1;
                        // Surface the assigned/used rowid for the C-API's
                        // sqlite3_last_insert_rowid (facade hook). Only rowid-
                        // alias tables have a last-insert-rowid in sqlite; the
                        // last inserted row of the statement wins.
                        if let Some(rc) = rowid_col {
                            if let Some(Value::Int(id)) = row.get(rc as usize) {
                                record_last_insert_rowid(*id);
                            }
                        }
                        if let Some(proj) = returning {
                            out.push(project_row(proj, &row, params, ctx.host_fns())?);
                        }
                        // AFTER INSERT FOR EACH ROW triggers fire on the row just
                        // written, on the SAME txn (DESIGN-TRIGGERS §4.1/§4.3). A
                        // failing trigger poisons the statement: the row landed and
                        // the body may have written before it raised.
                        // A SkipRow here only abandons remaining trigger work —
                        // the row is already written and stays counted.
                        if let Err(e) =
                            fire_insert(ctx, schema, &triggers.after_insert, *table, &row, triggers, depth)
                        {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Err(e) if is_uniqueness(&e) && !matches!(on_conflict, PlanOnConflict::Error) => {
                        // ON CONFLICT covers uniqueness ONLY. A CHECK or
                        // NOT NULL violation is NOT a conflict and must still
                        // fail — PostgreSQL draws the same line, and swallowing
                        // them would turn `DO NOTHING` into "ignore my
                        // constraints", which is the opposite of the point.
                        match on_conflict {
                            PlanOnConflict::Error => unreachable!("guarded above"),
                            PlanOnConflict::DoNothing => { /* skip this row */ }
                            PlanOnConflict::Replace => {
                                // Replace pre-deletes every conflicting row above,
                                // so a uniqueness error here means a constraint we
                                // did not probe (should not happen) — surface it
                                // rather than silently swallow.
                                *partial = applied > 0 || !precheck_failure(&e);
                                return Err(hide_constraint_variant(
                                    e,
                                    &t.name,
                                    with_check.is_some(),
                                ));
                            }
                            PlanOnConflict::DoUpdate {
                                target,
                                probe,
                                set,
                                filter,
                            } => {
                                // Find the row this collided with, BY THE KEY
                                // THE CALLER NAMED. Probing by anything else
                                // would update a row they did not ask about.
                                let found = match probe {
                                    ConflictProbe::Pk => {
                                        let pk: Vec<Value> = target
                                            .iter()
                                            .map(|c| row[*c as usize].clone())
                                            .collect();
                                        ctx.get_by_pk(*table, &pk)?
                                    }
                                    ConflictProbe::Index(ino) => {
                                        // Probe values in the INDEX's column
                                        // order — a composite target's list
                                        // order may differ (#55).
                                        let cols = &t
                                            .indexes
                                            .get(*ino as usize - 1)
                                            .ok_or_else(|| {
                                                Error::Internal(
                                                    "conflict probe index out of range".into(),
                                                )
                                            })?
                                            .columns;
                                        let vals: Vec<Value> = cols
                                            .iter()
                                            .map(|&c| row[c as usize].clone())
                                            .collect();
                                        // UNIQUE permits many NULLs, so any
                                        // NULL here cannot have collided with
                                        // anything and there is no row to find.
                                        if vals.iter().any(|v| v.is_null()) {
                                            None
                                        } else {
                                            ctx.get_by_index(*table, *ino, &vals)?
                                        }
                                    }
                                };
                                let Some(existing) = found else {
                                    // The insert failed on SOME uniqueness
                                    // constraint, but not the one named: a
                                    // PK-target insert that tripped a secondary
                                    // UNIQUE, or an email-target insert that
                                    // tripped the PK. That conflict is not the
                                    // one the caller asked to handle, so it is
                                    // an error -- exactly as in PostgreSQL, and
                                    // the alternative (silently doing nothing)
                                    // would hide a real collision.
                                    *partial = applied > 0 || !precheck_failure(&e);
                                    return Err(hide_constraint_variant(
                                        e,
                                        &t.name,
                                        with_check.is_some(),
                                    ));
                                };
                                // SET/WHERE see [existing ‖ proposed]: that is
                                // what `excluded.<c>` = Col(n + i) resolves to.
                                let mut both = existing.clone();
                                both.extend_from_slice(&row);
                                if let Some(f) = filter {
                                    match f.eval_filter_host(
                                        &mut Vec::new(),
                                        &both,
                                        params,
                                        ctx.host_fns(),
                                    ) {
                                        Ok(true) => {}
                                        // NULL and FALSE both skip: SQL needs
                                        // exactly TRUE to act.
                                        Ok(false) => continue,
                                        Err(e) => {
                                            *partial = applied > 0;
                                            return Err(e);
                                        }
                                    }
                                }
                                let mut new_row = existing;
                                for (c, program) in set {
                                    let v = program.eval_host(&both, params, ctx.host_fns())?;
                                    new_row[*c as usize] = v;
                                }
                                // DO UPDATE assigns into the column like any
                                // other write, so the same store-time affinity —
                                // and the generated columns are recomputed from
                                // the post-image, exactly as on a plain UPDATE.
                                t.apply_store_affinity(&mut new_row);
                                if generates {
                                    if let Err(e) = t.apply_generated(&mut new_row, &[]) {
                                        *partial = applied > 0;
                                        return Err(e);
                                    }
                                }
                                if let Err(e) = ctx.update_by_pk(*table, &new_row) {
                                    *partial = applied > 0 || !precheck_failure(&e);
                                    return Err(hide_constraint_variant(
                                        e,
                                        &t.name,
                                        with_check.is_some(),
                                    ));
                                }
                                written += 1;
                                if let Some(proj) = returning {
                                    out.push(project_row(proj, &new_row, params, ctx.host_fns())?);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // A pre-check failure left even this row unapplied, so
                        // the statement is partial only if earlier rows landed.
                        // NOTE the order: `partial` is decided from the ORIGINAL
                        // error, then the variant is hidden (§6.5).
                        *partial = applied > 0 || !precheck_failure(&e);
                        return Err(hide_constraint_variant(e, &t.name, with_check.is_some()));
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(written)),
            }
        }

        PlanStmt::Update {
            table,
            access,
            filter,
            post_filter,
            set,
            with_check,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Collect-then-mutate: gather the matching CURRENT rows first
            // (read-only; a failure here has no effects). The CORRELATED half
            // of the WHERE is applied to those rows before any of them is
            // touched, so the read that decides and the write that follows see
            // the same snapshot.
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            let old_rows =
                dml_survivors(ctx, schema, plan, params, old_rows, post_filter.as_ref())?;
            // The UPDATE's SET target columns — an `UPDATE OF <cols>` trigger
            // fires only when one of its columns is among these (sqlite
            // semantics). Statement-wide, so computed once.
            let changed: Vec<u16> = set.iter().map(|(c, _)| *c).collect();
            let mut affected = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            for old in &old_rows {
                let new_row = (|| -> Result<Vec<Value>> {
                    let mut new_row = old.clone();
                    for (c, program) in set {
                        // SQL semantics: ALL set-expressions evaluate against
                        // the OLD row, not against earlier assignments.
                        let slot = new_row
                            .get_mut(*c as usize)
                            .ok_or_else(|| internal("SET column"))?;
                        *slot = program.eval_host(old, params, ctx.host_fns())?;
                    }
                    // The assigned values enter the column exactly as an
                    // INSERT's do, so they take the same store-time affinity
                    // (sqlite applies it to `UPDATE … SET` too) — before the
                    // WITH CHECK, the triggers and RETURNING below see the row.
                    t.apply_store_affinity(&mut new_row);
                    // Generated columns are recomputed from the POST-image: a
                    // SET on one of their inputs changes them, which is why
                    // `UPDATE … SET <generated> = …` is refused at bind time —
                    // the expression is the only source of the value.
                    if t.has_generated() {
                        t.apply_generated(&mut new_row, &[])?;
                    }
                    Ok(new_row)
                })();
                let new_row = match new_row {
                    Ok(r) => r,
                    Err(e) => {
                        // Evaluation is side-effect-free; only rows already
                        // updated count.
                        *partial = affected > 0;
                        return Err(e);
                    }
                };
                // RLS WITH CHECK on the post-image (NULL and FALSE reject, §3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &new_row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = affected > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = affected > 0;
                            return Err(e);
                        }
                    }
                }
                // BEFORE UPDATE FOR EACH ROW triggers fire before the row is
                // rewritten (DESIGN-TRIGGERS §4.1): NEW = the post-image (read-
                // only), OLD = the pre-image. A failing body poisons the statement.
                match fire_update(
                    ctx,
                    schema,
                    &triggers.before_update,
                    *table,
                    &new_row,
                    old,
                    &changed,
                    triggers,
                    depth,
                ) {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): leave the row as it was, silently.
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                // FOREIGN KEY (#194). Two sides, both before the rewrite:
                // the post-image must still name a live parent, and any child
                // pointing at the PRE-image's key must be acted on.
                let mut fk_held = Vec::new();
                if let Some(g) = &triggers.fks {
                    let r = (|ctx: &mut dyn TxnCtx| -> Result<()> {
                        if g.has_outgoing(*table) {
                            crate::fk::check_child(ctx, schema, *table, &new_row, &mut fk_held)?;
                        }
                        if g.has_incoming(&t.name) {
                            crate::fk::on_parent_change(
                                ctx,
                                schema,
                                g,
                                *table,
                                old,
                                Some(&new_row),
                                &mut fk_held,
                                crate::fk::Phase::Guard,
                                0,
                            )?;
                        }
                        Ok(())
                    })(ctx);
                    if let Err(e) = r {
                        *partial = affected > 0;
                        return Err(e);
                    }
                }
                match ctx.update_by_pk(*table, &new_row) {
                    Ok(true) => {
                        affected += 1;
                        // The MUTATING half of the parent side runs here, on
                        // the far side of the write: `ON UPDATE CASCADE`
                        // carries children to a key that only exists now.
                        if let Some(g) = &triggers.fks {
                            if g.has_incoming(&t.name) {
                                if let Err(e) = crate::fk::on_parent_change(
                                    ctx, schema, g, *table, old, Some(&new_row), &mut fk_held,
                                    crate::fk::Phase::Act, 0,
                                ) {
                                    *partial = true;
                                    return Err(e);
                                }
                            }
                        }
                        push_fk_deferred(std::mem::take(&mut fk_held));
                        // RETURNING on UPDATE projects the POST-image: SQL
                        // returns the row as it now is, not as it was.
                        if let Some(proj) = returning {
                            out.push(project_row(proj, &new_row, params, ctx.host_fns())?);
                        }
                        // AFTER UPDATE FOR EACH ROW triggers fire on the updated
                        // row, on the SAME txn (DESIGN-TRIGGERS §4.1): NEW = the
                        // post-image, OLD = the pre-image. A failing trigger
                        // poisons the statement — the row changed and the body may
                        // have written before it raised.
                        // SkipRow here only abandons remaining trigger work —
                        // the row is already rewritten and stays counted.
                        if let Err(e) = fire_update(
                            ctx,
                            schema,
                            &triggers.after_update,
                            *table,
                            &new_row,
                            old,
                            &changed,
                            triggers,
                            depth,
                        ) {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Ok(false) => {} // row vanished: nothing changed
                    Err(e) => {
                        // `partial` from the original variant, then hide it (§6.5).
                        *partial = affected > 0 || !precheck_failure(&e);
                        return Err(hide_constraint_variant(e, &t.name, with_check.is_some()));
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(affected)),
            }
        }

        PlanStmt::Delete {
            table,
            access,
            filter,
            post_filter,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Gather full old rows (the residual filter needs them), then
            // delete by PK values extracted from each row. The CORRELATED half
            // of the WHERE runs over the gathered set BEFORE the first delete,
            // so a subquery that reads the target table sees the PRE-write
            // state — SQL's rule, and sqlite's.
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            let old_rows =
                dml_survivors(ctx, schema, plan, params, old_rows, post_filter.as_ref())?;
            let mut affected = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            for old in &old_rows {
                let mut pk = Vec::with_capacity(t.primary_key.len());
                for &i in &t.primary_key {
                    let v = match old.get(i as usize) {
                        Some(v) => v.clone(),
                        None => {
                            *partial = affected > 0;
                            return Err(internal("PK column"));
                        }
                    };
                    pk.push(v);
                }
                // BEFORE DELETE FOR EACH ROW triggers fire before the row is
                // removed (DESIGN-TRIGGERS §4.1): only OLD is available. A failing
                // body poisons the statement.
                match fire_delete(ctx, schema, &triggers.before_delete, *table, old, triggers, depth)
                {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): keep the row, silently.
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                // FOREIGN KEY (#194): every child pointing at this row is
                // cascaded, nulled or refused BEFORE the parent disappears.
                let mut fk_held = Vec::new();
                if let Some(g) = &triggers.fks {
                    if g.has_incoming(&t.name) {
                        if let Err(e) = crate::fk::on_parent_change(
                            ctx, schema, g, *table, old, None, &mut fk_held,
                            crate::fk::Phase::Guard, 0,
                        ) {
                            *partial = affected > 0;
                            return Err(e);
                        }
                    }
                }
                match ctx.delete_by_pk(*table, &pk) {
                    Ok(true) => {
                        affected += 1;
                        // Cascades run AFTER the parent is gone, as in sqlite.
                        if let Some(g) = &triggers.fks {
                            if g.has_incoming(&t.name) {
                                if let Err(e) = crate::fk::on_parent_change(
                                    ctx, schema, g, *table, old, None, &mut fk_held,
                                    crate::fk::Phase::Act, 0,
                                ) {
                                    *partial = true;
                                    return Err(e);
                                }
                            }
                        }
                        push_fk_deferred(std::mem::take(&mut fk_held));
                        // RETURNING on DELETE projects the row as it WAS: there
                        // is no post-image to show.
                        if let Some(proj) = returning {
                            out.push(project_row(proj, old, params, ctx.host_fns())?);
                        }
                        // AFTER DELETE FOR EACH ROW triggers fire on the deleted
                        // row, on the SAME txn (DESIGN-TRIGGERS §4.1): only OLD is
                        // available. A failing trigger poisons the statement.
                        // SkipRow here only abandons remaining trigger work —
                        // the row is already gone and stays counted.
                        if let Err(e) =
                            fire_delete(ctx, schema, &triggers.after_delete, *table, old, triggers, depth)
                        {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // delete_by_pk has no pre-check failure class: any
                        // error may have fired mid index maintenance.
                        *partial = true;
                        return Err(e);
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(affected)),
            }
        }

        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Err(Error::Unsupported(
            "transaction control cannot be executed as a plan; \
             use Database::begin() and WriteSession::commit()/rollback()"
                .into(),
        )),
        PlanStmt::Savepoint(_) | PlanStmt::Release(_) | PlanStmt::RollbackTo(_) => {
            Err(Error::Unsupported(
                "SAVEPOINT/RELEASE/ROLLBACK TO are handled by the write session, \
                 not executed as a plan; run them through WriteSession::query()"
                    .into(),
            ))
        }
    }
}

/// Fire `INSERT` triggers of one timing on `table` for one inserted row (only
/// `NEW` in scope). See [`fire_row_triggers`].
fn fire_insert(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    new_row: &[Value],
    triggers: &WriteRules,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, Some(new_row), None, &[], triggers, depth),
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire `UPDATE` triggers of one timing on `table` for one updated row: `NEW` =
/// the post-image, `OLD` = the pre-image (DESIGN-TRIGGERS §4.1). `changed` names
/// the columns the UPDATE assigned (its SET target list) — an `UPDATE OF <cols>`
/// trigger fires only when one of its columns is among them. See
/// [`fire_row_triggers`].
#[allow(clippy::too_many_arguments)]
fn fire_update(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    new_row: &[Value],
    old_row: &[Value],
    changed: &[u16],
    triggers: &WriteRules,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => {
            fire_row_triggers(ctx, schema, trigs, Some(new_row), Some(old_row), changed, triggers, depth)
        }
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire `DELETE` triggers of one timing on `table` for one deleted row (only
/// `OLD` in scope, the deleted row). See [`fire_row_triggers`].
fn fire_delete(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    old_row: &[Value],
    triggers: &WriteRules,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, None, Some(old_row), &[], triggers, depth),
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire a set of matching `… FOR EACH ROW` triggers for one changed row, on the
/// SAME `ctx` (DESIGN-TRIGGERS §4). `UPDATE OF <cols>` triggers are skipped
/// unless one of their columns is in `changed` (the UPDATE's SET target list;
/// empty for INSERT/DELETE, where `update_of` is always empty too). Each
/// trigger's optional `WHEN` is a 3VL gate (only TRUE fires; NULL and FALSE
/// skip); the body is a SEQUENCE of ordinary plans, each whose leading
/// parameters are the `NEW`/`OLD` columns named by its row-slot map, filled from
/// the `new`/`old` images and executed in body order by recursing on the held
/// txn at `depth + 1` — never through the facade, so the writer lock and intent
/// ring are never re-entered. A hard depth cap bounds any cascade.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_row_triggers(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    trigs: &[CompiledTrigger],
    new: Option<&[Value]>,
    old: Option<&[Value]>,
    changed: &[u16],
    triggers: &WriteRules,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    if trigs.is_empty() {
        return Ok(crate::trigger::FireOutcome::Proceed);
    }
    if depth + 1 > MAX_TRIGGER_DEPTH {
        return Err(Error::Unsupported(format!(
            "trigger recursion too deep (> {MAX_TRIGGER_DEPTH} levels)"
        )));
    }
    // Fill a row-slot map from the NEW/OLD images. A slot naming a side not
    // present for this event is an internal bug (the binder only emits slots the
    // event allows), so it fails closed rather than mis-binding.
    let pick = |map: &RowMap| -> Result<Vec<Value>> {
        map.iter()
            .map(|&(side, c)| {
                let row = match side {
                    RowSide::New => new,
                    RowSide::Old => old,
                };
                row.and_then(|r| r.get(c as usize).cloned())
                    .ok_or_else(|| internal("trigger NEW/OLD column out of row bounds"))
            })
            .collect()
    };
    for trig in trigs {
        // `UPDATE OF <cols>`: fire only when one named column is assigned by the
        // UPDATE (sqlite semantics — the SET target list, not a value change).
        if !trig.update_of.is_empty() && !trig.update_of.iter().any(|c| changed.contains(c)) {
            continue;
        }
        // `recursive_triggers` OFF (the default, sqlite's): a trigger that is
        // already ACTIVE in this cascade — its body is what (directly or via a
        // cycle) caused this fire — is not re-entered. This is what quietly
        // stops `AFTER INSERT ON t … INSERT INTO t` after one round instead of
        // erroring at the depth cap.
        if !triggers.recursive && trigger_is_active(&trig.name) {
            continue;
        }
        if let Some((prog, when_map)) = &trig.when {
            let wp = pick(when_map)?;
            let mut stack = Vec::new();
            if !prog.eval_filter(&mut stack, &[], &wp)? {
                continue;
            }
        }
        // The #74 work meter charges one row per (trigger, row) FIRE: the
        // depth cap bounds how DEEP a cascade goes, this bounds how WIDE —
        // an exponential fan-out trips `RuntimeBudget` at a fixed, repeatable
        // count instead of running 2^depth statements.
        ctx.charge_work(1, &|| format!("trigger \"{}\"", trig.name))?;
        let _active = ActiveTrigger::enter(&trig.name);
        match &trig.body {
            // Multi-statement body: each statement runs in order on the same txn.
            crate::trigger::TriggerBody::Sql(stmts) => {
                for stmt in stmts {
                    match stmt {
                        mpedb_sql::TriggerStmt::Dml(body_plan, body_map) => {
                            let body_params = pick(body_map)?;
                            let mut inner_partial = false;
                            exec_stmt_triggered(
                                ctx,
                                schema,
                                body_plan,
                                &body_params,
                                &mut inner_partial,
                                triggers,
                                depth + 1,
                            )?;
                        }
                        // `SELECT RAISE(…) [WHERE …]` (DESIGN-TRIGGERS §4.3):
                        // the gate is 3VL like WHEN — only TRUE raises.
                        mpedb_sql::TriggerStmt::Raise { kind, msg, gate } => {
                            if let Some((prog, gate_map)) = gate {
                                let gp = pick(gate_map)?;
                                let mut stack = Vec::new();
                                if !prog.eval_filter(&mut stack, &[], &gp)? {
                                    continue;
                                }
                            }
                            match kind {
                                mpedb_sql::TriggerRaise::Abort => {
                                    return Err(Error::Raise(msg.clone()));
                                }
                                // sqlite: IGNORE abandons the remainder of THIS
                                // trigger program, the row operation, and every
                                // subsequent trigger program for the row.
                                mpedb_sql::TriggerRaise::Ignore => {
                                    return Ok(crate::trigger::FireOutcome::SkipRow);
                                }
                            }
                        }
                        // A FROM-less `SELECT <expr>` body statement: sqlite
                        // evaluates it and drops the row. So do we — the values
                        // go nowhere, but an expression that RAISES still
                        // aborts the triggering statement, which is the point
                        // of evaluating rather than skipping.
                        mpedb_sql::TriggerStmt::Eval { progs, map } => {
                            let params = pick(map)?;
                            for prog in progs {
                                let _ = prog.eval(&[], &params)?;
                            }
                        }
                    }
                }
            }
            // PySpell body (DESIGN-TRIGGERS §5): evaluate the argument
            // programs over the row images, then run the pinned procedure's IR
            // on THIS ctx through the bridge — its embedded statements recurse
            // like an SQL body's, never through the facade.
            crate::trigger::TriggerBody::Spell(sb) => {
                let ready = sb.ready.as_ref().map_err(|m| {
                    Error::Unsupported(format!("trigger `{}`: {m}", trig.name))
                })?;
                let mut args = Vec::with_capacity(sb.args.len());
                let mut stack = Vec::new();
                for (prog, arg_map) in &sb.args {
                    let slots = pick(arg_map)?;
                    args.push(prog.eval_with_stack(&mut stack, &[], &slots)?);
                }
                let mut bridge = CtxBridge {
                    ctx,
                    schema,
                    plans: &ready.plans,
                    triggers,
                    depth,
                    streams: Vec::new(),
                };
                mpedb_spell::interp::run(
                    &ready.proc,
                    &args,
                    &mut bridge,
                    crate::trigger::TRIGGER_BUDGET,
                )?;
            }
        }
    }
    Ok(crate::trigger::FireOutcome::Proceed)
}

std::thread_local! {
    /// Names of the triggers whose bodies are executing on THIS thread's
    /// statement cascade, innermost last. A statement executes synchronously
    /// on one thread and nested fires recurse on the same one, so a
    /// thread-local stack IS the cascade's activation record — no signature
    /// threading through `exec_stmt_triggered`'s many callers.
    static ACTIVE_TRIGGERS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn trigger_is_active(name: &str) -> bool {
    ACTIVE_TRIGGERS.with(|a| a.borrow().iter().any(|n| n == name))
}

/// RAII activation record: pushed around a trigger's body execution, popped on
/// drop — including the `?`-unwind paths, so an aborting body can never leave
/// its name stuck "active" for the session's next statement.
struct ActiveTrigger;

impl ActiveTrigger {
    fn enter(name: &str) -> ActiveTrigger {
        ACTIVE_TRIGGERS.with(|a| a.borrow_mut().push(name.to_string()));
        ActiveTrigger
    }
}

impl Drop for ActiveTrigger {
    fn drop(&mut self) {
        ACTIVE_TRIGGERS.with(|a| {
            a.borrow_mut().pop();
        });
    }
}

/// [`DbBridge`](mpedb_spell::interp::DbBridge) over the LIVE transaction a
/// trigger fires inside (DESIGN-TRIGGERS §5.2): each embedded statement was
/// pre-resolved by hash at catalog build, and runs here by recursing on the
/// same `ctx` at `depth + 1` — the procedure sees the triggering statement's
/// uncommitted writes and unwinds with it, and its own DML fires nested
/// triggers under the same depth cap. Cursors materialize their result on
/// open (the k-row streaming path needs a reader slot a held write txn cannot
/// nest), which preserves semantics exactly — the interpreter's row budget
/// still meters consumption.
struct CtxBridge<'a> {
    ctx: &'a mut dyn TxnCtx,
    schema: &'a Schema,
    plans: &'a std::collections::HashMap<[u8; 32], Arc<CompiledPlan>>,
    triggers: &'a WriteRules,
    depth: u32,
    streams: Vec<Option<std::vec::IntoIter<Vec<Value>>>>,
}

impl CtxBridge<'_> {
    fn run_plan(
        &mut self,
        plan_ref: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<ExecResult> {
        let plan = self.plans.get(&plan_ref.hash.0).ok_or_else(|| {
            internal("trigger procedure references an unresolved plan (catalog-build bug)")
        })?;
        // Rebuild the full parameter buffer the way `session::resolve_params`
        // does, minus the session: user params, NULL holes for subplan slots
        // (the executor fills them), and the statement instant for a literal
        // `'now'`. Any other context key was refused at catalog build.
        let n_ctx = plan.context_keys.len();
        let n_sub = plan.n_subplan_slots() as usize;
        let n_user = plan.n_params as usize - n_ctx - n_sub;
        if params.len() != n_user {
            return Err(Error::WrongParamCount {
                expected: n_user,
                got: params.len(),
            });
        }
        let plan = plan.clone();
        let mut full = Vec::with_capacity(plan.n_params as usize);
        full.extend_from_slice(params);
        full.resize(n_user + n_sub, Value::Null);
        for key in &plan.context_keys {
            if key == mpedb_sql::STATEMENT_INSTANT_KEY {
                full.push(Value::Text(mpedb_types::sqlite_now_string(now_micros())));
            } else {
                return Err(Error::Unsupported(format!(
                    "current_setting('{key}') needs a session and is not \
                     available inside a trigger"
                )));
            }
        }
        let mut inner_partial = false;
        exec_stmt_triggered(
            self.ctx,
            self.schema,
            &plan,
            &full,
            &mut inner_partial,
            self.triggers,
            self.depth + 1,
        )
    }
}

impl mpedb_spell::interp::DbBridge for CtxBridge<'_> {
    fn query(
        &mut self,
        plan: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        match self.run_plan(plan, params)? {
            ExecResult::Rows { rows, .. } => Ok(rows),
            other => Err(internal(&format!(
                "trigger procedure query returned {other:?} (validator bug)"
            ))),
        }
    }

    fn exec(&mut self, plan: &mpedb_spell::ir::PlanRef, params: &[Value]) -> Result<u64> {
        match self.run_plan(plan, params)? {
            ExecResult::Affected(n) => Ok(n),
            // RETURNING inside a procedure's exec: row count is the answer.
            ExecResult::Rows { rows, .. } => Ok(rows.len() as u64),
            other => Err(internal(&format!(
                "trigger procedure exec returned {other:?} (validator bug)"
            ))),
        }
    }

    fn cursor_open(
        &mut self,
        plan: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<u32> {
        let rows = self.query(plan, params)?;
        let slot = self
            .streams
            .iter()
            .position(|s| s.is_none())
            .unwrap_or(self.streams.len());
        if slot == self.streams.len() {
            self.streams.push(None);
        }
        self.streams[slot] = Some(rows.into_iter());
        Ok(slot as u32)
    }

    fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>> {
        let slot = self
            .streams
            .get_mut(stream as usize)
            .ok_or_else(|| internal("trigger procedure advanced an unknown cursor"))?;
        let Some(it) = slot else {
            return Ok(None);
        };
        let row = it.next();
        if row.is_none() {
            *slot = None;
        }
        Ok(row)
    }
}

/// Project one written row through a `RETURNING` clause.
///
/// `host` carries the connection's host UDF closures (design/DESIGN-UDF.md);
/// `RETURNING plus1(x)` is a write-path expression like any other and resolves
/// them exactly as a SELECT list would.
fn project_row(
    proj: &[Projection],
    row: &[Value],
    params: &[Value],
    host: Option<&dyn HostFns>,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(proj.len());
    for p in proj {
        out.push(match p {
            Projection::Column(i) => row
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("RETURNING column out of row bounds"))?,
            Projection::Expr { program, .. } => program.eval_host(row, params, host)?,
        });
    }
    Ok(out)
}

/// Output column names for a `RETURNING` clause.
fn projection_names(proj: &[Projection], t: &TableDef) -> Vec<String> {
    proj.iter()
        .map(|p| match p {
            Projection::Column(i) => t
                .columns
                .get(*i as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "?".to_string()),
            Projection::Expr { name, .. } => name.clone(),
        })
        .collect()
}

/// Does this error mean "a uniqueness constraint said no"?
///
/// `ON CONFLICT` covers uniqueness ONLY — PostgreSQL is explicit about that,
/// and it matters: if a CHECK or NOT NULL violation counted as a conflict,
/// `DO NOTHING` would quietly mean "ignore my constraints" and the rows you
/// thought you validated would just be missing.
fn is_uniqueness(e: &Error) -> bool {
    matches!(
        e,
        Error::PrimaryKeyViolation { .. } | Error::UniqueViolation { .. }
    )
}

/// Resolve one INSERT row spec (params/consts/defaults) to concrete values.
/// Pure: touches no transaction state.
fn build_insert_row<'a>(
    t: &TableDef,
    plan: &CompiledPlan,
    params: &'a [Value],
    row_spec: &[InsertSource],
    now: i64,
    host: Option<&dyn HostFns>,
) -> Result<std::borrow::Cow<'a, [Value]>> {
    // #40 instrument: this is per ROW, so the timing only exists under the
    // leakstat feature — an unconditional Instant here would tax bulk loads.
    #[cfg(feature = "leakstat")]
    {
        let t0 = mpedb_core::Instant::now();
        let r = build_insert_row_impl(t, plan, params, row_spec, now, host);
        mpedb_core::engine::leakstat::add(
            &mpedb_core::engine::leakstat::EXEC_NS_BUILDROW,
            t0.elapsed().as_nanos() as u64,
        );
        r
    }
    #[cfg(not(feature = "leakstat"))]
    build_insert_row_impl(t, plan, params, row_spec, now, host)
}

fn build_insert_row_impl<'a>(
    t: &TableDef,
    plan: &CompiledPlan,
    params: &'a [Value],
    row_spec: &[InsertSource],
    now: i64,
    host: Option<&dyn HostFns>,
) -> Result<std::borrow::Cow<'a, [Value]>> {
    // The identity fast path: the common single-row INSERT where every column
    // comes straight from the caller's params, in declaration order — borrow
    // instead of cloning. This was the THIRD full deep-clone of a blob on its
    // way in (#40: ~2.3 ms of a warm 16 MiB insert, measured 2026-07-16 with
    // blob_warm --features leakstat). Any Default/Const/now() or reordered
    // spec takes the owned path below, so default resolution and the
    // partial-effects semantics of multi-row INSERT are untouched.
    if row_spec.len() == params.len()
        && row_spec
            .iter()
            .enumerate()
            .all(|(ci, s)| matches!(s, InsertSource::Param(i) if *i as usize == ci))
    {
        return Ok(std::borrow::Cow::Borrowed(params));
    }
    let mut row = Vec::with_capacity(row_spec.len());
    for (ci, src) in row_spec.iter().enumerate() {
        row.push(match src {
            InsertSource::Param(i) => params
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert param"))?,
            InsertSource::Const(i) => plan
                .consts
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert const"))?,
            InsertSource::Default => {
                let col = t.columns.get(ci).ok_or_else(|| internal("insert col"))?;
                // plan-validated: a column with no default is nullable
                default_cell(col.default.as_ref(), now, host)?
            }
            InsertSource::Expr(prog) => {
                // Dual row: empty tuple. Program carries its own const pool.
                // `host` is what makes a cell calling a registered UDF legal
                // (Django's `bulk_create` over such a column); the planner
                // used to refuse it precisely because this call had no scope.
                prog.eval_host(&[], params, host)?
            }
        });
    }
    Ok(std::borrow::Cow::Owned(row))
}

/// Coerce one `INSERT … SELECT` source value toward the target column type.
/// Only the loss-less integer→float widening is applied (the same the VALUES
/// path does at plan time via `coerce_const`); everything else passes through
/// and the engine's `validate_row` enforces the rigid type at write time.
fn coerce_insert_value(v: Value, ty: mpedb_types::ColumnType) -> Value {
    match (&v, ty) {
        (Value::Int(i), mpedb_types::ColumnType::Float64) => Value::Float(*i as f64),
        // The parameter rule (`coerce_params` below), applied to the
        // INSERT … SELECT copy: 0/1 IS sqlite's representation of a boolean,
        // so a source column feeding a `bool` target converts exactly —
        // Django's table rebuild writes `SELECT …, 0 AS awesome` into the
        // rebuilt table's bool column (`test_add_field_temp_default_boolean`).
        // Any other integer keeps refusing via the engine's type check.
        (Value::Int(i @ (0 | 1)), mpedb_types::ColumnType::Bool) => Value::Bool(*i == 1),
        _ => v,
    }
}
