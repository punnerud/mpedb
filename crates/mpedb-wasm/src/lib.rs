//! The mpedb browser playground — **the real engine**, compiled to wasm32.
//!
//! Nothing here reimplements or simulates mpedb. `Database::open_with_config`
//! runs the same code a native `:memory:` database runs: the COW B+tree, MVCC
//! snapshots, the freelist commit fixpoint, the rigid type/CHECK validation,
//! the MPEE join solver, content-hashed plans. Only the *kernel* is emulated,
//! by `mpedb_core::wasmcompat`, whose header argues each stub.
//!
//! That is the point of the demo, and it is also its honesty constraint: what
//! the page prints is what the engine returned. A query that errors reports
//! the engine's own message verbatim — the refusals ARE the product.
//!
//! # ABI
//!
//! A hand-rolled C ABI, no wasm-bindgen (see Cargo.toml). Two directions:
//!
//! - **In**: JS calls [`mpedb_alloc`], writes UTF-8 into the returned buffer,
//!   and passes `(ptr, len)`.
//! - **Out**: every call returns a pointer to `[u32 little-endian length][UTF-8
//!   JSON]`, which JS reads and then hands back to [`mpedb_free_result`].
//!
//! Every entry point returns a JSON document; failures are `{"ok":false,
//! "error":…}` rather than traps, so the page can always render something true.

use std::cell::RefCell;

use mpedb::{Config, Database, ExecResult};
use mpedb_sql::CompiledPlan;
use mpedb_types::Value;

pub mod demo;
pub mod examples;
mod json;

use json::{push_f64, push_str, Sep};

thread_local! {
    /// The one database this module instance owns. `wasm32-unknown-unknown`
    /// has no threads, so a thread-local IS a process-global here — and unlike
    /// a `static mut` it needs no `unsafe` to touch.
    static DB: RefCell<Option<Database>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Buffer plumbing
// ---------------------------------------------------------------------------

/// Allocate `len` bytes in the wasm heap for JS to write UTF-8 SQL into.
///
/// # Safety
/// The caller must eventually pass the pointer back to [`mpedb_free`] with the
/// same `len`.
#[no_mangle]
pub extern "C" fn mpedb_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Release a buffer obtained from [`mpedb_alloc`].
///
/// # Safety
/// `ptr`/`len` must be exactly what [`mpedb_alloc`] returned and was asked for.
#[no_mangle]
pub unsafe extern "C" fn mpedb_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

/// Release a result buffer returned by [`mpedb_open`] or [`mpedb_run`]. Reads
/// the length prefix itself, so JS need not track sizes.
///
/// # Safety
/// `ptr` must be a pointer this module returned and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn mpedb_free_result(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let len = u32::from_le_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]) as usize;
    drop(Vec::from_raw_parts(ptr, 0, len + 4));
}

/// Move a JSON string into a length-prefixed buffer JS can read.
fn ret(s: String) -> *mut u8 {
    let bytes = s.into_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 4);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
    let p = out.as_mut_ptr();
    std::mem::forget(out);
    p
}

fn err_json(msg: &str) -> String {
    let mut s = String::from("{\"ok\":false,\"error\":");
    push_str(&mut s, msg);
    s.push('}');
    s
}

/// # Safety
/// `ptr`/`len` must describe a live UTF-8 buffer from [`mpedb_alloc`].
unsafe fn take_str<'a>(ptr: *const u8, len: usize) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("null input pointer".into());
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len))
        .map_err(|e| format!("input was not valid UTF-8: {e}"))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Create the demo database. Idempotent: replaces any existing one, so the
/// page's "reset" button is just another call.
///
/// Returns `{"ok":true,"schema":[…],"seed_sql":"…"}` — the page shows the
/// exact DDL and INSERTs that produced what the user is querying, because a
/// demo whose data appeared by magic cannot be checked by the visitor.
#[no_mangle]
pub extern "C" fn mpedb_open() -> *mut u8 {
    ret(match demo::create() {
        Ok((db, seed_sql)) => {
            let mut s = String::from("{\"ok\":true,\"seed_sql\":");
            push_str(&mut s, &seed_sql);
            s.push_str(",\"tables\":");
            schema_json(&mut s, &db);
            s.push('}');
            DB.with(|d| *d.borrow_mut() = Some(db));
            s
        }
        Err(e) => err_json(&format!("could not create the demo database: {e}")),
    })
}

/// Run one SQL statement and report everything the page shows: rows, the
/// engine's own EXPLAIN, the plan hash, the footprint, and the MPEE A/B.
///
/// # Safety
/// `ptr`/`len` must describe a live UTF-8 buffer from [`mpedb_alloc`].
#[no_mangle]
pub unsafe extern "C" fn mpedb_run(ptr: *const u8, len: usize) -> *mut u8 {
    let sql = match take_str(ptr, len) {
        Ok(s) => s,
        Err(e) => return ret(err_json(&e)),
    };
    ret(DB.with(|d| match d.borrow().as_ref() {
        Some(db) => run_one(db, sql),
        None => err_json("the demo database is not open yet (call mpedb_open first)"),
    }))
}

/// The playground's example queries, from [`examples::GROUPS`] — the same list
/// `tests/examples.rs` asserts against, so a button cannot claim something the
/// engine no longer does.
#[no_mangle]
pub extern "C" fn mpedb_examples() -> *mut u8 {
    let mut s = String::from("{\"ok\":true,\"groups\":[");
    let mut gs = Sep::new();
    for g in examples::GROUPS {
        gs.sep(&mut s);
        s.push_str("{\"name\":");
        push_str(&mut s, g.name);
        s.push_str(",\"items\":[");
        let mut is = Sep::new();
        for it in g.items {
            is.sep(&mut s);
            s.push_str("{\"label\":");
            push_str(&mut s, it.label);
            s.push_str(",\"why\":");
            push_str(&mut s, it.why);
            s.push_str(",\"sql\":");
            push_str(&mut s, it.sql);
            s.push_str(",\"refuses\":");
            s.push_str(if it.expect == examples::Expect::Refuses { "true" } else { "false" });
            s.push('}');
        }
        s.push_str("]}");
    }
    s.push_str("]}");
    ret(s)
}

/// The mpedb version string the page displays, so a stale cached wasm is
/// visible rather than silent.
#[no_mangle]
pub extern "C" fn mpedb_version() -> *mut u8 {
    let mut s = String::from("{\"ok\":true,\"version\":");
    push_str(&mut s, env!("CARGO_PKG_VERSION"));
    s.push_str(",\"plan_format\":");
    s.push_str(&mpedb_types::FORMAT_VERSION.to_string());
    s.push('}');
    ret(s)
}

// ---------------------------------------------------------------------------
// One statement
// ---------------------------------------------------------------------------

fn run_one(db: &Database, sql: &str) -> String {
    let trimmed = sql.trim().trim_end_matches(';');
    if trimmed.is_empty() {
        return err_json("empty statement");
    }

    // Compile FIRST, and separately from execution, so the page can show the
    // plan even for a statement that then fails at run time — and so a
    // compile-time refusal (a type error, an unknown column) is reported as
    // exactly that. `prepare_detached` runs the same `compile_maybe_explain`
    // that `prepare`/`execute` use, so this IS the plan that runs.
    let mut out = String::from("{\"ok\":true");
    match db.prepare_detached(trimmed) {
        Ok(det) => {
            out.push_str(",\"plan_hash\":");
            push_str(&mut out, &det.hash.to_string());
            out.push_str(",\"plan_bytes\":");
            out.push_str(&det.blob.len().to_string());
            let bundle = db.schema();
            if let Ok(plan) = CompiledPlan::decode(&det.blob, &bundle.schema) {
                out.push_str(",\"explain\":");
                push_str(&mut out, plan.explain(&bundle.schema).trim_end());
                out.push_str(",\"footprint\":");
                footprint_json(&mut out, &plan, &bundle.schema);
            }
            out.push_str(",\"mpee\":");
            mpee_json(&mut out, db, trimmed);
        }
        Err(e) => {
            // A statement that does not compile has no plan to show. Report
            // the engine's message and stop — never invent a plan.
            return err_json(&e.to_string());
        }
    }

    match db.query(trimmed, &[]) {
        Ok(res) => {
            out.push_str(",\"result\":");
            result_json(&mut out, res);
            out.push('}');
            out
        }
        // Compiled but refused at execution: a CHECK violation, a type
        // mismatch, a UNIQUE conflict. This is the interesting case for the
        // demo, and the message is the engine's own, unedited.
        Err(e) => {
            let mut s = String::from("{\"ok\":false,\"error\":");
            push_str(&mut s, &e.to_string());
            s.push_str(",\"stage\":\"execute\"}");
            s
        }
    }
}

fn result_json(out: &mut String, res: ExecResult) {
    match res {
        ExecResult::Rows { columns, rows } => {
            out.push_str("{\"kind\":\"rows\",\"columns\":[");
            let mut sep = Sep::new();
            for c in &columns {
                sep.sep(out);
                push_str(out, c);
            }
            out.push_str("],\"rows\":[");
            let mut sep = Sep::new();
            for r in &rows {
                sep.sep(out);
                out.push('[');
                let mut inner = Sep::new();
                for v in r {
                    inner.sep(out);
                    value_json(out, v);
                }
                out.push(']');
            }
            out.push_str("]}");
        }
        ExecResult::Affected(n) => {
            out.push_str("{\"kind\":\"affected\",\"n\":");
            out.push_str(&n.to_string());
            out.push('}');
        }
        ExecResult::Explain(text) => {
            out.push_str("{\"kind\":\"explain\",\"text\":");
            push_str(out, &text);
            out.push('}');
        }
    }
}

/// A value as `{"t":<tag>,"v":<json>}`. The tag is what makes the demo able to
/// show mpedb's rigidity: an integer column that returned an integer is
/// visibly an integer, not a string that happens to look like one.
fn value_json(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("{\"t\":\"null\"}"),
        Value::Int(n) => {
            // i64 exceeds JS's exact integer range; send it as a string so the
            // page never displays a rounded number the engine did not produce.
            out.push_str("{\"t\":\"int\",\"v\":");
            push_str(out, &n.to_string());
            out.push('}');
        }
        Value::Float(f) => {
            out.push_str("{\"t\":\"float\",\"v\":");
            push_f64(out, *f);
            out.push('}');
        }
        Value::Bool(b) => {
            out.push_str("{\"t\":\"bool\",\"v\":");
            out.push_str(if *b { "true" } else { "false" });
            out.push('}');
        }
        Value::Text(s) => {
            out.push_str("{\"t\":\"text\",\"v\":");
            push_str(out, s);
            out.push('}');
        }
        Value::Blob(b) => {
            out.push_str("{\"t\":\"blob\",\"v\":");
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            push_str(out, &hex);
            out.push('}');
        }
        Value::Timestamp(us) => {
            out.push_str("{\"t\":\"timestamp\",\"v\":");
            push_str(out, &us.to_string());
            out.push('}');
        }
        Value::List(items) => {
            out.push_str("{\"t\":\"list\",\"v\":[");
            let mut sep = Sep::new();
            for i in items {
                sep.sep(out);
                value_json(out, i);
            }
            out.push_str("]}");
        }
    }
}

fn footprint_json(out: &mut String, plan: &CompiledPlan, schema: &mpedb_types::Schema) {
    let fp = &plan.footprint;
    let names = |set: &mpedb_types::TableSet| -> Vec<String> {
        (0..u32::try_from(schema.tables.len()).unwrap_or(0))
            .filter(|id| set.contains(*id))
            .map(|id| {
                schema
                    .table(id)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| format!("#{id}"))
            })
            .collect()
    };
    out.push_str("{\"read_only\":");
    out.push_str(if fp.read_only { "true" } else { "false" });
    out.push_str(",\"tables_read\":[");
    let mut sep = Sep::new();
    for n in names(&fp.tables_read) {
        sep.sep(out);
        push_str(out, &n);
    }
    out.push_str("],\"tables_written\":[");
    let mut sep = Sep::new();
    for n in names(&fp.tables_written) {
        sep.sep(out);
        push_str(out, &n);
    }
    out.push_str("],\"indexes_used\":");
    push_str(out, &format!("{:#x}", fp.indexes_used));
    out.push_str(",\"key_access\":");
    push_str(
        out,
        match &fp.key_access {
            mpedb_types::KeyAccess::Point(_) => "Point",
            mpedb_types::KeyAccess::Range { .. } => "Range",
            mpedb_types::KeyAccess::Full => "Full",
        },
    );
    out.push('}');
}

/// Compile the statement twice — solver on, solver off — and report both join
/// orders. This is the same `MPEDB_NO_MPEE` A/B the repo uses natively, driven
/// through `set_mpee_enabled` because a browser has no environment.
///
/// The switch is restored to `None` (defer to the environment) afterwards, so
/// the ordinary compile above and every later query are unaffected.
fn mpee_json(out: &mut String, db: &Database, sql: &str) {
    let order = |on: bool| -> Option<String> {
        mpedb_sql::set_mpee_enabled(Some(on));
        let r = db.prepare_detached(sql).ok().and_then(|d| {
            let bundle = db.schema();
            CompiledPlan::decode(&d.blob, &bundle.schema)
                .ok()
                .map(|p| (p.explain(&bundle.schema), d.hash.to_string()))
        });
        mpedb_sql::set_mpee_enabled(None);
        r.map(|(text, hash)| {
            let line = text
                .lines()
                .find(|l| l.trim_start().starts_with("join order:"))
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            format!("{line}\u{1}{hash}")
        })
    };
    let on = order(true);
    let off = order(false);
    // Only meaningful when the statement actually has a join chain.
    match (on, off) {
        (Some(a), Some(b)) => {
            let (a_line, a_hash) = a.split_once('\u{1}').unwrap_or((a.as_str(), ""));
            let (b_line, b_hash) = b.split_once('\u{1}').unwrap_or((b.as_str(), ""));
            if a_line.is_empty() && b_line.is_empty() {
                out.push_str("{\"applies\":false}");
                return;
            }
            out.push_str("{\"applies\":true,\"chosen\":");
            push_str(out, a_line);
            out.push_str(",\"textual\":");
            push_str(out, b_line);
            out.push_str(",\"reordered\":");
            out.push_str(if a_line != b_line { "true" } else { "false" });
            out.push_str(",\"same_hash\":");
            out.push_str(if a_hash == b_hash { "true" } else { "false" });
            out.push('}');
        }
        _ => out.push_str("{\"applies\":false}"),
    }
}

fn schema_json(out: &mut String, db: &Database) {
    let bundle = db.schema();
    out.push('[');
    let mut sep = Sep::new();
    for t in &bundle.schema.tables {
        if t.dead {
            continue;
        }
        sep.sep(out);
        out.push_str("{\"name\":");
        push_str(out, &t.name);
        out.push_str(",\"columns\":[");
        let mut cs = Sep::new();
        for (i, c) in t.columns.iter().enumerate() {
            cs.sep(out);
            out.push_str("{\"name\":");
            push_str(out, &c.name);
            out.push_str(",\"type\":");
            push_str(out, &format!("{:?}", c.ty));
            out.push_str(",\"nullable\":");
            out.push_str(if c.nullable { "true" } else { "false" });
            out.push_str(",\"pk\":");
            let is_pk = t.primary_key.contains(&(i as u16));
            out.push_str(if is_pk { "true" } else { "false" });
            out.push_str(",\"check\":");
            match &c.check {
                Some(x) => push_str(out, x),
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push_str("],\"indexes\":");
        out.push_str(&t.indexes.len().to_string());
        out.push('}');
    }
    out.push(']');
}

fn open_demo_config() -> Result<Config, mpedb_types::Error> {
    Config::from_toml_str(demo::CONFIG_TOML)
}

/// Open a fresh in-memory database with the playground's geometry. Public so
/// the native test can exercise the exact same path the browser takes.
pub fn open_database() -> Result<Database, mpedb_types::Error> {
    Database::open_with_config(open_demo_config()?)
}
