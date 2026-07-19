//! Parser unit tests, split out of [`super`] to keep that file under the size
//! limit. They exercise the grammar only through the public `parse_*` entry
//! points, so they live together regardless of which submodule now holds a
//! given production.

use super::{parse_expr_only, parse_statement, MAX_ORDER_BY_ITEMS, MAX_SELECT_ITEMS, MAX_SET_ITEMS};
use crate::ast::{BinOp, DeleteStmt, Expr, InsertStmt, SelectStmt, Stmt, UnOp};
use crate::plan::SetOp;
use mpedb_types::{Error, Value};

fn expr(src: &str) -> Expr {
    parse_expr_only(src).unwrap().0
}

/// Unary `+` is the identity and parses to NOTHING — `+x` is `x`, and the
/// sign chains the sqllogictest corpus is full of (`- + 43`, `+ ( - 78 )`)
/// reduce to the plain negation they mean.
#[test]
fn unary_plus_is_identity() {
    assert_eq!(expr("+ 43"), expr("43"));
    assert_eq!(expr("+ a"), expr("a"));
    assert_eq!(expr("- + 43"), expr("- 43"));
    assert_eq!(expr("+ ( - 78 )"), expr("(- 78)"));
    assert_eq!(expr("a + + b"), expr("a + b"));
}

/// `CAST(x AS type)` parses to its own node; `CAST` stays usable as an
/// ordinary identifier when not followed by `(`.
#[test]
fn cast_parses_and_concat_sits_in_the_additive_tier() {
    // The parser keeps the type name VERBATIM (any identifier is accepted; the
    // binder folds it to an affinity). Multi-word names join with a space; a
    // parenthesized size is dropped.
    assert_eq!(
        expr("CAST(a AS INTEGER)"),
        Expr::Cast(Box::new(Expr::Col("a".into())), "INTEGER".into())
    );
    assert_eq!(
        expr("cast(NULL as real)"),
        Expr::Cast(Box::new(Expr::Lit(Value::Null)), "real".into())
    );
    // An unknown name no longer errors — it is a valid type name to the parser.
    assert_eq!(
        expr("CAST(a AS lolwut)"),
        Expr::Cast(Box::new(Expr::Col("a".into())), "lolwut".into())
    );
    assert_eq!(
        expr("CAST(a AS VARCHAR(10))"),
        Expr::Cast(Box::new(Expr::Col("a".into())), "VARCHAR".into())
    );
    assert_eq!(
        expr("CAST(a AS DOUBLE PRECISION)"),
        Expr::Cast(Box::new(Expr::Col("a".into())), "DOUBLE PRECISION".into())
    );
    // bare `cast` is still a column name
    assert_eq!(expr("cast"), Expr::Col("cast".into()));

    // `a || b || c` is left-associative and binds like +/-
    assert_eq!(
        expr("a || b || c"),
        Expr::Binary(
            BinOp::Concat,
            Box::new(Expr::Binary(
                BinOp::Concat,
                Box::new(Expr::Col("a".into())),
                Box::new(Expr::Col("b".into()))
            )),
            Box::new(Expr::Col("c".into()))
        )
    );
    // lone `|` is a clear parse error, not a mystery token
    assert!(parse_expr_only("a | b").is_err());
}

/// A compound chain parses left-associatively, hoists the trailing
/// ORDER BY/LIMIT to the compound, and rejects them mid-chain.
#[test]
fn compound_selects_parse() {
    let stmt = |src: &str| parse_statement(src).unwrap().0;
    let Stmt::Compound(c) =
        stmt("SELECT a FROM t UNION ALL SELECT b FROM u UNION SELECT c FROM v ORDER BY 1 LIMIT 3")
    else {
        panic!("expected a compound");
    };
    assert_eq!(c.arms.len(), 3);
    assert_eq!(c.ops, vec![SetOp::UnionAll, SetOp::Union]);
    // hoisted off the last arm
    assert_eq!(c.order_by.len(), 1);
    assert_eq!(c.limit, Some(3));
    assert!(c.arms.iter().all(|a| a.order_by.is_empty() && a.limit.is_none()));

    let Stmt::Compound(c) = stmt("SELECT a FROM t EXCEPT SELECT a FROM u") else {
        panic!("expected a compound");
    };
    assert_eq!(c.ops, vec![SetOp::Except]);
    let Stmt::Compound(c) = stmt("SELECT a FROM t INTERSECT SELECT a FROM u") else {
        panic!("expected a compound");
    };
    assert_eq!(c.ops, vec![SetOp::Intersect]);

    // ORDER BY mid-chain is an error, not a silent per-arm sort.
    assert!(parse_statement("SELECT a FROM t ORDER BY a UNION SELECT b FROM u").is_err());
    // `union` is not eaten as a table alias.
    assert!(matches!(
        stmt("SELECT a FROM t UNION SELECT b FROM u"),
        Stmt::Compound(_)
    ));
    // CROSS JOIN desugars like the comma-join.
    let Stmt::Select(s) = stmt("SELECT a FROM t CROSS JOIN u") else {
        panic!("expected a select");
    };
    assert_eq!(s.joins.len(), 1);
    assert_eq!(s.joins[0].on, Expr::Lit(Value::Bool(true)));
}

fn col(name: &str) -> Box<Expr> {
    Box::new(Expr::Col(name.into()))
}

fn int(v: i64) -> Box<Expr> {
    Box::new(Expr::Lit(Value::Int(v)))
}

#[test]
fn or_binds_looser_than_and() {
    // a = 1 OR b = 2 AND c = 3  ==  a=1 OR (b=2 AND c=3)
    let e = expr("a = 1 OR b = 2 AND c = 3");
    let eq = |c: &str, v: i64| Box::new(Expr::Binary(BinOp::Eq, col(c), int(v)));
    assert_eq!(
        e,
        Expr::Binary(
            BinOp::Or,
            eq("a", 1),
            Box::new(Expr::Binary(BinOp::And, eq("b", 2), eq("c", 3)))
        )
    );
}

#[test]
fn not_binds_looser_than_comparison() {
    // NOT a = 1  ==  NOT (a = 1)
    let e = expr("NOT a = 1");
    assert_eq!(
        e,
        Expr::Unary(
            UnOp::Not,
            Box::new(Expr::Binary(BinOp::Eq, col("a"), int(1)))
        )
    );
}

/// BETWEEN's own AND must not be eaten by boolean AND: parsing the upper
/// bound with a full expression parse would swallow the AND and then fail
/// looking for the one it just consumed.
#[test]
fn between_desugars_to_a_range_conjunct() {
    let (e, _) = parse_expr_only("a BETWEEN 1 AND 3").unwrap();
    assert_eq!(
        e,
        Expr::Binary(
            BinOp::And,
            Box::new(Expr::Binary(BinOp::Ge, col("a"), int(1))),
            Box::new(Expr::Binary(BinOp::Le, col("a"), int(3))),
        )
    );
}

#[test]
fn between_composes_with_a_following_boolean_and() {
    let (e, _) = parse_expr_only("a BETWEEN 1 AND 3 AND b = 2").unwrap();
    // the trailing `AND b = 2` is a separate conjunct, not BETWEEN's bound
    assert!(matches!(&e, Expr::Binary(BinOp::And, _, r)
        if matches!(r.as_ref(), Expr::Binary(BinOp::Eq, ..))), "got {e:?}");
}

#[test]
fn not_between_negates_the_whole_conjunct() {
    let (e, _) = parse_expr_only("a NOT BETWEEN 1 AND 3").unwrap();
    assert!(matches!(&e, Expr::Unary(UnOp::Not, inner)
        if matches!(inner.as_ref(), Expr::Binary(BinOp::And, ..))), "got {e:?}");
}

#[test]
fn in_list_parses_both_shapes_and_negation() {
    let (e, _) = parse_expr_only("a IN (1, 2)").unwrap();
    assert!(matches!(&e, Expr::InList(_, items, false) if items.len() == 2), "got {e:?}");
    let (e, _) = parse_expr_only("a NOT IN (1)").unwrap();
    assert!(matches!(&e, Expr::InList(_, _, true)), "got {e:?}");
    // one-element parens must still be the context form when it IS one
    let (e, _) = parse_expr_only("a IN (current_setting('k'))").unwrap();
    assert!(matches!(&e, Expr::InContext(_, k, false) if k == "k"), "got {e:?}");
    let (e, _) = parse_expr_only("a NOT IN (current_setting('k'))").unwrap();
    assert!(matches!(&e, Expr::InContext(_, _, true)), "got {e:?}");
}

#[test]
fn unary_minus_binds_tighter_than_mul() {
    // -2 * 3 == (-2) * 3
    let e = expr("-2 * 3");
    assert_eq!(
        e,
        Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Unary(UnOp::Neg, int(2))),
            int(3)
        )
    );
}

#[test]
fn arithmetic_precedence_over_comparison() {
    // a + 1 < b * 2  ==  (a+1) < (b*2)
    let e = expr("a + 1 < b * 2");
    assert_eq!(
        e,
        Expr::Binary(
            BinOp::Lt,
            Box::new(Expr::Binary(BinOp::Add, col("a"), int(1))),
            Box::new(Expr::Binary(BinOp::Mul, col("b"), int(2)))
        )
    );
}

#[test]
fn comparisons_do_not_chain() {
    assert!(matches!(
        parse_expr_only("a < b < c"),
        Err(Error::Parse { .. })
    ));
}

#[test]
fn is_null_and_like() {
    assert_eq!(expr("a IS NULL"), Expr::IsNull(col("a"), false));
    assert_eq!(expr("a IS NOT NULL"), Expr::IsNull(col("a"), true));
    assert_eq!(
        expr("a LIKE 'x%'"),
        Expr::Like(col("a"), Box::new(Expr::Lit(Value::Text("x%".into()))))
    );
}

#[test]
fn is_distinct_general_form() {
    // `IS`/`IS NOT` with a non-NULL operand is the NULL-safe distinct-from
    // node, while the NULL forms stay `IsNull` (checked above).
    assert_eq!(expr("a IS b"), Expr::IsDistinct(col("a"), col("b"), false));
    assert_eq!(expr("a IS NOT b"), Expr::IsDistinct(col("a"), col("b"), true));
    assert_eq!(
        expr("a IS 1"),
        Expr::IsDistinct(col("a"), Box::new(Expr::Lit(Value::Int(1))), false)
    );
}

#[test]
fn question_params_number_left_to_right() {
    let (e, n) = parse_expr_only("? + ? = ?").unwrap();
    assert_eq!(n, 3);
    assert_eq!(
        e,
        Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Param(0)),
                Box::new(Expr::Param(1))
            )),
            Box::new(Expr::Param(2))
        )
    );
}

#[test]
fn mixing_param_styles_is_a_parse_error() {
    match parse_statement("SELECT * FROM t WHERE a = $1 AND b = ?") {
        Err(Error::Parse { pos, msg }) => {
            assert_eq!(pos, 37);
            assert!(msg.contains("mix"));
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[test]
fn dollar_params_report_max() {
    let (_, _, n) = parse_statement("SELECT * FROM t WHERE a = $3").unwrap();
    assert_eq!(n, 3);
}

#[test]
fn full_select() {
    let (s, explain, n) = parse_statement(
        "explain select a, b + 1 from t where a > 5 order by a asc, b desc limit 10 offset 2;",
    )
    .unwrap();
    assert!(explain);
    assert_eq!(n, 0);
    match s {
        Stmt::Select(sel) => {
            assert_eq!(sel.table.as_deref(), Some("t"));
            assert_eq!(sel.items.as_ref().unwrap().len(), 2);
            assert!(sel.where_clause.is_some());
            assert_eq!(
                sel.order_by,
                vec![
                    (Expr::Col("a".into()), false),
                    (Expr::Col("b".into()), true)
                ]
            );
            assert_eq!(sel.limit, Some(10));
            assert_eq!(sel.offset, Some(2));
        }
        other => panic!("expected select, got {other:?}"),
    }
}

/// `ORDER BY count(*)` — legal in sqlite and PG, and the reason ORDER BY
/// items are expressions rather than names. An identifier-only ORDER BY
/// rejects this at the tokenizer, before anything can rule on whether it
/// means something.
#[test]
fn order_by_takes_an_aggregate_not_just_a_name() {
    let (s, _, _) =
        parse_statement("select dept, count(*) from t group by dept order by count(*) desc")
            .unwrap();
    match s {
        Stmt::Select(sel) => {
            assert_eq!(sel.group_by, vec![Expr::Col("dept".into())]);
            assert_eq!(
                sel.order_by,
                vec![(
                    Expr::Agg(
                        mpedb_types::AggTarget::Native(mpedb_types::AggFn::Count),
                        None,
                        false,
                        None
                    ),
                    true
                )]
            );
        }
        other => panic!("expected select, got {other:?}"),
    }
}

#[test]
fn select_star_and_limits() {
    let (s, explain, _) = parse_statement("SELECT * FROM t").unwrap();
    assert!(!explain);
    assert!(matches!(s, Stmt::Select(SelectStmt { items: None, .. })));
    assert!(parse_statement("SELECT * FROM t LIMIT -1").is_err());
    assert!(parse_statement("SELECT * FROM t LIMIT $1").is_err());
    assert!(parse_statement("SELECT *, a FROM t").is_err());
}

#[test]
fn insert_forms() {
    let (s, _, n) = parse_statement("INSERT INTO t (a, b) VALUES (1, $1), (2, $2)").unwrap();
    assert_eq!(n, 2);
    match s {
        Stmt::Insert(ins) => {
            assert_eq!(ins.columns, Some(vec!["a".into(), "b".into()]));
            assert_eq!(ins.rows.len(), 2);
        }
        other => panic!("expected insert, got {other:?}"),
    }
    let (s, _, _) = parse_statement("INSERT INTO t VALUES (1, 2)").unwrap();
    assert!(matches!(s, Stmt::Insert(InsertStmt { columns: None, .. })));
}

#[test]
fn update_and_delete() {
    let (s, _, _) = parse_statement("UPDATE t SET a = 1, b = b + 1 WHERE c = 2").unwrap();
    match s {
        Stmt::Update(u) => assert_eq!(u.set.len(), 2),
        other => panic!("expected update, got {other:?}"),
    }
    let (s, _, _) = parse_statement("DELETE FROM t WHERE a = 1").unwrap();
    assert!(matches!(s, Stmt::Delete(_)));
    let (s, _, _) = parse_statement("DELETE FROM t").unwrap();
    assert!(matches!(s, Stmt::Delete(DeleteStmt { where_clause: None, .. })));
}

#[test]
fn txn_statements() {
    assert!(matches!(parse_statement("BEGIN").unwrap().0, Stmt::Begin));
    assert!(matches!(parse_statement("commit;").unwrap().0, Stmt::Commit));
    assert!(matches!(
        parse_statement("Rollback").unwrap().0,
        Stmt::Rollback
    ));
}

#[test]
fn savepoint_statements() {
    // SAVEPOINT / RELEASE / ROLLBACK TO — positional keywords, name is a
    // bare/quoted identifier or string. `ROLLBACK` alone stays plain rollback.
    assert!(matches!(
        parse_statement("SAVEPOINT a").unwrap().0,
        Stmt::Savepoint(n) if n == "a"
    ));
    assert!(matches!(
        parse_statement("release savepoint a").unwrap().0,
        Stmt::Release(n) if n == "a"
    ));
    assert!(matches!(
        parse_statement("RELEASE a").unwrap().0,
        Stmt::Release(n) if n == "a"
    ));
    assert!(matches!(
        parse_statement("ROLLBACK TO a").unwrap().0,
        Stmt::RollbackTo(n) if n == "a"
    ));
    assert!(matches!(
        parse_statement("ROLLBACK TRANSACTION TO SAVEPOINT a").unwrap().0,
        Stmt::RollbackTo(n) if n == "a"
    ));
    assert!(matches!(parse_statement("ROLLBACK").unwrap().0, Stmt::Rollback));
    assert!(matches!(
        parse_statement("ROLLBACK TRANSACTION").unwrap().0,
        Stmt::Rollback
    ));
    // Quoted and string-literal names.
    assert!(matches!(
        parse_statement("SAVEPOINT \"My Point\"").unwrap().0,
        Stmt::Savepoint(n) if n == "My Point"
    ));
    assert!(matches!(
        parse_statement("SAVEPOINT 'sp one'").unwrap().0,
        Stmt::Savepoint(n) if n == "sp one"
    ));
    // A missing name is a parse error, not a panic.
    assert!(parse_statement("SAVEPOINT").is_err());
    assert!(parse_statement("ROLLBACK TO").is_err());
}

#[test]
fn deep_nesting_is_a_parse_error_not_a_crash() {
    // Each of these used to overflow the parser stack and abort the
    // process (uncatchable). They must return Error::Parse instead.
    let parens = format!("{}a > 0{}", "(".repeat(2000), ")".repeat(2000));
    assert!(matches!(
        parse_expr_only(&parens),
        Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
    ));
    // The same input through the statement path (prepare()/CHECK).
    let sql = format!("SELECT * FROM t WHERE {parens}");
    assert!(matches!(
        parse_statement(&sql),
        Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
    ));
    let nots = format!("{}a", "NOT ".repeat(2000));
    assert!(matches!(
        parse_expr_only(&nots),
        Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
    ));
    let negs = format!("{}1", "-".repeat(2000));
    assert!(matches!(
        parse_expr_only(&negs),
        Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply")
    ));

    // Well inside the limit for every form. This used to say 100, back
    // when MAX_EXPR_DEPTH was 128 — a number that turned out not to be
    // survivable once CASE existed (see the constant's docs). 20 is still
    // far past any real statement.
    let d = 20;
    let parens = format!("{}a > 0{}", "(".repeat(d), ")".repeat(d));
    assert!(parse_expr_only(&parens).is_ok());
    assert!(parse_expr_only(&format!("{}a", "NOT ".repeat(d))).is_ok());
    assert!(parse_expr_only(&format!("{}1", "-".repeat(d))).is_ok());
}

/// Every construct that recurses is a stack-overflow vector, and each one
/// added is a NEW path the paren test does not cover. CASE, function
/// arguments and IN lists all descend through `expr()`, so they must hit the
/// same depth guard rather than the thread stack.
///
/// This is not hypothetical: extracting these blocks into their own frames
/// was forced by an actual overflow — inline, their locals were paid on
/// every one of the 128 permitted levels and 128 was no longer survivable.
#[test]
fn deep_nesting_through_the_new_constructs_is_also_a_parse_error() {
    let cases = [
        format!("{}1{}", "coalesce(".repeat(1000), ", 2)".repeat(1000)),
        format!("{}1{}", "abs(".repeat(1000), ")".repeat(1000)),
        format!("{}1{}", "CASE WHEN true THEN ".repeat(1000), " END".repeat(1000)),
        format!("a IN ({}1{})", "abs(".repeat(1000), ")".repeat(1000)),
        format!("{}a BETWEEN 1 AND 2{}", "(".repeat(1000), ")".repeat(1000)),
        format!("{}a > 0{}", "NOT (".repeat(1000), ")".repeat(1000)),
    ];
    for (i, sql) in cases.iter().enumerate() {
        match parse_expr_only(sql) {
            Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply") => {}
            other => panic!("case {i} must be a depth error, got {other:?}"),
        }
    }
}

#[test]
fn item_count_caps() {
    // 70000 projection items: rejected at parse time (the plan encoding
    // stores the count as u16; unchecked it would truncate).
    let mut sql = String::from("SELECT a");
    for _ in 0..69_999 {
        sql.push_str(",a");
    }
    sql.push_str(" FROM t");
    assert!(matches!(
        parse_statement(&sql),
        Err(Error::Parse { msg, .. }) if msg.contains("too many SELECT items")
    ));
    // Exactly the cap still parses.
    let mut sql = String::from("SELECT a");
    for _ in 0..MAX_SELECT_ITEMS - 1 {
        sql.push_str(",a");
    }
    sql.push_str(" FROM t");
    assert!(parse_statement(&sql).is_ok());

    // ORDER BY: 65 items rejected, 64 accepted.
    let mk_order = |n: usize| {
        format!(
            "SELECT * FROM t ORDER BY {}",
            vec!["a"; n].join(", ")
        )
    };
    assert!(matches!(
        parse_statement(&mk_order(MAX_ORDER_BY_ITEMS + 1)),
        Err(Error::Parse { msg, .. }) if msg.contains("too many ORDER BY items")
    ));
    assert!(parse_statement(&mk_order(MAX_ORDER_BY_ITEMS)).is_ok());

    // UPDATE SET: 1025 assignments rejected, 1024 accepted.
    let mk_set = |n: usize| {
        format!(
            "UPDATE t SET {}",
            vec!["a = 1"; n].join(", ")
        )
    };
    assert!(matches!(
        parse_statement(&mk_set(MAX_SET_ITEMS + 1)),
        Err(Error::Parse { msg, .. }) if msg.contains("too many SET assignments")
    ));
    assert!(parse_statement(&mk_set(MAX_SET_ITEMS)).is_ok());
}

#[test]
fn param_count_limit_is_enforced_not_truncated() {
    // 32768 rows x 2 columns = 65536 `?`: must be a parse error at the
    // 65536th `?`, never a silent wrap to n_params == 0 (which used to
    // panic the binder).
    let mut sql = String::from("INSERT INTO t (a, b) VALUES ");
    sql.push_str(&vec!["(?,?)"; 32_768].join(","));
    assert!(matches!(
        parse_statement(&sql),
        Err(Error::Parse { msg, .. }) if msg.contains("too many `?` parameters")
    ));

    // Exactly 65535 `?` (the maximum) still parses with the right count.
    let mut sql = String::from("INSERT INTO t (a) VALUES ");
    sql.push_str(&vec!["(?)"; 65_535].join(","));
    let (_, _, n) = parse_statement(&sql).unwrap();
    assert_eq!(n, 65_535);

    // $n form: $65535 is the maximum; $65536 is rejected by the
    // tokenizer.
    let (_, _, n) = parse_statement("SELECT * FROM t WHERE a = $65535").unwrap();
    assert_eq!(n, 65_535);
    assert!(parse_statement("SELECT * FROM t WHERE a = $65536").is_err());
}

#[test]
fn error_positions_and_trailing_input() {
    match parse_statement("SELECT FROM t") {
        Err(Error::Parse { pos, .. }) => assert_eq!(pos, 7),
        other => panic!("expected parse error, got {other:?}"),
    }
    // `FROM t garbage` is now `FROM t AS garbage` — a valid alias (#44).
    // Genuinely trailing input is a SECOND bare word after the alias.
    assert!(parse_statement("SELECT * FROM t alias garbage").is_err());
    assert!(parse_statement("SELECT * FROM t; SELECT * FROM t").is_err());
    assert!(parse_statement("EXPLAIN EXPLAIN SELECT * FROM t").is_err());
    assert!(parse_expr_only("a = ").is_err());
    assert!(parse_expr_only("(a = 1").is_err());
}

/// The budget must be survivable on the stack it is budgeted against.
///
/// MAX_PARSER_STACK is 1 MiB, i.e. half a default 2 MiB thread. Parse right
/// up to the guard inside exactly that 2 MiB and require an ERROR rather
/// than an abort: if a future construct or a compiler change makes frames
/// fatter than the budget assumes, this fails loudly here instead of taking
/// out the test binary somewhere unrelated.
#[test]
fn the_stack_budget_is_survivable_on_a_default_thread() {
    let inputs: Vec<String> = vec![
        // Deep enough to blow past the budget on every shape, cheap and
        // expensive alike.
        format!("{}a > 0{}", "(".repeat(4000), ")".repeat(4000)),
        format!("{}a", "NOT ".repeat(4000)),
        format!("{}1{}", "CASE WHEN true THEN ".repeat(4000), " END".repeat(4000)),
        format!("{}1{}", "coalesce(".repeat(4000), ", 2)".repeat(4000)),
        format!("{}1{}", "abs(".repeat(4000), ")".repeat(4000)),
        format!("a IN ({}1{})", "abs(".repeat(4000), ")".repeat(4000)),
    ];
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024) // the default a spawned thread gets
        .spawn(move || {
            for sql in &inputs {
                match parse_expr_only(sql) {
                    Err(Error::Parse { msg, .. }) if msg.contains("nested too deeply") => {}
                    other => panic!("expected a depth error, got {other:?}"),
                }
            }
        })
        .unwrap();
    h.join().expect(
        "the parser overflowed a 2 MiB stack before its own 1 MiB budget stopped it: \
         a frame grew, so MAX_PARSER_STACK no longer leaves room. Shrink the frame \
         (move locals into an #[inline(never)] helper) or lower the budget.",
    );
}


/// What the byte budget actually buys, per construct. Compare against the
/// measured ancestors (sqlite3 3.45: 93 nested parens, 18 nested CASE).
///   cargo test -p mpedb-sql --lib limits_probe -- --ignored --nocapture
#[test]
#[ignore]
fn limits_probe() {
    type Gen = Box<dyn Fn(usize) -> String>;
    let mk: Vec<(&str, Gen)> = vec![
        ("parens", Box::new(|d| format!("{}1{}", "(".repeat(d), ")".repeat(d)))),
        ("NOT", Box::new(|d| format!("{}a", "NOT ".repeat(d)))),
        ("CASE", Box::new(|d| format!("{}1{}", "CASE WHEN true THEN ".repeat(d), " END".repeat(d)))),
        ("coalesce", Box::new(|d| format!("{}1{}", "coalesce(".repeat(d), ", 2)".repeat(d)))),
    ];
    for (name, f) in mk {
        let (mut lo, mut hi) = (1usize, 4000usize);
        while lo < hi {
            let m = (lo + hi).div_ceil(2);
            let sql = f(m);
            let ok = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || parse_expr_only(&sql).is_ok())
                .unwrap()
                .join()
                .unwrap_or(false);
            if ok { lo = m } else { hi = m - 1 }
        }
        eprintln!("  mpedb max nested {name:>9}: {lo}");
    }
}

/// Both ancestors accept `<table>.<column>`, so mpedb does. The qualifier
/// is CHECKED rather than ignored: with one table in scope it is decoration,
/// but silently accepting `nonsense.id` would turn a typo into a
/// wrong-table read the day joins exist.
#[test]
fn table_qualified_columns_parse_and_are_distinct_from_excluded() {
    let (e, _) = parse_expr_only("orders.tenant").unwrap();
    assert_eq!(e, Expr::Qualified("orders".into(), "tenant".into()));
    // `excluded` is its own thing, not a table qualifier.
    let (e, _) = parse_expr_only("excluded.tenant").unwrap();
    assert_eq!(e, Expr::Excluded("tenant".into()));
    // A quoted qualifier is still a qualifier; a quoted `excluded` is a column.
    let (e, _) = parse_expr_only("\"excluded\"").unwrap();
    assert_eq!(e, Expr::Col("excluded".into()));
}

/// #1 — THE Django blocker. A quoted identifier must be usable everywhere a
/// bare one is, and in particular on EITHER side of a qualifier dot: Django
/// quotes every identifier it emits, so `"t"."c"` failing to parse failed ~every
/// ORM query before a single row was read.
///
/// All four bare/quoted mixes, and all three sqlite quoting spellings, must
/// produce the IDENTICAL expression — a quoted name differs from a bare one only
/// in what characters it may contain.
#[test]
fn a_quoted_identifier_can_qualify_a_dotted_reference() {
    let want = Expr::Qualified("t".into(), "c".into());
    for src in [
        "t.c",         // bare  . bare
        "\"t\".\"c\"", // quoted . quoted
        "\"t\".c",     // quoted . bare
        "t.\"c\"",     // bare  . quoted
        "`t`.`c`",     // backtick, both sides
        "[t].[c]",     // bracket, both sides
        "`t`.\"c\"",   // …and the spellings mix freely
        "[t].c",
    ] {
        assert_eq!(parse_expr_only(src).unwrap().0, want, "{src}");
    }
    // Quoting is what turns a keyword-ish word into a plain name, on both sides.
    assert_eq!(
        parse_expr_only("\"select\".\"from\"").unwrap().0,
        Expr::Qualified("select".into(), "from".into())
    );
    // A quoted qualifier is NOT the `excluded` pseudo-table and NOT a function:
    // quoting says "this is a name", so a quoted `excluded` stays a column.
    assert_eq!(
        parse_expr_only("\"excluded\".\"c\"").unwrap().0,
        Expr::Qualified("excluded".into(), "c".into())
    );
}

/// A THREE-part name refuses BY NAME rather than emitting the confusing
/// "unexpected trailing input `Dot`" the old grammar produced. mpedb's only
/// schema qualifier is the Workspace alias, which is stripped off a TABLE
/// reference and never off a column, so `db.t.c` could only ever be guessed at.
#[test]
fn three_part_names_refuse_by_name() {
    for src in ["main.t.c", "\"main\".\"t\".\"c\"", "[a].[b].[c]"] {
        let e = parse_expr_only(src).unwrap_err().to_string();
        assert!(e.contains("three-part name"), "{src}: {e}");
    }
}

/// `t.*` is per-table star expansion — a different feature from a qualified
/// column, and one mpedb does not have. It refuses by NAME rather than
/// complaining about a missing column name at a `*` the writer meant.
#[test]
fn per_table_star_refuses_by_name() {
    for src in ["t.*", "\"t\".*", "[t].*"] {
        let e = parse_statement(&format!("SELECT {src} FROM t"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("star expansion"), "{src}: {e}");
    }
}
