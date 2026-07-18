//! Built-in scalar functions: the [`ScalarFn`] enum and the
//! NULL-propagating [`call_scalar`] dispatch (plus `typeof`, which does not).

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
            other => return Err(Error::Corrupt(format!("unknown scalar function {other}"))),
        })
    }

    /// Allowed argument counts. Checked at verify time so `eval` can index the
    /// popped args without re-checking.
    pub fn arity_ok(self, argc: u8) -> bool {
        match self {
            ScalarFn::Lower | ScalarFn::Upper | ScalarFn::Length | ScalarFn::Abs
            | ScalarFn::Unicode | ScalarFn::Hex | ScalarFn::Typeof => argc == 1,
            ScalarFn::Round | ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => {
                argc == 1 || argc == 2
            }
            ScalarFn::Sqrt | ScalarFn::Sign | ScalarFn::Ceil | ScalarFn::Floor => argc == 1,
            ScalarFn::Substr => argc == 2 || argc == 3,
            ScalarFn::Instr | ScalarFn::Pow => argc == 2,
            ScalarFn::Replace => argc == 3,
            // char() is variadic: 0..=255 code points (the u8 argc caps it).
            ScalarFn::Char => true,
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
    // sqlite — never a NaN handed back into a typed column.
    let float_or_null = |r: f64| if r.is_nan() { Value::Null } else { Value::Float(r) };
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
        // ceil/floor preserve the argument's type (sqlite: an integer stays an
        // integer at any value; a float rounds toward +/-inf as a float).
        ScalarFn::Ceil | ScalarFn::Floor => match &args[0] {
            Value::Int(i) => Value::Int(*i),
            Value::Float(x) => Value::Float(if matches!(f, ScalarFn::Ceil) {
                x.ceil()
            } else {
                x.floor()
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
        // Handled ahead of the null gate above; unreachable here.
        ScalarFn::Typeof => unreachable!("typeof is dispatched before the null gate"),
    })
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
