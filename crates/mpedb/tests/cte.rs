//! Common Table Expressions (`WITH cte AS (SELECT …) SELECT …`, #CTE). A
//! non-recursive CTE is a statement-scoped named source: its body is flattened
//! onto its base table at bind time, reusing the derived-table keep-alias splice
//! (no planner/plan-bytes/executor change). Because the reference name is kept as
//! the base's alias, both unqualified refs and qualified `cte.col` / `FROM cte
//! AS x` (`x.col`) refs resolve. Only simple projection/filter bodies flatten;
//! RECURSIVE, column-lists and complex bodies are refused. Cross-checked vs
//! sqlite 3.45.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-cte-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match &rows(db.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

fn setup(db: &Database) {
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)", &[]).unwrap();
    for id in 1..=7 {
        db.query(
            &format!("INSERT INTO t (id, a, b, c) VALUES ({id}, {}, 'r{id}', {})", id, id * 10),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn basic_cte_flattens() {
    let (db, path) = open();
    setup(&db);
    // `WITH c AS (SELECT * FROM t WHERE a>4) SELECT id, a FROM c` → rows a>4.
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a > 4) SELECT id, a FROM c ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Int(5)],
        vec![Value::Int(6), Value::Int(6)],
        vec![Value::Int(7), Value::Int(7)],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn projection_body_and_outer_filter_merge() {
    let (db, path) = open();
    setup(&db);
    // Bare-column body + an unqualified outer filter that AND-merges.
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT id FROM c WHERE a < 6 ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregate_over_cte_and_multiple_ctes() {
    let (db, path) = open();
    setup(&db);
    // The outer may aggregate over the CTE (only the CTE body is constrained).
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a >= 3) SELECT count(*), sum(c) FROM c",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5), Value::Int(250)]]);

    // Multiple CTEs; only one referenced (unused CTEs are a safe leniency).
    assert_eq!(scalar_i64(
        &db,
        "WITH lo AS (SELECT * FROM t WHERE a < 3), hi AS (SELECT * FROM t WHERE a > 5) SELECT count(*) FROM hi",
    ), 2);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn qualified_refs_resolve() {
    let (db, path) = open();
    setup(&db);
    // `c.col` resolves in both the projection and the outer WHERE — the CTE name
    // is kept as the spliced base's alias. (sqlite 3.45: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT c.a FROM c WHERE c.a < 6 ORDER BY c.a",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    // A `SELECT *`-bodied CTE addressed by qualifier, projecting two base columns
    // (incl. column `c`, which the alias `c` must NOT shadow). (sqlite: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a > 2) SELECT c.id, c.c FROM c WHERE c.a < 6 ORDER BY c.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(3), Value::Int(30)],
        vec![Value::Int(4), Value::Int(40)],
        vec![Value::Int(5), Value::Int(50)],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reference_alias_resolves() {
    let (db, path) = open();
    setup(&db);
    // `FROM c AS x`: the reference alias `x` qualifies the columns. (sqlite: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT x.a FROM c AS x WHERE x.a < 6 ORDER BY x.a",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    // `SELECT *` over an aliased CTE exposes exactly the body's columns (id,a).
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 4) SELECT * FROM c AS x ORDER BY x.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Int(5)],
        vec![Value::Int(6), Value::Int(6)],
        vec![Value::Int(7), Value::Int(7)],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cte_joined_with_base_table() {
    let (db, path) = open();
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[]).unwrap();
    }
    // A CTE in the main FROM joined with a base table, addressed by qualified
    // refs on both sides. t rows a>4 = id 5,6,7; u.oid 1..6 → matches 5,6.
    // (Cross-checked vs sqlite 3.45.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 4) SELECT c.id, u.x FROM c JOIN u ON u.oid = c.id ORDER BY c.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Text("u5".into())],
        vec![Value::Int(6), Value::Text("u6".into())],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cte_in_join_operand() {
    let (db, path) = open();
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[]).unwrap();
    }
    // A CTE in JOIN-OPERAND position (`FROM base JOIN c ON …`) is spliced onto
    // its base with the CTE's WHERE folded into the ON. c body a>4 → id 5,6,7;
    // u.oid 1..6 → matches 5,6. (sqlite 3.45: u5|50, u6|60.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT u.x, c.c FROM u JOIN c ON c.id = u.oid ORDER BY u.x",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Text("u5".into()), Value::Int(50)],
        vec![Value::Text("u6".into()), Value::Int(60)],
    ]);
    // An explicit alias `AS k` on the joined CTE qualifies its columns.
    let got = rows(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT u.x, k.c FROM u JOIN c AS k ON k.id = u.oid ORDER BY u.x",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Text("u5".into()), Value::Int(50)],
        vec![Value::Text("u6".into()), Value::Int(60)],
    ]);
    // A column the CTE does NOT select must be REFUSED, not read from the base.
    //
    // Splicing the CTE onto its base under the CTE's own name is what makes
    // `c.col` resolve at all — and it also puts every column of the BASE within
    // reach, including the ones the CTE hid. Measured against 3.45.1:
    // `WITH c AS (SELECT a FROM t) SELECT u.id, c.b FROM u JOIN c ON …`
    // answered three rows of fabricated `b` where sqlite says `no such column:
    // c.b`. The `SELECT *` guard beside this one covers the case where the
    // outer names nothing; this is the case where it names too much.
    let e = db
        .query(
            "WITH c AS (SELECT id FROM t WHERE a > 4) SELECT u.x, c.c FROM u JOIN c ON c.id = u.oid",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("does not select `c`"), "{e}");

    // A CTE with a WHERE, joined with USING: refused by name. `plan_join_select`
    // rebuilds the join condition from the column list and DISCARDS `on`, so the
    // CTE's filter — which the splice folds into `on` — would be silently
    // dropped. Measured: it returned the filtered-out row. Qualifying the USING
    // columns needs the schema, so it cannot be desugared at this layer.
    let e = db
        .query(
            "WITH c AS (SELECT oid FROM t2 WHERE oid > 1) SELECT u.x FROM u JOIN c USING (oid)",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("USING/NATURAL") || e.contains("no such table"),
        "{e}"
    );

    // A no-WHERE CTE body joined: every u matches (c has all ids). (sqlite: u1..u6.)
    let got = rows(db.query(
        "WITH c AS (SELECT id FROM t) SELECT u.x FROM u JOIN c ON c.id = u.oid ORDER BY u.x",
        &[],
    ).unwrap());
    assert_eq!(got, (1..=6).map(|i| vec![Value::Text(format!("u{i}"))]).collect::<Vec<_>>());
    // LEFT JOIN with the CTE on the optional (non-preserved) side: u1..u4 have
    // no c match and NULL-extend; u5,u6 match. (sqlite: u1..u4 → NULL, u5→50, u6→60.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT u.x, c.c FROM u LEFT JOIN c ON c.id = u.oid ORDER BY u.x",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Text("u1".into()), Value::Null],
        vec![Value::Text("u2".into()), Value::Null],
        vec![Value::Text("u3".into()), Value::Null],
        vec![Value::Text("u4".into()), Value::Null],
        vec![Value::Text("u5".into()), Value::Int(50)],
        vec![Value::Text("u6".into()), Value::Int(60)],
    ]);
    // `SELECT *` over a `SELECT *`-bodied CTE joined with a base table: the star
    // correctly expands to the CTE's (== base t's) columns PLUS u's columns.
    // (sqlite 3.45: id,a,b,c,uid,oid,x → e.g. 5,5,r5,50,5,5,u5.)
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a > 4) SELECT * FROM c JOIN u ON u.oid = c.id ORDER BY c.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Int(5), Value::Text("r5".into()), Value::Int(50),
             Value::Int(5), Value::Int(5), Value::Text("u5".into())],
        vec![Value::Int(6), Value::Int(6), Value::Text("r6".into()), Value::Int(60),
             Value::Int(6), Value::Int(6), Value::Text("u6".into())],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cte_in_join_unsound_shapes_refused() {
    let (db, path) = open();
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[]).unwrap();
    }
    // A CTE on the PRESERVED side of a RIGHT JOIN cannot fold its WHERE into the
    // ON without resurrecting filtered-out rows — refused, never answered wrongly.
    assert!(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT u.x, c.c FROM u RIGHT JOIN c ON c.id = u.oid",
        &[],
    ).is_err());
    // `SELECT *` over a JOIN whose CTE body PROJECTS a subset would expose the
    // base's hidden columns — refused, whether the CTE is the JOIN operand …
    assert!(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT * FROM u JOIN c ON c.id = u.oid",
        &[],
    ).is_err());
    // … or the main FROM (its `*` expansion would silently drop the join cols).
    assert!(db.query(
        "WITH c AS (SELECT id, c FROM t WHERE a > 4) SELECT * FROM c JOIN u ON u.oid = c.id",
        &[],
    ).is_err());
    // A complex (aggregate) CTE body in JOIN position is refused at splice time.
    assert!(db.query(
        "WITH c AS (SELECT count(*) AS n FROM t) SELECT u.x FROM u JOIN c ON 1=1",
        &[],
    ).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cte_references_preceding_cte() {
    let (db, path) = open();
    setup(&db);
    // A CTE body referencing an EARLIER CTE resolves through the flat scope.
    // x = t rows a>2 (id 3..7); y = x rows id<6 (id 3,4,5). (sqlite 3.45: 3,4,5.)
    let got = rows(db.query(
        "WITH x AS (SELECT id, c FROM t WHERE a > 2), y AS (SELECT id, c FROM x WHERE id < 6) SELECT id, c FROM y ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(3), Value::Int(30)],
        vec![Value::Int(4), Value::Int(40)],
        vec![Value::Int(5), Value::Int(50)],
    ]);
    // A three-deep backward chain p → q → r, each narrowing the previous.
    // p: a>1 (id 2..7); q: p id<7 (id 2..6); r: q id<>4 (id 2,3,5,6). (sqlite: 2,3,5,6.)
    let got = rows(db.query(
        "WITH p AS (SELECT id FROM t WHERE a > 1), q AS (SELECT id FROM p WHERE id < 7), \
         r AS (SELECT id FROM q WHERE id <> 4) SELECT id FROM r ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(2)], vec![Value::Int(3)], vec![Value::Int(5)], vec![Value::Int(6)],
    ]);
    // A preceding CTE addressed by qualifier inside the later CTE's body.
    let got = rows(db.query(
        "WITH x AS (SELECT id, c FROM t WHERE a > 2), y AS (SELECT id FROM x WHERE x.id < 6) SELECT id FROM y ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn forward_and_cyclic_cte_refs_refused() {
    let (db, path) = open();
    setup(&db);
    // A forward reference (a earlier CTE naming a LATER one) is refused. sqlite
    // 3.45 accepts this; mpedb is deliberately stricter (never a wrong answer).
    assert!(db.query(
        "WITH a AS (SELECT id FROM b), b AS (SELECT id FROM t) SELECT * FROM a",
        &[],
    ).is_err());
    // A self reference is refused (sqlite: "circular reference").
    assert!(db.query(
        "WITH a AS (SELECT id FROM a) SELECT * FROM a",
        &[],
    ).is_err());
    // A two-CTE cycle is refused, bounded — never a hang.
    assert!(db.query(
        "WITH a AS (SELECT id FROM b), b AS (SELECT id FROM a) SELECT * FROM a",
        &[],
    ).is_err());
    // Duplicate CTE names are refused (sqlite: "duplicate WITH table name").
    assert!(db.query(
        "WITH a AS (SELECT id FROM t), a AS (SELECT id FROM t) SELECT * FROM a",
        &[],
    ).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn refusals() {
    let (db, path) = open();
    setup(&db);
    // RECURSIVE is refused.
    assert!(db.query("WITH RECURSIVE c AS (SELECT 1) SELECT * FROM c", &[]).is_err());
    // An explicit column list whose ARITY disagrees with the body is refused
    // (the list itself is supported — see
    // `an_explicit_column_list_renames_positionally`).
    assert!(db.query("WITH c(x, y) AS (SELECT a FROM t) SELECT x FROM c", &[]).is_err());
    // …and one over a `*` body, whose width is not known here.
    assert!(db.query("WITH c(x) AS (SELECT * FROM t) SELECT x FROM c", &[]).is_err());
    // A cardinality-CHANGING body (an aggregate with no GROUP BY) is refused
    // at reference time: it collapses to one row, so no substitution of its
    // projection into a per-base-row outer is the same query.
    assert!(db.query("WITH c AS (SELECT count(*) AS n FROM t) SELECT * FROM c", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A FROM-less CTE body is a constant row — collapse onto the dual path.
/// CPython's `test_cursor_description_cte_simple` is exactly
/// `WITH one AS (SELECT 1) SELECT * FROM one` and needs the column name `"1"`.
#[test]
fn fromless_cte_body_is_a_constant_row() {
    let (db, path) = open();
    // No user tables required.
    let res = db.query("WITH one AS (SELECT 1) SELECT * FROM one", &[]).unwrap();
    match res {
        ExecResult::Rows { rows, columns, .. } => {
            assert_eq!(columns, vec!["1".to_string()]);
            assert_eq!(rows, vec![vec![Value::Int(1)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Aliased projection: outer can name the column.
    let res = db
        .query("WITH one AS (SELECT 1 AS a, 2 AS b) SELECT a, b FROM one", &[])
        .unwrap();
    assert_eq!(
        rows(res),
        vec![vec![Value::Int(1), Value::Int(2)]]
    );
    // Outer WHERE over an aliased body column.
    let got = scalar_i64(
        &db,
        "WITH one AS (SELECT 7 AS x) SELECT x FROM one WHERE x = 7",
    );
    assert_eq!(got, 7);
    // Aggregate body still refused (cardinality change, not a constant row).
    assert!(db
        .query("WITH c AS (SELECT count(*) AS n) SELECT * FROM c", &[])
        .is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// `WITH c(p, q) AS (SELECT a, b FROM t)` IS
/// `WITH c AS (SELECT a AS p, b AS q FROM t)` — the column list is positional
/// renaming and nothing else, so it is applied to the body at parse time and
/// every stage below sees a body whose output names are already the declared
/// ones.
///
/// The renaming half is what needed the flattener changed: a body that renames
/// or COMPUTES its columns could not be spliced, because the outer names `p`
/// and the base table has no such column. Substituting each exposed name's
/// expression first leaves an outer that refers only to the base's columns.
#[test]
fn an_explicit_column_list_renames_positionally() {
    let (db, path) = open();
    setup(&db);
    let rows = |sql: &str| match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    };
    // Renaming, projection order, a computed item, and a WHERE over the
    // declared name.
    assert_eq!(
        rows("WITH c(p) AS (SELECT a FROM t WHERE a = 2) SELECT p FROM c"),
        vec![vec![Value::Int(2)]]
    );
    assert_eq!(
        rows("WITH c(p) AS (SELECT a + 10 FROM t WHERE a = 2) SELECT p FROM c"),
        vec![vec![Value::Int(12)]]
    );
    assert_eq!(
        rows("WITH c(p) AS (SELECT a FROM t) SELECT p FROM c WHERE p = 3"),
        vec![vec![Value::Int(3)]]
    );
    // An item that ALREADY carries an alias — `SELECT t.a AS a`, which is how
    // every ORM writes one — has it replaced, not appended: two aliases on one
    // item is a parse error rather than a rename. A qualified item also gets
    // its qualifier renamed to the reference name, since the splice re-aliases
    // the base.
    assert_eq!(
        rows("WITH c(p) AS (SELECT t.a AS a FROM t WHERE a = 2) SELECT p FROM c"),
        vec![vec![Value::Int(2)]]
    );
    // A VALUES body is a compound of constant rows and takes the derived-table
    // materialize path.
    assert_eq!(
        rows("WITH c(p) AS (VALUES (7), (8)) SELECT p FROM c WHERE p = 8"),
        vec![vec![Value::Int(8)]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A CTE body may carry PARAMETERS.
///
/// The body is captured as source and re-parsed where it is spliced, so its
/// parameters are numbered by that second parse. `$n` is ABSOLUTE — the body's
/// indices already are the caller's — but the outer parse never saw them, so
/// the statement's slot count has to be raised to cover the spliced AST.
/// Leaving it at the outer value indexed past the end and PANICKED, which is
/// what the blanket refusal was really holding back.
///
/// `?` is positional and the re-parse numbers the body's from zero — the same
/// slots the outer statement's own `?` take — so it is refused BY NAME.
/// Answering it would bind the wrong values rather than fail.
#[test]
fn a_cte_body_may_carry_dollar_parameters() {
    let (db, path) = open();
    setup(&db);
    let rows = |sql: &str, params: &[Value]| match db.query(sql, params).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    };
    // The body's own parameter.
    assert_eq!(
        rows("WITH c AS (SELECT a FROM t WHERE a = $1) SELECT a FROM c", &[Value::Int(3)]),
        vec![vec![Value::Int(3)]]
    );
    // Body and outer statement together, with the body holding the HIGHER
    // index — the case where the outer count alone is too small.
    assert_eq!(
        rows(
            "WITH c AS (SELECT a FROM t WHERE a > $2) SELECT a FROM c WHERE a < $1 ORDER BY a",
            &[Value::Int(4), Value::Int(2)]
        ),
        vec![vec![Value::Int(3)]]
    );
    // `?` in a body is refused, and the message says why.
    let e = db
        .query("WITH c AS (SELECT a FROM t WHERE a = ?) SELECT a FROM c", &[Value::Int(1)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("`?`"), "{e}");
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A RECURSIVE CTE body may carry `$n` parameters too.
///
/// Same rule as an ordinary CTE's, decided in the parser because that is what
/// owns the statement's slot count: `$n` is absolute and the count is raised to
/// cover the body's; `?` is positional and refused, because the re-parse would
/// number the body's from zero into the outer statement's own slots.
#[test]
fn a_recursive_cte_body_may_carry_dollar_parameters() {
    let (db, path) = open();
    setup(&db);
    let rows = |sql: &str, params: &[Value]| match db.query(sql, params).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    };
    assert_eq!(
        rows(
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < $1) \
             SELECT n FROM c ORDER BY n",
            &[Value::Int(3)]
        ),
        vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]
    );
    let e = db
        .query(
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < ?) \
             SELECT n FROM c",
            &[Value::Int(3)],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("`?`"), "{e}");
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A derived table as a JOIN OPERAND (plan §3 — the last SQLAlchemy test's
/// blocker): `FROM u JOIN (SELECT …) AS d ON …`. The parser desugars it to
/// the CTE it is — a synthetic `WITH d AS (…)` — so the Stage-B splice and
/// every S23 fence in it are INHERITED, not copied. Expected rows measured
/// on stock 3.45.1.
#[test]
fn a_derived_table_joins_like_the_cte_it_is() {
    let (db, path) = open();
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[])
            .unwrap();
    }
    // Same body and answer as `cte_in_join_operand`, spelled inline.
    let got = rows(
        db.query(
            "SELECT u.x, d.c FROM u JOIN (SELECT id, c FROM t WHERE a > 4) AS d \
             ON d.id = u.oid ORDER BY u.x",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Text("u5".into()), Value::Int(50)],
            vec![Value::Text("u6".into()), Value::Int(60)],
        ]
    );
    // LEFT JOIN with an empty-matching body NULL-extends (stock: u1..u6, NULLs
    // beyond the matches).
    let got = rows(
        db.query(
            "SELECT u.x, d.c FROM u LEFT JOIN (SELECT id, c FROM t WHERE a > 5) AS d \
             ON d.id = u.oid ORDER BY u.uid",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 6);
    assert_eq!(got[5], vec![Value::Text("u6".into()), Value::Int(60)]);
    assert_eq!(got[0], vec![Value::Text("u1".into()), Value::Null]);
    // The alias is how everything addresses it; anonymous refuses by name.
    let e = db
        .query("SELECT u.x FROM u JOIN (SELECT id FROM t) ON 1=1", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs an alias"), "{e}");
    // A non-spliceable body (here: an aggregate) in FIRST-join INNER position
    // MOVES into the leading-FROM slot and MATERIALIZES — `FROM u JOIN (X) d
    // ON c` is `FROM (X) d JOIN u ON c`, an INNER row-set identity. Stock
    // answers one derived row against every u row.
    let got = rows(
        db.query(
            "SELECT u.x, d.m FROM u JOIN (SELECT max(id) AS m FROM t) AS d ON 1=1 \
             ORDER BY u.uid",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 6);
    assert_eq!(got[0], vec![Value::Text("u1".into()), Value::Int(7)]);
    // …and the move is SINGLE-SLOT: a second non-spliceable derived operand
    // refuses by name (materialization has one leading position).
    let e = db
        .query(
            "SELECT u.x FROM u JOIN (SELECT max(id) AS m FROM t) AS d ON 1=1 \
             JOIN (SELECT min(id) AS m2 FROM t) AS e ON 1=1",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("materialization"), "{e}");
    // A non-spliceable body in a LEFT join is MATERIALIZED IN PLACE. It used
    // to refuse: the only materialization available was the INNER swap into
    // the leading FROM, and a LEFT join is not commutative so the swap was not
    // allowed. Now the working table is addressed from the join slot instead,
    // which moves nothing.
    let got = rows(
        db.query(
            "SELECT u.x FROM u LEFT JOIN (SELECT max(id) AS m FROM t) AS d ON 1=1 \
             ORDER BY u.x",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 6, "{got:?}");

    // …and it is a REAL left join, not an inner one wearing the name. The body
    // here yields one row whose `m` is NULL (an aggregate over no rows), so
    // the ON is never true: LEFT keeps all six rows with a NULL right side,
    // INNER would answer zero. This is the assertion that would catch
    // materialization quietly dropping the join kind.
    let got = rows(
        db.query(
            "SELECT u.x, d.m FROM u LEFT JOIN (SELECT max(id) AS m FROM t WHERE a > 1000) \
             AS d ON d.m = u.oid ORDER BY u.x",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 6, "left join dropped rows: {got:?}");
    assert!(got.iter().all(|r| r[1] == Value::Null), "right side not NULL-extended: {got:?}");
    drop(db);
    let _ = std::fs::remove_file(path);
}

/// The capture fence (found while building §3, fixed in the SHARED splice so
/// both the CTE and the derived-join path inherit it): a name the BODY cannot
/// resolve must not survive the move into the join's ON and quietly bind
/// against an OUTER table. Measured on 3.45.1 — both spellings refused with
/// `no such column`, while the splice answered rows: a shipped wrong answer.
#[test]
fn a_body_reference_the_body_cannot_resolve_does_not_capture_the_outer_scope() {
    let (db, path) = open();
    db.query("CREATE TABLE big (a INTEGER PRIMARY KEY)", &[]).unwrap();
    db.query("INSERT INTO big (a) VALUES (5)", &[]).unwrap();
    db.query("CREATE TABLE small (x INTEGER PRIMARY KEY)", &[]).unwrap();
    db.query("INSERT INTO small (x) VALUES (7)", &[]).unwrap();
    // Qualified foreign reference: sqlite's exact words.
    let e = db
        .query(
            "WITH c AS (SELECT x FROM small WHERE big.a = 5) \
             SELECT big.a FROM big JOIN c ON 1=1",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("no such column: big.a"), "{e}");
    // Bare reference that only an OUTER table holds: qualified with the
    // body's own FROM name by the fence, so the binder scopes it to the
    // spliced base and refuses (`unknown column c.a`) instead of answering.
    let e = db
        .query(
            "WITH c AS (SELECT x FROM small WHERE a = 5) \
             SELECT big.a FROM big JOIN c ON 1=1",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("column"), "{e}");
    // Same two fences through the derived-JOIN spelling.
    assert!(db
        .query(
            "SELECT big.a FROM big JOIN (SELECT x FROM small WHERE big.a = 5) AS d ON 1=1",
            &[],
        )
        .is_err());
    assert!(db
        .query(
            "SELECT big.a FROM big JOIN (SELECT x FROM small WHERE a = 5) AS d ON 1=1",
            &[],
        )
        .is_err());
    // And the fence must NOT break the body's OWN references, bare or
    // qualified: `WHERE x = 7` and `WHERE small.x = 7` both stay answers.
    let got = rows(
        db.query(
            "SELECT big.a, d.x FROM big JOIN (SELECT x FROM small WHERE x = 7) AS d ON 1=1",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(5), Value::Int(7)]]);
    let got = rows(
        db.query(
            "SELECT big.a, d.x FROM big JOIN (SELECT x FROM small WHERE small.x = 7) AS d ON 1=1",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(5), Value::Int(7)]]);
    drop(db);
    let _ = std::fs::remove_file(path);
}

/// PostgreSQL's AGGREGATE ORDER BY — `agg(x ORDER BY k)` — fixes the order the
/// aggregate consumes its group in, which is not the scan's and not the
/// statement's.
///
/// The pass count cannot check this: a wrong order still returns an array of
/// the right length. These assertions read the ELEMENTS.
#[test]
fn an_aggregate_orders_its_own_input() {
    let (db, path) = open();
    db.query("CREATE TABLE s (g INT, v TEXT, ord INT)", &[]).unwrap();
    // Inserted in an order that is NEITHER the ascending nor the descending
    // answer, so scan order cannot pass by accident.
    for (g, v, o) in [(1, "c", 3), (1, "a", 1), (1, "b", 2), (2, "z", 9)] {
        db.query(&format!("INSERT INTO s VALUES ({g}, '{v}', {o})"), &[]).unwrap();
    }
    let one = |sql: &str| -> String {
        match &rows(db.query(sql, &[]).unwrap())[0][0] {
            Value::List(items) => items
                .iter()
                .map(|v| match v {
                    Value::Text(s) => s.clone(),
                    Value::Null => "NULL".into(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(","),
            other => panic!("not an array: {other:?}"),
        }
    };
    assert_eq!(one("SELECT array_agg(v ORDER BY ord) FROM s WHERE g = 1"), "a,b,c");
    assert_eq!(one("SELECT array_agg(v ORDER BY ord DESC) FROM s WHERE g = 1"), "c,b,a");
    // Without one it is SCAN order, which is what PostgreSQL promises too.
    assert_eq!(one("SELECT array_agg(v) FROM s WHERE g = 1"), "c,a,b");

    // Two aggregates in ONE select may order differently — the reason this
    // cannot be folded into the statement's ORDER BY.
    let r = rows(
        db.query(
            "SELECT array_agg(v ORDER BY ord), array_agg(v ORDER BY ord DESC) FROM s WHERE g = 1",
            &[],
        )
        .unwrap(),
    );
    let txt = |v: &Value| match v {
        Value::List(i) => i
            .iter()
            .map(|x| match x {
                Value::Text(s) => s.clone(),
                Value::Null => "NULL".into(),
                o => format!("{o:?}"),
            })
            .collect::<Vec<_>>()
            .join(","),
        o => panic!("{o:?}"),
    };
    assert_eq!(txt(&r[0][0]), "a,b,c");
    assert_eq!(txt(&r[0][1]), "c,b,a");

    // Grouped, and the ORDER BY key is not in the output.
    let got = rows(
        db.query("SELECT g, array_agg(v ORDER BY ord) FROM s GROUP BY g ORDER BY g", &[])
            .unwrap(),
    );
    assert_eq!(got.len(), 2);
    assert_eq!(txt(&got[0][1]), "a,b,c");
    assert_eq!(txt(&got[1][1]), "z");

    // An order-INSENSITIVE aggregate is unaffected by one, which is what makes
    // buffering-and-replaying safe to apply uniformly.
    let got = rows(db.query("SELECT count(v ORDER BY ord) FROM s WHERE g = 1", &[]).unwrap());
    assert_eq!(got[0][0], Value::Int(3));
    drop(db);
    let _ = std::fs::remove_file(path);
}
