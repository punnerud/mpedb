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
    /// `None` = `SELECT *`.
    pub items: Option<Vec<Expr>>,
    pub where_clause: Option<Expr>,
    /// (column name, descending)
    pub order_by: Vec<(String, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertStmt {
    pub table: String,
    /// Explicit column list, if given.
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpdateStmt {
    pub table: String,
    pub set: Vec<(String, Expr)>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeleteStmt {
    pub table: String,
    pub where_clause: Option<Expr>,
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
    /// membership set never reaches the plan bytes.
    ///
    /// Deliberately its own node rather than `Binary(In, e, ContextRef)`: the
    /// right-hand side is not an expression that evaluates to a value on the
    /// stack, it is a param slot the InParam instruction reads directly.
    InContext(Box<Expr>, String),
}
