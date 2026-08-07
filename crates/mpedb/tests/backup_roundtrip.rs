//! Raw whole-database backup: does the copy come back as the same database?
//!
//! The interesting cases are not "does it round-trip" — that is table stakes —
//! but the three the design rests on: a backup taken while a writer is running
//! must be the SNAPSHOT and not a torn mixture; the file must be proportional
//! to the DATA and not to the arena; and every way of getting the geometry
//! wrong must be a named refusal rather than a silently misplaced page.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn cfg(name: &str, size_mb: u32, max_readers: u32) -> Config {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/backup-{name}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    Config::from_toml_str(&format!(
        "[database]\npath = \"{path}\"\nsize_mb = {size_mb}\nmax_readers = {max_readers}\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    ))
    .expect("config")
}

fn open(c: &Config) -> Database {
    Database::open_with_config(c.clone()).unwrap_or_else(|e| panic!("open: {e}"))
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// `query` with no params, for statements whose result is not rows.
trait Ddl {
    fn query_nc(&self, sql: &str) -> mpedb::Result<ExecResult>;
}
impl Ddl for Database {
    fn query_nc(&self, sql: &str) -> mpedb::Result<ExecResult> {
        self.query(sql, &[])
    }
}

fn seed(db: &Database, n: i64) {
    db.query_nc("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, blob BLOB)")
        .expect("create");
    for i in 0..n {
        db.query(
            "INSERT INTO t (id, name, blob) VALUES ($1, $2, $3)",
            &[
                Value::Int(i),
                Value::Text(format!("row {i}")),
                Value::Blob(vec![(i % 251) as u8; 300]),
            ],
        )
        .expect("insert");
    }
}

#[test]
fn a_backup_restores_to_the_same_rows_and_the_same_schema() {
    let src_cfg = cfg("src", 16, 8);
    let src = open(&src_cfg);
    seed(&src, 200);
    src.query_nc("CREATE INDEX t_name ON t (name)").expect("index");
    let before = rows(&src, "SELECT id, name FROM t ORDER BY id");
    let bytes = src.backup().expect("backup");
    drop(src);

    let dst_cfg = cfg("dst", 16, 8);
    let dst = open(&dst_cfg);
    dst.restore_backup(&bytes).expect("restore");

    assert_eq!(rows(&dst, "SELECT id, name FROM t ORDER BY id"), before);
    // The index came too — this is the whole B+tree, not a row export.
    assert_eq!(
        rows(&dst, "SELECT id FROM t WHERE name = 'row 42'"),
        vec![vec![Value::Int(42)]]
    );
    // And it is a live database, not a read-only image.
    dst.query(
        "INSERT INTO t (id, name, blob) VALUES ($1, $2, $3)",
        &[Value::Int(9999), Value::Text("after".into()), Value::Blob(vec![1])],
    )
    .expect("write after restore");
    assert_eq!(rows(&dst, "SELECT count(*) FROM t")[0][0], Value::Int(201));
}

/// The claim that makes this worth having over a SQL dump: the file is the
/// DATA, not the arena. A 64 MiB database holding a few rows must not produce a
/// 64 MiB backup.
#[test]
fn the_backup_is_proportional_to_the_data_and_not_to_the_arena() {
    let c = cfg("small", 64, 8);
    let db = open(&c);
    seed(&db, 5);
    let bytes = db.backup().expect("backup");
    assert!(
        bytes.len() < 1024 * 1024,
        "a 64 MiB arena with five rows produced {} bytes",
        bytes.len()
    );
}

/// A backup taken while another thread commits must be ONE snapshot: every row
/// the writer had committed before the copy started, and nothing torn. The
/// reader's pin is the only synchronisation, so this is the test that says the
/// COW argument in `mpedb_core::backup` holds in practice.
#[test]
fn a_backup_taken_under_a_running_writer_is_one_consistent_snapshot() {
    let c = cfg("concurrent", 32, 8);
    let db = Arc::new(open(&c));
    seed(&db, 50);

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 1_000i64;
            while !stop.load(Ordering::Relaxed) {
                let _ = db.query(
                    "INSERT INTO t (id, name, blob) VALUES ($1, $2, $3)",
                    &[
                        Value::Int(i),
                        Value::Text(format!("w{i}")),
                        Value::Blob(vec![7u8; 200]),
                    ],
                );
                i += 1;
            }
            i
        })
    };

    // Several backups while the writer runs; each must restore to a database
    // whose row count matches its OWN count of ids — an internally consistent
    // one, whatever the writer had reached.
    let mut taken: Vec<Vec<u8>> = Vec::new();
    for _ in 0..5 {
        taken.push(db.backup().expect("backup under a writer"));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = writer.join().expect("writer thread");

    for (k, bytes) in taken.iter().enumerate() {
        let dc = cfg(&format!("conc-dst{k}"), 32, 8);
        let dst = open(&dc);
        dst.restore_backup(bytes).unwrap_or_else(|e| panic!("restore {k}: {e}"));
        let n = match rows(&dst, "SELECT count(*) FROM t")[0][0] {
            Value::Int(n) => n,
            ref v => panic!("count is {v:?}"),
        };
        let ids = rows(&dst, "SELECT id FROM t ORDER BY id").len() as i64;
        assert_eq!(n, ids, "backup {k}: count and rows disagree — torn image");
        assert!(n >= 50, "backup {k} lost the seeded rows: {n}");
        // Every row is whole: the blob is the one that was written with it.
        for r in rows(&dst, "SELECT id, blob FROM t ORDER BY id") {
            let Value::Blob(b) = &r[1] else { panic!("blob is {:?}", r[1]) };
            assert!(b.len() == 300 || b.len() == 200, "row {:?} has {} bytes", r[0], b.len());
        }
    }
}

/// The geometry check. `max_readers` decides where the data region starts, so
/// restoring into an arena with a different width would put every page at the
/// wrong id — and that is the one failure that would NOT announce itself.
#[test]
fn restoring_into_a_different_reader_width_is_refused_by_name() {
    // 8 and 200 slots: 8×64 B fits one page, 200×64 B needs four, so these two
    // genuinely differ. (8 and 64 do not — both round to one page.)
    let src = open(&cfg("geo-src", 16, 8));
    seed(&src, 20);
    let bytes = src.backup().expect("backup");

    let dst = open(&cfg("geo-dst", 16, 200));
    let e = dst.restore_backup(&bytes).unwrap_err().to_string();
    assert!(e.contains("reader slots"), "{e}");
}

#[test]
fn restoring_into_a_database_that_has_been_written_to_is_refused() {
    let src = open(&cfg("used-src", 16, 8));
    seed(&src, 10);
    let bytes = src.backup().expect("backup");

    let dst = open(&cfg("used-dst", 16, 8));
    dst.query_nc("CREATE TABLE other (id INTEGER PRIMARY KEY)")
        .expect("create");
    let e = dst.restore_backup(&bytes).unwrap_err().to_string();
    assert!(e.contains("DDL run on it"), "{e}");
}

#[test]
fn a_backup_too_large_for_the_target_says_how_much_it_needs() {
    let src = open(&cfg("big-src", 64, 8));
    seed(&src, 4000);
    let bytes = src.backup().expect("backup");

    let dst = open(&cfg("big-dst", 1, 8));
    let e = dst.restore_backup(&bytes).unwrap_err().to_string();
    assert!(e.contains("size_mb"), "{e}");
}

/// A damaged file must be refused before a single page is written, or a failed
/// restore leaves something that opens and reads garbage.
#[test]
fn a_corrupted_backup_is_refused_and_leaves_the_target_untouched() {
    let src = open(&cfg("corrupt-src", 16, 8));
    seed(&src, 30);
    let mut bytes = src.backup().expect("backup");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;

    let dc = cfg("corrupt-dst", 16, 8);
    let dst = open(&dc);
    let e = dst.restore_backup(&bytes).unwrap_err().to_string();
    assert!(e.contains("checksum"), "{e}");
    // Still the empty database it was: the seed table is there and nothing else.
    assert!(dst.query("SELECT count(*) FROM t", &[]).is_err(), "t must not exist");
}

/// Truncation at every offset: an `Err`, never a panic and never a partial
/// write. The corpus convention for every decoder in this codebase.
#[test]
fn every_truncation_of_a_backup_is_refused() {
    let src = open(&cfg("trunc-src", 16, 8));
    seed(&src, 5);
    let bytes = src.backup().expect("backup");

    let dc = cfg("trunc-dst", 16, 8);
    let dst = open(&dc);
    // Every page boundary plus every offset in the header — the whole sweep is
    // O(pages) rather than O(bytes) so the test stays fast.
    let mut cuts: Vec<usize> = (0..96).collect();
    cuts.extend((0..bytes.len()).step_by(4096));
    for cut in cuts {
        if cut == bytes.len() {
            continue;
        }
        assert!(
            dst.restore_backup(&bytes[..cut]).is_err(),
            "truncation at {cut} was accepted"
        );
    }
}

/// The case the whole feature was asked for: an IN-MEMORY database, which is
/// what a wasm build has — there is no filesystem there, so `VACUUM INTO` and a
/// file copy are both unavailable and a SQL dump was the only way out.
///
/// Worth its own test rather than assumed from the file case: `:memory:` takes
/// a different path through `Shm` (a private meta pair and a process-local pin
/// table instead of the shared reader table), and "it works on a file" says
/// nothing about that.
#[test]
fn an_in_memory_database_backs_up_and_restores() {
    let mem = || {
        Config::from_toml_str(
            "[database]\npath = \":memory:\"\nsize_mb = 16\nmax_readers = 8\n\n\
             [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n",
        )
        .expect("config")
    };

    let src = Database::open_with_config(mem()).expect("open :memory:");
    seed(&src, 120);
    src.query_nc("CREATE INDEX t_name ON t (name)").expect("index");
    let before = rows(&src, "SELECT id, name FROM t ORDER BY id");
    let bytes = src.backup().expect("backup from :memory:");

    let dst = Database::open_with_config(mem()).expect("open :memory: again");
    dst.restore_backup(&bytes).expect("restore into :memory:");

    assert_eq!(rows(&dst, "SELECT id, name FROM t ORDER BY id"), before);
    assert_eq!(
        rows(&dst, "SELECT id FROM t WHERE name = 'row 99'"),
        vec![vec![Value::Int(99)]]
    );
    dst.query(
        "INSERT INTO t (id, name, blob) VALUES ($1, $2, $3)",
        &[Value::Int(5000), Value::Text("after".into()), Value::Blob(vec![2])],
    )
    .expect("write after restore");
    assert_eq!(rows(&dst, "SELECT count(*) FROM t")[0][0], Value::Int(121));

    // Two independent in-memory databases: the restore must not have reached
    // into the source's arena.
    assert_eq!(rows(&src, "SELECT count(*) FROM t")[0][0], Value::Int(120));
}

/// What the raw copy is worth against the SQL route, on the same data.
///
/// `#[ignore]`d: it is a measurement, not a contract — a number a test enforces
/// is a number that fails on a busy machine. Run it when the question comes up:
///
/// ```sh
/// MPEDB_TEST_DIR=/vol cargo test -p mpedb --release --test backup_roundtrip \
///   -- --ignored raw_copy --nocapture
/// ```
#[test]
#[ignore]
fn raw_copy_against_the_sql_route() {
    use std::time::Instant;

    let c = Config::from_toml_str(
        "[database]\npath = \":memory:\"\nsize_mb = 128\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n",
    )
    .expect("config");
    let db = Database::open_with_config(c).expect("open");
    db.query_nc("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, blob BLOB)")
        .expect("create");
    // Eight megabytes of blob, the shape a project full of images has.
    for i in 0..800i64 {
        db.query(
            "INSERT INTO t (id, name, blob) VALUES ($1, $2, $3)",
            &[
                Value::Int(i),
                Value::Text(format!("row {i}")),
                Value::Blob(vec![(i % 251) as u8; 10_000]),
            ],
        )
        .expect("insert");
    }

    let mut best_backup = std::time::Duration::from_secs(99);
    let mut bytes = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        bytes = db.backup().expect("backup");
        best_backup = best_backup.min(t.elapsed());
    }

    // The SQL route, as an app would write it: every row out, every blob hexed.
    let mut best_sql = std::time::Duration::from_secs(99);
    let mut sql_len = 0usize;
    for _ in 0..5 {
        let t = Instant::now();
        let mut out = String::new();
        for r in rows(&db, "SELECT id, name, blob FROM t ORDER BY id") {
            out.push_str("INSERT INTO t VALUES (");
            for (k, v) in r.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                match v {
                    Value::Int(i) => out.push_str(&i.to_string()),
                    Value::Text(s) => {
                        out.push('\'');
                        out.push_str(s);
                        out.push('\'');
                    }
                    Value::Blob(b) => {
                        const HEX: &[u8; 16] = b"0123456789ABCDEF";
                        out.push_str("X'");
                        for byte in b {
                            out.push(HEX[(byte >> 4) as usize] as char);
                            out.push(HEX[(byte & 0x0f) as usize] as char);
                        }
                        out.push('\'');
                    }
                    _ => out.push_str("NULL"),
                }
            }
            out.push_str(");\n");
        }
        sql_len = out.len();
        best_sql = best_sql.min(t.elapsed());
    }

    eprintln!("\n8 MB of blobs in 800 rows, best of 5:");
    eprintln!("  backup()   {best_backup:?}   {} B", bytes.len());
    eprintln!("  SQL route  {best_sql:?}   {sql_len} B  (before gzip, before the parse back)");
}
