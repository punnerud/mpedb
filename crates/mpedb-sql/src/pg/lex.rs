//! The bytes PostgreSQL spells differently from sqlite.
//!
//! [`special`] is called ONCE per token, from the top of
//! [`crate::token::tokenize_dialect`]'s loop, and only when the dialect is
//! `Postgres`. It returns `Ok(None)` for every byte it does not own, so the
//! sqlite arms below it stay the authority for everything else — the corpus
//! cannot move because of a byte this file never claims.
//!
//! # What is actually contested
//!
//! Three bytes, and each one is a genuine collision rather than a missing
//! feature:
//!
//! - **`:`** — sqlite dialect gives it to `:sym:` custom operators
//!   (SQL-EXTENSIONS.md). PG wants `::` for casts. Both cannot have it, so PG
//!   dialect trades `:sym:` away; a lone `:` there is a NAMED refusal that says
//!   so, rather than a confusing "unterminated operator".
//! - **`$`** — sqlite dialect requires digits after it (`$1`). PG has `$1` too,
//!   but also `$$…$$` and `$tag$…$tag$` dollar quoting. `$n` is kept identical;
//!   only the non-digit case is new.
//! - **`!`** — sqlite has `!=`. PG adds `!~` and `!~*`. `!=` is left to the
//!   sqlite arm, so only the regex forms are claimed here.
//!
//! `~*` is claimed too, but that one is pure addition: sqlite's `~` is prefix
//! bitwise-NOT and has no two-byte form at all.

use crate::token::Tok;
use mpedb_types::{Error, Result};

fn perr(pos: usize, msg: impl Into<String>) -> Error {
    Error::Parse {
        pos,
        msg: msg.into(),
    }
}

/// Lex one PG-only token at `i`, or `Ok(None)` if this byte is not ours.
///
/// Returns `(token, index just past it)`.
pub(crate) fn special(b: &[u8], i: usize) -> Result<Option<(Tok, usize)>> {
    Ok(Some(match b[i] {
        b':' => {
            if b.get(i + 1) == Some(&b':') {
                (Tok::DoubleColon, i + 2)
            } else {
                // Deliberately a refusal and not a fallthrough. Falling through
                // would hand `:` to the sqlite arm, which would scan for a
                // closing colon and report "custom operator needs a closing
                // `:`" — an error about a feature this dialect does not have,
                // which is exactly the kind of message that costs an hour.
                return Err(perr(
                    i,
                    "`:` is not an operator under the postgres dialect — `::` casts, \
                     and `:sym:` custom operators are sqlite-dialect only",
                ));
            }
        }
        b'$' => match dollar_quote(b, i)? {
            // `$1` and friends: not ours, the sqlite arm lexes them identically.
            None => return Ok(None),
            Some(hit) => hit,
        },
        b'~' => {
            if b.get(i + 1) == Some(&b'*') {
                (Tok::TildeStar, i + 2)
            } else {
                // Plain `~` is Tok::Tilde in both dialects. Let the sqlite arm
                // produce it so there is one spelling of that token, not two.
                return Ok(None);
            }
        }
        b'!' => match (b.get(i + 1), b.get(i + 2)) {
            (Some(b'~'), Some(b'*')) => (Tok::NotTildeStar, i + 3),
            (Some(b'~'), _) => (Tok::NotTilde, i + 2),
            // `!=` — sqlite's, unchanged.
            _ => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

/// `$$body$$` / `$tag$body$tag$`, or `Ok(None)` when this `$` opens a `$n`
/// parameter instead.
///
/// The tag rule is PostgreSQL's: letters, digits and underscore, and it may not
/// START with a digit — which is precisely what keeps `$1` a parameter. An
/// unterminated body is an error naming the tag it was looking for, because the
/// alternative (scanning to end of input and reporting a parse error 400 bytes
/// later) is unreadable in a migration file.
fn dollar_quote(b: &[u8], i: usize) -> Result<Option<(Tok, usize)>> {
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    let tag = &b[i + 1..j];
    if tag.first().is_some_and(u8::is_ascii_digit) {
        // `$1` — a parameter, not a tag. Hand it back to the sqlite arm.
        return Ok(None);
    }
    if b.get(j) != Some(&b'$') {
        // `$` followed by neither a tag-then-`$` nor digits. The sqlite arm
        // reports "expected parameter number after `$`", which is the right
        // message here too.
        return Ok(None);
    }
    // The opening delimiter is `$tag$`, spanning i..=j.
    let open = &b[i..=j];
    let body_start = j + 1;
    let mut k = body_start;
    while k + open.len() <= b.len() {
        if &b[k..k + open.len()] == open {
            let body = std::str::from_utf8(&b[body_start..k])
                .map_err(|_| perr(i, "dollar-quoted string is not UTF-8"))?;
            return Ok(Some((Tok::Str(body.to_string()), k + open.len())));
        }
        k += 1;
    }
    let shown = String::from_utf8_lossy(open);
    Err(perr(
        i,
        format!("dollar-quoted string opened with `{shown}` is never closed"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{tokenize_dialect, Tok};
    use mpedb_types::Dialect;

    fn pg(sql: &str) -> Vec<Tok> {
        tokenize_dialect(sql, Dialect::Postgres)
            .unwrap()
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    fn lite(sql: &str) -> Result<Vec<Tok>> {
        Ok(tokenize_dialect(sql, Dialect::Sqlite)?
            .into_iter()
            .map(|t| t.tok)
            .collect())
    }

    #[test]
    fn double_colon_is_a_cast_under_pg_and_a_lex_error_under_sqlite() {
        assert_eq!(
            pg("a::text"),
            vec![
                Tok::Ident("a".into()),
                Tok::DoubleColon,
                Tok::Ident("text".into())
            ]
        );
        // The whole reason the dialect has to reach the lexer: under sqlite
        // this is an unterminated `:sym:`, and no binder could rescue it.
        assert!(lite("a::text").is_err());
    }

    #[test]
    fn a_lone_colon_under_pg_names_the_trade_it_lost() {
        let e = tokenize_dialect("a :plus: b", Dialect::Postgres).unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("sqlite-dialect only"), "{msg}");
        // …and the same text still lexes as a custom operator under sqlite.
        assert_eq!(
            lite("a :plus: b").unwrap(),
            vec![
                Tok::Ident("a".into()),
                Tok::CustomOp("plus".into()),
                Tok::Ident("b".into())
            ]
        );
    }

    #[test]
    fn dollar_params_survive_the_dollar_quoting_rule() {
        // The tag may not start with a digit, which is the ONLY thing keeping
        // `$1` a parameter. Pin it in both dialects.
        assert_eq!(pg("$1"), vec![Tok::DollarParam(0)]);
        assert_eq!(pg("$12"), vec![Tok::DollarParam(11)]);
        assert_eq!(lite("$1").unwrap(), vec![Tok::DollarParam(0)]);
    }

    #[test]
    fn dollar_quoting_lexes_bodies_that_contain_quotes_and_the_other_tag() {
        assert_eq!(pg("$$it's$$"), vec![Tok::Str("it's".into())]);
        assert_eq!(pg("$q$a $$ b$q$"), vec![Tok::Str("a $$ b".into())]);
        assert_eq!(pg("$$$$"), vec![Tok::Str(String::new())]);
        // Nesting is by TAG, not by depth — `$$` inside `$q$…$q$` is body text.
        assert_eq!(pg("$tag$x$tag$"), vec![Tok::Str("x".into())]);
    }

    #[test]
    fn an_unclosed_dollar_string_names_the_delimiter_it_wanted() {
        let e = tokenize_dialect("$q$ never closed", Dialect::Postgres).unwrap_err();
        assert!(format!("{e}").contains("`$q$`"), "{e}");
    }

    #[test]
    fn regex_operators_are_pg_only_and_do_not_steal_bang_eq() {
        assert_eq!(
            pg("a ~ b"),
            vec![Tok::Ident("a".into()), Tok::Tilde, Tok::Ident("b".into())]
        );
        assert_eq!(
            pg("a ~* b"),
            vec![
                Tok::Ident("a".into()),
                Tok::TildeStar,
                Tok::Ident("b".into())
            ]
        );
        assert_eq!(
            pg("a !~ b"),
            vec![
                Tok::Ident("a".into()),
                Tok::NotTilde,
                Tok::Ident("b".into())
            ]
        );
        assert_eq!(
            pg("a !~* b"),
            vec![
                Tok::Ident("a".into()),
                Tok::NotTildeStar,
                Tok::Ident("b".into())
            ]
        );
        // `!=` must still be `!=` — the `!` arm claims only the regex forms.
        assert_eq!(
            pg("a != b"),
            vec![Tok::Ident("a".into()), Tok::Ne, Tok::Ident("b".into())]
        );
    }

    #[test]
    fn every_byte_this_module_does_not_own_lexes_identically_in_both_dialects() {
        // The load-bearing property: PG dialect ADDS spellings, it does not
        // reinterpret existing ones. Anything else would be a silent corpus
        // mover the moment a session picked postgres.
        for sql in [
            "SELECT a, b FROM t WHERE c = 1 AND d <> 'x'",
            "SELECT a || b, a -> 'k', a ->> 'k' FROM t",
            "SELECT ~a, a & b, a | b, a << 2, a >> 2 FROM t",
            "SELECT x'00ff', 1.5, .5, -3 FROM t",
            "INSERT INTO t VALUES ($1, ?, 'it''s')",
            "SELECT \"quoted\", `back`, [brack] FROM t",
            "SELECT a != b, a <= b, a >= b FROM t",
        ] {
            assert_eq!(lite(sql).unwrap(), pg(sql), "dialects disagreed on: {sql}");
        }
    }
}
