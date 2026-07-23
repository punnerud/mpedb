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

use crate::ir::{Op, PlanRef, Proc};
use mpedb_types::{Error, Result, Value};
use std::rc::Rc;

/// Execution budgets. Every executed instruction costs one instruction unit
/// (backward jumps therefore pay on every loop iteration); every
/// `DbQuery`/`DbExec`/`CursorOpen` costs one db-call unit on top; every
/// `CursorAdvance` costs one **row** unit — its own dimension, deliberately
/// generous (10M): charging cursor rows against the 10k db-call budget
/// would burn it on a single medium scan, while leaving them uncharged
/// would let `SELECT`-all cursors bypass row accounting entirely.
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
/// The cursor pair backs `CursorOpen`/`CursorAdvance`: `cursor_open`
/// returns a bridge-side stream id, `cursor_advance` pulls ONE row (O(1)
/// bridge memory) and frees the stream when it returns `None`. Only the
/// read path implements them for real — IR validation guarantees a proc
/// with `DbExec` never contains `CursorOpen` (v1 rule), so the write
/// bridge's implementations are unreachable-by-construction guards.
pub trait DbBridge {
    fn query(&mut self, plan: &PlanRef, params: &[Value]) -> Result<Vec<Vec<Value>>>;
    fn exec(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u64>;
    fn cursor_open(&mut self, plan: &PlanRef, params: &[Value]) -> Result<u32>;
    fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>>;
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
        PValue::Scalar(Value::Timestamp(_)) => true,
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
/// row (set by `CursorAdvance`, read by `CursorRow`), and the generation
/// that must match the handle. Exhaustion frees the slot for reuse and
/// bumps the generation so stale handles error instead of aliasing.
struct CurSlot {
    gen: u32,
    stream: Option<u32>,
    row: Option<PValue>,
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
    debug_assert_eq!(args.len(), proc.argc as usize);
    // Option<PValue>: None = never assigned (UnboundLocalError analogue).
    let mut locals: Vec<Option<PValue>> = Vec::with_capacity(proc.nlocals as usize);
    locals.extend(args.iter().map(|v| Some(PValue::Scalar(v.clone()))));
    locals.resize(proc.nlocals as usize, None);
    let mut stack: Vec<PValue> = Vec::with_capacity(proc.max_stack());
    let mut dbargs: Vec<Value> = Vec::new();
    // Grown on demand, bounded by MAX_CURSORS.
    let mut cursors: Vec<CurSlot> = Vec::new();
    let mut instrs_left = budget.instrs;
    let mut db_left = budget.db_calls;
    let mut rows_left = budget.rows;
    let mut pc = 0usize;
    loop {
        if instrs_left == 0 {
            return Err(budget_instr_err(budget.instrs));
        }
        instrs_left -= 1;
        // pc is always in range: validation proved every reachable
        // successor exists and every terminal path ends in Return.
        let op = proc.instrs[pc];
        pc += 1;
        match op {
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
            Op::DbQuery(p) | Op::DbExec(p) | Op::CursorOpen(p) => {
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
                        stack.push(PValue::Scalar(Value::Int(n)));
                    }
                    _ => {
                        // CursorOpen: claim a free slot (bounded), open the
                        // stream, hand out a slot+generation handle.
                        let slot = match cursors.iter().position(|c| c.stream.is_none()) {
                            Some(i) => i,
                            None if cursors.len() < MAX_CURSORS => {
                                cursors.push(CurSlot {
                                    gen: 0,
                                    stream: None,
                                    row: None,
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
                        stack.push(PValue::Cursor {
                            slot: slot as u16,
                            gen: c.gen,
                        });
                    }
                }
            }
            Op::CursorAdvance => {
                let slot = pop_cursor(&mut stack, &cursors)?;
                if rows_left == 0 {
                    return Err(budget_rows_err(budget.rows));
                }
                rows_left -= 1;
                let stream = cursors[slot].stream.expect("pop_cursor checked");
                match bridge.cursor_advance(stream)? {
                    Some(row) => {
                        cursors[slot].row = Some(PValue::Tuple(Rc::new(
                            row.into_iter().map(PValue::Scalar).collect(),
                        )));
                        stack.push(PValue::Scalar(Value::Bool(true)));
                    }
                    None => {
                        // Exhausted: the bridge freed the stream; recycle
                        // the slot and invalidate outstanding handles.
                        let c = &mut cursors[slot];
                        c.stream = None;
                        c.row = None;
                        c.gen = c.gen.wrapping_add(1);
                        stack.push(PValue::Scalar(Value::Bool(false)));
                    }
                }
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
                return to_proc_value(v);
            }
        }
    }
}

/// Pop and resolve a cursor handle: must be a cursor value, its slot must
/// be live, and its generation must match (a mismatch means the cursor was
/// exhausted — and the slot possibly reopened for a different cursor).
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
                streams: Vec::new(),
            }
        }

        /// Streams still open (an exhausted stream must have been freed).
        pub fn live_streams(&self) -> usize {
            self.streams.iter().filter(|s| s.is_some()).count()
        }
    }

    impl DbBridge for MockBridge {
        fn query(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<Vec<Vec<Value>>> {
            self.queries += 1;
            Ok(self.rows.clone())
        }
        fn exec(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<u64> {
            self.execs += 1;
            Ok(self.affected)
        }
        fn cursor_open(&mut self, _plan: &PlanRef, _params: &[Value]) -> Result<u32> {
            self.cursor_opens += 1;
            self.streams.push(Some(self.rows.clone().into()));
            Ok((self.streams.len() - 1) as u32)
        }
        fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>> {
            let slot = self
                .streams
                .get_mut(stream as usize)
                .ok_or_else(|| rt_err("mock: bad stream id"))?;
            let Some(q) = slot else {
                return Err(rt_err("mock: advance on a freed stream"));
            };
            match q.pop_front() {
                Some(row) => Ok(Some(row)),
                None => {
                    *slot = None; // contract: freed on exhaustion
                    Ok(None)
                }
            }
        }
    }
}
