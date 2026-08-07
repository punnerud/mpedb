//! The PL/pgSQL lexer.
//!
//! Hand-rolled rather than reused from `mpedb-sql`: that tokenizer serves the
//! SQL surface and knows `:sym:` operator macros, `$n` parameters and dialect
//! hooks, none of which mean the same thing here — and this crate deliberately
//! depends only on `mpedb-types`, which is what lets the whole PySpell layer be
//! database-free. Sharing the tokenizer would have traded that for a handful of
//! saved lines.
//!
//! What it must get right that a naive splitter would not:
//!
//! * **Dollar quoting.** `$$ … $$` and `$tag$ … $tag$` are how every real
//!   function body arrives, and the body contains apostrophes, semicolons and
//!   nested quotes. A tagged opener is closed ONLY by its exact tag, which is
//!   the whole reason the tagged form exists.
//! * **`$1`** is a positional parameter, and `$` immediately followed by a
//!   digit is therefore never the start of a dollar-quote.
//! * **`--` line comments and `/* */` block comments**, the latter NESTING, as
//!   PostgreSQL specifies (unlike C).
//! * **`''`** inside a single-quoted string is one apostrophe, not the end of
//!   the string followed by another one.

use mpedb_types::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// An unquoted identifier or keyword, FOLDED TO LOWER CASE — PostgreSQL
    /// folds unquoted names down, where the SQL standard folds up, and every
    /// comparison in the parser assumes the PostgreSQL rule.
    Word(String),
    /// A `"quoted identifier"`, case PRESERVED.
    Quoted(String),
    /// A string literal's CONTENT (quotes removed, `''` collapsed).
    Str(String),
    /// A dollar-quoted body's CONTENT and its tag (`""` for `$$`).
    Dollar { tag: String, body: String },
    Int(i64),
    Float(f64),
    /// `$1` — a positional parameter, 1-based as written.
    Param(u32),
    /// Punctuation and operators, as written: `( ) , ; . := = <> != < <= > >=
    /// + - * / % || .. :: [ ]`
    Punct(&'static str),
}

#[derive(Debug, Clone)]
pub struct Lexed {
    pub tok: Tok,
    /// Byte offset of the token's first character, for error locations.
    pub at: usize,
}

/// Multi-character operators, LONGEST FIRST — the order is the disambiguation:
/// `:=` must be tried before `::`… and before a bare `:` would be considered,
/// `<=` before `<`, `..` before `.`. Getting the order wrong turns `a := 1`
/// into `a : = 1` and reports a mystery.
const PUNCTS: &[&str] = &[
    ":=", "::", "<>", "!=", "<=", ">=", "||", "..", "(", ")", ",", ";", ".", "=", "<", ">", "+",
    "-", "*", "/", "%", "[", "]",
];

pub fn lex(src: &str) -> Result<Vec<Lexed>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        // whitespace
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `-- line comment`
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // `/* nesting block comment */` — PostgreSQL nests these, C does not,
        // and a body that comments out a chunk containing another comment is
        // exactly where the difference shows.
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Err(err(src, start, "unterminated /* block comment"));
            }
            continue;
        }
        let at = i;
        // 'string literal', with '' as one apostrophe
        if c == b'\'' {
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err(err(src, at, "unterminated string literal"));
                };
                if ch == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(ch as char);
                i += 1;
            }
            // Rebuild through the source slice so multi-byte UTF-8 survives:
            // pushing `ch as char` above is byte-wise and would mangle it.
            out.push(Lexed { tok: Tok::Str(unescape_single(&src[at + 1..i - 1])), at });
            continue;
        }
        // "quoted identifier"
        if c == b'"' {
            i += 1;
            let start = i;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i >= b.len() {
                return Err(err(src, at, "unterminated quoted identifier"));
            }
            out.push(Lexed { tok: Tok::Quoted(src[start..i].to_string()), at });
            i += 1;
            continue;
        }
        // `$1` parameter, or `$$`/`$tag$` dollar quote
        if c == b'$' {
            if b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let n: u32 = src[start..i]
                    .parse()
                    .map_err(|_| err(src, at, "parameter number out of range"))?;
                if n == 0 {
                    return Err(err(src, at, "parameters are numbered from $1"));
                }
                out.push(Lexed { tok: Tok::Param(n), at });
                continue;
            }
            let (tag, body, next) = dollar_quote(src, i)?;
            out.push(Lexed { tok: Tok::Dollar { tag, body }, at });
            i = next;
            continue;
        }
        // number
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                // `1..10` is a FOR range, not the number `1.` followed by
                // `.10`. One dot is a decimal point; a second ends the number.
                if b[i] == b'.' && b.get(i + 1) == Some(&b'.') {
                    break;
                }
                i += 1;
            }
            let text = &src[start..i];
            let tok = if text.contains('.') {
                Tok::Float(
                    text.parse()
                        .map_err(|_| err(src, at, format!("bad numeric literal `{text}`")))?,
                )
            } else {
                Tok::Int(
                    text.parse()
                        .map_err(|_| err(src, at, format!("integer literal `{text}` does not fit in 64 bits")))?,
                )
            };
            out.push(Lexed { tok, at });
            continue;
        }
        // identifier / keyword
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push(Lexed { tok: Tok::Word(src[start..i].to_ascii_lowercase()), at });
            continue;
        }
        // punctuation
        if let Some(p) = PUNCTS.iter().find(|p| src[i..].starts_with(**p)) {
            out.push(Lexed { tok: Tok::Punct(p), at });
            i += p.len();
            continue;
        }
        return Err(err(src, at, format!("unexpected character `{}`", c as char)));
    }
    Ok(out)
}

/// `''` → `'`, over a slice that is already the string's INTERIOR.
fn unescape_single(s: &str) -> String {
    s.replace("''", "'")
}

/// Read a dollar-quoted string starting at `i` (which points at `$`).
/// Returns (tag, body, index just past the closing delimiter).
///
/// The tag rule is PostgreSQL's: between the two `$` of the opener, an
/// optional identifier; the string ends at the FIRST occurrence of that exact
/// delimiter. `$$…$$` (empty tag) and `$body$…$body$` therefore nest, which is
/// how a function body can itself contain `$$`.
fn dollar_quote(src: &str, i: usize) -> Result<(String, String, usize)> {
    let b = src.as_bytes();
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if b.get(j) != Some(&b'$') {
        return Err(err(src, i, "`$` here starts neither a parameter nor a dollar-quote"));
    }
    let tag = src[i + 1..j].to_string();
    let delim = format!("${tag}$");
    let body_start = j + 1;
    let Some(rel) = src[body_start..].find(&delim) else {
        return Err(err(
            src,
            i,
            format!("unterminated dollar-quoted string (looking for `{delim}`)"),
        ));
    };
    let body_end = body_start + rel;
    Ok((tag, src[body_start..body_end].to_string(), body_end + delim.len()))
}

pub fn err(src: &str, at: usize, msg: impl AsRef<str>) -> Error {
    let (l, c) = crate::emit::line_col(src, at);
    crate::emit::cerr("plpgsql", l, c, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s).unwrap().into_iter().map(|l| l.tok).collect()
    }

    #[test]
    fn dollar_quote_beats_every_apostrophe_and_semicolon_inside_it() {
        let t = toks("AS $$ it's a body; with 'quotes' $$");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], Tok::Word("as".into()));
        let Tok::Dollar { tag, body } = &t[1] else { panic!("{:?}", t[1]) };
        assert_eq!(tag, "");
        assert_eq!(body, " it's a body; with 'quotes' ");
    }

    /// The whole point of the TAGGED form: an inner `$$` must not close it.
    #[test]
    fn a_tagged_dollar_quote_is_closed_only_by_its_own_tag() {
        let t = toks("$body$ inner $$ still inside $body$");
        let Tok::Dollar { tag, body } = &t[0] else { panic!("{t:?}") };
        assert_eq!(tag, "body");
        assert_eq!(body, " inner $$ still inside ");
    }

    #[test]
    fn a_dollar_followed_by_a_digit_is_a_parameter_not_a_quote() {
        assert_eq!(toks("$1 $12"), vec![Tok::Param(1), Tok::Param(12)]);
    }

    #[test]
    fn doubled_apostrophes_are_one_character_not_a_string_boundary() {
        assert_eq!(toks("'it''s'"), vec![Tok::Str("it's".into())]);
    }

    #[test]
    fn block_comments_nest_as_postgresql_specifies_and_c_does_not() {
        assert_eq!(toks("1 /* a /* b */ c */ 2"), vec![Tok::Int(1), Tok::Int(2)]);
        assert!(lex("/* unterminated").is_err());
    }

    /// `1..10` is a FOR range. Lexing it as the number `1.` would make the
    /// loop bounds unparseable and report the wrong thing.
    #[test]
    fn a_range_dot_dot_does_not_get_eaten_by_a_numeric_literal() {
        assert_eq!(
            toks("1..10"),
            vec![Tok::Int(1), Tok::Punct(".."), Tok::Int(10)]
        );
        assert_eq!(toks("1.5"), vec![Tok::Float(1.5)]);
    }

    #[test]
    fn assignment_is_not_two_tokens() {
        assert_eq!(
            toks("v := 1"),
            vec![Tok::Word("v".into()), Tok::Punct(":="), Tok::Int(1)]
        );
    }

    #[test]
    fn unquoted_names_fold_down_and_quoted_ones_do_not() {
        assert_eq!(
            toks("Foo \"Bar\""),
            vec![Tok::Word("foo".into()), Tok::Quoted("Bar".into())]
        );
    }

    #[test]
    fn a_multibyte_string_survives_the_interior_slice() {
        assert_eq!(toks("'ø…'"), vec![Tok::Str("ø…".into())]);
    }

    #[test]
    fn line_comments_end_at_the_newline_not_at_the_statement() {
        assert_eq!(toks("1 -- a ; b\n2"), vec![Tok::Int(1), Tok::Int(2)]);
    }
}
