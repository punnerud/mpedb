//! mpedb values ⇄ PostgreSQL's TEXT wire format.
//!
//! Only the text format, deliberately. The binary format is faster and is what
//! a tuned client asks for, but it is also where a wrong answer hides best: a
//! misencoded `int8` is eight bytes that decode to a plausible number with no
//! error anywhere. Text is self-describing enough that a mistake shows up as a
//! parse failure on the client, and the server advertises text-only in
//! `RowDescription`, so no client is surprised.
//!
//! The one place text is not a compromise is `numeric`: PostgreSQL's text form
//! for it IS the canonical decimal string, which is exactly how mpedb carries
//! the type. There is nothing lost in that column at all.

use mpedb_types::Value;

/// Render a value the way PostgreSQL's text output would.
///
/// `None` means SQL NULL, which the caller writes as a -1 length rather than as
/// any string — the difference between NULL and `''` never reaches this
/// function.
/// Render a value as the text of the PostgreSQL type the `RowDescription`
/// promised, which is not always what the mpedb value renders as on its own.
///
/// The two MUST agree. `uuid` and `bytea` are both `Blob`, so once the
/// description says `uuid` (from the column's declared type) a raw
/// `\x8e2082b3…` in the data row makes the client raise "badly formed
/// hexadecimal UUID string" — it was promised a UUID and handed bytea.
pub fn to_text_as(v: &Value, oid: u32) -> Option<Vec<u8>> {
    match (oid, v) {
        // int2vector (22) and oidvector (30) are ARRAYS that PostgreSQL prints
        // SPACE-separated and unbraced, not in the `{…}` array form — and
        // clients parse exactly that. `pg_index.indkey` is the one every ORM
        // reads, so getting this wrong changes what a reflected composite
        // index looks like.
        (22 | 30, Value::List(items)) => {
            let parts: Vec<String> = items
                .iter()
                .map(|x| match to_text(x) {
                    Some(b) => String::from_utf8_lossy(&b).into_owned(),
                    None => "0".to_string(),
                })
                .collect();
            Some(parts.join(" ").into_bytes())
        }
        // 2950 = uuid: canonical 8-4-4-4-12 lowercase hex.
        (2950, Value::Blob(b)) if b.len() == 16 => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Some(
                format!(
                    "{}-{}-{}-{}-{}",
                    &hex[0..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..32]
                )
                .into_bytes(),
            )
        }
        _ => to_text(v),
    }
}

pub fn to_text(v: &Value) -> Option<Vec<u8>> {
    Some(match v {
        Value::Null => return None,
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Float(f) => float_text(*f).into_bytes(),
        // PostgreSQL's boolean output is a single letter, not `true`/`false`.
        // A client comparing against 't' is the normal case, and psql prints
        // exactly this.
        Value::Bool(b) => if *b { "t" } else { "f" }.as_bytes().to_vec(),
        Value::Text(s) => s.as_bytes().to_vec(),
        // `bytea_output = hex` has been PostgreSQL's default since 9.0, and
        // every current client decodes it. The older escape format is
        // ambiguous in ways that bite on high bytes.
        Value::Blob(b) => {
            let mut out = Vec::with_capacity(2 + b.len() * 2);
            out.extend_from_slice(b"\\x");
            for byte in b {
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
            out
        }
        Value::Timestamp(us) => timestamp_text(*us).into_bytes(),
        // `date` and `time` print the way PostgreSQL prints them — the whole
        // point of storing them as their own types rather than as integers is
        // that the client gets a `datetime.date` back instead of `19723`.
        Value::Date(days) => mpedb_types::value::date_text(*days).into_bytes(),
        Value::Time(us) => mpedb_types::value::time_text(*us).into_bytes(),
        // Nothing to convert: PostgreSQL's text form for `numeric` IS the
        // canonical decimal string mpedb stores.
        Value::Numeric(n) => n.as_bytes().to_vec(),
        // An ARRAY, in PostgreSQL's own external representation. A list
        // reaches here two ways: as the result of `array_agg`, and as the
        // session-context list that is parameter-only — the second cannot be
        // in a result row, so what this renders is always the first.
        Value::List(items) => array_text(items),
    })
}

/// PostgreSQL's array external representation: `{1,2,3}`, `{}` for empty, the
/// bare word `NULL` for a null element.
///
/// An element is quoted when it could not otherwise be read back: when it is
/// empty, contains a delimiter, brace, quote, backslash or whitespace, or
/// would be mistaken for the null marker. Inside quotes `"` and `\` escape.
fn array_text(items: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + items.len() * 8);
    out.push(b'{');
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        match to_text(v) {
            // A NULL ELEMENT is the unquoted word; `"NULL"` is the STRING.
            None => out.extend_from_slice(b"NULL"),
            Some(raw) => {
                let needs_quote = raw.is_empty()
                    || raw.eq_ignore_ascii_case(b"null")
                    || raw.iter().any(|b| {
                        matches!(b, b'{' | b'}' | b',' | b'"' | b'\\') || b.is_ascii_whitespace()
                    });
                if !needs_quote {
                    out.extend_from_slice(&raw);
                } else {
                    out.push(b'"');
                    for b in raw {
                        if b == b'"' || b == b'\\' {
                            out.push(b'\\');
                        }
                        out.push(b);
                    }
                    out.push(b'"');
                }
            }
        }
    }
    out.push(b'}');
    out
}

/// The `pg_type.oid` of the ARRAY whose elements are `v`.
///
/// PostgreSQL has one array type per element type and a client decodes by it,
/// so this decides whether `array_agg(n)` comes back as a list of ints or a
/// list of strings. Taken from the first non-NULL element, which is the same
/// rule `RowDescription` uses for a scalar column; an all-NULL or empty array
/// reports `text[]`, the one every element can be read as.
pub fn array_oid_for(items: &[Value]) -> u32 {
    match items.iter().find_map(|v| v.column_type()) {
        Some(mpedb_types::ColumnType::Int64) => 1016,   // int8[]
        Some(mpedb_types::ColumnType::Float64) => 1022, // float8[]
        Some(mpedb_types::ColumnType::Bool) => 1000,    // bool[]
        Some(mpedb_types::ColumnType::Blob) => 1001,    // bytea[]
        Some(mpedb_types::ColumnType::Timestamp) => 1185, // timestamptz[]
        Some(mpedb_types::ColumnType::Date) => 1182,    // date[]
        Some(mpedb_types::ColumnType::Time) => 1183,    // time[]
        Some(mpedb_types::ColumnType::Numeric) => 1231, // numeric[]
        _ => 1009,                                      // text[]
    }
}

fn hex_digit(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        _ => b'a' + (n - 10),
    }
}

/// PostgreSQL prints a double with the shortest text that round-trips
/// (`extra_float_digits = 1`, the default since PG 12) — which is exactly what
/// Rust's `Display` for `f64` does. The three non-finite spellings are its own.
fn float_text(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Rust prints `10` for 10.0 and PostgreSQL prints `10` too — the decimal
    // point is not added by either.
    f.to_string()
}

/// mpedb's `Timestamp` is microseconds since the Unix epoch, UTC, so it renders
/// as PostgreSQL's `timestamptz` at UTC: `YYYY-MM-DD HH:MM:SS[.ffffff]+00`.
///
/// The fractional part is omitted when it is zero, which is what PostgreSQL
/// does; printing `.000000` is not wrong but it is not what a client's golden
/// output says either.
fn timestamp_text(us: i64) -> String {
    let (secs, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    let base = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if frac == 0 {
        format!("{base}+00")
    } else {
        // Trailing zeros are trimmed, again matching PostgreSQL: 1500 µs prints
        // as `.0015`, not `.001500`.
        let mut f = format!("{frac:06}");
        while f.ends_with('0') {
            f.pop();
        }
        format!("{base}.{f}+00")
    }
}



/// Days-from-civil, inverted — Howard Hinnant's algorithm, which is exact for
/// the whole i64 range and needs no calendar table.
///
/// Written out rather than pulled in: mpedb has no date dependency anywhere,
/// and adding one to a crate whose job is a socket would be the wrong trade.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, h as u32, mi as u32, s as u32)
}

/// Parse a bound parameter's text into the value an mpedb column wants.
///
/// `want` is what the plan says the parameter's type is; `None` means the plan
/// did not constrain it, in which case the text is carried as `Text` and the
/// binder's own coercion decides. Guessing a type from the SHAPE of the text
/// (all digits ⇒ integer) is what makes `'007'` become 7 in some drivers, so it
/// is not done here.
pub fn from_text(raw: Option<&[u8]>, want: Option<mpedb_types::ColumnType>) -> Result<Value, String> {
    use mpedb_types::ColumnType as C;
    let Some(raw) = raw else {
        return Ok(Value::Null);
    };
    let s = std::str::from_utf8(raw).map_err(|_| "parameter is not valid UTF-8".to_string())?;
    Ok(match want {
        None | Some(C::Text) | Some(C::Any) => Value::Text(s.to_string()),
        Some(C::Int64) => Value::Int(
            s.trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid input syntax for type bigint: \"{s}\""))?,
        ),
        Some(C::Float64) => Value::Float(match s.trim() {
            "NaN" => f64::NAN,
            "Infinity" | "inf" => f64::INFINITY,
            "-Infinity" | "-inf" => f64::NEG_INFINITY,
            t => t
                .parse::<f64>()
                .map_err(|_| format!("invalid input syntax for type double precision: \"{s}\""))?,
        }),
        Some(C::Bool) => Value::Bool(parse_bool(s.trim())?),
        Some(C::Blob) => Value::Blob(parse_bytea(s)?),
        Some(C::Timestamp) => Value::Timestamp(parse_timestamp(s.trim())?),
        // Through the shared calendar in `mpedb_types::value`: the store-time
        // coercion and `CAST` parse dates with the same code, and three copies
        // of a leap-year rule is three chances to disagree.
        Some(C::Date) => Value::Date(
            mpedb_types::value::parse_date_text(s)
                .ok_or_else(|| format!("invalid input syntax for type date: \"{s}\""))?,
        ),
        Some(C::Time) => Value::Time(
            mpedb_types::value::parse_time_text(s)
                .ok_or_else(|| format!("invalid input syntax for type time: \"{s}\""))?,
        ),
        Some(C::Numeric) => Value::Numeric(
            mpedb_types::value::parse_numeric(s)
                .ok_or_else(|| format!("invalid input syntax for type numeric: \"{s}\""))?,
        ),
    })
}

/// PostgreSQL accepts a generous set of boolean spellings, and clients do send
/// more than one: psycopg sends `t`/`f`, JDBC sends `TRUE`/`FALSE`, and hand
/// written SQL sends `1`/`0`.
fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Ok(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("invalid input syntax for type boolean: \"{s}\"")),
    }
}

/// `\xDEADBEEF` (hex, the modern default) or the legacy escape format.
fn parse_bytea(s: &str) -> Result<Vec<u8>, String> {
    if let Some(hex) = s.strip_prefix("\\x") {
        if hex.len() % 2 != 0 {
            return Err("invalid hexadecimal data: odd number of digits".into());
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        let b = hex.as_bytes();
        for pair in b.chunks(2) {
            let hi = hex_val(pair[0])?;
            let lo = hex_val(pair[1])?;
            out.push(hi << 4 | lo);
        }
        return Ok(out);
    }
    // Legacy escape format: `\\` is a backslash, `\NNN` is an octal byte, and
    // everything else is itself.
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(c) if c.is_ascii_digit() => {
                let oct = b
                    .get(i + 1..i + 4)
                    .ok_or_else(|| "invalid input syntax for type bytea".to_string())?;
                let mut v = 0u32;
                for d in oct {
                    if !(b'0'..=b'7').contains(d) {
                        return Err("invalid input syntax for type bytea".into());
                    }
                    v = v * 8 + u32::from(d - b'0');
                }
                if v > 255 {
                    return Err("invalid input syntax for type bytea".into());
                }
                out.push(v as u8);
                i += 4;
            }
            _ => return Err("invalid input syntax for type bytea".into()),
        }
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!(
            "invalid hexadecimal digit: \"{}\"",
            c as char
        )),
    }
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.ffffff][±HH[:MM]|Z]`, or a bare date.
///
/// Deliberately narrow: an unrecognised shape is a NAMED error rather than a
/// best guess. PostgreSQL's own `DateStyle` machinery accepts far more (and
/// resolves `01/02/03` differently depending on a session setting), and
/// reproducing THAT ambiguity would be reproducing a footgun.
fn parse_timestamp(s: &str) -> Result<i64, String> {
    let bad = || format!("invalid input syntax for type timestamp with time zone: \"{s}\"");
    let (date, rest) = s.split_once(['T', ' ']).unwrap_or((s, ""));
    let mut dp = date.split('-');
    let y: i64 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let mo: i64 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let d: i64 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return Err(bad());
    }

    // Split the zone off before the time, or `+01` would be read as a field.
    let (time, offset_secs) = split_zone(rest).ok_or_else(bad)?;
    let (mut h, mut mi, mut sec, mut us) = (0i64, 0i64, 0i64, 0i64);
    if !time.is_empty() {
        let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
        let mut tp = hms.split(':');
        h = tp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        mi = tp.next().unwrap_or("0").parse().map_err(|_| bad())?;
        sec = tp.next().unwrap_or("0").parse().map_err(|_| bad())?;
        if tp.next().is_some() {
            return Err(bad());
        }
        if !frac.is_empty() {
            // Pad or truncate to microseconds: `.5` is 500000 µs, and `.1234567`
            // is 123456 (PostgreSQL rounds, but truncating here never invents
            // precision the client did not send).
            let mut f = frac.to_string();
            f.truncate(6);
            while f.len() < 6 {
                f.push('0');
            }
            us = f.parse().map_err(|_| bad())?;
        }
    }
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&sec) {
        return Err(bad());
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    secs.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(us))
        .ok_or_else(bad)
}



/// Split a trailing time zone off, returning the time part and the offset in
/// seconds EAST of UTC. A missing zone reads as UTC, which is what mpedb's
/// `Timestamp` means by construction.
fn split_zone(rest: &str) -> Option<(&str, i64)> {
    if rest.is_empty() {
        return Some(("", 0));
    }
    if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((t, 0));
    }
    // Find a sign that is not the first character, so a negative year (already
    // handled above) cannot be mistaken for a zone.
    let bytes = rest.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let (t, z) = rest.split_at(i);
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let z = &z[1..];
            let (zh, zm) = match z.split_once(':') {
                Some((a, b)) => (a, b),
                None if z.len() == 4 => (&z[..2], &z[2..]),
                None => (z, "0"),
            };
            let zh: i64 = zh.parse().ok()?;
            let zm: i64 = zm.parse().ok()?;
            return Some((t, sign * (zh * 3600 + zm * 60)));
        }
    }
    Some((rest, 0))
}

/// Howard Hinnant's `days_from_civil`, the exact inverse of
/// [`civil_from_unix`]'s date half.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::ColumnType as C;

    fn txt(v: &Value) -> String {
        String::from_utf8(to_text(v).unwrap()).unwrap()
    }

    #[test]
    fn booleans_are_single_letters_not_the_words() {
        // psql prints `t`; a client comparing against 't' gets nothing if this
        // says "true", and nothing looks like an empty table.
        assert_eq!(txt(&Value::Bool(true)), "t");
        assert_eq!(txt(&Value::Bool(false)), "f");
    }

    #[test]
    fn null_is_absent_rather_than_any_string() {
        assert!(to_text(&Value::Null).is_none());
        // …and an empty string is very much present.
        assert_eq!(to_text(&Value::Text(String::new())), Some(Vec::new()));
    }

    #[test]
    fn floats_print_shortest_round_trip_with_postgresqls_special_names() {
        assert_eq!(txt(&Value::Float(10.0)), "10");
        assert_eq!(txt(&Value::Float(0.1)), "0.1");
        assert_eq!(txt(&Value::Float(1.0 / 3.0)), "0.3333333333333333");
        assert_eq!(txt(&Value::Float(f64::NAN)), "NaN");
        assert_eq!(txt(&Value::Float(f64::INFINITY)), "Infinity");
        assert_eq!(txt(&Value::Float(f64::NEG_INFINITY)), "-Infinity");
    }

    #[test]
    fn bytea_uses_the_hex_format_every_current_client_decodes() {
        assert_eq!(txt(&Value::Blob(vec![0xde, 0xad, 0x00, 0xff])), "\\xdead00ff");
        assert_eq!(txt(&Value::Blob(Vec::new())), "\\x");
    }

    #[test]
    fn timestamps_render_at_utc_and_trim_the_fraction() {
        // 2023-11-14T22:13:20Z is 1700000000 s.
        assert_eq!(
            txt(&Value::Timestamp(1_700_000_000_000_000)),
            "2023-11-14 22:13:20+00"
        );
        assert_eq!(
            txt(&Value::Timestamp(1_700_000_000_001_500)),
            "2023-11-14 22:13:20.0015+00"
        );
        // The epoch itself, and a pre-epoch instant — the case where a
        // truncating division instead of a Euclidean one is off by a day.
        assert_eq!(txt(&Value::Timestamp(0)), "1970-01-01 00:00:00+00");
        assert_eq!(
            txt(&Value::Timestamp(-1_000_000)),
            "1969-12-31 23:59:59+00"
        );
    }

    #[test]
    fn timestamp_text_round_trips_through_the_parser() {
        for us in [
            0i64,
            1_700_000_000_000_000,
            -1_000_000,
            -2_208_988_800_000_000, // 1900-01-01
            4_102_444_800_000_000,  // 2100-01-01
            1_700_000_000_001_500,
        ] {
            let s = timestamp_text(us);
            let back = parse_timestamp(&s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(back, us, "{s}");
        }
    }

    #[test]
    fn the_parser_takes_the_spellings_real_clients_send() {
        let ts = |s: &str| parse_timestamp(s).unwrap();
        let want = 1_700_000_000_000_000i64;
        assert_eq!(ts("2023-11-14 22:13:20+00"), want);
        assert_eq!(ts("2023-11-14T22:13:20Z"), want);
        assert_eq!(ts("2023-11-14 22:13:20"), want);
        // An offset is applied, not ignored — the bug that lands every row an
        // hour out and never errors.
        assert_eq!(ts("2023-11-14 23:13:20+01"), want);
        assert_eq!(ts("2023-11-14 23:43:20+01:30"), want);
        assert_eq!(ts("2023-11-14 21:13:20-01"), want);
        // A bare date is midnight UTC.
        assert_eq!(ts("2023-11-14"), 1_699_920_000_000_000);
    }

    #[test]
    fn an_unparseable_timestamp_is_named_rather_than_guessed() {
        for s in ["", "not a date", "2023-13-01", "2023-11-14 25:00:00", "14/11/2023"] {
            assert!(parse_timestamp(s).is_err(), "{s} should not parse");
        }
    }

    #[test]
    fn parameters_are_parsed_to_the_type_the_plan_asks_for_not_guessed_from_shape() {
        // The failure this prevents: `'007'` silently becoming 7 because it
        // looked numeric. The plan decides; the text does not.
        assert_eq!(
            from_text(Some(b"007"), Some(C::Text)).unwrap(),
            Value::Text("007".into())
        );
        assert_eq!(
            from_text(Some(b"007"), Some(C::Int64)).unwrap(),
            Value::Int(7)
        );
        assert_eq!(from_text(Some(b"007"), None).unwrap(), Value::Text("007".into()));
        assert_eq!(from_text(None, Some(C::Int64)).unwrap(), Value::Null);
    }

    #[test]
    fn boolean_parameters_accept_every_spelling_clients_send() {
        for s in ["t", "true", "TRUE", "y", "yes", "on", "1"] {
            assert_eq!(
                from_text(Some(s.as_bytes()), Some(C::Bool)).unwrap(),
                Value::Bool(true),
                "{s}"
            );
        }
        for s in ["f", "false", "FALSE", "n", "no", "off", "0"] {
            assert_eq!(
                from_text(Some(s.as_bytes()), Some(C::Bool)).unwrap(),
                Value::Bool(false),
                "{s}"
            );
        }
        assert!(from_text(Some(b"maybe"), Some(C::Bool)).is_err());
    }

    #[test]
    fn bytea_parses_both_wire_formats_and_round_trips_the_hex_one() {
        assert_eq!(parse_bytea("\\xdead00ff").unwrap(), vec![0xde, 0xad, 0, 0xff]);
        assert_eq!(parse_bytea("\\x").unwrap(), Vec::<u8>::new());
        // Legacy escape format.
        assert_eq!(parse_bytea("ab\\\\cd").unwrap(), b"ab\\cd".to_vec());
        assert_eq!(parse_bytea("\\000\\377").unwrap(), vec![0x00, 0xff]);
        assert!(parse_bytea("\\xabc").is_err()); // odd digits
        assert!(parse_bytea("\\xzz").is_err());

        for bytes in [vec![], vec![0u8], vec![0xff, 0x00, 0x7f, 0x80]] {
            let v = Value::Blob(bytes.clone());
            let s = txt(&v);
            assert_eq!(parse_bytea(&s).unwrap(), bytes);
        }
    }

    #[test]
    fn an_invalid_number_reports_postgresqls_own_error_wording() {
        // Clients match on this text more than anyone would like.
        let e = from_text(Some(b"twelve"), Some(C::Int64)).unwrap_err();
        assert!(e.contains("invalid input syntax for type bigint"), "{e}");
        let e = from_text(Some(b"x"), Some(C::Float64)).unwrap_err();
        assert!(
            e.contains("invalid input syntax for type double precision"),
            "{e}"
        );
    }

    #[test]
    fn float_parameters_accept_the_non_finite_spellings_they_are_printed_as() {
        let f = |s: &str| match from_text(Some(s.as_bytes()), Some(C::Float64)).unwrap() {
            Value::Float(f) => f,
            v => panic!("{v:?}"),
        };
        assert!(f("NaN").is_nan());
        assert_eq!(f("Infinity"), f64::INFINITY);
        assert_eq!(f("-Infinity"), f64::NEG_INFINITY);
    }
}

#[cfg(test)]
mod date_time_numeric_tests {
    use super::*;

    fn txt(v: &Value) -> String {
        String::from_utf8(to_text(v).unwrap()).unwrap()
    }

    /// A `date` prints as a date and a `time` as a clock — the assertion that
    /// was silently false while both were integers.
    #[test]
    fn they_print_the_way_postgresql_prints_them() {
        assert_eq!(txt(&Value::Date(0)), "1970-01-01");
        assert_eq!(txt(&Value::Date(18_263)), "2020-01-02");
        assert_eq!(txt(&Value::Date(-1)), "1969-12-31");
        assert_eq!(txt(&Value::Time(0)), "00:00:00");
        assert_eq!(txt(&Value::Time(11_045_000_000)), "03:04:05");
        assert_eq!(txt(&Value::Time(11_045_001_500)), "03:04:05.0015");
        assert_eq!(txt(&Value::Time(86_399_999_999)), "23:59:59.999999");
        // A numeric crosses as its own digits, with no float in the middle.
        assert_eq!(txt(&Value::Numeric("1.00".into())), "1.00");
        assert_eq!(
            txt(&Value::Numeric("123456789012345678901234567890.5".into())),
            "123456789012345678901234567890.5"
        );
    }

    #[test]
    fn a_client_literal_round_trips_through_the_parser() {
        use mpedb_types::ColumnType as C;
        for (ty, lit) in [
            (C::Date, "2020-01-02"),
            (C::Date, "1970-01-01"),
            (C::Date, "0001-01-01"),
            (C::Time, "03:04:05"),
            (C::Time, "00:00:00"),
            (C::Time, "23:59:59.999999"),
            (C::Time, "03:04:05.0015"),
            (C::Numeric, "1.00"),
            (C::Numeric, "-0.0001"),
            (C::Numeric, "12345678901234567890"),
        ] {
            let v = from_text(Some(lit.as_bytes()), Some(ty))
                .unwrap_or_else(|e| panic!("{lit}: {e}"));
            assert_eq!(txt(&v), lit, "{lit}");
        }
        // `HH:MM` is legal input; it prints back with the seconds PG prints.
        let v = from_text(Some(b"03:04"), Some(mpedb_types::ColumnType::Time)).unwrap();
        assert_eq!(txt(&v), "03:04:00");
        // A numeric's canonical form is what comes back, not the spelling.
        let v = from_text(Some(b"1e3"), Some(mpedb_types::ColumnType::Numeric)).unwrap();
        assert_eq!(txt(&v), "1000");
    }

    /// Bad input is `invalid input syntax for type <t>`, naming the type — the
    /// message PostgreSQL sends and SQLAlchemy's tests match on. Nothing here
    /// may take a leading number and call it a value.
    #[test]
    fn bad_input_is_refused_by_name() {
        use mpedb_types::ColumnType as C;
        for (ty, lit, want) in [
            (C::Date, "2020", "date"),
            (C::Date, "2020-13-01", "date"),
            (C::Date, "abc", "date"),
            (C::Date, "2020-01-02 03:04:05", "date"),
            (C::Time, "3", "time"),
            (C::Time, "25:00:00", "time"),
            (C::Time, "03:04:05+02", "time"),
            (C::Numeric, "1.2.3", "numeric"),
            (C::Numeric, "abc", "numeric"),
        ] {
            let e = from_text(Some(lit.as_bytes()), Some(ty))
                .expect_err(&format!("{lit} should be refused"));
            assert!(
                e.contains("invalid input syntax for type") && e.contains(want),
                "{lit}: {e}"
            );
        }
    }
}

#[cfg(test)]
mod oid_aware_rendering {
    use super::*;

    /// The `RowDescription` and the `DataRow` must agree. Once the description
    /// says `uuid` — which it does as soon as the column's DECLARED type is
    /// read — sending the bytea spelling makes the client raise "badly formed
    /// hexadecimal UUID string".
    #[test]
    fn a_uuid_column_renders_as_a_uuid_not_as_bytea() {
        let raw: Vec<u8> = vec![
            0x8e, 0x20, 0x82, 0xb3, 0x99, 0x60, 0x4d, 0x69, 0x92, 0x5a, 0x0f, 0xa5, 0xb9, 0x78,
            0xc5, 0x4b,
        ];
        let v = Value::Blob(raw.clone());
        assert_eq!(
            String::from_utf8(to_text_as(&v, 2950).unwrap()).unwrap(),
            "8e2082b3-9960-4d69-925a-0fa5b978c54b"
        );
        // …and the SAME bytes under the bytea oid keep the bytea spelling.
        assert_eq!(
            String::from_utf8(to_text_as(&v, 17).unwrap()).unwrap(),
            "\\x8e2082b399604d69925a0fa5b978c54b"
        );
    }

    /// A blob that is not 16 bytes is not a UUID however the description is
    /// labelled; it keeps the bytea spelling rather than being truncated or
    /// padded into one.
    #[test]
    fn a_wrong_length_blob_is_not_dressed_up_as_a_uuid() {
        let v = Value::Blob(vec![1, 2, 3]);
        assert_eq!(to_text_as(&v, 2950), to_text(&v));
        assert_eq!(to_text_as(&Value::Null, 2950), None);
    }
}

#[cfg(test)]
mod array_rendering {
    use super::*;

    fn txt(v: &Value) -> String {
        String::from_utf8(to_text(v).unwrap()).unwrap()
    }

    /// PostgreSQL's array external representation, including the two cases a
    /// naive join gets wrong: an element that must be QUOTED, and a NULL
    /// element, which is the bare word and not the string `"NULL"`.
    #[test]
    fn arrays_render_the_way_postgresql_renders_them() {
        assert_eq!(txt(&Value::List(vec![])), "{}");
        assert_eq!(
            txt(&Value::List(vec![Value::Int(1), Value::Int(2)])),
            "{1,2}"
        );
        assert_eq!(
            txt(&Value::List(vec![
                Value::Text("a".into()),
                Value::Null,
                Value::Text("b, c".into()),
            ])),
            "{a,NULL,\"b, c\"}"
        );
        // The STRING "NULL" is quoted so it cannot be read back as the marker,
        // and an empty string needs quoting to exist at all.
        assert_eq!(
            txt(&Value::List(vec![Value::Text("NULL".into()), Value::Text(String::new())])),
            "{\"NULL\",\"\"}"
        );
        // Quotes and backslashes escape.
        assert_eq!(
            txt(&Value::List(vec![Value::Text("a\"b\\c".into())])),
            "{\"a\\\"b\\\\c\"}"
        );
        // Braces and whitespace force quoting too.
        assert_eq!(txt(&Value::List(vec![Value::Text("{x}".into())])), "{\"{x}\"}");
    }

    /// The element type picks the ARRAY type, because the client decodes by
    /// it: `array_agg(n)` must come back as a list of ints, not of strings.
    #[test]
    fn the_element_type_picks_the_array_oid() {
        assert_eq!(array_oid_for(&[Value::Int(1)]), 1016);
        assert_eq!(array_oid_for(&[Value::Text("a".into())]), 1009);
        assert_eq!(array_oid_for(&[Value::Bool(true)]), 1000);
        // A leading NULL does not decide it; the first typed element does.
        assert_eq!(array_oid_for(&[Value::Null, Value::Int(1)]), 1016);
        // All-NULL and empty fall back to text[], which every element reads as.
        assert_eq!(array_oid_for(&[Value::Null]), 1009);
        assert_eq!(array_oid_for(&[]), 1009);
    }
}
