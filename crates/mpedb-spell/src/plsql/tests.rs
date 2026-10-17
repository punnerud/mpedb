//! Native compiler tests against public PL/SQL semantics, not a server oracle.

use super::compile;
use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
use crate::ir::Proc;
use mpedb_types::{Result, Value};

fn tuple(values: Vec<Value>) -> ProcValue {
    ProcValue::Tuple(values.into_iter().map(ProcValue::Scalar).collect())
}

#[test]
fn numeric_for_loops_are_inclusive_reverse_and_round_bounds_once() {
    for (bounds, expected) in [
        ("1..3", "123"),
        ("REVERSE 1..3", "321"),
        ("3..1", "0"),
        ("REVERSE 3..1", "0"),
        ("0.5..2.5", "123"),
    ] {
        let source = function("", "NUMBER", &format!("n NUMBER := 0; BEGIN FOR i IN {bounds} LOOP n := n * 10 + i; END LOOP; RETURN n; END;"));
        assert_eq!(
            call(&source, &[]).unwrap(),
            Value::Numeric(expected.into()),
            "{bounds}"
        );
    }
    let source = function("", "NUMBER", "lo NUMBER := 1; hi NUMBER := 3; n NUMBER := 0; BEGIN FOR i IN lo..hi LOOP lo := 10; hi := 0; n := n * 10 + i; END LOOP; RETURN n; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("123".into()));
}

#[test]
fn numeric_for_continue_exit_nesting_and_endpoint_overflow_are_correct() {
    let source = function("", "NUMBER", "n NUMBER := 0; BEGIN FOR i IN 1..5 LOOP CONTINUE WHEN i = 2; FOR j IN REVERSE 1..2 LOOP EXIT WHEN j = 1; n := n + i; END LOOP; EXIT WHEN i = 3; END LOOP; RETURN n; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("4".into()));
    for (reverse, bound) in [("", "2147483647"), ("REVERSE ", "-2147483648")] {
        let source = function("", "NUMBER", &format!("n NUMBER := 0; BEGIN FOR i IN {reverse}{bound}..{bound} LOOP n := i; CONTINUE; END LOOP; RETURN n; END;"));
        assert_eq!(call(&source, &[]).unwrap(), Value::Numeric(bound.into()));
    }
}

#[test]
fn for_index_hides_outer_names_in_bounds_and_restores_outer_scope() {
    let source = function("", "NUMBER", "i NUMBER := 7; n NUMBER := 0; BEGIN FOR i IN 1..3 LOOP n := n + i; END LOOP; RETURN n + i; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("13".into()));
    let source = function(
        "",
        "NUMBER",
        "i NUMBER := 3; BEGIN FOR i IN i..i+2 LOOP NULL; END LOOP; RETURN i; END;",
    );
    assert!(call(&source, &[])
        .unwrap_err()
        .to_string()
        .contains("cannot be NULL"));
    for body in [
        "BEGIN FOR i IN 1..3 LOOP i := 2; END LOOP; RETURN 1; END;",
        "BEGIN FOR i IN 1..3 LOOP NULL; END LOOP; RETURN i; END;",
        "BEGIN FOR i IN TRUE..3 LOOP NULL; END LOOP; RETURN 1; END;",
    ] {
        assert!(compile(&function("", "NUMBER", body)).is_err(), "{body}");
    }
}

#[test]
fn pls_integer_rounds_inputs_preserves_null_and_checks_expression_overflow() {
    let source = function(
        "p PLS_INTEGER",
        "PLS_INTEGER",
        "v BINARY_INTEGER := p; BEGIN RETURN v; END;",
    );
    for (input, expected) in [
        (Value::Null, Value::Null),
        (Value::Numeric("1.5".into()), Value::Int(2)),
        (Value::Numeric("-1.5".into()), Value::Int(-2)),
        (Value::Int(i32::MIN as i64), Value::Int(i32::MIN as i64)),
        (Value::Int(i32::MAX as i64), Value::Int(i32::MAX as i64)),
    ] {
        assert_eq!(call(&source, &[input]).unwrap(), expected);
    }
    assert!(matches!(
        call(&source, &[Value::Int(i32::MAX as i64 + 1)]),
        Err(mpedb_types::Error::ArithmeticOverflow)
    ));
    for body in [
        "BEGIN RETURN 50000 * 50000; END;",
        "BEGIN FOR i IN 50000..50000 LOOP RETURN i * i; END LOOP; END;",
        "BEGIN FOR i IN 2147483647..2147483647 LOOP RETURN i + 1; END LOOP; END;",
        "BEGIN FOR i IN -2147483648..-2147483648 LOOP RETURN -i; END LOOP; END;",
        "v PLS_INTEGER := -2147483648; BEGIN RETURN - -v; END;",
        "BEGIN FOR i IN 2147483648..1 LOOP NULL; END LOOP; RETURN 0; END;",
    ] {
        assert!(
            matches!(
                call(&function("", "NUMBER", body), &[]),
                Err(mpedb_types::Error::ArithmeticOverflow)
            ),
            "{body}"
        );
    }
    let source = function(
        "",
        "NUMBER",
        "BEGIN FOR i IN 2147483647..2147483647 LOOP RETURN i + 1.0; END LOOP; END;",
    );
    assert_eq!(
        call(&source, &[]).unwrap(),
        Value::Numeric("2147483648".into())
    );
    let source = function(
        "",
        "NUMBER",
        "BEGIN FOR i IN 3..3 LOOP RETURN i / 2; END LOOP; END;",
    );
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("1.5".into()));
}

#[test]
fn varchar_normalizes_empty_text_and_concatenates_null_without_losing_spaces() {
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2(10 CHAR) := p; BEGIN RETURN q'[ø]' || v || ' '; END;",
    );
    for (input, expected) in [
        (Value::Null, "ø "),
        (Value::Text("".into()), "ø "),
        (Value::Text("abc".into()), "øabc "),
    ] {
        assert_eq!(
            call(&source, &[input]).unwrap(),
            Value::Text(expected.into())
        );
    }
    let source = function("", "VARCHAR2", "BEGIN RETURN NULL || ''; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Null);
    let source = function("", "BOOLEAN", "BEGIN RETURN '' IS NULL; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Bool(true));
}

#[test]
fn varchar_byte_and_character_limits_enforce_utf8_and_absolute_byte_bound() {
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2(3 CHAR) := p; BEGIN RETURN v; END;",
    );
    assert_eq!(
        call(&source, &[Value::Text("øø".into())]).unwrap(),
        Value::Text("øø".into())
    );
    assert!(call(&source, &[Value::Text("øøøø".into())]).is_err());
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2(3 BYTE) := p; BEGIN RETURN v; END;",
    );
    assert!(call(&source, &[Value::Text("øø".into())]).is_err());
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2(3) := p; BEGIN RETURN v; END;",
    );
    assert!(call(&source, &[Value::Text("øø".into())]).is_err());
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2 := p; BEGIN RETURN v; END;",
    );
    let large = Value::Text("a".repeat(32767));
    assert_eq!(call(&source, std::slice::from_ref(&large)).unwrap(), large);
    assert!(call(&source, &[Value::Text("a".repeat(32768))]).is_err());
    assert!(call(&source, &[Value::Int(1)]).is_err());
    let source = function(
        "p VARCHAR2",
        "VARCHAR2",
        "v VARCHAR2(32767 CHAR) := p; BEGIN RETURN v || v; END;",
    );
    assert!(call(&source, &[Value::Text("ø".repeat(9000))]).is_err());
}

#[test]
fn literal_blank_padding_is_distinct_from_varchar_and_concat_comparison() {
    for (expression, expected) in [
        ("'a' = 'a '", Some(true)),
        ("'a ' <> 'a'", Some(false)),
        ("'a' <= 'a '", Some(true)),
        ("'a ' >= 'a'", Some(true)),
        ("'a' > 'a '", Some(false)),
        ("'a ' < 'a'", Some(false)),
        ("'a\t' < 'a '", Some(true)),
        ("'a' || '' = 'a '", Some(false)),
        ("'a' || 'b' = 'ab '", Some(false)),
        ("'' = ''", None),
    ] {
        let source = function("", "BOOLEAN", &format!("BEGIN RETURN {expression}; END;"));
        assert_eq!(
            call(&source, &[]).unwrap(),
            expected.map_or(Value::Null, Value::Bool),
            "{expression}"
        );
    }
    let source = function("p VARCHAR2", "BOOLEAN", "BEGIN RETURN p = 'a '; END;");
    assert_eq!(
        call(&source, &[Value::Text("a".into())]).unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        call(&source, &[Value::Text("a ".into())]).unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn varchar_output_parameters_preserve_the_procedure_copy_out_contract() {
    let source = "CREATE PROCEDURE p(a IN OUT VARCHAR2, b OUT VARCHAR2) IS BEGIN b := a || '!'; a := ''; END;";
    assert_eq!(
        call_result(source, &[Value::Text("ø".into()), Value::Int(8)]).unwrap(),
        tuple(vec![Value::Null, Value::Text("ø!".into())])
    );
}

#[test]
fn varchar_declarations_and_static_types_reject_unsupported_coercions() {
    for source in [
        function("p VARCHAR2(10)", "VARCHAR2", "BEGIN RETURN p; END;"),
        function("", "VARCHAR2(10)", "BEGIN RETURN 'x'; END;"),
        function("", "VARCHAR2", "v VARCHAR2(0); BEGIN RETURN v; END;"),
        function("", "VARCHAR2", "v VARCHAR2(32768); BEGIN RETURN v; END;"),
        function("", "VARCHAR2", "v VARCHAR2(1.5); BEGIN RETURN v; END;"),
        function("", "VARCHAR2", "BEGIN RETURN N'x'; END;"),
        function("", "VARCHAR2", "BEGIN RETURN 'x' || 1; END;"),
        function("", "NUMBER", "BEGIN RETURN '1'; END;"),
        function("", "BOOLEAN", "BEGIN RETURN TRUE OR 'x' = 1; END;"),
    ] {
        assert!(compile(&source).is_err(), "{source}");
    }
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
fn number_special_arithmetic_uses_the_initial_anonymous_block_profile() {
    for (expression, expected) in [
        ("p + m", "-Infinity"),
        ("m + p", "-Infinity"),
        ("p - p", "0"),
        ("m - m", "0"),
        ("p - m", "Infinity"),
        ("m - p", "-Infinity"),
        ("p * 0", "0"),
        ("m * -1", "Infinity"),
        ("p / p", "Infinity"),
        ("p / m", "-Infinity"),
        ("1 / p", "0"),
        ("-m", "Infinity"),
    ] {
        let source = function("", "NUMBER", &format!("a NUMBER := 1e125; p NUMBER; m NUMBER; BEGIN p := a * a; m := -p; RETURN {expression}; END;"));
        assert_eq!(
            call(&source, &[]).unwrap(),
            Value::Numeric(expected.into()),
            "{expression}"
        );
    }
    let source = function(
        "p NUMBER",
        "BOOLEAN",
        "BEGIN RETURN p > 1e125 AND p = p; END;",
    );
    assert_eq!(
        call(&source, &[Value::Numeric("Infinity".into())]).unwrap(),
        Value::Bool(true)
    );
    for body in [
        "a NUMBER := 1e125; n NUMBER; i PLS_INTEGER; BEGIN n := a*a; i := n; RETURN i; END;",
        "a NUMBER := 1e125; n NUMBER; BEGIN n := a*a; FOR i IN n..n LOOP NULL; END LOOP; RETURN 0; END;",
    ] {
        assert!(matches!(call(&function("", "NUMBER", body), &[]), Err(mpedb_types::Error::ArithmeticOverflow)));
    }
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
    assert_eq!(
        call(
            &function("", "NUMBER", "BEGIN RETURN 1e125 * 100; END;"),
            &[]
        )
        .unwrap(),
        Value::Numeric("Infinity".into())
    );
}

#[test]
fn plsql_division_literal_and_multiplication_underflow_become_zero() {
    assert_eq!(
        call(
            &function("", "NUMBER", "BEGIN RETURN 1e-100 / 1e100; END;"),
            &[]
        )
        .unwrap(),
        Value::Numeric("0".into())
    );
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
fn number_literals_and_nan_reject_while_special_results_round_trip() {
    for text in ["1e126", "-1e126"] {
        assert!(call(
            &function("", "NUMBER", &format!("BEGIN RETURN {text}; END;")),
            &[]
        )
        .is_err());
    }
    let source = function("p NUMBER", "NUMBER", "BEGIN RETURN p; END;");
    for text in ["NaN", "garbage", "1e126", "-1e126"] {
        assert!(call(&source, &[Value::Numeric(text.into())]).is_err());
    }
    for text in ["Infinity", "-Infinity"] {
        assert_eq!(
            call(&source, &[Value::Numeric(text.into())]).unwrap(),
            Value::Numeric(text.into())
        );
    }
    assert!(compile(&function("", "NUMBER", "BEGIN RETURN 1e126; END;")).is_err());
    assert!(call(&source, &[Value::Float(0.1)]).is_err());
}

#[test]
fn native_compilation_is_safe_on_small_embedding_thread_stacks() {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            for (nesting, valid) in [(100, true), (200, false)] {
                let source = function(
                    "",
                    "BOOLEAN",
                    &format!(
                        "BEGIN RETURN {}TRUE{}; END;",
                        "(".repeat(nesting),
                        ")".repeat(nesting)
                    ),
                );
                assert_eq!(compile(&source).is_ok(), valid);
            }
        })
        .unwrap()
        .join()
        .unwrap();
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

#[test]
fn exception_handlers_match_named_alternatives_and_rethrow_to_outer_blocks() {
    for (body,expected) in [
        ("BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 1; WHEN ZERO_DIVIDE OR TOO_MANY_ROWS THEN RETURN 2; WHEN OTHERS THEN RETURN 3; END;","2"),
        ("BEGIN BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 1; END; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 4; END;","4"),
        ("BEGIN BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN NO_DATA_FOUND THEN RAISE; END; EXCEPTION WHEN OTHERS THEN RETURN 5; END;","5"),
        ("n NUMBER; BEGIN n:=10/0; RETURN 0; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 6; END;","6"),
        ("v VARCHAR2(1); BEGIN v:='too long'; RETURN 0; EXCEPTION WHEN VALUE_ERROR THEN RETURN 7; END;","7"),
        ("BEGIN DECLARE v VARCHAR2(1):='too long'; BEGIN RETURN 0; EXCEPTION WHEN OTHERS THEN RETURN 1; END; EXCEPTION WHEN VALUE_ERROR THEN RETURN 8; END;","8"),
        ("BEGIN BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN RAISE NO_DATA_FOUND; END; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 9; END;","9"),
    ] {
        assert_eq!(call(&function("","NUMBER",body),&[]).unwrap(),Value::Numeric(expected.into()),"{body}");
    }
}

#[test]
fn leaving_exception_blocks_via_loop_control_does_not_leave_stale_handlers() {
    let body="n NUMBER:=0; BEGIN FOR i IN 1..3 LOOP BEGIN IF i=1 THEN CONTINUE; END IF; EXIT; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 99; END; END LOOP; n:=1/0; RETURN 0; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 10; END;";
    assert_eq!(call(&function("","NUMBER",body),&[]).unwrap(),Value::Numeric("10".into()));
    // Normal handler completion also ends its current-exception context.
    let body="n NUMBER:=0; BEGIN BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN n:=1; END; BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN NO_DATA_FOUND THEN RAISE; END; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN n+10; END;";
    assert_eq!(call(&function("","NUMBER",body),&[]).unwrap(),Value::Numeric("11".into()));
}

#[test]
fn malformed_handlers_and_raise_outside_a_handler_refuse() {
    for body in [
        "BEGIN RAISE; END;",
        "BEGIN NULL; EXCEPTION END;",
        "BEGIN NULL; EXCEPTION WHEN OTHERS THEN NULL; WHEN ZERO_DIVIDE THEN NULL; END;",
        "BEGIN NULL; EXCEPTION WHEN ZERO_DIVIDE OR ZERO_DIVIDE THEN NULL; END;",
        "BEGIN NULL; EXCEPTION WHEN OTHERS OR ZERO_DIVIDE THEN NULL; END;",
        "BEGIN NULL; EXCEPTION WHEN unknown_exception THEN NULL; END;",
    ] {assert!(compile(&function("","NUMBER",body)).is_err(),"{body}");}
}
