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
    std::env::set_var("MPEDB_RRETL_CHUNK", "7");
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
    std::env::remove_var("MPEDB_RRETL_CHUNK");
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
