//! Stage M2: stored SQL functions. The oracle is SQL itself — a PySpell body
//! must be indistinguishable from the equivalent SQL expression (values AND
//! `typeof()`), plans carrying spell calls must survive the shared registry
//! and a second attached handle, redefinition must re-bind through the
//! schema-generation gate, and a runaway body must trip its budget at the
//! same deterministic count everywhere.

use mpedb::spellfn::SpellLang;
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/spellfn-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "a"
  type = "int64"
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_spell_is_indistinguishable_from_the_sql_it_mirrors() {
    let (d, path) = db("mirror");
    let mut s = d.begin().unwrap();
    for id in 0..50i64 {
        s.query(
            "INSERT INTO t (id, a) VALUES ($1, $2)",
            &[Value::Int(id), Value::Int(id * 3 - 20)],
        )
        .unwrap();
    }
    s.commit().unwrap();

    let (name, hash) = d
        .create_function(SpellLang::Python, "def affine(x):\n    return x * 2 + 1\n")
        .unwrap();
    assert_eq!(name, "affine");
    assert_eq!(hash.len(), 64);

    // Values AND types, against the SQL expression, on every row.
    let got = rows(d.query("SELECT affine(a), typeof(affine(a)) FROM t ORDER BY id", &[]).unwrap());
    let want = rows(d.query("SELECT a * 2 + 1, typeof(a * 2 + 1) FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(got, want, "a spell must be indistinguishable from its SQL mirror");

    // In a WHERE, and with the full procedure subset (a loop).
    d.create_function(
        SpellLang::Python,
        "def sumto(n):\n    total = 0\n    i = 0\n    while i < n:\n        i = i + 1\n        total = total + i\n    return total\n",
    )
    .unwrap();
    let got = rows(d.query("SELECT sumto(10)", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(55)]], "loops are the full-procedure subset");
    let got = rows(d.query("SELECT id FROM t WHERE affine(a) > 0 ORDER BY id LIMIT 3", &[]).unwrap());
    let want = rows(d.query("SELECT id FROM t WHERE a * 2 + 1 > 0 ORDER BY id LIMIT 3", &[]).unwrap());
    assert_eq!(got, want);

    // The registry round-trip: prepare here, execute on a SECOND handle that
    // never compiled it — the shareability host UDFs are deliberately denied.
    let h = d.prepare("SELECT affine(a) FROM t WHERE id = $1").unwrap();
    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    let got = rows(d2.execute(&h, &[Value::Int(7)]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)]]); // (7*3-20)*2+1

    // Wrong arity is a bind refusal naming the arity.
    let e = d.query("SELECT affine(1, 2)", &[]).unwrap_err();
    assert!(e.to_string().contains("1 argument"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn redefinition_rebinds_through_the_generation_gate() {
    let (d, path) = db("redef");
    d.query("INSERT INTO t (id, a) VALUES (1, 10)", &[]).unwrap();
    d.create_function(SpellLang::Python, "def f(x):\n    return x + 1\n").unwrap();

    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert_eq!(
        rows(d2.query("SELECT f(a) FROM t", &[]).unwrap()),
        vec![vec![Value::Int(11)]],
        "the second handle sees the stored function"
    );

    // Redefine on handle 1; handle 2's next compile must see the NEW body.
    d.create_function(SpellLang::Python, "def f(x):\n    return x * 100\n").unwrap();
    assert_eq!(
        rows(d2.query("SELECT f(a) FROM t", &[]).unwrap()),
        vec![vec![Value::Int(1000)]],
        "redefinition re-binds across handles via the schema generation"
    );

    // Drop: the name refuses, the message is the ordinary unknown-function one.
    assert!(d.drop_function("f").unwrap());
    let e = d2.query("SELECT f(a) FROM t", &[]).unwrap_err();
    assert!(e.to_string().contains("unknown function"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn refusals_and_budgets_are_deterministic() {
    let (d, path) = db("budget");

    // A body that runs SQL is refused at define time, with redirection.
    let e = d
        .create_function(
            SpellLang::Python,
            "def g():\n    return db.query(\"SELECT 1\")\n",
        )
        .unwrap_err();
    assert!(e.to_string().contains("stored procedure"), "{e}");

    // A runaway loop trips the fixed budget — same count, every process.
    d.create_function(
        SpellLang::Python,
        "def spin():\n    x = 0\n    while x >= 0:\n        x = x + 1\n    return x\n",
    )
    .unwrap();
    let e1 = d.query("SELECT spin()", &[]).unwrap_err().to_string();
    let e2 = d.query("SELECT spin()", &[]).unwrap_err().to_string();
    assert_eq!(e1, e2, "the budget trip is deterministic");
    assert!(e1.contains("budget"), "{e1}");

    // Non-scalar returns cannot even COMPILE from this frontend (list and
    // tuple literals are refused at define time), so the runtime scalar
    // check in call_spell_fn is pure defense-in-depth against forged blobs.
    // Pin the define-time refusal — it is the specification.
    let e = d
        .create_function(SpellLang::Python, "def pair():\n    return 1, 2\n")
        .unwrap_err();
    assert!(e.to_string().contains("tuple"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn plsql_scalar_functions_run_from_sql_and_shared_prepared_plans() {
    let (d, path) = db("plsql-sql");
    let source = "CREATE FUNCTION oracle_scale(x NUMBER) RETURN NUMBER IS BEGIN RETURN x * 1.25; END;";
    let (name, _) = d.create_function(SpellLang::PlSql, source).unwrap();
    assert_eq!(name, "ORACLE_SCALE");
    assert_eq!(rows(d.query("SELECT oracle_scale(4), ORACLE_SCALE(NULL)", &[]).unwrap()), vec![vec![Value::Numeric("5".into()), Value::Null]]);
    d.query("INSERT INTO t (id, a) VALUES (1, 8)", &[]).unwrap();
    let hash = d.prepare("SELECT oracle_scale(a) FROM t WHERE id = $1").unwrap();
    let attached = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert_eq!(rows(attached.execute(&hash, &[Value::Int(1)]).unwrap()), vec![vec![Value::Numeric("10".into())]]);
    d.create_function(SpellLang::PlSql, "CREATE FUNCTION oracle_scale(x NUMBER) RETURN NUMBER IS BEGIN RETURN x / 2; END;").unwrap();
    assert_eq!(rows(attached.query("SELECT oracle_scale(a) FROM t", &[]).unwrap()), vec![vec![Value::Numeric("4".into())]]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn plsql_varchar_functions_run_from_shared_sql_plans_with_oracle_null_rules() {
    let (d, path) = db("plsql-varchar");
    d.create_function(SpellLang::PlSql, "CREATE FUNCTION oracle_greet(x VARCHAR2) RETURN VARCHAR2 IS prefix CONSTANT VARCHAR2(4 CHAR) := q'[Hei ]'; BEGIN RETURN prefix || x; END;").unwrap();
    let hash = d.prepare("SELECT oracle_greet($1)").unwrap();
    let attached = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    for (input, expected) in [
        (Value::Text("ø".into()), "Hei ø"),
        (Value::Text("".into()), "Hei "),
        (Value::Null, "Hei "),
    ] {
        assert_eq!(rows(attached.execute(&hash, &[input]).unwrap()), vec![vec![Value::Text(expected.into())]]);
    }
    d.create_function(SpellLang::PlSql, "CREATE FUNCTION oracle_greet(x VARCHAR2) RETURN VARCHAR2 IS BEGIN RETURN x || '!'; END;").unwrap();
    assert_eq!(rows(attached.query("SELECT oracle_greet('ø')", &[]).unwrap()), vec![vec![Value::Text("ø!".into())]]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn plsql_integer_for_routines_survive_shared_plan_linking_and_overflow_checks() {
    let (d, path) = db("plsql-for");
    d.create_function(SpellLang::PlSql, "CREATE FUNCTION oracle_square(p PLS_INTEGER) RETURN NUMBER IS BEGIN FOR i IN p..p LOOP RETURN i * i; END LOOP; END;").unwrap();
    let hash = d.prepare("SELECT oracle_square($1)").unwrap();
    let attached = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert_eq!(rows(attached.execute(&hash, &[Value::Int(7)]).unwrap()), vec![vec![Value::Numeric("49".into())]]);
    assert_eq!(rows(attached.execute(&hash, &[Value::Numeric("3.5".into())]).unwrap()), vec![vec![Value::Numeric("16".into())]]);
    assert!(attached.execute(&hash, &[Value::Int(50000)]).is_err());
    assert!(attached.execute(&hash, &[Value::Null]).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn plsql_number_specials_preserve_identity_through_shared_sql_plans() {
    let (d, path) = db("plsql-specials");
    d.create_function(SpellLang::PlSql, "CREATE FUNCTION oracle_overflow(p NUMBER) RETURN NUMBER IS BEGIN RETURN p * 1e125; END;").unwrap();
    let hash = d.prepare("SELECT oracle_overflow($1)").unwrap();
    let attached = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    for (input, expected) in [("1e125", "Infinity"), ("-1e125", "-Infinity")] {
        assert_eq!(rows(attached.execute(&hash, &[Value::Numeric(input.into())]).unwrap()), vec![vec![Value::Numeric(expected.into())]]);
    }
    assert_eq!(rows(attached.execute(&hash, &[Value::Null]).unwrap()), vec![vec![Value::Null]]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn procedure_and_out_signatures_cannot_be_registered_as_scalar_sql_functions() {
    let (d, path) = db("plsql-signature");
    for source in [
        "CREATE PROCEDURE bad IS BEGIN NULL; END;",
        "CREATE FUNCTION bad(x OUT NUMBER) RETURN NUMBER IS BEGIN x := 1; RETURN x; END;",
        "CREATE FUNCTION bad(x IN OUT BOOLEAN) RETURN BOOLEAN IS BEGIN RETURN x; END;",
    ] {
        assert!(d.create_function(SpellLang::PlSql, source).is_err());
    }
    assert!(d.list_functions().unwrap().is_empty());
    let _ = std::fs::remove_file(path);
}
