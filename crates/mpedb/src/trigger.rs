//! SQL triggers (DESIGN-TRIGGERS): the `trigger/<name>` sys-keyspace catalog
//! record, its versioned wire format, and the compiled, gen-gated [`TriggerSet`]
//! the executor consults at fire time.
//!
//! Triggers are pure sys-keyspace catalog entries, exactly like views and
//! policies: they do NOT enter the Schema canonical bytes and do NOT need a
//! `PLAN_FORMAT` change. A `CREATE`/`DROP TRIGGER` bumps `schema_gen`, so every
//! attached process (including a different one acting as the ring leader) drops
//! its cached [`TriggerSet`] and rebuilds it — the same freshness contract views
//! and policies already ride (DESIGN-TRIGGERS §6).
//!
//! Stage 3 fires `BEFORE`/`AFTER` × `INSERT`/`UPDATE`/`DELETE FOR EACH ROW` with
//! a multi-statement SQL body (`BEGIN <stmt>; … END`, each an INSERT/UPDATE/
//! DELETE, run in order on the same txn) and an optional `WHEN`, binding
//! `NEW.<col>` (post-image) and `OLD.<col>` (pre-image) per event. `UPDATE OF
//! <cols>` gates firing on one of the named columns appearing in the UPDATE's
//! SET list (sqlite semantics). `BEFORE` runs its body before the row mutation;
//! `NEW` is read-only (no `NEW.col := …` mutation syntax exists). Stage 5 adds
//! `EXECUTE PROCEDURE p(args…)` bodies: a stored PySpell procedure, pinned by
//! content hash at `CREATE` ([`StoredBody::Proc`]), fired per row on the same
//! txn through the executor's `CtxBridge` — a failing procedure vetoes the
//! write. `RAISE` completes stage 3's veto: `SELECT RAISE(ABORT,'msg')
//! [WHERE …]` aborts the statement with the user's message and
//! `SELECT RAISE(IGNORE) [WHERE …]` silently skips the row
//! ([`FireOutcome::SkipRow`]); `FAIL`/`ROLLBACK` are named refusals.
//! `INSTEAD OF` and `FOR EACH STATEMENT` remain named refusals at `CREATE`.

use super::*;
use mpedb_core::WriteTxn;
use mpedb_sql::RowMap;
use mpedb_types::ExprProgram;

/// Sys-keyspace prefix for a stored trigger: `trigger/<name>` → its record.
pub(crate) const TRIGGER_PREFIX: &[u8] = b"trigger/";
/// One byte past `/` (0x2F → 0x30) — the exclusive upper bound for a prefix
/// scan of `trigger/…`, so the scan never walks the whole sys-keyspace.
const TRIGGER_PREFIX_END: &[u8] = b"trigger0";

const TRG_MAGIC: &[u8; 4] = b"MTRG";
/// Versioned from day one (DESIGN-TRIGGERS §3.3). A later layout change is a new
/// format, not a migration (the no-backward-compat standing rule).
const TRIGGER_FORMAT: u16 = 1;

const BODY_TAG_SQL: u8 = 0;
/// A PySpell procedure body (DESIGN-TRIGGERS stage 5): the tag reserved since
/// stage 0. Uses the same TRIGGER_FORMAT — a stage-3 reader meets tag 1 as
/// `Corrupt`, which the no-backward-compat standing rule accepts.
const BODY_TAG_PROC: u8 = 1;

/// Per-fire budget for a PySpell trigger body (DESIGN-TRIGGERS §5.2). Applies
/// to EACH fired row: the interpreter's instruction/db-call/row meters are the
/// runaway guard on the spell side (the trigger depth cap still bounds any SQL
/// the procedure issues). Deterministic: the same statement trips at the same
/// count in every process.
pub(crate) const TRIGGER_BUDGET: mpedb_spell::interp::Budget = mpedb_spell::interp::Budget {
    instrs: 250_000,
    db_calls: 256,
    rows: 1_000_000,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TrgTiming {
    Before,
    After,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TrgEvent {
    Insert,
    Update,
    Delete,
}

/// A stored trigger's body (DESIGN-TRIGGERS §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoredBody {
    /// In-SQL statement list, as captured source.
    Sql(String),
    /// `EXECUTE PROCEDURE` dispatch. The procedure is PINNED BY CONTENT HASH,
    /// resolved from its name at `CREATE TRIGGER`: procedure re-definition does
    /// not bump `schema_gen`, so a name binding could go stale differently in
    /// different processes — the hash cannot (the `proch/<hash>` blob is
    /// immutable). Re-defining the procedure therefore does NOT re-target the
    /// trigger; re-CREATE the trigger to bind the new version. `name` is kept
    /// for display/reconstruction; `arg_srcs` are compiled at catalog load.
    Proc {
        name: String,
        hash: [u8; 32],
        arg_srcs: Vec<String>,
    },
}

/// A decoded `trigger/<name>` catalog record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredTrigger {
    pub name: String,
    /// Target table id — stable across `ALTER TABLE … RENAME` (DESIGN-TRIGGERS
    /// §3.1), so a rename needs no record rewrite.
    pub table_id: u32,
    pub timing: TrgTiming,
    pub event: TrgEvent,
    /// `UPDATE OF a, b` columns (empty = any column). Encoded for format
    /// stability; UPDATE triggers do not yet fire.
    pub update_of: Vec<u16>,
    pub when_src: Option<String>,
    pub body: StoredBody,
}

impl StoredTrigger {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.name.len());
        b.extend_from_slice(TRG_MAGIC);
        b.extend_from_slice(&TRIGGER_FORMAT.to_le_bytes());
        b.extend_from_slice(&self.table_id.to_le_bytes());
        b.push(match self.timing {
            TrgTiming::Before => 0,
            TrgTiming::After => 1,
        });
        b.push(match self.event {
            TrgEvent::Insert => 0,
            TrgEvent::Update => 1,
            TrgEvent::Delete => 2,
        });
        b.extend_from_slice(&(self.update_of.len() as u16).to_le_bytes());
        for &c in &self.update_of {
            b.extend_from_slice(&c.to_le_bytes());
        }
        put_str(&mut b, &self.name);
        match &self.when_src {
            Some(src) => {
                b.push(1);
                put_str(&mut b, src);
            }
            None => b.push(0),
        }
        match &self.body {
            StoredBody::Sql(sql) => {
                b.push(BODY_TAG_SQL);
                put_str(&mut b, sql);
            }
            StoredBody::Proc { name, hash, arg_srcs } => {
                b.push(BODY_TAG_PROC);
                put_str(&mut b, name);
                b.extend_from_slice(hash);
                b.extend_from_slice(&(arg_srcs.len() as u16).to_le_bytes());
                for a in arg_srcs {
                    put_str(&mut b, a);
                }
            }
        }
        b
    }

    /// Decode a record. Treats its input as hostile: every read is bounds-checked
    /// and returns [`Error::Corrupt`], never panics (CLAUDE.md decoder discipline).
    pub(crate) fn decode(bytes: &[u8]) -> Result<StoredTrigger> {
        let mut p = 0usize;
        if take(bytes, &mut p, 4)? != TRG_MAGIC {
            return Err(Error::Corrupt("trigger record: bad magic".into()));
        }
        let format = u16::from_le_bytes(take(bytes, &mut p, 2)?.try_into().unwrap());
        if format != TRIGGER_FORMAT {
            return Err(Error::Corrupt(format!(
                "trigger record: unknown TRIGGER_FORMAT {format}"
            )));
        }
        let table_id = u32::from_le_bytes(take(bytes, &mut p, 4)?.try_into().unwrap());
        let timing = match take(bytes, &mut p, 1)?[0] {
            0 => TrgTiming::Before,
            1 => TrgTiming::After,
            t => return Err(Error::Corrupt(format!("trigger record: bad timing {t}"))),
        };
        let event = match take(bytes, &mut p, 1)?[0] {
            0 => TrgEvent::Insert,
            1 => TrgEvent::Update,
            2 => TrgEvent::Delete,
            e => return Err(Error::Corrupt(format!("trigger record: bad event {e}"))),
        };
        let n_of = u16::from_le_bytes(take(bytes, &mut p, 2)?.try_into().unwrap()) as usize;
        let mut update_of = Vec::with_capacity(n_of.min(1024));
        for _ in 0..n_of {
            update_of.push(u16::from_le_bytes(take(bytes, &mut p, 2)?.try_into().unwrap()));
        }
        let name = take_str(bytes, &mut p)?;
        let when_src = match take(bytes, &mut p, 1)?[0] {
            0 => None,
            1 => Some(take_str(bytes, &mut p)?),
            t => return Err(Error::Corrupt(format!("trigger record: bad when tag {t}"))),
        };
        let body = match take(bytes, &mut p, 1)?[0] {
            BODY_TAG_SQL => StoredBody::Sql(take_str(bytes, &mut p)?),
            BODY_TAG_PROC => {
                let pname = take_str(bytes, &mut p)?;
                let hash: [u8; 32] = take(bytes, &mut p, 32)?.try_into().unwrap();
                let argc = u16::from_le_bytes(take(bytes, &mut p, 2)?.try_into().unwrap()) as usize;
                let mut arg_srcs = Vec::with_capacity(argc.min(256));
                for _ in 0..argc {
                    arg_srcs.push(take_str(bytes, &mut p)?);
                }
                StoredBody::Proc { name: pname, hash, arg_srcs }
            }
            t => return Err(Error::Corrupt(format!("trigger record: unknown body tag {t}"))),
        };
        if p != bytes.len() {
            return Err(Error::Corrupt("trigger record: trailing bytes".into()));
        }
        Ok(StoredTrigger {
            name,
            table_id,
            timing,
            event,
            update_of,
            when_src,
            body,
        })
    }
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

fn take<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = p
        .checked_add(n)
        .filter(|&e| e <= b.len())
        .ok_or_else(|| Error::Corrupt("trigger record: truncated".into()))?;
    let s = &b[*p..end];
    *p = end;
    Ok(s)
}

fn take_str(b: &[u8], p: &mut usize) -> Result<String> {
    let len = u32::from_le_bytes(take(b, p, 4)?.try_into().unwrap()) as usize;
    let s = take(b, p, len)?;
    std::str::from_utf8(s)
        .map(|s| s.to_owned())
        .map_err(|_| Error::Corrupt("trigger record: invalid utf-8".into()))
}

pub(crate) fn trigger_key(name: &str) -> Vec<u8> {
    let mut k = TRIGGER_PREFIX.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

/// The sys-key of the stored trigger `name` names, matched
/// ASCII-case-insensitively — `DROP TRIGGER tr` finds `CREATE TRIGGER Tr`.
/// The key keeps the DECLARED spelling, so only the matching folds; resolved
/// inside the caller's write txn so the test and the write are atomic.
fn resolve_trigger_key(w: &mut mpedb_core::WriteTxn<'_>, name: &str) -> Result<Option<Vec<u8>>> {
    for (subkey, _) in w.sys_scan_range(TRIGGER_PREFIX, TRIGGER_PREFIX_END)? {
        let Some(stored) = subkey.strip_prefix(TRIGGER_PREFIX) else { continue };
        if mpedb_types::ident_eq(&String::from_utf8_lossy(stored), name) {
            return Ok(Some(subkey));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------- compiled fire-set

/// A compiled trigger body: in-SQL statements, or a PySpell procedure dispatch
/// (DESIGN-TRIGGERS §5).
pub(crate) enum TriggerBody {
    /// Body statements in declaration order (DESIGN-TRIGGERS stage 3
    /// multi-statement bodies): DML with its own `NEW`/`OLD` row-slot map, or
    /// a `RAISE` veto statement.
    Sql(Vec<mpedb_sql::TriggerStmt>),
    Spell(SpellTriggerBody),
}

/// What a fired trigger set decided about the current row (DESIGN-TRIGGERS
/// §4.3): proceed normally, or — `RAISE(IGNORE)` — silently skip the row's
/// operation and all remaining trigger work for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FireOutcome {
    Proceed,
    SkipRow,
}

/// An `EXECUTE PROCEDURE` body, resolved at catalog build (gen-gated, so a DDL
/// commit anywhere rebuilds it). A resolution failure does NOT fail the build —
/// it poisons THIS body (`ready = Err(msg)`), so only statements that would
/// fire this trigger error, with a message naming the trigger; every other
/// table's DML is untouched.
pub(crate) struct SpellTriggerBody {
    /// Positional argument programs over the `NEW`/`OLD` images, in call order.
    pub args: Vec<(ExprProgram, RowMap)>,
    pub ready: std::result::Result<SpellReady, String>,
}

/// The resolved, fire-ready spell: the pinned procedure and EVERY embedded
/// plan pre-resolved by hash — fire time needs no registry, no `Database`, no
/// re-preparation (DESIGN-TRIGGERS §5.2: no SQL is ever parsed at fire time).
pub(crate) struct SpellReady {
    pub proc: Arc<mpedb_spell::ir::Proc>,
    pub plans: HashMap<[u8; 32], Arc<CompiledPlan>>,
}

/// One compiled trigger ready to fire: the body (SQL statements whose leading
/// parameters are the `NEW`/`OLD` columns named by their row-slot maps, or a
/// PySpell dispatch), an optional `WHEN` guard (its own program over its own
/// row-slot map), and the `UPDATE OF` column set. See DESIGN-TRIGGERS §3.4 and
/// `mpedb_sql::compile_trigger_body`.
pub(crate) struct CompiledTrigger {
    pub name: String,
    pub body: TriggerBody,
    /// `UPDATE OF a, b` column indices (empty = any column). UPDATE-event
    /// triggers only: a fire is gated on one of these columns appearing in the
    /// UPDATE's SET list (sqlite semantics — the SET target list, not whether the
    /// value actually changed).
    pub update_of: Vec<u16>,
    pub when: Option<(ExprProgram, RowMap)>,
}

/// The gen-gated set of triggers this process can fire, grouped by target table
/// id (DESIGN-TRIGGERS stage 3): `BEFORE`/`AFTER` × `INSERT`/`UPDATE`/`DELETE
/// FOR EACH ROW`. `INSTEAD OF`, `FOR EACH STATEMENT`, and `EXECUTE PROCEDURE`
/// are refused at `CREATE`, so those never reach here.
pub(crate) struct TriggerSet {
    pub before_insert: HashMap<u32, Vec<CompiledTrigger>>,
    pub before_update: HashMap<u32, Vec<CompiledTrigger>>,
    pub before_delete: HashMap<u32, Vec<CompiledTrigger>>,
    pub after_insert: HashMap<u32, Vec<CompiledTrigger>>,
    pub after_update: HashMap<u32, Vec<CompiledTrigger>>,
    pub after_delete: HashMap<u32, Vec<CompiledTrigger>>,
}

impl TriggerSet {
    /// The trigger-free set — allocation-free, so trigger-free databases pay
    /// nothing on the write path but one empty-map lookup per row.
    pub(crate) fn empty() -> TriggerSet {
        TriggerSet {
            before_insert: HashMap::new(),
            before_update: HashMap::new(),
            before_delete: HashMap::new(),
            after_insert: HashMap::new(),
            after_update: HashMap::new(),
            after_delete: HashMap::new(),
        }
    }
}

/// Build the stored body from the parsed spec, compile-checking everything
/// checkable at `CREATE TRIGGER` (define-time loudness, DESIGN-TRIGGERS §3.1):
/// an SQL body must compile; an `EXECUTE PROCEDURE` body must name a stored
/// procedure (looked up through `load_proc_blob`, so both the autocommit and
/// the in-txn DDL route resolve on their own snapshot), the argument count
/// must match the procedure's declared arity, and each argument source must
/// compile in the trigger's `NEW`/`OLD` scope. The procedure is pinned by
/// content hash here (see [`StoredBody::Proc`]).
fn build_stored_body(
    body: &mpedb_sql::TriggerBodySpec,
    table: &mpedb_types::TableDef,
    schema: &mpedb_types::Schema,
    allow_new: bool,
    allow_old: bool,
    load_proc_blob: &mut dyn FnMut(&str) -> Result<Option<Vec<u8>>>,
) -> Result<StoredBody> {
    match body {
        mpedb_sql::TriggerBodySpec::Sql(sql) => {
            let _ = mpedb_sql::compile_trigger_body(sql, table, schema, allow_new, allow_old)?;
            Ok(StoredBody::Sql(sql.clone()))
        }
        mpedb_sql::TriggerBodySpec::Proc { name, arg_srcs } => {
            let blob = load_proc_blob(name)?.ok_or_else(|| {
                Error::Bind(format!(
                    "CREATE TRIGGER: no stored procedure `{name}` — define it first \
                     (`mpedb proc define`)"
                ))
            })?;
            let proc = mpedb_spell::ir::Proc::decode(&blob)?;
            if proc.argc as usize != arg_srcs.len() {
                return Err(Error::Bind(format!(
                    "CREATE TRIGGER: procedure `{name}` takes {} argument(s), \
                     the trigger passes {}",
                    proc.argc,
                    arg_srcs.len()
                )));
            }
            for src in arg_srcs {
                let _ = mpedb_sql::compile_trigger_arg(src, table, allow_new, allow_old)?;
            }
            Ok(StoredBody::Proc {
                name: name.clone(),
                hash: *blake3::hash(&blob).as_bytes(),
                arg_srcs: arg_srcs.clone(),
            })
        }
    }
}

impl Database {
    /// Scan `trigger/*`, decode each record, compile the fireable ones against
    /// the live schema. Skips triggers whose target table was dropped (their
    /// record may linger — harmless, never fires). Called only on a
    /// `schema_gen` change (see [`Database::trigger_set`]).
    fn build_trigger_set(&self) -> Result<TriggerSet> {
        let bundle = self.engine.schema();
        let schema = &bundle.schema;
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan_range(TRIGGER_PREFIX, TRIGGER_PREFIX_END);
        r.finish()?;
        let mut set = TriggerSet::empty();
        for (subkey, value) in scan? {
            if !subkey.starts_with(TRIGGER_PREFIX) {
                continue;
            }
            let st = StoredTrigger::decode(&value)?;
            // Row-binding availability by event (DESIGN-TRIGGERS §1).
            let (allow_new, allow_old) = match st.event {
                TrgEvent::Insert => (true, false),
                TrgEvent::Update => (true, true),
                TrgEvent::Delete => (false, true),
            };
            let table = match schema.table(st.table_id) {
                Some(t) if !t.dead => t,
                _ => continue, // target dropped: orphan record, never fires
            };
            let body = match &st.body {
                StoredBody::Sql(sql) => TriggerBody::Sql(mpedb_sql::compile_trigger_body(
                    sql, table, schema, allow_new, allow_old,
                )?),
                StoredBody::Proc { name, hash, arg_srcs } => {
                    let mut args = Vec::with_capacity(arg_srcs.len());
                    for src in arg_srcs {
                        args.push(mpedb_sql::compile_trigger_arg(
                            src, table, allow_new, allow_old,
                        )?);
                    }
                    TriggerBody::Spell(SpellTriggerBody {
                        args,
                        ready: self.resolve_spell_body(name, hash),
                    })
                }
            };
            let when = match &st.when_src {
                Some(src) => Some(mpedb_sql::compile_trigger_when(
                    src, table, allow_new, allow_old,
                )?),
                None => None,
            };
            // The (timing, event) bucket this trigger fires from.
            let bucket = match (st.timing, st.event) {
                (TrgTiming::Before, TrgEvent::Insert) => &mut set.before_insert,
                (TrgTiming::Before, TrgEvent::Update) => &mut set.before_update,
                (TrgTiming::Before, TrgEvent::Delete) => &mut set.before_delete,
                (TrgTiming::After, TrgEvent::Insert) => &mut set.after_insert,
                (TrgTiming::After, TrgEvent::Update) => &mut set.after_update,
                (TrgTiming::After, TrgEvent::Delete) => &mut set.after_delete,
            };
            bucket.entry(st.table_id).or_default().push(CompiledTrigger {
                name: st.name,
                body,
                update_of: st.update_of,
                when,
            });
        }
        // Stable, deterministic fire order per table. Creation order is not
        // tracked in v1, so name order stands in for it (documented).
        for bucket in [
            &mut set.before_insert,
            &mut set.before_update,
            &mut set.before_delete,
            &mut set.after_insert,
            &mut set.after_update,
            &mut set.after_delete,
        ] {
            for v in bucket.values_mut() {
                v.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        Ok(set)
    }

    /// Resolve an `EXECUTE PROCEDURE` body to fire-readiness: load the PINNED
    /// procedure blob by content hash and pre-resolve every embedded plan.
    /// Failures return `Err(message)` — the poisoned-body containment: only
    /// firing statements see the error, and the message says how to repair.
    fn resolve_spell_body(
        &self,
        name: &str,
        hash: &[u8; 32],
    ) -> std::result::Result<SpellReady, String> {
        let proc = self
            .load_trigger_proc(hash)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "trigger procedure `{name}` (hash {}) is no longer stored — \
                     re-define the procedure and re-create the trigger",
                    mpedb_spell::hash::ProcHash(*hash)
                )
            })?;
        let mut plans = HashMap::with_capacity(proc.plans.len());
        for pr in &proc.plans {
            let plan = self.resolve_spell_plan(&pr.hash).map_err(|e| {
                format!(
                    "trigger procedure `{name}`: embedded statement (plan {}) \
                     cannot be resolved: {e} — re-define the procedure and \
                     re-create the trigger",
                    pr.hash
                )
            })?;
            // A trigger fires with no session: `current_setting()` context in
            // an embedded statement has nowhere to resolve from. Refuse at
            // build so the gap is a named poison, not a fire-time surprise.
            if let Some(key) = plan
                .context_keys
                .iter()
                .find(|k| k.as_str() != mpedb_sql::STATEMENT_INSTANT_KEY)
            {
                return Err(format!(
                    "trigger procedure `{name}`: embedded statement uses \
                     current_setting('{key}'), which needs a session and is \
                     not available inside a trigger"
                ));
            }
            plans.insert(pr.hash.0, plan);
        }
        Ok(SpellReady { proc, plans })
    }

    /// Load a stored PROCEDURE blob by content hash (`proch/<hash>`), blake3-
    /// verified, cached in the shared spell cache (hash-keyed ⇒ immutable ⇒
    /// never stale). Unlike [`Database::load_proc_by_hash`] (the stored-
    /// FUNCTION loader) this permits database operations — running SQL is the
    /// point of a trigger procedure.
    fn load_trigger_proc(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<Arc<mpedb_spell::ir::Proc>>> {
        if let Some(p) = self.spell_cache.read().expect(POISON).get(hash) {
            return Ok(Some(p.clone()));
        }
        let blob_key = crate::sys_record_subkey(crate::NS_PROC_HASH, hash)?;
        let r = self.engine.begin_read()?;
        let blob = r.sys_get(&blob_key);
        r.finish()?;
        let Some(blob) = blob? else {
            return Ok(None);
        };
        if *blake3::hash(&blob).as_bytes() != *hash {
            return Err(Error::Corrupt(
                "stored procedure blob does not match its hash".into(),
            ));
        }
        let proc = Arc::new(mpedb_spell::ir::Proc::decode(&blob)?);
        self.spell_cache
            .write()
            .expect(POISON)
            .insert(*hash, proc.clone());
        Ok(Some(proc))
    }

    /// Resolve one embedded plan hash: the shared cache/registry first; on
    /// `PlanInvalidated` (the blob predates a schema change) re-prepare from
    /// the registry record's stored SQL against the LIVE schema — the same
    /// re-derivation a view body gets, so a trigger procedure survives every
    /// schema change its statements still bind under.
    fn resolve_spell_plan(&self, hash: &mpedb_types::PlanHash) -> Result<Arc<CompiledPlan>> {
        match self.cached_or_load_tls(hash) {
            Ok(p) => Ok(p),
            Err(Error::PlanInvalidated) => {
                let subkey = crate::registry::plan_subkey(hash);
                let r = self.engine.begin_read()?;
                let record = r.sys_get(&subkey);
                r.finish()?;
                let Some(record) = record? else {
                    return Err(Error::UnknownPlan(*hash));
                };
                let sql = crate::registry::parse_record(&record)
                    .ok_or(Error::UnknownPlan(*hash))?
                    .sql
                    .to_owned();
                let (plan, _explain) = self.compile_maybe_explain(&sql)?;
                Ok(Arc::new(plan))
            }
            Err(e) => Err(e),
        }
    }

    /// The current [`TriggerSet`], rebuilt only when a DDL commit moved
    /// `schema_gen` (here or in another process). Identical freshness contract
    /// to the plan cache, so the two can never disagree within one statement.
    pub(crate) fn trigger_set(&self) -> Result<Arc<TriggerSet>> {
        let gen = self.engine.schema().schema_gen;
        {
            let g = self.trigger_cache.read().expect(POISON);
            if let Some((cached_gen, set)) = &*g {
                if *cached_gen == gen {
                    return Ok(set.clone());
                }
            }
        }
        let set = Arc::new(self.build_trigger_set()?);
        *self.trigger_cache.write().expect(POISON) = Some((gen, set.clone()));
        Ok(set)
    }

    /// Does `table` carry ANY trigger — `BEFORE` or `AFTER`, insert/update/
    /// delete? Used to keep the optimistic blind-apply path off tables that must
    /// run the executor to fire: the blind path never calls the executor, so it
    /// would skip every trigger (before OR after). On error, answers `true`
    /// (conservative: force the safe executor path).
    pub(crate) fn table_has_trigger(&self, table: u32) -> bool {
        self.trigger_set()
            .map(|s| {
                s.before_insert.contains_key(&table)
                    || s.before_update.contains_key(&table)
                    || s.before_delete.contains_key(&table)
                    || s.after_insert.contains_key(&table)
                    || s.after_update.contains_key(&table)
                    || s.after_delete.contains_key(&table)
            })
            .unwrap_or(true)
    }

    /// `CREATE TRIGGER …` (DESIGN-TRIGGERS §3.1). Resolves the target table,
    /// refuses the not-yet-supported forms by name, compile-checks the body +
    /// `WHEN` (define-time loudness), then stores the record and bumps
    /// `schema_gen` in one commit — the established view/policy DDL tail.
    pub(crate) fn apply_create_trigger(
        &self,
        spec: mpedb_sql::CreateTriggerSpec,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let table_id = bundle.schema.table_id(&spec.table).ok_or_else(|| {
            Error::Bind(format!("CREATE TRIGGER: no such table `{}`", spec.table))
        })?;
        let table = bundle.schema.table(table_id).expect("table_id resolved");

        // Timing: BEFORE and AFTER both fire (DESIGN-TRIGGERS stage 3). `NEW`
        // stays read-only either way (no mutation syntax), so BEFORE runs its
        // DML body before the row mutation but cannot rewrite the row.
        let timing = match spec.timing {
            mpedb_sql::TriggerTiming::Before => TrgTiming::Before,
            mpedb_sql::TriggerTiming::After => TrgTiming::After,
        };
        // Map the event, derive `NEW`/`OLD` availability (DESIGN-TRIGGERS §1),
        // and resolve any `UPDATE OF <cols>` names to column indices — a define-
        // time error if a named column does not exist.
        let (event, allow_new, allow_old, update_of) = match &spec.event {
            mpedb_sql::TriggerEvent::Insert => (TrgEvent::Insert, true, false, Vec::new()),
            mpedb_sql::TriggerEvent::Update { of } => {
                let mut cols = Vec::with_capacity(of.len());
                for name in of {
                    let idx = table.column_index(name).ok_or_else(|| {
                        Error::Bind(format!(
                            "CREATE TRIGGER: UPDATE OF: no such column `{name}` on table `{}`",
                            spec.table
                        ))
                    })?;
                    cols.push(idx);
                }
                (TrgEvent::Update, true, true, cols)
            }
            mpedb_sql::TriggerEvent::Delete => (TrgEvent::Delete, false, true, Vec::new()),
        };

        // Compile-check at define time so a broken trigger is rejected at CREATE,
        // not discovered at fire time (DESIGN-TRIGGERS §3.1).
        let stored_body = build_stored_body(
            &spec.body,
            table,
            &bundle.schema,
            allow_new,
            allow_old,
            &mut |name| self.sys_record_get(crate::NS_PROC, name.as_bytes()),
        )?;
        if let Some(when_src) = &spec.when_src {
            let _ = mpedb_sql::compile_trigger_when(when_src, table, allow_new, allow_old)?;
        }

        let key = trigger_key(&spec.name);
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        if resolve_trigger_key(&mut w, &spec.name)?.is_some() {
            w.abort();
            if spec.if_not_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!(
                "CREATE TRIGGER: trigger `{}` already exists",
                spec.name
            )));
        }
        let record = StoredTrigger {
            name: spec.name.clone(),
            table_id,
            timing,
            event,
            update_of,
            when_src: spec.when_src.clone(),
            body: stored_body,
        }
        .encode();
        let res = (|| {
            w.sys_put(&key, &record)?;
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
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `DROP TRIGGER [IF EXISTS] <name>`.
    pub(crate) fn apply_drop_trigger(&self, name: &str, if_exists: bool) -> Result<ExecResult> {
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let found = resolve_trigger_key(&mut w, name)?;
        let key = found.clone().unwrap_or_else(|| trigger_key(name));
        if found.is_none() {
            w.abort();
            if if_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!("DROP TRIGGER: no such trigger `{name}`")));
        }
        let res = (|| {
            w.sys_delete(&key)?;
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
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }
}

/// `CREATE TRIGGER` on an open write txn (WriteSession path — no nested commit).
pub(crate) fn create_trigger_on_txn(
    w: &mut WriteTxn<'_>,
    db: &Database,
    spec: mpedb_sql::CreateTriggerSpec,
) -> Result<()> {
    let bundle = w.schema_bundle();
    let table_id = bundle.schema.table_id(&spec.table).ok_or_else(|| {
        Error::Bind(format!("CREATE TRIGGER: no such table `{}`", spec.table))
    })?;
    let table = bundle
        .schema
        .table(table_id)
        .expect("table_id resolved")
        .clone();

    let timing = match spec.timing {
        mpedb_sql::TriggerTiming::Before => TrgTiming::Before,
        mpedb_sql::TriggerTiming::After => TrgTiming::After,
    };
    let (event, allow_new, allow_old, update_of) = match &spec.event {
        mpedb_sql::TriggerEvent::Insert => (TrgEvent::Insert, true, false, Vec::new()),
        mpedb_sql::TriggerEvent::Update { of } => {
            let mut cols = Vec::with_capacity(of.len());
            for name in of {
                let idx = table.column_index(name).ok_or_else(|| {
                    Error::Bind(format!(
                        "CREATE TRIGGER: UPDATE OF: no such column `{name}` on table `{}`",
                        spec.table
                    ))
                })?;
                cols.push(idx);
            }
            (TrgEvent::Update, true, true, cols)
        }
        mpedb_sql::TriggerEvent::Delete => (TrgEvent::Delete, false, true, Vec::new()),
    };

    let stored_body = build_stored_body(
        &spec.body,
        &table,
        &bundle.schema,
        allow_new,
        allow_old,
        // Resolve the procedure through THIS txn, so a procedure defined
        // earlier in the same uncommitted transaction is visible.
        &mut |name| {
            let k = crate::sys_record_subkey(crate::NS_PROC, name.as_bytes())?;
            w.sys_get(&k)
        },
    )?;
    if let Some(when_src) = &spec.when_src {
        let _ = mpedb_sql::compile_trigger_when(when_src, &table, allow_new, allow_old)?;
    }
    // Silence unused: db is available for future cache hooks; schema_gen bump
    // invalidates the trigger cache on commit.
    let _ = db;

    let key = trigger_key(&spec.name);
    if resolve_trigger_key(w, &spec.name)?.is_some() {
        if spec.if_not_exists {
            return Ok(());
        }
        return Err(Error::Bind(format!(
            "CREATE TRIGGER: trigger `{}` already exists",
            spec.name
        )));
    }
    let record = StoredTrigger {
        name: spec.name.clone(),
        table_id,
        timing,
        event,
        update_of,
        when_src: spec.when_src.clone(),
        body: stored_body,
    }
    .encode();
    w.sys_put(&key, &record)?;
    w.bump_schema_gen();
    Ok(())
}

/// `DROP TRIGGER` on an open write txn.
pub(crate) fn drop_trigger_on_txn(
    w: &mut WriteTxn<'_>,
    name: &str,
    if_exists: bool,
) -> Result<()> {
    let found = resolve_trigger_key(w, name)?;
    let key = found.clone().unwrap_or_else(|| trigger_key(name));
    if found.is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::Bind(format!("DROP TRIGGER: no such trigger `{name}`")));
    }
    w.sys_delete(&key)?;
    w.bump_schema_gen();
    Ok(())
}

/// Every stored trigger as `(name, tbl_name, create_sql)` for `sqlite_master`
/// / iterdump. `create_sql` is reconstructed from the catalog record so a dump
/// can re-create the trigger (verbatim storage is a separate C-API path).
impl Database {
    pub fn list_triggers(&self) -> Result<Vec<(String, String, String)>> {
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan_range(TRIGGER_PREFIX, TRIGGER_PREFIX_END);
        let bundle = self.engine.schema();
        r.finish()?;
        triggers_from_scan(scan?, &bundle.schema)
    }
}

/// Triggers visible through an open write txn (mid-transaction iterdump).
pub(crate) fn list_triggers_on_txn(
    w: &mut WriteTxn<'_>,
    bundle: &mpedb_core::engine::SchemaBundle,
) -> Result<Vec<(String, String, String)>> {
    let scan = w.sys_scan_range(TRIGGER_PREFIX, TRIGGER_PREFIX_END)?;
    triggers_from_scan(scan, &bundle.schema)
}

fn triggers_from_scan(
    scan: Vec<(Vec<u8>, Vec<u8>)>,
    schema: &mpedb_types::Schema,
) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    for (subkey, value) in scan {
        if !subkey.starts_with(TRIGGER_PREFIX) {
            continue;
        }
        let Ok(st) = StoredTrigger::decode(&value) else {
            continue;
        };
        let (tbl, col_names) = match schema.table(st.table_id) {
            Some(t) => (
                t.name.clone(),
                t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            ),
            None => (format!("table_{}", st.table_id), Vec::new()),
        };
        let sql = reconstruct_create_trigger(&st, &tbl, &col_names);
        out.push((st.name, tbl, sql));
    }
    Ok(out)
}

fn reconstruct_create_trigger(st: &StoredTrigger, table: &str, col_names: &[String]) -> String {
    let timing = match st.timing {
        TrgTiming::Before => "BEFORE",
        TrgTiming::After => "AFTER",
    };
    let event = match st.event {
        TrgEvent::Insert => "INSERT".to_string(),
        TrgEvent::Delete => "DELETE".to_string(),
        TrgEvent::Update => {
            if st.update_of.is_empty() {
                "UPDATE".to_string()
            } else {
                let cols: Vec<&str> = st
                    .update_of
                    .iter()
                    .filter_map(|&i| col_names.get(i as usize).map(|s| s.as_str()))
                    .collect();
                if cols.is_empty() {
                    "UPDATE".to_string()
                } else {
                    format!("UPDATE OF {}", cols.join(", "))
                }
            }
        }
    };
    let when = st
        .when_src
        .as_ref()
        .map(|w| format!(" WHEN {w}"))
        .unwrap_or_default();
    match &st.body {
        // Body is stored without the BEGIN/END wrapper the parser strips.
        StoredBody::Sql(sql) => format!(
            "CREATE TRIGGER \"{}\" {timing} {event} ON \"{table}\"{when} BEGIN {sql} END",
            st.name
        ),
        StoredBody::Proc { name, arg_srcs, .. } => format!(
            "CREATE TRIGGER \"{}\" {timing} {event} ON \"{table}\"{when} \
             EXECUTE PROCEDURE {name}({})",
            st.name,
            arg_srcs.join(", ")
        ),
    }
}

/// Delete every trigger record targeting `table_id`, inside the caller's write
/// txn — the `DROP TABLE` cascade (DESIGN-TRIGGERS §3.1). A dropped table's
/// triggers are dead; removing them keeps the catalog clean.
pub(crate) fn cascade_drop_triggers(w: &mut WriteTxn<'_>, table_id: u32) -> Result<()> {
    let records = w.sys_scan_range(TRIGGER_PREFIX, TRIGGER_PREFIX_END)?;
    for (subkey, value) in records {
        if !subkey.starts_with(TRIGGER_PREFIX) {
            continue;
        }
        if let Ok(st) = StoredTrigger::decode(&value) {
            if st.table_id == table_id {
                w.sys_delete(&subkey)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoredTrigger {
        StoredTrigger {
            name: "audit_ins".into(),
            table_id: 3,
            timing: TrgTiming::After,
            event: TrgEvent::Insert,
            update_of: vec![1, 4],
            when_src: Some("NEW.total > 100".into()),
            body: StoredBody::Sql("INSERT INTO audit (oid) VALUES (NEW.id)".into()),
        }
    }

    fn sample_proc() -> StoredTrigger {
        StoredTrigger {
            name: "enrich_upd".into(),
            table_id: 7,
            timing: TrgTiming::Before,
            event: TrgEvent::Update,
            update_of: Vec::new(),
            when_src: None,
            body: StoredBody::Proc {
                name: "enrich".into(),
                hash: [0xAB; 32],
                arg_srcs: vec!["NEW.id".into(), "coalesce(OLD.tag, 'none')".into()],
            },
        }
    }

    #[test]
    fn record_round_trips() {
        let t = sample();
        assert_eq!(StoredTrigger::decode(&t.encode()).unwrap(), t);
        // No-WHEN, no update_of variant.
        let t2 = StoredTrigger {
            update_of: Vec::new(),
            when_src: None,
            ..sample()
        };
        assert_eq!(StoredTrigger::decode(&t2.encode()).unwrap(), t2);
        // The stage-5 procedure body, including a zero-argument call.
        let t3 = sample_proc();
        assert_eq!(StoredTrigger::decode(&t3.encode()).unwrap(), t3);
        let t4 = StoredTrigger {
            body: StoredBody::Proc {
                name: "hook".into(),
                hash: [1; 32],
                arg_srcs: Vec::new(),
            },
            ..sample_proc()
        };
        assert_eq!(StoredTrigger::decode(&t4.encode()).unwrap(), t4);
    }

    #[test]
    fn decode_is_corrupt_never_panic_at_every_truncation() {
        for bytes in [sample().encode(), sample_proc().encode()] {
            for n in 0..bytes.len() {
                // Every prefix shorter than the whole must be Corrupt, not a panic.
                assert!(
                    StoredTrigger::decode(&bytes[..n]).is_err(),
                    "prefix of len {n} decoded"
                );
            }
            // Trailing garbage is rejected too.
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(StoredTrigger::decode(&extra).is_err());
        }
    }

    #[test]
    fn decode_rejects_bad_tags() {
        let mut b = sample().encode();
        b[4] = 9; // corrupt the format low byte
        assert!(matches!(StoredTrigger::decode(&b), Err(Error::Corrupt(_))));
    }
}
