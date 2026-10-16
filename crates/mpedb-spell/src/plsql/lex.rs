//! Oracle lexical units. Numeric tokens deliberately never pass through f64.

use mpedb_types::{Error, Result};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Identifier { name: String, quoted: bool },
    Number(String),
    String { value: String, national: bool },
    Bind(String),
    Symbol(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexed {
    pub token: Token,
    pub span: Range<usize>,
}

const SYMBOLS: &[&str] = &[
    ":=", "=>", "**", "..", "||", "<=", ">=", "<>", "!=", "~=", "^=", "<<", ">>", "+", "-", "*",
    "/", "=", "<", ">", "(", ")", "[", "]", ",", ";", ".", "%", "@",
];

pub(super) fn error(source: &str, at: usize, message: &str) -> Error {
    let prefix = &source[..at];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    Error::Unsupported(format!("PL/SQL at {line}:{column}: {message}"))
}

fn next(source: &str, at: usize) -> Option<char> {
    source.get(at..)?.chars().next()
}

fn identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '$' | '#')
}

/// SQL/PLSQL identifiers fold to uppercase; quoted identifiers retain case.
/// Comments are discarded. Oracle block comments do not nest.
pub fn lex(source: &str) -> Result<Vec<Lexed>> {
    let bytes = source.as_bytes();
    let mut at = 0;
    let mut tokens = Vec::new();
    while let Some(ch) = next(source, at) {
        if ch.is_ascii_whitespace() {
            at += ch.len_utf8();
            continue;
        }
        if source[at..].starts_with("--") {
            at = source[at..].find('\n').map_or(source.len(), |n| at + n);
            continue;
        }
        if source[at..].starts_with("/*") {
            let end = source[at + 2..]
                .find("*/")
                .ok_or_else(|| error(source, at, "unterminated block comment"))?;
            at += end + 4;
            continue;
        }
        let start = at;
        let rest = &bytes[at..];
        let national = matches!(rest.first(), Some(b'n' | b'N'));
        let prefix = usize::from(national);
        let alternative =
            matches!(rest.get(prefix), Some(b'q' | b'Q')) && rest.get(prefix + 1) == Some(&b'\'');
        let ordinary = rest.first() == Some(&b'\'') || (national && rest.get(1) == Some(&b'\''));
        let token = if alternative {
            at += prefix + 2;
            let open = next(source, at)
                .ok_or_else(|| error(source, start, "missing q-quote delimiter"))?;
            if open.is_whitespace() {
                return Err(error(source, at, "q-quote delimiter cannot be whitespace"));
            }
            let close = match open {
                '[' => ']',
                '(' => ')',
                '{' => '}',
                '<' => '>',
                other => other,
            };
            at += open.len_utf8();
            let content = at;
            let terminator = format!("{close}'");
            let end = source[at..]
                .find(&terminator)
                .ok_or_else(|| error(source, start, "unterminated q-quoted string"))?;
            at += end;
            let value = source[content..at].to_owned();
            at += terminator.len();
            Token::String { value, national }
        } else if ordinary || ch == '"' {
            let quoted_name = ch == '"';
            let quote = if quoted_name { '"' } else { '\'' };
            at += if national && !quoted_name { 2 } else { 1 };
            let mut value = String::new();
            loop {
                let current = next(source, at)
                    .ok_or_else(|| error(source, start, "unterminated quoted value"))?;
                at += current.len_utf8();
                if current == quote {
                    if !quoted_name && next(source, at) == Some(quote) {
                        value.push(quote);
                        at += 1;
                        continue;
                    }
                    break;
                }
                value.push(current);
            }
            if quoted_name {
                if value.is_empty() || value.contains('\0') {
                    return Err(error(
                        source,
                        start,
                        "empty or NUL-containing quoted identifier",
                    ));
                }
                Token::Identifier {
                    name: value,
                    quoted: true,
                }
            } else {
                Token::String { value, national }
            }
        } else if ch.is_ascii_digit()
            || (ch == '.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                at += 1;
            }
            if bytes.get(at) == Some(&b'.') && bytes.get(at + 1) != Some(&b'.') {
                at += 1;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
            }
            if matches!(bytes.get(at), Some(b'e' | b'E')) {
                at += 1;
                if matches!(bytes.get(at), Some(b'+' | b'-')) {
                    at += 1;
                }
                let digits = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if digits == at {
                    return Err(error(source, start, "numeric exponent requires digits"));
                }
            }
            if matches!(bytes.get(at), Some(b'f' | b'F' | b'd' | b'D')) {
                at += 1;
            }
            Token::Number(source[start..at].to_owned())
        } else if ch.is_alphabetic() {
            at += ch.len_utf8();
            while let Some(current) = next(source, at).filter(|&c| identifier_char(c)) {
                at += current.len_utf8();
            }
            Token::Identifier {
                name: source[start..at].to_uppercase(),
                quoted: false,
            }
        } else if ch == ':' && bytes.get(at + 1) != Some(&b'=') {
            at += 1;
            let content = at;
            let first =
                next(source, at).ok_or_else(|| error(source, start, "missing bind name"))?;
            if !(first.is_alphabetic() || first.is_ascii_digit()) {
                return Err(error(source, start, "invalid bind name"));
            }
            at += first.len_utf8();
            while let Some(current) = next(source, at).filter(|&c| identifier_char(c)) {
                at += current.len_utf8();
            }
            Token::Bind(source[content..at].to_uppercase())
        } else if let Some(&symbol) = SYMBOLS
            .iter()
            .find(|symbol| source[at..].starts_with(**symbol))
        {
            at += symbol.len();
            Token::Symbol(symbol)
        } else {
            return Err(error(source, at, &format!("unrecognized character {ch:?}")));
        };
        tokens.push(Lexed {
            token,
            span: start..at,
        });
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests;
