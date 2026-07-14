//! **Adapting a loose source's data to a strict target** (task #26.3).
//!
//! sqlite takes anything: a declared type is an affinity, so `INTEGER` may hold
//! the text `"42"`, `BOOLEAN` may hold `1`, and a timestamp may be an ISO
//! string. PostgreSQL takes none of it. mpedb sits in the middle holding the one
//! thing that makes the gap crossable — the source schema recorded at import
//! ([`crate::state::TableMap`]) — so it can say what each value *should* be and
//! propose how to get there.
//!
//! ## The line this module will not cross
//!
//! `CAST` is not adaptation. SQL's cast does **prefix parsing**: `'007abc'`
//! becomes `7`, `'12x'` becomes `12`, `'abc'` becomes `0`. Every one of those is
//! data loss wearing a success message, and DESIGN-MIRROR §4.5 already ruled it
//! out as a default ("silently corrupts data like '007abc'→7, do not default").
//!
//! So: **a coercion here parses the WHOLE value or refuses.** `"42"` → `42`;
//! `"007abc"` → [`Adaptation::Impossible`], never `7`.
//!
//! ## Three verdicts, because two would lie
//!
//! - [`Adaptation::Exact`] — reversible, nothing lost (`"42"` → `42`).
//! - [`Adaptation::Lossy`] — a real coercion that discards something
//!   (truncating to `varchar(4)`, rounding to `numeric(_,2)`). Legal, sometimes
//!   wanted, and never automatic: it needs its own opt-in beyond "adapt".
//! - [`Adaptation::Impossible`] — no coercion exists. `2147483648` does not fit
//!   `int4` by any honest means; the only answers are widen the column, drop the
//!   row, or stop.
//!
//! Collapsing Lossy into Exact would let `--adapt` silently truncate someone's
//! data; collapsing it into Impossible would refuse migrations that are fine.

use mpedb_types::{ColumnType, Value};

use crate::state::ColumnMap;

/// What can be done to make one value fit its recorded source column.
#[derive(Debug, Clone, PartialEq)]
pub enum Adaptation {
    /// Already fits — nothing to do.
    Fine,
    /// Coercible with nothing lost; the value it becomes.
    Exact(Value),
    /// Coercible only by discarding something. Carries the result AND what it
    /// costs, so the report can say it out loud.
    Lossy(Value, String),
    /// No coercion exists. Carries why.
    Impossible(String),
}

/// Parse a whole string as an integer — **no prefix parsing**. Leading/trailing
/// whitespace is forgiven (it is not data); anything else is not.
fn whole_int(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

fn whole_float(s: &str) -> Option<f64> {
    let f: f64 = s.trim().parse().ok()?;
    // NaN/inf round-trip badly and PG's float8 has its own spellings; leave them
    // to the target rather than inventing a mapping here.
    f.is_finite().then_some(f)
}

/// sqlite has no bool: apps store 0/1, and — less happily — 't'/'true'/'yes'.
/// Accept only unambiguous spellings; `"2"` or `"maybe"` is not a boolean, and
/// guessing would be exactly the silent corruption this module exists to avoid.
fn whole_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "t" | "true" | "yes" | "y" => Some(true),
        "0" | "f" | "false" | "no" | "n" => Some(false),
        _ => None,
    }
}

/// ISO-8601 → microseconds since the Unix epoch, UTC. Deliberately narrow:
/// `YYYY-MM-DDTHH:MM:SS[.fff][Z]` and `YYYY-MM-DD`. A date-parsing library would
/// accept far more, and every extra format it guesses is a chance to be
/// confidently wrong about someone's timestamps.
fn iso_to_micros(s: &str) -> Option<i64> {
    let t = s.trim();
    let (date, rest) = t.split_once(['T', ' ']).unwrap_or((t, ""));
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let (mut h, mut mi, mut sec, mut micros) = (0i64, 0i64, 0i64, 0i64);
    if !rest.is_empty() {
        let r = rest.trim_end_matches('Z');
        // an explicit offset needs real timezone maths: refuse rather than
        // silently treat +02:00 as UTC
        if r.contains('+') || r.rfind('-').is_some_and(|i| i > 0) {
            return None;
        }
        let mut p = r.split(':');
        h = p.next()?.parse().ok()?;
        mi = p.next()?.parse().ok()?;
        if let Some(s_part) = p.next() {
            match s_part.split_once('.') {
                Some((whole, frac)) => {
                    sec = whole.parse().ok()?;
                    let f: String = frac.chars().take(6).collect();
                    micros = f.parse::<i64>().ok()? * 10i64.pow(6 - f.len() as u32);
                }
                None => sec = s_part.parse().ok()?,
            }
        }
        if p.next().is_some() || h > 23 || mi > 59 || sec > 60 {
            return None;
        }
    }
    Some(days_from_civil(y, mo, da) * 86_400_000_000 + (h * 3600 + mi * 60 + sec) * 1_000_000 + micros)
}

/// Days since 1970-01-01 (Howard Hinnant's civil_from_days inverse) — exact for
/// the proleptic Gregorian calendar, no floating point.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn typmod_n(declared: &str) -> Option<i64> {
    let open = declared.find('(')?;
    let close = declared.rfind(')')?;
    declared[open + 1..close].split(',').next()?.trim().parse().ok()
}

fn base_of(declared: &str) -> String {
    declared
        .find('(')
        .map_or(declared, |i| &declared[..i])
        .trim()
        .to_ascii_lowercase()
}

/// What it would take to make `v` fit `c`'s recorded source column.
pub fn adapt(c: &ColumnMap, v: &Value) -> Adaptation {
    if v.is_null() {
        return if c.not_null {
            // A NULL is not a type problem, it is a missing fact. Inventing a
            // default here would fabricate data.
            Adaptation::Impossible(format!(
                "column `{}` is NOT NULL at the source and this row has no value",
                c.source_name
            ))
        } else {
            Adaptation::Fine
        };
    }

    let base = base_of(&c.source_type);

    // --- narrowing integers: no coercion can make it fit
    if matches!(base.as_str(), "int2" | "smallint" | "int4" | "integer" | "int") {
        let (lo, hi) = if base.starts_with("int2") || base == "smallint" {
            (i16::MIN as i64, i16::MAX as i64)
        } else {
            (i32::MIN as i64, i32::MAX as i64)
        };
        if let Value::Int(i) = v {
            if *i < lo || *i > hi {
                return Adaptation::Impossible(format!(
                    "{i} does not fit {} ({lo}..={hi}) — widen the column, drop the row, or stop",
                    c.source_type
                ));
            }
        }
    }

    // --- type drift: the value is not what the column was mapped as
    if let Some(actual) = v.column_type() {
        if actual != c.mapped {
            return coerce_to(c, v, c.mapped);
        }
    }

    // --- right type, but too big for the declared width
    if let (Value::Text(s), Some(n)) = (v, typmod_n(&c.source_type)) {
        if matches!(base.as_str(), "varchar" | "character varying" | "bpchar" | "character")
            && s.chars().count() as i64 > n
        {
            let cut: String = s.chars().take(n as usize).collect();
            return Adaptation::Lossy(
                Value::Text(cut),
                format!(
                    "truncates {} characters to {} — the discarded tail is gone for good",
                    s.chars().count(),
                    n
                ),
            );
        }
    }
    Adaptation::Fine
}

/// Coerce a drifted value to the column's mapped type, whole-value only.
fn coerce_to(c: &ColumnMap, v: &Value, want: ColumnType) -> Adaptation {
    let refuse = |why: &str| {
        Adaptation::Impossible(format!(
            "{} `{}` cannot become {} for source column `{}`: {why}",
            v.type_name(),
            v,
            want,
            c.source_type
        ))
    };
    match (v, want) {
        (Value::Text(s), ColumnType::Int64) => match whole_int(s) {
            Some(i) => Adaptation::Exact(Value::Int(i)),
            // The CAST trap, refused by name: '007abc' is not 7.
            None => refuse("not an integer in full (a partial parse would be data loss, not a cast)"),
        },
        (Value::Text(s), ColumnType::Float64) => match whole_float(s) {
            Some(f) => Adaptation::Exact(Value::Float(f)),
            None => refuse("not a finite number in full"),
        },
        (Value::Text(s), ColumnType::Bool) => match whole_bool(s) {
            Some(b) => Adaptation::Exact(Value::Bool(b)),
            None => refuse("not an unambiguous boolean spelling"),
        },
        (Value::Text(s), ColumnType::Timestamp) => match iso_to_micros(s) {
            Some(us) => Adaptation::Exact(Value::Timestamp(us)),
            None => refuse("not a UTC ISO-8601 timestamp (an explicit offset needs a real timezone conversion)"),
        },
        (Value::Int(i), ColumnType::Bool) => match i {
            0 => Adaptation::Exact(Value::Bool(false)),
            1 => Adaptation::Exact(Value::Bool(true)),
            _ => refuse("only 0 and 1 are booleans"),
        },
        (Value::Int(i), ColumnType::Float64) => Adaptation::Exact(Value::Float(*i as f64)),
        (Value::Int(i), ColumnType::Text) => Adaptation::Exact(Value::Text(i.to_string())),
        (Value::Int(us), ColumnType::Timestamp) => Adaptation::Exact(Value::Timestamp(*us)),
        (Value::Float(f), ColumnType::Int64) => {
            if f.fract() == 0.0 && f.is_finite() && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                Adaptation::Exact(Value::Int(*f as i64))
            } else if f.is_finite() {
                Adaptation::Lossy(
                    Value::Int(f.trunc() as i64),
                    format!("drops the fractional part of {f}"),
                )
            } else {
                refuse("not finite")
            }
        }
        (Value::Bool(b), ColumnType::Int64) => Adaptation::Exact(Value::Int(i64::from(*b))),
        (Value::Timestamp(us), ColumnType::Int64) => Adaptation::Exact(Value::Int(*us)),
        // Blobs are bytes with no agreed text encoding, and text→blob would have
        // to invent one. Refuse rather than pick.
        (_, ColumnType::Blob) | (Value::Blob(_), _) => {
            refuse("blobs have no lossless textual coercion")
        }
        _ => refuse("no defined coercion"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MapPolicy;

    fn col(source_type: &str, mapped: ColumnType, not_null: bool) -> ColumnMap {
        ColumnMap {
            source_name: "c".into(),
            source_type: source_type.into(),
            not_null,
            generated: false,
            identity: false,
            unique: false,
            mapped,
            policy: MapPolicy::Widened,
        }
    }

    /// The rule the module exists for: a coercion parses the WHOLE value or
    /// refuses. SQL's CAST would answer 7 to every one of these.
    #[test]
    fn never_prefix_parses_like_cast() {
        let c = col("INTEGER", ColumnType::Int64, false);
        for junk in ["007abc", "12x", "abc", "1 2", "", "3.9", "0x10", "1,000"] {
            let a = adapt(&c, &Value::Text(junk.into()));
            assert!(
                matches!(a, Adaptation::Impossible(_)),
                "`{junk}` must be refused, not silently coerced — got {a:?}"
            );
        }
        // ...while a whole, clean integer is exact
        assert_eq!(
            adapt(&c, &Value::Text(" 42 ".into())),
            Adaptation::Exact(Value::Int(42)),
            "surrounding whitespace is not data"
        );
    }

    #[test]
    fn loose_sqlite_shapes_coerce_exactly() {
        assert_eq!(
            adapt(&col("BOOLEAN", ColumnType::Bool, false), &Value::Int(1)),
            Adaptation::Exact(Value::Bool(true))
        );
        assert_eq!(
            adapt(&col("BOOLEAN", ColumnType::Bool, false), &Value::Text("TRUE".into())),
            Adaptation::Exact(Value::Bool(true))
        );
        // 2 is not a boolean, and guessing would be the whole disease
        assert!(matches!(
            adapt(&col("BOOLEAN", ColumnType::Bool, false), &Value::Int(2)),
            Adaptation::Impossible(_)
        ));
        assert_eq!(
            adapt(&col("REAL", ColumnType::Float64, false), &Value::Text("1.5".into())),
            Adaptation::Exact(Value::Float(1.5))
        );
    }

    #[test]
    fn iso_timestamps_parse_and_offsets_are_refused() {
        let c = col("DATETIME", ColumnType::Timestamp, false);
        // 2023-11-14T22:13:20Z == 1700000000 s
        assert_eq!(
            adapt(&c, &Value::Text("2023-11-14T22:13:20Z".into())),
            Adaptation::Exact(Value::Timestamp(1_700_000_000_000_000))
        );
        assert_eq!(
            adapt(&c, &Value::Text("1970-01-01".into())),
            Adaptation::Exact(Value::Timestamp(0))
        );
        assert_eq!(
            adapt(&c, &Value::Text("2023-11-14 22:13:20.5".into())),
            Adaptation::Exact(Value::Timestamp(1_700_000_000_500_000))
        );
        // an explicit offset needs real timezone maths — refuse, do not assume UTC
        assert!(matches!(
            adapt(&c, &Value::Text("2023-11-14T22:13:20+02:00".into())),
            Adaptation::Impossible(_)
        ));
        for junk in ["14/11/2023", "2023-13-01", "yesterday", "2023-11-14T99:00:00"] {
            assert!(
                matches!(adapt(&c, &Value::Text(junk.into())), Adaptation::Impossible(_)),
                "`{junk}` must not parse"
            );
        }
    }

    /// Lossy must stay its own verdict: folding it into Exact lets --adapt
    /// silently truncate; folding it into Impossible refuses fine migrations.
    #[test]
    fn lossy_coercions_are_reported_as_lossy_not_exact() {
        let a = adapt(&col("VARCHAR(4)", ColumnType::Text, false), &Value::Text("toolong".into()));
        match a {
            Adaptation::Lossy(Value::Text(s), why) => {
                assert_eq!(s, "tool");
                assert!(why.contains("gone for good"), "{why}");
            }
            other => panic!("expected Lossy, got {other:?}"),
        }
        // 42.0 -> 42 loses nothing; 42.5 -> 42 does
        let ic = col("INTEGER", ColumnType::Int64, false);
        assert_eq!(adapt(&ic, &Value::Float(42.0)), Adaptation::Exact(Value::Int(42)));
        assert!(matches!(adapt(&ic, &Value::Float(42.5)), Adaptation::Lossy(Value::Int(42), _)));
    }

    /// Some things no coercion can fix, and pretending otherwise is worse than
    /// stopping.
    #[test]
    fn impossible_stays_impossible() {
        // no honest way to fit int4
        assert!(matches!(
            adapt(&col("int4", ColumnType::Int64, false), &Value::Int(2_147_483_648)),
            Adaptation::Impossible(_)
        ));
        // a missing NOT NULL value is a missing fact, not a type problem
        assert!(matches!(
            adapt(&col("text", ColumnType::Text, true), &Value::Null),
            Adaptation::Impossible(_)
        ));
        // a nullable NULL is simply fine
        assert_eq!(adapt(&col("text", ColumnType::Text, false), &Value::Null), Adaptation::Fine);
    }

    #[test]
    fn well_typed_values_need_no_adaptation() {
        assert_eq!(
            adapt(&col("int8", ColumnType::Int64, false), &Value::Int(i64::MAX)),
            Adaptation::Fine
        );
        assert_eq!(
            adapt(&col("VARCHAR(8)", ColumnType::Text, false), &Value::Text("short".into())),
            Adaptation::Fine
        );
    }

    #[test]
    fn civil_days_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }
}
