use super::*;
use crate::expr::store_affinity;

/// The declared-name → (type, affinity) pair, which is what a column
/// definition needs and what one `ColumnType` alone cannot say.
#[test]
fn declared_splits_numeric_from_typeless() {
    for n in ["numeric", "decimal(10,2)", "decimal", "datetime", "date", "nosuchtype"] {
        assert_eq!(ColumnType::declared(n), (ColumnType::Any, Affinity::Numeric), "{n}");
    }
    // A column with NO declared type is the OPPOSITE behaviour under the
    // same storage type.
    assert_eq!(ColumnType::declared(""), (ColumnType::Any, Affinity::Blob));
    assert_eq!(ColumnType::declared("  "), (ColumnType::Any, Affinity::Blob));
    // The carve-outs, plus a name sqlite's rule already agrees with.
    assert_eq!(ColumnType::declared("any"), (ColumnType::Any, Affinity::Blob));
    assert_eq!(ColumnType::declared("bool"), (ColumnType::Bool, Affinity::Integer));
    assert_eq!(ColumnType::declared("int64"), (ColumnType::Int64, Affinity::Integer));
    // `timestamp` is NUMERIC affinity like `datetime` (#113) — SQL has no
    // timestamp literal, so a rigid one here was unwritable.
    assert_eq!(
        ColumnType::declared("timestamp"),
        (ColumnType::Any, Affinity::Numeric)
    );
    // BLOB affinity is the THIRD behaviour under `Any`: converts nothing,
    // and unlike the no-type column it is a name a consumer wrote.
    for n in ["blob", "longblob", "cblob"] {
        assert_eq!(ColumnType::declared(n), (ColumnType::Any, Affinity::Blob), "{n}");
    }
    // The rigid three: the affinity is sqlite's, and it agrees with the one
    // the type implies.
    for (n, t) in [
        ("bigint", ColumnType::Int64),
        ("double precision", ColumnType::Float64),
        ("varchar(100)", ColumnType::Text),
    ] {
        let (ty, aff) = ColumnType::declared(n);
        assert_eq!((ty, aff), (t, Affinity::implied_by(t)), "{n}");
        assert_eq!(aff, Affinity::from_type_name(n), "{n}");
    }
}

/// Store-time affinity is NOT `CAST`. Both are sqlite's, and they disagree —
/// pinned so the two can never be merged into one function with a flag.
#[test]
fn store_affinity_is_not_cast() {
    use crate::expr::{ExprProgram, Instr};
    let cast = |v: Value, aff: Affinity| {
        ExprProgram::new(vec![Instr::PushParam(0), Instr::Cast(aff)], vec![])
            .unwrap()
            .eval(&[], &[v])
            .unwrap()
    };
    // A numeric PREFIX is a number to CAST and not a number at all to
    // affinity, which leaves the text alone.
    assert_eq!(cast(Value::Text("12abc".into()), Affinity::Numeric), Value::Int(12));
    assert_eq!(
        store_affinity(Affinity::Numeric, Value::Text("12abc".into())),
        Value::Text("12abc".into())
    );
    // CAST stops at sqlite's 2^51 `RealSameAsInt` bound; store-time affinity
    // uses the full i64 round trip, so this one really does become an int.
    assert_eq!(cast(Value::Text("1e18".into()), Affinity::Numeric), Value::Float(1e18));
    assert_eq!(
        store_affinity(Affinity::Numeric, Value::Text("1e18".into())),
        Value::Int(1_000_000_000_000_000_000)
    );
    // A blob is parsed by CAST and never by affinity.
    assert_eq!(cast(Value::Blob(b"12".to_vec()), Affinity::Numeric), Value::Int(12));
    assert_eq!(
        store_affinity(Affinity::Numeric, Value::Blob(b"12".to_vec())),
        Value::Blob(b"12".to_vec())
    );
}

/// BLOB affinity — the typeless column — converts nothing, ever.
#[test]
fn blob_affinity_is_verbatim() {
    for v in [
        Value::Text("1.50".into()),
        Value::Text("abc".into()),
        Value::Int(3),
        Value::Float(1.0),
        Value::Blob(b"7".to_vec()),
        Value::Null,
    ] {
        assert_eq!(store_affinity(Affinity::Blob, v.clone()), v);
    }
}

/// The two affinities only the sqlite-overlay path can produce today.
#[test]
fn real_and_text_affinity_follow_sqlite() {
    // REAL: the NUMERIC rule, then widen an integer result.
    assert_eq!(store_affinity(Affinity::Real, Value::Text("0012".into())), Value::Float(12.0));
    assert_eq!(store_affinity(Affinity::Real, Value::Int(1)), Value::Float(1.0));
    assert_eq!(
        store_affinity(Affinity::Real, Value::Text("abc".into())),
        Value::Text("abc".into())
    );
    assert_eq!(
        store_affinity(Affinity::Real, Value::Blob(b"1".to_vec())),
        Value::Blob(b"1".to_vec())
    );
    // TEXT: numbers render, everything else is left alone.
    assert_eq!(store_affinity(Affinity::Text, Value::Int(1)), Value::Text("1".into()));
    assert_eq!(store_affinity(Affinity::Text, Value::Float(1.5)), Value::Text("1.5".into()));
    assert_eq!(
        store_affinity(Affinity::Text, Value::Blob(b"a".to_vec())),
        Value::Blob(b"a".to_vec())
    );
    assert_eq!(store_affinity(Affinity::Text, Value::Null), Value::Null);
    // INTEGER is NUMERIC at store time (sqlite's own documented equality).
    for v in [Value::Text("1.50".into()), Value::Text("abc".into()), Value::Float(1.0)] {
        assert_eq!(
            store_affinity(Affinity::Integer, v.clone()),
            store_affinity(Affinity::Numeric, v)
        );
    }
}

/// INTEGER against REAL is exact — no cast to a common type, which would
/// lose every magnitude past 2^53.
#[test]
fn int_against_real_is_exact() {
    let cmp = |a: Value, b: Value| a.sql_cmp(&b).unwrap().unwrap();
    assert_eq!(cmp(Value::Int(1000), Value::Float(1.5)), Ordering::Greater);
    assert_eq!(cmp(Value::Float(1.5), Value::Int(1000)), Ordering::Less);
    assert_eq!(cmp(Value::Int(1), Value::Float(1.0)), Ordering::Equal);
    assert_eq!(cmp(Value::Int(1), Value::Float(1.5)), Ordering::Less);
    assert_eq!(cmp(Value::Int(2), Value::Float(1.5)), Ordering::Greater);
    // Past 2^53: `i as f64` would round these together.
    assert_eq!(
        cmp(Value::Int(9007199254740993), Value::Float(9007199254740992.0)),
        Ordering::Greater
    );
    assert_eq!(
        cmp(Value::Float(9007199254740992.0), Value::Int(9007199254740993)),
        Ordering::Less
    );
    // Reals outside i64 range: no truncation, no UB.
    assert_eq!(cmp(Value::Int(i64::MAX), Value::Float(1e300)), Ordering::Less);
    assert_eq!(cmp(Value::Int(i64::MIN), Value::Float(-1e300)), Ordering::Greater);
    assert_eq!(cmp(Value::Int(0), Value::Float(f64::INFINITY)), Ordering::Less);
    assert_eq!(cmp(Value::Int(0), Value::Float(f64::NEG_INFINITY)), Ordering::Greater);
    // NaN sorts above every integer, agreeing with `float_total_cmp` so the
    // order stays total.
    assert_eq!(cmp(Value::Int(i64::MAX), Value::Float(f64::NAN)), Ordering::Less);
    assert_eq!(
        float_total_cmp(f64::NAN, f64::INFINITY),
        Ordering::Greater,
        "the two must agree on where NaN sits"
    );
    // A number against TEXT is still a clean REFUSAL: its answer depends on
    // comparison affinity, which is not implemented yet.
    assert!(Value::Int(1).sql_cmp(&Value::Text("1".into())).is_err());
}
