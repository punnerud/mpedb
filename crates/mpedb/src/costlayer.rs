//! The cost layer's stored state — stage M5 of the generic-solver program
//! (design/DESIGN-MPEE-GENERAL.md §9): tunables, the cost-policy spell, and
//! the readable statistics report.
//!
//! Everything here lives IN THE FILE, deliberately — the plan drafted a
//! `[mpee]` config section, but stored state is strictly better on the axis
//! that matters: **coherence**. Cost inputs decide chosen plans, chosen plans
//! are content-hashed, and two attached processes must never price
//! differently. A config value can drift between attachers; a sys-keyspace
//! record cannot — every process reads the same bytes off the same snapshot,
//! and changes bump `schema_gen` so cached plans re-prepare everywhere. This
//! is also exactly what the user asked: cost analysis and adjustment must not
//! be locked to config.
//!
//! Three pieces:
//! - **Tunables** (`tune/current`): named switches on the cost calculator.
//!   v1 ships `ndv_discount` (the stage-A per-index discount, default on).
//! - **The cost-policy spell** (`costpolicy/current` → a stored PySpell): a
//!   PROGRAMMABLE adjustment that runs at prepare inside the CostSource seam.
//!   It receives `(kind, table, index_no, bucket, rows_bucket, archetype)` —
//!   statistics AND the model's level-0 claim — and returns the bucket to
//!   use. Stored + content-hashed + gen-gated + budgeted ⇒ deterministic and
//!   identical in every process: the same coherence argument as the
//!   tunables, with code instead of switches.
//! - **The statistics report** (`Database::stats_report`): the READ side —
//!   what the engine believes (row counts, NDV buckets, analyze state), as an
//!   API + CLI. (SQL-queryable `mpedb_stats` joins `mpedb_operators` in
//!   waiting for the synthetic-table seam.)
//!
//! A policy that cannot even run FAILS the prepare (a probe call per compile,
//! naming the policy). A policy that errors on a SPECIFIC input degrades that
//! one decision to no-discount — deterministically, because every process
//! runs the same stored spell on the same inputs, so pricing never splits.

use std::sync::Arc;

use mpedb_spell::ir::Proc;
use mpedb_types::{Error, Result, Value};

pub const NS_TUNE: &str = "tune";
pub const NS_COST_POLICY: &str = "costpolicy";
const KEY: &[u8] = b"current";

/// The engine's named switches — cost-calculator knobs and behavioural ones
/// that must be COHERENT across attached processes (a per-connection setting
/// would let the same statement have different effects in different
/// processes). Strict parse: an unknown name is a typo describing a different
/// engine, and refuses with the known set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunables {
    /// Stage A's per-index distinct-key discount
    /// (`bucket(rows) − bucket(ndv)` on fully-pinned non-partial indexes).
    pub ndv_discount: bool,
    /// DESIGN-TRIGGERS §4.4: may a trigger's cascade re-enter a trigger that
    /// is already active? Default OFF (sqlite's default): while trigger T
    /// runs, T does not fire again — direct or via a cycle — which is what
    /// stops the classic `AFTER INSERT ON t … INSERT INTO t` runaway without
    /// an error. ON restores full recursion under the depth cap and the work
    /// meter.
    pub recursive_triggers: bool,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            ndv_discount: true,
            recursive_triggers: false,
        }
    }
}

impl Tunables {
    fn encode(&self) -> Vec<u8> {
        format!(
            "ndv_discount={}\nrecursive_triggers={}\n",
            self.ndv_discount, self.recursive_triggers
        )
        .into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Tunables> {
        let mut t = Tunables::default();
        for line in String::from_utf8_lossy(bytes).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parse_bool = |name: &str, v: &str| -> Result<bool> {
                match v {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    other => Err(Error::Corrupt(format!(
                        "tunable {name} has non-boolean value `{other}`"
                    ))),
                }
            };
            match line.split_once('=') {
                Some(("ndv_discount", v)) => t.ndv_discount = parse_bool("ndv_discount", v)?,
                Some(("recursive_triggers", v)) => {
                    t.recursive_triggers = parse_bool("recursive_triggers", v)?
                }
                _ => {
                    return Err(Error::Corrupt(format!(
                        "unknown tunable line `{line}` — known: ndv_discount, \
                         recursive_triggers"
                    )))
                }
            }
        }
        Ok(t)
    }

    pub fn parse_assignment(&mut self, assignment: &str) -> Result<()> {
        let parse_bool = |name: &str, v: &str| -> Result<bool> {
            match v {
                "true" => Ok(true),
                "false" => Ok(false),
                other => Err(Error::Unsupported(format!(
                    "{name} takes true|false, got `{other}`"
                ))),
            }
        };
        match assignment.split_once('=') {
            Some(("ndv_discount", v)) => {
                self.ndv_discount = parse_bool("ndv_discount", v)?;
                Ok(())
            }
            Some(("recursive_triggers", v)) => {
                self.recursive_triggers = parse_bool("recursive_triggers", v)?;
                Ok(())
            }
            _ => Err(Error::Unsupported(format!(
                "unknown tunable `{assignment}` — known: ndv_discount=true|false, \
                 recursive_triggers=true|false"
            ))),
        }
    }
}

/// One index's statistics, as the report shows them.
#[derive(Debug, Clone)]
pub struct StatLine {
    pub table: String,
    pub index_no: u32,
    pub columns: Vec<String>,
    pub rows: u64,
    pub rows_bucket: u32,
    /// `None` = never analyzed (or stale after DDL).
    pub ndv_bucket: Option<u32>,
}

impl crate::Database {
    // ---------------- tunables ----------------

    pub fn tunables(&self) -> Result<Tunables> {
        match self.sys_record_get(NS_TUNE, KEY)? {
            None => Ok(Tunables::default()),
            Some(b) => Tunables::decode(&b),
        }
    }

    /// Apply `name=value` and store — with the schema-generation bump that
    /// makes every process's cached plans re-prepare under the new pricing.
    pub fn set_tunable(&self, assignment: &str) -> Result<Tunables> {
        let mut t = self.tunables()?;
        t.parse_assignment(assignment)?;
        self.put_gen_bumped(NS_TUNE, &t.encode())?;
        Ok(t)
    }

    // ---------------- the cost-policy spell ----------------

    /// Install (or replace) the cost policy: a PySpell
    /// `def policy(kind, table, index_no, bucket, rows_bucket, archetype):`
    /// returning the bucket to use. `kind` is the channel — v1 calls it with
    /// `"ndv"` (the stage-A discount channel); more channels register here as
    /// they exist, which is the whole reason the argument exists.
    pub fn set_cost_policy(&self, lang: crate::spellfn::SpellLang, source: &str) -> Result<String> {
        let skeleton = match lang {
            crate::spellfn::SpellLang::Python => mpedb_spell::py::compile(source)?,
            crate::spellfn::SpellLang::Rust => mpedb_spell::rs::compile(source)?,
        };
        if !skeleton.calls.is_empty() {
            return Err(Error::Unsupported(
                "a cost policy cannot run SQL — it adjusts pricing, it does not read data"
                    .into(),
            ));
        }
        if skeleton.argc != 6 {
            return Err(Error::Unsupported(format!(
                "a cost policy takes (kind, table, index_no, bucket, rows_bucket, archetype) \
                 — 6 arguments, the definition takes {}",
                skeleton.argc
            )));
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
        // Blob content-addressed beside functions/operators; the policy
        // record binds "the current policy" to a hash.
        let blob_key = crate::sys_record_subkey(crate::spellfn::NS_FUNC_HASH, &hash.0)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&blob_key, &blob)?;
            let rec_key = crate::sys_record_subkey(NS_COST_POLICY, KEY)?;
            w.sys_put(&rec_key, &hash.0)?;
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
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(hash.to_string())
    }

    pub fn drop_cost_policy(&self) -> Result<bool> {
        let rec_key = crate::sys_record_subkey(NS_COST_POLICY, KEY)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let existed = match w.sys_delete(&rec_key) {
            Ok(x) => x,
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

    /// The active policy's spell, on one snapshot. Missing blob or a blob
    /// smuggling SQL fails CLOSED (Corrupt), not open.
    pub(crate) fn load_cost_policy(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
    ) -> Result<Option<Arc<Proc>>> {
        let rec_key = crate::sys_record_subkey(NS_COST_POLICY, KEY)?;
        let Some(hash_bytes) = r.sys_get(&rec_key)? else {
            return Ok(None);
        };
        let hash: [u8; 32] = hash_bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Corrupt("cost-policy record is not a 32-byte hash".into()))?;
        self.load_proc_by_hash(r, &hash)?
            .map(Some)
            .ok_or_else(|| Error::Corrupt("cost-policy names a spell blob that is missing".into()))
    }

    // ---------------- the read side ----------------

    /// What the engine believes about every analyzable index — the READ half
    /// of "cost analysis must not be locked away": row counts, buckets, and
    /// whether analyze() has run.
    pub fn stats_report(&self) -> Result<Vec<StatLine>> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let r = self.engine.begin_read()?;
        let mut out = Vec::new();
        for t in bundle.schema.tables.iter().filter(|t| !t.dead) {
            let rows = r.row_count(t.id).unwrap_or(0);
            for (pos, ix) in t.indexes.iter().enumerate().take(63) {
                let index_no = pos as u32 + 1;
                out.push(StatLine {
                    table: t.name.clone(),
                    index_no,
                    columns: ix
                        .columns
                        .iter()
                        .map(|&c| {
                            t.columns
                                .get(c as usize)
                                .map(|col| col.name.clone())
                                .unwrap_or_else(|| format!("#{c}"))
                        })
                        .collect(),
                    rows,
                    rows_bucket: crate::stats::bucket(rows),
                    ndv_bucket: crate::ndv_bucket_from(&r, &bundle.schema, t.id, index_no),
                });
            }
        }
        r.finish()?;
        Ok(out)
    }

    // ---------------- shared write helper ----------------

    fn put_gen_bumped(&self, ns: &str, value: &[u8]) -> Result<()> {
        let key = crate::sys_record_subkey(ns, KEY)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&key, value)?;
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
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(())
    }
}

/// Run the policy over one pricing decision. `bucket` is the base the engine
/// computed; the return is the bucket to use, clamped to the bucket domain.
/// Every failure names the policy — a broken policy must fail the prepare,
/// not silently split pricing across processes.
pub(crate) fn apply_policy(
    proc: &Proc,
    kind: &str,
    table: &str,
    index_no: u32,
    bucket: u32,
    rows_bucket: u32,
    archetype: &str,
) -> Result<u32> {
    let args = [
        Value::Text(kind.to_string()),
        Value::Text(table.to_string()),
        Value::Int(index_no as i64),
        Value::Int(bucket as i64),
        Value::Int(rows_bucket as i64),
        Value::Text(archetype.to_string()),
    ];
    match crate::spellfn::call_spell_fn(proc, &args) {
        Ok(Value::Int(v)) if (0..=64).contains(&v) => Ok(v as u32),
        Ok(Value::Int(v)) => Err(Error::Unsupported(format!(
            "cost policy returned bucket {v}, outside 0..=64"
        ))),
        Ok(other) => Err(Error::Unsupported(format!(
            "cost policy returned {} — a bucket must be an integer",
            other.type_name()
        ))),
        Err(e) => Err(Error::Unsupported(format!("cost policy failed: {e}"))),
    }
}
