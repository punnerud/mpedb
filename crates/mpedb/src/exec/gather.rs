use super::*;

/// `INNER JOIN`, as a nested loop over the outer scan.
///
/// The order of the four tests is the security contract, not an implementation
/// detail — see [`mpedb_sql::Join::policy`]. Each table's RLS `USING` runs over
/// ITS OWN row, before anything that can see both:
///
/// mpedb's expressions raise on division by zero and on overflow, and a raise
/// is observable. An `ON a.x / b.secret > 1` evaluated before b's policy would
/// report the existence of a row the policy hides — the row never comes back,
/// but the error says it was there. Filtering first is what makes the policy a
/// filter rather than a suggestion.
///
/// Cost: the inner side is read ONCE and held, so this is O(n+m) reads and
/// O(n·m) `on` evaluations, with the inner side resident. No predicate is
/// pushed into either scan yet — every conjunct of the user's WHERE waits for
/// the joined row — so both sides are full scans unless a POLICY pins a key.
/// `EXPLAIN` says so rather than leaving it to be found on a big table.
/// Does this access path reference the outer row (`KeyPart::OuterCol`)?
/// If so it is the index nested-loop form, resolved per outer row.
fn access_has_outer(a: &AccessPath) -> bool {
    let outer = |p: &KeyPart| matches!(p, KeyPart::OuterCol(_));
    let bound_outer = |b: &Option<KeyBound>| {
        b.as_ref().is_some_and(|b| b.parts.iter().any(outer))
    };
    match a {
        AccessPath::PkPoint(parts) => parts.iter().any(outer),
        AccessPath::IndexPoint { part, .. } => outer(part),
        AccessPath::PkRange { lo, hi } | AccessPath::IndexRange { lo, hi, .. } => {
            bound_outer(lo) || bound_outer(hi)
        }
        AccessPath::FullScan => false,
    }
}

/// Fetch one join step's candidate rows for ONE outer row — the index nested
/// loop. The join's POLICY runs here, over each fetched inner row alone,
/// BEFORE the residual ON can raise on it: the same RLS ordering contract as
/// the held path, where `gather_rows` applies it as the fetch filter.
fn fetch_inner(
    ctx: &mut dyn TxnCtx,
    join: &Join,
    plan: &CompiledPlan,
    params: &[Value],
    outer: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mut rows = match &join.access {
        AccessPath::PkPoint(parts) => {
            let mut pk = Vec::with_capacity(parts.len());
            let mut any_null = false;
            for p in parts {
                let v = resolve_part_outer(p, plan, params, outer)?;
                if v.is_null() {
                    // `inner.pk = NULL` is UNKNOWN: no candidates (and for a
                    // LEFT join, that means NULL-extension — SQL's answer).
                    any_null = true;
                    break;
                }
                pk.push(v);
            }
            if any_null {
                Vec::new()
            } else {
                ctx.get_by_pk(join.table, &pk)?.into_iter().collect()
            }
        }
        AccessPath::IndexPoint { index_no, part } => {
            let v = resolve_part_outer(part, plan, params, outer)?;
            if v.is_null() {
                Vec::new()
            } else {
                ctx.scan_by_index(join.table, *index_no, &v)?
            }
        }
        _ => return Err(internal("unparametrized access in index nested loop")),
    };
    if let Some(p) = &join.policy {
        let mut stack = Vec::with_capacity(p.max_stack());
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if p.eval_filter(&mut stack, &row, params)? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn gather_joined(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    outer_table: u32,
    outer_access: &AccessPath,
    outer_policy: Option<&ExprProgram>,
    joins: &[Join],
    joined_filter: Option<&ExprProgram>,
) -> Result<Vec<Vec<Value>>> {
    // Left-deep nested loop. Start with the outer's rows (its policy applies
    // through the access path), then fold in each join: gather that table's
    // rows — its policy runs over its OWN row, BEFORE any ON can raise on
    // it — and keep the pairs its ON accepts. Join `k`'s ON sees the row
    // accumulated so far, `[table0 ‖ … ‖ table_k]`, which is exactly the tuple
    // the planner bound and width-checked it against.
    let mut acc =
        gather_rows(ctx, outer_table, outer_access, outer_policy, plan, params, None)?;
    let mut stack = Vec::new();
    for join in joins {
        let inner_width = table_def(schema, join.table)?.columns.len();
        // An access with no OuterCol parts is resolved once: read the inner
        // side once and hold it (the pre-#49 execution — keeping it is what
        // stops an ON without equality from regressing to O(n·m) READS). One
        // WITH OuterCol parts is the index nested loop, fetched per outer row.
        let held: Option<Vec<Vec<Value>>> = if access_has_outer(&join.access) {
            None
        } else {
            Some(gather_rows(
                ctx,
                join.table,
                &join.access,
                join.policy.as_ref(),
                plan,
                params,
                None,
            )?)
        };
        let mut next = Vec::new();
        for a in &acc {
            let fetched;
            let candidates: &[Vec<Value>] = match &held {
                Some(rows) => rows,
                None => {
                    fetched = fetch_inner(ctx, join, plan, params, a)?;
                    &fetched
                }
            };
            let mut matched = false;
            for i in candidates {
                let mut joined = Vec::with_capacity(a.len() + i.len());
                joined.extend_from_slice(a);
                joined.extend_from_slice(i);
                if join.on.eval_filter(&mut stack, &joined, params)? {
                    matched = true;
                    next.push(joined);
                }
            }
            // LEFT: no match → ONE row with the inner side NULL-extended. The
            // ON is never evaluated over this row — it exists BECAUSE nothing
            // matched — so it cannot raise on it, and a policy-hidden inner
            // row reads as ABSENT (the outer row survives, NULL-extended,
            // never carrying the hidden row's values).
            if !matched && join.kind == JoinKind::Left {
                let mut joined = Vec::with_capacity(a.len() + inner_width);
                joined.extend_from_slice(a);
                joined.resize(a.len() + inner_width, Value::Null);
                next.push(joined);
            }
        }
        acc = next;
    }
    // WHERE runs once, over the full joined row — after every ON and every
    // per-table policy, because it can raise and a raise is observable.
    if let Some(f) = joined_filter {
        let mut kept = Vec::with_capacity(acc.len());
        for row in acc {
            if f.eval_filter(&mut stack, &row, params)? {
                kept.push(row);
            }
        }
        acc = kept;
    }
    Ok(acc)
}

pub(crate) fn resolve_part(part: &KeyPart, plan: &CompiledPlan, params: &[Value]) -> Result<Value> {
    Ok(match part {
        KeyPart::Param(i) => params
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("key param"))?,
        KeyPart::Const(i) => plan
            .consts
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("key const"))?,
        // Only legal inside a join's access path, where the outer row exists;
        // validate refuses it anywhere else, so reaching this is an exec bug.
        KeyPart::OuterCol(_) => return Err(internal("outer-column key part outside a join")),
    })
}

/// [`resolve_part`] with the accumulated outer row in scope — the index
/// nested-loop form, where `OuterCol(i)` is slot `i` of that row.
fn resolve_part_outer(
    part: &KeyPart,
    plan: &CompiledPlan,
    params: &[Value],
    outer: &[Value],
) -> Result<Value> {
    match part {
        KeyPart::OuterCol(i) => outer
            .get(*i as usize)
            .cloned()
            .ok_or_else(|| internal("outer key column out of row bounds")),
        other => resolve_part(other, plan, params),
    }
}

/// Fetch the candidate rows for an access path and apply the residual filter.
pub(super) fn gather_rows(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    cap: Option<usize>,
) -> Result<Vec<Vec<Value>>> {
    // Scan paths push the filter AND the cap down into the (possibly
    // streaming) scan. Point and index-equality paths gather their matches —
    // one row for a PK/unique probe, every equal row for a non-unique index —
    // and filter here (no cap pushdown; the caller's skip/take still bounds
    // what is returned).
    let mut rows = match access {
        AccessPath::PkPoint(parts) => {
            let mut pk = Vec::with_capacity(parts.len());
            for p in parts {
                pk.push(resolve_part(p, plan, params)?);
            }
            // A NULL PK part can never match a stored row (PK columns are NOT
            // NULL); get_by_pk simply misses — SQL's `pk = NULL` is UNKNOWN.
            ctx.get_by_pk(table, &pk)?.into_iter().collect()
        }
        AccessPath::PkRange { lo, hi } => {
            return match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                // A NULL bound makes the range predicate UNKNOWN for every
                // row: no matches.
                None => Ok(Vec::new()),
                Some((lo_k, hi_k)) => ctx.scan_rows_capped(
                    table,
                    lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    filter.map(|f| (f, params)),
                    cap,
                ),
            };
        }
        AccessPath::IndexPoint { index_no, part } => {
            let v = resolve_part(part, plan, params)?;
            if v.is_null() {
                Vec::new() // `col = NULL` is UNKNOWN; NULLs are never indexed
            } else {
                // N rows for a non-unique index, 0/1 for a unique one — the
                // engine picks the exact-get fast path when it is unique.
                ctx.scan_by_index(table, *index_no, &v)?
            }
        }
        AccessPath::IndexRange { index_no, lo, hi } => {
            match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
                // A NULL bound makes the range predicate UNKNOWN: no matches.
                None => Vec::new(),
                // The same prefix-bound construction as a composite-PK range
                // works over the index tree: both the unique (`value`) and the
                // non-unique (`value ‖ pk`) key layouts start with the encoded
                // value, and `prefix_hi` clears every continuation.
                Some((lo_k, hi_k)) => ctx.scan_by_index_range(
                    table,
                    *index_no,
                    lo_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                    hi_k.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
                )?,
            }
        }
        AccessPath::FullScan => {
            return ctx.scan_rows_capped(table, None, None, filter.map(|f| (f, params)), cap);
        }
    };
    if let Some(f) = filter {
        let mut stack = Vec::with_capacity(f.max_stack());
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if f.eval_filter(&mut stack, &row, params)? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok(rows)
}

pub(crate) type RawBound = (Vec<u8>, bool);

/// Raw encoded-key bounds for a Phase-1 PK range (bounds are over the FIRST
/// PK column only), with prefix semantics for composite PKs:
///
/// - `enc(v)`       = `keycode::encode_key(&[v])` — a strict prefix of every
///   composite key whose first column equals `v`.
/// - `prefix_hi(v)` = `enc(v) ++ [0xFF]` — greater than every key whose first
///   column equals `v` (continuation tags are 0x00/0x01 < 0xFF) and less than
///   the encoding of any larger first-column value.
///
/// lo inclusive → (enc(v), true); lo exclusive → (prefix_hi(v), true);
/// hi inclusive → (prefix_hi(v), false); hi exclusive → (enc(v), false).
/// Single-column PKs get identical results by the same construction.
///
/// Returns `Ok(None)` when a bound resolves to NULL (empty result).
pub(crate) fn range_bounds(
    lo: Option<&KeyBound>,
    hi: Option<&KeyBound>,
    plan: &CompiledPlan,
    params: &[Value],
) -> Result<Option<(Option<RawBound>, Option<RawBound>)>> {
    let resolve = |b: &KeyBound| -> Result<Option<Value>> {
        let part = b.parts.first().ok_or_else(|| internal("range bound"))?;
        let v = resolve_part(part, plan, params)?;
        Ok(if v.is_null() { None } else { Some(v) })
    };
    let lo_k = match lo {
        None => None,
        Some(b) => match resolve(b)? {
            None => return Ok(None),
            Some(v) => Some(if b.inclusive {
                (enc1(&v), true)
            } else {
                (prefix_hi(&v), true)
            }),
        },
    };
    let hi_k = match hi {
        None => None,
        Some(b) => match resolve(b)? {
            None => return Ok(None),
            Some(v) => Some(if b.inclusive {
                (prefix_hi(&v), false)
            } else {
                (enc1(&v), false)
            }),
        },
    };
    Ok(Some((lo_k, hi_k)))
}

fn enc1(v: &Value) -> Vec<u8> {
    keycode::encode_key(std::slice::from_ref(v))
}

fn prefix_hi(v: &Value) -> Vec<u8> {
    let mut k = enc1(v);
    k.push(0xFF);
    k
}

/// ORDER BY over full table rows: `Value::sql_cmp` per column with NULLS
/// FIRST ascending; descending columns reverse their comparison (NULLS LAST).
/// Stable, so ties keep scan (PK) order.
/// Top-K variant of [`gather_rows`] for `ORDER BY … LIMIT`: scan paths route
/// through the bounded-heap [`TxnCtx::scan_rows_topk`]; point paths return
/// their at-most-one matching row (trivially the top-K).
#[allow(clippy::too_many_arguments)]
pub(super) fn gather_topk(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    order_by: &[(u16, bool)],
    keep: usize,
) -> Result<Vec<Vec<Value>>> {
    match access {
        AccessPath::PkRange { lo, hi } => {
            match range_bounds(lo.as_ref(), hi.as_ref(), plan, params)? {
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
        AccessPath::FullScan => {
            ctx.scan_rows_topk(table, None, None, filter.map(|f| (f, params)), order_by, keep)
        }
        // Point/index paths gather their matches — at most one for PK/unique,
        // every equal/in-range row for a non-unique index — then sort and cap.
        // These materialize all matches before truncating; a streaming index
        // cursor is deliberately deferred (#48) until a real workload shows
        // the cost.
        AccessPath::PkPoint(_) | AccessPath::IndexPoint { .. } | AccessPath::IndexRange { .. } => {
            let mut r = gather_rows(ctx, table, access, filter, plan, params, None)?;
            sort_rows(&mut r, order_by);
            r.truncate(keep);
            Ok(r)
        }
    }
}

pub(super) fn sort_rows(rows: &mut [Vec<Value>], order_by: &[(u16, bool)]) {
    rows.sort_by(|a, b| cmp_rows(a, b, order_by));
}

/// Total sort order over two rows for an `ORDER BY` spec (column index +
/// descending flag), NULLS FIRST ascending. Shared by [`sort_rows`] and the
/// streaming top-K heap.
pub(super) fn cmp_rows(a: &[Value], b: &[Value], order_by: &[(u16, bool)]) -> Ordering {
    for &(col, desc) in order_by {
        let (Some(x), Some(y)) = (a.get(col as usize), b.get(col as usize)) else {
            continue;
        };
        let ord = cmp_order(x, y);
        if ord != Ordering::Equal {
            return if desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}

fn cmp_order(a: &Value, b: &Value) -> Ordering {
    match a.sql_cmp(b) {
        Ok(Some(ord)) => ord,
        // NULL involved: NULLS FIRST in ascending order.
        Ok(None) => match (a.is_null(), b.is_null()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
        // Cross-type comparison cannot happen within one rigidly-typed
        // column; treat the impossible as equal rather than panicking.
        Err(_) => Ordering::Equal,
    }
}
