//! Aggregate accumulators, and the NULL rules that make them SQL rather than
//! arithmetic.
//!
//! These are the rules people get wrong, so they are spelled out and tested
//! rather than assumed:
//!
//! | case | SQL says | why it bites |
//! |---|---|---|
//! | `SUM` over zero rows | **NULL**, not 0 | "no rows" and "rows summing to 0" are different facts |
//! | `COUNT` over zero rows | **0**, not NULL | count is the one that starts at zero |
//! | `SUM(x)` where some x are NULL | skips them | a NULL is missing data, not a zero |
//! | `SUM(x)` where ALL x are NULL | **NULL** | never seeing a value is not the same as summing nothing |
//! | `COUNT(x)` vs `COUNT(*)` | skips NULLs vs counts rows | the difference is the whole point of `COUNT(*)` |
//! | `MIN`/`MAX` over zero rows | **NULL** | |
//! | `AVG` | `SUM/COUNT` over the NON-NULL values | dividing by the row count instead is a silent wrong answer |
//!
//! Every rule above was checked against sqlite 3.45 rather than recalled, and
//! mpedb matches it on all seven.
//!
//! Integer `SUM` overflow is an ERROR, not a wrap — and I expected that to be a
//! place mpedb was stricter than sqlite. It is not: sqlite raises "integer
//! overflow" too. Both refuse rather than hand back a plausible wrong total.

use crate::{AggFn, Error, Result, Value};
use std::collections::BTreeSet;

/// One HOST aggregate's running state over ONE group (the C-API `xStep`/
/// `xFinal` path, design/DESIGN-UDF.md stage 2). The engine creates one per
/// group, feeds it every surviving row, and consumes it once.
///
/// The NULL rule here is deliberately the OPPOSITE of [`Accum`]'s: a built-in
/// aggregate skips NULL inputs, but sqlite hands a user aggregate every row
/// including NULL arguments and lets `xStep` decide. Django's `StdDevPop.step`
/// relies on exactly that.
pub trait HostAggState {
    /// Feed one row's already-evaluated arguments.
    fn step(&mut self, args: &[Value]) -> Result<()>;
    /// The group's result. Consumes the state (`xFinal` runs once and the
    /// aggregate context is freed straight after), so it takes `Box<Self>` to
    /// stay object-safe.
    fn finish(self: Box<Self>) -> Result<Value>;
}

/// Resolve a HOST-registered aggregate by name at exec time, mirroring
/// [`HostFns`](crate::HostFns) for scalars. Implemented by the facade over its
/// per-connection registry; `None` is threaded wherever no host aggregate can be
/// in scope, so the mechanism stays inert for every existing plan.
pub trait HostAggs {
    /// Start a fresh accumulation of `name` over `argc` arguments. An `Error`
    /// when nothing of that name/arity is registered — defensive; the binder
    /// already checked, but a registration can change between compile and
    /// execute.
    fn create(&self, name: &str, argc: i32) -> Result<Box<dyn HostAggState>>;
}

/// Running state for one aggregate over one group.
#[derive(Debug, Clone)]
pub struct Accum {
    func: AggFn,
    /// Rows seen (for `COUNT(*)`) or non-NULL values seen (everything else).
    n: u64,
    /// Running value; `None` until the first non-NULL input.
    acc: Option<Value>,
    /// `AVG` needs a float total regardless of the input's type.
    fsum: f64,
    /// `f(DISTINCT x)`: the values already accumulated in this group, keyed by
    /// their memcmp encoding (`Value` is neither `Hash` nor `Ord`). `None` for
    /// a non-DISTINCT aggregate — the set is what makes DISTINCT cost memory
    /// proportional to the group's distinct values, so a plain `sum(x)` must
    /// not pay for it.
    seen: Option<BTreeSet<Vec<u8>>>,
}

impl Accum {
    pub fn new(func: AggFn) -> Accum {
        Accum {
            func,
            n: 0,
            acc: None,
            fsum: 0.0,
            seen: None,
        }
    }

    /// `f(DISTINCT x)` — accumulate each distinct value once.
    ///
    /// NULL never reaches the set: `push` skips it before the dedup, which is
    /// why `count(DISTINCT x)` over an all-NULL group is 0 rather than 1. The
    /// difference from `SELECT DISTINCT`, where NULLs form one group, is that
    /// there NULL is a value being grouped and here it is a value being
    /// ignored.
    pub fn new_distinct(func: AggFn) -> Accum {
        Accum {
            seen: Some(BTreeSet::new()),
            ..Accum::new(func)
        }
    }

    /// Feed one row's value. `None` means `COUNT(*)` — there is no argument, so
    /// the row itself is the input and NULL cannot arise.
    pub fn push(&mut self, v: Option<&Value>) -> Result<()> {
        let Some(v) = v else {
            // COUNT(*): the row counts, whatever is in it.
            self.n += 1;
            return Ok(());
        };
        // Every other aggregate SKIPS NULL. This is the rule that separates
        // "summed nothing" from "saw no values" below.
        if v.is_null() {
            return Ok(());
        }
        // DISTINCT: a repeat of a value already accumulated in this group is
        // dropped here, AFTER the NULL skip above and BEFORE `n` moves — so it
        // affects count, sum and avg alike, and min/max not at all (they are
        // idempotent, which is why `min(DISTINCT x)` is legal but pointless).
        if let Some(seen) = &mut self.seen {
            if !seen.insert(crate::keycode::encode_key(std::slice::from_ref(v))) {
                return Ok(());
            }
        }
        self.n += 1;
        match self.func {
            AggFn::Count => {}
            AggFn::Total => match v {
                // Always a float running sum; no overflow to raise (unlike Sum's
                // integer path) and no NULL to return (finish is 0.0 over empty).
                Value::Int(i) => self.fsum += *i as f64,
                Value::Float(f) => self.fsum += *f,
                other => {
                    return Err(Error::TypeMismatch(format!(
                        "total() expects a number, got {}",
                        other.type_name()
                    )))
                }
            },
            AggFn::GroupConcat => {
                // Concatenate the non-NULL values' text (raw, not the quoted
                // Display form) with a ',' separator, in scan order.
                let piece = group_concat_text(v)?;
                self.acc = Some(match self.acc.take() {
                    None => Value::Text(piece),
                    Some(Value::Text(mut s)) => {
                        s.push(',');
                        s.push_str(&piece);
                        Value::Text(s)
                    }
                    Some(other) => {
                        return Err(Error::Internal(format!(
                            "group_concat accumulator held a non-text value {other:?}"
                        )))
                    }
                });
            }
            AggFn::Sum | AggFn::Avg => {
                match v {
                    Value::Int(i) => {
                        self.fsum += *i as f64;
                        self.acc = Some(match self.acc.take() {
                            None => Value::Int(*i),
                            Some(Value::Int(a)) => Value::Int(a.checked_add(*i).ok_or(
                                // Wrapping here would hand back a plausible
                                // wrong total; refuse instead.
                                Error::ArithmeticOverflow,
                            )?),
                            Some(Value::Float(a)) => Value::Float(a + *i as f64),
                            Some(other) => return Err(mixed(self.func, &other, v)),
                        });
                    }
                    Value::Float(f) => {
                        self.fsum += *f;
                        self.acc = Some(match self.acc.take() {
                            None => Value::Float(*f),
                            Some(Value::Float(a)) => Value::Float(a + *f),
                            // Int-then-Float in one column cannot happen under a
                            // rigid schema, but the accumulator must not depend
                            // on that to stay correct.
                            Some(Value::Int(a)) => Value::Float(a as f64 + *f),
                            Some(other) => return Err(mixed(self.func, &other, v)),
                        });
                    }
                    other => {
                        return Err(Error::TypeMismatch(format!(
                            "{}() expects a number, got {}",
                            self.func.name(),
                            other.type_name()
                        )))
                    }
                }
            }
            AggFn::Min | AggFn::Max => {
                // `min_max_prefers` returns false for an incomparable pair, which
                // keeps the incumbent rather than silently replacing it. The same
                // rule drives the executor's bare-column witness, so the two can
                // never disagree about which value (or row) an extremum picks.
                let keep = match &self.acc {
                    None => true,
                    Some(a) => self.func.min_max_prefers(a, v)?,
                };
                if keep {
                    self.acc = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    /// The group's result.
    pub fn finish(self) -> Value {
        match self.func {
            // The one aggregate that is 0 over an empty group. Everything else
            // is NULL, because "I saw no values" is not a number.
            AggFn::Count => Value::Int(self.n as i64),
            AggFn::Avg => {
                if self.n == 0 {
                    Value::Null
                } else {
                    // Over the NON-NULL count, never the row count: dividing by
                    // rows would quietly report a wrong average whenever the
                    // column has holes.
                    Value::Float(self.fsum / self.n as f64)
                }
            }
            // `total` is the one that is 0.0 over an empty group, and always a
            // float — the deliberate contrast with `sum`'s NULL.
            AggFn::Total => Value::Float(self.fsum),
            AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::GroupConcat => {
                self.acc.unwrap_or(Value::Null)
            }
        }
    }
}

/// One value's contribution to `group_concat`: its raw text (NOT the quoted
/// `Display` form). Text passes through; numbers/bool/timestamp stringify;
/// a blob has no lossless text form here and is refused.
fn group_concat_text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Timestamp(t) => t.to_string(),
        other => {
            return Err(Error::TypeMismatch(format!(
                "group_concat() cannot render a {} as text",
                other.type_name()
            )))
        }
    })
}

fn mixed(f: AggFn, a: &Value, b: &Value) -> Error {
    Error::TypeMismatch(format!(
        "{}() cannot mix {} and {}",
        f.name(),
        a.type_name(),
        b.type_name()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: AggFn, vals: &[Option<Value>]) -> Value {
        let mut a = Accum::new(f);
        for v in vals {
            a.push(v.as_ref()).unwrap();
        }
        a.finish()
    }

    /// The empty-group split: COUNT is 0, everything else is NULL. Getting this
    /// backwards turns "there were no rows" into "the total was zero", which are
    /// different facts and only one of them is true.
    #[test]
    fn empty_group_counts_zero_but_sums_null() {
        assert_eq!(run(AggFn::Count, &[]), Value::Int(0));
        assert_eq!(run(AggFn::Sum, &[]), Value::Null);
        assert_eq!(run(AggFn::Avg, &[]), Value::Null);
        assert_eq!(run(AggFn::Min, &[]), Value::Null);
        assert_eq!(run(AggFn::Max, &[]), Value::Null);
    }

    /// NULLs are skipped — a NULL is missing data, not a zero. And a group of
    /// ONLY NULLs sums to NULL, not 0: never seeing a value is not the same as
    /// summing nothing.
    #[test]
    fn nulls_are_skipped_and_an_all_null_group_is_null() {
        let vs = [Some(Value::Int(1)), Some(Value::Null), Some(Value::Int(2))];
        assert_eq!(run(AggFn::Sum, &vs), Value::Int(3));
        assert_eq!(run(AggFn::Count, &vs), Value::Int(2), "COUNT(x) skips NULL");
        assert_eq!(run(AggFn::Min, &vs), Value::Int(1));

        let all_null = [Some(Value::Null), Some(Value::Null)];
        assert_eq!(run(AggFn::Sum, &all_null), Value::Null);
        assert_eq!(run(AggFn::Count, &all_null), Value::Int(0));
    }

    /// `COUNT(*)` counts ROWS — that is the entire difference from `COUNT(x)`,
    /// and it is why the arg is an Option rather than a value that might be NULL.
    #[test]
    fn count_star_counts_rows_including_all_null_ones() {
        let mut a = Accum::new(AggFn::Count);
        for _ in 0..3 {
            a.push(None).unwrap(); // COUNT(*)
        }
        assert_eq!(a.finish(), Value::Int(3));
    }

    /// AVG divides by the NON-NULL count. Dividing by the row count is a silent
    /// wrong answer on any column with holes.
    #[test]
    fn avg_divides_by_the_non_null_count() {
        let vs = [
            Some(Value::Int(2)),
            Some(Value::Null),
            Some(Value::Int(4)),
        ];
        assert_eq!(run(AggFn::Avg, &vs), Value::Float(3.0)); // 6/2, not 6/3
    }

    /// Integer SUM overflow is an error. A wrapped total is a plausible wrong
    /// number, which is worse than a refusal.
    #[test]
    fn integer_sum_overflow_is_refused_not_wrapped() {
        let mut a = Accum::new(AggFn::Sum);
        a.push(Some(&Value::Int(i64::MAX))).unwrap();
        let r = a.push(Some(&Value::Int(1)));
        assert!(matches!(r, Err(Error::ArithmeticOverflow)), "got {r:?}");
    }

    fn run_distinct(f: AggFn, vals: &[Option<Value>]) -> Value {
        let mut a = Accum::new_distinct(f);
        for v in vals {
            a.push(v.as_ref()).unwrap();
        }
        a.finish()
    }

    /// `count(DISTINCT x)` collapses repeats and still skips NULL. Verified
    /// against sqlite 3.45 on the same data.
    #[test]
    fn distinct_collapses_repeats_and_still_skips_null() {
        let vs = [
            Some(Value::Int(100)),
            Some(Value::Int(100)),
            Some(Value::Int(200)),
            Some(Value::Null),
            Some(Value::Int(50)),
        ];
        assert_eq!(run_distinct(AggFn::Count, &vs), Value::Int(3));
        assert_eq!(run_distinct(AggFn::Sum, &vs), Value::Int(350)); // not 450
        assert_eq!(run_distinct(AggFn::Min, &vs), Value::Int(50));
    }

    /// An all-NULL group is 0 under `count(DISTINCT x)`, not 1: NULL is
    /// skipped BEFORE the dedup, so it never becomes "one distinct value".
    /// This is the one place DISTINCT-in-an-aggregate differs from SELECT
    /// DISTINCT, where NULLs do form a group.
    #[test]
    fn distinct_never_counts_null_as_a_value() {
        let all_null = [Some(Value::Null), Some(Value::Null)];
        assert_eq!(run_distinct(AggFn::Count, &all_null), Value::Int(0));
        assert_eq!(run_distinct(AggFn::Sum, &all_null), Value::Null);
    }

    /// AVG(DISTINCT x) averages the distinct values — 100 and 200 average to
    /// 150 however many times each was seen.
    #[test]
    fn avg_distinct_averages_the_distinct_values() {
        let vs = [
            Some(Value::Int(100)),
            Some(Value::Int(100)),
            Some(Value::Int(100)),
            Some(Value::Int(200)),
        ];
        assert_eq!(run_distinct(AggFn::Avg, &vs), Value::Float(150.0));
        assert_eq!(run(AggFn::Avg, &vs), Value::Float(125.0)); // without DISTINCT
    }

    /// min/max are idempotent, so DISTINCT cannot change them. Legal, and a
    /// no-op — pinned so a dedup bug cannot hide here.
    #[test]
    fn distinct_is_a_no_op_for_min_and_max() {
        let vs = [
            Some(Value::Int(5)),
            Some(Value::Int(5)),
            Some(Value::Int(1)),
            Some(Value::Int(9)),
        ];
        assert_eq!(run_distinct(AggFn::Min, &vs), run(AggFn::Min, &vs));
        assert_eq!(run_distinct(AggFn::Max, &vs), run(AggFn::Max, &vs));
    }

    #[test]
    fn min_max_over_text_use_sql_ordering() {
        let vs = [
            Some(Value::Text("pear".into())),
            Some(Value::Text("apple".into())),
        ];
        assert_eq!(run(AggFn::Min, &vs), Value::Text("apple".into()));
        assert_eq!(run(AggFn::Max, &vs), Value::Text("pear".into()));
    }

    #[test]
    fn sum_of_text_is_a_type_error() {
        let mut a = Accum::new(AggFn::Sum);
        assert!(a.push(Some(&Value::Text("x".into()))).is_err());
    }
}
