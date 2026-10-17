//! Native compiler tests against public PL/SQL semantics, not a server oracle.

use super::compile;
use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
use crate::ir::Proc;
use mpedb_types::{Result, Value};

fn tuple(values: Vec<Value>) -> ProcValue {
    ProcValue::Tuple(values.into_iter().map(ProcValue::Scalar).collect())
}

#[test]
fn exact_arithmetic_obeys_precedence_and_preserves_full_exponent_range() {
    for (expression, expected) in [
        ("0.1 + 0.2", "0.3"),
        ("10 - 3 * 2", "4"),
        ("(10 - 3) * 2", "14"),
        ("-(7.5 + 2.5) / 4", "-2.5"),
        ("1 / 8", "0.125"),
        ("5 / 2", "2.5"),
        ("1e100 + 1e100", "2e100"),
        ("1e-100 * 1e-20", "1e-120"),
        ("1 / 1e100", "1e-100"),
        ("9007199254740992.125 + 0.125", "9007199254740992.25"),
        ("-5 / -2", "2.5"),
        ("---2", "-2"),
    ] {
        // Separate minus tokens because Oracle's -- begins a line comment.
        let expression = expression.replace("---", "- - - ");
        let expected = mpedb_types::value::canonical_numeric(
            &mpedb_types::value::parse_numeric(expected).unwrap(),
        );
        let source = function("", "NUMBER", &format!("BEGIN RETURN {expression}; END;"));
        assert_eq!(
            call(&source, &[]).unwrap(),
            Value::Numeric(expected),
            "{expression}"
        );
    }
}

#[test]
fn arithmetic_nulls_zero_division_and_short_circuit_match_sql_rules() {
    for expression in ["NULL + 1", "NULL * 2", "-NULL", "NULL / 0"] {
        assert_eq!(
            call(
                &function("", "NUMBER", &format!("BEGIN RETURN {expression}; END;")),
                &[]
            )
            .unwrap(),
            Value::Null
        );
    }
    assert!(matches!(
        call(&function("", "NUMBER", "BEGIN RETURN 1 / 0; END;"), &[]),
        Err(mpedb_types::Error::DivisionByZero)
    ));
    assert_eq!(
        call(
            &function("", "BOOLEAN", "BEGIN RETURN FALSE AND (1 / 0 > 1); END;"),
            &[]
        )
        .unwrap(),
        Value::Bool(false)
    );
    assert!(call(
        &function("", "BOOLEAN", "BEGIN RETURN NULL AND (1 / 0 > 1); END;"),
        &[]
    )
    .is_err());
    assert!(compile(&function("", "BOOLEAN", "BEGIN RETURN TRUE OR 1 + 2; END;")).is_err());
}

#[test]
fn rounded_arithmetic_matches_measured_oracle_values() {
    for (expression, expected) in [
        ("1 / 3", "0.3333333333333333333333333333333333333333"),
        ("2 / 3", "0.6666666666666666666666666666666666666667"),
        ("-2 / 3", "-0.6666666666666666666666666666666666666667"),
        ("1 / 7", "0.1428571428571428571428571428571428571429"),
        ("1e-100 * 1e-100", "0"),
        ("1e-131", "0"),
        (
            "12345678901234567890123456789012345678901",
            "12345678901234567890123456789012345678900",
        ),
    ] {
        assert_eq!(
            call(
                &function("", "NUMBER", &format!("BEGIN RETURN {expression}; END;")),
                &[]
            )
            .unwrap(),
            Value::Numeric(expected.into()),
            "{expression}"
        );
    }
    assert!(call(
        &function("", "NUMBER", "BEGIN RETURN 1e125 * 100; END;"),
        &[]
    )
    .is_err());
}

#[test]
fn division_underflow_raises_while_literal_and_multiplication_underflow_zero() {
    assert!(matches!(
        call(
            &function("", "NUMBER", "BEGIN RETURN 1e-100 / 1e100; END;"),
            &[]
        ),
        Err(mpedb_types::Error::ArithmeticOverflow)
    ));
    assert_eq!(
        call(
            &function("", "NUMBER", "BEGIN RETURN 1e-100 * 1e-100; END;"),
            &[]
        )
        .unwrap(),
        Value::Numeric("0".into())
    );
    assert_eq!(
        call(&function("", "NUMBER", "BEGIN RETURN 1e-131; END;"), &[]).unwrap(),
        Value::Numeric("0".into())
    );
}

#[test]
fn number_rounding_ties_are_away_from_zero_and_trailing_zeros_are_bounded() {
    let source = function("p NUMBER", "NUMBER", "BEGIN RETURN p; END;");
    for (input, expected) in [
        (
            "1.000000000000000000000000000000000000005",
            "1.00000000000000000000000000000000000001",
        ),
        (
            "-1.000000000000000000000000000000000000005",
            "-1.00000000000000000000000000000000000001",
        ),
    ] {
        assert_eq!(
            call(&source, &[Value::Numeric(input.into())]).unwrap(),
            Value::Numeric(expected.into())
        );
    }
    assert_eq!(
        call(
            &source,
            &[Value::Numeric(format!("1.{}", "0".repeat(10000)))]
        )
        .unwrap(),
        Value::Numeric("1".into())
    );
}

#[test]
fn out_parameters_start_null_and_copy_out_in_declaration_order() {
    let source = "CREATE PROCEDURE copy_values(a NUMBER, b OUT NUMBER, c IN OUT BOOLEAN, d OUT BOOLEAN) IS BEGIN IF b IS NOT NULL THEN RETURN; END IF; b := a; c := NOT c; END;";
    assert_eq!(
        call_result(
            source,
            &[
                Value::Int(7),
                Value::Bool(true),
                Value::Bool(true),
                Value::Int(9)
            ]
        )
        .unwrap(),
        tuple(vec![
            Value::Numeric("7".into()),
            Value::Numeric("7".into()),
            Value::Bool(false),
            Value::Null
        ])
    );
}

#[test]
fn early_return_copies_outputs_and_nocopy_is_a_permitted_ignored_hint() {
    let source = "CREATE PROCEDURE set_value(a OUT NOCOPY NUMBER, b IN OUT NOCOPY BOOLEAN) IS BEGIN a := 12.25; RETURN; b := FALSE; END;";
    assert_eq!(
        call_result(source, &[Value::Null, Value::Bool(true)]).unwrap(),
        tuple(vec![Value::Numeric("12.25".into()), Value::Bool(true)])
    );
}

#[test]
fn functions_with_out_return_value_then_full_parameter_vector() {
    let source = function(
        "a NUMBER, b OUT NUMBER",
        "BOOLEAN",
        "BEGIN b := a; RETURN b IS NOT NULL; END;",
    );
    assert_eq!(
        call_result(&source, &[Value::Int(42), Value::Null]).unwrap(),
        tuple(vec![
            Value::Bool(true),
            Value::Numeric("42".into()),
            Value::Numeric("42".into())
        ])
    );
}

#[test]
fn failed_execution_never_returns_partial_out_values_or_mutates_inputs() {
    let args = [Value::Null, Value::Bool(true)];
    let source = function(
        "a OUT NUMBER, b IN OUT BOOLEAN",
        "NUMBER",
        "BEGIN a := 1; b := FALSE; END;",
    );
    assert!(call_result(&source, &args).is_err());
    assert_eq!(args, [Value::Null, Value::Bool(true)]);
}

fn call_result(source: &str, args: &[Value]) -> Result<ProcValue> {
    let s = compile(source)?;
    assert!(s.calls.is_empty());
    let p = Proc::new(s.name, s.argc, s.nlocals, vec![], s.consts, s.instrs)?;
    let p = Proc::decode(&p.encode())?;
    interp::run(
        &p,
        args,
        &mut MockBridge::new(),
        Budget {
            instrs: 1000,
            db_calls: 0,
            rows: 0,
        },
    )
}
fn call(source: &str, args: &[Value]) -> Result<Value> {
    match call_result(source, args)? {
        ProcValue::Scalar(value) => Ok(value),
        _ => panic!("unexpected container"),
    }
}

fn function(params: &str, result: &str, body: &str) -> String {
    let params = if params.is_empty() {
        String::new()
    } else {
        format!("({params})")
    };
    format!("CREATE OR REPLACE FUNCTION f{params} RETURN {result} IS {body}")
}

#[test]
fn boolean_logic_preserves_unknown_through_actual_frontend() {
    let source = function(
        "a BOOLEAN, b IN BOOLEAN",
        "BOOLEAN",
        "BEGIN RETURN NOT a OR b; END f;",
    );
    for (a, b, expected) in [
        (Value::Null, Value::Bool(false), Value::Null),
        (Value::Null, Value::Bool(true), Value::Bool(true)),
        (Value::Bool(true), Value::Null, Value::Null),
        (Value::Bool(false), Value::Null, Value::Bool(true)),
    ] {
        assert_eq!(call(&source, &[a, b]).unwrap(), expected);
    }
}

#[test]
fn nested_blocks_shadow_and_restore_names_without_losing_outer_assignments() {
    let source = function("p NUMBER", "NUMBER", "x NUMBER := 7; y NUMBER := 0; BEGIN DECLARE x NUMBER := 3; BEGIN BEGIN x := x + 1; END; y := x; END; RETURN x + y + p; END;");
    assert_eq!(
        call(&source, &[Value::Int(2)]).unwrap(),
        Value::Numeric("13".into())
    );
    let source = function(
        "p NUMBER",
        "NUMBER",
        "BEGIN DECLARE p NUMBER := 2; BEGIN RETURN p; END; END;",
    );
    assert_eq!(
        call(&source, &[Value::Int(9)]).unwrap(),
        Value::Numeric("2".into())
    );
    let source = "CREATE PROCEDURE p(x OUT NUMBER) IS BEGIN DECLARE x NUMBER := 4; BEGIN x := 5; END; x := 7; END;";
    assert_eq!(
        call_result(source, &[Value::Null]).unwrap(),
        tuple(vec![Value::Numeric("7".into())])
    );
}

#[test]
fn block_locals_reinitialize_on_each_loop_entry_and_exit_targets_outer_loop() {
    let source = function("", "NUMBER", "n NUMBER := 0; total NUMBER := 0; BEGIN WHILE n < 4 LOOP DECLARE x NUMBER DEFAULT 1; BEGIN n := n + 1; x := x + 1; CONTINUE WHEN n = 2; total := total + x; EXIT WHEN n = 3; END; END LOOP; RETURN total; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("4".into()));
}

#[test]
fn block_scopes_reject_leaks_duplicates_and_incomplete_initializer_references() {
    for body in [
        "BEGIN DECLARE x NUMBER := 1; BEGIN NULL; END; RETURN x; END;",
        "BEGIN DECLARE x NUMBER; x NUMBER; BEGIN NULL; END; RETURN 1; END;",
        "x NUMBER := 7; BEGIN DECLARE x NUMBER := x + 1; BEGIN RETURN x; END; END;",
        "x NUMBER := 7; BEGIN DECLARE y NUMBER := x; x NUMBER := 3; BEGIN RETURN y; END; END;",
        "BEGIN DECLARE x NUMBER := y; y NUMBER := 3; BEGIN RETURN x; END; END;",
        "BEGIN DECLARE c CONSTANT NUMBER := 1; BEGIN c := 2; END; RETURN 1; END;",
    ] {
        assert!(compile(&function("", "NUMBER", body)).is_err(), "{body}");
    }
}

#[test]
fn constants_initialize_in_order_and_remain_read_only() {
    let source = function(
        "p NUMBER",
        "NUMBER",
        "a CONSTANT NUMBER := p / 3; b CONSTANT NUMBER DEFAULT a * 2; BEGIN RETURN b; END;",
    );
    assert_eq!(
        call(&source, &[Value::Int(3)]).unwrap(),
        Value::Numeric("2".into())
    );
    assert_eq!(call(&source, &[Value::Null]).unwrap(), Value::Null);
    let source = function(
        "",
        "BOOLEAN",
        "c CONSTANT BOOLEAN DEFAULT TRUE; BEGIN RETURN c; END;",
    );
    assert_eq!(call(&source, &[]).unwrap(), Value::Bool(true));
    let source = function(
        "",
        "NUMBER",
        "c CONSTANT NUMBER := NULL; BEGIN RETURN c; END;",
    );
    assert_eq!(call(&source, &[]).unwrap(), Value::Null);
    for body in [
        "c CONSTANT NUMBER; BEGIN RETURN c; END;",
        "c CONSTANT NUMBER := 1; BEGIN c := 2; RETURN c; END;",
        "c CONSTANT NUMBER := 1; BEGIN IF FALSE THEN c := 2; END IF; RETURN c; END;",
        "c CONSTANT NUMBER := TRUE; BEGIN RETURN c; END;",
        "c CONSTANT BOOLEAN DEFAULT 1; BEGIN RETURN c; END;",
        "c CONSTANT NUMBER := c; BEGIN RETURN c; END;",
    ] {
        assert!(compile(&function("", "NUMBER", body)).is_err(), "{body}");
    }
}

#[test]
fn branches_declarations_and_assignments_keep_exact_numbers() {
    let source = function("p NUMBER", "NUMBER", "v NUMBER := -9007199254740992.12345; BEGIN IF p IS NULL THEN RETURN v; ELSIF p > 10 THEN v := p; ELSE v := 1.25; END IF; RETURN v; END;");
    assert_eq!(
        call(&source, &[Value::Null]).unwrap(),
        Value::Numeric("-9007199254740992.12345".into())
    );
    assert_eq!(
        call(&source, &[Value::Int(12)]).unwrap(),
        Value::Numeric("12".into())
    );
    assert_eq!(
        call(&source, &[Value::Int(1)]).unwrap(),
        Value::Numeric("1.25".into())
    );
}

#[test]
fn comparisons_to_null_are_unknown_instead_of_language_equality() {
    let source = function("p NUMBER", "BOOLEAN", "BEGIN RETURN p = NULL; END;");
    assert_eq!(call(&source, &[Value::Null]).unwrap(), Value::Null);
    let source = function("p NUMBER", "BOOLEAN", "BEGIN RETURN p IS NULL; END;");
    assert_eq!(call(&source, &[Value::Null]).unwrap(), Value::Bool(true));
}

#[test]
fn null_condition_selects_else_and_missing_return_raises() {
    let source = function(
        "p BOOLEAN",
        "NUMBER",
        "BEGIN IF p THEN RETURN 1; ELSE RETURN 2; END IF; END;",
    );
    assert_eq!(
        call(&source, &[Value::Null]).unwrap(),
        Value::Numeric("2".into())
    );
    let source = function(
        "p BOOLEAN",
        "NUMBER",
        "BEGIN IF p THEN RETURN 1; END IF; END;",
    );
    assert!(call(&source, &[Value::Bool(false)])
        .unwrap_err()
        .to_string()
        .contains("without RETURN"));
    assert_eq!(
        call(&source, &[Value::Bool(true)]).unwrap(),
        Value::Numeric("1".into())
    );
}

#[test]
fn loop_exit_continue_and_budget_use_native_control_flow() {
    let source = function("", "BOOLEAN", "flag BOOLEAN := TRUE; BEGIN WHILE flag LOOP flag := FALSE; CONTINUE; END LOOP; LOOP EXIT WHEN NULL; flag := TRUE; EXIT WHEN flag; END LOOP; RETURN flag; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Bool(true));
    let source = function("", "BOOLEAN", "BEGIN LOOP NULL; END LOOP; END;");
    assert!(call(&source, &[])
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn compiler_rejects_bad_types_even_in_dead_short_circuit_branches() {
    for body in [
        "BEGIN RETURN TRUE OR 3; END;",
        "BEGIN RETURN FALSE AND 3; END;",
        "BEGIN IF 3 THEN RETURN TRUE; END IF; END;",
        "BEGIN RETURN 3 = TRUE; END;",
        "v NUMBER := TRUE; BEGIN RETURN TRUE; END;",
    ] {
        assert!(compile(&function("", "BOOLEAN", body)).is_err(), "{body}");
    }
    assert!(call(
        &function("p BOOLEAN", "BOOLEAN", "BEGIN RETURN p; END;"),
        &[Value::Int(1)]
    )
    .is_err());
    assert!(call(
        &function("p NUMBER", "NUMBER", "BEGIN RETURN p; END;"),
        &[Value::Bool(true)]
    )
    .is_err());
}

#[test]
fn procedure_return_and_input_parameter_immutability_are_checked() {
    assert_eq!(
        call("CREATE PROCEDURE nothing IS BEGIN NULL; RETURN; END;", &[]).unwrap(),
        Value::Null
    );
    assert!(compile("CREATE PROCEDURE bad IS BEGIN RETURN 1; END;").is_err());
    assert!(compile(&function(
        "p BOOLEAN",
        "BOOLEAN",
        "BEGIN p := FALSE; RETURN p; END;"
    ))
    .is_err());
    assert!(compile(&function(
        "p IN NOCOPY NUMBER",
        "NUMBER",
        "BEGIN RETURN p; END;"
    ))
    .is_err());
}

#[test]
fn end_names_and_client_terminators_preserve_source_identity() {
    let source =
        "CREATE FUNCTION \"MiXeD\" RETURN BOOLEAN AS BEGIN RETURN TRUE; END \"MiXeD\";\n/\n";
    assert!(compile("CREATE FUNCTION f() RETURN BOOLEAN IS BEGIN RETURN TRUE; END;").is_err());
    assert_eq!(compile(source).unwrap().name, "MiXeD");
    assert_eq!(call(source, &[]).unwrap(), Value::Bool(true));
    assert!(compile("CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN TRUE; END g;").is_err());
    assert!(compile("CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN TRUE; END;/").is_err());
    assert!(
        compile("CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN TRUE; END; DROP TABLE t;")
            .is_err()
    );
}

#[test]
fn number_overflow_and_nonfinite_inputs_raise() {
    for text in ["1e126", "-1e126"] {
        assert!(call(
            &function("", "NUMBER", &format!("BEGIN RETURN {text}; END;")),
            &[]
        )
        .is_err());
    }
    let source = function("p NUMBER", "NUMBER", "BEGIN RETURN p; END;");
    for text in ["NaN", "Infinity", "-Infinity", "garbage"] {
        assert!(call(&source, &[Value::Numeric(text.into())]).is_err());
    }
    assert!(call(&source, &[Value::Float(0.1)]).is_err());
}

#[test]
fn nested_and_truncated_sources_fail_without_stack_overflow_or_panic() {
    let source = function(
        "p BOOLEAN",
        "BOOLEAN",
        "BEGIN IF p THEN RETURN NULL; ELSE RETURN FALSE; END IF; END;",
    );
    for end in 0..source.len() {
        let _ = compile(&source[..end]);
    }
    let source = function(
        "",
        "BOOLEAN",
        &format!(
            "BEGIN RETURN {}TRUE{}; END;",
            "(".repeat(200),
            ")".repeat(200)
        ),
    );
    assert!(compile(&source).is_err());
}
