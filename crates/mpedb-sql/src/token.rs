//! SQL tokenizer. Produces byte-offset-annotated tokens; keywords are
//! recognized case-insensitively.
//!
//! Identifiers are lexed VERBATIM — the tokenizer preserves the spelling and
//! does not fold. Folding happens where names are COMPARED
//! (`mpedb_types::ident`), because sqlite reports every name back in the
//! spelling it was declared with; the token is the thing that carries that
//! spelling. `Ident` vs `QuotedIdent` is kept only so the grammar can tell a
//! bare word that might be a keyword from a quoted one that never is — NOT
//! because quoting affects case (measured: it does not).

use mpedb_types::{Dialect, Error, Result};

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(feature = "pg-dialect"), allow(dead_code))]
pub(crate) enum Tok {
    /// Bare identifier (not a keyword). Spelled as written; comparisons
    /// against it fold ASCII case (`mpedb_types::ident_eq`).
    Ident(String),
    /// Quoted identifier. Three spellings, all sqlite's and all folded to this
    /// one token so the grammar never has to care which was written:
    /// `"a"` (`""` escapes), `` `a` `` (``` `` ``` escapes), `[a]` (no escape,
    /// closed by the first `]` — MS-Access/SQL-Server style, which sqlite
    /// accepts too).
    /// The bool is `true` for a DOUBLE-quoted identifier — the one spelling
    /// sqlite's DQS misfeature applies to: in EXPRESSION position, a
    /// double-quoted name that resolves to no column becomes a string
    /// LITERAL. Backtick and `[…]` never do (measured, 3.45.1), so the flag
    /// is what lets the parser tell the binder which names are eligible.
    QuotedIdent(String, bool),
    Kw(Kw),
    Int(i64),
    Float(f64),
    Str(String),
    Blob(Vec<u8>),
    /// `$n` parameter, stored 0-based ($1 == 0).
    DollarParam(u16),
    /// Anonymous `?` parameter (numbered by the parser).
    Question,
    Eq,
    Ne,
    Lt,
    Le,
    /// `||` — SQL concatenation.
    Concat,
    Gt,
    Ge,
    Plus,
    Minus,
    /// `->` — sqlite's JSON operator returning the selected node's JSON TEXT.
    Arrow,
    /// `->>` — sqlite's JSON operator returning the selected node as a SQL
    /// value. Lexed BEFORE `->`; see the `-` arm of the scanner.
    ArrowText,
    /// `:sym:` — a USER-DEFINED operator (stage M3, SQL-EXTENSIONS.md),
    /// self-delimited by colons so it lexes in one scan with no maximal-munch
    /// interaction with anything (`:` begins nothing else in this grammar —
    /// mpedb's parameters are `$n`/`?`, never `:name`). Carries the symbol
    /// between the colons; the PARSER decides what it means from the operator
    /// catalog, and an undefined one refuses by name there.
    CustomOp(String),
    Star,
    Slash,
    Percent,
    /// `&` — bitwise AND. A single `&`; sqlite has no `&&`.
    BitAnd,
    /// `|` — bitwise OR. Two of them (`||`) are [`Tok::Concat`] instead, which
    /// is why the lexer must look ahead one byte here.
    BitOr,
    /// `<<` — left shift. Lexed before `<` / `<=` / `<>` for the same reason.
    Shl,
    /// `>>` — right shift.
    Shr,
    /// `~` — bitwise NOT (prefix). sqlite has no infix `~`.
    Tilde,
    LParen,
    RParen,
    Comma,
    Semicolon,
    /// `.` — only ever used to qualify a table with a database alias
    /// (`alias.table`) for `Workspace` routing; not otherwise part of the grammar.
    Dot,
    /// `::` — PostgreSQL's cast operator, PG DIALECT ONLY.
    ///
    /// Constructed only by `pg::lex`, which is compiled only with the
    /// `pg-dialect` feature — so without it these four variants are declared
    /// and never built. They stay declared in BOTH builds on purpose: the
    /// parser matches on them unconditionally, and a `#[cfg]` on an enum
    /// variant would push that `#[cfg]` into every match arm.
    ///
    /// It cannot exist under the sqlite dialect because `:` opens a `:sym:`
    /// custom operator there (SQL-EXTENSIONS.md) and `a::text` would lex as an
    /// unterminated one. That is not a gap to be closed later: the two spellings
    /// want the same byte, so the dialect has to pick, and PG dialect trades
    /// `:sym:` away for `::`.
    DoubleColon,
    /// `~*` — PostgreSQL case-insensitive regex match. (Plain `~` is already
    /// [`Tok::Tilde`]; PG makes it INFIX, which the parser decides, not the
    /// lexer.)
    TildeStar,
    /// `!~` — PostgreSQL negated regex match.
    NotTilde,
    /// `!~*` — PostgreSQL negated case-insensitive regex match.
    NotTildeStar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kw {
    /// Only `x IN (current_setting('k'))` in v1 — the context-membership form
    /// (DESIGN-MULTIDB §2.6). General `IN (a, b, c)` is task #21.
    In,
    Select,
    From,
    Where,
    Order,
    By,
    As,
    Asc,
    Desc,
    Limit,
    Offset,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Begin,
    Commit,
    Rollback,
    Explain,
    And,
    Or,
    Not,
    Between,
    Case,
    When,
    Then,
    Else,
    End,
    Conflict,
    Do,
    Nothing,
    Returning,
    Group,
    Having,
    Distinct,
    Join,
    Inner,
    On,
    Is,
    Null,
    Like,
    Glob,
    Regexp,
    Match,
    True,
    False,
}

fn keyword(word: &str) -> Option<Kw> {
    // Case-insensitive keyword match; anything else is an identifier.
    Some(match word.to_ascii_uppercase().as_str() {
        "SELECT" => Kw::Select,
        "FROM" => Kw::From,
        "WHERE" => Kw::Where,
        "ORDER" => Kw::Order,
        "BY" => Kw::By,
        "AS" => Kw::As,
        "ASC" => Kw::Asc,
        "DESC" => Kw::Desc,
        "LIMIT" => Kw::Limit,
        "OFFSET" => Kw::Offset,
        "INSERT" => Kw::Insert,
        "INTO" => Kw::Into,
        "VALUES" => Kw::Values,
        "UPDATE" => Kw::Update,
        "SET" => Kw::Set,
        "DELETE" => Kw::Delete,
        "BEGIN" => Kw::Begin,
        "COMMIT" => Kw::Commit,
        "ROLLBACK" => Kw::Rollback,
        "EXPLAIN" => Kw::Explain,
        "AND" => Kw::And,
        "OR" => Kw::Or,
        "NOT" => Kw::Not,
        "BETWEEN" => Kw::Between,
        "CASE" => Kw::Case,
        "WHEN" => Kw::When,
        "THEN" => Kw::Then,
        "ELSE" => Kw::Else,
        "END" => Kw::End,
        "CONFLICT" => Kw::Conflict,
        "DO" => Kw::Do,
        "NOTHING" => Kw::Nothing,
        "RETURNING" => Kw::Returning,
        "GROUP" => Kw::Group,
        "HAVING" => Kw::Having,
        "DISTINCT" => Kw::Distinct,
        "JOIN" => Kw::Join,
        "INNER" => Kw::Inner,
        "ON" => Kw::On,
        "IS" => Kw::Is,
        "NULL" => Kw::Null,
        "LIKE" => Kw::Like,
        "GLOB" => Kw::Glob,
        "REGEXP" => Kw::Regexp,
        "MATCH" => Kw::Match,
        "IN" => Kw::In,
        "TRUE" => Kw::True,
        "FALSE" => Kw::False,
        _ => return None,
    })
}

/// A token plus its byte SPAN in the source: `pos` is the offset of its first
/// character, `end` one past its last. The span covers the token AS WRITTEN —
/// a quoted identifier's delimiters and doubled escapes included — so a
/// rewriter can splice over it without re-deriving where the token stopped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpTok {
    pub tok: Tok,
    pub pos: usize,
    pub end: usize,
}

/// The PostgreSQL containment / jsonb-path operator at the head of `rest`, if
/// there is one.
///
/// LONGEST FIRST: `@>` before `@`, `#>>` before `#>`. A shorter match would
/// name the wrong operator in the refusal, which is the whole thing this
/// function exists to avoid.
fn pg_json_operator(rest: &str) -> Option<&'static str> {
    // `<@` is NOT here: its `<` is consumed as a comparison operator before
    // this is ever reached, so it is recognised by looking backward at the
    // call site instead.
    ["#>>", "@>", "#>", "@@", "@?", "?&", "?|"]
        .into_iter()
        .find(|op| rest.starts_with(op))
}

fn perr(pos: usize, msg: impl Into<String>) -> Error {
    Error::Parse {
        pos,
        msg: msg.into(),
    }
}

/// Lex under the **sqlite** dialect — the spelling every existing caller, test
/// and corpus record means. Byte-for-byte what this function has always done.
pub(crate) fn tokenize(sql: &str) -> Result<Vec<SpTok>> {
    tokenize_dialect(sql, Dialect::Sqlite)
}

/// Lex under an explicit dialect.
///
/// The two dialects want the SAME BYTES for different things — `::` is a cast
/// in PG and an unterminated `:sym:` here, `$$` is a quoted string there and a
/// malformed `$n` here — so this is a lexer-level decision, not a binder one.
/// The PG-owned bytes are dispatched ONCE, before the sqlite match below, and
/// `pg::lex::special` hands back `None` for anything it does not own. That
/// shape is deliberate: the sqlite arms are untouched, so the dialect cannot
/// move a corpus answer by accident — it can only fail to add a PG one.
pub(crate) fn tokenize_dialect(sql: &str, dialect: Dialect) -> Result<Vec<SpTok>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let c = b[i];
        if dialect == Dialect::Postgres {
            if let Some((tok, next)) = crate::pg::lex::special(b, i)? {
                out.push(SpTok {
                    tok,
                    pos: start,
                    end: next,
                });
                i = next;
                continue;
            }
        }
        let tok = match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b';' => {
                i += 1;
                Tok::Semicolon
            }
            // A `.5`-style float — a fraction with NO leading digit, which is
            // what Django's ORM writes for `price * .5`. The digit arm cannot
            // see it (there is no leading digit), so the dispatch happens here.
            // A dot NOT followed by a digit stays a qualifier dot, so
            // `alias.table` is unaffected and a lone `.` still reaches the
            // parser's error, as in sqlite.
            b'.' if sql.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let (tok, next) = lex_number(sql, i)?;
                i = next;
                tok
            }
            b'.' => {
                i += 1;
                Tok::Dot
            }
            // `==` is sqlite's accepted alias for `=` (one token, identical
            // semantics — not a separate operator).
            b'=' => {
                i += if b.get(i + 1) == Some(&b'=') { 2 } else { 1 };
                Tok::Eq
            }
            b'+' => {
                i += 1;
                Tok::Plus
            }
            // `-` also opens the two JSON operators. `->>` MUST be tested
            // before `->`, or `a ->> '$.x'` lexes as `a -> (> '$.x')` and the
            // SQL-text form silently becomes the JSON-text one.
            b':' => {
                // `:sym:` — scan to the closing colon. Bounded and strict:
                // an unterminated or empty or whitespace-carrying symbol is a
                // lex error naming the doc, never a silent reinterpretation.
                let mut j = i + 1;
                while j < b.len() && b[j] != b':' && j - i <= 17 {
                    if b[j].is_ascii_whitespace() {
                        return Err(perr(
                            start,
                            "`:` begins a custom operator (`:sym:`), which cannot                              contain whitespace — see SQL-EXTENSIONS.md",
                        ));
                    }
                    j += 1;
                }
                if j >= b.len() || b[j] != b':' || j == i + 1 {
                    return Err(perr(
                        start,
                        "`:` begins a custom operator and needs a closing `:`                          (`:sym:`, at most 16 characters) — see SQL-EXTENSIONS.md",
                    ));
                }
                let sym = std::str::from_utf8(&b[i + 1..j])
                    .map_err(|_| perr(start, "custom operator symbol is not UTF-8"))?
                    .to_string();
                i = j + 1;
                Tok::CustomOp(sym)
            }
            b'-' => match (b.get(i + 1), b.get(i + 2)) {
                // `-- …` is a line comment ANYWHERE in the statement, not only
                // at the front: it runs to the next newline (or end of input).
                // Produces no token at all, so it is invisible to the parser.
                (Some(b'-'), _) => {
                    i += 2;
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                (Some(b'>'), Some(b'>')) => {
                    i += 3;
                    Tok::ArrowText
                }
                (Some(b'>'), _) => {
                    i += 2;
                    Tok::Arrow
                }
                _ => {
                    i += 1;
                    Tok::Minus
                }
            },
            b'*' => {
                i += 1;
                Tok::Star
            }
            // `/* … */` block comment, anywhere. sqlite does NOT require the
            // terminator: an unclosed `/*` comments out the rest of the input
            // rather than being a syntax error, so neither does this.
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            b'/' => {
                i += 1;
                Tok::Slash
            }
            b'%' => {
                i += 1;
                Tok::Percent
            }
            b'!' => {
                if b.get(i + 1) == Some(&b'=') {
                    i += 2;
                    Tok::Ne
                } else {
                    return Err(perr(start, "expected `!=`"));
                }
            }
            b'|' => match b.get(i + 1) {
                Some(b'|') => {
                    i += 2;
                    Tok::Concat
                }
                _ => {
                    i += 1;
                    Tok::BitOr
                }
            },
            b'&' => {
                i += 1;
                Tok::BitAnd
            }
            b'~' => {
                i += 1;
                Tok::Tilde
            }
            b'<' => match b.get(i + 1) {
                Some(b'=') => {
                    i += 2;
                    Tok::Le
                }
                Some(b'>') => {
                    i += 2;
                    Tok::Ne
                }
                Some(b'<') => {
                    i += 2;
                    Tok::Shl
                }
                _ => {
                    i += 1;
                    Tok::Lt
                }
            },
            b'>' => match b.get(i + 1) {
                Some(b'=') => {
                    i += 2;
                    Tok::Ge
                }
                Some(b'>') => {
                    i += 2;
                    Tok::Shr
                }
                _ => {
                    i += 1;
                    Tok::Gt
                }
            },
            b'?' => {
                i += 1;
                Tok::Question
            }
            b'$' => {
                i += 1;
                let dstart = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == dstart {
                    return Err(perr(start, "expected parameter number after `$`"));
                }
                let n: u32 = sql[dstart..i]
                    .parse()
                    .map_err(|_| perr(start, "parameter number out of range"))?;
                if n == 0 {
                    return Err(perr(start, "parameters are numbered from $1"));
                }
                if n > u16::MAX as u32 {
                    return Err(perr(start, "parameter number out of range"));
                }
                Tok::DollarParam((n - 1) as u16)
            }
            b'\'' => {
                let (s, next) = lex_string(sql, i)?;
                i = next;
                Tok::Str(s)
            }
            // The three quoted-identifier spellings sqlite accepts. All produce
            // the SAME token: a quoted identifier is usable everywhere a bare
            // one is, and nothing downstream should be able to tell them apart.
            b'"' | b'`' => {
                let (s, next) = lex_quoted_ident(sql, i, c)?;
                i = next;
                Tok::QuotedIdent(s, c == b'"')
            }
            // `[name]` — no escape mechanism (sqlite has none either): the
            // first `]` closes it, so a `]` cannot appear in a bracketed name.
            b'[' => {
                let (s, next) = lex_bracket_ident(sql, i)?;
                i = next;
                Tok::QuotedIdent(s, false)
            }
            b'0'..=b'9' => {
                let (tok, next) = lex_number(sql, i)?;
                i = next;
                tok
            }
            // An unquoted identifier. sqlite's `IdChar` counts every byte
            // >= 0x80 as an identifier character (it does no Unicode
            // classification at all), so `select 1 as café` lexes without
            // quotes. The input is already valid UTF-8, and every
            // continuation byte is >= 0x80, so the word slice below can only
            // end on a char boundary.
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                // Blob literal x'...' / X'...' (only when a quote follows).
                if (c == b'x' || c == b'X') && b.get(i + 1) == Some(&b'\'') {
                    let (blob, next) = lex_blob(sql, i)?;
                    i = next;
                    Tok::Blob(blob)
                } else {
                    let wstart = i;
                    // `$` CONTINUES an identifier (sqlite's `IdChar` includes
                    // it): `crafted_alia$` is one name, which Django's alias
                    // generator really emits. It cannot START one, so the `$n`
                    // parameter sigil above is untouched — that branch is only
                    // reached when `$` is a token's first byte.
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric()
                            || b[i] == b'_'
                            || b[i] == b'$'
                            || b[i] >= 0x80)
                    {
                        i += 1;
                    }
                    let word = &sql[wstart..i];
                    match keyword(word) {
                        Some(kw) => Tok::Kw(kw),
                        None => Tok::Ident(word.to_owned()),
                    }
                }
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap_or('?');
                // PostgreSQL's containment and jsonb-path operators get their
                // own sentence. `unexpected character \`@\`` is true and says
                // nothing: it names a punctuation mark where the reader wrote
                // `@>`, and it is the largest single member of that bucket —
                // 679 of 908 in the corpus's forty heaviest files, with `#`
                // (`#>`, `#>>`) another 119. Two characters, one feature.
                // `<@` needs a look BACKWARD: the tokenizer already consumed
                // `<` as a comparison operator, so by the time we fail we are
                // standing on the `@` alone. Found by the test, which is the
                // only way it would have been — the forward table looks right.
                let backward = (start > 0 && sql.as_bytes()[start - 1] == b'<' && ch == '@')
                    .then_some("<@");
                if let Some(op) = backward.or_else(|| pg_json_operator(&sql[i..])) {
                    return Err(perr(
                        start,
                        format!(
                            "`{op}` is one of PostgreSQL's containment / jsonb-path                              operators. mpedb stores JSON as TEXT and reads it with the                              sqlite functions (`json_extract`, `->`, `->>`), which have no                              containment test and no path type — `{op}` would need a jsonb                              VALUE to be a value, not a string that happens to hold JSON"
                        ),
                    ));
                }
                return Err(perr(start, format!("unexpected character `{ch}`")));
            }
        };
        out.push(SpTok { tok, pos: start, end: i });
    }
    Ok(out)
}

/// Lex a `'...'` string starting at the opening quote; `''` escapes a quote.
/// Returns the string and the index just past the closing quote.
fn lex_string(sql: &str, start: usize) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut i = start + 1;
    let mut s = String::new();
    let mut seg = i;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                s.push_str(&sql[seg..=i]); // keep one quote
                i += 2;
                seg = i;
            } else {
                s.push_str(&sql[seg..i]);
                return Ok((s, i + 1));
            }
        } else {
            i += 1;
        }
    }
    Err(perr(start, "unterminated string literal"))
}

/// Lex a `"..."` / `` `...` `` identifier starting at the opening quote `q`; a
/// doubled quote escapes one. Both spellings share this code because they share
/// the rule — only the delimiter byte differs.
fn lex_quoted_ident(sql: &str, start: usize, q: u8) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut i = start + 1;
    let mut s = String::new();
    let mut seg = i;
    while i < b.len() {
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                s.push_str(&sql[seg..=i]);
                i += 2;
                seg = i;
            } else {
                s.push_str(&sql[seg..i]);
                if s.is_empty() {
                    return Err(perr(start, "empty quoted identifier"));
                }
                return Ok((s, i + 1));
            }
        } else {
            i += 1;
        }
    }
    Err(perr(start, "unterminated quoted identifier"))
}

/// Lex a `[...]` identifier starting at the opening bracket. sqlite gives this
/// spelling NO escape mechanism, so the first `]` closes the name; an empty
/// `[]` is refused for the same reason `""` is.
fn lex_bracket_ident(sql: &str, start: usize) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b']' {
            let s = &sql[start + 1..i];
            if s.is_empty() {
                return Err(perr(start, "empty quoted identifier"));
            }
            return Ok((s.to_owned(), i + 1));
        }
        i += 1;
    }
    Err(perr(start, "unterminated quoted identifier"))
}

/// Lex an integer or float literal starting at a digit or at a `.` that is
/// followed by one.
///
/// Every form MEASURED against sqlite 3.45 rather than assumed, because the
/// permissive ones look like typos and the refused ones look legal:
///
/// ```text
///   .5  5.  .5e2  5.e2  1.        accepted, all REAL
///   0x1F  0X1f                    accepted, INTEGER
///   9223372036854775808           accepted, REAL (past i64 it is a float)
///   ..5  .  .e2  1e  0b101  1_000 refused
/// ```
fn lex_number(sql: &str, start: usize) -> Result<(Tok, usize)> {
    let b = sql.as_bytes();
    let mut i = start;
    // Hex integers (`0x1F`). sqlite takes them as INTEGER; the digits are
    // required, so a bare `0x` falls through to the decimal path and lexes as
    // `0` followed by the identifier `x`, which the parser then rejects.
    if b[i] == b'0'
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        let hstart = i + 2;
        i = hstart;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        // sqlite wraps a hex literal that overflows into the i64 bit pattern
        // (it reads exactly 16 significant hex digits); anything longer is
        // refused rather than silently truncated.
        let text = &sql[hstart..i];
        let v = u64::from_str_radix(text, 16)
            .map_err(|_| perr(start, "hex integer literal out of range"))?;
        return Ok((Tok::Int(v as i64), i));
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    // A `.` makes it a float whether or not fraction digits follow: `5.` is
    // 5.0 in sqlite. The leading-dot form arrives here with `i == start`.
    if i < b.len() && b[i] == b'.' {
        is_float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        // Exponent only if followed by [+-]?digit; otherwise the `e` starts
        // the next token (which the parser will reject in context).
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    // A numeric literal may not run straight into an identifier character.
    // Without this, `1e` lexed as `1` followed by the identifier `e` — and
    // `SELECT 1 e` is legal SQL meaning `1 AS e`, so `SELECT 1e` ANSWERED 1
    // where sqlite says "unrecognized token". Same for `0b101` (→ 0 AS b101)
    // and `1_000` (→ 1 AS _000). A space still makes the alias, which is the
    // form anyone actually writes.
    if i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        return Err(perr(start, "malformed numeric literal"));
    }
    let text = &sql[start..i];
    if is_float {
        // Rust's `f64::from_str` rejects the bare-dot forms Rust itself has no
        // literal for (`.5`, `5.`, `5.e2`), so the text is normalised before
        // parsing rather than each form being special-cased downstream.
        let mut norm = String::with_capacity(text.len() + 2);
        if text.starts_with('.') {
            norm.push('0');
        }
        norm.push_str(text);
        if let Some(p) = norm.find(['e', 'E']) {
            if norm[..p].ends_with('.') {
                norm.insert(p, '0');
            }
        } else if norm.ends_with('.') {
            norm.push('0');
        }
        let v: f64 = norm
            .parse()
            .map_err(|_| perr(start, "invalid float literal"))?;
        Ok((Tok::Float(v), i))
    } else {
        match text.parse::<i64>() {
            Ok(v) => Ok((Tok::Int(v), i)),
            // Past i64 sqlite keeps the value as a REAL rather than failing —
            // `SELECT 9223372036854775808` is 9.223372036854776e+18 there.
            // Refusing would have been a narrower engine on a literal a
            // generated query can legitimately contain.
            Err(_) => {
                let v: f64 = text
                    .parse()
                    .map_err(|_| perr(start, "integer literal out of range"))?;
                Ok((Tok::Float(v), i))
            }
        }
    }
}

/// Lex `x'hexdigits'` starting at the `x`.
fn lex_blob(sql: &str, start: usize) -> Result<(Vec<u8>, usize)> {
    let b = sql.as_bytes();
    let mut i = start + 2; // past x'
    let hstart = i;
    while i < b.len() && b[i] != b'\'' {
        if !b[i].is_ascii_hexdigit() {
            return Err(perr(i, "invalid hex digit in blob literal"));
        }
        i += 1;
    }
    if i >= b.len() {
        return Err(perr(start, "unterminated blob literal"));
    }
    let hex = &sql[hstart..i];
    if !hex.len().is_multiple_of(2) {
        return Err(perr(start, "blob literal must have an even number of hex digits"));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).unwrap();
        out.push(u8::from_str_radix(s, 16).unwrap());
    }
    Ok((out, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(sql: &str) -> Vec<Tok> {
        tokenize(sql).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    /// Keywords fold to a `Kw` token; identifiers keep their SPELLING (the
    /// fold happens at comparison time, not here), and a quoted word is never
    /// a keyword.
    fn keywords_fold_identifier_spelling_is_preserved() {
        assert_eq!(
            toks("select SeLeCt_x FROM users"),
            vec![
                Tok::Kw(Kw::Select),
                Tok::Ident("SeLeCt_x".into()),
                Tok::Kw(Kw::From),
                Tok::Ident("users".into()),
            ]
        );
        assert_eq!(toks("\"select\""), vec![Tok::QuotedIdent("select".into(), true)]);
    }

    #[test]
    fn string_and_blob_escapes() {
        assert_eq!(toks("'it''s'"), vec![Tok::Str("it's".into())]);
        assert_eq!(toks("''"), vec![Tok::Str(String::new())]);
        assert_eq!(toks("x'00ff'"), vec![Tok::Blob(vec![0, 255])]);
        assert_eq!(toks("X'AB'"), vec![Tok::Blob(vec![0xab])]);
        assert_eq!(toks("x ''"), vec![Tok::Ident("x".into()), Tok::Str(String::new())]);
    }

    #[test]
    fn numbers() {
        assert_eq!(toks("42"), vec![Tok::Int(42)]);
        assert_eq!(toks("1.5"), vec![Tok::Float(1.5)]);
        assert_eq!(toks("1e3"), vec![Tok::Float(1000.0)]);
        assert_eq!(toks("2.5e-1"), vec![Tok::Float(0.25)]);
        // i64::MAX stays an integer; one more becomes a REAL, which is what
        // sqlite does (`SELECT 9223372036854775808` is 9.223372036854776e+18
        // there). Refusing was a narrower engine on a literal a generated
        // query can legitimately contain.
        assert_eq!(toks("9223372036854775807"), vec![Tok::Int(i64::MAX)]);
        assert_eq!(toks("9223372036854775808"), vec![Tok::Float(9223372036854775808.0)]);

        // The bare-dot forms, all REAL — `.5` is what Django's ORM writes for
        // `price * .5`, and every one of these was a parse error.
        assert_eq!(toks(".5"), vec![Tok::Float(0.5)]);
        assert_eq!(toks("5."), vec![Tok::Float(5.0)]);
        assert_eq!(toks(".5e2"), vec![Tok::Float(50.0)]);
        assert_eq!(toks("5.e2"), vec![Tok::Float(500.0)]);
        assert_eq!(toks(".5E-2"), vec![Tok::Float(0.005)]);
        // Hex integers.
        assert_eq!(toks("0x1F"), vec![Tok::Int(31)]);
        assert_eq!(toks("0X1f"), vec![Tok::Int(31)]);
        // A dot NOT followed by a digit is still a qualifier dot.
        assert_eq!(toks("a.b"), vec![Tok::Ident("a".into()), Tok::Dot, Tok::Ident("b".into())]);
        // A literal running straight into an identifier character is
        // malformed, as in sqlite — without this `1e` lexed as `1` plus the
        // identifier `e`, and `SELECT 1e` ANSWERED 1 (an alias) where sqlite
        // says "unrecognized token".
        for bad in ["1e", "1e+", "0b101", "1_000", "1x", "0xZZ"] {
            assert!(
                tokenize(bad).is_err(),
                "`{bad}` must not lex as a number followed by an identifier"
            );
        }
        // …but a SPACE still makes the alias.
        assert_eq!(toks("1 e"), vec![Tok::Int(1), Tok::Ident("e".into())]);
    }

    #[test]
    fn params() {
        assert_eq!(toks("$1 $65535 ?"), vec![
            Tok::DollarParam(0),
            Tok::DollarParam(65534),
            Tok::Question
        ]);
        assert!(tokenize("$0").is_err());
        assert!(tokenize("$65536").is_err());
        assert!(tokenize("$x").is_err());
    }

    #[test]
    fn operators() {
        assert_eq!(
            toks("= != <> < <= > >= + - * / %"),
            vec![
                Tok::Eq,
                Tok::Ne,
                Tok::Ne,
                Tok::Lt,
                Tok::Le,
                Tok::Gt,
                Tok::Ge,
                Tok::Plus,
                Tok::Minus,
                Tok::Star,
                Tok::Slash,
                Tok::Percent
            ]
        );
    }

    /// The whole `>`/`>=`/`->`/`->>`/`-` family in one line, in an order that
    /// would expose a maximal-munch mistake: a scanner that tried `->` before
    /// `->>` turns `a ->> p` into `a -> (> p)`, which still PARSES (a
    /// comparison as the right operand) and silently returns JSON text where
    /// SQL text was asked for.
    #[test]
    fn json_arrows_vs_greater_than() {
        assert_eq!(
            toks("> >= -> ->> - >-> ->>>"),
            vec![
                Tok::Gt,
                Tok::Ge,
                Tok::Arrow,
                Tok::ArrowText,
                Tok::Minus,
                Tok::Gt,
                Tok::Arrow,
                Tok::ArrowText,
                Tok::Gt,
            ]
        );
        // No whitespace anywhere: `a->>'$.b'` is how every ORM writes it.
        assert_eq!(
            toks("a->>'$.b'"),
            vec![
                Tok::Ident("a".into()),
                Tok::ArrowText,
                Tok::Str("$.b".into()),
            ]
        );
        assert_eq!(
            toks("a-1"),
            vec![Tok::Ident("a".into()), Tok::Minus, Tok::Int(1)]
        );
    }

    /// #1: all THREE quoted-identifier spellings sqlite accepts lex to the SAME
    /// token, so a quoted name is usable everywhere a bare one is. The one
    /// thing downstream MAY tell apart is whether the spelling was DOUBLE
    /// quotes — the bool exists solely for the DQS misfeature (#132), where an
    /// unbound double-quoted name in expression position becomes a string
    /// literal and the other two spellings never do.
    #[test]
    fn every_quoting_spelling_is_one_token() {
        let dq = |s: &str| Tok::QuotedIdent(s.into(), true);
        let q = |s: &str| Tok::QuotedIdent(s.into(), false);
        assert_eq!(toks("\"t\""), vec![dq("t")]);
        assert_eq!(toks("`t`"), vec![q("t")]);
        assert_eq!(toks("[t]"), vec![q("t")]);
        // A doubled delimiter escapes one, for the two that have an escape.
        assert_eq!(toks("\"a\"\"b\""), vec![dq("a\"b")]);
        assert_eq!(toks("`a``b`"), vec![q("a`b")]);
        // `[...]` has NO escape in sqlite: the first `]` closes it.
        assert_eq!(toks("[a b]"), vec![q("a b")]);
        // Keywords, spaces and dots all survive quoting.
        assert_eq!(toks("[select]"), vec![q("select")]);
        assert_eq!(toks("`from`"), vec![q("from")]);
        assert_eq!(toks("\"a.b\""), vec![dq("a.b")]);
        // Every spelling of the dotted path lexes to ident-dot-ident.
        for src in ["\"t\".\"c\"", "`t`.`c`", "[t].[c]", "\"t\".c", "t.\"c\""] {
            assert_eq!(
                toks(src).len(),
                3,
                "{src} should lex to <ident> . <ident>"
            );
            assert_eq!(toks(src)[1], Tok::Dot, "{src}");
        }
        // Empty and unterminated are errors for every spelling.
        for bad in ["\"\"", "``", "[]", "\"a", "`a", "[a"] {
            assert!(tokenize(bad).is_err(), "{bad} should not lex");
        }
    }

    /// Comments are skipped ANYWHERE, not only at the front of a statement:
    /// `-- …` to end of line, `/* … */` inline, and an unterminated `/*` to
    /// end of input (sqlite accepts that rather than erroring). A comment must
    /// leave NO token behind — `select 7 -- c` used to lex as `7 - (-c)`.
    #[test]
    fn comments_are_skipped_everywhere() {
        assert_eq!(toks("select 7 -- comment"), vec![Tok::Kw(Kw::Select), Tok::Int(7)]);
        assert_eq!(
            toks("select -- c\n 7"),
            vec![Tok::Kw(Kw::Select), Tok::Int(7)]
        );
        assert_eq!(toks("7 /* c */ + 1"), vec![Tok::Int(7), Tok::Plus, Tok::Int(1)]);
        assert_eq!(toks("7/*c*/+1"), vec![Tok::Int(7), Tok::Plus, Tok::Int(1)]);
        assert_eq!(toks("7 /* unterminated"), vec![Tok::Int(7)]);
        assert_eq!(toks("/* lead */ 7"), vec![Tok::Int(7)]);
        // A comment marker INSIDE a string literal is text, not a comment.
        assert_eq!(toks("'-- x'"), vec![Tok::Str("-- x".into())]);
        assert_eq!(toks("'/* x */'"), vec![Tok::Str("/* x */".into())]);
        // Subtraction still lexes: `a - -1` is two minuses, `a--1` is a comment.
        assert_eq!(
            toks("a - -1"),
            vec![Tok::Ident("a".into()), Tok::Minus, Tok::Minus, Tok::Int(1)]
        );
        assert_eq!(toks("a--1"), vec![Tok::Ident("a".into())]);
        // Division still lexes: `a / b` and `a /b`.
        assert_eq!(
            toks("a / b"),
            vec![Tok::Ident("a".into()), Tok::Slash, Tok::Ident("b".into())]
        );
    }

    /// `==` is sqlite's alias for `=`; an unquoted identifier may carry any
    /// byte >= 0x80 (sqlite does no Unicode classification).
    #[test]
    fn eq_alias_and_high_byte_identifiers() {
        assert_eq!(toks("a == 1"), vec![Tok::Ident("a".into()), Tok::Eq, Tok::Int(1)]);
        assert_eq!(toks("a = 1"), vec![Tok::Ident("a".into()), Tok::Eq, Tok::Int(1)]);
        assert_eq!(toks("café"), vec![Tok::Ident("café".into())]);
        assert_eq!(toks("ÿ"), vec![Tok::Ident("ÿ".into())]);
        assert_eq!(
            toks("select 1 as αβ"),
            vec![
                Tok::Kw(Kw::Select),
                Tok::Int(1),
                Tok::Kw(Kw::As),
                Tok::Ident("αβ".into())
            ]
        );
        // A high byte does not swallow following ASCII punctuation.
        assert_eq!(
            toks("é.b"),
            vec![Tok::Ident("é".into()), Tok::Dot, Tok::Ident("b".into())]
        );
    }

    #[test]
    fn error_positions() {
        match tokenize("a = 'oops") {
            Err(Error::Parse { pos, .. }) => assert_eq!(pos, 4),
            other => panic!("expected parse error, got {other:?}"),
        }
        match tokenize("a @ b") {
            Err(Error::Parse { pos, .. }) => assert_eq!(pos, 2),
            other => panic!("expected parse error, got {other:?}"),
        }
        match tokenize("x'0g'") {
            Err(Error::Parse { pos, .. }) => assert_eq!(pos, 3),
            other => panic!("expected parse error, got {other:?}"),
        }
        assert!(tokenize("x'0'").is_err()); // odd digit count
    }
}

#[cfg(test)]
mod pg_json_op_tests {
    use super::*;

    /// The containment / jsonb-path operators name themselves. `unexpected
    /// character `@`` is true and useless: it points at a punctuation mark
    /// where the reader wrote `@>`, and those two characters were 798 of the
    /// 908 refusals in that bucket across the corpus's forty heaviest files.
    #[test]
    fn a_containment_operator_names_itself_rather_than_its_first_character() {
        for (sql, op) in [
            ("SELECT a @> b", "@>"),
            ("SELECT a <@ b", "<@"),
            ("SELECT a #> '{x}'", "#>"),
            ("SELECT a #>> '{x}'", "#>>"),
            ("SELECT a @@ b", "@@"),
        ] {
            let e = tokenize(sql).unwrap_err().to_string();
            assert!(e.contains(op), "{sql}\n  got: {e}");
            assert!(e.contains("jsonb-path"), "{sql}\n  got: {e}");
        }
    }

    /// Longest first: `#>>` must not be reported as `#>`, or the refusal names
    /// an operator the reader did not write.
    #[test]
    fn the_longest_operator_wins() {
        assert_eq!(pg_json_operator("#>>'{a}'"), Some("#>>"));
        assert_eq!(pg_json_operator("#>'{a}'"), Some("#>"));
        assert_eq!(pg_json_operator("@>x"), Some("@>"));
        // `<@` is deliberately absent — see the comment on the table.
        assert_eq!(pg_json_operator("<@x"), None);
        assert_eq!(pg_json_operator("nothing"), None);
    }

    /// The operators mpedb DOES have are untouched — a refusal added next to a
    /// working path is how that path quietly stops working.
    #[test]
    fn the_json_arrows_that_work_still_tokenize() {
        for sql in [
            "SELECT a -> 'k'",
            "SELECT a ->> 'k'",
            "SELECT json_extract(a, '$.k')",
            "SELECT a > b",
            "SELECT a >> 2",
        ] {
            assert!(tokenize(sql).is_ok(), "{sql}");
        }
    }
}
