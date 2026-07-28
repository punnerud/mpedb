//! The Python surface for ingest (INGEST-GUIDE.md), lifted out of
//! `lib.rs` to keep that file under the house's 2000-line ceiling.
//!
//! These are free functions over `&Database`; the `#[pymethods]` block in
//! `lib.rs` is a one-line delegation each, because pyo3 without the
//! `multiple-pymethods` feature allows only one such block per type.

use pyo3::prelude::*;
use mpedb::Value;

use crate::{map_err, py_to_value, value_to_py, ProgrammingError};

/// Rows from Python: a list of dicts keyed by column name (the natural
/// shape for API results), or a list of lists with `columns` given. The
/// dict form takes its column order from the FIRST row and refuses a later
/// row that disagrees — a ragged batch means the fetch is inconsistent,
/// and silently filling NULLs would hide it.
fn rows_from_py(
    rows: &Bound<'_, PyAny>,
    columns: Option<Vec<String>>,
) -> PyResult<(Vec<String>, Vec<Vec<Value>>)> {
    use pyo3::types::{PyDict, PyList};
    let list = rows
        .cast::<PyList>()
        .map_err(|_| ProgrammingError::new_err("ingest: rows must be a list"))?;
    let mut cols: Vec<String> = columns.unwrap_or_default();
    let mut out: Vec<Vec<Value>> = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        if let Ok(d) = item.cast::<PyDict>() {
            let mut keys: Vec<String> = Vec::with_capacity(d.len());
            let mut vals: Vec<Value> = Vec::with_capacity(d.len());
            for (k, v) in d.iter() {
                keys.push(k.extract::<String>().map_err(|_| {
                    ProgrammingError::new_err("ingest: row keys must be column-name strings")
                })?);
                vals.push(py_to_value(&v)?);
            }
            if cols.is_empty() {
                cols = keys;
            } else if keys.len() != cols.len()
                || !keys.iter().zip(&cols).all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                return Err(ProgrammingError::new_err(format!(
                    "ingest: row {i} has columns {keys:?} but the batch started with {cols:?} \
                     — a ragged batch means the fetch is inconsistent"
                )));
            } else {
                // Same names, possibly a different iteration order.
                let mut ordered = Vec::with_capacity(cols.len());
                for want in &cols {
                    let at = keys.iter().position(|k| k.eq_ignore_ascii_case(want)).unwrap();
                    ordered.push(vals[at].clone());
                }
                vals = ordered;
            }
            out.push(vals);
        } else {
            let seq = item.cast::<PyList>().map_err(|_| {
                ProgrammingError::new_err(format!(
                    "ingest: row {i} is neither a dict nor a list"
                ))
            })?;
            if cols.is_empty() {
                return Err(ProgrammingError::new_err(
                    "ingest: list rows need `columns=[...]`",
                ));
            }
            out.push(seq.iter().map(|v| py_to_value(&v)).collect::<PyResult<_>>()?);
        }
    }
    // No rows and no column names is the natural last page of a paged
    // fetch, not a mistake: it places nothing and still charges the call.
    Ok((cols, out))
}

fn receipt_dict(py: Python<'_>, r: &mpedb::ingest_run::IngestReport) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    d.set_item("run_id", r.run_id)?;
    d.set_item("edge", &r.edge)?;
    d.set_item("table", &r.table)?;
    d.set_item("mode", &r.mode)?;
    d.set_item("rows_in", r.rows_in)?;
    d.set_item("inserted", r.inserted)?;
    d.set_item("updated", r.updated)?;
    d.set_item("deleted", r.deleted)?;
    d.set_item("unchanged", r.unchanged)?;
    d.set_item("conflicts", r.conflicts)?;
    d.set_item("calls", r.calls)?;
    d.set_item("bytes", r.bytes)?;
    d.set_item("cursor_state", &r.cursor_state)?;
    d.set_item("caught", r.caught)?;
    d.set_item("missed", r.missed)?;
    d.set_item("watermark", value_to_py(py, r.watermark.clone())?)?;
    d.set_item("complete", r.complete)?;
    d.set_item("note", r.note())?;
    d.set_item("cursor_note", r.cursor_note())?;
    Ok(d.into_any().unbind())
}

/// A source declaration from the Python-native dict form. Every refusal
/// names the offending key; the REAL validation (tables exist, parents
/// resolve, deltas have a reconciling dump) happens in the engine.
fn ingest_spec_toml(spec: &Bound<'_, PyAny>) -> PyResult<String> {
    use pyo3::types::{PyDict, PyList};
    let d = spec
        .cast::<PyDict>()
        .map_err(|_| ProgrammingError::new_err("ingest spec: expected a dict or a TOML string"))?;
    let get = |k: &str| d.get_item(k).ok().flatten();
    let name = get("name")
        .and_then(|v| v.extract::<String>().ok())
        .ok_or_else(|| ProgrammingError::new_err("ingest spec: missing `name`"))?;
    let mut out = format!("[source]\nname = \"{name}\"\n");
    if let Some(p) = get("policy").and_then(|v| v.extract::<String>().ok()) {
        out.push_str(&format!("policy = \"{p}\"\n"));
    }
    for k in ["work_from", "work_to"] {
        if let Some(v) = get(k).and_then(|v| v.extract::<i64>().ok()) {
            out.push_str(&format!("{k} = {v}\n"));
        }
    }
    if let Some(bs) = get("budget").and_then(|v| v.cast::<PyList>().ok().cloned()) {
        for b in bs.iter() {
            let bd = b.cast::<PyDict>().map_err(|_| {
                ProgrammingError::new_err("ingest spec: each budget must be a dict")
            })?;
            out.push_str("\n[[source.budget]]\n");
            for k in ["profile"] {
                if let Some(v) = bd.get_item(k)?.and_then(|v| v.extract::<String>().ok()) {
                    out.push_str(&format!("{k} = \"{v}\"\n"));
                }
            }
            for k in ["window_secs", "calls", "bytes"] {
                if let Some(v) = bd.get_item(k)?.and_then(|v| v.extract::<i64>().ok()) {
                    out.push_str(&format!("{k} = {v}\n"));
                }
            }
        }
    }
    let edges = get("edges")
        .and_then(|v| v.cast::<PyList>().ok().cloned())
        .ok_or_else(|| ProgrammingError::new_err("ingest spec: missing `edges`"))?;
    for e in edges.iter() {
        let ed = e
            .cast::<PyDict>()
            .map_err(|_| ProgrammingError::new_err("ingest spec: each edge must be a dict"))?;
        out.push_str("\n[[source.edge]]\n");
        for k in ["name", "kind", "parent", "table", "strategy", "cursor"] {
            if let Some(v) = ed.get_item(k)?.and_then(|v| v.extract::<String>().ok()) {
                out.push_str(&format!("{k} = \"{v}\"\n"));
            }
        }
        for k in ["overlap_secs", "batch", "cost_calls", "cost_bytes", "weight"] {
            if let Some(v) = ed.get_item(k)?.and_then(|v| v.extract::<i64>().ok()) {
                out.push_str(&format!("{k} = {v}\n"));
            }
        }
    }
    Ok(out)
}


    
/// Declare an external source: the call graph, the budget vector and
/// the conflict policy. A dict or a TOML string; both store the same
/// canonical record. Validated NOW — tables, parents, acyclicity, and
/// the rule that every delta needs a reconciling dump.
pub(crate) fn ingest_define(db: &mpedb::Database, py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<()> {
        if let Ok(text) = spec.extract::<String>() {
        return py.detach(|| db.ingest_define(&text)).map_err(map_err);
    }
    let toml = ingest_spec_toml(spec)?;
    py.detach(|| db.ingest_define(&toml)).map_err(map_err)
}

pub(crate) fn ingest_sources(db: &mpedb::Database, py: Python<'_>) -> PyResult<Vec<String>> {
        py.detach(|| db.ingest_sources()).map_err(map_err)
}

pub(crate) fn ingest_show(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<String> {
        py.detach(|| db.ingest_show(name)).map_err(map_err)
}

pub(crate) fn ingest_drop(db: &mpedb::Database, py: Python<'_>, name: &str) -> PyResult<bool> {
        py.detach(|| db.ingest_drop(name)).map_err(map_err)
}

/// Open a streamed receipt. `mode` is `"dump"` (the whole table — the
/// only receipt that can see deletes) or `"delta"`.
pub(crate) fn ingest_begin(db: &mpedb::Database, py: Python<'_>, source: &str, target: &str, mode: &str) -> PyResult<i64> {
    let m = mpedb::ingest_run::Mode::parse(mode).map_err(map_err)?;
        py.detach(|| db.ingest_begin(source, target, m)).map_err(map_err)
}

/// Push one chunk of what you fetched. Rows are dicts keyed by column
/// name, or lists in the order of `columns`. `calls`/`bytes` are what
/// the call actually cost — mpedb cannot see the wire and trusts them.
pub(crate) fn ingest_rows(
    db: &mpedb::Database,
    py: Python<'_>,
    run_id: i64,
    rows: &Bound<'_, PyAny>,
    columns: Option<Vec<String>>,
    calls: i64,
    bytes: i64,
) -> PyResult<Py<PyAny>> {
    let (cols, vals) = rows_from_py(rows, columns)?;
        let r = py
        .detach(|| db.ingest_rows(run_id, &cols, &vals, calls, bytes))
        .map_err(map_err)?;
    receipt_dict(py, &r)
}

pub(crate) fn ingest_finish(db: &mpedb::Database, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
        let r = py.detach(|| db.ingest_finish(run_id)).map_err(map_err)?;
    receipt_dict(py, &r)
}

pub(crate) fn ingest_abandon(db: &mpedb::Database, py: Python<'_>, run_id: i64) -> PyResult<()> {
        py.detach(|| db.ingest_abandon(run_id)).map_err(map_err)
}

/// A whole small dump in one call: finds inserts, updates AND deletes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_dump(
    db: &mpedb::Database,
    py: Python<'_>,
    source: &str,
    target: &str,
    rows: &Bound<'_, PyAny>,
    columns: Option<Vec<String>>,
    calls: i64,
    bytes: i64,
) -> PyResult<Py<PyAny>> {
    let (cols, vals) = rows_from_py(rows, columns)?;
        let r = py
        .detach(|| db.ingest_dump(source, target, &cols, &vals, calls, bytes))
        .map_err(map_err)?;
    receipt_dict(py, &r)
}

/// A whole small delta in one call. Cannot see deletes, by definition.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_delta(
    db: &mpedb::Database,
    py: Python<'_>,
    source: &str,
    target: &str,
    rows: &Bound<'_, PyAny>,
    columns: Option<Vec<String>>,
    calls: i64,
    bytes: i64,
) -> PyResult<Py<PyAny>> {
    let (cols, vals) = rows_from_py(rows, columns)?;
        let r = py
        .detach(|| db.ingest_delta(source, target, &cols, &vals, calls, bytes))
        .map_err(map_err)?;
    receipt_dict(py, &r)
}

/// The observed model per edge: watermark, cursor verdict, change rate,
/// fan-out.
pub(crate) fn ingest_state(db: &mpedb::Database, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
        let st = py.detach(|| db.ingest_state(source)).map_err(map_err)?;
    let out = pyo3::types::PyDict::new(py);
    for (edge, s, overlap) in st {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("watermark", value_to_py(py, s.watermark.clone())?)?;
        d.set_item("cursor_col", &s.cursor_col)?;
        d.set_item("cursor_state", &s.cursor_state)?;
        d.set_item("caught", s.caught)?;
        d.set_item("missed", s.missed)?;
        d.set_item("receipts", s.receipts)?;
        d.set_item("changed_receipts", s.changed_receipts)?;
        d.set_item("fanout", s.fanout_per_call())?;
        d.set_item("lambda_per_poll", s.lambda_per_poll())?;
        d.set_item("overlap_secs", overlap)?;
        out.set_item(edge, d)?;
    }
    Ok(out.into_any().unbind())
}

/// The plan: which call, how often, in which profile — plus a `cron`
/// list ready to paste and a census of what could NOT be planned.
pub(crate) fn ingest_advise(db: &mpedb::Database, py: Python<'_>, source: &str, cmd: &str) -> PyResult<Py<PyAny>> {
        let plan = py.detach(|| db.ingest_advise(source)).map_err(map_err)?;
    let out = pyo3::types::PyDict::new(py);
    out.set_item("source", &plan.source)?;
    out.set_item("skipped", plan.skipped.clone())?;
    out.set_item("cron", plan.cron(cmd))?;
    let profiles = pyo3::types::PyList::empty(py);
    for p in &plan.profiles {
        let pd = pyo3::types::PyDict::new(py);
        pd.set_item("profile", &p.profile)?;
        pd.set_item("window_secs", p.window_secs)?;
        pd.set_item("budget_calls", p.budget_calls)?;
        pd.set_item("used_calls", p.used_calls)?;
        pd.set_item("used_bytes", p.used_bytes)?;
        pd.set_item("uniform_staleness", p.uniform_staleness)?;
        pd.set_item("solved_staleness", p.solved_staleness)?;
        pd.set_item("verdict", p.verdict())?;
        let es = pyo3::types::PyList::empty(py);
        for e in &p.edges {
            let ed = pyo3::types::PyDict::new(py);
            ed.set_item("edge", &e.edge)?;
            ed.set_item("table", &e.table)?;
            ed.set_item("kind", &e.kind)?;
            ed.set_item("strategy", &e.strategy)?;
            ed.set_item("rate_per_window", e.rate_per_window)?;
            ed.set_item("interval_secs", e.interval_secs)?;
            ed.set_item("cron", &e.cron)?;
            ed.set_item("fanout", e.fanout)?;
            ed.set_item("reason", &e.reason)?;
            es.append(ed)?;
        }
        pd.set_item("edges", es)?;
        profiles.append(pd)?;
    }
    out.set_item("profiles", profiles)?;
    Ok(out.into_any().unbind())
}

/// What the policy would not decide. Queryable IS the alert.
pub(crate) fn ingest_conflicts(db: &mpedb::Database, py: Python<'_>, source: &str) -> PyResult<Vec<Py<PyAny>>> {
        let cs = py.detach(|| db.ingest_conflicts(source)).map_err(map_err)?;
    cs.into_iter()
        .map(|c| {
            let d = pyo3::types::PyDict::new(py);
            d.set_item("tbl", c.table)?;
            d.set_item("k", value_to_py(py, c.key.clone())?)?;
            d.set_item("kind", c.kind)?;
            d.set_item("detail", c.detail)?;
            Ok(d.into_any().unbind())
        })
        .collect()
}

pub(crate) fn ingest_resolve(db: &mpedb::Database, py: Python<'_>, source: &str, take: &str) -> PyResult<u64> {
        py.detach(|| db.ingest_resolve(source, take)).map_err(map_err)
}


// ------------------------------------------------------------- the cascade

/// Queue derived calls from a receipt's keys, in the SAME transaction as
/// the rows that produced them. Returns how many were queued — a key
/// already waiting is not queued twice.
pub(crate) fn ingest_derive(
    db: &mpedb::Database,
    py: Python<'_>,
    run_id: i64,
    edge: &str,
    keys: &Bound<'_, PyAny>,
) -> PyResult<u64> {
    let mut ks = Vec::new();
    for k in keys.try_iter()? {
        ks.push(py_to_value(&k?)?);
    }
    py.detach(|| db.ingest_derive(run_id, edge, &ks)).map_err(map_err)
}

/// The next batch of derived calls this window's budget allows, or `None`
/// when it is spent — which is the budget working, not an error.
pub(crate) fn ingest_next(
    db: &mpedb::Database,
    py: Python<'_>,
    source: &str,
) -> PyResult<Option<Py<PyAny>>> {
    let Some(t) = py.detach(|| db.ingest_next(source)).map_err(map_err)? else {
        return Ok(None);
    };
    let d = pyo3::types::PyDict::new(py);
    d.set_item("lease", t.lease)?;
    d.set_item("edge", &t.edge)?;
    d.set_item("table", &t.table)?;
    let ks = pyo3::types::PyList::empty(py);
    for k in &t.keys {
        ks.append(value_to_py(py, k.clone())?)?;
    }
    d.set_item("keys", ks)?;
    Ok(Some(d.into_any().unbind()))
}

/// Retire a leased batch. What it FETCHED went in through an ordinary
/// receipt; this only says the keys are handled.
pub(crate) fn ingest_done(db: &mpedb::Database, py: Python<'_>, source: &str, lease: i64) -> PyResult<u64> {
    py.detach(|| db.ingest_done(source, lease)).map_err(map_err)
}

/// Give a leased batch back — a fetch that failed. Nothing is lost.
pub(crate) fn ingest_release(db: &mpedb::Database, py: Python<'_>, source: &str, lease: i64) -> PyResult<u64> {
    py.detach(|| db.ingest_release(source, lease)).map_err(map_err)
}

/// Reclaim leases held by workers that never came back.
pub(crate) fn ingest_reap(
    db: &mpedb::Database,
    py: Python<'_>,
    source: &str,
    older_than_secs: i64,
) -> PyResult<u64> {
    py.detach(|| mpedb::ingest_task::reap_leases(db, source, older_than_secs))
        .map_err(map_err)
}

/// How much derived work waits, per edge. A queue that only grows means
/// the budget cannot keep up with the fan-out.
pub(crate) fn ingest_pending(db: &mpedb::Database, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
    let ps = py.detach(|| db.ingest_pending(source)).map_err(map_err)?;
    let out = pyo3::types::PyDict::new(py);
    for (edge, n, leased) in ps {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("waiting", n)?;
        d.set_item("leased", leased > 0)?;
        out.set_item(edge, d)?;
    }
    Ok(out.into_any().unbind())
}

/// What is left of this window's budget, from what the receipts reported.
pub(crate) fn ingest_budget_left(db: &mpedb::Database, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
    let b = py.detach(|| db.ingest_budget_left(source)).map_err(map_err)?;
    let d = pyo3::types::PyDict::new(py);
    d.set_item("profile", &b.profile)?;
    d.set_item("calls", b.calls)?;
    d.set_item("bytes", b.bytes)?;
    d.set_item("window_secs", b.window_secs)?;
    Ok(d.into_any().unbind())
}
