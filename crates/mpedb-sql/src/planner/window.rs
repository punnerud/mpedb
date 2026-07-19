//! Window-function planning (design/DESIGN-WINDOW.md stage 1).
//!
//! The mirror image of `aggregate.rs`. `GROUP BY` collapses N base rows into one
//! grouped tuple `[keys ‖ aggs]`; a window function keeps all N rows and APPENDS
//! a per-row column, producing an extended tuple `[base row ‖ window results]`.
//! So the same machinery runs — lift the window calls out of the projection,
//! compile their sub-expressions over the base row, describe the extended tuple
//! as a synthetic scope, and re-bind the projection/ORDER BY over it — with a
//! row-PRESERVING synthetic tuple instead of a collapsing one.

use super::select::{ordinal, push_junk};
use super::*;

/// Does this expression contain a window function anywhere it would be planned?
///
/// Stops at aggregate and subquery boundaries, exactly as [`contains_agg`] does:
/// a window inside an aggregate argument is refused through the aggregate route
/// and the binder, and a window inside a subquery is that subquery's own
/// business (already lifted to a subplan before this runs).
pub(super) fn contains_window(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Window { .. } => true,
        E::Unary(_, a) | E::IsNull(a, _) | E::Cast(a, _) => contains_window(a),
        E::Binary(_, a, b)
        | E::Like(a, b)
        | E::Match(a, b)
        | E::IsDistinct(a, b, _)
        | E::Glob(a, b, _)
        | E::Regexp(a, b, _) => contains_window(a) || contains_window(b),
        E::InContext(a, _, _) => contains_window(a),
        E::Collate(a, _) => contains_window(a),
        E::InSubquery(a, _, _) | E::InParamSlot(a, _, _) => contains_window(a),
        E::InList(a, xs, _) => contains_window(a) || xs.iter().any(contains_window),
        E::Coalesce(xs) | E::Func(_, xs) => xs.iter().any(contains_window),
        E::Case(arms, els) => {
            arms.iter().any(|(c, r)| contains_window(c) || contains_window(r))
                || els.as_deref().is_some_and(contains_window)
        }
        E::Agg(..) | E::Subquery(_) | E::Exists(..) => false,
        E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..) => false,
    }
}

/// A window function pulled out of the projection/ORDER BY, before its
/// sub-expressions are bound. Structural equality drives slot reuse: two
/// identical windows (`rank() OVER w` selected AND ordered-by) share one slot,
/// so the window is computed once — exactly `lift_aggs`' aggregate reuse.
#[derive(PartialEq)]
struct WindowCollect {
    func: ast::WindowFunc,
    arg: Option<ast::Expr>,
    /// Trailing arguments (stage 2): lag/lead `[offset[, default]]`, nth_value
    /// `[n]`. Part of the structural key so two lag calls with different offsets
    /// do not share a slot.
    extra_args: Vec<ast::Expr>,
    distinct: bool,
    partition_by: Vec<ast::Expr>,
    order_by: Vec<(ast::Expr, bool)>,
}

/// Lift every window function out of `e`, replacing each with a reference to its
/// result slot in the extended tuple (`__w{k}`), and collecting the distinct
/// windows into `specs`. The window's OWN sub-expressions (arg, PARTITION BY,
/// ORDER BY) are NOT rewritten — they bind over the base row, like aggregate
/// arguments.
fn lift_windows(e: &ast::Expr, specs: &mut Vec<WindowCollect>) -> Result<ast::Expr> {
    use ast::Expr as E;
    let rec = |x: &ast::Expr, s: &mut Vec<WindowCollect>| lift_windows(x, s);
    Ok(match e {
        E::Window {
            func,
            arg,
            extra_args,
            distinct,
            spec,
        } => {
            let candidate = WindowCollect {
                func: func.clone(),
                arg: arg.as_deref().cloned(),
                extra_args: extra_args.clone(),
                distinct: *distinct,
                partition_by: spec.partition_by.clone(),
                order_by: spec.order_by.clone(),
            };
            let slot = match specs.iter().position(|s| *s == candidate) {
                Some(i) => i,
                None => {
                    specs.push(candidate);
                    specs.len() - 1
                }
            };
            E::Col(format!("__w{slot}"))
        }
        E::Unary(op, a) => E::Unary(*op, Box::new(rec(a, specs)?)),
        E::Cast(a, t) => E::Cast(Box::new(rec(a, specs)?), t.clone()),
        E::IsNull(a, n) => E::IsNull(Box::new(rec(a, specs)?), *n),
        E::Binary(op, a, b) => E::Binary(*op, Box::new(rec(a, specs)?), Box::new(rec(b, specs)?)),
        E::IsDistinct(a, b, n) => {
            E::IsDistinct(Box::new(rec(a, specs)?), Box::new(rec(b, specs)?), *n)
        }
        E::Like(a, b) => E::Like(Box::new(rec(a, specs)?), Box::new(rec(b, specs)?)),
        E::Match(a, b) => E::Match(Box::new(rec(a, specs)?), Box::new(rec(b, specs)?)),
        E::Glob(a, b, n) => E::Glob(Box::new(rec(a, specs)?), Box::new(rec(b, specs)?), *n),
        E::Regexp(a, b, n) => E::Regexp(Box::new(rec(a, specs)?), Box::new(rec(b, specs)?), *n),
        E::InList(a, xs, n) => E::InList(
            Box::new(rec(a, specs)?),
            xs.iter().map(|x| rec(x, specs)).collect::<Result<_>>()?,
            *n,
        ),
        E::InContext(a, k, n) => E::InContext(Box::new(rec(a, specs)?), k.clone(), *n),
        E::Collate(a, name) => E::Collate(Box::new(rec(a, specs)?), name.clone()),
        E::InParamSlot(a, slot, n) => E::InParamSlot(Box::new(rec(a, specs)?), *slot, *n),
        E::InSubquery(a, sq, n) => E::InSubquery(Box::new(rec(a, specs)?), sq.clone(), *n),
        E::Coalesce(xs) => E::Coalesce(xs.iter().map(|x| rec(x, specs)).collect::<Result<_>>()?),
        E::Func(f, xs) => E::Func(
            f.clone(),
            xs.iter().map(|x| rec(x, specs)).collect::<Result<_>>()?,
        ),
        E::Case(arms, els) => E::Case(
            arms.iter()
                .map(|(c, r)| Ok((rec(c, specs)?, rec(r, specs)?)))
                .collect::<Result<_>>()?,
            match els {
                Some(x) => Some(Box::new(rec(x, specs)?)),
                None => None,
            },
        ),
        // A window inside an aggregate/subquery is refused by the binder; leave
        // it for that refusal rather than lifting it here.
        other @ (E::Agg(..) | E::Subquery(_) | E::Exists(..)) => other.clone(),
        other @ (E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..)) => other.clone(),
    })
}

/// The plan-level function tag, an optional `default` program (lag/lead only),
/// and the synthetic result column's `(type, nullable)`. Ranking functions are
/// `Int64`, never NULL; aggregate windows adopt the aggregate result typing
/// verbatim; value/offset functions adopt the value's type and are always
/// nullable (an out-of-range offset or a short frame yields NULL). The value
/// functions' constant arguments (offset / n) and lag/lead's default are bound
/// HERE, so this takes `binder` (design/DESIGN-WINDOW.md §3.2 / stage 2).
fn resolve_window_func(
    binder: &mut Binder<'_>,
    func: &ast::WindowFunc,
    arg_ty: Option<ColumnType>,
    extra_args: &[ast::Expr],
) -> Result<(crate::plan::WindowFunc, Option<ExprProgram>, ColumnType, bool)> {
    use crate::plan::WindowFunc as P;
    use mpedb_types::AggFn;
    Ok(match func {
        ast::WindowFunc::RowNumber => (P::RowNumber, None, ColumnType::Int64, false),
        ast::WindowFunc::Rank => (P::Rank, None, ColumnType::Int64, false),
        ast::WindowFunc::DenseRank => (P::DenseRank, None, ColumnType::Int64, false),
        ast::WindowFunc::Agg(af) => {
            let (ty, nullable) = match af {
                AggFn::Count => (ColumnType::Int64, false),
                AggFn::Avg => (ColumnType::Float64, true),
                AggFn::Total => (ColumnType::Float64, false),
                AggFn::GroupConcat => (ColumnType::Text, true),
                // SUM/MIN/MAX keep the argument's type; NULL over an all-NULL
                // partition, like the grouped path.
                AggFn::Sum | AggFn::Min | AggFn::Max => (arg_ty.unwrap_or(ColumnType::Int64), true),
            };
            (P::Agg(*af), None, ty, nullable)
        }
        // lag/lead(expr [, offset [, default]]). The offset is a constant integer
        // (folded here); the default is an arbitrary expression evaluated at the
        // current row, so it must share the value's type.
        ast::WindowFunc::Lag | ast::WindowFunc::Lead => {
            let offset = match extra_args.first() {
                None => 1,
                Some(e) => const_int_arg(binder, e, "lag/lead offset")?,
            };
            let (default_prog, def_ty) = match extra_args.get(1) {
                None => (None, None),
                Some(e) => {
                    let (b, t) = binder.bind_expr(e)?;
                    (Some(compile_program(&b)?), t)
                }
            };
            let ty = unify_value_default(arg_ty, def_ty)?;
            let f = if matches!(func, ast::WindowFunc::Lag) {
                P::Lag(offset)
            } else {
                P::Lead(offset)
            };
            (f, default_prog, ty, true)
        }
        ast::WindowFunc::FirstValue => (P::FirstValue, None, arg_ty.unwrap_or(ColumnType::Int64), true),
        ast::WindowFunc::LastValue => (P::LastValue, None, arg_ty.unwrap_or(ColumnType::Int64), true),
        // nth_value(expr, n). `n` is a constant integer ≥ 1 (sqlite errors at
        // runtime on n < 1; a constant lets us refuse it cleanly at prepare).
        ast::WindowFunc::NthValue => {
            let n = match extra_args.first() {
                Some(e) => const_int_arg(binder, e, "nth_value n")?,
                None => return Err(bind_err("nth_value() requires a second argument")),
            };
            if n < 1 {
                return Err(bind_err(
                    "nth_value()'s second argument must be a positive integer constant",
                ));
            }
            (P::NthValue(n), None, arg_ty.unwrap_or(ColumnType::Int64), true)
        }
        // ntile(n): the bucket count `n` is a constant integer ≥ 1 (like
        // nth_value's n). It has NO per-row value argument — `n` is folded into
        // the tag here. Result is Int64, never NULL. (The ORDER BY requirement is
        // enforced by the caller, which sees the window's ORDER BY list.)
        ast::WindowFunc::Ntile => {
            let n = match extra_args.first() {
                Some(e) => const_int_arg(binder, e, "ntile bucket count")?,
                None => return Err(bind_err("ntile() requires an argument")),
            };
            if n < 1 {
                return Err(bind_err(
                    "ntile()'s argument must be a positive integer constant",
                ));
            }
            (P::Ntile(n), None, ColumnType::Int64, false)
        }
        // percent_rank()/cume_dist(): argument-less distribution functions
        // returning a float (never NULL). percent_rank uses rank() semantics;
        // cume_dist counts peers-inclusive. Neither requires ORDER BY — with none,
        // every row is a single peer group, so percent_rank is 0.0 and cume_dist
        // is 1.0 everywhere (matching sqlite, and deterministic).
        ast::WindowFunc::PercentRank => (P::PercentRank, None, ColumnType::Float64, false),
        ast::WindowFunc::CumeDist => (P::CumeDist, None, ColumnType::Float64, false),
    })
}

/// Bind a value-function offset / n argument and require it to fold to a
/// constant integer. A non-constant (a column, a parameter, an arithmetic
/// expression) or non-integer value is REFUSED: sqlite coerces the offset
/// per-row with brittle, version-specific rules (a non-integer float yields
/// all-NULL, non-numeric text yields 0), and the 0-wrong-answer contract forbids
/// guessing. The overwhelmingly common forms — `lag(x, 2)`, `nth_value(x, 3)` —
/// are integer literals and pass; a bare `lag(x)` never reaches here (offset
/// defaults to 1).
fn const_int_arg(binder: &mut Binder<'_>, e: &ast::Expr, what: &str) -> Result<i64> {
    let (b, _) = binder.bind_expr(e)?;
    match b {
        BExpr::Const(Value::Int(n)) => Ok(n),
        _ => Err(bind_err(format!(
            "{what} must be a constant integer — a non-constant or non-integer \
             {what} is refused (sqlite's per-row coercion is not reproducible)"
        ))),
    }
}

/// The result type of `lag`/`lead(expr, offset, default)`: the value and the
/// default share ONE type (a rigid engine has a single result column). A NULL /
/// untyped side adopts the other; a genuine int-vs-float or int-vs-text mismatch
/// is REFUSED (sqlite would type it per row). Both untyped ⇒ an all-NULL,
/// type-neutral column.
fn unify_value_default(
    value_ty: Option<ColumnType>,
    default_ty: Option<ColumnType>,
) -> Result<ColumnType> {
    match (value_ty, default_ty) {
        (Some(a), Some(b)) if a == b => Ok(a),
        (Some(a), Some(b)) => Err(bind_err(format!(
            "lag/lead default type ({b}) differs from the value type ({a}) — a rigid \
             engine needs a single result type; add an explicit CAST"
        ))),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Ok(ColumnType::Int64),
    }
}

/// A synthetic `TableDef` describing JUST the window result columns `__w{k}`,
/// appended after the base tables to form the extended scope. Never reaches the
/// row/key layer — it exists only so the ordinary binder can resolve `__w{k}`
/// and type-check the projection over the extended tuple.
fn synthetic_window_table(win_types: &[(ColumnType, bool)]) -> TableDef {
    let columns = win_types
        .iter()
        .enumerate()
        .map(|(k, &(ty, nullable))| mpedb_types::ColumnDef {
            name: format!("__w{k}"),
            ty,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None,
        })
        .collect();
    TableDef {
        id: 0,
        name: "$window".to_string(),
        columns,
        primary_key: vec![0],
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    }
}

/// A display name for one windowed SELECT item (EXPLAIN / output header only —
/// values, not names, are what the differential tests pin).
fn window_item_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Col(c) => c.clone(),
        ast::Expr::Qualified(_, c) => c.clone(),
        ast::Expr::Window { func, .. } => match func {
            ast::WindowFunc::RowNumber => "row_number()".to_string(),
            ast::WindowFunc::Rank => "rank()".to_string(),
            ast::WindowFunc::DenseRank => "dense_rank()".to_string(),
            ast::WindowFunc::Agg(f) => format!("{}()", f.name()),
            ast::WindowFunc::Lag => "lag()".to_string(),
            ast::WindowFunc::Lead => "lead()".to_string(),
            ast::WindowFunc::FirstValue => "first_value()".to_string(),
            ast::WindowFunc::LastValue => "last_value()".to_string(),
            ast::WindowFunc::NthValue => "nth_value()".to_string(),
            ast::WindowFunc::Ntile => "ntile()".to_string(),
            ast::WindowFunc::PercentRank => "percent_rank()".to_string(),
            ast::WindowFunc::CumeDist => "cume_dist()".to_string(),
        },
        _ => "?column?".to_string(),
    }
}

/// Plan a window SELECT over `binder`'s base scope — one table, DUAL, or a
/// join's `[outer ‖ inner]`. `binder` already carries the base scope and any
/// pinned subplan-slot types; its WHERE/policy have been bound into `filter`.
/// Correlated subplans are refused by the caller (the correlated executor path
/// does not run the window phase).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_window_select(
    s: &ast::SelectStmt,
    table_id: u32,
    access: AccessPath,
    filter: Option<ExprProgram>,
    joins: Vec<Join>,
    joined_filter: Option<ExprProgram>,
    mut binder: Binder<'_>,
    subplans: Vec<SubPlan>,
) -> Result<PlannedStmt> {
    let base_width = binder.scope_width();
    // The base tables (name ‖ def), for rebuilding the EXTENDED scope. Taken
    // before any mutation of the binder.
    let base_named = binder.scope.named();
    // Base-row column types, for `SELECT *` output typing.
    let base_col_types: Vec<Option<ColumnType>> = binder
        .scope
        .slot_types()
        .into_iter()
        .map(Some)
        .collect();

    // 1. Lift every window out of the SELECT list and ORDER BY.
    let mut specs: Vec<WindowCollect> = Vec::new();
    let rewritten_items: Option<Vec<(ast::Expr, Option<String>)>> = match &s.items {
        Some(items) => {
            let mut v = Vec::with_capacity(items.len());
            for (e, alias) in items {
                v.push((lift_windows(e, &mut specs)?, alias.clone()));
            }
            Some(v)
        }
        None => None,
    };
    let mut rewritten_order: Vec<(ast::Expr, bool)> = Vec::with_capacity(s.order_by.len());
    for (e, desc) in &s.order_by {
        rewritten_order.push((lift_windows(e, &mut specs)?, *desc));
    }
    if specs.is_empty() {
        return Err(bind_err("internal: window planner reached with no window function"));
    }
    // Keep prepare ⊆ decode: the decoder caps the window list at MAX_WINDOWS.
    if specs.len() > 64 {
        return Err(bind_err(
            "too many distinct window functions in one SELECT (max 64)",
        ));
    }

    // 2. Compile each window's sub-expressions over the BASE row.
    let mut windows: Vec<WindowSpec> = Vec::with_capacity(specs.len());
    let mut win_types: Vec<(ColumnType, bool)> = Vec::with_capacity(specs.len());
    for spec in &specs {
        if spec.distinct {
            return Err(bind_err(
                "DISTINCT is not allowed in a window aggregate (sqlite refuses it too)",
            ));
        }
        // ntile assigns buckets along the window order, so without an ORDER BY the
        // bucket numbers depend on the (arbitrary) scan order — a version-brittle,
        // non-reproducible answer. Refuse it cleanly rather than guess (the
        // 0-wrong-answer contract). `percent_rank`/`cume_dist` are well-defined
        // without ORDER BY (one peer group ⇒ 0.0 / 1.0), so they are allowed.
        if matches!(spec.func, ast::WindowFunc::Ntile) && spec.order_by.is_empty() {
            return Err(bind_err(
                "ntile() requires an ORDER BY in its OVER clause",
            ));
        }
        let (arg_prog, arg_ty) = match &spec.arg {
            None => (None, None),
            Some(a) => {
                let (b, ty) = binder.bind_expr(a)?;
                (Some(compile_program(&b)?), ty)
            }
        };
        let mut partition_by = Vec::with_capacity(spec.partition_by.len());
        for p in &spec.partition_by {
            let (b, _) = binder.bind_expr(p)?;
            partition_by.push(compile_program(&b)?);
        }
        let mut order_by = Vec::with_capacity(spec.order_by.len());
        for (p, desc) in &spec.order_by {
            let (b, _) = binder.bind_expr(p)?;
            order_by.push((compile_program(&b)?, *desc));
        }
        let (func, default, ty, nullable) =
            resolve_window_func(&mut binder, &spec.func, arg_ty, &spec.extra_args)?;
        win_types.push((ty, nullable));
        windows.push(WindowSpec {
            func,
            arg: arg_prog,
            distinct: false,
            partition_by,
            order_by,
            default,
        });
    }

    // 3. Rescope the binder to the EXTENDED tuple: base tables ‖ the `__w{k}`
    //    window table. Slot `base_width + k` is window `k`'s result. Two copies
    //    of the named list — one for the binder's scope, one for the naming
    //    scope `push_junk` needs (the binder owns its own).
    let window_table = synthetic_window_table(&win_types);
    let mut ext_bind = base_named.clone();
    ext_bind.push(("$window".to_string(), &window_table));
    let mut ext_name = base_named;
    ext_name.push(("$window".to_string(), &window_table));
    let name_scope = Scope::joined_named(ext_name)?;
    let mut binder = binder.rescope(Scope::joined_named(ext_bind)?);

    // 4. Bind the rewritten projection over the extended tuple.
    let mut out_types: Vec<Option<ColumnType>> = Vec::new();
    let mut projection: Vec<Projection> = match &rewritten_items {
        // `SELECT *`: the base columns, in order (window results are only ever
        // named explicitly, so `*` never includes them).
        None => {
            out_types = base_col_types;
            (0..base_width as u16).map(Projection::Column).collect()
        }
        Some(items) => {
            let mut proj = Vec::with_capacity(items.len());
            for ((rw, alias), (orig, _)) in items.iter().zip(s.items.as_ref().expect("items")) {
                let (b, ty) = binder.bind_expr(rw)?;
                out_types.push(ty);
                // Name from the ORIGINAL item (its alias, or a rendered form) —
                // never from the synthetic `__w`/base slot.
                let name = alias.clone().unwrap_or_else(|| window_item_name(orig));
                proj.push(Projection::Expr {
                    program: compile_program(&b)?,
                    name,
                });
            }
            proj
        }
    };

    // 5. ORDER BY runs over the projection (the window results live there, so the
    //    sort must follow the window phase). A key that matches a selected item
    //    names its position; otherwise it becomes a sort-only junk column —
    //    exactly the aggregate ORDER BY path.
    let n_items = s.items.as_ref().map_or(base_width, Vec::len);
    let mut order_by = Vec::with_capacity(rewritten_order.len());
    let mut order_junk = 0u16;
    for (i, ((e, desc), (orig, _))) in rewritten_order.iter().zip(&s.order_by).enumerate() {
        // Collation comes from the original key text; peel both the original
        // (for ordinal/alias) and the lifted expr (for item match / junk).
        let (orig, coll) = peel_collate(orig)?;
        let coll = coll.unwrap_or_default();
        let (e, _) = peel_collate(e)?;
        // `ORDER BY 1` — an output ordinal.
        if let Some(pos) = ordinal(orig, n_items)? {
            order_by.push((pos, *desc, coll));
            continue;
        }
        // A bare name matching a select-item ALIAS names that output position
        // (PG's rule: the output name wins over an input column).
        if let ast::Expr::Col(n) = orig {
            if let Some(items) = &s.items {
                if let Some(pos) = items.iter().position(|it| it.1.as_deref() == Some(n.as_str())) {
                    order_by.push((pos as u16, *desc, coll));
                    continue;
                }
            }
        }
        // A repeat of a selected item (same rewritten expression) reuses its
        // slot — `ORDER BY rank() OVER w` when `rank() OVER w` is selected.
        if let Some(items) = &rewritten_items {
            if let Some(pos) = items.iter().position(|(it, _)| it == e) {
                order_by.push((pos as u16, *desc, coll));
                continue;
            }
        }
        // Otherwise a sort-only column, appended to the projection. Refused
        // under DISTINCT (a key not in the output would dedup on an invisible
        // value) — but `check_distinct_order_by` has already ruled that out.
        let mut junk = if s.distinct {
            None
        } else {
            Some((&mut projection, &mut binder))
        };
        let (pos, added) = push_junk(&mut junk, e, &name_scope, i)?;
        order_by.push((pos, *desc, coll));
        order_junk += added;
    }

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select(SelectPlan {
            table: table_id,
            access,
            joins,
            joined_filter,
            post_filter: None,
            filter,
            projection,
            order_by,
            order_over: OrderOver::Projection,
            order_junk,
            limit: s.limit,
            offset: s.offset,
            distinct: s.distinct,
            aggregate: None,
            windows,
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        subplans,
    ))
}
