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
use mpedb_types::{Collation, DefaultExpr, PolicyCmd, Result, Value};

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

/// Why `AUTOINCREMENT` refuses by name instead of being accepted and quietly
/// downgraded.
///
/// `INTEGER PRIMARY KEY` is ALREADY a rowid alias here (#94/#85): a NULL or
/// omitted id is auto-assigned `max(rowid) + 1`. That is sqlite's behaviour
/// *without* the keyword, and mpedb matches it exactly — including the id reuse
/// after the top row is deleted (pinned differentially in
/// `crates/mpedb/tests/django_parse_gaps.rs`).
///
/// `AUTOINCREMENT` adds exactly ONE guarantee on top: a rowid is never REUSED,
/// even after the row holding it is deleted. sqlite honours it with a persisted
/// per-table high-water counter (the `sqlite_sequence` table), bumped inside the
/// same transaction as the insert. mpedb keeps no such counter — `next_rowid`
/// reads the current maximum out of the PK tree — so the guarantee cannot be
/// made without a new persisted, crash-safe, multi-process-visible sequence in
/// the catalog.
///
/// Accepting the keyword and not honouring it is the one outcome worse than
/// either alternative. A caller writes `AUTOINCREMENT` *because* ids must never
/// come back (an external reference, an audit trail, a resumable cursor);
/// handing them a reused id is wrong data, not a missing feature. So it refuses,
/// says what it cannot promise, and says what to use instead.
const AUTOINCREMENT_REFUSAL: &str =
    "AUTOINCREMENT is not supported — mpedb keeps no persisted rowid high-water \
     counter, so it cannot promise that an id is never reused after a delete, and \
     never reusing an id is the whole of what AUTOINCREMENT adds. A plain `INTEGER \
     PRIMARY KEY` already auto-assigns a NULL or omitted id as max(rowid)+1 (ids \
     ARE reused after deleting the top row, exactly as in sqlite without the \
     keyword); drop the keyword to use it";

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
    /// `COLLATE <name>` in a column definition → a built-in collating sequence
    /// (BINARY/NOCASE/RTRIM, case-insensitive). An unknown name is a clean parse
    /// error, matching sqlite's "no such collation sequence".
    fn parse_collation_name(&mut self) -> Result<Collation> {
        let name = self.ident("collation name after COLLATE")?;
        Collation::parse(&name)
            .ok_or_else(|| self.err_here(format!("no such collation sequence: {name}")))
    }

    /// The words that can START a column constraint and therefore can NOT be
    /// part of a declared type name. sqlite makes these real keyword tokens, so
    /// its `typetoken ::= ids*` rule stops at them for free; mpedb lexes them as
    /// ordinary identifiers (so a column may be called `check`), which means the
    /// stop set has to be written down. `NOT`/`NULL` are absent on purpose —
    /// they are [`Tok::Kw`], which `declared_type` stops at anyway.
    const COLUMN_CONSTRAINT_WORDS: &'static [&'static str] = &[
        "constraint",
        "primary",
        "unique",
        "check",
        "default",
        "collate",
        "references",
        "generated",
        "autoincrement",
        "deferrable",
    ];

    /// A declared SQL type name, in sqlite's liberal `typetoken` grammar: zero
    /// or more identifier words (`bigint`, `double precision`, `integer
    /// unsigned`, `unsigned big int`) optionally followed by a parenthesized
    /// size (`varchar(100)`, `decimal(10, 2)`).
    ///
    /// ANY name is accepted, because in sqlite a declared type is not a
    /// vocabulary but an input to the affinity rule — an unrecognized name is
    /// legal and means NUMERIC. The size is consumed and DROPPED: it never
    /// changes the affinity, and mpedb has no width-limited types, so honouring
    /// `varchar(100)` as a length limit would reject rows sqlite stores.
    ///
    /// Zero words is the legal TYPELESS column (`CREATE TABLE t(a)`,
    /// `a PRIMARY KEY`) → [`ColumnType::Any`], sqlite's no-affinity column.
    ///
    /// Resolution goes through [`mpedb_types::ColumnType::declared`], which is
    /// the same [`mpedb_types::Affinity::from_type_name`] rule `CAST` uses: one
    /// vocabulary and one mapping whether the name is written in a `CAST` or in
    /// a `CREATE TABLE`. It returns the AFFINITY alongside the storage type,
    /// because those two are what the declared name actually says and the
    /// storage type alone cannot distinguish `decimal(10,2)` (NUMERIC affinity —
    /// converts `'1.50'` to `1.5` on store) from no type at all (BLOB affinity —
    /// stores it verbatim). Both are `Any` columns; sqlite treats them
    /// oppositely.
    ///
    /// The third element is the declared text **verbatim**, sliced out of the
    /// source between the first type token and whatever follows it. `ty` and
    /// `affinity` are both lossy about the name (`float` → `Float64` whose
    /// canonical spelling is `REAL`; every unknown name → `(Any, Numeric)`),
    /// and `sqlite3_column_decltype` is defined as the text — a consumer that
    /// keys converters off it (CPython's `PARSE_DECLTYPES`) gets a different
    /// VALUE, with no error, when the canonical name is reported instead.
    fn declared_type(
        &mut self,
    ) -> Result<(mpedb_types::ColumnType, mpedb_types::Affinity, Option<String>)> {
        let start = self.toks.get(self.pos).map(|t| t.pos).unwrap_or(0);
        let mut words: Vec<String> = Vec::new();
        loop {
            match self.peek() {
                // A bare word is a type word unless it opens a constraint.
                Some(Tok::Ident(w)) => {
                    let lw = w.to_ascii_lowercase();
                    if Self::COLUMN_CONSTRAINT_WORDS.contains(&lw.as_str()) {
                        break;
                    }
                    words.push(lw);
                    self.pos += 1;
                }
                // A QUOTED word can never be a constraint keyword, so it is
                // always part of the type name (sqlite's `ids ::= ID|STRING`).
                Some(Tok::QuotedIdent(_)) => {
                    words.push(self.ident("a type name")?.to_ascii_lowercase())
                }
                _ => break,
            }
        }
        if words.is_empty() {
            // The typeless column: sqlite's BLOB (historically NONE) affinity,
            // which converts nothing. No text ⇒ no decltype (sqlite's NULL).
            return Ok((
                mpedb_types::ColumnType::Any,
                mpedb_types::Affinity::Blob,
                None,
            ));
        }
        // Optional `( n )` / `( n , m )` size — consumed and discarded.
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
            self.expect(&Tok::RParen, "`)` after a column type size")?;
        }
        // The verbatim span: from the first type token to the start of whatever
        // token follows (end of source if none), trimmed of the trailing gap.
        // Slicing the SOURCE rather than re-rendering the tokens is what keeps
        // case, spacing and the size suffix exactly as written, which is what
        // `sqlite3_column_decltype` promises.
        let end = self
            .toks
            .get(self.pos)
            .map(|t| t.pos)
            .unwrap_or(self.src.len());
        let text = self.src.get(start..end).unwrap_or("").trim();
        let (ty, aff) = mpedb_types::ColumnType::declared(&words.join(" "));
        let decl = (!text.is_empty()).then(|| text.to_string());
        Ok((ty, aff, decl))
    }

    /// The tail of a `REFERENCES <table> [(col, …)] [ON …|MATCH …|[NOT]
    /// DEFERRABLE …]*` clause — consumed and DISCARDED.
    ///
    /// This is not a shrug. sqlite's default is `PRAGMA foreign_keys = OFF`,
    /// under which sqlite ITSELF parses a foreign key and enforces nothing:
    /// the dangling child row goes in, the `ON DELETE CASCADE` never fires.
    /// mpedb has no `foreign_keys = ON` to switch to, so parse-and-drop is
    /// sqlite's default behaviour exactly — and mpedb must never claim to
    /// enforce an FK. Pinned differentially in
    /// `crates/mpedb/tests/django_parse_gaps.rs`.
    fn skip_references_clause(&mut self) -> Result<()> {
        let _table = self.ident("table name after REFERENCES")?;
        if self.peek() == Some(&Tok::LParen) {
            let _cols = self.paren_ident_list()?;
        }
        // `ON DELETE|UPDATE <action>`, `MATCH <name>`, `[NOT] DEFERRABLE
        // [INITIALLY DEFERRED|IMMEDIATE]`. Every one of them is a rule about
        // enforcement, and there is no enforcement, so every one is dropped.
        // `SET` and `MATCH` are real keywords in this tokenizer, the action
        // words are not.
        loop {
            if self.eat_kw(Kw::On) {
                if !(self.eat_kw(Kw::Delete) || self.eat_kw(Kw::Update)) {
                    return Err(self.err_here("expected DELETE or UPDATE after REFERENCES … ON"));
                }
                if self.eat_kw(Kw::Set) {
                    if !(self.eat_kw(Kw::Null) || self.eat_word("DEFAULT")) {
                        return Err(self.err_here("expected NULL or DEFAULT after ON … SET"));
                    }
                } else if self.eat_word("CASCADE") || self.eat_word("RESTRICT") {
                    // nothing more
                } else if self.eat_word("NO") {
                    self.expect_word("ACTION")?;
                } else {
                    return Err(self.err_here(
                        "expected SET NULL, SET DEFAULT, CASCADE, RESTRICT or NO ACTION",
                    ));
                }
            } else if self.eat_kw(Kw::Match) {
                let _ = self.ident("a name after MATCH")?;
            } else if self.at_deferrable() {
                let _ = self.eat_kw(Kw::Not);
                self.expect_word("DEFERRABLE")?;
                if self.eat_word("INITIALLY")
                    && !(self.eat_word("DEFERRED") || self.eat_word("IMMEDIATE"))
                {
                    return Err(self.err_here("expected DEFERRED or IMMEDIATE after INITIALLY"));
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    /// At `DEFERRABLE` or `NOT DEFERRABLE`. The two-token lookahead is what
    /// keeps `NOT NULL` — which follows a `REFERENCES` clause perfectly
    /// legally — from being eaten as the start of a deferrability clause.
    fn at_deferrable(&self) -> bool {
        let deferrable = |t: Option<&Tok>| {
            matches!(t, Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("DEFERRABLE"))
        };
        deferrable(self.peek())
            || (matches!(self.peek(), Some(Tok::Kw(Kw::Not))) && deferrable(self.peek_at(1)))
    }

    /// `DEFAULT <value>` in a column definition.
    ///
    /// A literal constant only, which is `ALTER TABLE ADD COLUMN`'s existing
    /// rule reused verbatim. sqlite is LOOSER here: it also takes a
    /// parenthesized expression (`DEFAULT (datetime('now'))`) and the bare
    /// `CURRENT_DATE`/`CURRENT_TIME`/`CURRENT_TIMESTAMP` words. mpedb's stored
    /// [`DefaultExpr`] can hold neither — it is a `Const` or the engine's
    /// commit-time `Now`, and `Now` is a microsecond `Timestamp` where sqlite's
    /// `CURRENT_TIMESTAMP` is the TEXT `'YYYY-MM-DD HH:MM:SS'`. Accepting the
    /// keyword would store a DIFFERENT value than sqlite stores, so both refuse
    /// by name: a clean refusal beats a value that is quietly not sqlite's.
    fn parse_column_default(&mut self) -> Result<DefaultExpr> {
        if let Some(Tok::Ident(w)) = self.peek() {
            let lw = w.to_ascii_lowercase();
            if matches!(lw.as_str(), "current_date" | "current_time" | "current_timestamp") {
                return Err(self.err_here(format!(
                    "DEFAULT {} is not supported — mpedb has no TEXT datetime default, and \
                     sqlite's is the string `YYYY-MM-DD HH:MM:SS`, so accepting the keyword \
                     would store a different value than sqlite stores; use a constant, or \
                     supply the value on INSERT",
                    lw.to_ascii_uppercase()
                )));
            }
        }
        if self.peek() == Some(&Tok::LParen) {
            return Err(self.err_here(
                "a parenthesized DEFAULT expression is not supported — mpedb stores a \
                 constant default, not an expression evaluated per row",
            ));
        }
        self.parse_add_column_default()
    }

    /// One `<name> [type] [constraint…]` column definition inside CREATE TABLE.
    fn parse_column_def(&mut self) -> Result<crate::ddl::CreateColumnSpec> {
        let cname = self.ident("column name")?;
        // sqlite's full declared-type grammar (`varchar(100)`,
        // `double precision`, an unknown name, or none at all).
        let (ty, affinity, decl) = self.declared_type()?;
        let mut col = crate::ddl::CreateColumnSpec {
            name: cname,
            ty,
            affinity,
            decl,
            not_null: false,
            unique: false,
            pk: false,
            default: None,
            check: None,
            collation: Collation::Binary,
        };
        loop {
            // A per-column constraint may carry a `CONSTRAINT <name>` prefix.
            // The name is dropped — see `parse_create_table`.
            let named = if self.eat_word("CONSTRAINT") {
                Some(self.ident("constraint name after CONSTRAINT")?)
            } else {
                None
            };
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
                // sqlite's `PRIMARY KEY [ASC|DESC] [AUTOINCREMENT]`. A
                // one-column index has no key order to choose, so the
                // direction is accepted and dropped, exactly as sqlite
                // does with it.
                let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
                col.pk = true;
                if self.eat_word("AUTOINCREMENT") {
                    return Err(self.err_here(AUTOINCREMENT_REFUSAL));
                }
            } else if self.eat_word("AUTOINCREMENT") {
                return Err(self.err_here(AUTOINCREMENT_REFUSAL));
            } else if self.eat_word("COLLATE") {
                col.collation = self.parse_collation_name()?;
            } else if self.eat_word("DEFAULT") {
                if col.default.is_some() {
                    return Err(
                        self.err_here(format!("column `{}` has more than one DEFAULT", col.name))
                    );
                }
                col.default = Some(self.parse_column_default()?);
            } else if self.eat_word("CHECK") {
                let src = self.capture_paren_source()?;
                // Several CHECKs on one column are one conjunction — which is
                // exactly what sqlite means by them too (every CHECK must pass).
                col.check = Some(match col.check.take() {
                    Some(prev) => format!("({prev}) AND ({src})"),
                    None => src,
                });
            } else if self.eat_word("REFERENCES") {
                self.skip_references_clause()?;
            } else if let Some(n) = named {
                return Err(
                    self.err_here(format!("expected a column constraint after `CONSTRAINT {n}`"))
                );
            } else {
                break;
            }
        }
        Ok(col)
    }

    /// `CREATE TABLE name (<column-def | table-constraint>, …)`.
    ///
    /// Column definitions take sqlite's constraint set; the table-level
    /// constraints are `PRIMARY KEY (…)`, `UNIQUE (…)`, `CHECK (…)` and
    /// `FOREIGN KEY (…) REFERENCES …`, each optionally introduced by
    /// `CONSTRAINT <name>`. Semantics (id assignment, pk resolution, DEFAULT
    /// type-checking, CHECK compilation, validation) live in the facade/engine —
    /// this only builds the spec.
    ///
    /// **A constraint NAME is parsed and DROPPED.** sqlite keeps it only to
    /// quote back in an error message; mpedb's constraint errors already name
    /// the table and the column, and a name that is stored but never read would
    /// be a schema-hash input that buys nothing. Duplicate names are therefore
    /// not diagnosed either — nor are they by sqlite across tables.
    fn parse_create_table(&mut self) -> Result<DdlStmt> {
        let name = self.ident("table name")?;
        self.expect(&Tok::LParen, "(")?;
        let mut columns: Vec<crate::ddl::CreateColumnSpec> = Vec::new();
        let mut table_pk: Vec<String> = Vec::new();
        let mut uniques: Vec<Vec<String>> = Vec::new();
        let mut checks: Vec<String> = Vec::new();
        loop {
            // `CONSTRAINT <name>` introduces a NAMED table constraint; once it
            // is there a column definition can no longer follow.
            let named = if self.eat_word("CONSTRAINT") {
                Some(self.ident("constraint name after CONSTRAINT")?)
            } else {
                None
            };
            if self.eat_word("PRIMARY") {
                self.expect_word("KEY")?;
                if !table_pk.is_empty() {
                    return Err(self.err_here("duplicate table-level PRIMARY KEY"));
                }
                table_pk = self.paren_ident_list()?;
            } else if self.eat_word("UNIQUE") {
                uniques.push(self.paren_ident_list()?);
            } else if self.eat_word("CHECK") {
                checks.push(self.capture_paren_source()?);
            } else if self.eat_word("FOREIGN") {
                self.expect_word("KEY")?;
                let _cols = self.paren_ident_list()?;
                self.expect_word("REFERENCES")?;
                self.skip_references_clause()?;
            } else if let Some(n) = named {
                return Err(self.err_here(format!(
                    "expected PRIMARY KEY, UNIQUE, CHECK or FOREIGN KEY after `CONSTRAINT {n}`"
                )));
            } else {
                columns.push(self.parse_column_def()?);
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
            checks,
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
            // The SAME declared-type grammar CREATE TABLE uses — `varchar(100)`
            // must not mean one thing in a CREATE and another in an ADD. Zero
            // type words is the typeless column (`ALTER TABLE t ADD COLUMN c`)
            // → Any, matching sqlite's no-affinity column.
            let (ty, affinity, decl) = self.declared_type()?;
            let mut col = crate::ddl::CreateColumnSpec {
                name: cname,
                ty,
                affinity,
                decl,
                not_null: false,
                unique: false,
                pk: false,
                default: None,
                check: None,
                collation: Collation::Binary,
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
                    let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
                    col.pk = true;
                    if self.eat_word("AUTOINCREMENT") {
                        return Err(self.err_here(AUTOINCREMENT_REFUSAL));
                    }
                } else if self.eat_word("AUTOINCREMENT") {
                    return Err(self.err_here(AUTOINCREMENT_REFUSAL));
                } else if self.eat_word("COLLATE") {
                    col.collation = self.parse_collation_name()?;
                } else if self.eat_word("DEFAULT") {
                    // `ADD COLUMN … DEFAULT <const>` fills existing rows with the
                    // constant (and a `NOT NULL DEFAULT <const>` becomes legal —
                    // the fill value is non-NULL). Only a literal is accepted,
                    // matching sqlite, which refuses a non-constant ADD-COLUMN
                    // default. The facade type-checks the value against `ty`.
                    col.default = Some(self.parse_add_column_default()?);
                } else if self.eat_word("CHECK") {
                    // sqlite REFUSES a CHECK on ADD COLUMN ("Cannot add a
                    // CHECK constraint"), because existing rows were never
                    // tested against it. Refusing is agreeing with sqlite.
                    let _ = self.capture_paren_source();
                    return Err(self.err_here(
                        "ALTER TABLE ADD COLUMN cannot carry a CHECK — the rows already in                          the table were never tested against it (sqlite refuses this too);                          declare the CHECK in CREATE TABLE",
                    ));
                } else if self.eat_word("REFERENCES") {
                    // Parsed and dropped, exactly as in CREATE TABLE and
                    // exactly as sqlite does under `foreign_keys = OFF`.
                    self.skip_references_clause()?;
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

    /// Parse the `DEFAULT <const>` value of an `ALTER TABLE ADD COLUMN` clause.
    /// sqlite accepts ONLY a literal constant here — an integer, float, string,
    /// blob, boolean, `NULL`, or a signed number — and refuses anything that
    /// needs evaluation (a parenthesized expression such as `(1+2)`, a function
    /// call, a column reference, or `CURRENT_*`) with "Cannot add a column with
    /// non-constant default". We match that: a non-literal default is a parse
    /// error. The value is folded into a [`DefaultExpr::Const`]; the facade
    /// type-checks it against the column type.
    fn parse_add_column_default(&mut self) -> Result<DefaultExpr> {
        // A leading sign only makes sense before a numeric literal.
        let signed = if self.eat(&Tok::Minus) {
            Some(true)
        } else if self.eat(&Tok::Plus) {
            Some(false)
        } else {
            None
        };
        let non_const = |p: &Self| {
            p.err_here(
                "ADD COLUMN DEFAULT must be a constant literal (a number, string, blob, \
                 boolean, or NULL) — a parenthesized expression, function call, column \
                 reference, or CURRENT_* default is not supported (matches sqlite)",
            )
        };
        let val = match self.advance() {
            Some(Tok::Int(i)) => {
                let i = if signed == Some(true) {
                    i.checked_neg()
                        .ok_or_else(|| self.err_here("integer literal overflows i64"))?
                } else {
                    i
                };
                Value::Int(i)
            }
            Some(Tok::Float(f)) => Value::Float(if signed == Some(true) { -f } else { f }),
            // A sign before a non-numeric literal is a syntax error.
            Some(Tok::Str(s)) if signed.is_none() => Value::Text(s),
            Some(Tok::Blob(b)) if signed.is_none() => Value::Blob(b),
            Some(Tok::Kw(Kw::Null)) if signed.is_none() => Value::Null,
            Some(Tok::Kw(Kw::True)) if signed.is_none() => Value::Bool(true),
            Some(Tok::Kw(Kw::False)) if signed.is_none() => Value::Bool(false),
            _ => return Err(non_const(self)),
        };
        Ok(DefaultExpr::Const(val))
    }
}
