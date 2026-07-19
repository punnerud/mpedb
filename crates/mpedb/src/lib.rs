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

mod exec;
mod policy_store;
mod registry;
pub mod risk;
mod ring_exec;
mod session;
mod shard;
mod sqlite_attach;
mod ddl_apply;
mod sqlite_overlay;
mod stream;
mod trigger;
mod workspace;

pub use risk::{estimate_plan_risk, RiskEstimate};
pub use session::Session;
pub use shard::ShardSet;
pub use sqlite_attach::SqliteAttach;
pub use sqlite_overlay::{LockMode, ReconcilePolicy, ReconcileReport, SqliteOverlay};
#[cfg(feature = "sqlite-checkpoint")]
pub use sqlite_overlay::CheckpointReport;
pub use stream::RowStream;
pub use workspace::{Workspace, WorkspaceTxn, WsPlan};

pub use mpedb_types::{
    ColumnDef, ColumnType, Config, DbOptions, Durability, Error, HostAggState, PlanHash, PolicyCmd,
    PolicyDef, Result, Schema, TableDef, Value, MAX_DB_SIZE_MB,
};

use exec::{exec_stmt, ReadCtx};
pub use exec::take_last_insert_rowid;
use mpedb_core::{CheckPrograms, Engine, WriteTxn};
use mpedb_sql::{CompiledPlan, HostUdfSet, PlanStmt};
use registry::{decode_registry_plan, patched_last_used, plan_subkey};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

const POISON: &str = "plan cache lock poisoned";

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
    file: std::fs::File,
    len: usize,
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
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, self.pos)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
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
    /// The schema generation this process's plan cache was built against. A DDL
    /// commit (here or in another process) bumps the meta `schema_gen`; on the
    /// next statement `gate_cache_on_schema` observes the mismatch and drops the
    /// whole cache, so a plan compiled against a table that has since been
    /// dropped, re-created (its id reused in place), or altered is never
    /// executed against the new catalog (#47 stage 4). Cache HITS bypass
    /// compilation, so this gate — not the compile-path refresh — is what makes
    /// id-reuse safe.
    cache_gen: std::sync::atomic::AtomicU64,
    /// The database file path this handle attached (for `Workspace` dup-file
    /// detection and diagnostics).
    path: std::path::PathBuf,
    /// The compiled trigger set (DESIGN-TRIGGERS), gated on `schema_gen` exactly
    /// like the plan cache: `(gen, set)`, rebuilt only when a `CREATE`/`DROP
    /// TRIGGER` (here or in another process) moves the gen. `None` = not yet
    /// built. Consulted by the write executor to fire `AFTER INSERT` triggers.
    trigger_cache: RwLock<Option<(u64, Arc<trigger::TriggerSet>)>>,
    /// Table ids this process declared `require_policy = true` for
    /// (DESIGN-MULTIDB §6.3). Resolved from names ONCE at open — so a typo or a
    /// renamed table fails immediately and loudly, rather than silently
    /// asserting nothing for the rest of the deployment's life.
    require_policy: std::collections::HashSet<u32>,
    /// GROUP BY column-strictness dialect ([`BareGroupBy`], COMPAT.md), from
    /// `[compat] bare_group_by` (default sqlite) — or `postgres` for a
    /// PostgreSQL-imported mirror. Passed into every `prepare` so a bare column
    /// is accepted (sqlite) or refused (postgres) per the data's origin.
    bare_group_by: mpedb_types::BareGroupBy,
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
fn compile_schema_checks(schema: &mpedb_types::Schema) -> Result<CheckPrograms> {
    let mut checks: CheckPrograms = Vec::with_capacity(schema.tables.len());
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
    }
    Ok(checks)
}

impl Database {
    /// Open (or create) the database described by a TOML config file.
    pub fn open(config_path: &Path) -> Result<Database> {
        Database::open_with_config(Config::from_file(config_path)?)
    }

    /// Attach an existing database file config-free, reading its stored schema
    /// and geometry (the file is schema-authoritative). Used by tooling — the
    /// mirror daemon/CLI, `dump`, the `mpedb <file.mpedb>` CLI — that must open a
    /// file it did not create a TOML for. Durability = `async` still needs a
    /// config (no background flusher); mirror files do not use it.
    ///
    /// CHECK constraints ARE enforced here. They did not used to be — this
    /// constructor once documented "a file that carries CHECK constraints must be
    /// opened via a config", which held only while a CHECK could arrive from
    /// nowhere but a config. `CREATE TABLE … CHECK (…)` is live DDL now, so a
    /// constraint can be born INSIDE the file, and every config-free attach then
    /// stored it and silently never enforced it — the CLI's own `.mpedb` path
    /// included. The compiler works off the schema, which the file carries, so
    /// there was never a reason to withhold it.
    pub fn open_from_file(path: &Path) -> Result<Database> {
        let engine = Engine::open_from_file(path)?;
        // Installing the compiler also REBUILDS the bundle it just loaded, so
        // this covers both what the file already carries and any later schema
        // the engine loads from the catalog — including one a peer process
        // creates while we are attached.
        engine.set_check_compiler(std::sync::Arc::new(compile_schema_checks))?;
        Ok(Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            cache_gen: std::sync::atomic::AtomicU64::new(0),
            trigger_cache: RwLock::new(None),
            path: path.to_path_buf(),
            // No config, so no §6.3 assertions — consistent with this
            // constructor's contract (it also skips CHECK programs): a
            // config-free attach enforces what the FILE carries, and
            // `require_policy` is a config-declared deployment assertion.
            require_policy: std::collections::HashSet::new(),
            // A config-free attach (mirror daemon/CLI, dump) has no `[compat]`
            // section to read, so it takes the lenient sqlite default — the same
            // default any config without `[compat]` gets. A PostgreSQL mirror
            // instead opens via `open_with_config` with the flag already set.
            bare_group_by: mpedb_types::BareGroupBy::default(),
            host_udfs: RwLock::new(HashMap::new()),
            host_aggs: RwLock::new(HashMap::new()),
        })
    }

    /// Open (or create) the database described by an already-parsed config.
    /// Compiles every column CHECK expression against its table and hands the
    /// programs to the engine, so constraint enforcement is identical in
    /// every attached process — then installs the compiler itself, so the SAME
    /// happens for every later schema the engine loads from the catalog.
    pub fn open_with_config(config: Config) -> Result<Database> {
        let checks = compile_schema_checks(&config.schema)?;
        let path = config.options.path.clone();
        let bare_group_by = config.options.bare_group_by;
        // Resolve the §6.3 assertions against the schema now: an unknown name is
        // a config error, not a no-op assertion nobody notices.
        let mut require_policy = std::collections::HashSet::new();
        for name in &config.options.require_policy {
            let id = config
                .schema
                .tables
                .iter()
                .position(|t| &t.name == name)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "require_policy names table `{name}`, which is not in the schema"
                    ))
                })?;
            require_policy.insert(id as u32);
        }
        let mut engine = Engine::open(&config, checks)?;
        // DESIGN-BLOBEXTENT §8: per-process knob, like durability. The format
        // self-describes, so this only decides what NEW writes do.
        engine.set_extent_threshold(config.options.extent_threshold);
        // From here on the ENGINE recompiles CHECK programs itself whenever it
        // rebuilds a bundle from the catalog. The config-derived vector above
        // only covers the SEED schema; a table created by `CREATE TABLE …
        // CHECK (…)` — in this process or another — is picked up through this,
        // instead of landing in the catalog as a constraint that is stored and
        // never enforced.
        engine.set_check_compiler(std::sync::Arc::new(compile_schema_checks))?;
        Ok(Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            cache_gen: std::sync::atomic::AtomicU64::new(0),
            trigger_cache: RwLock::new(None),
            path,
            require_policy,
            bare_group_by,
            host_udfs: RwLock::new(HashMap::new()),
            host_aggs: RwLock::new(HashMap::new()),
        })
    }

    /// The CURRENT schema (Arc'd bundle; derefs to [`Schema`]). DDL may swap
    /// it between calls — bind the Arc for a stable view.
    pub fn schema(&self) -> std::sync::Arc<mpedb_core::engine::SchemaBundle> {
        self.engine.schema()
    }

    /// The database file path this handle attached.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Register a HOST scalar UDF on this connection (the C-API
    /// `sqlite3_create_function` path, design/DESIGN-UDF.md). A SQL call
    /// `name(args)` that matches no built-in function then invokes `f` with the
    /// evaluated arguments. `n_arg` is the argument count, or `-1` for a variadic
    /// function that accepts any arity. Re-registering the same `(name, n_arg)`
    /// REPLACES the previous closure.
    ///
    /// A plan that calls a host UDF is valid only for the connection that
    /// registered it, so it is never published to the shared content-hashed plan
    /// registry — it is compiled and executed locally each time. Registering (or
    /// unregistering) therefore drops this connection's local plan cache: a
    /// cached plan may have resolved the name to the previous meaning, or errored
    /// on it as unknown.
    pub fn register_host_function<F>(&self, name: &str, n_arg: i32, f: F)
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        self.host_udfs
            .write()
            .expect(POISON)
            .insert((name.to_string(), n_arg), Arc::new(f));
        self.cache.write().expect(POISON).clear();
    }

    /// Remove a host UDF registered with [`register_host_function`]. Returns
    /// whether an entry was present. Drops the local plan cache (see that method).
    pub fn unregister_host_function(&self, name: &str, n_arg: i32) -> bool {
        let removed = self
            .host_udfs
            .write()
            .expect(POISON)
            .remove(&(name.to_string(), n_arg))
            .is_some();
        if removed {
            self.cache.write().expect(POISON).clear();
        }
        removed
    }

    /// Register a HOST AGGREGATE on this connection (the C-API `xStep`/`xFinal`
    /// path, design/DESIGN-UDF.md stage 2). A SQL call `name(arg)` then routes
    /// the whole SELECT through the aggregate planner and, per group, mints a
    /// fresh accumulator from `factory`, steps it once per surviving row (after
    /// `WHERE`, the policy predicate and any `FILTER`), and finishes it at the
    /// group's end. An EMPTY group still finishes a fresh accumulator, which is
    /// sqlite's rule (`xFinal` on a never-stepped context ⇒ typically NULL).
    ///
    /// The call shape is exactly ONE argument (`n_arg` 1, or `-1` for variadic);
    /// any other registered arity compiles to a clear error at the call site.
    /// Same plan-sharing rule as [`register_host_function`]: a plan naming a host
    /// aggregate never enters the shared registry, and registering drops this
    /// connection's local plan cache.
    pub fn register_host_aggregate<F>(&self, name: &str, n_arg: i32, factory: F)
    where
        F: Fn() -> Box<dyn mpedb_types::HostAggState> + Send + Sync + 'static,
    {
        self.host_aggs
            .write()
            .expect(POISON)
            .insert((name.to_string(), n_arg), Arc::new(factory));
        self.cache.write().expect(POISON).clear();
    }

    /// Remove a host aggregate registered with [`register_host_aggregate`].
    /// Returns whether an entry was present. Drops the local plan cache.
    pub fn unregister_host_aggregate(&self, name: &str, n_arg: i32) -> bool {
        let removed = self
            .host_aggs
            .write()
            .expect(POISON)
            .remove(&(name.to_string(), n_arg))
            .is_some();
        if removed {
            self.cache.write().expect(POISON).clear();
        }
        removed
    }

    /// The names + arities of the registered host UDFs, for the binder to resolve
    /// calls against (values never leave the registry — only names/arities reach
    /// compile). Empty when none are registered, so compilation is unchanged.
    /// Aggregates ride along: the PARSER needs them (see `HostUdfSet`).
    fn host_udf_set(&self) -> HostUdfSet {
        let f = self.host_udfs.read().expect(POISON);
        let a = self.host_aggs.read().expect(POISON);
        HostUdfSet::with_aggs(f.keys().cloned().collect(), a.keys().cloned().collect())
    }

    /// Snapshot the registry's closures for one execution (see [`HostFnTable`]).
    fn host_fn_table(&self) -> HostFnTable {
        let g = self.host_udfs.read().expect(POISON);
        HostFnTable {
            fns: g.iter().map(|((n, a), f)| (n.clone(), *a, f.clone())).collect(),
        }
    }

    /// Snapshot the aggregate factories for one execution (see [`HostAggTable`]).
    fn host_agg_table(&self) -> HostAggTable {
        let g = self.host_aggs.read().expect(POISON);
        HostAggTable {
            aggs: g.iter().map(|((n, a), f)| (n.clone(), *a, f.clone())).collect(),
        }
    }

    /// The closures ONE statement needs, or `None` when its plan calls no host
    /// UDF — the single gate every execution path (read, write, session, ring
    /// leader) goes through, so "which executions carry host closures" has one
    /// answer in one place.
    ///
    /// Snapshotting only for a `contains_host_call()` plan keeps the registry
    /// locks entirely off the hot path of every ordinary statement: a database
    /// with no UDFs registered, or a statement that calls none, does exactly what
    /// it did before.
    pub(crate) fn host_tables(&self, plan: &CompiledPlan) -> Option<(HostFnTable, HostAggTable)> {
        plan.contains_host_call()
            .then(|| (self.host_fn_table(), self.host_agg_table()))
    }

    /// Compile `sql` with this database's RLS policies injected (loaded from the
    /// catalog sys-keyspace on a pinned read snapshot, design/DESIGN-MULTIDB.md §3).
    /// The bool is the `EXPLAIN` flag. An empty policy set behaves exactly as
    /// plain compilation.
    /// Refresh this process's schema bundle if another process bumped the meta
    /// `schema_gen`, and — critically — drop the local plan cache when the gen
    /// actually moved. A cached plan is bound to a specific catalog layout; a
    /// DROP / re-CREATE (id reused in place) / ALTER since it was compiled makes
    /// it stale, and cache HITS never recompile. One `newest_meta` read in the
    /// common (unchanged) case; the cache is cleared only on a real gen change.
    fn gate_cache_on_schema(&self) -> Result<()> {
        self.engine.refresh_schema_if_stale()?;
        let gen = self.engine.schema().schema_gen;
        // Relaxed is fine: DDL is serialized under the writer lock, and a
        // same-instant race resolves at the txn's own captured snapshot. This
        // gate closes the common case — a DDL that committed in a PRIOR
        // statement — which is the one that would otherwise execute a stale plan.
        if self.cache_gen.swap(gen, std::sync::atomic::Ordering::Relaxed) != gen {
            self.cache.write().expect(POISON).clear();
        }
        Ok(())
    }

    fn compile_maybe_explain(&self, sql: &str) -> Result<(CompiledPlan, bool)> {
        // #47 stage 3/4: pick up another process's CREATE/DROP/ALTER before we
        // compile — a query against a just-created table must see it, one
        // against a just-dropped table must fail to bind, and a stale cached
        // plan must be evicted (the txn-begin reload happens too late, and a
        // cache hit skips compilation entirely).
        self.gate_cache_on_schema()?;
        let catalog = self.load_policy_catalog()?;
        let views = self.load_view_catalog()?;
        mpedb_sql::prepare_maybe_explain_with_views(
            sql,
            &self.schema(),
            &catalog,
            &views,
            self.bare_group_by,
            &self.host_udf_set(),
        )
    }

    /// Per output column, the `decltype` to report through the C-API shim
    /// (`sqlite3_column_decltype`) / Python `cursor.description[*][1]`. Compiles
    /// `sql` — surfacing any bind/plan error exactly as prepare would — and
    /// derives each column's declared type from the plan's projection (see
    /// [`CompiledPlan::output_decltypes`]): a bare base-table column reports its
    /// type, everything computed reports `None`. A non-SELECT yields an empty
    /// vec (all NULL). This does not execute or publish a plan.
    pub fn output_decltypes(&self, sql: &str) -> Result<Vec<Option<String>>> {
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(plan.output_decltypes(&self.schema()))
    }

    /// Compile `sql` against an EXPLICIT schema bundle — a [`WriteSession`]'s
    /// own view, which may already reflect DDL applied earlier in the SAME
    /// uncommitted transaction (#95). Unlike [`compile_maybe_explain`], there is
    /// no schema-gen gate: the session holds the single writer lock, so no peer
    /// DDL can move the committed schema underneath it, and its own uncommitted
    /// DDL is exactly what `schema` carries. Policies and views still come from
    /// the COMMITTED catalog (a policy/view created in the same uncommitted
    /// session is not yet visible — the pre-#95 contract, unchanged).
    fn compile_maybe_explain_with_schema(
        &self,
        sql: &str,
        schema: &mpedb_core::engine::SchemaBundle,
    ) -> Result<(CompiledPlan, bool)> {
        let catalog = self.load_policy_catalog()?;
        let views = self.load_view_catalog()?;
        mpedb_sql::prepare_maybe_explain_with_views(
            sql,
            schema,
            &catalog,
            &views,
            self.bare_group_by,
            &self.host_udf_set(),
        )
    }

    /// Prepare-time worst-case **risk estimate** (#74 layer 1) for an
    /// already-compiled plan, using the catalog's transactionally-exact row
    /// counts. Read-only: opens one read snapshot, executes nothing, and never
    /// touches plan bytes. See [`risk::estimate_plan_risk`].
    pub fn estimate_risk_for_plan(&self, plan: &CompiledPlan) -> Result<RiskEstimate> {
        let bundle = self.schema();
        let r = self.engine.begin_read()?;
        let est = risk::estimate_plan_risk(plan, &bundle.schema, &|tid| {
            r.row_count(tid).unwrap_or(0)
        });
        r.finish()?;
        Ok(est)
    }

    /// Compile `sql` and return its prepare-time risk estimate (#74) — the MPEE
    /// "answer at the start". Does not publish the plan or execute it.
    pub fn estimate_risk_sql(&self, sql: &str) -> Result<RiskEstimate> {
        let plan = self.compile_maybe_explain(sql)?.0;
        self.estimate_risk_for_plan(&plan)
    }

    /// #74 layer-1 surface: for a plan that structurally multiplies, log a
    /// warning when its worst-case estimate exceeds the warn threshold
    /// (`max_work_rows` when finite, else [`RISK_WARN_CEILING`]), naming the
    /// dominant node. Best-effort and advisory — a failure to open the read
    /// snapshot is swallowed, and this never refuses (the hard-refuse hook is
    /// [`RiskEstimate::exceeds`], left opt-in). Cheap: single-table plans skip
    /// the estimate (and its row-count reads) entirely.
    fn warn_if_risky(&self, plan: &CompiledPlan) {
        if !RiskEstimate::plan_can_multiply(plan) {
            return;
        }
        let budget = self.engine.work_budget();
        let threshold = if budget != 0 { budget } else { RISK_WARN_CEILING };
        if let Ok(est) = self.estimate_risk_for_plan(plan) {
            if est.work_rows > threshold {
                eprintln!(
                    "mpedb: runtime-budget risk — worst-case ~{} work-rows \
                     (dominant: {}) exceeds {}; this query is likely to hit \
                     [runtime] max_work_rows",
                    est.work_rows, est.dominant, threshold
                );
            }
        }
    }

    /// Compile `sql` to a content-hashed plan and publish it in the shared
    /// registry. Idempotent: statements differing only in whitespace, keyword
    /// case, or `?`/`$n` spelling produce the same hash.
    ///
    /// Read-first (reviewed invariant): local cache, then a registry probe in
    /// a *read* transaction; only a genuine miss opens a short write
    /// transaction to insert the entry — a read-mostly workload never touches
    /// the writer lock here.
    ///
    /// Must NOT be called while a [`WriteSession`] from this handle is open
    /// on the same thread (see the crate-level locking rules).
    pub fn prepare(&self, sql: &str) -> Result<PlanHash> {
        let plan = self.compile_maybe_explain(sql)?.0;
        self.warn_if_risky(&plan);
        let hash = plan.hash();
        self.register(hash, plan, sql)?;
        Ok(hash)
    }

    /// Execute a previously prepared plan by hash. The plan is taken from the
    /// local cache, or loaded (and fully re-validated) from the shared
    /// registry — so a hash prepared by *any* process works here.
    ///
    /// Errors: [`Error::UnknownPlan`] if the hash is in neither place (or the
    /// registry entry is corrupt) — re-`prepare` from SQL;
    /// [`Error::PlanInvalidated`] if the plan predates a schema change.
    pub fn execute(&self, hash: &PlanHash, params: &[Value]) -> Result<ExecResult> {
        self.execute_ctx(&Session::empty(), hash, params)
    }

    /// Like [`execute`](Self::execute) but with a [`Session`] whose values fill
    /// the plan's `current_setting()` references (design/DESIGN-MULTIDB.md §2). `params`
    /// are the caller-facing parameters only; the reserved context slots are
    /// filled from `session` (fail-closed on a missing key / NULL / wrong type).
    pub fn execute_ctx(
        &self,
        session: &Session,
        hash: &PlanHash,
        params: &[Value],
    ) -> Result<ExecResult> {
        // Evict a plan whose catalog changed underneath it before serving it
        // from cache (a DROP / re-CREATE / ALTER since prepare). Cheap in the
        // common case: one `newest_meta` read, no clear.
        self.gate_cache_on_schema()?;
        let plan = self.cached_or_load(hash)?;
        let full = session::resolve_params_timed(&plan, params, session)?;
        self.run_plan(Some(hash), &plan, &full)
    }

    /// One-shot prepare + execute. `EXPLAIN <stmt>` returns
    /// [`ExecResult::Explain`] without executing (and without publishing the
    /// plan); any other statement is published to the registry exactly like
    /// [`Database::prepare`].
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        self.query_ctx(&Session::empty(), sql, params)
    }

    /// Like [`query`](Self::query) but with a [`Session`] for `current_setting()`
    /// (design/DESIGN-MULTIDB.md §2). `params` are the caller-facing parameters only.
    pub fn query_ctx(
        &self,
        session: &Session,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        // RLS DDL (CREATE/DROP POLICY, ALTER TABLE … ROW LEVEL SECURITY) mutates
        // the catalog rather than compiling to a plan — apply it directly.
        if let Some(ddl) = mpedb_sql::parse_ddl(sql)? {
            return self.apply_ddl(ddl);
        }
        let (plan, is_explain) = self.compile_maybe_explain(sql)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(&self.schema())));
        }
        self.warn_if_risky(&plan);
        let hash = plan.hash();
        let plan = self.register(hash, plan, sql)?;
        let full = session::resolve_params_timed(&plan, params, session)?;
        self.run_plan(Some(&hash), &plan, &full)
    }

    // ---------------- detached (client-borne) plans ----------------

    /// Compile `sql` to a content-hashed plan **without publishing it to the
    /// shared registry**: the caller keeps the returned [`DetachedPlan`] and
    /// ships `(blob + hash + sql)` itself (Morten's detached-plan model). The
    /// returned `hash` is byte-identical to what [`Database::prepare`] would
    /// produce for the same SQL and schema — the two paths differ only in
    /// whether the plan is stored in the database.
    ///
    /// Unlike [`Database::prepare`], this never opens a write transaction and
    /// never takes the writer lock, so it is safe to call under read-mostly
    /// load and (unlike `prepare`) does not need the "no open WriteSession"
    /// caveat.
    pub fn prepare_detached(&self, sql: &str) -> Result<DetachedPlan> {
        let plan = self.compile_maybe_explain(sql)?.0;
        Ok(DetachedPlan {
            hash: plan.hash(),
            blob: plan.encode(),
            sql: sql.to_owned(),
        })
    }

    /// Execute a client-borne [`DetachedPlan`] **without ever touching the
    /// shared registry**. The blob is fully re-validated on the way in, in the
    /// same order the registry load path uses:
    ///
    /// 1. [`CompiledPlan::decode`] bounds-checks every field, range-checks all
    ///    indices against the live schema, and recomputes the footprint from
    ///    scratch (a forged footprint is rejected).
    /// 2. Its embedded `schema_hash` must equal the live schema — on mismatch
    ///    decode yields [`Error::PlanInvalidated`], which propagates so the
    ///    caller re-prepares from [`DetachedPlan::sql`]. (A byte flipped inside
    ///    the blob's `schema_hash` region is indistinguishable from a genuine
    ///    schema change and surfaces the same way; re-preparing from `sql`
    ///    heals both.)
    /// 3. **Integrity**: the decoded plan must hash to the carried `hash`.
    ///    `decode` does *not* check this (it validates against the schema, not
    ///    against a caller-supplied hash), so it is verified separately here. A
    ///    blob that decodes cleanly but does not match its advertised hash —
    ///    tampering that survived structural + schema validation, or a
    ///    mismatched (blob, hash) pair — is [`Error::Corrupt`]. It is
    ///    deliberately **not** [`Error::PlanInvalidated`] (that means "schema
    ///    changed, re-prepare") and **not** [`Error::UnknownPlan`] (that is the
    ///    registry's "missing", meaningless here — nothing is stored).
    ///
    /// Execution then reuses the exact same routing as [`Database::execute`]:
    /// `read_only` plans run on a lock-free read snapshot, DML autocommits
    /// through the writer lock. It is passed with no hash, so a detached DML
    /// statement leads its own commit directly rather than being enqueued on
    /// the intent ring (the ring leader loads intents *by hash from the
    /// registry*, and a detached plan is by definition not there).
    pub fn execute_detached(&self, plan: &DetachedPlan, params: &[Value]) -> Result<ExecResult> {
        // (1)+(2): structural + schema re-validation. PlanInvalidated (schema
        // drift) propagates verbatim; any other decode failure is Corrupt.
        let compiled = match CompiledPlan::decode(&plan.blob, &self.schema()) {
            Ok(p) => p,
            Err(Error::PlanInvalidated) => return Err(Error::PlanInvalidated),
            Err(e) => return Err(e),
        };
        // (3): integrity — the blob must match the hash the client carried.
        if compiled.hash() != plan.hash {
            return Err(Error::Corrupt(
                "detached plan blob does not match its carried hash".into(),
            ));
        }
        // Reuse the existing executor entry point (lib.rs-local; exec.rs is
        // untouched). `None` hash keeps DML off the intent ring. A detached
        // plan referencing `current_setting()` fails closed here (empty
        // session ⇒ missing-key error); a context-aware detached path is a
        // later addition.
        let full = session::resolve_params(&compiled, params, &Session::empty())?;
        self.run_plan(None, &compiled, &full)
    }

    /// Start an interactive multi-statement write transaction. Holds the
    /// single writer lock until commit/rollback/drop (drop = rollback).
    /// A second `begin()` from the same thread errors instead of hanging
    /// (ERRORCHECK mutex).
    pub fn begin(&self) -> Result<WriteSession<'_>> {
        self.begin_as(&Session::empty())
    }

    /// Begin a write transaction bound to a snapshot of `session` (SET LOCAL
    /// semantics, design/DESIGN-MULTIDB.md §2.5): every statement in the transaction
    /// resolves `current_setting()` against the context as it was *here*, so a
    /// later mutation of the caller's `Session` cannot bleed into an open
    /// transaction. The context is fixed for the transaction's lifetime.
    pub fn begin_as(&self, session: &Session) -> Result<WriteSession<'_>> {
        Ok(WriteSession {
            db: self,
            txn: self.engine.begin_write()?,
            session: session.clone(),
            poisoned: false,
            savepoints: Vec::new(),
        })
    }

    /// Verify the engine's page-accounting invariant (design/DESIGN.md §4.5).
    /// Takes the writer lock briefly; do not call with an open session.
    pub fn verify(&self) -> Result<()> {
        self.engine.verify_page_accounting()
    }

    /// Diagnostic counters for the high-water leak
    /// (`crates/mpedb-core/tests/high_water_leak.rs`):
    /// `(txn_id, high_water, oldest_pinned_bound, freelist_entries)`.
    ///
    /// For `examples/leak_probe.rs`. Costs a freelist walk, takes no writer
    /// lock, and pins nothing — perturbing the reader table is exactly what
    /// would corrupt the thing being measured.
    pub fn leak_counters(&self) -> Result<(u64, u64, u64, u64)> {
        self.engine.leak_counters()
    }

    /// Diagnostic (#37): see `Engine::freelist_shape`.
    pub fn freelist_shape(&self) -> Result<mpedb_core::engine::FreelistShape> {
        self.engine.freelist_shape()
    }

    // ---------------- system records: tooling/extension keyspace ----------

    /// Store a raw record in the reserved system keyspace, namespaced by
    /// `ns` (full key = `ns` bytes ++ `0x00` ++ `key`; `ns` must be
    /// non-empty, at most 64 bytes and NUL-free, which makes distinct
    /// namespaces prefix-disjoint from each other and from the plan
    /// registry's `plan/` prefix).
    ///
    /// This is a **tooling/extension API** (used by `mpedb-proc` for stored
    /// procedures, namespaces `proc`/`proch`): records are opaque bytes,
    /// shared across all attached processes, and live in the same catalog
    /// sys-keyspace as the plan registry. Like [`Database::prepare`], the
    /// write happens in its own short write transaction — do NOT call while
    /// a [`WriteSession`] from this handle is open on the same thread.
    pub fn sys_record_put(&self, ns: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let subkey = sys_record_subkey(ns, key)?;
        if value.len() > SYS_RECORD_MAX_VALUE {
            return Err(Error::Unsupported(format!(
                "system record value too large ({} > {SYS_RECORD_MAX_VALUE} bytes)",
                value.len()
            )));
        }
        let mut w = self.engine.begin_write()?;
        match w.sys_put(&subkey, value) {
            Ok(()) => w.commit(),
            Err(e) => {
                w.abort();
                Err(e)
            }
        }
    }

    /// Stream one `text`/`blob` column of one row into `out` in bounded
    /// chunks (≤ 256 KiB each) WITHOUT materializing the value — the chunked
    /// read API of DESIGN-BLOBEXTENT §5. `range` is `(offset, len)` within
    /// the value, `None` for all of it. Returns the byte count written, or
    /// `None` when the row is absent or the column is NULL. A snapshot
    /// eviction between chunks surfaces as an error — never as mixed bytes;
    /// the caller decides whether to retry against a fresh snapshot.
    pub fn blob_to_writer(
        &self,
        table: &str,
        pk_values: &[Value],
        col: &str,
        range: Option<(u64, u64)>,
        out: &mut dyn std::io::Write,
    ) -> Result<Option<u64>> {
        let table_id = self
            .schema()
            .table_id(table)
            .ok_or_else(|| Error::Unsupported(format!("unknown table `{table}`")))?;
        let bundle = self.schema();
        let t = bundle.table(table_id).expect("id from table_id");
        let col_idx = t
            .column_index(col)
            .ok_or_else(|| Error::Unsupported(format!("unknown column `{col}` in `{table}`")))?
            as usize;
        let r = self.engine.begin_read()?;
        let result = (|| -> Result<Option<u64>> {
            let Some(mut br) = r.blob_read(table_id, pk_values, col_idx, range)? else {
                return Ok(None);
            };
            let mut written = 0u64;
            while let Some(chunk) = br.next()? {
                out.write_all(&chunk).map_err(Error::from)?;
                written += chunk.len() as u64;
            }
            Ok(Some(written))
        })();
        r.finish()?;
        result
    }

    /// Read a system record (see [`Database::sys_record_put`]). Runs in a
    /// read transaction; never touches the writer lock.
    pub fn sys_record_get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let subkey = sys_record_subkey(ns, key)?;
        let r = self.engine.begin_read()?;
        let rec = r.sys_get(&subkey)?;
        r.finish()?;
        Ok(rec)
    }

    /// All records in namespace `ns`, as `(key, value)` pairs in key order,
    /// with the namespace prefix stripped. Read transaction only.
    pub fn sys_record_scan(&self, ns: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        check_sys_ns(ns)?;
        let mut prefix = Vec::with_capacity(ns.len() + 1);
        prefix.extend_from_slice(ns.as_bytes());
        prefix.push(0);
        let r = self.engine.begin_read()?;
        let all = r.sys_scan()?;
        r.finish()?;
        Ok(all
            .into_iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k[prefix.len()..].to_vec(), v))
            .collect())
    }

    // ---------------- internal: plan registry protocol ----------------

    /// Local-cache/registry publication for a freshly compiled plan.
    fn register(&self, hash: PlanHash, plan: CompiledPlan, sql: &str) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.cache.read().expect(POISON).get(&hash) {
            return Ok(p.clone());
        }
        let plan = Arc::new(plan);
        // A plan that calls a HOST-registered UDF is valid ONLY for this
        // connection (its closures live here), so it MUST NOT enter the shared
        // content-hashed `plan/<hash>` registry (design/DESIGN-UDF.md §2). Keep it
        // in the local cache only — this connection can still `execute(hash)` it,
        // and it is recompiled per connection rather than shared. The local cache
        // is dropped whenever the UDF set changes (`register_host_function`).
        if plan.contains_host_call() {
            self.cache.write().expect(POISON).insert(hash, plan.clone());
            return Ok(plan);
        }
        let blob = plan.encode();
        let subkey = plan_subkey(&hash);

        // READ-FIRST probe: is an identical record already published?
        let published = {
            let r = self.engine.begin_read()?;
            let rec = r.sys_get(&subkey)?;
            r.finish()?;
            rec.as_deref()
                .and_then(registry::parse_record)
                .is_some_and(|r| r.blob == blob)
        };
        if !published {
            // Genuine miss (or corrupt/stale entry): short write txn inserts
            // or overwrites. Re-checked under the lock so a racing process
            // that just published the same plan costs us no write.
            let mut w = self.engine.begin_write()?;
            match registry::insert_plan(&mut w, &hash, sql, &blob) {
                Ok(true) => w.commit()?,
                Ok(false) => w.abort(),
                Err(e) => {
                    w.abort();
                    return Err(e);
                }
            }
        }
        self.cache
            .write()
            .expect(POISON)
            .insert(hash, plan.clone());
        Ok(plan)
    }

    /// Local cache, else registry load: parse the record, fully re-validate
    /// the blob, verify the content hash, and cache locally.
    ///
    /// Deliberately does NOT refresh the entry's `last_used_txn`: bumping it
    /// needs the single writer lock, and this path serves read-only executes
    /// — a cold-cache SELECT must never block behind (or msync alongside) a
    /// live writer (design/DESIGN.md §7.3: read-only plans route to a read
    /// transaction, the writer lock is never touched). Registry recency is
    /// therefore insert time plus the ride-along bump in
    /// [`WriteSession::plan_by_hash`]; eviction tolerates entries whose
    /// `last_used_txn` was never bumped (see `registry::evict_if_full`).
    fn cached_or_load(&self, hash: &PlanHash) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.cache.read().expect(POISON).get(hash) {
            return Ok(p.clone());
        }
        let subkey = plan_subkey(hash);
        let record = {
            let r = self.engine.begin_read()?;
            let rec = r.sys_get(&subkey)?;
            r.finish()?;
            rec.ok_or(Error::UnknownPlan(*hash))?
        };
        let plan = Arc::new(decode_registry_plan(&record, hash, &self.schema())?);
        self.cache
            .write()
            .expect(POISON)
            .insert(*hash, plan.clone());
        Ok(plan)
    }

    // ---------------- internal: execution routing ----------------

    /// Route by the (decode-recomputed) footprint: read transaction for
    /// read-only plans, autocommit write transaction for DML.
    fn run_plan(
        &self,
        hash: Option<&PlanHash>,
        plan: &CompiledPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        if matches!(
            plan.stmt,
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback
        ) {
            return Err(Error::Unsupported(
                "BEGIN/COMMIT/ROLLBACK do nothing through execute(); \
                 use Database::begin() and WriteSession"
                    .into(),
            ));
        }
        if matches!(
            plan.stmt,
            PlanStmt::Savepoint(_) | PlanStmt::Release(_) | PlanStmt::RollbackTo(_)
        ) {
            return Err(Error::Unsupported(
                "SAVEPOINT/RELEASE/ROLLBACK TO require an open transaction; \
                 use Database::begin() and run them through the WriteSession \
                 (there is no autocommit savepoint)"
                    .into(),
            ));
        }
        if plan.footprint.read_only {
            // Reads never touch the writer lock or the ring.
            let mut partial = false;
            // Host UDF closures for the executor, only for a plan that calls one
            // (design/DESIGN-UDF.md). Snapshot once; kept alive for the whole
            // scan. Scalars and aggregates ride ONE gate — a plan naming a host
            // aggregate reports `contains_host_call` too.
            let tables = self.host_tables(plan);
            let host: Option<&dyn mpedb_types::HostFns> =
                tables.as_ref().map(|(f, _)| f as &dyn mpedb_types::HostFns);
            let host_aggs: Option<&dyn mpedb_types::HostAggs> =
                tables.as_ref().map(|(_, a)| a as &dyn mpedb_types::HostAggs);
            let r = self.engine.begin_read()?;
            // Staleness check UNDER THE SAME PIN that scans the rows (§4.3):
            // a policy edit that landed since compile invalidates the plan.
            // On error `r` drops here, releasing the reader slot.
            self.validate_policy_read(hash, plan, &r)?;
            let res = {
                let mut ctx = ReadCtx(&r, host, host_aggs);
                exec_stmt(&mut ctx, &self.schema(), plan, params, &mut partial)
            };
            match res {
                Ok(out) => {
                    r.finish()?; // SnapshotEvicted here invalidates the rows
                    Ok(out)
                }
                Err(e) => Err(e), // Drop releases the reader slot
            }
        } else {
            self.run_write_plan(hash, plan, params)
        }
    }

    /// Autocommit DML with Phase-2 group commit (design/DESIGN.md §5.3).
    ///
    /// Uncontended: take the lock immediately and lead (executing our own
    /// statement plus any pending intents in one commit). Contended: publish
    /// the statement as an intent and wait-or-lead — a bounded futex wait,
    /// then a `try_begin_write`, repeated. A SIGKILLed leader can therefore
    /// never strand us, and N contended writers cost one meta flip (and one
    /// msync in durable mode) instead of N.
    fn run_write_plan(
        &self,
        hash: Option<&PlanHash>,
        plan: &CompiledPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        // fast path: uncontended — validate policy staleness under the writer
        // lock we now hold (no policy edit can race it, §4).
        if let Some(mut txn) = self.engine.try_begin_write()? {
            self.validate_policy_write(hash, plan, &mut txn)?;
            return ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                .map(|r| r.expect("own statement always yields a result"));
        }
        // Contended: pre-check staleness on a read snapshot before enqueueing.
        // The leader that executes our intent also holds the writer lock, so a
        // policy edit racing an in-flight *durable* enqueued write has a bounded
        // one-commit residual window (documented limitation; durability=none
        // never rides the ring and is fully covered by the fast path above).
        {
            let r = self.engine.begin_read()?;
            let res = self.validate_policy_read(hash, plan, &r);
            let _ = r.finish();
            res?;
        }
        // Contended. Whether to ride the ring depends on the storage medium:
        // group commit pays for itself when commits are expensive (an msync
        // per commit in durability=commit — hundreds of µs to ms depending on
        // the disk), because N writers share ONE flush. On pure-memory /
        // non-durable databases a commit is microseconds, and the ring's
        // wait/wake round-trips would only add latency — block directly (the
        // lock holder still drains any intents, so mixed deployments stay
        // live).
        //
        // One plan class NEVER rides the ring: a plan that calls a host UDF
        // (design/DESIGN-UDF.md). The ring is a cross-PROCESS queue — a leader
        // loads an intent's plan BY HASH FROM THE SHARED REGISTRY, and a
        // host-call plan is deliberately never published there, so an enqueued
        // one could only come back as `UnknownPlan`. More importantly the
        // closures are connection-local: no other process may run our UDF, and
        // we may not run theirs. Leading our own statement keeps it in the
        // process that owns the closures.
        let use_ring = ring_exec::ring_enabled(self) && !plan.contains_host_call();
        let ring = self.engine.ring();
        let enqueued = if use_ring {
            hash.and_then(|h| {
                let blob = ring_exec::encode_params(params);
                ring.enqueue(h, &blob)
            })
        } else {
            None
        };
        let Some((idx, owned)) = enqueued else {
            // ring full / params too large / hash unavailable: block directly
            let txn = self.engine.begin_write()?;
            return ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                .map(|r| r.expect("own statement always yields a result"));
        };
        loop {
            if let Some(r) = ring.wait_result(idx, std::time::Duration::from_millis(2)) {
                ring.release(idx, owned);
                return match ring_exec::decode_ring_result(r) {
                    // the leader had no way to load our plan (e.g. local-only
                    // seeding); we DO have it — run it directly instead
                    Err(Error::UnknownPlan(_)) => {
                        let txn = self.engine.begin_write()?;
                        ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                            .map(|r| r.expect("own statement always yields a result"))
                    }
                    other => other,
                };
            }
            // another leader at work → keep waiting; lock free → we lead,
            // and our own intent is drained like any other
            if let Some(txn) = self.engine.try_begin_write()? {
                ring_exec::lead_and_execute(self, txn, None)?;
                if let Some(r) = ring.try_take_result(idx) {
                    ring.release(idx, owned);
                    return ring_exec::decode_ring_result(r);
                }
                // not drained (should not happen — we were READY); retry
            }
        }
    }
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
}

/// One entry on the [`WriteSession`] savepoint stack.
struct NamedSavepoint {
    name: String,
    snap: mpedb_core::TxnSavepointFull,
}

// ------------------------------------------------ system-record key helpers

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

fn sys_record_subkey(ns: &str, key: &[u8]) -> Result<Vec<u8>> {
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

impl WriteSession<'_> {
    /// Execute a prepared plan inside this transaction. Plans are resolved
    /// from the local cache or read from the registry *through this
    /// transaction* (no extra locking; the session already holds the writer
    /// lock).
    pub fn execute(&mut self, hash: &PlanHash, params: &[Value]) -> Result<ExecResult> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        let plan = self.plan_by_hash(hash)?;
        self.run(&plan, params)
    }

    /// Compile and run `sql` inside this transaction.
    ///
    /// Unlike [`Database::query`], the compiled plan is cached **only in the
    /// local plan cache and never published to the shared registry**: the
    /// registry insert needs its own write transaction, and this session
    /// already holds the single writer lock (the ERRORCHECK mutex would turn
    /// that into an error). Prepare statements you want shared *before*
    /// opening the session.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        // #95: DDL (CREATE/DROP/ALTER TABLE, CREATE INDEX) runs THROUGH this
        // session's transaction so the schema change lives in the txn's COW
        // catalog pages and commits/rolls back atomically with the session's
        // DML — never a mid-transaction commit.
        if let Some(ddl) = mpedb_sql::parse_ddl(sql)? {
            return self.apply_ddl(ddl);
        }
        // Compile against THIS session's schema view — which includes any DDL
        // this session already applied (its txn's captured bundle), not just the
        // committed schema. Policies/views are read from the committed catalog
        // (a policy/view created inside this uncommitted session is not yet
        // visible here).
        let schema = self.txn.schema_bundle();
        let (plan, is_explain) = self.db.compile_maybe_explain_with_schema(sql, &schema)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(&schema)));
        }
        let hash = plan.hash();
        let plan = {
            let mut cache = self.db.cache.write().expect(POISON);
            cache.entry(hash).or_insert_with(|| Arc::new(plan)).clone()
        };
        self.run(&plan, params)
    }

    /// Commit everything written through this session.
    ///
    /// A poisoned session (see the type-level docs) refuses: the transaction
    /// is rolled back and [`Error::Unsupported`] is returned, so a partially
    /// applied statement can never be persisted.
    pub fn commit(self) -> Result<()> {
        if self.poisoned {
            self.txn.abort();
            return Err(poisoned_err());
        }
        self.txn.commit()
    }

    /// Discard everything written through this session.
    pub fn rollback(self) {
        self.txn.abort()
    }

    // ------------- replication / CDC plane (DESIGN-MIRROR §3, §5.4) -------------
    //
    // The mirror applier and importer operate at the engine typed-API level —
    // BELOW RLS — inside a single session so row writes, CDC dirty-set changes,
    // and the mirror cursor all commit atomically in one meta flip. These
    // methods expose exactly that plane; they do not touch policies. They never
    // poison the session (the mirror manages its own savepoints); a failing op
    // leaves whatever partial effect the engine produced, to be unwound by the
    // caller via [`WriteSession::savepoint`]/[`WriteSession::rollback_to`].

    /// Turn CDC dirty-set capture on/off for this session's transaction. The
    /// importer and applier set it `false` so their own writes are not
    /// self-captured (DESIGN-MIRROR §3.8).
    pub fn set_capture(&mut self, on: bool) {
        self.txn.set_capture(on);
    }

    /// Allow this session to draw from the reserved control-page band so a
    /// control-plane commit succeeds even when the data region is full
    /// (DESIGN-MIRROR §3.10). Use only for small control writes.
    pub fn set_reserved_alloc(&mut self, on: bool) {
        self.txn.set_reserved_alloc(on);
    }

    /// Blind typed INSERT of a full row into `table_id`.
    pub fn insert_row(&mut self, table_id: u32, values: &[Value]) -> Result<()> {
        self.txn.insert_row(table_id, values)
    }

    /// INSERT a row whose column `stream_col` is PULLED from `src` a page at a
    /// time, so a large value is never resident (#43).
    ///
    /// `values[stream_col]` is a placeholder for the type check — pass an empty
    /// `Blob`/`Text`; the length comes from `src.len()`. The streamed column must
    /// be the row's last variable-length column.
    ///
    /// **Pull, not push, and it is deliberate.** A `writer.write_all(chunk)` API
    /// would hold the writer lock across YOUR code, so a blob arriving off a
    /// socket would block every other writer for as long as the network took.
    /// Here the engine asks for bytes as fast as it can write them. `src` is
    /// still called with the lock held — keep it cheap.
    ///
    /// Refused on a table with a secondary UNIQUE index: that probe needs the
    /// value, and the point of this call is that nobody has it.
    /// Copy a whole file into `stream_col` as a blob, streamed a page at a time
    /// so the file is never resident (#43). "Put this file in the database" as
    /// one call — the memory ceiling is one overflow page, not the file size, so
    /// a 4 GiB file inserts on a machine that could not hold it in RAM.
    ///
    /// `values[stream_col]` is a placeholder for the type check (pass an empty
    /// `Blob`/`Text`); the length comes from the file. The streamed column must
    /// be the row's last variable-length column, and the table must have no
    /// secondary UNIQUE index — same constraints as [`Self::insert_streaming`].
    pub fn insert_file(
        &mut self,
        table: &str,
        values: &[Value],
        stream_col: usize,
        path: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // FileBlobSource, not ReaderBlobSource: the file handle is what lets
        // the extent path bulk-copy kernel-side (copy_file_range, #50).
        let mut src = FileBlobSource::new(file, len);
        self.insert_streaming(table, values, stream_col, &mut src)
    }

    pub fn insert_streaming(
        &mut self,
        table: &str,
        values: &[Value],
        stream_col: usize,
        src: &mut dyn mpedb_core::btree::BlobSource,
    ) -> Result<()> {
        let table_id = self
            .db
            .engine
            .schema()
            .tables
            .iter()
            .position(|t| t.name == table)
            .ok_or_else(|| Error::Config(format!("no such table: {table}")))?
            as u32;
        self.txn
            .insert_row_streaming(table_id, values, stream_col, src)
    }

    /// Replace the full row with the given PK; returns whether it existed.
    pub fn update_by_pk(&mut self, table_id: u32, values: &[Value]) -> Result<bool> {
        self.txn.update_by_pk(table_id, values)
    }

    /// Delete by PK; returns whether the row existed.
    pub fn delete_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<bool> {
        self.txn.delete_by_pk(table_id, pk_values)
    }

    /// Read a full row by PK within this session's view.
    pub fn get_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        self.txn.get_by_pk(table_id, pk_values)
    }

    /// Statement-level savepoint for the mirror's per-op apply (§5.4/§6). Pairs
    /// with [`WriteSession::rollback_to`].
    pub fn savepoint(&self) -> mpedb_core::TxnSavepoint {
        self.txn.savepoint()
    }

    /// Roll back to a savepoint taken in this session.
    pub fn rollback_to(&mut self, sp: mpedb_core::TxnSavepoint) {
        self.txn.rollback_to(sp)
    }

    /// Store a namespaced system record through THIS transaction (atomic with
    /// the session's row writes — unlike [`Database::sys_record_put`], which
    /// opens its own txn and would self-lock under an open session).
    pub fn sys_record_put(&mut self, ns: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let subkey = sys_record_subkey(ns, key)?;
        if value.len() > SYS_RECORD_MAX_VALUE {
            return Err(Error::Unsupported(format!(
                "system record value too large ({} > {SYS_RECORD_MAX_VALUE} bytes)",
                value.len()
            )));
        }
        self.txn.sys_put(&subkey, value)
    }

    /// Read a namespaced system record through this transaction.
    pub fn sys_record_get(&mut self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let subkey = sys_record_subkey(ns, key)?;
        self.txn.sys_get(&subkey)
    }

    /// Delete a namespaced system record through this transaction.
    pub fn sys_record_delete(&mut self, ns: &str, key: &[u8]) -> Result<bool> {
        let subkey = sys_record_subkey(ns, key)?;
        self.txn.sys_delete(&subkey)
    }

    /// Scan namespaced system records whose key is in `[lo, hi)`, prefix-bounded
    /// (the mirror's dirty-set / park scans). Keys returned with the namespace
    /// prefix stripped.
    pub fn sys_record_scan_range(
        &mut self,
        ns: &str,
        lo: &[u8],
        hi: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let klo = sys_record_subkey(ns, lo)?;
        let khi = sys_record_subkey(ns, hi)?;
        let strip = ns.len() + 1;
        Ok(self
            .txn
            .sys_scan_range(&klo, &khi)?
            .into_iter()
            .map(|(k, v)| (k[strip..].to_vec(), v))
            .collect())
    }

    fn plan_by_hash(&mut self, hash: &PlanHash) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.db.cache.read().expect(POISON).get(hash) {
            return Ok(p.clone());
        }
        let subkey = plan_subkey(hash);
        let Some(record) = self.txn.sys_get(&subkey)? else {
            return Err(Error::UnknownPlan(*hash));
        };
        let plan = Arc::new(decode_registry_plan(&record, hash, &self.db.schema())?);
        // last_used_txn refresh rides on this transaction (commits or rolls
        // back with it — best-effort bookkeeping either way). A failed put
        // (e.g. DbFull) leaves the record as it was and must never fail the
        // user's statement over eviction metadata.
        if let Some(patched) = patched_last_used(&record, self.txn.meta.txn_id + 1) {
            let _ = self.txn.sys_put(&subkey, &patched);
        }
        self.db
            .cache
            .write()
            .expect(POISON)
            .insert(*hash, plan.clone());
        Ok(plan)
    }

    fn run(&mut self, plan: &CompiledPlan, params: &[Value]) -> Result<ExecResult> {
        // Transaction/savepoint control is handled here against the session's
        // own state, never compiled to an access path. `run` is only reached
        // from `query`/`execute`, which already refuse a poisoned session, so a
        // savepoint op never runs on a torn transaction.
        match &plan.stmt {
            PlanStmt::Savepoint(name) => {
                let snap = self.txn.savepoint_full()?;
                self.savepoints.push(NamedSavepoint {
                    name: name.clone(),
                    snap,
                });
                return Ok(ExecResult::Affected(0));
            }
            PlanStmt::Release(name) => {
                return self.release_savepoint(name).map(|()| ExecResult::Affected(0));
            }
            PlanStmt::RollbackTo(name) => {
                return self
                    .rollback_to_savepoint(name)
                    .map(|()| ExecResult::Affected(0));
            }
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {
                return Err(Error::Unsupported(
                    "the session already is a transaction; \
                     use WriteSession::commit()/rollback()"
                        .into(),
                ));
            }
            _ => {}
        }
        // Staleness check under this session's own write txn (holds the writer
        // lock, so no policy edit can race it). Local-cache plans only, so no
        // shared-registry eviction is needed here.
        self.db.validate_policy_write(None, plan, &mut self.txn)?;
        let full = session::resolve_params(plan, params, &self.session)?;
        let triggers = self.db.trigger_set()?;
        // Execute against the session's OWN schema view (== the txn's captured
        // bundle), so a statement touching a table this session created/altered
        // earlier resolves against the shape it will commit with (#95). For a
        // session that has applied no DDL this equals the committed schema.
        let schema = self.txn.schema_bundle();
        let mut partial = false;
        // Host UDF closures for THIS statement (design/DESIGN-UDF.md). This is
        // the path CPython's implicit transaction takes: after the first DML,
        // every statement — reads included — arrives here, so a UDF that
        // resolves in autocommit must resolve here too or Django breaks the
        // moment it stops committing between statements. `None` for a plan with
        // no host call, which then runs on the bare `&mut WriteTxn` exactly as
        // before.
        let tables = self.db.host_tables(plan);
        let host: Option<&dyn mpedb_types::HostFns> =
            tables.as_ref().map(|(f, _)| f as &dyn mpedb_types::HostFns);
        let aggs: Option<&dyn mpedb_types::HostAggs> =
            tables.as_ref().map(|(_, a)| a as &dyn mpedb_types::HostAggs);
        let mut ctx = exec::WriteCtx::new(&mut self.txn, host, aggs);
        let res = exec::exec_stmt_triggered(
            &mut ctx,
            &schema,
            plan,
            &full,
            &mut partial,
            &triggers,
            0,
        );
        if res.is_err() && partial {
            // The failed statement may have applied part of its effects; the
            // transaction no longer reflects whole statements. See the
            // poisoning contract in the WriteSession docs.
            self.poisoned = true;
        }
        res
    }

    /// Innermost (topmost) savepoint matching `name`, case-insensitively —
    /// sqlite's shadowing rule (a `RELEASE`/`ROLLBACK TO` targets the most
    /// recently opened savepoint of that name).
    fn find_savepoint(&self, name: &str) -> Option<usize> {
        self.savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// `RELEASE [SAVEPOINT] <name>`: drop `<name>` and everything above it from
    /// the stack. The changes made since STAY (they merge into the enclosing
    /// savepoint/transaction) — only the markers and their snapshots go. It does
    /// NOT commit to disk (the surrounding `WriteSession` still owns the
    /// transaction); commit is `WriteSession::commit`.
    fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let idx = self
            .find_savepoint(name)
            .ok_or_else(|| no_such_savepoint(name))?;
        self.savepoints.truncate(idx);
        Ok(())
    }

    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] <name>`: undo every change since
    /// `<name>` was opened, but KEEP `<name>` on the stack (it can be rolled
    /// back to again). Savepoints opened after it are discarded.
    fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        let idx = self
            .find_savepoint(name)
            .ok_or_else(|| no_such_savepoint(name))?;
        // Keep the target; drop everything opened after it.
        self.savepoints.truncate(idx + 1);
        // `WriteTxn::rollback_to_full` consumes the snapshot; hand it a clone so
        // the original stays on the stack for a repeat `ROLLBACK TO` the same
        // name. A refusal (a large-blob extent write in the scope) leaves the
        // stack intact and surfaces cleanly.
        let snap = self.savepoints[idx].snap.clone();
        self.txn.rollback_to_full(snap)
    }

    /// Apply a parsed DDL statement THROUGH this session's transaction (#95).
    ///
    /// Table DDL (CREATE / DROP / ALTER TABLE, CREATE INDEX, CREATE VIRTUAL
    /// TABLE) runs on `self.txn`, so the schema change lives in the txn's COW
    /// catalog pages and commits / rolls back atomically with the session's
    /// DML — no separate transaction, no mid-transaction commit (the forbidden
    /// "commit + autocommit-DDL + re-begin" hack). After a successful mutation
    /// the txn's captured schema bundle is rebuilt so a later statement in the
    /// SAME session sees the change, and the shared plan cache is dropped
    /// (mirroring the autocommit DDL discipline in `ddl_apply.rs`).
    ///
    /// A mid-operation failure (e.g. `DbFull` during an ALTER row rewrite) is
    /// undone with a full savepoint taken before the mutation, leaving the
    /// session usable — scoped to just the DDL, like the autocommit path's
    /// abort-on-error but without discarding the session's other work. If that
    /// undo is itself impossible (a large-blob extent write in the DDL), the
    /// session is poisoned instead (sound: commit then refuses).
    ///
    /// DDL that lives in a separate store or needs its own transaction (VIEW /
    /// POLICY / TRIGGER, RLS) is refused inside a session with a clear error;
    /// `ANALYZE` / `REINDEX` are accepted no-ops.
    fn apply_ddl(&mut self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::DdlStmt;
        if self.poisoned {
            return Err(poisoned_err());
        }
        // A SAVEPOINT is open: a `ROLLBACK TO` it would have to revert the txn's
        // captured schema bundle too, which the engine's savepoint snapshot does
        // NOT restore (it captures catalog PAGES, not the in-memory bundle).
        // Refuse cleanly rather than risk a bundle/catalog desync (#95).
        if !self.savepoints.is_empty() {
            return Err(Error::Unsupported(
                "DDL inside a SAVEPOINT is not supported by mpedb; RELEASE or \
                 ROLLBACK the savepoint first"
                    .into(),
            ));
        }
        // No-op DDL never touches the catalog — nothing to snapshot or reload.
        if matches!(ddl, DdlStmt::Analyze { .. } | DdlStmt::Reindex { .. }) {
            return Ok(ExecResult::Affected(0));
        }
        // Snapshot the txn BEFORE the mutation so a partial failure rolls back
        // cleanly. This captures catalog_root, table_roots, dirty-page contents
        // and schema_gen_bump — the bundle is NOT touched here, and we only
        // advance it AFTER the mutation succeeds, so the pre-mutation bundle
        // stays valid for the rollback path.
        let snap = self.txn.savepoint_full()?;
        match self.apply_ddl_inner(ddl) {
            Ok(res) => {
                // The mutation is in the txn's COW catalog pages. Rebuild the
                // txn's captured bundle so a later statement in this session
                // sees it, then drop the shared plan cache (a cached plan may
                // reference the pre-DDL catalog — the autocommit discipline).
                if let Err(e) = self.txn.reload_bundle_from_catalog() {
                    // Reading back what we just wrote failed (Corrupt): the txn
                    // view is now stale — poison so it cannot be committed.
                    self.poisoned = true;
                    return Err(e);
                }
                self.db.cache.write().expect(POISON).clear();
                Ok(res)
            }
            Err(e) => {
                // Undo any partial effect. `rollback_to_full` restores the
                // catalog/pages; the bundle was never advanced, so the session
                // view stays consistent and usable. If the scope crossed a
                // large-blob extent write it refuses — the txn is then torn, so
                // poison.
                if self.txn.rollback_to_full(snap).is_err() {
                    self.poisoned = true;
                }
                Err(e)
            }
        }
    }

    /// The DDL dispatch for [`apply_ddl`](Self::apply_ddl): resolve names against
    /// this session's CURRENT schema view and call the engine DDL primitive on
    /// `self.txn`. Any error (or the rollback / reload) is handled by the caller.
    fn apply_ddl_inner(&mut self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::DdlStmt;
        // Resolve against the session's own (possibly uncommitted-DDL) view, so
        // e.g. a CREATE INDEX names a table this same session just created.
        let schema = self.txn.schema_bundle();
        let resolve = |name: &str| -> Result<u32> {
            schema
                .schema
                .table_id(name)
                .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{name}`")))
        };
        match ddl {
            DdlStmt::CreateTable(spec) => {
                let def = crate::ddl_apply::table_def_from_spec(spec)?;
                self.txn.create_table(def)?;
            }
            DdlStmt::CreateVirtualTable(spec) => {
                if schema.schema.table_id(&spec.name).is_some() {
                    if spec.if_not_exists {
                        return Ok(ExecResult::Affected(0));
                    }
                    return Err(Error::Bind(format!(
                        "CREATE VIRTUAL TABLE: `{}` already exists",
                        spec.name
                    )));
                }
                let def = crate::ddl_apply::virtual_table_def_from_spec(spec)?;
                self.txn.create_table(def)?;
            }
            DdlStmt::DropTable { name, if_exists } => {
                let id = match schema.schema.table_id(&name) {
                    Some(id) => id,
                    None => {
                        if if_exists {
                            return Ok(ExecResult::Affected(0));
                        }
                        return Err(Error::Bind(format!("DROP TABLE: no such table `{name}`")));
                    }
                };
                // Cascade: a dropped table's triggers are dead — remove their
                // records in the same commit (DESIGN-TRIGGERS §3.1).
                crate::trigger::cascade_drop_triggers(&mut self.txn, id)?;
                self.txn.drop_table(id)?;
            }
            DdlStmt::AlterRenameTable { table, new_name } => {
                let id = resolve(&table)?;
                self.txn.alter_rename_table(id, &new_name)?;
            }
            DdlStmt::AlterRenameColumn { table, column, new_name } => {
                let id = resolve(&table)?;
                self.txn.alter_rename_column(id, &column, &new_name)?;
            }
            DdlStmt::AlterAddColumn { table, column } => {
                let (col, fill) = crate::ddl_apply::add_column_from_spec(&table, column)?;
                let id = resolve(&table)?;
                self.txn.alter_add_column(id, col, fill)?;
            }
            DdlStmt::AlterDropColumn { table, column } => {
                let id = resolve(&table)?;
                self.txn.alter_drop_column(id, &column)?;
            }
            DdlStmt::CreateIndex { table, columns, unique, .. } => {
                let id = schema
                    .schema
                    .table_id(&table)
                    .ok_or_else(|| Error::Bind(format!("CREATE INDEX: no such table `{table}`")))?;
                let t = schema.schema.table(id).expect("table_id resolved");
                let cols = crate::ddl_apply::resolve_index_columns(t, &table, &columns)?;
                // Idempotent by shape: an identical index already present is a no-op.
                if t.indexes.iter().any(|ix| ix.columns == cols && ix.unique == unique) {
                    return Ok(ExecResult::Affected(0));
                }
                self.txn.create_index(id, cols, unique)?;
            }
            // These live in a separate store or open their own transaction, so
            // they cannot ride this session's txn. Refuse cleanly (out of scope
            // for #95); run them in autocommit.
            DdlStmt::CreateView { .. }
            | DdlStmt::DropView { .. }
            | DdlStmt::CreatePolicy(_)
            | DdlStmt::DropPolicy { .. }
            | DdlStmt::AlterRls { .. }
            | DdlStmt::CreateTrigger(_)
            | DdlStmt::DropTrigger { .. } => {
                return Err(Error::Unsupported(
                    "CREATE/DROP VIEW, POLICY or TRIGGER and ALTER … ROW LEVEL \
                     SECURITY are not supported inside a transaction; run them in \
                     autocommit (outside BEGIN/COMMIT)"
                        .into(),
                ));
            }
            // Handled before the savepoint in `apply_ddl`; unreachable here.
            DdlStmt::Analyze { .. } | DdlStmt::Reindex { .. } => {}
        }
        Ok(ExecResult::Affected(0))
    }
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

// -------------------------------------------------------------------- tests

/// In-crate tests that need access to the private engine handle (raw registry
/// corruption, eviction accounting). The end-to-end suite lives in
/// `tests/facade.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQ: AtomicU64 = AtomicU64::new(0);

    fn test_paths(name: &str) -> PathBuf {
        let dir = if Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        dir.join(format!(
            "mpedb-facade-unit-{name}-{}-{}.mpedb",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_config(name: &str, size_mb: u64) -> (Config, PathBuf) {
        let path = test_paths(name);
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 64

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true
"#,
            path.display()
        );
        (Config::from_toml_str(&toml).unwrap(), path)
    }

    /// Two independent tables in one file — the shape every other write test
    /// here lacks.
    fn two_table_config(name: &str) -> (Config, PathBuf) {
        let dir = if Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let path = dir.join(format!("mpedb-{name}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = 8

[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"
"#,
            path.display()
        );
        (Config::from_toml_str(&toml).unwrap(), path)
    }

    /// Host scalar UDF dispatch through the facade (design/DESIGN-UDF.md): a
    /// registered `create_function`-style closure runs in SELECT/WHERE, with a
    /// bound param, and over text; an unregistered name errors; unregistering
    /// restores the error. Also verifies the plan-sharing bypass — a UDF plan is
    /// never published to the shared registry.
    #[test]
    fn host_scalar_udf_dispatch_and_registry_bypass() {
        let (cfg, path) = test_config("host-udf", 8);
        let _g = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            &[],
        )
        .unwrap();

        let ints = |r: ExecResult| -> Vec<i64> {
            let ExecResult::Rows { rows, .. } = r else { panic!("want rows") };
            rows.iter()
                .map(|row| match row[0] {
                    Value::Int(x) => x,
                    ref v => panic!("want int, got {v:?}"),
                })
                .collect()
        };

        // Unregistered → bind error.
        assert!(db.query("SELECT plus1(id) FROM users", &[]).is_err());

        db.register_host_function("plus1", 1, |a| match a {
            [Value::Int(x)] => Ok(Value::Int(x + 1)),
            _ => Err(Error::Unsupported("plus1 wants one int".into())),
        });
        db.register_host_function("addk", 2, |a| match a {
            [Value::Int(x), Value::Int(k)] => Ok(Value::Int(x + k)),
            _ => Err(Error::Unsupported("addk wants two ints".into())),
        });
        db.register_host_function("shout", 1, |a| match a {
            [Value::Text(s)] => Ok(Value::Text(s.to_uppercase())),
            _ => Err(Error::Unsupported("shout wants text".into())),
        });

        // SELECT plus1(id)
        assert_eq!(
            ints(db.query("SELECT plus1(id) FROM users ORDER BY id", &[]).unwrap()),
            vec![2, 3, 4]
        );
        // WHERE plus1(id) = 3  → id 2
        assert_eq!(
            ints(db.query("SELECT id FROM users WHERE plus1(id) = 3", &[]).unwrap()),
            vec![2]
        );
        // Bound param: addk(id, $1)
        assert_eq!(
            ints(
                db.query("SELECT addk(id, $1) FROM users ORDER BY id", &[Value::Int(10)])
                    .unwrap()
            ),
            vec![11, 12, 13]
        );
        // Text UDF.
        let ExecResult::Rows { rows, .. } =
            db.query("SELECT shout(email) FROM users ORDER BY id", &[]).unwrap()
        else {
            panic!("want rows")
        };
        assert_eq!(rows[0][0], Value::Text("A".into()));

        // Plan-sharing bypass: a UDF plan must NOT be in the shared registry, so a
        // FRESH handle to the same file (which registered no UDF) cannot execute
        // it by hash — it is UnknownPlan, not a silently-shared plan.
        let hash = db.prepare("SELECT plus1(id) FROM users").unwrap();
        let db2 = Database::open_from_file(_g.0.as_path()).unwrap();
        assert!(matches!(
            db2.execute(&hash, &[]),
            Err(Error::UnknownPlan(_))
        ));
        // The registering handle still runs it by hash (local cache).
        assert_eq!(ints(db.execute(&hash, &[]).unwrap()), vec![2, 3, 4]);

        // Unregister → unknown again.
        assert!(db.unregister_host_function("plus1", 1));
        assert!(db.query("SELECT plus1(id) FROM users", &[]).is_err());
    }

    /// Host AGGREGATE dispatch through the facade (design/DESIGN-UDF.md stage 2):
    /// a registered `xStep`/`xFinal`-style factory accumulates per group, sees
    /// NULL arguments (unlike a built-in), honors `FILTER`, produces one value
    /// for an EMPTY group, and — like a scalar UDF — keeps its plan out of the
    /// shared registry.
    #[test]
    fn host_aggregate_dispatch_and_registry_bypass() {
        /// `mysum(x)`: sums ints, and COUNTS the NULLs it is handed — the visible
        /// proof that a host aggregate is stepped for every row, where a built-in
        /// `sum` would have skipped them.
        #[derive(Default)]
        struct MySum {
            total: i64,
            nulls: i64,
        }
        impl mpedb_types::HostAggState for MySum {
            fn step(&mut self, args: &[Value]) -> Result<()> {
                match args {
                    [Value::Int(x)] => self.total += x,
                    [Value::Null] => self.nulls += 1,
                    _ => return Err(Error::Unsupported("mysum wants one int".into())),
                }
                Ok(())
            }
            fn finish(self: Box<Self>) -> Result<Value> {
                Ok(Value::Int(self.total * 1000 + self.nulls))
            }
        }

        let (cfg, path) = test_config("host-agg", 8);
        let _g = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let ints = |r: ExecResult| -> Vec<i64> {
            let ExecResult::Rows { rows, .. } = r else { panic!("want rows") };
            rows.iter()
                .map(|row| match row[0] {
                    Value::Int(x) => x,
                    ref v => panic!("want int, got {v:?}"),
                })
                .collect()
        };

        // Unregistered → bind error (an unknown function, not a silent NULL).
        assert!(db.query("SELECT mysum(id) FROM users", &[]).is_err());

        db.register_host_aggregate("mysum", 1, || Box::<MySum>::default());

        // EMPTY table: one group, `xFinal` on a never-stepped state.
        assert_eq!(ints(db.query("SELECT mysum(id) FROM users", &[]).unwrap()), vec![0]);

        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
            &[],
        )
        .unwrap();

        // Bare aggregate over the whole table: 1+2+3+4 = 10, no NULLs.
        assert_eq!(ints(db.query("SELECT mysum(id) FROM users", &[]).unwrap()), vec![10_000]);
        // GROUP BY: odd ids 1+3 = 4, even 2+4 = 6.
        assert_eq!(
            ints(
                db.query("SELECT mysum(id) FROM users GROUP BY id % 2 ORDER BY id % 2", &[])
                    .unwrap()
            ),
            vec![6_000, 4_000]
        );
        // FILTER (WHERE …) applies to a host aggregate exactly as to a built-in.
        assert_eq!(
            ints(
                db.query("SELECT mysum(id) FILTER (WHERE id > 2) FROM users", &[])
                    .unwrap()
            ),
            vec![7_000]
        );
        // NULL arguments REACH `xStep` (sqlite's rule; a built-in would skip
        // them): `mysum(NULL)` sees 4 NULLs and no ints.
        assert_eq!(ints(db.query("SELECT mysum(NULL) FROM users", &[]).unwrap()), vec![4]);
        // The result is dynamically typed (`ColumnType::Any`), exactly as a host
        // SCALAR's is — which today means arithmetic over it is refused at bind.
        // Asserted rather than left implicit: it is the SAME stage-1 limitation,
        // not a new one, and a future relaxation should change both together.
        db.register_host_function("plus1", 1, |a| match a {
            [Value::Int(x)] => Ok(Value::Int(x + 1)),
            _ => Err(Error::Unsupported("plus1 wants one int".into())),
        });
        assert!(db.query("SELECT mysum(id) + 1 FROM users", &[]).is_err());
        assert!(db.query("SELECT plus1(id) + 1 FROM users", &[]).is_err());
        // Both are fine on their own, and a host scalar INSIDE a host
        // aggregate's argument composes (two registries, one execution).
        assert_eq!(
            ints(db.query("SELECT mysum(plus1(id)) FROM users", &[]).unwrap()),
            vec![14_000]
        );

        // Plan-sharing bypass, exactly as for a host scalar: a fresh handle to
        // the same file cannot execute the plan by hash.
        let hash = db.prepare("SELECT mysum(id) FROM users").unwrap();
        let db2 = Database::open_from_file(_g.0.as_path()).unwrap();
        assert!(matches!(db2.execute(&hash, &[]), Err(Error::UnknownPlan(_))));
        assert_eq!(ints(db.execute(&hash, &[]).unwrap()), vec![10_000]);

        // A built-in name can never be shadowed by a registration.
        db.register_host_aggregate("sum", 1, || Box::<MySum>::default());
        assert_eq!(ints(db.query("SELECT sum(id) FROM users", &[]).unwrap()), vec![10]);

        // Unregister → unknown again.
        assert!(db.unregister_host_aggregate("mysum", 1));
        assert!(db.query("SELECT mysum(id) FROM users", &[]).is_err());
    }

    struct FileGuard(PathBuf);
    impl Drop for FileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn corrupt_registry_entries_degrade_to_unknown_plan() {
        let (cfg, path) = test_config("corrupt", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();
        let h = db.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a@x')",
            &params![],
        )
        .unwrap();

        // 1. Garbage bytes under the plan's own subkey.
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&h), b"total garbage, not a record")
            .unwrap();
        w.commit().unwrap();
        // Fresh handle (empty local cache) must hit the registry and reject.
        let db2 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db2.execute(&h, &params![1]),
            Err(Error::UnknownPlan(x)) if x == h
        ));

        // 2. Structurally valid record whose blob is a DIFFERENT plan
        //    (content-hash mismatch on load).
        let other = mpedb_sql::prepare("SELECT * FROM users WHERE id = $1 AND id > 0", &db.schema())
            .unwrap();
        assert_ne!(other.hash(), h);
        let rec = registry::encode_record("mismatch", &other.encode(), 1);
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&h), &rec).unwrap();
        w.commit().unwrap();
        let db3 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db3.execute(&h, &params![1]),
            Err(Error::UnknownPlan(x)) if x == h
        ));

        // 3. Blob compiled against a different schema → PlanInvalidated.
        let other_schema = Schema::new(vec![TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
                collation: mpedb_types::Collation::Binary,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
        }])
        .unwrap();
        let foreign = mpedb_sql::prepare("SELECT * FROM users WHERE id = $1", &other_schema).unwrap();
        let fh = foreign.hash();
        let rec = registry::encode_record("foreign", &foreign.encode(), 1);
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&fh), &rec).unwrap();
        w.commit().unwrap();
        let db4 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db4.execute(&fh, &params![1]),
            Err(Error::PlanInvalidated)
        ));

        // 4. A re-prepare on the corrupted hash overwrites and heals.
        let db5 = Database::open_with_config(cfg.clone()).unwrap();
        let h2 = db5.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        assert_eq!(h2, h);
        let db6 = Database::open_with_config(cfg).unwrap();
        assert!(matches!(
            db6.execute(&h, &params![1]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));

        db6.verify().unwrap();
    }

    #[test]
    fn decode_registry_plan_rejects_garbage_unit_level() {
        let (cfg, path) = test_config("decode-unit", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        let h = PlanHash([0x5A; 32]);
        for garbage in [
            &b""[..],
            &b"short"[..],
            &registry::encode_record("sql", b"not a plan blob", 3)[..],
        ] {
            assert!(matches!(
                decode_registry_plan(garbage, &h, &db.schema()),
                Err(Error::UnknownPlan(x)) if x == h
            ));
        }
    }

    #[test]
    fn sys_records_roundtrip_and_stay_clear_of_the_plan_registry() {
        let (cfg, path) = test_config("sysrec", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();

        // Namespace/key validation.
        assert!(db.sys_record_put("", b"k", b"v").is_err());
        assert!(db.sys_record_put("ns\0evil", b"k", b"v").is_err());
        assert!(db.sys_record_put(&"n".repeat(65), b"k", b"v").is_err());
        assert!(db.sys_record_put("ns", b"", b"v").is_err());
        assert!(db.sys_record_put("ns", &[7u8; 1025], b"v").is_err());
        assert!(db
            .sys_record_put("ns", b"k", &vec![0u8; SYS_RECORD_MAX_VALUE + 1])
            .is_err());
        assert!(db.sys_record_scan("").is_err());

        // Roundtrip, overwrite, scan with prefix stripping.
        db.sys_record_put("proc", b"transfer", b"blob-1").unwrap();
        db.sys_record_put("proc", b"transfer", b"blob-2").unwrap();
        db.sys_record_put("proc", b"other", b"blob-3").unwrap();
        db.sys_record_put("proch", b"\x01\x02", b"blob-2").unwrap();
        assert_eq!(
            db.sys_record_get("proc", b"transfer").unwrap().as_deref(),
            Some(&b"blob-2"[..])
        );
        assert_eq!(db.sys_record_get("proc", b"missing").unwrap(), None);
        let scan = db.sys_record_scan("proc").unwrap();
        assert_eq!(
            scan,
            vec![
                (b"other".to_vec(), b"blob-3".to_vec()),
                (b"transfer".to_vec(), b"blob-2".to_vec()),
            ]
        );
        // `proch` does not leak into a `proc` scan, nor vice versa.
        assert_eq!(db.sys_record_scan("proch").unwrap().len(), 1);

        // A plan whose registry subkey starts with the bytes `plan` must be
        // untouched by a `plan`-namespace record (0x00 vs '/' separator).
        let h = db.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        db.sys_record_put("plan", &h.0, b"not a plan record").unwrap();
        let db2 = Database::open_with_config(cfg).unwrap();
        assert!(db2.execute(&h, &params![1]).is_ok());
        db2.verify().unwrap();
    }

    #[test] // ~10 s in debug: fills the 4096-entry registry once
    fn registry_eviction_caps_entries() {
        let (cfg, path) = test_config("evict", 64);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let total = registry::MAX_REGISTRY_PLANS + 1;
        let mut hashes = Vec::with_capacity(total);
        for i in 0..total {
            hashes.push(
                db.prepare(&format!("SELECT * FROM users WHERE id = {i}"))
                    .unwrap(),
            );
        }

        // Inspect the registry through `db`'s OWN engine.
        //
        // This test used to open a SECOND `Database` on the same path here (the
        // obvious "a cold-cache handle reads the shared registry" check). That is
        // flaky: a second attach opens the file BY PATH, and `Shm::open` passes
        // `create(true)`, so if anything removes the /dev/shm file between the
        // fill and the second open — a concurrent process sweeping the shared
        // tmpfs, another test run, disk pressure — the second handle silently
        // CREATES AND FORMATS A FRESH, EMPTY database at that path and then
        // observes zero plans, and the "survivors still resolve" assertions blow
        // up even though eviction was perfectly correct. `db` already holds the
        // mmap of the real inode, so reading through `db.engine` is immune to any
        // such path churn. (The cross-handle "a fresh Database can load a plan
        // from the shared registry" behaviour is covered independently by
        // `sys_records_roundtrip_and_stay_clear_of_the_plan_registry`.)
        let r = db.engine.begin_read().unwrap();
        let present: std::collections::HashSet<Vec<u8>> = r
            .sys_scan()
            .unwrap()
            .into_iter()
            .filter(|(k, _)| k.starts_with(registry::PLAN_PREFIX))
            .map(|(k, _)| k)
            .collect();
        let n_plans = present.len();

        // EXACT count, not a range. This is a single writer that inserts exactly
        // one plan per commit, so the registry grows 0,1,2,…,MAX and
        // `evict_if_full` (registry.rs) trips exactly ONCE — at the (MAX+1)-th
        // insert, when the tree holds precisely MAX entries — dropping
        // EVICT_BATCH of them before adding the last, leaving MAX-EVICT_BATCH+1.
        // (A post-eviction *range* only arises with concurrent writers that can
        // push the count past MAX between eviction checks; there are none here,
        // so the deterministic value is the correct invariant — do not weaken it.)
        assert_eq!(
            n_plans,
            registry::MAX_REGISTRY_PLANS - registry::EVICT_BATCH + 1
        );

        // Eviction drops the OLDEST EVICT_BATCH by last_used_txn. Here
        // last_used_txn is strictly monotonic with prepare order (one commit per
        // prepare, txn_id += 1 each; verified: all stamps distinct), so the
        // evicted set is EXACTLY the first EVICT_BATCH prepared and the survivors
        // are EXACTLY the rest — a fixed partition with no sort tie-break.
        for &i in &[0usize, 1, registry::EVICT_BATCH - 1] {
            assert!(
                !present.contains(&plan_subkey(&hashes[i])),
                "plan #{i} (among the oldest {}) should have been evicted",
                registry::EVICT_BATCH
            );
        }
        // ...and the survivors still load through the COLD registry path —
        // `decode_registry_plan` is exactly what a cache-miss `execute` runs:
        // parse the record, re-validate the blob against the live schema, and
        // verify the recomputed content hash.
        let schema = db.schema();
        for &i in &[registry::EVICT_BATCH, registry::EVICT_BATCH + 1, total - 1] {
            let key = plan_subkey(&hashes[i]);
            assert!(present.contains(&key), "plan #{i} should have survived");
            let rec = r.sys_get(&key).unwrap().expect("survivor record present");
            decode_registry_plan(&rec, &hashes[i], &schema)
                .unwrap_or_else(|e| panic!("survivor plan #{i} must cold-load, got {e:?}"));
        }
        // An evicted hash leaves no record, so the cold path reports UnknownPlan.
        assert!(r.sys_get(&plan_subkey(&hashes[0])).unwrap().is_none());
        r.finish().unwrap();
        db.verify().unwrap();
    }

    // -------------------------------------------- detached (client-borne) plans

    #[test]
    fn detached_plan_encode_decode_roundtrip_and_truncation() {
        let dp = DetachedPlan {
            hash: PlanHash([0x37; 32]),
            blob: b"a compiled plan blob \x00\xff".to_vec(),
            sql: "SELECT * FROM users WHERE id = $1".to_owned(),
        };
        let bytes = dp.encode();
        assert_eq!(DetachedPlan::decode(&bytes).unwrap(), dp);

        // Every truncation fails cleanly (Corrupt), never panics.
        for cut in 0..bytes.len() {
            assert!(
                matches!(DetachedPlan::decode(&bytes[..cut]), Err(Error::Corrupt(_))),
                "truncation at {cut} must be Corrupt"
            );
        }
        // Trailing garbage and a bad format byte are rejected too.
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(DetachedPlan::decode(&trailing), Err(Error::Corrupt(_))));
        let mut bad_fmt = bytes.clone();
        bad_fmt[0] = 9;
        assert!(matches!(DetachedPlan::decode(&bad_fmt), Err(Error::Corrupt(_))));
        // Invalid utf-8 in the sql field.
        let mut bad_sql = DetachedPlan {
            hash: PlanHash([1; 32]),
            blob: vec![],
            sql: String::new(),
        }
        .encode();
        // append u32 sql_len = 1 then a lone 0xff (rewrite trailing sql section)
        bad_sql.truncate(1 + 32 + 4); // fmt + hash + blob_len(=0)
        bad_sql.extend_from_slice(&1u32.to_le_bytes());
        bad_sql.push(0xff);
        assert!(matches!(DetachedPlan::decode(&bad_sql), Err(Error::Corrupt(_))));
    }

    /// A detached plan from an OLDER binary (a different PLAN_FORMAT byte) is
    /// version drift, not tampering: the client must get `PlanInvalidated` —
    /// the documented re-prepare-from-`DetachedPlan::sql` path — not a
    /// `Corrupt` that reads as "your blob was forged". Pinned after the 7→8
    /// bump surfaced exactly this (adversarial review find).
    #[test]
    fn detached_plan_from_old_format_is_invalidated_not_corrupt() {
        let (cfg, path) = test_config("detached-format", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        let mut dp = db.prepare_detached("SELECT * FROM users WHERE id = $1").unwrap();
        // The format byte is byte 0 of the canonical encoding; regress it.
        assert_ne!(dp.blob[0], 7, "bump the test constant when the format moves");
        dp.blob[0] = 7;
        assert!(
            matches!(db.execute_detached(&dp, &params![1]), Err(Error::PlanInvalidated)),
            "an old-format blob must invalidate, not report corruption"
        );
    }

    #[test]
    fn detached_hash_matches_prepare_and_runs_without_registry() {
        let (cfg, path) = test_config("detached-basic", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        // prepare_detached hash == prepare() hash (same plan, same schema).
        let published = db.prepare(sql).unwrap();
        let dp = db.prepare_detached(sql).unwrap();
        assert_eq!(dp.hash, published);
        assert_eq!(dp.sql, sql);

        // A DML detached plan that was NEVER prepared/published.
        let ins = db
            .prepare_detached("INSERT INTO users (id, email) VALUES ($1, $2)")
            .unwrap();
        // Fresh handle (empty local cache) on the same file: the plan has no
        // registry entry, yet execute_detached runs it.
        let db2 = Database::open_with_config(cfg.clone()).unwrap();
        // Prove absence from the shared registry directly. Only `ins` is
        // detached-only; `dp`'s SQL was also published above via prepare() for
        // the hash-equality check, so it legitimately IS in the registry.
        {
            let r = db2.engine.begin_read().unwrap();
            let rec = r.sys_get(&plan_subkey(&ins.hash)).unwrap();
            r.finish().unwrap();
            assert!(rec.is_none(), "detached plan leaked into the registry");
        }
        // ...and execute(hash) (registry path) genuinely can't find it.
        assert!(matches!(
            db2.execute(&ins.hash, &params![9, "z@x"]),
            Err(Error::UnknownPlan(_))
        ));

        // execute_detached runs the DML and the SELECT correctly.
        assert_eq!(
            db2.execute_detached(&ins, &params![9, "z@x"]).unwrap(),
            ExecResult::Affected(1)
        );
        let sel = db2.prepare_detached(sql).unwrap();
        assert!(matches!(
            db2.execute_detached(&sel, &params![9]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        // Still not in the registry after executing.
        let r = db2.engine.begin_read().unwrap();
        assert!(r.sys_get(&plan_subkey(&ins.hash)).unwrap().is_none());
        r.finish().unwrap();
        db2.verify().unwrap();
    }

    #[test]
    fn detached_tampered_blob_or_hash_is_corrupt() {
        let (cfg, path) = test_config("detached-tamper", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query("INSERT INTO users (id, email) VALUES (1, 'a@x')", &params![])
            .unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        let good = db.prepare_detached(sql).unwrap();
        assert!(db.execute_detached(&good, &params![1]).is_ok());

        // (a) Flip a byte in the blob PAST the schema_hash region (bytes 1..33)
        // so this is a genuine content tamper, not a schema-hash edit: it must
        // be Corrupt (structural decode failure or hash mismatch), never a
        // silent wrong execution.
        let mut tampered = good.clone();
        let last = tampered.blob.len() - 1;
        assert!(last >= 33);
        tampered.blob[last] ^= 1;
        assert!(matches!(
            db.execute_detached(&tampered, &params![1]),
            Err(Error::Corrupt(_))
        ));

        // (b) A blob that decodes cleanly but is paired with the WRONG hash
        // exercises the separate integrity check → Corrupt (not PlanInvalidated,
        // not UnknownPlan).
        let mut wrong_hash = good.clone();
        wrong_hash.hash.0[0] ^= 0xFF;
        assert!(matches!(
            db.execute_detached(&wrong_hash, &params![1]),
            Err(Error::Corrupt(m)) if m.contains("does not match its carried hash")
        ));
        db.verify().unwrap();
    }

    #[test]
    fn detached_schema_change_returns_plan_invalidated_and_reprepare_recovers() {
        let (cfg, path) = test_config("detached-schema", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query("INSERT INTO users (id, email) VALUES (5, 'e@x')", &params![])
            .unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        // A plan compiled against a DIFFERENT schema (users has only `id`), so
        // its embedded schema_hash will not match the live one — exactly what a
        // client holding a stale detached plan across a schema migration has.
        let foreign_schema = Schema::new(vec![TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
                collation: mpedb_types::Collation::Binary,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
        }])
        .unwrap();
        let foreign = mpedb_sql::prepare(sql, &foreign_schema).unwrap();
        let stale = DetachedPlan {
            hash: foreign.hash(),
            blob: foreign.encode(),
            sql: sql.to_owned(),
        };
        assert!(matches!(
            db.execute_detached(&stale, &params![5]),
            Err(Error::PlanInvalidated)
        ));

        // Recovery: re-prepare from the carried sql against the live schema.
        let fresh = db.prepare_detached(&stale.sql).unwrap();
        assert_ne!(fresh.hash, stale.hash);
        assert!(matches!(
            db.execute_detached(&fresh, &params![5]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        db.verify().unwrap();
    }

    /// **Cross-table atomic commit** — DESIGN-MULTIDB §21 lists it as a headline
    /// property of the single-file option ("one writer lock, one meta flip"),
    /// and nothing tested it. Every write test in this repo touched exactly one
    /// table, so an advertised guarantee rested on the reader believing the
    /// architecture rather than on evidence.
    ///
    /// It follows from the design (a commit is one atomic meta flip regardless
    /// of how many trees the txn dirtied) — but "it follows" is what people say
    /// right before a regression. This pins both halves: both tables land
    /// together on commit, and NEITHER lands on rollback.
    #[test]
    fn one_transaction_writing_two_tables_is_atomic() {
        let (cfg, path) = two_table_config("xtable");
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let count = |t: &str| -> usize {
            match db.query(&format!("SELECT id FROM {t}"), &params![]).unwrap() {
                ExecResult::Rows { rows, .. } => rows.len(),
                o => panic!("{o:?}"),
            }
        };

        // Rollback: neither table may keep its row.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(1), Value::Int(10)]).unwrap();
        s.insert_row(1, &[Value::Int(1), Value::Int(20)]).unwrap();
        s.rollback();
        assert_eq!((count("a"), count("b")), (0, 0), "rollback must undo BOTH tables");

        // Commit: both land, in one meta flip.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(1), Value::Int(10)]).unwrap();
        s.insert_row(1, &[Value::Int(1), Value::Int(20)]).unwrap();
        s.commit().unwrap();
        assert_eq!((count("a"), count("b")), (1, 1), "commit must land BOTH tables");

        // And a failure on the SECOND table must not leave the first behind —
        // the case that would make "atomic" a lie.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(2), Value::Int(11)]).unwrap();
        let dup = s.insert_row(1, &[Value::Int(1), Value::Int(99)]); // PK 1 exists
        assert!(dup.is_err(), "expected a PK violation on table b");
        s.rollback();
        assert_eq!(
            (count("a"), count("b")),
            (1, 1),
            "a failure on the second table must not leave the first table's row"
        );
    }
}
