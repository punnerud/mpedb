//! Host UDF support for the C-API shim — the `sqlite3_create_function` path
//! (design/DESIGN-UDF.md): stage 1 SCALAR (`xFunc`) and stage 2 AGGREGATE
//! (`xStep`/`xFinal` + `sqlite3_aggregate_context`).
//!
//! A registered C `xFunc` is wrapped in a Rust closure that, on each SQL call,
//! builds shim [`SqliteValue`] arguments and a [`SqliteContext`], calls back
//! into `xFunc`, and maps the written result cell (or a `sqlite3_result_error`)
//! to an mpedb `Value`/`Error`. Every allocation is per-call; text/blob are
//! copied in (into `SqliteValue`) and out (in `sqlite3_result_text/_blob`), so
//! nothing aliases the engine's buffers.
//!
//! An aggregate uses the SAME marshalling, with one thing added: the per-
//! aggregation memory sqlite exposes as `sqlite3_aggregate_context`. That
//! buffer lives in the [`CAggState`] the engine mints for the group, so every
//! `xStep` and the final `xFinal` see one stable, zero-initialized allocation,
//! and it is freed when the state is consumed by `xFinal`.

use crate::valconv;
use mpedb::{Error as DbError, Result as DbResult, Value};
use std::os::raw::{c_char, c_int, c_uchar, c_void};

thread_local! {
    /// The `(code, message)` of the LAST `sqlite3_result_error*` that failed a
    /// UDF call on this thread. The engine can only tunnel the error as an
    /// opaque string (`Error::Unsupported("user function raised: …")`), which
    /// loses the CODE the callback chose — and CPython dispatches on it
    /// (NOMEM -> MemoryError, TOOBIG -> DataError) and asserts the exact TEXT.
    /// `run_stmt` drains this before executing and, when the statement's
    /// failure IS this error, presents the callback's own code + message.
    static LAST_UDF_ERROR: std::cell::RefCell<Option<(c_int, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// Take (and clear) the last UDF error recorded on this thread.
pub fn take_last_udf_error() -> Option<(c_int, String)> {
    LAST_UDF_ERROR.with(|c| c.borrow_mut().take())
}

pub(crate) fn stash_udf_error(code: c_int, msg: &str) {
    LAST_UDF_ERROR.with(|c| *c.borrow_mut() = Some((code, msg.to_string())));
}

/// The C scalar-callback signature `void(*)(sqlite3_context*, int,
/// sqlite3_value**)`. The shim's [`SqliteContext`]/[`SqliteValue`] stand in for
/// sqlite's opaque `sqlite3_context`/`sqlite3_value`; at the ABI level both are
/// plain pointers, so the C caller and these Rust types agree.
pub type XFunc = unsafe extern "C" fn(*mut SqliteContext, c_int, *mut *mut SqliteValue);

/// The raw callback + `pApp` a `create_function` registration carries. Raw
/// pointers are not `Send`/`Sync`, but a sqlite connection is single-threaded
/// per the C-API contract and the UDF only ever runs on the thread executing the
/// statement — so wrapping them for the facade's `Send + Sync` closure bound is
/// sound.
struct XFuncPtrs {
    x_func: XFunc,
    p_app: *mut c_void,
}
unsafe impl Send for XFuncPtrs {}
unsafe impl Sync for XFuncPtrs {}

impl XFuncPtrs {
    /// Call the C scalar callback.
    ///
    /// These are METHODS rather than direct field reads on purpose: edition-2021
    /// closures capture by FIELD, so a closure touching `ptrs.x_func` /
    /// `ptrs.p_app` would capture two bare `*mut c_void`s and bypass the
    /// `Send`/`Sync` impls above. Going through `&self` captures the whole
    /// struct, which is the thing that carries those impls.
    unsafe fn invoke(&self, ctx: *mut SqliteContext, argc: c_int, argv: *mut *mut SqliteValue) {
        (self.x_func)(ctx, argc, argv)
    }
    fn p_app(&self) -> *mut c_void {
        self.p_app
    }
}

/// One argument handed to a host UDF: an mpedb `Value` plus stable byte buffers
/// backing `sqlite3_value_text`/`_blob` — sqlite's contract is that those
/// pointers stay valid until the value is destroyed, i.e. through the `xFunc`
/// call, which is exactly this struct's lifetime.
pub struct SqliteValue {
    v: Value,
    /// Canonical bytes (text/blob rendering); empty for NULL.
    payload: Vec<u8>,
    /// `payload` followed by a NUL terminator, for `_text`.
    text_nul: Vec<u8>,
}

impl SqliteValue {
    fn new(v: Value) -> SqliteValue {
        let payload = valconv::as_bytes(&v).unwrap_or_default();
        let mut text_nul = payload.clone();
        text_nul.push(0);
        SqliteValue { v, payload, text_nul }
    }

    pub fn value(&self) -> &Value {
        &self.v
    }
    pub fn text_ptr(&self) -> *const c_uchar {
        self.text_nul.as_ptr()
    }
    pub fn blob_ptr(&self) -> *const c_void {
        self.payload.as_ptr() as *const c_void
    }
    pub fn bytes_len(&self) -> c_int {
        self.payload.len() as c_int
    }
}

/// The per-AGGREGATION memory behind `sqlite3_aggregate_context` — one buffer
/// shared by every `xStep` of one group and by its `xFinal`.
///
/// Allocated lazily and exactly once, on the first `sqlite3_aggregate_context(
/// ctx, n)` with `n > 0`, ZEROED (sqlite's documented contract — Django's
/// accumulators rely on a zeroed struct being a valid empty state). The `Vec` is
/// never grown afterwards, so `as_mut_ptr()` is the same address for the whole
/// aggregation, which is the OTHER half of the contract.
#[derive(Default)]
pub struct AggMem {
    buf: Vec<u8>,
}

impl AggMem {
    /// sqlite semantics: `n > 0` allocates-and-zeroes on the first call and
    /// returns the SAME pointer on every later call of this aggregation;
    /// `n <= 0` never allocates — it returns the existing buffer, or NULL if
    /// there is none (how `xFinal` detects a group that was never stepped).
    fn context(&mut self, n: c_int) -> *mut c_void {
        if self.buf.is_empty() {
            if n <= 0 {
                return std::ptr::null_mut();
            }
            self.buf = vec![0u8; n as usize];
        }
        self.buf.as_mut_ptr() as *mut c_void
    }
}

/// The result/error cell a host UDF writes through `sqlite3_result_*`, plus the
/// `pApp` returned by `sqlite3_user_data` and (for an aggregate) the
/// aggregation's [`AggMem`].
pub struct SqliteContext {
    result: Value,
    /// `Some((code, message))` once `sqlite3_result_error[_code]` was called.
    error: Option<(c_int, String)>,
    p_app: *mut c_void,
    /// The aggregation's context memory, or NULL for a scalar call — where
    /// `sqlite3_aggregate_context` correctly returns NULL, as sqlite does when
    /// it is misused outside an aggregate.
    agg: *mut AggMem,
}

impl SqliteContext {
    pub fn set_result(&mut self, v: Value) {
        self.result = v;
    }
    pub fn set_error(&mut self, code: c_int, msg: String) {
        self.error = Some((code, msg));
    }
    pub fn p_app(&self) -> *mut c_void {
        self.p_app
    }
    /// `sqlite3_aggregate_context(ctx, nBytes)`.
    ///
    /// # Safety
    /// Only valid while the [`CAggState`] that lent `agg` is alive — i.e. inside
    /// the `xStep`/`xFinal` call this context was built for, which is the only
    /// window a C callback ever holds the pointer.
    pub unsafe fn aggregate_context(&mut self, n: c_int) -> *mut c_void {
        match self.agg.is_null() {
            true => std::ptr::null_mut(),
            false => (*self.agg).context(n),
        }
    }
}

/// A registered UDF's identity + teardown state, tracked on the connection so it
/// can invoke the caller's `xDestroy(pApp)` when the entry is replaced, deleted,
/// or the connection closes (so CPython doesn't leak the wrapped callable).
pub struct HostFn {
    pub name: String,
    pub n_arg: i32,
    /// `true` for an `xStep`/`xFinal` AGGREGATE registration. Scalars and
    /// aggregates live in SEPARATE facade registries, so removing an entry has
    /// to know which one it came from — getting this wrong would leave a stale
    /// registration behind under the same `(name, n_arg)`.
    pub aggregate: bool,
    pub x_destroy: *mut c_void,
    pub p_app: *mut c_void,
    /// The callbacks themselves, kept alongside the teardown state so a
    /// registration can be RE-INSTALLED into a different `Database` — which is
    /// what `sqlite3_backup_*` does to the destination connection when it
    /// replaces its file (the closures live in the facade registry, so a
    /// reopened database starts with none). `x_func` for a scalar,
    /// `x_step`/`x_final` for an aggregate; the unused ones are NULL.
    pub x_func: *mut c_void,
    pub x_step: *mut c_void,
    pub x_final: *mut c_void,
}

impl HostFn {
    /// Install this registration into `db` (see the `x_func` field): used to
    /// carry a connection's UDFs across a database the shim had to reopen.
    ///
    /// # Safety
    /// The stored callback pointers must still be the caller's live functions —
    /// true for as long as the connection is open, which is the only window in
    /// which this is called.
    pub unsafe fn reinstall(&self, db: &mpedb::Database) {
        if self.aggregate {
            let step: XStep = std::mem::transmute(self.x_step);
            let fin: XFinal = std::mem::transmute(self.x_final);
            db.register_host_aggregate(&self.name, self.n_arg, make_agg_factory(step, fin, self.p_app));
        } else if !self.x_func.is_null() {
            let f: XFunc = std::mem::transmute(self.x_func);
            db.register_host_function(&self.name, self.n_arg, make_scalar_closure(f, self.p_app));
        }
    }

    /// Invoke the caller-supplied `xDestroy(pApp)` if present.
    pub unsafe fn destroy(&self) {
        if !self.x_destroy.is_null() {
            let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(self.x_destroy);
            f(self.p_app);
        }
    }
}

/// Build the `Fn(&[Value]) -> Result<Value>` closure the facade registers for a
/// scalar `xFunc`. Each call materializes the argument values + a context, calls
/// `xFunc`, then reads the result cell back (mapping a `sqlite3_result_error` to
/// an `Error`).
pub fn make_scalar_closure(
    x_func: XFunc,
    p_app: *mut c_void,
) -> impl Fn(&[Value]) -> DbResult<Value> + Send + Sync + 'static {
    let ptrs = XFuncPtrs { x_func, p_app };
    move |args: &[Value]| -> DbResult<Value> {
        // Owned argument values (buffers back the argv pointers) — kept alive
        // until after the call returns.
        let mut values: Vec<SqliteValue> = args.iter().cloned().map(SqliteValue::new).collect();
        let mut argv: Vec<*mut SqliteValue> =
            values.iter_mut().map(|v| v as *mut SqliteValue).collect();
        let mut ctx = SqliteContext {
            result: Value::Null,
            error: None,
            p_app: ptrs.p_app(),
            // A scalar has no aggregation, so `sqlite3_aggregate_context` in a
            // scalar callback returns NULL — sqlite's answer for that misuse.
            agg: std::ptr::null_mut(),
        };
        // SAFETY: `argv` points into `values`, both live through the call; `ctx`
        // is a valid, owned cell. The callback writes only through the shim
        // result/value accessors, which operate on these pointers.
        unsafe {
            ptrs.invoke(
                &mut ctx as *mut SqliteContext,
                args.len() as c_int,
                argv.as_mut_ptr(),
            );
        }
        // Explicit: `values`/`argv` must outlive the call above.
        drop(argv);
        drop(values);
        match ctx.error {
            Some((code, msg)) => {
                stash_udf_error(code, &msg);
                Err(DbError::Unsupported(format!("user function raised: {msg}")))
            }
            None => Ok(ctx.result),
        }
    }
}

// ---- aggregates (design/DESIGN-UDF.md stage 2) -----------------------------

/// The C aggregate step signature — identical to [`XFunc`]'s; sqlite reuses
/// `void(*)(sqlite3_context*, int, sqlite3_value**)` for `xStep`.
pub type XStep = XFunc;
/// The C aggregate finalizer, `void(*)(sqlite3_context*)`.
pub type XFinal = unsafe extern "C" fn(*mut SqliteContext);

/// The `xStep`/`xFinal`/`pApp` triple a `create_function` aggregate
/// registration carries. `Send`/`Sync` for the same reason [`XFuncPtrs`] is: a
/// sqlite connection is single-threaded per the C-API contract, and the
/// callbacks only ever run on the thread executing the statement.
#[derive(Clone, Copy)]
struct XAggPtrs {
    x_step: XStep,
    x_final: XFinal,
    p_app: *mut c_void,
}
unsafe impl Send for XAggPtrs {}
unsafe impl Sync for XAggPtrs {}

impl XAggPtrs {
    // Methods, not field reads, for the edition-2021 capture-by-field reason
    // spelled out on `XFuncPtrs::invoke`.
    unsafe fn step(&self, ctx: *mut SqliteContext, argc: c_int, argv: *mut *mut SqliteValue) {
        (self.x_step)(ctx, argc, argv)
    }
    unsafe fn finalize(&self, ctx: *mut SqliteContext) {
        (self.x_final)(ctx)
    }
    fn p_app(&self) -> *mut c_void {
        self.p_app
    }
}

/// One group's accumulation over a C aggregate: the callbacks, plus the
/// aggregation's `sqlite3_aggregate_context` memory.
pub struct CAggState {
    ptrs: XAggPtrs,
    mem: AggMem,
}

impl CAggState {
    /// Build the [`SqliteContext`] a callback sees. Borrowing `mem` MUTABLY here
    /// is what makes the aggregate-context pointer the same across steps.
    fn ctx(&mut self) -> SqliteContext {
        SqliteContext {
            result: Value::Null,
            error: None,
            p_app: self.ptrs.p_app(),
            agg: &mut self.mem as *mut AggMem,
        }
    }
}

impl mpedb::HostAggState for CAggState {
    fn step(&mut self, args: &[Value]) -> DbResult<()> {
        let mut values: Vec<SqliteValue> = args.iter().cloned().map(SqliteValue::new).collect();
        let mut argv: Vec<*mut SqliteValue> =
            values.iter_mut().map(|v| v as *mut SqliteValue).collect();
        let ptrs = self.ptrs;
        let mut ctx = self.ctx();
        // SAFETY: `argv` points into `values` and `ctx.agg` at `self.mem`; all
        // three outlive the call. The callback writes only through the shim
        // result/value/aggregate-context accessors, which operate on them.
        unsafe {
            ptrs.step(
                &mut ctx as *mut SqliteContext,
                args.len() as c_int,
                argv.as_mut_ptr(),
            );
        }
        drop(argv);
        drop(values);
        match ctx.error {
            Some((code, msg)) => {
                stash_udf_error(code, &msg);
                Err(DbError::Unsupported(format!(
                    "user aggregate raised: {msg}"
                )))
            }
            None => Ok(()),
        }
    }

    fn finish(mut self: Box<Self>) -> DbResult<Value> {
        let ptrs = self.ptrs;
        let mut ctx = self.ctx();
        // SAFETY: as `step`. `xFinal` runs exactly once — this method consumes
        // the state — and the aggregate context is freed when `self` drops on
        // return, which is sqlite's "freed after xFinal" contract.
        unsafe {
            ptrs.finalize(&mut ctx as *mut SqliteContext);
        }
        match ctx.error {
            Some((code, msg)) => {
                stash_udf_error(code, &msg);
                Err(DbError::Unsupported(format!(
                    "user aggregate raised: {msg}"
                )))
            }
            None => Ok(ctx.result),
        }
    }
}

/// Build the per-group factory the facade registers for an `xStep`/`xFinal`
/// pair: each call mints a fresh [`CAggState`] with its own zero-length (not yet
/// allocated) aggregate context.
pub fn make_agg_factory(
    x_step: XStep,
    x_final: XFinal,
    p_app: *mut c_void,
) -> impl Fn() -> Box<dyn mpedb::HostAggState> + Send + Sync + 'static {
    let ptrs = XAggPtrs { x_step, x_final, p_app };
    move || -> Box<dyn mpedb::HostAggState> {
        Box::new(CAggState { ptrs, mem: AggMem::default() })
    }
}

/// Copy a `sqlite3_result_text`/`_blob` byte buffer into an owned `Vec`, honoring
/// the length convention (`n < 0` = NUL-terminated for text). NULL pointer → an
/// empty buffer.
pub unsafe fn copy_result_bytes(p: *const c_char, n: c_int) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let len = if n < 0 {
        libc::strlen(p)
    } else {
        n as usize
    };
    std::slice::from_raw_parts(p as *const u8, len).to_vec()
}

/// The C collating-sequence callback: `int(*)(void*, int, const void*, int,
/// const void*)` — `(pArg, nA, pA, nB, pB)`, negative/zero/positive like
/// `strcmp`.
pub type XCompare =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int;

/// A registered collating sequence's identity + teardown state, tracked on the
/// connection so the caller's `xDestroy(pApp)` runs when the entry is replaced,
/// deleted, or the connection closes (CPython wraps a Python callable in
/// `pApp`).
pub struct HostColl {
    pub name: String,
    pub x_destroy: *mut c_void,
    pub p_app: *mut c_void,
    /// The comparator, kept for the same reason [`HostFn::x_func`] is.
    pub x_compare: *mut c_void,
}

impl HostColl {
    /// Install this collation into `db`.
    ///
    /// # Safety
    /// As [`HostFn::reinstall`].
    pub unsafe fn reinstall(&self, db: &mpedb::Database) {
        if !self.x_compare.is_null() {
            let cmp: XCompare = std::mem::transmute(self.x_compare);
            db.register_host_collation(&self.name, make_collation_closure(cmp, self.p_app));
        }
    }

    pub unsafe fn destroy(&self) {
        if !self.x_destroy.is_null() {
            let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(self.x_destroy);
            f(self.p_app);
        }
    }
}

/// `xCompare` + `pApp`, wrapped for the facade's `Send + Sync` closure bound.
/// Sound for the same reason `XFuncPtrs` is: a sqlite connection is
/// single-threaded per the C-API contract, and the comparator only ever runs on
/// the thread executing the statement.
struct XCmpPtrs {
    x_compare: XCompare,
    p_app: *mut c_void,
}
unsafe impl Send for XCmpPtrs {}
unsafe impl Sync for XCmpPtrs {}

impl XCmpPtrs {
    /// Call the C comparator with two UTF-8 byte runs. Lengths are BYTE counts,
    /// as sqlite passes them; the pointers are the string bodies and are NOT
    /// NUL-terminated (sqlite's contract — a collation must use the length).
    unsafe fn invoke(&self, a: &str, b: &str) -> c_int {
        (self.x_compare)(
            self.p_app,
            a.len() as c_int,
            a.as_ptr() as *const c_void,
            b.len() as c_int,
            b.as_ptr() as *const c_void,
        )
    }
}

/// Build the comparator the facade registers for an `xCompare`/`pApp` pair.
/// Only the SIGN of the callback's return is meaningful (sqlite's contract, and
/// what lets CPython's `test_collation_returns_large_integer` — which returns
/// ±2³² — order correctly).
pub fn make_collation_closure(
    x_compare: XCompare,
    p_app: *mut c_void,
) -> impl Fn(&str, &str) -> std::cmp::Ordering + Send + Sync + 'static {
    let ptrs = XCmpPtrs { x_compare, p_app };
    move |a: &str, b: &str| {
        // SAFETY: the pointers came from a `create_collation` registration on
        // this connection and are alive until the entry is replaced/deleted (at
        // which point the facade registration is dropped first).
        let r = unsafe { ptrs.invoke(a, b) };
        r.cmp(&0)
    }
}
