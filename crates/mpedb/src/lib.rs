//! mpedb — embedded, multi-process, shared-memory database.
//!
//! This is the user-facing facade: it compiles SQL **once** into
//! content-hashed plans ([`Database::prepare`]), executes plans by hash with
//! no parsing on the hot path ([`Database::execute`]), and maintains the
//! shared plan registry *inside* the database so any attached process can
//! `execute(hash, params)` for a plan it never prepared (design/DESIGN.md §7.2).
//!
//! ```no_run
//! use mpedb::{params, Config, Database};
//!
//! let db = Database::open_with_config(Config::from_toml_str("...").unwrap()).unwrap();
//! let h = db.prepare("SELECT * FROM users WHERE id = $1").unwrap(); // parse once
//! let rows = db.execute(&h, &params![42]).unwrap();                 // no parsing
//! ```
//!
//! # Use parameters, not literals
//!
//! Every distinct SQL text compiles to a distinct plan. Interpolating values
//! into the SQL string (`format!("... WHERE id = {id}")`) creates one plan
//! **per query** and floods the shared registry — the classic misuse. Always
//! pass values through `$n` parameters and [`params!`].
//!
//! # Locking rules
//!
//! - Never call [`Database::prepare`], [`Database::query`] (for uncached
//!   statements), or [`Database::verify`] while a [`WriteSession`] from the
//!   same handle is open on the same thread: they may need the single writer
//!   lock, and the ERRORCHECK mutex turns the relock into an error instead of
//!   a deadlock.
//! - [`WriteSession::query`] compiles SQL in-process and caches the plan
//!   **only locally** — it never touches the registry write path, precisely
//!   to avoid that self-lock.

/// Test-only: a database file that deletes itself, even when a test panics.
///
/// The tests here put their `.mpedb` on `/dev/shm` and named it by PID, then
/// removed it on the last line. A panicking test never reaches that line — so
/// every red run leaked ~8 MB of tmpfs, and re-running (which is exactly what
/// you do when a test is red) leaked more. `/dev/shm` reached 100% twice in one
/// day, and the resulting `StorageFull` surfaces as ~50 unrelated test failures,
/// which reads as "the code is broken" rather than "the disk is full".
///
/// Cleanup therefore belongs in a Drop guard, which panics DO run.
#[cfg(test)]
mod testdb;

mod database_open;
mod database_query;
mod write_session;

mod access;
pub mod backup;
pub mod collab;
pub mod sync;
mod exec;
mod multifile;
/// Where mpedb puts files it must create but never means to keep — the temp
/// member behind `CREATE TEMPORARY TABLE`, and anything else with a lifetime
/// shorter than the database. `MPEDB_TEST_DIR`, then `/dev/shm`, then the
/// platform temp dir. Exported so a harness that drives mpedb can put ITS
/// scratch on the same volume: when the two disagree, one full tmpfs makes
/// only half a run flap, which is far harder to diagnose than all of it.
pub use multifile::ephemeral_dir;
pub mod sqlscript;
mod policy_store;
mod registry;
pub mod risk;
mod ring_exec;
mod session;
mod shard;
mod sqlite_attach;
mod ddl_apply;
mod sqlite_overlay;
pub mod advisor;
pub mod costlayer;
pub mod opdef;
pub mod spellfn;
pub mod lens;
pub mod rretl;
pub mod rretl_codec;
pub mod rretl_store;
pub mod ingest;
pub mod ingest_plan;
pub mod ingest_run;
pub mod ingest_task;
pub mod rretl_map;
pub mod rretl_map_run;
pub mod rretl_map_stream;
pub mod model;
pub mod stats;
mod stream;
mod tier;
mod backtest;
pub mod colseg;
mod derived;
mod fk;
mod trigger;
mod workspace;

pub use backtest::TriggerBacktest;
pub use derived::DerivedSync;
pub use access::{ddl_access, plan_access, Access, AccessReport, ObjectKind, TxnOp};

/// Process-wide count of statements whose ADAPTIVE PARALLEL aggregate fold
/// engaged — i.e. a scan proved long enough at run time that its remaining key
/// range was handed to worker threads (`exec/parallel.rs`). A diagnostic and
/// test observable, deliberately the ONLY one: the parallel fold is proven to
/// change nothing else observable (values, ties, raises, budget refusal
/// points), so the invariance batteries use this to assert that the path they
/// exercise really ran. Monotone; never reset.
pub fn parallel_folds_engaged() -> u64 {
    exec::parallel_folds_engaged()
}
pub use risk::{estimate_plan_risk, RiskEstimate};
pub use session::Session;
pub use shard::ShardSet;
pub use sqlite_attach::SqliteAttach;
pub use sqlite_overlay::{LockMode, ReconcilePolicy, ReconcileReport, SqliteOverlay};
#[cfg(feature = "sqlite-checkpoint")]
pub use sqlite_overlay::CheckpointReport;
pub use stream::RowStream;
pub use tier::TierReport;
pub use workspace::{Workspace, WorkspaceTxn, WsPlan};

pub use mpedb_types::model::WorkloadModel;
/// Escape a path for embedding in a TOML basic string — see
/// [`mpedb_types::toml_escape`]. Re-exported because every binding that builds
/// a config from a user-supplied path needs it, and #159 found three that had
/// each solved it differently.
pub use mpedb_types::toml_escape;
pub use mpedb_types::{
    BudgetKind, Collation, ColumnDef, ColumnType, Config, DbOptions, Durability, Error, FkAction,
    Footprint, ForeignKeyDef, FtsModule, IndexDef,
    HostAggState, KeyAccess, KeyBound, KeyPart, PlanHash, PolicyCmd, PolicyDef, Result, Schema,
    TableDef, TableKind, TableSet, Value, INDEX_EXPR_COL, MAX_DB_SIZE_MB,
};
/// The strict decimal parser: what turns a client's literal into the canonical
/// text a [`Value::Numeric`] stores, and refuses what is not a number.
pub use mpedb_types::value::{canonical_numeric, numeric_cmp, parse_numeric};
/// The schema plus the generation it was read at — one value that answers both
/// "what is the schema" and "is my copy stale", CONSISTENTLY.
pub use mpedb_core::engine::SchemaBundle;
/// What kind of storage a database file sits on. Re-exported because the
/// design's "local filesystem" assumption is a fact CALLERS need — the C-API
/// shim answers `PRAGMA mpedb_filesystem` from it.
pub use mpedb_core::{fs_kind, FsKind};

use exec::{exec_stmt, ChargeMode, ReadCtx};
pub use exec::take_last_insert_rowid;
pub use fk::FkCheckRow;
/// Re-exported for the C-API shim, which has to know whether a DDL statement
/// targets the TEMP schema before it can file the statement's text there.
pub use mpedb_sql::{rename_table_in_ddl, rewrite_temp_ddl, AttachStmt};
use mpedb_core::{CheckPrograms, Engine, SchemaPrograms, WriteTxn};
use mpedb_sql::{CompiledPlan, HostUdfSet, PlanStmt};
use registry::{decode_registry_plan, patched_last_used, plan_subkey};
use std::collections::HashMap;

use mpedb_types::keycode;
use std::path::Path;
use std::sync::{Arc, RwLock};

pub(crate) const POISON: &str = "plan cache lock poisoned";

/// A host-registered scalar UDF closure (the C-API `create_function` path,
/// design/DESIGN-UDF.md): it receives the already-evaluated argument `Value`s
/// and returns the result or an error. `Send + Sync` so a [`Database`] behind an
/// `Arc` stays shareable across threads.
pub type HostScalarFn = Arc<dyn Fn(&[Value]) -> Result<Value> + Send + Sync>;

/// A snapshot of the per-connection UDF registry taken for ONE execution and
/// handed to the executor as a [`mpedb_types::HostFns`] resolver. Snapshotting
/// into a flat vector once per UDF-bearing statement keeps the registry lock off
/// the per-row hot path; the arity set is tiny (Django registers ~30), so the
/// linear name/arity lookup per call is negligible.
pub(crate) struct HostFnTable {
    fns: Vec<(String, i32, HostScalarFn)>,
    /// Stored-function IR by content hash (stage M2) — the CURRENT catalog,
    /// resolved once per spell-bearing statement. A hash the table lacks
    /// (a redefinition raced between compile and execute) refuses cleanly;
    /// the gen-gated re-prepare fixes the next call.
    spells: Vec<([u8; 32], std::sync::Arc<mpedb_spell::ir::Proc>)>,
}

/// Run arbitrary CALLER code and turn a panic into an error instead of letting
/// it unwind through the engine (design/DESIGN-UDF.md §Safety).
///
/// A host UDF is caller code executing inside a statement — and, on the write
/// path, inside an open write transaction holding the single writer lock. An
/// unwind from there is survivable (`WriteTxn::drop` releases the writer lock,
/// COW means nothing committed is touched, and a ring leader that dies mid-round
/// is exactly the case `recover_orphans` handles) but it is not *clean*: the
/// statement's partial effects would be discarded by a stack unwind rather than
/// by the executor's own savepoint/poison contract, and a `WriteSession` living
/// on a C-API handle is not dropped by the shim's `catch_unwind` at all — it
/// would survive with a torn statement and no poison flag.
///
/// So the panic is caught HERE, at the one boundary where caller code is
/// invoked, and becomes an ordinary statement error. Every existing failure path
/// then applies unchanged: the ring leader rolls its per-intent savepoint back,
/// `WriteSession::run` poisons a session whose statement was partially applied,
/// and the writer lock is released by the normal commit/abort path.
fn guard_panic<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            Err(Error::Unsupported(format!("{what} panicked: {msg}")))
        }
    }
}

impl mpedb_types::HostFns for HostFnTable {
    fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let argc = args.len() as i32;
        // Exact arity wins; a variadic `(name, -1)` registration is the fallback
        // — sqlite's rule for `create_function(..., nArg = -1, ...)`.
        let f = self
            .fns
            .iter()
            .find(|(n, a, _)| n == name && *a == argc)
            .or_else(|| self.fns.iter().find(|(n, a, _)| n == name && *a == -1))
            .map(|(_, _, f)| f)
            .ok_or_else(|| {
                Error::Unsupported(format!("host function {name}/{argc} is not registered"))
            })?;
        guard_panic(&format!("host function {name}/{argc}"), || f(args))
    }

    fn call_spell(&self, hash: &[u8; 32], args: &[Value]) -> Result<Value> {
        let proc = self
            .spells
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                Error::Unsupported(
                    "stored function was redefined or dropped since this plan compiled —                      re-prepare the statement"
                        .into(),
                )
            })?;
        // No guard_panic: a spell is OUR interpreter over validated IR, not
        // caller code — a panic there is an engine bug and should say so.
        crate::spellfn::call_spell_fn(proc, args)
    }
}

/// A host-registered AGGREGATE's factory (the C-API `xStep`/`xFinal` path,
/// design/DESIGN-UDF.md stage 2): called ONCE PER GROUP to mint a fresh
/// accumulator. A factory rather than a closure because an aggregate has state
/// — sqlite's per-aggregation `sqlite3_aggregate_context` — and two groups must
/// never share it.
pub type HostAggFactory = Arc<dyn Fn() -> Box<dyn mpedb_types::HostAggState> + Send + Sync>;

/// The aggregate twin of [`HostFnTable`]: a per-execution snapshot of the
/// registry, handed to the executor as a [`mpedb_types::HostAggs`] resolver.
pub(crate) struct HostAggTable {
    aggs: Vec<(String, i32, HostAggFactory)>,
}

/// A host aggregate state with the same panic boundary the scalars get
/// ([`guard_panic`]): `xStep`/`xFinal` are caller code, and neither may unwind
/// into the engine.
struct GuardedAggState {
    name: String,
    inner: Box<dyn mpedb_types::HostAggState>,
}

impl mpedb_types::HostAggState for GuardedAggState {
    fn step(&mut self, args: &[Value]) -> Result<()> {
        let name = &self.name;
        let inner = &mut self.inner;
        guard_panic(&format!("host aggregate {name}() step"), || {
            inner.step(args)
        })
    }
    fn finish(self: Box<Self>) -> Result<Value> {
        let me = *self;
        let name = me.name;
        let inner = me.inner;
        guard_panic(&format!("host aggregate {name}() finish"), move || {
            inner.finish()
        })
    }
    // The WINDOW half of the protocol gets the same panic boundary: `xInverse`
    // and `xValue` are caller code on the same footing as `xStep`.
    fn inverse(&mut self, args: &[Value]) -> Result<()> {
        let name = &self.name;
        let inner = &mut self.inner;
        guard_panic(&format!("host aggregate {name}() inverse"), || {
            inner.inverse(args)
        })
    }
    fn value(&mut self) -> Result<Value> {
        let name = &self.name;
        let inner = &mut self.inner;
        guard_panic(&format!("host aggregate {name}() value"), || inner.value())
    }
}

/// One registered collating sequence: `(a, b) -> Ordering` over two strings.
type HostCollCmp = Arc<dyn Fn(&str, &str) -> std::cmp::Ordering + Send + Sync>;

/// The collating sequences ONE execution sees — snapshotted from the registry
/// under the same gate as [`HostFnTable`]/[`HostAggTable`], so a re-registration
/// mid-statement cannot change the order half-way through a sort.
pub(crate) struct HostCollTable {
    colls: Vec<(String, HostCollCmp)>,
}

impl mpedb_types::HostColls for HostCollTable {
    fn has(&self, name: &str) -> bool {
        self.colls.iter().any(|(n, _)| n == name)
    }
    fn compare(&self, name: &str, a: &str, b: &str) -> std::cmp::Ordering {
        match self.colls.iter().find(|(n, _)| n == name) {
            // A panicking comparator cannot be allowed to unwind through
            // `sort_by` (which would leave the slice in an unspecified state);
            // treat it as "peers", the same containment `guard_panic` gives the
            // UDF paths, and let the sort finish.
            Some((_, f)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(a, b)))
                .unwrap_or(std::cmp::Ordering::Equal),
            None => std::cmp::Ordering::Equal,
        }
    }
}

impl mpedb_types::HostAggs for HostAggTable {
    fn create(&self, name: &str, argc: i32) -> Result<Box<dyn mpedb_types::HostAggState>> {
        let f = self
            .aggs
            .iter()
            .find(|(n, a, _)| n == name && *a == argc)
            .or_else(|| self.aggs.iter().find(|(n, a, _)| n == name && *a == -1))
            .map(|(_, _, f)| f)
            .ok_or_else(|| {
                Error::Unsupported(format!("host aggregate {name}/{argc} is not registered"))
            })?;
        let inner = guard_panic(&format!("host aggregate {name}() factory"), || Ok(f()))?;
        Ok(Box::new(GuardedAggState {
            name: name.to_string(),
            inner,
        }))
    }
}

/// Warn threshold for the prepare-time risk estimate (#74 layer 1) when the
/// engine's `max_work_rows` is `0` (unlimited): a worst-case estimate above this
/// still logs a warning, because even without a runtime cap a billion-plus
/// work-row plan is almost always a mistake.
const RISK_WARN_CEILING: u64 = 1_000_000_000;

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    /// SELECT: output column names and rows in output order.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// INSERT/UPDATE/DELETE: number of rows written.
    Affected(u64),
    /// `EXPLAIN <stmt>`: human-readable plan rendering; nothing executed.
    Explain(String),
}

/// A prepared point-select handle (SQLite `sqlite3_stmt` analogue for reads).
///
/// Holds the compiled plan and, when eligible, precomputed PkPoint projection
/// metadata so each [`Database::execute_prepared_select`] skips plan-cache
/// lookup and column-name rebuild. Create with [`Database::prepare_select`].
///
/// Invalidated after schema DDL that bumps `schema_gen` — re-prepare.
#[derive(Clone)]
pub struct PreparedSelect {
    hash: PlanHash,
    plan: std::sync::Arc<CompiledPlan>,
    /// Precomputed micro-exec shape; `None` → general select path.
    pk_hot: Option<exec::PkPointHot>,
    /// Schema generation observed at prepare; execute re-checks.
    schema_gen: u64,
}

impl PreparedSelect {
    /// Content hash of the underlying plan (same as [`Database::prepare`]).
    pub fn hash(&self) -> &PlanHash {
        &self.hash
    }

    /// Whether this handle uses the PkPoint micro-executor (point lookup).
    pub fn is_pk_point_hot(&self) -> bool {
        self.pk_hot.is_some()
    }
}

/// Thread-local last plan-cache hit: repeated `execute(same_hash)` skips the
/// `RwLock` on the process plan map (sqlite keeps the VDBE on the stmt).
struct TlsPlanHit {
    /// Which `Database` handle put this here. A `PlanHash` identifies the plan
    /// TEXT, not the connection it is legal on, and the cache is thread-wide —
    /// so without this a second handle on the same thread reads the first
    /// handle's plan. See `Database::conn_id`.
    conn_id: u64,
    hash: PlanHash,
    plan: std::sync::Arc<CompiledPlan>,
    cache_gen: u64,
}

std::thread_local! {
    static TLS_PLAN: std::cell::RefCell<Option<TlsPlanHit>> = const { std::cell::RefCell::new(None) };
}

/// What a SQL text compiled to, plus everything that answer depended on (#166).
///
/// Every other cache here is keyed by `PlanHash`, which is the content hash of
/// the FINISHED plan — safe by construction (a different compile result is a
/// different key) but unable to save the work it was built to save, because you
/// must compile to learn the key. This one is keyed by the TEXT, so it gives up
/// that property and has to buy it back by naming the compile's inputs:
///
/// * **`gen`** — `schema_gen`, which already moves for every catalog input a
///   compile reads: DDL, views, RLS policies, stored functions, `:op:` macros,
///   the model, tunables and the cost policy. `stats.rs` documents that breadth
///   as the reason it does NOT gen-guard its own records; the same breadth is
///   exactly what a plan memo needs.
/// * **`tables`** — `(table_id, magnitude(row_count), policy_epoch)` for the
///   tables the plan touches:
///   * *magnitude* because row counts reach the planner through
///     `mpedb_sql::magnitude` and nowhere else, and a table has to DOUBLE
///     before that moves (design/DESIGN-MPEE-SOLVER.md §6). Without it the memo
///     would freeze a plan chosen when the table held ten rows.
///   * *policy epoch* because RLS edits do NOT move `schema_gen` — they bump a
///     per-table epoch instead (`policy_store::policy_epoch`). The executor's
///     `validate_policy_read` only re-checks policies a plan baked IN, which
///     was sufficient while every `query()` recompiled and so could not miss a
///     policy being ADDED. A memo that skips the compile can, and
///     `default_deny_when_enabled_without_permissive_policy` caught exactly
///     that: `SELECT` → `disable_rls` → the same `SELECT` served the
///     RLS-enabled plan and returned 0 rows instead of 3.
///
/// All agreeing means a recompile would have produced this same plan, so a hit
/// is not an approximation of the compile path — it is that path's answer.
struct TextMemo {
    gen: u64,
    tables: Vec<(u32, u32, u64)>,
    hash: PlanHash,
    /// CLOCK reference bit. Set on a hit (which holds only a READ lock — the
    /// whole point is that a hit takes no write lock), cleared by the eviction
    /// sweep. Plain FIFO would evict a hot statement the moment an application
    /// with more distinct statements than `MEMO_CAP` touched a cold one, which
    /// is the shape a Django app actually has.
    used: std::sync::atomic::AtomicBool,
}

/// Entries kept per handle. Applications that inline literals rather than bind
/// parameters mint a new SQL text per call, so this map must be bounded or it
/// is a leak; `rusqlite`'s `prepare_cached` is bounded for the same reason.
const MEMO_CAP: usize = 512;

/// Whether a [`TextMemo`] hit should be re-derived and compared against a cold
/// compile (#166). On in debug, so every `cargo test` verifies it; a release
/// build opts in with `MPEDB_VERIFY_PLAN_MEMO=1`, which is what makes running
/// the whole sqllogictest corpus against the check affordable — the corpus is
/// where inputs nobody enumerated actually live, and it is far too large to
/// walk in a debug build.
fn verify_plan_memo() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(debug_assertions) || std::env::var_os("MPEDB_VERIFY_PLAN_MEMO").is_some()
    })
}

/// Bounded SQL-text → [`TextMemo`] map with CLOCK eviction.
#[derive(Default)]
struct MemoMap {
    map: HashMap<std::sync::Arc<str>, TextMemo>,
    /// Eviction order; `hand` walks it looking for a clear reference bit.
    ring: Vec<std::sync::Arc<str>>,
    hand: usize,
}

impl MemoMap {
    fn get(&self, sql: &str) -> Option<&TextMemo> {
        let e = self.map.get(sql)?;
        e.used.store(true, std::sync::atomic::Ordering::Relaxed);
        Some(e)
    }

    fn insert(&mut self, sql: &str, entry: TextMemo) {
        if let Some(slot) = self.map.get_mut(sql) {
            *slot = entry;
            return;
        }
        while self.map.len() >= MEMO_CAP && !self.ring.is_empty() {
            self.hand %= self.ring.len();
            let key = self.ring[self.hand].clone();
            match self.map.get(&key) {
                // Referenced since the last sweep: spare it, clear the bit.
                Some(e) if e.used.swap(false, std::sync::atomic::Ordering::Relaxed) => {
                    self.hand += 1;
                }
                _ => {
                    self.map.remove(&key);
                    self.ring.remove(self.hand);
                }
            }
        }
        let key: std::sync::Arc<str> = std::sync::Arc::from(sql);
        self.ring.push(key.clone());
        self.map.insert(key, entry);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.ring.clear();
        self.hand = 0;
    }
}

/// Hands out the process-unique [`Database::conn_id`]. Wrapping is not a
/// concern: at one handle per nanosecond a `u64` lasts ~584 years.
fn next_conn_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A blob source over any `std::io::Read` with a known length: the bridge from
/// a file, socket, or stream to `WriteSession::insert_streaming`. The engine
/// pulls a page at a time, so the reader's bytes are never all resident.
///
/// The length is declared up front (the leaf cell records the value's size) and
/// must match what the reader yields: `next_into` uses `read_exact`, so a reader
/// that ends early surfaces as an error rather than a short row.
pub struct ReaderBlobSource<R> {
    reader: R,
    len: usize,
}

impl<R: std::io::Read> ReaderBlobSource<R> {
    pub fn new(reader: R, len: usize) -> Self {
        ReaderBlobSource { reader, len }
    }
}

impl<R: std::io::Read> mpedb_core::btree::BlobSource for ReaderBlobSource<R> {
    fn len(&self) -> usize {
        self.len
    }
    fn next_into(&mut self, buf: &mut [u8]) -> Result<()> {
        self.reader.read_exact(buf)?;
        Ok(())
    }
}

/// A FILE as a blob source: `next_into` reads at its own cursor (pread —
/// never the fd's seek position), and `as_file` hands the engine the file so
/// the extent import can take the kernel-side `copy_file_range` fast path
/// (#50). The two views agree by construction: both start at offset 0 over
/// the same bytes, and the engine uses exactly one of them per insert.
pub struct FileBlobSource {
    // wasm32 reads neither: `next_into` there is the unreachable arm (no
    // filesystem means no `File` to construct this from in the first place).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    file: std::fs::File,
    len: usize,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pos: u64,
}

impl FileBlobSource {
    pub fn new(file: std::fs::File, len: usize) -> Self {
        FileBlobSource { file, len, pos: 0 }
    }
}

impl mpedb_core::btree::BlobSource for FileBlobSource {
    fn len(&self) -> usize {
        self.len
    }
    fn next_into(&mut self, buf: &mut [u8]) -> Result<()> {
        // wasm32: unreachable — a `std::fs::File` cannot be opened there, so
        // no `FileBlobSource` can be constructed in the first place. The arm
        // exists only because `read_exact_at` is a unix extension trait.
        #[cfg(target_arch = "wasm32")]
        {
            let _ = buf;
            return Err(mpedb_types::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "file blob sources need a filesystem; the wasm32 build has none",
            )));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            #[cfg(unix)]
            use std::os::unix::fs::FileExt;
            #[cfg(windows)]
            use mpedb_core::wincompat::FileExt;
            self.file.read_exact_at(buf, self.pos)?;
            self.pos += buf.len() as u64;
            Ok(())
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn as_file(&self) -> Option<&std::fs::File> {
        Some(&self.file)
    }
}

/// A **detached (client-borne) plan**: the compiled plan the SDK/client
/// carries itself instead of leaving in the shared registry (Morten's idea,
/// design/DESIGN.md §7.2 turned inside-out). Shipping `(blob + hash + sql)` between
/// components lets any process execute the plan with *no registry write* — the
/// database only has to VALIDATE integrity ([`Database::execute_detached`]),
/// never to store anything.
///
/// - `hash` — the plan's content hash ([`CompiledPlan::hash`]); the integrity
///   anchor and the coordination/idempotence id, exactly as in the registry.
/// - `blob` — [`CompiledPlan::encode`] output; the executable, self-validating
///   plan bytes.
/// - `sql` — the original SQL, carried so the client can transparently
///   re-`prepare_detached` when [`Database::execute_detached`] returns
///   [`Error::PlanInvalidated`] (the plan predates a schema change). The text
///   is stored **uncompressed**: SQL statements are small and a compression
///   dependency is not worth it; this field is exactly where a future
///   compressor would slot in (compress on [`DetachedPlan::encode`], inflate
///   on [`DetachedPlan::decode`]).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedPlan {
    pub hash: PlanHash,
    pub blob: Vec<u8>,
    pub sql: String,
}

/// Wire-format tag for [`DetachedPlan::encode`] (independent of the plan blob's
/// own format byte).
const DETACHED_FORMAT: u8 = 1;

impl DetachedPlan {
    /// Self-describing, deterministic serialization for shipping a detached
    /// plan between components:
    ///
    /// ```text
    /// u8 format(1) ‖ hash(32) ‖ u32 blob_len ‖ blob ‖ u32 sql_len ‖ sql
    /// ```
    ///
    /// (`sql` is stored uncompressed — see the type-level note.)
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 32 + 4 + self.blob.len() + 4 + self.sql.len());
        buf.push(DETACHED_FORMAT);
        buf.extend_from_slice(&self.hash.0);
        buf.extend_from_slice(&(self.blob.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.blob);
        buf.extend_from_slice(&(self.sql.len() as u32).to_le_bytes());
        buf.extend_from_slice(self.sql.as_bytes());
        buf
    }

    /// Decode bytes produced by [`DetachedPlan::encode`]. Treats its input as
    /// hostile: every field is bounds-checked, `sql` must be valid UTF-8, and
    /// trailing bytes are rejected. Any malformed input is [`Error::Corrupt`]
    /// — never a panic. Note this validates only the *envelope*; the plan blob
    /// itself is re-validated against the schema by
    /// [`Database::execute_detached`].
    pub fn decode(bytes: &[u8]) -> Result<DetachedPlan> {
        fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
            let end = pos
                .checked_add(n)
                .filter(|&e| e <= b.len())
                .ok_or_else(|| Error::Corrupt("truncated detached plan".into()))?;
            let s = &b[*pos..end];
            *pos = end;
            Ok(s)
        }
        let mut pos = 0usize;
        let format = take(bytes, &mut pos, 1)?[0];
        if format != DETACHED_FORMAT {
            return Err(Error::Corrupt(format!(
                "unknown detached plan format {format}"
            )));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(take(bytes, &mut pos, 32)?);
        let blob_len = u32::from_le_bytes(take(bytes, &mut pos, 4)?.try_into().unwrap()) as usize;
        let blob = take(bytes, &mut pos, blob_len)?.to_vec();
        let sql_len = u32::from_le_bytes(take(bytes, &mut pos, 4)?.try_into().unwrap()) as usize;
        let sql = std::str::from_utf8(take(bytes, &mut pos, sql_len)?)
            .map_err(|_| Error::Corrupt("invalid utf-8 in detached plan sql".into()))?
            .to_owned();
        if pos != bytes.len() {
            return Err(Error::Corrupt("trailing bytes in detached plan".into()));
        }
        Ok(DetachedPlan {
            hash: PlanHash(hash),
            blob,
            sql,
        })
    }
}

/// An open database handle: an attached engine plus a per-process cache of
/// compiled plans. Cheap to share behind an `Arc`; all methods take `&self`
/// except through [`WriteSession`].
pub struct Database {
    engine: Engine,
    cache: RwLock<HashMap<PlanHash, Arc<CompiledPlan>>>,
    /// SQL text → the plan it compiles to (#166), so a repeated statement skips
    /// tokenizer, binder, planner, MPEE solver, encode and hash — plus the five
    /// sys-keyspace reads `compile_maybe_explain` does before any of that.
    ///
    /// `cache` above cannot serve this: its key is the hash OF the compiled
    /// plan, so reaching it already costs the compile. Measured (#166,
    /// `examples/insert_cost.rs`, one idle box, 20k single-row inserts):
    /// `query(sql)` 13.80 µs against `execute(hash)` 3.97 µs on the same
    /// insert — 71 % of the statement was re-deriving a plan it had already
    /// derived. With this memo `query(sql)` is 5.41 µs, and the three arms it
    /// does not touch (`execute`, and 10- and 100-row batches) are unmoved,
    /// which is the control that says the 2.55× is this change. See
    /// [`TextMemo`] for what a hit has to agree with before it may be used.
    text_memo: RwLock<MemoMap>,
    /// Decoded stored-function IR by content hash (stage M2). Hash-keyed and
    /// immutable by construction, so it never needs invalidation — a
    /// redefinition is a NEW hash, and the old entry just stops being asked
    /// for once the gen-gated re-prepare lands.
    spell_cache: RwLock<HashMap<[u8; 32], Arc<mpedb_spell::ir::Proc>>>,
    /// The schema generation this process's plan cache was built against. A DDL
    /// commit (here or in another process) bumps the meta `schema_gen`; on the
    /// next statement `gate_cache_on_schema` observes the mismatch and drops the
    /// whole cache, so a plan compiled against a table that has since been
    /// dropped, re-created (its id reused in place), or altered is never
    /// executed against the new catalog (#47 stage 4). Cache HITS bypass
    /// compilation, so this gate — not the compile-path refresh — is what makes
    /// id-reuse safe.
    cache_gen: std::sync::atomic::AtomicU64,
    /// Process-unique identity for THIS handle, so the thread-local one-entry
    /// plan cache ([`TLS_PLAN`]) cannot serve one connection's plan to another.
    /// `cache_gen` alone is not enough: it starts at 0 on every handle, so two
    /// `Database`s open on the same thread collide on `(hash, 0)` — which let a
    /// host-UDF plan (deliberately connection-LOCAL, never published to the
    /// shared registry) be executed by a second handle that had never compiled
    /// it, and made a "fresh handle, cold cache" load silently skip the
    /// registry. Both are containment breaks, not just stale reads.
    conn_id: u64,
    /// The database file path this handle attached (for `Workspace` dup-file
    /// detection and diagnostics). For `:memory:shared:X` this is the
    /// *resolved* tmpfs path; for private `:memory:` it stays `":memory:"`.
    path: std::path::PathBuf,
    /// How this handle was opened — drives multi-process attach hints.
    storage: mpedb_types::StorageKind,
    /// The compiled trigger set (DESIGN-TRIGGERS), gated on `schema_gen` exactly
    /// like the plan cache: `(gen, set)`, rebuilt only when a `CREATE`/`DROP
    /// TRIGGER` (here or in another process) moves the gen. `None` = not yet
    /// built. Consulted by the write executor to fire `AFTER INSERT` triggers.
    rules_cache: RwLock<Option<(u64, bool, Arc<trigger::WriteRules>)>>,
    /// `PRAGMA foreign_keys` for THIS connection (#194). OFF by default —
    /// sqlite's default, and the only one that leaves an existing database's
    /// behaviour unchanged. `[compat] foreign_keys = true` in the config flips
    /// the initial value; the pragma flips it at runtime.
    fk_enforce: std::sync::atomic::AtomicBool,
    /// Table ids this process declared `require_policy = true` for
    /// (DESIGN-MULTIDB §6.3). Resolved from names ONCE at open — so a typo or a
    /// renamed table fails immediately and loudly, rather than silently
    /// asserting nothing for the rest of the deployment's life.
    require_policy: std::collections::HashSet<u32>,
    /// GROUP BY column-strictness dialect ([`Dialect`], COMPAT.md), from
    /// `[compat] dialect` (default sqlite) — or `postgres` for a
    /// PostgreSQL-imported mirror. Passed into every `prepare` so a bare column
    /// is accepted (sqlite) or refused (postgres) per the data's origin.
    dialect: mpedb_types::Dialect,
    /// This process's sync role (#157). Read once at open; it changes nothing
    /// about how a statement executes, only what a collaborative verdict is
    /// allowed to CLAIM — a replica's local commit is not the authority's word.
    role: crate::sync::Role,
    /// Host-registered scalar UDFs (the C-API `create_function` path,
    /// design/DESIGN-UDF.md), keyed by `(name, n_arg)` (`-1` = variadic). The
    /// binder is handed the names + arities at compile; the executor is handed
    /// the closures at run. A plan that calls one is compile-and-executed locally
    /// and NEVER published to the shared content-hashed registry (its closures
    /// live only in THIS connection). Per-connection, mutable at any time.
    host_udfs: RwLock<HashMap<(String, i32), HostScalarFn>>,
    /// Host-registered AGGREGATE UDFs (the C-API `xStep`/`xFinal` path,
    /// design/DESIGN-UDF.md stage 2), keyed the same way. The value is a FACTORY:
    /// the executor mints one accumulator per group, so no state is shared
    /// between groups or between concurrent executions. Same one-connection-only
    /// plan rule as the scalars.
    host_aggs: RwLock<HashMap<(String, i32), HostAggFactory>>,
    /// The subset of `host_aggs` registered with the WINDOW protocol
    /// (`register_host_window_aggregate`): states that also implement
    /// `inverse`/`value`, and so may appear under an `OVER` clause. Names only —
    /// the factory lives in `host_aggs` either way, and this is what the parser
    /// consults to decide whether `f(x) OVER (…)` is even grammatical.
    host_window_aggs: RwLock<std::collections::HashSet<String>>,
    /// Host-registered COLLATING SEQUENCES (`sqlite3_create_collation`,
    /// design/DESIGN-UDF.md stage 3), by NAME — a collation has no arity.
    ///
    /// Scope, deliberately narrow: a host collation is a COMPARATOR, so it can
    /// order an `ORDER BY … COLLATE <name>` and nothing else. It never reaches
    /// a KEY ENCODING — not a B+tree key, not a GROUP BY / DISTINCT fold, not a
    /// column's declared `COLLATE` — because an index built under BINARY cannot
    /// answer a host-collated probe, and a callback cannot produce sort bytes.
    /// The type system enforces it: everything else takes a built-in
    /// [`Collation`](mpedb_types::Collation), which no registration can produce.
    /// Same one-connection-only plan rule as the scalars and aggregates.
    host_colls: RwLock<HashMap<String, HostCollCmp>>,
    /// Writer-contention busy timeout in milliseconds (#109); `-1` = block
    /// indefinitely (the native default — mpedb's historical behavior). When
    /// `>= 0`, every facade path that takes the single writer lock bounds its
    /// wait to this budget and fails with [`Error::Busy`] at the deadline —
    /// the C-API shim maps that to `SQLITE_BUSY` / "database is locked". `0`
    /// means one immediate attempt, sqlite's no-busy-handler default. Set via
    /// [`Database::set_busy_timeout`]; atomic so the shim's
    /// `sqlite3_busy_timeout` (any thread) needs no `&mut`.
    busy_timeout_ms: std::sync::atomic::AtomicI64,
    /// Connection-local `ATTACH DATABASE` list (#51, sqlite compat) — never
    /// persisted, never shared. See `multifile.rs`.
    attached: RwLock<multifile::AttachState>,
    /// `CREATE TEMP VIEW` — name → (SELECT source, verbatim CREATE text).
    ///
    /// A temp view does NOT live in the temp MEMBER, and that is the whole
    /// point. A view is only stored text, and sqlite's temp views are
    /// connection-private while still reading `main` freely — a temp view over
    /// a main table is the ordinary case (SQLAlchemy's reflection suite builds
    /// one). Routing `CREATE VIEW temp.v AS SELECT … FROM u` at the member made
    /// it a CROSS statement between two files, which the DDL path had no
    /// meaning for and dropped on the floor: no view, no error, and a later
    /// `DROP VIEW` failing to bind was the first anyone heard of it.
    ///
    /// Held here, the text merges into main's `ViewCatalog` at compile
    /// (shadowing a main view of the same name, as sqlite resolves it) and the
    /// body binds against main with nothing special happening.
    temp_views: RwLock<std::collections::BTreeMap<String, (String, String)>>,
    /// Compiled cross-file plans (#51): connection-local like host-UDF plans
    /// (their table ids only mean anything against THIS attach list), so they
    /// live here and never touch the shared registry. Cleared on
    /// ATTACH/DETACH; epoch- and schema-gen-checked at every execute.
    cross_cache: RwLock<HashMap<PlanHash, Arc<multifile::CrossPlan>>>,
    /// `true` once any cross plan has been cached; lets
    /// [`execute`](Self::execute) skip the cross-cache `RwLock` on the common
    /// single-file path (prepare+bind SELECT hot path).
    cross_cache_live: std::sync::atomic::AtomicBool,
}

/// Compile every column CHECK source in `schema` into the engine's per-table /
/// per-column program vector.
///
/// The ONE place a CHECK source becomes an executable program — used for the
/// config's seed schema at open, and installed on the engine so every later
/// bundle (a peer's `CREATE TABLE … CHECK (…)`, this process's own DDL, a
/// writer's in-transaction reload) goes through the identical path. A source
/// that will not compile is an error here, never a silently unenforced
/// constraint.
fn compile_schema_checks(schema: &mpedb_types::Schema) -> Result<SchemaPrograms> {
    let mut checks: CheckPrograms = Vec::with_capacity(schema.tables.len());
    let mut index_predicates = Vec::with_capacity(schema.tables.len());
    let mut index_exprs = Vec::with_capacity(schema.tables.len());
    for table in &schema.tables {
        let mut per_col = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            per_col.push(match &col.check {
                None => None,
                Some(src) => Some(mpedb_sql::compile_check(src, table).map_err(|e| {
                    Error::Schema(format!(
                        "CHECK on `{}.{}` failed to compile: {e}",
                        table.name, col.name
                    ))
                })?),
            });
        }
        checks.push(per_col);
        // Partial-index predicates ride the same pass: one compiler call, one
        // schema, so a bundle can never hold checks from one schema and index
        // predicates from another.
        let mut per_index = Vec::with_capacity(table.indexes.len());
        for ix in &table.indexes {
            per_index.push(match &ix.predicate {
                None => None,
                Some(src) => Some(mpedb_sql::compile_index_predicate(src, table).map_err(|e| {
                    Error::Schema(format!(
                        "partial index predicate on `{}` failed to compile: {e}",
                        table.name
                    ))
                })?),
            });
        }
        index_predicates.push(per_index);
        // Key-part expressions ride the same pass, for the same reason: one
        // compiler call, one schema, so a bundle can never hold a key built
        // from one and a predicate from another.
        let mut per_index_exprs = Vec::with_capacity(table.indexes.len());
        for ix in &table.indexes {
            let mut parts = Vec::with_capacity(ix.exprs.len());
            for src in &ix.exprs {
                parts.push(match src {
                    None => None,
                    Some(src) => Some(mpedb_sql::compile_value_expr(src, table).map_err(|e| {
                        Error::Schema(format!(
                            "index key expression on `{}` failed to compile: {e}",
                            table.name
                        ))
                    })?),
                });
            }
            per_index_exprs.push(parts);
        }
        index_exprs.push(per_index_exprs);
    }
    Ok(SchemaPrograms { checks, index_predicates, index_exprs })
}


/// An interactive multi-statement write transaction. Both DML and SELECT run
/// inside the transaction, so SELECTs see the session's own uncommitted
/// writes. Dropping the session without [`WriteSession::commit`] rolls back.
///
/// # Statement failures and poisoning
///
/// A statement that fails *without* side effects leaves the session usable:
/// constraint violations the engine detects before mutating anything (a
/// duplicate key on a single-row INSERT, CHECK / NOT NULL / type errors)
/// simply surface, and the session may continue and commit.
///
/// Statements are **not** internally atomic, however. A multi-row INSERT
/// that fails on its third row has already inserted the first two; an
/// UPDATE/DELETE failing mid-loop has already modified earlier rows. When a
/// statement fails after possibly applying part of its effects, the session
/// becomes **poisoned**: every further [`execute`](WriteSession::execute) /
/// [`query`](WriteSession::query) call and the final
/// [`commit`](WriteSession::commit) return
/// [`Error::Unsupported`]`("transaction poisoned by a partially-applied
/// statement; rollback and retry")`, and `commit` rolls the transaction back
/// instead of persisting the torn statement.
/// [`rollback`](WriteSession::rollback) — explicit or via drop — works
/// normally.
pub struct WriteSession<'db> {
    db: &'db Database,
    txn: WriteTxn<'db>,
    /// The session context snapshotted at [`Database::begin_as`] (SET LOCAL
    /// semantics — §2.5): fixed for the transaction's lifetime so a later
    /// mutation of the caller's `Session` cannot bleed in. Empty for `begin()`.
    session: Session,
    /// A statement failed after applying part of its effects: the transaction
    /// no longer corresponds to any sequence of complete statements and must
    /// not be committed. See the type-level docs.
    poisoned: bool,
    /// The SQL `SAVEPOINT` stack (innermost last). Each entry pairs the
    /// savepoint's name with the engine-level COW snapshot captured when it was
    /// opened. `RELEASE`/`ROLLBACK TO` resolve the INNERMOST matching name
    /// (sqlite's shadowing rule) and compare names case-insensitively. The
    /// whole stack is discarded on commit/rollback (both consume the session).
    savepoints: Vec<NamedSavepoint>,
    /// This transaction has applied DDL, so its schema view (`txn`'s captured
    /// bundle) has diverged from the committed catalog (#95). It is a one-way
    /// latch: it disqualifies the session from the SQL-text plan memo for the
    /// rest of its life, in both directions (#168).
    ///
    /// The memo is a map over the COMMITTED catalog — that is what its
    /// `schema_gen` stamp names, and every other participant reads it that way.
    /// A plan compiled against uncommitted DDL is not a plan for that catalog:
    /// remembering `SELECT c FROM t` after an uncommitted `ADD COLUMN c` and
    /// then rolling back would leave the memo answering for a column that never
    /// existed. Serving one is the mirror image — the plan would be built for a
    /// schema this statement is not running against.
    ///
    /// Mechanically the READ side of the latch is a fast-path duplicate:
    /// an in-session DDL reloads the txn bundle stamped one past the
    /// committed `schema_gen` (`reload_bundle_from_catalog`), so every memo
    /// entry — stamped with a committed gen — already fails `memoized`'s
    /// gen compare. The WRITE
    /// side (the `remember` gate) is the load-bearing half and must stay:
    /// after the first DDL every subsequent reload stamps the SAME
    /// `committed+1`, so the gen stops discriminating between two different
    /// in-session schemas — and an entry stamped `committed+1` that survives
    /// this txn's ROLLBACK becomes a wrong-answer hit the moment a later,
    /// unrelated DDL commit reaches that generation with a different catalog.
    ddl_applied: bool,
    /// Plan hashes whose memo table-facts this session has already
    /// revalidated (`Database::plan_tables` — a `begin_read` plus a per-table
    /// probe). Within one session those facts are FROZEN: they read the last
    /// COMMITTED state, and no commit can happen while this session holds the
    /// single writer lock — policy edits are locked out the same way
    /// (policy_store's check runs under it). So the probe is paid once per
    /// (session, plan) and the hash alone is proof after that. Never carried
    /// across sessions; cleared with the `ddl_applied` latch.
    validated: std::collections::HashSet<PlanHash>,
}

/// One entry on the [`WriteSession`] savepoint stack.
struct NamedSavepoint {
    name: String,
    snap: mpedb_core::TxnSavepointFull,
}

// ------------------------------------------------ system-record key helpers

/// Stored-PROCEDURE namespaces (name-keyed and hash-keyed blobs). Owned here so
/// the trigger catalog (`EXECUTE PROCEDURE` bodies pin a `proch/<hash>` blob)
/// and `mpedb-proc`'s engine agree on one spelling; `mpedb-proc` re-exports
/// them.
pub const NS_PROC: &str = "proc";
pub const NS_PROC_HASH: &str = "proch";

const SYS_RECORD_MAX_NS: usize = 64;
const SYS_RECORD_MAX_KEY: usize = 1024;
const SYS_RECORD_MAX_VALUE: usize = 1 << 20;

fn check_sys_ns(ns: &str) -> Result<()> {
    if ns.is_empty() || ns.len() > SYS_RECORD_MAX_NS || ns.as_bytes().contains(&0) {
        return Err(Error::Unsupported(format!(
            "system record namespace must be 1..={SYS_RECORD_MAX_NS} NUL-free bytes, got {ns:?}"
        )));
    }
    Ok(())
}

/// Read one index's NDV bucket for the planner's `CostSource` — a single
/// sys-keyspace get on the compile's own snapshot. Every failure mode
/// (no record, stale generation, corrupt bytes) is `None`: a stats record is
/// advisory, and a prepare must never fail because of one.
pub(crate) fn ndv_bucket_from(
    r: &mpedb_core::engine::ReadTxn<'_>,
    schema: &Schema,
    table_id: u32,
    index_no: u32,
) -> Option<u32> {
    // The record is pinned to the index's IDENTITY (names), not the schema
    // generation — see stats.rs's module doc for the bug the difference fixed.
    let t = schema.tables.iter().find(|t| t.id == table_id && !t.dead)?;
    let ix = t.indexes.get((index_no.checked_sub(1)?) as usize)?;
    let cols: Vec<&str> = ix
        .columns
        .iter()
        .filter_map(|&c| t.columns.get(c as usize).map(|col| col.name.as_str()))
        .collect();
    let fp = stats::index_fingerprint(&t.name, &cols);
    let subkey = sys_record_subkey(stats::NS, &stats::record_key(table_id, index_no)).ok()?;
    let bytes = r.sys_get(&subkey).ok().flatten()?;
    let ndv = stats::decode_record(&bytes, &fp)?;
    (ndv > 0).then(|| stats::bucket(ndv))
}

pub(crate) fn sys_record_subkey(ns: &str, key: &[u8]) -> Result<Vec<u8>> {
    check_sys_ns(ns)?;
    if key.is_empty() || key.len() > SYS_RECORD_MAX_KEY {
        return Err(Error::Unsupported(format!(
            "system record key must be 1..={SYS_RECORD_MAX_KEY} bytes, got {} bytes",
            key.len()
        )));
    }
    let mut k = Vec::with_capacity(ns.len() + 1 + key.len());
    k.extend_from_slice(ns.as_bytes());
    k.push(0);
    k.extend_from_slice(key);
    Ok(k)
}

fn poisoned_err() -> Error {
    Error::Unsupported(
        "transaction poisoned by a partially-applied statement; rollback and retry".into(),
    )
}

/// `BEGIN`/`COMMIT`/`ROLLBACK` inside a [`WriteSession`] — one message for
/// both roads that can reach the refusal (`run_from`'s compiled-plan arm and
/// the `parse_txn_control` fast dispatch).
fn session_txn_verb_err() -> Error {
    Error::Unsupported(
        "the session already is a transaction; \
         use WriteSession::commit()/rollback()"
            .into(),
    )
}


/// The "no such savepoint" error, matching sqlite's message text verbatim so a
/// differential comparison sees the same failure. Uses `Error::Bind`, the
/// crate's convention for "no such <named object>".
fn no_such_savepoint(name: &str) -> Error {
    Error::Bind(format!("no such savepoint: {name}"))
}

// ---------------------------------------------------------------- params!

/// Conversion into [`Value`] for the [`params!`] macro.
pub trait IntoValue {
    fn into_value(self) -> Value;
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}
impl IntoValue for i64 {
    fn into_value(self) -> Value {
        Value::Int(self)
    }
}
impl IntoValue for i32 {
    fn into_value(self) -> Value {
        Value::Int(i64::from(self))
    }
}
impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::Float(self)
    }
}
impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}
impl IntoValue for &str {
    fn into_value(self) -> Value {
        Value::Text(self.to_owned())
    }
}
impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::Text(self)
    }
}
impl IntoValue for Vec<u8> {
    fn into_value(self) -> Value {
        Value::Blob(self)
    }
}
impl IntoValue for &[u8] {
    fn into_value(self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            None => Value::Null,
            Some(v) => v.into_value(),
        }
    }
}

/// Build a `Vec<Value>` parameter list:
/// `params![1i64, "text", 3.5, Value::Null]`.
#[macro_export]
macro_rules! params {
    () => { ::std::vec::Vec::<$crate::Value>::new() };
    ($($v:expr),+ $(,)?) => {
        ::std::vec::Vec::<$crate::Value>::from([
            $($crate::IntoValue::into_value($v)),+
        ])
    };
}

/// A resumable position in a database's change stream (#140).
///
/// Generations alone are NOT a position: they are counters inside one
/// incarnation of the file, and `notify_reset` zeroes them on reboot, on
/// reformat, and on delete-and-recreate. A client that stores a bare
/// generation and presents it after any of those returns holding a number
/// larger than the counter it meets, and concludes "unchanged" until the
/// counter catches up — sleeping through its own changes the whole way.
///
/// So a position is `(epoch, generations)`, and [`Listener::resume`] treats an
/// epoch it does not recognise as "everything may have changed". That is a
/// false wakeup, which is the direction this protocol always errs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeCursor {
    pub epoch: u64,
    pub seen: Vec<u64>,
}

/// A parked-and-resumable listener over a set of tables (#141 S3).
///
/// Wraps [`Database::wait_for_change_keyed`] so a caller stops hand-managing
/// the `seen` vector — the mistake that makes is silent (a stale `seen`
/// re-reports forever, a too-fresh one skips) and it is the same mistake for
/// every caller, so it belongs here once.
pub struct Listener<'a> {
    db: &'a Database,
    /// Committed txn id as of the last `advance()` — see [`Listener::snapshot`].
    snap_txn: u64,
    tables: Vec<u32>,
    keys: Vec<Option<Vec<Value>>>,
    epoch: u64,
    seen: Vec<u64>,
}

impl<'a> Listener<'a> {
    /// Park until one of the watched tables changes, or `timeout` elapses.
    /// Returns the table ids that moved, and advances the cursor past them.
    pub fn wait(&mut self, timeout: std::time::Duration) -> Vec<u32> {
        let changed = self.db.wait_for_change_keyed(&self.tables, &self.seen, &self.keys, timeout);
        self.advance();
        changed
    }

    /// Re-read every watched generation into the cursor. Called after each
    /// wait so the next one does not re-report what was just reported, and
    /// exposed because a caller that acts on a change wants to bank it.
    pub fn advance(&mut self) {
        self.epoch = self.db.notify_epoch();
        self.snap_txn = self.db.snapshot_txn();
        for (i, &t) in self.tables.iter().enumerate() {
            if let Some((gen, _)) = self.db.change_generation(t) {
                self.seen[i] = gen;
            }
        }
    }

    /// The position, for storing somewhere and presenting later.
    pub fn cursor(&self) -> ChangeCursor {
        ChangeCursor { epoch: self.epoch, seen: self.seen.clone() }
    }

    /// Resume from a stored position.
    ///
    /// A cursor from a different epoch, or with a different table count, is
    /// not comparable with what this file holds now — so it is discarded and
    /// every table reads as unseen. The caller then looks and finds either
    /// something or nothing, which is the correct outcome in both cases and
    /// the only one that cannot silently skip a change.
    pub fn resume(&mut self, cursor: &ChangeCursor) {
        if cursor.epoch == self.epoch && cursor.seen.len() == self.seen.len() {
            self.seen.clone_from(&cursor.seen);
        } else {
            self.seen.iter_mut().for_each(|g| *g = 0);
        }
    }

    pub fn tables(&self) -> &[u32] {
        &self.tables
    }

    /// The txn id this listener is caught up to — the token to hand
    /// [`Database::begin_guarded_for`] (#142 G1).
    ///
    /// Captured at the same moment as the generations, so acting on a wakeup
    /// guards against exactly what the listener has NOT seen. Taking a fresh
    /// snapshot after waking would silently include the very commit you woke
    /// for, and guard against nothing.
    pub fn snapshot(&self) -> u64 {
        self.snap_txn
    }
}

/// Could a change advertised as `published` (the second half of
/// [`Database::change_generation`]) have touched the primary key `key`?
///
/// The listener-side half of #141 N2, exposed on its own so a caller can
/// filter a generation it already observed without parking. `published == 0`
/// means the writer could not name a region, and answers `true` for every key
/// — the fail-safe direction this whole feature is built around.
pub fn key_region_matches(published: u64, key: &[Value]) -> bool {
    mpedb_core::shm::Shm::fingerprint_matches(published, &keycode::encode_key(key))
}

// -------------------------------------------------------------------- tests

/// In-crate tests that need access to the private engine handle (raw registry
/// corruption, eviction accounting). The end-to-end suite lives in
/// `tests/facade.rs`.
#[cfg(test)]
mod tests;
