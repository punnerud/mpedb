//! Native compiler tests against public PL/SQL semantics, not a server oracle.

use super::compile;
use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
use crate::ir::Proc;
use mpedb_types::{Result, Value};

fn call(source: &str, args: &[Value]) -> Result<Value> {
    let s = compile(source)?;
    assert!(s.calls.is_empty());
    let p = Proc::new(s.name, s.argc, s.nlocals, vec![], s.consts, s.instrs)?;
    let p = Proc::decode(&p.encode())?;
    match interp::run(
        &p,
        args,
        &mut MockBridge::new(),
        Budget {
            instrs: 1000,
            db_calls: 0,
            rows: 0,
        },
    )? {
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
    assert!(
        compile(&function("p OUT NUMBER", "NUMBER", "BEGIN RETURN p; END;"))
            .unwrap_err()
            .to_string()
            .contains("OUT")
    );
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
fn number_range_and_rounding_gaps_raise_instead_of_returning_wrong_values() {
    for text in ["1e126", "1e-131", "123456789012345678901234567890123456789"] {
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
