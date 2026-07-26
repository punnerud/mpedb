//! **Review finding R2: an unknown ring kind must fail conservatively.**
//!
//! The committed-footprint ring tags every entry with a `kind`, and the
//! readers switch on it. `POINT` is the only kind that can answer *no
//! conflict* — it compares an exact key hash. So a reader whose catch-all arm
//! falls through to POINT will read any kind it does not recognise as a point
//! write, compare an unrelated 64-bit value for equality, almost never match,
//! and report no conflict.
//!
//! That is the wrong-answer direction, reachable by nothing worse than adding
//! a variant. #143 and #144 each added one and each had to remember an arm
//! here; this test is so the next one does not have to remember.

use mpedb_core::shm::OFP_KIND_POINT;

/// Every kind value, including ones no writer produces today, must be handled
/// in the safe direction by the single-key validator.
#[test]
fn an_unrecognised_kind_is_treated_as_a_conflict() {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-ringkind-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    let cfg = mpedb_types::Config::from_toml_str(&toml).unwrap();
    let eng = mpedb_core::engine::Engine::open(&cfg, vec![vec![]]).unwrap();
    let shm = eng.shm_for_test();

    // Table 0, some key. A committed txn 1 recorded against our snapshot 0.
    let my_key = 0xABCD_EF01_2345_6789u64;
    let tbits = 1u64;

    // The one kind that may legitimately answer "no conflict": a point write
    // to a DIFFERENT key. This is the behaviour the catch-all used to give
    // every unknown kind.
    shm.opt_record(1, OFP_KIND_POINT, tbits, my_key ^ 1, u64::MAX, None);
    assert!(
        !shm.opt_conflict(0, 1, 0, my_key),
        "a point write to another key should not conflict — if this fails the \
         premise of the test is wrong, not the code"
    );

    // Now the same entry with kinds this reader does not know. Every one must
    // conflict: an unrecognised kind means `khash` cannot be interpreted, and
    // a reader that cannot interpret it cannot prove disjointness.
    // 0..=4 are the kinds defined today (EMPTY, POINT, TABLE, REGION, SHARD);
    // everything above is a value no writer emits, which is exactly the case
    // the catch-all decides.
    for kind in [5u64, 7, 42, 255, u64::MAX] {
        shm.opt_record(1, kind, tbits, my_key ^ 1, u64::MAX, None);
        assert!(
            shm.opt_conflict(0, 1, 0, my_key),
            "ring kind {kind} was read as a point write and reported NO conflict — \
             adding a kind must never be able to lose a conflict"
        );
    }

    drop(eng);
    let _ = std::fs::remove_file(&path);
}
