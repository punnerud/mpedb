//! N5 (pre-existing, found by the N2-fase-4 adversarial review, repro'd
//! identically on the PRE-journal HEAD): an extent-backed payload freed
//! after a savepoint and RECYCLED by a later insert was pwritten over —
//! extent bytes live in no page journal, and above the 256 KiB coalescing
//! buffer nothing restorable held them — so ROLLBACK TO restored the
//! allocator map pointing at clobbered offsets: a silent, committed wrong
//! answer. The fix PARKS own-freed runs while any full savepoint is open
//! (they rejoin the pool when the last layer closes, or at commit; the
//! ExtentSnapshot carries the parked set so rollback restores it exactly).

use mpedb::{Config, Database, ExecResult, Value};

fn filedb(dir: &std::path::Path) -> Database {
    let p = dir.join("t.mpedb");
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 128\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"txt\"\ntype = \"text\"\n",
        p.display().to_string().replace('\\', "/")
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn text_of(db: &Database, s: &mut mpedb::WriteSession<'_>, id: i64) -> String {
    let _ = db;
    match s.query("SELECT txt FROM t WHERE id = $1", &[Value::Int(id)]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            o => panic!("odd {o:?}"),
        },
        _ => panic!(),
    }
}

/// The review's exact repro: >256 KiB payload (past the coalescing buffer),
/// freed after the savepoint, recycled by the next insert, rolled back.
#[test]
fn recycled_extent_run_survives_rollback_to() {
    let dir = tempdir();
    let db = filedb(&dir);
    let a = "A".repeat(300_000);
    let b = "B".repeat(300_000);
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES ($1, $2)", &[Value::Int(1), Value::Text(a.clone())])
        .unwrap();
    s.query("SAVEPOINT s1", &[]).unwrap();
    s.query("DELETE FROM t WHERE id = $1", &[Value::Int(1)]).unwrap();
    s.query("INSERT INTO t VALUES ($1, $2)", &[Value::Int(2), Value::Text(b.clone())])
        .unwrap();
    s.query("ROLLBACK TO SAVEPOINT s1", &[]).unwrap();
    assert_eq!(text_of(&db, &mut s, 1), a, "payload must be row 1's bytes, not row 2's");
    s.commit().unwrap();
    match db.query("SELECT txt FROM t WHERE id = $1", &[Value::Int(1)]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Text(a.clone())),
        _ => panic!(),
    }
    db.verify().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

/// Parked runs must DRAIN: after the last savepoint closes the space is
/// allocatable again (RELEASE road), and a commit with a savepoint still
/// open reclaims it too (commit road) — otherwise every in-scope free would
/// leak its run for good, the #37 class.
#[test]
fn parked_runs_drain_on_release_and_commit() {
    let dir = tempdir();
    let db = filedb(&dir);
    let big = "C".repeat(300_000);
    // RELEASE road: free under a savepoint, release it, re-insert — the new
    // payload may land where the old one lived; content must be exact.
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES ($1, $2)", &[Value::Int(10), Value::Text(big.clone())])
        .unwrap();
    s.query("SAVEPOINT sp", &[]).unwrap();
    s.query("DELETE FROM t WHERE id = $1", &[Value::Int(10)]).unwrap();
    s.query("RELEASE SAVEPOINT sp", &[]).unwrap();
    s.query("INSERT INTO t VALUES ($1, $2)", &[Value::Int(11), Value::Text(big.clone())])
        .unwrap();
    assert_eq!(text_of(&db, &mut s, 11), big);
    s.commit().unwrap();
    // COMMIT road: free under a savepoint that stays open through commit.
    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT sp2", &[]).unwrap();
    s.query("DELETE FROM t WHERE id = $1", &[Value::Int(11)]).unwrap();
    s.query("INSERT INTO t VALUES ($1, $2)", &[Value::Int(12), Value::Text(big.clone())])
        .unwrap();
    s.commit().unwrap();
    match db.query("SELECT txt FROM t WHERE id = $1", &[Value::Int(12)]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Text(big.clone())),
        _ => panic!(),
    }
    db.verify().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let d = mpedb_testkit::scratch_base().join(format!(
        "mpedb-extsp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
