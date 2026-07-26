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

/// `seq` defaults to the editor id, so a test that does not care about ordering
/// still gets a stable, arrival-independent one. Tests that DO care pass it.
fn sub(editor: i64, snap: u64, at: u64, remove: u64, insert: &str) -> Submission {
    seq_sub(editor, editor as u64, snap, at, remove, insert)
}

fn seq_sub(editor: i64, seq: u64, snap: u64, at: u64, remove: u64, insert: &str) -> Submission {
    Submission { editor, seq, snap, key: 1, at, remove, insert: insert.into() }
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

// ---------------------------------------------------------------------------
// #155: the counter, not the network, decides the order
// ---------------------------------------------------------------------------

/// Deterministic xorshift — no `rand` dependency, and a failing seed is a
/// failing seed forever.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, (self.next() % (i as u64 + 1)) as usize);
        }
    }
}

/// **The load-bearing test of #155.** The same submissions, delivered in every
/// order, must produce the same text.
///
/// This is what the counter is *for*: without it the tie-break was arrival
/// order, so two editors at the same offset produced a different document
/// depending on network jitter — and no other party could predict the result.
///
/// The members deliberately include ties on `at` and a genuine overlap, because
/// those are the only cases where order is observable at all.
#[test]
fn arrival_order_cannot_change_the_result() {
    let (d, path) = db("shuffle");
    let seed = "0123456789abcdefghijklmnopqrstuv";
    d.query(
        "INSERT INTO block (id, body) VALUES (1, $1)",
        &[Value::Text(seed.into())],
    )
    .unwrap();

    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut agreed: Option<String> = None;

    for round in 0..40 {
        // Reset the cell, then take the snapshot the editors decided against.
        d.query(
            "UPDATE block SET body = $1 WHERE id = 1",
            &[Value::Text(seed.into())],
        )
        .unwrap();
        let snap = d.snapshot_txn();

        let mut subs = vec![
            // Two zero-width inserts at the SAME offset: pure order.
            seq_sub(7, 10, snap, 8, 0, "<"),
            seq_sub(3, 11, snap, 8, 0, ">"),
            // A genuine overlap: one of these must lose, and which one is the
            // counter's decision, not the offset's.
            seq_sub(1, 12, snap, 16, 4, "XXXX"),
            seq_sub(9, 13, snap, 18, 4, "YYYY"),
            // Ordinary disjoint work at both ends.
            seq_sub(5, 14, snap, 0, 2, "AA"),
            seq_sub(2, 15, snap, 28, 4, "ZZ"),
        ];
        rng.shuffle(&mut subs);

        d.submit_batch("block", "body", &subs).unwrap();
        let got = body(&d);
        match &agreed {
            None => agreed = Some(got),
            Some(first) => assert_eq!(
                &got, first,
                "round {round}: the same submissions in a different arrival order \
                 produced a different document — the batch is still ordering by \
                 arrival somewhere"
            ),
        }
    }

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Disjoint splices **commute**, so ordering by counter must give exactly what
/// ordering by offset gave.
///
/// This pins that the counter costs nothing in the common case and only decides
/// conflicts. If it changed the text here, the shift arithmetic would be wrong
/// — the counter is allowed to pick winners, never to move bytes.
#[test]
fn disjoint_members_commute_so_the_counter_changes_nothing() {
    let (a, pa) = db("commute-a");
    let (b, pb) = db("commute-b");
    for d in [&a, &b] {
        d.query(
            "INSERT INTO block (id, body) VALUES (1, $1)",
            &[Value::Text("AAAA....BBBB....CCCC".into())],
        )
        .unwrap();
    }

    // Counters ASCENDING with offset in one, DESCENDING in the other. Same
    // disjoint edits either way.
    let sa = a.snapshot_txn();
    a.submit_batch(
        "block",
        "body",
        &[
            seq_sub(1, 1, sa, 0, 4, "aa"),
            seq_sub(2, 2, sa, 8, 4, "bb"),
            seq_sub(3, 3, sa, 16, 4, "cc"),
        ],
    )
    .unwrap();

    let sb = b.snapshot_txn();
    b.submit_batch(
        "block",
        "body",
        &[
            seq_sub(3, 1, sb, 16, 4, "cc"),
            seq_sub(2, 2, sb, 8, 4, "bb"),
            seq_sub(1, 3, sb, 0, 4, "aa"),
        ],
    )
    .unwrap();

    assert_eq!(body(&a), "aa....bb....cc");
    assert_eq!(
        body(&a),
        body(&b),
        "disjoint splices commute, so the counter order must not change the text — \
         the shift arithmetic is order-dependent when it should not be"
    );

    drop(a);
    drop(b);
    let _ = std::fs::remove_file(&pa);
    let _ = std::fs::remove_file(&pb);
}

/// **The semantic change #155 makes.** On a real overlap the LOWER COUNTER
/// wins, whatever the offsets are.
///
/// Before #155 the lower offset won, which meant the winner was decided by
/// where in the paragraph you happened to be typing. Now it is decided by who
/// acted first, which is the thing "first wins" was always supposed to mean.
#[test]
fn the_lower_counter_wins_an_overlap_not_the_lower_offset() {
    let (d, path) = db("who-wins");
    seed(&d, "0123456789abcdef");
    let snap = d.snapshot_txn();

    // The LATER counter sits at the LOWER offset. Under the old offset-first
    // rule it would have won; it must now lose.
    let subs = [
        seq_sub(1, 2, snap, 4, 4, "EARLY-OFFSET"),
        seq_sub(2, 1, snap, 6, 4, "LOW-COUNTER"),
    ];
    let out = d.submit_batch("block", "body", &subs).unwrap();

    assert!(
        out[1].is_committed(),
        "the member that acted FIRST lost its overlap: {out:?}"
    );
    assert!(
        !out[0].is_committed(),
        "both members of an overlapping pair were accepted: {out:?}"
    );
    assert_eq!(body(&d), "012345LOW-COUNTERabcdef");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Two people typing at the same cursor position. Neither overlaps (both are
/// zero-width), so both land — and the counter decides which text comes first.
#[test]
fn equal_offsets_interleave_by_counter() {
    let (d, path) = db("same-spot");
    seed(&d, "[]");
    let snap = d.snapshot_txn();

    let out = d
        .submit_batch(
            "block",
            "body",
            &[
                seq_sub(4, 20, snap, 1, 0, "second"),
                seq_sub(8, 19, snap, 1, 0, "first"),
            ],
        )
        .unwrap();

    assert!(out.iter().all(|v| v.is_committed()), "{out:?}");
    assert_eq!(
        body(&d),
        "[firstsecond]",
        "two zero-width inserts at one offset did not interleave in counter order"
    );

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
