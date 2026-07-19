//! #47 stage 4: `DROP TABLE` end to end — the dropped table's rows and name
//! disappear, its pages are reclaimed (page accounting balances, and churn
//! stays bounded), its id is never reused (a re-created same-name table is a
//! fresh empty table on a new slot), other tables are untouched, the change
//! persists across reopen, and a second process sees the drop on its next
//! statement (schema-gen reload). `IF EXISTS` matches sqlite/PG.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn config(name: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-droptable-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // Seed with ONE table (`users`, id 0). Everything else is created live.
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 32

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "name"
  type = "text"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match &rows(db.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

#[test]
fn drop_removes_the_table_and_reclaims_pages() {
    let (cfg, path) = config("basic");
    let db = Database::open_with_config(cfg).unwrap();

    db.query("CREATE TABLE accounts (id INTEGER PRIMARY KEY, note TEXT)", &[]).unwrap();
    for id in 1..=50 {
        db.query(
            &format!("INSERT INTO accounts (id, note) VALUES ({id}, 'x')"),
            &[],
        )
        .unwrap();
    }
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM accounts"), 50);

    // DROP it. The name and every row are gone: SELECT no longer binds.
    assert!(matches!(
        db.query("DROP TABLE accounts", &[]).unwrap(),
        ExecResult::Affected(0)
    ));
    assert!(
        db.query("SELECT count(*) FROM accounts", &[]).is_err(),
        "dropped table must not bind"
    );
    assert!(
        db.query("INSERT INTO accounts (id, note) VALUES (1, 'y')", &[]).is_err(),
        "insert into dropped table must fail"
    );

    // Page accounting balances — the freed data/index pages are neither
    // leaked nor double-counted.
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn re_create_after_drop_is_a_fresh_empty_table() {
    // Tombstone-in-place + slot reuse (DESIGN-DROP-TABLE §0/§1): a same-name
    // table created after a drop refills the retired slot but is a brand-new,
    // EMPTY table — none of the old rows leak through, and it gets fresh trees.
    let (cfg, path) = config("recreate");
    let db = Database::open_with_config(cfg).unwrap();

    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (1, 'old')", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (2, 'old')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);

    db.query("DROP TABLE t", &[]).unwrap();
    // Same name again → fresh, empty table (old rows must NOT reappear).
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 0);
    db.query("INSERT INTO t (id, v) VALUES (1, 'new')", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("new".into())]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stale_plan_after_drop_recreate_is_evicted_across_processes() {
    // The schema-gen cache gate: a cache HIT bypasses compilation, so a plan
    // compiled before a DROP+re-CREATE would otherwise be served stale. The old
    // plan references the now-tombstoned id (whose catalog tree-roots are gone),
    // so without the gate process B would ERROR on a hit instead of returning
    // the fresh table's data. The gate observes the schema_gen bump, drops the
    // cache, and B recompiles against the new table.
    let (cfg, path) = config("stale-plan");
    let a = Database::open_with_config(cfg.clone()).unwrap();
    let b = Database::open_with_config(cfg).unwrap();

    // A creates `widget`; B caches a plan for the exact SQL by querying it.
    a.query("CREATE TABLE widget (id INTEGER PRIMARY KEY, kind TEXT)", &[]).unwrap();
    a.query("INSERT INTO widget (id, kind) VALUES (1, 'gears')", &[]).unwrap();
    assert_eq!(
        rows(b.query("SELECT kind FROM widget WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("gears".into())]]
    );

    // A drops it and re-creates a fresh `widget` (a new id under no-reuse) with
    // different data. The SQL text is identical, so B's cached plan would be
    // served on a hit unless the gate evicts it.
    a.query("DROP TABLE widget", &[]).unwrap();
    a.query("CREATE TABLE widget (id INTEGER PRIMARY KEY, kind TEXT)", &[]).unwrap();
    a.query("INSERT INTO widget (id, kind) VALUES (1, 'springs')", &[]).unwrap();

    // B must return the NEW row — not error on the stale plan, not the old row.
    assert_eq!(
        rows(b.query("SELECT kind FROM widget WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("springs".into())]]
    );
    b.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_leaves_other_tables_untouched() {
    let (cfg, path) = config("others");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("INSERT INTO users (id, name) VALUES (1, 'seed')", &[]).unwrap();

    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    db.query("CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    for id in 1..=10 {
        db.query(&format!("INSERT INTO a (id, v) VALUES ({id}, 'a')"), &[]).unwrap();
        db.query(&format!("INSERT INTO b (id, v) VALUES ({id}, 'b')"), &[]).unwrap();
    }

    db.query("DROP TABLE a", &[]).unwrap();
    // `b` and the seed table keep every row and still target the right tree.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM b"), 10);
    assert_eq!(
        rows(db.query("SELECT v FROM b WHERE id = 7", &[]).unwrap()),
        vec![vec![Value::Text("b".into())]]
    );
    db.query("INSERT INTO b (id, v) VALUES (11, 'b')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM b"), 11);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 1);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_if_exists_and_missing_table() {
    let (cfg, path) = config("ifexists");
    let db = Database::open_with_config(cfg).unwrap();

    // Missing table without IF EXISTS → error.
    assert!(db.query("DROP TABLE nope", &[]).is_err());
    // Missing table WITH IF EXISTS → success, no-op.
    assert!(matches!(
        db.query("DROP TABLE IF EXISTS nope", &[]).unwrap(),
        ExecResult::Affected(0)
    ));
    // Present table WITH IF EXISTS → dropped.
    db.query("CREATE TABLE gone (id INTEGER PRIMARY KEY)", &[]).unwrap();
    db.query("INSERT INTO gone (id) VALUES (1)", &[]).unwrap();
    assert!(matches!(
        db.query("DROP TABLE IF EXISTS gone", &[]).unwrap(),
        ExecResult::Affected(0)
    ));
    assert!(db.query("SELECT count(*) FROM gone", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_persists_across_reopen() {
    let (cfg, path) = config("persist");
    {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        db.query("CREATE TABLE keep (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        db.query("CREATE TABLE toss (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        db.query("INSERT INTO keep (id, v) VALUES (1, 'k')", &[]).unwrap();
        db.query("INSERT INTO toss (id, v) VALUES (1, 't')", &[]).unwrap();
        db.query("DROP TABLE toss", &[]).unwrap();
        db.verify().unwrap();
    }
    // Reopen: the drop is durable (`toss` gone), `keep` intact, and the
    // tombstoned slot round-trips through the v3 canonical bytes.
    {
        let db = Database::open_with_config(cfg).unwrap();
        assert!(db.query("SELECT count(*) FROM toss", &[]).is_err());
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM keep"), 1);
        // A new table can still be created after reopening a schema that
        // carries a tombstone.
        db.query("CREATE TABLE fresh (id INTEGER PRIMARY KEY)", &[]).unwrap();
        db.query("INSERT INTO fresh (id) VALUES (9)", &[]).unwrap();
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM fresh"), 1);
        db.verify().unwrap();
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn second_process_sees_the_drop() {
    // Two handles on the SAME file. B caches the schema at open; A drops a
    // table; B must see it gone on its next statement (schema-gen reload).
    let (cfg, path) = config("multiproc");
    let a = Database::open_with_config(cfg.clone()).unwrap();
    let b = Database::open_with_config(cfg).unwrap();

    a.query("CREATE TABLE shared (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    a.query("INSERT INTO shared (id, v) VALUES (1, 'x')", &[]).unwrap();
    // B warms its cached schema by reading the table A created.
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM shared"), 1);

    // A drops it (bumps schema_gen).
    a.query("DROP TABLE shared", &[]).unwrap();

    // B — still holding the stale schema — must pick up the drop on its next
    // statement (refresh-before-compile).
    assert!(
        b.query("SELECT count(*) FROM shared", &[]).is_err(),
        "B must observe the drop"
    );
    b.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn create_drop_churn_reclaims_pages() {
    // The high-water regression guard for DROP: repeatedly creating a table,
    // filling it, and dropping it must reclaim pages — otherwise a 16 MB file
    // (~4096 pages) runs out fast. Each dropped table's pages must return to
    // the pool. Bounded to 40 cycles: no-reuse (DESIGN-DROP-TABLE §0) retires
    // one id per create, so the id space (cap 64) is the real churn limit —
    // 40 stays well within it while still forcing heavy page reuse.
    let (cfg, path) = config("churn");
    let db = Database::open_with_config(cfg).unwrap();
    for cycle in 0..40 {
        db.query("CREATE TABLE churn (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .unwrap_or_else(|e| panic!("cycle {cycle}: create: {e}"));
        for id in 0..40 {
            db.query(
                &format!("INSERT INTO churn (id, v) VALUES ({id}, 'padding-padding-padding')"),
                &[],
            )
            .unwrap_or_else(|e| panic!("cycle {cycle}: insert: {e}"));
        }
        db.query("DROP TABLE churn", &[])
            .unwrap_or_else(|e| panic!("cycle {cycle}: drop: {e}"));
    }
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// SLOW (~110 s): 4096 DROP+CREATE cycles, each rewriting a schema record that
/// grows by one tombstone — the O(n²) tombstone-bloat cost DESIGN-TABLE-CAP §3
/// names as the thing that actually bounds MAX_TABLES. Run with `--ignored`.
/// The in-memory equivalent (`schema::tests::create_refuses_at_the_id_ceiling`)
/// covers the mint refusal and its message on every run.
#[test]
#[ignore = "~110 s: burns the whole 4096-id space through real commits"]
fn create_refuses_after_the_lifetime_id_ceiling() {
    // No-reuse's bounded cost, end to end: DROP+CREATE churn burns one id per
    // cycle, so eventually a CREATE refuses closed (a clean error, never
    // corruption) rather than mint past the MAX_TABLES id-space ceiling. This
    // is the deliberate limit; the offline `regenerate` compaction is the
    // escape hatch. The bound is derived from MAX_TABLES (4096 as of
    // design/DESIGN-TABLE-CAP.md) with slack, so the ceiling is reached
    // regardless of the exact system-table reserve.
    let (cfg, path) = config("ceiling");
    let db = Database::open_with_config(cfg).unwrap();
    let mut hit_ceiling = None;
    for cycle in 0..mpedb_types::MAX_TABLES + 64 {
        match db.query("CREATE TABLE spin (id INTEGER PRIMARY KEY)", &[]) {
            Ok(_) => {}
            Err(e) => {
                hit_ceiling = Some((cycle, e.to_string()));
                break;
            }
        }
        db.query("DROP TABLE spin", &[]).unwrap();
    }
    let (cycle, msg) = hit_ceiling.expect("id ceiling must eventually refuse a create");
    // A GOOD error: it names the exhausted resource and the way out, and it is
    // a schema error rather than a panic or a corruption.
    assert!(
        msg.contains("table-id space exhausted") && msg.contains("rebuild"),
        "unhelpful ceiling error at cycle {cycle}: {msg}"
    );
    // The ceiling is the id space, not some smaller accident: churn must have
    // burned essentially the whole of MAX_TABLES before refusing.
    assert!(
        cycle + 16 >= mpedb_types::MAX_TABLES,
        "refused far too early (cycle {cycle} of {})",
        mpedb_types::MAX_TABLES
    );
    // The database is still fully usable after the refusal — the seed table
    // and any survivor work.
    db.query("INSERT INTO users (id, name) VALUES (1, 'ok')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM users"), 1);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
