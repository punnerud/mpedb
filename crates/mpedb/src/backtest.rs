//! Trigger backtesting (DESIGN-TRIGGERS follow-up): replay a trigger — stored
//! or not-yet-created — against the target table's CURRENT rows inside a write
//! transaction that is ALWAYS aborted, and report what it WOULD have done:
//! which rows fire, which the `WHEN` gate skips, which are vetoed
//! (`RAISE(ABORT)` / a failing procedure), and the net row effect per table.
//!
//! This is the "inform before it goes live" half of the trigger story: mpedb
//! keeps no row history, so the honest replay corpus is the data as it stands
//! — each existing row treated as the event's subject. For INSERT the row IS a
//! faithful event (as if inserted today); for DELETE likewise; for UPDATE
//! there is no historical pre/post pair, so the replay binds `OLD` = `NEW` =
//! the row (the identity assumption, stated in the report) and treats every
//! column as a SET target so `UPDATE OF` triggers fire.
//!
//! Faithfulness over simulation: firing goes through the REAL executor
//! machinery — `fire_row_triggers` on a real `WriteTxn`, cascades into the
//! LIVE trigger set, depth cap, work meter, `recursive_triggers` suppression —
//! and each vetoed row rolls back to its savepoint so one veto does not mask
//! the next row's outcome. The transaction is aborted unconditionally at the
//! end (also on every error path): a backtest can never change the database.

use crate::exec;
use crate::trigger::{FireOutcome, StoredTrigger, TrgEvent, TrgTiming, TRIGGER_PREFIX};
use crate::Database;
use mpedb_sql::RowSide;
use mpedb_types::{Error, Result, Value};

/// What a trigger would have done to the data, per [`Database::backtest_trigger`].
#[derive(Debug, Clone)]
pub struct TriggerBacktest {
    pub trigger: String,
    /// `"BEFORE INSERT"`, `"AFTER UPDATE"`, …
    pub event: String,
    pub table: String,
    /// Rows in the target table (the replay corpus's universe).
    pub table_rows: u64,
    /// Rows actually replayed (≤ `table_rows`; capped by the `limit` argument).
    pub rows_scanned: u64,
    /// Rows whose `WHEN` gate passed and whose body ran to completion.
    pub fired: u64,
    /// Rows the `WHEN` gate skipped.
    pub skipped_when: u64,
    /// Rows the body skipped via `RAISE(IGNORE)`.
    pub ignored: u64,
    /// Rows the body VETOED — `RAISE(ABORT)`, a failing procedure, a
    /// constraint the body tripped. Each veto rolled back to its savepoint.
    pub vetoed: u64,
    /// The first few veto messages, verbatim.
    pub veto_examples: Vec<String>,
    /// Net row-count change per table from the fired bodies (cascades
    /// included), tables with a nonzero delta only. What the trigger would
    /// have done to the data — measured, then rolled back.
    pub net_rows: Vec<(String, i64)>,
    /// Stated replay assumption, when the event needed one (UPDATE).
    pub assumption: Option<String>,
}

impl std::fmt::Display for TriggerBacktest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "backtest `{}` ({} ON {}): {} of {} rows replayed",
            self.trigger, self.event, self.table, self.rows_scanned, self.table_rows
        )?;
        writeln!(
            f,
            "  fired {}, skipped by WHEN {}, ignored {}, vetoed {}",
            self.fired, self.skipped_when, self.ignored, self.vetoed
        )?;
        for m in &self.veto_examples {
            writeln!(f, "  veto: {m}")?;
        }
        if self.net_rows.is_empty() {
            writeln!(f, "  net row effect: none")?;
        } else {
            for (t, d) in &self.net_rows {
                writeln!(f, "  net rows {t}: {d:+}")?;
            }
        }
        if let Some(a) = &self.assumption {
            writeln!(f, "  assumption: {a}")?;
        }
        write!(f, "  (all effects rolled back — the database is unchanged)")
    }
}

impl Database {
    /// Backtest a trigger against the current data and report what it would
    /// have done — without changing anything (the replay transaction is
    /// always aborted).
    ///
    /// `sql_or_name` is either a full `CREATE TRIGGER …` statement (dry-run a
    /// trigger BEFORE creating it — nothing is stored) or the name of a
    /// stored trigger. `limit` caps how many target-table rows are replayed
    /// (`0` = all).
    pub fn backtest_trigger(&self, sql_or_name: &str, limit: u64) -> Result<TriggerBacktest> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let schema = &bundle.schema;

        // A CREATE TRIGGER statement is a dry spec; anything else is a name.
        let st: StoredTrigger = match mpedb_sql::parse_ddl(sql_or_name)? {
            Some(mpedb_sql::DdlStmt::CreateTrigger(spec)) => {
                crate::trigger::build_stored_trigger(&spec, schema, &mut |name| {
                    self.sys_record_get(crate::NS_PROC, name.as_bytes())
                })?
            }
            Some(_) => {
                return Err(Error::Unsupported(
                    "backtest takes a CREATE TRIGGER statement or a trigger name".into(),
                ))
            }
            None => self.load_stored_trigger_by_name(sql_or_name)?.ok_or_else(|| {
                Error::Bind(format!(
                    "no trigger named `{sql_or_name}` — pass a stored trigger's \
                     name or a full CREATE TRIGGER statement"
                ))
            })?,
        };
        let compiled = self.compile_stored_trigger(&st, schema)?.ok_or_else(|| {
            Error::Bind("the trigger's target table no longer exists".into())
        })?;
        let live = self.trigger_set()?;
        let t = schema
            .table(st.table_id)
            .ok_or_else(|| Error::Bind("the trigger's target table no longer exists".into()))?;
        let (event_word, assumption) = match st.event {
            TrgEvent::Insert => ("INSERT", None),
            TrgEvent::Delete => ("DELETE", None),
            TrgEvent::Update => (
                "UPDATE",
                Some(
                    "no update history exists, so each row replays as an \
                     identity update (OLD = NEW = the row) with every column \
                     as a SET target"
                        .to_string(),
                ),
            ),
        };
        let timing_word = match st.timing {
            TrgTiming::Before => "BEFORE",
            TrgTiming::After => "AFTER",
        };
        // For UPDATE replays every column counts as assigned (see above).
        let changed: Vec<u16> = match st.event {
            TrgEvent::Update => (0..t.columns.len() as u16).collect(),
            _ => Vec::new(),
        };
        let live_tables: Vec<(u32, String)> = schema
            .tables
            .iter()
            .filter(|x| !x.dead)
            .map(|x| (x.id, x.name.clone()))
            .collect();

        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        // Everything below runs inside a closure so ONE abort covers every
        // exit — success and error alike. The backtest must never commit.
        let res = (|| -> Result<TriggerBacktest> {
            let table_rows = w.row_count(st.table_id)?;
            let mut before = Vec::with_capacity(live_tables.len());
            for (id, _) in &live_tables {
                before.push(w.row_count(*id)? as i64);
            }
            let cap = if limit == 0 { None } else { Some(limit as usize) };
            let rows = {
                use crate::exec::TxnCtx as _;
                let mut ctx = exec::WriteCtx::new(&mut w, None, None, None);
                ctx.scan_rows_capped(st.table_id, None, None, None, cap)?
            };

            let mut out = TriggerBacktest {
                trigger: st.name.clone(),
                event: format!("{timing_word} {event_word}"),
                table: t.name.clone(),
                table_rows,
                rows_scanned: rows.len() as u64,
                fired: 0,
                skipped_when: 0,
                ignored: 0,
                vetoed: 0,
                veto_examples: Vec::new(),
                net_rows: Vec::new(),
                assumption,
            };
            let mut stack = Vec::new();
            for row in &rows {
                let (new, old): (Option<&[Value]>, Option<&[Value]>) = match st.event {
                    TrgEvent::Insert => (Some(row), None),
                    TrgEvent::Update => (Some(row), Some(row)),
                    TrgEvent::Delete => (None, Some(row)),
                };
                // Evaluate WHEN up front so the report can distinguish
                // "gate skipped" from "fired" (the fire path re-checks it —
                // same program, same 3VL, so the split is exact).
                if let Some((prog, map)) = &compiled.when {
                    let mut slots = Vec::with_capacity(map.len());
                    for &(side, c) in map.iter() {
                        let img = match side {
                            RowSide::New => new,
                            RowSide::Old => old,
                        };
                        slots.push(
                            img.and_then(|r| r.get(c as usize).cloned())
                                .ok_or_else(|| Error::Internal("backtest WHEN slot".into()))?,
                        );
                    }
                    if !prog.eval_filter(&mut stack, &[], &slots)? {
                        out.skipped_when += 1;
                        continue;
                    }
                }
                let sp = w.savepoint_full()?;
                let outcome = {
                    let mut ctx = exec::WriteCtx::new(&mut w, None, None, None);
                    exec::fire_row_triggers(
                        &mut ctx,
                        schema,
                        std::slice::from_ref(&compiled),
                        new,
                        old,
                        &changed,
                        &live,
                        0,
                    )
                };
                match outcome {
                    Ok(FireOutcome::Proceed) => out.fired += 1,
                    Ok(FireOutcome::SkipRow) => out.ignored += 1,
                    Err(e) => {
                        out.vetoed += 1;
                        if out.veto_examples.len() < 3 {
                            out.veto_examples.push(e.to_string());
                        }
                        w.rollback_to_full(sp)?;
                    }
                }
            }
            for (i, (id, name)) in live_tables.iter().enumerate() {
                let delta = w.row_count(*id)? as i64 - before[i];
                if delta != 0 {
                    out.net_rows.push((name.clone(), delta));
                }
            }
            Ok(out)
        })();
        w.abort();
        // The replay dirtied nothing durable, but the plan cache may have
        // loaded registry plans — leave it; nothing here bumped the gen.
        res
    }

    /// Read one stored trigger by name (ASCII-case-insensitive) off a read
    /// snapshot; `None` when no such trigger.
    fn load_stored_trigger_by_name(&self, name: &str) -> Result<Option<StoredTrigger>> {
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan_range(TRIGGER_PREFIX, b"trigger0");
        r.finish()?;
        for (subkey, value) in scan? {
            let Some(stored) = subkey.strip_prefix(TRIGGER_PREFIX) else { continue };
            if mpedb_types::ident_eq(&String::from_utf8_lossy(stored), name) {
                return Ok(Some(StoredTrigger::decode(&value)?));
            }
        }
        Ok(None)
    }
}
