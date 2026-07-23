//! The procedure engine: define (host-side parse → IR → publish) and call
//! (resolve → validate → interpret against the mpedb core).
//!
//! # Storage
//!
//! Procedures live in the facade's system-record keyspace
//! ([`Database::sys_record_put`]) under two namespaces:
//!
//! - `proc`  / `<name>`       → canonical proc blob (latest definition)
//! - `proch` / `<hash bytes>` → the same blob, content-addressed
//!
//! so calls can go by name (mutable binding, redefinable) or by hash
//! (immutable). Hash-keyed loads verify `blake3(blob) == key` before
//! decoding — accidental corruption degrades to [`Error::Corrupt`].
//! Name-keyed blobs have no external anchor (the name record *is* the
//! authority, like any user data under the §7.2 shared-fate trust model);
//! they still pass the full structural validation on every load.
//!
//! # Resolving a name-or-hash (git-style short hashes)
//!
//! Full content hashes are 64 hex chars — correct but unwieldy to type. Like
//! git short SHAs, [`ProcEngine::resolve`] also accepts a **unique hex
//! prefix** of a stored hash. The full 32-byte hashes are never shortened
//! internally — they remain the integrity/coordination anchor; only the
//! human-facing *resolution* is abbreviated. Precedence, highest first:
//!
//! 1. **Full 64-hex hash** present in `proch/` — resolved as a hash (works
//!    exactly as before; a 64-hex string that is not a stored hash falls
//!    through to the name lookup, since names may be hex-ish).
//! 2. **Exact defined name** — a name always wins over a hash *prefix*, so a
//!    procedure literally named `abcd` resolves to that procedure even if
//!    `abcd` is also a prefix of some content hash.
//! 3. **Hex prefix** of 4..=63 chars matching stored hashes in `proch/`:
//!    exactly one match resolves it; zero matches is "unknown procedure";
//!    two or more is an ambiguous-prefix error that lists the candidates'
//!    short hashes.
//!
//! # Transactions
//!
//! A proc containing any `DbExec` runs **entirely inside one
//! [`WriteSession`]**: it sees its own writes, commits atomically at a
//! successful return, and rolls back on *any* error — type errors, budget
//! exhaustion, constraint violations, a poisoned session. A proc with only
//! `DbQuery` never touches the writer lock: each query runs on a lock-free
//! read snapshot (consistency is per-statement, as with autocommit reads).
//!
//! All plans are prepared and published at define time; `call` never parses
//! SQL and never calls `prepare` — the facade locking rules hold trivially.

use mpedb_spell::emit::{cerr, CallKind, Skeleton};
use mpedb_spell::hash::ProcHash;
use mpedb_spell::interp::{self, Budget, DbBridge, ProcValue};
use mpedb_spell::ir::{PlanKind, PlanRef, Proc};
use mpedb_spell::{py, rs};
use mpedb::{Database, Error, ExecResult, Result, Value, WriteSession};
use mpedb_sql::PlanStmt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// The proc namespaces are owned by `mpedb` (the trigger catalog reads them
// too); re-exported here so existing `mpedb_proc::NS_PROC` users are unmoved.
pub use mpedb::{NS_PROC, NS_PROC_HASH};

const POISON: &str = "proc cache lock poisoned";

/// Source language of a procedure. Both compile to the same IR; semantics
/// follow the source language where they differ (notably `/` and `%` on
/// integers), so the same algorithm may hash differently across languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    Rust,
}

/// Catalog entry summary (see [`ProcEngine::list`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub name: String,
    pub hash: ProcHash,
    pub argc: u16,
    /// Whether the proc contains DML (and therefore runs transactionally).
    pub writes: bool,
}

/// Compiles, stores and executes procedures against one [`Database`].
/// Cheap to construct; holds only a per-process cache of validated procs
/// (keyed by content hash, so cached entries can never go stale).
pub struct ProcEngine<'db> {
    db: &'db Database,
    cache: RwLock<HashMap<ProcHash, Arc<Proc>>>,
    budget: Budget,
}

impl<'db> ProcEngine<'db> {
    pub fn new(db: &'db Database) -> ProcEngine<'db> {
        ProcEngine {
            db,
            cache: RwLock::new(HashMap::new()),
            budget: Budget::default(),
        }
    }

    /// Override the per-call budgets (defaults: 1_000_000 instructions,
    /// 10_000 db calls, 10_000_000 cursor rows).
    pub fn set_budget(&mut self, max_instrs: u64, max_db_calls: u64, max_rows: u64) {
        self.budget = Budget {
            instrs: max_instrs,
            db_calls: max_db_calls,
            rows: max_rows,
        };
    }

    /// Parse `source` (host-side, exactly once), compile every embedded SQL
    /// string through the facade (publishing the plans to the shared
    /// registry), assemble + validate the IR, and store the blob under both
    /// its name and its content hash. Idempotent: re-defining identical
    /// source writes nothing and returns the same hash; re-defining a name
    /// with a different body rebinds the name (the old hash stays callable).
    ///
    /// Must not be called while a [`WriteSession`] from the same handle is
    /// open on this thread (it prepares plans and writes records).
    pub fn define(&self, source: &str, lang: Lang) -> Result<ProcHash> {
        let skel = match lang {
            Lang::Python => py::compile(source)?,
            Lang::Rust => rs::compile(source)?,
        };
        let proc = self.link(&skel, lang)?;
        let blob = proc.encode();
        let hash = proc.hash();
        // Read-first publication, same shape as the plan registry.
        if self.db.sys_record_get(NS_PROC_HASH, &hash.0)?.as_deref() != Some(&blob[..]) {
            self.db.sys_record_put(NS_PROC_HASH, &hash.0, &blob)?;
        }
        if self.db.sys_record_get(NS_PROC, proc.name.as_bytes())?.as_deref() != Some(&blob[..]) {
            self.db.sys_record_put(NS_PROC, proc.name.as_bytes(), &blob)?;
        }
        self.cache
            .write()
            .expect(POISON)
            .insert(hash, Arc::new(proc));
        Ok(hash)
    }

    /// Compile the collected SQL against the live schema, check each call
    /// form against the recomputed plan footprint, publish, and assemble.
    fn link(&self, skel: &Skeleton, lang: Lang) -> Result<Proc> {
        let lang_tag = match lang {
            Lang::Python => "py",
            Lang::Rust => "rs",
        };
        let schema = self.db.schema();
        // The v1 cursor rule, surfaced as a LOCATED define-time error (IR
        // validation would reject the combination anyway, but as a blob
        // "Corrupt" — this names the offending call instead).
        if let Some(rows_call) = skel.calls.iter().find(|c| c.kind == CallKind::Rows) {
            if skel.calls.iter().any(|c| c.kind == CallKind::Exec) {
                return Err(cerr(
                    lang_tag,
                    rows_call.line,
                    rows_call.col,
                    "db.rows() cursors are allowed only in read-only procedures; \
                     this procedure also calls db.execute() — use db.query() here",
                ));
            }
        }
        let mut plans = Vec::with_capacity(skel.calls.len());
        for call in &skel.calls {
            let at = |msg: String| cerr(lang_tag, call.line, call.col, msg);
            let plan = mpedb_sql::prepare(&call.sql, &schema)
                .map_err(|e| at(format!("embedded SQL failed to compile: {e}")))?;
            if matches!(
                plan.stmt,
                PlanStmt::Begin
                    | PlanStmt::Commit
                    | PlanStmt::Rollback
                    | PlanStmt::Savepoint(_)
                    | PlanStmt::Release(_)
                    | PlanStmt::RollbackTo(_)
            ) {
                return Err(at(
                    "BEGIN/COMMIT/ROLLBACK/SAVEPOINT cannot appear inside a procedure; \
                     the whole procedure already is one transaction"
                        .into(),
                ));
            }
            let kind = match call.kind {
                CallKind::Query if plan.footprint.read_only => PlanKind::Query,
                CallKind::Query => {
                    return Err(at(
                        "db.query() requires a read-only SELECT; use db.execute() for DML"
                            .into(),
                    ))
                }
                CallKind::Rows if plan.footprint.read_only => PlanKind::Query,
                CallKind::Rows => {
                    return Err(at(
                        "db.rows() requires a read-only SELECT; use db.execute() for DML"
                            .into(),
                    ))
                }
                CallKind::Exec if !plan.footprint.read_only => PlanKind::Exec,
                CallKind::Exec => {
                    return Err(at(
                        "db.execute() requires DML; use db.query() for SELECT".into(),
                    ))
                }
            };
            // Count what the CALLER supplies — `n_params` also counts the
            // reserved subplan/context tail (a scalar subquery, a literal
            // `'now'`), which the executor fills itself; requiring arguments
            // for those refused every proc whose SQL held a subquery.
            if plan.n_user_params() as usize != call.argc as usize {
                return Err(at(format!(
                    "SQL takes {} parameter(s) but {} argument(s) were passed",
                    plan.n_user_params(),
                    call.argc
                )));
            }
            // Publish through the facade (shared registry) — the runtime
            // will execute purely by hash, possibly in another process.
            let hash = self.db.prepare(&call.sql)?;
            if hash != plan.hash() {
                return Err(Error::Internal(
                    "facade and define-time plan hashes diverged".into(),
                ));
            }
            plans.push(PlanRef {
                hash,
                kind,
                argc: call.argc,
            });
        }
        Proc::new(
            skel.name.clone(),
            skel.argc,
            skel.nlocals,
            plans,
            skel.consts.clone(),
            skel.instrs.clone(),
        )
    }

    /// Execute a stored procedure by name or 64-hex content hash.
    ///
    /// Errors roll back everything: a proc that writes runs inside a single
    /// write transaction which only commits on a successful `return`.
    pub fn call(&self, name_or_hash: &str, args: &[Value]) -> Result<ProcValue> {
        let proc = self.resolve(name_or_hash)?;
        if args.len() != proc.argc as usize {
            return Err(Error::WrongParamCount {
                expected: proc.argc as usize,
                got: args.len(),
            });
        }
        if proc.has_exec() {
            let mut session = self.db.begin()?;
            let res = {
                let mut bridge = SessionBridge {
                    session: &mut session,
                };
                interp::run(&proc, args, &mut bridge, self.budget)
            };
            match res {
                Ok(v) => {
                    session.commit()?;
                    Ok(v)
                }
                Err(e) => {
                    // Covers budget exhaustion, runtime type errors,
                    // constraint violations and poisoned sessions alike:
                    // nothing the proc did survives.
                    session.rollback();
                    Err(e)
                }
            }
        } else {
            let mut bridge = SnapshotBridge {
                db: self.db,
                streams: Vec::new(),
            };
            interp::run(&proc, args, &mut bridge, self.budget)
        }
    }

    /// All name-keyed procedures (undecodable blobs are skipped — this is
    /// a tooling view, and a later `define` heals a bad record).
    pub fn list(&self) -> Result<Vec<ProcInfo>> {
        let mut out = Vec::new();
        for (key, blob) in self.db.sys_record_scan(NS_PROC)? {
            let Ok(name) = String::from_utf8(key) else {
                continue;
            };
            let Ok(proc) = Proc::decode(&blob) else {
                continue;
            };
            out.push(ProcInfo {
                name,
                hash: proc.hash(),
                argc: proc.argc,
                writes: proc.has_exec(),
            });
        }
        Ok(out)
    }

    /// Summary of one procedure, by name or hash.
    pub fn info(&self, name_or_hash: &str) -> Result<ProcInfo> {
        let proc = self.resolve(name_or_hash)?;
        Ok(ProcInfo {
            name: proc.name.clone(),
            hash: proc.hash(),
            argc: proc.argc,
            writes: proc.has_exec(),
        })
    }

    // ------------------------------------------------------------ loading

    fn resolve(&self, name_or_hash: &str) -> Result<Arc<Proc>> {
        if name_or_hash.is_empty() {
            return Err(Error::Unsupported("empty procedure name".into()));
        }
        // Hash form first; a 64-hex string that misses falls through to the
        // name lookup (names may legally look hex-ish).
        if let Ok(h) = name_or_hash.parse::<ProcHash>() {
            if let Some(p) = self.cache.read().expect(POISON).get(&h) {
                return Ok(p.clone());
            }
            if let Some(blob) = self.db.sys_record_get(NS_PROC_HASH, &h.0)? {
                return self.load_hashed(&h, &blob);
            }
        }
        let Some(blob) = self.db.sys_record_get(NS_PROC, name_or_hash.as_bytes())? else {
            // Not a defined name. If it looks like an abbreviated content hash
            // (git-style short hash), resolve it by unique prefix over the
            // `proch/` namespace. Reached only after the name lookup missed, so
            // a real name always takes precedence over a hash prefix.
            if is_hash_prefix(name_or_hash) {
                return self.resolve_hash_prefix(name_or_hash);
            }
            return Err(Error::Unsupported(format!(
                "unknown procedure `{name_or_hash}` (define it first)"
            )));
        };
        let h = ProcHash(*blake3::hash(&blob).as_bytes());
        // NB: take the cached Arc out before matching — `load_hashed` needs
        // the cache *write* lock, so the read guard must not live across it.
        let cached = self.cache.read().expect(POISON).get(&h).cloned();
        let proc = match cached {
            Some(p) => p,
            None => self.load_hashed(&h, &blob)?,
        };
        if proc.name != name_or_hash {
            return Err(Error::Corrupt(format!(
                "proc record for `{name_or_hash}` contains a procedure named `{}`",
                proc.name
            )));
        }
        Ok(proc)
    }

    /// Verify content hash, fully re-validate, cache. The hash check makes
    /// hash-addressed execution immune to accidental blob corruption: bytes
    /// that do not hash to the key are never even decoded.
    fn load_hashed(&self, h: &ProcHash, blob: &[u8]) -> Result<Arc<Proc>> {
        if *blake3::hash(blob).as_bytes() != h.0 {
            return Err(Error::Corrupt(
                "proc blob does not match its content hash".into(),
            ));
        }
        let proc = Arc::new(Proc::decode(blob)?);
        self.cache
            .write()
            .expect(POISON)
            .insert(*h, proc.clone());
        Ok(proc)
    }

    /// Resolve a git-style short content-hash prefix over the `proch/`
    /// namespace. `prefix` is already known to be 4..=63 hex chars and not a
    /// defined name. Exactly one match resolves; zero is "unknown"; two or
    /// more lists the ambiguous candidates' short hashes.
    fn resolve_hash_prefix(&self, prefix: &str) -> Result<Arc<Proc>> {
        // Full hashes render lowercase (ProcHash Display); match case-folded.
        let needle = prefix.to_ascii_lowercase();
        let mut matches: Vec<ProcHash> = Vec::new();
        for (key, _blob) in self.db.sys_record_scan(NS_PROC_HASH)? {
            // `proch/` keys are raw 32-byte content hashes; ignore anything of
            // another length defensively.
            let Ok(bytes) = <[u8; 32]>::try_from(key) else {
                continue;
            };
            let h = ProcHash(bytes);
            if h.to_string().starts_with(&needle) {
                matches.push(h);
            }
        }
        match matches.as_slice() {
            [] => Err(Error::Unsupported(format!(
                "unknown procedure `{prefix}`: no defined name, and no proc hash \
                 begins with `{needle}`"
            ))),
            [h] => self.load_by_hash(h),
            many => {
                // List up to 8 candidate short hashes so the error is actionable.
                const LIST: usize = 8;
                let mut shown: Vec<String> =
                    many.iter().take(LIST).map(short_hash).collect();
                if many.len() > LIST {
                    shown.push(format!("… ({} more)", many.len() - LIST));
                }
                Err(Error::Unsupported(format!(
                    "ambiguous proc hash prefix `{prefix}` matches {} procedures: {}",
                    many.len(),
                    shown.join(", ")
                )))
            }
        }
    }

    /// Load a proc by full hash from cache or `proch/` (used by prefix
    /// resolution once a unique match is known to exist).
    fn load_by_hash(&self, h: &ProcHash) -> Result<Arc<Proc>> {
        if let Some(p) = self.cache.read().expect(POISON).get(h) {
            return Ok(p.clone());
        }
        let blob = self.db.sys_record_get(NS_PROC_HASH, &h.0)?.ok_or_else(|| {
            Error::Corrupt("proc hash vanished from proch between scan and load".into())
        })?;
        self.load_hashed(h, &blob)
    }
}

/// A hex string that could abbreviate a content hash: 4..=63 hex digits. A
/// full 64-hex hash is handled by the exact-hash path, and shorter than 4 is
/// too ambiguous to be useful (matching git's short-SHA minimum).
fn is_hash_prefix(s: &str) -> bool {
    (4..64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// First 12 hex chars of a hash, git-short style, for ambiguity messages.
fn short_hash(h: &ProcHash) -> String {
    h.to_string()[..12].to_owned()
}

// ------------------------------------------------------------------ bridges

/// Read-only path: every `DbQuery` routes through `Database::execute`,
/// which sends read-only plans to lock-free read snapshots; every cursor
/// is a [`mpedb::RowStream`] from `Database::stream_query` — one pinned
/// snapshot per cursor, rows pulled one at a time (O(1) memory in the
/// result size). Each open cursor holds one engine reader slot; the
/// interpreter's `MAX_CURSORS` bound keeps that far below `max_readers`.
struct SnapshotBridge<'a> {
    db: &'a Database,
    /// Stream ids handed to the interpreter are indices here; exhausted
    /// streams are dropped (releasing their snapshot) but their slot is
    /// never reused within one call — ids stay unambiguous.
    streams: Vec<Option<mpedb::RowStream<'a>>>,
}

impl DbBridge for SnapshotBridge<'_> {
    fn query(&mut self, plan: &PlanRef, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        match self.db.execute(&plan.hash, params)? {
            ExecResult::Rows { rows, .. } => Ok(rows),
            _ => Err(Error::Corrupt(
                "proc: query-kind plan did not produce rows".into(),
            )),
        }
    }

    fn exec(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<u64> {
        Err(Error::Internal(
            "read-only procedure attempted a DbExec (validator bug)".into(),
        ))
    }

    fn cursor_open(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u32> {
        let stream = self.db.stream_query(&plan.hash, params)?;
        self.streams.push(Some(stream));
        Ok((self.streams.len() - 1) as u32)
    }

    fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>> {
        let slot = self
            .streams
            .get_mut(stream as usize)
            .ok_or_else(|| Error::Internal("proc: unknown cursor stream id".into()))?;
        let Some(s) = slot else {
            return Err(Error::Internal(
                "proc: advance on an exhausted cursor stream".into(),
            ));
        };
        let row = s.next()?;
        if row.is_none() {
            *slot = None; // drop the stream: releases its reader slot NOW
        }
        Ok(row)
    }
}

/// Write path: the whole proc shares one session — queries see the proc's
/// own uncommitted writes, and the caller commits/rolls back atomically.
struct SessionBridge<'a, 'db> {
    session: &'a mut WriteSession<'db>,
}

impl DbBridge for SessionBridge<'_, '_> {
    fn query(&mut self, plan: &PlanRef, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        match self.session.execute(&plan.hash, params)? {
            ExecResult::Rows { rows, .. } => Ok(rows),
            _ => Err(Error::Corrupt(
                "proc: query-kind plan did not produce rows".into(),
            )),
        }
    }

    fn exec(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u64> {
        match self.session.execute(&plan.hash, params)? {
            ExecResult::Affected(n) => Ok(n),
            _ => Err(Error::Corrupt(
                "proc: exec-kind plan did not report an affected count".into(),
            )),
        }
    }

    // Unreachable by construction: IR validation rejects any proc that
    // contains both CursorOpen and DbExec (the v1 read-only-cursors rule),
    // and only has_exec procs run on this bridge.
    fn cursor_open(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<u32> {
        Err(Error::Internal(
            "write procedure attempted a cursor open (validator bug)".into(),
        ))
    }

    fn cursor_advance(&mut self, _stream: u32) -> Result<Option<Vec<Value>>> {
        Err(Error::Internal(
            "write procedure attempted a cursor advance (validator bug)".into(),
        ))
    }
}
