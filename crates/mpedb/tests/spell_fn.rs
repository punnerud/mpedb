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
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
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
