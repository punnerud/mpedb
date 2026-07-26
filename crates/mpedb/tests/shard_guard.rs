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
name = "blocks"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "ord"
  type = "int64"

  [[table.column]]
  name = "body"
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
/// witnesses [`OPT_RING_SLOTS`](mpedb_core::shm::OPT_RING_SLOTS) commits — 256
/// since #149, 64 before it. A guard whose snapshot is older cannot be
/// answered, so it is refused even when the surface is disjoint from everything
/// that happened. That binds how long a caller may think between reading and
/// writing, and it is the same shape as #135's rate law: a property of a
/// bounded structure, not a bug.
///
/// If someone later widens the ring again, this test fails and should be
/// updated deliberately — which is the point of pinning it. Its companion
/// `a_snapshot_a_hundred_commits_old_is_still_witnessed` pins the other side,
/// so a widening cannot be faked by simply trusting everything.
#[test]
fn a_snapshot_older_than_the_ring_is_refused_conservatively() {
    let (d, path) = db("toodold");
    let snap = d.snapshot_txn();

    // Past the window, all to a table the guarded action never mentions.
    for i in 0..300i64 {
        d.query("INSERT INTO other (id, v) VALUES ($1, 1)", &[Value::Int(i)]).unwrap();
    }

    let mut s = d.begin_guarded_for(snap, &["INSERT INTO orders (id, v) VALUES ($1, $2)"]).unwrap();
    s.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "a snapshot 300 commits old was trusted ({other:?}) — the ring cannot \
             witness that far back, so the only safe answer is to refuse"
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
///
/// It declares with VALUES, because that is the only form in which the claim
/// is true: `WHERE id = $2` with no value for `$2` names every row of the
/// table, and a declaration has to hold for the statement that never runs.
#[test]
fn two_actions_on_different_rows_of_one_table_both_commit() {
    let (d, path) = db("rows");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    d.query("INSERT INTO orders (id, v) VALUES (2, 2)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let sql = "UPDATE orders SET v = $1 WHERE id = $2";

    let pa = [Value::Int(10), Value::Int(1)];
    let mut a = d.begin_guarded_with(snap, &[(sql, &pa[..])]).unwrap();
    a.query(sql, &pa).unwrap();
    a.commit().expect("action on id=1 should commit");

    // Same table, same snapshot, different row. Before #143 this was refused.
    let pb = [Value::Int(20), Value::Int(2)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &pb[..])]).unwrap();
    b.query(sql, &pb).unwrap();
    b.commit().expect(
        "action on id=2 was refused because id=1 moved — the guard is still \
         table-granular and per-row sharding buys nothing",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The same pair, declared without values, MUST conflict.** `WHERE id = $2`
/// with no `$2` is a statement that may touch any row, and a guard that let
/// this through would be promising something it cannot keep for the branch
/// that did not run.
///
/// This is the price of the fix and it is deliberately pinned: the convenient
/// form is the safe one, and the precise one is opt-in.
#[test]
fn the_same_pair_declared_without_values_is_table_granular() {
    let (d, path) = db("rows-novalues");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    d.query("INSERT INTO orders (id, v) VALUES (2, 2)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let sql = ["UPDATE orders SET v = $1 WHERE id = $2"];
    let mut a = d.begin_guarded_for(snap, &sql).unwrap();
    a.query("UPDATE orders SET v = 10 WHERE id = 1", &[]).unwrap();
    a.commit().expect("the first action should commit");

    let mut b = d.begin_guarded_for(snap, &sql).unwrap();
    b.query("UPDATE orders SET v = 20 WHERE id = 2", &[]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — a declaration with no values named every \
             row, so it cannot be treated as naming one"
        ),
    }

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

/// **#146 K1, and the reason it exists.** One worker MOVES a block (writes
/// `ord`) while another EDITS the same block's text (`body`) — the SAME ROW,
/// different columns, both from the same snapshot. Both must commit.
///
/// This is the hard case in collaborative editing, and it is hard only when a
/// move is expressed as delete-plus-insert: then it tears the text out from
/// under everyone editing inside it. As a column write it is independent of
/// the text, so the edit **follows the move** structurally rather than by any
/// merge logic.
///
/// Row granularity (#143) passes every other test in this file and fails this
/// one, which is what makes it load-bearing rather than decorative. Verified
/// to fail with the column check removed.
#[test]
fn a_move_and_an_edit_on_one_row_both_commit() {
    let (d, path) = db("cols");
    d.query("INSERT INTO blocks (id, ord, body) VALUES (1, 10, 100)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let mv = ["UPDATE blocks SET ord = $1 WHERE id = $2"];
    let ed = ["UPDATE blocks SET body = $1 WHERE id = $2"];

    let mut a = d.begin_guarded_for(snap, &mv).unwrap();
    a.query(mv[0], &[Value::Int(20), Value::Int(1)]).unwrap();
    a.commit().expect("the move should commit");

    let mut b = d.begin_guarded_for(snap, &ed).unwrap();
    b.query(ed[0], &[Value::Int(200), Value::Int(1)]).unwrap();
    b.commit().expect(
        "the edit was refused because the block moved — a concurrent edit must \
         FOLLOW a move, and column granularity is what makes that structural",
    );

    // Both landed: the block is at its new position WITH the new text.
    let got = d.query("SELECT ord, body FROM blocks WHERE id = 1", &[]).unwrap();
    let mpedb::ExecResult::Rows { rows, .. } = got else { panic!("expected rows") };
    assert_eq!(rows[0][0], Value::Int(20), "the move was lost");
    assert_eq!(rows[0][1], Value::Int(200), "the edit was lost");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Two writes to the SAME column of the same row still conflict — without
/// this, the column filter could be "always allow".
#[test]
fn two_writes_to_one_column_still_conflict() {
    let (d, path) = db("samecol");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    let snap = d.snapshot_txn();
    let sql = ["UPDATE orders SET v = $1 WHERE id = $2"];

    let mut a = d.begin_guarded_for(snap, &sql).unwrap();
    a.query(sql[0], &[Value::Int(2), Value::Int(1)]).unwrap();
    a.commit().unwrap();

    let mut b = d.begin_guarded_for(snap, &sql).unwrap();
    b.query(sql[0], &[Value::Int(3), Value::Int(1)]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!("two writes to the same column both committed ({other:?})"),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A DELETE names no columns because it removes the whole row, so it must
/// conflict with a write to any of them. The fail-safe direction for K1.
#[test]
fn a_delete_conflicts_with_a_single_column_write() {
    let (d, path) = db("delcols");
    d.query("INSERT INTO orders (id, v) VALUES (1, 1)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let mut a = d.begin_guarded_for(snap, &["DELETE FROM orders WHERE id = $1"]).unwrap();
    a.query("DELETE FROM orders WHERE id = 1", &[]).unwrap();
    a.commit().unwrap();

    let mut b = d
        .begin_guarded_for(snap, &["UPDATE orders SET v = $1 WHERE id = $2"])
        .unwrap();
    b.query("UPDATE orders SET v = 5 WHERE id = 1", &[]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "a column write did not conflict with a DELETE of the same row ({other:?}) — \
             the row is gone, so every column is"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// An expression that READS a column puts it in the mask, because a concurrent
/// change to it would have altered the result. `SET ord = ord + 1` therefore
/// conflicts with a write to `ord`, where `SET ord = $1` would not.
///
/// This is where the column filter earns its exactness: `Instr::PushCol` is the
/// only instruction that touches the row, so "reads nothing" is provable rather
/// than assumed.
#[test]
fn an_expression_that_reads_a_column_guards_it() {
    let (d, path) = db("readcol");
    d.query("INSERT INTO blocks (id, ord, body) VALUES (1, 10, 100)", &[]).unwrap();
    let snap = d.snapshot_txn();

    // Someone bumps `body`.
    d.query("UPDATE blocks SET body = 999 WHERE id = 1", &[]).unwrap();

    // An action that writes `ord` but READS `body` must feel that.
    let sql = ["UPDATE blocks SET ord = body WHERE id = $1"];
    let mut s = d.begin_guarded_for(snap, &sql).unwrap();
    s.query(sql[0], &[Value::Int(1)]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "an action computing from `body` committed after `body` changed ({other:?}) — \
             the read side of the column mask is missing"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The declaration must cover a read taken BEFORE the session opened.**
///
/// This is the documented pattern, and the only one that makes sense with a
/// think time in it: take a snapshot, read what you need, work for as long as
/// you like outside any lock, then open the guarded session and write. The
/// read is what the write is derived from, so a concurrent change to it is
/// exactly the lost update the guard exists to refuse.
///
/// Nothing executed inside the session ever touches that row — only the
/// declaration names it. So this is a test of the DECLARATION, not of
/// accumulation, and it is the case #143's key regions can silently drop: the
/// surface narrowed from "the table" to "the keys I touched", and the key I
/// read outside is not one of them.
#[test]
fn a_read_taken_before_the_session_is_still_guarded() {
    let (d, path) = db("declared-read");
    d.query("INSERT INTO orders (id, v) VALUES (1, 10)", &[]).unwrap();
    // A DIFFERENT key from the one being read: the region summary does not
    // include the table, so reusing key 1 on both sides would collide in the
    // bloom and pass for the wrong reason.
    d.query("INSERT INTO audit (id, v) VALUES (7, 0)", &[]).unwrap();

    let snap = d.snapshot_txn();
    // Read row 1 of `orders` OUTSIDE the session — the value the decision is
    // made from.
    let seen = match d.query("SELECT v FROM orders WHERE id = 1", &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(v) => v,
            _ => panic!("expected an int"),
        },
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(seen, 10);

    // Someone else changes precisely that row while we think. Through a
    // session, so the commit records the exact key it wrote — an auto-commit
    // publishes "anywhere in this table" and would make this pass without
    // proving anything.
    let mut other = d.begin().unwrap();
    other.query("UPDATE orders SET v = 99 WHERE id = 1", &[]).unwrap();
    other.commit().unwrap();

    // Now act on what we read, writing somewhere else entirely.
    let mut s = d
        .begin_guarded_for(
            snap,
            &[
                "SELECT v FROM orders WHERE id = $1",
                "UPDATE audit SET v = $1 WHERE id = $2",
            ],
        )
        .unwrap();
    s.query("UPDATE audit SET v = 11 WHERE id = 7", &[]).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — the guarded action committed a decision \
             derived from a value that had already changed. The declared read was not covered."
        ),
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same declared read, this time named EXACTLY — and it must still catch
/// the change. Precision is only worth having if it does not quietly become
/// permission: the guard now knows the read was of `orders` row 1, and row 1
/// is what moved.
#[test]
fn a_precisely_declared_read_still_catches_its_own_row() {
    let (d, path) = db("declared-read-exact");
    d.query("INSERT INTO orders (id, v) VALUES (1, 10)", &[]).unwrap();
    d.query("INSERT INTO audit (id, v) VALUES (7, 0)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let read = "SELECT v FROM orders WHERE id = $1";
    let write = "UPDATE audit SET v = $1 WHERE id = $2";
    let rp = [Value::Int(1)];
    let wp = [Value::Int(11), Value::Int(7)];

    let mut other = d.begin().unwrap();
    other.query("UPDATE orders SET v = 99 WHERE id = 1", &[]).unwrap();
    other.commit().unwrap();

    let mut s = d
        .begin_guarded_with(snap, &[(read, &rp[..]), (write, &wp[..])])
        .unwrap();
    s.query(write, &wp).unwrap();
    match s.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — the declared read named row 1 and row 1 \
             changed, so the decision was made from a stale value"
        ),
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// And the half that makes precision worth anything: a change to a DIFFERENT
/// row of the table that was read must not refuse the action. Without this the
/// test above is satisfied by "always conflict".
#[test]
fn a_precisely_declared_read_ignores_another_row() {
    let (d, path) = db("declared-read-other");
    d.query("INSERT INTO orders (id, v) VALUES (1, 10)", &[]).unwrap();
    d.query("INSERT INTO orders (id, v) VALUES (3, 30)", &[]).unwrap();
    d.query("INSERT INTO audit (id, v) VALUES (7, 0)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let read = "SELECT v FROM orders WHERE id = $1";
    let write = "UPDATE audit SET v = $1 WHERE id = $2";
    let rp = [Value::Int(1)];
    let wp = [Value::Int(11), Value::Int(7)];

    // Someone changes row 3. We read row 1.
    let mut other = d.begin().unwrap();
    other.query("UPDATE orders SET v = 99 WHERE id = 3", &[]).unwrap();
    other.commit().unwrap();

    let mut s = d
        .begin_guarded_with(snap, &[(read, &rp[..]), (write, &wp[..])])
        .unwrap();
    s.query(write, &wp).unwrap();
    s.commit().expect(
        "a change to row 3 refused an action whose declared read named row 1 — the declaration \
         is not using the key it was given",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The move and the edit, through the declared API (#146 K1).** One editor
/// declares it may read and write `body`; a mover declares it may read and
/// write `ord`. Same row, same snapshot, both must commit — which is only true
/// if a declared SELECT contributes the columns it actually reads rather than
/// the whole row.
#[test]
fn a_declared_move_and_a_declared_edit_on_one_row_both_commit() {
    let (d, path) = db("declared-move");
    d.query("INSERT INTO blocks (id, ord, body) VALUES (1, 1, 100)", &[]).unwrap();
    let snap = d.snapshot_txn();

    let ed_read = "SELECT body FROM blocks WHERE id = $1";
    let ed_write = "UPDATE blocks SET body = $1 WHERE id = $2";
    let mv_read = "SELECT ord FROM blocks WHERE id = $1";
    let mv_write = "UPDATE blocks SET ord = $1 WHERE id = $2";
    let k = [Value::Int(1)];
    let wp = [Value::Int(2), Value::Int(1)];

    let mut ed = d
        .begin_guarded_with(snap, &[(ed_read, &k[..]), (ed_write, &wp[..])])
        .unwrap();
    ed.query(ed_write, &[Value::Int(200), Value::Int(1)]).unwrap();
    ed.commit().expect("the edit should commit");

    let mut mv = d
        .begin_guarded_with(snap, &[(mv_read, &k[..]), (mv_write, &wp[..])])
        .unwrap();
    mv.query(mv_write, &[Value::Int(9), Value::Int(1)]).unwrap();
    mv.commit().expect(
        "the move was refused by an edit to a different column of the same row — a declared \
         SELECT is still claiming the whole row",
    );

    // Both landed.
    match d.query("SELECT ord, body FROM blocks WHERE id = 1", &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int(9), "the move did not land");
            assert_eq!(rows[0][1], Value::Int(200), "the edit did not land");
        }
        other => panic!("expected rows, got {other:?}"),
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **#149, the width that actually bit.** Two guarded actions on rows whose key
/// hashes land on the SAME 64-bit region bit must both commit.
///
/// Keys 2 and 5 are not a coincidence: they are the pair arm F measured
/// colliding, and before #149 those two editors conflicted on every single
/// commit while touching rows that have nothing to do with each other. The
/// comparison folded both exact keys through `region_bit`, so exactness was
/// thrown away at the last step by both sides.
#[test]
fn two_rows_that_share_a_bloom_bit_no_longer_conflict() {
    let (d, path) = db("bloom-collide");
    for id in [2i64, 5] {
        d.query("INSERT INTO orders (id, v) VALUES ($1, 0)", &[Value::Int(id)]).unwrap();
    }
    let snap = d.snapshot_txn();
    let sql = "UPDATE orders SET v = $1 WHERE id = $2";

    let pa = [Value::Int(10), Value::Int(2)];
    let mut a = d.begin_guarded_with(snap, &[(sql, &pa[..])]).unwrap();
    a.query(sql, &pa).unwrap();
    a.commit().expect("the first action should commit");

    let pb = [Value::Int(20), Value::Int(5)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &pb[..])]).unwrap();
    b.query(sql, &pb).unwrap();
    b.commit().expect(
        "row 5 was refused because row 2 moved — the two share a region bit, and the \
         comparison is still folding exact keys through a 64-bit Bloom",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same key still conflicts once the comparison is exact. Without this,
/// "compare the exact sets" could be implemented as "never intersect" and the
/// test above would not notice.
#[test]
fn exact_key_comparison_still_catches_the_same_row() {
    let (d, path) = db("exact-same");
    d.query("INSERT INTO orders (id, v) VALUES (2, 0)", &[]).unwrap();
    let snap = d.snapshot_txn();
    let sql = "UPDATE orders SET v = $1 WHERE id = $2";
    let p = [Value::Int(10), Value::Int(2)];

    let mut a = d.begin_guarded_with(snap, &[(sql, &p[..])]).unwrap();
    a.query(sql, &p).unwrap();
    a.commit().unwrap();

    let mut b = d.begin_guarded_with(snap, &[(sql, &p[..])]).unwrap();
    b.query(sql, &p).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!("expected WriteConflict on the SAME row, got {other:?}"),
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **An action wider than the ring entry falls back, and the fallback is
/// conservative.** More than `OFP_MAX_KEYS` keys cannot be carried exactly, so
/// exactness is given up and the Bloom decides — which may cost a retry and may
/// never miss a conflict. Here the two actions genuinely share a row, so the
/// answer must be refusal regardless of which path was taken.
#[test]
fn more_keys_than_the_entry_holds_stays_conservative() {
    let (d, path) = db("overflow-keys");
    for id in 0..24i64 {
        d.query("INSERT INTO orders (id, v) VALUES ($1, 0)", &[Value::Int(id)]).unwrap();
    }
    let snap = d.snapshot_txn();
    let sql = "UPDATE orders SET v = $1 WHERE id = $2";

    // Twelve keys — past the eight a ring entry can name.
    let mut a = d.begin_guarded_for(snap, &[sql]).unwrap();
    for id in 0..12i64 {
        a.query(sql, &[Value::Int(1), Value::Int(id)]).unwrap();
    }
    a.commit().expect("the wide action should commit");

    // Row 3 is inside what A wrote, so this must be refused.
    let pb = [Value::Int(9), Value::Int(3)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &pb[..])]).unwrap();
    b.query(sql, &pb).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — an action too wide to name its keys \
             exactly must not be read as naming none of them"
        ),
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The ring's history is what bounds think time, and it is now 256 commits.**
/// A guarded action holding a snapshot across 100 unrelated commits used to be
/// refused with `SnapshotTooOld` — the question was unanswerable, not a
/// conflict. It is answerable now.
#[test]
fn a_snapshot_a_hundred_commits_old_is_still_witnessed() {
    let (d, path) = db("deep-history");
    d.query("INSERT INTO orders (id, v) VALUES (1, 0)", &[]).unwrap();
    d.query("INSERT INTO other (id, v) VALUES (1, 0)", &[]).unwrap();
    let snap = d.snapshot_txn();

    // 100 commits to a table we do not touch — well past the old 64.
    for i in 0..100i64 {
        let mut w = d.begin().unwrap();
        w.query("UPDATE other SET v = $1 WHERE id = 1", &[Value::Int(i)]).unwrap();
        w.commit().unwrap();
    }

    let sql = "UPDATE orders SET v = $1 WHERE id = $2";
    let p = [Value::Int(7), Value::Int(1)];
    let mut s = d.begin_guarded_with(snap, &[(sql, &p[..])]).unwrap();
    s.query(sql, &p).unwrap();
    s.commit().expect(
        "a snapshot 100 commits old was refused — the ring can no longer witness the window, \
         which caps how long a guarded action may think",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

