//! Regression: a FAILED schema reload inside `make_write_txn` must release
//! the writer lock. The reload takes a read pin internally, so a full reader
//! table (ReadersFull — designed-for, retryable) reaches that path; before
//! the #109 review it propagated with the lock HELD, leaking it for the
//! process lifetime — every later same-thread write was EDEADLK (or, under a
//! busy policy, a silent terminal Busy), and every other process burned its
//! full timeout per statement. Found by the #109 adversarial review.

use mpedb_core::Engine;
use mpedb_types::{Config, Error};

fn cfg(path: &std::path::Path, max_readers: u32) -> Config {
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = {max_readers}

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "text"
"#,
        path.display()
    );
    Config::from_toml_str(&toml).unwrap()
}

#[test]
fn readersfull_during_stale_reload_leaks_the_writer_lock_and_busyfold_masks_it() {
    let path = std::env::temp_dir().join(format!(
        "mpedb-lock-leak-probe-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let c = cfg(&path, 8);
    let n_tables = c.schema.tables.len();

    // Two engine handles on the same file (the sibling-connection topology).
    let eng_a = Engine::open(&c, vec![vec![]; n_tables]).unwrap();
    let eng_b = Engine::open(&c, vec![vec![]; n_tables]).unwrap();

    // Pin ALL reader slots at the current schema gen (pre-DDL) via handle B.
    let mut pins = Vec::new();
    loop {
        match eng_b.begin_read() {
            Ok(r) => pins.push(r),
            Err(Error::ReadersFull) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    assert!(!pins.is_empty());

    // Handle A commits DDL -> schema_gen bumps; B's bundle is now stale.
    {
        let mut def = c.schema.tables[0].clone();
        def.name = "u".into();
        let mut w = eng_a.begin_write().unwrap();
        w.create_table(def).unwrap();
        w.commit().unwrap();
    }

    // B: begin_write acquires the LOCK, sees the stale gen, reloads -> the
    // reload's internal begin_read hits ReadersFull -> `?` propagates WITHOUT
    // writer_unlock.
    let r1 = eng_b.begin_write_deadline(Some(
        std::time::Instant::now() + std::time::Duration::from_millis(50),
    ));
    match &r1 {
        Ok(_) => eprintln!("first begin_write_deadline: Ok (?)"),
        Err(e) => eprintln!("first begin_write_deadline: Err({e:?})"),
    }
    assert!(matches!(r1, Err(Error::ReadersFull)), "expected ReadersFull");
    drop(r1);

    // Release every pin: contention is GONE. If the lock were correctly
    // released, this begin must succeed.
    for p in pins {
        p.finish().unwrap();
    }

    let r2 = eng_b.begin_write_deadline(Some(
        std::time::Instant::now() + std::time::Duration::from_millis(200),
    ));
    // THE BUG: this answers Busy (the EDEADLK fold) even though nobody else
    // holds anything -- the leaked lock from step 1 is ours. With no deadline
    // it would at least be the loud EDEADLK internal error.
    // With the leak fixed, contention is gone and the lock must be free:
    // both the deadline path and the blocking path acquire.
    assert!(
        r2.is_ok(),
        "writer lock leaked by the failed reload: {:?}",
        r2.err()
    );
    drop(r2);
    eng_b.begin_write().expect("blocking begin_write after failed reload");
    let _ = std::fs::remove_file(&path);
}
