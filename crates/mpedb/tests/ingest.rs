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

// ---------------------------------------------------------------- planner

/// The planner is clock-free to test: feed it a synthetic receipt history
/// and assert the allocation's PROPERTIES, not a golden number.
fn history(d: &Database, source: &str, edge: &str, tbl: &str, n: i64, changed_every: i64) {
    // Receipts spaced one minute apart, so the observed interval is exact.
    // The run-id base is derived from the edge name so two histories in one
    // database cannot collide on the primary key.
    let base = 100_000
        + (edge.bytes().map(i64::from).sum::<i64>() * 1000) % 800_000;
    let mut w = d.begin().unwrap();
    // Clear any real receipt for this edge first: its wall-clock timestamp
    // sits ~50 years from the synthetic ones, and the observed interval —
    // (last - first) / (n-1) — would come out in the millions of seconds.
    w.query(
        "DELETE FROM ingest_stats WHERE source = $1 AND edge = $2",
        &[Value::Text(source.into()), Value::Text(edge.into())],
    )
    .unwrap();
    for i in 0..n {
        let changed = i64::from(changed_every > 0 && i % changed_every == 0);
        w.query(
            "INSERT INTO ingest_stats (run_id, source, edge, tbl, mode, ts_micros, rows_in, \
             inserted, updated, deleted, unchanged, conflicts, calls, bytes, changed, caught, \
             missed, watermark, verdict, state, note) VALUES ($1, $2, $3, $4, 'delta', $5, 0, \
             $6, 0, 0, 0, 0, 1, 100, $6, 0, 0, NULL, '', 'closed', '')",
            &[
                Value::Int(base + i),
                Value::Text(source.into()),
                Value::Text(edge.into()),
                Value::Text(tbl.into()),
                Value::Int(1_000_000_000 + i * 60_000_000),
                Value::Int(changed),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
    // And the accumulated model the estimator reads.
    let hits: i64 = (0..n).filter(|i| changed_every > 0 && i % changed_every == 0).count() as i64;
    let mut w = d.begin().unwrap();
    w.query(
        "INSERT OR REPLACE INTO ingest_state (source, edge, fingerprint, watermark, cursor_col, \
         cursor_state, caught, missed, fanout, receipts, changed_receipts, parent_calls, \
         ts_micros) VALUES ($1, $2, $3, NULL, '', 'unknown', 0, 0, 0, $4, $5, 0, 0)",
        &[
            Value::Text(source.into()),
            Value::Text(edge.into()),
            Value::Text(fingerprint_of(d, source, edge)),
            Value::Int(n),
            Value::Int(hits),
        ],
    )
    .unwrap();
    w.commit().unwrap();
}

fn fingerprint_of(d: &Database, source: &str, edge: &str) -> String {
    let spec = IngestSpec::from_toml_str(&d.ingest_show(source).unwrap()).unwrap();
    spec.edge(edge).unwrap().fingerprint()
}

/// The allocation respects the budget, starves nothing that changes, and
/// keeps the reconcile above its floor — the three properties the harmonic
/// objective was chosen for.
#[test]
fn the_plan_fits_the_budget_and_starves_nothing() {
    let (d, _s) = seeded("plan");
    // Make the tables exist so `resolve` passes, then hand the planner a
    // history: the delta edge changes on every receipt (very busy), the
    // dump edge rarely.
    d.ingest_delta("crm", "cases", &cols(), &[], 1, 10).unwrap();
    history(&d, "crm", "cases_delta", "cases", 20, 1);
    history(&d, "crm", "cases_full", "cases", 20, 10);

    let plan = d.ingest_advise("crm").unwrap();
    assert_eq!(plan.profiles.len(), 1, "{:?}", plan.skipped);
    let p = &plan.profiles[0];
    assert!(
        p.used_calls <= p.budget_calls as f64 * 1.001,
        "budget {} but {} used",
        p.budget_calls,
        p.used_calls
    );

    let roots: Vec<_> = p.edges.iter().filter(|e| e.kind == "root").collect();
    assert_eq!(roots.len(), 2);
    for e in &roots {
        assert!(
            e.rate_per_window > 0.0,
            "`{}` was starved — the harmonic objective must never do that: {e:?}",
            e.edge
        );
    }
    // The BUSY edge gets more calls than the quiet one — but only because
    // the objective says so, not because we allocated proportionally.
    let busy = roots.iter().find(|e| e.edge == "cases_delta").unwrap();
    let quiet = roots.iter().find(|e| e.edge == "cases_full").unwrap();
    assert!(busy.rate_per_window > quiet.rate_per_window, "{busy:?} vs {quiet:?}");
    // …and the expensive dump is not starved below its reconcile floor.
    assert!(quiet.cron.contains('*'), "{quiet:?}");
    assert!(!plan.cron("fetch.py").is_empty());
}

/// A dump is never scheduled at zero, however quiet its table: deletes and
/// the cursor trial both depend on it happening.
#[test]
fn the_reconcile_has_a_floor() {
    let (d, _s) = seeded("floor");
    d.ingest_delta("crm", "cases", &cols(), &[], 1, 10).unwrap();
    // A table that has never changed at all.
    history(&d, "crm", "cases_delta", "cases", 30, 0);
    history(&d, "crm", "cases_full", "cases", 30, 0);
    let plan = d.ingest_advise("crm").unwrap();
    let dump = plan.profiles[0]
        .edges
        .iter()
        .find(|e| e.edge == "cases_full")
        .unwrap();
    assert!(dump.rate_per_window > 0.0, "the reconcile was starved: {dump:?}");
    assert!(dump.reason.contains("reconcile floor"), "{}", dump.reason);
}

/// A never-observed edge is priced at a floor rather than at zero — that
/// is what earns it the observations the next plan can price properly.
/// And the census names it instead of dropping it.
#[test]
fn the_plan_names_what_it_could_not_price() {
    let (d, _s) = seeded("census");
    let plan = d.ingest_advise("crm").unwrap();
    assert!(
        plan.skipped.iter().any(|s| s.contains("receipt(s)")),
        "{:?}",
        plan.skipped
    );
    for e in plan.profiles[0].edges.iter().filter(|e| e.kind == "root") {
        assert!(e.rate_per_window > 0.0, "{e:?}");
    }
}

/// A derived edge is never scheduled — its rate IS the parent's rate times
/// the observed fan-out, and the report says where the budget goes.
#[test]
fn derived_edges_are_driven_not_scheduled() {
    let (d, _s) = db("derived");
    for t in ["cases", "details"] {
        d.query(
            &format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, subject ANY, updated_at ANY)"),
            &[],
        )
        .unwrap();
    }
    let spec = format!(
        "{}\n[[source.edge]]\nname = \"case_detail\"\nkind = \"derived\"\nparent = \
         \"cases_delta\"\ntable = \"details\"\nstrategy = \"dump\"\nbatch = 10\n\
         cost_calls = 1\n",
        SPEC
    );
    d.ingest_define(&spec).unwrap();
    d.ingest_delta("crm", "cases", &cols(), &[], 1, 10).unwrap();
    history(&d, "crm", "cases_delta", "cases", 20, 2);

    let plan = d.ingest_advise("crm").unwrap();
    let kid = plan.profiles[0]
        .edges
        .iter()
        .find(|e| e.edge == "case_detail")
        .unwrap();
    assert_eq!(kid.kind, "derived");
    assert_eq!(kid.rate_per_window, 0.0, "a derived edge is not scheduled");
    assert!(kid.cron.is_empty());
    assert!(kid.reason.contains("driven by `cases_delta`"), "{}", kid.reason);
    assert!(
        plan.skipped.iter().any(|s| s.contains("fan-out has never been observed")),
        "{:?}",
        plan.skipped
    );
}

/// Uniform is computed alongside as the control arm, and the verdict says
/// which won. (Proportional-to-change-rate loses to uniform under every
/// distribution — the trap the objective section exists to avoid.)
#[test]
fn the_plan_reports_against_the_uniform_control_arm() {
    let (d, _s) = seeded("control");
    d.ingest_delta("crm", "cases", &cols(), &[], 1, 10).unwrap();
    history(&d, "crm", "cases_delta", "cases", 20, 1);
    history(&d, "crm", "cases_full", "cases", 20, 5);
    let plan = d.ingest_advise("crm").unwrap();
    let p = &plan.profiles[0];
    assert!(p.uniform_staleness.is_finite(), "{p:?}");
    assert!(p.solved_staleness.is_finite());
    assert!(
        p.solved_staleness <= p.uniform_staleness * 1.001,
        "the solver must not lose to its own control arm: {}",
        p.verdict()
    );
    assert!(p.verdict().contains("uniform"));
}

// --------------------------------------------------------------- cascade

const CASCADE: &str = r#"
[source]
name = "sf"
policy = "source"

[[source.budget]]
profile = "work"
window_secs = 300
calls = 6

[[source.budget]]
profile = "off"
window_secs = 300
calls = 6

[[source.edge]]
name = "cases_delta"
kind = "root"
table = "cases"
strategy = "delta"
cursor = "updated_at"
cost_calls = 1

[[source.edge]]
name = "cases_full"
kind = "root"
table = "cases"
strategy = "dump"
cost_calls = 1

[[source.edge]]
name = "case_detail"
kind = "derived"
parent = "cases_delta"
table = "details"
strategy = "dump"
batch = 2
cost_calls = 1
"#;

fn cascaded(tag: &str) -> (Database, Scratch) {
    let (d, s) = db(tag);
    for t in ["cases", "details"] {
        d.query(
            &format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, subject ANY, updated_at ANY)"),
            &[],
        )
        .unwrap();
    }
    d.ingest_define(CASCADE).unwrap();
    (d, s)
}

/// The Salesforce shape: a root call returns keys, each key needs its own
/// call, and the budget decides how many happen now. Fan-out is MEASURED
/// by the derive, not declared.
#[test]
fn derived_calls_queue_atomically_and_batch_by_the_budget() {
    let (d, _s) = cascaded("cascade");
    let c = cols();

    // A delta receipt that found five changed cases, each needing a detail.
    let run = d.ingest_begin("sf", "cases_delta", Mode::Delta).unwrap();
    let batch: Vec<Vec<Value>> = (1..=5).map(|i| row(i, "x", i * 10)).collect();
    d.ingest_rows(run, &c, &batch, 1, 500).unwrap();
    let queued = d
        .ingest_derive(run, "case_detail", &(1..=5).map(Value::Int).collect::<Vec<_>>())
        .unwrap();
    assert_eq!(queued, 5);
    d.ingest_finish(run).unwrap();

    // Fan-out is now an observation: five keys from one parent call.
    let st = d.ingest_state("sf").unwrap();
    let det = &st.iter().find(|(n, _, _)| n == "case_detail").unwrap().1;
    assert_eq!((det.fanout, det.parent_calls), (5, 1));
    assert!((det.fanout_per_call() - 5.0).abs() < 1e-9);

    // The worker gets keys in batches of the edge's batch factor.
    let t = d.ingest_next("sf").unwrap().expect("work is waiting");
    assert_eq!(t.edge, "case_detail");
    assert_eq!(t.table, "details");
    assert_eq!(t.keys.len(), 2, "the edge declares batch = 2");
    assert_eq!(d.ingest_pending("sf").unwrap()[0].1, 5, "still queued until done");

    // Doing the work is an ordinary receipt — the DELTA door, because a
    // derived edge presents only the keys it was asked about.
    let detail: Vec<Vec<Value>> = t
        .keys
        .iter()
        .map(|k| vec![k.clone(), Value::Text("d".into()), Value::Int(1)])
        .collect();
    let e = d
        .ingest_dump("sf", "case_detail", &c, &detail, 1, 100)
        .unwrap_err()
        .to_string();
    assert!(e.contains("SCOPED to the keys that drove it"), "{e}");
    d.ingest_delta("sf", "case_detail", &c, &detail, 1, 100).unwrap();
    assert_eq!(d.ingest_done("sf", t.lease).unwrap(), 2);
    assert_eq!(d.ingest_pending("sf").unwrap()[0].1, 3);

    // Queueing the same keys again is idempotent — which is what makes a
    // re-read with overlap free.
    let run2 = d.ingest_begin("sf", "cases_delta", Mode::Delta).unwrap();
    d.ingest_rows(run2, &c, &[], 1, 0).unwrap();
    let again = d
        .ingest_derive(run2, "case_detail", &(3..=5).map(Value::Int).collect::<Vec<_>>())
        .unwrap();
    assert_eq!(again, 0, "already pending, so not queued twice");
    d.ingest_finish(run2).unwrap();
}

/// The budget stops the cascade. `ingest_next` returning None is the
/// budget working, not an error — and the next window resumes it.
#[test]
fn the_budget_stops_the_cascade_rather_than_an_error() {
    let (d, _s) = cascaded("budget");
    let c = cols();
    let run = d.ingest_begin("sf", "cases_delta", Mode::Delta).unwrap();
    d.ingest_rows(run, &c, &[row(1, "x", 10)], 1, 0).unwrap();
    d.ingest_derive(run, "case_detail", &(1..=20).map(Value::Int).collect::<Vec<_>>())
        .unwrap();
    d.ingest_finish(run).unwrap();

    let left = d.ingest_budget_left("sf").unwrap();
    assert_eq!(left.calls, 5, "6 budgeted, 1 spent by the delta: {left:?}");

    // Spend the rest in receipts of one call each.
    let mut handed = 0;
    while let Some(t) = d.ingest_next("sf").unwrap() {
        handed += t.keys.len();
        let rows: Vec<Vec<Value>> = t
            .keys
            .iter()
            .map(|k| vec![k.clone(), Value::Text("d".into()), Value::Int(1)])
            .collect();
        d.ingest_delta("sf", "case_detail", &c, &rows, 1, 0).unwrap();
        d.ingest_done("sf", t.lease).unwrap();
        assert!(handed <= 20, "handed more work than was queued");
    }
    // 6 calls budgeted, 1 spent by the delta, batch 2 → five batches of two.
    assert_eq!(handed, 10, "the budget bound the cascade");
    assert_eq!(d.ingest_budget_left("sf").unwrap().calls, 0);
    // The rest is still queued, waiting for the next window.
    assert!(d.ingest_pending("sf").unwrap()[0].1 > 0);
}

/// A lease that dies with its worker comes back: release explicitly, or
/// let the reaper take it. Nothing is lost either way.
#[test]
fn a_dead_worker_releases_its_keys() {
    let (d, _s) = cascaded("lease");
    let c = cols();
    let run = d.ingest_begin("sf", "cases_delta", Mode::Delta).unwrap();
    d.ingest_rows(run, &c, &[row(1, "x", 10)], 1, 0).unwrap();
    d.ingest_derive(run, "case_detail", &[Value::Int(1), Value::Int(2)]).unwrap();
    d.ingest_finish(run).unwrap();

    let t = d.ingest_next("sf").unwrap().unwrap();
    assert!(d.ingest_next("sf").unwrap().is_none(), "leased keys are not handed out twice");
    assert_eq!(d.ingest_release("sf", t.lease).unwrap(), 2);
    let again = d.ingest_next("sf").unwrap().unwrap();
    assert_eq!(again.keys.len(), 2);

    // And the reaper covers the worker that never came back at all.
    assert_eq!(mpedb::ingest_task::reap_leases(&d, "sf", 0).unwrap(), 2);
    assert!(d.ingest_next("sf").unwrap().is_some());
}

/// A derived edge may only be driven by the parent it declared, and a root
/// may not be driven at all — both refused by name.
#[test]
fn the_cascade_refuses_the_wrong_parent() {
    let (d, _s) = cascaded("parent");
    let c = cols();
    let run = d.ingest_begin("sf", "cases_full", Mode::Dump).unwrap();
    d.ingest_rows(run, &c, &[row(1, "x", 10)], 1, 0).unwrap();
    // `case_detail` declares `cases_delta` as its parent, not `cases_full`.
    let e = d
        .ingest_derive(run, "case_detail", &[Value::Int(1)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("parent is `cases_delta`"), "{e}");
    let e = d
        .ingest_derive(run, "cases_delta", &[Value::Int(1)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("roots are SCHEDULED"), "{e}");
    d.ingest_abandon(run).unwrap();
}

/// A paged dump whose row count is an exact multiple of the page size ends
/// with an empty page. That page cost a call, and it must not be an error.
#[test]
fn an_empty_last_page_still_charges_its_call() {
    let (d, _s) = seeded("emptypage");
    let c = cols();
    let run = d.ingest_begin("crm", "cases_full", Mode::Dump).unwrap();
    d.ingest_rows(run, &c, &[row(1, "a", 10), row(2, "b", 11)], 1, 200).unwrap();
    let r = d.ingest_rows(run, &[], &[], 1, 40).unwrap();
    assert_eq!((r.calls, r.bytes), (2, 240), "the empty page cost a call");
    let r = d.ingest_finish(run).unwrap();
    assert_eq!((r.inserted, r.deleted), (2, 0), "{r:?}");
    assert_eq!(ints(&d, "SELECT id FROM cases ORDER BY id"), vec![1, 2]);
}
