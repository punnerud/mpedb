//! Untyped AST produced by the parser and consumed by the binder.
//! Carries no source text except literal values and identifiers, so plans
//! built from it are automatically whitespace/keyword-case canonical.

use crate::plan::SetOp;
use mpedb_types::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Select(SelectStmt),
    /// `SELECT … UNION/EXCEPT/INTERSECT SELECT …` — set-operator chain.
    Compound(CompoundStmt),
    /// `WITH RECURSIVE t(cols) AS (<anchor> UNION[ ALL] <recursive>) <outer>`
    /// (design/DESIGN-CTE-RECURSIVE.md). A fixpoint, planned to
    /// `PlanStmt::RecursiveCte` — NOT flattened like a non-recursive CTE.
    RecursiveCte(RecursiveCteStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT <name>` — open a named savepoint on the current session's
    /// stack (transaction-control, handled by the write session, not compiled
    /// to an access path).
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] <name>` — merge `<name>` and everything above it
    /// into the enclosing savepoint/transaction.
    Release(String),
    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] <name>` — undo changes since
    /// `<name>` was established, keeping `<name>` on the stack.
    RollbackTo(String),
}

/// A `WITH RECURSIVE` statement (stage 1: a single recursive CTE, a single
/// anchor and a single recursive term). The anchor and recursive term are
/// captured as ordinary SELECTs; the outer statement follows the CTE body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecursiveCteStmt {
    pub name: String,
    /// The REQUIRED column list `t(c1, …)`.
    pub columns: Vec<String>,
    /// `UNION ALL` vs `UNION` between the anchor and the recursive term.
    pub union_all: bool,
    /// Non-recursive seed; must not reference the CTE.
    pub anchor: Box<SelectStmt>,
    /// Recursive term; references the CTE exactly once in a FROM/JOIN operand.
    pub recursive: Box<SelectStmt>,
    /// The outer statement (stage 1: a plain SELECT over the CTE).
    pub outer: Box<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectStmt {
    /// `None` = FROM-less (`SELECT 3+5`): the statement reads no table and
    /// evaluates its items over ONE synthetic empty row (sqlite/PG semantics).
    /// `joins` is empty whenever this is `None` — the parser cannot produce a
    /// join without a FROM.
    pub table: Option<String>,
    /// `FROM (SELECT …) [AS] alias` — a derived table (#74). Mutually exclusive
    /// with `table` at parse time; the view-inline pass flattens it onto the
    /// subquery's base table (merging WHERE, stripping the alias qualifier) and
    /// clears this back to `None` BEFORE planning, so the planner and executor
    /// never see it. Only simple projection/filter subquery bodies are
    /// flattenable; anything else is refused (design/DESIGN-DERIVED-TABLES.md, Stage B).
    pub from_derived: Option<Box<SelectStmt>>,
    /// `FROM t [AS] a` — the name `t`'s columns are addressed by. When present,
    /// the table's own name is NOT in scope (`FROM orders o` makes `orders.c`
    /// invalid and `o.c` valid — PG's rule), and it is what lets a table join
    /// itself. Purely a bind-time name: the compiled plan references columns by
    /// slot, so an alias never reaches the plan bytes.
    pub alias: Option<String>,
    /// `INNER JOIN <table> ON <cond>` chain. Empty = single table. Left-deep:
    /// join `k`'s ON may reference any table at or left of `k`.
    pub joins: Vec<JoinClause>,
    /// `SELECT DISTINCT` — deduplicate the OUTPUT rows (the projected tuple),
    /// which is why it cannot be pushed into the scan.
    pub distinct: bool,
    /// `None` = `SELECT *`. Each item is the expression and its optional
    /// alias (`expr [AS] name`) — the alias only names the output column.
    pub items: Option<Vec<(Expr, Option<String>)>>,
    pub where_clause: Option<Expr>,
    /// `GROUP BY` keys. Expressions rather than names because `GROUP BY t.col`
    /// is legal in sqlite and PG, and a qualified name is not a name — the
    /// planner still requires each key to BE a column.
    pub group_by: Vec<Expr>,
    /// `HAVING` — a predicate over the GROUPED row, not the base row.
    pub having: Option<Expr>,
    /// `ORDER BY <expr> [ASC|DESC]`, and whether it descends. An expression rather than a name because
    /// `ORDER BY count(*)` is legal in both sqlite and PG, and an aggregate is
    /// not a name. The planner still requires each item to REDUCE to a column
    /// of the output tuple — sorting by a computed expression is rejected, not
    /// silently mis-sorted.
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// A compound SELECT. `arms[0] ops[0] arms[1] …`, left-associative (sqlite's
/// precedence — the corpus' expected results assume it). The trailing
/// ORDER BY / LIMIT / OFFSET belong to the WHOLE compound; the parser rejects
/// them on any arm but the last, and hoists the last arm's out to here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompoundStmt {
    pub arms: Vec<SelectStmt>,
    /// `ops.len() == arms.len() - 1`.
    pub ops: Vec<SetOp>,
    /// Over the compound OUTPUT: an ordinal or a first-arm output name.
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertStmt {
    pub table: String,
    /// Explicit column list, if given.
    pub columns: Option<Vec<String>>,
    /// `VALUES` rows. Empty exactly when `select` is `Some` (the two forms are
    /// mutually exclusive).
    pub rows: Vec<Vec<Expr>>,
    /// `INSERT INTO t [(cols)] SELECT …` — the source query. Its output tuple
    /// (one per produced row) fills the listed columns in order.
    pub select: Option<Box<SelectStmt>>,
    pub on_conflict: OnConflict,
    /// `RETURNING` items; `Some(None)` = `RETURNING *`.
    pub returning: Option<Option<Vec<Expr>>>,
}

/// `ON CONFLICT` action.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OnConflict {
    /// No clause: a conflict is an error.
    Error,
    DoNothing,
    /// `INSERT OR REPLACE` — replace the conflicting row. The planner desugars
    /// it to `ON CONFLICT (<pk>) DO UPDATE SET <every non-pk col> = excluded`,
    /// and refuses a table with a secondary UNIQUE index (where sqlite's
    /// delete-on-any-unique-constraint semantics would diverge from a plain
    /// PK-keyed upsert).
    Replace,
    /// `ON CONFLICT (<target>) DO UPDATE SET … [WHERE …]`.
    DoUpdate {
        target: Vec<String>,
        set: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpdateStmt {
    pub table: String,
    pub set: Vec<(String, Expr)>,
    pub where_clause: Option<Expr>,
    pub returning: Option<Option<Vec<Expr>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeleteStmt {
    pub table: String,
    pub where_clause: Option<Expr>,
    pub returning: Option<Option<Vec<Expr>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    /// `||` — SQL concatenation.
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
}

/// What a missing inner match means for one join step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    /// `LEFT [OUTER] JOIN`: no match → one NULL-extended row.
    Left,
    /// `RIGHT [OUTER] JOIN` — planned as a LEFT with the sides swapped (and
    /// the projection remapped); never reaches the plan bytes.
    Right,
    /// `FULL [OUTER] JOIN`: unmatched rows on BOTH sides NULL-extend.
    Full,
}

/// `[NATURAL] [INNER | LEFT [OUTER]] JOIN <table> (ON <cond> | USING (c1, …))`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinClause {
    pub table: String,
    pub alias: Option<String>,
    pub kind: JoinKind,
    /// The ON condition. For a `USING`/`NATURAL` join this is the literal `true`
    /// at parse time and `using` carries the columns — the planner desugars it to
    /// `left.ci = right.ci AND …` there, because qualifying the LEFT side needs
    /// the schema (the column may live in any table left of this one). An empty
    /// `using` is a plain `ON` join.
    pub on: Expr,
    /// `JOIN … USING (c1, c2, …)` columns, in written order. Non-empty only for
    /// the USING form (or a NATURAL join whose common set is non-empty, filled at
    /// plan time). Two things follow from it, both at plan time: the ON
    /// equalities above, and — under `SELECT *` — the join columns are COALESCED
    /// (each appears once, from the left side) instead of once per side.
    pub using: Vec<String>,
    /// `NATURAL JOIN` — an implicit `USING` over ALL columns common to the two
    /// sides. The common set is a fact about the schema (rigid schema ⇒ static),
    /// but not known at parse time; the planner computes it and fills `using`
    /// before the USING→ON desugar, so a natural join is handled exactly like an
    /// explicit USING one. A natural join with NO common column keeps `using`
    /// empty and its `ON true`, i.e. a cross join (sqlite's rule).
    pub natural: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Lit(Value),
    /// 0-based parameter index.
    Param(u16),
    Col(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `IS NULL` (`negated` = `IS NOT NULL`).
    IsNull(Box<Expr>, bool),
    /// `x IS y` / `x IS NOT y` — NULL-safe "(not) distinct from". Unlike `=`/`<>`
    /// this is 2-valued (never NULL): `a IS b` is TRUE when both are NULL, FALSE
    /// when exactly one is, else `a = b`. `negated = false` is `IS`
    /// (is-not-distinct-from); `negated = true` is `IS NOT` (is-distinct-from).
    /// The `IS [NOT] NULL` forms stay [`Expr::IsNull`].
    IsDistinct(Box<Expr>, Box<Expr>, bool),
    /// `lhs LIKE pattern`.
    Like(Box<Expr>, Box<Expr>),
    /// `<col-or-table> MATCH <literal>` — FTS5 full-text search
    /// (design/DESIGN-FTS.md §3). Unlike LIKE/GLOB, MATCH is NOT a boolean
    /// expression: it is usable ONLY as a top-level WHERE conjunct against an
    /// FTS table, compiling to an `FtsScan` access path. Anywhere else — a scalar
    /// context, a non-FTS column, a SELECT-list item — it is an ERROR (identical
    /// to sqlite's "unable to use function MATCH in the requested context"),
    /// enforced by the binder. There is no `NOT MATCH` in sqlite.
    Match(Box<Expr>, Box<Expr>),
    /// `lhs [NOT] GLOB pattern` — sqlite's case-SENSITIVE `*`/`?`/`[...]`
    /// matcher. Carries `negated` (`NOT GLOB`), unlike [`Expr::Like`]: the
    /// bool is the whole difference from that node's shape.
    Glob(Box<Expr>, Box<Expr>, bool),
    /// `lhs [NOT] REGEXP pattern` — sqlite's `ext/misc/regexp.c` matcher (POSIX
    /// -ish: `.`, `* + ?`, `{p,q}`, `[...]`, `^`/`$`, `|`, `(...)`, `\d`/`\b`,
    /// escapes), case-SENSITIVE, unanchored. Same shape as [`Expr::Glob`]:
    /// `negated` carries `NOT REGEXP`.
    Regexp(Box<Expr>, Box<Expr>, bool),
    /// `CAST(x AS <type>)` — the raw type name is kept verbatim (any identifier
    /// is accepted, sqlite-style); the binder folds it to one of five
    /// [`mpedb_types::Affinity`]s. A multi-word name (`DOUBLE PRECISION`) is
    /// joined with single spaces; a parenthesized size (`VARCHAR(10)`) is
    /// dropped (it never affects affinity).
    Cast(Box<Expr>, String),
    /// `<expr> COLLATE <name>` — a postfix collation annotation (task: COLLATE).
    /// Binds tighter than any comparison. It carries the collation NAME as
    /// written (validated to a built-in at bind time); the binder honors it only
    /// as a direct comparison operand or ORDER BY term, and refuses it anywhere
    /// else — a `COLLATE` that cannot change a comparison or a sort is an error
    /// rather than a silently-ignored no-op.
    Collate(Box<Expr>, String),
    /// `(SELECT …)` — a scalar subquery: one output column; 0 rows = NULL,
    /// more than one row is a runtime error (PostgreSQL's rule — sqlite
    /// silently takes the first row). The planner lifts it out into the
    /// plan's subplan table and replaces this node with a reserved parameter.
    Subquery(Box<SelectStmt>),
    /// `[NOT] EXISTS (SELECT …)` — did the subquery produce any row. The
    /// bool is `negated`.
    Exists(Box<SelectStmt>, bool),
    /// `x [NOT] IN (SELECT …)` (#70) — membership in the subquery's single
    /// output column. The planner lifts the subquery into a LIST-kind
    /// subplan and rewrites this into [`Expr::InParamSlot`]. Uncorrelated
    /// only; the bool is `negated`.
    InSubquery(Box<Expr>, Box<SelectStmt>, bool),
    /// INTERNAL: produced only by the subquery lift — `lhs IN (<list at
    /// reserved param slot>)`, bound to the `InParam` membership
    /// instruction. Never comes out of the parser.
    InParamSlot(Box<Expr>, u16, bool),
    /// `current_setting('key')` — a session-context value, bound to a reserved
    /// parameter filled from the caller's [`Session`](mpedb) at execute time
    /// (design/DESIGN-MULTIDB.md §2.1). The value never enters the plan bytes, so one
    /// content-hashed plan serves every session.
    ContextRef(String),
    /// `<expr> IN (current_setting('key'))` — membership in a session-context
    /// list (design/DESIGN-MULTIDB.md §2.6). The key binds to ONE reserved param
    /// holding a [`mpedb_types::Value::List`], so the arity of the caller's
    /// membership set never reaches the plan bytes. The bool is `negated`.
    ///
    /// Deliberately its own node rather than `Binary(In, e, ContextRef)`: the
    /// right-hand side is not an expression that evaluates to a value on the
    /// stack, it is a param slot the InParam instruction reads directly.
    InContext(Box<Expr>, String, bool),
    /// `<expr> IN (e1, …, en)` / `NOT IN` — general SQL membership (task #21).
    /// The bool is `negated`.
    InList(Box<Expr>, Vec<Expr>, bool),
    /// `CASE WHEN c THEN r … [ELSE e] END` — the searched form. The simple
    /// form (`CASE x WHEN a …`) is desugared into this by the parser.
    /// `else_` is None for a missing ELSE, which SQL defines as NULL.
    Case(Vec<(Expr, Expr)>, Option<Box<Expr>>),
    /// A scalar function call. `coalesce`/`ifnull`/`nullif` never appear here:
    /// they must NOT propagate NULL, so the binder compiles them to control
    /// flow instead of a call.
    Func(String, Vec<Expr>),
    /// `coalesce(a, b, …)` — first non-NULL argument, evaluated lazily.
    Coalesce(Vec<Expr>),
    /// `excluded.<col>` — the proposed row inside `ON CONFLICT DO UPDATE`.
    Excluded(String),
    /// `<table>.<col>` — a table-qualified column reference. Kept distinct from
    /// [`Expr::Col`] so the binder can check the qualifier actually names the
    /// table in scope, rather than silently accepting `nonsense.id`.
    Qualified(String, String),
    /// `count(*)` / `sum(x)` / … — an AGGREGATE call.
    ///
    /// Its own node rather than an [`Expr::Func`] because it is not a scalar
    /// function and must not be compiled into one: a scalar runs per row and
    /// returns a value; an aggregate consumes a whole GROUP and only exists once
    /// the rows have been filtered and grouped. Conflating them is how an
    /// aggregate ends up reading the pre-filter tuple stream (DESIGN-MULTIDB §4).
    /// `None` = `count(*)`, which takes the ROW rather than a value.
    /// `f([DISTINCT] arg)`, or `count(*)` (a `None` arg — the row itself).
    /// The bool is DISTINCT: `count(DISTINCT x)` counts distinct non-NULL
    /// values of x.
    Agg(mpedb_types::AggFn, Option<Box<Expr>>, bool),
    /// `<fn>(args) OVER (<spec>)` — a WINDOW function (design/DESIGN-WINDOW.md).
    ///
    /// Its own node (not [`Expr::Agg`]/[`Expr::Func`]) because it is neither a
    /// per-row scalar nor a group-collapsing aggregate: it produces one value
    /// per row computed over a whole PARTITION, and every input row survives.
    /// Conflating it with `Agg` is how a window function would wrongly reach the
    /// GROUP BY machinery. Only ever appears in the SELECT list and ORDER BY;
    /// anywhere else the binder refuses it.
    Window {
        func: WindowFunc,
        /// The aggregate/value argument. `None` for `count(*)` and the ranking
        /// functions (which take no argument).
        arg: Option<Box<Expr>>,
        /// `DISTINCT` inside a window aggregate — refused in stage 1.
        distinct: bool,
        spec: WindowSpecAst,
    },
}

/// Which window function a [`Expr::Window`] calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WindowFunc {
    /// Ranking functions (stage 1a): distinct 1..n / gaps-on-ties / dense.
    RowNumber,
    Rank,
    DenseRank,
    /// An aggregate used as a window (stage 1b) — reuses the aggregate enum, so
    /// the NULL rules, overflow-is-an-error and result typing are identical.
    Agg(mpedb_types::AggFn),
}

/// The `OVER ( [PARTITION BY …] [ORDER BY …] )` spec. No explicit frame in
/// stage 1 — the default frame is computed implicitly (design/DESIGN-WINDOW.md §3.5).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowSpecAst {
    pub partition_by: Vec<Expr>,
    /// `(key, descending)`, mirroring [`SelectStmt::order_by`].
    pub order_by: Vec<(Expr, bool)>,
}
