//! Compact stack-based expression IR (PySpell-style: compiled once at
//! prepare/attach, evaluated many times with no parsing or allocation-heavy
//! AST walking).
//!
//! Used for WHERE filters, projections with computed columns, and CHECK
//! constraints. Follows SQL three-valued logic: comparisons and arithmetic
//! with NULL yield NULL; AND/OR/NOT use Kleene logic; a filter passes only if
//! the result is exactly TRUE.

use crate::error::{Error, Result};
use crate::value::{Collation, ColumnType, Value};
use std::cmp::Ordering;

mod codec;
mod ops;
mod printf;
mod scalar;

#[cfg(test)]
mod in_list_tests;
#[cfg(test)]
mod jump_tests;
#[cfg(test)]
mod tests;

pub use scalar::ScalarFn;

use ops::{
    glob_match, in_items_3vl, in_items_3vl_collated, in_list_3vl, like_match, regexp_match,
};
use scalar::call_scalar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Instr {
    /// Push column value by index into the row.
    PushCol(u16),
    /// Push statement parameter ($1 = index 0).
    PushParam(u16),
    /// Push constant from the program's const pool.
    PushConst(u16),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    And,
    Or,
    Not,
    IsNull,
    IsNotNull,
    /// `a IS b` — NULL-safe equality ("is not distinct from"). Pops 2, pushes a
    /// Bool, and NEVER pushes NULL: TRUE when both operands are NULL, FALSE when
    /// exactly one is, otherwise `a = b`. Two-valued, unlike [`Instr::Eq`], which
    /// yields NULL on a NULL side. This is what sqlite's `IS` operator means.
    IsNotDistinct,
    /// `a IS NOT b` — NULL-safe inequality ("is distinct from"), the exact
    /// negation of [`Instr::IsNotDistinct`]. Also two-valued: never NULL.
    IsDistinct,
    /// Coerce Int -> Float (inserted by the binder for mixed numerics).
    ToFloat,
    /// `CAST(x AS <type>)` — SQL type conversion (#56). NULL casts to NULL of
    /// any type; numeric conversions follow sqlite (float→int truncates
    /// toward zero, which is also what the corpus expects); a conversion that
    /// would have to INVENT data (text→number) raises instead of
    /// prefix-parsing the way sqlite does — that is the strictness line.
    Cast(ColumnType),
    /// `a || b` — SQL concatenation. NULL propagates; ints and bools render
    /// as text first (sqlite's rule); floats are refused until someone needs
    /// their formatting pinned down.
    Concat,
    /// SQL LIKE with pattern from the const pool (supports % and _).
    Like(u16),
    /// SQL GLOB with pattern from the const pool. Like [`Instr::Like`] but
    /// case-SENSITIVE, and the wildcards are sqlite's `*` (any run), `?` (one
    /// char) and `[...]` character classes rather than `%`/`_`. Same operand
    /// typing and NULL rules: any NULL operand yields NULL.
    Glob(u16),
    /// SQL REGEXP with pattern from the const pool. Like [`Instr::Glob`] but the
    /// pattern is sqlite's bundled `ext/misc/regexp.c` dialect (`.`, `* + ?`,
    /// `{p,q}`, `[...]`, `^`/`$`, `|`, `(...)`, `\d`/`\w`/`\s`/`\b`, escapes) —
    /// case-SENSITIVE, unanchored substring match. `NOT REGEXP` is a `Not`
    /// wrapped around this by the binder. Same operand typing and NULL rules:
    /// any NULL operand yields NULL. See [`regexp_match`].
    Regexp(u16),
    /// `<scalar> IN (<list param n>)` — set membership against a
    /// [`Value::List`] bound to parameter `n` (design/DESIGN-MULTIDB.md §2.6).
    ///
    /// The list is a PARAM, not a const: that is the whole design. Arity lives
    /// in the data, so the plan bytes — and therefore the plan hash — stay
    /// independent of how many orgs a given session belongs to, and one
    /// compiled plan serves every session (§4.1). Baking the list into the
    /// const pool would mint a plan per distinct membership set.
    InParam(u16),
    /// `<scalar> IN (<e1>, …, <en>)` — set membership against `n` values taken
    /// from the STACK (general SQL `IN`, task #21). Pops `n` list elements plus
    /// the probe beneath them, pushes the 3VL verdict.
    ///
    /// The counterpart of [`Instr::InParam`], and the split is deliberate: this
    /// form's arity IS part of the query text, so encoding it in the plan is
    /// correct — `x IN (1,2)` and `x IN (1,2,3)` are different queries and
    /// should hash differently. `InParam` exists precisely because a session's
    /// membership set must NOT reach the plan bytes (§4.1). Same 3VL either way.
    ///
    /// Not desugared into an OR-chain even though the 3VL works out identically:
    /// that duplicates the probe expression `n` times in the plan, and the probe
    /// may be arbitrarily large.
    InList(u16),
    /// Jump to absolute instruction index `t` unless the popped value is
    /// exactly TRUE — i.e. jump on FALSE **and on NULL**.
    ///
    /// Named for what it does rather than `JumpIfFalse`, because the NULL case
    /// is the whole subtlety: `CASE WHEN <null> THEN a ELSE b END` yields `b`,
    /// so an unknown condition must not be taken. A `JumpIfFalse` that treated
    /// NULL as "not false" would silently take the branch.
    JumpIfNotTrue(u16),
    /// Unconditional jump to absolute instruction index `t`.
    Jump(u16),
    /// PEEK the top of the stack: jump to `t` if it is NOT NULL, leaving it in
    /// place; otherwise fall through, still leaving it (a following [`Instr::Pop`]
    /// discards it).
    ///
    /// This is what makes `coalesce` lazy. Eager evaluation would not just be a
    /// nicety here: mpedb raises on division by zero (PostgreSQL's behaviour,
    /// not sqlite's NULL), so an eager `coalesce(x, 1/0)` would ERROR where both
    /// sqlite and PostgreSQL return x. Being a strict engine is the point;
    /// being strict in a way neither ancestor is would just be a third dialect.
    JumpIfNotNull(u16),
    /// Discard the top of the stack.
    Pop,
    /// Call a scalar function over the top `argc` values (leftmost deepest),
    /// replacing them with one result.
    Call(ScalarFn, u8),
    /// A comparison under an explicit collating sequence (task: COLLATE). Pops
    /// two values and pushes the 3VL verdict, exactly like the plain
    /// [`Instr::Eq`]..[`Instr::Ge`] family, but TEXT operands are ordered by
    /// `Collation` instead of bytewise. The binder emits it ONLY when the
    /// resolved collation is non-`Binary` and the compared type is text — a
    /// `Binary` or numeric comparison stays a plain nullary opcode, so existing
    /// plan bytes are unchanged.
    CmpColl(CmpKind, Collation),
    /// `<scalar> IN (<e1>, …, <en>)` under an explicit collation — the collated
    /// twin of [`Instr::InList`]. Pops `n` list elements plus the probe beneath
    /// them; text membership is decided under `Collation`. Emitted only for a
    /// non-`Binary` collation on a text probe.
    InListColl(u16, Collation),
}

/// Which of the six SQL comparison operators a collated [`Instr::CmpColl`]
/// evaluates. A tiny closed enum so one collated opcode covers all six rather
/// than minting six; the plain uncollated comparisons stay their own nullary
/// opcodes for wire stability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CmpKind {
    Eq = 0,
    Ne = 1,
    Lt = 2,
    Le = 3,
    Gt = 4,
    Ge = 5,
}

impl CmpKind {
    pub fn from_tag(t: u8) -> Option<CmpKind> {
        Some(match t {
            0 => CmpKind::Eq,
            1 => CmpKind::Ne,
            2 => CmpKind::Lt,
            3 => CmpKind::Le,
            4 => CmpKind::Gt,
            5 => CmpKind::Ge,
            _ => return None,
        })
    }

    /// Map an ordering verdict to this operator's boolean result.
    fn eval(self, ord: Ordering) -> bool {
        match self {
            CmpKind::Eq => ord == Ordering::Equal,
            CmpKind::Ne => ord != Ordering::Equal,
            CmpKind::Lt => ord == Ordering::Less,
            CmpKind::Le => ord != Ordering::Greater,
            CmpKind::Gt => ord == Ordering::Greater,
            CmpKind::Ge => ord != Ordering::Less,
        }
    }

    /// The operator symbol, for EXPLAIN.
    pub fn symbol(self) -> &'static str {
        match self {
            CmpKind::Eq => "=",
            CmpKind::Ne => "<>",
            CmpKind::Lt => "<",
            CmpKind::Le => "<=",
            CmpKind::Gt => ">",
            CmpKind::Ge => ">=",
        }
    }
}

/// A compiled expression: instruction sequence + constant pool.
#[derive(Debug, Clone, PartialEq)]
pub struct ExprProgram {
    pub instrs: Vec<Instr>,
    pub consts: Vec<Value>,
    /// Maximum stack depth, proven at construction/decode time so `eval`
    /// needs no per-instruction underflow checks to be panic-free.
    max_stack: usize,
}

impl ExprProgram {
    /// Build a program, verifying stack discipline and const/index bounds.
    pub fn new(instrs: Vec<Instr>, consts: Vec<Value>) -> Result<ExprProgram> {
        let max_stack = codec::validate(&instrs, &consts)?;
        Ok(ExprProgram {
            instrs,
            consts,
            max_stack,
        })
    }

    pub fn max_stack(&self) -> usize {
        self.max_stack
    }

    /// Evaluate against a decoded row and statement parameters.
    pub fn eval(&self, cols: &[Value], params: &[Value]) -> Result<Value> {
        let mut stack: Vec<Value> = Vec::with_capacity(self.max_stack);
        self.eval_with_stack(&mut stack, cols, params)
    }

    /// Hot-path variant reusing a scratch stack across rows.
    pub fn eval_with_stack(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
    ) -> Result<Value> {
        stack.clear();
        stack.reserve(self.max_stack);
        // A program counter rather than an iterator: jumps (CASE) need to move
        // it. `validate` proved every target is forward and in range, so this
        // terminates and never indexes out of bounds.
        let mut pc = 0usize;
        while pc < self.instrs.len() {
            let instr = self.instrs[pc];
            pc += 1;
            match instr {
                Instr::PushCol(i) => {
                    let v = cols.get(i as usize).ok_or_else(|| {
                        Error::Internal(format!("column index {i} out of row bounds"))
                    })?;
                    stack.push(v.clone());
                }
                Instr::PushParam(i) => {
                    let v = params.get(i as usize).ok_or(Error::WrongParamCount {
                        expected: i as usize + 1,
                        got: params.len(),
                    })?;
                    stack.push(v.clone());
                }
                Instr::PushConst(i) => stack.push(self.consts[i as usize].clone()),
                Instr::Eq | Instr::Ne | Instr::Lt | Instr::Le | Instr::Gt | Instr::Ge => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match a.sql_cmp(&b)? {
                        None => Value::Null,
                        Some(ord) => Value::Bool(match instr {
                            Instr::Eq => ord == Ordering::Equal,
                            Instr::Ne => ord != Ordering::Equal,
                            Instr::Lt => ord == Ordering::Less,
                            Instr::Le => ord != Ordering::Greater,
                            Instr::Gt => ord == Ordering::Greater,
                            Instr::Ge => ord != Ordering::Less,
                            _ => unreachable!(),
                        }),
                    });
                }
                Instr::Add | Instr::Sub | Instr::Mul | Instr::Div | Instr::Mod => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(arith(instr, a, b)?);
                }
                Instr::Neg => {
                    let a = stack.pop().expect("validated");
                    stack.push(match a {
                        Value::Null => Value::Null,
                        Value::Int(x) => {
                            Value::Int(x.checked_neg().ok_or(Error::ArithmeticOverflow)?)
                        }
                        Value::Float(x) => Value::Float(-x),
                        v => {
                            return Err(Error::TypeMismatch(format!(
                                "cannot negate {}",
                                v.type_name()
                            )))
                        }
                    });
                }
                Instr::And | Instr::Or => {
                    let b = to_bool3(stack.pop().expect("validated"))?;
                    let a = to_bool3(stack.pop().expect("validated"))?;
                    stack.push(match instr {
                        // Kleene 3VL
                        Instr::And => match (a, b) {
                            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                            (Some(true), Some(true)) => Value::Bool(true),
                            _ => Value::Null,
                        },
                        _ => match (a, b) {
                            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
                            (Some(false), Some(false)) => Value::Bool(false),
                            _ => Value::Null,
                        },
                    });
                }
                Instr::Not => {
                    let a = to_bool3(stack.pop().expect("validated"))?;
                    stack.push(match a {
                        None => Value::Null,
                        Some(x) => Value::Bool(!x),
                    });
                }
                Instr::IsNull | Instr::IsNotNull => {
                    let a = stack.pop().expect("validated");
                    let is_null = a.is_null();
                    stack.push(Value::Bool(if instr == Instr::IsNull {
                        is_null
                    } else {
                        !is_null
                    }));
                }
                Instr::IsNotDistinct | Instr::IsDistinct => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    // NULL-safe: two NULLs MATCH, one NULL does not, otherwise
                    // compare. This NEVER produces NULL — the whole point of IS,
                    // and why it is 2-valued rather than 3VL like `=`.
                    let same = match (a.is_null(), b.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => matches!(a.sql_cmp(&b)?, Some(Ordering::Equal)),
                    };
                    stack.push(Value::Bool(if instr == Instr::IsNotDistinct {
                        same
                    } else {
                        !same
                    }));
                }
                Instr::ToFloat => {
                    let a = stack.pop().expect("validated");
                    stack.push(match a {
                        Value::Null => Value::Null,
                        Value::Int(x) => Value::Float(x as f64),
                        Value::Float(x) => Value::Float(x),
                        v => {
                            return Err(Error::TypeMismatch(format!(
                                "cannot cast {} to float",
                                v.type_name()
                            )))
                        }
                    });
                }
                Instr::Cast(t) => {
                    let a = stack.pop().expect("validated");
                    stack.push(cast_value(a, t)?);
                }
                Instr::Concat => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(concat_value(a, b)?);
                }
                Instr::Like(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(like_match(p, s)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::Glob(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(glob_match(p, s)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "GLOB requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::Regexp(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(regexp_match(p, s)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "REGEXP requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::InParam(pi) => {
                    let probe = stack.pop().expect("validated");
                    let list = params.get(pi as usize).ok_or_else(|| {
                        Error::Corrupt("IN list parameter index out of range".into())
                    })?;
                    stack.push(in_list_3vl(&probe, list)?);
                }
                Instr::InList(n) => {
                    // The verifier proved depth >= n+1, so both splits are safe.
                    let at = stack.len() - n as usize;
                    let items: Vec<Value> = stack.split_off(at);
                    let probe = stack.pop().expect("validated");
                    stack.push(in_items_3vl(&probe, &items)?);
                }
                Instr::JumpIfNotTrue(t) => {
                    // Jump on FALSE *and* on NULL: an unknown WHEN must not be
                    // taken, or `CASE WHEN <null> THEN a ELSE b END` yields a.
                    let c = stack.pop().expect("validated");
                    if !matches!(c, Value::Bool(true)) {
                        // A non-bool condition is a bind-time error, but a
                        // hand-built program can still get here; treating it as
                        // "not true" is the safe reading and matches to_bool3's
                        // refusal to invent a truth value.
                        if !matches!(c, Value::Bool(false) | Value::Null) {
                            return Err(Error::TypeMismatch(format!(
                                "CASE condition must be bool, got {}",
                                c.type_name()
                            )));
                        }
                        pc = t as usize;
                    }
                }
                Instr::Jump(t) => pc = t as usize,
                Instr::JumpIfNotNull(t) => {
                    // PEEK, do not pop: on the taken path the value IS the
                    // result, so popping it would throw away what we jumped for.
                    if !stack.last().expect("validated").is_null() {
                        pc = t as usize;
                    }
                }
                Instr::Pop => {
                    stack.pop().expect("validated");
                }
                Instr::Call(f, argc) => {
                    // validate() proved depth >= argc and that argc is legal for
                    // this function, so the split and the indexing below hold.
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    stack.push(call_scalar(f, &args)?);
                }
                Instr::CmpColl(kind, coll) => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match a.sql_cmp_collated(&b, coll)? {
                        None => Value::Null,
                        Some(ord) => Value::Bool(kind.eval(ord)),
                    });
                }
                Instr::InListColl(n, coll) => {
                    let at = stack.len() - n as usize;
                    let items: Vec<Value> = stack.split_off(at);
                    let probe = stack.pop().expect("validated");
                    stack.push(in_items_3vl_collated(&probe, &items, coll)?);
                }
            }
        }
        Ok(stack.pop().expect("validated: exactly one result"))
    }

    /// Evaluate as a WHERE/CHECK predicate: passes only on exactly TRUE.
    pub fn eval_filter(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
    ) -> Result<bool> {
        match self.eval_with_stack(stack, cols, params)? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            v => Err(Error::TypeMismatch(format!(
                "predicate evaluated to {}, expected bool",
                v.type_name()
            ))),
        }
    }
}

fn to_bool3(v: Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(b)),
        v => Err(Error::TypeMismatch(format!(
            "expected bool, got {}",
            v.type_name()
        ))),
    }
}

fn arith(op: Instr, a: Value, b: Value) -> Result<Value> {
    use Value::*;
    match (&a, &b) {
        (Null, _) | (_, Null) => return Ok(Null),
        _ => {}
    }
    match (a, b) {
        (Int(x), Int(y)) => Ok(Int(match op {
            Instr::Add => x.checked_add(y).ok_or(Error::ArithmeticOverflow)?,
            Instr::Sub => x.checked_sub(y).ok_or(Error::ArithmeticOverflow)?,
            Instr::Mul => x.checked_mul(y).ok_or(Error::ArithmeticOverflow)?,
            Instr::Div => {
                if y == 0 {
                    return Err(Error::DivisionByZero);
                }
                x.checked_div(y).ok_or(Error::ArithmeticOverflow)?
            }
            Instr::Mod => {
                if y == 0 {
                    return Err(Error::DivisionByZero);
                }
                x.checked_rem(y).ok_or(Error::ArithmeticOverflow)?
            }
            _ => unreachable!(),
        })),
        (Float(x), Float(y)) => Ok(Float(match op {
            Instr::Add => x + y,
            Instr::Sub => x - y,
            Instr::Mul => x * y,
            Instr::Div => {
                if y == 0.0 {
                    return Err(Error::DivisionByZero);
                }
                x / y
            }
            Instr::Mod => {
                if y == 0.0 {
                    return Err(Error::DivisionByZero);
                }
                x % y
            }
            _ => unreachable!(),
        })),
        (a, b) => Err(Error::TypeMismatch(format!(
            "arithmetic on {} and {} (binder should have coerced)",
            a.type_name(),
            b.type_name()
        ))),
    }
}


/// `CAST` semantics (#56): NULL → NULL of any type; lossy numeric narrowing
/// follows sqlite (truncate toward zero — Rust's saturating `as`, which also
/// makes NaN/±inf deterministic); text→number is REFUSED rather than
/// prefix-parsed. Timestamps and blobs only cast to themselves.
fn cast_value(v: Value, t: ColumnType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (v, t) {
        (v, ColumnType::Any) => v,
        (Value::Int(i), ColumnType::Int64) => Value::Int(i),
        (Value::Float(f), ColumnType::Int64) => Value::Int(f as i64),
        (Value::Bool(b), ColumnType::Int64) => Value::Int(b as i64),
        (Value::Int(i), ColumnType::Float64) => Value::Float(i as f64),
        (Value::Float(f), ColumnType::Float64) => Value::Float(f),
        (Value::Bool(b), ColumnType::Float64) => Value::Float(b as u8 as f64),
        (Value::Text(s), ColumnType::Text) => Value::Text(s),
        (Value::Int(i), ColumnType::Text) => Value::Text(i.to_string()),
        (Value::Bool(b), ColumnType::Text) => Value::Text((b as i64).to_string()),
        (Value::Bool(b), ColumnType::Bool) => Value::Bool(b),
        (Value::Int(i), ColumnType::Bool) => Value::Bool(i != 0),
        (Value::Blob(b), ColumnType::Blob) => Value::Blob(b),
        (Value::Timestamp(u), ColumnType::Timestamp) => Value::Timestamp(u),
        (v, t) => {
            return Err(Error::TypeMismatch(format!(
                "CAST from {} to {t} would have to invent data",
                v.type_name()
            )))
        }
    })
}

/// `||` semantics: NULL propagates; ints and bools render as text (sqlite's
/// rule); floats are refused until their text formatting is pinned down.
fn concat_value(a: Value, b: Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let as_text = |v: Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s),
            Value::Int(i) => Ok(i.to_string()),
            Value::Bool(b) => Ok((b as i64).to_string()),
            v => Err(Error::TypeMismatch(format!(
                "|| cannot render {} as text",
                v.type_name()
            ))),
        }
    };
    let mut s = as_text(a)?;
    s.push_str(&as_text(b)?);
    Ok(Value::Text(s))
}
