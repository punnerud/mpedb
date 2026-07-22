//! #55 differential: composite secondary indexes end to end — planner
//! selection (asserted on the compiled access path), execution over the
//! engine's k-column trees, ON CONFLICT composite targets, NULL membership,
//! and join pushdown. Expectations are hand-computed; every indexed answer
//! is cross-checked against the same query's full-scan truth.

use mpedb::{Database, ExecResult, Value};
use mpedb_sql::AccessPath;
use mpedb_types::Config;

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

/// `/dev/shm` when present (fast tmpfs, mpedb's habitat), else the platform
/// temp dir — keeps the scratch path portable to macOS, where `/dev/shm` does
/// not exist (#66).
fn scratch_path(name: String) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    dir.join(name).to_string_lossy().into_owned()
}

fn setup(tag: &str) -> Database {
    let path = scratch_path(format!("mpedb-composite-{tag}-{}.mpedb", std::process::id()));
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
    let path = scratch_path(format!("mpedb-composite-join-{}.mpedb", std::process::id()));
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

// ------------------------------------- prefix probes and the NULL suffix --

/// A COMPOSITE index probed by a PREFIX loses every row whose uncovered
/// index columns are NULL — those rows have no entry at all (membership is
/// "no indexed column of this row is NULL"). Measured wrong answer before the
/// fix: `INDEX (a, b)` with `WHERE a = 5` answered `{1}` where sqlite 3.45
/// answers `{1, 2}`.
///
/// The rule the planner now takes — a prefix of length `k` is probeable only
/// when `columns[k..]` are all NOT NULL — is the one
/// `plan::agg_servable_by_index` had all along; the access paths simply never
/// got it. Differentialled against the BUNDLED sqlite, which indexes NULLs and
/// therefore never had the problem.
#[test]
fn a_prefix_probe_never_hides_rows_with_a_null_in_the_uncovered_suffix() {
    let path = scratch_path(format!(
        "mpedb-composite-nullsuffix-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = Database::open_with_config(
        Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\""
        ))
        .unwrap(),
    )
    .unwrap();

    // `b` nullable, `c` NOT NULL — one index of each shape over the same data.
    const DDL: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT, c INT NOT NULL)",
        "CREATE INDEX ix_ab ON t (a, b)",
        "CREATE INDEX ix_ac ON t (a, c)",
    ];
    const DATA: &str =
        "INSERT INTO t (id, a, b, c) VALUES (1,5,1,1),(2,5,NULL,2),(3,6,2,3),(4,NULL,9,4)";
    for d in DDL {
        db.query(d, &[]).unwrap();
    }
    db.query(DATA, &[]).unwrap();

    let want = |sql: &str| -> Vec<String> {
        let mut script = String::new();
        for d in DDL {
            script.push_str(d);
            script.push(';');
        }
        script.push_str(DATA);
        script.push(';');
        script.push_str(sql);
        script.push(';');
        sqlite_oracle::script_stdout(&script, "")
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    };
    let got = |sql: &str| -> Vec<String> {
        rows(db.query(sql, &[]).unwrap())
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Int(i) => i.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    };
    let plan = |sql: &str| -> String {
        match db.query(&format!("EXPLAIN {sql}"), &[]).unwrap() {
            ExecResult::Explain(t) => t,
            other => panic!("{other:?}"),
        }
    };

    // The two shapes a prefix probe takes, and the join probe.
    for sql in [
        "SELECT id FROM t WHERE a = 5 ORDER BY id",
        "SELECT id FROM t WHERE a > 4 ORDER BY id",
        "SELECT count(*) FROM t WHERE a = 5",
    ] {
        assert_eq!(got(sql), want(sql), "answer differs from sqlite for `{sql}`");
    }

    // `ix_ac`'s suffix is NOT NULL, so the prefix probe stays available — the
    // fix narrows exactly the lossy case and nothing else. (`ix_ab` is index 1
    // and is skipped; `ix_ac` is index 2.)
    let p = plan("SELECT id FROM t WHERE a = 5 ORDER BY id");
    assert!(
        p.contains("via index 2"),
        "the NOT NULL-suffix composite must still be probeable:\n{p}"
    );

    // Full-width coverage of the NULLABLE composite is always sound: the
    // pinning equality itself proves every indexed column non-NULL.
    let sql = "SELECT id FROM t WHERE a = 5 AND b = 1";
    let p = plan(sql);
    assert!(p.contains("via index 1"), "full width must use ix_ab:\n{p}");
    assert_eq!(got(sql), want(sql));

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
