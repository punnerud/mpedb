//! Stored SQL functions — PySpell definitions callable from SQL (stage M2).
//!
//! `create_function` compiles a `def name(args): …` (Python subset) or
//! `fn name(args) { … }` (Rust subset) through the mpedb-spell pipeline —
//! full procedure subset, loops included — and stores it in the shared
//! sys-keyspace: `funch/<hash>` holds the content-addressed IR blob,
//! `func/<name>` binds the name to a hash + arity. The catalog is
//! schema_gen-gated exactly like views: redefinition bumps the generation,
//! every process's plan cache drops, and the next prepare re-binds.
//!
//! The difference from a host UDF (DESIGN-UDF) is the whole point: the
//! definition lives in the FILE, so a plan calling it — which carries the
//! function's content HASH in its const pool ([`Instr::SpellCall`]) — is
//! deterministic across every attached process and may enter the shared plan
//! registry. A host closure is connection-local; a spell is database-global.
//!
//! v1 restrictions, each a refusal with a reason:
//! - a stored SQL function cannot run SQL (`db.query`/`db.exec` in the body) —
//!   a scalar evaluated per row must not open cursors mid-row; that is what
//!   stored PROCEDURES (`mpedb proc`) are for;
//! - the return value must be a scalar (a list/tuple has no SQL cell type);
//! - execution runs under a fixed instruction budget, so a runaway spell is a
//!   deterministic error at the same count on every process.

use std::sync::Arc;

use mpedb_spell::interp::{self, Budget, DbBridge, ProcValue};
use mpedb_spell::ir::{PlanRef, Proc};
use mpedb_types::{Error, Result, Value};

pub const NS_FUNC: &str = "func";
pub const NS_FUNC_HASH: &str = "funch";

/// Per-call instruction budget. Fixed and documented rather than configurable
/// (v1): the SAME statement must trip at the SAME count on every process, and
/// a per-connection knob would break that.
pub const FN_BUDGET: Budget = Budget { instrs: 250_000, db_calls: 0, rows: 0 };

/// The source language of a stored function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpellLang {
    Python,
    Rust,
}

/// One stored function, as `list_functions` reports it.
#[derive(Debug, Clone)]
pub struct SpellFnInfo {
    pub name: String,
    pub hash_hex: String,
    pub argc: u16,
}

const FUNC_RECORD_VERSION: u8 = 1;

fn encode_func_record(hash: &[u8; 32], argc: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(35);
    v.push(FUNC_RECORD_VERSION);
    v.extend_from_slice(hash);
    v.extend_from_slice(&argc.to_le_bytes());
    v
}

fn decode_func_record(bytes: &[u8]) -> Option<([u8; 32], u16)> {
    if bytes.len() != 35 || bytes[0] != FUNC_RECORD_VERSION {
        return None;
    }
    let hash: [u8; 32] = bytes[1..33].try_into().ok()?;
    let argc = u16::from_le_bytes(bytes[33..35].try_into().ok()?);
    Some((hash, argc))
}

/// The database bridge a SQL function gets: none. Unreachable for functions
/// this module defined (bodies with SQL are refused at create), so hitting it
/// means a forged `funch` blob — refuse, never guess.
struct RefuseDb;

impl DbBridge for RefuseDb {
    fn query(&mut self, _: &PlanRef, _: &[Value]) -> Result<Vec<Vec<Value>>> {
        Err(Error::Unsupported("a stored SQL function cannot touch the database".into()))
    }
    fn exec(&mut self, _: &PlanRef, _: &[Value]) -> Result<u64> {
        Err(Error::Unsupported("a stored SQL function cannot touch the database".into()))
    }
    fn cursor_open(&mut self, _: &PlanRef, _: &[Value]) -> Result<u32> {
        Err(Error::Unsupported("a stored SQL function cannot touch the database".into()))
    }
    fn cursor_advance(&mut self, _: u32) -> Result<Option<Vec<Value>>> {
        Err(Error::Unsupported("a stored SQL function cannot touch the database".into()))
    }
}

/// Run one stored function over already-evaluated arguments — the
/// [`mpedb_types::HostFns::call_spell`] backend.
pub(crate) fn call_spell_fn(proc: &Proc, args: &[Value]) -> Result<Value> {
    if args.len() != proc.argc as usize {
        return Err(Error::TypeMismatch(format!(
            "{}() takes {} argument(s), got {}",
            proc.name,
            proc.argc,
            args.len()
        )));
    }
    match interp::run(proc, args, &mut RefuseDb, FN_BUDGET)? {
        ProcValue::Scalar(v) => Ok(v),
        other => Err(Error::TypeMismatch(format!(
            "{}() returned a non-scalar ({other}); a SQL function must return a scalar",
            proc.name
        ))),
    }
}

impl crate::Database {
    /// Define (or redefine) a stored SQL function from source. The function's
    /// NAME and ARITY come from the definition itself (`def double(x):` is
    /// `double/1`) — there is nothing to keep in sync. Returns (name, hash).
    pub fn create_function(&self, lang: SpellLang, source: &str) -> Result<(String, String)> {
        let skeleton = match lang {
            SpellLang::Python => mpedb_spell::py::compile(source)?,
            SpellLang::Rust => mpedb_spell::rs::compile(source)?,
        };
        if !skeleton.calls.is_empty() {
            return Err(Error::Unsupported(
                "a stored SQL function cannot run SQL — use a stored procedure \
                 (`mpedb proc define`) for database work"
                    .into(),
            ));
        }
        let proc = Proc::new(
            skeleton.name.clone(),
            skeleton.argc,
            skeleton.nlocals,
            Vec::new(),
            skeleton.consts,
            skeleton.instrs,
        )?;
        let blob = proc.encode();
        let hash = proc.hash();
        let name = skeleton.name;

        // One commit: blob + name binding + generation bump land together —
        // the view-DDL shape (`apply_create_view`), applied to functions.
        let record = encode_func_record(&hash.0, proc.argc);
        let blob_key = crate::sys_record_subkey(NS_FUNC_HASH, &hash.0)?;
        let name_key = crate::sys_record_subkey(NS_FUNC, name.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&blob_key, &blob)?;
            w.sys_put(&name_key, &record)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        // The view-DDL tail: drop this process's plan cache and refresh the
        // schema bundle so the gen gate sees the bump immediately.
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok((name, hash.to_string()))
    }

    /// Remove a stored function's NAME binding. The content-addressed blob
    /// stays (another name, or a registered plan, may still reference it —
    /// content-addressed storage never breaks a pinned reference).
    pub fn drop_function(&self, name: &str) -> Result<bool> {
        let name_key = crate::sys_record_subkey(NS_FUNC, name.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let existed = match w.sys_delete(&name_key) {
            Ok(existed) => existed,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        };
        if !existed {
            w.abort();
            return Ok(false);
        }
        w.bump_schema_gen();
        w.commit()?;
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(true)
    }

    /// Every stored function's name binding.
    pub fn list_functions(&self) -> Result<Vec<SpellFnInfo>> {
        let r = self.engine.begin_read()?;
        let out = self.scan_func_records(&r)?;
        r.finish()?;
        Ok(out
            .into_iter()
            .map(|(name, hash, argc)| SpellFnInfo {
                name,
                hash_hex: mpedb_spell::ProcHash(hash).to_string(),
                argc,
            })
            .collect())
    }

    /// The function catalog on one snapshot, as the binder's `SpellFnSet`.
    pub(crate) fn load_spell_fns(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
    ) -> Result<mpedb_sql::SpellFnSet> {
        let mut set = mpedb_sql::SpellFnSet::default();
        for (name, hash, argc) in self.scan_func_records(r)? {
            set.insert(name, hash, argc);
        }
        Ok(set)
    }

    fn scan_func_records(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
    ) -> Result<Vec<(String, [u8; 32], u16)>> {
        // The ns prefix is `func\0`; `func\x01` is the exclusive upper bound.
        let mut lo = NS_FUNC.as_bytes().to_vec();
        lo.push(0);
        let mut hi = NS_FUNC.as_bytes().to_vec();
        hi.push(1);
        let mut out = Vec::new();
        for (k, v) in r.sys_scan_range(&lo, &hi)? {
            let name = String::from_utf8_lossy(&k[lo.len()..]).into_owned();
            let Some((hash, argc)) = decode_func_record(&v) else {
                // Advisory catalog: a corrupt name record degrades to "that
                // name is not defined", and the definition blob (verified by
                // hash on load) is untouched.
                continue;
            };
            out.push((name, hash, argc));
        }
        Ok(out)
    }

    /// Resolve + decode the current catalog's definitions for ONE execution —
    /// the spell half of `host_tables`. Hash-verified on load; cached
    /// process-wide by hash (immutable by construction).
    pub(crate) fn spell_table(&self) -> Result<Vec<([u8; 32], Arc<Proc>)>> {
        let r = self.engine.begin_read()?;
        let names = self.scan_func_records(&r)?;
        let mut out = Vec::with_capacity(names.len());
        for (_name, hash, _argc) in names {
            if let Some(p) = self.spell_cache.read().expect(crate::POISON).get(&hash) {
                out.push((hash, p.clone()));
                continue;
            }
            let blob_key = crate::sys_record_subkey(NS_FUNC_HASH, &hash)?;
            let Some(blob) = r.sys_get(&blob_key)? else {
                // A name bound to a missing blob: refuse at call time (the
                // table simply lacks the hash), never invent a function.
                continue;
            };
            if *blake3::hash(&blob).as_bytes() != hash {
                return Err(Error::Corrupt("stored function blob does not match its hash".into()));
            }
            let proc = Arc::new(Proc::decode(&blob)?);
            if proc.has_db_ops() {
                // A forged blob smuggling SQL ops past create_function's
                // refusal: fail closed at load, before any call runs.
                return Err(Error::Corrupt(
                    "stored function blob contains database operations".into(),
                ));
            }
            self.spell_cache
                .write()
                .expect(crate::POISON)
                .insert(hash, proc.clone());
            out.push((hash, proc));
        }
        r.finish()?;
        Ok(out)
    }
}
