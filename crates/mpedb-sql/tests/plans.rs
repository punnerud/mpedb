//! Integration tests: end-to-end prepare/encode/decode/hash behavior,
//! hash canonicalization, decode fuzzing, and CHECK compilation.

use mpedb_sql::{
    compile_check, prepare, prepare_maybe_explain, secondary_indexes, ColumnDef, ColumnType,
    CompiledPlan, DefaultExpr, Error, Schema, TableDef, Value,
};

fn col(name: &str, ty: ColumnType) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        ty,
        nullable: true,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: mpedb_sql::Collation::Binary,
        affinity: mpedb_types::Affinity::implied_by(ty),
    }
}

/// Tables sort by name: events = 0, orders = 1, users = 2.
fn schema() -> Schema {
    let users = TableDef {
        id: 0,
        name: "users".into(),
        columns: vec![
            ColumnDef {
                nullable: false,
                ..col("id", ColumnType::Int64)
            },
            ColumnDef {
                nullable: false,
                unique: true,
                indexed: false,
                ..col("email", ColumnType::Text)
            },
            col("age", ColumnType::Int64),
            col("score", ColumnType::Float64),
            col("active", ColumnType::Bool),
            ColumnDef {
                default: Some(DefaultExpr::Now),
                ..col("created", ColumnType::Timestamp)
            },
            col("avatar", ColumnType::Blob),
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_sql::TableKind::Standard,
    };
    let orders = TableDef {
        id: 0,
        name: "orders".into(),
        columns: vec![
            ColumnDef {
                nullable: false,
                ..col("user_id", ColumnType::Int64)
            },
            ColumnDef {
                nullable: false,
                ..col("item_no", ColumnType::Int64)
            },
            ColumnDef {
                unique: true,
                indexed: false,
                ..col("sku", ColumnType::Text)
            },
            col("note", ColumnType::Text),
        ],
        primary_key: vec![0, 1],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_sql::TableKind::Standard,
    };
    let events = TableDef {
        id: 0,
        name: "events".into(),
        columns: vec![
            ColumnDef {
                nullable: false,
                default: Some(DefaultExpr::Now),
                ..col("ts", ColumnType::Timestamp)
            },
            col("msg", ColumnType::Text),
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_sql::TableKind::Standard,
    };
    Schema::new(vec![users, orders, events]).unwrap()
}

/// A second, different schema (extra column) for cross-schema hash checks.
fn other_schema() -> Schema {
    let mut s = schema();
    let mut users = s.tables.iter().find(|t| t.name == "users").unwrap().clone();
    users.columns.push(col("extra", ColumnType::Int64));
    s.tables.retain(|t| t.name != "users");
    let mut tables = s.tables;
    tables.push(users);
    Schema::new(tables).unwrap()
}

fn corpus() -> Vec<&'static str> {
    vec![
        "SELECT * FROM users",
        "SELECT * FROM users WHERE id = $1",
        "SELECT * FROM users WHERE id = 42",
        "SELECT id, email FROM users WHERE age > 18 AND active",
        "SELECT id, age + 1, -score FROM users WHERE score >= 1.5 OR age IS NULL",
        "SELECT * FROM users WHERE id > 1 AND id <= $1 ORDER BY id",
        "SELECT * FROM users WHERE email = 'a@b'",
        "SELECT * FROM users WHERE email LIKE 'a%' AND NOT active",
        "SELECT * FROM users ORDER BY email DESC, age ASC LIMIT 100 OFFSET 10",
        "SELECT * FROM users WHERE avatar IS NOT NULL",
        "SELECT * FROM orders WHERE user_id = 1 AND item_no = 2",
        "SELECT * FROM orders WHERE user_id = $1",
        "SELECT * FROM orders WHERE sku = 'S-1'",
        "SELECT * FROM orders WHERE item_no % 2 = 0",
        "INSERT INTO users (id, email) VALUES ($1, $2)",
        "INSERT INTO users (id, email, age, score) VALUES (1, 'a', NULL, 2), (2, 'b', 3, 4.5)",
        "INSERT INTO users (email, id) VALUES ('swapped', 9)",
        "INSERT INTO events (msg) VALUES ('boot')",
        "INSERT INTO orders (user_id, item_no, sku) VALUES (1, 2, $1)",
        "UPDATE users SET age = age + 1 WHERE id = $1",
        "UPDATE users SET email = $1, score = 2 WHERE email = $2",
        "UPDATE users SET active = true",
        "DELETE FROM users WHERE id = 4",
        "DELETE FROM users WHERE id > $1 AND id < $2",
        "DELETE FROM orders WHERE sku = 'S-9'",
        "DELETE FROM orders",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "EXPLAIN SELECT * FROM users WHERE id = $1",
    ]
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

// ---- roundtrip + hash stability ---------------------------------------------

#[test]
fn roundtrip_equality_and_hash_stability_for_corpus() {
    let s = schema();
    for sql in corpus() {
        let p = prepare(sql, &s).unwrap_or_else(|e| panic!("prepare({sql}): {e}"));
        let bytes = p.encode();
        let q = CompiledPlan::decode(&bytes, &s).unwrap_or_else(|e| panic!("decode({sql}): {e}"));
        assert_eq!(p, q, "roundtrip mismatch: {sql}");
        assert_eq!(p.hash(), q.hash(), "hash changed across roundtrip: {sql}");
        assert_eq!(bytes, q.encode(), "re-encode differs: {sql}");
        assert_eq!(p.schema_hash, s.hash());
        assert_eq!(p.n_params as usize, p.param_types.len());
    }
}

#[test]
fn hash_ignores_whitespace_keyword_case_and_param_spelling() {
    let s = schema();
    let base = prepare("SELECT * FROM users WHERE id = $1 AND age > $2", &s)
        .unwrap()
        .hash();
    let variants = [
        "select * from users where id = $1 and age > $2",
        "SeLeCt    *\nFROM users\n\tWHERE id=$1 AND age>$2",
        "SELECT * FROM users WHERE id = ? AND age > ?;",
    ];
    for v in variants {
        assert_eq!(prepare(v, &s).unwrap().hash(), base, "hash differs: {v}");
    }
    // EXPLAIN compiles the inner statement: same plan, same hash.
    assert_eq!(
        prepare("EXPLAIN SELECT * FROM users WHERE id = $1 AND age > $2", &s)
            .unwrap()
            .hash(),
        base
    );
}

#[test]
fn hash_distinguishes_semantic_differences() {
    let s = schema();
    let h = |sql: &str| prepare(sql, &s).unwrap().hash();
    let base = h("SELECT * FROM users WHERE id = 1 ORDER BY email ASC LIMIT 10");
    for other in [
        "SELECT * FROM users WHERE id = 2 ORDER BY email ASC LIMIT 10", // literal
        "SELECT * FROM users WHERE age = 1 ORDER BY email ASC LIMIT 10", // column
        "SELECT * FROM users WHERE id = 1 ORDER BY email DESC LIMIT 10", // direction
        "SELECT * FROM users WHERE id = 1 ORDER BY age ASC LIMIT 10",   // sort column
        "SELECT * FROM users WHERE id = 1 ORDER BY email ASC LIMIT 11", // limit
        "SELECT * FROM users WHERE id = 1 ORDER BY email ASC",          // no limit
        "SELECT id FROM users WHERE id = 1 ORDER BY email ASC LIMIT 10", // projection
        "SELECT * FROM orders WHERE user_id = 1 AND item_no = 1",       // table
    ] {
        assert_ne!(h(other), base, "hash collision with: {other}");
    }
    // Swapped $n order is a different statement.
    assert_ne!(
        h("SELECT * FROM users WHERE id = $1 AND age > $2"),
        h("SELECT * FROM users WHERE id = $2 AND age > $1")
    );
    // Text literal case matters.
    assert_ne!(
        h("SELECT * FROM users WHERE email = 'A'"),
        h("SELECT * FROM users WHERE email = 'a'")
    );
    // Int vs float literal matters even when equal in value.
    assert_ne!(
        h("SELECT * FROM users WHERE score = 1"),
        h("SELECT * FROM users WHERE score = 1.5")
    );
}

#[test]
fn hash_depends_on_schema() {
    let a = schema();
    let b = other_schema();
    let sql = "SELECT id, email FROM users WHERE id = $1";
    let pa = prepare(sql, &a).unwrap();
    let pb = prepare(sql, &b).unwrap();
    assert_ne!(pa.hash(), pb.hash());
    // And a plan encoded under one schema does not decode under the other.
    assert!(matches!(
        CompiledPlan::decode(&pa.encode(), &b),
        Err(Error::PlanInvalidated)
    ));
}

// ---- explain ------------------------------------------------------------------

#[test]
fn explain_flag_and_output() {
    let s = schema();
    let (plan, is_explain) =
        prepare_maybe_explain("EXPLAIN SELECT * FROM users WHERE id = $1", &s).unwrap();
    assert!(is_explain);
    let (inner, not_explain) =
        prepare_maybe_explain("SELECT * FROM users WHERE id = $1", &s).unwrap();
    assert!(!not_explain);
    assert_eq!(plan, inner);
    let text = plan.explain(&s);
    assert!(text.contains("Select users"), "{text}");
    assert!(text.contains("PkPoint(id = $1)"), "{text}");
    assert!(text.contains("footprint:"), "{text}");
}

// ---- footprints -----------------------------------------------------------------

#[test]
fn read_only_routing() {
    let s = schema();
    for sql in corpus() {
        let p = prepare(sql, &s).unwrap();
        let is_write = sql.contains("INSERT") || sql.contains("UPDATE") || sql.contains("DELETE");
        assert_eq!(p.footprint.read_only, !is_write, "read_only wrong: {sql}");
        if p.footprint.read_only {
            assert_eq!(p.footprint.tables_written, 0, "{sql}");
        } else {
            assert_ne!(p.footprint.tables_written, 0, "{sql}");
        }
    }
}

#[test]
fn tampered_footprint_is_rejected() {
    let s = schema();
    for sql in [
        "SELECT * FROM users WHERE id = $1",
        "INSERT INTO users (id, email) VALUES (1, 'a')",
        "DELETE FROM orders WHERE user_id = 1 AND item_no = 2",
    ] {
        let p = prepare(sql, &s).unwrap();
        let good = p.encode();
        let mut evil = p.clone();
        evil.footprint.indexes_used ^= 1 << 5; // forge an index bit
        let bytes = evil.encode();
        assert_ne!(bytes, good);
        match CompiledPlan::decode(&bytes, &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("footprint"), "{m}"),
            other => panic!("tampered footprint accepted for {sql}: {other:?}"),
        }
    }
}

// ---- decode fuzzing --------------------------------------------------------------

#[test]
fn decode_never_panics_on_truncation() {
    let s = schema();
    for sql in corpus() {
        let bytes = prepare(sql, &s).unwrap().encode();
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation accepted at {cut} for {sql}"
            );
        }
    }
}

#[test]
fn decode_never_panics_on_bit_flips() {
    let s = schema();
    let encodings: Vec<Vec<u8>> = corpus()
        .iter()
        .map(|sql| prepare(sql, &s).unwrap().encode())
        .collect();
    let mut rng = XorShift(0x9e3779b97f4a7c15);
    for _ in 0..10_000 {
        let bytes = &encodings[(rng.next() % encodings.len() as u64) as usize];
        let mut mutated = bytes.clone();
        let flips = 1 + (rng.next() % 3);
        for _ in 0..flips {
            let bit = rng.next() % (mutated.len() as u64 * 8);
            mutated[(bit / 8) as usize] ^= 1 << (bit % 8);
        }
        // Must never panic. If it decodes, it decoded to a valid plan whose
        // re-encoding is accepted again (validation is deterministic).
        if let Ok(p) = CompiledPlan::decode(&mutated, &s) {
            let again = CompiledPlan::decode(&p.encode(), &s).expect("re-decode of valid plan");
            assert_eq!(p, again);
        }
    }
}

#[test]
fn decode_never_panics_on_random_garbage() {
    let s = schema();
    let mut rng = XorShift(42);
    for _ in 0..2_000 {
        let len = (rng.next() % 200) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next() & 0xff) as u8).collect();
        let _ = CompiledPlan::decode(&bytes, &s); // Err is fine, panic is not
    }
}

// ---- CHECK compilation -------------------------------------------------------------

#[test]
fn compile_check_binds_and_evaluates() {
    let s = schema();
    let users = s.table(s.table_id("users").unwrap()).unwrap();
    let p = compile_check("age >= 0 AND age < 200", users).unwrap();
    let row = |age: Value| {
        vec![
            Value::Int(1),
            Value::Text("a".into()),
            age,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    };
    let mut stack = Vec::new();
    assert!(p.eval_filter(&mut stack, &row(Value::Int(42)), &[]).unwrap());
    assert!(!p.eval_filter(&mut stack, &row(Value::Int(-1)), &[]).unwrap());
    // NULL age: CHECK yields NULL, which does not pass eval_filter (the
    // engine treats NULL checks as passing per SQL; that policy lives there).
    assert!(!p.eval_filter(&mut stack, &row(Value::Null), &[]).unwrap());
}

#[test]
fn compile_check_rejections() {
    let s = schema();
    let users = s.table(s.table_id("users").unwrap()).unwrap();
    // Parameters are not allowed.
    match compile_check("age > $1", users) {
        Err(Error::Bind(m)) => assert!(m.contains("parameter"), "{m}"),
        other => panic!("expected bind error, got {other:?}"),
    }
    assert!(compile_check("age > ?", users).is_err());
    // Must type to bool.
    assert!(matches!(compile_check("age + 1", users), Err(Error::Bind(_))));
    assert!(matches!(compile_check("'x'", users), Err(Error::Bind(_))));
    // Unknown column.
    assert!(matches!(compile_check("nope > 0", users), Err(Error::Bind(_))));
    // Statements are not expressions.
    assert!(matches!(
        compile_check("SELECT 1", users),
        Err(Error::Parse { .. })
    ));
    // Trailing garbage.
    assert!(matches!(
        compile_check("age > 0 age", users),
        Err(Error::Parse { .. })
    ));
}

// ---- misc public API ----------------------------------------------------------------

#[test]
fn secondary_index_helper_is_canonical() {
    let s = schema();
    let users = s.table(s.table_id("users").unwrap()).unwrap();
    let orders = s.table(s.table_id("orders").unwrap()).unwrap();
    assert_eq!(secondary_indexes(users), vec![Some(1)]); // email
    assert_eq!(secondary_indexes(orders), vec![Some(2)]); // sku
}

#[test]
fn unconstrained_param_stays_none() {
    let s = schema();
    let p = prepare("SELECT * FROM users WHERE $1 = $2", &s).unwrap();
    assert_eq!(p.n_params, 2);
    assert_eq!(p.param_types, vec![None, None]);
    // Roundtrips with the None types intact.
    let q = CompiledPlan::decode(&p.encode(), &s).unwrap();
    assert_eq!(q.param_types, vec![None, None]);
}

#[test]
fn parse_and_bind_error_taxonomy() {
    let s = schema();
    assert!(matches!(
        prepare("SELEC * FROM users", &s),
        Err(Error::Parse { .. })
    ));
    assert!(matches!(
        prepare("SELECT * FROM users WHERE", &s),
        Err(Error::Parse { .. })
    ));
    assert!(matches!(
        prepare("SELECT * FROM missing", &s),
        Err(Error::Bind(_))
    ));
    assert!(matches!(
        prepare("SELECT missing FROM users", &s),
        Err(Error::Bind(_))
    ));
    assert!(matches!(
        prepare("SELECT * FROM users WHERE 1", &s),
        Err(Error::Bind(_))
    ));
    // Division by zero is NOT an error: it folds to NULL (sqlite semantics),
    // so the statement prepares cleanly.
    assert!(prepare("SELECT 1/0 FROM users", &s).is_ok());
}

#[test]
fn quoted_identifiers() {
    let s = schema();
    let a = prepare("SELECT \"id\" FROM \"users\" WHERE \"email\" = 'x'", &s).unwrap();
    let b = prepare("SELECT id FROM users WHERE email = 'x'", &s).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.hash(), b.hash());
}
