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

use mpedb_types::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Bare identifier (not a keyword). Spelled as written; comparisons
    /// against it fold ASCII case (`mpedb_types::ident_eq`).
    Ident(String),
    /// Quoted identifier. Three spellings, all sqlite's and all folded to this
    /// one token so the grammar never has to care which was written:
    /// `"a"` (`""` escapes), `` `a` `` (``` `` ``` escapes), `[a]` (no escape,
    /// closed by the first `]` — MS-Access/SQL-Server style, which sqlite
    /// accepts too).
    QuotedIdent(String),
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

/// A token plus the byte offset of its first character in the source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpTok {
    pub tok: Tok,
    pub pos: usize,
}

fn perr(pos: usize, msg: impl Into<String>) -> Error {
    Error::Parse {
        pos,
        msg: msg.into(),
    }
}

pub(crate) fn tokenize(sql: &str) -> Result<Vec<SpTok>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let c = b[i];
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
            // A standalone `.` (a `.5`-style float is lexed inside the digit
            // arm, so this only fires for a qualifier dot like `alias.table`).
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
                Tok::QuotedIdent(s)
            }
            // `[name]` — no escape mechanism (sqlite has none either): the
            // first `]` closes it, so a `]` cannot appear in a bracketed name.
            b'[' => {
                let (s, next) = lex_bracket_ident(sql, i)?;
                i = next;
                Tok::QuotedIdent(s)
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
                return Err(perr(start, format!("unexpected character `{ch}`")));
            }
        };
        out.push(SpTok { tok, pos: start });
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

/// Lex an integer or float literal starting at a digit.
fn lex_number(sql: &str, start: usize) -> Result<(Tok, usize)> {
    let b = sql.as_bytes();
    let mut i = start;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
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
    let text = &sql[start..i];
    if is_float {
        let v: f64 = text
            .parse()
            .map_err(|_| perr(start, "invalid float literal"))?;
        Ok((Tok::Float(v), i))
    } else {
        let v: i64 = text
            .parse()
            .map_err(|_| perr(start, "integer literal out of range"))?;
        Ok((Tok::Int(v), i))
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
        assert_eq!(toks("\"select\""), vec![Tok::QuotedIdent("select".into())]);
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
        // i64::MAX ok; one more overflows with an error, not a panic.
        assert_eq!(toks("9223372036854775807"), vec![Tok::Int(i64::MAX)]);
        assert!(matches!(
            tokenize("9223372036854775808"),
            Err(Error::Parse { pos: 0, .. })
        ));
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
    /// token, so a quoted name is usable everywhere a bare one is and nothing
    /// downstream can tell which spelling was written.
    #[test]
    fn every_quoting_spelling_is_one_token() {
        let q = |s: &str| Tok::QuotedIdent(s.into());
        assert_eq!(toks("\"t\""), vec![q("t")]);
        assert_eq!(toks("`t`"), vec![q("t")]);
        assert_eq!(toks("[t]"), vec![q("t")]);
        // A doubled delimiter escapes one, for the two that have an escape.
        assert_eq!(toks("\"a\"\"b\""), vec![q("a\"b")]);
        assert_eq!(toks("`a``b`"), vec![q("a`b")]);
        // `[...]` has NO escape in sqlite: the first `]` closes it.
        assert_eq!(toks("[a b]"), vec![q("a b")]);
        // Keywords, spaces and dots all survive quoting.
        assert_eq!(toks("[select]"), vec![q("select")]);
        assert_eq!(toks("`from`"), vec![q("from")]);
        assert_eq!(toks("\"a.b\""), vec![q("a.b")]);
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
