//! #53's stream half (DESIGN-RRETL §15): the map learns what changed
//! instead of scanning for it.
//!
//! §14's daemon walks the source, the target and the state table every
//! round — O(rows) whatever the churn. Here a trigger on each mapped table
//! appends the key it just touched to a journal, and the daemon drains the
//! journal before it advances the scan. The scan does NOT go away, and that
//! is the whole design:
//!
//! - triggers fire on the SQL path only, so a `mirror import`, the typed row
//!   API, or a file swapped in underneath leaves no entry;
//! - `DROP TRIGGER` is a legal statement a user can run;
//! - a map defined over tables that already hold rows has no history for
//!   them.
//!
//! So the journal is a FAST PATH, never the truth — the same relationship
//! ingest's delta has to its dump. A map whose journal is the only thing
//! that ever runs will drift, and only the round finds it.
//!
//! **Why a journal and not a set.** A set keyed `(map, tbl, pk)` would
//! collapse a hot row to one entry — and it is exactly what the stage-4
//! duel broke: a raw pk inside a composite key hits the engine's 976-byte
//! encoded-key cap, which is why every other rRETL table keys on `pk_ref`
//! (blake3 of the pk's canonical bits). A trigger body cannot compute a
//! blake3. It can INSERT what it was handed. So: append, tolerate
//! duplicates, collapse them at drain time — which is also the cheapest
//! thing the write path can be asked to carry.
//!
//! **The echo terminates.** The syncer's own pushes fire these triggers
//! too (verified: a trigger fires inside a `WriteSession` exactly as it
//! does for a top-level statement). The re-appeared row classifies against
//! the state that push just wrote, comes out clean on both sides, and
//! writes nothing — no write, no trigger, no new entry. Exactly one extra
//! classification per pushed row.

use std::collections::HashSet;

use mpedb_types::{ColumnType, Result, Value};

use crate::rretl::{create_bookkeeping, pk_ref, rows_of, shape_gate, spec_col};
use crate::rretl_map::{
    classify_p1, classify_p2, MapSql, MapSpec, MapWriter, ResolvedTable, P1, P2,
};
use crate::rretl_map_run::MapRunReport;
use crate::WriteSession;

pub const T_MAP_DIRTY: &str = "rretl_map_dirty";
const DIRTY_SHAPE: [&str; 5] = ["seq", "map", "tbl", "side", "k"];

/// The journal. `seq` is an implicit rowid — the trigger inserts without
/// it — and it is both the identity and the order, so no clock is needed.
pub(crate) fn ensure_dirty_table(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use ColumnType::{Any, Int64, Text};
    if !shape_gate(have, T_MAP_DIRTY, &DIRTY_SHAPE)? {
        create_bookkeeping(
            s,
            T_MAP_DIRTY,
            vec![
                spec_col("seq", Int64),
                spec_col("map", Text),
                spec_col("tbl", Text),
                // Which side the write landed on. Informational — the
                // classification reads both sides regardless — but an
                // operator staring at a backlog wants to know.
                spec_col("side", Text),
                // The key VALUE. A digest would be one-way, and the drain
                // has to look the row up on both sides.
                spec_col("k", Any),
            ],
            &["seq"],
        )?;
    }
    Ok(())
}

/// `rrmap_<map>_<i><a|b>_<i|u|d>`. The `rrmap_` prefix is RESERVED: install
/// drops these names before creating them, so a user trigger that claims one
/// is replaced rather than refused — the alternative (refusing) would make a
/// map impossible to redefine, since its own triggers are always "taken".
///
/// The rest of the shape: the map, the table's INDEX in the spec,
/// the side, the event. Index rather than table name so the name stays
/// short and predictable — removal only needs to know how many tables the
/// PRIOR spec had, which a parse gives without resolving anything (a table
/// dropped since would make resolving fail, and `map define` must still be
/// able to clean up after itself).
fn trigger_name(map: &str, i: usize, side: char, ev: char) -> String {
    format!("rrmap_{map}_{i}{side}_{ev}")
}

/// Install the map's triggers. Idempotent: every name is dropped first, so
/// a redefine cannot leave a stale body behind.
pub(crate) fn install(
    s: &mut WriteSession<'_>,
    map: &str,
    tables: &[ResolvedTable],
    prior_tables: usize,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    remove(s, map, prior_tables.max(tables.len()))?;
    for (i, rt) in tables.iter().enumerate() {
        install_side(s, map, i, rt, 'a')?;
        // A map may MATERIALIZE its target (§13), so at define time the
        // target table can legitimately not exist yet — and `CREATE TRIGGER`
        // on a missing table is a bind error, not a warning. Its triggers go
        // in the moment the map creates it (`prepare_map_tables`), which is
        // also the only moment they could have been missed.
        if have.iter().any(|(n, _)| n.eq_ignore_ascii_case(&rt.dst)) {
            install_side(s, map, i, rt, 'b')?;
        }
    }
    Ok(())
}

/// One table, one side: the three row events, all appending to the journal.
pub(crate) fn install_side(
    s: &mut WriteSession<'_>,
    map: &str,
    i: usize,
    rt: &ResolvedTable,
    side: char,
) -> Result<()> {
    let (tbl, key) = if side == 'a' { (&rt.src, &rt.src_key) } else { (&rt.dst, &rt.dst_key) };
    for (ev, sql_ev, row) in
        [('i', "INSERT", "NEW"), ('u', "UPDATE", "NEW"), ('d', "DELETE", "OLD")]
    {
        let name = trigger_name(map, i, side, ev);
        s.query(&format!("DROP TRIGGER IF EXISTS {name}"), &[])?;
        s.query(
            &format!(
                // The pair's identity is the TARGET name — that is what the
                // state rows key on (§13.2), so the drain can find its
                // ResolvedTable from the entry alone.
                "CREATE TRIGGER {name} AFTER {sql_ev} ON \"{tbl}\" FOR EACH ROW \
                 BEGIN INSERT INTO {T_MAP_DIRTY} (map, tbl, side, k) \
                 VALUES ('{map}', '{}', '{side}', {row}.\"{key}\"); END",
                rt.dst
            ),
            &[],
        )?;
    }
    Ok(())
}

/// Remove the map's triggers. `n` is how many tables to cover — dropping a
/// name that is not there is a no-op, so covering the larger of the old and
/// new table counts is both safe and exact.
pub(crate) fn remove(s: &mut WriteSession<'_>, map: &str, n: usize) -> Result<()> {
    for i in 0..n {
        for side in ['a', 'b'] {
            for ev in ['i', 'u', 'd'] {
                s.query(
                    &format!("DROP TRIGGER IF EXISTS {}", trigger_name(map, i, side, ev)),
                    &[],
                )?;
            }
        }
    }
    Ok(())
}

/// Drop everything the map has queued. `map sync` calls this on success:
/// it held the writer lock for its whole transaction, so when it commits
/// there is nothing outstanding by construction, and stale entries would
/// only make the next round re-classify rows already known to be clean.
pub(crate) fn clear(
    s: &mut WriteSession<'_>,
    map: &str,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    if have.iter().any(|(n, _)| n == T_MAP_DIRTY) {
        s.query(
            &format!("DELETE FROM {T_MAP_DIRTY} WHERE map = $1"),
            &[Value::Text(map.into())],
        )?;
    }
    Ok(())
}

/// Consume up to `chunk` journal entries for one mapped table and sync the
/// distinct keys they name. Returns how many ENTRIES were consumed (not
/// keys: duplicates are expected and collapse here).
///
/// The caller runs this inside the same transaction as the scan chunk, so a
/// kill cannot separate "the row was synced" from "the entry was consumed".
pub(crate) fn drain_chunk(
    s: &mut WriteSession<'_>,
    map: &str,
    rt: &ResolvedTable,
    chunk: usize,
    report: &mut MapRunReport,
) -> Result<usize> {
    let rows = rows_of(s.query(
        &format!(
            "SELECT seq, k FROM {T_MAP_DIRTY} WHERE map = $1 AND tbl = $2 \
             ORDER BY seq LIMIT {chunk}"
        ),
        &[Value::Text(map.into()), Value::Text(rt.dst.clone())],
    )?)?;
    let Some(last) = rows.last().map(|r| r[0].clone()) else {
        return Ok(0);
    };
    let sql = MapSql::new(rt, chunk);
    let mut w = MapWriter::new(map, rt, &sql);
    let mut seen: HashSet<Vec<u8>> = HashSet::with_capacity(rows.len());
    for row in &rows {
        let key = &row[1];
        if !seen.insert(pk_ref(key)) {
            continue;
        }
        sync_one_key(s, map, rt, &sql, &mut w, key, report)?;
    }
    s.query(
        &format!("DELETE FROM {T_MAP_DIRTY} WHERE map = $1 AND tbl = $2 AND seq <= $3"),
        &[Value::Text(map.into()), Value::Text(rt.dst.clone()), last],
    )?;
    Ok(rows.len())
}

/// The three scan passes collapsed onto ONE key — and deliberately built
/// from the same `classify_p1`/`classify_p2` the scan uses, because three
/// copies of the decision would drift and that is exactly the bug class the
/// stage-4 duel found.
fn sync_one_key(
    s: &mut WriteSession<'_>,
    map: &str,
    rt: &ResolvedTable,
    sql: &MapSql,
    w: &mut MapWriter<'_>,
    key: &Value,
    report: &mut MapRunReport,
) -> Result<()> {
    report.rows += 1;
    let st = w.state_of(s, key)?;
    let a = rows_of(s.query(&sql.src_get, std::slice::from_ref(key))?)?.into_iter().next();
    if let Some(xs) = a {
        let b = w.target_row(s, key)?;
        return match classify_p1(rt, map, key, &xs, st, b, false)? {
            P1::Conflict(msg) => {
                report.note_conflict(msg);
                Ok(())
            }
            action => w.apply_p1(s, key, action, &mut report.moved),
        };
    }
    if let Some(ybs) = w.target_row(s, key)? {
        return match classify_p2(rt, map, key, &ybs, st)? {
            P2::Conflict(msg) => {
                report.note_conflict(msg);
                Ok(())
            }
            action => w.apply_p2(s, key, action, &mut report.moved),
        };
    }
    // Neither side holds it: the scan's third pass, for this key alone.
    w.sweep_state_row(s, &Value::Blob(pk_ref(key)), key)
}

impl crate::Database {
    /// How much the journal has waiting, per mapped table. A backlog that
    /// only grows means the daemon is not keeping up with the writes — a
    /// fact worth seeing rather than inferring from staleness.
    pub fn rretl_map_backlog(&self, map: &str) -> Result<Vec<(String, i64)>> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_MAP_DIRTY) {
            return Ok(Vec::new());
        }
        rows_of(self.query(
            &format!(
                "SELECT tbl, count(*) FROM {T_MAP_DIRTY} WHERE map = $1 \
                 GROUP BY tbl ORDER BY tbl"
            ),
            &[Value::Text(map.into())],
        )?)?
        .into_iter()
        .map(|r| Ok((crate::rretl::as_text(&r[0]), crate::rretl::as_int(&r[1])?)))
        .collect()
    }
}

/// How many tables the map's PRIOR spec had, for trigger removal. A spec
/// that no longer parses covers nothing — which is the right answer: its
/// triggers were named after tables nobody can enumerate any more, and
/// `DROP TABLE` took them with it.
pub(crate) fn prior_table_count(prior: Option<&[u8]>) -> usize {
    let Some(bytes) = prior else { return 0 };
    let Some(text) = bytes.split_first().and_then(|(_, t)| std::str::from_utf8(t).ok()) else {
        return 0;
    };
    MapSpec::from_toml_str(text).map(|s| s.tables.len()).unwrap_or(0)
}
