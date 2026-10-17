//! Row ordering, collation validation, and bounded top-K gathering.

use super::{
    gather_rows, range_bounds, AccessPath, Collation, CompiledPlan, Error, ExprProgram, HostColls,
    OrderColl, Ordering, Result, SortDir, TxnCtx, Value,
};

/// ORDER BY over full table rows: `Value::sql_cmp` per column with NULLS
/// FIRST ascending; descending columns reverse their comparison (NULLS LAST).
/// Stable, so ties keep scan (PK) order.
/// Top-K variant of [`gather_rows`] for `ORDER BY … LIMIT`: scan paths route
/// through the bounded-heap [`TxnCtx::scan_rows_topk`]; point paths return
/// their at-most-one matching row (trivially the top-K).
#[allow(clippy::too_many_arguments)]
pub(in crate::exec) fn gather_topk(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    order_by: &[(u16, SortDir, OrderColl)],
    keep: usize,
) -> Result<Vec<Vec<Value>>> {
    check_order_colls(order_by, ctx.host_colls())?;
    match access {
        AccessPath::PkRange { lo, hi } => {
            match range_bounds(
                lo.as_ref(),
                hi.as_ref(),
                plan,
                params,
                ctx.key_col_types(table, 0).as_deref(),
            )? {
                None => Ok(Vec::new()),
                Some((lo_k, hi_k)) => ctx.scan_rows_topk(
                    table,
                    lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    filter.map(|f| (f, params)),
                    order_by,
                    keep,
                ),
            }
        }
        AccessPath::FullScan => ctx.scan_rows_topk(
            table,
            None,
            None,
            filter.map(|f| (f, params)),
            order_by,
            keep,
        ),
        // Point/index paths gather their matches — at most one for PK/unique,
        // every equal/in-range row for a non-unique index — then sort and cap.
        // These materialize all matches before truncating; a streaming index
        // cursor is deliberately deferred (#48) until a real workload shows
        // the cost.
        AccessPath::PkPoint(_)
        | AccessPath::IndexPoint { .. }
        | AccessPath::IndexRange { .. }
        | AccessPath::Series { .. }
        | AccessPath::FtsScan { .. } => {
            let mut r = gather_rows(ctx, table, access, filter, plan, params, None)?;
            sort_rows(&mut r, order_by, ctx.host_colls());
            r.truncate(keep);
            Ok(r)
        }
    }
}

pub(in crate::exec) fn sort_rows(
    rows: &mut [Vec<Value>],
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) {
    rows.sort_by(|a, b| cmp_rows(a, b, order_by, colls));
}

/// Every HOST collating sequence an `ORDER BY` names must be registered on the
/// connection running the sort. Checked ONCE, before any comparison, because a
/// `sort_by` comparator has nowhere to report a failure — and answering
/// "peers", or falling back to BINARY, would return rows in a silently wrong
/// order. sqlite's own message for the miss is reproduced verbatim, because
/// consumers (CPython's `test_deregister_collation`) assert on it.
pub(in crate::exec) fn check_order_colls(
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) -> Result<()> {
    for (_, _, c) in order_by {
        if let Some(name) = c.host() {
            if !colls.is_some_and(|t| t.has(name)) {
                return Err(Error::Unsupported(format!(
                    "no such collation sequence: {name}"
                )));
            }
        }
    }
    Ok(())
}

/// Total sort order over two rows for an `ORDER BY` spec (column index,
/// direction + NULL placement, collation). Shared by [`sort_rows`] and the
/// streaming top-K heap. The [`Collation`] is applied to text keys and is
/// [`Collation::Binary`] (bytewise) for a plain `ORDER BY`.
///
/// The NULL placement is decided BEFORE the direction is applied, and is NOT
/// reversed by `DESC`: `NULLS FIRST` means first in the delivered order either
/// way. That is the whole content of the explicit clause — sqlite's DEFAULTS
/// (first for ASC, last for DESC) are what `SortDir::dir` already resolved to,
/// so a plain `ORDER BY x` and a plain `ORDER BY x DESC` come out here exactly
/// as they did before the clause existed.
pub(in crate::exec) fn cmp_rows(
    a: &[Value],
    b: &[Value],
    order_by: &[(u16, SortDir, OrderColl)],
    colls: Option<&dyn HostColls>,
) -> Ordering {
    for (col, dir, coll) in order_by {
        let (col, dir) = (*col, *dir);
        let (Some(x), Some(y)) = (a.get(col as usize), b.get(col as usize)) else {
            continue;
        };
        match (x.is_null(), y.is_null()) {
            // Two NULLs are peers on this key; fall through to the next one.
            (true, true) => continue,
            (true, false) => {
                return if dir.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                return if dir.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {}
        }
        let ord = cmp_order(x, y, coll, colls);
        if ord != Ordering::Equal {
            return if dir.desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}

/// Order two NON-NULL values (the NULL cases are settled by the caller's
/// placement rule before this is reached).
fn cmp_order(a: &Value, b: &Value, coll: &OrderColl, colls: Option<&dyn HostColls>) -> Ordering {
    // A HOST collating sequence orders TEXT against TEXT; every other pair is
    // settled by storage class, exactly as sqlite does (the callback is only
    // ever consulted for two text values). `check_order_colls` has already
    // guaranteed the name resolves, so a miss here is unreachable.
    let coll = match coll {
        OrderColl::Native(c) => *c,
        OrderColl::Host(name) => {
            return match (a, b, colls) {
                (Value::Text(x), Value::Text(y), Some(t)) => t.compare(name, x, y),
                _ => cmp_order(a, b, &OrderColl::Native(Collation::Binary), None),
            }
        }
    };
    // `sort_cmp`, not `sql_cmp`: an `any` column really can hold a number AND a
    // string, and sqlite orders those by storage class (NULL < numbers < text <
    // blob). `sql_cmp` refuses that pair, and turning the refusal into `Equal`
    // — as this did — is not an order: it left `ORDER BY` over such a column
    // returning rows in an arbitrary sequence.
    match a.sort_cmp(b, coll) {
        Some(ord) => ord,
        // A pair `sort_cmp` will not rank even by storage class (mpedb's own
        // Bool/Timestamp against another class): peers, as before.
        None => Ordering::Equal,
    }
}
