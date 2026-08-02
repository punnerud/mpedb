//! DB-API 2.0 (PEP 249) — the `sqlite3`-shaped surface. Split out of
//! `lib.rs` to keep that file under the 2000-line ceiling; the
//! engine-shaped API (`Database`, `Transaction`, `Session`) stays there.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mpedb::{Database as Db, Error as DbError, ExecResult, Value};
use pyo3::prelude::*;
use pyo3::types::PyList;

use crate::{closed_err, convert_params_sqlite3, map_err, rows_to_py, run_coercing};

// ---------------------------------------------------------------- DB-API 2.0
//
// PEP 249, so `sqlite3`-shaped code runs unchanged: connect / cursor /
// execute / fetchall, `?` placeholders, `description`, `rowcount`.
//
// What it does NOT pretend: mpedb's SQL is a real subset, so an unsupported
// statement fails here, loudly (ProgrammingError), rather than silently doing
// something else. Live DDL (`CREATE`/`DROP TABLE`, `ALTER … RENAME`/`ADD COLUMN`)
// does pass through `query`; the rest of a schema change is a config change or a
// `mirror import`.

/// A DB-API 2.0 connection: a [`PyDatabase`] plus the transaction state PEP
/// 249 requires (it has no autocommit — a connection is always in one).
/// Which engine answers this connection — the `pip install mpedb` routing
/// surface. `Native` is mpedb proper (`.mpedb` / `:memory:` / explicit
/// `mpedb.mpedb`); `Overlay` is the sqlite-backed mode (#69): a `.db` path is
/// READ as sqlite and every write lands in an mpedb delta pushed back into the
/// base by `checkpoint()` — `commit()` here — so the `.db` stays in sync.
enum Backend {
    Native(Arc<Db>),
    Overlay(Arc<Mutex<Option<mpedb::SqliteOverlay>>>, PathBuf),
}

impl Backend {
    /// `PRAGMA page_count`, answered honestly: the real backing bytes on
    /// disk (base + overlay sidecar for a `.db`), in 4096-byte pages —
    /// diskcache's `volume()` destructures this and steers its culling by it.
    /// `:memory:` (no file) answers 1.
    fn page_count(&self) -> i64 {
        let len = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let bytes = match self {
            Backend::Native(db) => len(db.path()),
            Backend::Overlay(_, base) => {
                let mut side = base.as_os_str().to_owned();
                side.push(".overlay.mpedb");
                len(base) + len(std::path::Path::new(&side))
            }
        };
        bytes.div_ceil(4096).max(1) as i64
    }
}

#[pyclass(name = "Connection", module = "mpedb", subclass)]
pub(crate) struct PyConnection {
    backend: Backend,
    /// sqlite3's `text_factory` — accepted and stored (str is the one
    /// behavior mpedb produces; sqlitedict SETS it at bootstrap and never
    /// needs another value).
    text_factory: Option<Py<PyAny>>,
    /// The open transaction. This used to BUFFER statements and replay them
    /// at commit — which meant a `SELECT` could not see this connection's own
    /// uncommitted writes, and the first real Django consumer died on exactly
    /// that (`TestCase` wraps every test in a transaction it rolls back, so
    /// virtually every ORM read depends on read-your-own-writes). It is now a
    /// REAL open `WriteSession`, sqlite's own model: the single writer lock is
    /// held from the first write until commit/rollback, reads route through
    /// the session and see everything it wrote, and errors surface at
    /// `execute()` where the caller is looking. The lock-holding cost is
    /// sqlite's too — a connection that sits in a transaction blocks other
    /// writers there exactly as here.
    txn: Option<Session>,
    /// sqlite3's `isolation_level`: `Some(level)` (default `""`) = the module
    /// opens the transaction implicitly before the first DML; `None` =
    /// autocommit — every statement commits on its own unless the caller
    /// issues an explicit `BEGIN` (Django's mode: it sets `None` and drives
    /// BEGIN/COMMIT itself).
    isolation_level: Option<String>,
    /// sqlite3's `row_factory` — `None` means plain tuples; a callable is
    /// invoked as `factory(cursor, row_tuple)` per row (`mpedb.Row` included).
    row_factory: Option<Py<PyAny>>,
    closed: bool,
}

/// The open write session backing a `Connection` while a transaction runs.
///
/// Same `'static` discipline as [`crate::PyTransaction`]: the session borrows
/// the `Arc<Db>` inside [`Backend::Native`], which lives at a stable heap
/// address for as long as the connection does, and `txn` is dropped before
/// `backend` (field order). Thread affinity is real (#161 — the writer lock
/// is an OS mutex with owner semantics), which is also sqlite3's own
/// `check_same_thread` rule: the session is used only from the thread that
/// opened it, enforced at every touch.
struct Session {
    /// Mutex so the GIL-released `py.detach` closure can borrow it (`&Mutex`
    /// is `Ungil`); it is never contended — `check_owner` pins use to one
    /// thread.
    w: Mutex<mpedb::WriteSession<'static>>,
    owner: std::thread::ThreadId,
}

impl Session {
    fn check_owner(&self) -> PyResult<()> {
        if std::thread::current().id() != self.owner {
            return Err(crate::ProgrammingError::new_err(
                "this connection's transaction belongs to the thread that started \
                 it — the writer lock has thread affinity (and sqlite3's \
                 check_same_thread rule applies). Use the connection from one \
                 thread, or commit before handing it over.",
            ));
        }
        Ok(())
    }
}

/// Open a real write session against `db`, lifetime-erased per the
/// [`crate::PyTransaction`] pattern (the `Arc<Db>` box outlives the session by
/// field order in [`PyConnection`]).
fn open_session(py: Python<'_>, db: &Arc<Db>) -> PyResult<Session> {
    let db = db.clone();
    let w = py
        .detach(move || -> Result<mpedb::WriteSession<'static>, DbError> {
            let session = db.begin()?;
            // SAFETY: the session borrows the Database inside the Arc, whose
            // referent has a stable heap address; the CONNECTION holds its own
            // Arc clone in `Backend::Native`, declared after `txn`, so the
            // borrow is dropped before its referent can be freed.
            Ok(unsafe {
                std::mem::transmute::<mpedb::WriteSession<'_>, mpedb::WriteSession<'static>>(session)
            })
        })
        .map_err(map_err)?;
    Ok(Session { w: Mutex::new(w), owner: std::thread::current().id() })
}

#[pymethods]
impl PyConnection {
    #[getter]
    fn get_text_factory(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.text_factory {
            Some(f) => f.clone_ref(py),
            None => py.get_type::<pyo3::types::PyString>().into_any().unbind(),
        }
    }

    #[setter]
    fn set_text_factory(&mut self, f: Py<PyAny>) {
        self.text_factory = Some(f);
    }

    /// PEP 249 `Connection.cursor()`, with sqlite3's `factory` parameter: a
    /// callable (typically a `Cursor` subclass — Django's
    /// `SQLiteCursorWrapper`) invoked as `factory(connection)`.
    #[pyo3(signature = (factory=None))]
    fn cursor(
        slf: Py<Self>,
        py: Python<'_>,
        factory: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        match factory {
            Some(f) => Ok(f.call1((slf,))?.unbind()),
            None => Ok(Py::new(py, PyCursor::fresh(slf))?.into_any()),
        }
    }

    /// sqlite3's `isolation_level`: `""`/`"DEFERRED"`/`"IMMEDIATE"`/
    /// `"EXCLUSIVE"` = implicit transactions, `None` = autocommit. Assigning
    /// commits a pending transaction first, as the stdlib does.
    #[getter]
    fn get_isolation_level(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.isolation_level {
            Some(l) => pyo3::types::PyString::new(py, l).into_any().unbind(),
            None => py.None(),
        }
    }

    #[setter]
    fn set_isolation_level(
        &mut self,
        py: Python<'_>,
        level: Option<String>,
    ) -> PyResult<()> {
        if self.txn.is_some() {
            self.commit(py)?;
        }
        match level.as_deref() {
            None | Some("") => {}
            Some(l)
                if ["deferred", "immediate", "exclusive"]
                    .contains(&l.to_ascii_lowercase().as_str()) => {}
            Some(other) => {
                return Err(crate::ProgrammingError::new_err(format!(
                    "isolation_level string must be '', 'DEFERRED', 'IMMEDIATE', \
                     or 'EXCLUSIVE' — got {other:?}"
                )))
            }
        }
        self.isolation_level = level;
        Ok(())
    }

    /// sqlite3's `in_transaction`: is a transaction open on this connection?
    #[getter]
    fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    #[getter]
    fn get_row_factory(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.row_factory {
            Some(f) => f.clone_ref(py),
            None => py.None(),
        }
    }

    #[setter]
    fn set_row_factory(&mut self, py: Python<'_>, f: Option<Py<PyAny>>) -> PyResult<()> {
        check_row_factory(py, &f)?;
        self.row_factory = f;
        Ok(())
    }

    /// PEP 249 `Connection.commit()`.
    fn commit(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.closed {
            return Err(closed_err());
        }
        match &self.backend {
            Backend::Native(_) => {
                let Some(session) = self.txn.take() else {
                    return Ok(()); // nothing open; a no-op, as in sqlite3
                };
                session.check_owner()?;
                py.detach(move || session.w.into_inner().expect("session poisoned").commit())
                    .map_err(map_err)
            }
            // Overlay writes are per-statement (autocommit into the delta);
            // `commit()` is the SYNC point: checkpoint pushes the delta into
            // the `.db` base, which is what keeps sqlite and mpedb one store.
            Backend::Overlay(ov, _) => {
                let ov = ov.clone();
                py.detach(move || -> Result<(), DbError> {
                    let mut g = ov.lock().expect("overlay poisoned");
                    g.as_mut()
                        .ok_or_else(|| DbError::Internal("overlay gone".into()))?
                        .checkpoint()
                        .map(|_| ())
                })
                .map_err(map_err)
            }
        }
    }

    /// PEP 249 `Connection.rollback()` — undo the open transaction.
    fn rollback(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.closed {
            return Err(closed_err());
        }
        if let Some(session) = self.txn.take() {
            session.check_owner()?;
            py.detach(move || session.w.into_inner().expect("session poisoned").rollback());
        }
        Ok(())
    }

    /// PEP 249 `Connection.close()`. Uncommitted work is discarded, which PEP
    /// 249 requires ("an implicit rollback is performed").
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        if let Some(session) = self.txn.take() {
            session.check_owner()?;
            py.detach(move || session.w.into_inner().expect("session poisoned").rollback());
        }
        self.closed = true;
        Ok(())
    }

    /// A convenience sqlite3 also has: `conn.execute(...)` makes a cursor.
    #[pyo3(signature = (sql, params=None))]
    fn execute(
        slf: Py<Self>,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyCursor> {
        let mut cur = PyCursor::fresh(slf);
        cur.execute(py, sql, params)?;
        Ok(cur)
    }

    /// sqlite3's `Connection.executemany(...)` convenience.
    fn executemany(
        slf: Py<Self>,
        py: Python<'_>,
        sql: &str,
        seq: &Bound<'_, PyAny>,
    ) -> PyResult<PyCursor> {
        let mut cur = PyCursor::fresh(slf);
        cur.executemany(py, sql, seq)?;
        Ok(cur)
    }

    /// sqlite3's `Connection.executescript(...)` convenience.
    fn executescript(slf: Py<Self>, py: Python<'_>, script: &str) -> PyResult<PyCursor> {
        let mut cur = PyCursor::fresh(slf);
        cur.executescript(py, script)?;
        Ok(cur)
    }

    /// `sqlite3.Connection.create_function(name, narg, func, *, deterministic)`.
    /// Django registers ~40 of these on EVERY new connection. The callable is
    /// invoked under the GIL per row; `deterministic` is accepted (mpedb's
    /// host UDFs are re-evaluated per row either way, so the flag changes
    /// nothing it would change in sqlite's planner).
    #[pyo3(signature = (name, narg, func, *, deterministic=false))]
    fn create_function(
        &self,
        name: &str,
        narg: i32,
        func: Option<Py<PyAny>>,
        deterministic: bool,
    ) -> PyResult<()> {
        let _ = deterministic;
        let Backend::Native(db) = &self.backend else {
            return Err(crate::ProgrammingError::new_err(
                "create_function on a .db overlay connection is not supported yet \
                 — reads run through sqlite's own engine there",
            ));
        };
        match func {
            None => {
                db.unregister_host_function(name, narg);
            }
            Some(f) => {
                db.register_host_function(name, narg, move |args: &[Value]| {
                    Python::attach(|py| {
                        let items: Vec<_> = args
                            .iter()
                            .map(|v| crate::value_to_py(py, v.clone()))
                            .collect::<PyResult<_>>()
                            .map_err(|e| DbError::Unsupported(format!("udf args: {e}")))?;
                        let tuple = pyo3::types::PyTuple::new(py, items)
                            .map_err(|e| DbError::Unsupported(format!("udf args: {e}")))?;
                        let out = f
                            .bind(py)
                            .call1(tuple)
                            .map_err(|e| DbError::Unsupported(format!("user-defined function raised: {e}")))?;
                        crate::py_to_value(&out)
                            .map_err(|e| DbError::Unsupported(format!("user-defined function returned an unbindable value: {e}")))
                    })
                });
            }
        }
        Ok(())
    }

    /// `sqlite3.Connection.create_aggregate(name, narg, aggregate_class)` —
    /// the class protocol: `cls()` per group, `.step(*args)` per row,
    /// `.finalize()` at the end.
    fn create_aggregate(
        &self,
        name: &str,
        narg: i32,
        aggregate_class: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let Backend::Native(db) = &self.backend else {
            return Err(crate::ProgrammingError::new_err(
                "create_aggregate on a .db overlay connection is not supported yet",
            ));
        };
        match aggregate_class {
            None => {
                db.unregister_host_aggregate(name, narg);
            }
            Some(cls) => {
                db.register_host_aggregate(name, narg, move || {
                    let inst = Python::attach(|py| cls.bind(py).call0().map(Bound::unbind));
                    Box::new(PyAggState { inst }) as Box<dyn mpedb::HostAggState>
                });
            }
        }
        Ok(())
    }

    /// `sqlite3.Connection.create_collation(name, callable_or_None)` — the
    /// callable returns <0/0/>0 like `strcmp`; `None` removes the collation.
    fn create_collation(&self, name: &str, callable: Option<Py<PyAny>>) -> PyResult<()> {
        let Backend::Native(db) = &self.backend else {
            return Err(crate::ProgrammingError::new_err(
                "create_collation on a .db overlay connection is not supported yet",
            ));
        };
        match callable {
            None => {
                db.unregister_host_collation(name);
            }
            Some(f) => {
                db.register_host_collation(name, move |a: &str, b: &str| {
                    Python::attach(|py| {
                        match f.bind(py).call1((a, b)).and_then(|r| r.extract::<i64>()) {
                            Ok(n) => n.cmp(&0),
                            // sqlite's rule for a broken comparator is
                            // undefined order, never a crash; BINARY order is
                            // the honest deterministic stand-in.
                            Err(_) => a.cmp(b),
                        }
                    })
                });
            }
        }
        Ok(())
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// sqlite3's semantics, which PEP 249 leaves open: commit on a clean exit,
    /// roll back on an exception.
    fn __exit__(
        &mut self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        _v: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if exc_type.is_some() {
            self.rollback(py)?;
        } else {
            self.commit(py)?;
        }
        Ok(false) // never swallow the exception
    }
}

/// sqlite3's `Row`: a row that indexes by position AND by column name
/// (case-insensitively), iterates, compares equal to itself, and lists its
/// `keys()`. Assign the CLASS to `row_factory` (`conn.row_factory =
/// mpedb.Row`) exactly as with the stdlib.
#[pyclass(name = "Row", module = "mpedb")]
pub(crate) struct PyRow {
    names: Vec<String>,
    values: Vec<Py<PyAny>>,
}

#[pymethods]
impl PyRow {
    /// The stdlib constructor shape: `Row(cursor, row_tuple)`.
    #[new]
    fn py_new(cursor: PyRef<'_, PyCursor>, row: &Bound<'_, PyAny>) -> PyResult<Self> {
        let py = row.py();
        let names = cursor.column_names(py);
        let values: Vec<Py<PyAny>> = row
            .try_iter()?
            .map(|v| v.map(Bound::unbind))
            .collect::<PyResult<_>>()?;
        Ok(PyRow { names, values })
    }

    fn keys(&self) -> Vec<String> {
        self.names.clone()
    }

    fn __len__(&self) -> usize {
        self.values.len()
    }

    fn __getitem__(&self, py: Python<'_>, key: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        if let Ok(i) = key.extract::<isize>() {
            let n = self.values.len() as isize;
            let i = if i < 0 { i + n } else { i };
            return self
                .values
                .get(usize::try_from(i).map_err(|_| pyo3::exceptions::PyIndexError::new_err("row index out of range"))?)
                .map(|v| v.clone_ref(py))
                .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("row index out of range"));
        }
        let name: String = key.extract()?;
        self.names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(&name))
            .map(|i| self.values[i].clone_ref(py))
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err(format!("no such column: {name}")))
    }

    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let list = PyList::new(py, self.values.iter().map(|v| v.clone_ref(py)))?;
        Ok(list.into_any().call_method0("__iter__")?.unbind())
    }
}

/// A Python aggregate-class instance driven through mpedb's
/// [`HostAggState`](mpedb::HostAggState) protocol — `step` and `finish`
/// re-enter Python under the GIL per call.
struct PyAggState {
    inst: Result<Py<PyAny>, PyErr>,
}

impl mpedb::HostAggState for PyAggState {
    fn step(&mut self, args: &[Value]) -> Result<(), DbError> {
        Python::attach(|py| {
            let inst = self
                .inst
                .as_ref()
                .map_err(|e| DbError::Unsupported(format!("aggregate __init__ raised: {e}")))?;
            let items: Vec<_> = args
                .iter()
                .map(|v| crate::value_to_py(py, v.clone()))
                .collect::<PyResult<_>>()
                .map_err(|e| DbError::Unsupported(format!("aggregate args: {e}")))?;
            let tuple = pyo3::types::PyTuple::new(py, items)
                .map_err(|e| DbError::Unsupported(format!("aggregate args: {e}")))?;
            inst.bind(py)
                .call_method1("step", tuple)
                .map(|_| ())
                .map_err(|e| DbError::Unsupported(format!("aggregate step raised: {e}")))
        })
    }

    fn finish(self: Box<Self>) -> Result<Value, DbError> {
        Python::attach(|py| {
            let inst = self
                .inst
                .as_ref()
                .map_err(|e| DbError::Unsupported(format!("aggregate __init__ raised: {e}")))?;
            let out = inst
                .bind(py)
                .call_method0("finalize")
                .map_err(|e| DbError::Unsupported(format!("aggregate finalize raised: {e}")))?;
            crate::py_to_value(&out)
                .map_err(|e| DbError::Unsupported(format!("aggregate finalize returned an unbindable value: {e}")))
        })
    }
}

/// PEP 249 `Cursor`. `subclass` because Django's sqlite3 backend defines
/// `class SQLiteCursorWrapper(Database.Cursor)` at import — without it the
/// backend cannot even load.
#[pyclass(name = "Cursor", module = "mpedb", subclass)]
pub(crate) struct PyCursor {
    conn: Py<PyConnection>,
    rows: Vec<Py<PyAny>>,
    pos: usize,
    /// PEP 249 `description`: 7-tuples, of which only `name` is meaningful
    /// here — the rest are None, which PEP 249 explicitly allows.
    description: Option<Py<PyAny>>,
    rowcount: i64,
    /// sqlite3's `lastrowid`: the rowid of the last successful INSERT through
    /// THIS cursor (`None` before any).
    lastrowid: Option<i64>,
    /// Per-cursor `row_factory`, seeded from the connection's at creation
    /// (sqlite3's rule) and assignable afterwards.
    row_factory: Option<Py<PyAny>>,
    /// The current result's column names — what `Row` indexes by.
    col_names: Vec<String>,
}

impl PyCursor {
    pub(crate) fn fresh(conn: Py<PyConnection>) -> Self {
        PyCursor {
            conn,
            rows: Vec::new(),
            pos: 0,
            description: None,
            rowcount: -1,
            lastrowid: None,
            row_factory: None,
            col_names: Vec::new(),
        }
    }

    pub(crate) fn column_names(&self, _py: Python<'_>) -> Vec<String> {
        self.col_names.clone()
    }

    /// The row factory in effect: the cursor's own, else the connection's.
    fn effective_row_factory(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        if let Some(f) = &self.row_factory {
            return Some(f.clone_ref(py));
        }
        self.conn.borrow(py).row_factory.as_ref().map(|f| f.clone_ref(py))
    }
}

/// Only `None` and the `mpedb.Row` class are accepted as row factories for
/// now: `Row` is what real consumers set, and a generic
/// `factory(cursor, row)` callable needs the cursor object threaded through
/// the result loader — refusing BY NAME beats silently never calling it.
fn check_row_factory(py: Python<'_>, f: &Option<Py<PyAny>>) -> PyResult<()> {
    if let Some(f) = f {
        let b = f.bind(py);
        if !b.is(py.get_type::<PyRow>()) {
            return Err(crate::ProgrammingError::new_err(
                "only mpedb.Row (or None) is supported as row_factory for now —                  a custom factory callable is a named gap, not a silent no-op",
            ));
        }
    }
    Ok(())
}

impl PyCursor {
    /// `PRAGMA name [= value]` / `PRAGMA name(arg)` — sqlite's contract,
    /// approximated honestly: a SET form is accepted and ignored (mpedb's
    /// journal/synchronous machinery is the config's, not per-connection), a
    /// READ of a pragma we can answer returns ONE row (diskcache destructures
    /// `PRAGMA page_size`), and anything else returns NO rows — which is
    /// exactly what sqlite does for an unknown pragma, and never a lie about
    /// a value we do not have.
    fn exec_pragma(&mut self, py: Python<'_>, rest: &str) -> PyResult<()> {
        let name_part = rest
            .split(['=', '('])
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let is_set = rest.contains('=');
        let answer: Option<i64> = if is_set {
            None
        } else {
            match name_part.as_str() {
                "page_size" => Some(4096),
                // The real backing bytes in 4096-byte pages (the caller drops
                // its connection borrow before exec_pragma — safe to borrow).
                "page_count" => Some(self.conn.borrow(py).backend.page_count()),
                "cache_size" => Some(-2000),
                "synchronous" => Some(2),
                "foreign_keys" => Some(0),
                "mmap_size" => Some(0),
                "user_version" | "application_id" | "auto_vacuum" | "temp_store"
                | "secure_delete" | "count_changes" => Some(0),
                "schema_version" | "freelist_count" => Some(1),
                "busy_timeout" => Some(5000),
                "max_page_count" => Some(1073741823),
                _ => None,
            }
        };
        self.rows.clear();
        self.pos = 0;
        self.rowcount = -1;
        self.description = None;
        if !is_set {
            if let Some(v) = answer {
                self.description = Some(describe(py, std::slice::from_ref(&name_part))?);
                let row = (v,).into_pyobject(py)?.into_any().unbind();
                self.rows = vec![row];
            } else if name_part == "journal_mode" {
                // The one TEXT-valued pragma real code reads (sqlitedict sets
                // then trusts it): answer with the honest equivalent.
                self.description = Some(describe(py, std::slice::from_ref(&name_part))?);
                let row = ("wal",).into_pyobject(py)?.into_any().unbind();
                self.rows = vec![row];
            }
        } else if name_part == "journal_mode" {
            // sqlite returns the RESULTING mode from a journal_mode SET.
            self.description = Some(describe(py, std::slice::from_ref(&name_part))?);
            let row = ("wal",).into_pyobject(py)?.into_any().unbind();
            self.rows = vec![row];
        }
        Ok(())
    }

    /// Load one ExecResult into this cursor's PEP 249 state — shared by the
    /// native read path and the overlay per-statement path.
    fn load_result(&mut self, py: Python<'_>, res: ExecResult) -> PyResult<()> {
        match res {
            ExecResult::Rows { columns, rows } => {
                let list = rows_to_py(py, rows)?;
                self.rowcount = -1; // PEP 249: undefined for SELECT
                self.description = Some(describe(py, &columns)?);
                self.col_names = columns;
                self.rows = list.iter().map(|r| r.unbind()).collect();
                self.pos = 0;
                // `row_factory = mpedb.Row`: wrap each tuple. The setter
                // guarantees the factory IS the Row class, so this constructs
                // directly rather than re-entering Python.
                if self.effective_row_factory(py).is_some() {
                    let names = self.col_names.clone();
                    self.rows = self
                        .rows
                        .iter()
                        .map(|r| -> PyResult<Py<PyAny>> {
                            let values: Vec<Py<PyAny>> = r
                                .bind(py)
                                .try_iter()?
                                .map(|v| v.map(Bound::unbind))
                                .collect::<PyResult<_>>()?;
                            Ok(Py::new(py, PyRow { names: names.clone(), values })?.into_any())
                        })
                        .collect::<PyResult<_>>()?;
                }
            }
            ExecResult::Affected(n) => {
                self.rowcount = n as i64;
                self.rows.clear();
                self.pos = 0;
                self.description = None;
            }
            ExecResult::Explain(text) => {
                self.description = Some(describe(py, &["plan".to_string()])?);
                let row = (text,).into_pyobject(py)?.into_any().unbind();
                self.rows = vec![row];
                self.pos = 0;
                self.rowcount = -1;
            }
        }
        Ok(())
    }

    fn is_write(sql: &str) -> bool {
        let t = sql.trim_start();
        ["insert", "update", "delete"]
            .iter()
            .any(|k| t.len() >= k.len() && t[..k.len()].eq_ignore_ascii_case(k))
    }
}

#[pymethods]
impl PyCursor {
    /// Subclass-instantiation path: `SQLiteCursorWrapper(connection)` — how
    /// Django constructs its cursor wrapper via `conn.cursor(factory=...)`.
    #[new]
    fn py_new(conn: Py<PyConnection>) -> Self {
        PyCursor::fresh(conn)
    }

    #[getter]
    fn lastrowid(&self, py: Python<'_>) -> Py<PyAny> {
        match self.lastrowid {
            Some(id) => id.into_pyobject(py).expect("i64").into_any().unbind(),
            None => py.None(),
        }
    }

    #[getter]
    fn get_row_factory(&self, py: Python<'_>) -> Py<PyAny> {
        match &self.row_factory {
            Some(f) => f.clone_ref(py),
            None => py.None(),
        }
    }

    #[setter]
    fn set_row_factory(&mut self, py: Python<'_>, f: Option<Py<PyAny>>) -> PyResult<()> {
        check_row_factory(py, &f)?;
        self.row_factory = f;
        Ok(())
    }

    #[getter]
    fn connection(&self, py: Python<'_>) -> Py<PyConnection> {
        self.conn.clone_ref(py)
    }

    /// PEP 249 `Cursor.execute()`.
    ///
    /// `?` needs no translation: mpedb's parser takes both `?` and `$n`
    /// natively (and refuses to mix them in one statement). Which is why there
    /// is no rewriter here — I wrote one, and it turned out to be a
    /// reimplementation of something the tokenizer already did.
    #[pyo3(signature = (sql, params=None))]
    fn execute(
        &mut self,
        py: Python<'_>,
        sql: &str,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let sql = sql.to_string();
        let vals = convert_params_sqlite3(params)?;
        let mut conn = self.conn.borrow_mut(py);
        if conn.closed {
            return Err(closed_err());
        }
        // The connection-bootstrap ritual every real sqlite3 consumer runs
        // before its first query (PY-COMPAT.md tier 1): PRAGMA in both forms,
        // and transaction control through execute(). Handled here, above the
        // backend split, so both engines answer identically.
        let bare = sql.trim().trim_end_matches(';').trim();
        if bare.len() >= 6 && bare[..6].eq_ignore_ascii_case("pragma") {
            drop(conn);
            return self.exec_pragma(py, bare[6..].trim());
        }
        let head = bare
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match head.as_str() {
            // BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]: opens the
            // REAL transaction (sqlite's semantics — Django drives its test
            // isolation with an explicit BEGIN and expects a later ROLLBACK
            // to undo everything since). Already in one → sqlite's exact
            // refusal. On the overlay backend the delta IS the transaction,
            // so BEGIN stays a no-op there (diskcache's _transact runs on
            // that backend).
            "begin" => {
                if let Backend::Native(db) = &conn.backend {
                    if conn.txn.is_some() {
                        return Err(crate::OperationalError::new_err(
                            "cannot start a transaction within a transaction",
                        ));
                    }
                    let db = db.clone();
                    conn.txn = Some(open_session(py, &db)?);
                }
                drop(conn);
                self.rowcount = -1;
                self.rows.clear();
                self.pos = 0;
                self.description = None;
                return Ok(());
            }
            "commit" | "end" => {
                PyConnection::commit(&mut conn, py)?;
                drop(conn);
                self.rowcount = -1;
                self.rows.clear();
                self.pos = 0;
                self.description = None;
                return Ok(());
            }
            "rollback" => {
                PyConnection::rollback(&mut conn, py)?;
                drop(conn);
                self.rowcount = -1;
                self.rows.clear();
                self.pos = 0;
                self.description = None;
                return Ok(());
            }
            _ => {}
        }
        // The overlay backend runs EVERYTHING per statement: reads consult
        // base+delta merged, writes land in the delta immediately (autocommit),
        // and `commit()` checkpoints into the base. Read-your-writes therefore
        // HOLDS on this backend — the delta is already durable.
        if let Backend::Overlay(ov, base) = &conn.backend {
            let ov = ov.clone();
            let base = base.clone();
            let sql2 = sql.clone();
            // DDL through the overlay: the overlay reads the BASE's schema and
            // has no DDL of its own, so the statement is applied TO THE BASE —
            // checkpoint first (push our deltas; an unpushed delta across a
            // schema change is exactly what the overlay refuses by name), run
            // the DDL via sqlite itself, reopen to re-derive the schema. The
            // forced sync point is a documented divergence from sqlite3's
            // transactional DDL.
            let t = sql2.trim_start();
            let is_ddl = ["create", "drop", "alter"]
                .iter()
                .any(|k| t.len() >= k.len() && t[..k.len()].eq_ignore_ascii_case(k));
            if is_ddl {
                py.detach(move || -> Result<(), DbError> {
                    let mut g = ov.lock().expect("overlay poisoned");
                    if let Some(o) = g.as_mut() {
                        o.checkpoint()?;
                    }
                    *g = None; // release the base (the overlay holds SHARED)
                    let c = rusqlite::Connection::open(&base)
                        .map_err(|e| DbError::Unsupported(format!("open base: {e}")))?;
                    c.execute_batch(&sql2)
                        .map_err(|e| DbError::Unsupported(format!("ddl: {e}")))?;
                    drop(c);
                    // The old overlay sidecar was seeded from the PRE-DDL
                    // schema; the checkpoint above emptied it, so dropping it
                    // is lossless and lets the reopen seed from the new
                    // schema (the overlay refuses a seed-hash mismatch).
                    let mut side = base.as_os_str().to_owned();
                    side.push(".overlay.mpedb");
                    let _ = std::fs::remove_file(std::path::Path::new(&side));
                    *g = Some(mpedb::SqliteOverlay::open(&base)?);
                    Ok(())
                })
                .map_err(map_err)?;
                drop(conn);
                self.rowcount = 0;
                self.rows.clear();
                self.pos = 0;
                self.description = None;
                return Ok(());
            }
            let res = py
                .detach(move || {
                    run_coercing(vals, |p| {
                        ov.lock()
                            .expect("overlay poisoned")
                            .as_mut()
                            .ok_or_else(|| DbError::Internal("overlay gone".into()))?
                            .query(&sql2, p)
                    })
                })
                .map_err(map_err)?;
            drop(conn);
            return self.load_result(py, res);
        }
        let Backend::Native(db) = &conn.backend else { unreachable!() };
        let db = db.clone();

        let is_write = PyCursor::is_write(&sql);
        // sqlite3's implicit-transaction rule: in a legacy isolation level
        // (the default `""`), the module opens the transaction before the
        // first DML; at `isolation_level = None` (autocommit — Django's mode)
        // nothing opens implicitly and a lone DML commits on its own below.
        if is_write && conn.txn.is_none() && conn.isolation_level.is_some() {
            conn.txn = Some(open_session(py, &db)?);
        }

        if let Some(session) = conn.txn.as_mut() {
            // Everything inside the open transaction — reads INCLUDED — runs
            // through the session, which is what makes a SELECT see this
            // connection's own uncommitted writes (sqlite's semantics; the
            // first real Django consumer's TestCase isolation depends on it).
            session.check_owner()?;
            let wm = &session.w;
            let sql2 = sql.clone();
            let res = py
                .detach(move || {
                    let mut w = wm.lock().expect("session poisoned");
                    run_coercing(vals, |p| w.query(&sql2, p))
                })
                .map_err(map_err)?;
            if is_write {
                if let Some(id) = mpedb::take_last_insert_rowid() {
                    self.lastrowid = Some(id);
                }
            }
            drop(conn);
            return self.load_result(py, res);
        }

        if !is_write {
            // No open transaction: a read runs against the committed snapshot.
            let sql2 = sql.clone();
            let res = py
                .detach(move || run_coercing(vals, |p| db.query(&sql2, p)))
                .map_err(map_err)?;
            drop(conn);
            return self.load_result(py, res);
        }

        // Autocommit DML (isolation_level = None, no explicit BEGIN): one
        // statement, one transaction, committed here — sqlite's autocommit.
        let sql2 = sql.clone();
        let res = py
            .detach(move || -> Result<ExecResult, DbError> {
                let mut w = db.begin()?;
                let r = run_coercing(vals, |p| w.query(&sql2, p))?;
                w.commit()?;
                Ok(r)
            })
            .map_err(map_err)?;
        if let Some(id) = mpedb::take_last_insert_rowid() {
            self.lastrowid = Some(id);
        }
        drop(conn);
        self.load_result(py, res)
    }

    /// PEP 249 `Cursor.executemany()`.
    fn executemany(
        &mut self,
        py: Python<'_>,
        sql: &str,
        seq: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let mut total = 0i64;
        for params in seq.try_iter()? {
            self.execute(py, sql, Some(&params?))?;
            if self.rowcount > 0 {
                total += self.rowcount;
            }
        }
        self.rowcount = total;
        Ok(())
    }

    /// sqlite3's `Cursor.executescript(script)`: commits any pending
    /// transaction first (the stdlib's rule), then runs the script's
    /// statements one by one — the splitter carves `CREATE TRIGGER … BEGIN
    /// <body> END` out whole, tracking `CASE`/`END` depth like sqlite's
    /// parser, and transaction control inside the script goes through the
    /// same interception `execute` applies.
    fn executescript(&mut self, py: Python<'_>, script: &str) -> PyResult<()> {
        {
            let mut conn = self.conn.borrow_mut(py);
            if conn.closed {
                return Err(closed_err());
            }
            PyConnection::commit(&mut conn, py)?;
        }
        let mut rest = script;
        while !mpedb::sqlscript::is_blank(rest) {
            let (stmt, tail) = mpedb::sqlscript::split_first(rest);
            if !mpedb::sqlscript::is_blank(stmt) {
                self.execute(py, stmt, None)?;
            }
            rest = tail;
        }
        Ok(())
    }

    fn fetchone(&mut self, py: Python<'_>) -> Option<Py<PyAny>> {
        let r = self.rows.get(self.pos).map(|r| r.clone_ref(py));
        if r.is_some() {
            self.pos += 1;
        }
        r
    }

    #[pyo3(signature = (size=1))]
    fn fetchmany(&mut self, py: Python<'_>, size: usize) -> Vec<Py<PyAny>> {
        let end = (self.pos + size).min(self.rows.len());
        let out = self.rows[self.pos..end].iter().map(|r| r.clone_ref(py)).collect();
        self.pos = end;
        out
    }

    fn fetchall(&mut self, py: Python<'_>) -> Vec<Py<PyAny>> {
        let out = self.rows[self.pos..].iter().map(|r| r.clone_ref(py)).collect();
        self.pos = self.rows.len();
        out
    }

    #[getter]
    fn description(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.description.as_ref().map(|d| d.clone_ref(py))
    }

    #[getter]
    fn rowcount(&self) -> i64 {
        self.rowcount
    }

    /// PEP 249 requires these to exist; mpedb has no server-side cursors, so
    /// they are no-ops rather than lies about a fetch size.
    #[pyo3(signature = (_size=None))]
    fn setinputsizes(&self, _size: Option<&Bound<'_, PyAny>>) {}
    #[pyo3(signature = (_size, _column=None))]
    fn setoutputsize(&self, _size: &Bound<'_, PyAny>, _column: Option<&Bound<'_, PyAny>>) {}

    fn close(&mut self) {
        self.rows.clear();
        self.pos = 0;
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.fetchone(py)
    }
}

/// PEP 249 `description`: one 7-tuple per column. Only `name` is known here;
/// the standard says the other six may be None.
fn describe(py: Python<'_>, columns: &[String]) -> PyResult<Py<PyAny>> {
    let out = PyList::empty(py);
    for c in columns {
        let t = (
            c.as_str(),
            py.None(),
            py.None(),
            py.None(),
            py.None(),
            py.None(),
            py.None(),
        );
        out.append(t.into_pyobject(py)?)?;
    }
    Ok(out.into_any().unbind())
}

/// PEP 249 `connect()` — sqlite3-shaped: takes a DATABASE path, not a config.
///
/// Routing (the `pip install mpedb` contract):
/// - `":memory:"`            → native mpedb, process-private memory
/// - `*.db`                  → sqlite-backed overlay (#69): reads the sqlite
///   file, writes land in an mpedb delta, `commit()` checkpoints back into the
///   `.db` — one store, kept in sync
/// - `*.toml`                → an explicit mpedb config file (the pre-package
///   behaviour, kept for callers that configure schema/durability up front)
/// - anything else (`.mpedb`)→ native mpedb file; tables are created with
///   ordinary `CREATE TABLE` (live DDL)
///
/// `engine="mpedb"` forces the native engine for ANY path — the
/// `mpedb.mpedb` submodule; `engine="sqlite3"` is the default routing above.
#[pyfunction]
#[pyo3(signature = (path, engine=None, isolation_level=Some(String::new())))]
pub(crate) fn connect(
    py: Python<'_>,
    path: PathBuf,
    engine: Option<&str>,
    isolation_level: Option<String>,
) -> PyResult<PyConnection> {
    let spelled = path.to_string_lossy().into_owned();
    let force_native = matches!(engine, Some("mpedb"));
    if !force_native && spelled.ends_with(".db") {
        let base = path.clone();
        let ov = py
            .detach(move || {
                // sqlite3.connect CREATES a missing database, and the overlay
                // refuses a base with NO tables — so a fresh/empty base is
                // seeded with one inert bootstrap table, IN THE BASE, by
                // sqlite itself (visible to sqlite tools, clearly named).
                // User tables arrive via CREATE TABLE (the DDL path above).
                let c = rusqlite::Connection::open(&path)
                    .map_err(|e| DbError::Unsupported(format!("create {}: {e}", path.display())))?;
                let n: i64 = c
                    .query_row("SELECT count(*) FROM sqlite_master WHERE type='table'", [], |r| r.get(0))
                    .map_err(|e| DbError::Unsupported(format!("probe {}: {e}", path.display())))?;
                if n == 0 {
                    c.execute_batch(
                        "CREATE TABLE mpedb_bootstrap (id INTEGER PRIMARY KEY)",
                    )
                    .map_err(|e| DbError::Unsupported(format!("init {}: {e}", path.display())))?;
                }
                drop(c);
                mpedb::SqliteOverlay::open(&path)
            })
            .map_err(map_err)?;
        return Ok(PyConnection {
            backend: Backend::Overlay(Arc::new(Mutex::new(Some(ov))), base),
            txn: None,
            isolation_level,
            row_factory: None,
            closed: false,
            text_factory: None,
        });
    }
    let db = if spelled.ends_with(".toml") && !force_native {
        py.detach(move || Db::open(&path)).map_err(map_err)?
    } else {
        // A database PATH (`:memory:`, `.mpedb`, or forced native): a minimal
        // config with no seed tables — `CREATE TABLE` (live DDL, #47) is how a
        // sqlite3-shaped caller builds schema. `size_mb` is PREALLOCATED
        // (fallocate), so the drop-in default is deliberately small — the
        // 1 GiB default ENOSPC'd a test suite that creates throwaway DBs
        // (PY-COMPAT.md tier 1). Bigger databases declare a .toml config.
        // mpedb refuses a schema with NO live tables, and a sqlite3-shaped
        // `connect("new.mpedb")` carries no schema — so the seed holds one
        // inert bootstrap table (the mpedb-capi solution, same name family);
        // user tables are created live via `CREATE TABLE`.
        let toml = format!(
            "[database]\npath = \"{}\"\nsize_mb = 64\nmax_readers = 64\n\n\
             [[table]]\nname = \"_mpedb_py_bootstrap\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n",
            // Was `replace('\\', "/").replace('"', "")` — which rewrote the
            // path rather than escaping it, and DELETING a quote silently opens
            // a different file than the caller named. Escape, never rewrite.
            mpedb::toml_escape(&spelled)
        );
        py.detach(move || {
            let cfg = mpedb::Config::from_toml_str(&toml)?;
            Db::open_with_config(cfg)
        })
        .map_err(map_err)?
    };
    Ok(PyConnection {
        backend: Backend::Native(Arc::new(db)),
        txn: None,
        isolation_level,
        row_factory: None,
        closed: false,
        text_factory: None,
    })
}

