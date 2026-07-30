//! rRETL stage 4 — table-SET maps (design/DESIGN-RRETL.md §13).
//!
//! The contract under test: a source set mirrors into a differently-shaped
//! target set through lens pairs; edits on EITHER side flow to the other on
//! `map sync`; repeating a sync is a no-op (the echo guard); both sides
//! moving is a CONFLICT that rolls the whole sync back; and every refusal —
//! creation without a residual, lossy edits flowing backwards — is named.

use mpedb::spellfn::SpellLang;
use mpedb::{Config, Database, ExecResult, Value};

struct Scratch(String);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn db(tag: &str) -> (Database, Scratch) {
    let path = format!(
        "{}/rretl-map-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\
         durability = \"none\"\n"
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

/// neg ⇄ neg (bijective, self-inverse) and mag ⇄ sgn (residual).
fn define_pairs(d: &Database) {
    let def = |src: &str| d.create_function(SpellLang::Python, src).unwrap();
    // The verifier's standing lessons, baked in: fractional floats break the
    // round trip, ±0.0 collide under `0 - x`, and huge magnitudes overflow —
    // so the pair guards its domain (`1 // 0` = deterministic refusal).
    def("def neg(x):\n    if x % 1 != 0:\n        return 1 // 0\n    if x == 0:\n        return 1 // 0\n    if x > 4000000000:\n        return 1 // 0\n    if x < 0 - 4000000000:\n        return 1 // 0\n    return 0 - x\n");
    d.create_lens("neg", "neg", "neg", mpedb::lens::LensClass::Bijective).unwrap();
    def("def mag(x):\n    if x < 0:\n        return 0 - x\n    return x\n");
    def("def sgn(x):\n    if x < 0:\n        return 1\n    return 0\n");
    def("def unmag(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n");
    d.create_residual_lens("mag", "mag", "sgn", "unmag", mpedb::ColumnType::Int64)
        .unwrap();
}

const MAP: &str = r#"
[map]
name = "crm"

[[map.table]]
source = "customers"
target = "crm_customers"
  [[map.table.column]]
  source = "label"
  target = "full_label"
  [[map.table.column]]
  source = "score"
  target = "neg_score"
  pair = "neg"
  [[map.table.column]]
  source = "balance"
  target = "abs_balance"
  pair = "mag"
"#;

fn seeded(tag: &str) -> (Database, Scratch) {
    let (d, s) = db(tag);
    d.query(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, label ANY, score ANY, balance ANY)",
        &[],
    )
    .unwrap();
    let mut w = d.begin().unwrap();
    for (id, label, score, bal) in
        [(1i64, "ada", 10i64, -5i64), (2, "bob", -3, 7), (3, "eve", 2, -1)]
    {
        w.query(
            "INSERT INTO customers (id, label, score, balance) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(id),
                Value::Text(label.into()),
                Value::Int(score),
                Value::Int(bal),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
    define_pairs(&d);
    d.rretl_map_define(MAP).unwrap();
    (d, s)
}

/// The whole story in one run: materialize, echo-free repeat, A→B, B→A with
/// a live residual, and the conflict that rolls back whole.
#[test]
fn a_map_syncs_both_ways_and_repeating_it_is_a_no_op() {
    let (d, _s) = seeded("bidir");

    // First sync: materialization. The target was auto-created.
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!((r.created_b, r.unchanged), (3, 0), "{r:?}");
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers ORDER BY id"), vec![-10, 3, -2]);
    assert_eq!(ints(&d, "SELECT abs_balance FROM crm_customers ORDER BY id"), vec![5, 7, 1]);

    // The echo guard: nothing moved, nothing moves.
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!((r.changed_total(), r.unchanged), (0, 3), "{r:?}");

    // A-side edit flows forward.
    d.query("UPDATE customers SET score = 4 WHERE id = 2", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.a_to_b, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers ORDER BY id"), vec![-10, -4, -2]);
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);

    // B-side edit flows back — the residual column via the LIVE rex: row 1's
    // balance is -5 (sign bit 1); editing |balance| to 9 must come home as -9.
    d.query("UPDATE crm_customers SET abs_balance = 9 WHERE id = 1", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.b_to_a, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT balance FROM customers ORDER BY id"), vec![-9, 7, -1]);
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);

    // Both sides move: conflict, named, rolled back WHOLE.
    d.query("UPDATE customers SET score = 100 WHERE id = 3", &[]).unwrap();
    d.query("UPDATE crm_customers SET neg_score = 42 WHERE id = 3", &[]).unwrap();
    let e = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(e.contains("CONFLICT") && e.contains("crm_customers"), "{e}");
    // Neither side was touched by the aborted run.
    assert_eq!(ints(&d, "SELECT score FROM customers WHERE id = 3"), vec![100]);
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers WHERE id = 3"), vec![42]);
    // The failure is first-class lineage.
    assert!(d
        .rretl_log()
        .unwrap()
        .iter()
        .any(|l| l.outcome == "failed" && l.lens == "map:crm"));

    // Resolve by undoing the B side (back to its last-synced value); the A
    // edit then flows.
    d.query("UPDATE crm_customers SET neg_score = $1 WHERE id = 3", &[Value::Int(-2)])
        .unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.a_to_b, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers WHERE id = 3"), vec![-100]);

    // Successful syncs are lineage too, and not unwindable.
    let mapped: Vec<_> = d
        .rretl_log()
        .unwrap()
        .into_iter()
        .filter(|l| l.outcome == "mapped")
        .collect();
    assert!(!mapped.is_empty());
    let e = d.rretl_revert(mapped[0].run_id).unwrap_err().to_string();
    assert!(e.contains("mapped"), "{e}");
}

/// Deletes propagate in both directions, the state row is cleared, and a
/// re-created key is a clean creation — never a false "deleted while
/// changed" conflict from stale state.
#[test]
fn deletes_propagate_and_recreated_keys_are_clean() {
    let (d, _s) = seeded("del");
    d.rretl_map_sync("crm").unwrap();

    // Delete on the target: the source row follows.
    d.query("DELETE FROM crm_customers WHERE id = 2", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.deleted_a, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT id FROM customers ORDER BY id"), vec![1, 3]);

    // Delete on the source: the target row follows.
    d.query("DELETE FROM customers WHERE id = 3", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.deleted_b, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT id FROM crm_customers ORDER BY id"), vec![1]);

    // Both sides delete the last row between syncs; then the key is REUSED.
    d.query("DELETE FROM customers WHERE id = 1", &[]).unwrap();
    d.query("DELETE FROM crm_customers WHERE id = 1", &[]).unwrap();
    d.rretl_map_sync("crm").unwrap(); // clears the stale state row (pass 3)
    let mut w = d.begin().unwrap();
    w.query(
        "INSERT INTO customers (id, label, score, balance) VALUES (1, $1, 2, -8)",
        &[Value::Text("new-ada".into())],
    )
    .unwrap();
    w.commit().unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.created_b, 1, "stale state must not fake a conflict: {r:?}");
    assert_eq!(ints(&d, "SELECT abs_balance FROM crm_customers ORDER BY id"), vec![8]);

    // Deleted on one side while the OTHER side changed = conflict.
    d.query("UPDATE customers SET score = 9 WHERE id = 1", &[]).unwrap();
    d.query("DELETE FROM crm_customers WHERE id = 1", &[]).unwrap();
    let e = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(e.contains("CONFLICT"), "{e}");
}

/// Creation rules: a row born on the target side inverts into the source
/// ONLY when every mapped column is bijective; a residual column refuses by
/// name (the §4 creation path), and a rowid-identity source refuses always.
#[test]
fn target_side_creation_follows_the_creation_path_rules() {
    let (d, _s) = seeded("create");
    // A second, all-bijective mapping in the same database.
    d.query("CREATE TABLE tags (id INTEGER PRIMARY KEY, w ANY)", &[]).unwrap();
    d.rretl_map_define(
        r#"
[map]
name = "tags"
[[map.table]]
source = "tags"
target = "ext_tags"
  [[map.table.column]]
  source = "w"
  target = "neg_w"
  pair = "neg"
"#,
    )
    .unwrap();
    d.rretl_map_sync("tags").unwrap();
    // Created on the target: inverts home.
    d.query("INSERT INTO ext_tags (id, neg_w) VALUES (7, -12)", &[]).unwrap();
    let r = d.rretl_map_sync("tags").unwrap();
    assert_eq!(r.created_a, 1, "{r:?}");
    assert_eq!(ints(&d, "SELECT w FROM tags"), vec![12]);

    // The residual mapping refuses a target-side birth, by name.
    d.rretl_map_sync("crm").unwrap();
    d.query(
        "INSERT INTO crm_customers (id, full_label, neg_score, abs_balance) \
         VALUES (9, $1, 0, 3)",
        &[Value::Text("ghost".into())],
    )
    .unwrap();
    let e = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(e.contains("abs_balance") && e.contains("creation path"), "{e}");
    d.query("DELETE FROM crm_customers WHERE id = 9", &[]).unwrap();

    // A rowid-identity source refuses target births always.
    d.query("CREATE TABLE notes (txt ANY)", &[]).unwrap();
    d.rretl_map_define(
        r#"
[map]
name = "notes"
[[map.table]]
source = "notes"
target = "ext_notes"
  [[map.table.column]]
  source = "txt"
  target = "txt2"
"#,
    )
    .unwrap();
    d.rretl_map_sync("notes").unwrap();
    d.query("INSERT INTO ext_notes (id, txt2) VALUES (1, $1)", &[Value::Text("hi".into())])
        .unwrap();
    let e = d.rretl_map_sync("notes").unwrap_err().to_string();
    assert!(e.contains("rowid"), "{e}");
}

/// A lossy pair flows forward freely and REFUSES to flow back, by name.
#[test]
fn a_lossy_column_is_one_way() {
    let (d, _s) = db("lossy");
    d.query("CREATE TABLE m (id INTEGER PRIMARY KEY, v ANY)", &[]).unwrap();
    d.query("INSERT INTO m (id, v) VALUES (1, 17)", &[]).unwrap();
    d.create_function(SpellLang::Python, "def floor10(x):\n    return x - (x % 10)\n")
        .unwrap();
    d.create_function(SpellLang::Python, "def ident(x):\n    return x\n").unwrap();
    d.create_lens("floor10", "floor10", "ident", mpedb::lens::LensClass::Lossy).unwrap();
    d.rretl_map_define(
        r#"
[map]
name = "l"
[[map.table]]
source = "m"
target = "ext_m"
  [[map.table.column]]
  source = "v"
  target = "coarse_v"
  pair = "floor10"
"#,
    )
    .unwrap();
    d.rretl_map_sync("l").unwrap();
    assert_eq!(ints(&d, "SELECT coarse_v FROM ext_m"), vec![10]);

    // Forward keeps flowing.
    d.query("UPDATE m SET v = 34 WHERE id = 1", &[]).unwrap();
    d.rretl_map_sync("l").unwrap();
    assert_eq!(ints(&d, "SELECT coarse_v FROM ext_m"), vec![30]);

    // Backward is refused with the pair's nature named.
    d.query("UPDATE ext_m SET coarse_v = 90 WHERE id = 1", &[]).unwrap();
    let e = d.rretl_map_sync("l").unwrap_err().to_string();
    assert!(e.contains("LOSSY"), "{e}");
    // And the source is untouched by the refusal.
    assert_eq!(ints(&d, "SELECT v FROM m"), vec![34]);
}

/// Chunk boundaries: with the chunk forced tiny, a 40-row map syncs, edits
/// carry both ways, and repeats stay no-ops.
#[test]
fn map_sync_crosses_chunk_boundaries_exactly() {
    // Thread-local, not `set_var`: this file's tests run as THREADS in one
    // process, and a process-global knob makes them race (see `ChunkGuard`).
    let _chunk = mpedb::rretl::ChunkGuard::new(7);
    let (d, _s) = seeded("chunky");
    let mut w = d.begin().unwrap();
    for id in 4..=40i64 {
        w.query(
            "INSERT INTO customers (id, label, score, balance) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(id),
                Value::Text(format!("c{id}")),
                Value::Int((id % 13) - 20),
                Value::Int((id % 7) - 8),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();

    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.created_b, 40, "{r:?}");
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);

    // Every row's round trip holds: neg(neg(score)) == score via the map.
    let scores = ints(&d, "SELECT score FROM customers ORDER BY id");
    let negs = ints(&d, "SELECT neg_score FROM crm_customers ORDER BY id");
    assert_eq!(scores.len(), 40);
    for (s0, n) in scores.iter().zip(&negs) {
        assert_eq!(*s0, -n);
    }

    // A B-side edit mid-table flows back across boundaries.
    d.query("UPDATE crm_customers SET abs_balance = 99 WHERE id = 20", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.b_to_a, 1, "{r:?}");
    let bal20 = ints(&d, "SELECT balance FROM customers WHERE id = 20");
    assert_eq!(bal20[0].abs(), 99);
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);
}

/// The definition surface: list/show/drop round-trip, and the validator's
/// named refusals (bad identifier, bookkeeping target, duplicate source
/// column, missing pair).
#[test]
fn map_definitions_round_trip_and_refuse_by_name() {
    let (d, _s) = seeded("defs");
    assert_eq!(d.rretl_maps().unwrap(), vec!["crm".to_string()]);
    assert!(d.rretl_map_show("crm").unwrap().contains("crm_customers"));
    let e = d.rretl_map_show("nope").unwrap_err().to_string();
    assert!(e.contains("nope"), "{e}");

    let bad = |toml: &str, needle: &str| {
        let e = d.rretl_map_define(toml).unwrap_err().to_string();
        assert!(e.contains(needle), "wanted `{needle}` in: {e}");
    };
    bad(
        "[map]\nname = \"x; drop\"\n[[map.table]]\nsource = \"customers\"\n\
         target = \"t\"\n  [[map.table.column]]\n  source = \"label\"\n  target = \"l\"\n",
        "not a legal map name",
    );
    bad(
        "[map]\nname = \"m\"\n[[map.table]]\nsource = \"customers\"\n\
         target = \"rretl_lineage\"\n  [[map.table.column]]\n  source = \"label\"\n  \
         target = \"l\"\n",
        "bookkeeping",
    );
    bad(
        "[map]\nname = \"m\"\n[[map.table]]\nsource = \"customers\"\ntarget = \"t\"\n\
         [[map.table.column]]\n  source = \"label\"\n  target = \"a\"\n\
         [[map.table.column]]\n  source = \"label\"\n  target = \"b\"\n",
        "mapped twice",
    );
    bad(
        "[map]\nname = \"m\"\n[[map.table]]\nsource = \"customers\"\ntarget = \"t\"\n\
         [[map.table.column]]\n  source = \"label\"\n  target = \"l\"\n  \
         pair = \"no_such_pair\"\n",
        "no lens pair named",
    );

    assert!(d.rretl_map_drop("crm").unwrap());
    assert!(!d.rretl_map_drop("crm").unwrap());
    assert!(d.rretl_maps().unwrap().is_empty());
    let e = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(e.contains("no map named"), "{e}");
}

/// The programmatic define door: a constructed spec stores the CANONICAL
/// TOML, `show` returns it, re-parsing it yields the same spec, and syncing
/// through it behaves identically to the TOML door.
#[test]
fn a_constructed_spec_round_trips_through_canonical_toml() {
    use mpedb::rretl_map::{MapColumn, MapSpec, MapTable};
    let (d, _s) = seeded("spec");
    d.rretl_map_drop("crm").unwrap();
    let spec = MapSpec {
        name: "crm".into(),
        stream: false,
        tables: vec![MapTable {
            source: "customers".into(),
            target: "crm_customers".into(),
            target_key: None,
            columns: vec![
                MapColumn { source: "label".into(), target: "full_label".into(), pair: None },
                MapColumn {
                    source: "balance".into(),
                    target: "abs_balance".into(),
                    pair: Some("mag".into()),
                },
            ],
        }],
    };
    d.rretl_map_define_spec(&spec).unwrap();
    let shown = d.rretl_map_show("crm").unwrap();
    let reparsed = MapSpec::from_toml_str(&shown).unwrap();
    assert_eq!(reparsed.name, "crm");
    assert_eq!(reparsed.tables[0].columns.len(), 2);
    assert_eq!(reparsed.tables[0].columns[1].pair.as_deref(), Some("mag"));

    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.created_b, 3, "{r:?}");
    assert_eq!(
        ints(&d, "SELECT abs_balance FROM crm_customers ORDER BY id"),
        vec![5, 7, 1]
    );
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);
}

/// `map check` is the read-only twin of sync: same classification, no
/// writes, every blocker named instead of aborting on the first.
#[test]
fn check_is_the_dry_run_twin_of_sync() {
    let (d, _s) = seeded("check");

    // Before the first sync nothing exists on the target side: the check
    // sees three creations pending and writes NOTHING (repeat is stable).
    let r = d.rretl_map_check("crm").unwrap();
    assert_eq!(r.tables.len(), 1);
    assert_eq!(r.tables[0].would_create_b, 3);
    assert_eq!(r.pending_total(), 3);
    assert!(!r.is_clean());
    let r2 = d.rretl_map_check("crm").unwrap();
    assert_eq!(r2.tables[0].would_create_b, 3, "check must not materialize");

    d.rretl_map_sync("crm").unwrap();
    let r = d.rretl_map_check("crm").unwrap();
    assert!(r.is_clean(), "post-sync steady state: {r:?}");
    assert_eq!(r.tables[0].unchanged, 3);

    // One edit per side (different rows): both directions pending, no
    // conflict, and a sync drains it back to clean.
    d.query("UPDATE customers SET score = 4 WHERE id = 1", &[]).unwrap();
    d.query("UPDATE crm_customers SET neg_score = 8 WHERE id = 2", &[]).unwrap();
    let r = d.rretl_map_check("crm").unwrap();
    assert_eq!(r.tables[0].pending_a2b, 1);
    assert_eq!(r.tables[0].pending_b2a, 1);
    assert!(r.tables[0].conflicts.is_empty());
    d.rretl_map_sync("crm").unwrap();
    assert!(d.rretl_map_check("crm").unwrap().is_clean());

    // Both sides of the SAME row: the sync aborts, the check names it and
    // keeps counting everything else.
    d.query("UPDATE customers SET score = 5 WHERE id = 3", &[]).unwrap();
    d.query("UPDATE crm_customers SET neg_score = 0 - 9 WHERE id = 3", &[]).unwrap();
    let r = d.rretl_map_check("crm").unwrap();
    assert_eq!(r.tables[0].conflicts.len(), 1, "{:?}", r.tables[0].conflicts);
    assert!(r.tables[0].conflicts[0].contains("CONFLICT"));
    assert!(r.tables[0].conflicts[0].contains("Int(3)"));
    assert_eq!(r.tables[0].unchanged, 2);
    assert!(d.rretl_map_sync("crm").is_err());

    // The documented resolution: revert one side, re-sync, clean again.
    d.query("UPDATE customers SET score = 2 WHERE id = 3", &[]).unwrap();
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.b_to_a, 1);
    assert!(d.rretl_map_check("crm").unwrap().is_clean());
}

/// The blind spot made visible. The echo guard's mechanism is NOT looking:
/// a row whose recorded hashes match both sides is skipped, so
/// `forward(A) == B` is never re-checked. Its realistic breaker needs no
/// tampering at all — `create_lens` REBINDS an existing pair name, and a
/// map resolves its pairs BY NAME at every sync, so the meaning of a
/// mapped column can change under a map whose state is entirely current.
/// The sync then reports nothing moved, forever, while every row's target
/// is the old pair's output; only `map check` and `rretl fsck` see it.
#[test]
fn a_rebound_pair_stands_diverged_and_only_check_and_fsck_see_it() {
    let (d, _s) = seeded("diverged");
    d.rretl_map_sync("crm").unwrap();
    assert!(d.rretl_map_check("crm").unwrap().is_clean());

    // Rebind `neg` to a DIFFERENT function under the same name. Nothing in
    // the map, the data or the state changed — only the meaning did.
    let def = |src: &str| d.create_function(SpellLang::Python, src).unwrap();
    def("def inc(x):\n    if x % 1 != 0:\n        return 1 // 0\n    if x == 0:\n        return 1 // 0\n    if x > 4000000000:\n        return 1 // 0\n    if x < 0 - 4000000000:\n        return 1 // 0\n    return x + 1\n");
    def("def dec(y):\n    if y % 1 != 0:\n        return 1 // 0\n    if y > 4000000000:\n        return 1 // 0\n    if y < 0 - 4000000000:\n        return 1 // 0\n    return y - 1\n");
    d.create_lens("neg", "inc", "dec", mpedb::lens::LensClass::Bijective)
        .unwrap();

    // The sync is structurally blind to this — that is the price of the
    // echo guard, and the reason the check exists.
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);

    let r = d.rretl_map_check("crm").unwrap();
    assert_eq!(r.tables[0].diverged.len(), 3, "{:?}", r.tables[0].diverged);
    assert_eq!(r.tables[0].unchanged, 0);
    assert_eq!(r.pending_total(), 0);
    assert!(!r.is_clean());

    let findings = d.rretl_fsck().unwrap();
    let map_findings: Vec<_> = findings.iter().filter(|f| f.contains("map `crm`")).collect();
    assert_eq!(map_findings.len(), 3, "{findings:?}");
}

/// A state row whose key VALUE no longer matches its key REFERENCE can
/// identify nothing: pass 3 must not act on it (it would delete a row
/// belonging to a different key) and must not feed it to a rigid point
/// lookup (a wrongly-typed value took sync, check AND fsck down with one
/// bind error). It stands as a named breach until someone clears it.
#[test]
fn a_state_row_whose_key_does_not_match_its_reference_is_named_not_obeyed() {
    let (d, _s) = seeded("badkey");
    d.rretl_map_sync("crm").unwrap();
    d.query("UPDATE rretl_map_state SET k = 'not even an int' WHERE k = 2", &[])
        .unwrap();

    // Everything still works, and the breach is named rather than fatal.
    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.changed_total(), 0);
    let chk = d.rretl_map_check("crm").unwrap();
    assert_eq!(chk.tables[0].diverged.len(), 1, "{:?}", chk.tables[0].diverged);
    assert!(chk.tables[0].diverged[0].contains("does not match"));
    let findings = d.rretl_fsck().unwrap();
    assert!(
        findings.iter().any(|f| f.contains("does not match")),
        "{findings:?}"
    );
    // The other two rows still sync normally.
    d.query("UPDATE customers SET score = 6 WHERE id = 1", &[]).unwrap();
    assert_eq!(d.rretl_map_sync("crm").unwrap().a_to_b, 1);
}

/// The map's target table is DROPPED (an external reset, a migration) and
/// its state rows outlive it. One missing-table condition cannot mean both
/// "materialize it" and "the target deleted every row" — the second
/// reading emptied the master. Refused by name; check and fsck say the
/// same; re-defining the map re-baselines and re-materializes.
#[test]
fn a_dropped_target_is_refused_not_read_as_a_mass_delete() {
    let (d, _s) = seeded("droptarget");
    d.rretl_map_sync("crm").unwrap();
    d.query("DROP TABLE crm_customers", &[]).unwrap();

    let e = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(e.contains("is GONE"), "{e}");
    assert!(e.contains("would empty"), "{e}");
    assert_eq!(ints(&d, "SELECT count(*) FROM customers")[0], 3);

    let chk = d.rretl_map_check("crm").unwrap();
    assert!(!chk.is_clean());
    assert!(chk.tables[0].conflicts[0].contains("is GONE"));
    assert!(d.rretl_fsck().unwrap().iter().any(|f| f.contains("is GONE")));

    // The documented recovery: drop (which clears state) and define again.
    // An IDENTICAL re-define is idempotent by design and keeps state, so it
    // is NOT the escape — the refusal names the one that is.
    assert!(d.rretl_map_drop("crm").unwrap());
    d.rretl_map_define(MAP).unwrap();
    assert_eq!(d.rretl_map_sync("crm").unwrap().created_b, 3);
    assert_eq!(ints(&d, "SELECT count(*) FROM customers")[0], 3);
    assert!(d.rretl_map_check("crm").unwrap().is_clean());
}

/// Hull 2 closed: redefining a map with a CHANGED spec re-baselines (its
/// state rows are deleted in the same txn), so the next sync re-arbitrates
/// instead of silently reading every row as clean under hashes recorded
/// for the OLD spec. An UNCHANGED redefine keeps state.
#[test]
fn redefining_a_changed_map_re_baselines_its_state() {
    let (d, _s) = seeded("redefine");
    d.rretl_map_sync("crm").unwrap();

    // Same TOML again: state survives, the next sync is still the no-op.
    d.rretl_map_define(MAP).unwrap();
    assert_eq!(
        ints(&d, "SELECT count(*) FROM rretl_map_state")[0],
        3,
        "an unchanged redefine must keep state"
    );
    assert_eq!(d.rretl_map_sync("crm").unwrap().changed_total(), 0);

    // Changed spec: the neg pair on score becomes an identity copy. The
    // raw value chains are untouched, so WITHOUT re-baselining every row
    // would read "both clean" and the target would keep the OLD pair's
    // forward forever — the silent failure this fix exists for.
    let changed = MAP.replace(
        "  source = \"score\"\n  target = \"neg_score\"\n  pair = \"neg\"",
        "  source = \"score\"\n  target = \"neg_score\"",
    );
    assert_ne!(changed, MAP);
    d.rretl_map_define(&changed).unwrap();
    assert_eq!(
        ints(&d, "SELECT count(*) FROM rretl_map_state")[0],
        0,
        "a changed redefine must re-baseline"
    );

    // Re-arbitration is honest: both sides hold the rows and now DISAGREE
    // (identity vs the old neg forward), which is a named conflict — not a
    // silent skip.
    let err = d.rretl_map_sync("crm").unwrap_err().to_string();
    assert!(err.contains("no recorded sync can arbitrate"), "{err}");
    let r = d.rretl_map_check("crm").unwrap();
    assert_eq!(r.tables[0].conflicts.len(), 3, "{:?}", r.tables[0].conflicts);
}

/// The value saboteur's finding: state keys are (map ‖ tbl ‖ pk_enc), so a
/// legal long TEXT pk — with the ceiling shrinking as the MAP and TARGET
/// NAMES grew — overflowed the engine's encoded-key cap mid-sync, wedging
/// the map behind an unnamed refusal while `map check` claimed a sync
/// would run through. State now keys on a fixed 32-byte digest of the pk;
/// the only key limit left is the source table's own.
#[test]
fn long_text_keys_and_long_names_sync_and_check_clean() {
    let (d, _s) = db("longkey");
    define_pairs(&d);
    d.query(
        "CREATE TABLE src_with_a_deliberately_long_name (k TEXT PRIMARY KEY, v ANY)",
        &[],
    )
    .unwrap();
    let (k1, k2) = ("K".repeat(974), format!("{}Z", "K".repeat(973)));
    let mut w = d.begin().unwrap();
    for (k, v) in [(&k1, 7i64), (&k2, 4)] {
        w.query(
            "INSERT INTO src_with_a_deliberately_long_name (k, v) VALUES ($1, $2)",
            &[Value::Text(k.clone()), Value::Int(v)],
        )
        .unwrap();
    }
    w.commit().unwrap();
    d.rretl_map_define(
        r#"
[map]
name = "a_map_name_that_is_quite_long_indeed"

[[map.table]]
source = "src_with_a_deliberately_long_name"
target = "ext_with_an_equally_long_target_name"
  [[map.table.column]]
  source = "v"
  target = "nv"
  pair = "neg"
"#,
    )
    .unwrap();

    let name = "a_map_name_that_is_quite_long_indeed";
    let r = d.rretl_map_sync(name).unwrap();
    assert_eq!(r.created_b, 2);
    assert!(d.rretl_map_check(name).unwrap().is_clean());

    // Both directions and a delete, all on the long keys.
    d.query(
        "UPDATE src_with_a_deliberately_long_name SET v = 9 WHERE k = $1",
        &[Value::Text(k1.clone())],
    )
    .unwrap();
    d.query(
        "UPDATE ext_with_an_equally_long_target_name SET nv = 0 - 5 WHERE k = $1",
        &[Value::Text(k2.clone())],
    )
    .unwrap();
    let r = d.rretl_map_sync(name).unwrap();
    assert_eq!((r.a_to_b, r.b_to_a), (1, 1));
    d.query(
        "DELETE FROM src_with_a_deliberately_long_name WHERE k = $1",
        &[Value::Text(k1)],
    )
    .unwrap();
    let r = d.rretl_map_sync(name).unwrap();
    assert_eq!(r.deleted_b, 1);
    assert!(d.rretl_map_check(name).unwrap().is_clean());
    assert!(d.rretl_fsck().unwrap().is_empty());
}

/// SQL identifiers are case-INSENSITIVE, so a map spec must resolve the
/// way `SELECT * FROM SRC` does. Before this a source in the wrong case
/// was refused by name while a TARGET in the wrong case was ACCEPTED —
/// then auto-created into a raw `duplicate table name` at sync, leaving a
/// permanently unsyncable map that `check` reported as ordinary pending
/// creations. Everything downstream now uses the schema's spelling.
#[test]
fn a_spec_in_the_wrong_case_resolves_like_sql_does() {
    let (d, _s) = seeded("case");
    d.query(
        "CREATE TABLE crm_customers (id INTEGER PRIMARY KEY, full_label ANY, \
         neg_score ANY, abs_balance ANY)",
        &[],
    )
    .unwrap();
    d.rretl_map_define(&MAP.replace("customers", "CUSTOMERS").replace("label", "LABEL"))
        .unwrap();

    // The report names the SCHEMA's spelling, not the spec's.
    let chk = d.rretl_map_check("crm").unwrap();
    assert_eq!(chk.tables[0].src, "customers");
    assert_eq!(chk.tables[0].dst, "crm_customers");
    assert_eq!(chk.tables[0].would_create_b, 3);

    let r = d.rretl_map_sync("crm").unwrap();
    assert_eq!(r.created_b, 3);
    assert!(d.rretl_map_check("crm").unwrap().is_clean());
    // And the round trip still works through the mis-cased spec.
    d.query("UPDATE crm_customers SET neg_score = 0 - 12 WHERE id = 1", &[])
        .unwrap();
    assert_eq!(d.rretl_map_sync("crm").unwrap().b_to_a, 1);
    assert_eq!(ints(&d, "SELECT score FROM customers WHERE id = 1"), vec![12]);
}

// ------------------------------------------------------------ #53 daemon

/// A map with TWO tables and a chunk of 2: the daemon must commit both
/// tables together (never finish one before starting the other), stop on
/// its row budget, and the NEXT run must resume where this one stopped
/// rather than starting the round over.
#[test]
fn the_daemon_advances_both_tables_together_and_resumes() {
    use mpedb::rretl_map_run::{RunOptions, RunStop};
    let (d, _s) = db("daemon");
    define_pairs(&d);
    for t in ["a1", "a2"] {
        d.query(
            &format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, v ANY)"),
            &[],
        )
        .unwrap();
    }
    let mut w = d.begin().unwrap();
    for t in ["a1", "a2"] {
        for id in 1..=6i64 {
            w.query(
                &format!("INSERT INTO {t} (id, v) VALUES ($1, $2)"),
                &[Value::Int(id), Value::Int(id * 10)],
            )
            .unwrap();
        }
    }
    w.commit().unwrap();
    d.rretl_map_define(
        r#"
[map]
name = "two"

[[map.table]]
source = "a1"
target = "b1"
  [[map.table.column]]
  source = "v"
  target = "nv"
  pair = "neg"

[[map.table]]
source = "a2"
target = "b2"
  [[map.table.column]]
  source = "v"
  target = "nv"
  pair = "neg"
"#,
    )
    .unwrap();

    // Budget: 4 rows. With chunk 2 that is one commit (2 rows from EACH
    // table) plus the budget check — so both targets must hold 2 rows,
    // never 4-and-0.
    let _chunk = mpedb::rretl::ChunkGuard::new(2);
    let opts = RunOptions { max_rows: Some(4), ..Default::default() };
    let r = d.rretl_map_run("two", &opts).unwrap();
    assert_eq!(r.stopped_by, Some(RunStop::Budget));
    assert_eq!(r.rows, 4, "{r:?}");
    assert_eq!(ints(&d, "SELECT count(*) FROM b1"), vec![2]);
    assert_eq!(ints(&d, "SELECT count(*) FROM b2"), vec![2]);

    // Resume: the next run continues rather than re-materializing rows 1-2.
    let r = d.rretl_map_run("two", &opts).unwrap();
    assert_eq!(r.moved.created_b, 4, "resumed run created rows 3-4: {r:?}");
    assert_eq!(ints(&d, "SELECT count(*) FROM b1"), vec![4]);
    assert_eq!(ints(&d, "SELECT count(*) FROM b2"), vec![4]);

    // Keep going until the round completes, then the map is fully synced
    // and `check` agrees.
    let mut guard = 0;
    loop {
        let r = d.rretl_map_run("two", &opts).unwrap();
        guard += 1;
        assert!(guard < 20, "the round never completed");
        if r.stopped_by == Some(RunStop::RoundComplete) {
            break;
        }
    }
    assert_eq!(ints(&d, "SELECT count(*) FROM b1"), vec![6]);
    assert_eq!(ints(&d, "SELECT count(*) FROM b2"), vec![6]);
    assert!(d.rretl_map_check("two").unwrap().is_clean());
    assert!(d.rretl_map_status("two").unwrap().round >= 1);
}

/// The runner restriction is a named refusal, and clearing it lifts the
/// refusal. (A policy guard, not an auth boundary — see the module docs.)
#[test]
fn a_map_can_be_restricted_to_one_runner() {
    use mpedb::rretl_map_run::RunOptions;
    let (d, _s) = seeded("runner");
    d.rretl_map_set_runner("crm", "server-1").unwrap();

    let anon = RunOptions::default();
    let e = d.rretl_map_run("crm", &anon).unwrap_err().to_string();
    assert!(e.contains("restricted to runner `server-1`"), "{e}");
    assert!(e.contains("no runner"), "{e}");

    let wrong = RunOptions { runner: Some("laptop".into()), ..Default::default() };
    let e = d.rretl_map_run("crm", &wrong).unwrap_err().to_string();
    assert!(e.contains("`laptop`"), "{e}");

    let right = RunOptions { runner: Some("server-1".into()), ..Default::default() };
    let r = d.rretl_map_run("crm", &right).unwrap();
    assert_eq!(r.moved.created_b, 3);
    assert_eq!(d.rretl_map_status("crm").unwrap().runner, "server-1");

    d.rretl_map_set_runner("crm", "").unwrap();
    d.rretl_map_run("crm", &anon).unwrap();
}

/// Unlike `map sync`, a conflict does not abort the daemon: it is counted,
/// named in the report, and the OTHER rows still sync. One unresolvable
/// row must never block every other row forever.
#[test]
fn the_daemon_counts_conflicts_and_keeps_going() {
    use mpedb::rretl_map_run::RunOptions;
    let (d, _s) = seeded("conflict");
    let opts = RunOptions::default();
    d.rretl_map_run("crm", &opts).unwrap();

    // Row 2 moves on BOTH sides (a conflict), row 1 only on the source.
    d.query("UPDATE customers SET score = 5 WHERE id = 2", &[]).unwrap();
    d.query("UPDATE crm_customers SET neg_score = 0 - 9 WHERE id = 2", &[]).unwrap();
    d.query("UPDATE customers SET score = 4 WHERE id = 1", &[]).unwrap();

    // `map sync` aborts whole; the daemon does not.
    assert!(d.rretl_map_sync("crm").is_err());
    let r = d.rretl_map_run("crm", &opts).unwrap();
    assert_eq!(r.conflicts, 1, "{r:?}");
    assert!(r.conflict_notes[0].contains("CONFLICT"));
    assert_eq!(r.moved.a_to_b, 1, "the unrelated row still synced: {r:?}");
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers WHERE id = 1"), vec![-4]);
    // The conflicted row is untouched and still visible to `check`.
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers WHERE id = 2"), vec![-9]);
    assert_eq!(d.rretl_map_check("crm").unwrap().tables[0].conflicts.len(), 1);
}

// ----------------------------------------------- §15: the stream form

/// A streaming map of the same shape. Everything else is `seeded`'s.
fn streamed(tag: &str) -> (Database, Scratch) {
    let (d, s) = seeded(tag);
    d.rretl_map_drop("crm").unwrap();
    d.rretl_map_define(&MAP.replace("name = \"crm\"", "name = \"crm\"\nstream = true"))
        .unwrap();
    (d, s)
}

fn backlog(d: &Database) -> i64 {
    d.rretl_map_backlog("crm").unwrap().iter().map(|(_, n)| n).sum()
}

fn run(d: &Database) -> mpedb::rretl_map_run::MapRunReport {
    d.rretl_map_run("crm", &mpedb::rretl_map_run::RunOptions::default()).unwrap()
}

/// The point of the whole section: a round costs the CHANGES, not the table
/// — and lands the same answer the scan would.
#[test]
fn the_journal_costs_the_changes_and_the_scan_costs_the_table() {
    let (d, _s) = streamed("cost");
    // Materialize, and let the round finish so the cursors are clean.
    d.rretl_map_sync("crm").unwrap();
    assert_eq!(backlog(&d), 0, "a full sync leaves nothing outstanding");

    // One row moves. The journal now names exactly it.
    d.query("UPDATE customers SET score = 40 WHERE id = 2", &[]).unwrap();
    assert_eq!(backlog(&d), 1);

    let r = run(&d);
    assert!(r.streamed >= 1, "the drain consumed the entry: {r:?}");
    assert_eq!(ints(&d, "SELECT neg_score FROM crm_customers ORDER BY id"), vec![-10, -40, -2]);

    // And the echo terminates: the push re-journaled the target row, the
    // next drain classifies it clean, and nothing is left.
    let r2 = run(&d);
    assert_eq!(r2.moved.changed_total(), 0, "the echo wrote nothing: {r2:?}");
    assert_eq!(backlog(&d), 0, "and drained itself");
}

/// The claim that keeps the design honest: the journal is a FAST PATH, so a
/// change it never heard about is still found — by the scan, which does not
/// go away.
#[test]
fn a_change_the_journal_missed_is_still_caught_by_the_round() {
    let (d, _s) = streamed("missed");
    d.rretl_map_sync("crm").unwrap();

    // A write, and then its journal entries are destroyed — exactly what a
    // `mirror import`, a dropped trigger or a restored file leaves behind:
    // rows that differ with nothing recording that they do.
    d.query("UPDATE customers SET label = 'ADA' WHERE id = 1", &[]).unwrap();
    d.query("DELETE FROM rretl_map_dirty", &[]).unwrap();
    assert_eq!(backlog(&d), 0, "nothing knows this row moved");

    // Rounds still walk everything, so the round finds it.
    for _ in 0..8 {
        let r = run(&d);
        if r.round > 0 {
            break;
        }
    }
    assert_eq!(
        rows(d.query("SELECT full_label FROM crm_customers WHERE id = 1", &[]).unwrap())[0][0],
        Value::Text("ADA".into())
    );
}

/// Streaming is a property of the definition, and turning it off has to take
/// the triggers with it — a journal nobody drains is a table that only grows.
#[test]
fn redefining_without_stream_removes_the_triggers() {
    let (d, _s) = streamed("toggle");
    d.rretl_map_sync("crm").unwrap();
    d.query("UPDATE customers SET score = 11 WHERE id = 1", &[]).unwrap();
    assert_eq!(backlog(&d), 1);

    // Redefine WITHOUT `stream`: the triggers go, and so does the backlog.
    d.rretl_map_define(MAP).unwrap();
    assert_eq!(backlog(&d), 0, "the journal was cleared with the triggers");
    d.query("UPDATE customers SET score = 12 WHERE id = 1", &[]).unwrap();
    assert_eq!(backlog(&d), 0, "and nothing records writes any more");

    // Back on again, and the write path records once more.
    d.rretl_map_define(&MAP.replace("name = \"crm\"", "name = \"crm\"\nstream = true"))
        .unwrap();
    d.query("UPDATE customers SET score = 13 WHERE id = 1", &[]).unwrap();
    assert_eq!(backlog(&d), 1);

    // And `map drop` leaves nothing behind either.
    d.rretl_map_drop("crm").unwrap();
    d.query("UPDATE customers SET score = 14 WHERE id = 1", &[]).unwrap();
    assert_eq!(backlog(&d), 0);
}

/// A delete on either side is a change like any other — the journal carries
/// OLD's key, so the drain sees the row that is no longer there.
#[test]
fn the_journal_carries_deletes_from_both_sides() {
    let (d, _s) = streamed("deletes");
    d.rretl_map_sync("crm").unwrap();

    d.query("DELETE FROM customers WHERE id = 3", &[]).unwrap();
    assert_eq!(backlog(&d), 1);
    run(&d);
    assert_eq!(ints(&d, "SELECT id FROM crm_customers ORDER BY id"), vec![1, 2]);

    // The other direction: the target loses a row, the source follows.
    d.query("DELETE FROM crm_customers WHERE id = 2", &[]).unwrap();
    run(&d);
    assert_eq!(ints(&d, "SELECT id FROM customers ORDER BY id"), vec![1]);
}

/// The chunk override must be visible to THIS thread and no other.
///
/// `cargo test` runs a file's tests as threads in one process, so the old
/// `std::env::set_var("MPEDB_RRETL_CHUNK", …)` hook was shared mutable state:
/// two tests each setting and clearing it raced, and the loser silently ran at
/// the 4096 default. That is what made the daemon-resume test above fail about
/// one run in three under a full parallel workspace test while passing alone.
#[test]
fn the_chunk_override_does_not_leak_across_threads() {
    let outer = mpedb::rretl::chunk_rows_for_tests();
    let g = mpedb::rretl::ChunkGuard::new(3);
    assert_eq!(mpedb::rretl::chunk_rows_for_tests(), 3);
    // A guard on THIS thread is invisible to another one.
    let seen = std::thread::spawn(mpedb::rretl::chunk_rows_for_tests)
        .join()
        .unwrap();
    assert_eq!(seen, outer, "the override escaped its thread");
    drop(g);
    assert_eq!(mpedb::rretl::chunk_rows_for_tests(), outer, "not restored");
}
