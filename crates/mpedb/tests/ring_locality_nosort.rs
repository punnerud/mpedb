//! Key-locality drain order, arm A/B — this binary is the KILL-SWITCH arm
//! (historical slot-order drain via `MPEDB_NO_BATCH_ROUTING=1`). See
//! `ring_locality.rs` for the sorted arm; both assert the same canonical
//! committed state.

#[path = "ring_locality_common/mod.rs"]
mod common;

#[test]
fn contended_batches_commit_canonical_state_slot_order_arm() {
    // Must be set before the first ring drain in this process: the switch is
    // read once (LazyLock). This is the only test in this binary, so the set
    // is race-free and observed.
    std::env::set_var("MPEDB_NO_BATCH_ROUTING", "1");
    common::run_contended_workload_and_assert_canonical_state("slot-order");
}
