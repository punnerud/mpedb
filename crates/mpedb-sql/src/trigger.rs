//! Compiling a trigger's SQL body and `WHEN` predicate against its target
//! table (DESIGN-TRIGGERS §3.4).
//!
//! A trigger body references the changing row through the `NEW.<col>` (and,
//! in later stages, `OLD.<col>`) pseudo-relation. Rather than teach the whole
//! planner a new column namespace, we rewrite each `NEW.<col>` reference into a
//! reserved **parameter** slot before planning: the body compiles to an
//! ordinary [`CompiledPlan`] whose leading parameters are the `NEW` columns, in
//! reference order, and the executor fills those slots from the inserted row at
//! fire time. This is the same "a name that resolves to a slot filled from a row
//! image at execution" shape `excluded.<col>` and `current_setting()` already
//! use — and because the rewrite is structural, an unhandled `NEW` position
//! fails *closed* at bind time (a clean refusal) rather than misbinding.
//!
//! v1 (stage 1) fires only `AFTER INSERT FOR EACH ROW`, so only `NEW` is in
//! scope; `OLD`, subqueries, and query parameters are named refusals.

use crate::ast::{self, Expr, Stmt};
use crate::binder::{self, Binder};
use crate::plan::dual_def;
use crate::policy::PolicyCatalog;
use crate::{parser, planner};
use mpedb_types::{Error, ExprProgram, Result, Schema, TableDef};

fn trg_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Compile a trigger's `BEGIN … END` body (a single INSERT/UPDATE/DELETE in v1)
/// against `target` — the table the trigger fires on. Returns the body plan and
/// the `NEW`-column map: body parameter slot `i` is filled from the inserted
/// row's column `new_map[i]` at fire time.
pub fn compile_trigger_body(
    body_sql: &str,
    target: &TableDef,
    schema: &Schema,
) -> Result<(crate::CompiledPlan, Vec<u16>)> {
    let (mut stmt, is_explain, n_params, ctes) = parser::parse_statement_ctes(body_sql)?;
    if is_explain {
        return Err(trg_err("EXPLAIN is not allowed in a trigger body"));
    }
    if n_params > 0 {
        return Err(trg_err(
            "query parameters ($1 / ?) are not allowed in a trigger body",
        ));
    }
    if !ctes.is_empty() {
        return Err(trg_err("WITH (CTE) is not supported in a trigger body yet"));
    }
    let mut new_map: Vec<u16> = Vec::new();
    rewrite_new_in_stmt(&mut stmt, target, &mut new_map)?;
    let plan = planner::plan_statement(&stmt, schema, new_map.len() as u16, &PolicyCatalog::empty())?;
    Ok((plan, new_map))
}

/// Compile a trigger's `WHEN (<cond>)` predicate against `target`. The predicate
/// may reference only `NEW.<col>` and constants (no bare columns, no subqueries,
/// no parameters). Returns the boolean program and the `NEW`-column map, filled
/// the same way as the body's.
pub fn compile_trigger_when(
    when_src: &str,
    target: &TableDef,
) -> Result<(ExprProgram, Vec<u16>)> {
    let (mut expr, n_params) = parser::parse_expr_only(when_src)?;
    if n_params > 0 {
        return Err(trg_err(
            "query parameters ($1 / ?) are not allowed in a trigger WHEN",
        ));
    }
    let mut new_map: Vec<u16> = Vec::new();
    rewrite_new_in_expr(&mut expr, target, &mut new_map)?;
    // Bind against the zero-column dual table so any surviving bare column
    // reference fails to resolve — in a trigger WHEN a bare name is not the new
    // row, only `NEW.<col>` is. `allow_params = true` lets the rewritten `NEW`
    // slots (now parameters) bind; `current_setting()` is refused by the rewrite.
    let mut b = Binder::new(dual_def(), new_map.len() as u16, true);
    let bound = b.bind_predicate(&expr)?;
    let program = binder::compile_program(&bound)?;
    Ok((program, new_map))
}

/// Rewrite `NEW.<col>` into a reserved parameter over a whole body statement.
fn rewrite_new_in_stmt(s: &mut Stmt, target: &TableDef, map: &mut Vec<u16>) -> Result<()> {
    match s {
        Stmt::Insert(i) => {
            if let Some(sel) = &mut i.select {
                rewrite_new_in_select(sel, target, map)?;
            }
            for row in &mut i.rows {
                for e in row {
                    rewrite_new_in_expr(e, target, map)?;
                }
            }
            if let ast::OnConflict::DoUpdate { set, where_clause, .. } = &mut i.on_conflict {
                for (_, e) in set {
                    rewrite_new_in_expr(e, target, map)?;
                }
                if let Some(w) = where_clause {
                    rewrite_new_in_expr(w, target, map)?;
                }
            }
            rewrite_new_in_returning(&mut i.returning, target, map)?;
            Ok(())
        }
        Stmt::Update(u) => {
            for (_, e) in &mut u.set {
                rewrite_new_in_expr(e, target, map)?;
            }
            if let Some(w) = &mut u.where_clause {
                rewrite_new_in_expr(w, target, map)?;
            }
            rewrite_new_in_returning(&mut u.returning, target, map)
        }
        Stmt::Delete(d) => {
            if let Some(w) = &mut d.where_clause {
                rewrite_new_in_expr(w, target, map)?;
            }
            rewrite_new_in_returning(&mut d.returning, target, map)
        }
        _ => Err(trg_err(
            "a trigger body must be a single INSERT, UPDATE, or DELETE statement",
        )),
    }
}

/// Rewrite `NEW.<col>` across an `INSERT … SELECT` source (and, recursively, a
/// derived table). Any `NEW` in a position not reached here stays a
/// `Qualified("new", …)` and is refused by the binder — fail-closed.
fn rewrite_new_in_select(
    s: &mut ast::SelectStmt,
    target: &TableDef,
    map: &mut Vec<u16>,
) -> Result<()> {
    if let Some(items) = &mut s.items {
        for (e, _) in items {
            rewrite_new_in_expr(e, target, map)?;
        }
    }
    if let Some(w) = &mut s.where_clause {
        rewrite_new_in_expr(w, target, map)?;
    }
    for e in &mut s.group_by {
        rewrite_new_in_expr(e, target, map)?;
    }
    if let Some(h) = &mut s.having {
        rewrite_new_in_expr(h, target, map)?;
    }
    for (e, _) in &mut s.order_by {
        rewrite_new_in_expr(e, target, map)?;
    }
    for j in &mut s.joins {
        rewrite_new_in_expr(&mut j.on, target, map)?;
    }
    if let Some(d) = &mut s.from_derived {
        rewrite_new_in_select(d, target, map)?;
    }
    Ok(())
}

fn rewrite_new_in_returning(
    r: &mut Option<Option<Vec<Expr>>>,
    target: &TableDef,
    map: &mut Vec<u16>,
) -> Result<()> {
    if let Some(Some(items)) = r {
        for e in items {
            rewrite_new_in_expr(e, target, map)?;
        }
    }
    Ok(())
}

/// Rewrite every `NEW.<col>` in one expression into `Param(slot)`, appending the
/// column index to `map`. Refuses `OLD`, subqueries, `current_setting()`, and
/// pre-existing parameters — the v1 surface. Any `NEW` position not reached here
/// stays a `Qualified("new", …)` node and is refused by the binder ("no table
/// named `new`"), so an omission fails closed rather than silently.
fn rewrite_new_in_expr(e: &mut Expr, target: &TableDef, map: &mut Vec<u16>) -> Result<()> {
    match e {
        Expr::Qualified(qual, col) => {
            if qual.eq_ignore_ascii_case("new") {
                let idx = target.column_index(col).ok_or_else(|| {
                    trg_err(format!("unknown column `NEW.{col}` on table `{}`", target.name))
                })?;
                if map.len() >= u16::MAX as usize {
                    return Err(trg_err("too many NEW references in one trigger body"));
                }
                let slot = map.len() as u16;
                map.push(idx);
                *e = Expr::Param(slot);
            } else if qual.eq_ignore_ascii_case("old") {
                return Err(trg_err(
                    "OLD is only available in UPDATE/DELETE triggers (not yet supported)",
                ));
            }
            // else: an ordinary table/alias qualifier — the binder resolves it.
            Ok(())
        }
        Expr::Param(_) => Err(trg_err(
            "query parameters ($1 / ?) are not allowed in a trigger body/WHEN",
        )),
        Expr::Subquery(_) | Expr::Exists(..) | Expr::InSubquery(..) | Expr::InParamSlot(..) => {
            Err(trg_err(
                "subqueries are not supported in a trigger body/WHEN yet",
            ))
        }
        Expr::ContextRef(_) | Expr::InContext(..) => Err(trg_err(
            "current_setting() is not supported in a trigger body/WHEN yet",
        )),
        Expr::Lit(_) | Expr::Col(_) | Expr::Excluded(_) => Ok(()),
        Expr::Unary(_, a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::Agg(_, Some(a), _) => rewrite_new_in_expr(a, target, map),
        Expr::Agg(_, None, _) => Ok(()),
        Expr::Binary(_, a, b)
        | Expr::IsDistinct(a, b, _)
        | Expr::Like(a, b)
        | Expr::Glob(a, b, _) => {
            rewrite_new_in_expr(a, target, map)?;
            rewrite_new_in_expr(b, target, map)
        }
        Expr::InList(a, items, _) => {
            rewrite_new_in_expr(a, target, map)?;
            for it in items {
                rewrite_new_in_expr(it, target, map)?;
            }
            Ok(())
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                rewrite_new_in_expr(c, target, map)?;
                rewrite_new_in_expr(r, target, map)?;
            }
            if let Some(e2) = else_ {
                rewrite_new_in_expr(e2, target, map)?;
            }
            Ok(())
        }
        Expr::Func(_, args) | Expr::Coalesce(args) => {
            for a in args {
                rewrite_new_in_expr(a, target, map)?;
            }
            Ok(())
        }
    }
}
