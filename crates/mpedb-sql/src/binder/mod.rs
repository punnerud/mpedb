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
    exact_float_as_int, Affinity, BareGroupBy, CmpKind, Collation, ColumnDef, ColumnType, Error,
    ExprProgram, Instr, Result, ScalarFn, TableDef, Value,
};

mod cmp;
mod funcs;
mod lower;
mod registry;
mod scope;
#[cfg(test)]
mod tests;

pub use registry::{HostUdfSet, OpSet, SpellFnSet};
pub(crate) use lower::{
    compile_program, declared_collation, fold, peel_collate, peel_order_collate,
    resolve_collation,
};
#[allow(unused_imports)] // children reach these through `use super::*`
use lower::{
    bit_op_name, cast_result_type, cmp_kind, fold_arms, fold_maybe, is_literal_now,
    like_glob_operand, maybe_not,
};
pub(crate) use scope::Scope;

/// Bound (name-resolved, type-checked, constant-folded) expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BExpr {
    Const(Value),
    Param(u16),
    Col(u16),
    Unary(BUnOp, Box<BExpr>),
    Binary(BinOp, Box<BExpr>, Box<BExpr>),
    /// `l IS r` / `l IS NOT r` — NULL-safe (not-)distinct-from, a 2-valued Bool.
    /// The bool is `negated` (`IS NOT`). Its own node rather than a `BinOp`
    /// because it is NOT 3VL: it compiles to a dedicated instruction that never
    /// yields NULL, so folding it through the comparison path would be wrong.
    IsDistinct(Box<BExpr>, Box<BExpr>, bool),
    /// LHS LIKE 'pattern' with a text-LITERAL pattern. The bool is
    /// `case_insensitive`: `true` under the sqlite dialect (ASCII case-folded,
    /// the default), `false` under the PostgreSQL dialect (`bare_group_by =
    /// "postgres"`, case-SENSITIVE). It picks the opcode at compile time —
    /// [`Instr::Like`] vs [`Instr::LikeCs`](mpedb_types::expr) — so the plan is
    /// self-describing and two dialects hash to distinct plans. The last field
    /// is the `ESCAPE` character (`None` = a bare LIKE), which selects the
    /// [`Instr::LikeEsc`]/[`Instr::LikeCsEsc`] opcodes instead — so an escaped
    /// and an unescaped LIKE also hash to distinct plans.
    Like(Box<BExpr>, String, bool, Option<char>),
    /// `LHS LIKE <expr> [ESCAPE c]` — the same matcher as [`BExpr::Like`] with
    /// a pattern that is NOT a literal: a bound parameter (Django's exact wire
    /// shape for every `startswith`/`contains`/`endswith`/`icontains` lookup,
    /// the whole reason this exists — #74 item 3, LIKE half), a column, any
    /// computed value. The bool/escape fields are [`BExpr::Like`]'s, and the
    /// ESCAPE argument itself stays a compile-time literal by deliberate
    /// policy — only the pattern goes dynamic.
    LikeDyn(Box<BExpr>, Box<BExpr>, bool, Option<char>),
    /// LHS GLOB 'pattern' — case-SENSITIVE `*`/`?`/`[...]` (sqlite), with a
    /// text-LITERAL pattern exactly like [`BExpr::Like`]; `NOT GLOB`
    /// is a `Not` wrapped around this by the binder, so this node itself is
    /// never negated.
    Glob(Box<BExpr>, String),
    /// `LHS GLOB <expr>` — [`BExpr::Glob`] with a non-literal pattern; the
    /// GLOB half of the same gap, closed in the same style. Like `Glob` it is
    /// never negated — `NOT GLOB` is a `Not` the binder wraps around it.
    GlobDyn(Box<BExpr>, Box<BExpr>),
    /// LHS REGEXP 'pattern' — sqlite's `ext/misc/regexp.c` dialect. The pattern
    /// is always a literal in Phase 1, exactly like [`BExpr::Glob`]; `NOT REGEXP`
    /// is a `Not` wrapped around this by the binder, so this node is never
    /// negated.
    Regexp(Box<BExpr>, String),
    /// `LHS REGEXP <expr>` — the same matcher as [`BExpr::Regexp`] with a
    /// pattern that is NOT a literal (a bound parameter, a column, any computed
    /// text). Django always BINDS its regex, which is the whole reason this
    /// exists (#74 item 3). Like `Regexp` it is never negated — `NOT REGEXP` is
    /// a `Not` the binder wraps around it.
    RegexpDyn(Box<BExpr>, Box<BExpr>),
    /// `LHS IN (<context list at reserved param n>)` (DESIGN-MULTIDB §2.6).
    InParam(Box<BExpr>, u16),
    /// The collated twin of [`BExpr::InParam`] — see [`Instr::InParamColl`].
    /// Built only for a non-Binary collation, so an ordinary subquery IN keeps
    /// the plain node and identical plan bytes.
    InParamColl(Box<BExpr>, u16, Collation),
    /// `LHS IN (e1, …, en)` — a general value list (task #21).
    InList(Box<BExpr>, Vec<BExpr>),
    /// `CASE WHEN c THEN r … ELSE e END`. `else_` is None for a missing ELSE
    /// (SQL: NULL).
    Case(Vec<(BExpr, BExpr)>, Option<Box<BExpr>>),
    /// A built-in scalar function over already-typed arguments.
    Call(ScalarFn, Vec<BExpr>),
    /// The scalar `max(a,b,…)`/`min(a,b,…)` under a non-BINARY collating
    /// sequence — sqlite's `SQLITE_FUNC_NEEDCOLL` rule: the first argument
    /// (left to right) that DEFINES a collation supplies it, and a bare column
    /// defines its declared one (BINARY counts as defined, so
    /// `max(bincol, nocasecol)` compares BINARY and stays a plain `Call`).
    /// Compiles to [`Instr::CallColl`](mpedb_types::expr).
    CallColl(ScalarFn, Vec<BExpr>, Collation),
    /// `coalesce(a, b, …)` — compiled to control flow, not a call, so later
    /// arguments are never evaluated once an earlier one is non-NULL.
    Coalesce(Vec<BExpr>),
    /// `CAST(x AS t)` — the target name has been folded to an [`Affinity`];
    /// conversion semantics live in [`Instr::Cast`](mpedb_types::expr).
    Cast(Box<BExpr>, Affinity),
    /// A comparison under an explicit collating sequence (task: COLLATE). The
    /// `BinOp` is one of the six comparison operators; TEXT operands compare
    /// under `Collation`. A distinct node from [`BExpr::Binary`] on purpose: the
    /// access-path extractor recognizes only `Binary(Eq, …)` as an index/PK
    /// probe, so a collated comparison is never turned into a bytewise key
    /// lookup — it always stays a residual filter over a full scan, which is
    /// what keeps NOCASE/RTRIM correct without a collated index.
    CollateCmp(BinOp, Box<BExpr>, Box<BExpr>, Collation),
    /// A comparison against a TYPELESS (`any`) column, under sqlite's
    /// **comparison affinity** + storage-class order (task: comparison
    /// affinity). The `Affinity` is the one sqlite's `sqlite3CompareAffinity`
    /// derives for the PAIR; it is applied to BOTH operands (as sqlite's
    /// `OP_Lt`-family does) before they are compared by class. [`Affinity::Blob`]
    /// means "apply nothing", sqlite's NONE.
    ///
    /// Like [`BExpr::CollateCmp`] this is a node of its own so the access-path
    /// extractor cannot mistake it for an index probe — and here that is free,
    /// since an `any` column can never be a key ([`Schema::validate`] refuses
    /// it), which is also why this can only ever be a residual filter.
    ClassCmp(BinOp, Box<BExpr>, Box<BExpr>, Collation, Affinity),
    /// `<probe> COLLATE <coll> IN (e1, …, en)` — the collated form of
    /// [`BExpr::InList`].
    InListColl(Box<BExpr>, Vec<BExpr>, Collation),
    /// A call to a HOST-registered scalar UDF (the C-API `create_function`
    /// path, design/DESIGN-UDF.md). Emitted when a function name matches no
    /// native `ScalarFn`/`AggFn` but DOES match a registered `(name, argc)` in
    /// the binder's [`HostUdfSet`]. Dynamically typed: the result is
    /// [`ColumnType::Any`] and arguments pass through with whatever type they
    /// have. Compiles to [`Instr::HostCall`], which stores the NAME (const pool)
    /// + arity, never the closure.
    HostCall { name: String, args: Vec<BExpr> },
    /// A stored PySpell function call: the definition's content hash rides the
    /// const pool ([`Instr::SpellCall`]); dynamically typed like a host UDF.
    SpellCall { hash: [u8; 32], args: Vec<BExpr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BUnOp {
    Neg,
    Not,
    IsNull,
    IsNotNull,
    ToFloat,
    /// `~x` — bitwise NOT. Same operand rule as the infix bitwise family
    /// ([`Binder::bit_operand`]): int64/bool/any, and the result is int64.
    BitNot,
}

/// Expression type: `None` = NULL literal or not yet constrained.
pub(crate) type Ty = Option<ColumnType>;

/// The symbol of a binary operator, for error messages.
fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "=",
        BinOp::Ne => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Concat => "||",
        BinOp::JsonArrow => "->",
        BinOp::JsonArrowText => "->>",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

/// `json_set`/`json_insert`/`json_replace` are `(X, PATH, VALUE, …)`: argument
/// 0 is the document and the VALUEs are at the even positions from 2 on.
fn json_edit_value_at(i: usize) -> Option<usize> {
    if i >= 2 && i.is_multiple_of(2) {
        Some(i / 2 - 1)
    } else {
        None
    }
}

/// Which argument positions of a JSON function are VALUE positions — the ones
/// whose reading depends on sqlite's per-value JSON subtype. `None` for a
/// function that has none (every reader, `json_patch`, `json_remove`).
pub(crate) fn json_value_positions(name: &str) -> Option<fn(usize) -> Option<usize>> {
    Some(match name {
        "json_quote" => |i| if i == 0 { Some(0) } else { None },
        "json_array" => Some,
        "json_object" => |i| if i.is_multiple_of(2) { None } else { Some(i / 2) },
        "json_set" | "json_insert" | "json_replace" => json_edit_value_at,
        _ => return None,
    })
}

/// Refuse a scalar subquery in a JSON VALUE position.
///
/// sqlite PROPAGATES its JSON subtype out of a scalar subquery
/// (`json_quote((SELECT json('[1]')))` is `[1]`, not `"[1]"`) but not out of a
/// FROM-subquery column, an aggregate, or `||`. mpedb cannot see through the
/// subplan boundary to tell those apart, so the shape is refused rather than
/// answered — and it has to be refused HERE, in the subquery lifter, because by
/// the time the binder runs the lift has already replaced the subquery with a
/// reserved parameter that is indistinguishable from a user one.
pub(crate) fn reject_subquery_in_json_value(name: &str, args: &[ast::Expr]) -> Result<()> {
    let lower = name.to_ascii_lowercase();
    let Some(value_at) = json_value_positions(&lower) else {
        return Ok(());
    };
    for (i, a) in args.iter().enumerate() {
        if value_at(i).is_some() && reaches_subquery(a) {
            return Err(bind_err(format!(
                "{lower}(): mpedb cannot tell whether this argument is JSON text or a plain \
                 string, because it is a scalar subquery, and sqlite decides it from a \
                 per-value JSON subtype that mpedb's values do not carry — one that sqlite \
                 propagates out of a scalar subquery but not out of a FROM-subquery column or \
                 an aggregate. Wrap the argument in `json(…)` to splice it as JSON, or in \
                 `'' || …` to force the quoted-string reading"
            )));
        }
    }
    Ok(())
}

/// Does the subtype of `e` come from a scalar subquery? Follows exactly the
/// shapes `Binder::json_ness` follows — a subquery buried under `||` or a CAST
/// carries no subtype in sqlite either, so it is not reachable.
fn reaches_subquery(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::Subquery(_) => true,
        ast::Expr::Case(arms, else_) => {
            arms.iter().any(|(_, r)| reaches_subquery(r))
                || else_.as_deref().is_some_and(reaches_subquery)
        }
        ast::Expr::Coalesce(items) => items.iter().any(reaches_subquery),
        _ => false,
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
    /// unevaluated? Then do not fold it, because folding may RAISE.
    ///
    /// Division by zero is NOT such a raise: like sqlite, mpedb folds `1/0`
    /// to NULL. Arithmetic overflow still is — mpedb raises it where sqlite
    /// wraps — and folding it in a dead branch would be just as wrong.
    /// Measured against live PG 16 with an overflowing constant `V` (e.g.
    /// `9223372036854775807 + 1`):
    ///   EXPLAIN SELECT V                         -> ERROR at PLAN time
    ///   SELECT coalesce(1, V)                    -> 1
    ///   SELECT coalesce(NULL, V)                 -> ERROR
    ///   SELECT CASE WHEN true  THEN 1 ELSE V END -> 1
    ///   SELECT CASE WHEN false THEN 1 ELSE V END -> ERROR
    ///
    /// So folding is not "never raise" (that would let `SELECT V` prepare
    /// cleanly and fail at every execute) and not "always raise" (that kills
    /// `coalesce(1, V)`). It is: fold the CONTROL FLOW first, drop the branch
    /// that cannot be taken WITHOUT evaluating it, then fold whatever
    /// survives — and let that raise.
    suppress_fold: bool,
    /// The tables this statement may name. See [`Scope`].
    pub scope: Scope<'a>,
    /// Types of ALL parameters: the `n_user_params` caller params first, then
    /// one appended reserved slot per distinct `current_setting()` key (in
    /// `ctx_keys` order). `current_setting()` refs bind to `Param(n_user + pos)`
    /// and are filled from the session at execute time (design/DESIGN-MULTIDB.md §2).
    pub param_types: Vec<Ty>,
    /// The DECLARED collation behind each parameter slot, where the slot stands
    /// in for an outer COLUMN — a correlation argument.
    ///
    /// Without it a comparison across a subquery boundary silently fell to
    /// BINARY: `col_coll` reads `BExpr::Col`, and a correlated reference is
    /// rewritten to a PARAM before the inner binder ever sees it, so the outer
    /// column's `COLLATE NOCASE` vanished. Measured wrong answers, not missing
    /// features — with `nc.a` declared NOCASE holding 'AB' and `nq.x` holding
    /// 'ab', both `a IN (SELECT x FROM nq)` and
    /// `EXISTS (SELECT 1 FROM nq WHERE nc.a = nq.x)` returned NO rows where
    /// sqlite returns one.
    ///
    /// Indexed by parameter slot; a slot that is an ordinary user parameter has
    /// no entry, because a bound value carries no declared collation.
    pub param_colls: Vec<Option<Collation>>,
    /// RUNG 4, and deliberately NOT `param_colls`: the collation of a lifted
    /// SUBQUERY's output column, indexed by its result slot.
    ///
    /// Kept apart because the two rungs are read in different places and
    /// merging them would be a wrong answer: `param_colls` is rung 2 (the
    /// outer COLUMN a correlation param stands for) and the general comparison
    /// arm reads it, while rung 4 applies ONLY to `IN (SELECT …)`. Measured:
    /// `'ab' = (SELECT a FROM nc)` does NOT take the subquery's collation in
    /// sqlite, in either operand order — folding rung 4 into `param_colls`
    /// would have made mpedb answer where it is correct today.
    pub slot_colls: Vec<Option<Collation>>,
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
    /// The statement instant (`'now'`) alone, without the rest of the session
    /// context. A DEFAULT is EVALUATED per INSERT rather than stored as a
    /// standing predicate, so "when the row was inserted" is exactly what it
    /// means — unlike a CHECK or an index expression, where a time-dependent
    /// answer would silently change under something already written.
    allow_instant: bool,
    /// The compat dialect (COMPAT.md). Reused as the LIKE-strictness signal
    /// exactly as it is the GROUP BY strictness signal (#87): [`BareGroupBy::Sqlite`]
    /// (default) compiles case-INsensitive LIKE that coerces a numeric operand to
    /// text; [`BareGroupBy::Postgres`] compiles case-SENSITIVE LIKE
    /// ([`Instr::LikeCs`]) and refuses a numeric operand. Set by the planner from
    /// the database's configured dialect (`set_dialect`); defaults to Sqlite so
    /// CHECK/policy binders and tests keep the sqlite behavior.
    bare_group_by: BareGroupBy,
    /// Host-registered scalar UDFs in scope (design/DESIGN-UDF.md). Set by the
    /// planner from the database's per-connection registry (`set_host_udfs`);
    /// empty for CHECK/policy binders and tests, so their function resolution is
    /// unchanged. Survives `rescope` like `bare_group_by` — a UDF call can appear
    /// at any nesting depth or over the grouped tuple.
    host_udfs: HostUdfSet,
}

fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

impl<'a> Binder<'a> {
    /// A binder for a `DEFAULT ( <expr> )` body: no parameters and no session
    /// context, but the STATEMENT INSTANT is allowed — a default is evaluated
    /// per INSERT, so `'now'` there means "when this row was inserted", which
    /// is the one time-dependent answer that does not change under anything
    /// already stored.
    pub fn new_default_expr(table: &'a TableDef) -> Binder<'a> {
        let mut b = Binder::with_scope(Scope::single(table), 0, false);
        b.allow_instant = true;
        b
    }

    /// Did the bound expression read the statement instant? A default that does
    /// not is a CONSTANT and the caller folds it to one.
    pub fn uses_statement_instant(&self) -> bool {
        self.ctx_keys.iter().any(|k| k == crate::STATEMENT_INSTANT_KEY)
    }

    pub fn new(table: &'a TableDef, n_params: u16, allow_params: bool) -> Binder<'a> {
        Binder::with_scope(Scope::single(table), n_params, allow_params)
    }

    pub fn with_scope(scope: Scope<'a>, n_params: u16, allow_params: bool) -> Binder<'a> {
        Binder {
            allow_excluded: false,
            suppress_fold: false,
            scope,
            param_types: vec![None; n_params as usize],
            param_colls: vec![None; n_params as usize],
            slot_colls: Vec::new(),
            n_user_params: n_params,
            ctx_keys: Vec::new(),
            ctx_list_keys: std::collections::BTreeSet::new(),
            allow_params,
            // `current_setting()` is allowed wherever caller params are (queries
            // and, later, policy predicates); disallowed in CHECK constraints.
            allow_context: allow_params,
            allow_instant: allow_params,
            bare_group_by: BareGroupBy::default(),
            host_udfs: HostUdfSet::default(),
        }
    }

    /// Select the compat dialect (COMPAT.md) that governs LIKE strictness. The
    /// planner calls this right after constructing a root binder so the database's
    /// configured [`BareGroupBy`] reaches the LIKE binding site; `rescope`d
    /// binders inherit it. Mirrors [`set_allow_excluded`](Self::set_allow_excluded).
    pub fn set_dialect(&mut self, mode: BareGroupBy) {
        self.bare_group_by = mode;
    }

    /// Install the HOST-registered scalar UDFs in scope for this binder
    /// (design/DESIGN-UDF.md). The planner calls this right after `set_dialect`
    /// on every root binder it constructs, so a UDF call resolves in queries,
    /// join operands, aggregate arguments, and (via `rescope`) the grouped tuple.
    /// Cheap: the set is a small `(name, arity)` vector cloned once per compile.
    /// sqlite's collation rung 2: the DECLARED collation behind an operand,
    /// or `None` for one that has none (a literal, an expression result).
    ///
    /// A correlation PARAM answers for the outer column it stands in for —
    /// reading only `Col` made every comparison across a subquery boundary
    /// fall to BINARY. Shared by the comparison arm and the two `IN` arms;
    /// three copies of this would drift, and the `IN`-subquery arm not having
    /// it at all is exactly how that form shipped a wrong answer.
    fn operand_collation(&self, e: &BExpr) -> Option<Collation> {
        match e {
            BExpr::Col(idx) => Some(self.scope.column_collation(*idx)),
            BExpr::Param(i) => self.param_colls.get(*i as usize).copied().flatten(),
            _ => None,
        }
    }

    pub fn set_host_udfs(&mut self, set: &HostUdfSet) {
        self.host_udfs = set.clone();
    }


    /// The HOST collating-sequence names in scope, for an ORDER BY key's
    /// `COLLATE`. Empty for a connection that registered none.
    pub(crate) fn host_colls(&self) -> &[String] {
        self.host_udfs.colls()
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
            param_colls: self.param_colls,
            slot_colls: self.slot_colls,
            n_user_params: self.n_user_params,
            ctx_keys: self.ctx_keys,
            ctx_list_keys: self.ctx_list_keys,
            allow_params: self.allow_params,
            allow_context: self.allow_context,
            allow_instant: self.allow_instant,
            // The compat dialect is a database-wide fact, so it survives a scope
            // change (a join's per-table rescopes must keep the same LIKE rules).
            bare_group_by: self.bare_group_by,
            // Host UDFs are a per-connection fact and likewise survive a rescope
            // (a UDF over the grouped tuple, or in a join operand, must resolve).
            host_udfs: self.host_udfs,
            // Neither survives a scope change: `excluded.` belongs to ON
            // CONFLICT, and fold suppression to whichever branch set it.
            allow_excluded: false,
            suppress_fold: false,
        }
    }

    /// Whether the sqlite compat dialect is in force. Gates every "accept what
    /// sqlite accepts" widening (truthiness, the bool/int bridge); the
    /// PostgreSQL dialect keeps mpedb's original rigid refusals.
    pub(crate) fn sqlite_dialect(&self) -> bool {
        self.bare_group_by == BareGroupBy::Sqlite
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

    /// The reserved slot carrying the STATEMENT-START instant, as an ISO-8601
    /// UTC time string — what a literal `'now'` in a date/time function binds
    /// to (design note: `mpedb_types::expr::datetime`, module header).
    ///
    /// It rides the session-context reserved-slot machinery verbatim, under a
    /// key no `current_setting()` may spell ([`crate::STATEMENT_INSTANT_KEY`],
    /// refused by name in both context-binding arms). That buys the whole
    /// mechanism for free — one slot per statement (so every `'now'` in one
    /// statement agrees), sized into `n_params` by the existing accounting,
    /// filled once per `execute()` by `resolve_params`, and encoded in the plan
    /// as nothing more than a key name.
    ///
    /// Where session context is not allowed — a CHECK body, an index
    /// expression, a DEFAULT — neither is `'now'`, and for the same reason: the
    /// expression is stored as SOURCE and re-evaluated later, so an answer that
    /// depends on WHEN it ran would silently change under it.
    fn statement_instant(&mut self) -> Result<BExpr> {
        if !self.allow_context && !self.allow_instant {
            return Err(bind_err(
                "'now' is not allowed in this expression: it binds the statement instant, \
                 and this expression is stored and re-evaluated later (a CHECK, a DEFAULT \
                 or an index expression), where a time-dependent answer would silently \
                 change under it",
            ));
        }
        let key = crate::STATEMENT_INSTANT_KEY;
        let pos = match self.ctx_keys.iter().position(|k| k == key) {
            Some(p) => p,
            None => {
                let idx = self.n_user_params as usize + self.ctx_keys.len();
                if idx >= u16::MAX as usize {
                    return Err(bind_err("too many parameters (including reserved slots)"));
                }
                self.ctx_keys.push(key.to_string());
                // Pinned TEXT: the slot always carries an ISO-8601 time string,
                // so the planner's "every reserved slot must be type-inferable"
                // guard is satisfied without any special case.
                self.param_types.push(Some(ColumnType::Text));
                self.ctx_keys.len() - 1
            }
        };
        Ok(BExpr::Param(self.n_user_params + pos as u16))
    }

    /// Bind a WHERE predicate: must type to bool (or NULL). A non-boolean is
    /// truthy-tested the way sqlite does — see [`Self::coerce_bool_ctx`].
    pub fn bind_predicate(&mut self, e: &ast::Expr) -> Result<BExpr> {
        let (b, ty) = self.bind_expr(e)?;
        let (b, ty) = self.unify_param(b, ty, ColumnType::Bool);
        let (b, ty) = self.coerce_bool_ctx(b, ty)?;
        match ty {
            None | Some(ColumnType::Bool) => Ok(b),
            Some(t) => Err(bind_err(format!(
                "predicate must be a boolean expression, got {t}"
            ))),
        }
    }

    /// Bind a CHECK expression: must type to bool, strictly (an untyped NULL is
    /// still refused here — a CHECK that can never be TRUE is a schema bug).
    /// A non-boolean is truthy-tested like sqlite ([`Self::coerce_bool_ctx`]);
    /// CHECK bodies are stored as SOURCE in the schema and recompiled at
    /// attach, so widening what compiles moves no canonical bytes.
    pub fn bind_check(&mut self, e: &ast::Expr) -> Result<BExpr> {
        let (b, ty) = self.bind_expr(e)?;
        let (b, ty) = self.coerce_bool_ctx(b, ty)?;
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
        // A bare parameter in a CASE's RESULT position lands in this column
        // and nowhere else, so it takes the column's type — the same inference
        // `SET c = ?` already makes one level up. Without it a `SET c = CASE
        // WHEN … THEN ? … END` (Django's bulk_update shape) leaves every arm
        // untyped, and a caller that relies on the declared type to convert
        // (the C-API shim turns an `int` 0/1 into a `bool` when the plan says
        // the column is one) sends the wrong storage class. Conditions are NOT
        // touched — they are booleans about other columns, not values of this
        // one — and an already-typed slot is left alone, so a parameter used in
        // two places keeps its first meaning and the conflict is still caught.
        if let ast::Expr::Case(arms, else_) = e {
            let results = arms.iter().map(|(_, r)| r).chain(else_.iter().map(|b| b.as_ref()));
            for r in results {
                if let ast::Expr::Param(i) = r {
                    if self.param_types[*i as usize].is_none() {
                        self.pin_param(*i, Some(col.ty));
                    }
                }
            }
        }
        let (b, ty) = self.bind_expr(e)?;
        // A column that CONVERTS on store (task #113: a rigid `int`/`real`/
        // `text` whose declaration came through `CREATE TABLE`, so it carries
        // sqlite's affinity) must NOT pin a bare parameter to its rigid type:
        // the whole point is that `SET name = ?` with an integer bound stores
        // `'5'`. Leaving the slot untyped is what lets the value reach the
        // store-time conversion; the engine then validates the CONVERTED value
        // against the column, so nothing untyped is stored unchecked.
        let (b, ty) = if col.converts_on_store() {
            (b, ty)
        } else {
            self.unify_param(b, ty, col.ty)
        };
        match ty {
            Some(t) if t == col.ty => Ok(b),
            // `any` is the loose-type escape (#23): every runtime-typed value
            // belongs, so a statically-typed assignment is never a type error.
            Some(_) if col.ty == ColumnType::Any => Ok(b),
            // The mirror image: a DYNAMICALLY-typed right-hand side (`any` — a
            // host UDF's result, design/DESIGN-UDF.md, or a typeless column) has
            // no static type to compare, exactly as in `unify_operands`, which
            // already lets `any` meet every concrete type and settles it at
            // runtime. Assignment settles it at runtime too, and settles it
            // EXACTLY: the engine validates every written value against its
            // column (`validate_row_in` — `fits`), so `SET n = my_udf(x)` with a
            // text result is a clean `TypeMismatch` on the row, never a wrong
            // value in an int64 column. Refusing at compile time instead would
            // reject `UPDATE … SET col = <udf>(…)` outright, which is the write
            // half of the UDF surface Django uses.
            Some(ColumnType::Any) => Ok(b),
            Some(ColumnType::Int64) if col.ty == ColumnType::Float64 => {
                fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(b)), self.suppress_fold)
            }
            // The other direction, and CONSTANTS ONLY (task #74). sqlite's
            // INTEGER affinity converts a real to an integer exactly when the
            // round trip is lossless, so `SET i = 9.0` stores the integer 9 —
            // which is Django's shape whenever a Python float reaches an
            // IntegerField. A constant is the only case where mpedb can VERIFY
            // losslessness at compile time, so it is the only case allowed:
            // `SET i = r` stays refused, because truncating a column of reals
            // would be a wrong answer rather than a wider one, and sqlite would
            // have stored the real itself in its typeless column.
            // A DDL-declared `int` column is NOT handled here: it carries
            // sqlite's INTEGER affinity, which is stricter at the i64 extremes
            // than `exact_float_as_int` (`sqlite3VdbeIntegerAffinity` refuses
            // exactly ±2^63, where this accepts the clamp), and applies it per
            // value at STORE time — so it falls through to the converting arm
            // below and `SET i = POWER(i, ?)` is allowed. This arm is the
            // config-declared `type = "int64"`, where rigidity is the contract
            // and there is no affinity to apply.
            Some(ColumnType::Float64)
                if col.ty == ColumnType::Int64 && !col.converts_on_store() =>
            {
                match &b {
                    BExpr::Const(Value::Float(f)) => match exact_float_as_int(*f) {
                        Some(i) => Ok(BExpr::Const(Value::Int(i))),
                        None => Err(bind_err(format!(
                            "cannot assign the float64 constant {f:e} to int64 column `{}` — \
                             it is not exactly an integer in the int64 range, and mpedb's \
                             rigid int64 cannot hold what sqlite would have stored",
                            col.name
                        ))),
                    },
                    _ => Err(bind_err(format!(
                        "cannot assign float64 to column `{}` of type int64: only a \
                         constant whose value is exactly an integer converts, because \
                         that is the only case losslessness can be checked at compile time",
                        col.name
                    ))),
                }
            }
            // sqlite stores a boolean AS the integer 0/1, so assigning one to an
            // integer column is exactly `CAST(x AS INTEGER)` — lossless and
            // sqlite-identical. This is Django's `SET flag = (a = b)` shape.
            Some(ColumnType::Bool)
                if col.ty == ColumnType::Int64 && self.bare_group_by == BareGroupBy::Sqlite =>
            {
                fold_maybe(BExpr::Cast(Box::new(b), Affinity::Integer), self.suppress_fold)
            }
            // The other direction is NOT symmetric, deliberately. `SET flag = 1`
            // / `= 0` folds into the bool domain and is exact. Any other integer
            // is REFUSED: sqlite would store `2` in its `bool` column and read
            // `2` back, which mpedb's rigid `Bool` cannot represent — truthy-
            // testing it to TRUE would be a wrong answer on read-back. A clean
            // refusal is the honest outcome, and Django only ever sends 0/1.
            Some(ColumnType::Int64)
                if col.ty == ColumnType::Bool && self.bare_group_by == BareGroupBy::Sqlite =>
            {
                match &b {
                    BExpr::Const(Value::Int(i @ (0 | 1))) => Ok(BExpr::Const(Value::Bool(*i == 1))),
                    _ => Err(bind_err(format!(
                        "cannot assign int64 to bool column `{}` — only the literals 0 and 1 \
                         convert; mpedb's bool holds no other integer",
                        col.name
                    ))),
                }
            }
            // Everything else on a column that CONVERTS on store: sqlite's
            // affinity runs on the way in and decides per value, so the static
            // types disagreeing is not the answer — `SET name = 5` on a
            // `name varchar(10)` stores `'5'`. A constant converts here and
            // now (so a value the conversion cannot land inside the rigid type
            // is still a clean BIND error, with the reason named); anything
            // else converts at store time and the engine then validates the
            // CONVERTED value against the column — a refusal, never a wrong
            // value.
            Some(t) if col.converts_on_store() => match b {
                BExpr::Const(v) => {
                    let v = col.store(v);
                    if v.fits(col.ty) {
                        Ok(BExpr::Const(v))
                    } else {
                        Err(bind_err(format!(
                            "cannot assign {t} to column `{}` of type {}: sqlite's {} \
                             affinity leaves this value a {}, which the column cannot \
                             hold — sqlite would have stored it as one",
                            col.name,
                            col.ty,
                            col.affinity.name(),
                            v.type_name()
                        )))
                    }
                }
                other => Ok(other),
            },
            Some(t) => Err(bind_err(format!(
                "cannot assign {t} to column `{}` of type {}",
                col.name, col.ty
            ))),
            None => {
                if let BExpr::Const(v) = &b {
                    if v.is_null() && !col.nullable {
                        // `NotNullViolation`, not a bind error, even though it
                        // is caught at BIND: the CLASS is what a consumer
                        // branches on. sqlite raises this at run time and the
                        // DBAPI maps it to `IntegrityError`; a bind error maps
                        // to `OperationalError`, so Django's
                        // `assertRaises(IntegrityError)` saw no error at all.
                        // Catching it earlier than sqlite is a feature; calling
                        // it something else is not.
                        return Err(Error::NotNullViolation {
                            table: self.scope.sole_table_name(),
                            column: col.name.clone(),
                        });
                    }
                }
                Ok(b)
            }
        }
    }

    /// Bind an expression bottom-up; returns the folded expression + type.
    pub fn bind_expr(&mut self, e: &ast::Expr) -> Result<(BExpr, Ty)> {
        match e {
            // RAISE never binds: inside a trigger body the trigger compiler
            // intercepts it before binding, so reaching here means any other
            // position — sqlite's own containment message.
            ast::Expr::Raise(..) => {
                Err(bind_err("RAISE() may only be used within a trigger-program"))
            }
            ast::Expr::Lit(v) => Ok((BExpr::Const(v.clone()), v.column_type())),
            ast::Expr::Param(i) => {
                if !self.allow_params {
                    return Err(bind_err("parameters are not allowed in this expression"));
                }
                // The parser sizes `n_params` to the max index it SAW, so a
                // statement it produced whole is always in range. An AST
                // assembled from TWO parses is not the parser's to guarantee —
                // a CTE body is captured as source and re-parsed, and its
                // parameters are numbered by that second parse. Splicing one in
                // with the count left at the outer statement's value indexed
                // past the end and PANICKED. A refusal upstream is what keeps
                // this true today; the check is here so that violating it is an
                // error rather than a crash.
                let ty = self.param_types.get(*i as usize).copied().ok_or_else(|| {
                    bind_err(format!(
                        "parameter ${} is out of range for a statement that declares {}",
                        *i as usize + 1,
                        self.param_types.len()
                    ))
                })?;
                Ok((BExpr::Param(*i), ty))
            }
            ast::Expr::Col(name) => {
                let (idx, ty) = self.scope.resolve(name)?;
                Ok((BExpr::Col(idx), Some(ty)))
            }
            ast::Expr::Unary(UnOp::Neg, a) => {
                let (a, at) = self.bind_expr(a)?;
                match at {
                    // `any` (a mixed CASE/COALESCE arm, host UDF result,
                    // typeless column) negates per VALUE: the runtime `Neg`
                    // already handles both numeric classes and refuses the
                    // rest cleanly, and the result stays `any`.
                    None
                    | Some(ColumnType::Int64)
                    | Some(ColumnType::Float64)
                    | Some(ColumnType::Any) => {}
                    Some(t) => return Err(bind_err(format!("cannot negate {t}"))),
                }
                let e = fold_maybe(BExpr::Unary(BUnOp::Neg, Box::new(a)), self.suppress_fold)?;
                Ok((e, at))
            }
            ast::Expr::Unary(UnOp::BitNot, a) => {
                let (a, at) = self.bind_expr(a)?;
                let (a, at) = self.unify_param(a, at, ColumnType::Int64);
                let a = self.bit_operand(a, at, "~")?;
                let e = fold_maybe(BExpr::Unary(BUnOp::BitNot, Box::new(a)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Int64)))
            }
            ast::Expr::Unary(UnOp::Not, a) => {
                let (a, at) = self.bind_expr(a)?;
                let (a, at) = self.unify_param(a, at, ColumnType::Bool);
                let (a, at) = self.coerce_bool_ctx(a, at)?;
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
            ast::Expr::IsDistinct(l, r, negated) => {
                let (l, lt) = self.bind_expr(l)?;
                let (r, rt) = self.bind_expr(r)?;
                // Both operands unify exactly like `=` — same type, the single
                // Int64->Float64 coercion. The difference is only in the RESULT,
                // which is 2-valued: `IS` never yields NULL, so it is its own
                // node with its own instruction rather than a 3VL comparison.
                let (l, lt, r, rt) = self.bridge_bool_int(l, lt, r, rt)?;
                let (l, r, _) = self.unify_operands(l, lt, r, rt, "compare")?;
                let e = fold_maybe(
                    BExpr::IsDistinct(Box::new(l), Box::new(r), *negated),
                    self.suppress_fold,
                )?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::Like(lhs, pat, escape) => {
                let (l, lt) = self.bind_expr(lhs)?;
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                // sqlite dialect: case-INsensitive, and a non-text operand
                // coerces to text. PostgreSQL dialect: case-SENSITIVE, and a
                // non-text operand is refused (rigid) — both keyed off the
                // same signal, for the pattern exactly as for the subject.
                let ci = self.bare_group_by == BareGroupBy::Sqlite;
                let l = like_glob_operand(l, lt, "LIKE", ci)?;
                let e = match pat.as_ref() {
                    // A text LITERAL keeps the const-pool form — every LIKE
                    // mpedb could compile before #74; its plan bytes are
                    // unchanged. Anything else (a bound parameter — Django
                    // always binds the pattern, with `ESCAPE '\'`, which is
                    // this whole task — a column, any computed value) takes
                    // the STACK form. The old restriction was structural,
                    // exactly as it was for REGEXP, and NOT a compiled-pattern
                    // cache: like_impl was recompiling per row even for a
                    // literal, and now memoizes for both forms.
                    ast::Expr::Lit(Value::Text(p)) => fold_maybe(
                        BExpr::Like(Box::new(l), p.clone(), ci, *escape),
                        self.suppress_fold,
                    )?,
                    other => {
                        let (p, pt) = self.bind_expr(other)?;
                        let (p, pt) = self.unify_param(p, pt, ColumnType::Text);
                        // The same text bridge as the subject's, then a fold:
                        // a constant that lands on text — `s LIKE 12` casts
                        // and folds to `'12'` — rejoins the LITERAL opcode
                        // and its plan bytes. A constant NULL stays dynamic
                        // (`BExpr::LikeDyn` is left out of `fold`'s foldable
                        // set for RegexpDyn's reason; the opcode's NULL rule
                        // answers it per row).
                        let p = fold_maybe(
                            like_glob_operand(p, pt, "LIKE pattern", ci)?,
                            self.suppress_fold,
                        )?;
                        match p {
                            BExpr::Const(Value::Text(s)) => fold_maybe(
                                BExpr::Like(Box::new(l), s, ci, *escape),
                                self.suppress_fold,
                            )?,
                            p => BExpr::LikeDyn(Box::new(l), Box::new(p), ci, *escape),
                        }
                    }
                };
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::Match(_, _) => {
                // MATCH is NOT a boolean expression (design/DESIGN-FTS.md §3):
                // it is usable ONLY as a top-level WHERE conjunct against an FTS
                // table, where the planner intercepts it into an `FtsScan`
                // BEFORE binding. Any MATCH reaching the binder — a scalar
                // context, a non-FTS column/table, a SELECT-list item, inside an
                // OR, or a second MATCH conjunct — is illegal, and mpedb raises
                // the identical sqlite error rather than inventing a fallback.
                Err(Error::Bind(
                    "unable to use function MATCH in the requested context".into(),
                ))
            }
            ast::Expr::Glob(lhs, pat, negated) => {
                // Same shape as LIKE, dyn pattern included. `NOT GLOB` is a
                // real `Not` over the 3VL result (via `maybe_not`) — NOT of
                // NULL is NULL, so a NULL operand still yields NULL as SQL
                // requires.
                let (l, lt) = self.bind_expr(lhs)?;
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                // GLOB is always case-SENSITIVE in both dialects; only the
                // coercion follows the dialect (coerce under sqlite, refuse
                // under PG), for the pattern exactly as for the subject.
                let coerce = self.bare_group_by == BareGroupBy::Sqlite;
                let l = like_glob_operand(l, lt, "GLOB", coerce)?;
                let g = match pat.as_ref() {
                    ast::Expr::Lit(Value::Text(p)) => {
                        fold_maybe(BExpr::Glob(Box::new(l), p.clone()), self.suppress_fold)?
                    }
                    other => {
                        let (p, pt) = self.bind_expr(other)?;
                        let (p, pt) = self.unify_param(p, pt, ColumnType::Text);
                        // Text bridge + fold, exactly as in the LIKE arm: a
                        // constant pattern rejoins the literal opcode.
                        let p = fold_maybe(
                            like_glob_operand(p, pt, "GLOB pattern", coerce)?,
                            self.suppress_fold,
                        )?;
                        match p {
                            BExpr::Const(Value::Text(s)) => {
                                fold_maybe(BExpr::Glob(Box::new(l), s), self.suppress_fold)?
                            }
                            p => BExpr::GlobDyn(Box::new(l), Box::new(p)),
                        }
                    }
                };
                let e = fold_maybe(maybe_not(g, *negated), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::Regexp(lhs, pat, negated) => {
                // In real sqlite the operator has NO built-in meaning: `x
                // REGEXP y` desugars to `regexp(y, x)` — PATTERN FIRST — and
                // errors unless the consumer registered that 2-argument
                // function (CPython/Django always register one with Python
                // `re` semantics). So when THIS connection has a host
                // `regexp/2`, the operator IS that call and the host dialect
                // must win over mpedb's native matcher below: the two dialects
                // diverge on patterns valid in both, and `(?i)…`/backreference
                // patterns (every Django `__iregex`) exist only in the host's
                // (wrong answer W3). With no registration the native NFA stays
                // — a documented mpedb EXTENSION (COMPAT.md); plain sqlite
                // would error `no such function: regexp`.
                if self.host_udfs.resolves("regexp", 2) {
                    // No type pinning on either operand: a host UDF receives
                    // whatever the expressions yield, exactly as the explicit
                    // `regexp(y, x)` call binds (the generic HostCall arm).
                    let (l, _) = self.bind_expr(lhs)?;
                    let (p, _) = self.bind_expr(pat)?;
                    let call = BExpr::HostCall {
                        name: "regexp".to_string(),
                        args: vec![p, l],
                    };
                    // Un-negated, the raw UDF result flows out (`Any`) — a
                    // boolean position truthy-tests it via `coerce_bool_ctx`,
                    // exactly how sqlite treats a UDF standing in a WHERE.
                    // `NOT REGEXP` is NOT over that truthiness: `Instr::Not`
                    // truthy-tests its operand (`truthy3` =
                    // sqlite3VdbeBooleanValue) with 3VL NULL propagation, so
                    // the negated form types Bool. Never folded: a host call
                    // has no compile-time value.
                    return Ok(if *negated {
                        (maybe_not(call, true), Some(ColumnType::Bool))
                    } else {
                        (call, Some(ColumnType::Any))
                    });
                }
                // Both operands are text and the result is Bool. `NOT REGEXP`
                // is a real `Not` over the 3VL result (via `maybe_not`) — NOT of
                // NULL is NULL, so a NULL operand still yields NULL as SQL
                // requires.
                //
                // A text LITERAL keeps the const-pool form (`BExpr::Regexp`),
                // which is every REGEXP mpedb could compile before #74 — its
                // plan bytes are unchanged. Anything else (a bound parameter,
                // a column, a computed text) takes the STACK form. Django
                // always binds its pattern, which is what item 3 is; the old
                // restriction was structural, inherited from LIKE/GLOB, and NOT
                // a compiled-regex cache — `regexp_match` was recompiling per
                // row even for a literal, and now memoizes for both forms.
                let (l, lt) = self.bind_expr(lhs)?;
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                match lt {
                    None | Some(ColumnType::Text) => {}
                    Some(t) => return Err(bind_err(format!("REGEXP requires text, got {t}"))),
                }
                let r = match pat.as_ref() {
                    ast::Expr::Lit(Value::Text(p)) => {
                        fold_maybe(BExpr::Regexp(Box::new(l), p.clone()), self.suppress_fold)?
                    }
                    other => {
                        let (p, pt) = self.bind_expr(other)?;
                        let (p, pt) = self.unify_param(p, pt, ColumnType::Text);
                        match pt {
                            None | Some(ColumnType::Text) | Some(ColumnType::Any) => {}
                            Some(t) => {
                                return Err(bind_err(format!(
                                    "REGEXP pattern must be text, got {t}"
                                )))
                            }
                        }
                        // Deliberately NOT folded even when both sides are
                        // constants: `fold` evaluates the whole node through the
                        // IR, and `BExpr::RegexpDyn` is left out of its foldable
                        // set for the same reason `InList` is — the literal path
                        // above already covers every constant pattern worth
                        // folding, and a non-literal one is a parameter.
                        BExpr::RegexpDyn(Box::new(l), Box::new(p))
                    }
                };
                let e = fold_maybe(maybe_not(r, *negated), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            ast::Expr::ContextRef(key) => {
                if !self.allow_context {
                    return Err(bind_err("current_setting() is not allowed in this expression"));
                }
                if key == crate::STATEMENT_INSTANT_KEY {
                    return Err(bind_err(format!(
                        "`{key}` is a reserved slot name (it carries the statement instant \
                         that a literal 'now' binds to) and cannot be read as a session setting"
                    )));
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
                if key == crate::STATEMENT_INSTANT_KEY {
                    return Err(bind_err(format!(
                        "`{key}` is a reserved slot name (it carries the statement instant \
                         that a literal 'now' binds to) and cannot be read as a session setting"
                    )));
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
                // A ROW VALUE probe — `(x, y) IN ((1, 2), (3, 4))`, and the
                // `VALUES` spelling of the same list — desugars to an OR of the
                // per-element `=` comparisons the scalar path already builds.
                // Each arm goes through `bind_row_value_cmp`, so arity
                // mismatches, type unification, collation and NULL 3VL are
                // decided in ONE place rather than a second copy here.
                if matches!(lhs.as_ref(), ast::Expr::RowValue(_)) {
                    return self.bind_row_value_in(lhs, items, *negated);
                }
                // The IR encodes the arity in a u16, and the stack verifier
                // proves depth n+1; both need this bound to be real.
                if items.len() > u16::MAX as usize {
                    return Err(bind_err("IN list is too long (max 65535 values)"));
                }
                // `x COLLATE <coll> IN (…)` — the probe's collation governs the
                // membership test (sqlite's left-operand rule). Peel it off the
                // probe so the inner expression binds normally.
                let (lhs_ast, lhs_coll) = peel_collate(lhs)?;
                let (l, lt) = self.bind_expr(lhs_ast)?;
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
                // `x IN (…)` compares under the LEFT operand's (probe's)
                // collation: an explicit `COLLATE` on the probe, else the probe
                // COLUMN's declared collation (rung 2), else BINARY.
                let coll = lhs_coll
                    .or_else(|| match &l {
                        BExpr::Col(idx) => Some(self.scope.column_collation(*idx)),
                        _ => None,
                    })
                    .unwrap_or_default();
                let node = if coll == Collation::Binary {
                    BExpr::InList(Box::new(l), all)
                } else {
                    BExpr::InListColl(Box::new(l), all, coll)
                };
                Ok((maybe_not(node, *negated), Some(ColumnType::Bool)))
            }
            ast::Expr::Case(arms, else_) => {
                let mut bound_conds = Vec::with_capacity(arms.len());
                let mut results = Vec::with_capacity(arms.len() + 1);
                for (c, r) in arms {
                    let (bc, ct) = self.bind_expr(c)?;
                    // A WHEN must be a predicate. A non-boolean one is
                    // truthy-tested exactly as sqlite does (`coerce_bool_ctx`), so
                    // `CASE WHEN 1 THEN …` compiles; only the PostgreSQL dialect
                    // keeps mpedb's original rigid refusal.
                    let (bc, ct) = self.coerce_bool_ctx(bc, ct)?;
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
                    .column_index(name)
                    .ok_or_else(|| bind_err(format!("unknown column `excluded.{name}`")))?
                    as usize;
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
            ast::Expr::Agg(f, _, _, _, _) => Err(bind_err(format!(
                "{}() is an aggregate and cannot be used here — aggregates are only \
                 allowed in a SELECT list or HAVING. A per-row filter is WHERE; a \
                 filter on a GROUPED result is HAVING.",
                f.name()
            ))),
            // A window function reaching the binder was NOT lifted by the window
            // planner, so it sits somewhere a window has no meaning — a WHERE,
            // HAVING, GROUP BY key, ON condition, an aggregate's argument, or a
            // nested window's PARTITION/ORDER/argument. Refuse it here so the
            // direct query path (which never round-trips through decode/validate)
            // rejects it in-process, with a message naming where windows are
            // allowed.
            ast::Expr::Window { .. } => Err(bind_err(
                "window functions may only appear in the SELECT list and ORDER BY \
                 — not in WHERE, GROUP BY, HAVING, a JOIN condition, an aggregate's \
                 argument, or inside another window",
            )),
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
                // A bare parameter among the arms must NOT be pinned by its
                // siblings. `coalesce` returns one branch VERBATIM — it never
                // combines them — so the parameter has no storage requirement
                // and its only consumer is whatever reads the result. Letting a
                // sibling pin it is how Django's `Concat` broke: it wraps every
                // operand in `COALESCE(expr, '')`, so binding `''` against an
                // `IntegerField` arm demanded an int and refused the empty
                // string.
                //
                // Recorded BEFORE unification and restored after, because
                // `unify_param` only writes `param_types` — it does not wrap the
                // expression — so releasing the slot leaves nothing
                // inconsistent. Narrow on purpose: only a param NOTHING ELSE has
                // already pinned, and only here.
                let free_params: Vec<u16> = bound
                    .iter()
                    .filter_map(|(e, _)| match e {
                        BExpr::Param(i)
                            if self
                                .param_types
                                .get(*i as usize)
                                .copied()
                                .flatten()
                                .is_none() =>
                        {
                            Some(*i)
                        }
                        _ => None,
                    })
                    .collect();
                // All branches are the one result, so they must unify — same
                // rule as CASE, and for the same reason. The unification still
                // runs, so `coalesce(int_col, text_col)` is still refused.
                let (bound, ty) = self.unify_result_arms(bound, "mix coalesce() argument types")?;
                if free_params.is_empty() {
                    return Ok((self.fold_coalesce(bound)?, ty));
                }
                for i in &free_params {
                    self.param_types[*i as usize] = None;
                }
                // The result is decided per value at runtime, like any other
                // untyped expression.
                Ok((self.fold_coalesce(bound)?, Some(ColumnType::Any)))
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
                // `x IN (SELECT …)` compares under the LEFT operand's
                // collation, exactly as the literal-list form above does:
                // an explicit `COLLATE` on the probe (rung 1), else the probe
                // COLUMN's declared one (rung 2), else BINARY. This arm read
                // neither, so a `COLLATE NOCASE` column compared BYTEWISE and
                // answered wrongly — measured against 3.45.1 with `a` holding
                // 'AB' and the subquery yielding 'ab': `a IN (…)` gave NO rows
                // where sqlite gives the row, and `a NOT IN (…)` gave BOTH
                // rows where sqlite gives one.
                let (lhs_ast, lhs_coll) = peel_collate(lhs)?;
                let (l, _lt) = self.bind_expr(lhs_ast)?;
                // Rung 1 (explicit COLLATE) then rung 2 (the probe COLUMN),
                // and only if BOTH are absent, rung 4: the SUBQUERY's output
                // collation. Measured — left always wins, and the one place
                // sqlite departs from that (an explicit COLLATE in the
                // subquery's projection) is a form this binder still refuses
                // by name, so it cannot be reached here.
                let coll = lhs_coll
                    .or_else(|| self.operand_collation(&l))
                    .or_else(|| self.slot_colls.get(*slot as usize).copied().flatten())
                    .unwrap_or_default();
                let node = if coll == Collation::Binary {
                    BExpr::InParam(Box::new(l), *slot)
                } else {
                    BExpr::InParamColl(Box::new(l), *slot, coll)
                };
                Ok((maybe_not(node, *negated), Some(ColumnType::Bool)))
            }
            ast::Expr::InSubquery(..) => Err(bind_err(
                "an IN subquery here was not lifted — this expression position \
                 does not support subqueries yet",
            )),
            ast::Expr::Subquery(_) | ast::Expr::Exists(..) => Err(bind_err(
                "a subquery is not supported in this position — subqueries work in \
                 the SELECT list and WHERE of a plain (non-aggregate) SELECT",
            )),
            ast::Expr::Cast(a, tyname) => {
                let aff = Affinity::from_type_name(tyname);
                let (a, at) = self.bind_expr(a)?;
                // A CAST's operand takes ANY type — converting is the whole
                // point of the cast. `CAST(? AS t)` used to PIN the parameter
                // to the affinity's storage type, which is PostgreSQL's way of
                // typing a bare param but makes the cast refuse exactly the
                // values it exists to convert: `INSERT INTO t (x) VALUES
                // (CAST(? AS VARCHAR(50)))` with an integer is what SQLAlchemy
                // writes, and sqlite stores `'1'`.
                //
                // The RESULT stays typed by the affinity (`cast_result_type`
                // below), so nothing downstream loses a type — only the
                // parameter SLOT is left free, which is what lets the caller
                // pass what sqlite would have converted.
                let e = fold_maybe(BExpr::Cast(Box::new(a), aff), self.suppress_fold)?;
                // The bind-time result type. A folded constant reports its own
                // concrete type; otherwise the affinity fixes it, except NUMERIC
                // whose type follows the source (an int/real source keeps its
                // type; text/blob becomes `Any` — decided per value at runtime).
                let ty = if let BExpr::Const(v) = &e {
                    v.column_type()
                } else {
                    cast_result_type(aff, at)
                };
                Ok((e, ty))
            }
            ast::Expr::Collate(_, name) => {
                // Validate the name so an unknown collation is reported as such
                // even in an unsupported position. A COLLATE reaches here only
                // when it is NOT a direct comparison operand or ORDER BY term
                // (those peel it before binding) — so it could not change any
                // comparison or sort, and mpedb refuses it rather than silently
                // dropping it (which under DISTINCT/GROUP BY would be a wrong
                // answer). Column-declared collation is stage 1b.
                resolve_collation(name)?;
                Err(bind_err(
                    "COLLATE is only supported directly on a comparison operand \
                     (e.g. `x = y COLLATE NOCASE`) or an ORDER BY term",
                ))
            }
            // A row value is not a scalar: it is legal ONLY as a direct operand
            // of a comparison, which `bind_binary` intercepts BEFORE reaching
            // here. Anything else — a SELECT-list item, an arithmetic operand, a
            // function argument, an IN probe/element — is a misuse, exactly as
            // sqlite reports it.
            ast::Expr::RowValue(_) => Err(bind_err("row value misused")),
        }
    }
}
