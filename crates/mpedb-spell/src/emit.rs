//! Shared code-emission plumbing for the two frontends: local-slot
//! allocation, constant pooling, jump patching, loop contexts, and the
//! define-time SQL call table. Frontends produce a [`Skeleton`]; the engine
//! then compiles each collected SQL string through the facade (publishing
//! the plans) and assembles the final [`crate::ir::Proc`].

use crate::ir::{Op, MAX_CONSTS, MAX_DB_ARGS, MAX_INSTRS, MAX_LOCALS, MAX_PLANS};
use mpedb_types::{Error, Result, Value};

/// Call form an embedded SQL string was collected from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// `db.query(...)` — must compile to a read-only SELECT plan.
    Query,
    /// `db.execute(...)` — must compile to a DML plan.
    Exec,
    /// `db.rows(...)` (a streaming cursor) — must compile to a read-only
    /// SELECT plan, exactly like [`CallKind::Query`]; kept distinct so
    /// define-time errors name the form the user actually wrote.
    Rows,
}

/// One `db.query`/`db.execute` site: the literal SQL, the number of
/// argument expressions passed, and the source line/column (for define-time
/// errors when the SQL fails to compile against the live schema).
#[derive(Debug, Clone)]
pub struct SqlCall {
    pub sql: String,
    pub kind: CallKind,
    pub argc: u8,
    pub line: usize,
    pub col: usize,
}

/// Frontend output: everything but the plan hashes (which only exist after
/// the engine prepares the collected SQL against a live schema).
#[derive(Debug)]
pub struct Skeleton {
    pub name: String,
    pub argc: u16,
    pub nlocals: u16,
    pub consts: Vec<Value>,
    pub instrs: Vec<Op>,
    /// `DbQuery(i)`/`DbExec(i)` in `instrs` refers to `calls[i]`.
    pub calls: Vec<SqlCall>,
}

/// Compile error helper: all frontend rejections funnel through here so the
/// message shape stays uniform. Uses `Error::Unsupported` (the crate cannot
/// add error variants to `mpedb-types`); messages carry line/column.
pub fn cerr(lang: &str, line: usize, col: usize, msg: impl AsRef<str>) -> Error {
    Error::Unsupported(format!(
        "proc({lang}) compile error at line {line}, column {col}: {}",
        msg.as_ref()
    ))
}

/// 1-based line and 0-based column of a byte offset in `src`.
pub fn line_col(src: &str, pos: usize) -> (usize, usize) {
    let pos = pos.min(src.len());
    let before = &src[..pos];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = before
        .rfind('\n')
        .map_or(before.chars().count(), |nl| before[nl + 1..].chars().count());
    (line, col)
}

pub struct FuncBuilder {
    pub instrs: Vec<Op>,
    pub consts: Vec<Value>,
    pub calls: Vec<SqlCall>,
    nlocals: u16,
    /// Innermost-last loop contexts for break/continue.
    loops: Vec<LoopCtx>,
}

pub struct LoopCtx {
    /// Jump target of `continue` (the condition re-check).
    pub start: u32,
    /// `break` jumps recorded here, patched to the loop end.
    pub breaks: Vec<usize>,
}

impl FuncBuilder {
    #[allow(clippy::new_without_default)] // widened from pub(crate) by the M2 split
    pub fn new() -> FuncBuilder {
        FuncBuilder {
            instrs: Vec::new(),
            consts: Vec::new(),
            calls: Vec::new(),
            nlocals: 0,
            loops: Vec::new(),
        }
    }

    pub fn nlocals(&self) -> u16 {
        self.nlocals
    }

    /// Allocate a fresh local slot (never reused; scoping is the
    /// frontend's business).
    pub fn alloc_local(&mut self) -> Result<u16> {
        if self.nlocals as usize >= MAX_LOCALS {
            return Err(Error::Unsupported(format!(
                "proc: too many local variables (max {MAX_LOCALS})"
            )));
        }
        let slot = self.nlocals;
        self.nlocals += 1;
        Ok(slot)
    }

    /// Intern a constant (deduplicated by equality; NaN never matches
    /// itself, which merely costs a duplicate slot).
    pub fn const_idx(&mut self, v: Value) -> Result<u16> {
        if let Some(i) = self.consts.iter().position(|c| *c == v) {
            return Ok(i as u16);
        }
        if self.consts.len() >= MAX_CONSTS {
            return Err(Error::Unsupported(format!(
                "proc: too many constants (max {MAX_CONSTS})"
            )));
        }
        self.consts.push(v);
        Ok((self.consts.len() - 1) as u16)
    }

    /// Append an instruction, returning its index.
    pub fn emit(&mut self, op: Op) -> Result<usize> {
        if self.instrs.len() >= MAX_INSTRS {
            return Err(Error::Unsupported(format!(
                "proc: program too large (max {MAX_INSTRS} instructions)"
            )));
        }
        self.instrs.push(op);
        Ok(self.instrs.len() - 1)
    }

    pub fn here(&self) -> u32 {
        self.instrs.len() as u32
    }

    /// Emit a jump-family op with a placeholder target; patch later.
    pub fn emit_jump(&mut self, op: Op) -> Result<usize> {
        self.emit(op)
    }

    /// Point the jump at `at` to the *current* end of the program.
    pub fn patch_to_here(&mut self, at: usize) {
        let t = self.here();
        self.instrs[at] = match self.instrs[at] {
            Op::Jump(_) => Op::Jump(t),
            Op::JumpIfFalse(_) => Op::JumpIfFalse(t),
            Op::JumpIfTrue(_) => Op::JumpIfTrue(t),
            other => unreachable!("patching non-jump {other:?}"),
        };
    }

    /// Register a db call site; returns the plan-table index for the op.
    pub fn add_call(&mut self, call: SqlCall) -> Result<u16> {
        if call.argc as usize > MAX_DB_ARGS {
            return Err(Error::Unsupported(format!(
                "proc: too many SQL parameters (max {MAX_DB_ARGS})"
            )));
        }
        // Dedup identical (sql, kind) sites so a query in a loop body that
        // appears twice in source still yields one plan-table entry.
        if let Some(i) = self
            .calls
            .iter()
            .position(|c| c.sql == call.sql && c.kind == call.kind && c.argc == call.argc)
        {
            return Ok(i as u16);
        }
        if self.calls.len() >= MAX_PLANS {
            return Err(Error::Unsupported(format!(
                "proc: too many distinct SQL statements (max {MAX_PLANS})"
            )));
        }
        self.calls.push(call);
        Ok((self.calls.len() - 1) as u16)
    }

    // ------------------------------------------------------------ loops

    pub fn push_loop(&mut self, start: u32) {
        self.loops.push(LoopCtx {
            start,
            breaks: Vec::new(),
        });
    }

    /// Close the innermost loop, patching every `break` to land here.
    pub fn pop_loop(&mut self) {
        let ctx = self.loops.pop().expect("loop stack underflow is a frontend bug");
        for at in ctx.breaks {
            self.patch_to_here(at);
        }
    }

    pub fn emit_break(&mut self) -> Result<bool> {
        if self.loops.is_empty() {
            return Ok(false);
        }
        let at = self.emit(Op::Jump(u32::MAX))?;
        self.loops.last_mut().expect("checked").breaks.push(at);
        Ok(true)
    }

    pub fn emit_continue(&mut self) -> Result<bool> {
        let Some(ctx) = self.loops.last() else {
            return Ok(false);
        };
        let start = ctx.start;
        self.emit(Op::Jump(start))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_math() {
        let src = "abc\ndef\nxyz";
        assert_eq!(line_col(src, 0), (1, 0));
        assert_eq!(line_col(src, 2), (1, 2));
        assert_eq!(line_col(src, 4), (2, 0));
        assert_eq!(line_col(src, 9), (3, 1));
        assert_eq!(line_col(src, 999), (3, 3));
    }

    #[test]
    fn const_dedup() {
        let mut b = FuncBuilder::new();
        let i = b.const_idx(Value::Int(7)).unwrap();
        let j = b.const_idx(Value::Int(7)).unwrap();
        let k = b.const_idx(Value::Int(8)).unwrap();
        assert_eq!(i, j);
        assert_ne!(i, k);
    }
}
