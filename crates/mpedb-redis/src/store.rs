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
//! per file. A stripe is always taken before a writer lock. `DEL` is the
//! only path that holds more than one stripe, and it takes them ascending.
//!
//! Reads (`GET`, `HGET`, `EXISTS`, `TTL`) do not take the per-slot stripe.
//! A call that races a type change can answer nil once. It does not return
//! the other type's bytes.

use crate::compress::{compress, decompress};
use crate::crc16::slot;
use mpedb::{toml_escape, Config, Database, ExecResult, Value, WriteSession};
use mpedb_core::btree::MAX_KEY;
use mpedb_types::keycode::encode_key;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

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
}

pub struct BlobStore {
    dir: PathBuf,
    shards: Vec<Database>,
    stripes: [Mutex<()>; STRIPES],
    zstd_level: i32,
    max_value_bytes: usize,
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
        Ok(BlobStore {
            dir: opts.dir.clone(),
            shards,
            stripes: std::array::from_fn(|_| Mutex::new(())),
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
        check_parts(&[Value::Blob(key.to_vec())])?;
        let db: &mut dyn Q = &mut self.db_for(key);
        if let Some(row) = self.live_string(db, key, now)? {
            return Ok(Some(self.decode_row(&row)?));
        }
        if self.live_hash(db, key, now)? {
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
        self.check_value(value)?;
        check_parts(&[Value::Blob(key.to_vec())])?;
        let (codec, stored) = compress(value, self.zstd_level)
            .map_err(|e| StoreError::Engine(format!("zstd: {e}")))?;
        let _guard = self.stripe(key);
        let db: &mut dyn Q = &mut self.db_for(key);
        if self.live_hash(db, key, now)? {
            return Err(StoreError::WrongType);
        }
        let live = self.string_live(db, key, now)?;
        match cond {
            SetCond::IfAbsent if live => return Ok(SetWritten::Skipped),
            SetCond::IfPresent if !live => return Ok(SetWritten::Skipped),
            _ => {}
        }
        let expires = match expiry {
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
                    Value::Blob(stored),
                    Value::Int(codec),
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
        // Every key is checked before any is deleted. A bad key in second
        // place used to cost the first one.
        for key in keys {
            check_parts(&[Value::Blob(key.to_vec())])?;
        }
        if let [key] = keys {
            let _guard = self.stripe(key);
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
        check_parts(&[Value::Blob(key.to_vec())])?;
        let _guard = self.stripe(key);
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
        check_parts(&[Value::Blob(key.to_vec())])?;
        let _guard = self.stripe(key);
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
                let n = affected(
                    db.query(
                        "DELETE FROM str_blobs \
                         WHERE key = ? AND expires_at IS NOT NULL AND expires_at <= ?",
                        &[Value::Blob(key), Value::Int(now)],
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
    // is ~5 ms, extents at 4 KiB against off: 7 KB 211 -> 723-845 writes/s,
    // 64 KB 177-229 -> 311-360, 120 KB 206-221 -> 212-232, 250 KB 163-173 ->
    // 114-122, 500 KB 86-111 -> 55-61. Small values belong in the log record
    // and large ones do not, and 64 KiB is below where the two cross.
    let extents = match opts.durability.as_str() {
        "wal" => "extent_threshold_kb = 64\n",
        _ => "",
    };
    let text = format!(
        r#"[database]
path = "{path}"
size_mb = {size}
durability = "{durability}"
max_readers = 1024
{extents}
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

#[derive(Debug)]
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
        })
        .unwrap()
    }

    fn open(shards: usize) -> BlobStore {
        open_in(scratch(), shards)
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
