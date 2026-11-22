//! The blob tables.
//!
//! One `.mpedb` file has one writer. A shard is another file, so N shards
//! are N writer locks — the same reason a SQLite blob server opens N
//! database files. Which file a key lands on is its cluster slot modulo N,
//! so every field of a hash stays on the file the hash name hashed to.
//!
//! Strings and hashes are different tables. A key is one or the other: `SET`
//! of a live hash, or `HSET` of a live string, is `WRONGTYPE`. Expiry is a
//! column on the row. A NULL `expires_at` is not in the index (an index
//! entry is skipped when any indexed column is NULL), which is the partial
//! index a reaper wants. Reads hide an expired row immediately. The reaper
//! deletes in bounded chunks, and the delete is conditional on the deadline
//! it observed, so a `SET` that replaced the row in between is left alone.
//!
//! Group commit is the engine's, and it runs for `durability = commit` and
//! `wal`. `async` and `none` acknowledge before that sync and skip the ring.
//! The ring carries up to 1 MiB of parameters per statement
//! (`RING_SPILL_CAP`). A write whose key and stored value exceed that takes
//! the writer lock and syncs alone.
//! One process should serve a data directory. A second one is refused with
//! an exclusive lock in the server, because the type check and the write
//! are two statements and the lock is what makes them one decision.
//!
//! A command that writes one row is one autocommit statement. `HSET` with
//! several fields and `DEL` with several keys run in a write session, one
//! per file. `SET` goes through a per-file write queue, which puts the
//! `SET`s waiting at the same moment in one transaction (`WriteQueue`). A
//! stripe is always taken before a writer lock. `DEL` and the write queue
//! are the paths that hold more than one stripe, and both take them
//! ascending.
//!
//! Reads (`GET`, `HGET`, `EXISTS`, `TTL`) do not take the per-slot stripe.
//! A call that races a type change can answer nil once. It does not return
//! the other type's bytes.

use crate::cache::Cache;
use crate::compress::{compress, decompress};
use crate::crc16::slot;
use mpedb::{toml_escape, Config, Database, ExecResult, Value, WriteSession};
use mpedb_core::btree::MAX_KEY;
use mpedb_types::keycode::encode_key;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockWriteGuard};

const STRIPES: usize = 64;
const META_NAME: &str = "META";

/// How a `SET` treats an existing value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetCond {
    /// Always write.
    Always,
    /// Write only when the key is absent or expired. Redis `NX`.
    IfAbsent,
    /// Write only when the key is a live string. Redis `XX`.
    IfPresent,
}

/// TTL to store with a new string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetExpiry {
    /// No expiry.
    None,
    /// Absolute deadline in milliseconds since the Unix epoch.
    At(i64),
}

/// One `SET`. See [`BlobStore::set_many`].
#[derive(Debug, Clone, Copy)]
pub struct SetItem<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub expiry: SetExpiry,
    pub cond: SetCond,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetWritten {
    Written,
    /// `NX`/`XX` condition failed. Not an error.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// No live key.
    Missing,
    /// Live key, no expiry.
    Persistent,
    /// Milliseconds still to live. Always greater than zero.
    Remaining(i64),
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub dir: PathBuf,
    pub shards: usize,
    pub durability: String,
    pub size_mb: u64,
    pub zstd_level: i32,
    pub max_value_bytes: usize,
    /// Memory for strings held in front of the files. Zero turns it off.
    pub cache_bytes: usize,
    /// Answer a plain `SET` from memory and write it to the file later. See
    /// [`Behind`]. A killed process loses what was not yet written.
    pub write_behind: bool,
}

pub struct BlobStore {
    dir: PathBuf,
    shards: Vec<Database>,
    stripes: [Mutex<()>; STRIPES],
    queues: Vec<WriteQueue>,
    /// False while no file holds a hash row, live or expired. It is set at
    /// open from the tables and by the first `HSET`, and never cleared. One
    /// process serves a directory, so nothing else can add a hash.
    hashes: AtomicBool,
    cache: Cache,
    behind: Option<Behind>,
    zstd_level: i32,
    max_value_bytes: usize,
}

/// `SET`s that were answered and are not in a file yet.
///
/// With `write_behind` a `SET` with no `NX` or `XX`, in a store that holds
/// no hash, is kept here and answered. [`BlobStore::drain`] writes what is
/// held, and a key set several times in between is written once. What is
/// held is lost when the process is killed, which no durability mode of the
/// engine allows: `none` keeps every answered write through a killed
/// process. So this is off unless the config asks for it.
///
/// A `GET` looks here first. Every other command reads the files, so it
/// first writes everything held and keeps new `SET`s out while it runs:
/// it takes `gate` for writing and a held `SET` takes it for reading.
struct Behind {
    gate: RwLock<()>,
    /// One drain at a time.
    draining: Mutex<()>,
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    /// Not handed to a drain yet.
    fresh: HashMap<Box<[u8]>, HeldSet>,
    /// Being written. Still answers `GET` until the cache or the file does.
    /// Rows a drain failed to write stay here and are written first by the
    /// next one.
    going: HashMap<Box<[u8]>, HeldSet>,
    fresh_bytes: usize,
    /// The last drain failed. `SET` waits for the file again until one
    /// succeeds, so the client hears the error.
    stuck: bool,
}

#[derive(Clone)]
struct HeldSet {
    value: Arc<[u8]>,
    expiry: SetExpiry,
    now: i64,
}

/// A `SET` that finds more than this held writes it out before it returns.
const HELD_BYTES: usize = 64 * 1024 * 1024;
/// Rows a drain hands the write queue at a time.
const DRAIN_ROWS: usize = 512;

fn expires_of(expiry: SetExpiry) -> Option<i64> {
    match expiry {
        SetExpiry::None => None,
        SetExpiry::At(ms) => Some(ms),
    }
}

/// The `SET`s waiting for one file.
///
/// A file has one writer, so `SET`s from eight connections were eight
/// commits one after the other, and sixteen pipelined on one connection were
/// sixteen. Here a caller queues its rows, and one caller at a time leads:
/// it drains the queue into one transaction, so the cost of a commit, and
/// under `commit` and `wal` the sync, is shared by everything that arrived
/// while the previous one ran. With one caller the queue holds one row and
/// it is the autocommit statement it always was.
struct WriteQueue {
    inner: Mutex<QueueInner>,
}

struct QueueInner {
    jobs: VecDeque<Arc<Job>>,
    /// Some caller is leading or has been handed the lead. Changed only
    /// under the queue's mutex, with the push that reads it, so a job can
    /// never be queued behind a leader that has already left.
    leading: bool,
}

/// One caller's rows, and where their answers are left.
struct Job {
    rows: Vec<Row>,
    state: Mutex<JobState>,
    wake: Condvar,
}

enum JobState {
    Waiting,
    /// The previous leader handed this job's owner the lead.
    Lead,
    Done(Vec<Result<SetWritten, StoreError>>),
}

/// Gives the lead to the owner of the oldest queued job, or marks the queue
/// leaderless when nothing is queued.
struct HandOff<'a>(&'a WriteQueue);

impl Drop for HandOff<'_> {
    fn drop(&mut self) {
        let mut inner = lock(&self.0.inner);
        match inner.jobs.front() {
            Some(next) => {
                *lock(&next.state) = JobState::Lead;
                next.wake.notify_one();
            }
            None => inner.leading = false,
        }
    }
}

/// Answers every job of a batch that the write left unanswered. That only
/// happens when the write panicked, and without this their owners would
/// sleep forever.
struct Answer<'a>(&'a [Arc<Job>]);

impl Drop for Answer<'_> {
    fn drop(&mut self) {
        for job in self.0 {
            let mut state = lock(&job.state);
            if !matches!(*state, JobState::Done(_)) {
                *state = JobState::Done(
                    job.rows
                        .iter()
                        .map(|_| Err(StoreError::Engine("the write batch was lost".into())))
                        .collect(),
                );
                job.wake.notify_one();
            }
        }
    }
}

/// A `SET` that has been checked and compressed.
struct Row {
    key: Vec<u8>,
    codec: i64,
    stored: Vec<u8>,
    /// The value as the client sent it, for the cache.
    plain: Option<Arc<[u8]>>,
    expiry: SetExpiry,
    cond: SetCond,
    now: i64,
}

/// One transaction holds at most this many rows, and takes no further job
/// once the stored bytes pass this. They bound how long it holds the writer
/// lock.
const BATCH_ROWS: usize = 256;
const BATCH_BYTES: usize = 8 * 1024 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl BlobStore {
    pub fn open(opts: &StoreOptions) -> Result<Self, StoreError> {
        validate_options(opts)?;
        fs::create_dir_all(&opts.dir)
            .map_err(|e| StoreError::Engine(format!("create {}: {e}", opts.dir.display())))?;
        adopt_meta(&opts.dir, opts.shards, opts.size_mb)?;
        let mut shards = Vec::with_capacity(opts.shards);
        for i in 0..opts.shards {
            let path = shard_path(&opts.dir, i);
            let cfg = shard_config(&path, opts)?;
            let db = Database::open_with_config(cfg)
                .map_err(|e| StoreError::Engine(format!("open {}: {e}", path.display())))?;
            shards.push(db);
        }
        let mut hashes = false;
        for db in &shards {
            let found = rows(
                db.query("SELECT hkey FROM hash_blobs LIMIT 1", &[])
                    .map_err(engine)?,
            )?;
            hashes |= !found.is_empty();
        }
        Ok(BlobStore {
            dir: opts.dir.clone(),
            hashes: AtomicBool::new(hashes),
            cache: Cache::new(opts.cache_bytes, STRIPES),
            behind: opts.write_behind.then(|| Behind {
                gate: RwLock::new(()),
                draining: Mutex::new(()),
                held: Mutex::new(Held::default()),
            }),
            shards,
            stripes: std::array::from_fn(|_| Mutex::new(())),
            queues: (0..opts.shards)
                .map(|_| WriteQueue {
                    inner: Mutex::new(QueueInner {
                        jobs: VecDeque::new(),
                        leading: false,
                    }),
                })
                .collect(),
            zstd_level: opts.zstd_level,
            max_value_bytes: opts.max_value_bytes,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn shards(&self) -> usize {
        self.shards.len()
    }

    pub fn get(&self, key: &[u8], now: i64) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(behind) = &self.behind {
            let held = lock(&behind.held);
            if let Some(set) = held.fresh.get(key).or_else(|| held.going.get(key)) {
                return Ok(is_live(expires_of(set.expiry), now).then(|| set.value.to_vec()));
            }
        }
        let part = slot(key) as usize % STRIPES;
        let mut seen = None;
        if self.cache.enabled() {
            match self.cache.get(part, key) {
                Ok((value, expires)) if is_live(expires, now) => return Ok(Some(value.to_vec())),
                // Expired here. The file may hold a hash by now.
                Ok(_) => {}
                Err(token) => seen = Some(token),
            }
        }
        check_parts(&[Value::Blob(key.to_vec())])?;
        let db: &mut dyn Q = &mut self.db_for(key);
        if let Some(row) = self.live_string(db, key, now)? {
            let value = self.decode_row(&row)?;
            if let Some(seen) = seen {
                self.cache.fill(part, seen, key, &value, row.expires);
            }
            return Ok(Some(value));
        }
        if self.hashes.load(Ordering::SeqCst) && self.live_hash(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        Ok(None)
    }

    pub fn set(
        &self,
        key: &[u8],
        value: &[u8],
        expiry: SetExpiry,
        cond: SetCond,
        now: i64,
    ) -> Result<SetWritten, StoreError> {
        let item = SetItem {
            key,
            value,
            expiry,
            cond,
        };
        self.set_many(&[item], now)
            .pop()
            .unwrap_or_else(|| Err(StoreError::Engine("set lost its result".into())))
    }

    /// Several `SET`s, in order. Each goes to its file's write queue, and a
    /// queue is drained into one transaction (see [`WriteQueue`]), so the
    /// `SET`s of one pipeline and the `SET`s of other connections waiting at
    /// the same moment commit together.
    ///
    /// The commands stay independent. A `WRONGTYPE` or a failed `NX` is that
    /// command's answer and the others are written.
    pub fn set_many(&self, items: &[SetItem<'_>], now: i64) -> Vec<Result<SetWritten, StoreError>> {
        let Some(behind) = &self.behind else {
            return self.set_through(items, now);
        };
        if items.iter().all(|item| item.cond == SetCond::Always) {
            let _gate = behind.gate.read().unwrap_or_else(|p| p.into_inner());
            // A hash would need the type check, and that reads the file.
            if !self.hashes.load(Ordering::SeqCst) {
                if let Some(out) = self.set_behind(behind, items, now) {
                    return out;
                }
            }
        }
        match self.settled() {
            Ok(_gate) => self.set_through(items, now),
            Err(e) => items.iter().map(|_| Err(e.clone())).collect(),
        }
    }

    /// Hold `items` in memory and answer them. `None` when the last drain
    /// failed, and the caller then waits for the file.
    fn set_behind(
        &self,
        behind: &Behind,
        items: &[SetItem<'_>],
        now: i64,
    ) -> Option<Vec<Result<SetWritten, StoreError>>> {
        let mut out = Vec::with_capacity(items.len());
        let full = {
            let mut held = lock(&behind.held);
            if held.stuck {
                return None;
            }
            for item in items {
                let checked = self
                    .check_value(item.value)
                    .and_then(|()| check_parts(&[Value::Blob(item.key.to_vec())]));
                if let Err(e) = checked {
                    out.push(Err(e));
                    continue;
                }
                let set = HeldSet {
                    value: Arc::from(item.value),
                    expiry: item.expiry,
                    now,
                };
                held.fresh_bytes += item.key.len() + item.value.len();
                if let Some(old) = held.fresh.insert(item.key.into(), set) {
                    held.fresh_bytes -= item.key.len() + old.value.len();
                }
                out.push(Ok(SetWritten::Written));
            }
            held.fresh_bytes > HELD_BYTES
        };
        if full {
            if let Err(e) = self.drain() {
                return Some(items.iter().map(|_| Err(e.clone())).collect());
            }
        }
        Some(out)
    }

    /// Write what `write_behind` holds. Returns how many rows were written.
    ///
    /// A row that fails stays held, still answers `GET`, and is written
    /// first by the next call.
    pub fn drain(&self) -> Result<u64, StoreError> {
        let Some(behind) = &self.behind else {
            return Ok(0);
        };
        let _one = lock(&behind.draining);
        let mut written = 0u64;
        // Twice at most: what an earlier drain left, then what is fresh.
        for _ in 0..2 {
            let sets: Vec<(Box<[u8]>, HeldSet)> = {
                let mut held = lock(&behind.held);
                if held.going.is_empty() {
                    if held.fresh.is_empty() {
                        held.stuck = false;
                        return Ok(written);
                    }
                    held.going = std::mem::take(&mut held.fresh);
                    held.fresh_bytes = 0;
                }
                held.going
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            let mut failed = None;
            let mut done: Vec<&[u8]> = Vec::with_capacity(sets.len());
            for chunk in sets.chunks(DRAIN_ROWS) {
                let mut by_shard: Vec<(Vec<&[u8]>, Vec<Row>)> = self
                    .shards
                    .iter()
                    .map(|_| (Vec::new(), Vec::new()))
                    .collect();
                for (key, set) in chunk {
                    match compress(&set.value, self.zstd_level) {
                        Ok((codec, stored)) => {
                            let (keys, rows) =
                                &mut by_shard[shard_index(slot(key), self.shards.len())];
                            keys.push(key);
                            rows.push(Row {
                                key: key.to_vec(),
                                codec,
                                stored,
                                plain: self.cache.enabled().then(|| Arc::clone(&set.value)),
                                expiry: set.expiry,
                                cond: SetCond::Always,
                                now: set.now,
                            });
                        }
                        Err(e) => failed = Some(StoreError::Engine(format!("zstd: {e}"))),
                    }
                }
                for (shard, (keys, rows)) in by_shard.into_iter().enumerate() {
                    if rows.is_empty() {
                        continue;
                    }
                    for (key, result) in keys.into_iter().zip(self.enqueue(shard, rows)) {
                        match result {
                            Ok(_) => done.push(key),
                            Err(e) => failed = Some(e),
                        }
                    }
                }
                if failed.is_some() {
                    break;
                }
            }
            let mut held = lock(&behind.held);
            for key in &done {
                held.going.remove(*key);
            }
            written += done.len() as u64;
            if let Some(e) = failed {
                held.stuck = true;
                return Err(e);
            }
        }
        Ok(written)
    }

    /// For a command that reads the files: everything held is written, and
    /// no `SET` is held while the returned guard lives.
    fn settled(&self) -> Result<Option<RwLockWriteGuard<'_, ()>>, StoreError> {
        let Some(behind) = &self.behind else {
            return Ok(None);
        };
        let gate = behind.gate.write().unwrap_or_else(|p| p.into_inner());
        self.drain()?;
        Ok(Some(gate))
    }

    fn set_through(&self, items: &[SetItem<'_>], now: i64) -> Vec<Result<SetWritten, StoreError>> {
        let mut out: Vec<Option<Result<SetWritten, StoreError>>> =
            items.iter().map(|_| None).collect();
        // Checked and compressed here, on the caller's thread, so the thread
        // that drains the queue only writes.
        let mut by_shard: Vec<(Vec<usize>, Vec<Row>)> = self
            .shards
            .iter()
            .map(|_| (Vec::new(), Vec::new()))
            .collect();
        for (i, item) in items.iter().enumerate() {
            let checked = self
                .check_value(item.value)
                .and_then(|()| check_parts(&[Value::Blob(item.key.to_vec())]))
                .and_then(|()| {
                    compress(item.value, self.zstd_level)
                        .map_err(|e| StoreError::Engine(format!("zstd: {e}")))
                });
            match checked {
                Ok((codec, stored)) => {
                    let (indexes, rows) =
                        &mut by_shard[shard_index(slot(item.key), self.shards.len())];
                    indexes.push(i);
                    rows.push(Row {
                        key: item.key.to_vec(),
                        codec,
                        stored,
                        plain: self.cache.enabled().then(|| Arc::from(item.value)),
                        expiry: item.expiry,
                        cond: item.cond,
                        now,
                    });
                }
                Err(e) => out[i] = Some(Err(e)),
            }
        }
        for (shard, (indexes, rows)) in by_shard.into_iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            for (i, result) in indexes.into_iter().zip(self.enqueue(shard, rows)) {
                out[i] = Some(result);
            }
        }
        out.into_iter()
            .map(|r| r.unwrap_or_else(|| Err(StoreError::Engine("set lost its result".into()))))
            .collect()
    }

    /// Queue `rows` for one file and return when they are written.
    ///
    /// The first caller to find the queue without a leader leads: it takes
    /// what is queued, writes it, and leaves each job its answers. Everyone
    /// else sleeps on its own job. When the leader's own rows are written it
    /// hands the lead to the owner of the oldest job still queued, so no
    /// caller serves the others while its own client waits.
    fn enqueue(&self, shard: usize, rows: Vec<Row>) -> Vec<Result<SetWritten, StoreError>> {
        let queue = &self.queues[shard];
        let job = Arc::new(Job {
            rows,
            state: Mutex::new(JobState::Waiting),
            wake: Condvar::new(),
        });
        let lead_now = {
            let mut inner = lock(&queue.inner);
            inner.jobs.push_back(Arc::clone(&job));
            !std::mem::replace(&mut inner.leading, true)
        };
        if !lead_now {
            let mut state = lock(&job.state);
            while matches!(*state, JobState::Waiting) {
                state = job.wake.wait(state).unwrap_or_else(|p| p.into_inner());
            }
            if let JobState::Done(results) = std::mem::replace(&mut *state, JobState::Lead) {
                return results;
            }
        }
        // Leading. The hand-off runs on every way out of this scope, a panic
        // included, or the queue would wait for a leader that is gone.
        let _hand_off = HandOff(queue);
        loop {
            let mut batch = Vec::new();
            let (mut count, mut bytes) = (0usize, 0usize);
            {
                let mut inner = lock(&queue.inner);
                while count < BATCH_ROWS && bytes < BATCH_BYTES {
                    let Some(next) = inner.jobs.pop_front() else {
                        break;
                    };
                    count += next.rows.len();
                    bytes += next.rows.iter().map(|r| r.stored.len()).sum::<usize>();
                    batch.push(next);
                }
            }
            let answer_all = Answer(&batch);
            self.write_batch(&self.shards[shard], &batch);
            drop(answer_all);
            let mut state = lock(&job.state);
            if let JobState::Done(results) = std::mem::replace(&mut *state, JobState::Lead) {
                return results;
            }
        }
    }

    /// One transaction for every row of `batch`. If the engine refuses the
    /// transaction, each row is written on its own, so each gets the answer
    /// it would have had alone.
    fn write_batch(&self, shard: &Database, batch: &[Arc<Job>]) {
        // Every stripe first, ascending, as `DEL` does, then the writer lock.
        let mut wanted: Vec<usize> = batch
            .iter()
            .flat_map(|job| job.rows.iter())
            .map(|row| slot(&row.key) as usize % STRIPES)
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let _guards: Vec<MutexGuard<'_, ()>> =
            wanted.into_iter().map(|i| lock(&self.stripes[i])).collect();
        let rows = batch.iter().map(|job| job.rows.len()).sum::<usize>();
        let together = if rows > 1 {
            self.write_in_txn(shard, batch).ok()
        } else {
            None
        };
        let mut results = together.unwrap_or_else(|| {
            let db: &mut dyn Q = &mut &*shard;
            batch
                .iter()
                .flat_map(|job| job.rows.iter())
                .map(|row| self.set_row(db, row))
                .collect()
        });
        // Still under the stripes, so the cache sees these rows in the order
        // the file did.
        if self.cache.enabled() {
            for (row, result) in batch.iter().flat_map(|job| job.rows.iter()).zip(&results) {
                let part = slot(&row.key) as usize % STRIPES;
                match (result, &row.plain) {
                    (Ok(SetWritten::Skipped), _) => {}
                    (Ok(SetWritten::Written), Some(plain)) => {
                        let expires = expires_of(row.expiry);
                        self.cache.put(part, &row.key, Arc::clone(plain), expires);
                    }
                    _ => self.cache.forget(part, &row.key),
                }
            }
        }
        for job in batch.iter().rev() {
            let mine = results.split_off(results.len() - job.rows.len());
            *lock(&job.state) = JobState::Done(mine);
            job.wake.notify_one();
        }
    }

    /// `Err` means nothing was written.
    fn write_in_txn(
        &self,
        shard: &Database,
        batch: &[Arc<Job>],
    ) -> Result<Vec<Result<SetWritten, StoreError>>, StoreError> {
        let mut txn = shard.begin().map_err(engine)?;
        let mut results = Vec::new();
        for row in batch.iter().flat_map(|job| job.rows.iter()) {
            match self.set_row(&mut txn, row) {
                Ok(written) => results.push(Ok(written)),
                Err(StoreError::WrongType) => results.push(Err(StoreError::WrongType)),
                Err(e) => {
                    txn.rollback();
                    return Err(e);
                }
            }
        }
        txn.commit().map_err(engine)?;
        Ok(results)
    }

    fn set_row(&self, db: &mut dyn Q, row: &Row) -> Result<SetWritten, StoreError> {
        let (key, now, cond) = (row.key.as_slice(), row.now, row.cond);
        // The type check is a probe of the hash table, and a store that
        // has never held a hash has nothing to probe.
        if self.hashes.load(Ordering::SeqCst) && self.live_hash(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        // Only `NX` and `XX` ask whether the string is there.
        if cond != SetCond::Always {
            let live = self.string_live(db, key, now)?;
            match cond {
                SetCond::IfAbsent if live => return Ok(SetWritten::Skipped),
                SetCond::IfPresent if !live => return Ok(SetWritten::Skipped),
                _ => {}
            }
        }
        let expires = match row.expiry {
            SetExpiry::None => Value::Null,
            SetExpiry::At(ms) => Value::Int(ms),
        };
        affected(
            db.q(
                "INSERT INTO str_blobs (key, data, codec, created_at, updated_at, expires_at, version) \
                 VALUES (?, ?, ?, ?, ?, ?, 1) \
                 ON CONFLICT (key) DO UPDATE SET \
                   data = excluded.data, \
                   codec = excluded.codec, \
                   updated_at = excluded.updated_at, \
                   expires_at = excluded.expires_at, \
                   version = version + 1",
                &[
                    Value::Blob(key.to_vec()),
                    Value::Blob(row.stored.clone()),
                    Value::Int(row.codec),
                    Value::Int(now),
                    Value::Int(now),
                    expires,
                ],
            )
            .map_err(engine)?,
        )?;
        Ok(SetWritten::Written)
    }

    pub fn del(&self, keys: &[&[u8]], now: i64) -> Result<u64, StoreError> {
        let _settled = self.settled()?;
        // Every key is checked before any is deleted. A bad key in second
        // place used to cost the first one.
        for key in keys {
            check_parts(&[Value::Blob(key.to_vec())])?;
        }
        if let [key] = keys {
            let _guard = self.stripe(key);
            let _forget = self.forgetting(&[key]);
            let db: &mut dyn Q = &mut self.db_for(key);
            return self.del_one(db, key, now);
        }
        // Several keys take every stripe they need, in ascending order, and
        // only then a writer lock. This is the one path that holds more than
        // one stripe, so ascending order cannot meet itself coming back.
        let mut wanted: Vec<usize> = keys.iter().map(|k| slot(k) as usize % STRIPES).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let _guards: Vec<MutexGuard<'_, ()>> = wanted
            .into_iter()
            .map(|i| self.stripes[i].lock().unwrap_or_else(|p| p.into_inner()))
            .collect();
        let _forget = self.forgetting(keys);
        // One transaction per file. Keys in different files are different
        // databases, and nothing here makes those commit together.
        let mut removed = 0u64;
        for (index, shard) in self.shards.iter().enumerate() {
            let mut mine = keys
                .iter()
                .filter(|k| shard_index(slot(k), self.shards.len()) == index)
                .peekable();
            if mine.peek().is_none() {
                continue;
            }
            let mut txn = shard.begin().map_err(engine)?;
            let mut n = 0u64;
            for key in mine {
                match self.del_one(&mut txn, key, now) {
                    Ok(one) => n += one,
                    Err(e) => {
                        txn.rollback();
                        return Err(e);
                    }
                }
            }
            txn.commit().map_err(engine)?;
            removed += n;
        }
        Ok(removed)
    }

    pub fn exists(&self, keys: &[&[u8]], now: i64) -> Result<u64, StoreError> {
        let _settled = self.settled()?;
        let mut n = 0u64;
        for key in keys {
            check_parts(&[Value::Blob(key.to_vec())])?;
            let db: &mut dyn Q = &mut self.db_for(key);
            if self.string_live(db, key, now)? || self.live_hash(db, key, now)? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Set a deadline. `Ok(true)` when a live key gained one.
    pub fn expire_at(&self, key: &[u8], deadline: i64, now: i64) -> Result<bool, StoreError> {
        let _settled = self.settled()?;
        check_parts(&[Value::Blob(key.to_vec())])?;
        let _guard = self.stripe(key);
        let _forget = self.forgetting(&[key]);
        let db: &mut dyn Q = &mut self.db_for(key);
        let n = affected(
            db.q(
                "UPDATE str_blobs SET expires_at = ? \
                 WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)",
                &[
                    Value::Int(deadline),
                    Value::Blob(key.to_vec()),
                    Value::Int(now),
                ],
            )
            .map_err(engine)?,
        )?;
        if n > 0 {
            return Ok(true);
        }
        if !self.live_hash(db, key, now)? {
            return Ok(false);
        }
        let n = affected(
            db.q(
                "UPDATE hash_blobs SET expires_at = ? \
                 WHERE hkey = ? AND (expires_at IS NULL OR expires_at > ?)",
                &[
                    Value::Int(deadline),
                    Value::Blob(key.to_vec()),
                    Value::Int(now),
                ],
            )
            .map_err(engine)?,
        )?;
        Ok(n > 0)
    }

    /// Clear a deadline. `Ok(true)` when a live key had one.
    pub fn persist(&self, key: &[u8], now: i64) -> Result<bool, StoreError> {
        let _settled = self.settled()?;
        check_parts(&[Value::Blob(key.to_vec())])?;
        let _guard = self.stripe(key);
        let _forget = self.forgetting(&[key]);
        let db: &mut dyn Q = &mut self.db_for(key);
        let n = affected(
            db.q(
                "UPDATE str_blobs SET expires_at = NULL \
                 WHERE key = ? AND expires_at IS NOT NULL AND expires_at > ?",
                &[Value::Blob(key.to_vec()), Value::Int(now)],
            )
            .map_err(engine)?,
        )?;
        if n > 0 {
            return Ok(true);
        }
        let n = affected(
            db.q(
                "UPDATE hash_blobs SET expires_at = NULL \
                 WHERE hkey = ? AND expires_at IS NOT NULL AND expires_at > ?",
                &[Value::Blob(key.to_vec()), Value::Int(now)],
            )
            .map_err(engine)?,
        )?;
        Ok(n > 0)
    }

    pub fn ttl(&self, key: &[u8], now: i64) -> Result<Ttl, StoreError> {
        let _settled = self.settled()?;
        check_parts(&[Value::Blob(key.to_vec())])?;
        let db: &mut dyn Q = &mut self.db_for(key);
        if let Some(expires) = self.string_expiry(db, key)? {
            if is_live(expires, now) {
                return Ok(ttl_of(expires, now));
            }
        }
        if let Some(expires) = self.hash_expiry(db, key, now)? {
            return Ok(ttl_of(expires, now));
        }
        Ok(Ttl::Missing)
    }

    pub fn hget(&self, key: &[u8], field: &[u8], now: i64) -> Result<Option<Vec<u8>>, StoreError> {
        let _settled = self.settled()?;
        check_parts(&[Value::Blob(key.to_vec()), Value::Blob(field.to_vec())])?;
        let db: &mut dyn Q = &mut self.db_for(key);
        if self.string_live(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        let Some(row) = self.field_row(db, key, field)? else {
            return Ok(None);
        };
        if !is_live(row.expires, now) {
            return Ok(None);
        }
        Ok(Some(self.decode_row(&row)?))
    }

    /// Returns how many fields were newly created. An expired field counts
    /// as new: the hash it belonged to is already gone.
    pub fn hset(&self, key: &[u8], fields: &[(&[u8], &[u8])], now: i64) -> Result<u64, StoreError> {
        let _settled = self.settled()?;
        for (field, value) in fields {
            self.check_value(value)?;
            check_parts(&[Value::Blob(key.to_vec()), Value::Blob(field.to_vec())])?;
        }
        let stored: Vec<(i64, Vec<u8>)> = fields
            .iter()
            .map(|(_, value)| {
                compress(value, self.zstd_level)
                    .map_err(|e| StoreError::Engine(format!("zstd: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let _guard = self.stripe(key);
        // Before the write, and under the stripe a `SET` of this key holds
        // while it reads the flag.
        self.hashes.store(true, Ordering::SeqCst);
        if fields.len() == 1 {
            let db: &mut dyn Q = &mut self.db_for(key);
            return self.hset_all(db, key, fields, &stored, now);
        }
        // Several fields are one transaction. The stripe is taken first and
        // the writer lock second, the order every other path uses.
        let mut txn = self.db_for(key).begin().map_err(engine)?;
        match self.hset_all(&mut txn, key, fields, &stored, now) {
            Ok(added) => {
                txn.commit().map_err(engine)?;
                Ok(added)
            }
            Err(e) => {
                txn.rollback();
                Err(e)
            }
        }
    }

    fn hset_all(
        &self,
        db: &mut dyn Q,
        key: &[u8],
        fields: &[(&[u8], &[u8])],
        stored: &[(i64, Vec<u8>)],
        now: i64,
    ) -> Result<u64, StoreError> {
        if self.string_live(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        let mut added = 0u64;
        for ((field, _), (codec, data)) in fields.iter().zip(stored) {
            if self.hset_one(db, key, field, *codec, data, now)? {
                added += 1;
            }
        }
        Ok(added)
    }

    pub fn hdel(&self, key: &[u8], fields: &[&[u8]], now: i64) -> Result<u64, StoreError> {
        let _settled = self.settled()?;
        check_parts(&[Value::Blob(key.to_vec())])?;
        for field in fields {
            check_parts(&[Value::Blob(key.to_vec()), Value::Blob(field.to_vec())])?;
        }
        let _guard = self.stripe(key);
        let db: &mut dyn Q = &mut self.db_for(key);
        if self.string_live(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        let mut removed = 0u64;
        for field in fields {
            let n = affected(
                db.q(
                    "DELETE FROM hash_blobs \
                     WHERE hkey = ? AND field = ? AND (expires_at IS NULL OR expires_at > ?)",
                    &[
                        Value::Blob(key.to_vec()),
                        Value::Blob(field.to_vec()),
                        Value::Int(now),
                    ],
                )
                .map_err(engine)?,
            )?;
            removed += n;
        }
        Ok(removed)
    }

    /// Delete up to `chunk` expired string rows and `chunk` expired hash
    /// fields. Safe to call while clients write: a row whose expiry moved
    /// into the future after it was selected is not deleted.
    pub fn reap(&self, now: i64, chunk: usize) -> Result<u64, StoreError> {
        if chunk == 0 {
            return Ok(0);
        }
        let mut removed = 0u64;
        for db in &self.shards {
            let strings = rows(
                db.query(
                    &format!("SELECT key FROM str_blobs WHERE expires_at <= ? LIMIT {chunk}"),
                    &[Value::Int(now)],
                )
                .map_err(engine)?,
            )?;
            for row in strings {
                let key = blob_col(&row, 0)?;
                let slot = slot(&key);
                let _guard = self.stripe_slot(slot);
                let _forget = self.forgetting(&[&key]);
                let n = affected(
                    db.query(
                        "DELETE FROM str_blobs \
                         WHERE key = ? AND expires_at IS NOT NULL AND expires_at <= ?",
                        &[Value::Blob(key.clone()), Value::Int(now)],
                    )
                    .map_err(engine)?,
                )?;
                removed += n;
            }
            let fields = rows(
                db.query(
                    &format!(
                        "SELECT hkey, field FROM hash_blobs WHERE expires_at <= ? LIMIT {chunk}"
                    ),
                    &[Value::Int(now)],
                )
                .map_err(engine)?,
            )?;
            for row in fields {
                let hkey = blob_col(&row, 0)?;
                let field = blob_col(&row, 1)?;
                let slot = slot(&hkey);
                let _guard = self.stripe_slot(slot);
                let n = affected(
                    db.query(
                        "DELETE FROM hash_blobs \
                         WHERE hkey = ? AND field = ? \
                           AND expires_at IS NOT NULL AND expires_at <= ?",
                        &[Value::Blob(hkey), Value::Blob(field), Value::Int(now)],
                    )
                    .map_err(engine)?,
                )?;
                removed += n;
            }
        }
        Ok(removed)
    }

    fn hset_one(
        &self,
        db: &mut dyn Q,
        key: &[u8],
        field: &[u8],
        codec: i64,
        data: &[u8],
        now: i64,
    ) -> Result<bool, StoreError> {
        let current = self.field_row(db, key, field)?;
        if let Some(row) = &current {
            if is_live(row.expires, now) {
                affected(
                    db.q(
                        "UPDATE hash_blobs SET data = ?, codec = ?, updated_at = ?, \
                         version = version + 1 \
                         WHERE hkey = ? AND field = ? \
                           AND (expires_at IS NULL OR expires_at > ?)",
                        &[
                            Value::Blob(data.to_vec()),
                            Value::Int(codec),
                            Value::Int(now),
                            Value::Blob(key.to_vec()),
                            Value::Blob(field.to_vec()),
                            Value::Int(now),
                        ],
                    )
                    .map_err(engine)?,
                )?;
                // A live field keeps the hash's existing deadline. HSET does
                // not refresh TTL.
                return Ok(false);
            }
        }
        // Missing, or still physically here but already expired. Inherit the
        // deadline of a live sibling, which is NULL when this field is the
        // first live one. The expired row's own deadline must not survive:
        // the hash it belonged to is gone.
        let inherited = self.hash_expiry(db, key, now)?.flatten();
        let expires = match inherited {
            Some(ms) => Value::Int(ms),
            None => Value::Null,
        };
        if current.is_some() {
            let n = affected(
                db.q(
                    "UPDATE hash_blobs SET data = ?, codec = ?, created_at = ?, \
                     updated_at = ?, expires_at = ?, version = 1 \
                     WHERE hkey = ? AND field = ? \
                       AND expires_at IS NOT NULL AND expires_at <= ?",
                    &[
                        Value::Blob(data.to_vec()),
                        Value::Int(codec),
                        Value::Int(now),
                        Value::Int(now),
                        expires.clone(),
                        Value::Blob(key.to_vec()),
                        Value::Blob(field.to_vec()),
                        Value::Int(now),
                    ],
                )
                .map_err(engine)?,
            )?;
            if n == 1 {
                return Ok(true);
            }
        }
        affected(
            db.q(
                "INSERT INTO hash_blobs \
                   (hkey, field, data, codec, created_at, updated_at, expires_at, version) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
                &[
                    Value::Blob(key.to_vec()),
                    Value::Blob(field.to_vec()),
                    Value::Blob(data.to_vec()),
                    Value::Int(codec),
                    Value::Int(now),
                    Value::Int(now),
                    expires,
                ],
            )
            .map_err(engine)?,
        )?;
        Ok(true)
    }

    fn del_one(&self, db: &mut dyn Q, key: &[u8], now: i64) -> Result<u64, StoreError> {
        let n = affected(
            db.q(
                "DELETE FROM str_blobs \
                 WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)",
                &[Value::Blob(key.to_vec()), Value::Int(now)],
            )
            .map_err(engine)?,
        )?;
        if n > 0 {
            return Ok(1);
        }
        if !self.live_hash(db, key, now)? {
            return Ok(0);
        }
        // The key existed, so every physical field goes, expired leftovers
        // included. The count is still one key.
        affected(
            db.q(
                "DELETE FROM hash_blobs WHERE hkey = ?",
                &[Value::Blob(key.to_vec())],
            )
            .map_err(engine)?,
        )?;
        Ok(1)
    }

    fn live_string(
        &self,
        db: &mut dyn Q,
        key: &[u8],
        now: i64,
    ) -> Result<Option<Stored>, StoreError> {
        let Some(row) = self.string_row(db, key)? else {
            return Ok(None);
        };
        if is_live(row.expires, now) {
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    /// Whether the key is a live string. Reads the deadline and not the
    /// value: the callers only ask which type the key is.
    fn string_live(&self, db: &mut dyn Q, key: &[u8], now: i64) -> Result<bool, StoreError> {
        Ok(self
            .string_expiry(db, key)?
            .is_some_and(|expires| is_live(expires, now)))
    }

    /// `Some(expires)` when the row exists, expired or not.
    fn string_expiry(&self, db: &mut dyn Q, key: &[u8]) -> Result<Option<Option<i64>>, StoreError> {
        let found = rows(
            db.q(
                "SELECT expires_at FROM str_blobs WHERE key = ?",
                &[Value::Blob(key.to_vec())],
            )
            .map_err(engine)?,
        )?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(opt_int(&found[0], 0)?)),
            n => Err(StoreError::Engine(format!(
                "string key lookup returned {n} rows"
            ))),
        }
    }

    fn string_row(&self, db: &mut dyn Q, key: &[u8]) -> Result<Option<Stored>, StoreError> {
        let found = rows(
            db.q(
                "SELECT data, codec, expires_at FROM str_blobs WHERE key = ?",
                &[Value::Blob(key.to_vec())],
            )
            .map_err(engine)?,
        )?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(stored_from(&found[0])?)),
            n => Err(StoreError::Engine(format!(
                "string key lookup returned {n} rows"
            ))),
        }
    }

    fn field_row(
        &self,
        db: &mut dyn Q,
        key: &[u8],
        field: &[u8],
    ) -> Result<Option<Stored>, StoreError> {
        let found = rows(
            db.q(
                "SELECT data, codec, expires_at FROM hash_blobs WHERE hkey = ? AND field = ?",
                &[Value::Blob(key.to_vec()), Value::Blob(field.to_vec())],
            )
            .map_err(engine)?,
        )?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(stored_from(&found[0])?)),
            n => Err(StoreError::Engine(format!(
                "hash field lookup returned {n} rows"
            ))),
        }
    }

    fn live_hash(&self, db: &mut dyn Q, key: &[u8], now: i64) -> Result<bool, StoreError> {
        Ok(self.hash_expiry(db, key, now)?.is_some())
    }

    /// `Some(expires)` of one live field, if the hash exists.
    /// `Some(None)` is a live hash with no deadline.
    fn hash_expiry(
        &self,
        db: &mut dyn Q,
        key: &[u8],
        now: i64,
    ) -> Result<Option<Option<i64>>, StoreError> {
        let found = rows(
            db.q(
                "SELECT expires_at FROM hash_blobs \
                 WHERE hkey = ? AND (expires_at IS NULL OR expires_at > ?) \
                 LIMIT 1",
                &[Value::Blob(key.to_vec()), Value::Int(now)],
            )
            .map_err(engine)?,
        )?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(opt_int(&found[0], 0)?)),
            n => Err(StoreError::Engine(format!("hash lookup returned {n} rows"))),
        }
    }

    fn decode_row(&self, row: &Stored) -> Result<Vec<u8>, StoreError> {
        decompress(row.codec, &row.data, self.max_value_bytes).map_err(StoreError::Corrupt)
    }

    fn check_value(&self, value: &[u8]) -> Result<(), StoreError> {
        if value.len() > self.max_value_bytes {
            Err(StoreError::ValueTooLarge {
                bytes: value.len(),
                max: self.max_value_bytes,
            })
        } else {
            Ok(())
        }
    }

    /// The cache forgets `keys` when the returned value is dropped, which is
    /// after the write and, declared after the stripe guard, before the
    /// stripe is released. It runs on an error too: a write that failed may
    /// still have changed the row.
    fn forgetting<'a>(&'a self, keys: &[&'a [u8]]) -> Forget<'a> {
        Forget(&self.cache, keys.to_vec())
    }

    fn db_for(&self, key: &[u8]) -> &Database {
        &self.shards[shard_index(slot(key), self.shards.len())]
    }

    fn stripe(&self, key: &[u8]) -> MutexGuard<'_, ()> {
        self.stripe_slot(slot(key))
    }

    fn stripe_slot(&self, slot: u16) -> MutexGuard<'_, ()> {
        self.stripes[slot as usize % STRIPES]
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Drop for BlobStore {
    fn drop(&mut self) {
        if let Err(e) = self.drain() {
            eprintln!("mpedb-redis: held writes were not written: {e}");
        }
    }
}

struct Forget<'a>(&'a Cache, Vec<&'a [u8]>);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        if self.0.enabled() {
            for key in &self.1 {
                self.0.forget(slot(key) as usize % STRIPES, key);
            }
        }
    }
}

/// Where a statement runs: on its own, or inside one transaction.
///
/// A single-statement command stays an autocommit statement, which is the
/// shape the engine's group commit takes. A command that writes more than
/// once runs in a [`WriteSession`], so a failure halfway leaves nothing
/// behind and a reader never sees half of it.
trait Q {
    fn q(&mut self, sql: &str, params: &[Value]) -> mpedb::Result<ExecResult>;
}

impl Q for &Database {
    fn q(&mut self, sql: &str, params: &[Value]) -> mpedb::Result<ExecResult> {
        self.query(sql, params)
    }
}

impl Q for WriteSession<'_> {
    fn q(&mut self, sql: &str, params: &[Value]) -> mpedb::Result<ExecResult> {
        self.query(sql, params)
    }
}

struct Stored {
    data: Vec<u8>,
    codec: i64,
    expires: Option<i64>,
}

fn stored_from(row: &[Value]) -> Result<Stored, StoreError> {
    Ok(Stored {
        data: blob_col(row, 0)?,
        codec: req_int(row, 1)?,
        expires: opt_int(row, 2)?,
    })
}

fn is_live(expires: Option<i64>, now: i64) -> bool {
    match expires {
        None => true,
        Some(deadline) => deadline > now,
    }
}

fn ttl_of(expires: Option<i64>, now: i64) -> Ttl {
    match expires {
        None => Ttl::Persistent,
        Some(deadline) if deadline > now => Ttl::Remaining(deadline - now),
        Some(_) => Ttl::Missing,
    }
}

fn shard_index(slot: u16, shards: usize) -> usize {
    slot as usize % shards
}

fn shard_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("shard-{index:04}.mpedb"))
}

fn validate_options(opts: &StoreOptions) -> Result<(), StoreError> {
    if opts.shards == 0 || opts.shards > 256 {
        return Err(StoreError::Engine("shards must be in 1..=256".into()));
    }
    if opts.size_mb < 1 || opts.size_mb > mpedb::MAX_DB_SIZE_MB {
        return Err(StoreError::Engine(format!(
            "size_mb must be in 1..={}",
            mpedb::MAX_DB_SIZE_MB
        )));
    }
    match opts.durability.as_str() {
        "none" | "commit" | "async" | "wal" => {}
        other => {
            return Err(StoreError::Engine(format!(
                "durability must be none|commit|async|wal, got `{other}`"
            )))
        }
    }
    if !(1..=19).contains(&opts.zstd_level) {
        return Err(StoreError::Engine("zstd_level must be in 1..=19".into()));
    }
    if opts.max_value_bytes == 0 {
        return Err(StoreError::Engine(
            "max_value_bytes must be at least 1".into(),
        ));
    }
    Ok(())
}

/// The directory remembers how it was created. Changing the shard count or
/// the file size under an existing directory would either hide rows (a key
/// now hashes to a different file) or be refused by the engine (geometry is
/// file-authoritative). Saying so here names the knob the operator passed.
fn adopt_meta(dir: &Path, shards: usize, size_mb: u64) -> Result<(), StoreError> {
    let path = dir.join(META_NAME);
    if path.is_file() {
        let (have_shards, have_size) = read_meta(&path)?;
        if have_shards != shards || have_size != size_mb {
            return Err(StoreError::Engine(format!(
                "data directory {} was created with {have_shards} shard(s) and {have_size} MiB; \
                 this process asked for {shards} shard(s) and {size_mb} MiB",
                dir.display()
            )));
        }
        return Ok(());
    }
    if let Some(name) = stray_shard(dir)? {
        return Err(StoreError::Engine(format!(
            "data directory {} has {name} but no {META_NAME}; refusing to guess the shard count",
            dir.display()
        )));
    }
    let body = format!("shards={shards}\nsize_mb={size_mb}\n");
    let tmp = dir.join("META.tmp");
    // META is what lets the next start trust the shard files. Without the
    // syncs a power loss can keep the shards and lose META, and then every
    // start is refused until someone writes it back by hand.
    let meta_err = |what: &str, e: std::io::Error| StoreError::Engine(format!("{what} meta: {e}"));
    {
        use std::io::Write;
        let mut file = fs::File::create(&tmp).map_err(|e| meta_err("create", e))?;
        file.write_all(body.as_bytes())
            .map_err(|e| meta_err("write", e))?;
        file.sync_all().map_err(|e| meta_err("sync", e))?;
    }
    fs::rename(&tmp, &path).map_err(|e| meta_err("rename", e))?;
    #[cfg(unix)]
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| meta_err("sync directory of", e))?;
    Ok(())
}

fn read_meta(path: &Path) -> Result<(usize, u64), StoreError> {
    let text = fs::read_to_string(path)
        .map_err(|e| StoreError::Engine(format!("read {}: {e}", path.display())))?;
    let mut shards = None;
    let mut size_mb = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(v) = line.strip_prefix("shards=") {
            shards = Some(parse_meta(v, "shards")?);
        } else if let Some(v) = line.strip_prefix("size_mb=") {
            size_mb = Some(parse_meta(v, "size_mb")?);
        } else {
            return Err(StoreError::Engine(format!(
                "bad line in {}: {line}",
                path.display()
            )));
        }
    }
    match (shards, size_mb) {
        (Some(shards), Some(size_mb)) => Ok((shards, size_mb)),
        _ => Err(StoreError::Engine(format!(
            "{} is missing shards or size_mb",
            path.display()
        ))),
    }
}

fn parse_meta<T: std::str::FromStr>(text: &str, name: &str) -> Result<T, StoreError> {
    text.parse::<T>()
        .map_err(|_| StoreError::Engine(format!("meta {name} is not a number: {text}")))
}

fn stray_shard(dir: &Path) -> Result<Option<String>, StoreError> {
    let entries = fs::read_dir(dir)
        .map_err(|e| StoreError::Engine(format!("read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::Engine(format!("read dir: {e}")))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("shard-") && name.ends_with(".mpedb") {
            return Ok(Some(name.into_owned()));
        }
    }
    Ok(None)
}

fn shard_config(path: &Path, opts: &StoreOptions) -> Result<Config, StoreError> {
    let path = path
        .to_str()
        .ok_or_else(|| StoreError::Engine("shard path is not utf-8".into()))?;
    let path = toml_escape(path);
    // max_readers is frozen in the file header. Pinning it means a later
    // default in the engine cannot move the data region of a file we already
    // created.
    //
    // Under `async` extent_threshold is left unset, and the engine's default
    // there is off, because an extent's payload is synced before the commit
    // returns: with a 7 KiB compressed value on one connection that was
    // 53-79 SET/s against 3 068-3 996 without.
    //
    // Under `wal` the engine's default is 4 KiB, and a batch then pays an
    // msync of the data file on top of the log's fdatasync. Measured
    // 2026-11-21, eight writers, incompressible values, a volume whose flush
    // is ~5 ms, extents at 4 KiB against off: 7 KB 401-427 -> 751-794
    // writes/s, 64 KB 301-351 -> 310-332, 120 KB 265-269 -> 213-215, 250 KB
    // 172-194 -> 92-105, 500 KB 80-119 -> 62-67. Small values belong in the
    // log record and large ones do not, and the two cross near 64 KB.
    let extents = match opts.durability.as_str() {
        "wal" => "extent_threshold_kb = 64\n",
        _ => "",
    };
    // A checkpoint msyncs the data file while it holds the writer lock, and
    // the engine's default takes one every 16 MiB of log. A small `SET` logs
    // about 12 KiB of page images, so that was one every 1 400 writes.
    // Measured 2026-11-21 under `async`, eight connections, 400 000 `SET`s
    // of 64 B, at 16 MiB against 256 MiB: 2 464 and 2 423 writes/s with a
    // worst request of 1.2 s, against 13 087 and 13 548 with a worst of
    // 0.5 s. Pipelined 16 deep, 1 600 000 `SET`s: 4 360 against 36 184. At
    // 1 024 MiB it was 15 270, so 256 has most of it. The price is a log of
    // up to 256 MiB on disk and that much to replay after a crash.
    let checkpoint = match opts.durability.as_str() {
        "wal" | "async" => "wal_checkpoint_mb = 256\n",
        _ => "",
    };
    let text = format!(
        r#"[database]
path = "{path}"
size_mb = {size}
durability = "{durability}"
max_readers = 1024
{extents}{checkpoint}
[[table]]
name = "str_blobs"
primary_key = ["key"]

  [[table.column]]
  name = "key"
  type = "blob"

  [[table.column]]
  name = "data"
  type = "blob"

  [[table.column]]
  name = "codec"
  type = "int64"

  [[table.column]]
  name = "created_at"
  type = "int64"

  [[table.column]]
  name = "updated_at"
  type = "int64"

  [[table.column]]
  name = "expires_at"
  type = "int64"
  nullable = true
  indexed = true

  [[table.column]]
  name = "version"
  type = "int64"

[[table]]
name = "hash_blobs"
primary_key = ["hkey", "field"]

  [[table.column]]
  name = "hkey"
  type = "blob"

  [[table.column]]
  name = "field"
  type = "blob"

  [[table.column]]
  name = "data"
  type = "blob"

  [[table.column]]
  name = "codec"
  type = "int64"

  [[table.column]]
  name = "created_at"
  type = "int64"

  [[table.column]]
  name = "updated_at"
  type = "int64"

  [[table.column]]
  name = "expires_at"
  type = "int64"
  nullable = true
  indexed = true

  [[table.column]]
  name = "version"
  type = "int64"
"#,
        size = opts.size_mb,
        durability = opts.durability,
    );
    Config::from_toml_str(&text).map_err(|e| StoreError::Engine(e.to_string()))
}

fn check_parts(parts: &[Value]) -> Result<(), StoreError> {
    // The `expires_at` index entry is the deadline followed by the primary
    // key, and it has the same cap. A key that fits alone but not behind a
    // deadline could be SET and never given a TTL.
    let max = MAX_KEY - encode_key(&[Value::Int(0)]).len();
    let bytes = encode_key(parts).len();
    if bytes > max {
        Err(StoreError::KeyTooLong { bytes, max })
    } else {
        Ok(())
    }
}

fn rows(result: ExecResult) -> Result<Vec<Vec<Value>>, StoreError> {
    match result {
        ExecResult::Rows { rows, .. } => Ok(rows),
        other => Err(StoreError::Engine(format!("expected rows, got {other:?}"))),
    }
}

fn affected(result: ExecResult) -> Result<u64, StoreError> {
    match result {
        ExecResult::Affected(n) => Ok(n),
        other => Err(StoreError::Engine(format!(
            "expected an affected count, got {other:?}"
        ))),
    }
}

fn blob_col(row: &[Value], index: usize) -> Result<Vec<u8>, StoreError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(bytes.clone()),
        Some(other) => Err(StoreError::Engine(format!(
            "column {index} is {other:?}, expected a blob"
        ))),
        None => Err(StoreError::Engine(format!("column {index} is missing"))),
    }
}

fn req_int(row: &[Value], index: usize) -> Result<i64, StoreError> {
    match row.get(index) {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => Err(StoreError::Engine(format!(
            "column {index} is {other:?}, expected an integer"
        ))),
        None => Err(StoreError::Engine(format!("column {index} is missing"))),
    }
}

fn opt_int(row: &[Value], index: usize) -> Result<Option<i64>, StoreError> {
    match row.get(index) {
        Some(Value::Null) => Ok(None),
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(other) => Err(StoreError::Engine(format!(
            "column {index} is {other:?}, expected an integer or null"
        ))),
        None => Err(StoreError::Engine(format!("column {index} is missing"))),
    }
}

fn engine(err: mpedb::Error) -> StoreError {
    StoreError::Engine(err.to_string())
}

#[derive(Debug, Clone)]
pub enum StoreError {
    WrongType,
    KeyTooLong { bytes: usize, max: usize },
    ValueTooLarge { bytes: usize, max: usize },
    Corrupt(String),
    Engine(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::WrongType => write!(
                f,
                "WRONGTYPE Operation against a key holding the wrong kind of value"
            ),
            StoreError::KeyTooLong { bytes, max } => {
                write!(f, "encoded key is {bytes} bytes (max {max})")
            }
            StoreError::ValueTooLarge { bytes, max } => {
                write!(f, "value is {bytes} bytes (max {max})")
            }
            StoreError::Corrupt(message) => write!(f, "corrupt blob: {message}"),
            StoreError::Engine(message) => f.write_str(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = mpedb_testkit::scratch_base().join(format!(
            "mpedb-redis-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn open_in(dir: PathBuf, shards: usize) -> BlobStore {
        BlobStore::open(&StoreOptions {
            dir,
            shards,
            durability: "none".into(),
            size_mb: 8,
            zstd_level: 3,
            max_value_bytes: 1024 * 1024,
            cache_bytes: 1024 * 1024,
            write_behind: false,
        })
        .unwrap()
    }

    fn open(shards: usize) -> BlobStore {
        open_in(scratch(), shards)
    }

    #[test]
    fn a_held_set_answers_get_is_seen_by_del_and_reaches_the_file() {
        let dir = scratch();
        let opts = StoreOptions {
            dir: dir.clone(),
            shards: 2,
            durability: "none".into(),
            size_mb: 8,
            zstd_level: 3,
            max_value_bytes: 1024 * 1024,
            cache_bytes: 1024 * 1024,
            write_behind: true,
        };
        let store = BlobStore::open(&opts).unwrap();
        for (key, value) in [(b"a", b"1"), (b"b", b"2"), (b"a", b"3"), (b"c", b"4")] {
            store
                .set(key, value, SetExpiry::None, SetCond::Always, 10)
                .unwrap();
        }
        assert_eq!(store.get(b"a", 10).unwrap(), Some(b"3".to_vec()));
        // Nothing is in a file yet, and `DEL` still finds the key.
        assert_eq!(store.del(&[b"b"], 10).unwrap(), 1);
        assert_eq!(store.get(b"b", 10).unwrap(), None);
        // `NX` reads the file, so it waits for it.
        let nx = store.set(b"a", b"9", SetExpiry::None, SetCond::IfAbsent, 10);
        assert_eq!(nx.unwrap(), SetWritten::Skipped);
        store
            .set(b"c", b"5", SetExpiry::At(20), SetCond::Always, 10)
            .unwrap();
        assert_eq!(store.get(b"c", 20).unwrap(), None);
        assert_eq!(store.drain().unwrap(), 1);
        assert_eq!(store.get(b"c", 19).unwrap(), Some(b"5".to_vec()));
        store
            .set(b"d", b"6", SetExpiry::None, SetCond::Always, 10)
            .unwrap();
        // A clean stop writes what is held.
        drop(store);
        let store = BlobStore::open(&opts).unwrap();
        assert_eq!(store.get(b"a", 10).unwrap(), Some(b"3".to_vec()));
        assert_eq!(store.get(b"d", 10).unwrap(), Some(b"6".to_vec()));
        assert_eq!(store.ttl(b"c", 10).unwrap(), Ttl::Remaining(10));
    }

    #[test]
    fn string_round_trip_nx_xx_and_binary_key() {
        let store = open(1);
        let now = 1_000_000;
        let key = b"a\0b\r\n";
        assert_eq!(
            store
                .set(key, b"v1", SetExpiry::None, SetCond::Always, now)
                .unwrap(),
            SetWritten::Written
        );
        assert_eq!(store.get(key, now).unwrap().unwrap(), b"v1");
        assert_eq!(
            store
                .set(key, b"v2", SetExpiry::None, SetCond::IfAbsent, now)
                .unwrap(),
            SetWritten::Skipped
        );
        assert_eq!(store.get(key, now).unwrap().unwrap(), b"v1");
        assert_eq!(
            store
                .set(b"missing", b"x", SetExpiry::None, SetCond::IfPresent, now)
                .unwrap(),
            SetWritten::Skipped
        );
        assert!(store.get(b"missing", now).unwrap().is_none());
        store
            .set(key, b"", SetExpiry::None, SetCond::IfPresent, now)
            .unwrap();
        assert_eq!(store.get(key, now).unwrap().unwrap(), b"");
    }

    #[test]
    fn expiry_hides_a_row_and_reap_does_not_eat_a_replacement() {
        let store = open(1);
        store
            .set(b"k", b"old", SetExpiry::At(50), SetCond::Always, 1)
            .unwrap();
        assert!(store.get(b"k", 50).unwrap().is_none());
        assert_eq!(store.ttl(b"k", 50).unwrap(), Ttl::Missing);
        // The reaper could have selected the row at deadline 50. A SET lands
        // with no TTL before the delete runs. The delete is conditional on
        // the deadline it saw, so the new row stays.
        store
            .set(b"k", b"new", SetExpiry::None, SetCond::Always, 60)
            .unwrap();
        assert_eq!(store.reap(50, 10).unwrap(), 0);
        assert_eq!(store.get(b"k", 60).unwrap().unwrap(), b"new");

        store
            .set(b"gone", b"x", SetExpiry::At(10), SetCond::Always, 1)
            .unwrap();
        assert_eq!(store.reap(10, 10).unwrap(), 1);
        assert!(store.get(b"gone", 11).unwrap().is_none());
    }

    #[test]
    fn hash_ttl_is_shared_and_a_revival_does_not_keep_the_old_deadline() {
        let store = open(1);
        let now = 1_000;
        assert_eq!(store.hset(b"h", &[(b"f1", b"v1")], now).unwrap(), 1);
        assert!(store.expire_at(b"h", now + 5_000, now).unwrap());
        assert_eq!(store.hset(b"h", &[(b"f1", b"v1b")], now).unwrap(), 0);
        assert_eq!(store.hset(b"h", &[(b"f2", b"v2")], now).unwrap(), 1);
        assert_eq!(
            store.hget(b"h", b"f1", now + 4_999).unwrap().unwrap(),
            b"v1b"
        );
        assert_eq!(
            store.hget(b"h", b"f2", now + 4_999).unwrap().unwrap(),
            b"v2"
        );
        assert!(store.hget(b"h", b"f2", now + 5_000).unwrap().is_none());

        assert_eq!(
            store
                .hset(b"h", &[(b"f1", b"revived")], now + 5_000)
                .unwrap(),
            1
        );
        assert_eq!(
            store.hget(b"h", b"f1", now + 5_000).unwrap().unwrap(),
            b"revived"
        );
        assert_eq!(store.ttl(b"h", now + 5_000).unwrap(), Ttl::Persistent);
        assert_eq!(store.hdel(b"h", &[b"f1"], now + 5_000).unwrap(), 1);
        assert_eq!(store.exists(&[b"h"], now + 5_000).unwrap(), 0);
    }

    #[test]
    fn set_many_is_one_commit_per_file() {
        let store = open(1);
        let keys: Vec<Vec<u8>> = (0..8).map(|i| format!("k{i}").into_bytes()).collect();
        let items: Vec<SetItem<'_>> = keys
            .iter()
            .map(|key| SetItem {
                key,
                value: b"v",
                expiry: SetExpiry::None,
                cond: SetCond::Always,
            })
            .collect();
        let before = store.shards[0].leak_counters().unwrap().0;
        let results = store.set_many(&items, 1);
        assert!(results.iter().all(|r| matches!(r, Ok(SetWritten::Written))));
        assert_eq!(store.shards[0].leak_counters().unwrap().0, before + 1);
        assert_eq!(store.get(b"k7", 1).unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn wrongtype_and_del_exists_ttl() {
        let store = open(1);
        let now = 10;
        store
            .set(b"s", b"v", SetExpiry::None, SetCond::Always, now)
            .unwrap();
        assert!(matches!(
            store.hget(b"s", b"f", now).unwrap_err(),
            StoreError::WrongType
        ));
        assert!(matches!(
            store.hset(b"s", &[(b"f", b"v")], now).unwrap_err(),
            StoreError::WrongType
        ));
        assert_eq!(store.del(&[b"s"], now).unwrap(), 1);
        assert_eq!(store.del(&[b"s"], now).unwrap(), 0);
        store.hset(b"h", &[(b"f", b"v")], now).unwrap();
        assert!(matches!(
            store
                .set(b"h", b"v", SetExpiry::None, SetCond::Always, now)
                .unwrap_err(),
            StoreError::WrongType
        ));
        assert!(matches!(
            store.get(b"h", now).unwrap_err(),
            StoreError::WrongType
        ));
        assert_eq!(store.exists(&[b"h", b"missing"], now).unwrap(), 1);
        assert!(store.expire_at(b"h", now + 2_500, now).unwrap());
        assert_eq!(store.ttl(b"h", now).unwrap(), Ttl::Remaining(2_500));
        assert!(store.persist(b"h", now).unwrap());
        assert_eq!(store.ttl(b"h", now).unwrap(), Ttl::Persistent);
        assert!(!store.persist(b"h", now).unwrap());
        assert_eq!(store.del(&[b"h"], now).unwrap(), 1);
        assert_eq!(store.ttl(b"h", now).unwrap(), Ttl::Missing);
        assert_eq!(store.exists(&[b"nope"], now).unwrap(), 0);
    }

    #[test]
    fn two_shards_round_trip_the_slot_they_were_written_on() {
        let store = open(2);
        let now = 1;
        for i in 0..50u8 {
            let key = [i];
            store
                .set(&key, &[i], SetExpiry::None, SetCond::Always, now)
                .unwrap();
        }
        for i in 0..50u8 {
            assert_eq!(store.get(&[i], now).unwrap().unwrap(), vec![i]);
        }
        assert_eq!(store.shards(), 2);
    }

    #[test]
    fn meta_refuses_a_different_shard_count() {
        let dir = scratch();
        open_in(dir.clone(), 1);
        let err = BlobStore::open(&StoreOptions {
            dir,
            shards: 2,
            durability: "none".into(),
            size_mb: 8,
            zstd_level: 3,
            max_value_bytes: 100,
            cache_bytes: 0,
            write_behind: false,
        })
        .err()
        .expect("a different shard count must be refused");
        let message = err.to_string();
        assert!(message.contains("1 shard"), "{message}");
        assert!(message.contains("2 shard"), "{message}");
    }

    #[test]
    fn repetitive_payload_is_stored_compressed() {
        let store = open(1);
        let raw = br#"{"w":1,"draw":"line"}"#.repeat(30);
        store
            .set(b"j", &raw, SetExpiry::None, SetCond::Always, 1)
            .unwrap();
        assert_eq!(store.get(b"j", 1).unwrap().unwrap(), raw);
        let db = store.db_for(b"j");
        let found = rows(
            db.query(
                "SELECT codec, data FROM str_blobs WHERE key = ?",
                &[Value::Blob(b"j".to_vec())],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(req_int(&found[0], 0).unwrap(), crate::compress::CODEC_ZSTD);
        let stored = blob_col(&found[0], 1).unwrap();
        assert!(
            stored.len() < raw.len(),
            "{} vs {}",
            stored.len(),
            raw.len()
        );
    }
}
