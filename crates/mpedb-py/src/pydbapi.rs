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

#[pyclass(name = "Connection", module = "mpedb")]
pub(crate) struct PyConnection {
    backend: Backend,
    /// sqlite3's `text_factory` — accepted and stored (str is the one
    /// behavior mpedb produces; sqlitedict SETS it at bootstrap and never
    /// needs another value).
    text_factory: Option<Py<PyAny>>,
    /// The open transaction, if anything has been written since the last
    /// commit/rollback. PEP 249 says a connection is always in a transaction;
    /// mpedb's writer lock is exclusive, so one is only TAKEN once there is
    /// something to write — otherwise a read-only connection would block every
    /// writer for as long as it stayed open.
    txn: Option<Session>,
    closed: bool,
}

/// The lazily-opened write session backing a `Connection`.
struct Session {
    /// Statements buffered since the last commit. Replayed inside one
    /// `WriteSession` at commit time.
    ///
    /// Buffering rather than holding the writer lock open is the whole design
    /// decision here: `conn.execute(…)` in a REPL or a web handler can sit for
    /// minutes before `commit()`, and mpedb has exactly one writer lock. A
    /// driver that grabbed it on the first INSERT would let one idle Python
    /// process stop every other writer on the machine.
    ///
    /// The cost is that a constraint violation surfaces at `commit()`, not at
    /// `execute()` — so `execute` runs each statement against a THROWAWAY
    /// session first, to fail early where the caller is looking. That doubles
    /// the work for writes; a caller who minds should use `Transaction`.
    pending: Vec<(String, Vec<Value>)>,
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

    /// PEP 249 `Connection.cursor()`.
    fn cursor(slf: Py<Self>) -> PyCursor {
        PyCursor {
            conn: slf,
            rows: Vec::new(),
            pos: 0,
            description: None,
            rowcount: -1,
        }
    }

    /// PEP 249 `Connection.commit()`.
    fn commit(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.closed {
            return Err(closed_err());
        }
        match &self.backend {
            Backend::Native(db) => {
                let Some(session) = self.txn.take() else {
                    return Ok(()); // nothing written; a no-op, as in sqlite3
                };
                let db = db.clone();
                py.detach(move || -> Result<(), DbError> {
                    let mut w = db.begin()?;
                    for (sql, params) in &session.pending {
                        w.query(sql, params)?;
                    }
                    w.commit()
                })
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

    /// PEP 249 `Connection.rollback()` — drop what was buffered.
    fn rollback(&mut self) -> PyResult<()> {
        if self.closed {
            return Err(closed_err());
        }
        self.txn = None;
        Ok(())
    }

    /// PEP 249 `Connection.close()`. Uncommitted work is discarded, which PEP
    /// 249 requires ("an implicit rollback is performed").
    fn close(&mut self) -> PyResult<()> {
        self.txn = None;
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
        let mut cur = PyConnection::cursor(slf);
        cur.execute(py, sql, params)?;
        Ok(cur)
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
            self.txn = None;
        } else {
            self.commit(py)?;
        }
        Ok(false) // never swallow the exception
    }
}

/// PEP 249 `Cursor`.
#[pyclass(name = "Cursor", module = "mpedb")]
pub(crate) struct PyCursor {
    conn: Py<PyConnection>,
    rows: Vec<Py<PyAny>>,
    pos: usize,
    /// PEP 249 `description`: 7-tuples, of which only `name` is meaningful
    /// here — the rest are None, which PEP 249 explicitly allows.
    description: Option<Py<PyAny>>,
    rowcount: i64,
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
                self.rows = list.iter().map(|r| r.unbind()).collect();
                self.pos = 0;
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
            // BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]: the PEP 249
            // connection is ALWAYS in a transaction (writes buffer until
            // commit; the overlay's delta is the transaction) — accept as a
            // no-op rather than refuse, which is what killed diskcache's
            // _transact.
            "begin" => {
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
                PyConnection::rollback(&mut conn)?;
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

        if !PyCursor::is_write(&sql) {
            // A read runs against the committed snapshot. It does NOT see this
            // connection's buffered writes — which is a real difference from
            // sqlite3 and is documented rather than papered over.
            let vals2 = vals.clone();
            let sql2 = sql.clone();
            let res = py
                .detach(move || run_coercing(vals2, |p| db.query(&sql2, p)))
                .map_err(map_err)?;
            drop(conn);
            return self.load_result(py, res);
        }

        // A write. Validate it NOW against a throwaway session so the error
        // lands where the caller is looking, then buffer it for commit().
        let db2 = db.clone();
        let sql2 = sql.clone();
        let vals2 = vals.clone();
        let n = py
            .detach(move || -> Result<u64, DbError> {
                let mut w = db2.begin()?;
                let r = w.query(&sql2, &vals2)?;
                w.rollback();
                Ok(match r {
                    ExecResult::Affected(n) => n,
                    _ => 0,
                })
            })
            .map_err(map_err)?;
        conn.txn
            .get_or_insert_with(|| Session { pending: Vec::new() })
            .pending
            .push((sql, vals));
        self.rowcount = n as i64;
        self.rows.clear();
        self.pos = 0;
        self.description = None;
        Ok(())
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
#[pyo3(signature = (path, engine=None))]
pub(crate) fn connect(py: Python<'_>, path: PathBuf, engine: Option<&str>) -> PyResult<PyConnection> {
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
        closed: false,
        text_factory: None,
    })
}

