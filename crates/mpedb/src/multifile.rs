//! Cross-file SELECT — sqlite `ATTACH DATABASE` compatibility (#51).
//!
//! A [`Database`] handle carries a connection-local attach list: `ATTACH
//! DATABASE 'other.mpedb' AS other` opens a second, fully independent engine
//! (config-free, file-authoritative — [`Database::open_from_file`]) and
//! registers it under a name; `SELECT … FROM main.t JOIN other.u` then reads
//! across the files. The mechanism is deliberately layered ABOVE everything
//! reviewed:
//!
//! - **Name resolution** happens before the parser: `mpedb_sql::resolve_db_refs`
//!   rewrites the statement so attached tables appear under mangled
//!   `"db.table"` names (see `mpedb-sql/src/dbref.rs`).
//! - **Compilation** happens against a MERGED, throwaway [`Schema`]: main's
//!   live tables under their own names plus each referenced attached table
//!   under its mangled name. `Schema::new` re-sorts and renumbers — the
//!   merged ids are private to this plan; a side map translates merged id →
//!   `(member, member-local id)`.
//! - **Execution** pins one independent `ReadTxn` per involved file and
//!   dispatches every `TxnCtx` row operation through the map ([`MultiCtx`]).
//!   Snapshot semantics are therefore **per-file-consistent, not globally
//!   serializable** — each member is read at its own MVCC snapshot, and a
//!   writer committing between the pins can be seen in one file and not the
//!   other (sqlite's attached databases behave the same way in WAL mode;
//!   documented in COMPAT.md).
//!
//! **Cross-file plans are connection-local**, exactly like host-UDF plans:
//! never published to the shared `plan/<hash>` registry (their table ids are
//! meaningless outside this handle's attach list), never encoded/decoded, so
//! the plan wire format and the footprint's per-file `u64` table domain are
//! untouched — no PLAN_FORMAT bump. `prepare` keeps them in a private
//! `cross_cache`; ATTACH/DETACH bumps the attach epoch and drops it.
//!
//! **Cross-file reads** are the main path. **Pure attached-only writes/DDL**
//! (every table in the statement lives on one attached member) forward to that
//! member's handle — including `ATTACH ':memory:'` + schema-qualified
//! `CREATE`/`INSERT` (CPython `test_database_source_name`). **Mixed**
//! main+attached writes, cross-file statements inside an open [`WriteSession`],
//! RLS policies on any involved member, ATTACHing a missing file, and
//! bound-parameter ATTACH paths are refused BY NAME.

use crate::exec::{exec_stmt, ChargeMode, ReadCtx, TxnCtx};
use crate::{Database, ExecResult, Session, POISON};
use mpedb_core::ReadTxn;
use mpedb_sql::{
    AttachStmt, CompiledPlan, DbResolution, DbScope, PlanHash, PolicyCatalog, SortDir,
};
use mpedb_types::{Config, Error, ExprProgram, HostFns, Result, Schema, TableDef, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static ATTACH_EPHEMERAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// `ATTACH ':memory:'` — a fresh empty mpedb file on `/dev/shm` (or temp),
/// unlinked on DETACH. Seed is a one-column dummy table the live DDL path can
/// grow past; CPython's backup of the attached schema only needs tables the
/// test creates.
fn open_ephemeral_attach() -> Result<Database> {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let seq = ATTACH_EPHEMERAL_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "mpedb-attach-mem-{}-{}.mpedb",
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_file(&path);
    let p = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let toml = format!(
        "[database]\npath = \"{p}\"\nsize_mb = 16\nmax_readers = 32\n\n\
         [[table]]\nname = \"attach_seed\"\nprimary_key = [\"id\"]\n\n\
           [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n"
    );
    Database::open_with_config(Config::from_toml_str(&toml)?)
}

/// sqlite's default `SQLITE_MAX_ATTACHED`.
const MAX_ATTACHED: usize = 10;

/// One attached member: the connection-local name and its own engine.
pub(crate) struct AttachedMember {
    pub name: String,
    pub db: Database,
    /// Created by `ATTACH ':memory:'` — unlink the backing file on DETACH.
    pub ephemeral: bool,
}

/// The connection-local attach list. `epoch` bumps on every ATTACH/DETACH so
/// cached cross plans (whose member slots index into a specific list) fail
/// closed with `PlanInvalidated` instead of reading the wrong file.
#[derive(Default)]
pub(crate) struct AttachState {
    pub members: Vec<AttachedMember>,
    pub epoch: u64,
}

impl AttachState {
    pub(crate) fn find(&self, name: &str) -> Option<usize> {
        self.members
            .iter()
            .position(|m| m.name.eq_ignore_ascii_case(name))
    }
}

/// A compiled cross-file plan — connection-local, never published.
pub(crate) struct CrossPlan {
    pub plan: Arc<CompiledPlan>,
    /// The merged schema the plan was compiled against (its table ids are the
    /// plan's ids; meaningless outside this plan).
    pub schema: Arc<Schema>,
    /// merged table id → (ctx slot, member-local table id); slot 0 = main,
    /// slot k = `member_names[k-1]`.
    pub map: Vec<(usize, u32)>,
    /// Involved attached members, in ctx-slot order (slot = index + 1).
    pub member_names: Vec<String>,
    /// `schema_gen` per slot (0 = main) at compile; re-checked under each
    /// execution's pins so member DDL invalidates instead of misreading.
    pub gens: Vec<u64>,
    /// The attach epoch this plan was compiled at.
    pub epoch: u64,
}

/// How a statement routes after database-name resolution.
pub(crate) enum DbRoute {
    /// Untouched: the ordinary single-file path.
    Passthrough,
    /// Rewritten (a `main.` qualifier was stripped) but still single-file.
    Main(String),
    /// References attached tables: compile+run on the cross path.
    Cross {
        sql: String,
        tables: Vec<(String, String)>,
    },
    /// A pure write/DDL on exactly one attached member — run on that handle.
    AttachedOnly {
        db: String,
        sql: String,
    },
}

impl Database {
    /// True when this handle has any `ATTACH`ed databases (the C-API shim
    /// defers statement validation to execution in that case, since only the
    /// execution path resolves cross-database names).
    pub fn has_attached_databases(&self) -> bool {
        !self.attached.read().expect(POISON).members.is_empty()
    }

    /// The attach list as `(name, file path)` in attach order — the shim's
    /// `PRAGMA database_list` source (main itself is seq 0 there).
    pub fn attached_databases(&self) -> Vec<(String, std::path::PathBuf)> {
        self.attached
            .read()
            .expect(POISON)
            .members
            .iter()
            .map(|m| (m.name.clone(), m.db.path().to_path_buf()))
            .collect()
    }

    /// Intercept `ATTACH`/`DETACH`; `None` means "not an attach statement".
    pub(crate) fn attach_stmt_hook(&self, sql: &str) -> Result<Option<ExecResult>> {
        match mpedb_sql::parse_attach(sql)? {
            None => Ok(None),
            Some(st) => self.exec_attach(st).map(Some),
        }
    }

    fn exec_attach(&self, st: AttachStmt) -> Result<ExecResult> {
        match st {
            AttachStmt::Attach { path, name } => {
                if name.is_empty() || name.contains('.') {
                    return Err(Error::Bind(
                        "attach name must be non-empty and contain no '.'".into(),
                    ));
                }
                if name.eq_ignore_ascii_case("main") || name.eq_ignore_ascii_case("temp") {
                    // sqlite: `database main is already in use`.
                    return Err(Error::Bind(format!("database {name} is already in use")));
                }
                if path.starts_with("file:") && path != "file::memory:" {
                    return Err(Error::Unsupported(
                        "ATTACH of URI databases is not supported; \
                         attach an existing .mpedb file by path, or ':memory:'"
                            .into(),
                    ));
                }
                let mut guard = self.attached.write().expect(POISON);
                if guard.find(&name).is_some() {
                    return Err(Error::Bind(format!("database {name} is already in use")));
                }
                if guard.members.len() >= MAX_ATTACHED {
                    // sqlite's message, verbatim.
                    return Err(Error::Unsupported(
                        "too many attached databases - max 10".into(),
                    ));
                }
                let (db, ephemeral) = if path == ":memory:" || path == "file::memory:" || path.is_empty()
                {
                    let db = open_ephemeral_attach()?;
                    (db, true)
                } else {
                    let p = std::path::Path::new(&path);
                    if !p.exists() {
                        return Err(Error::Unsupported(format!(
                            "cannot ATTACH `{path}`: file does not exist (mpedb v1 \
                             does not create databases on ATTACH; open it once with \
                             a config first)"
                        )));
                    }
                    (Database::open_from_file(p)?, false)
                };
                guard.members.push(AttachedMember {
                    name,
                    db,
                    ephemeral,
                });
                guard.epoch += 1;
                drop(guard);
                self.cross_cache.write().expect(POISON).clear();
                self.cross_cache_live
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                Ok(ExecResult::Affected(0))
            }
            AttachStmt::Detach { name } => {
                if name.eq_ignore_ascii_case("main") || name.eq_ignore_ascii_case("temp") {
                    return Err(Error::Bind(format!("cannot detach database {name}")));
                }
                let mut guard = self.attached.write().expect(POISON);
                match guard.find(&name) {
                    Some(i) => {
                        let member = guard.members.remove(i);
                        guard.epoch += 1;
                        drop(guard);
                        self.cross_cache.write().expect(POISON).clear();
                        self.cross_cache_live
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        if member.ephemeral {
                            let path = member.db.path().to_path_buf();
                            drop(member);
                            let _ = std::fs::remove_file(&path);
                            let _ = std::fs::remove_file(format!("{}-wal", path.display()));
                        }
                        Ok(ExecResult::Affected(0))
                    }
                    None => Err(Error::Bind(format!("no such database: {name}"))),
                }
            }
        }
    }

    /// Resolve database-qualified names. The fast path — no attachments and
    /// no `.` anywhere — is a single `contains` check.
    pub(crate) fn resolve_db_refs_hook(&self, sql: &str) -> Result<DbRoute> {
        let guard = self.attached.read().expect(POISON);
        if guard.members.is_empty() && !sql.contains('.') {
            return Ok(DbRoute::Passthrough);
        }
        let scope = if guard.members.is_empty() {
            // Only `main.` stripping is possible; bare names never consult
            // the (empty) attach list, so the name sets are not needed.
            DbScope::default()
        } else {
            self.build_scope(&guard)?
        };
        match mpedb_sql::resolve_db_refs(sql, &scope)? {
            DbResolution::MainOnly(s) => {
                if s == sql {
                    Ok(DbRoute::Passthrough)
                } else {
                    Ok(DbRoute::Main(s))
                }
            }
            DbResolution::Cross { sql, tables } => Ok(DbRoute::Cross { sql, tables }),
            DbResolution::AttachedOnly { db, sql } => Ok(DbRoute::AttachedOnly { db, sql }),
        }
    }

    /// Run a pure attached-only statement on the named member.
    pub(crate) fn query_attached_only(
        &self,
        db: &str,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        let guard = self.attached.read().expect(POISON);
        let i = guard
            .find(db)
            .ok_or_else(|| Error::Bind(format!("no such database: {db}")))?;
        // Clone Arc? Database is not Arc - we need to call query while holding
        // the read lock. Database::query takes &self, so this is fine.
        guard.members[i].db.query(sql, params)
    }

    fn build_scope(&self, guard: &AttachState) -> Result<DbScope> {
        // Bundles refresh lazily (txn-begin reload); resolution needs the
        // LIVE name sets — one newest_meta read per engine, reload only on
        // an actual DDL gen change.
        self.engine.refresh_schema_if_stale()?;
        for m in &guard.members {
            m.db.engine.refresh_schema_if_stale()?;
        }
        let mut main: HashSet<String> = self
            .schema()
            .tables
            .iter()
            .filter(|t| !t.dead)
            .map(|t| t.name.clone())
            .collect();
        for name in self.load_view_catalog()?.keys() {
            main.insert(name.clone());
        }
        let attached = guard
            .members
            .iter()
            .map(|m| {
                let names = m
                    .db
                    .schema()
                    .tables
                    .iter()
                    .filter(|t| !t.dead)
                    .map(|t| t.name.clone())
                    .collect();
                (m.name.clone(), names)
            })
            .collect();
        Ok(DbScope { main, attached })
    }

    /// Compile a rewritten cross-file statement against the merged schema.
    pub(crate) fn compile_cross(
        &self,
        guard: &AttachState,
        sql: &str,
        tables: &[(String, String)],
    ) -> Result<(CrossPlan, bool)> {
        // RLS policies are per-file state validated per-plan against per-file
        // snapshots — none of which the merged-id plan can represent. Refuse
        // by name rather than silently not enforcing anyone's policies.
        if !self.require_policy.is_empty() || !self.load_policy_catalog()?.is_empty() {
            return Err(Error::Unsupported(
                "cross-file SELECT is not supported on a database with RLS \
                 policies (v1)"
                    .into(),
            ));
        }
        // The recorded gens are re-checked under each execution's pins; they
        // must be the LIVE gens (a lazily-loaded bundle still carries the gen
        // it was loaded at — e.g. 0 right after open — and would false-alarm
        // `PlanInvalidated` on the first execute).
        self.engine.refresh_schema_if_stale()?;
        for m in &guard.members {
            m.db.engine.refresh_schema_if_stale()?;
        }
        let main_bundle = self.schema();
        let mut defs: Vec<TableDef> = main_bundle
            .tables
            .iter()
            .filter(|t| !t.dead)
            .cloned()
            .collect();
        let main_names: HashSet<String> = defs.iter().map(|t| t.name.clone()).collect();

        // Involved members, first-use order; slot k = index + 1.
        let mut member_names: Vec<String> = Vec::new();
        let mut gens: Vec<u64> = vec![main_bundle.schema_gen];
        let mut mangled: Vec<(String, usize, u32)> = Vec::new(); // (name, slot, local id)
        for (db, table) in tables {
            let mi = guard.find(db).ok_or_else(|| {
                Error::Bind(format!("no such table: {db}.{table}"))
            })?;
            let member = &guard.members[mi];
            let slot = match member_names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&member.name))
            {
                Some(k) => k + 1,
                None => {
                    let bundle = member.db.schema();
                    if !member.db.load_policy_catalog()?.is_empty() {
                        return Err(Error::Unsupported(format!(
                            "cross-file SELECT from attached database `{}` is \
                             not supported: it declares RLS policies (v1)",
                            member.name
                        )));
                    }
                    member_names.push(member.name.clone());
                    gens.push(bundle.schema_gen);
                    member_names.len()
                }
            };
            let bundle = member.db.schema();
            let def = bundle
                .tables
                .iter()
                .find(|t| !t.dead && mpedb_types::ident_eq(&t.name, table))
                .ok_or_else(|| Error::Bind(format!("no such table: {db}.{table}")))?;
            let mangled_name = mpedb_sql::mangle_db_table(&member.name, table);
            if main_names.contains(&mangled_name) {
                return Err(Error::Unsupported(format!(
                    "a main table is literally named `{mangled_name}`, which \
                     collides with the attached reference; rename one"
                )));
            }
            let mut def = def.clone();
            let local_id = def.id;
            def.name = mangled_name.clone();
            defs.push(def);
            mangled.push((mangled_name, slot, local_id));
        }

        let merged = Schema::new(defs).map_err(|e| {
            Error::Unsupported(format!(
                "cannot build the cross-file schema for this statement: {e}"
            ))
        })?;
        // merged id → (slot, local id); Schema::new renumbered by name-sort.
        let mut map: Vec<(usize, u32)> = Vec::with_capacity(merged.tables.len());
        for t in &merged.tables {
            if let Some((_, slot, local)) = mangled.iter().find(|(n, _, _)| n == &t.name) {
                map.push((*slot, *local));
            } else {
                let local = main_bundle
                    .tables
                    .iter()
                    .find(|mt| mt.name == t.name)
                    .map(|mt| mt.id)
                    .ok_or_else(|| {
                        Error::Internal(format!(
                            "merged table `{}` missing from the main schema",
                            t.name
                        ))
                    })?;
                map.push((0, local));
            }
        }

        let views = self.load_view_catalog()?;
        let (plan, is_explain) = mpedb_sql::prepare_maybe_explain_with_views(
            sql,
            &merged,
            &PolicyCatalog::empty(),
            &views,
            self.bare_group_by,
            &self.host_udf_set(),
            // A cross-file plan spans a MERGED schema whose table ids are
            // synthetic and whose members are separate files; there is no one
            // catalog to read counts from. The MPEE solver keeps its
            // structural term (cartesian-step avoidance, which needs no
            // statistics) and simply cannot rank the tables by size.
            mpedb_sql::NO_ROW_COUNTS,
        )?;
        if !plan.footprint.read_only {
            // The resolver refuses writes by name before this point; keep the
            // engine honest if a new statement shape slips through.
            return Err(Error::Unsupported(
                "cross-file statements are read-only in v1".into(),
            ));
        }
        Ok((
            CrossPlan {
                plan: Arc::new(plan),
                schema: Arc::new(merged),
                map,
                member_names,
                gens,
                epoch: guard.epoch,
            },
            is_explain,
        ))
    }

    /// One-shot compile + run for a cross-file statement (the `query` path).
    pub(crate) fn query_cross(
        &self,
        session: &Session,
        sql: &str,
        tables: &[(String, String)],
        params: &[Value],
    ) -> Result<ExecResult> {
        let guard = self.attached.read().expect(POISON);
        let (cp, is_explain) = self.compile_cross(&guard, sql, tables)?;
        if is_explain {
            return Ok(ExecResult::Explain(cp.plan.explain(&cp.schema)));
        }
        let full = crate::session::resolve_params_timed(&cp.plan, params, session)?;
        self.run_cross(&guard, &cp, &full, None)
    }

    /// Execute a cached cross plan by hash, if it is one. `Ok(None)` = not a
    /// cross plan; the caller continues on the ordinary path.
    pub(crate) fn execute_cross_cached(
        &self,
        session: &Session,
        hash: &PlanHash,
        params: &[Value],
    ) -> Result<Option<ExecResult>> {
        let cp = match self.cross_cache.read().expect(POISON).get(hash) {
            Some(cp) => cp.clone(),
            None => return Ok(None),
        };
        let guard = self.attached.read().expect(POISON);
        if guard.epoch != cp.epoch {
            drop(guard);
            self.evict_cross(hash);
            return Err(Error::PlanInvalidated);
        }
        let full = crate::session::resolve_params_timed(&cp.plan, params, session)?;
        self.run_cross(&guard, &cp, &full, Some(hash)).map(Some)
    }

    fn evict_cross(&self, hash: &PlanHash) {
        self.cross_cache.write().expect(POISON).remove(hash);
    }

    /// Pin one read snapshot per involved file and execute. Cross-file reads
    /// are per-file-consistent only — see the module docs.
    fn run_cross(
        &self,
        guard: &AttachState,
        cp: &CrossPlan,
        params: &[Value],
        hash: Option<&PlanHash>,
    ) -> Result<ExecResult> {
        let stale = || {
            if let Some(h) = hash {
                self.evict_cross(h);
            }
            Error::PlanInvalidated
        };
        // Members resolved by NAME against the live list: a detach+re-attach
        // under the same epoch is impossible (epoch bumps), but be explicit.
        let mut member_dbs: Vec<&Database> = Vec::with_capacity(cp.member_names.len());
        for name in &cp.member_names {
            match guard.find(name) {
                Some(i) => member_dbs.push(&guard.members[i].db),
                None => return Err(stale()),
            }
        }
        // Host UDFs (main's registry) for a plan that calls one.
        let tables_host = self.host_tables(&cp.plan);
        let host: Option<&dyn HostFns> =
            tables_host.as_ref().map(|(f, _, _)| f as &dyn HostFns);
        let host_aggs: Option<&dyn mpedb_types::HostAggs> =
            tables_host.as_ref().map(|(_, a, _)| a as &dyn mpedb_types::HostAggs);
        let host_colls: Option<&dyn mpedb_types::HostColls> =
            tables_host.as_ref().map(|(_, _, c)| c as &dyn mpedb_types::HostColls);

        // Pin main, then each member, checking schema staleness UNDER the pin
        // that will scan (a member's live DDL invalidates, never misreads).
        let main_txn = self.engine.begin_read()?;
        if self.engine.schema().schema_gen != cp.gens[0] {
            return Err(stale()); // drop releases the reader slot
        }
        let mut member_txns: Vec<ReadTxn<'_>> = Vec::with_capacity(member_dbs.len());
        for (i, db) in member_dbs.iter().enumerate() {
            let t = db.engine.begin_read()?;
            if db.engine.schema().schema_gen != cp.gens[i + 1] {
                return Err(stale());
            }
            member_txns.push(t);
        }
        let mut partial = false;
        let res = {
            let mut ctxs: Vec<ReadCtx<'_, '_>> = Vec::with_capacity(1 + member_txns.len());
            ctxs.push(ReadCtx(&main_txn, host, host_aggs, host_colls, ChargeMode::PerRow));
            for t in &member_txns {
                ctxs.push(ReadCtx(t, None, None, None, ChargeMode::PerRow));
            }
            let mut multi = MultiCtx {
                ctxs,
                map: &cp.map,
            };
            exec_stmt(&mut multi, &cp.schema, &cp.plan, params, &mut partial)
        };
        match res {
            Ok(out) => {
                main_txn.finish()?;
                for t in member_txns {
                    t.finish()?;
                }
                Ok(out)
            }
            Err(e) => Err(e), // drops release every reader slot
        }
    }
}

/// The cross-file execution context: routes every row operation to the
/// involved member's own pinned [`ReadCtx`] via the merged-id map. Ids
/// outside the map (the DUAL/CTE sentinels, which the executor handles
/// above the ctx) fall through to main, whose engine range-checks exactly
/// as the native path would.
struct MultiCtx<'t, 'e> {
    ctxs: Vec<ReadCtx<'t, 'e>>,
    map: &'t [(usize, u32)],
}

impl MultiCtx<'_, '_> {
    fn slot(&self, table: u32) -> (usize, u32) {
        self.map
            .get(table as usize)
            .copied()
            .unwrap_or((0, table))
    }
}

impl TxnCtx for MultiCtx<'_, '_> {
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.ctxs[0].host_fns()
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.ctxs[0].host_aggs()
    }
    fn host_colls(&self) -> Option<&dyn mpedb_types::HostColls> {
        self.ctxs[0].host_colls()
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].get_by_pk(local, pk)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].get_by_index(local, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].scan_by_index(local, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].scan_by_index_range(local, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].scan_rows_raw(local, lo, hi)
    }
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].scan_rows_capped(local, lo, hi, filter, cap)
    }
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, mpedb_types::OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].scan_rows_topk(local, lo, hi, filter, order_by, keep)
    }
    fn insert_row(&mut self, _table: u32, _values: &[Value]) -> Result<()> {
        Err(cross_write_bug())
    }
    fn update_by_pk(&mut self, _table: u32, _new_values: &[Value]) -> Result<bool> {
        Err(cross_write_bug())
    }
    fn delete_by_pk(&mut self, _table: u32, _pk: &[Value]) -> Result<bool> {
        Err(cross_write_bug())
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (m, local) = self.slot(table);
        self.ctxs[m].fts_prefix(local, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        // One budget for the whole statement: main's meter (the handle the
        // statement ran on), exactly one accounting domain per execution.
        self.ctxs[0].charge_work(n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        self.ctxs[0].join_cells_budget()
    }
}

fn cross_write_bug() -> Error {
    Error::Internal("cross-file plan routed a write operation".into())
}
