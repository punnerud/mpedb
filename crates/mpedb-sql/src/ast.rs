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
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectStmt {
    pub table: String,
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
    pub rows: Vec<Vec<Expr>>,
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

/// `[INNER | LEFT [OUTER]] JOIN <table> ON <cond>`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinClause {
    pub table: String,
    pub alias: Option<String>,
    pub kind: JoinKind,
    pub on: Expr,
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
    /// `lhs LIKE pattern`.
    Like(Box<Expr>, Box<Expr>),
    /// `CAST(x AS <type>)`.
    Cast(Box<Expr>, mpedb_types::ColumnType),
    /// `(SELECT …)` — a scalar subquery: one output column; 0 rows = NULL,
    /// more than one row is a runtime error (PostgreSQL's rule — sqlite
    /// silently takes the first row). The planner lifts it out into the
    /// plan's subplan table and replaces this node with a reserved parameter.
    Subquery(Box<SelectStmt>),
    /// `[NOT] EXISTS (SELECT …)` — did the subquery produce any row. The
    /// bool is `negated`.
    Exists(Box<SelectStmt>, bool),
    /// `current_setting('key')` — a session-context value, bound to a reserved
    /// parameter filled from the caller's [`Session`](mpedb) at execute time
    /// (DESIGN-MULTIDB.md §2.1). The value never enters the plan bytes, so one
    /// content-hashed plan serves every session.
    ContextRef(String),
    /// `<expr> IN (current_setting('key'))` — membership in a session-context
    /// list (DESIGN-MULTIDB.md §2.6). The key binds to ONE reserved param
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
}
