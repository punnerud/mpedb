//! PL/pgSQL frontend tests.
//!
//! Every accepted form is EXECUTED against the interpreter rather than merely
//! compiled, because a frontend that emits well-formed nonsense compiles fine.
//! Expected values are what PostgreSQL 16 returns for the same function — the
//! integer-division and modulo cases especially, where PL/pgSQL follows
//! PostgreSQL's truncate-toward-zero rule and not Python's floor.

use crate::interp::testutil::MockBridge;
use crate::interp::{self, Budget, ProcValue};
use crate::ir::{PlanKind, PlanRef, Proc};
use mpedb_types::{PlanHash, Result, Value};

const B: Budget = Budget { instrs: 100_000, db_calls: 0, rows: 0 };

/// Compile a whole `CREATE FUNCTION` and run it.
fn call(src: &str, args: &[Value]) -> Result<Value> {
    let skel = super::compile(src)?;
    assert_eq!(
        skel.calls.len(),
        0,
        "a stored SQL function must carry no db calls; every SQL-bearing form is refused"
    );
    let plans: Vec<PlanRef> = skel
        .calls
        .iter()
        .map(|c| PlanRef { hash: PlanHash([0u8; 32]), kind: PlanKind::Query, argc: c.argc })
        .collect();
    let proc = Proc::new(skel.name, skel.argc, skel.nlocals, plans, skel.consts, skel.instrs)?;
    let mut bridge = MockBridge::new();
    match interp::run(&proc, args, &mut bridge, B)? {
        ProcValue::Scalar(v) => Ok(v),
        other => panic!("a function must return a scalar, got {other:?}"),
    }
}

/// Wrap a body in the smallest header that carries `argc` parameters.
fn f(params: &str, body: &str) -> String {
    format!("CREATE FUNCTION f({params}) RETURNS int AS $$ {body} $$ LANGUAGE plpgsql")
}

fn err_of(src: &str) -> String {
    super::compile(src).unwrap_err().to_string()
}

// ------------------------------------------------------------------ accepted

#[test]
fn the_smallest_function_returns_its_expression() {
    assert_eq!(call(&f("", "BEGIN RETURN 41 + 1; END"), &[]).unwrap(), Value::Int(42));
}

#[test]
fn a_named_parameter_and_its_dollar_number_are_the_same_slot() {
    let src = f("amount int", "BEGIN RETURN amount + $1; END");
    assert_eq!(call(&src, &[Value::Int(5)]).unwrap(), Value::Int(10));
}

#[test]
fn an_unnamed_parameter_is_reachable_only_positionally() {
    let src = "CREATE FUNCTION f(int, int) RETURNS int AS $$ BEGIN RETURN $1 * $2; END $$ LANGUAGE plpgsql";
    assert_eq!(call(src, &[Value::Int(6), Value::Int(7)]).unwrap(), Value::Int(42));
}

#[test]
fn declare_allocates_and_initialises() {
    let src = f(
        "",
        "DECLARE a int := 3; b int; BEGIN b := a * 4; RETURN b; END",
    );
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(12));
}

/// PostgreSQL's rule: a DECLARE initialiser sees the OUTER binding, not the one
/// being declared. The reverse would read an unassigned slot and error.
#[test]
fn a_declare_initialiser_reads_the_outer_binding_not_its_own() {
    let src = f("x int", "DECLARE x int := x + 1; BEGIN RETURN x; END");
    assert_eq!(call(&src, &[Value::Int(10)]).unwrap(), Value::Int(11));
}

#[test]
fn postgresql_also_spells_assignment_with_a_bare_equals() {
    let src = f("", "DECLARE v int; BEGIN v = 9; RETURN v; END");
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(9));
}

#[test]
fn if_elsif_else_picks_exactly_one_arm() {
    let src = f(
        "n int",
        "BEGIN IF n < 0 THEN RETURN -1; ELSIF n = 0 THEN RETURN 0; ELSE RETURN 1; END IF; END",
    );
    for (arg, want) in [(-5, -1), (0, 0), (7, 1)] {
        assert_eq!(call(&src, &[Value::Int(arg)]).unwrap(), Value::Int(want), "n = {arg}");
    }
}

#[test]
fn while_loop_accumulates() {
    let src = f(
        "n int",
        "DECLARE i int := 1; s int := 0; BEGIN WHILE i <= n LOOP s := s + i; i := i + 1; END LOOP; RETURN s; END",
    );
    assert_eq!(call(&src, &[Value::Int(10)]).unwrap(), Value::Int(55));
}

#[test]
fn a_bare_loop_is_left_only_by_exit_when() {
    let src = f(
        "",
        "DECLARE i int := 0; BEGIN LOOP i := i + 1; EXIT WHEN i >= 4; END LOOP; RETURN i; END",
    );
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(4));
}

#[test]
fn for_in_range_counts_inclusively_like_postgresql() {
    let src = f(
        "",
        "DECLARE s int := 0; BEGIN FOR i IN 1..5 LOOP s := s + i; END LOOP; RETURN s; END",
    );
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(15));
}

#[test]
fn for_reverse_and_for_by_step() {
    let a = f("", "DECLARE s int := 0; BEGIN FOR i IN REVERSE 5..1 LOOP s := s * 10 + i; END LOOP; RETURN s; END");
    assert_eq!(call(&a, &[]).unwrap(), Value::Int(54321));
    let b = f("", "DECLARE s int := 0; BEGIN FOR i IN 1..10 BY 3 LOOP s := s + i; END LOOP; RETURN s; END");
    assert_eq!(call(&b, &[]).unwrap(), Value::Int(1 + 4 + 7 + 10));
}

/// The reason the FOR loop increments BEFORE it tests: with a test-first
/// lowering, `CONTINUE` jumps to the test, the variable never advances, and
/// this test hangs instead of failing.
#[test]
fn continue_inside_a_for_advances_the_variable() {
    let src = f(
        "",
        "DECLARE s int := 0; BEGIN FOR i IN 1..6 LOOP CONTINUE WHEN i % 2 = 0; s := s + i; END LOOP; RETURN s; END",
    );
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(1 + 3 + 5));
}

#[test]
fn the_for_variable_is_scoped_to_its_loop() {
    // `i` is a parameter; the loop shadows it and must restore it afterwards.
    let src = f(
        "i int",
        "DECLARE s int := 0; BEGIN FOR i IN 1..3 LOOP s := s + i; END LOOP; RETURN s * 100 + i; END",
    );
    assert_eq!(call(&src, &[Value::Int(7)]).unwrap(), Value::Int(607));
}

#[test]
fn exit_leaves_the_innermost_loop_only() {
    let src = f(
        "",
        "DECLARE s int := 0; BEGIN FOR i IN 1..3 LOOP FOR j IN 1..3 LOOP EXIT WHEN j = 2; s := s + 1; END LOOP; END LOOP; RETURN s; END",
    );
    assert_eq!(call(&src, &[]).unwrap(), Value::Int(3));
}

/// PostgreSQL's integer `/` truncates toward ZERO and `%` takes the DIVIDEND's
/// sign. Python floors and takes the divisor's sign — the two differ exactly on
/// negative operands, which is why the frontend emits `IntDiv`/`IntRem` and not
/// `TrueDiv`/`PyMod`. Values below are what PostgreSQL 16 returns.
#[test]
fn integer_division_and_modulo_follow_postgresql_not_python() {
    let d = f("a int, b int", "BEGIN RETURN a / b; END");
    let m = f("a int, b int", "BEGIN RETURN a % b; END");
    for (a, b, div, rem) in [(7, 2, 3, 1), (-7, 2, -3, -1), (7, -2, -3, 1), (-7, -2, 3, -1)] {
        assert_eq!(
            call(&d, &[Value::Int(a), Value::Int(b)]).unwrap(),
            Value::Int(div),
            "{a} / {b}"
        );
        assert_eq!(
            call(&m, &[Value::Int(a), Value::Int(b)]).unwrap(),
            Value::Int(rem),
            "{a} % {b}"
        );
    }
}

/// A NULL condition is NOT true in PostgreSQL, and the interpreter's
/// truthiness agrees. Asserted rather than assumed: it is the case one would
/// expect a language-semantics interpreter to get wrong.
#[test]
fn a_null_condition_does_not_take_the_branch() {
    let src = f("v int", "BEGIN IF v THEN RETURN 1; ELSE RETURN 0; END IF; END");
    assert_eq!(call(&src, &[Value::Null]).unwrap(), Value::Int(0));
    assert_eq!(call(&src, &[Value::Int(3)]).unwrap(), Value::Int(1));
}

#[test]
fn is_null_and_is_not_null_both_answer() {
    let src = f(
        "v int",
        "BEGIN IF v IS NULL THEN RETURN 100; END IF; IF v IS NOT NULL THEN RETURN 200; END IF; RETURN 0; END",
    );
    assert_eq!(call(&src, &[Value::Null]).unwrap(), Value::Int(100));
    assert_eq!(call(&src, &[Value::Int(1)]).unwrap(), Value::Int(200));
}

/// Value-preserving short circuit, and — more importantly — the right-hand side
/// must not run when the left decides the answer.
#[test]
fn and_or_short_circuit_without_evaluating_the_other_side() {
    // `1/0` would error if evaluated; it must not be.
    let a = f("", "BEGIN IF false AND 1 / 0 = 0 THEN RETURN 1; END IF; RETURN 2; END");
    assert_eq!(call(&a, &[]).unwrap(), Value::Int(2));
    let o = f("", "BEGIN IF true OR 1 / 0 = 0 THEN RETURN 1; END IF; RETURN 2; END");
    assert_eq!(call(&o, &[]).unwrap(), Value::Int(1));
}

#[test]
fn a_function_that_falls_off_the_end_returns_null() {
    let src = f("n int", "BEGIN IF n > 0 THEN RETURN 1; END IF; END");
    assert_eq!(call(&src, &[Value::Int(-1)]).unwrap(), Value::Null);
}

#[test]
fn comments_and_labels_and_a_trailing_semicolon_do_not_disturb_the_body() {
    let src = "CREATE FUNCTION f() RETURNS int AS $$
        -- leading comment
        DECLARE /* nested /* block */ comment */ v int := 1;
        BEGIN
            v := v + 1;  -- trailing
            RETURN v;
        END;
    $$ LANGUAGE plpgsql";
    assert_eq!(call(src, &[]).unwrap(), Value::Int(2));
}

#[test]
fn an_integer_cast_is_dropped_as_an_identity() {
    let src = f("n int", "BEGIN RETURN (n + 1)::int; END");
    assert_eq!(call(&src, &[Value::Int(1)]).unwrap(), Value::Int(2));
}

// ------------------------------------------------------------------ refusals

/// Each refusal must name the thing the user wrote — a shared "unsupported"
/// would send the reader looking in the wrong place. The needle is the word
/// they would search the message for.
#[test]
fn every_refusal_names_what_it_refused() {
    for (body, needle) in [
        ("BEGIN RAISE EXCEPTION 'boom'; END", "RAISE"),
        ("BEGIN EXECUTE 'SELECT 1'; END", "EXECUTE"),
        ("BEGIN PERFORM 1; END", "PERFORM"),
        ("DECLARE v int; BEGIN SELECT 1 INTO v; RETURN v; END", "SELECT"),
        ("BEGIN INSERT INTO t VALUES (1); RETURN 1; END", "INSERT"),
        ("BEGIN UPDATE t SET a = 1; RETURN 1; END", "INSERT/UPDATE/DELETE"),
        ("BEGIN FOR r IN SELECT 1 LOOP END LOOP; RETURN 1; END", "FOR"),
        ("BEGIN RETURN 'a' || 'b'; END", "||"),
        ("BEGIN RETURN 1::text; END", "text"),
        ("BEGIN RETURN g(1); END", "g("),
        ("BEGIN EXCEPTION WHEN others THEN RETURN 0; END", "EXCEPTION"),
        ("BEGIN GET DIAGNOSTICS x = ROW_COUNT; RETURN 1; END", "GET DIAGNOSTICS"),
        ("DECLARE v t%ROWTYPE; BEGIN RETURN 1; END", "%TYPE"),
        ("DECLARE v CONSTANT int := 1; BEGIN RETURN v; END", "CONSTANT"),
        ("DECLARE v int NOT NULL := 1; BEGIN RETURN v; END", "NOT NULL"),
        ("BEGIN RETURN NEXT 1; END", "RETURN NEXT"),
        ("BEGIN DECLARE v int; BEGIN RETURN 1; END; END", "nested BEGIN"),
        ("BEGIN CASE WHEN true THEN RETURN 1; END CASE; END", "CASE"),
    ] {
        let e = err_of(&f("", body));
        assert!(e.contains(needle), "body `{body}`\n  wanted `{needle}`, got: {e}");
    }
}

/// The one form that would COMPILE and then quietly disagree with PostgreSQL:
/// `x = NULL` is NULL there (never true) and true here when x is also null.
#[test]
fn comparing_to_null_with_equals_is_refused_and_points_at_is_null() {
    for body in ["BEGIN IF 1 = NULL THEN RETURN 1; END IF; RETURN 0; END",
                 "BEGIN IF 1 <> NULL THEN RETURN 1; END IF; RETURN 0; END"] {
        let e = err_of(&f("", body));
        assert!(e.contains("IS NULL"), "{body}\n  got: {e}");
    }
}

#[test]
fn exit_and_continue_outside_a_loop_are_refused_rather_than_ignored() {
    assert!(err_of(&f("", "BEGIN EXIT; END")).contains("EXIT outside a loop"));
    assert!(err_of(&f("", "BEGIN CONTINUE; END")).contains("CONTINUE outside a loop"));
}

#[test]
fn an_undeclared_name_is_named_in_the_error() {
    let e = err_of(&f("", "BEGIN RETURN zzz; END"));
    assert!(e.contains("`zzz` is not declared"), "{e}");
    let a = err_of(&f("", "BEGIN zzz := 1; RETURN 1; END"));
    assert!(a.contains("`zzz` is not declared"), "{a}");
}

#[test]
fn a_parameter_past_the_declared_arity_is_refused() {
    let e = err_of(&f("a int", "BEGIN RETURN $2; END"));
    assert!(e.contains("$2"), "{e}");
}

/// Errors inside the body must report a line in the FILE, not in the substring
/// between the dollar quotes.
#[test]
fn a_body_error_reports_its_line_in_the_original_source() {
    let src = "CREATE FUNCTION f() RETURNS int AS $$\nBEGIN\nRETURN zzz;\nEND\n$$ LANGUAGE plpgsql";
    let e = err_of(src);
    assert!(e.contains("line 3"), "{e}");
}

#[test]
fn trailing_input_after_end_is_refused_rather_than_silently_dropped() {
    let e = err_of(&f("", "BEGIN RETURN 1; END RETURN 2;"));
    assert!(e.contains("trailing input"), "{e}");
}

/// A runaway body must stop at the budget, deterministically, rather than hang.
#[test]
fn an_endless_loop_stops_at_the_instruction_budget() {
    let skel = super::compile(&f("", "BEGIN LOOP END LOOP; END")).unwrap();
    let proc = Proc::new(skel.name, skel.argc, skel.nlocals, Vec::new(), skel.consts, skel.instrs)
        .unwrap();
    let mut bridge = MockBridge::new();
    let e = interp::run(&proc, &[], &mut bridge, Budget { instrs: 5_000, db_calls: 0, rows: 0 })
        .unwrap_err();
    assert!(format!("{e}").contains("budget"), "{e}");
}

/// How much of PostgreSQL's OWN corpus this frontend compiles, and — more
/// useful — WHY the rest does not.
///
/// `#[ignore]`d and env-gated because it needs a PostgreSQL source tree, the
/// same convention the slow/instrumented tests elsewhere follow:
///
/// ```sh
/// MPEDB_PG_REGRESS=~/postgresql-16.14/src/test/regress \
///   cargo test -p mpedb-spell -- --ignored plpgsql_corpus --nocapture
/// ```
///
/// It asserts nothing about the RATE. A coverage number that a test enforces is
/// a number someone eventually moves by widening what is accepted, and this
/// frontend's refusals are load-bearing (see the module docs). What it does
/// assert is that no input PANICS — a frontend fed 470 hostile-by-accident
/// bodies must produce an error, never a crash.
#[test]
#[ignore]
fn plpgsql_corpus_coverage() {
    use std::collections::BTreeMap;
    let Ok(root) = std::env::var("MPEDB_PG_REGRESS") else {
        eprintln!("MPEDB_PG_REGRESS unset — skipping");
        return;
    };
    let re_start = "language";
    let mut srcs: Vec<String> = Vec::new();
    let dir = std::path::Path::new(&root).join("sql");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    for f in files {
        let txt = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).into_owned();
        let low = txt.to_ascii_lowercase();
        // Scan for `create [or replace] function` … `language plpgsql` … `;`
        let mut at = 0usize;
        while let Some(rel) = low[at..].find("create ") {
            let start = at + rel;
            let head: String = low[start..].chars().take(40).collect();
            if !(head.contains("function")) {
                at = start + 7;
                continue;
            }
            let Some(endrel) = low[start..].find(';') else { break };
            // The body's `;` do not count: extend to the last `;` after the
            // final dollar-quote of the statement.
            let mut end = start + endrel;
            if let Some(d1) = low[start..].find("$$") {
                if let Some(d2rel) = low[start + d1 + 2..].find("$$") {
                    let after = start + d1 + 2 + d2rel + 2;
                    end = low[after..].find(';').map_or(after, |r| after + r);
                }
            }
            let stmt = txt[start..=end.min(txt.len() - 1)].to_string();
            if stmt.to_ascii_lowercase().contains(re_start) {
                srcs.push(stmt);
            }
            at = end + 1;
        }
    }
    let mut ok = 0usize;
    let mut why: BTreeMap<String, usize> = BTreeMap::new();
    for s in &srcs {
        if !s.to_ascii_lowercase().contains("plpgsql") {
            continue;
        }
        match super::compile(s) {
            Ok(_) => ok += 1,
            Err(e) => {
                // Bucket by the message's SHAPE: everything after the location
                // prefix, truncated to the distinguishing clause.
                let m = e.to_string();
                let tail = m.split(": ").skip(2).collect::<Vec<_>>().join(": ");
                let key: String = tail.chars().take(60).collect();
                *why.entry(key).or_default() += 1;
            }
        }
    }
    let total = srcs.iter().filter(|s| s.to_ascii_lowercase().contains("plpgsql")).count();
    eprintln!("\nplpgsql corpus: {ok}/{total} compile ({:.1}%)", 100.0 * ok as f64 / total as f64);
    let mut ranked: Vec<_> = why.into_iter().collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
    for (k, n) in ranked.iter().take(20) {
        eprintln!("  {n:>4}  {k}");
    }
}
