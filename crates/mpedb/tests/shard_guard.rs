//! **The shard guard: acting on a notification without a global lock (#142 G1).**
//!
//! A notification says something changed; it gives no safe way to ACT. Two
//! listeners that wake on the same change and both write will race. PostgreSQL
//! answers that with `FOR UPDATE SKIP LOCKED` or advisory locks, on top of the
//! cluster-global commit lock measured in benchmarks/notify.md.
//!
//! Ours answers it with the thing that already made notification cheap: the
//! surface is already computed. Every statement is a compiled plan with a
//! footprint, so an action made of several statements has the union of theirs
//! — bigger than one statement, far smaller than global.
//!
//! The load-bearing test here is NOT that conflicts are caught. A global lock
//! would pass that one. It is `two_disjoint_actions_both_commit`.

use mpedb::{Config, Database, Error, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-guard-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "orders"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "audit"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "other"
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

/// **The one that matters.** Two guarded actions on tables that do not overlap
/// must BOTH commit. A test that only proves conflicts are caught would pass
/// just as well against a global lock — this is what says the lock is a shard.
#[test]
fn two_disjoint_actions_both_commit() {
    let (d, path) = db("disjoint");
    let snap = d.snapshot_txn();

    // Action A guards orders; action B guards other. Interleaved on purpose:
    // both are open against the same snapshot before either commits.
    let mut a = d.begin_guarded_for(snap, &["INSERT INTO orders (id, v) VALUES ($1, $2)"]).unwrap();
    a.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    a.commit().expect("action A on `orders` should commit");

    let mut b = d.begin_guarded_for(snap, &["INSERT INTO other (id, v) VALUES ($1, $2)"]).unwrap();
    b.query("INSERT INTO other (id, v) VALUES (1, 1)", &[]).unwrap();
    b.commit().expect(
        "action B on `other` was refused because A wrote `orders` — the guard is \
         behaving like a global lock, not a shard",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The guard bites when the surfaces DO overlap: a commit that lands after the
/// snapshot must make the guarded action fail rather than overwrite.
#[test]
fn an_overlapping_write_after_the_snapshot_is_refused() {
    let (d, path) = db("overlap");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    let snap = d.snapshot_txn();

    // Someone else moves `orders` after we took our snapshot.
    d.query("UPDATE orders SET v = 99 WHERE id = 1", &[]).unwrap();

    let mut s = d.begin_guarded_for(snap, &["UPDATE orders SET v = $1 WHERE id = $2"]).unwrap();
    s.query("UPDATE orders SET v = 2 WHERE id = 1", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!("expected WriteConflict, got {other:?} — the lost update went through"),
    }

    // And the interfering write is still there: the guard refused, it did not
    // half-apply.
    let got = d.query("SELECT v FROM orders WHERE id = 1", &[]).unwrap();
    let mpedb::ExecResult::Rows { rows, .. } = got else { panic!("expected rows") };
    assert_eq!(rows[0][0], Value::Int(99), "the refused action left a partial effect");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The union, including the READ half.** An action that SELECTs from
/// `orders` and INSERTs into `audit` must conflict with a write to *orders* —
/// not only with one to `audit`. Guarding writes alone would let the row the
/// decision was based on move and still let the action through: a lost update
/// wearing a guard.
#[test]
fn the_read_half_of_the_surface_is_guarded_too() {
    let (d, path) = db("readhalf");
    d.query("INSERT INTO orders (id, v) VALUES (1, 5)", &[]).unwrap();
    let snap = d.snapshot_txn();

    // The interfering write touches ONLY the table the action reads.
    d.query("UPDATE orders SET v = 6 WHERE id = 1", &[]).unwrap();

    let mut s = d
        .begin_guarded_for(
            snap,
            &[
                "SELECT v FROM orders WHERE id = $1",
                "INSERT INTO audit (id, v) VALUES ($1, $2)",
            ],
        )
        .unwrap();
    s.query("INSERT INTO audit (id, v) VALUES (1, 5)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict from the READ half of the surface, got {other:?} — \
             the decision's input moved and the action committed anyway"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Declaring is what keeps the surface small, and declaring MORE must make it
/// bigger — a declaration mentioning `other` must feel a write to `other` even
/// though nothing in the action touched it. That is the property #142 G2/G3
/// depend on: the shard is known before anything runs.
#[test]
fn a_declared_but_unused_statement_still_widens_the_guard() {
    let (d, path) = db("declared");
    let snap = d.snapshot_txn();
    d.query("INSERT INTO other (id, v) VALUES (7, 7)", &[]).unwrap();

    let mut s = d
        .begin_guarded_for(
            snap,
            &[
                "INSERT INTO orders (id, v) VALUES ($1, $2)",
                "DELETE FROM other WHERE id = $1", // declared, never run
            ],
        )
        .unwrap();
    s.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict: `other` was declared as a possible operation \
             and was written after the snapshot, got {other:?}"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// An undeclared statement widens the surface as it runs, so a wrong
/// declaration makes the guard BIGGER, never wrong.
#[test]
fn an_undeclared_statement_widens_rather_than_escapes() {
    let (d, path) = db("undeclared");
    let snap = d.snapshot_txn();
    d.query("INSERT INTO audit (id, v) VALUES (9, 9)", &[]).unwrap();

    // Declares only `orders`, then writes `audit` too.
    let mut s = d.begin_guarded_for(snap, &["INSERT INTO orders (id, v) VALUES ($1, $2)"]).unwrap();
    s.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    s.query("INSERT INTO audit (id, v) VALUES (1, 1)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "a statement outside the declaration escaped the guard entirely: {other:?}"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// An unguarded session is untouched by any of this — the default path must
/// not start failing because the guard exists.
#[test]
fn an_unguarded_session_is_unaffected() {
    let (d, path) = db("unguarded");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    let mut s = d.begin().unwrap();
    s.query("UPDATE orders SET v = 2 WHERE id = 1", &[]).unwrap();
    s.commit().expect("an ordinary session must commit regardless of what else happened");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **A documented limit, as a test rather than as prose.** The OPT ring
/// witnesses 64 commits. A guard whose snapshot is older cannot be answered, so
/// it is refused — even when the surface is disjoint from everything that
/// happened. That binds how long a caller may think between reading and
/// writing, and it is the same shape as #135's rate law: a property of a
/// bounded structure, not a bug.
///
/// If someone later widens the ring, this test fails and should be updated
/// deliberately — which is the point of pinning it.
#[test]
fn a_snapshot_older_than_the_ring_is_refused_conservatively() {
    let (d, path) = db("toodold");
    let snap = d.snapshot_txn();

    // 65 commits, all to a table the guarded action never mentions.
    for i in 0..65i64 {
        d.query("INSERT INTO other (id, v) VALUES ($1, 1)", &[Value::Int(i)]).unwrap();
    }

    let mut s = d.begin_guarded_for(snap, &["INSERT INTO orders (id, v) VALUES ($1, $2)"]).unwrap();
    s.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "a snapshot {} commits old was trusted ({other:?}) — the ring cannot \
             witness that far back, so the only safe answer is to refuse",
            65
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The retry loop a caller is expected to write, end to end: refuse, re-read,
/// succeed. Without this the guard is a way to fail rather than a way to act.
#[test]
fn the_expected_retry_succeeds_on_the_second_attempt() {
    let (d, path) = db("retry");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();

    let stale = d.snapshot_txn();
    d.query("UPDATE orders SET v = 2 WHERE id = 1", &[]).unwrap();

    let sql = ["UPDATE orders SET v = $1 WHERE id = $2"];
    let mut attempts = 0;
    let mut snap = stale;
    loop {
        attempts += 1;
        assert!(attempts <= 3, "retry did not converge");
        let mut s = d.begin_guarded_for(snap, &sql).unwrap();
        s.query("UPDATE orders SET v = 3 WHERE id = 1", &[]).unwrap();
        match s.commit() {
            Ok(()) => break,
            Err(Error::WriteConflict) => snap = d.snapshot_txn(), // re-read and go again
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert_eq!(attempts, 2, "expected exactly one refusal then one success");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **#143, and the reason it exists.** Two actions on the SAME table but
/// different rows must both commit. Table granularity made these conflict, and
/// arm E measured what that cost: per-user sharding bought nothing and guarded
/// throughput was flat while the unguarded control scaled linearly.
///
/// The key is what distinguishes them, so this is the row-level analogue of
/// `two_disjoint_actions_both_commit` — and like that one, a guard that only
/// caught conflicts would pass every other test in this file and still fail
/// here.
#[test]
fn two_actions_on_different_rows_of_one_table_both_commit() {
    let (d, path) = db("rows");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    d.query("INSERT INTO orders (id, v) VALUES (2, 2)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let sql = ["UPDATE orders SET v = $1 WHERE id = $2"];

    let mut a = d.begin_guarded_for(snap, &sql).unwrap();
    a.query("UPDATE orders SET v = 10 WHERE id = 1", &[]).unwrap();
    a.commit().expect("action on id=1 should commit");

    // Same table, same snapshot, different row. Before #143 this was refused.
    let mut b = d.begin_guarded_for(snap, &sql).unwrap();
    b.query("UPDATE orders SET v = 20 WHERE id = 2", &[]).unwrap();
    b.commit().expect(
        "action on id=2 was refused because id=1 moved — the guard is still \
         table-granular and per-row sharding buys nothing",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same key still conflicts. Without this, the row-level filter could be
/// "always allow" and the test above would not notice.
#[test]
fn two_actions_on_the_same_row_still_conflict() {
    let (d, path) = db("samerow");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    let snap = d.snapshot_txn();
    let sql = ["UPDATE orders SET v = $1 WHERE id = $2"];

    let mut a = d.begin_guarded_for(snap, &sql).unwrap();
    a.query("UPDATE orders SET v = 10 WHERE id = 1", &[]).unwrap();
    a.commit().unwrap();

    let mut b = d.begin_guarded_for(snap, &sql).unwrap();
    b.query("UPDATE orders SET v = 20 WHERE id = 1", &[]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!("two actions on the SAME row both committed ({other:?}) — a lost update"),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A writer that cannot name where it landed must still conflict with
/// everything on its tables. This is the fail-safe direction of #143: refining
/// by key may only narrow conflicts for writers that NAMED a key.
#[test]
fn a_wide_write_still_conflicts_with_every_key() {
    let (d, path) = db("widewrite");
    for id in 1..=3i64 {
        d.query("INSERT INTO orders (id, v) VALUES ($1, 1)", &[Value::Int(id)]).unwrap();
    }
    let snap = d.snapshot_txn();

    // Names no single key: the region summary must degrade to "anywhere".
    d.query("UPDATE orders SET v = v + 1", &[]).unwrap();

    let mut s = d
        .begin_guarded_for(snap, &["UPDATE orders SET v = $1 WHERE id = $2"])
        .unwrap();
    s.query("UPDATE orders SET v = 99 WHERE id = 3", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "a table-wide UPDATE did not conflict with a point action ({other:?}) — \
             the key filter narrowed for a writer that never named a key"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Symmetric fail-safe: a READER that cannot name its key must be guarded
/// against every write to its tables, or an action deciding from a scan could
/// commit on data that moved underneath it.
#[test]
fn a_guard_that_scanned_conflicts_with_any_write() {
    let (d, path) = db("scanguard");
    for id in 1..=3i64 {
        d.query("INSERT INTO orders (id, v) VALUES ($1, 1)", &[Value::Int(id)]).unwrap();
    }
    let snap = d.snapshot_txn();
    d.query("UPDATE orders SET v = 5 WHERE id = 1", &[]).unwrap();

    let mut s = d
        .begin_guarded_for(
            snap,
            &["SELECT v FROM orders", "INSERT INTO audit (id, v) VALUES ($1, $2)"],
        )
        .unwrap();
    s.query("SELECT v FROM orders", &[]).unwrap(); // names no key
    s.query("INSERT INTO audit (id, v) VALUES (1, 1)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "an action that decided from a full scan committed after the scanned \
             table changed ({other:?})"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}
