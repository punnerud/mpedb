//! Parameters too large for an intent slot ride the ring through `<db>-ring`
//! (design/DESIGN.md §5.3), so writers of large values share a flush.
//!
//! The batch is built the way `ring_stmt_atomicity` builds one: a session
//! holds the writer lock while every worker enqueues, so all intents are READY
//! when it lets go.

use mpedb::{params, Config, Database, Durability, ExecResult, Value};
use std::path::PathBuf;
use std::sync::Arc;

const WRITERS: i64 = 8;

fn open(name: &str) -> (Arc<Database>, PathBuf) {
    let path = mpedb_testkit::scratch_base()
        .join(format!("mpedb-ring-spill-{name}-{}.mpedb", std::process::id()));
    for suffix in ["", "-wal", "-ring"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 64
max_readers = 64

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "blob"
"#,
        path.display()
    );
    let mut cfg = Config::from_toml_str(&toml).unwrap();
    cfg.options.durability = Durability::Wal;
    (Arc::new(Database::open_with_config(cfg).unwrap()), path)
}

/// Sizes on both sides of the inline slot, the extent threshold and a page.
fn value(k: i64) -> Vec<u8> {
    let len = [900usize, 3_000, 7_000, 70_000][k as usize % 4] + k as usize;
    (0..len).map(|i| (i as u8) ^ (k as u8).wrapping_mul(31)).collect()
}

/// Returns how many commits the eight inserts took.
fn enqueue_all(db: &Arc<Database>, mid: impl FnOnce()) -> u64 {
    let ins = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)").unwrap();
    let before = db.snapshot_txn();
    let holder = db.begin().unwrap();
    let handles: Vec<_> = (0..WRITERS)
        .map(|k| {
            let db = db.clone();
            std::thread::spawn(move || match db.execute(&ins, &params![k, value(k)]) {
                Ok(ExecResult::Affected(1)) => {}
                other => panic!("insert {k}: {other:?}"),
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(400));
    mid();
    holder.rollback();
    for h in handles {
        h.join().unwrap();
    }
    db.snapshot_txn() - before
}

fn check(db: &Database) {
    db.verify().unwrap();
    let ExecResult::Rows { rows, .. } = db.query("SELECT id, v FROM t", &params![]).unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), WRITERS as usize);
    for r in rows {
        let (Value::Int(k), Value::Blob(v)) = (&r[0], &r[1]) else {
            panic!("unexpected row {r:?}");
        };
        assert_eq!(v, &value(*k), "value {k} must come back as written");
    }
}

#[test]
fn large_values_share_one_commit() {
    let (db, path) = open("batch");
    let commits = enqueue_all(&db, || {});
    check(&db);
    // One leader drained all of them. Without the overflow file each of the
    // eight would have been its own commit.
    assert_eq!(commits, 1, "eight spilled intents, one commit");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The overflow file is wiped while the intents wait. No leader may execute
/// bytes that are not the ones published: each owner gets its intent back and
/// writes it on the direct path.
#[test]
fn a_lost_spill_is_never_executed() {
    let (db, path) = open("lost");
    let ring = format!("{}-ring", path.display());
    let commits = enqueue_all(&db, || {
        let len = std::fs::metadata(&ring).unwrap().len() as usize;
        assert!(len > 0, "the workers must have spilled");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&ring).unwrap();
        f.write_all(&vec![0xA5u8; len]).unwrap();
    });
    check(&db);
    // Every value here is over the slot, so every owner wrote alone, after
    // the round that handed the intents back.
    assert!(commits >= WRITERS as u64, "{commits} commits for {WRITERS} direct writes");
    drop(db);
    let _ = std::fs::remove_file(&path);
}
