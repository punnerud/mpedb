//! `vec_l2` / `vec_cosine` — the value tests that ARE the specification.
//!
//! These two have no sqlite oracle (sqlite has no vector surface), so the
//! rule from [[widening-can-create-wrong-answers]] applies with extra force:
//! every accepted shape is pinned by VALUE here, and every refused shape by
//! its message. Truncation-at-every-offset is the house rule for decoders,
//! and a blob of f32s is a decoder.

use super::scalar::{call_scalar, ScalarFn};
use crate::{Error, Value};

fn blob(fs: &[f32]) -> Value {
    Value::Blob(fs.iter().flat_map(|f| f.to_le_bytes()).collect())
}

fn l2(a: &Value, b: &Value) -> Result<Value, Error> {
    call_scalar(ScalarFn::VecL2, &[a.clone(), b.clone()])
}

fn cos(a: &Value, b: &Value) -> Result<Value, Error> {
    call_scalar(ScalarFn::VecCosine, &[a.clone(), b.clone()])
}

fn f(v: Result<Value, Error>) -> f64 {
    match v.expect("scalar accepted") {
        Value::Float(x) => x,
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn l2_values_are_exact_on_representable_cases() {
    // 3-4-5 triangle: the distance is exactly 5.
    assert_eq!(f(l2(&blob(&[0.0, 0.0]), &blob(&[3.0, 4.0]))), 5.0);
    // Identical vectors: exactly 0.
    assert_eq!(f(l2(&blob(&[1.5, -2.5, 8.0]), &blob(&[1.5, -2.5, 8.0]))), 0.0);
    // One dimension: plain absolute difference.
    assert_eq!(f(l2(&blob(&[10.0]), &blob(&[7.0]))), 3.0);
    // Empty vectors: distance between two zero-dimensional points is 0 — the
    // sum over no terms. Accepted, because refusing it would make the answer
    // depend on data (an empty blob is a valid column value).
    assert_eq!(f(l2(&blob(&[]), &blob(&[]))), 0.0);
}

#[test]
fn cosine_values_are_exact_on_representable_cases() {
    // Parallel: distance 0.
    assert_eq!(f(cos(&blob(&[2.0, 0.0]), &blob(&[5.0, 0.0]))), 0.0);
    // Orthogonal: distance 1.
    assert_eq!(f(cos(&blob(&[1.0, 0.0]), &blob(&[0.0, 3.0]))), 1.0);
    // Opposite: distance 2.
    assert_eq!(f(cos(&blob(&[1.0, 0.0]), &blob(&[-4.0, 0.0]))), 2.0);
    // A zero vector has no direction: NULL, not an error and not a guess.
    assert_eq!(cos(&blob(&[0.0, 0.0]), &blob(&[1.0, 2.0])).unwrap(), Value::Null);
    assert_eq!(cos(&blob(&[]), &blob(&[])).unwrap(), Value::Null);
}

#[test]
fn null_propagates_like_every_scalar() {
    assert_eq!(l2(&Value::Null, &blob(&[1.0])).unwrap(), Value::Null);
    assert_eq!(cos(&blob(&[1.0]), &Value::Null).unwrap(), Value::Null);
}

#[test]
fn dimension_mismatch_is_a_refusal_never_a_guess() {
    let e = l2(&blob(&[1.0, 2.0]), &blob(&[1.0])).unwrap_err();
    assert!(
        matches!(&e, Error::TypeMismatch(m) if m.contains("dimension mismatch")),
        "got {e:?}"
    );
}

#[test]
fn truncation_at_every_offset_is_refused() {
    // A 2-dim blob truncated at every non-multiple-of-4 length must refuse —
    // silently dropping a dimension is how a wrong nearest-neighbour is born.
    let full: Vec<u8> = [1.0f32, 2.0].iter().flat_map(|f| f.to_le_bytes()).collect();
    for cut in 0..full.len() {
        let v = Value::Blob(full[..cut].to_vec());
        let ok = blob(&[1.0, 2.0]);
        let r = l2(&v, &ok);
        if cut % 4 == 0 {
            // A valid (shorter) vector: refused as a MISMATCH instead.
            assert!(
                matches!(&r, Err(Error::TypeMismatch(m)) if m.contains("dimension mismatch")),
                "cut {cut}: got {r:?}"
            );
        } else {
            assert!(
                matches!(&r, Err(Error::TypeMismatch(m)) if m.contains("multiple of 4")),
                "cut {cut}: got {r:?}"
            );
        }
    }
}

#[test]
fn non_blob_arguments_are_refused_by_name_and_position() {
    let e = l2(&Value::Text("x".into()), &blob(&[1.0])).unwrap_err();
    assert!(
        matches!(&e, Error::TypeMismatch(m) if m.contains("vec_l2() argument 1")),
        "got {e:?}"
    );
    let e = cos(&blob(&[1.0]), &Value::Int(3)).unwrap_err();
    assert!(
        matches!(&e, Error::TypeMismatch(m) if m.contains("vec_cosine() argument 2")),
        "got {e:?}"
    );
}

#[test]
fn f64_accumulation_survives_catastrophic_f32_cases() {
    // 1e8 differs from 1e8+1 by 1, but f32 cannot represent 1e8+1 — the
    // inputs collapse BEFORE the function sees them (that is the storage
    // format's precision, stated not hidden). What the function itself must
    // not do is lose precision it was GIVEN: f64 accumulation over many small
    // terms.
    let n = 10_000;
    let a = blob(&vec![0.01f32; n]);
    let b = blob(&vec![0.0f32; n]);
    let got = f(l2(&a, &b));
    let want = (f64::from(0.01f32).powi(2) * n as f64).sqrt();
    assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
}
