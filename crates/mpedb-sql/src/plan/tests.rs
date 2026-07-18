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
        // INSERT OR REPLACE (format 33): the OC_REPLACE tag must survive
        // encode/decode/validate/hash and the truncation/bit-flip fuzz.
        "INSERT OR REPLACE INTO users (id, email) VALUES ($1, $2)",
        "INSERT INTO users (id, email, age) VALUES (1, 'a', NULL), (2, 'b', 3)",
        "INSERT INTO events (msg) VALUES (x'00ff')" ,
        "UPDATE users SET age = age + 1, score = 0.5 WHERE id = $1",
        "UPDATE users SET email = $1 WHERE email = $2",
        "DELETE FROM users WHERE id = 4",
        "DELETE FROM orders",
        "SELECT id FROM users UNION SELECT user_id FROM orders ORDER BY 1 LIMIT 5",
        "SELECT id FROM users UNION ALL SELECT id FROM users",
        "SELECT users.id FROM users FULL OUTER JOIN orders ON users.id = orders.user_id",
        "SELECT users.id FROM users RIGHT JOIN orders ON users.id = orders.user_id",
        "SELECT id FROM users EXCEPT SELECT user_id FROM orders",
        "SELECT id FROM users INTERSECT SELECT user_id FROM orders OFFSET 1",
        // FROM-less (#67, format 10): the DUAL sentinel must survive
        // encode/decode/validate — alone, in compound arms, with WHERE and
        // aggregates, and as a subquery on either side.
        "SELECT 3+5",
        "SELECT 3, 'x', 1.5 WHERE 1=1",
        "SELECT count(*)",
        "SELECT 1 UNION SELECT 2",
        "SELECT 15 UNION SELECT id FROM users",
        "SELECT (SELECT 3)",
        // IN (SELECT ...) (#70, format 11): LIST-kind subplan + InParam.
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders)",
        "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders) AND active",
        "SELECT id FROM users WHERE id = (SELECT 4)",
        // Nested subqueries (#73 §3 stage 1, format 20): a subplan carries its
        // OWN uncorrelated lifts — IN-inside-IN, EXISTS-inside-EXISTS, and a
        // scalar whose body holds another scalar — must all survive the recursive
        // encode/decode/validate.
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE item_no IN (SELECT age FROM users))",
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE EXISTS (SELECT 1 FROM events))",
        "SELECT id FROM users WHERE age = (SELECT max(age) FROM users WHERE age < (SELECT max(item_no) FROM orders))",
        // Nested subqueries CORRELATED to their immediate parent (#73 §3 stage 2,
        // same format 20): a nested lift now carries `outer_args` that index into
        // its parent subplan's row and a parent `post_filter` — both must survive
        // the recursive encode/decode/validate. EXISTS-in-EXISTS and a scalar
        // whose correlated body holds another correlated EXISTS.
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id \
         AND EXISTS (SELECT 1 FROM events WHERE events.msg = orders.sku))",
        "SELECT id FROM users WHERE age = (SELECT max(item_no) FROM orders WHERE orders.user_id = users.id \
         AND EXISTS (SELECT 1 FROM events WHERE events.msg = orders.sku))",
        // Window functions (design/DESIGN-WINDOW.md stage 1, format 24): the trailing
        // window list on a Select must survive encode/decode/validate — ranking,
        // aggregate OVER, multiple windows, and a window in ORDER BY (junk).
        "SELECT id, row_number() OVER (PARTITION BY active ORDER BY age) FROM users",
        "SELECT id, rank() OVER (ORDER BY age DESC), dense_rank() OVER (ORDER BY age DESC) FROM users",
        "SELECT id, sum(age) OVER (PARTITION BY active ORDER BY age), count(*) OVER (PARTITION BY active) FROM users ORDER BY id",
        "SELECT id FROM users ORDER BY dense_rank() OVER (ORDER BY age), id",
        // Recursive CTEs (design/DESIGN-CTE-RECURSIVE.md stage 1, format 26): the
        // new `RecursiveCte` node — name, columns+types, union_all byte and three
        // nested SelectPlans — must survive encode/decode/validate. A DUAL-anchor
        // counting generator (UNION ALL, outer LIMIT) and a transitive closure
        // whose recursive term JOINs the working table (UNION dedup).
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c LIMIT 10",
        "WITH RECURSIVE r(n) AS (SELECT id FROM users UNION \
         SELECT orders.user_id FROM orders JOIN r ON orders.user_id = r.n) SELECT n FROM r ORDER BY n",
        // COLLATE (format 28): the additive collated-compare / collated-IN
        // opcodes and the per-ORDER-BY collation byte must survive
        // encode/decode/validate/hash — on a comparison, an IN list, and both a
        // single-table and a compound ORDER BY.
        "SELECT id FROM users WHERE email = 'x' COLLATE NOCASE",
        "SELECT id FROM users WHERE email COLLATE RTRIM = $1",
        "SELECT id FROM users WHERE email COLLATE NOCASE IN ('a', 'b')",
        "SELECT id, email FROM users ORDER BY email COLLATE NOCASE, id",
        "SELECT id, email FROM users ORDER BY email COLLATE RTRIM DESC, id",
        "SELECT id FROM users UNION SELECT user_id FROM orders ORDER BY 1 COLLATE NOCASE",
        // CAST affinity (format 29): the `Instr::Cast` payload byte is now an
        // Affinity (1..=5). Each of the five affinities must survive
        // encode/decode/validate/hash — reached via known and unknown type names.
        "SELECT CAST(age AS SIGNED), CAST(score AS INTEGER), CAST(email AS REAL) FROM users",
        "SELECT CAST(email AS BLOB), CAST(age AS VARCHAR(10)) FROM users",
        // Compound bodies in a lifted subquery (#56/format 31): a scalar, an
        // `IN`, and an `EXISTS` whose body is a whole UNION/INTERSECT/EXCEPT
        // compound — the subplan now carries a body-discriminant byte + a
        // `CompoundPlan`, which must survive encode/decode/validate.
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders UNION SELECT age FROM users)",
        "SELECT id FROM users WHERE age IN (SELECT item_no FROM orders INTERSECT SELECT age FROM users) AND active",
        "SELECT id FROM users WHERE age NOT IN (SELECT item_no FROM orders EXCEPT SELECT age FROM users)",
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders UNION ALL SELECT 2)",
        "SELECT (SELECT 1 UNION SELECT 2 LIMIT 1)",
        // A compound body NESTED inside a plain-select subquery: the outer IN's
        // Select body carries an inner IN whose body is a whole compound — the
        // compound rides as an (uncorrelated) child subplan of the middle.
        "SELECT id FROM users WHERE id IN \
         (SELECT user_id FROM orders WHERE item_no IN (SELECT age FROM users UNION SELECT id FROM users))",
        "SELECT id FROM users WHERE id = (SELECT user_id FROM orders UNION SELECT 4 LIMIT 1)",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        // Savepoint control statements carry a name in the plan bytes, so they
        // must survive encode/decode/hash AND the decoder's truncation/bit-flip
        // fuzz (a corrupt length or non-utf8 name must Err, never panic).
        "SAVEPOINT my_point",
        "RELEASE my_point",
        "ROLLBACK TO my_point",
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

/// The COLLATE bytes (format 28) get their own truncation sweep: a collated
/// plan carries the additive compare/IN opcodes (op tag + kind + collation
/// byte) and a per-ORDER-BY collation byte, so every cut through them must Err
/// and never panic. Also asserts the collated plan round-trips and EXPLAIN does
/// not panic on the new instructions.
#[test]
fn decode_rejects_truncation_in_collate() {
    let s = test_schema();
    for sql in [
        "SELECT id FROM users WHERE email = 'x' COLLATE NOCASE",
        "SELECT id FROM users WHERE email COLLATE NOCASE IN ('a', 'b')",
        "SELECT id, email FROM users ORDER BY email COLLATE RTRIM, id",
    ] {
        let p = prepare(sql, &s).unwrap();
        let _ = p.explain(&s); // must not panic on CmpColl/InListColl/order-by
        let bytes = p.encode();
        let q = CompiledPlan::decode(&bytes, &s).expect(sql);
        assert_eq!(p, q, "roundtrip mismatch for {sql}");
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation at {cut} must fail for {sql}"
            );
        }
    }
}

/// The CAST affinity byte (format 29) gets its own sweep: a plan carrying each
/// affinity opcode must round-trip, and every truncation through it must Err
/// (never panic). It also guards the version bump itself — a plan blob whose
/// leading format byte is set back to 28 must fail CLOSED with `PlanInvalidated`
/// (re-prepare), never be misread as a valid plan under the new opcode meaning.
#[test]
fn decode_rejects_truncation_and_stale_format_in_cast() {
    let s = test_schema();
    for sql in [
        "SELECT CAST(age AS SIGNED) FROM users",
        "SELECT CAST(email AS DECIMAL), CAST(score AS BLOB) FROM users",
        "SELECT CAST(age AS TEXT), CAST(email AS INTEGER), CAST(score AS REAL) FROM users",
    ] {
        let p = prepare(sql, &s).unwrap();
        let _ = p.explain(&s); // must not panic rendering the affinity name
        let bytes = p.encode();
        assert_eq!(bytes[0], 33, "plan format byte for {sql}");
        let q = CompiledPlan::decode(&bytes, &s).expect(sql);
        assert_eq!(p, q, "roundtrip mismatch for {sql}");
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation at {cut} must fail for {sql}"
            );
        }
        // Stale format: an old (28) reader-era blob is re-prepared, not misread.
        let mut stale = bytes.clone();
        stale[0] = 28;
        assert!(
            matches!(CompiledPlan::decode(&stale, &s), Err(Error::PlanInvalidated)),
            "a format-28 CAST plan must be PlanInvalidated, not misread, for {sql}"
        );
    }
}

/// sqlite "bare columns" (format 30): a grouped SELECT whose projection carries
/// a bare column fixed by a single min/max must round-trip through the wire, hold
/// the `bare_cols` list, survive truncation at every offset, and re-prepare
/// (never be misread) when the format byte is set back to 29.
#[test]
fn bare_group_by_roundtrips_and_rejects_truncation_and_stale_format() {
    let s = test_schema();
    // `email` and `id` are bare; `max(age)` is the single extremum that fixes
    // them. `prepare` defaults to sqlite-lenient mode, so this compiles.
    for sql in [
        "SELECT email, max(age) FROM users GROUP BY active",
        "SELECT id, email, min(age) FROM users GROUP BY active",
        "SELECT email, max(age) FROM users",
    ] {
        let p = prepare(sql, &s).unwrap();
        // The plan must actually carry the bare columns (else exec has nothing
        // to fill the projection's grouped-tuple slots from).
        let PlanStmt::Select(sp) = &p.stmt else { panic!("expected select for {sql}") };
        let agg = sp.aggregate.as_ref().expect("aggregate");
        assert!(!agg.bare_cols.is_empty(), "bare_cols must be populated for {sql}");

        let bytes = p.encode();
        assert_eq!(bytes[0], 33, "plan format byte for {sql}");
        let q = CompiledPlan::decode(&bytes, &s).expect(sql);
        assert_eq!(p, q, "roundtrip mismatch for {sql}");
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation at {cut} must fail for {sql}"
            );
        }
        let mut stale = bytes.clone();
        stale[0] = 29;
        assert!(
            matches!(CompiledPlan::decode(&stale, &s), Err(Error::PlanInvalidated)),
            "a format-29 bare-column plan must be PlanInvalidated, not misread, for {sql}"
        );
    }
}

/// A bad collation tag byte in an ORDER-BY key is rejected as Corrupt, never
/// decoded into a phantom collation.
#[test]
fn bad_order_by_collation_tag_is_rejected() {
    let s = test_schema();
    let p = prepare("SELECT id, email FROM users ORDER BY email COLLATE NOCASE, id", &s).unwrap();
    let bytes = p.encode();
    // Find the NOCASE (1) collation byte the encoder wrote for the first key and
    // corrupt it to an out-of-range tag; decode must reject it.
    let mut tampered = None;
    for i in 0..bytes.len() {
        if bytes[i] == 1 {
            let mut b = bytes.clone();
            b[i] = 0x7f; // no such collation tag
            if matches!(CompiledPlan::decode(&b, &s), Err(Error::Corrupt(_))) {
                tampered = Some(());
                break;
            }
        }
    }
    assert!(
        tampered.is_some(),
        "a corrupt collation tag somewhere in the plan must be rejected as Corrupt"
    );
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

/// The window list bytes (format 24) get their own truncation sweep: a windowed
/// plan carries a window count, per-window func/distinct bytes, an optional arg
/// program, and PARTITION BY / ORDER BY program lists — every cut through them
/// must fail closed rather than decode a half-read window.
#[test]
fn decode_rejects_truncation_in_windows() {
    let s = test_schema();
    let p = prepare(
        "SELECT id, row_number() OVER (PARTITION BY active ORDER BY age), \
         sum(age) OVER (PARTITION BY active ORDER BY age) FROM users \
         ORDER BY rank() OVER (ORDER BY age DESC), id",
        &s,
    )
    .unwrap();
    // Sanity: this really carries a window list.
    match &p.stmt {
        PlanStmt::Select(sp) => assert!(!sp.windows.is_empty(), "expected windows"),
        other => panic!("expected a Select, got {other:?}"),
    }
    // EXPLAIN renders the window phase (and does not panic on it).
    let ex = p.explain(&s);
    assert!(ex.contains("window __w"), "EXPLAIN should show the windows:\n{ex}");
    let bytes = p.encode();
    for cut in 0..bytes.len() {
        assert!(
            CompiledPlan::decode(&bytes[..cut], &s).is_err(),
            "truncation at {cut} must fail"
        );
    }
    // The full blob round-trips (and re-validates the window programs).
    assert_eq!(CompiledPlan::decode(&bytes, &s).unwrap(), p);
}

/// The recursive SubPlan bytes (#73 §3, format 20) get their own truncation
/// sweep: a nested-subplan plan carries `sub_base`/`slot_type`/child-count bytes
/// and a whole inner SubPlan record, and every cut through them must fail closed
/// rather than decode a half-read tree.
#[test]
fn decode_rejects_truncation_in_nested_subplan() {
    let s = test_schema();
    let p = prepare(
        "SELECT id FROM users WHERE age = \
         (SELECT max(age) FROM users WHERE age < (SELECT max(item_no) FROM orders))",
        &s,
    )
    .unwrap();
    // Sanity: this really is a nested tree (a subplan with a child subplan).
    assert_eq!(p.subplans.len(), 1);
    assert_eq!(p.subplans[0].subplans.len(), 1);
    let bytes = p.encode();
    for cut in 0..bytes.len() {
        assert!(
            CompiledPlan::decode(&bytes[..cut], &s).is_err(),
            "truncation at {cut} must fail"
        );
    }
    // The full blob round-trips (and re-validates the whole tree).
    assert_eq!(CompiledPlan::decode(&bytes, &s).unwrap(), p);
}

/// The compound-subquery-body bytes (#56/format 31) get their own sweep: a
/// subplan whose body is a whole compound carries a body-discriminant byte + a
/// `CompoundPlan` (arm count, ops, per-arm SELECTs, ORDER BY/LIMIT), and every
/// cut through them must fail closed rather than decode a half-read compound.
/// EXPLAIN renders the compound body without panicking, and a format-30 blob is
/// re-prepared (never misread) — the whole-plan version gate.
#[test]
fn compound_subplan_roundtrips_rejects_truncation_and_stale_format() {
    let s = test_schema();
    for sql in [
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders UNION SELECT age FROM users)",
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders INTERSECT SELECT 1 FROM events)",
        "SELECT (SELECT 1 UNION SELECT 2 LIMIT 1)",
    ] {
        let p = prepare(sql, &s).unwrap();
        // Sanity: the subplan really carries a COMPOUND body.
        assert!(
            p.subplans
                .iter()
                .any(|sp| matches!(sp.body, SubBody::Compound(_))),
            "expected a compound subplan body for {sql}"
        );
        let _ = p.explain(&s); // must not panic on the compound body render
        let bytes = p.encode();
        assert_eq!(bytes[0], 33, "plan format byte for {sql}");
        assert_eq!(CompiledPlan::decode(&bytes, &s).unwrap(), p, "roundtrip for {sql}");
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation at {cut} must fail for {sql}"
            );
        }
        let mut stale = bytes.clone();
        stale[0] = 30;
        assert!(
            matches!(CompiledPlan::decode(&stale, &s), Err(Error::PlanInvalidated)),
            "a format-30 compound-subplan plan must be PlanInvalidated, not misread, for {sql}"
        );
    }
}

/// The recursive-CTE bytes (format 26) get their own truncation sweep: a
/// `RecursiveCte` plan carries a name, a columns+types list, a `union_all` byte
/// and THREE nested SelectPlans (anchor / recursive / outer), and every cut
/// through them must fail closed rather than decode a half-read node. EXPLAIN
/// renders the node without panicking.
#[test]
fn decode_rejects_truncation_in_recursive_cte() {
    let s = test_schema();
    let p = prepare(
        "WITH RECURSIVE r(n) AS (SELECT id FROM users UNION \
         SELECT orders.user_id FROM orders JOIN r ON orders.user_id = r.n) \
         SELECT n FROM r WHERE n > 0 ORDER BY n LIMIT 5",
        &s,
    )
    .unwrap();
    // Sanity: this really is a recursive-CTE node whose recursive term joins the
    // working table.
    match &p.stmt {
        PlanStmt::RecursiveCte(rc) => {
            assert_eq!(rc.columns.len(), 1);
            assert!(!rc.union_all);
            assert_eq!(rc.recursive.joins.len(), 1);
        }
        other => panic!("expected a RecursiveCte, got {other:?}"),
    }
    // EXPLAIN renders the node (and does not panic naming the working table).
    let ex = p.explain(&s);
    assert!(ex.contains("RecursiveCte r(n)"), "EXPLAIN should show the node:\n{ex}");
    let bytes = p.encode();
    for cut in 0..bytes.len() {
        assert!(
            CompiledPlan::decode(&bytes[..cut], &s).is_err(),
            "truncation at {cut} must fail"
        );
    }
    // The full blob round-trips (and re-validates all three components + §3).
    assert_eq!(CompiledPlan::decode(&bytes, &s).unwrap(), p);
}

/// The same truncation sweep for a CORRELATED nested tree (#73 §3 stage 2): the
/// child carries `outer_args` bytes and the middle a `post_filter`, so a cut
/// through any of them must fail closed rather than decode a half-read tree.
#[test]
fn decode_rejects_truncation_in_correlated_nested_subplan() {
    let s = test_schema();
    let p = prepare(
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id \
         AND EXISTS (SELECT 1 FROM events WHERE events.msg = orders.sku))",
        &s,
    )
    .unwrap();
    // Sanity: a correlated child (one outer_arg) under a middle with a post_filter.
    assert_eq!(p.subplans.len(), 1);
    assert_eq!(p.subplans[0].subplans.len(), 1);
    assert_eq!(p.subplans[0].subplans[0].outer_args.len(), 1);
    assert!(p.subplans[0].body.as_select().unwrap().post_filter.is_some());
    let bytes = p.encode();
    for cut in 0..bytes.len() {
        assert!(
            CompiledPlan::decode(&bytes[..cut], &s).is_err(),
            "truncation at {cut} must fail"
        );
    }
    assert_eq!(CompiledPlan::decode(&bytes, &s).unwrap(), p);
}

/// A nested subquery CORRELATED to its immediate parent (#73 §3 stage 2) is a
/// legal plan and must round-trip through encode/decode/validate. The forged
/// shapes it is NOT allowed to take are still rejected: a correlation arg that
/// does not match `sub_base`, or one that points past the parent's row.
#[test]
fn correlated_nested_subplan_round_trips_and_bounds() {
    let s = test_schema();
    let p = prepare(
        "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE orders.user_id = users.id \
         AND EXISTS (SELECT 1 FROM events WHERE events.msg = orders.sku))",
        &s,
    )
    .unwrap();
    // The grandchild really correlates to its immediate parent (orders): one
    // outer_arg, and the middle plan carries a post_filter for the EXISTS.
    assert_eq!(p.subplans.len(), 1);
    assert_eq!(p.subplans[0].subplans.len(), 1);
    assert_eq!(p.subplans[0].subplans[0].outer_args.len(), 1);
    assert!(
        p.subplans[0].body.as_select().unwrap().post_filter.is_some(),
        "correlated child ⇒ parent post_filter"
    );
    // Legal blob round-trips (and re-validates the whole correlated tree).
    assert_eq!(CompiledPlan::decode(&p.encode(), &s).unwrap(), p);

    // Forge the grandchild's correlation arg to look correlated WITHOUT moving
    // its sub_base: the executor would fill children into the wrong slots, so
    // decode refuses the inconsistency.
    let mut evil = p.clone();
    evil.subplans[0].subplans[0].outer_args.push(0);
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("sub_base"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // Forge the correlation arg to point past the parent (orders has 4 columns):
    // an outer_arg out of the parent row is corrupt.
    let mut evil = p.clone();
    evil.subplans[0].subplans[0].outer_args = vec![99];
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(
            m.contains("out of the outer row") || m.contains("sub_base"),
            "{m}"
        ),
        other => panic!("expected Corrupt, got {other:?}"),
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
    // + nconsts(2) + consts + subplan count(1, none here).
    assert!(p.context_keys.is_empty());
    assert!(p.subplans.is_empty());
    let mut off =
        1 + 32 + 2 + p.param_types.len() + 2 + 2 + p.policies.len() * (4 + 8 + 32) + 2;
    for c in &p.consts {
        let mut tmp = Vec::new();
        write_value(&mut tmp, c);
        off += tmp.len();
    }
    off += 1; // subplan count byte
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
        PlanStmt::Select(SelectPlan { access, .. }) => {
            *access = AccessPath::PkPoint(vec![KeyPart::Const(0)]);
        }
        _ => unreachable!(),
    }
    assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

    // Const index out of range inside a key part.
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select(SelectPlan { access, .. }) => {
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
        PlanStmt::Select(SelectPlan {
            order_over,
            projection,
            ..
        }) => {
            assert_eq!(*order_over, OrderOver::Projection);
            assert_eq!(projection.len(), 1);
        }
        _ => unreachable!(),
    }
    let mut evil = p.clone();
    match &mut evil.stmt {
        PlanStmt::Select(SelectPlan { order_by, .. }) => order_by[0].0 = 1,
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
        PlanStmt::Select(SelectPlan { order_over, .. }) => *order_over = OrderOver::Grouped,
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
        PlanStmt::Select(SelectPlan {
            table,
            joins,
            aggregate: Some(a),
            ..
        }) if !joins.is_empty() => {
            let j = &joins[0];
            let o = s.table(*table).unwrap().columns.len();
            let i = s.table(j.table).unwrap().columns.len();
            // `users.email` is column 1 of users, which sits after all of
            // orders' columns in the joined row — a slot no single-table
            // bound would accept.
            assert_eq!(a.group_by, vec![GroupKey::Col((o + 1) as u16)]);
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
        PlanStmt::Select(SelectPlan {
            aggregate: Some(a), ..
        }) => a.group_by[0] = GroupKey::Col(joined_w as u16),
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
        PlanStmt::Select(SelectPlan {
            order_junk,
            order_over,
            projection,
            ..
        }) => {
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
        PlanStmt::Select(SelectPlan { order_junk, .. }) => *order_junk = 1,
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
        PlanStmt::Select(SelectPlan {
            order_junk,
            order_over,
            projection,
            ..
        }) => {
            assert_eq!(*order_junk, 1, "a computed key needs a sort-only column");
            assert_eq!(*order_over, OrderOver::Projection);
            assert_eq!(projection.len(), 2, "one output + one sort-only");
        }
        _ => unreachable!(),
    }
    let mut evil = p2.clone();
    match &mut evil.stmt {
        PlanStmt::Select(SelectPlan { order_junk, .. }) => *order_junk = 2,
        _ => unreachable!(),
    }
    match CompiledPlan::decode(&evil.encode(), &s) {
        Err(Error::Corrupt(m)) => assert!(m.contains("no output"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }

    // (c) junk under DISTINCT.
    let mut evil = p2.clone();
    match &mut evil.stmt {
        PlanStmt::Select(SelectPlan { distinct, .. }) => *distinct = true,
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
        PlanStmt::Select(SelectPlan { projection, .. }) => {
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
        PlanStmt::Select(SelectPlan { order_by, .. }) => {
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
        PlanStmt::Select(SelectPlan { projection, .. }) => match &projection[0] {
            Projection::Expr { name, .. } => assert_eq!(name, "age + 1"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
