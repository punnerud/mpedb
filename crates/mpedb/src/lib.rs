//! mpedb — embedded, multi-process, shared-memory database.
//!
//! This is the user-facing facade: it compiles SQL **once** into
//! content-hashed plans ([`Database::prepare`]), executes plans by hash with
//! no parsing on the hot path ([`Database::execute`]), and maintains the
//! shared plan registry *inside* the database so any attached process can
//! `execute(hash, params)` for a plan it never prepared (DESIGN.md §7.2).
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
mod ring_exec;
mod session;
mod shard;
mod sqlite_attach;
mod sqlite_overlay;
mod stream;
mod workspace;

pub use session::Session;
pub use shard::ShardSet;
pub use sqlite_attach::SqliteAttach;
pub use sqlite_overlay::SqliteOverlay;
#[cfg(feature = "sqlite-checkpoint")]
pub use sqlite_overlay::CheckpointReport;
pub use stream::RowStream;
pub use workspace::{Workspace, WorkspaceTxn, WsPlan};

pub use mpedb_types::{
    ColumnDef, ColumnType, Config, DbOptions, Durability, Error, PlanHash, PolicyCmd, PolicyDef,
    Result, Schema, TableDef, Value,
};

use exec::{exec_stmt, ReadCtx};
use mpedb_core::{CheckPrograms, Engine, WriteTxn};
use mpedb_sql::{CompiledPlan, PlanStmt};
use registry::{decode_registry_plan, patched_last_used, plan_subkey};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

const POISON: &str = "plan cache lock poisoned";

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
/// DESIGN.md §7.2 turned inside-out). Shipping `(blob + hash + sql)` between
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
    /// The database file path this handle attached (for `Workspace` dup-file
    /// detection and diagnostics).
    path: std::path::PathBuf,
    /// Table ids this process declared `require_policy = true` for
    /// (DESIGN-MULTIDB §6.3). Resolved from names ONCE at open — so a typo or a
    /// renamed table fails immediately and loudly, rather than silently
    /// asserting nothing for the rest of the deployment's life.
    require_policy: std::collections::HashSet<u32>,
}

impl Database {
    /// Open (or create) the database described by a TOML config file.
    pub fn open(config_path: &Path) -> Result<Database> {
        Database::open_with_config(Config::from_file(config_path)?)
    }

    /// Attach an existing database file config-free, reading its stored schema
    /// and geometry (the file is schema-authoritative). Used by tooling — the
    /// mirror daemon/CLI, `dump`, etc. — that must open a file it did not create
    /// a TOML for. CHECK programs are NOT reconstructed (a file that carries
    /// CHECK constraints must be opened via a config for enforcement); durability
    /// = `async` also needs a config (no background flusher). Mirror files use
    /// neither, so this is exactly what they need.
    pub fn open_from_file(path: &Path) -> Result<Database> {
        let engine = Engine::open_from_file(path)?;
        Ok(Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            path: path.to_path_buf(),
            // No config, so no §6.3 assertions — consistent with this
            // constructor's contract (it also skips CHECK programs): a
            // config-free attach enforces what the FILE carries, and
            // `require_policy` is a config-declared deployment assertion.
            require_policy: std::collections::HashSet::new(),
        })
    }

    /// Open (or create) the database described by an already-parsed config.
    /// Compiles every column CHECK expression against its table and hands the
    /// programs to the engine, so constraint enforcement is identical in
    /// every attached process.
    pub fn open_with_config(config: Config) -> Result<Database> {
        let mut checks: CheckPrograms = Vec::with_capacity(config.schema.tables.len());
        for table in &config.schema.tables {
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
        let path = config.options.path.clone();
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
        Ok(Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            path,
            require_policy,
        })
    }

    pub fn schema(&self) -> &Schema {
        self.engine.schema()
    }

    /// The database file path this handle attached.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Compile `sql` with this database's RLS policies injected (loaded from the
    /// catalog sys-keyspace on a pinned read snapshot, DESIGN-MULTIDB.md §3).
    /// The bool is the `EXPLAIN` flag. An empty policy set behaves exactly as
    /// plain compilation.
    fn compile_maybe_explain(&self, sql: &str) -> Result<(CompiledPlan, bool)> {
        let catalog = self.load_policy_catalog()?;
        mpedb_sql::prepare_maybe_explain_with_policies(sql, self.schema(), &catalog)
    }

    /// Apply a parsed RLS DDL statement to the catalog (autocommit — each takes
    /// the writer lock once and bumps the table's policy epoch). Returns
    /// `Affected(0)`; RLS DDL touches no user rows.
    fn apply_ddl(&self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::{DdlStmt, RlsAction};
        match ddl {
            DdlStmt::CreatePolicy(spec) => {
                let def = mpedb_types::PolicyDef {
                    name: spec.name,
                    command: spec.command,
                    permissive: spec.permissive,
                    using_src: spec.using_src,
                    check_src: spec.check_src,
                };
                // Lint BEFORE creating, but never block on it (§6.4): a leaky
                // unique key is a design smell the author may have accepted, not
                // something the database gets to veto. Findings come back as rows
                // so they print through the ordinary result path — a lint nobody
                // sees is worthless, and a library must not print for its caller.
                let findings = self.lint_policy(&spec.table, &def)?;
                self.create_policy(&spec.table, &def)?;
                if !findings.is_empty() {
                    return Ok(ExecResult::Rows {
                        columns: vec!["warning".into()],
                        rows: findings.into_iter().map(|w| vec![Value::Text(w)]).collect(),
                    });
                }
            }
            DdlStmt::DropPolicy { table, name } => {
                self.drop_policy(&table, &name)?;
            }
            DdlStmt::AlterRls { table, action } => match action {
                RlsAction::Enable { force } => self.enable_rls(&table, force)?,
                RlsAction::Disable => self.disable_rls(&table)?,
            },
        }
        Ok(ExecResult::Affected(0))
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
    /// the plan's `current_setting()` references (DESIGN-MULTIDB.md §2). `params`
    /// are the caller-facing parameters only; the reserved context slots are
    /// filled from `session` (fail-closed on a missing key / NULL / wrong type).
    pub fn execute_ctx(
        &self,
        session: &Session,
        hash: &PlanHash,
        params: &[Value],
    ) -> Result<ExecResult> {
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
    /// (DESIGN-MULTIDB.md §2). `params` are the caller-facing parameters only.
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
            return Ok(ExecResult::Explain(plan.explain(self.schema())));
        }
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
        let compiled = match CompiledPlan::decode(&plan.blob, self.schema()) {
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
    /// semantics, DESIGN-MULTIDB.md §2.5): every statement in the transaction
    /// resolves `current_setting()` against the context as it was *here*, so a
    /// later mutation of the caller's `Session` cannot bleed into an open
    /// transaction. The context is fixed for the transaction's lifetime.
    pub fn begin_as(&self, session: &Session) -> Result<WriteSession<'_>> {
        Ok(WriteSession {
            db: self,
            txn: self.engine.begin_write()?,
            session: session.clone(),
            poisoned: false,
        })
    }

    /// Verify the engine's page-accounting invariant (DESIGN.md §4.5).
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
        let t = self.schema().table(table_id).expect("id from table_id");
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
    /// live writer (DESIGN.md §7.3: read-only plans route to a read
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
        let plan = Arc::new(decode_registry_plan(&record, hash, self.schema())?);
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
        if plan.footprint.read_only {
            // Reads never touch the writer lock or the ring.
            let mut partial = false;
            let r = self.engine.begin_read()?;
            // Staleness check UNDER THE SAME PIN that scans the rows (§4.3):
            // a policy edit that landed since compile invalidates the plan.
            // On error `r` drops here, releasing the reader slot.
            self.validate_policy_read(hash, plan, &r)?;
            let res = {
                let mut ctx = ReadCtx(&r);
                exec_stmt(&mut ctx, self.schema(), plan, params, &mut partial)
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

    /// Autocommit DML with Phase-2 group commit (DESIGN.md §5.3).
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
        let use_ring = ring_exec::ring_enabled(self);
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
        let schema = self.db.schema();
        // Policies are read from the committed catalog snapshot (a policy
        // created inside *this* uncommitted session is not yet visible here).
        let (plan, is_explain) = self.db.compile_maybe_explain(sql)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(schema)));
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
        let plan = Arc::new(decode_registry_plan(&record, hash, self.db.schema())?);
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
        if matches!(
            plan.stmt,
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback
        ) {
            return Err(Error::Unsupported(
                "the session already is a transaction; \
                 use WriteSession::commit()/rollback()"
                    .into(),
            ));
        }
        // Staleness check under this session's own write txn (holds the writer
        // lock, so no policy edit can race it). Local-cache plans only, so no
        // shared-registry eviction is needed here.
        self.db.validate_policy_write(None, plan, &mut self.txn)?;
        let full = session::resolve_params(plan, params, &self.session)?;
        let mut partial = false;
        let res = exec_stmt(&mut self.txn, self.db.schema(), plan, &full, &mut partial);
        if res.is_err() && partial {
            // The failed statement may have applied part of its effects; the
            // transaction no longer reflects whole statements. See the
            // poisoning contract in the WriteSession docs.
            self.poisoned = true;
        }
        res
    }
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
        let other = mpedb_sql::prepare("SELECT * FROM users WHERE id = $1 AND id > 0", db.schema())
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
            name: "users".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
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
                decode_registry_plan(garbage, &h, db.schema()),
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
        let db = Database::open_with_config(cfg.clone()).unwrap();

        let total = registry::MAX_REGISTRY_PLANS + 1;
        let mut hashes = Vec::with_capacity(total);
        for i in 0..total {
            hashes.push(
                db.prepare(&format!("SELECT * FROM users WHERE id = {i}"))
                    .unwrap(),
            );
        }

        // Count registry entries directly.
        let r = db.engine.begin_read().unwrap();
        let n_plans = r
            .sys_scan()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(registry::PLAN_PREFIX))
            .count();
        r.finish().unwrap();
        assert_eq!(
            n_plans,
            registry::MAX_REGISTRY_PLANS - registry::EVICT_BATCH + 1
        );

        // The oldest plans were evicted; a fresh handle cannot load them...
        let db2 = Database::open_with_config(cfg).unwrap();
        assert!(matches!(
            db2.execute(&hashes[0], &params![]),
            Err(Error::UnknownPlan(_))
        ));
        // ...but younger ones and the newest still resolve.
        assert!(db2.execute(&hashes[registry::EVICT_BATCH], &params![]).is_ok());
        assert!(db2.execute(&hashes[total - 1], &params![]).is_ok());
        db2.verify().unwrap();
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
            name: "users".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
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
