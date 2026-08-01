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
    CreatePolicySpec, CreateTriggerSpec, DdlStmt, RlsAction, TriggerBodySpec, TriggerEvent,
    TriggerTiming,
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
            } else if p.eat_word("INDEX") {
                p.parse_drop_index()?
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
/// `AUTOINCREMENT` anywhere but directly after `PRIMARY KEY` — sqlite's own
/// rule ("AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY").
const AUTOINCREMENT_REFUSAL_PLACE: &str =
    "AUTOINCREMENT is only allowed immediately after INTEGER PRIMARY KEY";

const AUTOINCREMENT_REFUSAL: &str =
    "AUTOINCREMENT is not supported — mpedb keeps no persisted rowid high-water \
     counter, so it cannot promise that an id is never reused after a delete, and \
     never reusing an id is the whole of what AUTOINCREMENT adds. A plain `INTEGER \
     PRIMARY KEY` already auto-assigns a NULL or omitted id as max(rowid)+1 (ids \
     ARE reused after deleting the top row, exactly as in sqlite without the \
     keyword); drop the keyword to use it";

impl<'a> Parser<'a> {
    /// sqlite's per-constraint conflict clause: `… ON CONFLICT {ROLLBACK |
    /// ABORT | FAIL | IGNORE | REPLACE}`, which sets the DEFAULT resolution
    /// used when a statement violates THIS constraint without its own
    /// `INSERT OR …` prefix.
    ///
    /// mpedb's default is ABORT (statements are atomic and a violation errors),
    /// so `ON CONFLICT ABORT` is accepted and dropped — it says exactly what
    /// already happens. Every other action would change the outcome of a
    /// conflicting statement (skip it, replace the row, keep a partial
    /// statement, abort the transaction) and mpedb's schema carries no
    /// per-constraint action to honor it with, so those refuse BY NAME rather
    /// than being parsed and silently ignored — a swallowed `ON CONFLICT
    /// IGNORE` turns "skipped" into "raised", which is a wrong answer.
    ///
    /// Returns without consuming anything when no `ON CONFLICT` follows.
    fn conflict_clause(&mut self, what: &str) -> Result<()> {
        // `ON`, `CONFLICT` and `ROLLBACK` are reserved keywords; the other four
        // action words are plain identifiers.
        if !(self.peek() == Some(&Tok::Kw(Kw::On))
            && self.peek_at(1) == Some(&Tok::Kw(Kw::Conflict)))
        {
            return Ok(());
        }
        self.advance();
        self.advance();
        if self.eat_word("ABORT") {
            return Ok(());
        }
        let action = match self.peek() {
            Some(Tok::Kw(Kw::Rollback)) => "ROLLBACK",
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("FAIL") => "FAIL",
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("IGNORE") => "IGNORE",
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("REPLACE") => "REPLACE",
            _ => {
                return Err(self.err_here(
                    "expected ROLLBACK, ABORT, FAIL, IGNORE or REPLACE after ON CONFLICT",
                ))
            }
        };
        Err(self.err_here(format!(
            "`ON CONFLICT {action}` on {what} is not supported: mpedb's schema carries no \
             per-constraint conflict action, and accepting it silently would resolve a \
             conflict differently from what was written. `ON CONFLICT ABORT` (mpedb's \
             behaviour) is accepted; otherwise write the action on the statement \
             (`INSERT OR {action} …`)"
        )))
    }

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
                Some(Tok::QuotedIdent(..)) => {
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
    /// DEFERRABLE …]*` clause, KEPT.
    ///
    /// It was consumed and discarded until 2026-07-29, and that was not a
    /// shrug: sqlite's own default is `PRAGMA foreign_keys = OFF`, under which
    /// sqlite too parses a foreign key and enforces nothing. Parse-and-drop was
    /// sqlite's default behaviour exactly; what it lacked was an `ON` to offer.
    /// `crates/mpedb/tests/django_parse_gaps.rs` pinned the old behaviour and
    /// moved with this change.
    ///
    /// `child` is the child-side column list, already parsed by the caller —
    /// one name for the column-level shorthand, the parenthesised list for the
    /// table-level `FOREIGN KEY (…)` form.
    fn references_clause(
        &mut self,
        child: Vec<String>,
        name: Option<String>,
    ) -> Result<crate::ddl::ForeignKeySpec> {
        use mpedb_types::FkAction;
        let parent = self.ident("table name after REFERENCES")?;
        let parent_columns = if self.peek() == Some(&Tok::LParen) {
            self.paren_ident_list()?
        } else {
            Vec::new()
        };
        let mut on_delete = FkAction::NoAction;
        let mut on_update = FkAction::NoAction;
        let mut deferred = false;
        // `ON DELETE|UPDATE <action>`, `MATCH <name>`, `[NOT] DEFERRABLE
        // [INITIALLY DEFERRED|IMMEDIATE]`. `SET` and `MATCH` are real keywords
        // in this tokenizer, the action words are not.
        loop {
            if self.eat_kw(Kw::On) {
                let is_delete = if self.eat_kw(Kw::Delete) {
                    true
                } else if self.eat_kw(Kw::Update) {
                    false
                } else {
                    return Err(self.err_here("expected DELETE or UPDATE after REFERENCES … ON"));
                };
                let action = if self.eat_kw(Kw::Set) {
                    if self.eat_kw(Kw::Null) {
                        FkAction::SetNull
                    } else if self.eat_word("DEFAULT") {
                        FkAction::SetDefault
                    } else {
                        return Err(self.err_here("expected NULL or DEFAULT after ON … SET"));
                    }
                } else if self.eat_word("CASCADE") {
                    FkAction::Cascade
                } else if self.eat_word("RESTRICT") {
                    FkAction::Restrict
                } else if self.eat_word("NO") {
                    self.expect_word("ACTION")?;
                    FkAction::NoAction
                } else {
                    return Err(self.err_here(
                        "expected SET NULL, SET DEFAULT, CASCADE, RESTRICT or NO ACTION",
                    ));
                };
                *(if is_delete {
                    &mut on_delete
                } else {
                    &mut on_update
                }) = action;
            } else if self.eat_kw(Kw::Match) {
                // MATCH SIMPLE is the only mode sqlite implements; MATCH FULL
                // and MATCH PARTIAL parse and behave as SIMPLE there. Following
                // sqlite means accepting the word and ignoring it — the modes
                // differ only for a PARTIALLY NULL composite key, and SIMPLE
                // (skip the check) is what both engines do.
                let _ = self.ident("a name after MATCH")?;
            } else if self.at_deferrable() {
                let not = self.eat_kw(Kw::Not);
                self.expect_word("DEFERRABLE")?;
                // Only `DEFERRABLE INITIALLY DEFERRED` defers. A bare
                // `DEFERRABLE`, `INITIALLY IMMEDIATE`, and `NOT DEFERRABLE`
                // are all immediate — sqlite's rule exactly.
                if self.eat_word("INITIALLY") {
                    if self.eat_word("DEFERRED") {
                        deferred = !not;
                    } else if !self.eat_word("IMMEDIATE") {
                        return Err(self.err_here("expected DEFERRED or IMMEDIATE after INITIALLY"));
                    }
                }
            } else {
                break;
            }
        }
        Ok(crate::ddl::ForeignKeySpec {
            columns: child,
            parent,
            parent_columns,
            on_delete,
            on_update,
            deferred,
            name,
        })
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

    /// One key part of a `CREATE INDEX` list: `(column name, expression source)`
    /// with exactly one of the two meaningful.
    ///
    /// Told apart by LOOKAHEAD rather than by trying and backtracking: a part is
    /// a plain column exactly when an identifier is followed by what ends a
    /// part — `,`, `)`, or `ASC`/`DESC`. `LOWER(a)`, `a || b` and a bare literal
    /// all take the expression branch.
    fn index_key_part(&mut self) -> Result<(String, Option<String>, Option<Collation>)> {
        let ends_part = matches!(
            self.peek_at(1),
            Some(Tok::Comma) | Some(Tok::RParen) | Some(Tok::Kw(Kw::Asc)) | Some(Tok::Kw(Kw::Desc))
        );
        // `QuotedIdent` as well as `Ident`: Django quotes every identifier, and
        // reading `("app_label", "model")` as two EXPRESSIONS gave both parts
        // the same sentinel ordinal — which then failed the duplicate-column
        // check and took down every migration that has a composite index.
        let named = matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(..)));
        if named && ends_part {
            return Ok((self.ident("column name")?, None, None));
        }
        // `a COLLATE NOCASE` is a COLUMN part with its comparison changed, not
        // an expression: MEASURED at sqlite 3.45.1 it reports as index_xinfo
        // column `a` with coll NOCASE, and a duplicate under it names the
        // COLUMN. So it keeps its ordinal and overrides only the key encoding.
        // COLLATE is a plain word here, not a keyword token (the column-
        // definition grammar eats it with `eat_word` too).
        let collate_next = matches!(self.peek_at(1), Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("COLLATE"));
        if named && collate_next {
            let col = self.ident("column name")?;
            let _ = self.eat_word("COLLATE");
            let coll = self.parse_collation_name()?;
            return Ok((col, None, Some(coll)));
        }
        let start = self.here();
        let before = self.max_params;
        let _e = self.expr()?;
        let end = self.here();
        let src = self.src.get(start..end).unwrap_or("").trim().to_string();
        if src.is_empty() {
            return Err(self.err_here("empty expression in a CREATE INDEX key"));
        }
        // A parameter would make the KEY differ between two writers of the same
        // index, which is not an index. Counted by the PARSER rather than
        // matched in the text: a `?` inside a string literal is not a
        // parameter, and a text scan cannot tell the two apart.
        if self.max_params != before {
            return Err(self.err_here(
                "a parameter is not allowed in an index expression",
            ));
        }
        Ok((String::new(), Some(src), None))
    }

    /// `DEFAULT <value>` in a column definition.
    ///
    /// A literal constant, a parenthesized CONSTANT expression, or one of
    /// sqlite's three time keywords.
    ///
    /// The keywords used to be refused, and the reason given was sound for the
    /// schema as it was: mpedb's stored [`DefaultExpr`] could hold only a
    /// `Const` or the engine's microsecond `Timestamp` `Now`, where sqlite's
    /// `CURRENT_TIMESTAMP` is the TEXT `'YYYY-MM-DD HH:MM:SS'` — so accepting
    /// the word would have stored a DIFFERENT value than sqlite stores. The
    /// schema now carries the three forms itself (canonical bytes v15), and the
    /// engine fills them from the same clock and the same renderer the
    /// EXPRESSION path uses, so the two spellings of `CURRENT_TIMESTAMP` cannot
    /// disagree.
    fn parse_column_default(&mut self) -> Result<ColumnDefault> {
        if let Some(Tok::Ident(w)) = self.peek() {
            let kw = match w.to_ascii_lowercase().as_str() {
                "current_date" => Some(DefaultExpr::CurrentDate),
                "current_time" => Some(DefaultExpr::CurrentTime),
                "current_timestamp" => Some(DefaultExpr::CurrentTimestamp),
                _ => None,
            };
            if let Some(kw) = kw {
                self.pos += 1;
                return Ok(ColumnDefault::Value(kw));
            }
        }
        if self.peek() == Some(&Tok::LParen) {
            // `DEFAULT ( <expr> )`. sqlite refuses a default that reads another
            // column — MEASURED: `b INT DEFAULT (a+1)` is "default value of
            // column [b] is not constant". So the expression is CLOSED: it has
            // no row to read, and its value is the same for every row forever.
            // That is a CONSTANT, just one written as arithmetic — which is how
            // Django's ORM emits `db_default=Pi()` and `db_default=Coalesce(4.5,
            // Pi())`. Captured as source here and folded to a literal where the
            // expression compiler lives; a source that does NOT fold is refused
            // there, by name.
            return Ok(ColumnDefault::Expr(self.capture_paren_source()?));
        }
        self.parse_add_column_default().map(ColumnDefault::Value)
    }

    /// The tail of a generated-column clause, positioned at the `AS`:
    /// `AS ( <expr> ) [STORED | VIRTUAL]`.
    ///
    /// The expression is captured as SOURCE (like `CHECK`) — the parser has no
    /// column list to bind against — and the storage word defaults to `VIRTUAL`
    /// when absent, which is sqlite's default. Only ONE of the two words may
    /// appear: `STORED VIRTUAL` is a syntax error in sqlite and here.
    fn parse_generated_tail(&mut self) -> Result<(String, mpedb_types::GeneratedKind)> {
        self.expect_kw(Kw::As, "AS")?;
        if self.peek() != Some(&Tok::LParen) {
            return Err(self.err_here(
                "a generated column's expression must be parenthesized: `AS ( <expr> )`",
            ));
        }
        let src = self.capture_paren_source()?;
        // VIRTUAL is sqlite's default when neither word is written, so the
        // explicit `VIRTUAL` and the absent one deliberately land in one arm.
        let kind = if self.eat_word("STORED") {
            mpedb_types::GeneratedKind::Stored
        } else {
            let _ = self.eat_word("VIRTUAL");
            mpedb_types::GeneratedKind::Virtual
        };
        Ok((src, kind))
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
            default_src: None,
            default_text: None, autoincrement: false,
            check: None,
            collation: Collation::Binary,
            generated: None,
            references: None,
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
                self.conflict_clause("a NOT NULL constraint")?;
            } else if self.eat_kw(Kw::Null) {
                col.not_null = false;
            } else if self.eat_word("UNIQUE") {
                col.unique = true;
                self.conflict_clause("a UNIQUE constraint")?;
            } else if self.eat_word("PRIMARY") {
                self.expect_word("KEY")?;
                // sqlite's `PRIMARY KEY [ASC|DESC] [AUTOINCREMENT]`. A
                // one-column index has no key order to choose, so the
                // direction is accepted and dropped, exactly as sqlite
                // does with it.
                let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
                col.pk = true;
                self.conflict_clause("a PRIMARY KEY constraint")?;
                if self.eat_word("AUTOINCREMENT") {
                    col.autoincrement = true;
                }
            } else if self.eat_word("AUTOINCREMENT") {
                // Only ever legal directly after `PRIMARY KEY`, as in sqlite.
                return Err(self.err_here(AUTOINCREMENT_REFUSAL_PLACE));
            } else if self.eat_word("COLLATE") {
                col.collation = self.parse_collation_name()?;
            } else if self.eat_word("DEFAULT") {
                if col.default.is_some() {
                    return Err(
                        self.err_here(format!("column `{}` has more than one DEFAULT", col.name))
                    );
                }
                // sqlite's `PRAGMA table_info` reports `dflt_value` as the
                // DDL TEXT of the default, not its value: `'x'` keeps its
                // quotes, `3+5` stays unfolded, `1` on a BOOLEAN column stays
                // `1`. Captured here because only the parser knows the span;
                // the stored VALUE cannot reproduce it (`DEFAULT 1` and
                // `DEFAULT true` fold to one value and print differently).
                let at = self.here();
                match self.parse_column_default()? {
                    ColumnDefault::Value(d) => {
                        col.default = Some(d);
                        col.default_text = self
                            .src
                            .get(at..self.here())
                            .map(|t| t.trim().to_string())
                            .filter(|t| !t.is_empty());
                    }
                    // `DEFAULT ( <expr> )` reports WITHOUT the wrapping parens
                    // — measured: sqlite says `3+5`, not `(3+5)` — and that is
                    // exactly the text `capture_paren_source` already returned.
                    ColumnDefault::Expr(src) => {
                        col.default_text = Some(src.clone());
                        col.default_src = Some(src);
                    }
                }
            } else if self.eat_word("CHECK") {
                let src = self.capture_paren_source()?;
                // Several CHECKs on one column are one conjunction — which is
                // exactly what sqlite means by them too (every CHECK must pass).
                col.check = Some(match col.check.take() {
                    Some(prev) => format!("({prev}) AND ({src})"),
                    None => src,
                });
            } else if self.eat_word("REFERENCES") {
                if col.references.is_some() {
                    return Err(self.err_here(format!(
                        "column `{}` carries more than one REFERENCES clause",
                        col.name
                    )));
                }
                col.references = Some(self.references_clause(vec![col.name.clone()], None)?);
            } else if self.eat_word("GENERATED") {
                // `GENERATED ALWAYS AS (…)`. The two words are one token pair in
                // sqlite's grammar — `GENERATED` alone is absorbed into the
                // declared TYPE there, which `declared_type`'s stop set makes
                // unreachable here, so a lone `GENERATED` is a clean error.
                self.expect_word("ALWAYS")?;
                if col.generated.is_some() {
                    return Err(self.err_here(format!(
                        "column `{}` is declared generated more than once",
                        col.name
                    )));
                }
                col.generated = Some(self.parse_generated_tail()?);
            } else if matches!(self.peek(), Some(Tok::Kw(Kw::As))) {
                // The short spelling: `<col> <type> AS (<expr>) [STORED|VIRTUAL]`.
                if col.generated.is_some() {
                    return Err(self.err_here(format!(
                        "column `{}` is declared generated more than once",
                        col.name
                    )));
                }
                col.generated = Some(self.parse_generated_tail()?);
            } else if let Some(n) = named {
                return Err(
                    self.err_here(format!("expected a column constraint after `CONSTRAINT {n}`"))
                );
            } else {
                break;
            }
        }
        // sqlite refuses both of these at CREATE TABLE, by name.
        if col.generated.is_some() {
            if col.default.is_some() {
                return Err(self.err_here(format!(
                    "cannot use DEFAULT on generated column `{}`",
                    col.name
                )));
            }
            if col.pk {
                return Err(self.err_here(format!(
                    "generated column `{}` cannot be part of the PRIMARY KEY",
                    col.name
                )));
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
        let if_not_exists = if self.eat_word("IF") {
            self.expect_kw(Kw::Not, "NOT")?;
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("table name")?;
        self.expect(&Tok::LParen, "(")?;
        let mut columns: Vec<crate::ddl::CreateColumnSpec> = Vec::new();
        let mut table_pk: Vec<String> = Vec::new();
        let mut uniques: Vec<Vec<String>> = Vec::new();
        let mut checks: Vec<String> = Vec::new();
        let mut foreign_keys: Vec<crate::ddl::ForeignKeySpec> = Vec::new();
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
                self.conflict_clause("a PRIMARY KEY constraint")?;
            } else if self.eat_word("UNIQUE") {
                uniques.push(self.paren_ident_list()?);
                self.conflict_clause("a UNIQUE constraint")?;
            } else if self.eat_word("CHECK") {
                checks.push(self.capture_paren_source()?);
            } else if self.eat_word("FOREIGN") {
                self.expect_word("KEY")?;
                let cols = self.paren_ident_list()?;
                self.expect_word("REFERENCES")?;
                foreign_keys.push(self.references_clause(cols, named)?);
            } else if let Some(n) = named {
                return Err(self.err_here(format!(
                    "expected PRIMARY KEY, UNIQUE, CHECK or FOREIGN KEY after `CONSTRAINT {n}`"
                )));
            } else {
                let mut col = self.parse_column_def()?;
                // The column-level shorthand IS a table constraint; it just
                // spells its one child column implicitly. Hoisting it here
                // keeps the resolver with a single list to walk.
                if let Some(fk) = col.references.take() {
                    foreign_keys.push(fk);
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
            if_not_exists,
            // The table is AUTOINCREMENT when its rowid-alias column is.
            autoincrement: {
                // sqlite's rule: only on an INTEGER PRIMARY KEY. Anywhere else
                // it is a syntax error there, and accepting it would promise a
                // never-reused id on a key mpedb does not assign.
                if let Some(c) = columns.iter().find(|c| c.autoincrement) {
                    if c.ty != crate::ColumnType::Int64 {
                        return Err(self.err_here(format!(
                            "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY; \
                             `{}` is {}",
                            c.name, c.ty
                        )));
                    }
                }
                columns.iter().any(|c| c.autoincrement)
            },
            columns,
            table_pk,
            uniques,
            checks,
            foreign_keys,
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
        let module = if module.eq_ignore_ascii_case("fts5") {
            mpedb_types::FtsModule::Fts5
        } else if module.eq_ignore_ascii_case("fts4") {
            // Same engine underneath (mpedb's inverted index); the module tag
            // makes the facade lay down sqlite's five shadow tables so the
            // CATALOG reads back as sqlite's would (plan §7).
            mpedb_types::FtsModule::Fts4
        } else {
            return Err(self.err_here(format!(
                "only `fts5` and `fts4` virtual tables are supported (got `{module}`); \
                 fts3/rtree and custom modules are a deliberate non-goal (mpedb has no \
                 extension ABI)"
            )));
        };
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
                    Some(Tok::Str(s)) | Some(Tok::Ident(s)) | Some(Tok::QuotedIdent(s, _)) => s,
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
            module,
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

    /// `DROP INDEX [IF EXISTS] <name>`. sqlite also accepts a
    /// `<schema>.<name>` form; a qualified name would need ATTACH-aware
    /// resolution and is left to parse as the bare identifier it is.
    fn parse_drop_index(&mut self) -> Result<DdlStmt> {
        let if_exists = if self.eat_word("IF") {
            self.expect_word("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident("index name")?;
        Ok(DdlStmt::DropIndex { name, if_exists })
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
    /// predicate and the body (`BEGIN … END` SQL, or `EXECUTE PROCEDURE
    /// p(args…)` — stage 5, PySpell) are captured as source text and
    /// re-compiled by the facade at apply/load time — exactly like a view's
    /// SELECT and a policy predicate. `INSTEAD OF` and `FOR EACH STATEMENT`
    /// are named refusals.
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
            // sqlite's documented default when the timing word is omitted
            // (`CREATE TRIGGER t UPDATE OF c ON tbl BEGIN … END`), and the
            // shape CPython's own dump round-trip writes.
            TriggerTiming::Before
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
        // Body: `BEGIN <stmt>; … END` (SQL) or `EXECUTE PROCEDURE p(args…)`
        // (PySpell, DESIGN-TRIGGERS stage 5).
        let body = if self.eat_word("EXECUTE") {
            self.expect_word("PROCEDURE")?;
            let proc_name = self.ident("procedure name")?;
            let arg_srcs = self.capture_call_arg_sources()?;
            TriggerBodySpec::Proc {
                name: proc_name,
                arg_srcs,
            }
        } else if self.eat_kw(Kw::Begin) {
            TriggerBodySpec::Sql(self.capture_begin_end_source()?)
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
            body,
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

    /// Capture the comma-separated argument SOURCES of an `EXECUTE PROCEDURE
    /// p(arg, …)` call: consume `( … )`, splitting at top-level commas only (a
    /// comma inside nested parens — a function call in an argument — does not
    /// split). Each fragment is compiled by the facade in the trigger's
    /// `NEW`/`OLD` scope at apply time. `p()` yields an empty list.
    fn capture_call_arg_sources(&mut self) -> Result<Vec<String>> {
        self.expect(&Tok::LParen, "`(` after the procedure name")?;
        let mut args = Vec::new();
        let mut start = self.here();
        let mut depth = 0usize;
        loop {
            let here = self.here();
            match self.advance() {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => {
                    if depth == 0 {
                        let frag = self.src.get(start..here).unwrap_or("").trim();
                        if !frag.is_empty() {
                            args.push(frag.to_string());
                        } else if !args.is_empty() {
                            return Err(self.err_here("empty procedure argument"));
                        }
                        return Ok(args);
                    }
                    depth -= 1;
                }
                Some(Tok::Comma) if depth == 0 => {
                    let frag = self.src.get(start..here).unwrap_or("").trim();
                    if frag.is_empty() {
                        return Err(self.err_here("empty procedure argument"));
                    }
                    args.push(frag.to_string());
                    start = self.here();
                }
                Some(_) => {}
                None => return Err(self.err_here("unterminated procedure argument list")),
            }
        }
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
        // A key part is a bare column name or an EXPRESSION over the table's
        // columns (`CREATE INDEX i ON t (LOWER(a), b)`). A bare name stays a
        // plain column part — every index the format had before v13, and the
        // only kind the planner will pick.
        let mut columns: Vec<String> = Vec::new();
        let mut exprs: Vec<Option<String>> = Vec::new();
        let mut collations: Vec<Option<Collation>> = Vec::new();
        loop {
            let (col, ex, coll) = self.index_key_part()?;
            columns.push(col);
            exprs.push(ex);
            collations.push(coll);
            // Per-column ASC/DESC is accepted and ignored (indexes ascend).
            let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, ")")?;
        // Nothing downstream should have to ask twice whether this is an
        // expression index: the vector is empty unless a part actually is one.
        if exprs.iter().all(Option::is_none) {
            exprs.clear();
        }
        if collations.iter().all(Option::is_none) {
            collations.clear();
        }
        // Optional partial-index predicate (P1). Capture the source text so the
        // schema can store it; the expression is validated by parsing it.
        let where_clause = if self.eat_kw(Kw::Where) {
            let start = self.here();
            let _pred = self.expr()?;
            // Refuse parameters in the predicate until P6 (Guarded access).
            // A bare `WHERE c = $1` cannot be proven at plan time.
            let end = self.here();
            let src = self
                .src
                .get(start..end)
                .unwrap_or("")
                .trim()
                .to_string();
            if src.is_empty() {
                return Err(self.err_here("empty WHERE clause on CREATE INDEX"));
            }
            // Parameter tokens in the predicate: refuse by name (P6).
            if src.contains('$') || src.contains('?') {
                return Err(self.err_here(
                    "a parameterized partial-index predicate is not supported yet \
                     (P6: AccessPath::Guarded); use a literal predicate",
                ));
            }
            Some(src)
        } else {
            None
        };
        Ok(DdlStmt::CreateIndex {
            exprs,
            collations,
            name,
            table,
            columns,
            unique,
            if_not_exists,
            where_clause,
        })
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
        if matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(..))) {
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
                default_src: None,
                default_text: None, autoincrement: false,
                check: None,
                collation: Collation::Binary,
                generated: None,
                references: None,
            };
            loop {
                if self.eat_kw(Kw::Not) {
                    self.expect_kw(Kw::Null, "NULL")?;
                    col.not_null = true;
                    self.conflict_clause("a NOT NULL constraint")?;
                } else if self.eat_kw(Kw::Null) {
                    col.not_null = false;
                } else if self.eat_word("UNIQUE") {
                    col.unique = true;
                    self.conflict_clause("a UNIQUE constraint")?;
                } else if self.eat_word("PRIMARY") {
                    self.expect_word("KEY")?;
                    let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
                    col.pk = true;
                    self.conflict_clause("a PRIMARY KEY constraint")?;
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
                    col.references = Some(self.references_clause(vec![col.name.clone()], None)?);
                } else if self.eat_word("GENERATED") {
                    self.expect_word("ALWAYS")?;
                    col.generated = Some(self.parse_generated_tail()?);
                } else if matches!(self.peek(), Some(Tok::Kw(Kw::As))) {
                    col.generated = Some(self.parse_generated_tail()?);
                } else {
                    break;
                }
            }
            if col.generated.is_some() {
                if col.default.is_some() {
                    return Err(self.err_here(format!(
                        "cannot use DEFAULT on generated column `{}`",
                        col.name
                    )));
                }
                if col.pk {
                    return Err(self.err_here(format!(
                        "generated column `{}` cannot be part of the PRIMARY KEY",
                        col.name
                    )));
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
        // sqlite accepts REDUNDANT parentheses around a literal here — `(5)`,
        // `(-5)`, `('hi')` all work — while refusing anything it would have to
        // evaluate: `(3+4)` is "Cannot add a column with non-constant default"
        // (MEASURED at 3.45.1). The rule is about the VALUE being a literal, not
        // about the punctuation, because ADD COLUMN has to fill the rows already
        // in the table without computing anything. So peel one layer and require
        // a literal inside; a computed one falls through to the refusal below.
        if self.peek() == Some(&Tok::LParen) {
            let save = self.pos;
            self.pos += 1;
            if let Ok(d) = self.parse_add_column_default() {
                if self.eat(&Tok::RParen) {
                    return Ok(d);
                }
            }
            self.pos = save;
        }
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

/// What a column's `DEFAULT` clause turned out to be.
///
/// A literal resolves in the parser. A parenthesized expression cannot: folding
/// it needs the expression compiler, which lives above this layer — so the
/// source travels, and `CreateColumnSpec::default_src` carries it exactly one
/// step further.
enum ColumnDefault {
    Value(DefaultExpr),
    Expr(String),
}
