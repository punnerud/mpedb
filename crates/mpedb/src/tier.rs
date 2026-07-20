//! Cold-data tiering v1 (#78) — DRAIN rows from a hot database into a cold
//! `.mpedb` file (design/DESIGN-SYNC-TIERING.md, staging step 3's row/`drain`
//! mode cut down to a one-shot operation; read-back rides `ATTACH` (#51)).
//!
//! **The ordering is load-bearing** (doc §2, the same discipline as the
//! freelist fixpoint): a row is reclaimed from hot only after the copy in cold
//! is COMMITTED and read back verified. Per batch:
//!
//! 1. open a hot write transaction (the writer lock also elects this process
//!    the single drainer for that file — doc §5) and SELECT up to `batch`
//!    rows matching the predicate;
//! 2. copy them into cold in one cold write transaction. A row whose PK is
//!    already in cold with IDENTICAL content is recognized as an earlier
//!    drain's landing (crash between the two commits) and not re-inserted; a
//!    DIFFERENT row under the same PK is an explicit conflict error, never an
//!    overwrite — the cold copy may be an archived predecessor of a reused
//!    key, and guessing would destroy one of the two rows;
//! 3. commit cold, then re-read every copied row from cold and compare
//!    bit-exactly (the doc's push-verify-reclaim);
//! 4. only then delete the rows from hot and commit hot.
//!
//! SIGKILL at any instant therefore leaves every row in hot, in cold, or in
//! BOTH — never in neither. Re-running the same drain converges: identical
//! duplicates are counted `reconciled` and only the hot delete remains.
//! `mpedb tier crash` (the CLI harness) SIGKILLs a drainer at random instants
//! and asserts exactly this.
//!
//! Crash model: process death (SIGKILL), independent of durability mode —
//! commits live in the shared mmap. POWER-loss safety of the handoff
//! additionally requires the cold handle to run `durability = commit|wal`
//! (per-process open-time knob, as everywhere).
//!
//! v1 refusals (by name, never silently wrong): FTS tables, databases with
//! RLS policies (the typed row path runs below policies, like the mirror
//! applier), hot DELETE / cold INSERT triggers on the table (they would not
//! fire), and hot/cold table definitions that differ in ANY way beyond the
//! table id. `reference` mode (stubs), remote cold stores, re-warm, and the
//! background drainer service are v2+ (doc §7).

use crate::{Database, ExecResult, WriteSession};
use mpedb_types::config::{default_extent_threshold, DEFAULT_MAX_JOIN_CELLS, DEFAULT_MAX_WORK_ROWS};
use mpedb_types::{
    Concurrency, Config, DbOptions, Durability, Error, FilePerms, Result, Schema, TableDef, Value,
};
use std::path::Path;

/// What one [`Database::tier_drain`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierReport {
    /// Rows copied to cold AND deleted from hot by this call.
    pub moved: u64,
    /// Rows found already in cold with identical content (an earlier drain's
    /// crash window) — deleted from hot only.
    pub reconciled: u64,
    /// Transaction pairs (bounded by `batch` rows each).
    pub batches: u64,
}

/// `"name"` with embedded quotes doubled — identifier quoting for the SQL the
/// drain composes from schema names.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Row equality for the drain's identity checks: exactly [`Value`] equality,
/// except floats compare by BIT PATTERN (storage roundtrips bits, and derived
/// `PartialEq` would call a drained `NaN` row "different" forever).
fn vals_eq(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Value::Float(f), Value::Float(g)) => f.to_bits() == g.to_bits(),
            _ => x == y,
        })
}

/// The live, non-dead definition of `table`, or a clear error naming `side`.
fn live_def(db: &Database, table: &str, side: &str) -> Result<TableDef> {
    db.engine.refresh_schema_if_stale()?;
    db.schema()
        .tables
        .iter()
        .find(|t| !t.dead && t.name == table)
        .cloned()
        .ok_or_else(|| Error::Bind(format!("tier: no table `{table}` in the {side} database")))
}

impl Database {
    /// Create a fresh cold `.mpedb` at `dest` carrying exactly this (hot)
    /// database's definition of `table` — the file a first
    /// [`Database::tier_drain`] targets. Refuses to touch an existing file.
    ///
    /// A crash DURING creation can leave a torn file that never reached the
    /// INIT_READY marker; opening it reports `Corrupt`. Nothing has left the
    /// hot database at that point — delete the torn file and re-run.
    pub fn tier_create_cold(
        &self,
        dest: &Path,
        table: &str,
        size_bytes: u64,
        durability: Durability,
    ) -> Result<Database> {
        let def = live_def(self, table, "hot")?;
        if def.kind.is_fts() {
            return Err(Error::Unsupported(
                "tier: FTS tables are not tierable in v1".into(),
            ));
        }
        if dest.exists() {
            return Err(Error::Config(format!(
                "tier: `{}` already exists; refusing to re-seed it (drain into it \
                 directly, or remove it first)",
                dest.display()
            )));
        }
        // `Schema::new` re-normalizes flags and re-derives `indexes`; both are
        // idempotent on a def that already went through them at hot's seed, so
        // the cold table is bit-identical (the drain re-checks this).
        let schema = Schema::new(vec![def])?;
        Database::open_with_config(Config {
            options: DbOptions {
                path: dest.to_path_buf(),
                size_bytes,
                max_readers: 64,
                durability,
                concurrency: Concurrency::Serial,
                extent_threshold: default_extent_threshold(),
                max_work_rows: DEFAULT_MAX_WORK_ROWS,
                max_join_cells: DEFAULT_MAX_JOIN_CELLS,
                require_policy: Default::default(),
                bare_group_by: Default::default(),
                perms: FilePerms {
                    mode: None,
                    owner: None,
                    group: None,
                },
            },
            schema,
        })
    }

    /// Drain every row of `table` matching `predicate` (a SQL boolean
    /// expression over the table's columns; `params` bind its placeholders)
    /// from this (hot) database into `cold`, at most `batch` rows per
    /// transaction pair. See the module docs for the protocol and its
    /// crash-safety argument. Idempotent: re-running the same drain after a
    /// crash converges without losing or duplicating rows.
    pub fn tier_drain(
        &self,
        cold: &Database,
        table: &str,
        predicate: &str,
        params: &[Value],
        batch: usize,
    ) -> Result<TierReport> {
        if batch == 0 {
            return Err(Error::Config("tier: batch must be >= 1".into()));
        }
        let hot_def = live_def(self, table, "hot")?;
        let cold_def = live_def(cold, table, "cold")?;
        if hot_def.kind.is_fts() {
            return Err(Error::Unsupported(
                "tier: FTS tables are not tierable in v1".into(),
            ));
        }
        // Rigid identity check, table id aside (ids are per-file positions).
        {
            let mut a = hot_def.clone();
            let mut b = cold_def.clone();
            a.id = 0;
            b.id = 0;
            if a != b {
                return Err(Error::Schema(format!(
                    "tier: table `{table}` differs between hot and cold (columns, \
                     types, PK, indexes and constraints must match exactly); \
                     drain into a file seeded from this database"
                )));
            }
        }
        // The drain runs on the typed row plane, BELOW RLS policies (like the
        // mirror applier) — refuse by name rather than silently not enforcing.
        if !self.require_policy.is_empty()
            || !self.load_policy_catalog()?.is_empty()
            || !cold.require_policy.is_empty()
            || !cold.load_policy_catalog()?.is_empty()
        {
            return Err(Error::Unsupported(
                "tier: drain is not supported on databases with RLS policies (v1)".into(),
            ));
        }
        // Typed-plane writes fire no triggers; refuse the ones that would be
        // skipped (hot-side DELETE, cold-side INSERT).
        let hot_trg = self.trigger_set()?;
        if hot_trg.before_delete.contains_key(&hot_def.id)
            || hot_trg.after_delete.contains_key(&hot_def.id)
        {
            return Err(Error::Unsupported(format!(
                "tier: hot table `{table}` has DELETE triggers, which a drain \
                 would not fire (v1)"
            )));
        }
        let cold_trg = cold.trigger_set()?;
        if cold_trg.before_insert.contains_key(&cold_def.id)
            || cold_trg.after_insert.contains_key(&cold_def.id)
        {
            return Err(Error::Unsupported(format!(
                "tier: cold table `{table}` has INSERT triggers, which a drain \
                 would not fire (v1)"
            )));
        }

        let pk_ords = hot_def.primary_key.clone();
        let cols_sql = hot_def
            .columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let tbl_sql = quote_ident(table);
        // Explicit column list (never `*`): an implicit-rowid table hides its
        // PK from `*`, and the drain must carry row IDENTITY, not just shape.
        let sel_sql = format!("SELECT {cols_sql} FROM {tbl_sql} WHERE ({predicate}) LIMIT {batch}");
        let pk_pred = pk_ords
            .iter()
            .enumerate()
            .map(|(i, &o)| {
                format!("{} = ${}", quote_ident(&hot_def.columns[o as usize].name), i + 1)
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let verify_sql = format!("SELECT {cols_sql} FROM {tbl_sql} WHERE {pk_pred}");
        // Detached: compiled against cold, never published to cold's registry.
        let verify_plan = cold.prepare_detached(&verify_sql)?;
        let pk_of = |row: &[Value]| -> Vec<Value> {
            pk_ords.iter().map(|&o| row[o as usize].clone()).collect()
        };

        let mut report = TierReport::default();
        loop {
            // 1. Hot write txn FIRST: the writer lock is held across the whole
            // batch, so the rows selected are exactly the rows deleted.
            let mut hot_s: WriteSession<'_> = self.begin()?;
            let rows = match hot_s.query(&sel_sql, params)? {
                ExecResult::Rows { rows, .. } => rows,
                other => {
                    drop(hot_s); // rollback
                    return Err(Error::Bind(format!(
                        "tier: the predicate did not compile to a row-returning \
                         SELECT (got {other:?})"
                    )));
                }
            };
            if rows.is_empty() {
                hot_s.rollback();
                break;
            }

            // 2. Copy to cold; recognize an earlier crashed drain's landings.
            let mut cold_s = cold.begin()?;
            let mut inserted = 0u64;
            let mut reconciled = 0u64;
            for row in &rows {
                let pk = pk_of(row);
                match cold_s.get_by_pk(cold_def.id, &pk)? {
                    None => {
                        cold_s.insert_row(cold_def.id, row)?;
                        inserted += 1;
                    }
                    Some(existing) if vals_eq(&existing, row) => reconciled += 1,
                    Some(_) => {
                        cold_s.rollback();
                        hot_s.rollback();
                        return Err(Error::Schema(format!(
                            "tier: conflict on `{table}` primary key {pk:?} — cold \
                             already holds a DIFFERENT row under that key (possibly \
                             an archived predecessor of a reused key); refusing to \
                             overwrite either side. Resolve by deleting the row from \
                             one side, then re-run the drain"
                        )));
                    }
                }
            }
            // 3. Cold commits BEFORE any hot delete (doc §2 — crash between
            // the commits duplicates, never loses)...
            cold_s.commit()?;
            // ...and the landing is verified through a fresh cold snapshot
            // before hot gives anything up.
            for row in &rows {
                let pk = pk_of(row);
                let landed = match cold.execute_detached(&verify_plan, &pk)? {
                    ExecResult::Rows { mut rows, .. } => rows.pop(),
                    _ => None,
                };
                if !landed.is_some_and(|got| vals_eq(&got, row)) {
                    // hot_s drops → rollback; the batch stays fully in hot
                    // (and, committed, in cold) — safe, converges on re-run.
                    return Err(Error::Corrupt(format!(
                        "tier: row {pk:?} did not read back from cold identically \
                         after commit; hot row NOT reclaimed"
                    )));
                }
            }
            // 4. Reclaim from hot.
            for row in &rows {
                if !hot_s.delete_by_pk(hot_def.id, &pk_of(row))? {
                    return Err(Error::Internal(
                        "tier: a selected row vanished under the held writer lock".into(),
                    ));
                }
            }
            hot_s.commit()?;
            report.moved += inserted;
            report.reconciled += reconciled;
            report.batches += 1;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vals_eq_is_bitwise_on_floats_and_value_eq_elsewhere() {
        let nan = f64::NAN;
        assert!(vals_eq(&[Value::Float(nan)], &[Value::Float(nan)]));
        assert!(!vals_eq(&[Value::Float(0.0)], &[Value::Float(-0.0)]));
        assert!(vals_eq(
            &[Value::Int(1), Value::Text("x".into()), Value::Null],
            &[Value::Int(1), Value::Text("x".into()), Value::Null],
        ));
        assert!(!vals_eq(&[Value::Int(1)], &[Value::Int(2)]));
        assert!(!vals_eq(&[Value::Int(1)], &[Value::Int(1), Value::Null]));
    }

    #[test]
    fn quote_ident_doubles_quotes() {
        assert_eq!(quote_ident("t"), "\"t\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
