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

/// View name → its `SELECT` source text (re-parsed at reference time). The same
/// shape backs a statement-scoped CTE scope (CTE name → CTE body text).
pub type ViewCatalog = HashMap<String, String>;

const MAX_VIEW_DEPTH: usize = 16;

fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Rewrite every view reference in `stmt` into its base table, and flatten any
/// derived table `FROM (SELECT …)` (#74) onto its base. Equivalent to
/// [`inline_views_with_ctes`] with an empty CTE scope.
pub fn inline_views(stmt: &mut Stmt, views: &ViewCatalog) -> Result<()> {
    inline_views_with_ctes(stmt, views, &ViewCatalog::new())
}

/// Like [`inline_views`], but with a SECOND, statement-scoped catalog of CTE
/// bodies (`WITH c AS (…) …`, #CTE) kept DISTINCT from the persistent views.
///
/// The two catalogs splice differently on purpose. A persistent view is spliced
/// by the *strip-name* path (which refuses an alias), so only unqualified refs
/// work — unchanged. A CTE reference is spliced by the same *keep-alias*
/// machinery `flatten_derived` uses: the reference name (or an explicit
/// `FROM c AS x` alias) becomes the base's alias and the body's own qualifier is
/// remapped onto it, so `c.col` / `x.col` resolve. A CTE shadows a same-named
/// persistent view for this one statement; stored-view behavior is undisturbed.
///
/// The walk always runs — a derived table must be flattened even with two empty
/// catalogs, or the planner would silently ignore it — but it is a cheap no-op
/// when the statement names no view/CTE and carries no derived table.
pub fn inline_views_with_ctes(
    stmt: &mut Stmt,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
) -> Result<()> {
    match stmt {
        Stmt::Select(s) => flatten_select(s, views, ctes, 0),
        Stmt::Compound(c) => {
            for arm in &mut c.arms {
                flatten_select(arm, views, ctes, 0)?;
            }
            Ok(())
        }
        // A view/CTE target in a write is not supported (mpedb has no updatable
        // views); the write planner will reject the unknown table cleanly, but
        // catch an explicit view/CTE name here with a clearer message.
        Stmt::Insert(i) => {
            refuse_view_target(&i.table, views, ctes, "INSERT")?;
            // `INSERT … SELECT … FROM <view|cte|derived>` must flatten its
            // source SELECT too (the target check above only guards the write
            // target). Without this, a CTE/view named in the source is left
            // unresolved for the planner.
            if let Some(sel) = &mut i.select {
                flatten_select(sel, views, ctes, 0)?;
            }
            Ok(())
        }
        Stmt::Update(u) => refuse_view_target(&u.table, views, ctes, "UPDATE"),
        Stmt::Delete(d) => refuse_view_target(&d.table, views, ctes, "DELETE"),
        _ => Ok(()),
    }
}

fn refuse_view_target(
    table: &str,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    op: &str,
) -> Result<()> {
    if ctes.contains_key(table) {
        return Err(bind_err(format!("cannot {op} a CTE (`{table}`)")));
    }
    if views.contains_key(table) {
        return Err(bind_err(format!("cannot {op} a view (`{table}`)")));
    }
    Ok(())
}

fn flatten_select(
    s: &mut SelectStmt,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    if depth > MAX_VIEW_DEPTH {
        return Err(bind_err("view nesting too deep (recursive view?)"));
    }
    // A view or CTE in a JOIN position is not supported yet (the keep-alias
    // splice only rewrites the main FROM; a joined CTE would need its body's
    // WHERE folded into the join's ON). Refuse cleanly, never answer wrongly.
    for j in &s.joins {
        if ctes.contains_key(&j.table) {
            return Err(bind_err(format!(
                "CTE `{}` used in a JOIN is not supported yet",
                j.table
            )));
        }
        if views.contains_key(&j.table) {
            return Err(bind_err(format!(
                "view `{}` used in a JOIN is not supported yet",
                j.table
            )));
        }
    }
    // Recurse into subqueries first (they may reference views/CTEs too).
    if let Some(items) = &mut s.items {
        for (e, _) in items {
            flatten_expr(e, views, ctes, depth)?;
        }
    }
    if let Some(w) = &mut s.where_clause {
        flatten_expr(w, views, ctes, depth)?;
    }
    for g in &mut s.group_by {
        flatten_expr(g, views, ctes, depth)?;
    }
    if let Some(h) = &mut s.having {
        flatten_expr(h, views, ctes, depth)?;
    }
    for (e, _) in &mut s.order_by {
        flatten_expr(e, views, ctes, depth)?;
    }

    // Derived table `FROM (SELECT …) t` (#74): splice its simple body onto the
    // base table BEFORE the view/CTE splice below, so a base uncovered here is a
    // real table (any view inside the body was already resolved by the recursive
    // flatten). Runs only when the parser produced a `from_derived`.
    if s.from_derived.is_some() {
        flatten_derived(s, views, ctes, depth)?;
    }

    let Some(tname) = s.table.clone() else {
        return Ok(());
    };
    // A CTE reference shadows a same-named persistent view for this statement.
    // Splice it with the keep-alias machinery so `cte.col` / `FROM cte AS x`
    // (`x.col`) resolve — distinct from the view path below, which strips names.
    if ctes.contains_key(&tname) {
        return flatten_cte(s, &tname, views, ctes, depth);
    }
    // The main FROM: if it is a view, splice its body in (strip-name path).
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
    flatten_select(&mut vsel, views, ctes, depth + 1)?;
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

/// Splice a `FROM cte [AS alias]` reference onto the CTE body's base table,
/// KEEPING the reference name (or the explicit alias) as the base's alias —
/// the same keep-alias splice `flatten_derived` performs for a derived table.
/// This is what makes `cte.col` and `x.col` (`FROM cte AS x`) resolve, and what
/// separates the CTE path from the persistent-view path (which strips the name
/// and refuses an alias). Only the same simple projection/filter body is
/// flattenable; anything else is refused by `check_simple`.
fn flatten_cte(
    s: &mut SelectStmt,
    tname: &str,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    let cte_src = ctes.get(tname).expect("caller checked the CTE exists");
    // The name the outer query addresses the CTE by: an explicit `AS x`, else
    // the CTE name itself. Kept as the base's alias so qualified refs resolve.
    let ref_alias = s.alias.clone().unwrap_or_else(|| tname.to_string());
    let (cte_stmt, _explain, n_params) = parse_statement(cte_src)
        .map_err(|e| bind_err(format!("CTE `{tname}` body does not parse: {e}")))?;
    if n_params != 0 {
        return Err(bind_err(format!("CTE `{tname}` body must not use parameters")));
    }
    let Stmt::Select(mut body) = cte_stmt else {
        return Err(bind_err(format!("CTE `{tname}` body is not a simple SELECT")));
    };
    // The body itself may reference a view or another CTE.
    flatten_select(&mut body, views, ctes, depth + 1)?;
    check_simple(&body, tname)?;

    // The body exposes its columns under its own source name (its inner alias
    // if any, else its base table name). Rename that to the reference alias so
    // the body's WHERE reads under the same name the outer query uses.
    let from_name = body
        .alias
        .clone()
        .or_else(|| body.table.clone())
        .expect("check_simple guarantees a FROM table");
    let mut body_where = body.where_clause.take();
    if let Some(w) = &mut body_where {
        rename_qualifier(w, &from_name, &ref_alias);
    }

    s.table = body.table.take();
    // Keep the reference alias as the base's alias — this is what lets outer
    // `cte.col` / `x.col` refs resolve, and it shadows the base's real name.
    s.alias = Some(ref_alias);
    s.where_clause = merge_where(body_where, s.where_clause.take());
    if s.items.is_none() {
        // `SELECT * FROM cte`: expose the body's own column list, not a fresh
        // `*` over the base (which could carry columns the body hid).
        s.items = body.items.take();
    }
    Ok(())
}

/// Splice a derived table `FROM (SELECT …) t` onto its base (#74, Stage B).
///
/// A derived table is a view whose body is written inline and whose alias is
/// intrinsic — `t` is how the outer query addresses the body's columns. So,
/// unlike a stored view (which V1 refuses to alias), the alias is KEPT: the
/// base is read `FROM base AS t`, which makes every outer `t.col` resolve to a
/// base column. Only the same simple projection/filter body a view allows is
/// flattenable; the body's own references are remapped to the single alias `t`
/// so the collapsed query reads under one name.
fn flatten_derived(
    s: &mut SelectStmt,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    if depth > MAX_VIEW_DEPTH {
        return Err(bind_err("derived-table nesting too deep"));
    }
    let mut body = s.from_derived.take().expect("caller checked from_derived is Some");
    // A derived table must be named: its alias is how the outer query addresses
    // the exposed columns (PostgreSQL requires it; sqlite usually carries it).
    let Some(dalias) = s.alias.clone() else {
        return Err(bind_err(
            "a derived table `FROM (SELECT …)` must have an alias",
        ));
    };
    // The body itself may reference a view, a CTE, or a nested derived table.
    flatten_select(&mut body, views, ctes, depth + 1)?;
    check_simple(&body, &dalias)?;

    // The body exposes its columns under its own source name — its inner alias
    // if it has one, else its base table name. Rename that to the derived alias
    // so the body's WHERE reads under the same `t` the outer query uses.
    let from_name = body
        .alias
        .clone()
        .or_else(|| body.table.clone())
        .expect("check_simple guarantees a FROM table");
    let mut body_where = body.where_clause.take();
    if let Some(w) = &mut body_where {
        rename_qualifier(w, &from_name, &dalias);
    }

    s.table = body.table.take();
    // Keep the derived alias as the base's alias — this is what lets outer
    // `t.col` refs resolve, and it shadows the base's real name (PG's rule).
    s.alias = Some(dalias);
    s.where_clause = merge_where(body_where, s.where_clause.take());
    if s.items.is_none() {
        // `SELECT * FROM (…) t`: expose the body's own column list, not a
        // fresh `*` over the base (which could carry columns the body hid).
        s.items = body.items.take();
    }
    Ok(())
}

/// Rewrite every `from.col` qualifier to `to.col` in place. Used to collapse a
/// derived table's body onto the single outer alias; only the qualifier changes,
/// never the column, and bare [`Expr::Col`] refs (no qualifier) are untouched.
fn rename_qualifier(e: &mut Expr, from: &str, to: &str) {
    match e {
        Expr::Qualified(q, _) => {
            if q == from {
                *q = to.to_string();
            }
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => rename_qualifier(a, from, to),
        Expr::Binary(_, a, b) | Expr::Like(a, b) => {
            rename_qualifier(a, from, to);
            rename_qualifier(b, from, to);
        }
        Expr::IsDistinct(a, b, _) => {
            rename_qualifier(a, from, to);
            rename_qualifier(b, from, to);
        }
        Expr::InList(a, list, _) => {
            rename_qualifier(a, from, to);
            for item in list {
                rename_qualifier(item, from, to);
            }
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                rename_qualifier(c, from, to);
                rename_qualifier(r, from, to);
            }
            if let Some(x) = else_ {
                rename_qualifier(x, from, to);
            }
        }
        Expr::Coalesce(items) | Expr::Func(_, items) => {
            for item in items {
                rename_qualifier(item, from, to);
            }
        }
        Expr::InParamSlot(a, _, _) | Expr::InContext(a, _, _) => rename_qualifier(a, from, to),
        Expr::Agg(_, Some(arg), _) => rename_qualifier(arg, from, to),
        // A subquery in the body opens its own scope; refuse-by-check_simple
        // keeps aggregate/correlated bodies out, and a plain uncorrelated
        // subquery does not see the derived alias, so it is left as-is.
        Expr::Subquery(_) | Expr::Exists(_, _) | Expr::InSubquery(_, _, _) => {}
        Expr::Lit(_)
        | Expr::Param(_)
        | Expr::Col(_)
        | Expr::ContextRef(_)
        | Expr::Excluded(_)
        | Expr::Agg(_, None, _) => {}
    }
}

/// Recurse into any subquery `SelectStmt` carried by an expression.
fn flatten_expr(
    e: &mut Expr,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    match e {
        Expr::Subquery(s) | Expr::Exists(s, _) => flatten_select(s, views, ctes, depth),
        Expr::InSubquery(lhs, s, _) => {
            flatten_expr(lhs, views, ctes, depth)?;
            flatten_select(s, views, ctes, depth)
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => {
            flatten_expr(a, views, ctes, depth)
        }
        Expr::Binary(_, a, b) | Expr::Like(a, b) => {
            flatten_expr(a, views, ctes, depth)?;
            flatten_expr(b, views, ctes, depth)
        }
        Expr::IsDistinct(a, b, _) => {
            flatten_expr(a, views, ctes, depth)?;
            flatten_expr(b, views, ctes, depth)
        }
        Expr::InList(a, list, _) => {
            flatten_expr(a, views, ctes, depth)?;
            for item in list {
                flatten_expr(item, views, ctes, depth)?;
            }
            Ok(())
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                flatten_expr(c, views, ctes, depth)?;
                flatten_expr(r, views, ctes, depth)?;
            }
            if let Some(e) = else_ {
                flatten_expr(e, views, ctes, depth)?;
            }
            Ok(())
        }
        Expr::Coalesce(items) | Expr::Func(_, items) => {
            for item in items {
                flatten_expr(item, views, ctes, depth)?;
            }
            Ok(())
        }
        Expr::Agg(_, Some(arg), _) => flatten_expr(arg, views, ctes, depth),
        _ => Ok(()),
    }
}

/// The V1 flattenable grammar. Anything outside it is refused (never answered).
fn check_simple(v: &SelectStmt, name: &str) -> Result<()> {
    let bad = |what: &str| {
        Err(bind_err(format!(
            "`{name}` uses {what}, which is not supported yet (only a \
             single-table projection/filter source can be flattened)"
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
