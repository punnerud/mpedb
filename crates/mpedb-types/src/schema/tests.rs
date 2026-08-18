use super::*;

fn sample() -> Schema {
    Schema::new(vec![TableDef {
        id: 0,
        name: "users".into(),
        columns: vec![
            ColumnDef { generated: None, default_text: None, decl: None,
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: Affinity::implied_by(ColumnType::Int64),
            },
            ColumnDef { generated: None, default_text: None, decl: None,
                name: "email".into(),
                ty: ColumnType::Text,
                nullable: false,
                unique: true,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: Affinity::implied_by(ColumnType::Text),
            },
            ColumnDef { generated: None, default_text: None, decl: None,
                name: "age".into(),
                ty: ColumnType::Int64,
                nullable: true,
                unique: false,
                indexed: false,
                default: Some(DefaultExpr::Const(Value::Int(0))),
                check: Some("age >= 0 AND age < 200".into()),
                collation: Collation::Binary,
                affinity: Affinity::implied_by(ColumnType::Int64),
            },
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap()
}

#[test]
fn canonical_roundtrip_and_stable_hash() {
    let s = sample();
    let restored = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, restored);
    assert_eq!(s.hash(), restored.hash());
}

#[test]
fn table_order_is_name_sorted_regardless_of_input_order() {
    let mk = |names: &[&str]| {
        Schema::new(
            names
                .iter()
                .map(|n| TableDef {
                    id: 0,
                    name: n.to_string(),
                    columns: vec![ColumnDef { generated: None, default_text: None, decl: None,
                        name: "id".into(),
                        ty: ColumnType::Int64,
                        nullable: false,
                        unique: false,
                        indexed: false,
                        default: None,
                        check: None, collation: Collation::Binary,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    }],
                    primary_key: vec![0],
                    indexes: vec![],
                    dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
                    foreign_keys: Vec::new(),
                })
                .collect(),
        )
        .unwrap()
    };
    assert_eq!(mk(&["b", "a"]).hash(), mk(&["a", "b"]).hash());
    assert_eq!(mk(&["b", "a"]).table_id("a"), Some(0));
}

#[test]
fn rejects_bad_schemas() {
    // nullable PK
    let bad = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![ColumnDef { generated: None, default_text: None, decl: None,
            name: "id".into(),
            ty: ColumnType::Int64,
            nullable: true,
            unique: false,
            indexed: false,
            default: None,
            check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        }],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }]);
    assert!(bad.is_err());
    // reserved prefix
    let bad = Schema::new(vec![TableDef {
        id: 0,
        name: "__mpedb_plans".into(),
        columns: vec![ColumnDef { generated: None, default_text: None, decl: None,
            name: "id".into(),
            ty: ColumnType::Int64,
            nullable: false,
            unique: false,
            indexed: false,
            default: None,
            check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        }],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }]);
    assert!(bad.is_err());
}
/// **The constraint that used to block `CREATE TABLE`, and its fix**
/// (DESIGN-SCHEMA-V2). v1's table id WAS the name-sort position, so
/// adding a table renumbered every table sorting after it — and the id
/// is a key (`cat_tree_key`, CDC bitmaps, mirror families, every plan).
/// v2 makes the id EXPLICIT in the canonical bytes; seeding still
/// assigns name-sorted (deterministic under config reordering), but
/// CREATE TABLE will APPEND at the next free id. In this format window
/// ids must stay dense 0..n (`position == id` is enforced, which is
/// what keeps every positional engine cache correct until DROP's audit).
#[test]
fn ids_are_dense_explicit_and_survive_the_wire() {
    let col = |n: &str| ColumnDef { generated: None, default_text: None, decl: None, name: n.into(), ty: ColumnType::Int64,
            affinity: Affinity::implied_by(ColumnType::Int64),
        nullable: false, unique: false, indexed: false, default: None, check: None, collation: Collation::Binary };
    let tbl = |n: &str| TableDef { id: 0, name: n.into(), columns: vec![col("id")],
        primary_key: vec![0], indexes: vec![], dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
            foreign_keys: Vec::new(),
        };

    let s = Schema::new(vec![tbl("orders"), tbl("users"), tbl("accounts")]).unwrap();
    let got: Vec<(&str, u32)> =
        s.tables.iter().map(|t| (t.name.as_str(), t.id)).collect();
    assert_eq!(got, vec![("accounts", 0), ("orders", 1), ("users", 2)]);

    // Explicit in the bytes: the id round-trips, it is not re-derived.
    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r);

    // Non-dense ids refuse in this window — a gapped file must never
    // reach the positional engine caches.
    let mut gapped = s.clone();
    gapped.tables[1].id = 5;
    gapped.tables[2].id = 6;
    let err = Schema::from_canonical_bytes(&gapped.canonical_bytes()).unwrap_err();
    assert!(format!("{err}").contains("dense"), "{err}");
}

#[test]
fn indexes_derive_normalize_and_roundtrip() {
    let mut t = TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef { generated: None, default_text: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                unique: true, indexed: true, default: None, check: None, collation: Collation::Binary },
            ColumnDef { generated: None, default_text: None, decl: None, name: "a".into(), ty: ColumnType::Int64, nullable: true,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                unique: true, indexed: true, default: None, check: None, collation: Collation::Binary },
            ColumnDef { generated: None, default_text: None, decl: None, name: "b".into(), ty: ColumnType::Text, nullable: true,
                    affinity: Affinity::implied_by(ColumnType::Text),
                unique: false, indexed: true, default: None, check: None, collation: Collation::Binary },
        ],
        primary_key: vec![0],
        indexes: vec![IndexDef { columns: vec![1, 2], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false }],
        dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    };
    // The single-PK column's flags are noise and must normalize away.
    t.columns[0].unique = true;
    let s = Schema::new(vec![t]).unwrap();
    let t = &s.tables[0];
    // Derived (declaration order) then explicit, with flags normalized:
    // `a` unique+indexed → ONE unique index; PK column contributes none.
    assert_eq!(
        t.indexes,
        vec![
            // Derived from `a`'s `unique` column flag, which IS a constraint.
            IndexDef { columns: vec![1], unique: true, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: true },
            IndexDef { columns: vec![2], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false },
            IndexDef { columns: vec![1, 2], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false },
        ]
    );
    assert!(!t.columns[0].unique && !t.columns[0].indexed);
    assert!(t.columns[1].unique && !t.columns[1].indexed);

    // Wire round-trip reconstructs the same flags and the same list.
    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r);
    assert_eq!(s.hash(), r.hash());
}

#[test]
fn hostile_bytes_refuse_cleanly() {
    let col = |n: &str, ty| ColumnDef { generated: None, default_text: None, decl: None, name: n.into(), ty, nullable: true,
            affinity: Affinity::implied_by(ty),
        unique: false, indexed: false, default: None, check: None, collation: Collation::Binary };
    let base = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef { generated: None, default_text: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                unique: false, indexed: false, default: None, check: None, collation: Collation::Binary },
            col("v", ColumnType::Any),
            col("w", ColumnType::Int64),
        ],
        primary_key: vec![0],
        indexes: vec![IndexDef { columns: vec![2], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false }],
        dead: false, pk_name: None, kind: TableKind::Standard, implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();

    // An index over an `any` column is ACCEPTED now (`ANY_KEY_COLUMNS`):
    // such a tree is keyed by storage class, whose equality and order are
    // sqlite's, and the planner — not the schema — is what keeps it from
    // ever being probed.
    let mut ok = base.clone();
    ok.tables[0].indexes = vec![IndexDef { columns: vec![1], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false }];
    Schema::from_canonical_bytes(&ok.canonical_bytes()).unwrap();

    // Duplicate index shapes refuse.
    let mut evil = base.clone();
    evil.tables[0]
        .indexes
        .push(IndexDef { columns: vec![2], unique: false, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false });
    assert!(Schema::from_canonical_bytes(&evil.canonical_bytes()).is_err());

    // An index equal to the whole single-column PK is REDUNDANT, not
    // illegal: the PK tree answers every probe it could, but sqlite builds
    // it and Django's `remove_unique_together` on a pk field emits exactly
    // this. Refusing it blocked a migration with nothing wrong in it.
    let mut redundant = base.clone();
    redundant.tables[0].indexes = vec![IndexDef { columns: vec![0], unique: true, predicate: None, name: None, exprs: Vec::new(), collations: Vec::new(), from_constraint: false }];
    Schema::from_canonical_bytes(&redundant.canonical_bytes()).unwrap();

    // v1 bytes (version byte 1) refuse by name — no migration exists.
    let mut v1ish = base.canonical_bytes();
    v1ish[0] = 1;
    let err = Schema::from_canonical_bytes(&v1ish).unwrap_err();
    assert!(format!("{err}").contains("unknown schema version 1"), "{err}");

    // Truncation at EVERY offset yields Corrupt, never a panic.
    let bytes = base.canonical_bytes();
    for i in 0..bytes.len() {
        assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
    }
}

fn tbl(n: &str) -> TableDef {
    let col = |n: &str| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty: ColumnType::Int64, nullable: false,
        unique: false, indexed: false, default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ColumnType::Int64),
    };
    TableDef { id: 0, name: n.into(), columns: vec![col("id")],
        primary_key: vec![0], indexes: vec![], dead: false, pk_name: None, kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false, foreign_keys: Vec::new() }
}

#[test]
fn identifier_rule_allows_what_quoting_exists_for() {
    // Everything sqlite 3.45.1 accepts and we can represent faithfully.
    for good in [
        "weird tbl",
        "tbl-with.punct!",
        "1st table",
        "tabell_æøå",
        "a\"b",
        "  padded  ",
        "   ", // whitespace-only: a footgun, not a hazard, and sqlite allows it
        "[bracketish]",
        "`tick`",
        &"a".repeat(MAX_IDENTIFIER_LEN),
        &format!("app_{}", "m".repeat(130)), // 134: Django's m2m through-table
    ] {
        assert!(valid_identifier(good), "should be valid: {good:?}");
        // …and it survives canonical bytes byte-for-byte.
        let sch = Schema::new(vec![tbl(good)]).unwrap();
        let back = Schema::from_canonical_bytes(&sch.canonical_bytes()).unwrap();
        assert_eq!(back.tables[0].name, good);
        assert_eq!(back, sch);
    }
    // What stays refused, and why (see `valid_identifier`).
    for bad in [
        "",                              // no identity in any surface
        "nul\u{0}name",                  // NUL truncates the C-API's const char*
        "line\nbreak",                   // breaks every line-oriented surface
        "carriage\rreturn",
        "tab\tsep",
        "del\u{7f}",
        "c1\u{85}",                       // C1 control
        "__mpedb_internal",              // reserved prefix
        &"a".repeat(MAX_IDENTIFIER_LEN + 1),
    ] {
        assert!(!valid_identifier(bad), "should be invalid: {bad:?}");
    }
    // A non-ASCII name is measured in BYTES, not chars: 128 × 2-byte chars
    // is 256 bytes and over the limit even though it is 128 characters.
    let wide = "æ".repeat(128);
    assert_eq!(wide.chars().count(), 128);
    assert!(!valid_identifier(&wide));
}

#[test]
fn drop_tombstones_in_place_and_never_reuses_the_id() {
    // Seed {a,b,c} → ids {0,1,2} (name-sorted).
    let s = Schema::new(vec![tbl("a"), tbl("b"), tbl("c")]).unwrap();
    assert_eq!(s.table_id("b"), Some(1));

    // Drop the MIDDLE table (id 1). The slot is tombstoned in place:
    // position == id still holds, `b` is gone, `a`/`c` untouched.
    let s = s.with_dropped_table(1).unwrap();
    assert_eq!(s.tables.len(), 3, "vec does not shrink — dead slot stays");
    assert!(s.tables[1].dead && s.tables[1].name.is_empty());
    assert_eq!(s.table_id("b"), None, "dropped name no longer resolves");
    assert_eq!(s.table_id("a"), Some(0));
    assert_eq!(s.table_id("c"), Some(2));
    assert_eq!(s.live_tables().count(), 2);
    // Every slot's id equals its position (dense, dead included).
    for (pos, t) in s.tables.iter().enumerate() {
        assert_eq!(t.id, pos as u32);
    }

    // A new table takes the NEXT id (3 = tables.len()), NEVER the dropped
    // id 1 — the no-reuse guarantee, from the materialized dead slot.
    let s = s.with_added_table(tbl("d")).unwrap();
    assert_eq!(s.table_id("d"), Some(3));
    assert_eq!(s.tables.len(), 4);

    // Re-CREATE the dropped NAME: gets a fresh id (4), not the old 1.
    let s = s.with_added_table(tbl("b")).unwrap();
    assert_eq!(s.table_id("b"), Some(4));

    // Drop the HIGHEST id (4) — len unchanged, so the next mint is still
    // 5, not the just-freed 4.
    let s = s.with_dropped_table(4).unwrap();
    assert_eq!(s.tables.len(), 5);
    let s = s.with_added_table(tbl("e")).unwrap();
    assert_eq!(s.table_id("e"), Some(5));
}

#[test]
fn tombstoned_schema_round_trips_through_v3_bytes() {
    let s = Schema::new(vec![tbl("a"), tbl("b"), tbl("c")]).unwrap();
    let s = s.with_dropped_table(1).unwrap(); // dead slot at 1
    let s = s.with_added_table(tbl("z")).unwrap(); // id 3
    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r, "dead slot + ids survive the wire byte-for-byte");
    assert_eq!(s.hash(), r.hash());
    // The version byte is 9.
    assert_eq!(s.canonical_bytes()[0], 20);
    // A v8 file refuses cleanly (no misread of the new generated bytes).
    let mut v8 = s.canonical_bytes();
    v8[0] = 8;
    let err = Schema::from_canonical_bytes(&v8).unwrap_err();
    assert!(format!("{err}").contains("unknown schema version 8"), "{err}");
}

#[test]
fn hostile_tombstone_bytes_refuse() {
    // A "dead" slot that carries content is corrupt.
    let s = Schema::new(vec![tbl("a"), tbl("b")]).unwrap();
    let mut evil = s.with_dropped_table(1).unwrap();
    evil.tables[1].dead = true;
    evil.tables[1].name = "ghost".into(); // a dead slot must be empty
    let err = Schema::from_canonical_bytes(&evil.canonical_bytes()).unwrap_err();
    assert!(format!("{err}").contains("tombstone"), "{err}");
    // An all-tombstone schema is LEGAL — it is exactly what CREATE+DROP
    // leaves in a zero-table-seed database — and must round-trip.
    let mut none = Schema::new(vec![tbl("a")]).unwrap();
    none.tables[0] = TableDef::tombstone(0);
    let r = Schema::from_canonical_bytes(&none.canonical_bytes()).unwrap();
    assert_eq!(none.tables.len(), r.tables.len());
    assert!(r.tables[0].dead);
}

#[test]
fn create_refuses_at_the_id_ceiling() {
    // Fill to MAX_TABLES with live + dead slots; the next create fails
    // closed rather than minting id >= MAX_TABLES. No-reuse
    // means DROP+CREATE churn grows `tables.len()` by one per cycle, so a
    // churny workload is what eventually reaches this bound — the
    // deliberate, bounded, detectable limit (§0), with offline `regenerate`
    // compaction as the escape hatch.
    let tables: Vec<TableDef> = (0..MAX_TABLES - 8).map(|i| tbl(&format!("t{i}"))).collect();
    let mut s = Schema::new(tables).unwrap();
    // Burn ids up to the ceiling via drop+recreate without exceeding the
    // live-count guard (dead slots accumulate; live count stays flat).
    while s.tables.len() < MAX_TABLES {
        let id = s.tables.iter().rposition(|t| !t.dead).unwrap() as u32;
        s = s.with_dropped_table(id).unwrap();
        if s.tables.len() < MAX_TABLES {
            s = s.with_added_table(tbl(&format!("r{}", s.tables.len()))).unwrap();
        }
    }
    assert_eq!(s.tables.len(), MAX_TABLES);
    let err = s.with_added_table(tbl("overflow")).unwrap_err();
    assert!(format!("{err}").contains("exhausted"), "{err}");
    // The refusal has to say what to DO about it — an operator who is out of
    // ids at 4096 has two ways forward and no way to guess either from
    // "exhausted".
    assert!(format!("{err}").contains("compact-ids"), "{err}");
    assert!(format!("{err}").contains("max_tables"), "{err}");
}

/// `[database] max_tables` binds the MINT, and the ceiling binds the mint's
/// config. Cheap to test at a small cap, which is the point: the budget is a
/// parameter now, so exercising it no longer costs 4096 tables.
#[test]
fn configured_cap_bounds_the_mint() {
    let mut s = Schema::new(vec![tbl("a")]).unwrap();
    s = s.with_added_table_capped(tbl("b"), 3).unwrap();
    s = s.with_added_table_capped(tbl("c"), 3).unwrap();
    let err = s.with_added_table_capped(tbl("d"), 3).unwrap_err();
    assert!(format!("{err}").contains('3'), "{err}");

    // A tombstone still SPENDS its slot — that is the whole shape of the
    // problem the cap describes, and a cap that counted only live tables
    // would quietly make the budget mean something else.
    let s2 = s.with_dropped_table(2).unwrap();
    assert_eq!(s2.tables.len(), 3);
    assert!(s2.with_added_table_capped(tbl("d"), 3).is_err());

    // A config may not authorise minting past what a decoder accepts.
    let huge = s.with_added_table_capped(tbl("d"), usize::MAX);
    assert!(huge.is_ok(), "a cap above the ceiling clamps, it does not fail");
}

/// An implicit-rowid table (#94): visible columns plus a trailing hidden
/// Int64 `rowid` sole PK. It round-trips byte-for-byte through v5, the flag
/// survives, and the helpers report the right visible set.
#[test]
fn implicit_rowid_round_trips_and_helpers() {
    let col = |n: &str, ty| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty, nullable: true, unique: false, indexed: false,
        default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ty),
    };
    let rowid = ColumnDef { generated: None, default_text: None, decl: None,
        name: "rowid".into(), ty: ColumnType::Int64, nullable: false,
        unique: false, indexed: false, default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ColumnType::Int64),
    };
    let s = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![col("a", ColumnType::Any), col("b", ColumnType::Text), rowid],
        primary_key: vec![2],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: true, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    let t = &s.tables[0];
    assert_eq!(t.hidden_rowid_col(), Some(2));
    assert_eq!(t.visible_column_count(), 2);
    assert_eq!(
        t.visible_columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(t.rowid_name_col("rowid"), Some(2));
    assert_eq!(t.rowid_name_col("OID"), Some(2));
    assert_eq!(t.rowid_name_col("_rowid_"), Some(2));
    assert_eq!(t.rowid_name_col("a"), None);
    // The auto-assign machinery treats it as a rowid alias.
    assert_eq!(t.rowid_alias_col(), Some(2));

    // A declared single int64 PK (no hidden column) is sqlite's INTEGER
    // PRIMARY KEY shape: the alias spellings resolve to the PK itself.
    // A TEXT PK is the WITHOUT-ROWID analog — the name stays unknown,
    // exactly as sqlite refuses it there (oracle-checked).
    let pkcol = |n: &str, ty| {
        let mut c = col(n, ty);
        c.nullable = false;
        c
    };
    let ipk = Schema::new(vec![TableDef {
        id: 0,
        name: "n".into(),
        columns: vec![pkcol("id", ColumnType::Int64), col("x", ColumnType::Text)],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    assert_eq!(ipk.tables[0].rowid_name_col("rowid"), Some(0));
    assert_eq!(ipk.tables[0].rowid_name_col("_ROWID_"), Some(0));
    let wr = Schema::new(vec![TableDef {
        id: 0,
        name: "w".into(),
        columns: vec![pkcol("k", ColumnType::Text), col("v", ColumnType::Int64)],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    assert_eq!(wr.tables[0].rowid_name_col("rowid"), None);

    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r);
    assert_eq!(s.hash(), r.hash());
    assert_eq!(s.canonical_bytes()[0], 20);

    // Truncation at every offset is Corrupt, never a panic.
    let bytes = s.canonical_bytes();
    for i in 0..bytes.len() {
        assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
    }
}

/// The VERBATIM declared-type text survives the v8 wire byte-for-byte, is
/// what `decltype()` reports (canonical name only where there is no text),
/// contributes to the hash, and truncation at every offset is Corrupt.
#[test]
fn column_decl_text_round_trips_and_is_hostile_safe() {
    let col = |n: &str, ty, decl: Option<&str>| ColumnDef { generated: None, default_text: None,
        name: n.into(),
        ty,
        nullable: true,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: Collation::Binary,
        affinity: Affinity::implied_by(ty),
        decl: decl.map(str::to_string),
    };
    let s = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef { generated: None, nullable: false, ..col("id", ColumnType::Int64, Some("INTEGER")) },
            // `float` is the case the canonical name loses: mpedb stores
            // Float64, whose canonical spelling is REAL.
            col("f", ColumnType::Float64, Some("float")),
            // No declared type at all: sqlite reports NULL, and so must this.
            col("n", ColumnType::Any, None),
            // An unknown name is legal in sqlite and IS the decltype.
            col("x", ColumnType::Any, Some("number(5)")),
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    let c = &s.tables[0].columns;
    assert_eq!(c[1].decltype(), Some("float"), "verbatim, not REAL");
    assert_eq!(c[2].decltype(), None, "no declared type ⇒ sqlite's NULL");
    assert_eq!(c[3].decltype(), Some("number(5)"));
    // A column with no text falls back to the canonical name.
    let mut plain = s.clone();
    plain.tables[0].columns[1].decl = None;
    assert_eq!(plain.tables[0].columns[1].decltype(), Some("REAL"));

    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r);
    assert_eq!(s.hash(), r.hash());
    assert_eq!(s.canonical_bytes()[0], 20);
    // The text is part of the schema identity: `f float` and `f REAL` are
    // the same storage and DIFFERENT schemas, because a consumer keying
    // converters off the decltype sees two different columns.
    assert_ne!(s.hash(), plain.hash());

    let bytes = s.canonical_bytes();
    for i in 0..bytes.len() {
        assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
    }
    // A hostile blob with an out-of-range decl tag refuses cleanly. Column
    // `n` encodes as `<len> "n" <ty> <flags> <collation> <affinity> <decl>`.
    let mut evil = bytes.clone();
    let np = evil.windows(5).position(|w| w == b"\x01\x00\x00\x00n").unwrap();
    let decl_at = np + 5 + 1 /*ty*/ + 1 /*flags*/ + 1 /*collation*/ + 1 /*affinity*/;
    assert_eq!(evil[decl_at], 0, "located the decl tag byte");
    evil[decl_at] = 0x7f;
    let err = Schema::from_canonical_bytes(&evil).unwrap_err();
    assert!(format!("{err}").contains("decl"), "{err}");
}

/// A DECLARED column collation (`COLLATE NOCASE`) survives the v6 wire
/// byte-for-byte, contributes to the hash, and truncation at every offset is
/// Corrupt — the roundtrip/truncation contract extended to the new field.
#[test]
fn column_collation_round_trips_and_is_hostile_safe() {
    let col = |n: &str, ty, coll| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty, nullable: true, unique: false, indexed: false,
        default: None, check: None, collation: coll,
        affinity: Affinity::implied_by(ty),
    };
    let id = ColumnDef { generated: None, default_text: None, decl: None,
        name: "id".into(), ty: ColumnType::Int64, nullable: false, unique: false,
        indexed: false, default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ColumnType::Int64),
    };
    let s = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        // A NOCASE and an RTRIM text column, neither indexed (a collated key
        // is refused — see `collated_key_column_refused`).
        columns: vec![
            id,
            col("name", ColumnType::Text, Collation::NoCase),
            col("code", ColumnType::Text, Collation::Rtrim),
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();
    assert_eq!(s.tables[0].columns[1].collation, Collation::NoCase);
    assert_eq!(s.tables[0].columns[2].collation, Collation::Rtrim);

    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r);
    assert_eq!(s.hash(), r.hash());
    assert_eq!(s.canonical_bytes()[0], 20);

    // The collation changes the hash: a BINARY `name` is a different schema.
    let mut plain = s.clone();
    plain.tables[0].columns[1].collation = Collation::Binary;
    assert_ne!(s.hash(), plain.hash());

    // Truncation at every offset is Corrupt, never a panic.
    let bytes = s.canonical_bytes();
    for i in 0..bytes.len() {
        assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
    }

    // A hostile v6 blob with an out-of-range collation tag refuses cleanly.
    // Column `name` encodes as `<len> "name" <ty> <nullable> <collation> …`,
    // so its collation byte is 6 bytes past the start of the literal "name".
    let mut evil = s.canonical_bytes();
    let np = evil.windows(4).position(|w| w == b"name").unwrap();
    let coll_at = np + 4 /*name*/ + 1 /*ty*/ + 1 /*nullable*/;
    assert_eq!(evil[coll_at], Collation::NoCase as u8, "located the collation byte");
    evil[coll_at] = 0x7f;
    let err = Schema::from_canonical_bytes(&evil).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("collation"), "{err}");
}

/// A non-BINARY collation on a key column (PRIMARY KEY / UNIQUE / index) is
/// REFUSED — collated on-disk keys are deferred, never answered wrong. Also
/// enforced on decode (a hostile v6 blob cannot smuggle one in).
#[test]
fn collated_key_column_accepted_but_non_text_refused() {
    let mk = |unique: bool| {
        Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { generated: None, default_text: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    unique: false, indexed: false, default: None, check: None,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    collation: Collation::Binary },
                ColumnDef { generated: None, default_text: None, decl: None, name: "name".into(), ty: ColumnType::Text, nullable: true,
                    unique, indexed: !unique, default: None, check: None,
                        affinity: Affinity::implied_by(ColumnType::Text),
                    collation: Collation::NoCase },
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, pk_name: None,
            kind: TableKind::Standard,
            implicit_rowid: false, autoincrement: false,
            foreign_keys: Vec::new(),
        }])
    };
    // A UNIQUE and a plain index on a NOCASE column are now ACCEPTED (the
    // engine folds the value into the on-disk key).
    assert!(mk(true).is_ok());
    assert!(mk(false).is_ok());

    // A NOCASE PRIMARY KEY column is accepted too.
    assert!(Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![ColumnDef { generated: None, default_text: None, decl: None, name: "k".into(), ty: ColumnType::Text, nullable: false,
            unique: false, indexed: false, default: None, check: None,
                affinity: Affinity::implied_by(ColumnType::Text),
            collation: Collation::NoCase }],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .is_ok());

    // But a non-TEXT collation is still refused (collation affects text only).
    let err = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef { generated: None, default_text: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                unique: false, indexed: false, default: None, check: None,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                collation: Collation::Binary },
            ColumnDef { generated: None, default_text: None, decl: None, name: "n".into(), ty: ColumnType::Int64, nullable: true,
                unique: false, indexed: false, default: None, check: None,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                collation: Collation::NoCase },
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap_err();
    assert!(format!("{err}").contains("text"), "{err}");
}

/// A hostile v5 blob that merely flips `implicit_rowid` on a shape that is
/// not a trailing NOT-NULL Int64 `rowid` sole PK must refuse — otherwise
/// `SELECT *` would hide an arbitrary column.
#[test]
fn hostile_implicit_rowid_bytes_refuse() {
    let s = sample(); // explicit-PK `users`, last column is `age` (nullable Int64)
    let mut evil = s.clone();
    evil.tables[0].implicit_rowid = true;
    let err = Schema::from_canonical_bytes(&evil.canonical_bytes()).unwrap_err();
    assert!(format!("{err}").contains("implicit_rowid"), "{err}");
}

// ------------------------------------------------ generated columns (v9)

/// A generated column survives the v9 wire — SOURCE, kind and COMPILED
/// PROGRAM — contributes to the hash, and truncation at every offset is
/// `Corrupt`, never a panic. The program is on the wire (unlike a CHECK,
/// which is source-only) precisely so a decoded schema can evaluate the
/// column with no SQL layer present.
#[test]
fn generated_column_round_trips_and_is_hostile_safe() {
    use crate::expr::Instr;
    let plain = |n: &str, ty| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty, nullable: true, unique: false, indexed: false,
        default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ty),
    };
    // g = a + a, over column 0.
    let prog = ExprProgram::new(
        vec![Instr::PushCol(0), Instr::PushCol(0), Instr::Add],
        vec![],
    )
    .unwrap();
    let mut g = plain("g", ColumnType::Int64);
    g.generated = Some(GeneratedCol {
        expr: "a + a".into(),
        kind: GeneratedKind::Stored,
        program: prog,
    });
    let s = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            ColumnDef { nullable: false, ..plain("a", ColumnType::Int64) },
            g,
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();

    let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
    assert_eq!(s, r, "the compiled program survives byte-for-byte");
    assert_eq!(s.hash(), r.hash());
    assert_eq!(s.canonical_bytes()[0], 20);

    // It computes, in declaration order, through the decoded schema.
    let mut row = vec![Value::Int(21), Value::Null];
    r.tables[0].apply_generated(&mut row, &[]).unwrap();
    assert_eq!(row[1], Value::Int(42));
    assert!(r.tables[0].has_generated());

    let bytes = s.canonical_bytes();
    for i in 0..bytes.len() {
        assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
    }
}

/// The rules that make one declaration-order pass sound. Each is checked on
/// a HOSTILE hand-built schema, i.e. on the decode path, not just at DDL.
#[test]
fn generated_column_validation_rules() {
    use crate::expr::Instr;
    let plain = |n: &str, ty| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty, nullable: true, unique: false, indexed: false,
        default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ty),
    };
    let gen_reading = |n: &str, col: u16, kind| {
        let mut c = plain(n, ColumnType::Int64);
        c.generated = Some(GeneratedCol {
            expr: format!("col{col}"),
            kind,
            program: ExprProgram::new(vec![Instr::PushCol(col)], vec![]).unwrap(),
        });
        c
    };
    let build = |cols: Vec<ColumnDef>, pk: Vec<u16>| {
        Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: cols,
            primary_key: pk,
            indexes: vec![],
            dead: false, pk_name: None,
            kind: TableKind::Standard,
            implicit_rowid: false, autoincrement: false,
            foreign_keys: Vec::new(),
        }])
    };
    let a = || ColumnDef { nullable: false, ..plain("a", ColumnType::Int64) };

    // Reading an EARLIER generated column is fine.
    assert!(build(
        vec![a(), gen_reading("g", 0, GeneratedKind::Stored), gen_reading("h", 1, GeneratedKind::Stored)],
        vec![0]
    )
    .is_ok());

    // Reading a LATER one, or ITSELF, is refused — the two shapes that
    // would make a single declaration-order pass read a stale value.
    for (cols, what) in [
        (vec![a(), gen_reading("g", 2, GeneratedKind::Stored), gen_reading("h", 0, GeneratedKind::Stored)], "forward"),
        (vec![a(), gen_reading("g", 1, GeneratedKind::Stored)], "self"),
    ] {
        let err = build(cols, vec![0]).unwrap_err();
        assert!(
            format!("{err}").contains("declared at or after it"),
            "{what}: {err}"
        );
    }

    // Out-of-range ordinal (a corrupt mapping) is refused, not a panic.
    let err = build(vec![a(), gen_reading("g", 99, GeneratedKind::Virtual)], vec![0]).unwrap_err();
    assert!(format!("{err}").contains("out of range"), "{err}");

    // A parameter has nothing to bind to per row.
    let mut p = plain("g", ColumnType::Int64);
    p.generated = Some(GeneratedCol {
        expr: "$1".into(),
        kind: GeneratedKind::Virtual,
        program: ExprProgram::new(vec![Instr::PushParam(0)], vec![]).unwrap(),
    });
    let err = build(vec![a(), p], vec![0]).unwrap_err();
    assert!(format!("{err}").contains("references a parameter"), "{err}");

    // PRIMARY KEY and DEFAULT are both refused on a generated column.
    let err = build(vec![a(), ColumnDef { nullable: false, ..gen_reading("g", 0, GeneratedKind::Stored) }], vec![0, 1])
        .unwrap_err();
    assert!(format!("{err}").contains("PRIMARY KEY"), "{err}");
    let mut d = gen_reading("g", 0, GeneratedKind::Stored);
    d.default = Some(DefaultExpr::Const(Value::Int(1)));
    let err = build(vec![a(), d], vec![0]).unwrap_err();
    assert!(format!("{err}").contains("DEFAULT"), "{err}");
}

/// DROP COLUMN shifts ordinals; RENAME COLUMN invalidates the expression
/// TEXT. So a drop is legal and RENUMBERS the generated program, unless a
/// SURVIVING generated column reads the column being dropped — which is
/// where sqlite refuses too. A rename needs the rewritten `AS (…)` source
/// handed in, and refuses without it; a drop never needs one, because it
/// renames nothing.
#[test]
fn drop_renumbers_a_generated_program_and_rename_needs_a_rewritten_source() {
    use crate::expr::Instr;
    let plain = |n: &str, ty, nullable| ColumnDef { generated: None, default_text: None, decl: None,
        name: n.into(), ty, nullable, unique: false, indexed: false,
        default: None, check: None, collation: Collation::Binary,
        affinity: Affinity::implied_by(ty),
    };
    // `g` reads `b`, which is column 2 — so dropping `a` in front of it
    // MUST move the program to 1. Reading 2 afterwards would be `g`
    // silently computing from itself.
    let mut g = plain("g", ColumnType::Int64, true);
    g.generated = Some(GeneratedCol {
        expr: "b".into(),
        kind: GeneratedKind::Stored,
        program: ExprProgram::new(vec![Instr::PushCol(2)], vec![]).unwrap(),
    });
    let s = Schema::new(vec![TableDef {
        id: 0,
        name: "t".into(),
        columns: vec![
            plain("id", ColumnType::Int64, false),
            plain("a", ColumnType::Int64, true),
            plain("b", ColumnType::Int64, true),
            g,
        ],
        primary_key: vec![0],
        indexes: vec![],
        dead: false, pk_name: None,
        kind: TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    }])
    .unwrap();

    // A surviving generated column reads `b`: refused, and the message
    // names both sides so the caller knows what to drop first.
    let err = s.with_dropped_column(0, "b").unwrap_err();
    let err = format!("{err}");
    assert!(err.contains("`b`") && err.contains("`g`"), "{err}");

    // Dropping `a`, which nothing generated reads, is legal — and `g`'s
    // program follows its input down from ordinal 2 to 1.
    let after = s.with_dropped_column(0, "a").unwrap();
    let t = after.table(0).unwrap();
    assert_eq!(
        t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "b", "g"]
    );
    assert_eq!(
        t.columns[2].generated.as_ref().unwrap().program.instrs,
        vec![Instr::PushCol(1)]
    );
    // The primary key moved with it, and the result validates.
    assert_eq!(t.primary_key, vec![0]);

    // Dropping the ONLY generated column is the ordinary case — there is
    // no program left to renumber. This was refused table-wide before.
    let after = s.with_dropped_column(0, "g").unwrap();
    assert!(!after.table(0).unwrap().has_generated());

    // A rename with NO rewritten source is refused: the `AS (…)` text would
    // keep naming a column that no longer exists. Silence is not consent.
    let err = s.with_renamed_column(0, "b", "z", &[], &[]).unwrap_err();
    assert!(format!("{err}").contains("no rewritten expression"), "{err}");

    // Supplied, it is taken — and the ordinal must be the generated
    // column's own, not the renamed one's.
    let after = s.with_renamed_column(0, "b", "z", &[(3, "z".into())], &[]).unwrap();
    let t = after.table(0).unwrap();
    assert_eq!(t.columns[2].name, "z");
    assert_eq!(t.columns[3].generated.as_ref().unwrap().expr, "z");
    // …and the program is untouched: a rename moves no ordinals.
    assert_eq!(
        t.columns[3].generated.as_ref().unwrap().program.instrs,
        vec![Instr::PushCol(2)]
    );

    // An ordinal that is not a generated column is a caller bug, not a
    // silent no-op. (Ordinal 3 is supplied too, so this trips the entry
    // check and not the completeness one above it.)
    let err = s
        .with_renamed_column(0, "b", "z", &[(3, "z".into()), (1, "z".into())], &[])
        .unwrap_err();
    assert!(format!("{err}").contains("not generated"), "{err}");
}
