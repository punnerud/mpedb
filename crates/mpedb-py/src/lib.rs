//! Python bindings for mpedb. Importable module name: `mpedb`.
//!
//! Design notes:
//! - No module-level mutable state; every handle owns its state behind
//!   `Arc`/`Mutex`, so the module is friendly to free-threaded CPython and to
//!   many interpreters in one process.
//! - Every engine call runs inside `Python::detach`, so other Python
//!   threads (and, with the GIL released, MVCC readers in this process) make
//!   progress while the engine works.
//! - The GIL-released closures never create Python objects; parameters are
//!   converted to `mpedb::Value` before releasing the GIL and results are
//!   converted back after re-acquiring it.
//! - Locking rules are inherited from the Rust facade: never call
//!   `Database.prepare` / `Database.verify` / an uncached `Database.query`
//!   while a `Transaction` from the same handle is open on the same thread.

mod pydbapi;
mod pyingest;
mod pyrretl;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, Utc};
use mpedb::{
    Database as Db, DetachedPlan, Error as DbError, ExecResult, PlanHash, Value, WriteSession,
};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyFloat, PyInt, PyList, PyString, PyTuple};
use pyo3::IntoPyObjectExt;

// --------------------------------------------------------------- exceptions

create_exception!(mpedb, Error, PyException, "Base class for all mpedb errors.");
create_exception!(
    mpedb,
    IntegrityError,
    Error,
    "Constraint violation: primary key, UNIQUE, NOT NULL, or CHECK."
);
create_exception!(
    mpedb,
    ProgrammingError,
    Error,
    "SQL / API misuse: parse, bind, type mismatch, wrong parameter count, \
     unknown or invalidated plan, unsupported statement."
);
create_exception!(
    mpedb,
    OperationalError,
    Error,
    "Runtime failure: I/O, corruption, capacity (DbFull/ReadersFull), evicted \
     snapshot, config/schema mismatch, poisoned write session, engine internals."
);

fn map_err(e: DbError) -> PyErr {
    let msg = e.to_string();
    match &e {
        DbError::PrimaryKeyViolation { .. }
        | DbError::UniqueViolation { .. }
        | DbError::NotNullViolation { .. }
        | DbError::CheckViolation { .. } => IntegrityError::new_err(msg),
        // sqlite raises OperationalError ("no such table/column") for unknown
        // schema objects, and real consumers catch by THAT class — diskcache
        // probes its Settings table on every open and swallows
        // OperationalError; a ProgrammingError there crashed Cache.__init__
        // universally (PY-COMPAT.md tier 1). Same taxonomy here.
        DbError::Bind(s) if s.contains("unknown table") || s.contains("unknown column") => {
            OperationalError::new_err(msg)
        }
        DbError::Parse { .. }
        | DbError::Bind(_)
        | DbError::TypeMismatch(_)
        | DbError::WrongParamCount { .. }
        | DbError::UnknownPlan(_)
        | DbError::PlanInvalidated => ProgrammingError::new_err(msg),
        // A poisoned WriteSession surfaces as Error::Unsupported("transaction
        // poisoned by a partially-applied statement; ...") in the facade; the
        // Python API promises OperationalError for it.
        DbError::Unsupported(s) if s.contains("poisoned") => OperationalError::new_err(msg),
        DbError::Unsupported(_) => ProgrammingError::new_err(msg),
        // Io, Corrupt, DbFull, ReadersFull, SnapshotEvicted, Config, Schema,
        // Internal, DivisionByZero, ArithmeticOverflow, and anything new.
        _ => OperationalError::new_err(msg),
    }
}

pub(crate) fn closed_err() -> PyErr {
    ProgrammingError::new_err("transaction is already closed (committed or rolled back)")
}

// ---------------------------------------------------------- value conversion

/// Python -> Value. Checked in order; `bool` MUST precede `int` because
/// Python's bool is a subclass of int.
pub(crate) fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if obj.cast::<PyInt>().is_ok() {
        // extract::<i64> raises OverflowError for out-of-range ints.
        return Ok(Value::Int(obj.extract::<i64>()?));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(Value::Float(f.value()));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::Text(s.to_str()?.to_owned()));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(Value::Blob(b.as_bytes().to_vec()));
    }
    if let Ok(b) = obj.cast::<PyByteArray>() {
        return Ok(Value::Blob(b.to_vec()));
    }
    // memoryview (sqlite3.Binary) binds as a blob — 13 diskcache tests bind
    // one directly, and PEP 249's Binary() constructor returns exactly this.
    if let Ok(mv) = obj.cast::<pyo3::types::PyMemoryView>() {
        if let Ok(b) = mv.call_method0("tobytes") {
            if let Ok(by) = b.cast::<PyBytes>() {
                return Ok(Value::Blob(by.as_bytes().to_vec()));
            }
        }
    }
    // Aware datetime (any fixed offset) -> UTC microseconds.
    if let Ok(dt) = obj.extract::<DateTime<FixedOffset>>() {
        return Ok(Value::Timestamp(dt.with_timezone(&Utc).timestamp_micros()));
    }
    // Naive datetime: treated as UTC. Checked BEFORE date/time, because a
    // `datetime` is also a `date` in Python and extracting it as one would
    // silently drop the clock.
    if let Ok(dt) = obj.extract::<NaiveDateTime>() {
        return Ok(Value::Timestamp(dt.and_utc().timestamp_micros()));
    }
    if let Ok(d) = obj.extract::<chrono::NaiveDate>() {
        return Ok(Value::Date(
            i64::from(d.num_days_from_ce()) - i64::from(UNIX_EPOCH_CE_DAYS),
        ));
    }
    if let Ok(t) = obj.extract::<chrono::NaiveTime>() {
        use chrono::Timelike;
        return Ok(Value::Time(
            i64::from(t.num_seconds_from_midnight()) * 1_000_000
                + i64::from(t.nanosecond() / 1000),
        ));
    }
    // `decimal.Decimal` binds as an exact decimal, never through a float.
    if obj.get_type().name().is_ok_and(|n| n == "Decimal") {
        let s = obj.str()?.extract::<String>()?;
        if let Some(n) = mpedb::parse_numeric(&s) {
            return Ok(Value::Numeric(n));
        }
    }
    Err(PyTypeError::new_err(format!(
        "cannot bind {} as an mpedb parameter \
         (expected None, bool, int, float, str, bytes/bytearray, datetime, \
         date, time, or Decimal)",
        obj.get_type()
    )))
}

/// Days from the proleptic-Gregorian epoch (year 1, day 1 — what chrono's
/// `num_days_from_ce` counts) to 1970-01-01, which is where mpedb's `Date`
/// counts from.
const UNIX_EPOCH_CE_DAYS: i32 = 719_163;

/// Value -> Python. Timestamps come back as timezone-aware
/// `datetime.datetime` in UTC.
pub(crate) fn value_to_py<'py>(py: Python<'py>, v: Value) -> PyResult<Bound<'py, PyAny>> {
    match v {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Int(x) => x.into_bound_py_any(py),
        Value::Float(x) => x.into_bound_py_any(py),
        Value::Bool(x) => x.into_bound_py_any(py),
        Value::Text(s) => s.into_bound_py_any(py),
        Value::Blob(b) => PyBytes::new(py, &b).into_bound_py_any(py),
        Value::Timestamp(us) => DateTime::<Utc>::from_timestamp_micros(us)
            .ok_or_else(|| {
                OperationalError::new_err(format!(
                    "stored timestamp out of datetime range: {us} microseconds"
                ))
            })?
            .into_bound_py_any(py),
        // The types a PostgreSQL client asks for, in the Python objects it
        // expects back: `datetime.date`, `datetime.time`, `decimal.Decimal`.
        // Anything else here would be the round trip silently failing — a
        // `date` coming back as the integer 18_264 is the bug this whole type
        // surface exists to close.
        Value::Date(days) => chrono::NaiveDate::from_num_days_from_ce_opt(
            days.saturating_add(i64::from(UNIX_EPOCH_CE_DAYS)).try_into().unwrap_or(i32::MAX),
        )
        .ok_or_else(|| OperationalError::new_err(format!("stored date out of range: {days} days")))?
        .into_bound_py_any(py),
        Value::Time(us) => chrono::NaiveTime::from_num_seconds_from_midnight_opt(
            (us.div_euclid(1_000_000)).try_into().unwrap_or(u32::MAX),
            (us.rem_euclid(1_000_000) * 1000) as u32,
        )
        .ok_or_else(|| {
            OperationalError::new_err(format!("stored time out of range: {us} microseconds"))
        })?
        .into_bound_py_any(py),
        // Through `decimal.Decimal`'s TEXT constructor, which is exact — the
        // float constructor is not, and this type exists precisely so the
        // digits survive.
        Value::Numeric(n) => py
            .import("decimal")?
            .getattr("Decimal")?
            .call1((n,)),
        // A context list (§2.6) is param-only, so no query result can contain
        // one. Render it as a Python list anyway rather than erroring: this is
        // an output conversion, and the shape maps exactly.
        Value::List(items) => {
            let out = pyo3::types::PyList::empty(py);
            for it in items {
                out.append(value_to_py(py, it)?)?;
            }
            out.into_bound_py_any(py)
        }
    }
}

pub(crate) fn rows_to_py<'py>(py: Python<'py>, rows: Vec<Vec<Value>>) -> PyResult<Bound<'py, PyList>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(row.len());
        for v in row {
            cells.push(value_to_py(py, v)?);
        }
        out.push(PyTuple::new(py, cells)?);
    }
    PyList::new(py, out)
}

/// SELECT -> list[tuple]; DML -> int (affected rows); EXPLAIN -> str.
fn exec_result_to_py(py: Python<'_>, res: ExecResult) -> PyResult<Py<PyAny>> {
    match res {
        ExecResult::Rows { rows, .. } => Ok(rows_to_py(py, rows)?.into_any().unbind()),
        ExecResult::Affected(n) => n.into_py_any(py),
        ExecResult::Explain(s) => s.into_py_any(py),
    }
}

/// `params` may be None or any non-str/bytes iterable (list, tuple, ...).
fn convert_params(params: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    let Some(obj) = params else {
        return Ok(Vec::new());
    };
    if obj.is_none() {
        return Ok(Vec::new());
    }
    if obj.cast::<PyString>().is_ok() || obj.cast::<PyBytes>().is_ok() {
        return Err(PyTypeError::new_err(
            "params must be a sequence of values (list/tuple), not str or bytes",
        ));
    }
    let mut out = Vec::new();
    for item in obj.try_iter()? {
        out.push(py_to_value(&item?)?);
    }
    Ok(out)
}

/// The sqlite3-compat variant of [`convert_params`]: stdlib sqlite3 binds
/// Python `True`/`False` as the integers 1/0 (sqlite has no bool storage
/// class), and real code compares them against INTEGER columns — diskcache's
/// `raw = ?` is exactly that. The native `mpedb.Database` API keeps
/// `Value::Bool` for declared bool columns; only the drop-in surface flattens.
fn convert_params_sqlite3(params: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<Value>> {
    let Some(obj) = params else {
        return Ok(Vec::new());
    };
    if obj.is_none() {
        return Ok(Vec::new());
    }
    if obj.cast::<PyString>().is_ok() || obj.cast::<PyBytes>().is_ok() {
        return Err(PyTypeError::new_err(
            "params must be a sequence of values (list/tuple), not str or bytes",
        ));
    }
    let mut out = Vec::new();
    for item in obj.try_iter()? {
        let item = item?;
        let v = convert_one_sqlite3(&item)?;
        out.push(match v {
            Value::Bool(b) => Value::Int(i64::from(b)),
            other => other,
        });
    }
    Ok(out)
}

/// One bound value on the sqlite3 surface, with the stdlib's adapter chain:
/// the BASE types bind directly; anything else consults the registered
/// adapters (`mpedb.adapters`, exact type, `PrepareProtocol`) FIRST — Django
/// registers its own date/datetime/Decimal adapters and depends on them
/// winning — then the object's own `__conform__(PrepareProtocol)`, and only
/// then the native conversion (which covers datetime when nothing is
/// registered, like the stdlib's default adapters).
fn convert_one_sqlite3(item: &Bound<'_, PyAny>) -> PyResult<Value> {
    let py = item.py();
    let is_base = item.is_none()
        || item.cast::<pyo3::types::PyBool>().is_ok()
        || item.cast::<pyo3::types::PyInt>().is_ok()
        || item.cast::<pyo3::types::PyFloat>().is_ok()
        || item.cast::<PyString>().is_ok()
        || item.cast::<PyBytes>().is_ok()
        || item.cast::<pyo3::types::PyByteArray>().is_ok()
        || item.cast::<pyo3::types::PyMemoryView>().is_ok();
    if is_base {
        return py_to_value(item);
    }
    if let Ok(module) = py.import("mpedb") {
        if let (Ok(proto), Ok(adapters)) =
            (module.getattr("PrepareProtocol"), module.getattr("adapters"))
        {
            let key = pyo3::types::PyTuple::new(
                py,
                [item.get_type().into_any(), proto.clone()],
            )?;
            if let Ok(adapter) = adapters.get_item(&key) {
                let adapted = adapter.call1((item,))?;
                return py_to_value(&adapted);
            }
            if let Ok(conform) = item.getattr("__conform__") {
                let adapted = conform.call1((proto,))?;
                if !adapted.is_none() {
                    return py_to_value(&adapted);
                }
            }
        }
    }
    py_to_value(item)
}

fn parse_hash(plan_hash: &str) -> PyResult<PlanHash> {
    plan_hash.parse::<PlanHash>().map_err(|_| {
        ProgrammingError::new_err(format!(
            "invalid plan hash (expected 64 hex chars): {plan_hash:?}"
        ))
    })
}

// -------------------------------------------- int -> timestamp param coercion

/// The facade validates parameters before executing anything and reports
/// exactly "parameter $N is int64, statement requires timestamp". The Python
/// API accepts ints as raw microseconds for timestamp parameters, so on that
/// precise pre-execution error we coerce the offending Int param and retry.
/// Returns the 0-based index to coerce, or None.
fn coercible_param(e: &DbError, params: &[Value]) -> Option<usize> {
    let DbError::TypeMismatch(msg) = e else {
        return None;
    };
    let rest = msg.strip_prefix("parameter $")?;
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    if &rest[digits_end..] != " is int64, statement requires timestamp" {
        return None;
    }
    let n: usize = rest[..digits_end].parse().ok()?;
    let i = n.checked_sub(1)?;
    match params.get(i) {
        Some(Value::Int(_)) => Some(i),
        _ => None,
    }
}

/// Run `f` with `params`, upgrading Int params to Timestamp (raw µs) when the
/// pre-execution validator asks for it. Terminates: each retry replaces one
/// Int with a Timestamp, which can never trip the same message again.
fn run_coercing<F>(mut params: Vec<Value>, mut f: F) -> Result<ExecResult, DbError>
where
    F: FnMut(&[Value]) -> Result<ExecResult, DbError>,
{
    loop {
        match f(&params) {
            Err(e) => match coercible_param(&e, &params) {
                Some(i) => {
                    let Value::Int(x) = params[i] else { unreachable!() };
                    params[i] = Value::Timestamp(x);
                }
                None => return Err(e),
            },
            ok => return ok,
        }
    }
}

// ------------------------------------------------------------------ Database

/// An open database handle (opens or creates the database described by a
/// TOML config file). Thread-safe; share one handle across threads.
#[pyclass(frozen, name = "Database", module = "mpedb")]
struct PyDatabase {
    db: Arc<Db>,
    /// Threads of this handle that currently have a `Transaction` open (#161).
    /// A `Vec` and not a set: the count is 0 or 1 in every sane program, and
    /// the check runs once per call.
    open_txns: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

/// Registers "a transaction from this handle is open on thread T" for exactly
/// as long as it lives (#161).
///
/// Stored INSIDE the same `Option` as the session, so every path that ends a
/// transaction — `commit`, `rollback`, `__exit__`, or the Python object simply
/// being collected — releases it with no separate step anyone can forget. A
/// registration that has to be un-done by hand is one that eventually is not.
struct TxnGuard {
    open: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    thread: std::thread::ThreadId,
}

impl Drop for TxnGuard {
    fn drop(&mut self) {
        if let Ok(mut v) = self.open.lock() {
            if let Some(i) = v.iter().position(|t| *t == self.thread) {
                v.swap_remove(i);
            }
        }
    }
}

impl PyDatabase {
    /// Refuse a `Database`-level call that would deadlock against a
    /// `Transaction` this same thread already has open (#161).
    ///
    /// The four locking rules used to be documentation. Two of them can be
    /// ENFORCED, and this is the first: a `Database` method that may publish a
    /// plan, or autocommit DML, takes the single writer lock — which the open
    /// transaction is holding. Same thread, same lock: the call does not fail,
    /// it HANGS, and a hang is the least debuggable outcome an API can offer.
    ///
    /// Same-thread only, deliberately. Another thread calling `db.query` while
    /// this one holds the writer lock is ordinary contention and waits its
    /// turn; refusing that would break legitimate concurrency.
    fn refuse_if_txn_open(&self, method: &str, hint: &str) -> PyResult<()> {
        let me = std::thread::current().id();
        let open = self.open_txns.lock().expect("open-txn registry poisoned");
        if open.contains(&me) {
            return Err(ProgrammingError::new_err(format!(
                "Database.{method}() cannot run while a Transaction from this \
                 handle is open on this thread — it needs the single writer lock, \
                 which that transaction holds, so the call would block forever \
                 rather than fail. {hint}"
            )));
        }
        Ok(())
    }
}

#[pymethods]
impl PyDatabase {
    #[new]
    fn new(py: Python<'_>, config_path: PathBuf) -> PyResult<Self> {
        let db = py
            .detach(move || Db::open(&config_path))
            .map_err(map_err)?;
        Ok(PyDatabase {
            db: Arc::new(db),
            open_txns: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Compile SQL to a content-hashed plan, publish it in the shared
    /// registry, and return the 64-hex plan hash.
    fn prepare(&self, py: Python<'_>, sql: &str) -> PyResult<String> {
        self.refuse_if_txn_open("prepare", "Commit or roll back the transaction first.")?;
        let db = &self.db;
        let h = py.detach(|| db.prepare(sql)).map_err(map_err)?;
        Ok(h.to_string())
    }

    /// Compile SQL to a **detached (client-borne) plan** WITHOUT publishing it
    /// to the shared registry. Returns `(hash, blob, sql)` where `hash` is the
    /// 64-hex plan hash and `blob` is a self-describing bytes object to ship to
    /// (or store for) `execute_detached` — a second process/handle can execute
    /// it with no registry entry. The plan is NOT in the registry, so
    /// `execute(hash, ...)` for the same hash raises until someone `prepare`s
    /// it.
    fn prepare_detached(&self, py: Python<'_>, sql: &str) -> PyResult<(String, Py<PyBytes>, String)> {
        let db = &self.db;
        let dp = py.detach(|| db.prepare_detached(sql)).map_err(map_err)?;
        let hash = dp.hash.to_string();
        let blob = PyBytes::new(py, &dp.encode()).unbind();
        Ok((hash, blob, dp.sql))
    }

    /// Execute a detached plan `blob` (as returned by `prepare_detached`),
    /// validating its integrity against this database's schema and NEVER
    /// touching the shared registry. SELECT -> list[tuple]; DML -> int. A blob
    /// that does not match its carried hash raises OperationalError (corrupt);
    /// one built against a different schema raises ProgrammingError
    /// (invalidated — re-prepare).
    #[pyo3(signature = (blob, params=None))]
    fn execute_detached(
        &self,
        py: Python<'_>,
        blob: &[u8],
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let vals = convert_params(params)?;
        let db = &self.db;
        let res = py
            .detach(move || -> Result<ExecResult, DbError> {
                let dp = DetachedPlan::decode(blob)?;
                run_coercing(vals, |p| db.execute_detached(&dp, p))
            })
            .map_err(map_err)?;
        exec_result_to_py(py, res)
    }

    /// A caching client [`Session`]: send SQL, and the session compiles each
    /// distinct statement once (client-side, as a detached plan), caches it
    /// locally, and executes by hash thereafter — no re-parsing, no registry
    /// write per statement, and transparent recovery on a schema change.
    fn session(&self) -> PySession {
        PySession {
            db: self.db.clone(),
            plans: Mutex::new(HashMap::new()),
        }
    }

    /// Execute a prepared plan by 64-hex hash (prepared by ANY process
    /// attached to this database). SELECT -> list[tuple]; DML -> int.
    #[pyo3(signature = (plan_hash, params=None))]
    fn execute(
        &self,
        py: Python<'_>,
        plan_hash: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let hash = parse_hash(plan_hash)?;
        let vals = convert_params(params)?;
        let db = &self.db;
        let res = py
            .detach(move || run_coercing(vals, |p| db.execute(&hash, p)))
            .map_err(map_err)?;
        exec_result_to_py(py, res)
    }

    /// One-shot prepare + execute. SELECT -> list[tuple]; DML -> int;
    /// `EXPLAIN <stmt>` -> str.
    #[pyo3(signature = (sql, params=None))]
    fn query(
        &self,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("query", "Use the transaction's own `txn.query(...)`, which runs on its snapshot and sees its uncommitted writes.")?;
        let vals = convert_params(params)?;
        let db = &self.db;
        let res = py
            .detach(move || run_coercing(vals, |p| db.query(sql, p)))
            .map_err(map_err)?;
        exec_result_to_py(py, res)
    }

    /// Like `query`, but returns `(columns, rows)` for callers that need
    /// output column names. Raises ProgrammingError for non-row statements.
    #[pyo3(signature = (sql, params=None))]
    fn query_full(
        &self,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<(Vec<String>, Py<PyList>)> {
        self.refuse_if_txn_open("query_full", "Use the transaction's own `txn.query(...)`, which runs on its snapshot and sees its uncommitted writes.")?;
        let vals = convert_params(params)?;
        let db = &self.db;
        let res = py
            .detach(move || run_coercing(vals, |p| db.query(sql, p)))
            .map_err(map_err)?;
        match res {
            ExecResult::Rows { columns, rows } => {
                Ok((columns, rows_to_py(py, rows)?.unbind()))
            }
            _ => Err(ProgrammingError::new_err(
                "query_full requires a statement that returns rows (SELECT)",
            )),
        }
    }

    /// Render the plan for `sql` without executing it.
    fn explain(&self, py: Python<'_>, sql: &str) -> PyResult<String> {
        let trimmed = sql.trim_start();
        let already = trimmed.len() >= 7
            && trimmed[..7].eq_ignore_ascii_case("explain")
            && trimmed.as_bytes().get(7).is_none_or(|c| c.is_ascii_whitespace());
        let text = if already {
            sql.to_owned()
        } else {
            format!("EXPLAIN {sql}")
        };
        let db = &self.db;
        let res = py
            .detach(move || db.query(&text, &[]))
            .map_err(map_err)?;
        match res {
            ExecResult::Explain(s) => Ok(s),
            _ => Err(ProgrammingError::new_err("EXPLAIN produced no plan text")),
        }
    }

    /// Names of all tables in the schema.
    /// LIVE tables only. `DROP TABLE` retires a slot in place with a tombstone
    /// whose name is empty, and this returned it as `""` — a table that cannot
    /// be queried, in the list a program iterates to find out what it can query
    /// (#163).
    fn tables(&self) -> Vec<String> {
        self.db.schema().live_tables().map(|t| t.name.clone()).collect()
    }

    /// Verify the engine's page-accounting invariant; raises on failure.
    /// Takes the writer lock briefly — never call with an open Transaction
    /// on this thread.
    fn verify(&self, py: Python<'_>) -> PyResult<()> {
        self.refuse_if_txn_open("verify", "Commit or roll back the transaction first.")?;
        let db = &self.db;
        py.detach(move || db.verify()).map_err(map_err)
    }

    /// Start an interactive write transaction (holds the single writer lock).
    /// Use as a context manager: commits on clean exit, rolls back on
    /// exception. A statement that fails after partially applying its effects
    /// poisons the session: further calls and commit raise OperationalError;
    /// only rollback (or `with`-exit via exception) is allowed.
    fn begin(&self, py: Python<'_>) -> PyResult<PyTransaction> {
        // A second `begin()` on the same thread would block on the writer lock
        // this thread already holds — the same hang, from the most obvious
        // possible mistake (#161).
        self.refuse_if_txn_open(
            "begin",
            "A transaction is not re-entrant; commit or roll back the first one.",
        )?;
        let db = self.db.clone();
        let session = py
            .detach(|| -> Result<WriteSession<'static>, DbError> {
                let session = db.begin()?;
                // SAFETY: the session borrows the Database inside `db` (an
                // Arc, so the referent has a stable heap address and never
                // moves). PyTransaction stores this Arc clone in `_db`,
                // declared AFTER `session`, so the borrow is dropped before
                // its referent can be freed.
                Ok(unsafe {
                    std::mem::transmute::<WriteSession<'_>, WriteSession<'static>>(session)
                })
            })
            .map_err(map_err)?;
        let me = std::thread::current().id();
        self.open_txns
            .lock()
            .expect("open-txn registry poisoned")
            .push(me);
        Ok(PyTransaction {
            session: Mutex::new(Some((
                session,
                TxnGuard { open: self.open_txns.clone(), thread: me },
            ))),
            owner: me,
            _db: db,
        })
    }

    // ------------------------------------------------- sync (#157)

    /// This process's sync role: `"standalone"`, `"replica"` or `"authority"`,
    /// from `[sync] role` in the config. A deployment fact, not a file property.
    #[getter]
    fn role(&self) -> &'static str {
        self.db.role().as_str()
    }

    /// Turn change capture on for `tables` on BOTH this database and
    /// `upstream`. Required once before any sync; without it the change log
    /// stays empty and every sync is a silent no-op.
    fn sync_enable(
        &self,
        py: Python<'_>,
        upstream: &PyDatabase,
        tables: Vec<String>,
    ) -> PyResult<()> {
        let (a, b) = (&self.db, &upstream.db);
        py.detach(|| {
            let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
            mpedb::sync::SyncLink::new(a, b, 1).enable(&names)
        })
        .map_err(map_err)
    }

    /// Push local changes up, then pull everything down. Returns a dict:
    /// `{"pulled": n, "pushed": n, "deleted": n, "conflicts": n, "cursor": n}`.
    ///
    /// `link` distinguishes this link's cursors from another's — two replicas
    /// of one upstream must not share a number.
    ///
    /// `resolve` is `"upstream-wins"` (default) or `"local-wins"`: who wins when
    /// both ends changed the same row. Neither merges — for a value many people
    /// edit at once, use `submit_batch` instead so the edits compose.
    #[pyo3(signature = (upstream, tables, link=1, resolve="upstream-wins"))]
    fn sync(
        &self,
        py: Python<'_>,
        upstream: &PyDatabase,
        tables: Vec<String>,
        link: u64,
        resolve: &str,
    ) -> PyResult<std::collections::HashMap<String, u64>> {
        let (a, b) = (&self.db, &upstream.db);
        let policy = mpedb::sync::Resolve::parse(resolve).map_err(map_err)?;
        let out = py
            .detach(|| {
                let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
                mpedb::sync::SyncLink::new(a, b, link)
                    .with_resolve(policy)
                    .sync(&names)
            })
            .map_err(map_err)?;
        Ok(std::collections::HashMap::from([
            ("pulled".to_string(), out.pulled.upserts),
            ("pushed".to_string(), out.pushed.upserts),
            ("deleted".to_string(), out.pulled.deletes + out.pushed.deletes),
            // Conflicts are decided at PUSH; the pull half never reports any.
            ("conflicts".to_string(), out.pushed.conflicts),
            // Rows this call rewrote LOCALLY — upstream values adopted for rows
            // we lost. Without it a sync can change local data silently.
            ("local_writes".to_string(), out.pushed.local_writes),
            ("cursor".to_string(), out.pulled.cursor),
        ]))
    }

    /// Copy everything the upstream already has, once, when attaching a replica
    /// to a database that is **not empty**.
    ///
    /// `sync_enable` is not retroactive: rows written before capture was turned
    /// on are in no change log and replicate never, silently. This is the step
    /// that fixes it. O(rows) — for bootstrap, not for a schedule.
    #[pyo3(signature = (upstream, tables, link=1))]
    fn sync_seed(
        &self,
        py: Python<'_>,
        upstream: &PyDatabase,
        tables: Vec<String>,
        link: u64,
    ) -> PyResult<u64> {
        let (a, b) = (&self.db, &upstream.db);
        let rep = py
            .detach(|| {
                let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
                mpedb::sync::SyncLink::new(a, b, link).seed(&names)
            })
            .map_err(map_err)?;
        Ok(rep.upserts)
    }

    /// How many changed rows this database is behind `upstream` — the number a
    /// UI shows as "syncing…".
    #[pyo3(signature = (upstream, tables, link=1))]
    fn sync_lag(
        &self,
        py: Python<'_>,
        upstream: &PyDatabase,
        tables: Vec<String>,
        link: u64,
    ) -> PyResult<u64> {
        let (a, b) = (&self.db, &upstream.db);
        py.detach(|| {
            let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
            mpedb::sync::SyncLink::new(a, b, link).lag(&names)
        })
        .map_err(map_err)
    }

    /// Per-table `(row_count, content_hash)` — how you assert that two
    /// databases actually agree. Order-independent, so scan order does not
    /// matter.
    fn fingerprint(
        &self,
        py: Python<'_>,
        tables: Vec<String>,
    ) -> PyResult<std::collections::HashMap<String, (u64, u64)>> {
        let db = &self.db;
        let fp = py
            .detach(|| {
                let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
                mpedb::sync::fingerprint(db, &names)
            })
            .map_err(map_err)?;
        Ok(fp.into_iter().collect())
    }

    /// Apply many sub-edits to one text cell in ONE transaction, and answer each
    /// one (#153/#155).
    ///
    /// `edits` is a list of dicts with keys `editor`, `seq`, `at`, `remove`,
    /// `insert` and `key`. `seq` is the ORDER the editors acted in — assigned by
    /// the caller, never by the engine — so the same edits produce the same text
    /// however they arrive.
    ///
    /// Returns one string per edit: `"committed"`, `"provisional"` (a replica's
    /// local commit, not yet confirmed by an authority), `"lost"` or
    /// `"deadline"`.
    fn submit_batch(
        &self,
        py: Python<'_>,
        table: &str,
        column: &str,
        edits: Vec<std::collections::HashMap<String, Bound<'_, PyAny>>>,
    ) -> PyResult<Vec<&'static str>> {
        let mut subs = Vec::with_capacity(edits.len());
        for (i, e) in edits.iter().enumerate() {
            let need = |k: &str| -> PyResult<&Bound<'_, PyAny>> {
                e.get(k).ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err(format!(
                        "edit {i} is missing key {k:?} (need editor, seq, key, at, remove, insert)"
                    ))
                })
            };
            subs.push(mpedb::collab::Submission {
                editor: need("editor")?.extract()?,
                seq: need("seq")?.extract()?,
                snap: match e.get("snap") {
                    Some(v) => v.extract()?,
                    None => self.db.snapshot_txn(),
                },
                key: need("key")?.extract()?,
                at: need("at")?.extract()?,
                remove: need("remove")?.extract()?,
                insert: need("insert")?.extract()?,
            });
        }
        let db = &self.db;
        let verdicts = py
            .detach(|| db.submit_batch(table, column, &subs))
            .map_err(map_err)?;
        Ok(verdicts
            .into_iter()
            .map(|v| match v {
                mpedb::collab::EditVerdict::Committed => "committed",
                mpedb::collab::EditVerdict::Provisional { .. } => "provisional",
                mpedb::collab::EditVerdict::Lost { .. } => "lost",
                mpedb::collab::EditVerdict::DeadlineExpired => "deadline",
            })
            .collect())
    }

    // ------------------------------------------------------ rRETL (Reversible ETL)

    /// Store a PySpell function from PYTHON SOURCE (a `def name(args): ...`
    /// in the deterministic subset — see PYSPELL-RRETL.md). The function's name
    /// and arity come from the definition itself. Returns (name, hex hash).
    fn define_function(&self, py: Python<'_>, source: &str) -> PyResult<(String, String)> {
        self.refuse_if_txn_open("define_function", "Commit or roll back first.")?;
        let db = &self.db;
        py.detach(|| db.create_function(mpedb::spellfn::SpellLang::Python, source))
            .map_err(map_err)
    }

    /// Register a BIJECTIVE lens pair over two stored functions, or a LOSSY
    /// one (`residual` class goes through `create_residual_lens`). The
    /// declaration is VERIFIED against the probe corpus and refused with a
    /// named counter-example if it does not hold. Returns the sample count.
    /// `probes` extends the built-in verification corpus with YOUR domain's
    /// edge values — a pair that breaks on one is refused AT REGISTRATION,
    /// value named, instead of aborting the first apply that meets it.
    #[pyo3(signature = (name, forward, inverse, r#class="bijective", probes=None))]
    fn create_lens(
        &self,
        py: Python<'_>,
        name: &str,
        forward: &str,
        inverse: &str,
        r#class: &str,
        probes: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<u32> {
        self.refuse_if_txn_open("create_lens", "Commit or roll back first.")?;
        let class = mpedb::lens::LensClass::parse(r#class).map_err(map_err)?;
        let probes = probes
            .unwrap_or_default()
            .iter()
            .map(py_to_value)
            .collect::<PyResult<Vec<_>>>()?;
        let db = &self.db;
        py.detach(|| db.create_lens_with_probes(name, forward, inverse, class, &probes))
            .map_err(map_err)
    }

    /// Register a RESIDUAL lens triple: forward/1, rex/1 (the residual
    /// extractor — what forward loses), inverse/2. The declared residual type
    /// is verified against actual rex outputs. Returns the sample count.
    #[pyo3(signature = (name, forward, rex, inverse, residual_type, probes=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_residual_lens(
        &self,
        py: Python<'_>,
        name: &str,
        forward: &str,
        rex: &str,
        inverse: &str,
        residual_type: &str,
        probes: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<u32> {
        self.refuse_if_txn_open("create_residual_lens", "Commit or roll back first.")?;
        let rt = mpedb::ColumnType::parse(residual_type).ok_or_else(|| {
            ProgrammingError::new_err(format!(
                "unknown residual type {residual_type:?} — expected int64, float64, bool, \
                 text, blob, timestamp or any"
            ))
        })?;
        let probes = probes
            .unwrap_or_default()
            .iter()
            .map(py_to_value)
            .collect::<PyResult<Vec<_>>>()?;
        let db = &self.db;
        py.detach(|| {
            db.create_residual_lens_with_probes(name, forward, rex, inverse, rt, &probes)
        })
        .map_err(map_err)
    }

    /// Every registered lens pair, as dicts.
    fn lenses(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let db = &self.db;
        let infos = py.detach(|| db.list_lenses()).map_err(map_err)?;
        infos
            .into_iter()
            .map(|l| {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("name", l.name)?;
                d.set_item("class", l.class.as_str())?;
                d.set_item("forward_hash", l.forward_hash)?;
                d.set_item("inverse_hash", l.inverse_hash)?;
                d.set_item("rex_hash", l.rex_hash)?;
                d.set_item("residual_type", l.residual_type)?;
                d.set_item("samples", l.samples)?;
                d.set_item("healthy", l.healthy)?;
                Ok(d.into_any().unbind())
            })
            .collect()
    }

    // ------------------------------------------------------------- rRETL
    // Bodies live in `pyrretl.rs` (the house's 2000-line rule); pyo3
    // allows one `#[pymethods]` block per type, so these delegate.

    fn rretl_apply(&self, py: Python<'_>, pair: &str, table: &str, column: &str) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("rretl_apply", "Commit or roll back first.")?;
        pyrretl::rretl_apply(&self.db, py, pair, table, column)
    }
    fn rretl_revert(&self, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("rretl_revert", "Commit or roll back first.")?;
        pyrretl::rretl_revert(&self.db, py, run_id)
    }
    fn rretl_putback(&self, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("rretl_putback", "Commit or roll back first.")?;
        pyrretl::rretl_putback(&self.db, py, run_id)
    }
    fn rretl_fsck(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        pyrretl::rretl_fsck(&self.db, py)
    }
    fn rretl_put_version(&self, py: Python<'_>, obj: &str, data: &[u8]) -> PyResult<i64> {
        self.refuse_if_txn_open("rretl_put_version", "Commit or roll back first.")?;
        pyrretl::rretl_put_version(&self.db, py, obj, data)
    }
    fn rretl_get_version(&self, py: Python<'_>, obj: &str, ver: i64) -> PyResult<Py<PyAny>> {
        pyrretl::rretl_get_version(&self.db, py, obj, ver)
    }
    fn rretl_versions(&self, py: Python<'_>, obj: &str) -> PyResult<Vec<Py<PyAny>>> {
        pyrretl::rretl_versions(&self.db, py, obj)
    }
    fn rretl_prune_versions(&self, py: Python<'_>, obj: &str, keep: u64) -> PyResult<u64> {
        self.refuse_if_txn_open("rretl_prune_versions", "Commit or roll back first.")?;
        pyrretl::rretl_prune_versions(&self.db, py, obj, keep)
    }
    fn rretl_pack_in(&self, py: Python<'_>, name: &str, data: &[u8]) -> PyResult<i64> {
        self.refuse_if_txn_open("rretl_pack_in", "Commit or roll back first.")?;
        pyrretl::rretl_pack_in(&self.db, py, name, data)
    }
    fn rretl_pack_out(&self, py: Python<'_>, archive_id: i64) -> PyResult<Py<PyAny>> {
        pyrretl::rretl_pack_out(&self.db, py, archive_id)
    }
    fn rretl_archives(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        pyrretl::rretl_archives(&self.db, py)
    }
    fn rretl_map_define(&self, py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<()> {
        self.refuse_if_txn_open("rretl_map_define", "Commit or roll back first.")?;
        pyrretl::rretl_map_define(&self.db, py, spec)
    }
    fn rretl_map_sync(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("rretl_map_sync", "Commit or roll back first.")?;
        pyrretl::rretl_map_sync(&self.db, py, name)
    }
    fn rretl_map_check(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        pyrretl::rretl_map_check(&self.db, py, name)
    }
    #[pyo3(signature = (name, max_secs=None, max_rows=None, runner=None, lease_secs=None))]
    fn rretl_map_run(
        &self,
        py: Python<'_>,
        name: &str,
        max_secs: Option<u64>,
        max_rows: Option<u64>,
        runner: Option<String>,
        lease_secs: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("rretl_map_run", "Commit or roll back first.")?;
        pyrretl::rretl_map_run(&self.db, py, name, max_secs, max_rows, runner, lease_secs)
    }
    fn rretl_map_set_runner(&self, py: Python<'_>, name: &str, runner: &str) -> PyResult<()> {
        self.refuse_if_txn_open("rretl_map_set_runner", "Commit or roll back first.")?;
        pyrretl::rretl_map_set_runner(&self.db, py, name, runner)
    }
    fn rretl_map_status(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        pyrretl::rretl_map_status(&self.db, py, name)
    }
    fn rretl_maps(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        pyrretl::rretl_maps(&self.db, py)
    }
    fn rretl_map_show(&self, py: Python<'_>, name: &str) -> PyResult<String> {
        pyrretl::rretl_map_show(&self.db, py, name)
    }
    fn rretl_map_drop(&self, py: Python<'_>, name: &str) -> PyResult<bool> {
        self.refuse_if_txn_open("rretl_map_drop", "Commit or roll back first.")?;
        pyrretl::rretl_map_drop(&self.db, py, name)
    }
    // ------------------------------------------------------------ ingest
    // Bodies in `pyingest.rs`; see the rRETL note above for why.

    fn ingest_define(&self, py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<()> {
        self.refuse_if_txn_open("ingest_define", "Commit or roll back first.")?;
        pyingest::ingest_define(&self.db, py, spec)
    }
    fn ingest_sources(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        pyingest::ingest_sources(&self.db, py)
    }
    fn ingest_show(&self, py: Python<'_>, name: &str) -> PyResult<String> {
        pyingest::ingest_show(&self.db, py, name)
    }
    fn ingest_drop(&self, py: Python<'_>, name: &str) -> PyResult<bool> {
        self.refuse_if_txn_open("ingest_drop", "Commit or roll back first.")?;
        pyingest::ingest_drop(&self.db, py, name)
    }
    #[pyo3(signature = (source, target, mode="delta"))]
    /// Open a streamed receipt; returns an integer run id. `mode` is
    /// `"dump"` (the whole table — the only receipt that sees deletes) or
    /// `"delta"`.
    fn ingest_begin(&self, py: Python<'_>, source: &str, target: &str, mode: &str) -> PyResult<i64> {
        self.refuse_if_txn_open("ingest_begin", "Commit or roll back first.")?;
        pyingest::ingest_begin(&self.db, py, source, target, mode)
    }
    /// Push one chunk of a streamed receipt.
    #[pyo3(signature = (run_id, rows, columns=None, calls=1, bytes=0))]
    fn ingest_rows(
        &self,
        py: Python<'_>,
        run_id: i64,
        rows: &Bound<'_, PyAny>,
        columns: Option<Vec<String>>,
        calls: i64,
        bytes: i64,
    ) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("ingest_rows", "Commit or roll back first.")?;
        pyingest::ingest_rows(&self.db, py, run_id, rows, columns, calls, bytes)
    }
    /// Close a receipt. For a dump, this is where deletes are found.
    fn ingest_finish(&self, py: Python<'_>, run_id: i64) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("ingest_finish", "Commit or roll back first.")?;
        pyingest::ingest_finish(&self.db, py, run_id)
    }
    /// Give up on an open receipt — a fetch that failed halfway. The rows
    /// already pushed stay; the delete sweep does NOT run. Finishing a
    /// half-fed dump instead would read everything you never reached as
    /// deleted.
    fn ingest_abandon(&self, py: Python<'_>, run_id: i64) -> PyResult<()> {
        self.refuse_if_txn_open("ingest_abandon", "Commit or roll back first.")?;
        pyingest::ingest_abandon(&self.db, py, run_id)
    }
    /// A whole small dump in one call: finds inserts, updates AND deletes.
    /// `rows` are dicts keyed by column name, or lists plus `columns=[...]`.
    /// `calls`/`bytes` are what the fetch actually cost you.
    #[pyo3(signature = (source, target, rows, columns=None, calls=1, bytes=0))]
    #[allow(clippy::too_many_arguments)]
    fn ingest_dump(
        &self,
        py: Python<'_>,
        source: &str,
        target: &str,
        rows: &Bound<'_, PyAny>,
        columns: Option<Vec<String>>,
        calls: i64,
        bytes: i64,
    ) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("ingest_dump", "Commit or roll back first.")?;
        pyingest::ingest_dump(&self.db, py, source, target, rows, columns, calls, bytes)
    }
    /// A whole small delta in one call. Cannot see deletes, by definition.
    #[pyo3(signature = (source, target, rows, columns=None, calls=1, bytes=0))]
    #[allow(clippy::too_many_arguments)]
    fn ingest_delta(
        &self,
        py: Python<'_>,
        source: &str,
        target: &str,
        rows: &Bound<'_, PyAny>,
        columns: Option<Vec<String>>,
        calls: i64,
        bytes: i64,
    ) -> PyResult<Py<PyAny>> {
        self.refuse_if_txn_open("ingest_delta", "Commit or roll back first.")?;
        pyingest::ingest_delta(&self.db, py, source, target, rows, columns, calls, bytes)
    }
    /// The observed model per edge: watermark, cursor verdict, caught and
    /// missed, change rate, fan-out, and when it last reported.
    fn ingest_state(&self, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
        pyingest::ingest_state(&self.db, py, source)
    }
    #[pyo3(signature = (source, cmd="./fetch.py"))]
    fn ingest_advise(&self, py: Python<'_>, source: &str, cmd: &str) -> PyResult<Py<PyAny>> {
        pyingest::ingest_advise(&self.db, py, source, cmd)
    }
    fn ingest_conflicts(&self, py: Python<'_>, source: &str) -> PyResult<Vec<Py<PyAny>>> {
        pyingest::ingest_conflicts(&self.db, py, source)
    }
    #[pyo3(signature = (source, take="local"))]
    fn ingest_resolve(&self, py: Python<'_>, source: &str, take: &str) -> PyResult<u64> {
        self.refuse_if_txn_open("ingest_resolve", "Commit or roll back first.")?;
        pyingest::ingest_resolve(&self.db, py, source, take)
    }

    /// Queue derived calls from this receipt's keys, in the SAME
    /// transaction as the rows that produced them.
    /// The trigger-fed journal's backlog per mapped table (rRETL §15).
    fn rretl_map_backlog(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        pyrretl::rretl_map_backlog(&self.db, py, name)
    }
    fn ingest_derive(
        &self,
        py: Python<'_>,
        run_id: i64,
        edge: &str,
        keys: &Bound<'_, PyAny>,
    ) -> PyResult<u64> {
        self.refuse_if_txn_open("ingest_derive", "Commit or roll back first.")?;
        pyingest::ingest_derive(&self.db, py, run_id, edge, keys)
    }
    /// The next batch of derived calls this window's budget allows, or None
    /// — which means EITHER the budget is spent or the queue is empty; ask
    /// `ingest_pending` to tell those apart.
    fn ingest_next(&self, py: Python<'_>, source: &str) -> PyResult<Option<Py<PyAny>>> {
        self.refuse_if_txn_open("ingest_next", "Commit or roll back first.")?;
        pyingest::ingest_next(&self.db, py, source)
    }
    fn ingest_done(&self, py: Python<'_>, source: &str, lease: i64) -> PyResult<u64> {
        self.refuse_if_txn_open("ingest_done", "Commit or roll back first.")?;
        pyingest::ingest_done(&self.db, py, source, lease)
    }
    fn ingest_release(&self, py: Python<'_>, source: &str, lease: i64) -> PyResult<u64> {
        self.refuse_if_txn_open("ingest_release", "Commit or roll back first.")?;
        pyingest::ingest_release(&self.db, py, source, lease)
    }
    #[pyo3(signature = (source, older_than_secs=900))]
    fn ingest_reap(&self, py: Python<'_>, source: &str, older_than_secs: i64) -> PyResult<u64> {
        self.refuse_if_txn_open("ingest_reap", "Commit or roll back first.")?;
        pyingest::ingest_reap(&self.db, py, source, older_than_secs)
    }
    fn ingest_pending(&self, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
        pyingest::ingest_pending(&self.db, py, source)
    }
    fn ingest_budget_left(&self, py: Python<'_>, source: &str) -> PyResult<Py<PyAny>> {
        pyingest::ingest_budget_left(&self.db, py, source)
    }

    /// Every rRETL run, oldest first, failed runs included, as dicts.
    fn rretl_log(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let db = &self.db;
        let log = py.detach(|| db.rretl_log()).map_err(map_err)?;
        log.into_iter()
            .map(|l| {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("run_id", l.run_id)?;
                d.set_item("lens", l.lens)?;
                d.set_item("table", l.table)?;
                d.set_item("column", l.column)?;
                d.set_item("rows", l.rows)?;
                d.set_item("outcome", l.outcome)?;
                d.set_item("error", l.error)?;
                Ok(d.into_any().unbind())
            })
            .collect()
    }
}


pub(crate) fn rretl_report_to_py(py: Python<'_>, r: mpedb::rretl::RretlReport) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    d.set_item("run_id", r.run_id)?;
    d.set_item("rows", r.rows)?;
    d.set_item("residuals", r.residuals)?;
    Ok(d.into_any().unbind())
}

// --------------------------------------------------------------- Transaction

/// An interactive multi-statement write transaction (`Database.begin()`).
/// SELECTs inside the transaction see its own uncommitted writes.
#[pyclass(frozen, name = "Transaction", module = "mpedb")]
struct PyTransaction {
    /// None once committed / rolled back. Field order matters: `session`
    /// must drop before `_db` (see the transmute in `begin`). The [`TxnGuard`]
    /// rides in the same `Option` so that ending the transaction, by ANY route,
    /// also deregisters it (#161).
    session: Mutex<Option<(WriteSession<'static>, TxnGuard)>>,
    /// The thread that called `begin` (#161). The writer lock has thread
    /// affinity, so using this from another thread is undefined at the OS
    /// level, not merely discouraged.
    owner: std::thread::ThreadId,
    _db: Arc<Db>,
}

impl PyTransaction {
    /// The writer lock is a mutex with THREAD affinity — on Linux a robust
    /// `PROCESS_SHARED` pthread mutex, on macOS/Windows an errorcheck mutex
    /// behind the FLD-2 sidecar lock. Unlocking one from a thread that did not
    /// lock it is undefined behaviour in POSIX, not a policy this API invented,
    /// so it is refused rather than documented (#161).
    fn check_owner(&self) -> PyResult<()> {
        if std::thread::current().id() != self.owner {
            return Err(ProgrammingError::new_err(
                "this Transaction belongs to the thread that created it — the \
                 writer lock has thread affinity, and releasing it from another \
                 thread is undefined at the OS level. Open a Transaction on the \
                 thread that will use it, or hand the WORK to the owning thread.",
            ));
        }
        Ok(())
    }

    fn with_session<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut WriteSession<'static>) -> Result<R, DbError> + Send,
    ) -> PyResult<R>
    where
        R: Send,
    {
        self.check_owner()?;
        // The mutex is only ever taken with the GIL released; taking it while
        // holding the GIL could deadlock against a thread that holds the
        // mutex and is waiting to re-acquire the GIL.
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let (session, _) = guard.as_mut().ok_or_else(closed_err)?;
            f(session).map_err(map_err)
        })
    }
}

#[pymethods]
impl PyTransaction {
    /// Execute a prepared plan inside this transaction.
    #[pyo3(signature = (plan_hash, params=None))]
    fn execute(
        &self,
        py: Python<'_>,
        plan_hash: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let hash = parse_hash(plan_hash)?;
        let vals = convert_params(params)?;
        let res = self.with_session(py, move |s| {
            run_coercing(vals, |p| s.execute(&hash, p))
        })?;
        exec_result_to_py(py, res)
    }

    /// Compile and run SQL inside this transaction (plan cached only in this
    /// process, never published to the shared registry).
    #[pyo3(signature = (sql, params=None))]
    fn query(
        &self,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let vals = convert_params(params)?;
        let res = self.with_session(py, move |s| run_coercing(vals, |p| s.query(sql, p)))?;
        exec_result_to_py(py, res)
    }

    /// INSERT one row into `table`, streaming column index `stream_col`
    /// straight from the file at `path`, a page at a time — the file is never
    /// resident, so it may be far larger than RAM. `values` is the full row in
    /// column order; `values[stream_col]` is a placeholder for the type check
    /// (pass `b""`), the length comes from the file.
    ///
    /// Path-based on purpose (no read()-callback variant): the engine PULLS
    /// pages while holding the single writer lock, and re-entering Python per
    /// page under that lock is the documented footgun. A path keeps Python out
    /// of the loop entirely.
    ///
    /// Engine constraints: the streamed column must be the table's LAST
    /// variable-length column, and a table with a secondary UNIQUE index is
    /// refused (the uniqueness probe needs the value nobody has yet).
    #[pyo3(signature = (table, values, stream_col, path))]
    fn insert_file(
        &self,
        py: Python<'_>,
        table: &str,
        values: &Bound<'_, PyAny>,
        stream_col: usize,
        path: PathBuf,
    ) -> PyResult<()> {
        let vals = convert_params(Some(values))?;
        self.with_session(py, move |s| s.insert_file(table, &vals, stream_col, &path))
    }

    /// Commit everything written through this transaction. A poisoned
    /// session refuses (OperationalError) and rolls back instead.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.check_owner()?;
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let (session, _reg) = guard.take().ok_or_else(closed_err)?;
            session.commit().map_err(map_err)
        })
    }

    /// Discard everything written through this transaction.
    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        self.check_owner()?;
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let (session, _reg) = guard.take().ok_or_else(closed_err)?;
            session.rollback();
            Ok(())
        })
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Commit on clean exit, roll back if an exception is propagating.
    /// Never suppresses the exception. No-op if already closed manually.
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc_value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let _ = (exc_value, traceback);
        // NOT owner-checked: `__exit__` runs on whatever thread is unwinding,
        // and refusing there would turn a bug into an un-exitable `with` block
        // that leaks the writer lock. The session's own `commit`/`rollback`
        // still go through the engine, which errors rather than corrupting.
        let clean = exc_type.is_none();
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            match guard.take() {
                None => Ok(false), // closed inside the `with` body: fine
                Some((session, _reg)) => {
                    if clean {
                        session.commit().map_err(map_err)?;
                    } else {
                        session.rollback();
                    }
                    Ok(false)
                }
            }
        })
    }
}

// ------------------------------------------------------------------- Session

/// A caching client session (`Database.session()`). Compiles each distinct SQL
/// string exactly once into a client-side detached plan, caches it locally,
/// and executes by hash thereafter. On a schema change it transparently
/// re-prepares from the cached SQL and retries once. Thread-safe: the cache is
/// behind a mutex and every engine call runs with the GIL released.
#[pyclass(frozen, name = "Session", module = "mpedb")]
struct PySession {
    db: Arc<Db>,
    plans: Mutex<HashMap<String, Arc<DetachedPlan>>>,
}

#[pymethods]
impl PySession {
    /// Run `sql` with optional `params`. First use of a given SQL string
    /// compiles + caches it; later uses skip parsing entirely. SELECT ->
    /// list[tuple]; DML -> int.
    #[pyo3(signature = (sql, params=None))]
    fn run(
        &self,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let vals = convert_params(params)?;
        let cached = self
            .plans
            .lock()
            .expect("session cache poisoned")
            .get(sql)
            .cloned();
        let db = self.db.clone();
        let sqls = sql.to_owned();
        let (res, plan) = py
            .detach(move || -> Result<(ExecResult, Arc<DetachedPlan>), DbError> {
                let plan = match cached {
                    Some(p) => p,
                    None => Arc::new(db.prepare_detached(&sqls)?),
                };
                let retry_vals = vals.clone();
                match run_coercing(vals, |p| db.execute_detached(&plan, p)) {
                    Ok(r) => Ok((r, plan)),
                    // Schema changed under us: re-prepare from the cached SQL
                    // and retry once (the fresh plan uses the current schema).
                    Err(DbError::PlanInvalidated) => {
                        let fresh = Arc::new(db.prepare_detached(&sqls)?);
                        let r = run_coercing(retry_vals, |p| db.execute_detached(&fresh, p))?;
                        Ok((r, fresh))
                    }
                    Err(e) => Err(e),
                }
            })
            .map_err(map_err)?;
        self.plans
            .lock()
            .expect("session cache poisoned")
            .insert(sql.to_owned(), plan);
        exec_result_to_py(py, res)
    }

    /// Number of distinct SQL statements currently cached (compiled once each).
    fn cached_plans(&self) -> usize {
        self.plans.lock().expect("session cache poisoned").len()
    }
}

// -------------------------------------------------------------------- module

/// Compile-time proof that the pyclasses are fully thread-safe (required
/// for sharing across Python threads and for `allow_threads` closures).
#[allow(dead_code)]
fn assert_thread_safe() {
    fn ok<T: Send + Sync>() {}
    ok::<PyDatabase>();
    ok::<PyTransaction>();
    ok::<PySession>();
}

#[pymodule(name = "_native", gil_used = false)]
fn mpedb_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add_class::<PyTransaction>()?;
    m.add_class::<PySession>()?;
    // DB-API 2.0 (PEP 249).
    m.add_class::<crate::pydbapi::PyConnection>()?;
    m.add_class::<crate::pydbapi::PyCursor>()?;
    m.add_class::<crate::pydbapi::PyRow>()?;
    m.add_function(wrap_pyfunction!(crate::pydbapi::connect, m)?)?;
    m.add("apilevel", "2.0")?;
    // 1 = "threads may share the module, but not connections". A Connection
    // holds a buffered transaction and is not synchronized; `Database` itself
    // is Send+Sync and safe to share.
    m.add("threadsafety", 1)?;
    m.add("paramstyle", "qmark")?;
    m.add("Error", m.py().get_type::<Error>())?;
    m.add("IntegrityError", m.py().get_type::<IntegrityError>())?;
    m.add("ProgrammingError", m.py().get_type::<ProgrammingError>())?;
    m.add("OperationalError", m.py().get_type::<OperationalError>())?;
    Ok(())
}
