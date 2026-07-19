//! Phase-2 group commit: the facade side of the intent ring.
//!
//! Contended autocommit DML routes through `mpedb_core::ring`: the writer
//! that wins the lock becomes *leader* and executes every pending intent
//! inside its own transaction — N writes, one meta flip, one msync. Each
//! intent runs under a statement savepoint, so one failing intent rolls back
//! alone and the rest of the batch commits (per-intent errors travel back
//! through the slot).
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
//! - Per-intent savepoints capture state at each intent's own start, so
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
use mpedb_sql::{AccessPath, CompiledPlan, InsertSource, PlanStmt};
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
        tables.as_ref().map(|(f, _)| f as &dyn mpedb_types::HostFns);
    let aggs: Option<&dyn mpedb_types::HostAggs> =
        tables.as_ref().map(|(_, a)| a as &dyn mpedb_types::HostAggs);
    let mut ctx = WriteCtx::new(txn, host, aggs);
    exec_stmt_triggered(&mut ctx, &db.schema(), plan, params, partial, triggers, 0)
}

/// Execute one prepared foreign intent inside the leader's transaction.
fn execute_prepared(
    db: &Database,
    txn: &mut WriteTxn<'_>,
    plan: &CompiledPlan,
    params: &[Value],
    triggers: &TriggerSet,
) -> Result<u64> {
    let mut partial = false;
    match exec_stmt_triggered(txn, &db.schema(), plan, params, &mut partial, triggers, 0)? {
        ExecResult::Affected(n) => Ok(n),
        _ => Err(Error::Internal("write plan returned rows".into())),
    }
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
    let mut txn = db.engine.begin_write()?;
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
                let txn = db.engine.begin_write()?;
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
                let mut txn = db.engine.begin_write()?;
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

    let mut staged = Vec::with_capacity(batch.len());
    for p in &batch {
        match &p.prepared {
            Ok((plan, params)) => {
                let sp = txn.savepoint();
                match execute_prepared(db, &mut txn, plan, params, &triggers) {
                    Ok(affected) => ring.stage_result(p.intent.idx, affected, 0, &[], next_txn),
                    Err(e) => {
                        txn.rollback_to(sp);
                        let (code, msg) = encode_error(&e);
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
    let mut own_result: Option<Result<ExecResult>> = None;
    if let Some((plan, params)) = own {
        let sp = txn.savepoint();
        let mut partial = false;
        match exec_own(db, &mut txn, plan, params, &triggers, &mut partial) {
            Ok(out) => own_result = Some(Ok(out)),
            Err(e) => {
                txn.rollback_to(sp);
                own_result = Some(Err(e));
            }
        }
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
        let col = |name: &str, nullable: bool| ColumnDef {
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
