//! **`submit_batch` — K editors' sub-edits in one transaction (#153).**
//!
//! The load-bearing test is `three_edits_out_of_order_all_land_correctly`, and
//! it asserts on the resulting STRING rather than on `Ok(())` on purpose: a
//! missing intra-batch rebase is not an error, it is a splice on the wrong
//! bytes. `Ok(())` would be returned either way.

use mpedb::collab::Submission;
use mpedb::collab::EditVerdictExt as _;
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-batch-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "block"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "text"
"#,
        path.display()
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (d, path)
}

fn seed(d: &Database, s: &str) {
    d.query(
        "INSERT INTO block (id, body) VALUES (1, $1)",
        &[Value::Text(s.into())],
    )
    .unwrap();
}

fn body(d: &Database) -> String {
    match d.query("SELECT body FROM block WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn sub(editor: i64, snap: u64, at: u64, remove: u64, insert: &str) -> Submission {
    Submission { editor, snap, key: 1, at, remove, insert: insert.into() }
}

/// **The whole point.** Three editors splice disjoint parts of one block,
/// submitted in an order that has nothing to do with their offsets, and all
/// three land where their authors meant.
///
/// A batch without intra-batch rebasing would still return `Ok` — the first
/// edit changes the length, and the later ones then splice at coordinates that
/// no longer mean what they meant. That is why the assertion is on the text.
#[test]
fn three_edits_out_of_order_all_land_correctly() {
    let (d, path) = db("order");
    seed(&d, "AAAA....BBBB....CCCC");
    let snap = d.snapshot_txn();

    // Arrival order: tail, head, middle. Each replaces 4 bytes with 2, so every
    // one of them shifts the ones after it.
    let subs = [
        sub(3, snap, 16, 4, "cc"),
        sub(1, snap, 0, 4, "aa"),
        sub(2, snap, 8, 4, "bb"),
    ];
    let out = d.submit_batch("block", "body", &subs).unwrap();

    assert!(
        out.iter().all(|v| v.is_committed()),
        "every disjoint edit should have landed, got {out:?}"
    );
    assert_eq!(
        body(&d),
        "aa....bb....cc",
        "an edit landed on the wrong bytes — the batch is not rebasing its own members \
         against each other"
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same three edits applied one commit at a time, each rebased by the
/// engine's committed-ring walk (#151), must give the SAME text. If the two
/// rebases disagreed, one of them would be wrong — and this is the only test
/// that would notice.
#[test]
fn a_batch_agrees_with_the_same_edits_committed_one_by_one() {
    let (d, path) = db("agree");
    seed(&d, "AAAA....BBBB....CCCC");
    let snap = d.snapshot_txn();
    let sql = "UPDATE block SET body = splice(body, $1, $2, $3) WHERE id = $4";
    for (at, ins) in [(0i64, "aa"), (8, "bb"), (16, "cc")] {
        let p = [Value::Int(at), Value::Int(4), Value::Text(ins.into()), Value::Int(1)];
        let mut s = d.begin_guarded_with(snap, &[(sql, &p[..])]).unwrap();
        s.query(sql, &p).unwrap();
        s.commit().unwrap();
    }
    let one_at_a_time = body(&d);

    let (d2, path2) = db("agree2");
    seed(&d2, "AAAA....BBBB....CCCC");
    let snap2 = d2.snapshot_txn();
    let subs = [
        sub(3, snap2, 16, 4, "cc"),
        sub(1, snap2, 0, 4, "aa"),
        sub(2, snap2, 8, 4, "bb"),
    ];
    d2.submit_batch("block", "body", &subs).unwrap();

    assert_eq!(
        body(&d2),
        one_at_a_time,
        "the batch and the one-at-a-time path disagree — two rebases that must be the same \
         arithmetic are not"
    );

    drop(d);
    drop(d2);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&path2);
}

/// **A real overlap loses alone.** One member wants bytes another member is
/// already rewriting; that member is refused and every other member still
/// commits. Without this, the implementation could have been "refuse the whole
/// batch", which would make one careless editor able to stall everyone.
#[test]
fn an_overlapping_member_loses_alone() {
    let (d, path) = db("overlap");
    seed(&d, "AAAA....BBBB");
    let snap = d.snapshot_txn();

    let subs = [
        sub(1, snap, 0, 4, "aa"),
        sub(2, snap, 2, 2, "XX"), // inside editor 1's range
        sub(3, snap, 8, 4, "bb"),
    ];
    let out = d.submit_batch("block", "body", &subs).unwrap();

    assert!(out[0].is_committed(), "the first edit should stand: {out:?}");
    assert!(out[2].is_committed(), "an unrelated edit was taken down with the loser: {out:?}");
    assert!(
        !out[1].is_committed(),
        "an edit overlapping another member should lose: {out:?}"
    );
    assert_eq!(body(&d), "aa....bb", "the surviving members did not both land");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **Nobody starves.** The editor whose offset always sorts last still lands
/// every round, as long as it does not overlap. Being slow is not what costs
/// an edit; wanting the same bytes is.
#[test]
fn the_last_in_sort_order_still_lands_every_round() {
    let (d, path) = db("starve");
    seed(&d, "0123456789abcdefghij");

    for round in 0..5 {
        let snap = d.snapshot_txn();
        // The "slow" editor is always at the far end of the block.
        let subs = [
            sub(1, snap, 0, 1, "x"),
            sub(9, snap, 10, 1, "z"),
        ];
        let out = d.submit_batch("block", "body", &subs).unwrap();
        assert!(
            out[1].is_committed(),
            "round {round}: the last-sorted editor was refused — it is being starved, not \
             conflicted: {out:?}"
        );
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// An empty batch is not an error and does not open a transaction.
#[test]
fn an_empty_batch_is_a_no_op() {
    let (d, path) = db("empty");
    seed(&d, "abc");
    let out = d.submit_batch("block", "body", &[]).unwrap();
    assert!(out.is_empty());
    assert_eq!(body(&d), "abc");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A single-member batch is exactly one edit, and must behave like one.
#[test]
fn a_batch_of_one_is_just_the_edit() {
    let (d, path) = db("one");
    seed(&d, "hello world");
    let snap = d.snapshot_txn();
    let out = d.submit_batch("block", "body", &[sub(1, snap, 6, 5, "there")]).unwrap();
    assert!(out[0].is_committed());
    assert_eq!(body(&d), "hello there");
    drop(d);
    let _ = std::fs::remove_file(&path);
}
