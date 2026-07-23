//! The procedure IR: a compact, stack-based bytecode with locals, jumps and
//! two database ops. This is the **security boundary** (the PySpell model):
//! source text is parsed exactly once, on the host, at `define` time; what is
//! stored in — and later loaded from — the database is only this IR, and the
//! decoder re-derives every safety property (operand bounds, jump targets,
//! stack discipline, plan-arity agreement) from the bytes themselves.
//! Hostile or bit-rotted blobs yield [`Error::Corrupt`], never a panic.
//!
//! # Semantics (differ deliberately from the SQL expression IR)
//!
//! Unlike `mpedb_types::expr` (SQL three-valued logic), procedure code uses
//! ordinary Python/Rust semantics: `Value::Null` plays the role of Python's
//! `None`, `None == None` is *true*, comparisons never yield "unknown", and
//! `and`/`or`/`not` are plain short-circuit boolean logic compiled to jumps.
//! Integer overflow is an error (no wrapping, no bigints); division by zero
//! is an error. Division comes in per-frontend flavors: [`Op::TrueDiv`] and
//! [`Op::FloorDiv`]/[`Op::PyMod`] carry Python semantics, [`Op::IntDiv`] and
//! [`Op::IntRem`] carry Rust semantics; each frontend emits its own.
//!
//! # Budget
//!
//! Every *executed instruction* costs one unit of the instruction budget (so
//! in particular every backward jump does), and every
//! `DbQuery`/`DbExec`/`CursorOpen` additionally costs one unit of the
//! db-call budget. `CursorAdvance` deliberately does NOT cost a db call —
//! one full-table scan would burn the whole 10k db-call budget — it costs
//! one unit of the separate, generous row budget (default 10M rows).
//!
//! # Cursors
//!
//! `CursorOpen(p)` opens a streaming scan over `plans[p]` (which must be
//! `Query`-kind — validated statically, decode included) and pushes an
//! opaque cursor handle; `CursorAdvance` pulls ONE row from the engine
//! (O(1) interpreter memory) and pushes whether a row is available;
//! `CursorRow` pushes the current row as a tuple. Handles are runtime
//! values with slot+generation identity; the runtime bounds live cursors
//! (see `interp::MAX_CURSORS`) and rejects stale handles. **v1 rule:**
//! cursors are allowed only in read-only procedures — a program containing
//! both `CursorOpen` and `DbExec` fails validation, so the question of a
//! cursor observing (or blocking on) the proc's own write session never
//! arises. Write procs keep the materializing `DbQuery`.

use crate::hash::ProcHash;
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{Error, PlanHash, Result, Value};

/// Format version, embedded in every blob and covered by the content hash.
pub const PROC_FORMAT: u16 = 1;

const MAGIC: &[u8; 4] = b"MPRC";

pub const MAX_NAME: usize = 128;
pub const MAX_LOCALS: usize = 1024;
pub const MAX_PLANS: usize = 256;
pub const MAX_CONSTS: usize = 4096;
pub const MAX_INSTRS: usize = 1 << 20;
/// Static bound on operand-stack depth, proven by [`validate`].
pub const MAX_STACK: usize = 256;
/// Most parameters one embedded SQL statement may take.
pub const MAX_DB_ARGS: usize = 64;

/// How an embedded plan is used. `Query` plans must be read-only SELECTs;
/// `Exec` plans must be DML. Enforced at define time against the recomputed
/// plan footprint, and covered by the proc's content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Query,
    Exec,
}

/// One entry of the proc's plan table: the SQL was compiled at define time,
/// published to the shared plan registry, and only its content hash is
/// embedded here. The runtime executes by hash and **never parses SQL**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRef {
    pub hash: PlanHash,
    pub kind: PlanKind,
    /// Exact number of `$n` parameters the plan takes (checked at define
    /// time against the compiled plan, re-checked structurally at decode).
    pub argc: u8,
}

/// One bytecode instruction. Stack effects are fixed per opcode (db ops pop
/// their plan's `argc`), which is what makes static validation possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push `consts[i]`.
    LoadConst(u16),
    /// Push `locals[i]`; using a local before any store is a runtime error.
    LoadLocal(u16),
    /// Pop into `locals[i]`.
    StoreLocal(u16),
    /// Discard the top of stack (expression statements).
    Pop,
    /// Duplicate the top of stack (short-circuit and/or).
    Dup,
    /// Arithmetic negation (int checked, float IEEE).
    Neg,
    /// Boolean not, via truthiness.
    Not,
    Add,
    Sub,
    Mul,
    /// Python `/`: int/int yields float; division by zero errors.
    TrueDiv,
    /// Python `//`: floor division; int overflow / zero divisor error.
    FloorDiv,
    /// Rust `/`: int/int truncates toward zero; float follows IEEE.
    IntDiv,
    /// Python `%`: result takes the divisor's sign.
    PyMod,
    /// Rust `%`: result takes the dividend's sign; float follows IEEE.
    IntRem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// len() of a list/tuple/text/blob.
    Len,
    /// `container[index]`; negative indices wrap Python-style.
    Index,
    /// Unconditional jump to an absolute instruction index.
    Jump(u32),
    /// Pop; jump if falsey.
    JumpIfFalse(u32),
    /// Pop; jump if truthy.
    JumpIfTrue(u32),
    /// Pop `plans[p].argc` scalar args, run the SELECT plan, push the rows
    /// as a list of tuples.
    DbQuery(u16),
    /// Pop `plans[p].argc` scalar args, run the DML plan, push the affected
    /// row count as an int.
    DbExec(u16),
    /// Pop `plans[p].argc` scalar args, open a streaming scan over the
    /// `Query`-kind plan `plans[p]`, push a cursor handle (module docs).
    CursorOpen(u16),
    /// Pop a cursor handle, pull one row from the engine (storing it as the
    /// cursor's current row), push `true`; or push `false` and close the
    /// cursor when the scan is exhausted. Costs one row-budget unit.
    CursorAdvance,
    /// Pop a cursor handle, push its current row as a tuple.
    CursorRow,
    /// Pop the return value and finish the procedure.
    Return,
}

const OP_LOAD_CONST: u8 = 1;
const OP_LOAD_LOCAL: u8 = 2;
const OP_STORE_LOCAL: u8 = 3;
const OP_POP: u8 = 4;
const OP_DUP: u8 = 5;
const OP_NEG: u8 = 6;
const OP_NOT: u8 = 7;
const OP_ADD: u8 = 8;
const OP_SUB: u8 = 9;
const OP_MUL: u8 = 10;
const OP_TRUE_DIV: u8 = 11;
const OP_FLOOR_DIV: u8 = 12;
const OP_INT_DIV: u8 = 13;
const OP_PY_MOD: u8 = 14;
const OP_INT_REM: u8 = 15;
const OP_EQ: u8 = 16;
const OP_NE: u8 = 17;
const OP_LT: u8 = 18;
const OP_LE: u8 = 19;
const OP_GT: u8 = 20;
const OP_GE: u8 = 21;
const OP_LEN: u8 = 22;
const OP_INDEX: u8 = 23;
const OP_JUMP: u8 = 24;
const OP_JUMP_IF_FALSE: u8 = 25;
const OP_JUMP_IF_TRUE: u8 = 26;
const OP_DB_QUERY: u8 = 27;
const OP_DB_EXEC: u8 = 28;
const OP_RETURN: u8 = 29;
// Cursor opcodes are an additive extension of format 1: old blobs decode
// unchanged, and a pre-cursor build rejects a cursor-bearing blob with
// `invalid opcode` (Corrupt) instead of misinterpreting it.
const OP_CURSOR_OPEN: u8 = 30;
const OP_CURSOR_ADVANCE: u8 = 31;
const OP_CURSOR_ROW: u8 = 32;

/// A validated procedure: everything the runtime needs, nothing it has to
/// trust. Constructible only through [`Proc::new`] / [`Proc::decode`], both
/// of which run the full static validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub name: String,
    /// Number of parameters; they occupy `locals[0..argc]`.
    pub argc: u16,
    pub nlocals: u16,
    pub plans: Vec<PlanRef>,
    pub consts: Vec<Value>,
    pub instrs: Vec<Op>,
    /// Proven maximum operand-stack depth (validation artifact).
    max_stack: usize,
    /// Whether any instruction is `DbExec` — decides transactional routing.
    /// Recomputed from the instructions, never stored.
    has_exec: bool,
}

impl Proc {
    pub fn new(
        name: String,
        argc: u16,
        nlocals: u16,
        plans: Vec<PlanRef>,
        consts: Vec<Value>,
        instrs: Vec<Op>,
    ) -> Result<Proc> {
        check_name(&name)?;
        if (argc as usize) > nlocals as usize || nlocals as usize > MAX_LOCALS {
            return Err(Error::Corrupt("proc: bad local count".into()));
        }
        if plans.len() > MAX_PLANS || consts.len() > MAX_CONSTS {
            return Err(Error::Corrupt("proc: plan/const table too large".into()));
        }
        for p in &plans {
            if p.argc as usize > MAX_DB_ARGS {
                return Err(Error::Corrupt("proc: plan takes too many parameters".into()));
            }
        }
        let max_stack = validate(&instrs, nlocals, consts.len(), &plans)?;
        let has_exec = instrs.iter().any(|op| matches!(op, Op::DbExec(_)));
        Ok(Proc {
            name,
            argc,
            nlocals,
            plans,
            consts,
            instrs,
            max_stack,
            has_exec,
        })
    }

    pub fn max_stack(&self) -> usize {
        self.max_stack
    }

    /// True if the proc contains any `DbExec`: it must run inside a single
    /// write transaction. False: it may run lock-free on read snapshots.
    pub fn has_exec(&self) -> bool {
        self.has_exec
    }

    /// Any database operation at all — query, exec, or cursor. A stored SQL
    /// FUNCTION (stage M2) must have none; this is the load-time re-check
    /// behind `create_function`'s define-time refusal.
    pub fn has_db_ops(&self) -> bool {
        self.instrs
            .iter()
            .any(|op| matches!(op, Op::DbQuery(_) | Op::DbExec(_) | Op::CursorOpen(_)))
            || !self.plans.is_empty()
    }

    /// Canonical serialization; the blob stored in the database and the
    /// preimage of [`Proc::hash`].
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.instrs.len() * 3);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&PROC_FORMAT.to_le_bytes());
        b.push(self.name.len() as u8);
        b.extend_from_slice(self.name.as_bytes());
        b.extend_from_slice(&self.argc.to_le_bytes());
        b.extend_from_slice(&self.nlocals.to_le_bytes());
        b.extend_from_slice(&(self.plans.len() as u16).to_le_bytes());
        for p in &self.plans {
            b.extend_from_slice(&p.hash.0);
            b.push(match p.kind {
                PlanKind::Query => 0,
                PlanKind::Exec => 1,
            });
            b.push(p.argc);
        }
        b.extend_from_slice(&(self.consts.len() as u16).to_le_bytes());
        for c in &self.consts {
            write_value(&mut b, c);
        }
        b.extend_from_slice(&(self.instrs.len() as u32).to_le_bytes());
        for &op in &self.instrs {
            encode_op(&mut b, op);
        }
        b
    }

    /// Content hash of the canonical blob (format version included in the
    /// preimage via the header). Same philosophy as plan hashes (§7): the
    /// hash *is* the identity, across every attached process.
    pub fn hash(&self) -> ProcHash {
        ProcHash(*blake3::hash(&self.encode()).as_bytes())
    }

    /// Fully re-validating decode. Every read is bounds-checked, the whole
    /// buffer must be consumed, and the decoded program passes the same
    /// static validation as freshly compiled code — a hostile blob can be
    /// rejected but can never make the runtime read out of bounds.
    pub fn decode(buf: &[u8]) -> Result<Proc> {
        let err = || Error::Corrupt("proc: truncated blob".into());
        let mut pos = 0usize;
        let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
            let end = pos
                .checked_add(n)
                .filter(|&e| e <= buf.len())
                .ok_or_else(err)?;
            let s = &buf[*pos..end];
            *pos = end;
            Ok(s)
        };
        if take(&mut pos, 4)? != MAGIC {
            return Err(Error::Corrupt("proc: bad magic".into()));
        }
        let version = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        if version != PROC_FORMAT {
            return Err(Error::Corrupt(format!(
                "proc: format version {version} (this build understands {PROC_FORMAT})"
            )));
        }
        let name_len = take(&mut pos, 1)?[0] as usize;
        if name_len > MAX_NAME {
            return Err(Error::Corrupt("proc: name too long".into()));
        }
        let name = std::str::from_utf8(take(&mut pos, name_len)?)
            .map_err(|_| Error::Corrupt("proc: name is not utf-8".into()))?
            .to_owned();
        let argc = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        let nlocals = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        let nplans = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as usize;
        if nplans > MAX_PLANS {
            return Err(Error::Corrupt("proc: too many plans".into()));
        }
        let mut plans = Vec::with_capacity(nplans);
        for _ in 0..nplans {
            let hash = PlanHash(take(&mut pos, 32)?.try_into().unwrap());
            let kind = match take(&mut pos, 1)?[0] {
                0 => PlanKind::Query,
                1 => PlanKind::Exec,
                k => return Err(Error::Corrupt(format!("proc: invalid plan kind {k}"))),
            };
            let argc = take(&mut pos, 1)?[0];
            plans.push(PlanRef { hash, kind, argc });
        }
        let nconsts = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as usize;
        if nconsts > MAX_CONSTS {
            return Err(Error::Corrupt("proc: too many constants".into()));
        }
        let mut consts = Vec::with_capacity(nconsts.min(1024));
        for _ in 0..nconsts {
            consts.push(read_value(buf, &mut pos)?);
        }
        let ninstrs = u32::from_le_bytes(take(&mut pos, 4)?.try_into().unwrap()) as usize;
        if ninstrs > MAX_INSTRS {
            return Err(Error::Corrupt("proc: program too large".into()));
        }
        let mut instrs = Vec::with_capacity(ninstrs.min(4096));
        for _ in 0..ninstrs {
            instrs.push(decode_op(buf, &mut pos)?);
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("proc: trailing bytes after program".into()));
        }
        // Full revalidation: identical to the compile-time path.
        Proc::new(name, argc, nlocals, plans, consts, instrs)
    }
}

fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !head_ok
        || name.len() > MAX_NAME
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error::Corrupt(format!(
            "proc: invalid procedure name {name:?}"
        )));
    }
    Ok(())
}

fn encode_op(b: &mut Vec<u8>, op: Op) {
    let mut u16arg = |code: u8, x: u16| {
        b.push(code);
        b.extend_from_slice(&x.to_le_bytes());
    };
    match op {
        Op::LoadConst(x) => u16arg(OP_LOAD_CONST, x),
        Op::LoadLocal(x) => u16arg(OP_LOAD_LOCAL, x),
        Op::StoreLocal(x) => u16arg(OP_STORE_LOCAL, x),
        Op::DbQuery(x) => u16arg(OP_DB_QUERY, x),
        Op::DbExec(x) => u16arg(OP_DB_EXEC, x),
        Op::CursorOpen(x) => u16arg(OP_CURSOR_OPEN, x),
        Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
            b.push(match op {
                Op::Jump(_) => OP_JUMP,
                Op::JumpIfFalse(_) => OP_JUMP_IF_FALSE,
                _ => OP_JUMP_IF_TRUE,
            });
            b.extend_from_slice(&t.to_le_bytes());
        }
        Op::Pop => b.push(OP_POP),
        Op::Dup => b.push(OP_DUP),
        Op::Neg => b.push(OP_NEG),
        Op::Not => b.push(OP_NOT),
        Op::Add => b.push(OP_ADD),
        Op::Sub => b.push(OP_SUB),
        Op::Mul => b.push(OP_MUL),
        Op::TrueDiv => b.push(OP_TRUE_DIV),
        Op::FloorDiv => b.push(OP_FLOOR_DIV),
        Op::IntDiv => b.push(OP_INT_DIV),
        Op::PyMod => b.push(OP_PY_MOD),
        Op::IntRem => b.push(OP_INT_REM),
        Op::Eq => b.push(OP_EQ),
        Op::Ne => b.push(OP_NE),
        Op::Lt => b.push(OP_LT),
        Op::Le => b.push(OP_LE),
        Op::Gt => b.push(OP_GT),
        Op::Ge => b.push(OP_GE),
        Op::Len => b.push(OP_LEN),
        Op::Index => b.push(OP_INDEX),
        Op::CursorAdvance => b.push(OP_CURSOR_ADVANCE),
        Op::CursorRow => b.push(OP_CURSOR_ROW),
        Op::Return => b.push(OP_RETURN),
    }
}

fn decode_op(buf: &[u8], pos: &mut usize) -> Result<Op> {
    let err = || Error::Corrupt("proc: truncated instruction".into());
    let op = *buf.get(*pos).ok_or_else(err)?;
    *pos += 1;
    let mut u16arg = || -> Result<u16> {
        let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
        *pos += 2;
        Ok(u16::from_le_bytes(raw.try_into().unwrap()))
    };
    Ok(match op {
        OP_LOAD_CONST => Op::LoadConst(u16arg()?),
        OP_LOAD_LOCAL => Op::LoadLocal(u16arg()?),
        OP_STORE_LOCAL => Op::StoreLocal(u16arg()?),
        OP_DB_QUERY => Op::DbQuery(u16arg()?),
        OP_DB_EXEC => Op::DbExec(u16arg()?),
        OP_CURSOR_OPEN => Op::CursorOpen(u16arg()?),
        OP_JUMP | OP_JUMP_IF_FALSE | OP_JUMP_IF_TRUE => {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            let t = u32::from_le_bytes(raw.try_into().unwrap());
            match op {
                OP_JUMP => Op::Jump(t),
                OP_JUMP_IF_FALSE => Op::JumpIfFalse(t),
                _ => Op::JumpIfTrue(t),
            }
        }
        OP_POP => Op::Pop,
        OP_DUP => Op::Dup,
        OP_NEG => Op::Neg,
        OP_NOT => Op::Not,
        OP_ADD => Op::Add,
        OP_SUB => Op::Sub,
        OP_MUL => Op::Mul,
        OP_TRUE_DIV => Op::TrueDiv,
        OP_FLOOR_DIV => Op::FloorDiv,
        OP_INT_DIV => Op::IntDiv,
        OP_PY_MOD => Op::PyMod,
        OP_INT_REM => Op::IntRem,
        OP_EQ => Op::Eq,
        OP_NE => Op::Ne,
        OP_LT => Op::Lt,
        OP_LE => Op::Le,
        OP_GT => Op::Gt,
        OP_GE => Op::Ge,
        OP_LEN => Op::Len,
        OP_INDEX => Op::Index,
        OP_CURSOR_ADVANCE => Op::CursorAdvance,
        OP_CURSOR_ROW => Op::CursorRow,
        OP_RETURN => Op::Return,
        other => return Err(Error::Corrupt(format!("proc: invalid opcode {other}"))),
    })
}

/// Stack effect (pops, pushes) of an instruction; db ops consult the plan
/// table (operand bounds are checked before this is called).
fn stack_effect(op: Op, plans: &[PlanRef]) -> (usize, usize) {
    match op {
        Op::LoadConst(_) | Op::LoadLocal(_) => (0, 1),
        Op::StoreLocal(_) | Op::Pop => (1, 0),
        Op::Dup => (1, 2),
        Op::Neg | Op::Not | Op::Len => (1, 1),
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::TrueDiv
        | Op::FloorDiv
        | Op::IntDiv
        | Op::PyMod
        | Op::IntRem
        | Op::Eq
        | Op::Ne
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::Index => (2, 1),
        Op::Jump(_) => (0, 0),
        Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => (1, 0),
        Op::DbQuery(p) | Op::DbExec(p) | Op::CursorOpen(p) => {
            (plans[p as usize].argc as usize, 1)
        }
        Op::CursorAdvance | Op::CursorRow => (1, 1),
        Op::Return => (1, 0),
    }
}

/// Static validation (the decode-side sibling of `expr::validate`, extended
/// with a control-flow graph walk):
///
/// 1. every operand index (local, const, plan, jump target) is in range —
///    checked for *all* instructions, reachable or not;
/// 2. a worklist pass over the CFG proves that every reachable instruction
///    is entered at one consistent stack depth, that the stack never
///    underflows or exceeds [`MAX_STACK`], and that execution can never fall
///    off the end of the program (every terminal path ends in `Return`).
///
/// Returns the proven maximum stack depth so the interpreter can
/// preallocate and skip per-instruction underflow checks.
fn validate(instrs: &[Op], nlocals: u16, nconsts: usize, plans: &[PlanRef]) -> Result<usize> {
    if instrs.is_empty() {
        return Err(Error::Corrupt("proc: empty program".into()));
    }
    let n = instrs.len();
    // Pass 1: operand bounds, including unreachable code.
    let mut any_cursor = false;
    let mut any_exec = false;
    for &op in instrs {
        match op {
            Op::LoadLocal(i) | Op::StoreLocal(i) => {
                if i >= nlocals {
                    return Err(Error::Corrupt("proc: local index out of range".into()));
                }
            }
            Op::LoadConst(i) => {
                if i as usize >= nconsts {
                    return Err(Error::Corrupt("proc: const index out of range".into()));
                }
            }
            Op::DbQuery(p) | Op::DbExec(p) | Op::CursorOpen(p)
                if p as usize >= plans.len() =>
            {
                return Err(Error::Corrupt("proc: plan index out of range".into()));
            }
            // A cursor scans; only Query-kind (read-only SELECT) plans scan.
            Op::CursorOpen(p) if plans[p as usize].kind != PlanKind::Query => {
                return Err(Error::Corrupt(
                    "proc: cursor over a non-Query plan".into(),
                ));
            }
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) if t as usize >= n => {
                return Err(Error::Corrupt("proc: jump target out of range".into()));
            }
            _ => {}
        }
        match op {
            Op::CursorOpen(_) => any_cursor = true,
            Op::DbExec(_) => any_exec = true,
            _ => {}
        }
    }
    // v1 cursor rule (module docs): cursors only in read-only procedures.
    // Checked structurally so hostile blobs cannot smuggle a cursor into a
    // write session either.
    if any_cursor && any_exec {
        return Err(Error::Corrupt(
            "proc: cursors are allowed only in read-only procedures \
             (no DbExec together with CursorOpen)"
                .into(),
        ));
    }
    // Pass 2: CFG walk with per-entry stack depths.
    let mut entry: Vec<Option<usize>> = vec![None; n];
    let mut work: Vec<(usize, usize)> = vec![(0, 0)];
    let mut max = 0usize;
    let visit = |pc: usize,
                     depth: usize,
                     entry: &mut Vec<Option<usize>>,
                     work: &mut Vec<(usize, usize)>|
     -> Result<()> {
        if pc >= n {
            return Err(Error::Corrupt(
                "proc: control flow falls off the end of the program".into(),
            ));
        }
        match entry[pc] {
            Some(d) if d == depth => {}
            Some(_) => {
                return Err(Error::Corrupt(
                    "proc: inconsistent stack depth at join point".into(),
                ))
            }
            None => {
                entry[pc] = Some(depth);
                work.push((pc, depth));
            }
        }
        Ok(())
    };
    // Seed pc 0.
    entry[0] = Some(0);
    while let Some((pc, depth)) = work.pop() {
        let op = instrs[pc];
        let (pops, pushes) = stack_effect(op, plans);
        if depth < pops {
            return Err(Error::Corrupt("proc: stack underflow".into()));
        }
        let after = depth - pops + pushes;
        if after > MAX_STACK {
            return Err(Error::Corrupt("proc: stack too deep".into()));
        }
        max = max.max(after);
        match op {
            Op::Return => {}
            Op::Jump(t) => visit(t as usize, after, &mut entry, &mut work)?,
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                visit(t as usize, after, &mut entry, &mut work)?;
                visit(pc + 1, after, &mut entry, &mut work)?;
            }
            _ => visit(pc + 1, after, &mut entry, &mut work)?,
        }
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(kind: PlanKind, argc: u8) -> PlanRef {
        PlanRef {
            hash: PlanHash([7u8; 32]),
            kind,
            argc,
        }
    }

    fn mk(instrs: Vec<Op>, consts: Vec<Value>, plans: Vec<PlanRef>) -> Result<Proc> {
        Proc::new("t".into(), 1, 2, plans, consts, instrs)
    }

    #[test]
    fn roundtrip_and_hash_stability() {
        let p = Proc::new(
            "transfer".into(),
            3,
            5,
            vec![plan(PlanKind::Query, 1), plan(PlanKind::Exec, 2)],
            vec![Value::Int(0), Value::Text("x".into()), Value::Null],
            vec![
                Op::LoadLocal(0),
                Op::DbQuery(0),
                Op::StoreLocal(3),
                Op::LoadLocal(1),
                Op::LoadLocal(2),
                Op::DbExec(1),
                Op::Pop,
                Op::LoadConst(2),
                Op::Return,
            ],
        )
        .unwrap();
        let blob = p.encode();
        let q = Proc::decode(&blob).unwrap();
        assert_eq!(p, q);
        assert_eq!(p.hash(), q.hash());
        assert!(p.has_exec());
        // hash covers the plan kind byte
        let mut r = q.clone();
        r.plans[0].kind = PlanKind::Exec;
        assert_ne!(p.hash(), r.hash());
    }

    #[test]
    fn decode_fuzz_truncation_and_bitflips() {
        let p = Proc::new(
            "f".into(),
            1,
            3,
            vec![plan(PlanKind::Query, 2)],
            vec![Value::Int(41), Value::Float(2.5), Value::Text("s".into())],
            vec![
                Op::LoadLocal(0),
                Op::LoadConst(0),
                Op::Add,
                Op::StoreLocal(1),
                Op::LoadLocal(1),
                Op::LoadConst(1),
                Op::Lt,
                Op::JumpIfFalse(12),
                Op::LoadLocal(1),
                Op::LoadConst(2),
                Op::DbQuery(0),
                Op::Return,
                Op::LoadConst(0),
                Op::Return,
            ],
        )
        .unwrap();
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        // Truncation at every offset: error, never panic.
        for cut in 0..blob.len() {
            assert!(Proc::decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        // Every single-bit flip: decode may succeed only if it still
        // validates; it must never panic. (Exhaustive: 8 * len decodes.)
        for byte in 0..blob.len() {
            for bit in 0..8 {
                let mut evil = blob.clone();
                evil[byte] ^= 1 << bit;
                let _ = Proc::decode(&evil);
            }
        }
        // Trailing garbage is rejected.
        let mut long = blob.clone();
        long.push(0);
        assert!(Proc::decode(&long).is_err());
    }

    #[test]
    fn validation_rejects_bad_programs() {
        // stack underflow
        assert!(mk(vec![Op::Add], vec![], vec![]).is_err());
        // empty
        assert!(mk(vec![], vec![], vec![]).is_err());
        // falls off the end
        assert!(mk(vec![Op::LoadConst(0)], vec![Value::Null], vec![]).is_err());
        // jump out of range
        assert!(mk(
            vec![Op::Jump(9), Op::LoadConst(0), Op::Return],
            vec![Value::Null],
            vec![]
        )
        .is_err());
        // conditional jump at the very end falls through past the program
        assert!(mk(
            vec![Op::LoadConst(0), Op::JumpIfFalse(0)],
            vec![Value::Bool(true)],
            vec![]
        )
        .is_err());
        // inconsistent depth at join: one path pushes 1, the other 2
        assert!(mk(
            vec![
                Op::LoadConst(0),   // 0: cond
                Op::JumpIfFalse(4), // 1
                Op::LoadConst(0),   // 2
                Op::LoadConst(0),   // 3  (depth 2 into pc 4)
                Op::LoadConst(0),   // 4  (depth 0 or 2)
                Op::Return,
            ],
            vec![Value::Bool(true)],
            vec![]
        )
        .is_err());
        // local/const/plan out of range
        assert!(mk(vec![Op::LoadLocal(99), Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::LoadConst(0), Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::DbQuery(0), Op::Return], vec![], vec![]).is_err());
        // Return with empty stack
        assert!(mk(vec![Op::Return], vec![], vec![]).is_err());
        // bad name
        assert!(Proc::new("9x".into(), 0, 0, vec![], vec![], vec![
            Op::LoadConst(0),
            Op::Return
        ])
        .is_err());
        // argc > nlocals
        assert!(Proc::new("f".into(), 3, 1, vec![], vec![Value::Null], vec![
            Op::LoadConst(0),
            Op::Return
        ])
        .is_err());
    }

    #[test]
    fn db_ops_pop_their_plan_arity() {
        // DbQuery with argc 2 but only one value on the stack: underflow.
        assert!(mk(
            vec![Op::LoadConst(0), Op::DbQuery(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .is_err());
        // Correct arity validates.
        let p = mk(
            vec![
                Op::LoadConst(0),
                Op::LoadConst(0),
                Op::DbQuery(0),
                Op::Return,
            ],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .unwrap();
        assert!(!p.has_exec());
        assert_eq!(p.max_stack(), 2);
    }

    /// Cursor ops: encode/decode roundtrip, full decode fuzz (truncation at
    /// every offset + every single-bit flip — same bar as the base opcode
    /// set), and the static validation rules specific to cursors.
    #[test]
    fn cursor_ops_roundtrip_and_decode_fuzz() {
        // A realistic streaming loop: open, advance, read, accumulate.
        let p = Proc::new(
            "scan_sum".into(),
            1,
            4,
            vec![plan(PlanKind::Query, 1)],
            vec![Value::Int(0), Value::Int(1)],
            vec![
                Op::LoadConst(0),    // 0: acc = 0
                Op::StoreLocal(1),   // 1
                Op::LoadLocal(0),    // 2: arg for the plan
                Op::CursorOpen(0),   // 3: c = cursor
                Op::StoreLocal(2),   // 4
                Op::LoadLocal(2),    // 5: loop head
                Op::CursorAdvance,   // 6
                Op::JumpIfFalse(16), // 7: -> end
                Op::LoadLocal(2),    // 8
                Op::CursorRow,       // 9
                Op::LoadConst(1),    // 10
                Op::Index,           // 11: row[1]
                Op::LoadLocal(1),    // 12
                Op::Add,             // 13
                Op::StoreLocal(1),   // 14
                Op::Jump(5),         // 15: the backward jump
                Op::LoadLocal(1),    // 16: end: return acc
                Op::Return,          // 17
            ],
        )
        .unwrap();
        assert!(!p.has_exec(), "cursors do not make a proc a writer");
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        for cut in 0..blob.len() {
            assert!(Proc::decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        for byte in 0..blob.len() {
            for bit in 0..8 {
                let mut evil = blob.clone();
                evil[byte] ^= 1 << bit;
                let _ = Proc::decode(&evil); // may reject, must not panic
            }
        }
    }

    #[test]
    fn cursor_validation_rules() {
        // CursorOpen plan index out of range.
        assert!(mk(vec![Op::CursorOpen(0), Op::Return], vec![], vec![]).is_err());
        // CursorOpen over an Exec-kind plan: rejected even though the index
        // is in range (a cursor is a scan; DML does not scan).
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorOpen(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Exec, 1)],
        )
        .is_err());
        // CursorOpen pops its plan's arity (underflow with argc 2, depth 1).
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorOpen(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .is_err());
        // v1 rule: CursorOpen + DbExec in one proc is structurally invalid,
        // even when the DbExec is unreachable.
        assert!(mk(
            vec![
                Op::CursorOpen(0),  // query plan, argc 0
                Op::Return,
                Op::LoadConst(0),   // unreachable
                Op::DbExec(1),
                Op::Return,
            ],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 0), plan(PlanKind::Exec, 1)],
        )
        .is_err());
        // Advance/Row on an empty stack underflow.
        assert!(mk(vec![Op::CursorAdvance, Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::CursorRow, Op::Return], vec![], vec![]).is_err());
        // The happy shape validates.
        let p = mk(
            vec![
                Op::CursorOpen(0),
                Op::CursorAdvance,
                Op::Return,
            ],
            vec![],
            vec![plan(PlanKind::Query, 0)],
        )
        .unwrap();
        assert_eq!(p.max_stack(), 1);
    }

    #[test]
    fn version_and_magic_are_checked() {
        let p = mk(
            vec![Op::LoadConst(0), Op::Return],
            vec![Value::Int(1)],
            vec![],
        )
        .unwrap();
        let mut blob = p.encode();
        blob[0] = b'X'; // magic
        assert!(matches!(Proc::decode(&blob), Err(Error::Corrupt(_))));
        let mut blob = p.encode();
        blob[4] = 0xFF; // version
        assert!(matches!(Proc::decode(&blob), Err(Error::Corrupt(_))));
    }
}
