//! **A batch whose members hold DIFFERENT snapshots (#154).**
//!
//! `submit_batch` guards against the oldest snapshot in the set, which is
//! right — a guard against a newer one would forgive a decision made against a
//! version that had already moved. But the *rebase walk* is a different
//! question, and using the same oldest snapshot for it is wrong: a member whose
//! own snapshot is newer has ALREADY seen the commits in between, and its `at`
//! is expressed in coordinates that already include them. Walking it from the
//! batch minimum applies those deltas a second time.
//!
//! That is not a lost edit. It is a valid splice on the wrong bytes, with no
//! error — which is why the assertion here is on the resulting STRING.

use mpedb::collab::{EditVerdictExt as _, Submission};
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-mixsnap-{tag}-{}.mpedb", std::process::id()));
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

fn body(d: &Database) -> String {
    match d.query("SELECT body FROM block WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Two editors, two different snapshots, disjoint bytes — and the newer one
/// must land where it meant.
///
/// Timeline:
///   snap0: "AAAA....BBBB....CCCC"          (20 bytes)
///   commit: splice(0, 4, "aa")  ->  "aa....BBBB....CCCC"   (18 bytes, delta -2)
///   snap1 = that commit
///
/// Editor A read at snap0 and wants "CCCC", which is at 16 in ITS view.
/// Editor B read at snap1 and wants "BBBB", which is at 6 in ITS view.
///
/// A must be carried across the commit it never saw: 16 -> 14. B must NOT be,
/// because it already saw it. Walking B from the batch minimum shifts it to 4,
/// where it eats two dots and half of "BBBB".
#[test]
fn a_member_with_a_newer_snapshot_is_not_rebased_twice() {
    let (d, path) = db("newer");
    d.query(
        "INSERT INTO block (id, body) VALUES (1, $1)",
        &[Value::Text("AAAA....BBBB....CCCC".into())],
    )
    .unwrap();

    let snap0 = d.snapshot_txn();

    // An independent commit that shortens the value ahead of both editors.
    //
    // It goes through a SESSION on purpose: only that path runs `widen_guard`,
    // and therefore only that path publishes an exact byte range and length
    // delta for later edits to be carried across. The same statement in
    // autocommit publishes RANGE_ANY and every pending sub-edit collides with
    // it — fail-safe, but it would make this test pass for the wrong reason.
    {
        let sql = "UPDATE block SET body = splice(body, $1, $2, $3) WHERE id = $4";
        let p = [
            Value::Int(0),
            Value::Int(4),
            Value::Text("aa".into()),
            Value::Int(1),
        ];
        let mut s = d.begin_guarded_with(snap0, &[(sql, &p[..])]).unwrap();
        s.query(sql, &p).unwrap();
        s.commit().unwrap();
    }
    assert_eq!(body(&d), "aa....BBBB....CCCC", "setup is not what the test assumes");
    let snap1 = d.snapshot_txn();
    assert!(snap1 > snap0, "the setup commit did not advance the snapshot");

    let subs = [
        // Read at snap0: "CCCC" sits at 16 in the ORIGINAL string.
        Submission { editor: 1, seq: 1, snap: snap0, key: 1, at: 16, remove: 4, insert: "cc".into() },
        // Read at snap1: "BBBB" sits at 6 in the SHORTENED string.
        Submission { editor: 2, seq: 2, snap: snap1, key: 1, at: 6, remove: 4, insert: "bb".into() },
    ];
    let out = d.submit_batch("block", "body", &subs).unwrap();

    assert!(
        out.iter().all(|v| v.is_committed()),
        "both edits are disjoint and should have landed: {out:?}"
    );
    assert_eq!(
        body(&d),
        "aa....bb....cc",
        "a member was rebased against a commit it had already seen — its offset \
         was shifted twice, so the splice landed on bytes its author never saw"
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}
