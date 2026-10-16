use super::*;

#[test]
fn parses_what_postgresql_parses() {
    for (input, want) in [
        ("0", "0"),
        ("-0", "0"),
        ("+0.000", "0.000"),
        ("1", "1"),
        ("+1", "1"),
        // The SCALE survives — this is what a `Decimal("1.00")` round trip
        // rests on, and what `canonical_numeric` deliberately erases for
        // the key.
        ("1.0", "1.0"),
        ("1.00", "1.00"),
        ("0001.5000", "1.5000"),
        ("-1.50", "-1.50"),
        (" 42 ", "42"),
        // Exponents are EXPANDED: one value, one spelling.
        ("1e3", "1000"),
        ("1E3", "1000"),
        ("1e+3", "1000"),
        ("1.5e-3", "0.0015"),
        ("-2.5e2", "-250"),
        ("1.50e2", "150"),
        ("1e-5", "0.00001"),
        (".5", "0.5"),
        ("5.", "5"),
        // The specials, by name.
        ("nan", "NaN"),
        ("NaN", "NaN"),
        ("-nan", "NaN"),
        ("infinity", "Infinity"),
        ("-Infinity", "-Infinity"),
        ("inf", "Infinity"),
    ] {
        assert_eq!(parse_numeric(input).as_deref(), Some(want), "{input}");
    }
}

#[test]
fn refuses_what_is_not_a_number() {
    for input in [
        "", " ", "abc", "1.2.3", "1_000", "--1", "1e", "1e+", "1ee3", "0x10", "1 2", ".",
        "1e999999999999999999999", "1e-99999999999999999999",
    ] {
        assert_eq!(parse_numeric(input), None, "{input} should not parse");
    }
}

/// A `numeric` compares by VALUE — the property `1.0 = 1.00` rests on.
#[test]
fn compares_by_value_not_by_spelling() {
    use Ordering::*;
    for (a, b, want) in [
        ("1.0", "1.00", Equal),
        ("1", "1.000", Equal),
        ("0", "-0", Equal),
        ("+5", "5", Equal),
        ("9", "10", Less),
        ("0.5", "0.51", Less),
        ("-0.5", "-0.51", Greater),
        ("-1", "1", Less),
        ("-100", "-9", Less),
        ("0.00001", "0.0001", Less),
        // PostgreSQL puts NaN above everything, unlike a float NaN.
        ("NaN", "Infinity", Greater),
        ("Infinity", "999999999999999999999", Greater),
        ("-Infinity", "-999999999999999999999", Less),
        ("NaN", "NaN", Equal),
    ] {
        assert_eq!(numeric_cmp(a, b), want, "{a} vs {b}");
        assert_eq!(numeric_cmp(b, a), want.reverse(), "{b} vs {a}");
    }
}

/// The stored form keeps the scale; the key form does not. Two functions,
/// two jobs — a single "canonical" would have to give up one of them.
#[test]
fn the_stored_form_and_the_key_form_differ_on_purpose() {
    for (input, stored, key) in [
        ("1.00", "1.00", "1"),
        ("1.50", "1.50", "1.5"),
        ("0.000", "0.000", "0"),
        ("100", "100", "100"),
    ] {
        assert_eq!(parse_numeric(input).as_deref(), Some(stored), "stored {input}");
        assert_eq!(canonical_numeric(stored), key, "key {input}");
    }
    // …and the two spellings still compare and group as ONE value.
    assert_eq!(numeric_cmp("1.00", "1.0"), Ordering::Equal);
}

/// `sql_cmp` routes a numeric pair to the value comparison, so a `WHERE`
/// on a numeric column is not a spelling test.
#[test]
fn sql_cmp_answers_a_numeric_pair() {
    let a = Value::Numeric("1.0".into());
    let b = Value::Numeric("1.00".into());
    assert_eq!(a.sql_cmp(&b).unwrap(), Some(Ordering::Equal));
    let c = Value::Numeric("10".into());
    let d = Value::Numeric("9".into());
    assert_eq!(c.sql_cmp(&d).unwrap(), Some(Ordering::Greater));
}

/// A numeric against an INTEGER stays a refusal rather than a rank-only
/// "equal". Ranking it with the numbers made `Numeric("2")` and `Int(1)`
/// compare equal, which is the wrong answer this split exists to avoid.
#[test]
fn a_numeric_against_an_integer_is_not_silently_equal() {
    let n = Value::Numeric("2".into());
    let i = Value::Int(1);
    assert!(n.sql_cmp(&i).is_err());
    assert_eq!(n.sort_cmp(&i, Collation::Binary), None);
}
