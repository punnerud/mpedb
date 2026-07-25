//! **Fractional order keys: moving one thing without renumbering the rest (#146).**
//!
//! A document that many people edit is a table of blocks, not one field — which
//! is what makes different paragraphs different *rows* and therefore free of
//! conflict (#143). What that reframing needs is an ordering that can be
//! changed one row at a time.
//!
//! With an integer `ord`, inserting between positions 3 and 4 means renumbering
//! everything after it: one move writes the whole document and collides with
//! every other editor. With a **fractional** key there is always room between
//! any two neighbours, so a move writes exactly one row and two people moving
//! different blocks never meet.
//!
//! That is also what makes concurrent edits *follow* a move. The mover writes
//! `ord`; someone editing the same block writes `body`. Different columns, no
//! conflict, and the edit lands on the block wherever it now sits — because it
//! was anchored to the block and not to an offset in a monolith. The hard case
//! in collaborative editing stops being hard when the move stops being
//! delete-plus-insert.
//!
//! # The model
//!
//! A key is a base-256 fraction: `[0x80]` is 0.5, `[0x40, 0x80]` is
//! 0.25 + 1/512. `between` is the midpoint, computed exactly — add the two
//! fractions, halve, and take the one extra digit that halving may need. There
//! is always a midpoint, so `between` never fails on a well-formed pair.
//!
//! **Invariant: no trailing zero bytes.** The fraction model and `memcmp` agree
//! on every key that has none, and disagree on keys that do — `[0x40]` and
//! `[0x40, 0x00]` are the same fraction but different byte strings. Producing
//! only trimmed keys keeps `memcmp` order (which is what the B+tree uses) and
//! fraction order the same thing.

/// A key strictly between `lo` and `hi`, or `None` when they are not in order.
///
/// `None` for either end means "unbounded": `between(None, None)` is a first
/// key, `between(Some(last), None)` appends, `between(None, Some(first))`
/// prepends.
///
/// The result is always strictly between under `memcmp`, which is the order the
/// B+tree sorts by — so a caller can hand it straight to a `TEXT`/`BLOB`
/// ordering column with no further encoding.
pub fn between(lo: Option<&[u8]>, hi: Option<&[u8]>) -> Option<Vec<u8>> {
    let a = lo.unwrap_or(&[]);
    let (hi_is_one, b): (bool, &[u8]) = match hi {
        Some(h) => (false, h),
        None => (true, &[]),
    };
    // Order check first: a caller with reversed bounds has a bug, and inventing
    // a key for it would bury that bug in data that sorts wrong later.
    if !hi_is_one && a >= b {
        return None;
    }

    let n = a.len().max(b.len()) + 1;
    let mut sum: Vec<u16> = (0..n)
        .map(|i| {
            *a.get(i).unwrap_or(&0) as u16 + *b.get(i).unwrap_or(&0) as u16
        })
        .collect();

    // Carry, right to left. `hi_is_one` means the upper bound is 1.0, which
    // contributes a carry into the integer place rather than a digit.
    let mut int_part = u16::from(hi_is_one);
    for i in (1..n).rev() {
        if sum[i] >= 256 {
            sum[i] -= 256;
            sum[i - 1] += 1;
        }
    }
    if sum[0] >= 256 {
        sum[0] -= 256;
        int_part += 1;
    }

    // Halve. A remainder carries 128 into the next digit, which is why a
    // midpoint may be one byte longer than its neighbours — that growth is the
    // known cost of fractional indexing and is measured, not assumed
    // (`repeated_insertion_growth_is_linear`).
    let mut out = Vec::with_capacity(n + 1);
    let mut rem = int_part;
    for s in sum.into_iter().take(n) {
        let cur = rem * 256 + s;
        out.push((cur / 2) as u8);
        rem = cur % 2;
    }
    if rem > 0 {
        out.push(128);
    }

    // Trailing zeros would be the same fraction but a different byte string,
    // and `memcmp` would then disagree with the model.
    while out.last() == Some(&0) {
        out.pop();
    }

    // An exact midpoint of two distinct fractions cannot equal either, but the
    // trim above is the one step that could in principle collapse it. Checked
    // rather than argued, because a key equal to its neighbour sorts wrong
    // forever and silently.
    if out.as_slice() <= a || (!hi_is_one && out.as_slice() >= b) {
        return None;
    }
    Some(out)
}

/// `n` keys in order, for seeding a fresh document without calling
/// [`between`] in a loop and growing every key.
pub fn initial(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            // Spread across the space so later insertions between any pair have
            // room without immediately lengthening.
            let step = 256usize / (n + 1);
            vec![((i + 1) * step).clamp(1, 255) as u8]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(a: &[u8], b: &[u8]) -> Vec<u8> {
        between(Some(a), Some(b)).expect("well-ordered pair has a midpoint")
    }

    /// The one property everything else rests on, over every adjacent pair the
    /// byte space has room for.
    #[test]
    fn the_result_is_always_strictly_between() {
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                let (lo, hi) = ([a], [b]);
                match between(Some(&lo), Some(&hi)) {
                    Some(c) => {
                        assert!(lo[..] < c[..], "{a} < {c:?} failed");
                        assert!(c[..] < hi[..], "{c:?} < {b} failed");
                    }
                    None => assert!(a >= b, "refused a well-ordered pair {a} < {b}"),
                }
            }
        }
    }

    /// Neighbours with no gap at all: consecutive bytes. The midpoint must
    /// descend a digit rather than give up — this is the case an integer key
    /// cannot serve and the reason for the whole module.
    #[test]
    fn adjacent_bytes_still_have_room() {
        let c = mid(&[0x40], &[0x41]);
        assert!(c.len() > 1, "expected a longer key, got {c:?}");
        assert!([0x40][..] < c[..] && c[..] < [0x41][..]);
    }

    /// Open ends: first key, prepend, append.
    #[test]
    fn unbounded_ends() {
        let first = between(None, None).unwrap();
        assert!(!first.is_empty());

        let before = between(None, Some(&first)).unwrap();
        assert!(before[..] < first[..], "{before:?} !< {first:?}");

        let after = between(Some(&first), None).unwrap();
        assert!(after[..] > first[..], "{after:?} !> {first:?}");

        // And the very smallest key still has room before it.
        let tiny = between(None, Some(&[0x01])).unwrap();
        assert!(tiny[..] < [0x01][..], "{tiny:?} !< [1]");
    }

    /// Reversed or equal bounds are a caller bug, and inventing a key for them
    /// would hide it in data that sorts wrong later.
    #[test]
    fn out_of_order_bounds_are_refused() {
        assert_eq!(between(Some(&[0x40]), Some(&[0x40])), None);
        assert_eq!(between(Some(&[0x41]), Some(&[0x40])), None);
    }

    /// Never a trailing zero: the fraction model and `memcmp` only agree on
    /// trimmed keys, and the B+tree sorts by `memcmp`.
    #[test]
    fn results_never_carry_a_trailing_zero() {
        let mut a = vec![0x01];
        for _ in 0..500 {
            let c = mid(&a, &[0xff]);
            assert_ne!(c.last(), Some(&0), "trailing zero in {c:?}");
            a = c;
        }
    }

    /// **The move case, as an ordering fact.** Take a document's blocks, move
    /// one from the end to the middle by rewriting only its key, and the order
    /// must be exactly what was asked for — with every other key untouched.
    #[test]
    fn a_move_rewrites_exactly_one_key() {
        let keys = initial(5);
        let before: Vec<Vec<u8>> = keys.clone();

        // Move the last block between the first and second.
        let moved = mid(&keys[0], &keys[1]);
        let mut after = keys.clone();
        after[4] = moved;

        // Every other key is byte-identical: one row was written.
        for i in 0..4 {
            assert_eq!(after[i], before[i], "block {i} was rewritten by a move");
        }
        // And the resulting order is the intended one.
        let mut sorted: Vec<(usize, &Vec<u8>)> = after.iter().enumerate().collect();
        sorted.sort_by(|x, y| x.1.cmp(y.1));
        let order: Vec<usize> = sorted.into_iter().map(|(i, _)| i).collect();
        assert_eq!(order, vec![0, 4, 1, 2, 3]);
    }

    /// The known cost, **measured rather than assumed**: repeatedly inserting
    /// into the same gap lengthens the key. This pins the growth so a future
    /// change that makes it worse is visible, and reports the number the
    /// rebalancing decision will be made on.
    #[test]
    fn repeated_insertion_growth_is_linear() {
        let (mut lo, hi) = (vec![0x40u8], vec![0x41u8]);
        let mut lens = Vec::new();
        for _ in 0..10_000 {
            lo = mid(&lo, &hi);
            lens.push(lo.len());
        }
        // Always still in order, ten thousand insertions deep.
        assert!(lo[..] < hi[..]);
        let final_len = *lens.last().unwrap();
        // Worst case is one byte per insertion; the halving reuses digits, so
        // it is far better than that in practice. The bound is what matters:
        // this must not be quadratic.
        assert!(
            final_len <= 10_002,
            "key grew to {final_len} bytes over 10k insertions — worse than linear"
        );
        println!("10k same-gap insertions: final key {final_len} bytes");
    }

    /// Interleaved insertions across a whole document stay ordered — the shape
    /// a real editing session has, rather than one pathological gap.
    #[test]
    fn many_interleaved_insertions_stay_ordered() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut keys = initial(8);
        for _ in 0..3000 {
            let i = (next() % (keys.len() as u64 + 1)) as usize;
            let lo = (i > 0).then(|| keys[i - 1].clone());
            let hi = (i < keys.len()).then(|| keys[i].clone());
            let k = between(lo.as_deref(), hi.as_deref())
                .expect("an in-order gap always has a midpoint");
            keys.insert(i, k);
        }
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "order broke: {:?} !< {:?}", w[0], w[1]);
        }
    }

    /// A fresh document's keys are ordered and short.
    #[test]
    fn initial_keys_are_ordered_and_one_byte() {
        for n in [1usize, 2, 5, 64, 200] {
            let ks = initial(n);
            assert_eq!(ks.len(), n);
            for k in &ks {
                assert_eq!(k.len(), 1, "seed key {k:?} is not one byte");
            }
            for w in ks.windows(2) {
                assert!(w[0] < w[1], "seed keys out of order at n={n}: {ks:?}");
            }
        }
    }
}
