//! Compact stack-based expression IR (PySpell-style: compiled once at
//! prepare/attach, evaluated many times with no parsing or allocation-heavy
//! AST walking).
//!
//! Used for WHERE filters, projections with computed columns, and CHECK
//! constraints. Follows SQL three-valued logic: comparisons and arithmetic
//! with NULL yield NULL; AND/OR/NOT use Kleene logic; a filter passes only if
//! the result is exactly TRUE.

use crate::error::{Error, Result};
use crate::value::{read_value, write_value, Value};
use std::cmp::Ordering;

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
    /// Coerce Int -> Float (inserted by the binder for mixed numerics).
    ToFloat,
    /// SQL LIKE with pattern from the const pool (supports % and _).
    Like(u16),
}

const OP_PUSH_COL: u8 = 1;
const OP_PUSH_PARAM: u8 = 2;
const OP_PUSH_CONST: u8 = 3;
const OP_EQ: u8 = 4;
const OP_NE: u8 = 5;
const OP_LT: u8 = 6;
const OP_LE: u8 = 7;
const OP_GT: u8 = 8;
const OP_GE: u8 = 9;
const OP_ADD: u8 = 10;
const OP_SUB: u8 = 11;
const OP_MUL: u8 = 12;
const OP_DIV: u8 = 13;
const OP_MOD: u8 = 14;
const OP_NEG: u8 = 15;
const OP_AND: u8 = 16;
const OP_OR: u8 = 17;
const OP_NOT: u8 = 18;
const OP_IS_NULL: u8 = 19;
const OP_IS_NOT_NULL: u8 = 20;
const OP_TO_FLOAT: u8 = 21;
const OP_LIKE: u8 = 22;

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
        let max_stack = validate(&instrs, &consts)?;
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
        for &instr in &self.instrs {
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

    /// Deterministic serialization (part of plan blobs and plan hashing).
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.consts.len() as u16).to_le_bytes());
        for c in &self.consts {
            write_value(buf, c);
        }
        buf.extend_from_slice(&(self.instrs.len() as u32).to_le_bytes());
        for &i in &self.instrs {
            match i {
                Instr::PushCol(x) => {
                    buf.push(OP_PUSH_COL);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::PushParam(x) => {
                    buf.push(OP_PUSH_PARAM);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::PushConst(x) => {
                    buf.push(OP_PUSH_CONST);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Like(x) => {
                    buf.push(OP_LIKE);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Eq => buf.push(OP_EQ),
                Instr::Ne => buf.push(OP_NE),
                Instr::Lt => buf.push(OP_LT),
                Instr::Le => buf.push(OP_LE),
                Instr::Gt => buf.push(OP_GT),
                Instr::Ge => buf.push(OP_GE),
                Instr::Add => buf.push(OP_ADD),
                Instr::Sub => buf.push(OP_SUB),
                Instr::Mul => buf.push(OP_MUL),
                Instr::Div => buf.push(OP_DIV),
                Instr::Mod => buf.push(OP_MOD),
                Instr::Neg => buf.push(OP_NEG),
                Instr::And => buf.push(OP_AND),
                Instr::Or => buf.push(OP_OR),
                Instr::Not => buf.push(OP_NOT),
                Instr::IsNull => buf.push(OP_IS_NULL),
                Instr::IsNotNull => buf.push(OP_IS_NOT_NULL),
                Instr::ToFloat => buf.push(OP_TO_FLOAT),
            }
        }
    }

    /// Bounds-checked decode; re-validates stack discipline so a corrupt or
    /// hostile plan blob cannot cause memory unsafety or panics at eval time.
    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<ExprProgram> {
        let err = || Error::Corrupt("truncated expression program".into());
        let nconsts = {
            let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
            *pos += 2;
            u16::from_le_bytes(raw.try_into().unwrap()) as usize
        };
        let mut consts = Vec::with_capacity(nconsts.min(1024));
        for _ in 0..nconsts {
            consts.push(read_value(buf, pos)?);
        }
        let ninstrs = {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            u32::from_le_bytes(raw.try_into().unwrap()) as usize
        };
        if ninstrs > 1 << 20 {
            return Err(Error::Corrupt("expression too large".into()));
        }
        let mut instrs = Vec::with_capacity(ninstrs.min(4096));
        for _ in 0..ninstrs {
            let op = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            let mut read_u16_arg = || -> Result<u16> {
                let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
                *pos += 2;
                Ok(u16::from_le_bytes(raw.try_into().unwrap()))
            };
            instrs.push(match op {
                OP_PUSH_COL => Instr::PushCol(read_u16_arg()?),
                OP_PUSH_PARAM => Instr::PushParam(read_u16_arg()?),
                OP_PUSH_CONST => Instr::PushConst(read_u16_arg()?),
                OP_LIKE => Instr::Like(read_u16_arg()?),
                OP_EQ => Instr::Eq,
                OP_NE => Instr::Ne,
                OP_LT => Instr::Lt,
                OP_LE => Instr::Le,
                OP_GT => Instr::Gt,
                OP_GE => Instr::Ge,
                OP_ADD => Instr::Add,
                OP_SUB => Instr::Sub,
                OP_MUL => Instr::Mul,
                OP_DIV => Instr::Div,
                OP_MOD => Instr::Mod,
                OP_NEG => Instr::Neg,
                OP_AND => Instr::And,
                OP_OR => Instr::Or,
                OP_NOT => Instr::Not,
                OP_IS_NULL => Instr::IsNull,
                OP_IS_NOT_NULL => Instr::IsNotNull,
                OP_TO_FLOAT => Instr::ToFloat,
                _ => return Err(Error::Corrupt(format!("invalid opcode {op}"))),
            });
        }
        ExprProgram::new(instrs, consts)
    }
}

/// Static verification: const indices in range, stack never underflows, and
/// the program leaves exactly one value. Returns the maximum stack depth.
fn validate(instrs: &[Instr], consts: &[Value]) -> Result<usize> {
    if instrs.is_empty() {
        return Err(Error::Corrupt("empty expression program".into()));
    }
    let mut depth: usize = 0;
    let mut max = 0usize;
    for &i in instrs {
        let (pops, pushes) = match i {
            Instr::PushCol(_) | Instr::PushParam(_) => (0, 1),
            Instr::PushConst(c) => {
                if c as usize >= consts.len() {
                    return Err(Error::Corrupt("const index out of range".into()));
                }
                (0, 1)
            }
            Instr::Like(c) => {
                if c as usize >= consts.len() {
                    return Err(Error::Corrupt("const index out of range".into()));
                }
                (1, 1)
            }
            Instr::Neg | Instr::Not | Instr::IsNull | Instr::IsNotNull | Instr::ToFloat => (1, 1),
            _ => (2, 1),
        };
        if depth < pops {
            return Err(Error::Corrupt("expression stack underflow".into()));
        }
        depth = depth - pops + pushes;
        max = max.max(depth);
    }
    if depth != 1 {
        return Err(Error::Corrupt(
            "expression program must leave exactly one value".into(),
        ));
    }
    Ok(max)
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

/// SQL LIKE: `%` matches any run, `_` matches one char. Iterative
/// two-pointer algorithm — O(n·m) worst case, no recursion, no regex dep.
fn like_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);
    while ti < t.len() {
        // The wildcard branch MUST precede the literal branch: a literal '%'
        // in the SUBJECT would otherwise consume the pattern's '%' as a
        // one-character match ('a%c' LIKE 'a%' must be TRUE).
        if pi < p.len() && p[pi] == '%' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            star_ti += 1;
            pi = star_pi + 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(instrs: Vec<Instr>, consts: Vec<Value>) -> ExprProgram {
        ExprProgram::new(instrs, consts).unwrap()
    }

    #[test]
    fn check_constraint_age_range() {
        // age >= 0 AND age < 200
        let p = prog(
            vec![
                Instr::PushCol(0),
                Instr::PushConst(0),
                Instr::Ge,
                Instr::PushCol(0),
                Instr::PushConst(1),
                Instr::Lt,
                Instr::And,
            ],
            vec![Value::Int(0), Value::Int(200)],
        );
        let mut stack = Vec::new();
        assert!(p.eval_filter(&mut stack, &[Value::Int(42)], &[]).unwrap());
        assert!(!p.eval_filter(&mut stack, &[Value::Int(-1)], &[]).unwrap());
        assert!(!p.eval_filter(&mut stack, &[Value::Int(200)], &[]).unwrap());
        // NULL age: predicate is NULL -> does not pass
        assert!(!p.eval_filter(&mut stack, &[Value::Null], &[]).unwrap());
    }

    #[test]
    fn three_valued_logic() {
        // NULL OR true = true ; NULL AND true = NULL ; NOT NULL = NULL
        let or = prog(
            vec![Instr::PushCol(0), Instr::PushConst(0), Instr::Or],
            vec![Value::Bool(true)],
        );
        assert_eq!(or.eval(&[Value::Null], &[]).unwrap(), Value::Bool(true));
        let and = prog(
            vec![Instr::PushCol(0), Instr::PushConst(0), Instr::And],
            vec![Value::Bool(true)],
        );
        assert_eq!(and.eval(&[Value::Null], &[]).unwrap(), Value::Null);
        let not = prog(vec![Instr::PushCol(0), Instr::Not], vec![]);
        assert_eq!(not.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    }

    #[test]
    fn params_and_arith() {
        // $1 + 10 = col0
        let p = prog(
            vec![
                Instr::PushParam(0),
                Instr::PushConst(0),
                Instr::Add,
                Instr::PushCol(0),
                Instr::Eq,
            ],
            vec![Value::Int(10)],
        );
        assert_eq!(
            p.eval(&[Value::Int(52)], &[Value::Int(42)]).unwrap(),
            Value::Bool(true)
        );
        assert!(matches!(
            p.eval(&[Value::Int(52)], &[]),
            Err(Error::WrongParamCount { .. })
        ));
        assert!(matches!(
            prog(
                vec![Instr::PushConst(0), Instr::PushConst(0), Instr::Div],
                vec![Value::Int(0)]
            )
            .eval(&[], &[]),
            Err(Error::DivisionByZero)
        ));
    }

    #[test]
    fn like_patterns() {
        assert!(like_match("he%o", "hello"));
        assert!(like_match("%", ""));
        assert!(like_match("h_llo", "hallo"));
        assert!(!like_match("h_llo", "hllo"));
        assert!(like_match("%abc", "xxabc"));
        assert!(!like_match("abc%", "xabc"));
        assert!(like_match("a%b%c", "a123b456c"));
        // literal '%' in the subject must not consume the wildcard
        assert!(like_match("%", "%%"));
        assert!(like_match("a%", "a%c"));
        assert!(like_match("%c", "a%c"));
        assert!(like_match("a%c", "a%c"));
    }

    #[test]
    fn rejects_malformed_programs() {
        assert!(ExprProgram::new(vec![Instr::Eq], vec![]).is_err()); // underflow
        assert!(ExprProgram::new(vec![], vec![]).is_err()); // empty
        assert!(ExprProgram::new(
            vec![Instr::PushConst(0), Instr::PushConst(1)],
            vec![Value::Int(1), Value::Int(2)]
        )
        .is_err()); // two results
        assert!(ExprProgram::new(vec![Instr::PushConst(5)], vec![]).is_err()); // bad const
    }

    #[test]
    fn encode_decode_roundtrip_and_corrupt_safety() {
        let p = prog(
            vec![
                Instr::PushCol(3),
                Instr::Like(0),
                Instr::PushParam(1),
                Instr::And,
            ],
            vec![Value::Text("a%".into())],
        );
        let mut buf = Vec::new();
        p.encode_into(&mut buf);
        let mut pos = 0;
        let q = ExprProgram::decode(&buf, &mut pos).unwrap();
        assert_eq!(p, q);
        assert_eq!(pos, buf.len());
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
    }
}
