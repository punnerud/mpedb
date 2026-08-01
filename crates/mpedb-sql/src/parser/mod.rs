//! Recursive-descent parser for the Phase 1 SQL subset.
//!
//! Precedence, loosest to tightest:
//! `OR` < `AND` < `NOT` < comparison / `IS [NOT] NULL` / `LIKE`
//! < `+ -` < `* / %` < unary `-` < primary.
//! Comparisons do not chain (`a < b < c` is a parse error).
//!
//! This file holds the [`Parser`] struct, the parse-time limits, the parse
//! entry points, the shared token-navigation helpers and the top-level
//! statement dispatch. The grammar productions live in sibling submodules that
//! reach those helpers via `super` (descendant visibility, the same mechanism
//! [`ddl`] uses): [`select`] (SELECT / compound / FROM / JOIN and the
//! standalone VALUES statement), [`expr`] (the expression tier and its
//! suffixes) and [`dml`] (INSERT / UPDATE / DELETE).

use crate::ast::{Expr, Stmt};
use crate::token::{tokenize, Kw, SpTok, Tok};
use mpedb_types::{Error, Result};

mod ddl;
mod dml;
mod expr;
mod select;
pub(crate) use ddl::parse_ddl;

#[cfg(test)]
mod tests;

/// Parser stack budget, in bytes.
///
/// The grammar is recursive descent, so hostile SQL (or a hostile CHECK source
/// reaching [`parse_expr_only`] at attach time) can overflow the thread stack
/// and abort the process — uncatchable. Something must stop it.
///
/// **Measure the stack, do not count the nodes.** This started as a node count
/// (`MAX_EXPR_DEPTH`), which is a proxy for the thing that actually runs out,
/// and the proxy broke twice: adding CASE made one level cost ~20 KB instead of
/// a few hundred bytes, so a count tuned for parenthesised arithmetic silently
/// stopped fitting the stack, and a count re-tuned for CASE would have punished
/// cheap constructs for the expensive one's appetite. Measured on this grammar
/// in a debug build: nested parens cost well under 1 KB per level, nested CASE
/// about 20 KB.
///
/// PostgreSQL solves it this way too (`check_stack_depth()` against
/// `max_stack_depth`, default 2 MB), and the difference is visible:
///
/// | nested parens | nested CASE |
/// |---|---|
/// | sqlite3: 93 (errors, does not crash) | sqlite3: **18** |
/// | PostgreSQL: 500+ | PostgreSQL: bounded by real stack use |
///
/// A byte budget gives both: thousands of cheap levels, and a stop long before
/// an expensive one exhausts the stack — and it re-tunes itself for free when a
/// release build makes every frame smaller, or when a future construct makes one
/// fatter.
///
/// 1 MiB is half the 2 MiB Rust gives a spawned thread, so there is headroom for
/// whatever called us. Measured, both builds, because quoting only one would
/// mislead — a debug build pays for every local, a release build keeps them in
/// registers and puts CASE's arm vector on the heap:
///
/// | nested construct | mpedb (release) | mpedb (debug) | sqlite3 3.45 | PostgreSQL 16 |
/// |---|---|---|---|---|
/// | parens | 457 | ~84 | 93 | 500+ |
/// | CASE | 457 | ~68 | **18** | 500+ |
///
/// So: past sqlite on both shapes in the build that ships, still safe in the
/// build that does not — and, unlike a fixed node count, it re-tunes itself
/// when frames change instead of quietly becoming a lie.
const MAX_PARSER_STACK: usize = 1024 * 1024;

/// Hard ceiling on nesting regardless of stack cost.
///
/// The byte budget is the real guard; this is a backstop for a pathological
/// grammar path whose frames are so small that a hostile input could build a
/// gigantic AST while staying under the budget. Deliberately far above anything
/// legitimate — and above both ancestors' limits.
const MAX_EXPR_DEPTH: u32 = 2000;

/// Parse-time item caps. Plan wire counts are serialized as `u16`
/// ([`crate::plan`]); these caps keep every count far away from the
/// truncation edge (and bound memory for hostile statements). They are
/// re-validated on the decode side — keep in sync with
/// `CompiledPlan::decode` (plan.rs).
pub(crate) const MAX_SELECT_ITEMS: usize = 4096;
/// Ceiling on compound SELECT arms — must not exceed the plan decoder's
/// `MAX_COMPOUND_ARMS` (both are 64; the corpus' longest chain is 9).
const MAX_COMPOUND_ARMS: usize = 64;
pub(crate) const MAX_ORDER_BY_ITEMS: usize = 64;
pub(crate) const MAX_SET_ITEMS: usize = 1024;
/// Tables in one FROM — sqlite's own limit, and the width of the join solver's
/// state mask. Unlike its neighbours this one is not headroom against the wire
/// format: it is the point past which there is no plan worth making, so the
/// refusal is the answer. See the call site in `parser::select`.
pub(crate) const MAX_FROM_TABLES: usize = 64;

/// Parse a complete statement. Returns the AST, whether it was wrapped in
/// `EXPLAIN`, and the number of parameters ($n gives max n; `?` are numbered
/// left-to-right in statement order).
pub(crate) fn parse_statement(sql: &str) -> Result<(Stmt, bool, u16)> {
    let (stmt, is_explain, n_params, ctes) =
        parse_statement_ctes(sql, &[], &[], &crate::binder::OpSet::default())?;
    if !ctes.is_empty() {
        return Err(Error::Bind(
            "WITH (common table expressions) is only handled by the top-level \
             compile path, not here"
                .into(),
        ));
    }
    Ok((stmt, is_explain, n_params))
}

/// `WITH` CTE definitions: each `(name, body-source-text)`, re-parsed and
/// flattened like a view at reference time (#CTE).
pub(crate) type CteDefs = Vec<(String, String)>;

/// Does `sql` contain a POSITIONAL `?` parameter?
///
/// Tokenized rather than searched: a `?` inside a string literal is not a
/// parameter, and a text scan cannot tell the two apart.
pub fn has_question_param(sql: &str) -> Result<bool> {
    Ok(tokenize(sql)?.iter().any(|t| t.tok == Tok::Question))
}

/// `VALUES (a, b), (c, d)` → `SELECT a, b UNION ALL SELECT c, d`.
///
/// `None` when the source does not start with `VALUES`, or when the row groups
/// are not the plain parenthesised lists this can rewrite — in which case the
/// body is left exactly as written and refuses downstream as it did before.
fn values_body_to_select(src: &str) -> Option<String> {
    let toks = tokenize(src).ok()?;
    let first = toks.first()?;
    let is_values = match &first.tok {
        Tok::Kw(Kw::Values) => true,
        Tok::Ident(w) => w.eq_ignore_ascii_case("VALUES"),
        _ => false,
    };
    if !is_values {
        return None;
    }
    // Each row is a depth-0 `( … )` group; anything between the groups other
    // than a comma means this is not the simple shape.
    let mut rows: Vec<&str> = Vec::new();
    let mut i = 1usize;
    while i < toks.len() {
        if !matches!(toks[i].tok, Tok::LParen) {
            return None;
        }
        let open = i;
        let mut depth = 0usize;
        let close = loop {
            match toks.get(i)?.tok {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break i;
                    }
                }
                _ => {}
            }
            i += 1;
        };
        let inner = src.get(toks[open].pos + 1..toks[close].pos)?.trim();
        if inner.is_empty() {
            return None;
        }
        rows.push(inner);
        i = close + 1;
        match toks.get(i).map(|t| &t.tok) {
            None => break,
            Some(Tok::Comma) => i += 1,
            // Trailing text (an ORDER BY, a LIMIT) — not this shape.
            Some(_) => return None,
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(
        rows.iter()
            .map(|r| format!("SELECT {r}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL "),
    )
}


/// Rewrite `SELECT a, b FROM t` into `SELECT a AS x, b AS y FROM t` — a CTE's
/// explicit column list, applied to the body source.
///
/// Depth-aware, so a comma inside a function call or a nested subquery does not
/// split an item, and it stops at the first depth-0 clause keyword after the
/// select list. A COMPOUND body is aliased on its FIRST arm, which is where SQL
/// takes a compound's column names from.
///
/// `Err` (a message, not a parse error — the caller positions it) for the two
/// shapes this cannot rename: a `*` item, whose arity is unknown until the
/// schema is in hand, and a count that disagrees with the list.
fn alias_select_items(src: &str, names: &[String]) -> std::result::Result<String, String> {
    let toks = tokenize(src).map_err(|e| format!("body does not tokenize: {e}"))?;
    if !matches!(toks.first().map(|t| &t.tok), Some(Tok::Kw(Kw::Select))) {
        return Err("body must be a SELECT".into());
    }
    // Where each item starts, and where the select list ends.
    let end_of = |i: usize| toks.get(i).map_or(src.len(), |t: &SpTok| t.pos);
    let mut starts = vec![1usize];
    let mut list_end = toks.len();
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(1) {
        match &t.tok {
            Tok::LParen => depth += 1,
            Tok::RParen => depth = depth.saturating_sub(1),
            Tok::Comma if depth == 0 => starts.push(i + 1),
            // The select list ends at the first depth-0 clause keyword. `AS`
            // is not one — an item may already carry an alias, which the
            // declared name then replaces. UNION/INTERSECT/EXCEPT are
            // positional words rather than keyword tokens, so they are matched
            // by text, and they matter: a compound body takes its column names
            // from the FIRST arm, which is the arm being aliased.
            Tok::Kw(Kw::From | Kw::Where | Kw::Group | Kw::Order | Kw::Limit)
                if depth == 0 =>
            {
                list_end = i;
                break;
            }
            Tok::Ident(w)
                if depth == 0
                    && ["UNION", "INTERSECT", "EXCEPT"]
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k)) =>
            {
                list_end = i;
                break;
            }
            _ => {}
        }
    }
    // Item i spans [start_i, next boundary) — the comma before the next item,
    // or the end of the list.
    let mut items: Vec<&str> = Vec::with_capacity(starts.len());
    for (n, &st) in starts.iter().enumerate() {
        let mut end_tok = starts.get(n + 1).map_or(list_end, |&s| s - 1);
        // An item may ALREADY carry an alias — `SELECT some_table.id AS id` is
        // how every ORM writes one — and the declared name replaces it. Left
        // in place it produced `id AS id AS "p"`, two aliases on one item,
        // which is a parse error rather than a rename.
        if end_tok >= st + 2
            && matches!(toks.get(end_tok - 2).map(|t| &t.tok), Some(Tok::Kw(Kw::As)))
        {
            end_tok -= 2;
        }
        let text = src[end_of(st)..end_of(end_tok)].trim();
        if text.is_empty() {
            return Err("an empty select-list item".into());
        }
        items.push(text);
    }
    if items.len() != names.len() {
        return Err(format!(
            "declares {} column(s) but the body returns {}",
            names.len(),
            items.len()
        ));
    }
    if items.iter().any(|t| t.ends_with('*')) {
        return Err("a `*` item cannot be renamed positionally".into());
    }
    let mut out = String::with_capacity(src.len() + names.len() * 8);
    out.push_str(&src[..end_of(1)]);
    for (n, (item, name)) in items.iter().zip(names).enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        // An item may already end in `AS <alias>`; the declared name replaces
        // it, and a second `AS` after the first is what the parser reads last.
        out.push_str(item);
        out.push_str(" AS ");
        out.push('"');
        out.push_str(&name.replace('"', "\"\""));
        out.push('"');
    }
    out.push(' ');
    out.push_str(&src[end_of(list_end)..]);
    Ok(out)
}


/// Like [`parse_statement`] but also returns any leading `WITH` CTE definitions
/// as `(name, body-source-text)` pairs (#CTE). The caller folds them into the
/// view catalog so `crate::view::inline_views` flattens a `FROM cte` reference
/// exactly as it flattens a view — no planner/plan-bytes/executor change.
pub(crate) fn parse_statement_ctes(
    sql: &str,
    // HOST aggregate `(name, n_arg)` registrations (design/DESIGN-UDF.md stage
    // 2); `&[]` for every caller that has none.
    host_aggs: &[(String, i32)],
    // The subset of `host_aggs` registered with sqlite's WINDOW protocol
    // (`create_window_function`: xValue + xInverse as well as xStep/xFinal).
    // Only these may take an OVER clause; `&[]` for every caller with none.
    window_aggs: &[String],
    // Custom operators (stage M3, SQL-EXTENSIONS.md); default for callers
    // with none — a `:sym:` token then refuses by name.
    ops: &crate::binder::OpSet,
) -> Result<(Stmt, bool, u16, CteDefs)> {
    let toks = tokenize(sql)?;
    let mut p = Parser::new(sql, toks);
    p.host_aggs = host_aggs.to_vec();
    p.window_aggs = window_aggs.to_vec();
    p.ops = ops.clone();
    p.statement_tail()
}

impl<'a> Parser<'a> {
    /// The whole statement pipeline after catalog setup: EXPLAIN, statement
    /// operators, WITH RECURSIVE, CTE prefix, the statement itself. A method
    /// (not a free function) so a STATEMENT OPERATOR's expansion re-enters it
    /// on a sub-parser with the same catalogs and a bumped depth.
    fn statement_tail(&mut self) -> Result<(Stmt, bool, u16, CteDefs)> {
    let is_explain = if self.eat_kw(Kw::Explain) {
        if self.peek_kw(Kw::Explain) {
            return Err(self.err_here("EXPLAIN cannot be nested"));
        }
        true
    } else {
        false
    };
    // A STATEMENT operator (fixity bit 4, SQL-EXTENSIONS.md): `:graph: <rest>`
    // swallows the remainder of the source as ONE raw operand and its macro
    // returns a complete statement — a user-defined sub-LANGUAGE fronting SQL.
    // The expansion re-enters this same pipeline (operator catalog included,
    // depth-capped), so a language's output may itself use `:op:` forms.
    if let Some(Tok::CustomOp(sym)) = self.peek().cloned() {
        let fixity = self.op_fixity(&sym)?;
        if fixity != 4 {
            return Err(self.err_here(format!(
                ":{sym}: is an expression operator and cannot begin a statement —                  see SQL-EXTENSIONS.md"
            )));
        }
        self.pos += 1;
        let rest = self.src[self.cur_byte()..].trim();
        // Consume the raw tail: the macro owns its own syntax from here.
        self.pos = self.toks.len();
        let fragment = self.ops.expand(&sym, &[rest])?;
        if self.op_depth >= 8 {
            return Err(Error::Parse {
                pos: 0,
                msg: format!(":{sym}: statement expansion is nested more than 8 levels deep"),
            });
        }
        let toks = crate::token::tokenize(&fragment).map_err(|e| Error::Parse {
            pos: 0,
            msg: format!(":{sym}: expanded to text that does not lex: {e}"),
        })?;
        let mut sub = Parser::new(&fragment, toks);
        sub.ops = self.ops.clone();
        sub.op_depth = self.op_depth + 1;
        sub.host_aggs = self.host_aggs.clone();
        sub.window_aggs = self.window_aggs.clone();
        sub.style = self.style;
        sub.next_question = self.next_question;
        sub.max_params = self.max_params;
        let (stmt, inner_explain, _n, ctes) = sub.statement_tail().map_err(|e| Error::Parse {
            pos: 0,
            msg: format!(":{sym}: expanded to a statement that does not parse: {e}"),
        })?;
        self.next_question = sub.next_question;
        self.max_params = self.max_params.max(sub.max_params);
        let n_params = self.n_params()?;
        return Ok((stmt, is_explain || inner_explain, n_params, ctes));
    }
    // `WITH RECURSIVE …` is a wholly different mechanism from a non-recursive
    // `WITH` (a fixpoint, not bind-time flattening), so it is parsed here into a
    // single `Stmt::RecursiveCte` rather than a `(name, body)` CTE list.
    let is_recursive_with = matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("WITH"))
        && matches!(self.peek_at(1), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("RECURSIVE"));
    if is_recursive_with {
        let stmt = self.recursive_cte_stmt()?;
        self.eat(&Tok::Semicolon);
        self.expect_eof()?;
        let n_params = self.n_params()?;
        return Ok((stmt, is_explain, n_params, Vec::new()));
    }
    let ctes = self.with_prefix()?;
    let stmt = self.statement()?;
    self.eat(&Tok::Semicolon);
    self.expect_eof()?;
    let n_params = self.n_params()?;
    Ok((stmt, is_explain, n_params, ctes))
    }
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
    /// Approximate stack address where parsing began; the byte budget is
    /// measured against it (see [`Parser::enter_expr`]).
    stack_base: usize,
    /// HOST-registered AGGREGATE `(name, n_arg)` pairs visible to the compiling
    /// connection (design/DESIGN-UDF.md stage 2). The parser needs them because
    /// `myagg(x)` must take the AGGREGATE grammar branch — the one that also
    /// accepts `DISTINCT` and a trailing `FILTER (WHERE …)` — and that decision
    /// is made before the argument list is read. Empty for every caller that
    /// registered none, so the grammar is bit-for-bit unchanged for them.
    host_aggs: Vec<(String, i32)>,
    /// Host aggregates that ALSO carry the window protocol (see
    /// `parse_statement_ctes`). A subset of `host_aggs` by name.
    window_aggs: Vec<String>,
    /// Custom-operator catalog (stage M3): fixity map + the macro expander.
    /// Empty for callers with no database — a `:sym:` token then refuses by
    /// name. Owned (cloned once per parse) so fragment sub-parsers share it.
    ops: crate::binder::OpSet,
    /// Macro-expansion nesting depth: an operator whose expansion reaches
    /// another operator recurses here; the cap turns a self-expanding
    /// definition into a deterministic refusal instead of a stack overflow.
    op_depth: u8,
    /// The core just parsed by `select_core` carried a NEGATIVE `LIMIT`/`OFFSET`
    /// — sqlite's "no limit" / "no skip" idiom, which the AST spells as the
    /// clause being absent. `compound_chain` still has to reject it before a
    /// set operator (`LIMIT` binds to the whole compound), and `Option::is_some`
    /// can no longer see it. Written by every `select_core`, read immediately
    /// after by `compound_chain` — nothing parses a core in between.
    neg_limit_in_core: bool,
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
            host_aggs: Vec::new(),
            window_aggs: Vec::new(),
            ops: crate::binder::OpSet::default(),
            op_depth: 0,
            neg_limit_in_core: false,
            stack_base: {
                let probe = 0u8;
                &probe as *const u8 as usize
            },
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

    // ---- token plumbing ----------------------------------------------

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n).map(|t| &t.tok)
    }

    fn here(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|t| t.pos)
            .unwrap_or(self.src.len())
    }

    /// The byte offset of the CURRENT token's first character (end of input
    /// when exhausted) — the seam operand-text capture cuts on.
    fn cur_byte(&self) -> usize {
        self.toks.get(self.pos).map(|t| t.pos).unwrap_or(self.src.len())
    }

    /// Fixity of a custom operator, or the discoverable refusal.
    fn op_fixity(&self, sym: &str) -> Result<u8> {
        self.ops.fixity(sym).ok_or_else(|| {
            self.err_here(format!(
                "unknown operator :{sym}: — see SQL-EXTENSIONS.md, or `mpedb op list`"
            ))
        })
    }

    /// Run a custom operator's macro over its operands' SOURCE TEXT and parse
    /// the expansion in place (stage M3). The expansion is an ordinary
    /// expression: every bind rule applies to it afterwards, so a macro
    /// cannot smuggle anything past a refusal. Parameter numbering carries
    /// through the sub-parse (an operand containing `$1`/`?` keeps its slot),
    /// and the depth cap turns a self-expanding operator into a deterministic
    /// refusal instead of a stack overflow.
    fn expand_custom_op(&mut self, sym: &str, operands: &[String]) -> Result<Expr> {
        if self.op_depth >= 8 {
            return Err(Error::Parse {
                pos: self.cur_byte(),
                msg: format!(
                    ":{sym}: expansion is nested more than 8 levels deep —                      an operator must not expand into itself"
                ),
            });
        }
        let refs: Vec<&str> = operands.iter().map(String::as_str).collect();
        let fragment = self.ops.expand(sym, &refs)?;
        let toks = crate::token::tokenize(&fragment).map_err(|e| {
            Error::Parse {
                pos: self.cur_byte(),
                msg: format!(":{sym}: expanded to text that does not lex: {e}"),
            }
        })?;
        let mut sub = Parser::new(&fragment, toks);
        sub.ops = self.ops.clone();
        sub.op_depth = self.op_depth + 1;
        sub.host_aggs = self.host_aggs.clone();
        sub.window_aggs = self.window_aggs.clone();
        sub.style = self.style;
        sub.next_question = self.next_question;
        sub.max_params = self.max_params;
        let e = sub.expr().map_err(|err| Error::Parse {
            pos: self.cur_byte(),
            msg: format!(":{sym}: expanded to `{fragment}`, which does not parse: {err}"),
        })?;
        if sub.pos != sub.toks.len() {
            return Err(Error::Parse {
                pos: self.cur_byte(),
                msg: format!(
                    ":{sym}: expanded to `{fragment}`, which has trailing tokens                      after the expression"
                ),
            });
        }
        // Parameter numbering flows back so the statement's count is right.
        self.style = sub.style;
        self.next_question = sub.next_question;
        self.max_params = self.max_params.max(sub.max_params);
        Ok(e)
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

    fn peek_not_glob(&self) -> bool {
        matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Kw(Kw::Not)))
            && matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Kw(Kw::Glob)))
    }

    fn peek_not_like(&self) -> bool {
        matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Kw(Kw::Not)))
            && matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Kw(Kw::Like)))
    }

    /// The `[ASC|DESC] [NULLS FIRST|NULLS LAST]` tail of one ORDER BY term.
    ///
    /// sqlite's default placement is a function of the direction — NULLs first
    /// for ASC, last for DESC — and `NULLS FIRST`/`NULLS LAST` overrides it
    /// independently of the direction, including on a term with no explicit
    /// ASC/DESC (`ORDER BY s NULLS LAST` is ASC with NULLs last).
    ///
    /// Neither `NULLS` nor `FIRST`/`LAST` is a reserved word. That is safe HERE
    /// and nowhere else: an ORDER BY term has just been parsed, so a following
    /// bare `nulls` cannot be a column or an alias — which is exactly why the
    /// unconsumed token used to resurface as whatever the ENCLOSING construct
    /// wanted next (`expected ) after IN subquery`) and send gap reports
    /// chasing the paren.
    fn sort_dir(&mut self) -> Result<crate::plan::SortDir> {
        let desc = if self.eat_kw(Kw::Desc) {
            true
        } else {
            self.eat_kw(Kw::Asc);
            false
        };
        let nulls_first = if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("NULLS"))
        {
            self.pos += 1;
            match self.peek() {
                Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("FIRST") => {
                    self.pos += 1;
                    Some(true)
                }
                Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("LAST") => {
                    self.pos += 1;
                    Some(false)
                }
                _ => return Err(self.err_here("expected FIRST or LAST after NULLS")),
            }
        } else {
            None
        };
        Ok(crate::plan::SortDir::new(desc, nulls_first))
    }

    /// The argument of `LIKE … ESCAPE <c>`, already past the `ESCAPE` word.
    ///
    /// sqlite takes an arbitrary EXPRESSION here and raises `ESCAPE expression
    /// must be a single character` at step time for anything that is not a
    /// one-character string. mpedb accepts only the literal — the same
    /// restriction the LIKE pattern itself already carries — and refuses the
    /// rest at PREPARE time, by name. In particular sqlite's coercions
    /// (`ESCAPE 5` ≡ `ESCAPE '5'`) are NOT imitated: a clean refusal beats a
    /// second, silently different set of numeric-to-text rules.
    fn escape_char(&mut self) -> Result<char> {
        let s = match self.peek() {
            Some(Tok::Str(s)) => s.clone(),
            _ => {
                return Err(self.err_here(
                    "ESCAPE requires a single-character string literal, e.g. ESCAPE '\\'",
                ))
            }
        };
        self.pos += 1;
        let mut it = s.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Ok(c),
            // sqlite's own wording, so a Django/ORM user sees what sqlite says.
            _ => Err(self.err_here("ESCAPE expression must be a single character")),
        }
    }

    fn peek_not_regexp(&self) -> bool {
        matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Kw(Kw::Not)))
            && matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Kw(Kw::Regexp)))
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

    // ---- word / identifier helpers (shared with parser::ddl) ---------

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

    /// Identifier (bare or quoted).
    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(..)) => {
                match self.advance() {
                    Some(Tok::Ident(s)) | Some(Tok::QuotedIdent(s, _)) => Ok(s),
                    _ => unreachable!(),
                }
            }
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }

    // ---- statements ---------------------------------------------------

    /// Parse an optional leading `WITH [RECURSIVE] name AS ( body ) [, …]`
    /// prefix (#CTE), returning each CTE as `(name, body-source-text)`. `WITH`
    /// and `RECURSIVE` are positional words (not keywords), so a table/column
    /// named `with` is unaffected. Each body is captured verbatim between its
    /// parentheses — re-parsed and flattened like a view at reference time — so
    /// the body's own `$n`/`?` params never touch the outer parameter counter.
    fn with_prefix(&mut self) -> Result<CteDefs> {
        if !self.eat_word("WITH") {
            return Ok(Vec::new());
        }
        if self.eat_word("RECURSIVE") {
            return Err(self.err_here("WITH RECURSIVE is not supported yet"));
        }
        let mut ctes = Vec::new();
        loop {
            let name = self.ident("a CTE name after WITH")?;
            // `WITH c(x, y) AS (SELECT a, b FROM t)` IS
            // `WITH c AS (SELECT a AS x, b AS y FROM t)` — the column list is
            // positional renaming and nothing else. Applying it to the captured
            // body here means the flattener, the binder and every downstream
            // stage see a body whose output names are already the declared
            // ones, with no catalog type or signature change anywhere.
            let mut cols: Vec<String> = Vec::new();
            if self.eat(&Tok::LParen) {
                loop {
                    cols.push(self.ident("a column name in the CTE column list")?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)` closing the CTE column list")?;
                if cols.is_empty() {
                    return Err(self.err_here("a CTE column list must not be empty"));
                }
            }
            self.expect_kw(Kw::As, "AS after the CTE name")?;
            let body = self.capture_paren_source()?;
            // `WITH c(a, b) AS (VALUES (1, 2), (3, 4))` — a VALUES body is a
            // compound of constant-row SELECTs and nothing more, so it is
            // rewritten into one before anything else looks at it. SQLAlchemy
            // writes this for a literal-rows CTE.
            let body = values_body_to_select(&body).unwrap_or(body);
            let body = if cols.is_empty() {
                body
            } else {
                alias_select_items(&body, &cols).map_err(|msg| Error::Parse {
                    pos: self.here(),
                    msg: format!("CTE `{name}` column list: {msg}"),
                })?
            };
            ctes.push((name, body));
            if ctes.len() > 32 {
                return Err(self.err_here("too many CTEs in one WITH (max 32)"));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(ctes)
    }

    /// Parse a `WITH RECURSIVE t(c1, …) AS (<anchor> UNION[ ALL] <recursive>)
    /// <outer>` statement (design/DESIGN-CTE-RECURSIVE.md stage 1). The body is
    /// captured verbatim and re-parsed as a 2-arm UNION compound — reusing the
    /// parser's own arm splitting rather than scanning for the operator, so a
    /// UNION nested in a subquery cannot miscount the arms. The body must be
    /// parameter-free (like a non-recursive CTE body); the OUTER statement's
    /// params flow through the main parser as usual.
    fn recursive_cte_stmt(&mut self) -> Result<Stmt> {
        self.eat_word("WITH"); // presence checked by the caller
        self.eat_word("RECURSIVE");
        let name = self.ident("a CTE name after WITH RECURSIVE")?;
        // The column list is REQUIRED for a recursive CTE (sqlite enforces it).
        self.expect(&Tok::LParen, "a `(column, …)` list — required for a recursive CTE")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.ident("a column name in the recursive CTE column list")?);
            if columns.len() > 1024 {
                return Err(self.err_here("too many columns in the recursive CTE list"));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)` to close the recursive CTE column list")?;
        self.expect_kw(Kw::As, "AS after the recursive CTE column list")?;
        let body_src = self.capture_paren_source()?;
        // Re-parse the body as its own statement: it must be a 2-arm
        // UNION / UNION ALL compound, parameter-free.
        let (body_stmt, body_explain, body_params) = parse_statement(&body_src)?;
        if body_explain {
            return Err(self.err_here("EXPLAIN is not allowed inside a recursive CTE body"));
        }
        // Parameters in the body, on the same rule as an ordinary CTE's: `$n`
        // is ABSOLUTE, so the body's indices already are the caller's — the
        // statement's slot count just has to cover them, and the parser owns
        // that count here. `?` is POSITIONAL and the re-parse numbers the
        // body's from zero, colliding with the outer statement's own; refused
        // by name, because answering it would bind the wrong values.
        if has_question_param(&body_src)? {
            return Err(self.err_here(
                "a recursive CTE body uses `?` parameters, which are numbered by                  position and would collide with the outer statement's; use                  `$1`-style numbering, which is absolute",
            ));
        }
        self.max_params = self.max_params.max(body_params as u32);
        let comp = match body_stmt {
            Stmt::Compound(c) => c,
            _ => {
                return Err(self.err_here(
                    "a recursive CTE body must be `<anchor> UNION [ALL] <recursive>`",
                ))
            }
        };
        if comp.arms.len() != 2 {
            return Err(self.err_here(
                "stage 1 supports exactly one anchor and one recursive term \
                 (a single UNION [ALL])",
            ));
        }
        if !comp.order_by.is_empty() || comp.limit.is_some() || comp.offset.is_some() {
            return Err(self.err_here(
                "ORDER BY / LIMIT inside a recursive CTE body is not supported yet \
                 (stage 1 is breadth-first FIFO)",
            ));
        }
        let union_all = match comp.ops[0] {
            crate::plan::SetOp::Union => false,
            crate::plan::SetOp::UnionAll => true,
            _ => {
                return Err(self.err_here(
                    "a recursive CTE must combine its terms with UNION or UNION ALL",
                ))
            }
        };
        let mut arms = comp.arms;
        let recursive = arms.pop().expect("two arms");
        let anchor = arms.pop().expect("two arms");
        // Stage 1: exactly one recursive CTE (no mutual / multi-CTE recursion).
        if self.eat(&Tok::Comma) {
            return Err(self.err_here(
                "multiple / mutually recursive CTEs in one WITH RECURSIVE are not \
                 supported yet (stage 1: a single recursive CTE)",
            ));
        }
        let outer = self.statement()?;
        Ok(Stmt::RecursiveCte(crate::ast::RecursiveCteStmt {
            name,
            columns,
            union_all,
            anchor: Box::new(anchor),
            recursive: Box::new(recursive),
            outer: Box::new(outer),
        }))
    }

    fn statement(&mut self) -> Result<Stmt> {
        match self.peek() {
            Some(Tok::Kw(Kw::Select)) => self.select_stmt(),
            Some(Tok::Kw(Kw::Values)) => self.values_stmt(),
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
                // `ROLLBACK [TRANSACTION] [TO [SAVEPOINT] <name>]`. `TRANSACTION`,
                // `TO` and `SAVEPOINT` are positional words (not keywords), so a
                // column named `to`/`transaction`/`savepoint` is unaffected.
                self.eat_word("TRANSACTION");
                if self.eat_word("TO") {
                    self.eat_word("SAVEPOINT");
                    let name = self.savepoint_name("a savepoint name after ROLLBACK TO")?;
                    Ok(Stmt::RollbackTo(name))
                } else {
                    Ok(Stmt::Rollback)
                }
            }
            _ => {
                // Bare `REPLACE INTO …` is sqlite's alias for `INSERT OR REPLACE
                // INTO …`. Gated on a following `INTO` so a table/column named
                // `replace` (or the `replace()` scalar) is never mis-consumed.
                if matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("REPLACE"))
                    && matches!(self.peek_at(1), Some(Tok::Kw(Kw::Into)))
                {
                    self.pos += 1; // consume REPLACE; insert_body expects INTO next
                    return self.insert_body(Some(crate::ast::OnConflict::Replace));
                }
                // `SAVEPOINT`/`RELEASE` are positional words (like `WITH`), so a
                // table/column named `savepoint`/`release` is unaffected.
                if self.eat_word("SAVEPOINT") {
                    let name = self.savepoint_name("a savepoint name after SAVEPOINT")?;
                    Ok(Stmt::Savepoint(name))
                } else if self.eat_word("RELEASE") {
                    self.eat_word("SAVEPOINT");
                    let name = self.savepoint_name("a savepoint name after RELEASE")?;
                    Ok(Stmt::Release(name))
                } else {
                    Err(self.err_here(
                        "expected a statement (SELECT, VALUES, INSERT, UPDATE, DELETE, \
                         BEGIN, COMMIT, ROLLBACK, SAVEPOINT, RELEASE)",
                    ))
                }
            }
        }
    }

    /// A savepoint name: a bare/quoted identifier or a string literal (sqlite
    /// accepts all three). Comparison for RELEASE/ROLLBACK TO is
    /// case-insensitive (see the write session), matching sqlite.
    fn savepoint_name(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(..)) => self.ident(what),
            Some(Tok::Str(_)) => match self.advance() {
                Some(Tok::Str(s)) => Ok(s),
                _ => unreachable!(),
            },
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }
}
