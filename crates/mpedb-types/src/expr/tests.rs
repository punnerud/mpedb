use super::*;

fn prog(instrs: Vec<Instr>, consts: Vec<Value>) -> ExprProgram {
    ExprProgram::new(instrs, consts).unwrap()
}

/// A trivial [`HostFns`] for the `HostCall` tests: `plus1(x)=x+1`, everything
/// else is an unknown-function error (the defensive path).
struct TestHost;
impl HostFns for TestHost {
    fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        match (name, args) {
            ("plus1", [Value::Int(x)]) => Ok(Value::Int(x + 1)),
            _ => Err(Error::Internal(format!("no host fn {name}/{}", args.len()))),
        }
    }
}

#[test]
fn host_call_dispatches_codec_roundtrips_and_needs_a_resolver() {
    // `plus1($1)` — name in the const pool, one stack argument.
    let p = prog(
        vec![
            Instr::PushParam(0),
            Instr::HostCall(0, 1),
        ],
        vec![Value::Text("plus1".into())],
    );
    // With a resolver in scope the closure runs.
    let host = TestHost;
    assert_eq!(
        p.eval_host(&[], &[Value::Int(41)], Some(&host)).unwrap(),
        Value::Int(42)
    );
    // With NO resolver the opcode refuses — and refuses as a documented SCOPE
    // limit (`Unsupported`, naming the function), never as `Internal`, which
    // renders "internal error (bug in mpedb)" for what is a known boundary.
    let e = p.eval(&[], &[Value::Int(1)]).unwrap_err();
    assert!(
        matches!(&e, Error::Unsupported(m) if m.contains("plus1") && m.contains("not in scope")),
        "expected a clean out-of-scope refusal, got {e:?}"
    );
    // An unregistered name/arity surfaces the resolver's error, not a panic.
    let q = prog(
        vec![Instr::PushParam(0), Instr::HostCall(0, 1)],
        vec![Value::Text("nope".into())],
    );
    assert!(q.eval_host(&[], &[Value::Int(1)], Some(&host)).is_err());

    // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
    // A name index past the const pool is Corrupt at construction, never a
    // panic at eval.
    assert!(matches!(
        ExprProgram::new(vec![Instr::PushParam(0), Instr::HostCall(9, 1)], vec![Value::Text("x".into())]),
        Err(Error::Corrupt(_))
    ));
    // A non-text name constant is Corrupt at eval (hostile-blob defense).
    let bad = prog(vec![Instr::PushParam(0), Instr::HostCall(0, 1)], vec![Value::Int(7)]);
    assert!(matches!(
        bad.eval_host(&[], &[Value::Int(1)], Some(&host)),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn check_constraint_age_range() {
    // age >= 0 AND age < 200
    let p = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::Ge,
            Instr::PushCol(0),
            Instr::PushConst(1),
            Instr::Lt,
            Instr::And,
        ],
        vec![Value::Int(0), Value::Int(200)],
    );
    let mut stack = Vec::new();
    assert!(p.eval_filter(&mut stack, &[Value::Int(42)], &[]).unwrap());
    assert!(!p.eval_filter(&mut stack, &[Value::Int(-1)], &[]).unwrap());
    assert!(!p.eval_filter(&mut stack, &[Value::Int(200)], &[]).unwrap());
    // NULL age: predicate is NULL -> does not pass
    assert!(!p.eval_filter(&mut stack, &[Value::Null], &[]).unwrap());
}

#[test]
fn three_valued_logic() {
    // NULL OR true = true ; NULL AND true = NULL ; NOT NULL = NULL
    let or = prog(
        vec![Instr::PushCol(0), Instr::PushConst(0), Instr::Or],
        vec![Value::Bool(true)],
    );
    assert_eq!(or.eval(&[Value::Null], &[]).unwrap(), Value::Bool(true));
    let and = prog(
        vec![Instr::PushCol(0), Instr::PushConst(0), Instr::And],
        vec![Value::Bool(true)],
    );
    assert_eq!(and.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    let not = prog(vec![Instr::PushCol(0), Instr::Not], vec![]);
    assert_eq!(not.eval(&[Value::Null], &[]).unwrap(), Value::Null);
}

#[test]
fn params_and_arith() {
    // $1 + 10 = col0
    let p = prog(
        vec![
            Instr::PushParam(0),
            Instr::PushConst(0),
            Instr::Add,
            Instr::PushCol(0),
            Instr::Eq,
        ],
        vec![Value::Int(10)],
    );
    assert_eq!(
        p.eval(&[Value::Int(52)], &[Value::Int(42)]).unwrap(),
        Value::Bool(true)
    );
    assert!(matches!(
        p.eval(&[Value::Int(52)], &[]),
        Err(Error::WrongParamCount { .. })
    ));
    // Division by zero yields NULL (sqlite semantics), not an error.
    assert_eq!(
        prog(
            vec![Instr::PushConst(0), Instr::PushConst(0), Instr::Div],
            vec![Value::Int(0)]
        )
        .eval(&[], &[])
        .unwrap(),
        Value::Null
    );
    // Overflow still raises: i64::MIN / -1.
    assert!(matches!(
        prog(
            vec![Instr::PushConst(0), Instr::PushConst(1), Instr::Div],
            vec![Value::Int(i64::MIN), Value::Int(-1)]
        )
        .eval(&[], &[]),
        Err(Error::ArithmeticOverflow)
    ));
}

#[test]
fn like_patterns() {
    assert!(like_match("he%o", "hello", None));
    assert!(like_match("%", "", None));
    assert!(like_match("h_llo", "hallo", None));
    assert!(!like_match("h_llo", "hllo", None));
    assert!(like_match("%abc", "xxabc", None));
    assert!(!like_match("abc%", "xabc", None));
    assert!(like_match("a%b%c", "a123b456c", None));
    // literal '%' in the subject must not consume the wildcard
    assert!(like_match("%", "%%", None));
    assert!(like_match("a%", "a%c", None));
    assert!(like_match("%c", "a%c", None));
    assert!(like_match("a%c", "a%c", None));
    // Case-insensitive for ASCII (sqlite default): pattern and subject fold.
    assert!(like_match("ab%", "Ab2", None));
    assert!(like_match("AB%", "abc", None));
    assert!(like_match("h_LLO", "Hello", None));
    assert!(like_match("ABC", "abc", None));
    // Non-ASCII is NOT folded (matches sqlite / NOCASE).
    assert!(!like_match("héllo", "HÉLLO", None));
}

/// `LIKE … ESCAPE c`. Every expectation here was READ OFF `sqlite3` 3.45.1 —
/// see the doc comment on `compile_pattern` for the rules they pin.
#[test]
fn like_escape_matches_sqlite() {
    let e = Some('\\');
    // An escaped `%`/`_` is a literal one.
    assert!(like_match("100\\%", "100%", e));
    assert!(!like_match("100\\%", "100x", e));
    assert!(like_match("a\\_b", "a_b", e));
    assert!(!like_match("a\\_b", "axb", e));
    // The escape before a character that is NEITHER wildcard nor itself: sqlite
    // makes it a plain literal (`'ab' LIKE 'a\b' ESCAPE '\'` is TRUE).
    assert!(like_match("a\\b", "ab", e));
    assert!(!like_match("a\\b", "a\\b", e));
    // A doubled escape is a literal escape character.
    assert!(like_match("a\\\\b", "a\\b", e));
    // A DANGLING escape at the end of the pattern never matches — not the empty
    // subject, not the pattern's own text, not even under a preceding `%`.
    assert!(!like_match("ab\\", "ab", e));
    assert!(!like_match("ab\\", "ab\\", e));
    assert!(!like_match("\\", "", e));
    assert!(!like_match("%a\\", "ab", e));
    // Unescaped wildcards keep working alongside an escape character.
    assert!(like_match("a%c", "abbbc", e));
    // sqlite's `likeFunc`: an escape that IS `%` clears matchAll, one that is
    // `_` clears matchOne — the wildcard stops being a wildcard entirely.
    assert!(like_match("a%%b", "a%b", Some('%')));
    assert!(!like_match("a%%b", "axb", Some('%')));
    assert!(!like_match("a%b", "axb", Some('%')));
    assert!(like_match("a__b", "a_b", Some('_')));
    assert!(!like_match("a_%b", "anythingb", Some('_')));
    assert!(like_match("__", "_", Some('_')));
    // An escaped literal still folds case under the sqlite dialect …
    assert!(like_match("a\\b", "AB", e));
    assert!(like_match("\\a\\B", "aB", e));
    // … and does NOT under the PostgreSQL (case-sensitive) dialect.
    assert!(!like_match_cs("a\\b", "AB", e));
    assert!(like_match_cs("a\\b", "ab", e));
    assert!(like_match_cs("100\\%", "100%", e));
    assert!(!like_match_cs("ab\\", "ab", e));
    // A multi-BYTE but single-CHARACTER escape is legal (sqlite reads one UTF-8
    // character, not one byte).
    assert!(!like_match("aéb", "aéb", Some('é')));
    assert!(like_match("aéb", "ab", Some('é')));
    // Django's exact shape: `%foo%` under ESCAPE '\'.
    assert!(like_match("%foo%", "xxfooyy", e));
    assert!(like_match("%\\%foo%", "xx%fooyy", e));
    assert!(!like_match("%\\%foo%", "xxfooyy", e));
}

/// The pattern memo is keyed on `(pattern, escape)` VALUE, not on any notion
/// of "the statement" — alternating patterns and escapes through the same
/// thread must never answer from a stale compiled form. (This is what makes
/// the one-slot memo safe for the dyn-pattern opcodes, where the pattern can
/// change per row.)
#[test]
fn like_memo_alternating_patterns_and_escapes() {
    for _ in 0..3 {
        assert!(like_match("a%", "abc", None));
        assert!(!like_match("b%", "abc", None));
        // Same pattern, different ESCAPE: must recompile — under ESCAPE '%'
        // the `%` stops being a wildcard.
        assert!(like_match("a%", "abc", None));
        assert!(!like_match("a%", "abc", Some('%')));
        // Dangling-escape failure is cached per (pattern, esc) too, and must
        // not leak onto the escape-less reading of the same bytes.
        assert!(!like_match("ab\\", "ab", Some('\\')));
        assert!(like_match("ab\\", "ab\\", None));
        // ci is NOT part of the key: the same compiled form serves both
        // dialects with different literal comparison.
        assert!(like_match("AB%", "abc", None));
        assert!(!like_match_cs("AB%", "abc", None));
    }
}

/// The dyn-pattern LIKE/GLOB opcodes (#74 item 3, LIKE half): the const
/// forms' matcher and NULL rules with the pattern popped off the stack.
/// Operands arrive as text or NULL — the binder CAST-bridges coercible
/// non-text operands exactly as it always did for subjects — so the raw
/// non-text arms are corrupt-plan errors, asserted as such.
#[test]
fn like_dyn_opcodes_match_null_and_type_rules() {
    let t = |s: &str| Value::Text(s.into());
    let like2 = |a: Value, p: Value| {
        prog(vec![Instr::PushParam(0), Instr::PushParam(1), Instr::LikeDyn], vec![])
            .eval(&[], &[a, p])
    };

    // Plain dynamic matching, ASCII case folded (sqlite dialect).
    assert_eq!(like2(t("hello"), t("he%o")).unwrap(), Value::Bool(true));
    assert_eq!(like2(t("HELLO"), t("he%o")).unwrap(), Value::Bool(true));
    assert_eq!(like2(t("hello"), t("x%")).unwrap(), Value::Bool(false));
    // NULL propagates from either side.
    assert_eq!(like2(Value::Null, t("a%")).unwrap(), Value::Null);
    assert_eq!(like2(t("a"), Value::Null).unwrap(), Value::Null);
    assert_eq!(like2(Value::Null, Value::Null).unwrap(), Value::Null);
    // A raw non-text operand is a corrupt plan, not a coercion opportunity.
    assert!(like2(t("12"), Value::Int(12)).is_err());
    assert!(like2(t("A"), Value::Blob(vec![0x41])).is_err());

    // The escape form takes its one-character escape from the const pool;
    // Django's exact shape (`s LIKE ? ESCAPE '\'`) with the pattern bound.
    let esc = |a: Value, p: Value| {
        prog(
            vec![Instr::PushParam(0), Instr::PushParam(1), Instr::LikeDynEsc(0)],
            vec![Value::Text("\\".into())],
        )
        .eval(&[], &[a, p])
        .unwrap()
    };
    assert_eq!(esc(t("100%"), t("100\\%")), Value::Bool(true));
    assert_eq!(esc(t("100x"), t("100\\%")), Value::Bool(false));
    // A DANGLING escape arriving AT RUNTIME matches nothing — sqlite's
    // NOMATCH, a legal answer, not an error (unlike REGEXP's W3 rule).
    assert_eq!(esc(t("ab"), t("ab\\")), Value::Bool(false));
    assert_eq!(esc(t(""), t("\\")), Value::Bool(false));

    // Case-SENSITIVE (PG dialect) twins.
    let cs = |a: Value, p: Value| {
        prog(vec![Instr::PushParam(0), Instr::PushParam(1), Instr::LikeCsDyn], vec![])
            .eval(&[], &[a, p])
    };
    assert_eq!(cs(t("AB"), t("a%")).unwrap(), Value::Bool(false));
    assert_eq!(cs(t("ab"), t("a%")).unwrap(), Value::Bool(true));
    assert_eq!(cs(t("ab"), Value::Null).unwrap(), Value::Null);
    assert!(cs(t("12"), Value::Int(12)).is_err());
    let cse = prog(
        vec![Instr::PushParam(0), Instr::PushParam(1), Instr::LikeCsDynEsc(0)],
        vec![Value::Text("\\".into())],
    );
    assert_eq!(cse.eval(&[], &[t("100%"), t("100\\%")]).unwrap(), Value::Bool(true));
    assert_eq!(cse.eval(&[], &[t("100%"), t("100\\%25")]).unwrap(), Value::Bool(false));

    // GLOB dyn: glob wildcards, always case-sensitive, same NULL/type rules.
    let glob2 = |a: Value, p: Value| {
        prog(vec![Instr::PushParam(0), Instr::PushParam(1), Instr::GlobDyn], vec![])
            .eval(&[], &[a, p])
    };
    assert_eq!(glob2(t("abc"), t("a*")).unwrap(), Value::Bool(true));
    assert_eq!(glob2(t("ABC"), t("a*")).unwrap(), Value::Bool(false));
    assert_eq!(glob2(t("abc"), t("a%")).unwrap(), Value::Bool(false)); // `%` is no GLOB wildcard
    assert_eq!(glob2(Value::Null, t("a")).unwrap(), Value::Null);
    assert!(glob2(t("1"), Value::Int(1)).is_err());

    // codec: all five opcodes round-trip, and truncation at every offset is
    // Corrupt, never a panic (repo rule).
    let p5 = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::LikeDyn,
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::LikeCsDyn,
            Instr::And,
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::LikeDynEsc(0),
            Instr::And,
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::LikeCsDynEsc(0),
            Instr::And,
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::GlobDyn,
            Instr::And,
        ],
        vec![Value::Text("\\".into())],
    );
    let mut buf = Vec::new();
    p5.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p5);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }

    // The dyn-escape const is proved a one-character text at validation, so a
    // hand-built plan with a bad escape slot is Corrupt, exactly like the
    // const-pattern escape forms.
    for bad in [Value::Text("".into()), Value::Text("ab".into()), Value::Int(5)] {
        assert!(ExprProgram::new(
            vec![Instr::PushCol(0), Instr::PushCol(1), Instr::LikeDynEsc(0)],
            vec![bad],
        )
        .is_err());
    }
}

#[test]
fn glob_patterns() {
    // `*` = any run (incl. empty); `?` = exactly one char.
    assert!(glob_match("a*", "abc"));
    assert!(glob_match("a*", "a"));
    assert!(glob_match("*c", "abc"));
    assert!(glob_match("a*c", "abxyzc"));
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("a?c", "ac")); // `?` needs a char
    assert!(!glob_match("a?c", "abbc"));
    assert!(glob_match("*", ""));
    assert!(glob_match("a*b*c", "axxbyyc"));

    // Case-SENSITIVE — the property that distinguishes GLOB from a
    // case-folding matcher (and the point sqlite makes about GLOB vs LIKE).
    assert!(!glob_match("A*", "abc"));
    assert!(glob_match("A*", "Abc"));
    assert!(!glob_match("abc", "ABC"));

    // Character classes: sets, ranges, negation.
    assert!(glob_match("[abc]", "b"));
    assert!(!glob_match("[abc]", "d"));
    assert!(glob_match("[a-c]x", "bx"));
    assert!(!glob_match("[a-c]x", "dx"));
    assert!(glob_match("[^a-c]x", "dx"));
    assert!(!glob_match("[^a-c]x", "bx"));
    // Class is case-sensitive too: `[a-c]` excludes uppercase.
    assert!(!glob_match("[a-c]", "B"));
    // A leading `]` is a literal set member.
    assert!(glob_match("[]x]", "]"));
    assert!(glob_match("[]x]", "x"));
    // `-` as first/last member is literal, not a range.
    assert!(glob_match("[-a]", "-"));
    assert!(glob_match("[a-]", "-"));
    // A `*`/`?` inside a class is a literal char, not a wildcard.
    assert!(glob_match("[*?]", "*"));
    assert!(glob_match("[*?]", "?"));
    assert!(!glob_match("[*?]", "a"));
    // An unterminated set fails the whole match (sqlite NOMATCH).
    assert!(!glob_match("[abc", "a"));

    // Literal `*`/`?` in the pattern are ALWAYS wildcards (no escape), so a
    // literal one must be matched via a class — the same rule sqlite has.
    assert!(glob_match("a[*]b", "a*b"));
    assert!(!glob_match("a[*]b", "axb"));
}

#[test]
fn regexp_patterns() {
    // Unanchored: a pattern matches ANY substring unless `^`/`$` pin an end.
    assert!(regexp_match("abc", "xxabcyy").unwrap());
    assert!(regexp_match("^abc", "abcyy").unwrap());
    assert!(!regexp_match("^abc", "xabc").unwrap());
    assert!(regexp_match("abc$", "xxabc").unwrap());
    assert!(!regexp_match("abc$", "abcx").unwrap());
    assert!(regexp_match("^$", "").unwrap());
    assert!(regexp_match("", "anything").unwrap()); // empty pattern matches everywhere
    assert!(!regexp_match("^abc$", "abcd").unwrap());

    // `.` — any single char, INCLUDING newline (sqlite's `.`).
    assert!(regexp_match("a.c", "abc").unwrap());
    assert!(regexp_match("^a.c$", "a\nc").unwrap());
    assert!(!regexp_match("^a.c$", "ac").unwrap());

    // Quantifiers `* + ?`.
    assert!(regexp_match("^ab*c$", "ac").unwrap());
    assert!(regexp_match("^ab*c$", "abbbc").unwrap());
    assert!(!regexp_match("^ab+c$", "ac").unwrap());
    assert!(regexp_match("^ab+c$", "abc").unwrap());
    assert!(regexp_match("^ab?c$", "ac").unwrap());
    assert!(regexp_match("^ab?c$", "abc").unwrap());
    assert!(!regexp_match("^ab?c$", "abbc").unwrap());

    // Counted repetition `{p}` / `{p,}` / `{p,q}` / `{,q}`.
    assert!(regexp_match("^a{3}$", "aaa").unwrap());
    assert!(!regexp_match("^a{3}$", "aa").unwrap());
    assert!(regexp_match("^a{2,}$", "aaaa").unwrap());
    assert!(!regexp_match("^a{2,}$", "a").unwrap());
    assert!(regexp_match("^a{2,4}$", "aaa").unwrap());
    assert!(!regexp_match("^a{2,4}$", "aaaaa").unwrap());
    assert!(regexp_match("^a{,3}$", "aa").unwrap()); // `{,3}` == `{0,3}`

    // Character classes: set, range, negation, and sqlite's literal-`]`/`-`
    // rules. A `[a-]` (dash consumed as a range upper bound past `]`) is
    // malformed → non-matching, like sqlite.
    assert!(regexp_match("^[abc]$", "b").unwrap());
    assert!(!regexp_match("^[abc]$", "d").unwrap());
    assert!(regexp_match("^[a-c]+$", "abcabc").unwrap());
    assert!(!regexp_match("^[a-c]+$", "abd").unwrap());
    assert!(regexp_match("^[^x]$", "y").unwrap());
    assert!(!regexp_match("^[^x]$", "x").unwrap());
    assert!(regexp_match("^[]x]$", "]").unwrap()); // leading `]` is literal
    assert!(regexp_match("^[-a]$", "-").unwrap()); // leading `-` is literal
    assert!(regexp_match("[a-]", "-").is_err()); // malformed → named error (sqlite errors too)

    // Alternation and grouping.
    assert!(regexp_match("^(cat|dog)$", "dog").unwrap());
    assert!(!regexp_match("^(cat|dog)$", "cow").unwrap());
    assert!(regexp_match("^(ab)+$", "ababab").unwrap());
    assert!(!regexp_match("^(ab)+$", "aba").unwrap());
    assert!(regexp_match("^a(b|c)d$", "acd").unwrap());

    // Backslash escapes: metacharacters, C escapes, Perl classes, `\b`.
    assert!(regexp_match("^a\\.c$", "a.c").unwrap());
    assert!(!regexp_match("^a\\.c$", "axc").unwrap());
    assert!(regexp_match("^a\\*c$", "a*c").unwrap());
    assert!(regexp_match("^a\\\\b$", "a\\b").unwrap());
    assert!(regexp_match("^\\t$", "\t").unwrap());
    assert!(regexp_match("^\\d+$", "2026").unwrap());
    assert!(!regexp_match("^\\d+$", "20a6").unwrap());
    assert!(regexp_match("^\\w+$", "a_1").unwrap());
    assert!(regexp_match("\\bbar", "foo bar").unwrap());
    assert!(!regexp_match("\\bbar", "foobar").unwrap());
    assert!(regexp_match("^\\D$", "x").unwrap());
    assert!(regexp_match("\\u0041", "A").unwrap()); // \uXXXX code point
    assert!(regexp_match("\\x41", "A").unwrap()); // \xXX code point

    // Case-SENSITIVE, like GLOB.
    assert!(!regexp_match("abc", "ABC").unwrap());
    assert!(regexp_match("ABC", "ABC").unwrap());

    // Malformed patterns never panic; they are a NAMED error, as sqlite's own
    // regexp extension raises. ("Match nothing" was wrong answer W3: a consumer
    // whose registered regexp() UDF speaks a richer dialect got silent empty
    // results — Django's __iregex prepends (?i) to every pattern.)
    assert!(regexp_match("(ab", "ab").is_err()); // unmatched '('
    assert!(regexp_match("a)b", "a)b").is_err()); // unmatched ')'
    assert!(regexp_match("[abc", "a").is_err()); // unterminated class
    assert!(regexp_match("*a", "a").is_err()); // quantifier without operand
    assert!(regexp_match("a{3,1}", "aa").is_err()); // n < m
    assert!(regexp_match("a{0}", "").is_err()); // both zero
    assert!(regexp_match("\\y", "y").is_err()); // unknown escape
    assert!(regexp_match("(?i)fo+", "FOO").is_err()); // the W3 shape itself

    // A count far above the program cap is refused (bounded, no hang) — even
    // over an empty body, where a naive expander would spin.
    assert!(regexp_match("a{999999999}", "aa").is_err());
    assert!(regexp_match("(){999999999}", "").is_err());
}

#[test]
fn glob_program_null_and_type_rules() {
    // `col0 GLOB 'a*'` — NULL operand yields NULL, exactly like LIKE.
    let p = prog(vec![Instr::PushCol(0), Instr::Glob(0)], vec![Value::Text("a*".into())]);
    assert_eq!(p.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(p.eval(&[Value::Text("xbc".into())], &[]).unwrap(), Value::Bool(false));
    assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    // A non-text operand is a type error, not a silent non-match — the binder
    // CAST-bridges coercible operands (sqlite3_value_text's conversion), so
    // one reaching the opcode raw is a corrupt plan.
    assert!(matches!(
        p.eval(&[Value::Int(1)], &[]),
        Err(Error::TypeMismatch(_))
    ));

    // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}

#[test]
fn regexp_program_null_and_type_rules() {
    // `col0 REGEXP '^a.c$'` — NULL operand yields NULL, exactly like GLOB/LIKE.
    let p = prog(
        vec![Instr::PushCol(0), Instr::Regexp(0)],
        vec![Value::Text("^a.c$".into())],
    );
    assert_eq!(p.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(p.eval(&[Value::Text("abbc".into())], &[]).unwrap(), Value::Bool(false));
    assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    // A non-text operand is a type error, not a silent non-match.
    assert!(matches!(
        p.eval(&[Value::Int(1)], &[]),
        Err(Error::TypeMismatch(_))
    ));

    // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}

#[test]
fn collated_compare_and_in_semantics_and_codec() {
    use crate::value::Collation;
    // `col0 = 'abc' COLLATE NOCASE` — ASCII-case-insensitive equality.
    let p = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::CmpColl(CmpKind::Eq, Collation::NoCase),
        ],
        vec![Value::Text("abc".into())],
    );
    assert_eq!(p.eval(&[Value::Text("ABC".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(p.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(p.eval(&[Value::Text("abd".into())], &[]).unwrap(), Value::Bool(false));
    // NULL propagates, exactly like the plain comparison.
    assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    // Unicode is NOT folded: 'É' != 'é' under NOCASE.
    let up = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::CmpColl(CmpKind::Eq, Collation::NoCase),
        ],
        vec![Value::Text("É".into())],
    );
    assert_eq!(up.eval(&[Value::Text("é".into())], &[]).unwrap(), Value::Bool(false));

    // RTRIM ignores trailing spaces.
    let rt = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::CmpColl(CmpKind::Eq, Collation::Rtrim),
        ],
        vec![Value::Text("abc".into())],
    );
    assert_eq!(rt.eval(&[Value::Text("abc   ".into())], &[]).unwrap(), Value::Bool(true));

    // `col0 COLLATE NOCASE IN ('X', 'y')`.
    let inl = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::PushConst(1),
            Instr::InListColl(2, Collation::NoCase),
        ],
        vec![Value::Text("X".into()), Value::Text("y".into())],
    );
    assert_eq!(inl.eval(&[Value::Text("x".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(inl.eval(&[Value::Text("Y".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(inl.eval(&[Value::Text("z".into())], &[]).unwrap(), Value::Bool(false));

    // codec: roundtrip + truncation safety on both new opcodes.
    for prog in [&p, &rt, &inl] {
        let mut buf = Vec::new();
        prog.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(&ExprProgram::decode(&buf, &mut pos).unwrap(), prog);
        assert_eq!(pos, buf.len());
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
    }
    // A bad collation tag byte in the CmpColl encoding is Corrupt, not a panic.
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let bad = *buf.last().unwrap(); // the collation byte is last
    assert_eq!(bad, Collation::NoCase as u8);
    *buf.last_mut().unwrap() = 0x7f;
    assert!(matches!(ExprProgram::decode(&buf, &mut 0), Err(Error::Corrupt(_))));
}

/// Comparison affinity + the storage-class comparison: the two opcodes the
/// binder emits for `price < '40.0'` over a typeless column.
#[test]
fn comparison_affinity_and_class_compare_semantics_and_codec() {
    use crate::value::{Affinity, Collation};
    use std::slice::from_ref;
    // `affinity(col0, NUMERIC) < affinity('40.0', NUMERIC)`.
    let p = prog(
        vec![
            Instr::PushCol(0),
            Instr::Affinity(Affinity::Numeric),
            Instr::PushConst(0),
            Instr::Affinity(Affinity::Numeric),
            Instr::CmpClass(CmpKind::Lt, Collation::Binary),
        ],
        vec![Value::Text("40.0".into())],
    );
    // The text operand converts to 40.0 and the numbers compare numerically —
    // sqlite's answer for a NUMERIC column holding 1000 and 35.
    assert_eq!(p.eval(&[Value::Int(1000)], &[]).unwrap(), Value::Bool(false));
    assert_eq!(p.eval(&[Value::Int(35)], &[]).unwrap(), Value::Bool(true));
    assert_eq!(p.eval(&[Value::Float(40.5)], &[]).unwrap(), Value::Bool(false));
    // A value the affinity CANNOT convert stays text and is ranked by class:
    // every number is below every text, so 'abc' is not < 40.0.
    assert_eq!(p.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(false));
    // NULL still propagates as NULL (3VL), not as "incomparable".
    assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Null);
    // A blob is above every text, which is above every number.
    assert_eq!(p.eval(&[Value::Blob(vec![0x41])], &[]).unwrap(), Value::Bool(false));

    // **`Affinity` is NOT `Cast`**, and this is the divergence that forced its
    // own opcode: a CAST forces a number out of anything, affinity converts
    // only when the whole string is numeric.
    let cast = prog(
        vec![Instr::PushCol(0), Instr::Cast(Affinity::Numeric)],
        vec![],
    );
    let aff = prog(
        vec![Instr::PushCol(0), Instr::Affinity(Affinity::Numeric)],
        vec![],
    );
    for (input, cast_out, aff_out) in [
        (Value::Text("40.0".into()), Value::Int(40), Value::Int(40)),
        (Value::Text("abc".into()), Value::Int(0), Value::Text("abc".into())),
        (Value::Text("12ab".into()), Value::Int(12), Value::Text("12ab".into())),
        (Value::Text("".into()), Value::Int(0), Value::Text("".into())),
        (Value::Blob(b"7".to_vec()), Value::Int(7), Value::Blob(b"7".to_vec())),
    ] {
        assert_eq!(cast.eval(from_ref(&input), &[]).unwrap(), cast_out, "cast {input:?}");
        assert_eq!(aff.eval(from_ref(&input), &[]).unwrap(), aff_out, "affinity {input:?}");
    }

    // A `Bool`/`Timestamp` has no sqlite storage class, so it is REFUSED here
    // rather than given an invented rank.
    assert!(p.eval(&[Value::Bool(true)], &[]).is_err());

    // The collated form folds TEXT under the collation, numbers untouched.
    let nc = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::CmpClass(CmpKind::Eq, Collation::NoCase),
        ],
        vec![Value::Text("ABC".into())],
    );
    assert_eq!(nc.eval(&[Value::Text("abc".into())], &[]).unwrap(), Value::Bool(true));
    assert_eq!(nc.eval(&[Value::Int(1)], &[]).unwrap(), Value::Bool(false));

    // codec: roundtrip + truncation safety on both new opcodes.
    for prog in [&p, &aff, &nc] {
        let mut buf = Vec::new();
        prog.encode_into(&mut buf);
        let mut pos = 0;
        assert_eq!(&ExprProgram::decode(&buf, &mut pos).unwrap(), prog);
        assert_eq!(pos, buf.len());
        for cut in 0..buf.len() {
            let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
        }
    }
    // Bad tag bytes are Corrupt, not a panic: the collation of a CmpClass…
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    *buf.last_mut().unwrap() = 0x7f;
    assert!(matches!(ExprProgram::decode(&buf, &mut 0), Err(Error::Corrupt(_))));
    // …and the affinity byte of an Affinity.
    let mut buf = Vec::new();
    aff.encode_into(&mut buf);
    *buf.last_mut().unwrap() = 0x7f;
    assert!(matches!(ExprProgram::decode(&buf, &mut 0), Err(Error::Corrupt(_))));
}

#[test]
fn rejects_malformed_programs() {
    assert!(ExprProgram::new(vec![Instr::Eq], vec![]).is_err()); // underflow
    assert!(ExprProgram::new(vec![], vec![]).is_err()); // empty
    assert!(ExprProgram::new(
        vec![Instr::PushConst(0), Instr::PushConst(1)],
        vec![Value::Int(1), Value::Int(2)]
    )
    .is_err()); // two results
    assert!(ExprProgram::new(vec![Instr::PushConst(5)], vec![]).is_err()); // bad const
}

#[test]
fn encode_decode_roundtrip_and_corrupt_safety() {
    let p = prog(
        vec![
            Instr::PushCol(3),
            Instr::Like(0),
            Instr::PushParam(1),
            Instr::And,
        ],
        vec![Value::Text("a%".into())],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    let q = ExprProgram::decode(&buf, &mut pos).unwrap();
    assert_eq!(p, q);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}

#[test]
fn cast_and_concat_semantics_and_codec() {
    use crate::value::Affinity;
    let cast = |v: Value, aff: Affinity| {
        prog(vec![Instr::PushParam(0), Instr::Cast(aff)], vec![]).eval(&[], &[v])
    };
    // NULL casts to NULL for every affinity.
    assert_eq!(cast(Value::Null, Affinity::Integer).unwrap(), Value::Null);
    assert_eq!(cast(Value::Null, Affinity::Blob).unwrap(), Value::Null);
    // float→int truncates toward zero (sqlite's rule), NaN/inf saturate
    // deterministically instead of being UB.
    assert_eq!(cast(Value::Float(-1.9), Affinity::Integer).unwrap(), Value::Int(-1));
    assert_eq!(cast(Value::Float(f64::NAN), Affinity::Integer).unwrap(), Value::Int(0));
    assert_eq!(
        cast(Value::Float(f64::INFINITY), Affinity::Integer).unwrap(),
        Value::Int(i64::MAX)
    );
    assert_eq!(cast(Value::Int(3), Affinity::Real).unwrap(), Value::Float(3.0));
    assert_eq!(cast(Value::Int(-7), Affinity::Text).unwrap(), Value::Text("-7".into()));
    assert_eq!(cast(Value::Bool(true), Affinity::Integer).unwrap(), Value::Int(1));
    // Permissive text→number: a leading numeric prefix parses; INTEGER stops at
    // the first non-digit, REAL takes the float prefix, NUMERIC decides int/real.
    assert_eq!(cast(Value::Text("12ab".into()), Affinity::Integer).unwrap(), Value::Int(12));
    assert_eq!(cast(Value::Text("1e3".into()), Affinity::Integer).unwrap(), Value::Int(1));
    assert_eq!(cast(Value::Text("abc".into()), Affinity::Integer).unwrap(), Value::Int(0));
    assert_eq!(cast(Value::Text("1e3".into()), Affinity::Real).unwrap(), Value::Float(1000.0));
    assert_eq!(cast(Value::Text("3.5".into()), Affinity::Numeric).unwrap(), Value::Float(3.5));
    assert_eq!(cast(Value::Text("3.0".into()), Affinity::Numeric).unwrap(), Value::Int(3));
    // A real VALUE stays real under NUMERIC even when integral.
    assert_eq!(cast(Value::Float(3.0), Affinity::Numeric).unwrap(), Value::Float(3.0));
    // real→text uses sqlite's %!.15g; int→blob is the bytes of its text.
    assert_eq!(cast(Value::Float(2.9), Affinity::Text).unwrap(), Value::Text("2.9".into()));
    assert_eq!(cast(Value::Int(90), Affinity::Blob).unwrap(), Value::Blob(b"90".to_vec()));
    assert_eq!(cast(Value::Blob(b"A".to_vec()), Affinity::Text).unwrap(), Value::Text("A".into()));
    // The one deviation: a non-UTF-8 blob has no mpedb TEXT representation.
    assert!(cast(Value::Blob(vec![0xff]), Affinity::Text).is_err());

    let cat = |a: Value, b: Value| {
        prog(
            vec![Instr::PushParam(0), Instr::PushParam(1), Instr::Concat],
            vec![],
        )
        .eval(&[], &[a, b])
    };
    assert_eq!(
        cat(Value::Text("ab".into()), Value::Int(3)).unwrap(),
        Value::Text("ab3".into())
    );
    assert_eq!(cat(Value::Text("x".into()), Value::Null).unwrap(), Value::Null);
    assert!(cat(Value::Text("x".into()), Value::Float(1.5)).is_err());

    // codec: roundtrip, truncation safety, and a bad CAST affinity tag.
    let p = prog(
        vec![
            Instr::PushCol(0),
            Instr::Cast(Affinity::Text),
            Instr::PushCol(1),
            Instr::Concat,
        ],
        vec![],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
    // Corrupt the Cast's type-tag byte: find OP_CAST and break the byte
    // after it — decode must say Corrupt, never panic or misread.
    let i = buf.iter().position(|&b| b == 30).unwrap(); // OP_CAST
    let mut evil = buf.clone();
    evil[i + 1] = 0xEE;
    assert!(matches!(
        ExprProgram::decode(&evil, &mut 0),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn new_scalar_fns_eval_match_sqlite_and_propagate_null() {
    // Build `f($1, $2, …)` over params so any Value (NULL included) reaches
    // the function unchanged.
    let call = |f: ScalarFn, args: &[Value]| -> Result<Value> {
        let mut instrs: Vec<Instr> =
            (0..args.len()).map(|i| Instr::PushParam(i as u16)).collect();
        instrs.push(Instr::Call(f, args.len() as u8));
        ExprProgram::new(instrs, vec![]).unwrap().eval(&[], args)
    };
    let t = |s: &str| Value::Text(s.into());

    // char: code points -> string; variadic; char() is empty. NULL
    // propagates here (documented gap vs sqlite, which reads NULL as 0).
    assert_eq!(call(ScalarFn::Char, &[Value::Int(72), Value::Int(105)]).unwrap(), t("Hi"));
    assert_eq!(call(ScalarFn::Char, &[Value::Int(230)]).unwrap(), t("æ"));
    assert_eq!(call(ScalarFn::Char, &[]).unwrap(), t(""));
    assert_eq!(call(ScalarFn::Char, &[Value::Int(72), Value::Null]).unwrap(), Value::Null);
    // An out-of-range code point becomes the replacement char, not a panic.
    assert_eq!(call(ScalarFn::Char, &[Value::Int(-1)]).unwrap(), t("\u{FFFD}"));

    // unicode: first char's code point; empty string -> NULL; NULL -> NULL.
    assert_eq!(call(ScalarFn::Unicode, &[t("A")]).unwrap(), Value::Int(65));
    assert_eq!(call(ScalarFn::Unicode, &[t("abc")]).unwrap(), Value::Int(97));
    assert_eq!(call(ScalarFn::Unicode, &[t("æ")]).unwrap(), Value::Int(230));
    assert_eq!(call(ScalarFn::Unicode, &[t("")]).unwrap(), Value::Null);
    assert_eq!(call(ScalarFn::Unicode, &[Value::Null]).unwrap(), Value::Null);

    // hex: uppercase hex of UTF-8 bytes (text) or raw bytes (blob).
    assert_eq!(call(ScalarFn::Hex, &[t("abc")]).unwrap(), t("616263"));
    assert_eq!(call(ScalarFn::Hex, &[t("z")]).unwrap(), t("7A"));
    assert_eq!(
        call(ScalarFn::Hex, &[Value::Blob(vec![0x00, 0xff, 0x10])]).unwrap(),
        t("00FF10")
    );
    assert_eq!(call(ScalarFn::Hex, &[t("")]).unwrap(), t(""));
    assert_eq!(call(ScalarFn::Hex, &[Value::Null]).unwrap(), Value::Null);
    assert!(matches!(call(ScalarFn::Hex, &[Value::Int(1)]), Err(Error::TypeMismatch(_))));

    // typeof: does NOT propagate NULL — typeof(NULL) is the text 'null'.
    assert_eq!(call(ScalarFn::Typeof, &[Value::Null]).unwrap(), t("null"));
    assert_eq!(call(ScalarFn::Typeof, &[Value::Int(1)]).unwrap(), t("integer"));
    assert_eq!(call(ScalarFn::Typeof, &[Value::Float(1.0)]).unwrap(), t("real"));
    assert_eq!(call(ScalarFn::Typeof, &[t("x")]).unwrap(), t("text"));
    assert_eq!(call(ScalarFn::Typeof, &[Value::Blob(vec![1])]).unwrap(), t("blob"));
    // mpedb's own first-class types report a SQLITE storage class, not their
    // own name: `typeof()` is a sqlite function whose whole documented range is
    // the five class names, and `sqlite3_column_type` already calls both of
    // these SQLITE_INTEGER. A sixth string would be a different answer to a
    // sqlite question rather than an error.
    assert_eq!(call(ScalarFn::Typeof, &[Value::Bool(true)]).unwrap(), t("integer"));
    assert_eq!(call(ScalarFn::Typeof, &[Value::Bool(false)]).unwrap(), t("integer"));
    assert_eq!(call(ScalarFn::Typeof, &[Value::Timestamp(0)]).unwrap(), t("integer"));
    // Param-only, unreachable as a result value; mapped like `column_type` does.
    assert_eq!(call(ScalarFn::Typeof, &[Value::List(vec![])]).unwrap(), t("null"));
    // The range is CLOSED: no `Value` names anything outside the five.
    for v in [
        Value::Null,
        Value::Int(1),
        Value::Float(1.0),
        t("x"),
        Value::Blob(vec![1]),
        Value::Bool(true),
        Value::Timestamp(1),
        Value::List(vec![Value::Int(1)]),
    ] {
        let got = call(ScalarFn::Typeof, std::slice::from_ref(&v)).unwrap();
        let name = match &got {
            Value::Text(s) => s.clone(),
            other => panic!("typeof returned a non-text {other:?}"),
        };
        assert!(
            matches!(name.as_str(), "null" | "integer" | "real" | "text" | "blob"),
            "typeof({v:?}) = {name:?} is outside sqlite's five storage classes"
        );
    }

    // trim(x, set): strip a set of chars from BOTH ends; 1-arg trims spaces.
    assert_eq!(call(ScalarFn::Trim, &[t("xxhixx"), t("x")]).unwrap(), t("hi"));
    assert_eq!(call(ScalarFn::Trim, &[t("  hi  ")]).unwrap(), t("hi"));
    assert_eq!(call(ScalarFn::Trim, &[t("hi"), Value::Null]).unwrap(), Value::Null);

    // codec: the four new tags round-trip and truncation stays Corrupt,
    // never a panic (repo rule). A linear chain keeps depth at 1 throughout.
    let p = prog(
        vec![
            Instr::PushConst(0),          // Int 104
            Instr::Call(ScalarFn::Char, 1),   // "h"
            Instr::Call(ScalarFn::Unicode, 1), // 104
            Instr::Call(ScalarFn::Char, 1),    // "h"
            Instr::Call(ScalarFn::Hex, 1),     // "68"
            Instr::Call(ScalarFn::Typeof, 1),  // "text"
        ],
        vec![Value::Int(104)],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}

#[test]
fn math_scalar_fns_match_sqlite_and_domain_edges() {
    // Build `f($1, $2, …)` over params so any Value (NULL included) reaches
    // the function unchanged.
    let call = |f: ScalarFn, args: &[Value]| -> Value {
        let mut instrs: Vec<Instr> =
            (0..args.len()).map(|i| Instr::PushParam(i as u16)).collect();
        instrs.push(Instr::Call(f, args.len() as u8));
        ExprProgram::new(instrs, vec![]).unwrap().eval(&[], args).unwrap()
    };
    // Approximate float equality (sqlite renders ~15 digits; here we compare the
    // f64s directly, so a tight relative tolerance is plenty).
    let approx = |v: Value, want: f64| match v {
        Value::Float(x) => assert!(
            (x - want).abs() <= 1e-12 * want.abs().max(1.0),
            "got {x}, want {want}"
        ),
        other => panic!("expected Float, got {other:?}"),
    };
    let f = |x: f64| Value::Float(x);
    let i = Value::Int;
    use std::f64::consts::{E, PI};

    // exp: e^x. Overflow is KEPT as +inf (sqlite returns Inf), never NULL.
    approx(call(ScalarFn::Exp, &[i(0)]), 1.0);
    approx(call(ScalarFn::Exp, &[f(1.0)]), E);
    assert!(matches!(call(ScalarFn::Exp, &[f(1000.0)]), Value::Float(x) if x.is_infinite()));

    // ln / log10 / log2: NULL for a non-positive argument (sqlite checks x<=0).
    approx(call(ScalarFn::Ln, &[f(E)]), 1.0);
    approx(call(ScalarFn::Ln, &[i(1)]), 0.0);
    assert_eq!(call(ScalarFn::Ln, &[i(0)]), Value::Null);
    assert_eq!(call(ScalarFn::Ln, &[i(-1)]), Value::Null);
    approx(call(ScalarFn::Log10, &[i(100)]), 2.0);
    assert_eq!(call(ScalarFn::Log10, &[i(0)]), Value::Null);
    assert_eq!(call(ScalarFn::Log10, &[i(-1)]), Value::Null);
    approx(call(ScalarFn::Log2, &[i(8)]), 3.0);
    assert_eq!(call(ScalarFn::Log2, &[i(0)]), Value::Null);

    // log(b, x): base b > 1 and x > 0, else NULL (matches sqlite exactly).
    approx(call(ScalarFn::LogBase, &[i(2), i(8)]), 3.0);
    approx(call(ScalarFn::LogBase, &[i(10), i(1000)]), 3.0);
    approx(call(ScalarFn::LogBase, &[i(3), i(1)]), 0.0); // x == 1 is allowed
    assert_eq!(call(ScalarFn::LogBase, &[i(1), i(5)]), Value::Null); // base == 1
    assert_eq!(call(ScalarFn::LogBase, &[f(0.5), i(8)]), Value::Null); // base < 1
    assert_eq!(call(ScalarFn::LogBase, &[i(0), i(5)]), Value::Null);
    assert_eq!(call(ScalarFn::LogBase, &[i(-2), i(8)]), Value::Null);
    assert_eq!(call(ScalarFn::LogBase, &[i(2), i(-1)]), Value::Null);

    // Trig / hyperbolic. asin/acos out of [-1, 1] → NaN → NULL.
    approx(call(ScalarFn::Sin, &[i(0)]), 0.0);
    approx(call(ScalarFn::Cos, &[i(0)]), 1.0);
    approx(call(ScalarFn::Tan, &[i(0)]), 0.0);
    approx(call(ScalarFn::Asin, &[i(0)]), 0.0);
    approx(call(ScalarFn::Acos, &[i(1)]), 0.0);
    approx(call(ScalarFn::Atan, &[i(0)]), 0.0);
    assert_eq!(call(ScalarFn::Asin, &[i(2)]), Value::Null);
    assert_eq!(call(ScalarFn::Acos, &[i(2)]), Value::Null);
    approx(call(ScalarFn::Sinh, &[i(0)]), 0.0);
    approx(call(ScalarFn::Cosh, &[i(0)]), 1.0);
    approx(call(ScalarFn::Tanh, &[i(0)]), 0.0);

    // atan2(y, x): note y is the FIRST argument.
    approx(call(ScalarFn::Atan2, &[i(1), i(1)]), PI / 4.0);
    approx(call(ScalarFn::Atan2, &[i(1), i(0)]), PI / 2.0);
    approx(call(ScalarFn::Atan2, &[i(0), i(1)]), 0.0);

    // radians / degrees are exact inverses on these values.
    approx(call(ScalarFn::Radians, &[i(180)]), PI);
    approx(call(ScalarFn::Degrees, &[f(PI)]), 180.0);

    // pi(): the one nullary scalar.
    approx(call(ScalarFn::Pi, &[]), PI);

    // mod(x, y) = fmod (sign of the dividend); a zero divisor → NULL, NOT the
    // `%` operator's DivisionByZero error.
    approx(call(ScalarFn::Mod, &[i(7), i(2)]), 1.0);
    approx(call(ScalarFn::Mod, &[i(-7), i(2)]), -1.0);
    approx(call(ScalarFn::Mod, &[i(7), i(-2)]), 1.0);
    approx(call(ScalarFn::Mod, &[f(-7.5), i(2)]), -1.5);
    assert_eq!(call(ScalarFn::Mod, &[i(5), i(0)]), Value::Null);

    // trunc: type-PRESERVING like ceil/floor (int stays int, float truncates).
    assert_eq!(call(ScalarFn::Trunc, &[f(2.9)]), Value::Float(2.0));
    assert_eq!(call(ScalarFn::Trunc, &[f(-2.9)]), Value::Float(-2.0));
    assert_eq!(call(ScalarFn::Trunc, &[i(5)]), Value::Int(5));

    // NULL propagates through every one (pi has no argument).
    for f in [ScalarFn::Exp, ScalarFn::Ln, ScalarFn::Sin, ScalarFn::Trunc] {
        assert_eq!(call(f, &[Value::Null]), Value::Null);
    }
    assert_eq!(call(ScalarFn::Atan2, &[Value::Null, i(1)]), Value::Null);
    assert_eq!(call(ScalarFn::LogBase, &[i(2), Value::Null]), Value::Null);
    assert_eq!(call(ScalarFn::Mod, &[Value::Null, i(2)]), Value::Null);

    // A non-number argument is a runtime type error, like sqrt/pow.
    let bad = {
        let p = prog(vec![Instr::PushParam(0), Instr::Call(ScalarFn::Sin, 1)], vec![]);
        p.eval(&[], &[Value::Text("x".into())])
    };
    assert!(matches!(bad, Err(Error::Corrupt(_)) | Err(Error::TypeMismatch(_))));

    // codec: a linear chain of the new tags round-trips, and truncation at every
    // offset stays Corrupt rather than panicking (repo rule). Depth stays 1.
    let p = prog(
        vec![
            Instr::PushConst(0),                // Float 8.0
            Instr::Call(ScalarFn::Log2, 1),     // 3.0
            Instr::Call(ScalarFn::Exp, 1),      // e^3
            Instr::Call(ScalarFn::Ln, 1),       // 3.0
            Instr::Call(ScalarFn::Trunc, 1),    // 3.0
            Instr::Call(ScalarFn::Sin, 1),      // sin(3)
        ],
        vec![Value::Float(8.0)],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }

    // pi() round-trips as a zero-argument call.
    let pp = prog(vec![Instr::Call(ScalarFn::Pi, 0)], vec![]);
    let mut b2 = Vec::new();
    pp.encode_into(&mut b2);
    assert_eq!(ExprProgram::decode(&b2, &mut 0).unwrap(), pp);
}

#[test]
fn is_distinct_is_null_safe_and_two_valued() {
    // `a IS b` == IsNotDistinct: NULL-safe equality that never yields NULL.
    let isnd = |a: Value, b: Value| {
        prog(
            vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsNotDistinct],
            vec![],
        )
        .eval(&[], &[a, b])
        .unwrap()
    };
    assert_eq!(isnd(Value::Null, Value::Null), Value::Bool(true));
    assert_eq!(isnd(Value::Null, Value::Int(1)), Value::Bool(false));
    assert_eq!(isnd(Value::Int(1), Value::Null), Value::Bool(false));
    assert_eq!(isnd(Value::Int(1), Value::Int(1)), Value::Bool(true));
    assert_eq!(isnd(Value::Int(1), Value::Int(2)), Value::Bool(false));
    // Text operands compare the same way.
    assert_eq!(
        isnd(Value::Text("a".into()), Value::Text("a".into())),
        Value::Bool(true)
    );
    assert_eq!(
        isnd(Value::Text("a".into()), Value::Text("b".into())),
        Value::Bool(false)
    );

    // `a IS NOT b` == IsDistinct: the exact negation, still never NULL.
    let isd = |a: Value, b: Value| {
        prog(
            vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsDistinct],
            vec![],
        )
        .eval(&[], &[a, b])
        .unwrap()
    };
    assert_eq!(isd(Value::Null, Value::Null), Value::Bool(false));
    assert_eq!(isd(Value::Null, Value::Int(1)), Value::Bool(true));
    assert_eq!(isd(Value::Int(1), Value::Null), Value::Bool(true));
    assert_eq!(isd(Value::Int(1), Value::Int(1)), Value::Bool(false));
    assert_eq!(isd(Value::Int(1), Value::Int(2)), Value::Bool(true));

    // A NULL result is impossible, so as a filter predicate every case is
    // decided — unlike `=`, where NULL denies. `NULL IS NULL` passes.
    let p = prog(
        vec![Instr::PushParam(0), Instr::PushParam(1), Instr::IsNotDistinct],
        vec![],
    );
    assert!(p
        .eval_filter(&mut Vec::new(), &[], &[Value::Null, Value::Null])
        .unwrap());
    assert!(!p
        .eval_filter(&mut Vec::new(), &[], &[Value::Null, Value::Int(1)])
        .unwrap());

    // codec: roundtrip + truncation safety (repo rule: Corrupt, never panic).
    let prog2 = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushCol(1),
            Instr::IsNotDistinct,
            Instr::PushCol(2),
            Instr::PushCol(3),
            Instr::IsDistinct,
            Instr::And,
        ],
        vec![],
    );
    let mut buf = Vec::new();
    prog2.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), prog2);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}

// ---- §2.6 `col IN (context list)` under 3VL ----

fn in_prog() -> ExprProgram {
    // PushCol(0) ; InParam(0)   ==   `c0 IN ($1)`
    ExprProgram::new(vec![Instr::PushCol(0), Instr::InParam(0)], vec![]).unwrap()
}

fn eval_in(probe: Value, list: Value) -> Value {
    in_prog().eval(&[probe], &[list]).unwrap()
}

#[test]
fn in_list_three_valued_logic() {
    let l = |v: Vec<Value>| Value::List(v);

    // plain hit / miss
    assert_eq!(eval_in(Value::Int(2), l(vec![Value::Int(1), Value::Int(2)])), Value::Bool(true));
    assert_eq!(eval_in(Value::Int(9), l(vec![Value::Int(1), Value::Int(2)])), Value::Bool(false));

    // a match WINS over a NULL element — this is why the NULL scan cannot
    // short-circuit before the equality scan.
    assert_eq!(
        eval_in(Value::Int(2), l(vec![Value::Null, Value::Int(2)])),
        Value::Bool(true)
    );

    // no match + a NULL element ⇒ UNKNOWN, not FALSE: the NULL might have
    // been the match.
    assert_eq!(eval_in(Value::Int(9), l(vec![Value::Null, Value::Int(2)])), Value::Null);

    // NULL probe is never TRUE
    assert_eq!(eval_in(Value::Null, l(vec![Value::Int(1)])), Value::Null);

    // empty set denies CLEANLY (FALSE, not NULL): "belongs to nothing".
    assert_eq!(eval_in(Value::Int(1), l(vec![])), Value::Bool(false));

    // an entirely-NULL set is an unknown set
    assert_eq!(eval_in(Value::Int(1), Value::Null), Value::Null);
}

/// A filter passes only on exactly TRUE, so every UNKNOWN above must deny.
/// This is the property a policy actually rests on.
#[test]
fn in_list_unknown_denies_in_a_filter() {
    let p = in_prog();
    // no match + NULL element ⇒ UNKNOWN ⇒ row not visible
    assert!(!p
        .eval_filter(&mut Vec::new(), &[Value::Int(9)], &[Value::List(vec![Value::Null])])
        .unwrap());
    // NULL probe ⇒ UNKNOWN ⇒ row not visible
    assert!(!p
        .eval_filter(&mut Vec::new(), &[Value::Null], &[Value::List(vec![Value::Int(1)])])
        .unwrap());
    // a real match is visible
    assert!(p
        .eval_filter(&mut Vec::new(), &[Value::Int(1)], &[Value::List(vec![Value::Int(1)])])
        .unwrap());
}

/// A type mismatch inside the list must ERROR, not quietly deny every row —
/// a silent deny would look exactly like "this tenant owns nothing".
#[test]
fn in_list_type_mismatch_is_an_error_not_a_silent_deny() {
    let r = in_prog().eval(&[Value::Int(1)], &[Value::List(vec![Value::Text("1".into())])]);
    assert!(matches!(r, Err(Error::TypeMismatch(_))), "got {r:?}");
    // and a non-list param is likewise a caller error
    let r2 = in_prog().eval(&[Value::Int(1)], &[Value::Int(1)]);
    assert!(matches!(r2, Err(Error::TypeMismatch(_))), "got {r2:?}");
}

#[test]
fn in_param_roundtrips_and_out_of_range_param_is_corrupt() {
    let p = in_prog();
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    // a program referencing param 5 with no params supplied must not panic
    let bad = ExprProgram::new(vec![Instr::PushCol(0), Instr::InParam(5)], vec![]).unwrap();
    assert!(matches!(bad.eval(&[Value::Int(1)], &[]), Err(Error::Corrupt(_))));
}

/// Lists cross the intent ring as params, so they must survive write/read.
#[test]
fn list_value_roundtrips_through_the_param_codec() {
    use crate::value::{read_value, write_value};
    let v = Value::List(vec![Value::Int(1), Value::Text("a".into()), Value::Null]);
    let mut buf = Vec::new();
    write_value(&mut buf, &v);
    let mut pos = 0;
    assert_eq!(read_value(&buf, &mut pos).unwrap(), v);

    // truncation at every offset yields Corrupt, never a panic
    for cut in 0..buf.len() {
        let mut pos = 0;
        let _ = read_value(&buf[..cut], &mut pos); // must not panic
    }
    // a nested list is rejected on the way in
    let mut nested = Vec::new();
    write_value(&mut nested, &Value::List(vec![Value::List(vec![Value::Int(1)])]));
    let mut pos = 0;
    assert!(matches!(read_value(&nested, &mut pos), Err(Error::Corrupt(_))));
}

/// The bitwise family (#74 item 2): the coercion, the shift corners, NULL
/// propagation, and the codec round trip including truncation safety.
///
/// Every expected value here was read off `sqlite3` 3.45.1 before it was
/// written down; `crates/mpedb/tests/bitwise.rs` re-checks the same cases
/// against the live binary so the two cannot drift.
#[test]
fn bitwise_semantics_and_codec() {
    let bin = |op: Instr, a: Value, b: Value| {
        prog(vec![Instr::PushParam(0), Instr::PushParam(1), op], vec![]).eval(&[], &[a, b])
    };
    let not = |a: Value| prog(vec![Instr::PushParam(0), Instr::BitNot], vec![]).eval(&[], &[a]);
    let i = Value::Int;

    // Plain integer AND/OR.
    assert_eq!(bin(Instr::BitOr, i(3), i(1)).unwrap(), i(3));
    assert_eq!(bin(Instr::BitAnd, i(3), i(1)).unwrap(), i(1));
    assert_eq!(bin(Instr::BitAnd, i(-1), i(5)).unwrap(), i(5));
    assert_eq!(not(i(5)).unwrap(), i(-6));
    assert_eq!(not(i(0)).unwrap(), i(-1));

    // Shifts: a count of 64+ clears, except `>>` of a negative (arithmetic).
    assert_eq!(bin(Instr::Shl, i(1), i(64)).unwrap(), i(0));
    assert_eq!(bin(Instr::Shl, i(1), i(63)).unwrap(), i(i64::MIN));
    assert_eq!(bin(Instr::Shr, i(8), i(64)).unwrap(), i(0));
    assert_eq!(bin(Instr::Shr, i(-8), i(64)).unwrap(), i(-1));
    assert_eq!(bin(Instr::Shr, i(-1), i(63)).unwrap(), i(-1));
    assert_eq!(bin(Instr::Shr, i(-8), i(1)).unwrap(), i(-4));
    assert_eq!(bin(Instr::Shr, i(-8), i(0)).unwrap(), i(-8));
    // A NEGATIVE count shifts the other way; -64 and below clamp to 64.
    assert_eq!(bin(Instr::Shl, i(1), i(-1)).unwrap(), i(0));
    assert_eq!(bin(Instr::Shr, i(1), i(-1)).unwrap(), i(2));
    assert_eq!(bin(Instr::Shl, i(5), i(-2)).unwrap(), i(1));
    assert_eq!(bin(Instr::Shr, i(-5), i(-2)).unwrap(), i(-20));
    assert_eq!(bin(Instr::Shl, i(1), i(-64)).unwrap(), i(0));
    assert_eq!(bin(Instr::Shl, i(1), i(i64::MIN)).unwrap(), i(0));
    // `<<` WRAPS — the one integer op in mpedb that does not raise.
    assert_eq!(bin(Instr::Shl, i(i64::MAX), i(1)).unwrap(), i(-2));
    assert_eq!(bin(Instr::Shl, i(i64::MIN), i(1)).unwrap(), i(0));
    assert_eq!(bin(Instr::Shr, i(i64::MIN), i(1)).unwrap(), i(-4611686018427387904));

    // sqlite's coercion: reals truncate toward zero and clamp…
    assert_eq!(bin(Instr::BitOr, Value::Float(3.7), i(0)).unwrap(), i(3));
    assert_eq!(bin(Instr::BitOr, Value::Float(-3.7), i(0)).unwrap(), i(-3));
    assert_eq!(bin(Instr::BitOr, Value::Float(1e300), i(0)).unwrap(), i(i64::MAX));
    assert_eq!(bin(Instr::BitOr, Value::Float(-1e300), i(0)).unwrap(), i(i64::MIN));
    assert_eq!(bin(Instr::BitOr, Value::Float(9.3e18), i(0)).unwrap(), i(i64::MAX));
    assert_eq!(bin(Instr::BitOr, Value::Float(f64::NAN), i(0)).unwrap(), i(0));
    // …text takes an integer-PREFIX parse that stops at the first non-digit…
    let t = |s: &str| Value::Text(s.into());
    for (s, want) in [
        ("3", 3i64),
        ("abc", 0),
        ("", 0),
        ("1e3", 1),
        ("3.9", 3),
        ("12abc", 12),
        (" -5", -5),
        ("+5", 5),
        ("--5", 0),
        ("5 ", 5),
        ("\t\n\x0b\x0c\r7", 7),
        ("0x10", 0),
        ("  +0009 ", 9),
    ] {
        assert_eq!(bin(Instr::BitOr, t(s), i(0)).unwrap(), i(want), "for {s:?}");
    }
    // …and overflow in that parse CLAMPS rather than wrapping a u64.
    for (s, want) in [
        ("99999999999999999999", i64::MAX),
        ("-99999999999999999999", i64::MIN),
        ("9223372036854775808", i64::MAX),
        ("-9223372036854775809", i64::MIN),
        ("18446744073709551617", i64::MAX),
        ("000000000000000000000000000000009223372036854775807", i64::MAX),
        ("9223372036854775807", i64::MAX),
        ("-9223372036854775808", i64::MIN),
    ] {
        assert_eq!(bin(Instr::BitOr, t(s), i(0)).unwrap(), i(want), "for {s:?}");
    }
    // Blobs take the same parse over their bytes; bool IS the integer 0/1.
    assert_eq!(bin(Instr::BitOr, Value::Blob(b"12".to_vec()), i(0)).unwrap(), i(12));
    assert_eq!(bin(Instr::BitOr, Value::Blob(b"A".to_vec()), i(0)).unwrap(), i(0));
    assert_eq!(bin(Instr::BitOr, Value::Bool(true), i(2)).unwrap(), i(3));
    assert_eq!(not(Value::Bool(false)).unwrap(), i(-1));

    // NULL propagates through all five, and 0 is not NULL.
    for op in [Instr::BitOr, Instr::BitAnd, Instr::Shl, Instr::Shr] {
        assert_eq!(bin(op, Value::Null, i(1)).unwrap(), Value::Null);
        assert_eq!(bin(op, i(1), Value::Null).unwrap(), Value::Null);
        assert_eq!(bin(op, Value::Null, Value::Null).unwrap(), Value::Null);
    }
    assert_eq!(not(Value::Null).unwrap(), Value::Null);

    // mpedb's own Timestamp has no sqlite storage class, so it has no answer
    // to reproduce — a clean error, never an invented integer.
    assert!(matches!(
        bin(Instr::BitOr, Value::Timestamp(0), i(1)),
        Err(Error::TypeMismatch(_))
    ));

    // Codec: all five round-trip, and every truncation is a clean error.
    let p = prog(
        vec![
            Instr::PushParam(0),
            Instr::BitNot,
            Instr::PushConst(0),
            Instr::BitAnd,
            Instr::PushConst(0),
            Instr::BitOr,
            Instr::PushConst(0),
            Instr::Shl,
            Instr::PushConst(0),
            Instr::Shr,
        ],
        vec![Value::Int(1)],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
    // `BitNot` is UNARY: a program that treats it as binary must not validate.
    assert!(ExprProgram::new(vec![Instr::BitNot], vec![]).is_err());
    assert!(ExprProgram::new(vec![Instr::PushParam(0), Instr::BitAnd], vec![]).is_err());
}

/// The SCALAR `max`/`min` (#74 item 5): sqlite's `minmaxFunc`, tie rule
/// included, plus the codec round trip for tags 60/61.
///
/// Expected values read off `sqlite3` 3.45.1;
/// `crates/mpedb/tests/min_max_scalar.rs` re-checks them against the binary.
#[test]
fn scalar_min_max_semantics_and_codec() {
    let call = |f: ScalarFn, args: &[Value]| -> Result<Value> {
        let mut instrs: Vec<Instr> = (0..args.len()).map(|i| Instr::PushParam(i as u16)).collect();
        instrs.push(Instr::Call(f, args.len() as u8));
        prog(instrs, vec![]).eval(&[], args)
    };
    let mx = |a: &[Value]| call(ScalarFn::Max2, a).unwrap();
    let mn = |a: &[Value]| call(ScalarFn::Min2, a).unwrap();
    let i = Value::Int;
    let f = Value::Float;
    let t = |s: &str| Value::Text(s.into());

    assert_eq!(mx(&[i(1), i(2)]), i(2));
    assert_eq!(mn(&[i(1), i(2)]), i(1));
    assert_eq!(mx(&[i(1), i(2), i(3), i(0)]), i(3));
    assert_eq!(mn(&[i(1), i(2), i(3), i(0)]), i(0));

    // ANY NULL argument yields NULL, at any position.
    for args in [
        vec![i(1), Value::Null],
        vec![Value::Null, i(1)],
        vec![i(1), Value::Null, i(3)],
        vec![Value::Null, Value::Null],
    ] {
        assert_eq!(mx(&args), Value::Null, "{args:?}");
        assert_eq!(mn(&args), Value::Null, "{args:?}");
    }

    // The winning ARGUMENT is returned unchanged: `max(3, 2.5)` is the INTEGER
    // 3, not 3.0. Selection, not computation.
    assert_eq!(mx(&[i(3), f(2.5)]), i(3));
    assert_eq!(mx(&[i(1), f(2.5)]), f(2.5));
    assert_eq!(mn(&[i(3), f(2.5)]), f(2.5));

    // The TIE rule, sqlite's `minmaxFunc` loop verbatim: `max` keeps the
    // EARLIER equal argument (`cmp < 0` replaces), `min` takes the LATER one
    // (`cmp >= 0` replaces). Observable only when the tied values differ in
    // class, which is exactly `typeof(max(1, 1.0))` vs `typeof(min(1, 1.0))`.
    assert_eq!(mx(&[i(1), f(1.0)]), i(1));
    assert_eq!(mn(&[i(1), f(1.0)]), f(1.0));
    assert_eq!(mx(&[f(1.0), i(1)]), f(1.0));
    assert_eq!(mn(&[f(1.0), i(1)]), i(1));
    assert_eq!(mx(&[i(2), f(2.0), i(2)]), i(2));

    // Storage-class order across classes (numbers < text < blob), and the
    // BINARY collation for text ('B' = 0x42 < 'a' = 0x61).
    assert_eq!(mx(&[t("a"), i(1)]), t("a"));
    assert_eq!(mn(&[t("a"), i(1)]), i(1));
    assert_eq!(mx(&[t("a"), t("B")]), t("a"));
    assert_eq!(mn(&[t("a"), t("B")]), t("B"));
    assert_eq!(mx(&[Value::Blob(vec![1]), t("a")]), Value::Blob(vec![1]));

    // mpedb's own Bool/Timestamp have no sqlite storage class, so a pair
    // involving one against a different class has no sqlite answer to
    // reproduce: an error, never an arbitrary winner.
    assert!(matches!(
        call(ScalarFn::Max2, &[Value::Bool(true), t("a")]),
        Err(Error::TypeMismatch(_))
    ));
    assert!(matches!(
        call(ScalarFn::Min2, &[Value::Timestamp(0), i(1)]),
        Err(Error::TypeMismatch(_))
    ));
    // Same-type bools and timestamps are fine.
    assert_eq!(mx(&[Value::Bool(false), Value::Bool(true)]), Value::Bool(true));
    assert_eq!(mn(&[Value::Timestamp(5), Value::Timestamp(2)]), Value::Timestamp(2));

    // Arity: two or more. One argument is the AGGREGATE and must not validate
    // as this scalar.
    assert!(!ScalarFn::Max2.arity_ok(0));
    assert!(!ScalarFn::Max2.arity_ok(1));
    assert!(ScalarFn::Max2.arity_ok(2));
    assert!(ScalarFn::Min2.arity_ok(255));
    assert!(ExprProgram::new(
        vec![Instr::PushParam(0), Instr::Call(ScalarFn::Max2, 1)],
        vec![]
    )
    .is_err());

    // Codec: tags 60/61 round-trip, and every truncation is a clean error.
    let p = prog(
        vec![
            Instr::PushConst(0),
            Instr::PushConst(0),
            Instr::Call(ScalarFn::Max2, 2),
            Instr::PushConst(0),
            Instr::Call(ScalarFn::Min2, 2),
        ],
        vec![Value::Int(1)],
    );
    let mut buf = Vec::new();
    p.encode_into(&mut buf);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&buf, &mut pos).unwrap(), p);
    assert_eq!(pos, buf.len());
    for cut in 0..buf.len() {
        let _ = ExprProgram::decode(&buf[..cut], &mut 0); // must not panic
    }
}
