//! Lowering: BExpr -> ExprProgram (compile_program/emit), constant folding,
//! and the free collation/comparison helpers (split from binder.rs; see mod.rs).

use super::*;

/// The SQL spelling of a bitwise operator, for its error messages.
pub(super) fn bit_op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        _ => unreachable!("bit_op_name on {op:?}"),
    }
}

/// Wrap in NOT when the source said `NOT IN`. Deliberately a real `Not` over
/// the 3VL result rather than an inverted membership test: `NOT IN` must yield
/// NULL (not TRUE) when the list holds a NULL and nothing matched, and NOT of
/// NULL is NULL — so the plain negation is exactly right, and reimplementing it
/// would be a second place for the NULL rules to drift.
pub(super) fn maybe_not(e: BExpr, negated: bool) -> BExpr {
    if negated {
        BExpr::Unary(BUnOp::Not, Box::new(e))
    } else {
        e
    }
}

/// Coerce a LIKE/GLOB operand to text the way sqlite does — the SUBJECT, and
/// since #74 (LIKE half) the non-literal PATTERN too (`op = "LIKE pattern"`
/// etc., so a refusal names the right half of the statement). sqlite applies
/// `sqlite3_value_text` to both `likeFunc` operands, so `12 LIKE '1%'` is
/// `'12' LIKE '1%'` and `'12' LIKE 12` is TRUE — a numeric operand is CAST to
/// text (the exact same conversion as `CAST(x AS TEXT)`, which is
/// sqlite-verified) rather than refused. Text stays as-is; a bare parameter
/// (`None`) has already been pinned to Text.
///
/// A statically-typed BLOB is refused by name — and deliberately so, because
/// there is no single "sqlite answer" to match: LIKE-on-blob is BUILD-
/// DEPENDENT. The bundled differential oracle (stock amalgamation defaults)
/// coerces blob bytes as text via `sqlite3_value_text`, while a CLI built
/// with `SQLITE_LIKE_DOESNT_MATCH_BLOBS` (Debian/Ubuntu's, e.g. the 3.45.1
/// on this machine's PATH) answers FALSE for a blob on EITHER side. A
/// runtime blob through an `any` column follows the ORACLE (the repo's
/// acceptance baseline): the CAST bridge reinterprets its bytes as text, and
/// refuses by name the non-UTF-8 bytes a Rust `String` cannot hold.
///
/// `coerce` follows the compat dialect: `true` (sqlite) casts a non-text
/// operand to text; `false` (PostgreSQL) refuses it with mpedb's original
/// rigid error, so `id LIKE '1%'` on an integer column is a bind error rather
/// than a silent stringify. Text and blob handling are identical in both
/// dialects.
pub(super) fn like_glob_operand(l: BExpr, lt: Option<ColumnType>, op: &str, coerce: bool) -> Result<BExpr> {
    match lt {
        None | Some(ColumnType::Text) => Ok(l),
        Some(ColumnType::Blob) => Err(bind_err(format!("{op} requires text, got blob"))),
        Some(_) if coerce => Ok(BExpr::Cast(Box::new(l), Affinity::Text)),
        Some(t) => Err(bind_err(format!("{op} requires text, got {t}"))),
    }
}

/// Constant-fold one node whose children are already folded: if every child
/// is a constant, evaluate now (via the same IR evaluator used at run time,
/// so semantics — including division-by-zero errors — match exactly).
/// The bind-time result type of a non-constant `CAST` to `aff` over a source of
/// type `src`. INTEGER/REAL/TEXT/BLOB are fixed. NUMERIC is the subtle one: an
/// int/real/bool/timestamp source keeps a concrete numeric type (the runtime
/// value is guaranteed to match), but a text/blob source can yield either an
/// int or a real per value, so it is `Any` (mpedb's per-value-typed scalar).
pub(super) fn cast_result_type(aff: Affinity, src: Ty) -> Ty {
    use ColumnType as T;
    Some(match aff {
        Affinity::Integer => T::Int64,
        Affinity::Real => T::Float64,
        Affinity::Text => T::Text,
        Affinity::Blob => T::Blob,
        Affinity::Numeric => match src {
            Some(T::Int64) | Some(T::Bool) | Some(T::Timestamp) => T::Int64,
            Some(T::Float64) => T::Float64,
            // text, blob, or an already-`Any` source → per-value at runtime.
            Some(T::Text) | Some(T::Blob) | Some(T::Any) => T::Any,
            // NULL / untyped-parameter source: no static type.
            None => return None,
        },
    })
}

/// Resolve an ORDER-BY collation NAME to a built-in, or — when the compiling
/// connection registered one under that name — a HOST collation
/// (design/DESIGN-UDF.md stage 3). An unknown name is still the clean bind
/// error, so a typo is caught here rather than at sort time.
///
/// A built-in always wins, exactly as it does for a function name: no
/// registration can redefine BINARY/NOCASE/RTRIM.
pub(crate) fn resolve_order_collation(name: &str, host: &[String]) -> Result<mpedb_types::OrderColl> {
    if let Some(c) = Collation::parse(name) {
        return Ok(mpedb_types::OrderColl::Native(c));
    }
    if let Some(h) = host.iter().find(|h| h.eq_ignore_ascii_case(name)) {
        return Ok(mpedb_types::OrderColl::Host(h.clone()));
    }
    Err(bind_err(format!("no such collation sequence: {name}")))
}

/// [`peel_collate`] for an ORDER BY key, where a HOST collation is legal.
/// Chained `COLLATE`s resolve to the outermost, as there; the shadowed names
/// are still validated (against the built-ins AND the host registrations).
pub(crate) fn peel_order_collate<'a>(
    e: &'a ast::Expr,
    host: &[String],
) -> Result<(&'a ast::Expr, Option<mpedb_types::OrderColl>)> {
    let ast::Expr::Collate(inner, name) = e else {
        return Ok((e, None));
    };
    let coll = resolve_order_collation(name, host)?;
    let mut cur: &ast::Expr = inner;
    while let ast::Expr::Collate(next, n) = cur {
        resolve_order_collation(n, host)?;
        cur = next;
    }
    Ok((cur, Some(coll)))
}

/// Resolve a collation NAME (as written after `COLLATE`) to a built-in, or a
/// clean bind error naming the unsupported collation.
pub(crate) fn resolve_collation(name: &str) -> Result<Collation> {
    Collation::parse(name).ok_or_else(|| bind_err(format!("no such collation sequence: {name}")))
}

/// Peel a top-level explicit `COLLATE` off an AST expression, returning the
/// inner expression and its resolved collation (`None` when there is no
/// `COLLATE`). Chained `COLLATE`s (`x COLLATE A COLLATE B`) resolve to the
/// OUTERMOST — the last one written — matching sqlite; the shadowed inner names
/// are still validated. Any `COLLATE` nested DEEPER than the peeled operand is
/// left in `inner` for [`Binder::bind_expr`] to refuse, so it can never be
/// silently dropped.
pub(crate) fn peel_collate(e: &ast::Expr) -> Result<(&ast::Expr, Option<Collation>)> {
    let ast::Expr::Collate(inner, name) = e else {
        return Ok((e, None));
    };
    let coll = resolve_collation(name)?;
    let mut cur = inner.as_ref();
    while let ast::Expr::Collate(next, n) = cur {
        resolve_collation(n)?;
        cur = next.as_ref();
    }
    Ok((cur, Some(coll)))
}

/// The collation an ORDER BY / comparison key gets from the COLUMN it names,
/// when no explicit `COLLATE` overrides — sqlite's precedence rung 2 ("if either
/// operand is a column, use that column's declared collation"). Returns
/// [`Collation::Binary`] when the key is not a bare column reference (an ordinal,
/// an expression, a literal): those carry no column collation, exactly as in
/// sqlite. A `+col` is NOT treated as a column here (rare; falls back to BINARY,
/// which only differs from sqlite for a `+`-prefixed collated column in ORDER BY).
pub(crate) fn declared_collation(key: &ast::Expr, scope: &Scope) -> Collation {
    let slot = match key {
        ast::Expr::Col(n, _) => scope.resolve(n).ok().map(|(i, _)| i),
        ast::Expr::Qualified(q, n) => scope.resolve_qualified(q, n).ok().map(|(i, _)| i),
        _ => None,
    };
    slot.map(|s| scope.column_collation(s)).unwrap_or(Collation::Binary)
}

/// Is this argument the LITERAL time string `'now'`?
///
/// sqlite's `isDate` compares case-insensitively after skipping leading
/// whitespace, and its own tokenizer has already stripped the quotes — so
/// `' NOW '` is `'now'` there and here. Only a bind-time literal qualifies: a
/// column or parameter whose VALUE happens to be `now` cannot be rewritten into
/// the statement-instant slot and stays refused at runtime (see
/// `mpedb_types::expr::datetime`), because resolving it would need a clock read
/// per row and would drift within one statement.
pub(super) fn is_literal_now(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Lit(Value::Text(s)) if s.trim().eq_ignore_ascii_case("now"))
}

/// Map one of the six comparison `BinOp`s to its collated-instruction kind.
pub(super) fn cmp_kind(op: BinOp) -> CmpKind {
    match op {
        BinOp::Eq => CmpKind::Eq,
        BinOp::Ne => CmpKind::Ne,
        BinOp::Lt => CmpKind::Lt,
        BinOp::Le => CmpKind::Le,
        BinOp::Gt => CmpKind::Gt,
        BinOp::Ge => CmpKind::Ge,
        _ => unreachable!("cmp_kind is only reached for comparison operators"),
    }
}

pub(crate) fn fold(e: BExpr) -> Result<BExpr> {
    let foldable = match &e {
        BExpr::Unary(_, a) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Binary(_, a, b) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::IsDistinct(a, b, _) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        // `'ABC' = 'abc' COLLATE NOCASE` folds to a constant like any other
        // all-const comparison — compile_program emits the CmpColl and eval
        // applies the collation.
        BExpr::CollateCmp(_, a, b, _) => {
            matches!(a.as_ref(), BExpr::Const(_)) && matches!(b.as_ref(), BExpr::Const(_))
        }
        BExpr::InListColl(..) => false,
        BExpr::Like(a, _, _, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Glob(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Regexp(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        BExpr::Cast(a, _) => matches!(a.as_ref(), BExpr::Const(_)),
        // Never foldable: the list is a session value, not a literal.
        BExpr::InParam(..) | BExpr::InParamColl(..) => false,
        // A CASE is branching control flow, not a value-in/value-out node; the
        // fold path evaluates whole programs and has no business here.
        BExpr::Case(..) => false,
        BExpr::Coalesce(..) => false,
        // NEVER folded, and one of the two reasons is load-bearing rather than
        // an economy:
        //
        //  1. **Determinism (the gate).** A compiled plan is CONTENT-HASHED and
        //     published to a registry SHARED ACROSS PROCESSES. Folding a call
        //     whose arguments carry the statement instant would bake a
        //     COMPILE-TIME clock reading into plan bytes that every later
        //     process reuses — a wrong answer that outlives the process that
        //     made it, in a shared file. [`Binder::statement_instant`] already
        //     makes that structurally impossible (the instant is a `Param`, and
        //     a `Param` is not a `Const`, so no `Call` reading it could ever
        //     satisfy an all-const test), but the rule is stated HERE, at the
        //     gate, so that a future "fold all-const calls" optimisation has to
        //     read it before it can be written: **a call is foldable only if
        //     every argument is a `Const`, which the statement instant can never
        //     be.**
        //  2. Economy: folding would have to reproduce `call_scalar`'s NULL
        //     rules here, which is not worth a special case.
        BExpr::Call(..) | BExpr::CallColl(..) => false,
        // Foldable in principle (`2 IN (1,2)` is TRUE), but deliberately not:
        // the fold path evaluates via ExprProgram over a const-only program, and
        // an all-const IN list is not worth a special case. It stays a runtime
        // InList — correct, just not folded.
        BExpr::InList(..) => false,
        _ => false,
    };
    if !foldable {
        return Ok(e);
    }
    let program = compile_program(&e)?;
    let v = program.eval(&[], &[])?;
    Ok(BExpr::Const(v))
}

/// Fold every surviving arm of a CASE.
pub(super) fn fold_arms(arms: Vec<(BExpr, BExpr)>) -> Result<Vec<(BExpr, BExpr)>> {
    let mut out = Vec::with_capacity(arms.len());
    for (c, r) in arms {
        out.push((fold(c)?, fold(r)?));
    }
    Ok(out)
}

/// Fold, unless we are binding a branch that constant control flow may delete
/// unevaluated. See [`Binder::suppress_fold`].
pub(super) fn fold_maybe(e: BExpr, suppressed: bool) -> Result<BExpr> {
    if suppressed {
        Ok(e)
    } else {
        fold(e)
    }
}

/// Compile a bound expression to the shared stack IR.
pub(crate) fn compile_program(e: &BExpr) -> Result<ExprProgram> {
    let mut instrs = Vec::new();
    let mut consts = Vec::new();
    emit(e, &mut instrs, &mut consts)?;
    ExprProgram::new(instrs, consts)
        .map_err(|err| Error::Internal(format!("codegen produced invalid program: {err}")))
}

fn emit(e: &BExpr, instrs: &mut Vec<Instr>, consts: &mut Vec<Value>) -> Result<()> {
    match e {
        BExpr::Const(v) => {
            let idx = push_const(consts, v.clone())?;
            instrs.push(Instr::PushConst(idx));
        }
        BExpr::Param(i) => instrs.push(Instr::PushParam(*i)),
        BExpr::Col(i) => instrs.push(Instr::PushCol(*i)),
        BExpr::Unary(op, a) => {
            emit(a, instrs, consts)?;
            instrs.push(match op {
                BUnOp::Neg => Instr::Neg,
                BUnOp::Not => Instr::Not,
                BUnOp::IsNull => Instr::IsNull,
                BUnOp::IsNotNull => Instr::IsNotNull,
                BUnOp::ToFloat => Instr::ToFloat,
                BUnOp::BitNot => Instr::BitNot,
            });
        }
        BExpr::Cast(a, t) => {
            emit(a, instrs, consts)?;
            instrs.push(Instr::Cast(*t));
        }
        BExpr::ConcatN(ops) => {
            for a in ops {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::ConcatN(ops.len() as u16));
        }
        BExpr::Binary(op, a, b) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(match op {
                BinOp::Add => Instr::Add,
                BinOp::Sub => Instr::Sub,
                BinOp::Mul => Instr::Mul,
                BinOp::Div => Instr::Div,
                BinOp::Mod => Instr::Mod,
                BinOp::Eq => Instr::Eq,
                BinOp::Ne => Instr::Ne,
                BinOp::Lt => Instr::Lt,
                BinOp::Le => Instr::Le,
                BinOp::Gt => Instr::Gt,
                BinOp::Ge => Instr::Ge,
                BinOp::Concat => Instr::Concat,
                BinOp::And => Instr::And,
                BinOp::Or => Instr::Or,
                // `->`/`->>` are bound to `BExpr::Call`, never to a binary
                // node, so no opcode exists (or is needed) for them here.
                BinOp::JsonArrow | BinOp::JsonArrowText => {
                    return Err(bind_err(
                        "internal: a JSON accessor reached the binary emitter",
                    ))
                }
                BinOp::BitAnd => Instr::BitAnd,
                BinOp::BitOr => Instr::BitOr,
                BinOp::Shl => Instr::Shl,
                BinOp::Shr => Instr::Shr,
            });
        }
        BExpr::IsDistinct(a, b, negated) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(if *negated {
                Instr::IsDistinct
            } else {
                Instr::IsNotDistinct
            });
        }
        BExpr::Like(a, pattern, case_insensitive, escape) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            // The dialect chose case-(in)sensitivity at bind time; emit the
            // matching opcode so the plan is self-describing. The ESCAPE
            // character rides in the const pool as a one-character text.
            instrs.push(match escape {
                None if *case_insensitive => Instr::Like(idx),
                None => Instr::LikeCs(idx),
                Some(c) => {
                    let e = push_const(consts, Value::Text(c.to_string()))?;
                    if *case_insensitive {
                        Instr::LikeEsc(idx, e)
                    } else {
                        Instr::LikeCsEsc(idx, e)
                    }
                }
            });
        }
        // The dyn-pattern forms: subject first, pattern on top (popped first).
        // Dialect and escape-ness still select the opcode — the escape rides
        // the const pool exactly as in the literal form; only the pattern is
        // on the stack.
        BExpr::LikeDyn(a, p, case_insensitive, escape) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(match escape {
                None if *case_insensitive => Instr::LikeDyn,
                None => Instr::LikeCsDyn,
                Some(c) => {
                    let e = push_const(consts, Value::Text(c.to_string()))?;
                    if *case_insensitive {
                        Instr::LikeDynEsc(e)
                    } else {
                        Instr::LikeCsDynEsc(e)
                    }
                }
            });
        }
        BExpr::Glob(a, pattern) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            instrs.push(Instr::Glob(idx));
        }
        BExpr::GlobDyn(a, p) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(Instr::GlobDyn);
        }
        BExpr::RegexpDyn(a, p) => {
            emit(a, instrs, consts)?;
            emit(p, instrs, consts)?;
            instrs.push(Instr::RegexpDyn);
        }
        BExpr::Regexp(a, pattern) => {
            emit(a, instrs, consts)?;
            let idx = push_const(consts, Value::Text(pattern.clone()))?;
            instrs.push(Instr::Regexp(idx));
        }
        BExpr::InParam(a, idx) => {
            emit(a, instrs, consts)?;
            instrs.push(Instr::InParam(*idx));
        }
        BExpr::InParamColl(a, idx, coll) => {
            emit(a, instrs, consts)?;
            instrs.push(Instr::InParamColl(*idx, *coll));
        }
        BExpr::Case(arms, else_) => {
            // WHEN c JumpIfNotTrue next; THEN r; Jump end; … ELSE e; end:
            // Targets are patched afterwards because they are forward — which
            // is also exactly what the verifier requires.
            let mut jumps_to_end = Vec::new();
            for (c, r) in arms {
                emit(c, instrs, consts)?;
                let jnt = instrs.len();
                instrs.push(Instr::JumpIfNotTrue(0)); // patched below
                emit(r, instrs, consts)?;
                jumps_to_end.push(instrs.len());
                instrs.push(Instr::Jump(0)); // patched below
                let next_arm = instrs.len();
                patch(instrs, jnt, next_arm)?;
            }
            match else_ {
                Some(e) => emit(e, instrs, consts)?,
                None => {
                    let idx = push_const(consts, Value::Null)?;
                    instrs.push(Instr::PushConst(idx));
                }
            }
            let end = instrs.len();
            for j in jumps_to_end {
                patch(instrs, j, end)?;
            }
        }
        BExpr::Call(f, args) => {
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::Call(*f, args.len() as u8));
        }
        BExpr::CallColl(f, args, coll) => {
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::CallColl(*f, args.len() as u8, *coll));
        }
        BExpr::Coalesce(args) => {
            // Lazily: evaluate an argument, and if it is non-NULL jump to the
            // end WITH IT STILL ON THE STACK — it is the result. Otherwise pop
            // the NULL and try the next. The last argument needs no test: if we
            // reach it, it is the answer whatever it is.
            //
            // This is why JumpIfNotNull peeks instead of popping, and why
            // coalesce is not a Call: an eager coalesce(x, 1/0) would RAISE,
            // where both sqlite and PostgreSQL return x.
            let mut ends = Vec::new();
            let last = args.len() - 1;
            for (i, a) in args.iter().enumerate() {
                emit(a, instrs, consts)?;
                if i == last {
                    break;
                }
                ends.push(instrs.len());
                instrs.push(Instr::JumpIfNotNull(0)); // patched below
                instrs.push(Instr::Pop);
            }
            let end = instrs.len();
            for j in ends {
                patch(instrs, j, end)?;
            }
        }
        BExpr::InList(a, items) => {
            // Probe first, then the elements on top of it: InList(n) pops n
            // elements and finds the probe beneath them.
            emit(a, instrs, consts)?;
            for it in items {
                emit(it, instrs, consts)?;
            }
            instrs.push(Instr::InList(items.len() as u16));
        }
        BExpr::CollateCmp(op, a, b, coll) => {
            emit(a, instrs, consts)?;
            emit(b, instrs, consts)?;
            instrs.push(Instr::CmpColl(cmp_kind(*op), *coll));
        }
        // Comparison affinity is applied to BOTH operands (sqlite's `OP_Lt`
        // family does exactly that, and applying it to a value that already
        // has the target class is a no-op), then they are compared by class.
        // `Blob` is sqlite's NONE — nothing to apply, so nothing is emitted.
        BExpr::ClassCmp(op, a, b, coll, aff) => {
            emit(a, instrs, consts)?;
            if *aff != Affinity::Blob {
                instrs.push(Instr::Affinity(*aff));
            }
            emit(b, instrs, consts)?;
            if *aff != Affinity::Blob {
                instrs.push(Instr::Affinity(*aff));
            }
            instrs.push(Instr::CmpClass(cmp_kind(*op), *coll));
        }
        BExpr::InListColl(a, items, coll) => {
            emit(a, instrs, consts)?;
            for it in items {
                emit(it, instrs, consts)?;
            }
            instrs.push(Instr::InListColl(items.len() as u16, *coll));
        }
        BExpr::HostCall { name, args } => {
            // The NAME rides the const pool (a plan stores the name + arity, not
            // the closure); the arguments are pushed left-to-right, then the
            // opcode pops `argc` and leaves the one result.
            let name_idx = push_const(consts, Value::Text(name.clone()))?;
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::HostCall(name_idx, args.len() as u16));
        }
        BExpr::SpellCall { hash, args } => {
            // The content HASH rides the const pool as a 32-byte blob — the
            // definition is in the file, so the hash IS the function, and the
            // decode-side validator re-proves the shape.
            let hash_idx = push_const(consts, Value::Blob(hash.to_vec()))?;
            for a in args {
                emit(a, instrs, consts)?;
            }
            instrs.push(Instr::SpellCall(hash_idx, args.len() as u16));
        }
    }
    Ok(())
}

/// Fill in a forward jump target once it is known.
///
/// The index is a u16 in the IR, so a program with more than 65535 instructions
/// cannot express its own jumps. Caught here rather than silently truncating
/// into a target that points somewhere plausible and wrong.
pub(super) fn patch(instrs: &mut [Instr], at: usize, target: usize) -> Result<()> {
    let t = u16::try_from(target).map_err(|_| {
        Error::Internal("expression is too large to compile (more than 65535 instructions)".into())
    })?;
    match &mut instrs[at] {
        Instr::JumpIfNotTrue(x) | Instr::Jump(x) | Instr::JumpIfNotNull(x) => *x = t,
        other => return Err(Error::Internal(format!("patch target is not a jump: {other:?}"))),
    }
    Ok(())
}

pub(super) fn push_const(consts: &mut Vec<Value>, v: Value) -> Result<u16> {
    if consts.len() >= u16::MAX as usize {
        return Err(bind_err("expression has too many constants"));
    }
    consts.push(v);
    Ok((consts.len() - 1) as u16)
}
