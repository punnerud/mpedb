use super::*;
use mpedb_types::Config;

/// A `Config` that takes its database file with it when it dies.
///
/// These tests used to leave the file behind. The name carries the pid, so
/// every run leaked a fresh one and they accumulated forever — and nobody
/// noticed, because a dev box's `/tmp` is enormous. A Raspberry Pi's is a
/// 100 MB tmpfs, and ONE run of this suite left 35 MB in it.
///
/// Derefs to `Config`, so the fourteen call sites did not change, and it
/// cleans up on UNWIND too — a panicking test is exactly when the file is
/// least likely to be removed by a line at the end of the function, which
/// is how the `/dev/shm` version of this bug survived its first fix.
struct TestCfg {
    cfg: Config,
    path: std::path::PathBuf,
}

impl std::ops::Deref for TestCfg {
    type Target = Config;
    fn deref(&self) -> &Config {
        &self.cfg
    }
}

impl Drop for TestCfg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // The WAL sidecar is part of the database; leaving it behind also
        // leaves the next run to open a database beside a foreign log.
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
    }
}

fn test_config(name: &str, size_mb: u64) -> TestCfg {
    let path = std::env::temp_dir()
        .join("mpedb-engine-tests")
        .join(format!("{}-{}.mpedb", name, std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 64

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
"#,
        path.display()
    );
    TestCfg {
        cfg: Config::from_toml_str(&toml).unwrap(),
        path,
    }
}

fn open(cfg: &Config) -> Engine {
    Engine::open(cfg, vec![vec![]; cfg.schema.tables.len()]).unwrap()
}

fn user(id: i64, email: &str, age: Option<i64>) -> Vec<Value> {
    vec![
        Value::Int(id),
        Value::Text(email.into()),
        age.map(Value::Int).unwrap_or(Value::Null),
    ]
}

#[test]
fn crud_cycle_with_constraints() {
    let cfg = test_config("crud", 8);
    let eng = open(&cfg);

    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(1, "a@x.no", Some(30))).unwrap();
    w.insert_row(0, &user(2, "b@x.no", None)).unwrap();
    // duplicate PK
    assert!(matches!(
        w.insert_row(0, &user(1, "c@x.no", None)),
        Err(Error::PrimaryKeyViolation { .. })
    ));
    // duplicate unique email
    assert!(matches!(
        w.insert_row(0, &user(3, "a@x.no", None)),
        Err(Error::UniqueViolation { .. })
    ));
    // NOT NULL
    assert!(matches!(
        w.insert_row(0, &[Value::Int(4), Value::Null, Value::Null]),
        Err(Error::NotNullViolation { .. })
    ));
    // rigid type
    assert!(matches!(
        w.insert_row(0, &[Value::Int(5), Value::Int(9), Value::Null]),
        Err(Error::TypeMismatch(_))
    ));
    w.commit().unwrap();

    // read it back through a snapshot
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get_by_pk(0, &[Value::Int(1)]).unwrap(), Some(user(1, "a@x.no", Some(30))));
    assert_eq!(r.get_by_index(0, 1, &Value::Text("b@x.no".into())).unwrap(),
               Some(user(2, "b@x.no", None)));
    assert_eq!(r.row_count(0).unwrap(), 2);
    r.finish().unwrap();

    // update: change indexed column, old index entry must vanish
    let mut w = eng.begin_write().unwrap();
    assert!(w.update_by_pk(0, &user(1, "a2@x.no", Some(31))).unwrap());
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get_by_index(0, 1, &Value::Text("a@x.no".into())).unwrap(), None);
    assert!(r.get_by_index(0, 1, &Value::Text("a2@x.no".into())).unwrap().is_some());
    r.finish().unwrap();

    // delete
    let mut w = eng.begin_write().unwrap();
    assert!(w.delete_by_pk(0, &[Value::Int(1)]).unwrap());
    assert!(!w.delete_by_pk(0, &[Value::Int(1)]).unwrap());
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get_by_pk(0, &[Value::Int(1)]).unwrap(), None);
    assert_eq!(r.get_by_index(0, 1, &Value::Text("a2@x.no".into())).unwrap(), None);
    assert_eq!(r.row_count(0).unwrap(), 1);
    r.finish().unwrap();

    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn persistence_across_reopen() {
    let cfg = test_config("persist", 8);
    {
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..100 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.commit().unwrap();
    }
    // fresh attach to the same file
    let eng = open(&cfg);
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 100);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(42)]).unwrap(),
        Some(user(42, "u42@x.no", Some(42)))
    );
    r.finish().unwrap();
    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn snapshot_isolation_across_commits() {
    let cfg = test_config("mvcc", 8);
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(1, "a@x.no", Some(1))).unwrap();
    w.commit().unwrap();

    let r = eng.begin_read().unwrap(); // pins txn with exactly row 1

    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(2, "b@x.no", Some(2))).unwrap();
    assert!(w.update_by_pk(0, &user(1, "a@x.no", Some(99))).unwrap());
    w.commit().unwrap();

    // the pinned snapshot must be completely unaffected
    assert_eq!(r.row_count(0).unwrap(), 1);
    assert_eq!(r.get_by_pk(0, &[Value::Int(2)]).unwrap(), None);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
        Some(user(1, "a@x.no", Some(1)))
    );
    r.finish().unwrap();

    // a fresh snapshot sees the new state
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 2);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
        Some(user(1, "a@x.no", Some(99)))
    );
    r.finish().unwrap();
    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn abort_leaves_no_trace_and_no_leak() {
    let cfg = test_config("abort", 8);
    let eng = open(&cfg);
    let before = eng.shm.newest_meta().unwrap();
    let mut w = eng.begin_write().unwrap();
    for i in 0..50 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
    }
    w.abort();
    let after = eng.shm.newest_meta().unwrap();
    assert_eq!(before, after, "abort must not change committed state");
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 0);
    r.finish().unwrap();
    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn freelist_reclaims_pages_under_churn() {
    let cfg = test_config("churn", 8);
    let eng = open(&cfg);
    // steady-state churn: insert+delete the same rows repeatedly; with a
    // working freelist, high_water must stabilize instead of growing
    // until DbFull.
    let mut high_water_after_warmup = 0;
    for round in 0..40 {
        let mut w = eng.begin_write().unwrap();
        for i in 0..50 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(round))).unwrap();
        }
        w.commit().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..50 {
            assert!(w.delete_by_pk(0, &[Value::Int(i)]).unwrap());
        }
        w.commit().unwrap();
        let hw = eng.shm.newest_meta().unwrap().high_water;
        if round == 10 {
            high_water_after_warmup = hw;
        }
        if round > 10 {
            assert!(
                hw <= high_water_after_warmup + 8,
                "high_water grew from {high_water_after_warmup} to {hw} by \
                 round {round}: freelist is not reclaiming"
            );
        }
    }
    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn pinned_reader_blocks_reclaim_until_released() {
    let cfg = test_config("pin-reclaim", 8);
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    for i in 0..200 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
    }
    w.commit().unwrap();

    let r = eng.begin_read().unwrap(); // pin old snapshot
    let mut w = eng.begin_write().unwrap();
    for i in 0..200 {
        w.delete_by_pk(0, &[Value::Int(i)]).unwrap();
    }
    w.commit().unwrap();
    let hw_pinned = eng.shm.newest_meta().unwrap().high_water;

    // while pinned, churn must grow the file (no reclaim of its pages)
    let mut w = eng.begin_write().unwrap();
    for i in 0..100 {
        w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), None)).unwrap();
    }
    w.commit().unwrap();
    assert!(eng.shm.newest_meta().unwrap().high_water > hw_pinned);

    r.finish().unwrap(); // release the pin

    // after release, steady churn reclaims: high_water stabilizes
    let mut stable = eng.shm.newest_meta().unwrap().high_water;
    for round in 0..20 {
        let mut w = eng.begin_write().unwrap();
        for i in 0..100 {
            w.delete_by_pk(0, &[Value::Int(1000 + i)]).unwrap();
        }
        for i in 0..100 {
            w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), None)).unwrap();
        }
        w.commit().unwrap();
        let hw = eng.shm.newest_meta().unwrap().high_water;
        if round >= 5 {
            assert!(hw <= stable + 8, "no reclaim after pin release");
        }
        stable = stable.max(hw);
    }
    std::fs::remove_file(&cfg.options.path).unwrap();
}

#[test]
fn page_accounting_sys_api_and_open_from_file() {
    let cfg = test_config("accounting", 8);
    let eng = open(&cfg);
    // invariant must hold after every kind of commit
    eng.verify_page_accounting().unwrap();
    let mut w = eng.begin_write().unwrap();
    for i in 0..300 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
    }
    w.sys_put(b"plan/abc", b"BLOB-1").unwrap();
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap();

    let mut w = eng.begin_write().unwrap();
    for i in 0..150 {
        w.delete_by_pk(0, &[Value::Int(i * 2)]).unwrap();
    }
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap();

    // sys records readable from snapshots and writers
    let r = eng.begin_read().unwrap();
    assert_eq!(r.sys_get(b"plan/abc").unwrap().unwrap(), b"BLOB-1");
    assert_eq!(r.sys_scan().unwrap().len(), 1);
    // stored schema equals the config schema
    assert_eq!(r.stored_schema().unwrap(), cfg.schema);
    r.finish().unwrap();

    // config-free open sees the same data and schema
    let eng2 = Engine::open_from_file(&cfg.options.path).unwrap();
    assert_eq!(eng2.schema(), &cfg.schema);
    let r = eng2.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 150);
    r.finish().unwrap();

    std::fs::remove_file(&cfg.options.path).unwrap();
}

// ------------------------------------------------- wal durability tests

fn wal_config(name: &str) -> Config {
    wal_class_config(name, "wal")
}

/// WAL-class config with the given durability (`wal` or `async`).
fn wal_class_config(name: &str, durability: &str) -> Config {
    let base = std::path::Path::new("/dev/shm");
    let dir = if base.is_dir() {
        base.join("mpedb-engine-wal-tests")
    } else {
        std::env::temp_dir().join("mpedb-engine-wal-tests")
    };
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-{}.mpedb", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&path));
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 64
durability = "{durability}"

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
"#,
        path.display()
    );
    Config::from_toml_str(&toml).unwrap()
}

fn wal_cleanup(cfg: &Config) {
    let _ = std::fs::remove_file(&cfg.options.path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&cfg.options.path));
    // Drop the shared test dir once the last file is gone — remove_dir
    // only succeeds on an empty directory, so concurrent tests keep it
    // alive and only the final teardown actually removes it.
    if let Some(dir) = cfg.options.path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Regress the mapping to a plausible post-power-loss state: stale lock
/// area (wal_len/wal_ckpt as of `stale_len`/`stale_ckpt`) and both meta
/// slots rolled back to genesis — then replay the log.
fn simulate_reboot_and_recover(eng: &Engine, stale_ckpt: u64, stale_len: u64) -> u64 {
    use std::sync::atomic::Ordering;
    eng.shm.wal_ckpt().store(stale_ckpt, Ordering::Release);
    eng.shm.wal_len().store(stale_len, Ordering::Release);
    let genesis = MetaSnapshot {
        slot: 0,
        extent_map_root: 0,
            txn_id: 0,
        catalog_root: 0,
        freelist_root: 0,
        high_water: eng.shm.data_start,
    };
    eng.shm.write_meta_slot(0, &genesis);
    eng.shm.write_meta_slot(1, &genesis);
    eng.shm.wal_recover().unwrap()
}

#[test]
fn wal_mode_crud_persistence_and_reopen() {
    let cfg = wal_config("crud");
    {
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..100 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap();
        // durable gate: readers see the commit only after the fdatasync
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 100);
        r.finish().unwrap();
    }
    // reattach (no reboot): the mapping is authoritative, no replay needed
    let eng = open(&cfg);
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 100);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(42)]).unwrap(),
        Some(user(42, "u42@x.no", Some(42)))
    );
    r.finish().unwrap();
    wal_cleanup(&cfg);
}

#[test]
fn wal_recovery_rebuilds_engine_state_from_log_alone() {
    let cfg = wal_config("recover");
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    for i in 0..60 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
    }
    w.commit().unwrap();
    let mut w = eng.begin_write().unwrap();
    for i in 0..30 {
        w.delete_by_pk(0, &[Value::Int(i * 2)]).unwrap();
    }
    w.commit().unwrap();

    // power loss that wrote NOTHING volatile back: even both meta slots
    // are gone; the log alone must rebuild the committed state
    simulate_reboot_and_recover(&eng, 0, 0);

    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 30);
    assert_eq!(r.get_by_pk(0, &[Value::Int(0)]).unwrap(), None);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
        Some(user(1, "u1@x.no", Some(1)))
    );
    assert_eq!(
        r.get_by_index(0, 1, &Value::Text("u1@x.no".into())).unwrap(),
        Some(user(1, "u1@x.no", Some(1)))
    );
    r.finish().unwrap();
    eng.verify_page_accounting().unwrap();

    // replay idempotency, engine level: recover again, same state
    simulate_reboot_and_recover(&eng, 0, 0);
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 30);
    r.finish().unwrap();
    eng.verify_page_accounting().unwrap();
    wal_cleanup(&cfg);
}

/// #58: a blob spanning several overflow pages must survive power loss via
/// the WAL alone. The KIND_OVERFLOW arm of the SPLIT page encoding was live
/// in every durable blob write and replayed by NO test until now — this is
/// the end-to-end proof: insert a 5-page blob in wal mode, lose every
/// volatile page and both meta slots, recover from the log, read it back
/// byte-identical, and pass page accounting.
#[test]
fn wal_recovery_replays_overflow_chains_byte_identical() {
    let base = std::path::Path::new("/dev/shm");
    let dir = if base.is_dir() {
        base.join("mpedb-engine-wal-tests")
    } else {
        std::env::temp_dir().join("mpedb-engine-wal-tests")
    };
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("blob-recover-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&path));
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 64
durability = "wal"

[[table]]
name = "files"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "data"
  type = "blob"
"#,
        path.display()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let eng = open(&cfg);

    // ~20 KiB = 5 overflow pages (OVERFLOW_CAP = 4080), deterministic bytes.
    let blob: Vec<u8> = (0..20_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u8)
        .collect();
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &[Value::Int(1), Value::Blob(blob.clone())]).unwrap();
    w.commit().unwrap();

    // Power loss that wrote NOTHING volatile back — the log alone rebuilds.
    simulate_reboot_and_recover(&eng, 0, 0);

    let r = eng.begin_read().unwrap();
    let row = r
        .get_by_pk(0, &[Value::Int(1)])
        .unwrap()
        .expect("the blob row must survive the reboot");
    assert_eq!(
        row[1],
        Value::Blob(blob),
        "the overflow chain must replay byte-identical"
    );
    r.finish().unwrap();
    eng.verify_page_accounting().unwrap();
    wal_cleanup(&cfg);
}

#[test]
fn wal_checkpoint_then_recovery_spans_the_boundary() {
    use std::sync::atomic::Ordering;
    let cfg = wal_config("ckpt");
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    for i in 0..40 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
    }
    w.commit().unwrap();
    // force a checkpoint (threshold 1 byte): main file caught up, ckpt=len
    eng.shm.wal_checkpoint_if(1).unwrap();
    let ckpt = eng.shm.wal_ckpt().load(Ordering::Acquire);
    assert_eq!(ckpt, eng.shm.wal_len().load(Ordering::Acquire));
    assert!(ckpt > 0);

    // post-checkpoint commits...
    let mut w = eng.begin_write().unwrap();
    for i in 40..70 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
    }
    w.commit().unwrap();

    // ...survive a reboot whose lock-area wal_len writeback was lost
    // (metas regressed too); scan starts at the durable ckpt
    let end = simulate_reboot_and_recover(&eng, ckpt, ckpt);
    assert!(end > ckpt, "post-checkpoint records must be replayed");
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 70);
    r.finish().unwrap();
    eng.verify_page_accounting().unwrap();
    wal_cleanup(&cfg);
}

// ---------------------------- async (deferred-fsync WAL) durability tests
//
// The deterministic contract tests (visibility-before-durability, flushed
// recovery, un-flushed torn tail) live at the Shm level (see shm::tests),
// where there is no background flusher to race. This is the full-stack
// integration: real flusher thread + clean-shutdown final flush on Engine
// drop + reopen.

#[test]
fn async_end_to_end_flusher_and_reopen() {
    let cfg = wal_class_config("async-e2e", "async");
    {
        let eng = open(&cfg); // durability=async spawns the flusher
        let mut w = eng.begin_write().unwrap();
        for i in 0..200 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.commit().unwrap();
        // VISIBILITY: observable immediately, without waiting for a flush.
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 200);
        r.finish().unwrap();
        eng.verify_page_accounting().unwrap();
        // Engine drop here stops the flusher AFTER a synchronous final
        // flush — clean shutdown loses nothing (§5.4.2).
    }
    // reattach (no reboot): mapping authoritative, everything persisted
    let eng = open(&cfg);
    let r = eng.begin_read().unwrap();
    assert_eq!(r.row_count(0).unwrap(), 200);
    assert_eq!(
        r.get_by_pk(0, &[Value::Int(150)]).unwrap(),
        Some(user(150, "u150@x.no", Some(150)))
    );
    r.finish().unwrap();
    wal_cleanup(&cfg);
}

#[test]
fn concurrent_readers_and_writer_threads() {
    let cfg = test_config("threads", 16);
    let eng = std::sync::Arc::new(open(&cfg));
    let mut w = eng.begin_write().unwrap();
    // bank invariant: total balance is conserved by transfers
    for i in 0..20 {
        w.insert_row(0, &user(i, &format!("acct{i}@x.no"), Some(1000))).unwrap();
    }
    w.commit().unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::new();
    // 4 reader threads validating the invariant on every snapshot
    for _ in 0..4 {
        let eng = eng.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut checks = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let r = eng.begin_read().unwrap();
                let mut c = r.scan(0, None, None).unwrap();
                let mut sum = 0i64;
                let mut rows = 0;
                while let Some(row) = c.next().unwrap() {
                    if let Value::Int(b) = row[2] {
                        sum += b;
                    }
                    rows += 1;
                }
                assert_eq!(rows, 20, "snapshot must always see all 20 accounts");
                assert_eq!(sum, 20_000, "balance sum must be invariant");
                r.finish().unwrap();
                checks += 1;
            }
            checks
        }));
    }
    // 1 writer thread doing random transfers
    {
        let eng = eng.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut x = 0x12345u64;
            for _ in 0..300 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let from = (x % 20) as i64;
                let to = ((x >> 8) % 20) as i64;
                if from == to {
                    continue;
                }
                let mut w = eng.begin_write().unwrap();
                let a = w.get_by_pk(0, &[Value::Int(from)]).unwrap().unwrap();
                let b = w.get_by_pk(0, &[Value::Int(to)]).unwrap().unwrap();
                let (Value::Int(ab), Value::Int(bb)) = (&a[2], &b[2]) else {
                    panic!()
                };
                let amount = (x % 50) as i64;
                let mut a2 = a.clone();
                let mut b2 = b.clone();
                a2[2] = Value::Int(ab - amount);
                b2[2] = Value::Int(bb + amount);
                w.update_by_pk(0, &a2).unwrap();
                w.update_by_pk(0, &b2).unwrap();
                w.commit().unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            0u64
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    wal_cleanup(&cfg);
}

fn enable_capture(eng: &Engine, tables: &[u32]) {
    let mut cfg = CaptureConfig::default();
    for &t in tables {
        cfg.set_captured(t, true);
    }
    cfg.generation = 1;
    let mut w = eng.begin_write().unwrap();
    w.set_capture(false); // the control write must not capture itself
    w.sys_put(cdc::CDC_TABS_KEY, &cfg.encode()).unwrap();
    w.commit().unwrap();
}

fn dirty(eng: &Engine) -> Vec<DirtyEntry> {
    let r = eng.begin_read().unwrap();
    let raw = r
        .sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END)
        .unwrap();
    r.finish().unwrap();
    raw.iter().map(|(_, v)| DirtyEntry::decode(v).unwrap()).collect()
}

fn set_write_block(eng: &Engine, blocked: &[u32]) {
    let mut cfg = CaptureConfig::default();
    for &t in blocked {
        cfg.set_blocked(t, true);
    }
    cfg.generation = 1;
    let mut w = eng.begin_write().unwrap();
    w.set_capture(false);
    w.sys_put(cdc::CDC_TABS_KEY, &cfg.encode()).unwrap();
    w.commit().unwrap();
}

#[test]
fn cdc_write_block_refuses_typed_mutators_with_no_side_effects() {
    let cfg = test_config("cdcblock", 8);
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(1, "a@x.no", Some(10))).unwrap();
    w.commit().unwrap();

    set_write_block(&eng, &[0]);

    let mut w = eng.begin_write().unwrap();
    assert!(matches!(
        w.insert_row(0, &user(2, "b@x.no", Some(20))),
        Err(Error::Frozen { table_id: 0 })
    ));
    assert!(matches!(
        w.update_by_pk(0, &user(1, "a2@x.no", Some(11))),
        Err(Error::Frozen { table_id: 0 })
    ));
    assert!(matches!(
        w.delete_by_pk(0, &[Value::Int(1)]),
        Err(Error::Frozen { table_id: 0 })
    ));
    drop(w); // abort

    // the seeded row is untouched (the checks fired before any side effect)
    let mut w = eng.begin_write().unwrap();
    assert!(w.get_by_pk(0, &[Value::Int(1)]).unwrap().is_some());
    assert!(w.get_by_pk(0, &[Value::Int(2)]).unwrap().is_none());
    drop(w);

    // clearing the block re-enables writes
    set_write_block(&eng, &[]);
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(2, "b@x.no", Some(20))).unwrap();
    w.commit().unwrap();
    let mut w = eng.begin_write().unwrap();
    assert!(w.get_by_pk(0, &[Value::Int(2)]).unwrap().is_some());
    drop(w);
    eng.verify_page_accounting().unwrap();
}

#[test]
fn reserved_pages_extend_the_alloc_ceiling_past_normal_dbfull() {
    // fill a small db to the NORMAL ceiling in one txn, then prove a
    // reserved-mode allocation still succeeds (the control-plane headroom).
    let cfg = test_config("reservedpool", 2);
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    let mut i = 0i64;
    loop {
        match w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))) {
            Ok(()) => i += 1,
            Err(Error::DbFull) => break,
            Err(e) => panic!("unexpected error while filling: {e}"),
        }
    }
    assert!(i > 0, "db should hold at least some rows");
    // normal mode is now full
    assert!(matches!(
        w.insert_row(0, &user(i, "x@x.no", Some(0))),
        Err(Error::DbFull)
    ));
    // reserved mode reaches into the reserve band and succeeds (the normal
    // ceiling was RESERVED_CONTROL_PAGES below page_count)
    w.set_reserved_alloc(true);
    w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i)))
        .expect("reserved allocation should succeed past the normal ceiling");
    // the flag persists through commit, so a reserved control write's whole
    // commit (COW + freelist fixpoint) also draws from the reserve band
    w.set_reserved_alloc(false);
    w.set_reserved_alloc(true);
    w.sys_put(b"mir\0halt", b"db_full").unwrap();
    w.commit().expect("reserved control write commits past the normal full");
    let r = eng.begin_read().unwrap();
    assert_eq!(r.sys_get(b"mir\0halt").unwrap().unwrap(), b"db_full");
    r.finish().unwrap();
}

#[test]
fn cdc_write_block_refuses_optimistic_blind_apply() {
    let path = std::env::temp_dir()
        .join("mpedb-engine-tests")
        .join(format!("cdcblockopt-{}.mpedb", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 64\n\
         [[table]]\nname = \"kv\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"v\"\ntype = \"int64\"\n",
        path.display()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let eng = Engine::open(&cfg, vec![vec![]]).unwrap();
    set_write_block(&eng, &[0]);

    let key = keycode::encode_key(&[Value::Int(7)]);
    let payload =
        row::encode_row(&[Value::Int(7), Value::Int(1)], &[ColumnType::Int64; 2]).unwrap();
    let mut w = eng.begin_write().unwrap();
    assert!(matches!(
        w.optimistic_insert(0, &key, &payload),
        Err(Error::Frozen { table_id: 0 })
    ));
    assert!(matches!(
        w.optimistic_upsert(0, &key, &payload),
        Err(Error::Frozen { table_id: 0 })
    ));
    assert!(matches!(
        w.optimistic_delete(0, &key),
        Err(Error::Frozen { table_id: 0 })
    ));
    drop(w);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&path));
}

#[test]
fn cdc_capture_hooks_all_typed_mutators() {
    let cfg = test_config("cdccap", 8);
    let eng = open(&cfg);

    // no capture configured → writes leave no dirty entries
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(1, "a@x.no", Some(10))).unwrap();
    w.commit().unwrap();
    assert_eq!(dirty(&eng).len(), 0);

    enable_capture(&eng, &[0]);
    eng.verify_page_accounting().unwrap(); // A

    // insert → one Upsert entry keyed by the PK keycode
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &user(2, "b@x.no", Some(20))).unwrap();
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap(); // B
    let d = dirty(&eng);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].op, DirtyOp::Upsert);
    assert_eq!(d[0].pk_keycode, keycode::encode_key(&[Value::Int(2)]));

    // update same PK coalesces (still one, still Upsert)
    let mut w = eng.begin_write().unwrap();
    w.update_by_pk(0, &user(2, "b2@x.no", Some(21))).unwrap();
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap(); // C
    let d = dirty(&eng);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].op, DirtyOp::Upsert);

    // delete flips the coalesced entry to a tombstone
    let mut w = eng.begin_write().unwrap();
    assert!(w.delete_by_pk(0, &[Value::Int(2)]).unwrap());
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap(); // D
    let d = dirty(&eng);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].op, DirtyOp::Delete);

    // a suppressed replication-plane write captures nothing
    let mut w = eng.begin_write().unwrap();
    w.set_capture(false);
    w.insert_row(0, &user(3, "c@x.no", Some(30))).unwrap();
    w.commit().unwrap();
    assert_eq!(dirty(&eng).len(), 1); // still just PK=2's tombstone

    // savepoint rollback unwinds a captured dirty entry (COW §3.4). This
    // also exercises capture-triggered refill inside a savepoint (the
    // rollback_to reusable/freelist-root consistency fix).
    let mut w = eng.begin_write().unwrap();
    let sp = w.savepoint();
    w.insert_row(0, &user(4, "d@x.no", Some(40))).unwrap();
    assert_eq!(
        w.sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END).unwrap().len(),
        2
    );
    w.rollback_to(sp);
    assert_eq!(
        w.sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END).unwrap().len(),
        1
    );
    w.commit().unwrap();
    assert_eq!(dirty(&eng).len(), 1);

    eng.verify_page_accounting().unwrap();
}

#[test]
fn savepoint_rollback_after_refill_keeps_accounting_exact() {
    // Regression (found via the CDC hook): when refill_reusable runs INSIDE
    // a savepoint it pulls committed-freelist pages into `reusable` and
    // deletes their freelist entry; rollback_to must restore both `reusable`
    // and `freelist_root` together or those pages get listed twice.
    let cfg = test_config("sprefill", 8);
    let eng = open(&cfg);
    let mut w = eng.begin_write().unwrap();
    for i in 0..400 {
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
    }
    w.commit().unwrap();
    let mut w = eng.begin_write().unwrap();
    for i in 0..400 {
        w.delete_by_pk(0, &[Value::Int(i)]).unwrap();
    }
    w.commit().unwrap();
    // tiny commits with no live reader advance the oldest-pinned bound past
    // the delete, making its freed pages reclaimable by refill
    for _ in 0..2 {
        let mut w = eng.begin_write().unwrap();
        w.sys_put(b"tick", b"x").unwrap();
        w.commit().unwrap();
    }
    eng.verify_page_accounting().unwrap();

    // allocate heavily INSIDE a savepoint (forces refill), then roll back
    let mut w = eng.begin_write().unwrap();
    let sp = w.savepoint();
    for i in 0..400 {
        w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), Some(i))).unwrap();
    }
    w.rollback_to(sp);
    w.commit().unwrap();
    eng.verify_page_accounting().unwrap();
}

#[test]
fn cdc_capture_hooks_optimistic_blind_apply() {
    // a table with no secondary index, so the optimistic trio is legal
    let path = std::env::temp_dir()
        .join("mpedb-engine-tests")
        .join(format!("cdcopt-{}.mpedb", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 64\n\
         [[table]]\nname = \"kv\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"v\"\ntype = \"int64\"\n",
        path.display()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let eng = Engine::open(&cfg, vec![vec![]]).unwrap();
    enable_capture(&eng, &[0]);

    let key = keycode::encode_key(&[Value::Int(7)]);
    let payload =
        row::encode_row(&[Value::Int(7), Value::Int(100)], &[ColumnType::Int64; 2]).unwrap();

    let mut w = eng.begin_write().unwrap();
    assert!(w.optimistic_insert(0, &key, &payload).unwrap());
    w.commit().unwrap();
    let d = dirty(&eng);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].op, DirtyOp::Upsert);

    let mut w = eng.begin_write().unwrap();
    assert!(w.optimistic_delete(0, &key).unwrap());
    w.commit().unwrap();
    assert_eq!(dirty(&eng)[0].op, DirtyOp::Delete);

    eng.verify_page_accounting().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&path));
}

#[test]
fn sys_scan_range_is_prefix_bounded_and_txn_id_tracks_commits() {
    let cfg = test_config("sysrange", 8);
    let eng = open(&cfg);

    let mut w = eng.begin_write().unwrap();
    // several families sharing the sys region
    w.sys_put(b"cdc\0d/\x00\x00\x00\x00A", b"1").unwrap();
    w.sys_put(b"cdc\0d/\x00\x00\x00\x00B", b"2").unwrap();
    w.sys_put(b"cdc\0tabs", b"T").unwrap();
    w.sys_put(b"plan/xyz", b"P").unwrap();
    w.sys_put(b"mir\0epoch", b"E").unwrap();
    w.commit().unwrap();

    // scan just the cdc dirty family [cdc\0d/, cdc\0d0): 0x30 ('0') is the
    // byte after '/' (0x2f), an exclusive upper bound past every d/ key.
    let r = eng.begin_read().unwrap();
    let dirty = r.sys_scan_range(b"cdc\0d/", b"cdc\0d0").unwrap();
    assert_eq!(dirty.len(), 2, "only the two d/ entries, not tabs/plan/mir");
    assert_eq!(dirty[0].0, b"cdc\0d/\x00\x00\x00\x00A");
    assert_eq!(dirty[1].1, b"2");
    assert_eq!(r.sys_scan().unwrap().len(), 5); // whole region still intact
    let t_after = r.txn_id();
    r.finish().unwrap();

    // txn_id advances by exactly one per commit
    let mut w = eng.begin_write().unwrap();
    assert_eq!(w.meta.txn_id, t_after);
    w.sys_put(b"cdc\0d/\x00\x00\x00\x00C", b"3").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.txn_id(), t_after + 1);
    // writer-side prefix scan agrees with the reader
    let mut w = eng.begin_write().unwrap();
    assert_eq!(w.sys_scan_range(b"cdc\0d/", b"cdc\0d0").unwrap().len(), 3);
    drop(w);
    r.finish().unwrap();
    eng.verify_page_accounting().unwrap();
}
