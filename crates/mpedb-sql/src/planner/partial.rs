//! Partial-index implication — "may this partial index answer this query?"
//! (design/DESIGN-WORKLOAD-INDEXES.md §5.5, **v1**).
//!
//! A partial index `I` carries a predicate `p`; only rows satisfying `p` are
//! members. So `I` may serve a query with predicate `q` **only if `q ⇒ p`** —
//! otherwise the probe misses rows that exist. That is the one failure mode
//! this whole module exists to prevent, and it is why the test is
//! **sound by construction and deliberately incomplete**: it proves the
//! implication or it declines, and declining only costs a full scan.
//!
//! # What v1 ships
//!
//! §5.5 lists an entailment lattice with six rows. v1 ships **rows 1–3 only** —
//! exact atom match plus the `IS NOT NULL` weakenings — because those are the
//! rows whose soundness is a one-line argument. The range-subsumption rows
//! (`c > 5` entails `c > 3`) are a v2 with a differential test per row: an
//! off-by-one there returns fewer rows than exist, which is not a slow query
//! but a wrong answer.
//!
//! Everything else is refused **by name**, in [`canon_atom`]:
//!
//! - a disjunction, a `NOT`, a bare boolean column, `LIKE`/`GLOB`/`REGEXP`,
//!   `IN`, a function call, an expression on either side of a comparison;
//! - a comparison against `NULL` (`c = NULL` is never TRUE, so a predicate
//!   containing it selects nothing — a shape not worth reasoning about);
//! - [`BExpr::ClassCmp`] / [`BExpr::CollateCmp`], the typeless-column and
//!   explicit-`COLLATE` comparison nodes. Their meaning depends on an affinity
//!   and a collation the atom does not carry, so two structurally equal atoms
//!   could still mean different things — exactly the confusion an implication
//!   test must not have.
//!
//! # Why structural equality is the right comparison
//!
//! §5.5 says value comparison "uses the column's collation". This module
//! instead compares [`Value`]s **structurally** (derived `PartialEq`). That is
//! strictly narrower and can only decline: under `NOCASE`, `p: c = 'Bob'` and
//! `q: c = 'bob'` really are the same predicate, and we decline. It can never
//! over-claim, because both atoms name the SAME column of the SAME table — so
//! identical `(column, op, value)` triples are the identical predicate,
//! whatever collation or affinity that column carries.
//!
//! # Parameters
//!
//! `WHERE status = $1` does not imply `WHERE status = 'active'`: the compiler
//! does not know `$1`. A query atom over a parameter is therefore not evidence
//! (it is silently not canonicalized), and a *parameterized index predicate* is
//! refused at CREATE INDEX time already. §5.5's `AccessPath::Guarded` (P6) is
//! what would lift this; it is not v1.

use super::*;
use crate::binder::BUnOp;

/// One canonical predicate atom over a single column slot of one table.
///
/// Deliberately tiny: this is the entire vocabulary the implication test can
/// reason about, and anything outside it is not evidence and not provable.
#[derive(Debug, Clone, PartialEq)]
enum PAtom {
    IsNull(u16),
    IsNotNull(u16),
    /// `col <op> <non-NULL literal>`, `op` one of the six comparisons.
    Cmp(u16, BinOp, Value),
}

/// May index `ix` of `table` be used as an access path for a query whose WHERE
/// splits into `conjuncts`?
///
/// `true` for every whole-table index (the overwhelming case, and free — no
/// parse, no bind). For a partial index this parses and binds the stored
/// predicate source, canonicalizes both sides into [`PAtom`]s and demands that
/// **every** atom of the index predicate be entailed by **some** atom of the
/// query. Any step that cannot be taken answers `false`.
///
/// `conjuncts` must be the WHOLE query predicate — including the conjuncts the
/// access-path extractor has already consumed into key parts, since those still
/// constrain the rows the probe returns and are therefore legitimate evidence
/// (§5.5 step 1).
pub(super) fn index_usable(
    table: &TableDef,
    ix: &mpedb_types::IndexDef,
    conjuncts: &[BExpr],
) -> bool {
    let Some(src) = &ix.predicate else {
        return true; // whole-table: nothing to prove
    };
    let Some(p_atoms) = predicate_atoms(table, src) else {
        return false;
    };
    // An empty conjunct list is `q = TRUE`, which implies nothing but `TRUE`.
    let q_atoms: Vec<PAtom> = conjuncts.iter().filter_map(canon_atom).collect();
    p_atoms
        .iter()
        .all(|p| q_atoms.iter().any(|q| entails(q, p)))
}

/// The index predicate's canonical atoms, or `None` if any part of it is
/// outside the v1 vocabulary (in which case the index is unusable — an index
/// mpedb cannot reason about is an index it must not probe).
fn predicate_atoms(table: &TableDef, src: &str) -> Option<Vec<PAtom>> {
    let (expr, n_params) = crate::parser::parse_expr_only(src).ok()?;
    if n_params > 0 {
        // Refused at CREATE INDEX already; belt and braces, because a schema
        // can also arrive from a config file or a foreign catalog.
        return None;
    }
    // `allow_params = false` mirrors `compile_check`: a predicate is a property
    // of the index, so nothing session- or statement-scoped may enter it.
    let mut binder = Binder::new(table, 0, false);
    let bound = binder.bind_check(&expr).ok()?;
    let mut parts = Vec::new();
    split_and(bound, &mut parts);
    parts.iter().map(canon_atom).collect()
}

/// One conjunct → one atom, or `None` for anything outside the v1 vocabulary.
///
/// Matching ONLY [`BExpr::Binary`] and the two `IS [NOT] NULL` unary nodes is
/// what makes structural [`Value`] equality sound (see the module docs): the
/// collated and typeless comparison nodes carry semantics in fields the atom
/// does not record, so they are not canonicalized at all.
fn canon_atom(e: &BExpr) -> Option<PAtom> {
    let flipped = |op: BinOp| match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    };
    match e {
        BExpr::Unary(BUnOp::IsNull, a) => match a.as_ref() {
            BExpr::Col(c) => Some(PAtom::IsNull(*c)),
            _ => None,
        },
        BExpr::Unary(BUnOp::IsNotNull, a) => match a.as_ref() {
            BExpr::Col(c) => Some(PAtom::IsNotNull(*c)),
            _ => None,
        },
        BExpr::Binary(op, l, r) if is_cmp(*op) => match (l.as_ref(), r.as_ref()) {
            // A NULL literal is refused on both sides: `c = NULL` is NULL, never
            // TRUE, so it is neither usable evidence nor a predicate worth
            // proving — and admitting it would put a value in the atom that no
            // comparison can ever match.
            (BExpr::Col(c), BExpr::Const(v)) if !v.is_null() => {
                Some(PAtom::Cmp(*c, *op, v.clone()))
            }
            (BExpr::Const(v), BExpr::Col(c)) if !v.is_null() => {
                Some(PAtom::Cmp(*c, flipped(*op), v.clone()))
            }
            _ => None,
        },
        _ => None,
    }
}

fn is_cmp(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    )
}

/// Does query atom `q` entail index atom `p`? Rows 1–3 of the §5.5 lattice,
/// and **only** those.
///
/// The soundness argument, one line per arm:
///
/// - identical atoms: `x ⇒ x`.
/// - `c <op> v` with `v` non-NULL and `op ≠ ≠` is 3-valued NULL when `c` is
///   NULL, so a row passing it has `c` non-NULL: it entails `IS NOT NULL`.
///   `≠` is excluded because §5.5 excludes it — the lattice is the reviewed
///   contract and narrower is always allowed.
fn entails(q: &PAtom, p: &PAtom) -> bool {
    match (p, q) {
        // Row 1.
        (PAtom::IsNull(pc), PAtom::IsNull(qc)) => pc == qc,
        // Row 2, exact.
        (PAtom::IsNotNull(pc), PAtom::IsNotNull(qc)) => pc == qc,
        // Row 2, the weakening: any non-≠ comparison against a non-NULL value.
        (PAtom::IsNotNull(pc), PAtom::Cmp(qc, op, v)) => {
            pc == qc && *op != BinOp::Ne && !v.is_null()
        }
        // Row 3, exact atom match. Structural value equality — see module docs.
        (PAtom::Cmp(pc, pop, pv), PAtom::Cmp(qc, qop, qv)) => {
            pc == qc && pop == qop && pv == qv
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::{ColumnDef, IndexDef};

    fn col(name: &str, ty: ColumnType, nullable: bool) -> ColumnDef {
        ColumnDef {
            generated: None,
            decl: None,
            name: name.into(),
            ty,
            nullable,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: mpedb_types::Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ty),
        }
    }

    fn tbl() -> TableDef {
        TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                col("id", ColumnType::Int64, false),
                col("a", ColumnType::Int64, true),
                col("b", ColumnType::Text, true),
            ],
            primary_key: vec![0],
            indexes: Vec::new(),
            dead: false,
            implicit_rowid: false,
            kind: mpedb_types::TableKind::Standard,
        }
    }

    fn ix(pred: Option<&str>) -> IndexDef {
        IndexDef {
            columns: vec![1],
            unique: false,
            predicate: pred.map(|s| s.to_string()),
        }
    }

    /// Bind a WHERE-shaped source string into conjuncts, the way the planner
    /// hands them to [`index_usable`].
    fn conj(t: &TableDef, src: &str) -> Vec<BExpr> {
        let (e, n) = crate::parser::parse_expr_only(src).unwrap();
        let mut binder = Binder::new(t, n, true);
        let b = binder.bind_predicate(&e).unwrap();
        let mut out = Vec::new();
        split_and(b, &mut out);
        out
    }

    #[test]
    fn whole_table_index_is_always_usable() {
        let t = tbl();
        assert!(index_usable(&t, &ix(None), &conj(&t, "a = 1")));
        assert!(index_usable(&t, &ix(None), &[]));
    }

    #[test]
    fn is_not_null_predicate_and_its_weakenings() {
        let t = tbl();
        let i = ix(Some("a IS NOT NULL"));
        // exact
        assert!(index_usable(&t, &i, &conj(&t, "a IS NOT NULL")));
        // the weakening: any non-≠ comparison against a non-NULL literal
        assert!(index_usable(&t, &i, &conj(&t, "a = 7")));
        assert!(index_usable(&t, &i, &conj(&t, "a > 7")));
        assert!(index_usable(&t, &i, &conj(&t, "a <= 7")));
        assert!(index_usable(&t, &i, &conj(&t, "a = 7 AND b = 'x'")));
        // ≠ is deliberately NOT a weakening in v1 (§5.5 excludes it)
        assert!(!index_usable(&t, &i, &conj(&t, "a <> 7")));
        // a parameter is not evidence
        assert!(!index_usable(&t, &i, &conj(&t, "a = $1")));
        // the wrong column proves nothing
        assert!(!index_usable(&t, &i, &conj(&t, "b IS NOT NULL")));
        // and neither does IS NULL, which is the OPPOSITE claim
        assert!(!index_usable(&t, &i, &conj(&t, "a IS NULL")));
        assert!(!index_usable(&t, &i, &[]));
    }

    #[test]
    fn is_null_predicate_needs_the_same_claim() {
        let t = tbl();
        let i = ix(Some("b IS NULL"));
        assert!(index_usable(&t, &i, &conj(&t, "a = 1 AND b IS NULL")));
        assert!(!index_usable(&t, &i, &conj(&t, "a = 1")));
        assert!(!index_usable(&t, &i, &conj(&t, "b IS NOT NULL")));
        // A comparison NEVER entails IS NULL — it excludes NULL.
        assert!(!index_usable(&t, &i, &conj(&t, "b = 'x'")));
    }

    #[test]
    fn equality_predicate_needs_the_identical_atom() {
        let t = tbl();
        let i = ix(Some("a = 5"));
        assert!(index_usable(&t, &i, &conj(&t, "a = 5")));
        assert!(index_usable(&t, &i, &conj(&t, "5 = a"))); // operand order canonicalized
        assert!(index_usable(&t, &i, &conj(&t, "a = 5 AND b = 'x'")));
        // v1 ships NO range subsumption: `a = 5` really does imply `a > 4`,
        // but proving it is v2 with its own differential battery.
        assert!(!index_usable(&t, &ix(Some("a > 4")), &conj(&t, "a = 5")));
        assert!(!index_usable(&t, &i, &conj(&t, "a = 6")));
        assert!(!index_usable(&t, &i, &conj(&t, "a > 5")));
        assert!(!index_usable(&t, &i, &conj(&t, "a IS NOT NULL")));
    }

    #[test]
    fn a_conjunction_predicate_needs_every_atom() {
        let t = tbl();
        let i = ix(Some("a IS NOT NULL AND b IS NULL"));
        assert!(index_usable(&t, &i, &conj(&t, "a = 1 AND b IS NULL")));
        assert!(!index_usable(&t, &i, &conj(&t, "a = 1")));
        assert!(!index_usable(&t, &i, &conj(&t, "b IS NULL")));
    }

    #[test]
    fn a_predicate_outside_the_vocabulary_is_refused_whole() {
        let t = tbl();
        // Disjunction, LIKE, NOT, a bare expression — none canonicalize, so the
        // index is never usable, not even by a query that repeats it verbatim.
        for pred in [
            "a IS NOT NULL OR b IS NULL",
            "b LIKE 'x%'",
            "NOT (a IS NULL)",
            "a + 1 = 5",
            "abs(a) = 5",
            "a = NULL",
        ] {
            let i = ix(Some(pred));
            assert!(
                !index_usable(&t, &i, &conj(&t, pred)),
                "predicate `{pred}` must not be provable"
            );
        }
    }

    #[test]
    fn an_unparsable_predicate_declines_rather_than_panics() {
        let t = tbl();
        for pred in ["", "a IS NOT", "nosuchcol IS NULL", ")("] {
            assert!(!index_usable(&t, &ix(Some(pred)), &conj(&t, "a = 1")));
        }
    }
}
