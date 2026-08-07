//! Binder unit tests (split from binder.rs).
    use super::*;
    use crate::parser::parse_expr_only;
    use mpedb_types::ColumnDef;

    fn table() -> TableDef {
        let col = |name: &str, ty: ColumnType, nullable: bool| ColumnDef { generated: None, default_text: None, decl: None,
            name: name.into(),
            ty,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None, collation: Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ty),
        };
        TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                col("id", ColumnType::Int64, false),
                col("score", ColumnType::Float64, true),
                col("name", ColumnType::Text, true),
                col("active", ColumnType::Bool, true),
                col("data", ColumnType::Blob, true),
                col("created", ColumnType::Timestamp, true),
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false, autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        }
    }

    fn bind(src: &str, n_params: u16) -> Result<(BExpr, Ty, Vec<Ty>)> {
        let (ast, n) = parse_expr_only(src)?;
        assert!(n <= n_params, "test forgot params");
        let t = table();
        let mut b = Binder::new(&t, n_params, true);
        let (e, ty) = b.bind_expr(&ast)?;
        Ok((e, ty, b.param_types))
    }

    #[test]
    fn rigid_cross_type_rejections() {
        for src in [
            "name = 1",
            "id = 'x'",
            "id + 'x'",
            "name + name",
            "created = 1",
            "data = 'x'",
            "-name",
            // (`name LIKE 1` used to sit here; a constant numeric pattern now
            // binds under the sqlite dialect and coerces at runtime — sqlite's
            // likeFunc rule, #74 item 3. The PG dialect still refuses it; see
            // `like_pattern_dyn_binds_and_blob_refuses_by_name`.)
            // Arithmetic on a bool is still rigid — the int/bool bridge is a
            // COMPARISON/assignment rule, never a general interchange.
            "active + 1",
        ] {
            assert!(
                matches!(bind(src, 0), Err(Error::Bind(_))),
                "expected bind error for {src}"
            );
        }
        // Formerly rigid, now sqlite-compatible (Django gap #5). `active` is a
        // bool column, `id` an int64 one.
        for src in ["active = 1", "active = 0", "NOT id", "id AND active"] {
            assert!(bind(src, 0).is_ok(), "expected {src} to bind");
        }
        // `active = 1` keeps the plain `Binary(Eq, Col, Const)` shape — the int
        // literal folds into the bool domain rather than casting the column, so
        // an index probe on the column survives.
        let (e, ty, _) = bind("active = 1", 0).unwrap();
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Eq,
                Box::new(BExpr::Col(3)),
                Box::new(BExpr::Const(Value::Bool(true))),
            )
        );
        // A non-0/1 integer casts the BOOL side up instead, so `active = 2` is
        // FALSE (sqlite's answer) rather than TRUE.
        let (e, _, _) = bind("active = 2", 0).unwrap();
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Eq,
                Box::new(BExpr::Cast(Box::new(BExpr::Col(3)), Affinity::Integer)),
                Box::new(BExpr::Const(Value::Int(2))),
            )
        );
    }

    #[test]
    fn int_to_float_coercion_and_folding() {
        // Column int meets float literal: column side gets ToFloat.
        let (e, ty, _) = bind("id < 1.5", 0).unwrap();
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(
            e,
            BExpr::Binary(
                BinOp::Lt,
                Box::new(BExpr::Unary(BUnOp::ToFloat, Box::new(BExpr::Col(0)))),
                Box::new(BExpr::Const(Value::Float(1.5)))
            )
        );
        // Both literals: fully folded, int coerced.
        let (e, ty, _) = bind("1 + 2.5", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Float(3.5)));
        assert_eq!(ty, Some(ColumnType::Float64));
        // Pure-int folding.
        let (e, _, _) = bind("2 + 3 * 4", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Int(14)));
        // Bool folding through comparisons and logic.
        let (e, _, _) = bind("1 < 2 AND NOT false", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
        // LIKE folding.
        let (e, _, _) = bind("'hello' LIKE 'he%'", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
    }

    #[test]
    fn fold_matches_the_runtime_semantics() {
        // Division / modulo by zero folds to NULL (sqlite semantics), exactly
        // as the runtime `/` and `%` operators evaluate them.
        assert_eq!(bind("1 / 0", 0).unwrap().0, BExpr::Const(Value::Null));
        assert_eq!(bind("1 % 0", 0).unwrap().0, BExpr::Const(Value::Null));
        // Overflow, however, still raises at fold time as it does at runtime.
        assert!(matches!(
            bind("9223372036854775807 + 1", 0),
            Err(Error::ArithmeticOverflow)
        ));
    }

    #[test]
    fn param_unification() {
        // Param adopts column type.
        let (_, _, params) = bind("id = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        let (_, _, params) = bind("name = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Text)]);
        // Bool context.
        let (_, _, params) = bind("$1 AND active", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Bool)]);
        // LIKE lhs.
        let (_, _, params) = bind("$1 LIKE 'x%'", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Text)]);
        // Same param twice, consistent.
        let (_, _, params) = bind("id = $1 AND $1 < 10", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        // Unused param stays unconstrained.
        let (_, _, params) = bind("id = $2", 2).unwrap();
        assert_eq!(params, vec![None, Some(ColumnType::Int64)]);
    }

    #[test]
    fn param_unification_conflicts() {
        // $1 pinned to text, then used where int is required.
        assert!(matches!(
            bind("name = $1 AND id = $1", 1),
            Err(Error::Bind(_))
        ));
        // Int-typed param in float context is legal (ToFloat at use site).
        let (e, _, params) = bind("id = $1 AND score = $1", 1).unwrap();
        assert_eq!(params, vec![Some(ColumnType::Int64)]);
        // The second use wraps the param in ToFloat.
        let s = format!("{e:?}");
        assert!(s.contains("ToFloat"), "expected ToFloat in {s}");
    }

    #[test]
    fn like_pattern_dyn_binds_and_blob_refuses_by_name() {
        // #74 item 3, LIKE half: a bound / column / computed pattern BINDS —
        // the old "must be a literal" refusal was structural, exactly as it
        // was for REGEXP. A bare parameter is pinned to text.
        let (e, ty, params) = bind("name LIKE $1", 1).unwrap();
        assert!(matches!(e, BExpr::LikeDyn(..)), "{e:?}");
        assert_eq!(ty, Some(ColumnType::Bool));
        assert_eq!(params[0], Some(ColumnType::Text));
        // A per-row COLUMN pattern is legal (sqlite evaluates it per row).
        assert!(matches!(bind("name LIKE name", 0), Ok((BExpr::LikeDyn(..), _, _))));
        // GLOB closed the same way.
        assert!(matches!(bind("name GLOB $1", 1), Ok((BExpr::GlobDyn(..), _, _))));
        // A text LITERAL keeps the const-pool node — its plan bytes are the
        // pre-#74 ones.
        assert!(matches!(bind("name LIKE 'a%'", 0), Ok((BExpr::Like(..), _, _))));
        // A statically-BLOB pattern is refused by name, naming the PATTERN
        // half of the statement (`data` is the blob column).
        match bind("name LIKE data", 0) {
            Err(Error::Bind(m)) => assert!(m.contains("LIKE pattern"), "{m}"),
            other => panic!("expected bind error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_column() {
        match bind("nope = 1", 0) {
            Err(Error::Bind(m)) => assert!(m.contains("nope")),
            other => panic!("expected bind error, got {other:?}"),
        }
    }

    #[test]
    fn predicate_typing() {
        let t = table();
        let mut b = Binder::new(&t, 0, true);
        // A non-boolean predicate is truthy-tested like sqlite, not refused:
        // `WHERE 42` desugars to `42 <> 0` and folds to TRUE.
        let (ast, _) = parse_expr_only("42").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(true)));
        let (ast, _) = parse_expr_only("0").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(false)));
        // A text predicate takes the CAST-to-REAL path (sqlite's RealValue).
        let (ast, _) = parse_expr_only("'3abc'").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(true)));
        let (ast, _) = parse_expr_only("'abc'").unwrap();
        assert_eq!(b.bind_predicate(&ast).unwrap(), BExpr::Const(Value::Bool(false)));
        let (ast, _) = parse_expr_only("id = 42").unwrap();
        assert!(b.bind_predicate(&ast).is_ok());
        // NULL predicate is legal (never passes).
        let (ast, _) = parse_expr_only("NULL").unwrap();
        assert!(b.bind_predicate(&ast).is_ok());
        // Bare param in predicate position becomes bool.
        let mut b = Binder::new(&t, 1, true);
        let (ast, _) = parse_expr_only("$1").unwrap();
        b.bind_predicate(&ast).unwrap();
        assert_eq!(b.param_types, vec![Some(ColumnType::Bool)]);
    }

    #[test]
    fn no_params_mode() {
        let t = table();
        let mut b = Binder::new(&t, 1, false);
        let (ast, _) = parse_expr_only("id = $1").unwrap();
        assert!(matches!(b.bind_expr(&ast), Err(Error::Bind(_))));
    }

    #[test]
    fn null_comparisons_fold_to_null() {
        let (e, _, _) = bind("1 = NULL", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Null));
        let (e, _, _) = bind("NULL IS NULL", 0).unwrap();
        assert_eq!(e, BExpr::Const(Value::Bool(true)));
    }

    #[test]
    fn compiled_program_evaluates() {
        let (e, _, _) = bind("id + 1 < 10", 0).unwrap();
        let p = compile_program(&e).unwrap();
        assert_eq!(
            p.eval(&[Value::Int(5), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null], &[])
                .unwrap(),
            Value::Bool(true)
        );
    }


    fn bind_ok(sql: &str) -> (BExpr, Ty) {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        b.bind_expr(&e).unwrap()
    }
    fn bind_err_msg(sql: &str) -> String {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        format!("{}", b.bind_expr(&e).unwrap_err())
    }

    /// The constant-folding / laziness boundary. The raising case that a dead
    /// branch must NOT evaluate is arithmetic overflow (mpedb raises it where
    /// sqlite wraps); `OVF` below is `9223372036854775807 + 1`. Division by
    /// zero is deliberately NOT a raise — mpedb folds `1/0` to NULL like
    /// sqlite — so it doubles here as the positive control:
    ///
    ///   never fold a live raise -> `SELECT OVF` would prepare clean and fail
    ///     at every execute. PG raises at PLAN time.
    ///   always fold every branch -> `coalesce(1, OVF)` dies, though both
    ///     sqlite and PG answer 1.
    ///
    /// The rule is neither: fold the CONTROL FLOW first and drop the
    /// unreachable branch WITHOUT evaluating it; fold what survives, and let
    /// that raise.
    #[test]
    fn folding_drops_dead_branches_before_it_can_raise_on_them() {
        const OVF: &str = "9223372036854775807 + 1";
        // arg0 is a non-NULL constant -> the whole coalesce IS it, and the
        // overflow is never folded. PG: 1.
        assert_eq!(
            bind_ok(&format!("coalesce(1, {OVF})")).0,
            BExpr::Const(Value::Int(1))
        );
        // arg0 is a NULL constant -> dropped; the overflow becomes reachable
        // -> raises. PG: ERROR.
        assert!(matches!(
            bind_expr_res(&format!("coalesce(NULL, {OVF})")),
            Err(Error::ArithmeticOverflow)
        ));
        // Same rule through CASE.
        assert_eq!(
            bind_ok(&format!("CASE WHEN true THEN 1 ELSE {OVF} END")).0,
            BExpr::Const(Value::Int(1))
        );
        assert!(matches!(
            bind_expr_res(&format!("CASE WHEN false THEN 1 ELSE {OVF} END")),
            Err(Error::ArithmeticOverflow)
        ));
        // Division by zero, in contrast, is NULL, never a raise — even when
        // reachable. `coalesce(NULL, 1/0)` reduces to `coalesce(1/0)` = NULL.
        assert_eq!(bind_ok("coalesce(1, 1/0)").0, BExpr::Const(Value::Int(1)));
        assert_eq!(
            bind_expr_res("coalesce(NULL, 1/0)").unwrap().0,
            BExpr::Const(Value::Null)
        );
        // A live branch still folds normally.
        assert_eq!(bind_ok("1 + 2").0, BExpr::Const(Value::Int(3)));
    }

    fn bind_expr_res(sql: &str) -> Result<(BExpr, Ty)> {
        let t = table();
        let (e, n) = parse_expr_only(sql).unwrap();
        let mut b = Binder::new(&t, n, true);
        b.bind_expr(&e)
    }

    #[test]
    fn coalesce_arguments_must_unify() {
        // int64/text is ACCEPTED now: both are sqlite storage classes, and the
        // result is typed per row like every other mixed arm set. Django's
        // `GeneratedField` compiles `Concat` to exactly this shape.
        let (_, ty) = bind_ok("coalesce(id, 'x')");
        assert_eq!(ty, Some(ColumnType::Any));
        // A type with NO sqlite storage class stays refused — `bool` is
        // mpedb's own, so a per-row rule over it would be invented rather than
        // reproduced — and the message still names the CAST fix.
        assert!(bind_err_msg("coalesce(name, active)").contains("CAST"));
        // Explicitly casting every arm to one type still yields that type.
        let (_, ty) = bind_ok("coalesce(CAST(id AS REAL), 1.5)");
        assert_eq!(ty, Some(ColumnType::Float64));
    }

    /// int64 ∪ float64 RESULT arms: sqlite types the winning arm per ROW
    /// (`COALESCE(30, avg(x))` is the INTEGER 30 when arm 1 wins), so no arm
    /// is coerced — each keeps its own type and value, and the expression
    /// types as `any`, decided per value at runtime. Widening instead was
    /// measured at 82 wrong answers in the sqllogictest expr corpus.
    #[test]
    fn mixed_numeric_result_arms_type_as_any() {
        // Constant COALESCE folds to the winning arm UNWIDENED: the integer 30.
        let (e, ty) = bind_ok("coalesce(30, 1.5)");
        assert_eq!(e, BExpr::Const(Value::Int(30)));
        assert_eq!(ty, Some(ColumnType::Any));
        // Non-constant arms stay control flow, typed any.
        let (_, ty) = bind_ok("coalesce(score, 1)");
        assert_eq!(ty, Some(ColumnType::Any));
        // Same rule through CASE (and its sugar iif); the winning constant
        // arm keeps its own type: sqlite answers 1, not 1.0.
        let (e, ty) = bind_ok("CASE WHEN true THEN 1 ELSE 2.5 END");
        assert_eq!(e, BExpr::Const(Value::Int(1)));
        assert_eq!(ty, Some(ColumnType::Any));
        let (_, ty) = bind_ok("CASE WHEN active THEN 1 ELSE 2.5 END");
        assert_eq!(ty, Some(ColumnType::Any));
        let (_, ty) = bind_ok("iif(active, 1, 2.5)");
        assert_eq!(ty, Some(ColumnType::Any));
        // An `any` arm (here a NUMERIC cast of text) mixes with anything.
        let (_, ty) = bind_ok("coalesce(CAST(name AS NUMERIC), name)");
        assert_eq!(ty, Some(ColumnType::Any));
    }

    /// The per-row rule is sqlite's; PostgreSQL PROMOTES the arms statically
    /// (`COALESCE(30, 1.5) / 35` is numeric division ≈0.857 in PG, integer
    /// division 0 per-row), so under the postgres dialect the mix stays the
    /// original rigid refusal — a clean error, never either engine's answer.
    #[test]
    fn mixed_arms_stay_refused_under_postgres_dialect() {
        let t = table();
        let (e, n) = parse_expr_only("coalesce(30, 1.5)").unwrap();
        let mut b = Binder::new(&t, n, true);
        b.set_dialect(Dialect::Postgres);
        let err = format!("{}", b.bind_expr(&e).unwrap_err());
        assert!(err.contains("CAST"), "{err}");
    }

    /// A zero divisor is NULL under sqlite and an ERROR under PostgreSQL, and
    /// the dialect must reach the OPCODE rather than a runtime flag.
    #[test]
    fn division_by_zero_raises_under_postgres_and_stays_null_under_sqlite() {
        use mpedb_types::expr::Instr;

        let bind = |sql: &str, d: Dialect| {
            let t = table();
            let (e, n) = parse_expr_only(sql).unwrap();
            let mut b = Binder::new(&t, n, true);
            b.set_dialect(d);
            b.bind_expr(&e)
        };

        // sqlite: folds to NULL, exactly as before. This is the no-loss half
        // and it is the reason the change is two opcodes and not one edit to
        // `arith`.
        let (e, _) = bind("1 / 0", Dialect::Sqlite).expect("sqlite folds");
        assert!(matches!(e, BExpr::Const(Value::Null)), "{e:?}");
        let (e, _) = bind("7 % 0", Dialect::Sqlite).expect("sqlite folds");
        assert!(matches!(e, BExpr::Const(Value::Null)), "{e:?}");

        // PostgreSQL: the fold EVALUATES the program, so a constant divisor
        // refuses at bind time rather than at execution. Same outcome for the
        // caller, one round trip earlier.
        let err = bind("1 / 0", Dialect::Postgres).unwrap_err();
        assert!(format!("{err}").contains("division by zero"), "{err}");
        let err = bind("7 % 0", Dialect::Postgres).unwrap_err();
        assert!(format!("{err}").contains("division by zero"), "{err}");
        // Floats too: PostgreSQL's `1.0 / 0` is an error, not an infinity.
        let err = bind("1.0 / 0.0", Dialect::Postgres).unwrap_err();
        assert!(format!("{err}").contains("division by zero"), "{err}");

        // A NON-constant divisor must compile, and to the STRICT opcode — the
        // whole point is that the plan bytes say which dialect made them.
        let (e, _) = bind("id / 2", Dialect::Postgres).expect("binds");
        let p = super::lower::compile_program(&e).expect("compiles");
        assert!(p.instrs.contains(&Instr::DivStrict), "{:?}", p.instrs);
        let (e, _) = bind("id / 2", Dialect::Sqlite).expect("binds");
        let p = super::lower::compile_program(&e).expect("compiles");
        assert!(p.instrs.contains(&Instr::Div), "{:?}", p.instrs);
        assert!(!p.instrs.contains(&Instr::DivStrict));
    }

    #[test]
    fn function_arity_and_types_are_compile_errors() {
        assert!(bind_err_msg("lower(id)").contains("must be text"));
        assert!(bind_err_msg("length('a', 'b')").contains("argument"));
        assert!(bind_err_msg("abs('x')").contains("number"));
        assert!(bind_err_msg("frobnicate(1)").contains("unknown function"));
    }

    /// abs/round keep their argument's numeric type rather than pinning one.
    #[test]
    fn abs_and_round_return_their_argument_type() {
        assert_eq!(bind_ok("abs(id)").1, Some(ColumnType::Int64));
        assert_eq!(bind_ok("abs(score)").1, Some(ColumnType::Float64));
        assert_eq!(bind_ok("length(name)").1, Some(ColumnType::Int64));
        assert_eq!(bind_ok("lower(name)").1, Some(ColumnType::Text));
    }

    /// nullif is CASE, not a function: reusing the desugaring keeps one set of
    /// NULL/equality rules rather than two.
    #[test]
    fn nullif_desugars_to_case() {
        let (e, _) = bind_ok("nullif(id, 1)");
        assert!(matches!(e, BExpr::Case(..)), "got {e:?}");
    }

    /// [`Scope`] exists so the NEXT step changes one type instead of 45 call
    /// sites. That claim is only worth anything if a two-table scope actually
    /// resolves, so this builds one directly — no SQL surface reaches it yet.
    ///
    /// The rule it pins: a column resolves to an OFFSET INTO THE TUPLE the
    /// expression is evaluated over. One table = the row. `ON CONFLICT DO
    /// UPDATE` = `[existing ‖ proposed]`, which is why `excluded.<c>` is
    /// `Col(n + i)`. A join = the concatenated rows. Same rule, wider tuple.
    #[test]
    fn a_scope_can_already_hold_two_tables() {
        let a = table(); // id, score, name, active, data, created
        let b = TableDef {
            id: 0,
            name: "other".into(),
            columns: vec![ColumnDef { generated: None, default_text: None, decl: None,
                name: "tag".into(),
                ty: ColumnType::Text,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: mpedb_types::Affinity::implied_by(ColumnType::Text),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false, autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        };
        let sc = Scope {
            names: vec![a.name.clone(), b.name.clone()],
            tables: vec![&a, &b],
        };
        assert_eq!(sc.width(), a.columns.len() + 1);

        // Table b's column sits AFTER a's, at a's width — the concatenation.
        let (slot, ty) = sc.resolve("tag").unwrap();
        assert_eq!((slot as usize, ty), (a.columns.len(), ColumnType::Text));
        // Table a's columns keep their slots, so nothing shifts under them.
        assert_eq!(sc.resolve("id").unwrap().0, 0);
        // Qualifiers reach either side.
        assert_eq!(sc.resolve_qualified("other", "tag").unwrap().0 as usize, a.columns.len());
        assert_eq!(sc.resolve_qualified("t", "id").unwrap().0, 0);
        // A qualifier naming no table in scope is an error, not a silent pick.
        assert!(sc.resolve_qualified("nonsense", "id").is_err());
    }

    /// Ambiguity must be an ERROR. With one table it cannot arise; the day it
    /// can, guessing is a wrong-table read — the exact failure the footprint
    /// discipline exists to prevent.
    #[test]
    fn an_ambiguous_column_is_refused_rather_than_guessed() {
        let a = table();
        let b = TableDef {
            id: 0,
            name: "other".into(),
            columns: vec![ColumnDef { generated: None, default_text: None, decl: None,
                name: "id".into(), // collides with a.id
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: mpedb_types::Affinity::implied_by(ColumnType::Int64),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            implicit_rowid: false, autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        };
        let sc = Scope {
            names: vec![a.name.clone(), b.name.clone()],
            tables: vec![&a, &b],
        };
        let e = sc.resolve("id").unwrap_err();
        assert!(format!("{e}").contains("ambiguous"), "got {e}");
        // ...but qualifying resolves it, to the right side each time.
        assert_eq!(sc.resolve_qualified("t", "id").unwrap().0, 0);
        assert_eq!(sc.resolve_qualified("other", "id").unwrap().0 as usize, a.columns.len());
    }
