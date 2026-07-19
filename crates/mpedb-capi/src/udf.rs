//! Host scalar UDF support for the C-API shim — the `sqlite3_create_function`
//! path (design/DESIGN-UDF.md, stage 1: SCALAR only).
//!
//! A registered C `xFunc` is wrapped in a Rust closure that, on each SQL call,
//! builds shim [`SqliteValue`] arguments and a [`SqliteContext`], calls back
//! into `xFunc`, and maps the written result cell (or a `sqlite3_result_error`)
//! to an mpedb `Value`/`Error`. Every allocation is per-call; text/blob are
//! copied in (into `SqliteValue`) and out (in `sqlite3_result_text/_blob`), so
//! nothing aliases the engine's buffers.

use crate::valconv;
use mpedb::{Error as DbError, Result as DbResult, Value};
use std::os::raw::{c_char, c_int, c_uchar, c_void};

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

/// The result/error cell a host UDF writes through `sqlite3_result_*`, plus the
/// `pApp` returned by `sqlite3_user_data`.
pub struct SqliteContext {
    result: Value,
    /// `Some((code, message))` once `sqlite3_result_error[_code]` was called.
    error: Option<(c_int, String)>,
    p_app: *mut c_void,
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
}

/// A registered scalar UDF's identity + teardown state, tracked on the
/// connection so it can invoke the caller's `xDestroy(pApp)` when the entry is
/// replaced, deleted, or the connection closes (so CPython doesn't leak the
/// wrapped callable).
pub struct HostFn {
    pub name: String,
    pub n_arg: i32,
    pub x_destroy: *mut c_void,
    pub p_app: *mut c_void,
}

impl HostFn {
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
            Some((_, msg)) => Err(DbError::Unsupported(format!("user function raised: {msg}"))),
            None => Ok(ctx.result),
        }
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
