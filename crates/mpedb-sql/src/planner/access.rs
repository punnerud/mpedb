use super::*;

/// Decompose the (already bound and folded) WHERE expression into an access
/// path plus a residual predicate. Consumed conjuncts move into the access
/// path; literals become plan-level constants.
pub(super) fn extract_access(
    bound_where: Option<BExpr>,
    table: &TableDef,
    consts: &mut Vec<Value>,
) -> Result<(AccessPath, Option<BExpr>)> {
    let Some(w) = bound_where else {
        return Ok((AccessPath::FullScan, None));
    };
    let mut conjuncts = Vec::new();
    split_and(w, &mut conjuncts);
    let cmps: Vec<Option<(u16, BinOp, Atom)>> = conjuncts.iter().map(as_col_cmp).collect();
    let mut consumed = vec![false; conjuncts.len()];

    // Find the first unconsumed conjunct `col <op-in-set> atom` on `col`.
    let find = |consumed: &[bool], col: u16, ops: &[BinOp]| -> Option<(usize, BinOp, Atom)> {
        cmps.iter().enumerate().find_map(|(i, c)| match c {
            Some((cc, op, atom)) if !consumed[i] && *cc == col && ops.contains(op) => {
                Some((i, *op, atom.clone()))
            }
            _ => None,
        })
    };

    // A TYPELESS (`any`) key column is never an access path — not as a PK
    // point, not as a PK range, not through an index. The schema now ALLOWS
    // such a key (its tree is keyed by storage class, `keycode::KeySpec`, whose
    // equality and order are sqlite's), but a probe is a strictly stronger
    // claim than a well-ordered tree, and two things break it:
    //
    //   - **affinity.** sqlite compares `x = <bound>` on a typeless column
    //     only AFTER applying the pair's comparison affinity to the bound
    //     (`sqlite3CompareAffinity`; the binder's `BExpr::ClassCmp`). A raw
    //     bound encoded straight into a key skips that step, so a NUMERIC
    //     column would answer `dt < '2020-01-01'` by comparing a number
    //     against a text — "every number is smaller" — instead of against the
    //     converted value.
    //   - **mpedb's own types.** `Bool`/`Timestamp` have no sqlite storage
    //     class. `Value::sort_cmp` calls them peers (and `Instr::CmpClass`
    //     raises a type error), while the key encoding must give them SOME
    //     rank to stay a total order. A range bound would silently include or
    //     exclude by that invented rank where the filter refuses.
    //
    // Keeping every predicate over an `any` column a residual filter over a
    // full scan is what makes the comparison-affinity rule's own proof still
    // hold: a `ClassCmp` is never an access path, so that rule can only ever
    // rewrite a filter. `as_col_cmp` matches `BExpr::Binary` only and so
    // already skips `ClassCmp`; this guard is the one that does not depend on
    // which node the binder happened to choose.
    let typeless = |col: u16| table.columns[col as usize].ty == ColumnType::Any;

    // Index MEMBERSHIP is "no indexed column of this row is NULL" (one rule,
    // `engine::index_row_key`). For a SINGLE-column index that is free: a
    // conjunct that pins the column (`a = 5`, `a > 5`) is 3VL-NULL on a NULL
    // `a`, so no row the probe should have returned is missing.
    //
    // A COMPOSITE index covered only to a PREFIX is a different claim, and the
    // one this guard exists for: `INDEX (a, b)` probed by `a = 5` returns the
    // entries with `a = 5` — which excludes every row whose `b` is NULL, and
    // those rows DO satisfy `a = 5`. Measured against sqlite 3.45: mpedb
    // answered `{1}` where sqlite answers `{1, 2}`. Fewer rows than exist.
    //
    // So a prefix of length `k` may be probed only when every column from `k`
    // on is declared NOT NULL — then membership IS "the pinned columns are
    // non-NULL", which the pinning conjuncts already guarantee. Full-width
    // coverage is the `k == columns.len()` case and always passes. This is the
    // same rule `plan::agg_servable_by_index` states for the aggregate-over-
    // index path ("…and the trailing columns NOT NULL"); the access paths were
    // simply never given it.
    let suffix_not_null = |ix: &mpedb_types::IndexDef, k: usize| -> bool {
        ix.columns[k.min(ix.columns.len())..]
            .iter()
            .all(|&c| !table.columns[c as usize].nullable)
    };

    // 1. Every PK column pinned by equality -> PkPoint.
    let pins: Vec<Option<(usize, BinOp, Atom)>> = table
        .primary_key
        .iter()
        .map(|&pk| if typeless(pk) { None } else { find(&consumed, pk, &[BinOp::Eq]) })
        .collect();
    if pins.iter().all(Option::is_some) {
        let mut parts = Vec::with_capacity(pins.len());
        for pin in pins.into_iter().flatten() {
            let (i, _, atom) = pin;
            consumed[i] = true;
            parts.push(atom.to_key_part(consts)?);
        }
        let residual = rebuild_residual(conjuncts, &consumed);
        return Ok((AccessPath::PkPoint(parts), residual));
    }

    // 2. Index-equality probe — BEFORE any PK range: an index probe touches
    // O(matches) rows, so it dominates an unbounded range scan (`WHERE pk >=
    // $1 AND email = $2` must not scan the range; the range conjuncts stay
    // behind as the residual filter). #55: an index matches on the LONGEST
    // equality-covered PREFIX of its columns (k = 1 is the single-column
    // case). Selection: a UNIQUE index covered to its FULL width (at most
    // one row) beats everything; otherwise the longest covered prefix wins,
    // ties to the lowest index_no — deterministic, and the only selectivity
    // facts the schema can state without statistics. Indexes beyond the
    // 64-bit footprint bitmap are never chosen.
    {
        // Per index: the equality pins covering its column prefix.
        let cover = |ix: &mpedb_types::IndexDef| -> Vec<(usize, Atom)> {
            let mut pins = Vec::new();
            for &col in &ix.columns {
                // See `typeless` above.
                if typeless(col) {
                    break;
                }
                match find(&consumed, col, &[BinOp::Eq]) {
                    Some((i, _, atom)) => pins.push((i, atom)),
                    None => break,
                }
            }
            pins
        };
        // (index position, covering pins, full-width-unique)
        type Candidate = (usize, Vec<(usize, Atom)>, bool);
        let mut best: Option<Candidate> = None;
        for (pos, ix) in table.indexes.iter().enumerate() {
            if pos >= 63 {
                break;
            }
            let pins = cover(ix);
            // An UNCOVERED nullable column past the pinned prefix makes the
            // probe lossy — see `suffix_not_null`.
            if pins.is_empty() || !suffix_not_null(ix, pins.len()) {
                continue;
            }
            // Partial index: usable only if the query predicate ENTAILS the
            // index predicate (§5.5) — otherwise the probe would miss rows
            // that exist. Checked after `cover` so a whole-table statement
            // never pays the predicate parse. `q` is the WHOLE conjunct list,
            // consumed parts included: a pinned equality still constrains the
            // rows the probe returns, so it is legitimate evidence.
            if ix.predicate.is_some() && !super::partial::index_usable(table, ix, &conjuncts) {
                continue;
            }
            let full_unique = ix.unique && pins.len() == ix.columns.len();
            let better = match &best {
                None => true,
                Some((_, bpins, bfull)) => {
                    (full_unique && !bfull)
                        || (full_unique == *bfull && pins.len() > bpins.len())
                }
            };
            if better {
                best = Some((pos, pins, full_unique));
            }
        }
        if let Some((pos, pins, _)) = best {
            let mut parts = Vec::with_capacity(pins.len());
            for (i, atom) in pins {
                consumed[i] = true;
                parts.push(atom.to_key_part(consts)?);
            }
            let residual = rebuild_residual(conjuncts, &consumed);
            return Ok((
                AccessPath::IndexPoint { index_no: (pos + 1) as u32, parts },
                residual,
            ));
        }
    }

    // A collated key column NEVER feeds a raw keycode BOUND: the executor builds
    // range bounds bytewise (no fold), so a bound over folded on-disk keys could
    // skip a matching row. Equality still works via the value-based Point paths
    // above (the engine folds those); only <, >, BETWEEN, and the multi-col-PK
    // equality-as-point-range fall through here — and for a collated column they
    // must stay a residual filter (`sql_cmp` honors the collation) over a scan.
    let collated = |col: u16| table.columns[col as usize].collation != Collation::Binary;

    // 3. Range over the first PK column. `typeless` blocks it for the same
    // reason `collated` does — the bound would be encoded raw, and for an
    // `any` column raw is not what the tree is keyed by (see above).
    let first_pk = table.primary_key[0];
    let unbounded = |col: u16| collated(col) || typeless(col);
    let mut lo = None;
    let mut hi = None;
    if table.primary_key.len() > 1 && !unbounded(first_pk) {
        // Equality on the first PK column of a multi-column PK when full
        // pinning failed: inclusive point-range lo = hi.
        if let Some((i, _, atom)) = find(&consumed, first_pk, &[BinOp::Eq]) {
            consumed[i] = true;
            let part = atom.to_key_part(consts)?;
            let bound = KeyBound {
                parts: vec![part],
                inclusive: true,
            };
            lo = Some(bound.clone());
            hi = Some(bound);
        }
    }
    if lo.is_none() && hi.is_none() && !unbounded(first_pk) {
        if let Some((i, op, atom)) = find(&consumed, first_pk, &[BinOp::Gt, BinOp::Ge]) {
            consumed[i] = true;
            lo = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Ge,
            });
        }
        if let Some((i, op, atom)) = find(&consumed, first_pk, &[BinOp::Lt, BinOp::Le]) {
            consumed[i] = true;
            hi = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Le,
            });
        }
    }
    if lo.is_some() || hi.is_some() {
        let residual = rebuild_residual(conjuncts, &consumed);
        return Ok((AccessPath::PkRange { lo, hi }, residual));
    }

    // 3.5 Range over a secondary index column (#48: IndexRange) — after
    // PkRange (the PK tree needs no per-hit row fetch, so it wins when both
    // have range conjuncts) and before a full scan. First index in
    // declaration order with a range conjunct wins; both bounds on the SAME
    // column are consumed together, everything else stays residual.
    for (pos, ix) in table.indexes.iter().enumerate() {
        if pos >= 63 {
            break; // beyond the footprint bitmap — never chosen
        }
        // Partial: same §5.5 implication test as the IndexPoint loop above.
        if ix.predicate.is_some() && !super::partial::index_usable(table, ix, &conjuncts) {
            continue;
        }
        // Phase-1 rule (same as PkRange): range over the FIRST index column
        // only — its encoding is a key prefix, so this serves composite
        // indexes unchanged. That makes the bound a coverage of exactly ONE
        // column, so the same NOT-NULL suffix rule the point probe takes
        // applies with k = 1 (`suffix_not_null` above): a nullable second
        // column would put rows the range covers outside the tree.
        let col = ix.columns[0];
        if unbounded(col) || !suffix_not_null(ix, 1) {
            continue; // collated / typeless / lossy composite: residual filter
        }
        let mut lo = None;
        let mut hi = None;
        if let Some((i, op, atom)) = find(&consumed, col, &[BinOp::Gt, BinOp::Ge]) {
            consumed[i] = true;
            lo = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Ge,
            });
        }
        if let Some((i, op, atom)) = find(&consumed, col, &[BinOp::Lt, BinOp::Le]) {
            consumed[i] = true;
            hi = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Le,
            });
        }
        if lo.is_some() || hi.is_some() {
            let residual = rebuild_residual(conjuncts, &consumed);
            return Ok((
                AccessPath::IndexRange {
                    index_no: (pos + 1) as u32,
                    lo,
                    hi,
                },
                residual,
            ));
        }
    }

    // 4. Full scan; the whole predicate stays as the filter.
    let residual = rebuild_residual(conjuncts, &consumed);
    Ok((AccessPath::FullScan, residual))
}
