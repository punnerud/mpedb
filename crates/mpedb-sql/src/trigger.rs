//! Compiling a trigger's SQL body and `WHEN` predicate against its target
//! table (DESIGN-TRIGGERS §3.4).
//!
//! A trigger body references the changing row through the `NEW.<col>` and
//! `OLD.<col>` pseudo-relations. Rather than teach the whole planner a new
//! column namespace, we rewrite each `NEW.<col>` / `OLD.<col>` reference into a
//! reserved **parameter** slot before planning: the body compiles to an
//! ordinary [`CompiledPlan`] whose leading parameters are the referenced
//! `NEW`/`OLD` columns, in reference order, and the executor fills those slots
//! from the row images at fire time. This is the same "a name that resolves to a
//! slot filled from a row image at execution" shape `excluded.<col>` and
//! `current_setting()` already use — and because the rewrite is structural, an
//! unhandled `NEW`/`OLD` position fails *closed* at bind time (a clean refusal)
//! rather than misbinding.
//!
//! Binding availability by event (sqlite's rule, DESIGN-TRIGGERS §1) is passed
//! in by the caller as `allow_new` / `allow_old`: INSERT has only `NEW`, DELETE
//! only `OLD`, UPDATE both. A reference to an unavailable side is a bind-time
//! refusal. Subqueries, aggregates over rows, and query parameters in a
//! body/WHEN are named refusals.

use crate::ast::{self, Expr, Stmt};
use crate::binder::{self, Binder};
use crate::plan::dual_def;
use crate::policy::PolicyCatalog;
use crate::token::{tokenize, Tok};
use crate::{parser, planner};
use mpedb_types::{Error, ExprProgram, Result, Schema, TableDef};

/// Which row image a reserved trigger slot is filled from at fire time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowSide {
    /// The post-image (`NEW.<col>`): the inserted row, or an UPDATE's new row.
    New,
    /// The pre-image (`OLD.<col>`): the deleted row, or an UPDATE's old row.
    Old,
}

/// One reserved body/WHEN slot: filled from `side`'s image, column `col`.
/// Slots are numbered in reference order across both sides (parameter `i` is
/// `map[i]`).
pub type RowMap = Vec<(RowSide, u16)>;

/// Which pseudo-relations are in scope while rewriting a trigger body/WHEN.
struct RowScope<'t> {
    target: &'t TableDef,
    allow_new: bool,
    allow_old: bool,
}

fn trg_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Compile a trigger's `BEGIN <stmt>; … END` body against `target` — the table
/// the trigger fires on. `allow_new`/`allow_old` gate the `NEW`/`OLD`
/// pseudo-relations per the event (DESIGN-TRIGGERS §1). The body may hold a
/// SEQUENCE of INSERT/UPDATE/DELETE statements (DESIGN-TRIGGERS stage 3), fired
/// in order on the same txn; each compiles to its own plan and row-slot map (body
/// parameter slot `i` of statement `n` is filled from `map[n][i].0`'s image,
/// column `map[n][i].1`, at fire time). Returns one `(plan, map)` per statement,
/// in body order.
pub fn compile_trigger_body(
    body_sql: &str,
    target: &TableDef,
    schema: &Schema,
    allow_new: bool,
    allow_old: bool,
) -> Result<Vec<(crate::CompiledPlan, RowMap)>> {
    let stmt_srcs = split_body_statements(body_sql)?;
    if stmt_srcs.is_empty() {
        return Err(trg_err("a trigger body must contain at least one statement"));
    }
    let mut out = Vec::with_capacity(stmt_srcs.len());
    for stmt_src in &stmt_srcs {
        // No host UDFs in a trigger body: it is compiled once and stored in the
        // catalog, so it must not depend on one connection's registrations.
        let (mut stmt, is_explain, n_params, ctes) = parser::parse_statement_ctes(stmt_src, &[], &[], &crate::binder::OpSet::default())?;
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
        let scope = RowScope { target, allow_new, allow_old };
        let mut map: RowMap = Vec::new();
        rewrite_row_in_stmt(&mut stmt, &scope, &mut map)?;
        // The GROUP BY dialect is irrelevant here: a trigger body is DML and
        // refuses subqueries, so it never reaches aggregate planning where a bare
        // column could appear. Compile under the lenient default.
        let plan = planner::plan_statement(
            &stmt,
            schema,
            map.len() as u16,
            &PolicyCatalog::empty(),
            mpedb_types::BareGroupBy::Sqlite,
            // A trigger body cannot call host UDFs (stage 1): it is compiled at
            // CREATE TRIGGER time, out of any connection's UDF scope.
            &crate::binder::HostUdfSet::default(),
            // A trigger body is compiled once at CREATE TRIGGER time and its
            // bytes are stored in the catalog, so it must NOT depend on the
            // row counts of the moment — the plan would then differ from every
            // later re-derivation of the same trigger. Zero source: the MPEE
            // solver's structural term still applies, its size-ranking term
            // does not (design/DESIGN-MPEE-SOLVER.md §6).
            crate::planner::NO_ROW_COUNTS,
        )?;
        out.push((plan, map));
    }
    Ok(out)
}

/// Split a trigger's `BEGIN … END` body source into its top-level statements at
/// statement-separating semicolons. Tokenizing (rather than a naive `char` split)
/// means a `;` inside a string literal is never mistaken for a separator, and
/// empty fragments (from `;;` or a leading/trailing `;`) are dropped. A `;` token
/// is always a statement boundary — it never nests inside an expression — so no
/// depth tracking is needed here (the `CASE … END` balancing lives in the parser
/// that captured this source).
fn split_body_statements(body_sql: &str) -> Result<Vec<String>> {
    let toks = tokenize(body_sql)?;
    let mut stmts = Vec::new();
    let mut start = 0usize;
    for sp in &toks {
        if sp.tok == Tok::Semicolon {
            let frag = body_sql.get(start..sp.pos).unwrap_or("").trim();
            if !frag.is_empty() {
                stmts.push(frag.to_string());
            }
            start = sp.pos + 1; // one byte past the ASCII ';'
        }
    }
    let frag = body_sql.get(start..).unwrap_or("").trim();
    if !frag.is_empty() {
        stmts.push(frag.to_string());
    }
    Ok(stmts)
}

/// Compile a trigger's `WHEN (<cond>)` predicate against `target`. The predicate
/// may reference only `NEW.<col>` / `OLD.<col>` (per `allow_new`/`allow_old`) and
/// constants (no bare columns, no subqueries, no parameters). Returns the boolean
/// program and the row-slot map, filled the same way as the body's.
pub fn compile_trigger_when(
    when_src: &str,
    target: &TableDef,
    allow_new: bool,
    allow_old: bool,
) -> Result<(ExprProgram, RowMap)> {
    let (mut expr, n_params) = parser::parse_expr_only(when_src)?;
    if n_params > 0 {
        return Err(trg_err(
            "query parameters ($1 / ?) are not allowed in a trigger WHEN",
        ));
    }
    let scope = RowScope { target, allow_new, allow_old };
    let mut map: RowMap = Vec::new();
    rewrite_row_in_expr(&mut expr, &scope, &mut map)?;
    // Bind against the zero-column dual table so any surviving bare column
    // reference fails to resolve — in a trigger WHEN a bare name is not a row,
    // only `NEW.<col>` / `OLD.<col>` are. `allow_params = true` lets the
    // rewritten row slots (now parameters) bind; `current_setting()` is refused
    // by the rewrite.
    let mut b = Binder::new(dual_def(), map.len() as u16, true);
    let bound = b.bind_predicate(&expr)?;
    let program = binder::compile_program(&bound)?;
    Ok((program, map))
}

/// Rewrite `NEW.<col>` / `OLD.<col>` into reserved parameters over a whole body
/// statement.
fn rewrite_row_in_stmt(s: &mut Stmt, scope: &RowScope, map: &mut RowMap) -> Result<()> {
    match s {
        Stmt::Insert(i) => {
            if let Some(sel) = &mut i.select {
                rewrite_row_in_select(sel, scope, map)?;
            }
            for row in &mut i.rows {
                for e in row {
                    rewrite_row_in_expr(e, scope, map)?;
                }
            }
            if let ast::OnConflict::DoUpdate { set, where_clause, .. } = &mut i.on_conflict {
                for (_, e) in set {
                    rewrite_row_in_expr(e, scope, map)?;
                }
                if let Some(w) = where_clause {
                    rewrite_row_in_expr(w, scope, map)?;
                }
            }
            rewrite_row_in_returning(&mut i.returning, scope, map)?;
            Ok(())
        }
        Stmt::Update(u) => {
            for (_, e) in &mut u.set {
                rewrite_row_in_expr(e, scope, map)?;
            }
            if let Some(w) = &mut u.where_clause {
                rewrite_row_in_expr(w, scope, map)?;
            }
            rewrite_row_in_returning(&mut u.returning, scope, map)
        }
        Stmt::Delete(d) => {
            if let Some(w) = &mut d.where_clause {
                rewrite_row_in_expr(w, scope, map)?;
            }
            rewrite_row_in_returning(&mut d.returning, scope, map)
        }
        _ => Err(trg_err(
            "a trigger body must be a single INSERT, UPDATE, or DELETE statement",
        )),
    }
}

/// Rewrite `NEW`/`OLD` across an `INSERT … SELECT` source (and, recursively, a
/// derived table). Any `NEW`/`OLD` in a position not reached here stays a
/// `Qualified(…)` and is refused by the binder — fail-closed.
fn rewrite_row_in_select(s: &mut ast::SelectStmt, scope: &RowScope, map: &mut RowMap) -> Result<()> {
    if let Some(items) = &mut s.items {
        for (e, _) in items {
            rewrite_row_in_expr(e, scope, map)?;
        }
    }
    if let Some(w) = &mut s.where_clause {
        rewrite_row_in_expr(w, scope, map)?;
    }
    for e in &mut s.group_by {
        rewrite_row_in_expr(e, scope, map)?;
    }
    if let Some(h) = &mut s.having {
        rewrite_row_in_expr(h, scope, map)?;
    }
    for (e, _) in &mut s.order_by {
        rewrite_row_in_expr(e, scope, map)?;
    }
    for j in &mut s.joins {
        rewrite_row_in_expr(&mut j.on, scope, map)?;
    }
    if let Some(d) = &mut s.from_derived {
        match d.as_mut() {
            ast::SubqueryBody::Select(b) => rewrite_row_in_select(b, scope, map)?,
            ast::SubqueryBody::Compound(c) => {
                for arm in &mut c.arms {
                    rewrite_row_in_select(arm, scope, map)?;
                }
            }
        }
    }
    Ok(())
}

fn rewrite_row_in_returning(
    r: &mut Option<Option<Vec<Expr>>>,
    scope: &RowScope,
    map: &mut RowMap,
) -> Result<()> {
    if let Some(Some(items)) = r {
        for e in items {
            rewrite_row_in_expr(e, scope, map)?;
        }
    }
    Ok(())
}

/// Bind one `NEW`/`OLD` qualified reference to a fresh reserved slot, checking the
/// side is in scope and the column exists on the target.
fn bind_row_ref(scope: &RowScope, side: RowSide, col: &str, map: &mut RowMap) -> Result<Expr> {
    let allowed = match side {
        RowSide::New => scope.allow_new,
        RowSide::Old => scope.allow_old,
    };
    if !allowed {
        return Err(trg_err(match side {
            RowSide::New => "NEW is not available in a DELETE trigger",
            RowSide::Old => "OLD is only available in UPDATE/DELETE triggers",
        }));
    }
    let name = match side {
        RowSide::New => "NEW",
        RowSide::Old => "OLD",
    };
    let idx = scope.target.column_index(col).ok_or_else(|| {
        trg_err(format!(
            "unknown column `{name}.{col}` on table `{}`",
            scope.target.name
        ))
    })?;
    if map.len() >= u16::MAX as usize {
        return Err(trg_err("too many NEW/OLD references in one trigger body"));
    }
    let slot = map.len() as u16;
    map.push((side, idx));
    Ok(Expr::Param(slot))
}

/// Rewrite every `NEW.<col>` / `OLD.<col>` in one expression into `Param(slot)`,
/// appending its `(side, column)` to `map`. Refuses subqueries,
/// `current_setting()`, and pre-existing parameters — the v1 surface. Any
/// `NEW`/`OLD` position not reached here stays a `Qualified(…)` node and is
/// refused by the binder ("no table named `new`/`old`"), so an omission fails
/// closed rather than silently.
fn rewrite_row_in_expr(e: &mut Expr, scope: &RowScope, map: &mut RowMap) -> Result<()> {
    match e {
        Expr::Qualified(qual, col) => {
            if qual.eq_ignore_ascii_case("new") {
                *e = bind_row_ref(scope, RowSide::New, col, map)?;
            } else if qual.eq_ignore_ascii_case("old") {
                *e = bind_row_ref(scope, RowSide::Old, col, map)?;
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
        Expr::Window { .. } => Err(trg_err(
            "window functions are not supported in a trigger body/WHEN yet",
        )),
        Expr::ContextRef(_) | Expr::InContext(..) => Err(trg_err(
            "current_setting() is not supported in a trigger body/WHEN yet",
        )),
        Expr::Lit(_) | Expr::Col(_) | Expr::Excluded(_) => Ok(()),
        Expr::Unary(_, a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::Collate(a, _) => rewrite_row_in_expr(a, scope, map),
        // An aggregate's argument AND its `FILTER (WHERE …)` both read the row;
        // rewrite `new`/`old` references in each.
        Expr::Agg(_, arg, _, filter, extra) => {
            if let Some(a) = arg {
                rewrite_row_in_expr(a, scope, map)?;
            }
            for x in extra {
                rewrite_row_in_expr(x, scope, map)?;
            }
            if let Some(f) = filter {
                rewrite_row_in_expr(f, scope, map)?;
            }
            Ok(())
        }
        Expr::Binary(_, a, b)
        | Expr::IsDistinct(a, b, _)
        | Expr::Like(a, b, _)
        | Expr::Match(a, b)
        | Expr::Glob(a, b, _)
        | Expr::Regexp(a, b, _) => {
            rewrite_row_in_expr(a, scope, map)?;
            rewrite_row_in_expr(b, scope, map)
        }
        Expr::InList(a, items, _) => {
            rewrite_row_in_expr(a, scope, map)?;
            for it in items {
                rewrite_row_in_expr(it, scope, map)?;
            }
            Ok(())
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                rewrite_row_in_expr(c, scope, map)?;
                rewrite_row_in_expr(r, scope, map)?;
            }
            if let Some(e2) = else_ {
                rewrite_row_in_expr(e2, scope, map)?;
            }
            Ok(())
        }
        Expr::Func(_, args) | Expr::Coalesce(args) | Expr::RowValue(args) => {
            for a in args {
                rewrite_row_in_expr(a, scope, map)?;
            }
            Ok(())
        }
    }
}
