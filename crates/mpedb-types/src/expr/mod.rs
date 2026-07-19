//! Compact stack-based expression IR (PySpell-style: compiled once at
//! prepare/attach, evaluated many times with no parsing or allocation-heavy
//! AST walking).
//!
//! Used for WHERE filters, projections with computed columns, and CHECK
//! constraints. Follows SQL three-valued logic: comparisons and arithmetic
//! with NULL yield NULL; AND/OR/NOT use Kleene logic; a filter passes only if
//! the result is exactly TRUE.

use crate::error::{Error, Result};
use crate::value::{Affinity, Collation, Value};
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
    glob_match, in_items_3vl, in_items_3vl_collated, in_list_3vl, like_match, like_match_cs,
    regexp_match,
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
    /// `CAST(x AS <type>)` — sqlite's permissive, affinity-based conversion
    /// (the type name resolves to one of five [`Affinity`]s in the binder).
    /// NULL casts to NULL; text→number parses a leading numeric prefix
    /// (`'12ab'`→12); real→int truncates toward zero; `NUMERIC` yields an int
    /// when integral else a real. See `cast_value`.
    Cast(Affinity),
    /// `a || b` — SQL concatenation. NULL propagates; ints and bools render
    /// as text first (sqlite's rule); floats are refused until someone needs
    /// their formatting pinned down.
    Concat,
    /// SQL LIKE with pattern from the const pool (supports % and _).
    Like(u16),
    /// Case-SENSITIVE LIKE (PostgreSQL dialect); otherwise identical to
    /// [`Instr::Like`]. Emitted for a `bare_group_by = "postgres"` database,
    /// where `'a' LIKE 'A'` is FALSE. See [`like_match_cs`](ops::like_match_cs).
    LikeCs(u16),
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
    /// nicety here: mpedb raises on arithmetic overflow, so an eager
    /// `coalesce(x, <overflowing expr>)` would ERROR where both sqlite and
    /// PostgreSQL return x. (Division by zero is NOT such a case: like sqlite,
    /// mpedb evaluates `1/0` to NULL rather than raising.)
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
    /// Call a HOST-registered scalar UDF (the C-API `create_function` path,
    /// design/DESIGN-UDF.md). The first `u16` is the const-pool index of the
    /// function NAME (a [`Value::Text`]); the second is the argument count. Pops
    /// `argc` values (leftmost deepest), looks the name up in the eval context's
    /// [`HostFns`], invokes it, and pushes the one result.
    ///
    /// The plan stores only the NAME + arity, never the closure — closures are
    /// not serializable, and a plan carrying a host call is valid ONLY for a
    /// connection that registered that UDF, so the facade never publishes it to
    /// the shared plan registry. With no host functions in scope (a plan without
    /// a host call, a CHECK constraint, a test) this opcode is unreachable, so
    /// the change is behavior-neutral for every existing plan.
    HostCall(u16, u16),
}

/// Resolve a HOST-registered scalar UDF by name at eval time (the C-API
/// `create_function` path, design/DESIGN-UDF.md). Implemented by the facade over
/// its per-connection registry; `None` is threaded wherever no UDF can be in
/// scope (CHECK constraints, tests, any plan without a [`Instr::HostCall`]), so
/// the whole mechanism stays inert for existing plans.
pub trait HostFns {
    /// Invoke the scalar function `name` over already-evaluated `args`
    /// (leftmost first), returning its result. An `Error` when no function of
    /// that name/arity is registered — defensive; the binder already checked the
    /// name at compile time, but a registration can change between compile and
    /// execute.
    fn call(&self, name: &str, args: &[Value]) -> Result<Value>;
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

    /// Does this program call a host-registered UDF ([`Instr::HostCall`])? The
    /// facade uses this to keep a plan that references a per-connection UDF OUT
    /// of the shared content-hashed plan registry (design/DESIGN-UDF.md): such a
    /// plan is valid only for the connection that registered the function.
    pub fn has_host_call(&self) -> bool {
        self.instrs.iter().any(|i| matches!(i, Instr::HostCall(..)))
    }

    /// Evaluate against a decoded row and statement parameters. No host UDFs in
    /// scope — a program containing [`Instr::HostCall`] errors (defensive; only
    /// the executor's host-aware path reaches such a program).
    pub fn eval(&self, cols: &[Value], params: &[Value]) -> Result<Value> {
        self.eval_host(cols, params, None)
    }

    /// [`eval`](Self::eval) with a [`HostFns`] resolver for host-registered
    /// scalar UDFs (design/DESIGN-UDF.md). `None` behaves exactly like `eval`.
    pub fn eval_host(
        &self,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
    ) -> Result<Value> {
        let mut stack: Vec<Value> = Vec::with_capacity(self.max_stack);
        self.eval_with_stack_host(&mut stack, cols, params, host)
    }

    /// Hot-path variant reusing a scratch stack across rows.
    pub fn eval_with_stack(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
    ) -> Result<Value> {
        self.eval_with_stack_host(stack, cols, params, None)
    }

    /// [`eval_with_stack`](Self::eval_with_stack) with a [`HostFns`] resolver.
    /// `None` is behavior-identical to `eval_with_stack`.
    pub fn eval_with_stack_host(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
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
                Instr::LikeCs(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(like_match_cs(p, s)),
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
                Instr::HostCall(name_idx, argc) => {
                    // The name lives in the const pool; validate proved the index
                    // is in range, and a hostile blob whose const is not text is
                    // Corrupt, not a panic.
                    let name = match &self.consts[name_idx as usize] {
                        Value::Text(s) => s,
                        _ => {
                            return Err(Error::Corrupt(
                                "host-call name constant is not text".into(),
                            ))
                        }
                    };
                    // validate proved depth >= argc, so the split holds.
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    let host = host.ok_or_else(|| {
                        Error::Internal(format!(
                            "host function `{name}` called with no host functions in scope"
                        ))
                    })?;
                    stack.push(host.call(name, &args)?);
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
        self.eval_filter_host(stack, cols, params, None)
    }

    /// [`eval_filter`](Self::eval_filter) with a [`HostFns`] resolver for
    /// host-registered scalar UDFs (design/DESIGN-UDF.md). `None` is
    /// behavior-identical to `eval_filter`.
    pub fn eval_filter_host(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
    ) -> Result<bool> {
        match self.eval_with_stack_host(stack, cols, params, host)? {
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
        (Int(x), Int(y)) => Ok(match op {
            Instr::Add => Int(x.checked_add(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Sub => Int(x.checked_sub(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Mul => Int(x.checked_mul(y).ok_or(Error::ArithmeticOverflow)?),
            // sqlite yields NULL on division/modulo by zero (not an error). A
            // non-zero divisor still guards the one i64::MIN / -1 overflow that
            // `checked_div`/`checked_rem` report — that stays an error.
            Instr::Div if y == 0 => Null,
            Instr::Div => Int(x.checked_div(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Mod if y == 0 => Null,
            Instr::Mod => Int(x.checked_rem(y).ok_or(Error::ArithmeticOverflow)?),
            _ => unreachable!(),
        }),
        (Float(x), Float(y)) => Ok(match op {
            Instr::Add => Float(x + y),
            Instr::Sub => Float(x - y),
            Instr::Mul => Float(x * y),
            // sqlite yields NULL on division/modulo by zero (not an error).
            Instr::Div if y == 0.0 => Null,
            Instr::Div => Float(x / y),
            Instr::Mod if y == 0.0 => Null,
            Instr::Mod => Float(x % y),
            _ => unreachable!(),
        }),
        (a, b) => Err(Error::TypeMismatch(format!(
            "arithmetic on {} and {} (binder should have coerced)",
            a.type_name(),
            b.type_name()
        ))),
    }
}


/// `CAST` semantics — sqlite's permissive, affinity-based conversion. NULL
/// casts to NULL for every affinity. The conversions match sqlite 3.45 exactly
/// (differential-tested in `crates/mpedb/tests/cast_affinity.rs`):
///
/// - **Integer**: real truncates toward zero (saturating, so NaN→0, ±inf→i64
///   min/max); text/blob parse a leading *integer* prefix (`'12ab'`→12,
///   `'1e3'`→1 — stops at `e`, `'abc'`→0); bool→0/1; timestamp→its micros.
/// - **Real**: int/bool/timestamp widen to f64; text/blob parse a leading
///   *float* prefix (`'1e3'`→1000.0, `'abc'`→0.0).
/// - **Text**: int/bool/timestamp/real render as sqlite text (real via
///   `%!.15g`); blob is reinterpreted as its bytes — refused only when those
///   bytes are not valid UTF-8 (mpedb `Text` is a Rust `String`; the one
///   deviation from sqlite, which keeps raw bytes).
/// - **Blob**: the value's *text rendering* as bytes (`90`→`x'3930'`), or a
///   blob unchanged.
/// - **Numeric**: an already-typed int/real is left as-is (a real stays real
///   even when integral); text/blob become an int when the whole string is a
///   pure `i64` or the parsed value is integral with `|v| < 2^51`, else a real
///   — sqlite's `NUMERIC` affinity.
fn cast_value(v: Value, aff: Affinity) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match aff {
        Affinity::Integer => Value::Int(to_integer(&v)),
        Affinity::Real => Value::Float(to_real(&v)),
        Affinity::Numeric => to_numeric(v),
        Affinity::Text => match v {
            Value::Text(s) => Value::Text(s),
            Value::Blob(b) => Value::Text(String::from_utf8(b).map_err(|_| {
                Error::TypeMismatch(
                    "CAST of a non-UTF-8 BLOB to TEXT is not representable in mpedb".into(),
                )
            })?),
            other => Value::Text(render_scalar_text(&other)),
        },
        Affinity::Blob => match v {
            Value::Blob(b) => Value::Blob(b),
            Value::Text(s) => Value::Blob(s.into_bytes()),
            other => Value::Blob(render_scalar_text(&other).into_bytes()),
        },
    })
}

/// sqlite whitespace for numeric-prefix skipping: space, tab, LF, FF, CR.
fn is_sql_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

/// A non-blob, non-text scalar rendered to sqlite text (the shared path for
/// TEXT and BLOB affinity). Reals use `%!.15g`; bools render as their integer.
fn render_scalar_text(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Float(f) => String::from_utf8(printf::float_to_text(*f))
            .expect("float_to_text is ASCII"),
        Value::Timestamp(t) => t.to_string(),
        Value::Text(s) => s.clone(),
        // Blob is handled by the caller (bytes are copied directly, not via
        // this text path); a List never reaches CAST.
        Value::Blob(_) | Value::Null | Value::List(_) => String::new(),
    }
}

/// INTEGER-affinity conversion of a non-NULL value.
fn to_integer(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        // `f as i64` saturates (NaN→0, ±inf→bounds) and truncates toward zero.
        Value::Float(f) => *f as i64,
        Value::Bool(b) => *b as i64,
        Value::Timestamp(t) => *t,
        Value::Text(s) => int_prefix(s.as_bytes()),
        Value::Blob(b) => int_prefix(b),
        Value::Null | Value::List(_) => 0,
    }
}

/// REAL-affinity conversion of a non-NULL value.
fn to_real(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => *b as u8 as f64,
        Value::Timestamp(t) => *t as f64,
        Value::Text(s) => float_prefix(s.as_bytes()),
        Value::Blob(b) => float_prefix(b),
        Value::Null | Value::List(_) => 0.0,
    }
}

/// NUMERIC-affinity conversion of a non-NULL value. Already-numeric values are
/// left untouched (a real stays real); text/blob are parsed by `bytes_to_numeric`.
fn to_numeric(v: Value) -> Value {
    match v {
        Value::Int(_) | Value::Float(_) => v,
        Value::Bool(b) => Value::Int(b as i64),
        Value::Timestamp(t) => Value::Int(t),
        Value::Text(s) => bytes_to_numeric(s.as_bytes()),
        Value::Blob(b) => bytes_to_numeric(&b),
        Value::Null | Value::List(_) => v,
    }
}

/// Parse a leading integer prefix (sqlite `sqlite3Atoi64`): optional leading
/// whitespace, optional sign, then decimal digits, stopping at the first
/// non-digit. Overflow saturates to the i64 bounds. No digits → 0.
fn int_prefix(b: &[u8]) -> i64 {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut acc: i64 = 0;
    let mut saw = false;
    while i < b.len() && b[i].is_ascii_digit() {
        saw = true;
        let d = (b[i] - b'0') as i64;
        acc = match acc.checked_mul(10).and_then(|a| a.checked_add(d)) {
            Some(a) => a,
            None => return if neg { i64::MIN } else { i64::MAX },
        };
        i += 1;
    }
    if !saw {
        return 0;
    }
    if neg {
        acc.checked_neg().unwrap_or(i64::MIN)
    } else {
        acc
    }
}

/// The end index of the leading float token (sqlite `sqlite3AtoF` grammar):
/// `[ws][sign]digits[.digits][(e|E)[sign]digits]`, also accepting `.5`. Returns
/// `(numeric_start, numeric_end, saw_digit)` — the slice `b[start..end]` is
/// ASCII and parseable by Rust's `f64::from_str`.
fn float_token(b: &[u8]) -> (usize, usize, bool) {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let start = i;
    if matches!(b.get(i), Some(b'+') | Some(b'-')) {
        i += 1;
    }
    let mut saw = false;
    while i < b.len() && b[i].is_ascii_digit() {
        saw = true;
        i += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            saw = true;
            i += 1;
        }
    }
    // An exponent only counts when the mantissa had a digit AND the exponent
    // itself has one (`'1e'` parses as `1`, not `1e<nothing>`).
    if saw && matches!(b.get(i), Some(b'e') | Some(b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+') | Some(b'-')) {
            j += 1;
        }
        if matches!(b.get(j), Some(d) if d.is_ascii_digit()) {
            j += 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    (start, i, saw)
}

/// Parse a leading float prefix; no numeric prefix → 0.0.
fn float_prefix(b: &[u8]) -> f64 {
    let (start, end, saw) = float_token(b);
    if !saw {
        return 0.0;
    }
    // The token is pure ASCII by construction.
    std::str::from_utf8(&b[start..end])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// sqlite's NUMERIC parse of text/blob bytes: an integer when the whole
/// (trimmed) string is a pure `i64`, OR when the parsed float is integral and
/// `|v| < 2^51` (sqlite's `sqlite3RealSameAsInt` bound); else a real.
fn bytes_to_numeric(b: &[u8]) -> Value {
    if let Some(i) = full_i64(b) {
        return Value::Int(i);
    }
    let r = float_prefix(b);
    let i = r as i64; // saturating
    const LIM: i64 = 1 << 51;
    if i as f64 == r && i > -LIM && i < LIM {
        Value::Int(i)
    } else {
        Value::Float(r)
    }
}

/// `Some(i)` iff the whole string (ignoring leading/trailing whitespace) is a
/// valid `i64` integer literal — sqlite's `sqlite3Atoi64` returning 0. Any
/// non-whitespace trailing byte, `.`/`e`, or overflow → `None`.
fn full_i64(b: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digit_start = i;
    // Accumulate the magnitude in u64, as sqlite's `sqlite3Atoi64` does, so the
    // i64::MIN magnitude (2^63, one past i64::MAX) still parses as an integer.
    let mut acc: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        let d = (b[i] - b'0') as u64;
        acc = acc.checked_mul(10).and_then(|a| a.checked_add(d))?;
        i += 1;
    }
    if i == digit_start {
        return None; // no digits
    }
    let mut j = i;
    while j < b.len() && is_sql_space(b[j]) {
        j += 1;
    }
    if j != b.len() {
        return None; // trailing non-whitespace (incl. '.'/'e')
    }
    if neg {
        // Magnitude fits iff acc <= 2^63; acc == 2^63 negates to i64::MIN.
        (acc <= (1u64 << 63)).then(|| acc.wrapping_neg() as i64)
    } else {
        (acc <= i64::MAX as u64).then_some(acc as i64)
    }
}

/// sqlite's **store-time** affinity conversion: what happens to a value on its
/// way INTO a column. This is NOT [`cast_value`], and the two deliberately
/// disagree — verified against sqlite3 3.45.1:
///
/// | expression                       | `CAST` | stored in a `NUMERIC` column |
/// |----------------------------------|--------|------------------------------|
/// | `'12abc'`                        | `12`   | `'12abc'` (TEXT)             |
/// | `'1e18'`                         | `1.0e18` (real) | `1000000000000000000` (int) |
/// | `x'3132'`                        | `12`   | `x'3132'` (BLOB)             |
///
/// `CAST` parses a numeric PREFIX and always yields a number; store-time
/// affinity converts only when the conversion is lossless and reversible, and
/// otherwise leaves the value exactly as it was. Mixing them up is a wrong
/// answer, which is why they are separate functions rather than one with a
/// flag.
///
/// This is applied ONLY to a [`ColumnType::Any`](crate::ColumnType::Any)
/// column, which is mpedb's per-value column and therefore the only one that
/// can hold whatever the conversion produces. Every rigid column REFUSES a
/// mismatched value instead — narrower than sqlite, never a different answer —
/// so [`ColumnDef::converts_on_store`](crate::ColumnDef::converts_on_store) is
/// what decides, not this function.
///
/// The five affinities, all differentially checked against sqlite3 3.45.1:
///
/// * **NUMERIC** and **INTEGER** — identical at store time (sqlite's own docs
///   say so, and `applyAffinity` really does take the same branch for both). A
///   TEXT value is converted only if the WHOLE string (ignoring surrounding
///   whitespace) is a well-formed decimal literal: `'1.50'`→`1.5`,
///   `'0012'`→`12`, `'1e3'`→`1000`, `'  7  '`→`7`. `'abc'`, `''`, `'12abc'`,
///   `'0x10'`, `'1_000'`, `'inf'` and `'2024-01-01'` all stay TEXT. A pure
///   integer literal that fits an `i64` keeps its EXACT value even past 2^53
///   (`'9007199254740993'`, which no `f64` could hold); everything else becomes
///   a real and then collapses back to an integer only if it round-trips
///   through `i64` exactly. A REAL value collapses to an integer under that
///   same rule (`1.0`→`1`, `-0.0`→`0`). Blobs are NOT parsed (unlike `CAST`).
/// * **REAL** — the NUMERIC rule, and then anything that ended up an integer is
///   widened to a real, so the column reads back `12.0`, not `12`.
/// * **TEXT** — a number is rendered to its sqlite text spelling (`1`→`'1'`,
///   `1.5`→`'1.5'`, `%!.15g`); text, blobs and NULL are left alone.
/// * **BLOB** — sqlite's "NONE" affinity: nothing is converted, ever. This is
///   the affinity of a column with NO declared type.
///
/// mpedb's own `Bool` and `Timestamp` values pass through unchanged under every
/// affinity: sqlite has no such storage classes to agree or disagree with, no
/// differential test can observe them, and an `Any` column has always stored
/// them as themselves.
pub fn store_affinity(aff: Affinity, v: Value) -> Value {
    match aff {
        // NONE/BLOB affinity converts nothing — the typeless column.
        Affinity::Blob => v,
        Affinity::Integer | Affinity::Numeric => numerify(v),
        Affinity::Real => match numerify(v) {
            Value::Int(i) => Value::Float(i as f64),
            other => other,
        },
        Affinity::Text => match v {
            Value::Int(_) | Value::Float(_) => Value::Text(render_scalar_text(&v)),
            other => other,
        },
    }
}

/// The shared NUMERIC/INTEGER store-time step (`applyNumericAffinity(_, 1)`).
fn numerify(v: Value) -> Value {
    match v {
        Value::Text(s) => match numeric_from_full_text(s.as_bytes()) {
            Some(n) => n,
            None => Value::Text(s),
        },
        Value::Float(f) => match real_as_int(f) {
            Some(i) => Value::Int(i),
            None => Value::Float(f),
        },
        other => other,
    }
}

/// `Some(number)` iff the whole byte string is a well-formed numeric literal —
/// sqlite's `sqlite3AtoF` returning > 0 — else `None`, meaning "leave it TEXT".
fn numeric_from_full_text(b: &[u8]) -> Option<Value> {
    let (start, end, saw) = float_token(b);
    if !saw {
        return None;
    }
    // `sqlite3AtoF` reports a *partial* parse (rc <= 0) for anything with
    // extraneous text, and `applyNumericAffinity` then stores the string
    // unchanged. Leading whitespace was already skipped by `float_token`.
    if b[end..].iter().any(|&c| !is_sql_space(c)) {
        return None;
    }
    // The token is pure ASCII by construction; an out-of-range exponent parses
    // to ±inf, exactly as sqlite's strtod does.
    let r: f64 = std::str::from_utf8(&b[start..end]).ok()?.parse().ok()?;
    // sqlite's `alsoAnInt`: a pure integer literal keeps its exact i64 value,
    // which is how `'9007199254740993'` survives a round trip that `f64` would
    // round to ...992.
    if let Some(i) = full_i64(b) {
        return Some(Value::Int(i));
    }
    Some(match real_as_int(r) {
        Some(i) => Value::Int(i),
        None => Value::Float(r),
    })
}

/// sqlite's `sqlite3VdbeIntegerAffinity`: a real becomes an integer only when
/// the real→int→real round trip is exact AND the integer is neither `i64`
/// extreme (sqlite ticket #3922 — `9223372036854775807.0` is not a value any
/// `f64` names, so calling it that integer would not be reversible).
fn real_as_int(r: f64) -> Option<i64> {
    // sqlite3RealToI64 clamps rather than invoking UB on an out-of-range cast.
    let ix = if r <= i64::MIN as f64 {
        i64::MIN
    } else if r >= i64::MAX as f64 {
        i64::MAX
    } else {
        r as i64
    };
    // NaN fails this comparison, so a NaN stays a real.
    (r == ix as f64 && ix > i64::MIN && ix < i64::MAX).then_some(ix)
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
