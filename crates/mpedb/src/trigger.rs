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
//! `NEW` is read-only (no `NEW.col := …` mutation syntax exists), and `RAISE`
//! is not expressible in the DML-only body grammar, so BEFORE cannot veto a
//! write yet. `INSTEAD OF`, `FOR EACH STATEMENT`, and `EXECUTE PROCEDURE` are
//! named refusals at `CREATE`.

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
// tag 1 (Proc / PySpell body) is reserved for DESIGN-TRIGGERS stage 5.

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
    pub body_sql: String,
}

impl StoredTrigger {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + self.name.len() + self.body_sql.len());
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
        b.push(BODY_TAG_SQL);
        put_str(&mut b, &self.body_sql);
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
        let body_sql = match take(bytes, &mut p, 1)?[0] {
            BODY_TAG_SQL => take_str(bytes, &mut p)?,
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
            body_sql,
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

// ---------------------------------------------------------- compiled fire-set

/// One compiled trigger ready to fire: the body statements (each plan's leading
/// parameters are the `NEW`/`OLD` columns named by its row-slot map, run in body
/// order on the same txn), an optional `WHEN` guard (its own program over its own
/// row-slot map), and the `UPDATE OF` column set. See DESIGN-TRIGGERS §3.4 and
/// `mpedb_sql::compile_trigger_body`.
pub(crate) struct CompiledTrigger {
    pub name: String,
    /// Body statements in declaration order, each with its own `NEW`/`OLD`
    /// row-slot map (DESIGN-TRIGGERS stage 3 multi-statement bodies).
    pub body: Vec<(CompiledPlan, RowMap)>,
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
            let body =
                mpedb_sql::compile_trigger_body(&st.body_sql, table, schema, allow_new, allow_old)?;
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
        let _ = mpedb_sql::compile_trigger_body(
            &spec.body_sql,
            table,
            &bundle.schema,
            allow_new,
            allow_old,
        )?;
        if let Some(when_src) = &spec.when_src {
            let _ = mpedb_sql::compile_trigger_when(when_src, table, allow_new, allow_old)?;
        }

        let key = trigger_key(&spec.name);
        let mut w = self.engine.begin_write()?;
        if matches!(w.sys_get(&key), Ok(Some(_))) {
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
            body_sql: spec.body_sql.clone(),
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
        let key = trigger_key(name);
        let mut w = self.engine.begin_write()?;
        if !matches!(w.sys_get(&key), Ok(Some(_))) {
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
            body_sql: "INSERT INTO audit (oid) VALUES (NEW.id)".into(),
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
    }

    #[test]
    fn decode_is_corrupt_never_panic_at_every_truncation() {
        let bytes = sample().encode();
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

    #[test]
    fn decode_rejects_bad_tags() {
        let mut b = sample().encode();
        b[4] = 9; // corrupt the format low byte
        assert!(matches!(StoredTrigger::decode(&b), Err(Error::Corrupt(_))));
    }
}
