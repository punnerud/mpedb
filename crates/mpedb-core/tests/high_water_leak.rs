//! **Fixed 2026-07-15.** Sustained concurrent churn used to grow the high-water
//! mark without bound, on a working set that did not grow — the top genuine bug
//! in the engine. This file is the record of what it was, how it was found, and
//! the six wrong answers, because the wrong answers are the expensive part.
//!
//! # The bug
//!
//! A 1000-key table — ~30 KB of live rows — churned with insert/update/delete
//! from N processes. The live set never grew. The FILE filled anyway:
//!
//! ```text
//!   64 MB:  8 writers survive 5 s, fail at 10 s
//!  128 MB:  8 writers survive 10 s, fail at 20 s
//!  256 MB:  8 writers survive 20 s, fail at 40 s
//! ```
//!
//! Doubling the file exactly doubled the survivable time — linear, unbounded. It
//! was gated on CONCURRENCY, not duration (1 and 2 writers never leaked; 4+ did),
//! and only in `mixed`, the only mode with DELETE.
//!
//! After the fix: **8 writers, 64 MB, 60 s, 4.9M ops, `verify: ok`** — where the
//! scaling law above would have demanded ~384 MB. Throughput is flat across
//! 10/30/60 s (84.6k → 82.4k → 81.8k ops/s), i.e. no slow degradation either.
//!
//! # The cause
//!
//! `refill_reusable` used to `btree::delete` the entry it drew pages from. That
//! made every page drawn a page the commit fixpoint had to write back — it
//! records what is free, and a drawn page was then listed nowhere else. So the
//! fixpoint's own work was coupled to the pool the writer held: `candidate =
//! freed ∪ reusable`.
//!
//! The fixpoint cannot refill (`in_freelist_op` — it is mutating the tree refill
//! reads), so the instant its pool ran dry it took a page off `high_water`.
//! design/DESIGN.md §4.5 calls that "a few pages of slack". It measured as ~1 page per
//! 43 commits, forever, because **the pool's size is irrelevant to the bump
//! rate**: refill handed over ONE entry no matter how many entries existed, so
//! 12,000 free pages did a dry fixpoint no good at all.
//!
//! The fix is to make refill **read-only**: draw the pages, leave the entry, and
//! let the fixpoint strike out only what got consumed. An entry nobody allocates
//! out of is never rewritten — so a writer can hold a deep pool for free, and the
//! fixpoint stops falling through to `high_water`. design/DESIGN.md §4.5 has the
//! protocol; the cost is a measured **-7.05% [-8.71, -5.40], n=20 pairs** on the
//! write path.
//!
//! # Six dead hypotheses — the expensive part
//!
//! Three died by being IMPLEMENTED and measured doing nothing, or harm. That is
//! the cheapest way to kill one; reasoning about this code was reliably wrong.
//!
//! 1. **"The oldest-pinned bound stalls under 4+ writers."** The lead this file
//!    itself used to give. Measured lag: 17-134 txns out of ~300k. Dead.
//! 2. **"Leftover `reusable` is dropped on the floor."** The fixpoint recorded
//!    `freed ∪ reusable`. It was not dropped — that was the *problem*, not the
//!    bug. Dead.
//! 3. **"The once-per-txn recompute cap is the limiter."** Dead — see 5.
//! 4. **"The fixpoint runs dry, so stock `reusable` before it starts."** Tried
//!    twice; both runs: high_water 27k→**65k**, throughput halved, DbFull at
//!    10 s. Feeding the fixpoint made it hungrier — the coupling above. (The
//!    first attempt also had a bug worth avoiding: its retry loop read
//!    `reusable.len()` as progress, but evicting a small entry cost more pages
//!    than it yielded, so the pool came out SHORTER and the loop drained the
//!    freelist. Never infer reclamation from the pool's length.)
//! 5. **"The bound goes stale, locking the freelist out."** A REAL defect —
//!    refill only recomputed when `freed_txn > bound`, but it always drew the
//!    OLDEST entry, which is almost always reclaimable, so the bound refreshed
//!    on 2% of commits under 4 writers vs 53% under 1, and lag grew to 933.
//!    Recomputing unconditionally **fixed the lag (933 → 0-1) and did not touch
//!    the leak.** Reverted: no measured win, no change to reviewed code.
//! 6. **"Cap what refill takes; leave the rest in the entry."** Entry size is
//!    `entry = entry - used + freed`, a fixed point for ANY size — no restoring
//!    force, so it random-walked to 20 pages under 4 writers vs 4.7 under 1.
//!    Capping is the missing force. Also 65k, and the entry COUNT exploded to
//!    4,827: each commit still added an entry while refill only shrank one.
//!    (Right diagnosis, wrong half — the leftover has to stop needing a
//!    write-back at all, not merely get smaller.)
//! 7. **"Let the fixpoint unwind and refill instead of minting."** Killed by
//!    READING design/DESIGN.md §4.5 rather than measuring: the `high_water` fallback IS
//!    the termination argument. Allocation falling back to something that frees
//!    nothing is what bounds the loop. A refill inside the fixpoint frees pages
//!    (its own COW), which grows the set, which needs another pass. Unbounded.
//!
//! # What the instrument said, and why it was needed
//!
//! `cargo build --release -p mpedb --features leakstat --example leak_probe`
//! then `./target/release/examples/leak_probe /tmp/lp 4 14`. It killed 1, 2 and
//! 5 outright and produced the two facts that finally constrained the answer:
//!
//! - **Every page ever allocated was in the freelist** (16,146 of high_water
//!   16,294), every entry fresh, reclamation never gated (`refill_not_yet` fired
//!   2 times in 199k commits). Nothing was lost. Nothing was stuck.
//! - **The growth rate was CONSTANT** (3144, 2982, 2413, 3049, 3559, 3329, 2900,
//!   3233 per 1.4 s) while the freelist tree grew 10×. So: no feedback loop, and
//!   tree depth was not the driver. A fixed leak per commit.
//!
//! Per-commit page accounting closed exactly (in 17.45 + freed 5.33 = 22.78 vs
//! out 17.46 + used 5.31 = 22.77), which is what proved the leak was the *tail*
//! of a distribution — the fixpoint held ~12 pages and needed ~6, and fit 98.5%
//! of the time.

/// The regression. Not automated in-process on purpose: the bug needs N real
/// processes and tens of seconds, and the CLI's stress harness already does
/// exactly that — a second copy here would be a worse version of a tool that
/// exists. Run this after touching `refill_reusable`, `freelist_plan`, or the
/// commit fixpoint:
///
/// ```sh
/// cargo build --release -p mpedb-cli
/// # used to die at 10 s with "8 child(ren) failed" / "database is out of space"
/// ./target/release/mpedb stress --dir /tmp/x --workers 8 --secs 60 --mode mixed
/// # stress mixed: workers=8 secs=60 ops=4907573 ... throughput=81775 ops/s
/// # verify: ok
/// ```
///
/// `verify: ok` is doing real work here: the first cut of the fix passed the
/// leak test and failed page accounting with `page 84 leaked: neither reachable
/// nor freelisted`. A leak fix that corrupts is worth nothing — always run the
/// verifier, not just the survival check.
#[test]
#[ignore = "needs 8 processes and 60 s: run `mpedb stress --workers 8 --secs 60 \
            --mode mixed` and check both throughput and `verify: ok`"]
fn sustained_concurrent_churn_no_longer_grows_the_high_water() {
    panic!(
        "not automated — see this file's module docs. Reproduce with:\n  \
         mpedb stress --dir /tmp/x --workers 8 --secs 60 --mode mixed"
    );
}
