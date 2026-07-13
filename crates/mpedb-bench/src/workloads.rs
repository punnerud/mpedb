//! The five workloads, engine-agnostic.
//!
//! Point workloads self-calibrate: a short calibration burst estimates the
//! op rate, N is chosen so the measured cell runs ~`target_s` seconds
//! (clamped to keep every cell in the required 2-10 s window at sane rates),
//! then exactly N operations are measured one by one.
//!
//! Timed workloads (contended-writes, read-while-write) run for a fixed wall
//! time with per-thread connections and aggregate per-op latencies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{err, stats_from, BResult, LatStats, Rng};

/// Distinct id bases so key spaces never collide within a (reset) cell.
const INSERT_BASE: i64 = 1_000_000_000;
const CONTENDED_BASE: i64 = 3_000_000_000;
const RWW_BASE: i64 = 4_000_000_000;
/// Per-thread id stride in timed workloads.
const THREAD_STRIDE: i64 = 100_000_000;

#[derive(Clone, Copy)]
pub struct RunCfg {
    /// Rows seeded before select/update/read workloads.
    pub seed_rows: i64,
    /// Wall-time target for calibrated point cells.
    pub target_s: f64,
    /// Calibration burst length.
    pub calib_s: f64,
    /// Fixed wall time for the two timed workloads.
    pub timed_s: f64,
    /// Op-count caps (memory / db-size bound), inserts vs reads/updates.
    pub max_insert_ops: u64,
    pub max_point_ops: u64,
}

impl RunCfg {
    pub fn full() -> RunCfg {
        RunCfg {
            seed_rows: 50_000,
            target_s: 3.0,
            calib_s: 0.3,
            timed_s: 5.0,
            max_insert_ops: 2_000_000,
            max_point_ops: 20_000_000,
        }
    }

    pub fn quick() -> RunCfg {
        RunCfg {
            seed_rows: 5_000,
            target_s: 0.4,
            calib_s: 0.1,
            timed_s: 1.0,
            max_insert_ops: 100_000,
            max_point_ops: 500_000,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    PointInsert,
    PointSelect,
    PointUpdate,
    ContendedWrites,
    ReadWhileWrite,
}

pub const ALL_WORKLOADS: [Workload; 5] = [
    Workload::PointInsert,
    Workload::PointSelect,
    Workload::PointUpdate,
    Workload::ContendedWrites,
    Workload::ReadWhileWrite,
];

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Workload::PointInsert => "point-insert",
            Workload::PointSelect => "point-select",
            Workload::PointUpdate => "point-update",
            Workload::ContendedWrites => "contended-writes",
            Workload::ReadWhileWrite => "read-while-write",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Workload::PointInsert => "single client, autocommit, N sequential-key inserts",
            Workload::PointSelect => "single client, N random PK lookups (warm)",
            Workload::PointUpdate => "single client, N random PK updates (autocommit)",
            Workload::ContendedWrites => "4 threads x autocommit inserts, distinct keys, 5 s",
            Workload::ReadWhileWrite => "3 reader threads + 1 writer thread, 5 s",
        }
    }
}

/// A completed cell. `reads`/`writes` filled per workload:
/// point-select → reads only; point-insert/update/contended → writes only;
/// read-while-write → both.
#[derive(Debug, Clone)]
pub struct CellResult {
    pub reads: Option<LatStats>,
    pub writes: Option<LatStats>,
}

pub fn run_workload(engine: &mut dyn Engine, w: Workload, cfg: &RunCfg) -> BResult<CellResult> {
    engine
        .reset_and_seed(cfg.seed_rows)
        .map_err(|e| format!("setup (reset+seed): {e}"))?;
    match w {
        Workload::PointInsert => {
            let s = run_point(&mut *engine.conn()?, PointKind::Insert, cfg)?;
            Ok(CellResult {
                reads: None,
                writes: Some(s),
            })
        }
        Workload::PointSelect => {
            let s = run_point(&mut *engine.conn()?, PointKind::Select, cfg)?;
            Ok(CellResult {
                reads: Some(s),
                writes: None,
            })
        }
        Workload::PointUpdate => {
            let s = run_point(&mut *engine.conn()?, PointKind::Update, cfg)?;
            Ok(CellResult {
                reads: None,
                writes: Some(s),
            })
        }
        Workload::ContendedWrites => {
            let (_, writes) = run_timed(engine, 4, 0, CONTENDED_BASE, cfg)?;
            Ok(CellResult {
                reads: None,
                writes,
            })
        }
        Workload::ReadWhileWrite => {
            let (reads, writes) = run_timed(engine, 1, 3, RWW_BASE, cfg)?;
            Ok(CellResult { reads, writes })
        }
    }
}

// ------------------------------------------------------------ point workloads

/// Single-client sequential point-insert (the `PointInsert` inner loop),
/// exposed for the durability-class comparison (`dur_compare`).
pub fn measure_single_insert(conn: &mut dyn Conn, cfg: &RunCfg) -> BResult<LatStats> {
    run_point(conn, PointKind::Insert, cfg)
}

/// Batched point-insert: each measured op is ONE commit of `batch` rows (a
/// transaction / WriteSession). Reports throughput in ROWS/s (`ops` = rows)
/// with per-BATCH commit-latency percentiles — the amortization the
/// durable-on-ack class buys from batching.
pub fn measure_batched_insert(
    conn: &mut dyn Conn,
    cfg: &RunCfg,
    batch: i64,
) -> BResult<LatStats> {
    let mut next = INSERT_BASE;
    let calib_start = Instant::now();
    let calib_dur = Duration::from_secs_f64(cfg.calib_s);
    let mut calib_batches = 0u64;
    while calib_start.elapsed() < calib_dur || calib_batches < 3 {
        conn.insert_batch(next, batch)?;
        next += batch;
        calib_batches += 1;
    }
    let rate = calib_batches as f64 / calib_start.elapsed().as_secs_f64();
    let cap = (cfg.max_insert_ops / batch.max(1) as u64).max(5);
    let n_batches = ((rate * cfg.target_s) as u64).clamp(5, cap);

    let mut lat: Vec<u32> = Vec::with_capacity(n_batches as usize);
    let t0 = Instant::now();
    for _ in 0..n_batches {
        let t = Instant::now();
        conn.insert_batch(next, batch)?;
        next += batch;
        lat.push(t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
    }
    let elapsed = t0.elapsed();
    let mut stats = stats_from(lat, elapsed);
    stats.ops = n_batches * batch as u64; // ops/s = ROWS committed per second
    Ok(stats)
}

#[derive(Clone, Copy)]
enum PointKind {
    Insert,
    Select,
    Update,
}

fn run_point(conn: &mut dyn Conn, kind: PointKind, cfg: &RunCfg) -> BResult<LatStats> {
    let mut rng = Rng::seeded(&[0xBEEF, cfg.seed_rows as u64]);
    let mut next_id = INSERT_BASE;
    let seed_rows = cfg.seed_rows as u64;

    let op = |conn: &mut dyn Conn, rng: &mut Rng, next_id: &mut i64| -> BResult<()> {
        match kind {
            PointKind::Insert => {
                let id = *next_id;
                *next_id += 1;
                conn.insert(id, &email_for(id), age_for(id))
            }
            PointKind::Select => {
                let id = rng.below(seed_rows) as i64;
                if conn.select(id)? {
                    Ok(())
                } else {
                    err(format!("seeded row id={id} not found"))
                }
            }
            PointKind::Update => {
                let id = rng.below(seed_rows) as i64;
                conn.update(id, rng.below(100) as i64)
            }
        }
    };

    // Calibration burst (also warms caches for the "warm" select cell).
    let calib_start = Instant::now();
    let calib_dur = Duration::from_secs_f64(cfg.calib_s);
    let mut calib_ops = 0u64;
    while calib_start.elapsed() < calib_dur || calib_ops < 20 {
        op(conn, &mut rng, &mut next_id)?;
        calib_ops += 1;
    }
    let rate = calib_ops as f64 / calib_start.elapsed().as_secs_f64();
    let cap = match kind {
        PointKind::Insert => cfg.max_insert_ops,
        _ => cfg.max_point_ops,
    };
    let n = ((rate * cfg.target_s) as u64).clamp(50, cap);

    // Measured run: exactly N ops, per-op latency.
    let mut lat: Vec<u32> = Vec::with_capacity(n as usize);
    let t0 = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        op(conn, &mut rng, &mut next_id)?;
        lat.push(t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
    }
    Ok(stats_from(lat, t0.elapsed()))
}

// ------------------------------------------------------------ timed workloads

type ThreadOut = BResult<(Vec<u32>, f64)>;

/// `writers` insert distinct fresh keys; `readers` do random point selects on
/// the seeded rows. Runs `cfg.timed_s` seconds of wall time. Returns
/// aggregate (reads, writes); ops/s is total ops over the slowest thread's
/// elapsed time.
fn run_timed(
    engine: &dyn Engine,
    writers: usize,
    readers: usize,
    base_id: i64,
    cfg: &RunCfg,
) -> BResult<(Option<LatStats>, Option<LatStats>)> {
    // Open all connections up front so setup errors surface before spawning.
    let mut wconns: Vec<Box<dyn Conn>> = (0..writers)
        .map(|_| engine.conn())
        .collect::<BResult<_>>()?;
    let mut rconns: Vec<Box<dyn Conn>> = (0..readers)
        .map(|_| engine.conn())
        .collect::<BResult<_>>()?;

    let stop = AtomicBool::new(false);
    let seed_rows = cfg.seed_rows as u64;
    let (mut wouts, mut routs): (Vec<ThreadOut>, Vec<ThreadOut>) = (vec![], vec![]);

    std::thread::scope(|s| {
        let stop = &stop;
        let mut whandles = Vec::with_capacity(writers);
        for (t, mut conn) in wconns.drain(..).enumerate() {
            whandles.push(s.spawn(move || -> ThreadOut {
                let mut id = base_id + t as i64 * THREAD_STRIDE;
                let mut lat = Vec::new();
                let t0 = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    conn.insert(id, &email_for(id), age_for(id))?;
                    lat.push(t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
                    id += 1;
                }
                Ok((lat, t0.elapsed().as_secs_f64()))
            }));
        }
        let mut rhandles = Vec::with_capacity(readers);
        for (t, mut conn) in rconns.drain(..).enumerate() {
            rhandles.push(s.spawn(move || -> ThreadOut {
                let mut rng = Rng::seeded(&[0xF00D, t as u64]);
                let mut lat = Vec::new();
                let t0 = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let id = rng.below(seed_rows) as i64;
                    let t = Instant::now();
                    conn.select(id)?;
                    lat.push(t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
                }
                Ok((lat, t0.elapsed().as_secs_f64()))
            }));
        }

        std::thread::sleep(Duration::from_secs_f64(cfg.timed_s));
        stop.store(true, Ordering::Relaxed);
        wouts = whandles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| err("writer thread panicked")))
            .collect();
        routs = rhandles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| err("reader thread panicked")))
            .collect();
    });

    let aggregate = |outs: Vec<ThreadOut>| -> BResult<Option<LatStats>> {
        if outs.is_empty() {
            return Ok(None);
        }
        let mut all = Vec::new();
        let mut wall = 0f64;
        for o in outs {
            let (lat, elapsed) = o?;
            all.extend_from_slice(&lat);
            wall = wall.max(elapsed);
        }
        Ok(Some(stats_from(all, Duration::from_secs_f64(wall))))
    };
    Ok((aggregate(routs)?, aggregate(wouts)?))
}
