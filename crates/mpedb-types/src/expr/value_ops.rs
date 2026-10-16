//! Scalar arithmetic, exact decimal operations, casts, affinity, and concatenation.

use super::*;

pub(super) fn arith(op: Instr, a: Value, b: Value) -> Result<Value> {
    use Value::*;
    match (&a, &b) {
        (Null, _) | (_, Null) => return Ok(Null),
        _ => {}
    }
    // An int meeting a float happens only through a DYNAMICALLY typed operand
    // (`ColumnType::Any` — a mixed CASE/COALESCE arm, a host UDF result, a
    // typeless column): every statically-typed plan had its int side coerced
    // by the binder (`ToFloat`) before an Arith was emitted. sqlite's rule is
    // that if either operand is REAL the arithmetic is real, so widen per
    // VALUE here — exactly where sqlite decides it. This cannot change any
    // previously-accepted answer: a mixed pair used to be the TypeMismatch
    // fall-through below.
    let (a, b) = match (a, b) {
        (Int(x), Float(y)) => (Float(x as f64), Float(y)),
        (Float(x), Int(y)) => (Float(x), Float(y as f64)),
        other => other,
    };
    match (a, b) {
        (Int(x), Int(y)) => Ok(match op {
            Instr::Add => Int(x.checked_add(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Sub => Int(x.checked_sub(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Mul => Int(x.checked_mul(y).ok_or(Error::ArithmeticOverflow)?),
            // sqlite yields NULL on division/modulo by zero (not an error). A
            // non-zero divisor still guards the one i64::MIN / -1 overflow that
            // `checked_div`/`checked_rem` report — that stays an error.
            Instr::Div if y == 0 => Null,
            Instr::Div => Int(x.checked_div(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::Mod if y == 0 => Null,
            Instr::Mod => Int(x.checked_rem(y).ok_or(Error::ArithmeticOverflow)?),
            // PostgreSQL raises 22012 rather than yielding NULL.
            Instr::DivStrict | Instr::ModStrict if y == 0 => return Err(Error::DivisionByZero),
            Instr::DivStrict => Int(x.checked_div(y).ok_or(Error::ArithmeticOverflow)?),
            Instr::ModStrict => Int(x.checked_rem(y).ok_or(Error::ArithmeticOverflow)?),
            _ => unreachable!(),
        }),
        (Float(x), Float(y)) => Ok(match op {
            Instr::Add => Float(x + y),
            Instr::Sub => Float(x - y),
            Instr::Mul => Float(x * y),
            // sqlite yields NULL on division/modulo by zero (not an error).
            Instr::Div if y == 0.0 => Null,
            Instr::Div => Float(x / y),
            Instr::Mod if y == 0.0 => Null,
            Instr::Mod => Float(x % y),
            // The float path raises too. PostgreSQL's `1.0 / 0` is an error,
            // not an infinity — IEEE division is not what SQL asked for.
            Instr::DivStrict | Instr::ModStrict if y == 0.0 => return Err(Error::DivisionByZero),
            Instr::DivStrict => Float(x / y),
            Instr::ModStrict => Float(x % y),
            _ => unreachable!(),
        }),
        // Exact decimal arithmetic. An INT joins it as a scale-0 decimal
        // rather than dragging the decimal down to a float: `numeric + 1` is
        // numeric in PostgreSQL, and going through f64 is the precision loss
        // the type exists to prevent. A float operand never reaches here —
        // `unify_types` widened the decimal to f64 first, which is
        // PostgreSQL's rule too.
        (Numeric(x), Numeric(y)) => numeric_arith(op, &x, &y),
        (Numeric(x), Int(y)) => numeric_arith(op, &x, &y.to_string()),
        (Int(x), Numeric(y)) => numeric_arith(op, &x.to_string(), &y),
        (a, b) => Err(Error::TypeMismatch(format!(
            "arithmetic on {} and {} (binder should have coerced)",
            a.type_name(),
            b.type_name()
        ))),
    }
}

/// `+ - * / %` on two exact decimals.
///
/// Both sides are scaled to a common i128 mantissa, so the result is exact for
/// `+ - *` and correctly rounded for `/`. A value needing more than 38
/// significant digits, or a product that overflows, is a NAMED refusal rather
/// than a silent fall back to f64 — this type exists so the digits survive,
/// and quietly losing them here would be the wrong answer it was added to
/// prevent.
pub(crate) fn numeric_arith(op: Instr, a: &str, b: &str) -> Result<Value> {
    let too_big = || {
        Error::Unsupported(format!(
            "exact decimal arithmetic on `{a}` and `{b}` needs more than 38 \
             significant digits — CAST to double precision to compute it \
             approximately"
        ))
    };
    let (ma, sa) = crate::value::decimal_parts(a).ok_or_else(too_big)?;
    let (mb, sb) = crate::value::decimal_parts(b).ok_or_else(too_big)?;
    // Align to a common scale for everything except multiplication, whose
    // scale is the SUM and needs no alignment.
    let scale = sa.max(sb);
    let lift = |m: i128, s: u32| -> Option<i128> { m.checked_mul(10i128.checked_pow(scale - s)?) };
    let text = |m: i128, s: u32| Value::Numeric(crate::value::decimal_text(m, s));

    Ok(match op {
        Instr::Add => {
            let (x, y) = (
                lift(ma, sa).ok_or_else(too_big)?,
                lift(mb, sb).ok_or_else(too_big)?,
            );
            text(x.checked_add(y).ok_or_else(too_big)?, scale)
        }
        Instr::Sub => {
            let (x, y) = (
                lift(ma, sa).ok_or_else(too_big)?,
                lift(mb, sb).ok_or_else(too_big)?,
            );
            text(x.checked_sub(y).ok_or_else(too_big)?, scale)
        }
        Instr::Mul => text(ma.checked_mul(mb).ok_or_else(too_big)?, sa + sb),
        // sqlite yields NULL on a zero divisor; the PostgreSQL dialect's
        // strict pair raises, one arm below.
        Instr::Div | Instr::Mod if mb == 0 => Value::Null,
        Instr::DivStrict | Instr::ModStrict if mb == 0 => return Err(Error::DivisionByZero),
        Instr::Div | Instr::DivStrict => {
            // PostgreSQL computes a quotient to at least 16 significant
            // digits; 16 past the common scale is at least that and is
            // deterministic, which matters because two processes must agree.
            let rs = scale + 16;
            let num = lift(ma, sa)
                .and_then(|x| x.checked_mul(10i128.checked_pow(rs)?))
                .ok_or_else(too_big)?;
            let den = lift(mb, sb).ok_or_else(too_big)?;
            // Round half away from zero, as PostgreSQL's numeric division does.
            let q = num / den;
            let r = (num % den).abs();
            let q = if r * 2 >= den.abs() {
                q + if (num < 0) != (den < 0) { -1 } else { 1 }
            } else {
                q
            };
            text(q, rs)
        }
        Instr::Mod | Instr::ModStrict => {
            let (x, y) = (
                lift(ma, sa).ok_or_else(too_big)?,
                lift(mb, sb).ok_or_else(too_big)?,
            );
            text(x % y, scale)
        }
        _ => unreachable!(),
    })
}

/// `CAST` semantics — sqlite's permissive, affinity-based conversion. NULL
/// casts to NULL for every affinity. The conversions match sqlite 3.45 exactly
/// (differential-tested in `crates/mpedb/tests/cast_affinity.rs`):
///
/// - **Integer**: real truncates toward zero (saturating, so NaN→0, ±inf→i64
///   min/max); text/blob parse a leading *integer* prefix (`'12ab'`→12,
///   `'1e3'`→1 — stops at `e`, `'abc'`→0); bool→0/1; timestamp→its micros.
/// - **Real**: int/bool/timestamp widen to f64; text/blob parse a leading
///   *float* prefix (`'1e3'`→1000.0, `'abc'`→0.0).
/// - **Text**: int/bool/timestamp/real render as sqlite text (real via
///   `%!.15g`); blob is reinterpreted as its bytes — refused only when those
///   bytes are not valid UTF-8 (mpedb `Text` is a Rust `String`; the one
///   deviation from sqlite, which keeps raw bytes).
/// - **Blob**: the value's *text rendering* as bytes (`90`→`x'3930'`), or a
///   blob unchanged.
/// - **Numeric**: an already-typed int/real is left as-is (a real stays real
///   even when integral); text/blob become an int when the whole string is a
///   pure `i64` or the parsed value is integral with `|v| < 2^51`, else a real
///   — sqlite's `NUMERIC` affinity.
pub(super) fn cast_value(v: Value, aff: Affinity) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match aff {
        Affinity::Integer => Value::Int(to_integer(&v)),
        Affinity::Real => Value::Float(to_real(&v)),
        Affinity::Numeric => to_numeric(v),
        Affinity::Text => match v {
            Value::Text(s) => Value::Text(s),
            Value::Blob(b) => Value::Text(String::from_utf8(b).map_err(|_| {
                Error::TypeMismatch(
                    "CAST of a non-UTF-8 BLOB to TEXT is not representable in mpedb".into(),
                )
            })?),
            other => Value::Text(render_scalar_text(&other)),
        },
        Affinity::Blob => match v {
            Value::Blob(b) => Value::Blob(b),
            Value::Text(s) => Value::Blob(s.into_bytes()),
            other => Value::Blob(render_scalar_text(&other).into_bytes()),
        },
    })
}

/// PostgreSQL's `CAST`: produce the target type exactly, or raise.
///
/// The message is PostgreSQL's own (`invalid input syntax for type date:
/// "2020"`), because that is what a client's error handling matches on and
/// what a user reading a log expects to see.
///
/// Only the types where sqlite's affinity rule gives a WRONG answer are
/// handled here; the binder sends everything else down [`cast_value`], so this
/// changes nothing that was already right.
/// A `CAST` to a PostgreSQL type by OID — the dispatcher behind
/// [`Instr::CastPg`].
///
/// It exists because the mpedb type is not enough to choose a PARSE: `uuid`
/// and `bytea` are both `Blob`, and reading a UUID's text as bytea (or the
/// other way) is a wrong answer, not an error.
///
/// Only the types whose text form sqlite's affinity rule gets WRONG are
/// handled strictly here; everything else falls back to that rule, which
/// already agrees with PostgreSQL for it. Adding a type to this list is how a
/// new strictness lands, and the list is short on purpose — each entry is a
/// measured divergence, not a guess.
pub(crate) fn cast_pg(v: &Value, oid: u32) -> Result<Value> {
    let Some(ty) = crate::pgtype::by_oid(oid) else {
        return Err(Error::Internal(format!(
            "CAST to unknown PostgreSQL oid {oid}"
        )));
    };
    if v.is_null() {
        return Ok(Value::Null);
    }
    let bad = || {
        Error::TypeMismatch(format!(
            "invalid input syntax for type {}: \"{}\"",
            ty.name,
            render_scalar_text(v)
        ))
    };
    match ty.name {
        // A UUID is 16 BYTES, and its text is hex with optional dashes —
        // `'6f8b9c1e-…'::uuid` used to be the integer 6.
        "uuid" => match v {
            Value::Blob(b) if b.len() == 16 => Ok(v.clone()),
            Value::Text(s) => parse_uuid(s).map(Value::Blob).ok_or_else(bad),
            _ => Err(bad()),
        },
        "bytea" => match v {
            Value::Blob(_) => Ok(v.clone()),
            Value::Text(s) => parse_bytea(s).map(Value::Blob).ok_or_else(bad),
            _ => Err(bad()),
        },
        _ => match ty.mpedb {
            Some(
                m @ (ColumnType::Date
                | ColumnType::Time
                | ColumnType::Timestamp
                | ColumnType::Numeric),
            ) => cast_typed(v, m),
            // Everything else keeps sqlite's affinity rule, which already
            // gives PostgreSQL's answer for it.
            _ => cast_value(v.clone(), Affinity::from_type_name(ty.name)),
        },
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (dashes optional, any case) as the
/// 16 bytes PostgreSQL stores. Strict: a wrong length or a non-hex digit is
/// `None`, never a prefix.
fn parse_uuid(s: &str) -> Option<Vec<u8>> {
    let hex: String = s.trim().chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..16)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// `\xDEADBEEF` (the modern default) or the legacy escape format.
fn parse_bytea(s: &str) -> Option<Vec<u8>> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("\\x") {
        if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
            .collect();
    }
    // Escape format: `\\` is a backslash, `\NNN` an octal byte, else itself.
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if i + 1 < b.len() && b[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else if i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let oct = std::str::from_utf8(&b[i + 1..i + 4]).ok()?;
            out.push(u8::from_str_radix(oct, 8).ok()?);
            i += 4;
        } else {
            return None;
        }
    }
    Some(out)
}

pub(crate) fn cast_typed(v: &Value, ty: ColumnType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let bad = |v: &Value| {
        Error::TypeMismatch(format!(
            "invalid input syntax for type {}: \"{}\"",
            ty.name(),
            render_scalar_text(v)
        ))
    };
    Ok(match (ty, v) {
        // Already the type: a cast to what you are is the identity, never a
        // round trip through text that could lose a digit.
        (ColumnType::Date, Value::Date(_))
        | (ColumnType::Time, Value::Time(_))
        | (ColumnType::Timestamp, Value::Timestamp(_))
        | (ColumnType::Numeric, Value::Numeric(_)) => v.clone(),

        (ColumnType::Date, Value::Text(s)) => {
            Value::Date(crate::value::parse_date_text(s).ok_or_else(|| bad(v))?)
        }
        // PostgreSQL's `timestamp::date` truncates toward the past, which is
        // what `div_euclid` does for negative instants too.
        (ColumnType::Date, Value::Timestamp(us)) => Value::Date(us.div_euclid(86_400_000_000)),

        (ColumnType::Time, Value::Text(s)) => {
            Value::Time(crate::value::parse_time_text(s).ok_or_else(|| bad(v))?)
        }
        (ColumnType::Time, Value::Timestamp(us)) => Value::Time(us.rem_euclid(86_400_000_000)),

        (ColumnType::Timestamp, Value::Text(s)) => {
            Value::Timestamp(crate::value::parse_timestamp_text(s).ok_or_else(|| bad(v))?)
        }
        // Midnight UTC, which is what PostgreSQL's `date::timestamp` gives.
        (ColumnType::Timestamp, Value::Date(d)) => {
            Value::Timestamp(d.checked_mul(86_400_000_000).ok_or_else(|| bad(v))?)
        }

        (ColumnType::Numeric, Value::Text(s)) => {
            Value::Numeric(crate::value::parse_numeric(s).ok_or_else(|| bad(v))?)
        }
        (ColumnType::Numeric, Value::Int(i)) => Value::Numeric(i.to_string()),
        (ColumnType::Numeric, Value::Bool(b)) => Value::Numeric(i64::from(*b).to_string()),
        // Through the shortest text that round-trips the float, which is every
        // digit it actually held — going via an integer would truncate.
        (ColumnType::Numeric, Value::Float(f)) => {
            Value::Numeric(crate::value::parse_numeric(&f.to_string()).ok_or_else(|| bad(v))?)
        }

        // Everything else is a cast PostgreSQL does not define either
        // (`bytea::date`, `date::time`); say so rather than invent one.
        _ => return Err(bad(v)),
    })
}

/// sqlite whitespace for numeric-prefix skipping: space, tab, LF, FF, CR.
fn is_sql_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

/// A non-blob, non-text scalar rendered to sqlite text (the shared path for
/// TEXT and BLOB affinity). Reals use `%!.15g`; bools render as their integer.
fn render_scalar_text(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Float(f) => {
            String::from_utf8(printf::float_to_text(*f)).expect("float_to_text is ASCII")
        }
        Value::Timestamp(t) => t.to_string(),
        // A date/time renders as the integer it is: this is the sqlite CAST
        // path, whose job is the affinity conversion, not a calendar. The
        // PostgreSQL rendering (`2020-01-02`) belongs on the wire, where the
        // type OID says how to read it.
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        // A NUMERIC is already canonical text — rendering it any other way
        // would be the float round trip the type exists to avoid.
        Value::Numeric(n) => n.clone(),
        Value::Text(s) => s.clone(),
        // Blob is handled by the caller (bytes are copied directly, not via
        // this text path); a List never reaches CAST.
        Value::Blob(_) | Value::Null | Value::List(_) => String::new(),
    }
}

/// INTEGER-affinity conversion of a non-NULL value.
fn to_integer(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        // `f as i64` saturates (NaN→0, ±inf→bounds) and truncates toward zero.
        Value::Float(f) => *f as i64,
        Value::Bool(b) => *b as i64,
        Value::Timestamp(t) | Value::Date(t) | Value::Time(t) => *t,
        // Its digits, by the same prefix rule text uses — a numeric IS a
        // number, so this is a conversion and not a parse of something else.
        Value::Numeric(n) => int_prefix(n.as_bytes()),
        Value::Text(s) => int_prefix(s.as_bytes()),
        Value::Blob(b) => int_prefix(b),
        Value::Null | Value::List(_) => 0,
    }
}

/// REAL-affinity conversion of a non-NULL value.
pub(super) fn to_real(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => *b as u8 as f64,
        Value::Timestamp(t) | Value::Date(t) | Value::Time(t) => *t as f64,
        Value::Numeric(n) => float_prefix(n.as_bytes()),
        Value::Text(s) => float_prefix(s.as_bytes()),
        Value::Blob(b) => float_prefix(b),
        Value::Null | Value::List(_) => 0.0,
    }
}

/// NUMERIC-affinity conversion of a non-NULL value. Already-numeric values are
/// left untouched (a real stays real); text/blob are parsed by `bytes_to_numeric`.
fn to_numeric(v: Value) -> Value {
    match v {
        Value::Int(_) | Value::Float(_) => v,
        Value::Bool(b) => Value::Int(b as i64),
        Value::Timestamp(t) | Value::Date(t) | Value::Time(t) => Value::Int(t),
        // Left ALONE. Numeric affinity turns text into a number and would
        // turn this one into a float, which is exactly the exactness this
        // type carries — it is already numeric, so there is nothing to apply.
        Value::Numeric(_) => v,
        Value::Text(s) => bytes_to_numeric(s.as_bytes()),
        Value::Blob(b) => bytes_to_numeric(&b),
        Value::Null | Value::List(_) => v,
    }
}

/// Parse a leading integer prefix (sqlite `sqlite3Atoi64`): optional leading
/// whitespace, optional sign, then decimal digits, stopping at the first
/// non-digit. Overflow saturates to the i64 bounds. No digits → 0.
fn int_prefix(b: &[u8]) -> i64 {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut acc: i64 = 0;
    let mut saw = false;
    while i < b.len() && b[i].is_ascii_digit() {
        saw = true;
        let d = (b[i] - b'0') as i64;
        acc = match acc.checked_mul(10).and_then(|a| a.checked_add(d)) {
            Some(a) => a,
            None => return if neg { i64::MIN } else { i64::MAX },
        };
        i += 1;
    }
    if !saw {
        return 0;
    }
    if neg {
        acc.checked_neg().unwrap_or(i64::MIN)
    } else {
        acc
    }
}

/// The end index of the leading float token (sqlite `sqlite3AtoF` grammar):
/// `[ws][sign]digits[.digits][(e|E)[sign]digits]`, also accepting `.5`. Returns
/// `(numeric_start, numeric_end, saw_digit)` — the slice `b[start..end]` is
/// ASCII and parseable by Rust's `f64::from_str`.
fn float_token(b: &[u8]) -> (usize, usize, bool) {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let start = i;
    if matches!(b.get(i), Some(b'+') | Some(b'-')) {
        i += 1;
    }
    let mut saw = false;
    while i < b.len() && b[i].is_ascii_digit() {
        saw = true;
        i += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            saw = true;
            i += 1;
        }
    }
    // An exponent only counts when the mantissa had a digit AND the exponent
    // itself has one (`'1e'` parses as `1`, not `1e<nothing>`).
    if saw && matches!(b.get(i), Some(b'e') | Some(b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+') | Some(b'-')) {
            j += 1;
        }
        if matches!(b.get(j), Some(d) if d.is_ascii_digit()) {
            j += 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    (start, i, saw)
}

/// Parse a leading float prefix; no numeric prefix → 0.0.
pub(super) fn float_prefix(b: &[u8]) -> f64 {
    let (start, end, saw) = float_token(b);
    if !saw {
        return 0.0;
    }
    // The token is pure ASCII by construction.
    std::str::from_utf8(&b[start..end])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// sqlite's NUMERIC parse of text/blob bytes: an integer when the whole
/// (trimmed) string is a pure `i64`, OR when the parsed float is integral and
/// `|v| < 2^51` (sqlite's `sqlite3RealSameAsInt` bound); else a real.
fn bytes_to_numeric(b: &[u8]) -> Value {
    if let Some(i) = full_i64(b) {
        return Value::Int(i);
    }
    let r = float_prefix(b);
    let i = r as i64; // saturating
    const LIM: i64 = 1 << 51;
    if i as f64 == r && i > -LIM && i < LIM {
        Value::Int(i)
    } else {
        Value::Float(r)
    }
}

/// `Some(i)` iff the whole string (ignoring leading/trailing whitespace) is a
/// valid `i64` integer literal — sqlite's `sqlite3Atoi64` returning 0. Any
/// non-whitespace trailing byte, `.`/`e`, or overflow → `None`.
fn full_i64(b: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < b.len() && is_sql_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digit_start = i;
    // Accumulate the magnitude in u64, as sqlite's `sqlite3Atoi64` does, so the
    // i64::MIN magnitude (2^63, one past i64::MAX) still parses as an integer.
    let mut acc: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        let d = (b[i] - b'0') as u64;
        acc = acc.checked_mul(10).and_then(|a| a.checked_add(d))?;
        i += 1;
    }
    if i == digit_start {
        return None; // no digits
    }
    let mut j = i;
    while j < b.len() && is_sql_space(b[j]) {
        j += 1;
    }
    if j != b.len() {
        return None; // trailing non-whitespace (incl. '.'/'e')
    }
    if neg {
        // Magnitude fits iff acc <= 2^63; acc == 2^63 negates to i64::MIN.
        (acc <= (1u64 << 63)).then(|| acc.wrapping_neg() as i64)
    } else {
        (acc <= i64::MAX as u64).then_some(acc as i64)
    }
}

/// sqlite's **store-time** affinity conversion: what happens to a value on its
/// way INTO a column. This is NOT [`cast_value`], and the two deliberately
/// disagree — verified against sqlite3 3.45.1:
///
/// | expression                       | `CAST` | stored in a `NUMERIC` column |
/// |----------------------------------|--------|------------------------------|
/// | `'12abc'`                        | `12`   | `'12abc'` (TEXT)             |
/// | `'1e18'`                         | `1.0e18` (real) | `1000000000000000000` (int) |
/// | `x'3132'`                        | `12`   | `x'3132'` (BLOB)             |
///
/// `CAST` parses a numeric PREFIX and always yields a number; store-time
/// affinity converts only when the conversion is lossless and reversible, and
/// otherwise leaves the value exactly as it was. Mixing them up is a wrong
/// answer, which is why they are separate functions rather than one with a
/// flag.
///
/// This is applied ONLY to a [`ColumnType::Any`](crate::ColumnType::Any)
/// column, which is mpedb's per-value column and therefore the only one that
/// can hold whatever the conversion produces. Every rigid column REFUSES a
/// mismatched value instead — narrower than sqlite, never a different answer —
/// so [`ColumnDef::converts_on_store`](crate::ColumnDef::converts_on_store) is
/// what decides, not this function.
///
/// The five affinities, all differentially checked against sqlite3 3.45.1:
///
/// * **NUMERIC** and **INTEGER** — identical at store time (sqlite's own docs
///   say so, and `applyAffinity` really does take the same branch for both). A
///   TEXT value is converted only if the WHOLE string (ignoring surrounding
///   whitespace) is a well-formed decimal literal: `'1.50'`→`1.5`,
///   `'0012'`→`12`, `'1e3'`→`1000`, `'  7  '`→`7`. `'abc'`, `''`, `'12abc'`,
///   `'0x10'`, `'1_000'`, `'inf'` and `'2024-01-01'` all stay TEXT. A pure
///   integer literal that fits an `i64` keeps its EXACT value even past 2^53
///   (`'9007199254740993'`, which no `f64` could hold); everything else becomes
///   a real and then collapses back to an integer only if it round-trips
///   through `i64` exactly. A REAL value collapses to an integer under that
///   same rule (`1.0`→`1`, `-0.0`→`0`). Blobs are NOT parsed (unlike `CAST`).
/// * **REAL** — the NUMERIC rule, and then anything that ended up an integer is
///   widened to a real, so the column reads back `12.0`, not `12`.
/// * **TEXT** — a number is rendered to its sqlite text spelling (`1`→`'1'`,
///   `1.5`→`'1.5'`, `%!.15g`); text, blobs and NULL are left alone.
/// * **BLOB** — sqlite's "NONE" affinity: nothing is converted, ever. This is
///   the affinity of a column with NO declared type.
///
/// mpedb's own `Bool` and `Timestamp` values pass through unchanged under every
/// affinity: sqlite has no such storage classes to agree or disagree with, no
/// differential test can observe them, and an `Any` column has always stored
/// them as themselves.
pub fn store_affinity(aff: Affinity, v: Value) -> Value {
    match aff {
        // NONE/BLOB affinity converts nothing — the typeless column.
        Affinity::Blob => v,
        Affinity::Integer | Affinity::Numeric => numerify(v),
        Affinity::Real => match numerify(v) {
            Value::Int(i) => Value::Float(i as f64),
            other => other,
        },
        Affinity::Text => match v {
            Value::Int(_) | Value::Float(_) => Value::Text(render_scalar_text(&v)),
            other => other,
        },
    }
}

/// The shared NUMERIC/INTEGER store-time step (`applyNumericAffinity(_, 1)`).
fn numerify(v: Value) -> Value {
    match v {
        Value::Text(s) => match numeric_from_full_text(s.as_bytes()) {
            Some(n) => n,
            None => Value::Text(s),
        },
        Value::Float(f) => match real_as_int(f) {
            Some(i) => Value::Int(i),
            None => Value::Float(f),
        },
        other => other,
    }
}

/// `Some(number)` iff the whole byte string is a well-formed numeric literal —
/// sqlite's `sqlite3AtoF` returning > 0 — else `None`, meaning "leave it TEXT".
fn numeric_from_full_text(b: &[u8]) -> Option<Value> {
    let (start, end, saw) = float_token(b);
    if !saw {
        return None;
    }
    // `sqlite3AtoF` reports a *partial* parse (rc <= 0) for anything with
    // extraneous text, and `applyNumericAffinity` then stores the string
    // unchanged. Leading whitespace was already skipped by `float_token`.
    if b[end..].iter().any(|&c| !is_sql_space(c)) {
        return None;
    }
    // The token is pure ASCII by construction; an out-of-range exponent parses
    // to ±inf, exactly as sqlite's strtod does.
    let r: f64 = std::str::from_utf8(&b[start..end]).ok()?.parse().ok()?;
    // sqlite's `alsoAnInt`: a pure integer literal keeps its exact i64 value,
    // which is how `'9007199254740993'` survives a round trip that `f64` would
    // round to ...992.
    if let Some(i) = full_i64(b) {
        return Some(Value::Int(i));
    }
    Some(match real_as_int(r) {
        Some(i) => Value::Int(i),
        None => Value::Float(r),
    })
}

/// sqlite's `sqlite3VdbeIntegerAffinity`: a real becomes an integer only when
/// the real→int→real round trip is exact AND the integer is neither `i64`
/// extreme (sqlite ticket #3922 — `9223372036854775807.0` is not a value any
/// `f64` names, so calling it that integer would not be reversible).
fn real_as_int(r: f64) -> Option<i64> {
    // sqlite3RealToI64 clamps rather than invoking UB on an out-of-range cast.
    let ix = if r <= i64::MIN as f64 {
        i64::MIN
    } else if r >= i64::MAX as f64 {
        i64::MAX
    } else {
        r as i64
    };
    // NaN fails this comparison, so a NaN stays a real.
    (r == ix as f64 && ix > i64::MIN && ix < i64::MAX).then_some(ix)
}

/// The [`Instr::ConcatN`] body: byte-level `||` over the whole chain.
/// NULL propagates; text and BLOB contribute their raw bytes; ints and bools
/// render (sqlite's rule); floats are refused (formatting unpinned) — same
/// set the binder admits. Valid UTF-8 -> Text; anything else is the decode
/// error, with the LOSSY rendering sqlite consumers display (each invalid
/// sequence as U+FFFD — measured against CPython's own message generator).
pub(super) fn concat_bytes_n(ops: Vec<Value>) -> Result<Value> {
    if ops.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let mut bytes: Vec<u8> = Vec::new();
    for v in ops {
        match v {
            Value::Text(s) => bytes.extend_from_slice(s.as_bytes()),
            Value::Blob(b) => bytes.extend_from_slice(&b),
            Value::Int(i) => bytes.extend_from_slice(i.to_string().as_bytes()),
            Value::Bool(b) => bytes.push(if b { b'1' } else { b'0' }),
            v => {
                return Err(Error::TypeMismatch(format!(
                    "|| cannot render {} as text",
                    v.type_name()
                )))
            }
        }
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(Value::Text(s)),
        Err(e) => Err(Error::NonUtf8Concat {
            column: None,
            text: String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }),
    }
}

/// `||` semantics: NULL propagates; ints and bools render as text (sqlite's
/// rule); floats are refused until their text formatting is pinned down.
pub(super) fn concat_value(a: Value, b: Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let as_text = |v: Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s),
            Value::Int(i) => Ok(i.to_string()),
            Value::Bool(b) => Ok((b as i64).to_string()),
            v => Err(Error::TypeMismatch(format!(
                "|| cannot render {} as text",
                v.type_name()
            ))),
        }
    };
    let mut s = as_text(a)?;
    s.push_str(&as_text(b)?);
    Ok(Value::Text(s))
}
