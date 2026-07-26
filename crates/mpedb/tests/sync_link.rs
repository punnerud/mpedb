//! **mpedb ⇄ mpedb sync (#157).**
//!
//! Every test here asserts on **convergence**, not on a return value. A sync
//! that reports "12 rows applied" and leaves the two databases disagreeing has
//! done worse than nothing, and `SyncReport` cannot tell you which happened —
//! `sync::fingerprint` can.

use std::collections::BTreeMap;

use mpedb::collab::{EditVerdict, EditVerdictExt as _, Submission};
use mpedb::sync::{fingerprint, Resolve, Role, SyncLink};
use mpedb::{Config, Database, ExecResult, Value};

const TABLES: [&str; 1] = ["doc"];

fn toml_for(path: &std::path::Path, role: &str) -> String {
    format!(
        r#"
[database]
path = "{}"
size_mb = 32
max_readers = 16

[sync]
role = "{role}"

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
  name = "owner"
  type = "int64"
"#,
        path.display()
    )
}

fn node(tag: &str, role: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-sync-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = Database::open_with_config(Config::from_toml_str(&toml_for(&path, role)).unwrap())
        .unwrap();
    (d, path)
}

fn put(d: &Database, id: i64, body: &str, owner: i64) {
    d.query(
        "INSERT OR REPLACE INTO doc (id, body, owner) VALUES ($1, $2, $3)",
        &[Value::Int(id), Value::Text(body.into()), Value::Int(owner)],
    )
    .unwrap();
}

fn body_of(d: &Database, id: i64) -> Option<String> {
    match d
        .query("SELECT body FROM doc WHERE id = $1", &[Value::Int(id)])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows.first().map(|r| match &r[0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }),
        other => panic!("{other:?}"),
    }
}

fn fp(d: &Database) -> BTreeMap<String, (u64, u64)> {
    fingerprint(d, &TABLES).unwrap()
}

// ---------------------------------------------------------------------------

/// The role is read from config, and it is the ONLY thing that changes.
#[test]
fn the_role_is_config_and_nothing_else_moves() {
    let (a, pa) = node("role-a", "standalone");
    let (b, pb) = node("role-b", "replica");
    let (c, pc) = node("role-c", "authority");

    assert_eq!(a.role(), Role::Standalone);
    assert_eq!(b.role(), Role::Replica);
    assert_eq!(c.role(), Role::Authority);

    // The same statements, the same answers, in all three roles.
    for d in [&a, &b, &c] {
        put(d, 1, "hello", 7);
        assert_eq!(body_of(d, 1).as_deref(), Some("hello"));
    }

    drop(a);
    drop(b);
    drop(c);
    for p in [pa, pb, pc] {
        let _ = std::fs::remove_file(p);
    }
}

/// An unknown role is refused **by name**, listing what is valid — the model
/// language's rule, applied here because a typo in a deployment knob must not
/// silently mean "standalone".
#[test]
fn an_unknown_role_is_refused_by_name() {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-sync-badrole-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let msg = match Database::open_with_config(
        Config::from_toml_str(&toml_for(&path, "primary")).unwrap(),
    ) {
        Ok(_) => panic!("an unknown sync role opened silently"),
        Err(e) => format!("{e}"),
    };
    assert!(msg.contains("primary"), "the refusal does not name the bad value: {msg}");
    assert!(
        msg.contains("standalone") && msg.contains("replica") && msg.contains("authority"),
        "the refusal does not list the valid values: {msg}"
    );
    let _ = std::fs::remove_file(&path);
}

/// **A replica's local commit is `Provisional`, and a standalone's is not.**
///
/// Pinned both ways on purpose: a test that only checks the replica would pass
/// just as well if every instance returned `Provisional`, which would make the
/// verdict meaningless.
#[test]
fn a_replica_commits_provisionally_and_a_standalone_does_not() {
    let (r, pr) = node("prov-r", "replica");
    let (s, ps) = node("prov-s", "standalone");

    for d in [&r, &s] {
        put(d, 1, "0123456789abcdef", 1);
    }

    let sub = |d: &Database| Submission {
        editor: 1,
        seq: 1,
        snap: d.snapshot_txn(),
        key: 1,
        at: 0,
        remove: 4,
        insert: "AAAA".into(),
    };

    let vr = r.submit_batch("doc", "body", &[sub(&r)]).unwrap();
    assert!(
        matches!(vr[0], EditVerdict::Provisional { .. }),
        "a replica claimed an authority's word: {vr:?}"
    );
    assert!(!vr[0].is_committed(), "Provisional must not read as committed");

    let vs = s.submit_batch("doc", "body", &[sub(&s)]).unwrap();
    assert_eq!(
        vs[0],
        EditVerdict::Committed,
        "a standalone has nobody to ask, so its commit IS authoritative: {vs:?}"
    );

    // Either way the edit landed locally — Provisional is about who confirmed
    // it, never about whether it happened.
    for d in [&r, &s] {
        assert_eq!(body_of(d, 1).as_deref(), Some("AAAA456789abcdef"));
    }

    drop(r);
    drop(s);
    let _ = std::fs::remove_file(&pr);
    let _ = std::fs::remove_file(&ps);
}

/// **Two replicas of one upstream converge.** The load-bearing test: each makes
/// disjoint local changes, both sync, and all three databases end byte-identical.
#[test]
fn two_replicas_and_an_upstream_converge() {
    let (up, pu) = node("conv-up", "authority");
    let (r1, p1) = node("conv-r1", "replica");
    let (r2, p2) = node("conv-r2", "replica");

    let l1 = SyncLink::new(&r1, &up, 1);
    let l2 = SyncLink::new(&r2, &up, 2);
    l1.enable(&TABLES).unwrap();
    l2.enable(&TABLES).unwrap();

    put(&r1, 1, "from-one", 1);
    put(&r1, 2, "also-one", 1);
    put(&r2, 10, "from-two", 2);

    // Each pushes its own, then both pull everything.
    l1.sync(&TABLES).unwrap();
    l2.sync(&TABLES).unwrap();
    l1.sync(&TABLES).unwrap();
    l2.sync(&TABLES).unwrap();

    assert_eq!(fp(&up), fp(&r1), "replica 1 diverged from the upstream");
    assert_eq!(fp(&up), fp(&r2), "replica 2 diverged from the upstream");
    assert_eq!(body_of(&r2, 1).as_deref(), Some("from-one"));
    assert_eq!(body_of(&r1, 10).as_deref(), Some("from-two"));

    drop(up);
    drop(r1);
    drop(r2);
    for p in [pu, p1, p2] {
        let _ = std::fs::remove_file(p);
    }
}

/// **A long offline period costs one row image per changed row, not one per
/// edit.** That is what the coalesced dirty set buys, and it is the property
/// that makes "offline for an hour" cheap rather than linear in typing.
#[test]
fn a_long_offline_replica_catches_up_on_coalesced_images() {
    let (up, pu) = node("off-up", "authority");
    let (r, pr) = node("off-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    // The replica is "offline": it does not sync while the upstream churns.
    // 200 writes over 10 rows.
    for round in 0..20 {
        for id in 0..10i64 {
            put(&up, id, &format!("v{round}-{id}"), id);
        }
    }
    assert_eq!(link.lag(&TABLES).unwrap(), 10, "lag must be CHANGED ROWS, not edits");

    let rep = link.pull(&TABLES).unwrap();
    assert_eq!(
        rep.upserts, 10,
        "catch-up replayed per-edit instead of per-row: {rep:?}"
    );
    assert_eq!(fp(&up), fp(&r), "an offline replica did not converge on catch-up");
    assert_eq!(link.lag(&TABLES).unwrap(), 0);

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **A real divergence is decided at PUSH, and counted.**
///
/// Conflict detection lives in push rather than pull because push is the moment
/// a local change is first *offered*: deciding at pull would mean discarding an
/// edit the upstream had never been shown.
#[test]
fn the_upstream_wins_a_row_conflict_and_it_is_counted() {
    let (up, pu) = node("conf-up", "authority");
    let (r, pr) = node("conf-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    put(&up, 1, "seed", 0);
    link.pull(&TABLES).unwrap();
    assert_eq!(body_of(&r, 1).as_deref(), Some("seed"));

    // Both edit row 1 while disconnected.
    put(&r, 1, "replica-version", 1);
    put(&up, 1, "upstream-version", 2);

    let rep = link.push(&TABLES).unwrap();
    assert_eq!(rep.conflicts, 1, "a real divergence was not reported: {rep:?}");
    assert_eq!(
        body_of(&up, 1).as_deref(),
        Some("upstream-version"),
        "the replica overwrote a value it should have lost to"
    );
    assert_eq!(
        body_of(&r, 1).as_deref(),
        Some("upstream-version"),
        "the replica kept showing a change it had already lost"
    );

    // And it must stay decided: a second sync changes nothing.
    link.sync(&TABLES).unwrap();
    assert_eq!(fp(&up), fp(&r));

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// `Resolve::LocalWins` — the other half of the choice. Still **counted**,
/// because an overwrite the caller cannot see is data loss with better manners.
#[test]
fn local_wins_overwrites_and_still_reports_the_conflict() {
    let (up, pu) = node("lw-up", "authority");
    let (r, pr) = node("lw-r", "replica");
    let link = SyncLink::new(&r, &up, 1).with_resolve(Resolve::LocalWins);
    link.enable(&TABLES).unwrap();

    put(&up, 1, "seed", 0);
    link.pull(&TABLES).unwrap();

    put(&r, 1, "the-field-work", 1);
    put(&up, 1, "someone-elses-touch", 2);

    let rep = link.push(&TABLES).unwrap();
    assert_eq!(rep.conflicts, 1, "LocalWins hid the conflict instead of reporting it");
    assert_eq!(
        body_of(&up, 1).as_deref(),
        Some("the-field-work"),
        "LocalWins did not actually win"
    );

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **Regression: a replica's newest edit must not lose to its own older one.**
///
/// Found by `G-rows` in the benchmark, not by a test: the conflict column read
/// 400 conflicts on 400 *disjoint* rows, which meant every pushed row was coming
/// back and being mistaken for somebody else's change. The same mistake also
/// threw away the replica's newer unpushed edit in favour of the value it had
/// pushed a moment earlier — a silent lost update.
///
/// The fix is the recorded base: a row the upstream still has at the agreed
/// image is a row nobody else touched, whoever put it there.
#[test]
fn a_replicas_own_echo_is_not_a_conflict() {
    let (up, pu) = node("echo-up", "authority");
    let (r, pr) = node("echo-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    put(&r, 1, "v1", 1);
    let first = link.push(&TABLES).unwrap();
    assert_eq!(first.conflicts, 0, "a first push conflicted with nothing: {first:?}");
    assert_eq!(body_of(&up, 1).as_deref(), Some("v1"));

    // The replica edits again before pulling its own change back.
    put(&r, 1, "v2", 2);
    let out = link.sync(&TABLES).unwrap();
    assert_eq!(
        out.pushed.conflicts, 0,
        "the replica's own echo was counted as somebody else's change: {out:?}"
    );
    assert_eq!(
        body_of(&up, 1).as_deref(),
        Some("v2"),
        "the newer local edit was replaced by the replica's OWN older pushed value"
    );
    assert_eq!(body_of(&r, 1).as_deref(), Some("v2"));
    assert_eq!(fp(&up), fp(&r));

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// A pull that finds nothing must not move the cursor or write anything —
/// otherwise a polling replica rewrites its own state forever.
#[test]
fn an_idle_sync_is_a_no_op() {
    let (up, pu) = node("idle-up", "authority");
    let (r, pr) = node("idle-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    put(&up, 1, "x", 0);
    let first = link.pull(&TABLES).unwrap();
    assert_eq!(first.upserts, 1);

    let second = link.pull(&TABLES).unwrap();
    assert_eq!(second.upserts, 0);
    assert_eq!(second.deletes, 0);
    assert_eq!(second.cursor, first.cursor, "an idle pull moved the cursor");

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **Cell-plane sub-edits from separate replicas merge instead of clobbering.**
///
/// This is the difference between the two planes stated as a test: a row-plane
/// push of the whole `body` would make the second writer's version replace the
/// first's. Sub-edits carrying `(seq, editor)` compose.
#[test]
fn sub_edits_from_two_replicas_merge_at_the_authority() {
    let (up, pu) = node("cell-up", "authority");
    let (r1, p1) = node("cell-r1", "replica");
    let (r2, p2) = node("cell-r2", "replica");
    let l1 = SyncLink::new(&r1, &up, 1);
    let l2 = SyncLink::new(&r2, &up, 2);
    l1.enable(&TABLES).unwrap();
    l2.enable(&TABLES).unwrap();

    put(&up, 1, "AAAA....BBBB", 0);
    l1.pull(&TABLES).unwrap();
    l2.pull(&TABLES).unwrap();

    let snap = up.snapshot_txn();
    // Each replica edits its own end of the value, offline, and stamps its own
    // counter at the moment its editor acted.
    let e1 = Submission { editor: 1, seq: 5, snap, key: 1, at: 0, remove: 4, insert: "aa".into() };
    let e2 = Submission { editor: 2, seq: 6, snap, key: 1, at: 8, remove: 4, insert: "bb".into() };

    let v1 = l1.push_edits("doc", "body", &[e1]).unwrap();
    let v2 = l2.push_edits("doc", "body", &[e2]).unwrap();
    assert!(v1[0].is_committed(), "{v1:?}");
    assert!(v2[0].is_committed(), "{v2:?}");

    assert_eq!(
        body_of(&up, 1).as_deref(),
        Some("aa....bb"),
        "a sub-edit clobbered the other replica's instead of merging with it"
    );

    drop(up);
    drop(r1);
    drop(r2);
    for p in [pu, p1, p2] {
        let _ = std::fs::remove_file(p);
    }
}

/// GC must not strand a replica that is still behind. The watermark is the
/// caller's to choose, and this pins what happens when it is chosen correctly
/// (below the slowest cursor) — the entries the slow replica still needs stay.
#[test]
fn gc_below_the_slowest_cursor_keeps_what_is_still_needed() {
    let (up, pu) = node("gc-up", "authority");
    let (fast, pf) = node("gc-fast", "replica");
    let (slow, ps) = node("gc-slow", "replica");
    let lf = SyncLink::new(&fast, &up, 1);
    let ls = SyncLink::new(&slow, &up, 2);
    lf.enable(&TABLES).unwrap();
    ls.enable(&TABLES).unwrap();

    put(&up, 1, "one", 0);
    let after_first = lf.pull(&TABLES).unwrap().cursor;
    put(&up, 2, "two", 0);
    lf.pull(&TABLES).unwrap();

    // `slow` has seen nothing, so its cursor is 0 — the watermark must be 0 too.
    assert_eq!(ls.lag(&TABLES).unwrap(), 2);
    let dropped = ls.gc_upstream(&TABLES, 0).unwrap();
    assert_eq!(dropped, 0, "gc at the slowest cursor threw away live changes");
    assert_eq!(ls.lag(&TABLES).unwrap(), 2);

    // Once slow has caught up, trimming to the older watermark is safe.
    ls.pull(&TABLES).unwrap();
    assert_eq!(fp(&up), fp(&slow));
    let dropped = ls.gc_upstream(&TABLES, after_first).unwrap();
    assert!(dropped >= 1, "gc below every live cursor dropped nothing");

    drop(up);
    drop(fast);
    drop(slow);
    for p in [pu, pf, ps] {
        let _ = std::fs::remove_file(p);
    }
}

/// A delete propagates. Easy to forget, and an upsert-only sync looks perfect
/// until a row that should be gone stays.
#[test]
fn deletes_propagate() {
    let (up, pu) = node("del-up", "authority");
    let (r, pr) = node("del-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    put(&up, 1, "here", 0);
    put(&up, 2, "stays", 0);
    link.pull(&TABLES).unwrap();
    assert_eq!(fp(&up), fp(&r));

    up.query("DELETE FROM doc WHERE id = 1", &[]).unwrap();
    let rep = link.pull(&TABLES).unwrap();
    assert_eq!(rep.deletes, 1, "{rep:?}");
    assert!(body_of(&r, 1).is_none(), "a deleted row survived the sync");
    assert_eq!(fp(&up), fp(&r));

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

// ---------------------------------------------------------------------------
// Regressions from the adversarial pass (#157). Both were found by attacking
// the API, not by reading it, and both were silent.
// ---------------------------------------------------------------------------

fn two_table_toml(path: &std::path::Path, role: &str) -> String {
    format!(
        r#"
[database]
path = "{}"
size_mb = 32
max_readers = 16

[sync]
role = "{role}"

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
  name = "owner"
  type = "int64"

[[table]]
name = "note"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "text"
  type = "text"

[[table]]
name = "uniq"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "slot"
  type = "int64"
  unique = true
"#,
        path.display()
    )
}

fn wide_node(tag: &str, role: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-syncreg-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = Database::open_with_config(
        Config::from_toml_str(&two_table_toml(&path, role)).unwrap(),
    )
    .unwrap();
    (d, path)
}

/// **Syncing one table must not skip another's pending changes.**
///
/// The cursor used to be per link. `pull(["doc"])` advanced it past every
/// pending `note` entry, and the next `pull(["note"])` then skipped them
/// forever — silently, with `lag()` reporting 0 because it consulted the same
/// poisoned cursor. Syncing a hot table and a cold table on different cadences
/// is an ordinary thing to want.
#[test]
fn pulling_one_table_does_not_skip_another() {
    let (up, pu) = wide_node("tbl-up", "authority");
    let (r, pr) = wide_node("tbl-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&["doc", "note"]).unwrap();

    // `note` gets the EARLIER txn, so a shared cursor jumps past it.
    up.query(
        "INSERT INTO note (id, text) VALUES ($1, $2)",
        &[Value::Int(1), Value::Text("note one".into())],
    )
    .unwrap();
    up.query(
        "INSERT INTO doc (id, body, owner) VALUES ($1, $2, $3)",
        &[Value::Int(1), Value::Text("doc one".into()), Value::Int(0)],
    )
    .unwrap();

    link.pull(&["doc"]).unwrap();
    assert_eq!(
        link.lag(&["note"]).unwrap(),
        1,
        "lag consulted a cursor another table's pull had already advanced"
    );

    let rep = link.pull(&["note"]).unwrap();
    assert_eq!(rep.upserts, 1, "the other table's change was skipped: {rep:?}");
    match r.query("SELECT text FROM note WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 1, "note row never arrived"),
        other => panic!("{other:?}"),
    }

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **Permuting a UNIQUE column must not wedge the link.**
///
/// Swapping two rows' unique values is legal at both endpoints and illegal in
/// between. Applying row-at-a-time as delete+insert walked straight through the
/// illegal state, so every pull failed with `UniqueViolation`, rolled back, and
/// left the cursor where it was — wedged permanently rather than failing once.
/// Deleting every affected key before inserting any dissolves the swap.
#[test]
fn a_unique_column_permutation_still_converges() {
    let (up, pu) = wide_node("uniq-up", "authority");
    let (r, pr) = wide_node("uniq-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&["uniq"]).unwrap();

    let set = |d: &Database, id: i64, slot: i64| {
        d.query(
            "INSERT OR REPLACE INTO uniq (id, slot) VALUES ($1, $2)",
            &[Value::Int(id), Value::Int(slot)],
        )
        .unwrap();
    };
    set(&up, 1, 1);
    set(&up, 2, 2);
    link.pull(&["uniq"]).unwrap();
    assert_eq!(fingerprint(&up, &["uniq"]).unwrap(), fingerprint(&r, &["uniq"]).unwrap());

    // Swap, via a spare slot so the upstream itself stays legal throughout.
    set(&up, 1, 99);
    set(&up, 2, 1);
    set(&up, 1, 2);

    let rep = link
        .pull(&["uniq"])
        .expect("a legal permutation must not fail the whole pull");
    assert!(rep.upserts >= 1, "{rep:?}");
    assert_eq!(
        fingerprint(&up, &["uniq"]).unwrap(),
        fingerprint(&r, &["uniq"]).unwrap(),
        "a UNIQUE permutation left the replica behind"
    );

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **`lag` must not count the replica's own echo.**
///
/// After a push that left both ends byte-identical, the pushed rows are past
/// the pull cursor and were counted as being behind — so a write-heavy replica
/// showed "syncing 7…" on a link with nothing to do, which is exactly wrong for
/// the one caller the number exists for.
#[test]
fn lag_does_not_count_our_own_push() {
    let (up, pu) = node("lag-up", "authority");
    let (r, pr) = node("lag-r", "replica");
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    for id in 0..7i64 {
        put(&r, id, "local work", 1);
    }
    assert_eq!(link.lag(&TABLES).unwrap(), 0, "local work is not lag");

    link.push(&TABLES).unwrap();
    assert_eq!(
        link.lag(&TABLES).unwrap(),
        0,
        "the replica's own push was reported back to it as lag"
    );

    // A real upstream change still shows.
    put(&up, 99, "from the authority", 0);
    assert_eq!(link.lag(&TABLES).unwrap(), 1);

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}

/// **`enable` is not retroactive, and `seed` is the fix.**
///
/// Rows that existed before capture was turned on are in no change log, so they
/// replicate never — silently, with `lag()` reading 0. Both reviewers hit this,
/// and neither could express "point a replica at a database that already has
/// data" until `seed` existed.
#[test]
fn seed_bootstraps_a_replica_onto_an_existing_database() {
    let (up, pu) = node("seed-up", "authority");
    let (r, pr) = node("seed-r", "replica");

    // Populate FIRST — the order a real migration arrives in.
    for id in 0..25i64 {
        put(&up, id, &format!("pre-existing-{id}"), 0);
    }

    let link = SyncLink::new(&r, &up, 1);
    link.enable(&TABLES).unwrap();

    // Without the seed there is genuinely nothing to find, and nothing says so.
    assert_eq!(link.lag(&TABLES).unwrap(), 0);
    assert_eq!(link.pull(&TABLES).unwrap().upserts, 0);

    let rep = link.seed(&TABLES).unwrap();
    assert_eq!(rep.upserts, 25, "{rep:?}");
    assert_eq!(fp(&up), fp(&r), "seed did not bring the replica up to date");

    // And normal sync carries on from there, without replaying the copy.
    put(&up, 100, "after the seed", 0);
    let out = link.sync(&TABLES).unwrap();
    assert_eq!(out.pulled.upserts, 1, "the seed's cursor replayed history: {out:?}");
    assert_eq!(fp(&up), fp(&r));

    drop(up);
    drop(r);
    let _ = std::fs::remove_file(&pu);
    let _ = std::fs::remove_file(&pr);
}
