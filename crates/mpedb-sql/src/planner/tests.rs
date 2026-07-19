use super::*;
use crate::prepare;
use mpedb_types::{ColumnDef, DefaultExpr};

fn col(name: &str, ty: ColumnType) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        ty,
        nullable: true,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: mpedb_types::Collation::Binary,
        affinity: mpedb_types::Affinity::implied_by(ty),
    }
}

/// Tables sort by name: events = 0, orders = 1, users = 2.
pub(crate) fn test_schema() -> Schema {
    let users = TableDef {
        id: 0,
        name: "users".into(),
        columns: vec![
            ColumnDef { nullable: false, ..col("id", ColumnType::Int64) },
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
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    };
    let orders = TableDef {
        id: 0,
        name: "orders".into(),
        columns: vec![
            ColumnDef { nullable: false, ..col("user_id", ColumnType::Int64) },
            ColumnDef { nullable: false, ..col("item_no", ColumnType::Int64) },
            ColumnDef { unique: true, ..col("sku", ColumnType::Text) },
            col("note", ColumnType::Text),
        ],
        primary_key: vec![0, 1],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
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
        kind: mpedb_types::TableKind::Standard,
    };
    Schema::new(vec![users, orders, events]).unwrap()
}

fn access_of(plan: &CompiledPlan) -> &AccessPath {
    match &plan.stmt {
        PlanStmt::Select(SelectPlan { access, .. })
        | PlanStmt::Update { access, .. }
        | PlanStmt::Delete { access, .. } => access,
        other => panic!("no access path in {other:?}"),
    }
}

fn filter_of(plan: &CompiledPlan) -> Option<&mpedb_types::ExprProgram> {
    match &plan.stmt {
        PlanStmt::Select(SelectPlan { filter, .. })
        | PlanStmt::Update { filter, .. }
        | PlanStmt::Delete { filter, .. } => filter.as_ref(),
        other => panic!("no filter in {other:?}"),
    }
}

#[test]
fn secondary_index_numbering() {
    let s = test_schema();
    // users: id is by itself the whole PK -> skipped even though the PK
    // tree covers it; email (declared unique) is index 1.
    assert_eq!(secondary_indexes(s.table(2).unwrap()), vec![Some(1)]);
    // orders: sku is index 1.
    assert_eq!(secondary_indexes(s.table(1).unwrap()), vec![Some(2)]);
    // A unique column that is part of a multi-column PK is NOT skipped.
    let t = TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef {
                nullable: false,
                unique: true,
                indexed: false,
                ..col("a", ColumnType::Int64)
            },
            ColumnDef { nullable: false, ..col("b", ColumnType::Int64) },
        ],
        primary_key: vec![0, 1],
        indexes: vec![],
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    };
    // The derivation lives in Schema::new now (single source: TableDef.indexes).
    let s = Schema::new(vec![t]).unwrap();
    assert_eq!(secondary_indexes(s.table(0).unwrap()), vec![Some(0)]);
}

#[test]
fn pk_point_on_single_column_pk() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id = $1", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Param(0)]));
    assert!(filter_of(&p).is_none());
    assert_eq!(p.param_types, vec![Some(ColumnType::Int64)]);
    // Reversed operand order works too, with a literal into the pool.
    let p = prepare("SELECT * FROM users WHERE 5 = id", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Const(0)]));
    assert_eq!(p.consts, vec![Value::Int(5)]);
}

#[test]
fn pk_point_consumes_only_key_conjuncts() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id = 1 AND age > 2", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Const(0)]));
    let f = filter_of(&p).expect("residual filter");
    // Residual is `age > 2`.
    let name = crate::plan::render_program(f, &|c| format!("c{c}"));
    assert_eq!(name, "c2 > 2");
}

#[test]
fn multi_column_pk_point_and_point_range() {
    let s = test_schema();
    let p = prepare(
        "SELECT * FROM orders WHERE user_id = 1 AND item_no = $1",
        &s,
    )
    .unwrap();
    assert_eq!(
        access_of(&p),
        &AccessPath::PkPoint(vec![KeyPart::Const(0), KeyPart::Param(0)])
    );
    // Only the first PK column pinned: inclusive point-range.
    let p = prepare("SELECT * FROM orders WHERE user_id = 7", &s).unwrap();
    let b = KeyBound {
        parts: vec![KeyPart::Const(0)],
        inclusive: true,
    };
    assert_eq!(
        access_of(&p),
        &AccessPath::PkRange {
            lo: Some(b.clone()),
            hi: Some(b)
        }
    );
    assert!(filter_of(&p).is_none());
    // Second PK column alone cannot be used: full scan + residual.
    let p = prepare("SELECT * FROM orders WHERE item_no = 7", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::FullScan);
    assert!(filter_of(&p).is_some());
}

#[test]
fn pk_range_extraction() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id > 1 AND id <= $1", &s).unwrap();
    assert_eq!(
        access_of(&p),
        &AccessPath::PkRange {
            lo: Some(KeyBound {
                parts: vec![KeyPart::Const(0)],
                inclusive: false
            }),
            hi: Some(KeyBound {
                parts: vec![KeyPart::Param(0)],
                inclusive: true
            }),
        }
    );
    assert!(filter_of(&p).is_none());
    // One-sided range.
    let p = prepare("SELECT * FROM users WHERE id >= 10", &s).unwrap();
    assert_eq!(
        access_of(&p),
        &AccessPath::PkRange {
            lo: Some(KeyBound {
                parts: vec![KeyPart::Const(0)],
                inclusive: true
            }),
            hi: None,
        }
    );
    // Extra bounds on the same column stay in the residual.
    let p = prepare("SELECT * FROM users WHERE id > 1 AND id > 2", &s).unwrap();
    assert!(matches!(access_of(&p), AccessPath::PkRange { lo: Some(_), hi: None }));
    assert!(filter_of(&p).is_some());
}

/// The whole reason BETWEEN is desugared in the parser instead of carried
/// as its own node: `x >= lo AND x <= hi` is the shape extract_access
/// already recognises, so BETWEEN becomes a range SCAN with no residual
/// filter and no second spelling for the planner to learn.
#[test]
fn between_plans_as_a_range_scan_not_a_full_scan() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id BETWEEN 1 AND $1", &s).unwrap();
    assert_eq!(
        access_of(&p),
        &AccessPath::PkRange {
            lo: Some(KeyBound {
                parts: vec![KeyPart::Const(0)],
                inclusive: true
            }),
            hi: Some(KeyBound {
                parts: vec![KeyPart::Param(0)],
                inclusive: true
            }),
        }
    );
    assert!(filter_of(&p).is_none(), "BETWEEN must leave no residual filter");

    // NOT BETWEEN cannot be a range (it is the complement of one), so it
    // must fall back honestly rather than plan a wrong range.
    let p = prepare("SELECT * FROM users WHERE id NOT BETWEEN 1 AND 5", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::FullScan);
    assert!(filter_of(&p).is_some());
}

#[test]
fn in_list_is_a_full_scan_with_a_residual_for_now() {
    // A PK IN-list could become n point lookups; it does not yet, and the
    // honest plan is a scan plus the filter -- correct, just not clever.
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id IN (1, 2)", &s).unwrap();
    assert_eq!(access_of(&p), &AccessPath::FullScan);
    assert!(filter_of(&p).is_some());
}

#[test]
fn unique_probe_beats_pk_range() {
    // `WHERE id >= $1 AND email = $2` must be a unique probe with the
    // range as residual — not an unbounded PK range scan (workbench
    // finding, 2026-07-13).
    let schema = test_schema();
    let p = prepare("SELECT id FROM users WHERE id >= $1 AND email = $2", &schema).unwrap();
    match &p.stmt {
        PlanStmt::Select(SelectPlan { access, filter, .. }) => {
            assert!(
                matches!(access, AccessPath::IndexPoint { .. }),
                "expected IndexPoint, got {access:?}"
            );
            assert!(filter.is_some(), "range conjunct must remain as residual");
        }
        other => panic!("unexpected stmt {other:?}"),
    }
}

#[test]
fn index_point_on_unique_column() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE email = $1 AND age = 3", &s).unwrap();
    assert_eq!(
        access_of(&p),
        &AccessPath::IndexPoint {
            index_no: 1,
            parts: vec![KeyPart::Param(0)]
        }
    );
    assert!(filter_of(&p).is_some());
    assert_eq!(p.footprint.indexes_used, 0b11); // PK fetch + index 1
    assert_eq!(p.footprint.key_access, KeyAccess::Full);
    // PK access beats index access.
    let p = prepare("SELECT * FROM users WHERE email = 'a' AND id = 1", &s).unwrap();
    assert!(matches!(access_of(&p), AccessPath::PkPoint(_)));
}

#[test]
fn null_literal_is_never_a_key() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id = NULL", &s).unwrap();
    // `id = NULL` folds to NULL, which is not extractable: full scan.
    assert_eq!(access_of(&p), &AccessPath::FullScan);
    assert!(filter_of(&p).is_some());
}

#[test]
fn order_by_pk_prefix_elision() {
    let s = test_schema();
    let order = |sql: &str| match prepare(sql, &s).unwrap().stmt {
        PlanStmt::Select(SelectPlan { order_by, .. }) => order_by,
        other => panic!("{other:?}"),
    };
    use mpedb_types::Collation::Binary;
    assert_eq!(order("SELECT * FROM users ORDER BY id"), vec![]);
    assert_eq!(order("SELECT * FROM users ORDER BY id ASC"), vec![]);
    assert_eq!(
        order("SELECT * FROM users ORDER BY id DESC"),
        vec![(0u16, true, Binary)]
    );
    assert_eq!(
        order("SELECT * FROM users ORDER BY email"),
        vec![(1u16, false, Binary)]
    );
    assert_eq!(order("SELECT * FROM orders ORDER BY user_id, item_no"), vec![]);
    assert_eq!(order("SELECT * FROM orders ORDER BY user_id"), vec![]);
    assert_eq!(
        order("SELECT * FROM orders ORDER BY item_no, user_id"),
        vec![(1u16, false, Binary), (0, false, Binary)]
    );
    // Not elided over an index probe (index order != PK order).
    assert_eq!(
        order("SELECT * FROM users WHERE email = 'x' ORDER BY id"),
        vec![(0u16, false, Binary)]
    );
    // Unknown ORDER BY column is a bind error.
    assert!(matches!(
        prepare("SELECT * FROM users ORDER BY nope", &s),
        Err(Error::Bind(_))
    ));
}

#[test]
fn select_star_projects_all_columns_in_order() {
    let s = test_schema();
    match prepare("SELECT * FROM users", &s).unwrap().stmt {
        PlanStmt::Select(SelectPlan { projection, .. }) => {
            assert_eq!(
                projection,
                (0..6u16).map(Projection::Column).collect::<Vec<_>>()
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_footprint_point_extraction() {
    let s = test_schema();
    // Single row, PK from a literal: exact point write set.
    let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a')", &s).unwrap();
    assert_eq!(p.footprint.key_access, KeyAccess::Point(vec![KeyPart::Const(0)]));
    assert_eq!(p.footprint.tables_written, 1 << 2);
    assert_eq!(p.footprint.tables_read, 0);
    assert_eq!(p.footprint.indexes_used, 0b11); // PK + email index
    assert!(!p.footprint.read_only);
    // Multi-row: Full.
    let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b')", &s).unwrap();
    assert_eq!(p.footprint.key_access, KeyAccess::Full);
    // Defaulted PK: Full.
    let p = prepare("INSERT INTO events (msg) VALUES ('x')", &s).unwrap();
    assert_eq!(p.footprint.key_access, KeyAccess::Full);
    match &p.stmt {
        PlanStmt::Insert { rows, .. } => {
            assert_eq!(rows[0][0], InsertSource::Default);
            assert!(matches!(rows[0][1], InsertSource::Const(_)));
        }
        other => panic!("{other:?}"),
    }
    // Multi-column PK point.
    let p = prepare("INSERT INTO orders (user_id, item_no) VALUES ($1, $2)", &s).unwrap();
    assert_eq!(
        p.footprint.key_access,
        KeyAccess::Point(vec![KeyPart::Param(0), KeyPart::Param(1)])
    );
}

#[test]
fn update_delete_footprints() {
    let s = test_schema();
    let p = prepare("UPDATE users SET age = age + 1 WHERE id = $1", &s).unwrap();
    assert_eq!(p.footprint.tables_read, 1 << 2);
    assert_eq!(p.footprint.tables_written, 1 << 2);
    assert_eq!(p.footprint.indexes_used, 0b01); // age has no index
    assert!(matches!(p.footprint.key_access, KeyAccess::Point(_)));
    assert!(!p.footprint.read_only);
    // Updating an indexed column adds its bit.
    let p = prepare("UPDATE users SET email = $1 WHERE id = $2", &s).unwrap();
    assert_eq!(p.footprint.indexes_used, 0b11);
    // Delete maintains every index.
    let p = prepare("DELETE FROM users WHERE id = 1", &s).unwrap();
    assert_eq!(p.footprint.indexes_used, 0b11);
    assert!(matches!(p.footprint.key_access, KeyAccess::Point(_)));
    let p = prepare("DELETE FROM orders", &s).unwrap();
    assert_eq!(p.footprint.key_access, KeyAccess::Full);
    assert_eq!(p.footprint.indexes_used, 0b11);
}

#[test]
fn txn_control_footprints() {
    let s = test_schema();
    for sql in ["BEGIN", "COMMIT", "ROLLBACK"] {
        let p = prepare(sql, &s).unwrap();
        assert_eq!(p.footprint.tables_read, 0);
        assert_eq!(p.footprint.tables_written, 0);
        assert_eq!(p.footprint.indexes_used, 0);
        assert_eq!(p.footprint.key_access, KeyAccess::Full);
        assert!(p.footprint.read_only);
        assert_eq!(p.n_params, 0);
    }
}

#[test]
fn update_rejects_pk_and_bad_types() {
    let s = test_schema();
    match prepare("UPDATE users SET id = 2 WHERE id = 1", &s) {
        Err(Error::Bind(m)) => assert!(m.contains("primary key")),
        other => panic!("expected bind error, got {other:?}"),
    }
    assert!(matches!(
        prepare("UPDATE orders SET item_no = 1", &s),
        Err(Error::Bind(_))
    ));
    assert!(matches!(
        prepare("UPDATE users SET age = 'x'", &s),
        Err(Error::Bind(_))
    ));
    assert!(matches!(
        prepare("UPDATE users SET email = NULL", &s),
        Err(Error::Bind(_))
    ));
    // A column assigned more than once keeps only the rightmost occurrence
    // (sqlite R-34751-18293) — accepted, not an error, and it compiles to ONE
    // assignment for that column.
    match &prepare("UPDATE users SET age = 1, age = 2", &s).unwrap().stmt {
        PlanStmt::Update { set, .. } => assert_eq!(set.len(), 1),
        other => panic!("expected Update, got {other:?}"),
    }
    // Int expression into float column is coerced.
    let p = prepare("UPDATE users SET score = age + 1 WHERE id = 1", &s).unwrap();
    match &p.stmt {
        PlanStmt::Update { set, .. } => {
            let rendered = crate::plan::render_program(&set[0].1, &|c| format!("c{c}"));
            assert_eq!(rendered, "float(c2 + 1)");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_binding_rules() {
    let s = test_schema();
    // Omitting a NOT NULL column without default is a bind error.
    match prepare("INSERT INTO users (id) VALUES (1)", &s) {
        Err(Error::Bind(m)) => assert!(m.contains("email")),
        other => panic!("expected bind error, got {other:?}"),
    }
    // Explicit NULL into NOT NULL column.
    assert!(matches!(
        prepare("INSERT INTO users (id, email) VALUES (1, NULL)", &s),
        Err(Error::Bind(_))
    ));
    // Type mismatch.
    assert!(matches!(
        prepare("INSERT INTO users (id, email) VALUES ('x', 'a')", &s),
        Err(Error::Bind(_))
    ));
    // Non-literal expressions are rejected.
    assert!(matches!(
        prepare("INSERT INTO users (id, email) VALUES (1 + id, 'a')", &s),
        Err(Error::Bind(_))
    ));
    // ...but constant-foldable expressions are fine.
    let p = prepare("INSERT INTO users (id, email) VALUES (-1, 'a')", &s).unwrap();
    assert_eq!(p.consts[0], Value::Int(-1));
    // Int literal into float column is folded to a float const.
    let p = prepare("INSERT INTO users (id, email, score) VALUES (1, 'a', 5)", &s).unwrap();
    assert_eq!(p.consts[2], Value::Float(5.0));
    // Wrong tuple width.
    assert!(matches!(
        prepare("INSERT INTO users (id, email) VALUES (1)", &s),
        Err(Error::Bind(_))
    ));
    // Duplicate column.
    assert!(matches!(
        prepare("INSERT INTO users (id, id) VALUES (1, 2)", &s),
        Err(Error::Bind(_))
    ));
    // Param types unify to column types.
    let p = prepare("INSERT INTO users (id, email, score) VALUES ($1, $2, $3)", &s).unwrap();
    assert_eq!(
        p.param_types,
        vec![
            Some(ColumnType::Int64),
            Some(ColumnType::Text),
            Some(ColumnType::Float64)
        ]
    );
    // Conflicting param inference across columns.
    assert!(matches!(
        prepare("INSERT INTO users (id, email) VALUES ($1, $1)", &s),
        Err(Error::Bind(_))
    ));
}

#[test]
fn unknown_table_is_bind_error() {
    let s = test_schema();
    match prepare("SELECT * FROM nope", &s) {
        Err(Error::Bind(m)) => assert!(m.contains("nope")),
        other => panic!("expected bind error, got {other:?}"),
    }
}
