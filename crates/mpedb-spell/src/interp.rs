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
    /// A list the procedure built itself (`NewList` and friends).
    Seq(Rc<Built<Vec<PValue>>>),
    /// A dict the procedure built: text keys in insertion order.
    Map(Rc<Built<Vec<(String, PValue)>>>),
}

/// A container built by the procedure, with the two measures that bound it.
/// `depth` is how deeply containers nest inside it and `cells` how many
/// values it holds in all, counted as the tree a caller would receive (a
/// value placed twice counts twice). Both are kept on the container so a
/// build op checks its limits without walking what it is given.
#[derive(Debug, Clone, PartialEq)]
pub struct Built<T> {
    depth: u8,
    cells: u32,
    items: T,
}

impl<T: Default> Built<T> {
    fn empty() -> Built<T> {
        Built {
            depth: 1,
            cells: 1,
            items: T::default(),
        }
    }
}

/// Deepest nesting of built containers. Bounds every recursive walk
/// (equality, conversion on return, drop).
pub const MAX_BUILT_DEPTH: u8 = 8;
/// Most values one built container may hold, nested ones included.
pub const MAX_BUILT_CELLS: u32 = 1_000_000;
/// An op whose work grows with a container's length costs one extra
/// instruction-budget unit per this many elements it touches, so a
/// procedure cannot buy unbounded work for one unit.
pub const BUILT_WORK_PER_UNIT: usize = 32;

/// Depth and cell count of a value about to be placed in a container.
fn measure(v: &PValue) -> Result<(u8, u32)> {
    Ok(match v {
        PValue::Scalar(_) => (0, 1),
        PValue::Tuple(t) => (1, 1u32.saturating_add(t.len() as u32)),
        PValue::List(l) => {
            let mut cells = 1u32;
            for item in l.iter() {
                cells = cells.saturating_add(match item {
                    PValue::Tuple(t) | PValue::List(t) => 1u32.saturating_add(t.len() as u32),
                    _ => 1,
                });
            }
            (2, cells)
        }
        PValue::Seq(b) => (b.depth, b.cells),
        PValue::Map(b) => (b.depth, b.cells),
        PValue::Cursor { .. } => {
            return Err(type_err("a cursor cannot be stored in a list or dict"))
        }
    })
}

/// Account for one value added to a built container.
fn grow<T>(b: &mut Built<T>, (depth, cells): (u8, u32)) -> Result<()> {
    let depth = depth.saturating_add(1);
    if depth > MAX_BUILT_DEPTH {
        return Err(rt_err(format!(
            "lists and dicts may nest at most {MAX_BUILT_DEPTH} levels deep"
        )));
    }
    let total = b.cells.saturating_add(cells);
    if total > MAX_BUILT_CELLS {
        return Err(rt_err(format!(
            "a list or dict may hold at most {MAX_BUILT_CELLS} values"
        )));
    }
    b.depth = b.depth.max(depth);
    b.cells = total;
    Ok(())
}

/// Set `key` in a built dict: replace in place (the key keeps its position,
/// as in Python) or append.
fn dict_set(b: &mut Built<Vec<(String, PValue)>>, key: String, v: PValue) -> Result<()> {
    let m = measure(&v)?;
    if let Some(at) = b.items.iter().position(|(k, _)| *k == key) {
        let (_, old) = measure(&b.items[at].1)?;
        b.cells = b.cells.saturating_sub(old);
        grow(b, m)?;
        b.items[at].1 = v;
    } else {
        grow(b, m)?;
        b.items.push((key, v));
    }
    Ok(())
}

/// What `call` hands back: the scalar, or (for procs that return query
/// results) a list/tuple tree. Containers exist only on the way *out* —
/// they cannot be passed back in as arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcValue {
    Scalar(Value),
    List(Vec<ProcValue>),
    Tuple(Vec<ProcValue>),
    /// A dict the procedure built: text keys in insertion order.
    Map(Vec<(String, ProcValue)>),
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
            ProcValue::Map(items) => {
                f.write_str("{")?;
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k:?}: {v}")?;
                }
                f.write_str("}")
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
        // Bounded by MAX_BUILT_DEPTH and MAX_BUILT_CELLS, both enforced when
        // the container was built.
        PValue::Seq(b) => ProcValue::List(
            b.items
                .iter()
                .cloned()
                .map(to_proc_value)
                .collect::<Result<_>>()?,
        ),
        PValue::Map(b) => ProcValue::Map(
            b.items
                .iter()
                .map(|(k, v)| Ok((k.clone(), to_proc_value(v.clone())?)))
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
    /// `CallProc`: the procedure stored under `hash`. The default refuses,
    /// so a bridge that was not built to resolve procedures cannot run one
    /// by accident.
    fn load_proc(&mut self, _hash: &crate::hash::ProcHash) -> Result<std::sync::Arc<Proc>> {
        Err(Error::Unsupported(
            "proc runtime: calling another procedure is not available here".into(),
        ))
    }
    /// Drop a stream before it is exhausted (a called procedure returned
    /// with its cursor still open). Bridges without streams need nothing.
    fn cursor_close(&mut self, _stream: u32) -> Result<()> {
        Ok(())
    }
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
        PValue::Seq(_) => "list",
        PValue::Map(_) => "dict",
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
        PValue::Seq(b) => !b.items.is_empty(),
        PValue::Map(b) => !b.items.is_empty(),
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
        // A built list equals a list with equal elements, built or queried.
        (Seq(x), Seq(y)) => {
            x.items.len() == y.items.len() && x.items.iter().zip(&y.items).all(|(a, b)| eq(a, b))
        }
        (Seq(x), PValue::List(y)) | (PValue::List(y), Seq(x)) => {
            x.items.len() == y.len() && x.items.iter().zip(y.iter()).all(|(a, b)| eq(a, b))
        }
        // Dicts are equal when they hold the same keys with equal values,
        // in any order (Python).
        (Map(x), Map(y)) => {
            x.items.len() == y.items.len()
                && x.items.iter().all(|(k, a)| {
                    y.items.iter().any(|(k2, b)| k == k2 && eq(a, b))
                })
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
const _: () = assert!(MAX_CURSORS <= 32);

struct CurSlot {
    gen: u32,
    stream: Option<u32>,
    row: Option<PValue>,
}

/// Deepest chain of procedure calls. Calls form a DAG (a callee is fixed by
/// hash when its caller is defined), so this bounds the native stack, not
/// termination.
pub const MAX_CALL_DEPTH: usize = 16;

/// What one top-level call shares across every procedure it runs: the
/// bridge, the budgets and the cursor table.
struct State<'a> {
    bridge: &'a mut dyn DbBridge,
    budget: Budget,
    instrs_left: u64,
    db_left: u64,
    rows_left: u64,
    /// Grown on demand, bounded by MAX_CURSORS.
    cursors: Vec<CurSlot>,
    depth: usize,
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
    let mut st = State {
        bridge,
        budget,
        instrs_left: budget.instrs,
        db_left: budget.db_calls,
        rows_left: budget.rows,
        cursors: Vec::new(),
        depth: 0,
    };
    let locals = args.iter().map(|v| Some(PValue::Scalar(v.clone()))).collect();
    to_proc_value(exec(proc, locals, &mut st)?)
}

/// One bit per cursor slot that is taken (`MAX_CURSORS` fits a `u32`).
fn cursors_in_use(cursors: &[CurSlot]) -> u32 {
    cursors
        .iter()
        .enumerate()
        .filter(|(_, c)| c.stream.is_some())
        .fold(0, |bits, (i, _)| bits | (1 << i))
}

/// Close every cursor taken since `before`: what a called procedure opened
/// ends with that procedure, whether it returned or failed. Handles to
/// them go stale, so a cursor cannot be used outside the procedure that
/// opened it.
fn close_cursors_since(st: &mut State<'_>, before: u32) -> Result<()> {
    let now = cursors_in_use(&st.cursors);
    for i in 0..st.cursors.len() {
        if now & !before & (1 << i) == 0 {
            continue;
        }
        if let Some(stream) = st.cursors[i].stream {
            st.bridge.cursor_close(stream)?;
        }
        let c = &mut st.cursors[i];
        c.stream = None;
        c.row = None;
        c.gen = c.gen.wrapping_add(1);
    }
    Ok(())
}

/// One activation: `locals` holds the arguments, and the budgets and
/// cursors in `st` are the top-level call's.
fn exec(proc: &Proc, mut locals: Vec<Option<PValue>>, st: &mut State<'_>) -> Result<PValue> {
    // Option<PValue>: None = never assigned (UnboundLocalError analogue).
    locals.resize(proc.nlocals as usize, None);
    let mut stack: Vec<PValue> = Vec::with_capacity(proc.max_stack());
    let mut dbargs: Vec<Value> = Vec::new();
    let mut pc = 0usize;
    // Charge the extra budget of an op that touches `n` container elements.
    macro_rules! work {
        ($n:expr) => {{
            let extra = ($n / BUILT_WORK_PER_UNIT) as u64;
            if st.instrs_left < extra {
                return Err(budget_instr_err(st.budget.instrs));
            }
            st.instrs_left -= extra;
        }};
    }
    loop {
        if st.instrs_left == 0 {
            return Err(budget_instr_err(st.budget.instrs));
        }
        st.instrs_left -= 1;
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
                // Comparing built containers walks them; charge the walk.
                // Dict equality looks every key up in the other dict.
                match (&a, &b) {
                    (PValue::Seq(x), PValue::Seq(y)) => work!(x.cells.min(y.cells) as usize),
                    (PValue::Map(x), PValue::Map(y)) => {
                        let n = x.cells.min(y.cells) as usize;
                        work!(n.saturating_mul(x.items.len().min(y.items.len()).max(1)));
                    }
                    (PValue::Seq(x), _) | (_, PValue::Seq(x)) => work!(x.cells as usize),
                    _ => {}
                }
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
                    PValue::Seq(b) => b.items.len(),
                    PValue::Map(b) => b.items.len(),
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
                if let PValue::Map(b) = &cont {
                    let PValue::Scalar(Value::Text(key)) = &idx else {
                        return Err(type_err(format!(
                            "dict keys are text, not {}",
                            tyname(&idx)
                        )));
                    };
                    work!(b.items.len());
                    match b.items.iter().find(|(k, _)| k == key) {
                        Some((_, v)) => stack.push(v.clone()),
                        None => return Err(rt_err(format!("key {key:?} is not in the dict"))),
                    }
                    continue;
                }
                let i = match idx {
                    PValue::Scalar(Value::Int(i)) => i,
                    v => {
                        return Err(type_err(format!(
                            "indices must be integers, not {}",
                            tyname(&v)
                        )))
                    }
                };
                let items: &[PValue] = match &cont {
                    PValue::List(items) | PValue::Tuple(items) => items,
                    PValue::Seq(b) => &b.items,
                    v => {
                        return Err(type_err(format!(
                            "{} is not indexable (only lists, rows and dicts are)",
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
                if st.db_left == 0 {
                    return Err(budget_db_err(st.budget.db_calls));
                }
                st.db_left -= 1;
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
                        let rows = st.bridge.query(plan, &dbargs)?;
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
                        let n = st.bridge.exec(plan, &dbargs)?;
                        let n = i64::try_from(n)
                            .map_err(|_| rt_err("affected-row count exceeds i64"))?;
                        stack.push(PValue::Scalar(Value::Int(n)));
                    }
                    _ => {
                        // CursorOpen: claim a free slot (bounded), open the
                        // stream, hand out a slot+generation handle.
                        let slot = match st.cursors.iter().position(|c| c.stream.is_none()) {
                            Some(i) => i,
                            None if st.cursors.len() < MAX_CURSORS => {
                                st.cursors.push(CurSlot {
                                    gen: 0,
                                    stream: None,
                                    row: None,
                                });
                                st.cursors.len() - 1
                            }
                            None => {
                                return Err(rt_err(format!(
                                    "too many open cursors (max {MAX_CURSORS}); \
                                     iterate one to exhaustion before opening more"
                                )))
                            }
                        };
                        let stream = st.bridge.cursor_open(plan, &dbargs)?;
                        let c = &mut st.cursors[slot];
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
                let slot = pop_cursor(&mut stack, &st.cursors)?;
                if st.rows_left == 0 {
                    return Err(budget_rows_err(st.budget.rows));
                }
                st.rows_left -= 1;
                let stream = st.cursors[slot].stream.expect("pop_cursor checked");
                match st.bridge.cursor_advance(stream)? {
                    Some(row) => {
                        st.cursors[slot].row = Some(PValue::Tuple(Rc::new(
                            row.into_iter().map(PValue::Scalar).collect(),
                        )));
                        stack.push(PValue::Scalar(Value::Bool(true)));
                    }
                    None => {
                        // Exhausted: the bridge freed the stream; recycle
                        // the slot and invalidate outstanding handles.
                        let c = &mut st.cursors[slot];
                        c.stream = None;
                        c.row = None;
                        c.gen = c.gen.wrapping_add(1);
                        stack.push(PValue::Scalar(Value::Bool(false)));
                    }
                }
            }
            Op::CursorRow => {
                let slot = pop_cursor(&mut stack, &st.cursors)?;
                match &st.cursors[slot].row {
                    Some(row) => stack.push(row.clone()),
                    None => {
                        return Err(rt_err(
                            "cursor has no current row (advance it first)",
                        ))
                    }
                }
            }
            Op::CallProc(i) => {
                let callee = proc.callees[i as usize];
                if st.depth >= MAX_CALL_DEPTH {
                    return Err(rt_err(format!(
                        "procedures call each other more than {MAX_CALL_DEPTH} deep"
                    )));
                }
                let target = st.bridge.load_proc(&callee.hash)?;
                // The caller's blob promised what the callee is; a callee
                // that takes other arguments, writes or opens cursors when
                // the entry says it does not would break the routing the
                // caller was given.
                if target.argc != callee.argc
                    || (target.has_exec() && !callee.writes)
                    || (target.has_cursor() && !callee.cursors)
                {
                    return Err(Error::Corrupt(
                        "proc: a called procedure does not match its callee entry".into(),
                    ));
                }
                let base = stack.len() - callee.argc as usize; // >= 0: validated
                let args: Vec<Option<PValue>> = stack.drain(base..).map(Some).collect();
                let before = cursors_in_use(&st.cursors);
                st.depth += 1;
                let result = exec(&target, args, st);
                st.depth -= 1;
                close_cursors_since(st, before)?;
                stack.push(result?);
            }
            Op::NewList => stack.push(PValue::Seq(Rc::new(Built::empty()))),
            Op::NewDict => stack.push(PValue::Map(Rc::new(Built::empty()))),
            Op::ListAppend => {
                let v = stack.pop().expect("validated");
                let PValue::Seq(mut list) = stack.pop().expect("validated") else {
                    return Err(type_err("only a list built here ([...]) can be added to"));
                };
                let m = measure(&v)?;
                // A list nothing else holds grows in place; a shared one is
                // copied first, and the copy is charged.
                if Rc::get_mut(&mut list).is_none() {
                    work!(list.items.len());
                }
                let b = Rc::make_mut(&mut list);
                grow(b, m)?;
                b.items.push(v);
                stack.push(PValue::Seq(list));
            }
            Op::ListExtend => {
                let other = stack.pop().expect("validated");
                let PValue::Seq(mut list) = stack.pop().expect("validated") else {
                    return Err(type_err("only a list built here ([...]) can be added to"));
                };
                let more: &[PValue] = match &other {
                    PValue::List(items) | PValue::Tuple(items) => items,
                    PValue::Seq(b) => &b.items,
                    v => {
                        return Err(type_err(format!(
                            "* in a list needs a list or a row, not {}",
                            tyname(v)
                        )))
                    }
                };
                work!(more.len());
                if Rc::get_mut(&mut list).is_none() {
                    work!(list.items.len());
                }
                let b = Rc::make_mut(&mut list);
                for v in more {
                    grow(b, measure(v)?)?;
                    b.items.push(v.clone());
                }
                stack.push(PValue::Seq(list));
            }
            Op::DictSet => {
                let v = stack.pop().expect("validated");
                let key = stack.pop().expect("validated");
                let PValue::Map(mut map) = stack.pop().expect("validated") else {
                    return Err(type_err("only a dict built here ({...}) can be added to"));
                };
                let PValue::Scalar(Value::Text(key)) = key else {
                    return Err(type_err(format!(
                        "dict keys must be text, not {}",
                        tyname(&key)
                    )));
                };
                work!(map.items.len());
                dict_set(Rc::make_mut(&mut map), key, v)?;
                stack.push(PValue::Map(map));
            }
            Op::DictMerge => {
                let other = stack.pop().expect("validated");
                let PValue::Map(mut map) = stack.pop().expect("validated") else {
                    return Err(type_err("only a dict built here ({...}) can be added to"));
                };
                let PValue::Map(more) = other else {
                    return Err(type_err(format!(
                        "** in a dict needs a dict, not {}",
                        tyname(&other)
                    )));
                };
                // Every entry is looked up in a dict that may grow to both.
                let n = more.items.len();
                work!(n.saturating_mul(map.items.len() + n));
                let b = Rc::make_mut(&mut map);
                for (k, v) in &more.items {
                    dict_set(b, k.clone(), v.clone())?;
                }
                stack.push(PValue::Map(map));
            }
            Op::Contains => {
                let cont = stack.pop().expect("validated");
                let item = stack.pop().expect("validated");
                let found = match &cont {
                    PValue::List(items) | PValue::Tuple(items) => {
                        work!(items.len());
                        items.iter().any(|v| eq(v, &item))
                    }
                    PValue::Seq(b) => {
                        work!(b.cells as usize);
                        b.items.iter().any(|v| eq(v, &item))
                    }
                    PValue::Map(b) => {
                        work!(b.items.len());
                        match &item {
                            PValue::Scalar(Value::Text(key)) => b.items.iter().any(|(k, _)| k == key),
                            _ => false,
                        }
                    }
                    PValue::Scalar(Value::Text(hay)) => {
                        let PValue::Scalar(Value::Text(needle)) = &item else {
                            return Err(type_err(format!(
                                "`in` on text needs text on the left, not {}",
                                tyname(&item)
                            )));
                        };
                        work!(hay.len());
                        hay.contains(needle.as_str())
                    }
                    v => {
                        return Err(type_err(format!(
                            "`in` needs a list, a row, a dict or text on the right, not {}",
                            tyname(v)
                        )))
                    }
                };
                stack.push(PValue::Scalar(Value::Bool(found)));
            }
            Op::Return => {
                let v = stack.pop().expect("validated");
                return Ok(v);
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
