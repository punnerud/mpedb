//! FTS `MATCH` execution — posting-list set algebra (design/DESIGN-FTS.md §4).
//!
//! Evaluate a compiled [`FtsQuery`] against the inverted index into the sorted,
//! distinct set of matching rowids: a term yields the docids of its postings
//! (across its allowed columns, honoring `prefix`/`^initial`); `AND` intersects,
//! `OR` merges, `X NOT Y` differences. The intersect is driven by the RARER side
//! (the MPEE "rarest term first" heuristic, design/DESIGN-MPEE-OPT.md), and every
//! posting entry visited charges the #74 work meter, so a runaway MATCH trips the
//! runtime budget like any scan. Results are ascending by rowid (stage 1 has no
//! ranking).

use super::TxnCtx;
use mpedb_sql::{FtsQuery, FtsTerm};
use mpedb_types::fts::{self, Doclist};
use mpedb_types::{Error, Result};
use std::collections::BTreeSet;

/// The matching rowids of `query` against FTS table `table`, ascending and
/// distinct.
pub(super) fn evaluate(ctx: &mut dyn TxnCtx, table: u32, query: &FtsQuery) -> Result<Vec<i64>> {
    match query {
        FtsQuery::Term(t) => term_docs(ctx, table, t),
        FtsQuery::And(a, b) => {
            // Drive the intersect by the RARER (smaller) candidate set: the
            // other side is only probed, never materialized further — the
            // "rarest term first" collapse (design/DESIGN-MPEE-OPT.md).
            let mut da = evaluate(ctx, table, a)?;
            let mut db = evaluate(ctx, table, b)?;
            if db.len() < da.len() {
                std::mem::swap(&mut da, &mut db);
            }
            Ok(intersect(&da, &db))
        }
        FtsQuery::Or(a, b) => {
            let da = evaluate(ctx, table, a)?;
            let db = evaluate(ctx, table, b)?;
            Ok(union(&da, &db))
        }
        FtsQuery::AndNot(a, b) => {
            let da = evaluate(ctx, table, a)?;
            let db = evaluate(ctx, table, b)?;
            Ok(difference(&da, &db))
        }
    }
}

/// The docids matching one term. Both exact and prefix terms are prefix scans of
/// the inverted index: an exact term selects `token ‖ 0x00` (that token, every
/// column); a prefix term selects `token` (every indexed term starting with it,
/// every column). Column filters and `^initial` are applied here.
fn term_docs(ctx: &mut dyn TxnCtx, table: u32, t: &FtsTerm) -> Result<Vec<i64>> {
    let scan_prefix = if t.prefix {
        fts::posting_key_scan_prefix(&t.token)
    } else {
        fts::posting_key_exact_prefix(&t.token)
    };
    let entries = ctx.fts_prefix(table, &scan_prefix)?;
    let mut set: BTreeSet<i64> = BTreeSet::new();
    for (key, dl_bytes) in entries {
        let colno = fts::posting_key_colno(&key)
            .ok_or_else(|| Error::Corrupt("malformed FTS posting key".into()))?;
        if !t.columns.is_empty() && !t.columns.contains(&colno) {
            continue;
        }
        let dl = Doclist::decode(&dl_bytes)?;
        for (docid, positions) in &dl.docs {
            // One work unit per posting entry visited (#74).
            ctx.charge_work(1, &|| "fts posting scan".to_string())?;
            // `^term`: the token must occur at position 0 in this column.
            if t.initial && positions.first() != Some(&0) {
                continue;
            }
            set.insert(*docid);
        }
    }
    Ok(set.into_iter().collect())
}

/// Sorted-set intersection. `a` is the rarer (smaller) side; each of its docids
/// is probed against `b`. Output stays ascending.
fn intersect(a: &[i64], b: &[i64]) -> Vec<i64> {
    a.iter().copied().filter(|x| b.binary_search(x).is_ok()).collect()
}

/// Sorted-set union (merge, deduplicated). Both inputs ascending.
fn union(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Sorted-set difference `a \ b` (`X NOT Y`), preserving `a`'s ascending order.
fn difference(a: &[i64], b: &[i64]) -> Vec<i64> {
    a.iter().copied().filter(|x| b.binary_search(x).is_err()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ops() {
        assert_eq!(intersect(&[1, 2, 3, 5], &[2, 5, 8]), vec![2, 5]);
        assert_eq!(union(&[1, 3, 5], &[2, 3, 6]), vec![1, 2, 3, 5, 6]);
        assert_eq!(difference(&[1, 2, 3, 4], &[2, 4]), vec![1, 3]);
        assert_eq!(intersect(&[], &[1, 2]), Vec::<i64>::new());
        assert_eq!(union(&[], &[1, 2]), vec![1, 2]);
        assert_eq!(difference(&[1, 2], &[]), vec![1, 2]);
    }
}
