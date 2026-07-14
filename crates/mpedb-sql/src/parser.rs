//! Recursive-descent parser for the Phase 1 SQL subset.
//!
//! Precedence, loosest to tightest:
//! `OR` < `AND` < `NOT` < comparison / `IS [NOT] NULL` / `LIKE`
//! < `+ -` < `* / %` < unary `-` < primary.
//! Comparisons do not chain (`a < b < c` is a parse error).

use crate::ast::{BinOp, DeleteStmt, Expr, InsertStmt, SelectStmt, Stmt, UnOp, UpdateStmt};
use crate::ddl::{CreatePolicySpec, DdlStmt, RlsAction};
use crate::token::{tokenize, Kw, SpTok, Tok};
use mpedb_types::{Error, PolicyCmd, Result, Value};

/// Maximum expression nesting depth. The expression grammar is recursive
/// descent, so parser stack use is proportional to nesting; without a bound,
/// hostile SQL (or a hostile CHECK source reaching [`parse_expr_only`] at
/// attach time) overflows the thread stack and aborts the process instead of
/// returning an error. 128 is far beyond any legitimate statement while
/// keeping worst-case stack use trivial.
const MAX_EXPR_DEPTH: u32 = 128;

/// Parse-time item caps. Plan wire counts are serialized as `u16`
/// ([`crate::plan`]); these caps keep every count far away from the
/// truncation edge (and bound memory for hostile statements). They are
/// re-validated on the decode side — keep in sync with
/// `CompiledPlan::decode` (plan.rs).
pub(crate) const MAX_SELECT_ITEMS: usize = 4096;
pub(crate) const MAX_ORDER_BY_ITEMS: usize = 64;
pub(crate) const MAX_SET_ITEMS: usize = 1024;

/// Parse a complete statement. Returns the AST, whether it was wrapped in
/// `EXPLAIN`, and the number of parameters ($n gives max n; `?` are numbered
/// left-to-right in statement order).
pub(crate) fn parse_statement(sql: &str) -> Result<(Stmt, bool, u16)> {
    let toks = tokenize(sql)?;
    let mut p = Parser::new(sql, toks);
    let is_explain = if p.eat_kw(Kw::Explain) {
        if p.peek_kw(Kw::Explain) {
            return Err(p.err_here("EXPLAIN cannot be nested"));
        }
        true
    } else {
        false
    };
    let stmt = p.statement()?;
    p.eat(&Tok::Semicolon);
    p.expect_eof()?;
    let n_params = p.n_params()?;
    Ok((stmt, is_explain, n_params))
}

/// Recognize and parse a row-level-security DDL statement (`CREATE POLICY`,
/// `DROP POLICY`, `ALTER TABLE … ROW LEVEL SECURITY`). Returns `Ok(None)` if
/// `sql` is not DDL — the caller then compiles it as an ordinary statement.
/// The DDL words are plain identifiers (not reserved keywords), so no existing
/// column name is affected.
pub(crate) fn parse_ddl(sql: &str) -> Result<Option<DdlStmt>> {
    let toks = tokenize(sql)?;
    let mut p = Parser::new(sql, toks);
    let ddl = match p.peek_ident_ci().as_deref() {
        Some("create") => {
            p.advance();
            p.parse_create_policy()?
        }
        Some("drop") => {
            p.advance();
            p.parse_drop_policy()?
        }
        Some("alter") => {
            p.advance();
            p.parse_alter_rls()?
        }
        _ => return Ok(None),
    };
    p.eat(&Tok::Semicolon);
    p.expect_eof()?;
    Ok(Some(ddl))
}

/// Parse exactly one expression (used for CHECK constraints). Returns the
/// expression and the number of parameters referenced.
pub(crate) fn parse_expr_only(src: &str) -> Result<(Expr, u16)> {
    let toks = tokenize(src)?;
    let mut p = Parser::new(src, toks);
    let e = p.expr()?;
    p.expect_eof()?;
    let n_params = p.n_params()?;
    Ok((e, n_params))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamStyle {
    Unset,
    Dollar,
    Question,
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<SpTok>,
    pos: usize,
    style: ParamStyle,
    /// Next index for a `?` parameter.
    next_question: u32,
    /// max(param index)+1 seen so far.
    max_params: u32,
    /// Current expression nesting depth (see [`MAX_EXPR_DEPTH`]).
    depth: u32,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, toks: Vec<SpTok>) -> Self {
        Parser {
            src,
            toks,
            pos: 0,
            style: ParamStyle::Unset,
            next_question: 0,
            max_params: 0,
            depth: 0,
        }
    }

    /// Bounded by construction ($n indices come from the tokenizer's u16 and
    /// the `?` counter is capped in `primary()`), but never trust an `as`
    /// cast to enforce it: a silent wrap here once turned 65536 parameters
    /// into `n_params == 0` and an out-of-bounds panic in the binder.
    fn n_params(&self) -> Result<u16> {
        u16::try_from(self.max_params).map_err(|_| Error::Parse {
            pos: self.src.len(),
            msg: "too many parameters (max 65535)".into(),
        })
    }

    /// Enter one level of expression recursion; rejects hostile nesting
    /// before it can overflow the stack.
    fn enter_expr(&mut self) -> Result<()> {
        if self.depth >= MAX_EXPR_DEPTH {
            return Err(self.err_here("expression nested too deeply"));
        }
        self.depth += 1;
        Ok(())
    }

    fn exit_expr(&mut self) {
        debug_assert!(self.depth > 0);
        self.depth -= 1;
    }

    // ---- token plumbing ----------------------------------------------

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn here(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|t| t.pos)
            .unwrap_or(self.src.len())
    }

    fn err_here(&self, msg: impl Into<String>) -> Error {
        Error::Parse {
            pos: self.here(),
            msg: msg.into(),
        }
    }

    fn advance(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.tok.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, kw: Kw) -> bool {
        self.eat(&Tok::Kw(kw))
    }

    /// `NOT IN` needs two tokens of lookahead: by the time cmp_expr runs, the
    /// higher-precedence `not_expr` has already passed on this NOT, so `x NOT IN
    /// (…)` only parses if we recognise the pair here.
    fn peek_not_between(&self) -> bool {
        matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Kw(Kw::Not)))
            && matches!(
                self.toks.get(self.pos + 1).map(|t| &t.tok),
                Some(Tok::Kw(Kw::Between))
            )
    }

    fn peek_not_in(&self) -> bool {
        matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Kw(Kw::Not)))
            && matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Kw(Kw::In)))
    }

    fn peek_kw(&self, kw: Kw) -> bool {
        self.peek() == Some(&Tok::Kw(kw))
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected {what}")))
        }
    }

    fn expect_kw(&mut self, kw: Kw, what: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected {what}")))
        }
    }

    fn expect_eof(&mut self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(t) => Err(self.err_here(format!("unexpected trailing input `{t:?}`"))),
        }
    }

    // ---- DDL plumbing (RLS policy statements) ------------------------

    /// The current token as a lowercased identifier, if it is a bare Ident.
    fn peek_ident_ci(&self) -> Option<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => Some(s.to_ascii_lowercase()),
            _ => None,
        }
    }

    /// Consume a bare identifier equal (case-insensitively) to `w`.
    fn eat_word(&mut self, w: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s.eq_ignore_ascii_case(w) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn expect_word(&mut self, w: &str) -> Result<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.err_here(format!("expected `{w}`")))
        }
    }

    /// Capture the SOURCE of a `( <expr> )` — the balanced substring between the
    /// parentheses — without parsing it (stored verbatim, re-bound later, §3.2).
    fn capture_paren_source(&mut self) -> Result<String> {
        self.expect(&Tok::LParen, "`(`")?;
        let start = self.here();
        let mut depth = 1usize;
        let close = loop {
            let here = self.here();
            match self.advance() {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        break here;
                    }
                }
                Some(_) => {}
                None => return Err(self.err_here("unterminated parenthesized policy expression")),
            }
        };
        let src = self.src.get(start..close).unwrap_or("").trim().to_string();
        if src.is_empty() {
            return Err(self.err_here("policy expression must not be empty"));
        }
        Ok(src)
    }

    fn policy_command(&mut self) -> Result<PolicyCmd> {
        if self.eat_kw(Kw::Select) {
            Ok(PolicyCmd::Select)
        } else if self.eat_kw(Kw::Insert) {
            Ok(PolicyCmd::Insert)
        } else if self.eat_kw(Kw::Update) {
            Ok(PolicyCmd::Update)
        } else if self.eat_kw(Kw::Delete) {
            Ok(PolicyCmd::Delete)
        } else if self.eat_word("ALL") {
            Ok(PolicyCmd::All)
        } else {
            Err(self.err_here("expected ALL, SELECT, INSERT, UPDATE, or DELETE"))
        }
    }

    fn expect_row_level_security(&mut self) -> Result<()> {
        self.expect_word("ROW")?;
        self.expect_word("LEVEL")?;
        self.expect_word("SECURITY")
    }

    fn parse_create_policy(&mut self) -> Result<DdlStmt> {
        self.expect_word("POLICY")?;
        let name = self.ident("policy name")?;
        self.expect_word("ON")?;
        let table = self.ident("table name")?;
        let mut permissive = true;
        if self.eat_word("AS") {
            if self.eat_word("PERMISSIVE") {
                permissive = true;
            } else if self.eat_word("RESTRICTIVE") {
                permissive = false;
            } else {
                return Err(self.err_here("expected PERMISSIVE or RESTRICTIVE"));
            }
        }
        let command = if self.eat_word("FOR") {
            self.policy_command()?
        } else {
            PolicyCmd::All
        };
        let using_src = if self.eat_word("USING") {
            Some(self.capture_paren_source()?)
        } else {
            None
        };
        let check_src = if self.eat_word("WITH") {
            self.expect_word("CHECK")?;
            Some(self.capture_paren_source()?)
        } else {
            None
        };
        if using_src.is_none() && check_src.is_none() {
            return Err(self.err_here("a policy must have USING and/or WITH CHECK"));
        }
        Ok(DdlStmt::CreatePolicy(CreatePolicySpec {
            name,
            table,
            command,
            permissive,
            using_src,
            check_src,
        }))
    }

    fn parse_drop_policy(&mut self) -> Result<DdlStmt> {
        self.expect_word("POLICY")?;
        let name = self.ident("policy name")?;
        self.expect_word("ON")?;
        let table = self.ident("table name")?;
        Ok(DdlStmt::DropPolicy { table, name })
    }

    fn parse_alter_rls(&mut self) -> Result<DdlStmt> {
        self.expect_word("TABLE")?;
        let table = self.ident("table name")?;
        let action = if self.eat_word("ENABLE") {
            self.expect_row_level_security()?;
            RlsAction::Enable { force: false }
        } else if self.eat_word("FORCE") {
            self.expect_row_level_security()?;
            RlsAction::Enable { force: true }
        } else if self.eat_word("DISABLE") {
            self.expect_row_level_security()?;
            RlsAction::Disable
        } else {
            return Err(self.err_here("expected ENABLE, FORCE, or DISABLE ROW LEVEL SECURITY"));
        };
        Ok(DdlStmt::AlterRls { table, action })
    }

    /// Identifier (bare or quoted).
    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_)) => {
                match self.advance() {
                    Some(Tok::Ident(s)) | Some(Tok::QuotedIdent(s)) => Ok(s),
                    _ => unreachable!(),
                }
            }
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }

    // ---- statements ---------------------------------------------------

    fn statement(&mut self) -> Result<Stmt> {
        match self.peek() {
            Some(Tok::Kw(Kw::Select)) => self.select_stmt(),
            Some(Tok::Kw(Kw::Insert)) => self.insert_stmt(),
            Some(Tok::Kw(Kw::Update)) => self.update_stmt(),
            Some(Tok::Kw(Kw::Delete)) => self.delete_stmt(),
            Some(Tok::Kw(Kw::Begin)) => {
                self.pos += 1;
                Ok(Stmt::Begin)
            }
            Some(Tok::Kw(Kw::Commit)) => {
                self.pos += 1;
                Ok(Stmt::Commit)
            }
            Some(Tok::Kw(Kw::Rollback)) => {
                self.pos += 1;
                Ok(Stmt::Rollback)
            }
            _ => Err(self.err_here("expected a statement (SELECT, INSERT, UPDATE, DELETE, BEGIN, COMMIT, ROLLBACK)")),
        }
    }

    fn select_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Select, "SELECT")?;
        let items = if self.eat(&Tok::Star) {
            None
        } else {
            let mut items = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                if items.len() >= MAX_SELECT_ITEMS {
                    return Err(self.err_here(format!(
                        "too many SELECT items (max {MAX_SELECT_ITEMS})"
                    )));
                }
                items.push(self.expr()?);
            }
            Some(items)
        };
        self.expect_kw(Kw::From, "FROM")?;
        let table = self.ident("table name")?;
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw(Kw::Order) {
            self.expect_kw(Kw::By, "BY after ORDER")?;
            loop {
                let col = self.ident("column name in ORDER BY")?;
                let desc = if self.eat_kw(Kw::Desc) {
                    true
                } else {
                    self.eat_kw(Kw::Asc);
                    false
                };
                order_by.push((col, desc));
                if order_by.len() > MAX_ORDER_BY_ITEMS {
                    return Err(self.err_here(format!(
                        "too many ORDER BY items (max {MAX_ORDER_BY_ITEMS})"
                    )));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let limit = if self.eat_kw(Kw::Limit) {
            Some(self.nonneg_int("LIMIT")?)
        } else {
            None
        };
        let offset = if self.eat_kw(Kw::Offset) {
            Some(self.nonneg_int("OFFSET")?)
        } else {
            None
        };
        Ok(Stmt::Select(SelectStmt {
            table,
            items,
            where_clause,
            order_by,
            limit,
            offset,
        }))
    }

    fn nonneg_int(&mut self, what: &str) -> Result<u64> {
        match self.peek() {
            Some(&Tok::Int(v)) if v >= 0 => {
                self.pos += 1;
                Ok(v as u64)
            }
            _ => Err(self.err_here(format!("{what} requires a non-negative integer literal"))),
        }
    }

    fn insert_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Insert, "INSERT")?;
        self.expect_kw(Kw::Into, "INTO")?;
        let table = self.ident("table name")?;
        let columns = if self.eat(&Tok::LParen) {
            let mut cols = vec![self.ident("column name")?];
            while self.eat(&Tok::Comma) {
                cols.push(self.ident("column name")?);
            }
            self.expect(&Tok::RParen, "`)`")?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw(Kw::Values, "VALUES")?;
        let mut rows = Vec::new();
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
        Ok(Stmt::Insert(InsertStmt {
            table,
            columns,
            rows,
        }))
    }

    fn update_stmt(&mut self) -> Result<Stmt> {
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
        Ok(Stmt::Update(UpdateStmt {
            table,
            set,
            where_clause,
        }))
    }

    fn delete_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Delete, "DELETE")?;
        self.expect_kw(Kw::From, "FROM")?;
        let table = self.ident("table name")?;
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Delete(DeleteStmt {
            table,
            where_clause,
        }))
    }

    // ---- expressions ----------------------------------------------------

    // Depth guards: `expr()` covers the `( expr )` cycle through `primary()`;
    // `not_expr`/`unary_expr` guard their direct self-recursion, which does
    // not pass back through `expr()`.

    fn expr(&mut self) -> Result<Expr> {
        self.enter_expr()?;
        let e = self.or_expr();
        self.exit_expr();
        e
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut e = self.and_expr()?;
        while self.eat_kw(Kw::Or) {
            let r = self.and_expr()?;
            e = Expr::Binary(BinOp::Or, Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut e = self.not_expr()?;
        while self.eat_kw(Kw::And) {
            let r = self.not_expr()?;
            e = Expr::Binary(BinOp::And, Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw(Kw::Not) {
            self.enter_expr()?;
            let e = self.not_expr();
            self.exit_expr();
            Ok(Expr::Unary(UnOp::Not, Box::new(e?)))
        } else {
            self.cmp_expr()
        }
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let mut e = self.add_expr()?;
        let mut seen_cmp = false;
        loop {
            if self.eat_kw(Kw::Is) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::Null, "NULL after IS")?;
                e = Expr::IsNull(Box::new(e), negated);
                continue;
            }
            if !seen_cmp && self.peek_kw(Kw::Like) {
                self.pos += 1;
                let pat = self.add_expr()?;
                e = Expr::Like(Box::new(e), Box::new(pat));
                seen_cmp = true;
                continue;
            }
            // `x BETWEEN a AND b` — desugared right here into
            // `x >= a AND x <= b` rather than carried as its own node.
            //
            // That is not laziness: the planner extracts a PkRange from exactly
            // that conjunct shape, so the desugaring turns `id BETWEEN 10 AND 20`
            // into a range SCAN instead of a full scan plus a filter. A dedicated
            // BETWEEN node would have to teach extract_access a second spelling
            // of the same fact.
            //
            // The cost is that `x` appears twice in the plan. For the usual
            // `column BETWEEN lo AND hi` that is one extra PushCol.
            if !seen_cmp && (self.peek_kw(Kw::Between) || self.peek_not_between()) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::Between, "BETWEEN")?;
                // add_expr, NOT expr: the AND below belongs to BETWEEN's own
                // syntax, so a full expression parse would swallow it and then
                // fail looking for an AND that is already gone.
                let lo = self.add_expr()?;
                self.expect_kw(Kw::And, "AND in BETWEEN")?;
                let hi = self.add_expr()?;
                let ge = Expr::Binary(BinOp::Ge, Box::new(e.clone()), Box::new(lo));
                let le = Expr::Binary(BinOp::Le, Box::new(e), Box::new(hi));
                let both = Expr::Binary(BinOp::And, Box::new(ge), Box::new(le));
                // NOT BETWEEN is NOT(a AND b), which under 3VL is NOT the same as
                // (NOT a OR NOT b) when an operand is NULL -- De Morgan holds in
                // 3VL, but only if both sides are the SAME 3VL values, and
                // spelling it out twice invites drift. Negate the whole thing.
                e = if negated {
                    Expr::Unary(UnOp::Not, Box::new(both))
                } else {
                    both
                };
                seen_cmp = true;
                continue;
            }
            // `x IN (…)` and `x NOT IN (…)`. Two shapes share the syntax:
            // a session-context list (§2.6, one reserved param — the arity must
            // NOT reach the plan bytes) and a general value list (#21, arity IS
            // the query). Which one is decided by what is inside the parens.
            if !seen_cmp && (self.peek_kw(Kw::In) || self.peek_not_in()) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::In, "IN")?;
                self.expect(&Tok::LParen, "`(` after IN")?;
                // `IN ()` is a syntax error in PostgreSQL too. Allowing it would
                // also mean an InList(0) instruction, which the IR rejects.
                if self.peek() == Some(&Tok::RParen) {
                    return Err(self.err_here("IN needs at least one value: `IN ()` is empty"));
                }
                let first = self.expr()?;
                if let (Expr::ContextRef(key), Some(&Tok::RParen)) = (&first, self.peek()) {
                    let key = key.clone();
                    self.pos += 1;
                    e = Expr::InContext(Box::new(e), key, negated);
                    seen_cmp = true;
                    continue;
                }
                let mut items = vec![first];
                while self.eat(&Tok::Comma) {
                    items.push(self.expr()?);
                }
                self.expect(&Tok::RParen, "`)` closing IN")?;
                e = Expr::InList(Box::new(e), items, negated);
                seen_cmp = true;
                continue;
            }
            let op = match self.peek() {
                Some(Tok::Eq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => break,
            };
            if seen_cmp {
                break; // non-chaining: leftover op is a trailing-input error
            }
            self.pos += 1;
            let r = self.add_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
            seen_cmp = true;
        }
        Ok(e)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut e = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let r = self.mul_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut e = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let r = self.unary_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    fn unary_expr(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            self.enter_expr()?;
            let e = self.unary_expr();
            self.exit_expr();
            Ok(Expr::Unary(UnOp::Neg, Box::new(e?)))
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let pos = self.here();
        match self.advance() {
            Some(Tok::Int(v)) => Ok(Expr::Lit(Value::Int(v))),
            Some(Tok::Float(v)) => Ok(Expr::Lit(Value::Float(v))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::Text(s))),
            Some(Tok::Blob(b)) => Ok(Expr::Lit(Value::Blob(b))),
            Some(Tok::Kw(Kw::True)) => Ok(Expr::Lit(Value::Bool(true))),
            Some(Tok::Kw(Kw::False)) => Ok(Expr::Lit(Value::Bool(false))),
            Some(Tok::Kw(Kw::Null)) => Ok(Expr::Lit(Value::Null)),
            Some(Tok::DollarParam(i)) => {
                self.param_style(ParamStyle::Dollar, pos)?;
                self.max_params = self.max_params.max(i as u32 + 1);
                Ok(Expr::Param(i))
            }
            Some(Tok::Question) => {
                self.param_style(ParamStyle::Question, pos)?;
                let i = self.next_question;
                // Reject the 65536th `?` (index 65535): max_params must stay
                // <= 65535 so it round-trips through the u16 plan encoding —
                // the same limit the tokenizer enforces for `$n`.
                if i >= u16::MAX as u32 {
                    return Err(Error::Parse {
                        pos,
                        msg: "too many `?` parameters (max 65535)".into(),
                    });
                }
                self.next_question += 1;
                self.max_params = self.max_params.max(i + 1);
                Ok(Expr::Param(i as u16))
            }
            Some(Tok::Ident(s)) => {
                // `current_setting('key')` is the only function form in Phase 1.
                // Recognized only as a bare identifier immediately followed by
                // `(`; a quoted "current_setting" or one without `(` is a column.
                if s.eq_ignore_ascii_case("current_setting") && self.eat(&Tok::LParen) {
                    let key = match self.advance() {
                        Some(Tok::Str(k)) if !k.is_empty() => k,
                        _ => {
                            return Err(self.err_here(
                                "current_setting() takes a single non-empty string-literal key",
                            ))
                        }
                    };
                    self.expect(&Tok::RParen, "`)`")?;
                    Ok(Expr::ContextRef(key))
                } else {
                    Ok(Expr::Col(s))
                }
            }
            Some(Tok::QuotedIdent(s)) => Ok(Expr::Col(s)),
            Some(Tok::LParen) => {
                let e = self.expr()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(e)
            }
            _ => Err(Error::Parse {
                pos,
                msg: "expected an expression".into(),
            }),
        }
    }

    fn param_style(&mut self, style: ParamStyle, pos: usize) -> Result<()> {
        if self.style == ParamStyle::Unset {
            self.style = style;
        } else if self.style != style {
            return Err(Error::Parse {
                pos,
                msg: "cannot mix `?` and `$n` parameters in one statement".into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(src: &str) -> Expr {
        parse_expr_only(src).unwrap().0
    }

    fn col(name: &str) -> Box<Expr> {
        Box::new(Expr::Col(name.into()))
    }

    fn int(v: i64) -> Box<Expr> {
        Box::new(Expr::Lit(Value::Int(v)))
    }

    #[test]
    fn or_binds_looser_than_and() {
        // a = 1 OR b = 2 AND c = 3  ==  a=1 OR (b=2 AND c=3)
        let e = expr("a = 1 OR b = 2 AND c = 3");
        let eq = |c: &str, v: i64| Box::new(Expr::Binary(BinOp::Eq, col(c), int(v)));
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Or,
                eq("a", 1),
                Box::new(Expr::Binary(BinOp::And, eq("b", 2), eq("c", 3)))
            )
        );
    }

    #[test]
    fn not_binds_looser_than_comparison() {
        // NOT a = 1  ==  NOT (a = 1)
        let e = expr("NOT a = 1");
        assert_eq!(
            e,
            Expr::Unary(
                UnOp::Not,
                Box::new(Expr::Binary(BinOp::Eq, col("a"), int(1)))
            )
        );
    }

    /// BETWEEN's own AND must not be eaten by boolean AND: parsing the upper
    /// bound with a full expression parse would swallow the AND and then fail
    /// looking for the one it just consumed.
    #[test]
    fn between_desugars_to_a_range_conjunct() {
        let (e, _) = parse_expr_only("a BETWEEN 1 AND 3").unwrap();
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::And,
                Box::new(Expr::Binary(BinOp::Ge, col("a"), int(1))),
                Box::new(Expr::Binary(BinOp::Le, col("a"), int(3))),
            )
        );
    }

    #[test]
    fn between_composes_with_a_following_boolean_and() {
        let (e, _) = parse_expr_only("a BETWEEN 1 AND 3 AND b = 2").unwrap();
        // the trailing `AND b = 2` is a separate conjunct, not BETWEEN's bound
        assert!(matches!(&e, Expr::Binary(BinOp::And, _, r)
            if matches!(r.as_ref(), Expr::Binary(BinOp::Eq, ..))), "got {e:?}");
    }

    #[test]
    fn not_between_negates_the_whole_conjunct() {
        let (e, _) = parse_expr_only("a NOT BETWEEN 1 AND 3").unwrap();
        assert!(matches!(&e, Expr::Unary(UnOp::Not, inner)
            if matches!(inner.as_ref(), Expr::Binary(BinOp::And, ..))), "got {e:?}");
    }

    #[test]
    fn in_list_parses_both_shapes_and_negation() {
        let (e, _) = parse_expr_only("a IN (1, 2)").unwrap();
        assert!(matches!(&e, Expr::InList(_, items, false) if items.len() == 2), "got {e:?}");
        let (e, _) = parse_expr_only("a NOT IN (1)").unwrap();
        assert!(matches!(&e, Expr::InList(_, _, true)), "got {e:?}");
        // one-element parens must still be the context form when it IS one
        let (e, _) = parse_expr_only("a IN (current_setting('k'))").unwrap();
        assert!(matches!(&e, Expr::InContext(_, k, false) if k == "k"), "got {e:?}");
        let (e, _) = parse_expr_only("a NOT IN (current_setting('k'))").unwrap();
        assert!(matches!(&e, Expr::InContext(_, _, true)), "got {e:?}");
    }

    #[test]
    fn unary_minus_binds_tighter_than_mul() {
        // -2 * 3 == (-2) * 3
        let e = expr("-2 * 3");
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Mul,
                Box::new(Expr::Unary(UnOp::Neg, int(2))),
                int(3)
            )
        );
    }

    #[test]
    fn arithmetic_precedence_over_comparison() {
        // a + 1 < b * 2  ==  (a+1) < (b*2)
        let e = expr("a + 1 < b * 2");
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Lt,
                Box::new(Expr::Binary(BinOp::Add, col("a"), int(1))),
                Box::new(Expr::Binary(BinOp::Mul, col("b"), int(2)))
            )
        );
    }

    #[test]
    fn comparisons_do_not_chain() {
        assert!(matches!(
            parse_expr_only("a < b < c"),
            Err(Error::Parse { .. })
        ));
    }

    #[test]
    fn is_null_and_like() {
        assert_eq!(expr("a IS NULL"), Expr::IsNull(col("a"), false));
        assert_eq!(expr("a IS NOT NULL"), Expr::IsNull(col("a"), true));
        assert_eq!(
            expr("a LIKE 'x%'"),
            Expr::Like(col("a"), Box::new(Expr::Lit(Value::Text("x%".into()))))
        );
    }

    #[test]
    fn question_params_number_left_to_right() {
        let (e, n) = parse_expr_only("? + ? = ?").unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            e,
            Expr::Binary(
                BinOp::Eq,
                Box::new(Expr::Binary(
                    BinOp::Add,
                    Box::new(Expr::Param(0)),
                    Box::new(Expr::Param(1))
                )),
                Box::new(Expr::Param(2))
            )
        );
    }

    #[test]
    fn mixing_param_styles_is_a_parse_error() {
        match parse_statement("SELECT * FROM t WHERE a = $1 AND b = ?") {
            Err(Error::Parse { pos, msg }) => {
                assert_eq!(pos, 37);
                assert!(msg.contains("mix"));
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn dollar_params_report_max() {
        let (_, _, n) = parse_statement("SELECT * FROM t WHERE a = $3").unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn full_select() {
        let (s, explain, n) = parse_statement(
            "explain select a, b + 1 from t where a > 5 order by a asc, b desc limit 10 offset 2;",
        )
        .unwrap();
        assert!(explain);
        assert_eq!(n, 0);
        match s {
            Stmt::Select(sel) => {
                assert_eq!(sel.table, "t");
                assert_eq!(sel.items.as_ref().unwrap().len(), 2);
                assert!(sel.where_clause.is_some());
                assert_eq!(sel.order_by, vec![("a".into(), false), ("b".into(), true)]);
                assert_eq!(sel.limit, Some(10));
                assert_eq!(sel.offset, Some(2));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn select_star_and_limits() {
        let (s, explain, _) = parse_statement("SELECT * FROM t").unwrap();
        assert!(!explain);
        assert!(matches!(s, Stmt::Select(SelectStmt { items: None, .. })));
        assert!(parse_statement("SELECT * FROM t LIMIT -1").is_err());
        assert!(parse_statement("SELECT * FROM t LIMIT $1").is_err());
        assert!(parse_statement("SELECT *, a FROM t").is_err());
    }

    #[test]
    fn insert_forms() {
        let (s, _, n) = parse_statement("INSERT INTO t (a, b) VALUES (1, $1), (2, $2)").unwrap();
        assert_eq!(n, 2);
        match s {
            Stmt::Insert(ins) => {
                assert_eq!(ins.columns, Some(vec!["a".into(), "b".into()]));
                assert_eq!(ins.rows.len(), 2);
            }
            other => panic!("expected insert, got {other:?}"),
        }
        let (s, _, _) = parse_statement("INSERT INTO t VALUES (1, 2)").unwrap();
        assert!(matches!(s, Stmt::Insert(InsertStmt { columns: None, .. })));
    }

    #[test]
    fn update_and_delete() {
        let (s, _, _) = parse_statement("UPDATE t SET a = 1, b = b + 1 WHERE c = 2").unwrap();
        match s {
            Stmt::Update(u) => assert_eq!(u.set.len(), 2),
            other => panic!("expected update, got {other:?}"),
        }
        let (s, _, _) = parse_statement("DELETE FROM t WHERE a = 1").unwrap();
        assert!(matches!(s, Stmt::Delete(_)));
        let (s, _, _) = parse_statement("DELETE FROM t").unwrap();
        assert!(matches!(s, Stmt::Delete(DeleteStmt { where_clause: None, .. })));
    }

    #[test]
    fn txn_statements() {
        assert!(matches!(parse_statement("BEGIN").unwrap().0, Stmt::Begin));
        assert!(matches!(parse_statement("commit;").unwrap().0, Stmt::Commit));
        assert!(matches!(
            parse_statement("Rollback").unwrap().0,
            Stmt::Rollback
        ));
    }

    #[test]
    fn deep_nesting_is_a_parse_error_not_a_crash() {
        // Each of these used to overflow the parser stack and abort the
        // process (uncatchable). They must return Error::Parse instead.
        let parens = format!("{}a > 0{}", "(".repeat(2000), ")".repeat(2000));
        assert!(matches!(
            parse_expr_only(&parens),
            Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
        ));
        // The same input through the statement path (prepare()/CHECK).
        let sql = format!("SELECT * FROM t WHERE {parens}");
        assert!(matches!(
            parse_statement(&sql),
            Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
        ));
        let nots = format!("{}a", "NOT ".repeat(2000));
        assert!(matches!(
            parse_expr_only(&nots),
            Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
        ));
        let negs = format!("{}1", "-".repeat(2000));
        assert!(matches!(
            parse_expr_only(&negs),
            Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
        ));

        // Depth 100 is comfortably within the limit for every form.
        let parens = format!("{}a > 0{}", "(".repeat(100), ")".repeat(100));
        assert!(parse_expr_only(&parens).is_ok());
        assert!(parse_expr_only(&format!("{}a", "NOT ".repeat(100))).is_ok());
        assert!(parse_expr_only(&format!("{}1", "-".repeat(100))).is_ok());
    }

    #[test]
    fn item_count_caps() {
        // 70000 projection items: rejected at parse time (the plan encoding
        // stores the count as u16; unchecked it would truncate).
        let mut sql = String::from("SELECT a");
        for _ in 0..69_999 {
            sql.push_str(",a");
        }
        sql.push_str(" FROM t");
        assert!(matches!(
            parse_statement(&sql),
            Err(Error::Parse { msg, .. }) if msg.contains("too many SELECT items")
        ));
        // Exactly the cap still parses.
        let mut sql = String::from("SELECT a");
        for _ in 0..MAX_SELECT_ITEMS - 1 {
            sql.push_str(",a");
        }
        sql.push_str(" FROM t");
        assert!(parse_statement(&sql).is_ok());

        // ORDER BY: 65 items rejected, 64 accepted.
        let mk_order = |n: usize| {
            format!(
                "SELECT * FROM t ORDER BY {}",
                vec!["a"; n].join(", ")
            )
        };
        assert!(matches!(
            parse_statement(&mk_order(MAX_ORDER_BY_ITEMS + 1)),
            Err(Error::Parse { msg, .. }) if msg.contains("too many ORDER BY items")
        ));
        assert!(parse_statement(&mk_order(MAX_ORDER_BY_ITEMS)).is_ok());

        // UPDATE SET: 1025 assignments rejected, 1024 accepted.
        let mk_set = |n: usize| {
            format!(
                "UPDATE t SET {}",
                vec!["a = 1"; n].join(", ")
            )
        };
        assert!(matches!(
            parse_statement(&mk_set(MAX_SET_ITEMS + 1)),
            Err(Error::Parse { msg, .. }) if msg.contains("too many SET assignments")
        ));
        assert!(parse_statement(&mk_set(MAX_SET_ITEMS)).is_ok());
    }

    #[test]
    fn param_count_limit_is_enforced_not_truncated() {
        // 32768 rows x 2 columns = 65536 `?`: must be a parse error at the
        // 65536th `?`, never a silent wrap to n_params == 0 (which used to
        // panic the binder).
        let mut sql = String::from("INSERT INTO t (a, b) VALUES ");
        sql.push_str(&vec!["(?,?)"; 32_768].join(","));
        assert!(matches!(
            parse_statement(&sql),
            Err(Error::Parse { msg, .. }) if msg.contains("too many `?` parameters")
        ));

        // Exactly 65535 `?` (the maximum) still parses with the right count.
        let mut sql = String::from("INSERT INTO t (a) VALUES ");
        sql.push_str(&vec!["(?)"; 65_535].join(","));
        let (_, _, n) = parse_statement(&sql).unwrap();
        assert_eq!(n, 65_535);

        // $n form: $65535 is the maximum; $65536 is rejected by the
        // tokenizer.
        let (_, _, n) = parse_statement("SELECT * FROM t WHERE a = $65535").unwrap();
        assert_eq!(n, 65_535);
        assert!(parse_statement("SELECT * FROM t WHERE a = $65536").is_err());
    }

    #[test]
    fn error_positions_and_trailing_input() {
        match parse_statement("SELECT FROM t") {
            Err(Error::Parse { pos, .. }) => assert_eq!(pos, 7),
            other => panic!("expected parse error, got {other:?}"),
        }
        assert!(parse_statement("SELECT * FROM t garbage").is_err());
        assert!(parse_statement("SELECT * FROM t; SELECT * FROM t").is_err());
        assert!(parse_statement("EXPLAIN EXPLAIN SELECT * FROM t").is_err());
        assert!(parse_expr_only("a = ").is_err());
        assert!(parse_expr_only("(a = 1").is_err());
    }
}
