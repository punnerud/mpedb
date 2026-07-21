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

/// The names + arities of the HOST-registered UDFs visible to the connection
/// compiling this statement (the C-API `create_function` path,
/// design/DESIGN-UDF.md). Threaded into the binder exactly as the compat dialect
/// is (`set_dialect`/`set_host_udfs`): a function call that matches no native
/// scalar/aggregate but DOES match a registered `(name, argc)` (or a variadic
/// `(name, -1)`) compiles to a [`BExpr::HostCall`]. Empty for every connection
/// that registered none — then function resolution is exactly as before.
///
/// Stage 2 adds `aggs`, the `xStep`/`xFinal` registrations. Those are resolved
/// EARLIER than scalars — in the PARSER, because `myagg(DISTINCT x) FILTER
/// (WHERE …)` is aggregate GRAMMAR and the parser must know to take that branch
/// before it reads the argument list. The two namespaces are checked in the
/// order native aggregate → host aggregate → native scalar → host scalar, so a
/// name registered as both an aggregate and a scalar is read as the aggregate.
#[derive(Debug, Clone, Default)]
pub struct HostUdfSet {
    fns: Vec<(String, i32)>,
    aggs: Vec<(String, i32)>,
    /// HOST collating-sequence names (`sqlite3_create_collation`). Names only:
    /// a collation has no arity, and the comparator itself never leaves the
    /// connection's registry — the plan carries the NAME and the executor
    /// resolves it (design/DESIGN-UDF.md stage 3).
    colls: Vec<String>,
    /// Host aggregates registered with sqlite's WINDOW protocol
    /// (`create_window_function` — `xValue`/`xInverse` on top of
    /// `xStep`/`xFinal`). A NAME subset of `aggs`: an entry here is also in
    /// `aggs`, and only an entry here may take an `OVER` clause.
    window_aggs: Vec<String>,
}

impl HostUdfSet {
    /// Build from `(name, n_arg)` pairs; `n_arg == -1` is sqlite's variadic
    /// "any arity" registration.
    pub fn new(fns: Vec<(String, i32)>) -> HostUdfSet {
        HostUdfSet { fns, ..Default::default() }
    }

    /// Build from the scalar AND aggregate registrations.
    pub fn with_aggs(fns: Vec<(String, i32)>, aggs: Vec<(String, i32)>) -> HostUdfSet {
        HostUdfSet { fns, aggs, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.fns.is_empty() && self.aggs.is_empty() && self.colls.is_empty()
    }

    /// The registered host AGGREGATE names, for the parser's grammar decision.
    /// Name-only on purpose: the parser must choose the aggregate branch BEFORE
    /// it has parsed the arguments, so arity is checked afterwards
    /// ([`host_agg_arity_ok`](Self::host_agg_arity_ok)).
    /// The registered host COLLATION names, for the ORDER-BY peel. Empty for
    /// every caller that registered none, so collation resolution is exactly as
    /// before for them.
    pub fn colls(&self) -> &[String] {
        &self.colls
    }

    /// Replace the host COLLATION names (the shim registers them per
    /// connection, alongside the scalar/aggregate registries).
    pub fn set_colls(&mut self, colls: Vec<String>) {
        self.colls = colls;
    }

    /// The host aggregates that may be used as WINDOW functions.
    pub fn window_aggs(&self) -> &[String] {
        &self.window_aggs
    }

    /// Replace the window-capable subset (the shim registers these per
    /// connection alongside the plain aggregates).
    pub fn set_window_aggs(&mut self, names: Vec<String>) {
        self.window_aggs = names;
    }

    pub fn agg_names(&self) -> Vec<String> {
        self.aggs.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The `(name, n_arg)` pairs of the registered host aggregates.
    pub fn aggs(&self) -> &[(String, i32)] {
        &self.aggs
    }

    /// Is `name` registered as a host aggregate accepting `argc` arguments?
    /// Exact arity or a variadic `-1`, the same rule scalars use.
    pub fn host_agg_arity_ok(&self, name: &str, argc: usize) -> bool {
        let argc = argc as i32;
        self.aggs
            .iter()
            .any(|(n, a)| n == name && (*a == argc || *a == -1))
    }

    /// Does a call `name(<argc args>)` match a registered host UDF? An exact
    /// `(name, argc)` wins; otherwise a variadic `(name, -1)` also matches.
    fn resolves(&self, name: &str, argc: usize) -> bool {
        let argc = argc as i32;
        self.fns
            .iter()
            .any(|(n, a)| n == name && (*a == argc || *a == -1))
    }
}


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
            // A real column wins; only then does an implicit-rowid table's
            // `rowid`/`_rowid_`/`oid` alias resolve to the hidden column (#94),
            // matching sqlite's shadowing rule. `column_index` already finds the
            // literal `rowid` column, so this fallback covers the other spellings
            // and case variants without changing explicit-PK name resolution.
            if let Some(i) = t.column_index(name).or_else(|| t.rowid_name_col(name)) {
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

    /// The `(addressing-name, table)` pairs, in tuple order. Lets a caller
    /// rebuild an EXTENDED scope (base tables ‖ a synthetic tuple) without
    /// knowing whether the base is one table or a join — the window planner
    /// appends its `__w{k}` result table this way (design/DESIGN-WINDOW.md §3.3).
    pub fn named(&self) -> Vec<(String, &'a TableDef)> {
        self.names
            .iter()
            .cloned()
            .zip(self.tables.iter().copied())
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

    /// The DECLARED collating sequence of the column at tuple slot `c` — sqlite's
    /// comparison/ORDER-BY precedence rung "if the operand is a column, use the
    /// column's collation". [`Collation::Binary`] for an out-of-range slot (a
    /// synthetic tuple with no such column), which degrades to the default.
    pub fn column_collation(&self, c: u16) -> Collation {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                return t.columns[c as usize - base].collation;
            }
            base += t.columns.len();
        }
        Collation::Binary
    }

    /// The `(type, affinity)` of the column at tuple slot `c` — what
    /// `sqlite3ExprAffinity` reads off a column reference, plus the storage type
    /// that says whether the column is the typeless one. `None` for a slot that
    /// names no column (a synthetic tuple), which the caller reads as "no
    /// affinity", exactly as sqlite reads a non-column expression.
    pub fn column_shape(&self, c: u16) -> Option<(ColumnType, Affinity)> {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                let col = &t.columns[c as usize - base];
                return Some((col.ty, col.affinity));
            }
            base += t.columns.len();
        }
        None
    }

    /// Resolve a QUALIFIED `<table>.<column>`. The qualifier is checked rather
    /// than dropped: accepting `nonsense.id` as `id` turns a typo into a
    /// wrong-table read the moment a scope holds more than one table.
    pub fn resolve_qualified(&self, qual: &str, name: &str) -> Result<(u16, ColumnType)> {
        for (k, t) in self.tables.iter().enumerate() {
            if self.names[k].eq_ignore_ascii_case(qual) {
                let i = t.column_index(name).or_else(|| t.rowid_name_col(name)).ok_or_else(|| {
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
            n_user_params: self.n_user_params,
            ctx_keys: self.ctx_keys,
            ctx_list_keys: self.ctx_list_keys,
            allow_params: self.allow_params,
            allow_context: self.allow_context,
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
        if !self.allow_context {
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
            ast::Expr::Cast(a, tyname) => {
                let aff = Affinity::from_type_name(tyname);
                let (a, at) = self.bind_expr(a)?;
                // `CAST(? AS t)` pins a bare parameter to the affinity's storage
                // type — PG's canonical way to type a param. NUMERIC has no
                // single storage type, so it does not pin.
                let (a, at) = match affinity_pin_type(aff) {
                    Some(pin) => self.unify_param(a, at, pin),
                    None => (a, at),
                };
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

    fn bind_binary(&mut self, op: BinOp, l: &ast::Expr, r: &ast::Expr) -> Result<(BExpr, Ty)> {
        // COLLATE is honored on comparison operands only. Peel a top-level
        // COLLATE off each side HERE, before binding, so the inner expression
        // binds normally and the resolved collation feeds the precedence rule
        // below. For every other operator the raw operands are bound, so a
        // COLLATE there reaches `bind_expr` and is refused rather than ignored.
        let is_cmp = matches!(
            op,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        );
        // Row-value (tuple) comparison — `(a, …) OP (b, …)` with a parenthesized
        // list of ≥2 expressions on at least one side. Desugars to scalar boolean
        // logic (see `bind_row_value_cmp`); NO plan/format change. Intercepted
        // before the operand bind below so the row values do not hit the
        // "row value misused" arm.
        if is_cmp
            && (matches!(l, ast::Expr::RowValue(_)) || matches!(r, ast::Expr::RowValue(_)))
        {
            return self.bind_row_value_cmp(op, l, r);
        }
        let (l_ast, l_coll, r_ast, r_coll) = if is_cmp {
            let (la, lc) = peel_collate(l)?;
            let (ra, rc) = peel_collate(r)?;
            (la, lc, ra, rc)
        } else {
            (l, None, r, None)
        };
        let (l, lt) = self.bind_expr(l_ast)?;
        let (r, rt) = self.bind_expr(r_ast)?;
        match op {
            // The two JSON accessors are OPERATORS in the grammar but scalar
            // CALLS in the IR, so they never reach `BExpr::Binary` — one less
            // opcode, and `->`/`->>` share the whole path machinery with
            // `json_extract`. `->` always yields JSON text (or NULL); `->>`
            // yields whatever SQL value the node unwraps to, hence `Any`.
            BinOp::JsonArrow | BinOp::JsonArrowText => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                match lt {
                    Some(ColumnType::Text) | Some(ColumnType::Any) | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "`{}` expects JSON text on the left, got {other}",
                            op_symbol(op)
                        )))
                    }
                }
                // The right operand is a path (text) or an array index
                // (integer); a bare param adopts text, which is what every ORM
                // binds there.
                let (r, rt) = self.unify_param(r, rt, ColumnType::Text);
                match rt {
                    Some(ColumnType::Text)
                    | Some(ColumnType::Int64)
                    | Some(ColumnType::Any)
                    | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "`{}` expects a JSON path (text) or an array index (int64) on the \
                             right, got {other}",
                            op_symbol(op)
                        )))
                    }
                }
                let f = if op == BinOp::JsonArrow {
                    ScalarFn::JsonArrow
                } else {
                    ScalarFn::JsonArrowText
                };
                let ret = if op == BinOp::JsonArrow {
                    ColumnType::Text
                } else {
                    ColumnType::Any
                };
                let e = fold_maybe(BExpr::Call(f, vec![l, r]), self.suppress_fold)?;
                Ok((e, Some(ret)))
            }
            BinOp::And | BinOp::Or => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Bool);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Bool);
                let (l, lt) = self.coerce_bool_ctx(l, lt)?;
                let (r, rt) = self.coerce_bool_ctx(r, rt)?;
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
                let (l, lt, r, rt) = self.bridge_bool_int(l, lt, r, rt)?;
                // Equality pins from columns so `WHERE id = ?` stays Binary and
                // remains a PkPoint/IndexPoint. Inequality leaves the param free
                // (ClassCmp+Numeric) so `year >= 1942.1` is exact numeric compare.
                let is_eq = matches!(op, BinOp::Eq | BinOp::Ne);
                let (l, r, unified) = if is_eq {
                    self.unify_compare_eq(l, lt, r, rt)?
                } else {
                    self.unify_compare_operands(l, lt, r, rt)?
                };
                // sqlite's collation precedence, in order: an explicit `COLLATE`
                // on the LEFT operand, else on the RIGHT; else the LEFT operand's
                // COLUMN collation (rung 2), else the RIGHT column's; else BINARY.
                // A non-Binary result gets its own `CollateCmp` node so the
                // access-path extractor never mistakes it for an index probe; a
                // Binary comparison stays a plain `Binary` node, byte-for-byte
                // unchanged (and a Binary-collated text column resolves to Binary,
                // so an index/PK equality on it is untouched). Collation degrades
                // to bytewise for non-text at runtime, so emitting it for any
                // statically-unpinned operand is safe.
                let col_coll = |e: &BExpr| match e {
                    BExpr::Col(idx) => Some(self.scope.column_collation(*idx)),
                    _ => None,
                };
                let coll = l_coll
                    .or(r_coll)
                    .or_else(|| col_coll(&l))
                    .or_else(|| col_coll(&r))
                    .unwrap_or_default();
                // Comparison affinity + storage-class order. Equality against a
                // typed column deliberately does NOT take ClassCmp (keeps the
                // Binary probe). See `class_cmp_affinity`.
                let node = match self.class_cmp_affinity(unified, &l, &r, is_eq) {
                    Some(aff) => BExpr::ClassCmp(op, Box::new(l), Box::new(r), coll, aff),
                    None if coll == Collation::Binary => {
                        BExpr::Binary(op, Box::new(l), Box::new(r))
                    }
                    None => BExpr::CollateCmp(op, Box::new(l), Box::new(r), coll),
                };
                let e = fold_maybe(node, self.suppress_fold)?;
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
                    // `Any` is admitted for the same reason comparison admits
                    // it: the operand's real type is only known per value, and
                    // the runtime `arith` already refuses a non-numeric one.
                    // Without this, `doc ->> '$.n' + 1` — and every arithmetic
                    // over a host UDF result — would be a COMPILE error even
                    // though the values are numbers.
                    if t != ColumnType::Int64
                        && t != ColumnType::Float64
                        && t != ColumnType::Any
                    {
                        return Err(bind_err(format!(
                            "arithmetic requires int64 or float64 operands, got {t}"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, ty))
            }
            // `&`, `|`, `<<`, `>>` (task #74 item 2). NOT unified like
            // arithmetic: sqlite's bitwise operators do not have a "wider
            // operand type" at all — both sides are cast to an integer and the
            // result is ALWAYS an integer. So each side is typed on its own and
            // the result is int64 regardless.
            BinOp::BitAnd | BinOp::BitOr | BinOp::Shl | BinOp::Shr => {
                let name = bit_op_name(op);
                let (l, lt) = self.unify_param(l, lt, ColumnType::Int64);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Int64);
                let l = self.bit_operand(l, lt, name)?;
                let r = self.bit_operand(r, rt, name)?;
                let e =
                    fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Int64)))
            }
        }
    }

    /// Type-check ONE operand of a bitwise operator.
    ///
    /// sqlite casts every operand to an integer with a total conversion
    /// (`sqlite3VdbeIntValue`): a real truncates toward zero, a text takes an
    /// integer-prefix parse, `'abc'` becomes 0. mpedb accepts the operand types
    /// where that conversion is a NO-OP and refuses the rest by name:
    ///
    /// * `int64` — the operand type these operators are for.
    /// * `bool` — sqlite has no boolean; it IS the integer 0/1, the same
    ///   mapping `bind_assign` already uses for `SET int_col = (a = b)`.
    /// * `any` — the typeless escape. Its runtime value gets sqlite's FULL
    ///   coercion in [`mpedb_types::expr`], which is the contract `any` already
    ///   has for comparisons (`Instr::CmpClass`): rigid types are pinned at
    ///   compile time, `any` gets sqlite's runtime rules.
    /// * an untyped NULL — propagates, like every other operator.
    ///
    /// A statically-typed `float64`, `text` or `blob` is REFUSED, and refused
    /// rather than silently truncated for the same reason a non-integral
    /// parameter is (task #74 item 1): `r & 1` on a column of reals would
    /// answer a question about `trunc(r)` without saying so. `CAST(r AS
    /// INTEGER)` asks for it explicitly and is what the message names.
    fn bit_operand(&mut self, e: BExpr, t: Ty, op: &str) -> Result<BExpr> {
        match t {
            None | Some(ColumnType::Int64) | Some(ColumnType::Bool) | Some(ColumnType::Any) => {
                Ok(e)
            }
            Some(t) => Err(bind_err(format!(
                "`{op}` requires int64 operands, got {t} — sqlite would silently \
                 convert it to an integer (truncating a real, taking the leading \
                 digits of a text); write `CAST(x AS INTEGER)` to ask for that"
            ))),
        }
    }

    /// Bind a ROW-VALUE (tuple) comparison `(a1,…,an) OP (b1,…,bn)`. Both sides
    /// must be explicit row values of EQUAL arity; the comparison desugars to
    /// ordinary scalar boolean logic (see [`Self::desugar_row_cmp`]) which is
    /// provably NULL-correct 3VL and matches sqlite bit-for-bit — there is no
    /// plan/format change. Every other shape is refused as a clean bind error
    /// (never a wrong answer): a row value against a scalar, a subquery RHS, or
    /// an arity mismatch.
    fn bind_row_value_cmp(&mut self, op: BinOp, l: &ast::Expr, r: &ast::Expr) -> Result<(BExpr, Ty)> {
        use ast::Expr as E;
        let (lhs, rhs) = match (l, r) {
            (E::RowValue(a), E::RowValue(b)) => (a, b),
            // `(a, b) = (SELECT …)` — a row value against a subquery. Deferred by
            // name. (In a plain SELECT the scalar-subquery lift runs before the
            // binder, so the subquery arrives here only from a CHECK / policy /
            // trigger expression, which is not lifted; a single-column subquery
            // in a plain SELECT is lifted to a scalar param and lands in the
            // "row value misused" arm below, which is likewise a clean refusal.)
            (E::RowValue(_), E::Subquery(_))
            | (E::Subquery(_), E::RowValue(_))
            | (E::RowValue(_), E::InSubquery(..))
            | (E::InSubquery(..), E::RowValue(_)) => {
                return Err(bind_err(
                    "a row value compared against a subquery is not supported",
                ));
            }
            // A row value against a scalar (or vice versa) — sqlite: "row value
            // misused".
            _ => return Err(bind_err("row value misused")),
        };
        if lhs.len() != rhs.len() {
            return Err(bind_err(format!(
                "row values have an unequal number of columns: left has {}, right has {}",
                lhs.len(),
                rhs.len()
            )));
        }
        // The parser only ever builds a RowValue with ≥2 elements; be defensive
        // rather than index out of range if that ever changes.
        if lhs.is_empty() {
            return Err(bind_err("row value misused"));
        }
        let desugared = Self::desugar_row_cmp(op, lhs, rhs);
        // Bind the desugared scalar expression through the ordinary path: each
        // element pair binds exactly like the corresponding scalar comparison
        // (same type unification, coercions, collation precedence and folding),
        // and the And/Or/Not combinators fold bottom-up — so the result is a
        // fully constant-folded `BExpr` typed `Bool`, with no new node kind.
        self.bind_expr(&desugared)
    }

    /// Desugar `(a1,…,an) OP (b1,…,bn)` (equal arity ≥ 1) into the scalar boolean
    /// expression sqlite uses — provably NULL-correct 3VL:
    ///
    /// - `=`  → `a1=b1 AND … AND an=bn`
    /// - `<>` → `NOT (a1=b1 AND … AND an=bn)`
    /// - `<`  → `a1<b1 OR (a1=b1 AND (a2<b2 OR (a2=b2 AND (… AND an<bn))))`
    ///   (right-nested, lexicographic).
    /// - `<=` / `>` / `>=` — the same lexicographic shape; only the operator
    ///   differs: a STRICT `<`/`>` at every non-last level, and the base operator
    ///   `<`/`<=`/`>`/`>=` at the LAST element.
    ///
    /// Building an `ast::Expr` (rather than a `BExpr`) and re-binding it is what
    /// makes each element pair reuse the scalar comparison binding verbatim.
    fn desugar_row_cmp(op: BinOp, a: &[ast::Expr], b: &[ast::Expr]) -> ast::Expr {
        use ast::Expr as E;
        let cmp = |i: usize, o: BinOp| -> E {
            E::Binary(o, Box::new(a[i].clone()), Box::new(b[i].clone()))
        };
        let eq = |i: usize| cmp(i, BinOp::Eq);
        let n = a.len();
        match op {
            BinOp::Eq | BinOp::Ne => {
                // Conjunction of element equalities; `<>` negates the whole.
                let mut acc = eq(0);
                for i in 1..n {
                    acc = E::Binary(BinOp::And, Box::new(acc), Box::new(eq(i)));
                }
                if op == BinOp::Ne {
                    E::Unary(ast::UnOp::Not, Box::new(acc))
                } else {
                    acc
                }
            }
            // The four ordering operators share one right-nested recursion; the
            // strict per-level operator and the base (last-element) operator are
            // the only difference between them.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let (strict, last) = match op {
                    BinOp::Lt => (BinOp::Lt, BinOp::Lt),
                    BinOp::Le => (BinOp::Lt, BinOp::Le),
                    BinOp::Gt => (BinOp::Gt, BinOp::Gt),
                    BinOp::Ge => (BinOp::Gt, BinOp::Ge),
                    _ => unreachable!(),
                };
                // Build from the last element back to the first.
                let mut acc = cmp(n - 1, last);
                for i in (0..n - 1).rev() {
                    // a_i strict b_i OR (a_i = b_i AND acc)
                    let tail = E::Binary(BinOp::And, Box::new(eq(i)), Box::new(acc));
                    acc = E::Binary(BinOp::Or, Box::new(cmp(i, strict)), Box::new(tail));
                }
                acc
            }
            // Not reachable: `bind_row_value_cmp` is only called for the six
            // comparison operators.
            _ => unreachable!("desugar_row_cmp called with a non-comparison operator"),
        }
    }

    /// Make both operands the same type: unify bare parameters, apply the one
    /// legal coercion (Int64 -> Float64), reject everything else cross-type.
    /// Returns the (possibly coerced) operands and the common type
    /// (`None` when it could not be pinned).
    /// Bridge a `bool`/`int64` COMPARISON the way sqlite's storage does, and
    /// only for a comparison (`=`, `<`, …, `IS`) — never for arithmetic.
    ///
    /// sqlite has no boolean type: a `BooleanField` column literally holds the
    /// integers 0 and 1, which is why Django writes `WHERE "t"."flag" = 1`.
    /// mpedb keeps a rigid `Bool`, so the two must be reconciled — but by the
    /// integer VALUE of the bool, never by truthiness of the int:
    ///
    /// * an int CONSTANT that is exactly 0 or 1 folds into the bool domain
    ///   (`flag = 1` -> `flag = TRUE`). This is the shape Django emits, and
    ///   keeping both sides `Bool` keeps the node a plain `Binary(Eq, Col,
    ///   Const)` — so an index/PK probe on the column survives. Ordering is
    ///   exact too: `FALSE < TRUE` is `0 < 1`, and 0/1 are the only bools.
    /// * anything else casts the BOOL side UP to its integer 0/1
    ///   (`Instr::Cast(Integer)`). So `flag = 2` is FALSE and `flag = -1` is
    ///   FALSE — which is what sqlite answers, because the column only ever
    ///   holds 0 or 1. Truthy-testing the int instead would make `flag = 2`
    ///   TRUE: a wrong answer, and precisely the over-reach this avoids.
    ///
    /// NULL is untouched on both paths, so 3VL is unchanged.
    fn bridge_bool_int(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, Ty, BExpr, Ty)> {
        use ColumnType::{Bool, Int64};
        if self.bare_group_by != BareGroupBy::Sqlite {
            return Ok((l, lt, r, rt)); // PostgreSQL: `flag = 1` stays an error
        }
        // Fold a 0/1 int literal into the bool domain.
        let as_bool = |e: &BExpr| match e {
            BExpr::Const(Value::Int(i @ (0 | 1))) => Some(BExpr::Const(Value::Bool(*i == 1))),
            _ => None,
        };
        match (lt, rt) {
            (Some(Bool), Some(Int64)) => Ok(match as_bool(&r) {
                Some(rb) => (l, Some(Bool), rb, Some(Bool)),
                None => (
                    BExpr::Cast(Box::new(l), Affinity::Integer),
                    Some(Int64),
                    r,
                    Some(Int64),
                ),
            }),
            (Some(Int64), Some(Bool)) => Ok(match as_bool(&l) {
                Some(lb) => (lb, Some(Bool), r, Some(Bool)),
                None => (
                    l,
                    Some(Int64),
                    BExpr::Cast(Box::new(r), Affinity::Integer),
                    Some(Int64),
                ),
            }),
            _ => Ok((l, lt, r, rt)),
        }
    }

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
        self.unify_types(l, lt, r, rt, verb)
    }

    /// Equality: pin bare params from COLUMN or CAST so `WHERE id = ?` is a
    /// Binary probe (PkPoint). Text/float binds that are not exact for the
    /// column type still refuse at coerce_params (or convert when exact).
    fn unify_compare_eq(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, BExpr, Ty)> {
        let pin_source = |e: &BExpr, t: Ty| -> Option<ColumnType> {
            match (e, t) {
                (BExpr::Col(_), Some(t)) if t != ColumnType::Any => Some(t),
                (BExpr::Cast(_, _), Some(t)) => Some(t),
                _ => None,
            }
        };
        let (l, lt) = match pin_source(&r, rt) {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match pin_source(&l, lt) {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        self.unify_types(l, lt, r, rt, "compare")
    }

    /// Inequality: never pin a bare param from a COLUMN.
    ///
    /// `year >= ?` with a float bind (Django annotate) must compare numerically.
    /// ClassCmp+Numeric does that; Binary+int pin would refuse 1942.1.
    fn unify_compare_operands(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, BExpr, Ty)> {
        let pin_source = |e: &BExpr, t: Ty| -> Option<ColumnType> {
            match (e, t) {
                (BExpr::Cast(_, _), Some(t)) => Some(t),
                _ => None,
            }
        };
        let (l, lt) = match pin_source(&r, rt) {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match pin_source(&l, lt) {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        self.unify_types(l, lt, r, rt, "compare")
    }

    fn unify_types(
        &self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
        verb: &str,
    ) -> Result<(BExpr, BExpr, Ty)> {
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
            // A dynamically-typed operand (`ColumnType::Any` — a host UDF result
            // (design/DESIGN-UDF.md) or a typeless column) unifies with ANY
            // concrete type: the real value is typed at runtime, where `sql_cmp`
            // and `arith` handle the actual pair (numeric comparison already
            // crosses Int/Float). The unified type stays `Any`.
            (Some(ColumnType::Any), Some(_)) | (Some(_), Some(ColumnType::Any)) => {
                Ok((l, r, Some(ColumnType::Any)))
            }
            (Some(a), Some(b)) => Err(bind_err(format!("cannot {verb} {a} and {b}"))),
            (Some(t), None) | (None, Some(t)) => Ok((l, r, Some(t))),
            (None, None) => Ok((l, r, None)),
        }
    }

    /// sqlite's **comparison affinity** for a comparison that touches a
    /// TYPELESS (`any`) column: the affinity applied to BOTH operands before
    /// they are compared by storage class. `None` means "not this rule" — the
    /// caller then keeps the plain comparison, which REFUSES a cross-class pair
    /// exactly as it does today.
    ///
    /// A port of `sqlite3CompareAffinity`, with `sqlite3ExprAffinity` narrowed
    /// to the two shapes that carry one: a COLUMN (its declared affinity) and a
    /// `CAST` (its target's). Everything else — a literal, a parameter, any
    /// computed expression — has NO affinity, which is sqlite's rule too.
    ///
    /// Two gates, both deliberate:
    ///
    /// - the unified type must be `Any` or UNKNOWN. Only a comparison that is
    ///   not statically pinned can meet two storage classes at runtime; every
    ///   rigid one was already pinned by the binder and must stay
    ///   byte-identical. Unknown is the `CAST(? AS NUMERIC) = ?` shape, where
    ///   NUMERIC pins neither side — Django's `DecimalField` filter.
    /// - one operand must CARRY an affinity — a bare `any` COLUMN, or a `CAST`
    ///   (`CAST(x AS NUMERIC) > ?`, which Django writes for every `DecimalField`
    ///   aggregate) — and NEITHER may be a bare column of a concrete type. The
    ///   second half is what keeps the rule from silently rewriting an
    ///   `<indexed column> = <host UDF>` comparison — correct either way, but it
    ///   would lose the index probe (`ClassCmp` is never an access path). A CAST
    ///   is never an index probe itself, so admitting it costs no access path.
    ///
    /// Everything outside those gates keeps today's behavior, which is the only
    /// reason this can be landed without auditing every comparison in the
    /// language: an unvetted pair still refuses rather than ordering by class,
    /// and ordering by class WITHOUT the affinity is the wrong answer this
    /// rule exists to avoid (`price < '40.0'` would say "every number is
    /// smaller than a text" where sqlite compares against 40.0).
    fn class_cmp_affinity(
        &self,
        unified: Ty,
        l: &BExpr,
        r: &BExpr,
        is_eq: bool,
    ) -> Option<Affinity> {
        let is_param = |e: &BExpr| matches!(e, BExpr::Param(_));
        let is_col = |e: &BExpr| matches!(e, BExpr::Col(_));
        // Column vs bare param for INEQUALITY only: ClassCmp+Numeric so a float
        // bind against an INTEGER column is exact (Django annotate). Equality
        // keeps Binary so access extraction can still form PkPoint/IndexPoint.
        if !is_eq && ((is_param(l) && is_col(r)) || (is_param(r) && is_col(l))) {
            let col_e = if is_col(l) { l } else { r };
            if let BExpr::Col(i) = col_e {
                if let Some((_, aff)) = self.scope.column_shape(*i) {
                    let numeric = matches!(
                        aff,
                        Affinity::Integer | Affinity::Real | Affinity::Numeric
                    );
                    return Some(if numeric { Affinity::Numeric } else { aff });
                }
            }
        }
        if !matches!(unified, Some(ColumnType::Any) | None) {
            // A concrete unified type normally means both sides already agree
            // and a plain Binary comparison is correct. The one exception is a
            // bare PARAM left untyped against a literal/expression: unified is
            // the concrete side's type, but at runtime the bound value may be
            // any class — emit ClassCmp (no affinity) so `1 = ?` with a text
            // bind answers FALSE rather than "cannot compare".
            if (!is_param(l) && !is_param(r)) || is_col(l) || is_col(r) {
                return None;
            }
            // Fall through with admit via the param path below; `aff_of` for a
            // const/param pair is (None, None) → Blob (no conversion).
        }
        let col_ty = |e: &BExpr| match e {
            BExpr::Col(i) => self.scope.column_shape(*i).map(|(t, _)| t),
            _ => None,
        };
        let is_any_col = |e: &BExpr| col_ty(e) == Some(ColumnType::Any);
        let is_typed_col = |e: &BExpr| col_ty(e).is_some_and(|t| t != ColumnType::Any);
        let is_cast = |e: &BExpr| matches!(e, BExpr::Cast(..));
        // A bare `any` column admits the rule on its own (unchanged). A CAST
        // admits it only when the OTHER side is not a bare concrete column,
        // which is the shape whose index probe must survive. A bare PARAM
        // against a non-column (literal / expression) is the same rule: no
        // affinity, class-order at runtime (CPython `select 1 as a where a=?`).
        let admit = is_any_col(l)
            || is_any_col(r)
            || ((is_cast(l) || is_cast(r)) && !is_typed_col(l) && !is_typed_col(r))
            || ((is_param(l) || is_param(r)) && !is_typed_col(l) && !is_typed_col(r));
        if !admit {
            return None;
        }
        let aff_of = |e: &BExpr| match e {
            BExpr::Col(i) => self.scope.column_shape(*i).map(|(_, a)| a),
            BExpr::Cast(_, a) => Some(*a),
            _ => None,
        };
        let numeric =
            |a: Affinity| matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric);
        Some(match (aff_of(l), aff_of(r)) {
            // Both operands carry an affinity: NUMERIC if either is numeric,
            // else none. (This is where sqlite does NOT apply TEXT: a text
            // column against a typeless one compares raw.)
            (Some(a), Some(b)) => {
                if numeric(a) || numeric(b) {
                    Affinity::Numeric
                } else {
                    Affinity::Blob
                }
            }
            // One side carries an affinity and the other does not: use it.
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Affinity::Blob,
        })
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
            // Fold this REACHABLE arg first, so a foldable constant like `-24`
            // (`Unary(Neg, Const)`, left unfolded by `suppress_fold`) is
            // recognized as the answer — otherwise `coalesce(-24, col)` would
            // keep `col` alive even though it can never be reached. Args AFTER
            // the first non-NULL constant are unreachable and are NEVER folded
            // below (their raise stays suppressed), exactly as before.
            let a = fold(a)?;
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
            // Survivors are already folded above.
            1 => Ok(live.pop().expect("len 1")),
            _ => Ok(BExpr::Coalesce(live)),
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
        // `iif(c, a, b)` is control flow — exactly `CASE WHEN c THEN a ELSE b
        // END`. Desugared here (like nullif) so the CASE path owns the
        // bool-condition rule and the laziness, and iif never NULL-propagates.
        if name == "iif" {
            if args.len() != 3 {
                return Err(bind_err("iif() takes exactly 3 arguments"));
            }
            let case = ast::Expr::Case(
                vec![(args[0].clone(), args[1].clone())],
                Some(Box::new(args[2].clone())),
            );
            return self.bind_expr(&case);
        }
        // `char(X1, …, Xn)` is variadic and every argument is an integer code
        // point, so it is bound here rather than through the fixed-arity `want`
        // table below — each argument is pinned/checked to int64 individually.
        if name == "char" {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                let (e, t) = self.bind_expr(a)?;
                let (e, t) = self.unify_param(e, t, ColumnType::Int64);
                match t {
                    Some(ColumnType::Int64) | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "char() arguments must be int64 code points, got {other}"
                        )))
                    }
                }
                out.push(e);
            }
            if u8::try_from(out.len()).is_err() {
                return Err(bind_err("char() takes at most 255 arguments"));
            }
            return Ok((BExpr::Call(ScalarFn::Char, out), Some(ColumnType::Text)));
        }
        // `printf(FORMAT, …)` / `format(FORMAT, …)` is variadic: the first
        // argument is the format string (pinned to text) and the rest are data
        // arguments of ANY type — the format's specifiers coerce them at
        // runtime, and the format may be a non-literal, so the binder cannot
        // (and must not) pin the data arguments to a type. `format` is an exact
        // alias for `printf`.
        if name == "printf" || name == "format" {
            if args.is_empty() {
                return Err(bind_err(
                    "printf()/format() requires at least a format string argument",
                ));
            }
            let mut out = Vec::with_capacity(args.len());
            for (idx, a) in args.iter().enumerate() {
                let (e, t) = self.bind_expr(a)?;
                if idx == 0 {
                    // The format string must be text; a bare param adopts text.
                    let (e, t) = self.unify_param(e, t, ColumnType::Text);
                    match t {
                        Some(ColumnType::Text) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "printf()/format() format string must be text, got {other}"
                            )))
                        }
                    }
                    out.push(e);
                } else {
                    // A data argument keeps whatever type it has; an untyped bare
                    // param is left for resolve_params to report (printf cannot
                    // pin it — the specifier that consumes it is only known at
                    // runtime).
                    out.push(e);
                }
            }
            if u8::try_from(out.len()).is_err() {
                return Err(bind_err("printf()/format() takes at most 255 arguments"));
            }
            return Ok((BExpr::Call(ScalarFn::Printf, out), Some(ColumnType::Text)));
        }
        // The JSON family. `json_array`/`json_object`/`json_set`/`json_insert`/
        // `json_replace` take VALUE arguments whose reading depends on sqlite's
        // per-value JSON subtype, so they are bound specially (a leading
        // bitmask argument); `json_quote` of an already-JSON argument is that
        // argument. See [`Self::bind_json_call`].
        if name.starts_with("json") {
            if let Some(bound) = self.bind_json_call(name, args)? {
                return Ok(bound);
            }
        }
        // sqlite's SCALAR `max(a, b, …)` / `min(a, b, …)` (#74 item 5). Variadic
        // and typed by SELECTION rather than by computation, which neither the
        // fixed `want` table nor the `ret` recomputation below can express, so
        // it binds here like `char`/`printf` do.
        if (name == "max" || name == "min") && args.len() >= 2 {
            let mut bound = Vec::with_capacity(args.len());
            for a in args {
                bound.push(self.bind_expr(a)?);
            }
            if u8::try_from(bound.len()).is_err() {
                return Err(bind_err(format!("{name}() takes at most 255 arguments")));
            }
            // The distinct CONCRETE argument types (an untyped NULL or an
            // unpinned bare parameter contributes none).
            let mut kinds: Vec<ColumnType> = Vec::new();
            for (_, t) in &bound {
                if let Some(t) = t {
                    if !kinds.contains(t) {
                        kinds.push(*t);
                    }
                }
            }
            // The result type. This is a SELECTION: the winning ARGUMENT is
            // returned unchanged, so a mixed-type call can produce either
            // argument's type and the honest answer is `any`.
            //
            //  * one concrete type  -> that type. `max(i, 3)` is int64.
            //  * numbers only       -> `any`. sqlite's `max(3, 2.5)` is the
            //    INTEGER 3 and `max(1, 2.5)` is the REAL 2.5; widening to
            //    float64 would turn the first into 3.0, a different value.
            //  * an `any` present   -> `any`; the runtime orders by storage
            //    class, which is sqlite's own rule (`Value::sort_cmp`).
            //  * anything else      -> REFUSED by name. sqlite would order a
            //    number against a text by storage class, but that is the same
            //    cross-class comparison `sql_cmp` refuses everywhere else, and
            //    mpedb's own bool/timestamp have no class at all.
            let numeric = |t: &ColumnType| {
                matches!(t, ColumnType::Int64 | ColumnType::Float64 | ColumnType::Any)
            };
            let ret = match kinds.len() {
                0 => None,
                1 => Some(kinds[0]),
                _ if kinds.iter().all(numeric) || kinds.contains(&ColumnType::Any) => {
                    Some(ColumnType::Any)
                }
                _ => {
                    let names: Vec<String> = kinds.iter().map(|t| t.to_string()).collect();
                    return Err(bind_err(format!(
                        "{name}() cannot order arguments of different types ({}) — sqlite \
                         would rank them by storage class, which is the cross-type comparison \
                         mpedb refuses everywhere else; CAST them to one type",
                        names.join(" and ")
                    )));
                }
            };
            // With exactly one concrete type, a bare parameter adopts it — so
            // `max(?, i)` binds the way `? > i` does. With a mixed call there is
            // nothing to adopt, and the parameter is left for `resolve_params`
            // to report.
            let out = bound
                .into_iter()
                .map(|(e, t)| match (kinds.len(), ret) {
                    (1, Some(w)) => self.unify_param(e, t, w).0,
                    _ => e,
                })
                .collect();
            let f = if name == "max" { ScalarFn::Max2 } else { ScalarFn::Min2 };
            return Ok((BExpr::Call(f, out), ret));
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
            "sqrt" => ScalarFn::Sqrt,
            "pow" | "power" => ScalarFn::Pow,
            "sign" => ScalarFn::Sign,
            "ceil" | "ceiling" => ScalarFn::Ceil,
            "floor" => ScalarFn::Floor,
            "trunc" => ScalarFn::Trunc,
            "unicode" => ScalarFn::Unicode,
            "hex" => ScalarFn::Hex,
            "typeof" => ScalarFn::Typeof,
            // sqlite built-ins added for the Django/C-API surface: `quote(X)`
            // (Django's `last_executed_query` calls `QUOTE(?)` per parameter)
            // and `strftime(FORMAT, TIME)`.
            "quote" => ScalarFn::Quote,
            "strftime" => ScalarFn::Strftime,
            // The rest of sqlite's date/time family. All four share
            // `strftime`'s time-string grammar and its refusals; the only
            // difference is the fixed output format (and `julianday`'s REAL).
            "date" => ScalarFn::Date,
            "time" => ScalarFn::Time,
            "datetime" => ScalarFn::DateTime,
            "julianday" => ScalarFn::JulianDay,
            // Math (sqlite 3.45). `log` is base-10 with one argument and
            // log-base-b with two, so it dispatches on the argument count here —
            // `log10`/`log2` name the fixed-base forms directly.
            "exp" => ScalarFn::Exp,
            "ln" => ScalarFn::Ln,
            "log10" => ScalarFn::Log10,
            "log2" => ScalarFn::Log2,
            "log" => match args.len() {
                1 => ScalarFn::Log10,
                2 => ScalarFn::LogBase,
                n => {
                    return Err(bind_err(format!(
                        "log() takes 1 argument (base-10) or 2 (log(base, x)), got {n}"
                    )))
                }
            },
            "sin" => ScalarFn::Sin,
            "cos" => ScalarFn::Cos,
            "tan" => ScalarFn::Tan,
            "asin" => ScalarFn::Asin,
            "acos" => ScalarFn::Acos,
            "atan" => ScalarFn::Atan,
            "atan2" => ScalarFn::Atan2,
            "sinh" => ScalarFn::Sinh,
            "cosh" => ScalarFn::Cosh,
            "tanh" => ScalarFn::Tanh,
            "radians" => ScalarFn::Radians,
            "degrees" => ScalarFn::Degrees,
            "pi" => ScalarFn::Pi,
            "mod" => ScalarFn::Mod,
            // A name that matches no native scalar (nor an aggregate — those are
            // lifted before binding) may still be a HOST-registered UDF (the
            // C-API `create_function` path, design/DESIGN-UDF.md). A host UDF is
            // dynamically typed: bind every argument through unchanged (no
            // pinning) and grade the result to `Any`. A name matching neither is
            // the unchanged "unknown function" error.
            other => {
                if self.host_udfs.resolves(other, args.len()) {
                    if u16::try_from(args.len()).is_err() {
                        return Err(bind_err(format!(
                            "{other}() called with too many arguments"
                        )));
                    }
                    let mut bound = Vec::with_capacity(args.len());
                    for a in args {
                        bound.push(self.bind_expr(a)?.0);
                    }
                    return Ok((
                        BExpr::HostCall {
                            name: other.to_string(),
                            args: bound,
                        },
                        Some(ColumnType::Any),
                    ));
                }
                return Err(bind_err(format!(
                    "unknown function `{other}()`; available: lower, upper, length, trim, \
                     ltrim, rtrim, replace, instr, substr, substring, char, unicode, hex, \
                     typeof, abs, round, ceil, floor, trunc, sqrt, pow, sign, exp, ln, log, \
                     log10, log2, sin, cos, tan, asin, acos, atan, atan2, sinh, cosh, tanh, \
                     radians, degrees, pi, mod, printf, format, quote, strftime, date, \
                     time, datetime, julianday, json, \
                     json_valid, json_type, json_quote, json_array_length, json_extract, \
                     json_array, json_object, json_patch, json_remove, json_replace, \
                     json_set, json_insert, iif, coalesce, ifnull, nullif"
                )));
            }
        };
        // Which argument of this function is sqlite's TIMESTRING (the only
        // position a `'now'` can occupy — every later argument is a modifier,
        // and modifiers are refused wholesale).
        let time_arg: Option<usize> = match f {
            ScalarFn::Strftime => Some(1),
            ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime | ScalarFn::JulianDay => Some(0),
            _ => None,
        };
        let mut bound = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            // A LITERAL `'now'` in the time-value position binds the
            // STATEMENT-START instant: it is rewritten into the reserved
            // instant slot, which the facade fills once per execute (sqlite's
            // `iCurrentTime` rule — one instant per statement, so two `'now'`s
            // in one statement read the SAME slot and agree). The plan itself
            // carries only a parameter reference, so a content-hashed plan
            // shared across processes can never carry a compile-time clock.
            if time_arg == Some(i) && is_literal_now(a) {
                bound.push((self.statement_instant()?, Some(ColumnType::Text)));
                continue;
            }
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
            ScalarFn::Lower | ScalarFn::Upper => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            // length/unicode: text in, integer out.
            ScalarFn::Length | ScalarFn::Unicode => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Int64))
            }
            // abs/round/ceil/floor/trunc keep their argument's numeric type, so
            // they are checked below rather than pinned to one.
            ScalarFn::Abs | ScalarFn::Round | ScalarFn::Ceil | ScalarFn::Floor
            | ScalarFn::Trunc => (&[], None),
            ScalarFn::Substr => (
                &[Some(ColumnType::Text), Some(ColumnType::Int64), Some(ColumnType::Int64)],
                Some(ColumnType::Text),
            ),
            ScalarFn::Replace => (
                &[Some(ColumnType::Text), Some(ColumnType::Text), Some(ColumnType::Text)],
                Some(ColumnType::Text),
            ),
            // trim/ltrim/rtrim: text, and an optional text set of trim chars.
            ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::Instr => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Int64))
            }
            // sqrt/pow and the transcendental math functions take numbers (int
            // or float, unpinned like abs/round) but ALWAYS return a float; `pi`
            // is nullary and also returns a float. sign always returns an integer.
            ScalarFn::Sqrt
            | ScalarFn::Pow
            | ScalarFn::Exp
            | ScalarFn::Ln
            | ScalarFn::Log10
            | ScalarFn::Log2
            | ScalarFn::LogBase
            | ScalarFn::Sin
            | ScalarFn::Cos
            | ScalarFn::Tan
            | ScalarFn::Asin
            | ScalarFn::Acos
            | ScalarFn::Atan
            | ScalarFn::Atan2
            | ScalarFn::Sinh
            | ScalarFn::Cosh
            | ScalarFn::Tanh
            | ScalarFn::Radians
            | ScalarFn::Degrees
            | ScalarFn::Pi
            | ScalarFn::Mod => (&[], Some(ColumnType::Float64)),
            ScalarFn::Sign => (&[], Some(ColumnType::Int64)),
            // hex accepts text OR blob — two types the fixed `want` table
            // cannot express — so its argument is left unpinned and checked in
            // the `ret` recomputation below. typeof accepts ANY type. Both
            // return text.
            ScalarFn::Hex | ScalarFn::Typeof => (&[], Some(ColumnType::Text)),
            // quote(X) accepts EVERY type (that is the point of it) and returns
            // text. Its argument stays unpinned so `quote($1)` — the shape
            // Django's `last_executed_query` emits — binds without the binder
            // having to guess the parameter's type.
            ScalarFn::Quote => (&[], Some(ColumnType::Text)),
            // strftime(FORMAT, TIMESTRING): both text, text out. Pinning the
            // time argument to text is what makes `strftime('%Y', 2455352.5)`
            // — sqlite's Julian-day form — a COMPILE error rather than a
            // per-row surprise.
            ScalarFn::Strftime => (
                &[Some(ColumnType::Text), Some(ColumnType::Text)],
                Some(ColumnType::Text),
            ),
            // date/time/datetime(TIMESTRING): text in, text out — same pin, and
            // for the same reason (the Julian-day NUMBER form is a compile
            // error, not a per-row surprise). julianday returns sqlite's REAL.
            ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::JulianDay => (&[Some(ColumnType::Text)], Some(ColumnType::Float64)),
            // char/printf and the scalar max/min are variadic and bound
            // specially above (never reached here); present only so this match
            // stays exhaustive over ScalarFn.
            ScalarFn::Char | ScalarFn::Printf => (&[], Some(ColumnType::Text)),
            // The whole JSON family is bound by `bind_json_call` (and the two
            // accessors by `bind_binary`), so none of these reach the generic
            // path; the arm exists only to keep the match exhaustive.
            ScalarFn::Json
            | ScalarFn::JsonValid
            | ScalarFn::JsonType
            | ScalarFn::JsonQuote
            | ScalarFn::JsonArrayLength
            | ScalarFn::JsonExtract
            | ScalarFn::JsonArrow
            | ScalarFn::JsonArrowText
            | ScalarFn::JsonArray
            | ScalarFn::JsonObject
            | ScalarFn::JsonPatch
            | ScalarFn::JsonRemove
            | ScalarFn::JsonReplace
            | ScalarFn::JsonSet
            | ScalarFn::JsonInsert => (&[], Some(ColumnType::Text)),
            ScalarFn::Max2 | ScalarFn::Min2 => (&[], None),
        };
        let mut out = Vec::with_capacity(bound.len());
        for (i, (e, t)) in bound.into_iter().enumerate() {
            match want.get(i).copied().flatten() {
                Some(w) => {
                    let (e, t) = self.unify_param(e, t, w);
                    if let Some(t) = t {
                        // A DYNAMICALLY typed argument (`any` — a typeless
                        // column, a host UDF, a per-row CASE) passes: its class
                        // is not known until the row is read, and the runtime
                        // implementation checks the value it actually gets.
                        // That is narrower than sqlite (which coerces
                        // `length(123)` to 3) but the narrowing is a refusal at
                        // the row, never a different value — and refusing at
                        // COMPILE time refused the whole query for values that
                        // are of the right class every time.
                        if t != w && t != ColumnType::Any {
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
            // `round()` is sqlite's one numeric function that does NOT keep the
            // argument's type: it always answers a REAL (`round(7)` is `7.0`).
            ScalarFn::Round => match self.static_type(&out[0]) {
                Some(ColumnType::Int64)
                | Some(ColumnType::Float64)
                | Some(ColumnType::Any)
                | None => Some(ColumnType::Float64),
                Some(other) => {
                    return Err(bind_err(format!("{name}() expects a number, got {other}")))
                }
            },
            ScalarFn::Abs | ScalarFn::Ceil | ScalarFn::Floor | ScalarFn::Trunc => {
                // Numeric in, same numeric out. The type is the argument's —
                // and a DYNAMICALLY typed argument (`any`) keeps `any`: the
                // runtime already preserves int-ness per value (sqlite's
                // `floor(7)` is the integer 7, `floor(7.5)` the real 7.0), and
                // refuses a non-number at the row it meets one.
                let t = self.static_type(&out[0]);
                match t {
                    Some(ColumnType::Int64)
                    | Some(ColumnType::Float64)
                    | Some(ColumnType::Any)
                    | None => t,
                    Some(other) => {
                        return Err(bind_err(format!("{name}() expects a number, got {other}")))
                    }
                }
            }
            // hex accepts text or blob (like the runtime); reject anything else
            // at COMPILE time rather than at the first row.
            ScalarFn::Hex => match self.static_type(&out[0]) {
                Some(ColumnType::Text)
                | Some(ColumnType::Blob)
                | Some(ColumnType::Any)
                | None => Some(ColumnType::Text),
                Some(other) => {
                    return Err(bind_err(format!("hex() expects text or blob, got {other}")))
                }
            },
            _ => ret,
        };
        Ok((BExpr::Call(f, out), ret))
    }

    /// Bind one of sqlite's JSON functions, or `Ok(None)` if `name` is not one
    /// (so a host UDF called `jsonify()` still resolves normally).
    ///
    /// # The JSON subtype, and why it is decided HERE
    ///
    /// sqlite has no JSON type, but it does mark a *value* with an internal
    /// `JSON` subtype whenever a JSON function produced it, and the functions
    /// that take VALUE arguments read that mark:
    ///
    /// ```text
    /// json_object('a', json('[1,2]'))  ->  {"a":[1,2]}     -- spliced raw
    /// json_object('a',      '[1,2]' )  ->  {"a":"[1,2]"}   -- quoted as text
    /// ```
    ///
    /// mpedb's `Value` carries no subtype, and adding one would mean threading
    /// a flag through the whole expression stack. It does not have to: sqlite
    /// sets that mark in exactly one place — the return of a JSON function —
    /// and mpedb can see, at BIND time, whether an argument *is* such a call.
    /// So the binder computes a bitmask of which value arguments are JSON and
    /// prepends it as a hidden leading argument (see `ScalarFn::JsonArray`).
    ///
    /// The three shapes where a static answer could differ from sqlite's
    /// runtime one are REFUSED by name rather than guessed:
    ///
    /// * `json_extract(…)` / `->>` in a value position — sqlite subtypes
    ///   `json_extract`'s result only when the extracted node is an object or
    ///   an array, which is a property of the DATA, not of the query;
    /// * a scalar subquery — sqlite propagates the subtype out of one
    ///   (`json_quote((SELECT json('[1]')))` is `[1]`) but not out of a FROM
    ///   subquery's column, an aggregate, or `||`; mpedb cannot see through
    ///   the subplan boundary to tell those apart;
    /// * a `CASE`/`coalesce`/`iif` whose arms DISAGREE — sqlite's answer is
    ///   whichever arm fires.
    fn bind_json_call(
        &mut self,
        name: &str,
        args: &[ast::Expr],
    ) -> Result<Option<(BExpr, Ty)>> {
        // The table-valued and aggregate JSON functions are a different
        // machinery entirely; name them rather than report "unknown function".
        if matches!(
            name,
            "json_each" | "json_tree" | "json_group_array" | "json_group_object"
        ) {
            return Err(bind_err(format!(
                "{name}() is not implemented: `json_each`/`json_tree` are TABLE-VALUED \
                 functions and `json_group_array`/`json_group_object` are AGGREGATES, neither \
                 of which mpedb's scalar-function machinery can express"
            )));
        }
        if name.starts_with("jsonb") {
            return Err(bind_err(format!(
                "{name}() is not implemented: sqlite 3.45's JSONB is a BINARY encoding stored \
                 in a BLOB, and mpedb implements the TEXT JSON functions only"
            )));
        }
        // Fixed-shape readers: no value arguments, so no subtype question.
        let simple = match name {
            "json" => Some((ScalarFn::Json, ColumnType::Text)),
            "json_valid" => Some((ScalarFn::JsonValid, ColumnType::Int64)),
            "json_type" => Some((ScalarFn::JsonType, ColumnType::Text)),
            "json_array_length" => Some((ScalarFn::JsonArrayLength, ColumnType::Int64)),
            // One path unwraps to whatever the node holds; several wrap into a
            // JSON array (text). `Any` covers both.
            "json_extract" => Some((ScalarFn::JsonExtract, ColumnType::Any)),
            "json_patch" => Some((ScalarFn::JsonPatch, ColumnType::Text)),
            "json_remove" => Some((ScalarFn::JsonRemove, ColumnType::Text)),
            _ => None,
        };
        if let Some((f, ret)) = simple {
            let argc = u8::try_from(args.len())
                .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
            if !f.arity_ok(argc) {
                return Err(bind_err(format!(
                    "{name}() cannot take {argc} argument(s)"
                )));
            }
            let mut out = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                let (e, t) = self.bind_expr(a)?;
                // Argument 0 is the document, and every later argument is a
                // path — except `json_valid`'s FLAGS, which is an integer.
                let want = if i == 1 && f == ScalarFn::JsonValid {
                    ColumnType::Int64
                } else {
                    ColumnType::Text
                };
                let (e, t) = self.unify_param(e, t, want);
                match t {
                    Some(t) if t == want => {}
                    // `json_valid` accepts ANY type for its document argument
                    // (sqlite answers 1 for a number, 0 for a blob), and `Any`
                    // is decided per value at runtime.
                    Some(ColumnType::Any) | None => {}
                    Some(_) if i == 0 && f == ScalarFn::JsonValid => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "{name}() argument {} must be {want}, got {other}",
                            i + 1
                        )))
                    }
                }
                out.push(e);
            }
            return Ok(Some((BExpr::Call(f, out), Some(ret))));
        }
        // `json_quote(X)`: an argument that is ALREADY JSON is returned
        // unchanged by sqlite (its subtype survives), and every JSON-producing
        // call already yields minified JSON text — so the whole call is that
        // argument. Nothing to encode, no mask.
        if name == "json_quote" {
            if args.len() != 1 {
                return Err(bind_err(format!(
                    "json_quote() takes exactly 1 argument, got {}",
                    args.len()
                )));
            }
            if self.json_ness(&args[0], "json_quote()")? {
                let (e, _) = self.bind_expr(&args[0])?;
                return Ok(Some((e, Some(ColumnType::Text))));
            }
            let (e, _) = self.bind_expr(&args[0])?;
            return Ok(Some((
                BExpr::Call(ScalarFn::JsonQuote, vec![e]),
                Some(ColumnType::Text),
            )));
        }
        // The writers: a leading subtype bitmask, then the SQL arguments.
        // `value_at` says which argument positions are VALUES (the rest are
        // documents or paths, always read as JSON/text).
        let f = match name {
            "json_array" => ScalarFn::JsonArray,
            "json_object" => ScalarFn::JsonObject,
            "json_set" => ScalarFn::JsonSet,
            "json_insert" => ScalarFn::JsonInsert,
            "json_replace" => ScalarFn::JsonReplace,
            _ => return Ok(None),
        };
        // ONE table of value positions, shared with the lifter's subquery
        // refusal — the two must never drift apart.
        let value_at = json_value_positions(name).expect("a writer has value positions");
        let mut mask: u64 = 0;
        let mut out: Vec<BExpr> = Vec::with_capacity(args.len() + 1);
        // Placeholder; filled in once the mask is known.
        out.push(BExpr::Const(Value::Int(0)));
        for (i, a) in args.iter().enumerate() {
            match value_at(i) {
                Some(slot) => {
                    if slot >= 64 {
                        return Err(bind_err(format!(
                            "{name}() takes at most 64 value arguments in mpedb: the JSON \
                             subtype of each value is carried as a 64-bit mask on the compiled \
                             call"
                        )));
                    }
                    if self.json_ness(a, &format!("{name}()"))? {
                        mask |= 1u64 << slot;
                    }
                    // A value argument keeps whatever type it has: every SQL
                    // type has a JSON rendering (a BLOB is the one runtime
                    // error, matching sqlite's "JSON cannot hold BLOB values").
                    out.push(self.bind_expr(a)?.0);
                }
                None => {
                    // A document/path/label position: text.
                    let (e, t) = self.bind_expr(a)?;
                    let (e, t) = self.unify_param(e, t, ColumnType::Text);
                    match t {
                        Some(ColumnType::Text) | Some(ColumnType::Any) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "{name}() argument {} must be text, got {other}",
                                i + 1
                            )))
                        }
                    }
                    out.push(e);
                }
            }
        }
        out[0] = BExpr::Const(Value::Int(mask as i64));
        let argc = u8::try_from(out.len())
            .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
        if !f.arity_ok(argc) {
            return Err(bind_err(format!(
                "{name}() cannot take {} argument(s)",
                args.len()
            )));
        }
        Ok(Some((BExpr::Call(f, out), Some(ColumnType::Text))))
    }

    /// Is `e` an expression sqlite would mark with the JSON subtype? See
    /// [`Self::bind_json_call`] for why this is decidable and what is refused.
    fn json_ness(&mut self, e: &ast::Expr, what: &str) -> Result<bool> {
        let undecidable = |why: &str| {
            Err(bind_err(format!(
                "{what}: mpedb cannot tell whether this argument is JSON text or a plain \
                 string, because {why}. sqlite decides it from a per-value JSON subtype that \
                 mpedb's values do not carry. Wrap the argument in `json(…)` to splice it as \
                 JSON, or in `'' || …` to force the quoted-string reading"
            )))
        };
        Ok(match e {
            ast::Expr::Func(name, _) => match name.to_ascii_lowercase().as_str() {
                // Every one of these returns minified JSON text with the
                // subtype set (verified against 3.45.1).
                "json" | "json_array" | "json_object" | "json_insert" | "json_replace"
                | "json_set" | "json_remove" | "json_patch" | "json_quote" => true,
                // Value-dependent: sqlite subtypes json_extract's result only
                // when the node is an object or an array.
                "json_extract" => return undecidable("`json_extract()` is JSON only when the \
                                                      extracted node is an object or an array"),
                _ => false,
            },
            // `->` always yields JSON text; `->>` never does (verified:
            // `json_quote('{\"a\":[9]}' ->> '$.a')` is the quoted `"[9]"`).
            ast::Expr::Binary(BinOp::JsonArrow, _, _) => true,
            ast::Expr::Binary(BinOp::JsonArrowText, _, _) => false,
            // The subtype flows with the value through lazy control flow, so
            // fold over the arms — and refuse when they disagree.
            ast::Expr::Case(arms, else_) => {
                let mut it = arms
                    .iter()
                    .map(|(_, r)| r)
                    .chain(else_.iter().map(|b| b.as_ref()));
                let mut acc: Option<bool> = None;
                for arm in &mut it {
                    // A NULL arm is neither: it cannot be observed either way.
                    if matches!(arm, ast::Expr::Lit(Value::Null)) {
                        continue;
                    }
                    let j = self.json_ness(arm, what)?;
                    match acc {
                        None => acc = Some(j),
                        Some(prev) if prev == j => {}
                        Some(_) => {
                            return undecidable(
                                "its CASE arms disagree — some are JSON, some are plain text",
                            )
                        }
                    }
                }
                acc.unwrap_or(false)
            }
            ast::Expr::Coalesce(items) => {
                let mut acc: Option<bool> = None;
                for it in items {
                    if matches!(it, ast::Expr::Lit(Value::Null)) {
                        continue;
                    }
                    let j = self.json_ness(it, what)?;
                    match acc {
                        None => acc = Some(j),
                        Some(prev) if prev == j => {}
                        Some(_) => {
                            return undecidable(
                                "its coalesce/ifnull arms disagree — some are JSON, some are \
                                 plain text",
                            )
                        }
                    }
                }
                acc.unwrap_or(false)
            }
            ast::Expr::Subquery(_) => {
                return undecidable(
                    "it is a scalar subquery, and sqlite propagates the subtype out of one but \
                     not out of a FROM-subquery column or an aggregate",
                )
            }
            // Everything else — a literal, a column, a parameter, `||`, CAST,
            // a non-JSON function, a host UDF — carries no subtype in sqlite
            // either, so plain text is the exact answer.
            _ => false,
        })
    }

    /// The type of an already-bound expression, where it is knowable without
    /// re-binding. Used for the functions whose return type is their argument's.
    fn static_type(&self, e: &BExpr) -> Ty {
        match e {
            BExpr::Const(v) => v.column_type(),
            // A column reference resolves through the WHOLE evaluated tuple —
            // `Scope::column_shape` walks the scoped tables in slot order, the
            // same walk `Scope::resolve` used to hand out the slot.
            //
            // This used to read `scope.only().columns[…]`, which ASSERTS on a
            // scope wider than one table: `SELECT a.id FROM a JOIN b ON … WHERE
            // ABS(b.id) = 1` panicked in the binder, because `abs`/`round`/
            // `ceil`/`floor`/`trunc`/`hex` are the functions whose return type
            // IS their argument's, so binding one over a joined column came
            // through here. The scope was never single-table on this path; only
            // the lookup assumed it was.
            //
            // `excluded.<c>` binds to Col(n + i) over `[existing ‖ proposed]`,
            // which is the one tuple WIDER than the scope: fold the index back
            // into the scope's width so a second-half reference reports the
            // column's real type instead of falling off the end. That scope is
            // single-table by construction (an ON CONFLICT target is one
            // table), so the fold and the join walk never interact.
            BExpr::Col(i) => {
                let n = self.scope.width();
                let slot = (*i as usize % n.max(1)) as u16;
                self.scope.column_shape(slot).map(|(t, _)| t)
            }
            BExpr::Param(i) => self.param_types[*i as usize],
            BExpr::Unary(BUnOp::ToFloat, _) => Some(ColumnType::Float64),
            BExpr::Call(
                ScalarFn::Length | ScalarFn::Instr | ScalarFn::Sign | ScalarFn::Unicode,
                _,
            ) => Some(ColumnType::Int64),
            // sqrt/pow, the transcendental math functions, and nullary pi are
            // always float.
            BExpr::Call(
                ScalarFn::Sqrt
                | ScalarFn::Pow
                | ScalarFn::Exp
                | ScalarFn::Ln
                | ScalarFn::Log10
                | ScalarFn::Log2
                | ScalarFn::LogBase
                | ScalarFn::Sin
                | ScalarFn::Cos
                | ScalarFn::Tan
                | ScalarFn::Asin
                | ScalarFn::Acos
                | ScalarFn::Atan
                | ScalarFn::Atan2
                | ScalarFn::Sinh
                | ScalarFn::Cosh
                | ScalarFn::Tanh
                | ScalarFn::Radians
                | ScalarFn::Degrees
                | ScalarFn::Pi
                | ScalarFn::Mod,
                _,
            ) => Some(ColumnType::Float64),
            BExpr::Call(
                ScalarFn::Abs
                | ScalarFn::Round
                | ScalarFn::Ceil
                | ScalarFn::Floor
                | ScalarFn::Trunc,
                a,
            ) => self.static_type(&a[0]),
            BExpr::Call(_, _) => Some(ColumnType::Text),
            BExpr::IsDistinct(..) => Some(ColumnType::Bool),
            _ => None,
        }
    }

    /// Type the RESULT arms of a CASE / COALESCE (and their sugar: ifnull,
    /// iif, nullif) — arms whose value IS the result. sqlite types these per
    /// ROW: the arm actually taken keeps its own type, so
    /// `COALESCE(30, avg(x)) / 35` divides an INTEGER when arm 1 wins.
    /// Widening 30 to 30.0 is therefore a WRONG ANSWER factory (measured: 82
    /// in the sqllogictest expr tree when it was tried), so **no arm is ever
    /// coerced here**. Instead:
    ///
    ///  * zero or one concrete arm type -> that type, exactly as before; a
    ///    bare parameter adopts it ([`Self::unify_many`], whose int->float
    ///    widening is unreachable with a single kind).
    ///  * a NUMERIC mix (int64 ∪ float64), or any arm already `any` -> every
    ///    arm keeps its own type AND its own value, and the result is typed
    ///    per VALUE at runtime: [`ColumnType::Any`]. The CASE/COALESCE
    ///    runtime is pure control flow (the winning arm's value is returned
    ///    untouched), so the per-row semantics are exact; every downstream
    ///    consumer of `any` already exists — `typeof()` reads the value,
    ///    comparison unification admits `any` ([`Self::unify_operands`]),
    ///    arithmetic settles per value, ORDER BY uses `Value::sort_cmp`,
    ///    DISTINCT/GROUP BY key via `encode_group_key`, and sum/avg/min/max
    ///    accumulate mixed int/float exactly as sqlite does. This is the
    ///    same rule, for the same selection-not-computation reason, as
    ///    scalar `max()`/`min()`.
    ///  * any other mix (text ∪ int64, blob ∪ text, bool/timestamp ∪
    ///    anything) -> still refused with the CAST fix in the message.
    ///    sqlite legalizes those too, but the mpedb runtime refuses a
    ///    cross-CLASS comparison rather than rank number-vs-text, so an
    ///    `any` holding such a mix invites runtime refusals downstream —
    ///    and mpedb's own bool/timestamp have no sqlite storage class at
    ///    all. No measured corpus record needs them (design/CORPUS-STATUS.md).
    ///
    /// sqlite dialect ONLY ([`Self::sqlite_dialect`], like `coerce_bool_ctx`
    /// and `like_glob_operand`): PostgreSQL types COALESCE/CASE statically by
    /// promoting the arms to their common numeric supertype, so
    /// `COALESCE(30, 1.5) / 35` is NUMERIC division in PG (≈0.857) where the
    /// per-row rule divides integers (0). Under the postgres dialect the
    /// original rigid refusal is kept — a clean error, never either engine's
    /// wrong answer.
    ///
    /// Comparison unification (`unify_many` for IN lists) keeps the int ->
    /// float widening: there the widened value only feeds a comparison, and
    /// a comparison's TYPE cannot leak.
    fn unify_result_arms(
        &mut self,
        operands: Vec<(BExpr, Ty)>,
        verb: &str,
    ) -> Result<(Vec<BExpr>, Ty)> {
        let mut kinds: Vec<ColumnType> = Vec::new();
        for (_, t) in &operands {
            if let Some(t) = *t {
                if !kinds.contains(&t) {
                    kinds.push(t);
                }
            }
        }
        if kinds.len() <= 1 {
            return self.unify_many(operands, verb);
        }
        let numeric = |t: &ColumnType| {
            matches!(t, ColumnType::Int64 | ColumnType::Float64 | ColumnType::Any)
        };
        if self.sqlite_dialect() && (kinds.iter().all(numeric) || kinds.contains(&ColumnType::Any))
        {
            // Mixed arms, typed per row. A bare parameter among them has
            // nothing to adopt (there is no one target type), and is left
            // for `resolve_params` to report — same as a mixed max()/min().
            return Ok((
                operands.into_iter().map(|(e, _)| e).collect(),
                Some(ColumnType::Any),
            ));
        }
        let names: Vec<String> = kinds.iter().map(|t| t.to_string()).collect();
        Err(bind_err(format!(
            "cannot {verb}: {} — sqlite would type this per row; \
             add an explicit CAST so every arm is one type",
            names.join(" and ")
        )))
    }

    fn unify_many(&mut self, operands: Vec<(BExpr, Ty)>, _verb: &str) -> Result<(Vec<BExpr>, Ty)> {
        // A dynamically-typed operand (`any` — a mixed CASE/COALESCE arm, a
        // host UDF result, a typeless column) unifies with the whole set the
        // way it unifies with one operand in `unify_operands`: nothing is
        // coerced, and the settled type is `any`. The runtime handles the
        // actual pairs — an IN membership runs each element through `sql_cmp`
        // (numeric comparison crosses int/float; a cross-CLASS pair is a
        // clean refusal, never a silent non-match). A bare parameter adopts
        // `any` (= any value accepted), exactly as it did before this rule
        // when EVERY operand was `any`.
        if operands.iter().any(|(_, t)| *t == Some(ColumnType::Any)) {
            let out = operands
                .into_iter()
                .map(|(e, t)| self.unify_param(e, t, ColumnType::Any).0)
                .collect();
            return Ok((out, Some(ColumnType::Any)));
        }
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
                // Mixed non-numeric classes (text vs int64, …): settle to `any`
                // so membership runs at runtime under class order / numeric
                // compare (sqlite). Django's injection probe is
                // `name IN (num_chairs + '…')` — text probe, int expression.
                Some(_) => ColumnType::Any,
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

    /// sqlite's TRUTHINESS: coerce a non-boolean value that stands in a
    /// **boolean context** (WHERE/HAVING/ON/FILTER, `NOT`, `AND`/`OR`,
    /// `CASE WHEN`, `CHECK`) into a bool. Django writes `WHERE "tbl"."flag"`
    /// for a `BooleanField` and binds `True` as the integer 1, so a rigid
    /// refusal here is the single largest sqlite-compat gap.
    ///
    /// The rule is taken from the sqlite binary (3.45.1), not from intuition.
    /// sqlite's `sqlite3VdbeBooleanValue` is: NULL stays unknown, an integer is
    /// `!= 0`, and **everything else is `sqlite3VdbeRealValue(x) != 0.0`** — the
    /// leading-float-prefix parse, applied to text AND to a blob's raw bytes.
    /// Verified against the binary in every boolean position:
    ///
    /// | value | truthy | why |
    /// |---|---|---|
    /// | `2`, `-1`, `0.5` | yes | non-zero |
    /// | `0`, `0.0`, `-0.0` | no | zero |
    /// | `'3abc'`, `'1e3'`, `'.5'`, `' 1 '` | yes | float prefix is non-zero |
    /// | `'abc'`, `'0'`, `'0abc'`, `'0x1'`, `''` | no | float prefix is 0.0 |
    /// | `x'31'` (`"1"`) | yes | blob bytes read as text |
    /// | `x'30'` (`"0"`), `x'00'`, `x''` | no | ditto |
    /// | `NULL` | unknown | 3VL |
    ///
    /// That is EXACTLY [`Affinity::Real`] as `Instr::Cast` already implements
    /// it (`to_real` -> `float_prefix`, itself differential-tested against
    /// sqlite in `crates/mpedb/tests/cast_affinity.rs`), so the whole rule
    /// desugars into instructions that already exist:
    ///
    /// * `int64`   -> `x <> 0`
    /// * `float64` -> `x <> 0.0`      (`-0.0 == 0.0` in `sql_cmp`, so it is FALSE)
    /// * anything else (text, blob, timestamp, `any`) -> `CAST(x AS REAL) <> 0.0`
    ///
    /// No new opcode, therefore **no `PLAN_FORMAT` bump**. `<>` is 3VL, so NULL
    /// propagates and every consumer (WHERE skips the row, `CASE WHEN` takes
    /// ELSE, `NOT NULL` is NULL, Kleene `AND`/`OR`) already behaves like sqlite.
    ///
    /// A bool or a still-unconstrained operand passes through untouched — this
    /// only ever ACCEPTS more, it never changes an answer mpedb already gives.
    /// Under the PostgreSQL dialect (`bare_group_by = "postgres"`) the rigid
    /// refusal is kept, exactly as [`like_glob_operand`] keeps it there.
    pub(crate) fn coerce_bool_ctx(&mut self, e: BExpr, t: Ty) -> Result<(BExpr, Ty)> {
        let src = match t {
            // Already boolean, or nothing to coerce (NULL literal / bare param).
            None | Some(ColumnType::Bool) => return Ok((e, t)),
            Some(src) => src,
        };
        if self.bare_group_by != BareGroupBy::Sqlite {
            return Ok((e, t)); // PostgreSQL: `WHERE 1` stays an error
        }
        let (probe, zero) = match src {
            ColumnType::Int64 => (e, Value::Int(0)),
            ColumnType::Float64 => (e, Value::Float(0.0)),
            // Fold the CAST first, so a constant boolean context (`WHERE 'abc'`)
            // reduces all the way to a Bool const and the planner can see it is
            // dead. `Affinity::Real` never errors, so folding it is always safe.
            _ => (
                fold_maybe(BExpr::Cast(Box::new(e), Affinity::Real), self.suppress_fold)?,
                Value::Float(0.0),
            ),
        };
        let e = fold_maybe(
            BExpr::Binary(BinOp::Ne, Box::new(probe), Box::new(BExpr::Const(zero))),
            self.suppress_fold,
        )?;
        Ok((e, Some(ColumnType::Bool)))
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

/// The SQL spelling of a bitwise operator, for its error messages.
fn bit_op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        _ => unreachable!("bit_op_name on {op:?}"),
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

/// Coerce a LIKE/GLOB operand to text the way sqlite does — the SUBJECT, and
/// since #74 (LIKE half) the non-literal PATTERN too (`op = "LIKE pattern"`
/// etc., so a refusal names the right half of the statement). sqlite applies
/// `sqlite3_value_text` to both `likeFunc` operands, so `12 LIKE '1%'` is
/// `'12' LIKE '1%'` and `'12' LIKE 12` is TRUE — a numeric operand is CAST to
/// text (the exact same conversion as `CAST(x AS TEXT)`, which is
/// sqlite-verified) rather than refused. Text stays as-is; a bare parameter
/// (`None`) has already been pinned to Text.
///
/// A statically-typed BLOB is refused by name — and deliberately so, because
/// there is no single "sqlite answer" to match: LIKE-on-blob is BUILD-
/// DEPENDENT. The bundled differential oracle (stock amalgamation defaults)
/// coerces blob bytes as text via `sqlite3_value_text`, while a CLI built
/// with `SQLITE_LIKE_DOESNT_MATCH_BLOBS` (Debian/Ubuntu's, e.g. the 3.45.1
/// on this machine's PATH) answers FALSE for a blob on EITHER side. A
/// runtime blob through an `any` column follows the ORACLE (the repo's
/// acceptance baseline): the CAST bridge reinterprets its bytes as text, and
/// refuses by name the non-UTF-8 bytes a Rust `String` cannot hold.
///
/// `coerce` follows the compat dialect: `true` (sqlite) casts a non-text
/// operand to text; `false` (PostgreSQL) refuses it with mpedb's original
/// rigid error, so `id LIKE '1%'` on an integer column is a bind error rather
/// than a silent stringify. Text and blob handling are identical in both
/// dialects.
fn like_glob_operand(l: BExpr, lt: Option<ColumnType>, op: &str, coerce: bool) -> Result<BExpr> {
    match lt {
        None | Some(ColumnType::Text) => Ok(l),
        Some(ColumnType::Blob) => Err(bind_err(format!("{op} requires text, got blob"))),
        Some(_) if coerce => Ok(BExpr::Cast(Box::new(l), Affinity::Text)),
        Some(t) => Err(bind_err(format!("{op} requires text, got {t}"))),
    }
}

/// Constant-fold one node whose children are already folded: if every child
/// is a constant, evaluate now (via the same IR evaluator used at run time,
/// so semantics — including division-by-zero errors — match exactly).
/// The storage type a bare `CAST(? AS t)` parameter is pinned to. sqlite's
/// affinities map onto one mpedb type each — except NUMERIC, whose runtime type
/// is decided per value, so a NUMERIC-cast parameter stays unpinned (`None`).
fn affinity_pin_type(aff: Affinity) -> Option<ColumnType> {
    Some(match aff {
        Affinity::Integer => ColumnType::Int64,
        Affinity::Real => ColumnType::Float64,
        Affinity::Text => ColumnType::Text,
        Affinity::Blob => ColumnType::Blob,
        Affinity::Numeric => return None,
    })
}

/// The bind-time result type of a non-constant `CAST` to `aff` over a source of
/// type `src`. INTEGER/REAL/TEXT/BLOB are fixed. NUMERIC is the subtle one: an
/// int/real/bool/timestamp source keeps a concrete numeric type (the runtime
/// value is guaranteed to match), but a text/blob source can yield either an
/// int or a real per value, so it is `Any` (mpedb's per-value-typed scalar).
fn cast_result_type(aff: Affinity, src: Ty) -> Ty {
    use ColumnType as T;
    Some(match aff {
        Affinity::Integer => T::Int64,
        Affinity::Real => T::Float64,
        Affinity::Text => T::Text,
        Affinity::Blob => T::Blob,
        Affinity::Numeric => match src {
            Some(T::Int64) | Some(T::Bool) | Some(T::Timestamp) => T::Int64,
            Some(T::Float64) => T::Float64,
            // text, blob, or an already-`Any` source → per-value at runtime.
            Some(T::Text) | Some(T::Blob) | Some(T::Any) => T::Any,
            // NULL / untyped-parameter source: no static type.
            None => return None,
        },
    })
}

/// Resolve an ORDER-BY collation NAME to a built-in, or — when the compiling
/// connection registered one under that name — a HOST collation
/// (design/DESIGN-UDF.md stage 3). An unknown name is still the clean bind
/// error, so a typo is caught here rather than at sort time.
///
/// A built-in always wins, exactly as it does for a function name: no
/// registration can redefine BINARY/NOCASE/RTRIM.
pub(crate) fn resolve_order_collation(name: &str, host: &[String]) -> Result<mpedb_types::OrderColl> {
    if let Some(c) = Collation::parse(name) {
        return Ok(mpedb_types::OrderColl::Native(c));
    }
    if let Some(h) = host.iter().find(|h| h.eq_ignore_ascii_case(name)) {
        return Ok(mpedb_types::OrderColl::Host(h.clone()));
    }
    Err(bind_err(format!("no such collation sequence: {name}")))
}

/// [`peel_collate`] for an ORDER BY key, where a HOST collation is legal.
/// Chained `COLLATE`s resolve to the outermost, as there; the shadowed names
/// are still validated (against the built-ins AND the host registrations).
pub(crate) fn peel_order_collate<'a>(
    e: &'a ast::Expr,
    host: &[String],
) -> Result<(&'a ast::Expr, Option<mpedb_types::OrderColl>)> {
    let ast::Expr::Collate(inner, name) = e else {
        return Ok((e, None));
    };
    let coll = resolve_order_collation(name, host)?;
    let mut cur: &ast::Expr = inner;
    while let ast::Expr::Collate(next, n) = cur {
        resolve_order_collation(n, host)?;
        cur = next;
    }
    Ok((cur, Some(coll)))
}

/// Resolve a collation NAME (as written after `COLLATE`) to a built-in, or a
/// clean bind error naming the unsupported collation.
pub(crate) fn resolve_collation(name: &str) -> Result<Collation> {
    Collation::parse(name).ok_or_else(|| bind_err(format!("no such collation sequence: {name}")))
}

/// Peel a top-level explicit `COLLATE` off an AST expression, returning the
/// inner expression and its resolved collation (`None` when there is no
/// `COLLATE`). Chained `COLLATE`s (`x COLLATE A COLLATE B`) resolve to the
/// OUTERMOST — the last one written — matching sqlite; the shadowed inner names
/// are still validated. Any `COLLATE` nested DEEPER than the peeled operand is
/// left in `inner` for [`Binder::bind_expr`] to refuse, so it can never be
/// silently dropped.
pub(crate) fn peel_collate(e: &ast::Expr) -> Result<(&ast::Expr, Option<Collation>)> {
    let ast::Expr::Collate(inner, name) = e else {
        return Ok((e, None));
    };
    let coll = resolve_collation(name)?;
    let mut cur = inner.as_ref();
    while let ast::Expr::Collate(next, n) = cur {
        resolve_collation(n)?;
        cur = next.as_ref();
    }
    Ok((cur, Some(coll)))
}

/// The collation an ORDER BY / comparison key gets from the COLUMN it names,
/// when no explicit `COLLATE` overrides — sqlite's precedence rung 2 ("if either
/// operand is a column, use that column's declared collation"). Returns
/// [`Collation::Binary`] when the key is not a bare column reference (an ordinal,
/// an expression, a literal): those carry no column collation, exactly as in
/// sqlite. A `+col` is NOT treated as a column here (rare; falls back to BINARY,
/// which only differs from sqlite for a `+`-prefixed collated column in ORDER BY).
pub(crate) fn declared_collation(key: &ast::Expr, scope: &Scope) -> Collation {
    let slot = match key {
        ast::Expr::Col(n) => scope.resolve(n).ok().map(|(i, _)| i),
        ast::Expr::Qualified(q, n) => scope.resolve_qualified(q, n).ok().map(|(i, _)| i),
        _ => None,
    };
    slot.map(|s| scope.column_collation(s)).unwrap_or(Collation::Binary)
}

/// Is this argument the LITERAL time string `'now'`?
///
/// sqlite's `isDate` compares case-insensitively after skipping leading
/// whitespace, and its own tokenizer has already stripped the quotes — so
/// `' NOW '` is `'now'` there and here. Only a bind-time literal qualifies: a
/// column or parameter whose VALUE happens to be `now` cannot be rewritten into
/// the statement-instant slot and stays refused at runtime (see
/// `mpedb_types::expr::datetime`), because resolving it would need a clock read
/// per row and would drift within one statement.
fn is_literal_now(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Lit(Value::Text(s)) if s.trim().eq_ignore_ascii_case("now"))
}

/// Map one of the six comparison `BinOp`s to its collated-instruction kind.
fn cmp_kind(op: BinOp) -> CmpKind {
    match op {
        BinOp::Eq => CmpKind::Eq,
        BinOp::Ne => CmpKind::Ne,
        BinOp::Lt => CmpKind::Lt,
        BinOp::Le => CmpKind::Le,
        BinOp::Gt => CmpKind::Gt,
        BinOp::Ge => CmpKind::Ge,
        _ => unreachable!("cmp_kind is only reached for comparison operators"),
    }
}

pub(crate) fn fold(e: BExpr) -> Result<BExpr> {
    let foldable = match &e {
        BExpr::Unary(_, a) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Binary(_, a, b) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::IsDistinct(a, b, _) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        // `'ABC' = 'abc' COLLATE NOCASE` folds to a constant like any other
        // all-const comparison — compile_program emits the CmpColl and eval
        // applies the collation.
        BExpr::CollateCmp(_, a, b, _) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::InListColl(..) => false,
        BExpr::Like(a, _, _, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Glob(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Regexp(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Cast(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        // Never foldable: the list is a session value, not a literal.
        BExpr::InParam(..) => false,
        // A CASE is branching control flow, not a value-in/value-out node; the
        // fold path evaluates whole programs and has no business here.
        BExpr::Case(..) => false,
        BExpr::Coalesce(..) => false,
        // NEVER folded, and one of the two reasons is load-bearing rather than
        // an economy:
        //
        //  1. **Determinism (the gate).** A compiled plan is CONTENT-HASHED and
        //     published to a registry SHARED ACROSS PROCESSES. Folding a call
        //     whose arguments carry the statement instant would bake a
        //     COMPILE-TIME clock reading into plan bytes that every later
        //     process reuses — a wrong answer that outlives the process that
        //     made it, in a shared file. [`Binder::statement_instant`] already
        //     makes that structurally impossible (the instant is a `Param`, and
        //     a `Param` is not a `Const`, so no `Call` reading it could ever
        //     satisfy an all-const test), but the rule is stated HERE, at the
        //     gate, so that a future "fold all-const calls" optimisation has to
        //     read it before it can be written: **a call is foldable only if
        //     every argument is a `Const`, which the statement instant can never
        //     be.**
        //  2. Economy: folding would have to reproduce `call_scalar`'s NULL
        //     rules here, which is not worth a special case.
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
                BUnOp::BitNot => Instr::BitNot,
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
                // `->`/`->>` are bound to `BExpr::Call`, never to a binary
                // node, so no opcode exists (or is needed) for them here.
                BinOp::JsonArrow | BinOp::JsonArrowText => {
                    return Err(bind_err(
                        "internal: a JSON accessor reached the binary emitter",
                    ))
                }
                BinOp::BitAnd => Instr::BitAnd,
                BinOp::BitOr => Instr::BitOr,
                BinOp::Shl => Instr::Shl,
                BinOp::Shr => Instr::Shr,
            });
        }
        BExpr::IsDistinct(a, b, negated) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(if *negated {
                Instr::IsDistinct
            } else {
                Instr::IsNotDistinct
            });
        }
        BExpr::Like(a, pattern, case_insensitive, escape) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            // The dialect chose case-(in)sensitivity at bind time; emit the
            // matching opcode so the plan is self-describing. The ESCAPE
            // character rides in the const pool as a one-character text.
            instrs.push(match escape {
                None if *case_insensitive => Instr::Like(idx),
                None => Instr::LikeCs(idx),
                Some(c) => {
                    let e = push_const(consts, Value::Text(c.to_string()))?;
                    if *case_insensitive {
                        Instr::LikeEsc(idx, e)
                    } else {
                        Instr::LikeCsEsc(idx, e)
                    }
                }
            });
        }
        // The dyn-pattern forms: subject first, pattern on top (popped first).
        // Dialect and escape-ness still select the opcode — the escape rides
        // the const pool exactly as in the literal form; only the pattern is
        // on the stack.
        BExpr::LikeDyn(a, p, case_insensitive, escape) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(match escape {
                None if *case_insensitive => Instr::LikeDyn,
                None => Instr::LikeCsDyn,
                Some(c) => {
                    let e = push_const(consts, Value::Text(c.to_string()))?;
                    if *case_insensitive {
                        Instr::LikeDynEsc(e)
                    } else {
                        Instr::LikeCsDynEsc(e)
                    }
                }
            });
        }
        BExpr::Glob(a, pattern) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            instrs.push(Instr::Glob(idx));
        }
        BExpr::GlobDyn(a, p) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(Instr::GlobDyn);
        }
        BExpr::RegexpDyn(a, p) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(Instr::RegexpDyn);
        }
        BExpr::Regexp(a, pattern) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            instrs.push(Instr::Regexp(idx));
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
        BExpr::CollateCmp(op, a, b, coll) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(Instr::CmpColl(cmp_kind(*op), *coll));
        }
        // Comparison affinity is applied to BOTH operands (sqlite's `OP_Lt`
        // family does exactly that, and applying it to a value that already
        // has the target class is a no-op), then they are compared by class.
        // `Blob` is sqlite's NONE — nothing to apply, so nothing is emitted.
        BExpr::ClassCmp(op, a, b, coll, aff) => {
            emit(a, instrs, consts)?;
            if *aff != Affinity::Blob {
                instrs.push(Instr::Affinity(*aff));
            }
            emit(b, instrs, consts)?;
            if *aff != Affinity::Blob {
                instrs.push(Instr::Affinity(*aff));
            }
            instrs.push(Instr::CmpClass(cmp_kind(*op), *coll));
        }
        BExpr::InListColl(a, items, coll) => {
            emit(a, instrs, consts)?;
            for it in items {
                emit(it, instrs, consts)?;
            }
            instrs.push(Instr::InListColl(items.len() as u16, *coll));
        }
        BExpr::HostCall { name, args } => {
            // The NAME rides the const pool (a plan stores the name + arity, not
            // the closure); the arguments are pushed left-to-right, then the
            // opcode pops `argc` and leaves the one result.
            let name_idx = push_const(consts, Value::Text(name.clone()))?;
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::HostCall(name_idx, args.len() as u16));
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
        let col = |name: &str, ty: ColumnType, nullable: bool| ColumnDef { generated: None, decl: None,
            name: name.into(),
            ty,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None, collation: Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ty),
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
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
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
            "id + 'x'",
            "name + name",
            "created = 1",
            "data = 'x'",
            "-name",
            // (`name LIKE 1` used to sit here; a constant numeric pattern now
            // binds under the sqlite dialect and coerces at runtime — sqlite's
            // likeFunc rule, #74 item 3. The PG dialect still refuses it; see
            // `like_pattern_dyn_binds_and_blob_refuses_by_name`.)
            // Arithmetic on a bool is still rigid — the int/bool bridge is a
            // COMPARISON/assignment rule, never a general interchange.
            "active + 1",
        ] {
            assert!(
                matches!(bind(src, 0), Err(Error::Bind(_))),
                "expected bind error for {src}"
            );
        }
        // Formerly rigid, now sqlite-compatible (Django gap #5). `active` is a
        // bool column, `id` an int64 one.
        for src in ["active = 1", "active = 0", "NOT id", "id AND active"] {
            assert!(bind(src, 0).is_ok(), "expected {src} to bind");
        }
        // `active = 1` keeps the plain `Binary(Eq, Col, Const)` shape — the int
        // literal folds into the bool domain rather than casting the column, so
        // an index probe on the column survives.
        let (e, ty, _) = bind("active = 1", 0).unwrap();
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Eq,
                Box::new(BExpr::Col(3)),
                Box::new(BExpr::Const(Value::Bool(true))),
            )
        );
        // A non-0/1 integer casts the BOOL side up instead, so `active = 2` is
        // FALSE (sqlite's answer) rather than TRUE.
        let (e, _, _) = bind("active = 2", 0).unwrap();
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Eq,
                Box::new(BExpr::Cast(Box::new(BExpr::Col(3)), Affinity::Integer)),
                Box::new(BExpr::Const(Value::Int(2))),
            )
        );
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
    fn fold_matches_the_runtime_semantics() {
        // Division / modulo by zero folds to NULL (sqlite semantics), exactly
        // as the runtime `/` and `%` operators evaluate them.
        assert_eq!(bind("1 / 0", 0).unwrap().0, BExpr::Const(Value::Null));
        assert_eq!(bind("1 % 0", 0).unwrap().0, BExpr::Const(Value::Null));
        // Overflow, however, still raises at fold time as it does at runtime.
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
    fn like_pattern_dyn_binds_and_blob_refuses_by_name() {
        // #74 item 3, LIKE half: a bound / column / computed pattern BINDS —
        // the old "must be a literal" refusal was structural, exactly as it
        // was for REGEXP. A bare parameter is pinned to text.
        let (e, ty, params) = bind("name LIKE $1", 1).unwrap();
        assert!(matches!(e, BExpr::LikeDyn(..)), "{e:?}");
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(params[0], Some(ColumnType::Text));
        // A per-row COLUMN pattern is legal (sqlite evaluates it per row).
        assert!(matches!(bind("name LIKE name", 0), Ok((BExpr::LikeDyn(..), _, _))));
        // GLOB closed the same way.
        assert!(matches!(bind("name GLOB $1", 1), Ok((BExpr::GlobDyn(..), _, _))));
        // A text LITERAL keeps the const-pool node — its plan bytes are the
        // pre-#74 ones.
        assert!(matches!(bind("name LIKE 'a%'", 0), Ok((BExpr::Like(..), _, _))));
        // A statically-BLOB pattern is refused by name, naming the PATTERN
        // half of the statement (`data` is the blob column).
        match bind("name LIKE data", 0) {
            Err(Error::Bind(m)) => assert!(m.contains("LIKE pattern"), "{m}"),
            other => panic!("expected bind error, got {other:?}"),
        }
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
        // A non-boolean predicate is truthy-tested like sqlite, not refused:
        // `WHERE 42` desugars to `42 <> 0` and folds to TRUE.
        let (ast, _) = parse_expr_only("42").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(true)));
        let (ast, _) = parse_expr_only("0").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(false)));
        // A text predicate takes the CAST-to-REAL path (sqlite's RealValue).
        let (ast, _) = parse_expr_only("'3abc'").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(true)));
        let (ast, _) = parse_expr_only("'abc'").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(false)));
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

    /// The constant-folding / laziness boundary. The raising case that a dead
    /// branch must NOT evaluate is arithmetic overflow (mpedb raises it where
    /// sqlite wraps); `OVF` below is `9223372036854775807 + 1`. Division by
    /// zero is deliberately NOT a raise — mpedb folds `1/0` to NULL like
    /// sqlite — so it doubles here as the positive control:
    ///
    ///   never fold a live raise -> `SELECT OVF` would prepare clean and fail
    ///     at every execute. PG raises at PLAN time.
    ///   always fold every branch -> `coalesce(1, OVF)` dies, though both
    ///     sqlite and PG answer 1.
    ///
    /// The rule is neither: fold the CONTROL FLOW first and drop the
    /// unreachable branch WITHOUT evaluating it; fold what survives, and let
    /// that raise.
    #[test]
    fn folding_drops_dead_branches_before_it_can_raise_on_them() {
        const OVF: &str = "9223372036854775807 + 1";
        // arg0 is a non-NULL constant -> the whole coalesce IS it, and the
        // overflow is never folded. PG: 1.
        assert_eq!(
            bind_ok(&format!("coalesce(1, {OVF})")).0,
            BExpr::Const(Value::Int(1))
        );
        // arg0 is a NULL constant -> dropped; the overflow becomes reachable
        // -> raises. PG: ERROR.
        assert!(matches!(
            bind_expr_res(&format!("coalesce(NULL, {OVF})")),
            Err(Error::ArithmeticOverflow)
        ));
        // Same rule through CASE.
        assert_eq!(
            bind_ok(&format!("CASE WHEN true THEN 1 ELSE {OVF} END")).0,
            BExpr::Const(Value::Int(1))
        );
        assert!(matches!(
            bind_expr_res(&format!("CASE WHEN false THEN 1 ELSE {OVF} END")),
            Err(Error::ArithmeticOverflow)
        ));
        // Division by zero, in contrast, is NULL, never a raise — even when
        // reachable. `coalesce(NULL, 1/0)` reduces to `coalesce(1/0)` = NULL.
        assert_eq!(bind_ok("coalesce(1, 1/0)").0, BExpr::Const(Value::Int(1)));
        assert_eq!(
            bind_expr_res("coalesce(NULL, 1/0)").unwrap().0,
            BExpr::Const(Value::Null)
        );
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
        // A non-numeric mix stays refused (sqlite would rank number-vs-text
        // by storage class downstream, which mpedb refuses everywhere), and
        // the message names both types and the CAST fix.
        let msg = bind_err_msg("coalesce(id, 'x')");
        assert!(msg.contains("coalesce") && msg.contains("CAST"), "{msg}");
        assert!(bind_err_msg("coalesce(name, active)").contains("CAST"));
        // Explicitly casting every arm to one type still yields that type.
        let (_, ty) = bind_ok("coalesce(CAST(id AS REAL), 1.5)");
        assert_eq!(ty, Some(ColumnType::Float64));
    }

    /// int64 ∪ float64 RESULT arms: sqlite types the winning arm per ROW
    /// (`COALESCE(30, avg(x))` is the INTEGER 30 when arm 1 wins), so no arm
    /// is coerced — each keeps its own type and value, and the expression
    /// types as `any`, decided per value at runtime. Widening instead was
    /// measured at 82 wrong answers in the sqllogictest expr corpus.
    #[test]
    fn mixed_numeric_result_arms_type_as_any() {
        // Constant COALESCE folds to the winning arm UNWIDENED: the integer 30.
        let (e, ty) = bind_ok("coalesce(30, 1.5)");
        assert_eq!(e, BExpr::Const(Value::Int(30)));
        assert_eq!(ty, Some(ColumnType::Any));
        // Non-constant arms stay control flow, typed any.
        let (_, ty) = bind_ok("coalesce(score, 1)");
        assert_eq!(ty, Some(ColumnType::Any));
        // Same rule through CASE (and its sugar iif); the winning constant
        // arm keeps its own type: sqlite answers 1, not 1.0.
        let (e, ty) = bind_ok("CASE WHEN true THEN 1 ELSE 2.5 END");
        assert_eq!(e, BExpr::Const(Value::Int(1)));
        assert_eq!(ty, Some(ColumnType::Any));
        let (_, ty) = bind_ok("CASE WHEN active THEN 1 ELSE 2.5 END");
        assert_eq!(ty, Some(ColumnType::Any));
        let (_, ty) = bind_ok("iif(active, 1, 2.5)");
        assert_eq!(ty, Some(ColumnType::Any));
        // An `any` arm (here a NUMERIC cast of text) mixes with anything.
        let (_, ty) = bind_ok("coalesce(CAST(name AS NUMERIC), name)");
        assert_eq!(ty, Some(ColumnType::Any));
    }

    /// The per-row rule is sqlite's; PostgreSQL PROMOTES the arms statically
    /// (`COALESCE(30, 1.5) / 35` is numeric division ≈0.857 in PG, integer
    /// division 0 per-row), so under the postgres dialect the mix stays the
    /// original rigid refusal — a clean error, never either engine's answer.
    #[test]
    fn mixed_arms_stay_refused_under_postgres_dialect() {
        let t = table();
        let (e, n) = parse_expr_only("coalesce(30, 1.5)").unwrap();
        let mut b = Binder::new(&t, n, true);
        b.set_dialect(BareGroupBy::Postgres);
        let err = format!("{}", b.bind_expr(&e).unwrap_err());
        assert!(err.contains("CAST"), "{err}");
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
            columns: vec![ColumnDef { generated: None, decl: None,
                name: "tag".into(),
                ty: ColumnType::Text,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: mpedb_types::Affinity::implied_by(ColumnType::Text),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
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
            columns: vec![ColumnDef { generated: None, decl: None,
                name: "id".into(), // collides with a.id
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: mpedb_types::Affinity::implied_by(ColumnType::Int64),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
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
