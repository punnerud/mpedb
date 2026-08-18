//! Function-call binding: scalar/JSON/host/spell calls, static typing, and
//! result-arm unification for CASE/COALESCE (split from binder.rs; see mod.rs).

use super::*;

impl<'a> Binder<'a> {
    /// Bind a built-in scalar function: resolve the name, check the argument
    /// types against what the function accepts, and give the call a type.
    ///
    /// `nullif` is desugared here rather than implemented: it is exactly
    /// `CASE WHEN a = b THEN NULL ELSE a END`, and re-implementing it would be a
    /// second place for the NULL and equality rules to drift.
    pub(super) fn bind_func(&mut self, name: &str, args: &[ast::Expr]) -> Result<(BExpr, Ty)> {
        // PostgreSQL's own function names, resolved by REWRITING to mpedb's
        // rather than by new opcodes — so the PG surface costs the plan format
        // nothing. `resolve` returns `None` for every name that is not
        // PG-specific, which is what keeps `lower`, `abs` and the rest on
        // exactly ONE code path in both dialects (`pg/funcs.rs`).
        if self.dialect == Dialect::Postgres {
            if let Some(hit) = crate::pg::funcs::resolve(name, args.len()) {
                use crate::pg::funcs::PgFunc;
                match hit? {
                    PgFunc::Const(v) => {
                        // `Ty` is `Option<ColumnType>` — None is the untyped
                        // NULL, which none of these constants is.
                        let ty = v.column_type();
                        return Ok((BExpr::Const(v), ty));
                    }
                    PgFunc::ConstOfAny(v) => {
                        let ty = v.column_type();
                        return Ok((BExpr::Const(v), ty));
                    }
                    PgFunc::TypeOf => {
                        // Exact, not a guess: bind the argument and read the
                        // type the binder gave it. An untyped NULL or an
                        // unpinned parameter has no static type, and PostgreSQL
                        // calls that `unknown` — the same word, so the answer
                        // stays honest rather than inventing `text`.
                        let Some(arg) = args.first() else {
                            return Err(bind_err("pg_typeof() takes exactly 1 argument"));
                        };
                        let (_, ty) = self.bind_expr(arg)?;
                        let name = match ty {
                            Some(t) => mpedb_types::pgtype::by_oid(
                                mpedb_types::pgtype::default_oid(t),
                            )
                            .map(|p| p.name)
                            .unwrap_or("unknown"),
                            None => "unknown",
                        };
                        return Ok((
                            BExpr::Const(Value::Text(name.into())),
                            Some(ColumnType::Text),
                        ));
                    }
                    PgFunc::Scalar(f) => {
                        let mut bound = Vec::with_capacity(args.len());
                        for a in args {
                            bound.push(self.bind_expr(a)?.0);
                        }
                        if !f.arity_ok(bound.len() as u8) {
                            return Err(bind_err(format!(
                                "{}() called with {} argument(s)",
                                f.name(),
                                bound.len()
                            )));
                        }
                        return Ok((BExpr::Call(f, bound), Some(ColumnType::Text)));
                    }
                    PgFunc::AlwaysTrue => {
                        return Ok((BExpr::Const(Value::Bool(true)), Some(ColumnType::Bool)))
                    }
                    PgFunc::FirstArg => {
                        let Some(first) = args.first() else {
                            return Err(bind_err(format!("{name}() takes at least 1 argument")));
                        };
                        return self.bind_expr(first);
                    }
                    PgFunc::SiblingColumn(col) => {
                        // The rendered answer lives on the relation the OID came
                        // from, so the qualifier has to be carried over: `c.oid`
                        // becomes `c.condef`, and an unqualified `oid` becomes an
                        // unqualified reference the ordinary scope rules resolve.
                        let sibling = match args.first() {
                            Some(ast::Expr::Qualified(t, _)) => {
                                ast::Expr::Qualified(t.clone(), col.to_string())
                            }
                            Some(ast::Expr::Col(_, _)) => ast::Expr::Col(col.to_string(), false),
                            _ => {
                                return Err(bind_err(format!(
                                    "{name}() is supported only over a catalog \
                                     column (e.g. pg_constraint.oid)"
                                )))
                            }
                        };
                        return self.bind_expr(&sibling);
                    }
                    PgFunc::Alias(real) => return self.bind_func(real, args),
                    PgFunc::AliasSwap2(real) => {
                        let swapped = [args[1].clone(), args[0].clone()];
                        return self.bind_func(real, &swapped);
                    }
                }
            }
        }
        if name == "nullif" {
            if args.len() != 2 {
                return Err(bind_err("nullif() takes exactly 2 arguments"));
            }
            let eq = ast::Expr::Binary(
                ast::BinOp::Eq,
                Box::new(args[0].clone()),
                Box::new(args[1].clone()),
            );
            let case = ast::Expr::Case(
                vec![(eq, ast::Expr::Lit(Value::Null))],
                Some(Box::new(args[0].clone())),
            );
            return self.bind_expr(&case);
        }
        // `iif(c, a, b)` is control flow — exactly `CASE WHEN c THEN a ELSE b
        // END`. Desugared here (like nullif) so the CASE path owns the
        // bool-condition rule and the laziness, and iif never NULL-propagates.
        if name == "iif" {
            if args.len() != 3 {
                return Err(bind_err("iif() takes exactly 3 arguments"));
            }
            let case = ast::Expr::Case(
                vec![(args[0].clone(), args[1].clone())],
                Some(Box::new(args[2].clone())),
            );
            return self.bind_expr(&case);
        }
        // `char(X1, …, Xn)` is variadic and every argument is an integer code
        // point, so it is bound here rather than through the fixed-arity `want`
        // table below — each argument is pinned/checked to int64 individually.
        if name == "char" {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                let (e, t) = self.bind_expr(a)?;
                let (e, t) = self.unify_param(e, t, ColumnType::Int64);
                match t {
                    Some(ColumnType::Int64) | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "char() arguments must be int64 code points, got {other}"
                        )))
                    }
                }
                out.push(e);
            }
            if u8::try_from(out.len()).is_err() {
                return Err(bind_err("char() takes at most 255 arguments"));
            }
            return Ok((BExpr::Call(ScalarFn::Char, out), Some(ColumnType::Text)));
        }
        // `printf(FORMAT, …)` / `format(FORMAT, …)` is variadic: the first
        // argument is the format string (pinned to text) and the rest are data
        // arguments of ANY type — the format's specifiers coerce them at
        // runtime, and the format may be a non-literal, so the binder cannot
        // (and must not) pin the data arguments to a type. `format` is an exact
        // alias for `printf`.
        if name == "printf" || name == "format" {
            if args.is_empty() {
                return Err(bind_err(
                    "printf()/format() requires at least a format string argument",
                ));
            }
            let mut out = Vec::with_capacity(args.len());
            for (idx, a) in args.iter().enumerate() {
                let (e, t) = self.bind_expr(a)?;
                if idx == 0 {
                    // The format string must be text; a bare param adopts text.
                    let (e, t) = self.unify_param(e, t, ColumnType::Text);
                    match t {
                        Some(ColumnType::Text) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "printf()/format() format string must be text, got {other}"
                            )))
                        }
                    }
                    out.push(e);
                } else {
                    // A data argument keeps whatever type it has; an untyped bare
                    // param is left for resolve_params to report (printf cannot
                    // pin it — the specifier that consumes it is only known at
                    // runtime).
                    out.push(e);
                }
            }
            if u8::try_from(out.len()).is_err() {
                return Err(bind_err("printf()/format() takes at most 255 arguments"));
            }
            return Ok((BExpr::Call(ScalarFn::Printf, out), Some(ColumnType::Text)));
        }
        // The JSON family. `json_array`/`json_object`/`json_set`/`json_insert`/
        // `json_replace` take VALUE arguments whose reading depends on sqlite's
        // per-value JSON subtype, so they are bound specially (a leading
        // bitmask argument); `json_quote` of an already-JSON argument is that
        // argument. See [`Self::bind_json_call`].
        if name.starts_with("json") {
            if let Some(bound) = self.bind_json_call(name, args)? {
                return Ok(bound);
            }
        }
        // sqlite's SCALAR `max(a, b, …)` / `min(a, b, …)` (#74 item 5). Variadic
        // and typed by SELECTION rather than by computation, which neither the
        // fixed `want` table nor the `ret` recomputation below can express, so
        // it binds here like `char`/`printf` do.
        if (name == "max" || name == "min") && args.len() >= 2 {
            let mut bound = Vec::with_capacity(args.len());
            for a in args {
                bound.push(self.bind_expr(a)?);
            }
            if u8::try_from(bound.len()).is_err() {
                return Err(bind_err(format!("{name}() takes at most 255 arguments")));
            }
            // The distinct CONCRETE argument types (an untyped NULL or an
            // unpinned bare parameter contributes none).
            let mut kinds: Vec<ColumnType> = Vec::new();
            for (_, t) in &bound {
                if let Some(t) = t {
                    if !kinds.contains(t) {
                        kinds.push(*t);
                    }
                }
            }
            // The result type. This is a SELECTION: the winning ARGUMENT is
            // returned unchanged, so a mixed-type call can produce either
            // argument's type and the honest answer is `any`.
            //
            //  * one concrete type  -> that type. `max(i, 3)` is int64.
            //  * numbers only       -> `any`. sqlite's `max(3, 2.5)` is the
            //    INTEGER 3 and `max(1, 2.5)` is the REAL 2.5; widening to
            //    float64 would turn the first into 3.0, a different value.
            //  * an `any` present   -> `any`; the runtime orders by storage
            //    class, which is sqlite's own rule (`Value::sort_cmp`).
            //  * anything else      -> REFUSED by name. sqlite would order a
            //    number against a text by storage class, but that is the same
            //    cross-class comparison `sql_cmp` refuses everywhere else, and
            //    mpedb's own bool/timestamp have no class at all.
            let numeric = |t: &ColumnType| {
                matches!(t, ColumnType::Int64 | ColumnType::Float64 | ColumnType::Any)
            };
            let ret = match kinds.len() {
                0 => None,
                1 => Some(kinds[0]),
                _ if kinds.iter().all(numeric) || kinds.contains(&ColumnType::Any) => {
                    Some(ColumnType::Any)
                }
                _ => {
                    let names: Vec<String> = kinds.iter().map(|t| t.to_string()).collect();
                    return Err(bind_err(format!(
                        "{name}() cannot order arguments of different types ({}) — sqlite \
                         would rank them by storage class, which is the cross-type comparison \
                         mpedb refuses everywhere else; CAST them to one type",
                        names.join(" and ")
                    )));
                }
            };
            // With exactly one concrete type, a bare parameter adopts it — so
            // `max(?, i)` binds the way `? > i` does. With a mixed call there is
            // nothing to adopt, and the parameter is left for `resolve_params`
            // to report.
            let out = bound
                .into_iter()
                .map(|(e, t)| match (kinds.len(), ret) {
                    (1, Some(w)) => self.unify_param(e, t, w).0,
                    _ => e,
                })
                .collect();
            let f = if name == "max" { ScalarFn::Max2 } else { ScalarFn::Min2 };
            // NEEDCOLL (probed against the bundled 3.45.0): the comparison runs
            // under the collation of the first argument, left to right, that
            // DEFINES one — a bare column (its declared collation; a plain
            // BINARY column defines BINARY and STOPS the search), descending
            // through CAST as sqlite's `sqlite3ExprCollSeq` does. Literals and
            // computed arguments define none. Binary keeps the plain opcode,
            // so uncollated plans are byte-identical.
            let coll = args
                .iter()
                .find_map(|a| self.defining_collation(a))
                .unwrap_or(Collation::Binary);
            if coll != Collation::Binary {
                return Ok((BExpr::CallColl(f, out, coll), ret));
            }
            return Ok((BExpr::Call(f, out), ret));
        }
        let f = match name {
            "lower" => ScalarFn::Lower,
            "upper" => ScalarFn::Upper,
            "length" => ScalarFn::Length,
            "trim" => ScalarFn::Trim,
            "abs" => ScalarFn::Abs,
            "round" => ScalarFn::Round,
            "substr" | "substring" => ScalarFn::Substr,
            "replace" => ScalarFn::Replace,
            "ltrim" => ScalarFn::Ltrim,
            "rtrim" => ScalarFn::Rtrim,
            "instr" => ScalarFn::Instr,
            "sqrt" => ScalarFn::Sqrt,
            "pow" | "power" => ScalarFn::Pow,
            "sign" => ScalarFn::Sign,
            "ceil" | "ceiling" => ScalarFn::Ceil,
            "floor" => ScalarFn::Floor,
            "trunc" => ScalarFn::Trunc,
            "unicode" => ScalarFn::Unicode,
            "hex" => ScalarFn::Hex,
            "typeof" => ScalarFn::Typeof,
            // sqlite built-ins added for the Django/C-API surface: `quote(X)`
            // (Django's `last_executed_query` calls `QUOTE(?)` per parameter)
            // and `strftime(FORMAT, TIME)`.
            "quote" => ScalarFn::Quote,
            "strftime" => ScalarFn::Strftime,
            // The rest of sqlite's date/time family. All four share
            // `strftime`'s time-string grammar and its refusals; the only
            // difference is the fixed output format (and `julianday`'s REAL).
            "date" => ScalarFn::Date,
            "time" => ScalarFn::Time,
            "datetime" => ScalarFn::DateTime,
            "julianday" => ScalarFn::JulianDay,
            // Vector distance over f32-LE blob embeddings (stage D,
            // design/DESIGN-MPEE-GENERAL.md). No sqlite counterpart — the
            // differential oracle has nothing to say here, so the runtime's
            // own shape refusals are the specification.
            "vec_l2" => ScalarFn::VecL2,
            "vec_cosine" => ScalarFn::VecCosine,
            "splice" => ScalarFn::Splice,
            // Math (sqlite 3.45). `log` is base-10 with one argument and
            // log-base-b with two, so it dispatches on the argument count here —
            // `log10`/`log2` name the fixed-base forms directly.
            "exp" => ScalarFn::Exp,
            "ln" => ScalarFn::Ln,
            "log10" => ScalarFn::Log10,
            "log2" => ScalarFn::Log2,
            "log" => match args.len() {
                1 => ScalarFn::Log10,
                2 => ScalarFn::LogBase,
                n => {
                    return Err(bind_err(format!(
                        "log() takes 1 argument (base-10) or 2 (log(base, x)), got {n}"
                    )))
                }
            },
            "sin" => ScalarFn::Sin,
            "cos" => ScalarFn::Cos,
            "tan" => ScalarFn::Tan,
            "asin" => ScalarFn::Asin,
            "acos" => ScalarFn::Acos,
            "atan" => ScalarFn::Atan,
            "atan2" => ScalarFn::Atan2,
            "sinh" => ScalarFn::Sinh,
            "cosh" => ScalarFn::Cosh,
            "tanh" => ScalarFn::Tanh,
            "radians" => ScalarFn::Radians,
            "degrees" => ScalarFn::Degrees,
            "pi" => ScalarFn::Pi,
            "mod" => ScalarFn::Mod,
            // A name that matches no native scalar (nor an aggregate — those are
            // lifted before binding) may still be a HOST-registered UDF (the
            // C-API `create_function` path, design/DESIGN-UDF.md). A host UDF is
            // dynamically typed: bind every argument through unchanged (no
            // pinning) and grade the result to `Any`. A name matching neither is
            // the unchanged "unknown function" error.
            other => {
                // Stored functions resolve BEFORE host UDFs: a definition in
                // the FILE outranks a per-connection closure, and the order is
                // load-bearing — the stored plan is shareable, the host plan
                // is not, and two connections must not disagree about which
                // one a name means.
                if let Some((hash, argc)) = self.host_udfs.spells.resolve(other) {
                    if args.len() != argc as usize {
                        return Err(bind_err(format!(
                            "{other}() takes {argc} argument(s), got {}",
                            args.len()
                        )));
                    }
                    let mut bound = Vec::with_capacity(args.len());
                    for a in args {
                        bound.push(self.bind_expr(a)?.0);
                    }
                    return Ok((
                        BExpr::SpellCall { hash, args: bound },
                        Some(ColumnType::Any),
                    ));
                }
                if self.host_udfs.resolves(other, args.len()) {
                    if u16::try_from(args.len()).is_err() {
                        return Err(bind_err(format!(
                            "{other}() called with too many arguments"
                        )));
                    }
                    let mut bound = Vec::with_capacity(args.len());
                    for a in args {
                        bound.push(self.bind_expr(a)?.0);
                    }
                    return Ok((
                        BExpr::HostCall {
                            name: other.to_string(),
                            args: bound,
                        },
                        Some(ColumnType::Any),
                    ));
                }
                return Err(bind_err(format!(
                    "unknown function `{other}()`; available: lower, upper, length, trim, \
                     ltrim, rtrim, replace, instr, substr, substring, char, unicode, hex, \
                     typeof, abs, round, ceil, floor, trunc, sqrt, pow, sign, exp, ln, log, \
                     log10, log2, sin, cos, tan, asin, acos, atan, atan2, sinh, cosh, tanh, \
                     radians, degrees, pi, mod, printf, format, quote, strftime, date, \
                     time, datetime, julianday, json, \
                     json_valid, json_type, json_quote, json_array_length, json_extract, \
                     json_array, json_object, json_patch, json_remove, json_replace, \
                     json_set, json_insert, iif, coalesce, ifnull, nullif"
                )));
            }
        };
        // The five math functions whose DOMAIN error PostgreSQL raises and
        // sqlite answers NULL for. Rewritten here, once, on the resolved
        // function rather than on the name — so a future alias (`log` for
        // `ln`, say) cannot pick up the sqlite form by spelling.
        //
        // `pow`/`exp` are deliberately NOT here: sqlite returns Inf and so
        // does PostgreSQL, so there is nothing to make strict. Adding them
        // would turn agreement into refusal, which is the failure mode a
        // compatibility change invites.
        let f = if self.dialect == Dialect::Postgres {
            match f {
                ScalarFn::Sqrt => ScalarFn::SqrtStrict,
                ScalarFn::Ln => ScalarFn::LnStrict,
                ScalarFn::Log10 => ScalarFn::Log10Strict,
                ScalarFn::Log2 => ScalarFn::Log2Strict,
                ScalarFn::LogBase => ScalarFn::LogBaseStrict,
                other => other,
            }
        } else {
            f
        };
        // Which argument of this function is sqlite's TIMESTRING (the only
        // position a `'now'` can occupy — every later argument is a modifier,
        // and modifiers are refused wholesale).
        let time_arg: Option<usize> = match f {
            ScalarFn::Strftime => Some(1),
            ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime | ScalarFn::JulianDay => Some(0),
            _ => None,
        };
        let mut bound = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            // A LITERAL `'now'` in the time-value position binds the
            // STATEMENT-START instant: it is rewritten into the reserved
            // instant slot, which the facade fills once per execute (sqlite's
            // `iCurrentTime` rule — one instant per statement, so two `'now'`s
            // in one statement read the SAME slot and agree). The plan itself
            // carries only a parameter reference, so a content-hashed plan
            // shared across processes can never carry a compile-time clock.
            if time_arg == Some(i) && is_literal_now(a) {
                bound.push((self.statement_instant()?, Some(ColumnType::Text)));
                continue;
            }
            bound.push(self.bind_expr(a)?);
        }
        let argc = u8::try_from(bound.len())
            .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
        if !f.arity_ok(argc) {
            return Err(bind_err(format!(
                "{name}() cannot take {argc} argument(s)"
            )));
        }
        // Pin each argument's type where the function demands one, so a bare
        // `$1` gets a type and a wrong type is a COMPILE error rather than a
        // per-row surprise.
        let (want, ret): (&[Option<ColumnType>], Ty) = match f {
            ScalarFn::Lower | ScalarFn::Upper => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            // length/unicode: text in, integer out.
            ScalarFn::Length | ScalarFn::Unicode => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Int64))
            }
            // abs/round/ceil/floor/trunc keep their argument's numeric type, so
            // they are checked below rather than pinned to one.
            ScalarFn::Abs | ScalarFn::Round | ScalarFn::Ceil | ScalarFn::Floor
            | ScalarFn::Trunc => (&[], None),
            // Checked below (blob-or-any per argument, Float64 out).
            ScalarFn::VecL2 | ScalarFn::VecCosine => (&[], Some(ColumnType::Float64)),
            // splice(x, at, remove, insert) — the value's type comes from `x`,
            // so it is checked below rather than pinned. The offsets are
            // integers and pinned here.
            ScalarFn::Splice => (
                &[None, Some(ColumnType::Int64), Some(ColumnType::Int64), None],
                None,
            ),
            ScalarFn::Substr => (
                &[Some(ColumnType::Text), Some(ColumnType::Int64), Some(ColumnType::Int64)],
                Some(ColumnType::Text),
            ),
            ScalarFn::Replace => (
                &[Some(ColumnType::Text), Some(ColumnType::Text), Some(ColumnType::Text)],
                Some(ColumnType::Text),
            ),
            // trim/ltrim/rtrim: text, and an optional text set of trim chars.
            ScalarFn::Trim | ScalarFn::Ltrim | ScalarFn::Rtrim => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::Instr => {
                (&[Some(ColumnType::Text), Some(ColumnType::Text)], Some(ColumnType::Int64))
            }
            // sqrt/pow and the transcendental math functions take numbers (int
            // or float, unpinned like abs/round) but ALWAYS return a float; `pi`
            // is nullary and also returns a float. sign always returns an integer.
            // `format_type(oid, typmod)`: an integer oid and an integer
            // modifier in, a type NAME out. Listed here so the arity/type
            // table stays the one place a scalar's shape is declared, even
            // though only `pg::funcs` can reach it.
            ScalarFn::FormatType => (&[], Some(ColumnType::Text)),
            // Returns an ARRAY, which has no `ColumnType` — `Any`, like every
            // other value typed per row. Its argument is unconstrained: the
            // check that it IS an array happens where the array is read.
            ScalarFn::Subscripts => (&[], Some(ColumnType::Any)),
            ScalarFn::Sqrt
            | ScalarFn::SqrtStrict
            | ScalarFn::LnStrict
            | ScalarFn::Log10Strict
            | ScalarFn::Log2Strict
            | ScalarFn::LogBaseStrict
            | ScalarFn::Pow
            | ScalarFn::Exp
            | ScalarFn::Ln
            | ScalarFn::Log10
            | ScalarFn::Log2
            | ScalarFn::LogBase
            | ScalarFn::Sin
            | ScalarFn::Cos
            | ScalarFn::Tan
            | ScalarFn::Asin
            | ScalarFn::Acos
            | ScalarFn::Atan
            | ScalarFn::Atan2
            | ScalarFn::Sinh
            | ScalarFn::Cosh
            | ScalarFn::Tanh
            | ScalarFn::Radians
            | ScalarFn::Degrees
            | ScalarFn::Pi
            | ScalarFn::Mod => (&[], Some(ColumnType::Float64)),
            ScalarFn::Sign => (&[], Some(ColumnType::Int64)),
            // hex accepts text OR blob — two types the fixed `want` table
            // cannot express — so its argument is left unpinned and checked in
            // the `ret` recomputation below. typeof accepts ANY type. Both
            // return text.
            ScalarFn::Hex | ScalarFn::Typeof => (&[], Some(ColumnType::Text)),
            // quote(X) accepts EVERY type (that is the point of it) and returns
            // text. Its argument stays unpinned so `quote($1)` — the shape
            // Django's `last_executed_query` emits — binds without the binder
            // having to guess the parameter's type.
            ScalarFn::Quote => (&[], Some(ColumnType::Text)),
            // strftime(FORMAT, TIMESTRING): both text, text out. Pinning the
            // time argument to text is what makes `strftime('%Y', 2455352.5)`
            // — sqlite's Julian-day form — a COMPILE error rather than a
            // per-row surprise.
            ScalarFn::Strftime => (
                &[Some(ColumnType::Text), Some(ColumnType::Text)],
                Some(ColumnType::Text),
            ),
            // date/time/datetime(TIMESTRING): text in, text out — same pin, and
            // for the same reason (the Julian-day NUMBER form is a compile
            // error, not a per-row surprise). julianday returns sqlite's REAL.
            ScalarFn::Date | ScalarFn::Time | ScalarFn::DateTime => {
                (&[Some(ColumnType::Text)], Some(ColumnType::Text))
            }
            ScalarFn::JulianDay => (&[Some(ColumnType::Text)], Some(ColumnType::Float64)),
            // char/printf and the scalar max/min are variadic and bound
            // specially above (never reached here); present only so this match
            // stays exhaustive over ScalarFn.
            ScalarFn::Char | ScalarFn::Printf => (&[], Some(ColumnType::Text)),
            // The whole JSON family is bound by `bind_json_call` (and the two
            // accessors by `bind_binary`), so none of these reach the generic
            // path; the arm exists only to keep the match exhaustive.
            ScalarFn::Json
            | ScalarFn::JsonValid
            | ScalarFn::JsonType
            | ScalarFn::JsonQuote
            | ScalarFn::JsonArrayLength
            | ScalarFn::JsonExtract
            | ScalarFn::JsonArrow
            | ScalarFn::JsonArrowText
            | ScalarFn::JsonArray
            | ScalarFn::JsonObject
            | ScalarFn::JsonPatch
            | ScalarFn::JsonRemove
            | ScalarFn::JsonReplace
            | ScalarFn::JsonSet
            | ScalarFn::JsonInsert
            | ScalarFn::JsonQuoteExtract => (&[], Some(ColumnType::Text)),
            ScalarFn::Max2 | ScalarFn::Min2 => (&[], None),
        };
        let mut out = Vec::with_capacity(bound.len());
        for (i, (e, t)) in bound.into_iter().enumerate() {
            match want.get(i).copied().flatten() {
                Some(w) => {
                    let (e, t) = self.unify_param(e, t, w);
                    if let Some(t) = t {
                        // A DYNAMICALLY typed argument (`any` — a typeless
                        // column, a host UDF, a per-row CASE) passes: its class
                        // is not known until the row is read, and the runtime
                        // implementation checks the value it actually gets.
                        // That is narrower than sqlite (which coerces
                        // `length(123)` to 3) but the narrowing is a refusal at
                        // the row, never a different value — and refusing at
                        // COMPILE time refused the whole query for values that
                        // are of the right class every time.
                        if t != w && t != ColumnType::Any {
                            return Err(bind_err(format!(
                                "{name}() argument {} must be {w}, got {t}",
                                i + 1
                            )));
                        }
                    }
                    out.push(e);
                }
                None => out.push(e),
            }
        }
        let ret = match f {
            // `round()` is sqlite's one numeric function that does NOT keep the
            // argument's type: it always answers a REAL (`round(7)` is `7.0`).
            ScalarFn::Round => match self.static_type(&out[0]) {
                Some(ColumnType::Int64)
                | Some(ColumnType::Float64)
                | Some(ColumnType::Any)
                | None => Some(ColumnType::Float64),
                Some(other) => {
                    return Err(bind_err(format!("{name}() expects a number, got {other}")))
                }
            },
            ScalarFn::Abs | ScalarFn::Ceil | ScalarFn::Floor | ScalarFn::Trunc => {
                // Numeric in, same numeric out. The type is the argument's —
                // and a DYNAMICALLY typed argument (`any`) keeps `any`: the
                // runtime already preserves int-ness per value (sqlite's
                // `floor(7)` is the integer 7, `floor(7.5)` the real 7.0), and
                // refuses a non-number at the row it meets one.
                let t = self.static_type(&out[0]);
                match t {
                    Some(ColumnType::Int64)
                    | Some(ColumnType::Float64)
                    | Some(ColumnType::Any)
                    | None => t,
                    Some(other) => {
                        return Err(bind_err(format!("{name}() expects a number, got {other}")))
                    }
                }
            }
            // Both arguments must be blobs (or dynamically typed); anything
            // else is refused at COMPILE time. The runtime still owns the
            // shape rules the type system cannot see (f32 alignment, equal
            // dimensionality).
            ScalarFn::VecL2 | ScalarFn::VecCosine => {
                for (i, arg) in out.iter().enumerate().take(2) {
                    match self.static_type(arg) {
                        Some(ColumnType::Blob) | Some(ColumnType::Any) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "{name}() argument {} must be a blob embedding, got {other}",
                                i + 1
                            )))
                        }
                    }
                }
                Some(ColumnType::Float64)
            }
            // splice() returns whatever it was given: text in, text out. The
            // value and the insert must agree in kind, and both must be
            // text-or-blob — checked here so a `splice(int_col, …)` is refused
            // at compile time rather than at the first row.
            ScalarFn::Splice => {
                let mut out_ty = None;
                for (i, idx) in [0usize, 3].into_iter().enumerate() {
                    match self.static_type(&out[idx]) {
                        Some(t @ (ColumnType::Text | ColumnType::Blob)) => {
                            if i == 0 {
                                out_ty = Some(t);
                            }
                        }
                        Some(ColumnType::Any) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "splice() argument {} must be text or blob, got {other}",
                                idx + 1
                            )))
                        }
                    }
                }
                out_ty
            }
            // hex accepts text or blob (like the runtime); reject anything else
            // at COMPILE time rather than at the first row.
            ScalarFn::Hex => match self.static_type(&out[0]) {
                Some(ColumnType::Text)
                | Some(ColumnType::Blob)
                | Some(ColumnType::Any)
                | None => Some(ColumnType::Text),
                Some(other) => {
                    return Err(bind_err(format!("hex() expects text or blob, got {other}")))
                }
            },
            _ => ret,
        };
        Ok((BExpr::Call(f, out), ret))
    }

    /// Bind one of sqlite's JSON functions, or `Ok(None)` if `name` is not one
    /// (so a host UDF called `jsonify()` still resolves normally).
    ///
    /// # The JSON subtype, and why it is decided HERE
    ///
    /// sqlite has no JSON type, but it does mark a *value* with an internal
    /// `JSON` subtype whenever a JSON function produced it, and the functions
    /// that take VALUE arguments read that mark:
    ///
    /// ```text
    /// json_object('a', json('[1,2]'))  ->  {"a":[1,2]}     -- spliced raw
    /// json_object('a',      '[1,2]' )  ->  {"a":"[1,2]"}   -- quoted as text
    /// ```
    ///
    /// mpedb's `Value` carries no subtype, and adding one would mean threading
    /// a flag through the whole expression stack. It does not have to: sqlite
    /// sets that mark in exactly one place — the return of a JSON function —
    /// and mpedb can see, at BIND time, whether an argument *is* such a call.
    /// So the binder computes a bitmask of which value arguments are JSON and
    /// prepends it as a hidden leading argument (see `ScalarFn::JsonArray`).
    ///
    /// The three shapes where a static answer could differ from sqlite's
    /// runtime one are REFUSED by name rather than guessed:
    ///
    /// * `json_extract(…)` / `->>` in a value position — sqlite subtypes
    ///   `json_extract`'s result only when the extracted node is an object or
    ///   an array, which is a property of the DATA, not of the query;
    /// * a scalar subquery — sqlite propagates the subtype out of one
    ///   (`json_quote((SELECT json('[1]')))` is `[1]`) but not out of a FROM
    ///   subquery's column, an aggregate, or `||`; mpedb cannot see through
    ///   the subplan boundary to tell those apart;
    /// * a `CASE`/`coalesce`/`iif` whose arms DISAGREE — sqlite's answer is
    ///   whichever arm fires.
    pub(super) fn bind_json_call(
        &mut self,
        name: &str,
        args: &[ast::Expr],
    ) -> Result<Option<(BExpr, Ty)>> {
        // The table-valued and aggregate JSON functions are a different
        // machinery entirely; name them rather than report "unknown function".
        if matches!(
            name,
            "json_each" | "json_tree" | "json_group_array" | "json_group_object"
        ) {
            return Err(bind_err(format!(
                "{name}() is not implemented: `json_each`/`json_tree` are TABLE-VALUED \
                 functions and `json_group_array`/`json_group_object` are AGGREGATES, neither \
                 of which mpedb's scalar-function machinery can express"
            )));
        }
        if name.starts_with("jsonb") {
            return Err(bind_err(format!(
                "{name}() is not implemented: sqlite 3.45's JSONB is a BINARY encoding stored \
                 in a BLOB, and mpedb implements the TEXT JSON functions only"
            )));
        }
        // Fixed-shape readers: no value arguments, so no subtype question.
        let simple = match name {
            "json" => Some((ScalarFn::Json, ColumnType::Text)),
            "json_valid" => Some((ScalarFn::JsonValid, ColumnType::Int64)),
            "json_type" => Some((ScalarFn::JsonType, ColumnType::Text)),
            "json_array_length" => Some((ScalarFn::JsonArrayLength, ColumnType::Int64)),
            // One path unwraps to whatever the node holds; several wrap into a
            // JSON array (text). `Any` covers both.
            "json_extract" => Some((ScalarFn::JsonExtract, ColumnType::Any)),
            "json_patch" => Some((ScalarFn::JsonPatch, ColumnType::Text)),
            "json_remove" => Some((ScalarFn::JsonRemove, ColumnType::Text)),
            _ => None,
        };
        if let Some((f, ret)) = simple {
            let argc = u8::try_from(args.len())
                .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
            if !f.arity_ok(argc) {
                return Err(bind_err(format!(
                    "{name}() cannot take {argc} argument(s)"
                )));
            }
            let mut out = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                let (e, t) = self.bind_expr(a)?;
                // Argument 0 is the document, and every later argument is a
                // path — except `json_valid`'s FLAGS, which is an integer.
                let want = if i == 1 && f == ScalarFn::JsonValid {
                    ColumnType::Int64
                } else {
                    ColumnType::Text
                };
                let (e, t) = self.unify_param(e, t, want);
                match t {
                    Some(t) if t == want => {}
                    // `json_valid` accepts ANY type for its document argument
                    // (sqlite answers 1 for a number, 0 for a blob), and `Any`
                    // is decided per value at runtime.
                    Some(ColumnType::Any) | None => {}
                    Some(_) if i == 0 && f == ScalarFn::JsonValid => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "{name}() argument {} must be {want}, got {other}",
                            i + 1
                        )))
                    }
                }
                out.push(e);
            }
            return Ok(Some((BExpr::Call(f, out), Some(ret))));
        }
        // `json_quote(X)`: an argument that is ALREADY JSON is returned
        // unchanged by sqlite (its subtype survives), and every JSON-producing
        // call already yields minified JSON text — so the whole call is that
        // argument. Nothing to encode, no mask.
        if name == "json_quote" {
            if args.len() != 1 {
                return Err(bind_err(format!(
                    "json_quote() takes exactly 1 argument, got {}",
                    args.len()
                )));
            }
            // `json_quote(json_extract(D, P…))` is the one composition whose
            // answer is value-dependent — sqlite subtypes an extract only when
            // the extracted node is an object or an array — and it is the shape
            // consumers actually write (`CAST(JSON_QUOTE(JSON_EXTRACT(x, p)) AS
            // VARCHAR)`). The pair is emitted as ONE call, evaluated where the
            // node type is still in hand. Asking `json_ness` first would refuse
            // it, so this is checked BEFORE.
            if let ast::Expr::Func(inner, iargs) = &args[0] {
                if inner.eq_ignore_ascii_case("json_extract") && iargs.len() >= 2 {
                    let mut out = Vec::with_capacity(iargs.len());
                    for a in iargs {
                        let (e, t) = self.bind_expr(a)?;
                        let (e, _) = self.unify_param(e, t, ColumnType::Text);
                        out.push(e);
                    }
                    return Ok(Some((
                        BExpr::Call(ScalarFn::JsonQuoteExtract, out),
                        Some(ColumnType::Text),
                    )));
                }
            }
            if self.json_ness(&args[0], "json_quote()")? {
                let (e, _) = self.bind_expr(&args[0])?;
                return Ok(Some((e, Some(ColumnType::Text))));
            }
            let (e, _) = self.bind_expr(&args[0])?;
            return Ok(Some((
                BExpr::Call(ScalarFn::JsonQuote, vec![e]),
                Some(ColumnType::Text),
            )));
        }
        // The writers: a leading subtype bitmask, then the SQL arguments.
        // `value_at` says which argument positions are VALUES (the rest are
        // documents or paths, always read as JSON/text).
        let f = match name {
            "json_array" => ScalarFn::JsonArray,
            "json_object" => ScalarFn::JsonObject,
            "json_set" => ScalarFn::JsonSet,
            "json_insert" => ScalarFn::JsonInsert,
            "json_replace" => ScalarFn::JsonReplace,
            _ => return Ok(None),
        };
        // ONE table of value positions, shared with the lifter's subquery
        // refusal — the two must never drift apart.
        let value_at = json_value_positions(name).expect("a writer has value positions");
        let mut mask: u64 = 0;
        let mut out: Vec<BExpr> = Vec::with_capacity(args.len() + 1);
        // Placeholder; filled in once the mask is known.
        out.push(BExpr::Const(Value::Int(0)));
        for (i, a) in args.iter().enumerate() {
            match value_at(i) {
                Some(slot) => {
                    if slot >= 64 {
                        return Err(bind_err(format!(
                            "{name}() takes at most 64 value arguments in mpedb: the JSON \
                             subtype of each value is carried as a 64-bit mask on the compiled \
                             call"
                        )));
                    }
                    if self.json_ness(a, &format!("{name}()"))? {
                        mask |= 1u64 << slot;
                    }
                    // A value argument keeps whatever type it has: every SQL
                    // type has a JSON rendering (a BLOB is the one runtime
                    // error, matching sqlite's "JSON cannot hold BLOB values").
                    out.push(self.bind_expr(a)?.0);
                }
                None => {
                    // A document/path/label position: text.
                    let (e, t) = self.bind_expr(a)?;
                    let (e, t) = self.unify_param(e, t, ColumnType::Text);
                    match t {
                        Some(ColumnType::Text) | Some(ColumnType::Any) | None => {}
                        Some(other) => {
                            return Err(bind_err(format!(
                                "{name}() argument {} must be text, got {other}",
                                i + 1
                            )))
                        }
                    }
                    out.push(e);
                }
            }
        }
        out[0] = BExpr::Const(Value::Int(mask as i64));
        let argc = u8::try_from(out.len())
            .map_err(|_| bind_err(format!("{name}() called with too many arguments")))?;
        if !f.arity_ok(argc) {
            return Err(bind_err(format!(
                "{name}() cannot take {} argument(s)",
                args.len()
            )));
        }
        Ok(Some((BExpr::Call(f, out), Some(ColumnType::Text))))
    }

    /// Is `e` an expression sqlite would mark with the JSON subtype? See
    /// [`Self::bind_json_call`] for why this is decidable and what is refused.
    pub(super) fn json_ness(&mut self, e: &ast::Expr, what: &str) -> Result<bool> {
        let undecidable = |why: &str| {
            Err(bind_err(format!(
                "{what}: mpedb cannot tell whether this argument is JSON text or a plain \
                 string, because {why}. sqlite decides it from a per-value JSON subtype that \
                 mpedb's values do not carry. Wrap the argument in `json(…)` to splice it as \
                 JSON, or in `'' || …` to force the quoted-string reading"
            )))
        };
        Ok(match e {
            ast::Expr::Func(name, _) => match name.to_ascii_lowercase().as_str() {
                // Every one of these returns minified JSON text with the
                // subtype set (verified against 3.45.1).
                "json" | "json_array" | "json_object" | "json_insert" | "json_replace"
                | "json_set" | "json_remove" | "json_patch" | "json_quote" => true,
                // Value-dependent: sqlite subtypes json_extract's result only
                // when the node is an object or an array.
                "json_extract" => return undecidable("`json_extract()` is JSON only when the \
                                                      extracted node is an object or an array"),
                _ => false,
            },
            // `->` always yields JSON text; `->>` never does (verified:
            // `json_quote('{\"a\":[9]}' ->> '$.a')` is the quoted `"[9]"`).
            ast::Expr::Binary(BinOp::JsonArrow, _, _) => true,
            ast::Expr::Binary(BinOp::JsonArrowText, _, _) => false,
            // The subtype flows with the value through lazy control flow, so
            // fold over the arms — and refuse when they disagree.
            ast::Expr::Case(arms, else_) => {
                let mut it = arms
                    .iter()
                    .map(|(_, r)| r)
                    .chain(else_.iter().map(|b| b.as_ref()));
                let mut acc: Option<bool> = None;
                for arm in &mut it {
                    // A NULL arm is neither: it cannot be observed either way.
                    if matches!(arm, ast::Expr::Lit(Value::Null)) {
                        continue;
                    }
                    let j = self.json_ness(arm, what)?;
                    match acc {
                        None => acc = Some(j),
                        Some(prev) if prev == j => {}
                        Some(_) => {
                            return undecidable(
                                "its CASE arms disagree — some are JSON, some are plain text",
                            )
                        }
                    }
                }
                acc.unwrap_or(false)
            }
            ast::Expr::Coalesce(items) => {
                let mut acc: Option<bool> = None;
                for it in items {
                    if matches!(it, ast::Expr::Lit(Value::Null)) {
                        continue;
                    }
                    let j = self.json_ness(it, what)?;
                    match acc {
                        None => acc = Some(j),
                        Some(prev) if prev == j => {}
                        Some(_) => {
                            return undecidable(
                                "its coalesce/ifnull arms disagree — some are JSON, some are \
                                 plain text",
                            )
                        }
                    }
                }
                acc.unwrap_or(false)
            }
            ast::Expr::Subquery(_) => {
                return undecidable(
                    "it is a scalar subquery, and sqlite propagates the subtype out of one but \
                     not out of a FROM-subquery column or an aggregate",
                )
            }
            // Everything else — a literal, a column, a parameter, `||`, CAST,
            // a non-JSON function, a host UDF — carries no subtype in sqlite
            // either, so plain text is the exact answer.
            _ => false,
        })
    }

    /// The type of an already-bound expression, where it is knowable without
    /// re-binding. Used for the functions whose return type is their argument's.
    pub(super) fn static_type(&self, e: &BExpr) -> Ty {
        match e {
            BExpr::Const(v) => v.column_type(),
            // A column reference resolves through the WHOLE evaluated tuple —
            // `Scope::column_shape` walks the scoped tables in slot order, the
            // same walk `Scope::resolve` used to hand out the slot.
            //
            // This used to read `scope.only().columns[…]`, which ASSERTS on a
            // scope wider than one table: `SELECT a.id FROM a JOIN b ON … WHERE
            // ABS(b.id) = 1` panicked in the binder, because `abs`/`round`/
            // `ceil`/`floor`/`trunc`/`hex` are the functions whose return type
            // IS their argument's, so binding one over a joined column came
            // through here. The scope was never single-table on this path; only
            // the lookup assumed it was.
            //
            // `excluded.<c>` binds to Col(n + i) over `[existing ‖ proposed]`,
            // which is the one tuple WIDER than the scope: fold the index back
            // into the scope's width so a second-half reference reports the
            // column's real type instead of falling off the end. That scope is
            // single-table by construction (an ON CONFLICT target is one
            // table), so the fold and the join walk never interact.
            BExpr::Col(i) => {
                let n = self.scope.width();
                let slot = (*i as usize % n.max(1)) as u16;
                self.scope.column_shape(slot).map(|(t, _)| t)
            }
            BExpr::Param(i) => self.param_types[*i as usize],
            BExpr::Unary(BUnOp::ToFloat, _) => Some(ColumnType::Float64),
            BExpr::Call(
                ScalarFn::Length | ScalarFn::Instr | ScalarFn::Sign | ScalarFn::Unicode,
                _,
            ) => Some(ColumnType::Int64),
            // sqrt/pow, the transcendental math functions, and nullary pi are
            // always float.
            BExpr::Call(
                ScalarFn::Sqrt
                | ScalarFn::SqrtStrict
                | ScalarFn::LnStrict
                | ScalarFn::Log10Strict
                | ScalarFn::Log2Strict
                | ScalarFn::LogBaseStrict
                | ScalarFn::Pow
                | ScalarFn::Exp
                | ScalarFn::Ln
                | ScalarFn::Log10
                | ScalarFn::Log2
                | ScalarFn::LogBase
                | ScalarFn::Sin
                | ScalarFn::Cos
                | ScalarFn::Tan
                | ScalarFn::Asin
                | ScalarFn::Acos
                | ScalarFn::Atan
                | ScalarFn::Atan2
                | ScalarFn::Sinh
                | ScalarFn::Cosh
                | ScalarFn::Tanh
                | ScalarFn::Radians
                | ScalarFn::Degrees
                | ScalarFn::Pi
                | ScalarFn::Mod,
                _,
            ) => Some(ColumnType::Float64),
            BExpr::Call(
                ScalarFn::Abs
                | ScalarFn::Round
                | ScalarFn::Ceil
                | ScalarFn::Floor
                | ScalarFn::Trunc,
                a,
            ) => self.static_type(&a[0]),
            BExpr::Call(_, _) => Some(ColumnType::Text),
            // Only the scalar min/max carry a collation, and only when a TEXT
            // column argument supplied it — typed like the plain-Call fallback.
            BExpr::CallColl(..) => Some(ColumnType::Text),
            BExpr::IsDistinct(..) => Some(ColumnType::Bool),
            _ => None,
        }
    }

    /// Type the RESULT arms of a CASE / COALESCE (and their sugar: ifnull,
    /// iif, nullif) — arms whose value IS the result. sqlite types these per
    /// ROW: the arm actually taken keeps its own type, so
    /// `COALESCE(30, avg(x)) / 35` divides an INTEGER when arm 1 wins.
    /// Widening 30 to 30.0 is therefore a WRONG ANSWER factory (measured: 82
    /// in the sqllogictest expr tree when it was tried), so **no arm is ever
    /// coerced here**. Instead:
    ///
    ///  * zero or one concrete arm type -> that type, exactly as before; a
    ///    bare parameter adopts it ([`Self::unify_many`], whose int->float
    ///    widening is unreachable with a single kind).
    ///  * a NUMERIC mix (int64 ∪ float64), or any arm already `any` -> every
    ///    arm keeps its own type AND its own value, and the result is typed
    ///    per VALUE at runtime: [`ColumnType::Any`]. The CASE/COALESCE
    ///    runtime is pure control flow (the winning arm's value is returned
    ///    untouched), so the per-row semantics are exact; every downstream
    ///    consumer of `any` already exists — `typeof()` reads the value,
    ///    comparison unification admits `any` ([`Self::unify_operands`]),
    ///    arithmetic settles per value, ORDER BY uses `Value::sort_cmp`,
    ///    DISTINCT/GROUP BY key via `encode_group_key`, and sum/avg/min/max
    ///    accumulate mixed int/float exactly as sqlite does. This is the
    ///    same rule, for the same selection-not-computation reason, as
    ///    scalar `max()`/`min()`.
    ///  * any other mix (text ∪ int64, blob ∪ text, bool/timestamp ∪
    ///    anything) -> still refused with the CAST fix in the message.
    ///    sqlite legalizes those too, but the mpedb runtime refuses a
    ///    cross-CLASS comparison rather than rank number-vs-text, so an
    ///    `any` holding such a mix invites runtime refusals downstream —
    ///    and mpedb's own bool/timestamp have no sqlite storage class at
    ///    all. No measured corpus record needs them (design/CORPUS-STATUS.md).
    ///
    /// sqlite dialect ONLY ([`Self::sqlite_dialect`], like `coerce_bool_ctx`
    /// and `like_glob_operand`): PostgreSQL types COALESCE/CASE statically by
    /// promoting the arms to their common numeric supertype, so
    /// `COALESCE(30, 1.5) / 35` is NUMERIC division in PG (≈0.857) where the
    /// per-row rule divides integers (0). Under the postgres dialect the
    /// original rigid refusal is kept — a clean error, never either engine's
    /// wrong answer.
    ///
    /// Comparison unification (`unify_many` for IN lists) keeps the int ->
    /// float widening: there the widened value only feeds a comparison, and
    /// a comparison's TYPE cannot leak.
    pub(super) fn unify_result_arms(
        &mut self,
        operands: Vec<(BExpr, Ty)>,
        verb: &str,
    ) -> Result<(Vec<BExpr>, Ty)> {
        let mut kinds: Vec<ColumnType> = Vec::new();
        for (_, t) in &operands {
            if let Some(t) = *t {
                if !kinds.contains(&t) {
                    kinds.push(t);
                }
            }
        }
        if kinds.len() <= 1 {
            return self.unify_many(operands, verb);
        }
        // Every type that HAS a sqlite storage class. `Bool` and `Timestamp`
        // are deliberately absent: they are mpedb's own, so a per-row rule
        // over them would be inventing semantics rather than reproducing
        // sqlite's, and the rigid refusal stays right for them.
        //
        // TEXT is here because Django's `GeneratedField` compiles `Concat` to
        // `COALESCE("name",'') || COALESCE("rider_id",'')` — an int64/text mix
        // in every generated column built from a text and a numeric field.
        // sqlite types that per row, and `||` consumes the result to TEXT
        // regardless, so the widening does not leak an `Any` into the column.
        let storage_class = |t: &ColumnType| {
            matches!(
                t,
                ColumnType::Int64 | ColumnType::Float64 | ColumnType::Text | ColumnType::Any
            )
        };
        if self.sqlite_dialect()
            && (kinds.iter().all(storage_class) || kinds.contains(&ColumnType::Any))
        {
            // Mixed arms, typed per row. A bare parameter among them has
            // nothing to adopt (there is no one target type), and is left
            // for `resolve_params` to report — same as a mixed max()/min().
            return Ok((
                operands.into_iter().map(|(e, _)| e).collect(),
                Some(ColumnType::Any),
            ));
        }
        let names: Vec<String> = kinds.iter().map(|t| t.to_string()).collect();
        Err(bind_err(format!(
            "cannot {verb}: {} — sqlite would type this per row; \
             add an explicit CAST so every arm is one type",
            names.join(" and ")
        )))
    }

    pub(super) fn unify_many(&mut self, operands: Vec<(BExpr, Ty)>, _verb: &str) -> Result<(Vec<BExpr>, Ty)> {
        // A dynamically-typed operand (`any` — a mixed CASE/COALESCE arm, a
        // host UDF result, a typeless column) unifies with the whole set the
        // way it unifies with one operand in `unify_operands`: nothing is
        // coerced, and the settled type is `any`. The runtime handles the
        // actual pairs — an IN membership runs each element through `sql_cmp`
        // (numeric comparison crosses int/float; a cross-CLASS pair is
        // sqlite's clean FALSE for a literal `IN (…)` list, and a refusal
        // for a host-BOUND list — see `ops::CrossClass`). A bare param adopts
        // `any` (= any value accepted), exactly as it did before this rule
        // when EVERY operand was `any`.
        if operands.iter().any(|(_, t)| *t == Some(ColumnType::Any)) {
            let out = operands
                .into_iter()
                .map(|(e, t)| self.unify_param(e, t, ColumnType::Any).0)
                .collect();
            return Ok((out, Some(ColumnType::Any)));
        }
        // Target type = the one every non-param operand agrees on, widened to
        // Float64 if ints and floats are mixed.
        let mut target: Ty = None;
        for (_, t) in &operands {
            let Some(t) = *t else { continue };
            target = Some(match target {
                None => t,
                Some(prev) if prev == t => prev,
                Some(ColumnType::Int64) if t == ColumnType::Float64 => ColumnType::Float64,
                Some(ColumnType::Float64) if t == ColumnType::Int64 => ColumnType::Float64,
                // Mixed non-numeric classes (text vs int64, …): settle to `any`
                // so membership runs at runtime under class order / numeric
                // compare (sqlite). Django's injection probe is
                // `name IN (num_chairs + '…')` — text probe, int expression.
                Some(_) => ColumnType::Any,
            });
        }
        let Some(target) = target else {
            // Nothing pinned the type (all NULLs / bare params). Leave them be;
            // resolve_params reports an unresolved param.
            return Ok((operands.into_iter().map(|(e, _)| e).collect(), None));
        };
        let mut out = Vec::with_capacity(operands.len());
        for (e, t) in operands {
            let (e, t) = self.unify_param(e, t, target);
            out.push(match t {
                Some(ColumnType::Int64) if target == ColumnType::Float64 => {
                    fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(e)), self.suppress_fold)?
                }
                _ => e,
            });
        }
        Ok((out, Some(target)))
    }

    /// sqlite's TRUTHINESS: coerce a non-boolean value that stands in a
    /// **boolean context** (WHERE/HAVING/ON/FILTER, `NOT`, `AND`/`OR`,
    /// `CASE WHEN`, `CHECK`) into a bool. Django writes `WHERE "tbl"."flag"`
    /// for a `BooleanField` and binds `True` as the integer 1, so a rigid
    /// refusal here is the single largest sqlite-compat gap.
    ///
    /// The rule is taken from the sqlite binary (3.45.1), not from intuition.
    /// sqlite's `sqlite3VdbeBooleanValue` is: NULL stays unknown, an integer is
    /// `!= 0`, and **everything else is `sqlite3VdbeRealValue(x) != 0.0`** — the
    /// leading-float-prefix parse, applied to text AND to a blob's raw bytes.
    /// Verified against the binary in every boolean position:
    ///
    /// | value | truthy | why |
    /// |---|---|---|
    /// | `2`, `-1`, `0.5` | yes | non-zero |
    /// | `0`, `0.0`, `-0.0` | no | zero |
    /// | `'3abc'`, `'1e3'`, `'.5'`, `' 1 '` | yes | float prefix is non-zero |
    /// | `'abc'`, `'0'`, `'0abc'`, `'0x1'`, `''` | no | float prefix is 0.0 |
    /// | `x'31'` (`"1"`) | yes | blob bytes read as text |
    /// | `x'30'` (`"0"`), `x'00'`, `x''` | no | ditto |
    /// | `NULL` | unknown | 3VL |
    ///
    /// That is EXACTLY [`Affinity::Real`] as `Instr::Cast` already implements
    /// it (`to_real` -> `float_prefix`, itself differential-tested against
    /// sqlite in `crates/mpedb/tests/cast_affinity.rs`), so the whole rule
    /// desugars into instructions that already exist:
    ///
    /// * `int64`   -> `x <> 0`
    /// * `float64` -> `x <> 0.0`      (`-0.0 == 0.0` in `sql_cmp`, so it is FALSE)
    /// * anything else (text, blob, timestamp, `any`) -> `CAST(x AS REAL) <> 0.0`
    ///
    /// No new opcode, therefore **no `PLAN_FORMAT` bump**. `<>` is 3VL, so NULL
    /// propagates and every consumer (WHERE skips the row, `CASE WHEN` takes
    /// ELSE, `NOT NULL` is NULL, Kleene `AND`/`OR`) already behaves like sqlite.
    ///
    /// A bool or a still-unconstrained operand passes through untouched — this
    /// only ever ACCEPTS more, it never changes an answer mpedb already gives.
    /// Under the PostgreSQL dialect (`dialect = "postgres"`) the rigid
    /// refusal is kept, exactly as [`like_glob_operand`] keeps it there.
    pub(crate) fn coerce_bool_ctx(&mut self, e: BExpr, t: Ty) -> Result<(BExpr, Ty)> {
        let src = match t {
            // Already boolean, or nothing to coerce (NULL literal / bare param).
            None | Some(ColumnType::Bool) => return Ok((e, t)),
            Some(src) => src,
        };
        if self.dialect != Dialect::Sqlite {
            return Ok((e, t)); // PostgreSQL: `WHERE 1` stays an error
        }
        let (probe, zero) = match src {
            ColumnType::Int64 => (e, Value::Int(0)),
            ColumnType::Float64 => (e, Value::Float(0.0)),
            // Fold the CAST first, so a constant boolean context (`WHERE 'abc'`)
            // reduces all the way to a Bool const and the planner can see it is
            // dead. `Affinity::Real` never errors, so folding it is always safe.
            _ => (
                fold_maybe(BExpr::Cast(Box::new(e), Affinity::Real), self.suppress_fold)?,
                Value::Float(0.0),
            ),
        };
        let e = fold_maybe(
            BExpr::Binary(BinOp::Ne, Box::new(probe), Box::new(BExpr::Const(zero))),
            self.suppress_fold,
        )?;
        Ok((e, Some(ColumnType::Bool)))
    }

    /// If `e` is a bare parameter with no inferred type yet, pin it to `ty`.
    pub(super) fn unify_param(&mut self, e: BExpr, t: Ty, ty: ColumnType) -> (BExpr, Ty) {
        if t.is_none() {
            if let BExpr::Param(i) = e {
                if self.param_types[i as usize].is_none() {
                    self.param_types[i as usize] = Some(ty);
                    return (e, Some(ty));
                }
            }
        }
        (e, t)
    }
}
