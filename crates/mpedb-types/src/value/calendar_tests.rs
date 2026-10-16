use super::*;

#[test]
fn dates_round_trip_across_the_whole_range() {
    for (text, days) in [
        ("1970-01-01", 0),
        ("1970-01-02", 1),
        ("1969-12-31", -1),
        ("2020-01-01", 18262),
        ("2020-01-02", 18263),
        ("2020-02-29", 18321), // a leap day
        ("2000-02-29", 11016), // the 400-year rule
        ("0001-01-01", -719162),
        ("9999-12-31", 2932896),
    ] {
        assert_eq!(parse_date_text(text), Some(days), "parse {text}");
        assert_eq!(date_text(days), text, "render {days}");
    }
}

/// The bug this whole type surface exists to close: sqlite's CAST rule
/// takes the leading number, so `'2020-01-02'` became `2020`. Nothing here
/// may accept a prefix.
#[test]
fn a_date_is_all_or_nothing() {
    for bad in [
        "2020",
        "2020-01",
        "2020-01-02 03:04:05",
        "2020-01-02T03:04",
        "2020-13-01",
        "2020-00-01",
        "2020-01-00",
        "2020-01-32",
        "2021-02-29", // not a leap year
        "1900-02-29", // the 100-year rule
        "2020-+1-02",
        "abc",
        "",
        "2020-01-02x",
    ] {
        assert_eq!(parse_date_text(bad), None, "{bad} should not parse");
    }
}

#[test]
fn times_round_trip_and_refuse_a_zone() {
    for (text, us) in [
        ("00:00:00", 0),
        ("03:04:05", 11_045_000_000),
        ("23:59:59.999999", 86_399_999_999),
        ("03:04:05.0015", 11_045_001_500),
    ] {
        assert_eq!(parse_time_text(text), Some(us), "parse {text}");
        assert_eq!(time_text(us), text, "render {us}");
    }
    // Legal input that renders back with the seconds PostgreSQL prints.
    assert_eq!(parse_time_text("03:04"), Some(11_040_000_000));
    // 24:00:00 is PostgreSQL's end-of-day; 24:00:01 is not a time.
    assert_eq!(parse_time_text("24:00:00"), Some(86_400_000_000));
    for bad in ["3", "25:00:00", "24:00:01", "03:04:05+02", "03:04:05Z", "0a:00:00", ""] {
        assert_eq!(parse_time_text(bad), None, "{bad} should not parse");
    }
}
