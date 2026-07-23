//! Frontend/interpreter semantics tests without a database: compile with a
//! real frontend, execute against a mock bridge, and spot-check results
//! against what CPython / rustc actually produce.

use crate::emit::CallKind;
use crate::interp::testutil::MockBridge;
use crate::interp::{self, Budget, ProcValue};
use crate::ir::{PlanKind, PlanRef, Proc};
use crate::{py, rs};
use mpedb_types::{Error, PlanHash, Result, Value};

/// Compile with the given frontend and run against a mock bridge (plan
/// hashes are dummies — the bridge never dereferences them).
fn run_lang(
    compile: fn(&str) -> Result<crate::emit::Skeleton>,
    src: &str,
    args: &[Value],
    bridge: &mut MockBridge,
    budget: Budget,
) -> Result<ProcValue> {
    let skel = compile(src)?;
    let plans = skel
        .calls
        .iter()
        .map(|c| PlanRef {
            hash: PlanHash([0u8; 32]),
            kind: match c.kind {
                CallKind::Query | CallKind::Rows => PlanKind::Query,
                CallKind::Exec => PlanKind::Exec,
            },
            argc: c.argc,
        })
        .collect();
    let proc = Proc::new(
        skel.name,
        skel.argc,
        skel.nlocals,
        plans,
        skel.consts,
        skel.instrs,
    )?;
    // Every compiled proc must also survive an encode/decode roundtrip.
    assert_eq!(Proc::decode(&proc.encode()).unwrap(), proc);
    interp::run(&proc, args, bridge, budget)
}

fn run_py(src: &str, args: &[Value]) -> Result<ProcValue> {
    run_lang(py::compile, src, args, &mut MockBridge::new(), Budget::default())
}

fn run_rs(src: &str, args: &[Value]) -> Result<ProcValue> {
    run_lang(rs::compile, src, args, &mut MockBridge::new(), Budget::default())
}

fn scalar(v: Value) -> ProcValue {
    ProcValue::Scalar(v)
}

#[track_caller]
fn expect_py(src: &str, args: &[Value], want: Value) {
    assert_eq!(run_py(src, args).unwrap(), scalar(want));
}

#[track_caller]
fn expect_rs(src: &str, args: &[Value], want: Value) {
    assert_eq!(run_rs(src, args).unwrap(), scalar(want));
}

// ------------------------------------------------- Python arithmetic parity

#[test]
fn python_division_and_modulo_match_cpython() {
    // CPython:  7 // -2 == -4;  -7 // 2 == -4;  7 % -2 == -1;  -7 % 2 == 1
    expect_py("def f(a, b): return a // b", &[Value::Int(7), Value::Int(-2)], Value::Int(-4));
    expect_py("def f(a, b): return a // b", &[Value::Int(-7), Value::Int(2)], Value::Int(-4));
    expect_py("def f(a, b): return a % b", &[Value::Int(7), Value::Int(-2)], Value::Int(-1));
    expect_py("def f(a, b): return a % b", &[Value::Int(-7), Value::Int(2)], Value::Int(1));
    // True division on ints yields float: 7 / 2 == 3.5
    expect_py("def f(a, b): return a / b", &[Value::Int(7), Value::Int(2)], Value::Float(3.5));
    // Float floor division: 7.5 // 2 == 3.0 ; -7.5 // 2 == -4.0
    expect_py("def f(): return 7.5 // 2", &[], Value::Float(3.0));
    expect_py("def f(): return -7.5 // 2", &[], Value::Float(-4.0));
    // Float modulo takes the divisor's sign: -7.5 % 2 == 0.5
    expect_py("def f(): return -7.5 % 2", &[], Value::Float(0.5));
}

#[test]
fn python_overflow_and_zero_division_are_errors() {
    let add = "def f(a, b): return a + b";
    assert!(matches!(
        run_py(add, &[Value::Int(i64::MAX), Value::Int(1)]),
        Err(Error::ArithmeticOverflow)
    ));
    assert!(matches!(
        run_py("def f(a): return -a", &[Value::Int(i64::MIN)]),
        Err(Error::ArithmeticOverflow)
    ));
    for src in [
        "def f(a): return a / 0",
        "def f(a): return a // 0",
        "def f(a): return a % 0",
        "def f(a): return a / 0.0",
    ] {
        assert!(
            matches!(run_py(src, &[Value::Int(1)]), Err(Error::DivisionByZero)),
            "{src}"
        );
    }
    // i64 extremes are expressible.
    expect_py(
        "def f(): return -9223372036854775808 + 9223372036854775807",
        &[],
        Value::Int(-1),
    );
}

#[test]
fn python_truthiness_and_boolops_are_value_preserving() {
    // `and`/`or` return an operand, not a bool (CPython semantics).
    expect_py("def f(): return 0 or 5", &[], Value::Int(5));
    expect_py("def f(): return 1 and 2", &[], Value::Int(2));
    expect_py("def f(): return '' and 1", &[], Value::Text("".into()));
    expect_py("def f(): return '' or 'x'", &[], Value::Text("x".into()));
    expect_py("def f(): return not 0", &[], Value::Bool(true));
    expect_py("def f(): return not ''", &[], Value::Bool(true));
    expect_py("def f(): return not None", &[], Value::Bool(true));
    expect_py("def f(a): return a or 'fallback'", &[Value::Null], Value::Text("fallback".into()));
    // Short circuit: the second operand must not evaluate.
    expect_py("def f(a): return a == 0 or 10 / a > 1", &[Value::Int(0)], Value::Bool(true));
}

#[test]
fn python_none_and_comparisons() {
    // Ordinary Python equality, NOT SQL 3VL.
    expect_py("def f(a): return a == None", &[Value::Null], Value::Bool(true));
    expect_py("def f(a): return a is None", &[Value::Null], Value::Bool(true));
    expect_py("def f(a): return a is not None", &[Value::Null], Value::Bool(false));
    expect_py("def f(a): return a == None", &[Value::Int(0)], Value::Bool(false));
    // Numeric cross-type comparison.
    expect_py("def f(): return 1 == 1.0", &[], Value::Bool(true));
    expect_py("def f(): return 1 < 2.5", &[], Value::Bool(true));
    // Mismatched equality is False, not an error…
    expect_py("def f(a): return a == 'x'", &[Value::Int(1)], Value::Bool(false));
    // …but mismatched *ordering* is an error (like CPython's TypeError).
    assert!(matches!(
        run_py("def f(a): return a < 'x'", &[Value::Int(1)]),
        Err(Error::TypeMismatch(_))
    ));
    // Text ops.
    expect_py("def f(a): return a + '!'", &[Value::Text("hi".into())], Value::Text("hi!".into()));
    expect_py("def f(): return 'abc' < 'abd'", &[], Value::Bool(true));
    expect_py("def f(): return len('hløl')", &[], Value::Int(4)); // code points
}

#[test]
fn python_control_flow() {
    let gauss = "
def f(n):
    s = 0
    i = 1
    while i <= n:
        s += i
        i += 1
    return s
";
    expect_py(gauss, &[Value::Int(100)], Value::Int(5050));

    let elif = "
def f(x):
    if x < 0:
        return 'neg'
    elif x == 0:
        return 'zero'
    else:
        return 'pos'
";
    expect_py(elif, &[Value::Int(-5)], Value::Text("neg".into()));
    expect_py(elif, &[Value::Int(0)], Value::Text("zero".into()));
    expect_py(elif, &[Value::Int(3)], Value::Text("pos".into()));

    let brk = "
def f(n):
    i = 0
    total = 0
    while True:
        i += 1
        if i > n:
            break
        if i % 2 == 0:
            continue
        total += i
    return total
";
    expect_py(brk, &[Value::Int(10)], Value::Int(25)); // 1+3+5+7+9

    // Falling off the end returns None.
    expect_py("def f(a):\n    a += 1", &[Value::Int(1)], Value::Null);
}

#[test]
fn python_unbound_local_is_a_runtime_error() {
    let src = "
def f(a):
    if a:
        x = 1
    return x
";
    expect_py(src, &[Value::Bool(true)], Value::Int(1));
    let e = run_py(src, &[Value::Bool(false)]).unwrap_err();
    assert!(
        e.to_string().contains("before assignment"),
        "unexpected error: {e}"
    );
}

// ------------------------------------------------------ Rust semantics side

#[test]
fn rust_division_and_modulo_match_rustc() {
    // rustc:  7 / -2 == -3;  -7 / 2 == -3;  7 % -2 == 1;  -7 % 2 == -1
    expect_rs("fn f(a: i64, b: i64) -> i64 { a / b }", &[Value::Int(7), Value::Int(-2)], Value::Int(-3));
    expect_rs("fn f(a: i64, b: i64) -> i64 { a / b }", &[Value::Int(-7), Value::Int(2)], Value::Int(-3));
    expect_rs("fn f(a: i64, b: i64) -> i64 { a % b }", &[Value::Int(7), Value::Int(-2)], Value::Int(1));
    expect_rs("fn f(a: i64, b: i64) -> i64 { a % b }", &[Value::Int(-7), Value::Int(2)], Value::Int(-1));
    // Integer / stays integer in the Rust frontend.
    expect_rs("fn f() -> i64 { 7 / 2 }", &[], Value::Int(3));
    // Rust float division by zero is IEEE, not an error.
    expect_rs(
        "fn f(a: f64) -> f64 { a / 0.0 }",
        &[Value::Float(1.0)],
        Value::Float(f64::INFINITY),
    );
    // …but integer division by zero errors.
    assert!(matches!(
        run_rs("fn f(a: i64) -> i64 { a / 0 }", &[Value::Int(1)]),
        Err(Error::DivisionByZero)
    ));
    assert!(matches!(
        run_rs("fn f(a: i64, b: i64) -> i64 { a * b }", &[Value::Int(i64::MAX), Value::Int(2)]),
        Err(Error::ArithmeticOverflow)
    ));
    expect_rs("fn f() -> i64 { -9223372036854775808 }", &[], Value::Int(i64::MIN));
}

#[test]
fn rust_control_flow_scoping_and_tail_expr() {
    let gauss = "
fn f(n: i64) -> i64 {
    let mut s = 0;
    let mut i = 1;
    while i <= n {
        s += i;
        i += 1;
    }
    s
}
";
    expect_rs(gauss, &[Value::Int(100)], Value::Int(5050));

    // Shadowing + block scoping: the inner x disappears with its block.
    let shadow = "
fn f() -> i64 {
    let x = 1;
    let x = x + 1;
    {
        let x = 100;
        x + 0;
    }
    x
}
";
    expect_rs(shadow, &[], Value::Int(2));

    let logic = "
fn f(a: bool, b: bool) -> bool {
    a && !b || a == b
}
";
    expect_rs(logic, &[Value::Bool(true), Value::Bool(false)], Value::Bool(true));
    expect_rs(logic, &[Value::Bool(false), Value::Bool(true)], Value::Bool(false));

    let brk = "
fn f(n: i64) -> i64 {
    let mut i = 0;
    let mut total = 0;
    while true {
        i += 1;
        if i > n { break; }
        if i % 2 == 0 { continue; }
        total += i;
    }
    total
}
";
    expect_rs(brk, &[Value::Int(10)], Value::Int(25));

    // No trailing expression: implicit unit (Null).
    expect_rs("fn f() { let _x = 1; }", &[], Value::Null);
}

#[test]
fn same_algorithm_both_languages_same_behavior() {
    // gcd via Euclid: % agrees across frontends for non-negative inputs.
    let py_src = "
def gcd(a, b):
    while b != 0:
        t = a % b
        a = b
        b = t
    return a
";
    let rs_src = "
fn gcd(a: i64, b: i64) -> i64 {
    let mut a = a;
    let mut b = b;
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}
";
    for (x, y) in [(12, 18), (270, 192), (17, 5), (0, 7), (42, 42)] {
        let args = [Value::Int(x), Value::Int(y)];
        assert_eq!(
            run_py(py_src, &args).unwrap(),
            run_rs(rs_src, &args).unwrap(),
            "gcd({x}, {y})"
        );
    }
}

// -------------------------------------------------------------------- budget

#[test]
fn instruction_budget_stops_infinite_loops_both_frontends() {
    let mut bridge = MockBridge::new();
    let tight = Budget {
        instrs: 10_000,
        db_calls: 10,
        rows: 100,
    };
    let e = run_lang(
        py::compile,
        "def f():\n    while True:\n        pass",
        &[],
        &mut bridge,
        tight,
    )
    .unwrap_err();
    assert!(e.to_string().contains("instruction budget"), "{e}");
    let e = run_lang(rs::compile, "fn f() { while true { } }", &[], &mut bridge, tight)
        .unwrap_err();
    assert!(e.to_string().contains("instruction budget"), "{e}");
}

#[test]
fn db_call_budget_is_enforced() {
    let src = "
def f():
    i = 0
    while i < 100:
        db.execute(\"DELETE FROM t WHERE id = $1\", [i])
        i += 1
    return i
";
    let mut bridge = MockBridge::new();
    let e = run_lang(
        py::compile,
        src,
        &[],
        &mut bridge,
        Budget {
            instrs: 1_000_000,
            db_calls: 5,
            rows: 100,
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("db-call budget"), "{e}");
    assert_eq!(bridge.execs, 5, "budget must stop the sixth call");
}

// ------------------------------------------------------------- db ops (mock)

#[test]
fn query_results_are_lists_of_tuples() {
    let src = "
def f():
    rows = db.query(\"SELECT a, b FROM t\")
    if len(rows) == 0:
        return None
    return rows[0][1] + rows[-1][0]
";
    let mut bridge = MockBridge::new();
    bridge.rows = vec![
        vec![Value::Int(10), Value::Int(1)],
        vec![Value::Int(20), Value::Int(2)],
    ];
    let got = run_lang(py::compile, src, &[], &mut bridge, Budget::default()).unwrap();
    assert_eq!(got, ProcValue::Scalar(Value::Int(21))); // rows[0][1]=1 + rows[-1][0]=20
    assert_eq!(bridge.queries, 1);

    // Returning the whole result set yields a List of Tuples.
    let all = "def f(): return db.query(\"SELECT a, b FROM t\")";
    let mut bridge2 = MockBridge::new();
    bridge2.rows = vec![vec![Value::Int(1), Value::Null]];
    let got = run_lang(py::compile, all, &[], &mut bridge2, Budget::default()).unwrap();
    assert_eq!(
        got,
        ProcValue::List(vec![ProcValue::Tuple(vec![
            ProcValue::Scalar(Value::Int(1)),
            ProcValue::Scalar(Value::Null),
        ])])
    );
}

#[test]
fn containers_cannot_cross_the_db_boundary() {
    let src = "
def f():
    rows = db.query(\"SELECT a FROM t\")
    return db.execute(\"DELETE FROM t WHERE id = $1\", [rows])
";
    let mut bridge = MockBridge::new();
    bridge.rows = vec![vec![Value::Int(1)]];
    let e = run_lang(py::compile, src, &[], &mut bridge, Budget::default()).unwrap_err();
    assert!(
        e.to_string().contains("scalar values can cross"),
        "unexpected error: {e}"
    );
    assert_eq!(bridge.execs, 0, "the exec must be rejected before the bridge");
}

// ------------------------------------------------------------ cursors (mock)

/// The two frontend cursor surfaces compute the same fold: Python
/// `for row in db.rows(...)`, Rust `while db.cursor_next(c)`.
#[test]
fn cursor_loops_stream_and_agree_across_frontends() {
    let rows = vec![
        vec![Value::Int(1), Value::Int(10)],
        vec![Value::Int(2), Value::Int(20)],
        vec![Value::Int(3), Value::Int(12)],
    ];
    let py_src = "
def total(lo):
    s = 0
    for row in db.rows(\"SELECT id, v FROM t WHERE id >= $1\", [lo]):
        s = s + row[1]
    return s
";
    let rs_src = "
fn total(lo: i64) -> i64 {
    let mut s = 0;
    let c = db.rows(\"SELECT id, v FROM t WHERE id >= $1\", &[lo]);
    while db.cursor_next(c) {
        s += db.cursor_col(c, 1);
    }
    s
}
";
    for (compile, src) in [
        (py::compile as fn(&str) -> Result<crate::emit::Skeleton>, py_src),
        (rs::compile, rs_src),
    ] {
        let mut bridge = MockBridge::new();
        bridge.rows = rows.clone();
        let got = run_lang(compile, src, &[Value::Int(1)], &mut bridge, Budget::default())
            .unwrap();
        assert_eq!(got, scalar(Value::Int(42)));
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.queries, 0, "db.rows must not materialize");
        assert_eq!(
            bridge.live_streams(),
            0,
            "exhausted cursor must free its stream"
        );
    }
}

#[test]
fn cursor_break_continue_and_row_binding() {
    let src = "
def f():
    s = 0
    for row in db.rows(\"SELECT id, v FROM t\"):
        if row[0] == 2:
            continue
        if row[0] == 3:
            break
        s = s + row[1]
    return s
";
    let mut bridge = MockBridge::new();
    bridge.rows = vec![
        vec![Value::Int(1), Value::Int(5)],
        vec![Value::Int(2), Value::Int(500)], // skipped by continue
        vec![Value::Int(3), Value::Int(900)], // break before adding
        vec![Value::Int(4), Value::Int(900)], // never reached
    ];
    let got = run_lang(py::compile, src, &[], &mut bridge, Budget::default()).unwrap();
    assert_eq!(got, scalar(Value::Int(5)));
    // break abandoned the cursor mid-scan: its stream stays open (the call
    // boundary cleans it up; documented v1 behavior).
    assert_eq!(bridge.live_streams(), 1);
}

#[test]
fn cursor_row_budget_is_enforced() {
    let src = "
def f():
    s = 0
    for row in db.rows(\"SELECT id FROM t\"):
        s = s + 1
    return s
";
    let mut bridge = MockBridge::new();
    bridge.rows = (0..10).map(|i| vec![Value::Int(i)]).collect();
    // 10 rows need 11 advances (the last one reports exhaustion); a budget
    // of 5 dies mid-scan…
    let e = run_lang(
        py::compile,
        src,
        &[],
        &mut bridge,
        Budget {
            instrs: 1_000_000,
            db_calls: 10,
            rows: 5,
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("row budget"), "{e}");
    // …and 11 exactly suffices.
    let mut bridge = MockBridge::new();
    bridge.rows = (0..10).map(|i| vec![Value::Int(i)]).collect();
    let got = run_lang(
        py::compile,
        src,
        &[],
        &mut bridge,
        Budget {
            instrs: 1_000_000,
            db_calls: 10,
            rows: 11,
        },
    )
    .unwrap();
    assert_eq!(got, scalar(Value::Int(10)));
}

#[test]
fn open_cursors_are_bounded() {
    // 17 for-loops, each breaking on its first row: 17 opens, none
    // exhausted — the 17th must hit MAX_CURSORS (16).
    let mut src = String::from("def f():\n");
    for _ in 0..17 {
        src.push_str("    for row in db.rows(\"SELECT id FROM t\"):\n        break\n");
    }
    src.push_str("    return 0\n");
    let mut bridge = MockBridge::new();
    bridge.rows = vec![vec![Value::Int(1)]];
    let e = run_lang(py::compile, &src, &[], &mut bridge, Budget::default()).unwrap_err();
    assert!(e.to_string().contains("too many open cursors"), "{e}");
    assert_eq!(bridge.cursor_opens, 16, "the 17th open must be rejected");
}

#[test]
fn exhausted_cursor_handle_is_stale() {
    // Advancing a cursor after it reported exhaustion errors (the handle's
    // generation was bumped; slots may be recycled).
    let src = "
fn f() -> i64 {
    let c = db.rows(\"SELECT id FROM t\");
    while db.cursor_next(c) { }
    if db.cursor_next(c) {
        return 1;
    }
    0
}
";
    let mut bridge = MockBridge::new();
    bridge.rows = vec![vec![Value::Int(1)]];
    let e = run_lang(rs::compile, src, &[], &mut bridge, Budget::default()).unwrap_err();
    assert!(e.to_string().contains("cursor is closed"), "{e}");
}

#[test]
fn cursors_cannot_escape_or_cross_the_db_boundary() {
    // Returned: rejected at the Return.
    let src = "fn f() -> i64 { let c = db.rows(\"SELECT id FROM t\"); c }";
    let e = run_lang(rs::compile, src, &[], &mut MockBridge::new(), Budget::default())
        .unwrap_err();
    assert!(e.to_string().contains("cannot be returned"), "{e}");
    // Passed as a db argument: the scalar boundary rejects it.
    let src = "
fn f() -> i64 {
    let c = db.rows(\"SELECT id FROM t\");
    let rows = db.query(\"SELECT id FROM t WHERE id = $1\", &[c]);
    rows.len()
}
";
    let e = run_lang(rs::compile, src, &[], &mut MockBridge::new(), Budget::default())
        .unwrap_err();
    assert!(e.to_string().contains("scalar values can cross"), "{e}");
    // Arithmetic on a handle is a type error mentioning "cursor".
    let src = "fn f() -> i64 { let c = db.rows(\"SELECT id FROM t\"); c + 1 }";
    let e = run_lang(rs::compile, src, &[], &mut MockBridge::new(), Budget::default())
        .unwrap_err();
    assert!(e.to_string().contains("cursor"), "{e}");
}

#[test]
fn index_errors_are_clean() {
    let src = "def f():\n    rows = db.query(\"SELECT a FROM t\")\n    return rows[3]";
    let mut bridge = MockBridge::new();
    bridge.rows = vec![vec![Value::Int(1)]];
    let e = run_lang(py::compile, src, &[], &mut bridge, Budget::default()).unwrap_err();
    assert!(e.to_string().contains("out of range"), "{e}");
    // Indexing a scalar is a type error.
    let e = run_py("def f(a): return a[0]", &[Value::Int(3)]).unwrap_err();
    assert!(matches!(e, Error::TypeMismatch(_)), "{e}");
}
