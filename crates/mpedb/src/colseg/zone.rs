use super::*;

// ------------------------------------------------------- zone-map predicates

/// The comparison a zone map can reason about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

/// A predicate a block's zone map can decide WITHOUT decoding: `col OP k`,
/// over an integer column, where `k` is a folded constant or a query
/// parameter.
pub struct ZonePred {
    pub col: u16,
    pub op: Cmp,
    pub k: i64,
}

/// Recognize the whole filter as one integer comparison against a constant or
/// parameter — the shape a zone map can decide.
///
/// Deliberately narrow, and every restriction is a correctness one rather than
/// laziness:
/// - the program must be the ENTIRE filter, so nothing else can disqualify a
///   row a block-level "all pass" conclusion would then wave through;
/// - integers (and timestamps) only, because a float zone map is built with
///   comparisons NaN loses, so "every value passes" would not follow from it;
/// - the constant must be an integer of the same class, so no cross-type
///   coercion happens here that the row path would have done differently.
///
/// Anything else returns `None` and the ordinary filtered fold runs.
pub fn zone_predicate(prog: &mpedb_types::ExprProgram, params: &[Value]) -> Option<ZonePred> {
    use mpedb_types::Instr;
    let [a, b, c] = prog.instrs.as_slice() else {
        return None;
    };
    let op = match c {
        Instr::Lt => Cmp::Lt,
        Instr::Le => Cmp::Le,
        Instr::Gt => Cmp::Gt,
        Instr::Ge => Cmp::Ge,
        Instr::Eq => Cmp::Eq,
        _ => return None,
    };
    let operand = |i: &Instr| -> Option<i64> {
        match i {
            Instr::PushConst(x) => match prog.consts.get(*x as usize)? {
                Value::Int(v) | Value::Timestamp(v) => Some(*v),
                _ => None,
            },
            Instr::PushParam(x) => match params.get(*x as usize)? {
                Value::Int(v) | Value::Timestamp(v) => Some(*v),
                _ => None,
            },
            _ => None,
        }
    };
    match (a, b) {
        (Instr::PushCol(col), rhs) => Some(ZonePred { col: *col, op, k: operand(rhs)? }),
        // `1000 <= day_id` — the same fact with the operands swapped, so the
        // comparison must be mirrored, not merely reused.
        (lhs, Instr::PushCol(col)) => {
            let k = operand(lhs)?;
            let op = match op {
                Cmp::Lt => Cmp::Gt,
                Cmp::Le => Cmp::Ge,
                Cmp::Gt => Cmp::Lt,
                Cmp::Ge => Cmp::Le,
                Cmp::Eq => Cmp::Eq,
            };
            Some(ZonePred { col: *col, op, k })
        }
        _ => None,
    }
}

/// What a block's zone map says about a predicate, before any value is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// No row in the block can satisfy it — skip the block entirely.
    None,
    /// Every row satisfies it — take the block without testing.
    All,
    /// Some might; the block has to be read.
    Some,
}

pub fn zone_verdict(b: &Block<'_>, p: &ZonePred) -> Verdict {
    if b.n_rows == 0 {
        return Verdict::None;
    }
    let Some((lo, hi)) = b.int_bounds() else {
        // No integer bounds. Two reasons, and they differ: an INTEGER column
        // whose every row is NULL satisfies nothing (NULL passes no
        // comparison), while a non-integer column simply has to be read.
        return if b.is_int_column() {
            Verdict::None
        } else {
            Verdict::Some
        };
    };
    let (all, none) = match p.op {
        Cmp::Ge => (lo >= p.k, hi < p.k),
        Cmp::Gt => (lo > p.k, hi <= p.k),
        Cmp::Le => (hi <= p.k, lo > p.k),
        Cmp::Lt => (hi < p.k, lo >= p.k),
        Cmp::Eq => (lo == p.k && hi == p.k, p.k < lo || p.k > hi),
    };
    if none {
        // Sound with NULLs present too: a NULL satisfies no comparison, so if
        // no non-null value can pass, no row can.
        Verdict::None
    } else if all && b.null_free() {
        // "All" needs null-freeness: the bounds describe the non-null values
        // only, and a NULL would not have passed.
        Verdict::All
    } else {
        Verdict::Some
    }
}

