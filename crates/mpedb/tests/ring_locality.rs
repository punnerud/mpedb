//! Key-locality drain order, arm A/B — this binary is the DEFAULT arm
//! (sorted drain). Its twin `ring_locality_nosort.rs` runs the identical
//! workload with `MPEDB_NO_BATCH_ROUTING=1` (slot-order drain); both assert
//! the same canonical committed state, so the two orderings are proven
//! state-equivalent. One binary per arm because the switch is read once per
//! process (LazyLock).

#[path = "ring_locality_common/mod.rs"]
mod common;

#[test]
fn contended_batches_commit_canonical_state_sorted_arm() {
    assert!(
        std::env::var("MPEDB_NO_BATCH_ROUTING").is_err()
            && std::env::var("MPEDB_RING_NO_SORT").is_err(),
        "sorted arm must run without the kill-switch set"
    );
    common::run_contended_workload_and_assert_canonical_state("sorted");
}
