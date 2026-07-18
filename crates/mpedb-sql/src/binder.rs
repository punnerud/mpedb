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
use mpedb_types::{
    ColumnDef, ColumnType, Error, ExprProgram, Instr, Result, ScalarFn, TableDef, Value,
};

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
    /// `CASE WHEN c THEN r … ELSE e END`. `else_` is None for a missing ELSE
    /// (SQL: NULL).
    Case(Vec<(BExpr, BExpr)>, Option<Box<BExpr>>),
    /// A built-in scalar function over already-typed arguments.
    Call(ScalarFn, Vec<BExpr>),
    /// `coalesce(a, b, …)` — compiled to control flow, not a call, so later
    /// arguments are never evaluated once an earlier one is non-NULL.
    Coalesce(Vec<BExpr>),
    /// `CAST(x AS t)` — semantics live in [`Instr::Cast`](mpedb_types::expr).
    Cast(Box<BExpr>, ColumnType),
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


/// The tables a statement can name, and how a column reference resolves to a
/// slot in the row the expression will see.
///
/// **Why this exists as a type instead of a `&TableDef` field.** The binder held
/// exactly one table, so "which table is this column in" was never a question —
/// and every layer above inherited that assumption without stating it. The
/// footprint never did: `tables_read`/`tables_written` are bitmaps over
/// `MAX_TABLES` and `conflicts_with` is a bitmap AND, so a multi-table access set
/// has always been *representable*. The binder is what made it unreachable.
///
/// Today a scope holds one table and this is a pure refactor — same resolution,
/// same errors, no new SQL. It exists so the next step (a second table) changes
/// this type rather than 45 call sites, and so the rule that matters is written
/// down in ONE place: **a column resolves to an offset into the tuple the
/// expression is evaluated over.** For a single table that is the row itself. For
/// `ON CONFLICT DO UPDATE` it is already `[existing ‖ proposed]`, which is why
/// `excluded.<c>` binds to `Col(n + i)`. For a join it will be the concatenation
/// of the joined rows. Same rule, wider tuple.
pub(crate) struct Scope<'a> {
    /// Tables in tuple order. The slot base of table `k` is the sum of the widths
    /// before it.
    tables: Vec<&'a TableDef>,
    /// The name each table is ADDRESSED by — its alias if the query gave one,
    /// else its own name. Parallel to `tables`. Qualified resolution matches
    /// against this, which is what implements PG's rule that `FROM orders o`
    /// puts `o` in scope and NOT `orders`, and what lets a table join itself
    /// under two different names.
    names: Vec<String>,
}

impl<'a> Scope<'a> {
    pub fn single(t: &'a TableDef) -> Scope<'a> {
        Scope { names: vec![t.name.clone()], tables: vec![t] }
    }

    /// Single table addressed by `name` (an alias). `FROM orders o WHERE o.id`.
    pub fn single_named(name: String, t: &'a TableDef) -> Scope<'a> {
        Scope { names: vec![name], tables: vec![t] }
    }

    /// A join's scope, each table addressed by an explicit (possibly aliased)
    /// name. Tuple order IS the order given: the outer table's columns come
    /// first, so its slots are its own column indices — which is what lets an
    /// outer-only predicate be handed to the single-table access extractor
    /// unchanged.
    pub fn joined_named(named: Vec<(String, &'a TableDef)>) -> Result<Scope<'a>> {
        // Two tables addressed by the SAME name make `x.c` ambiguous with no way
        // to say which side. That is a self-join with no (or duplicate) aliases;
        // refuse it, but a self-join with two distinct aliases is now fine.
        for (i, (a, _)) in named.iter().enumerate() {
            for (b, _) in &named[i + 1..] {
                if a.eq_ignore_ascii_case(b) {
                    return Err(bind_err(format!(
                        "`{a}` is used for two tables in this statement: give each side of a \
                         self-join a distinct alias (`FROM t a JOIN t b ON …`)"
                    )));
                }
            }
        }
        let (names, tables) = named.into_iter().unzip();
        Ok(Scope { names, tables })
    }


    /// The only table, for the paths that are still single-table by
    /// construction (INSERT's target, RLS policy binding, `excluded.`).
    /// Panics if the scope is wider — a caller that reaches for "the" table of a
    /// join has a bug that must not be papered over with an arbitrary choice.
    pub fn only(&self) -> &'a TableDef {
        assert_eq!(
            self.tables.len(),
            1,
            "Scope::only() on a {}-table scope: this path has not been taught about joins",
            self.tables.len()
        );
        self.tables[0]
    }

    /// Slot offset of table `k`'s first column in the evaluated tuple.
    fn base(&self, k: usize) -> usize {
        self.tables[..k].iter().map(|t| t.columns.len()).sum()
    }

    /// Total tuple width.
    pub fn width(&self) -> usize {
        self.base(self.tables.len())
    }

    /// Resolve an UNQUALIFIED column name. Ambiguity is an error, never a
    /// silent pick: with one table it cannot happen, and the day it can, a
    /// wrong guess is a wrong-table read.
    pub fn resolve(&self, name: &str) -> Result<(u16, ColumnType)> {
        let mut found: Option<(u16, ColumnType)> = None;
        for (k, t) in self.tables.iter().enumerate() {
            if let Some(i) = t.column_index(name) {
                let slot = (self.base(k) + i as usize) as u16;
                if found.is_some() {
                    return Err(bind_err(format!(
                        "column `{name}` is ambiguous: qualify it with a table name"
                    )));
                }
                found = Some((slot, t.columns[i as usize].ty));
            }
        }
        found.ok_or_else(|| {
            bind_err(format!(
                "unknown column `{name}` in {}",
                self.describe()
            ))
        })
    }

    /// Name a slot for humans: bare with one table, `<table>.<column>` with
    /// more — because `did` alone would not say which side it came from, and
    /// both sides usually have one.
    ///
    /// The single place that answers "what is slot N called", so EXPLAIN, the
    /// output header and an error message cannot drift apart.
    /// Column types of the whole tuple, in slot order — the concatenation of
    /// the scoped tables' columns.
    pub fn slot_types(&self) -> Vec<ColumnType> {
        self.tables
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| c.ty))
            .collect()
    }

    pub fn slot_name(&self, c: u16) -> String {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                let col = &t.columns[c as usize - base].name;
                return if self.tables.len() == 1 {
                    col.clone()
                } else {
                    format!("{}.{}", t.name, col)
                };
            }
            base += t.columns.len();
        }
        format!("col#{c}")
    }

    /// Resolve a QUALIFIED `<table>.<column>`. The qualifier is checked rather
    /// than dropped: accepting `nonsense.id` as `id` turns a typo into a
    /// wrong-table read the moment a scope holds more than one table.
    pub fn resolve_qualified(&self, qual: &str, name: &str) -> Result<(u16, ColumnType)> {
        for (k, t) in self.tables.iter().enumerate() {
            if self.names[k].eq_ignore_ascii_case(qual) {
                let i = t.column_index(name).ok_or_else(|| {
                    bind_err(format!("unknown column `{qual}.{name}`"))
                })?;
                return Ok((
                    (self.base(k) + i as usize) as u16,
                    t.columns[i as usize].ty,
                ));
            }
        }
        Err(bind_err(format!(
            "no table named `{qual}` in this statement ({})",
            self.describe()
        )))
    }

    fn describe(&self) -> String {
        // Report the names the query addresses tables by (aliases), so an
        // "unknown table `x`" points at what the user actually wrote.
        match self.names.len() {
            1 => format!("table `{}`", self.names[0]),
            _ => format!(
                "tables {}",
                self.names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

pub(crate) struct Binder<'a> {
    /// Is `excluded.<col>` in scope? Only inside `ON CONFLICT DO UPDATE`.
    ///
    /// When set, an `excluded` reference binds to `Col(n_cols + i)`: the
    /// executor evaluates these programs over the EXISTING row concatenated
    /// with the PROPOSED row, so the second half is the proposed values. That
    /// needs no new instruction and no second column namespace in the IR — a
    /// column index is a column index.
    allow_excluded: bool,
    /// Are we binding a branch that constant control flow may delete
    /// unevaluated? Then do not fold it, because folding RAISES.
    ///
    /// PostgreSQL's rule, measured rather than assumed (live PG 16):
    ///   EXPLAIN SELECT 1/0                        -> ERROR at PLAN time
    ///   SELECT coalesce(1, 1/0)                   -> 1
    ///   SELECT coalesce(NULL, 1/0)                -> ERROR
    ///   SELECT CASE WHEN true  THEN 1 ELSE 1/0 END -> 1
    ///   SELECT CASE WHEN false THEN 1 ELSE 1/0 END -> ERROR
    ///
    /// So folding is not "never raise" (that would let `SELECT 1/0` prepare
    /// cleanly and fail at every execute) and not "always raise" (that kills
    /// `coalesce(1, 1/0)`, which both ancestors answer). It is: fold the
    /// CONTROL FLOW first, drop the branch that cannot be taken WITHOUT
    /// evaluating it, then fold whatever survives — and let that raise.
    suppress_fold: bool,
    /// The tables this statement may name. See [`Scope`].
    pub scope: Scope<'a>,
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
        Binder::with_scope(Scope::single(table), n_params, allow_params)
    }

    pub fn with_scope(scope: Scope<'a>, n_params: u16, allow_params: bool) -> Binder<'a> {
        Binder {
            allow_excluded: false,
            suppress_fold: false,
            scope,
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

    /// Pin a parameter slot's type before binding — used for the reserved
    /// subplan-result slots, whose types the planner KNOWS from the inner
    /// select's output rather than inferring from usage.
    pub fn pin_param(&mut self, i: u16, ty: Option<ColumnType>) {
        self.param_types[i as usize] = ty;
    }

    /// Move this binder's PARAMETER and CONTEXT state onto a new scope.
    ///
    /// An aggregate query binds in two passes over two different tuples — the
    /// aggregate arguments over the base row, then the projection and HAVING
    /// over the grouped tuple `[keys ‖ aggs]`. Both passes must share one
    /// parameter table: `$1` means the same slot on either side, and a type
    /// pinned by `sum(qty * $1)` has to be visible to the projection. Starting a
    /// second binder from scratch would give the two passes separate parameter
    /// universes and silently accept `$1` meaning two things.
    /// Width of the tuple this binder's expressions evaluate over.
    pub fn scope_width(&self) -> usize {
        self.scope.width()
    }

    pub fn rescope<'b>(self, scope: Scope<'b>) -> Binder<'b> {
        Binder {
            scope,
            param_types: self.param_types,
            n_user_params: self.n_user_params,
            ctx_keys: self.ctx_keys,
            ctx_list_keys: self.ctx_list_keys,
            allow_params: self.allow_params,
            allow_context: self.allow_context,
            // Neither survives a scope change: `excluded.` belongs to ON
            // CONFLICT, and fold suppression to whichever branch set it.
            allow_excluded: false,
            suppress_fold: false,
        }
    }

    /// Bring `excluded.<col>` in or out of scope (ON CONFLICT DO UPDATE only).
    pub fn set_allow_excluded(&mut self, on: bool) {
        self.allow_excluded = on;
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
            // `any` is the loose-type escape (#23): every runtime-typed value
            // belongs, so a statically-typed assignment is never a type error.
            Some(_) if col.ty == ColumnType::Any => Ok(b),
            Some(ColumnType::Int64) if col.ty == ColumnType::Float64 => {
                fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(b)), self.suppress_fold)
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
                let (idx, ty) = self.scope.resolve(name)?;
                Ok((BExpr::Col(idx), Some(ty)))
            }
            ast::Expr::Unary(UnOp::Neg, a) => {
                let (a, at) = self.bind_expr(a)?;
                match at {
                    None | Some(ColumnType::Int64) | Some(ColumnType::Float64) => {}
                    Some(t) => return Err(bind_err(format!("cannot negate {t}"))),
                }
                let e = fold_maybe(BExpr::Unary(BUnOp::Neg, Box::new(a)), self.suppress_fold)?;
                Ok((e, at))
            }
            ast::Expr::Unary(UnOp::Not, a) => {
                let (a, at) = self.bind_expr(a)?;
                let (a, at) = self.unify_param(a, at, ColumnType::Bool);
                match at {
                    None | Some(ColumnType::Bool) => {}
                    Some(t) => return Err(bind_err(format!("NOT requires a boolean, got {t}"))),
                }
                let e = fold_maybe(BExpr::Unary(BUnOp::Not, Box::new(a)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::IsNull(a, negated) => {
                let (a, _) = self.bind_expr(a)?;
                let op = if *negated {
                    BUnOp::IsNotNull
                } else {
                    BUnOp::IsNull
                };
                let e = fold_maybe(BExpr::Unary(op, Box::new(a)), self.suppress_fold)?;
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
                let e = fold_maybe(BExpr::Like(Box::new(l), pattern), self.suppress_fold)?;
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
                let (mut all, _) = self.unify_many(all, "compare with IN list")?;
                let l = all.remove(0);
                Ok((
                    maybe_not(BExpr::InList(Box::new(l), all), *negated),
                    Some(ColumnType::Bool),
                ))
            }
            ast::Expr::Case(arms, else_) => {
                let mut bound_conds = Vec::with_capacity(arms.len());
                let mut results = Vec::with_capacity(arms.len() + 1);
                for (c, r) in arms {
                    let (bc, ct) = self.bind_expr(c)?;
                    // A WHEN must be a predicate. mpedb is rigidly typed, so
                    // `CASE WHEN 1 THEN …` is an error here rather than sqlite's
                    // truthiness coercion — the same trade the whole engine makes.
                    match ct {
                        Some(ColumnType::Bool) | None => {}
                        Some(t) => {
                            return Err(bind_err(format!(
                                "CASE WHEN must be a bool condition, got {t}"
                            )))
                        }
                    }
                    bound_conds.push(bc);
                    let outer = self.suppress_fold;
                    self.suppress_fold = true;
                    let r = self.bind_expr(r);
                    self.suppress_fold = outer;
                    results.push(r?);
                }
                if let Some(e) = else_ {
                    let outer = self.suppress_fold;
                    self.suppress_fold = true;
                    let e = self.bind_expr(e);
                    self.suppress_fold = outer;
                    results.push(e?);
                } else {
                    // A missing ELSE is NULL, and it is a RESULT: it has to take
                    // part in unification, or `CASE WHEN c THEN 1 END` would
                    // claim type Int64 while returning NULL on the else path.
                    results.push((BExpr::Const(Value::Null), None));
                }
                // Every arm must produce one type — a CASE has a single type,
                // and this is where a mixed `THEN 1 … THEN 'x'` is caught at
                // COMPILE time instead of returning whichever type the row hit.
                let (mut unified, ty) = self.unify_result_arms(results, "mix CASE result types")?;
                let else_b = unified.pop().expect("pushed above");
                let arms_b: Vec<(BExpr, BExpr)> = bound_conds.into_iter().zip(unified).collect();
                Ok((self.fold_case(arms_b, else_b)?, ty))
            }
            ast::Expr::Qualified(qual, name) => {
                // One table in scope, so the qualifier must be it. Accepting
                // any qualifier would let `nonsense.id` silently mean `id`, and
                // when joins arrive that typo becomes a wrong-table read.
                let (idx, ty) = self.scope.resolve_qualified(qual, name)?;
                Ok((BExpr::Col(idx), Some(ty)))
            }
            ast::Expr::Excluded(name) => {
                if !self.allow_excluded {
                    return Err(bind_err(
                        "`excluded` is only in scope inside ON CONFLICT ... DO UPDATE",
                    ));
                }
                // ON CONFLICT targets exactly one table, so `only()` is right
                // here rather than a scope lookup — and if a join ever reaches
                // this path, only() asserts instead of guessing.
                let t = self.scope.only();
                let i = t
                    .columns
                    .iter()
                    .position(|c| c.name == *name)
                    .ok_or_else(|| bind_err(format!("unknown column `excluded.{name}`")))?;
                let n = t.columns.len();
                Ok((BExpr::Col((n + i) as u16), Some(t.columns[i].ty)))
            }
            // An aggregate is not a scalar and must never compile into one: a
            // scalar runs per row and yields a value; an aggregate consumes a
            // whole group and only exists after filtering and grouping. The
            // planner lifts aggregates OUT of the projection before binding
            // what is left, so reaching here means one appeared where no
            // grouping happens — a WHERE clause, a CHECK, a policy, a SET.
            //
            // `WHERE count(*) > 1` is the classic: it reads naturally and is
            // meaningless (the filter runs per row, before any group exists).
            // SQL spells that HAVING, and saying so beats "unknown function".
            ast::Expr::Agg(f, _, _) => Err(bind_err(format!(
                "{}() is an aggregate and cannot be used here — aggregates are only \
                 allowed in a SELECT list or HAVING. A per-row filter is WHERE; a \
                 filter on a GROUPED result is HAVING.",
                f.name()
            ))),
            ast::Expr::Coalesce(args) => {
                if args.is_empty() {
                    return Err(bind_err("coalesce() needs at least one argument"));
                }
                // Bind (so every argument is still TYPE-checked -- PG rejects
                // `coalesce(1, 'abc')` too) but do not fold yet: an argument
                // after a non-NULL constant is unreachable, and folding it
                // would raise for something that will never run.
                let outer = self.suppress_fold;
                self.suppress_fold = true;
                let mut bound = Vec::with_capacity(args.len());
                for a in args {
                    bound.push(self.bind_expr(a)?);
                }
                self.suppress_fold = outer;
                // All branches are the one result, so they must unify — same
                // rule as CASE, and for the same reason.
                let (bound, ty) = self.unify_result_arms(bound, "mix coalesce() argument types")?;
                Ok((self.fold_coalesce(bound)?, ty))
            }
            ast::Expr::Func(name, args) => self.bind_func(name, args),
            ast::Expr::Binary(op, l, r) => self.bind_binary(*op, l, r),
            // The planner LIFTS subqueries out (each becomes a subplan and a
            // reserved parameter) before binding. One reaching the binder is
            // therefore a subquery in a position the lift does not cover —
            // say so instead of "unknown expression".
            // The lift's IN-subquery marker: the slot holds a LIST at
            // runtime; membership is the same runtime-typed 3VL core the
            // session-context lists use, so the lhs binds free.
            ast::Expr::InParamSlot(lhs, slot, negated) => {
                let (l, _lt) = self.bind_expr(lhs)?;
                Ok((
                    maybe_not(BExpr::InParam(Box::new(l), *slot), *negated),
                    Some(ColumnType::Bool),
                ))
            }
            ast::Expr::InSubquery(..) => Err(bind_err(
                "an IN subquery here was not lifted — this expression position \
                 does not support subqueries yet",
            )),
            ast::Expr::Subquery(_) | ast::Expr::Exists(..) => Err(bind_err(
                "a subquery is not supported in this position — subqueries work in \
                 the SELECT list and WHERE of a plain (non-aggregate) SELECT",
            )),
            ast::Expr::Cast(a, t) => {
                let (a, at) = self.bind_expr(a)?;
                // `CAST(? AS t)` pins the parameter — PG's canonical way to
                // type a param, and it makes the cast the identity.
                let (a, at) = self.unify_param(a, at, *t);
                // Refuse at bind time the casts that cannot succeed on ANY
                // non-NULL value (same accept set as Instr::Cast at runtime).
                if let Some(src) = at {
                    if !cast_possible(src, *t) {
                        return Err(bind_err(format!(
                            "CAST from {src} to {t} would have to invent data"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Cast(Box::new(a), *t), self.suppress_fold)?;
                Ok((e, Some(*t)))
            }
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
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let (l, r, _) = self.unify_operands(l, lt, r, rt, "compare")?;
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Concat => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Text);
                // Same render set as the runtime: text/int/bool (Any decided
                // per value); floats are refused until formatting is pinned.
                for t in [lt, rt].into_iter().flatten() {
                    if !matches!(
                        t,
                        ColumnType::Text | ColumnType::Int64 | ColumnType::Bool | ColumnType::Any
                    ) {
                        return Err(bind_err(format!(
                            "`||` requires text, int64, or bool operands, got {t}"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Text)))
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
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
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
                let l = fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(l)), self.suppress_fold)?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            (Some(ColumnType::Float64), Some(ColumnType::Int64)) => {
                let r = fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(r)), self.suppress_fold)?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            (Some(a), Some(b)) => Err(bind_err(format!("cannot {verb} {a} and {b}"))),
            (Some(t), None) | (None, Some(t)) => Ok((l, r, Some(t))),
            (None, None) => Ok((l, r, None)),
        }
    }

    /// Fold a `coalesce`'s CONTROL FLOW, PostgreSQL-style.
    ///
    /// Leading NULL constants can never be the answer, so drop them. If what is
    /// then first is a non-NULL constant, IT is the answer and every later
    /// argument is dead — dropped without ever being folded, which is exactly
    /// why `coalesce(1, 1/0)` returns 1 instead of raising. Whatever survives is
    /// folded normally, so `coalesce(NULL, 1/0)` still raises: that divide is
    /// genuinely reachable.
    fn fold_coalesce(&mut self, args: Vec<BExpr>) -> Result<BExpr> {
        let mut live = Vec::with_capacity(args.len());
        for a in args {
            if matches!(&a, BExpr::Const(Value::Null)) {
                continue; // a NULL constant is never the result
            }
            let dead_after = matches!(&a, BExpr::Const(_));
            live.push(a);
            if dead_after {
                break; // a non-NULL constant answers; the rest is unreachable
            }
        }
        match live.len() {
            // every argument was a NULL constant
            0 => Ok(BExpr::Const(Value::Null)),
            1 => fold(live.pop().expect("len 1")),
            _ => {
                let mut out = Vec::with_capacity(live.len());
                for a in live {
                    out.push(fold(a)?);
                }
                Ok(BExpr::Coalesce(out))
            }
        }
    }

    /// Fold a CASE's control flow: an arm whose condition is constant FALSE or
    /// NULL is dead and is dropped unfolded; an arm whose condition is constant
    /// TRUE answers, and everything after it (including ELSE) is dead.
    fn fold_case(&mut self, arms: Vec<(BExpr, BExpr)>, else_: BExpr) -> Result<BExpr> {
        let mut live = Vec::with_capacity(arms.len());
        for (c, r) in arms {
            match &c {
                BExpr::Const(Value::Bool(false)) | BExpr::Const(Value::Null) => continue,
                BExpr::Const(Value::Bool(true)) => {
                    // This arm always wins.
                    if live.is_empty() {
                        return fold(r);
                    }
                    live.push((c, r));
                    let (arms, (_, r)) = {
                        let last = live.pop().expect("just pushed");
                        (live, last)
                    };
                    return Ok(BExpr::Case(fold_arms(arms)?, Some(Box::new(fold(r)?))));
                }
                _ => live.push((c, r)),
            }
        }
        if live.is_empty() {
            return fold(else_);
        }
        Ok(BExpr::Case(fold_arms(live)?, Some(Box::new(fold(else_)?))))
    }

    /// Bind a built-in scalar function: resolve the name, check the argument
    /// types against what the function accepts, and give the call a type.
    ///
    /// `nullif` is desugared here rather than implemented: it is exactly
    /// `CASE WHEN a = b THEN NULL ELSE a END`, and re-implementing it would be a
    /// second place for the NULL and equality rules to drift.
    fn bind_func(&mut self, name: &str, args: &[ast::Expr]) -> Result<(BExpr, Ty)> {
        if name == "nullif" {
            if args.len() != 2 {
                return Err(bind_err("nullif() takes exactly 2 arguments"));
            }
            let eq = ast::Expr::Binary(
                ast::BinOp::Eq,
                Box::new(args[0].clone()),
                Box::new(args[1].clone()),
            );
            let case = ast::Expr::Case(
                vec![(eq, ast::Expr::Lit(Value::Null))],
                Some(Box::new(args[0].clone())),
            );
            return self.bind_expr(&case);
        }
        let f = match name {
            "lower" => ScalarFn::Lower,
            "upper" => ScalarFn::Upper,
            "length" => ScalarFn::Length,
            "trim" => ScalarFn::Trim,
            "abs" => ScalarFn::Abs,
            "round" => ScalarFn::Round,
            "substr" | "substring" => ScalarFn::Substr,
            "replace" => ScalarFn::Replace,
            "ltrim" => ScalarFn::Ltrim,
            "rtrim" => ScalarFn::Rtrim,
            "instr" => ScalarFn::Instr,
            other => {
                return Err(bind_err(format!(
                    "unknown function `{other}()`; available: lower, upper, length, trim, \
                     ltrim, rtrim, replace, instr, abs, round, substr, coalesce, ifnull, nullif"
                )))
            }
        };
        let mut bound = Vec::with_capacity(args.len());
        for a in args {
            bound.push(self.bind_expr(a)?);
        }
        let argc = u8::try_from(bound.len())
            .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
        if !f.arity_ok(argc) {
            return Err(bind_err(format!(
                "{name}() cannot take {argc} argument(s)"
            )));
        }
        // Pin each argument's type where the function demands one, so a bare
        // `$1` gets a type and a wrong type is a COMPILE error rather than a
        // per-row surprise.
        let (want, ret): (&[Option<ColumnType>], Ty) = match f {
            ScalarFn::Lower | ScalarFn::Upper | ScalarFn::Trim => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::Length => (&[Some(ColumnType::Text)], Some(ColumnType::Int64)),
            // abs/round keep their argument's numeric type, so they are checked
            // below rather than pinned to one.
            ScalarFn::Abs | ScalarFn::Round => (&[], None),
            ScalarFn::Substr => (
                &[Some(ColumnType::Text), Some(ColumnType::Int64), Some(ColumnType::Int64)],
                Some(ColumnType::Text),
            ),
            ScalarFn::Replace => (
                &[Some(ColumnType::Text), Some(ColumnType::Text), Some(ColumnType::Text)],
                Some(ColumnType::Text),
            ),
            // ltrim/rtrim: text, and an optional text set of trim characters.
            ScalarFn::Ltrim | ScalarFn::Rtrim => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::Instr => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Int64))
            }
        };
        let mut out = Vec::with_capacity(bound.len());
        for (i, (e, t)) in bound.into_iter().enumerate() {
            match want.get(i).copied().flatten() {
                Some(w) => {
                    let (e, t) = self.unify_param(e, t, w);
                    if let Some(t) = t {
                        if t != w {
                            return Err(bind_err(format!(
                                "{name}() argument {} must be {w}, got {t}",
                                i + 1
                            )));
                        }
                    }
                    out.push(e);
                }
                None => out.push(e),
            }
        }
        let ret = match f {
            ScalarFn::Abs | ScalarFn::Round => {
                // Numeric in, same numeric out. The type is the argument's.
                let t = self.static_type(&out[0]);
                match t {
                    Some(ColumnType::Int64) | Some(ColumnType::Float64) | None => t,
                    Some(other) => {
                        return Err(bind_err(format!("{name}() expects a number, got {other}")))
                    }
                }
            }
            _ => ret,
        };
        Ok((BExpr::Call(f, out), ret))
    }

    /// The type of an already-bound expression, where it is knowable without
    /// re-binding. Used for the functions whose return type is their argument's.
    fn static_type(&self, e: &BExpr) -> Ty {
        match e {
            BExpr::Const(v) => v.column_type(),
            // An `excluded.<c>` binds to Col(n + i); fold the index back so a
            // second-half reference reports the column's real type instead of
            // indexing off the end.
            // `excluded.<c>` binds to Col(n + i) over [existing ‖ proposed], so
            // fold the index back into the base row's width.
            BExpr::Col(i) => {
                let n = self.scope.width();
                self.scope
                    .only()
                    .columns
                    .get(*i as usize % n.max(1))
                    .map(|c| c.ty)
            }
            BExpr::Param(i) => self.param_types[*i as usize],
            BExpr::Unary(BUnOp::ToFloat, _) => Some(ColumnType::Float64),
            BExpr::Call(ScalarFn::Length | ScalarFn::Instr, _) => Some(ColumnType::Int64),
            BExpr::Call(ScalarFn::Abs | ScalarFn::Round, a) => self.static_type(&a[0]),
            BExpr::Call(_, _) => Some(ColumnType::Text),
            _ => None,
        }
    }

    /// Unify n operands to one common type: the same rules as
    /// [`Self::unify_operands`] (bare params adopt, Int64 -> Float64 is the only
    /// coercion, anything else cross-type is an error) but applied across the
    /// whole set, so no operand is left behind at a type the others moved off.
    /// Like [`unify_many`], for arms whose unified value IS the result
    /// (CASE / COALESCE): int64/float64 mixing is REFUSED rather than
    /// widened. sqlite types these per ROW — the arm actually taken keeps
    /// its own type, so `COALESCE(30, avg(x)) / 35` divides an INTEGER when
    /// arm 1 wins; widening 30 to 30.0 silently turns that into float
    /// division (measured: 82 wrong answers in the sqllogictest expr tree).
    /// Rigid typing cannot express "the type of the winning arm", so the mix
    /// is a compile error that names the fix. Comparison unification
    /// (`unify_many` for IN lists) keeps the widening: there the widened
    /// value only feeds a comparison, and a comparison's TYPE cannot leak.
    fn unify_result_arms(
        &mut self,
        operands: Vec<(BExpr, Ty)>,
        verb: &str,
    ) -> Result<(Vec<BExpr>, Ty)> {
        let mixes = |a: ColumnType, b: ColumnType| {
            (a == ColumnType::Int64 && b == ColumnType::Float64)
                || (a == ColumnType::Float64 && b == ColumnType::Int64)
        };
        let mut seen: Ty = None;
        for (_, t) in &operands {
            let Some(t) = *t else { continue };
            match seen {
                Some(prev) if mixes(prev, t) => {
                    return Err(bind_err(format!(
                        "cannot {verb}: int64 and float64 — sqlite would type this per row; \
                         add an explicit CAST so every arm is one type"
                    )));
                }
                _ => seen = Some(t),
            }
        }
        self.unify_many(operands, verb)
    }

    fn unify_many(&mut self, operands: Vec<(BExpr, Ty)>, verb: &str) -> Result<(Vec<BExpr>, Ty)> {
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
            return Ok((operands.into_iter().map(|(e, _)| e).collect(), None));
        };
        let mut out = Vec::with_capacity(operands.len());
        for (e, t) in operands {
            let (e, t) = self.unify_param(e, t, target);
            out.push(match t {
                Some(ColumnType::Int64) if target == ColumnType::Float64 => {
                    fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(e)), self.suppress_fold)?
                }
                _ => e,
            });
        }
        Ok((out, Some(target)))
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
/// The type-level projection of `Instr::Cast`'s runtime accept set: true when
/// SOME non-NULL value of `src` casts to `dst`. Text→number is the deliberate
/// strictness line (no prefix-parse); blob/timestamp only cast to themselves.
fn cast_possible(src: ColumnType, dst: ColumnType) -> bool {
    use ColumnType as T;
    if src == dst || src == T::Any || dst == T::Any {
        return true;
    }
    matches!(
        (src, dst),
        (T::Int64, T::Float64 | T::Text | T::Bool)
            | (T::Float64, T::Int64)
            | (T::Bool, T::Int64 | T::Float64 | T::Text)
    )
}

pub(crate) fn fold(e: BExpr) -> Result<BExpr> {
    let foldable = match &e {
        BExpr::Unary(_, a) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Binary(_, a, b) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::Like(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Cast(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        // Never foldable: the list is a session value, not a literal.
        BExpr::InParam(..) => false,
        // A CASE is branching control flow, not a value-in/value-out node; the
        // fold path evaluates whole programs and has no business here.
        BExpr::Case(..) => false,
        BExpr::Coalesce(..) => false,
        // Const-foldable in principle; not worth a special case, and folding
        // would have to reproduce call_scalar's NULL rules here.
        BExpr::Call(..) => false,
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

/// Fold every surviving arm of a CASE.
fn fold_arms(arms: Vec<(BExpr, BExpr)>) -> Result<Vec<(BExpr, BExpr)>> {
    let mut out = Vec::with_capacity(arms.len());
    for (c, r) in arms {
        out.push((fold(c)?, fold(r)?));
    }
    Ok(out)
}

/// Fold, unless we are binding a branch that constant control flow may delete
/// unevaluated. See [`Binder::suppress_fold`].
fn fold_maybe(e: BExpr, suppressed: bool) -> Result<BExpr> {
    if suppressed {
        Ok(e)
    } else {
        fold(e)
    }
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
        BExpr::Cast(a, t) => {
            emit(a, instrs, consts)?;
            instrs.push(Instr::Cast(*t));
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
                BinOp::Concat => Instr::Concat,
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
        BExpr::Case(arms, else_) => {
            // WHEN c JumpIfNotTrue next; THEN r; Jump end; … ELSE e; end:
            // Targets are patched afterwards because they are forward — which
            // is also exactly what the verifier requires.
            let mut jumps_to_end = Vec::new();
            for (c, r) in arms {
                emit(c, instrs, consts)?;
                let jnt = instrs.len();
                instrs.push(Instr::JumpIfNotTrue(0)); // patched below
                emit(r, instrs, consts)?;
                jumps_to_end.push(instrs.len());
                instrs.push(Instr::Jump(0)); // patched below
                let next_arm = instrs.len();
                patch(instrs, jnt, next_arm)?;
            }
            match else_ {
                Some(e) => emit(e, instrs, consts)?,
                None => {
                    let idx = push_const(consts, Value::Null)?;
                    instrs.push(Instr::PushConst(idx));
                }
            }
            let end = instrs.len();
            for j in jumps_to_end {
                patch(instrs, j, end)?;
            }
        }
        BExpr::Call(f, args) => {
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::Call(*f, args.len() as u8));
        }
        BExpr::Coalesce(args) => {
            // Lazily: evaluate an argument, and if it is non-NULL jump to the
            // end WITH IT STILL ON THE STACK — it is the result. Otherwise pop
            // the NULL and try the next. The last argument needs no test: if we
            // reach it, it is the answer whatever it is.
            //
            // This is why JumpIfNotNull peeks instead of popping, and why
            // coalesce is not a Call: an eager coalesce(x, 1/0) would RAISE,
            // where both sqlite and PostgreSQL return x.
            let mut ends = Vec::new();
            let last = args.len() - 1;
            for (i, a) in args.iter().enumerate() {
                emit(a, instrs, consts)?;
                if i == last {
                    break;
                }
                ends.push(instrs.len());
                instrs.push(Instr::JumpIfNotNull(0)); // patched below
                instrs.push(Instr::Pop);
            }
            let end = instrs.len();
            for j in ends {
                patch(instrs, j, end)?;
            }
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

/// Fill in a forward jump target once it is known.
///
/// The index is a u16 in the IR, so a program with more than 65535 instructions
/// cannot express its own jumps. Caught here rather than silently truncating
/// into a target that points somewhere plausible and wrong.
fn patch(instrs: &mut [Instr], at: usize, target: usize) -> Result<()> {
    let t = u16::try_from(target).map_err(|_| {
        Error::Internal("expression is too large to compile (more than 65535 instructions)".into())
    })?;
    match &mut instrs[at] {
        Instr::JumpIfNotTrue(x) | Instr::Jump(x) | Instr::JumpIfNotNull(x) => *x = t,
        other => return Err(Error::Internal(format!("patch target is not a jump: {other:?}"))),
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
            indexed: false,
            default: None,
            check: None,
        };
        TableDef {
            id: 0,
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
            indexes: vec![],
            dead: false,
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


    fn bind_ok(sql: &str) -> (BExpr, Ty) {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        b.bind_expr(&e).unwrap()
    }
    fn bind_err_msg(sql: &str) -> String {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        format!("{}", b.bind_expr(&e).unwrap_err())
    }

    /// The constant-folding / laziness boundary, pinned against MEASURED
    /// PostgreSQL 16 behaviour rather than a guess. Every line here was run
    /// against a live PG first; getting this wrong in either direction is easy:
    ///
    ///   never raise at fold time -> `SELECT 1/0` prepares clean, fails at
    ///     every execute. PG raises at PLAN time (EXPLAIN SELECT 1/0 errors).
    ///   always raise at fold time -> `coalesce(1, 1/0)` dies, though BOTH
    ///     sqlite and PG answer 1.
    ///
    /// The rule is neither: fold the CONTROL FLOW first and drop the
    /// unreachable branch WITHOUT evaluating it; fold what survives, and let
    /// that raise.
    #[test]
    fn folding_drops_dead_branches_before_it_can_raise_on_them() {
        // arg0 is a non-NULL constant -> the whole coalesce IS it, and 1/0 is
        // never folded. PG: 1.
        assert_eq!(bind_ok("coalesce(1, 1/0)").0, BExpr::Const(Value::Int(1)));
        // arg0 is a NULL constant -> dropped; 1/0 becomes reachable -> raises.
        // PG: ERROR division by zero.
        assert!(matches!(
            bind_expr_res("coalesce(NULL, 1/0)"),
            Err(Error::DivisionByZero)
        ));
        // Same rule through CASE. PG: 1, then ERROR.
        assert_eq!(
            bind_ok("CASE WHEN true THEN 1 ELSE 1/0 END").0,
            BExpr::Const(Value::Int(1))
        );
        assert!(matches!(
            bind_expr_res("CASE WHEN false THEN 1 ELSE 1/0 END"),
            Err(Error::DivisionByZero)
        ));
        // A live branch still folds normally.
        assert_eq!(bind_ok("1 + 2").0, BExpr::Const(Value::Int(3)));
    }

    fn bind_expr_res(sql: &str) -> Result<(BExpr, Ty)> {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        b.bind_expr(&e)
    }

    #[test]
    fn coalesce_arguments_must_unify() {
        assert!(bind_err_msg("coalesce(id, 'x')").contains("coalesce"));
        // int/float mixing in RESULT arms is refused, not widened: sqlite
        // types the winning arm per row, so widening 30 to 30.0 changes the
        // arithmetic downstream (measured: 82 wrong answers in the expr
        // corpus). The message names the fix, and the CAST works.
        assert!(bind_err_msg("coalesce(id, 1.5)").contains("CAST"));
        let (_, ty) = bind_ok("coalesce(CAST(id AS REAL), 1.5)");
        assert_eq!(ty, Some(ColumnType::Float64));
    }

    #[test]
    fn function_arity_and_types_are_compile_errors() {
        assert!(bind_err_msg("lower(id)").contains("must be text"));
        assert!(bind_err_msg("length('a', 'b')").contains("argument"));
        assert!(bind_err_msg("abs('x')").contains("number"));
        assert!(bind_err_msg("frobnicate(1)").contains("unknown function"));
    }

    /// abs/round keep their argument's numeric type rather than pinning one.
    #[test]
    fn abs_and_round_return_their_argument_type() {
        assert_eq!(bind_ok("abs(id)").1, Some(ColumnType::Int64));
        assert_eq!(bind_ok("abs(score)").1, Some(ColumnType::Float64));
        assert_eq!(bind_ok("length(name)").1, Some(ColumnType::Int64));
        assert_eq!(bind_ok("lower(name)").1, Some(ColumnType::Text));
    }

    /// nullif is CASE, not a function: reusing the desugaring keeps one set of
    /// NULL/equality rules rather than two.
    #[test]
    fn nullif_desugars_to_case() {
        let (e, _) = bind_ok("nullif(id, 1)");
        assert!(matches!(e, BExpr::Case(..)), "got {e:?}");
    }

    /// [`Scope`] exists so the NEXT step changes one type instead of 45 call
    /// sites. That claim is only worth anything if a two-table scope actually
    /// resolves, so this builds one directly — no SQL surface reaches it yet.
    ///
    /// The rule it pins: a column resolves to an OFFSET INTO THE TUPLE the
    /// expression is evaluated over. One table = the row. `ON CONFLICT DO
    /// UPDATE` = `[existing ‖ proposed]`, which is why `excluded.<c>` is
    /// `Col(n + i)`. A join = the concatenated rows. Same rule, wider tuple.
    #[test]
    fn a_scope_can_already_hold_two_tables() {
        let a = table(); // id, score, name, active, data, created
        let b = TableDef {
            id: 0,
            name: "other".into(),
            columns: vec![ColumnDef {
                name: "tag".into(),
                ty: ColumnType::Text,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
        };
        let sc = Scope {
            names: vec![a.name.clone(), b.name.clone()],
            tables: vec![&a, &b],
        };
        assert_eq!(sc.width(), a.columns.len() + 1);

        // Table b's column sits AFTER a's, at a's width — the concatenation.
        let (slot, ty) = sc.resolve("tag").unwrap();
        assert_eq!((slot as usize, ty), (a.columns.len(), ColumnType::Text));
        // Table a's columns keep their slots, so nothing shifts under them.
        assert_eq!(sc.resolve("id").unwrap().0, 0);
        // Qualifiers reach either side.
        assert_eq!(sc.resolve_qualified("other", "tag").unwrap().0 as usize, a.columns.len());
        assert_eq!(sc.resolve_qualified("t", "id").unwrap().0, 0);
        // A qualifier naming no table in scope is an error, not a silent pick.
        assert!(sc.resolve_qualified("nonsense", "id").is_err());
    }

    /// Ambiguity must be an ERROR. With one table it cannot arise; the day it
    /// can, guessing is a wrong-table read — the exact failure the footprint
    /// discipline exists to prevent.
    #[test]
    fn an_ambiguous_column_is_refused_rather_than_guessed() {
        let a = table();
        let b = TableDef {
            id: 0,
            name: "other".into(),
            columns: vec![ColumnDef {
                name: "id".into(), // collides with a.id
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
        };
        let sc = Scope {
            names: vec![a.name.clone(), b.name.clone()],
            tables: vec![&a, &b],
        };
        let e = sc.resolve("id").unwrap_err();
        assert!(format!("{e}").contains("ambiguous"), "got {e}");
        // ...but qualifying resolves it, to the right side each time.
        assert_eq!(sc.resolve_qualified("t", "id").unwrap().0, 0);
        assert_eq!(sc.resolve_qualified("other", "id").unwrap().0 as usize, a.columns.len());
    }
}
