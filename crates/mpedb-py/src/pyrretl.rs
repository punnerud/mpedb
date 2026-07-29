//! The Python surface for rRETL (PYSPELL-RRETL.md), lifted out of
//! `lib.rs` to keep that file under the house's 2000-line ceiling. Free
//! functions over `&Database`; the `#[pymethods]` block delegates.

use pyo3::prelude::*;
use pyo3::types::PyList;

use crate::{map_err, rretl_report_to_py, ProgrammingError};

/// Build a [`mpedb::rretl_map::MapSpec`] from the Python-native dict form.
/// Every refusal names the missing/mistyped key; the REAL validation (legal
/// identifiers, tables exist, pairs load) happens in the engine, identically
/// for both the dict and the TOML door.
fn map_spec_from_py(spec: &Bound<'_, PyAny>) -> PyResult<mpedb::rretl_map::MapSpec> {
    use pyo3::types::PyDict;
    fn bad(m: String) -> PyErr {
        ProgrammingError::new_err(m)
    }
    fn need_str(d: &Bound<'_, PyDict>, k: &str, ctx: &str) -> PyResult<String> {
        d.get_item(k)?
            .ok_or_else(|| bad(format!("map spec: missing `{k}` in {ctx}")))?
            .extract::<String>()
            .map_err(|_| bad(format!("map spec: `{k}` in {ctx} must be a string")))
    }
    fn opt_str(d: &Bound<'_, PyDict>, k: &str, ctx: &str) -> PyResult<Option<String>> {
        match d.get_item(k)? {
            None => Ok(None),
            Some(v) if v.is_none() => Ok(None),
            Some(v) => v
                .extract::<String>()
                .map(Some)
                .map_err(|_| bad(format!("map spec: `{k}` in {ctx} must be a string"))),
        }
    }
    let d = spec.cast::<PyDict>().map_err(|_| {
        bad("rretl_map_define takes a TOML string or a dict {name, tables: [{source, \
             target, target_key?, columns: [{source, target, pair?}]}]}"
            .into())
    })?;
    let name = need_str(d, "name", "the map dict")?;
    // The journal is opt-in (§15.4): it puts an insert on every write to a
    // mapped table, so it is declared, never inferred.
    let stream = match d.get_item("stream")? {
        None => false,
        Some(v) if v.is_none() => false,
        Some(v) => v
            .extract::<bool>()
            .map_err(|_| bad("map spec: `stream` must be true or false".into()))?,
    };
    let tables_any = d
        .get_item("tables")?
        .ok_or_else(|| bad("map spec: missing `tables`".into()))?;
    let tables_list = tables_any
        .cast::<PyList>()
        .map_err(|_| bad("map spec: `tables` must be a list of dicts".into()))?;
    let mut tables = Vec::new();
    for t in tables_list.iter() {
        let td = t
            .cast::<PyDict>()
            .map_err(|_| bad("map spec: each table must be a dict".into()))?;
        let source = need_str(td, "source", "a table")?;
        let target = need_str(td, "target", "a table")?;
        let target_key = opt_str(td, "target_key", "a table")?;
        let cols_any = td
            .get_item("columns")?
            .ok_or_else(|| bad(format!("map spec: table `{source}` has no `columns`")))?;
        let cols_list = cols_any
            .cast::<PyList>()
            .map_err(|_| bad("map spec: `columns` must be a list of dicts".into()))?;
        let mut columns = Vec::new();
        for c in cols_list.iter() {
            let cd = c
                .cast::<PyDict>()
                .map_err(|_| bad("map spec: each column must be a dict".into()))?;
            columns.push(mpedb::rretl_map::MapColumn {
                source: need_str(cd, "source", "a column")?,
                target: need_str(cd, "target", "a column")?,
                pair: opt_str(cd, "pair", "a column")?,
            });
        }
        tables.push(mpedb::rretl_map::MapTable { source, target, target_key, columns });
    }
    Ok(mpedb::rretl_map::MapSpec { name, tables, stream })
}

    /// Transform `table.column` IN PLACE with a registered pair, in ONE
/// transaction: per-row residuals are kept in `rretl_residual`, the run in
/// `rretl_lineage`, and 100% of rows verify against the source hash BEFORE
/// the commit that destroys the source. Returns
/// `{"run_id", "rows", "residuals"}`.
pub(crate) fn rretl_apply(
    db: &mpedb::Database,
    py: Python<'_>,
    pair: &str,
    table: &str,
    column: &str,
) -> PyResult<Py<PyAny>> {
    let r = py.detach(|| db.rretl_apply(pair, table, column)).map_err(map_err)?;
    rretl_report_to_py(py, r)
}

/// Undo run `run_id` EXACTLY. Hash-gated: refused if the column changed
/// outside the pipeline — for a column you have edited, use
/// `rretl_putback`, which exists for exactly that.
pub(crate) fn rretl_revert(db: &mpedb::Database, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
    let r = py.detach(|| db.rretl_revert(run_id)).map_err(map_err)?;
    rretl_report_to_py(py, r)
}

/// Invert run `run_id` while KEEPING edits made to the transformed
/// column — the lens putback. Edited values flow back through
/// `inverse(edited, residual)`; deleted rows stay deleted; every row is
/// PutRes-verified (`forward(x') == y'`, `rex(x') == r`) before commit.
pub(crate) fn rretl_putback(db: &mpedb::Database, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
    let r = py.detach(|| db.rretl_putback(run_id)).map_err(map_err)?;
    rretl_report_to_py(py, r)
}

/// Verify-at-rest: re-check every standing run (top-run hash, residual
/// coverage, pair loadability). Returns a list of finding strings —
/// empty means clean. Reports, never repairs.
pub(crate) fn rretl_fsck(db: &mpedb::Database, py: Python<'_>) -> PyResult<Vec<String>> {
    py.detach(|| db.rretl_fsck()).map_err(map_err)
}

/// Store `data` as the next VERSION of `obj` (stage 3): the new version
/// is kept full, the previous newest is rewritten as a reverse delta —
/// verified byte-identical, as persisted, before the commit — and every
/// 8th version stays full forever. Returns the version number.
pub(crate) fn rretl_put_version(db: &mpedb::Database, py: Python<'_>, obj: &str, data: &[u8]) -> PyResult<i64> {
    py.detach(|| db.rretl_put_version(obj, data)).map_err(map_err)
}

/// Materialize version `ver` of `obj` as bytes. Every reconstruction
/// step is hash-verified; corruption is a named error, never wrong bytes.
pub(crate) fn rretl_get_version(db: &mpedb::Database, py: Python<'_>, obj: &str, ver: i64) -> PyResult<Py<PyAny>> {
    let bytes = py.detach(|| db.rretl_get_version(obj, ver)).map_err(map_err)?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).into_any().unbind())
}

/// Every version of `obj`, oldest first, as dicts:
/// `{"ver", "stored_as", "bytes", "content_hash"}`.
pub(crate) fn rretl_versions(db: &mpedb::Database, py: Python<'_>, obj: &str) -> PyResult<Vec<Py<PyAny>>> {
    let vers = py.detach(|| db.rretl_versions(obj)).map_err(map_err)?;
    vers.into_iter()
        .map(|v| {
            let d = pyo3::types::PyDict::new(py);
            d.set_item("ver", v.ver)?;
            d.set_item("stored_as", v.stored_as)?;
            d.set_item("bytes", v.bytes)?;
            d.set_item("content_hash", v.content_hash)?;
            Ok(d.into_any().unbind())
        })
        .collect()
}

/// Delete the OLDEST versions of `obj`, keeping the newest `keep` —
/// chain-safe by construction (deltas base upward), recorded as lineage
/// outcome `pruned`. Returns how many were deleted; `keep = 0` refused.
pub(crate) fn rretl_prune_versions(db: &mpedb::Database, py: Python<'_>, obj: &str, keep: u64) -> PyResult<u64> {
    py.detach(|| db.rretl_prune_versions(obj, keep)).map_err(map_err)
}

/// Splice a zip archive into the database: members become rows in
/// `rretl_archive_members`, the residual keeps every non-data byte, and
/// the reconstruction is verified byte-identical BEFORE the ingest
/// commits. Returns the archive id.
pub(crate) fn rretl_pack_in(db: &mpedb::Database, py: Python<'_>, name: &str, data: &[u8]) -> PyResult<i64> {
    py.detach(|| db.rretl_pack_in(name, data)).map_err(map_err)
}

/// Rebuild archive `archive_id` byte-identically, hash-gated against the
/// original: a member row changed outside the pipeline is a named error.
pub(crate) fn rretl_pack_out(db: &mpedb::Database, py: Python<'_>, archive_id: i64) -> PyResult<Py<PyAny>> {
    let bytes = py.detach(|| db.rretl_pack_out(archive_id)).map_err(map_err)?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).into_any().unbind())
}

/// Every spliced archive, oldest first, as dicts:
/// `{"archive_id", "name", "members", "content_hash"}`.
pub(crate) fn rretl_archives(db: &mpedb::Database, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
    let arches = py.detach(|| db.rretl_archives()).map_err(map_err)?;
    arches
        .into_iter()
        .map(|a| {
            let d = pyo3::types::PyDict::new(py);
            d.set_item("archive_id", a.archive_id)?;
            d.set_item("name", a.name)?;
            d.set_item("members", a.members)?;
            d.set_item("content_hash", a.content_hash)?;
            Ok(d.into_any().unbind())
        })
        .collect()
}

/// Store (or replace) a table-SET map (stage 4, §13): source tables
/// mirrored into a different target shape through lens pairs, synced
/// both ways. Takes either the mapping TOML as a string, or — the
/// Python-native form — a dict:
///
/// ```python
/// db.rretl_map_define({
///     "name": "crm",
///     "tables": [{
///         "source": "customers",
///         "target": "crm_customers",       # "target_key" optional
///         "columns": [
///             {"source": "name",   "target": "full_name"},
///             {"source": "temp_c", "target": "temp_f", "pair": "celsius"},
///         ],
///     }],
/// })
/// ```
///
/// Both forms store the same canonical record; the spec is validated
/// NOW — sources, identities, pairs.
pub(crate) fn rretl_map_define(db: &mpedb::Database, py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<()> {
    if let Ok(text) = spec.extract::<String>() {
        return py.detach(|| db.rretl_map_define(&text)).map_err(map_err);
    }
    let ms = map_spec_from_py(spec)?;
    py.detach(|| db.rretl_map_define_spec(&ms)).map_err(map_err)
}

/// Sync a map, BOTH directions, in one transaction. Repeating a sync is
/// a no-op (the state-hash echo guard); both sides moved = a named
/// conflict that aborts whole. Returns the per-direction counts.
pub(crate) fn rretl_map_sync(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let r = py.detach(|| db.rretl_map_sync(name)).map_err(map_err)?;
    let d = pyo3::types::PyDict::new(py);
    d.set_item("run_id", r.run_id)?;
    d.set_item("a_to_b", r.a_to_b)?;
    d.set_item("b_to_a", r.b_to_a)?;
    d.set_item("created_b", r.created_b)?;
    d.set_item("created_a", r.created_a)?;
    d.set_item("deleted_a", r.deleted_a)?;
    d.set_item("deleted_b", r.deleted_b)?;
    d.set_item("unchanged", r.unchanged)?;
    Ok(d.into_any().unbind())
}

/// Read-only dry run of a map sync: one dict per table pair with the
/// would-move counts, EVERY named conflict (a sync aborts on the
/// first), and `diverged` — rows whose state says both sides are clean
/// while forward(source) != target, the silent breach the echo guard
/// cannot see. `clean` on the report level = nothing to do, nothing
/// wrong.
pub(crate) fn rretl_map_check(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let r = py.detach(|| db.rretl_map_check(name)).map_err(map_err)?;
    let top = pyo3::types::PyDict::new(py);
    top.set_item("clean", r.is_clean())?;
    top.set_item("pending_total", r.pending_total())?;
    let tables = pyo3::types::PyList::empty(py);
    for t in &r.tables {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("source", &t.src)?;
        d.set_item("target", &t.dst)?;
        d.set_item("pending_a2b", t.pending_a2b)?;
        d.set_item("pending_b2a", t.pending_b2a)?;
        d.set_item("would_create_b", t.would_create_b)?;
        d.set_item("would_create_a", t.would_create_a)?;
        d.set_item("would_delete_a", t.would_delete_a)?;
        d.set_item("would_delete_b", t.would_delete_b)?;
        d.set_item("would_adopt", t.would_adopt)?;
        d.set_item("unchanged", t.unchanged)?;
        d.set_item("orphan_state", t.orphan_state)?;
        d.set_item("conflicts", t.conflicts.clone())?;
        d.set_item("diverged", t.diverged.clone())?;
        tables.append(d)?;
    }
    top.set_item("tables", tables)?;
    Ok(top.into_any().unbind())
}

/// One bounded, resumable pass of the map daemon (#53) — the cron
/// form. Commits as it goes, so a run that hits its budget has still
/// moved what it moved and the NEXT run resumes from the cursor;
/// every commit advances the whole set (a chunk from each table).
/// Conflicts are counted and skipped, never fatal. Returns a dict.
pub(crate) fn rretl_map_run(
    db: &mpedb::Database,
    py: Python<'_>,
    name: &str,
    max_secs: Option<u64>,
    max_rows: Option<u64>,
    runner: Option<String>,
    lease_secs: Option<u64>,
) -> PyResult<Py<PyAny>> {
    let opts = mpedb::rretl_map_run::RunOptions { max_secs, max_rows, runner, lease_secs };
    let r = py.detach(|| db.rretl_map_run(name, &opts)).map_err(map_err)?;
    let d = pyo3::types::PyDict::new(py);
    d.set_item("round", r.round)?;
    d.set_item("rows", r.rows)?;
    d.set_item("commits", r.commits)?;
    d.set_item("streamed", r.streamed)?;
    d.set_item("conflicts", r.conflicts)?;
    d.set_item("conflict_notes", r.conflict_notes.clone())?;
    d.set_item(
        "stopped_by",
        match r.stopped_by {
            Some(mpedb::rretl_map_run::RunStop::RoundComplete) => "round_complete",
            Some(mpedb::rretl_map_run::RunStop::Budget) => "budget",
            None => "nothing",
        },
    )?;
    d.set_item("round_complete",
        r.stopped_by == Some(mpedb::rretl_map_run::RunStop::RoundComplete))?;
    d.set_item("a_to_b", r.moved.a_to_b)?;
    d.set_item("b_to_a", r.moved.b_to_a)?;
    d.set_item("created_b", r.moved.created_b)?;
    d.set_item("created_a", r.moved.created_a)?;
    d.set_item("deleted_a", r.moved.deleted_a)?;
    d.set_item("deleted_b", r.moved.deleted_b)?;
    d.set_item("unchanged", r.moved.unchanged)?;
    d.set_item("note", r.note())?;
    Ok(d.into_any().unbind())
}

/// Restrict which runner may `rretl_map_run` this map (empty string
/// clears it). A guard against mistakes — a laptop picking up the
/// cron job — and NOT an auth boundary: anything that can write the
/// file can claim any runner name.
pub(crate) fn rretl_map_set_runner(db: &mpedb::Database, py: Python<'_>, name: &str, runner: &str) -> PyResult<()> {
    py.detach(|| db.rretl_map_set_runner(name, runner)).map_err(map_err)
}

/// The map's daemon status: runner, round, live lease, and which
/// tables are mid-round.
pub(crate) fn rretl_map_status(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let st = py.detach(|| db.rretl_map_status(name)).map_err(map_err)?;
    let d = pyo3::types::PyDict::new(py);
    d.set_item("runner", &st.runner)?;
    d.set_item("round", st.round)?;
    d.set_item("lease_owner", &st.lease_owner)?;
    d.set_item("lease_until", st.lease_until)?;
    d.set_item("note", &st.note)?;
    let ip = pyo3::types::PyList::empty(py);
    for (tbl, phase) in &st.in_progress {
        let e = pyo3::types::PyDict::new(py);
        e.set_item("table", tbl)?;
        e.set_item("pass", phase)?;
        ip.append(e)?;
    }
    d.set_item("in_progress", ip)?;
    Ok(d.into_any().unbind())
}

/// Every stored map name.
pub(crate) fn rretl_maps(db: &mpedb::Database, py: Python<'_>) -> PyResult<Vec<String>> {
    py.detach(|| db.rretl_maps()).map_err(map_err)
}

/// The stored mapping TOML, verbatim.
pub(crate) fn rretl_map_show(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<String> {
    py.detach(|| db.rretl_map_show(name)).map_err(map_err)
}

/// Drop a map (its sync state rows remain). True when it existed.
pub(crate) fn rretl_map_drop(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<bool> {
    py.detach(|| db.rretl_map_drop(name)).map_err(map_err)
}

/// How much the trigger-fed journal has waiting, per mapped table
/// (DESIGN-RRETL §15). Empty on a map that is not streaming.
pub(crate) fn rretl_map_backlog(
    db: &mpedb::Database,
    py: Python<'_>,
    name: &str,
) -> PyResult<Py<PyAny>> {
    let b = py.detach(|| db.rretl_map_backlog(name)).map_err(map_err)?;
    let out = pyo3::types::PyDict::new(py);
    for (tbl, n) in b {
        out.set_item(tbl, n)?;
    }
    Ok(out.into_any().unbind())
}
