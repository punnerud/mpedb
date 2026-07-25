//! **Key-level notification filtering (#139 S2).**
//!
//! Table granularity already answers "why should a write to A wake a listener
//! on B". This is the next question: why should a write to `id = 7` wake a
//! listener watching `id = 42`?
//!
//! The answer comes free from the footprint. `KeyAccess::Point` names its key
//! symbolically — `KeyPart::Param(0)` — and resolving it against the bound
//! params yields the exact key bytes without executing anything, which is the
//! same trick the intent ring uses to sort a batch by locality.
//!
//! What matters most here is the FAILURE direction. A region that is wrong in
//! the "different key" direction makes a listener sleep through its own
//! change, which is the one thing this feature may never do. So a region may
//! always be WIDER than the truth and never narrower, and the unresolvable
//! cases publish 0 — "somewhere in this table" — which matches every key.
//!
//! **#141 N2 widened what counts as resolvable.** S2 could only answer for a
//! single `KeyAccess::Point`; a bounded `Range` and a multi-key batch both fell
//! back to 0 even though the footprint knew exactly where they landed. Since
//! `keycode` is memcmp-ordered, the common byte prefix of two keys contains
//! every key between them, so the published region is now that prefix — points
//! being the case where it is the whole key. The granularity that buys is
//! spelled out on `a_bounded_range_write_publishes_a_block_region`.

use mpedb::{Config, Database, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-nkey-{tag}-{}.mpedb", std::process::id()));
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

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        path.display()
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (d, path)
}

/// The digest published for table 0 after running `sql`.
fn digest_after(d: &Database, sql: &str, params: &[Value]) -> u64 {
    d.query(sql, params).unwrap();
    d.change_generation(0).expect("table 0 owns its slot").1
}

/// A single-row INSERT names one key, and two different keys produce two
/// different digests — otherwise there is nothing to filter on.
#[test]
fn a_point_write_publishes_a_key_specific_digest() {
    let (d, path) = db("point");
    let a = digest_after(&d, "INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(7)]);
    let b = digest_after(&d, "INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(42)]);
    assert_ne!(a, 0, "a point INSERT published no key digest at all");
    assert_ne!(b, 0, "a point INSERT published no key digest at all");
    assert_ne!(
        a, b,
        "writing id=7 and id=42 published the same digest — a listener cannot filter"
    );
    // The same key twice must be stable, or a listener could never match.
    let again = digest_after(&d, "UPDATE t SET v = 9 WHERE id = $1", &[Value::Int(7)]);
    assert_eq!(again, a, "the same key produced a different digest on a later write");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A write whose key is not a single resolvable point must publish 0, so a
/// key-filtering listener looks instead of sleeping.
#[test]
fn a_wide_write_publishes_zero_not_a_guess() {
    let (d, path) = db("wide");
    d.query("INSERT INTO t (id, v) VALUES (1, 1)", &[]).unwrap();
    d.query("INSERT INTO t (id, v) VALUES (2, 2)", &[]).unwrap();
    let digest = digest_after(&d, "UPDATE t SET v = v + 1", &[]);
    assert_eq!(
        digest, 0,
        "a table-wide UPDATE advertised a specific key — a listener on another key would sleep through it"
    );
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The honest-degradation rule, which is the one a batch commit exercises.
///
/// S2 collapsed straight to 0 here. #141 N2 keeps the two keys' **common
/// prefix** instead, which is sharper and — this is the part that matters —
/// just as safe, because every key the batch touched lies under the prefix
/// they share by construction.
///
/// So this asserts the GUARANTEE, not the mechanism: after a batch naming two
/// keys, a listener on either one must be told its key could have moved.
/// Asserting `digest == 0` would have been asserting how it is implemented,
/// and would fail the moment the implementation got better at its job.
#[test]
fn a_batch_of_two_keys_wakes_both_keys_listeners() {
    let (d, path) = db("batch");
    let mut s = d.begin().unwrap();
    s.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(11)]).unwrap();
    s.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(22)]).unwrap();
    s.commit().unwrap();
    let published = d.change_generation(0).expect("table 0 owns its slot").1;
    for id in [11i64, 22] {
        assert!(
            mpedb::key_region_matches(published, &[Value::Int(id)]),
            "a batch that wrote id={id} advertised a region excluding it — \
             that listener would sleep through its own change"
        );
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A bounded range names a region, and #141 N2 is the point where the footprint
/// stops throwing it away.
///
/// **The granularity is a byte prefix, and it is worth being exact about what
/// that buys.** `keycode` encodes an int64 as a tag plus 8 sign-flipped
/// big-endian bytes, so the common prefix of 10 and 12 is 8 of 9 bytes: the
/// region is the 256-aligned block containing both. A listener on id = 11 is
/// therefore NOT filtered out by a write to 10..12 — but one on 256, or 1000,
/// or 10^9, is. Against an int64 keyspace that excludes essentially all of it
/// while never excluding a neighbour, which is the honest shape of the win:
/// coarse, one-sided, and free.
///
/// (Text keys fare better — distinct prefixes are distinct regions from the
/// first differing byte, which is where this granularity is naturally useful.)
#[test]
fn a_bounded_range_write_publishes_a_block_region() {
    let (d, path) = db("range");
    for id in [10i64, 11, 12, 300, 1000, 100_000] {
        d.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(id)]).unwrap();
    }
    d.query("UPDATE t SET v = 9 WHERE id >= $1 AND id <= $2", &[Value::Int(10), Value::Int(12)])
        .unwrap();
    let published = d.change_generation(0).expect("table 0 owns its slot").1;
    assert_ne!(published, 0, "a bounded range published nothing — the Range arm is still discarded");

    // Never a missed wakeup: everything actually written must match.
    for id in [10i64, 11, 12] {
        assert!(
            mpedb::key_region_matches(published, &[Value::Int(id)]),
            "id={id} was updated but the advertised region excludes it — that \
             listener would sleep through its own change"
        );
    }
    // And the filter must actually filter: keys in other blocks are excluded.
    for id in [300i64, 1000, 100_000] {
        assert!(
            !mpedb::key_region_matches(published, &[Value::Int(id)]),
            "id={id} is in a different 256-block than the 10..12 update but was \
             not filtered — the region is not being published at all"
        );
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A write with no resolvable region must publish 0, and 0 must match
/// everything — the fail-safe direction, asserted directly rather than
/// inferred from the tests above.
#[test]
fn an_unknown_region_matches_every_key() {
    for id in [0i64, 1, -1, i64::MAX, i64::MIN, 12345] {
        assert!(
            mpedb::key_region_matches(0, &[Value::Int(id)]),
            "an unknown region excluded id={id} — a listener would sleep through a wide write"
        );
    }
}

/// The same key twice in one transaction stays specific: nothing was lost, so
/// nothing needs to degrade.
#[test]
fn one_key_twice_in_one_transaction_stays_specific() {
    let (d, path) = db("same");
    let single = digest_after(&d, "INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(5)]);
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 2 WHERE id = $1", &[Value::Int(5)]).unwrap();
    s.query("UPDATE t SET v = 3 WHERE id = $1", &[Value::Int(5)]).unwrap();
    s.commit().unwrap();
    let digest = d.change_generation(0).expect("table 0 owns its slot").1;
    assert_eq!(
        digest, single,
        "two writes to the SAME key degraded to unknown — the filter gave up for nothing"
    );
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// End to end through the waiting API: the differential the plan asks for.
/// Same answers as an unfiltered listener for changes that ARE yours, strictly
/// fewer wakeups for changes that are not.
///
/// No threads: the write happens first, so a filtered listener must burn its
/// whole timeout and an unfiltered one must return at once. That makes the
/// difference a fact about the filter rather than about scheduling.
#[test]
fn a_keyed_listener_skips_another_keys_write_but_never_its_own() {
    let (d, path) = db("keyed");
    d.query("INSERT INTO t (id, v) VALUES (5, 1)", &[]).unwrap();
    let seen = d.change_generation(0).map(|(g, _)| g).unwrap_or(0);

    // A point write to id = 5.
    d.query("UPDATE t SET v = 2 WHERE id = $1", &[Value::Int(5)]).unwrap();

    let short = std::time::Duration::from_millis(250);
    let far = vec![Some(vec![Value::Int(1_000_000)])];
    let mine = vec![Some(vec![Value::Int(5)])];

    let t0 = std::time::Instant::now();
    let woke_far = d.wait_for_change_keyed(&[0], &[seen], &far, short);
    let waited = t0.elapsed();
    assert!(woke_far.is_empty(), "a listener on a far key was woken by a write to id=5");
    assert!(waited >= std::time::Duration::from_millis(200), "it returned early ({waited:?}) — it did not actually filter and park");

    let woke_mine = d.wait_for_change_keyed(&[0], &[seen], &mine, short);
    assert_eq!(woke_mine, vec![0], "the listener on id=5 was NOT woken by the write to id=5");

    // And an unkeyed listener behaves exactly as before the filter existed.
    let woke_any = d.wait_for_change(&[0], &[seen], short);
    assert_eq!(woke_any, vec![0], "an unkeyed listener stopped seeing a change it used to see");

    drop(d);
    let _ = std::fs::remove_file(&path);
}
