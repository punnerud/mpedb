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

    // 1. Every PK column pinned by equality -> PkPoint.
    let pins: Vec<Option<(usize, BinOp, Atom)>> = table
        .primary_key
        .iter()
        .map(|&pk| find(&consumed, pk, &[BinOp::Eq]))
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
                // `any` can never be probed: index order is encoding order,
                // not sql_cmp order. The schema refuses indexing it, so this
                // is unreachable — kept as the planner's own guarantee.
                if table.columns[col as usize].ty == ColumnType::Any {
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
            if pins.is_empty() {
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

    // 3. Range over the first PK column.
    let first_pk = table.primary_key[0];
    let mut lo = None;
    let mut hi = None;
    if table.primary_key.len() > 1 {
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
    if lo.is_none() && hi.is_none() {
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
        // Phase-1 rule (same as PkRange): range over the FIRST index column
        // only — its encoding is a key prefix, so this serves composite
        // indexes unchanged.
        let col = ix.columns[0];
        if table.columns[col as usize].ty == ColumnType::Any {
            continue; // no order across types — see the equality guard
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
