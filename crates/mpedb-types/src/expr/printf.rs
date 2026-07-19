//! `printf(FORMAT, …)` / `format(FORMAT, …)` — sqlite's C-printf-style string
//! formatter, ported to match sqlite 3.45 **exactly** over its supported
//! specifier set.
//!
//! This is a faithful re-implementation of sqlite's `sqlite3_str_vappendf`
//! (the `SQLITE_PRINTF_SQLFUNC` argument-list path) plus `sqlite3FpDecode`.
//! Two properties are load-bearing:
//!
//! 1. **Determinism across platforms.** sqlite's float decoder has two code
//!    paths — an 80-bit `long double` path (used on x86-64 Linux) and a
//!    portable Dekker double-double path (used where `long double == double`,
//!    e.g. Apple Silicon). They are bit-identical in their output (verified
//!    over thousands of values/precisions), and this port uses the double-double
//!    path so mpedb produces the SAME bytes on every target — a database
//!    requirement, since plans are content-hashed and shared across machines.
//! 2. **sqlite's dialect, not C stdio.** `%c` takes the first character of the
//!    argument's TEXT rendering (not a code point), `%x`/`%o`/`%u` operate on
//!    the 64-bit representation, integer precision zero-pads, `,` is a thousands
//!    separator, `%q`/`%Q`/`%w` are SQL escapes, and float rounding follows
//!    `sqlite3FpDecode` (so `%.0f` of 3.5 is "3", not "4"). Each is verified
//!    against the `sqlite3` 3.45 CLI in `crates/mpedb/tests/printf_fn.rs`.
//!
//! Supported specifiers: `d i u x X o c s q Q w % f e E g G`. Flags:
//! `- + space 0 # , !` plus field width, `.precision`, and `*` (width/precision
//! from the next argument). Any other conversion character HALTS output at that
//! point, exactly as sqlite does for an unrecognized conversion.

use crate::error::{Error, Result};
use crate::value::Value;

/// Safety valve: a field width or precision above this yields a runtime error
/// rather than letting a hostile plan request a multi-gigabyte allocation.
/// sqlite caps at `SQLITE_LIMIT_LENGTH` (~1e9) and raises `SQLITE_TOOBIG`; mpedb
/// caps lower and raises too — the surface is far below any real format string.
const MAX_FIELD: i64 = 10_000_000;

/// sqlite's whitespace set for numeric coercion (`sqlite3Isspace`):
/// space plus `\t \n \v \f \r`.
fn is_space(b: u8) -> bool {
    b == b' ' || (0x09..=0x0d).contains(&b)
}

/// `sqlite3Atoi64`: skip leading whitespace, an optional sign, then decimal
/// digits, stopping at the first non-digit; overflow saturates to
/// `i64::MIN`/`i64::MAX`. Non-numeric text yields 0. (This is what
/// `sqlite3_value_int64` does for a text/blob argument, so `%d` of `'12abc'`
/// is 12 and of `'abc'` is 0.)
fn atoi64(z: &[u8]) -> i64 {
    let mut i = 0;
    while i < z.len() && is_space(z[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < z.len() && (z[i] == b'-' || z[i] == b'+') {
        neg = z[i] == b'-';
        i += 1;
    }
    // Accumulate in u128 with a clamp so 20+ digit inputs can't wrap.
    let cap = (i64::MAX as u128) + 1; // magnitude of i64::MIN
    let mut u: u128 = 0;
    while i < z.len() && z[i].is_ascii_digit() {
        u = u * 10 + (z[i] - b'0') as u128;
        if u > cap {
            u = cap;
        }
        i += 1;
    }
    if u > i64::MAX as u128 {
        if neg {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if neg {
        -(u as i64)
    } else {
        u as i64
    }
}

/// `sqlite3AtoF` (approximated): skip leading whitespace, then take the maximal
/// `[+-]?digits(.digits)?([eE][+-]?digits)?` prefix and parse it; non-numeric
/// text yields 0.0. Matches sqlite for the well-formed numeric text that a `%f`
/// argument would carry, and gives 0.0 for `'abc'` exactly as sqlite does.
pub(crate) fn atof(z: &[u8]) -> f64 {
    let mut i = 0;
    while i < z.len() && is_space(z[i]) {
        i += 1;
    }
    let start = i;
    if i < z.len() && (z[i] == b'+' || z[i] == b'-') {
        i += 1;
    }
    let mut saw_digit = false;
    while i < z.len() && z[i].is_ascii_digit() {
        i += 1;
        saw_digit = true;
    }
    if i < z.len() && z[i] == b'.' {
        i += 1;
        while i < z.len() && z[i].is_ascii_digit() {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return 0.0;
    }
    if i < z.len() && (z[i] == b'e' || z[i] == b'E') {
        let mut j = i + 1;
        if j < z.len() && (z[j] == b'+' || z[j] == b'-') {
            j += 1;
        }
        let mut saw_exp = false;
        while j < z.len() && z[j].is_ascii_digit() {
            j += 1;
            saw_exp = true;
        }
        if saw_exp {
            i = j;
        }
    }
    std::str::from_utf8(&z[start..i])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// `sqlite3_value_int64` for a printf argument.
fn arg_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Bool(b) => *b as i64,
        // Rust's saturating `as` matches `sqlite3RealToI64`: truncate toward
        // zero, NaN → 0, ±inf → i64::MIN/MAX.
        Value::Float(f) => *f as i64,
        Value::Text(s) => atoi64(s.as_bytes()),
        Value::Blob(b) => atoi64(b),
        // No sqlite equivalent; use the raw micros.
        Value::Timestamp(t) => *t,
        Value::Null | Value::List(_) => 0,
    }
}

/// `sqlite3_value_double` for a printf argument.
fn arg_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Bool(b) => *b as u8 as f64,
        Value::Float(f) => *f,
        Value::Text(s) => atof(s.as_bytes()),
        Value::Blob(b) => atof(b),
        Value::Timestamp(t) => *t as f64,
        Value::Null | Value::List(_) => 0.0,
    }
}

/// `sqlite3_value_text` for a printf argument: `None` is sqlite's NULL text
/// pointer (which `%s` renders as empty and `%q`/`%Q`/`%w` render as the NULL
/// word). A float renders through `%!.15g`, matching sqlite's REAL→TEXT.
fn arg_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::Int(i) => Some(i.to_string().into_bytes()),
        // sqlite has no bool; mpedb renders it as its integer, as `||` does.
        Value::Bool(b) => Some((*b as i64).to_string().into_bytes()),
        Value::Float(f) => Some(float_to_text(*f)),
        Value::Blob(b) => Some(b.clone()),
        Value::Timestamp(t) => Some(t.to_string().into_bytes()),
        Value::List(_) => Some(Vec::new()),
    }
}

/// sqlite's REAL→TEXT rendering is `printf("%!.15g", r)`. Shared with
/// `CAST(x AS TEXT/BLOB)` so a real always renders identically everywhere.
pub(crate) fn float_to_text(r: f64) -> Vec<u8> {
    let mut prec = 15;
    render_float(r, FloatKind::Generic, &mut prec, false, true, 0, false, false)
}

/// sqlite's `sqlite3QuoteValue` REAL rendering: `%!.15g`, handed back to
/// `sqlite3AtoF` and — when the double does NOT come back bit-identical —
/// re-rendered as `%!.20e` so the literal round-trips. `CAST(0.1+0.2 AS TEXT)`
/// is `0.3`; `quote(0.1+0.2)` takes the second branch.
///
/// `None` means "that second branch, and mpedb will not guess it". The reason
/// is in sqlite's own `sqlite3FpDecode`: it has TWO implementations, chosen at
/// startup by `sqlite3Config.bUseLongDouble` (`hasHighPrecisionDouble()`).
/// A build whose `long double` is wider than 8 bytes AND works — x86-64 — scales
/// the value with 80-bit arithmetic and hands back **18** significant digits;
/// a build without one (macOS/Apple Silicon, where `long double == double`)
/// takes the Dekker double-double path this module ports and lands on **19**,
/// with a different final digit. At `%!.15g` the two agree, because 15 digits
/// is inside the region both compute identically. At `%!.20e` they do not:
/// x86-64 sqlite prints `quote(0.1+0.2)` as `3.00000000000000044e-01`, and the
/// digits past the 15th are an artifact of ~18 chained `× 10` roundings in a
/// register format Rust cannot even name — not something a second decimal
/// algorithm can reproduce, and not something two sqlite builds agree on.
///
/// So `quote()` is byte-exact on the branch that IS well defined and refuses,
/// by name and with the value, on the branch that is not.
///
/// A non-finite value is NOT refused: `%!.15g` of an infinity is the renderer's
/// platform-independent special case (`Inf` / `-Inf`), and sqlite's `%!.20e`
/// fallback prints the same word, so both branches agree.
pub(crate) fn quote_float(r: f64) -> Option<Vec<u8>> {
    let short = float_to_text(r);
    if !r.is_finite() || atof(&short) == r {
        Some(short)
    } else {
        None
    }
}

// ---- FpDecode (double-double path) ---------------------------------------

/// Decoded floating-point value: sign, the significant decimal digits (ASCII),
/// and the position of the decimal point. Mirrors sqlite's `FpDecode`.
struct Fp {
    neg: bool,
    digits: Vec<u8>,
    /// Location of the decimal point (digits before it).
    idp: i32,
    /// 0 = normal, 1 = Infinity, 2 = NaN.
    special: u8,
}

/// sqlite's `dekkerMul2`: Dekker double-double multiply, `x *= (y + yy)`.
/// The intermediate truncations to binary64 are what make the algorithm exact;
/// Rust's f64 arithmetic already truncates each operation, so no `volatile` is
/// needed. The expression order mirrors the C source verbatim (the final add is
/// IEEE-commutative, so the `assign_op` rewrite would be equivalent, but the
/// literal form is kept for line-for-line comparability with sqlite).
#[allow(clippy::assign_op_pattern)]
fn dekker_mul2(x: &mut [f64; 2], y: f64, yy: f64) {
    let mask = 0xffff_ffff_fc00_0000u64;
    let hx = f64::from_bits(x[0].to_bits() & mask);
    let tx = x[0] - hx;
    let hy = f64::from_bits(y.to_bits() & mask);
    let ty = y - hy;
    let p = hx * hy;
    let q = hx * ty + tx * hy;
    let c = p + q;
    let mut cc = p - c + q + tx * ty;
    cc = x[0] * yy + x[1] * y + cc;
    x[0] = c + cc;
    x[1] = c - x[0];
    x[1] += cc;
}

/// Port of `sqlite3FpDecode` using the portable double-double path.
///
/// `iround`: rounding directive (see the call sites in [`render_float`]).
/// `mxround`: maximum significant digits (16, or 26 for the `!` alt form).
// The scaling constants are sqlite's verbatim double-double error terms; they
// carry more decimal digits than an f64 holds and round (at compile time) to
// exactly the f64 sqlite compiles them to — that exactness is the point, so the
// excessive-precision lint is allowed here deliberately.
#[allow(clippy::excessive_precision)]
fn fp_decode(mut r: f64, mut iround: i32, mxround: i32) -> Fp {
    let neg;
    if r < 0.0 {
        neg = true;
        r = -r;
    } else if r == 0.0 {
        return Fp {
            neg: false,
            digits: vec![b'0'],
            idp: 1,
            special: 0,
        };
    } else {
        neg = false;
    }
    let bits = r.to_bits();
    if (bits >> 52) & 0x7ff == 0x7ff {
        let special = if bits != 0x7ff0_0000_0000_0000 { 2 } else { 1 };
        return Fp {
            neg,
            digits: Vec::new(),
            idp: 0,
            special,
        };
    }

    // Scale r into [1e17, 1e19) with a double-double, tracking the power of ten.
    let mut exp: i32 = 0;
    let mut rr = [r, 0.0f64];
    if rr[0] > 9.223_372_036_854_774_784e18 {
        while rr[0] > 9.223_372_036_854_774_784e118 {
            exp += 100;
            dekker_mul2(&mut rr, 1.0e-100, -1.999_189_980_260_288_361_96e-117);
        }
        while rr[0] > 9.223_372_036_854_774_784e28 {
            exp += 10;
            dekker_mul2(&mut rr, 1.0e-10, -3.643_219_731_549_774_157_9e-27);
        }
        while rr[0] > 9.223_372_036_854_774_784e18 {
            exp += 1;
            dekker_mul2(&mut rr, 1.0e-1, -5.551_115_123_125_782_702_1e-18);
        }
    } else {
        while rr[0] < 9.223_372_036_854_774_784e-83 {
            exp -= 100;
            dekker_mul2(&mut rr, 1.0e100, -1.590_289_110_975_991_804_6e83);
        }
        while rr[0] < 9.223_372_036_854_774_784e7 {
            exp -= 10;
            dekker_mul2(&mut rr, 1.0e10, 0.0);
        }
        while rr[0] < 9.223_372_036_854_774_78e17 {
            exp -= 1;
            dekker_mul2(&mut rr, 1.0e1, 0.0);
        }
    }
    let v: u64 = if rr[1] < 0.0 {
        (rr[0] as u64).wrapping_sub((-rr[1]) as u64)
    } else {
        (rr[0] as u64).wrapping_add(rr[1] as u64)
    };

    // Extract significant digits, filling from the right of the buffer.
    let mut buf = [0u8; 24];
    let mut i: i32 = 23;
    let mut vv = v;
    while vv != 0 {
        buf[i as usize] = (vv % 10) as u8 + b'0';
        i -= 1;
        vv /= 10;
    }
    let mut n: i32 = 23 - i;
    let mut idp: i32 = n + exp;

    if iround < 0 {
        iround = idp - iround;
        if iround == 0 && buf[(i + 1) as usize] >= b'5' {
            iround = 1;
            buf[i as usize] = b'0';
            i -= 1;
            n += 1;
            idp += 1;
        }
    }
    if iround > 0 && (iround < n || n > mxround) {
        if iround > mxround {
            iround = mxround;
        }
        n = iround;
        // z == &buf[i+1]; round to `iround` significant digits.
        if buf[(i + 1 + iround) as usize] >= b'5' {
            let mut j = iround - 1;
            loop {
                let idx = (i + 1 + j) as usize;
                buf[idx] += 1;
                if buf[idx] <= b'9' {
                    break;
                }
                buf[idx] = b'0';
                if j == 0 {
                    buf[i as usize] = b'1';
                    i -= 1;
                    n += 1;
                    idp += 1;
                    break;
                }
                j -= 1;
            }
        }
    }
    // Strip trailing zeros.
    while n > 0 && buf[(i + 1 + (n - 1)) as usize] == b'0' {
        n -= 1;
    }
    let start = (i + 1) as usize;
    Fp {
        neg,
        digits: buf[start..start + n as usize].to_vec(),
        idp,
        special: 0,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FloatKind {
    Float,   // %f
    Exp,     // %e / %E
    Generic, // %g / %G
}

/// Render one floating-point field (sign + digits, no field-width padding, no
/// zero-fill). `upper` selects `E`/`e` for the exponent letter. Mirrors the
/// `etFLOAT`/`etEXP`/`etGENERIC` layout in `sqlite3_str_vappendf`.
#[allow(clippy::too_many_arguments)]
fn render_float(
    value: f64,
    mut kind: FloatKind,
    precision: &mut i32,
    flag_alt: bool,
    flag_altform2: bool,
    flag_prefix: u8,
    cthousand: bool,
    upper: bool,
) -> Vec<u8> {
    if *precision < 0 {
        *precision = 6;
    }
    let iround = match kind {
        FloatKind::Float => -*precision,
        FloatKind::Generic => *precision,
        FloatKind::Exp => *precision + 1,
    };
    let s = fp_decode(value, iround, if flag_altform2 { 26 } else { 16 });

    if s.special != 0 {
        // NaN / Infinity. (mpedb math never yields NaN, and inf is rare.)
        if s.special == 2 {
            return if flag_altform2 {
                b"null".to_vec()
            } else {
                b"NaN".to_vec()
            };
        }
        let mut out = Vec::new();
        if s.neg {
            out.push(b'-');
        } else if flag_prefix != 0 {
            out.push(flag_prefix);
        }
        out.extend_from_slice(b"Inf");
        return out;
    }

    let prefix = if s.neg { b'-' } else { flag_prefix };
    let exp = s.idp - 1;
    if kind == FloatKind::Generic && *precision > 0 {
        *precision -= 1;
    }

    let flag_rtz;
    if kind == FloatKind::Generic {
        flag_rtz = !flag_alt;
        if exp < -4 || exp > *precision {
            kind = FloatKind::Exp;
        } else {
            *precision -= exp;
            kind = FloatKind::Float;
        }
    } else {
        flag_rtz = flag_altform2;
    }
    let mut e2 = if kind == FloatKind::Exp { 0 } else { s.idp - 1 };

    let flag_dp = *precision > 0 || flag_alt || flag_altform2;
    let mut out: Vec<u8> = Vec::new();
    if prefix != 0 {
        out.push(prefix);
    }
    // Digits before the decimal point.
    let n = s.digits.len() as i32;
    let mut j = 0i32;
    if e2 < 0 {
        out.push(b'0');
    } else {
        while e2 >= 0 {
            out.push(if j < n { s.digits[j as usize] } else { b'0' });
            j += 1;
            if cthousand && e2 % 3 == 0 && e2 > 1 {
                out.push(b',');
            }
            e2 -= 1;
        }
    }
    // The decimal point.
    if flag_dp {
        out.push(b'.');
    }
    // Leading zeros after the point but before the first significant digit.
    e2 += 1;
    while e2 < 0 && *precision > 0 {
        out.push(b'0');
        *precision -= 1;
        e2 += 1;
    }
    // Significant digits after the point.
    while *precision > 0 {
        out.push(if j < n { s.digits[j as usize] } else { b'0' });
        j += 1;
        *precision -= 1;
    }
    // Remove trailing zeros and a bare trailing point.
    if flag_rtz && flag_dp {
        while *out.last().unwrap() == b'0' {
            out.pop();
        }
        if *out.last().unwrap() == b'.' {
            if flag_altform2 {
                out.push(b'0');
            } else {
                out.pop();
            }
        }
    }
    // The "eNNN" suffix.
    if kind == FloatKind::Exp {
        let mut exp2 = s.idp - 1;
        out.push(if upper { b'E' } else { b'e' });
        if exp2 < 0 {
            out.push(b'-');
            exp2 = -exp2;
        } else {
            out.push(b'+');
        }
        if exp2 >= 100 {
            out.push((exp2 / 100) as u8 + b'0');
            exp2 %= 100;
        }
        out.push((exp2 / 10) as u8 + b'0');
        out.push((exp2 % 10) as u8 + b'0');
    }
    out
}

// ---- integer rendering ---------------------------------------------------

/// Insert `,` thousands separators into a run of ASCII decimal digits.
fn add_commas(digits: &[u8]) -> Vec<u8> {
    let n = digits.len();
    let mut out = Vec::with_capacity(n + n / 3);
    for (idx, &d) in digits.iter().enumerate() {
        if idx > 0 && (n - idx).is_multiple_of(3) {
            out.push(b',');
        }
        out.push(d);
    }
    out
}

/// Render one integer field (`%d %i %u %x %X %o`) INCLUDING sign and alt-form
/// prefix, but before field-width space padding. `precision` may be raised by
/// the caller for zero-padding.
#[allow(clippy::too_many_arguments)]
fn render_int(
    v: i64,
    base: u64,
    upper: bool,
    signed: bool,
    mut flag_alt: bool,
    flag_zeropad: bool,
    flag_prefix: u8,
    width: i64,
    mut precision: i64,
    cthousand: bool,
    alt_prefix: &[u8],
) -> Vec<u8> {
    let (mut longvalue, prefix): (u64, u8) = if signed {
        if v < 0 {
            (v.unsigned_abs(), b'-')
        } else {
            (v as u64, flag_prefix)
        }
    } else {
        (v as u64, 0)
    };
    if longvalue == 0 {
        flag_alt = false;
    }
    if flag_zeropad && precision < width - (prefix != 0) as i64 {
        precision = width - (prefix != 0) as i64;
    }
    let cset: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut digits: Vec<u8> = Vec::new();
    loop {
        digits.push(cset[(longvalue % base) as usize]);
        longvalue /= base;
        if longvalue == 0 {
            break;
        }
    }
    digits.reverse();
    while (digits.len() as i64) < precision {
        digits.insert(0, b'0');
    }
    let mut body = if cthousand {
        add_commas(&digits)
    } else {
        digits
    };
    let mut out = Vec::with_capacity(body.len() + 3);
    if flag_alt && !alt_prefix.is_empty() {
        out.extend_from_slice(alt_prefix);
    }
    if prefix != 0 {
        out.push(prefix);
    }
    out.append(&mut body);
    out
}

// ---- the formatter --------------------------------------------------------

/// Take the next positional argument (or `None` if exhausted). Mirrors sqlite's
/// `getIntArg`/`getTextArg`, which consume an argument only when one remains.
fn take<'a>(args: &'a [Value], ai: &mut usize) -> Option<&'a Value> {
    let v = args.get(*ai);
    if v.is_some() {
        *ai += 1;
    }
    v
}

fn clamp_field(x: i64) -> Result<i64> {
    if x > MAX_FIELD {
        Err(Error::TypeMismatch(format!(
            "printf(): field width/precision {x} exceeds the maximum {MAX_FIELD}"
        )))
    } else {
        Ok(x)
    }
}

/// The first UTF-8 character of `text`, as its raw bytes (0..=4 bytes). Empty
/// when `text` is empty. Mirrors sqlite's `%c` handling of a text argument.
fn first_utf8_char(text: &[u8]) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let b0 = text[0];
    let mut out = vec![b0];
    if b0 & 0xc0 == 0xc0 {
        let mut k = 1;
        while out.len() < 4 && k < text.len() && text[k] & 0xc0 == 0x80 {
            out.push(text[k]);
            k += 1;
        }
    }
    out
}

/// Append `field` to `out` with sqlite's field-width padding. When
/// `char_width` is set (the `!` alt form and `%c`), the pad count is measured in
/// characters, so field width counts UTF-8 continuation bytes as zero-width.
fn append_with_width(out: &mut Vec<u8>, field: &[u8], mut width: i64, left: bool, char_width: bool) {
    if char_width && width > 0 {
        for &b in field {
            if b & 0xc0 == 0x80 {
                width += 1;
            }
        }
    }
    let pad = width - field.len() as i64;
    if pad > 0 {
        if left {
            out.extend_from_slice(field);
            out.extend(std::iter::repeat_n(b' ', pad as usize));
        } else {
            out.extend(std::iter::repeat_n(b' ', pad as usize));
            out.extend_from_slice(field);
        }
    } else {
        out.extend_from_slice(field);
    }
}

/// Evaluate `printf(FORMAT, …)` / `format(FORMAT, …)`. Runs ahead of the null
/// gate: a NULL format argument (or a missing one) and an EMPTY format string
/// both yield NULL, while individual NULL data arguments are handled per
/// specifier and never propagate.
pub(super) fn sqlite_printf(args: &[Value]) -> Result<Value> {
    let fmt: &[u8] = match args.first() {
        Some(Value::Text(s)) => s.as_bytes(),
        // The binder pins the format to text; a NULL/absent format is NULL.
        _ => return Ok(Value::Null),
    };
    if fmt.is_empty() {
        // sqlite returns NULL for an empty format string (nothing is appended);
        // any format that processes at least one byte returns (possibly empty)
        // text.
        return Ok(Value::Null);
    }

    let mut out: Vec<u8> = Vec::new();
    let mut ai = 1usize; // next positional argument
    let n = fmt.len();
    let mut i = 0usize;

    while i < n {
        if fmt[i] != b'%' {
            let start = i;
            i += 1;
            while i < n && fmt[i] != b'%' {
                i += 1;
            }
            out.extend_from_slice(&fmt[start..i]);
            continue;
        }
        // At a '%'.
        i += 1;
        if i >= n {
            out.push(b'%'); // trailing '%'
            break;
        }

        // Parse flags, width, precision.
        let mut flag_leftjustify = false;
        let mut flag_prefix: u8 = 0;
        let mut flag_alt = false;
        let mut flag_altform2 = false;
        let mut flag_zeropad = false;
        let mut cthousand = false;
        let mut width: i64 = 0;
        let mut precision: i64 = -1;
        let mut done = false;

        while !done && i < n {
            match fmt[i] {
                b'-' => {
                    flag_leftjustify = true;
                    i += 1;
                }
                b'+' => {
                    flag_prefix = b'+';
                    i += 1;
                }
                b' ' => {
                    flag_prefix = b' ';
                    i += 1;
                }
                b'#' => {
                    flag_alt = true;
                    i += 1;
                }
                b'!' => {
                    flag_altform2 = true;
                    i += 1;
                }
                b'0' => {
                    flag_zeropad = true;
                    i += 1;
                }
                b',' => {
                    cthousand = true;
                    i += 1;
                }
                b'l' => {
                    i += 1;
                    if i < n && fmt[i] == b'l' {
                        i += 1;
                    }
                    done = true;
                }
                b'1'..=b'9' => {
                    let mut wx: i64 = 0;
                    while i < n && fmt[i].is_ascii_digit() {
                        wx = (wx * 10 + (fmt[i] - b'0') as i64).min(MAX_FIELD + 1);
                        i += 1;
                    }
                    width = clamp_field(wx)?;
                    if !(i < n && (fmt[i] == b'.' || fmt[i] == b'l')) {
                        done = true;
                    }
                }
                b'*' => {
                    i += 1;
                    let mut w = take(args, &mut ai).map(arg_i64).unwrap_or(0);
                    if w < 0 {
                        flag_leftjustify = true;
                        w = if w >= -2147483647 { -w } else { 0 };
                    }
                    width = clamp_field(w)?;
                    if !(i < n && (fmt[i] == b'.' || fmt[i] == b'l')) {
                        done = true;
                    }
                }
                b'.' => {
                    i += 1;
                    if i < n && fmt[i] == b'*' {
                        i += 1;
                        let p = take(args, &mut ai).map(arg_i64).unwrap_or(0);
                        precision = if p < 0 {
                            if p >= -2147483647 {
                                -p
                            } else {
                                -1
                            }
                        } else {
                            p
                        };
                    } else {
                        let mut px: i64 = 0;
                        while i < n && fmt[i].is_ascii_digit() {
                            px = (px * 10 + (fmt[i] - b'0') as i64).min(MAX_FIELD + 1);
                            i += 1;
                        }
                        precision = px;
                    }
                    if precision >= 0 {
                        precision = clamp_field(precision)?;
                    }
                    if !(i < n && fmt[i] == b'l') {
                        done = true;
                    }
                }
                _ => done = true,
            }
        }

        if i >= n {
            // Format ended mid-specifier: sqlite stops here.
            break;
        }
        let conv = fmt[i];
        i += 1;

        match conv {
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' => {
                let (base, upper, signed, alt_prefix): (u64, bool, bool, &[u8]) = match conv {
                    b'd' | b'i' => (10, false, true, b""),
                    b'u' => (10, false, false, b""),
                    b'x' => (16, false, false, b"0x"),
                    b'X' => (16, true, false, b"0X"),
                    b'o' => (8, false, false, b"0"),
                    _ => unreachable!(),
                };
                // The thousands separator applies to %d/%i/%u only (etRADIX
                // clears it).
                let comma = cthousand && base == 10;
                let v = take(args, &mut ai).map(arg_i64).unwrap_or(0);
                let field = render_int(
                    v,
                    base,
                    upper,
                    signed,
                    flag_alt,
                    flag_zeropad,
                    flag_prefix,
                    width,
                    precision,
                    comma,
                    alt_prefix,
                );
                append_with_width(&mut out, &field, width, flag_leftjustify, false);
            }
            b'f' | b'e' | b'E' | b'g' | b'G' => {
                let kind = match conv {
                    b'f' => FloatKind::Float,
                    b'e' | b'E' => FloatKind::Exp,
                    _ => FloatKind::Generic,
                };
                let upper = conv == b'E' || conv == b'G';
                let r = take(args, &mut ai).map(arg_f64).unwrap_or(0.0);
                let mut prec = precision as i32;
                let mut field = render_float(
                    r,
                    kind,
                    &mut prec,
                    flag_alt,
                    flag_altform2,
                    flag_prefix,
                    cthousand,
                    upper,
                );
                // Zero-fill: insert '0' after the sign to reach the field width.
                if flag_zeropad && !flag_leftjustify && (field.len() as i64) < width {
                    let has_sign = matches!(field.first(), Some(&(b'-' | b'+' | b' ')));
                    let insert_at = has_sign as usize;
                    let pad = width as usize - field.len();
                    for _ in 0..pad {
                        field.insert(insert_at, b'0');
                    }
                }
                append_with_width(&mut out, &field, width, flag_leftjustify, false);
            }
            b'c' => {
                let ch = take(args, &mut ai)
                    .and_then(arg_text)
                    .map(|t| first_utf8_char(&t))
                    .unwrap_or_default();
                // precision > 1 repeats the character.
                let copies = if precision > 1 { precision as usize } else { 1 };
                let mut field = Vec::with_capacity(ch.len() * copies);
                for _ in 0..copies {
                    field.extend_from_slice(&ch);
                }
                append_with_width(&mut out, &field, width, flag_leftjustify, true);
            }
            b's' => {
                let s = take(args, &mut ai).and_then(arg_text).unwrap_or_default();
                let field: Vec<u8> = if precision >= 0 {
                    let end = string_prefix_len(&s, precision as usize, flag_altform2);
                    s[..end].to_vec()
                } else {
                    s
                };
                append_with_width(&mut out, &field, width, flag_leftjustify, flag_altform2);
            }
            b'q' | b'Q' | b'w' => {
                let q = if conv == b'w' { b'"' } else { b'\'' };
                let opt = take(args, &mut ai).and_then(arg_text);
                let isnull = opt.is_none();
                let escarg: Vec<u8> = opt.unwrap_or_else(|| {
                    if conv == b'Q' {
                        b"NULL".to_vec()
                    } else {
                        b"(NULL)".to_vec()
                    }
                });
                let need_quote = !isnull && conv == b'Q';
                let used = if precision >= 0 {
                    string_prefix_len(&escarg, precision as usize, flag_altform2)
                } else {
                    escarg.len()
                };
                let mut field = Vec::with_capacity(used + 2);
                if need_quote {
                    field.push(q);
                }
                for &ch in &escarg[..used] {
                    field.push(ch);
                    if ch == q {
                        field.push(q);
                    }
                }
                if need_quote {
                    field.push(q);
                }
                append_with_width(&mut out, &field, width, flag_leftjustify, flag_altform2);
            }
            b'%' => {
                append_with_width(&mut out, b"%", width, flag_leftjustify, false);
            }
            b'n' => {
                // sqlite's %n writes the running length to a pointer arg and
                // outputs nothing; in a SQL context there is no pointer, so it
                // is a no-op producing no output.
            }
            // Any other conversion character is unrecognized: sqlite stops
            // appending at this point and returns what it has so far.
            _ => break,
        }
    }

    Ok(Value::Text(String::from_utf8_lossy(&out).into_owned()))
}

/// Byte length of the first `limit` characters of `s`. When `char_based` is set
/// (the `!` alt form), a character is a whole UTF-8 sequence; otherwise a
/// character is one byte (sqlite's default `%s` precision counts bytes).
fn string_prefix_len(s: &[u8], limit: usize, char_based: bool) -> usize {
    if !char_based {
        return limit.min(s.len());
    }
    let mut bi = 0;
    let mut cnt = 0;
    while cnt < limit && bi < s.len() {
        bi += 1;
        if s[bi - 1] & 0xc0 == 0xc0 {
            while bi < s.len() && s[bi] & 0xc0 == 0x80 {
                bi += 1;
            }
        }
        cnt += 1;
    }
    bi
}
