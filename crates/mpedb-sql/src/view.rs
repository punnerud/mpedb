//! `CREATE VIEW` flattening (#73, design/DESIGN-VIEW.md). A view is a named `SELECT`;
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

use crate::ast::{CompoundStmt, Expr, JoinClause, JoinKind, SelectStmt, Stmt, SubqueryBody};
use crate::parser::parse_statement;
use crate::plan::SetOp;
use mpedb_types::{fold_ident, ident_eq, Error, Result};
use std::collections::{HashMap, HashSet};

/// View name → its `SELECT` source text (re-parsed at reference time). The same
/// shape backs a statement-scoped CTE scope (CTE name → CTE body text).
///
/// Keys are the name as DECLARED (`CREATE VIEW MiXeD` keys on `MiXeD`), because
/// that is the spelling every consumer reports back. Lookups therefore cannot
/// use `HashMap::get`, whose hashing is byte-exact — they go through
/// [`catalog_get`] / [`catalog_has`], which fold ASCII case like every other
/// identifier. Both catalogs are small (a handful of views; ≤ 32 CTEs), so the
/// linear scan costs nothing on the compile path.
pub type ViewCatalog = HashMap<String, String>;

/// The body of the view/CTE named `name`, matched ASCII-case-insensitively.
pub fn catalog_get<'a>(cat: &'a ViewCatalog, name: &str) -> Option<&'a String> {
    cat.iter().find(|(k, _)| ident_eq(k, name)).map(|(_, v)| v)
}

/// Is there a view/CTE named `name` (ASCII-case-insensitively)?
pub fn catalog_has(cat: &ViewCatalog, name: &str) -> bool {
    catalog_get(cat, name).is_some()
}

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
        Stmt::Compound(c) => flatten_compound(c, views, ctes, 0),
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
    if catalog_has(ctes, table) {
        return Err(bind_err(format!("cannot {op} a CTE (`{table}`)")));
    }
    if catalog_has(views, table) {
        return Err(bind_err(format!("cannot {op} a view (`{table}`)")));
    }
    Ok(())
}

/// Validate the ordering of a `WITH` clause's CTE definitions BEFORE flattening.
///
/// A CTE body may reference an EARLIER CTE (a backward reference), which resolves
/// during flattening because both live in the statement's flat CTE scope. It may
/// NOT reference itself, a LATER CTE (a forward reference), or form a cycle:
/// each is refused here with a clear message. This keeps `WITH a AS (SELECT …
/// FROM b), b AS (…) …` (forward) and `WITH a AS (SELECT … FROM a) …` (self) out
/// of the flat scope, where the former would resolve leniently and the latter
/// would only trip the depth guard with a vaguer message. It is stricter than
/// sqlite (which accepts a non-cyclic forward reference) but matches its
/// self/cyclic-reference refusal, and is never a wrong answer. Duplicate CTE
/// names are refused too (sqlite: "duplicate WITH table name").
///
/// A body that fails to parse here is skipped, not refused: an UNUSED broken
/// body stays a safe leniency, and a USED one is refused at flatten time.
pub fn validate_cte_order(ctes: &[(String, String)]) -> Result<()> {
    // Keyed on the FOLDED name: `WITH c AS (…), C AS (…)` is one duplicate name,
    // not two CTEs (sqlite: `duplicate WITH table name`). The sets are lookup
    // keys only; the diagnostics below quote the names as written.
    let all: HashSet<String> = ctes.iter().map(|(n, _)| fold_ident(n)).collect();
    // Names of the strictly-preceding CTEs — the only ones a body may reference.
    let mut preceding: HashSet<String> = HashSet::new();
    for (name, body) in ctes {
        if let Ok((stmt, _explain, _n)) = parse_statement(body) {
            let mut refs = Vec::new();
            match &stmt {
                Stmt::Select(s) => collect_source_names(s, &mut refs),
                Stmt::Compound(c) => {
                    for arm in &c.arms {
                        collect_source_names(arm, &mut refs);
                    }
                }
                _ => {}
            }
            for r in &refs {
                let rk = fold_ident(r);
                if all.contains(&rk) && !preceding.contains(&rk) {
                    return Err(bind_err(format!(
                        "CTE `{name}` references `{r}`, which is not defined before \
                         it (self, forward, or cyclic CTE references are not supported)"
                    )));
                }
            }
        }
        if !preceding.insert(fold_ident(name)) {
            return Err(bind_err(format!("duplicate CTE name `{name}` in WITH")));
        }
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
    // A CTE named in a JOIN operand is spliced onto its base table with the
    // SAME keep-alias derived-table logic used for the main FROM: the base is
    // read `JOIN <base> AS <c>` and the CTE body's WHERE is AND-merged into the
    // join's ON (`flatten_cte_join`). A view in a JOIN keeps the old refusal —
    // views use the strip-name splice and have no alias to resolve `v.col`
    // against, so folding them into a join is out of scope here.
    let outer_has_items = s.items.is_some();
    for j in &mut s.joins {
        if catalog_has(ctes, &j.table) {
            flatten_cte_join(j, views, ctes, depth, outer_has_items)?;
        } else if catalog_has(views, &j.table) {
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
    if catalog_has(ctes, &tname) {
        return flatten_cte(s, &tname, views, ctes, depth);
    }
    // The main FROM: if it is a view, splice its body in (strip-name path).
    let Some(view_src) = catalog_get(views, &tname) else {
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
    let cte_src = catalog_get(ctes, tname).expect("caller checked the CTE exists");
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
    // FROM-less body (`WITH one AS (SELECT 1) SELECT * FROM one`): there is no
    // base table to splice onto. Collapse the outer onto the dual select that
    // the body already is — CPython's `test_cursor_description_cte_simple`
    // and every constant-row CTE. A table-backed body keeps the keep-alias path.
    if body.table.is_none() {
        return flatten_cte_fromless(s, tname, body, &ref_alias);
    }
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
        if s.joins.is_empty() {
            // `SELECT * FROM cte`: expose the body's own column list, not a
            // fresh `*` over the base (which could carry columns the body hid).
            s.items = body.items.take();
        } else if body.items.is_some() {
            // `SELECT * FROM cte JOIN …` with a PROJECTING body: `*` must expand
            // to the CTE's columns PLUS the joined tables', which the single body
            // item-list cannot express — installing it would silently drop the
            // join's columns. Refuse rather than answer wrongly. (A `SELECT *`
            // body is fine: its `*` correctly expands over every source below.)
            return Err(bind_err(format!(
                "`SELECT *` over a JOIN with a projecting CTE (`{tname}`) is not \
                 supported yet; list the output columns explicitly"
            )));
        }
        // else: `SELECT *`-bodied CTE with a join — leave `*` to expand over all
        // sources; the base carries every column the body exposed.
    }
    Ok(())
}

/// Collapse `FROM <fromless-cte>` onto the dual SELECT the body already is.
///
/// A FROM-less CTE body (`SELECT 1`, `SELECT 1 AS a, 2 AS b`) is one synthetic
/// row with a projection — exactly what the dual path already plans. There is
/// no base table to keep-alias onto, so the outer becomes the body: table
/// cleared, items installed (or outer items rewritten against the body's
/// aliases), WHERE clauses merged. Joins against a constant-row CTE are
/// refused rather than answered with a silent cross product of the wrong shape.
fn flatten_cte_fromless(
    s: &mut SelectStmt,
    tname: &str,
    mut body: SelectStmt,
    ref_alias: &str,
) -> Result<()> {
    // Same residual grammar as `check_simple`, minus the FROM-less + computed
    // projection bans (those are the whole point of this path).
    let mut bad: Vec<&str> = Vec::new();
    if !body.joins.is_empty() {
        bad.push("a JOIN");
    }
    if body.distinct {
        bad.push("DISTINCT");
    }
    if !body.group_by.is_empty() || body.having.is_some() {
        bad.push("GROUP BY/HAVING");
    }
    if !body.order_by.is_empty() {
        bad.push("ORDER BY");
    }
    if body.limit.is_some() || body.offset.is_some() {
        bad.push("LIMIT/OFFSET");
    }
    if let Some(items) = &body.items {
        if items.iter().any(|(e, _)| expr_aggregates(e)) {
            bad.push("an aggregate");
        }
    } else {
        return Err(bind_err(format!(
            "CTE `{tname}` body is `SELECT *` without a FROM clause — no columns"
        )));
    }
    if !bad.is_empty() {
        return Err(bind_err(format!(
            "`{tname}` uses a FROM-less body + {}, which is not supported yet \
             (only a constant-row projection is flattenable)",
            bad.join(" + ")
        )));
    }
    if !s.joins.is_empty() {
        return Err(bind_err(format!(
            "a FROM-less CTE (`{tname}`) cannot appear in a JOIN yet"
        )));
    }

    let body_items = body.items.take().expect("checked above");
    // Map exposed name → expression, for rewriting outer projections / WHERE
    // that name the CTE's columns (`SELECT x FROM one` after `SELECT 1 AS x`).
    // Unaliased items use the same display name the dual planner would give
    // them (`1`, `1+1`, …) so `SELECT "1" FROM one` also resolves.
    let mut by_name: HashMap<String, Expr> = HashMap::new();
    for (e, alias) in &body_items {
        let name = alias
            .clone()
            .unwrap_or_else(|| fromless_item_name(e));
        // First wins — a duplicate alias in the body is a later bind error on
        // the dual path, not something to paper over here.
        by_name.entry(fold_ident(&name)).or_insert_with(|| e.clone());
    }

    if s.items.is_none() {
        // `SELECT * FROM cte` → the body's own projection (and its aliases).
        s.items = Some(body_items);
    } else {
        // Rewrite outer items against the body's exposed names.
        for (e, _) in s.items.as_mut().expect("is_some") {
            rewrite_cte_cols(e, &by_name, tname, ref_alias)?;
        }
    }
    if let Some(w) = &mut s.where_clause {
        rewrite_cte_cols(w, &by_name, tname, ref_alias)?;
    }
    // Body WHERE (if any) is already over dual/aliases; merge as-is. Outer
    // WHERE was rewritten above to the same expressions.
    s.where_clause = merge_where(body.where_clause.take(), s.where_clause.take());
    s.table = None;
    s.alias = None;
    Ok(())
}

/// Display name of an unaliased FROM-less projection item — matches what the
/// dual planner puts in `cursor.description` (`1`, `1+1`, a bare col name, …).
fn fromless_item_name(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => match v {
            mpedb_types::Value::Int(i) => i.to_string(),
            mpedb_types::Value::Float(f) => {
                // Same short form the dual planner's program render uses for
                // whole-number floats: keep a trailing `.0` only when needed.
                let s = f.to_string();
                s
            }
            mpedb_types::Value::Text(t) => t.clone(),
            mpedb_types::Value::Null => "NULL".into(),
            mpedb_types::Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            other => format!("{other:?}"),
        },
        Expr::Col(n) => n.clone(),
        // Fall back to a stable placeholder; callers that need the exact dual
        // name go through an explicit alias.
        _ => "?column?".into(),
    }
}

/// Replace `Col(name)` / `Qualified(cte|alias, name)` with the CTE body's
/// expression for that name. Unknown names stay so the binder reports them.
fn rewrite_cte_cols(
    e: &mut Expr,
    by_name: &HashMap<String, Expr>,
    tname: &str,
    ref_alias: &str,
) -> Result<()> {
    match e {
        Expr::Col(n) => {
            if let Some(repl) = by_name.get(&fold_ident(n)) {
                *e = repl.clone();
            }
            Ok(())
        }
        Expr::Qualified(q, n) => {
            if ident_eq(q, tname) || ident_eq(q, ref_alias) {
                if let Some(repl) = by_name.get(&fold_ident(n)) {
                    *e = repl.clone();
                    return Ok(());
                }
            }
            // Recurse into children only when we did not replace the node.
            Ok(())
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Collate(a, _) => {
            rewrite_cte_cols(a, by_name, tname, ref_alias)
        }
        Expr::Binary(_, a, b)
        | Expr::Like(a, b, _)
        | Expr::Match(a, b)
        | Expr::IsDistinct(a, b, _)
        | Expr::Glob(a, b, _)
        | Expr::Regexp(a, b, _) => {
            rewrite_cte_cols(a, by_name, tname, ref_alias)?;
            rewrite_cte_cols(b, by_name, tname, ref_alias)
        }
        Expr::InList(a, list, _) => {
            rewrite_cte_cols(a, by_name, tname, ref_alias)?;
            for item in list {
                rewrite_cte_cols(item, by_name, tname, ref_alias)?;
            }
            Ok(())
        }
        Expr::InParamSlot(a, _, _) | Expr::InContext(a, _, _) => {
            rewrite_cte_cols(a, by_name, tname, ref_alias)
        }
        Expr::InSubquery(a, _, _) => rewrite_cte_cols(a, by_name, tname, ref_alias),
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                rewrite_cte_cols(c, by_name, tname, ref_alias)?;
                rewrite_cte_cols(r, by_name, tname, ref_alias)?;
            }
            if let Some(x) = else_ {
                rewrite_cte_cols(x, by_name, tname, ref_alias)?;
            }
            Ok(())
        }
        Expr::Coalesce(items) | Expr::Func(_, items) | Expr::RowValue(items) => {
            for item in items {
                rewrite_cte_cols(item, by_name, tname, ref_alias)?;
            }
            Ok(())
        }
        Expr::Agg(_, arg, _, filter, extra) => {
            if let Some(a) = arg {
                rewrite_cte_cols(a, by_name, tname, ref_alias)?;
            }
            for x in extra {
                rewrite_cte_cols(x, by_name, tname, ref_alias)?;
            }
            if let Some(f) = filter {
                rewrite_cte_cols(f, by_name, tname, ref_alias)?;
            }
            Ok(())
        }
        Expr::Window {
            arg,
            extra_args,
            spec,
            ..
        } => {
            if let Some(a) = arg {
                rewrite_cte_cols(a, by_name, tname, ref_alias)?;
            }
            for x in extra_args {
                rewrite_cte_cols(x, by_name, tname, ref_alias)?;
            }
            // Partition/order keys may name the CTE's columns too.
            for e in &mut spec.partition_by {
                rewrite_cte_cols(e, by_name, tname, ref_alias)?;
            }
            for (e, _) in &mut spec.order_by {
                rewrite_cte_cols(e, by_name, tname, ref_alias)?;
            }
            Ok(())
        }
        Expr::Lit(_)
        | Expr::Param(_)
        | Expr::ContextRef(_)
        | Expr::Excluded(_)
        | Expr::Subquery(_)
        | Expr::Exists(_, _) => Ok(()),
    }
}

/// Splice a CTE named in a JOIN operand (`… JOIN c ON p`) onto its base table,
/// reusing the keep-alias derived-table logic: the base is read under the
/// reference name (or an explicit `AS x`) and the CTE body's WHERE is AND-merged
/// into the join's ON. So `c.col` in the ON / projection / outer WHERE resolves,
/// exactly as it does for a CTE in the main FROM.
///
/// **Soundness.** Merging the CTE's WHERE into the ON is only correct when the
/// CTE is NOT a preserved join side: for an INNER join, or the optional (right)
/// side of a LEFT join, filtering the CTE's rows before the join is equivalent
/// to filtering them in the ON. On the preserved side of a RIGHT/FULL join it is
/// not (it would resurrect rows the body filtered out, NULL-extended), so those
/// are refused. A projecting CTE body under an outer `SELECT *` is also refused:
/// the join's `*` would expand to the base's full column list, exposing columns
/// the body hid. Both are clean refusals, never a wrong answer.
fn flatten_cte_join(
    j: &mut JoinClause,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
    outer_has_items: bool,
) -> Result<()> {
    let tname = j.table.clone();
    if !matches!(j.kind, JoinKind::Inner | JoinKind::Left) {
        return Err(bind_err(format!(
            "CTE `{tname}` on the preserved side of a RIGHT/FULL JOIN is not \
             supported yet"
        )));
    }
    let cte_src = catalog_get(ctes, &tname).expect("caller checked the CTE exists");
    // The name the outer query addresses the CTE by: an explicit `AS x`, else
    // the CTE name itself. Kept as the base's alias so qualified refs resolve.
    let ref_alias = j.alias.clone().unwrap_or_else(|| tname.clone());
    let (cte_stmt, _explain, n_params) = parse_statement(cte_src)
        .map_err(|e| bind_err(format!("CTE `{tname}` body does not parse: {e}")))?;
    if n_params != 0 {
        return Err(bind_err(format!("CTE `{tname}` body must not use parameters")));
    }
    let Stmt::Select(mut body) = cte_stmt else {
        return Err(bind_err(format!("CTE `{tname}` body is not a simple SELECT")));
    };
    // The body itself may reference a view or another (preceding) CTE.
    flatten_select(&mut body, views, ctes, depth + 1)?;
    check_simple(&body, &tname)?;
    // `SELECT *` over the join cannot expand a projecting CTE body correctly —
    // the base carries columns the body hid. Refuse rather than answer wrongly.
    if !outer_has_items && body.items.is_some() {
        return Err(bind_err(format!(
            "`SELECT *` over a JOIN with a projecting CTE (`{tname}`) is not \
             supported yet; list the output columns explicitly"
        )));
    }

    // Rename the body's own qualifier to the reference alias, so its WHERE reads
    // under the same name the outer query and ON clause use.
    let from_name = body
        .alias
        .clone()
        .or_else(|| body.table.clone())
        .expect("check_simple guarantees a FROM table");
    let mut body_where = body.where_clause.take();
    if let Some(w) = &mut body_where {
        rename_qualifier(w, &from_name, &ref_alias);
    }

    // Splice: the join now reads the CTE's base under the reference alias, and
    // the CTE's WHERE is AND-merged into the join's ON (sound for INNER/LEFT).
    j.table = body.table.take().expect("check_simple guarantees a FROM table");
    j.alias = Some(ref_alias);
    if let Some(cw) = body_where {
        let existing = j.on.clone();
        j.on = Expr::Binary(crate::ast::BinOp::And, Box::new(existing), Box::new(cw));
    }
    Ok(())
}

/// Splice a derived table `FROM (SELECT …) t` onto its base (#74, Stage B) —
/// or leave it in place for the planner to MATERIALIZE (Stage A).
///
/// A derived table is a view whose body is written inline and whose alias is
/// intrinsic — `t` is how the outer query addresses the body's columns. So,
/// unlike a stored view (which V1 refuses to alias), the alias is KEPT: the
/// base is read `FROM base AS t`, which makes every outer `t.col` resolve to a
/// base column. Only a simple, ALIASED projection/filter body (the same
/// grammar a view allows) is spliced — it yields the better plan (index access
/// paths on the base). Everything else — aggregate/GROUP BY/DISTINCT/join/
/// ORDER BY+LIMIT bodies, compound bodies, and alias-less derived tables —
/// stays in `from_derived` for `PlanStmt::Derived` materialization at the top
/// level (a nested position refuses it by name at plan time), never an error
/// here.
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
    // The body's own view/CTE/nested-derived references flatten first, whatever
    // happens to the body itself.
    match body.as_mut() {
        SubqueryBody::Select(b) => flatten_select(b, views, ctes, depth + 1)?,
        SubqueryBody::Compound(c) => flatten_compound(c, views, ctes, depth + 1)?,
    }
    // §5.7: a body that is only a WRAPPER around another derived table is that
    // derived table — `FROM (SELECT * FROM (<X>)) o` and Django's
    // projection-restricting `FROM (SELECT sq.a, sq.b FROM (<X>) sq) o` both
    // read exactly `<X>`'s rows. Collapsing here is what turns a
    // derived-inside-a-derived (a nested position, refused by name) back into
    // the single outermost derived table Stage A materializes.
    for _ in 0..MAX_VIEW_DEPTH {
        collapse_passthrough(&mut body);
        if !collapse_projection_passthrough(s, &mut body) {
            break;
        }
    }
    if let SubqueryBody::Compound(c) = body.as_mut() {
        splice_passthrough_arms(c);
    }
    let splice_alias = match (body.as_ref(), s.alias.clone()) {
        (SubqueryBody::Select(b), Some(dalias)) if check_simple(b, &dalias).is_ok() => {
            Some(dalias)
        }
        _ => None,
    };
    let Some(dalias) = splice_alias else {
        // Not flattenable: hand the body to the planner unchanged.
        s.from_derived = Some(body);
        return Ok(());
    };
    let SubqueryBody::Select(mut body) = *body else {
        unreachable!("splice_alias is only Some for a Select body")
    };

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

// ---------------------------------------------------------------------------
// A derived table in a NESTED position, via the pass-through wrapper
// (design/DESIGN-DERIVED-TABLES.md §5.7).
//
// sqlite has no parenthesized compound operand and no way to name a
// non-flattenable body inline, so every generator that needs one writes the
// SAME wrapper: `SELECT * FROM ( <body> )`. Django's `SQLCompiler` writes it for
// a nested combinator, and its `subquery`-wrapping path writes the
// projection-restricting cousin `SELECT sq.a, sq.b FROM ( <body> ) sq`.
//
// Both wrappers are IDENTITIES over the body's rows, so they can be removed
// where a derived table would otherwise have to be materialized in a position
// mpedb cannot represent. Removing them is a pure AST rewrite: no plan-format
// change, no new executor surface, and the shapes that are NOT identities keep
// their existing refusal rather than being approximated.
// ---------------------------------------------------------------------------

/// `SELECT * FROM (<body>) [alias]` — and nothing else: no projection list, no
/// join, no DISTINCT, no WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET.
///
/// Such a SELECT **is** its body. It filters nothing, reprojects nothing,
/// reorders nothing, dedups nothing and limits nothing; and by sqlite's naming
/// rule a `SELECT *` over a derived table exposes exactly the body's own output
/// names, in the body's own order. The alias is irrelevant: with no expression
/// anywhere in the wrapper, nothing can reference it.
fn is_passthrough(s: &SelectStmt) -> bool {
    s.from_derived.is_some()
        && s.items.is_none()
        && s.joins.is_empty()
        && !s.distinct
        && s.where_clause.is_none()
        && s.group_by.is_empty()
        && s.having.is_none()
        && s.order_by.is_empty()
        && s.limit.is_none()
        && s.offset.is_none()
}

/// Drop every pass-through wrapper standing where a subquery BODY may stand
/// (a lifted subquery's body, a derived table's body). Bounded by the same
/// nesting limit the view splice uses; each step strictly removes one level.
fn collapse_passthrough(body: &mut SubqueryBody) {
    for _ in 0..MAX_VIEW_DEPTH {
        let SubqueryBody::Select(s) = body else { return };
        if !is_passthrough(s) {
            return;
        }
        let inner = s.from_derived.take().expect("is_passthrough checked it");
        *body = *inner;
    }
}

/// Flatten each arm of a compound, then splice away any pass-through arm.
fn flatten_compound(
    c: &mut CompoundStmt,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    for arm in &mut c.arms {
        flatten_select(arm, views, ctes, depth)?;
    }
    splice_passthrough_arms(c);
    Ok(())
}

/// May arm `k` — a pass-through wrapper — be replaced by its body IN PLACE,
/// left-associatively, without changing what the compound computes?
fn arm_splice_ok(c: &CompoundStmt, k: usize) -> bool {
    match c.arms[k].from_derived.as_deref() {
        // A plain SELECT body simply becomes the arm — unless it carries its
        // own ORDER BY / LIMIT / OFFSET, which bind INSIDE the parenthesized
        // body and would become the WHOLE compound's if spliced out. That is a
        // different query, so it keeps the refusal.
        Some(SubqueryBody::Select(b)) => {
            b.order_by.is_empty() && b.limit.is_none() && b.offset.is_none()
        }
        Some(SubqueryBody::Compound(inner)) => {
            if !inner.order_by.is_empty() || inner.limit.is_some() || inner.offset.is_some() {
                return false;
            }
            // Arm 0 is EXACT for any operators: a compound chain is already
            // evaluated left-associatively, so `(b0 ⊕ b1) op a1 …` and the flat
            // `b0 ⊕ b1 op a1 …` are the same evaluation, bracket for bracket.
            if k == 0 {
                return true;
            }
            // Anywhere else the splice turns `acc op (b0 ⊕ b1)` into
            // `(acc op b0) ⊕ b1`, which is equal only when `op` == `⊕` and that
            // operator is ASSOCIATIVE. `UNION` (∪), `UNION ALL` (⊎) and
            // `INTERSECT` (∩) are. `EXCEPT` is not — `A \ (B \ C) ≠ (A \ B) \ C`
            // — and neither is a MIXED chain: `A ∪ (B ⊎ C)` dedups C's
            // duplicates where `(A ∪ B) ⊎ C` keeps them. Both stay refused.
            let op = c.ops[k - 1];
            matches!(op, SetOp::Union | SetOp::UnionAll | SetOp::Intersect)
                && inner.ops.iter().all(|o| *o == op)
        }
        None => false,
    }
}

/// Replace every spliceable pass-through arm by its body: a plain SELECT
/// becomes the arm, a compound contributes its own arms and operators to the
/// enclosing chain.
fn splice_passthrough_arms(c: &mut CompoundStmt) {
    let mut k = 0;
    while k < c.arms.len() {
        if !(is_passthrough(&c.arms[k]) && arm_splice_ok(c, k)) {
            k += 1;
            continue;
        }
        let body = *c.arms[k].from_derived.take().expect("arm_splice_ok checked it");
        match body {
            SubqueryBody::Select(b) => {
                c.arms[k] = b;
                k += 1;
            }
            SubqueryBody::Compound(inner) => {
                let n = inner.arms.len();
                // `ops[i]` sits between `arms[i]` and `arms[i+1]`, so the inner
                // chain's operators go in at exactly `k`.
                c.ops.splice(k..k, inner.ops);
                c.arms.splice(k..=k, inner.arms);
                k += n;
            }
        }
    }
}

/// The output column NAMES of a subquery body, when they can be read off the
/// AST: the item's alias, else a bare (possibly qualified) column's own short
/// name. `None` when any name would depend on the planner's rendering of an
/// expression, or on a `SELECT *` expansion — in which case the caller must not
/// reason about the body's columns and simply declines to rewrite.
///
/// A compound's names come from its first arm (sqlite's and PG's rule), which
/// is the same rule `planner::derived::body_output_names` applies to the plan.
fn body_output_names(b: &SubqueryBody) -> Option<Vec<String>> {
    let arm = match b {
        SubqueryBody::Select(s) => s,
        SubqueryBody::Compound(c) => c.arms.first()?,
    };
    arm.items
        .as_ref()?
        .iter()
        .map(|(e, alias)| match (alias, e) {
            (Some(a), _) => Some(a.clone()),
            (None, Expr::Col(n)) | (None, Expr::Qualified(_, n)) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// The columns a PROJECTION-ONLY pass-through selects out of its derived
/// source: `SELECT i.a, i.b FROM (<X>) i` → `["a", "b"]`.
///
/// Every item must be a BARE column reference with no rename, so its output
/// name is the inner column's own name and the outer query's references carry
/// over unchanged. A qualifier must name the wrapper's own alias — nothing else
/// is in scope — and everything else about the wrapper must be inert, exactly
/// as for [`is_passthrough`].
fn projection_passthrough(m: &SelectStmt) -> Option<Vec<String>> {
    if m.from_derived.is_none()
        || !m.joins.is_empty()
        || m.distinct
        || m.where_clause.is_some()
        || !m.group_by.is_empty()
        || m.having.is_some()
        || !m.order_by.is_empty()
        || m.limit.is_some()
        || m.offset.is_some()
    {
        return None;
    }
    m.items
        .as_ref()?
        .iter()
        .map(|(e, alias)| match (alias, e) {
            (None, Expr::Col(n)) => Some(n.clone()),
            (None, Expr::Qualified(q, n)) if m.alias.as_deref() == Some(q.as_str()) => {
                Some(n.clone())
            }
            _ => None,
        })
        .collect()
}

/// Does any expression in this statement name one of `hidden`?
fn mentions_any(s: &SelectStmt, hidden: &[String]) -> bool {
    let hit = |e: &Expr| expr_mentions(e, hidden);
    s.items.as_ref().is_some_and(|it| it.iter().any(|(e, _)| hit(e)))
        || s.where_clause.as_ref().is_some_and(hit)
        || s.group_by.iter().any(hit)
        || s.having.as_ref().is_some_and(hit)
        || s.order_by.iter().any(|(e, _)| hit(e))
        || s.joins.iter().any(|j| hit(&j.on))
}

/// A deliberately CONSERVATIVE name scan: any column reference spelled `n` (or
/// `<anything>.n`) counts as a mention. Over-counting only costs a rewrite that
/// was in fact safe; under-counting would turn sqlite's "no such column" into
/// an answer, which is the one outcome that is never allowed.
fn expr_mentions(e: &Expr, hidden: &[String]) -> bool {
    let sub = |x: &Expr| expr_mentions(x, hidden);
    let body = |b: &SubqueryBody| match b {
        SubqueryBody::Select(s) => mentions_any(s, hidden),
        SubqueryBody::Compound(c) => c.arms.iter().any(|a| mentions_any(a, hidden)),
    };
    match e {
        Expr::Col(n) | Expr::Qualified(_, n) => {
            hidden.iter().any(|h| h.eq_ignore_ascii_case(n))
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Collate(a, _) => sub(a),
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) | Expr::Match(a, b) => sub(a) || sub(b),
        Expr::IsDistinct(a, b, _) | Expr::Glob(a, b, _) | Expr::Regexp(a, b, _) => {
            sub(a) || sub(b)
        }
        Expr::InList(a, list, _) => sub(a) || list.iter().any(sub),
        Expr::Case(arms, else_) => {
            arms.iter().any(|(c, r)| sub(c) || sub(r)) || else_.as_deref().is_some_and(sub)
        }
        Expr::Coalesce(items) | Expr::Func(_, items) | Expr::RowValue(items) => {
            items.iter().any(sub)
        }
        Expr::InParamSlot(a, _, _) | Expr::InContext(a, _, _) => sub(a),
        Expr::Agg(_, arg, _, filter, extra) => {
            arg.as_deref().is_some_and(sub)
                || extra.iter().any(sub)
                || filter.as_deref().is_some_and(sub)
        }
        Expr::Window { arg, spec, .. } => {
            arg.as_deref().is_some_and(sub)
                || spec.partition_by.iter().any(sub)
                || spec.order_by.iter().any(|(o, _)| sub(o))
        }
        // A subquery opens its own scope but MAY correlate outward, so scan it
        // too — over-counting is the safe direction here.
        Expr::Subquery(b) | Expr::Exists(b, _) => body(b),
        Expr::InSubquery(a, b, _) => sub(a) || body(b),
        Expr::Lit(_)
        | Expr::Param(_)
        | Expr::ContextRef(_)
        | Expr::Excluded(_) => false,
    }
}

/// Collapse a derived-table body that is a projection-only pass-through over
/// ANOTHER derived table — Django's `subquery` wrapper:
///
/// ```sql
/// SELECT count(*) FROM (SELECT sq.a, sq.b FROM (<X>) sq) o
/// ```
///
/// The middle SELECT reads `<X>`'s rows and drops columns; it does not filter,
/// reorder, dedup or limit. So the outer statement may read `<X>` DIRECTLY,
/// keeping its own alias — provided the columns the middle hid stay hidden.
/// They do, because:
///
/// - a `SELECT *` outer is first expanded to exactly the middle's item list
///   (so the output tuple is unchanged, in the middle's order); and
/// - if the outer mentions any hidden name anywhere, the rewrite is DECLINED
///   and the nested-derived refusal stands. sqlite answers "no such column"
///   there, and a refusal agrees with an error where an answer would not.
///
/// Returns `true` when it rewrote `body`.
fn collapse_projection_passthrough(s: &mut SelectStmt, body: &mut SubqueryBody) -> bool {
    let SubqueryBody::Select(m) = body else { return false };
    let Some(sel) = projection_passthrough(m) else { return false };
    let inner = m.from_derived.as_deref().expect("projection_passthrough checked it");
    let Some(xnames) = body_output_names(inner) else { return false };
    // Every selected name must really be one of the inner body's columns —
    // otherwise the original statement is an error, and this pass must not
    // turn it into anything else.
    if !sel.iter().all(|n| xnames.iter().any(|x| x.eq_ignore_ascii_case(n))) {
        return false;
    }
    let hidden: Vec<String> = xnames
        .iter()
        .filter(|x| !sel.iter().any(|n| n.eq_ignore_ascii_case(x)))
        .cloned()
        .collect();
    // `SELECT * FROM (<middle>) o` must keep exposing the MIDDLE's columns, in
    // the middle's order. With no join, spelling them out does exactly that;
    // with a join, `*` also spans the joined tables and one item list cannot
    // express the union — decline.
    let expand_star = s.items.is_none();
    if expand_star && !s.joins.is_empty() {
        return false;
    }
    // Decided BEFORE anything is mutated. The `*` expansion can only introduce
    // `sel` names, never a hidden one, so scanning the un-expanded statement is
    // the same question.
    if !hidden.is_empty() && mentions_any(s, &hidden) {
        return false;
    }
    if expand_star {
        s.items = Some(sel.iter().map(|n| (Expr::Col(n.clone()), None)).collect());
    }
    let SubqueryBody::Select(m) = body else { unreachable!("matched above") };
    let inner = m.from_derived.take().expect("projection_passthrough checked it");
    *body = *inner;
    true
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
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Collate(a, _) => {
            rename_qualifier(a, from, to)
        }
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) | Expr::Match(a, b) => {
            rename_qualifier(a, from, to);
            rename_qualifier(b, from, to);
        }
        Expr::IsDistinct(a, b, _) | Expr::Glob(a, b, _) | Expr::Regexp(a, b, _) => {
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
        Expr::Coalesce(items) | Expr::Func(_, items) | Expr::RowValue(items) => {
            for item in items {
                rename_qualifier(item, from, to);
            }
        }
        Expr::InParamSlot(a, _, _) | Expr::InContext(a, _, _) => rename_qualifier(a, from, to),
        // Both the aggregate ARGUMENT and its `FILTER (WHERE …)` may name the
        // derived alias — rename inside each.
        Expr::Agg(_, arg, _, filter, extra) => {
            if let Some(a) = arg {
                rename_qualifier(a, from, to);
            }
            for x in extra {
                rename_qualifier(x, from, to);
            }
            if let Some(f) = filter {
                rename_qualifier(f, from, to);
            }
        }
        // A window's sub-expressions may name the derived alias too. (Derived
        // bodies with windows are refused earlier, so this is belt-and-braces.)
        Expr::Window { arg, spec, .. } => {
            if let Some(a) = arg {
                rename_qualifier(a, from, to);
            }
            for p in &mut spec.partition_by {
                rename_qualifier(p, from, to);
            }
            for (o, _) in &mut spec.order_by {
                rename_qualifier(o, from, to);
            }
        }
        // A subquery in the body opens its own scope; refuse-by-check_simple
        // keeps aggregate/correlated bodies out, and a plain uncorrelated
        // subquery does not see the derived alias, so it is left as-is.
        Expr::Subquery(_) | Expr::Exists(_, _) | Expr::InSubquery(_, _, _) => {}
        Expr::Lit(_)
        | Expr::Param(_)
        | Expr::Col(_)
        | Expr::ContextRef(_)
        | Expr::Excluded(_) => {}
    }
}

/// Flatten views/CTEs/derived tables inside a subquery BODY — a plain SELECT or
/// each arm of a compound (#56/format 31 in a subquery position).
fn flatten_body(
    body: &mut SubqueryBody,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    match body {
        SubqueryBody::Select(s) => flatten_select(s, views, ctes, depth)?,
        SubqueryBody::Compound(c) => flatten_compound(c, views, ctes, depth)?,
    }
    // `IN (SELECT * FROM (<body>))` — the wrapper adds nothing, so the BODY is
    // the subquery. Dropping it is what lets a compound (or any other
    // non-flattenable body) stand in a subquery position, where a derived
    // table itself is still refused by name.
    collapse_passthrough(body);
    if let SubqueryBody::Compound(c) = body {
        splice_passthrough_arms(c);
    }
    Ok(())
}

/// Recurse into any subquery body carried by an expression.
fn flatten_expr(
    e: &mut Expr,
    views: &ViewCatalog,
    ctes: &ViewCatalog,
    depth: usize,
) -> Result<()> {
    match e {
        Expr::Subquery(s) | Expr::Exists(s, _) => flatten_body(s, views, ctes, depth),
        Expr::InSubquery(lhs, s, _) => {
            flatten_expr(lhs, views, ctes, depth)?;
            flatten_body(s, views, ctes, depth)
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => {
            flatten_expr(a, views, ctes, depth)
        }
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) | Expr::Match(a, b) => {
            flatten_expr(a, views, ctes, depth)?;
            flatten_expr(b, views, ctes, depth)
        }
        Expr::IsDistinct(a, b, _) | Expr::Glob(a, b, _) | Expr::Regexp(a, b, _) => {
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
        Expr::Agg(_, arg, _, filter, extra) => {
            if let Some(a) = arg {
                flatten_expr(a, views, ctes, depth)?;
            }
            for x in extra {
                flatten_expr(x, views, ctes, depth)?;
            }
            if let Some(f) = filter {
                flatten_expr(f, views, ctes, depth)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Collect every base name a SELECT reads FROM — its main table, its JOIN
/// operands, its derived-table body, and any subquery inside its expressions.
/// [`validate_cte_order`] uses this to see every CTE name a body references.
fn collect_source_names(s: &SelectStmt, out: &mut Vec<String>) {
    if let Some(t) = &s.table {
        out.push(t.clone());
    }
    for j in &s.joins {
        out.push(j.table.clone());
    }
    if let Some(d) = &s.from_derived {
        collect_body_sources(d, out);
    }
    if let Some(items) = &s.items {
        for (e, _) in items {
            collect_expr_sources(e, out);
        }
    }
    if let Some(w) = &s.where_clause {
        collect_expr_sources(w, out);
    }
    for g in &s.group_by {
        collect_expr_sources(g, out);
    }
    if let Some(h) = &s.having {
        collect_expr_sources(h, out);
    }
    for (e, _) in &s.order_by {
        collect_expr_sources(e, out);
    }
}

/// Collect the base names a subquery BODY reads FROM — a plain SELECT or each
/// arm of a compound (#56/format 31 in a subquery position).
fn collect_body_sources(body: &SubqueryBody, out: &mut Vec<String>) {
    match body {
        SubqueryBody::Select(s) => collect_source_names(s, out),
        SubqueryBody::Compound(c) => {
            for arm in &c.arms {
                collect_source_names(arm, out);
            }
        }
    }
}

/// Recurse into a subquery-bearing expression, collecting the base names any
/// nested SELECT reads FROM. Mirrors [`flatten_expr`]'s traversal.
fn collect_expr_sources(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Subquery(s) | Expr::Exists(s, _) => collect_body_sources(s, out),
        Expr::InSubquery(lhs, s, _) => {
            collect_expr_sources(lhs, out);
            collect_body_sources(s, out);
        }
        Expr::Unary(_, a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => collect_expr_sources(a, out),
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) | Expr::Match(a, b) => {
            collect_expr_sources(a, out);
            collect_expr_sources(b, out);
        }
        Expr::IsDistinct(a, b, _) | Expr::Glob(a, b, _) | Expr::Regexp(a, b, _) => {
            collect_expr_sources(a, out);
            collect_expr_sources(b, out);
        }
        Expr::InList(a, list, _) => {
            collect_expr_sources(a, out);
            for item in list {
                collect_expr_sources(item, out);
            }
        }
        Expr::Case(arms, else_) => {
            for (c, r) in arms {
                collect_expr_sources(c, out);
                collect_expr_sources(r, out);
            }
            if let Some(x) = else_ {
                collect_expr_sources(x, out);
            }
        }
        Expr::Coalesce(items) | Expr::Func(_, items) => {
            for item in items {
                collect_expr_sources(item, out);
            }
        }
        Expr::InParamSlot(a, _, _) | Expr::InContext(a, _, _) => collect_expr_sources(a, out),
        Expr::Agg(_, arg, _, filter, extra) => {
            if let Some(a) = arg {
                collect_expr_sources(a, out);
            }
            for x in extra {
                collect_expr_sources(x, out);
            }
            if let Some(f) = filter {
                collect_expr_sources(f, out);
            }
        }
        _ => {}
    }
}

/// The V1 flattenable grammar. Anything outside it is refused (never answered).
///
/// The message names **every** reason the body cannot flatten, not just the
/// first one hit. That is not cosmetics: this refusal is the front door to the
/// derived-table gap, and when it reported only the first failing check the
/// gap was mis-attributed downstream. A measured Django run classified 14
/// statements as "a JOIN inside a derived table" — and every one of those 14
/// ALSO had a `GROUP BY` or `DISTINCT` body, so flattening the join would have
/// closed exactly none of them. One `check_simple` call is one bind error, so
/// listing all the reasons costs nothing and stops the next reader from
/// building the wrong thing.
fn check_simple(v: &SelectStmt, name: &str) -> Result<()> {
    let mut bad: Vec<&str> = Vec::new();
    if v.table.is_none() {
        bad.push("a FROM-less body");
    }
    if !v.joins.is_empty() {
        bad.push("a JOIN");
    }
    if v.distinct {
        bad.push("DISTINCT");
    }
    if !v.group_by.is_empty() || v.having.is_some() {
        bad.push("GROUP BY/HAVING");
    }
    if !v.order_by.is_empty() {
        bad.push("ORDER BY");
    }
    if v.limit.is_some() || v.offset.is_some() {
        bad.push("LIMIT/OFFSET");
    }
    // Items must be `*` or bare, un-aliased columns (so exposed name == base
    // column name and no expression remapping is needed).
    if let Some(items) = &v.items {
        if items.iter().any(|(_, alias)| alias.is_some()) {
            bad.push("an aliased/renamed column");
        }
        // A QUALIFIED column (`b.c`) is named separately from a genuinely
        // computed item: it is blocked only because `flatten_derived` renames
        // the body's qualifier in the WHERE and not in the item list, so the
        // spliced `b.c` would not resolve under the derived alias. Calling that
        // "computed" sent a reader looking for an expression that is not there.
        if items.iter().any(|(e, _)| matches!(e, Expr::Qualified(..))) {
            bad.push("a qualified column (`t.c`) in the projection");
        }
        if items
            .iter()
            .any(|(e, _)| !matches!(e, Expr::Col(_) | Expr::Qualified(..)))
        {
            bad.push("a computed (non-column) projection");
        }
        // An aggregate with no GROUP BY still collapses the body to one row —
        // named separately from "computed" because the cardinality change, not
        // the expression, is what makes it unflattenable.
        if items.iter().any(|(e, _)| expr_aggregates(e)) && v.group_by.is_empty() {
            bad.push("an aggregate");
        }
    }
    if bad.is_empty() {
        return Ok(());
    }
    Err(bind_err(format!(
        "`{name}` uses {}, which is not supported yet (only a single-table \
         projection/filter source can be flattened)",
        bad.join(" + ")
    )))
}

/// Does this projection item aggregate (or window-aggregate) the body's rows?
/// Only the top-level shape matters — a subquery opens its own scope, so an
/// aggregate inside one does not collapse THIS body.
fn expr_aggregates(e: &Expr) -> bool {
    match e {
        Expr::Agg(..) | Expr::Window { .. } => true,
        Expr::Unary(_, a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::Collate(a, _)
        | Expr::InParamSlot(a, _, _)
        | Expr::InContext(a, _, _) => expr_aggregates(a),
        Expr::Binary(_, a, b)
        | Expr::Like(a, b, _)
        | Expr::Match(a, b)
        | Expr::IsDistinct(a, b, _)
        | Expr::Glob(a, b, _)
        | Expr::Regexp(a, b, _) => expr_aggregates(a) || expr_aggregates(b),
        Expr::InList(a, list, _) => expr_aggregates(a) || list.iter().any(expr_aggregates),
        Expr::Case(arms, els) => {
            arms.iter().any(|(c, r)| expr_aggregates(c) || expr_aggregates(r))
                || els.as_deref().is_some_and(expr_aggregates)
        }
        Expr::Coalesce(xs) | Expr::Func(_, xs) | Expr::RowValue(xs) => {
            xs.iter().any(expr_aggregates)
        }
        Expr::Subquery(_)
        | Expr::Exists(..)
        | Expr::InSubquery(..)
        | Expr::Lit(_)
        | Expr::Param(_)
        | Expr::Col(_)
        | Expr::Qualified(..)
        | Expr::ContextRef(_)
        | Expr::Excluded(_) => false,
    }
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
