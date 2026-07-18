//! DDL and RLS-policy statement parsing: `CREATE`/`DROP`/`ALTER TABLE`,
//! `CREATE INDEX`, `CREATE`/`DROP VIEW`, and row-level-security policies.
//!
//! Split out of the recursive-descent parser in [`super`] to keep that file
//! under the size limit. The shared [`Parser`] token helpers (`ident`,
//! `eat_word`, `expect_kw`, `advance`, …) stay in `super` and remain reachable
//! here because `parser::ddl` is a descendant module: private methods on
//! `Parser` are visible to descendants. This file holds only the DDL grammar.

use super::Parser;
use crate::ddl::{
    CreatePolicySpec, CreateTriggerSpec, DdlStmt, RlsAction, TriggerEvent, TriggerTiming,
};
use crate::token::{tokenize, Kw, Tok};
use mpedb_types::{PolicyCmd, Result};

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
            } else if p.eat_word("VIRTUAL") {
                p.expect_word("TABLE")?;
                p.parse_create_virtual_table()?
            } else if p.eat_word("UNIQUE") {
                p.expect_word("INDEX")?;
                p.parse_create_index(true)?
            } else if p.eat_word("INDEX") {
                p.parse_create_index(false)?
            } else if p.eat_word("VIEW") {
                p.parse_create_view()?
            } else if p.eat_word("TRIGGER") {
                p.parse_create_trigger()?
            } else {
                p.parse_create_policy()?
            }
        }
        Some("drop") => {
            p.advance();
            if p.eat_word("TABLE") {
                p.parse_drop_table()?
            } else if p.eat_word("VIEW") {
                p.parse_drop_view()?
            } else if p.eat_word("TRIGGER") {
                p.parse_drop_trigger()?
            } else {
                p.parse_drop_policy()?
            }
        }
        Some("alter") => {
            p.advance();
            p.parse_alter()?
        }
        Some("analyze") => {
            p.advance();
            p.parse_analyze()?
        }
        Some("reindex") => {
            p.advance();
            p.parse_reindex()?
        }
        _ => return Ok(None),
    };
    p.eat(&Tok::Semicolon);
    p.expect_eof()?;
    Ok(Some(ddl))
}

impl<'a> Parser<'a> {
    /// The current token as a lowercased identifier, if it is a bare Ident.
    fn peek_ident_ci(&self) -> Option<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => Some(s.to_ascii_lowercase()),
            _ => None,
        }
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
    /// `pub(super)` so the CTE `WITH` prefix in the parent module can reuse it.
    pub(super) fn capture_paren_source(&mut self) -> Result<String> {
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

    /// `CREATE VIRTUAL TABLE [IF NOT EXISTS] <name> USING fts5(<col>, …
    /// [, tokenize='unicode61'|'ascii'])` (design/DESIGN-FTS.md §1). Only the
    /// `fts5` module is accepted (fts3/fts4/rtree and custom C modules refuse by
    /// name — mpedb has no extension ABI). Columns are bare identifiers; the one
    /// supported option is `tokenize=`. Semantics (rowid PK, tree seeding) live
    /// in the facade/engine, exactly like `CREATE TABLE`.
    fn parse_create_virtual_table(&mut self) -> Result<DdlStmt> {
        let if_not_exists = if self.eat_word("IF") {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("virtual table name")?;
        self.expect_word("USING")?;
        let module = self.ident("virtual-table module")?;
        if !module.eq_ignore_ascii_case("fts5") {
            return Err(self.err_here(format!(
                "only `fts5` virtual tables are supported (got `{module}`); fts3/fts4/rtree \
                 and custom modules are a deliberate non-goal (mpedb has no extension ABI)"
            )));
        }
        self.expect(&Tok::LParen, "(")?;
        let mut columns: Vec<String> = Vec::new();
        let mut tokenizer = mpedb_types::Tokenizer::Unicode61;
        loop {
            // An option `name = value` vs. a bare column name: look ahead for
            // `<ident> =`.
            let is_option = matches!(self.peek(), Some(Tok::Ident(_)))
                && self.peek_at(1) == Some(&Tok::Eq);
            if is_option {
                let optname = self.ident("option name")?.to_ascii_lowercase();
                self.expect(&Tok::Eq, "=")?;
                let val = match self.advance() {
                    Some(Tok::Str(s)) | Some(Tok::Ident(s)) | Some(Tok::QuotedIdent(s)) => s,
                    _ => {
                        return Err(
                            self.err_here("expected a tokenizer name, e.g. 'unicode61' or 'ascii'")
                        )
                    }
                };
                if optname != "tokenize" {
                    return Err(self.err_here(format!(
                        "fts5 option `{optname}=` is not supported yet (stage 1 supports only \
                         `tokenize=`; content/prefix/detail/columnsize are stage 3)"
                    )));
                }
                // Accept only the bare tokenizer name — sqlite allows trailing
                // args (`'unicode61 remove_diacritics 2'`), which are stage 3.
                let mut parts = val.split_whitespace();
                let base = parts.next().unwrap_or("");
                if parts.next().is_some() {
                    return Err(self.err_here(
                        "tokenizer arguments beyond the name (remove_diacritics, separators, \
                         a wrapped tokenizer) are not supported yet (stage 3)",
                    ));
                }
                match mpedb_types::Tokenizer::parse(base) {
                    Some(t) => tokenizer = t,
                    None => {
                        return Err(self.err_here(format!(
                            "unsupported tokenizer `{base}` (stage 1: unicode61, ascii; \
                             porter/trigram are stage 3)"
                        )))
                    }
                }
            } else {
                let col = self.ident("column name")?;
                // `col UNINDEXED` and other per-column options are stage 3: a
                // trailing word that is not a comma/paren is refused.
                if matches!(self.peek(), Some(Tok::Ident(_))) {
                    let w = self.ident("").unwrap_or_default();
                    return Err(self.err_here(format!(
                        "fts5 column option `{w}` (e.g. UNINDEXED) is not supported yet"
                    )));
                }
                columns.push(col);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, ")")?;
        if columns.is_empty() {
            return Err(self.err_here("an fts5 table needs at least one column"));
        }
        Ok(DdlStmt::CreateVirtualTable(crate::ddl::CreateVirtualTableSpec {
            name,
            columns,
            tokenizer,
            if_not_exists,
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

    fn parse_create_view(&mut self) -> Result<DdlStmt> {
        let if_not_exists = if self.eat_word("IF") {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("view name")?;
        // `CREATE VIEW v(a,b) AS …` (explicit column names) is not supported yet.
        if self.peek() == Some(&Tok::LParen) {
            return Err(self.err_here("CREATE VIEW with an explicit column list is not supported"));
        }
        self.expect_kw(Kw::As, "AS")?;
        // Capture the SELECT as source text (re-parsed + flattened at reference
        // time, like an RLS predicate). Everything from here to the end is the
        // view body; consume the tokens so `expect_eof` is satisfied.
        let start = self.here();
        let select_sql = self.src[start..].trim().trim_end_matches(';').trim().to_string();
        if select_sql.is_empty() {
            return Err(self.err_here("CREATE VIEW: empty SELECT body"));
        }
        while self.peek().is_some() {
            self.advance();
        }
        Ok(DdlStmt::CreateView { name, select_sql, if_not_exists })
    }

    fn parse_drop_view(&mut self) -> Result<DdlStmt> {
        let if_exists = if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("view name")?;
        Ok(DdlStmt::DropView { name, if_exists })
    }

    /// `CREATE TRIGGER [IF NOT EXISTS] <name> {BEFORE|AFTER}
    ///    {INSERT|UPDATE [OF cols]|DELETE} ON <table> [FOR EACH ROW]
    ///    [WHEN (<cond>)] BEGIN <stmt>; END` (DESIGN-TRIGGERS §2). The `WHEN`
    /// predicate and the `BEGIN … END` body are captured as source text and
    /// re-compiled by the facade at apply/load time — exactly like a view's
    /// SELECT and a policy predicate. `INSTEAD OF`, `FOR EACH STATEMENT` and
    /// `EXECUTE PROCEDURE` are named refusals (later stages / PySpell).
    fn parse_create_trigger(&mut self) -> Result<DdlStmt> {
        let if_not_exists = if self.eat_word("IF") {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("trigger name")?;
        let timing = if self.eat_word("BEFORE") {
            TriggerTiming::Before
        } else if self.eat_word("AFTER") {
            TriggerTiming::After
        } else if self.eat_word("INSTEAD") {
            let _ = self.eat_word("OF");
            return Err(self.err_here(
                "INSTEAD OF triggers are not supported (they need updatable views)",
            ));
        } else {
            return Err(self.err_here("expected BEFORE, AFTER, or INSTEAD OF"));
        };
        let event = if self.eat_kw(Kw::Insert) {
            TriggerEvent::Insert
        } else if self.eat_kw(Kw::Delete) {
            TriggerEvent::Delete
        } else if self.eat_kw(Kw::Update) {
            let of = if self.eat_word("OF") {
                let mut cols = vec![self.ident("column name")?];
                while self.eat(&Tok::Comma) {
                    cols.push(self.ident("column name")?);
                }
                cols
            } else {
                Vec::new()
            };
            TriggerEvent::Update { of }
        } else {
            return Err(self.err_here("expected INSERT, UPDATE, or DELETE"));
        };
        self.expect_kw(Kw::On, "ON")?;
        let table = self.ident("table name")?;
        // FOR EACH ROW is the only granularity (accepted, and assumed if
        // omitted). FOR EACH STATEMENT is a named refusal (Postgres-only).
        if self.eat_word("FOR") {
            self.expect_word("EACH")?;
            if self.eat_word("ROW") {
                // the only supported granularity
            } else if self.eat_word("STATEMENT") {
                return Err(self.err_here(
                    "FOR EACH STATEMENT triggers are not supported (mpedb has no set-level trigger)",
                ));
            } else {
                return Err(self.err_here("expected ROW or STATEMENT after FOR EACH"));
            }
        }
        let when_src = if self.eat_kw(Kw::When) {
            Some(self.capture_paren_source()?)
        } else {
            None
        };
        // Body: `BEGIN <stmt>; … END` (SQL). `EXECUTE PROCEDURE` (PySpell) is a
        // named refusal until DESIGN-TRIGGERS stage 5.
        let body_sql = if self.eat_word("EXECUTE") {
            let _ = self.eat_word("PROCEDURE");
            return Err(self.err_here(
                "EXECUTE PROCEDURE (PySpell) trigger bodies are not supported yet",
            ));
        } else if self.eat_kw(Kw::Begin) {
            self.capture_begin_end_source()?
        } else {
            return Err(
                self.err_here("expected BEGIN … END (or EXECUTE PROCEDURE) for the trigger body")
            );
        };
        Ok(DdlStmt::CreateTrigger(CreateTriggerSpec {
            name,
            timing,
            event,
            table,
            when_src,
            body_sql,
            if_not_exists,
        }))
    }

    /// Capture the SOURCE between a trigger's `BEGIN` (already consumed) and its
    /// matching `END`, balancing nested `CASE … END` (and any nested block) so a
    /// `CASE` inside the body does not terminate the capture early.
    fn capture_begin_end_source(&mut self) -> Result<String> {
        let start = self.here();
        let mut depth = 1usize;
        let end_pos = loop {
            let here = self.here();
            match self.advance() {
                Some(Tok::Kw(Kw::Case)) | Some(Tok::Kw(Kw::Begin)) => depth += 1,
                Some(Tok::Kw(Kw::End)) => {
                    depth -= 1;
                    if depth == 0 {
                        break here;
                    }
                }
                Some(_) => {}
                None => return Err(self.err_here("unterminated trigger body: expected END")),
            }
        };
        let src = self
            .src
            .get(start..end_pos)
            .unwrap_or("")
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if src.is_empty() {
            return Err(self.err_here("trigger body must contain a statement"));
        }
        Ok(src)
    }

    fn parse_drop_trigger(&mut self) -> Result<DdlStmt> {
        let if_exists = if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("trigger name")?;
        Ok(DdlStmt::DropTrigger { name, if_exists })
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<DdlStmt> {
        let if_not_exists = if self.eat_word("IF") {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("index name")?;
        self.expect_kw(Kw::On, "ON")?;
        let table = self.ident("table name")?;
        self.expect(&Tok::LParen, "(")?;
        let mut columns = vec![self.ident("column name")?];
        // Per-column ASC/DESC is accepted and ignored (indexes are ascending).
        let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
        while self.eat(&Tok::Comma) {
            columns.push(self.ident("column name")?);
            let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
        }
        self.expect(&Tok::RParen, ")")?;
        Ok(DdlStmt::CreateIndex { name, table, columns, unique, if_not_exists })
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

    /// `ANALYZE [<name>]` — an accepted no-op (mpedb's planner is rule-based and
    /// keeps no statistics). The optional target (a table/index/schema name) is
    /// consumed and ignored; it is not required to exist.
    fn parse_analyze(&mut self) -> Result<DdlStmt> {
        let name = self.opt_target_name()?;
        Ok(DdlStmt::Analyze { name })
    }

    /// `REINDEX [<name>]` — an accepted no-op (mpedb maintains indexes eagerly).
    /// The optional target (table or index name — indistinguishable here) is
    /// consumed and ignored.
    fn parse_reindex(&mut self) -> Result<DdlStmt> {
        let target = self.opt_target_name()?;
        Ok(DdlStmt::Reindex { target })
    }

    /// Consume an optional single identifier target (bare or quoted), returning
    /// `None` at end of statement / a trailing `;`. Shared by ANALYZE/REINDEX.
    fn opt_target_name(&mut self) -> Result<Option<String>> {
        if matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_))) {
            Ok(Some(self.ident("table or index name")?))
        } else {
            Ok(None)
        }
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
        if self.eat_word("ADD") {
            self.eat_word("COLUMN"); // optional, as in sqlite/PG
            let cname = self.ident("column name")?;
            let tyword = self.ident("column type")?;
            let Some(ty) = mpedb_types::ColumnType::parse(&tyword.to_ascii_lowercase()) else {
                return Err(self.err_here(format!(
                    "unknown column type `{tyword}` (int64/text/real/bool/blob/timestamp/any)"
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
                } else if self.eat_word("DEFAULT")
                    || self.eat_word("CHECK")
                    || self.eat_word("REFERENCES")
                {
                    return Err(self.err_here(
                        "DEFAULT/CHECK/REFERENCES are not supported in ADD COLUMN yet",
                    ));
                } else {
                    break;
                }
            }
            return Ok(DdlStmt::AlterAddColumn { table, column: col });
        }
        if self.eat_word("DROP") {
            self.eat_word("COLUMN"); // optional, as in sqlite/PG
            let column = self.ident("column name")?;
            return Ok(DdlStmt::AlterDropColumn { table, column });
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
}
