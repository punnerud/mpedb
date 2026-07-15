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
//! One writer survives 20 s where four die in 10. The threshold sits at that
//! box's core count (4), which points at the oldest-pinned bound lagging once
//! every core can hold a live pin at once — but that is a HYPOTHESIS. Four
//! hypotheses about the bulk-write gap died on measurement the same day, so it
//! is written here as a question, not a finding.
//!
//! Only `mixed` triggers it: the only mode with DELETE, so the only one that
//! churns the freelist hard. `bank`, `unique` and `incr` all survive 8 writers.
//! Pre-existing — reproduced on the commit before the writer-lock spin.
//!
//! # Where to start looking
//!
//! `engine::WriteTxn::refill_reusable` reclaims iff `freed_txn <= bound`, where
//! `bound` is `shm::compute_oldest_pinned`, cached in a monotone `fetch_max`
//! word and recomputed at most **once per write txn**. CLAUDE.md's invariant
//! list says `<=` is correct and `<` is the off-by-one that "causes an unbounded
//! high-water leak" — the code has `<=`, so this is a *different* leak.
//!
//! Questions worth answering before touching anything:
//!
//! - Does the bound actually advance under 4+ writers, or does it stall?
//! - `refill_reusable` reclaims ONE freelist entry per call, and is called only
//!   when `reusable` is empty. Is one chunk per refill enough at this churn
//!   rate, or does the freelist grow faster than it drains?
//! - Is the once-per-txn recompute cap (`bound_recomputed`) the limiter?
//!
//! This is reviewed MVCC/freelist protocol code (DESIGN.md §5). It wants the
//! design-review treatment, not a quick edit.

/// Reproduce:
///
/// ```sh
/// cargo build --release -p mpedb-cli
/// ./target/release/mpedb stress --dir /tmp/x --workers 8 --secs 10 --mode mixed
/// # mpedb: mixed child: unexpected error: database is out of space
/// # mpedb: 8 child(ren) failed
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
