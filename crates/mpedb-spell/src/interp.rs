//! The procedure interpreter: a tight loop over validated IR with a hard
//! instruction budget and a db-call budget. All structural safety (operand
//! bounds, jump targets, stack depths, db arity) was proven by
//! `ir::validate`, so the loop needs no per-instruction underflow checks to
//! be panic-free; what remains dynamic is *typing* (procs are dynamically
//! typed like Python) and the budgets.
//!
//! # Value model
//!
//! Scalars are exactly [`mpedb::Value`] — `Value::Null` is Python's `None`.
//! Query results introduce runtime-only containers ([`PValue::List`] of
//! [`PValue::Tuple`] rows) which never cross the database boundary: db-call
//! arguments must be scalars, and only scalars can be stored in rows.
//! Semantics are ordinary Python/Rust, **not** SQL 3VL: `None == None` is
//! true, `1 == 1.0` is true (numeric cross-comparison), ordering across
//! unrelated types is an error, arithmetic overflow and division by zero
//! are errors. Divergences from CPython, chosen for a rigidly-typed store:
//! bools are not ints (`True + 1` errors), and `bool`/`int` never compare
//! equal.
//!
//! Explicit `Sql*` instructions instead use SQL three-valued comparisons and
//! strictly typed BOOLEAN conditions. This lets procedural SQL frontends share
//! the validated, budgeted runtime without changing Python/Rust semantics.

mod oracle_exception;
mod sql_logic;
mod oracle_number;
mod plsql_number;
mod oracle_constraint;

pub(crate) fn validate_oracle_number_literal(text: &str) -> Result<()> {
    oracle_number::normalize(text).map(|_| ())
}

/// Oracle rounds EXCEPTION_INIT numeric literals as BINARY_INTEGER. Decimal
/// literals keep the same exact path as PL/SQL assignments, never f64.
pub(crate) fn oracle_exception_init_code(text: &str) -> Result<u32> {
    let code = if text.ends_with(['f', 'F', 'd', 'D']) {
        let suffix = text.as_bytes()[text.len() - 1];
        let digits = &text[..text.len() - 1];
        let n = if matches!(suffix, b'f' | b'F') {
            digits
                .parse::<f32>()
                .map(f64::from)
                .map_err(|_| Error::ArithmeticOverflow)?
        } else {
            digits
                .parse::<f64>()
                .map_err(|_| Error::ArithmeticOverflow)?
        };
        let rounded = n.round();
        if !rounded.is_finite() || !(-1_000_000.0..=100.0).contains(&rounded) {
            return Err(Error::ArithmeticOverflow);
        }
        rounded as i64
    } else {
        let Value::Int(n) = oracle_number::integer(PValue::Scalar(Value::Numeric(text.into())))?
        else {
            unreachable!("numeric literal")
        };
        n
    };
    match code {
        100 => Ok(1403),
        -1_000_000..=-1 if code != -1403 => Ok(code.unsigned_abs() as u32),
        _ => Err(Error::Unsupported(
            "illegal Oracle EXCEPTION_INIT error number".into(),
        )),
    }
}

mod oracle_text;
mod legacy_oracle_number;
#[cfg(test)]
mod oracle_number_tests;
#[cfg(test)]
mod tuple_tests;

use crate::ir::{Op, PlanRef, Proc};
use mpedb_types::{Error, Result, Value};
use std::collections::HashMap;
use std::rc::Rc;

/// Execution budgets. Every executed instruction costs one instruction unit
/// (backward jumps therefore pay on every loop iteration); every
/// `DbQuery`/`DbExec`/`CursorOpen` costs one db-call unit on top; every
/// `CursorAdvance`, and every `CursorFetch` that pulls, costs one **row**
/// unit — its own dimension, deliberately generous (10M): charging cursor
/// rows against the 10k db-call budget would burn it on a single medium
/// scan, while leaving them uncharged would let `SELECT`-all cursors bypass
/// row accounting entirely. A fetch that already found no row, and
/// `CursorClose`, cost no further row unit.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub instrs: u64,
    pub db_calls: u64,
    pub rows: u64,
}

impl Budget {
    pub const DEFAULT_INSTRS: u64 = 1_000_000;
    pub const DEFAULT_DB_CALLS: u64 = 10_000;
    pub const DEFAULT_ROWS: u64 = 10_000_000;
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            instrs: Budget::DEFAULT_INSTRS,
            db_calls: Budget::DEFAULT_DB_CALLS,
            rows: Budget::DEFAULT_ROWS,
        }
    }
}

/// Most cursors open at once per call (slots are recycled when a cursor is
/// exhausted). Each open cursor pins one engine reader slot, so this stays
/// far below any sane `max_readers`.
pub const MAX_CURSORS: usize = 16;

/// Runtime value: a scalar [`Value`], a proc-runtime-only container, or an
/// opaque cursor handle. A handle names a slot in the interpreter's cursor
/// table plus the generation it was opened under — a handle outliving its
/// cursor (the slot was closed and possibly recycled) is detected and
/// rejected, never aliased.
#[derive(Debug, Clone, PartialEq)]
pub enum PValue {
    Scalar(Value),
    List(Rc<Vec<PValue>>),
    Tuple(Rc<Vec<PValue>>),
    Cursor { slot: u16, gen: u32 },
}

/// What `call` hands back: the scalar, or (for procs that return query
/// results) a list/tuple tree. Containers exist only on the way *out* —
/// they cannot be passed back in as arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcValue {
    Scalar(Value),
    List(Vec<ProcValue>),
    Tuple(Vec<ProcValue>),
}

impl std::fmt::Display for ProcValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn seq(f: &mut std::fmt::Formatter<'_>, items: &[ProcValue]) -> std::fmt::Result {
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}")?;
            }
            Ok(())
        }
        match self {
            ProcValue::Scalar(v) => write!(f, "{v}"),
            ProcValue::List(items) => {
                f.write_str("[")?;
                seq(f, items)?;
                f.write_str("]")
            }
            ProcValue::Tuple(items) => {
                f.write_str("(")?;
                seq(f, items)?;
                f.write_str(")")
            }
        }
    }
}

/// Convert an interpreter value into a caller-visible one. Fallible only
/// for cursors: a handle is meaningless outside the call that opened it
/// (its snapshot dies with the call), so returning one is an error.
fn to_proc_value(v: PValue) -> Result<ProcValue> {
    Ok(match v {
        PValue::Scalar(s) => ProcValue::Scalar(s),
        PValue::List(items) => ProcValue::List(
            items
                .iter()
                .cloned()
                .map(to_proc_value)
                .collect::<Result<_>>()?,
        ),
        PValue::Tuple(items) => ProcValue::Tuple(
            items
                .iter()
                .cloned()
                .map(to_proc_value)
                .collect::<Result<_>>()?,
        ),
        PValue::Cursor { .. } => {
            return Err(type_err(
                "a cursor cannot be returned from a procedure; \
                 return the values you read from it",
            ))
        }
    })
}

/// How the interpreter reaches the database. The write path binds this to a
/// `WriteSession` (one transaction for the whole call), the read path to
/// lock-free `Database::execute` snapshots. The interpreter itself neither
/// knows nor cares — it only ever presents plan *hashes*.
///
/// The cursor trio backs `CursorOpen`/`CursorAdvance`/`CursorFetch`/
/// `CursorClose`. `cursor_open` returns a bridge-side stream id.
/// `cursor_advance` pulls ONE row (O(1) bridge memory) and leaves the
/// stream allocated when it returns `None`. `cursor_close` drops that
/// stream without reading. Python `CursorAdvance` closes on exhaustion;
/// Oracle `CursorFetch` does not, so CLOSE can still succeed. Only the
/// read path implements them for real — IR validation guarantees a proc
/// with `DbExec` never contains `CursorOpen` (v1 rule), so the write
/// bridge's implementations are unreachable-by-construction guards.
pub trait DbBridge {
    fn query(&mut self, plan: &PlanRef, params: &[Value]) -> Result<Vec<Vec<Value>>>;
    fn exec(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u64>;
    /// `DbReturning`: the RETURNING rows of one DML statement. The default
    /// refuses, so a bridge that only counts affected rows cannot drop the cells.
    fn exec_returning(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<Vec<Vec<Value>>> {
        Err(Error::Unsupported(
            "RETURNING binds are not available on this bridge".into(),
        ))
    }
    fn cursor_open(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u32>;
    fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>>;
    fn cursor_close(&mut self, stream: u32) -> Result<()>;
}

fn rt_err(msg: impl Into<String>) -> Error {
    Error::Unsupported(format!("proc runtime: {}", msg.into()))
}

fn type_err(msg: impl Into<String>) -> Error {
    Error::TypeMismatch(format!("proc runtime: {}", msg.into()))
}

pub fn budget_instr_err(limit: u64) -> Error {
    Error::Unsupported(format!(
        "proc budget: instruction budget exhausted (limit {limit}); \
         the procedure was aborted and any writes rolled back"
    ))
}

pub fn budget_db_err(limit: u64) -> Error {
    Error::Unsupported(format!(
        "proc budget: db-call budget exhausted (limit {limit}); \
         the procedure was aborted and any writes rolled back"
    ))
}

pub fn budget_rows_err(limit: u64) -> Error {
    Error::Unsupported(format!(
        "proc budget: cursor row budget exhausted (limit {limit}); \
         the procedure was aborted"
    ))
}

fn tyname(v: &PValue) -> &'static str {
    match v {
        PValue::Scalar(s) => s.type_name(),
        PValue::List(_) => "list",
        PValue::Tuple(_) => "tuple",
        PValue::Cursor { .. } => "cursor",
    }
}

/// Python-style truthiness. `None`/0/0.0/""/empty containers are falsey;
/// a cursor handle, like any opaque object, is truthy.
fn truthy(v: &PValue) -> bool {
    match v {
        PValue::Scalar(Value::Null) => false,
        PValue::Scalar(Value::Bool(b)) => *b,
        PValue::Scalar(Value::Int(i)) => *i != 0,
        PValue::Scalar(Value::Float(f)) => *f != 0.0,
        PValue::Scalar(Value::Text(s)) => !s.is_empty(),
        PValue::Scalar(Value::Blob(b)) => !b.is_empty(),
        PValue::Scalar(Value::Timestamp(_)) | PValue::Scalar(Value::Date(_)) => true,
        // A `time` is micros since midnight, so midnight itself is 0 — and
        // Python's `datetime.time(0, 0)` has been truthy since 3.5. The
        // integer rule would make it falsey; the type rule is the right one.
        PValue::Scalar(Value::Time(_)) => true,
        // `Decimal("0")` is falsey in Python, and every spelling of zero
        // canonicalises to `0`, so the string test IS the value test.
        PValue::Scalar(Value::Numeric(n)) => n != "0",
        // A session-context list (§2.6) is param-only, but a proc can hold one
        // it was handed. Treat it like every other container here: empty is
        // falsey.
        PValue::Scalar(Value::List(v)) => !v.is_empty(),
        PValue::List(v) => !v.is_empty(),
        PValue::Tuple(v) => !v.is_empty(),
        PValue::Cursor { .. } => true,
    }
}

/// Python-style equality: numeric int/float cross-compare, `None` equals
/// only `None`, mismatched types are unequal (not an error), containers
/// compare element-wise. Divergence: bools never equal ints. Cursor handles
/// compare by identity (slot + generation).
fn eq(a: &PValue, b: &PValue) -> bool {
    use PValue::*;
    use Value::*;
    match (a, b) {
        (Scalar(x), Scalar(y)) => match (x, y) {
            (Int(i), Float(f)) | (Float(f), Int(i)) => (*i as f64) == *f,
            (x, y) => x == y, // Value::PartialEq is same-variant only
        },
        (PValue::List(x), PValue::List(y)) | (Tuple(x), Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| eq(a, b))
        }
        (Cursor { slot: s1, gen: g1 }, Cursor { slot: s2, gen: g2 }) => {
            s1 == s2 && g1 == g2
        }
        _ => false,
    }
}

/// Python-style ordering: numeric cross-compare allowed; text, blob, bool
/// and timestamp compare within their own type; anything else errors (like
/// Python's `TypeError: '<' not supported`).
fn cmp(a: &PValue, b: &PValue) -> Result<std::cmp::Ordering> {
    use Value::*;
    let (x, y) = match (a, b) {
        (PValue::Scalar(x), PValue::Scalar(y)) => (x, y),
        _ => {
            return Err(type_err(format!(
                "ordering not supported between {} and {}",
                tyname(a),
                tyname(b)
            )))
        }
    };
    let ord = match (x, y) {
        (Int(p), Int(q)) => p.cmp(q),
        (Float(p), Float(q)) => p
            .partial_cmp(q)
            .ok_or_else(|| rt_err("NaN is not orderable"))?,
        (Int(p), Float(q)) => (*p as f64)
            .partial_cmp(q)
            .ok_or_else(|| rt_err("NaN is not orderable"))?,
        (Float(p), Int(q)) => p
            .partial_cmp(&(*q as f64))
            .ok_or_else(|| rt_err("NaN is not orderable"))?,
        (Bool(p), Bool(q)) => p.cmp(q),
        (Text(p), Text(q)) => p.cmp(q),
        (Blob(p), Blob(q)) => p.cmp(q),
        (Timestamp(p), Timestamp(q)) => p.cmp(q),
        _ => {
            return Err(type_err(format!(
                "ordering not supported between {} and {}",
                x.type_name(),
                y.type_name()
            )))
        }
    };
    Ok(ord)
}

/// Binary arithmetic. Ints are checked (overflow errors), int/float mixes
/// promote to float (Python-style; the Rust frontend is dynamically typed
/// at runtime too — documented divergence), `+` concatenates text.
fn arith(op: Op, a: PValue, b: PValue) -> Result<PValue> {
    use Value::*;
    let (x, y) = match (a, b) {
        (PValue::Scalar(x), PValue::Scalar(y)) => (x, y),
        (a, b) => {
            return Err(type_err(format!(
                "arithmetic not supported between {} and {}",
                tyname(&a),
                tyname(&b)
            )))
        }
    };
    if let (Text(p), Text(q), Op::Add) = (&x, &y, op) {
        let mut s = String::with_capacity(p.len() + q.len());
        s.push_str(p);
        s.push_str(q);
        return Ok(PValue::Scalar(Text(s)));
    }
    let scalar = |v: Value| Ok(PValue::Scalar(v));
    match (&x, &y) {
        (Int(p), Int(q)) => {
            let (p, q) = (*p, *q);
            let ovf = || Error::ArithmeticOverflow;
            match op {
                Op::Add => scalar(Int(p.checked_add(q).ok_or_else(ovf)?)),
                Op::Sub => scalar(Int(p.checked_sub(q).ok_or_else(ovf)?)),
                Op::Mul => scalar(Int(p.checked_mul(q).ok_or_else(ovf)?)),
                // Python `/` on ints yields a float.
                Op::TrueDiv => {
                    if q == 0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Float(p as f64 / q as f64))
                }
                // Python `//`: floor division.
                Op::FloorDiv => {
                    if q == 0 {
                        return Err(Error::DivisionByZero);
                    }
                    let d = p.checked_div(q).ok_or_else(ovf)?;
                    let adj = (p % q != 0) && ((p < 0) != (q < 0));
                    scalar(Int(if adj { d - 1 } else { d }))
                }
                // Rust `/`: truncation toward zero.
                Op::IntDiv => {
                    if q == 0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Int(p.checked_div(q).ok_or_else(ovf)?))
                }
                // Python `%`: sign of the divisor.
                Op::PyMod => {
                    if q == 0 {
                        return Err(Error::DivisionByZero);
                    }
                    let r = p.checked_rem(q).ok_or_else(ovf)?;
                    let adj = r != 0 && ((r < 0) != (q < 0));
                    scalar(Int(if adj { r + q } else { r }))
                }
                // Rust `%`: sign of the dividend.
                Op::IntRem => {
                    if q == 0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Int(p.checked_rem(q).ok_or_else(ovf)?))
                }
                _ => unreachable!("arith called with non-arith op"),
            }
        }
        (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
            let p = match &x {
                Int(i) => *i as f64,
                Float(f) => *f,
                _ => unreachable!(),
            };
            let q = match &y {
                Int(i) => *i as f64,
                Float(f) => *f,
                _ => unreachable!(),
            };
            match op {
                Op::Add => scalar(Float(p + q)),
                Op::Sub => scalar(Float(p - q)),
                Op::Mul => scalar(Float(p * q)),
                // Python raises ZeroDivisionError on float / 0.0 too.
                Op::TrueDiv => {
                    if q == 0.0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Float(p / q))
                }
                Op::FloorDiv => {
                    if q == 0.0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Float((p / q).floor()))
                }
                Op::PyMod => {
                    if q == 0.0 {
                        return Err(Error::DivisionByZero);
                    }
                    scalar(Float(p - (p / q).floor() * q))
                }
                // Rust float semantics: IEEE, no zero-divisor error.
                Op::IntDiv => scalar(Float(p / q)),
                Op::IntRem => scalar(Float(p % q)),
                _ => unreachable!("arith called with non-arith op"),
            }
        }
        _ => Err(type_err(format!(
            "arithmetic not supported between {} and {}",
            x.type_name(),
            y.type_name()
        ))),
    }
}

/// One interpreter-side cursor slot: the bridge's stream id, the current
/// row (set by a pull, read by `CursorRow`), and the generation that must
/// match the handle. Python exhaustion and Oracle CLOSE free the slot and
/// bump the generation so stale handles error instead of aliasing. Oracle
/// FETCH that finds no row sets `exhausted` and keeps the stream, so the
/// handle stays open and a later OPEN cannot reuse the slot.
/// `fetched` separates "OPEN, no FETCH yet" (`%FOUND` is NULL) from a
/// FETCH that returned no row (`%FOUND` is false). `row == None` is both.
struct CurSlot {
    gen: u32,
    stream: Option<u32>,
    row: Option<PValue>,
    exhausted: bool,
    fetched: bool,
    rowcount: u64,
}

/// Run a validated procedure. `args` were checked against `proc.argc` by
/// the caller; locals beyond the parameters start *unassigned* (reading one
/// before a store errors, mirroring Python's UnboundLocalError).
pub fn run(
    proc: &Proc,
    args: &[Value],
    bridge: &mut dyn DbBridge,
    budget: Budget,
) -> Result<ProcValue> {
    let mut package = HashMap::new();
    run_with_package(proc, args, bridge, budget, &mut package)
}

/// Like [`run`], but package variables live in `package` across calls.
/// A missing key is NULL. `PkgSet` writes immediately, including NULL, and
/// a later error does not remove that write.
pub fn run_with_package(
    proc: &Proc,
    args: &[Value],
    bridge: &mut dyn DbBridge,
    budget: Budget,
    package: &mut HashMap<String, Value>,
) -> Result<ProcValue> {
    debug_assert_eq!(args.len(), proc.argc as usize);
    // Option<PValue>: None = never assigned (UnboundLocalError analogue).
    let mut locals: Vec<Option<PValue>> = Vec::with_capacity(proc.nlocals as usize);
    locals.extend(args.iter().map(|v| Some(PValue::Scalar(v.clone()))));
    locals.resize(proc.nlocals as usize, None);
    let mut stack: Vec<PValue> = Vec::with_capacity(proc.max_stack());
    let mut dbargs: Vec<Value> = Vec::new();
    // Grown on demand, bounded by MAX_CURSORS.
    let mut cursors: Vec<CurSlot> = Vec::new();
    // Implicit SQL% cursor. None until a SELECT INTO or DML completes.
    // Explicit OPEN/FETCH/CLOSE do not write it, and a caught error leaves
    // it in place so the handler can read the statement that raised.
    // Ordinary DML does not write `bulk`. A FORALL publishes through FINISH.
    let mut implicit_sql: Option<(bool, i64)> = None;
    let mut bulk = BulkCursor::new();
    let mut instrs_left = budget.instrs;
    let mut db_left = budget.db_calls;
    let mut rows_left = budget.rows;
    let mut pc = 0usize;
    let mut frames: Vec<oracle_exception::Frame> = Vec::new();
    let mut caught: Vec<oracle_exception::Caught> = Vec::new();
    loop {
        while frames.last().is_some_and(|f| pc < f.start || pc >= f.end) { frames.pop(); }
        while caught.last().is_some_and(|c| pc < c.start || pc >= c.end) { caught.pop(); }
        if instrs_left == 0 {
            return Err(budget_instr_err(budget.instrs));
        }
        instrs_left -= 1;
        // pc is always in range: validation proved every reachable
        // successor exists and every terminal path ends in Return.
        let op = proc.instrs[pc];
        pc += 1;
        let step = (|| -> Result<Option<ProcValue>> {
        match op {
            Op::OracleTry { handler, end, handler_end } => {
                if frames.len() == oracle_exception::MAX_SCOPES { return Err(rt_err("Oracle exception nesting budget exceeded")); }
                frames.push(oracle_exception::Frame { start: pc, end: end as usize, handler: handler as usize, handler_end: handler_end as usize });
            }
            Op::OracleExceptionCode => {
                let code = match caught.last().map(|c| &c.error) {
                    Some(Error::Oracle { code, .. }) => *code,
                    Some(Error::OracleUserException { .. }) => 0,
                    _ => return Err(Error::Corrupt("proc: no active Oracle exception".into())),
                };
                stack.push(PValue::Scalar(Value::Int(code as i64)));
            }
            Op::OracleReraise => {
                return Err(caught.pop().ok_or_else(|| rt_err("RAISE has no active exception"))?.error);
            }
            Op::OracleNotNull => {
                match stack.last().expect("validated") {
                    PValue::Scalar(Value::Null) => return Err(Error::Oracle {code:6502,message:"PL/SQL: value or conversion error".into()}),
                    PValue::Scalar(_) => {},
                    _ => return Err(type_err("NOT NULL requires a scalar value")),
                }
            }
            Op::OracleRaiseUser(i) => return Err(Error::OracleUserException { identity: oracle_exception::user_identity(proc, i) }),
            Op::OracleMatchUser(i) => {
                let error = &caught.last().ok_or_else(|| Error::Corrupt("proc: no active Oracle exception".into()))?.error;
                let matches = matches!(error, Error::OracleUserException { identity } if *identity == oracle_exception::user_identity(proc, i));
                stack.push(PValue::Scalar(Value::Bool(matches)));
            }
            Op::OracleRaiseBound(code) => return Err(oracle_exception::bound(code)),
            Op::OracleRaise(code) => return Err(oracle_exception::named(code)),
            Op::OracleSqlCode => stack.push(PValue::Scalar(Value::Numeric(oracle_exception::sqlcode(&caught)?.to_string()))),
            Op::OracleSqlErrm => stack.push(PValue::Scalar(Value::Text(oracle_exception::sqlerrm(&caught)?))),
            Op::OracleSqlErrmCode => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_exception::explicit(value, &caught)?));
            }
            Op::OracleRaiseApplication => {
                let message = stack.pop().expect("validated");
                let code = stack.pop().expect("validated");
                return Err(oracle_exception::application(code, message)?);
            }
            Op::LoadConst(i) => stack.push(PValue::Scalar(proc.consts[i as usize].clone())),
            Op::LoadLocal(i) => match &locals[i as usize] {
                Some(v) => stack.push(v.clone()),
                None => {
                    return Err(rt_err(format!(
                        "local variable #{i} used before assignment"
                    )))
                }
            },
            Op::StoreLocal(i) => {
                locals[i as usize] = Some(stack.pop().expect("validated"));
            }
            Op::Pop => {
                stack.pop().expect("validated");
            }
            Op::Dup => {
                let top = stack.last().expect("validated").clone();
                stack.push(top);
            }
            Op::Neg => {
                let v = stack.pop().expect("validated");
                stack.push(match v {
                    PValue::Scalar(Value::Int(i)) => PValue::Scalar(Value::Int(
                        i.checked_neg().ok_or(Error::ArithmeticOverflow)?,
                    )),
                    PValue::Scalar(Value::Float(f)) => PValue::Scalar(Value::Float(-f)),
                    v => return Err(type_err(format!("cannot negate {}", tyname(&v)))),
                });
            }
            Op::Not => {
                let v = stack.pop().expect("validated");
                stack.push(PValue::Scalar(Value::Bool(!truthy(&v))));
            }
            Op::Tuple(count) => {
                let base = stack.len() - count as usize;
                let mut items = Vec::with_capacity(count as usize);
                for item in stack.drain(base..) {
                    // A nested tuple constructor plus DUP could otherwise
                    // create exponentially expanding return values in O(n)
                    // instructions. OUT values are scalar; forbid containers.
                    if !matches!(item, PValue::Scalar(_)) {
                        return Err(type_err("procedure tuple elements must be scalar"));
                    }
                    items.push(item);
                }
                stack.push(PValue::Tuple(Rc::new(items)));
            }
            Op::MissingReturn => return Err(rt_err("PL/SQL function reached END without RETURN")),
            Op::OracleSelectOne => {
                let PValue::List(rows) = stack.pop().expect("validated") else {
                    return Err(type_err("PL/SQL INTO requires a query row set"));
                };
                match rows.len() {
                    0 => {
                        implicit_sql = Some((false, 0));
                        return Err(Error::Oracle { code: 1403, message: "no data found".into() });
                    }
                    1 => {
                        if !matches!(&rows[0], PValue::Tuple(_)) {
                            return Err(type_err("PL/SQL INTO requires a query row tuple"));
                        }
                        implicit_sql = Some((true, 1));
                        stack.push(rows[0].clone());
                    }
                    _ => {
                        // A scalar INTO stops once a second row proves too many.
                        implicit_sql = Some((true, 1));
                        return Err(Error::Oracle { code: 1422, message: "exact fetch returns more than requested number of rows".into() });
                    }
                }
            }
            Op::OracleNumberConstraint { precision, scale } => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_constraint::number(value, precision, scale)?));
            }
            Op::OracleVarchar(limit) => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_text::varchar(value, limit)?));
            }
            Op::OracleConcat | Op::OracleCharEq | Op::OracleCharNe | Op::OracleCharLt | Op::OracleCharLe | Op::OracleCharGt | Op::OracleCharGe => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_text::binary(op, a, b)?));
            }
            Op::OracleInteger | Op::OracleIntNeg => {
                let value = stack.pop().expect("validated");
                let value = if op == Op::OracleIntNeg { oracle_number::integer_negate(value)? } else { oracle_number::integer(value)? };
                stack.push(PValue::Scalar(value));
            }
            Op::OracleIntAdd | Op::OracleIntSub | Op::OracleIntMul => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_number::integer_binary(op, a, b)?));
            }
            Op::OracleLoopBound => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_number::loop_bound(value)?));
            }
            Op::OraclePlSqlNumber | Op::OraclePlSqlNeg | Op::OraclePlSqlInteger | Op::OraclePlSqlLoopBound => {
                let value = stack.pop().expect("validated");
                let value = match op {
                    Op::OraclePlSqlNumber => plsql_number::number(value)?,
                    Op::OraclePlSqlNeg => plsql_number::negate(value)?,
                    Op::OraclePlSqlInteger => plsql_number::integer(value, false)?,
                    Op::OraclePlSqlLoopBound => plsql_number::integer(value, true)?,
                    _ => unreachable!(),
                };
                stack.push(PValue::Scalar(value));
            }
            Op::OraclePlSqlAdd | Op::OraclePlSqlSub | Op::OraclePlSqlMul | Op::OraclePlSqlDivide => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(plsql_number::binary(op, a, b)?));
            }
            Op::OracleNumber => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_number::number(value)?));
            }
            Op::OracleNeg => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_number::negate(value)?));
            }
            Op::OracleAdd | Op::OracleSub | Op::OracleMul | Op::OracleDiv | Op::OraclePlSqlDiv => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(oracle_number::binary(op, a, b)?));
            }
            Op::SqlNeg => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(legacy_oracle_number::negate(value)?));
            }
            Op::SqlAdd | Op::SqlSub | Op::SqlMul | Op::SqlDiv => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(legacy_oracle_number::binary(op, a, b)?));
            }
            Op::SqlNumber => {
                let value = stack.pop().expect("validated");
                stack.push(PValue::Scalar(sql_logic::number(value)?));
            }
            Op::SqlNot | Op::SqlBool => {
                let value = sql_logic::boolean(stack.pop().expect("validated"))?;
                let value = if op == Op::SqlNot { value.map(|v| !v) } else { value };
                stack.push(PValue::Scalar(value.map_or(Value::Null, Value::Bool)));
            }
            Op::SqlAnd | Op::SqlOr | Op::SqlEq | Op::SqlNe
            | Op::SqlLt | Op::SqlLe | Op::SqlGt | Op::SqlGe => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(PValue::Scalar(sql_logic::binary(op, a, b)?));
            }
            Op::JumpIfSqlFalse(target) | Op::JumpIfSqlTrue(target) => {
                let value = sql_logic::boolean(stack.pop().expect("validated"))?;
                let wanted = matches!(op, Op::JumpIfSqlTrue(_));
                if value == Some(wanted) {
                    pc = target as usize;
                    return Ok(None);
                }
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::TrueDiv
            | Op::FloorDiv
            | Op::IntDiv
            | Op::PyMod
            | Op::IntRem => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                stack.push(arith(op, a, b)?);
            }
            Op::Eq | Op::Ne => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                let e = eq(&a, &b);
                stack.push(PValue::Scalar(Value::Bool(if op == Op::Eq { e } else { !e })));
            }
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let b = stack.pop().expect("validated");
                let a = stack.pop().expect("validated");
                let ord = cmp(&a, &b)?;
                use std::cmp::Ordering::*;
                let r = match op {
                    Op::Lt => ord == Less,
                    Op::Le => ord != Greater,
                    Op::Gt => ord == Greater,
                    _ => ord != Less,
                };
                stack.push(PValue::Scalar(Value::Bool(r)));
            }
            Op::Len => {
                let v = stack.pop().expect("validated");
                let n = match &v {
                    PValue::List(items) => items.len(),
                    PValue::Tuple(items) => items.len(),
                    // Python len(str) counts code points, not bytes.
                    PValue::Scalar(Value::Text(s)) => s.chars().count(),
                    PValue::Scalar(Value::Blob(b)) => b.len(),
                    v => return Err(type_err(format!("{} has no len()", tyname(v)))),
                };
                stack.push(PValue::Scalar(Value::Int(n as i64)));
            }
            Op::Index => {
                let idx = stack.pop().expect("validated");
                let cont = stack.pop().expect("validated");
                let i = match idx {
                    PValue::Scalar(Value::Int(i)) => i,
                    v => {
                        return Err(type_err(format!(
                            "indices must be integers, not {}",
                            tyname(&v)
                        )))
                    }
                };
                let items = match &cont {
                    PValue::List(items) | PValue::Tuple(items) => items,
                    v => {
                        return Err(type_err(format!(
                            "{} is not indexable (only query results are)",
                            tyname(v)
                        )))
                    }
                };
                // Python-style: negative indices wrap once.
                let n = items.len() as i64;
                let eff = if i < 0 { i + n } else { i };
                if eff < 0 || eff >= n {
                    return Err(rt_err(format!(
                        "index {i} out of range for length {n}"
                    )));
                }
                stack.push(items[eff as usize].clone());
            }
            Op::Jump(t) => pc = t as usize,
            Op::JumpIfFalse(t) => {
                let v = stack.pop().expect("validated");
                if !truthy(&v) {
                    pc = t as usize;
                }
            }
            Op::JumpIfTrue(t) => {
                let v = stack.pop().expect("validated");
                if truthy(&v) {
                    pc = t as usize;
                }
            }
            Op::DbQuery(p) | Op::DbExec(p) | Op::DbReturning(p) | Op::CursorOpen(p) => {
                if db_left == 0 {
                    return Err(budget_db_err(budget.db_calls));
                }
                db_left -= 1;
                let plan = &proc.plans[p as usize];
                let argc = plan.argc as usize;
                dbargs.clear();
                dbargs.reserve(argc);
                // Args were pushed left-to-right; drain preserving order.
                let base = stack.len() - argc; // >= 0: validated
                for v in stack.drain(base..) {
                    match v {
                        PValue::Scalar(s) => dbargs.push(s),
                        v => {
                            return Err(type_err(format!(
                                "only scalar values can cross the database boundary, got {}",
                                tyname(&v)
                            )))
                        }
                    }
                }
                match op {
                    Op::DbQuery(_) => {
                        let rows = bridge.query(plan, &dbargs)?;
                        let list: Vec<PValue> = rows
                            .into_iter()
                            .map(|row| {
                                PValue::Tuple(Rc::new(
                                    row.into_iter().map(PValue::Scalar).collect(),
                                ))
                            })
                            .collect();
                        stack.push(PValue::List(Rc::new(list)));
                    }
                    Op::DbExec(_) => {
                        let n = bridge.exec(plan, &dbargs)?;
                        let n = i64::try_from(n)
                            .map_err(|_| rt_err("affected-row count exceeds i64"))?;
                        implicit_sql = Some((n > 0, n));
                        stack.push(PValue::Scalar(Value::Int(n)));
                    }
                    Op::DbReturning(_) => {
                        // The production bridge raises 1403/1422 before this
                        // loop sees the rows. OracleSelectOne covers the mock,
                        // which returns the row list instead.
                        let rows = match bridge.exec_returning(plan, &dbargs) {
                            Ok(rows) => rows,
                            Err(error @ Error::Oracle { code: 1403, .. }) => {
                                implicit_sql = Some((false, 0));
                                return Err(error);
                            }
                            Err(error @ Error::Oracle { code: 1422, .. }) => {
                                implicit_sql = Some((true, 1));
                                return Err(error);
                            }
                            Err(error) => return Err(error),
                        };
                        let list: Vec<PValue> = rows
                            .into_iter()
                            .map(|row| {
                                PValue::Tuple(Rc::new(
                                    row.into_iter().map(PValue::Scalar).collect(),
                                ))
                            })
                            .collect();
                        stack.push(PValue::List(Rc::new(list)));
                    }
                    _ => {
                        // CursorOpen: claim a free slot (bounded), open the
                        // stream, hand out a slot+generation handle.
                        let slot = match cursors
                            .iter()
                            .position(|c| c.stream.is_none() && !c.exhausted)
                        {
                            Some(i) => i,
                            None if cursors.len() < MAX_CURSORS => {
                                cursors.push(CurSlot {
                                    gen: 0,
                                    stream: None,
                                    row: None,
                                    exhausted: false,
                                    fetched: false,
                                    rowcount: 0,
                                });
                                cursors.len() - 1
                            }
                            None => {
                                return Err(rt_err(format!(
                                    "too many open cursors (max {MAX_CURSORS}); \
                                     iterate one to exhaustion before opening more"
                                )))
                            }
                        };
                        let stream = bridge.cursor_open(plan, &dbargs)?;
                        let c = &mut cursors[slot];
                        c.stream = Some(stream);
                        c.row = None;
                        c.exhausted = false;
                        c.fetched = false;
                        c.rowcount = 0;
                        stack.push(PValue::Cursor {
                            slot: slot as u16,
                            gen: c.gen,
                        });
                    }
                }
            }
            Op::CursorAdvance | Op::CursorFetch => {
                let fetch = matches!(op, Op::CursorFetch);
                let slot = pop_cursor(&mut stack, &cursors)?;
                if fetch && cursors[slot].exhausted {
                    stack.push(PValue::Scalar(Value::Bool(false)));
                } else {
                    if rows_left == 0 {
                        return Err(budget_rows_err(budget.rows));
                    }
                    rows_left -= 1;
                    let stream = cursors[slot].stream.expect("pop_cursor checked");
                    match bridge.cursor_advance(stream)? {
                        Some(row) => {
                            let c = &mut cursors[slot];
                            c.row = Some(PValue::Tuple(Rc::new(
                                row.into_iter().map(PValue::Scalar).collect(),
                            )));
                            c.fetched = true;
                            c.rowcount = c.rowcount.saturating_add(1);
                            stack.push(PValue::Scalar(Value::Bool(true)));
                        }
                        None if fetch => {
                            // Oracle not-found: the cursor stays open. CLOSE
                            // is what releases the stream. ROWCOUNT does not
                            // move, and FOUND becomes false rather than NULL.
                            let c = &mut cursors[slot];
                            c.row = None;
                            c.exhausted = true;
                            c.fetched = true;
                            stack.push(PValue::Scalar(Value::Bool(false)));
                        }
                        None => {
                            bridge.cursor_close(stream)?;
                            release_cursor(&mut cursors[slot]);
                            stack.push(PValue::Scalar(Value::Bool(false)));
                        }
                    }
                }
            }
            Op::CursorClose => {
                let slot = pop_cursor(&mut stack, &cursors)?;
                let stream = cursors[slot].stream.expect("pop_cursor checked");
                bridge.cursor_close(stream)?;
                release_cursor(&mut cursors[slot]);
            }
            Op::CursorAttr(which) => {
                let value = stack.pop().expect("validated");
                stack.push(cursor_attr(&cursors, value, which)?);
            }
            Op::SqlAttr(which) => {
                stack.push(implicit_sql_attr(implicit_sql, which)?);
            }
            Op::SqlBulk(mode) => {
                bulk.step(mode, &mut stack, &mut implicit_sql)?;
            }
            Op::BulkCollect => {
                let rows = stack.pop().expect("validated");
                stack.push(bulk_collect_column(&mut implicit_sql, rows)?);
            }
            Op::NestedIndex => {
                let collection = stack.pop().expect("validated");
                let index = stack.pop().expect("validated");
                stack.push(nested_index(collection, index)?);
            }
            Op::PkgGet(index) => {
                let key = package_key(&proc.consts, index)?;
                let value = package.get(key).cloned().unwrap_or(Value::Null);
                stack.push(PValue::Scalar(value));
            }
            Op::PkgSet(index) => {
                let key = package_key(&proc.consts, index)?;
                let value = match stack.pop().expect("validated") {
                    PValue::Scalar(value) => value,
                    _ => return Err(type_err("a package variable requires a scalar value")),
                };
                package.insert(key.to_string(), value);
            }
            Op::CursorRow => {
                let slot = pop_cursor(&mut stack, &cursors)?;
                match &cursors[slot].row {
                    Some(row) => stack.push(row.clone()),
                    None => {
                        return Err(rt_err(
                            "cursor has no current row (advance it first)",
                        ))
                    }
                }
            }
            Op::Return => {
                let v = stack.pop().expect("validated");
                return to_proc_value(v).map(Some);
            }
        }
        Ok(None)
        })();
        match step {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {},
            Err(error) => {
                let Some(frame) = frames.pop() else { return Err(oracle_exception::entry_boundary(error)); };
                let error = oracle_exception::semantic(error, op)?;
                stack.clear(); // validator requires an empty stack at Try entry
                pc = frame.handler;
                caught.retain(|c| pc >= c.start && pc < c.end);
                if caught.len() == oracle_exception::MAX_SCOPES { return Err(rt_err("Oracle exception context budget exceeded")); }
                caught.push(oracle_exception::Caught { start: frame.handler, end: frame.handler_end, error });
            }
        }
    }
}

fn release_cursor(slot: &mut CurSlot) {
    slot.stream = None;
    slot.row = None;
    slot.exhausted = false;
    slot.fetched = false;
    slot.rowcount = 0;
    slot.gen = slot.gen.wrapping_add(1);
}

/// Row counts for the most recent dense `FORALL`.
///
/// `BEGIN` runs after the bounds are evaluated and replaces the map, so a
/// bounds error leaves the previous counts. Each recorded iteration stores
/// the `DbExec` count, and zero is a real count. A subscript this `FORALL`
/// did not execute is NULL; it does not raise `NO_DATA_FOUND`. `FINISH`
/// sets `SQL%ROWCOUNT` to the sum and `SQL%FOUND` from the last iteration,
/// and only when at least one iteration ran. Ordinary DML does not write
/// this map.
///
/// An exception in a later iteration does not roll back earlier iterations
/// of that `FORALL`. Oracle does that without `SAVE EXCEPTIONS`. This
/// runtime stops and keeps the counts already recorded.
struct BulkCursor {
    map: Option<HashMap<i64, i64>>,
    sum: i64,
    iters: u64,
    last_found: bool,
}

impl BulkCursor {
    fn new() -> Self {
        Self {
            map: None,
            sum: 0,
            iters: 0,
            last_found: false,
        }
    }

    fn step(
        &mut self,
        mode: u8,
        stack: &mut Vec<PValue>,
        implicit_sql: &mut Option<(bool, i64)>,
    ) -> Result<()> {
        match mode {
            crate::ir::SQL_BULK_BEGIN => {
                self.map = Some(HashMap::new());
                self.sum = 0;
                self.iters = 0;
                self.last_found = false;
            }
            crate::ir::SQL_BULK_RECORD => {
                let index = bulk_index(stack.pop().expect("validated"))?;
                let Some((found, count)) = *implicit_sql else {
                    return Err(rt_err("FORALL recorded no row count"));
                };
                self.map.get_or_insert_with(HashMap::new).insert(index, count);
                self.sum = self
                    .sum
                    .checked_add(count)
                    .ok_or_else(|| oracle_exception::named(6502))?;
                self.last_found = found;
                self.iters = self.iters.saturating_add(1);
            }
            crate::ir::SQL_BULK_FINISH => {
                if self.iters > 0 {
                    *implicit_sql = Some((self.last_found, self.sum));
                }
            }
            crate::ir::SQL_BULK_LOAD => {
                let index = bulk_index(stack.pop().expect("validated"))?;
                let value = self
                    .map
                    .as_ref()
                    .and_then(|map| map.get(&index).copied())
                    .map_or(Value::Null, Value::Int);
                stack.push(PValue::Scalar(value));
            }
            _ => {
                return Err(Error::Corrupt(
                    "proc: invalid SQL%BULK_ROWCOUNT operation".into(),
                ))
            }
        }
        Ok(())
    }
}

fn bulk_index(value: PValue) -> Result<i64> {
    match value {
        PValue::Scalar(Value::Int(index)) => Ok(index),
        // A NULL subscript is Oracle 6502, not a missing FORALL entry.
        _ => Err(oracle_exception::named(6502)),
    }
}

/// First column of a query row list. An empty result is an empty nested
/// table, not NULL, and `SQL%ROWCOUNT` is the fetched count.
fn package_key(consts: &[Value], index: u16) -> Result<&str> {
    match consts.get(index as usize) {
        Some(Value::Text(text)) if text.contains('.') => Ok(text.as_str()),
        _ => Err(Error::Corrupt("proc: package variable key is invalid".into())),
    }
}

fn bulk_collect_column(implicit_sql: &mut Option<(bool, i64)>, rows: PValue) -> Result<PValue> {
    let PValue::List(rows) = rows else {
        return Err(type_err("BULK COLLECT requires a query row set"));
    };
    let mut elems = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let PValue::Tuple(cols) = row else {
            return Err(type_err("BULK COLLECT requires a query row tuple"));
        };
        if cols.len() != 1 {
            return Err(type_err("BULK COLLECT requires one select item"));
        }
        elems.push(cols[0].clone());
    }
    let count = i64::try_from(elems.len()).map_err(|_| rt_err("BULK COLLECT row count exceeds i64"))?;
    *implicit_sql = Some((count > 0, count));
    Ok(PValue::List(Rc::new(elems)))
}

/// 1-based nested-table subscript. A NULL element already stored is returned
/// unchanged. The table itself being NULL is a different error.
fn nested_index(collection: PValue, index: PValue) -> Result<PValue> {
    let list = match collection {
        PValue::Scalar(Value::Null) => {
            return Err(Error::Oracle {
                code: 6531,
                message: "Reference to uninitialized collection".into(),
            })
        }
        PValue::List(list) => list,
        other => {
            return Err(type_err(format!(
                "expected a nested table, got {}",
                tyname(&other)
            )))
        }
    };
    let index = match index {
        PValue::Scalar(Value::Int(index)) => index,
        _ => return Err(oracle_exception::named(6502)),
    };
    if index < 1 || index as usize > list.len() {
        return Err(Error::Oracle {
            code: 6533,
            message: "Subscript beyond count".into(),
        });
    }
    Ok(list[(index as usize) - 1].clone())
}

/// Implicit `SQL%` attribute. `ISOPEN` is always false. The other three
/// are NULL until a SELECT INTO or DML has recorded `(found, rowcount)`.
fn implicit_sql_attr(state: Option<(bool, i64)>, which: u8) -> Result<PValue> {
    let value = match which {
        crate::ir::CURSOR_ATTR_ISOPEN => Value::Bool(false),
        crate::ir::CURSOR_ATTR_FOUND => state.map_or(Value::Null, |(found, _)| Value::Bool(found)),
        crate::ir::CURSOR_ATTR_NOTFOUND => {
            state.map_or(Value::Null, |(found, _)| Value::Bool(!found))
        }
        crate::ir::CURSOR_ATTR_ROWCOUNT => state.map_or(Value::Null, |(_, count)| Value::Int(count)),
        _ => return Err(Error::Corrupt("proc: invalid cursor attribute".into())),
    };
    Ok(PValue::Scalar(value))
}

/// Explicit cursor attribute. A NULL handle is a cursor that was never
/// opened or has been closed: `%ISOPEN` is false and the others raise
/// INVALID_CURSOR. Do not use [`pop_cursor`] here; that reports a host
/// error and rejects the NULL handle Oracle treats as "not open".
fn cursor_attr(cursors: &[CurSlot], value: PValue, which: u8) -> Result<PValue> {
    let open = match value {
        PValue::Scalar(Value::Null) => None,
        PValue::Cursor { slot, gen } => cursors
            .get(slot as usize)
            .filter(|cursor| cursor.gen == gen && cursor.stream.is_some()),
        other => {
            return Err(type_err(format!(
                "expected a cursor, got {}",
                tyname(&other)
            )))
        }
    };
    let Some(cursor) = open else {
        return if which == crate::ir::CURSOR_ATTR_ISOPEN {
            Ok(PValue::Scalar(Value::Bool(false)))
        } else {
            Err(Error::Oracle {
                code: 1001,
                message: "cursor number is invalid or does not exist".into(),
            })
        };
    };
    let found = if cursor.fetched {
        Some(cursor.row.is_some())
    } else {
        None
    };
    let value = match which {
        crate::ir::CURSOR_ATTR_ISOPEN => Value::Bool(true),
        crate::ir::CURSOR_ATTR_FOUND => found.map_or(Value::Null, Value::Bool),
        crate::ir::CURSOR_ATTR_NOTFOUND => found.map_or(Value::Null, |found| Value::Bool(!found)),
        crate::ir::CURSOR_ATTR_ROWCOUNT => Value::Int(
            i64::try_from(cursor.rowcount).map_err(|_| rt_err("cursor row count exceeds i64"))?,
        ),
        _ => return Err(Error::Corrupt("proc: invalid cursor attribute".into())),
    };
    Ok(PValue::Scalar(value))
}

/// Pop and resolve a cursor handle: must be a cursor value, its slot must
/// be live, and its generation must match (a mismatch means the cursor was
/// closed — and the slot possibly reopened for a different cursor).
fn pop_cursor(stack: &mut Vec<PValue>, cursors: &[CurSlot]) -> Result<usize> {
    let v = stack.pop().expect("validated");
    let PValue::Cursor { slot, gen } = v else {
        return Err(type_err(format!(
            "expected a cursor (from db.rows), got {}",
            tyname(&v)
        )));
    };
    match cursors.get(slot as usize) {
        Some(c) if c.gen == gen && c.stream.is_some() => Ok(slot as usize),
        _ => Err(rt_err("cursor is closed (it was iterated to exhaustion)")),
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::collections::VecDeque;

    /// Bridge for interpreter-only tests: no database, canned responses.
    /// Cursors stream the same canned `rows`, one row per advance.
    pub struct MockBridge {
        pub rows: Vec<Vec<Value>>,
        pub affected: u64,
        pub queries: usize,
        pub execs: usize,
        pub cursor_opens: usize,
        /// Parameters each `cursor_open` actually received, in call order.
        pub cursor_params: Vec<Vec<Value>>,
        /// Rows a `DbReturning` call returns. `None` means this mock was not
        /// given RETURNING rows; `Some` empty is a no-row RETURNING result.
        pub returning_rows: Option<Vec<Vec<Value>>>,
        /// Parameters each `exec_returning` received, in call order.
        pub exec_params: Vec<Vec<Value>>,
        /// Parameters each plain `exec` received, in call order.
        /// `exec_params` stays the RETURNING list so existing callers
        /// can tell a count-only DML from a RETURNING call.
        pub dml_params: Vec<Vec<Value>>,
        /// Rows `cursor_advance` actually pulled, including a not-found pull.
        pub advances: usize,
        /// Streams `cursor_close` released.
        pub closes: usize,
        streams: Vec<Option<VecDeque<Vec<Value>>>>,
    }

    impl MockBridge {
        #[allow(clippy::new_without_default)] // widened from pub(crate) by the M2 split
        pub fn new() -> MockBridge {
            MockBridge {
                rows: vec![],
                affected: 1,
                queries: 0,
                execs: 0,
                cursor_opens: 0,
                cursor_params: Vec::new(),
                returning_rows: None,
                exec_params: Vec::new(),
                dml_params: Vec::new(),
                advances: 0,
                closes: 0,
                streams: Vec::new(),
            }
        }

        /// Streams the bridge still holds. Python exhaustion and Oracle
        /// CLOSE both release theirs; a FETCH that found no row does not.
        pub fn live_streams(&self) -> usize {
            self.streams.iter().filter(|s| s.is_some()).count()
        }
    }

    impl DbBridge for MockBridge {
        fn query(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<Vec<Vec<Value>>> {
            self.queries += 1;
            Ok(self.rows.clone())
        }
        fn exec(&mut self, _plan: &PlanRef, params: &[Value]) -> Result<u64> {
            self.execs += 1;
            self.dml_params.push(params.to_vec());
            Ok(self.affected)
        }
        fn exec_returning(&mut self, _plan: &PlanRef, params: &[Value]) -> Result<Vec<Vec<Value>>> {
            self.execs += 1;
            self.exec_params.push(params.to_vec());
            self.returning_rows
                .clone()
                .ok_or_else(|| rt_err("mock: RETURNING rows were not provided"))
        }
        fn cursor_open(&mut self, _plan: &PlanRef, params: &[Value]) -> Result<u32> {
            self.cursor_opens += 1;
            self.cursor_params.push(params.to_vec());
            self.streams.push(Some(self.rows.clone().into()));
            Ok((self.streams.len() - 1) as u32)
        }
        fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>> {
            self.advances += 1;
            let slot = self
                .streams
                .get_mut(stream as usize)
                .ok_or_else(|| rt_err("mock: bad stream id"))?;
            let Some(q) = slot else {
                return Err(rt_err("mock: advance on a freed stream"));
            };
            Ok(q.pop_front())
        }
        fn cursor_close(&mut self, stream: u32) -> Result<()> {
            self.closes += 1;
            let slot = self
                .streams
                .get_mut(stream as usize)
                .ok_or_else(|| rt_err("mock: bad stream id"))?;
            if slot.is_none() {
                return Err(rt_err("mock: close on a freed stream"));
            }
            *slot = None;
            Ok(())
        }
    }
}
