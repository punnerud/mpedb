//! SELECT / compound-SELECT / FROM / JOIN grammar for the recursive-descent
//! parser, plus the standalone `VALUES` statement that desugars into a compound
//! of FROM-less SELECTs.
//!
//! Split out of [`super`] to keep that file under the size limit. The shared
//! [`Parser`] token helpers live in `super` and stay reachable here because
//! `parser::select` is a descendant module. `select_stmt`, `values_stmt`,
//! `select_core` and `eat_all_quantifier` are `pub(super)` so the statement
//! dispatch, the DML grammar and the expression grammar can reach them.

use super::{Parser, MAX_COMPOUND_ARMS, MAX_ORDER_BY_ITEMS, MAX_SELECT_ITEMS};
use crate::ast::{CompoundStmt, Expr, JoinClause, JoinKind, SelectStmt, Stmt};
use crate::plan::{SetOp, SortDir};
use crate::token::{Kw, Tok};
use mpedb_types::{ident_eq, Result, Value};

/// Re-spell a bare ORDER BY key to the SELECT-item alias it names, when the two
/// differ only in ASCII case.
///
/// ORDER BY is the ONE place where an output alias outranks a base column of
/// the same name — `SELECT a AS b FROM t ORDER BY b` sorts by `a`, even though
/// the table has its own `b` (measured; GROUP BY and HAVING do NOT do this, and
/// resolve against the base columns). The planner implements that precedence by
/// comparing the key against each item's alias, and it compares them EXACTLY.
///
/// That exactness was harmless while column lookup was exact too: `ORDER BY B`
/// simply failed to resolve at all. Once column lookup folds ASCII case, the
/// alias test misses, the key falls through to the base column, and the query
/// silently sorts by the WRONG column — a refusal turned into a wrong answer,
/// which is the specific hazard this whole change had to avoid. Normalizing the
/// key's spelling here restores the precedence for every consumer of the AST at
/// once (plain, joined, windowed and DISTINCT ORDER BY all share these fields).
///
/// Only a BARE identifier is rewritten. A qualified `t.B` or an expression
/// `B+0` names the base column even when an alias `b` exists (both measured),
/// and both are left alone — matching the planner's own `Expr::Col` guard.
/// First match wins, as `position` does downstream: with `a AS x, b AS X`,
/// `ORDER BY x` takes the first.
///
/// This rewrites the *sort key*, never a stored or reported name.
fn align_order_by_alias_case(
    items: &Option<Vec<(Expr, Option<String>)>>,
    order_by: &mut [(Expr, SortDir)],
) {
    let Some(items) = items else { return };
    for (key, _) in order_by.iter_mut() {
        let Expr::Col(n) = key else { continue };
        // An EXACT alias already resolves; only a case-differing one needs help.
        if items.iter().any(|it| it.1.as_deref() == Some(n.as_str())) {
            continue;
        }
        if let Some(alias) = items
            .iter()
            .filter_map(|it| it.1.as_deref())
            .find(|a| ident_eq(a, n))
        {
            *n = alias.to_owned();
        }
    }
}

/// A FROM-less `SELECT <items>` — reads no table and evaluates its items over
/// ONE synthetic row (the #67 DUAL sentinel). The building block a standalone
/// `VALUES` row desugars into.
fn from_less_select(items: Vec<(Expr, Option<String>)>) -> SelectStmt {
    SelectStmt {
        table: None,
        from_derived: None,
        alias: None,
        joins: Vec::new(),
        distinct: false,
        items: Some(items),
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    drop_trailing: 0,
    }
}

impl<'a> Parser<'a> {
    /// Standalone `VALUES (a, b), (c, d), …` — a top-level row-returning
    /// statement (sqlite). Desugared HERE, at parse time, into the equivalent
    /// compound `SELECT a, b UNION ALL SELECT c, d UNION ALL …` of FROM-less
    /// SELECTs (#67's DUAL sentinel), so it reuses the existing compound
    /// planner/executor with ZERO plan-format change. Rules mirror sqlite:
    /// every tuple has the SAME arity, at least one tuple, and the output
    /// columns are named `column1..columnN` — aliases set on the FIRST arm,
    /// since a compound takes its output names from arm 0. A single tuple is a
    /// plain `Stmt::Select`; more than one becomes a `UNION ALL` compound.
    ///
    /// Only the top-level statement form is handled here. `VALUES` as a
    /// subquery/derived-table source (`FROM (VALUES …)`) is not: a multi-row
    /// VALUES is a compound, which a derived-table body (a single `SelectStmt`)
    /// cannot hold, and wiring that would reach into the view-flatten pass.
    pub(super) fn values_stmt(&mut self) -> Result<Stmt> {
        self.expect_kw(Kw::Values, "VALUES")?;
        let mut arms: Vec<SelectStmt> = Vec::new();
        loop {
            self.expect(&Tok::LParen, "`(` starting a VALUES row")?;
            let mut row = vec![self.expr()?];
            while self.eat(&Tok::Comma) {
                if row.len() >= MAX_SELECT_ITEMS {
                    return Err(self.err_here(format!(
                        "too many columns in a VALUES row (max {MAX_SELECT_ITEMS})"
                    )));
                }
                row.push(self.expr()?);
            }
            self.expect(&Tok::RParen, "`)` closing a VALUES row")?;
            // Every row must have the arity of the first — sqlite/PG both
            // reject a ragged VALUES rather than NULL-padding it.
            if let Some(first) = arms.first() {
                let want = first.items.as_ref().map_or(0, Vec::len);
                if row.len() != want {
                    return Err(self.err_here(format!(
                        "all VALUES rows must have the same number of columns \
                         (the first row has {want}, this one has {})",
                        row.len()
                    )));
                }
            }
            // Arm 0 names the output columns `column1..N` (sqlite's names); the
            // later arms only supply values — a compound's output names come
            // from its first arm, so naming them there would be dead weight.
            let first_arm = arms.is_empty();
            let items = row
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let alias = first_arm.then(|| format!("column{}", i + 1));
                    (e, alias)
                })
                .collect();
            arms.push(from_less_select(items));
            if !self.eat(&Tok::Comma) {
                break;
            }
            // The desugaring targets the compound-SELECT machinery, which caps
            // its arms at the plan decoder's limit; a VALUES with more rows than
            // that has no plan representation, so refuse it clearly.
            if arms.len() >= MAX_COMPOUND_ARMS {
                return Err(self.err_here(format!(
                    "too many VALUES rows (max {MAX_COMPOUND_ARMS})"
                )));
            }
        }
        if arms.len() == 1 {
            return Ok(Stmt::Select(arms.into_iter().next().expect("one arm")));
        }
        let ops = vec![SetOp::UnionAll; arms.len() - 1];
        Ok(Stmt::Compound(CompoundStmt {
            arms,
            ops,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }))
    }

    /// `SELECT …`, or a compound chain `SELECT … UNION [ALL]/EXCEPT/INTERSECT
    /// SELECT …`. Ops apply left-associatively with equal precedence (sqlite's
    /// rule; PostgreSQL binds INTERSECT tighter — documented deviation).
    pub(super) fn select_stmt(&mut self) -> Result<Stmt> {
        let first = self.select_core()?;
        if self.peek_compound_op().is_none() {
            return Ok(Stmt::Select(first));
        }
        Ok(Stmt::Compound(self.compound_chain(first)?))
    }

    /// A subquery BODY used as a value/list/existence — a scalar `(…)`,
    /// `x IN (…)` or `EXISTS (…)`. A plain `SELECT`, or a whole compound
    /// `SELECT … UNION/EXCEPT/INTERSECT … [ORDER BY … LIMIT …]` (#56 in a
    /// subquery position). The caller has already consumed the opening `(` and
    /// consumes the closing `)` afterward, so a trailing ORDER BY/LIMIT here
    /// belongs to the compound, exactly as in a top-level compound.
    pub(super) fn subquery_body(&mut self) -> Result<crate::ast::SubqueryBody> {
        use crate::ast::SubqueryBody;
        let first = self.select_core()?;
        if self.peek_compound_op().is_none() {
            return Ok(SubqueryBody::Select(first));
        }
        Ok(SubqueryBody::Compound(self.compound_chain(first)?))
    }

    /// Parse the set-operator chain that follows an already-parsed first arm
    /// (the caller has confirmed a compound op is next). Shared by a top-level
    /// compound statement and a compound subquery body, so the arm rules — same
    /// left-associative precedence, ORDER BY/LIMIT only after the last arm — can
    /// never drift between the two positions.
    fn compound_chain(&mut self, first: SelectStmt) -> Result<CompoundStmt> {
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
            if !prev.order_by.is_empty()
                || prev.limit.is_some()
                || prev.offset.is_some()
                || self.neg_limit_in_core
            {
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
        Ok(CompoundStmt { arms, ops, order_by, limit, offset })
    }

    /// Eat the no-op `ALL` quantifier (the explicit opposite of DISTINCT):
    /// `SELECT ALL x`, `count(ALL x)`. Positional word, consumed only when an
    /// expression can follow — `SELECT all FROM t` still names a column, and
    /// `count(all)` still counts one.
    pub(super) fn eat_all_quantifier(&mut self) {
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

    pub(super) fn select_core(&mut self) -> Result<SelectStmt> {
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
        let (table, from_derived, from_alias, joins) = if self.eat_kw(Kw::From) {
            // A `(` immediately followed by SELECT is a derived table
            // `FROM (SELECT …) [AS] alias` (#74) — distinct from a `( join
            // group )`, whose paren wraps table names. The view-inline pass
            // flattens a simple derived body before planning; the rest refuse.
            let (table, from_derived, from_alias, mut from_parens) = if self.peek()
                == Some(&Tok::LParen)
                && matches!(self.peek_at(1), Some(Tok::Kw(Kw::Select)))
            {
                self.expect(&Tok::LParen, "(")?;
                // A plain SELECT body, or a whole compound `SELECT … UNION …`
                // (the same grammar a subquery position accepts). A compound
                // body cannot be flattened onto a base table, so it is always
                // MATERIALIZED (design/DESIGN-DERIVED-TABLES.md §5).
                let inner = self.subquery_body()?;
                self.expect(&Tok::RParen, "`)` to close the derived table")?;
                // The alias names the derived columns; optional, as in sqlite
                // (accept bare or `AS` form here).
                let from_alias = self.opt_table_alias()?;
                (None, Some(Box::new(inner)), from_alias, 0usize)
            } else {
                // `FROM ( a JOIN b ON … )` — parens around a join group. For the
                // left-deep chains this grammar builds they are associativity
                // no-ops, so opening parens are counted and their closers
                // consumed between join steps. (A paren group as the INNER side
                // of a join — `a JOIN (b JOIN c)` — is NOT expressible left-deep
                // and stays a parse error.)
                let mut from_parens = 0usize;
                while self.eat(&Tok::LParen) {
                    from_parens += 1;
                }
                let table = self.ident("table name")?;
                let from_alias = self.opt_table_alias()?;
                (Some(table), None, from_alias, from_parens)
            };
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
                        using: Vec::new(),
                        natural: false,
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
                        using: Vec::new(),
                        natural: false,
                    });
                } else if matches!(self.peek_join_kind(), Some("NATURAL")) {
                    joins.push(self.natural_join()?);
                } else if let Some(kind) = self.peek_join_kind() {
                    // A stray side word (a bare `OUTER JOIN`, or a join kind we
                    // do not accept in this position): the ON condition it would
                    // need is not here, so refuse rather than guess.
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
            (table, from_derived, from_alias, joins)
        } else {
            (None, None, None, Vec::new())
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
                order_by.push((col, self.sort_dir()?));
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
        align_order_by_alias_case(&items, &mut order_by);
        self.neg_limit_in_core = false;
        let limit = if self.eat_kw(Kw::Limit) {
            self.limit_int("LIMIT")?
        } else {
            None
        };
        let offset = if self.eat_kw(Kw::Offset) {
            // A negative OFFSET skips nothing (sqlite clamps it to 0), which
            // `Some(0)` says exactly.
            Some(self.limit_int("OFFSET")?.unwrap_or(0))
        } else {
            None
        };
        Ok(SelectStmt {
            table,
            from_derived,
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
        drop_trailing: 0,
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
        // `USING` is likewise positional (`JOIN b USING (id)`): reading it as an
        // alias for `b` would lose the join condition. Quote it to name a table.
        let is_using = matches!(self.peek(), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("USING"));
        if matches!(self.peek(), Some(Tok::Ident(_)))
            && !is_using
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
        // The join condition — either `ON <cond>` or `USING (c1, …)`. `USING` is
        // a positional word (not a keyword), so a table/column named `using` is
        // unaffected; NATURAL (the implicit USING over all common columns) is
        // parsed by `natural_join`, not here. The desugaring to
        // `left.ci = right.ci AND …` and the `SELECT *` coalescing happen at plan
        // time (the LEFT qualifier needs the schema) — here we only capture the
        // columns.
        if self.eat_word("USING") {
            // RIGHT/FULL USING would have to carry the coalesced column through
            // the side-swap (RIGHT→LEFT) and both-sides-whole (FULL) rewrites;
            // refuse rather than silently mis-order the output.
            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                return Err(self.err_here(
                    "JOIN … USING is only supported on [INNER] JOIN and LEFT JOIN — \
                     write the ON condition explicitly for RIGHT/FULL joins",
                ));
            }
            let using = self.using_columns()?;
            return Ok(JoinClause {
                table,
                alias,
                kind,
                on: Expr::Lit(Value::Bool(true)),
                using,
                natural: false,
            });
        }
        // ON is otherwise required. A comma-join / cross join is a cartesian
        // product, and the times someone means one are far outnumbered by the
        // times they forgot the condition.
        self.expect_kw(Kw::On, "ON after JOIN — the join condition is required (or USING (…))")?;
        let on = self.expr()?;
        Ok(JoinClause { table, alias, kind, on, using: Vec::new(), natural: false })
    }

    /// `NATURAL [INNER | LEFT [OUTER]] JOIN <table> [alias]` — the join condition
    /// is IMPLICIT: an equality over every column common to the two sides. That
    /// set is a fact about the schema, which a rigid schema makes static, but it
    /// is not known here — so we carry only `natural` and leave `using` empty for
    /// the planner to fill before its USING→ON desugar (`join.rs`). RIGHT / FULL /
    /// CROSS are refused for the SAME reason `JOIN … USING` refuses them: the
    /// coalesced column cannot survive the side-swap / both-sides-whole rewrites.
    fn natural_join(&mut self) -> Result<JoinClause> {
        self.pos += 1; // consume NATURAL
        let kind = if self.eat_kw(Kw::Inner) {
            JoinKind::Inner
        } else if self.eat_word("LEFT") {
            let _ = self.eat_word("OUTER");
            JoinKind::Left
        } else if self.eat_word("RIGHT")
            || self.eat_word("FULL")
            || matches!(self.peek_join_kind(), Some("CROSS"))
        {
            return Err(self.err_here(
                "NATURAL is only supported on [INNER] JOIN and NATURAL LEFT JOIN — \
                 write the ON / USING condition explicitly for RIGHT/FULL/CROSS joins",
            ));
        } else {
            // Bare `NATURAL JOIN` is an inner join.
            JoinKind::Inner
        };
        self.expect_kw(Kw::Join, "JOIN after NATURAL")?;
        let table = self.ident("table name after NATURAL JOIN")?;
        let alias = self.opt_table_alias()?;
        Ok(JoinClause {
            table,
            alias,
            kind,
            on: Expr::Lit(Value::Bool(true)),
            using: Vec::new(),
            natural: true,
        })
    }

    /// `(c1, c2, …)` — the non-empty column list of a `JOIN … USING`. Bare or
    /// quoted identifiers; the plan-time desugar checks each one exists in BOTH
    /// sides.
    fn using_columns(&mut self) -> Result<Vec<String>> {
        self.expect(&Tok::LParen, "`(` after USING")?;
        let mut cols = vec![self.ident("column name in USING")?];
        while self.eat(&Tok::Comma) {
            if cols.len() >= MAX_SELECT_ITEMS {
                return Err(self.err_here(format!(
                    "too many USING columns (max {MAX_SELECT_ITEMS})"
                )));
            }
            cols.push(self.ident("column name in USING")?);
        }
        self.expect(&Tok::RParen, "`)` closing the USING column list")?;
        Ok(cols)
    }

    /// A `LIMIT` / `OFFSET` value. An integer literal, optionally signed:
    /// sqlite reads a NEGATIVE `LIMIT` as "no limit" and a negative `OFFSET`
    /// as "skip nothing" (`LIMIT -1 OFFSET 5` on five rows yields rows 3..5),
    /// and Django emits `LIMIT -1` for every open-ended slice `qs[5:]`.
    /// `Ok(None)` is that no-bound answer; the caller decides what absence
    /// means for its clause. A negative value is remembered in
    /// `neg_limit_in_core` so `compound_chain` can still reject a `LIMIT`
    /// before a set operator, which absence alone no longer shows.
    fn limit_int(&mut self, what: &str) -> Result<Option<u64>> {
        let neg = self.eat(&Tok::Minus);
        match self.peek() {
            Some(&Tok::Int(v)) if v >= 0 => {
                self.pos += 1;
                if neg {
                    self.neg_limit_in_core = true;
                    // `-0` is zero, not "no bound".
                    return Ok((v == 0).then_some(0));
                }
                Ok(Some(v as u64))
            }
            _ => Err(self.err_here(format!("{what} requires an integer literal"))),
        }
    }
}
