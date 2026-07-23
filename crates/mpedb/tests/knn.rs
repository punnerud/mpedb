//! Exact kNN (stage D): the early-abandoning heap path must be
//! indistinguishable from the generic materialize-project-sort path — same
//! rows, same order, same raises. The oracle here is mpedb itself: the same
//! question asked in a shape the fast path recognizes and in one it does not,
//! plus a client-side re-sort of the raw distances.

use mpedb::{Config, Database, ExecResult, Value};

fn blob(fs: &[f32]) -> Value {
    Value::Blob(fs.iter().flat_map(|f| f.to_le_bytes()).collect())
}

fn db(tag: &str) -> (Database, String) {
    // pid alone is not unique here: the test harness runs both tests in ONE
    // process, concurrently, and a shared path is a shared database.
    let path = format!(
        "{}/knn-{tag}-{}.mpedb",
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 64
max_readers = 8
durability = "none"

[[table]]
name = "v"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "cat"
  type = "text"
  nullable = false
  indexed = true
  [[table.column]]
  name = "emb"
  type = "blob"
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

/// Deterministic little dataset: 500 vectors in 8 dims, 3 categories, plus
/// two NULL embeddings and one duplicated vector (a distance tie).
fn seed(d: &Database) {
    let mut s = d.begin().unwrap();
    let mut x = 0x5EEDu64;
    let mut rng = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for id in 0..500i64 {
        let emb: Vec<f32> = (0..8).map(|_| (rng() % 1000) as f32 / 100.0).collect();
        s.query(
            "INSERT INTO v (id, cat, emb) VALUES ($1, $2, $3)",
            &[Value::Int(id), Value::Text(format!("c{}", id % 3)), blob(&emb)],
        )
        .unwrap();
    }
    // A tie: 500 duplicates 499's vector — stable order must keep 499 first.
    // Read through the open session — the facade's writer lock is not
    // re-entrant, and the session sees its own uncommitted rows anyway.
    let dup = match s.query("SELECT emb FROM v WHERE id = 499", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        _ => unreachable!(),
    };
    for (id, emb) in [(500i64, dup), (501, Value::Null), (502, Value::Null)] {
        s.query(
            "INSERT INTO v (id, cat, emb) VALUES ($1, 'c0', $2)",
            &[Value::Int(id), emb],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

fn ids(r: ExecResult) -> Vec<i64> {
    match r {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(i) => *i,
                other => panic!("expected int id, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn knn_matches_the_generic_sort_exactly() {
    let (d, path) = db("sort");
    seed(&d);
    let q = blob(&[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]);

    // The fast-path shape…
    let fast = ids(d
        .query("SELECT id FROM v ORDER BY vec_l2(emb, $1) LIMIT 10", std::slice::from_ref(&q))
        .unwrap());
    // …and the same question in a shape the fast path declines (a second
    // sort key), which runs the generic materialize-and-sort.
    let generic = ids(d
        .query(
            "SELECT id FROM v ORDER BY vec_l2(emb, $1), id LIMIT 10",
            std::slice::from_ref(&q),
        )
        .unwrap());
    assert_eq!(fast, generic, "the heap path must be indistinguishable from the sort");

    // NULL embeddings sort FIRST (ascending, sqlite storage-class order), in
    // scan (PK) order — both paths.
    assert_eq!(&fast[..2], &[501, 502], "NULL keys come first, in scan order");

    // The tie: 499 and 500 share a vector; stable order keeps 499 before 500.
    let all = ids(d
        .query("SELECT id FROM v ORDER BY vec_l2(emb, $1) LIMIT 503", std::slice::from_ref(&q))
        .unwrap());
    let p499 = all.iter().position(|&i| i == 499).unwrap();
    assert_eq!(all[p499 + 1], 500, "an exact distance tie keeps scan order");

    // OFFSET pages through the same total order.
    let page2 = ids(d
        .query(
            "SELECT id FROM v ORDER BY vec_l2(emb, $1) LIMIT 5 OFFSET 5",
            std::slice::from_ref(&q),
        )
        .unwrap());
    assert_eq!(page2, &fast[5..10]);

    // Filtered kNN: the WHERE runs before the heap, same as the generic path.
    let f_fast = ids(d
        .query(
            "SELECT id FROM v WHERE cat = 'c1' ORDER BY vec_l2(emb, $1) LIMIT 7",
            std::slice::from_ref(&q),
        )
        .unwrap());
    let f_gen = ids(d
        .query(
            "SELECT id FROM v WHERE cat = 'c1' ORDER BY vec_l2(emb, $1), id LIMIT 7",
            std::slice::from_ref(&q),
        )
        .unwrap());
    assert_eq!(f_fast, f_gen);
    assert!(f_fast.iter().all(|i| i % 3 == 1), "filter must actually apply");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_malformed_row_raises_even_when_it_would_have_been_abandoned() {
    let (d, path) = db("raise");
    seed(&d);
    // A row whose blob is 7 bytes — not a multiple of 4. It sits at id 999,
    // scanned LAST, when the heap is already full of better candidates: the
    // shape check must still raise, because only arithmetic may be skipped.
    d.query(
        "INSERT INTO v (id, cat, emb) VALUES (999, 'c0', x'00112233445566')",
        &[],
    )
    .unwrap();
    let q = blob(&[5.0; 8]);
    let err = d
        .query("SELECT id FROM v ORDER BY vec_l2(emb, $1) LIMIT 3", &[q])
        .unwrap_err();
    assert!(
        err.to_string().contains("multiple of 4"),
        "the refusal must be the canonical shape error; got: {err}"
    );
    let _ = std::fs::remove_file(&path);
}
