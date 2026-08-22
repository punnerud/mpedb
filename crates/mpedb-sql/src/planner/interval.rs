//! Islands: the interval a predicate confines a column to.
//!
//! A predicate can bound a column without ever naming a bound.
//! `(lat-A)*(lat-A) + (lon-B)*(lon-B) < R*R` is a circle, and a circle is a
//! box in every axis — but the planner sees `f(col) > 0`, classifies it
//! UNKNOWN ([`super::mpee`]), and takes a full scan over a tree that could
//! have answered it.
//!
//! This module computes the box. Not by recognizing the expression — nothing
//! here knows Pythagoras, circles or geography — but by ARITHMETIC in the
//! interval domain, run BACKWARD: given that the whole predicate must hold,
//! what does each subexpression have to be, and therefore what does the column
//! have to be? The circle falls out of two facts the arithmetic already knows:
//! a square is non-negative, and a bounded sum of non-negative terms bounds
//! every term.
//!
//! # The contract, which is one-sided
//!
//! The interval is always a SUPERSET of the values that can satisfy the
//! predicate. Never a subset — a subset would silently drop rows, which is the
//! one failure mode a planner must not have. When the analysis cannot make
//! progress the answer is [`Iv::ALL`], which prices as a full scan and is
//! merely slow.
//!
//! Everything below preserves that direction. Where a step could only be
//! approximated, it widens; where widening is not obviously sound, it gives up
//! and returns `ALL`.
//!
//! # Scope
//!
//! Numbers only. Text and blob columns have an order but no arithmetic, so
//! there is nothing to invert; they take the ordinary equality/range paths in
//! [`super::access`]. NULL is not in the domain either: every comparison is
//! 3VL-NULL on a NULL operand, so a row with a NULL column cannot satisfy a
//! comparison, and excluding it from the interval is sound.

use crate::ast::BinOp;
use crate::binder::{BExpr, BUnOp};
use mpedb_types::Value;

/// A closed interval over the reals, as a conservative over-approximation.
///
/// Closed even when the predicate is strict: `x < 5` yields `..=5`, which is a
/// superset of `..<5`. Widening is always sound here, and carrying strictness
/// would buy a boundary row at the cost of a second field in every operation.
///
/// `lo > hi` is the empty interval — the predicate cannot be satisfied at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Iv {
    pub lo: f64,
    pub hi: f64,
}

impl Iv {
    /// Unbounded: the analysis learned nothing.
    pub const ALL: Iv = Iv { lo: f64::NEG_INFINITY, hi: f64::INFINITY };

    fn point(v: f64) -> Iv {
        Iv { lo: v, hi: v }
    }

    fn upto(v: f64) -> Iv {
        Iv { lo: f64::NEG_INFINITY, hi: v }
    }

    fn from(v: f64) -> Iv {
        Iv { lo: v, hi: f64::INFINITY }
    }

    /// Did the analysis bound anything at all?
    pub fn is_all(&self) -> bool {
        self.lo == f64::NEG_INFINITY && self.hi == f64::INFINITY
    }

    /// No value satisfies this.
    pub fn is_empty(&self) -> bool {
        self.lo > self.hi
    }

    /// Intersection — what AND does to two constraints on the same column.
    fn meet(self, o: Iv) -> Iv {
        Iv { lo: self.lo.max(o.lo), hi: self.hi.min(o.hi) }
    }

    /// Hull of the union — what OR does. The hull, not the union, because the
    /// result must stay a single interval; it is a superset either way.
    fn join(self, o: Iv) -> Iv {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        Iv { lo: self.lo.min(o.lo), hi: self.hi.max(o.hi) }
    }

    /// Guard every construction: a NaN bound would make comparisons false in
    /// both directions and turn the interval into something neither superset
    /// nor subset. Widen to ALL instead.
    fn sane(self) -> Iv {
        if self.lo.is_nan() || self.hi.is_nan() {
            Iv::ALL
        } else {
            self
        }
    }
}

/// The interval `col` is confined to by `conjuncts`, or `None` when unbounded.
///
/// `conjuncts` are the top-level AND terms of a WHERE clause (see
/// [`super::atoms::split_and`]). Each is analyzed on its own and the results intersected,
/// which is exactly what AND means for a superset.
pub(super) fn island(conjuncts: &[BExpr], col: u16) -> Option<Iv> {
    let mut acc = Iv::ALL;
    for c in conjuncts {
        acc = acc.meet(from_pred(c, col));
    }
    let acc = acc.sane();
    if acc.is_all() {
        None
    } else {
        Some(acc)
    }
}

/// What one predicate says about `col`.
fn from_pred(p: &BExpr, col: u16) -> Iv {
    match p {
        BExpr::Binary(BinOp::And, a, b) => from_pred(a, col).meet(from_pred(b, col)),

        // OR constrains only as far as BOTH arms agree. An arm that says
        // nothing makes the whole disjunction say nothing, which `join` gives
        // for free (hull with ALL is ALL).
        BExpr::Binary(BinOp::Or, a, b) => from_pred(a, col).join(from_pred(b, col)),

        BExpr::Binary(op, l, r) if is_cmp(*op) => {
            // One side must be a known number; the other is the expression to
            // invert. Both sides constant tells us nothing about any column,
            // and both sides variable is beyond this analysis.
            if let Some(k) = num(r) {
                return back(l, col, target(*op, k));
            }
            if let Some(k) = num(l) {
                // `k < expr` is `expr > k`.
                return back(r, col, target(flip(*op), k));
            }
            Iv::ALL
        }

        // A bare NOT could be pushed through, but `NOT (a < b)` is `a >= b`
        // only in 2-valued logic — under 3VL a NULL operand makes both false.
        // The safe reading of a negation is that it constrains nothing.
        _ => Iv::ALL,
    }
}

/// The interval an expression must land in for `expr <op> k` to hold.
fn target(op: BinOp, k: f64) -> Iv {
    match op {
        BinOp::Lt | BinOp::Le => Iv::upto(k),
        BinOp::Gt | BinOp::Ge => Iv::from(k),
        BinOp::Eq => Iv::point(k),
        // `<>` excludes one value out of a continuum: the superset is
        // everything.
        _ => Iv::ALL,
    }
}

/// `k <op> expr` read as `expr <flip(op)> k`.
fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

fn is_cmp(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

/// The backward step: given that `e` must lie in `t`, where must `col` lie?
///
/// Every arm either inverts exactly or widens. An arm that cannot do either
/// returns `ALL`, which loses the bound but never the rows.
fn back(e: &BExpr, col: u16, t: Iv) -> Iv {
    if t.is_all() {
        return Iv::ALL;
    }
    match e {
        // The column itself: the target IS the answer.
        BExpr::Col(c) if *c == col => t.sane(),

        // Some other column, or a value this analysis cannot see through.
        // Whatever it is, it says nothing about `col`.
        BExpr::Col(_) | BExpr::Param(_) | BExpr::Const(_) => Iv::ALL,

        BExpr::Binary(BinOp::Add, a, b) => {
            // Constant on one side: shift the target the other way.
            if let Some(k) = num(b) {
                return back(a, col, Iv { lo: t.lo - k, hi: t.hi - k }.sane());
            }
            if let Some(k) = num(a) {
                return back(b, col, Iv { lo: t.lo - k, hi: t.hi - k }.sane());
            }
            // Both sides variable — and this is the arm the circle needs. If
            // both terms are provably non-negative and the sum is bounded
            // above, then each term is bounded above by the same number: no
            // term can exceed a sum it only adds to. That is the whole of
            // Pythagoras here, and it is stated without mentioning it.
            if t.hi.is_finite() && nonneg(a) && nonneg(b) {
                let each = Iv::upto(t.hi);
                return back(a, col, each).meet(back(b, col, each)).sane();
            }
            Iv::ALL
        }

        BExpr::Binary(BinOp::Sub, a, b) => {
            if let Some(k) = num(b) {
                // `a - k ∈ t`  ⇒  `a ∈ t + k`
                return back(a, col, Iv { lo: t.lo + k, hi: t.hi + k }.sane());
            }
            if let Some(k) = num(a) {
                // `k - b ∈ t`  ⇒  `b ∈ k - t`, which flips the ends.
                return back(b, col, Iv { lo: k - t.hi, hi: k - t.lo }.sane());
            }
            Iv::ALL
        }

        BExpr::Binary(BinOp::Mul, a, b) => {
            // A square, written the way SQL writes one. `x*x ∈ ..=h` bounds
            // |x| by sqrt(h) — the step that turns a circle into a box.
            if same(a, b) {
                if t.hi < 0.0 {
                    // A square is never negative: nothing satisfies this.
                    return Iv { lo: 1.0, hi: -1.0 };
                }
                if t.hi.is_finite() {
                    let r = t.hi.sqrt();
                    return back(a, col, Iv { lo: -r, hi: r }.sane());
                }
                return Iv::ALL;
            }
            if let Some(k) = num(b) {
                return back(a, col, div_target(t, k));
            }
            if let Some(k) = num(a) {
                return back(b, col, div_target(t, k));
            }
            Iv::ALL
        }

        BExpr::Binary(BinOp::Div, a, b) | BExpr::Binary(BinOp::DivStrict, a, b) => {
            // Only `expr / k`. `k / expr` inverts through a pole at zero and
            // is not worth the care it would need.
            if let Some(k) = num(b) {
                if k == 0.0 {
                    return Iv::ALL;
                }
                let scaled = if k > 0.0 {
                    Iv { lo: t.lo * k, hi: t.hi * k }
                } else {
                    Iv { lo: t.hi * k, hi: t.lo * k }
                };
                return back(a, col, scaled.sane());
            }
            Iv::ALL
        }

        // `-x ∈ t`  ⇒  `x ∈ -t`, ends swapped.
        BExpr::Unary(BUnOp::Neg, a) => back(a, col, Iv { lo: -t.hi, hi: -t.lo }.sane()),

        // A widening to float changes representation, not value or order.
        BExpr::Unary(BUnOp::ToFloat, a) => back(a, col, t),

        _ => Iv::ALL,
    }
}

/// `expr * k ∈ t`  ⇒  `expr ∈ t / k`, with the ends swapped for a negative
/// `k`. A zero factor makes the product constant, which says nothing.
fn div_target(t: Iv, k: f64) -> Iv {
    if k == 0.0 {
        return Iv::ALL;
    }
    let d = if k > 0.0 {
        Iv { lo: t.lo / k, hi: t.hi / k }
    } else {
        Iv { lo: t.hi / k, hi: t.lo / k }
    };
    d.sane()
}

/// Is this expression non-negative for every input?
///
/// Only shapes that are non-negative BY CONSTRUCTION count. Being wrong here
/// in the permissive direction would break the sum rule above and could
/// under-approximate, so anything uncertain answers `false`.
fn nonneg(e: &BExpr) -> bool {
    match e {
        BExpr::Const(v) => num_of(v).is_some_and(|k| k >= 0.0),
        // A square, however deeply nested.
        BExpr::Binary(BinOp::Mul, a, b) if same(a, b) => true,
        // A sum or product of non-negatives stays non-negative.
        BExpr::Binary(BinOp::Add, a, b) | BExpr::Binary(BinOp::Mul, a, b) => {
            nonneg(a) && nonneg(b)
        }
        _ => false,
    }
}

/// Structural equality, enough to recognize `x * x` as a square.
///
/// Deliberately shallow: it compares the shapes this analysis can invert, and
/// answers `false` for everything else. A false negative costs a bound; a
/// false positive would claim a square where there is none, so the arms that
/// could go wrong are simply not written.
fn same(a: &BExpr, b: &BExpr) -> bool {
    match (a, b) {
        (BExpr::Col(x), BExpr::Col(y)) => x == y,
        (BExpr::Param(x), BExpr::Param(y)) => x == y,
        (BExpr::Const(x), BExpr::Const(y)) => x == y,
        (BExpr::Binary(o1, a1, b1), BExpr::Binary(o2, a2, b2)) => {
            o1 == o2 && same(a1, a2) && same(b1, b2)
        }
        (BExpr::Unary(o1, x), BExpr::Unary(o2, y)) => o1 == o2 && same(x, y),
        (BExpr::Cast(x, a1), BExpr::Cast(y, a2)) => a1 == a2 && same(x, y),
        _ => false,
    }
}

/// A literal's numeric value, if it has one. Parameters are deliberately NOT
/// resolved: their value is not known at plan time, and a plan is compiled
/// once and reused for every binding.
fn num(e: &BExpr) -> Option<f64> {
    match e {
        BExpr::Const(v) => num_of(v),
        _ => None,
    }
}

fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(n: u16) -> BExpr {
        BExpr::Col(n)
    }

    fn lit(x: f64) -> BExpr {
        BExpr::Const(Value::Float(x))
    }

    fn bin(op: BinOp, a: BExpr, b: BExpr) -> BExpr {
        BExpr::Binary(op, Box::new(a), Box::new(b))
    }

    /// `(c - k) * (c - k)` — a squared offset, the shape a distance predicate
    /// is built from.
    fn sq_off(c: u16, k: f64) -> BExpr {
        let d = bin(BinOp::Sub, col(c), lit(k));
        bin(BinOp::Mul, d.clone(), d)
    }

    /// The load-bearing property: every value that CAN satisfy the predicate
    /// must lie inside the island. A subset would drop rows, and drop them
    /// silently — the one failure this analysis must never have.
    ///
    /// Checked by brute force against the predicate's own meaning, written
    /// twice: once as a `BExpr` for the analysis, once as a Rust closure for
    /// the truth. Agreement between two independent statements of the same
    /// predicate is the point; one of them alone proves nothing.
    fn superset(name: &str, pred: &BExpr, truth: impl Fn(f64, f64) -> bool) {
        let iv = island(std::slice::from_ref(pred), 0);
        let mut satisfied = 0usize;
        let mut lo_seen = f64::INFINITY;
        let mut hi_seen = f64::NEG_INFINITY;
        for i in -400i32..=400 {
            for j in -60i32..=60 {
                let (x, y) = (i as f64 * 0.05, j as f64 * 0.05);
                if !truth(x, y) {
                    continue;
                }
                satisfied += 1;
                lo_seen = lo_seen.min(x);
                hi_seen = hi_seen.max(x);
                if let Some(iv) = iv {
                    assert!(
                        x >= iv.lo && x <= iv.hi,
                        "{name}: ({x}, {y}) satisfies the predicate but lies outside \
                         the island [{}, {}] — the analysis LOST a row",
                        iv.lo,
                        iv.hi
                    );
                }
            }
        }
        assert!(satisfied > 0, "{name}: the test predicate is never satisfiable — fix the case");
        if let Some(iv) = iv {
            // Sanity in the other direction: an island that is not actually
            // narrower than what the rows occupy proves nothing about the
            // analysis working, only about it not lying.
            eprintln!(
                "{name}: rows span [{lo_seen}, {hi_seen}], island [{}, {}]",
                iv.lo, iv.hi
            );
        }
    }

    #[test]
    fn circle_becomes_a_box_without_knowing_it_is_a_circle() {
        // (c0 - 3)^2 + (c1 - 1)^2 < 4  — a circle of radius 2 about (3, 1).
        // The analysis must find c0 ∈ [1, 5] from arithmetic alone.
        let pred = bin(
            BinOp::Lt,
            bin(BinOp::Add, sq_off(0, 3.0), sq_off(1, 1.0)),
            lit(4.0),
        );
        superset("circle", &pred, |x, y| {
            (x - 3.0).powi(2) + (y - 1.0).powi(2) < 4.0
        });
        let iv = island(std::slice::from_ref(&pred), 0).expect("circle must bound c0");
        assert!(iv.lo <= 1.0 && iv.lo > 0.9, "lower bound {} should be ~1", iv.lo);
        assert!(iv.hi >= 5.0 && iv.hi < 5.1, "upper bound {} should be ~5", iv.hi);
        // And it must bound the OTHER axis just as well — a box, not a slab.
        let iv1 = island(std::slice::from_ref(&pred), 1).expect("circle must bound c1");
        assert!(iv1.lo <= -1.0 && iv1.lo > -1.1, "c1 lower {} should be ~-1", iv1.lo);
        assert!(iv1.hi >= 3.0 && iv1.hi < 3.1, "c1 upper {} should be ~3", iv1.hi);
    }

    #[test]
    fn plain_range_and_arithmetic() {
        // c0 * 2 <= 10  ⇒  c0 ≤ 5
        let p = bin(BinOp::Le, bin(BinOp::Mul, col(0), lit(2.0)), lit(10.0));
        superset("mul", &p, |x, _| x * 2.0 <= 10.0);
        assert_eq!(island(std::slice::from_ref(&p), 0).unwrap().hi, 5.0);

        // c0 + 7 > 10  ⇒  c0 > 3
        let p = bin(BinOp::Gt, bin(BinOp::Add, col(0), lit(7.0)), lit(10.0));
        superset("add", &p, |x, _| x + 7.0 > 10.0);
        assert_eq!(island(std::slice::from_ref(&p), 0).unwrap().lo, 3.0);

        // 10 > c0 - 2  ⇒  c0 < 12   (constant on the LEFT)
        let p = bin(BinOp::Gt, lit(10.0), bin(BinOp::Sub, col(0), lit(2.0)));
        superset("flip", &p, |x, _| 10.0 > x - 2.0);
        assert_eq!(island(std::slice::from_ref(&p), 0).unwrap().hi, 12.0);

        // A NEGATIVE factor must swap the ends, not keep them.
        // c0 * -2 <= 10  ⇒  c0 ≥ -5
        let p = bin(BinOp::Le, bin(BinOp::Mul, col(0), lit(-2.0)), lit(10.0));
        superset("neg-factor", &p, |x, _| x * -2.0 <= 10.0);
        let iv = island(std::slice::from_ref(&p), 0).unwrap();
        assert_eq!(iv.lo, -5.0);
        assert!(iv.hi.is_infinite());
    }

    #[test]
    fn and_narrows_or_widens() {
        let lo = bin(BinOp::Ge, col(0), lit(2.0));
        let hi = bin(BinOp::Le, col(0), lit(8.0));

        let both = [lo.clone(), hi.clone()];
        let iv = island(&both, 0).unwrap();
        assert_eq!((iv.lo, iv.hi), (2.0, 8.0));

        // OR takes the hull, so an arm that constrains a DIFFERENT column
        // leaves this one unbounded.
        let other = bin(BinOp::Le, col(1), lit(0.0));
        let either = bin(BinOp::Or, lo.clone(), other);
        assert!(island(std::slice::from_ref(&either), 0).is_none());

        // Two arms on the same column: the hull covers both.
        let either = bin(BinOp::Or, bin(BinOp::Le, col(0), lit(1.0)), hi);
        let iv = island(std::slice::from_ref(&either), 0).unwrap();
        assert!(iv.lo.is_infinite() && iv.hi == 8.0);
    }

    #[test]
    fn unbounded_is_always_a_legal_answer() {
        // A parameter is not known at plan time.
        let p = bin(BinOp::Lt, col(0), BExpr::Param(1));
        assert!(island(std::slice::from_ref(&p), 0).is_none());

        // Another column says nothing about this one.
        let p = bin(BinOp::Lt, col(1), lit(5.0));
        assert!(island(std::slice::from_ref(&p), 0).is_none());

        // `<>` removes one point from a continuum.
        let p = bin(BinOp::Ne, col(0), lit(5.0));
        assert!(island(std::slice::from_ref(&p), 0).is_none());

        // A shape with no inverse here.
        let p = bin(BinOp::Lt, bin(BinOp::Mod, col(0), lit(3.0)), lit(1.0));
        assert!(island(std::slice::from_ref(&p), 0).is_none());

        // A sum of terms that are NOT provably non-negative must not use the
        // sum rule: `c0 + c1 < 4` allows any c0 whatsoever.
        let p = bin(BinOp::Lt, bin(BinOp::Add, col(0), col(1)), lit(4.0));
        superset("open-sum", &p, |x, y| x + y < 4.0);
        assert!(
            island(std::slice::from_ref(&p), 0).is_none(),
            "an unbounded sum must not be mistaken for a bounded one"
        );
    }

    #[test]
    fn a_square_below_zero_is_unsatisfiable() {
        // x*x < -1 holds for nothing; the empty interval says so.
        let p = bin(BinOp::Lt, sq_off(0, 0.0), lit(-1.0));
        let iv = island(std::slice::from_ref(&p), 0).expect("bounded");
        assert!(iv.is_empty(), "expected empty, got [{}, {}]", iv.lo, iv.hi);
    }

    #[test]
    fn a_lower_bound_on_a_square_does_not_bound_the_column() {
        // x*x > 4 is satisfied by x = 100 and x = -100 alike: no interval.
        let p = bin(BinOp::Gt, sq_off(0, 0.0), lit(4.0));
        superset("sq-lower", &p, |x, _| x * x > 4.0);
        assert!(island(std::slice::from_ref(&p), 0).is_none());
    }
}
