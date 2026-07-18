//! DDL and RLS-policy statement parsing: `CREATE`/`DROP`/`ALTER TABLE`,
//! `CREATE INDEX`, `CREATE`/`DROP VIEW`, and row-level-security policies.
//!
//! Split out of the recursive-descent parser in [`super`] to keep that file
//! under the size limit. The shared [`Parser`] token helpers (`ident`,
//! `eat_word`, `expect_kw`, `advance`, …) stay in `super` and remain reachable
//! here because `parser::ddl` is a descendant module: private methods on
//! `Parser` are visible to descendants. This file holds only the DDL grammar.

use super::Parser;
use crate::ddl::{CreatePolicySpec, DdlStmt, RlsAction};
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
            } else if p.eat_word("UNIQUE") {
                p.expect_word("INDEX")?;
                p.parse_create_index(true)?
            } else if p.eat_word("INDEX") {
                p.parse_create_index(false)?
            } else if p.eat_word("VIEW") {
                p.parse_create_view()?
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
