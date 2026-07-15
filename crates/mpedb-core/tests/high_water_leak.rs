//! **Known bug**: sustained concurrent churn grows the high-water mark without
//! bound, on a working set that does not grow.
//!
//! Found 2026-07-15 by `mpedb stress --workers 8 --secs 10 --mode mixed`, which
//! reports it as "8 child(ren) failed" — indistinguishable from a correctness
//! failure until you read the child's line ("database is out of space").
//!
//! # What was measured
//!
//! A 1000-key table — ~30 KB of live rows — churned with insert/update/delete
//! from N processes. The live set never grows. The FILE fills anyway:
//!
//! ```text
//!   64 MB:  8 writers survive 5 s, fail at 10 s
//!  128 MB:  8 writers survive 10 s, fail at 20 s
//!  256 MB:  8 writers survive 20 s, fail at 40 s
//! ```
//!
//! **Doubling the file exactly doubles the survivable time** — linear,
//! unbounded growth. No file size fixes it; each one buys proportional delay.
//!
//! And it is gated on CONCURRENCY, not duration:
//!
//! ```text
//!  64 MB, 10 s:  1 writer ok · 2 writers ok · 4 FULL · 6 FULL · 8 FULL
//! ```
//!
//! Only `mixed` triggers it: the only mode with DELETE, so the only one that
//! churns the freelist hard. `bank`, `unique` and `incr` all survive 8 writers.
//! Pre-existing — reproduced on the commit before the writer-lock spin.
//!
//! # What the instrument says (2026-07-15)
//!
//! Run `mpedb/examples/leak_probe` (see below). Under 4 writers:
//!
//! ```text
//! final: txn=795032 high_water=16294 bound=794746
//! freelist: 633 entries holding 16146 pages
//! age(txns): <10:10 <100:90 <1k:533 <10k:0 <100k:0 older:0
//! ```
//!
//! **Every page ever allocated is sitting in the freelist** (16146 of 16294),
//! every entry is fresh, and reclamation is never gated (`refill_not_yet` fires
//! 2 times in 199k commits). Nothing is lost, nothing is stuck. The pool simply
//! GROWS, and high-water grows with it.
//!
//! The one asymmetry between the 1-writer case (no leak at all) and 4 writers:
//!
//! | per writer, 6 s      | 1 writer | 4 writers |
//! |----------------------|---------:|----------:|
//! | `alloc_hw` (the leak)|       22 |     2 910 |
//! | ...inside the fixpoint|       4 |     2 215 |
//! | pages per refill     |      5.6 |      21.3 |
//! | leftover pages/commit|      1.2 |      13.8 |
//!
//! 2910 × 4 writers ≈ the whole measured high-water. **75% of the bumps happen
//! inside the commit fixpoint**, where `in_freelist_op` disables refill by
//! design — the "few pages of slack" its comment promises is the entire leak.
//!
//! # Five dead hypotheses — do not re-run these
//!
//! Each was killed by measurement, not by argument:
//!
//! 1. **"The oldest-pinned bound stalls under 4+ writers."** The lead this file
//!    used to give. Measured lag is 17-134 txns out of ~300k. Dead.
//! 2. **"Leftover `reusable` is dropped on the floor."** The commit fixpoint
//!    records `freed ∪ reusable`. It is not dropped. Dead.
//! 3. **"The once-per-txn recompute cap (`bound_recomputed`) is the limiter."**
//!    Dead — see 5.
//! 4. **"The fixpoint runs dry, so stock `reusable` before it starts."** Tried:
//!    `reserve_reusable(32)` ahead of `in_freelist_op = true`. It did cut
//!    `alloc_hw_in_fl` (2899→1743) — and made the leak **2.4× WORSE**
//!    (high_water 27k→65k, throughput halved, DbFull at 10 s). Each writer
//!    hoards pages in a private `reusable` that no other writer can see, so the
//!    shared freelist starves: `refill_no_tree` went 15→622, leftover went to 95
//!    pages/commit. **Reserving feeds the thing it was meant to starve.**
//! 5. **"The bound goes stale, locking the freelist out."** This one is a real
//!    defect: refill only recomputes when `freed_txn > bound`, but refill always
//!    takes the OLDEST entry, which is almost always reclaimable — so the bound
//!    is refreshed on 2% of commits under 4 writers vs 53% under 1, and `lag`
//!    grows to 933. Recomputing once per txn unconditionally **fixes the lag
//!    (933 → 0-1) and does not touch the leak** (0.0244 vs 0.0217 pages/txn —
//!    if anything worse). Reverted: a change to reviewed protocol code with no
//!    measured win does not go in. The staleness is safe (a low bound is the
//!    conservative direction), and it is now a *known* non-cause.
//!
//! # Where the evidence actually points
//!
//! At the commit fixpoint. It is a convergence loop: it records
//! `freed ∪ reusable`, but its own upserts ALLOCATE from `reusable`, which
//! changes the set it just recorded, so it iterates (~2.0 iterations/commit
//! measured). **The work it does grows with `|reusable|`** — which is why
//! hypothesis 4 backfired. Any fix has to shrink that coupling, not feed it.
//!
//! Open questions, in the order worth asking:
//!
//! - Why does the pool need to be *bigger over time* when the bound never gates
//!   it and the live set is constant? Something adds a page per ~50 commits and
//!   never removes one. Find that, and the leak is found.
//! - Can the fixpoint's own COW draw from a source refill does not have to
//!   re-enter — a small stash reserved at txn START, not at commit? (Note this
//!   is *not* hypothesis 4: the failure there was hoarding across the commit,
//!   which starves peers.)
//! - Is re-recording leftover `reusable` under `freelist_key(new_txn, ..)` right
//!   at all? Those pages already passed the bound gate when refill handed them
//!   over; re-stamping them with a newer txn is strictly more conservative than
//!   needed, and it churns the tree.
//!
//! This is reviewed MVCC/freelist protocol code (DESIGN.md §5). It wants the
//! design-review treatment, not a quick edit. Five hypotheses have now died
//! here; measure the sixth before you believe it.

/// Reproduce:
///
/// ```sh
/// cargo build --release -p mpedb-cli
/// ./target/release/mpedb stress --dir /tmp/x --workers 8 --secs 10 --mode mixed
/// # mpedb: mixed child: unexpected error: database is out of space
/// # mpedb: 8 child(ren) failed
/// ```
///
/// Instrument (counters + freelist shape, off unless the feature is on):
///
/// ```sh
/// cargo build --release -p mpedb --features leakstat --example leak_probe
/// ./target/release/examples/leak_probe /tmp/lp 4 14
/// ```
///
/// Deliberately not automated in-process: the bug needs N real processes and
/// ~10 seconds, and the CLI's stress harness already does exactly that. A
/// second copy of it here would be a worse version of a tool that exists — and
/// this test's job is to make sure the finding is not lost, not to re-run it on
/// every `cargo test`.
#[test]
#[ignore = "known bug, not automated: see the module docs and reproduce with \
            `mpedb stress --workers 8 --secs 10 --mode mixed`"]
fn sustained_concurrent_churn_grows_the_high_water_without_bound() {
    panic!(
        "not automated — see this file's module docs. Reproduce with:\n  \
         mpedb stress --dir /tmp/x --workers 8 --secs 10 --mode mixed"
    );
}
