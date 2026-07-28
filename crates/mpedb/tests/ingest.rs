//! Ingest (design/DESIGN-INGEST.md): the receipt protocol.
//!
//! The contract under test: a dump finds inserts, updates AND deletes; a
//! delta cannot see deletes and says so; a cursor candidate is VERIFIED
//! against every dump and named when it lies; the policy decides or the
//! conflict is recorded — never a silent overwrite; and a declaration that
//! could not catch every change is refused at define time.

use mpedb::ingest::{IngestSpec, Policy};
use mpedb::ingest_run::Mode;
use mpedb::{Config, Database, ExecResult, Value};

struct Scratch(String);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn db(tag: &str) -> (Database, Scratch) {
    let path = format!(
        "{}/ingest-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\ndurability = \"none\"\n"
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Scratch(path),
    )
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ints(d: &Database, sql: &str) -> Vec<i64> {
    rows(d.query(sql, &[]).unwrap())
        .into_iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        })
        .collect()
}

const SPEC: &str = r#"
[source]
name = "crm"
policy = "source"

[[source.budget]]
profile = "work"
window_secs = 300
calls = 100
bytes = 1000000

[[source.edge]]
name = "cases_delta"
kind = "root"
table = "cases"
strategy = "delta"
cursor = "updated_at"
overlap_secs = 60
cost_calls = 1

[[source.edge]]
name = "cases_full"
kind = "root"
table = "cases"
strategy = "dump"
cost_calls = 10
"#;

fn cols() -> Vec<String> {
    ["id", "subject", "updated_at"].iter().map(|s| s.to_string()).collect()
}

fn row(id: i64, subject: &str, at: i64) -> Vec<Value> {
    vec![Value::Int(id), Value::Text(subject.into()), Value::Int(at)]
}

fn seeded(tag: &str) -> (Database, Scratch) {
    let (d, s) = db(tag);
    d.query(
        "CREATE TABLE cases (id INTEGER PRIMARY KEY, subject ANY, updated_at ANY)",
        &[],
    )
    .unwrap();
    d.ingest_define(SPEC).unwrap();
    (d, s)
}

/// The whole story of a dump: rows arrive, rows change, rows vanish — and
/// only the dump can see the last one.
#[test]
fn a_dump_finds_inserts_updates_and_deletes() {
    let (d, _s) = seeded("dump");
    let c = cols();

    let r = d
        .ingest_dump("crm", "cases", &c, &[row(1, "a", 10), row(2, "b", 20)], 1, 500)
        .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (2, 0, 0), "{r:?}");
    assert!(r.complete);

    // Same rows again: nothing moves, and the receipt says so.
    let r = d
        .ingest_dump("crm", "cases", &c, &[row(1, "a", 10), row(2, "b", 20)], 1, 500)
        .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted, r.unchanged), (0, 0, 0, 2));

    // One edited, one gone, one new.
    let r = d
        .ingest_dump("crm", "cases", &c, &[row(1, "a2", 30), row(3, "c", 31)], 1, 500)
        .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (1, 1, 1), "{r:?}");
    assert_eq!(ints(&d, "SELECT id FROM cases ORDER BY id"), vec![1, 3]);
    assert_eq!(
        rows(d.query("SELECT subject FROM cases WHERE id = 1", &[]).unwrap())[0][0],
        Value::Text("a2".into())
    );
}

/// A delta cannot see a delete — nothing in "rows that changed" says a row
/// is gone. The receipt records that rather than implying coverage.
#[test]
fn a_delta_cannot_see_deletes_and_says_so() {
    let (d, _s) = seeded("delta");
    let c = cols();
    d.ingest_dump("crm", "cases", &c, &[row(1, "a", 10), row(2, "b", 20)], 1, 500)
        .unwrap();

    // The source deleted row 2, but a delta only ever presents survivors.
    let r = d.ingest_delta("crm", "cases", &c, &[row(1, "a2", 30)], 1, 100).unwrap();
    assert_eq!((r.updated, r.deleted), (1, 0));
    assert!(!r.complete, "a delta must not claim completeness");
    assert!(r.note().contains("cannot see deletes"), "{}", r.note());
    assert_eq!(ints(&d, "SELECT count(*) FROM cases"), vec![2], "row 2 still stands");

    // Only the dump finds it.
    let r = d.ingest_dump("crm", "cases", &c, &[row(1, "a2", 30)], 1, 500).unwrap();
    assert_eq!(r.deleted, 1);
    assert_eq!(ints(&d, "SELECT count(*) FROM cases"), vec![1]);
}

/// THE point of the design. `updated_at` is set on insert and forgotten on
/// update — the commonest lie in the wild. A delta over it silently loses
/// every edit; the dump proves it and names the verdict.
#[test]
fn the_dump_catches_a_lying_updated_at() {
    let (d, _s) = seeded("lying");
    let c = cols();
    d.ingest_dump("crm", "cases", &c, &[row(1, "a", 10), row(2, "b", 20)], 1, 500)
        .unwrap();

    // A delta moves the watermark to 20.
    d.ingest_delta("crm", "cases", &c, &[row(2, "b", 20)], 1, 100).unwrap();
    let st = d.ingest_state("crm").unwrap();
    let delta = st.iter().find(|(n, _, _)| n == "cases_delta").unwrap();
    assert_eq!(delta.1.watermark, Value::Int(20));
    assert_eq!(delta.1.cursor_state, "unknown", "no dump has judged it yet");

    // Now the source edits row 1 WITHOUT touching updated_at — the lie.
    let r = d
        .ingest_dump("crm", "cases", &c, &[row(1, "EDITED", 10), row(2, "b", 20)], 1, 500)
        .unwrap();
    assert_eq!(r.updated, 1);
    assert_eq!(r.missed, 1, "the cursor would have LOST that row: {r:?}");
    assert_eq!(r.cursor_state, "unsafe");
    assert!(r.cursor_note().contains("UNSAFE"), "{}", r.cursor_note());

    // The verdict is recorded against BOTH the dump edge and the delta edge
    // it judges — the delta is the one that would have done the losing.
    let st = d.ingest_state("crm").unwrap();
    for name in ["cases_delta", "cases_full"] {
        let e = st.iter().find(|(n, _, _)| n == name).unwrap();
        assert_eq!(e.1.cursor_state, "unsafe", "{name}");
        assert_eq!(e.1.missed, 1, "{name}");
    }
}

/// A cursor that behaves earns `safe` — and keeps earning it, because
/// every dump re-tests it.
#[test]
fn an_honest_cursor_is_verified_safe() {
    let (d, _s) = seeded("honest");
    let c = cols();
    d.ingest_dump("crm", "cases", &c, &[row(1, "a", 10)], 1, 500).unwrap();
    d.ingest_delta("crm", "cases", &c, &[row(1, "a", 10)], 1, 100).unwrap();

    // Edited AND the timestamp moved, as an honest source does.
    let r = d.ingest_dump("crm", "cases", &c, &[row(1, "a2", 50)], 1, 500).unwrap();
    assert_eq!((r.updated, r.caught, r.missed), (1, 1, 0), "{r:?}");
    assert_eq!(r.watermark, Value::Int(50));
    assert_eq!(r.cursor_state, "safe");
    assert!(r.cursor_note().contains("safe so far"), "{}", r.cursor_note());
}

/// Policy `local` never overwrites: the difference stands and is RECORDED.
/// A vanished row is a conflict too — the local row is not deleted behind
/// the operator's back.
#[test]
fn policy_local_records_instead_of_overwriting() {
    let (d, _s) = db("local");
    d.query(
        "CREATE TABLE cases (id INTEGER PRIMARY KEY, subject ANY, updated_at ANY)",
        &[],
    )
    .unwrap();
    d.ingest_define(&SPEC.replace("policy = \"source\"", "policy = \"local\"")).unwrap();
    let c = cols();
    d.ingest_dump("crm", "cases", &c, &[row(1, "a", 10), row(2, "b", 20)], 1, 500)
        .unwrap();

    // Row 1 differs upstream, row 2 vanished upstream.
    let r = d.ingest_dump("crm", "cases", &c, &[row(1, "THEIRS", 30)], 1, 500).unwrap();
    assert_eq!((r.updated, r.deleted), (0, 0), "local wins: {r:?}");
    assert_eq!(r.conflicts, 2, "both the difference and the vanishing: {r:?}");
    assert_eq!(
        rows(d.query("SELECT subject FROM cases WHERE id = 1", &[]).unwrap())[0][0],
        Value::Text("a".into()),
        "the local row stands"
    );
    assert_eq!(ints(&d, "SELECT count(*) FROM cases"), vec![2]);

    let cs = d.ingest_conflicts("crm").unwrap();
    assert_eq!(cs.len(), 2, "{cs:?}");
    assert!(cs.iter().any(|c| c.kind == "differs"));
    assert!(cs.iter().any(|c| c.kind == "vanished"));

    assert_eq!(d.ingest_resolve("crm", "local").unwrap(), 2);
    assert!(d.ingest_conflicts("crm").unwrap().is_empty());
    let e = d.ingest_resolve("crm", "source").unwrap_err().to_string();
    assert!(e.contains("that is a call"), "{e}");
}

/// A streamed dump crosses chunk boundaries exactly, and its memory does
/// not grow: the sweep for deletes is chunked too.
#[test]
fn a_streamed_dump_crosses_chunk_boundaries() {
    std::env::set_var("MPEDB_RRETL_CHUNK", "3");
    let (d, _s) = seeded("chunks");
    let c = cols();
    let all: Vec<Vec<Value>> = (1..=10).map(|i| row(i, "x", i * 10)).collect();
    d.ingest_dump("crm", "cases", &c, &all, 1, 500).unwrap();
    assert_eq!(ints(&d, "SELECT count(*) FROM cases"), vec![10]);

    // Push a smaller dump in three chunks; the missing keys must all be
    // found by the sweep.
    let keep: Vec<Vec<Value>> = (1..=4).map(|i| row(i, "y", i * 10)).collect();
    let run = d.ingest_begin("crm", "cases", Mode::Dump).unwrap();
    for ch in keep.chunks(2) {
        d.ingest_rows(run, &c, ch, 1, 100).unwrap();
    }
    let r = d.ingest_finish(run).unwrap();
    assert_eq!((r.updated, r.deleted), (4, 6), "{r:?}");
    assert_eq!(ints(&d, "SELECT id FROM cases ORDER BY id"), vec![1, 2, 3, 4]);
    std::env::remove_var("MPEDB_RRETL_CHUNK");
}

/// Two receipts on one table cannot interleave: a dump and a delta at once
/// would need a watermark dedupe rule, and v1's answer is that the window
/// never opens (DESIGN-INGEST P8).
#[test]
fn one_receipt_per_table_at_a_time() {
    let (d, _s) = seeded("serial");
    let run = d.ingest_begin("crm", "cases", Mode::Dump).unwrap();
    let e = d.ingest_begin("crm", "cases", Mode::Delta).unwrap_err().to_string();
    assert!(e.contains("still open"), "{e}");
    assert!(e.contains("cannot interleave"), "{e}");
    d.ingest_abandon(run).unwrap();
    d.ingest_begin("crm", "cases", Mode::Delta).unwrap();
}

/// The declaration is where a source that could never catch every change
/// is refused — with the reason, not a shrug.
#[test]
fn the_declaration_refuses_what_cannot_work() {
    let (d, _s) = db("refuse");
    d.query("CREATE TABLE cases (id INTEGER PRIMARY KEY, subject ANY, updated_at ANY)", &[])
        .unwrap();

    let only_delta = r#"
[source]
name = "bad"
[[source.edge]]
name = "d"
table = "cases"
strategy = "delta"
cursor = "updated_at"
"#;
    let e = d.ingest_define(only_delta).unwrap_err().to_string();
    assert!(e.contains("declare a dump edge"), "{e}");
    assert!(e.contains("Deletes are visible through nothing else"), "{e}");

    let no_cursor = r#"
[source]
name = "bad"
[[source.edge]]
name = "d"
table = "cases"
strategy = "delta"
[[source.edge]]
name = "f"
table = "cases"
strategy = "dump"
"#;
    let e = d.ingest_define(no_cursor).unwrap_err().to_string();
    assert!(e.contains("declares no cursor candidate"), "{e}");

    let orphan = r#"
[source]
name = "bad"
[[source.edge]]
name = "f"
table = "cases"
strategy = "dump"
[[source.edge]]
name = "kid"
kind = "derived"
table = "cases"
strategy = "dump"
parent = "ghost"
"#;
    let e = d.ingest_define(orphan).unwrap_err().to_string();
    assert!(e.contains("which is not an edge"), "{e}");

    let cycle = r#"
[source]
name = "bad"
[[source.edge]]
name = "a"
kind = "derived"
table = "cases"
strategy = "dump"
parent = "b"
[[source.edge]]
name = "b"
kind = "derived"
table = "cases"
strategy = "dump"
parent = "a"
"#;
    let e = d.ingest_define(cycle).unwrap_err().to_string();
    assert!(e.contains("CYCLE"), "{e}");

    let bad_policy = SPEC.replace("policy = \"source\"", "policy = \"newest\"");
    let e = d.ingest_define(&bad_policy).unwrap_err().to_string();
    assert!(e.contains("clock you do not control"), "{e}");
}

/// A row without the identity, or with an unknown column, is refused BY
/// NAME — never silently dropped, which is how a sync quietly loses a
/// column for months.
#[test]
fn a_receipt_refuses_rows_it_cannot_place() {
    let (d, _s) = seeded("rowshape");
    let bad_col: Vec<String> = ["id", "nope"].iter().map(|s| s.to_string()).collect();
    let e = d
        .ingest_dump("crm", "cases", &bad_col, &[vec![Value::Int(1), Value::Int(2)]], 1, 0)
        .unwrap_err()
        .to_string();
    assert!(e.contains("has no column `nope`"), "{e}");

    let no_pk: Vec<String> = vec!["subject".into()];
    let e = d
        .ingest_dump("crm", "cases", &no_pk, &[vec![Value::Text("x".into())]], 1, 0)
        .unwrap_err()
        .to_string();
    assert!(e.contains("must carry `id`"), "{e}");

    let c = cols();
    let e = d
        .ingest_dump("crm", "cases", &c, &[vec![Value::Null, Value::Text("x".into()), Value::Int(1)]], 1, 0)
        .unwrap_err()
        .to_string();
    assert!(e.contains("NULL in `id`"), "{e}");
}

/// The change-rate estimator is the LLN closed form, not the naive one —
/// the naive `hits/receipts` saturates at 1 and cannot express "changes
/// faster than we look".
#[test]
fn the_change_rate_estimator_does_not_saturate() {
    let (d, _s) = seeded("lambda");
    let c = cols();
    // Ten receipts, every one of them finding a change.
    for i in 1..=10i64 {
        d.ingest_delta("crm", "cases", &c, &[row(i, "x", i * 10)], 1, 100).unwrap();
    }
    let st = d.ingest_state("crm").unwrap();
    let e = &st.iter().find(|(n, _, _)| n == "cases_delta").unwrap().1;
    assert_eq!((e.receipts, e.changed_receipts), (10, 10));
    // Naive would be 10/10 = 1.0 and could never exceed it. The LLN form
    // is hits/(k+1-hits) = 10/1 = 10.0 — "far faster than we look".
    assert!(e.lambda_per_poll() > 5.0, "{}", e.lambda_per_poll());

    // A quiet edge estimates low, not zero-or-one.
    let (d2, _s2) = seeded("lambda2");
    for _ in 0..9 {
        d2.ingest_delta("crm", "cases", &c, &[], 1, 10).unwrap();
    }
    d2.ingest_delta("crm", "cases", &c, &[row(1, "x", 10)], 1, 10).unwrap();
    let st = d2.ingest_state("crm").unwrap();
    let e = &st.iter().find(|(n, _, _)| n == "cases_delta").unwrap().1;
    assert_eq!((e.receipts, e.changed_receipts), (10, 1));
    let l = e.lambda_per_poll();
    assert!((0.05..0.2).contains(&l), "{l}");
}

/// Both doors — TOML text and a constructed spec — store ONE canonical
/// form, so `show` and re-parsing behave identically whichever was used.
#[test]
fn both_declaration_doors_store_one_form() {
    let (d, _s) = seeded("doors");
    let shown = d.ingest_show("crm").unwrap();
    let reparsed = IngestSpec::from_toml_str(&shown).unwrap();
    assert_eq!(reparsed.name, "crm");
    assert_eq!(reparsed.policy, Policy::Source);
    assert_eq!(reparsed.edges.len(), 2);

    d.ingest_define_spec(&reparsed).unwrap();
    assert_eq!(d.ingest_show("crm").unwrap(), reparsed.to_toml());
    assert_eq!(d.ingest_sources().unwrap(), vec!["crm"]);
    assert!(d.ingest_drop("crm").unwrap());
    assert!(d.ingest_sources().unwrap().is_empty());
}
