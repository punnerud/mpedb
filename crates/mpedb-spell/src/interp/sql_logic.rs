//! SQL three-valued operators for procedural frontends. These are separate
//! opcodes so existing Python/Rust blobs keep their original semantics.

use super::{tyname, type_err, Op, PValue};
use mpedb_types::{Result, Value};
use std::cmp::Ordering;

pub(super) fn number(value: PValue) -> Result<Value> {
    match value {
        PValue::Scalar(Value::Null) => Ok(Value::Null),
        PValue::Scalar(Value::Int(n)) => Ok(Value::Numeric(n.to_string())),
        PValue::Scalar(Value::Numeric(text)) => {
            let text = mpedb_types::value::parse_numeric(&text)
                .filter(|s| !matches!(s.as_str(), "NaN" | "Infinity" | "-Infinity"))
                .ok_or_else(|| type_err("PL/SQL NUMBER requires a finite decimal"))?;
            let unsigned = text.trim_start_matches('-');
            let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            let whole = whole.trim_start_matches('0');
            let digits = format!("{whole}{fraction}");
            let significant = digits.trim_start_matches('0').trim_end_matches('0');
            if significant.len() > 38 {
                return Err(type_err(
                    "PL/SQL NUMBER rounding above 38 significant digits is not implemented",
                ));
            }
            if whole.len() > 126
                || (whole.is_empty() && fraction.find(|c| c != '0').is_some_and(|n| n >= 130))
            {
                return Err(type_err("PL/SQL NUMBER exponent outside supported range"));
            }
            Ok(Value::Numeric(text))
        }
        other => Err(type_err(format!(
            "PL/SQL NUMBER conversion from {} is not implemented",
            tyname(&other)
        ))),
    }
}

pub(super) fn boolean(value: PValue) -> Result<Option<bool>> {
    match value {
        PValue::Scalar(Value::Null) => Ok(None),
        PValue::Scalar(Value::Bool(value)) => Ok(Some(value)),
        other => Err(type_err(format!(
            "SQL condition requires BOOLEAN or NULL, got {}",
            tyname(&other)
        ))),
    }
}

pub(super) fn binary(op: Op, a: PValue, b: PValue) -> Result<Value> {
    let answer = if matches!(op, Op::SqlAnd | Op::SqlOr) {
        let (a, b) = (boolean(a)?, boolean(b)?);
        match (op, a, b) {
            (Op::SqlAnd, Some(false), _) | (Op::SqlAnd, _, Some(false)) => Some(false),
            (Op::SqlAnd, Some(true), Some(true)) => Some(true),
            (Op::SqlOr, Some(true), _) | (Op::SqlOr, _, Some(true)) => Some(true),
            (Op::SqlOr, Some(false), Some(false)) => Some(false),
            _ => None,
        }
    } else {
        let (PValue::Scalar(a), PValue::Scalar(b)) = (a, b) else {
            return Err(type_err("SQL comparison requires scalar operands"));
        };
        a.sql_cmp(&b)?.map(|order| match op {
            Op::SqlEq => order == Ordering::Equal,
            Op::SqlNe => order != Ordering::Equal,
            Op::SqlLt => order == Ordering::Less,
            Op::SqlLe => order != Ordering::Greater,
            Op::SqlGt => order == Ordering::Greater,
            Op::SqlGe => order != Ordering::Less,
            _ => unreachable!("SQL binary opcode dispatch"),
        })
    };
    Ok(answer.map_or(Value::Null, Value::Bool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::Proc;

    fn execute(ops: Vec<Op>, args: &[Value]) -> Result<Value> {
        let p = Proc::new(
            "sql_logic".into(),
            args.len() as u16,
            args.len() as u16,
            vec![],
            vec![Value::Bool(true), Value::Bool(false), Value::Int(0)],
            ops,
        )?;
        let p = Proc::decode(&p.encode())?;
        match interp::run(&p, args, &mut MockBridge::new(), Budget::default())? {
            ProcValue::Scalar(value) => Ok(value),
            _ => panic!("scalar operator returned a container"),
        }
    }

    #[test]
    fn complete_boolean_truth_tables_survive_serialization() {
        use Value::{Bool as B, Null as N};
        let values = [B(false), B(true), N];
        let and = [
            [B(false), B(false), B(false)],
            [B(false), B(true), N],
            [B(false), N, N],
        ];
        let or = [
            [B(false), B(true), N],
            [B(true), B(true), B(true)],
            [N, B(true), N],
        ];
        for (i, a) in values.iter().enumerate() {
            assert_eq!(
                execute(
                    vec![Op::LoadLocal(0), Op::SqlNot, Op::Return],
                    std::slice::from_ref(a)
                )
                .unwrap(),
                [B(true), B(false), N][i]
            );
            for (j, b) in values.iter().enumerate() {
                for (op, expected) in [(Op::SqlAnd, &and[i][j]), (Op::SqlOr, &or[i][j])] {
                    assert_eq!(
                        &execute(
                            vec![Op::LoadLocal(0), Op::LoadLocal(1), op, Op::Return],
                            &[a.clone(), b.clone()]
                        )
                        .unwrap(),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn comparisons_propagate_null_and_preserve_exact_decimal_order() {
        for op in [
            Op::SqlEq,
            Op::SqlNe,
            Op::SqlLt,
            Op::SqlLe,
            Op::SqlGt,
            Op::SqlGe,
        ] {
            let code = vec![Op::LoadLocal(0), Op::LoadLocal(1), op, Op::Return];
            for args in [
                [Value::Null, Value::Null],
                [Value::Null, Value::Int(1)],
                [Value::Int(1), Value::Null],
            ] {
                assert_eq!(execute(code.clone(), &args).unwrap(), Value::Null);
            }
            for (a, b) in [(1, 2), (2, 2), (2, 1)] {
                let expected = match op {
                    Op::SqlEq => a == b,
                    Op::SqlNe => a != b,
                    Op::SqlLt => a < b,
                    Op::SqlLe => a <= b,
                    Op::SqlGt => a > b,
                    Op::SqlGe => a >= b,
                    _ => unreachable!(),
                };
                assert_eq!(
                    execute(code.clone(), &[Value::Int(a), Value::Int(b)]).unwrap(),
                    Value::Bool(expected)
                );
            }
            assert!(execute(code, &[Value::Int(1), Value::Text("1".into())]).is_err());
        }
        assert_eq!(
            execute(
                vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::SqlLt, Op::Return],
                &[
                    Value::Numeric("9007199254740992.1".into()),
                    Value::Numeric("9007199254740992.2".into())
                ]
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            execute(
                vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::SqlEq, Op::Return],
                &[Value::Numeric("1.0".into()), Value::Numeric("1.00".into())]
            )
            .unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn sql_conditions_reject_language_truthiness() {
        for value in [Value::Int(0), Value::Int(1), Value::Text("true".into())] {
            for op in [Op::SqlBool, Op::SqlNot] {
                assert!(execute(
                    vec![Op::LoadLocal(0), op, Op::Return],
                    std::slice::from_ref(&value)
                )
                .is_err());
            }
        }
        assert_eq!(
            execute(vec![Op::LoadLocal(0), Op::Not, Op::Return], &[Value::Null]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            execute(
                vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::Eq, Op::Return],
                &[Value::Null, Value::Null]
            )
            .unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn short_circuit_jumps_do_not_treat_null_as_false() {
        for (op, taken) in [(Op::JumpIfSqlFalse(4), false), (Op::JumpIfSqlTrue(4), true)] {
            let code = vec![
                Op::LoadLocal(0),
                op,
                Op::LoadConst(1),
                Op::Return,
                Op::LoadConst(0),
                Op::Return,
            ];
            for value in [Value::Null, Value::Bool(false), Value::Bool(true)] {
                let expected = Value::Bool(value == Value::Bool(taken));
                assert_eq!(execute(code.clone(), &[value]).unwrap(), expected);
            }
            assert!(execute(code, &[Value::Int(1)]).is_err());
        }
    }

    #[test]
    fn sql_jump_targets_and_stack_effects_are_validated() {
        assert!(Proc::new(
            "bad".into(),
            0,
            0,
            vec![],
            vec![],
            vec![Op::SqlNot, Op::Return]
        )
        .is_err());
        assert!(Proc::new(
            "bad".into(),
            0,
            0,
            vec![],
            vec![Value::Null],
            vec![Op::LoadConst(0), Op::JumpIfSqlFalse(99), Op::Return]
        )
        .is_err());
    }

    #[test]
    fn short_circuit_skips_errors_only_for_determining_boolean() {
        for (jump, determining) in [(Op::JumpIfSqlFalse(8), false), (Op::JumpIfSqlTrue(8), true)] {
            // The right operand would divide zero by zero. A determining
            // left operand skips it; NULL must evaluate it and raise.
            let code = vec![
                Op::LoadLocal(0),
                Op::Dup,
                jump,
                Op::LoadConst(2),
                Op::LoadConst(2),
                Op::IntDiv,
                Op::SqlAnd,
                Op::Return,
                Op::Return,
            ];
            assert_eq!(
                execute(code.clone(), &[Value::Bool(determining)]).unwrap(),
                Value::Bool(determining)
            );
            assert!(execute(code.clone(), &[Value::Bool(!determining)]).is_err());
            assert!(execute(code, &[Value::Null]).is_err());
        }
    }

    #[test]
    fn sql_backward_jump_still_consumes_instruction_budget() {
        let p = Proc::new(
            "loop".into(),
            1,
            1,
            vec![],
            vec![Value::Null],
            vec![
                Op::LoadLocal(0),
                Op::JumpIfSqlTrue(0),
                Op::LoadConst(0),
                Op::Return,
            ],
        )
        .unwrap();
        let budget = Budget {
            instrs: 10,
            db_calls: 0,
            rows: 0,
        };
        assert!(interp::run(&p, &[Value::Bool(true)], &mut MockBridge::new(), budget).is_err());
        assert_eq!(
            interp::run(&p, &[Value::Null], &mut MockBridge::new(), budget).unwrap(),
            ProcValue::Scalar(Value::Null)
        );
    }
}
