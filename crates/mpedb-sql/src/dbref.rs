//! Database-qualified table references — sqlite `ATTACH` compatibility (#51).
//!
//! sqlite's multi-database model addresses tables as `dbname.table`, where
//! `main` is the primary file and further files are attached under
//! connection-local names. mpedb's binder/planner/plan-format are all
//! single-schema, so cross-file SQL is handled HERE, before the parser: a
//! token-level pass resolves every table reference against a [`DbScope`]
//! (main's names + the attach list) and rewrites the statement so the
//! downstream pipeline sees ordinary single-schema SQL — attached tables
//! appear under mangled `"db.table"` names (a legal mpedb table name; the
//! facade builds a merged schema registering them) with the bare table name
//! as the row alias, exactly the row-name sqlite exposes for an unaliased
//! `FROM other.u` (probe P5c: `u.x` resolves; P4b: `other.u.x` too).
//!
//! The semantics implemented are DERIVED FROM THE SQLITE BINARY, not assumed
//! (probes in `crates/mpedb/tests/attach_semantics.rs` pin them against the
//! bundled oracle):
//!
//! - Unqualified names resolve **main first, then the attach list in attach
//!   order** (probes P1/P2/P2b). A CTE name shadows everything (P23).
//! - Database names match **case-insensitively** (P14); table names keep
//!   mpedb's case-sensitive dialect.
//! - `main.t` is always valid and means the primary file (P3).
//! - An unknown qualifier is `no such table: nope.t` (P3b).
//! - A 3-part column name `db.table.col` is valid exactly when that table
//!   appears in FROM without an explicit alias (P4b/P5); the rewrite maps it
//!   to the row alias, so an aliased entry refuses downstream like sqlite.
//! - An explicit alias hides both `db.table.col` and `table.col` (P5/P5b).
//!
//! What this pass REFUSES BY NAME (v1 honest-refusal set; every one is a
//! clean error, never a differing answer):
//!
//! - Writes/DDL that reference an attached database anywhere (sqlite allows
//!   them; mpedb v1 is read-only cross-file).
//! - Two same-named tables from different databases in one FROM without
//!   explicit aliases (sqlite resolves per-reference and errors only on
//!   ambiguous columns; the rewrite cannot represent two row-scopes with one
//!   name, so it asks for aliases instead of risking a wrong answer).

use crate::token::{self, Kw, SpTok, Tok};
use mpedb_types::{Error, Result};
use std::collections::HashSet;

/// The name scope a statement resolves against: main's table/view names and
/// the attach list in attach order, each with its live table names.
#[derive(Debug, Default)]
pub struct DbScope {
    /// Bare names resolvable in the primary file (tables AND views).
    pub main: HashSet<String>,
    /// `(db name, its table names)` in attach order.
    pub attached: Vec<(String, HashSet<String>)>,
}

impl DbScope {
    fn find_db(&self, name: &str) -> Option<usize> {
        self.attached
            .iter()
            .position(|(a, _)| a.eq_ignore_ascii_case(name))
    }
}

/// The outcome of resolving a statement against a [`DbScope`].
#[derive(Debug, Clone, PartialEq)]
pub enum DbResolution {
    /// The statement references no attached table. The returned SQL has any
    /// `main.` qualifiers stripped and routes to the ordinary single-file
    /// path (its plan hash equals the unqualified spelling's, exactly like
    /// `Workspace` routing).
    MainOnly(String),
    /// At least one attached table: the rewritten SQL references mangled
    /// `"db.table"` names, and `tables` lists each `(db, table)` the caller
    /// must register in the merged schema (dedup'd, in first-use order).
    Cross {
        sql: String,
        tables: Vec<(String, String)>,
    },
}

/// A parsed `ATTACH` / `DETACH` statement (sqlite grammar:
/// `ATTACH [DATABASE] 'path' AS name`, `DETACH [DATABASE] name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachStmt {
    Attach { path: String, name: String },
    Detach { name: String },
}

/// Parse an `ATTACH`/`DETACH` statement, or `None` if `sql` is anything else.
/// sqlite accepts an arbitrary expression for the path (including bound
/// parameters, probe P18); v1 accepts a string literal only and refuses the
/// rest by name.
pub fn parse_attach(sql: &str) -> Result<Option<AttachStmt>> {
    let head_is = |word: &str| {
        sql.trim_start()
            .get(..word.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(word))
    };
    if !head_is("ATTACH") && !head_is("DETACH") {
        return Ok(None);
    }
    let toks = token::tokenize(sql)?;
    let mut toks = toks.as_slice();
    // Strip one trailing semicolon (the tokenizer keeps it).
    if let [rest @ .., last] = toks {
        if last.tok == Tok::Semicolon {
            toks = rest;
        }
    }
    let word = |t: Option<&SpTok>| -> Option<String> {
        match t.map(|t| &t.tok) {
            Some(Tok::Ident(s)) | Some(Tok::QuotedIdent(s)) => Some(s.clone()),
            Some(Tok::Str(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let is_kw_database = |t: Option<&SpTok>| {
        matches!(t.map(|t| &t.tok), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("DATABASE"))
    };
    let head = word(toks.first()).unwrap_or_default();
    if head.eq_ignore_ascii_case("ATTACH") {
        let mut i = 1;
        if is_kw_database(toks.get(i)) {
            i += 1;
        }
        let path = match toks.get(i).map(|t| &t.tok) {
            Some(Tok::Str(s)) => s.clone(),
            Some(Tok::Question) | Some(Tok::DollarParam(_)) => {
                return Err(Error::Unsupported(
                    "ATTACH with a bound parameter is not supported; \
                     use a string literal path"
                        .into(),
                ))
            }
            _ => {
                return Err(Error::Parse {
                    pos: toks.get(i).map_or(sql.len(), |t| t.pos),
                    msg: "ATTACH requires a quoted string literal file path".into(),
                })
            }
        };
        i += 1;
        if !matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Kw(Kw::As))) {
            return Err(Error::Parse {
                pos: toks.get(i).map_or(sql.len(), |t| t.pos),
                msg: "expected AS <name> after the ATTACH file path".into(),
            });
        }
        i += 1;
        let name = word(toks.get(i)).ok_or_else(|| Error::Parse {
            pos: toks.get(i).map_or(sql.len(), |t| t.pos),
            msg: "expected a database name after AS".into(),
        })?;
        if toks.len() != i + 1 {
            return Err(Error::Parse {
                pos: toks[i + 1].pos,
                msg: "unexpected trailing input after ATTACH … AS <name>".into(),
            });
        }
        Ok(Some(AttachStmt::Attach { path, name }))
    } else {
        let mut i = 1;
        if is_kw_database(toks.get(i)) {
            i += 1;
        }
        let name = word(toks.get(i)).ok_or_else(|| Error::Parse {
            pos: toks.get(i).map_or(sql.len(), |t| t.pos),
            msg: "expected a database name after DETACH".into(),
        })?;
        if toks.len() != i + 1 {
            return Err(Error::Parse {
                pos: toks[i + 1].pos,
                msg: "unexpected trailing input after DETACH <name>".into(),
            });
        }
        Ok(Some(AttachStmt::Detach { name }))
    }
}

/// The mangled single-schema name an attached table is registered under.
pub fn mangle(db: &str, table: &str) -> String {
    format!("{db}.{table}")
}

/// Always-quoted identifier emission — safe for any name (keywords, dots).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Clone, Copy, PartialEq)]
enum StmtClass {
    Read,
    Write,
    Ddl,
    /// No table references possible (BEGIN/COMMIT/PRAGMA/…): pass through.
    Opaque,
}

/// One `Ident`/`QuotedIdent` payload, or `None`.
fn ident_of(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Ident(s) | Tok::QuotedIdent(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Bare (unquoted) identifier matching `word` case-insensitively — for the
/// positional non-keyword words (LEFT/CROSS/UNION/…), which quoting demotes
/// to plain identifiers, exactly as the parser treats them.
fn is_word(tok: &Tok, word: &str) -> bool {
    matches!(tok, Tok::Ident(s) if s.eq_ignore_ascii_case(word))
}

fn is_any_word(tok: &Tok, words: &[&str]) -> bool {
    words.iter().any(|w| is_word(tok, w))
}

/// Words that terminate a FROM list at its own depth. `Kw`-tokens WHERE /
/// GROUP / HAVING / ORDER / LIMIT / OFFSET / ON / RETURNING are matched as
/// keywords; the compound operators are positional identifiers.
fn ends_from_list(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Kw(Kw::Where)
            | Tok::Kw(Kw::Group)
            | Tok::Kw(Kw::Having)
            | Tok::Kw(Kw::Order)
            | Tok::Kw(Kw::Limit)
            | Tok::Kw(Kw::Offset)
            | Tok::Kw(Kw::Returning)
            | Tok::Kw(Kw::Select)
            | Tok::Kw(Kw::Set)
    ) || is_any_word(tok, &["UNION", "EXCEPT", "INTERSECT", "WINDOW"])
}

/// Join-kind / positional words that may sit between a table reference and
/// the next JOIN keyword — never aliases (mirrors `opt_table_alias`).
fn is_join_side_word(tok: &Tok) -> bool {
    is_any_word(tok, &["LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "OUTER", "USING"])
}

/// One table reference found in a table position.
struct TableRef {
    /// Token index of the first token (the qualifier if present, else name).
    start: usize,
    /// Token index of the table-name token.
    name_idx: usize,
    /// `db` qualifier as written, if any.
    db: Option<String>,
    name: String,
    /// Whether an explicit alias follows (AS-form or bare).
    has_alias: bool,
    /// The FROM-scope this entry belongs to (index into the scope list).
    scope: usize,
}

/// How one reference resolved.
enum Target {
    Main,
    Attached(usize),
    /// Bare name known nowhere — leave it for the binder's error.
    Unknown,
}

/// Byte-span edit: replace `sql[start..end]` with `text`.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Resolve every table reference in `sql` against `scope` and rewrite
/// (see the module docs for the full contract).
pub fn resolve_db_refs(sql: &str, scope: &DbScope) -> Result<DbResolution> {
    let toks = token::tokenize(sql)?;
    if toks.is_empty() {
        return Ok(DbResolution::MainOnly(sql.to_string()));
    }
    // End of token i's span = start of token i+1 (trailing trivia included;
    // replacements append one space so splices never fuse tokens).
    let end_of = |i: usize| toks.get(i + 1).map_or(sql.len(), |t| t.pos);

    let mut head = 0usize;
    if toks[head].tok == Tok::Kw(Kw::Explain) {
        head += 1;
    }
    let class = match toks.get(head).map(|t| &t.tok) {
        Some(Tok::Kw(Kw::Select)) | Some(Tok::Kw(Kw::Values)) => StmtClass::Read,
        Some(t) if is_word(t, "WITH") => StmtClass::Read,
        Some(Tok::Kw(Kw::Insert)) | Some(Tok::Kw(Kw::Update)) | Some(Tok::Kw(Kw::Delete)) => {
            StmtClass::Write
        }
        Some(t) if is_any_word(t, &["CREATE", "DROP", "ALTER", "REINDEX", "REPLACE"]) => {
            // REPLACE INTO is DML, but like all writes it only needs the
            // refuse/strip pass, which Ddl and Write share below.
            if is_word(t, "REPLACE") {
                StmtClass::Write
            } else {
                StmtClass::Ddl
            }
        }
        _ => StmtClass::Opaque,
    };
    if class == StmtClass::Opaque {
        return Ok(DbResolution::MainOnly(sql.to_string()));
    }

    // CTE names shadow every table for bare resolution (probe P23).
    let cte_names = collect_cte_names(&toks, head);

    // DDL and writes share the simple contract: `main.` strips, an attached
    // qualifier refuses. DDL additionally never enters the FROM machinery
    // (its dotted names can only be the target reference).
    if class == StmtClass::Ddl {
        return resolve_ddl(sql, scope, &toks, end_of);
    }

    let refs = collect_table_refs(&toks, head)?;

    // Resolve each reference.
    let mut resolved: Vec<(usize, Target)> = Vec::new(); // (refs idx, target)
    for (i, r) in refs.iter().enumerate() {
        let target = match &r.db {
            Some(db) if db.eq_ignore_ascii_case("main") => Target::Main,
            Some(db) => match scope.find_db(db) {
                Some(m) => Target::Attached(m),
                None => {
                    return Err(Error::Bind(format!("no such table: {db}.{}", r.name)));
                }
            },
            None => {
                if cte_names.contains(&r.name) || scope.main.contains(&r.name) {
                    Target::Main
                } else if let Some(m) = scope
                    .attached
                    .iter()
                    .position(|(_, names)| names.contains(&r.name))
                {
                    Target::Attached(m)
                } else {
                    Target::Unknown
                }
            }
        };
        resolved.push((i, target));
    }

    let any_attached = resolved
        .iter()
        .any(|(_, t)| matches!(t, Target::Attached(_)));

    if class == StmtClass::Write {
        if let Some((i, Target::Attached(m))) = resolved
            .iter()
            .find(|(_, t)| matches!(t, Target::Attached(_)))
        {
            let r = &refs[*i];
            let db = &scope.attached[*m].0;
            return Err(Error::Unsupported(format!(
                "cross-file writes are not supported in v1: table `{}` is in \
                 attached database `{db}` (write through a handle opened on \
                 that file instead)",
                r.name
            )));
        }
        // No attached reference: strip `main.` qualifiers and pass through.
        let mut edits = Vec::new();
        for r in &refs {
            if r.db.is_some() {
                edits.push(Edit {
                    start: toks[r.start].pos,
                    end: end_of(r.name_idx),
                    text: format!("{} ", quote_ident(&r.name)),
                });
            }
        }
        // 3-part `main.t.c` column refs in expressions.
        three_part_edits(&toks, scope, &refs, end_of, &mut edits)?;
        return Ok(DbResolution::MainOnly(apply_edits(sql, edits)));
    }

    // Read statement. Refuse the one shape the alias rewrite cannot express:
    // two same-named FROM entries from different databases in one scope
    // without explicit aliases (sqlite answers per-reference and errors only
    // on ambiguous columns; one row-name cannot mean two tables here).
    if any_attached {
        for (ai, (i, t)) in resolved.iter().enumerate() {
            if !matches!(t, Target::Attached(_)) {
                continue;
            }
            let r = &refs[*i];
            if r.has_alias {
                continue;
            }
            for (j, u) in resolved.iter().take(ai).map(|(j, u)| (*j, u)) {
                let s = &refs[j];
                if s.scope == r.scope && !s.has_alias && s.name == r.name {
                    let same = match (t, u) {
                        (Target::Attached(a), Target::Attached(b)) => a == b,
                        _ => false,
                    };
                    if !same {
                        return Err(Error::Unsupported(format!(
                            "table name `{}` appears from two databases in one \
                             FROM; add AS aliases to disambiguate",
                            r.name
                        )));
                    }
                }
            }
        }
    }

    let mut edits = Vec::new();
    let mut cross_tables: Vec<(String, String)> = Vec::new();
    for (i, target) in &resolved {
        let r = &refs[*i];
        match target {
            Target::Main => {
                if r.db.is_some() {
                    edits.push(Edit {
                        start: toks[r.start].pos,
                        end: end_of(r.name_idx),
                        text: format!("{} ", quote_ident(&r.name)),
                    });
                }
            }
            Target::Attached(m) => {
                let db = &scope.attached[*m].0;
                let mangled = mangle(db, &r.name);
                let alias = if r.has_alias {
                    String::new()
                } else {
                    format!(" AS {}", quote_ident(&r.name))
                };
                edits.push(Edit {
                    start: toks[r.start].pos,
                    end: end_of(r.name_idx),
                    text: format!("{}{alias} ", quote_ident(&mangled)),
                });
                let pair = (db.clone(), r.name.clone());
                if !cross_tables.contains(&pair) {
                    cross_tables.push(pair);
                }
            }
            Target::Unknown => {}
        }
    }
    three_part_edits(&toks, scope, &refs, end_of, &mut edits)?;
    let out = apply_edits(sql, edits);
    if cross_tables.is_empty() {
        Ok(DbResolution::MainOnly(out))
    } else {
        Ok(DbResolution::Cross {
            sql: out,
            tables: cross_tables,
        })
    }
}

/// DDL: strip `main.` on any dotted name; refuse an attached qualifier.
fn resolve_ddl(
    sql: &str,
    scope: &DbScope,
    toks: &[SpTok],
    end_of: impl Fn(usize) -> usize,
) -> Result<DbResolution> {
    let mut edits = Vec::new();
    let mut i = 0;
    while i + 2 < toks.len() {
        let trip = (
            ident_of(&toks[i].tok),
            &toks[i + 1].tok,
            ident_of(&toks[i + 2].tok),
        );
        if let (Some(db), Tok::Dot, Some(name)) = trip {
            if db.eq_ignore_ascii_case("main") {
                let name = name.to_string();
                edits.push(Edit {
                    start: toks[i].pos,
                    end: end_of(i + 2),
                    text: format!("{} ", quote_ident(&name)),
                });
                i += 3;
                continue;
            }
            if let Some(m) = scope.find_db(db) {
                return Err(Error::Unsupported(format!(
                    "DDL on an attached database is not supported in v1: \
                     `{}.{name}` is in attached database `{}`",
                    db, scope.attached[m].0
                )));
            }
        }
        i += 1;
    }
    Ok(DbResolution::MainOnly(apply_edits(sql, edits)))
}

/// Collect `WITH [RECURSIVE] name [(cols)] AS ( … ) [, name …]` names.
fn collect_cte_names(toks: &[SpTok], head: usize) -> HashSet<String> {
    let mut names = HashSet::new();
    if !toks.get(head).is_some_and(|t| is_word(&t.tok, "WITH")) {
        return names;
    }
    let mut i = head + 1;
    if toks.get(i).is_some_and(|t| is_word(&t.tok, "RECURSIVE")) {
        i += 1;
    }
    while let Some(name) = toks.get(i).and_then(|t| ident_of(&t.tok)) {
        names.insert(name.to_string());
        i += 1;
        // Optional column list.
        if toks.get(i).is_some_and(|t| t.tok == Tok::LParen) {
            i = skip_balanced(toks, i);
        }
        if !toks
            .get(i)
            .is_some_and(|t| t.tok == Tok::Kw(Kw::As))
        {
            break;
        }
        i += 1;
        if !toks.get(i).is_some_and(|t| t.tok == Tok::LParen) {
            break;
        }
        i = skip_balanced(toks, i);
        if toks.get(i).is_some_and(|t| t.tok == Tok::Comma) {
            i += 1;
            continue;
        }
        break;
    }
    names
}

/// `toks[at]` is `(`; return the index just past its matching `)`.
fn skip_balanced(toks: &[SpTok], at: usize) -> usize {
    let mut depth = 0usize;
    let mut i = at;
    while i < toks.len() {
        match toks[i].tok {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// The FROM-list state machine: find every table-reference position.
/// One `FromMode` per FROM clause (subqueries push their own); a mode
/// expects a table name right after FROM/JOIN and after each list comma at
/// its own depth. INTO/UPDATE targets are read directly.
fn collect_table_refs(toks: &[SpTok], head: usize) -> Result<Vec<TableRef>> {
    struct FromMode {
        depth: usize,
        expect_table: bool,
        scope: usize,
    }
    let mut refs: Vec<TableRef> = Vec::new();
    let mut modes: Vec<FromMode> = Vec::new();
    let mut depth = 0usize;
    let mut next_scope = 0usize;
    let mut i = head;
    // A direct write target follows INSERT INTO / REPLACE INTO / UPDATE.
    let mut expect_target = false;
    // Paren bookkeeping: `FROM ( a JOIN b ON … )` parens are associativity
    // no-ops in the grammar — TRANSPARENT here (`true` = slack: they do not
    // change `depth`, so the commas/JOINs inside stay at the FROM list's own
    // depth, exactly as the parser consumes them between join steps).
    let mut paren_stack: Vec<bool> = Vec::new();
    while i < toks.len() {
        let tok = &toks[i].tok;
        match tok {
            Tok::LParen => {
                let in_table_slot = modes
                    .last()
                    .is_some_and(|m| m.depth == depth && m.expect_table);
                let subq = match toks.get(i + 1).map(|t| &t.tok) {
                    Some(Tok::Kw(Kw::Select)) => true,
                    Some(t2) => is_word(t2, "WITH"),
                    None => false,
                };
                if in_table_slot && subq {
                    // A derived table `FROM (SELECT …)`: the paren satisfies
                    // the slot; its inner FROM pushes its own mode.
                    if let Some(m) = modes.last_mut() {
                        m.expect_table = false;
                    }
                }
                let slack = in_table_slot && !subq;
                paren_stack.push(slack);
                if !slack {
                    depth += 1;
                }
            }
            Tok::RParen => {
                if let Some(slack) = paren_stack.pop() {
                    if !slack {
                        depth = depth.saturating_sub(1);
                        while modes.last().is_some_and(|m| m.depth > depth) {
                            modes.pop();
                        }
                    }
                }
            }
            Tok::Semicolon => {
                modes.clear();
                paren_stack.clear();
                depth = 0;
                expect_target = false;
            }
            Tok::Kw(Kw::From) => {
                modes.push(FromMode {
                    depth,
                    expect_table: true,
                    scope: next_scope,
                });
                next_scope += 1;
            }
            Tok::Kw(Kw::Join) => {
                if let Some(m) = modes.last_mut() {
                    if m.depth == depth {
                        m.expect_table = true;
                    }
                }
            }
            Tok::Kw(Kw::Into) => {
                expect_target = true;
            }
            Tok::Kw(Kw::Update) => {
                // `ON CONFLICT (…) DO UPDATE SET …` is not a write target.
                if i == 0 || !matches!(toks[i - 1].tok, Tok::Kw(Kw::Do)) {
                    expect_target = true;
                }
            }
            Tok::Comma => {
                if let Some(m) = modes.last_mut() {
                    if m.depth == depth {
                        m.expect_table = true;
                    }
                }
            }
            t if ends_from_list(t) && !matches!(t, Tok::Kw(Kw::Select)) => {
                if modes.last().is_some_and(|m| m.depth == depth) {
                    modes.pop();
                }
            }
            Tok::Kw(Kw::Select) => {
                // A SELECT at a mode's own depth is the next compound arm
                // (UNION SELECT …): that FROM list is over.
                if modes.last().is_some_and(|m| m.depth == depth) {
                    modes.pop();
                }
            }
            _ => {
                let in_table_slot = expect_target
                    || modes
                        .last()
                        .is_some_and(|m| m.depth == depth && m.expect_table);
                if in_table_slot {
                    if let Some(name) = ident_of(tok) {
                        // Never a table: the join-side positional words.
                        if !expect_target && is_join_side_word(tok) {
                            i += 1;
                            continue;
                        }
                        let start = i;
                        let (db, name, name_idx) = if toks
                            .get(i + 1)
                            .is_some_and(|t| t.tok == Tok::Dot)
                        {
                            match toks.get(i + 2).and_then(|t| ident_of(&t.tok)) {
                                Some(n) => (Some(name.to_string()), n.to_string(), i + 2),
                                None => (None, name.to_string(), i),
                            }
                        } else {
                            (None, name.to_string(), i)
                        };
                        // A further dot (`a.b.c` in table position) is not a
                        // table reference — leave it to the parser's error.
                        if toks.get(name_idx + 1).is_some_and(|t| t.tok == Tok::Dot) {
                            i += 1;
                            continue;
                        }
                        // Explicit alias?
                        let has_alias = match toks.get(name_idx + 1).map(|t| &t.tok) {
                            Some(Tok::Kw(Kw::As)) => true,
                            Some(Tok::QuotedIdent(_)) => true,
                            Some(Tok::Ident(_)) => {
                                let t = &toks[name_idx + 1].tok;
                                !is_join_side_word(t)
                                    && !is_any_word(t, &["UNION", "EXCEPT", "INTERSECT"])
                            }
                            _ => false,
                        };
                        refs.push(TableRef {
                            start,
                            name_idx,
                            db,
                            name,
                            has_alias,
                            scope: modes.last().map_or(usize::MAX, |m| m.scope),
                        });
                        if expect_target {
                            expect_target = false;
                        } else if let Some(m) = modes.last_mut() {
                            m.expect_table = false;
                        }
                        i = name_idx + 1;
                        continue;
                    }
                    // Not an identifier (e.g. `(` handled above, VALUES …):
                    // the slot stays open only for parens; anything else
                    // closes it.
                    if !matches!(tok, Tok::LParen) {
                        if expect_target {
                            expect_target = false;
                        } else if let Some(m) = modes.last_mut() {
                            m.expect_table = false;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    Ok(refs)
}

/// Rewrite 3-part `db.table.col` column references (probe P4b/P4c): the
/// `db.table` prefix becomes the row-name the FROM rewrite exposes — the
/// bare table name. Positions inside collected table refs are skipped.
fn three_part_edits(
    toks: &[SpTok],
    scope: &DbScope,
    refs: &[TableRef],
    end_of: impl Fn(usize) -> usize,
    edits: &mut Vec<Edit>,
) -> Result<()> {
    let mut in_ref = vec![false; toks.len()];
    for r in refs {
        in_ref[r.start..=r.name_idx].fill(true);
    }
    let mut i = 0;
    while i + 4 < toks.len() {
        if in_ref[i] {
            i += 1;
            continue;
        }
        let parts = (
            ident_of(&toks[i].tok),
            &toks[i + 1].tok,
            ident_of(&toks[i + 2].tok),
            &toks[i + 3].tok,
            ident_of(&toks[i + 4].tok),
        );
        if let (Some(db), Tok::Dot, Some(table), Tok::Dot, Some(_col)) = parts {
            let is_db =
                db.eq_ignore_ascii_case("main") || scope.find_db(db).is_some();
            if is_db {
                let table = table.to_string();
                edits.push(Edit {
                    start: toks[i].pos,
                    end: end_of(i + 2),
                    text: format!("{} ", quote_ident(&table)),
                });
                i += 5;
                continue;
            }
        }
        i += 1;
    }
    Ok(())
}

fn apply_edits(sql: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| e.start);
    let mut out = String::with_capacity(sql.len() + 32 * edits.len());
    let mut at = 0usize;
    for e in edits {
        debug_assert!(e.start >= at, "overlapping db-ref edits");
        out.push_str(&sql[at..e.start]);
        out.push_str(&e.text);
        at = e.end;
    }
    out.push_str(&sql[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> DbScope {
        let mut main = HashSet::new();
        main.insert("t".to_string());
        main.insert("v".to_string());
        let mut other = HashSet::new();
        other.insert("u".to_string());
        other.insert("t".to_string());
        let mut third = HashSet::new();
        third.insert("u".to_string());
        third.insert("w".to_string());
        DbScope {
            main,
            attached: vec![
                ("other".to_string(), other),
                ("third".to_string(), third),
            ],
        }
    }

    fn cross(sql: &str) -> (String, Vec<(String, String)>) {
        match resolve_db_refs(sql, &scope()).unwrap() {
            DbResolution::Cross { sql, tables } => (sql, tables),
            DbResolution::MainOnly(s) => panic!("expected Cross, got MainOnly({s})"),
        }
    }

    fn main_only(sql: &str) -> String {
        match resolve_db_refs(sql, &scope()).unwrap() {
            DbResolution::MainOnly(s) => s,
            DbResolution::Cross { sql, .. } => panic!("expected MainOnly, got Cross({sql})"),
        }
    }

    #[test]
    fn qualified_single_table() {
        let (sql, tables) = cross("SELECT * FROM other.u WHERE x = 1");
        assert_eq!(sql, "SELECT * FROM \"other.u\" AS \"u\" WHERE x = 1");
        assert_eq!(tables, vec![("other".into(), "u".into())]);
    }

    #[test]
    fn bare_name_resolves_main_first_then_attach_order() {
        // `t` is in main and other: main wins (probe P1).
        assert_eq!(main_only("SELECT * FROM t"), "SELECT * FROM t");
        // `u` is in other and third: first attach order wins (probe P2b).
        let (sql, tables) = cross("SELECT * FROM u");
        assert_eq!(sql, "SELECT * FROM \"other.u\" AS \"u\" ");
        assert_eq!(tables, vec![("other".into(), "u".into())]);
        // `w` only in third.
        let (_, tables) = cross("SELECT * FROM w");
        assert_eq!(tables, vec![("third".into(), "w".into())]);
    }

    #[test]
    fn main_qualifier_strips() {
        assert_eq!(main_only("SELECT * FROM main.t"), "SELECT * FROM \"t\" ");
        assert_eq!(
            main_only("SELECT main.t.a FROM main.t"),
            "SELECT \"t\" .a FROM \"t\" "
        );
    }

    #[test]
    fn cross_join_and_three_part() {
        let (sql, tables) =
            cross("SELECT t.a, other.u.y FROM main.t JOIN other.u ON other.u.x = t.a");
        assert_eq!(
            sql,
            "SELECT t.a, \"u\" .y FROM \"t\" JOIN \"other.u\" AS \"u\" ON \"u\" .x = t.a"
        );
        assert_eq!(tables, vec![("other".into(), "u".into())]);
    }

    #[test]
    fn explicit_alias_kept() {
        let (sql, _) = cross("SELECT z.y FROM other.u AS z");
        assert_eq!(sql, "SELECT z.y FROM \"other.u\" AS z");
        let (sql, _) = cross("SELECT z.y FROM other.u z");
        assert_eq!(sql, "SELECT z.y FROM \"other.u\" z");
    }

    #[test]
    fn unknown_db_is_no_such_table() {
        let e = resolve_db_refs("SELECT * FROM nope.t", &scope()).unwrap_err();
        assert!(e.to_string().contains("no such table: nope.t"), "{e}");
    }

    #[test]
    fn unknown_bare_name_left_to_binder() {
        assert_eq!(main_only("SELECT * FROM zzz"), "SELECT * FROM zzz");
    }

    #[test]
    fn writes_to_attached_refused_by_name() {
        for sql in [
            "INSERT INTO other.u (x) VALUES (1)",
            "UPDATE other.u SET y = 1",
            "DELETE FROM other.u",
            // bare name resolving to attached
            "INSERT INTO u (x) VALUES (1)",
            // main write reading attached
            "INSERT INTO t SELECT x, 'a' FROM other.u",
            "UPDATE t SET tag = 'x' WHERE a IN (SELECT x FROM other.u)",
        ] {
            let e = resolve_db_refs(sql, &scope()).unwrap_err();
            assert!(
                e.to_string().contains("cross-file writes"),
                "{sql} → {e}"
            );
        }
    }

    #[test]
    fn write_to_main_with_qualifier_strips() {
        assert_eq!(
            main_only("INSERT INTO main.t (a) VALUES (1)"),
            "INSERT INTO \"t\" (a) VALUES (1)"
        );
        assert_eq!(
            main_only("UPDATE main.t SET a = 1"),
            "UPDATE \"t\" SET a = 1"
        );
        assert_eq!(main_only("DELETE FROM main.t"), "DELETE FROM \"t\" ");
    }

    #[test]
    fn ddl_on_attached_refused_main_strips() {
        let e = resolve_db_refs("CREATE TABLE other.w (q INT)", &scope()).unwrap_err();
        assert!(e.to_string().contains("DDL on an attached database"), "{e}");
        let e = resolve_db_refs("DROP TABLE third.u", &scope()).unwrap_err();
        assert!(e.to_string().contains("DDL on an attached database"), "{e}");
        assert_eq!(
            main_only("DROP TABLE main.t"),
            "DROP TABLE \"t\" "
        );
    }

    #[test]
    fn same_name_two_dbs_needs_aliases() {
        let e = resolve_db_refs("SELECT * FROM main.t, other.t", &scope()).unwrap_err();
        assert!(e.to_string().contains("add AS aliases"), "{e}");
        // With aliases it flows.
        let (sql, tables) =
            cross("SELECT a.tag, b.tag FROM main.t AS a, other.t AS b");
        assert_eq!(
            sql,
            "SELECT a.tag, b.tag FROM \"t\" AS a, \"other.t\" AS b"
        );
        assert_eq!(tables, vec![("other".into(), "t".into())]);
    }

    #[test]
    fn self_join_same_attached_table_twice_is_fine() {
        let (sql, tables) =
            cross("SELECT a.x FROM other.u AS a JOIN other.u AS b ON a.x = b.y");
        assert_eq!(
            sql,
            "SELECT a.x FROM \"other.u\" AS a JOIN \"other.u\" AS b ON a.x = b.y"
        );
        assert_eq!(tables, vec![("other".into(), "u".into())]);
    }

    #[test]
    fn cte_shadows_attached_table() {
        // `u` would resolve to other.u, but the CTE shadows it (probe P23).
        let s = main_only("WITH u(z) AS (SELECT 42) SELECT * FROM u");
        assert_eq!(s, "WITH u(z) AS (SELECT 42) SELECT * FROM u");
    }

    #[test]
    fn subquery_and_compound_positions() {
        let (sql, tables) = cross("SELECT a FROM t WHERE a IN (SELECT x FROM other.u)");
        assert_eq!(
            sql,
            "SELECT a FROM t WHERE a IN (SELECT x FROM \"other.u\" AS \"u\" )"
        );
        assert_eq!(tables, vec![("other".into(), "u".into())]);
        let (sql, _) = cross("SELECT a FROM t UNION SELECT x FROM other.u ORDER BY 1");
        assert_eq!(
            sql,
            "SELECT a FROM t UNION SELECT x FROM \"other.u\" AS \"u\" ORDER BY 1"
        );
    }

    #[test]
    fn string_literals_are_never_rewritten() {
        let s = main_only("SELECT * FROM t WHERE tag = 'from other.u to x'");
        assert_eq!(s, "SELECT * FROM t WHERE tag = 'from other.u to x'");
    }

    #[test]
    fn left_join_side_words_not_aliases() {
        let (sql, _) =
            cross("SELECT t.a FROM t LEFT JOIN other.u ON u.x = t.a");
        assert_eq!(
            sql,
            "SELECT t.a FROM t LEFT JOIN \"other.u\" AS \"u\" ON u.x = t.a"
        );
    }

    #[test]
    fn attach_parses() {
        assert_eq!(
            parse_attach("ATTACH DATABASE '/x/y.mpedb' AS other").unwrap(),
            Some(AttachStmt::Attach {
                path: "/x/y.mpedb".into(),
                name: "other".into()
            })
        );
        assert_eq!(
            parse_attach("attach 'f.mpedb' as o2;").unwrap(),
            Some(AttachStmt::Attach {
                path: "f.mpedb".into(),
                name: "o2".into()
            })
        );
        assert_eq!(
            parse_attach("DETACH DATABASE other").unwrap(),
            Some(AttachStmt::Detach {
                name: "other".into()
            })
        );
        assert_eq!(
            parse_attach("DETACH \"we ird\"").unwrap(),
            Some(AttachStmt::Detach {
                name: "we ird".into()
            })
        );
        assert_eq!(parse_attach("SELECT 1").unwrap(), None);
        // Bound-parameter path refused by name (probe P18 divergence).
        assert!(parse_attach("ATTACH ? AS x").is_err());
    }
}
