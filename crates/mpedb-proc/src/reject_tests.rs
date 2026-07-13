//! Every construct outside the two subsets must be rejected at compile time
//! with a located, explanatory error — the sandbox is defined by what the
//! frontends refuse to compile.

use crate::{py, rs};

#[track_caller]
fn reject_py(src: &str, needle: &str) {
    let e = py::compile(src).expect_err("must be rejected");
    let msg = e.to_string();
    assert!(
        msg.contains(needle) && msg.contains("line"),
        "expected located error containing {needle:?}, got: {msg}"
    );
}

#[track_caller]
fn reject_rs(src: &str, needle: &str) {
    let e = rs::compile(src).expect_err("must be rejected");
    let msg = e.to_string();
    assert!(
        msg.contains(needle) && msg.contains("line"),
        "expected located error containing {needle:?}, got: {msg}"
    );
}

// --------------------------------------------------------------- python side

#[test]
fn python_rejects_escape_hatches() {
    // import at module level: not a single def
    let e = py::compile("import os\ndef f(): return 1").unwrap_err();
    assert!(e.to_string().contains("exactly one"), "{e}");
    // import inside the function
    reject_py("def f():\n    import os\n    return 1", "import");
    // arbitrary calls
    reject_py("def f():\n    return open('/etc/passwd')", "may be called");
    reject_py("def f():\n    return eval('1')", "may be called");
    // attribute escapes
    reject_py("def f():\n    return db.__class__", "attribute access");
    reject_py("def f():\n    return db.query.__globals__", "attribute access");
    // db itself is not a value
    reject_py("def f():\n    x = db\n    return 1", "db");
    // db.anything_else
    reject_py("def f():\n    return db.raw('DROP TABLE x')", "db.raw does not exist");
}

#[test]
fn python_rejects_dynamic_sql() {
    reject_py(
        "def f(name):\n    return db.query('SELECT * FROM ' + name)",
        "string literal",
    );
    reject_py(
        "def f(q):\n    return db.query(q, [1])",
        "string literal",
    );
    reject_py(
        "def f(a):\n    return db.query('SELECT 1', a)",
        "list literal",
    );
    reject_py(
        "def f(a):\n    return db.query('SELECT 1', x=1)",
        "keyword",
    );
}

#[test]
fn python_rejects_out_of_subset_constructs() {
    reject_py("def f():\n    return [x for x in range(3)]", "comprehension");
    reject_py("def f():\n    return f'{1}'", "f-string");
    reject_py("def f():\n    return lambda: 1", "lambda");
    reject_py("def f():\n    return {'a': 1}", "dict");
    reject_py("def f():\n    return (1, 2)", "tuple");
    reject_py("def f():\n    return [1, 2]", "list literals are only allowed");
    // `for` exists ONLY over db.rows(...); everything else stays rejected.
    reject_py("def f():\n    for i in [1]:\n        pass", "only supported over db.rows");
    reject_py("def f(n):\n    for i in range(n):\n        pass", "only supported over db.rows");
    reject_py(
        "def f():\n    for r in db.query('SELECT pk FROM t'):\n        pass",
        "only supported over db.rows",
    );
    reject_py("def f():\n    class C:\n        pass", "class");
    reject_py("def f():\n    try:\n        pass\n    except:\n        pass", "try");
    reject_py("def f():\n    with open('x') as g:\n        pass", "with");
    reject_py("def f():\n    global g\n    return 1", "global");
    reject_py("def f():\n    assert True", "assert");
    reject_py("def f():\n    raise ValueError()", "raise");
    reject_py("def f():\n    x: int = 1\n    return x", "annotated");
    reject_py("def f():\n    return 1 if True else 2", "conditional expression");
    reject_py("def f(a):\n    return 0 < a < 10", "chained comparison");
    reject_py("def f(a):\n    return a in [1]", "`in` is not supported");
    reject_py("def f(a):\n    return a is 5", "only supported against None");
    reject_py("def f(a):\n    return a ** 2", "not supported");
    reject_py("def f(a):\n    return a & 1", "not supported");
    reject_py("def f(a):\n    return ~a", "bitwise");
    reject_py("def f(rows):\n    return rows[0:2]", "slicing");
    reject_py("def f():\n    return b'bytes'", "bytes");
    reject_py("def f():\n    return 99999999999999999999999999", "out of i64 range");
    reject_py("def f():\n    return missing_var", "undefined variable");
    reject_py("def f():\n    break", "break outside");
    reject_py("def f(a, b=2):\n    return a", "defaults");
    reject_py("def f(*args):\n    return 1", "*args");
    reject_py("def f(a: int):\n    return a", "annotations");
    reject_py("def f(a):\n    a, b = 1, 2\n    return a", "plain variables");
    reject_py("def f():\n    while True:\n        break\n    else:\n        pass", "while/else");
}

/// The cursor surface has its own fences: db.rows is for-only in Python,
/// SQL stays a literal, and the for target is a plain name.
#[test]
fn python_cursor_forms_are_fenced() {
    reject_py(
        "def f():\n    c = db.rows('SELECT pk FROM t')\n    return 1",
        "iterable of a for loop",
    );
    reject_py(
        "def f():\n    return len(db.rows('SELECT pk FROM t'))",
        "iterable of a for loop",
    );
    reject_py(
        "def f(q):\n    for r in db.rows(q):\n        pass",
        "string literal",
    );
    reject_py(
        "def f():\n    for a, b in db.rows('SELECT pk, a FROM t'):\n        pass",
        "plain variable",
    );
    reject_py(
        "def f():\n    for r in db.rows('SELECT pk FROM t'):\n        pass\n    else:\n        pass",
        "for/else",
    );
    reject_py(
        "def f():\n    async for r in db.rows('SELECT pk FROM t'):\n        pass",
        "async for",
    );
}

#[test]
fn python_rejects_non_function_sources() {
    for src in ["", "x = 1", "def f(): pass\ndef g(): pass", "1 + 1"] {
        assert!(py::compile(src).is_err(), "{src:?} must be rejected");
    }
    // duplicate parameter (rustpython's parser rejects it itself)
    let e = py::compile("def f(a, a): return a").unwrap_err();
    assert!(e.to_string().contains("duplicate"), "{e}");
    // real syntax errors carry a location too
    let e = py::compile("def f(:\n    return 1").unwrap_err();
    assert!(e.to_string().contains("syntax error"), "{e}");
}

// ----------------------------------------------------------------- rust side

#[test]
fn rust_rejects_escape_hatches() {
    reject_rs(
        "fn f() -> i64 { std::process::exit(1) }",
        "free function calls",
    );
    reject_rs("fn f() -> i64 { unsafe { 1 } }", "not supported");
    reject_rs(
        "fn f() -> i64 { let x = db; 1 }",
        "db",
    );
    reject_rs("fn f() -> i64 { db.raw(\"DROP TABLE x\"); 1 }", "db.raw does not exist");
    reject_rs("fn f(s: String) -> i64 { s.field; 1 }", "field access");
    reject_rs("fn f() { println!(\"hi\"); }", "macros");
    reject_rs("fn f(s: String) -> i64 { s.push('x'); 1 }", "not supported");
}

#[test]
fn rust_rejects_dynamic_sql() {
    reject_rs(
        "fn f(q: &str) -> i64 { db.execute(q, &[1]); 1 }",
        "string literal",
    );
    reject_rs(
        "fn f(a: i64) -> i64 { db.execute(\"DELETE FROM t\", a); 1 }",
        "array literal",
    );
}

#[test]
fn rust_rejects_out_of_subset_constructs() {
    reject_rs("fn f(a: i64) -> i64 { match a { _ => 1 } }", "control flow is statement-only");
    reject_rs("fn f() -> i64 { for _i in 0..3 { } 1 }", "for/loop");
    reject_rs("fn f() -> i64 { loop { } }", "for/loop");
    reject_rs("fn f() -> i64 { let g = || 1; 1 }", "closures");
    reject_rs("fn f() -> i64 { let x = &1; 1 }", "references");
    reject_rs("fn f(a: i64) -> i64 { a & 1 }", "bitwise");
    reject_rs("fn f(a: i64) -> i64 { a << 1 }", "<<");
    reject_rs("fn f<T>(a: i64) -> i64 { a }", "generic");
    reject_rs("async fn f() -> i64 { 1 }", "async");
    reject_rs("fn f(v: Vec<i64>) -> i64 { 1 }", "unsupported parameter type");
    reject_rs("fn f() -> Vec<i64> { }", "unsupported return type");
    reject_rs("fn f() -> i64 { let (a, b) = (1, 2); a }", "plain identifiers");
    reject_rs("fn f() -> i64 { let x = 1; x = 2; x }", "not declared `mut`");
    reject_rs("fn f() -> i64 { let mut x = 1; x += missing; x }", "undefined variable");
    reject_rs("fn f() -> i64 { if true { 1 } else { 2 } }", "yield values");
    reject_rs("fn f() -> i64 { break; 1 }", "break outside");
    reject_rs("fn f() -> i64 { 'a: while true { break 'a; } 1 }", "label");
    reject_rs("fn f() -> i64 { while true { } fn g() {} 1 }", "nested items");
    reject_rs("fn f() -> i64 { if let Some(_x) = y { } 1 }", "if-let");
    reject_rs("fn f() -> i64 { return 99999999999999999999999999; }", "out of i64 range");
}

#[test]
fn rust_rejects_non_function_sources() {
    for src in ["", "struct S;", "fn f() {} fn g() {}", "const X: i64 = 1;"] {
        assert!(rs::compile(src).is_err(), "{src:?} must be rejected");
    }
    let e = rs::compile("fn f(a: i64, a: i64) -> i64 { a }").unwrap_err();
    assert!(e.to_string().contains("duplicate parameter"), "{e}");
    let e = rs::compile("fn f( -> i64 { 1 }").unwrap_err();
    assert!(e.to_string().contains("syntax error"), "{e}");
}

/// Rust cursor surface fences: arity and literalness.
#[test]
fn rust_cursor_forms_are_fenced() {
    reject_rs(
        "fn f() -> i64 { db.cursor_next(); 1 }",
        "exactly one cursor",
    );
    reject_rs(
        "fn f() -> i64 { let c = db.rows(\"SELECT pk FROM t\"); db.cursor_col(c); 1 }",
        "a cursor and a column index",
    );
    reject_rs(
        "fn f(q: &str) -> i64 { let c = db.rows(q); 1 }",
        "string literal",
    );
}

// -------------------------------------------------- located error formatting

#[test]
fn errors_carry_line_and_column() {
    let e = py::compile("def f():\n    x = 1\n    return open('x')").unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("line 3"), "wrong location: {msg}");
    let e = rs::compile("fn f() -> i64 {\n    let x = 1;\n    x & 1\n}").unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("line 3"), "wrong location: {msg}");
}
