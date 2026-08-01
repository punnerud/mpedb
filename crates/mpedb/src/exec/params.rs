use super::*;

/// Validate bound parameters against the plan's inferred types, applying the
/// implicit conversions that are **provably lossless** for the value at hand.
///
/// # bool ⇄ int64
///
/// CPython's `sqlite3` binds Python `True`/`False` through `sqlite3_bind_int64`
/// as 1/0, and Django does exactly that for every `BooleanField` lookup — so a
/// slot the binder pinned to `Bool` is handed an `Int`. 0 and 1 convert, since
/// that IS sqlite's representation of a boolean. Any other integer is REFUSED
/// rather than truthy-tested: mpedb's rigid `Bool` cannot hold it, and sqlite
/// would have stored and returned the integer itself. The reverse — a real
/// `Bool` in an int64 slot, which the Rust/Python SDKs can produce — is always
/// exact (`TRUE` -> 1).
///
/// # int64 ⇄ float64 (task #74)
///
/// The same shape, one level up. sqlite has no parameter types at all: a
/// `sqlite3_bind_int64(1)` against `WHERE real_col > ?` is compared numerically
/// against the real column, and a `sqlite3_bind_double(1.0)` into an INTEGER
/// column is stored as the integer 1 by INTEGER affinity. mpedb infers a type
/// per slot instead (`WHERE r > ?` pins `$1` to `float64`), so the driver's
/// choice of bind function — which for Django/CPython follows the *Python*
/// value's type, not the column's — decided whether the statement ran.
///
/// Bridging at BIND, like the bool case, rather than widening the type lattice:
/// the lattice is what makes a plan's operand types static, and `unify_operands`
/// already inserts a `ToFloat` for a genuinely mixed *expression*. What was
/// missing is only that a bound scalar cannot carry its own coercion.
///
/// **Both directions convert only when the round trip is exact**, and the
/// inexact cases are refused by name rather than rounded:
///
/// * `Int -> Float`: refused above 2^53-ish magnitudes, where `n as f64` is no
///   longer `n`. sqlite compares an integer against a real EXACTLY
///   (`sqlite3IntFloatCompare`), so rounding the parameter first could flip a
///   `>` on a large key — a wrong answer, not a wider one.
/// * `Float -> Int`: refused for a non-integral value (`1.5`) and for anything
///   outside the i64 range. Truncating would answer `i > 1.5` as `i > 1` (or
///   `i > 2`), and storing it would silently lose the fraction. sqlite's own
///   INTEGER affinity converts a real only when it is losslessly integral, and
///   this is that rule.
///
/// Returns `Cow::Borrowed` (no copy) whenever nothing needed converting, which
/// is every statement whose parameters already match.
pub(crate) fn coerce_params<'a>(
    plan: &CompiledPlan,
    params: &'a [Value],
) -> Result<std::borrow::Cow<'a, [Value]>> {
    use std::borrow::Cow;
    if params.len() != plan.n_params as usize {
        return Err(Error::WrongParamCount {
            expected: plan.n_params as usize,
            got: params.len(),
        });
    }
    let mut out: Option<Vec<Value>> = None;
    for (i, pt) in plan.param_types.iter().enumerate() {
        let (Some(t), Some(v)) = (pt, params.get(i)) else {
            continue;
        };
        if v.fits(*t) {
            continue;
        }
        let bridged = match (v, t) {
            (Value::Int(n @ (0 | 1)), mpedb_types::ColumnType::Bool) => Some(Value::Bool(*n == 1)),
            (Value::Bool(b), mpedb_types::ColumnType::Int64) => Some(Value::Int(*b as i64)),
            (Value::Int(n), mpedb_types::ColumnType::Float64) => {
                exact_int_as_float(*n).map(Value::Float)
            }
            (Value::Float(f), mpedb_types::ColumnType::Int64) => {
                exact_float_as_int(*f).map(Value::Int)
            }
            // sqlite affinity: a full integer/float text against an int/real slot
            // converts (Django and CPython often bind numbers as text).
            //
            // ⚠ A NON-numeric text must NOT become 0 here. `CAST('abc' AS
            // INTEGER)` is 0 in sqlite, but BINDING A PARAMETER IS NOT A CAST,
            // and the two disagree exactly where it matters: sqlite evaluates
            // `id = 'abc'` by comparing storage classes (integer sorts below
            // text) and answers FALSE for every row, whereas coercing to 0
            // makes it MATCH a row whose id is 0. That is a wrong answer, not
            // a lenient one — and it is the distinction E3(b) settled: apply
            // the conversion and stay rigid about its RESULT. Returning None
            // falls through to a named type error, which is narrower than
            // sqlite and never different from it.
            //
            // The arithmetic case (`num_chairs + ?`, where sqlite really does
            // coerce to 0) is a genuine gap, but it cannot be decided here:
            // `coerce_params` sees the slot, not whether the parameter is used
            // in arithmetic or in a comparison. Closing it needs the use site,
            // not a blanket rule. See C-API-COMPAT.md's named refusal for
            // `test_expressions_not_introduce_sql_injection_via_untrusted_string_inclusion`.
            (Value::Text(s), mpedb_types::ColumnType::Int64) => {
                s.trim().parse::<i64>().ok().map(Value::Int)
            }
            (Value::Text(s), mpedb_types::ColumnType::Float64) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(Value::Float),
            _ => None,
        };
        match bridged {
            Some(nv) => out.get_or_insert_with(|| params.to_vec())[i] = nv,
            None => {
                // Name the reason when the two types WOULD have bridged and it
                // was this particular value that could not — "1.5 is not an
                // integer" is actionable where "float64 vs int64" is not. Every
                // other pair keeps the exact wording it has always had (the
                // Python SDK matches on the timestamp one).
                let why = match (v, t) {
                    (Value::Int(_), mpedb_types::ColumnType::Float64) => {
                        " (too large to convert to float64 without losing precision)"
                    }
                    (Value::Float(f), mpedb_types::ColumnType::Int64) => {
                        if f.is_finite() && f.fract() == 0.0 {
                            " (outside the int64 range)"
                        } else {
                            " (not an exact integer)"
                        }
                    }
                    _ => "",
                };
                return Err(Error::TypeMismatch(format!(
                    "parameter ${} is {}, statement requires {}{}",
                    i + 1,
                    v.type_name(),
                    t,
                    why
                )));
            }
        }
    }
    Ok(match out {
        Some(v) => Cow::Owned(v),
        None => Cow::Borrowed(params),
    })
}
