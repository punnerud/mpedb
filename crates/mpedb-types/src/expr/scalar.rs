//! Built-in scalar functions: the [`ScalarFn`] enum and the
//! NULL-propagating [`call_scalar`] dispatch (plus `typeof`, which does not).

use super::printf::sqlite_printf;
use super::*;

/// The built-in scalar functions. Deliberately a closed enum rather than a name
/// lookup: the id is what goes in the plan bytes, so it must be stable and
/// exhaustively decodable — an unknown id is [`Error::Corrupt`], never a
/// silently-missing function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScalarFn {
    Lower = 1,
    Upper = 2,
    Length = 3,
    Trim = 4,
    Abs = 5,
    Round = 6,
    Substr = 7,
    Replace = 8,
    Ltrim = 9,
    Rtrim = 10,
    Instr = 11,
    Sqrt = 12,
    Pow = 13,
    Sign = 14,
    Ceil = 15,
    Floor = 16,
    /// `char(X1, …, Xn)` — the string of the given Unicode code points.
    /// Variadic (0..=255 args); NULL-propagating like every other scalar here
    /// (sqlite instead treats a NULL argument as code point 0 — a documented gap).
    Char = 17,
    /// `unicode(x)` — the Unicode code point of the FIRST character of `x`
    /// (NULL for the empty string).
    Unicode = 18,
    /// `hex(x)` — uppercase hexadecimal of the argument's bytes (text/blob).
    Hex = 19,
    /// `typeof(x)` — the datatype name of `x`. The one scalar that must NOT
    /// null-propagate: `typeof(NULL)` is the text `'null'`, so it is handled
    /// ahead of the null gate in [`call_scalar`].
    Typeof = 20,
    // --- Math functions (sqlite 3.45 built-ins). Each takes number(s) and
    // returns a Float64 (int in → float out), NULL-propagating like the rest,
    // and models its sound domain on sqlite: an out-of-domain result is NULL.
    /// `exp(x)` — e raised to `x`.
    Exp = 21,
    /// `ln(x)` — natural logarithm; NULL for `x <= 0` (sqlite).
    Ln = 22,
    /// `log10(x)` / `log(x)` — base-10 logarithm; NULL for `x <= 0` (sqlite).
    Log10 = 23,
    /// `log2(x)` — base-2 logarithm; NULL for `x <= 0` (sqlite).
    Log2 = 24,
    /// `log(b, x)` — logarithm of `x` in base `b`. NULL unless `b > 1` and
    /// `x > 0` (sqlite requires ln(b) > 0, i.e. b > 1).
    LogBase = 25,
    /// `sin(x)` — sine (radians).
    Sin = 26,
    /// `cos(x)` — cosine (radians).
    Cos = 27,
    /// `tan(x)` — tangent (radians).
    Tan = 28,
    /// `asin(x)` — arcsine; NULL outside `[-1, 1]` (domain error → NaN → NULL).
    Asin = 29,
    /// `acos(x)` — arccosine; NULL outside `[-1, 1]`.
    Acos = 30,
    /// `atan(x)` — arctangent.
    Atan = 31,
    /// `atan2(y, x)` — angle of the point `(x, y)` from the positive x-axis.
    Atan2 = 32,
    /// `sinh(x)` — hyperbolic sine.
    Sinh = 33,
    /// `cosh(x)` — hyperbolic cosine.
    Cosh = 34,
    /// `tanh(x)` — hyperbolic tangent.
    Tanh = 35,
    /// `radians(x)` — degrees → radians.
    Radians = 36,
    /// `degrees(x)` — radians → degrees.
    Degrees = 37,
    /// `pi()` — the constant π. The one 0-argument scalar.
    Pi = 38,
    /// `mod(x, y)` — floating-point remainder `x - y*trunc(x/y)` (sqlite); a
    /// zero divisor yields NULL (NaN → NULL), NOT the `%` operator's error.
    Mod = 39,
    /// `trunc(x)` — truncate toward zero. Type-PRESERVING like `ceil`/`floor`
    /// (sqlite: an integer stays an integer, a float truncates to a float).
    Trunc = 40,
    /// `printf(FORMAT, …)` / `format(FORMAT, …)` — sqlite's C-printf-style string
    /// formatter. Variadic; the first argument is the format string and the rest
    /// are its data arguments, coerced per-specifier at RUNTIME (an arg used with
    /// `%d` is cast to integer, with `%s` to text, following sqlite's rules). The
    /// one scalar besides `typeof` that does NOT null-propagate: a NULL data
    /// argument is handled per specifier (`%s` of NULL is empty, `%d` of NULL is
    /// 0), and only a NULL/empty FORMAT yields NULL. See [`super::printf`].
    Printf = 41,
    /// `quote(X)` — the SQL literal that denotes `X`, exactly as sqlite's
    /// `quoteFunc` writes it. NULL → the literal text `NULL` (so, like `typeof`
    /// and `printf`, it must NOT null-propagate), text → single-quoted with
    /// `''` doubling, integer → the plain digits, real → `%!.15g` with sqlite's
    /// `%!.20e` round-trip fallback, blob → `X'…'` uppercase hex.
    Quote = 42,
    /// `strftime(FORMAT, TIMESTRING)` — sqlite's time formatter, restricted to
    /// the ISO-8601 time strings and the specifier set mpedb can reproduce
    /// BYTE-for-byte; anything outside that is a clean error naming the
    /// offending specifier or time string, never a guessed value.
    /// See [`super::datetime`].
    Strftime = 43,
    /// `json(X)` — validate `X` as RFC 8259 JSON text and return it minified
    /// (all whitespace outside strings removed, every token's spelling kept).
    /// See [`super::json`].
    Json = 44,
}

impl ScalarFn {
    pub(super) fn from_tag(t: u8) -> Result<ScalarFn> {
        Ok(match t {
            1 => ScalarFn::Lower,
            2 => ScalarFn::Upper,
            3 => ScalarFn::Length,
            4 => ScalarFn::Trim,
            5 => ScalarFn::Abs,
            6 => ScalarFn::Round,
            7 => ScalarFn::Substr,
            8 => ScalarFn::Replace,
            9 => ScalarFn::Ltrim,
            10 => ScalarFn::Rtrim,
            11 => ScalarFn::Instr,
            12 => ScalarFn::Sqrt,
            13 => ScalarFn::Pow,
            14 => ScalarFn::Sign,
            15 => ScalarFn::Ceil,
            16 => ScalarFn::Floor,
            17 => ScalarFn::Char,
            18 => ScalarFn::Unicode,
            19 => ScalarFn::Hex,
            20 => ScalarFn::Typeof,
            21 => ScalarFn::Exp,
            22 => ScalarFn::Ln,
            23 => ScalarFn::Log10,
            24 => ScalarFn::Log2,
            25 => ScalarFn::LogBase,
            26 => ScalarFn::Sin,
            27 => ScalarFn::Cos,
            28 => ScalarFn::Tan,
            29 => ScalarFn::Asin,
            30 => ScalarFn::Acos,
            31 => ScalarFn::Atan,
            32 => ScalarFn::Atan2,
            33 => ScalarFn::Sinh,
            34 => ScalarFn::Cosh,
            35 => ScalarFn::Tanh,
            36 => ScalarFn::Radians,
            37 => ScalarFn::Degrees,
            38 => ScalarFn::Pi,
            39 => ScalarFn::Mod,
            40 => ScalarFn::Trunc,
            41 => ScalarFn::Printf,
            42 => ScalarFn::Quote,
            43 => ScalarFn::Strftime,
            44 => ScalarFn::Json,
            other => return Err(Error::Corrupt(format!("unknown scalar function {other}"))),
        })
    }

    /// Allowed argument counts. Checked at verify time so `eval` can index the
    /// popped args without re-checking.
    pub fn arity_ok(self, argc: u8) -> bool {
        match self {
            ScalarFn::Lower | ScalarFn::Upper | ScalarFn::Length | ScalarFn::Abs
            | ScalarFn::Unicode | ScalarFn::Hex | ScalarFn::Typeof | ScalarFn::Quote
            | ScalarFn::Json => argc == 1,
            // sqlite's strftime is `(FORMAT, TIMESTRING, modifier…)`. mpedb
            // accepts the arity so the refusal can NAME the modifiers rather
            // than report a bare arity mismatch (see `call_scalar`).
            ScalarFn::Strftime => argc >= 2,
            ScalarFn::Round | ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => {
                argc == 1 || argc == 2
            }
            ScalarFn::Sqrt | ScalarFn::Sign | ScalarFn::Ceil | ScalarFn::Floor => argc == 1,
            ScalarFn::Substr => argc == 2 || argc == 3,
            ScalarFn::Instr | ScalarFn::Pow => argc == 2,
            ScalarFn::Replace => argc == 3,
            // char() is variadic: 0..=255 code points (the u8 argc caps it).
            ScalarFn::Char => true,
            // printf()/format() are variadic; the format string is required, so
            // at least one argument.
            ScalarFn::Printf => argc >= 1,
            // Math: one-argument transcendentals and trunc.
            ScalarFn::Exp | ScalarFn::Ln | ScalarFn::Log10 | ScalarFn::Log2
            | ScalarFn::Sin | ScalarFn::Cos | ScalarFn::Tan | ScalarFn::Asin
            | ScalarFn::Acos | ScalarFn::Atan | ScalarFn::Sinh | ScalarFn::Cosh
            | ScalarFn::Tanh | ScalarFn::Radians | ScalarFn::Degrees | ScalarFn::Trunc => {
                argc == 1
            }
            // Math: two-argument.
            ScalarFn::LogBase | ScalarFn::Atan2 | ScalarFn::Mod => argc == 2,
            // `pi()` is the one nullary scalar.
            ScalarFn::Pi => argc == 0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ScalarFn::Lower => "lower",
            ScalarFn::Upper => "upper",
            ScalarFn::Length => "length",
            ScalarFn::Trim => "trim",
            ScalarFn::Abs => "abs",
            ScalarFn::Round => "round",
            ScalarFn::Substr => "substr",
            ScalarFn::Replace => "replace",
            ScalarFn::Ltrim => "ltrim",
            ScalarFn::Rtrim => "rtrim",
            ScalarFn::Instr => "instr",
            ScalarFn::Sqrt => "sqrt",
            ScalarFn::Pow => "pow",
            ScalarFn::Sign => "sign",
            ScalarFn::Ceil => "ceil",
            ScalarFn::Floor => "floor",
            ScalarFn::Char => "char",
            ScalarFn::Unicode => "unicode",
            ScalarFn::Hex => "hex",
            ScalarFn::Typeof => "typeof",
            ScalarFn::Exp => "exp",
            ScalarFn::Ln => "ln",
            ScalarFn::Log10 => "log10",
            ScalarFn::Log2 => "log2",
            ScalarFn::LogBase => "log",
            ScalarFn::Sin => "sin",
            ScalarFn::Cos => "cos",
            ScalarFn::Tan => "tan",
            ScalarFn::Asin => "asin",
            ScalarFn::Acos => "acos",
            ScalarFn::Atan => "atan",
            ScalarFn::Atan2 => "atan2",
            ScalarFn::Sinh => "sinh",
            ScalarFn::Cosh => "cosh",
            ScalarFn::Tanh => "tanh",
            ScalarFn::Radians => "radians",
            ScalarFn::Degrees => "degrees",
            ScalarFn::Pi => "pi",
            ScalarFn::Mod => "mod",
            ScalarFn::Trunc => "trunc",
            ScalarFn::Printf => "printf",
            ScalarFn::Quote => "quote",
            ScalarFn::Strftime => "strftime",
            ScalarFn::Json => "json",
        }
    }
}

/// Evaluate a scalar function. `validate` already proved the arity, so the
/// indexing here is total.
///
/// Every one of these is NULL-propagating: any NULL argument yields NULL,
/// without looking at the others. That is the SQL rule, and it is why the
/// null-tolerant functions (`coalesce`, `nullif`) are NOT here — they are
/// compiled to control flow instead, precisely because they must NOT propagate.
pub(super) fn call_scalar(f: ScalarFn, args: &[Value]) -> Result<Value> {
    // `typeof` is the one scalar here that must SEE a NULL rather than
    // propagate it: `typeof(NULL)` is the text `'null'`, not NULL. So it runs
    // ahead of the null gate every function below relies on.
    if matches!(f, ScalarFn::Typeof) {
        return Ok(Value::Text(sqlite_typeof(&args[0]).to_string()));
    }
    // printf/format also run ahead of the null gate: a NULL data argument is
    // formatted per specifier, not propagated, and only a NULL/empty FORMAT
    // yields NULL. (The whole arg vector is handed to the formatter.)
    if matches!(f, ScalarFn::Printf) {
        return sqlite_printf(args);
    }
    // `quote(NULL)` is the four-character text `NULL` — the SQL literal that
    // denotes NULL — so it too runs ahead of the null gate.
    if matches!(f, ScalarFn::Quote) {
        return sqlite_quote(&args[0]);
    }
    if args.iter().any(|a| a.is_null()) {
        return Ok(Value::Null);
    }
    let text = |v: &Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects text, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    let int = |v: &Value| -> Result<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects an integer, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    let num = |v: &Value| -> Result<f64> {
        match v {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(x) => Ok(*x),
            other => Err(Error::TypeMismatch(format!(
                "{}() expects a number, got {}",
                f.name(),
                other.type_name()
            ))),
        }
    };
    // A math result that is NaN (e.g. sqrt of a negative) is SQL NULL, matching
    // sqlite — never a NaN handed back into a typed column. An infinity is KEPT
    // (sqlite returns `Inf` for `exp(1000)`), so only NaN maps to NULL here.
    let float_or_null = |r: f64| if r.is_nan() { Value::Null } else { Value::Float(r) };
    // The logarithms are NULL for a non-positive argument: sqlite explicitly
    // checks `x <= 0` and returns NULL rather than the C library's `-inf`/NaN.
    // For `x > 0` the result is finite, so no extra guard is needed after.
    let log_or_null =
        |x: f64, f: fn(f64) -> f64| if x > 0.0 { Value::Float(f(x)) } else { Value::Null };
    Ok(match f {
        ScalarFn::Lower => Value::Text(text(&args[0])?.to_lowercase()),
        ScalarFn::Upper => Value::Text(text(&args[0])?.to_uppercase()),
        // CHARACTERS, not bytes: `length('æ')` is 1. A byte count would be a
        // silent wrong answer for every non-ASCII string, which is most of the
        // strings in this author's part of the world.
        ScalarFn::Length => Value::Int(text(&args[0])?.chars().count() as i64),
        ScalarFn::Trim => {
            let s = text(&args[0])?;
            match args.get(1) {
                // trim(x, set): strip any of the given characters from BOTH
                // ends — the two-sided analogue of ltrim/rtrim (sqlite's rule).
                Some(_) => {
                    let set: Vec<char> = text(&args[1])?.chars().collect();
                    Value::Text(s.trim_matches(|c| set.contains(&c)).to_string())
                }
                None => Value::Text(s.trim().to_string()),
            }
        }
        ScalarFn::Abs => match &args[0] {
            // i64::MIN has no positive counterpart: negating it overflows and
            // would panic in debug and silently wrap in release.
            Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| {
                Error::TypeMismatch("abs(): integer overflow (i64::MIN has no absolute value)".into())
            })?),
            Value::Float(x) => Value::Float(x.abs()),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "abs() expects a number, got {}",
                    other.type_name()
                )))
            }
        },
        ScalarFn::Round => {
            let digits = if args.len() == 2 { int(&args[1])? } else { 0 };
            match &args[0] {
                // Rounding an integer is the integer, at any digit count.
                Value::Int(i) => Value::Int(*i),
                Value::Float(x) => {
                    let p = 10f64.powi(digits.clamp(-15, 15) as i32);
                    Value::Float((x * p).round() / p)
                }
                other => {
                    return Err(Error::TypeMismatch(format!(
                        "round() expects a number, got {}",
                        other.type_name()
                    )))
                }
            }
        }
        ScalarFn::Substr => {
            // sqlite/PostgreSQL agree here: 1-based, and a start below 1 counts
            // toward the string rather than erroring.
            let s: Vec<char> = text(&args[0])?.chars().collect();
            let start = int(&args[1])?;
            let begin = if start < 1 { 0usize } else { (start - 1) as usize };
            let end = match args.len() {
                3 => {
                    let n = int(&args[2])?;
                    if n <= 0 {
                        begin
                    } else {
                        // saturating: begin+n can exceed usize on a hostile plan
                        begin.saturating_add(n as usize).min(s.len())
                    }
                }
                _ => s.len(),
            };
            let begin = begin.min(s.len());
            let end = end.max(begin).min(s.len());
            Value::Text(s[begin..end].iter().collect())
        }
        ScalarFn::Replace => {
            let s = text(&args[0])?;
            let from = text(&args[1])?;
            let to = text(&args[2])?;
            // sqlite: an empty search string leaves the input unchanged (Rust's
            // `str::replace("")` would instead splice `to` between every char).
            if from.is_empty() {
                Value::Text(s)
            } else {
                Value::Text(s.replace(&from, &to))
            }
        }
        ScalarFn::Ltrim => {
            let s = text(&args[0])?;
            match args.get(1) {
                Some(_) => {
                    let set: Vec<char> = text(&args[1])?.chars().collect();
                    Value::Text(s.trim_start_matches(|c| set.contains(&c)).to_string())
                }
                None => Value::Text(s.trim_start().to_string()),
            }
        }
        ScalarFn::Rtrim => {
            let s = text(&args[0])?;
            match args.get(1) {
                Some(_) => {
                    let set: Vec<char> = text(&args[1])?.chars().collect();
                    Value::Text(s.trim_end_matches(|c| set.contains(&c)).to_string())
                }
                None => Value::Text(s.trim_end().to_string()),
            }
        }
        ScalarFn::Instr => {
            // 1-based character position of the first occurrence of the needle,
            // 0 when absent; an empty needle is at position 1 (sqlite's rule).
            let hay: Vec<char> = text(&args[0])?.chars().collect();
            let needle: Vec<char> = text(&args[1])?.chars().collect();
            let pos = if needle.is_empty() {
                1
            } else if needle.len() > hay.len() {
                0
            } else {
                (0..=hay.len() - needle.len())
                    .find(|&i| hay[i..i + needle.len()] == needle[..])
                    .map_or(0, |i| i as i64 + 1)
            };
            Value::Int(pos)
        }
        // sqrt of a negative and pow with a non-real result are NULL (sqlite),
        // and both always return a float regardless of the argument types.
        ScalarFn::Sqrt => float_or_null(num(&args[0])?.sqrt()),
        ScalarFn::Pow => float_or_null(num(&args[0])?.powf(num(&args[1])?)),
        ScalarFn::Sign => match &args[0] {
            Value::Int(i) => Value::Int(i.signum()),
            Value::Float(x) => Value::Int(if *x > 0.0 {
                1
            } else if *x < 0.0 {
                -1
            } else {
                0 // covers +0.0, -0.0, and (unreachable here) NaN
            }),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "sign() expects a number, got {}",
                    other.type_name()
                )))
            }
        },
        // ceil/floor/trunc preserve the argument's type (sqlite: an integer
        // stays an integer at any value; a float rounds toward +/-inf, or
        // toward zero for trunc, as a float).
        ScalarFn::Ceil | ScalarFn::Floor | ScalarFn::Trunc => match &args[0] {
            Value::Int(i) => Value::Int(*i),
            Value::Float(x) => Value::Float(match f {
                ScalarFn::Ceil => x.ceil(),
                ScalarFn::Floor => x.floor(),
                _ => x.trunc(),
            }),
            other => {
                return Err(Error::TypeMismatch(format!(
                    "{}() expects a number, got {}",
                    f.name(),
                    other.type_name()
                )))
            }
        },
        // char(X1, …, Xn): the string of the given Unicode code points.
        // Variadic; char() is the empty string. NULL propagates (the gate
        // above) — sqlite instead reads a NULL argument as code point 0
        // (a NUL char); the rigid rule is uniform NULL propagation, and this
        // is the one documented gap.
        ScalarFn::Char => {
            let mut s = String::with_capacity(args.len());
            for a in args {
                let cp = int(a)?;
                // An out-of-range or surrogate code point becomes U+FFFD
                // rather than erroring or panicking — a hostile plan must
                // stay safe.
                let c = u32::try_from(cp)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{FFFD}');
                s.push(c);
            }
            Value::Text(s)
        }
        // unicode(x): the code point of the FIRST character of x, or NULL for
        // the empty string (sqlite — there is no first character to name).
        ScalarFn::Unicode => match text(&args[0])?.chars().next() {
            Some(c) => Value::Int(c as i64),
            None => Value::Null,
        },
        // hex(x): uppercase hex of the argument's bytes. Text hexes its UTF-8
        // bytes, a blob its raw bytes (sqlite). A number is refused rather
        // than sqlite's render-to-text-then-hex, a loose-typing artifact.
        ScalarFn::Hex => {
            let bytes: &[u8] = match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Blob(b) => b.as_slice(),
                other => {
                    return Err(Error::TypeMismatch(format!(
                        "hex() expects text or blob, got {}",
                        other.type_name()
                    )))
                }
            };
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            let mut out = String::with_capacity(bytes.len() * 2);
            for &b in bytes {
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
            Value::Text(out)
        }
        // --- Math functions. Every one takes number(s) and returns a float.
        // exp/sinh/cosh may overflow to +/-inf, which is KEPT (sqlite returns
        // `Inf`); only a NaN (e.g. asin out of [-1,1]) becomes NULL.
        ScalarFn::Exp => float_or_null(num(&args[0])?.exp()),
        // ln/log10/log2: NULL for a non-positive argument (sqlite).
        ScalarFn::Ln => log_or_null(num(&args[0])?, f64::ln),
        ScalarFn::Log10 => log_or_null(num(&args[0])?, f64::log10),
        ScalarFn::Log2 => log_or_null(num(&args[0])?, f64::log2),
        // log(b, x): sqlite requires the base b > 1 (it checks ln(b) > 0) and
        // x > 0, else NULL. The result is ln(x)/ln(b) = `x.log(b)`, finite there.
        ScalarFn::LogBase => {
            let b = num(&args[0])?;
            let x = num(&args[1])?;
            if b > 1.0 && x > 0.0 {
                Value::Float(x.log(b))
            } else {
                Value::Null
            }
        }
        ScalarFn::Sin => float_or_null(num(&args[0])?.sin()),
        ScalarFn::Cos => float_or_null(num(&args[0])?.cos()),
        ScalarFn::Tan => float_or_null(num(&args[0])?.tan()),
        ScalarFn::Asin => float_or_null(num(&args[0])?.asin()),
        ScalarFn::Acos => float_or_null(num(&args[0])?.acos()),
        ScalarFn::Atan => float_or_null(num(&args[0])?.atan()),
        // atan2(y, x): note the argument order — y first, like sqlite and C.
        ScalarFn::Atan2 => float_or_null(num(&args[0])?.atan2(num(&args[1])?)),
        ScalarFn::Sinh => float_or_null(num(&args[0])?.sinh()),
        ScalarFn::Cosh => float_or_null(num(&args[0])?.cosh()),
        ScalarFn::Tanh => float_or_null(num(&args[0])?.tanh()),
        ScalarFn::Radians => float_or_null(num(&args[0])?.to_radians()),
        ScalarFn::Degrees => float_or_null(num(&args[0])?.to_degrees()),
        ScalarFn::Pi => Value::Float(std::f64::consts::PI),
        // mod(x, y) = x - y*trunc(x/y), which is exactly Rust's `%` on floats
        // (C fmod). A zero divisor gives NaN → NULL — the same NULL the `%`
        // operator yields on a zero divisor (sqlite semantics).
        ScalarFn::Mod => float_or_null(num(&args[0])? % num(&args[1])?),
        // strftime(FORMAT, TIMESTRING): sqlite's time formatter over the ISO-8601
        // time strings, restricted to the specifiers mpedb reproduces exactly.
        ScalarFn::Strftime => super::datetime::sqlite_strftime(args)?,
        // json(X): validate and minify. Text only — sqlite's `json(5)` (a bare
        // number) and its JSON5 extensions are refused by name rather than
        // approximated.
        ScalarFn::Json => super::json::sqlite_json(&args[0])?,
        // Handled ahead of the null gate above; unreachable here.
        ScalarFn::Typeof => unreachable!("typeof is dispatched before the null gate"),
        ScalarFn::Printf => unreachable!("printf is dispatched before the null gate"),
        ScalarFn::Quote => unreachable!("quote is dispatched before the null gate"),
    })
}

/// sqlite's `quoteFunc`: the SQL literal denoting `v`.
///
/// * NULL → the bare word `NULL` (not a quoted string).
/// * INTEGER → the plain decimal digits.
/// * REAL → [`printf::quote_float`] (`%!.15g`, with sqlite's `%!.20e`
///   round-trip fallback).
/// * TEXT → `'…'` with every embedded `'` doubled (sqlite's `%Q`). Newlines and
///   every other byte pass through verbatim.
/// * BLOB → `X'…'` with UPPERCASE hex digits.
///
/// **The one deliberate divergence:** sqlite reaches the text through
/// `sqlite3_value_text`, a NUL-terminated C string, so `quote()` of a string
/// containing an embedded NUL silently TRUNCATES the literal there
/// (`quote(char(97,0,98))` is `'a'`). mpedb's TEXT can hold a NUL, and a
/// quoting function that silently drops the tail of its input is exactly the
/// kind of quiet wrong answer this engine refuses — so that case is a clean
/// error naming the byte offset instead.
fn sqlite_quote(v: &Value) -> Result<Value> {
    Ok(Value::Text(match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        // mpedb's two extra first-class scalars have no sqlite counterpart to
        // agree with; both render as the integer they already render as under
        // `||`, `CAST(… AS TEXT)` and `printf`, so the literal round-trips.
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Timestamp(t) => t.to_string(),
        Value::Float(x) => match super::printf::quote_float(*x) {
            Some(bytes) => String::from_utf8(bytes).expect("quote_float is ASCII"),
            None => {
                return Err(Error::TypeMismatch(format!(
                    "quote(): the real {x:?} needs more than 15 significant digits to round-trip, \
                     and sqlite's fallback rendering for that case (`%!.20e`) is not portable — \
                     sqlite3FpDecode picks between an 80-bit `long double` scaling (18 digits) \
                     and a Dekker double-double one (19 digits, different last digit) at startup, \
                     per build, via sqlite3Config.bUseLongDouble. mpedb refuses rather than emit \
                     a near-miss literal; CAST({x:?} AS TEXT) gives sqlite's %!.15g rendering, \
                     which IS portable"
                )))
            }
        },
        Value::Text(s) => {
            if let Some(pos) = s.find('\0') {
                return Err(Error::TypeMismatch(format!(
                    "quote(): the text argument contains an embedded NUL byte at offset {pos}; \
                     sqlite would silently truncate the literal there, so mpedb refuses it \
                     rather than return a shortened literal"
                )));
            }
            let mut out = String::with_capacity(s.len() + 2);
            out.push('\'');
            for c in s.chars() {
                if c == '\'' {
                    out.push('\'');
                }
                out.push(c);
            }
            out.push('\'');
            out
        }
        Value::Blob(b) => {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            let mut out = String::with_capacity(b.len() * 2 + 3);
            out.push_str("X'");
            for &byte in b {
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
            out.push('\'');
            out
        }
        Value::List(_) => {
            return Err(Error::TypeMismatch(
                "quote() has no literal form for a list value".into(),
            ))
        }
    }))
}

/// sqlite `typeof()` datatype string. The five sqlite core names
/// (`null`/`integer`/`real`/`text`/`blob`) match sqlite exactly; mpedb's two
/// extra first-class types report their own honest names (sqlite has no such
/// type to agree or disagree with), and a param-only `List` names itself.
fn sqlite_typeof(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Int(_) => "integer",
        Value::Float(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
        Value::Bool(_) => "boolean",
        Value::Timestamp(_) => "timestamp",
        Value::List(_) => "list",
    }
}
