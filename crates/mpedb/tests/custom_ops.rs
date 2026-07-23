//! Stage M3: `:sym:` custom operators — bind-time PySpell macros over operand
//! SOURCE TEXT. The oracle is the hand-written expansion: an operator query
//! must produce the same answers AND the same plan hash as the SQL it expands
//! to, because the plan contains only the expansion. Plus the founding
//! example — `TIME :>: now` with both identifiers undefined — the four-fixity
//! matrix, containment, chaining, and the self-expansion depth cap.

use mpedb::opdef::OpFixity;
use mpedb::spellfn::SpellLang;
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/customops-{tag}-{}.mpedb",
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
name = "orders"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "t"
  type = "text"
  nullable = false
  [[table.column]]
  name = "amount"
  type = "int64"

[[table]]
name = "edge"
primary_key = ["eid"]
  [[table.column]]
  name = "eid"
  type = "int64"
  [[table.column]]
  name = "src"
  type = "int64"
  nullable = false
  indexed = true
  [[table.column]]
  name = "dst"
  type = "int64"
  nullable = false
  indexed = true
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
fn the_founding_example_time_gt_now() {
    let (d, path) = db("time");
    // One order far in the past, one far in the future.
    d.query(
        "INSERT INTO orders (id, t, amount) VALUES \
         (1, '1990-01-01 00:00:00', 5), (2, '2990-01-01 00:00:00', 7)",
        &[],
    )
    .unwrap();

    // The macro DECIDES what the bare names mean: `TIME` is the `t` column,
    // `now` is datetime('now'); anything else passes through parenthesized.
    d.create_operator(
        ">",
        OpFixity::Infix,
        SpellLang::Python,
        "def timecmp(l, r):\n\
         \x20   lhs = \"(\" + l + \")\"\n\
         \x20   if l == \"TIME\":\n\
         \x20       lhs = \"t\"\n\
         \x20   rhs = \"(\" + r + \")\"\n\
         \x20   if r == \"now\":\n\
         \x20       rhs = \"datetime('now')\"\n\
         \x20   return lhs + \" > \" + rhs\n",
        "time comparison with TIME/now vocabulary",
    )
    .unwrap();

    // Neither TIME nor now exists anywhere — the operands never reach the
    // binder; the macro's expansion does.
    let got = rows(d.query("SELECT id FROM orders WHERE TIME :>: now", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2)]], "only the future order is > now");

    // Containment: the same undefined identifier OUTSIDE an operand is still
    // the ordinary bind error.
    let e = d.query("SELECT TIME FROM orders", &[]).unwrap_err();
    assert!(e.to_string().contains("TIME") || e.to_string().contains("unknown column"), "{e}");

    // And the operator still works on ordinary expressions.
    let got = rows(d.query("SELECT id FROM orders WHERE amount :>: 6", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2)]]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_four_fixities_parse_and_expand() {
    let (d, path) = db("fixity");
    d.query("INSERT INTO orders (id, t, amount) VALUES (1, 'x', 10)", &[]).unwrap();

    d.create_operator(
        "+%", OpFixity::Infix, SpellLang::Python,
        "def pct(l, r):\n    return \"(\" + l + \") * (100 + (\" + r + \")) / 100\"\n",
        "a +% b: a increased by b percent",
    ).unwrap();
    d.create_operator(
        "sq", OpFixity::Postfix, SpellLang::Python,
        "def sq(l):\n    return \"(\" + l + \") * (\" + l + \")\"\n",
        "x :sq: squares",
    ).unwrap();
    d.create_operator(
        "neg", OpFixity::Prefix, SpellLang::Python,
        "def neg(r):\n    return \"0 - (\" + r + \")\"\n",
        ":neg: x negates",
    ).unwrap();
    d.create_operator(
        "answer", OpFixity::Niladic, SpellLang::Python,
        "def answer():\n    return \"42\"\n",
        ":answer: — niladic still runs code",
    ).unwrap();

    let one = |sql: &str| rows(d.query(sql, &[]).unwrap())[0][0].clone();
    assert_eq!(one("SELECT 200 :+%: 10"), Value::Int(220), "infix (11)");
    assert_eq!(one("SELECT 7 :sq:"), Value::Int(49), "postfix (10)");
    assert_eq!(one("SELECT :neg: 5"), Value::Int(-5), "prefix (01)");
    assert_eq!(one("SELECT :answer:"), Value::Int(42), "niladic (00)");

    // Parameters inside operands keep their slots through the sub-parse.
    assert_eq!(
        rows(d.query("SELECT $1 :sq:", &[Value::Int(9)]).unwrap())[0][0],
        Value::Int(81)
    );

    // A fixity/arity mismatch is refused at create, naming both.
    let e = d
        .create_operator(
            "bad", OpFixity::Infix, SpellLang::Python,
            "def bad(x):\n    return x\n", "",
        )
        .unwrap_err();
    assert!(e.to_string().contains("2 operand(s)"), "{e}");

    // Chaining refuses with direction.
    let e = d.query("SELECT 1 :+%: 2 :+%: 3", &[]).unwrap_err();
    assert!(e.to_string().contains("parenthesize"), "{e}");

    // Unknown operator refusal names the doc.
    let e = d.query("SELECT 1 :nope: 2", &[]).unwrap_err();
    assert!(e.to_string().contains("SQL-EXTENSIONS.md"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn model_installed_edge_operator_matches_its_expansion_exactly() {
    let (d, path) = db("model");
    let mut s = d.begin().unwrap();
    for (i, (a, b)) in [(1i64, 2i64), (2, 3), (3, 1), (1, 3)].iter().enumerate() {
        s.query(
            "INSERT INTO edge (eid, src, dst) VALUES ($1, $2, $3)",
            &[Value::Int(i as i64), Value::Int(*a), Value::Int(*b)],
        )
        .unwrap();
    }
    s.commit().unwrap();

    // The model's ROLE is what tells `:->:` which table joins.
    d.set_model(
        r#"
[model]
archetype = "graph-traversal"

[[model.table]]
name = "edge"
role = "edge"

  [[model.table.access]]
  kind = "traverse"
  columns = ["src", "dst"]
"#,
    )
    .unwrap();
    let installed = d.install_model_operators().unwrap();
    assert_eq!(installed, ["->"]);

    // Same answers AND the same plan hash as the hand-written expansion —
    // the plan contains only the expansion, so the hashes must collide.
    let sugar = "SELECT id FROM orders WHERE id :->: 3";
    let manual = "SELECT id FROM orders WHERE EXISTS \
                  (SELECT 1 FROM edge WHERE edge.src = (id) AND edge.dst = (3))";
    d.query("INSERT INTO orders (id, t, amount) VALUES (1,'a',0),(2,'b',0),(3,'c',0)", &[])
        .unwrap();
    assert_eq!(
        rows(d.query(sugar, &[]).unwrap()),
        rows(d.query(manual, &[]).unwrap()),
        "sugar and expansion must agree"
    );
    assert_eq!(
        d.prepare(sugar).unwrap(),
        d.prepare(manual).unwrap(),
        "the operator leaves NO trace in the plan: identical hashes"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn self_expansion_trips_the_depth_cap_deterministically() {
    let (d, path) = db("depth");
    d.create_operator(
        "loop", OpFixity::Niladic, SpellLang::Python,
        "def looper():\n    return \":loop:\"\n",
        "expands to itself",
    )
    .unwrap();
    let e1 = d.query("SELECT :loop:", &[]).unwrap_err().to_string();
    let e2 = d.query("SELECT :loop:", &[]).unwrap_err().to_string();
    assert_eq!(e1, e2, "the depth refusal is deterministic");
    assert!(e1.contains("8 levels"), "{e1}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_statement_operator_fronts_its_own_language() {
    let (d, path) = db("lang");
    let mut s = d.begin().unwrap();
    for (i, (a, b)) in [(1i64, 2i64), (2, 3)].iter().enumerate() {
        s.query(
            "INSERT INTO edge (eid, src, dst) VALUES ($1, $2, $3)",
            &[Value::Int(i as i64), Value::Int(*a), Value::Int(*b)],
        )
        .unwrap();
    }
    s.query("INSERT INTO orders (id, t, amount) VALUES (1,'a',0),(2,'b',0),(3,'c',0)", &[])
        .unwrap();
    s.commit().unwrap();

    // The inner expression operator the language's output uses — the
    // "`::` inside it, further" part of the ask.
    d.create_operator(
        "->", OpFixity::Infix, SpellLang::Python,
        "def edge_step(l, r):\n    return \"EXISTS (SELECT 1 FROM edge WHERE edge.src = (\" + l + \") AND edge.dst = (\" + r + \"))\"\n",
        "edge exists",
    ).unwrap();

    // The STATEMENT operator (fixity bit 4 = 100): swallows the whole rest of
    // the source and returns a complete statement. One :graph: token, and a
    // graph language behind it.
    d.create_operator(
        "graph", OpFixity::Statement, SpellLang::Python,
        "def graphlang(rest):\n\
         \x20   if rest == \"count\":\n\
         \x20       return \"SELECT count(*) FROM edge\"\n\
         \x20   return \"SELECT id FROM orders WHERE id :->: (\" + rest + \") ORDER BY id\"\n",
        "a tiny graph language: `count`, or `reachable-to <expr>`",
    ).unwrap();

    // The language's own vocabulary…
    let got = rows(d.query(":graph: count", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2)]]);
    // …and an expansion that itself uses the inner `:->:` operator.
    let got = rows(d.query(":graph: 3", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(2)]], "order 2 has an edge to 3");

    // A statement operator in expression position refuses by name…
    let e = d.query("SELECT :graph:", &[]).unwrap_err();
    assert!(e.to_string().contains("STATEMENT operator"), "{e}");
    // …and an expression operator cannot begin a statement.
    let e = d.query(":->: 3", &[]).unwrap_err();
    assert!(e.to_string().contains("expression operator"), "{e}");

    let _ = std::fs::remove_file(&path);
}
