//! `CREATE VIEW` flattening (#73, DESIGN-VIEW.md). A view is a named `SELECT`;
//! a query that names it in `FROM` is rewritten to read the view's base table
//! with the view's `WHERE` merged in. mpedb has no derived-table machinery, so
//! flattening reuses the ordinary single-table planner and adds zero plan
//! surface — at the cost of a bounded grammar (simple views only; the rest are
//! refused at reference time, never answered wrongly).
//!
//! **V1 grammar (provably correct without expression remapping):** the view body
//! is `SELECT <items> FROM <one base table or view> [WHERE <p>]`, no JOIN /
//! GROUP BY / HAVING / DISTINCT / LIMIT / ORDER BY / aggregate, and `<items>` is
//! `*` or a list of **bare columns with no alias**. So an exposed column name is
//! always its own base column name — the outer query's references need no
//! rewriting; only `SELECT * FROM v` expands to the view's column list.

use crate::ast::{Expr, SelectStmt, Stmt};
use crate::parser::parse_statement;
use mpedb_types::{Error, Result};
use std::collections::HashMap;

/// View name → its `SELECT` source text (re-parsed at reference time).
pub type ViewCatalog = HashMap<String, String>;

const MAX_VIEW_DEPTH: usize = 16;

fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Rewrite every view reference in `stmt` into its base table. No-op when the
/// catalog is empty or the statement names no view.
pub fn inline_views(stmt: &mut Stmt, views: &ViewCatalog) -> Result<()> {
    if views.is_empty() {
        return Ok(());
    }
    match stmt {
        Stmt::Select(s) => flatten_select(s, views, 0),
        Stmt::Compound(c) => {
            for arm in &mut c.arms {
                flatten_select(arm, views, 0)?;
            }
            Ok(())
        }
        // A view target in a write is not supported (mpedb has no updatable
        // views); the write planner will reject the unknown table cleanly, but
        // catch an explicit view name here with a clearer message.
        Stmt::Insert(i) => refuse_view_target(&i.table, views, "INSERT"),
        Stmt::Update(u) => refuse_view_target(&u.table, views, "UPDATE"),
        Stmt::Delete(d) => refuse_view_target(&d.table, views, "DELETE"),
        _ => Ok(()),
    }
}

fn refuse_view_target(table: &str, views: &ViewCatalog, op: &str) -> Result<()> {
    if views.contains_key(table) {
        return Err(bind_err(format!("cannot {op} a view (`{table}`)")));
    }
    Ok(())
}

fn flatten_select(s: &mut SelectStmt, views: &ViewCatalog, depth: usize) -> Result<()> {
    if depth > MAX_VIEW_DEPTH {
        return Err(bind_err("view nesting too deep (recursive view?)"));
    }
    // A view in a JOIN position is not supported in V1.
    for j in &s.joins {
        if views.contains_key(&j.table) {
            return Err(bind_err(format!(
                "view `{}` used in a JOIN is not supported yet",
                j.table
            )));
        }
    }
    // Recurse into subqueries first (they may reference views too).
    if let Some(items) = &mut s.items {
        for (e, _) in items {
            flatten_expr(e, views, depth)?;
        }
    }
    if let Some(w) = &mut s.where_clause {
        flatten_expr(w, views, depth)?;
    }
    for g in &mut s.group_by {
        flatten_expr(g, views, depth)?;
    }
    if let Some(h) = &mut s.having {
        flatten_expr(h, views, depth)?;
    }
    for (e, _) in &mut s.order_by {
        flatten_expr(e, views, depth)?;
    }

    // The main FROM: if it is a view, splice its body in.
    let Some(tname) = s.table.clone() else {
        return Ok(());
    };
    let Some(view_src) = views.get(&tname) else {
        return Ok(());
    };
    if s.alias.is_some() {
        return Err(bind_err(format!(
            "aliasing a view (`{tname}`) is not supported yet"
        )));
    }
    // Parse + recursively flatten the view body.
    let (view_stmt, _explain, n_params) = parse_statement(view_src)
        .map_err(|e| bind_err(format!("view `{tname}` body does not parse: {e}")))?;
    if n_params != 0 {
        return Err(bind_err(format!("view `{tname}` body must not use parameters")));
    }
    let Stmt::Select(mut vsel) = view_stmt else {
        return Err(bind_err(format!("view `{tname}` body is not a simple SELECT")));
    };
    flatten_select(&mut vsel, views, depth + 1)?;
    check_simple(&vsel, &tname)?;

    // Splice: read the view's base, AND-merge the view's WHERE, and expand a
    // `SELECT *` over the view to the view's own column list.
    s.table = vsel.table.take();
    s.where_clause = merge_where(vsel.where_clause.take(), s.where_clause.take());
    if s.items.is_none() {
        // `SELECT * FROM v`: return the view's columns, not the base's.
        s.items = vsel.items.take();
    }
    Ok(())
}

/// Recurse into any subquery `SelectStmt` carried by an expression.
fn flatten_expr(e: &mut Expr, views: &ViewCatalog, depth: usize) -> Result<()> {
    match e {
        Expr::Subquery(s) | Expr::Exists(s, _) => flatten_select(s, views, depth),
        Expr::InSubquery(lhs, s, _) => {
            flatten_expr(lhs, views, depth)?;
            flatten_select(s, views, depth)
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => flatten_expr(a, views, depth),
        Expr::Binary(_, a, b) | Expr::Like(a, b) => {
            flatten_expr(a, views, depth)?;
            flatten_expr(b, views, depth)
        }
        Expr::InList(a, list, _) => {
            flatten_expr(a, views, depth)?;
            for item in list {
                flatten_expr(item, views, depth)?;
            }
            Ok(())
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                flatten_expr(c, views, depth)?;
                flatten_expr(r, views, depth)?;
            }
            if let Some(e) = else_ {
                flatten_expr(e, views, depth)?;
            }
            Ok(())
        }
        Expr::Coalesce(items) | Expr::Func(_, items) => {
            for item in items {
                flatten_expr(item, views, depth)?;
            }
            Ok(())
        }
        Expr::Agg(_, Some(arg), _) => flatten_expr(arg, views, depth),
        _ => Ok(()),
    }
}

/// The V1 flattenable grammar. Anything outside it is refused (never answered).
fn check_simple(v: &SelectStmt, name: &str) -> Result<()> {
    let bad = |what: &str| {
        Err(bind_err(format!(
            "view `{name}` uses {what}, which is not supported yet (only a \
             single-table projection/filter view can be flattened)"
        )))
    };
    if v.table.is_none() {
        return bad("a FROM-less body");
    }
    if !v.joins.is_empty() {
        return bad("a JOIN");
    }
    if v.distinct {
        return bad("DISTINCT");
    }
    if !v.group_by.is_empty() || v.having.is_some() {
        return bad("GROUP BY/HAVING");
    }
    if !v.order_by.is_empty() {
        return bad("ORDER BY");
    }
    if v.limit.is_some() || v.offset.is_some() {
        return bad("LIMIT/OFFSET");
    }
    // Items must be `*` or bare, un-aliased columns (so exposed name == base
    // column name and no expression remapping is needed).
    if let Some(items) = &v.items {
        for (e, alias) in items {
            if alias.is_some() {
                return bad("an aliased/renamed column");
            }
            if !matches!(e, Expr::Col(_)) {
                return bad("a computed (non-column) projection");
            }
        }
    }
    Ok(())
}

fn merge_where(a: Option<Expr>, b: Option<Expr>) -> Option<Expr> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(Expr::Binary(
            crate::ast::BinOp::And,
            Box::new(x),
            Box::new(y),
        )),
    }
}
