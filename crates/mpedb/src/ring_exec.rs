//! Phase-2 group commit: the facade side of the intent ring.
//!
//! Contended autocommit DML routes through `mpedb_core::ring`: the writer
//! that wins the lock becomes *leader* and executes every pending intent
//! inside its own transaction — N writes, one meta flip, one msync. Each
//! intent runs under a statement savepoint, so one failing intent rolls back
//! alone and the rest of the batch commits (per-intent errors travel back
//! through the slot). When the savepoint cannot undo the failure exactly —
//! a statement that applied part of itself in place on a page an earlier
//! batch member had already dirtied — the leader restarts the whole round
//! with that member's error pre-decided (`undo_is_exact`; DESIGN.md §5.3).
//!
//! Correctness contract (see `ring.rs`): results + `committed_in_txn` stamps
//! are staged BEFORE the flip; posting/waking happens after. A leader dying
//! at any instruction is recovered by the next lock holder via
//! `recover_orphans` — committed batches get their staged results posted,
//! uncommitted ones re-execute from scratch.
//!
//! # Batch ordering: key-locality drain
//!
//! The leader executes the drained batch in **key-locality order** — sorted
//! by `(written table id, key rank, materialized key bytes, slot idx)` —
//! instead of raw slot order. Adjacent-key mutations inside the one COW
//! transaction then share root-to-leaf page copies (a page dirtied by intent
//! k is mutated in place by intent k+1), shrinking the pages copied per
//! batch and, in `durability = commit`, the msync byte range and run count.
//! The key is computable without executing: `KeyAccess::Point` footprints
//! resolve every PK part to keycode bytes (memcmp order == key order),
//! `Range` uses its lo bound, and `Full` (or any unresolvable key) sorts
//! last within its table.
//!
//! Why reordering is sound:
//! - Batch members are **causally concurrent**: results are staged before
//!   the flip and posted after (the `ring.rs` contract), so a writer that
//!   depends on another intent's outcome can only enqueue after that
//!   intent's batch committed — dependent intents never share a batch.
//! - Concurrent autocommit writers have NO ordering guarantee and never had
//!   one: enqueue picks slots via a pid-randomized EMPTY-slot scan, so slot
//!   order was already arbitrary w.r.t. arrival. The sort is a free choice
//!   of linearization within one meta flip.
//! - Intents with the SAME (table, key bytes) have identical sort keys, so
//!   the slot-idx tiebreak preserves their relative slot order — duplicate-PK
//!   insert races and same-key insert/delete pairs resolve within a batch
//!   exactly as before. Only cross-key relative order changes, and cross-key
//!   point ops commute. The one observable difference: a Point write and an
//!   OVERLAPPING Range/Full write in the same batch may swap relative order —
//!   both are valid serializations of causally concurrent statements.
//! - Per-intent savepoints (plus the round restart for the failures a
//!   savepoint cannot undo) capture state at each intent's own start, so
//!   failure isolation is order-independent; recovery (`recover_orphans`)
//!   is keyed by slot idx + stamp, never by execution order — an uncommitted
//!   batch re-executes under the next leader with the same deterministic rule.
//!
//! `MPEDB_NO_BATCH_ROUTING=1` (alias `MPEDB_RING_NO_SORT=1`) restores the
//! historical slot-order drain for A/B measurement. `MPEDB_RING_STATS=1`
//! emits one `mpedb-ring-batch` stderr line per committed batch (never
//! enable in throughput arms — the writes perturb timing).

use crate::exec::{exec_stmt_triggered, resolve_part, WriteCtx};
use crate::trigger::TriggerSet;
use crate::{Database, ExecResult};
use mpedb_core::{row, PendingIntent, WriteTxn};
use mpedb_sql::{AccessPath, CompiledPlan, InsertSource, PlanStmt, Projection};
use mpedb_types::expr::{Instr, ScalarFn};
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{
    keycode, Concurrency, DefaultExpr, Error, KeyAccess, KeyPart, PlanHash, Result, Value,
};
use std::sync::Arc;
use std::time::Instant;

const SEP: char = '\x1f';

/// Serialize statement parameters for a ring slot.
pub(crate) fn encode_params(params: &[Value]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + params.len() * 12);
    buf.extend_from_slice(&(params.len() as u16).to_le_bytes());
    for v in params {
        write_value(&mut buf, v);
    }
    buf
}

fn decode_params(buf: &[u8]) -> Result<Vec<Value>> {
    if buf.len() < 2 {
        return Err(Error::Corrupt("truncated intent params".into()));
    }
    let n = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_value(buf, &mut pos)?);
    }
    Ok(out)
}

/// Error → (code, message) for the 126-byte slot field. Field strings are
/// joined with 0x1f; truncation degrades messages, never safety.
pub(crate) fn encode_error(e: &Error) -> (u32, Vec<u8>) {
    match e {
        Error::PrimaryKeyViolation { table } => (1, table.as_bytes().to_vec()),
        Error::UniqueViolation { table, constraint } => {
            (2, format!("{table}{SEP}{constraint}").into_bytes())
        }
        Error::NotNullViolation { table, column } => {
            (3, format!("{table}{SEP}{column}").into_bytes())
        }
        Error::CheckViolation { table, column, expr } => {
            (4, format!("{table}{SEP}{column}{SEP}{expr}").into_bytes())
        }
        Error::TypeMismatch(m) => (5, m.as_bytes().to_vec()),
        Error::WrongParamCount { expected, got } => {
            (6, format!("{expected}{SEP}{got}").into_bytes())
        }
        Error::UnknownPlan(h) => (7, h.to_string().into_bytes()),
        Error::PlanInvalidated => (8, Vec::new()),
        Error::DbFull => (9, Vec::new()),
        Error::Corrupt(m) => (10, m.as_bytes().to_vec()),
        other => (255, other.to_string().into_bytes()),
    }
}

pub(crate) fn decode_ring_result(r: mpedb_core::RingResult) -> Result<ExecResult> {
    if r.err_code == 0 {
        return Ok(ExecResult::Affected(r.affected));
    }
    let msg = String::from_utf8_lossy(&r.err_msg).into_owned();
    let mut parts = msg.split(SEP);
    let mut next = || parts.next().unwrap_or("").to_owned();
    Err(match r.err_code {
        1 => Error::PrimaryKeyViolation { table: next() },
        2 => Error::UniqueViolation {
            table: next(),
            constraint: next(),
        },
        3 => Error::NotNullViolation {
            table: next(),
            column: next(),
        },
        4 => Error::CheckViolation {
            table: next(),
            column: next(),
            expr: next(),
        },
        5 => Error::TypeMismatch(msg),
        6 => {
            let expected = next().parse().unwrap_or(0);
            let got = next().parse().unwrap_or(0);
            Error::WrongParamCount { expected, got }
        }
        7 => msg
            .parse::<PlanHash>()
            .map(Error::UnknownPlan)
            .unwrap_or_else(|_| Error::PlanInvalidated),
        8 => Error::PlanInvalidated,
        9 => Error::DbFull,
        10 => Error::Corrupt(msg),
        _ => Error::Internal(format!("batched execution failed: {msg}")),
    })
}

// ---------------------------------------------- key-locality batch ordering

/// Kill-switch for the key-locality drain order (default ON). Setting
/// `MPEDB_NO_BATCH_ROUTING=1` (or the alias `MPEDB_RING_NO_SORT=1`) restores
/// the historical slot-order drain exactly. Read once per process, mirroring
/// [`ring_enabled`]'s `MPEDB_NO_RING` — set it on the stress parent so
/// workers inherit one arm.
fn sort_enabled() -> bool {
    static KILL: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("MPEDB_NO_BATCH_ROUTING").is_ok()
            || std::env::var("MPEDB_RING_NO_SORT").is_ok()
    });
    !*KILL
}

/// Per-batch instrumentation on stderr (`mpedb-ring-batch` lines).
fn stats_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_RING_STATS").is_ok());
    *ON
}

/// Rank within one table's bucket: keyed accesses first (ordered by their
/// memcmp-ordered key bytes), no-key accesses (Full / unresolvable) last.
const RANK_KEYED: u8 = 0;
const RANK_NO_KEY: u8 = 1;

/// `(written table id, rank, key bytes, slot idx)`. keycode is
/// memcmp-ordered, so byte order == key order; the slot-idx tiebreak keeps
/// same-key intents in their historical slot order.
type SortKey = (u32, u8, Vec<u8>, u32);

/// An intent with its plan loaded and its params decoded exactly once,
/// *before* ordering ("buy once, cache what you bought"): the sort key and
/// the execution loop both reuse them, so ordering adds no second plan-cache
/// probe and no second param decode. `Err` carries exactly the error the old
/// in-loop path produced; it is staged per-intent as before.
struct PreparedIntent {
    intent: PendingIntent,
    prepared: Result<(Arc<CompiledPlan>, Vec<Value>)>,
}

/// The checks mirror the old `execute_intent` prelude in the same order, so
/// per-intent errors are byte-identical through the slot.
fn prepare_intent(db: &Database, intent: PendingIntent) -> PreparedIntent {
    let prepared = (|| {
        let plan = db.cached_or_load(&intent.hash)?;
        if plan.footprint.read_only
            || matches!(
                plan.stmt,
                PlanStmt::Begin
                    | PlanStmt::Commit
                    | PlanStmt::Rollback
                    | PlanStmt::Savepoint(_)
                    | PlanStmt::Release(_)
                    | PlanStmt::RollbackTo(_)
            )
        {
            return Err(Error::Unsupported(
                "only DML plans may enter the intent ring".into(),
            ));
        }
        // A plan calling a host UDF is CONNECTION-LOCAL (design/DESIGN-UDF.md):
        // its closures live in the enqueuing process's registry, so this leader
        // must not execute it — resolving the name against OUR registry could
        // call a different function of the same name, which is a wrong answer.
        // Unreachable in practice (such a plan is never published to the shared
        // registry, and `run_write_plan` keeps it off the ring), so this is the
        // belt to that braces: an explicit refusal, staged per-intent like any
        // other prepare error, never a silent mis-resolution.
        if plan.contains_host_call() {
            return Err(Error::Unsupported(
                "a statement calling a host-registered UDF is connection-local \
                 and cannot be executed by another connection's group-commit \
                 leader"
                    .into(),
            ));
        }
        let params = decode_params(&intent.params)?;
        Ok((plan, params))
    })();
    PreparedIntent { intent, prepared }
}

fn resolve_key_bytes(parts: &[KeyPart], plan: &CompiledPlan, params: &[Value]) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(parts.len());
    for p in parts {
        vals.push(resolve_part(p, plan, params).ok()?);
    }
    Some(keycode::encode_key(&vals))
}

/// Deterministic locality key, computed without executing anything. A NULL
/// key part encodes fine under keycode; the intent then simply misses at
/// execution (`pk = NULL` is UNKNOWN), so its placement is irrelevant to its
/// outcome.
fn locality_key(p: &PreparedIntent) -> SortKey {
    let idx = p.intent.idx;
    let Ok((plan, params)) = &p.prepared else {
        // unknown/undecodable plans sort last globally; their error is
        // staged per-intent regardless of position
        return (u32::MAX, RANK_NO_KEY, Vec::new(), idx);
    };
    // DML writes exactly one table. A degenerate footprint with an EMPTY write
    // set sorts last under `u32::MAX` — still deterministic and > every valid
    // table id; execute_prepared rejects read-only plans regardless. (This was
    // `trailing_zeros()` over a u128 bitmap; the set is sparse now, so the
    // written table is simply its first — and only — element.)
    let table = plan.footprint.tables_written.first().unwrap_or(u32::MAX);
    let (rank, key) = match &plan.footprint.key_access {
        KeyAccess::Point(parts) => match resolve_key_bytes(parts, plan, params) {
            Some(k) => (RANK_KEYED, k),
            None => (RANK_NO_KEY, Vec::new()),
        },
        KeyAccess::Range { lo: Some(lo), .. } => {
            match resolve_key_bytes(&lo.parts, plan, params) {
                Some(k) => (RANK_KEYED, k),
                None => (RANK_NO_KEY, Vec::new()),
            }
        }
        // unbounded below: the scan starts at the table's first key
        KeyAccess::Range { lo: None, .. } => (RANK_KEYED, Vec::new()),
        KeyAccess::Full => (RANK_NO_KEY, Vec::new()),
    };
    (table, rank, key, idx)
}

/// Execute the CALLER'S OWN statement inside the writer transaction it holds,
/// with this connection's host UDF closures in scope (design/DESIGN-UDF.md).
///
/// Only the OWN statement gets them, never a drained foreign intent: the
/// closures belong to this connection, and `prepare_intent` refuses a host-call
/// intent outright (a leader must never run another connection's UDF name
/// against its own registry — same name, different function).
///
/// This changes nothing about the ring protocol (§5.3): it swaps which `dyn
/// TxnCtx` the statement executes against, inside the same savepoint, at the
/// same point in the round. No staging, posting, commit, or release ordering is
/// touched.
/// Tell the transaction which key this statement's change is confined to, for
/// change notification (#139 S2). The footprint already knows: `KeyAccess`
/// resolves to concrete key bytes from (plan, params) alone, which is the same
/// trick `locality_key` above uses to sort a batch without executing it.
///
/// Every written table is hinted, with 0 meaning "somewhere in here". Hinting
/// only the resolvable ones would be a lie by omission: a batch where
/// statement 1 rewrites the whole table and statement 2 touches key K would
/// then advertise K, and a listener watching a different key would sleep
/// through statement 1.
/// The columns a statement's outcome depends on, or `None` for "all of them".
///
/// Shared by execution (`widen_guard`) and by declaration
/// (`Database::begin_guarded_with`) precisely so the two cannot drift: a
/// declaration that summarised columns differently from the execution it
/// declares would be a guarantee that changes depending on which branch ran.
///
/// **UPDATE** is exact: `PlanStmt::Update` names what it assigns, and
/// `Instr::PushCol` is the only instruction that reads the row, so the true
/// footprint is (assigned) ∪ (read by its expressions and filter). `SET ord =
/// $1` and `SET body = $1` read nothing, which is why a move and an edit on one
/// row stop conflicting; write `SET ord = ord + 1` and `ord` joins the mask,
/// because a concurrent change to it would have altered the result.
///
/// **SELECT** is exact only in the shape where the question has one answer: a
/// single table, no join, no aggregate, no window, no DISTINCT, no ORDER BY.
/// Then the result is a function of the projected columns, the filter's
/// columns, and nothing else — a concurrent change to any other column of the
/// same row cannot alter what this statement returned, so it cannot alter the
/// decision made from it. Every richer shape (a join's row count, an
/// aggregate's input set, a LIMIT's ordering) can turn on a column that appears
/// nowhere in the plan's expressions, so those keep the whole row. A statement
/// carrying subplans keeps the whole row for the same reason: a subquery's
/// column reads live on the subplan, not in any field examined here.
///
/// **INSERT and DELETE** are always all columns: one creates every column and
/// the other removes every column.
pub(crate) fn plan_cols(plan: &CompiledPlan) -> Option<u64> {
    // A subquery reads whatever it reads — including other columns of THIS
    // table — and none of it appears in the fields examined below. It arrives
    // through a parameter slot, not through `Instr::PushCol`, so the masks are
    // blind to it. This covers UPDATE's `SET x = (SELECT …)` as well as a
    // SELECT with an `IN (SELECT …)`.
    if !plan.subplans.is_empty() {
        return None;
    }
    match &plan.stmt {
        PlanStmt::Update { set, filter, .. } => {
            let mut m = filter.as_ref().map_or(0u64, |f| f.cols_read_mask());
            for (col, e) in set {
                m |= 1u64 << (col & 63);
                m |= e.cols_read_mask();
            }
            Some(m)
        }
        PlanStmt::Select(sp) => {
            if !sp.joins.is_empty()
                || sp.aggregate.is_some()
                || !sp.windows.is_empty()
                || sp.distinct
                || !sp.order_by.is_empty()
                || sp.joined_filter.is_some()
                || sp.post_filter.is_some()
                || sp.limit.is_some()
                || sp.offset.is_some()
            {
                return None;
            }
            let mut m = sp.filter.as_ref().map_or(0u64, |f| f.cols_read_mask());
            for p in &sp.projection {
                match p {
                    Projection::Column(c) => m |= 1u64 << (c & 63),
                    Projection::Expr { program, .. } => m |= program.cols_read_mask(),
                }
            }
            Some(m)
        }
        _ => None,
    }
}

/// The byte range inside a value a statement rewrites, resolved against
/// `params`, or `None` for "the whole value" (#151).
///
/// **It is already in the request.** `UPDATE … SET body = splice(body, $2, $3,
/// $4)` carries its own range in its parameters, exactly as `WHERE id = $1`
/// carries its key — so the surface can be decided before a byte is touched,
/// and a collision is one integer comparison rather than a page read. That is
/// the same property that made declaring keys work (#148), applied one level
/// finer.
///
/// Recognised only in the shape where the answer is exact: a single-column
/// UPDATE whose value is `splice(<that same column>, at, remove, …)` with
/// constant-or-parameter offsets. Anything else rewrites the whole value, which
/// is what `None` says.
pub(crate) fn plan_range(plan: &CompiledPlan, params: &[Value]) -> Option<(u32, u32)> {
    let PlanStmt::Update { set, .. } = &plan.stmt else {
        return None;
    };
    let [(col, prog)] = set.as_slice() else {
        return None;
    };
    // The shape: PushCol(col), <at>, <remove>, <insert>, Call(Splice, 4).
    let instrs = &prog.instrs;
    let (Some(Instr::PushCol(c0)), Some(Instr::Call(ScalarFn::Splice, 4))) =
        (instrs.first(), instrs.last())
    else {
        return None;
    };
    if c0 != col {
        // Splicing one column into another is a whole-value write of the
        // target, not a sub-edit of it.
        return None;
    }
    let at = const_or_param(&instrs[1], plan, params)?;
    let remove = const_or_param(&instrs[2], plan, params)?;
    let (at, remove) = (u32::try_from(at).ok()?, u32::try_from(remove).ok()?);
    Some((at, at.checked_add(remove)?))
}

/// **Carry a pending sub-edit's offset forward, in place (#151).**
///
/// The client computed `at` against the version it was shown. Between then and
/// now, other commits may have inserted or deleted text *earlier in the same
/// cell*, so `at` no longer points where the client meant. This rewrites it to
/// where it means the same thing in the value as it stands — and refuses when
/// the two edits are genuinely about the same bytes.
///
/// Without it the guard has to treat every earlier edit as invalidating: an
/// edit near the start of a cell would refuse every pending edit after it, and
/// fifty people editing one paragraph would serialize on whoever typed first.
///
/// Returns `Err(WriteConflict)` on a real collision — **at execution rather
/// than at commit**, which is earlier feedback for the same answer. `Ok(false)`
/// means there was nothing to rebase (not a guarded session, or not a splice),
/// and the statement runs unchanged.
pub(crate) fn rebase_splice_params(
    txn: &mut WriteTxn<'_>,
    plan: &CompiledPlan,
    params: &mut [Value],
) -> Result<bool> {
    let PlanStmt::Update { table, set, .. } = &plan.stmt else {
        return Ok(false);
    };
    let [(col, prog)] = set.as_slice() else {
        return Ok(false);
    };
    // The `at` must be a PARAMETER, not a literal: a literal offset is baked
    // into the plan and shared by every caller of that plan, so rewriting it
    // would rebase somebody else's statement.
    let instrs = &prog.instrs;
    let (Some(Instr::PushCol(c0)), Some(Instr::PushParam(at_slot)), Some(Instr::Call(ScalarFn::Splice, 4))) =
        (instrs.first(), instrs.get(1), instrs.last())
    else {
        return Ok(false);
    };
    if c0 != col {
        return Ok(false);
    }
    let at = match params.get(*at_slot as usize) {
        Some(Value::Int(n)) if *n >= 0 => *n as u64,
        _ => return Ok(false),
    };
    let remove = match const_or_param(&instrs[2], plan, params) {
        Some(n) => n as u64,
        None => return Ok(false),
    };
    let key = match plan_key(plan, params) {
        Some(k) => k,
        // Without an exact key there is no single cell to rebase within.
        None => return Ok(false),
    };
    match txn.rebase_splice(*table, key, *col, at, remove) {
        Some(mpedb_core::shm::RebaseOutcome::At(new_at)) => {
            params[*at_slot as usize] = Value::Int(new_at as i64);
            txn.mark_rebased();
            Ok(true)
        }
        Some(mpedb_core::shm::RebaseOutcome::Collision) => Err(Error::WriteConflict),
        // The window is unwitnessable, so the offset cannot be carried forward.
        // Refusing is the same fail-safe direction every other limit takes.
        None => Err(Error::WriteConflict),
    }
}

/// How much a splice changes the value's length: `insert.len() - remove`.
/// `None` for anything that is not a splice, or whose sizes are not knowable
/// before execution.
pub(crate) fn plan_delta(plan: &CompiledPlan, params: &[Value]) -> Option<i64> {
    let PlanStmt::Update { set, .. } = &plan.stmt else {
        return None;
    };
    let [(col, prog)] = set.as_slice() else {
        return None;
    };
    let instrs = &prog.instrs;
    let (Some(Instr::PushCol(c0)), Some(Instr::Call(ScalarFn::Splice, 4))) =
        (instrs.first(), instrs.last())
    else {
        return None;
    };
    if c0 != col {
        return None;
    }
    let remove = const_or_param(&instrs[2], plan, params)?;
    let ins = match &instrs[3] {
        Instr::PushConst(k) => plan.consts.get(*k as usize)?,
        Instr::PushParam(p) => params.get(*p as usize)?,
        _ => return None,
    };
    let ins_len = match ins {
        Value::Text(t) => t.len() as i64,
        Value::Blob(b) => b.len() as i64,
        _ => return None,
    };
    Some(ins_len - remove)
}

/// A literal or a bound parameter as an integer; anything computed is `None`,
/// because a range that depends on the row is not known before execution.
fn const_or_param(i: &Instr, plan: &CompiledPlan, params: &[Value]) -> Option<i64> {
    let v: &Value = match i {
        Instr::PushConst(k) => plan.consts.get(*k as usize)?,
        Instr::PushParam(p) => params.get(*p as usize)?,
        _ => return None,
    };
    match v {
        Value::Int(n) if *n >= 0 => Some(*n),
        _ => None,
    }
}

/// The point key a statement names, resolved against `params`, or `None` when
/// it names anything other than exactly one key. `None` is "anywhere".
pub(crate) fn plan_key(plan: &CompiledPlan, params: &[Value]) -> Option<u64> {
    match &plan.footprint.key_access {
        KeyAccess::Point(parts) => resolve_key_bytes(parts, plan, params).map(|b| key_hash(&b)),
        _ => None,
    }
}


/// Widen an open guard by this statement's whole footprint (#142 G1).
///
/// READS are included, and that is the point rather than an oversight: the
/// SELECT that decided what to write is as much a part of a guarded action as
/// the INSERT that acted on it. Guarding only the writes would let another
/// commit change the row you based the decision on and still let you through —
/// a lost update wearing a guard.
///
/// Called from the same place as `hint_notify_keys`, because both want exactly
/// what has already been resolved here: the plan's footprint against these
/// params.
pub(crate) fn widen_guard(txn: &mut WriteTxn<'_>, plan: &CompiledPlan, params: &[Value]) {
    for t in plan.footprint.tables_read.iter() {
        txn.guard_widen(t);
    }
    for t in plan.footprint.tables_written.iter() {
        txn.guard_widen(t);
    }
    // #143: the key, not only the table. Arm E measured what table granularity
    // costs — two workers on different rows of one table conflicted on every
    // commit, and per-user sharding bought nothing. A statement that names a
    // single point contributes that key; anything else contributes "anywhere",
    // which is the pre-#143 behaviour and the safe direction.
    let key = match &plan.footprint.key_access {
        KeyAccess::Point(parts) => resolve_key_bytes(parts, plan, params).map(|b| key_hash(&b)),
        _ => None,
    };
    txn.guard_touch_key(key);
    if !plan.footprint.tables_written.is_empty() {
        txn.record_written_key(key);
    }

    // #144: the shard, when it can be PROVEN. A key summary saturates and a
    // per-user workload has the wrong shape for one anyway — whether two
    // actions conflict should depend on whether it is the same user, not on
    // how many users exist.
    // #146 K1: which COLUMNS. `PlanStmt::Update` names what it assigns, and
    // `Instr::PushCol` is the only instruction that reads the row — so an
    // UPDATE's true column footprint is (assigned) ∪ (read by its expressions
    // and filter), exactly.
    //
    // That exactness is the whole feature: `SET ord = $1` and `SET body = $1`
    // read nothing, so a move and an edit on one row stop conflicting. Add a
    // column reference — `SET ord = ord + 1` — and that column joins the mask,
    // because a concurrent change to it would have altered the result.
    //
    // Everything else widens to ALL columns. A DELETE removes every column; an
    // INSERT creates every column; and a SELECT that DECIDED depends on more
    // than its filter names, so narrowing it would risk losing a conflict.
    let mask = plan_cols(plan);
    let mask_all = mask.is_none();
    let mut cols: Vec<u16> = Vec::new();
    if let Some(m) = mask {
        for b in 0..64u16 {
            if m & (1u64 << b) != 0 {
                cols.push(b);
            }
        }
    }
    if mask_all {
        txn.guard_touch_col(None);
        if !plan.footprint.tables_written.is_empty() {
            txn.record_written_col(None);
        }
    } else {
        for c in cols {
            txn.guard_touch_col(Some(c));
            if !plan.footprint.tables_written.is_empty() {
                txn.record_written_col(Some(c));
            }
        }
    }

    // #151: WHERE in the value, not only which column. The range rides the
    // same declaration the key does.
    let rng = plan_range(plan, params);
    txn.guard_touch_range(rng);
    if !plan.footprint.tables_written.is_empty() {
        txn.record_written_range(rng);
        // The length change is what a LATER pending edit needs in order to
        // carry its own offset across this commit. Publishing the range without
        // it would say where this edit was but not how far it moved everything
        // after it — half an answer, and the half that does not compose.
        if let Some(d) = plan_delta(plan, params) {
            txn.record_written_delta(d);
        }
    }

    let sh = plan_shard(plan, params);
    txn.guard_shard(sh);
    if !plan.footprint.tables_written.is_empty() {
        txn.record_written_shard(sh);
    }
}

/// The shard identity this statement runs under, or `None` when it cannot be
/// proven (#144).
///
/// **The coverage requirement is the whole soundness argument.** Two sessions
/// with different `app.tenant` values cannot touch the same row *because the
/// policy on each touched table keeps them apart* — on the write path too
/// (`policy_store::validate_policy_write`). Touch one table without such a
/// policy and that proof is gone, so the answer is `None`: unknown, conflicts
/// with everything, exactly the pre-#144 behaviour.
///
/// The values come from the tail of the resolved params, where
/// `session::resolve_params` already bound them — the same vector this
/// function is handed.
fn plan_shard(plan: &CompiledPlan, params: &[Value]) -> Option<u64> {
    if plan.context_keys.is_empty() {
        return None;
    }
    // Every table this statement touches — read or written — must carry a
    // policy. `plan.policies` is the set the binder stamped, so this is the
    // compiler's own record of what it folded in.
    let covered = |t: u32| plan.policies.iter().any(|p| p.table == t);
    if !plan.footprint.tables_read.iter().all(covered)
        || !plan.footprint.tables_written.iter().all(covered)
    {
        return None;
    }
    // Context slots are the last `context_keys.len()` params (plan/mod.rs).
    let base = params.len().checked_sub(plan.context_keys.len())?;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    for (k, v) in plan.context_keys.iter().zip(&params[base..]) {
        buf.extend_from_slice(k.as_bytes());
        buf.push(0x1f);
        write_value(&mut buf, v);
        buf.push(0x1e);
    }
    // Never 0: 0 is a legitimate hash and `Option` already carries "unknown",
    // but keeping it nonzero means a zeroed ring slot can never read as a
    // valid shard.
    Some(key_hash(&buf) | 1)
}

pub(crate) fn hint_notify_keys(txn: &mut WriteTxn<'_>, plan: &CompiledPlan, params: &[Value]) {
    let written: Vec<u32> = plan.footprint.tables_written.iter().collect();
    // A key names one table's key space. With several written tables there is
    // no single key space for it to name.
    let region: Option<Vec<u8>> = if written.len() == 1 {
        match &plan.footprint.key_access {
            // A point is its own region: the whole key.
            KeyAccess::Point(parts) => resolve_key_bytes(parts, plan, params),
            // #141 N2: a bounded range names a region too, and the footprint
            // has known it all along — S2 threw it away and published
            // "somewhere in this table" for every range write.
            //
            // `keycode` is memcmp-ordered, so every key between `lo` and `hi`
            // shares their common byte prefix. Inclusivity does not matter:
            // excluding an endpoint only removes keys from a region that still
            // contains all the rest, and the region is allowed to be wider
            // than the truth — never narrower.
            KeyAccess::Range { lo: Some(lo), hi: Some(hi) } => {
                match (resolve_key_bytes(&lo.parts, plan, params), resolve_key_bytes(&hi.parts, plan, params)) {
                    (Some(l), Some(h)) => {
                        let n = l.iter().zip(&h).take_while(|(a, b)| a == b).count();
                        (n > 0).then(|| l[..n].to_vec())
                    }
                    _ => None,
                }
            }
            // Half-open below or above: unbounded on one side, so no prefix
            // bounds it. Unknown is the honest answer.
            _ => None,
        }
    } else {
        None
    };
    for t in written {
        txn.hint_notify_key(t, region.as_deref());
    }
}

fn exec_own(
    db: &Database,
    txn: &mut WriteTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
    triggers: &TriggerSet,
    partial: &mut bool,
) -> Result<ExecResult> {
    let tables = db.host_tables(plan);
    let host: Option<&dyn mpedb_types::HostFns> =
        tables.as_ref().map(|(f, _, _)| f as &dyn mpedb_types::HostFns);
    let aggs: Option<&dyn mpedb_types::HostAggs> =
        tables.as_ref().map(|(_, a, _)| a as &dyn mpedb_types::HostAggs);
    let colls: Option<&dyn mpedb_types::HostColls> =
        tables.as_ref().map(|(_, _, c)| c as &dyn mpedb_types::HostColls);
    hint_notify_keys(txn, plan, params);
    let mut ctx = WriteCtx::new(txn, host, aggs, colls);
    exec_stmt_triggered(&mut ctx, &db.schema(), plan, params, partial, triggers, 0)
}

/// Execute one prepared foreign intent inside the leader's transaction.
///
/// `partial` is the executor's statement-atomicity out-flag, propagated to the
/// caller because the leader's per-intent undo depends on it (§5.3): a failure
/// with `*partial == true` may have mutated pages an earlier intent in this
/// same batch already made dirty, and `WriteTxn::rollback_to` cannot undo those.
fn execute_prepared(
    db: &Database,
    txn: &mut WriteTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
    triggers: &TriggerSet,
    partial: &mut bool,
) -> Result<u64> {
    hint_notify_keys(txn, plan, params);
    match exec_stmt_triggered(txn, &db.schema(), plan, params, partial, triggers, 0)? {
        ExecResult::Affected(n) => Ok(n),
        _ => Err(Error::Internal("write plan returned rows".into())),
    }
}

/// Can the cheap per-statement savepoint undo this failure EXACTLY?
///
/// `rollback_to` restores root pointers and page accounting. That is exact for
/// everything a statement COW-allocated (the restore drops those pages), but it
/// cannot undo an in-place mutation of a page that was ALREADY dirty when the
/// savepoint was taken — the page id does not change, so the restored root
/// points at the same, already-mutated page. Nor does any savepoint cover the
/// extent allocator. So the undo is exact iff either:
///
/// * the statement applied nothing at all (`!partial` — the executor's
///   contract is that this never under-reports), or
/// * the transaction was pristine when the statement began AND the statement
///   did not touch extents, in which case every page it wrote it also
///   allocated.
///
/// When neither holds the round is torn and the leader must `restart` (§5.3).
fn undo_is_exact(txn: &WriteTxn<'_>, partial: bool, pristine_before: bool) -> bool {
    !partial || (pristine_before && txn.extents_untouched())
}

/// Leader round: drain all READY intents into `txn` (one savepoint each),
/// optionally execute the caller's own statement last, group-commit, post
/// results, wake waiters. Returns the own statement's outcome.
///
/// On commit failure nothing is unstaged here: the stamps exceed the
/// committed txn id, so the next leader's `recover_orphans` re-arms the
/// intents (doing it here would race a successor that already re-staged).
pub(crate) fn ring_enabled(db: &Database) -> bool {
    static KILL: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_NO_RING").is_ok());
    // Group commit pays when commits are expensive (a sync per commit on a
    // real disk); on µs-cheap commits the ring's wait/wake latency dominates.
    // `wal` rides the ring exactly like `commit`: one record + one fdatasync
    // per BATCH is where the sequential log shines. MPEDB_NO_RING exists for
    // A/B measurement.
    !*KILL
        // Optimistic mode commits per-writer (no group-commit leader), so the
        // ring is bypassed entirely — every autocommit write reaches
        // `lead_and_execute` on the direct path (DESIGN-PHASE3).
        && db.engine.concurrency() != Concurrency::Optimistic
        && matches!(
            db.engine.durability(),
            mpedb_types::Durability::Commit | mpedb_types::Durability::Wal
        )
}

// ============================================================ optimistic path
//
// `concurrency = "optimistic"` (DESIGN-PHASE3, default OFF). Routed here from
// `lead_and_execute` for the eligible statement class (single-table PK-point
// INSERT/UPDATE/DELETE on a table with no secondary index). Everything else in
// optimistic mode falls through to the serial direct path below.
//
// Protocol: release the writer lock we were handed, run a snapshot-pinned PREP
// off-lock (resolve the key, read the current row, build+validate+encode the
// new row), then take a SHORT critical section to (1) validate our footprint
// against the committed-footprint ring — first-committer-wins, `WriteConflict`
// on overlap — and (2) blind-apply the pre-built op. On conflict we retry
// against a fresh snapshot up to a bound, then fall back to a plain serial
// execute (guaranteed progress). The apply is the *only* tree mutation; prep's
// reads are the parallelizable work (see the ceiling analysis in DESIGN-PHASE3).

const OPT_MAX_RETRIES: u32 = 8;

/// Optional optimistic-path counters (`MPEDB_OPT_STATS=1`): committed applies,
/// WriteConflict retries, and serial fallbacks. Confirms the WriteConflict path
/// actually fires under contention; a summary line is emitted every 10k
/// applies (per process). Never enabled in throughput arms.
static OPT_APPLIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPT_CONFLICTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPT_FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn opt_stats_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_OPT_STATS").is_ok());
    *ON
}

fn opt_stats_bump(applies: u64, conflicts: u64, fallbacks: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if !opt_stats_enabled() {
        return;
    }
    let a = OPT_APPLIES.fetch_add(applies, Relaxed) + applies;
    let c = OPT_CONFLICTS.fetch_add(conflicts, Relaxed) + conflicts;
    let f = OPT_FALLBACKS.fetch_add(fallbacks, Relaxed) + fallbacks;
    if applies > 0 && a % 10_000 < applies {
        use std::io::Write;
        let _ = std::io::stderr().write_all(
            format!("mpedb-opt-stats pid={} applies={a} conflicts={c} fallbacks={f}\n",
                    std::process::id()).as_bytes(),
        );
    }
}

/// FNV-1a over the encoded key. Hash collisions only ever cause an extra
/// (false) conflict → retry, never a missed conflict, so this is sound.
fn key_hash(key: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // avoid 0 colliding with an "empty" slot's default hash on point compares
    h | 1
}

fn opt_now_micros() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Is this plan eligible for the optimistic blind-apply path?
fn optimistic_eligible(db: &Database, plan: &CompiledPlan) -> bool {
    if plan.contains_host_call() {
        // The blind-apply route builds and validates the row OFF the executor
        // (`optimistic_prep` evaluates filters with no host resolver), so a plan
        // calling a host UDF would refuse there. Route it to the serial
        // executor, which carries the connection's closures
        // (design/DESIGN-UDF.md).
        return false;
    }
    if plan.footprint.tables_written.len() != 1 {
        return false;
    }
    let Some(table) = plan.footprint.tables_written.first() else {
        return false;
    };
    if db.engine.has_secondary_index(table) {
        return false; // index maintenance defeats key-level footprints
    }
    if db.engine.table_is_fts(table) {
        // An FTS table has no `TableDef.indexes`, so `has_secondary_index` is
        // false — but the row path maintains its inverted index, which the
        // blind-apply route would skip. Route it through the serial executor.
        return false;
    }
    if db.table_has_trigger(table) {
        // The blind-apply path never calls the executor, so it would skip firing
        // ANY trigger — BEFORE or AFTER, insert/update/delete — route such tables
        // through the serial executor instead.
        return false;
    }
    match &plan.stmt {
        PlanStmt::Insert { rows, .. } => rows.len() == 1,
        PlanStmt::Update { access, .. } | PlanStmt::Delete { access, .. } => {
            matches!(access, AccessPath::PkPoint(_))
        }
        _ => false,
    }
}

/// The mutation prep decided to perform under the lock.
enum ApplyOp {
    Insert(Vec<u8>), // InsertOnly of this payload
    Upsert(Vec<u8>), // replace (UPDATE)
    Delete,
}

/// Outcome of the off-lock prep pass.
enum Prep {
    /// A mutation to validate + blind-apply, returning `Affected(affected)`.
    Apply {
        table: u32,
        key: Vec<u8>,
        key_hash: u64,
        snap: u64,
        op: ApplyOp,
        affected: u64,
    },
    /// A snapshot-INDEPENDENT terminal (row-only validation error, NULL-key
    /// no-op): return immediately, no lock needed.
    Direct(Result<ExecResult>),
    /// A snapshot-DEPENDENT terminal (PK already exists / row absent / SET
    /// evaluation error): return `outcome` only after confirming, under the
    /// lock, that our key was not touched since the snapshot.
    Confirm {
        table: u32,
        key_hash: u64,
        snap: u64,
        outcome: Result<ExecResult>,
    },
    /// Anything the fast path does not cleanly handle: run it serially.
    Fallback,
}

/// Build the prep decision against a pinned read snapshot (no writer lock).
fn optimistic_prep(db: &Database, plan: &CompiledPlan, params: &[Value]) -> Prep {
    // Only reached for an `optimistic_eligible` plan, which requires exactly
    // one written table; the fallback keeps the extraction total regardless.
    let Some(table) = plan.footprint.tables_written.first() else {
        return Prep::Fallback;
    };
    let Some(types) = db.engine.col_types(table) else {
        return Prep::Fallback;
    };
    let types = types.to_vec();
    let bundle = db.schema();
    let Some(tdef) = bundle.table(table) else {
        return Prep::Fallback;
    };
    let pk_cols = tdef.primary_key.clone();

    let r = match db.engine.begin_read() {
        Ok(r) => r,
        Err(_) => return Prep::Fallback,
    };
    let snap = r.meta.txn_id;
    let prep = optimistic_prep_inner(db, &r, plan, params, table, &types, &pk_cols, snap);
    // A snapshot eviction mid-prep means our reads may be inconsistent: fall
    // back to a serial execute rather than trust them.
    match r.finish() {
        Ok(()) => prep,
        Err(_) => Prep::Fallback,
    }
}

#[allow(clippy::too_many_arguments)]
fn optimistic_prep_inner(
    db: &Database,
    r: &mpedb_core::ReadTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
    table: u32,
    types: &[mpedb_types::ColumnType],
    pk_cols: &[u16],
    snap: u64,
) -> Prep {
    match &plan.stmt {
        PlanStmt::Insert { rows, .. } => {
            let row_spec = &rows[0];
            if row_spec.len() != types.len() {
                return Prep::Fallback;
            }
            let now = opt_now_micros();
            let mut values = Vec::with_capacity(row_spec.len());
            for (ci, src) in row_spec.iter().enumerate() {
                let v = match src {
                    InsertSource::Param(i) => match params.get(*i as usize) {
                        Some(v) => v.clone(),
                        None => return Prep::Fallback,
                    },
                    InsertSource::Const(i) => match plan.consts.get(*i as usize) {
                        Some(v) => v.clone(),
                        None => return Prep::Fallback,
                    },
                    InsertSource::Default => match db.schema().table(table).and_then(|t| t.columns.get(ci)) {
                        Some(c) => match &c.default {
                            Some(DefaultExpr::Const(v)) => v.clone(),
                            Some(DefaultExpr::Now) => Value::Timestamp(now),
                            None => Value::Null,
                        },
                        None => return Prep::Fallback,
                    },
                    // Expression cells need the dual-row eval path; fall back
                    // to the ordinary writer rather than re-implement it here.
                    InsertSource::Expr(_) => return Prep::Fallback,
                };
                values.push(v);
            }
            // Row-only validation error is snapshot-independent: return directly.
            if let Err(e) = db.engine.validate_row_public(table, &values) {
                return Prep::Direct(Err(e));
            }
            let pk_vals: Vec<Value> = pk_cols.iter().map(|&i| values[i as usize].clone()).collect();
            // The engine's own PK-tree encoding, not `encode_key`: this key is
            // applied to the tree verbatim, and a collated or TYPELESS (`any`)
            // PK column does not encode the plain way (`keycode::KeySpec`).
            let key = db.engine.pk_key(table, &pk_vals);
            let kh = key_hash(&key);
            match r.get_by_pk(table, &pk_vals) {
                Ok(Some(_)) => Prep::Confirm {
                    table,
                    key_hash: kh,
                    snap,
                    outcome: Err(Error::PrimaryKeyViolation { table: tname(db, table) }),
                },
                Ok(None) => match row::encode_row(&values, types) {
                    Ok(payload) => Prep::Apply {
                        table,
                        key,
                        key_hash: kh,
                        snap,
                        op: ApplyOp::Insert(payload),
                        affected: 1,
                    },
                    Err(_) => Prep::Fallback,
                },
                Err(_) => Prep::Fallback,
            }
        }

        PlanStmt::Update { access, filter, set, .. } => {
            let AccessPath::PkPoint(parts) = access else {
                return Prep::Fallback;
            };
            let Some(pk_vals) = resolve_pk(parts, plan, params) else {
                return Prep::Fallback;
            };
            if pk_vals.iter().any(|v| v.is_null()) {
                return Prep::Direct(Ok(ExecResult::Affected(0))); // pk = NULL matches nothing
            }
            // The engine's own PK-tree encoding, not `encode_key`: this key is
            // applied to the tree verbatim, and a collated or TYPELESS (`any`)
            // PK column does not encode the plain way (`keycode::KeySpec`).
            let key = db.engine.pk_key(table, &pk_vals);
            let kh = key_hash(&key);
            let old = match r.get_by_pk(table, &pk_vals) {
                Ok(Some(old)) => old,
                Ok(None) => {
                    return Prep::Confirm {
                        table, key_hash: kh, snap,
                        outcome: Ok(ExecResult::Affected(0)),
                    }
                }
                Err(_) => return Prep::Fallback,
            };
            let mut stack = Vec::new();
            if let Some(f) = filter {
                match f.eval_filter(&mut stack, &old, params) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Prep::Confirm {
                            table, key_hash: kh, snap,
                            outcome: Ok(ExecResult::Affected(0)),
                        }
                    }
                    Err(e) => {
                        return Prep::Confirm { table, key_hash: kh, snap, outcome: Err(e) }
                    }
                }
            }
            let mut new_row = old.clone();
            for (c, prog) in set {
                match prog.eval(&old, params) {
                    Ok(v) => {
                        let Some(slot) = new_row.get_mut(*c as usize) else {
                            return Prep::Fallback;
                        };
                        *slot = v;
                    }
                    Err(e) => return Prep::Confirm { table, key_hash: kh, snap, outcome: Err(e) },
                }
            }
            if let Err(e) = db.engine.validate_row_public(table, &new_row) {
                return Prep::Confirm { table, key_hash: kh, snap, outcome: Err(e) };
            }
            match row::encode_row(&new_row, types) {
                Ok(payload) => Prep::Apply {
                    table, key, key_hash: kh, snap, op: ApplyOp::Upsert(payload), affected: 1,
                },
                Err(_) => Prep::Fallback,
            }
        }

        PlanStmt::Delete { access, filter, .. } => {
            let AccessPath::PkPoint(parts) = access else {
                return Prep::Fallback;
            };
            let Some(pk_vals) = resolve_pk(parts, plan, params) else {
                return Prep::Fallback;
            };
            if pk_vals.iter().any(|v| v.is_null()) {
                return Prep::Direct(Ok(ExecResult::Affected(0)));
            }
            // The engine's own PK-tree encoding, not `encode_key`: this key is
            // applied to the tree verbatim, and a collated or TYPELESS (`any`)
            // PK column does not encode the plain way (`keycode::KeySpec`).
            let key = db.engine.pk_key(table, &pk_vals);
            let kh = key_hash(&key);
            let old = match r.get_by_pk(table, &pk_vals) {
                Ok(Some(old)) => old,
                Ok(None) => {
                    return Prep::Confirm {
                        table, key_hash: kh, snap, outcome: Ok(ExecResult::Affected(0)),
                    }
                }
                Err(_) => return Prep::Fallback,
            };
            if let Some(f) = filter {
                let mut stack = Vec::new();
                match f.eval_filter(&mut stack, &old, params) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Prep::Confirm {
                            table, key_hash: kh, snap, outcome: Ok(ExecResult::Affected(0)),
                        }
                    }
                    Err(e) => return Prep::Confirm { table, key_hash: kh, snap, outcome: Err(e) },
                }
            }
            Prep::Apply { table, key, key_hash: kh, snap, op: ApplyOp::Delete, affected: 1 }
        }

        _ => Prep::Fallback,
    }
}

fn resolve_pk(parts: &[KeyPart], plan: &CompiledPlan, params: &[Value]) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        out.push(resolve_part(p, plan, params).ok()?);
    }
    Some(out)
}

fn tname(db: &Database, table: u32) -> String {
    db.schema()
        .table(table)
        .map(|t| t.name.clone())
        .unwrap_or_default()
}

/// Plain serial execute of one statement under a fresh writer lock — the
/// optimistic fallback (ineligible statements and exhausted-retry conflicts).
fn serial_execute(db: &Database, plan: &CompiledPlan, params: &[Value]) -> Result<ExecResult> {
    let triggers = db.trigger_set()?;
    let mut txn = db.engine.begin_write_deadline(db.busy_deadline())?;
    let mut partial = false;
    match exec_own(db, &mut txn, plan, params, &triggers, &mut partial) {
        Ok(out) => {
            txn.commit()?;
            Ok(out)
        }
        Err(e) => {
            txn.abort();
            Err(e)
        }
    }
}

/// Optimistic execution of the caller's own statement. `held` is the writer
/// lock handed to us by `lead_and_execute`; we release it immediately and run
/// the off-lock prep, so the expensive read/build/validate happens without
/// blocking other writers.
fn optimistic_execute(
    db: &Database,
    held: WriteTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
) -> Result<ExecResult> {
    held.abort(); // release the lock: prep runs off-lock

    let mut conflicts = 0u64;
    for _ in 0..OPT_MAX_RETRIES {
        match optimistic_prep(db, plan, params) {
            Prep::Fallback => {
                opt_stats_bump(0, conflicts, 1);
                return serial_execute(db, plan, params);
            }
            Prep::Direct(outcome) => {
                opt_stats_bump(1, conflicts, 0);
                return outcome;
            }
            Prep::Confirm { table, key_hash, snap, outcome } => {
                let txn = db.engine.begin_write_deadline(db.busy_deadline())?;
                if txn.optimistic_validate(snap, table, key_hash).is_err() {
                    txn.abort();
                    conflicts += 1;
                    continue; // world changed under our key: re-prep
                }
                txn.abort(); // no mutation to make
                opt_stats_bump(1, conflicts, 0);
                return outcome;
            }
            Prep::Apply { table, key, key_hash, snap, op, affected } => {
                let mut txn = db.engine.begin_write_deadline(db.busy_deadline())?;
                if txn.optimistic_validate(snap, table, key_hash).is_err() {
                    txn.abort();
                    conflicts += 1;
                    continue;
                }
                let applied = match &op {
                    ApplyOp::Insert(payload) => match txn.optimistic_insert(table, &key, payload) {
                        Ok(true) => Ok(()),
                        // PK appeared despite validation passing (hash-level
                        // false-negative is impossible here since validate
                        // covers this exact key) → real violation
                        Ok(false) => Err(Error::PrimaryKeyViolation { table: tname(db, table) }),
                        Err(e) => Err(e),
                    },
                    ApplyOp::Upsert(payload) => txn.optimistic_upsert(table, &key, payload),
                    ApplyOp::Delete => match txn.optimistic_delete(table, &key) {
                        Ok(true) => Ok(()),
                        Ok(false) => {
                            // row vanished despite validation: nothing to do
                            txn.abort();
                            opt_stats_bump(1, conflicts, 0);
                            return Ok(ExecResult::Affected(0));
                        }
                        Err(e) => Err(e),
                    },
                };
                match applied {
                    Ok(()) => {
                        txn.set_commit_point(table, key_hash);
                        txn.commit()?;
                        opt_stats_bump(1, conflicts, 0);
                        return Ok(ExecResult::Affected(affected));
                    }
                    Err(e) => {
                        txn.abort();
                        opt_stats_bump(1, conflicts, 0);
                        return Err(e);
                    }
                }
            }
        }
    }
    // retries exhausted under sustained contention: guaranteed-progress serial
    opt_stats_bump(0, conflicts, 1);
    serial_execute(db, plan, params)
}

pub(crate) fn lead_and_execute(
    db: &Database,
    mut txn: WriteTxn<'_>,
    own: Option<(&CompiledPlan, &[Value])>,
) -> Result<Option<ExecResult>> {
    // Optimistic concurrency: route the eligible statement class through the
    // off-lock prep + validate + blind-apply path (DESIGN-PHASE3). Everything
    // else in optimistic mode falls through to the serial direct path below
    // (the ring is disabled in optimistic mode, so `own` is always Some here).
    if db.engine.concurrency() == Concurrency::Optimistic {
        if let Some((plan, params)) = own {
            if optimistic_eligible(db, plan) {
                return optimistic_execute(db, txn, plan, params).map(Some);
            }
        }
    }
    // The trigger set to fire from: the leader's own gen-gated set, applied to
    // its own statement AND every foreign intent it drains (DESIGN-TRIGGERS
    // §4.5). Built here so the whole round shares one set.
    let triggers = db.trigger_set()?;
    if !ring_enabled(db) {
        // pure direct path: no scans, no staging — nobody can be enqueued
        // (enqueue is gated identically, and durability is file-frozen so
        // every attached process agrees)
        let mut own_result = None;
        if let Some((plan, params)) = own {
            let mut partial = false;
            match exec_own(db, &mut txn, plan, params, &triggers, &mut partial) {
                Ok(out) => own_result = Some(Ok(out)),
                Err(e) => {
                    txn.abort();
                    return Err(e);
                }
            }
        }
        txn.commit()?;
        return match own_result {
            None => Ok(None),
            Some(r) => r.map(Some),
        };
    }
    let ring = db.engine.ring();
    ring.recover_orphans(txn.meta.txn_id);
    ring.reclaim_dead();
    let next_txn = txn.meta.txn_id + 1;

    let intents = ring.collect_ready();
    let stats = stats_enabled();
    let exec_start = stats.then(Instant::now);
    let sorted = sort_enabled();

    let mut batch: Vec<PreparedIntent> =
        intents.into_iter().map(|i| prepare_intent(db, i)).collect();
    if sorted && batch.len() > 1 {
        // Stable sort on keys materialized once per element: the closure runs
        // once per intent (over the already-decoded plan+params), comparisons
        // are pure memcmp on the cached tuples.
        batch.sort_by_cached_key(locality_key);
    }
    // decision datum for a future Range-aware slice: how many intents land
    // in the no-key bucket under this workload
    let nokey = if stats {
        batch
            .iter()
            .filter(|p| locality_key(p).1 != RANK_KEYED)
            .count()
    } else {
        0
    };

    // §5.3 statement atomicity across a batch. A member whose failure TORE the
    // transaction (see `undo_is_exact`) cannot be undone in place, so the leader
    // throws the whole round away (`txn.restart()` — the writer lock is kept, so
    // no other process can steal these intents) and replays it with that
    // member's outcome PRE-DECIDED: on the replay it is not executed, its error
    // is staged verbatim, and the batch commits around it exactly as §5.3
    // promises. The replay re-runs its predecessors against the same committed
    // snapshot in the same order, so it reproduces the state in which that error
    // was raised — the error stays the right answer.
    //
    // Termination: every restart pre-decides one member that was not pre-decided
    // before, and a pre-decided member never executes again, so the round runs
    // at most `batch.len() + 1` times. The happy path pays NOTHING: one bool
    // read per member.
    let mut predecided: Vec<Option<(u32, Vec<u8>)>> = vec![None; batch.len()];
    let mut own_predecided: Option<Error> = None;
    let mut staged;
    let mut own_result: Option<Result<ExecResult>>;
    'round: loop {
        staged = Vec::with_capacity(batch.len());
        for (i, p) in batch.iter().enumerate() {
            if let Some((code, msg)) = &predecided[i] {
                // torn on an earlier attempt: staged, never re-executed
                ring.stage_result(p.intent.idx, 0, *code, msg, next_txn);
                staged.push((p.intent.idx, p.intent.word));
                continue;
            }
            match &p.prepared {
                Ok((plan, params)) => {
                    let pristine = txn.is_pristine();
                    let sp = txn.savepoint();
                    let mut partial = false;
                    match execute_prepared(db, &mut txn, plan, params, &triggers, &mut partial) {
                        Ok(affected) => ring.stage_result(p.intent.idx, affected, 0, &[], next_txn),
                        Err(e) => {
                            let (code, msg) = encode_error(&e);
                            if !undo_is_exact(&txn, partial, pristine) {
                                predecided[i] = Some((code, msg));
                                txn.restart();
                                continue 'round;
                            }
                            txn.rollback_to(sp);
                            ring.stage_result(p.intent.idx, 0, code, &msg, next_txn);
                        }
                    }
                }
                // plan load / param decode failed before touching the
                // transaction: stage the error directly (the old in-loop path
                // rolled back an untouched savepoint — same state, same error)
                Err(e) => {
                    let (code, msg) = encode_error(e);
                    ring.stage_result(p.intent.idx, 0, code, &msg, next_txn);
                }
            }
            staged.push((p.intent.idx, p.intent.word));
        }

        // the caller's own statement, savepointed like any other batch member
        own_result = None;
        if let Some((plan, params)) = own {
            if let Some(e) = own_predecided.take() {
                own_result = Some(Err(e));
            } else {
                let pristine = txn.is_pristine();
                let sp = txn.savepoint();
                let mut partial = false;
                match exec_own(db, &mut txn, plan, params, &triggers, &mut partial) {
                    Ok(out) => own_result = Some(Ok(out)),
                    Err(e) => {
                        if !undo_is_exact(&txn, partial, pristine) {
                            own_predecided = Some(e);
                            txn.restart();
                            continue 'round;
                        }
                        txn.rollback_to(sp);
                        own_result = Some(Err(e));
                    }
                }
            }
        }
        break;
    }

    if staged.is_empty() && matches!(own_result, Some(Err(_)) | None) {
        // nothing to commit
        txn.abort();
        return match own_result {
            Some(r) => r.map(Some),
            None => Ok(None),
        };
    }

    // Post under the lock (commit_with): after the flip the staged results
    // are authoritative, and no stale poster can outlive its incarnation.
    let page_stats = stats.then(|| txn.dirty_page_stats());
    let commit_start = stats.then(Instant::now);
    txn.commit_with(|| {
        for (idx, word) in &staged {
            ring.post_done(*idx, *word);
        }
    })?;
    if let (Some((pages, runs)), Some(t0), Some(t1)) = (page_stats, exec_start, commit_start) {
        use std::io::Write;
        // one write_all per line (like ring_debug) so multi-process output
        // never interleaves
        let line = format!(
            "mpedb-ring-batch pid={} intents={} own={} sorted={} nokey={nokey} \
             pages={pages} runs={runs} exec_us={} commit_us={}\n",
            std::process::id(),
            staged.len(),
            own.is_some() as u8,
            sorted as u8,
            t1.duration_since(t0).as_micros(),
            t1.elapsed().as_micros(),
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
    match own_result {
        None => Ok(None),
        Some(r) => r.map(Some),
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::{ColumnDef, ColumnType, Schema, TableDef};

    /// Tables `a` (id 0) and `b` (id 1), each `(id int64 PK, v int64 NULL)`.
    fn test_schema() -> Schema {
        let col = |name: &str, nullable: bool| ColumnDef { generated: None, decl: None,
            name: name.into(),
            ty: ColumnType::Int64,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: mpedb_types::Collation::Binary,
            affinity: mpedb_types::Affinity::Integer,
        };
        let table = |name: &str| TableDef {
            id: 0,
            name: name.into(),
            columns: vec![col("id", false), col("v", true)],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
        };
        Schema::new(vec![table("a"), table("b")]).unwrap()
    }

    fn prep(schema: &Schema, sql: &str, params: Vec<Value>, idx: u32) -> PreparedIntent {
        let plan = Arc::new(mpedb_sql::prepare(sql, schema).unwrap());
        let intent = PendingIntent {
            idx,
            word: 0,
            hash: plan.hash(),
            params: encode_params(&params),
        };
        PreparedIntent {
            intent,
            prepared: Ok((plan, params)),
        }
    }

    fn broken(idx: u32) -> PreparedIntent {
        let hash = PlanHash([0u8; 32]);
        PreparedIntent {
            intent: PendingIntent {
                idx,
                word: 0,
                hash,
                params: Vec::new(),
            },
            prepared: Err(Error::UnknownPlan(hash)),
        }
    }

    const INS_A: &str = "INSERT INTO a (id, v) VALUES ($1, 0)";

    #[test]
    fn point_keys_sort_numerically_via_keycode() {
        let s = test_schema();
        let two = prep(&s, INS_A, vec![Value::Int(2)], 1);
        let ten = prep(&s, INS_A, vec![Value::Int(10)], 0);
        // memcmp on keycode bytes == numeric order, not decimal-string order
        assert!(locality_key(&two) < locality_key(&ten));
        let neg = prep(&s, "DELETE FROM a WHERE id = $1", vec![Value::Int(-5)], 2);
        assert!(locality_key(&neg) < locality_key(&two));
    }

    #[test]
    fn no_key_intents_sort_last_within_their_table() {
        let s = test_schema();
        let point = prep(
            &s,
            "UPDATE a SET v = 1 WHERE id = $1",
            vec![Value::Int(i64::MAX)],
            0,
        );
        let full = prep(&s, "UPDATE a SET v = 1", vec![], 1);
        let multi = prep(
            &s,
            "INSERT INTO a (id, v) VALUES ($1, 0), ($2, 0)",
            vec![Value::Int(0), Value::Int(1)],
            2,
        );
        assert_eq!(locality_key(&full).1, RANK_NO_KEY);
        assert_eq!(locality_key(&multi).1, RANK_NO_KEY, "multi-row INSERT degrades to Full");
        assert!(locality_key(&point) < locality_key(&full));
        assert!(locality_key(&point) < locality_key(&multi));
        // ...but still ahead of the NEXT table's intents
        let b_point = prep(
            &s,
            "INSERT INTO b (id, v) VALUES ($1, 0)",
            vec![Value::Int(i64::MIN)],
            3,
        );
        assert!(locality_key(&full) < locality_key(&b_point));
    }

    #[test]
    fn intents_group_by_written_table() {
        let s = test_schema();
        let a = prep(&s, INS_A, vec![Value::Int(1_000_000)], 7);
        let b = prep(
            &s,
            "INSERT INTO b (id, v) VALUES ($1, 0)",
            vec![Value::Int(-1_000_000)],
            0,
        );
        assert_eq!(locality_key(&a).0, 0);
        assert_eq!(locality_key(&b).0, 1);
        assert!(locality_key(&a) < locality_key(&b));
    }

    #[test]
    fn range_uses_lo_bound_and_unbounded_lo_sorts_first() {
        let s = test_schema();
        let range = prep(&s, "DELETE FROM a WHERE id >= $1", vec![Value::Int(5)], 0);
        let k = locality_key(&range);
        assert_eq!(k.1, RANK_KEYED);
        assert_eq!(k.2, keycode::encode_key(&[Value::Int(5)]));
        let below = prep(&s, "DELETE FROM a WHERE id = $1", vec![Value::Int(4)], 1);
        let above = prep(&s, "DELETE FROM a WHERE id = $1", vec![Value::Int(6)], 2);
        assert!(locality_key(&below) < k);
        assert!(k < locality_key(&above));
        // lo: None — the scan starts at the table's first key
        let unbounded = prep(&s, "DELETE FROM a WHERE id <= $1", vec![Value::Int(5)], 3);
        let uk = locality_key(&unbounded);
        assert_eq!(uk.1, RANK_KEYED);
        assert!(uk < locality_key(&below));
    }

    #[test]
    fn equal_keys_fall_back_to_slot_order() {
        let s = test_schema();
        let early = prep(&s, INS_A, vec![Value::Int(7)], 3);
        let late = prep(&s, INS_A, vec![Value::Int(7)], 9);
        assert!(locality_key(&early) < locality_key(&late));
        // swap the slot assignment and the order swaps with it
        let early = prep(&s, INS_A, vec![Value::Int(7)], 9);
        let late = prep(&s, INS_A, vec![Value::Int(7)], 3);
        assert!(locality_key(&late) < locality_key(&early));
    }

    #[test]
    fn unloadable_plans_sort_last_globally() {
        let s = test_schema();
        let bad = broken(0);
        assert_eq!(locality_key(&bad).0, u32::MAX);
        let b_full = prep(&s, "UPDATE b SET v = 1", vec![], 200);
        assert!(locality_key(&b_full) < locality_key(&bad));
    }

    #[test]
    fn slot_permutations_sort_identically() {
        let s = test_schema();
        let keys = [40i64, -3, 17, 999, 0, 23];
        let want = vec![-3i64, 0, 17, 23, 40, 999];
        for perm in [[0u32, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [2, 0, 5, 1, 4, 3]] {
            let mut batch: Vec<PreparedIntent> = keys
                .iter()
                .zip(perm)
                .map(|(k, idx)| prep(&s, INS_A, vec![Value::Int(*k)], idx))
                .collect();
            batch.sort_by_cached_key(locality_key);
            let got: Vec<i64> = batch
                .iter()
                .map(|p| match &p.prepared {
                    Ok((_, params)) => match params[0] {
                        Value::Int(i) => i,
                        _ => unreachable!(),
                    },
                    Err(_) => unreachable!(),
                })
                .collect();
            assert_eq!(got, want, "slot layout {perm:?} must not change the drain order");
        }
    }
}
