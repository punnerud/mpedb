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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
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

fn closed_err() -> PyErr {
    ProgrammingError::new_err("transaction is already closed (committed or rolled back)")
}

// ---------------------------------------------------------- value conversion

/// Python -> Value. Checked in order; `bool` MUST precede `int` because
/// Python's bool is a subclass of int.
fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
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
    // Aware datetime (any fixed offset) -> UTC microseconds.
    if let Ok(dt) = obj.extract::<DateTime<FixedOffset>>() {
        return Ok(Value::Timestamp(dt.with_timezone(&Utc).timestamp_micros()));
    }
    // Naive datetime: treated as UTC.
    if let Ok(dt) = obj.extract::<NaiveDateTime>() {
        return Ok(Value::Timestamp(dt.and_utc().timestamp_micros()));
    }
    Err(PyTypeError::new_err(format!(
        "cannot bind {} as an mpedb parameter \
         (expected None, bool, int, float, str, bytes/bytearray, or datetime)",
        obj.get_type()
    )))
}

/// Value -> Python. Timestamps come back as timezone-aware
/// `datetime.datetime` in UTC.
fn value_to_py<'py>(py: Python<'py>, v: Value) -> PyResult<Bound<'py, PyAny>> {
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
    }
}

fn rows_to_py<'py>(py: Python<'py>, rows: Vec<Vec<Value>>) -> PyResult<Bound<'py, PyList>> {
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
}

#[pymethods]
impl PyDatabase {
    #[new]
    fn new(py: Python<'_>, config_path: PathBuf) -> PyResult<Self> {
        let db = py
            .detach(move || Db::open(&config_path))
            .map_err(map_err)?;
        Ok(PyDatabase { db: Arc::new(db) })
    }

    /// Compile SQL to a content-hashed plan, publish it in the shared
    /// registry, and return the 64-hex plan hash.
    fn prepare(&self, py: Python<'_>, sql: &str) -> PyResult<String> {
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
    fn tables(&self) -> Vec<String> {
        self.db
            .schema()
            .tables
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    /// Verify the engine's page-accounting invariant; raises on failure.
    /// Takes the writer lock briefly — never call with an open Transaction
    /// on this thread.
    fn verify(&self, py: Python<'_>) -> PyResult<()> {
        let db = &self.db;
        py.detach(move || db.verify()).map_err(map_err)
    }

    /// Start an interactive write transaction (holds the single writer lock).
    /// Use as a context manager: commits on clean exit, rolls back on
    /// exception. A statement that fails after partially applying its effects
    /// poisons the session: further calls and commit raise OperationalError;
    /// only rollback (or `with`-exit via exception) is allowed.
    fn begin(&self, py: Python<'_>) -> PyResult<PyTransaction> {
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
        Ok(PyTransaction {
            session: Mutex::new(Some(session)),
            _db: db,
        })
    }
}

// --------------------------------------------------------------- Transaction

/// An interactive multi-statement write transaction (`Database.begin()`).
/// SELECTs inside the transaction see its own uncommitted writes.
#[pyclass(frozen, name = "Transaction", module = "mpedb")]
struct PyTransaction {
    /// None once committed / rolled back. Field order matters: `session`
    /// must drop before `_db` (see the transmute in `begin`).
    session: Mutex<Option<WriteSession<'static>>>,
    _db: Arc<Db>,
}

impl PyTransaction {
    fn with_session<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut WriteSession<'static>) -> Result<R, DbError> + Send,
    ) -> PyResult<R>
    where
        R: Send,
    {
        // The mutex is only ever taken with the GIL released; taking it while
        // holding the GIL could deadlock against a thread that holds the
        // mutex and is waiting to re-acquire the GIL.
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let session = guard.as_mut().ok_or_else(closed_err)?;
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

    /// Commit everything written through this transaction. A poisoned
    /// session refuses (OperationalError) and rolls back instead.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let session = guard.take().ok_or_else(closed_err)?;
            session.commit().map_err(map_err)
        })
    }

    /// Discard everything written through this transaction.
    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            let session = guard.take().ok_or_else(closed_err)?;
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
        let clean = exc_type.is_none();
        py.detach(|| {
            let mut guard = self.session.lock().expect("transaction mutex poisoned");
            match guard.take() {
                None => Ok(false), // closed inside the `with` body: fine
                Some(session) => {
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

#[pymodule(name = "mpedb", gil_used = false)]
fn mpedb_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add_class::<PyTransaction>()?;
    m.add_class::<PySession>()?;
    m.add("Error", m.py().get_type::<Error>())?;
    m.add("IntegrityError", m.py().get_type::<IntegrityError>())?;
    m.add("ProgrammingError", m.py().get_type::<ProgrammingError>())?;
    m.add("OperationalError", m.py().get_type::<OperationalError>())?;
    Ok(())
}
