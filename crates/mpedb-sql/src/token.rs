//! SQL tokenizer. Produces byte-offset-annotated tokens; keywords are
//! recognized case-insensitively, identifiers are case-sensitive.

use mpedb_types::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Bare identifier (case-sensitive, not a keyword).
    Ident(String),
    /// Double-quoted identifier (`""` escapes a quote).
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
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
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
    Is,
    Null,
    Like,
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
        "IS" => Kw::Is,
        "NULL" => Kw::Null,
        "LIKE" => Kw::Like,
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
            b'=' => {
                i += 1;
                Tok::Eq
            }
            b'+' => {
                i += 1;
                Tok::Plus
            }
            b'-' => {
                i += 1;
                Tok::Minus
            }
            b'*' => {
                i += 1;
                Tok::Star
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
            b'<' => match b.get(i + 1) {
                Some(b'=') => {
                    i += 2;
                    Tok::Le
                }
                Some(b'>') => {
                    i += 2;
                    Tok::Ne
                }
                _ => {
                    i += 1;
                    Tok::Lt
                }
            },
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    i += 2;
                    Tok::Ge
                } else {
                    i += 1;
                    Tok::Gt
                }
            }
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
            b'"' => {
                let (s, next) = lex_quoted_ident(sql, i)?;
                i = next;
                Tok::QuotedIdent(s)
            }
            b'0'..=b'9' => {
                let (tok, next) = lex_number(sql, i)?;
                i = next;
                tok
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                // Blob literal x'...' / X'...' (only when a quote follows).
                if (c == b'x' || c == b'X') && b.get(i + 1) == Some(&b'\'') {
                    let (blob, next) = lex_blob(sql, i)?;
                    i = next;
                    Tok::Blob(blob)
                } else {
                    let wstart = i;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
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

/// Lex a `"..."` identifier starting at the opening quote; `""` escapes.
fn lex_quoted_ident(sql: &str, start: usize) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut i = start + 1;
    let mut s = String::new();
    let mut seg = i;
    while i < b.len() {
        if b[i] == b'"' {
            if b.get(i + 1) == Some(&b'"') {
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
    fn keywords_case_insensitive_identifiers_not() {
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
