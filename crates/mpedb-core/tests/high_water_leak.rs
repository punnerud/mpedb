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
//! # The growth rate is CONSTANT — so it is not a feedback loop
//!
//! high_water deltas per 1.4 s, 4 writers: 3144, 2982, 2413, 3049, 3559, 3329,
//! 2900, 3233. Flat. The freelist tree grew 10× over that same window. Any
//! story where a bigger/deeper freelist causes more bumps predicts an
//! ACCELERATING rate, and there isn't one.
//!
//! So it is a fixed leak per commit: ~2200 pages/s ÷ ~94k commits/s =
//! **one page per ~43 commits**, and `alloc_hw`/commit measures 0.021 — the
//! same number. Pool size is irrelevant to the bump rate, which is exactly why
//! the pool grows forever: refill hands over ONE entry no matter how many
//! entries exist, so 12,000 free pages help a dry fixpoint not at all.
//!
//! Per-commit page accounting closes (4 writers): in 17.45 (refill) + freed
//! 5.33 = 22.78; out 17.46 (recorded) + used 5.31 = 22.77. **No page goes
//! missing.** At fixpoint entry a txn holds ~12.1 pages and the fixpoint needs
//! ~6. It fits 98.5% of the time. The leak is the *tail* of that distribution,
//! and it is constant because the distribution is stationary.
//!
//! # Six dead hypotheses — do not re-run these
//!
//! Each was killed by measurement, not by argument:
//!
//! 1. **"The oldest-pinned bound stalls under 4+ writers."** The lead this file
//!    used to give. Measured lag is 17-134 txns out of ~300k. Dead.
//! 2. **"Leftover `reusable` is dropped on the floor."** The commit fixpoint
//!    records `freed ∪ reusable`. It is not dropped. Dead.
//! 3. **"The once-per-txn recompute cap (`bound_recomputed`) is the limiter."**
//!    Dead — see 5.
//! 4. **"The fixpoint runs dry, so stock `reusable` before it starts."** Tried
//!    TWICE, because the first attempt had a bug worth knowing about. Both runs
//!    ended identically: high_water 27k→**65k**, throughput halved, DbFull at
//!    10 s. It does cut `alloc_hw_in_fl` — and triples `alloc_hw` overall.
//!
//!    The reason is structural, not a tuning miss: the fixpoint records
//!    `freed ∪ reusable`, so **every page you hand a writer is a page the
//!    fixpoint must write back**, in a value that grows its own work and its
//!    iteration count (`commit_leftover` hit 101 pages/commit;
//!    `refill_net_negative` hit 51% of refills). **You cannot give the fixpoint
//!    more pages, because giving it pages makes it need more pages.**
//!
//!    The bug in attempt one, worth avoiding: the retry loop used
//!    `if reusable.len() == before { break }` as its progress check. Evicting an
//!    entry COSTS pages, so a small entry is net-negative and the pool comes out
//!    SHORTER — which that check reads as progress, looping until the freelist
//!    is drained. `refill_reusable` should report reclamation as a bool; never
//!    infer it from the pool's length.
//! 6. **"Take only what you need: cap the take, leave the rest in the entry
//!    under its own key."** The natural answer to 4 — entry size is
//!    `entry = entry - used + freed`, a fixed point for ANY size (no restoring
//!    force), and it random-walks to 20 pages under 4 writers vs 4.7 under 1.
//!    Capping the take is a restoring force, and it keeps leftover pages from
//!    being re-stamped with a newer txn than they need. Tried with
//!    `REFILL_TAKE_PAGES = 16`: **worse again** (65k, DbFull at 10 s) and the
//!    entry COUNT exploded to 4,827 — each commit still adds its own entry while
//!    refill only shrinks one, so entries pile up faster than they retire.
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
//! Everything above narrows to one sentence: **the commit fixpoint mints a page
//! whenever it runs dry, and nothing can stop it running dry**, because
//! `in_freelist_op` forbids the one move that would help (refill re-entering the
//! tree the fixpoint is mutating — a real hazard, not a missing check).
//!
//! Hypotheses 4 and 6 are the two obvious ways to feed it, and both fail for the
//! same structural reason: the fixpoint's own work is coupled to `|reusable|`
//! through `candidate = freed ∪ reusable`. Pages you give it are pages it must
//! write back, which is more work, which needs more pages. The coupling is the
//! bug; any fix has to cut it rather than tune around it.
//!
//! **The untried idea, and the one the evidence supports:** don't let the
//! fixpoint mint at all — let it UNWIND. When an allocation inside the fixpoint
//! finds `reusable` empty, fail the iteration (`Error`-style, or a `dry` flag),
//! leave `in_freelist_op`, refill (legal now — nothing is mid-traversal), and
//! re-run the iteration from the top. The loop already re-reads
//! `freed ∪ reusable` every pass and upserts under the same `new_txn` key, so a
//! half-applied iteration is re-applied, not corrupted. This needs the
//! convergence and bounded-iteration arguments in DESIGN.md §4.5 re-checked
//! (the 64-iteration cap becomes reachable in a new way), which is exactly the
//! design-review treatment this code wants.
//!
//! Also worth settling, though measured NOT to be the leak: re-recording
//! leftover `reusable` under `freelist_key(new_txn, ..)` is strictly more
//! conservative than needed — those pages already passed the bound gate when
//! refill handed them over.
//!
//! This is reviewed MVCC/freelist protocol code (DESIGN.md §5). Six hypotheses
//! have now died here, two of them by being *implemented and measured doing
//! nothing or doing harm*. That is the cheapest way to kill one: measure the
//! seventh before you believe it.

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
