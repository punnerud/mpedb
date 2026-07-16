//! The engine: transactions, catalog, freelist, and the typed row API.
//!
//! Ties the shared-memory layer (`shm`) to the COW B+tree (`btree`):
//! - `ReadTxn` — a pinned MVCC snapshot; lock-free, read-only.
//! - `WriteTxn` — the writer-lock holder; implements [`PageStore`] with COW
//!   discipline, allocates from the freelist/high-water mark, and commits via
//!   the double-buffered meta protocol (DESIGN.md §5.2).
//!
//! Catalog tree layout (root in meta.catalog_root):
//! - key `[0x00]` → canonical schema bytes
//! - key `[0x01, table_id BE, index_no BE]` → `[root u64 LE ‖ row_count u64 LE]`
//!
//! Freelist tree layout (root in meta.freelist_root):
//! - key = freeing txn id (u64 BE) → concatenated freed page ids (u64 LE each)

/// Diagnostic counters for the high-water leak (#37, see
/// `tests/high_water_leak.rs`). Process-local and Relaxed, dumped by
/// `mpedb/examples/leak_probe`.
///
/// Off unless built with `--features leakstat`: `bump`/`add` compile to nothing,
/// so the alloc path pays zero. Five hypotheses about this leak have died on
/// measurement — the instrument stays so the sixth is cheap to test.
pub mod leakstat {
    use std::sync::atomic::{AtomicU64, Ordering};
    macro_rules! counters {
        ($($n:ident),* $(,)?) => {
            $(pub static $n: AtomicU64 = AtomicU64::new(0);)*
            pub fn dump(tag: &str) {
                let mut s = String::new();
                $(s.push_str(&format!(" {}={}", stringify!($n).to_lowercase(),
                    $n.load(Ordering::Relaxed)));)*
                eprintln!("leakstat[{tag}]:{s}");
            }
        };
    }
    counters!(
        ALLOC_REUSABLE,   // alloc served from a reclaimed page
        ALLOC_HW,         // alloc that bumped the high-water mark — THE LEAK
        ALLOC_HW_IN_FL,   // ...of those, ones made *during* the commit fixpoint
        REFILL_CALLS,     // refill_reusable entered
        REFILL_NO_TREE,   // ...freelist empty
        REFILL_NOT_YET,   // ...oldest entry newer than the bound
        REFILL_OK,        // ...reclaimed an entry
        REFILL_PAGES,     // pages that reclaim yielded
        RECOMPUTES,       // compute_oldest_pinned calls (the only bound advance)
        COMMIT_ENTRIES,   // freelist entries written by commit fixpoints
        COMMIT_PAGES,     // page ids recorded by commit fixpoints
        COMMITS,          // commit fixpoints entered
        COMMIT_FREED,     // pages genuinely freed by the txn
        COMMIT_LEFTOVER,  // reclaimed-but-unused pages handed back at commit
        // #40: where the ~3.8 µs per overflow page goes. Nanoseconds — the three
        // must add up to the wall time `btree::write_overflow` takes, or the
        // cost is somewhere this does not look.
        OVF_PAGES,        // overflow pages written
        OVF_NS_ALLOC,     // ...in alloc_raw (freelist pop, refill, dirty insert)
        OVF_NS_WRITE,     // ...in page_mut + header + payload memcpy + tail zero
        OVF_NS_CHAIN,     // ...in the 2nd page_mut, only to set prev's next-ptr
        // #40: the phases of insert_row. write_overflow is only 20% of execute;
        // one of these has to hold the other 80%.
        INS_NS_VALIDATE,  // validate_row
        INS_NS_ENCODE,    // row::encode_row — copies the blob a SECOND time
        INS_NS_BTREE,     // btree::insert (which contains write_overflow)
        INS_NS_COMMIT,    // commit_with: freelist fixpoint + meta publish
        EXEC_NS_RESOLVE,  // session::resolve_params — DEEP-clones every Value
    );
    #[inline(always)]
    pub fn bump(c: &AtomicU64) {
        add(c, 1);
    }
    #[inline(always)]
    pub fn add(c: &AtomicU64, n: u64) {
        #[cfg(feature = "leakstat")]
        c.fetch_add(n, Ordering::Relaxed);
        #[cfg(not(feature = "leakstat"))]
        {
            let _ = (c, n);
        }
    }
}

use crate::btree::{self, InsertMode};
use crate::cdc::{self, CaptureConfig, DirtyEntry, DirtyOp};
use crate::pagestore::PageStore;
use crate::row;
use crate::shm::{MetaSnapshot, Shm};
use mpedb_types::{
    keycode, Concurrency, ColumnType, Config, Durability, Error, ExprProgram, Result, Schema, Value,
    PAGE_SIZE,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

/// Deferred-fsync interval for `durability = async` (§5.4.2), env-overridable
/// via `MPEDB_WAL_FLUSH_MS` (default 10 ms; min 1 ms). The flush interval is
/// the upper bound on the power-loss loss window in wall-clock terms.
fn wal_flush_interval() -> Duration {
    static MS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        std::env::var("MPEDB_WAL_FLUSH_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10)
    });
    Duration::from_millis((*MS).max(1))
}

/// The background deferred-fsync flusher (durability = async). Owns a clone of
/// the shared `Arc<Shm>`, so the mapping is not unmapped until this thread has
/// joined (Engine::drop joins before releasing its own Arc).
struct Flusher {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

fn spawn_flusher(shm: Arc<Shm>) -> Flusher {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let interval = wal_flush_interval();
    let handle = std::thread::Builder::new()
        .name("mpedb-wal-flush".into())
        .spawn(move || {
            while !stop_thread.load(AtomicOrdering::Acquire) {
                std::thread::sleep(interval);
                // Best-effort: a transient flush error just leaves wal_len
                // where it was; the next tick (or the next writer's own path)
                // retries. Never propagated — a background flush cannot fail a
                // commit that already returned.
                let _ = shm.wal_flush_deferred();
            }
            let _ = shm.wal_flush_deferred(); // final flush on clean shutdown
        })
        .expect("spawn mpedb wal flusher");
    Flusher { stop, handle }
}

const CAT_SCHEMA_KEY: &[u8] = &[0x00];

fn cat_tree_key(table_id: u32, index_no: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(0x01);
    k.extend_from_slice(&table_id.to_be_bytes());
    k.extend_from_slice(&index_no.to_be_bytes());
    k
}

/// Freed-page lists are chunked so every freelist value stays inline in its
/// leaf (never an overflow chain): rewriting an inline value in a dirty leaf
/// frees and allocates nothing, which is what makes the commit-time fixpoint
/// (DESIGN.md §4.5) converge.
const FREELIST_CHUNK_PAGES: usize = 120; // 960-byte values

/// How deep a page pool a writer draws before allocating, and how many freelist
/// entries it may draw from to get there (#37). Drawing is read-only and an
/// undrawn-down entry costs nothing at commit, so the pool exists purely to
/// keep the commit fixpoint — which cannot refill — off the high-water mark.
const FREELIST_POOL_TARGET: usize = 32;
const FREELIST_POOL_DRAWS: u32 = 4;

/// `((txn_id, high_water, bound), [(freed_txn, n_pages)] oldest first)` —
/// see [`Engine::freelist_shape`].
pub type FreelistShape = ((u64, u64, u64), Vec<(u64, usize)>);

/// Pages at the top of the file reserved for control-plane writes (mirror
/// HALTED/frozen/cursor/park markers) so they can still commit when the data
/// region is full (DESIGN-MIRROR §3.10). Only `WriteTxn::set_reserved_alloc`
/// txns may allocate from this band; data and CDC capture hit DbFull first.
/// Sized for a few small sys-record commits (each ~ one record + catalog COW +
/// freelist fixpoint), not for bulk work.
const RESERVED_CONTROL_PAGES: u64 = 48;

/// System keys live in the catalog tree under a reserved prefix; the facade
/// uses them for the shared plan registry.
const SYS_PREFIX: u8 = 0x02;

fn sys_key(subkey: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + subkey.len());
    k.push(SYS_PREFIX);
    k.extend_from_slice(subkey);
    k
}

/// Best-effort wall-clock micros since the Unix epoch, for CDC dirty entries
/// (used only by the off-by-default newest-wins conflict policy). A clock before
/// the epoch yields 0 rather than a negative surprise.
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn freelist_key(txn: u64, chunk: u16) -> [u8; 10] {
    let mut k = [0u8; 10];
    k[..8].copy_from_slice(&txn.to_be_bytes());
    k[8..].copy_from_slice(&chunk.to_be_bytes());
    k
}

/// Secondary unique index columns for a table, per the shared numbering
/// convention (DESIGN.md §4.4): index 0 = PK tree; unique columns in
/// declaration order get 1, 2, …; a column that is by itself the whole PK is
/// skipped.
/// The secondary-index B+tree key for value `v` of a row whose primary key
/// encodes to `pk_key`. A UNIQUE index keys by the value alone; a non-unique one
/// appends the pk so duplicate values become distinct, memcmp-ordered
/// (value, pk) entries — and `encode_key` is a plain concatenation of
/// `encode_value`, so this equals `encode_key([v, ...pk_values])`.
fn index_ikey(unique: bool, v: &mpedb_types::Value, pk_key: &[u8]) -> Vec<u8> {
    let mut k = keycode::encode_key(std::slice::from_ref(v));
    if !unique {
        k.extend_from_slice(pk_key);
    }
    k
}

pub fn secondary_index_columns(table: &mpedb_types::TableDef) -> Vec<u16> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            // A column with `unique` OR `indexed` is a secondary index. The PK's
            // own single column is skipped — it already has index 0 (the PK
            // tree). Both engine and SQL derive this identically, in
            // column-declaration order, so index numbers agree (CLAUDE.md).
            (c.unique || c.indexed)
                && !(table.primary_key.len() == 1 && table.primary_key[0] == *i as u16)
        })
        .map(|(i, _)| i as u16)
        .collect()
}

/// Per-column compiled CHECK programs, one entry per table (indexed like
/// `schema.tables`), one per column. Compiled by the facade (SQL layer);
/// `None` = no CHECK on that column.
pub type CheckPrograms = Vec<Vec<Option<ExprProgram>>>;

pub struct Engine {
    shm: Arc<Shm>,
    schema: Schema,
    checks: CheckPrograms,
    sec_indexes: Vec<Vec<u16>>,
    /// Parallel to `sec_indexes`: is index `k` UNIQUE (value→pk, enforced) or a
    /// plain non-unique index (composite `(value‖pk)` key, duplicates allowed)?
    /// The storage form and whether an insert is uniqueness-checked both follow
    /// from this. Same order as `sec_indexes`.
    sec_unique: Vec<Vec<bool>>,
    col_types: Vec<Vec<ColumnType>>,
    concurrency: Concurrency,
    /// Deferred-fsync flusher; `Some` only for `durability = async` (§5.4.2).
    flusher: Option<Flusher>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(f) = self.flusher.take() {
            // Stop, then do a synchronous final flush ourselves (covers the
            // window even if the thread is mid-sleep), then join so the
            // mapping outlives the thread's last access before our Arc drops.
            f.stop.store(true, AtomicOrdering::Release);
            let _ = self.shm.wal_flush_deferred();
            let _ = f.handle.join();
        }
    }
}

impl Engine {
    /// Open or create the database described by `config`. `checks` must have
    /// one entry per table (empty vecs are fine if no CHECKs are used — the
    /// facade compiles them from the schema's `check` sources).
    pub fn open(config: &Config, checks: CheckPrograms) -> Result<Engine> {
        let schema = config.schema.clone();
        if checks.len() != schema.tables.len() {
            return Err(Error::Internal(
                "checks vector does not match schema table count".into(),
            ));
        }
        let shm = Shm::open(
            &config.options.path,
            config.options.size_bytes,
            config.options.max_readers,
            config.options.durability,
            &schema.hash(),
            &config.options.perms,
        )?;
        let sec_indexes: Vec<Vec<u16>> =
            schema.tables.iter().map(secondary_index_columns).collect();
        let sec_unique: Vec<Vec<bool>> = schema
            .tables
            .iter()
            .zip(&sec_indexes)
            .map(|(t, cols)| cols.iter().map(|&c| t.columns[c as usize].unique).collect())
            .collect();
        let col_types = schema
            .tables
            .iter()
            .map(|t| t.columns.iter().map(|c| c.ty).collect())
            .collect();
        let shm = Arc::new(shm);
        // durability = async: a background thread coalesces fdatasync on a
        // bounded interval so commits ack without waiting for the flush
        // (crash-consistent, power-loss loses a bounded window — §5.4.2).
        let flusher = (config.options.durability == Durability::Async)
            .then(|| spawn_flusher(shm.clone()));
        let engine = Engine {
            shm,
            schema,
            checks,
            sec_indexes,
            sec_unique,
            col_types,
            concurrency: config.options.concurrency,
            flusher,
        };
        engine.bootstrap_catalog()?;
        Ok(engine)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// First writer initializes the catalog; racing processes see a non-zero
    /// catalog root under the writer lock and skip.
    fn bootstrap_catalog(&self) -> Result<()> {
        if self.shm.newest_meta()?.catalog_root != 0 {
            return Ok(());
        }
        let mut txn = self.begin_write()?;
        if txn.catalog_root != 0 {
            txn.abort();
            return Ok(());
        }
        let schema_bytes = self.schema.canonical_bytes();
        let out = btree::insert(
            &mut txn,
            0,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&schema_bytes),
            InsertMode::InsertOnly,
        )?;
        txn.catalog_root = out.new_root;
        for (tid, table) in self.schema.tables.iter().enumerate() {
            let mut index_nos = vec![0u32];
            index_nos.extend((1..=self.sec_indexes[tid].len()).map(|i| i as u32));
            for ino in index_nos {
                let root = txn.catalog_root;
                let out = btree::insert(
                    &mut txn,
                    root,
                    &cat_tree_key(tid as u32, ino),
                    &mut btree::Payload::Flat(&[0u8; 16]),
                    InsertMode::InsertOnly,
                )?;
                txn.catalog_root = out.new_root;
            }
            let _ = table;
        }
        txn.commit()
    }

    /// Open an existing database from the file alone — geometry and schema
    /// are read from the file (no config needed). For tooling (`mpedb dump`);
    /// CHECK constraints are not enforced through this handle.
    pub fn open_from_file(path: &std::path::Path) -> Result<Engine> {
        let shm = Shm::open_existing(path)?;
        // read the stored schema under a pinned snapshot
        let schema = {
            let (slot, word, meta) = shm.claim_and_pin()?;
            struct Raw<'a>(&'a Shm);
            impl PageStore for Raw<'_> {
                fn page(&self, id: u64) -> Result<&[u8]> {
                    self.0.page(id)
                }
                fn page_mut(&mut self, _: u64) -> Result<&mut [u8]> {
                    Err(Error::Internal("read-only".into()))
                }
                fn alloc(&mut self) -> Result<u64> {
                    Err(Error::Internal("read-only".into()))
                }
                fn free(&mut self, _: u64) -> Result<()> {
                    Err(Error::Internal("read-only".into()))
                }
                fn is_dirty(&self, _: u64) -> bool {
                    false
                }
            }
            let res = btree::get(&Raw(&shm), meta.catalog_root, CAT_SCHEMA_KEY);
            shm.release_slot(slot, word);
            let bytes = res?.ok_or_else(|| Error::Corrupt("no schema stored in catalog".into()))?;
            Schema::from_canonical_bytes(&bytes)?
        };
        let sec_indexes: Vec<Vec<u16>> =
            schema.tables.iter().map(secondary_index_columns).collect();
        let sec_unique: Vec<Vec<bool>> = schema
            .tables
            .iter()
            .zip(&sec_indexes)
            .map(|(t, cols)| cols.iter().map(|&c| t.columns[c as usize].unique).collect())
            .collect();
        let col_types = schema
            .tables
            .iter()
            .map(|t| t.columns.iter().map(|c| c.ty).collect())
            .collect();
        let checks = vec![Vec::new(); schema.tables.len()];
        Ok(Engine {
            shm: Arc::new(shm),
            schema,
            checks,
            sec_indexes,
            sec_unique,
            col_types,
            concurrency: Concurrency::Serial,
            flusher: None, // read-only tooling handle; async needs a config
        })
    }

    /// The write-path concurrency discipline this engine was opened with
    /// (DESIGN-PHASE3.md). Serial is the default and shipped behavior.
    pub fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    /// Whether `table_id` has any secondary (unique) index. Optimistic
    /// blind-apply is only eligible for tables without one — index
    /// maintenance needs the current row and degrades footprints below the
    /// key level (DESIGN.md §7.3 honesty rule).
    pub fn has_secondary_index(&self, table_id: u32) -> bool {
        self.sec_indexes
            .get(table_id as usize)
            .is_some_and(|s| !s.is_empty())
    }

    /// Validate a full row against the schema (types, NOT NULL, CHECK) without
    /// mutating anything — used by the optimistic prep pass off the writer
    /// lock. Public wrapper over the internal validator.
    pub fn validate_row_public(&self, table_id: u32, values: &[Value]) -> Result<()> {
        self.validate_row(table_id, values)
    }

    /// Column types for `table_id` (for off-lock row encoding in optimistic
    /// prep).
    pub fn col_types(&self, table_id: u32) -> Option<&[ColumnType]> {
        self.col_types.get(table_id as usize).map(|v| v.as_slice())
    }

    /// Verify the page-accounting invariant (DESIGN.md §4.5): every page in
    /// the data region below the high-water mark is either reachable from the
    /// committed roots or listed in the freelist — exactly one of the two.
    /// Takes the writer lock for a stable view; commits nothing.
    /// Diagnostic counters for the high-water leak
    /// (`crates/mpedb-core/tests/high_water_leak.rs`):
    /// `(txn_id, high_water, oldest_pinned_bound, freelist_entries)`.
    ///
    /// Reads the committed meta and walks the freelist, so it costs a scan —
    /// it exists to be called a few times a second by a probe, not on any path
    /// that matters. It takes no writer lock and pins nothing: perturbing the
    /// reader table is exactly what would corrupt the measurement.
    pub fn leak_counters(&self) -> Result<(u64, u64, u64, u64)> {
        let meta = self.shm.newest_meta()?;
        let bound = self
            .shm
            .oldest_pinned_cache()
            .load(std::sync::atomic::Ordering::Acquire);
        let mut ents = 0u64;
        if meta.freelist_root != 0 {
            let r = self.begin_read()?;
            let mut c = btree::cursor(&r, meta.freelist_root, None, None)?;
            while c.next(&r)?.is_some() {
                ents += 1;
            }
        }
        Ok((meta.txn_id, meta.high_water, bound, ents))
    }

    /// TEMPORARY (#37): the freelist's *shape* — `(freed_txn, n_pages)` per
    /// entry, oldest first, plus `(txn_id, high_water, bound)`. Tells apart
    /// "entries are stuck (old, reclaimable, never drained)" from "entries are
    /// churn (all fresh)" — an aggregate counter cannot.
    pub fn freelist_shape(&self) -> Result<FreelistShape> {
        let meta = self.shm.newest_meta()?;
        let bound = self
            .shm
            .oldest_pinned_cache()
            .load(std::sync::atomic::Ordering::Acquire);
        let mut out = Vec::new();
        if meta.freelist_root != 0 {
            let r = self.begin_read()?;
            let mut c = btree::cursor(&r, meta.freelist_root, None, None)?;
            while let Some((k, v)) = c.next(&r)? {
                if k.len() == 10 {
                    out.push((u64::from_be_bytes(k[..8].try_into().unwrap()), v.len() / 8));
                }
            }
        }
        Ok(((meta.txn_id, meta.high_water, bound), out))
    }

    pub fn verify_page_accounting(&self) -> Result<()> {
        let txn = self.begin_write()?;
        let res = (|| {
            let mut reachable = std::collections::BTreeSet::new();
            btree::collect_pages(&txn, txn.catalog_root, &mut reachable)?;
            let lo = [0x01u8];
            let hi = [SYS_PREFIX];
            let mut c = btree::cursor(
                &txn,
                txn.catalog_root,
                Some((&lo[..], true)),
                Some((&hi[..], false)),
            )?;
            let mut roots = Vec::new();
            while let Some((_k, v)) = c.next(&txn)? {
                if v.len() == 16 {
                    roots.push(u64::from_le_bytes(v[0..8].try_into().unwrap()));
                }
            }
            for r in roots {
                btree::collect_pages(&txn, r, &mut reachable)?;
            }
            btree::collect_pages(&txn, txn.freelist_root, &mut reachable)?;

            let mut freelisted = std::collections::BTreeSet::new();
            if txn.freelist_root != 0 {
                let mut c = btree::cursor(&txn, txn.freelist_root, None, None)?;
                while let Some((_k, v)) = c.next(&txn)? {
                    for ch in v.chunks_exact(8) {
                        let id = u64::from_le_bytes(ch.try_into().unwrap());
                        if !freelisted.insert(id) {
                            return Err(Error::Corrupt(format!(
                                "page {id} listed twice in freelist"
                            )));
                        }
                    }
                }
            }
            for id in &freelisted {
                if reachable.contains(id) {
                    return Err(Error::Corrupt(format!(
                        "page {id} both reachable and freelisted"
                    )));
                }
            }
            for id in self.shm.data_start..txn.high_water {
                if !reachable.contains(&id) && !freelisted.contains(&id) {
                    return Err(Error::Corrupt(format!(
                        "page {id} leaked: neither reachable nor freelisted"
                    )));
                }
            }
            for &id in reachable.iter().chain(freelisted.iter()) {
                if id < self.shm.data_start || id >= txn.high_water {
                    return Err(Error::Corrupt(format!(
                        "page {id} recorded outside the data region"
                    )));
                }
            }
            Ok(())
        })();
        txn.abort();
        res
    }

    pub fn begin_read(&self) -> Result<ReadTxn<'_>> {
        let (slot, word, meta) = self.shm.claim_and_pin()?;
        Ok(ReadTxn {
            eng: self,
            slot,
            word,
            meta,
            released: false,
        })
    }

    /// Non-blocking variant of [`Engine::begin_write`]: Ok(None) if another
    /// process currently holds the writer lock.
    pub fn try_begin_write(&self) -> Result<Option<WriteTxn<'_>>> {
        let Some(recovered) = self.shm.try_writer_lock()? else {
            return Ok(None);
        };
        self.make_write_txn(recovered).map(Some)
    }

    /// The shared intent ring (leader-side methods require holding a
    /// [`WriteTxn`], i.e. the writer lock).
    pub fn ring(&self) -> crate::ring::IntentRing<'_> {
        crate::ring::IntentRing::new(&self.shm)
    }

    pub fn durability(&self) -> mpedb_types::Durability {
        self.shm.durability
    }

    pub fn begin_write(&self) -> Result<WriteTxn<'_>> {
        let recovered = self.shm.writer_lock()?;
        self.make_write_txn(recovered)
    }

    fn make_write_txn(&self, recovered: bool) -> Result<WriteTxn<'_>> {
        let meta = match self.shm.newest_meta() {
            Ok(m) => m,
            Err(e) => {
                self.shm.writer_unlock();
                return Err(e);
            }
        };
        Ok(WriteTxn {
            eng: self,
            meta,
            catalog_root: meta.catalog_root,
            freelist_root: meta.freelist_root,
            high_water: meta.high_water,
            table_roots: HashMap::new(),
            dirty: DirtySet::default(),
            reusable: Vec::new(),
            freed: BTreeSet::new(),
            taken: Vec::new(),
            refill_cursor: None,
            bound_recomputed: false,
            in_freelist_op: false,
            recovered,
            finished: false,
            written_tables: 0,
            commit_point: None,
            capture_enabled: true,
            capture_cfg: None,
            reserved_alloc: false,
        })
    }

    // ---------- row-level helpers shared by both txn kinds ----------

    fn table(&self, table_id: u32) -> Result<&mpedb_types::TableDef> {
        self.schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))
    }

    fn pk_key(&self, table_id: u32, values: &[Value]) -> Result<Vec<u8>> {
        let table = self.table(table_id)?;
        let pk_vals: Vec<Value> = table
            .primary_key
            .iter()
            .map(|&i| values[i as usize].clone())
            .collect();
        Ok(keycode::encode_key(&pk_vals))
    }

    /// Validate a full row against the schema: arity, rigid types, NOT NULL,
    /// CHECK (SQL semantics: violated only when the predicate is FALSE —
    /// NULL/UNKNOWN passes).
    fn validate_row(&self, table_id: u32, values: &[Value]) -> Result<()> {
        let table = self.table(table_id)?;
        if values.len() != table.columns.len() {
            return Err(Error::TypeMismatch(format!(
                "table `{}` has {} columns, row has {}",
                table.name,
                table.columns.len(),
                values.len()
            )));
        }
        for (v, c) in values.iter().zip(&table.columns) {
            if !v.fits(c.ty) {
                return Err(Error::TypeMismatch(format!(
                    "column `{}.{}` is {}, value is {}",
                    table.name,
                    c.name,
                    c.ty,
                    v.type_name()
                )));
            }
            if v.is_null() && !c.nullable {
                return Err(Error::NotNullViolation {
                    table: table.name.clone(),
                    column: c.name.clone(),
                });
            }
        }
        let mut stack = Vec::new();
        for (ci, check) in self.checks[table_id as usize].iter().enumerate() {
            if let Some(program) = check {
                match program.eval_with_stack(&mut stack, values, &[])? {
                    Value::Bool(false) => {
                        let c = &table.columns[ci];
                        return Err(Error::CheckViolation {
                            table: table.name.clone(),
                            column: c.name.clone(),
                            expr: c.check.clone().unwrap_or_default(),
                        });
                    }
                    Value::Bool(true) | Value::Null => {}
                    v => {
                        return Err(Error::TypeMismatch(format!(
                            "CHECK evaluated to {}, expected bool",
                            v.type_name()
                        )))
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- ReadTxn

pub struct ReadTxn<'e> {
    eng: &'e Engine,
    slot: u32,
    word: u64,
    pub meta: MetaSnapshot,
    released: bool,
}

impl PageStore for ReadTxn<'_> {
    fn page(&self, id: u64) -> Result<&[u8]> {
        self.eng.shm.page(id)
    }
    fn page_mut(&mut self, _id: u64) -> Result<&mut [u8]> {
        Err(Error::Internal("write through a read transaction".into()))
    }
    fn alloc(&mut self) -> Result<u64> {
        Err(Error::Internal("alloc through a read transaction".into()))
    }
    fn free(&mut self, _id: u64) -> Result<()> {
        Err(Error::Internal("free through a read transaction".into()))
    }
    fn is_dirty(&self, _id: u64) -> bool {
        false
    }
}

impl ReadTxn<'_> {
    /// Confirm the snapshot is still protected (long scans call this).
    pub fn still_pinned(&self) -> bool {
        self.eng.shm.slot_still_owned(self.slot, self.word)
    }

    pub fn finish(mut self) -> Result<()> {
        self.released = true;
        if self.eng.shm.release_slot(self.slot, self.word) {
            Ok(())
        } else {
            Err(Error::SnapshotEvicted)
        }
    }

    fn tree_root(&self, table_id: u32, index_no: u32) -> Result<u64> {
        catalog_entry(self, self.meta.catalog_root, table_id, index_no).map(|(r, _)| r)
    }

    pub fn row_count(&self, table_id: u32) -> Result<u64> {
        catalog_entry(self, self.meta.catalog_root, table_id, 0).map(|(_, c)| c)
    }

    pub fn get_by_pk(&self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        let key = keycode::encode_key(pk_values);
        let root = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.eng.col_types[table_id as usize],
            )?)),
        }
    }

    /// Point probe of a secondary unique index; returns the full row.
    pub fn get_by_index(
        &self,
        table_id: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Option<Vec<Value>>> {
        let ikey = keycode::encode_key(std::slice::from_ref(value));
        let iroot = self.tree_root(table_id, index_no)?;
        let Some(pk_bytes) = btree::get(self, iroot, &ikey)? else {
            return Ok(None);
        };
        let root = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &pk_bytes)? {
            None => Err(Error::Corrupt(
                "index entry points at a missing row".into(),
            )),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.eng.col_types[table_id as usize],
            )?)),
        }
    }

    /// All rows whose `index_no` column equals `value` — the index equality
    /// lookup (`WHERE col = value`). Works for a UNIQUE index too (0 or 1
    /// rows; those take the exact-get fast path below). The index tree is
    /// keyed by `(value ‖ pk)` for a non-unique index and by `value` alone
    /// for a unique one; both start with `encode_key([value])`, so scanning
    /// from that prefix and stopping when the prefix no longer matches yields
    /// exactly the matches — O(matches + 1).
    pub fn scan_by_index(
        &self,
        table_id: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Vec<Vec<Value>>> {
        if value.is_null() {
            return Ok(Vec::new()); // NULL is never indexed
        }
        let unique = index_no >= 1
            && self
                .eng
                .sec_unique
                .get(table_id as usize)
                .and_then(|v| v.get(index_no as usize - 1))
                .copied()
                .unwrap_or(false);
        if unique {
            return Ok(self.get_by_index(table_id, index_no, value)?.into_iter().collect());
        }
        let prefix = keycode::encode_key(std::slice::from_ref(value));
        let iroot = self.tree_root(table_id, index_no)?;
        let root = self.tree_root(table_id, 0)?;
        let types = &self.eng.col_types[table_id as usize];
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, Some((&prefix[..], true)), None)?;
        while let Some((k, pk_bytes)) = c.next(self)? {
            if !k.starts_with(&prefix) {
                break; // past every (value, *) entry
            }
            match btree::get(self, root, &pk_bytes)? {
                Some(bytes) => out.push(row::decode_row(&bytes, types)?),
                None => {
                    return Err(Error::Corrupt("index entry points at a missing row".into()))
                }
            }
        }
        Ok(out)
    }

    pub fn scan(
        &self,
        table_id: u32,
        lo: Option<(&[Value], bool)>,
        hi: Option<(&[Value], bool)>,
    ) -> Result<RowCursor<'_, '_>> {
        let root = self.tree_root(table_id, 0)?;
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key(v), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key(v), inc));
        let cursor = btree::cursor(
            self,
            root,
            lo_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            hi_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        )?;
        Ok(RowCursor {
            txn: self,
            cursor,
            table_id,
            steps: 0,
        })
    }

    /// Range scan with raw encoded-key bounds (the SQL executor needs prefix
    /// semantics on composite PKs that value-level bounds cannot express).
    pub fn scan_raw(
        &self,
        table_id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<RowCursor<'_, '_>> {
        let root = self.tree_root(table_id, 0)?;
        let cursor = btree::cursor(self, root, lo, hi)?;
        Ok(RowCursor {
            txn: self,
            cursor,
            table_id,
            steps: 0,
        })
    }

    /// Read a system record (reserved catalog keyspace).
    pub fn sys_get(&self, subkey: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(self, self.meta.catalog_root, &sys_key(subkey))
    }

    /// All system records, subkeys with the reserved prefix stripped.
    pub fn sys_scan(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let lo = [SYS_PREFIX];
        let hi = [SYS_PREFIX + 1];
        let mut c = btree::cursor(self, self.meta.catalog_root, Some((&lo, true)), Some((&hi, false)))?;
        let mut out = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            out.push((k[1..].to_vec(), v));
        }
        Ok(out)
    }

    /// System records whose subkey is in `[lo, hi)` (both given without the
    /// reserved prefix, which is added internally). Prefix-bounded so a caller
    /// can walk one family (e.g. a CDC dirty-set `d/<table>…`) in O(matches)
    /// rather than scanning the whole sys region and filtering.
    pub fn sys_scan_range(&self, lo: &[u8], hi: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let klo = sys_key(lo);
        let khi = sys_key(hi);
        let mut c = btree::cursor(
            self,
            self.meta.catalog_root,
            Some((&klo, true)),
            Some((&khi, false)),
        )?;
        let mut out = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            out.push((k[1..].to_vec(), v));
        }
        Ok(out)
    }

    /// This snapshot's commit id (the monotone per-file txn counter). Used as
    /// the push high-water `H` in the mirror protocol (DESIGN-MIRROR §6).
    pub fn txn_id(&self) -> u64 {
        self.meta.txn_id
    }

    /// The canonical schema stored inside the database at init.
    pub fn stored_schema(&self) -> Result<Schema> {
        let bytes = btree::get(self, self.meta.catalog_root, CAT_SCHEMA_KEY)?
            .ok_or_else(|| Error::Corrupt("no schema stored in catalog".into()))?;
        Schema::from_canonical_bytes(&bytes)
    }
}

impl Drop for ReadTxn<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.eng.shm.release_slot(self.slot, self.word);
        }
    }
}

/// Forward row cursor. Periodically re-validates the snapshot pin so a
/// (future) max-pin-age eviction surfaces as `SnapshotEvicted`, never as
/// silently corrupt rows.
pub struct RowCursor<'t, 'e> {
    txn: &'t ReadTxn<'e>,
    cursor: btree::Cursor,
    table_id: u32,
    steps: u32,
}

impl RowCursor<'_, '_> {
    // fallible + streaming, so deliberately not std::iter::Iterator
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Vec<Value>>> {
        self.steps += 1;
        if self.steps.is_multiple_of(256) && !self.txn.still_pinned() {
            return Err(Error::SnapshotEvicted);
        }
        match self.cursor.next(self.txn)? {
            None => Ok(None),
            Some((_k, v)) => Ok(Some(row::decode_row(
                &v,
                &self.txn.eng.col_types[self.table_id as usize],
            )?)),
        }
    }
}

fn catalog_entry<S: PageStore + ?Sized>(
    store: &S,
    catalog_root: u64,
    table_id: u32,
    index_no: u32,
) -> Result<(u64, u64)> {
    let bytes = btree::get(store, catalog_root, &cat_tree_key(table_id, index_no))?
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "missing catalog entry for table {table_id} index {index_no}"
            ))
        })?;
    if bytes.len() != 16 {
        return Err(Error::Corrupt("bad catalog entry size".into()));
    }
    Ok((
        u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
    ))
}

// --------------------------------------------------------------- WriteTxn

/// The pages this transaction has COWed, so `page_mut` can tell "already mine"
/// from "needs copying" in O(1).
///
/// The hasher is the point. `HashSet<u64>` defaults to SipHash-1-3, built to
/// survive an adversary choosing keys — and these keys are page ids this
/// process just allocated. A CPU profile of a bulk write on armv7 put ~15% of
/// the run inside `DefaultHasher`, because `page_mut` hashes on EVERY call and
/// one row touches several pages.
///
/// fxhash's multiply-rotate instead. Measured, paired, alternating arms:
/// **+3.5% on armv7** (95% CI [+2.1, +4.9], n=15) and **nothing measurable on
/// x86-64** (-0.1%, CI [-2.2, +1.9], n=25). Free on the reference platform,
/// real on a supported one.
///
/// It still spreads the low bits, which matters because hashbrown takes its
/// control byte from the TOP 7: a pass-through hash would put dense sequential
/// ids in one control-byte class and turn every probe into a linear scan — the
/// obvious "optimization" that is slower.
type DirtySet = HashSet<u64, BuildHasherDefault<PageIdHasher>>;

#[derive(Default)]
pub(crate) struct PageIdHasher(u64);

impl Hasher for PageIdHasher {
    fn write(&mut self, bytes: &[u8]) {
        // Only ever hashes u64 page ids; a byte-slice key means someone reused
        // this for something it was never measured for.
        debug_assert!(
            false,
            "PageIdHasher is for u64 page ids, got {} bytes",
            bytes.len()
        );
        for b in bytes {
            self.write_u64(*b as u64);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n)
            .wrapping_mul(0x517c_c1b7_2722_0a95)
            .rotate_left(26);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

pub struct WriteTxn<'e> {
    eng: &'e Engine,
    pub meta: MetaSnapshot,
    catalog_root: u64,
    freelist_root: u64,
    high_water: u64,
    /// (table_id, index_no) → (root, row_count); loaded lazily, written back
    /// into the catalog at commit.
    table_roots: HashMap<(u32, u32), (u64, u64)>,
    dirty: DirtySet,
    /// Pages this txn may allocate. They are **still listed in the freelist**:
    /// `refill_reusable` reads entries, it does not remove them (DESIGN.md
    /// §4.5). `taken` remembers where each came from so the commit fixpoint can
    /// strike out exactly the ones that got consumed.
    reusable: Vec<u64>,
    /// Freelist entries drawn from, in draw order. Drawing is FREE — an entry
    /// nobody allocated out of is left untouched at commit. Only consumption
    /// costs a write. That decoupling is the fix for #37.
    taken: Vec<TakenEntry>,
    /// Last key `refill_reusable` drew from; the next draw starts strictly
    /// after it, so an entry is never drawn twice (it is still in the tree).
    refill_cursor: Option<[u8; 10]>,
    freed: BTreeSet<u64>,
    bound_recomputed: bool,
    /// True while a mutation of the freelist tree itself is in progress.
    /// `alloc` must NOT trigger `refill_reusable` then: the refill deletes a
    /// freelist entry via `btree::delete` on the same tree the in-progress
    /// mutation is rewriting — two interleaved mutations with different root
    /// snapshots lose updates and hand out live pages (double allocation,
    /// "double free"/"listed twice" corruption seen in multi-process stress).
    in_freelist_op: bool,
    /// Robust-mutex recovery ran when this txn acquired the lock.
    pub recovered: bool,
    finished: bool,
    /// Bitmap of user tables whose data this txn mutated (set in
    /// `set_tree_root`). Recorded into the committed-footprint ring at commit
    /// in optimistic mode; unused (and free) in serial mode.
    written_tables: u64,
    /// Set by the optimistic blind-apply path to record a precise
    /// (table, key_hash) point footprint at commit instead of a table-level
    /// one. `None` for every other path.
    commit_point: Option<(u32, u64)>,
    /// CDC dirty-set capture is on for this txn (default). The replication
    /// plane (mirror applier/importer) turns it OFF via [`WriteTxn::set_capture`]
    /// so its own writes are not self-captured (DESIGN-MIRROR §3.8). Transient:
    /// never persisted, dies with the txn.
    capture_enabled: bool,
    /// Lazily-loaded `cdc\0tabs` control record, cached for the txn's lifetime
    /// (capture enablement is set in a separate txn, so it is stable here).
    capture_cfg: Option<CaptureConfig>,
    /// This txn may allocate from the reserved control-page band (§3.10). Set by
    /// the mirror for control-only commits; default off.
    reserved_alloc: bool,
}

impl PageStore for WriteTxn<'_> {
    fn page(&self, id: u64) -> Result<&[u8]> {
        self.eng.shm.page(id)
    }

    fn page_mut(&mut self, id: u64) -> Result<&mut [u8]> {
        // COW discipline enforced in production, not just tests: committed
        // pages are immutable while readers may hold snapshots.
        if !self.dirty.contains(&id) {
            return Err(Error::Internal(format!(
                "page_mut on non-dirty page {id} (COW violation)"
            )));
        }
        self.eng.shm.page_mut_unchecked(id)
    }

    fn alloc(&mut self) -> Result<u64> {
        let id = self.alloc_id()?;
        self.dirty.insert(id);
        self.eng.shm.page_mut_unchecked(id)?.fill(0);
        Ok(id)
    }

    fn alloc_raw(&mut self) -> Result<u64> {
        // Identical to `alloc` minus the full-page fill(0). Split rather than
        // making `alloc` lazy: every other caller (btree nodes) relies on the
        // zeroed contract, and quietly weakening it for all of them to speed up
        // one path is how a subtle corruption gets introduced.
        let id = self.alloc_id()?;
        self.dirty.insert(id);
        Ok(id)
    }

    fn free(&mut self, id: u64) -> Result<()> {
        if self.dirty.remove(&id) {
            // allocated this txn: immediately reusable, invisible to readers.
            // Sorted insert — `reusable` is kept ordered so `freelist_plan` can
            // test membership by binary search instead of building a HashSet
            // per fixpoint pass (#37: that set was 14% of the write path).
            if let Err(at) = self.reusable.binary_search(&id) {
                self.reusable.insert(at, id);
            }
            return Ok(());
        }
        if !self.freed.insert(id) {
            return Err(Error::Internal(format!("double free of page {id}")));
        }
        Ok(())
    }

    fn is_dirty(&self, id: u64) -> bool {
        self.dirty.contains(&id)
    }
}

impl WriteTxn<'_> {
    /// Pick the next page id (freelist-reuse first, then high-water), without
    /// touching its contents. Shared by `alloc` and `alloc_raw`.
    fn alloc_id(&mut self) -> Result<u64> {
        // Draw a POOL, not one page's worth. Drawing is read-only and costs
        // nothing at commit unless the pages get consumed (see `taken`), so a
        // deep pool is free — and it is what keeps the fixpoint below from
        // running dry and minting high-water pages (#37).
        //
        // Still not during a freelist op: refill reads the tree with a cursor,
        // and mid-`btree::insert` `freelist_root` still points at the old,
        // intact tree — consistent to read, but the entry it would draw from
        // may be one the in-progress mutation is about to rewrite.
        if self.reusable.is_empty() && !self.in_freelist_op {
            // Only when DRY: `alloc_id` runs several times per txn, and topping
            // the pool up on each one would cost a tree descent per allocation.
            for _ in 0..FREELIST_POOL_DRAWS {
                if self.reusable.len() >= FREELIST_POOL_TARGET || !self.refill_reusable()? {
                    break;
                }
            }
        }
        let id = match self.reusable.pop() {
            Some(id) => {
                leakstat::bump(&leakstat::ALLOC_REUSABLE);
                id
            }
            None => {
                leakstat::bump(&leakstat::ALLOC_HW);
                if self.in_freelist_op {
                    leakstat::bump(&leakstat::ALLOC_HW_IN_FL);
                }
                // The top RESERVED_CONTROL_PAGES of the file are dispensed only
                // to `reserved_alloc` txns, so the mirror's control-plane writes
                // (HALTED/frozen/cursor/park markers) can still commit when the
                // data region is otherwise full (DESIGN-MIRROR §3.10). Data and
                // CDC capture use the normal ceiling and hit DbFull first.
                let ceiling = if self.reserved_alloc {
                    self.eng.shm.page_count
                } else {
                    self.eng
                        .shm
                        .page_count
                        .saturating_sub(RESERVED_CONTROL_PAGES)
                };
                if self.high_water >= ceiling {
                    return Err(Error::DbFull);
                }
                let id = self.high_water;
                self.high_water += 1;
                id
            }
        };
        Ok(id)
    }
}


impl<'e> WriteTxn<'e> {
    /// Pull one reclaimable freelist entry into `reusable`.
    ///
    /// Reusable iff its freeing txn is **at or below** the oldest-pinned bound.
    /// Not *strictly* below: that off-by-one is the one CLAUDE.md's invariant
    /// list calls out as causing an unbounded high-water leak, and this comment
    /// used to describe it — the code has always been right.
    ///
    /// ⚠ There is a *different* unbounded-high-water bug reachable from here
    /// under sustained concurrent churn; see
    /// `tests/high_water_leak.rs`.
    /// What this commit's freelist writes should be: `(key, ids)` per entry,
    /// where an EMPTY id list means "delete this key". Keys absent from the
    /// result are to be left exactly as they are.
    ///
    /// Two sources:
    ///
    /// 1. Every entry `refill_reusable` drew from, minus the pages that got
    ///    consumed. An entry nothing was allocated out of is **omitted** — it
    ///    still holds precisely what it held, so writing it would be pure churn.
    ///    That omission is what makes drawing a deep pool free, and it is the
    ///    whole fix for #37.
    /// 2. This txn's own free set, under `new_txn`: pages COWed away, plus any
    ///    page left in `reusable` that no drawn entry lists (this txn allocated
    ///    it from the high-water mark, or out of an entry it then drew dry, and
    ///    freed it again — nothing else records it, so omitting it would leak
    ///    it outright).
    fn freelist_plan(&self, new_txn: u64, written: &[([u8; 10], Vec<u64>)]) -> Vec<([u8; 10], Vec<u64>)> {
        // `reusable` is sorted (see `free`/`refill_reusable`), so "is this page
        // still free?" is a binary search — no per-pass set to build.
        let mut out: Vec<([u8; 10], Vec<u64>)> = Vec::with_capacity(self.taken.len() + 1);
        for e in &self.taken {
            let kept: Vec<u64> = e
                .ids
                .iter()
                .copied()
                .filter(|id| self.reusable.binary_search(id).is_ok())
                .collect();
            // Once a pass has rewritten an entry it must stay in the plan
            // FOREVER, even if a later pass frees every page back and it looks
            // untouched again. Dropping it here would make the reconcile pass
            // below see a key in `written` that the plan no longer claims, and
            // delete it — with its pages listed nowhere. The page-accounting
            // verifier catches that as "page N leaked: neither reachable nor
            // freelisted", which is exactly how this was found.
            if kept.len() != e.ids.len() || written.iter().any(|(k, _)| *k == e.key) {
                out.push((e.key, kept)); // shrunk, or emptied = delete
            }
        }
        // A reusable page that some drawn entry still lists is accounted for by
        // that entry; everything else in the pool is this txn's own.
        //
        // freed and reusable are disjoint by construction — `free` routes a page
        // to exactly one of them, by whether this txn allocated it.
        let taken = &self.taken;
        let mut own: Vec<u64> = self.freed.iter().copied().collect();
        own.extend(
            self.reusable
                .iter()
                .copied()
                .filter(|id| !taken.iter().any(|e| e.ids.binary_search(id).is_ok())),
        );
        own.sort_unstable();
        debug_assert!(own.windows(2).all(|w| w[0] < w[1]), "own must be strictly ascending");
        for (i, chunk) in own.chunks(FREELIST_CHUNK_PAGES).enumerate() {
            out.push((freelist_key(new_txn, i as u16), chunk.to_vec()));
        }
        out
    }

    /// Draw one freelist entry's pages into `reusable` — **without removing the
    /// entry**. Returns whether an entry was drawn.
    ///
    /// Read-only, and that is the whole point (DESIGN.md §4.5). It used to
    /// `btree::delete` the entry, which made every page drawn a page the commit
    /// fixpoint had to write back (it records what is free, and a drawn page is
    /// listed nowhere else). That coupled the fixpoint's own page appetite to
    /// the pool it was handed: feeding it made it hungrier, which is why two
    /// separate attempts to feed it made #37 strictly worse. Leaving the entry
    /// in place decouples them — an entry nobody allocates out of costs nothing
    /// at commit.
    fn refill_reusable(&mut self) -> Result<bool> {
        debug_assert!(!self.in_freelist_op, "refill re-entered a freelist op");
        leakstat::bump(&leakstat::REFILL_CALLS);
        if self.freelist_root == 0 {
            leakstat::bump(&leakstat::REFILL_NO_TREE);
            return Ok(false);
        }
        // Start strictly after the last entry drawn: it is still in the tree,
        // so an inclusive scan would hand out its pages a second time.
        let lo = self.refill_cursor;
        let mut c = btree::cursor(
            self,
            self.freelist_root,
            lo.as_ref().map(|k| (k.as_slice(), false)),
            None,
        )?;
        let Some((key, val)) = c.next(self)? else {
            leakstat::bump(&leakstat::REFILL_NO_TREE);
            return Ok(false);
        };
        if key.len() != 10 || val.len() % 8 != 0 {
            return Err(Error::Corrupt("bad freelist entry".into()));
        }
        let freed_txn = u64::from_be_bytes(key[..8].try_into().unwrap());
        // Pages freed BY commit T are referenced only by snapshots < T (commit
        // T is what replaced them), so they are reusable iff T <= oldest pin.
        let mut bound = self
            .eng
            .shm
            .oldest_pinned_cache()
            .load(std::sync::atomic::Ordering::Acquire);
        if freed_txn > bound && !self.bound_recomputed {
            // the cached bound is stale-conservative; recompute once per txn
            self.bound_recomputed = true;
            leakstat::bump(&leakstat::RECOMPUTES);
            bound = self.eng.shm.compute_oldest_pinned(self.meta.txn_id);
        }
        if freed_txn > bound {
            // Entries are keyed by freeing txn, so this one is the oldest
            // drawable and nothing behind it can be older. Stop.
            leakstat::bump(&leakstat::REFILL_NOT_YET);
            return Ok(false);
        }
        // Validate every id: corrupt freelist bytes must never let alloc
        // zero-fill meta/lock/reader pages (page_mut_unchecked only
        // bounds-checks the upper end).
        let mut ids = Vec::with_capacity(val.len() / 8);
        for chunk in val.chunks_exact(8) {
            let id = u64::from_le_bytes(chunk.try_into().unwrap());
            if id < self.eng.shm.data_start || id >= self.eng.shm.page_count {
                return Err(Error::Corrupt(format!(
                    "freelist lists page {id} outside the data region"
                )));
            }
            // Entries are written sorted, and `freelist_plan` binary-searches
            // them. Enforce it rather than assume it: an unsorted value would
            // silently mis-answer "is this page still listed here?", which
            // double-allocates or leaks instead of failing.
            if ids.last().is_some_and(|&prev| prev >= id) {
                return Err(Error::Corrupt(
                    "freelist entry is not strictly ascending".into(),
                ));
            }
            ids.push(id);
        }
        leakstat::bump(&leakstat::REFILL_OK);
        leakstat::add(&leakstat::REFILL_PAGES, ids.len() as u64);
        let key: [u8; 10] = key.try_into().expect("checked len == 10 above");
        self.reusable.extend(ids.iter().copied());
        self.reusable.sort_unstable();
        self.taken.push(TakenEntry { key, ids });
        self.refill_cursor = Some(key);
        Ok(true)
    }

    fn tree_root(&mut self, table_id: u32, index_no: u32) -> Result<(u64, u64)> {
        if let Some(&e) = self.table_roots.get(&(table_id, index_no)) {
            return Ok(e);
        }
        let e = catalog_entry(self, self.catalog_root, table_id, index_no)?;
        self.table_roots.insert((table_id, index_no), e);
        Ok(e)
    }

    fn set_tree_root(&mut self, table_id: u32, index_no: u32, root: u64, count: u64) {
        self.written_tables |= 1u64 << (table_id & 63);
        self.table_roots.insert((table_id, index_no), (root, count));
    }

    // ---------- optimistic concurrency (DESIGN-PHASE3) ----------

    /// First-committer-wins validation for an optimistic write of
    /// `(table_id, key_hash)` prepared against snapshot `snap_txn`. Must be
    /// called while holding the writer lock (i.e. on a live `WriteTxn`), before
    /// applying. Returns `Error::WriteConflict` if a conflicting commit landed
    /// on our footprint since the snapshot.
    pub fn optimistic_validate(
        &self,
        snap_txn: u64,
        table_id: u32,
        key_hash: u64,
    ) -> Result<()> {
        if self
            .eng
            .shm
            .opt_conflict(snap_txn, self.meta.txn_id, table_id, key_hash)
        {
            return Err(Error::WriteConflict);
        }
        Ok(())
    }

    /// Record a precise point footprint for this commit (blind-apply path).
    pub fn set_commit_point(&mut self, table_id: u32, key_hash: u64) {
        self.commit_point = Some((table_id, key_hash));
    }

    /// Blind INSERT of a pre-validated, pre-encoded row (optimistic apply).
    /// Caller guarantees `table_id` has no secondary index and the row was
    /// validated during prep; footprint validation guarantees the PK's state
    /// is unchanged since the snapshot. Returns false if the PK already exists.
    pub fn optimistic_insert(
        &mut self,
        table_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<bool> {
        debug_assert!(self.eng.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::insert(self, root, key, &mut btree::Payload::Flat(payload), InsertMode::InsertOnly)?;
        if out.existed {
            return Ok(false);
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);
        self.capture_dirty(table_id, key, DirtyOp::Upsert)?;
        Ok(true)
    }

    /// Blind UPDATE (replace) of an existing PK with a pre-validated row.
    pub fn optimistic_upsert(
        &mut self,
        table_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        debug_assert!(self.eng.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::insert(self, root, key, &mut btree::Payload::Flat(payload), InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);
        self.capture_dirty(table_id, key, DirtyOp::Upsert)?;
        Ok(())
    }

    /// Blind DELETE of a PK. Returns whether the row existed.
    pub fn optimistic_delete(&mut self, table_id: u32, key: &[u8]) -> Result<bool> {
        debug_assert!(self.eng.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::delete(self, root, key)?;
        if out.existed {
            self.set_tree_root(table_id, 0, out.new_root, count - 1);
            self.capture_dirty(table_id, key, DirtyOp::Delete)?;
        }
        Ok(out.existed)
    }

    // ---------- typed row API ----------

    /// Insert a row whose column `stream_col` is pulled from `src` instead of
    /// being handed over as a `Value` (#43). Nothing large is ever resident: the
    /// engine asks `src` for a page at a time as it writes.
    ///
    /// Pass `Value::Blob(vec![])` (or `Text("")`) in `values` at `stream_col` —
    /// it is a placeholder for the type check; its length comes from
    /// `src.len()`. The streamed column must be the row's LAST variable-length
    /// column (`row::encode_row_head_for_stream` enforces it).
    ///
    /// The lock is held for the duration, and `src` is called with it held —
    /// which is exactly why this pulls rather than handing out a writer: a slow
    /// source blocks every other writer either way, but here that window is the
    /// engine's to bound rather than the caller's to forget.
    pub fn insert_row_streaming(
        &mut self,
        table_id: u32,
        values: &[Value],
        stream_col: usize,
        src: &mut dyn btree::BlobSource,
    ) -> Result<()> {
        self.check_write_blocked(table_id)?;
        self.eng.validate_row(table_id, values)?;
        let table = self.eng.table(table_id)?;
        let tname = table.name.clone();
        if !self.eng.sec_indexes[table_id as usize].is_empty() {
            // A UNIQUE probe needs the value, and the whole point here is that
            // nobody has it. Refuse rather than half-check.
            return Err(Error::Unsupported(
                "streaming insert into a table with a secondary UNIQUE index".into(),
            ));
        }
        let key = self.eng.pk_key(table_id, values)?;
        let types = &self.eng.col_types[table_id as usize];
        let (head, _total) =
            row::encode_row_head_for_stream(values, types, stream_col, src.len())?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let mut payload = btree::Payload::Stream { head: &head, src };
        let out = btree::insert(self, root, &key, &mut payload, InsertMode::InsertOnly)?;
        if out.existed {
            return Err(Error::PrimaryKeyViolation { table: tname });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);
        Ok(())
    }

    pub fn insert_row(&mut self, table_id: u32, values: &[Value]) -> Result<()> {
        self.check_write_blocked(table_id)?;
        let __t = std::time::Instant::now();
        self.eng.validate_row(table_id, values)?;
        leakstat::add(&leakstat::INS_NS_VALIDATE, __t.elapsed().as_nanos() as u64);
        let table = self.eng.table(table_id)?;
        let tname = table.name.clone();
        let sec = self.eng.sec_indexes[table_id as usize].clone();
        let key = self.eng.pk_key(table_id, values)?;
        // #42: for a row that will SPILL, hand btree the parts instead of a
        // buffer. `encode_row` materialises the whole row — a large blob included
        // — into a fresh heap Vec whose only purpose is to be copied straight
        // back out into overflow pages; that measured 10.1 ms of a 23.5 ms 16 MiB
        // insert (~42%), because a fresh malloc faults its anonymous pages just
        // like the file mapping does.
        //
        // Switch on size, and take the size BEFORE encoding: an inline row's leaf
        // cell has to be contiguous anyway, so the parts form buys it nothing and
        // its slice-of-slices would be pure overhead on the hot path. Small rows
        // therefore take EXACTLY the old code path — no regression by
        // construction rather than by measurement, which matters here because
        // this box cannot resolve a few percent without ~50 paired runs.
        let __t = std::time::Instant::now();
        let types = &self.eng.col_types[table_id as usize];
        let spills = row::encoded_len(values, types) > btree::MAX_INLINE_VAL;
        let flat = if spills { None } else { Some(row::encode_row(values, types)?) };
        let parts = if spills {
            Some(row::encode_row_parts(values, types)?)
        } else {
            None
        };
        let pieces: Vec<&[u8]> = match &parts {
            Some((head, bodies)) => std::iter::once(head.as_slice())
                .chain(bodies.iter().copied())
                .collect(),
            None => Vec::new(),
        };
        let mut payload = match &flat {
            Some(b) => btree::Payload::Flat(b),
            None => btree::Payload::Parts(&pieces),
        };
        leakstat::add(&leakstat::INS_NS_ENCODE, __t.elapsed().as_nanos() as u64);

        // UNIQUE pre-check on secondary indexes before mutating anything, so
        // a violation aborts with zero side effects on the dirty state. Only
        // UNIQUE indexes are checked — a plain `indexed` column allows dups.
        let sec_unique = self.eng.sec_unique[table_id as usize].clone();
        for (i, &col) in sec.iter().enumerate() {
            if !sec_unique[i] {
                continue;
            }
            let v = &values[col as usize];
            if v.is_null() {
                continue; // SQL: UNIQUE permits multiple NULLs
            }
            let ino = (i + 1) as u32;
            let (iroot, _) = self.tree_root(table_id, ino)?;
            let ikey = keycode::encode_key(std::slice::from_ref(v));
            if btree::get(self, iroot, &ikey)?.is_some() {
                return Err(Error::UniqueViolation {
                    table: tname,
                    constraint: table_column_name(self.eng, table_id, col),
                });
            }
        }

        let (root, count) = self.tree_root(table_id, 0)?;
        let __t = std::time::Instant::now();
        let out = btree::insert(self, root, &key, &mut payload, InsertMode::InsertOnly)?;
        leakstat::add(&leakstat::INS_NS_BTREE, __t.elapsed().as_nanos() as u64);
        if out.existed {
            return Err(Error::PrimaryKeyViolation { table: tname });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);

        for (i, &col) in sec.iter().enumerate() {
            let v = &values[col as usize];
            if v.is_null() {
                continue;
            }
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            // UNIQUE: key is the value alone (value→pk). Non-unique: the value
            // may repeat, so the key is `(value ‖ pk)` — unique by construction,
            // duplicates on the value coexist as distinct index entries. Both
            // store the pk as the payload so a lookup fetches the row.
            let ikey = index_ikey(sec_unique[i], v, &key);
            let out = btree::insert(self, iroot, &ikey, &mut btree::Payload::Flat(&key), InsertMode::InsertOnly)?;
            if out.existed {
                // pre-check passed (unique) / composite is unique (non-unique),
                // so a collision is engine inconsistency.
                return Err(Error::Internal("secondary index collision within txn".into()));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount + 1);
        }
        self.capture_dirty(table_id, &key, DirtyOp::Upsert)?;
        Ok(())
    }

    pub fn get_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        let key = keycode::encode_key(pk_values);
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.eng.col_types[table_id as usize],
            )?)),
        }
    }

    /// Delete by primary key; returns whether the row existed.
    pub fn delete_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<bool> {
        self.check_write_blocked(table_id)?;
        let key = keycode::encode_key(pk_values);
        let (root, count) = self.tree_root(table_id, 0)?;
        // fetch old row first: index maintenance needs its column values
        let sec_unique = self.eng.sec_unique[table_id as usize].clone();
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.eng.col_types[table_id as usize])?;
        let out = btree::delete(self, root, &key)?;
        debug_assert!(out.existed);
        self.set_tree_root(table_id, 0, out.new_root, count - 1);

        let sec = self.eng.sec_indexes[table_id as usize].clone();
        for (i, &col) in sec.iter().enumerate() {
            let v = &old[col as usize];
            if v.is_null() {
                continue;
            }
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let ikey = index_ikey(sec_unique[i], v, &key);
            let out = btree::delete(self, iroot, &ikey)?;
            if !out.existed {
                return Err(Error::Corrupt("missing index entry on delete".into()));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount - 1);
        }
        self.capture_dirty(table_id, &key, DirtyOp::Delete)?;
        Ok(true)
    }

    /// Replace the row with the given PK. PK columns must be unchanged
    /// (enforced; the SQL layer rejects PK updates at bind time too).
    pub fn update_by_pk(&mut self, table_id: u32, new_values: &[Value]) -> Result<bool> {
        self.check_write_blocked(table_id)?;
        self.eng.validate_row(table_id, new_values)?;
        let table = self.eng.table(table_id)?;
        let tname = table.name.clone();
        let key = self.eng.pk_key(table_id, new_values)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.eng.col_types[table_id as usize])?;

        let sec = self.eng.sec_indexes[table_id as usize].clone();
        let sec_unique = self.eng.sec_unique[table_id as usize].clone();
        // pre-check UNIQUE conflicts for changed unique-indexed columns
        for (i, &col) in sec.iter().enumerate() {
            if !sec_unique[i] {
                continue;
            }
            let (ov, nv) = (&old[col as usize], &new_values[col as usize]);
            if index_value_equal(ov, nv) || nv.is_null() {
                continue;
            }
            let ino = (i + 1) as u32;
            let (iroot, _) = self.tree_root(table_id, ino)?;
            let ikey = keycode::encode_key(std::slice::from_ref(nv));
            if btree::get(self, iroot, &ikey)?.is_some() {
                return Err(Error::UniqueViolation {
                    table: tname.clone(),
                    constraint: table_column_name(self.eng, table_id, col),
                });
            }
        }

        let payload = row::encode_row(new_values, &self.eng.col_types[table_id as usize])?;
        let out = btree::insert(self, root, &key, &mut btree::Payload::Flat(&payload), InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);

        for (i, &col) in sec.iter().enumerate() {
            let (ov, nv) = (&old[col as usize], &new_values[col as usize]);
            if index_value_equal(ov, nv) {
                continue;
            }
            let ino = (i + 1) as u32;
            let (mut iroot, mut icount) = self.tree_root(table_id, ino)?;
            if !ov.is_null() {
                let okey = index_ikey(sec_unique[i], ov, &key);
                let out = btree::delete(self, iroot, &okey)?;
                if !out.existed {
                    return Err(Error::Corrupt("missing index entry on update".into()));
                }
                iroot = out.new_root;
                icount -= 1;
            }
            if !nv.is_null() {
                let nkey = index_ikey(sec_unique[i], nv, &key);
                let out = btree::insert(self, iroot, &nkey, &mut btree::Payload::Flat(&key), InsertMode::InsertOnly)?;
                if out.existed {
                    return Err(Error::Internal("secondary index collision within txn".into()));
                }
                iroot = out.new_root;
                icount += 1;
            }
            self.set_tree_root(table_id, ino, iroot, icount);
        }
        self.capture_dirty(table_id, &key, DirtyOp::Upsert)?;
        Ok(true)
    }

    /// Collect PKs of rows in a PK range (scan step of UPDATE/DELETE plans;
    /// collect-then-mutate keeps cursors and mutations from aliasing).
    pub fn collect_pk_range(
        &mut self,
        table_id: u32,
        lo: Option<(&[Value], bool)>,
        hi: Option<(&[Value], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (root, _) = self.tree_root(table_id, 0)?;
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key(v), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key(v), inc));
        let mut c = btree::cursor(
            self,
            root,
            lo_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            hi_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        )?;
        let table = self.eng.table(table_id)?;
        let pk_types: Vec<ColumnType> = table.pk_types();
        let mut out = Vec::new();
        while let Some((k, _)) = c.next(self)? {
            out.push(keycode::decode_key(&k, &pk_types)?);
        }
        Ok(out)
    }

    /// Scan full rows within the writer's own (uncommitted) view.
    pub fn scan_rows(
        &mut self,
        table_id: u32,
        lo: Option<(&[Value], bool)>,
        hi: Option<(&[Value], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (root, _) = self.tree_root(table_id, 0)?;
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key(v), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key(v), inc));
        let mut c = btree::cursor(
            self,
            root,
            lo_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            hi_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        )?;
        let mut out = Vec::new();
        while let Some((_k, v)) = c.next(self)? {
            out.push(row::decode_row(&v, &self.eng.col_types[table_id as usize])?);
        }
        Ok(out)
    }

    pub fn row_count(&mut self, table_id: u32) -> Result<u64> {
        self.tree_root(table_id, 0).map(|(_, c)| c)
    }

    /// Scan full rows with raw encoded-key bounds within the writer's view.
    pub fn scan_rows_raw(
        &mut self,
        table_id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (root, _) = self.tree_root(table_id, 0)?;
        let mut c = btree::cursor(self, root, lo, hi)?;
        let mut out = Vec::new();
        while let Some((_k, v)) = c.next(self)? {
            out.push(row::decode_row(&v, &self.eng.col_types[table_id as usize])?);
        }
        Ok(out)
    }

    /// Point probe of a secondary unique index within the writer's view.
    pub fn get_by_index(
        &mut self,
        table_id: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Option<Vec<Value>>> {
        let ikey = keycode::encode_key(std::slice::from_ref(value));
        let (iroot, _) = self.tree_root(table_id, index_no)?;
        let Some(pk_bytes) = btree::get(self, iroot, &ikey)? else {
            return Ok(None);
        };
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &pk_bytes)? {
            None => Err(Error::Corrupt("index entry points at a missing row".into())),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.eng.col_types[table_id as usize],
            )?)),
        }
    }

    /// All rows whose `index_no` column equals `value`, within the writer's
    /// view — the write-side twin of [`ReadTxn::scan_by_index`], with the same
    /// unique fast path and `(value ‖ pk)` prefix-scan contract.
    pub fn scan_by_index(
        &mut self,
        table_id: u32,
        index_no: u32,
        value: &Value,
    ) -> Result<Vec<Vec<Value>>> {
        if value.is_null() {
            return Ok(Vec::new()); // NULL is never indexed
        }
        let unique = index_no >= 1
            && self
                .eng
                .sec_unique
                .get(table_id as usize)
                .and_then(|v| v.get(index_no as usize - 1))
                .copied()
                .unwrap_or(false);
        if unique {
            return Ok(self.get_by_index(table_id, index_no, value)?.into_iter().collect());
        }
        let prefix = keycode::encode_key(std::slice::from_ref(value));
        let (iroot, _) = self.tree_root(table_id, index_no)?;
        let (root, _) = self.tree_root(table_id, 0)?;
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, Some((&prefix[..], true)), None)?;
        while let Some((k, pk_bytes)) = c.next(self)? {
            if !k.starts_with(&prefix) {
                break; // past every (value, *) entry
            }
            match btree::get(self, root, &pk_bytes)? {
                Some(bytes) => out.push(row::decode_row(
                    &bytes,
                    &self.eng.col_types[table_id as usize],
                )?),
                None => {
                    return Err(Error::Corrupt("index entry points at a missing row".into()))
                }
            }
        }
        Ok(out)
    }

    pub fn sys_get(&mut self, subkey: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(self, self.catalog_root, &sys_key(subkey))
    }

    pub fn sys_put(&mut self, subkey: &[u8], value: &[u8]) -> Result<()> {
        let root = self.catalog_root;
        let out = btree::insert(self, root, &sys_key(subkey), &mut btree::Payload::Flat(value), InsertMode::Upsert)?;
        self.catalog_root = out.new_root;
        Ok(())
    }

    pub fn sys_delete(&mut self, subkey: &[u8]) -> Result<bool> {
        let root = self.catalog_root;
        let out = btree::delete(self, root, &sys_key(subkey))?;
        self.catalog_root = out.new_root;
        Ok(out.existed)
    }

    pub fn sys_scan(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let lo = [SYS_PREFIX];
        let hi = [SYS_PREFIX + 1];
        let root = self.catalog_root;
        let mut c = btree::cursor(self, root, Some((&lo, true)), Some((&hi, false)))?;
        let mut out = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            out.push((k[1..].to_vec(), v));
        }
        Ok(out)
    }

    /// Prefix-bounded sys scan within the writer's view (see
    /// [`ReadTxn::sys_scan_range`]). `lo`/`hi` are subkeys; the reserved prefix
    /// is added internally.
    pub fn sys_scan_range(&mut self, lo: &[u8], hi: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let klo = sys_key(lo);
        let khi = sys_key(hi);
        let root = self.catalog_root;
        let mut c = btree::cursor(self, root, Some((&klo, true)), Some((&khi, false)))?;
        let mut out = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            out.push((k[1..].to_vec(), v));
        }
        Ok(out)
    }

    // ---------- change-data-capture (DESIGN-MIRROR §3) ----------

    /// Turn CDC dirty-set capture on/off for THIS transaction only. The mirror's
    /// replication plane (applier + importer) sets it `false` so its own writes
    /// do not become dirty entries that would echo back on the next push
    /// (DESIGN-MIRROR §3.8). Transient in-memory state; never persisted.
    pub fn set_capture(&mut self, on: bool) {
        self.capture_enabled = on;
    }

    /// Allow this txn to allocate from the reserved control-page band (§3.10),
    /// so a small control-plane commit (HALTED/frozen/cursor/park marker) can
    /// succeed when the data region is full. Use ONLY for txns that write a few
    /// sys records — never for data or bulk work, which would consume the
    /// reserve and defeat its purpose.
    pub fn set_reserved_alloc(&mut self, on: bool) {
        self.reserved_alloc = on;
    }

    /// Lazily read and cache the `cdc\0tabs` control record (default empty when
    /// absent). Enablement is set in a separate txn, so it is stable for ours.
    fn capture_config(&mut self) -> Result<CaptureConfig> {
        if let Some(c) = self.capture_cfg {
            return Ok(c);
        }
        let c = match self.sys_get(cdc::CDC_TABS_KEY)? {
            Some(bytes) => CaptureConfig::decode(&bytes)?,
            None => CaptureConfig::default(),
        };
        self.capture_cfg = Some(c);
        Ok(c)
    }

    /// Refuse a mutation targeting a write-blocked (frozen) table. Checked from
    /// the txn's own snapshot under the writer lock, before any side effect, so
    /// the mirror's authority-switch freeze (DESIGN-MIRROR §3.9) covers every
    /// write path — typed API, optimistic blind-apply, ring-leader-executed
    /// intents, and raw-engine users — not just the facade. Independent of
    /// capture suppression: a frozen table is unwritable even on the
    /// replication plane.
    fn check_write_blocked(&mut self, table_id: u32) -> Result<()> {
        if self.capture_config()?.is_blocked(table_id) {
            return Err(Error::Frozen { table_id });
        }
        Ok(())
    }

    /// Record a dirty entry for a captured table after a successful mutation.
    /// No-op when capture is suppressed for this txn or the table is not
    /// captured (the common case → one cached sys_get, then nothing). The entry
    /// is an ordinary sys-put into the catalog tree, so a savepoint rollback
    /// unwinds it for free (DESIGN-MIRROR §3.4).
    fn capture_dirty(&mut self, table_id: u32, keycode: &[u8], op: DirtyOp) -> Result<()> {
        if !self.capture_enabled {
            return Ok(());
        }
        let cfg = self.capture_config()?;
        if !cfg.is_captured(table_id) {
            return Ok(());
        }
        let entry = DirtyEntry {
            op,
            last_txn: self.meta.txn_id + 1,
            wall_us: now_micros(),
            pk_keycode: keycode.to_vec(),
        };
        let key = cdc::dirty_key(table_id, keycode);
        self.sys_put(&key, &entry.encode())
    }

    // ---------- instrumentation ----------

    /// Dirty-page accounting for the current transaction:
    /// `(dirty page count, contiguous page-id runs)`.
    ///
    /// The run count is by construction exactly the number of `msync_range`
    /// calls step 3 of [`WriteTxn::commit_with`] would issue for the
    /// *current* dirty set in `durability = commit` (the commit itself
    /// dirties a few more pages for the catalog write-back and the freelist
    /// fixpoint, so a pre-commit reading slightly undercounts — consistently
    /// so). Read-only; used by the intent-ring leader's optional batch
    /// instrumentation (`MPEDB_RING_STATS`).
    pub fn dirty_page_stats(&self) -> (usize, usize) {
        let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        let runs = if ids.is_empty() {
            0
        } else {
            1 + ids.windows(2).filter(|w| w[1] != w[0] + 1).count()
        };
        (ids.len(), runs)
    }

    // ---------- savepoints ----------

    /// Capture a statement-level savepoint. COW makes this cheap: a rollback
    /// only restores root pointers and returns pages allocated after the
    /// savepoint to the reusable pool (originals were never touched).
    /// Used by the batch leader so one failing intent aborts only itself.
    pub fn savepoint(&self) -> TxnSavepoint {
        TxnSavepoint {
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            table_roots: self.table_roots.clone(),
            dirty: self.dirty.clone(),
            freed: self.freed.clone(),
            reusable: self.reusable.clone(),
            taken: self.taken.clone(),
            refill_cursor: self.refill_cursor,
            high_water: self.high_water,
        }
    }

    /// Roll back to a savepoint taken in this transaction. `high_water` is
    /// deliberately NOT restored: pages physically allocated from it since the
    /// savepoint (ids in `[sp.high_water, high_water)`) belong to no committed
    /// freelist entry, so they are returned to `reusable` and the commit
    /// fixpoint records them as freed — page accounting stays exact.
    ///
    /// `reusable` and `freelist_root` MUST be restored together: if
    /// `refill_reusable` ran after the savepoint it pulled committed-freelist
    /// pages into `reusable` AND deleted their freelist entry (advancing
    /// `freelist_root`). Restoring `freelist_root` un-deletes that entry, so
    /// those pages are back in the freelist; keeping them in `reusable` too
    /// would list them twice at commit. Restoring `reusable` to the savepoint
    /// snapshot drops exactly the refill-pulled pages while re-offering the
    /// pages that were reusable before the savepoint.
    pub fn rollback_to(&mut self, sp: TxnSavepoint) {
        debug_assert!(!self.in_freelist_op);
        self.catalog_root = sp.catalog_root;
        self.freelist_root = sp.freelist_root;
        self.table_roots = sp.table_roots;
        self.dirty = sp.dirty;
        self.freed = sp.freed;
        self.reusable = sp.reusable;
        self.taken = sp.taken;
        self.refill_cursor = sp.refill_cursor;
        for id in sp.high_water..self.high_water {
            self.reusable.push(id);
        }
    }

    // ---------- commit / abort ----------

    pub fn commit(self) -> Result<()> {
        self.commit_with(|| {})
    }

    /// Commit, running `after_flip` after the meta publish (and durability
    /// steps) but BEFORE the writer lock is released. The intent-ring leader
    /// posts batch results there: with posting serialized under the lock, a
    /// slot can never be picked up, released, and re-used while a stale
    /// poster still holds a reference to its previous incarnation.
    pub fn commit_with<F: FnOnce()>(self, after_flip: F) -> Result<()> {
        let __commit_t = std::time::Instant::now();
        let __r = self.commit_inner(after_flip);
        leakstat::add(&leakstat::INS_NS_COMMIT, __commit_t.elapsed().as_nanos() as u64);
        __r
    }

    fn commit_inner<F: FnOnce()>(mut self, after_flip: F) -> Result<()> {
        let new_txn = self.meta.txn_id + 1;

        // 1. write back catalog entries (may COW catalog pages → more frees)
        let entries: Vec<((u32, u32), (u64, u64))> =
            self.table_roots.iter().map(|(&k, &v)| (k, v)).collect();
        for ((tid, ino), (root, cnt)) in entries {
            let mut val = [0u8; 16];
            val[0..8].copy_from_slice(&root.to_le_bytes());
            val[8..16].copy_from_slice(&cnt.to_le_bytes());
            let cat_root = self.catalog_root;
            let out = btree::insert(
                &mut self,
                cat_root,
                &cat_tree_key(tid, ino),
                &mut btree::Payload::Flat(&val),
                InsertMode::Upsert,
            )?;
            self.catalog_root = out.new_root;
        }

        // 2. freelist fixpoint (DESIGN.md §4.5). Two things get written:
        //
        //   - each drawn entry, minus whatever we consumed out of it (deleted
        //     if we consumed all of it, left completely alone if we consumed
        //     none — the common case, and the reason drawing is free);
        //   - this commit's own free set, under this commit's txn id.
        //
        // "Own free set" = pages COWed away, plus any page still sitting in
        // `reusable` that no drawn entry lists. Those are pages this txn
        // allocated (from the high-water mark, or from an entry it then fully
        // consumed) and freed again; nothing else records them, so dropping
        // them here would leak them outright.
        //
        // The circularity is LMDB's: these writes themselves allocate and free
        // pages, changing what should have been written. So iterate to a
        // fixpoint. Termination (unchanged by the read-only refill, which frees
        // nothing): each pass can only add pages freed by COWing the
        // height-bounded freelist path, the sets grow monotonically, and once
        // `reusable` is consumed allocation falls back to `high_water`, which
        // frees nothing — so the loop is bounded by O(tree height).
        let mut written: Vec<([u8; 10], Vec<u64>)> = Vec::new();
        let mut iterations = 0;
        // The whole fixpoint mutates the freelist tree: block refill so no
        // cursor read can draw from an entry these writes are rewriting (see
        // `in_freelist_op`). Allocations fall back to reusable/high-water.
        self.in_freelist_op = true;
        leakstat::add(&leakstat::COMMIT_FREED, self.freed.len() as u64);
        leakstat::add(&leakstat::COMMIT_LEFTOVER, self.reusable.len() as u64);
        leakstat::bump(&leakstat::COMMITS);
        loop {
            iterations += 1;
            if iterations > 64 {
                return Err(Error::Internal("freelist fixpoint did not converge".into()));
            }
            let plan = self.freelist_plan(new_txn, &written);
            if plan == written {
                break;
            }
            // Apply the DIFF against the previous pass: a key whose value did
            // not move must not be rewritten, or its COW would dirty the tree
            // again every pass and the loop would never settle.
            for (k, _) in &written {
                if !plan.iter().any(|(pk, _)| pk == k) {
                    let fl_root = self.freelist_root;
                    let out = btree::delete(&mut self, fl_root, k)?;
                    self.freelist_root = out.new_root;
                }
            }
            for (k, ids) in &plan {
                if written.iter().any(|(wk, wv)| wk == k && wv == ids) {
                    continue;
                }
                let fl_root = self.freelist_root;
                self.freelist_root = if ids.is_empty() {
                    // drawn dry — the entry goes away
                    btree::delete(&mut self, fl_root, k)?.new_root
                } else {
                    let mut val = Vec::with_capacity(ids.len() * 8);
                    for &id in ids {
                        val.extend_from_slice(&id.to_le_bytes());
                    }
                    btree::insert(&mut self, fl_root, k, &mut btree::Payload::Flat(&val), InsertMode::Upsert)?.new_root
                };
            }
            leakstat::add(&leakstat::COMMIT_ENTRIES, plan.len() as u64);
            leakstat::add(
                &leakstat::COMMIT_PAGES,
                plan.iter().map(|(_, v)| v.len() as u64).sum::<u64>(),
            );
            written = plan;
        }
        self.taken.clear();
        self.in_freelist_op = false;

        // 3. durability: data must be durable before the meta references it
        let snapshot = MetaSnapshot {
            slot: self.meta.slot,
            txn_id: new_txn,
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            high_water: self.high_water,
        };
        match self.eng.shm.durability {
            Durability::Commit => {
                let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
                ids.sort_unstable();
                let mut i = 0;
                while i < ids.len() {
                    let start = ids[i];
                    let mut end = start;
                    while i + 1 < ids.len() && ids[i + 1] == end + 1 {
                        i += 1;
                        end = ids[i];
                    }
                    // NO barrier per run: `F_FULLFSYNC` is per-fd, and every
                    // run here is data, i.e. the same ordering class. One
                    // barrier below covers them all. Barriering each run cost a
                    // platter flush per CONTIGUOUS RUN — the sequential-insert
                    // benchmark has one run and never showed it, but a random
                    // update scatters the btree path and the freelist path, so
                    // N=3-5 and this was 4-6 flushes per commit (#41).
                    self.eng.shm.msync_range_nobarrier(
                        start as usize * PAGE_SIZE,
                        (end - start + 1) as usize * PAGE_SIZE,
                    )?;
                    i += 1;
                }
                // ⚠ ORDERING: this barrier is what makes the data durable BEFORE
                // the meta that will reference it (DESIGN.md §4.1). It cannot be
                // merged with the meta's barrier below — a single barrier over
                // both would let a power loss land meta on the platter and not
                // its data, and meta_T would then be checksum-valid pointing at
                // COW pages that were never written. Two flushes is the FLOOR
                // here; `wal` gets away with one because its record is a single
                // self-describing checksummed object with no ordering to keep.
                self.eng.shm.sync_barrier()?;
            }
            // WAL-class (§5.4): ONE sequential record replaces the scattered
            // COW-page msyncs above and the meta-page msync below. `wal`
            // fdatasyncs it before ack (durable-on-ack); `async` only appends
            // and lets the background flusher coalesce the fdatasync
            // (crash-consistent, deferred — §5.4.2). Either way, on error the
            // commit aborts cleanly: nothing was published, Drop unlocks.
            Durability::Wal | Durability::Async => {
                let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
                ids.sort_unstable();
                if self.eng.shm.durability == Durability::Async {
                    self.eng.shm.wal_append_async(&ids, &snapshot)?;
                } else {
                    self.eng.shm.wal_commit(&ids, &snapshot)?;
                }
            }
            Durability::None => {}
        }

        // 3b. Optimistic mode: record this commit's footprint into the
        // committed-footprint ring BEFORE the flip, so any successor that reads
        // the flipped meta is guaranteed to already see this entry (both run
        // under the writer lock). Every commit records — data writes as POINT
        // or TABLE, catalog/sys-only commits as EMPTY — so an optimistic
        // validator never sees a spurious gap for a same-mode committer.
        if self.eng.concurrency == Concurrency::Optimistic {
            use crate::shm::{OFP_KIND_EMPTY, OFP_KIND_POINT, OFP_KIND_TABLE};
            let (kind, tbits, khash) = match self.commit_point {
                Some((table, khash)) => (OFP_KIND_POINT, 1u64 << (table & 63), khash),
                None if self.written_tables != 0 => {
                    (OFP_KIND_TABLE, self.written_tables, 0)
                }
                None => (OFP_KIND_EMPTY, 0, 0),
            };
            self.eng.shm.opt_record(new_txn, kind, tbits, khash);
        }

        // 4. publish (fence(Release) + atomic stores inside)
        let new_slot = self.eng.shm.write_meta_slot(self.meta.slot, &snapshot);

        match self.eng.shm.durability {
            Durability::Commit => {
                self.eng.shm.msync_page(new_slot)?;
                self.eng
                    .shm
                    .durable_txn()
                    .fetch_max(new_txn, std::sync::atomic::Ordering::AcqRel);
            }
            // WAL: the record (pages + meta fields) is already durable, so
            // the commit may be acknowledged right after the flip — no
            // meta-page msync (recovery replays the log, never trusts the
            // mapping metas; see shm::recover_after_owner_death).
            Durability::Wal => {
                self.eng
                    .shm
                    .durable_txn()
                    .fetch_max(new_txn, std::sync::atomic::Ordering::AcqRel);
            }
            Durability::None | Durability::Async => {}
        }

        after_flip();
        if self.eng.shm.durability.uses_wal() {
            // Amortized checkpoint (wal AND async), still under the writer
            // lock, after the results are posted (the full-mapping msync can
            // take a while and must not delay waiter wakeups). An msync failure
            // here is swallowed deliberately: the checkpoint simply does not
            // advance, recovery still replays from the old wal_ckpt, and the
            // next commit retries — whereas failing a commit that is already
            // durable and acknowledged would be a lie.
            let _ = self.eng.shm.wal_maybe_checkpoint();
        }
        self.finished = true;
        self.eng.shm.writer_unlock();
        Ok(())
    }

    /// Discard everything. COW means committed state was never touched; pages
    /// taken from high_water simply stay above the committed watermark, and
    /// freelist consumption was only recorded in dirty pages now dropped.
    pub fn abort(mut self) {
        self.finished = true;
        self.eng.shm.writer_unlock();
    }
}

/// Equality as the index sees it: encoded-key comparison, so all NaNs are
/// equal and -0.0 == 0.0 (Value's PartialEq disagrees on NaN, which caused
/// spurious UniqueViolations when updating rows that keep a NaN in a unique
/// column).
fn index_value_equal(a: &Value, b: &Value) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => true,
        (true, false) | (false, true) => false,
        _ => {
            keycode::encode_key(std::slice::from_ref(a))
                == keycode::encode_key(std::slice::from_ref(b))
        }
    }
}

/// Opaque statement-savepoint state (see [`WriteTxn::savepoint`]).
pub struct TxnSavepoint {
    catalog_root: u64,
    freelist_root: u64,
    table_roots: HashMap<(u32, u32), (u64, u64)>,
    dirty: DirtySet,
    freed: BTreeSet<u64>,
    reusable: Vec<u64>,
    taken: Vec<TakenEntry>,
    refill_cursor: Option<[u8; 10]>,
    high_water: u64,
}

/// One freelist entry `refill_reusable` drew pages from. The entry is still in
/// the tree with `ids` still listed; the commit fixpoint rewrites it with
/// whatever is left unconsumed (or deletes it if nothing is).
#[derive(Clone)]
struct TakenEntry {
    /// Freelist keys are always (txn BE, chunk BE) — exactly 10 bytes. Inline,
    /// not a `Vec`: the commit fixpoint rebuilds its plan on every pass, and a
    /// heap allocation per key per pass was a measurable slice of the write path.
    key: [u8; 10],
    ids: Vec<u64>,
}

fn table_column_name(eng: &Engine, table_id: u32, col: u16) -> String {
    eng.schema
        .table(table_id)
        .map(|t| t.columns[col as usize].name.clone())
        .unwrap_or_else(|| format!("col{col}"))
}

impl Drop for WriteTxn<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.eng.shm.writer_unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::Config;

    /// A `Config` that takes its database file with it when it dies.
    ///
    /// These tests used to leave the file behind. The name carries the pid, so
    /// every run leaked a fresh one and they accumulated forever — and nobody
    /// noticed, because a dev box's `/tmp` is enormous. A Raspberry Pi's is a
    /// 100 MB tmpfs, and ONE run of this suite left 35 MB in it.
    ///
    /// Derefs to `Config`, so the fourteen call sites did not change, and it
    /// cleans up on UNWIND too — a panicking test is exactly when the file is
    /// least likely to be removed by a line at the end of the function, which
    /// is how the `/dev/shm` version of this bug survived its first fix.
    struct TestCfg {
        cfg: Config,
        path: std::path::PathBuf,
    }

    impl std::ops::Deref for TestCfg {
        type Target = Config;
        fn deref(&self) -> &Config {
            &self.cfg
        }
    }

    impl Drop for TestCfg {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            // The WAL sidecar is part of the database; leaving it behind also
            // leaves the next run to open a database beside a foreign log.
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        }
    }

    fn test_config(name: &str, size_mb: u64) -> TestCfg {
        let path = std::env::temp_dir()
            .join("mpedb-engine-tests")
            .join(format!("{}-{}.mpedb", name, std::process::id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = {size_mb}
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
        TestCfg {
            cfg: Config::from_toml_str(&toml).unwrap(),
            path,
        }
    }

    fn open(cfg: &Config) -> Engine {
        Engine::open(cfg, vec![vec![]; cfg.schema.tables.len()]).unwrap()
    }

    fn user(id: i64, email: &str, age: Option<i64>) -> Vec<Value> {
        vec![
            Value::Int(id),
            Value::Text(email.into()),
            age.map(Value::Int).unwrap_or(Value::Null),
        ]
    }

    #[test]
    fn crud_cycle_with_constraints() {
        let cfg = test_config("crud", 8);
        let eng = open(&cfg);

        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(1, "a@x.no", Some(30))).unwrap();
        w.insert_row(0, &user(2, "b@x.no", None)).unwrap();
        // duplicate PK
        assert!(matches!(
            w.insert_row(0, &user(1, "c@x.no", None)),
            Err(Error::PrimaryKeyViolation { .. })
        ));
        // duplicate unique email
        assert!(matches!(
            w.insert_row(0, &user(3, "a@x.no", None)),
            Err(Error::UniqueViolation { .. })
        ));
        // NOT NULL
        assert!(matches!(
            w.insert_row(0, &[Value::Int(4), Value::Null, Value::Null]),
            Err(Error::NotNullViolation { .. })
        ));
        // rigid type
        assert!(matches!(
            w.insert_row(0, &[Value::Int(5), Value::Int(9), Value::Null]),
            Err(Error::TypeMismatch(_))
        ));
        w.commit().unwrap();

        // read it back through a snapshot
        let r = eng.begin_read().unwrap();
        assert_eq!(r.get_by_pk(0, &[Value::Int(1)]).unwrap(), Some(user(1, "a@x.no", Some(30))));
        assert_eq!(r.get_by_index(0, 1, &Value::Text("b@x.no".into())).unwrap(),
                   Some(user(2, "b@x.no", None)));
        assert_eq!(r.row_count(0).unwrap(), 2);
        r.finish().unwrap();

        // update: change indexed column, old index entry must vanish
        let mut w = eng.begin_write().unwrap();
        assert!(w.update_by_pk(0, &user(1, "a2@x.no", Some(31))).unwrap());
        w.commit().unwrap();
        let r = eng.begin_read().unwrap();
        assert_eq!(r.get_by_index(0, 1, &Value::Text("a@x.no".into())).unwrap(), None);
        assert!(r.get_by_index(0, 1, &Value::Text("a2@x.no".into())).unwrap().is_some());
        r.finish().unwrap();

        // delete
        let mut w = eng.begin_write().unwrap();
        assert!(w.delete_by_pk(0, &[Value::Int(1)]).unwrap());
        assert!(!w.delete_by_pk(0, &[Value::Int(1)]).unwrap());
        w.commit().unwrap();
        let r = eng.begin_read().unwrap();
        assert_eq!(r.get_by_pk(0, &[Value::Int(1)]).unwrap(), None);
        assert_eq!(r.get_by_index(0, 1, &Value::Text("a2@x.no".into())).unwrap(), None);
        assert_eq!(r.row_count(0).unwrap(), 1);
        r.finish().unwrap();

        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn persistence_across_reopen() {
        let cfg = test_config("persist", 8);
        {
            let eng = open(&cfg);
            let mut w = eng.begin_write().unwrap();
            for i in 0..100 {
                w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
            }
            w.commit().unwrap();
        }
        // fresh attach to the same file
        let eng = open(&cfg);
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 100);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(42)]).unwrap(),
            Some(user(42, "u42@x.no", Some(42)))
        );
        r.finish().unwrap();
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn snapshot_isolation_across_commits() {
        let cfg = test_config("mvcc", 8);
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(1, "a@x.no", Some(1))).unwrap();
        w.commit().unwrap();

        let r = eng.begin_read().unwrap(); // pins txn with exactly row 1

        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(2, "b@x.no", Some(2))).unwrap();
        assert!(w.update_by_pk(0, &user(1, "a@x.no", Some(99))).unwrap());
        w.commit().unwrap();

        // the pinned snapshot must be completely unaffected
        assert_eq!(r.row_count(0).unwrap(), 1);
        assert_eq!(r.get_by_pk(0, &[Value::Int(2)]).unwrap(), None);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
            Some(user(1, "a@x.no", Some(1)))
        );
        r.finish().unwrap();

        // a fresh snapshot sees the new state
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 2);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
            Some(user(1, "a@x.no", Some(99)))
        );
        r.finish().unwrap();
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn abort_leaves_no_trace_and_no_leak() {
        let cfg = test_config("abort", 8);
        let eng = open(&cfg);
        let before = eng.shm.newest_meta().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..50 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
        }
        w.abort();
        let after = eng.shm.newest_meta().unwrap();
        assert_eq!(before, after, "abort must not change committed state");
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 0);
        r.finish().unwrap();
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn freelist_reclaims_pages_under_churn() {
        let cfg = test_config("churn", 8);
        let eng = open(&cfg);
        // steady-state churn: insert+delete the same rows repeatedly; with a
        // working freelist, high_water must stabilize instead of growing
        // until DbFull.
        let mut high_water_after_warmup = 0;
        for round in 0..40 {
            let mut w = eng.begin_write().unwrap();
            for i in 0..50 {
                w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(round))).unwrap();
            }
            w.commit().unwrap();
            let mut w = eng.begin_write().unwrap();
            for i in 0..50 {
                assert!(w.delete_by_pk(0, &[Value::Int(i)]).unwrap());
            }
            w.commit().unwrap();
            let hw = eng.shm.newest_meta().unwrap().high_water;
            if round == 10 {
                high_water_after_warmup = hw;
            }
            if round > 10 {
                assert!(
                    hw <= high_water_after_warmup + 8,
                    "high_water grew from {high_water_after_warmup} to {hw} by \
                     round {round}: freelist is not reclaiming"
                );
            }
        }
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn pinned_reader_blocks_reclaim_until_released() {
        let cfg = test_config("pin-reclaim", 8);
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..200 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
        }
        w.commit().unwrap();

        let r = eng.begin_read().unwrap(); // pin old snapshot
        let mut w = eng.begin_write().unwrap();
        for i in 0..200 {
            w.delete_by_pk(0, &[Value::Int(i)]).unwrap();
        }
        w.commit().unwrap();
        let hw_pinned = eng.shm.newest_meta().unwrap().high_water;

        // while pinned, churn must grow the file (no reclaim of its pages)
        let mut w = eng.begin_write().unwrap();
        for i in 0..100 {
            w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), None)).unwrap();
        }
        w.commit().unwrap();
        assert!(eng.shm.newest_meta().unwrap().high_water > hw_pinned);

        r.finish().unwrap(); // release the pin

        // after release, steady churn reclaims: high_water stabilizes
        let mut stable = eng.shm.newest_meta().unwrap().high_water;
        for round in 0..20 {
            let mut w = eng.begin_write().unwrap();
            for i in 0..100 {
                w.delete_by_pk(0, &[Value::Int(1000 + i)]).unwrap();
            }
            for i in 0..100 {
                w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), None)).unwrap();
            }
            w.commit().unwrap();
            let hw = eng.shm.newest_meta().unwrap().high_water;
            if round >= 5 {
                assert!(hw <= stable + 8, "no reclaim after pin release");
            }
            stable = stable.max(hw);
        }
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    #[test]
    fn page_accounting_sys_api_and_open_from_file() {
        let cfg = test_config("accounting", 8);
        let eng = open(&cfg);
        // invariant must hold after every kind of commit
        eng.verify_page_accounting().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..300 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.sys_put(b"plan/abc", b"BLOB-1").unwrap();
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap();

        let mut w = eng.begin_write().unwrap();
        for i in 0..150 {
            w.delete_by_pk(0, &[Value::Int(i * 2)]).unwrap();
        }
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap();

        // sys records readable from snapshots and writers
        let r = eng.begin_read().unwrap();
        assert_eq!(r.sys_get(b"plan/abc").unwrap().unwrap(), b"BLOB-1");
        assert_eq!(r.sys_scan().unwrap().len(), 1);
        // stored schema equals the config schema
        assert_eq!(r.stored_schema().unwrap(), cfg.schema);
        r.finish().unwrap();

        // config-free open sees the same data and schema
        let eng2 = Engine::open_from_file(&cfg.options.path).unwrap();
        assert_eq!(eng2.schema(), &cfg.schema);
        let r = eng2.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 150);
        r.finish().unwrap();

        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    // ------------------------------------------------- wal durability tests

    fn wal_config(name: &str) -> Config {
        wal_class_config(name, "wal")
    }

    /// WAL-class config with the given durability (`wal` or `async`).
    fn wal_class_config(name: &str, durability: &str) -> Config {
        let base = std::path::Path::new("/dev/shm");
        let dir = if base.is_dir() {
            base.join("mpedb-engine-wal-tests")
        } else {
            std::env::temp_dir().join("mpedb-engine-wal-tests")
        };
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}-{}.mpedb", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::shm::wal_path(&path));
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = 8
max_readers = 64
durability = "{durability}"

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

    fn wal_cleanup(cfg: &Config) {
        let _ = std::fs::remove_file(&cfg.options.path);
        let _ = std::fs::remove_file(crate::shm::wal_path(&cfg.options.path));
    }

    /// Regress the mapping to a plausible post-power-loss state: stale lock
    /// area (wal_len/wal_ckpt as of `stale_len`/`stale_ckpt`) and both meta
    /// slots rolled back to genesis — then replay the log.
    fn simulate_reboot_and_recover(eng: &Engine, stale_ckpt: u64, stale_len: u64) -> u64 {
        use std::sync::atomic::Ordering;
        eng.shm.wal_ckpt().store(stale_ckpt, Ordering::Release);
        eng.shm.wal_len().store(stale_len, Ordering::Release);
        let genesis = MetaSnapshot {
            slot: 0,
            txn_id: 0,
            catalog_root: 0,
            freelist_root: 0,
            high_water: eng.shm.data_start,
        };
        eng.shm.write_meta_slot(0, &genesis);
        eng.shm.write_meta_slot(1, &genesis);
        eng.shm.wal_recover().unwrap()
    }

    #[test]
    fn wal_mode_crud_persistence_and_reopen() {
        let cfg = wal_config("crud");
        {
            let eng = open(&cfg);
            let mut w = eng.begin_write().unwrap();
            for i in 0..100 {
                w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
            }
            w.commit().unwrap();
            eng.verify_page_accounting().unwrap();
            // durable gate: readers see the commit only after the fdatasync
            let r = eng.begin_read().unwrap();
            assert_eq!(r.row_count(0).unwrap(), 100);
            r.finish().unwrap();
        }
        // reattach (no reboot): the mapping is authoritative, no replay needed
        let eng = open(&cfg);
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 100);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(42)]).unwrap(),
            Some(user(42, "u42@x.no", Some(42)))
        );
        r.finish().unwrap();
        wal_cleanup(&cfg);
    }

    #[test]
    fn wal_recovery_rebuilds_engine_state_from_log_alone() {
        let cfg = wal_config("recover");
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..60 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.commit().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..30 {
            w.delete_by_pk(0, &[Value::Int(i * 2)]).unwrap();
        }
        w.commit().unwrap();

        // power loss that wrote NOTHING volatile back: even both meta slots
        // are gone; the log alone must rebuild the committed state
        simulate_reboot_and_recover(&eng, 0, 0);

        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 30);
        assert_eq!(r.get_by_pk(0, &[Value::Int(0)]).unwrap(), None);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(1)]).unwrap(),
            Some(user(1, "u1@x.no", Some(1)))
        );
        assert_eq!(
            r.get_by_index(0, 1, &Value::Text("u1@x.no".into())).unwrap(),
            Some(user(1, "u1@x.no", Some(1)))
        );
        r.finish().unwrap();
        eng.verify_page_accounting().unwrap();

        // replay idempotency, engine level: recover again, same state
        simulate_reboot_and_recover(&eng, 0, 0);
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 30);
        r.finish().unwrap();
        eng.verify_page_accounting().unwrap();
        wal_cleanup(&cfg);
    }

    #[test]
    fn wal_checkpoint_then_recovery_spans_the_boundary() {
        use std::sync::atomic::Ordering;
        let cfg = wal_config("ckpt");
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..40 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
        }
        w.commit().unwrap();
        // force a checkpoint (threshold 1 byte): main file caught up, ckpt=len
        eng.shm.wal_checkpoint_if(1).unwrap();
        let ckpt = eng.shm.wal_ckpt().load(Ordering::Acquire);
        assert_eq!(ckpt, eng.shm.wal_len().load(Ordering::Acquire));
        assert!(ckpt > 0);

        // post-checkpoint commits...
        let mut w = eng.begin_write().unwrap();
        for i in 40..70 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), None)).unwrap();
        }
        w.commit().unwrap();

        // ...survive a reboot whose lock-area wal_len writeback was lost
        // (metas regressed too); scan starts at the durable ckpt
        let end = simulate_reboot_and_recover(&eng, ckpt, ckpt);
        assert!(end > ckpt, "post-checkpoint records must be replayed");
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 70);
        r.finish().unwrap();
        eng.verify_page_accounting().unwrap();
        wal_cleanup(&cfg);
    }

    // ---------------------------- async (deferred-fsync WAL) durability tests
    //
    // The deterministic contract tests (visibility-before-durability, flushed
    // recovery, un-flushed torn tail) live at the Shm level (see shm::tests),
    // where there is no background flusher to race. This is the full-stack
    // integration: real flusher thread + clean-shutdown final flush on Engine
    // drop + reopen.

    #[test]
    fn async_end_to_end_flusher_and_reopen() {
        let cfg = wal_class_config("async-e2e", "async");
        {
            let eng = open(&cfg); // durability=async spawns the flusher
            let mut w = eng.begin_write().unwrap();
            for i in 0..200 {
                w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
            }
            w.commit().unwrap();
            // VISIBILITY: observable immediately, without waiting for a flush.
            let r = eng.begin_read().unwrap();
            assert_eq!(r.row_count(0).unwrap(), 200);
            r.finish().unwrap();
            eng.verify_page_accounting().unwrap();
            // Engine drop here stops the flusher AFTER a synchronous final
            // flush — clean shutdown loses nothing (§5.4.2).
        }
        // reattach (no reboot): mapping authoritative, everything persisted
        let eng = open(&cfg);
        let r = eng.begin_read().unwrap();
        assert_eq!(r.row_count(0).unwrap(), 200);
        assert_eq!(
            r.get_by_pk(0, &[Value::Int(150)]).unwrap(),
            Some(user(150, "u150@x.no", Some(150)))
        );
        r.finish().unwrap();
        wal_cleanup(&cfg);
    }

    #[test]
    fn concurrent_readers_and_writer_threads() {
        let cfg = test_config("threads", 16);
        let eng = std::sync::Arc::new(open(&cfg));
        let mut w = eng.begin_write().unwrap();
        // bank invariant: total balance is conserved by transfers
        for i in 0..20 {
            w.insert_row(0, &user(i, &format!("acct{i}@x.no"), Some(1000))).unwrap();
        }
        w.commit().unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::new();
        // 4 reader threads validating the invariant on every snapshot
        for _ in 0..4 {
            let eng = eng.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                let mut checks = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let r = eng.begin_read().unwrap();
                    let mut c = r.scan(0, None, None).unwrap();
                    let mut sum = 0i64;
                    let mut rows = 0;
                    while let Some(row) = c.next().unwrap() {
                        if let Value::Int(b) = row[2] {
                            sum += b;
                        }
                        rows += 1;
                    }
                    assert_eq!(rows, 20, "snapshot must always see all 20 accounts");
                    assert_eq!(sum, 20_000, "balance sum must be invariant");
                    r.finish().unwrap();
                    checks += 1;
                }
                checks
            }));
        }
        // 1 writer thread doing random transfers
        {
            let eng = eng.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                let mut x = 0x12345u64;
                for _ in 0..300 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let from = (x % 20) as i64;
                    let to = ((x >> 8) % 20) as i64;
                    if from == to {
                        continue;
                    }
                    let mut w = eng.begin_write().unwrap();
                    let a = w.get_by_pk(0, &[Value::Int(from)]).unwrap().unwrap();
                    let b = w.get_by_pk(0, &[Value::Int(to)]).unwrap().unwrap();
                    let (Value::Int(ab), Value::Int(bb)) = (&a[2], &b[2]) else {
                        panic!()
                    };
                    let amount = (x % 50) as i64;
                    let mut a2 = a.clone();
                    let mut b2 = b.clone();
                    a2[2] = Value::Int(ab - amount);
                    b2[2] = Value::Int(bb + amount);
                    w.update_by_pk(0, &a2).unwrap();
                    w.update_by_pk(0, &b2).unwrap();
                    w.commit().unwrap();
                }
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                0u64
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        std::fs::remove_file(&cfg.options.path).unwrap();
    }

    fn enable_capture(eng: &Engine, tables: &[u32]) {
        let mut cfg = CaptureConfig::default();
        for &t in tables {
            cfg.set_captured(t, true);
        }
        cfg.generation = 1;
        let mut w = eng.begin_write().unwrap();
        w.set_capture(false); // the control write must not capture itself
        w.sys_put(cdc::CDC_TABS_KEY, &cfg.encode()).unwrap();
        w.commit().unwrap();
    }

    fn dirty(eng: &Engine) -> Vec<DirtyEntry> {
        let r = eng.begin_read().unwrap();
        let raw = r
            .sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END)
            .unwrap();
        r.finish().unwrap();
        raw.iter().map(|(_, v)| DirtyEntry::decode(v).unwrap()).collect()
    }

    fn set_write_block(eng: &Engine, blocked: &[u32]) {
        let mut cfg = CaptureConfig::default();
        for &t in blocked {
            cfg.set_blocked(t, true);
        }
        cfg.generation = 1;
        let mut w = eng.begin_write().unwrap();
        w.set_capture(false);
        w.sys_put(cdc::CDC_TABS_KEY, &cfg.encode()).unwrap();
        w.commit().unwrap();
    }

    #[test]
    fn cdc_write_block_refuses_typed_mutators_with_no_side_effects() {
        let cfg = test_config("cdcblock", 8);
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(1, "a@x.no", Some(10))).unwrap();
        w.commit().unwrap();

        set_write_block(&eng, &[0]);

        let mut w = eng.begin_write().unwrap();
        assert!(matches!(
            w.insert_row(0, &user(2, "b@x.no", Some(20))),
            Err(Error::Frozen { table_id: 0 })
        ));
        assert!(matches!(
            w.update_by_pk(0, &user(1, "a2@x.no", Some(11))),
            Err(Error::Frozen { table_id: 0 })
        ));
        assert!(matches!(
            w.delete_by_pk(0, &[Value::Int(1)]),
            Err(Error::Frozen { table_id: 0 })
        ));
        drop(w); // abort

        // the seeded row is untouched (the checks fired before any side effect)
        let mut w = eng.begin_write().unwrap();
        assert!(w.get_by_pk(0, &[Value::Int(1)]).unwrap().is_some());
        assert!(w.get_by_pk(0, &[Value::Int(2)]).unwrap().is_none());
        drop(w);

        // clearing the block re-enables writes
        set_write_block(&eng, &[]);
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(2, "b@x.no", Some(20))).unwrap();
        w.commit().unwrap();
        let mut w = eng.begin_write().unwrap();
        assert!(w.get_by_pk(0, &[Value::Int(2)]).unwrap().is_some());
        drop(w);
        eng.verify_page_accounting().unwrap();
    }

    #[test]
    fn reserved_pages_extend_the_alloc_ceiling_past_normal_dbfull() {
        // fill a small db to the NORMAL ceiling in one txn, then prove a
        // reserved-mode allocation still succeeds (the control-plane headroom).
        let cfg = test_config("reservedpool", 2);
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        let mut i = 0i64;
        loop {
            match w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))) {
                Ok(()) => i += 1,
                Err(Error::DbFull) => break,
                Err(e) => panic!("unexpected error while filling: {e}"),
            }
        }
        assert!(i > 0, "db should hold at least some rows");
        // normal mode is now full
        assert!(matches!(
            w.insert_row(0, &user(i, "x@x.no", Some(0))),
            Err(Error::DbFull)
        ));
        // reserved mode reaches into the reserve band and succeeds (the normal
        // ceiling was RESERVED_CONTROL_PAGES below page_count)
        w.set_reserved_alloc(true);
        w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i)))
            .expect("reserved allocation should succeed past the normal ceiling");
        // the flag persists through commit, so a reserved control write's whole
        // commit (COW + freelist fixpoint) also draws from the reserve band
        w.set_reserved_alloc(false);
        w.set_reserved_alloc(true);
        w.sys_put(b"mir\0halt", b"db_full").unwrap();
        w.commit().expect("reserved control write commits past the normal full");
        let r = eng.begin_read().unwrap();
        assert_eq!(r.sys_get(b"mir\0halt").unwrap().unwrap(), b"db_full");
        r.finish().unwrap();
    }

    #[test]
    fn cdc_write_block_refuses_optimistic_blind_apply() {
        let path = std::env::temp_dir()
            .join("mpedb-engine-tests")
            .join(format!("cdcblockopt-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 64\n\
             [[table]]\nname = \"kv\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
             [[table.column]]\nname = \"v\"\ntype = \"int64\"\n",
            path.display()
        );
        let cfg = Config::from_toml_str(&toml).unwrap();
        let eng = Engine::open(&cfg, vec![vec![]]).unwrap();
        set_write_block(&eng, &[0]);

        let key = keycode::encode_key(&[Value::Int(7)]);
        let payload =
            row::encode_row(&[Value::Int(7), Value::Int(1)], &[ColumnType::Int64; 2]).unwrap();
        let mut w = eng.begin_write().unwrap();
        assert!(matches!(
            w.optimistic_insert(0, &key, &payload),
            Err(Error::Frozen { table_id: 0 })
        ));
        assert!(matches!(
            w.optimistic_upsert(0, &key, &payload),
            Err(Error::Frozen { table_id: 0 })
        ));
        assert!(matches!(
            w.optimistic_delete(0, &key),
            Err(Error::Frozen { table_id: 0 })
        ));
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cdc_capture_hooks_all_typed_mutators() {
        let cfg = test_config("cdccap", 8);
        let eng = open(&cfg);

        // no capture configured → writes leave no dirty entries
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(1, "a@x.no", Some(10))).unwrap();
        w.commit().unwrap();
        assert_eq!(dirty(&eng).len(), 0);

        enable_capture(&eng, &[0]);
        eng.verify_page_accounting().unwrap(); // A

        // insert → one Upsert entry keyed by the PK keycode
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &user(2, "b@x.no", Some(20))).unwrap();
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap(); // B
        let d = dirty(&eng);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].op, DirtyOp::Upsert);
        assert_eq!(d[0].pk_keycode, keycode::encode_key(&[Value::Int(2)]));

        // update same PK coalesces (still one, still Upsert)
        let mut w = eng.begin_write().unwrap();
        w.update_by_pk(0, &user(2, "b2@x.no", Some(21))).unwrap();
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap(); // C
        let d = dirty(&eng);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].op, DirtyOp::Upsert);

        // delete flips the coalesced entry to a tombstone
        let mut w = eng.begin_write().unwrap();
        assert!(w.delete_by_pk(0, &[Value::Int(2)]).unwrap());
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap(); // D
        let d = dirty(&eng);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].op, DirtyOp::Delete);

        // a suppressed replication-plane write captures nothing
        let mut w = eng.begin_write().unwrap();
        w.set_capture(false);
        w.insert_row(0, &user(3, "c@x.no", Some(30))).unwrap();
        w.commit().unwrap();
        assert_eq!(dirty(&eng).len(), 1); // still just PK=2's tombstone

        // savepoint rollback unwinds a captured dirty entry (COW §3.4). This
        // also exercises capture-triggered refill inside a savepoint (the
        // rollback_to reusable/freelist-root consistency fix).
        let mut w = eng.begin_write().unwrap();
        let sp = w.savepoint();
        w.insert_row(0, &user(4, "d@x.no", Some(40))).unwrap();
        assert_eq!(
            w.sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END).unwrap().len(),
            2
        );
        w.rollback_to(sp);
        assert_eq!(
            w.sys_scan_range(cdc::CDC_DIRTY_PREFIX, cdc::CDC_DIRTY_PREFIX_END).unwrap().len(),
            1
        );
        w.commit().unwrap();
        assert_eq!(dirty(&eng).len(), 1);

        eng.verify_page_accounting().unwrap();
    }

    #[test]
    fn savepoint_rollback_after_refill_keeps_accounting_exact() {
        // Regression (found via the CDC hook): when refill_reusable runs INSIDE
        // a savepoint it pulls committed-freelist pages into `reusable` and
        // deletes their freelist entry; rollback_to must restore both `reusable`
        // and `freelist_root` together or those pages get listed twice.
        let cfg = test_config("sprefill", 8);
        let eng = open(&cfg);
        let mut w = eng.begin_write().unwrap();
        for i in 0..400 {
            w.insert_row(0, &user(i, &format!("u{i}@x.no"), Some(i))).unwrap();
        }
        w.commit().unwrap();
        let mut w = eng.begin_write().unwrap();
        for i in 0..400 {
            w.delete_by_pk(0, &[Value::Int(i)]).unwrap();
        }
        w.commit().unwrap();
        // tiny commits with no live reader advance the oldest-pinned bound past
        // the delete, making its freed pages reclaimable by refill
        for _ in 0..2 {
            let mut w = eng.begin_write().unwrap();
            w.sys_put(b"tick", b"x").unwrap();
            w.commit().unwrap();
        }
        eng.verify_page_accounting().unwrap();

        // allocate heavily INSIDE a savepoint (forces refill), then roll back
        let mut w = eng.begin_write().unwrap();
        let sp = w.savepoint();
        for i in 0..400 {
            w.insert_row(0, &user(1000 + i, &format!("v{i}@x.no"), Some(i))).unwrap();
        }
        w.rollback_to(sp);
        w.commit().unwrap();
        eng.verify_page_accounting().unwrap();
    }

    #[test]
    fn cdc_capture_hooks_optimistic_blind_apply() {
        // a table with no secondary index, so the optimistic trio is legal
        let path = std::env::temp_dir()
            .join("mpedb-engine-tests")
            .join(format!("cdcopt-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 64\n\
             [[table]]\nname = \"kv\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
             [[table.column]]\nname = \"v\"\ntype = \"int64\"\n",
            path.display()
        );
        let cfg = Config::from_toml_str(&toml).unwrap();
        let eng = Engine::open(&cfg, vec![vec![]]).unwrap();
        enable_capture(&eng, &[0]);

        let key = keycode::encode_key(&[Value::Int(7)]);
        let payload =
            row::encode_row(&[Value::Int(7), Value::Int(100)], &[ColumnType::Int64; 2]).unwrap();

        let mut w = eng.begin_write().unwrap();
        assert!(w.optimistic_insert(0, &key, &payload).unwrap());
        w.commit().unwrap();
        let d = dirty(&eng);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].op, DirtyOp::Upsert);

        let mut w = eng.begin_write().unwrap();
        assert!(w.optimistic_delete(0, &key).unwrap());
        w.commit().unwrap();
        assert_eq!(dirty(&eng)[0].op, DirtyOp::Delete);

        eng.verify_page_accounting().unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sys_scan_range_is_prefix_bounded_and_txn_id_tracks_commits() {
        let cfg = test_config("sysrange", 8);
        let eng = open(&cfg);

        let mut w = eng.begin_write().unwrap();
        // several families sharing the sys region
        w.sys_put(b"cdc\0d/\x00\x00\x00\x00A", b"1").unwrap();
        w.sys_put(b"cdc\0d/\x00\x00\x00\x00B", b"2").unwrap();
        w.sys_put(b"cdc\0tabs", b"T").unwrap();
        w.sys_put(b"plan/xyz", b"P").unwrap();
        w.sys_put(b"mir\0epoch", b"E").unwrap();
        w.commit().unwrap();

        // scan just the cdc dirty family [cdc\0d/, cdc\0d0): 0x30 ('0') is the
        // byte after '/' (0x2f), an exclusive upper bound past every d/ key.
        let r = eng.begin_read().unwrap();
        let dirty = r.sys_scan_range(b"cdc\0d/", b"cdc\0d0").unwrap();
        assert_eq!(dirty.len(), 2, "only the two d/ entries, not tabs/plan/mir");
        assert_eq!(dirty[0].0, b"cdc\0d/\x00\x00\x00\x00A");
        assert_eq!(dirty[1].1, b"2");
        assert_eq!(r.sys_scan().unwrap().len(), 5); // whole region still intact
        let t_after = r.txn_id();
        r.finish().unwrap();

        // txn_id advances by exactly one per commit
        let mut w = eng.begin_write().unwrap();
        assert_eq!(w.meta.txn_id, t_after);
        w.sys_put(b"cdc\0d/\x00\x00\x00\x00C", b"3").unwrap();
        w.commit().unwrap();
        let r = eng.begin_read().unwrap();
        assert_eq!(r.txn_id(), t_after + 1);
        // writer-side prefix scan agrees with the reader
        let mut w = eng.begin_write().unwrap();
        assert_eq!(w.sys_scan_range(b"cdc\0d/", b"cdc\0d0").unwrap().len(), 3);
        drop(w);
        r.finish().unwrap();
        eng.verify_page_accounting().unwrap();
    }
}

#[cfg(test)]
mod debug_tests {
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
        std::fs::remove_file(&cfg.options.path).unwrap();
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
            let payload = row::encode_row(&[Value::Int(key), Value::Int(val)], &eng.col_types[0]).unwrap();
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
        std::fs::remove_file(&path).unwrap();
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
}
