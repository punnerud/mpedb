use super::{run, testutil::MockBridge, Budget, ProcValue};
use crate::ir::{Op, Proc, MAX_STACK};
use mpedb_types::Value;

#[test]
fn scalar_tuple_constructor_round_trips_and_keeps_order_and_nulls() {
    let p = Proc::new(
        "tuple".into(),
        0,
        0,
        vec![],
        vec![Value::Int(12), Value::Null, Value::Bool(true)],
        vec![
            Op::LoadConst(0),
            Op::LoadConst(1),
            Op::LoadConst(2),
            Op::Tuple(3),
            Op::Return,
        ],
    )
    .unwrap();
    let p = Proc::decode(&p.encode()).unwrap();
    assert_eq!(
        run(&p, &[], &mut MockBridge::new(), Budget::default()).unwrap(),
        ProcValue::Tuple(vec![
            ProcValue::Scalar(Value::Int(12)),
            ProcValue::Scalar(Value::Null),
            ProcValue::Scalar(Value::Bool(true))
        ])
    );
    let p = Proc::new(
        "empty".into(),
        0,
        0,
        vec![],
        vec![],
        vec![Op::Tuple(0), Op::Return],
    )
    .unwrap();
    assert_eq!(
        run(&p, &[], &mut MockBridge::new(), Budget::default()).unwrap(),
        ProcValue::Tuple(vec![])
    );
}

#[test]
fn tuple_underflow_and_unreachable_oversized_arity_are_rejected() {
    assert!(Proc::new(
        "bad".into(),
        0,
        0,
        vec![],
        vec![Value::Null],
        vec![Op::LoadConst(0), Op::Tuple(2), Op::Return]
    )
    .is_err());
    assert!(Proc::new(
        "bad".into(),
        0,
        0,
        vec![],
        vec![Value::Null],
        vec![
            Op::LoadConst(0),
            Op::Return,
            Op::Tuple((MAX_STACK + 1) as u16)
        ]
    )
    .is_err());
}

#[test]
fn nested_tuple_expansion_is_refused_before_return_materialization() {
    let p = Proc::new(
        "expansion".into(),
        0,
        0,
        vec![],
        vec![Value::Int(1)],
        vec![
            Op::LoadConst(0),
            Op::Tuple(1),
            Op::Dup,
            Op::Tuple(2),
            Op::Return,
        ],
    )
    .unwrap();
    let p = Proc::decode(&p.encode()).unwrap();
    assert!(run(&p, &[], &mut MockBridge::new(), Budget::default())
        .unwrap_err()
        .to_string()
        .contains("must be scalar"));
}
