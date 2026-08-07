//! The `CREATE FUNCTION` header: name, parameters, return type, body.
//!
//! The unit this frontend accepts is the WHOLE statement, not the bare body
//! between the dollar quotes, and that is a migration decision rather than a
//! convenience. `pg_dump` emits `CREATE FUNCTION … AS $$ … $$ LANGUAGE
//! plpgsql;` as one statement, and it is the header — not the body — that
//! carries the function's name and its parameters' names. A frontend fed only
//! the body would have to be TOLD both, which means the migration path would
//! need a second channel alongside the dump, and the two could disagree.

use super::lex::{err, Lexed, Tok};
use mpedb_types::Result;

/// A parsed `CREATE FUNCTION` header plus the body text still to be compiled.
#[derive(Debug)]
pub struct Head {
    pub name: String,
    /// Parameter names in declaration order. An unnamed parameter (PostgreSQL
    /// allows `f(int)`) gets `$n`, so the body can still address it.
    pub params: Vec<String>,
    pub body: String,
    /// Byte offset of the body's first character within the ORIGINAL source,
    /// so an error inside the body reports a line in the file the user wrote
    /// rather than a line in a substring they never saw.
    pub body_at: usize,
}

/// PostgreSQL parameter modes. `IN` is the default and the only one that maps:
/// `OUT`/`INOUT`/`VARIADIC` change the CALLING CONVENTION, and a stored mpedb
/// function returns exactly one scalar.
const REFUSED_MODES: &[&str] = &["out", "inout", "variadic"];

pub fn parse_head(src: &str, t: &[Lexed]) -> Result<Head> {
    let mut i = 0usize;
    let want_word = |i: &mut usize, w: &str| -> Result<()> {
        match t.get(*i) {
            Some(Lexed { tok: Tok::Word(x), .. }) if x == w => {
                *i += 1;
                Ok(())
            }
            Some(l) => Err(err(src, l.at, format!("expected `{}`", w.to_uppercase()))),
            None => Err(err(src, src.len(), format!("expected `{}`", w.to_uppercase()))),
        }
    };
    want_word(&mut i, "create")?;
    if matches!(t.get(i), Some(Lexed { tok: Tok::Word(w), .. }) if w == "or") {
        i += 1;
        want_word(&mut i, "replace")?;
    }
    // `PROCEDURE` is refused separately from an unknown word: a PostgreSQL
    // PROCEDURE has no return value and is invoked with CALL, so accepting it
    // here would store something that could never be called the way it was
    // written.
    match t.get(i) {
        Some(Lexed { tok: Tok::Word(w), at }) if w == "procedure" => {
            return Err(err(
                src,
                *at,
                "CREATE PROCEDURE returns nothing and is invoked with CALL — mpedb stores \
                 it as a stored PROCEDURE (`mpedb proc`), not as a function; \
                 CREATE FUNCTION is what this compiles",
            ))
        }
        _ => want_word(&mut i, "function")?,
    }

    // name — optionally schema-qualified. mpedb has ONE namespace, so a
    // qualifier is dropped rather than refused: `public.f` and `f` are the same
    // function here, and refusing the qualified spelling would reject most
    // dumps for a distinction the engine does not make.
    let mut name = ident(src, t, &mut i, "function name")?;
    if matches!(t.get(i), Some(Lexed { tok: Tok::Punct("."), .. })) {
        i += 1;
        name = ident(src, t, &mut i, "function name after the schema qualifier")?;
    }

    // parameter list
    expect_punct(src, t, &mut i, "(")?;
    let mut params: Vec<String> = Vec::new();
    if !matches!(t.get(i), Some(Lexed { tok: Tok::Punct(")"), .. })) {
        loop {
            let n = params.len() + 1;
            params.push(one_param(src, t, &mut i, n)?);
            if matches!(t.get(i), Some(Lexed { tok: Tok::Punct(","), .. })) {
                i += 1;
                continue;
            }
            break;
        }
    }
    expect_punct(src, t, &mut i, ")")?;

    // RETURNS <type>  |  RETURNS SETOF <type>  |  RETURNS TABLE (…)
    //
    // Parsed for its REFUSALS — SETOF, TABLE(…) and trigger are each a
    // different feature — and then discarded, like every other type name here
    // (see `type_name`). Keeping it would be a fidelity claim the runtime does
    // not enforce: the IR's values carry their own type.
    if matches!(t.get(i), Some(Lexed { tok: Tok::Word(w), .. }) if w == "returns") {
        let at = t[i].at;
        i += 1;
        match t.get(i) {
            Some(Lexed { tok: Tok::Word(w), .. }) if w == "setof" => {
                return Err(err(
                    src,
                    at,
                    "RETURNS SETOF makes the function a ROW SOURCE, callable in FROM \
                     position — mpedb's stored functions return one scalar per call, and \
                     inventing rows for it would be a different feature (the table-function \
                     row source `generate_series` uses)",
                ))
            }
            Some(Lexed { tok: Tok::Word(w), .. }) if w == "table" => {
                return Err(err(
                    src,
                    at,
                    "RETURNS TABLE(…) makes the function a ROW SOURCE, callable in FROM \
                     position — see RETURNS SETOF; mpedb's stored functions return one \
                     scalar per call",
                ))
            }
            Some(Lexed { tok: Tok::Word(w), .. }) if w == "trigger" => {
                return Err(err(
                    src,
                    at,
                    "RETURNS trigger is a TRIGGER function: it reads NEW/OLD and its return \
                     value decides whether the row is written. mpedb has triggers, but they \
                     carry their body inline rather than calling out to a stored function \
                     (design/DESIGN-TRIGGERS.md) — port the body into the trigger",
                ))
            }
            _ => {}
        }
        let _ = type_name(src, t, &mut i)?;
    }

    // The tail — LANGUAGE / AS / IMMUTABLE / … — in any order, as PostgreSQL
    // allows. Only two of them carry information here; the rest are execution
    // hints for a planner that does not exist yet, and skipping them is honest
    // (they change no answer) where refusing them would reject working dumps.
    let mut body: Option<(String, usize)> = None;
    let mut language: Option<String> = None;
    while i < t.len() {
        match &t[i] {
            // The statement's own terminator ends the header; nothing after it
            // belongs to this function.
            Lexed { tok: Tok::Punct(";"), .. } => break,
            Lexed { tok: Tok::Word(w), .. } if w == "language" => {
                i += 1;
                language = Some(ident(src, t, &mut i, "language name")?.to_ascii_lowercase());
            }
            Lexed { tok: Tok::Word(w), .. } if w == "as" => {
                i += 1;
                match t.get(i) {
                    Some(Lexed { tok: Tok::Dollar { body: b, .. }, at }) => {
                        // +1 for the `$`, +tag, +1 for the closing `$` of the
                        // opener: the body starts just past the whole opener.
                        let open = match &t[i].tok {
                            Tok::Dollar { tag, .. } => tag.len() + 2,
                            _ => unreachable!(),
                        };
                        body = Some((b.clone(), at + open));
                        i += 1;
                    }
                    Some(Lexed { tok: Tok::Str(s), at }) => {
                        body = Some((s.clone(), *at + 1));
                        i += 1;
                    }
                    Some(l) => {
                        return Err(err(
                            src,
                            l.at,
                            "expected the function body after AS — a dollar-quoted string \
                             (`$$ … $$`) or a single-quoted one",
                        ))
                    }
                    None => return Err(err(src, src.len(), "expected the function body after AS")),
                }
            }
            // Every other tail word is a volatility/security/cost/parallel
            // hint. Consumed with any following token that belongs to it.
            Lexed { tok: Tok::Word(_), .. } => i += 1,
            Lexed { tok: Tok::Int(_) | Tok::Float(_) | Tok::Str(_), .. } => i += 1,
            l => return Err(err(src, l.at, "unexpected token in the CREATE FUNCTION tail")),
        }
    }

    let lang = language.unwrap_or_default();
    if lang != "plpgsql" {
        return Err(err(
            src,
            0,
            if lang.is_empty() {
                "CREATE FUNCTION without a LANGUAGE clause".to_string()
            } else {
                format!(
                    "LANGUAGE {lang} — this frontend compiles plpgsql; a `LANGUAGE sql` body \
                     is a different (and simpler) thing and is not wired up yet"
                )
            },
        ));
    }
    let Some((body, body_at)) = body else {
        return Err(err(src, 0, "CREATE FUNCTION without an AS body"));
    };
    Ok(Head { name, params, body, body_at })
}

/// One parameter: `[mode] [name] type [DEFAULT expr]`.
///
/// PostgreSQL's own grammar is ambiguous here — in `f(a int)` the first word is
/// a name, in `f(int)` it is a type — and it resolves the ambiguity the same
/// way this does: if exactly one word remains before the comma or the closing
/// paren, it is a TYPE and the parameter is unnamed.
fn one_param(src: &str, t: &[Lexed], i: &mut usize, n: usize) -> Result<String> {
    if let Some(Lexed { tok: Tok::Word(w), at }) = t.get(*i) {
        if REFUSED_MODES.contains(&w.as_str()) {
            return Err(err(
                src,
                *at,
                format!(
                    "parameter mode {} changes the calling convention — an mpedb stored \
                     function takes scalars in and returns exactly one scalar",
                    w.to_uppercase()
                ),
            ));
        }
        if w == "in" {
            *i += 1;
        }
    }
    let first = ident(src, t, i, "parameter name or type")?;
    // A type follows only when something other than `,` `)` or DEFAULT is next.
    let named = match t.get(*i) {
        Some(Lexed { tok: Tok::Punct(","), .. }) | Some(Lexed { tok: Tok::Punct(")"), .. }) => false,
        Some(Lexed { tok: Tok::Word(w), .. }) if w == "default" => false,
        None => false,
        _ => true,
    };
    let name = if named {
        let _ty = type_name(src, t, i)?;
        first
    } else {
        format!("${n}")
    };
    if matches!(t.get(*i), Some(Lexed { tok: Tok::Word(w), .. }) if w == "default") {
        let at = t[*i].at;
        return Err(err(
            src,
            at,
            "a parameter DEFAULT would let the function be called with fewer arguments than \
             it declares, and an mpedb stored function is resolved by (name, arity)",
        ));
    }
    Ok(name)
}

/// A type name, consumed and DISCARDED: `numeric(10,2)`, `character varying`,
/// `int[]`, `pg_catalog.text`.
///
/// Discarded on purpose. PL/pgSQL is dynamically typed at the level this
/// frontend compiles to — the IR's values are scalars decided per value — so
/// keeping the name would let it be reported back as a fidelity claim the
/// runtime does not enforce. What the type DOES decide (refusals) is decided in
/// `parse_head` before the name gets here.
fn type_name(src: &str, t: &[Lexed], i: &mut usize) -> Result<String> {
    let mut out = ident(src, t, i, "type name")?;
    // schema-qualified: `pg_catalog.text`
    if matches!(t.get(*i), Some(Lexed { tok: Tok::Punct("."), .. })) {
        *i += 1;
        out = ident(src, t, i, "type name after the schema qualifier")?;
    }
    // multi-word: `character varying`, `double precision`, `timestamp with
    // time zone`. Consumed while the next word cannot start something else.
    const CONTINUES: &[&str] = &[
        "varying", "precision", "with", "without", "time", "zone", "int", "integer", "unsigned",
    ];
    while matches!(t.get(*i), Some(Lexed { tok: Tok::Word(w), .. }) if CONTINUES.contains(&w.as_str()))
    {
        out.push(' ');
        out.push_str(match &t[*i].tok {
            Tok::Word(w) => w,
            _ => unreachable!(),
        });
        *i += 1;
    }
    // `(10, 2)` modifiers
    if matches!(t.get(*i), Some(Lexed { tok: Tok::Punct("("), .. })) {
        let mut depth = 0usize;
        loop {
            match t.get(*i) {
                Some(Lexed { tok: Tok::Punct("("), .. }) => depth += 1,
                Some(Lexed { tok: Tok::Punct(")"), .. }) => {
                    depth -= 1;
                    *i += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                Some(_) => {}
                None => return Err(err(src, src.len(), "unterminated type modifier")),
            }
            *i += 1;
        }
    }
    // `[]` array suffix
    while matches!(t.get(*i), Some(Lexed { tok: Tok::Punct("["), .. })) {
        *i += 1;
        if matches!(t.get(*i), Some(Lexed { tok: Tok::Punct("]"), .. })) {
            *i += 1;
        }
        out.push_str("[]");
    }
    Ok(out)
}

fn ident(src: &str, t: &[Lexed], i: &mut usize, what: &str) -> Result<String> {
    match t.get(*i) {
        Some(Lexed { tok: Tok::Word(w), .. }) => {
            *i += 1;
            Ok(w.clone())
        }
        Some(Lexed { tok: Tok::Quoted(w), .. }) => {
            *i += 1;
            Ok(w.clone())
        }
        Some(l) => Err(err(src, l.at, format!("expected {what}"))),
        None => Err(err(src, src.len(), format!("expected {what}"))),
    }
}

fn expect_punct(src: &str, t: &[Lexed], i: &mut usize, p: &str) -> Result<()> {
    match t.get(*i) {
        Some(Lexed { tok: Tok::Punct(x), .. }) if *x == p => {
            *i += 1;
            Ok(())
        }
        Some(l) => Err(err(src, l.at, format!("expected `{p}`"))),
        None => Err(err(src, src.len(), format!("expected `{p}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plpgsql::lex::lex;

    fn head(s: &str) -> Result<Head> {
        parse_head(s, &lex(s)?)
    }

    #[test]
    fn a_pg_dump_shaped_statement_parses_whole() {
        let h = head(
            "CREATE OR REPLACE FUNCTION public.add_tax(amount numeric, rate numeric)\n\
             RETURNS numeric AS $$ BEGIN RETURN amount * rate; END $$ LANGUAGE plpgsql IMMUTABLE;",
        )
        .unwrap();
        assert_eq!(h.name, "add_tax");
        assert_eq!(h.params, vec!["amount", "rate"]);
        assert!(h.body.contains("RETURN amount * rate"));
    }

    /// PostgreSQL's own ambiguity: one word before the comma is a TYPE, so the
    /// parameter is unnamed and the body addresses it as `$1`.
    #[test]
    fn an_unnamed_parameter_becomes_its_dollar_number() {
        let h = head("CREATE FUNCTION f(int, text) RETURNS int AS $$ BEGIN RETURN $1; END $$ LANGUAGE plpgsql")
            .unwrap();
        assert_eq!(h.params, vec!["$1", "$2"]);
    }

    #[test]
    fn multi_word_and_qualified_and_modified_types_are_consumed_whole() {
        let h = head(
            "CREATE FUNCTION f(a character varying(10), b pg_catalog.timestamp with time zone, \
             c int[]) RETURNS double precision AS $$ BEGIN RETURN 1; END $$ LANGUAGE plpgsql",
        )
        .unwrap();
        assert_eq!(h.params, vec!["a", "b", "c"]);
    }

    /// Each of these is a DIFFERENT feature, so each gets its own message —
    /// a single "unsupported" would send the reader looking in the wrong place.
    #[test]
    fn row_returning_and_trigger_forms_refuse_by_their_own_names() {
        for (sql, needle) in [
            ("CREATE FUNCTION f() RETURNS SETOF int AS $$ BEGIN END $$ LANGUAGE plpgsql", "SETOF"),
            ("CREATE FUNCTION f() RETURNS TABLE(a int) AS $$ BEGIN END $$ LANGUAGE plpgsql", "TABLE"),
            ("CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN END $$ LANGUAGE plpgsql", "trigger"),
            ("CREATE PROCEDURE p() AS $$ BEGIN END $$ LANGUAGE plpgsql", "PROCEDURE"),
            ("CREATE FUNCTION f(OUT a int) RETURNS int AS $$ BEGIN END $$ LANGUAGE plpgsql", "OUT"),
            ("CREATE FUNCTION f(a int DEFAULT 1) RETURNS int AS $$ BEGIN END $$ LANGUAGE plpgsql", "DEFAULT"),
        ] {
            let e = head(sql).unwrap_err().to_string();
            assert!(e.contains(needle), "{sql}\n  got: {e}");
        }
    }

    #[test]
    fn a_non_plpgsql_language_says_which_one_it_got() {
        let e = head("CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql")
            .unwrap_err()
            .to_string();
        assert!(e.contains("LANGUAGE sql"), "{e}");
    }

    /// The body offset must point into the ORIGINAL source, or every error
    /// inside the body reports a line number from a string the user never saw.
    #[test]
    fn the_body_offset_indexes_the_original_source() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $tag$BEGIN RETURN 1; END$tag$ LANGUAGE plpgsql";
        let h = head(sql).unwrap();
        assert_eq!(&sql[h.body_at..h.body_at + 5], "BEGIN");
    }
}
