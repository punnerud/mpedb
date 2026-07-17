use super::*;

#[test]
#[ignore]
fn churn_debug() {
    let cfg = debug_cfg();
    let eng = Engine::open(&cfg, vec![vec![]; 1]).unwrap();
    for round in 0..30 {
        let mut w = eng.begin_write().unwrap();
        for i in 0..50 {
            w.insert_row(0, &[Value::Int(i), Value::Text(format!("u{i}@x.no")), Value::Int(round)]).unwrap();
        }
        w.commit().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..50 {
            w.delete_by_pk(0, &[Value::Int(i)]).unwrap();
        }
        w.commit().unwrap();
        // count freelist contents
        let w = eng.begin_write().unwrap();
        let meta = w.meta;
        let mut entries = 0;
        let mut pages = 0;
        if meta.freelist_root != 0 {
            let mut c = btree::cursor(&w, meta.freelist_root, None, None).unwrap();
            while let Some((k, v)) = c.next(&w).unwrap() {
                entries += 1;
                pages += v.len() / 8;
                let _ = k;
            }
        }
        w.abort();
        println!("round {round}: high_water={} freelist_entries={entries} freelist_pages={pages}", meta.high_water);
    }
    let _ = std::fs::remove_file(&cfg.options.path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&cfg.options.path));
    if let Some(dir) = cfg.options.path.parent() {
        let _ = std::fs::remove_dir(dir); // succeeds only once the dir is empty
    }
}

/// Phase-3 ceiling measurement: decompose a serial autocommit PK-point
/// write transaction (durability=none) into lock / execute / commit phases.
/// This bounds what optimistic parallel execution could ever save — only
/// the "execute" phase is even a candidate to move off the writer lock, and
/// the COW-rebase obstacle means most of it is redone on apply anyway.
/// Run: `cargo test -p mpedb-core -- --ignored decompose_write_phases --nocapture`.
#[test]
#[ignore]
fn decompose_write_phases() {
    use std::time::Instant;
    // table with ONLY a PK (the optimistic-eligible class: no secondary
    // index maintenance, exact key-level footprint).
    let path = std::env::temp_dir()
        .join("mpedb-engine-tests")
        .join(format!("decomp-{}.mpedb", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 32\nmax_readers = 64\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\
           [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
           [[table.column]]\n  name = \"v\"\n  type = \"int64\"\n  nullable = false\n",
        path.display()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let eng = Engine::open(&cfg, vec![vec![]]).unwrap();

    const ROWS: i64 = 2000;
    let mut w = eng.begin_write().unwrap();
    for i in 0..ROWS {
        w.insert_row(0, &[Value::Int(i), Value::Int(i)]).unwrap();
    }
    w.commit().unwrap();

    let iters = 20_000u64;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    // warm
    for _ in 0..2000 {
        let key = (next() % ROWS as u64) as i64;
        let mut w = eng.begin_write().unwrap();
        w.update_by_pk(0, &[Value::Int(key), Value::Int(key + 1)]).unwrap();
        w.commit().unwrap();
    }

    let (mut t_begin, mut t_exec, mut t_commit) = (0u128, 0u128, 0u128);
    let whole = Instant::now();
    for _ in 0..iters {
        let key = (next() % ROWS as u64) as i64;
        let val = next() as i64;
        let s = Instant::now();
        let mut w = eng.begin_write().unwrap();
        t_begin += s.elapsed().as_nanos();
        let s = Instant::now();
        w.update_by_pk(0, &[Value::Int(key), Value::Int(val)]).unwrap();
        t_exec += s.elapsed().as_nanos();
        let s = Instant::now();
        w.commit().unwrap();
        t_commit += s.elapsed().as_nanos();
    }
    let total = whole.elapsed().as_nanos();
    let per = |n: u128| n as f64 / iters as f64;
    let pct = |n: u128| 100.0 * n as f64 / total as f64;
    println!("\n=== decompose_write_phases (UPDATE by PK, PK-only table, none) ===");
    println!("iters={iters}  total_per_txn={:.0}ns  ({:.0} txn/s single-thread)",
             per(total), 1e9 / per(total));
    println!("  begin(lock+meta): {:6.0}ns  {:4.1}%", per(t_begin), pct(t_begin));
    println!("  execute(tree COW): {:5.0}ns  {:4.1}%  <- max parallelizable", per(t_exec), pct(t_exec));
    println!("  commit(freelist+flip+unlock): {:.0}ns  {:.1}%", per(t_commit), pct(t_commit));
    println!("  (unaccounted loop/rng): {:.1}%",
             100.0 - pct(t_begin) - pct(t_exec) - pct(t_commit));

    // Split "execute" into the read-traversal (parallelizable in prep,
    // and skippable at apply for a PK-only blind upsert) vs the COW write
    // (unavoidably serial: it must run against the CURRENT committed tree).
    let (mut t_read, mut t_write, mut t_encode) = (0u128, 0u128, 0u128);
    let probe = 20_000u64;
    for _ in 0..probe {
        let key = (next() % ROWS as u64) as i64;
        let val = next() as i64;
        let mut w = eng.begin_write().unwrap();
        // read traversal (what prep does; apply for a PK-only table can skip)
        let s = Instant::now();
        let _ = w.get_by_pk(0, &[Value::Int(key)]).unwrap();
        t_read += s.elapsed().as_nanos();
        // row encode (done in prep, reused at apply)
        let s = Instant::now();
        let payload = row::encode_row(&[Value::Int(key), Value::Int(val)], &eng.col_types(0).unwrap()).unwrap();
        t_encode += s.elapsed().as_nanos();
        // pure COW write: blind Upsert of the pre-encoded payload (this is
        // exactly what an optimistic apply on a PK-only table would run)
        let (root, _) = w.tree_root(0, 0).unwrap();
        let pk = keycode::encode_key(&[Value::Int(key)]);
        let s = Instant::now();
        let out = btree::insert(&mut w, root, &pk, &mut btree::Payload::Flat(&payload), InsertMode::Upsert).unwrap();
        t_write += s.elapsed().as_nanos();
        w.set_tree_root(0, 0, out.new_root, 0);
        w.abort();
    }
    let perp = |n: u128| n as f64 / probe as f64;
    println!("execute split: read_traversal={:.0}ns  encode={:.0}ns  COW_write={:.0}ns",
             perp(t_read), perp(t_encode), perp(t_write));
    let cs_serial = per(t_exec) + per(t_commit) + per(t_begin);
    let cs_optimistic = perp(t_write) + per(t_commit); // blind apply + commit
    println!("critical-section: serial={:.0}ns  optimistic-apply(blind)={:.0}ns  ceiling={:.2}x",
             cs_serial, cs_optimistic, cs_serial / cs_optimistic);

    // Same decomposition for INSERT+DELETE churn (mixed-like).
    let (mut ti_exec, mut ti_commit, mut td_exec, mut td_commit) = (0u128, 0u128, 0u128, 0u128);
    let churn = 5000u64;
    for _ in 0..churn {
        let key = ROWS + (next() % 4000) as i64;
        let mut w = eng.begin_write().unwrap();
        let s = Instant::now();
        let _ = w.insert_row(0, &[Value::Int(key), Value::Int(key)]);
        ti_exec += s.elapsed().as_nanos();
        let s = Instant::now();
        w.commit().unwrap();
        ti_commit += s.elapsed().as_nanos();
        let mut w = eng.begin_write().unwrap();
        let s = Instant::now();
        let _ = w.delete_by_pk(0, &[Value::Int(key)]);
        td_exec += s.elapsed().as_nanos();
        let s = Instant::now();
        w.commit().unwrap();
        td_commit += s.elapsed().as_nanos();
    }
    let perc = |n: u128| n as f64 / churn as f64;
    println!("INSERT: exec={:.0}ns commit={:.0}ns | DELETE: exec={:.0}ns commit={:.0}ns",
             perc(ti_exec), perc(ti_commit), perc(td_exec), perc(td_commit));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::shm::wal_path(&path));
}

fn debug_cfg() -> Config {
    let path = std::env::temp_dir()
        .join("mpedb-engine-tests")
        .join(format!("churn-debug-{}.mpedb", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
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
    Config::from_toml_str(&toml).unwrap()
}
