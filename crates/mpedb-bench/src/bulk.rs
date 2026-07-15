//! Bulk MB/s: how fast can bytes actually get in and out?
//!
//! The point-op suite measures ops/s on ~30-byte rows, which is dominated by
//! per-row overhead. This section instead pushes a **blob payload** through each
//! engine and reports MB/s, next to the number that gives it meaning:
//!
//! **the raw-Rust baseline** — the same bytes written to a plain file with
//! `std::fs`, on the same medium, under the same durability promise. An engine's
//! MB/s alone says nothing (it is mostly a property of the disk); an engine's
//! MB/s *as a fraction of what the medium can do* is the honest number, and it
//! is what this module reports.
//!
//! Also **scan MB/s** — reading it all back, the shape analytics actually hits.
//!
//! # Where the bulk write actually spends its time
//!
//! Measured 2026-07-15 with `examples/bulk_only.rs`, which does ONLY the blob
//! write so a trace attributes to it and not to the point-op suite that shares
//! the `--io` run. Linux, tmpfs, `durability = "none"`, 128 MiB logical:
//!
//! **1. It is not I/O.** `strace -c` over the whole write: 14 `write`, 5
//! `getpid`, and nothing else of substance — the syscall time is close/munmap
//! at teardown. Every microsecond is user-space. That rules out the entire
//! class of "it is the msync/write pattern" explanations at a stroke.
//!
//! **2. It is per-ROW, not per-byte.** The same 128 MiB, at different value
//! sizes:
//!
//! ```text
//!      64 B (2097152 rows):  50 MiB/s   = 1.22 µs/row
//!     256 B ( 524288 rows): 133 MiB/s   = 1.84 µs/row
//!    4096 B (  32768 rows): 349 MiB/s   = 11.2 µs/row
//!   65536 B (   2048 rows): 726 MiB/s   = 86.1 µs/row
//! ```
//!
//! 64 B and 256 B cost nearly the same PER ROW despite 4× the bytes: there is
//! a fixed ~1 µs per row that the payload does not touch. Copies would show as
//! a flat MiB/s; instead it climbs 14×.
//!
//! A real CPU profile (Raspberry Pi 3 B+, armv7, `perf record` — the only box
//! to hand with a working profiler) puts the ~1 µs in: musl's memcpy (~24%),
//! malloc/free (~14%), and `DefaultHasher` (~15%). That last one is the
//! `HashSet<u64>` of COWed pages — `page_mut` hashes on EVERY call, and a
//! single row touches several pages.
//!
//! Swapping SipHash for fxhash was measured properly and **taken**: **+3.5% on
//! armv7** (95% CI [+2.1, +4.9], n=15 pairs) and **nothing measurable on
//! x86-64** (-0.1%, CI [-2.2, +1.9], n=25 pairs). Note it is not 15% — removing
//! 15% of a profile does not hand back 15%, since the hash still happens, just
//! cheaper.
//!
//! The `contains` itself cannot go: it is the COW guard, and catching a
//! violation of it in production is the point. What could remove the hash
//! entirely, on every platform, is a BITSET — page ids are dense and bounded by
//! `high_water`, so a shift and a mask replace it. Untried.
//!
//! # How to measure this without fooling yourself
//!
//! Both of those numbers are the SECOND answer. The first attempt said
//! "+2/-2/+0.6%" — pure noise, because the two binaries were **identical**
//! (`git stash` took the example with it, the build failed, and `cp` copied the
//! same file twice). **md5 the arms before believing an A/B.**
//!
//! The second attempt, 3 alternating reps on the dev box, said "-1.6%, a
//! regression" and that was wrong too. Run-to-run CV on this dev box is **9.0%**
//! (mean 332, sd 30, range 255-351 over 10 reps): three reps cannot resolve a
//! 2% effect, and the "regression" was noise that got as far as a commit
//! message before anyone checked.
//!
//! A Raspberry Pi 3 B+ — 11x slower, and running a live ADS-B decoder the whole
//! time — has a CV of **1.6%** (mean 30.0, sd 0.47). Steady load beats
//! fast-but-bursty. **Use the Pi for A/B decisions**, with paired alternating
//! arms and enough reps to put a confidence interval on the difference; use the
//! fast machines for absolute numbers.
//!
//! (Numbers from a musl build: musl's memcpy is slower than glibc's, so the
//! 24% would look different against glibc. The hashing would not.)
//!
//! **3. Most of that fixed cost is the ENGINE, not SQL.** `bulk_only … raw`
//! bypasses the SQL layer (no plan lookup, no param validation, no expression
//! IR) for the engine's typed row API:
//!
//! ```text
//!     64 B: sql  48 vs raw  62 MiB/s   — SQL is 23% of the rate
//!    256 B: sql 131 vs raw 160 MiB/s   — 18%
//!   4096 B: sql 341 vs raw 378 MiB/s   — 10%
//! ```
//!
//! So ~1 µs/row remains with SQL removed entirely: btree descent, COW page
//! allocation, freelist, dirty set. **That is the target.** Fewer rows or
//! cheaper rows — not fewer copies.
//!
//! (Caveat: a shared dev box, not an isolated bench. The syscall COUNTS are
//! exact regardless, and a 14× spread across the size sweep is far outside
//! noise. Absolute MiB/s here is not comparable to `RESULTS-*.md`.)
//!
//! # Dead ends, measured — do not re-run these
//!
//! Two explanations were proposed in the task and are both **measurably too
//! small to matter** at the 4 KiB payload this module uses:
//!
//! - **The API-forced clone.** sqlite's `execute` binds `&buf` (a borrow);
//!   mpedb's `Value::Blob` owns its bytes, so the harness clones per row — and
//!   so would a real caller. That is a genuine API cost, not a harness artifact.
//!   It is also **~2%**: a 4 KiB `Vec<u8>` clone measures ~119 ns (≈33 GiB/s)
//!   against a ~6.5 µs/row budget at 602 MiB/s. (Measure it with `black_box` on
//!   both sides or the optimizer deletes the loop and reports terabytes/s.)
//! - **`dirty.insert(id)` per touched page.** A `HashSet<u64>` insert, ~2–3 per
//!   4 KiB row: ~1%.
//!
//! Two earlier guesses died the same way: an "overflow-page cliff" (4080 B =
//! 619.8 MiB/s vs 4096 B = 614.9 — 0.8% apart) and an msync-granularity theory
//! on macOS (`F_FULLFSYNC` is per-FD, not per-range). Four hypotheses, four
//! measurements, four corpses. The size sweep above is what a measurement
//! looks like when it actually says something.
//!
//! NOT measured here: write amplification. The obvious proxy — physical file
//! bytes per logical byte — is meaningless for mpedb, whose file is preallocated
//! to a fixed `size_mb` and never grows, so the ratio would report our own
//! provisioning choice (and would have printed a suspiciously exact `4.00x`
//! because the harness sizes the file at 4x the payload). Measuring it honestly
//! needs per-process block-layer accounting (`/proc/self/io` `write_bytes`),
//! which is Linux-only and does not see PostgreSQL's server-side writes at all.
//! Left out rather than shipped wrong.
//!
//! Honesty: "MB" is **logical payload** (rows × value bytes), never the physical
//! file — an engine cannot look good by storing less than it was asked to.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::util::{err, BResult};

/// 1 MiB, the unit for every MB/s in this module.
const MIB: f64 = 1024.0 * 1024.0;

/// One measured bulk cell.
pub struct BulkRow {
    pub engine: String,
    pub config: String,
    pub class: &'static str,
    /// Logical payload MiB written.
    pub logical_mib: f64,
    pub write_mibs: f64,
    pub scan_mibs: f64,
    /// True for the raw baseline row (the denominator), false for engines.
    pub is_baseline: bool,
}

pub struct BulkCfg {
    /// Logical payload per cell.
    pub total_mib: u64,
    /// Bytes per row value (the blob column).
    pub value_bytes: usize,
    /// Rows per transaction/commit for the durable cells.
    pub batch_rows: i64,
}

impl BulkCfg {
    pub fn full() -> BulkCfg {
        BulkCfg {
            total_mib: 256,
            value_bytes: 4096,
            batch_rows: 256,
        }
    }
    pub fn quick() -> BulkCfg {
        BulkCfg {
            total_mib: 16,
            value_bytes: 4096,
            batch_rows: 256,
        }
    }
    fn rows(&self) -> i64 {
        (self.total_mib as usize * 1024 * 1024 / self.value_bytes) as i64
    }
    fn logical_mib(&self) -> f64 {
        (self.rows() as usize * self.value_bytes) as f64 / MIB
    }
}

/// A deterministic, incompressible-ish payload: engines/filesystems that
/// transparently compress must not get a free ride. (APFS does not compress by
/// default and neither do our engines, but the moment one did, a zero-filled
/// buffer would silently make it look infinitely fast.)
fn payload(value_bytes: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(value_bytes);
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    while v.len() < value_bytes {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(value_bytes);
    v
}

// ---------------------------------------------------------------- raw baseline

/// The medium's ceiling: `std::fs` write of the same payload, same durability.
///
/// `durable` mirrors the commit-class promise — the barrier runs before we stop
/// the clock, and it is `mpedb_core::durability_barrier`, i.e. literally the call
/// the engine's own durable commit waits on (F_FULLFSYNC on Apple). A baseline
/// using plain `fsync()` there would beat a truly durable engine by ~10x and we
/// would print it as a result.
///
/// Two things this baseline has to get right or it stops being a denominator:
///
/// **Buffered writes.** It writes in 1 MiB chunks, not one syscall per 4 KiB row.
/// An unbuffered per-row baseline measures syscall overhead, and the engines —
/// which all batch internally — then "beat the raw file", which is nonsense (we
/// measured SQLite at 103% of raw before this).
///
/// **Warm reads, like the engines get.** The read is NOT cache-dropped: the
/// engines scan data they just wrote, straight out of the page cache, so a
/// cold-vs-warm comparison would be rigged (mpedb scanned at 266% of a
/// cache-dropped raw read). Both sides warm measures the software path, which is
/// the honest thing this column can compare. Neither number is a disk-read
/// benchmark.
pub fn raw_baseline(dir: &Path, cfg: &BulkCfg, durable: bool) -> BResult<(f64, f64)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("raw-baseline.bin");
    let _ = std::fs::remove_file(&path);
    // 1 MiB of the row payload, repeated — buffered like any real writer
    let row = payload(cfg.value_bytes, 0x5eed);
    let per_chunk = (1 << 20) / cfg.value_bytes;
    let chunk_buf: Vec<u8> = row.iter().cycle().take(per_chunk * cfg.value_bytes).copied().collect();
    let rows = cfg.rows();
    let chunks = rows as usize / per_chunk;
    let tail = rows as usize % per_chunk;

    // --- write
    let t0 = Instant::now();
    {
        let mut f = std::fs::File::create(&path)?;
        for _ in 0..chunks {
            f.write_all(&chunk_buf)?;
        }
        for _ in 0..tail {
            f.write_all(&row)?;
        }
        f.flush()?;
        if durable {
            use std::os::unix::io::AsRawFd;
            if mpedb_core::durability_barrier(f.as_raw_fd()) != 0 {
                return err("raw baseline fsync failed");
            }
        }
    }
    let write_mibs = cfg.logical_mib() / t0.elapsed().as_secs_f64();

    // --- read back (warm, see above)
    let t1 = Instant::now();
    let mut f = std::fs::File::open(&path)?;
    let mut chunk = vec![0u8; 1 << 20];
    let mut total = 0usize;
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    let read_mibs = total as f64 / MIB / t1.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&path);
    Ok((write_mibs, read_mibs))
}

// ---------------------------------------------------------------------- mpedb

pub fn mpedb_bulk(dir: &Path, durability: &str, cfg: &BulkCfg) -> BResult<BulkRow> {
    use mpedb::{params, Database, ExecResult, Value};

    std::fs::create_dir_all(dir)?;
    let path = dir.join("bulk.mpedb");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("bulk.mpedb-wal"));

    // headroom: COW + freelist churn needs well over the logical payload
    let size_mb = (cfg.total_mib * 4).max(64);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 64
durability = "{durability}"

[[table]]
name = "blobs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "data"
  type = "blob"
  nullable = false
"#,
        path.display()
    );
    let cfg_path = dir.join("bulk-config.toml");
    std::fs::write(&cfg_path, toml)?;
    let db = Database::open(&cfg_path)?;
    let ins = db.prepare("INSERT INTO blobs (id, data) VALUES ($1, $2)")?;
    let buf = payload(cfg.value_bytes, 0xb10b);
    let rows = cfg.rows();

    // --- write, batched in WriteSessions (one commit per batch)
    let t0 = Instant::now();
    let mut id = 0i64;
    while id < rows {
        let n = cfg.batch_rows.min(rows - id);
        let mut s = db.begin()?;
        for k in 0..n {
            s.execute(&ins, &params![id + k, Value::Blob(buf.clone())])?;
        }
        s.commit()?;
        id += n;
    }
    let write_mibs = cfg.logical_mib() / t0.elapsed().as_secs_f64();

    // --- full scan
    let t1 = Instant::now();
    let scanned = match db.query("SELECT data FROM blobs", &[])? {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Blob(b) => b.len(),
                _ => 0,
            })
            .sum::<usize>(),
        other => return err(format!("unexpected scan result: {other:?}")),
    };
    let scan_mibs = scanned as f64 / MIB / t1.elapsed().as_secs_f64();
    if scanned as f64 / MIB < cfg.logical_mib() * 0.99 {
        return err(format!(
            "mpedb scan returned {:.1} MiB, expected {:.1}",
            scanned as f64 / MIB,
            cfg.logical_mib()
        ));
    }

    drop(db);
    Ok(BulkRow {
        engine: "mpedb 0.1.0".into(),
        config: format!("durability={durability}"),
        class: class_of(durability),
        logical_mib: cfg.logical_mib(),
        write_mibs,
        scan_mibs,
        is_baseline: false,
    })
}

fn class_of(durability: &str) -> &'static str {
    match durability {
        "none" => "none-class",
        _ => "commit-class",
    }
}

// --------------------------------------------------------------------- sqlite

pub fn sqlite_bulk(dir: &Path, durable: bool, cfg: &BulkCfg) -> BResult<BulkRow> {
    use rusqlite::Connection;

    std::fs::create_dir_all(dir)?;
    let path = dir.join("bulk.sqlite3");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    let conn = Connection::open(&path)?;
    if durable {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        // same real barrier as mpedb on Apple — see eng_sqlite::fullfsync_on_apple
        if cfg!(target_vendor = "apple") {
            conn.pragma_update(None, "fullfsync", true)?;
        }
    } else {
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        conn.pragma_update(None, "synchronous", "OFF")?;
    }
    conn.execute_batch("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB NOT NULL) STRICT;")?;

    let buf = payload(cfg.value_bytes, 0xb10b);
    let rows = cfg.rows();
    let mut conn = conn;

    let t0 = Instant::now();
    let mut id = 0i64;
    while id < rows {
        let n = cfg.batch_rows.min(rows - id);
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare_cached("INSERT INTO blobs (id, data) VALUES (?1, ?2)")?;
            for k in 0..n {
                ins.execute(rusqlite::params![id + k, &buf])?;
            }
        }
        tx.commit()?;
        id += n;
    }
    let write_mibs = cfg.logical_mib() / t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut scanned = 0usize;
    {
        let mut stmt = conn.prepare("SELECT data FROM blobs")?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let b: Vec<u8> = r.get(0)?;
            scanned += b.len();
        }
    }
    let scan_mibs = scanned as f64 / MIB / t1.elapsed().as_secs_f64();

    drop(conn);
    Ok(BulkRow {
        engine: "SQLite 3.45.0".into(),
        config: if durable {
            "sync=FULL+WAL".into()
        } else {
            "sync=OFF+MEMORY".into()
        },
        class: if durable { "commit-class" } else { "none-class" },
        logical_mib: cfg.logical_mib(),
        write_mibs,
        scan_mibs,
        is_baseline: false,
    })
}

// ----------------------------------------------------------------- postgresql

pub fn pg_bulk(client: &mut postgres::Client, label: &str, class: &'static str, cfg: &BulkCfg) -> BResult<BulkRow> {
    client.batch_execute(
        "DROP TABLE IF EXISTS blobs; CREATE TABLE blobs (id bigint PRIMARY KEY, data bytea NOT NULL);",
    )?;
    let buf = payload(cfg.value_bytes, 0xb10b);
    let rows = cfg.rows();

    let t0 = Instant::now();
    let mut id = 0i64;
    while id < rows {
        let n = cfg.batch_rows.min(rows - id);
        let mut tx = client.transaction()?;
        {
            let stmt = tx.prepare("INSERT INTO blobs (id, data) VALUES ($1, $2)")?;
            for k in 0..n {
                tx.execute(&stmt, &[&(id + k), &buf.as_slice()])?;
            }
        }
        tx.commit()?;
        id += n;
    }
    let write_mibs = cfg.logical_mib() / t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut scanned = 0usize;
    for row in client.query("SELECT data FROM blobs", &[])? {
        let b: &[u8] = row.get(0);
        scanned += b.len();
    }
    let scan_mibs = scanned as f64 / MIB / t1.elapsed().as_secs_f64();

    Ok(BulkRow {
        engine: "PostgreSQL".into(),
        config: label.to_string(),
        class,
        logical_mib: cfg.logical_mib(),
        write_mibs,
        scan_mibs,
        is_baseline: false,
    })
}
