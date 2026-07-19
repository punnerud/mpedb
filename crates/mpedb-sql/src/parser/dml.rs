//! INSERT / UPDATE / DELETE grammar for the recursive-descent parser,
//! including the `ON CONFLICT` and `RETURNING` clauses.
//!
//! Split out of [`super`] to keep that file under the size limit. The shared
//! [`Parser`] token helpers live in `super` and stay reachable here because
//! `parser::dml` is a descendant module. `insert_stmt`, `update_stmt` and
//! `delete_stmt` are `pub(super)` so the statement dispatch in `super` can
//! reach them; they in turn call `select_core`/`expr` (also `pub(super)`).

use super::{Parser, MAX_SET_ITEMS};
use crate::ast::{DeleteStmt, Expr, InsertStmt, OnConflict, Stmt, UpdateStmt};
use crate::token::{Kw, Tok};
use mpedb_types::Result;

impl<'a> Parser<'a> {
    pub(super) fn insert_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Insert, "INSERT")?;
        // sqlite's conflict-resolution prefix: INSERT OR {IGNORE | ABORT | FAIL
        // | ROLLBACK | REPLACE}. IGNORE = skip conflicting rows; ABORT/FAIL/
        // ROLLBACK = error (mpedb's default). REPLACE deletes every row the new
        // one conflicts with (PK + each unique index) then inserts.
        let or_conflict = if self.eat_kw(Kw::Or) {
            if self.eat_word("IGNORE") {
                Some(OnConflict::DoNothing)
            } else if self.eat_word("REPLACE") {
                Some(OnConflict::Replace)
            } else if self.eat_word("ABORT") || self.eat_word("FAIL") || self.eat_word("ROLLBACK") {
                Some(OnConflict::Error)
            } else {
                return Err(
                    self.err_here("expected IGNORE, REPLACE, ABORT, FAIL, or ROLLBACK after OR")
                );
            }
        } else {
            None
        };
        self.insert_body(or_conflict)
    }

    /// The `[…] INTO table [(cols)] {VALUES … | SELECT …} [ON CONFLICT …]
    /// [RETURNING …]` tail shared by `INSERT [OR …]` and the bare `REPLACE INTO`
    /// alias (sqlite's `REPLACE INTO t …` == `INSERT OR REPLACE INTO t …`).
    /// `or_conflict` is the prefix-determined action (the `REPLACE` alias passes
    /// `Some(OnConflict::Replace)`); when `None`, a trailing `ON CONFLICT` wins.
    pub(super) fn insert_body(&mut self, or_conflict: Option<OnConflict>) -> Result<Stmt> {
        self.expect_kw(Kw::Into, "INTO")?;
        let table = self.ident("table name")?;
        let mut columns = if self.eat(&Tok::LParen) {
            let mut cols = vec![self.ident("column name")?];
            while self.eat(&Tok::Comma) {
                cols.push(self.ident("column name")?);
            }
            self.expect(&Tok::RParen, "`)`")?;
            Some(cols)
        } else {
            None
        };
        // `INSERT INTO t [(cols)] SELECT …` — a source query instead of VALUES.
        let mut rows = Vec::new();
        let mut select = None;
        if self.peek() == Some(&Tok::Kw(Kw::Select)) {
            select = Some(Box::new(self.select_core()?));
        } else if self.eat_word("DEFAULT") {
            // `INSERT INTO t DEFAULT VALUES` — insert ONE row where every column
            // takes its default (a rowid alias auto-assigns; a NOT NULL column
            // with no default is an error, exactly as sqlite). Represented as an
            // explicit EMPTY column list + one empty values row, so `plan_insert`
            // sources every column from its `Default`. A column list cannot be
            // combined with it (sqlite rejects `INSERT INTO t (a) DEFAULT VALUES`).
            self.expect_kw(Kw::Values, "VALUES after DEFAULT")?;
            if columns.is_some() {
                return Err(self.err_here(
                    "DEFAULT VALUES cannot be combined with a column list",
                ));
            }
            columns = Some(Vec::new());
            rows.push(Vec::new());
        } else {
            self.expect_kw(Kw::Values, "VALUES")?;
            loop {
                self.expect(&Tok::LParen, "`(`")?;
                let mut row = vec![self.expr()?];
                while self.eat(&Tok::Comma) {
                    row.push(self.expr()?);
                }
                self.expect(&Tok::RParen, "`)`")?;
                rows.push(row);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            if rows.len() > u16::MAX as usize {
                return Err(self.err_here("too many rows in one INSERT (max 65535)"));
            }
        }
        // A trailing ON CONFLICT clause and the OR-prefix are two spellings of
        // the same thing; the prefix wins when both are present.
        let trailing = self.on_conflict_clause()?;
        let on_conflict = or_conflict.unwrap_or(trailing);
        let returning = self.returning_clause()?;
        Ok(Stmt::Insert(InsertStmt {
            table,
            columns,
            rows,
            select,
            on_conflict,
            returning,
        }))
    }

    /// `ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET … [WHERE …]`.
    fn on_conflict_clause(&mut self) -> Result<OnConflict> {
        if !self.eat_kw(Kw::On) {
            return Ok(OnConflict::Error);
        }
        self.expect_kw(Kw::Conflict, "CONFLICT after ON")?;
        let mut target = Vec::new();
        if self.eat(&Tok::LParen) {
            target.push(self.ident("conflict-target column")?);
            while self.eat(&Tok::Comma) {
                target.push(self.ident("conflict-target column")?);
            }
            self.expect(&Tok::RParen, "`)` closing the conflict target")?;
        }
        self.expect_kw(Kw::Do, "DO after ON CONFLICT")?;
        if self.eat_kw(Kw::Nothing) {
            if !target.is_empty() {
                // PG allows it, but the target then does nothing but mislead:
                // DO NOTHING already covers every unique constraint, so naming
                // one suggests a narrowing that does not happen.
                return Err(self.err_here(
                    "ON CONFLICT DO NOTHING takes no conflict target: it already applies to \
                     every unique constraint on the table",
                ));
            }
            return Ok(OnConflict::DoNothing);
        }
        self.expect_kw(Kw::Update, "UPDATE or NOTHING after DO")?;
        if target.is_empty() {
            return Err(self.err_here(
                "ON CONFLICT ... DO UPDATE needs a conflict target, e.g. ON CONFLICT (id) DO \
                 UPDATE: without it there is no way to know which existing row to update",
            ));
        }
        self.expect_kw(Kw::Set, "SET after DO UPDATE")?;
        let mut set = Vec::new();
        loop {
            let col = self.ident("column name")?;
            self.expect(&Tok::Eq, "`=`")?;
            set.push((col, self.expr()?));
            if set.len() > MAX_SET_ITEMS {
                return Err(self.err_here(format!(
                    "too many SET assignments (max {MAX_SET_ITEMS})"
                )));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(OnConflict::DoUpdate {
            target,
            set,
            where_clause,
        })
    }

    /// `RETURNING * | expr, …`
    fn returning_clause(&mut self) -> Result<Option<Option<Vec<Expr>>>> {
        if !self.eat_kw(Kw::Returning) {
            return Ok(None);
        }
        if self.eat(&Tok::Star) {
            return Ok(Some(None));
        }
        let mut items = vec![self.expr()?];
        while self.eat(&Tok::Comma) {
            items.push(self.expr()?);
        }
        Ok(Some(Some(items)))
    }

    pub(super) fn update_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Update, "UPDATE")?;
        let table = self.ident("table name")?;
        self.expect_kw(Kw::Set, "SET")?;
        let mut set = Vec::new();
        loop {
            let col = self.ident("column name")?;
            self.expect(&Tok::Eq, "`=`")?;
            let val = self.expr()?;
            set.push((col, val));
            if set.len() > MAX_SET_ITEMS {
                return Err(self.err_here(format!(
                    "too many SET assignments (max {MAX_SET_ITEMS})"
                )));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning_clause()?;
        Ok(Stmt::Update(UpdateStmt {
            table,
            set,
            where_clause,
            returning,
        }))
    }

    pub(super) fn delete_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Delete, "DELETE")?;
        self.expect_kw(Kw::From, "FROM")?;
        let table = self.ident("table name")?;
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning_clause()?;
        Ok(Stmt::Delete(DeleteStmt {
            table,
            where_clause,
            returning,
        }))
    }
}
