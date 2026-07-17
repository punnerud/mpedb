//! Recursive-descent parser for the Phase 1 SQL subset.
//!
//! Precedence, loosest to tightest:
//! `OR` < `AND` < `NOT` < comparison / `IS [NOT] NULL` / `LIKE`
//! < `+ -` < `* / %` < unary `-` < primary.
//! Comparisons do not chain (`a < b < c` is a parse error).

use crate::ast::{
    BinOp, CompoundStmt, DeleteStmt, Expr, InsertStmt, JoinClause, JoinKind, OnConflict,
    SelectStmt, Stmt, UnOp, UpdateStmt,
};
use crate::plan::SetOp;
use crate::ddl::{CreatePolicySpec, DdlStmt, RlsAction};
use crate::token::{tokenize, Kw, SpTok, Tok};
use mpedb_types::{Error, PolicyCmd, Result, Value};

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
            if p.eat_word("TABLE") {
                p.parse_create_table()?
            } else {
                p.parse_create_policy()?
            }
        }
        Some("drop") => {
            p.advance();
            if p.eat_word("TABLE") {
                p.parse_drop_table()?
            } else {
                p.parse_drop_policy()?
            }
        }
        Some("alter") => {
            p.advance();
            p.parse_alter()?
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
    /// Approximate stack address where parsing began; the byte budget is
    /// measured against it (see [`Self::enter_expr`]).
    stack_base: usize,
}

/// The aggregate names, matched case-insensitively. Kept out of the scalar
/// function table on purpose: a scalar runs per row, an aggregate consumes a
/// group, and the parser must not let one become the other.
fn agg_fn(name: &str) -> Option<mpedb_types::AggFn> {
    use mpedb_types::AggFn::*;
    Some(match name.to_ascii_lowercase().as_str() {
        "count" => Count,
        "sum" => Sum,
        "avg" => Avg,
        "min" => Min,
        "max" => Max,
        _ => return None,
    })
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

    /// Enter one level of expression recursion, refusing to go deeper than the
    /// stack can hold.
    ///
    /// Reads the approximate stack pointer (the address of a local) and compares
    /// it to the base captured when parsing began. Stacks grow DOWN on every
    /// platform mpedb supports (Linux x86-64/ARM, macOS/Apple Silicon), so
    /// `base - here` is bytes consumed; `saturating_sub` keeps a surprise from
    /// turning into a panic. This is what PostgreSQL's `check_stack_depth()`
    /// does, for the same reason.
    fn enter_expr(&mut self) -> Result<()> {
        let probe = 0u8;
        let here = &probe as *const u8 as usize;
        if self.stack_base.saturating_sub(here) > MAX_PARSER_STACK {
            return Err(self.err_here("expression nested too deeply (parser stack exhausted)"));
        }
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

    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n).map(|t| &t.tok)
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

    /// `CREATE TABLE name (col TYPE [NOT NULL|UNIQUE|PRIMARY KEY]…,
    /// …[, PRIMARY KEY (a, b)][, UNIQUE (a, b)]…)`. Semantics (id
    /// assignment, pk resolution, validation) live in the facade/engine —
    /// this only builds the spec. `DEFAULT`/`CHECK`/foreign keys refuse by
    /// name so the gap is visible, not silent.
    fn parse_create_table(&mut self) -> Result<DdlStmt> {
        let name = self.ident("table name")?;
        self.expect(&Tok::LParen, "(")?;
        let mut columns = Vec::new();
        let mut table_pk: Vec<String> = Vec::new();
        let mut uniques: Vec<Vec<String>> = Vec::new();
        loop {
            if self.eat_word("PRIMARY") {
                self.expect_word("KEY")?;
                if !table_pk.is_empty() {
                    return Err(self.err_here("duplicate table-level PRIMARY KEY"));
                }
                table_pk = self.paren_ident_list()?;
            } else if self.eat_word("UNIQUE") {
                uniques.push(self.paren_ident_list()?);
            } else {
                let cname = self.ident("column name")?;
                let tyword = self.ident("column type")?;
                let Some(ty) = mpedb_types::ColumnType::parse(&tyword.to_ascii_lowercase())
                else {
                    return Err(self.err_here(format!(
                        "unknown column type `{tyword}` (int64/text/real/bool/blob/\
                         timestamp/any)"
                    )));
                };
                let mut col = crate::ddl::CreateColumnSpec {
                    name: cname,
                    ty,
                    not_null: false,
                    unique: false,
                    pk: false,
                };
                loop {
                    // NOT and NULL are reserved keywords (Tok::Kw), not
                    // identifiers — the rest of the constraint words are not.
                    if self.eat_kw(Kw::Not) {
                        self.expect_kw(Kw::Null, "NULL")?;
                        col.not_null = true;
                    } else if self.eat_kw(Kw::Null) {
                        col.not_null = false;
                    } else if self.eat_word("UNIQUE") {
                        col.unique = true;
                    } else if self.eat_word("PRIMARY") {
                        self.expect_word("KEY")?;
                        col.pk = true;
                    } else if self.eat_word("DEFAULT") || self.eat_word("CHECK")
                        || self.eat_word("REFERENCES")
                    {
                        return Err(self.err_here(
                            "DEFAULT/CHECK/REFERENCES are not supported in CREATE TABLE \
                             yet — declare them in the config schema",
                        ));
                    } else {
                        break;
                    }
                }
                columns.push(col);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, ")")?;
        Ok(DdlStmt::CreateTable(crate::ddl::CreateTableSpec {
            name,
            columns,
            table_pk,
            uniques,
        }))
    }

    /// `( ident [, ident]* )`
    fn paren_ident_list(&mut self) -> Result<Vec<String>> {
        self.expect(&Tok::LParen, "(")?;
        let mut out = vec![self.ident("column name")?];
        while self.eat(&Tok::Comma) {
            out.push(self.ident("column name")?);
        }
        self.expect(&Tok::RParen, ")")?;
        Ok(out)
    }

    fn parse_create_policy(&mut self) -> Result<DdlStmt> {
        self.expect_word("POLICY")?;
        let name = self.ident("policy name")?;
        self.expect_kw(Kw::On, "ON")?;
        let table = self.ident("table name")?;
        let mut permissive = true;
        if self.eat_kw(Kw::As) {
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

    fn parse_drop_table(&mut self) -> Result<DdlStmt> {
        // Optional `IF EXISTS`.
        let if_exists = if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("table name")?;
        Ok(DdlStmt::DropTable { name, if_exists })
    }

    fn parse_drop_policy(&mut self) -> Result<DdlStmt> {
        self.expect_word("POLICY")?;
        let name = self.ident("policy name")?;
        self.expect_kw(Kw::On, "ON")?;
        let table = self.ident("table name")?;
        Ok(DdlStmt::DropPolicy { table, name })
    }

    fn parse_alter(&mut self) -> Result<DdlStmt> {
        self.expect_word("TABLE")?;
        let table = self.ident("table name")?;
        // RENAME forms (pure schema metadata) branch off before the RLS words.
        if self.eat_word("RENAME") {
            if self.eat_word("TO") {
                let new_name = self.ident("new table name")?;
                return Ok(DdlStmt::AlterRenameTable { table, new_name });
            }
            // `RENAME COLUMN a TO b` or the bare `RENAME a TO b` (sqlite accepts
            // both; COLUMN is optional).
            self.eat_word("COLUMN");
            let column = self.ident("column name")?;
            if !self.eat_word("TO") {
                return Err(self.err_here("expected TO in RENAME COLUMN"));
            }
            let new_name = self.ident("new column name")?;
            return Ok(DdlStmt::AlterRenameColumn { table, column, new_name });
        }
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

    /// `SELECT …`, or a compound chain `SELECT … UNION [ALL]/EXCEPT/INTERSECT
    /// SELECT …`. Ops apply left-associatively with equal precedence (sqlite's
    /// rule; PostgreSQL binds INTERSECT tighter — documented deviation).
    fn select_stmt(&mut self) -> Result<Stmt> {
        let first = self.select_core()?;
        if self.peek_compound_op().is_none() {
            return Ok(Stmt::Select(first));
        }
        let mut arms = vec![first];
        let mut ops = Vec::new();
        while let Some(word) = self.peek_compound_op() {
            self.pos += 1;
            let op = match word {
                "UNION" => {
                    if self.eat_word("ALL") {
                        SetOp::UnionAll
                    } else {
                        SetOp::Union
                    }
                }
                "EXCEPT" => SetOp::Except,
                _ => SetOp::Intersect,
            };
            // ORDER BY / LIMIT bind to the WHOLE compound and can therefore
            // only follow the LAST arm — sqlite and PG both reject this shape.
            let prev = arms.last().expect("at least one arm");
            if !prev.order_by.is_empty() || prev.limit.is_some() || prev.offset.is_some() {
                return Err(self.err_here(
                    "ORDER BY / LIMIT / OFFSET apply to the whole compound — move them                      after the last SELECT",
                ));
            }
            if arms.len() >= MAX_COMPOUND_ARMS {
                return Err(self.err_here(format!(
                    "too many compound SELECT arms (max {MAX_COMPOUND_ARMS})"
                )));
            }
            ops.push(op);
            arms.push(self.select_core()?);
        }
        // The trailing clauses parsed into the last arm; they belong to the
        // compound. Ordinals / names in them resolve against the OUTPUT.
        let last = arms.last_mut().expect("at least two arms");
        let order_by = std::mem::take(&mut last.order_by);
        let limit = last.limit.take();
        let offset = last.offset.take();
        Ok(Stmt::Compound(CompoundStmt { arms, ops, order_by, limit, offset }))
    }

    /// Eat the no-op `ALL` quantifier (the explicit opposite of DISTINCT):
    /// `SELECT ALL x`, `count(ALL x)`. Positional word, consumed only when an
    /// expression can follow — `SELECT all FROM t` still names a column, and
    /// `count(all)` still counts one.
    fn eat_all_quantifier(&mut self) {
        if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("ALL"))
            && !matches!(
                self.peek_at(1),
                None | Some(Tok::Kw(Kw::From))
                    | Some(Tok::Comma)
                    | Some(Tok::RParen)
                    | Some(Tok::Semicolon)
            )
        {
            self.pos += 1;
        }
    }

    /// The next token starts a compound set operator, without consuming it.
    /// UNION / EXCEPT / INTERSECT are positional words, not keywords — a
    /// quoted identifier is how you'd name a table `union`.
    fn peek_compound_op(&self) -> Option<&'static str> {
        let w = match self.peek() {
            Some(Tok::Ident(w)) => w,
            _ => return None,
        };
        ["UNION", "EXCEPT", "INTERSECT"]
            .into_iter()
            .find(|k| w.eq_ignore_ascii_case(k))
    }

    fn select_core(&mut self) -> Result<SelectStmt> {
        self.expect_kw(Kw::Select, "SELECT")?;
        let distinct = self.eat_kw(Kw::Distinct);
        if !distinct {
            self.eat_all_quantifier();
        }
        let items = if self.eat(&Tok::Star) {
            None
        } else {
            let mut items = vec![self.select_item()?];
            while self.eat(&Tok::Comma) {
                if items.len() >= MAX_SELECT_ITEMS {
                    return Err(self.err_here(format!(
                        "too many SELECT items (max {MAX_SELECT_ITEMS})"
                    )));
                }
                items.push(self.select_item()?);
            }
            Some(items)
        };
        // FROM is optional (sqlite/PG): `SELECT 3+5` reads no table and
        // evaluates over ONE synthetic empty row. WHERE/ORDER BY/LIMIT
        // still parse below -- sqlite allows `SELECT 3 WHERE 1`.
        let (table, from_alias, joins) = if self.eat_kw(Kw::From) {
            // `FROM ( a JOIN b ON … )` — parens around a join group. For the
            // left-deep chains this grammar builds they are associativity no-ops,
            // so opening parens are counted and their closers consumed between
            // join steps. (A paren group as the INNER side of a join — `a JOIN
            // (b JOIN c)` — is NOT expressible left-deep and stays a parse error.)
            let mut from_parens = 0usize;
            while self.eat(&Tok::LParen) {
                from_parens += 1;
            }
            let table = self.ident("table name")?;
            let from_alias = self.opt_table_alias()?;
            let mut joins = Vec::new();
            // ONE left-deep chain where `,` and the JOIN keywords are equal
            // separators — sqlite's FROM grammar, and the corpus interleaves them
            // freely (`FROM a CROSS JOIN b, c`). The comma-join and CROSS JOIN
            // ARE the cartesian product, written in syntax whose whole meaning is
            // "every pair" (unlike a bare `JOIN b` with a forgotten ON, which
            // stays refused): desugared to `INNER JOIN … ON true`, with WHERE
            // filtering over the joined row — sqlite/PG semantics exactly.
            loop {
                if from_parens > 0 && self.eat(&Tok::RParen) {
                    from_parens -= 1;
                } else if self.eat(&Tok::Comma) {
                    let t = self.ident("table name after ','")?;
                    let alias = self.opt_table_alias()?;
                    joins.push(JoinClause {
                        table: t,
                        alias,
                        kind: JoinKind::Inner,
                        on: Expr::Lit(Value::Bool(true)),
                    });
                } else if self.eat_kw(Kw::Inner) {
                    self.expect_kw(Kw::Join, "JOIN after INNER")?;
                    joins.push(self.join_tail(JoinKind::Inner)?);
                } else if self.eat_kw(Kw::Join) {
                    joins.push(self.join_tail(JoinKind::Inner)?);
                } else if self.eat_word("LEFT") {
                    // The optional OUTER changes nothing — LEFT JOIN and
                    // LEFT OUTER JOIN are the same join.
                    let _ = self.eat_word("OUTER");
                    self.expect_kw(Kw::Join, "JOIN after LEFT")?;
                    joins.push(self.join_tail(JoinKind::Left)?);
                } else if self.eat_word("RIGHT") {
                    let _ = self.eat_word("OUTER");
                    self.expect_kw(Kw::Join, "JOIN after RIGHT")?;
                    joins.push(self.join_tail(JoinKind::Right)?);
                } else if self.eat_word("FULL") {
                    let _ = self.eat_word("OUTER");
                    self.expect_kw(Kw::Join, "JOIN after FULL")?;
                    joins.push(self.join_tail(JoinKind::Full)?);
                } else if matches!(self.peek_join_kind(), Some("CROSS")) {
                    // `CROSS JOIN t` is the cartesian product written in the
                    // syntax whose whole meaning is "every pair" — exactly the
                    // comma-join, so it desugars the same way (no ON clause).
                    self.pos += 1;
                    self.expect_kw(Kw::Join, "JOIN after CROSS")?;
                    let t = self.ident("table name after CROSS JOIN")?;
                    let alias = self.opt_table_alias()?;
                    joins.push(JoinClause {
                        table: t,
                        alias,
                        kind: JoinKind::Inner,
                        on: Expr::Lit(Value::Bool(true)),
                    });
                } else if let Some(kind) = self.peek_join_kind() {
                    // Only NATURAL is left unsupported: its join condition is
                    // implicit in column NAMES, which rigid schemas make a trap.
                    return Err(self.err_here(format!(
                        "{kind} JOIN is not supported — write the ON condition explicitly",
                    )));
                } else {
                    break;
                }
            }
            if from_parens > 0 {
                return Err(self.err_here("unclosed `(` in FROM"));
            }
            (Some(table), from_alias, joins)
        } else {
            (None, None, Vec::new())
        };
        let where_clause = if self.eat_kw(Kw::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        // GROUP BY … HAVING …, between WHERE and ORDER BY. The order is SQL's
        // and it is also the execution order: filter, then group, then HAVING —
        // which is exactly why HAVING sees the grouped row and WHERE cannot.
        let mut group_by: Vec<Expr> = Vec::new();
        if self.eat_kw(Kw::Group) {
            self.expect_kw(Kw::By, "BY after GROUP")?;
            loop {
                group_by.push(self.expr()?);
                if group_by.len() > MAX_ORDER_BY_ITEMS {
                    return Err(self.err_here(format!(
                        "too many GROUP BY items (max {MAX_ORDER_BY_ITEMS})"
                    )));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let having = if self.eat_kw(Kw::Having) {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw(Kw::Order) {
            self.expect_kw(Kw::By, "BY after ORDER")?;
            loop {
                let col = self.expr()?;
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
        Ok(SelectStmt {
            table,
            alias: from_alias,
            joins,
            distinct,
            items,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// One SELECT-list item: `expr [[AS] alias]`. A bare identifier right
    /// after the expression is an alias, as in sqlite/PostgreSQL —
    /// unambiguous because everything that can otherwise follow an item
    /// (FROM, WHERE, GROUP, ORDER, LIMIT, `,`, `;`, EOF) is a keyword token
    /// or not an identifier at all. A quoted identifier is always an alias.
    fn select_item(&mut self) -> Result<(Expr, Option<String>)> {
        let e = self.expr()?;
        if self.eat_kw(Kw::As) {
            return Ok((e, Some(self.ident("alias after AS")?)));
        }
        // A quoted identifier is always an alias. A bare one is too — UNLESS
        // it is a compound operator: with FROM optional (#67), `SELECT 1
        // UNION SELECT 2` puts `UNION` right after an item, and reading it as
        // the item's alias would swallow the second arm.
        if matches!(self.peek(), Some(Tok::QuotedIdent(_))) {
            return Ok((e, Some(self.ident("select-item alias")?)));
        }
        if matches!(self.peek(), Some(Tok::Ident(_))) && self.peek_compound_op().is_none() {
            return Ok((e, Some(self.ident("select-item alias")?)));
        }
        Ok((e, None))
    }

    /// Name an unsupported join kind, without consuming it. `None` if the next
    /// token does not start one.
    fn peek_join_kind(&self) -> Option<&'static str> {
        let w = match self.peek() {
            Some(Tok::Ident(w)) => w,
            _ => return None,
        };
        ["LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "OUTER"]
            .into_iter()
            .find(|k| w.eq_ignore_ascii_case(k))
    }

    /// The part of a JOIN after the `JOIN` keyword.
    /// `[AS] ident` after a table name, or nothing. A bare identifier here is
    /// unambiguous: every other thing that can follow a table name (JOIN, ON,
    /// WHERE, GROUP, ORDER, LIMIT, `;`, EOF) is a keyword or not an ident.
    fn opt_table_alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw(Kw::As) {
            return Ok(Some(self.ident("alias after AS")?));
        }
        // A bare ident is an alias — UNLESS it is a join-kind word. LEFT / RIGHT
        // / FULL / CROSS / NATURAL / OUTER are not keywords (they are recognised
        // positionally), so without this `FROM emp LEFT JOIN dept` would read
        // `LEFT` as an alias for `emp` and lose the join. A quoted identifier is
        // always an alias — quoting is how you'd name a table `left`.
        if matches!(self.peek(), Some(Tok::QuotedIdent(_))) {
            return Ok(Some(self.ident("table alias")?));
        }
        if matches!(self.peek(), Some(Tok::Ident(_)))
            && self.peek_join_kind().is_none()
            // …nor is a compound operator: `FROM t1 UNION SELECT` must not
            // read `UNION` as t1's alias and lose the second arm.
            && self.peek_compound_op().is_none()
        {
            return Ok(Some(self.ident("table alias")?));
        }
        Ok(None)
    }

    fn join_tail(&mut self, kind: JoinKind) -> Result<JoinClause> {
        let table = self.ident("table name after JOIN")?;
        let alias = self.opt_table_alias()?;
        // ON is required. A comma-join / cross join is a cartesian product, and
        // the times someone means one are far outnumbered by the times they
        // forgot the condition.
        self.expect_kw(Kw::On, "ON after JOIN — the join condition is required")?;
        let on = self.expr()?;
        Ok(JoinClause { table, alias, kind, on })
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
        let on_conflict = self.on_conflict_clause()?;
        let returning = self.returning_clause()?;
        Ok(Stmt::Insert(InsertStmt {
            table,
            columns,
            rows,
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
        let returning = self.returning_clause()?;
        Ok(Stmt::Update(UpdateStmt {
            table,
            set,
            where_clause,
            returning,
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
        let returning = self.returning_clause()?;
        Ok(Stmt::Delete(DeleteStmt {
            table,
            where_clause,
            returning,
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
            if !seen_cmp && (self.peek_kw(Kw::Between) || self.peek_not_between()) {
                e = self.between_suffix(e)?;
                seen_cmp = true;
                continue;
            }
            if !seen_cmp && (self.peek_kw(Kw::In) || self.peek_not_in()) {
                e = self.in_suffix(e)?;
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
                // `||` sits in the additive tier (left-associative). sqlite
                // technically binds it tighter than `*`; nothing in the corpus
                // or a sane query observes the difference, and additive keeps
                // the grammar flat.
                Some(Tok::Concat) => BinOp::Concat,
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
        } else if self.eat(&Tok::Plus) {
            // Unary `+` is the identity, as in sqlite and PostgreSQL — parsed
            // and DROPPED, so `+ col`, `- + 43` and `+ ( - 78 )` all work.
            // No AST node: identity would only be something for later stages
            // to look through. This single arm was the sqllogictest corpus'
            // single largest blocker (#62: ~55% of all refused statements).
            self.enter_expr()?;
            let e = self.unary_expr();
            self.exit_expr();
            e
        } else {
            self.primary()
        }
    }

    /// `CASE [x] WHEN … THEN … [ELSE …] END`.
    ///
    /// The simple form is desugared into the searched form (`CASE x WHEN a` ->
    /// `CASE WHEN x = a`), so only one shape reaches the binder. That duplicates
    /// `x` per arm in the plan, and it is the one place this differs from the
    /// standard's letter: SQL evaluates the operand once. For pure expressions —
    /// and every expression here is pure, there are no functions with side
    /// effects — the observable result is identical.
    ///
    /// 3VL falls out of the desugaring for free: `CASE x WHEN NULL` becomes
    /// `x = NULL`, which is NULL, which is not TRUE, so the arm is skipped —
    /// exactly what the standard requires.
    fn case_expr(&mut self) -> Result<Expr> {
        // A simple-form operand is anything up to WHEN.
        let operand = if self.peek_kw(Kw::When) {
            None
        } else {
            Some(self.expr()?)
        };
        let mut arms = Vec::new();
        while self.eat_kw(Kw::When) {
            let cond = self.expr()?;
            self.expect_kw(Kw::Then, "THEN after WHEN")?;
            let then = self.expr()?;
            let cond = match &operand {
                Some(x) => Expr::Binary(BinOp::Eq, Box::new(x.clone()), Box::new(cond)),
                None => cond,
            };
            arms.push((cond, then));
        }
        if arms.is_empty() {
            return Err(self.err_here("CASE needs at least one WHEN"));
        }
        let else_ = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw(Kw::End, "END closing CASE")?;
        Ok(Expr::Case(arms, else_))
    }


    /// `x BETWEEN a AND b`, split out of `cmp_expr`.
    ///
    /// `#[inline(never)]` and a separate frame on purpose: `cmp_expr` sits on
    /// the mutually-recursive descent path, so every local here would otherwise
    /// be paid on EVERY nesting level. Inlining these blocks back into cmp_expr
    /// grew the per-level frame enough that MAX_EXPR_DEPTH=128 overflowed the
    /// stack in a debug build — the exact crash the depth guard exists to stop.
    #[inline(never)]
    fn between_suffix(&mut self, e: Expr) -> Result<Expr> {
        let negated = self.eat_kw(Kw::Not);
        self.expect_kw(Kw::Between, "BETWEEN")?;
        // add_expr, NOT expr: the AND below belongs to BETWEEN's own syntax, so
        // a full expression parse would swallow it and then fail looking for an
        // AND that is already gone.
        let lo = self.add_expr()?;
        self.expect_kw(Kw::And, "AND in BETWEEN")?;
        let hi = self.add_expr()?;
        // Desugared to `x >= lo AND x <= hi`: that is the shape the planner's
        // extract_access already turns into a PkRange, so BETWEEN becomes a
        // range SCAN rather than a full scan plus filter. The cost is `x`
        // appearing twice in the plan.
        let ge = Expr::Binary(BinOp::Ge, Box::new(e.clone()), Box::new(lo));
        let le = Expr::Binary(BinOp::Le, Box::new(e), Box::new(hi));
        let both = Expr::Binary(BinOp::And, Box::new(ge), Box::new(le));
        // NOT BETWEEN negates the whole conjunct rather than being spelled out
        // as (NOT a OR NOT b): De Morgan holds in 3VL, but writing it twice
        // invites the two spellings to drift.
        Ok(if negated {
            Expr::Unary(UnOp::Not, Box::new(both))
        } else {
            both
        })
    }

    /// `x IN (…)` / `x NOT IN (…)`, split out of `cmp_expr` (see
    /// [`Self::between_suffix`] for why).
    ///
    /// Two shapes share the syntax: a session-context list (§2.6 — one reserved
    /// param, because the arity must NOT reach the plan bytes) and a general
    /// value list (#21 — the arity IS the query). What is inside the parens
    /// decides which.
    #[inline(never)]
    fn in_suffix(&mut self, e: Expr) -> Result<Expr> {
        let negated = self.eat_kw(Kw::Not);
        self.expect_kw(Kw::In, "IN")?;
        self.expect(&Tok::LParen, "`(` after IN")?;
        // `IN ()` is a syntax error in PostgreSQL too, and it would also mean an
        // InList(0) instruction, which the IR rejects.
        if self.peek() == Some(&Tok::RParen) {
            return Err(self.err_here("IN needs at least one value: `IN ()` is empty"));
        }
        // `IN (SELECT …)` — membership in a subquery's output (#70). The
        // SELECT keyword right after the paren decides, same rule as the
        // scalar-subquery primary.
        if matches!(self.peek(), Some(Tok::Kw(Kw::Select))) {
            let inner = self.select_core()?;
            self.expect(&Tok::RParen, "`)` after IN subquery")?;
            return Ok(Expr::InSubquery(Box::new(e), Box::new(inner), negated));
        }
        let first = self.expr()?;
        if let (Expr::ContextRef(key), Some(&Tok::RParen)) = (&first, self.peek()) {
            let key = key.clone();
            self.pos += 1;
            return Ok(Expr::InContext(Box::new(e), key, negated));
        }
        let mut items = vec![first];
        while self.eat(&Tok::Comma) {
            items.push(self.expr()?);
        }
        self.expect(&Tok::RParen, "`)` closing IN")?;
        Ok(Expr::InList(Box::new(e), items, negated))
    }

    /// `name(args…)`, split out of `primary` (see [`Self::between_suffix`]).
    #[inline(never)]
    fn call_suffix(&mut self, name: String) -> Result<Expr> {
        self.expect(&Tok::LParen, "`(`")?;
        // Aggregates are intercepted BEFORE the scalar argument parse, because
        // `count(*)` has an argument that is not an expression. `*` there is not
        // "all columns" — it means "the row itself", which is the whole reason
        // count(*) and count(x) differ on NULLs.
        if let Some(f) = agg_fn(&name) {
            if self.eat(&Tok::Star) {
                self.expect(&Tok::RParen, "`)` closing count(*)")?;
                if f != mpedb_types::AggFn::Count {
                    return Err(self.err_here(format!(
                        "{}(*) is not valid — only count(*) takes the row itself; \
                         {}() needs a value",
                        f.name(),
                        f.name()
                    )));
                }
                return Ok(Expr::Agg(f, None, false));
            }
            let distinct = self.eat_kw(Kw::Distinct);
            if !distinct {
                self.eat_all_quantifier();
            }
            if distinct && self.peek() == Some(&Tok::Star) {
                // sqlite and PG both make this a syntax error, and they are
                // right: `count(*)` counts ROWS, and "distinct rows" is what
                // SELECT DISTINCT means — there is nothing for DISTINCT to
                // apply to inside the parens.
                return Err(self.err_here(format!(
                    "{}(DISTINCT *) is not valid — use SELECT DISTINCT, or name a column",
                    f.name()
                )));
            }
            let arg = self.expr()?;
            if self.peek() == Some(&Tok::Comma) {
                return Err(self.err_here(format!("{}() takes exactly one argument", f.name())));
            }
            self.expect(&Tok::RParen, "`)` closing the argument list")?;
            return Ok(Expr::Agg(f, Some(Box::new(arg)), distinct));
        }
        let mut args = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            args.push(self.expr()?);
            while self.eat(&Tok::Comma) {
                args.push(self.expr()?);
            }
        }
        self.expect(&Tok::RParen, "`)` closing the argument list")?;
        let lname = name.to_ascii_lowercase();
        Ok(match lname.as_str() {
            "coalesce" => Expr::Coalesce(args),
            // ifnull IS coalesce/2; a separate node would be a second place for
            // the laziness to be got wrong.
            "ifnull" => {
                if args.len() != 2 {
                    return Err(self.err_here("ifnull() takes exactly 2 arguments"));
                }
                Expr::Coalesce(args)
            }
            _ => Expr::Func(lname, args),
        })
    }


    /// Everything a bare identifier can turn into: `current_setting(...)`, a
    /// function call, `excluded.<col>`, or a plain column.
    ///
    /// `#[inline(never)]` and its own frame for the same reason as
    /// [`Self::between_suffix`]: `primary` is on the mutually-recursive descent
    /// path, so any local here is paid on EVERY nesting level. This one is not
    /// hypothetical either — folding it back into `primary` overflowed the
    /// stack at the permitted depth, and the test that caught it is
    /// `deep_nesting_through_the_new_constructs_is_also_a_parse_error`.
    #[inline(never)]
    fn ident_suffix(&mut self, s: String) -> Result<Expr> {

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
                    return Ok(Expr::ContextRef(key));
                }
                if self.peek() == Some(&Tok::LParen) {
                    return self.call_suffix(s);
                }
                // `excluded.<col>` — the proposed row inside ON CONFLICT DO
                // UPDATE. Only a BARE `excluded` qualifies; a quoted
                // "excluded" stays a column name, so a table that really has a
                // column called `excluded` keeps working.
                if s.eq_ignore_ascii_case("excluded") && self.peek() == Some(&Tok::Dot) {
                    self.pos += 1;
                    let col = self.ident("column name after `excluded.`")?;
                    return Ok(Expr::Excluded(col));
                }
                // `<qualifier>.<column>` — both ancestors accept it, so mpedb
                // does too. There is exactly one table in scope (no joins yet),
                // so the qualifier is checked against it and then dropped: the
                // binder resolves a plain column name either way. When joins
                // arrive the qualifier stops being decoration and this is where
                // it gets used.
                if self.peek() == Some(&Tok::Dot) {
                    self.pos += 1;
                    let col = self.ident("column name after `.`")?;
                    return Ok(Expr::Qualified(s, col));
                }
                Ok(Expr::Col(s))
    }

    fn primary(&mut self) -> Result<Expr> {
        let pos = self.here();
        if self.eat_kw(Kw::Case) {
            return self.case_expr();
        }
        // `EXISTS ( SELECT … )` — positional word like CAST; NOT EXISTS
        // arrives here through unary NOT, which is exactly `Exists` negated
        // by a Not instruction, so no dedicated node is needed for it.
        if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("EXISTS"))
            && matches!(self.peek_at(1), Some(Tok::LParen))
        {
            self.pos += 2;
            if !matches!(self.peek(), Some(Tok::Kw(Kw::Select))) {
                return Err(self.err_here("EXISTS takes a subquery: EXISTS (SELECT …)"));
            }
            let inner = self.select_core()?;
            self.expect(&Tok::RParen, "`)` after EXISTS subquery")?;
            return Ok(Expr::Exists(Box::new(inner), false));
        }
        // `CAST ( expr AS <typename> )` — CAST is a positional word, not a
        // keyword, so a table may still be named `cast`.
        if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("CAST"))
            && matches!(self.peek_at(1), Some(Tok::LParen))
        {
            self.pos += 2;
            let e = self.expr()?;
            self.expect_kw(Kw::As, "AS in CAST")?;
            let tyname = self.ident("type name in CAST")?;
            let ty = cast_type(&tyname).ok_or_else(|| {
                self.err_here(format!("unknown CAST target type `{tyname}`"))
            })?;
            self.expect(&Tok::RParen, ") after CAST")?;
            return Ok(Expr::Cast(Box::new(e), ty));
        }
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
            Some(Tok::Ident(s)) => self.ident_suffix(s),
            Some(Tok::QuotedIdent(s)) => Ok(Expr::Col(s)),
            Some(Tok::LParen) => {
                // `(SELECT …)` is a scalar subquery, not a parenthesized
                // expression — the SELECT keyword right after `(` decides.
                if matches!(self.peek(), Some(Tok::Kw(Kw::Select))) {
                    let inner = self.select_core()?;
                    self.expect(&Tok::RParen, "`)` after subquery")?;
                    return Ok(Expr::Subquery(Box::new(inner)));
                }
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


/// Map a SQL type name to the CAST target. sqlite's affinity vocabulary and
/// the standard's both land on mpedb's five scalars; NUMERIC/DECIMAL take
/// float64 (mpedb has no arbitrary-precision numeric — documented).
fn cast_type(name: &str) -> Option<mpedb_types::ColumnType> {
    use mpedb_types::ColumnType as T;
    let up = name.to_ascii_uppercase();
    Some(match up.as_str() {
        "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" | "INT2" | "INT8" => T::Int64,
        "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" | "DECIMAL" => T::Float64,
        "TEXT" | "CHAR" | "VARCHAR" | "CHARACTER" | "CLOB" | "STRING" => T::Text,
        "BOOLEAN" | "BOOL" => T::Bool,
        "BLOB" => T::Blob,
        "TIMESTAMP" => T::Timestamp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(src: &str) -> Expr {
        parse_expr_only(src).unwrap().0
    }

    /// Unary `+` is the identity and parses to NOTHING — `+x` is `x`, and the
    /// sign chains the sqllogictest corpus is full of (`- + 43`, `+ ( - 78 )`)
    /// reduce to the plain negation they mean.
    #[test]
    fn unary_plus_is_identity() {
        assert_eq!(expr("+ 43"), expr("43"));
        assert_eq!(expr("+ a"), expr("a"));
        assert_eq!(expr("- + 43"), expr("- 43"));
        assert_eq!(expr("+ ( - 78 )"), expr("(- 78)"));
        assert_eq!(expr("a + + b"), expr("a + b"));
    }

    /// `CAST(x AS type)` parses to its own node; `CAST` stays usable as an
    /// ordinary identifier when not followed by `(`.
    #[test]
    fn cast_parses_and_concat_sits_in_the_additive_tier() {
        use mpedb_types::ColumnType as T;
        assert_eq!(
            expr("CAST(a AS INTEGER)"),
            Expr::Cast(Box::new(Expr::Col("a".into())), T::Int64)
        );
        assert_eq!(
            expr("cast(NULL as real)"),
            Expr::Cast(Box::new(Expr::Lit(Value::Null)), T::Float64)
        );
        assert!(parse_expr_only("CAST(a AS lolwut)").is_err());
        // bare `cast` is still a column name
        assert_eq!(expr("cast"), Expr::Col("cast".into()));

        // `a || b || c` is left-associative and binds like +/-
        assert_eq!(
            expr("a || b || c"),
            Expr::Binary(
                BinOp::Concat,
                Box::new(Expr::Binary(
                    BinOp::Concat,
                    Box::new(Expr::Col("a".into())),
                    Box::new(Expr::Col("b".into()))
                )),
                Box::new(Expr::Col("c".into()))
            )
        );
        // lone `|` is a clear parse error, not a mystery token
        assert!(parse_expr_only("a | b").is_err());
    }

    /// A compound chain parses left-associatively, hoists the trailing
    /// ORDER BY/LIMIT to the compound, and rejects them mid-chain.
    #[test]
    fn compound_selects_parse() {
        let stmt = |src: &str| parse_statement(src).unwrap().0;
        let Stmt::Compound(c) =
            stmt("SELECT a FROM t UNION ALL SELECT b FROM u UNION SELECT c FROM v ORDER BY 1 LIMIT 3")
        else {
            panic!("expected a compound");
        };
        assert_eq!(c.arms.len(), 3);
        assert_eq!(c.ops, vec![SetOp::UnionAll, SetOp::Union]);
        // hoisted off the last arm
        assert_eq!(c.order_by.len(), 1);
        assert_eq!(c.limit, Some(3));
        assert!(c.arms.iter().all(|a| a.order_by.is_empty() && a.limit.is_none()));

        let Stmt::Compound(c) = stmt("SELECT a FROM t EXCEPT SELECT a FROM u") else {
            panic!("expected a compound");
        };
        assert_eq!(c.ops, vec![SetOp::Except]);
        let Stmt::Compound(c) = stmt("SELECT a FROM t INTERSECT SELECT a FROM u") else {
            panic!("expected a compound");
        };
        assert_eq!(c.ops, vec![SetOp::Intersect]);

        // ORDER BY mid-chain is an error, not a silent per-arm sort.
        assert!(parse_statement("SELECT a FROM t ORDER BY a UNION SELECT b FROM u").is_err());
        // `union` is not eaten as a table alias.
        assert!(matches!(
            stmt("SELECT a FROM t UNION SELECT b FROM u"),
            Stmt::Compound(_)
        ));
        // CROSS JOIN desugars like the comma-join.
        let Stmt::Select(s) = stmt("SELECT a FROM t CROSS JOIN u") else {
            panic!("expected a select");
        };
        assert_eq!(s.joins.len(), 1);
        assert_eq!(s.joins[0].on, Expr::Lit(Value::Bool(true)));
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
                assert_eq!(sel.table.as_deref(), Some("t"));
                assert_eq!(sel.items.as_ref().unwrap().len(), 2);
                assert!(sel.where_clause.is_some());
                assert_eq!(
                    sel.order_by,
                    vec![
                        (Expr::Col("a".into()), false),
                        (Expr::Col("b".into()), true)
                    ]
                );
                assert_eq!(sel.limit, Some(10));
                assert_eq!(sel.offset, Some(2));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    /// `ORDER BY count(*)` — legal in sqlite and PG, and the reason ORDER BY
    /// items are expressions rather than names. An identifier-only ORDER BY
    /// rejects this at the tokenizer, before anything can rule on whether it
    /// means something.
    #[test]
    fn order_by_takes_an_aggregate_not_just_a_name() {
        let (s, _, _) =
            parse_statement("select dept, count(*) from t group by dept order by count(*) desc")
                .unwrap();
        match s {
            Stmt::Select(sel) => {
                assert_eq!(sel.group_by, vec![Expr::Col("dept".into())]);
                assert_eq!(
                    sel.order_by,
                    vec![(Expr::Agg(mpedb_types::AggFn::Count, None, false), true)]
                );
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

        // Well inside the limit for every form. This used to say 100, back
        // when MAX_EXPR_DEPTH was 128 — a number that turned out not to be
        // survivable once CASE existed (see the constant's docs). 20 is still
        // far past any real statement.
        let d = 20;
        let parens = format!("{}a > 0{}", "(".repeat(d), ")".repeat(d));
        assert!(parse_expr_only(&parens).is_ok());
        assert!(parse_expr_only(&format!("{}a", "NOT ".repeat(d))).is_ok());
        assert!(parse_expr_only(&format!("{}1", "-".repeat(d))).is_ok());
    }

    /// Every construct that recurses is a stack-overflow vector, and each one
    /// added is a NEW path the paren test does not cover. CASE, function
    /// arguments and IN lists all descend through `expr()`, so they must hit the
    /// same depth guard rather than the thread stack.
    ///
    /// This is not hypothetical: extracting these blocks into their own frames
    /// was forced by an actual overflow — inline, their locals were paid on
    /// every one of the 128 permitted levels and 128 was no longer survivable.
    #[test]
    fn deep_nesting_through_the_new_constructs_is_also_a_parse_error() {
        let cases = [
            format!("{}1{}", "coalesce(".repeat(1000), ", 2)".repeat(1000)),
            format!("{}1{}", "abs(".repeat(1000), ")".repeat(1000)),
            format!("{}1{}", "CASE WHEN true THEN ".repeat(1000), " END".repeat(1000)),
            format!("a IN ({}1{})", "abs(".repeat(1000), ")".repeat(1000)),
            format!("{}a BETWEEN 1 AND 2{}", "(".repeat(1000), ")".repeat(1000)),
            format!("{}a > 0{}", "NOT (".repeat(1000), ")".repeat(1000)),
        ];
        for (i, sql) in cases.iter().enumerate() {
            match parse_expr_only(sql) {
                Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply") => {}
                other => panic!("case {i} must be a depth error, got {other:?}"),
            }
        }
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
        // `FROM t garbage` is now `FROM t AS garbage` — a valid alias (#44).
        // Genuinely trailing input is a SECOND bare word after the alias.
        assert!(parse_statement("SELECT * FROM t alias garbage").is_err());
        assert!(parse_statement("SELECT * FROM t; SELECT * FROM t").is_err());
        assert!(parse_statement("EXPLAIN EXPLAIN SELECT * FROM t").is_err());
        assert!(parse_expr_only("a = ").is_err());
        assert!(parse_expr_only("(a = 1").is_err());
    }

    /// The budget must be survivable on the stack it is budgeted against.
    ///
    /// MAX_PARSER_STACK is 1 MiB, i.e. half a default 2 MiB thread. Parse right
    /// up to the guard inside exactly that 2 MiB and require an ERROR rather
    /// than an abort: if a future construct or a compiler change makes frames
    /// fatter than the budget assumes, this fails loudly here instead of taking
    /// out the test binary somewhere unrelated.
    #[test]
    fn the_stack_budget_is_survivable_on_a_default_thread() {
        let inputs: Vec<String> = vec![
            // Deep enough to blow past the budget on every shape, cheap and
            // expensive alike.
            format!("{}a > 0{}", "(".repeat(4000), ")".repeat(4000)),
            format!("{}a", "NOT ".repeat(4000)),
            format!("{}1{}", "CASE WHEN true THEN ".repeat(4000), " END".repeat(4000)),
            format!("{}1{}", "coalesce(".repeat(4000), ", 2)".repeat(4000)),
            format!("{}1{}", "abs(".repeat(4000), ")".repeat(4000)),
            format!("a IN ({}1{})", "abs(".repeat(4000), ")".repeat(4000)),
        ];
        let h = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024) // the default a spawned thread gets
            .spawn(move || {
                for sql in &inputs {
                    match parse_expr_only(sql) {
                        Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply") => {}
                        other => panic!("expected a depth error, got {other:?}"),
                    }
                }
            })
            .unwrap();
        h.join().expect(
            "the parser overflowed a 2 MiB stack before its own 1 MiB budget stopped it: \
             a frame grew, so MAX_PARSER_STACK no longer leaves room. Shrink the frame \
             (move locals into an #[inline(never)] helper) or lower the budget.",
        );
    }


    /// What the byte budget actually buys, per construct. Compare against the
    /// measured ancestors (sqlite3 3.45: 93 nested parens, 18 nested CASE).
    ///   cargo test -p mpedb-sql --lib limits_probe -- --ignored --nocapture
    #[test]
    #[ignore]
    fn limits_probe() {
        type Gen = Box<dyn Fn(usize) -> String>;
        let mk: Vec<(&str, Gen)> = vec![
            ("parens", Box::new(|d| format!("{}1{}", "(".repeat(d), ")".repeat(d)))),
            ("NOT", Box::new(|d| format!("{}a", "NOT ".repeat(d)))),
            ("CASE", Box::new(|d| format!("{}1{}", "CASE WHEN true THEN ".repeat(d), " END".repeat(d)))),
            ("coalesce", Box::new(|d| format!("{}1{}", "coalesce(".repeat(d), ", 2)".repeat(d)))),
        ];
        for (name, f) in mk {
            let (mut lo, mut hi) = (1usize, 4000usize);
            while lo < hi {
                let m = (lo + hi).div_ceil(2);
                let sql = f(m);
                let ok = std::thread::Builder::new()
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || parse_expr_only(&sql).is_ok())
                    .unwrap()
                    .join()
                    .unwrap_or(false);
                if ok { lo = m } else { hi = m - 1 }
            }
            eprintln!("  mpedb max nested {name:>9}: {lo}");
        }
    }

    /// Both ancestors accept `<table>.<column>`, so mpedb does. The qualifier
    /// is CHECKED rather than ignored: with one table in scope it is decoration,
    /// but silently accepting `nonsense.id` would turn a typo into a
    /// wrong-table read the day joins exist.
    #[test]
    fn table_qualified_columns_parse_and_are_distinct_from_excluded() {
        let (e, _) = parse_expr_only("orders.tenant").unwrap();
        assert_eq!(e, Expr::Qualified("orders".into(), "tenant".into()));
        // `excluded` is its own thing, not a table qualifier.
        let (e, _) = parse_expr_only("excluded.tenant").unwrap();
        assert_eq!(e, Expr::Excluded("tenant".into()));
        // A quoted qualifier is still a qualifier; a quoted `excluded` is a column.
        let (e, _) = parse_expr_only("\"excluded\"").unwrap();
        assert_eq!(e, Expr::Col("excluded".into()));
    }
}
