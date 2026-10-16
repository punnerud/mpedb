use super::*;

#[test]
fn timestamps_round_trip() {
    for (text, us) in [
        ("1970-01-01 00:00:00+00", 0i64),
        ("2023-11-14 22:13:20+00", 1_700_000_000_000_000),
        ("1969-12-31 23:59:59+00", -1_000_000),
        ("2023-11-14 22:13:20.0015+00", 1_700_000_000_001_500),
    ] {
        assert_eq!(parse_timestamp_text(text), Some(us), "parse {text}");
        assert_eq!(timestamp_text(us), text, "render {us}");
    }
}

#[test]
fn zones_and_separators_are_understood() {
    let base = parse_timestamp_text("2020-01-02 03:04:05").unwrap();
    assert_eq!(parse_timestamp_text("2020-01-02T03:04:05"), Some(base));
    assert_eq!(parse_timestamp_text("2020-01-02 03:04:05Z"), Some(base));
    // +01 means the instant is an hour EARLIER in UTC.
    assert_eq!(parse_timestamp_text("2020-01-02 04:04:05+01"), Some(base));
    assert_eq!(parse_timestamp_text("2020-01-02 02:04:05-01"), Some(base));
    assert_eq!(parse_timestamp_text("2020-01-02 04:34:05+01:30"), Some(base));
    // A bare date is midnight.
    assert_eq!(
        parse_timestamp_text("2020-01-02"),
        Some(days_from_civil(2020, 1, 2).unwrap() * 86_400 * 1_000_000)
    );
}

/// Same rule as `parse_date_text`: all or nothing, never a prefix.
#[test]
fn a_timestamp_is_all_or_nothing() {
    for bad in ["2020", "2020-01", "2020-13-01 00:00:00", "2021-02-29 00:00:00", "abc", ""] {
        assert_eq!(parse_timestamp_text(bad), None, "{bad} should not parse");
    }
}
