//! **What is the substrate ceiling for intra-query read parallelism?**
//!
//! mpedb's concurrency machinery (MVCC snapshots, lock-free readers) exists
//! for parallel *requests*; a serial workload never touches it. But the same
//! substrate should let ONE job's work be split across threads: N readers on
//! the same snapshot coordinate on nothing, so a partitioned parallel scan
//! has near-zero coordination cost *if* the executor splits the work. Before
//! designing that executor, this probe measures the ceiling WITHOUT touching
//! the engine: the same aggregate, hand-partitioned into N contiguous PK
//! ranges, run on N threads through today's public API (`execute` /
//! `stream_query` on `WHERE id >= $1 AND id < $2` plans), combined in the
//! probe. If hand-partitioned scans do not scale, no executor change will —
//! that is the whole experiment (`design/DESIGN-PARALLEL-READ.md`).
//!
//! Write-side intra-parallelism is measured dead and OUT OF SCOPE
//! (`design/DESIGN-PHASE3.md` §2: the COW mutation is serial, ceiling 1.28x).
//!
//! # What one (shape, N) cell does
//!
//! Split `[0, rows)` into contiguous PK ranges, spawn N scoped threads on
//! one shared `Database` handle (it is `Sync`), each executes the SAME
//! prepared plan with `(lo, hi)` params, the probe merges the partial
//! accumulators (scalars add; GROUP BY maps merge — exactly what a parallel
//! fold executor would do), and the merged answer is asserted equal to the
//! unpartitioned base answer. Wall time includes thread spawn + join + merge:
//! that is what a naive executor would pay (a real one amortizes spawn with a
//! pool; the measured no-op spawn floor is printed once for subtraction).
//!
//! Two schedules per N, because the M3's cores are asymmetric (P+E):
//!
//! - `equal`:  N chunks, one per thread — the slowest (E-core) partition
//!   gates the wall clock.
//! - `morsel`: 8N chunks pulled from a shared atomic counter — work-stealing
//!   in its simplest form (Hyper/DuckDB-style morsel-driven scheduling).
//!   `morsel n=1` doubles as the chunking-overhead control: same thread,
//!   8x the plan executions.
//!
//! # Shapes
//!
//! ```text
//!   scan    SELECT id, a               via stream_query, fold in the PROBE
//!                                      (raw substrate: scan + decode, no agg)
//!   count   SELECT count(*)            engine fold per partition
//!   sum     SELECT sum(a)              engine fold per partition
//!   g10     GROUP BY g10   (10 groups)   merge 10-entry maps
//!   g10k    GROUP BY gk    (10k groups)  merge 10k-entry maps
//!   join    src JOIN dim (10k rows) ON src.gk = dim.id, count+sum —
//!           partition the OUTER side, inner held per thread
//! ```
//!
//! # Snapshot pinning
//!
//! A parallel fold is only correct if all N partitions read the SAME
//! snapshot. Verified two ways after the sweep:
//!
//! 1. **Behaviorally** (all modes): open N partition streams, THEN commit an
//!    update that moves one row in every partition by a huge delta, then
//!    drain. The drained sum must equal the pre-update sum — every stream
//!    pinned its snapshot at open, none saw the write.
//! 2. **By txn id** (file mode): a sidecar `mpedb_core::Engine::open_from_file`
//!    handle reads `ReadTxn::txn_id()` before and after the N opens; equal ids
//!    bracket the opens, so all N pinned that id. The FACADE cannot express
//!    this check — `Database` exposes no snapshot handle and `RowStream` no
//!    txn id — which is the API gap the design doc names.
//!
//! # Usage
//!
//! ```text
//! par_ceiling <mode> <rows>        mode = file | mem
//!   PAR_SWEEP=1,2,4,8,11   thread counts (default)
//!   PAR_REPS=3             timed reps per cell (min reported; 1 warmup)
//!   PAR_SQLITE=0           skip the rusqlite control
//! ```
//!
//! Control: the same base statements on bundled sqlite (rusqlite), single
//! thread — sqlite has no intra-query parallelism — plus, in file mode, the
//! same hand-partitioning across N sqlite *connections* (possible, but with
//! per-connection page caches and no cross-connection snapshot guarantee;
//! connection opens are inside the timed region, footnoted in the doc).
//!
//! # Results (M3 Pro, 5P+6E, 2026-07-21 — analysis in DESIGN-PARALLEL-READ.md)
//!
//! Speedup over the unpartitioned statement, best of equal/morsel, min of 3:
//!
//! ```text
//!   file, 1M rows      n=2    n=4    n=8    n=11   base ms   n=11 ms
//!   scan               1.78   3.39   4.85   5.48     158.4      28.9
//!   count              1.79   3.34   4.63   5.43     157.3      29.0
//!   sum                1.82   3.39   4.80   5.52     182.4      33.1
//!   g10                1.87   3.58   5.19   5.77     244.0      42.3
//!   g10k               1.76   3.19   4.01   4.63     277.7      60.0
//!   join               1.91   3.66   5.51   6.56     594.0      90.6
//!
//!   file, 4M rows: scan 6.44x, count 6.20x, sum 6.13x, g10 6.35x,
//!                  g10k 5.96x, join 6.80x   (scaling IMPROVES with size)
//!   mem == file within noise at 1M (shared substrate, storage-agnostic).
//!   100k rows: caps at ~2-3x.  10k rows: <= 1.25x, n=11 is 0.56x (SLOWER).
//! ```
//!
//! The curve never flattens from coordination — 4M scales better than 1M —
//! it flattens at the machine: ~6.2-6.8x is ~85% of the ideal throughput of
//! 5 P-cores + 6 half-speed E-cores. The substrate ceiling is the silicon.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mpedb::{Config, Database, ExecResult, PlanHash, Value};

// ------------------------------------------------------------------ fixture

const DIM_ROWS: i64 = 10_000;

struct Tmp {
    db: Arc<Database>,
    path: Option<String>,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(format!("{p}-wal"));
        }
    }
}

fn schema_toml(path: &str, size_mb: usize) -> String {
    format!(
        "[database]\npath = \"{path}\"\nsize_mb = {size_mb}\nmax_readers = 32\n\
         durability = \"none\"\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = 0\n\n\
         [[table]]\nname = \"src\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"g10\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"gk\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"a\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"t\"\ntype = \"text\"\n\n\
         [[table]]\nname = \"dim\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\n"
    )
}

fn tmp_path(tag: &str) -> String {
    let dir = ["/mnt/xfs/mpedb-scratch", "/tmp"]
        .into_iter()
        .find(|d| std::fs::create_dir_all(d).is_ok())
        .unwrap_or("/tmp");
    let p = format!("{dir}/par-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{p}-wal"));
    p
}

fn open_fixture(mode: &str, rows: usize) -> Tmp {
    // ~64 B/row plus slack; over-sizing costs address space, not RSS.
    let size_mb = 256 + rows / 4000;
    let (path_str, path) = match mode {
        "file" => {
            let p = tmp_path(&format!("{rows}"));
            (p.clone(), Some(p))
        }
        "mem" => (":memory:".to_string(), None),
        other => panic!("mode must be file|mem, got `{other}`"),
    };
    let cfg = Config::from_toml_str(&schema_toml(&path_str, size_mb)).unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    Tmp { db: Arc::new(db), path }
}

/// Chunked multi-row VALUES inside a WriteSession — the loader shape
/// `examples/mem_shapes.rs` settled on (compiles locally, publishes nothing).
fn load_chunks(db: &Database, rows: usize, mut sql: impl FnMut(usize, usize) -> String) {
    let mut i = 0;
    while i < rows {
        let stop = (i + 10_000).min(rows);
        let mut w = db.begin().unwrap();
        while i < stop {
            let end = (i + 500).min(stop);
            w.query(&sql(i, end), &[]).unwrap();
            i = end;
        }
        w.commit().unwrap();
    }
}

fn load(db: &Database, rows: usize) {
    load_chunks(db, rows, |i, end| {
        let vals: Vec<String> = (i..end)
            .map(|k| format!("({k}, {}, {}, {k}, 'payload text row {k}')", k % 10, k % 10_000))
            .collect();
        format!("INSERT INTO src (id, g10, gk, a, t) VALUES {}", vals.join(", "))
    });
    load_chunks(db, DIM_ROWS as usize, |i, end| {
        let vals: Vec<String> = (i..end).map(|j| format!("({j}, {})", j as i64 * 7)).collect();
        format!("INSERT INTO dim (id, k) VALUES {}", vals.join(", "))
    });
}

// ------------------------------------------------------------------- shapes

/// Canonical answer: group key -> (count-ish, sum-ish). Scalar shapes live
/// under key 0. Partition merge is entrywise addition — exactly the combine
/// step a parallel fold executor would run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Ans(BTreeMap<i64, (i64, i64)>);

impl Ans {
    fn absorb(&mut self, other: Ans) {
        for (k, (c, s)) in other.0 {
            let e = self.0.entry(k).or_insert((0, 0));
            e.0 += c;
            e.1 += s;
        }
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        // sum() over an empty partition is NULL; count it as 0 so a
        // rows-not-divisible-by-N tail partition still merges cleanly.
        Value::Null => 0,
        other => panic!("expected int, got {other:?}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// One output row; columns fold as (count, sum) under key 0.
    Scalar,
    /// Output rows are (group, count, sum).
    Groups,
    /// `stream_query`, fold (count, sum of col 1) in the probe.
    Stream,
}

struct Shape {
    name: &'static str,
    base_sql: &'static str,
    part_sql: &'static str,
    kind: Kind,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "scan",
        base_sql: "SELECT id, a FROM src",
        part_sql: "SELECT id, a FROM src WHERE id >= $1 AND id < $2",
        kind: Kind::Stream,
    },
    Shape {
        name: "count",
        base_sql: "SELECT count(*) FROM src",
        part_sql: "SELECT count(*) FROM src WHERE id >= $1 AND id < $2",
        kind: Kind::Scalar,
    },
    Shape {
        name: "sum",
        base_sql: "SELECT sum(a) FROM src",
        part_sql: "SELECT sum(a) FROM src WHERE id >= $1 AND id < $2",
        kind: Kind::Scalar,
    },
    Shape {
        name: "g10",
        base_sql: "SELECT g10, count(*), sum(a) FROM src GROUP BY g10",
        part_sql: "SELECT g10, count(*), sum(a) FROM src WHERE id >= $1 AND id < $2 GROUP BY g10",
        kind: Kind::Groups,
    },
    Shape {
        name: "g10k",
        base_sql: "SELECT gk, count(*), sum(a) FROM src GROUP BY gk",
        part_sql: "SELECT gk, count(*), sum(a) FROM src WHERE id >= $1 AND id < $2 GROUP BY gk",
        kind: Kind::Groups,
    },
    Shape {
        name: "join",
        base_sql: "SELECT count(*), sum(dim.k) FROM src, dim WHERE src.gk = dim.id",
        part_sql: "SELECT count(*), sum(dim.k) FROM src, dim \
                   WHERE src.gk = dim.id AND src.id >= $1 AND src.id < $2",
        kind: Kind::Scalar,
    },
];

fn fold_rows(kind: Kind, rows: Vec<Vec<Value>>) -> Ans {
    let mut out = Ans::default();
    match kind {
        Kind::Scalar => {
            assert_eq!(rows.len(), 1, "scalar shape must yield one row");
            let r = &rows[0];
            let c = int(&r[0]);
            let s = if r.len() > 1 { int(&r[1]) } else { 0 };
            out.absorb(Ans(BTreeMap::from([(0, (c, s))])));
        }
        Kind::Groups => {
            for r in rows {
                out.absorb(Ans(BTreeMap::from([(int(&r[0]), (int(&r[1]), int(&r[2])))])));
            }
        }
        Kind::Stream => unreachable!("streams fold incrementally"),
    }
    out
}

fn run_part(db: &Database, shape: &Shape, hash: &PlanHash, params: &[Value]) -> Ans {
    if shape.kind == Kind::Stream {
        let mut s = db.stream_query(hash, params).unwrap();
        let (mut c, mut sum) = (0i64, 0i64);
        while let Some(r) = s.next().unwrap() {
            c += 1;
            sum += int(&r[1]);
        }
        Ans(BTreeMap::from([(0, (c, sum))]))
    } else {
        match db.execute(hash, params).unwrap() {
            ExecResult::Rows { rows, .. } => fold_rows(shape.kind, rows),
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

/// N contiguous PK ranges over the dense id space [0, rows). Dense ids make
/// this the BEST-case split; a real executor would split on B+tree structure.
fn bounds(rows: usize, n: usize) -> Vec<(i64, i64)> {
    (0..n)
        .map(|i| (((i * rows) / n) as i64, (((i + 1) * rows) / n) as i64))
        .collect()
}

// -------------------------------------------------------------- measurement

fn reps() -> usize {
    std::env::var("PAR_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
}

/// 1 warmup + `reps()` timed runs; the answer (identical every run) and the
/// minimum wall time.
fn timed(mut f: impl FnMut() -> Ans) -> (Duration, Ans) {
    let ans = f();
    let mut best = Duration::MAX;
    for _ in 0..reps() {
        let t0 = Instant::now();
        let a = f();
        best = best.min(t0.elapsed());
        assert_eq!(a, ans, "answer changed between reps");
    }
    (best, ans)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn spawn_floor(n: usize) -> Duration {
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| std::hint::black_box(0u64));
        }
    });
    t0.elapsed()
}

fn sweep(t: &Tmp, mode: &str, rows: usize, ns: &[usize]) {
    println!(
        "[floor] spawn+join of 11 no-op threads: {:.3} ms (pay this per statement without a pool)",
        ms(spawn_floor(11))
    );
    for shape in SHAPES {
        let base_hash = t.db.prepare(shape.base_sql).unwrap();
        let part_hash = t.db.prepare(shape.part_sql).unwrap();
        let ph = &part_hash;
        if let Ok(ExecResult::Explain(e)) =
            t.db.query(&format!("EXPLAIN {}", shape.part_sql), &[Value::Int(0), Value::Int(1)])
        {
            let mut flat = e.split_whitespace().collect::<Vec<_>>().join(" ");
            flat.truncate(220);
            println!("[plan] shape={} part={flat:?}", shape.name);
        }
        let db = &t.db;
        let (base_d, base_ans) = timed(|| run_part(db, shape, &base_hash, &[]));
        println!(
            "[mpedb] shape={} mode={mode} rows={rows} n=base ms={:.2}",
            shape.name,
            ms(base_d)
        );
        for &n in ns {
            // `equal` = one chunk per thread; `morsel` = 8N chunks pulled from
            // a shared counter, so a fast core that finishes early steals the
            // rest instead of idling behind an E-core straggler.
            for (sched, nchunks) in [("equal", n), ("morsel", (8 * n).min(rows))] {
                let parts = bounds(rows, nchunks);
                let (d, ans) = timed(|| {
                    let next = AtomicUsize::new(0);
                    let partials: Vec<Ans> = std::thread::scope(|s| {
                        let handles: Vec<_> = (0..n)
                            .map(|_| {
                                s.spawn(|| {
                                    let mut acc = Ans::default();
                                    loop {
                                        let i = next.fetch_add(1, Relaxed);
                                        let Some(&(lo, hi)) = parts.get(i) else { break };
                                        acc.absorb(run_part(
                                            db,
                                            shape,
                                            ph,
                                            &[Value::Int(lo), Value::Int(hi)],
                                        ));
                                    }
                                    acc
                                })
                            })
                            .collect();
                        handles.into_iter().map(|h| h.join().unwrap()).collect()
                    });
                    let mut merged = Ans::default();
                    for p in partials {
                        merged.absorb(p);
                    }
                    merged
                });
                assert_eq!(ans, base_ans, "partitioned answer diverged (shape={})", shape.name);
                println!(
                    "[mpedb] shape={} mode={mode} rows={rows} n={n} sched={sched} ms={:.2} speedup={:.2}",
                    shape.name,
                    ms(d),
                    ms(base_d) / ms(d)
                );
            }
        }
    }
}

// ------------------------------------------------- snapshot pinning evidence

/// Runs AFTER the sweep (it commits marker updates and does not undo them).
fn verify_snapshots(t: &Tmp, rows: usize, n: usize) {
    const BIG: i64 = 1_000_000_000_000;
    let expect: i64 = (rows as i64) * (rows as i64 - 1) / 2; // sum(a), a = id
    let hash = t.db.prepare("SELECT id, a FROM src WHERE id >= $1 AND id < $2").unwrap();

    // Sidecar engine handle on the same FILE: the only way to observe txn ids
    // today. Nothing in the facade exposes them, and :memory: cannot be
    // re-attached at all — both are named findings.
    let sidecar = t.path.as_ref().map(|p| {
        mpedb_core::Engine::open_from_file(std::path::Path::new(p)).unwrap()
    });
    let txn_of = |e: &mpedb_core::Engine| e.begin_read().unwrap().txn_id();

    let t0 = sidecar.as_ref().map(&txn_of);
    let mut streams: Vec<_> = bounds(rows, n)
        .into_iter()
        .map(|(lo, hi)| {
            t.db.stream_query(&hash, &[Value::Int(lo), Value::Int(hi)]).unwrap()
        })
        .collect();
    let t1 = sidecar.as_ref().map(&txn_of);
    match (t0, t1) {
        (Some(a), Some(b)) => {
            assert_eq!(a, b, "a commit slipped between the N stream opens");
            println!("[snapshot] txn_id constant across {n} stream opens: {a} (sidecar engine)");
        }
        _ => println!(
            "[snapshot] txn_id check skipped (:memory: — no facade access to txn ids, \
             and a private memfd cannot be re-attached)"
        ),
    }

    // The adversarial write: move one row in EVERY partition, after every
    // stream is open. A stream that pins lazily (or re-pins) will see it.
    let mut w = t.db.begin().unwrap();
    for (lo, hi) in bounds(rows, n) {
        let mid = (lo + hi) / 2;
        w.query(&format!("UPDATE src SET a = a + {BIG} WHERE id = {mid}"), &[]).unwrap();
    }
    w.commit().unwrap();
    if let (Some(e), Some(b)) = (sidecar.as_ref(), t1) {
        let t2 = txn_of(e);
        assert!(t2 > b, "marker commit must advance the txn id");
        println!("[snapshot] marker commit advanced txn_id to {t2}");
    }

    let mut total = 0i64;
    for s in &mut streams {
        while let Some(r) = s.next().unwrap() {
            total += int(&r[1]);
        }
    }
    assert_eq!(
        total, expect,
        "a partition stream observed the post-open commit — snapshot pin broken"
    );
    println!("[snapshot] {n} streams drained AFTER the commit: sum unchanged ({total}) — all pinned at open");

    let fresh = match t.db.query("SELECT sum(a) FROM src", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => int(&rows[0][0]),
        _ => unreachable!(),
    };
    assert_eq!(fresh, expect + BIG * n as i64, "a fresh read must see the markers");
    println!("[snapshot] fresh read sees all {n} markers — the pin was the streams', not the file's");
}

// ------------------------------------------------------------ sqlite control

fn sqlite_load(conn: &rusqlite::Connection, rows: usize) {
    conn.execute_batch(
        "CREATE TABLE src(id INTEGER PRIMARY KEY, g10 INTEGER, gk INTEGER, a INTEGER, t TEXT);\n\
         CREATE TABLE dim(id INTEGER PRIMARY KEY, k INTEGER);",
    )
    .unwrap();
    let mut i = 0;
    while i < rows {
        let end = (i + 10_000).min(rows);
        let tx = conn.unchecked_transaction().unwrap();
        let mut j = i;
        while j < end {
            let stop = (j + 500).min(end);
            let vals: Vec<String> = (j..stop)
                .map(|k| format!("({k}, {}, {}, {k}, 'payload text row {k}')", k % 10, k % 10_000))
                .collect();
            tx.execute_batch(&format!(
                "INSERT INTO src (id, g10, gk, a, t) VALUES {}",
                vals.join(", ")
            ))
            .unwrap();
            j = stop;
        }
        tx.commit().unwrap();
        i = end;
    }
    let vals: Vec<String> = (0..DIM_ROWS).map(|j| format!("({j}, {})", j * 7)).collect();
    conn.execute_batch(&format!("INSERT INTO dim (id, k) VALUES {}", vals.join(", ")))
        .unwrap();
}

fn sqlite_ans(conn: &rusqlite::Connection, sql: &str, kind: Kind) -> Ans {
    let mut stmt = conn.prepare_cached(sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Ans::default();
    while let Some(r) = rows.next().unwrap() {
        let e = match kind {
            Kind::Scalar | Kind::Stream => {
                let c: i64 = r.get(0).unwrap();
                let s: i64 = r.get(1).unwrap_or(0);
                (0i64, (c, s))
            }
            Kind::Groups => {
                let g: i64 = r.get(0).unwrap();
                (g, (r.get(1).unwrap(), r.get(2).unwrap()))
            }
        };
        out.absorb(Ans(BTreeMap::from([e])));
    }
    out
}

fn sqlite_control(mode: &str, rows: usize, ns: &[usize]) {
    let path = tmp_path(&format!("sqlite-{rows}")).replace(".mpedb", ".sqlite");
    let conn = match mode {
        "file" => {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;").unwrap();
            c
        }
        _ => rusqlite::Connection::open_in_memory().unwrap(),
    };
    sqlite_load(&conn, rows);
    for shape in SHAPES {
        if shape.kind == Kind::Stream {
            continue; // the probe-side fold is mpedb-specific
        }
        let (d, _) = timed(|| sqlite_ans(&conn, shape.base_sql, shape.kind));
        println!("[sqlite] shape={} mode={mode} rows={rows} n=1 ms={:.2}", shape.name, ms(d));
    }
    // sqlite CAN be hand-partitioned too — one CONNECTION per thread on a
    // file DB (private page caches, no shared-snapshot guarantee). Measure
    // it for count so the mpedb curves are compared against sqlite's best
    // possible parallel story, not a strawman.
    if mode == "file" {
        let base_sql = "SELECT count(*) FROM src";
        let part_sql = "SELECT count(*) FROM src WHERE id >= ?1 AND id < ?2";
        let (base_d, _) = timed(|| sqlite_ans(&conn, base_sql, Kind::Scalar));
        for &n in ns {
            let parts = bounds(rows, n);
            let (d, _) = timed(|| {
                let partials: Vec<i64> = std::thread::scope(|s| {
                    let handles: Vec<_> = parts
                        .iter()
                        .map(|&(lo, hi)| {
                            let p: &str = &path;
                            s.spawn(move || {
                                let c = rusqlite::Connection::open(p).unwrap();
                                c.query_row(part_sql, [lo, hi], |r| r.get::<_, i64>(0)).unwrap()
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                Ans(BTreeMap::from([(0, (partials.iter().sum(), 0))]))
            });
            println!(
                "[sqlite] shape=count mode=file rows={rows} n={n} ms={:.2} speedup={:.2} (one conn/thread)",
                ms(d),
                ms(base_d) / ms(d)
            );
        }
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

// --------------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: par_ceiling <file|mem> <rows>   (see module docs)");
        std::process::exit(2);
    }
    let mode = args[1].clone();
    let rows: usize = args[2].parse().expect("rows must be a number");
    let ns: Vec<usize> = std::env::var("PAR_SWEEP")
        .unwrap_or_else(|_| "1,2,4,8,11".into())
        .split(',')
        .map(|s| s.trim().parse().expect("PAR_SWEEP must be ints"))
        .collect();

    let t = open_fixture(&mode, rows);
    let t0 = Instant::now();
    load(&t.db, rows);
    println!("[fixture] mode={mode} rows={rows} loaded in {:.1} s", t0.elapsed().as_secs_f64());

    sweep(&t, &mode, rows, &ns);
    verify_snapshots(&t, rows, *ns.last().unwrap());

    if std::env::var("PAR_SQLITE").as_deref() != Ok("0") {
        sqlite_control(&mode, rows, &ns);
    }
}
