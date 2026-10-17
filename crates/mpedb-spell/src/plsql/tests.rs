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
        "BEGIN NULL; EXCEPTION WHEN OTHERS OR ZERO_DIVIDE THEN NULL; END;",
        "BEGIN NULL; EXCEPTION WHEN unknown_exception THEN NULL; END;",
    ] {assert!(compile(&function("","NUMBER",body)).is_err(),"{body}");}
}

#[test]
fn sqlcode_and_sqlerrm_follow_handler_scope_and_no_data_found_sign() {
    for (body,expected) in [
        ("BEGIN RETURN SQLCODE; END;","0"),
        ("BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN OTHERS THEN RETURN SQLCODE(); END;","100"),
        ("BEGIN RAISE TOO_MANY_ROWS; EXCEPTION WHEN OTHERS THEN RETURN SQLCODE; END;","-1422"),
        ("n NUMBER:=0; BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN NO_DATA_FOUND THEN n:=SQLCODE; END; RETURN n+SQLCODE; END;","-1376"),
        ("BEGIN BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN OTHERS THEN NULL; END; RETURN SQLCODE; END;","0"),
    ] {assert_eq!(call(&function("","NUMBER",body),&[]).unwrap(),Value::Numeric(expected.into()),"{body}");}
    for (body,expected) in [
        ("BEGIN RETURN SQLERRM(); END;","ORA-0000: normal, successful completion"),
        ("BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;","ORA-01403: no data found"),
        ("n NUMBER; BEGIN n:=1/0; RETURN ''; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN SQLERRM; END;","ORA-01476: divisor is equal to zero"),
    ] {assert_eq!(call(&function("","VARCHAR2",body),&[]).unwrap(),Value::Text(expected.into()),"{body}");}
}

#[test]
fn application_errors_preserve_codes_messages_rounding_and_utf8_limits() {
    for (code,expected) in [("-20000","-20000"),("-20999","-20999"),("-20000.5","-20001"),("-19999","-21000"),("NULL","-21000")] {
        let body=format!("BEGIN RAISE_APPLICATION_ERROR({code},'ø',FALSE); EXCEPTION WHEN OTHERS THEN RETURN SQLCODE; END;");
        assert_eq!(call(&function("","NUMBER",&body),&[]).unwrap(),Value::Numeric(expected.into()));
    }
    let body="BEGIN RAISE_APPLICATION_ERROR(-20001,'ø'); EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;";
    assert_eq!(call(&function("","VARCHAR2",body),&[]).unwrap(),Value::Text("ORA-20001: ø".into()));
    let body="BEGIN RAISE_APPLICATION_ERROR(-19999,'ignored'); EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;";
    assert_eq!(call(&function("","VARCHAR2",body),&[]).unwrap(),Value::Text("ORA-21000: error number argument to raise_application_error of -19999 is out of range".into()));
    let long="ø".repeat(1500);
    let body=format!("BEGIN RAISE_APPLICATION_ERROR(-20001,'{long}'); EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;");
    let Value::Text(text)=call(&function("","VARCHAR2",&body),&[]).unwrap() else{panic!("text required")};
    assert_eq!(text.len(),511);assert!(text.starts_with("ORA-20001: "));
    let body=format!("BEGIN RAISE_APPLICATION_ERROR(-20001,'{long}'); END;");
    let error=call(&function("","NUMBER",&body),&[]).unwrap_err();
    assert!(matches!(error,mpedb_types::Error::Oracle{code:20001,ref message} if message.len()==2034));
    assert!(compile(&function("","NUMBER","BEGIN RAISE_APPLICATION_ERROR(-20001,'x',TRUE); END;")).is_err());
}

#[test]
fn constrained_numbers_round_assignments_before_checking_precision() {
    for (ty, input, expected) in [
        ("NUMBER(4,2)", "99.994", "99.99"),
        ("NUMBER(4,2)", "99.995", "-6502"),
        ("NUMBER(4,2)", "-12.345", "-12.35"),
        ("NUMBER(3,-2)", "99949", "99900"),
        ("NUMBER(3,-2)", "99950", "-6502"),
        ("NUMBER(3,-2)", "-150", "-200"),
        ("NUMBER(4,5)", "0.012345", "0.01235"),
        ("NUMBER(4,5)", "0.099995", "-6502"),
        ("NUMBER(2)", "99.4", "99"),
        ("NUMBER(2)", "99.5", "-6502"),
        ("NUMBER(1,+2)", "0.004", "0"),
    ] {
        let source = function("p NUMBER", "NUMBER", &format!("n {ty}; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN -6502; END;"));
        assert_eq!(call(&source, &[Value::Numeric(input.into())]).unwrap(), Value::Numeric(expected.into()), "{ty}: {input}");
        assert_eq!(call(&source, &[Value::Null]).unwrap(), Value::Null);
    }
    let source = function("", "NUMBER", "n NUMBER(3,1):=1.24; BEGIN n:=n*10+0.05; RETURN n; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("12.1".into()));
    let source = function("", "NUMBER", "n CONSTANT NUMBER(4,2):=-1.235; BEGIN RETURN n; END;");
    assert_eq!(call(&source, &[]).unwrap(), Value::Numeric("-1.24".into()));
}

#[test]
fn constrained_number_declarations_reject_invalid_or_formal_constraints() {
    for ty in ["NUMBER(0)", "NUMBER(39)", "NUMBER(-1)", "NUMBER(1.0)", "NUMBER(2,-85)", "NUMBER(2,128)", "NUMBER(2,1.5)", "NUMBER(*,2)", "NUMBER(2,32768)"] {
        assert!(compile(&function("", "NUMBER", &format!("n {ty}; BEGIN RETURN n; END;"))).is_err(), "{ty}");
    }
    assert!(compile(&function("p NUMBER(4,2)", "NUMBER", "BEGIN RETURN p; END;")).is_err());
    assert!(compile(&function("", "NUMBER(4,2)", "BEGIN RETURN 1; END;")).is_err());
}

#[test]
fn oracle_integer_and_decimal_aliases_apply_local_constraints_only() {
    for ty in ["INTEGER", "INT", "SMALLINT", "DEC", "DECIMAL", "NUMERIC"] {
        let local = function("p NUMBER", "NUMBER", &format!("n {ty}; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN -6502; END;"));
        for (input, expected) in [("1.5","2"),("-1.5","-2"),("100000000000000000000","100000000000000000000"),("1e39","-6502")] {
            assert_eq!(call(&local,&[Value::Numeric(mpedb_types::value::parse_numeric(input).unwrap())]).unwrap(),Value::Numeric(expected.into()),"{ty}: {input}");
        }
        let formal = function(&format!("p {ty}"),ty,"BEGIN RETURN p; END;");
        for input in ["1.5","-1.5","1e39"] {
            let value = Value::Numeric(mpedb_types::value::parse_numeric(input).unwrap());
            assert_eq!(call(&formal,std::slice::from_ref(&value)).unwrap(),value,"formal {ty}");
        }
    }
    let source = function("", "NUMBER", "n DECIMAL(4,2):=1.235; BEGIN RETURN n; END;");
    assert_eq!(call(&source,&[]).unwrap(),Value::Numeric("1.24".into()));
    for ty in ["INTEGER(3)","SMALLINT(2)","DECIMAL(39)","NUMERIC(4,-85)"] {
        assert!(compile(&function("","NUMBER",&format!("n {ty}; BEGIN RETURN n; END;"))).is_err());
    }
}

#[test]
fn not_null_locals_check_coerced_values_and_preserve_the_old_value_on_failure() {
    let source=function("p NUMBER","NUMBER","n NUMBER NOT NULL:=7; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;");
    assert_eq!(
        call(&source, &[Value::Null]).unwrap(),
        Value::Numeric("7".into())
    );
    assert_eq!(
        call(&source, &[Value::Numeric("3.5".into())]).unwrap(),
        Value::Numeric("3.5".into())
    );
    let text=function("p VARCHAR2","VARCHAR2","n VARCHAR2(8 CHAR) NOT NULL DEFAULT 'old'; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;");
    for value in [Value::Null, Value::Text(String::new())] {
        assert_eq!(call(&text, &[value]).unwrap(), Value::Text("old".into()));
    }
    assert_eq!(
        call(&text, &[Value::Text("ø🙂漢".into())]).unwrap(),
        Value::Text("ø🙂漢".into())
    );
    let boolean=function("p BOOLEAN","BOOLEAN","n BOOLEAN NOT NULL:=TRUE; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;");
    assert_eq!(call(&boolean, &[Value::Null]).unwrap(), Value::Bool(true));
    assert_eq!(
        call(&boolean, &[Value::Bool(false)]).unwrap(),
        Value::Bool(false)
    );
}
#[test]
fn not_null_declarations_require_initializers_and_literal_null_assignments_are_compile_errors() {
    for body in [
        "n NUMBER NOT NULL; BEGIN RETURN 1; END;",
        "n NUMBER NOT NULL:=NULL; BEGIN RETURN 1; END;",
        "n NUMBER NOT NULL:=(NULL); BEGIN RETURN 1; END;",
        "n NUMBER NOT NULL:=1; BEGIN n:=NULL; RETURN n; END;",
        "n NUMBER NOT NULL:=1; BEGIN IF FALSE THEN n:=NULL; END IF; RETURN n; END;",
    ] {
        assert!(compile(&function("", "NUMBER", body)).is_err(), "{body}");
    }
    let constant = function(
        "",
        "NUMBER",
        "n CONSTANT NUMBER NOT NULL:=2; BEGIN RETURN n; END;",
    );
    assert_eq!(call(&constant, &[]).unwrap(), Value::Numeric("2".into()));
    let source = function(
        "",
        "VARCHAR2",
        "n VARCHAR2(4) NOT NULL:=''; BEGIN RETURN n; END;",
    );
    assert!(matches!(
        call(&source, &[]),
        Err(mpedb_types::Error::Oracle { code: 6502, .. })
    ));
}
#[test]
fn not_null_initialization_errors_escape_their_own_block_handlers() {
    let source=function("p NUMBER","NUMBER","BEGIN DECLARE n NUMBER NOT NULL:=p; BEGIN RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN 101; END; EXCEPTION WHEN VALUE_ERROR THEN RETURN 202; END;");
    assert_eq!(
        call(&source, &[Value::Null]).unwrap(),
        Value::Numeric("202".into())
    );
    assert_eq!(
        call(&source, &[Value::Numeric("3".into())]).unwrap(),
        Value::Numeric("3".into())
    );
}

#[test]
fn integer_numeric_overflow_is_distinct_from_value_error_in_handlers() {
    let source=function("p NUMBER","NUMBER","n PLS_INTEGER NOT NULL:=7; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; WHEN OTHERS THEN RETURN SQLCODE; END;");
    assert_eq!(
        call(&source, &[Value::Null]).unwrap(),
        Value::Numeric("7".into())
    );
    assert_eq!(
        call(&source, &[Value::Numeric("2147483648".into())]).unwrap(),
        Value::Numeric("-1426".into())
    );
    let arithmetic=function("","VARCHAR2","n PLS_INTEGER:=2147483647; BEGIN n:=n+1; RETURN 'bad'; EXCEPTION WHEN VALUE_ERROR THEN RETURN 'wrong handler'; WHEN OTHERS THEN RETURN SQLERRM; END;");
    assert_eq!(
        call(&arithmetic, &[]).unwrap(),
        Value::Text("ORA-01426: numeric overflow".into())
    );
}

#[test]
fn user_exceptions_preserve_lexical_identity_and_diagnostic_kind() {
    for (body, expected) in [
        ("e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN DUP_VAL_ON_INDEX THEN RETURN -99; WHEN e THEN RETURN SQLCODE; END;", "1"),
        ("e EXCEPTION; BEGIN RAISE DUP_VAL_ON_INDEX; EXCEPTION WHEN e THEN RETURN -99; WHEN DUP_VAL_ON_INDEX THEN RETURN SQLCODE; END;", "-1"),
        ("e EXCEPTION; BEGIN DECLARE e EXCEPTION; BEGIN RAISE e; END; EXCEPTION WHEN e THEN RETURN -99; WHEN OTHERS THEN RETURN SQLCODE; END;", "1"),
        ("e EXCEPTION; BEGIN BEGIN RAISE e; EXCEPTION WHEN OTHERS THEN DECLARE e EXCEPTION; BEGIN RAISE; EXCEPTION WHEN e THEN RETURN -99; WHEN OTHERS THEN RETURN 2; END; END; END;", "2"),
        ("e NUMBER:=7; BEGIN DECLARE e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN e THEN NULL; END; RETURN e; END;", "7"),
        ("ZERO_DIVIDE EXCEPTION; BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN SQLCODE; END;", "1"),
        ("e EXCEPTION; BEGIN BEGIN RAISE e; EXCEPTION WHEN e THEN RAISE; END; EXCEPTION WHEN e THEN RETURN 9; END;", "9"),
    ] {
        assert_eq!(call(&function("", "NUMBER", body), &[]).unwrap(), Value::Numeric(expected.into()), "{body}");
    }
    assert_eq!(
        call(
            &function(
                "",
                "VARCHAR2",
                "e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN e THEN RETURN SQLERRM; END;"
            ),
            &[]
        )
        .unwrap(),
        Value::Text("User-Defined Exception".into())
    );
    let error = call(
        &function("", "NUMBER", "e EXCEPTION; BEGIN RAISE e; END;"),
        &[],
    )
    .unwrap_err();
    assert!(matches!(
        error,
        mpedb_types::Error::OracleUserException { .. }
    ));
    assert_eq!(
        error.to_string(),
        "ORA-06510: PL/SQL: unhandled user-defined exception"
    );
}

#[test]
fn user_exception_names_share_declaration_namespace_and_cannot_be_values() {
    for body in [
        "e EXCEPTION; e EXCEPTION; BEGIN RETURN 1; END;",
        "e NUMBER; e EXCEPTION; BEGIN RETURN 1; END;",
        "e EXCEPTION; e NUMBER; BEGIN RETURN 1; END;",
        "e EXCEPTION; BEGIN RETURN e; END;",
        "e EXCEPTION; BEGIN DECLARE e NUMBER; BEGIN RAISE e; END; RETURN 1; END;",
        "BEGIN DECLARE e EXCEPTION; BEGIN NULL; END; RAISE e; END;",
    ] {
        assert!(compile(&function("", "NUMBER", body)).is_err(), "{body}");
    }
}

#[test]
fn exception_init_codes_aliases_and_entry_boundaries() {
    assert_eq!(crate::interp::oracle_exception_init_code("-1.499999999f").unwrap(), 2);
    assert_eq!(crate::interp::oracle_exception_init_code("-1.499999999d").unwrap(), 1);
    assert_eq!(crate::interp::oracle_exception_init_code("-1.499999999").unwrap(), 1);
    for (body,expected) in [
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);BEGIN RAISE_APPLICATION_ERROR(-20001,'x');EXCEPTION WHEN e THEN RETURN SQLCODE;END;","-20001"),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,100);BEGIN RAISE NO_DATA_FOUND;EXCEPTION WHEN e THEN RETURN SQLCODE;END;","100"),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1476);n NUMBER;BEGIN n:=1/0;RETURN 0;EXCEPTION WHEN e THEN RETURN SQLCODE;END;","-1476"),
        ("e EXCEPTION;f EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);PRAGMA EXCEPTION_INIT(f,-20001);BEGIN RAISE e;EXCEPTION WHEN e OR f THEN RETURN SQLCODE;END;","-20001"),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1);BEGIN RAISE e;EXCEPTION WHEN e OR DUP_VAL_ON_INDEX THEN RETURN SQLCODE;END;","-1"),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);PRAGMA EXCEPTION_INIT(e,-20002);BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN SQLCODE;END;","-20002"),
        ("e EXCEPTION;BEGIN RAISE e;EXCEPTION WHEN e OR e THEN RETURN SQLCODE;END;","1"),
    ] { assert_eq!(call(&function("","NUMBER",body),&[]).unwrap(),Value::Numeric(expected.into()),"{body}"); }
    for token in [
        "-1", "-1.0", "-1e0", "-1f", "-1d", "-0.9", "-1.5", "-999999", "-1000000", "+100",
    ] {
        let code = crate::interp::oracle_exception_init_code(token).unwrap();
        let expected = if code == 1403 { 100 } else { -i64::from(code) };
        let body=format!("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,{token});BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN SQLCODE;END;");
        assert_eq!(
            call(&function("", "NUMBER", &body), &[]).unwrap(),
            Value::Numeric(expected.to_string())
        );
    }
    for code in [65536, 1000000] {
        let body = format!("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-{code});BEGIN RAISE e;END;");
        let error = call(&function("", "NUMBER", &body), &[]).unwrap_err();
        assert!(matches!(
            error,
            mpedb_types::Error::Oracle { code: 6515, .. }
        ));
        assert_eq!(
            error.to_string(),
            format!("ORA-06515: PL/SQL: unhandled exception {code}")
        );
    }
}

#[test]
fn exception_init_rejects_invalid_codes_scope_and_separate_alias_handlers() {
    for token in [
        "0", "1", "-1403", "-1000001", "-1e100", "(-1)", "-20000-1", "100.5",
    ] {
        assert!(
            compile(&function(
                "",
                "NUMBER",
                &format!("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,{token});BEGIN RETURN 0;END;")
            ))
            .is_err(),
            "{token}"
        );
    }
    for body in [
        "PRAGMA EXCEPTION_INIT(e,-1);e EXCEPTION;BEGIN RETURN 0;END;",
        "e NUMBER;PRAGMA EXCEPTION_INIT(e,-1);BEGIN RETURN 0;END;",
        "PRAGMA EXCEPTION_INIT(NO_DATA_FOUND,100);BEGIN RETURN 0;END;",
        "e EXCEPTION;BEGIN DECLARE PRAGMA EXCEPTION_INIT(e,-1);BEGIN RETURN 0;END;END;",
        "e EXCEPTION;f EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1);PRAGMA EXCEPTION_INIT(f,-1);BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN 1;WHEN f THEN RETURN 2;END;",
        "e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1);BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN 1;WHEN DUP_VAL_ON_INDEX THEN RETURN 2;END;",
    ] {assert!(compile(&function("","NUMBER",body)).is_err(),"{body}");}
}

#[test]
fn every_predefined_exception_resolves_catches_and_renders_native_text() {
    for &(name, code, message) in crate::oracle_exceptions::PREDEFINED {
        let body = format!("BEGIN RAISE {name}; EXCEPTION WHEN {name} THEN RETURN SQLCODE; END;");
        let expected = if code == 1403 { 100 } else { -i64::from(code) };
        assert_eq!(
            call(&function("", "NUMBER", &body), &[]).unwrap(),
            Value::Numeric(expected.to_string()),
            "{name}"
        );
        let body = format!("BEGIN RAISE {name}; EXCEPTION WHEN {name} THEN RETURN SQLERRM; END;");
        assert_eq!(
            call(&function("", "VARCHAR2", &body), &[]).unwrap(),
            Value::Text(format!("ORA-{code:05}: {message}")),
            "{name}"
        );
    }
}
#[test]
fn sqlerrm_arguments_preserve_null_rounding_sign_and_current_error_detail() {
    for (arg, expected) in [
        ("NULL", None),
        ("0", Some("ORA-0000: normal, successful completion")),
        ("1", Some("User-Defined Exception")),
        ("2", Some(" -2: non-ORACLE exception ")),
        ("100", Some("ORA-01403: no data found")),
        ("-1403", Some("ORA-01403: no data found")),
        ("1403", Some(" -1403: non-ORACLE exception ")),
        ("-20001", Some("ORA-20001: ")),
        ("20001", Some(" -20001: non-ORACLE exception ")),
        ("-65536", Some(" 65536: non-ORACLE exception ")),
        ("-2147483648", Some(" -2147483648: non-ORACLE exception ")),
        ("2147483647", Some(" -2147483647: non-ORACLE exception ")),
        (
            "-1.5",
            Some("ORA-00002: Message 2 not found;  product=RDBMS; facility=ORA"),
        ),
        ("1.5", Some(" -2: non-ORACLE exception ")),
    ] {
        let body = format!("BEGIN RETURN SQLERRM({arg}); END;");
        assert_eq!(
            call(&function("", "VARCHAR2", &body), &[]).unwrap(),
            expected.map_or(Value::Null, |s| Value::Text(s.into())),
            "{arg}"
        );
    }
    for arg in ["SQLCODE", "-20001"] {
        let body=format!("BEGIN RAISE_APPLICATION_ERROR(-20001,'ø detail');EXCEPTION WHEN OTHERS THEN RETURN SQLERRM({arg});END;");
        assert_eq!(
            call(&function("", "VARCHAR2", &body), &[]).unwrap(),
            Value::Text("ORA-20001: ø detail".into())
        );
    }
    let body = "BEGIN RETURN SQLERRM(2147483648);EXCEPTION WHEN OTHERS THEN RETURN SQLERRM;END;";
    assert_eq!(
        call(&function("", "VARCHAR2", body), &[]).unwrap(),
        Value::Text("ORA-01426: numeric overflow".into())
    );
    assert!(compile(&function("", "NUMBER", "BEGIN RETURN SQLCODE(1);END;")).is_err());
}

#[test]
fn standard_qualification_reaches_predefined_exception_under_local_shadowing() {
    let body="ZERO_DIVIDE EXCEPTION;BEGIN RAISE STANDARD.ZERO_DIVIDE;EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 999;WHEN STANDARD.ZERO_DIVIDE THEN RETURN SQLCODE;END;";
    assert_eq!(
        call(&function("", "NUMBER", body), &[]).unwrap(),
        Value::Numeric("-1476".into())
    );
    assert!(compile(&function(
        "",
        "NUMBER",
        "STANDARD NUMBER;BEGIN RAISE STANDARD.ZERO_DIVIDE;END;"
    ))
    .is_err());
}

#[test]
fn anonymous_block_treats_an_uninitialized_variable_as_null() {
    let err = call(
        "DECLARE v NUMBER; BEGIN IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; END;",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ORA-01403: no data found"), "{err}");

    assert_eq!(
        call(
            "DECLARE v NUMBER := 4; BEGIN IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; END;",
            &[],
        )
        .unwrap(),
        Value::Null
    );
    let err = call(
        "DECLARE v NUMBER := 4; BEGIN v := NULL; IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; END;",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ORA-01403: no data found"), "{err}");

    assert_eq!(
        call(
            "DECLARE v NUMBER; BEGIN BEGIN IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; RAISE ZERO_DIVIDE; EXCEPTION WHEN NO_DATA_FOUND THEN NULL; END; END;",
            &[],
        )
        .unwrap(),
        Value::Null
    );
    let err = call(
        "DECLARE v NUMBER := 4; BEGIN BEGIN IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; RAISE ZERO_DIVIDE; EXCEPTION WHEN NO_DATA_FOUND THEN NULL; END; END;",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ORA-01476: divisor is equal to zero"), "{err}");

    assert_eq!(
        call(
            "DECLARE n NUMBER NOT NULL := 7; v NUMBER; BEGIN n := v; RAISE ZERO_DIVIDE; EXCEPTION WHEN VALUE_ERROR THEN NULL; END;",
            &[],
        )
        .unwrap(),
        Value::Null
    );
    let err = call(
        "DECLARE n NUMBER NOT NULL := 7; v NUMBER; BEGIN n := v; END;",
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ORA-06502:"), "{err}");

    assert_eq!(
        call(
            "DECLARE v NUMBER := 4; BEGIN DECLARE v NUMBER; BEGIN IF v IS NOT NULL THEN RAISE ZERO_DIVIDE; END IF; END; IF v IS NULL THEN RAISE NO_DATA_FOUND; END IF; END;",
            &[],
        )
        .unwrap(),
        Value::Null
    );
    assert_eq!(
        call(
            "DECLARE v NUMBER; BEGIN IF v IS NULL THEN RETURN; END IF; RAISE NO_DATA_FOUND; END;",
            &[],
        )
        .unwrap(),
        Value::Null
    );
    assert_eq!(call("BEGIN NULL; END;", &[]).unwrap(), Value::Null);
    assert_eq!(call("BEGIN\nNULL;\nEND;\n/\n", &[]).unwrap(), Value::Null);

    for (source, needle) in [
        ("BEGIN NULL; END block;", "cannot have an END name"),
        ("BEGIN RETURN 1; END;", "expected ;"),
        ("BEGIN NULL; END; DROP TABLE t;", "trailing input"),
        ("BEGIN NULL; END; /", "its own line"),
        (
            "DECLARE v NUMBER; v NUMBER; BEGIN NULL; END;",
            "duplicate parameter or variable name",
        ),
    ] {
        let err = compile(source).unwrap_err().to_string();
        assert!(err.contains("PL/SQL at "), "{needle}: {err}");
        assert!(err.contains(needle), "{needle}: {err}");
    }
    let err = super::compile_function("BEGIN NULL; END;")
        .unwrap_err()
        .to_string();
    assert!(err.contains("PL/SQL at "), "{err}");
    assert!(err.contains("anonymous block"), "{err}");
}
