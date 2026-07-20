//! #78 cold-data tiering v1: drain + read-back.
//!
//! The zero-wrong-answers contract: a drained + read-back dataset must be
//! ROW-IDENTICAL to the pre-drain dataset — checked here by comparing the
//! full sorted rowset before the drain against (hot rows ∪ cold rows) after,
//! both directly and through the `ATTACH` cross-file read path (#51).
//! Plus the reconcile/conflict semantics the crash protocol depends on.

use mpedb::{Config, Database, Durability, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn scratch(tag: &str) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".to_string()
    } else {
        std::env::temp_dir().to_string_lossy().into_owned()
    };
    format!(
        "{dir}/mpedb-tier-{tag}-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

struct Files(Vec<String>);
impl Drop for Files {
    fn drop(&mut self) {
        for f in &self.0 {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// events(id pk, ts timestamp, kind text indexed, score float, payload blob,
/// note text nullable, ok bool) — every storable type class, a secondary
/// index, and NULLs, so the drain's identity checks are exercised across the
/// whole row codec.
fn hot_db(path: &str) -> Database {
    let toml = format!(
        r#"[database]
path = "{path}"
size_mb = 16
max_readers = 8

[[table]]
name = "events"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "ts"
  type = "timestamp"
  nullable = false

  [[table.column]]
  name = "kind"
  type = "text"
  nullable = false
  indexed = true

  [[table.column]]
  name = "score"
  type = "float64"
  nullable = false

  [[table.column]]
  name = "payload"
  type = "blob"
  nullable = false

  [[table.column]]
  name = "note"
  type = "text"
  nullable = true

  [[table.column]]
  name = "ok"
  type = "bool"
  nullable = false
"#
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

/// Deterministic row `i` — recomputable, so "row-identical" has one source of
/// truth. NULL note on every third row; NaN score on i == 17 (bit-identity).
fn row(i: i64) -> Vec<Value> {
    Vec::from([
        Value::Int(i),
        Value::Timestamp(1_700_000_000_000_000 + i * 86_400_000_000),
        Value::Text(format!("k{}", i % 5)),
        if i == 17 {
            Value::Float(f64::NAN)
        } else {
            Value::Float(i as f64 * 1.5)
        },
        Value::Blob(vec![(i % 251) as u8; (i % 7 + 1) as usize]),
        if i % 3 == 0 {
            Value::Null
        } else {
            Value::Text(format!("note-{i}"))
        },
        Value::Bool(i % 2 == 0),
    ])
}

fn seed(db: &Database, n: i64) {
    let ins = db
        .prepare("INSERT INTO events (id, ts, kind, score, payload, note, ok) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .unwrap();
    let mut s = db.begin().unwrap();
    for i in 0..n {
        s.execute(&ins, &row(i)).unwrap();
    }
    s.commit().unwrap();
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Bit-exact row comparison (floats by bits — NaN must roundtrip).
fn assert_rowsets_identical(mut a: Vec<Vec<Value>>, mut b: Vec<Vec<Value>>, what: &str) {
    let key = |r: &Vec<Value>| match &r[0] {
        Value::Int(i) => *i,
        v => panic!("non-int id {v:?}"),
    };
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(a.len(), b.len(), "{what}: row COUNT diverged");
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.len(), y.len(), "{what}: arity diverged at id {:?}", x[0]);
        for (i, (u, v)) in x.iter().zip(y).enumerate() {
            let same = match (u, v) {
                (Value::Float(f), Value::Float(g)) => f.to_bits() == g.to_bits(),
                _ => u == v,
            };
            assert!(same, "{what}: id {:?} col {i}: {u:?} != {v:?}", x[0]);
        }
    }
}

const SEL: &str = "SELECT id, ts, kind, score, payload, note, ok FROM events";

/// The v1 vertical slice end to end: drain by predicate, then prove the
/// drained + read-back dataset row-identical to the pre-drain dataset — via
/// direct reads AND via `ATTACH` + cross-file UNION ALL — and that the same
/// drain re-run is a no-op.
#[test]
fn drain_roundtrip_is_row_identical() {
    let hp = scratch("rt-hot") + ".mpedb";
    let cp = scratch("rt-cold") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone()]);
    let hot = hot_db(&hp);
    seed(&hot, 100);
    let before = rows_of(&hot, SEL);
    assert_eq!(before.len(), 100);

    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "events", 16 << 20, Durability::None)
        .unwrap();
    // ts < threshold ⇒ ids 0..60 drain; batch 17 forces several txn pairs
    // (and a final partial batch).
    let report = hot
        .tier_drain(
            &cold,
            "events",
            "ts < $1",
            &[Value::Timestamp(1_700_000_000_000_000 + 60 * 86_400_000_000)],
            17,
        )
        .unwrap();
    assert_eq!(report.moved, 60);
    assert_eq!(report.reconciled, 0);
    assert_eq!(report.batches, 4, "60 rows at batch 17 = 3 full + 1 partial");

    // No row lost, none duplicated, none altered — direct reads.
    let hot_rows = rows_of(&hot, SEL);
    let cold_rows = rows_of(&cold, SEL);
    assert_eq!(hot_rows.len(), 40);
    assert_eq!(cold_rows.len(), 60);
    let mut union: Vec<Vec<Value>> = hot_rows.clone();
    union.extend(cold_rows.clone());
    assert_rowsets_identical(before.clone(), union, "drain roundtrip (direct)");
    assert_rowsets_identical(
        cold_rows.clone(),
        (0..60).map(row).collect(),
        "cold content vs recomputed seed",
    );

    // Read-back rides ATTACH (#51): the documented v1 union query.
    hot.query(&format!("ATTACH DATABASE '{cp}' AS cold"), &[]).unwrap();
    let attached = rows_of(
        &hot,
        "SELECT id, ts, kind, score, payload, note, ok FROM events \
         UNION ALL \
         SELECT id, ts, kind, score, payload, note, ok FROM cold.events",
    );
    assert_rowsets_identical(before, attached, "drain roundtrip (ATTACH read-back)");
    // And the secondary index in cold answers point probes (index carried over).
    let probe = rows_of(&cold, "SELECT id FROM events WHERE kind = 'k3' ORDER BY id");
    let expect: Vec<Vec<Value>> = (0..60).filter(|i| i % 5 == 3).map(|i| vec![Value::Int(i)]).collect();
    assert_eq!(probe, expect, "cold secondary index probe");

    // Idempotence: the same drain again moves nothing.
    let again = hot
        .tier_drain(
            &cold,
            "events",
            "ts < $1",
            &[Value::Timestamp(1_700_000_000_000_000 + 60 * 86_400_000_000)],
            17,
        )
        .unwrap();
    assert_eq!(again, mpedb::TierReport::default());
}

/// The crash window's landing state, reproduced deliberately: rows already in
/// cold (identical) while still in hot — exactly what a SIGKILL between the
/// cold commit and the hot commit leaves. Re-running the drain must count
/// them `reconciled`, delete them from hot, and not duplicate them in cold.
#[test]
fn drain_reconciles_an_interrupted_handoff() {
    let hp = scratch("rc-hot") + ".mpedb";
    let cp = scratch("rc-cold") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone()]);
    let hot = hot_db(&hp);
    seed(&hot, 30);
    let before = rows_of(&hot, SEL);
    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "events", 16 << 20, Durability::None)
        .unwrap();

    // Simulate the crashed first phase: ids 0..10 landed in cold, hot intact.
    let ins = cold
        .prepare("INSERT INTO events (id, ts, kind, score, payload, note, ok) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .unwrap();
    let mut s = cold.begin().unwrap();
    for i in 0..10 {
        s.execute(&ins, &row(i)).unwrap();
    }
    s.commit().unwrap();

    let report = hot.tier_drain(&cold, "events", "id < 20", &[], 64).unwrap();
    assert_eq!(report.moved, 10, "ids 10..20 were only in hot");
    assert_eq!(report.reconciled, 10, "ids 0..10 were already landed");

    let mut union = rows_of(&hot, SEL);
    assert_eq!(union.len(), 10);
    let cold_rows = rows_of(&cold, SEL);
    assert_eq!(cold_rows.len(), 20, "no duplicates from the reconcile");
    union.extend(cold_rows);
    assert_rowsets_identical(before, union, "reconcile");
}

/// A DIFFERENT row under the same PK in cold is an explicit refusal — never
/// an overwrite in either direction, and the hot side loses nothing.
#[test]
fn drain_refuses_a_cold_conflict_and_changes_nothing() {
    let hp = scratch("cf-hot") + ".mpedb";
    let cp = scratch("cf-cold") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone()]);
    let hot = hot_db(&hp);
    seed(&hot, 10);
    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "events", 16 << 20, Durability::None)
        .unwrap();
    // An archived predecessor: same PK 3, different content.
    let mut clash = row(3);
    clash[3] = Value::Float(-999.0);
    let ins = cold
        .prepare("INSERT INTO events (id, ts, kind, score, payload, note, ok) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .unwrap();
    let mut s = cold.begin().unwrap();
    s.execute(&ins, &clash).unwrap();
    s.commit().unwrap();

    let hot_before = rows_of(&hot, SEL);
    let cold_before = rows_of(&cold, SEL);
    let err = hot.tier_drain(&cold, "events", "id < 5", &[], 64).unwrap_err();
    assert!(
        err.to_string().contains("conflict"),
        "want an explicit conflict error, got: {err}"
    );
    assert_rowsets_identical(hot_before, rows_of(&hot, SEL), "hot untouched after conflict");
    assert_rowsets_identical(cold_before, rows_of(&cold, SEL), "cold untouched after conflict");
}

/// Composite (multi-column) PK: identity and deletes must key on the full PK.
#[test]
fn drain_handles_composite_primary_keys() {
    let hp = scratch("cpk-hot") + ".mpedb";
    let cp = scratch("cpk-cold") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone()]);
    let toml = format!(
        r#"[database]
path = "{hp}"
size_mb = 16

[[table]]
name = "m"
primary_key = ["a", "b"]

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"

  [[table.column]]
  name = "v"
  type = "int64"
  nullable = false
"#
    );
    let hot = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for a in 0..6i64 {
        for b in ["x", "y"] {
            hot.query(
                "INSERT INTO m (a, b, v) VALUES ($1, $2, $3)",
                &[Value::Int(a), Value::Text(b.into()), Value::Int(a * 10)],
            )
            .unwrap();
        }
    }
    let before = rows_of(&hot, "SELECT a, b, v FROM m");
    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "m", 16 << 20, Durability::None)
        .unwrap();
    let report = hot
        .tier_drain(&cold, "m", "a < 3 AND b = 'x'", &[], 2)
        .unwrap();
    assert_eq!(report.moved, 3);
    let mut union = rows_of(&hot, "SELECT a, b, v FROM m");
    assert_eq!(union.len(), 9);
    union.extend(rows_of(&cold, "SELECT a, b, v FROM m"));
    // sort key: (a, b) — reuse the id-keyed helper by prefix uniqueness of a*2+b
    union.sort_by(|r, s| format!("{r:?}").cmp(&format!("{s:?}")));
    let mut want = before;
    want.sort_by(|r, s| format!("{r:?}").cmp(&format!("{s:?}")));
    assert_eq!(union, want, "composite-PK roundtrip");
}

/// The v1 refusals fail loudly by name, and a schema drift between hot and
/// cold is a hard error before anything moves.
#[test]
fn drain_refusals_are_explicit() {
    let hp = scratch("rf-hot") + ".mpedb";
    let cp = scratch("rf-cold") + ".mpedb";
    let cp2 = scratch("rf-cold2") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone(), cp2.clone()]);
    let hot = hot_db(&hp);
    seed(&hot, 5);

    // Unknown table.
    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "events", 16 << 20, Durability::None)
        .unwrap();
    let e = hot.tier_drain(&cold, "nope", "1", &[], 8).unwrap_err();
    assert!(e.to_string().contains("no table `nope`"), "{e}");

    // Schema drift: a cold `events` with a different shape.
    let toml = format!(
        r#"[database]
path = "{cp2}"
size_mb = 16

[[table]]
name = "events"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "other"
  type = "text"
  nullable = true
"#
    );
    let drifted = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let e = hot.tier_drain(&drifted, "events", "1", &[], 8).unwrap_err();
    assert!(e.to_string().contains("differs between hot and cold"), "{e}");

    // Hot DELETE trigger: the drain would skip it — refuse by name.
    hot.query(
        "CREATE TRIGGER t_del AFTER DELETE ON events BEGIN \
         UPDATE events SET note = 'gone' WHERE id = -1; END",
        &[],
    )
    .unwrap();
    let e = hot.tier_drain(&cold, "events", "id < 3", &[], 8).unwrap_err();
    assert!(e.to_string().contains("DELETE triggers"), "{e}");
    assert_eq!(rows_of(&hot, SEL).len(), 5, "refusal moved nothing");

    // tier_create_cold refuses to clobber an existing file.
    let e = match hot.tier_create_cold(std::path::Path::new(&cp), "events", 16 << 20, Durability::None)
    {
        Err(e) => e,
        Ok(_) => panic!("tier_create_cold clobbered an existing file"),
    };
    assert!(e.to_string().contains("already exists"), "{e}");
}

/// An implicit-rowid table (#94): `SELECT *` hides the rowid, but the drain
/// must carry it — identity, not just shape. The drained rows keep their
/// rowids in cold, so read-back by rowid answers identically.
#[test]
fn drain_carries_hidden_rowids() {
    let hp = scratch("rid-hot") + ".mpedb";
    let cp = scratch("rid-cold") + ".mpedb";
    let _f = Files(vec![hp.clone(), cp.clone()]);
    let toml = format!(
        "[database]\npath = \"{hp}\"\nsize_mb = 16\n\n[[table]]\nname = \"seed\"\n\
         primary_key = [\"id\"]\n[[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let hot = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    hot.query("CREATE TABLE logs (msg TEXT)", &[]).unwrap();
    for i in 0..10i64 {
        hot.query("INSERT INTO logs (msg) VALUES ($1)", &[Value::Text(format!("m{i}"))])
            .unwrap();
    }
    let before = rows_of(&hot, "SELECT rowid, msg FROM logs");
    let cold = hot
        .tier_create_cold(std::path::Path::new(&cp), "logs", 16 << 20, Durability::None)
        .unwrap();
    let report = hot.tier_drain(&cold, "logs", "rowid <= 5", &[], 3).unwrap();
    assert_eq!(report.moved, 5);
    let mut union = rows_of(&hot, "SELECT rowid, msg FROM logs");
    union.extend(rows_of(&cold, "SELECT rowid, msg FROM logs"));
    assert_rowsets_identical(before, union, "hidden-rowid roundtrip");
}
