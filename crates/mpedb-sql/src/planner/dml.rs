//! INSERT/UPDATE/DELETE planning (moved verbatim from `planner/mod.rs`).

use super::*;

/// `INSERT … VALUES (<expression>)` → `INSERT … SELECT <expression>`, or
/// `None` to leave the statement alone.
///
/// Only a SINGLE row is rewritten — a multi-row VALUES would need
/// `UNION ALL`, which the INSERT … SELECT path refuses by name — and only when
/// the row holds something the VALUES path cannot carry. That is decided by
/// BINDING it: a bare parameter and anything that const-folds (`1 + 1`,
/// `-24`) stay on the VALUES path with their existing constant coercion and
/// NOT-NULL checks, so no statement that compiles today changes shape. A bind
/// error here yields `None` too — the real path reports it, with its own
/// message.
fn values_as_select(
    s: &ast::InsertStmt,
    table: &mpedb_types::TableDef,
    n_params: u16,
    mode: Dialect,
    host_udfs: &HostUdfSet,
) -> Option<ast::InsertStmt> {
    if s.select.is_some() || s.rows.len() != 1 {
        return None;
    }
    let row = &s.rows[0];
    let mut probe = Binder::new(table, n_params, true);
    probe.set_dialect(mode);
    probe.set_host_udfs(host_udfs);
    let needs_select = row.iter().any(|e| match probe.bind_expr(e) {
        Ok((BExpr::Const(_), _)) | Ok((BExpr::Param(_), _)) => false,
        Ok(_) => true,
        // A subquery has no bound form OUTSIDE a SELECT — the binder refuses it
        // by name — so a refusal is the signal too, for everything that is not
        // already a literal or a bare parameter (which cannot fail to bind).
        // The SELECT path then reports whatever the real error is.
        Err(_) => !matches!(e, ast::Expr::Lit(_) | ast::Expr::Param(_)),
    });
    if !needs_select {
        return None;
    }
    Some(ast::InsertStmt {
        table: s.table.clone(),
        columns: s.columns.clone(),
        rows: Vec::new(),
        select: Some(Box::new(ast::SelectStmt {
            table: None,
            from_derived: None,
            from_series: None,
            alias: None,
            joins: Vec::new(),
            distinct: false,
            items: Some(row.iter().map(|e| (e.clone(), None)).collect()),
            where_clause: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        drop_trailing: 0,
        })),
        on_conflict: s.on_conflict.clone(),
        returning: s.returning.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_insert(
    s: &ast::InsertStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: Dialect,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;

    // `VALUES (<expression>)` — a function call, a scalar subquery, arithmetic
    // over one — is the same statement as `SELECT <expression>` with no FROM:
    // sqlite evaluates a VALUES row exactly once, over no row, which is what a
    // FROM-less SELECT already is here. Django writes it for every
    // `RETURNING` insert of a database function (`STRFTIME(…)`, `LOWER(?)`).
    // Rewritten rather than given its own `InsertSource` variant: the
    // INSERT … SELECT path already evaluates, projects and RETURNs, and this
    // needs no plan-format change. The trigger is a row that the VALUES path
    // would REFUSE (nothing that folds to a constant or is a bare parameter
    // moves), so every statement that compiles today keeps its exact plan.
    if let Some(rewritten) = values_as_select(s, table, n_params, mode, host_udfs) {
        return plan_insert(
            &rewritten, schema, n_params, catalog, mode, host_udfs, row_count, consts,
        );
    }

    let mut binder = Binder::new(table, n_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);

    // Map each table column to its position in the VALUES tuples (or None).
    let listed: Vec<u16> = match &s.columns {
        Some(names) => {
            let mut cols = Vec::with_capacity(names.len());
            for name in names {
                let idx = table.column_index(name).ok_or_else(|| {
                    bind_err(format!("unknown column `{name}` in table `{}`", table.name))
                })?;
                if cols.contains(&idx) {
                    return Err(bind_err(format!("duplicate column `{name}` in INSERT")));
                }
                // A generated column's value is the expression's, always.
                // sqlite: "cannot INSERT into generated column".
                if table.columns[idx as usize].generated.is_some() {
                    return Err(bind_err(format!(
                        "cannot INSERT into generated column `{name}`"
                    )));
                }
                cols.push(idx);
            }
            cols
        }
        // No column list: the VISIBLE, non-GENERATED columns, in order. A hidden
        // implicit rowid (#94) is NOT listed — it falls through to
        // `InsertSource::Default` below and the rowid-alias auto-assign (#85)
        // fills it with `max(rowid)+1`. A generated column is not listed either:
        // sqlite's `INSERT INTO t VALUES (…)` counts only the non-generated
        // columns, and the value is computed at write time.
        None => (0..table.visible_column_count() as u16)
            .filter(|&i| table.columns[i as usize].generated.is_none())
            .collect(),
    };
    let mut slot_of_col: Vec<Option<usize>> = vec![None; table.columns.len()];
    for (slot, &col) in listed.iter().enumerate() {
        slot_of_col[col as usize] = Some(slot);
    }
    // A single-column INTEGER PRIMARY KEY is a rowid alias (sqlite): an omitted
    // or NULL value auto-assigns at execution time, so it is exempt from both
    // the "NOT NULL must be inserted" rule below and the NULL-const rejection.
    // The auto-assign is carried as `InsertSource::Default` on that column —
    // unambiguous, since a NOT-NULL no-default PK column could never take a
    // Default before this feature (so no plan format change is needed).
    let rowid_col = table.rowid_alias_col();
    // Columns omitted from the list must be defaultable (the rowid alias is not).
    for (ci, col) in table.columns.iter().enumerate() {
        if slot_of_col[ci].is_none()
            && !col.nullable
            && col.default.is_none()
            && col.generated.is_none()
            && Some(ci as u16) != rowid_col
        {
            // The same reasoning as the assignment site: detected at bind,
            // but it IS a NOT NULL violation and must carry that class, or a
            // consumer catching `IntegrityError` sees nothing.
            return Err(Error::NotNullViolation {
                table: table.name.clone(),
                column: col.name.clone(),
            });
        }
    }

    // INSERT … SELECT: plan the source query and map its output tuple to the
    // target columns. Its params/context/list keys and subplans merge into
    // this statement's below.
    let mut from_select = None;
    let mut sel_ptypes: Vec<Option<ColumnType>> = Vec::new();
    let mut sel_ctx: Vec<String> = Vec::new();
    let mut sel_list: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut sel_subplans: Vec<SubPlan> = Vec::new();
    if let Some(sel_stmt) = &s.select {
        let (sp_stmt, sp_pt, sp_ctx, sp_list, _sp_agg, sp_sub) =
            plan_select(sel_stmt, schema, n_params, catalog, mode, host_udfs, row_count, consts, None, &[])?;
        let PlanStmt::Select(sp) = sp_stmt else {
            return Err(bind_err(
                "INSERT … SELECT: a compound (UNION/EXCEPT/INTERSECT) source is not supported",
            ));
        };
        if sp.projection.len() != listed.len() {
            return Err(bind_err(format!(
                "INSERT … SELECT: the source has {} column(s), but {} are expected",
                sp.projection.len(),
                listed.len()
            )));
        }
        let col_map: Vec<Option<u16>> =
            slot_of_col.iter().map(|s| s.map(|x| x as u16)).collect();
        from_select = Some(crate::plan::InsertSelect { plan: Box::new(sp), col_map });
        sel_ptypes = sp_pt;
        sel_ctx = sp_ctx;
        sel_list = sp_list;
        sel_subplans = sp_sub;
    }

    let mut rows = Vec::with_capacity(s.rows.len());
    for row in &s.rows {
        if from_select.is_some() {
            return Err(bind_err("INSERT cannot have both VALUES and a SELECT source"));
        }
        if row.len() != listed.len() {
            return Err(bind_err(format!(
                "INSERT row has {} values, expected {}",
                row.len(),
                listed.len()
            )));
        }
        let mut sources = Vec::with_capacity(table.columns.len());
        for (ci, col) in table.columns.iter().enumerate() {
            let src = match slot_of_col[ci] {
                None => InsertSource::Default,
                Some(slot) => {
                    let (b, _) = binder.bind_expr(&row[slot])?;
                    match b {
                        // An explicit NULL on the rowid-alias PK auto-assigns,
                        // exactly like an omitted value — carried as Default and
                        // resolved to max(rowid)+1 at execution.
                        BExpr::Const(v) if v.is_null() && Some(ci as u16) == rowid_col => {
                            InsertSource::Default
                        }
                        BExpr::Const(v) => {
                            // On a column that CONVERTS on store (#113) sqlite's
                            // affinity is the WHOLE rule, and `coerce_const`
                            // must not run on top of it: its float→int step is
                            // looser than `sqlite3VdbeIntegerAffinity`, and
                            // stacking the two stored `'-9223372036854775809'`
                            // as the clamped i64 MIN where sqlite keeps the
                            // real. A boolean is folded to its integer first,
                            // because sqlite has no boolean storage class for
                            // an affinity to see.
                            let v = if col.converts_on_store() {
                                col.store(match v {
                                    Value::Bool(b) if binder.sqlite_dialect() => {
                                        Value::Int(b as i64)
                                    }
                                    other => other,
                                })
                            } else {
                                coerce_const(v, col.ty, binder.sqlite_dialect())
                            };
                            if v.is_null() && !col.nullable {
                                // Third of the three bind-time NOT NULL sites,
                                // and it carries the same class for the same
                                // reason: `IntegrityError`, not
                                // `OperationalError`.
                                return Err(Error::NotNullViolation {
                                    table: table.name.clone(),
                                    column: col.name.clone(),
                                });
                            }
                            if !v.fits(col.ty) {
                                // Name the reason when `coerce_const` TRIED and
                                // the value itself was the obstacle — sqlite
                                // STRICT refuses this one too ("cannot store
                                // REAL value in INT column"), so saying which
                                // real is the useful half of the message.
                                let why = match (&v, col.ty) {
                                    (Value::Float(_), ColumnType::Int64) => {
                                        " — it is not exactly an integer in the int64 range"
                                    }
                                    _ => "",
                                };
                                return Err(bind_err(format!(
                                    "value of type {} cannot be inserted into column `{}` of type {}{}",
                                    v.type_name(),
                                    col.name,
                                    col.ty,
                                    why
                                )));
                            }
                            InsertSource::Const(push_plan_const(consts, v)?)
                        }
                        BExpr::Param(i) => {
                            // A column that CONVERTS on store pins nothing: the
                            // bound value goes through sqlite's store affinity
                            // and is validated AFTER conversion, which is the
                            // whole point (`INSERT INTO t(name) VALUES (?)`
                            // with an integer bound stores `'1'`). See
                            // `ColumnDef::converts_on_store`.
                            if !col.converts_on_store() {
                                match binder.param_types[i as usize] {
                                    None => binder.param_types[i as usize] = Some(col.ty),
                                    Some(t) if t == col.ty => {}
                                    Some(t) => {
                                        return Err(bind_err(format!(
                                            "parameter ${} already inferred as {t}, but column `{}` is {}",
                                            i + 1,
                                            col.name,
                                            col.ty
                                        )))
                                    }
                                }
                            }
                            InsertSource::Param(i)
                        }
                        // Expression cell (Django bulk_create Now(), arithmetic,
                        // scalar subquery in VALUES, multi-row with mixed
                        // literals). Evaluated over the dual row at insert time.
                        other => {
                            let program = compile_program(&other)?;
                            // A HOST-registered UDF used to be refused here:
                            // `build_insert_row` had no host scope, so the cell
                            // would compile and then fail at execute with
                            // "host function …() is not in scope", and refusing
                            // early at least said why. It has one now, and
                            // `stmt_has_host_call` walks these cells, so the
                            // plan both reaches the executor with the table in
                            // scope and stays OUT of the shared registry — the
                            // second half being the one that matters, since a
                            // connection-local call must never be published.
                            // Django's `bulk_create` over a column whose value
                            // is a registered function is this exact shape.
                            InsertSource::Expr(program)
                        }
                    }
                }
            };
            sources.push(src);
        }
        rows.push(sources);
    }

    // RLS gate on the new row (INSERT ignores USING; WITH CHECK is the sole gate).
    let with_check = write_check(&mut binder, catalog, table_id, &table.name, PolicyCmd::Insert)?
        .map(|b| compile_program(&b))
        .transpose()?;

    // §6.5: ON CONFLICT is refused on an RLS table rather than silently
    // weakening the classification-oracle closure. `with_check.is_some()` is
    // exact — the planner emits it iff RLS is enabled on the target — and it is
    // the same signal hide_constraint_variant keys off, so the two cannot drift.
    if !matches!(s.on_conflict, ast::OnConflict::Error) && with_check.is_some() {
        return Err(bind_err(format!(
            "ON CONFLICT is not supported on `{}`, which has row-level security \
             (DESIGN-MULTIDB §6.5): a silent skip would tell the caller that a row it \
             cannot see exists, and DO UPDATE would overwrite one. Use a plain INSERT and \
             handle the rejection.",
            table.name
        )));
    }

    let on_conflict = plan_on_conflict(&s.on_conflict, &mut binder, table, table_id, consts)?;
    let returning = plan_returning(&s.returning, &mut binder, table)?;

    let (mut param_types, mut context_keys, mut list_keys) = binder.into_parts();
    // Merge the source query's inferences into this statement's (INSERT …
    // SELECT). Param spaces are shared (both planned against the same
    // `n_params`), so unify element-wise; a genuine type conflict is an error.
    if from_select.is_some() {
        if param_types.len() < sel_ptypes.len() {
            param_types.resize(sel_ptypes.len(), None);
        }
        for (i, t) in sel_ptypes.into_iter().enumerate() {
            if let Some(t) = t {
                match param_types[i] {
                    None => param_types[i] = Some(t),
                    Some(existing) if existing == t => {}
                    Some(existing) => {
                        return Err(bind_err(format!(
                            "parameter ${} used as both {existing} and {t}",
                            i + 1
                        )))
                    }
                }
            }
        }
        context_keys.extend(sel_ctx);
        context_keys.sort();
        context_keys.dedup();
        list_keys.extend(sel_list);
        // A source item that is a BARE parameter lands in exactly one column,
        // so it takes that column's type — the same inference the VALUES path
        // makes for `VALUES (?)`. Without it a `VALUES (LOWER(?), ?)` rewritten
        // to a SELECT would leave the second parameter untyped, and a caller
        // that relies on the declared type to convert (the C-API shim turns an
        // `int` 0/1 into a `bool` when the plan says the column is one) would
        // send the wrong storage class. Only fills a slot nothing else typed.
        if let Some(sel) = &s.select {
            if let Some(items) = &sel.items {
                for (slot, (item, _)) in items.iter().enumerate() {
                    let (Some(&col), ast::Expr::Param(i)) = (listed.get(slot), item) else {
                        continue;
                    };
                    let i = *i as usize;
                    if param_types.get(i).is_some_and(|t| t.is_none()) {
                        param_types[i] = Some(table.columns[col as usize].ty);
                    }
                }
            }
        }
    }
    Ok((
        PlanStmt::Insert {
            table: table_id,
            rows,
            from_select,
            with_check,
            on_conflict,
            returning,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        sel_subplans,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_update(
    s: &ast::UpdateStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: Dialect,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    // Subqueries in the WHERE lift out FIRST (#97), exactly as they do for a
    // SELECT: each becomes a `SubPlan` + reserved slot and is replaced by
    // `Param(slot)`, so everything below sees only a parameter.
    let (where_ast, subplans, slot_types, _slot_colls) = lift_where(
        s.where_clause.as_ref(), table, &s.table, schema, n_params, catalog, mode, host_udfs,
        row_count, consts, "UPDATE",
    )?;
    let eff_params = n_params + subplans.len() as u16;
    let mut binder = Binder::new(table, eff_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }

    // sqlite (R-34751-18293): when a column is assigned more than once, all but
    // the RIGHTMOST occurrence is ignored — not evaluated, not type-checked. So
    // resolve each name, then keep only the last expression per column (in
    // first-appearance order) and bind/compile just those. The executor
    // evaluates every SET against the OLD row, so collapsing duplicates never
    // changes a surviving assignment.
    let mut last_expr: Vec<Option<&ast::Expr>> = vec![None; table.columns.len()];
    let mut order: Vec<u16> = Vec::new();
    for (name, expr) in &s.set {
        let idx = table.column_index(name).ok_or_else(|| {
            bind_err(format!("unknown column `{name}` in table `{}`", table.name))
        })?;
        if table.is_pk_column(idx) {
            return Err(bind_err(format!(
                "cannot update primary key column `{name}`"
            )));
        }
        // sqlite: "cannot UPDATE generated column".
        if table.columns[idx as usize].generated.is_some() {
            return Err(bind_err(format!(
                "cannot UPDATE generated column `{name}`"
            )));
        }
        if last_expr[idx as usize].is_none() {
            order.push(idx);
        }
        last_expr[idx as usize] = Some(expr);
    }
    let mut set = Vec::with_capacity(order.len());
    for idx in order {
        let col = &table.columns[idx as usize];
        let expr = last_expr[idx as usize].expect("recorded in order");
        let b = binder.bind_assign(expr, col)?;
        set.push((idx, compile_program(&b)?));
    }

    let bound_where = where_ast
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    // The UPDATE policy restricts the target set, and (read-via-write) the
    // SELECT policy is folded in too — see `read_policy`.
    let policy = read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Update)?;
    // A CORRELATED subplan's slot is empty until the row is in hand, so its
    // conjuncts must NOT reach the access-path extractor — an `IndexPoint` on
    // an unfilled slot would be a wrong answer, not a refusal. Split first,
    // exactly as the SELECT path does, and hand the correlated half to the
    // executor as a per-row residual.
    let correlated: Vec<bool> = subplans.iter().map(|s| !s.outer_args.is_empty()).collect();
    let (gather_pred, corr_pred) =
        subquery::split_correlated(merge_and(bound_where, policy), n_params, &correlated);
    let (access, residual) = extract_access(gather_pred, table, consts)?;
    let filter = residual.map(|e| compile_program(&e)).transpose()?;
    let post_filter = corr_pred.map(|e| compile_program(&e)).transpose()?;

    // WITH CHECK gates the post-image (falls back to USING per PG rule).
    let with_check = write_check(&mut binder, catalog, table_id, &table.name, PolicyCmd::Update)?
        .map(|b| compile_program(&b))
        .transpose()?;
    let returning = plan_returning(&s.returning, &mut binder, table)?;
    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Update {
            returning,
            table: table_id,
            access,
            filter,
            post_filter,
            set,
            with_check,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        subplans,
    ))
}

/// `lift_where`'s result: `None` expression when there was no WHERE at all.
type OptLiftedWhere = (Option<ast::Expr>, Vec<SubPlan>, Vec<Ty>, Vec<Option<Collation>>);

/// The shared WHERE-lift both write planners run (#97). `None` / subquery-free
/// WHERE clauses take the zero-cost path and produce no subplans at all.
#[allow(clippy::too_many_arguments)]
fn lift_where(
    where_clause: Option<&ast::Expr>,
    table: &TableDef,
    table_name: &str,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: Dialect,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
    op: &str,
) -> Result<OptLiftedWhere> {
    match where_clause {
        Some(w) if subquery::expr_has_subquery(w) => {
            let (e, subs, tys, colls) = subquery::lift_dml_where(
                w, table, table_name, schema, n_params, catalog, mode, host_udfs, row_count, consts, op,
            )?;
            Ok((Some(e), subs, tys, colls))
        }
        other => Ok((other.cloned(), Vec::new(), Vec::new(), Vec::new())),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_delete(
    s: &ast::DeleteStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: Dialect,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    // Subqueries in the WHERE lift out FIRST (#97) — see `plan_update`.
    let (where_ast, subplans, slot_types, _slot_colls) = lift_where(
        s.where_clause.as_ref(), table, &s.table, schema, n_params, catalog, mode, host_udfs,
        row_count, consts, "DELETE",
    )?;
    let eff_params = n_params + subplans.len() as u16;
    let mut binder = Binder::new(table, eff_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }
    let bound_where = where_ast
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    let policy = read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Delete)?;
    // A CORRELATED subplan's slot is empty until the row is in hand, so its
    // conjuncts must NOT reach the access-path extractor — an `IndexPoint` on
    // an unfilled slot would be a wrong answer, not a refusal. Split first,
    // exactly as the SELECT path does, and hand the correlated half to the
    // executor as a per-row residual.
    let correlated: Vec<bool> = subplans.iter().map(|s| !s.outer_args.is_empty()).collect();
    let (gather_pred, corr_pred) =
        subquery::split_correlated(merge_and(bound_where, policy), n_params, &correlated);
    let (access, residual) = extract_access(gather_pred, table, consts)?;
    let filter = residual.map(|e| compile_program(&e)).transpose()?;
    let post_filter = corr_pred.map(|e| compile_program(&e)).transpose()?;
    let returning = plan_returning(&s.returning, &mut binder, table)?;
    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Delete {
            returning,
            table: table_id,
            access,
            filter,
            post_filter,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        subplans,
    ))
}

/// Fold a constant into its column's type where the conversion is EXACT.
///
/// Two cases: the Int -> Float widening, and (sqlite dialect only) the
/// int/bool bridge — sqlite has no boolean type, so `INSERT INTO t (flag)
/// VALUES (1)` on a `BooleanField` is the shape Django emits. Only the
/// literals 0 and 1 convert: sqlite would store `2` in its `bool` column and
/// hand `2` back, which mpedb's rigid `Bool` cannot represent, so anything
/// else falls through to the `fits` check and is refused rather than guessed.
/// A `Bool` constant landing in an int64 column goes the other way and is
/// always exact — that IS sqlite's storage (`TRUE` -> 1).
///
/// The Float -> Int direction (task #74) is sqlite's INTEGER affinity: a real
/// is stored as an integer exactly when the round trip is lossless, so
/// `INSERT INTO t (i) VALUES (8.0)` stores the integer 8 in sqlite and here.
/// `8.5` is NOT converted — it falls through to the caller's `fits` check and
/// is refused, because sqlite would keep the real in its typeless column and
/// mpedb's rigid int64 cannot. Both dialects agree on the lossless case, so
/// unlike the bool bridges this one is not dialect-gated.
fn coerce_const(v: Value, ty: ColumnType, sqlite: bool) -> Value {
    match (&v, ty) {
        (Value::Int(i), ColumnType::Float64) => Value::Float(*i as f64),
        (Value::Float(f), ColumnType::Int64) => match exact_float_as_int(*f) {
            Some(i) => Value::Int(i),
            None => v,
        },
        (Value::Int(i @ (0 | 1)), ColumnType::Bool) if sqlite => Value::Bool(*i == 1),
        (Value::Bool(b), ColumnType::Int64) if sqlite => Value::Int(*b as i64),
        _ => v,
    }
}

pub(super) fn push_plan_const(consts: &mut Vec<Value>, v: Value) -> Result<u16> {
    if consts.len() >= u16::MAX as usize {
        return Err(bind_err("statement has too many constants"));
    }
    consts.push(v);
    Ok((consts.len() - 1) as u16)
}

