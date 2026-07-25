//! **Sub-edits: rebasing a splice through the edits that beat it (#145).**
//!
//! The guard (#142–#144) refuses when two actions touch the same value. Inside
//! one shard that is still all-or-nothing on a field: two workers editing
//! *different parts* of a hundred-kilobyte document conflict even though they
//! never touch the same bytes. That is not a real conflict, it is a resolution
//! too coarse — the last one in the day's sequence table → key → shard →
//! **value → sub-value**.
//!
//! ## Why this is rebase and not a CRDT
//!
//! A CRDT merges without coordination, and pays for it: per-position identity,
//! version maps on every operation, metadata that grows with edit history, and
//! compaction that needs to know what every replica has seen. cola earns that
//! price honestly — it is built for peers with no common clock.
//!
//! **mpedb has a total order at commit.** One writer lock, so every commit
//! sees every earlier commit. The problem is not merging arbitrary concurrency;
//! it is that a worker thinks between reading and writing, and the world moves
//! underneath. Rebasing a splice through a known, ordered history is a strictly
//! smaller problem, and it needs no per-character metadata at all.
//!
//! What is worth taking from cola is the *anchor* idea — a position that
//! survives later edits — which is what [`Splice::rebase`] computes, as
//! arithmetic rather than as a data structure.
//!
//! ## What it will not do
//!
//! Two edits to the **same span** are a real conflict, and this refuses them
//! rather than inventing a merge. A CRDT would produce *some* answer there;
//! we produce an error and the caller re-reads. That is weaker, deliberately,
//! and it is the line where "resolve the conflict" turns into "guess at intent".

/// One edit against a byte string: remove `remove` bytes at `at`, put `insert`
/// there instead. A pure insertion has `remove == 0`; a pure deletion has an
/// empty `insert`.
///
/// Offsets are **byte** offsets, not characters. The engine stores bytes and
/// `keycode` orders bytes; introducing a second unit here would mean two
/// notions of "position" that must agree forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub at: u64,
    pub remove: u64,
    pub insert: Vec<u8>,
}

/// Why a splice could not be rebased — always a genuine overlap, never a
/// limitation of the arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clash {
    /// Both edits remove bytes from an overlapping range.
    RemovedSpansOverlap,
    /// The earlier edit inserted inside the range this one removes, so applying
    /// this one would silently delete text it never saw.
    InsertedInsideMyRemoval,
    /// The earlier edit removed the position this one inserts at, so there is
    /// no longer a place to put the text.
    MyPositionWasRemoved,
}

impl Splice {
    /// The byte length this splice produces where it consumed `remove`.
    fn inserted(&self) -> u64 {
        self.insert.len() as u64
    }

    /// End of the range this splice removes (== `at` for a pure insertion).
    fn end(&self) -> u64 {
        self.at + self.remove
    }

    /// Rebase `self` through `earlier`, which committed first.
    ///
    /// Returns the splice to apply against the *post-`earlier`* value, or the
    /// reason the two genuinely collide.
    ///
    /// The three outcomes are the whole feature:
    ///
    /// | `earlier` relative to me | result |
    /// |---|---|
    /// | entirely at or before my start | my offset shifts by its net length change |
    /// | entirely at or after my end | unchanged |
    /// | overlapping my span | [`Clash`] |
    ///
    /// **Two pure insertions never clash**, whatever their offsets — including
    /// the same offset. There is nothing to disagree about: the total commit
    /// order says whose text goes first, and mine shifts past theirs. That case
    /// is checked before overlap precisely so a shared insertion point does not
    /// read as a collision.
    pub fn rebase(&self, earlier: &Splice) -> Result<Splice, Clash> {
        let (a, r) = (self.at, self.remove);
        let (b, s) = (earlier.at, earlier.remove);
        let j = earlier.inserted();

        // Two insertions: order, not conflict.
        if r != 0 || s != 0 {
            let clash = if r > 0 && s > 0 {
                // Removed ranges intersect?
                (a < earlier.end() && b < self.end()).then_some(Clash::RemovedSpansOverlap)
            } else if r > 0 {
                // They inserted; only strictly inside my removal is a problem.
                // Landing exactly on either boundary is not: their text ends up
                // beside what I remove, not within it.
                (a < b && b < self.end()).then_some(Clash::InsertedInsideMyRemoval)
            } else {
                // I insert; only strictly inside their removal is a problem.
                (b < a && a < earlier.end()).then_some(Clash::MyPositionWasRemoved)
            };
            if let Some(c) = clash {
                return Err(c);
            }
        }

        // Checked BEFORE "entirely after": at equal offsets both tests pass,
        // and the total order says the earlier commit's bytes come first, so
        // mine moves past them.
        let at = if earlier.end() <= a {
            // Net length change of an edit that finished at or before my start.
            // Signed, because a deletion shrinks what precedes me.
            (a + j).checked_sub(s).expect("earlier.end() <= a bounds the subtraction")
        } else {
            // Entirely at or after my end — nothing before me moved.
            debug_assert!(b >= self.end());
            a
        };

        Ok(Splice { at, remove: r, insert: self.insert.clone() })
    }

    /// Rebase through a whole history, oldest first. Each step rebases through
    /// the *already-rebased* frame, which is what makes a chain of edits
    /// compose.
    pub fn rebase_through<'a>(
        &self,
        history: impl IntoIterator<Item = &'a Splice>,
    ) -> Result<Splice, Clash> {
        let mut cur = self.clone();
        for e in history {
            cur = cur.rebase(e)?;
        }
        Ok(cur)
    }

    /// Apply to a byte string. Out-of-range offsets clamp rather than panic:
    /// a rebased splice is always in range by construction, and a hand-built
    /// one must not be able to crash the engine.
    pub fn apply(&self, buf: &mut Vec<u8>) {
        let at = (self.at as usize).min(buf.len());
        let end = (at + self.remove as usize).min(buf.len());
        buf.splice(at..end, self.insert.iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(at: u64, t: &str) -> Splice {
        Splice { at, remove: 0, insert: t.as_bytes().to_vec() }
    }
    fn del(at: u64, n: u64) -> Splice {
        Splice { at, remove: n, insert: Vec::new() }
    }
    fn rep(at: u64, n: u64, t: &str) -> Splice {
        Splice { at, remove: n, insert: t.as_bytes().to_vec() }
    }

    /// The load-bearing property: two edits in different parts of one value
    /// both apply, and the result contains both. A test that only showed
    /// overlap being refused would pass against the pre-#145 engine.
    #[test]
    fn two_edits_in_different_spans_both_apply() {
        let base = b"the quick brown fox".to_vec();
        let a = rep(4, 5, "slow"); // "quick" -> "slow"
        let b = rep(16, 3, "cat"); // "fox"   -> "cat"

        // Commit order a, b: b rebases through a.
        let mut left = base.clone();
        a.apply(&mut left);
        b.rebase(&a).unwrap().apply(&mut left);

        // Commit order b, a: a rebases through b.
        let mut right = base.clone();
        b.apply(&mut right);
        a.rebase(&b).unwrap().apply(&mut right);

        assert_eq!(String::from_utf8(left.clone()).unwrap(), "the slow brown cat");
        assert_eq!(left, right, "the two commit orders produced different values");
    }

    /// Every relative position of a non-overlapping pair, as a table rather
    /// than as spot checks — this arithmetic is the part that is easy to get
    /// subtly wrong.
    #[test]
    fn the_shift_table() {
        // (mine, earlier, expected rebased offset)
        let cases: &[(Splice, Splice, u64)] = &[
            // insertion before me: I move right by its length
            (ins(10, "x"), ins(3, "abc"), 13),
            // insertion after me: unchanged
            (ins(10, "x"), ins(20, "abc"), 10),
            // insertion exactly at my offset: total order puts theirs first
            (ins(10, "x"), ins(10, "abc"), 13),
            // deletion before me: I move left
            (ins(10, "x"), del(2, 4), 6),
            // deletion ending exactly at my offset
            (ins(10, "x"), del(6, 4), 6),
            // deletion starting exactly at my offset: mine goes first
            (ins(10, "x"), del(10, 4), 10),
            // deletion after me
            (ins(10, "x"), del(20, 4), 10),
            // replacement before me, net longer
            (rep(10, 2, "z"), rep(0, 2, "abcd"), 12),
            // replacement before me, net shorter
            (rep(10, 2, "z"), rep(0, 4, "a"), 7),
            // earlier ends exactly where my removal starts
            (rep(10, 2, "z"), del(5, 5), 5),
            // earlier starts exactly where my removal ends
            (rep(10, 2, "z"), ins(12, "q"), 10),
        ];
        for (mine, earlier, want) in cases {
            let got = mine.rebase(earlier).unwrap_or_else(|c| {
                panic!("{mine:?} through {earlier:?} clashed ({c:?}) but should shift")
            });
            assert_eq!(got.at, *want, "{mine:?} rebased through {earlier:?}");
            assert_eq!(got.remove, mine.remove, "rebase changed what I remove");
            assert_eq!(got.insert, mine.insert, "rebase changed what I insert");
        }
    }

    /// The three genuine collisions, each identified rather than lumped
    /// together — the reason matters to a caller deciding what to do next.
    #[test]
    fn the_clash_table() {
        let cases: &[(Splice, Splice, Clash)] = &[
            (del(10, 5), del(12, 5), Clash::RemovedSpansOverlap),
            (del(10, 5), del(8, 5), Clash::RemovedSpansOverlap),
            (rep(10, 5, "x"), rep(10, 5, "y"), Clash::RemovedSpansOverlap),
            (del(10, 5), ins(12, "x"), Clash::InsertedInsideMyRemoval),
            (ins(12, "x"), del(10, 5), Clash::MyPositionWasRemoved),
        ];
        for (mine, earlier, want) in cases {
            match mine.rebase(earlier) {
                Err(got) => assert_eq!(got, *want, "{mine:?} through {earlier:?}"),
                Ok(s) => panic!("{mine:?} through {earlier:?} rebased to {s:?}, expected {want:?}"),
            }
        }
    }

    /// Boundary-touching edits are NOT collisions. This is the pair the naive
    /// `<=` writes wrong, and getting it wrong turns every adjacent edit into a
    /// spurious retry.
    #[test]
    fn touching_boundaries_do_not_clash() {
        // Their insertion sits exactly at the start of my removal.
        assert!(del(10, 5).rebase(&ins(10, "x")).is_ok());
        // ...and exactly at its end.
        assert!(del(10, 5).rebase(&ins(15, "x")).is_ok());
        // Their deletion ends exactly where mine begins.
        assert!(del(10, 5).rebase(&del(5, 5)).is_ok());
        // ...and begins exactly where mine ends.
        assert!(del(10, 5).rebase(&del(15, 5)).is_ok());
    }

    /// Two insertions never clash, at any offsets including the same one.
    #[test]
    fn insertions_never_clash() {
        for a in 0..8u64 {
            for b in 0..8u64 {
                assert!(
                    ins(a, "x").rebase(&ins(b, "yy")).is_ok(),
                    "insertion at {a} clashed with insertion at {b}"
                );
            }
        }
    }

    /// A chain composes. Each logged splice is expressed against the value as
    /// it stood just before it — which is what a log of *applied* edits
    /// contains — so rebasing steps forward one frame at a time.
    ///
    /// The assertion is the INTENT, not a hand-computed string: my edit named
    /// two specific bytes of the original, and after three unrelated edits
    /// moved them around it must still land on those two bytes and no others.
    /// Writing the expected value out by hand is how this test lied the first
    /// time it was written.
    #[test]
    fn a_chain_of_earlier_edits_composes() {
        let base = b"0123456789".to_vec();
        let hist = [ins(0, "AA"), ins(4, "B"), del(9, 1)];
        let mine = rep(8, 2, "XY"); // the bytes "89", in ORIGINAL coordinates

        let mut buf = base.clone();
        for e in &hist {
            e.apply(&mut buf);
        }
        let before = String::from_utf8(buf.clone()).unwrap();
        let r = mine.rebase_through(&hist).unwrap();
        r.apply(&mut buf);
        let after = String::from_utf8(buf).unwrap();

        // The intent: exactly the substring "89" became "XY", and nothing else
        // moved. Derived from `before` rather than asserted as a literal.
        let want = before.replacen("89", "XY", 1);
        assert_eq!(after, want, "the rebased edit did not land on the bytes it named");
        assert!(!after.contains("89"), "the original bytes survived: {after}");
    }

    /// Determinism is a project-wide requirement (#57/#92): the same edits in
    /// the same commit order must produce byte-identical output, every time.
    #[test]
    fn the_same_order_is_byte_identical() {
        let base = b"alpha beta gamma delta".to_vec();
        let edits = [rep(0, 5, "ALPHA"), rep(11, 5, "GAMMA"), ins(22, "!")];
        let run = || {
            let mut buf = base.clone();
            let mut applied: Vec<Splice> = Vec::new();
            for e in &edits {
                let r = e.rebase_through(applied.iter()).unwrap();
                r.apply(&mut buf);
                applied.push(r);
            }
            buf
        };
        assert_eq!(run(), run());
        assert_eq!(String::from_utf8(run()).unwrap(), "ALPHA beta GAMMA delta!");
    }

    /// **The invariant, fuzzed.** Rebasing must preserve *which original bytes*
    /// an edit targets. So: label every byte of the base uniquely, apply a
    /// random history, rebase a random edit through it, and check the rebased
    /// edit removes exactly the labels the original one named.
    ///
    /// That is stronger than any table of offsets, because it does not depend
    /// on me having worked out the right answer by hand — which is exactly how
    /// the chain test above was wrong the first time.
    ///
    /// Deterministic xorshift, per the project's no-rand convention: a failure
    /// is reproducible from the seed printed in the panic.
    #[test]
    fn rebase_preserves_which_original_bytes_are_targeted() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for seed in 0..3000u64 {
            // Labels 0..24 as single bytes, so a byte IS its identity.
            let base: Vec<u8> = (0..24u8).collect();
            let n = base.len() as u64;

            // My edit names a span of the ORIGINAL; record the labels in it.
            let a = next() % n;
            let r = next() % (n - a + 1);
            let mine = Splice { at: a, remove: r, insert: b"##".to_vec() };
            let targeted: Vec<u8> = base[a as usize..(a + r) as usize].to_vec();

            // A short history, each entry against the value as it then stood.
            let mut buf = base.clone();
            let mut hist = Vec::new();
            for _ in 0..(next() % 4) {
                let len = buf.len() as u64;
                let at = next() % (len + 1);
                let rem = next() % (len - at + 1).min(4);
                // Inserted bytes are high-valued so they can never be mistaken
                // for an original label.
                let e = Splice { at, remove: rem, insert: vec![0xF0, 0xF1] };
                e.apply(&mut buf);
                hist.push(e);
            }

            let Ok(reb) = mine.rebase_through(&hist) else {
                continue; // a genuine clash; the clash table covers those
            };
            let lo = (reb.at as usize).min(buf.len());
            let hi = (lo + reb.remove as usize).min(buf.len());
            let removed: Vec<u8> = buf[lo..hi].to_vec();

            // Every label still present must be exactly the ones I named. A
            // history deletion may have already removed some of them, so the
            // check is subset-plus-no-strangers rather than equality.
            for b in &removed {
                assert!(
                    targeted.contains(b),
                    "seed {seed}: rebased edit would remove byte {b}, which it \
                     never named (named {targeted:?}, history {hist:?}, \
                     rebased {reb:?})"
                );
            }
            for b in &targeted {
                if buf.contains(b) {
                    assert!(
                        removed.contains(b),
                        "seed {seed}: byte {b} was named and still exists, but \
                         the rebased edit misses it (history {hist:?}, rebased {reb:?})"
                    );
                }
            }
        }
    }

    /// A hand-built splice past the end of the value must clamp, not panic:
    /// the engine may not be crashable by a bad offset from a caller.
    #[test]
    fn out_of_range_clamps_instead_of_panicking() {
        let mut buf = b"abc".to_vec();
        rep(100, 50, "z").apply(&mut buf);
        assert_eq!(buf, b"abcz");
        let mut buf2 = b"abc".to_vec();
        del(1, 999).apply(&mut buf2);
        assert_eq!(buf2, b"a");
    }
}
