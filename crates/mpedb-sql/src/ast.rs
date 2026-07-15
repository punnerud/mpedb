//! Untyped AST produced by the parser and consumed by the binder.
//! Carries no source text except literal values and identifiers, so plans
//! built from it are automatically whitespace/keyword-case canonical.

use mpedb_types::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Select(SelectStmt),
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
    /// `INNER JOIN <table> ON <cond>`. One join, so two tables — the plan and
    /// the executor are written for a pair, and an N-way join is a follow-up
    /// rather than something this quietly half-does.
    pub join: Option<JoinClause>,
    /// `SELECT DISTINCT` — deduplicate the OUTPUT rows (the projected tuple),
    /// which is why it cannot be pushed into the scan.
    pub distinct: bool,
    /// `None` = `SELECT *`.
    pub items: Option<Vec<Expr>>,
    pub where_clause: Option<Expr>,
    /// `GROUP BY` column names. Empty with aggregates present = one group over
    /// every surviving row.
    pub group_by: Vec<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
}

/// `[INNER] JOIN <table> ON <cond>`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinClause {
    pub table: String,
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
