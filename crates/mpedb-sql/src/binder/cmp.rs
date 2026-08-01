//! Comparison binding: operand unification, collation precedence, the
//! sqlite-vs-postgres dialect gates, and row-value desugaring (split from
//! binder.rs; see mod.rs).

use super::*;

impl<'a> Binder<'a> {
    /// Does this argument expression DEFINE a collating sequence, and which
    /// (sqlite `sqlite3ExprCollSeq`, as the scalar min/max NEEDCOLL search
    /// asks it)? A bare column defines its DECLARED collation — `Some(Binary)`
    /// for an undeclared one, which is a real answer that STOPS the
    /// left-to-right search (probed: `max(bincol, 'B' COLLATE NOCASE)` compares
    /// BINARY on 3.45.0). CAST is descended through, as sqlite does. Everything
    /// else — a literal, a computed expression — defines none. An explicit
    /// `COLLATE` in argument position is refused by `bind_expr` before this
    /// could matter, never silently dropped.
    pub(super) fn defining_collation(&self, e: &ast::Expr) -> Option<Collation> {
        match e {
            ast::Expr::Col(..) | ast::Expr::Qualified(..) => {
                Some(declared_collation(e, &self.scope))
            }
            ast::Expr::Cast(inner, _) => self.defining_collation(inner),
            _ => None,
        }
    }

    pub(super) fn bind_binary(&mut self, op: BinOp, l: &ast::Expr, r: &ast::Expr) -> Result<(BExpr, Ty)> {
        // COLLATE is honored on comparison operands only. Peel a top-level
        // COLLATE off each side HERE, before binding, so the inner expression
        // binds normally and the resolved collation feeds the precedence rule
        // below. For every other operator the raw operands are bound, so a
        // COLLATE there reaches `bind_expr` and is refused rather than ignored.
        let is_cmp = matches!(
            op,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        );
        // Row-value (tuple) comparison — `(a, …) OP (b, …)` with a parenthesized
        // list of ≥2 expressions on at least one side. Desugars to scalar boolean
        // logic (see `bind_row_value_cmp`); NO plan/format change. Intercepted
        // before the operand bind below so the row values do not hit the
        // "row value misused" arm.
        if is_cmp
            && (matches!(l, ast::Expr::RowValue(_)) || matches!(r, ast::Expr::RowValue(_)))
        {
            return self.bind_row_value_cmp(op, l, r);
        }
        let (l_ast, l_coll, r_ast, r_coll) = if is_cmp {
            let (la, lc) = peel_collate(l)?;
            let (ra, rc) = peel_collate(r)?;
            (la, lc, ra, rc)
        } else {
            (l, None, r, None)
        };
        let (l, lt) = self.bind_expr(l_ast)?;
        let (r, rt) = self.bind_expr(r_ast)?;
        match op {
            // The two JSON accessors are OPERATORS in the grammar but scalar
            // CALLS in the IR, so they never reach `BExpr::Binary` — one less
            // opcode, and `->`/`->>` share the whole path machinery with
            // `json_extract`. `->` always yields JSON text (or NULL); `->>`
            // yields whatever SQL value the node unwraps to, hence `Any`.
            BinOp::JsonArrow | BinOp::JsonArrowText => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                match lt {
                    Some(ColumnType::Text) | Some(ColumnType::Any) | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "`{}` expects JSON text on the left, got {other}",
                            op_symbol(op)
                        )))
                    }
                }
                // The right operand is a path (text) or an array index
                // (integer); a bare param adopts text, which is what every ORM
                // binds there.
                let (r, rt) = self.unify_param(r, rt, ColumnType::Text);
                match rt {
                    Some(ColumnType::Text)
                    | Some(ColumnType::Int64)
                    | Some(ColumnType::Any)
                    | None => {}
                    Some(other) => {
                        return Err(bind_err(format!(
                            "`{}` expects a JSON path (text) or an array index (int64) on the \
                             right, got {other}",
                            op_symbol(op)
                        )))
                    }
                }
                let f = if op == BinOp::JsonArrow {
                    ScalarFn::JsonArrow
                } else {
                    ScalarFn::JsonArrowText
                };
                let ret = if op == BinOp::JsonArrow {
                    ColumnType::Text
                } else {
                    ColumnType::Any
                };
                let e = fold_maybe(BExpr::Call(f, vec![l, r]), self.suppress_fold)?;
                Ok((e, Some(ret)))
            }
            BinOp::And | BinOp::Or => {
                let (l, lt) = self.unify_param(l, lt, ColumnType::Bool);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Bool);
                let (l, lt) = self.coerce_bool_ctx(l, lt)?;
                let (r, rt) = self.coerce_bool_ctx(r, rt)?;
                for t in [lt, rt].into_iter().flatten() {
                    if t != ColumnType::Bool {
                        return Err(bind_err(format!(
                            "AND/OR requires boolean operands, got {t}"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let (l, lt, r, rt) = self.bridge_bool_int(l, lt, r, rt)?;
                // Equality pins from columns so `WHERE id = ?` stays Binary and
                // remains a PkPoint/IndexPoint. Inequality leaves the param free
                // (ClassCmp+Numeric) so `year >= 1942.1` is exact numeric compare.
                let is_eq = matches!(op, BinOp::Eq | BinOp::Ne);
                // COLUMN vs COLUMN across the numeric/text divide, decided
                // BEFORE unification because `unify_types` errors on
                // `(Int64, Text)` and `class_cmp_affinity` is never reached.
                //
                // sqlite applies NUMERIC affinity to both sides when one has a
                // numeric affinity and the other TEXT, so an INTEGER pk joined
                // against a `varchar` FK matches — which is exactly what
                // Django's generic relations do (`object_id` is a CharField).
                // mpedb refused it as "cannot compare int64 and text".
                //
                // STRICTLY column-vs-column. It must never widen to a
                // parameter or a constant: `as_col_cmp` matches `ClassCmp`, so
                // `id = '007'` would build a `PkPoint` on the UNCONVERTED text,
                // probe for `Text("007")`, miss, and return no rows where
                // sqlite returns row 7 — a wrong answer, not a refusal.
                // Column-vs-column is safe because `as_atom` rejects a `Col`
                // on the other side, so no single-table access path forms.
                let numeric_text_cols = self.numeric_vs_text_columns(&l, &r);
                let (l, r, unified) = if numeric_text_cols {
                    (l, r, Some(ColumnType::Any))
                } else if is_eq {
                    self.unify_compare_eq(l, lt, r, rt)?
                } else {
                    self.unify_compare_operands(l, lt, r, rt)?
                };
                // sqlite's collation precedence, in order: an explicit `COLLATE`
                // on the LEFT operand, else on the RIGHT; else the LEFT operand's
                // COLUMN collation (rung 2), else the RIGHT column's; else BINARY.
                // A non-Binary result gets its own `CollateCmp` node so the
                // access-path extractor never mistakes it for an index probe; a
                // Binary comparison stays a plain `Binary` node, byte-for-byte
                // unchanged (and a Binary-collated text column resolves to Binary,
                // so an index/PK equality on it is untouched). Collation degrades
                // to bytewise for non-text at runtime, so emitting it for any
                // statically-unpinned operand is safe.
                // A correlation PARAM stands in for an outer column, and rung 2
                // is about the COLUMN's declared collation — so the param must
                // answer for the column it replaces. Reading only `Col` made
                // every comparison across a subquery boundary fall to BINARY,
                // which is a wrong answer wherever the outer column is NOCASE.
                let coll = l_coll
                    .or(r_coll)
                    .or_else(|| self.operand_collation(&l))
                    .or_else(|| self.operand_collation(&r))
                    .unwrap_or_default();
                // Comparison affinity + storage-class order. Equality against a
                // typed column deliberately does NOT take ClassCmp (keeps the
                // Binary probe). See `class_cmp_affinity`.
                let node = match if numeric_text_cols {
                    Some(Affinity::Numeric)
                } else {
                    self.class_cmp_affinity(unified, &l, &r, is_eq)
                } {
                    Some(aff) => BExpr::ClassCmp(op, Box::new(l), Box::new(r), coll, aff),
                    None if coll == Collation::Binary => {
                        BExpr::Binary(op, Box::new(l), Box::new(r))
                    }
                    None => BExpr::CollateCmp(op, Box::new(l), Box::new(r), coll),
                };
                let e = fold_maybe(node, self.suppress_fold)?;
                Ok((e, Some(ColumnType::Bool)))
            }
            BinOp::Concat => {
                // §8: may a BLOB reach this chain? An unpinned parameter
                // (type still None), an `Any` operand (typeless column, UDF
                // result) or a declared blob can each deliver one at run
                // time. sqlite dialect only — PostgreSQL's `||` refuses as
                // before.
                let blob_possible = self.sqlite_dialect()
                    && ([&lt, &rt].into_iter().any(|t| {
                        matches!(t, None | Some(ColumnType::Any) | Some(ColumnType::Blob))
                    })
                        // An operand that is ALREADY the n-ary node forces the
                        // n-ary path: `'xxx' || ? || 'yyy'` binds left-deep,
                        // the inner pair took ConcatN for its parameter, and
                        // the outer (Text ++ Text on paper) must SPLICE INTO
                        // it or 'yyy' never joins the byte recombination —
                        // measured: stock's message carries 'xxx�yyy', a
                        // two-level tree stopped at 'xxx�'.
                        || matches!(l, BExpr::ConcatN(_))
                        || matches!(r, BExpr::ConcatN(_)));
                if blob_possible {
                    // Floats keep their refusal in BOTH forms.
                    for t in [lt, rt].into_iter().flatten() {
                        if matches!(t, ColumnType::Float64) {
                            return Err(bind_err(format!(
                                "`||` requires text, int64, or bool operands, got {t}"
                            )));
                        }
                    }
                    // ONE node for the WHOLE chain: splice nested concat
                    // operands (either form) so the bytes recombine across
                    // every operand, exactly as sqlite evaluates the chain.
                    // Parameters stay UNPINNED — the slot remains `Any`, which
                    // is what lets CPython bind a blob into `'xxx' || ?`.
                    let mut ops = Vec::new();
                    splice_concat(l, &mut ops);
                    splice_concat(r, &mut ops);
                    return Ok((BExpr::ConcatN(ops), Some(ColumnType::Text)));
                }
                let (l, lt) = self.unify_param(l, lt, ColumnType::Text);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Text);
                // Same render set as the runtime: text/int/bool (Any decided
                // per value); floats are refused until formatting is pinned.
                for t in [lt, rt].into_iter().flatten() {
                    if !matches!(
                        t,
                        ColumnType::Text | ColumnType::Int64 | ColumnType::Bool | ColumnType::Any
                    ) {
                        return Err(bind_err(format!(
                            "`||` requires text, int64, or bool operands, got {t}"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Text)))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let (l, r, ty) = self.unify_operands(l, lt, r, rt, "arithmetic on")?;
                if let Some(t) = ty {
                    // `Any` is admitted for the same reason comparison admits
                    // it: the operand's real type is only known per value, and
                    // the runtime `arith` already refuses a non-numeric one.
                    // Without this, `doc ->> '$.n' + 1` — and every arithmetic
                    // over a host UDF result — would be a COMPILE error even
                    // though the values are numbers.
                    if t != ColumnType::Int64
                        && t != ColumnType::Float64
                        && t != ColumnType::Any
                    {
                        return Err(bind_err(format!(
                            "arithmetic requires int64 or float64 operands, got {t}"
                        )));
                    }
                }
                let e = fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, ty))
            }
            // `&`, `|`, `<<`, `>>` (task #74 item 2). NOT unified like
            // arithmetic: sqlite's bitwise operators do not have a "wider
            // operand type" at all — both sides are cast to an integer and the
            // result is ALWAYS an integer. So each side is typed on its own and
            // the result is int64 regardless.
            BinOp::BitAnd | BinOp::BitOr | BinOp::Shl | BinOp::Shr => {
                let name = bit_op_name(op);
                let (l, lt) = self.unify_param(l, lt, ColumnType::Int64);
                let (r, rt) = self.unify_param(r, rt, ColumnType::Int64);
                let l = self.bit_operand(l, lt, name)?;
                let r = self.bit_operand(r, rt, name)?;
                let e =
                    fold_maybe(BExpr::Binary(op, Box::new(l), Box::new(r)), self.suppress_fold)?;
                Ok((e, Some(ColumnType::Int64)))
            }
        }
    }

    /// Type-check ONE operand of a bitwise operator.
    ///
    /// sqlite casts every operand to an integer with a total conversion
    /// (`sqlite3VdbeIntValue`): a real truncates toward zero, a text takes an
    /// integer-prefix parse, `'abc'` becomes 0. mpedb accepts the operand types
    /// where that conversion is a NO-OP and refuses the rest by name:
    ///
    /// * `int64` — the operand type these operators are for.
    /// * `bool` — sqlite has no boolean; it IS the integer 0/1, the same
    ///   mapping `bind_assign` already uses for `SET int_col = (a = b)`.
    /// * `any` — the typeless escape. Its runtime value gets sqlite's FULL
    ///   coercion in [`mpedb_types::expr`], which is the contract `any` already
    ///   has for comparisons (`Instr::CmpClass`): rigid types are pinned at
    ///   compile time, `any` gets sqlite's runtime rules.
    /// * an untyped NULL — propagates, like every other operator.
    ///
    /// A statically-typed `float64`, `text` or `blob` is REFUSED, and refused
    /// rather than silently truncated for the same reason a non-integral
    /// parameter is (task #74 item 1): `r & 1` on a column of reals would
    /// answer a question about `trunc(r)` without saying so. `CAST(r AS
    /// INTEGER)` asks for it explicitly and is what the message names.
    pub(super) fn bit_operand(&mut self, e: BExpr, t: Ty, op: &str) -> Result<BExpr> {
        match t {
            None | Some(ColumnType::Int64) | Some(ColumnType::Bool) | Some(ColumnType::Any) => {
                Ok(e)
            }
            Some(t) => Err(bind_err(format!(
                "`{op}` requires int64 operands, got {t} — sqlite would silently \
                 convert it to an integer (truncating a real, taking the leading \
                 digits of a text); write `CAST(x AS INTEGER)` to ask for that"
            ))),
        }
    }

    /// Bind a ROW-VALUE (tuple) comparison `(a1,…,an) OP (b1,…,bn)`. Both sides
    /// must be explicit row values of EQUAL arity; the comparison desugars to
    /// ordinary scalar boolean logic (see [`Self::desugar_row_cmp`]) which is
    /// provably NULL-correct 3VL and matches sqlite bit-for-bit — there is no
    /// plan/format change. Every other shape is refused as a clean bind error
    /// (never a wrong answer): a row value against a scalar, a subquery RHS, or
    /// an arity mismatch.
    /// `(a, b) [NOT] IN ((x, y), …)` — an OR of per-element row comparisons.
    ///
    /// `NOT IN` is the NEGATION of the whole disjunction, not an AND of `<>`s:
    /// that is what makes a NULL anywhere in a non-matching row give NULL
    /// rather than TRUE, which is sqlite's answer and the one an ORM's
    /// `not_in()` depends on.
    pub(super) fn bind_row_value_in(
        &mut self,
        lhs: &ast::Expr,
        items: &[ast::Expr],
        negated: bool,
    ) -> Result<(BExpr, Ty)> {
        use ast::Expr as E;
        // An EMPTY list is FALSE (`NOT IN` TRUE) — no row can match, NULLs
        // included, exactly as the scalar path answers it.
        if items.is_empty() {
            return self.bind_expr(&E::Lit(Value::Bool(negated)));
        }
        // Built as AST and bound through the ordinary path, exactly as
        // `bind_row_value_cmp` does with its own desugar: each arm then gets
        // the same unification, coercion and constant folding a hand-written
        // `(x=1 AND y=2) OR …` would, with no new node kind anywhere.
        let mut acc = E::Binary(BinOp::Eq, Box::new(lhs.clone()), Box::new(items[0].clone()));
        for it in &items[1..] {
            let arm = E::Binary(BinOp::Eq, Box::new(lhs.clone()), Box::new(it.clone()));
            acc = E::Binary(BinOp::Or, Box::new(acc), Box::new(arm));
        }
        if negated {
            acc = E::Unary(ast::UnOp::Not, Box::new(acc));
        }
        self.bind_expr(&acc)
    }

    pub(super) fn bind_row_value_cmp(&mut self, op: BinOp, l: &ast::Expr, r: &ast::Expr) -> Result<(BExpr, Ty)> {
        use ast::Expr as E;
        let (lhs, rhs) = match (l, r) {
            (E::RowValue(a), E::RowValue(b)) => (a, b),
            // `(a, b) = (SELECT …)` — a row value against a subquery. Deferred by
            // name. (In a plain SELECT the scalar-subquery lift runs before the
            // binder, so the subquery arrives here only from a CHECK / policy /
            // trigger expression, which is not lifted; a single-column subquery
            // in a plain SELECT is lifted to a scalar param and lands in the
            // "row value misused" arm below, which is likewise a clean refusal.)
            (E::RowValue(_), E::Subquery(_))
            | (E::Subquery(_), E::RowValue(_))
            | (E::RowValue(_), E::InSubquery(..))
            | (E::InSubquery(..), E::RowValue(_)) => {
                return Err(bind_err(
                    "a row value compared against a subquery is not supported",
                ));
            }
            // A row value against a scalar (or vice versa) — sqlite: "row value
            // misused".
            _ => return Err(bind_err("row value misused")),
        };
        if lhs.len() != rhs.len() {
            return Err(bind_err(format!(
                "row values have an unequal number of columns: left has {}, right has {}",
                lhs.len(),
                rhs.len()
            )));
        }
        // The parser only ever builds a RowValue with ≥2 elements; be defensive
        // rather than index out of range if that ever changes.
        if lhs.is_empty() {
            return Err(bind_err("row value misused"));
        }
        let desugared = Self::desugar_row_cmp(op, lhs, rhs);
        // Bind the desugared scalar expression through the ordinary path: each
        // element pair binds exactly like the corresponding scalar comparison
        // (same type unification, coercions, collation precedence and folding),
        // and the And/Or/Not combinators fold bottom-up — so the result is a
        // fully constant-folded `BExpr` typed `Bool`, with no new node kind.
        self.bind_expr(&desugared)
    }

    /// Desugar `(a1,…,an) OP (b1,…,bn)` (equal arity ≥ 1) into the scalar boolean
    /// expression sqlite uses — provably NULL-correct 3VL:
    ///
    /// - `=`  → `a1=b1 AND … AND an=bn`
    /// - `<>` → `NOT (a1=b1 AND … AND an=bn)`
    /// - `<`  → `a1<b1 OR (a1=b1 AND (a2<b2 OR (a2=b2 AND (… AND an<bn))))`
    ///   (right-nested, lexicographic).
    /// - `<=` / `>` / `>=` — the same lexicographic shape; only the operator
    ///   differs: a STRICT `<`/`>` at every non-last level, and the base operator
    ///   `<`/`<=`/`>`/`>=` at the LAST element.
    ///
    /// Building an `ast::Expr` (rather than a `BExpr`) and re-binding it is what
    /// makes each element pair reuse the scalar comparison binding verbatim.
    pub(super) fn desugar_row_cmp(op: BinOp, a: &[ast::Expr], b: &[ast::Expr]) -> ast::Expr {
        use ast::Expr as E;
        let cmp = |i: usize, o: BinOp| -> E {
            E::Binary(o, Box::new(a[i].clone()), Box::new(b[i].clone()))
        };
        let eq = |i: usize| cmp(i, BinOp::Eq);
        let n = a.len();
        match op {
            BinOp::Eq | BinOp::Ne => {
                // Conjunction of element equalities; `<>` negates the whole.
                let mut acc = eq(0);
                for i in 1..n {
                    acc = E::Binary(BinOp::And, Box::new(acc), Box::new(eq(i)));
                }
                if op == BinOp::Ne {
                    E::Unary(ast::UnOp::Not, Box::new(acc))
                } else {
                    acc
                }
            }
            // The four ordering operators share one right-nested recursion; the
            // strict per-level operator and the base (last-element) operator are
            // the only difference between them.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let (strict, last) = match op {
                    BinOp::Lt => (BinOp::Lt, BinOp::Lt),
                    BinOp::Le => (BinOp::Lt, BinOp::Le),
                    BinOp::Gt => (BinOp::Gt, BinOp::Gt),
                    BinOp::Ge => (BinOp::Gt, BinOp::Ge),
                    _ => unreachable!(),
                };
                // Build from the last element back to the first.
                let mut acc = cmp(n - 1, last);
                for i in (0..n - 1).rev() {
                    // a_i strict b_i OR (a_i = b_i AND acc)
                    let tail = E::Binary(BinOp::And, Box::new(eq(i)), Box::new(acc));
                    acc = E::Binary(BinOp::Or, Box::new(cmp(i, strict)), Box::new(tail));
                }
                acc
            }
            // Not reachable: `bind_row_value_cmp` is only called for the six
            // comparison operators.
            _ => unreachable!("desugar_row_cmp called with a non-comparison operator"),
        }
    }

    /// Make both operands the same type: unify bare parameters, apply the one
    /// legal coercion (Int64 -> Float64), reject everything else cross-type.
    /// Returns the (possibly coerced) operands and the common type
    /// (`None` when it could not be pinned).
    /// Bridge a `bool`/`int64` COMPARISON the way sqlite's storage does, and
    /// only for a comparison (`=`, `<`, …, `IS`) — never for arithmetic.
    ///
    /// sqlite has no boolean type: a `BooleanField` column literally holds the
    /// integers 0 and 1, which is why Django writes `WHERE "t"."flag" = 1`.
    /// mpedb keeps a rigid `Bool`, so the two must be reconciled — but by the
    /// integer VALUE of the bool, never by truthiness of the int:
    ///
    /// * an int CONSTANT that is exactly 0 or 1 folds into the bool domain
    ///   (`flag = 1` -> `flag = TRUE`). This is the shape Django emits, and
    ///   keeping both sides `Bool` keeps the node a plain `Binary(Eq, Col,
    ///   Const)` — so an index/PK probe on the column survives. Ordering is
    ///   exact too: `FALSE < TRUE` is `0 < 1`, and 0/1 are the only bools.
    /// * anything else casts the BOOL side UP to its integer 0/1
    ///   (`Instr::Cast(Integer)`). So `flag = 2` is FALSE and `flag = -1` is
    ///   FALSE — which is what sqlite answers, because the column only ever
    ///   holds 0 or 1. Truthy-testing the int instead would make `flag = 2`
    ///   TRUE: a wrong answer, and precisely the over-reach this avoids.
    ///
    /// NULL is untouched on both paths, so 3VL is unchanged.
    pub(super) fn bridge_bool_int(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, Ty, BExpr, Ty)> {
        use ColumnType::{Bool, Int64};
        if self.bare_group_by != BareGroupBy::Sqlite {
            return Ok((l, lt, r, rt)); // PostgreSQL: `flag = 1` stays an error
        }
        // Fold a 0/1 int literal into the bool domain.
        let as_bool = |e: &BExpr| match e {
            BExpr::Const(Value::Int(i @ (0 | 1))) => Some(BExpr::Const(Value::Bool(*i == 1))),
            _ => None,
        };
        match (lt, rt) {
            (Some(Bool), Some(Int64)) => Ok(match as_bool(&r) {
                Some(rb) => (l, Some(Bool), rb, Some(Bool)),
                None => (
                    BExpr::Cast(Box::new(l), Affinity::Integer),
                    Some(Int64),
                    r,
                    Some(Int64),
                ),
            }),
            (Some(Int64), Some(Bool)) => Ok(match as_bool(&l) {
                Some(lb) => (lb, Some(Bool), r, Some(Bool)),
                None => (
                    l,
                    Some(Int64),
                    BExpr::Cast(Box::new(r), Affinity::Integer),
                    Some(Int64),
                ),
            }),
            _ => Ok((l, lt, r, rt)),
        }
    }

    pub(super) fn unify_operands(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
        verb: &str,
    ) -> Result<(BExpr, BExpr, Ty)> {
        // A bare unconstrained param adopts the other side's type.
        let (l, lt) = match rt {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match lt {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        self.unify_types(l, lt, r, rt, verb)
    }

    /// Equality: pin bare params from COLUMN or CAST so `WHERE id = ?` is a
    /// Binary probe (PkPoint). Text/float binds that are not exact for the
    /// column type still refuse at coerce_params (or convert when exact).
    pub(super) fn unify_compare_eq(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, BExpr, Ty)> {
        let pin_source = |e: &BExpr, t: Ty| -> Option<ColumnType> {
            match (e, t) {
                (BExpr::Col(_), Some(t)) if t != ColumnType::Any => Some(t),
                (BExpr::Cast(_, _), Some(t)) => Some(t),
                _ => None,
            }
        };
        let (l, lt) = match pin_source(&r, rt) {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match pin_source(&l, lt) {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        self.unify_types(l, lt, r, rt, "compare")
    }

    /// Inequality: never pin a bare param from a COLUMN.
    ///
    /// `year >= ?` with a float bind (Django annotate) must compare numerically.
    /// ClassCmp+Numeric does that; Binary+int pin would refuse 1942.1.
    pub(super) fn unify_compare_operands(
        &mut self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
    ) -> Result<(BExpr, BExpr, Ty)> {
        let pin_source = |e: &BExpr, t: Ty| -> Option<ColumnType> {
            match (e, t) {
                (BExpr::Cast(_, _), Some(t)) => Some(t),
                _ => None,
            }
        };
        let (l, lt) = match pin_source(&r, rt) {
            Some(t) => self.unify_param(l, lt, t),
            None => (l, lt),
        };
        let (r, rt) = match pin_source(&l, lt) {
            Some(t) => self.unify_param(r, rt, t),
            None => (r, rt),
        };
        self.unify_types(l, lt, r, rt, "compare")
    }

    pub(super) fn unify_types(
        &self,
        l: BExpr,
        lt: Ty,
        r: BExpr,
        rt: Ty,
        verb: &str,
    ) -> Result<(BExpr, BExpr, Ty)> {
        match (lt, rt) {
            (Some(a), Some(b)) if a == b => Ok((l, r, Some(a))),
            (Some(ColumnType::Int64), Some(ColumnType::Float64)) => {
                let l = fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(l)), self.suppress_fold)?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            (Some(ColumnType::Float64), Some(ColumnType::Int64)) => {
                let r = fold_maybe(BExpr::Unary(BUnOp::ToFloat, Box::new(r)), self.suppress_fold)?;
                Ok((l, r, Some(ColumnType::Float64)))
            }
            // A dynamically-typed operand (`ColumnType::Any` — a host UDF result
            // (design/DESIGN-UDF.md) or a typeless column) unifies with ANY
            // concrete type: the real value is typed at runtime, where `sql_cmp`
            // and `arith` handle the actual pair (numeric comparison already
            // crosses Int/Float). The unified type stays `Any`.
            (Some(ColumnType::Any), Some(_)) | (Some(_), Some(ColumnType::Any)) => {
                Ok((l, r, Some(ColumnType::Any)))
            }
            (Some(a), Some(b)) => Err(bind_err(format!("cannot {verb} {a} and {b}"))),
            (Some(t), None) | (None, Some(t)) => Ok((l, r, Some(t))),
            (None, None) => Ok((l, r, None)),
        }
    }

    /// sqlite's **comparison affinity** for a comparison that touches a
    /// TYPELESS (`any`) column: the affinity applied to BOTH operands before
    /// they are compared by storage class. `None` means "not this rule" — the
    /// caller then keeps the plain comparison, which REFUSES a cross-class pair
    /// exactly as it does today.
    ///
    /// A port of `sqlite3CompareAffinity`, with `sqlite3ExprAffinity` narrowed
    /// to the two shapes that carry one: a COLUMN (its declared affinity) and a
    /// `CAST` (its target's). Everything else — a literal, a parameter, any
    /// computed expression — has NO affinity, which is sqlite's rule too.
    ///
    /// Two gates, both deliberate:
    ///
    /// - the unified type must be `Any` or UNKNOWN. Only a comparison that is
    ///   not statically pinned can meet two storage classes at runtime; every
    ///   rigid one was already pinned by the binder and must stay
    ///   byte-identical. Unknown is the `CAST(? AS NUMERIC) = ?` shape, where
    ///   NUMERIC pins neither side — Django's `DecimalField` filter.
    /// - one operand must CARRY an affinity — a bare `any` COLUMN, or a `CAST`
    ///   (`CAST(x AS NUMERIC) > ?`, which Django writes for every `DecimalField`
    ///   aggregate) — and NEITHER may be a bare column of a concrete type. The
    ///   second half is what keeps the rule from silently rewriting an
    ///   `<indexed column> = <host UDF>` comparison — correct either way, but it
    ///   would lose the index probe (`ClassCmp` is never an access path). A CAST
    ///   is never an index probe itself, so admitting it costs no access path.
    ///
    /// Everything outside those gates keeps today's behavior, which is the only
    /// reason this can be landed without auditing every comparison in the
    /// language: an unvetted pair still refuses rather than ordering by class,
    /// and ordering by class WITHOUT the affinity is the wrong answer this
    /// rule exists to avoid (`price < '40.0'` would say "every number is
    /// smaller than a text" where sqlite compares against 40.0).
    /// Both operands are BARE COLUMNS, exactly one carrying a numeric affinity
    /// (`Integer`/`Real`/`Numeric`) and the other `Text`.
    ///
    /// That is the one shape where sqlite's `sqlite3CompareAffinity` applies
    /// NUMERIC to both sides and mpedb's rigid unification refuses. Restricted
    /// to columns on BOTH sides deliberately — see the call site for why a
    /// parameter or constant on either side would be a wrong answer.
    pub(super) fn numeric_vs_text_columns(&self, l: &BExpr, r: &BExpr) -> bool {
        let aff = |e: &BExpr| match e {
            BExpr::Col(i) => self.scope.column_shape(*i).map(|(_, a)| a),
            _ => None,
        };
        let (Some(la), Some(ra)) = (aff(l), aff(r)) else {
            return false;
        };
        let num = |a: Affinity| {
            matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric)
        };
        (num(la) && ra == Affinity::Text) || (la == Affinity::Text && num(ra))
    }

    pub(super) fn class_cmp_affinity(
        &self,
        unified: Ty,
        l: &BExpr,
        r: &BExpr,
        is_eq: bool,
    ) -> Option<Affinity> {
        let is_param = |e: &BExpr| matches!(e, BExpr::Param(_));
        let is_col = |e: &BExpr| matches!(e, BExpr::Col(_));
        // Column vs bare param for INEQUALITY only: ClassCmp+Numeric so a float
        // bind against an INTEGER column is exact (Django annotate). Equality
        // keeps Binary so access extraction can still form PkPoint/IndexPoint.
        if !is_eq && ((is_param(l) && is_col(r)) || (is_param(r) && is_col(l))) {
            let col_e = if is_col(l) { l } else { r };
            if let BExpr::Col(i) = col_e {
                if let Some((_, aff)) = self.scope.column_shape(*i) {
                    let numeric = matches!(
                        aff,
                        Affinity::Integer | Affinity::Real | Affinity::Numeric
                    );
                    return Some(if numeric { Affinity::Numeric } else { aff });
                }
            }
        }
        if !matches!(unified, Some(ColumnType::Any) | None) {
            // A concrete unified type normally means both sides already agree
            // and a plain Binary comparison is correct. The one exception is a
            // bare PARAM left untyped against a literal/expression: unified is
            // the concrete side's type, but at runtime the bound value may be
            // any class — emit ClassCmp (no affinity) so `1 = ?` with a text
            // bind answers FALSE rather than "cannot compare".
            if (!is_param(l) && !is_param(r)) || is_col(l) || is_col(r) {
                return None;
            }
            // Fall through with admit via the param path below; `aff_of` for a
            // const/param pair is (None, None) → Blob (no conversion).
        }
        let col_ty = |e: &BExpr| match e {
            BExpr::Col(i) => self.scope.column_shape(*i).map(|(t, _)| t),
            _ => None,
        };
        let is_any_col = |e: &BExpr| col_ty(e) == Some(ColumnType::Any);
        let is_typed_col = |e: &BExpr| col_ty(e).is_some_and(|t| t != ColumnType::Any);
        let is_cast = |e: &BExpr| matches!(e, BExpr::Cast(..));
        // A bare `any` column admits the rule on its own (unchanged). A CAST
        // admits it only when the OTHER side is not a bare concrete column,
        // which is the shape whose index probe must survive. A bare PARAM
        // against a non-column (literal / expression) is the same rule: no
        // affinity, class-order at runtime (CPython `select 1 as a where a=?`).
        let admit = is_any_col(l)
            || is_any_col(r)
            || ((is_cast(l) || is_cast(r)) && !is_typed_col(l) && !is_typed_col(r))
            || ((is_param(l) || is_param(r)) && !is_typed_col(l) && !is_typed_col(r));
        if !admit {
            return None;
        }
        let aff_of = |e: &BExpr| match e {
            BExpr::Col(i) => self.scope.column_shape(*i).map(|(_, a)| a),
            BExpr::Cast(_, a) => Some(*a),
            _ => None,
        };
        let numeric =
            |a: Affinity| matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric);
        Some(match (aff_of(l), aff_of(r)) {
            // Both operands carry an affinity: NUMERIC if either is numeric,
            // else none. (This is where sqlite does NOT apply TEXT: a text
            // column against a typeless one compares raw.)
            (Some(a), Some(b)) => {
                if numeric(a) || numeric(b) {
                    Affinity::Numeric
                } else {
                    Affinity::Blob
                }
            }
            // One side carries an affinity and the other does not: use it.
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Affinity::Blob,
        })
    }

    /// Fold a `coalesce`'s CONTROL FLOW, PostgreSQL-style.
    ///
    /// Leading NULL constants can never be the answer, so drop them. If what is
    /// then first is a non-NULL constant, IT is the answer and every later
    /// argument is dead — dropped without ever being folded, which is exactly
    /// why `coalesce(1, 1/0)` returns 1 instead of raising. Whatever survives is
    /// folded normally, so `coalesce(NULL, 1/0)` still raises: that divide is
    /// genuinely reachable.
    pub(super) fn fold_coalesce(&mut self, args: Vec<BExpr>) -> Result<BExpr> {
        let mut live = Vec::with_capacity(args.len());
        for a in args {
            // Fold this REACHABLE arg first, so a foldable constant like `-24`
            // (`Unary(Neg, Const)`, left unfolded by `suppress_fold`) is
            // recognized as the answer — otherwise `coalesce(-24, col)` would
            // keep `col` alive even though it can never be reached. Args AFTER
            // the first non-NULL constant are unreachable and are NEVER folded
            // below (their raise stays suppressed), exactly as before.
            let a = fold(a)?;
            if matches!(&a, BExpr::Const(Value::Null)) {
                continue; // a NULL constant is never the result
            }
            let dead_after = matches!(&a, BExpr::Const(_));
            live.push(a);
            if dead_after {
                break; // a non-NULL constant answers; the rest is unreachable
            }
        }
        match live.len() {
            // every argument was a NULL constant
            0 => Ok(BExpr::Const(Value::Null)),
            // Survivors are already folded above.
            1 => Ok(live.pop().expect("len 1")),
            _ => Ok(BExpr::Coalesce(live)),
        }
    }

    /// Fold a CASE's control flow: an arm whose condition is constant FALSE or
    /// NULL is dead and is dropped unfolded; an arm whose condition is constant
    /// TRUE answers, and everything after it (including ELSE) is dead.
    pub(super) fn fold_case(&mut self, arms: Vec<(BExpr, BExpr)>, else_: BExpr) -> Result<BExpr> {
        let mut live = Vec::with_capacity(arms.len());
        for (c, r) in arms {
            match &c {
                BExpr::Const(Value::Bool(false)) | BExpr::Const(Value::Null) => continue,
                BExpr::Const(Value::Bool(true)) => {
                    // This arm always wins.
                    if live.is_empty() {
                        return fold(r);
                    }
                    live.push((c, r));
                    let (arms, (_, r)) = {
                        let last = live.pop().expect("just pushed");
                        (live, last)
                    };
                    return Ok(BExpr::Case(fold_arms(arms)?, Some(Box::new(fold(r)?))));
                }
                _ => live.push((c, r)),
            }
        }
        if live.is_empty() {
            return fold(else_);
        }
        Ok(BExpr::Case(fold_arms(live)?, Some(Box::new(fold(else_)?))))
    }
}


/// Flatten a concat operand into the n-ary chain (§8): an inner `ConcatN`
/// or `Binary(Concat)` contributes its OPERANDS, so the whole `||` chain is
/// one node whatever mix of paths bound the parts.
fn splice_concat(e: BExpr, out: &mut Vec<BExpr>) {
    match e {
        BExpr::ConcatN(ops) => {
            for o in ops {
                splice_concat(o, out);
            }
        }
        BExpr::Binary(BinOp::Concat, a, b) => {
            splice_concat(*a, out);
            splice_concat(*b, out);
        }
        other => out.push(other),
    }
}
