//! Built-in scalar functions: the [`ScalarFn`] enum and the
//! NULL-propagating [`call_scalar`] dispatch (plus `typeof`, which does not).

use super::json;
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
    // --- sqlite's JSON function set (see [`super::json`]). JSON is TEXT here,
    // exactly as in sqlite: no JSON `ColumnType`, no schema-format change.
    /// `json(X)` — validate `X` as JSON and return it MINIFIED (token spellings
    /// preserved, whitespace between tokens dropped). Strict RFC 8259: sqlite
    /// 3.45's JSON5 extensions are refused by name rather than rewritten.
    Json = 44,
    /// `json_valid(X[, FLAGS])` — 1/0. Grammar bit 1 (strict text) only; bits
    /// 2 (JSON5) and 4/8 (JSONB) are refused by name.
    JsonValid = 45,
    /// `json_type(X[, PATH])` — `object`/`array`/`integer`/`real`/`true`/
    /// `false`/`null`/`text`, or NULL when the path selects nothing.
    JsonType = 46,
    /// `json_quote(X)` — `X` as a JSON value. Does NOT null-propagate:
    /// `json_quote(NULL)` is the four-character text `null`.
    JsonQuote = 47,
    /// `json_array_length(X[, PATH])`.
    JsonArrayLength = 48,
    /// `json_extract(X, PATH, …)` — one path unwraps to a SQL value, several
    /// wrap into a JSON array.
    JsonExtract = 49,
    /// The `->` OPERATOR: the selected node's JSON TEXT.
    JsonArrow = 50,
    /// The `->>` OPERATOR: the selected node as a SQL value.
    JsonArrowText = 51,
    /// `json_array(…)`. Argument 0 is the binder-supplied JSON-subtype bitmask
    /// (see `binder::json_ness`), not a user argument.
    JsonArray = 52,
    /// `json_object(LABEL, VALUE, …)`, with the same leading subtype bitmask.
    JsonObject = 53,
    /// `json_patch(TARGET, PATCH)` — RFC 7396 merge patch. No subtype mask:
    /// both arguments are documents.
    JsonPatch = 54,
    /// `json_remove(X, PATH, …)`. No subtype mask: it takes no values.
    JsonRemove = 55,
    /// `json_replace(X, PATH, VALUE, …)`, with a leading subtype bitmask.
    JsonReplace = 56,
    /// `json_set(X, PATH, VALUE, …)`, with a leading subtype bitmask.
    JsonSet = 57,
    /// `json_insert(X, PATH, VALUE, …)`, with a leading subtype bitmask.
    JsonInsert = 58,
    /// `max(X, Y, …)` / `min(X, Y, …)` — sqlite's **scalar** two-or-more
    /// argument forms, which are a different function from the one-argument
    /// aggregates of the same name (`minmaxFunc` vs `minmaxStep`); the PARSER
    /// routes on arity, so `max(x)` is still the aggregate.
    ///
    /// A SELECTION, not a computation: the winning ARGUMENT is returned
    /// unchanged, so `max(3, 2.5)` is the integer 3 and `max(1, 2.5)` is the
    /// real 2.5 — which is why the result type is `any` for a mixed-type call
    /// rather than a widened number.
    ///
    /// Ordering is sqlite's storage-class order ([`Value::sort_cmp`], the same
    /// one `ORDER BY`/`DISTINCT` use), and the TIE RULE is sqlite's
    /// `minmaxFunc` loop verbatim: `max` keeps the EARLIER of two equal
    /// arguments, `min` takes the LATER one. Observable when the tied values
    /// have different classes — sqlite's `typeof(max(1, 1.0))` is `integer`
    /// while `typeof(min(1, 1.0))` is `real`.
    ///
    /// NULL-propagating like the rest: ANY NULL argument yields NULL (sqlite:
    /// `minmaxFunc` returns early on the first NULL it sees).
    ///
    /// Tags 60/61 rather than 44/45: several branches were in flight in the
    /// 44..59 window, and a tag hole costs nothing while a collision would
    /// silently call the wrong function.
    Max2 = 60,
    Min2 = 61,
    /// `date(TIMESTRING)` — sqlite's `dateFunc`: the ISO date `YYYY-MM-DD` of
    /// the parsed instant. Exactly `strftime('%Y-%m-%d', X)` (sqlite formats it
    /// with the same `"%04d-%02d-%02d"`), so it rides the same time-string
    /// grammar and the same refusals. See [`super::datetime`].
    Date = 62,
    /// `time(TIMESTRING)` — sqlite's `timeFunc`: `HH:MM:SS`, the whole-second
    /// truncation of the parsed h/m/s (so `'…:56.999'` is `56`, and the
    /// unnormalised `'2020-01-01 24:00'` really does print `24:00:00`).
    Time = 63,
    /// `datetime(TIMESTRING)` — sqlite's `datetimeFunc`: `YYYY-MM-DD HH:MM:SS`.
    DateTime = 64,
    /// `julianday(TIMESTRING)` — sqlite's `juliandayFunc`: the Julian day as a
    /// REAL (`iJD/86400000.0`). The one member of the family that returns a
    /// number rather than text.
    JulianDay = 65,
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
            45 => ScalarFn::JsonValid,
            46 => ScalarFn::JsonType,
            47 => ScalarFn::JsonQuote,
            48 => ScalarFn::JsonArrayLength,
            49 => ScalarFn::JsonExtract,
            50 => ScalarFn::JsonArrow,
            51 => ScalarFn::JsonArrowText,
            52 => ScalarFn::JsonArray,
            53 => ScalarFn::JsonObject,
            54 => ScalarFn::JsonPatch,
            55 => ScalarFn::JsonRemove,
            56 => ScalarFn::JsonReplace,
            57 => ScalarFn::JsonSet,
            58 => ScalarFn::JsonInsert,
            60 => ScalarFn::Max2,
            61 => ScalarFn::Min2,
            62 => ScalarFn::Date,
            63 => ScalarFn::Time,
            64 => ScalarFn::DateTime,
            65 => ScalarFn::JulianDay,
            other => return Err(Error::Corrupt(format!("unknown scalar function {other}"))),
        })
    }

    /// Allowed argument counts. Checked at verify time so `eval` can index the
    /// popped args without re-checking.
    pub fn arity_ok(self, argc: u8) -> bool {
        match self {
            ScalarFn::Lower | ScalarFn::Upper | ScalarFn::Length | ScalarFn::Abs
            | ScalarFn::Unicode | ScalarFn::Hex | ScalarFn::Typeof
            | ScalarFn::Quote => argc == 1,
            // sqlite's strftime is `(FORMAT, TIMESTRING, modifier…)`. mpedb
            // accepts the arity so the refusal can NAME the modifiers rather
            // than report a bare arity mismatch (see `call_scalar`).
            ScalarFn::Strftime => argc >= 2,
            // The `date`/`time`/`datetime`/`julianday` family is
            // `(TIMESTRING, modifier…)` in sqlite. Same reason as strftime: the
            // arity is ACCEPTED so `call_scalar` can refuse the modifier
            // language BY NAME rather than report a bare arity mismatch.
            ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime | ScalarFn::JulianDay => {
                argc >= 1
            }
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
            // --- JSON. `json`/`json_quote` take exactly one argument;
            // `json_valid`/`json_type`/`json_array_length` take an optional
            // second (FLAGS or PATH).
            ScalarFn::Json | ScalarFn::JsonQuote => argc == 1,
            ScalarFn::JsonValid | ScalarFn::JsonType | ScalarFn::JsonArrayLength => {
                argc == 1 || argc == 2
            }
            // `json_extract(X, PATH, …)` and `json_remove(X, PATH, …)` are
            // variadic over paths; the document plus at least one path.
            ScalarFn::JsonExtract => argc >= 2,
            ScalarFn::JsonRemove => argc >= 1,
            // The two operators are strictly binary.
            ScalarFn::JsonArrow | ScalarFn::JsonArrowText | ScalarFn::JsonPatch => argc == 2,
            // The value-taking writers carry a BINDER-SUPPLIED leading subtype
            // bitmask, so their plan arity is one more than the SQL arity:
            // `json_array()` is argc 1, `json_object(k,v)` is argc 3, and
            // `json_set(X,p,v)` is argc 4. The even/odd shape is re-checked at
            // eval time, where the message can name the function.
            ScalarFn::JsonArray | ScalarFn::JsonObject => argc >= 1,
            ScalarFn::JsonReplace | ScalarFn::JsonSet | ScalarFn::JsonInsert => argc >= 2,
            // The scalar min/max start at TWO arguments — one argument is the
            // aggregate, and the parser never emits these below that arity.
            ScalarFn::Max2 | ScalarFn::Min2 => argc >= 2,
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
            ScalarFn::JsonValid => "json_valid",
            ScalarFn::JsonType => "json_type",
            ScalarFn::JsonQuote => "json_quote",
            ScalarFn::JsonArrayLength => "json_array_length",
            ScalarFn::JsonExtract => "json_extract",
            ScalarFn::JsonArrow => "->",
            ScalarFn::JsonArrowText => "->>",
            ScalarFn::JsonArray => "json_array",
            ScalarFn::JsonObject => "json_object",
            ScalarFn::JsonPatch => "json_patch",
            ScalarFn::JsonRemove => "json_remove",
            ScalarFn::JsonReplace => "json_replace",
            ScalarFn::JsonSet => "json_set",
            ScalarFn::JsonInsert => "json_insert",
            ScalarFn::Max2 => "max",
            ScalarFn::Min2 => "min",
            ScalarFn::Date => "date",
            ScalarFn::Time => "time",
            ScalarFn::DateTime => "datetime",
            ScalarFn::JulianDay => "julianday",
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
    // The JSON functions that must SEE a NULL rather than propagate it.
    // `json_quote(NULL)` is the text `null`; the writers turn a NULL VALUE
    // argument into JSON `null` (`json_array(NULL)` is `[null]`); `json_valid`
    // raises on a NULL FLAGS instead of returning NULL; and `json_remove`/
    // `json_set` have their own, different NULL rules. Every one of these is
    // verified against sqlite 3.45.1 — see [`super::json`].
    match f {
        ScalarFn::JsonQuote => return json::json_quote(args),
        ScalarFn::JsonValid => return json::json_valid(args),
        ScalarFn::JsonArray => return json::json_array(args),
        ScalarFn::JsonObject => return json::json_object(args),
        ScalarFn::JsonRemove => return json::json_remove(args),
        ScalarFn::JsonSet => return json::json_set(args),
        ScalarFn::JsonInsert => return json::json_insert(args),
        ScalarFn::JsonReplace => return json::json_replace(args),
        _ => {}
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
        // date/time/datetime/julianday: sqlite's `dateFunc`/`timeFunc`/
        // `datetimeFunc`/`juliandayFunc` over the SAME time-string grammar
        // strftime uses, so the whole family shares one parser and one set of
        // refusals. A literal `'now'` was rewritten by the binder into the
        // statement-instant parameter, so it arrives here as an ordinary
        // ISO-8601 string (design note in [`super::datetime`]).
        ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime | ScalarFn::JulianDay => {
            super::datetime::sqlite_date_family(f, args)?
        }
        // --- JSON readers. These DO null-propagate (verified: a NULL document
        // or a NULL path yields NULL), so they sit under the gate.
        ScalarFn::Json => json::json(args)?,
        ScalarFn::JsonType => json::json_type(args)?,
        ScalarFn::JsonArrayLength => json::json_array_length(args)?,
        ScalarFn::JsonExtract => json::json_extract(args)?,
        ScalarFn::JsonArrow => json::json_arrow(args)?,
        ScalarFn::JsonArrowText => json::json_arrow_text(args)?,
        ScalarFn::JsonPatch => json::json_patch(args)?,
        // sqlite's SCALAR max()/min(): pick the winning ARGUMENT and return it
        // UNCHANGED (`sqlite3_result_value(context, argv[iBest])`), so the
        // result keeps that argument's type — `max(3, 2.5)` is the integer 3.
        // The null gate above already covers "any NULL argument yields NULL".
        ScalarFn::Max2 | ScalarFn::Min2 => min_max_scalar(f, args)?,
        // Handled ahead of the null gate above; unreachable here.
        ScalarFn::Typeof => unreachable!("typeof is dispatched before the null gate"),
        ScalarFn::Printf => unreachable!("printf is dispatched before the null gate"),
        ScalarFn::Quote => unreachable!("quote is dispatched before the null gate"),
        ScalarFn::JsonQuote
        | ScalarFn::JsonValid
        | ScalarFn::JsonArray
        | ScalarFn::JsonObject
        | ScalarFn::JsonRemove
        | ScalarFn::JsonSet
        | ScalarFn::JsonInsert
        | ScalarFn::JsonReplace => {
            unreachable!("the JSON writers are dispatched before the null gate")
        }
    })
}

/// sqlite's `minmaxFunc` — the scalar `max(X, Y, …)` / `min(X, Y, …)`.
///
/// A transcription of sqlite's loop, tie rule included:
///
/// ```c
/// iBest = 0;
/// for(i=1; i<argc; i++){
///   if( (sqlite3MemCompare(argv[iBest], argv[i], pColl) ^ mask) >= 0 ) iBest = i;
/// }
/// sqlite3_result_value(context, argv[iBest]);
/// ```
///
/// `mask` is 0 for `min` and -1 for `max`, so the test is `cmp >= 0` for `min`
/// (a TIE replaces, taking the LATER argument) and `cmp < 0` for `max` (a tie
/// does NOT replace, keeping the EARLIER one). That asymmetry is observable
/// whenever two equal values have different storage classes: sqlite's
/// `typeof(max(1, 1.0))` is `integer` and `typeof(min(1, 1.0))` is `real`.
///
/// `sqlite3MemCompare` is the storage-class order, which is exactly
/// [`Value::sort_cmp`] — the same comparison `ORDER BY` and `DISTINCT` use, and
/// the one whose doc comment already names MIN/MAX as a caller. NULLs are gone
/// by the time this runs, so a `None` from it can only be an INCOMPARABLE pair
/// (mpedb's own `Bool`/`Timestamp` against a different class, reachable only
/// through an `any` value). That is an error rather than an arbitrary winner:
/// sqlite has no such pair, so there is no answer of sqlite's to reproduce.
fn min_max_scalar(f: ScalarFn, args: &[Value]) -> Result<Value> {
    let want_max = matches!(f, ScalarFn::Max2);
    let mut best = 0usize;
    for i in 1..args.len() {
        let Some(ord) = args[best].sort_cmp(&args[i], Collation::Binary) else {
            return Err(Error::TypeMismatch(format!(
                "{}() cannot order {} against {}",
                f.name(),
                args[best].type_name(),
                args[i].type_name()
            )));
        };
        let replace = if want_max { ord == Ordering::Less } else { ord != Ordering::Less };
        if replace {
            best = i;
        }
    }
    Ok(args[best].clone())
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

/// sqlite `typeof()` datatype string — **exactly** one of the five sqlite
/// storage-class names `null`/`integer`/`real`/`text`/`blob`, for every
/// `Value`, always.
///
/// `typeof()` is a borrowed sqlite function, and it borrows sqlite's contract
/// with it: its documented range is those five strings and nothing else, so
/// every consumer switches on exactly those five. mpedb's own first-class
/// `Bool` and `Timestamp` used to answer `'boolean'`/`'timestamp'` here —
/// honest about mpedb's type system, but a *different answer* rather than an
/// error to a caller who asked a sqlite question. Three things settle it:
///
///  1. Range. A sixth string is wrong against the only specification the
///     function has. There is no PG reading to preserve either — PG spells it
///     `pg_typeof()` and has no `typeof()` at all — so this is not a dialect
///     question and must not be gated on one.
///  2. Internal consistency. `sqlite3_column_type` already reports
///     `SQLITE_INTEGER` for `Bool`/`Timestamp` (mpedb-capi `valconv::sqlite_type`),
///     and `_int64`/`_text` render them `1` / `"1"`. Through every other
///     accessor the value already IS an integer; `typeof` was the lone
///     dissenter, so the two disagreed about the same value.
///  3. The shim cannot fix it. `typeof()` is evaluated in the engine and
///     reaches the C boundary as an ordinary `Value::Text`, indistinguishable
///     from a text column whose content happens to be the word `boolean`.
///     Remapping strings at the boundary would corrupt real data — so the only
///     place the fix can live without inventing a new wrong answer is here.
///
/// A `Value::List` is param-only (`IN (?)` context) and cannot reach an
/// expression result; it maps to `null` to match the same defensive choice
/// `valconv::sqlite_type` makes, which keeps "typeof and column_type never
/// disagree" total over all eight variants.
fn sqlite_typeof(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        // mpedb's Bool and Timestamp are integer storage: 0/1 and microseconds
        // since the epoch. Both read back as SQLITE_INTEGER through the C API.
        Value::Int(_) | Value::Bool(_) | Value::Timestamp(_) => "integer",
        Value::Float(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
        Value::List(_) => "null",
    }
}
