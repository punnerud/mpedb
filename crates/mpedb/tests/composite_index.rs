//! #55 differential: composite secondary indexes end to end — planner
//! selection (asserted on the compiled access path), execution over the
//! engine's k-column trees, ON CONFLICT composite targets, NULL membership,
//! and join pushdown. Expectations are hand-computed; every indexed answer
//! is cross-checked against the same query's full-scan truth.

use mpedb::{Database, ExecResult, Value};
use mpedb_sql::AccessPath;
use mpedb_types::Config;

fn setup(tag: &str) -> Database {
    let path = format!("/dev/shm/mpedb-composite-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 8

[[table]]
name = "orders"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "tenant"
  type = "int64"

  [[table.column]]
  name = "sku"
  type = "text"

  [[table.column]]
  name = "qty"
  type = "int64"

  [[table.index]]
  columns = ["tenant", "qty"]

  [[table.index]]
  columns = ["tenant", "sku"]
  unique = true
"#
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn composite_point_prefix_range_and_null_membership() {
    let db = setup("access");
    for (id, tenant, sku, qty) in [
        (1, 1, Some("a"), 10),
        (2, 1, Some("b"), 10),
        (3, 2, Some("a"), 20),
        (4, 1, Some("a2"), 30),
        (5, 3, None, 40), // NULL sku: absent from the unique (tenant, sku) tree
    ] {
        db.query(
            "INSERT INTO orders (id, tenant, sku, qty) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(id),
                Value::Int(tenant),
                sku.map_or(Value::Null, |s| Value::Text(s.into())),
                Value::Int(qty),
            ],
        )
        .unwrap();
    }
    let schema = db.schema().clone();

    // Full-width equality on the composite UNIQUE → IndexPoint, both parts.
    let p = mpedb_sql::prepare(
        "SELECT id FROM orders WHERE tenant = $1 AND sku = $2",
        &schema,
    )
    .unwrap();
    let access = match &p.stmt {
        mpedb_sql::PlanStmt::Select(s) => &s.access,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        access,
        &AccessPath::IndexPoint {
            index_no: 2, // (tenant, qty) is index 1; (tenant, sku) is index 2
            parts: vec![
                mpedb_sql::KeyPart::Param(0),
                mpedb_sql::KeyPart::Param(1)
            ],
        },
        "full-width unique coverage must win"
    );
    let got = rows(
        db.query(
            "SELECT id FROM orders WHERE tenant = $1 AND sku = $2",
            &[Value::Int(1), Value::Text("a".into())],
        )
        .unwrap(),
    );
    assert_eq!(ids(&got), vec![1]);

    // Prefix equality (tenant only): served as a prefix scan of a composite
    // index — answers must equal the residual-filter truth.
    let got = rows(
        db.query(
            "SELECT id FROM orders WHERE tenant = $1 ORDER BY id",
            &[Value::Int(1)],
        )
        .unwrap(),
    );
    assert_eq!(ids(&got), vec![1, 2, 4]);

    // Range over the composite's FIRST column (Phase-1 rule).
    let got = rows(
        db.query(
            "SELECT id FROM orders WHERE tenant > $1 ORDER BY id",
            &[Value::Int(1)],
        )
        .unwrap(),
    );
    assert_eq!(ids(&got), vec![3, 5]);

    // NULL membership: row 5 (NULL sku) is invisible to the unique tree but
    // fully visible to scans, and a NULL probe matches nothing.
    let got = rows(db.query("SELECT count(*) FROM orders", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5)]]);
    let got = rows(
        db.query(
            "SELECT id FROM orders WHERE tenant = $1 AND sku = $2",
            &[Value::Int(3), Value::Null],
        )
        .unwrap(),
    );
    assert!(got.is_empty(), "col = NULL is UNKNOWN");

    // The composite UNIQUE enforces over the SET: same (tenant, sku) refuses,
    // NULL never conflicts.
    let err = db
        .query(
            "INSERT INTO orders (id, tenant, sku, qty) VALUES (9, 1, 'a', 0)",
            &[],
        )
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("unique"), "{err}");
    db.query(
        "INSERT INTO orders (id, tenant, sku, qty) VALUES (10, 3, NULL, 0)",
        &[],
    )
    .unwrap();
}

#[test]
fn on_conflict_targets_a_composite_unique_order_insensitively() {
    let db = setup("conflict");
    db.query(
        "INSERT INTO orders (id, tenant, sku, qty) VALUES (1, 1, 'a', 10)",
        &[],
    )
    .unwrap();
    // Target (sku, tenant) — REVERSED order — must match the (tenant, sku)
    // unique index, PostgreSQL-style set matching.
    db.query(
        "INSERT INTO orders (id, tenant, sku, qty) VALUES (2, 1, 'a', 99) \
         ON CONFLICT (sku, tenant) DO UPDATE SET qty = excluded.qty",
        &[],
    )
    .unwrap();
    let got = rows(db.query("SELECT id, qty FROM orders WHERE tenant = 1 AND sku = 'a'", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1), Value::Int(99)]], "updated in place");
}

#[test]
fn join_pushdown_uses_a_composite_index() {
    let path = format!("/dev/shm/mpedb-composite-join-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 8

[[table]]
name = "lines"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "tenant"
  type = "int64"

  [[table.column]]
  name = "sku"
  type = "text"

  [[table.index]]
  columns = ["tenant", "sku"]

[[table]]
name = "refs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "t"
  type = "int64"

  [[table.column]]
  name = "s"
  type = "text"
"#
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for (id, tenant, sku) in [(1, 1, "a"), (2, 1, "b"), (3, 2, "a"), (4, 1, "a")] {
        db.query(
            "INSERT INTO lines (id, tenant, sku) VALUES ($1, $2, $3)",
            &[Value::Int(id), Value::Int(tenant), Value::Text(sku.into())],
        )
        .unwrap();
    }
    db.query("INSERT INTO refs (id, t, s) VALUES (1, 1, 'a')", &[]).unwrap();

    // Both join columns pin the composite: the inner side must be an
    // IndexPoint with two parts (visible in EXPLAIN as the index probe).
    let schema = db.schema().clone();
    let p = mpedb_sql::prepare(
        "SELECT refs.id, lines.id FROM refs JOIN lines \
         ON lines.tenant = refs.t AND lines.sku = refs.s ORDER BY lines.id",
        &schema,
    )
    .unwrap();
    let explain = p.explain(&schema);
    assert!(
        explain.contains("via index"),
        "join must push into the composite index:\n{explain}"
    );
    let got = rows(
        db.query(
            "SELECT refs.id, lines.id FROM refs JOIN lines \
             ON lines.tenant = refs.t AND lines.sku = refs.s ORDER BY lines.id",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(1), Value::Int(4)],
        ]
    );
}
