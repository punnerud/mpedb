//! Persisted instruction semantics cannot change under an existing proc hash.
use super::{run, testutil::MockBridge, Budget, ProcValue};
use crate::ir::{Op, Proc};
use mpedb_types::Value;

fn evaluate(consts: Vec<Value>, instrs: Vec<Op>) -> mpedb_types::Result<ProcValue> {
    let p = Proc::new("number_version".into(), 0, 0, vec![], consts, instrs)?;
    let p = Proc::decode(&p.encode())?;
    run(&p, &[], &mut MockBridge::new(), Budget::default())
}

#[test]
fn legacy_number_conversion_still_refuses_underflow_and_large_precision() {
    for number in ["1e-131", "123456789012345678901234567890123456789"] {
        assert!(evaluate(
            vec![Value::Numeric(number.into())],
            vec![Op::LoadConst(0), Op::SqlNumber, Op::Return]
        )
        .is_err());
    }
    assert_eq!(
        evaluate(
            vec![Value::Numeric("1e-131".into())],
            vec![Op::LoadConst(0), Op::OracleNumber, Op::Return]
        )
        .unwrap(),
        ProcValue::Scalar(Value::Numeric("0".into()))
    );
}

#[test]
fn legacy_division_retains_its_error_and_new_division_has_distinct_encoding() {
    let consts = vec![Value::Numeric("1".into()), Value::Numeric("3".into())];
    let program = |op| {
        Proc::new(
            "division".into(),
            0,
            0,
            vec![],
            consts.clone(),
            vec![Op::LoadConst(0), Op::LoadConst(1), op, Op::Return],
        )
        .unwrap()
    };
    let old = program(Op::SqlDiv);
    let new = program(Op::OracleDiv);
    assert_ne!(old.hash(), new.hash());
    assert!(run(
        &Proc::decode(&old.encode()).unwrap(),
        &[],
        &mut MockBridge::new(),
        Budget::default()
    )
    .is_err());
    assert_eq!(
        run(
            &Proc::decode(&new.encode()).unwrap(),
            &[],
            &mut MockBridge::new(),
            Budget::default()
        )
        .unwrap(),
        ProcValue::Scalar(Value::Numeric(
            "0.3333333333333333333333333333333333333333".into()
        ))
    );
}

#[test]
fn plsql_underflow_instruction_does_not_change_stored_sql_style_division() {
    let program = |op| {
        Proc::new(
            "underflow".into(),
            0,
            0,
            vec![],
            vec![
                Value::Numeric("1e-100".into()),
                Value::Numeric("1e100".into()),
            ],
            vec![Op::LoadConst(0), Op::LoadConst(1), op, Op::Return],
        )
        .unwrap()
    };
    let old = program(Op::OracleDiv);
    let new = program(Op::OraclePlSqlDiv);
    assert_ne!(old.hash(), new.hash());
    let execute = |p: Proc| {
        run(
            &Proc::decode(&p.encode()).unwrap(),
            &[],
            &mut MockBridge::new(),
            Budget::default(),
        )
    };
    assert!(matches!(
        execute(old),
        Err(mpedb_types::Error::ArithmeticOverflow)
    ));
    assert_eq!(
        execute(new).unwrap(),
        ProcValue::Scalar(Value::Numeric("0".into()))
    );
}

#[test]
fn special_value_instructions_keep_finite_only_stored_routines_unchanged() {
    let input = vec![Value::Numeric("Infinity".into())];
    assert!(evaluate(
        input.clone(),
        vec![Op::LoadConst(0), Op::OracleNumber, Op::Return]
    )
    .is_err());
    assert_eq!(
        evaluate(
            input,
            vec![Op::LoadConst(0), Op::OraclePlSqlNumber, Op::Return]
        )
        .unwrap(),
        ProcValue::Scalar(Value::Numeric("Infinity".into()))
    );
    let program = |op| {
        Proc::new(
            "overflow".into(),
            0,
            0,
            vec![],
            vec![Value::Numeric("1e125".into())],
            vec![Op::LoadConst(0), Op::LoadConst(0), op, Op::Return],
        )
        .unwrap()
    };
    let old = program(Op::OracleMul);
    let new = program(Op::OraclePlSqlMul);
    assert_ne!(old.hash(), new.hash());
    let execute = |p: Proc| {
        run(
            &Proc::decode(&p.encode()).unwrap(),
            &[],
            &mut MockBridge::new(),
            Budget::default(),
        )
    };
    assert!(matches!(
        execute(old),
        Err(mpedb_types::Error::ArithmeticOverflow)
    ));
    assert_eq!(
        execute(new).unwrap(),
        ProcValue::Scalar(Value::Numeric("Infinity".into()))
    );
}
