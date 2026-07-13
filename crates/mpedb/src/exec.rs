//! Plan executor: runs a validated [`CompiledPlan`] against an engine
//! transaction. Shared by the autocommit paths on [`crate::Database`] and the
//! interactive [`crate::WriteSession`] via the [`TxnCtx`] abstraction.

use crate::ExecResult;
use mpedb_core::{ReadTxn, WriteTxn};
use mpedb_sql::{AccessPath, CompiledPlan, InsertSource, PlanStmt, Projection};
use mpedb_types::{
    keycode, DefaultExpr, Error, ExprProgram, KeyBound, KeyPart, Result, Schema, TableDef, Value,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// The row operations the executor needs, implemented by both transaction
/// kinds. Write operations on a read transaction are unreachable by
/// construction (routing is by the recomputed `footprint.read_only`) and
/// return `Error::Internal` if ever hit.
pub(crate) trait TxnCtx {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>>;
    fn get_by_index(&mut self, table: u32, index_no: u32, value: &Value)
        -> Result<Option<Vec<Value>>>;
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>>;
    /// Scan with the residual filter applied per row and an optional cap on
    /// KEPT rows — the LIMIT/OFFSET pushdown (MPEE "stream under a memory
    /// budget" transfer: never materialize what the query will not return).
    /// The default collects the whole range first (used by WriteTxn contexts,
    /// where collect-then-mutate is the rule anyway); ReadCtx overrides it
    /// with true cursor streaming, which is the autocommit SELECT path.
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let keep = match filter {
                Some((f, params)) => f.eval_filter(&mut stack, &row, params)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    /// Streaming top-K for `ORDER BY … LIMIT`: return the `keep` smallest
    /// rows under `order_by` (already sorted), scanning under a bounded
    /// `keep`-sized heap so memory is O(keep) instead of O(matched rows) —
    /// the MPEE "stream under a memory budget" transfer applied to sorted
    /// pagination. The default materializes the whole range then sorts and
    /// truncates (used by WriteTxn contexts); ReadCtx overrides it with a
    /// true streaming heap.
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, bool)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let ok = match filter {
                Some((f, params)) => f.eval_filter(&mut stack, &row, params)?,
                None => true,
            };
            if ok {
                kept.push(row);
            }
        }
        sort_rows(&mut kept, order_by);
        kept.truncate(keep);
        Ok(kept)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()>;
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool>;
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool>;
}

impl TxnCtx for WriteTxn<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self, table, pk)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_index(self, table, index_no, value)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_rows_raw(self, table, lo, hi)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        WriteTxn::insert_row(self, table, values)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        WriteTxn::update_by_pk(self, table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        WriteTxn::delete_by_pk(self, table, pk)
    }
}

/// Adapter over a pinned read snapshot.
pub(crate) struct ReadCtx<'t, 'e>(pub &'t ReadTxn<'e>);

impl TxnCtx for ReadCtx<'_, '_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk(table, pk)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Option<Vec<Value>>> {
        self.0.get_by_index(table, index_no, value)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut out = Vec::new();
        while let Some(row) = cursor.next()? {
            out.push(row);
        }
        Ok(out)
    }
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        // true streaming: stop pulling from the B+tree cursor the moment the
        // cap is reached — `SELECT ... LIMIT k` does O(offset+k) work
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        while let Some(row) = cursor.next()? {
            let keep = match filter {
                Some((f, params)) => f.eval_filter(&mut stack, &row, params)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, bool)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        if keep == 0 {
            return Ok(Vec::new());
        }
        // Bounded max-heap of the `keep` smallest rows seen so far: the heap's
        // top is the *worst* kept row, so a newcomer that sorts before it
        // evicts it. Never more than `keep` rows are held, regardless of how
        // many the scan yields.
        let mut heap: BinaryHeap<Ranked<'_>> = BinaryHeap::with_capacity(keep + 1);
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut stack = Vec::new();
        // Scan sequence = PK order; used as a stable tiebreaker so equal
        // ORDER BY keys come out exactly as the engine's stable `sort_rows`
        // would order them (scan/PK order), matching the non-top-K path.
        let mut seq: u64 = 0;
        while let Some(row) = cursor.next()? {
            let ok = match filter {
                Some((f, params)) => f.eval_filter(&mut stack, &row, params)?,
                None => true,
            };
            if !ok {
                continue;
            }
            let cand = Ranked { row, order_by, seq };
            seq += 1;
            if heap.len() < keep {
                heap.push(cand);
            } else if cand < *heap.peek().expect("keep >= 1") {
                heap.pop();
                heap.push(cand);
            }
        }
        Ok(heap.into_sorted_vec().into_iter().map(|r| r.row).collect())
    }
    fn insert_row(&mut self, _table: u32, _values: &[Value]) -> Result<()> {
        Err(read_txn_write_bug())
    }
    fn update_by_pk(&mut self, _table: u32, _new_values: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
    fn delete_by_pk(&mut self, _table: u32, _pk: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
}

/// A row wrapped with its `ORDER BY` spec so a [`BinaryHeap`] (max-heap)
/// keeps the smallest rows: `Ord` follows the sort order, so the heap's max
/// is the row that sorts *last*.
struct Ranked<'a> {
    row: Vec<Value>,
    order_by: &'a [(u16, bool)],
    seq: u64,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: the ORDER BY spec. Secondary: scan sequence ASCENDING
        // regardless of the ORDER BY direction — a stable sort keeps equal
        // keys in original (scan) order, so the tiebreak is never reversed.
        cmp_rows(&self.row, &other.row, self.order_by).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked<'_> {}

fn read_txn_write_bug() -> Error {
    Error::Internal("DML plan routed to a read transaction".into())
}

fn internal(msg: &str) -> Error {
    Error::Internal(format!("validated plan out of bounds: {msg}"))
}

/// True when `e` is a constraint error that the engine's row mutators
/// (`insert_row`/`update_by_pk`) raise from their pre-checks, strictly
/// *before* mutating any tree: a call that failed this way left the
/// transaction untouched. Anything else (DbFull, Corrupt, Internal, Io, ...)
/// can fire mid-mutation and must be treated as a possible partial effect.
fn precheck_failure(e: &Error) -> bool {
    matches!(
        e,
        Error::TypeMismatch(_)
            | Error::NotNullViolation { .. }
            | Error::CheckViolation { .. }
            | Error::UniqueViolation { .. }
            | Error::PrimaryKeyViolation { .. }
    )
}

/// Execute one statement plan against `ctx`. `params` are validated first
/// (count, then declared types; NULL always passes the type check —
/// nullability is enforced by the engine where it matters).
///
/// `partial` is an out-flag for statement-level atomicity: when the returned
/// value is an `Err`, `*partial == true` means the failed statement may
/// already have applied some of its effects to `ctx` (e.g. a multi-row
/// INSERT that violated a constraint on its third row inserted the first
/// two). Callers that keep the transaction alive across statement failures
/// ([`crate::WriteSession`]) must then poison it; the autocommit path aborts
/// the whole transaction on any error and can ignore the flag. The flag is
/// never set spuriously *false* (never under-reports), but it may be
/// conservatively *true* for failures whose partial effects cannot be ruled
/// out.
pub(crate) fn exec_stmt(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
) -> Result<ExecResult> {
    validate_params(plan, params)?;
    match &plan.stmt {
        PlanStmt::Select {
            table,
            access,
            filter,
            projection,
            order_by,
            limit,
            offset,
        } => {
            let t = table_def(schema, *table)?;
            let skip_take_bound = || {
                limit.map(|l| {
                    let l = l.min(usize::MAX as u64) as usize;
                    let o = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
                    l.saturating_add(o)
                })
            };
            let rows = if order_by.is_empty() {
                // No surviving sort (the planner elides ORDER BY that matches
                // PK scan order): stream and stop after offset+limit rows.
                gather_rows(ctx, *table, access, filter.as_ref(), plan, params, skip_take_bound())?
            } else if let Some(keep) = skip_take_bound() {
                // ORDER BY … LIMIT: bounded top-K, O(offset+limit) memory
                // instead of materializing every match (already sorted).
                gather_topk(ctx, *table, access, filter.as_ref(), plan, params, order_by, keep)?
            } else {
                // ORDER BY with no LIMIT: must materialize and sort in full.
                let mut r = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
                sort_rows(&mut r, order_by);
                r
            };
            let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
            let take = limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
            let mut out = Vec::new();
            for row in rows.into_iter().skip(skip).take(take) {
                let mut orow = Vec::with_capacity(projection.len());
                for p in projection {
                    orow.push(match p {
                        Projection::Column(i) => row
                            .get(*i as usize)
                            .cloned()
                            .ok_or_else(|| internal("projection column"))?,
                        Projection::Expr { program, .. } => program.eval(&row, params)?,
                    });
                }
                out.push(orow);
            }
            let columns = projection
                .iter()
                .map(|p| match p {
                    Projection::Column(i) => t
                        .columns
                        .get(*i as usize)
                        .map(|c| c.name.clone())
                        .ok_or_else(|| internal("projection column name")),
                    Projection::Expr { name, .. } => Ok(name.clone()),
                })
                .collect::<Result<Vec<String>>>()?;
            Ok(ExecResult::Rows { columns, rows: out })
        }

        PlanStmt::Insert {
            table,
            rows,
            with_check,
        } => {
            let t = table_def(schema, *table)?;
            // Bind-time `now()`: captured exactly once per execute() call so
            // every DEFAULT now() in a multi-row INSERT gets the same value
            // (reviewed determinism requirement).
            let now = now_micros();
            // `applied` = rows fully inserted before the current one.
            for (applied, row_spec) in rows.iter().enumerate() {
                let row = match build_insert_row(t, plan, params, row_spec, now) {
                    Ok(row) => row,
                    Err(e) => {
                        // Row construction touches nothing; only rows already
                        // inserted count as partial effects.
                        *partial = applied > 0;
                        return Err(e);
                    }
                };
                // RLS WITH CHECK on the new row (before the engine's PK/unique
                // pre-checks): NULL and FALSE both reject (§3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = applied > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = applied > 0;
                            return Err(e);
                        }
                    }
                }
                if let Err(e) = ctx.insert_row(*table, &row) {
                    // A pre-check failure left even this row unapplied, so
                    // the statement is partial only if earlier rows landed.
                    *partial = applied > 0 || !precheck_failure(&e);
                    return Err(e);
                }
            }
            Ok(ExecResult::Affected(rows.len() as u64))
        }

        PlanStmt::Update {
            table,
            access,
            filter,
            set,
            with_check,
        } => {
            let t = table_def(schema, *table)?;
            // Collect-then-mutate: gather the matching CURRENT rows first
            // (read-only; a failure here has no effects).
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            let mut affected = 0u64;
            for old in &old_rows {
                let new_row = (|| -> Result<Vec<Value>> {
                    let mut new_row = old.clone();
                    for (c, program) in set {
                        // SQL semantics: ALL set-expressions evaluate against
                        // the OLD row, not against earlier assignments.
                        let slot = new_row
                            .get_mut(*c as usize)
                            .ok_or_else(|| internal("SET column"))?;
                        *slot = program.eval(old, params)?;
                    }
                    Ok(new_row)
                })();
                let new_row = match new_row {
                    Ok(r) => r,
                    Err(e) => {
                        // Evaluation is side-effect-free; only rows already
                        // updated count.
                        *partial = affected > 0;
                        return Err(e);
                    }
                };
                // RLS WITH CHECK on the post-image (NULL and FALSE reject, §3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &new_row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = affected > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = affected > 0;
                            return Err(e);
                        }
                    }
                }
                match ctx.update_by_pk(*table, &new_row) {
                    Ok(true) => affected += 1,
                    Ok(false) => {} // row vanished: nothing changed
                    Err(e) => {
                        *partial = affected > 0 || !precheck_failure(&e);
                        return Err(e);
                    }
                }
            }
            Ok(ExecResult::Affected(affected))
        }

        PlanStmt::Delete {
            table,
            access,
            filter,
        } => {
            let t = table_def(schema, *table)?;
            // Gather full old rows (the residual filter needs them), then
            // delete by PK values extracted from each row.
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            let mut affected = 0u64;
            for old in &old_rows {
                let mut pk = Vec::with_capacity(t.primary_key.len());
                for &i in &t.primary_key {
                    let v = match old.get(i as usize) {
                        Some(v) => v.clone(),
                        None => {
                            *partial = affected > 0;
                            return Err(internal("PK column"));
                        }
                    };
                    pk.push(v);
                }
                match ctx.delete_by_pk(*table, &pk) {
                    Ok(true) => affected += 1,
                    Ok(false) => {}
                    Err(e) => {
                        // delete_by_pk has no pre-check failure class: any
                        // error may have fired mid index maintenance.
                        *partial = true;
                        return Err(e);
                    }
                }
            }
            Ok(ExecResult::Affected(affected))
        }

        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Err(Error::Unsupported(
            "transaction control cannot be executed as a plan; \
             use Database::begin() and WriteSession::commit()/rollback()"
                .into(),
        )),
    }
}

/// Resolve one INSERT row spec (params/consts/defaults) to concrete values.
/// Pure: touches no transaction state.
fn build_insert_row(
    t: &TableDef,
    plan: &CompiledPlan,
    params: &[Value],
    row_spec: &[InsertSource],
    now: i64,
) -> Result<Vec<Value>> {
    let mut row = Vec::with_capacity(row_spec.len());
    for (ci, src) in row_spec.iter().enumerate() {
        row.push(match src {
            InsertSource::Param(i) => params
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert param"))?,
            InsertSource::Const(i) => plan
                .consts
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert const"))?,
            InsertSource::Default => {
                let col = t.columns.get(ci).ok_or_else(|| internal("insert col"))?;
                match &col.default {
                    Some(DefaultExpr::Const(v)) => v.clone(),
                    Some(DefaultExpr::Now) => Value::Timestamp(now),
                    None => Value::Null, // plan-validated: column is nullable
                }
            }
        });
    }
    Ok(row)
}

pub(crate) fn validate_params(plan: &CompiledPlan, params: &[Value]) -> Result<()> {
    if params.len() != plan.n_params as usize {
        return Err(Error::WrongParamCount {
            expected: plan.n_params as usize,
            got: params.len(),
        });
    }
    for (i, pt) in plan.param_types.iter().enumerate() {
        if let (Some(t), Some(v)) = (pt, params.get(i)) {
            if !v.fits(*t) {
                return Err(Error::TypeMismatch(format!(
                    "parameter ${} is {}, statement requires {}",
                    i + 1,
                    v.type_name(),
                    t
                )));
            }
        }
    }
    Ok(())
}

fn table_def(schema: &Schema, table: u32) -> Result<&TableDef> {
    schema
        .table(table)
        .ok_or_else(|| internal("table id out of range"))
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
    })
}

/// Fetch the candidate rows for an access path and apply the residual filter.
fn gather_rows(
    ctx: &mut dyn TxnCtx,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    plan: &CompiledPlan,
    params: &[Value],
    cap: Option<usize>,
) -> Result<Vec<Vec<Value>>> {
    // Scan paths push the filter AND the cap down into the (possibly
    // streaming) scan; point paths return at most one row and filter here.
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
                ctx.get_by_index(table, *index_no, &v)?.into_iter().collect()
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
fn gather_topk(
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
        // Point paths yield at most one row: gather it, sort trivially, cap.
        AccessPath::PkPoint(_) | AccessPath::IndexPoint { .. } => {
            let mut r = gather_rows(ctx, table, access, filter, plan, params, None)?;
            sort_rows(&mut r, order_by);
            r.truncate(keep);
            Ok(r)
        }
    }
}

fn sort_rows(rows: &mut [Vec<Value>], order_by: &[(u16, bool)]) {
    rows.sort_by(|a, b| cmp_rows(a, b, order_by));
}

/// Total sort order over two rows for an `ORDER BY` spec (column index +
/// descending flag), NULLS FIRST ascending. Shared by [`sort_rows`] and the
/// streaming top-K heap.
fn cmp_rows(a: &[Value], b: &[Value], order_by: &[(u16, bool)]) -> Ordering {
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

/// Microseconds since the Unix epoch, captured once per execute() call.
fn now_micros() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(_) => 0, // clock before the epoch: store 0 rather than panic
    }
}
