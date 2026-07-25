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
//! What matters most here is the FAILURE direction. A digest that is wrong in
//! the "different key" direction makes a listener sleep through its own
//! change, which is the one thing this feature may never do. So every case
//! that cannot be resolved to a single key must publish 0 — "somewhere in this
//! table" — and the tests below pin exactly that, including the batch case
//! where two statements name two different keys.

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

/// The honest-degradation rule, which is the one a batch commit exercises:
/// two statements naming two different keys in one transaction collapse to 0.
/// Publishing the last writer's key would let a listener watching the first
/// one sleep through its own change.
#[test]
fn two_keys_in_one_transaction_collapse_to_unknown() {
    let (d, path) = db("batch");
    let mut s = d.begin().unwrap();
    s.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(11)]).unwrap();
    s.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(22)]).unwrap();
    s.commit().unwrap();
    let digest = d.change_generation(0).expect("table 0 owns its slot").1;
    assert_eq!(
        digest, 0,
        "a transaction touching two keys advertised one of them — the other key's listener would sleep"
    );
    drop(d);
    let _ = std::fs::remove_file(&path);
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
