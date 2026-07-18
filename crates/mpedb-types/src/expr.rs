//! Compact stack-based expression IR (PySpell-style: compiled once at
//! prepare/attach, evaluated many times with no parsing or allocation-heavy
//! AST walking).
//!
//! Used for WHERE filters, projections with computed columns, and CHECK
//! constraints. Follows SQL three-valued logic: comparisons and arithmetic
//! with NULL yield NULL; AND/OR/NOT use Kleene logic; a filter passes only if
//! the result is exactly TRUE.

use crate::value::ColumnType;
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
    /// `<scalar> IN (<list param n>)` — set membership against a
    /// [`Value::List`] bound to parameter `n` (DESIGN-MULTIDB.md §2.6).
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
}

/// The built-in scalar functions. Deliberately a closed enum rather than a name
/// lookup: the id is what goes in the plan bytes, so it must be stable and
/// exhaustively decodable — an unknown id is [`Error::Corrupt`], never a
/// silently-missing function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScalarFn {
    Lower = 1,
    Upper = 2,
    Length = 3,
    Trim = 4,
    Abs = 5,
    Round = 6,
    Substr = 7,
    Replace = 8,
    Ltrim = 9,
    Rtrim = 10,
    Instr = 11,
    Sqrt = 12,
    Pow = 13,
    Sign = 14,
    Ceil = 15,
    Floor = 16,
}

impl ScalarFn {
    fn from_tag(t: u8) -> Result<ScalarFn> {
        Ok(match t {
            1 => ScalarFn::Lower,
            2 => ScalarFn::Upper,
            3 => ScalarFn::Length,
            4 => ScalarFn::Trim,
            5 => ScalarFn::Abs,
            6 => ScalarFn::Round,
            7 => ScalarFn::Substr,
            8 => ScalarFn::Replace,
            9 => ScalarFn::Ltrim,
            10 => ScalarFn::Rtrim,
            11 => ScalarFn::Instr,
            12 => ScalarFn::Sqrt,
            13 => ScalarFn::Pow,
            14 => ScalarFn::Sign,
            15 => ScalarFn::Ceil,
            16 => ScalarFn::Floor,
            other => return Err(Error::Corrupt(format!("unknown scalar function {other}"))),
        })
    }

    /// Allowed argument counts. Checked at verify time so `eval` can index the
    /// popped args without re-checking.
    pub fn arity_ok(self, argc: u8) -> bool {
        match self {
            ScalarFn::Lower | ScalarFn::Upper | ScalarFn::Length | ScalarFn::Trim
            | ScalarFn::Abs => argc == 1,
            ScalarFn::Round | ScalarFn::Ltrim | ScalarFn::Rtrim => argc == 1 || argc == 2,
            ScalarFn::Sqrt | ScalarFn::Sign | ScalarFn::Ceil | ScalarFn::Floor => argc == 1,
            ScalarFn::Substr => argc == 2 || argc == 3,
            ScalarFn::Instr | ScalarFn::Pow => argc == 2,
            ScalarFn::Replace => argc == 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ScalarFn::Lower => "lower",
            ScalarFn::Upper => "upper",
            ScalarFn::Length => "length",
            ScalarFn::Trim => "trim",
            ScalarFn::Abs => "abs",
            ScalarFn::Round => "round",
            ScalarFn::Substr => "substr",
            ScalarFn::Replace => "replace",
            ScalarFn::Ltrim => "ltrim",
            ScalarFn::Rtrim => "rtrim",
            ScalarFn::Instr => "instr",
            ScalarFn::Sqrt => "sqrt",
            ScalarFn::Pow => "pow",
            ScalarFn::Sign => "sign",
            ScalarFn::Ceil => "ceil",
            ScalarFn::Floor => "floor",
        }
    }
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
const OP_IN_PARAM: u8 = 23;
const OP_IN_LIST: u8 = 24;
const OP_JUMP_IF_NOT_TRUE: u8 = 25;
const OP_JUMP: u8 = 26;
const OP_JUMP_IF_NOT_NULL: u8 = 27;
const OP_POP: u8 = 28;
const OP_CALL: u8 = 29;
const OP_CAST: u8 = 30;
const OP_CONCAT: u8 = 31;
const OP_IS_NOT_DISTINCT: u8 = 32;
const OP_IS_DISTINCT: u8 = 33;
const OP_GLOB: u8 = 34;

/// SQL `x IN (…)` under three-valued logic — the semantics that decide whether
/// a policy admits a row, so they are spelled out rather than approximated:
///
/// | case | result | why |
/// |---|---|---|
/// | `x` is NULL | **NULL** | never TRUE; a filter needs exactly TRUE, so the row stays invisible |
/// | `x` equals some element | **TRUE** | a match wins even if other elements are NULL — which is why the NULL scan cannot come first |
/// | no match, some element NULL | **NULL** | the NULL *might* have been the match; SQL refuses to say FALSE |
/// | no match, no NULL elements | **FALSE** | |
/// | empty list | **FALSE** | nothing to match, and NOT NULL: an empty membership set means "belongs to nothing" and must deny cleanly |
///
/// The `IS NOT DISTINCT FROM` reading (NULL matching NULL) is deliberately NOT
/// used: standard `IN` compares with `=`, and a context list containing NULL
/// must not silently make NULL-keyed rows visible.
fn in_list_3vl(probe: &Value, list: &Value) -> Result<Value> {
    let items = match list {
        Value::List(items) => items,
        Value::Null => {
            // The whole set is NULL (e.g. an unset context key bound to NULL):
            // membership in an unknown set is unknown.
            return Ok(Value::Null);
        }
        other => {
            return Err(Error::TypeMismatch(format!(
                "IN expects a context list, got {}",
                other.type_name()
            )))
        }
    };
    in_items_3vl(probe, items)
}

/// The 3VL core, over items from anywhere. Shared by [`Instr::InParam`] (items
/// from a param-bound list) and [`Instr::InList`] (items from the stack) so the
/// two forms cannot drift apart on the NULL rules above — which decide whether
/// a policy admits a row.
fn in_items_3vl(probe: &Value, items: &[Value]) -> Result<Value> {
    // `x IN ()` — membership in the EMPTY set — is FALSE for any `x`, NULL
    // included: nothing is a member of nothing (SQL 3VL). This MUST precede the
    // null-probe short-circuit below, or `NULL IN (<empty subquery>)` wrongly
    // yields NULL where sqlite/PostgreSQL give 0. Fires for a literal `IN ()`
    // (a zero-element `InList`), an empty subquery, and an empty context list
    // alike — all three share this core.
    if items.is_empty() {
        return Ok(Value::Bool(false));
    }
    if probe.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for it in items {
        if it.is_null() {
            saw_null = true;
            continue;
        }
        // Type mismatches inside the list are the caller's error, not a silent
        // non-match: `org_id IN (list-of-text)` must not quietly deny every row.
        match probe.sql_cmp(it)? {
            Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
            _ => continue,
        }
    }
    Ok(if saw_null { Value::Null } else { Value::Bool(false) })
}

/// Evaluate a scalar function. `validate` already proved the arity, so the
/// indexing here is total.
///
/// Every one of these is NULL-propagating: any NULL argument yields NULL,
/// without looking at the others. That is the SQL rule, and it is why the
/// null-tolerant functions (`coalesce`, `nullif`) are NOT here — they are
/// compiled to control flow instead, precisely because they must NOT propagate.
fn call_scalar(f: ScalarFn, args: &[Value]) -> Result<Value> {
    if args.iter().any(|a| a.is_null()) {
        return Ok(Value::Null);
    }
    let text = |v: &Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects text, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    let int = |v: &Value| -> Result<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects an integer, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    let num = |v: &Value| -> Result<f64> {
        match v {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(x) => Ok(*x),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects a number, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    // A math result that is NaN (e.g. sqrt of a negative) is SQL NULL, matching
    // sqlite — never a NaN handed back into a typed column.
    let float_or_null = |r: f64| if r.is_nan() { Value::Null } else { Value::Float(r) };
    Ok(match f {
        ScalarFn::Lower => Value::Text(text(&args[0])?.to_lowercase()),
        ScalarFn::Upper => Value::Text(text(&args[0])?.to_uppercase()),
        // CHARACTERS, not bytes: `length('æ')` is 1. A byte count would be a
        // silent wrong answer for every non-ASCII string, which is most of the
        // strings in this author's part of the world.
        ScalarFn::Length => Value::Int(text(&args[0])?.chars().count() as i64),
        ScalarFn::Trim => Value::Text(text(&args[0])?.trim().to_string()),
        ScalarFn::Abs => match &args[0] {
            // i64::MIN has no positive counterpart: negating it overflows and
            // would panic in debug and silently wrap in release.
            Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| {
                Error::TypeMismatch("abs(): integer overflow (i64::MIN has no absolute value)".into())
            })?),
            Value::Float(x) => Value::Float(x.abs()),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "abs() expects a number, got {}",
                    other.type_name()
                )))
            }
        },
        ScalarFn::Round => {
            let digits = if args.len() == 2 { int(&args[1])? } else { 0 };
            match &args[0] {
                // Rounding an integer is the integer, at any digit count.
                Value::Int(i) => Value::Int(*i),
                Value::Float(x) => {
                    let p = 10f64.powi(digits.clamp(-15, 15) as i32);
                    Value::Float((x * p).round() / p)
                }
                other => {
                    return Err(Error::TypeMismatch(format!(
                        "round() expects a number, got {}",
                        other.type_name()
                    )))
                }
            }
        }
        ScalarFn::Substr => {
            // sqlite/PostgreSQL agree here: 1-based, and a start below 1 counts
            // toward the string rather than erroring.
            let s: Vec<char> = text(&args[0])?.chars().collect();
            let start = int(&args[1])?;
            let begin = if start < 1 { 0usize } else { (start - 1) as usize };
            let end = match args.len() {
                3 => {
                    let n = int(&args[2])?;
                    if n <= 0 {
                        begin
                    } else {
                        // saturating: begin+n can exceed usize on a hostile plan
                        begin.saturating_add(n as usize).min(s.len())
                    }
                }
                _ => s.len(),
            };
            let begin = begin.min(s.len());
            let end = end.max(begin).min(s.len());
            Value::Text(s[begin..end].iter().collect())
        }
        ScalarFn::Replace => {
            let s = text(&args[0])?;
            let from = text(&args[1])?;
            let to = text(&args[2])?;
            // sqlite: an empty search string leaves the input unchanged (Rust's
            // `str::replace("")` would instead splice `to` between every char).
            if from.is_empty() {
                Value::Text(s)
            } else {
                Value::Text(s.replace(&from, &to))
            }
        }
        ScalarFn::Ltrim => {
            let s = text(&args[0])?;
            match args.get(1) {
                Some(_) => {
                    let set: Vec<char> = text(&args[1])?.chars().collect();
                    Value::Text(s.trim_start_matches(|c| set.contains(&c)).to_string())
                }
                None => Value::Text(s.trim_start().to_string()),
            }
        }
        ScalarFn::Rtrim => {
            let s = text(&args[0])?;
            match args.get(1) {
                Some(_) => {
                    let set: Vec<char> = text(&args[1])?.chars().collect();
                    Value::Text(s.trim_end_matches(|c| set.contains(&c)).to_string())
                }
                None => Value::Text(s.trim_end().to_string()),
            }
        }
        ScalarFn::Instr => {
            // 1-based character position of the first occurrence of the needle,
            // 0 when absent; an empty needle is at position 1 (sqlite's rule).
            let hay: Vec<char> = text(&args[0])?.chars().collect();
            let needle: Vec<char> = text(&args[1])?.chars().collect();
            let pos = if needle.is_empty() {
                1
            } else if needle.len() > hay.len() {
                0
            } else {
                (0..=hay.len() - needle.len())
                    .find(|&i| hay[i..i + needle.len()] == needle[..])
                    .map_or(0, |i| i as i64 + 1)
            };
            Value::Int(pos)
        }
        // sqrt of a negative and pow with a non-real result are NULL (sqlite),
        // and both always return a float regardless of the argument types.
        ScalarFn::Sqrt => float_or_null(num(&args[0])?.sqrt()),
        ScalarFn::Pow => float_or_null(num(&args[0])?.powf(num(&args[1])?)),
        ScalarFn::Sign => match &args[0] {
            Value::Int(i) => Value::Int(i.signum()),
            Value::Float(x) => Value::Int(if *x > 0.0 {
                1
            } else if *x < 0.0 {
                -1
            } else {
                0 // covers +0.0, -0.0, and (unreachable here) NaN
            }),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "sign() expects a number, got {}",
                    other.type_name()
                )))
            }
        },
        // ceil/floor preserve the argument's type (sqlite: an integer stays an
        // integer at any value; a float rounds toward +/-inf as a float).
        ScalarFn::Ceil | ScalarFn::Floor => match &args[0] {
            Value::Int(i) => Value::Int(*i),
            Value::Float(x) => Value::Float(if matches!(f, ScalarFn::Ceil) {
                x.ceil()
            } else {
                x.floor()
            }),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "{}() expects a number, got {}",
                    f.name(),
                    other.type_name()
                )))
            }
        },
    })
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
                Instr::Glob(x) => {
                    buf.push(OP_GLOB);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::InList(x) => {
                    buf.push(OP_IN_LIST);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::JumpIfNotTrue(x) => {
                    buf.push(OP_JUMP_IF_NOT_TRUE);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Jump(x) => {
                    buf.push(OP_JUMP);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::JumpIfNotNull(x) => {
                    buf.push(OP_JUMP_IF_NOT_NULL);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Pop => buf.push(OP_POP),
                Instr::Call(f, argc) => {
                    buf.push(OP_CALL);
                    buf.push(f as u8);
                    buf.push(argc);
                }
                Instr::InParam(x) => {
                    buf.push(OP_IN_PARAM);
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
                Instr::IsNotDistinct => buf.push(OP_IS_NOT_DISTINCT),
                Instr::IsDistinct => buf.push(OP_IS_DISTINCT),
                Instr::ToFloat => buf.push(OP_TO_FLOAT),
                Instr::Cast(t) => {
                    buf.push(OP_CAST);
                    buf.push(t as u8);
                }
                Instr::Concat => buf.push(OP_CONCAT),
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
                OP_GLOB => Instr::Glob(read_u16_arg()?),
                OP_IN_PARAM => Instr::InParam(read_u16_arg()?),
                OP_IN_LIST => Instr::InList(read_u16_arg()?),
                OP_JUMP_IF_NOT_TRUE => Instr::JumpIfNotTrue(read_u16_arg()?),
                OP_JUMP => Instr::Jump(read_u16_arg()?),
                OP_JUMP_IF_NOT_NULL => Instr::JumpIfNotNull(read_u16_arg()?),
                OP_POP => Instr::Pop,
                OP_CALL => {
                    let f = *buf.get(*pos).ok_or_else(err)?;
                    let argc = *buf.get(*pos + 1).ok_or_else(err)?;
                    *pos += 2;
                    Instr::Call(ScalarFn::from_tag(f)?, argc)
                }
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
                OP_IS_NOT_DISTINCT => Instr::IsNotDistinct,
                OP_IS_DISTINCT => Instr::IsDistinct,
                OP_TO_FLOAT => Instr::ToFloat,
                OP_CAST => {
                    let t = *buf.get(*pos).ok_or_else(err)?;
                    *pos += 1;
                    Instr::Cast(
                        ColumnType::from_tag(t)
                            .ok_or_else(|| Error::Corrupt("bad CAST type tag".into()))?,
                    )
                }
                OP_CONCAT => Instr::Concat,
                _ => return Err(Error::Corrupt(format!("invalid opcode {op}"))),
            });
        }
        ExprProgram::new(instrs, consts)
    }
}

/// Static verification, so `eval` needs no per-instruction safety checks:
/// const indices in range, no stack underflow, exactly one value left — and,
/// with jumps in the language, two more properties that used to be free.
///
/// **Termination.** Jump targets must be strictly FORWARD. A backward jump
/// would make an expression able to loop, and `eval` runs per row inside the
/// engine with no fuel counter: a crafted plan could hang a reader forever.
/// Forward-only makes the program a DAG, so it always terminates.
///
/// **Depth agreement at merge points.** With branches, "the depth here" is no
/// longer one number per program: an instruction can be reached from several
/// predecessors, and if they disagree the stack means different things
/// depending on the row's data. This walks a depth-per-index map and refuses
/// any disagreement, which is what keeps `max_stack` a real bound and every
/// `pop().expect("validated")` honest.
///
/// Returns the maximum stack depth over all paths.
fn validate(instrs: &[Instr], consts: &[Value]) -> Result<usize> {
    if instrs.is_empty() {
        return Err(Error::Corrupt("empty expression program".into()));
    }
    let n = instrs.len();
    // depth on ENTRY to each index; index n is the program's exit.
    let mut depth_at: Vec<Option<usize>> = vec![None; n + 1];
    depth_at[0] = Some(0);
    let mut max = 0usize;

    // Record the depth on entry to `t`, rejecting a disagreement.
    fn merge(depth_at: &mut [Option<usize>], t: usize, d: usize) -> Result<()> {
        match depth_at[t] {
            None => {
                depth_at[t] = Some(d);
                Ok(())
            }
            Some(prev) if prev == d => Ok(()),
            Some(prev) => Err(Error::Corrupt(format!(
                "stack depth disagrees at instruction {t}: {prev} on one path, {d} on another"
            ))),
        }
    }

    for i in 0..n {
        // Every instruction must be reachable. Dead code after a Jump is not a
        // harmless curiosity here: it means the encoder or a corrupt plan built
        // something whose stack shape was never proven.
        let d = depth_at[i]
            .ok_or_else(|| Error::Corrupt(format!("instruction {i} is unreachable")))?;
        max = max.max(d);

        let check_target = |t: u16| -> Result<usize> {
            let t = t as usize;
            if t <= i {
                return Err(Error::Corrupt(format!(
                    "backward jump {i} -> {t}: expression programs must terminate"
                )));
            }
            if t > n {
                return Err(Error::Corrupt(format!("jump target {t} past end of program")));
            }
            Ok(t)
        };

        match instrs[i] {
            Instr::Jump(t) => {
                let t = check_target(t)?;
                merge(&mut depth_at, t, d)?;
                // no fall-through: depth_at[i+1] is set only if something jumps there
            }
            Instr::JumpIfNotTrue(t) => {
                let t = check_target(t)?;
                if d < 1 {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                let d = d - 1; // pops the condition on BOTH paths
                merge(&mut depth_at, t, d)?;
                merge(&mut depth_at, i + 1, d)?;
            }
            Instr::JumpIfNotNull(t) => {
                let t = check_target(t)?;
                if d < 1 {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                // Peeks: the depth is UNCHANGED on both paths.
                merge(&mut depth_at, t, d)?;
                merge(&mut depth_at, i + 1, d)?;
            }
            instr => {
                let (pops, pushes) = match instr {
                    Instr::PushCol(_) | Instr::PushParam(_) => (0, 1),
                    Instr::PushConst(c) => {
                        if c as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        (0, 1)
                    }
                    Instr::Like(c) | Instr::Glob(c) => {
                        if c as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        (1, 1)
                    }
                    // Pops the probe scalar, pushes the 3VL result; the list comes
                    // from a param slot, not the stack, so the arity is not here.
                    Instr::InParam(_) => (1, 1),
                    Instr::Cast(_) => (1, 1),
                    Instr::Concat => (2, 1),
                    // n list elements plus the probe beneath them. n == 0 is the
                    // empty set `x IN ()`: eval pops the probe and pushes FALSE
                    // (`in_items_3vl` on an empty slice), so it is a valid (1, 1)
                    // op — NOT a no-op that leaves the probe posing as a bool.
                    Instr::InList(nl) => (nl as usize + 1, 1),
                    Instr::Neg | Instr::Not | Instr::IsNull | Instr::IsNotNull | Instr::ToFloat => {
                        (1, 1)
                    }
                    Instr::Pop => (1, 0),
                    // Arity is checked HERE, once per program, so eval can index
                    // the args without re-checking per row.
                    Instr::Call(f, argc) => {
                        if !f.arity_ok(argc) {
                            return Err(Error::Corrupt(format!(
                                "{}() called with {argc} argument(s)",
                                f.name()
                            )));
                        }
                        (argc as usize, 1)
                    }
                    Instr::Jump(_) | Instr::JumpIfNotTrue(_) | Instr::JumpIfNotNull(_) => {
                        unreachable!("handled above")
                    }
                    _ => (2, 1),
                };
                if d < pops {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                let nd = d - pops + pushes;
                max = max.max(nd);
                merge(&mut depth_at, i + 1, nd)?;
            }
        }
    }

    match depth_at[n] {
        Some(1) => Ok(max),
        Some(other) => Err(Error::Corrupt(format!(
            "expression program must leave exactly one value, leaves {other}"
        ))),
        None => Err(Error::Corrupt(
            "expression program never reaches its end".into(),
        )),
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

/// Result of matching one non-`*` GLOB pattern token against a single string
/// character. A token is a literal, `?`, or a `[...]` set.
enum GlobTok {
    /// Matched; the pattern index just past this token.
    Yes(usize),
    /// A well-formed token that did NOT match this character.
    No,
    /// A `[` set with no closing `]`. sqlite treats that as a whole-match
    /// failure (`patternCompare` returns NOMATCH), so the caller stops.
    Unterminated,
}

/// Match the GLOB `[...]` set at `p[start]` (`p[start] == '['`) against char
/// `c`. Mirrors sqlite `patternCompare`'s set logic:
/// - a leading `^` inverts the class;
/// - a `]` immediately after `[`/`[^` is a LITERAL member, not the terminator;
/// - `a-z` is a range, but a `-` that is first, last-before-`]`, or right after
///   a completed range is a literal `-`;
/// - an unterminated set fails the whole comparison.
fn glob_set(p: &[char], start: usize, c: char) -> GlobTok {
    let mut i = start + 1;
    let mut invert = false;
    let mut seen = false;
    // The previous set member available to start a range. `None` (sqlite's
    // `prior_c == 0`) means no range can start here — which is why a leading
    // literal `]` deliberately leaves it unset.
    let mut prior: Option<char> = None;
    if i < p.len() && p[i] == '^' {
        invert = true;
        i += 1;
    }
    if i < p.len() && p[i] == ']' {
        if c == ']' {
            seen = true;
        }
        i += 1; // leading `]` is literal; prior stays None (sqlite parity)
    }
    while i < p.len() && p[i] != ']' {
        let ch = p[i];
        if ch == '-' && prior.is_some() && i + 1 < p.len() && p[i + 1] != ']' {
            let lo = prior.expect("checked is_some");
            let hi = p[i + 1];
            if c >= lo && c <= hi {
                seen = true;
            }
            prior = None; // a completed range cannot itself start another
            i += 2;
        } else {
            if ch == c {
                seen = true;
            }
            prior = Some(ch);
            i += 1;
        }
    }
    if i >= p.len() {
        return GlobTok::Unterminated; // no closing `]`
    }
    if seen ^ invert {
        GlobTok::Yes(i + 1)
    } else {
        GlobTok::No
    }
}

/// sqlite GLOB: `*` matches any run, `?` matches exactly one char, and `[...]`
/// is a character class (`[^...]`, ranges). Case-SENSITIVE (unlike LIKE, which
/// sqlite also leaves case-sensitive but with `%`/`_`). Iterative two-pointer
/// with `*` backtracking — O(n·m) worst case, no recursion, no regex dep, the
/// same shape as [`like_match`].
fn glob_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
            continue;
        }
        // Does the current (non-`*`) token match this character?
        let matched = if pi < p.len() {
            match p[pi] {
                '?' => Some(pi + 1),
                '[' => match glob_set(&p, pi, t[ti]) {
                    GlobTok::Yes(next) => Some(next),
                    GlobTok::No => None,
                    // An unterminated set fails the whole comparison, at every
                    // position — so no amount of `*` backtracking can rescue it.
                    GlobTok::Unterminated => return false,
                },
                c if c == t[ti] => Some(pi + 1),
                _ => None,
            }
        } else {
            None
        };
        if let Some(next_pi) = matched {
            pi = next_pi;
            ti += 1;
        } else if star_pi != usize::MAX {
            // Let the most recent `*` absorb one more character.
            star_ti += 1;
            pi = star_pi + 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
    fn glob_patterns() {
        // `*` = any run (incl. empty); `?` = exactly one char.
        assert!(glob_match("a*", "abc"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("*c", "abc"));
        assert!(glob_match("a*c", "abxyzc"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac")); // `?` needs a char
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "axxbyyc"));

        // Case-SENSITIVE — the property that distinguishes GLOB from a
        // case-folding matcher (and the point sqlite makes about GLOB vs LIKE).
        assert!(!glob_match("A*", "abc"));
        assert!(glob_match("A*", "Abc"));
        assert!(!glob_match("abc", "ABC"));

        // Character classes: sets, ranges, negation.
        assert!(glob_match("[abc]", "b"));
        assert!(!glob_match("[abc]", "d"));
        assert!(glob_match("[a-c]x", "bx"));
        assert!(!glob_match("[a-c]x", "dx"));
        assert!(glob_match("[^a-c]x", "dx"));
        assert!(!glob_match("[^a-c]x", "bx"));
        // Class is case-sensitive too: `[a-c]` excludes uppercase.
        assert!(!glob_match("[a-c]", "B"));
        // A leading `]` is a literal set member.
        assert!(glob_match("[]x]", "]"));
        assert!(glob_match("[]x]", "x"));
        // `-` as first/last member is literal, not a range.
        assert!(glob_match("[-a]", "-"));
        assert!(glob_match("[a-]", "-"));
        // A `*`/`?` inside a class is a literal char, not a wildcard.
        assert!(glob_match("[*?]", "*"));
        assert!(glob_match("[*?]", "?"));
        assert!(!glob_match("[*?]", "a"));
        // An unterminated set fails the whole match (sqlite NOMATCH).
        assert!(!glob_match("[abc", "a"));

        // Literal `*`/`?` in the pattern are ALWAYS wildcards (no escape), so a
        // literal one must be matched via a class — the same rule sqlite has.
        assert!(glob_match("a[*]b", "a*b"));
        assert!(!glob_match("a[*]b", "axb"));
    }

    #[test]
    fn glob_program_null_and_type_rules() {
        // `col0 GLOB 'a*'` — NULL operand yields NULL, exactly like LIKE.
        let p = prog(vec![Instr::PushCol(0), Instr::Glob(0)], vec![Value::Text("a*".into())]);
        assert_eq!(p.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(true));
        assert_eq!(p.eval(&[Value::Text("xbc".into())], &[]).unwrap(), Value::Bool(false));
        assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Null);
        // A non-text operand is a type error, not a silent non-match.
        assert!(matches!(
            p.eval(&[Value::Int(1)], &[]),
            Err(Error::TypeMismatch(_))
        ));

        // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
        let mut buf = Vec::new();
        p.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
        assert_eq!(pos, buf.len());
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
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

    #[test]
    fn cast_and_concat_semantics_and_codec() {
        let cast = |v: Value, t: ColumnType| {
            prog(vec![Instr::PushParam(0), Instr::Cast(t)], vec![]).eval(&[], &[v])
        };
        // NULL casts to NULL of every type.
        assert_eq!(cast(Value::Null, ColumnType::Int64).unwrap(), Value::Null);
        assert_eq!(cast(Value::Null, ColumnType::Blob).unwrap(), Value::Null);
        // float→int truncates toward zero (sqlite's rule), NaN/inf saturate
        // deterministically instead of being UB.
        assert_eq!(cast(Value::Float(-1.9), ColumnType::Int64).unwrap(), Value::Int(-1));
        assert_eq!(cast(Value::Float(f64::NAN), ColumnType::Int64).unwrap(), Value::Int(0));
        assert_eq!(
            cast(Value::Float(f64::INFINITY), ColumnType::Int64).unwrap(),
            Value::Int(i64::MAX)
        );
        assert_eq!(cast(Value::Int(3), ColumnType::Float64).unwrap(), Value::Float(3.0));
        assert_eq!(cast(Value::Int(-7), ColumnType::Text).unwrap(), Value::Text("-7".into()));
        assert_eq!(cast(Value::Bool(true), ColumnType::Int64).unwrap(), Value::Int(1));
        assert_eq!(cast(Value::Int(0), ColumnType::Bool).unwrap(), Value::Bool(false));
        // The strictness line: text never parses into a number.
        assert!(cast(Value::Text("12".into()), ColumnType::Int64).is_err());
        assert!(cast(Value::Blob(vec![1]), ColumnType::Text).is_err());

        let cat = |a: Value, b: Value| {
            prog(
                vec![Instr::PushParam(0), Instr::PushParam(1), Instr::Concat],
                vec![],
            )
            .eval(&[], &[a, b])
        };
        assert_eq!(
            cat(Value::Text("ab".into()), Value::Int(3)).unwrap(),
            Value::Text("ab3".into())
        );
        assert_eq!(cat(Value::Text("x".into()), Value::Null).unwrap(), Value::Null);
        assert!(cat(Value::Text("x".into()), Value::Float(1.5)).is_err());

        // codec: roundtrip, truncation safety, and a bad CAST type tag.
        let p = prog(
            vec![
                Instr::PushCol(0),
                Instr::Cast(ColumnType::Text),
                Instr::PushCol(1),
                Instr::Concat,
            ],
            vec![],
        );
        let mut buf = Vec::new();
        p.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
        // Corrupt the Cast's type-tag byte: find OP_CAST and break the byte
        // after it — decode must say Corrupt, never panic or misread.
        let i = buf.iter().position(|&b| b == 30).unwrap(); // OP_CAST
        let mut evil = buf.clone();
        evil[i + 1] = 0xEE;
        assert!(matches!(
            ExprProgram::decode(&evil, &mut 0),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn is_distinct_is_null_safe_and_two_valued() {
        // `a IS b` == IsNotDistinct: NULL-safe equality that never yields NULL.
        let isnd = |a: Value, b: Value| {
            prog(
                vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsNotDistinct],
                vec![],
            )
            .eval(&[], &[a, b])
            .unwrap()
        };
        assert_eq!(isnd(Value::Null, Value::Null), Value::Bool(true));
        assert_eq!(isnd(Value::Null, Value::Int(1)), Value::Bool(false));
        assert_eq!(isnd(Value::Int(1), Value::Null), Value::Bool(false));
        assert_eq!(isnd(Value::Int(1), Value::Int(1)), Value::Bool(true));
        assert_eq!(isnd(Value::Int(1), Value::Int(2)), Value::Bool(false));
        // Text operands compare the same way.
        assert_eq!(
            isnd(Value::Text("a".into()), Value::Text("a".into())),
            Value::Bool(true)
        );
        assert_eq!(
            isnd(Value::Text("a".into()), Value::Text("b".into())),
            Value::Bool(false)
        );

        // `a IS NOT b` == IsDistinct: the exact negation, still never NULL.
        let isd = |a: Value, b: Value| {
            prog(
                vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsDistinct],
                vec![],
            )
            .eval(&[], &[a, b])
            .unwrap()
        };
        assert_eq!(isd(Value::Null, Value::Null), Value::Bool(false));
        assert_eq!(isd(Value::Null, Value::Int(1)), Value::Bool(true));
        assert_eq!(isd(Value::Int(1), Value::Null), Value::Bool(true));
        assert_eq!(isd(Value::Int(1), Value::Int(1)), Value::Bool(false));
        assert_eq!(isd(Value::Int(1), Value::Int(2)), Value::Bool(true));

        // A NULL result is impossible, so as a filter predicate every case is
        // decided — unlike `=`, where NULL denies. `NULL IS NULL` passes.
        let p = prog(
            vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsNotDistinct],
            vec![],
        );
        assert!(p
            .eval_filter(&mut Vec::new(), &[], &[Value::Null, Value::Null])
            .unwrap());
        assert!(!p
            .eval_filter(&mut Vec::new(), &[], &[Value::Null, Value::Int(1)])
            .unwrap());

        // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
        let prog2 = prog(
            vec![
                Instr::PushCol(0),
                Instr::PushCol(1),
                Instr::IsNotDistinct,
                Instr::PushCol(2),
                Instr::PushCol(3),
                Instr::IsDistinct,
                Instr::And,
            ],
            vec![],
        );
        let mut buf = Vec::new();
        prog2.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), prog2);
        assert_eq!(pos, buf.len());
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
    }

    // ---- §2.6 `col IN (context list)` under 3VL ----

    fn in_prog() -> ExprProgram {
        // PushCol(0) ; InParam(0)   ==   `c0 IN ($1)`
        ExprProgram::new(vec![Instr::PushCol(0), Instr::InParam(0)], vec![]).unwrap()
    }

    fn eval_in(probe: Value, list: Value) -> Value {
        in_prog().eval(&[probe], &[list]).unwrap()
    }

    #[test]
    fn in_list_three_valued_logic() {
        let l = |v: Vec<Value>| Value::List(v);

        // plain hit / miss
        assert_eq!(eval_in(Value::Int(2), l(vec![Value::Int(1), Value::Int(2)])), Value::Bool(true));
        assert_eq!(eval_in(Value::Int(9), l(vec![Value::Int(1), Value::Int(2)])), Value::Bool(false));

        // a match WINS over a NULL element — this is why the NULL scan cannot
        // short-circuit before the equality scan.
        assert_eq!(
            eval_in(Value::Int(2), l(vec![Value::Null, Value::Int(2)])),
            Value::Bool(true)
        );

        // no match + a NULL element ⇒ UNKNOWN, not FALSE: the NULL might have
        // been the match.
        assert_eq!(eval_in(Value::Int(9), l(vec![Value::Null, Value::Int(2)])), Value::Null);

        // NULL probe is never TRUE
        assert_eq!(eval_in(Value::Null, l(vec![Value::Int(1)])), Value::Null);

        // empty set denies CLEANLY (FALSE, not NULL): "belongs to nothing".
        assert_eq!(eval_in(Value::Int(1), l(vec![])), Value::Bool(false));

        // an entirely-NULL set is an unknown set
        assert_eq!(eval_in(Value::Int(1), Value::Null), Value::Null);
    }

    /// A filter passes only on exactly TRUE, so every UNKNOWN above must deny.
    /// This is the property a policy actually rests on.
    #[test]
    fn in_list_unknown_denies_in_a_filter() {
        let p = in_prog();
        // no match + NULL element ⇒ UNKNOWN ⇒ row not visible
        assert!(!p
            .eval_filter(&mut Vec::new(), &[Value::Int(9)], &[Value::List(vec![Value::Null])])
            .unwrap());
        // NULL probe ⇒ UNKNOWN ⇒ row not visible
        assert!(!p
            .eval_filter(&mut Vec::new(), &[Value::Null], &[Value::List(vec![Value::Int(1)])])
            .unwrap());
        // a real match is visible
        assert!(p
            .eval_filter(&mut Vec::new(), &[Value::Int(1)], &[Value::List(vec![Value::Int(1)])])
            .unwrap());
    }

    /// A type mismatch inside the list must ERROR, not quietly deny every row —
    /// a silent deny would look exactly like "this tenant owns nothing".
    #[test]
    fn in_list_type_mismatch_is_an_error_not_a_silent_deny() {
        let r = in_prog().eval(&[Value::Int(1)], &[Value::List(vec![Value::Text("1".into())])]);
        assert!(matches!(r, Err(Error::TypeMismatch(_))), "got {r:?}");
        // and a non-list param is likewise a caller error
        let r2 = in_prog().eval(&[Value::Int(1)], &[Value::Int(1)]);
        assert!(matches!(r2, Err(Error::TypeMismatch(_))), "got {r2:?}");
    }

    #[test]
    fn in_param_roundtrips_and_out_of_range_param_is_corrupt() {
        let p = in_prog();
        let mut buf = Vec::new();
        p.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
        // a program referencing param 5 with no params supplied must not panic
        let bad = ExprProgram::new(vec![Instr::PushCol(0), Instr::InParam(5)], vec![]).unwrap();
        assert!(matches!(bad.eval(&[Value::Int(1)], &[]), Err(Error::Corrupt(_))));
    }

    /// Lists cross the intent ring as params, so they must survive write/read.
    #[test]
    fn list_value_roundtrips_through_the_param_codec() {
        use crate::value::{read_value, write_value};
        let v = Value::List(vec![Value::Int(1), Value::Text("a".into()), Value::Null]);
        let mut buf = Vec::new();
        write_value(&mut buf, &v);
        let mut pos = 0;
        assert_eq!(read_value(&buf, &mut pos).unwrap(), v);

        // truncation at every offset yields Corrupt, never a panic
        for cut in 0..buf.len() {
            let mut pos = 0;
            let _ = read_value(&buf[..cut], &mut pos); // must not panic
        }
        // a nested list is rejected on the way in
        let mut nested = Vec::new();
        write_value(&mut nested, &Value::List(vec![Value::List(vec![Value::Int(1)])]));
        let mut pos = 0;
        assert!(matches!(read_value(&nested, &mut pos), Err(Error::Corrupt(_))));
    }
}

#[cfg(test)]
mod in_list_tests {
    use super::*;

    fn prog(instrs: Vec<Instr>, consts: Vec<Value>) -> ExprProgram {
        ExprProgram::new(instrs, consts).unwrap()
    }

    /// `InList` and `InParam` must agree on 3VL exactly — they share
    /// `in_items_3vl` for that reason, and this pins that they cannot drift.
    #[test]
    fn in_list_and_in_param_give_identical_3vl() {
        let cases: Vec<(Value, Vec<Value>, Option<bool>)> = vec![
            (Value::Int(2), vec![Value::Int(1), Value::Int(2)], Some(true)),
            (Value::Int(3), vec![Value::Int(1), Value::Int(2)], Some(false)),
            // no match but a NULL is present -> unknown, NOT false
            (Value::Int(3), vec![Value::Int(1), Value::Null], None),
            // a match wins even alongside a NULL
            (Value::Int(1), vec![Value::Int(1), Value::Null], Some(true)),
            // NULL probe is never TRUE
            (Value::Null, vec![Value::Int(1)], None),
        ];
        for (probe, items, want) in cases {
            let via_list = prog(
                {
                    let mut i = vec![Instr::PushConst(0)];
                    for n in 0..items.len() {
                        i.push(Instr::PushConst(1 + n as u16));
                    }
                    i.push(Instr::InList(items.len() as u16));
                    i
                },
                std::iter::once(probe.clone()).chain(items.clone()).collect(),
            )
            .eval(&[], &[])
            .unwrap();

            let via_param = prog(vec![Instr::PushConst(0), Instr::InParam(0)], vec![probe.clone()])
                .eval(&[], &[Value::List(items.clone())])
                .unwrap();

            let got = match &via_list {
                Value::Null => None,
                Value::Bool(b) => Some(*b),
                v => panic!("non-bool {v:?}"),
            };
            assert_eq!(got, want, "InList({probe:?}, {items:?})");
            assert_eq!(via_list, via_param, "the two IN forms disagree on {probe:?} in {items:?}");
        }
    }

    /// `x IN ()` — the empty set — is FALSE for every probe, NULL included
    /// (SQL 3VL). A zero-arity InList pops the probe and pushes that FALSE, so
    /// the verifier ACCEPTS the program and eval yields Bool(false), never the
    /// probe left posing as a bool.
    #[test]
    fn zero_arity_in_list_is_empty_set_false() {
        for probe in [Value::Int(5), Value::Null] {
            let got = prog(vec![Instr::PushConst(0), Instr::InList(0)], vec![probe.clone()])
                .eval(&[], &[])
                .unwrap();
            assert_eq!(got, Value::Bool(false), "{probe:?} IN () must be FALSE");
        }
    }

    #[test]
    fn in_list_underflow_is_corrupt() {
        // claims 3 elements but only 2 values are pushed
        let r = ExprProgram::new(
            vec![Instr::PushCol(0), Instr::PushCol(1), Instr::InList(3)],
            vec![],
        );
        assert!(matches!(r, Err(Error::Corrupt(_))), "got {r:?}");
    }

    #[test]
    fn in_list_round_trips_through_the_codec() {
        let p = prog(
            vec![
                Instr::PushCol(0),
                Instr::PushConst(0),
                Instr::PushConst(1),
                Instr::InList(2),
            ],
            vec![Value::Int(1), Value::Int(2)],
        );
        let mut bytes = Vec::new();
        p.encode_into(&mut bytes);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&bytes, &mut pos).unwrap(), p);
        assert_eq!(pos, bytes.len());
        // truncation at every offset must be Corrupt, never a panic (repo rule)
        for n in 0..bytes.len() {
            let mut pos = 0;
            assert!(
                ExprProgram::decode(&bytes[..n], &mut pos).is_err(),
                "truncation at {n} decoded"
            );
        }
    }
}

#[cfg(test)]
mod jump_tests {
    use super::*;

    /// A backward jump would let a per-row expression loop forever. `eval` runs
    /// inside the engine with no fuel counter, so a crafted or corrupt plan
    /// could hang a reader. The verifier must refuse it, not the evaluator.
    #[test]
    fn backward_jump_is_refused_so_programs_terminate() {
        let r = ExprProgram::new(
            vec![Instr::PushConst(0), Instr::Jump(0)],
            vec![Value::Bool(true)],
        );
        assert!(
            matches!(&r, Err(Error::Corrupt(m)) if m.contains("terminate")),
            "got {r:?}"
        );
        // self-jump is backward too (t <= i)
        let r = ExprProgram::new(vec![Instr::PushConst(0), Instr::Jump(1)], vec![Value::Null]);
        assert!(matches!(r, Err(Error::Corrupt(_))), "got {r:?}");
    }

    /// If two paths reach an instruction at different depths, the stack means
    /// different things depending on the row's data — and `max_stack` stops
    /// being a bound.
    #[test]
    fn disagreeing_stack_depth_at_a_merge_is_corrupt() {
        // path A leaves 2 values at index 4, path B leaves 1
        let r = ExprProgram::new(
            vec![
                Instr::PushConst(0),        // 0: cond
                Instr::JumpIfNotTrue(4),    // 1: -> 4 with depth 0
                Instr::PushConst(1),        // 2: depth 1
                Instr::PushConst(1),        // 3: depth 2  (falls through to 4)
                Instr::PushConst(1),        // 4: merge point — disagreement
            ],
            vec![Value::Bool(true), Value::Int(1)],
        );
        assert!(
            matches!(&r, Err(Error::Corrupt(m)) if m.contains("disagrees")),
            "got {r:?}"
        );
    }

    #[test]
    fn jump_past_the_end_and_unreachable_code_are_corrupt() {
        let r = ExprProgram::new(
            vec![Instr::PushConst(0), Instr::Jump(99)],
            vec![Value::Int(1)],
        );
        assert!(matches!(&r, Err(Error::Corrupt(m)) if m.contains("past end")), "got {r:?}");

        // instruction 2 can never be reached: nothing falls into it or jumps to it
        let r = ExprProgram::new(
            vec![Instr::PushConst(0), Instr::Jump(3), Instr::Neg, Instr::PushConst(0)],
            vec![Value::Int(1)],
        );
        assert!(matches!(&r, Err(Error::Corrupt(m)) if m.contains("unreachable")), "got {r:?}");
    }

    /// The shape the CASE codegen actually emits, evaluated both ways.
    #[test]
    fn case_shaped_program_evaluates_both_branches() {
        // CASE WHEN col0 THEN 10 ELSE 20 END
        let p = ExprProgram::new(
            vec![
                Instr::PushCol(0),       // 0
                Instr::JumpIfNotTrue(4), // 1 -> else
                Instr::PushConst(0),     // 2: 10
                Instr::Jump(5),          // 3 -> end
                Instr::PushConst(1),     // 4: 20
            ],
            vec![Value::Int(10), Value::Int(20)],
        )
        .unwrap();
        assert_eq!(p.eval(&[Value::Bool(true)], &[]).unwrap(), Value::Int(10));
        assert_eq!(p.eval(&[Value::Bool(false)], &[]).unwrap(), Value::Int(20));
        // NULL must take the ELSE arm, not the THEN arm
        assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Int(20));

        // and it must survive the codec, truncation included
        let mut bytes = Vec::new();
        p.encode_into(&mut bytes);
        let mut pos = 0;
        assert_eq!(ExprProgram::decode(&bytes, &mut pos).unwrap(), p);
        for n in 0..bytes.len() {
            let mut pos = 0;
            assert!(ExprProgram::decode(&bytes[..n], &mut pos).is_err(), "truncation at {n}");
        }
    }
}
