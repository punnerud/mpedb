use super::*;

fn values(source: &str) -> Vec<Token> {
    lex(source).unwrap().into_iter().map(|t| t.token).collect()
}

#[test]
fn preserves_exact_number_spellings_and_range_delimiters() {
    assert_eq!(
        values("9007199254740992.123456789 1..3 .5 1.e-130 2F 3d"),
        vec![
            Token::Number("9007199254740992.123456789".into()),
            Token::Number("1".into()),
            Token::Symbol(".."),
            Token::Number("3".into()),
            Token::Number(".5".into()),
            Token::Number("1.e-130".into()),
            Token::Number("2F".into()),
            Token::Number("3d".into()),
        ]
    );
    for text in ["1e", "1e+", "1e-"] {
        assert!(lex(text).is_err());
    }
}

#[test]
fn oracle_strings_keep_national_identity_and_literal_backslashes() {
    assert_eq!(
        values("'it''s' N'blå' 'a\\b' '' q'[BEGIN 'x'; END;]' nq'!日本!'"),
        vec![
            Token::String {
                value: "it's".into(),
                national: false
            },
            Token::String {
                value: "blå".into(),
                national: true
            },
            Token::String {
                value: "a\\b".into(),
                national: false
            },
            Token::String {
                value: "".into(),
                national: false
            },
            Token::String {
                value: "BEGIN 'x'; END;".into(),
                national: false
            },
            Token::String {
                value: "日本".into(),
                national: true
            },
        ]
    );
    for (open, close) in [
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('<', '>'),
        ('!', '!'),
        ('é', 'é'),
    ] {
        assert_eq!(
            values(&format!("Q'{open}x'y{close}'")),
            vec![Token::String {
                value: "x'y".into(),
                national: false
            }]
        );
    }
    for text in ["'x", "q'[x'", "nq'!x", "q' x '"] {
        assert!(lex(text).is_err());
    }
}

#[test]
fn identifiers_and_binds_follow_oracle_case_rules() {
    assert_eq!(
        values("Foo \"Foo\" a_b$# :value :1 := => ** || %ROWTYPE"),
        vec![
            Token::Identifier {
                name: "FOO".into(),
                quoted: false
            },
            Token::Identifier {
                name: "Foo".into(),
                quoted: true
            },
            Token::Identifier {
                name: "A_B$#".into(),
                quoted: false
            },
            Token::Bind("VALUE".into()),
            Token::Bind("1".into()),
            Token::Symbol(":="),
            Token::Symbol("=>"),
            Token::Symbol("**"),
            Token::Symbol("||"),
            Token::Symbol("%"),
            Token::Identifier {
                name: "ROWTYPE".into(),
                quoted: false
            },
        ]
    );
    for text in ["\"\"", "\"a\0b\"", ":", ":-", "_name"] {
        assert!(lex(text).is_err());
    }
}

#[test]
fn comments_do_not_eat_literals_and_source_spans_are_utf8_boundaries() {
    let source = "-- før\n/* ikke nested /* */\n BEGIN q'!/* -- */!' ; END;";
    let tokens = lex(source).unwrap();
    assert_eq!(tokens.len(), 5);
    for token in &tokens {
        assert!(!source[token.span.clone()].is_empty());
    }
    assert_eq!(&source[tokens[1].span.clone()], "q'!/* -- */!'");
    assert!(lex("/* open").is_err());
    assert!(lex("-- kommentar\n  `")
        .unwrap_err()
        .to_string()
        .contains("2:3"));
}

#[test]
fn truncated_multibyte_sources_never_panic() {
    let source = "CREATE FUNCTION blå RETURN VARCHAR2 IS BEGIN RETURN nq'«æøå»'; END;";
    for end in 0..=source.len() {
        if source.is_char_boundary(end) {
            let _ = lex(&source[..end]);
        }
    }
}
