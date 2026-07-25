//! **Change notification under load: mpedb vs PostgreSQL LISTEN/NOTIFY (#139 S4).**
//!
//! The shape is DBOS's (dbos.dev/blog/postgres-listen-notify-scalability), because
//! their measurement is the reason this cell exists. They found that committing
//! a transaction that calls NOTIFY takes a **global exclusive lock**, held from
//! the start of commit until the transaction is fsync'ed — so notifying writes
//! cannot group-commit and serialize against each other. They measured 2.9K
//! writes/s that way, and 60K (20x) once the notifications were batched off the
//! commit path, at 15-100 ms latency.
//!
//! Two arms, run against both engines:
//!
//! * **A — notify per commit.** Every insert is its own transaction and every
//!   transaction notifies. This is the arm the global lock punishes.
//! * **B — batched.** `BATCH` inserts per transaction, one notification at the
//!   end. This is DBOS's workaround, and it is what tells us how much of arm A's
//!   result was the lock rather than the write.
//!
//! Both numbers matter and neither is the whole story, so both are reported:
//! **throughput** (writes/s) and **notification latency** (a listener's wakeup
//! minus the writer's commit, p50/p99).
//!
//! **Fairness (#122).** PostgreSQL runs with `fsync=on, synchronous_commit=on`,
//! so mpedb runs `durability = commit` — its slowest durable mode — not `none`.
//! Comparing a durable engine against a non-durable one is the mistake that
//! benchmark row already had to be corrected for once.
//!
//! **Platform honesty.** mpedb's wakeup is a real futex only on Linux; on macOS
//! `futex_wake_all` is a no-op and the waiter polls at ~200 us, so the M3 run is
//! expected to show a latency floor that says something about the platform, not
//! about the design. It is reported, not hidden.

use std::path::PathBuf;
use std::time::{Duration, Instant};

// The dependency graph carries two `fallible_iterator` versions, so the
// notification iterator's `next` is ambiguous unless it comes from the one
// `postgres` itself was built against.
use postgres::fallible_iterator::FallibleIterator;

use crate::eng_pg::PgServer;
use crate::util::BResult;

/// Inserts per arm. Small enough that a 2.9K/s engine finishes, large enough
/// that a 100K/s one is not measuring startup.
const ROWS: usize = 3_000;
/// Transactions per batch in arm B.
const BATCH: usize = 100;
/// Concurrent writers in arm C — the arm where a GLOBAL lock is visible at all.
const WRITERS: usize = 4;
/// Listener counts for arm D. PostgreSQL's `SignalBackends()` walks EVERY
/// listener in the cluster under an exclusive `NotifyQueueLock` (async.c:2337,
/// see design/PG-NOTIFY-ANATOMY.md), so its per-notify cost is O(all
/// listeners) rather than O(interested). mpedb wakes one futex word whatever
/// is parked on it. This arm is where that difference is either visible or is
/// not, and the answer also decides whether #141 N4 has a target.
const LISTENER_COUNTS: [usize; 3] = [1, 10, 100];
/// Rows for arm D. Lower than `ROWS`: the arm runs six times (three counts x
/// two engines) and the question is a ratio between counts, not an absolute.
const D_ROWS: usize = 1_000;

pub struct ArmResult {
    pub engine: &'static str,
    pub arm: &'static str,
    pub writes_per_sec: f64,
    pub lat_p50_us: u64,
    pub lat_p99_us: u64,
    pub notifications: usize,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

// ----------------------------------------------------------------- postgres

/// PostgreSQL, the reference. A trigger-free explicit `NOTIFY` per commit is
/// the friendliest reading of arm A — a trigger would only add per-row work on
/// top of the same lock.
fn pg_arm(
    pg: &PgServer,
    arm: &'static str,
    batch: usize,
    notify: bool,
) -> BResult<ArmResult> {
    let mut writer = postgres::Client::connect(&pg.conn_str(), postgres::NoTls)
        .map_err(|e| format!("pg writer connect: {e}"))?;
    writer
        .batch_execute(
            "DROP TABLE IF EXISTS nbench; CREATE TABLE nbench (id bigint primary key, v bigint);",
        )
        .map_err(|e| format!("pg schema: {e}"))?;

    let mut listener = postgres::Client::connect(&pg.conn_str(), postgres::NoTls)
        .map_err(|e| format!("pg listener connect: {e}"))?;
    listener.batch_execute("LISTEN nbench_ch;").map_err(|e| format!("pg listen: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<(u64, Instant)>();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_l = stop.clone();
    // The listener thread drains notifications and stamps arrival. libpq
    // surfaces them on the connection, so a cheap query drives the pump.
    let lt = std::thread::spawn(move || {
        let mut seen = 0usize;
        while !stop_l.load(std::sync::atomic::Ordering::Relaxed) {
            // Drive the connection so libpq surfaces pending notifications,
            // then drain them. A cheap query is the documented pump.
            let _ = listener.simple_query("SELECT 1");
            let mut notes = listener.notifications();
            let mut iter = notes.timeout_iter(Duration::from_millis(50));
            while let Ok(Some(n)) = iter.next() {
                let sent: u64 = n.payload().parse().unwrap_or(0);
                seen += 1;
                let _ = tx.send((sent, Instant::now()));
            }
        }
        seen
    });

    let t0 = Instant::now();
    let mut sent_at: Vec<(u64, Instant)> = Vec::new();
    let mut id = 0i64;
    let mut n_tx = 0usize;
    while (id as usize) < ROWS {
        let mut txn = writer.transaction().map_err(|e| format!("pg begin: {e}"))?;
        for _ in 0..batch {
            if id as usize >= ROWS {
                break;
            }
            txn.execute("INSERT INTO nbench (id, v) VALUES ($1, $2)", &[&id, &id])
                .map_err(|e| format!("pg insert: {e}"))?;
            id += 1;
        }
        let stamp = n_tx as u64;
        if notify {
            txn.batch_execute(&format!("NOTIFY nbench_ch, '{stamp}'"))
                .map_err(|e| format!("pg notify: {e}"))?;
            sent_at.push((stamp, Instant::now()));
        }
        txn.commit().map_err(|e| format!("pg commit: {e}"))?;
        n_tx += 1;
    }
    let elapsed = t0.elapsed();

    // Give in-flight notifications a moment, then stop the pump.
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let notifications = lt.join().unwrap_or(0);

    let mut lat: Vec<u64> = Vec::new();
    while let Ok((stamp, at)) = rx.try_recv() {
        if let Some((_, sent)) = sent_at.iter().find(|(s, _)| *s == stamp) {
            lat.push(at.duration_since(*sent).as_micros() as u64);
        }
    }
    lat.sort_unstable();
    Ok(ArmResult {
        engine: if notify { "postgres" } else { "postgres(ctl)" },
        arm,
        writes_per_sec: ROWS as f64 / elapsed.as_secs_f64(),
        lat_p50_us: pct(&lat, 0.50),
        lat_p99_us: pct(&lat, 0.99),
        notifications,
    })
}

// -------------------------------------------------------------------- mpedb

fn mpedb_arm(
    dir: &std::path::Path,
    arm: &'static str,
    batch: usize,
    durability: &str,
    listeners: usize,
) -> BResult<ArmResult> {
    use mpedb::{Config, Database, Value};

    // Slug: arm labels carry spaces, and a path is not the place to find out
    // whether every layer quotes them.
    let slug: String = arm.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let path = dir.join(format!("notify-{slug}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 16
durability = "{durability}"

[[table]]
name = "nbench"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(
        Config::from_toml_str(&toml).map_err(|e| format!("mpedb config: {e}"))?,
    )
    .map_err(|e| format!("mpedb open: {e}"))?;

    // The listener is a second handle on the same file — no server, no channel
    // registration: it parks on the table's generation.
    let listener_db = Database::open_with_config(
        Config::from_toml_str(&toml).map_err(|e| format!("mpedb config: {e}"))?,
    )
    .map_err(|e| format!("mpedb listener open: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<(u64, Instant)>();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_l = stop.clone();
    // listeners = 0 is the CONTROL arm: the engine still publishes every
    // generation, nobody parks, and the difference against the listening run
    // is what notification actually costs.
    let lt = (listeners > 0).then(|| std::thread::spawn(move || {
        let mut seen = 0usize;
        let mut gen = listener_db.change_generation(0).map(|(g, _)| g).unwrap_or(0);
        while !stop_l.load(std::sync::atomic::Ordering::Relaxed) {
            let woke = listener_db.wait_for_change(&[0], &[gen], Duration::from_millis(50));
            if !woke.is_empty() {
                let now = Instant::now();
                let g = listener_db.change_generation(0).map(|(x, _)| x).unwrap_or(gen);
                seen += 1;
                let _ = tx.send((g, now));
                gen = g;
            }
        }
        seen
    }));

    let t0 = Instant::now();
    let mut sent_at: Vec<(u64, Instant)> = Vec::new();
    let mut id = 0i64;
    while (id as usize) < ROWS {
        if batch == 1 {
            db.query(
                "INSERT INTO nbench (id, v) VALUES ($1, $2)",
                &[Value::Int(id), Value::Int(id)],
            )
            .map_err(|e| format!("mpedb insert: {e}"))?;
            id += 1;
        } else {
            let mut s = db.begin().map_err(|e| format!("mpedb begin: {e}"))?;
            for _ in 0..batch {
                if id as usize >= ROWS {
                    break;
                }
                s.query(
                    "INSERT INTO nbench (id, v) VALUES ($1, $2)",
                    &[Value::Int(id), Value::Int(id)],
                )
                .map_err(|e| format!("mpedb insert: {e}"))?;
                id += 1;
            }
            s.commit().map_err(|e| format!("mpedb commit: {e}"))?;
        }
        // The generation AFTER the commit is what the listener will observe.
        if let Some((g, _)) = db.change_generation(0) {
            sent_at.push((g, Instant::now()));
        }
    }
    let elapsed = t0.elapsed();

    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let notifications = lt.map(|h| h.join().unwrap_or(0)).unwrap_or(0);

    let mut lat: Vec<u64> = Vec::new();
    while let Ok((g, at)) = rx.try_recv() {
        if let Some((_, sent)) = sent_at.iter().find(|(s, _)| *s == g) {
            lat.push(at.duration_since(*sent).as_micros() as u64);
        }
    }
    lat.sort_unstable();
    let _ = std::fs::remove_file(&path);
    Ok(ArmResult {
        engine: if listeners == 0 { "mpedb(ctl)" } else { "mpedb" },
        arm,
        writes_per_sec: ROWS as f64 / elapsed.as_secs_f64(),
        lat_p50_us: pct(&lat, 0.50),
        lat_p99_us: pct(&lat, 0.99),
        notifications,
    })
}

/// **The arm that actually tests the thesis.** DBOS's 20x is a CONTENTION
/// result: Postgres's notify lock is global, so it only bites when several
/// transactions try to commit notifications at once. A single writer never
/// meets it — measured here at ~3% — so a single-writer cell cannot reproduce
/// their finding, and saying otherwise would be quoting their number for a
/// workload that does not produce it.
///
/// `WRITERS` writers, each committing `ROWS / WRITERS` rows with a notify per
/// commit. Throughput only; per-notification latency under contention needs a
/// per-writer stamp the shared channel does not carry.
fn pg_concurrent(pg: &PgServer, notify: bool) -> BResult<ArmResult> {
    let per = ROWS / WRITERS;
    let conn = pg.conn_str();
    {
        let mut c = postgres::Client::connect(&conn, postgres::NoTls)
            .map_err(|e| format!("pg setup: {e}"))?;
        c.batch_execute(
            "DROP TABLE IF EXISTS nconc; CREATE TABLE nconc (id bigserial primary key, w int);",
        )
        .map_err(|e| format!("pg schema: {e}"))?;
    }
    let t0 = Instant::now();
    let mut hs = Vec::new();
    for w in 0..WRITERS {
        let conn = conn.clone();
        hs.push(std::thread::spawn(move || -> Result<(), String> {
            let mut c = postgres::Client::connect(&conn, postgres::NoTls)
                .map_err(|e| format!("pg connect: {e}"))?;
            for _ in 0..per {
                let mut txn = c.transaction().map_err(|e| format!("begin: {e}"))?;
                txn.execute("INSERT INTO nconc (w) VALUES ($1)", &[&(w as i32)])
                    .map_err(|e| format!("insert: {e}"))?;
                if notify {
                    txn.batch_execute("NOTIFY nconc_ch, 'x'")
                        .map_err(|e| format!("notify: {e}"))?;
                }
                txn.commit().map_err(|e| format!("commit: {e}"))?;
            }
            Ok(())
        }));
    }
    for h in hs {
        h.join().map_err(|_| "writer panicked".to_string())??;
    }
    Ok(ArmResult {
        engine: if notify { "postgres" } else { "postgres(ctl)" },
        arm: if notify { "C concurrent" } else { "C conc no notify" },
        writes_per_sec: (per * WRITERS) as f64 / t0.elapsed().as_secs_f64(),
        lat_p50_us: 0,
        lat_p99_us: 0,
        notifications: 0,
    })
}

/// mpedb's side of the contention arm. Same shape: `WRITERS` processes' worth
/// of writers (threads here, each with its own handle) committing with the
/// notification publish in the path.
fn mpedb_concurrent(dir: &std::path::Path, listeners: usize) -> BResult<ArmResult> {
    use mpedb::{Config, Database, Value};
    let per = ROWS / WRITERS;
    let path = dir.join("notify-conc.mpedb");
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 32
durability = "wal"

[[table]]
name = "nconc"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "w"
  type = "int64"
"#,
        path.display()
    );
    let cfg = || {
        Config::from_toml_str(&toml).map_err(|e| format!("mpedb config: {e}"))
    };
    let seed = Database::open_with_config(cfg()?).map_err(|e| format!("mpedb open: {e}"))?;
    drop(seed);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lt = (listeners > 0).then(|| {
        let toml = toml.clone();
        let stop_l = stop.clone();
        std::thread::spawn(move || {
            let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
            let mut gen = db.change_generation(0).map(|(g, _)| g).unwrap_or(0);
            let mut seen = 0usize;
            while !stop_l.load(std::sync::atomic::Ordering::Relaxed) {
                if !db.wait_for_change(&[0], &[gen], Duration::from_millis(50)).is_empty() {
                    gen = db.change_generation(0).map(|(g, _)| g).unwrap_or(gen);
                    seen += 1;
                }
            }
            seen
        })
    });

    let t0 = Instant::now();
    let mut hs = Vec::new();
    for w in 0..WRITERS {
        let toml = toml.clone();
        hs.push(std::thread::spawn(move || -> Result<(), String> {
            let db = Database::open_with_config(
                Config::from_toml_str(&toml).map_err(|e| format!("cfg: {e}"))?,
            )
            .map_err(|e| format!("open: {e}"))?;
            for i in 0..per {
                let id = (w * per + i) as i64;
                db.query(
                    "INSERT INTO nconc (id, w) VALUES ($1, $2)",
                    &[Value::Int(id), Value::Int(w as i64)],
                )
                .map_err(|e| format!("insert: {e}"))?;
            }
            Ok(())
        }));
    }
    for h in hs {
        h.join().map_err(|_| "writer panicked".to_string())??;
    }
    let elapsed = t0.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let notifications = lt.map(|h| h.join().unwrap_or(0)).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    Ok(ArmResult {
        engine: if listeners == 0 { "mpedb(ctl)" } else { "mpedb" },
        arm: if listeners == 0 { "C conc no listener" } else { "C concurrent" },
        writes_per_sec: (per * WRITERS) as f64 / elapsed.as_secs_f64(),
        lat_p50_us: 0,
        lat_p99_us: 0,
        notifications,
    })
}

// ------------------------------------------------------------------- driver


/// **Arm D — listener scaling.** One writer, `n` listeners, on both engines.
///
/// This is the axis arms A-C leave untested: they all run a single listener,
/// so PostgreSQL's per-notify walk over every listener in the cluster costs
/// the same as a walk over one. Here the walk gets something to walk.
///
/// mpedb's side is a control on the same axis: `futex_wake_all` is one
/// syscall whatever is parked on the word, so the prediction is a flat line.
/// A line that is NOT flat would mean the wakeup fan-out costs the writer
/// something, which is exactly what #141 N4 would then have to fix.
fn pg_listener_scaling(pg: &PgServer, n: usize) -> BResult<ArmResult> {
    let conn = pg.conn_str();
    {
        let mut c = postgres::Client::connect(&conn, postgres::NoTls)
            .map_err(|e| format!("pg setup: {e}"))?;
        c.batch_execute("DROP TABLE IF EXISTS nscale; CREATE TABLE nscale (id bigserial primary key);")
            .map_err(|e| format!("pg schema: {e}"))?;
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut listeners = Vec::new();
    for _ in 0..n {
        let conn = conn.clone();
        let stop = stop.clone();
        let ready = ready.clone();
        listeners.push(std::thread::spawn(move || -> Result<usize, String> {
            let mut c = postgres::Client::connect(&conn, postgres::NoTls)
                .map_err(|e| format!("listener connect: {e}"))?;
            c.batch_execute("LISTEN nscale_ch").map_err(|e| format!("listen: {e}"))?;
            ready.fetch_add(1, std::sync::atomic::Ordering::Release);
            let mut seen = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                let mut notifs = c.notifications();
                let mut it = notifs.timeout_iter(Duration::from_millis(50));
                while let Some(_n) = it.next().map_err(|e| format!("notif: {e}"))? {
                    seen += 1;
                }
            }
            Ok(seen)
        }));
    }
    // Every listener must be inside LISTEN before the writer starts, or the
    // arm measures a partly-empty listener table.
    let deadline = Instant::now() + Duration::from_secs(30);
    while ready.load(std::sync::atomic::Ordering::Acquire) < n {
        if Instant::now() > deadline {
            stop.store(true, std::sync::atomic::Ordering::Release);
            return Err(format!("only {} of {n} listeners connected", ready.load(std::sync::atomic::Ordering::Acquire)).into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut c = postgres::Client::connect(&conn, postgres::NoTls)
        .map_err(|e| format!("pg writer: {e}"))?;
    let t0 = Instant::now();
    for _ in 0..D_ROWS {
        let mut txn = c.transaction().map_err(|e| format!("begin: {e}"))?;
        txn.execute("INSERT INTO nscale DEFAULT VALUES", &[]).map_err(|e| format!("insert: {e}"))?;
        txn.batch_execute("NOTIFY nscale_ch, 'x'").map_err(|e| format!("notify: {e}"))?;
        txn.commit().map_err(|e| format!("commit: {e}"))?;
    }
    let rate = D_ROWS as f64 / t0.elapsed().as_secs_f64();
    stop.store(true, std::sync::atomic::Ordering::Release);
    let mut woke = 0usize;
    for h in listeners {
        woke += h.join().map_err(|_| "listener panicked".to_string())??;
    }
    Ok(ArmResult {
        engine: "postgres",
        arm: Box::leak(format!("D {n} listeners").into_boxed_str()),
        writes_per_sec: rate,
        lat_p50_us: 0,
        lat_p99_us: 0,
        notifications: woke,
    })
}

fn mpedb_listener_scaling(dir: &std::path::Path, n: usize) -> BResult<ArmResult> {
    use mpedb::{Config, Database, Value};
    let path = dir.join(format!("notify-scale-{n}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 256
durability = "wal"

[[table]]
name = "nscale"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    let db = std::sync::Arc::new(
        Database::open_with_config(Config::from_toml_str(&toml).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?,
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut listeners = Vec::new();
    for _ in 0..n {
        let db = db.clone();
        let stop = stop.clone();
        listeners.push(std::thread::spawn(move || -> usize {
            let mut seen = db.change_generation(0).map(|(g, _)| g).unwrap_or(0);
            let mut woke = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                if !db.wait_for_change(&[0], &[seen], Duration::from_millis(50)).is_empty() {
                    woke += 1;
                    seen = db.change_generation(0).map(|(g, _)| g).unwrap_or(seen);
                }
            }
            woke
        }));
    }
    // Parked-listener count is observable, so waiting for it beats sleeping.
    let deadline = Instant::now() + Duration::from_secs(30);
    while db.notify_waiter_count() < n as u32 {
        if Instant::now() > deadline {
            stop.store(true, std::sync::atomic::Ordering::Release);
            return Err(format!("only {} of {n} listeners parked", db.notify_waiter_count()).into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let t0 = Instant::now();
    for i in 0..D_ROWS {
        db.query("INSERT INTO nscale (id) VALUES ($1)", &[Value::Int(i as i64)])
            .map_err(|e| e.to_string())?;
    }
    let rate = D_ROWS as f64 / t0.elapsed().as_secs_f64();
    stop.store(true, std::sync::atomic::Ordering::Release);
    let woke: usize = listeners.into_iter().map(|h| h.join().unwrap_or(0)).sum();
    let _ = std::fs::remove_file(&path);
    Ok(ArmResult {
        engine: "mpedb",
        arm: Box::leak(format!("D {n} listeners").into_boxed_str()),
        writes_per_sec: rate,
        lat_p50_us: 0,
        lat_p99_us: 0,
        notifications: woke,
    })
}

pub fn run(scratch: PathBuf) -> BResult<()> {
    std::fs::create_dir_all(&scratch).map_err(|e| format!("scratch: {e}"))?;
    println!(
        "notify cell: {ROWS} rows. A = notify per commit, B = {BATCH} rows per commit, \
         C = {WRITERS} CONCURRENT writers notifying per commit (the arm a global lock shows up in)"
    );
    println!(
        "postgres: fsync=on synchronous_commit=on   mpedb: durability=wal (log-based, \
         like-for-like) plus one durability=commit row (#122)"
    );
    println!("'no listener' rows are the control: the engine publishes, nobody parks.");
    #[cfg(not(target_os = "linux"))]
    println!(
        "NOTE: this is not Linux — mpedb's futex wake is a no-op here and the listener polls \
         at ~200 us, so its latency floor is a platform fact, not a design one."
    );



    // mpedb needs no server, so it goes first and cannot be blamed for PG setup.
    // `wal` is the log-based mode, the like-for-like against PostgreSQL's WAL.
    // `commit` publishes whole pages and is mpedb's SLOWEST durable mode —
    // reported too, labelled, because quoting only the flattering one is the
    // mistake #122 already had to correct in this very benchmark.
    let mut rows: Vec<ArmResult> = vec![
        mpedb_arm(&scratch, "A no listener", 1, "wal", 0)?,
        mpedb_arm(&scratch, "A per-commit", 1, "wal", 1)?,
        mpedb_arm(&scratch, "B no listener", BATCH, "wal", 0)?,
        mpedb_arm(&scratch, "B batched", BATCH, "wal", 1)?,
        mpedb_arm(&scratch, "A per-commit/pg-sync", 1, "commit", 1)?,
        mpedb_concurrent(&scratch, 0)?,
        mpedb_concurrent(&scratch, 1)?,
    ];
    for n in LISTENER_COUNTS {
        rows.push(mpedb_listener_scaling(&scratch, n)?);
    }

    let datadir = scratch.join("pgdata");
    let sockdir = scratch.join("pgsock");
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&sockdir).map_err(|e| format!("sockdir: {e}"))?;
    match PgServer::start_general_conn(datadir, sockdir, "on", "on", 256) {
        Ok(pg) => {
            // The symmetric control: same commits, no NOTIFY. Without it the
            // arm-A number cannot be attributed — it could be the notify lock
            // or it could be plain fsync, and the whole thesis turns on which.
            rows.push(pg_arm(&pg, "A no notify", 1, false)?);
            rows.push(pg_arm(&pg, "A per-commit", 1, true)?);
            rows.push(pg_arm(&pg, "B no notify", BATCH, false)?);
            rows.push(pg_arm(&pg, "B batched", BATCH, true)?);
            rows.push(pg_concurrent(&pg, false)?);
            rows.push(pg_concurrent(&pg, true)?);
            for n in LISTENER_COUNTS {
                rows.push(pg_listener_scaling(&pg, n)?);
            }
        }
        Err(e) => println!("postgres unavailable, mpedb-only run: {e}"),
    }

    println!();
    println!(
        "{:<10} {:<14} {:>12} {:>12} {:>12} {:>8}",
        "engine", "arm", "writes/s", "lat p50 us", "lat p99 us", "notifs"
    );
    for r in &rows {
        println!(
            "{:<10} {:<14} {:>12.0} {:>12} {:>12} {:>8}",
            r.engine, r.arm, r.writes_per_sec, r.lat_p50_us, r.lat_p99_us, r.notifications
        );
    }
    Ok(())
}
