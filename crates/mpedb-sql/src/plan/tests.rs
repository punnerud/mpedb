use super::*;
use crate::planner::tests::test_schema;
use crate::prepare;

fn sample_sqls() -> Vec<&'static str> {
    vec![
        "SELECT * FROM users WHERE id = $1",
        "SELECT id, email, age + 1 FROM users WHERE age > 18 AND score < 2.5 ORDER BY email DESC LIMIT 10 OFFSET 5",
        "SELECT * FROM users WHERE id > 1 AND id <= $1",
        "SELECT * FROM users WHERE email = 'a@b' AND active",
        "SELECT * FROM orders WHERE user_id = 1 AND item_no = 2",
        "SELECT * FROM orders WHERE user_id = 7 AND note IS NOT NULL",
        "SELECT * FROM users WHERE email LIKE 'a%' OR NOT active",
        "INSERT INTO users (id, email) VALUES ($1, $2)",
        "INSERT INTO users (id, email, age) VALUES (1, 'a', NULL), (2, 'b', 3)",
        "INSERT INTO events (msg) VALUES (x'00ff')" ,
        "UPDATE users SET age = age + 1, score = 0.5 WHERE id = $1",
        "UPDATE users SET email = $1 WHERE email = $2",
        "DELETE FROM users WHERE id = 4",
        "DELETE FROM orders",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ]
}

#[test]
fn roundtrip_every_sample() {
    let s = test_schema();
    for sql in sample_sqls() {
        if sql.contains("x'00ff'") {
            continue; // blob into text column: bind error, skipped here
        }
        let p = prepare(sql, &s).unwrap();
        let bytes = p.encode();
        let q = CompiledPlan::decode(&bytes, &s).expect(sql);
        assert_eq!(p, q, "roundtrip mismatch for {sql}");
        assert_eq!(p.hash(), q.hash(), "hash instability for {sql}");
    }
}

#[test]
fn decode_rejects_wrong_schema() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id = 1", &s).unwrap();
    let bytes = p.encode();
    // A schema with one fewer table has a different hash.
    let other = Schema::new(vec![s.table(2).unwrap().clone()]).unwrap();
    assert!(matches!(
        CompiledPlan::decode(&bytes, &other),
        Err(Error::PlanInvalidated)
    ));
}

#[test]
fn decode_rejects_truncation_everywhere() {
    let s = test_schema();
    let p = prepare(
        "SELECT id, age + 1 FROM users WHERE id > $1 ORDER BY email LIMIT 3",
        &s,
    )
    .unwrap();
    let bytes = p.encode();
    for cut in 0..bytes.len() {
        assert!(
            CompiledPlan::decode(&bytes[..cut], &s).is_err(),
            "truncation at {cut} must fail"
        );
    }
}

#[test]
fn tampered_footprint_byte_is_rejected() {
    let s = test_schema();
    let p = prepare("SELECT * FROM users WHERE id = $1", &s).unwrap();
    let bytes = p.encode();
    // Footprint starts right after: format(1) + schema(32) + nparams(2)
    // + param tags(n) + context_keys count(2, none here)
    // + npolicies(2) + npolicies * (table 4 + epoch 8 + hash 32)
    // + nconsts(2) + consts.
    assert!(p.context_keys.is_empty());
    let mut off =
        1 + 32 + 2 + p.param_types.len() + 2 + 2 + p.policies.len() * (4 + 8 + 32) + 2;
    for c in &p.consts {
        let mut tmp = Vec::new();
        write_value(&mut tmp, c);
        off += tmp.len();
    }
    // Flip the low bit of tables_read: decode must catch the forgery.
    let mut evil = bytes.clone();
    evil[off] ^= 1;
    match CompiledPlan::decode(&evil, &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("footprint"), "{m}"),
        other => panic!("expected footprint corruption error, got {other:?}"),
    }
    // Flip read_only (offset +24): rejected as inconsistent.
    let mut evil = bytes.clone();
    evil[off + 24] ^= 1;
    assert!(CompiledPlan::decode(&evil, &s).is_err());
}

#[test]
fn tampered_semantics_are_rejected() {
    let s = test_schema();
    // Build a hand-corrupted plan: valid structure, PK-column SET.
    let p = prepare("UPDATE users SET age = 1 WHERE id = 1", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Update { set, .. } => set[0].0 = 0, // id is the PK
        _ => unreachable!(),
    }
    let bytes = evil.encode();
    assert!(CompiledPlan::decode(&bytes, &s).is_err());

    // Out-of-range table id.
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Update { table, .. } => *table = 63,
        _ => unreachable!(),
    }
    assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

    // PkPoint with the wrong arity.
    let p = prepare("SELECT * FROM orders WHERE user_id = 1 AND item_no = 2", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { access, .. } => {
            *access = AccessPath::PkPoint(vec![KeyPart::Const(0)]);
        }
        _ => unreachable!(),
    }
    assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

    // Const index out of range inside a key part.
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { access, .. } => {
            *access = AccessPath::PkPoint(vec![KeyPart::Const(60000), KeyPart::Const(1)]);
        }
        _ => unreachable!(),
    }
    assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

    // Param index beyond n_params inside a program.
    let p = prepare("SELECT * FROM users WHERE age > $1", &s).unwrap();
    let mut evil = p.clone();
    evil.param_types.clear(); // n_params -> 0 on re-encode
    assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());
}

/// A tampered blob whose sort key indexes past the tuple it claims to
/// order must be rejected at decode.
///
/// This is why `order_over` is a field rather than something inferred from
/// `distinct`/`aggregate`: the decoder has to know WHICH tuple to bound
/// against, and the failure is quiet if it guesses wrong — `cmp_rows` skips
/// a key it cannot fetch, so an out-of-range index does not crash, it drops
/// the sort and answers an ORDER BY query in arbitrary order.
#[test]
fn order_by_index_is_bounded_against_the_tuple_it_orders() {
    let s = test_schema();
    // `SELECT DISTINCT email` projects ONE column but the table has more,
    // so index 1 is in range for the base row and out of range for the
    // projection. Bounding against the wrong one accepts this.
    let p = prepare("SELECT DISTINCT email FROM users ORDER BY email", &s).unwrap();
    match &p.stmt {
        PlanStmt::Select {
            order_over,
            projection,
            ..
        } => {
            assert_eq!(*order_over, OrderOver::Projection);
            assert_eq!(projection.len(), 1);
        }
        _ => unreachable!(),
    }
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { order_by, .. } => order_by[0].0 = 1,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("order-by column"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // And a plain Select cannot claim to order a grouped tuple it has not
    // got.
    let p = prepare("SELECT id FROM users ORDER BY id", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { order_over, .. } => *order_over = OrderOver::Grouped,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("grouped"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

/// Sort-only columns are trimmed by the executor on the strength of a
/// COUNT in the plan. A tampered count is therefore a way to make the
/// executor trim real output, or to smuggle junk past a DISTINCT where it
/// would dedup on a value the caller never sees. Decode must refuse all
/// three shapes.
/// A tampered plan claiming "target (email), probe pk" would find a row by
/// PRIMARY KEY and update it as if it were the email conflict — the wrong
/// row, no error, no crash. Decode recomputes the probe from the target and
/// refuses a mismatch.
/// An aggregate over a join groups the JOINED row, so its GROUP BY slots
/// and aggregate arguments are bounded by the joined width — not the outer
/// table's, which is narrower and would reject a legitimate plan, and not
/// nothing, which would let a hostile one read past the tuple.
#[test]
fn aggregate_over_a_join_is_bounded_by_the_joined_width() {
    let s = test_schema();
    let p = prepare(
        "SELECT count(*) FROM orders JOIN users ON orders.user_id = users.id \
         GROUP BY users.email",
        &s,
    )
    .unwrap();
    let (outer_w, joined_w) = match &p.stmt {
        PlanStmt::Select {
            table,
            joins,
            aggregate: Some(a),
            ..
        } if !joins.is_empty() => {
            let j = &joins[0];
            let o = s.table(*table).unwrap().columns.len();
            let i = s.table(j.table).unwrap().columns.len();
            // `users.email` is column 1 of users, which sits after all of
            // orders' columns in the joined row — a slot no single-table
            // bound would accept.
            assert_eq!(a.group_by, vec![(o + 1) as u16]);
            (o, o + i)
        }
        other => panic!("expected a joined aggregate plan, got {other:?}"),
    };
    assert!(joined_w > outer_w, "the join must widen the row");
    // Round-trips.
    CompiledPlan::decode(&p.encode(), &s).unwrap();

    // One past the joined row is out of range.
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select {
            aggregate: Some(a), ..
        } => a.group_by[0] = joined_w as u16,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("GROUP BY column"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn conflict_probe_must_match_its_target() {
    let s = test_schema();
    let p = prepare(
        "INSERT INTO users (id, email) VALUES ($1, $2) \
         ON CONFLICT (email) DO UPDATE SET email = excluded.email",
        &s,
    )
    .unwrap();
    match &p.stmt {
        PlanStmt::Insert {
            on_conflict: PlanOnConflict::DoUpdate { probe, .. },
            ..
        } => assert!(
            matches!(probe, ConflictProbe::Index(_)),
            "email is a secondary unique column, got {probe:?}"
        ),
        other => panic!("expected an upsert plan, got {other:?}"),
    }

    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Insert {
            on_conflict: PlanOnConflict::DoUpdate { probe, .. },
            ..
        } => *probe = ConflictProbe::Pk,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("probe"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // And the reverse: a PK target cannot claim an index probe.
    let p = prepare(
        "INSERT INTO users (id, email) VALUES ($1, $2) \
         ON CONFLICT (id) DO UPDATE SET email = excluded.email",
        &s,
    )
    .unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Insert {
            on_conflict: PlanOnConflict::DoUpdate { probe, .. },
            ..
        } => *probe = ConflictProbe::Index(1),
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("probe"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn order_junk_count_is_validated() {
    let s = test_schema();
    let p = prepare("SELECT id FROM users ORDER BY email", &s).unwrap();
    match &p.stmt {
        PlanStmt::Select {
            order_junk,
            order_over,
            projection,
            ..
        } => {
            // The key is a plain column, so it sorts the base row and needs
            // no junk column at all.
            assert_eq!(*order_junk, 0);
            assert_eq!(*order_over, OrderOver::BaseRow);
            assert_eq!(projection.len(), 1);
        }
        _ => unreachable!(),
    }

    // (a) junk without a projection sort: nothing would ever trim it.
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { order_junk, .. } => *order_junk = 1,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("projection sort"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // (b) junk that eats the entire output.
    let p2 = prepare("SELECT id FROM users ORDER BY email + 1", &s);
    // `email` is text; if that does not bind, use a numeric key instead.
    let p2 = match p2 {
        Ok(p) => p,
        Err(_) => prepare("SELECT email FROM users ORDER BY id + 1", &s).unwrap(),
    };
    match &p2.stmt {
        PlanStmt::Select {
            order_junk,
            order_over,
            projection,
            ..
        } => {
            assert_eq!(*order_junk, 1, "a computed key needs a sort-only column");
            assert_eq!(*order_over, OrderOver::Projection);
            assert_eq!(projection.len(), 2, "one output + one sort-only");
        }
        _ => unreachable!(),
    }
    let mut evil = p2.clone();
    match &mut evil.stmt {
        PlanStmt::Select { order_junk, .. } => *order_junk = 2,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("no output"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // (c) junk under DISTINCT.
    let mut evil = p2.clone();
    match &mut evil.stmt {
        PlanStmt::Select { distinct, .. } => *distinct = true,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("DISTINCT"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn oversized_counts_in_plan_bytes_are_rejected() {
    let s = test_schema();

    // The parse-time caps make prepare() refuse oversized statements, so
    // hand-build oversized plans in memory: their encodings are exactly
    // what a tampered registry blob would look like, and decode must
    // reject the count before trusting it.
    let p = prepare("SELECT id FROM users", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { projection, .. } => {
            let item = projection[0].clone();
            while projection.len() <= crate::parser::MAX_SELECT_ITEMS {
                projection.push(item.clone());
            }
        }
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("projection items"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    let p = prepare("SELECT id FROM users ORDER BY email", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select { order_by, .. } => {
            assert!(!order_by.is_empty());
            let item = order_by[0];
            while order_by.len() <= crate::parser::MAX_ORDER_BY_ITEMS {
                order_by.push(item);
            }
        }
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("order-by"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    let p = prepare("UPDATE users SET age = 1 WHERE id = 1", &s).unwrap();
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Update { set, .. } => {
            let item = set[0].clone();
            while set.len() <= crate::parser::MAX_SET_ITEMS {
                set.push(item.clone());
            }
        }
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("SET assignments"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn explain_is_informative() {
    let s = test_schema();
    let p = prepare(
        "SELECT id, age + 1 FROM users WHERE id = $1 AND age > 18 LIMIT 3",
        &s,
    )
    .unwrap();
    let e = p.explain(&s);
    assert!(e.contains("Select users"), "{e}");
    assert!(e.contains("PkPoint(id = $1)"), "{e}");
    assert!(e.contains("filter: age > 18"), "{e}");
    assert!(e.contains("project: id, age + 1"), "{e}");
    assert!(e.contains("limit: 3"), "{e}");
    assert!(e.contains("read_only=true"), "{e}");

    let p = prepare("SELECT * FROM users WHERE id > 1 AND id <= $1", &s).unwrap();
    let e = p.explain(&s);
    assert!(e.contains("PkRange(id > 1, id <= $1)"), "{e}");

    let p = prepare("SELECT * FROM users WHERE email = 'x'", &s).unwrap();
    let e = p.explain(&s);
    assert!(e.contains("IndexPoint(email = 'x') via index 1"), "{e}");

    let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a')", &s).unwrap();
    let e = p.explain(&s);
    assert!(e.contains("Insert users"), "{e}");
    assert!(e.contains("id = 1"), "{e}");
    assert!(e.contains("email = 'a'"), "{e}");
    assert!(e.contains("created = DEFAULT"), "{e}");

    let p = prepare("UPDATE users SET age = age + 1 WHERE id = 2", &s).unwrap();
    let e = p.explain(&s);
    assert!(e.contains("Update users"), "{e}");
    assert!(e.contains("set: age = age + 1"), "{e}");

    let p = prepare("DELETE FROM users", &s).unwrap();
    let e = p.explain(&s);
    assert!(e.contains("Delete users"), "{e}");
    assert!(e.contains("FullScan"), "{e}");

    assert!(prepare("BEGIN", &s).unwrap().explain(&s).contains("Begin"));
}

#[test]
fn projection_names_are_canonical() {
    let s = test_schema();
    let a = prepare("SELECT age+1 FROM users", &s).unwrap();
    // Identifiers are case-sensitive: AGE does not exist.
    assert!(matches!(
        prepare("select AGE + 1 from users", &s),
        Err(Error::Bind(_))
    ));
    // Whitespace and keyword case do not affect the plan or the name.
    let b = prepare("select age\n  +\n1 from users", &s).unwrap();
    assert_eq!(a, b);
    match &a.stmt {
        PlanStmt::Select { projection, .. } => match &projection[0] {
            Projection::Expr { name, .. } => assert_eq!(name, "age + 1"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
