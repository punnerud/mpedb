//! The engine: transactions, catalog, freelist, and the typed row API.
//!
//! Ties the shared-memory layer (`shm`) to the COW B+tree (`btree`):
//! - `ReadTxn` — a pinned MVCC snapshot; lock-free, read-only.
//! - `WriteTxn` — the writer-lock holder; implements [`PageStore`] with COW
//!   discipline, allocates from the freelist/high-water mark, and commits via
//!   the double-buffered meta protocol (design/DESIGN.md §5.2).
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
        // DESIGN-BLOBEXTENT §6/§13.6: space reconciles like #40's time did.
        EXTENT_ALLOC_PAGES, // pages allocated into extent runs
        EXTENT_FREED_PAGES, // pages of committed extents freed again
        EXTENT_FRAG_PAGES,  // free-but-fragmented pages at a DbFull
        REFLINK_HITS,       // FICLONERANGE actually engaged, not fallback-copied
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
        EXEC_NS_RESOLVE,  // session::resolve_params — DEEP-cloned every Value until the Cow fix
        EXEC_NS_BUILDROW, // exec::build_insert_row — the THIRD full deep-clone of the blob
        EXEC_NS_STMT,     // exec::exec_stmt in total — resolve + stmt must ≈ execute() wall time
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
    keycode, keycode::KeySpec, Concurrency, ColumnType, Config, Durability, Error,
    ExprProgram, Result, Schema, Value, PAGE_SIZE,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

mod commit;
mod extent;
mod fts;
mod freelist;
mod read;
mod write;

pub(crate) use fts::FTS_INDEX_NO;

#[cfg(test)]
mod debug_tests;
#[cfg(test)]
mod tests;

pub use read::{ReadTxn, RowCursor};
pub use write::{TxnSavepoint, TxnSavepointFull, WriteTxn};
use write::DirtySet;

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

/// Deterministic per-statement-execution "work-row" meter (#74,
/// design/DESIGN-RUNTIME-BUDGET.md). Both txn kinds own one, seeded from
/// [`Engine::work_budget`]; the scan/cursor layer and the SQL executor charge
/// the SAME meter. `budget == 0` is the unlimited sentinel.
///
/// The counter is a running sum incremented once per row a scan yields (and per
/// nested-loop-join candidate / correlated-subquery re-evaluation in the exec
/// layer). Data-driven and never time/random: the same query over the same data
/// trips at the same `used` on every machine.
pub struct WorkMeter {
    budget: u64,
    used: AtomicU64,
}

impl WorkMeter {
    pub fn new(budget: u64) -> WorkMeter {
        WorkMeter { budget, used: AtomicU64::new(0) }
    }

    /// Charge `n` work-rows and return [`Error::RuntimeBudget`] once the running
    /// total crosses the budget. `which` is called ONLY on the error path, so a
    /// coarse attribution label costs nothing on the hot path. Relaxed ordering:
    /// a transaction's cursors and its executor run on one thread — there is no
    /// cross-thread ordering to establish, only the sum.
    #[inline]
    pub fn charge(&self, n: u64, which: impl FnOnce() -> String) -> Result<()> {
        let used = self
            .used
            .fetch_add(n, AtomicOrdering::Relaxed)
            .saturating_add(n);
        if self.budget != 0 && used > self.budget {
            return Err(Error::RuntimeBudget {
                kind: mpedb_types::BudgetKind::WorkRows,
                limit: self.budget,
                used,
                which: which(),
            });
        }
        Ok(())
    }

    /// Work-rows charged so far this execution.
    pub fn used(&self) -> u64 {
        self.used.load(AtomicOrdering::Relaxed)
    }

    /// The configured budget (`0` = unlimited).
    pub fn budget(&self) -> u64 {
        self.budget
    }
}

/// The `which` attribution for a scan of `table_id` (#74). Built lazily, only
/// when the budget trips, so it costs nothing on the hot path.
pub(super) fn scan_label(schema: &Schema, table_id: u32) -> String {
    match schema.table(table_id) {
        Some(t) => format!("scan of table \"{}\"", t.name),
        None => format!("scan of table #{table_id}"),
    }
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
/// (design/DESIGN.md §4.5) converge.
const FREELIST_CHUNK_PAGES: usize = 120; // 960-byte values

/// DROP frees a whole table's pages in ONE commit, and `freelist_plan` keys the
/// resulting freed-page chunks with a `u16` chunk index (`FREELIST_CHUNK_PAGES`
/// pages each), so a single commit can record at most `65536 * 120` ≈ 7.86M
/// freed pages before the index wraps and two chunks collide. We refuse a DROP
/// whose table exceeds this bound (with margin for the fixpoint's own freed
/// pages) rather than corrupt the freelist — ~23 GB in one table is far past any
/// first-version workload, and cross-commit bounded reclamation is deferred
/// (DESIGN-DROP-TABLE §2).
const MAX_DROP_PAGES: u64 = 6_000_000;

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

/// Freelist entry kinds (the byte between txn and chunk in the key).
/// DESIGN-BLOBEXTENT §3.3: there was no spare byte in the old 10-byte key, so
/// v3 keys carry an explicit kind — txn stays FIRST so the refill scan's
/// early-stop-by-oldest-txn order survives unchanged.
pub(super) const FK_PAGES: u8 = 0;
/// Extent runs: values are `(start_page u64 LE ‖ npages u32 LE)` pairs.
/// Lands with the extent allocator; until then a kind-1 entry is corrupt.
pub(super) const FK_RUNS: u8 = 1;

/// Read an extent's bytes out of the mapping, bounds-checked against the
/// MAPPING (page_count), not just high_water — checked u64 math throughout
/// (DESIGN-BLOBEXTENT §3.1).
pub(super) fn read_extent_from_shm(
    shm: &Shm,
    start_page: u64,
    total_len: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    let npages = total_len
        .checked_add(PAGE_SIZE as u64 - 1)
        .ok_or_else(|| Error::Corrupt("extent length overflow".into()))?
        / PAGE_SIZE as u64;
    let end = start_page
        .checked_add(npages)
        .ok_or_else(|| Error::Corrupt("extent run overflows the page space".into()))?;
    if start_page < shm.data_start || end > shm.page_count {
        return Err(Error::Corrupt("extent outside the data region".into()));
    }
    let off = (start_page as usize)
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| Error::Corrupt("extent offset overflow".into()))?;
    out.extend_from_slice(shm.bytes(off, total_len as usize)?);
    Ok(())
}

fn freelist_key(txn: u64, kind: u8, chunk: u16) -> [u8; 11] {
    let mut k = [0u8; 11];
    k[..8].copy_from_slice(&txn.to_be_bytes());
    k[8] = kind;
    k[9..].copy_from_slice(&chunk.to_be_bytes());
    k
}

/// Encode a point-probe key. A single plain `Int` PK is 9 bytes and fits in
/// `stack` (no heap); every other shape uses `heap` via [`keycode::encode_key_spec`].
/// Used by PK lookups on the prepare+bind SELECT hot path.
pub(super) fn encode_probe_key<'a>(
    values: &[Value],
    specs: &[keycode::KeySpec],
    stack: &'a mut [u8; 9],
    heap: &'a mut Vec<u8>,
) -> &'a [u8] {
    if values.len() == 1
        && matches!(values[0], Value::Int(_))
        && specs.first().is_none_or(|s| s.is_plain())
    {
        let Value::Int(x) = &values[0] else {
            unreachable!("matched Int above");
        };
        // keycode: TAG_PRESENT (0x01) ‖ (x as u64 ^ signbit) BE — identical to
        // encode_value for Int under a plain KeySpec.
        stack[0] = 0x01;
        stack[1..9].copy_from_slice(&((*x as u64) ^ (1u64 << 63)).to_be_bytes());
        return &stack[..9];
    }
    *heap = keycode::encode_key_spec(values, specs);
    heap.as_slice()
}

/// Secondary unique index columns for a table, per the shared numbering
/// convention (design/DESIGN.md §4.4): index 0 = PK tree; unique columns in
/// declaration order get 1, 2, …; a column that is by itself the whole PK is
/// skipped.
/// The secondary-index B+tree key for a row's indexed columns, or `None`
/// when ANY indexed column is NULL — the membership rule (DESIGN-SCHEMA-V2
/// §1.4): at k = 1 this is exactly the historical skip-if-NULL, and SQL
/// uniqueness never treats NULLs as conflicting. A UNIQUE index keys by the
/// values alone; a non-unique one appends the pk so duplicate values become
/// distinct, memcmp-ordered entries. `encode_key` is a plain concatenation
/// of `encode_value`, so the k = 1 output is byte-identical to the old
/// single-value construction — existing index trees need no rebuild.
fn index_row_key(
    unique: bool,
    cols: &[u16],
    row: &[mpedb_types::Value],
    pk_key: &[u8],
    specs: &[KeySpec],
) -> Option<Vec<u8>> {
    let mut k = Vec::with_capacity(16 * cols.len() + pk_key.len());
    for (j, &c) in cols.iter().enumerate() {
        let v = &row[c as usize];
        if v.is_null() {
            return None;
        }
        keycode::encode_value_spec(&mut k, v, specs.get(j).copied().unwrap_or_default());
    }
    if !unique {
        // The PK suffix is the ROW IDENTITY and is never folded — it must
        // round-trip to fetch the row. Only the indexed VALUE prefix collates.
        k.extend_from_slice(pk_key);
    }
    Some(k)
}

/// Per-column compiled CHECK programs, one entry per table (indexed like
/// `schema.tables`), one per column. Compiled by the facade (SQL layer);
/// `None` = no CHECK on that column.
pub type CheckPrograms = Vec<Vec<Option<ExprProgram>>>;

/// Compiles a schema's `ColumnDef::check` SOURCE strings into [`CheckPrograms`].
///
/// mpedb-core stores CHECK sources but cannot parse SQL, so the facade installs
/// one of these ([`Engine::set_check_compiler`]) and the engine calls it every
/// time it (re)builds a bundle from the catalog. Without it, a table created by
/// `CREATE TABLE … CHECK (…)` carries its constraint in the schema and gets an
/// EMPTY program slot — a constraint that is stored, reported by the catalog,
/// and never enforced. Whether a CHECK fires must not depend on whether the
/// table came from the config or from DDL.
pub type CheckCompiler = dyn Fn(&Schema) -> Result<CheckPrograms> + Send + Sync;

/// The schema plus every per-table cache derived from it, immutable as a
/// unit (#47): transactions capture ONE `Arc<SchemaBundle>` at begin, so a
/// txn sees one schema version even while DDL swaps the engine's current
/// bundle underneath. `TableDef.indexes` is the single derivation source.
pub struct SchemaBundle {
    /// The meta `schema_gen` this bundle was loaded at — compared against
    /// the committed meta at txn begin; mismatch ⇒ reload from the catalog.
    pub schema_gen: u64,
    pub schema: Schema,
    pub checks: CheckPrograms,
    /// Per table, per index (`index_no - 1`): the indexed column ordinals in
    /// key order.
    pub sec_indexes: Vec<Vec<Vec<u16>>>,
    /// Parallel to `sec_indexes`: is index `k` UNIQUE (values→pk, enforced)
    /// or plain non-unique (`(values ‖ pk)` key, duplicates allowed)?
    pub sec_unique: Vec<Vec<bool>>,
    pub col_types: Vec<Vec<ColumnType>>,
    /// Per table: the [`KeySpec`] of each PRIMARY KEY column, in key order —
    /// parallel to `TableDef.primary_key`. Empty-folding when every entry is
    /// plain (the common case).
    pub pk_specs: Vec<Vec<KeySpec>>,
    /// Per table, per secondary index (`index_no - 1`): the [`KeySpec`] of each
    /// indexed column in key order — parallel to `sec_indexes`.
    pub sec_specs: Vec<Vec<Vec<KeySpec>>>,
    /// Per table: does ANY key column (a PK column or a column of any secondary
    /// index) need something other than the plain bytewise encoding — a
    /// non-`Binary` collation, or a typeless (`any`) column, which keys by
    /// STORAGE CLASS? `false` ⇒ every key builder takes the plain path and
    /// never touches the spec vectors, so an ordinary database pays nothing
    /// (and encodes byte-for-byte as before).
    pub any_key_special: Vec<bool>,
}

impl std::ops::Deref for SchemaBundle {
    type Target = Schema;
    fn deref(&self) -> &Schema {
        &self.schema
    }
}

impl SchemaBundle {
    pub fn new_at(schema_gen: u64, schema: Schema, checks: CheckPrograms) -> SchemaBundle {
        let mut b = SchemaBundle::new(schema, checks);
        b.schema_gen = schema_gen;
        b
    }

    pub fn new(schema: Schema, checks: CheckPrograms) -> SchemaBundle {
        let sec_indexes = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.columns.clone()).collect())
            .collect();
        let sec_unique = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.unique).collect())
            .collect();
        let col_types = schema
            .tables
            .iter()
            .map(|t| t.columns.iter().map(|c| c.ty).collect())
            .collect();
        let spec_of = |c: &mpedb_types::ColumnDef| KeySpec::for_column(c.ty, c.collation);
        let pk_specs: Vec<Vec<KeySpec>> = schema
            .tables
            .iter()
            .map(|t| t.primary_key.iter().map(|&i| spec_of(&t.columns[i as usize])).collect())
            .collect();
        let sec_specs: Vec<Vec<Vec<KeySpec>>> = schema
            .tables
            .iter()
            .map(|t| {
                t.indexes
                    .iter()
                    .map(|ix| ix.columns.iter().map(|&i| spec_of(&t.columns[i as usize])).collect())
                    .collect()
            })
            .collect();
        let any_key_special: Vec<bool> = pk_specs
            .iter()
            .zip(&sec_specs)
            .map(|(pk, sec)| pk.iter().chain(sec.iter().flatten()).any(|s| !s.is_plain()))
            .collect();
        SchemaBundle {
            schema_gen: 0,
            schema,
            checks,
            sec_indexes,
            sec_unique,
            col_types,
            pk_specs,
            sec_specs,
            any_key_special,
        }
    }

    /// Key specs for the PK columns of `table_id`, in key order — or `&[]` when
    /// every key column of the table is plain, so the caller uses the bytewise
    /// encoder. `encode_key_spec(v, &[])` == `encode_key(v)`.
    #[inline]
    pub fn pk_coll(&self, table_id: u32) -> &[KeySpec] {
        let t = table_id as usize;
        if self.any_key_special.get(t).copied().unwrap_or(false) {
            &self.pk_specs[t]
        } else {
            &[]
        }
    }

    /// Key specs for secondary index `index_no` (`>= 1`) of `table_id`, in key
    /// order — or `&[]` when every key column of the table is plain.
    #[inline]
    pub fn index_coll(&self, table_id: u32, index_no: u32) -> &[KeySpec] {
        let t = table_id as usize;
        if index_no >= 1 && self.any_key_special.get(t).copied().unwrap_or(false) {
            self.sec_specs
                .get(t)
                .and_then(|v| v.get(index_no as usize - 1))
                .map(Vec::as_slice)
                .unwrap_or(&[])
        } else {
            &[]
        }
    }
}

pub struct Engine {
    shm: Arc<Shm>,
    /// The CURRENT schema bundle. Swapped whole by DDL / staleness reload;
    /// read paths clone the Arc once per transaction, never per operation.
    bundle: std::sync::RwLock<Arc<SchemaBundle>>,
    concurrency: Concurrency,
    /// Deferred-fsync flusher; `Some` only for `durability = async` (§5.4.2).
    flusher: Option<Flusher>,
    /// Payloads strictly larger than this take an extent run instead of an
    /// overflow chain (DESIGN-BLOBEXTENT §8). `None` = disabled — today's
    /// behavior byte for byte, and the A/B's control arm.
    extent_threshold: Option<usize>,
    /// Per-statement-execution work-row budget (#74), copied into each txn's
    /// [`WorkMeter`] at begin. `0` = unlimited.
    work_budget: u64,
    /// Live-cell budget on join materialization (#74 extension): the SQL
    /// executor's nested-loop join reads it via the txn and bounds the
    /// `Value` cells its intermediate product holds. `0` = unlimited.
    join_cells_budget: u64,
    /// Installed by the facade; recompiles CHECK programs whenever a bundle is
    /// (re)built from the catalog. `None` until then — and forever for the
    /// core's own tests, which declare no CHECKs.
    check_compiler: std::sync::RwLock<Option<Arc<CheckCompiler>>>,
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
        let shm = Arc::new(shm);
        // durability = async: a background thread coalesces fdatasync on a
        // bounded interval so commits ack without waiting for the flush
        // (crash-consistent, power-loss loses a bounded window — §5.4.2).
        let flusher = (config.options.durability == Durability::Async)
            .then(|| spawn_flusher(shm.clone()));
        let engine = Engine {
            shm,
            bundle: std::sync::RwLock::new(Arc::new(SchemaBundle::new(schema, checks))),
            concurrency: config.options.concurrency,
            flusher,
            extent_threshold: None,
            work_budget: config.options.max_work_rows,
            join_cells_budget: config.options.max_join_cells,
            check_compiler: std::sync::RwLock::new(None),
        };
        engine.bootstrap_catalog()?;
        // #47 stage 1: the FILE is authoritative for the schema. The config
        // seeded it (first open) or must hash-match the SEED (the frozen
        // M_SCHEMA_HASH, which never changes); the LIVE schema — which DDL
        // may have evolved past the seed — is read from the catalog. For a
        // never-evolved file the two are byte-identical and this is a no-op.
        engine.reload_schema_from_catalog()?;
        Ok(engine)
    }

    /// The current schema bundle — one Arc clone; transactions capture it at
    /// begin so their view is stable for their whole lifetime.
    pub fn bundle(&self) -> Arc<SchemaBundle> {
        self.bundle.read().expect("schema bundle lock poisoned").clone()
    }

    /// Refresh the cached schema if a DDL commit (this process or another)
    /// bumped the meta's `schema_gen` past this process's bundle (#47 stage
    /// 3). One `newest_meta` read (no pin); reloads from the catalog only on
    /// mismatch. The facade calls this BEFORE compiling fresh SQL so a query
    /// against a just-created table sees it — the txn-begin reload alone is
    /// too late, compilation runs first.
    pub fn refresh_schema_if_stale(&self) -> Result<()> {
        if self.shm.newest_meta()?.schema_gen != self.bundle().schema_gen {
            self.reload_schema_from_catalog()?;
        }
        Ok(())
    }

    /// The CURRENT schema (an Arc'd bundle; derefs to [`Schema`]). Callers
    /// that need a `&Schema` bind the Arc first — the schema may be swapped
    /// by DDL, so no plain reference can be handed out.
    pub fn schema(&self) -> Arc<SchemaBundle> {
        self.bundle()
    }

    /// Install the facade's CHECK compiler and immediately rebuild the current
    /// bundle's programs with it.
    ///
    /// The rebuild is the point, not a nicety: [`Engine::open`] has already
    /// loaded the LIVE schema from the catalog, which may carry CHECK sources
    /// the config's seed schema never had (a peer process ran `CREATE TABLE …
    /// CHECK (…)`). Without the rebuild this process would enforce the config's
    /// checks and silently skip the catalog's.
    pub fn set_check_compiler(&self, f: Arc<CheckCompiler>) -> Result<()> {
        *self.check_compiler.write().expect("check compiler lock poisoned") = Some(f);
        let cur = self.bundle();
        let checks = self.compile_checks(&cur.schema, &cur.checks);
        *self.bundle.write().expect("schema bundle lock poisoned") =
            Arc::new(SchemaBundle::new_at(cur.schema_gen, cur.schema.clone(), checks));
        Ok(())
    }

    /// CHECK programs for `schema`, via the installed compiler.
    ///
    /// The fallback — used before the facade installs one, and if compilation
    /// fails — carries `prev` forward by table position (ids are dense in this
    /// format window) and leaves a new table's slot empty. That is exactly what
    /// this code did before the compiler existed.
    ///
    /// A compile FAILURE cannot be reported from here: this runs inside a bundle
    /// swap that a reader/writer begin already committed to. It also cannot
    /// happen in practice — every CHECK source is compiled against its finished
    /// table before the DDL that stores it commits, so a source in the catalog
    /// is a source that compiled. If one ever did fail, the empty slot is the
    /// same "not yet compiled" state the pre-compiler code left, and the next
    /// `CREATE`/`ALTER` naming that column re-reports it.
    pub(super) fn compile_checks(&self, schema: &Schema, prev: &CheckPrograms) -> CheckPrograms {
        let compiler = self
            .check_compiler
            .read()
            .expect("check compiler lock poisoned")
            .clone();
        if let Some(f) = compiler {
            if let Ok(c) = f(schema) {
                debug_assert_eq!(c.len(), schema.tables.len());
                if c.len() == schema.tables.len() {
                    return c;
                }
            }
        }
        let mut checks = prev.clone();
        checks.resize(schema.tables.len(), Vec::new());
        checks
    }

    /// Replace the current bundle with the catalog's stored schema. The
    /// schema-gen staleness check (#47 stage 3) routes writers here after a
    /// DDL commit by another process; `&self` — in-flight transactions keep
    /// their captured bundle untouched.
    pub fn reload_schema_from_catalog(&self) -> Result<()> {
        let r = self.begin_read_unchecked()?;
        let gen = r.meta.schema_gen;
        let stored = r.stored_schema();
        r.finish()?;
        let stored = stored?;
        let cur = self.bundle();
        if stored == cur.schema && gen == cur.schema_gen {
            return Ok(());
        }
        // CHECK programs are compiled by the facade from check sources; the
        // installed compiler recompiles the WHOLE stored schema, so a table a
        // peer created with a CHECK is enforced here too and not merely stored.
        let checks = self.compile_checks(&stored, &cur.checks);
        *self.bundle.write().expect("schema bundle lock poisoned") =
            Arc::new(SchemaBundle::new_at(gen, stored, checks));
        Ok(())
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
        let bundle = self.bundle();
        let schema_bytes = bundle.schema.canonical_bytes();
        let out = btree::insert(
            &mut txn,
            0,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&schema_bytes),
            InsertMode::InsertOnly,
        )?;
        txn.catalog_root = out.new_root;
        for (tid, table) in bundle.schema.tables.iter().enumerate() {
            let mut index_nos = vec![0u32];
            index_nos.extend((1..=bundle.sec_indexes[tid].len()).map(|i| i as u32));
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
        let sec_indexes: Vec<Vec<Vec<u16>>> = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.columns.clone()).collect())
            .collect();
        let _ = sec_indexes; // derived inside SchemaBundle::new
        let checks = vec![Vec::new(); schema.tables.len()];
        Ok(Engine {
            shm: Arc::new(shm),
            bundle: std::sync::RwLock::new(Arc::new(SchemaBundle::new(schema, checks))),
            concurrency: Concurrency::Serial,
            flusher: None, // read-only tooling handle; async needs a config
            extent_threshold: None,
            // Tooling handle (no config): the budgets are unlimited (a `dump`/
            // verify pass legitimately scans whole tables).
            work_budget: 0,
            join_cells_budget: 0,
            // Read-only tooling: no SQL layer to install a compiler, and no
            // writes for a CHECK to guard.
            check_compiler: std::sync::RwLock::new(None),
        })
    }

    /// The per-statement-execution work-row budget (#74) this engine was opened
    /// with (`0` = unlimited). The facade reads it to relate a prepare-time risk
    /// estimate to the runtime cap.
    pub fn work_budget(&self) -> u64 {
        self.work_budget
    }

    /// The join-materialization live-cell budget this engine was opened with
    /// (`0` = unlimited). The SQL executor's nested-loop join charges against
    /// it per cell its intermediate product holds.
    pub fn join_cells_budget(&self) -> u64 {
        self.join_cells_budget
    }

    /// The write-path concurrency discipline this engine was opened with
    /// (design/DESIGN-PHASE3.md). Serial is the default and shipped behavior.
    pub fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    /// Whether `table_id` has any secondary (unique) index. Optimistic
    /// blind-apply is only eligible for tables without one — index
    /// maintenance needs the current row and degrades footprints below the
    /// key level (design/DESIGN.md §7.3 honesty rule).
    pub fn has_secondary_index(&self, table_id: u32) -> bool {
        self.bundle()
            .sec_indexes
            .get(table_id as usize)
            .is_some_and(|s| !s.is_empty())
    }

    /// Whether `table_id` is an FTS virtual table. An FTS table has an inverted
    /// index maintained by the row-mutation path, so — like a table with a
    /// secondary index — it must NOT take the optimistic blind-apply route,
    /// which would skip that maintenance (see `optimistic_eligible`).
    pub fn table_is_fts(&self, table_id: u32) -> bool {
        self.bundle()
            .schema
            .table(table_id)
            .is_some_and(|t| t.kind.is_fts())
    }

    /// Validate a full row against the schema (types, NOT NULL, CHECK) without
    /// mutating anything — used by the optimistic prep pass off the writer
    /// lock. Public wrapper over the internal validator.
    pub fn validate_row_public(&self, table_id: u32, values: &[Value]) -> Result<()> {
        self.validate_row(table_id, values)
    }

    /// Column types for `table_id` (for off-lock row encoding in optimistic
    /// prep). Cloned out of the current bundle — a rare, cold path.
    pub fn col_types(&self, table_id: u32) -> Option<Vec<ColumnType>> {
        self.bundle().col_types.get(table_id as usize).cloned()
    }

    /// The on-disk PK-tree key for an already-projected PK tuple.
    ///
    /// Callers OUTSIDE the engine that need a raw B+tree key — the intent
    /// ring's conflict/apply keys, a streaming cursor's resume bound — must go
    /// through this rather than `keycode::encode_key`, which is only the
    /// encoding of a table whose every key column is plain. A collated key
    /// column folds its text, and a typeless (`any`) key column encodes by
    /// STORAGE CLASS; hand-rolling `encode_key` there produces a key the tree
    /// does not use, so a lookup misses and a write lands in the wrong slot.
    pub fn pk_key(&self, table_id: u32, pk_values: &[Value]) -> Vec<u8> {
        keycode::encode_key_spec(pk_values, self.bundle().pk_coll(table_id))
    }

    /// Verify the page-accounting invariant (design/DESIGN.md §4.5): every page in
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
    /// Enable/disable the extent path (DESIGN-BLOBEXTENT §8). Values whose
    /// encoded payload exceeds the threshold take an extent run; `None`
    /// keeps every value on the inline/overflow path.
    pub fn set_extent_threshold(&mut self, threshold: Option<usize>) {
        self.extent_threshold = threshold;
    }

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
                if k.len() == 11 && k[8] == FK_PAGES {
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
            btree::collect_pages(&txn, txn.extent_map_root, &mut reachable)?;

            // Extent refs from every data tree must equal the extent map
            // EXACTLY: an unmapped ref dangles, an unreferenced map entry
            // leaks, a duplicated start means two cells own one run.
            let mut refs: Vec<(u64, u32)> = Vec::new();
            {
                let lo = [0x01u8];
                let hi = [SYS_PREFIX];
                let mut c = btree::cursor(
                    &txn,
                    txn.catalog_root,
                    Some((&lo[..], true)),
                    Some((&hi[..], false)),
                )?;
                let mut roots2 = Vec::new();
                while let Some((_k, v)) = c.next(&txn)? {
                    if v.len() == 16 {
                        roots2.push(u64::from_le_bytes(v[0..8].try_into().unwrap()));
                    }
                }
                for r in roots2 {
                    btree::collect_extents(&txn, r, &mut refs)?;
                }
            }
            refs.sort_unstable();
            if refs.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(Error::Corrupt("two cells reference one extent".into()));
            }
            let mut mapped: Vec<(u64, u32)> = Vec::new();
            if txn.extent_map_root != 0 {
                let mut c = btree::cursor(&txn, txn.extent_map_root, None, None)?;
                while let Some((k, v)) = c.next(&txn)? {
                    if k.len() != 8 || v.len() != 4 {
                        return Err(Error::Corrupt("bad extent map entry".into()));
                    }
                    mapped.push((
                        u64::from_be_bytes(k.try_into().unwrap()),
                        u32::from_le_bytes(v.try_into().unwrap()),
                    ));
                }
            }
            if refs != mapped {
                return Err(Error::Corrupt(format!(
                    "extent map disagrees with tree references \
                     ({} mapped, {} referenced)",
                    mapped.len(),
                    refs.len()
                )));
            }

            let mut freelisted = std::collections::BTreeSet::new();
            let mut free_runs: Vec<(u64, u32)> = Vec::new();
            if txn.freelist_root != 0 {
                let mut c = btree::cursor(&txn, txn.freelist_root, None, None)?;
                while let Some((k, v)) = c.next(&txn)? {
                    if k.len() != 11 {
                        return Err(Error::Corrupt("bad freelist key length".into()));
                    }
                    if k[8] == FK_RUNS {
                        free_runs.extend(extent::decode_run_entry(
                            &v,
                            self.shm.data_start,
                            self.shm.page_count,
                        )?);
                        continue;
                    }
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
            // Interval discipline: live runs and free runs together must be
            // pairwise DISJOINT — partial overlap is the corruption class a
            // set-insert check cannot see (DESIGN-BLOBEXTENT §3.2).
            let mut runs: Vec<(u64, u32)> = mapped.iter().chain(free_runs.iter()).copied().collect();
            runs.sort_unstable();
            for w in runs.windows(2) {
                if w[0].0 + u64::from(w[0].1) > w[1].0 {
                    return Err(Error::Corrupt(format!(
                        "extent runs overlap: {}+{} and {}+{}",
                        w[0].0, w[0].1, w[1].0, w[1].1
                    )));
                }
            }
            let in_runs = |id: u64| -> bool {
                let i = runs.partition_point(|&(s, _)| s <= id);
                i > 0 && {
                    let (s, n) = runs[i - 1];
                    id < s + u64::from(n)
                }
            };
            for &id in &freelisted {
                if in_runs(id) {
                    return Err(Error::Corrupt(format!(
                        "page {id} both freelisted and inside a run"
                    )));
                }
            }
            for &id in &reachable {
                if in_runs(id) {
                    return Err(Error::Corrupt(format!(
                        "page {id} both tree-reachable and inside a run"
                    )));
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
                if !reachable.contains(&id) && !freelisted.contains(&id) && !in_runs(id) {
                    return Err(Error::Corrupt(format!(
                        "page {id} leaked: neither reachable, freelisted, nor in a run"
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
        let txn = self.begin_read_unchecked()?;
        // #47 stage 3: a DDL commit bumped the meta's schema_gen past our
        // bundle's — reload from the catalog (swap) and re-capture, so this
        // reader sees the schema its pinned snapshot actually has.
        if txn.meta.schema_gen != txn.bundle.schema_gen {
            txn.finish()?;
            self.reload_schema_from_catalog()?;
            return self.begin_read_unchecked();
        }
        Ok(txn)
    }

    /// `begin_read` without the schema-gen staleness check — used BY the
    /// reload itself (its inner read would otherwise recurse forever on the
    /// very mismatch it exists to repair).
    fn begin_read_unchecked(&self) -> Result<ReadTxn<'_>> {
        let (slot, word, meta) = self.shm.claim_and_pin()?;
        Ok(ReadTxn {
            eng: self,
            bundle: self.bundle(),
            slot,
            word,
            meta,
            released: false,
            work: WorkMeter::new(self.work_budget),
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

    /// Bounded-wait variant of [`Engine::begin_write`] (#109): poll the writer
    /// lock until `deadline`, then fail with [`Error::Busy`]. `None` delegates
    /// to the blocking `begin_write` (identical semantics, including the
    /// same-thread EDEADLK diagnostic). A deadline at-or-before now still makes
    /// exactly ONE acquisition attempt — sqlite's "timeout 0 = immediate BUSY
    /// on contention, immediate acquire otherwise".
    ///
    /// This sits entirely OUTSIDE the §5.3 intent-ring protocol and §4.2 lock
    /// attributes: it only changes how long we WAIT for the lock, via the same
    /// `try_writer_lock` the ring's wait-or-lead loop uses. Owner-death
    /// recovery is unchanged — `try_writer_lock` adopts EOWNERDEAD (Linux) /
    /// the DIRTY word (macOS flock), so a SIGKILLed holder converts a waiter's
    /// next poll into an acquire, never a hang past the deadline.
    pub fn begin_write_deadline(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> Result<WriteTxn<'_>> {
        let Some(deadline) = deadline else {
            return self.begin_write();
        };
        // Escalating poll: 100 µs doubling to a 5 ms cap. Fine-grained enough
        // that a released lock is picked up promptly (and an elapsed-time
        // assertion lands just past the timeout), coarse enough that a full
        // 5 s CPython-default wait costs ~1k syscalls, not a spin.
        let mut step = std::time::Duration::from_micros(100);
        loop {
            match self.try_begin_write() {
                Ok(Some(txn)) => return Ok(txn),
                Ok(None) => {}
                // The lock is held by THIS VERY THREAD (glibc's ROBUST+
                // ERRORCHECK trylock answers the owner EDEADLK; the contract
                // mpedb-capi's `is_writer_lock_reentry` also matches). On
                // macOS trylock answers EBUSY for the same relock
                // (rdar://16261552), so this arm is dead there and a
                // same-thread sibling waits out its full deadline — bounded,
                // just not immediate: waiting cannot succeed, the
                // owner IS the caller. Answer Busy now — deterministic
                // deadlock avoidance with the same terminal result sqlite
                // reaches only after burning the whole timeout. The
                // deadline-None path keeps the loud EDEADLK diagnostic.
                Err(Error::Internal(m)) if m.contains("writer lock re-entered") => {
                    return Err(Error::Busy);
                }
                Err(e) => return Err(e),
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(Error::Busy);
            }
            std::thread::sleep(step.min(deadline - now));
            step = (step * 2).min(std::time::Duration::from_millis(5));
        }
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
        // #47 stage 3: another process's DDL commit bumped schema_gen —
        // reload before this txn captures its bundle (under the writer
        // lock, so the reloaded schema cannot move again beneath us).
        //
        // The error arm MUST unlock, exactly as newest_meta's above: the
        // reload takes a read pin internally, so a full reader table
        // (ReadersFull — a designed-for, retryable state) reaches here, and
        // propagating it with the lock held leaks the writer lock for the
        // process's lifetime. Repro'd during the #109 review: every later
        // same-thread write became EDEADLK — or, under a busy policy, a
        // silent terminal Busy — and every OTHER process burned its full
        // timeout per statement until this process died.
        if meta.schema_gen != self.bundle().schema_gen {
            if let Err(e) = self.reload_schema_from_catalog() {
                self.shm.writer_unlock();
                return Err(e);
            }
        }
        // Private :memory: with no reader pins → in-place leaf mutation (no
        // COW freelist tax). Exclusive flag blocks new pins for the txn life.
        let in_place = self.shm.try_begin_exclusive_write();
        Ok(WriteTxn {
            eng: self,
            bundle: self.bundle(),
            meta,
            catalog_root: meta.catalog_root,
            freelist_root: meta.freelist_root,
            extent_map_root: meta.extent_map_root,
            schema_gen_bump: false,
            run_pool: Vec::new(),
            taken_runs: Vec::new(),
            freed_runs: Vec::new(),
            allocated_runs: std::collections::HashMap::new(),
            pending_map_edits: Vec::new(),
            extent_dirty: Vec::new(),
            extent_buf: Vec::new(),
            extent_buf_off: 0,
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
            work: WorkMeter::new(self.work_budget),
            in_place,
            inplace_undo: HashMap::new(),
        })
    }

    // ---------- row-level helpers shared by both txn kinds ----------

    /// The primary-key encoded key for `values`, resolved against an EXPLICIT
    /// bundle — a writer passes its own captured bundle so a table it created
    /// earlier in the SAME (uncommitted) transaction resolves, even though the
    /// engine's committed bundle does not yet know it (#95). Identical to the
    /// committed bundle on every path that is not in-transaction DDL.
    pub(super) fn pk_key_in(
        &self,
        bundle: &SchemaBundle,
        table_id: u32,
        values: &[Value],
    ) -> Result<Vec<u8>> {
        let table = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let pk_vals: Vec<Value> = table
            .primary_key
            .iter()
            .map(|&i| values[i as usize].clone())
            .collect();
        Ok(keycode::encode_key_spec(&pk_vals, bundle.pk_coll(table_id)))
    }

    /// Validate a full row against the schema: arity, rigid types, NOT NULL,
    /// CHECK (SQL semantics: violated only when the predicate is FALSE —
    /// NULL/UNKNOWN passes).
    fn validate_row(&self, table_id: u32, values: &[Value]) -> Result<()> {
        // The current bundle: safe for every caller — writers hold the
        // writer lock (which serializes DDL), and the optimistic prep pass
        // re-validates under the lock.
        self.validate_row_in(&self.bundle(), table_id, values)
    }

    /// [`validate_row`](Self::validate_row) against an EXPLICIT bundle — a
    /// writer validates against its OWN captured bundle so a row for a table
    /// created/altered earlier in the same uncommitted transaction is checked
    /// against the shape it will commit with, and its CHECK programs, arity and
    /// NOT NULL flags come from that same consistent view (#95). Identical to
    /// `validate_row` when the captured and committed bundles agree.
    pub(super) fn validate_row_in(
        &self,
        bundle: &SchemaBundle,
        table_id: u32,
        values: &[Value],
    ) -> Result<()> {
        let table = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
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
                // The row has already been through the column's store-time
                // affinity, so on a converting column (task #113) this value is
                // what sqlite's conversion LEFT — naming that is the difference
                // between "float64 vs int64" and "7.5 is not an integer, and
                // sqlite would have kept the real".
                let why = if c.converts_on_store() {
                    format!(
                        " (sqlite's {} affinity left it a {}, which this column cannot hold)",
                        c.affinity.name(),
                        v.type_name()
                    )
                } else {
                    String::new()
                };
                return Err(Error::TypeMismatch(format!(
                    "column `{}.{}` is {}, value is {}{}",
                    table.name,
                    c.name,
                    c.ty,
                    v.type_name(),
                    why
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
        for (ci, check) in bundle.checks[table_id as usize].iter().enumerate() {
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
