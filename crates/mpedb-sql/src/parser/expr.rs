//! Expression grammar for the recursive-descent parser.
//!
//! Precedence, loosest to tightest:
//! `OR` < `AND` < `NOT` < comparison / `IS [NOT] NULL` / `LIKE`
//! < `+ -` < `* / %` < unary `-` < primary.
//! Comparisons do not chain (`a < b < c` is a parse error).
//!
//! Split out of [`super`] to keep that file under the size limit. The shared
//! [`Parser`] token helpers and struct fields live in `super` and stay reachable
//! here because `parser::expr` is a descendant module. `expr` itself is
//! `pub(super)` so the statement/DML/SELECT grammar and the `parse_expr_only`
//! entry point can reach it.

use super::{Parser, ParamStyle, MAX_EXPR_DEPTH, MAX_ORDER_BY_ITEMS, MAX_PARSER_STACK};
use crate::ast::{
    BinOp, Expr, FrameAst, FrameBound, FrameMode, SelectStmt, SubqueryBody, UnOp, WindowFunc,
    WindowSpecAst,
};
use crate::token::{Kw, Tok};
use mpedb_types::{Error, Result, Value};

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
        "total" => Total,
        "group_concat" => GroupConcat,
        _ => return None,
    })
}

/// The zero-argument ranking / distribution window functions (stage 1a +
/// stage 2b). Recognized only when `(` follows (i.e. as a call), so a bare
/// `rank` / `row_number` column name is unaffected — they are NOT reserved
/// words. Each REQUIRES an `OVER` clause and takes no arguments; `sqlite`
/// refuses `rank()` used any other way too. `percent_rank`/`cume_dist` are
/// zero-argument distribution functions and join the ranking functions here;
/// `ntile(n)` DOES take an argument (its bucket count) and is handled
/// separately in `call_suffix`.
fn window_rank_fn(name: &str) -> Option<WindowFunc> {
    Some(match name.to_ascii_lowercase().as_str() {
        "row_number" => WindowFunc::RowNumber,
        "rank" => WindowFunc::Rank,
        "dense_rank" => WindowFunc::DenseRank,
        "percent_rank" => WindowFunc::PercentRank,
        "cume_dist" => WindowFunc::CumeDist,
        _ => return None,
    })
}

/// The value/offset window functions (stage 2), with their `(min, max)`
/// argument arity. Each takes a real expression argument list (unlike the
/// zero-argument ranking functions) and, like them, is only valid as a window
/// function — `OVER` is required and a bare `lag`/`lead`/… column name is
/// unaffected (recognized only when `(` follows).
fn window_value_fn(name: &str) -> Option<(WindowFunc, usize, usize)> {
    Some(match name.to_ascii_lowercase().as_str() {
        // lag/lead: expr [, offset [, default]].
        "lag" => (WindowFunc::Lag, 1, 3),
        "lead" => (WindowFunc::Lead, 1, 3),
        "first_value" => (WindowFunc::FirstValue, 1, 1),
        "last_value" => (WindowFunc::LastValue, 1, 1),
        // nth_value: expr, n.
        "nth_value" => (WindowFunc::NthValue, 2, 2),
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    /// Enter one level of expression recursion, refusing to go deeper than the
    /// stack can hold.
    ///
    /// Reads the approximate stack pointer (the address of a local) and compares
    /// it to the base captured when parsing began. Stacks grow DOWN on every
    /// platform mpedb supports (Linux x86-64/ARM, macOS/Apple Silicon), so
    /// `base - here` is bytes consumed; `saturating_sub` keeps a surprise from
    /// turning into a panic. This is what PostgreSQL's `check_stack_depth()`
    /// does, for the same reason.
    /// Parse a `CAST` target type name — sqlite's liberal grammar: one or more
    /// identifier words (`DOUBLE PRECISION`, `UNSIGNED BIG INT`) followed by an
    /// OPTIONAL parenthesized size (`VARCHAR(10)`, `DECIMAL(10, 2)`). Any name
    /// is accepted; the binder maps it to an affinity. The words are returned
    /// joined by single spaces; the size is consumed and discarded (it never
    /// changes the affinity). At least one word is required, except a single
    /// quoted empty identifier (`CAST(x AS "")`) is allowed and yields "".
    fn type_name(&mut self) -> Result<String> {
        let mut words: Vec<String> = Vec::new();
        while matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_))) {
            words.push(self.ident("a type name in CAST")?);
        }
        if words.is_empty() {
            return Err(self.err_here("expected a type name in CAST"));
        }
        // Optional `( number )` or `( number , number )` size/precision — accept
        // and drop it. Signed numbers are allowed (some dialects write them).
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            while matches!(
                self.peek(),
                Some(Tok::Int(_))
                    | Some(Tok::Float(_))
                    | Some(Tok::Comma)
                    | Some(Tok::Plus)
                    | Some(Tok::Minus)
            ) {
                self.pos += 1;
            }
            self.expect(&Tok::RParen, ") after a type size in CAST")?;
        }
        Ok(words.join(" "))
    }

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

    // ---- expressions ----------------------------------------------------

    // Depth guards: `expr()` covers the `( expr )` cycle through `primary()`;
    // `not_expr`/`unary_expr` guard their direct self-recursion, which does
    // not pass back through `expr()`.

    pub(super) fn expr(&mut self) -> Result<Expr> {
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
        let mut e = self.bit_expr()?;
        let mut seen_cmp = false;
        loop {
            if self.eat_kw(Kw::Is) {
                let negated = self.eat_kw(Kw::Not);
                if self.eat_kw(Kw::Null) {
                    e = Expr::IsNull(Box::new(e), negated);
                } else {
                    // General `x IS y` / `x IS NOT y` — NULL-safe (not-)distinct.
                    // The right operand parses at the additive tier, like `=`'s
                    // RHS, so `IS` sits at the comparison level and does not chain
                    // into a following comparison.
                    let rhs = self.bit_expr()?;
                    e = Expr::IsDistinct(Box::new(e), Box::new(rhs), negated);
                }
                continue;
            }
            // `x LIKE pat` / `x NOT LIKE pat`. `NOT LIKE` ≡ `NOT (x LIKE pat)`
            // under 3VL (a NULL operand stays NULL through the outer NOT), so it
            // desugars here without a distinct AST node — the `NOT` needs the
            // two-token lookahead `not_expr` already passed on, like `NOT GLOB`.
            if !seen_cmp && (self.peek_kw(Kw::Like) || self.peek_not_like()) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::Like, "LIKE")?;
                let pat = self.bit_expr()?;
                // `LIKE … ESCAPE <char>`. `ESCAPE` is not a reserved word, but
                // sqlite's grammar admits nothing else in this position, so a
                // bare `escape` here can never be a column or an alias.
                let escape = if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("ESCAPE"))
                {
                    self.pos += 1;
                    Some(self.escape_char()?)
                } else {
                    None
                };
                let like = Expr::Like(Box::new(e), Box::new(pat), escape);
                e = if negated {
                    Expr::Unary(UnOp::Not, Box::new(like))
                } else {
                    like
                };
                seen_cmp = true;
                continue;
            }
            // `<col-or-table> MATCH <literal>` (FTS5). There is no `NOT MATCH`;
            // the RHS parses at the additive tier like `=`'s, so MATCH sits at
            // comparison precedence and does not chain. Whether it is legal
            // (an FTS table/column) is decided in the binder/planner, not here.
            if !seen_cmp && self.peek_kw(Kw::Match) {
                self.pos += 1;
                let pat = self.bit_expr()?;
                e = Expr::Match(Box::new(e), Box::new(pat));
                seen_cmp = true;
                continue;
            }
            // `x GLOB pat` / `x NOT GLOB pat` — the `NOT` is part of the
            // operator (like `NOT IN`/`NOT BETWEEN`), so it needs the two-token
            // lookahead that the higher-precedence `not_expr` already passed on.
            if !seen_cmp && (self.peek_kw(Kw::Glob) || self.peek_not_glob()) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::Glob, "GLOB")?;
                let pat = self.bit_expr()?;
                e = Expr::Glob(Box::new(e), Box::new(pat), negated);
                seen_cmp = true;
                continue;
            }
            // `x REGEXP pat` / `x NOT REGEXP pat` — same shape as GLOB: the `NOT`
            // is part of the operator, so it needs the two-token lookahead that
            // `not_expr` already passed on.
            if !seen_cmp && (self.peek_kw(Kw::Regexp) || self.peek_not_regexp()) {
                let negated = self.eat_kw(Kw::Not);
                self.expect_kw(Kw::Regexp, "REGEXP")?;
                let pat = self.bit_expr()?;
                e = Expr::Regexp(Box::new(e), Box::new(pat), negated);
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
            let r = self.bit_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
            seen_cmp = true;
        }
        Ok(e)
    }

    /// The bitwise tier: `&`, `|`, `<<`, `>>`, all at ONE precedence level and
    /// left-associative, sitting between the comparisons above and `+`/`-`
    /// below. That is sqlite's `parse.y` verbatim
    /// (`%left BITAND BITOR LSHIFT RSHIFT` between `%left GT LE LT GE` and
    /// `%left PLUS MINUS`) and it is observable: `1 + 2 | 4` is `(1+2) | 4` = 7,
    /// `2 | 1 << 2` is `(2|1) << 2` = 12, and `x = a | b` is `x = (a|b)`.
    ///
    /// Every comparison-tier right operand parses HERE rather than at the
    /// additive tier, which is what puts `a | b` on the right of `=`, `LIKE`,
    /// `BETWEEN` and friends.
    fn bit_expr(&mut self) -> Result<Expr> {
        let mut e = self.add_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::BitAnd) => BinOp::BitAnd,
                Some(Tok::BitOr) => BinOp::BitOr,
                Some(Tok::Shl) => BinOp::Shl,
                Some(Tok::Shr) => BinOp::Shr,
                _ => break,
            };
            self.pos += 1;
            let r = self.add_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
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
        let mut e = self.json_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let r = self.json_expr()?;
            e = Expr::Binary(op, Box::new(e), Box::new(r));
        }
        Ok(e)
    }

    /// `a -> b`, `a ->> b` — sqlite's JSON accessors. Its own tier, tighter
    /// than `*`, because that is where sqlite puts them, and left-associative
    /// so `doc -> '$.a' -> '$.b'` walks two levels (verified against 3.45.1).
    fn json_expr(&mut self) -> Result<Expr> {
        let mut e = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Arrow) => BinOp::JsonArrow,
                Some(Tok::ArrowText) => BinOp::JsonArrowText,
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
        } else if self.eat(&Tok::Tilde) {
            // `~` sits exactly where unary `-` does: sqlite declares
            // `%right BITNOT` and gives unary minus that same precedence, so
            // the two nest freely (`~-5` = 4, `-~5` = 6) and both bind tighter
            // than any infix operator (`~5 + 1` = -5).
            self.enter_expr()?;
            let e = self.unary_expr();
            self.exit_expr();
            Ok(Expr::Unary(UnOp::BitNot, Box::new(e?)))
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
            self.collate_expr()
        }
    }

    /// `<primary> [COLLATE <name>]*` — the postfix collation operator (task:
    /// COLLATE). It sits just below unary so it binds TIGHTER than every binary
    /// operator, matching the requirement that `x COLLATE NOCASE = y` parses as
    /// `(x COLLATE NOCASE) = y`. Chained `COLLATE`s associate left-to-right, so
    /// `x COLLATE A COLLATE B` is `(x COLLATE A) COLLATE B` (the outer wins, as
    /// in sqlite). `COLLATE`/the collation names are non-reserved words, so a
    /// column literally named `collate` still parses as an identifier here.
    fn collate_expr(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        while self.eat_word("COLLATE") {
            let name = self.ident("collation name after COLLATE")?;
            e = Expr::Collate(Box::new(e), name);
        }
        Ok(e)
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
        let lo = self.bit_expr()?;
        self.expect_kw(Kw::And, "AND in BETWEEN")?;
        let hi = self.bit_expr()?;
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
        // sqlite shorthand: `x IN <table>` == `x IN (SELECT * FROM <table>)`
        // (the table must have a single column; the InSubquery lift enforces it).
        if self.peek() != Some(&Tok::LParen) {
            if matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_))) {
                let tname = self.ident("table name after IN")?;
                let inner = SelectStmt {
                    table: Some(tname),
                    from_derived: None,
                    alias: None,
                    joins: Vec::new(),
                    distinct: false,
                    items: None,
                    where_clause: None,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                return Ok(Expr::InSubquery(
                    Box::new(e),
                    Box::new(SubqueryBody::Select(inner)),
                    negated,
                ));
            }
            return Err(self.err_here("`(` or a table name after IN"));
        }
        self.expect(&Tok::LParen, "`(` after IN")?;
        // `IN ()` — the EMPTY set — is accepted (sqlite allows it; PostgreSQL
        // does not, but accepting it rejects nothing PG accepts). It is FALSE
        // for every probe, NULL included (`NOT IN ()` TRUE) — the 3VL empty-set
        // rule. Compiles to a zero-element InList that evaluates the probe (so
        // its errors still surface), then yields FALSE.
        if self.eat(&Tok::RParen) {
            return Ok(Expr::InList(Box::new(e), Vec::new(), negated));
        }
        // `IN (SELECT …)` — membership in a subquery's output (#70). The
        // SELECT keyword right after the paren decides, same rule as the
        // scalar-subquery primary. The body may be a compound `SELECT … UNION …`
        // (#56/format 31).
        if matches!(self.peek(), Some(Tok::Kw(Kw::Select))) {
            let inner = self.subquery_body()?;
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
    /// Is `name` a HOST-registered aggregate (design/DESIGN-UDF.md stage 2)?
    ///
    /// Name-only, because the grammar branch is chosen before the arguments are
    /// read; the ARITY is checked afterwards against the actual argument count
    /// (`check_host_agg_arity`), so a 2-ary or variadic registration is called
    /// with as many arguments as it was written with. `count(*)`'s "the row
    /// itself" shape stays exclusive to `count`.
    fn host_agg_target(&mut self, name: &str) -> Option<mpedb_types::AggTarget> {
        let lname = name.to_ascii_lowercase();
        self.host_aggs
            .iter()
            .any(|(n, _)| *n == lname)
            .then_some(mpedb_types::AggTarget::Host(lname))
    }

    /// Post-parse arity gate for a host aggregate: the CALL's argument count
    /// must match a registration for this name (exact, or a variadic `-1`).
    fn check_host_agg_arity(&self, name: &str, argc: usize) -> Result<()> {
        if self.host_aggs.iter().any(|(n, a)| n == name && (*a == argc as i32 || *a == -1)) {
            return Ok(());
        }
        let arities: Vec<String> = self
            .host_aggs
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, a)| a.to_string())
            .collect();
        Err(self.err_here(format!(
            "{name}() is a user-defined aggregate registered for {} argument(s), \
             not {argc}",
            arities.join("/")
        )))
    }

    fn call_suffix(&mut self, name: String) -> Result<Expr> {
        self.expect(&Tok::LParen, "`(`")?;
        // Ranking window functions (`row_number`/`rank`/`dense_rank`) take no
        // argument and are meaningless without OVER — recognized here so they
        // never fall through to `Expr::Func` (which would then fail as an
        // unknown scalar) and so a bare `rank` column keeps working.
        if let Some(func) = window_rank_fn(&name) {
            if self.peek() != Some(&Tok::RParen) {
                return Err(self.err_here(format!(
                    "{name}() is a window function and takes no arguments"
                )));
            }
            self.expect(&Tok::RParen, "`)` closing the window function")?;
            if !self.peek_over() {
                return Err(self.err_here(format!(
                    "{name}() is a window function and requires an OVER clause"
                )));
            }
            let spec = self.window_over()?;
            return Ok(Expr::Window {
                func,
                arg: None,
                extra_args: Vec::new(),
                distinct: false,
                spec,
            });
        }
        // Aggregates are intercepted BEFORE the scalar argument parse, because
        // `count(*)` has an argument that is not an expression. `*` there is not
        // "all columns" — it means "the row itself", which is the whole reason
        // count(*) and count(x) differ on NULLs.
        //
        // A HOST aggregate (`xStep`/`xFinal`, design/DESIGN-UDF.md stage 2) joins
        // the built-ins here rather than falling through to `Expr::Func`: taking
        // the aggregate branch is what gives it DISTINCT, `FILTER (WHERE …)`,
        // and — decisively — an `Expr::Agg` node, which is what routes the whole
        // SELECT to the aggregate planner. A built-in name always wins, so no
        // registration can redefine `count`.
        let target = match agg_fn(&name) {
            Some(f) => Some(mpedb_types::AggTarget::Native(f)),
            None => self.host_agg_target(&name),
        };
        if let Some(target) = target {
            let f_native = target.native();
            // Arguments AFTER the first. Non-empty only for a HOST aggregate
            // registered with arity > 1 — sqlite's `create_aggregate(name, N,
            // cls)` is an N-ary contract and CPython's own suite registers 2-ary
            // and variadic ones. Every built-in leaves this empty.
            let mut host_extra: Vec<Expr> = Vec::new();
            let (arg, distinct): (Option<Box<Expr>>, bool) = if self.eat(&Tok::Star) {
                self.expect(&Tok::RParen, "`)` closing count(*)")?;
                if f_native != Some(mpedb_types::AggFn::Count) {
                    return Err(self.err_here(format!(
                        "{}(*) is not valid — only count(*) takes the row itself; \
                         {}() needs a value",
                        target.name(),
                        target.name()
                    )));
                }
                (None, false)
            } else {
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
                        target.name()
                    )));
                }
                let arg = self.expr()?;
                // A comma after the first argument of a HOST aggregate opens the
                // rest of ITS argument list. Only a host name takes this branch:
                // a built-in still falls through to the `min(a,b)`/`max(a,b)`
                // scalar rule and then to the one-argument error, so no built-in
                // grammar changes. `DISTINCT` is excluded — sqlite has no
                // multi-argument DISTINCT aggregate, and the dedup key here is
                // the single argument.
                let host_nary = self.peek() == Some(&Tok::Comma)
                    && target.host().is_some()
                    && !distinct;
                if host_nary {
                    while self.eat(&Tok::Comma) {
                        host_extra.push(self.expr()?);
                    }
                    self.expect(&Tok::RParen, "`)` closing the argument list")?;
                } else if self.peek() == Some(&Tok::Comma) {
                    // `max(a, b)` / `min(a, b)` are sqlite's SCALAR forms — a
                    // different C function from the aggregates of the same name
                    // (`minmaxFunc` vs `minmaxStep`), routed here on ARITY, which
                    // is exactly how sqlite's own function table resolves them
                    // (`FUNCTION(min,1,…)` registers the aggregate and
                    // `FUNCTION(min,-1,…)` the scalar).
                    //
                    // Three things this must NOT do, and each is tested:
                    // `max(x)` stays the aggregate (no comma, never reaches
                    // here); `max(DISTINCT x)` stays the aggregate (DISTINCT is
                    // aggregate-only grammar, so it takes the arity error
                    // below); and a HOST aggregate registered as `max` keeps its
                    // own arity rule (`f_native` is None for it).
                    if !distinct
                        && matches!(
                            f_native,
                            Some(mpedb_types::AggFn::Min) | Some(mpedb_types::AggFn::Max)
                        )
                    {
                        let mut args = vec![arg];
                        while self.eat(&Tok::Comma) {
                            args.push(self.expr()?);
                        }
                        self.expect(&Tok::RParen, "`)` closing the argument list")?;
                        if self.peek_filter_paren() || self.peek_over() {
                            return Err(self.err_here(format!(
                                "{}() with {} arguments is the SCALAR form and takes no FILTER \
                                 or OVER clause — those belong to the one-argument aggregate \
                                 (sqlite: \"max() may not be used as a window function\")",
                                target.name(),
                                args.len()
                            )));
                        }
                        // The generic scalar-call node; the binder resolves the
                        // name to `ScalarFn::Max2`/`Min2`. Only ever built with
                        // two or more arguments, which is what keeps the
                        // one-argument aggregate unreachable from here.
                        return Ok(Expr::Func(target.name().to_ascii_lowercase(), args));
                    }
                    return Err(self.err_here(format!(
                        "{}() takes exactly one argument",
                        target.name()
                    )));
                } else {
                    self.expect(&Tok::RParen, "`)` closing the argument list")?;
                }
                (Some(Box::new(arg)), distinct)
            };
            if let Some(hname) = target.host() {
                self.check_host_agg_arity(hname, arg.is_some() as usize + host_extra.len())?;
            }
            // An OPTIONAL trailing `FILTER (WHERE <cond>)` (sqlite 3.30+/PG):
            // the aggregate accumulates only the rows where `cond` is TRUE. In
            // the grammar FILTER precedes OVER. `FILTER` is NOT a reserved word,
            // so it is a keyword ONLY when immediately followed by `(` — a bare
            // `count(*) filter` (no paren) is `filter` used as an output ALIAS,
            // exactly as sqlite/PG parse it, so peek for the paren before
            // committing. `(`, `WHERE`, the predicate, and `)` are then mandatory.
            let filter = if self.peek_filter_paren() {
                self.pos += 1; // FILTER
                self.expect(&Tok::LParen, "`(` after FILTER")?;
                self.expect_kw(Kw::Where, "WHERE inside `FILTER (WHERE …)`")?;
                let cond = self.expr()?;
                self.expect(&Tok::RParen, "`)` closing `FILTER (WHERE …)`")?;
                Some(Box::new(cond))
            } else {
                None
            };
            // An `OVER` clause turns the aggregate into a WINDOW aggregate
            // (stage 1b): the same `AggFn`, but computed over a partition with
            // every row surviving rather than collapsed into one. FILTER on a
            // window aggregate is standard SQL but mpedb refuses it — only plain
            // grouped/scalar aggregates carry a FILTER.
            if self.peek_over() {
                if filter.is_some() {
                    return Err(self.err_here(
                        "FILTER (WHERE …) on a window aggregate (OVER …) is not supported",
                    ));
                }
                // A plain `xStep`/`xFinal` HOST aggregate still has no window
                // form: it cannot be rewound over a moving frame. sqlite draws
                // exactly the same line — a window registration is the separate
                // `create_window_function`, which ALSO supplies `xValue` and
                // `xInverse`. Only a name registered that way (`window_aggs`)
                // takes an OVER clause; anything else is refused here rather
                // than silently answered with a whole-partition value.
                let func = match f_native {
                    Some(f) => WindowFunc::Agg(f),
                    None => {
                        let hname = target.name().to_ascii_lowercase();
                        if !self.window_aggs.contains(&hname) {
                            return Err(self.err_here(format!(
                                "{}() is a user-defined aggregate and cannot be used with \
                                 OVER — a window function must be registered with the \
                                 inverse/value pair (sqlite's create_window_function)",
                                target.name()
                            )));
                        }
                        // The sliding protocol feeds `xInverse` the SAME
                        // arguments a row was stepped with, one row at a time,
                        // so only the single-argument shape is expressible here.
                        if arg.is_none() || !host_extra.is_empty() {
                            return Err(self.err_here(format!(
                                "{}() OVER (…) takes exactly one argument",
                                target.name()
                            )));
                        }
                        WindowFunc::Host(hname)
                    }
                };
                let spec = self.window_over()?;
                return Ok(Expr::Window {
                    func,
                    arg,
                    extra_args: Vec::new(),
                    distinct,
                    spec,
                });
            }
            return Ok(Expr::Agg(target, arg, distinct, filter, host_extra));
        }
        let mut args = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            args.push(self.expr()?);
            while self.eat(&Tok::Comma) {
                args.push(self.expr()?);
            }
        }
        self.expect(&Tok::RParen, "`)` closing the argument list")?;
        // Value/offset window functions (stage 2): lag/lead/first_value/
        // last_value/nth_value. They take a real expression argument list
        // (parsed just above) and, like the ranking functions, are ONLY valid as
        // window functions — so an `OVER` clause is required and the argument
        // arity is fixed per function. Their first argument is the value `expr`;
        // any trailing arguments (a lag/lead offset+default, an nth_value n) ride
        // in `extra_args`.
        if let Some((func, min, max)) = window_value_fn(&name) {
            let lname = name.to_ascii_lowercase();
            if !self.peek_over() {
                return Err(self.err_here(format!(
                    "{lname}() may only be used as a window function — it requires an OVER clause"
                )));
            }
            if args.len() < min || args.len() > max {
                let want = if min == max {
                    format!("exactly {min} argument(s)")
                } else {
                    format!("{min} to {max} arguments")
                };
                return Err(self.err_here(format!(
                    "{lname}() takes {want}, got {}",
                    args.len()
                )));
            }
            let spec = self.window_over()?;
            let mut it = args.into_iter();
            // `min >= 1` for every value function, so the value `expr` is present.
            let arg = Box::new(it.next().expect("value window function has an expr argument"));
            let extra_args: Vec<Expr> = it.collect();
            return Ok(Expr::Window {
                func,
                arg: Some(arg),
                extra_args,
                distinct: false,
                spec,
            });
        }
        // ntile(n) (stage 2b): a DISTRIBUTION function. Its single argument is the
        // bucket count `n` (a constant integer ≥ 1, folded at plan time), NOT a
        // per-row value expression — so it rides in `extra_args` with `arg =
        // None`, like the argument-less ranking functions. Only valid as a window
        // function (OVER required); the planner further requires an ORDER BY,
        // since the bucket assignment is otherwise order-dependent.
        if name.eq_ignore_ascii_case("ntile") {
            if !self.peek_over() {
                return Err(self.err_here(
                    "ntile() may only be used as a window function — it requires an OVER clause",
                ));
            }
            if args.len() != 1 {
                return Err(self.err_here(format!(
                    "ntile() takes exactly one argument (the bucket count), got {}",
                    args.len()
                )));
            }
            let spec = self.window_over()?;
            return Ok(Expr::Window {
                func: WindowFunc::Ntile,
                arg: None,
                extra_args: args,
                distinct: false,
                spec,
            });
        }
        // A scalar function call followed by OVER is not a window function — every
        // supported window function (ranking, distribution, aggregate `OVER`, and
        // value/offset) is handled above. Refuse it by name rather than misread
        // the OVER.
        if self.peek_over() {
            return Err(self.err_here(format!(
                "`{}` is not a window function — only row_number/rank/dense_rank, \
                 ntile/percent_rank/cume_dist, aggregate `OVER`, and \
                 lag/lead/first_value/last_value/nth_value are available",
                name.to_ascii_lowercase()
            )));
        }
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

    /// The next token is a bare `OVER` word immediately followed by `(` — the
    /// start of a window spec. `OVER` is positional (not reserved), so a column
    /// named `over` is unaffected.
    fn peek_over(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("OVER"))
            && matches!(self.peek_at(1), Some(Tok::LParen))
    }

    /// The next token is a bare `FILTER` word immediately followed by `(` — the
    /// start of an aggregate `FILTER (WHERE …)` clause. `FILTER` is positional
    /// (not reserved), so a bare `filter` NOT followed by `(` stays an ordinary
    /// identifier / output alias, exactly as sqlite and PostgreSQL parse it.
    fn peek_filter_paren(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("FILTER"))
            && matches!(self.peek_at(1), Some(Tok::LParen))
    }

    /// Consume the `OVER` word (its presence guaranteed by [`Self::peek_over`])
    /// and parse the window spec.
    fn window_over(&mut self) -> Result<WindowSpecAst> {
        self.pos += 1; // OVER
        self.window_spec()
    }

    /// `( [PARTITION BY <expr>, …] [ORDER BY <expr> [ASC|DESC], …] [frame] )`.
    /// `PARTITION` is a positional word (not reserved), so a column of that name
    /// still works; the `ORDER`/`BY`/`ASC`/`DESC` keywords are reused. A trailing
    /// `ROWS`/`RANGE`/`GROUPS` clause is parsed as an explicit frame
    /// ([`Self::window_frame`]); its semantics are validated by the planner.
    fn window_spec(&mut self) -> Result<WindowSpecAst> {
        self.expect(&Tok::LParen, "`(` after OVER")?;
        let mut partition_by = Vec::new();
        if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("PARTITION")) {
            self.pos += 1;
            self.expect_kw(Kw::By, "BY after PARTITION")?;
            loop {
                partition_by.push(self.expr()?);
                if partition_by.len() > MAX_ORDER_BY_ITEMS {
                    return Err(self.err_here(format!(
                        "too many PARTITION BY items (max {MAX_ORDER_BY_ITEMS})"
                    )));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        if self.eat_kw(Kw::Order) {
            self.expect_kw(Kw::By, "BY after ORDER")?;
            loop {
                let key = self.expr()?;
                let desc = if self.eat_kw(Kw::Desc) {
                    true
                } else {
                    self.eat_kw(Kw::Asc);
                    false
                };
                // A WINDOW's ORDER BY carries only a direction — the window
                // frame, the peer groups and the `dirs` comparator in the
                // executor are all built on that, and none of them has a NULL
                // placement to honour. sqlite accepts `NULLS FIRST/LAST` here;
                // mpedb refuses it BY NAME rather than accepting it and sorting
                // the partition sqlite's default way regardless, which would be
                // a silently wrong row order. (Statement-level `ORDER BY …
                // NULLS FIRST/LAST` IS supported — see `Parser::sort_dir`.)
                if matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("NULLS")) {
                    return Err(self.err_here(
                        "NULLS FIRST/LAST inside OVER (ORDER BY …) is not supported yet — \
                         only ASC / DESC (the statement's own ORDER BY does support it)",
                    ));
                }
                order_by.push((key, desc));
                if order_by.len() > MAX_ORDER_BY_ITEMS {
                    return Err(self.err_here(format!(
                        "too many ORDER BY items in OVER (max {MAX_ORDER_BY_ITEMS})"
                    )));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let frame = self.window_frame()?;
        self.expect(&Tok::RParen, "`)` closing the window spec")?;
        Ok(WindowSpecAst {
            partition_by,
            order_by,
            frame,
        })
    }

    /// An optional explicit frame: `{ROWS | RANGE | GROUPS} ( <start-bound> |
    /// BETWEEN <start-bound> AND <end-bound> )`. `ROWS`/`RANGE`/`GROUPS`,
    /// `BETWEEN`'s bound words (`UNBOUNDED`/`PRECEDING`/`CURRENT`/`ROW`/
    /// `FOLLOWING`) are all positional words (not reserved), so columns of those
    /// names still work outside an OVER clause. The shorthand `{…} <start>`
    /// desugars to `BETWEEN <start> AND CURRENT ROW`. Boundary legality (a start
    /// that isn't UNBOUNDED FOLLOWING, an end that isn't UNBOUNDED PRECEDING, and
    /// start ≤ end) is enforced by the planner, so its message names the exact
    /// problem uniformly.
    fn window_frame(&mut self) -> Result<Option<FrameAst>> {
        let mode = if self.eat_word("ROWS") {
            FrameMode::Rows
        } else if self.eat_word("RANGE") {
            FrameMode::Range
        } else if self.eat_word("GROUPS") {
            FrameMode::Groups
        } else {
            return Ok(None);
        };
        let (start, end) = if self.eat_kw(Kw::Between) {
            let start = self.frame_bound()?;
            self.expect_kw(Kw::And, "AND in a BETWEEN frame")?;
            let end = self.frame_bound()?;
            (start, end)
        } else {
            // Shorthand: `{ROWS|RANGE|GROUPS} <start>` ≡ `BETWEEN <start> AND
            // CURRENT ROW`.
            (self.frame_bound()?, FrameBound::CurrentRow)
        };
        Ok(Some(FrameAst { mode, start, end }))
    }

    /// One frame boundary: `UNBOUNDED PRECEDING`, `<N> PRECEDING`, `CURRENT ROW`,
    /// `<N> FOLLOWING`, or `UNBOUNDED FOLLOWING`. `<N>` is a non-negative integer
    /// literal (a non-constant or non-integer offset is refused, since the
    /// content-hashed plan bakes it in — a per-row/parameter offset would be
    /// version-brittle).
    fn frame_bound(&mut self) -> Result<FrameBound> {
        if self.eat_word("UNBOUNDED") {
            if self.eat_word("PRECEDING") {
                return Ok(FrameBound::UnboundedPreceding);
            }
            if self.eat_word("FOLLOWING") {
                return Ok(FrameBound::UnboundedFollowing);
            }
            return Err(self.err_here("expected PRECEDING or FOLLOWING after UNBOUNDED"));
        }
        if self.eat_word("CURRENT") {
            if self.eat_word("ROW") {
                return Ok(FrameBound::CurrentRow);
            }
            return Err(self.err_here("expected ROW after CURRENT"));
        }
        // `<N> PRECEDING | FOLLOWING`. Only a bare non-negative integer literal is
        // accepted; a negative sign, a parameter, or any expression is a clean
        // parse error here (the offset must be a constant baked into the plan).
        if let Some(&Tok::Int(n)) = self.peek() {
            self.pos += 1;
            let n = n as u64; // Tok::Int is always ≥ 0 (a sign is a separate token)
            if self.eat_word("PRECEDING") {
                return Ok(FrameBound::Preceding(n));
            }
            if self.eat_word("FOLLOWING") {
                return Ok(FrameBound::Following(n));
            }
            return Err(self.err_here("expected PRECEDING or FOLLOWING after a frame offset"));
        }
        Err(self.err_here(
            "expected a frame boundary (UNBOUNDED PRECEDING, <N> PRECEDING, CURRENT ROW, \
             <N> FOLLOWING, or UNBOUNDED FOLLOWING)",
        ))
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
                    return self.dot_suffix(s);
                }
                Ok(Expr::Col(s))
    }

    /// `<qualifier> . <column>` — the caller has seen a bare or quoted
    /// identifier and `Dot` is next. Both ancestors accept the form, so mpedb
    /// does too: the qualifier names a table or alias in scope and the binder
    /// resolves the pair.
    ///
    /// Quoting is irrelevant HERE by construction — the caller passes the
    /// already-unquoted qualifier text and `ident()` accepts either spelling for
    /// the part after the dot — which is what makes `"t"."c"`, `"t".c`, `t."c"`,
    /// `` `t`.`c` ``, `[t].[c]` and `t.c` one grammar rather than six.
    ///
    /// A THIRD dot (`db.t.c`) is refused by name. sqlite reads it as
    /// schema-qualified, but mpedb's only schema qualifier is the Workspace
    /// alias, which `split_db_alias` strips off a TABLE reference and never off
    /// a column — so accepting it here could only ever guess.
    fn dot_suffix(&mut self, qualifier: String) -> Result<Expr> {
        self.pos += 1; // the `.`
        // `t.*` is per-table star expansion, a different feature from a
        // qualified column — name it, rather than emit "expected a column name"
        // at a `*` the writer put there on purpose.
        if self.peek() == Some(&Tok::Star) {
            return Err(self.err_here(format!(
                "`{qualifier}.*` (per-table star expansion) is not supported — \
                 use a bare `*`, or list the columns"
            )));
        }
        let col = self.ident("column name after `.`")?;
        if self.peek() == Some(&Tok::Dot) {
            return Err(self.err_here(format!(
                "a three-part name (`{qualifier}.{col}.…`) is not supported — \
                 qualify a column with its table or alias only"
            )));
        }
        Ok(Expr::Qualified(qualifier, col))
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
            let inner = self.subquery_body()?;
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
            let tyname = self.type_name()?;
            self.expect(&Tok::RParen, ") after CAST")?;
            return Ok(Expr::Cast(Box::new(e), tyname));
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
            // A quoted identifier is a COLUMN, never a function/`excluded.`/
            // `current_setting()` — quoting is precisely how you say "this is a
            // name, not a word with a meaning". It does take the qualifier dot,
            // though: `"t"."c"` must be exactly `t.c` (Django quotes every
            // identifier it emits, so this is the difference between working
            // and not).
            Some(Tok::QuotedIdent(s)) => {
                if self.peek() == Some(&Tok::Dot) {
                    return self.dot_suffix(s);
                }
                Ok(Expr::Col(s))
            }
            Some(Tok::LParen) => {
                // `(SELECT …)` is a scalar subquery, not a parenthesized
                // expression — the SELECT keyword right after `(` decides. The
                // body may be a compound `SELECT … UNION … LIMIT 1` (#56/format 31).
                if matches!(self.peek(), Some(Tok::Kw(Kw::Select))) {
                    let inner = self.subquery_body()?;
                    self.expect(&Tok::RParen, "`)` after subquery")?;
                    return Ok(Expr::Subquery(Box::new(inner)));
                }
                let first = self.expr()?;
                // A comma here makes this a ROW VALUE (tuple): `(e1, e2, …)` with
                // ≥2 elements — the operand of a row-value comparison
                // `(a, b) = (c, d)` / `< <= > >=` (keyset pagination). A single
                // `(expr)` stays plain grouping. This does NOT affect `(SELECT …)`
                // (handled above), function-call argument lists, `IN (…)` lists,
                // or `VALUES (…)` — each of those consumes its own `(` elsewhere
                // and never reaches this atom-level paren.
                if self.peek() == Some(&Tok::Comma) {
                    let mut items = vec![first];
                    while self.eat(&Tok::Comma) {
                        items.push(self.expr()?);
                    }
                    self.expect(&Tok::RParen, "`)` closing a row value")?;
                    return Ok(Expr::RowValue(items));
                }
                self.expect(&Tok::RParen, "`)`")?;
                Ok(first)
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
