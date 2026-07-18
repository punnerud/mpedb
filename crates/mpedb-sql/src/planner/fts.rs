//! FTS5 `MATCH` planning (design/DESIGN-FTS.md §3): recognize a top-level
//! `<col-or-table> MATCH 'literal'` WHERE conjunct against an FTS table, parse
//! the literal into a compiled [`FtsQuery`] tree (normalizing every term with
//! the table's frozen tokenizer), and turn it into an [`AccessPath::FtsScan`].
//!
//! MATCH anywhere ELSE — a scalar context, a non-FTS column/table, a SELECT-list
//! item, inside an OR, or a second MATCH conjunct — is left in the residual WHERE
//! for the binder to reject with the identical sqlite error ("unable to use
//! function MATCH in the requested context", `binder.rs`). This module only
//! consumes the legal shape.

use super::*;
use crate::plan::{FtsQuery, FtsTerm};
use mpedb_types::fts::{self, Tokenizer};

/// Look for a top-level `Expr::Match` conjunct in `where_clause`. If found, build
/// the `FtsScan` for it and return the residual WHERE (the conjunct removed); a
/// leftover MATCH in the residual is rejected later by the binder. Returns `None`
/// when there is no MATCH at all — the ordinary access path then applies.
///
/// `table_ref` is the name the query uses for the table (its alias if any, else
/// the real name), so `f MATCH …` after `FROM ft AS f` resolves.
pub(super) fn extract_fts_where(
    where_clause: Option<&ast::Expr>,
    table: &TableDef,
    table_ref: &str,
) -> Result<Option<(AccessPath, Option<ast::Expr>)>> {
    let Some(w) = where_clause else {
        return Ok(None);
    };
    let mut conjuncts = Vec::new();
    split_and_ast(w.clone(), &mut conjuncts);
    let Some(idx) = conjuncts.iter().position(|c| matches!(c, ast::Expr::Match(_, _))) else {
        return Ok(None);
    };
    let ast::Expr::Match(lhs, rhs) = &conjuncts[idx] else {
        unreachable!("position matched a Match node")
    };
    let access = plan_fts_match(lhs, rhs, table, table_ref)?;
    conjuncts.remove(idx);
    Ok(Some((access, rebuild_and(conjuncts))))
}

fn split_and_ast(e: ast::Expr, out: &mut Vec<ast::Expr>) {
    match e {
        ast::Expr::Binary(BinOp::And, l, r) => {
            split_and_ast(*l, out);
            split_and_ast(*r, out);
        }
        other => out.push(other),
    }
}

fn rebuild_and(conjuncts: Vec<ast::Expr>) -> Option<ast::Expr> {
    let mut it = conjuncts.into_iter();
    let mut acc = it.next()?;
    for e in it {
        acc = ast::Expr::Binary(BinOp::And, Box::new(acc), Box::new(e));
    }
    Some(acc)
}

/// The one error sqlite raises for any misuse of MATCH.
fn match_ctx_err() -> Error {
    Error::Bind("unable to use function MATCH in the requested context".into())
}

fn plan_fts_match(
    lhs: &ast::Expr,
    rhs: &ast::Expr,
    table: &TableDef,
    table_ref: &str,
) -> Result<AccessPath> {
    // MATCH is only meaningful against an FTS table.
    let Some(tokenizer) = table.kind.fts_tokenizer() else {
        return Err(match_ctx_err());
    };
    // Right operand: a text literal, parsed at plan time (design/DESIGN-FTS.md
    // §3). A parameter is refused by name (Phase 1), matching LIKE/GLOB.
    let query_str = match rhs {
        ast::Expr::Lit(Value::Text(s)) => s.clone(),
        ast::Expr::Param(_) => {
            return Err(Error::Bind(
                "MATCH right operand must be a string literal (Phase 1)".into(),
            ))
        }
        _ => return Err(match_ctx_err()),
    };
    // Left operand: the FTS table itself (whole-row) or one of its content
    // columns (column-scoped). Anything else is the misuse error.
    let default_columns: Vec<u16> = match lhs {
        ast::Expr::Col(name) => resolve_match_target(table, table_ref, name)?,
        ast::Expr::Qualified(tbl, col) => {
            if !tbl.eq_ignore_ascii_case(table_ref) && !tbl.eq_ignore_ascii_case(&table.name) {
                return Err(match_ctx_err());
            }
            resolve_match_target(table, table_ref, col)?
        }
        _ => return Err(match_ctx_err()),
    };
    let query = parse_query(&query_str, tokenizer, table, &default_columns)?;
    Ok(AccessPath::FtsScan { query })
}

/// Resolve the MATCH left operand `name` against the FTS table: the table's own
/// name/alias ⇒ whole-row (no column restriction, `[]`); a content column ⇒
/// that column's ordinal; anything else ⇒ the misuse error.
fn resolve_match_target(table: &TableDef, table_ref: &str, name: &str) -> Result<Vec<u16>> {
    if name.eq_ignore_ascii_case(table_ref) || name.eq_ignore_ascii_case(&table.name) {
        return Ok(Vec::new());
    }
    match content_colno(table, name) {
        Some(colno) => Ok(vec![colno]),
        None => Err(match_ctx_err()),
    }
}

/// The FTS colno of a declared content column by name (case-insensitive), or
/// `None` if the name is not a content column (e.g. the rowid PK).
fn content_colno(table: &TableDef, name: &str) -> Option<u16> {
    let col_index = table.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))? as u16;
    table.fts_colno(col_index)
}

// ---- FTS5 query-string grammar (design/DESIGN-FTS.md §3) -------------------
//
// or_expr  := and_expr (OR and_expr)*
// and_expr := not_expr ( [AND] not_expr )*        (juxtaposition = implicit AND)
// not_expr := primary (NOT primary)*
// primary  := '(' or_expr ')' | colfilter? '^'? word '*'?
// colfilter := word ':' | '{' word+ '}' ':'
//
// Precedence highest→lowest: NOT, AND, OR (sqlite fts5). The operators AND/OR/NOT
// are case-SENSITIVE uppercase; lowercase `and` is an ordinary term.

#[derive(Debug, Clone, PartialEq)]
enum QTok {
    Word(String),
    Star,
    Caret,
    Colon,
    LParen,
    RParen,
    LBrace,
    RBrace,
}

fn lex_query(s: &str) -> Result<Vec<QTok>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '"' => {
                return Err(Error::Bind(
                    "FTS phrase queries (\"…\") are not supported yet (stage 2)".into(),
                ))
            }
            '+' => {
                return Err(Error::Bind(
                    "FTS phrase concatenation (+) is not supported yet (stage 2)".into(),
                ))
            }
            '*' => {
                out.push(QTok::Star);
                chars.next();
            }
            '^' => {
                out.push(QTok::Caret);
                chars.next();
            }
            ':' => {
                out.push(QTok::Colon);
                chars.next();
            }
            '(' => {
                out.push(QTok::LParen);
                chars.next();
            }
            ')' => {
                out.push(QTok::RParen);
                chars.next();
            }
            '{' => {
                out.push(QTok::LBrace);
                chars.next();
            }
            '}' => {
                out.push(QTok::RBrace);
                chars.next();
            }
            _ if c.is_alphanumeric() => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() {
                        w.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(QTok::Word(w));
            }
            // Any other character (whitespace, `-`, `.`, `_`, …) separates terms.
            _ => {
                chars.next();
            }
        }
    }
    Ok(out)
}

struct QParser<'a> {
    toks: Vec<QTok>,
    pos: usize,
    tokenizer: Tokenizer,
    table: &'a TableDef,
    default_columns: &'a [u16],
    depth: usize,
}

/// Parse `s` into a compiled query tree. Terms are normalized by `tokenizer`;
/// bare terms inherit `default_columns` (the scan-level restriction), which
/// `col:`/`{a b}:` filters override.
fn parse_query(
    s: &str,
    tokenizer: Tokenizer,
    table: &TableDef,
    default_columns: &[u16],
) -> Result<FtsQuery> {
    let toks = lex_query(s)?;
    if toks.is_empty() {
        // Matches sqlite's "fts5: syntax error near \"\"".
        return Err(Error::Bind("fts5: syntax error (empty MATCH query)".into()));
    }
    let mut p = QParser { toks, pos: 0, tokenizer, table, default_columns, depth: 0 };
    let q = p.or_expr()?;
    if p.pos != p.toks.len() {
        return Err(Error::Bind("fts5: syntax error in MATCH query".into()));
    }
    // The decoder caps the TOTAL node count at MAX_FTS_DEPTH (one budget unit per
    // node); parsing above only bounds paren nesting, so a flat `a b c …` /
    // `a OR b OR …` chain could bind here yet fail to decode in another process,
    // publishing an undecodable "poison" plan to the shared registry. Enforce the
    // same total-node cap at bind so every accepted query round-trips.
    if fts_node_count(&q) > crate::plan::MAX_FTS_DEPTH {
        return Err(Error::Bind(
            "fts5: MATCH query too large (too many terms/operators)".into(),
        ));
    }
    Ok(q)
}

/// Total node count of an FTS query tree (leaves + operators), counted
/// ITERATIVELY so a long left-leaning chain cannot overflow the stack. Matches
/// the decoder's per-node `budget`, so a bind-accepted query is decode-accepted.
fn fts_node_count(root: &FtsQuery) -> usize {
    let mut stack = vec![root];
    let mut n = 0usize;
    while let Some(q) = stack.pop() {
        n += 1;
        match q {
            FtsQuery::And(a, b) | FtsQuery::Or(a, b) | FtsQuery::AndNot(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            FtsQuery::Term(_) => {}
        }
    }
    n
}

impl QParser<'_> {
    fn peek(&self) -> Option<&QTok> {
        self.toks.get(self.pos)
    }

    /// Is the current token the uppercase operator keyword `kw` (`AND`/`OR`/`NOT`)?
    fn peek_op(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(QTok::Word(w)) if w == kw)
    }

    /// Can the current token START a primary (so juxtaposition = implicit AND)?
    fn peek_primary_start(&self) -> bool {
        match self.peek() {
            Some(QTok::LParen) | Some(QTok::LBrace) | Some(QTok::Caret) => true,
            Some(QTok::Word(w)) => w != "AND" && w != "OR" && w != "NOT",
            _ => false,
        }
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > crate::plan::MAX_FTS_DEPTH {
            return Err(Error::Bind("MATCH query nests too deeply".into()));
        }
        Ok(())
    }

    fn or_expr(&mut self) -> Result<FtsQuery> {
        self.enter()?;
        let mut e = self.and_expr()?;
        while self.peek_op("OR") {
            self.pos += 1;
            let rhs = self.and_expr()?;
            e = FtsQuery::Or(Box::new(e), Box::new(rhs));
        }
        self.depth -= 1;
        Ok(e)
    }

    fn and_expr(&mut self) -> Result<FtsQuery> {
        let mut e = self.not_expr()?;
        loop {
            if self.peek_op("AND") {
                self.pos += 1;
                let rhs = self.not_expr()?;
                e = FtsQuery::And(Box::new(e), Box::new(rhs));
            } else if self.peek_primary_start() {
                // Juxtaposition is an implicit AND.
                let rhs = self.not_expr()?;
                e = FtsQuery::And(Box::new(e), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> Result<FtsQuery> {
        let mut e = self.primary()?;
        while self.peek_op("NOT") {
            self.pos += 1;
            let rhs = self.primary()?;
            e = FtsQuery::AndNot(Box::new(e), Box::new(rhs));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<FtsQuery> {
        if matches!(self.peek(), Some(QTok::Word(w)) if w == "NEAR") {
            return Err(Error::Bind(
                "FTS NEAR queries are not supported yet (stage 2)".into(),
            ));
        }
        if matches!(self.peek(), Some(QTok::LParen)) {
            self.pos += 1;
            let inner = self.or_expr()?;
            match self.peek() {
                Some(QTok::RParen) => {
                    self.pos += 1;
                    return Ok(inner);
                }
                _ => return Err(Error::Bind("fts5: unbalanced parenthesis in MATCH query".into())),
            }
        }
        // Optional column filter.
        let columns = self.column_filter()?;
        // Optional initial-token anchor.
        let initial = if matches!(self.peek(), Some(QTok::Caret)) {
            self.pos += 1;
            true
        } else {
            false
        };
        // The term word.
        let raw = match self.peek() {
            Some(QTok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                w
            }
            _ => return Err(Error::Bind("fts5: expected a term in MATCH query".into())),
        };
        // Optional prefix marker.
        let prefix = if matches!(self.peek(), Some(QTok::Star)) {
            self.pos += 1;
            true
        } else {
            false
        };
        let token = fts::normalize_term(self.tokenizer, &raw)
            .ok_or_else(|| Error::Bind(format!("MATCH term `{raw}` has no searchable token")))?;
        Ok(FtsQuery::Term(FtsTerm { token, prefix, initial, columns }))
    }

    /// Parse an optional `col:` or `{a b …}:` prefix, returning the FTS column
    /// ordinals it restricts to; a bare term inherits `default_columns`.
    fn column_filter(&mut self) -> Result<Vec<u16>> {
        // `{ a b }:` form.
        if matches!(self.peek(), Some(QTok::LBrace)) {
            self.pos += 1;
            let mut cols = Vec::new();
            loop {
                match self.peek() {
                    Some(QTok::Word(w)) => {
                        let name = w.clone();
                        self.pos += 1;
                        cols.push(self.resolve_col(&name)?);
                    }
                    Some(QTok::RBrace) => break,
                    _ => return Err(Error::Bind("fts5: bad column filter in MATCH query".into())),
                }
            }
            self.pos += 1; // RBrace
            if !matches!(self.peek(), Some(QTok::Colon)) {
                return Err(Error::Bind("fts5: expected `:` after column filter".into()));
            }
            self.pos += 1; // Colon
            if cols.is_empty() {
                return Err(Error::Bind("fts5: empty column filter".into()));
            }
            return Ok(cols);
        }
        // `col:` form — a Word immediately followed by a Colon.
        if let Some(QTok::Word(w)) = self.peek() {
            if matches!(self.toks.get(self.pos + 1), Some(QTok::Colon)) {
                let name = w.clone();
                self.pos += 2; // Word Colon
                return Ok(vec![self.resolve_col(&name)?]);
            }
        }
        Ok(self.default_columns.to_vec())
    }

    fn resolve_col(&self, name: &str) -> Result<u16> {
        content_colno(self.table, name)
            .ok_or_else(|| Error::Bind(format!("no such column: {name}")))
    }
}
