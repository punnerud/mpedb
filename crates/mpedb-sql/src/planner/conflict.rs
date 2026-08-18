//! ON CONFLICT and RETURNING compilation (moved verbatim from `planner/mod.rs`).

use super::*;

/// Compile an `ON CONFLICT` action.
///
/// The target must be a key the executor can PROBE: the primary key, or one
/// secondary UNIQUE column. That is the real constraint, and it is not
/// stylistic — the executor has to find the row you conflicted with, and
/// guessing ("you said (email), I will upsert on the PK anyway") updates the
/// wrong row silently.
///
/// A multi-column non-PK target has no probe even when each column is unique
/// on its own: `get_by_index` takes one value, and "unique together" is not
/// something the schema can declare.
pub(super) fn plan_on_conflict(
    oc: &ast::OnConflict,
    binder: &mut Binder,
    table: &mpedb_types::TableDef,
    _table_id: u32,
    _consts: &mut Vec<Value>,
) -> Result<PlanOnConflict> {
    let (target, set, where_clause) = match oc {
        ast::OnConflict::Error => return Ok(PlanOnConflict::Error),
        ast::OnConflict::DoNothing => return Ok(PlanOnConflict::DoNothing),
        // `INSERT OR REPLACE` is a first-class executor variant: it deletes
        // every existing row the proposed row would conflict with (on the PK OR
        // any secondary UNIQUE index) then inserts — sqlite's real
        // delete-on-any-unique semantics, which a single PK-keyed upsert cannot
        // express (it only covers PK conflicts and updates one row).
        ast::OnConflict::Replace => return Ok(PlanOnConflict::Replace),
        ast::OnConflict::DoUpdate {
            target,
            set,
            where_clause,
        } => (target, set, where_clause),
    };
    let mut tcols = Vec::with_capacity(target.len());
    for name in target {
        let i = table
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| bind_err(format!("unknown conflict-target column `{name}`")))?;
        tcols.push(i as u16);
    }
    let Some(probe) = conflict_probe_opt(table, &tcols) else {
        let pk_names: Vec<&str> = table
            .primary_key
            .iter()
            .map(|i| table.columns[*i as usize].name.as_str())
            .collect();
        let mut usable = vec![format!("({})", pk_names.join(", "))];
        // Only UNIQUE indexes can witness a conflict; a non-unique index
        // never can (several rows may share the values).
        for ix in table
            .indexes
            .iter()
            .filter(|ix| ix.unique && !ix.has_expression_part())
        {
            // An expression index cannot be an ON CONFLICT target — there are no
            // column NAMES to write in the conflict clause — and its ordinals
            // carry a sentinel that panics if used to index `columns`.
            let names: Vec<&str> = ix
                .columns
                .iter()
                .map(|&c| table.columns[c as usize].name.as_str())
                .collect();
            usable.push(format!("({})", names.join(", ")));
        }
        return Err(bind_err(format!(
            "ON CONFLICT ({}) is not supported: the target must be a key this can probe to \
             find the row you conflicted with — the primary key, or a UNIQUE index's \
             column set. Usable here: {}.",
            target.join(", "),
            usable.join(", ")
        )));
    };
    // `excluded.<c>` is in scope only here, and binds to Col(n + i): the
    // executor runs these over [existing ‖ proposed].
    binder.set_allow_excluded(true);
    let mut bset = Vec::with_capacity(set.len());
    for (name, e) in set {
        let i = table
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| bind_err(format!("unknown column `{name}` in DO UPDATE SET")))?;
        if table.columns[i].generated.is_some() {
            binder.set_allow_excluded(false);
            return Err(bind_err(format!(
                "cannot UPDATE generated column `{name}`"
            )));
        }
        let (b, ty) = binder.bind_expr(e)?;
        if let Some(t) = ty {
            // Same rule as `bind_assign`: `any` accepts every typed value, and
            // so does a column that converts on store (#113) — the conversion
            // runs at write time and the engine validates its result.
            if t != table.columns[i].ty
                && table.columns[i].ty != ColumnType::Any
                && !table.columns[i].converts_on_store()
            {
                binder.set_allow_excluded(false);
                return Err(bind_err(format!(
                    "cannot assign {t} to column `{name}` of type {}",
                    table.columns[i].ty
                )));
            }
        }
        bset.push((i as u16, compile_program(&b)?));
    }
    let filter = match where_clause {
        Some(w) => {
            let (b, ty) = binder.bind_expr(w)?;
            // A boolean context like any other: a non-bool is truthy-tested the
            // way sqlite does (`Binder::coerce_bool_ctx`).
            let (b, ty) = match binder.coerce_bool_ctx(b, ty) {
                Ok(v) => v,
                Err(e) => {
                    binder.set_allow_excluded(false);
                    return Err(e);
                }
            };
            if !matches!(ty, Some(ColumnType::Bool) | None) {
                binder.set_allow_excluded(false);
                return Err(bind_err("ON CONFLICT ... WHERE must be a bool condition"));
            }
            Some(compile_program(&b)?)
        }
        None => None,
    };
    binder.set_allow_excluded(false);
    Ok(PlanOnConflict::DoUpdate {
        target: tcols,
        probe,
        set: bset,
        filter,
    })
}

/// Compile a `RETURNING` clause into a projection over the written row.
pub(super) fn plan_returning(
    r: &crate::ast::ReturningClause,
    binder: &mut Binder,
    table: &mpedb_types::TableDef,
) -> Result<Option<Vec<Projection>>> {
    let Some(items) = r else { return Ok(None) };
    let Some(items) = items else {
        // RETURNING * — the VISIBLE columns only; the hidden implicit rowid is
        // never surfaced by a star (#94), exactly as `SELECT *`.
        return Ok(Some(
            (0..table.visible_column_count() as u16).map(Projection::Column).collect(),
        ));
    };
    let mut proj = Vec::with_capacity(items.len());
    for (e, alias) in items {
        // An explicit alias RENAMES the output column, so it cannot take the
        // bare-column path: `Projection::Column` reports the TABLE's name for
        // it, which is the one thing the alias was written to change. The
        // expression path carries a name, so it is the one that can honour it —
        // and `RETURNING t.id, t.id AS id__1` needs exactly that, since the two
        // items are the same column told apart only by the alias.
        if let Some(name) = alias {
            let (b, _) = binder.bind_expr(e)?;
            proj.push(Projection::Expr {
                program: compile_program(&b)?,
                name: name.clone(),
            });
            continue;
        }
        // A bare column, in either spelling. `t.id` is `Expr::Qualified` and
        // used to fall to the expression arm below, where the display name is
        // `?column?` — so a client that read the result by column name found
        // nothing, for a RETURNING list that named its columns perfectly well.
        let bare = match e {
            ast::Expr::Col(name, _) => Some(name),
            ast::Expr::Qualified(q, name) if mpedb_types::ident::ident_eq(q, &table.name) => {
                Some(name)
            }
            _ => None,
        };
        match bare {
            Some(name) => {
                let i = table
                    .columns
                    .iter()
                    .position(|c| mpedb_types::ident::ident_eq(&c.name, name))
                    .ok_or_else(|| bind_err(format!("unknown column `{name}` in RETURNING")))?;
                proj.push(Projection::Column(i as u16));
            }
            None => {
                let (b, _) = binder.bind_expr(e)?;
                proj.push(Projection::Expr {
                    program: compile_program(&b)?,
                    name: render_expr_name(e),
                });
            }
        }
    }
    Ok(Some(proj))
}

/// A display name for a RETURNING expression item that named no alias.
///
/// PostgreSQL's own fallback for an unnameable expression is `?column?`, and a
/// bare column keeps its own name. A QUALIFIED column never reaches here — it
/// resolves to `Projection::Column` above, which is where its name comes from.
fn render_expr_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Col(c, _) => c.clone(),
        _ => "?column?".to_string(),
    }
}

