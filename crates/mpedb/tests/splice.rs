//! **`splice()` — the sub-edit as an ordinary scalar (#151).**
//!
//! The load-bearing test is `two_disjoint_splices_of_one_cell_both_land`. A
//! test that only proved `splice()` computes the right string would pass just
//! as well against `substr() || … || substr()`, and would say nothing about the
//! thing this exists for: that a sub-edit composes where a whole-value write
//! loses one of the two.

use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-splice-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "doc"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "text"

  [[table.column]]
  name = "raw"
  type = "blob"
"#,
        path.display()
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (d, path)
}

fn body(d: &Database) -> String {
    match d.query("SELECT body FROM doc WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seed(d: &Database, s: &str) {
    d.query(
        "INSERT INTO doc (id, body, raw) VALUES (1, $1, x'00')",
        &[Value::Text(s.into())],
    )
    .unwrap();
}

/// Insert, delete, replace — the three shapes, as a value expression.
#[test]
fn the_three_shapes() {
    let (d, path) = db("shapes");
    seed(&d, "hello world");

    // insert at 5
    d.query("UPDATE doc SET body = splice(body, 5, 0, ',') WHERE id = 1", &[]).unwrap();
    assert_eq!(body(&d), "hello, world");
    // delete the comma again
    d.query("UPDATE doc SET body = splice(body, 5, 1, '') WHERE id = 1", &[]).unwrap();
    assert_eq!(body(&d), "hello world");
    // replace "world" with "there"
    d.query("UPDATE doc SET body = splice(body, 6, 5, 'there') WHERE id = 1", &[]).unwrap();
    assert_eq!(body(&d), "hello there");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The whole point.** Two editors splice disjoint ranges of the SAME cell,
/// each against the version it read, and both edits survive. The control is in
/// the same test: doing it as whole-value writes loses one.
#[test]
fn two_disjoint_splices_of_one_cell_both_land() {
    let (d, path) = db("disjoint");
    seed(&d, "AAAA....BBBB");

    // Both read the same version.
    let before = body(&d);
    assert_eq!(before, "AAAA....BBBB");

    // Editor 1 rewrites the head, editor 2 the tail — as sub-edits.
    let mut a = d.begin().unwrap();
    a.query("UPDATE doc SET body = splice(body, 0, 4, 'xxxx') WHERE id = 1", &[]).unwrap();
    a.commit().unwrap();

    let mut b = d.begin().unwrap();
    b.query("UPDATE doc SET body = splice(body, 8, 4, 'yyyy') WHERE id = 1", &[]).unwrap();
    b.commit().unwrap();

    assert_eq!(
        body(&d),
        "xxxx....yyyy",
        "a sub-edit did not compose — this is the entire reason splice() exists"
    );

    // The control: the same two intentions expressed as whole-value writes,
    // each computed from the version both editors read. One is lost.
    seed_second(&d);
    let seen = "AAAA....BBBB";
    let mut a = d.begin().unwrap();
    a.query(
        "UPDATE doc SET body = $1 WHERE id = 2",
        &[Value::Text(format!("xxxx{}", &seen[4..]))],
    )
    .unwrap();
    a.commit().unwrap();
    let mut b = d.begin().unwrap();
    b.query(
        "UPDATE doc SET body = $1 WHERE id = 2",
        &[Value::Text(format!("{}yyyy", &seen[..8]))],
    )
    .unwrap();
    b.commit().unwrap();
    let whole = match d.query("SELECT body FROM doc WHERE id = 2", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(
        whole, "AAAA....yyyy",
        "the control is supposed to LOSE the first edit — if it did not, this test \
         proves nothing about splice()"
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

fn seed_second(d: &Database) {
    d.query(
        "INSERT INTO doc (id, body, raw) VALUES (2, $1, x'00')",
        &[Value::Text("AAAA....BBBB".into())],
    )
    .unwrap();
}

/// **Strict, never clamping.** A range past the end is a stale offset — a wrong
/// question — and answering it by clamping would silently mangle the value.
#[test]
fn a_range_past_the_end_is_refused() {
    let (d, path) = db("oob");
    seed(&d, "short");
    for (at, rem) in [(10i64, 0i64), (0, 99), (4, 5)] {
        let e = d
            .query(
                "UPDATE doc SET body = splice(body, $1, $2, 'x') WHERE id = 1",
                &[Value::Int(at), Value::Int(rem)],
            )
            .unwrap_err();
        assert!(
            format!("{e}").contains("outside a value"),
            "at={at} remove={rem}: refusal did not say why: {e}"
        );
    }
    assert_eq!(body(&d), "short", "a refused splice must not have written");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **A cut inside a multi-byte character is refused**, because completing it
/// would produce invalid UTF-8 — worse than an error, and the kind of thing
/// that is only discovered much later.
#[test]
fn a_cut_inside_a_character_is_refused() {
    let (d, path) = db("utf8");
    seed(&d, "æøå"); // 6 bytes, boundaries at 0, 2, 4, 6

    let e = d
        .query("UPDATE doc SET body = splice(body, 1, 1, 'x') WHERE id = 1", &[])
        .unwrap_err();
    assert!(
        format!("{e}").contains("multi-byte character"),
        "refusal did not say why: {e}"
    );

    // The aligned edit of the same value works, so this is about the boundary
    // and not about non-ASCII text being refused wholesale.
    d.query("UPDATE doc SET body = splice(body, 2, 2, 'x') WHERE id = 1", &[]).unwrap();
    assert_eq!(body(&d), "æxå");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Blobs splice too, on raw bytes, with no boundary rule to obey.
#[test]
fn blobs_splice_on_raw_bytes() {
    let (d, path) = db("blob");
    d.query(
        "INSERT INTO doc (id, body, raw) VALUES (1, '', $1)",
        &[Value::Blob(vec![1, 2, 3, 4])],
    )
    .unwrap();
    d.query("UPDATE doc SET raw = splice(raw, 1, 2, $1) WHERE id = 1", &[Value::Blob(vec![9])])
        .unwrap();
    match d.query("SELECT raw FROM doc WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Blob(vec![1, 9, 4]));
        }
        other => panic!("{other:?}"),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// NULL propagates, like every other scalar here.
#[test]
fn null_propagates() {
    let (d, path) = db("null");
    seed(&d, "abc");
    match d.query("SELECT splice(body, NULL, 0, 'x') FROM doc WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Null),
        other => panic!("{other:?}"),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A splice of a non-text, non-blob column is refused at COMPILE time, not at
/// the first row — the rigid-schema rule the rest of the engine follows.
#[test]
fn splicing_an_integer_column_is_refused_at_compile_time() {
    let (d, path) = db("badtype");
    let e = d.prepare("UPDATE doc SET body = splice(id, 0, 1, 'x') WHERE id = 1").unwrap_err();
    assert!(
        format!("{e}").contains("must be text or blob"),
        "refusal did not name the problem: {e}"
    );
    drop(d);
    let _ = std::fs::remove_file(&path);
}

// ------------------------------------------------- the guard's fifth dimension

/// **#151: the range is part of the request, so the guard can use it.** Two
/// editors splice disjoint ranges of the SAME cell, both declaring what they
/// will touch, and both commit. Before the byte range was a dimension the guard
/// saw two writes to one (table, key, column) and refused the second — even
/// though `splice()` would have composed them.
///
/// The later editor goes first on purpose: an edit that begins after mine ends
/// can move neither my bytes nor my offsets, which is the direction the rule is
/// written in.
#[test]
fn two_declared_splices_of_disjoint_ranges_both_commit() {
    let (d, path) = db("guard-disjoint");
    seed(&d, "AAAA....BBBB");
    let snap = d.snapshot_txn();

    let tail = [Value::Int(8), Value::Int(4), Value::Text("yyyy".into()), Value::Int(1)];
    let head = [Value::Int(0), Value::Int(4), Value::Text("xxxx".into()), Value::Int(1)];
    let sql = "UPDATE doc SET body = splice(body, $1, $2, $3) WHERE id = $4";

    let mut b = d.begin_guarded_with(snap, &[(sql, &tail[..])]).unwrap();
    b.query(sql, &tail).unwrap();
    b.commit().expect("the tail edit should commit");

    let mut a = d.begin_guarded_with(snap, &[(sql, &head[..])]).unwrap();
    a.query(sql, &head).unwrap();
    a.commit().expect(
        "an edit at bytes 0..4 was refused because another editor changed bytes 8..12 of the \
         same cell — the guard is not using the range the request declared",
    );

    assert_eq!(body(&d), "xxxx....yyyy", "both sub-edits should be in the value");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same range still conflicts. Without this, "compare ranges" could be
/// implemented as "never overlap" and the test above would not notice.
#[test]
fn two_splices_of_the_same_range_still_conflict() {
    let (d, path) = db("guard-same");
    seed(&d, "AAAA....BBBB");
    let snap = d.snapshot_txn();
    let sql = "UPDATE doc SET body = splice(body, $1, $2, $3) WHERE id = $4";
    let p = [Value::Int(0), Value::Int(4), Value::Text("xxxx".into()), Value::Int(1)];

    let mut a = d.begin_guarded_with(snap, &[(sql, &p[..])]).unwrap();
    a.query(sql, &p).unwrap();
    a.commit().unwrap();

    let q = [Value::Int(0), Value::Int(4), Value::Text("zzzz".into()), Value::Int(1)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &q[..])]).unwrap();
    b.query(sql, &q).unwrap();
    match b.commit() {
        Err(mpedb::Error::WriteConflict) => {}
        other => panic!("expected WriteConflict on the SAME byte range, got {other:?}"),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **An earlier edit invalidates a later one, and that is deliberate.** Here
/// the head edit is length-PRESERVING, so nothing actually shifted and
/// `splice()` itself is perfectly happy — the guard is the one that refuses,
/// on the rule that a commit beginning before my range *may* have moved the
/// bytes my offset was computed against.
///
/// Conservative, and knowingly so: distinguishing "moved" from "did not move"
/// needs the length delta and then the offsets rebased, which is a per-cell
/// edit history the engine does not keep (DESIGN-COLLAB §3). Refusing returns
/// `Lost`, which is exactly the signal a client needs to resubmit.
#[test]
fn an_edit_before_mine_invalidates_my_offsets() {
    let (d, path) = db("guard-shift");
    seed(&d, "AAAA....BBBB");
    let snap = d.snapshot_txn();
    let sql = "UPDATE doc SET body = splice(body, $1, $2, $3) WHERE id = $4";

    // The HEAD commits first this time, replacing 4 bytes with 4.
    let head = [Value::Int(0), Value::Int(4), Value::Text("xxxx".into()), Value::Int(1)];
    let mut a = d.begin_guarded_with(snap, &[(sql, &head[..])]).unwrap();
    a.query(sql, &head).unwrap();
    a.commit().unwrap();

    let tail = [Value::Int(8), Value::Int(4), Value::Text("yyyy".into()), Value::Int(1)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &tail[..])]).unwrap();
    b.query(sql, &tail).unwrap();
    match b.commit() {
        Err(mpedb::Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — an edit that began before mine may have \
             shifted the bytes my offset was computed against, and the rule is deliberately \
             conservative about that"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// And when the earlier edit really did shift things, **`splice()` catches it
/// before the guard ever runs** — at execution, because the offset now points
/// past the end of a value that got shorter. Two independent layers refuse the
/// same stale offset, which is the redundancy worth having.
#[test]
fn a_shifted_offset_is_refused_by_splice_itself() {
    let (d, path) = db("guard-shrunk");
    seed(&d, "AAAA....BBBB");
    let sql = "UPDATE doc SET body = splice(body, $1, $2, $3) WHERE id = $4";

    // Shrink the value from 12 bytes to 9.
    let head = [Value::Int(0), Value::Int(4), Value::Text("x".into()), Value::Int(1)];
    d.query(sql, &head).unwrap();

    let tail = [Value::Int(8), Value::Int(4), Value::Text("yyyy".into()), Value::Int(1)];
    let e = d.query(sql, &tail).unwrap_err();
    assert!(
        format!("{e}").contains("outside a value"),
        "a stale offset past the end should be refused by splice(), got: {e}"
    );
    assert_eq!(body(&d), "x....BBBB", "a refused splice must not have written");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A whole-value write names no range and must therefore conflict with every
/// sub-edit of that cell — the fail-safe reading of "I could have touched
/// anything".
#[test]
fn a_whole_value_write_conflicts_with_every_sub_edit() {
    let (d, path) = db("guard-whole");
    seed(&d, "AAAA....BBBB");
    let snap = d.snapshot_txn();

    let whole = "UPDATE doc SET body = $1 WHERE id = $2";
    let wp = [Value::Text("replaced".into()), Value::Int(1)];
    let mut a = d.begin_guarded_with(snap, &[(whole, &wp[..])]).unwrap();
    a.query(whole, &wp).unwrap();
    a.commit().unwrap();

    let sql = "UPDATE doc SET body = splice(body, $1, $2, $3) WHERE id = $4";
    let tail = [Value::Int(0), Value::Int(1), Value::Text("y".into()), Value::Int(1)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &tail[..])]).unwrap();
    b.query(sql, &tail).unwrap();
    match b.commit() {
        Err(mpedb::Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — a write that could not name a range must \
             be taken to have rewritten the whole value"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}
