//! Name resolution, rigid type checking, parameter-type unification,
//! constant folding, and compilation of bound expressions to
//! [`mpedb_types::ExprProgram`].
//!
//! Typing rules (rigid): comparisons and arithmetic require identical types.
//! The single implicit coercion is Int64 -> Float64 (`Instr::ToFloat`,
//! constant-folded when the operand is a literal). Parameters acquire types
//! by unification from context, left to right; a bare unconstrained parameter
//! adopts the type of whatever it first meets. Expressions whose type cannot
//! be pinned (e.g. arithmetic over two unconstrained parameters) stay
//! unconstrained and are validated at execute time.

use crate::ast::{self, BinOp, UnOp};
use mpedb_types::{ColumnDef, ColumnType, Error, ExprProgram, Instr, Result, TableDef, Value};

/// Bound (name-resolved, type-checked, constant-folded) expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BExpr {
    Const(Value),
    Param(u16),
    Col(u16),
    Unary(BUnOp, Box<BExpr>),
    Binary(BinOp, Box<BExpr>, Box<BExpr>),
    /// LHS LIKE 'pattern' (pattern is always a literal in Phase 1).
    Like(Box<BExpr>, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BUnOp {
    Neg,
    Not,
    IsNull,
    IsNotNull,
    ToFloat,
}

/// Expression type: `None` = NULL literal or not yet constrained.
pub(crate) type Ty = Option<ColumnType>;

pub(crate) struct Binder<'a> {
    pub table: &'a TableDef,
    /// Types of ALL parameters: the `n_user_params` caller params first, then
    /// one appended reserved slot per distinct `current_setting()` key (in
    /// `ctx_keys` order). `current_setting()` refs bind to `Param(n_user + pos)`
    /// and are filled from the session at execute time (DESIGN-MULTIDB.md §2).
    pub param_types: Vec<Ty>,
    /// Number of caller-facing parameters; reserved context slots start here.
    n_user_params: u16,
    /// Distinct session-context keys, in first-reference order; index `p` maps
    /// to reserved parameter `n_user_params + p`.
    ctx_keys: Vec<String>,
    allow_params: bool,
    allow_context: bool,
}

fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

impl<'a> Binder<'a> {
    pub fn new(table: &'a TableDef, n_params: u16, allow_params: bool) -> Binder<'a> {
        Binder {
            table,
            param_types: vec![None; n_params as usize],
            n_user_params: n_params,
            ctx_keys: Vec::new(),
            allow_params,
            // `current_setting()` is allowed wherever caller params are (queries
            // and, later, policy predicates); disallowed in CHECK constraints.
            allow_context: allow_params,
        }
    }

    /// Consume the binder, yielding the full parameter-type vector (user
    /// params followed by the reserved context slots, in `ctx_keys` order) and
    /// the distinct session-context keys. Slot `p` is parameter index
    /// `n_user_params + p`, with type `param_types[n_user_params + p]`.
    pub fn into_parts(self) -> (Vec<Ty>, Vec<String>) {
        (self.param_types, self.ctx_keys)
    }

    /// Bind a WHERE predicate: must type to bool (or NULL).
    pub fn bind_predicate(&mut self, e: &ast::Expr) -> Result<BExpr> {
        let (b, ty) = self.bind_expr(e)?;
        let (b, ty) = self.unify_param(b, ty, ColumnType::Bool);
        match ty {
            None | Some(ColumnType::Bool) => Ok(b),
            Some(t) => Err(bind_err(format!(
                "predicate must be a boolean expression, got {t}"
            ))),
        }
    }

    /// Bind a CHECK expression: must type to bool, strictly.
    pub fn bind_check(&mut self, e: &ast::Expr) -> Result<BExpr> {
        let (b, ty) = self.bind_expr(e)?;
        match ty {
            Some(ColumnType::Bool) => Ok(b),
            Some(t) => Err(bind_err(format!(
                "CHECK expression must be boolean, got {t}"
            ))),
            None => Err(bind_err("CHECK expression must be boolean")),
        }
    }

    /// Bind an expression assigned to a column (UPDATE SET): unify a bare
    /// parameter to the column type, apply the Int64 -> Float64 coercion,
    /// reject cross-type and statically-NULL-into-NOT-NULL assignments.
    pub fn bind_assign(&mut self, e: &ast::Expr, col: &ColumnDef) -> Result<BExpr> {
        let (b, ty) = self.bind_expr(e)?;
        let (b, ty) = self.unify_param(b, ty, col.ty);
        match ty {
            Some(t) if t == col.ty => Ok(b),
            Some(ColumnType::Int64) if col.ty == ColumnType::Float64 => {
                fold(BExpr::Unary(BUnOp::ToFloat, Box::new(b)))
            }
            Some(t) => Err(bind_err(format!(
                "cannot assign {t} to column `{}` of type {}",
                col.name, col.ty
            ))),
            None => {
                if let BExpr::Const(v) = &b {
                    if v.is_null() && !col.nullable {
                        return Err(bind_err(format!(
                            "cannot assign NULL to NOT NULL column `{}`",
                            col.name
                        )));
                    }
                }
                Ok(b)
            }
        }
    }

    /// Bind an expression bottom-up; returns the folded expression + type.
    pub fn bind_expr(&mut self, e: &ast::Expr) -> Result<(BExpr, Ty)> {
        match e {
            ast::Expr::Lit(v) => Ok((BExpr::Const(v.clone()), v.column_type())),
            ast::Expr::Param(i) => {
                if !self.allow_params {
                    return Err(bind_err("parameters are not allowed in this expression"));
                }
                // Guaranteed in range: the parser sized n_params to max index.
                Ok((BExpr::Param(*i), self.param_types[*i as usize]))
            }
            ast::Expr::Col(name) => {
                let idx = self.table.column_index(name).ok_or_else(|| {
                    bind_err(format!(
                        "unknown column `{name}` in table `{}`",
                        self.table.name
                    ))
                })?;
                Ok((BExpr::Col(idx), Some(self.table.columns[idx as usize].ty)))
            }
            ast::Expr::Unary(UnOp::Neg, a) => {
                let (a, at) = self.bind_expr(a)?;
                match at {
                    None | Some(ColumnType::Int64) | Some(ColumnType::Float64) => {}
                    Some(t) => return Err(bind_err(format!("cannot negate {t}"))),
                }
                let e = fold(BExpr::Unary(BUnOp::Neg, Box::new(a)))?;
                Ok((e, at))
            }
            ast::Expr::Unary(UnOp::Not, a) => {
                let (a, at) = self.bind_expr(a)?;
                let (a, at) = self.unify_param(a, at, ColumnType::Bool);
                match at {
                    None | Some(ColumnType::Bool) => {}
                    Some(t) => return Err(bind_err(format!("NOT requires a boolean, got {t}"))),
                }
                let e = fold(BExpr::Unary(BUnOp::Not, Box::new(a)))?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::IsNull(a, negated) => {
                let (a, _) = self.bind_expr(a)?;
                let op = if *negated {
                    BUnOp::IsNotNull
                } else {
                    BUnOp::IsNull
                };
                let e = fold(BExpr::Unary(op, Box::new(a)))?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::Like(lhs, pat) => {
                let pattern = match pat.as_ref() {
                    ast::Expr::Lit(Value::Text(p)) => p.clone(),
                    ast::Expr::Param(_) => {
                        return Err(bind_err("LIKE pattern must be a literal in Phase 1"))
                    }
                    _ => return Err(bind_err("LIKE pattern must be a string literal")),
                };
                let (l, lt) = self.bind_expr(lhs)?;
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                match lt {
                    None | Some(ColumnType::Text) => {}
                    Some(t) => return Err(bind_err(format!("LIKE requires text, got {t}"))),
                }
                let e = fold(BExpr::Like(Box::new(l), pattern))?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::ContextRef(key) => {
                if !self.allow_context {
                    return Err(bind_err("current_setting() is not allowed in this expression"));
                }
                // One reserved parameter per distinct key, appended after the
                // caller params. The value is filled from the session at exec;
                // the type is inferred exactly like a bare parameter (unified
                // from whatever it is compared to).
                let pos = match self.ctx_keys.iter().position(|k| k == key) {
                    Some(p) => p,
                    None => {
                        let idx = self.n_user_params as usize + self.ctx_keys.len();
                        if idx >= u16::MAX as usize {
                            return Err(bind_err("too many parameters (including session context)"));
                        }
                        self.ctx_keys.push(key.clone());
                        self.param_types.push(None);
                        self.ctx_keys.len() - 1
                    }
                };
                let idx = self.n_user_params + pos as u16;
                Ok((BExpr::Param(idx), self.param_types[idx as usize]))
            }
            ast::Expr::Binary(op, l, r) => self.bind_binary(*op, l, r),
        }
    }

    fn bind_binary(&mut self, op: BinOp, l: &ast::Expr, r: &ast::Expr) -> Result<(BExpr, Ty)> {
        let (l, lt) = self.bind_expr(l)?;
        let (r, rt) = self.bind_expr(r)?;
        match op {
            BinOp::And | BinOp::Or => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Bool);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Bool);
                for t in [lt, rt].into_iter().flatten() {
                    if t != ColumnType::Bool {
                        return Err(bind_err(format!(
                            "AND/OR requires boolean operands, got {t}"
                        )));
                    }
                }
                let e = fold(BExpr::Binary(op, Box::new(l), Box::new(r)))?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let (l, r, _) = self.unify_operands(l, lt, r, rt, "compare")?;
                let e = fold(BExpr::Binary(op, Box::new(l), Box::new(r)))?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let (l, r, ty) = self.unify_operands(l, lt, r, rt, "arithmetic on")?;
                if let Some(t) = ty {
                    if t != ColumnType::Int64 && t != ColumnType::Float64 {
                        return Err(bind_err(format!(
                            "arithmetic requires int64 or float64 operands, got {t}"
                        )));
                    }
                }
                let e = fold(BExpr::Binary(op, Box::new(l), Box::new(r)))?;
                Ok((e, ty))
            }
        }
    }

    /// Make both operands the same type: unify bare parameters, apply the one
    /// legal coercion (Int64 -> Float64), reject everything else cross-type.
    /// Returns the (possibly coerced) operands and the common type
    /// (`None` when it could not be pinned).
    fn unify_operands(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
        verb: &str,
    ) -> Result<(BExpr, BExpr, Ty)> {
        // A bare unconstrained param adopts the other side's type.
        let (l, lt) = match rt {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match lt {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        match (lt, rt) {
            (Some(a), Some(b)) if a == b => Ok((l, r, Some(a))),
            (Some(ColumnType::Int64), Some(ColumnType::Float64)) => {
                let l = fold(BExpr::Unary(BUnOp::ToFloat, Box::new(l)))?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            (Some(ColumnType::Float64), Some(ColumnType::Int64)) => {
                let r = fold(BExpr::Unary(BUnOp::ToFloat, Box::new(r)))?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            (Some(a), Some(b)) => Err(bind_err(format!("cannot {verb} {a} and {b}"))),
            (Some(t), None) | (None, Some(t)) => Ok((l, r, Some(t))),
            (None, None) => Ok((l, r, None)),
        }
    }

    /// If `e` is a bare parameter with no inferred type yet, pin it to `ty`.
    fn unify_param(&mut self, e: BExpr, t: Ty, ty: ColumnType) -> (BExpr, Ty) {
        if t.is_none() {
            if let BExpr::Param(i) = e {
                if self.param_types[i as usize].is_none() {
                    self.param_types[i as usize] = Some(ty);
                    return (e, Some(ty));
                }
            }
        }
        (e, t)
    }
}

/// Constant-fold one node whose children are already folded: if every child
/// is a constant, evaluate now (via the same IR evaluator used at run time,
/// so semantics — including division-by-zero errors — match exactly).
pub(crate) fn fold(e: BExpr) -> Result<BExpr> {
    let foldable = match &e {
        BExpr::Unary(_, a) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Binary(_, a, b) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::Like(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        _ => false,
    };
    if !foldable {
        return Ok(e);
    }
    let program = compile_program(&e)?;
    let v = program.eval(&[], &[])?;
    Ok(BExpr::Const(v))
}

/// Compile a bound expression to the shared stack IR.
pub(crate) fn compile_program(e: &BExpr) -> Result<ExprProgram> {
    let mut instrs = Vec::new();
    let mut consts = Vec::new();
    emit(e, &mut instrs, &mut consts)?;
    ExprProgram::new(instrs, consts)
        .map_err(|err| Error::Internal(format!("codegen produced invalid program: {err}")))
}

fn emit(e: &BExpr, instrs: &mut Vec<Instr>, consts: &mut Vec<Value>) -> Result<()> {
    match e {
        BExpr::Const(v) => {
            let idx = push_const(consts, v.clone())?;
            instrs.push(Instr::PushConst(idx));
        }
        BExpr::Param(i) => instrs.push(Instr::PushParam(*i)),
        BExpr::Col(i) => instrs.push(Instr::PushCol(*i)),
        BExpr::Unary(op, a) => {
            emit(a, instrs, consts)?;
            instrs.push(match op {
                BUnOp::Neg => Instr::Neg,
                BUnOp::Not => Instr::Not,
                BUnOp::IsNull => Instr::IsNull,
                BUnOp::IsNotNull => Instr::IsNotNull,
                BUnOp::ToFloat => Instr::ToFloat,
            });
        }
        BExpr::Binary(op, a, b) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(match op {
                BinOp::Add => Instr::Add,
                BinOp::Sub => Instr::Sub,
                BinOp::Mul => Instr::Mul,
                BinOp::Div => Instr::Div,
                BinOp::Mod => Instr::Mod,
                BinOp::Eq => Instr::Eq,
                BinOp::Ne => Instr::Ne,
                BinOp::Lt => Instr::Lt,
                BinOp::Le => Instr::Le,
                BinOp::Gt => Instr::Gt,
                BinOp::Ge => Instr::Ge,
                BinOp::And => Instr::And,
                BinOp::Or => Instr::Or,
            });
        }
        BExpr::Like(a, pattern) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            instrs.push(Instr::Like(idx));
        }
    }
    Ok(())
}

fn push_const(consts: &mut Vec<Value>, v: Value) -> Result<u16> {
    if consts.len() >= u16::MAX as usize {
        return Err(bind_err("expression has too many constants"));
    }
    consts.push(v);
    Ok((consts.len() - 1) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_expr_only;
    use mpedb_types::ColumnDef;

    fn table() -> TableDef {
        let col = |name: &str, ty: ColumnType, nullable: bool| ColumnDef {
            name: name.into(),
            ty,
            nullable,
            unique: false,
            default: None,
            check: None,
        };
        TableDef {
            name: "t".into(),
            columns: vec![
                col("id", ColumnType::Int64, false),
                col("score", ColumnType::Float64, true),
                col("name", ColumnType::Text, true),
                col("active", ColumnType::Bool, true),
                col("data", ColumnType::Blob, true),
                col("created", ColumnType::Timestamp, true),
            ],
            primary_key: vec![0],
        }
    }

    fn bind(src: &str, n_params: u16) -> Result<(BExpr, Ty, Vec<Ty>)> {
        let (ast, n) = parse_expr_only(src)?;
        assert!(n <= n_params, "test forgot params");
        let t = table();
        let mut b = Binder::new(&t, n_params, true);
        let (e, ty) = b.bind_expr(&ast)?;
        Ok((e, ty, b.param_types))
    }

    #[test]
    fn rigid_cross_type_rejections() {
        for src in [
            "name = 1",
            "id = 'x'",
            "active = 1",
            "id + 'x'",
            "name + name",
            "created = 1",
            "data = 'x'",
            "-name",
            "NOT id",
            "id AND active",
            "name LIKE 1",
        ] {
            assert!(
                matches!(bind(src, 0), Err(Error::Bind(_))),
                "expected bind error for {src}"
            );
        }
    }

    #[test]
    fn int_to_float_coercion_and_folding() {
        // Column int meets float literal: column side gets ToFloat.
        let (e, ty, _) = bind("id < 1.5", 0).unwrap();
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Lt,
                Box::new(BExpr::Unary(BUnOp::ToFloat, Box::new(BExpr::Col(0)))),
                Box::new(BExpr::Const(Value::Float(1.5)))
            )
        );
        // Both literals: fully folded, int coerced.
        let (e, ty, _) = bind("1 + 2.5", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Float(3.5)));
        assert_eq!(ty, Some(ColumnType::Float64));
        // Pure-int folding.
        let (e, _, _) = bind("2 + 3 * 4", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Int(14)));
        // Bool folding through comparisons and logic.
        let (e, _, _) = bind("1 < 2 AND NOT false", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
        // LIKE folding.
        let (e, _, _) = bind("'hello' LIKE 'he%'", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
    }

    #[test]
    fn fold_time_division_by_zero_is_the_runtime_error() {
        assert!(matches!(bind("1 / 0", 0), Err(Error::DivisionByZero)));
        assert!(matches!(bind("1 % 0", 0), Err(Error::DivisionByZero)));
        assert!(matches!(
            bind("9223372036854775807 + 1", 0),
            Err(Error::ArithmeticOverflow)
        ));
    }

    #[test]
    fn param_unification() {
        // Param adopts column type.
        let (_, _, params) = bind("id = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        let (_, _, params) = bind("name = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Text)]);
        // Bool context.
        let (_, _, params) = bind("$1 AND active", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Bool)]);
        // LIKE lhs.
        let (_, _, params) = bind("$1 LIKE 'x%'", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Text)]);
        // Same param twice, consistent.
        let (_, _, params) = bind("id = $1 AND $1 < 10", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        // Unused param stays unconstrained.
        let (_, _, params) = bind("id = $2", 2).unwrap();
        assert_eq!(params, vec![None, Some(ColumnType::Int64)]);
    }

    #[test]
    fn param_unification_conflicts() {
        // $1 pinned to text, then used where int is required.
        assert!(matches!(
            bind("name = $1 AND id = $1", 1),
            Err(Error::Bind(_))
        ));
        // Int-typed param in float context is legal (ToFloat at use site).
        let (e, _, params) = bind("id = $1 AND score = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        // The second use wraps the param in ToFloat.
        let s = format!("{e:?}");
        assert!(s.contains("ToFloat"), "expected ToFloat in {s}");
    }

    #[test]
    fn like_pattern_must_be_literal() {
        match bind("name LIKE $1", 1) {
            Err(Error::Bind(m)) => assert!(m.contains("literal in Phase 1")),
            other => panic!("expected bind error, got {other:?}"),
        }
        assert!(bind("name LIKE name", 0).is_err());
    }

    #[test]
    fn unknown_column() {
        match bind("nope = 1", 0) {
            Err(Error::Bind(m)) => assert!(m.contains("nope")),
            other => panic!("expected bind error, got {other:?}"),
        }
    }

    #[test]
    fn predicate_typing() {
        let t = table();
        let mut b = Binder::new(&t, 0, true);
        let (ast, _) = parse_expr_only("42").unwrap();
        assert!(matches!(b.bind_predicate(&ast), Err(Error::Bind(_))));
        let (ast, _) = parse_expr_only("id = 42").unwrap();
        assert!(b.bind_predicate(&ast).is_ok());
        // NULL predicate is legal (never passes).
        let (ast, _) = parse_expr_only("NULL").unwrap();
        assert!(b.bind_predicate(&ast).is_ok());
        // Bare param in predicate position becomes bool.
        let mut b = Binder::new(&t, 1, true);
        let (ast, _) = parse_expr_only("$1").unwrap();
        b.bind_predicate(&ast).unwrap();
        assert_eq!(b.param_types, vec![Some(ColumnType::Bool)]);
    }

    #[test]
    fn no_params_mode() {
        let t = table();
        let mut b = Binder::new(&t, 1, false);
        let (ast, _) = parse_expr_only("id = $1").unwrap();
        assert!(matches!(b.bind_expr(&ast), Err(Error::Bind(_))));
    }

    #[test]
    fn null_comparisons_fold_to_null() {
        let (e, _, _) = bind("1 = NULL", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Null));
        let (e, _, _) = bind("NULL IS NULL", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
    }

    #[test]
    fn compiled_program_evaluates() {
        let (e, _, _) = bind("id + 1 < 10", 0).unwrap();
        let p = compile_program(&e).unwrap();
        assert_eq!(
            p.eval(&[Value::Int(5), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null], &[])
                .unwrap(),
            Value::Bool(true)
        );
    }
}
