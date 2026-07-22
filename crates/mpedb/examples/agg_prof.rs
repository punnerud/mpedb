//! **Where do the ~160-180 ns/row of a serial aggregate fold actually go?**
//! (design/DESIGN-PARALLEL-READ.md said "per-row cost FIRST"; this probe is
//! the attribution that decision asked for.)
//!
//! Fixture = `examples/par_ceiling.rs`'s: `src(id, g10, gk, a, t)` with
//! `t` a ~20-byte text, 1M rows by default. Stages measured, min-of-N wall
//! time, reported as ns/row beside allocations/row from a counting global
//! allocator (allocation COUNT, not bytes — the malloc/free pair is the
//! suspected unit cost):
//!
//! ```text
//!   sql:count   SELECT count(*) FROM src            the full fold path
//!   sql:sum     SELECT sum(a) FROM src
//!   sql:g10     GROUP BY g10 (10 groups), count+sum
//!   sql:g10k    GROUP BY gk (10k groups), count+sum
//!   raw:cursor  ReadTxn::scan → RowCursor::next drain (no SQL executor):
//!               btree walk + work-meter + full-row decode
//!   mem:decode  row::decode_row over the same 1M row images, contiguous
//!   mem:meter   WorkMeter::charge x 1M
//!   sqlite      the same statements on bundled rusqlite (control)
//! ```
//!
//! `sql:X - raw:cursor` is the executor's own overhead (batching, fold,
//! group map); `raw:cursor - mem:decode - mem:meter` approximates the btree
//! cursor's per-row cost including its two per-row heap copies (key, value).
//!
//! Usage: `agg_prof <rows> [reps]` — the fixture file is cached under
//! `/mnt/xfs/mpedb-scratch` and reused when the row count matches, so
//! iterating on the engine re-measures in seconds, not minutes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use mpedb::{Config, Database, ExecResult, Value};

// ---------------------------------------------------------- alloc counting

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ABYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        ABYTES.fetch_add(l.size(), Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        ABYTES.fetch_add(l.size(), Relaxed);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        ABYTES.fetch_add(new, Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// ----------------------------------------------------------------- fixture

fn schema_toml(path: &str, size_mb: usize) -> String {
    format!(
        "[database]\npath = \"{path}\"\nsize_mb = {size_mb}\nmax_readers = 32\n\
         durability = \"none\"\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = 0\n\n\
         [[table]]\nname = \"src\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g10\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"gk\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\nnullable = false\nindexed = true\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n"
    )
}

fn load(db: &Database, rows: usize) {
    let mut i = 0;
    while i < rows {
        let stop = (i + 10_000).min(rows);
        let mut w = db.begin().unwrap();
        while i < stop {
            let end = (i + 500).min(stop);
            let vals: Vec<String> = (i..end)
                .map(|k| {
                    format!("({k}, {}, {}, {k}, 'payload text row {k}')", k % 10, k % 10_000)
                })
                .collect();
            w.query(
                &format!("INSERT INTO src (id, g10, gk, a, t) VALUES {}", vals.join(", ")),
                &[],
            )
            .unwrap();
            i = end;
        }
        w.commit().unwrap();
    }
}

fn open_fixture(rows: usize) -> (Database, String) {
    let dir = ["/mnt/xfs/mpedb-scratch", "/tmp"]
        .into_iter()
        .find(|d| std::fs::create_dir_all(d).is_ok())
        .unwrap_or("/tmp");
    let path = format!("{dir}/aggprof-{rows}.mpedb");
    let size_mb = 256 + rows / 4000;
    let cfg = Config::from_toml_str(&schema_toml(&path, size_mb)).unwrap();
    let existed = std::fs::metadata(&path).is_ok();
    let db = match Database::open_with_config(cfg) {
        Ok(db) => db,
        Err(e) => {
            // A stale cache from an older schema: rebuild once.
            eprintln!("(cached fixture rejected: {e}; rebuilding)");
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{path}-wal"));
            let cfg = Config::from_toml_str(&schema_toml(&path, size_mb)).unwrap();
            Database::open_with_config(cfg).unwrap()
        }
    };
    let have = match db.query("SELECT count(*) FROM src", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n as usize,
            _ => 0,
        },
        _ => 0,
    };
    if have != rows {
        if existed {
            eprintln!("(cached fixture has {have} rows, want {rows}; reloading)");
            db.query("DELETE FROM src", &[]).unwrap();
        }
        load(&db, rows);
    }
    (db, path)
}

// -------------------------------------------------------------- measuring

struct M {
    ns_per_row: f64,
    allocs_per_row: f64,
    ms: f64,
}

fn measure(rows: usize, reps: usize, mut f: impl FnMut()) -> M {
    let mut best = f64::MAX;
    let mut best_allocs = 0usize;
    for _ in 0..reps {
        let a0 = ALLOCS.load(Relaxed);
        let t0 = Instant::now();
        f();
        let dt = t0.elapsed().as_secs_f64();
        let da = ALLOCS.load(Relaxed) - a0;
        if dt < best {
            best = dt;
            best_allocs = da;
        }
    }
    M {
        ns_per_row: best * 1e9 / rows as f64,
        allocs_per_row: best_allocs as f64 / rows as f64,
        ms: best * 1e3,
    }
}

fn report(name: &str, m: &M) {
    println!(
        "{name:<44} {:>9.1} ns/row {:>7.2} allocs/row {:>9.1} ms",
        m.ns_per_row, m.allocs_per_row, m.ms
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1_000_000);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);

    let (db, path) = open_fixture(rows);
    println!("== agg_prof: {rows} rows, min of {reps} ==");
    if let Ok(n) = std::env::var("MPEDB_FOLD_BATCH") {
        println!("   (MPEDB_FOLD_BATCH={n})");
    }

    // ---- the SQL shapes -------------------------------------------------
    for (name, sql) in [
        ("sql:count  SELECT count(*)", "SELECT count(*) FROM src"),
        ("sql:sum    SELECT sum(a)", "SELECT sum(a) FROM src"),
        // `g10`/`gk` are UNINDEXED, so these are the BASE-TABLE fold — the
        // ~140 ns/row floor the decode-to-accumulator fusion targets. `a`
        // above rides the format-59 index tree and measures that path.
        ("sql:sumb   SELECT sum(g10) [base]", "SELECT sum(g10) FROM src"),
        ("sql:avgb   SELECT avg(g10) [base]", "SELECT avg(g10) FROM src"),
        ("sql:mmb    min(gk), max(gk) [base]", "SELECT min(gk), max(gk) FROM src"),
        ("sql:mixb   count(*), sum(g10) [base]", "SELECT count(*), sum(g10) FROM src"),
        ("sql:cnta   SELECT count(a)", "SELECT count(a) FROM src"),
        ("sql:avg    SELECT avg(a)", "SELECT avg(a) FROM src"),
        ("sql:minmax SELECT min(a), max(a)", "SELECT min(a), max(a) FROM src"),
        ("sql:g10    GROUP BY g10", "SELECT g10, count(*), sum(a) FROM src GROUP BY g10"),
        ("sql:g10k   GROUP BY gk", "SELECT gk, count(*), sum(a) FROM src GROUP BY gk"),
    ] {
        let m = measure(rows, reps, || {
            let r = db.query(sql, &[]).unwrap();
            let ExecResult::Rows { rows: out, .. } = r else { panic!() };
            std::hint::black_box(&out);
        });
        report(name, &m);
    }

    // ---- raw cursor drain (no SQL executor) -----------------------------
    // A sidecar engine handle on the same file: exactly the substrate the
    // executor's ReadCtx scans through, minus everything above it.
    let eng = mpedb_core::Engine::open_from_file(std::path::Path::new(&path)).unwrap();
    let table = eng.schema().schema.table_id("src").unwrap();
    let m = measure(rows, reps, || {
        let rt = eng.begin_read().unwrap();
        let mut c = rt.scan(table, None, None).unwrap();
        let mut n = 0usize;
        while let Some(row) = c.next().unwrap() {
            std::hint::black_box(&row);
            n += 1;
        }
        assert_eq!(n, rows);
        rt.finish().unwrap();
    });
    report("raw:cursor  RowCursor drain (chg+decode)", &m);

    // ---- decode alone, over contiguous re-encoded images ----------------
    let types: Vec<mpedb_types::ColumnType> =
        eng.schema().col_types[table as usize].clone();
    let images: Vec<Vec<u8>> = {
        let rt = eng.begin_read().unwrap();
        let mut c = rt.scan(table, None, None).unwrap();
        let mut v = Vec::with_capacity(rows);
        while let Some(row) = c.next().unwrap() {
            v.push(mpedb_core::row::encode_row(&row, &types).unwrap());
        }
        rt.finish().unwrap();
        v
    };
    let m = measure(rows, reps, || {
        for img in &images {
            let row = mpedb_core::row::decode_row(img, &types).unwrap();
            std::hint::black_box(&row);
        }
    });
    report("mem:decode  decode_row x rows", &m);
    drop(images);

    // ---- the work meter alone -------------------------------------------
    let meter = mpedb_core::WorkMeter::new(0);
    let m = measure(rows, reps, || {
        for _ in 0..rows {
            meter.charge(1, String::new).unwrap();
        }
        std::hint::black_box(meter.used());
    });
    report("mem:meter   WorkMeter::charge x rows", &m);

    // ---- malloc pair calibration ----------------------------------------
    let key = [7u8; 9];
    let val = [42u8; 60];
    let m = measure(rows, reps, || {
        for _ in 0..rows {
            let k = key.to_vec();
            let v = val.to_vec();
            std::hint::black_box((&k, &v));
        }
    });
    report("mem:2alloc  key9.to_vec + val60.to_vec", &m);

    // ---- sqlite control --------------------------------------------------
    if std::env::var("AGG_SQLITE").as_deref() != Ok("0") {
        let sq = rusqlite::Connection::open_in_memory().unwrap();
        sq.execute_batch(
            "CREATE TABLE src (id INTEGER PRIMARY KEY, g10 INTEGER, gk INTEGER, \
             a INTEGER NOT NULL, t TEXT); CREATE INDEX i_a ON src (a);",
        )
        .unwrap();
        {
            let tx = sq.unchecked_transaction().unwrap();
            let mut ins = tx
                .prepare("INSERT INTO src VALUES (?1, ?2, ?3, ?4, ?5)")
                .unwrap();
            for k in 0..rows as i64 {
                ins.execute(rusqlite::params![
                    k,
                    k % 10,
                    k % 10_000,
                    k,
                    format!("payload text row {k}")
                ])
                .unwrap();
            }
            drop(ins);
            tx.commit().unwrap();
        }
        for (name, sql) in [
            ("sqlite:count", "SELECT count(*) FROM src"),
            ("sqlite:sum", "SELECT sum(a) FROM src"),
            ("sqlite:sumb", "SELECT sum(g10) FROM src"),
            ("sqlite:mmb", "SELECT min(gk), max(gk) FROM src"),
            ("sqlite:cnta", "SELECT count(a) FROM src"),
            ("sqlite:avg", "SELECT avg(a) FROM src"),
            ("sqlite:minmax", "SELECT min(a), max(a) FROM src"),
            ("sqlite:g10", "SELECT g10, count(*), sum(a) FROM src GROUP BY g10"),
            ("sqlite:g10k", "SELECT gk, count(*), sum(a) FROM src GROUP BY gk"),
        ] {
            let m = measure(rows, reps, || {
                let mut st = sq.prepare_cached(sql).unwrap();
                let mut sqrows = st.query([]).unwrap();
                let mut n = 0i64;
                while let Some(r) = sqrows.next().unwrap() {
                    // Generic read: avg() answers REAL where the rest are INTEGER.
                    let c0: rusqlite::types::Value = r.get(0).unwrap();
                    std::hint::black_box(&c0);
                    n += 1;
                }
                std::hint::black_box(n);
            });
            report(name, &m);
        }
    }
}
