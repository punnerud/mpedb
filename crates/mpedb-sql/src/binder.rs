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
    /// `LHS IN (<context list at reserved param n>)` (DESIGN-MULTIDB §2.6).
    InParam(Box<BExpr>, u16),
    /// `LHS IN (e1, …, en)` — a general value list (task #21).
    InList(Box<BExpr>, Vec<BExpr>),
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
    /// The subset of `ctx_keys` whose slot holds a [`Value::List`] for an `IN`
    /// membership test (§2.6). A list slot has no `ColumnType`, so it cannot
    /// unify with a scalar use of the same key — keeping the set explicit is
    /// what lets both bind arms reject that mix instead of silently picking one.
    ctx_list_keys: std::collections::BTreeSet<String>,
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
            ctx_list_keys: std::collections::BTreeSet::new(),
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
    /// `(param_types, context_keys, list_context_keys)`. The third is the subset
    /// of keys whose slot holds a [`Value::List`] for an `IN` test (§2.6): those
    /// legitimately have NO scalar `Ty`, so the planner's "every context slot
    /// must be type-inferable" guard has to know to skip them.
    pub fn into_parts(self) -> (Vec<Ty>, Vec<String>, std::collections::BTreeSet<String>) {
        (self.param_types, self.ctx_keys, self.ctx_list_keys)
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
                if self.ctx_list_keys.contains(key) {
                    return Err(bind_err(format!(
                        "session key `{key}` is used both as an IN list and as a scalar; \
                         a context slot is one or the other"
                    )));
                }
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
            ast::Expr::InContext(lhs, key, negated) => {
                if !self.allow_context {
                    return Err(bind_err("current_setting() is not allowed in this expression"));
                }
                let (l, _lt) = self.bind_expr(lhs)?;
                // The slot holds a LIST, which has no ColumnType — so it can
                // never unify with a scalar use of the same key. Reject that
                // outright: one slot cannot be both, and silently picking one
                // would make `k` mean different things in two conjuncts of the
                // same policy.
                if let Some(p) = self.ctx_keys.iter().position(|k| k == key) {
                    let idx = self.n_user_params as usize + p;
                    if !self.ctx_list_keys.contains(key) {
                        return Err(bind_err(format!(
                            "session key `{key}` is used both as a scalar and as an IN list;                              a context slot is one or the other"
                        )));
                    }
                    return Ok((
                        maybe_not(BExpr::InParam(Box::new(l), idx as u16), *negated),
                        Some(ColumnType::Bool),
                    ));
                }
                let idx = self.n_user_params as usize + self.ctx_keys.len();
                if idx >= u16::MAX as usize {
                    return Err(bind_err("too many parameters (including session context)"));
                }
                self.ctx_keys.push(key.clone());
                self.ctx_list_keys.insert(key.clone());
                // `None` = "no scalar column type": resolve_params keys off
                // ctx_list_keys to know a List belongs here.
                self.param_types.push(None);
                Ok((
                    maybe_not(BExpr::InParam(Box::new(l), idx as u16), *negated),
                    Some(ColumnType::Bool),
                ))
            }
            ast::Expr::InList(lhs, items, negated) => {
                // The IR encodes the arity in a u16, and the stack verifier
                // proves depth n+1; both need this bound to be real.
                if items.len() > u16::MAX as usize {
                    return Err(bind_err("IN list is too long (max 65535 values)"));
                }
                let (l, lt) = self.bind_expr(lhs)?;
                let mut all = vec![(l, lt)];
                for it in items {
                    all.push(self.bind_expr(it)?);
                }
                // Unify ALL n+1 operands at once, not pairwise against the probe.
                // Pairwise is subtly wrong: in `x IN (1, 2.5)` with x Int64, the
                // probe would be coerced to Float64 by element 2 while element 1
                // stayed Int64, and the rigid comparison would then fail at
                // runtime on a query the binder had already accepted.
                let mut all = self.unify_many(all, "compare with IN list")?;
                let l = all.remove(0);
                Ok((
                    maybe_not(BExpr::InList(Box::new(l), all), *negated),
                    Some(ColumnType::Bool),
                ))
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

    /// Unify n operands to one common type: the same rules as
    /// [`Self::unify_operands`] (bare params adopt, Int64 -> Float64 is the only
    /// coercion, anything else cross-type is an error) but applied across the
    /// whole set, so no operand is left behind at a type the others moved off.
    fn unify_many(&mut self, operands: Vec<(BExpr, Ty)>, verb: &str) -> Result<Vec<BExpr>> {
        // Target type = the one every non-param operand agrees on, widened to
        // Float64 if ints and floats are mixed.
        let mut target: Ty = None;
        for (_, t) in &operands {
            let Some(t) = *t else { continue };
            target = Some(match target {
                None => t,
                Some(prev) if prev == t => prev,
                Some(ColumnType::Int64) if t == ColumnType::Float64 => ColumnType::Float64,
                Some(ColumnType::Float64) if t == ColumnType::Int64 => ColumnType::Float64,
                Some(prev) => return Err(bind_err(format!("cannot {verb}: {prev} and {t}"))),
            });
        }
        let Some(target) = target else {
            // Nothing pinned the type (all NULLs / bare params). Leave them be;
            // resolve_params reports an unresolved param.
            return Ok(operands.into_iter().map(|(e, _)| e).collect());
        };
        let mut out = Vec::with_capacity(operands.len());
        for (e, t) in operands {
            let (e, t) = self.unify_param(e, t, target);
            out.push(match t {
                Some(ColumnType::Int64) if target == ColumnType::Float64 => {
                    fold(BExpr::Unary(BUnOp::ToFloat, Box::new(e)))?
                }
                _ => e,
            });
        }
        Ok(out)
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

/// Wrap in NOT when the source said `NOT IN`. Deliberately a real `Not` over
/// the 3VL result rather than an inverted membership test: `NOT IN` must yield
/// NULL (not TRUE) when the list holds a NULL and nothing matched, and NOT of
/// NULL is NULL — so the plain negation is exactly right, and reimplementing it
/// would be a second place for the NULL rules to drift.
fn maybe_not(e: BExpr, negated: bool) -> BExpr {
    if negated {
        BExpr::Unary(BUnOp::Not, Box::new(e))
    } else {
        e
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
        // Never foldable: the list is a session value, not a literal.
        BExpr::InParam(..) => false,
        // Foldable in principle (`2 IN (1,2)` is TRUE), but deliberately not:
        // the fold path evaluates via ExprProgram over a const-only program, and
        // an all-const IN list is not worth a special case. It stays a runtime
        // InList — correct, just not folded.
        BExpr::InList(..) => false,
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
        BExpr::InParam(a, idx) => {
            emit(a, instrs, consts)?;
            instrs.push(Instr::InParam(*idx));
        }
        BExpr::InList(a, items) => {
            // Probe first, then the elements on top of it: InList(n) pops n
            // elements and finds the probe beneath them.
            emit(a, instrs, consts)?;
            for it in items {
                emit(it, instrs, consts)?;
            }
            instrs.push(Instr::InList(items.len() as u16));
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
