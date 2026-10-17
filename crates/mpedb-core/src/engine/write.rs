mod concurrency;
mod schema;
mod savepoint;

use super::*;
use super::freelist::TakenEntry;

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
pub(super) type DirtySet = HashSet<u64, BuildHasherDefault<PageIdHasher>>;

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

/// An optimistic guard on a write transaction (#142 G1).
///
/// The contract in one line: **this transaction commits only if nothing in
/// `surface` was written by a commit after `snap_txn`.** Otherwise
/// `Error::WriteConflict`, and the caller re-reads and retries.
///
/// It is not a lock. Nothing is held, nothing must be released, and a
/// SIGKILLed holder costs exactly nothing — a process that died simply did not
/// commit. In an engine whose premise is that processes may be killed at any
/// instant, that is worth more than mutual exclusion, and it is why the task
/// queue's claim was already written this way (`queue.rs`'s guarded UPDATE).
///
/// What it does NOT provide is exclusivity of the WORK: two actors may both
/// compute, and only one commits. Where the work has external side effects
/// that cannot run twice, a lease is required instead (#142 G2).
#[derive(Debug, Clone)]
pub struct Guard {
    pub snap_txn: u64,
    /// Key-region summary of everything this action has actually touched
    /// (#143). `None` until a statement runs; once any statement cannot name
    /// its key it becomes `REGION_ANY` and stays there.
    ///
    /// Regions accumulate from EXECUTION while the table set is DECLARED, and
    /// the asymmetry is deliberate. The table set must be stable before
    /// anything runs, because that is the shard's identity. A key is only known
    /// once its parameters are bound, and a key never touched cannot have been
    /// invalidated — so narrowing by what actually ran is sound where narrowing
    /// the table set would not be.
    pub regions: u64,
    /// The same keys, EXACTLY, or `None` once there were more than
    /// `OFP_MAX_KEYS` of them or one could not be named (#149).
    ///
    /// The Bloom above is a 64-bit summary with one bit per key, so two
    /// unrelated rows share a bit 1/64 of the time — which at eight concurrent
    /// rows is a coin flip, and a permanent one for the pair that collides.
    /// Carrying the exact set alongside costs a `Vec` per guarded transaction
    /// and removes every false conflict the summary would have invented.
    pub keys: Option<Vec<u64>>,
    /// The byte range inside the value this action rewrites (#151), or `None`
    /// for "the whole value".
    ///
    /// Accumulated as (min lo, max hi) — the union, which is conservative:
    /// two splices at opposite ends of one cell declare the span between them
    /// and conflict with anything inside it. Exact per-splice ranges would need
    /// a list, and one action rarely splices twice.
    pub range: Option<(u32, u32)>,
    /// A rebase walk already decided the range dimension for this action
    /// (#151), so the commit-time test skips it.
    pub rebased: bool,
    /// The shard this action runs under (#144), or `None` when it cannot be
    /// proven — no session context, or a table without a policy on that key.
    ///
    /// Set once and only narrowed to `None`: statements must AGREE on the
    /// shard. Two statements under different tenants are not one shard, and
    /// letting the last one win would be exactly the wrong-answer direction.
    pub shard: Option<u64>,
    /// Columns this action touched, folded `& 63` (#146 K1). Starts at 0 and
    /// only ever grows; 0 at commit means "nothing named a column" and is
    /// widened to [`COLS_ANY`] there rather than here, so the growing-only rule
    /// stays simple.
    pub cols: u64,
    /// Has any statement run yet? Distinguishes "no shard proven" from "no
    /// statement has spoken", which would otherwise both read as `None`.
    pub shard_seen: bool,
    /// Union of every statement's footprint, folded `& 63` to match the OPT
    /// ring. Folding costs false conflicts (table 7 aliases 71), never a
    /// missed one — the same trade the ring already makes.
    pub surface: u64,
}

pub struct WriteTxn<'e> {
    pub(super) eng: &'e Engine,
    /// Schema view captured at begin (#47). For writers this always equals
    /// the engine's current bundle (the writer lock serializes DDL), but
    /// capturing keeps one rule for both txn kinds.
    pub(super) bundle: Arc<SchemaBundle>,
    pub meta: MetaSnapshot,
    pub(super) catalog_root: u64,
    pub(super) freelist_root: u64,
    pub(super) high_water: u64,
    /// Root of the extent map (DESIGN-BLOBEXTENT §3.2). Carried through the
    /// commit snapshot; mutated once the extent allocator lands.
    pub(super) extent_map_root: u64,
    /// #47: set by a DDL mutation in this txn; the commit snapshot then
    /// writes `meta.schema_gen + 1`, signalling every other process to
    /// reload its schema from the catalog.
    pub(super) schema_gen_bump: bool,
    /// (table_id, index_no) → (root, row_count); loaded lazily, written back
    /// into the catalog at commit.
    pub(super) table_roots: HashMap<(u32, u32), (u64, u64)>,
    pub(super) dirty: DirtySet,
    /// Pages allocated by this transaction, ever. Monotone; only ever compared
    /// against a value captured in a savepoint, so it is a change DETECTOR and
    /// not a count of anything live. Read it with `allocs()`; #160 is what it
    /// is for.
    pub(super) allocs: u64,
    /// Pages this txn may allocate. They are **still listed in the freelist**:
    /// `refill_reusable` reads entries, it does not remove them (design/DESIGN.md
    /// §4.5). `taken` remembers where each came from so the commit fixpoint can
    /// strike out exactly the ones that got consumed.
    pub(super) reusable: Vec<u64>,
    /// Freelist entries drawn from, in draw order. Drawing is FREE — an entry
    /// nobody allocated out of is left untouched at commit. Only consumption
    /// costs a write. That decoupling is the fix for #37.
    pub(super) taken: Vec<TakenEntry>,
    /// Last key `refill_reusable` drew from; the next draw starts strictly
    /// after it, so an entry is never drawn twice (it is still in the tree).
    pub(super) refill_cursor: Option<[u8; 11]>,
    pub(super) freed: BTreeSet<u64>,
    pub(super) bound_recomputed: bool,
    /// True while a mutation of the freelist tree itself is in progress.
    /// `alloc` must NOT trigger `refill_reusable` then: the refill deletes a
    /// freelist entry via `btree::delete` on the same tree the in-progress
    /// mutation is rewriting — two interleaved mutations with different root
    /// snapshots lose updates and hand out live pages (double allocation,
    /// "double free"/"listed twice" corruption seen in multi-process stress).
    pub(super) in_freelist_op: bool,
    // ---- extents (DESIGN-BLOBEXTENT) ----
    /// Private run pool, sorted by start, with per-run provenance.
    pub(super) run_pool: Vec<extent::PoolRun>,
    /// Drawn kind-1 freelist entries (left in the tree; struck out at commit
    /// for what was consumed — write-back = pool runs still attributed).
    pub(super) taken_runs: Vec<extent::TakenRunEntry>,
    /// Committed extents freed by this txn — the commit's OWN kind-1 set.
    pub(super) freed_runs: Vec<(u64, u32)>,
    /// Runs allocated by THIS txn (start -> npages): a free of one of these
    /// returns to the pool with `from: None` (the attribution rule), never
    /// to `freed_runs` and never to a drawn entry's write-back.
    pub(super) allocated_runs: std::collections::HashMap<u64, u32>,
    /// Extent-map edits in CHRONOLOGICAL order, applied in ONE place at
    /// commit (never from inside a data-tree operation — no nested tree
    /// mutation). Order matters: a same-txn alloc→free→realloc touches the
    /// same start three times, and grouping inserts before deletes would
    /// leave the final state wrong.
    pub(super) pending_map_edits: Vec<super::extent::MapEdit>,
    /// Runs pwritten this txn — commit's range-sync list. On Linux these
    /// msyncs ARE the pre-flip durability (the barrier is a no-op there).
    pub(super) extent_dirty: Vec<(u64, u32)>,
    /// Coalescing buffer for SMALL extent payloads: consecutive high-water
    /// allocations are contiguous, so a whole batch becomes one pwrite
    /// instead of one per row (the 4 KiB cell's syscall tax, measured 0.93×
    /// before this). Bounded (§ EXTENT_BUF_CAP); flushed on discontiguity,
    /// on any extent READ in this txn (the mapping must see the bytes), and
    /// at commit before the range-syncs — payload-before-reference holds
    /// because nothing publishes until the flip.
    pub(super) extent_buf: Vec<u8>,
    /// File offset where `extent_buf` starts; meaningless when buf is empty.
    pub(super) extent_buf_off: u64,

    /// Robust-mutex recovery ran when this txn acquired the lock.
    pub recovered: bool,
    pub(super) finished: bool,
    /// Bitmap of user tables whose data this txn mutated (set in
    /// `set_tree_root`). Recorded into the committed-footprint ring at commit
    /// in optimistic mode; unused (and free) in serial mode.
    pub(super) written_tables: u64,
    /// Tables whose ROWS this transaction mutated — the exact set, unlike
    /// [`Self::written_tables`], which folds mod 64 and would therefore MISS a
    /// table with id ≥ 64. A missed table would leave a stale columnar segment
    /// readable, which is a wrong answer, so this one cannot be lossy
    /// (design/DESIGN-COLUMNAR.md §6).
    ///
    /// **Monotonic within the transaction: a savepoint rollback does NOT
    /// shrink it.** Over-bumping a generation only invalidates a segment that
    /// was in fact still valid — slower, never wrong — while under-bumping
    /// would publish a stale segment as fresh. The asymmetry decides the
    /// design: this set only ever grows until the txn ends.
    pub(super) mutated_tables: std::collections::BTreeSet<u32>,
    /// Per-table cache of the columnar WATERMARK PK, loaded lazily on the first
    /// mutation of a table (DESIGN-COLUMNAR §7). `None` value = the table has no
    /// watermark record (the overwhelmingly common case — one point `sys_get`
    /// per mutated table per txn, then free). `Some(pk)` = a covered mutation
    /// (key ≤ pk) must delete the record. NOT snapshotted by `savepoint`: the
    /// boundary is stable within a txn, and the delete is re-issued idempotently
    /// on every covered mutation, so a rollback that RESTORES a deleted record is
    /// re-invalidated by the next covered write — never a stale segment read.
    pub(super) colwm_cache: HashMap<u32, Option<Vec<u8>>>,
    /// Set by the optimistic blind-apply path to record a precise
    /// (table, key_hash) point footprint at commit instead of a table-level
    /// one. `None` for every other path.
    pub(super) commit_point: Option<(u32, u64)>,
    /// The optimistic guard (#142 G1), when this txn was begun guarded.
    ///
    /// `snap_txn` is what the caller had observed; `surface` is the union of
    /// every statement's footprint run through the session, folded to the same
    /// `& 63` bitmap the OPT ring records — READS included, because the SELECT
    /// that decided is as much a part of the action as the INSERT that acted.
    /// Validated once in `commit_inner`, under the writer lock, before the
    /// meta flip.
    pub(super) guard: Option<Guard>,
    /// Key-region summary of what this txn WROTE, published to the commit ring
    /// so later guards can tell "same table" from "same row" (#143).
    pub(super) written_regions: u64,
    /// The exact keys this txn wrote, or `None` once it could not name them
    /// all (#149). Published alongside the summary so a later guard can answer
    /// exactly instead of through a 64-bit Bloom.
    pub(super) written_keys: Option<Vec<u64>>,
    /// The byte range this txn rewrote (#151), `None` = the whole value.
    pub(super) written_range: Option<(u32, u32)>,
    /// How much this txn changed the value's LENGTH (#151) — what lets a later
    /// pending edit carry its offset across this commit.
    pub(super) written_delta: i64,
    /// The shard this txn's writes all belong to (#144), or `None` if they do
    /// not all belong to one provable shard.
    /// Columns this txn WROTE, same folding and same widening rule.
    pub(super) written_cols: u64,
    pub(super) written_shard: Option<u64>,
    pub(super) written_shard_seen: bool,
    /// Per-table key REGION for change notification (#139 S2, widened by
    /// #141 N2): the common `keycode` byte prefix of every key this
    /// transaction is known to have touched in that table. `None` means
    /// "unknown" — a listener filtering on a key must then look.
    pub(super) notify_keys: std::collections::HashMap<u32, Option<Vec<u8>>>,
    /// CDC dirty-set capture is on for this txn (default). The replication
    /// plane (mirror applier/importer) turns it OFF via [`WriteTxn::set_capture`]
    /// so its own writes are not self-captured (DESIGN-MIRROR §3.8). Transient:
    /// never persisted, dies with the txn.
    pub(super) capture_enabled: bool,
    /// Lazily-loaded `cdc\0tabs` control record, cached for the txn's lifetime
    /// (capture enablement is set in a separate txn, so it is stable here).
    pub(super) capture_cfg: Option<CaptureConfig>,
    /// This txn may allocate from the reserved control-page band (§3.10). Set by
    /// the mirror for control-only commits; default off.
    pub(super) reserved_alloc: bool,
    /// Deterministic per-execution work-row meter (#74). Scans charge it here;
    /// the SQL executor charges the same meter via [`WriteTxn::charge_work`] for
    /// DML that runs a correlated subquery or a nested-loop join.
    pub(super) work: WorkMeter,
    /// Private `:memory:` with no concurrent pins: COW adopts pages in place
    /// (no freelist free of the old image). Cleared with exclusive_write on
    /// commit/abort.
    pub(super) in_place: bool,
    /// Pre-mutation images for [`Self::in_place`] pages — restored on abort so
    /// a failed autocommit cannot leave half-written committed pages.
    pub(super) inplace_undo: HashMap<u64, Box<[u8; PAGE_SIZE]>>,
    /// How many COMMITTED pages this txn has ADOPTED in place, ever (the
    /// private `:memory:` mode's third page state). Monotone like [`allocs`]:
    /// only its DIFFERENCE against a savepoint's capture is ever asked, in
    /// [`Self::undo_is_exact`] — adoption mutates a page under its ORIGINAL
    /// id, which the root-pointer restore cannot undo, so a savepoint with
    /// adoptions after it is never exactly undoable by the cheap pair.
    pub(super) adopts: u64,
    /// Lazily-filled pre-image journals for the outstanding FULL savepoints
    /// (#N2 fase 4): layer k belongs to the k-th open [`savepoint_full`],
    /// and [`PageStore::page_mut`] records a dirty page's bytes into the
    /// INNERMOST layer at its first mutation after that savepoint. Folding
    /// on release keeps outer savepoints exact — the parent inherits the
    /// child's image for every page it has not seen itself. Empty whenever
    /// no full savepoint is open, which is the hot path's whole cost: one
    /// emptiness check per `page_mut`.
    pub(super) sp_layers: Vec<SpLayer>,
    /// Epochs name layers so a stale [`TxnSavepointFull`] token can never
    /// address a recycled slot — depth alone would, silently.
    pub(super) sp_epoch_next: u64,
    /// Own-created extent runs freed while a FULL savepoint is open (N5,
    /// found pre-existing by the fase-4 adversarial review): a freed run
    /// re-entering `run_pool` can be recycled and its payload pwritten over
    /// — bytes no page journal sees, because extent payloads never enter
    /// `dirty` — and a ROLLBACK TO then restores the allocator map pointing
    /// at the clobbered offsets: a silent committed wrong answer (the
    /// coalescing buffer masks it only ≤ 256 KiB). Parked runs are not
    /// allocatable; they drain back into the pool when the last layer
    /// closes ([`release_savepoint_full`]) or at commit, and they ride the
    /// [`ExtentSnapshot`] so a rollback restores them exactly.
    pub(super) parked_runs: Vec<(u64, u32)>,
}

/// One lazily-filled pre-image journal layer (see [`WriteTxn::sp_layers`]).
pub(super) struct SpLayer {
    pub(super) epoch: u64,
    pub(super) images: HashMap<u64, Box<[u8; PAGE_SIZE]>>,
}

/// Where a payload too big for the tree ended up.
///
/// Three insert paths make this same choice — a streaming insert, a flat one,
/// and an upsert — and having each spell it out as an `Option` of a tuple is
/// how the three would come to disagree about a fourth case. One name, one
/// mapping to a `Payload`, three callers.
enum BigRef {
    None,
    Extent { start_page: u64, total_len: u64, npages: u32 },
    External { hash: u128, total_len: u64 },
}

impl<'e> WriteTxn<'e> {
    /// Put a payload outside the file as its own content-named object.
    ///
    /// Nothing is allocated and no map edit is recorded: the arena never learns
    /// this payload existed, which is the point — the page accounting has
    /// nothing to answer for and the freelist has nothing to reclaim.
    fn write_external_payload(&mut self, pieces: &[&[u8]], total_len: u64) -> Result<BigRef> {
        let mut writer = self.eng.shm.external_begin()?;
        for piece in pieces {
            writer.write(piece)?;
        }
        let (hash, written) = writer.finish()?;
        if written != total_len {
            return Err(Error::Internal(
                "external payload length disagrees with the encoded row".into(),
            ));
        }
        Ok(BigRef::External { hash, total_len })
    }

    /// The key `row` occupies in secondary index `i` of table position `tid`, or
    /// `None` when the row is not a MEMBER of that index.
    ///
    /// The single place membership is decided, for every write path. There are
    /// two ways out of an index and both must produce the same `None`:
    ///
    /// * any indexed column is NULL (SQL's own index-membership rule), and
    /// * the row fails a partial index's `WHERE` predicate.
    ///
    /// Every caller already treats `None` as "no entry", so folding the second
    /// rule in here is what makes insert, delete, update and the initial build
    /// agree by construction. Three copies of this test would drift, and the
    /// one that drifted would leave an index entry the table does not justify.
    ///
    /// **`IndexState` is deliberately NOT consulted here.** A `Building` index
    /// is maintained exactly like a `Ready` one — that is the whole point of
    /// publishing it before it is filled: rows written during the build get
    /// their entries from the ordinary write paths, and the background fill
    /// only has to cover the rows that already existed. Skipping maintenance
    /// for a `Building` index would leave it missing precisely the rows
    /// written while it was being built, and nothing afterwards would notice.
    /// The state is read by the PLANNER (`IndexDef::usable_for_access`), which
    /// is a question about answering queries, not about staying correct.
    fn index_entry_key(
        &self,
        tid: usize,
        i: usize,
        row: &[Value],
        pk_key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let table = &self.bundle.schema.tables[tid];
        let ix = &table.indexes[i];
        if !super::index_predicate_admits(
            self.bundle.index_predicates.get(tid).and_then(|v| v.get(i)),
            ix.predicate.as_ref(),
            &table.name,
            row,
        )? {
            return Ok(None);
        }
        // A plain-column index reads the row directly. An EXPRESSION index
        // (v13) first evaluates each expression part into a synthetic row, so
        // exactly one key builder serves both — and the any-NULL membership rule
        // then applies to the value the key would actually carry.
        if ix.exprs.is_empty() {
            return Ok(index_row_key(
                self.bundle.sec_unique[tid][i],
                &self.bundle.sec_indexes[tid][i],
                row,
                pk_key,
                &self.bundle.sec_specs[tid][i],
            ));
        }
        let progs = self
            .bundle
            .index_exprs
            .get(tid)
            .and_then(|v| v.get(i))
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "index on `{}` has expression key parts but no compiled programs                      (this handle has no SQL layer installed). Refusing rather than                      writing a key built without them.",
                    table.name
                ))
            })?;
        let mut parts = Vec::with_capacity(ix.exprs.len());
        for (k, e) in ix.exprs.iter().enumerate() {
            match e {
                None => parts.push(row[self.bundle.sec_indexes[tid][i][k] as usize].clone()),
                Some(_) => {
                    let prog = progs.get(k).and_then(Option::as_ref).ok_or_else(|| {
                        Error::Unsupported(format!(
                            "index on `{}`: key part {k} has no compiled expression",
                            table.name
                        ))
                    })?;
                    let mut stack = Vec::new();
                    parts.push(prog.eval_with_stack(&mut stack, row, &[])?);
                }
            }
        }
        // The synthetic row is the evaluated parts in key order, so the ordinals
        // handed to the builder are simply 0..n.
        let ords: Vec<u16> = (0..parts.len() as u16).collect();
        Ok(index_row_key(
            self.bundle.sec_unique[tid][i],
            &ords,
            &parts,
            pk_key,
            &self.bundle.sec_specs[tid][i],
        ))
    }

    pub(super) fn tree_root(&mut self, table_id: u32, index_no: u32) -> Result<(u64, u64)> {
        if let Some(&e) = self.table_roots.get(&(table_id, index_no)) {
            if std::env::var_os("SP_TRACE").is_some() {
                eprintln!(
                    "[sp] tree_root(t{table_id},i{index_no}) CACHE -> (root={}, count={})",
                    e.0, e.1
                );
            }
            return Ok(e);
        }
        let (root, count, _gen) = catalog_entry(self, self.catalog_root, table_id, index_no)?;
        if std::env::var_os("SP_TRACE").is_some() {
            eprintln!(
                "[sp] tree_root(t{table_id},i{index_no}) FALLBACK catalog_root={} -> (root={root}, count={count})",
                self.catalog_root
            );
        }
        self.table_roots.insert((table_id, index_no), (root, count));
        Ok((root, count))
    }

    pub(super) fn set_tree_root(&mut self, table_id: u32, index_no: u32, root: u64, count: u64) {
        if std::env::var_os("SP_TRACE").is_some() {
            eprintln!("[sp] set_tree_root(t{table_id},i{index_no}) root={root} count={count}");
        }
        // `& 63`: deliberate mod-64 fold, unchanged by the sparse-footprint work
        // (DESIGN-TABLE-CAP §5). A given table always folds to the same bit, so
        // a real conflict is never missed; aliasing only ever costs an extra
        // optimistic re-validation.
        self.written_tables |= 1u64 << (table_id & 63);
        // The exact, non-lossy twin of the bitmap above — see `mutated_tables`.
        // This is the single choke-point every row mutation funnels through,
        // which is what makes the set complete.
        self.mutated_tables.insert(table_id);
        self.table_roots.insert((table_id, index_no), (root, count));
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
        // The streamed column is always TEXT or BLOB (`row` refuses to stream
        // anything else), and neither is a column that converts — but the
        // row's OTHER columns can be, so the same rule applies here.
        let affined = self.with_store_affinity(table_id, values);
        let values = self.with_generated(table_id, &affined)?;
        let values = &values[..];
        self.eng.validate_row_in(&self.bundle, table_id, values)?;
        if self.eng.table_is_fts(table_id) {
            // An FTS table's inverted index needs the full column text resident
            // to tokenize; a never-resident stream cannot be indexed, so a
            // streamed insert would commit a content row the index can't find —
            // a silent wrong answer via a public API. Refuse. SQL INSERT routes
            // through the typed `insert_row`, which does maintain the index.
            return Err(Error::Unsupported(
                "streaming insert into an FTS table (tokenization needs the full text resident)"
                    .into(),
            ));
        }
        let table = self
            .bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let tname = table.name.clone();
        if !self.bundle.sec_indexes[table_id as usize].is_empty() {
            // A UNIQUE probe needs the value, and the whole point here is that
            // nobody has it. Refuse rather than half-check.
            return Err(Error::Unsupported(
                "streaming insert into a table with a secondary UNIQUE index".into(),
            ));
        }
        let key = self.eng.pk_key_in(&self.bundle, table_id, values)?;
        let types = &self.bundle.col_types[table_id as usize];
        let (head, _total) =
            row::encode_row_head_for_stream(values, types, stream_col, src.len())?;
        let (root, count) = self.tree_root(table_id, 0)?;
        // DESIGN-BLOBEXTENT §4 commitment 4: the declared length reserves the
        // run up front and the source streams into it via pwrite, a page at a
        // time — nothing large resident, no mapping faults. A source that
        // yields FEWER bytes than declared errors out of `next_into` and the
        // txn hard-aborts: a reference over a partially written run is how a
        // dead writer's residual bytes would become readable.
        let total_len = (head.len() + src.len()) as u64;
        let as_extent = self
            .eng
            .extent_threshold
            .is_some_and(|t| total_len as usize > t);
        let external = if as_extent && self.eng.external_extents {
            // Same expected-failure rule as below: probe the PK before a single
            // payload byte exists, so a duplicate leaves the txn clean enough
            // to continue.
            if btree::get(self, root, &key)?.is_some() {
                return Err(Error::PrimaryKeyViolation { table: tname.clone(), pk: pk_constraint_name(self.eng, table_id) });
            }
            // Streamed straight through the hash into the object, so a payload
            // of any size costs one pass and never a second resident copy. No
            // kernel-copy fast path here yet: `copy_file_range` would want the
            // bytes to land in the temp file, and the hash still has to see
            // every one of them, so the copy would buy nothing it does not
            // immediately give back.
            let mut writer = self.eng.shm.external_begin()?;
            writer.write(&head)?;
            let mut remaining = src.len();
            let mut buf = vec![0u8; PAGE_SIZE.min(64 * 1024)];
            while remaining > 0 {
                let take = remaining.min(buf.len());
                // A short source surfaces here exactly as it does on the extent
                // path, and hard-aborts the txn: a reference over a partially
                // written payload is how a dead writer's bytes become readable.
                src.next_into(&mut buf[..take])?;
                writer.write(&buf[..take])?;
                remaining -= take;
            }
            let (hash, written) = writer.finish()?;
            if written != total_len {
                return Err(Error::Internal(
                    "external payload length disagrees with the streamed row".into(),
                ));
            }
            Some((hash, total_len))
        } else {
            None
        };
        let mut payload = if let Some((hash, total_len)) = external {
            btree::Payload::ExternalRef { hash, total_len }
        } else if as_extent {
            // Same expected-failure rule as insert_row: probe the PK before
            // any payload byte or bookkeeping exists, so a duplicate leaves
            // the txn clean enough to continue.
            if btree::get(self, root, &key)?.is_some() {
                return Err(Error::PrimaryKeyViolation { table: tname, pk: pk_constraint_name(self.eng, table_id) });
            }
            let npages = u32::try_from(total_len.div_ceil(PAGE_SIZE as u64))
                .map_err(|_| Error::Unsupported("value too large for one extent".into()))?;
            let start_page = self.alloc_run(npages)?;
            leakstat::add(&leakstat::EXTENT_ALLOC_PAGES, u64::from(npages));
            let mut off = start_page * PAGE_SIZE as u64;
            self.eng.shm.file_write_at(&head, off)?;
            off += head.len() as u64;
            let mut remaining = src.len();
            // #50 import fast path: a FILE source bulk-copies kernel-side
            // (copy_file_range — measured +70% over this loop on ext4). The
            // copy dirties the shared page cache exactly as pwrite does, so
            // extent_dirty's msync discipline covers it unchanged. Partial
            // progress (EXDEV, EOPNOTSUPP, short file) falls through to the
            // pwrite loop FROM the copied offset via pread — never through
            // `next_into`, whose cursor did not move.
            // wasm32: no file-backed source can exist, so the kernel-copy
            // fast path is statically absent and every insert takes the
            // `next_into` loop below.
            #[cfg(target_arch = "wasm32")]
            let fast: Option<&crate::wasmcompat::File> = None;
            #[cfg(not(target_arch = "wasm32"))]
            let fast = src.as_file();
            if let Some(f) = fast {
                let copied = self
                    .eng
                    .shm
                    .file_copy_range_from(f, 0, off, remaining as u64)?;
                off += copied;
                remaining -= copied as usize;
                if remaining > 0 {
                    // wasm32: `read_exact_at` is an inherent method on the
                    // wasmcompat File, so no extension trait to import.
                    #[cfg(all(unix, not(target_arch = "wasm32")))]
                    use std::os::unix::fs::FileExt;
                    #[cfg(windows)]
                    use crate::wincompat::FileExt;
                    let mut buf = vec![0u8; PAGE_SIZE.min(64 * 1024)];
                    let mut src_off = copied;
                    while remaining > 0 {
                        let take = remaining.min(buf.len());
                        // A short file surfaces as UnexpectedEof — the same
                        // hard-abort class as a short next_into.
                        f.read_exact_at(&mut buf[..take], src_off).map_err(Error::Io)?;
                        self.eng.shm.file_write_at(&buf[..take], off)?;
                        off += take as u64;
                        src_off += take as u64;
                        remaining -= take;
                    }
                }
            } else {
                let mut buf = vec![0u8; PAGE_SIZE.min(64 * 1024)];
                while remaining > 0 {
                    let take = remaining.min(buf.len());
                    src.next_into(&mut buf[..take])?;
                    self.eng.shm.file_write_at(&buf[..take], off)?;
                    off += take as u64;
                    remaining -= take;
                }
            }
            let cap = u64::from(npages) * PAGE_SIZE as u64;
            if total_len < cap {
                let zeros = [0u8; 4096];
                self.eng
                    .shm
                    .file_write_at(&zeros[..(cap - total_len) as usize], off)?;
            }
            self.extent_dirty.push((start_page, npages));
            self.pending_map_edits
                .push(extent::MapEdit::Insert(start_page, npages));
            btree::Payload::ExtentRef { start_page, total_len, npages }
        } else {
            btree::Payload::Stream { head: &head, src }
        };
        let out = btree::insert(self, root, &key, &mut payload, InsertMode::InsertOnly)?;
        if out.existed {
            return Err(Error::PrimaryKeyViolation { table: tname, pk: pk_constraint_name(self.eng, table_id) });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);
        self.note_columnar_mutation(table_id, &key)?;
        Ok(())
    }

    /// sqlite's STORE-TIME AFFINITY, applied to a row on its way in.
    ///
    /// The backstop under every writer: the SQL layer already converts so that
    /// RETURNING and the triggers see the same row the engine writes, but this
    /// is the choke point all three write entry points share, so a caller that
    /// reaches the typed row API directly cannot store an unconverted value
    /// into an affinity column. The conversion is idempotent, so doing it twice
    /// costs a copy and changes nothing.
    ///
    /// Borrows unless the table actually has a converting column, keeping the
    /// zero-copy insert path (#40) intact for every rigid table.
    fn with_store_affinity<'v>(
        &self,
        table_id: u32,
        values: &'v [Value],
    ) -> std::borrow::Cow<'v, [Value]> {
        match self.bundle.schema.table(table_id) {
            Some(t) if t.converts_on_store() && t.needs_store_affinity(values) => {
                let mut owned = values.to_vec();
                t.apply_store_affinity(&mut owned);
                std::borrow::Cow::Owned(owned)
            }
            _ => std::borrow::Cow::Borrowed(values),
        }
    }

    /// Compute every GENERATED column of `values` (design: `apply_generated`).
    ///
    /// The same backstop shape as [`with_store_affinity`](Self::with_store_affinity)
    /// and for the same reason: the SQL layer computes generated values so
    /// RETURNING, the triggers and the uniqueness pre-checks all see the row the
    /// engine will write, but this is the choke point every write entry point
    /// shares, so a caller reaching the typed row API directly (the mirror
    /// importer, an SDK) cannot store a stale or NULL generated value. The
    /// computation is idempotent — it recomputes from the same inputs — so doing
    /// it twice costs a copy and changes nothing.
    ///
    /// Borrows unless the table actually has a generated column.
    fn with_generated<'v>(
        &self,
        table_id: u32,
        values: &'v [Value],
    ) -> Result<std::borrow::Cow<'v, [Value]>> {
        match self.bundle.schema.table(table_id) {
            Some(t) if t.has_generated() => {
                let mut owned = values.to_vec();
                t.apply_generated(&mut owned, &[])?;
                Ok(std::borrow::Cow::Owned(owned))
            }
            _ => Ok(std::borrow::Cow::Borrowed(values)),
        }
    }

    pub fn insert_row(&mut self, table_id: u32, values: &[Value]) -> Result<()> {
        self.check_write_blocked(table_id)?;
        let affined = self.with_store_affinity(table_id, values);
        let values = self.with_generated(table_id, &affined)?;
        let values = &values[..];
        #[cfg(feature = "leakstat")]
        let __t = crate::os::Instant::now();
        self.eng.validate_row_in(&self.bundle, table_id, values)?;
        #[cfg(feature = "leakstat")]
        leakstat::add(&leakstat::INS_NS_VALIDATE, __t.elapsed().as_nanos() as u64);
        // Do NOT clone `sec_indexes` / `sec_specs` / `sec_unique` here: they are
        // stable for the life of the bundle, and cloning them on every insert
        // was pure hot-path tax (bench users table has one UNIQUE secondary).
        // Index the tables by `i` and re-borrow between mut engine calls.
        let tid = table_id as usize;
        let n_sec = self.bundle.sec_indexes[tid].len();
        let key = self.eng.pk_key_in(&self.bundle, table_id, values)?;
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
        #[cfg(feature = "leakstat")]
        let __t = crate::os::Instant::now();
        let types = &self.bundle.col_types[tid];
        let encoded_len = row::encoded_len(values, types);
        let spills = encoded_len > btree::MAX_INLINE_VAL;
        // DESIGN-BLOBEXTENT §4: a row whose payload exceeds the threshold is
        // pwritten into a run FIRST — payload before reference — and the tree
        // gets the 20-byte vkind=2 cell instead of an overflow chain.
        let as_extent = self
            .eng
            .extent_threshold
            .is_some_and(|t| encoded_len > t);
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
        #[cfg(feature = "leakstat")]
        leakstat::add(&leakstat::INS_NS_ENCODE, __t.elapsed().as_nanos() as u64);

        // UNIQUE pre-check: only required on the extent path, where a payload
        // pwrite + map edit would otherwise land before a secondary UNIQUE
        // collision is known (caller may continue the txn after UniqueViolation,
        // so orphan map edits would commit an unreferenced extent).
        //
        // For the common inline path we skip the pre-check get: secondary
        // `InsertOnly` reports `existed` and we undo the PK (+ earlier
        // secondaries) so the dirty set is clean. That saves one btree walk
        // per UNIQUE secondary on the success path (bench users table: email).
        if as_extent {
            for i in 0..n_sec {
                if !self.bundle.sec_unique[tid][i] {
                    continue;
                }
                // A unique key is the values alone; any-NULL means no entry and
                // no conflict (SQL: UNIQUE permits multiple NULLs), and so does
                // failing a partial index's predicate.
                let Some(ikey) = self.index_entry_key(tid, i, values, &[])? else {
                    continue;
                };
                let ino = (i + 1) as u32;
                let (iroot, _) = self.tree_root(table_id, ino)?;
                if btree::get(self, iroot, &ikey)?.is_some() {
                    let tname = self
                        .bundle
                        .schema
                        .table(table_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_default();
                    let cols = &self.bundle.sec_indexes[tid][i];
                    return Err(Error::UniqueViolation {
                        table: tname,
                        constraint: index_constraint_name(self.eng, table_id, cols),
                    });
                }
            }
        }

        // The extent path writes the payload NOW (pwrite through the file,
        // range-synced at commit) and hands the tree only the reference.
        // A crash before commit publishes nothing: the run allocation lives
        // in this txn's private pool/high-water and dies with it.
        //
        // EXPECTED failures must come first: a caller may legally continue
        // the txn after a PK/UNIQUE violation, and an already-recorded map
        // edit would then commit an extent nothing references. So the PK is
        // probed here (UNIQUE was pre-checked above for extent), before any
        // payload byte or bookkeeping exists.
        let extent_ref: BigRef = if as_extent {
            let (root_probe, _) = self.tree_root(table_id, 0)?;
            if btree::get(self, root_probe, &key)?.is_some() {
                let tname = self
                    .bundle
                    .schema
                    .table(table_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                return Err(Error::PrimaryKeyViolation { table: tname, pk: pk_constraint_name(self.eng, table_id) });
            }
            let total_len = encoded_len as u64;
            if self.eng.external_extents {
                match (&flat, &parts) {
                    (Some(b), _) => self.write_external_payload(&[b.as_slice()], total_len)?,
                    (None, Some(_)) => self.write_external_payload(&pieces, total_len)?,
                    (None, None) => unreachable!("payload has exactly one form"),
                }
            } else {
                let npages = u32::try_from(total_len.div_ceil(PAGE_SIZE as u64))
                    .map_err(|_| Error::Unsupported("value too large for one extent".into()))?;
                let start_page = self.alloc_run(npages)?;
                leakstat::add(&leakstat::EXTENT_ALLOC_PAGES, u64::from(npages));
                match (&flat, &parts) {
                    (Some(b), _) => {
                        self.write_extent_payload(start_page, npages, &[b.as_slice()], total_len)?
                    }
                    (None, Some(_)) => {
                        self.write_extent_payload(start_page, npages, &pieces, total_len)?
                    }
                    (None, None) => unreachable!("payload has exactly one form"),
                }
                self.pending_map_edits.push(extent::MapEdit::Insert(start_page, npages));
                BigRef::Extent { start_page, total_len, npages }
            }
        } else {
            BigRef::None
        };
        let mut payload = match (&extent_ref, &flat) {
            (BigRef::Extent { start_page, total_len, npages }, _) => {
                btree::Payload::ExtentRef {
                    start_page: *start_page,
                    total_len: *total_len,
                    npages: *npages,
                }
            }
            (BigRef::External { hash, total_len }, _) => btree::Payload::ExternalRef {
                hash: *hash,
                total_len: *total_len,
            },
            (BigRef::None, Some(b)) => btree::Payload::Flat(b),
            (BigRef::None, None) => btree::Payload::Parts(&pieces),
        };

        let (root, count) = self.tree_root(table_id, 0)?;
        #[cfg(feature = "leakstat")]
        let __t = crate::os::Instant::now();
        let out = btree::insert(self, root, &key, &mut payload, InsertMode::InsertOnly)?;
        #[cfg(feature = "leakstat")]
        leakstat::add(&leakstat::INS_NS_BTREE, __t.elapsed().as_nanos() as u64);
        if out.existed {
            let tname = self
                .bundle
                .schema
                .table(table_id)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(Error::PrimaryKeyViolation { table: tname, pk: pk_constraint_name(self.eng, table_id) });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);

        // Secondary indexes installed so far (positions into sec_indexes), for
        // undo if a later UNIQUE collision rolls the row back.
        let mut installed: Vec<u32> = Vec::new();
        for i in 0..n_sec {
            // UNIQUE: key is the values alone (values→pk). Non-unique: the
            // values may repeat, so the key is `(values ‖ pk)` — unique by
            // construction. Any-NULL ⇒ no entry (membership rule). Both
            // store the pk as the payload so a lookup fetches the row.
            let unique = self.bundle.sec_unique[tid][i];
            let Some(ikey) = self.index_entry_key(tid, i, values, &key)? else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let out = btree::insert(self, iroot, &ikey, &mut btree::Payload::Flat(&key), InsertMode::InsertOnly)?;
            if out.existed {
                if unique && !as_extent {
                    // Undo PK + earlier secondaries so the txn is clean for a
                    // caller that continues after UniqueViolation.
                    self.undo_partial_insert(table_id, &key, values, &installed)?;
                    let tname = self
                        .bundle
                        .schema
                        .table(table_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_default();
                    let cols = &self.bundle.sec_indexes[tid][i];
                    return Err(Error::UniqueViolation {
                        table: tname,
                        constraint: index_constraint_name(self.eng, table_id, cols),
                    });
                }
                // Extent path pre-checked UNIQUE; non-unique composite is
                // unique by construction — collision is engine inconsistency.
                return Err(Error::Internal("secondary index collision within txn".into()));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount + 1);
            installed.push(i as u32);
        }
        // FTS inverted-index maintenance rides the same txn (design/DESIGN-FTS.md
        // §1): a no-op unless `table_id` is an FTS table.
        self.fts_maybe_index(table_id, values, true)?;
        self.capture_dirty(table_id, &key, DirtyOp::Upsert)?;
        self.note_columnar_mutation(table_id, &key)?;
        Ok(())
    }

    /// Roll back a PK row + the secondary index entries listed in `installed`
    /// (positions into `sec_indexes`) after a mid-insert UNIQUE collision on
    /// the inline path. Does not touch FTS or the CDC dirty set — those only
    /// run after all indexes land.
    fn undo_partial_insert(
        &mut self,
        table_id: u32,
        pk_key: &[u8],
        values: &[Value],
        installed: &[u32],
    ) -> Result<()> {
        let tid = table_id as usize;
        for &i in installed.iter().rev() {
            let i = i as usize;
            let Some(ikey) = self.index_entry_key(tid, i, values, pk_key)? else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let out = btree::delete(self, iroot, &ikey)?;
            if !out.existed {
                return Err(Error::Internal(
                    "undo_partial_insert: missing secondary just installed".into(),
                ));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount.saturating_sub(1));
        }
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::delete(self, root, pk_key)?;
        if !out.existed {
            return Err(Error::Internal(
                "undo_partial_insert: missing PK just installed".into(),
            ));
        }
        self.set_tree_root(table_id, 0, out.new_root, count.saturating_sub(1));
        Ok(())
    }

    pub fn get_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        let mut stack = [0u8; 9];
        let mut heap = Vec::new();
        let key = super::encode_probe_key(
            pk_values,
            self.bundle.pk_coll(table_id),
            &mut stack,
            &mut heap,
        );
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
            )?)),
        }
    }

    /// See [`ReadTxn::get_by_pk_cols`](super::ReadTxn::get_by_pk_cols).
    pub fn get_by_pk_cols(
        &mut self,
        table_id: u32,
        pk_values: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        let mut stack = [0u8; 9];
        let mut heap = Vec::new();
        let key = super::encode_probe_key(
            pk_values,
            self.bundle.pk_coll(table_id),
            &mut stack,
            &mut heap,
        );
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, key)? {
            None => Ok(None),
            Some(bytes) => {
                let types = &self.bundle.col_types[table_id as usize];
                let mut out = Vec::with_capacity(cols.len());
                for &c in cols {
                    out.push(row::decode_column(&bytes, types, c as usize)?);
                }
                Ok(Some(out))
            }
        }
    }

    /// The next value to auto-assign to an INTEGER PRIMARY KEY rowid alias:
    /// `max(existing pk) + 1`, or 1 for an empty table. This is sqlite's plain
    /// (non-AUTOINCREMENT) rule — the *current* maximum plus one — so a deleted
    /// top row's id can be reused. The PK tree is memcmp-ordered and `keycode`
    /// preserves signed-integer order, so the rightmost key is the true maximum;
    /// the lookup is O(tree height). Assumes a single-column integer PK (the
    /// caller checked `rowid_alias_col`); a non-integer key here is a bug.
    pub fn next_rowid(&mut self, table_id: u32) -> Result<i64> {
        let (root, _) = self.tree_root(table_id, 0)?;
        let from_tree = match btree::max_key(self, root)? {
            None => 0,
            Some(key) => match keycode::decode_key(&key, &[ColumnType::Int64])?.into_iter().next() {
                Some(Value::Int(m)) => m,
                _ => {
                    return Err(Error::Internal(
                        "rowid-alias primary key is not an integer".into(),
                    ))
                }
            },
        };
        // Without AUTOINCREMENT the answer is `max(rowid) + 1`, so deleting the
        // top row hands its id back — sqlite's behaviour without the keyword,
        // and mpedb's until now.
        if !self.bundle.schema.tables[table_id as usize].autoincrement {
            return Ok(from_tree.saturating_add(1));
        }
        // WITH it, the id must never come back, which needs a HIGH-WATER that
        // survives the delete that emptied the tree. Kept in the catalog's sys
        // keyspace and written in THIS transaction — the same one the row goes
        // in — so a crash between the two is impossible and a peer process
        // reads the same counter through the same snapshot discipline as every
        // other catalog record.
        let key = rowid_seq_key(table_id);
        let stored = match self.sys_get(&key)? {
            Some(b) if b.len() == 8 => i64::from_le_bytes(b[..8].try_into().expect("len 8")),
            _ => 0,
        };
        let next = stored.max(from_tree).saturating_add(1);
        self.sys_put(&key, &next.to_le_bytes())?;
        Ok(next)
    }

    /// An INSERT that SUPPLIES its rowid still moves an AUTOINCREMENT table's
    /// high-water: sqlite records `max(seq-or-0, id)` in `sqlite_sequence` the
    /// moment the first id is ASSIGNED — explicit or auto, and clamped at zero
    /// for a negative or zero first id (measured: `INSERT VALUES (-3)` into a
    /// fresh table puts `0` in the sequence, and the next auto id is 1) — so a
    /// handed-out id can never come back. Without it, `insert 9; delete;
    /// insert NULL` gave 1 where sqlite gives 10: a reused id under a keyword
    /// whose whole promise is never-reuse.
    ///
    /// UPDATE deliberately has no such hook — sqlite does not bump the
    /// sequence on `UPDATE … SET id = 50` either; the next auto id is still 51
    /// because [`Self::next_rowid`] takes `max(stored, tree max) + 1`, which
    /// covers that case with no write. Tables without the keyword return at
    /// the first branch and pay nothing else.
    pub fn note_rowid(&mut self, table_id: u32, id: i64) -> Result<()> {
        if !self.bundle.schema.tables[table_id as usize].autoincrement {
            return Ok(());
        }
        let key = rowid_seq_key(table_id);
        let stored = match self.sys_get(&key)? {
            Some(b) if b.len() == 8 => Some(i64::from_le_bytes(b[..8].try_into().expect("len 8"))),
            _ => None,
        };
        let new = stored.unwrap_or(0).max(id);
        if stored != Some(new) {
            self.sys_put(&key, &new.to_le_bytes())?;
        }
        Ok(())
    }

    /// Delete by primary key; returns whether the row existed.
    pub fn delete_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<bool> {
        self.check_write_blocked(table_id)?;
        let key = keycode::encode_key_spec(pk_values, self.bundle.pk_coll(table_id));
        let (root, count) = self.tree_root(table_id, 0)?;
        // fetch old row first: index maintenance needs its column values
        let tid = table_id as usize;
        let n_sec = self.bundle.sec_indexes[tid].len();
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.bundle.col_types[tid])?;
        let out = btree::delete(self, root, &key)?;
        debug_assert!(out.existed);
        self.set_tree_root(table_id, 0, out.new_root, count - 1);

        for i in 0..n_sec {
            let Some(ikey) = self.index_entry_key(tid, i, &old, &key)? else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let out = btree::delete(self, iroot, &ikey)?;
            if !out.existed {
                // A BUILDING index legitimately has holes: this row sits ahead
                // of the backfill cursor, so its entry was never written and
                // there is nothing to remove. For a READY index a hole is
                // engine corruption and stays one.
                //
                // The count guard is the load-bearing half. `icount` is u64,
                // so decrementing it for an entry that was not there wraps to
                // u64::MAX — and that count is persisted, read back by
                // `count_index_entries`, and consumed by the `count(*)`-from-
                // entry-count path. It would SURVIVE the flip to Ready and
                // answer wrongly, with no error, long after the build.
                if self.bundle.sec_state[tid][i] == mpedb_types::IndexState::Ready {
                    return Err(Error::Corrupt("missing index entry on delete".into()));
                }
            } else {
                self.set_tree_root(table_id, ino, out.new_root, icount - 1);
            }
        }
        // Remove the deleted row's postings (FTS tables only).
        self.fts_maybe_index(table_id, &old, false)?;
        self.capture_dirty(table_id, &key, DirtyOp::Delete)?;
        self.note_columnar_mutation(table_id, &key)?;
        Ok(true)
    }

    /// Replace the row with the given PK. PK columns must be unchanged
    /// (enforced; the SQL layer rejects PK updates at bind time too).
    pub fn update_by_pk(&mut self, table_id: u32, new_values: &[Value]) -> Result<bool> {
        self.check_write_blocked(table_id)?;
        let affined = self.with_store_affinity(table_id, new_values);
        let new_values = self.with_generated(table_id, &affined)?;
        let new_values = &new_values[..];
        self.eng.validate_row_in(&self.bundle, table_id, new_values)?;
        let tid = table_id as usize;
        let n_sec = self.bundle.sec_indexes[tid].len();
        let key = self.eng.pk_key_in(&self.bundle, table_id, new_values)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.bundle.col_types[tid])?;

        // pre-check UNIQUE conflicts for changed unique-indexed columns
        for i in 0..n_sec {
            if !self.bundle.sec_unique[tid][i] {
                continue;
            }
            // "changed" is measured AS THE INDEX SEES IT: under a collated index
            // `'Bob' → 'bob'` folds to the same key, so it is NOT a change — else
            // the pre-check below would find the row's OWN entry and raise a
            // phantom self-conflict.
            let cols = &self.bundle.sec_indexes[tid][i];
            // An EXPRESSION part has no column to compare, so this cheap test
            // cannot answer for it — `exact` below routes such an index to the
            // key comparison instead, and the `get` keeps this loop total in
            // the meantime.
            let changed = cols.iter().enumerate().any(|(j, &c)| {
                match (old.get(c as usize), new_values.get(c as usize)) {
                    (Some(o), Some(n)) => {
                        !index_value_equal(o, n, self.bundle.sec_specs[tid][i][j])
                    }
                    _ => true,
                }
            });
            // …but "changed" is only the WHOLE question for a whole-table
            // index. A partial one can be JOINED by a row whose indexed columns
            // never moved — the predicate reads other columns (MEASURED against
            // sqlite: `UPDATE t SET b=9` makes an existing `a` collide under
            // `UNIQUE(a) WHERE b>5`). So the cheap compare may only skip where
            // it is complete.
            // Same for an EXPRESSION index: its key parts are computed, so a
            // comparison of the indexed COLUMNS does not decide whether the key
            // moved. Both classes take the exact key comparison below.
            let ixd = &self.bundle.schema.tables[tid].indexes[i];
            let exact = ixd.predicate.is_some() || !ixd.exprs.is_empty();
            if !exact && !changed {
                continue;
            }
            // Any-NULL in the NEW values ⇒ no entry ⇒ nothing to conflict, and
            // so does failing the predicate.
            let Some(ikey) = self.index_entry_key(tid, i, new_values, &[])? else {
                continue;
            };
            // For a partial index the row's OWN entry may already sit under
            // this key (it was a member and stayed one); `changed` is what
            // rules that out for a whole-table index, and this is the same
            // question asked exactly.
            if exact && self.index_entry_key(tid, i, &old, &[])?.as_deref() == Some(ikey.as_slice())
            {
                continue;
            }
            let ino = (i + 1) as u32;
            let (iroot, _) = self.tree_root(table_id, ino)?;
            if btree::get(self, iroot, &ikey)?.is_some() {
                let tname = self
                    .bundle
                    .schema
                    .table(table_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                let cols = &self.bundle.sec_indexes[tid][i];
                return Err(Error::UniqueViolation {
                    table: tname,
                    constraint: index_constraint_name(self.eng, table_id, cols),
                });
            }
        }

        let payload = row::encode_row(new_values, &self.bundle.col_types[tid])?;
        // The threshold applies to updates exactly as to inserts; the upsert
        // frees the OLD value's chain/run through `free_old_val` either way.
        let extent_ref = if self.eng.extent_threshold.is_some_and(|t| payload.len() > t) {
            let total_len = payload.len() as u64;
            if self.eng.external_extents {
                self.write_external_payload(&[payload.as_slice()], total_len)?
            } else {
                let npages = u32::try_from(total_len.div_ceil(PAGE_SIZE as u64))
                    .map_err(|_| Error::Unsupported("value too large for one extent".into()))?;
                let start_page = self.alloc_run(npages)?;
                leakstat::add(&leakstat::EXTENT_ALLOC_PAGES, u64::from(npages));
                self.write_extent_payload(start_page, npages, &[payload.as_slice()], total_len)?;
                self.pending_map_edits.push(extent::MapEdit::Insert(start_page, npages));
                BigRef::Extent { start_page, total_len, npages }
            }
        } else {
            BigRef::None
        };
        let mut up = match extent_ref {
            BigRef::Extent { start_page, total_len, npages } => {
                btree::Payload::ExtentRef { start_page, total_len, npages }
            }
            BigRef::External { hash, total_len } => {
                btree::Payload::ExternalRef { hash, total_len }
            }
            BigRef::None => btree::Payload::Flat(&payload),
        };
        let out = btree::insert(self, root, &key, &mut up, InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);

        for i in 0..n_sec {
            let cols = &self.bundle.sec_indexes[tid][i];
            // An EXPRESSION part has no column to compare, so this cheap test
            // cannot answer for it — `exact` below routes such an index to the
            // key comparison instead, and the `get` keeps this loop total in
            // the meantime.
            let changed = cols.iter().enumerate().any(|(j, &c)| {
                match (old.get(c as usize), new_values.get(c as usize)) {
                    (Some(o), Some(n)) => {
                        !index_value_equal(o, n, self.bundle.sec_specs[tid][i][j])
                    }
                    _ => true,
                }
            });
            // Same asymmetry as the pre-check: a partial index also moves when
            // its predicate's ANSWER moves, and the predicate may read columns
            // the index does not cover.
            let ixd = &self.bundle.schema.tables[tid].indexes[i];
            let exact = ixd.predicate.is_some() || !ixd.exprs.is_empty();
            if !exact && !changed {
                continue;
            }
            let okey = self.index_entry_key(tid, i, &old, &key)?;
            let nkey = self.index_entry_key(tid, i, new_values, &key)?;
            // Membership and key both unchanged: nothing to do. (For a
            // whole-table index `changed` already decided this; for a partial
            // one this is where a no-op predicate re-evaluation stops.)
            if okey == nkey {
                continue;
            }
            let ino = (i + 1) as u32;
            let (mut iroot, mut icount) = self.tree_root(table_id, ino)?;
            if let Some(okey) = okey {
                let out = btree::delete(self, iroot, &okey)?;
                if !out.existed {
                    // The delete path's rule, for the same reason and with the
                    // same u64 wrap waiting behind it — see `delete_by_pk`.
                    if self.bundle.sec_state[tid][i] == mpedb_types::IndexState::Ready {
                        return Err(Error::Corrupt("missing index entry on update".into()));
                    }
                } else {
                    iroot = out.new_root;
                    icount -= 1;
                }
            }
            if let Some(nkey) = nkey {
                let out = btree::insert(self, iroot, &nkey, &mut btree::Payload::Flat(&key), InsertMode::InsertOnly)?;
                if out.existed {
                    return Err(Error::Internal("secondary index collision within txn".into()));
                }
                iroot = out.new_root;
                icount += 1;
            }
            self.set_tree_root(table_id, ino, iroot, icount);
        }
        // FTS: the rowid PK is unchanged (enforced), so re-index in place —
        // remove the OLD text's postings, add the NEW text's.
        self.fts_maybe_index(table_id, &old, false)?;
        self.fts_maybe_index(table_id, new_values, true)?;
        self.capture_dirty(table_id, &key, DirtyOp::Upsert)?;
        self.note_columnar_mutation(table_id, &key)?;
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
        let pkc = self.bundle.pk_coll(table_id);
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
        let mut c = btree::cursor(
            self,
            root,
            lo_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            hi_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        )?;
        // Project the PK out of the ROW, not out of the key. For every rigidly
        // typed PK the two agree exactly; for a TYPELESS (`any`) PK column they
        // do not, and the row is the one that is right. Such a column keys by
        // STORAGE CLASS (`keycode::KeySpec`), which deliberately gives the
        // integer `1` and the real `1.0` the same bytes — that is what makes it
        // one PK, as sqlite has it — so no key decoder can say which of the two
        // the row holds. The row stores the value verbatim and is already in
        // hand here, so the collected PK round-trips a value-returning
        // `UPDATE … RETURNING` unchanged.
        let pk_cols = self
            .bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?
            .primary_key
            .clone();
        let col_types = self.bundle.col_types[table_id as usize].clone();
        let mut out = Vec::new();
        while let Some((_k, v)) = c.next(self)? {
            let row = row::decode_row(&v, &col_types)?;
            out.push(
                pk_cols
                    .iter()
                    .map(|&i| {
                        row.get(i as usize).cloned().ok_or_else(|| {
                            Error::Corrupt("row shorter than its primary key".into())
                        })
                    })
                    .collect::<Result<Vec<Value>>>()?,
            );
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
        let pkc = self.bundle.pk_coll(table_id);
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
        let mut c = btree::cursor(
            self,
            root,
            lo_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
            hi_k.as_ref().map(|(k, i)| (k.as_slice(), *i)),
        )?;
        let mut out = Vec::new();
        while let Some((_k, v)) = c.next(self)? {
            out.push(row::decode_row(&v, &self.bundle.col_types[table_id as usize])?);
        }
        Ok(out)
    }

    /// The key columns' declared types for `index_no` — see
    /// [`crate::SchemaBundle::key_col_types`]. Writers need it for the same
    /// reason readers do: an UPDATE or DELETE with a range predicate builds
    /// the same key bounds a SELECT does.
    pub fn key_col_types(
        &self,
        table_id: u32,
        index_no: u32,
    ) -> Option<Vec<mpedb_types::ColumnType>> {
        self.bundle.key_col_types(table_id, index_no)
    }

    pub fn row_count(&mut self, table_id: u32) -> Result<u64> {
        self.tree_root(table_id, 0).map(|(_, c)| c)
    }

    /// Charge `n` work-rows against this execution's budget (#74) and return
    /// [`Error::RuntimeBudget`] once it is exceeded. Exposed so the SQL executor
    /// can charge the SAME meter its scans do (a DML statement's correlated
    /// subquery or nested-loop join). `which` is evaluated only on the error path.
    pub fn charge_work(&self, n: u64, which: impl FnOnce() -> String) -> Result<()> {
        self.work.charge(n, which)
    }

    /// Work-rows charged so far this execution (#74).
    pub fn work_used(&self) -> u64 {
        self.work.used()
    }

    /// The configured join-materialization live-cell budget (`0` = unlimited).
    /// The SQL executor's nested-loop join reads it to bound its intermediate
    /// product.
    pub fn join_cells_budget(&self) -> u64 {
        self.eng.join_cells_budget()
    }

    /// The configured work-row budget (`0` = unlimited, #74).
    pub fn work_budget(&self) -> u64 {
        self.work.budget()
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
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            out.push(row::decode_row(&v, &self.bundle.col_types[table_id as usize])?);
        }
        Ok(out)
    }

    /// Point probe of a secondary unique index within the writer's view.
    pub fn get_by_index(
        &mut self,
        table_id: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let ikey = keycode::encode_key_spec(values, self.bundle.index_coll(table_id, index_no));
        let (iroot, _) = self.tree_root(table_id, index_no)?;
        let Some(pk_bytes) = btree::get(self, iroot, &ikey)? else {
            return Ok(None);
        };
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &pk_bytes)? {
            None => Err(Error::Corrupt("index entry points at a missing row".into())),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
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
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        if values.iter().any(|v| v.is_null()) {
            return Ok(Vec::new()); // any-NULL rows are never indexed
        }
        let full_unique = index_no >= 1
            && self
                .bundle
                .sec_unique
                .get(table_id as usize)
                .and_then(|v| v.get(index_no as usize - 1))
                .copied()
                .unwrap_or(false)
            && self
                .bundle
                .sec_indexes
                .get(table_id as usize)
                .and_then(|v| v.get(index_no as usize - 1))
                .is_some_and(|cols| cols.len() == values.len());
        if full_unique {
            return Ok(self.get_by_index(table_id, index_no, values)?.into_iter().collect());
        }
        let prefix = keycode::encode_key_spec(values, self.bundle.index_coll(table_id, index_no));
        let (iroot, _) = self.tree_root(table_id, index_no)?;
        let (root, _) = self.tree_root(table_id, 0)?;
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, Some((&prefix[..], true)), None)?;
        while let Some((k, pk_bytes)) = c.next(self)? {
            if !k.starts_with(&prefix) {
                break; // past every (value, *) entry
            }
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            match btree::get(self, root, &pk_bytes)? {
                Some(bytes) => out.push(row::decode_row(
                    &bytes,
                    &self.bundle.col_types[table_id as usize],
                )?),
                None => {
                    return Err(Error::Corrupt("index entry points at a missing row".into()))
                }
            }
        }
        Ok(out)
    }

    /// Write-side twin of [`ReadTxn::scan_by_index_range`].
    pub fn scan_by_index_range(
        &mut self,
        table_id: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (iroot, _) = self.tree_root(table_id, index_no)?;
        let (root, _) = self.tree_root(table_id, 0)?;
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, lo, hi)?;
        while let Some((_k, pk_bytes)) = c.next(self)? {
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            match btree::get(self, root, &pk_bytes)? {
                Some(bytes) => out.push(row::decode_row(
                    &bytes,
                    &self.bundle.col_types[table_id as usize],
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

    /// Store `bytes` as a CONTIGUOUS extent under sys key `subkey`, as a genuine
    /// [`ExtentRef`](btree::Payload::ExtentRef) catalog cell — the columnar-
    /// segment storage of DESIGN-COLUMNAR §7.3. It is the EXACT allocate →
    /// payload → map-edit → insert sequence a spilling row value takes, so the
    /// run inherits that path's crash safety (the commit fixpoint / extent map)
    /// and MVCC pinning unchanged, AND two properties fall out for free:
    ///
    /// * the page-accounting verifier accounts for the run generically — it
    ///   already walks every tree's [`collect_extents`](btree::collect_extents),
    ///   and the catalog tree is one of them, so a colseg extent is neither
    ///   "leaked" nor an unreferenced map entry (which a raw extent with its ref
    ///   hidden in a `Flat` value WOULD have been);
    /// * an [`Upsert`](InsertMode::Upsert) that replaces this key, and a
    ///   [`sys_delete`](Self::sys_delete) that removes it, free the old run
    ///   through the ONE `free_old_val` path the btree uses for row extents — so
    ///   colseg needs NO manual free bookkeeping (no double-free surface).
    ///
    /// Because the bytes land contiguous in the file, a reader borrows them
    /// ZERO-COPY straight from the mapping ([`ReadTxn::extent_slice`], via
    /// [`ReadTxn::sys_extent_ref`]) instead of copying a 512 KiB overflow chain
    /// out per block — the measured OLAP scan bottleneck, not the aggregation.
    pub fn sys_put_extent(&mut self, subkey: &[u8], bytes: &[u8]) -> Result<()> {
        let total_len = bytes.len() as u64;
        if total_len == 0 {
            return Err(Error::Internal("sys_put_extent of an empty payload".into()));
        }
        let npages = u32::try_from(total_len.div_ceil(PAGE_SIZE as u64))
            .map_err(|_| Error::Unsupported("segment block too large for one extent".into()))?;
        // Allocate + write the payload + record the map edit BEFORE inserting the
        // cell, exactly as `insert_row_streaming` does. A failure past the alloc
        // aborts the whole txn (map edits and the alloc are discarded on
        // rollback), so a half-written run never survives to leak.
        let start_page = self.alloc_run(npages)?;
        leakstat::add(&leakstat::EXTENT_ALLOC_PAGES, u64::from(npages));
        self.write_extent_payload(start_page, npages, &[bytes], total_len)?;
        self.pending_map_edits
            .push(super::extent::MapEdit::Insert(start_page, npages));
        let mut payload = btree::Payload::ExtentRef { start_page, total_len, npages };
        let root = self.catalog_root;
        let out = btree::insert(self, root, &sys_key(subkey), &mut payload, InsertMode::Upsert)?;
        self.catalog_root = out.new_root;
        Ok(())
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

    /// [`sys_scan_range`](Self::sys_scan_range) yielding only KEYS, values never
    /// resolved (see [`ReadTxn::sys_scan_range_keys`]). The columnar
    /// re-compaction deletes the old blocks of one column by key without pulling
    /// each 512 KiB extent back in.
    pub fn sys_scan_range_keys(&mut self, lo: &[u8], hi: &[u8]) -> Result<Vec<Vec<u8>>> {
        let klo = sys_key(lo);
        let khi = sys_key(hi);
        let root = self.catalog_root;
        let mut c = btree::cursor(self, root, Some((&klo, true)), Some((&khi, false)))?;
        let mut out = Vec::new();
        while let Some(k) = c.next_key(self)? {
            out.push(k[1..].to_vec());
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
    /// Fill the cache, then hand back a BORROW. `CaptureConfig` holds sparse
    /// `TableSet`s rather than two `u128`s, so it is no longer `Copy` — and
    /// this is on the per-ROW write path (`check_write_blocked`,
    /// `capture_dirty`), so returning by value would allocate per row. Callers
    /// take the borrow, extract a `bool`, and drop it before touching `self`
    /// again.
    fn capture_config(&mut self) -> Result<&CaptureConfig> {
        if self.capture_cfg.is_none() {
            let c = match self.sys_get(cdc::CDC_TABS_KEY)? {
                Some(bytes) => CaptureConfig::decode(&bytes)?,
                None => CaptureConfig::default(),
            };
            self.capture_cfg = Some(c);
        }
        Ok(self.capture_cfg.as_ref().expect("just filled"))
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
        if !self.capture_config()?.is_captured(table_id) {
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


}

/// Equality as the index sees it: encoded-key comparison under the column's
/// collation, so all NaNs are equal, -0.0 == 0.0 (Value's PartialEq disagrees
/// on NaN, which caused spurious UniqueViolations when updating rows that keep a
/// NaN in a unique column), and two texts equal under a collated index (e.g.
/// `'Bob'`/`'bob'` under NOCASE) compare equal — they occupy the same key slot.
fn index_value_equal(a: &Value, b: &Value, spec: keycode::KeySpec) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => true,
        (true, false) | (false, true) => false,
        _ => {
            let (mut ka, mut kb) = (Vec::new(), Vec::new());
            keycode::encode_value_spec(&mut ka, a, spec);
            keycode::encode_value_spec(&mut kb, b, spec);
            ka == kb
        }
    }
}

/// Opaque statement-savepoint state (see [`WriteTxn::savepoint`]).
///
/// `Clone` so a caller keeping a stack of savepoints (the SQL `SAVEPOINT`
/// surface) can roll back to the same point more than once: `rollback_to`
/// consumes the snapshot, so it is handed a clone while the original stays on
/// the stack.
#[derive(Clone)]
pub struct TxnSavepoint {
    pub(super) allocs: u64,
    pub(super) adopts: u64,
    /// Was the transaction pristine when this savepoint was taken? Captured
    /// here rather than asked of the caller: it is the load-bearing half of
    /// [`WriteTxn::undo_is_exact`], and a caller that computes it a line too
    /// late gets a wrong answer that looks right.
    pristine: bool,
    catalog_root: u64,
    freelist_root: u64,
    table_roots: HashMap<(u32, u32), (u64, u64)>,
    dirty: DirtySet,
    freed: BTreeSet<u64>,
    reusable: Vec<u64>,
    taken: Vec<TakenEntry>,
    refill_cursor: Option<[u8; 11]>,
    high_water: u64,
}

impl TxnSavepoint {
    /// Was the transaction pristine when this savepoint was taken? See
    /// [`WriteTxn::undo_is_exact`].
    pub fn was_pristine(&self) -> bool {
        self.pristine
    }
}

/// A full statement-savepoint for the SQL `SAVEPOINT` surface (see
/// [`WriteTxn::savepoint_full`]): the accounting snapshot plus dirty-page
/// contents and the working-set scalars `rollback_to` does not restore.
///
/// `Clone` so a caller keeping a savepoint STACK can `ROLLBACK TO` the same
/// point more than once (`rollback_to_full` consumes the value; the caller
/// clones and keeps the original).
#[derive(Clone)]
pub struct TxnSavepointFull {
    base: TxnSavepoint,
    /// Which [`WriteTxn::sp_layers`] entry holds this savepoint's lazily
    /// captured pre-images, plus the epoch that proves the slot was not
    /// recycled. The token is what makes this struct cheap to clone and keep
    /// on a stack — the 4 KiB images live in the transaction, not here.
    layer_depth: usize,
    layer_epoch: u64,
    /// The pages ADOPTED IN PLACE (private `:memory:`) as of capture — the
    /// keys of `inplace_undo`. Rollback restores the committed pre-image of
    /// every page adopted AFTER the savepoint from `inplace_undo` and drops
    /// the entry; pages adopted BEFORE are already dirty and travel in
    /// `page_images`. Without this, an adoption after a PRISTINE savepoint
    /// was invisible to both undo channels: the page kept its original id
    /// (root restore a no-op) and no image existed — `ROLLBACK TO SAVEPOINT`
    /// silently kept the writes (found by the first real Django consumer).
    inplace_keys: std::collections::HashSet<u64>,
    schema_gen_bump: bool,
    written_tables: u64,
    commit_point: Option<(u32, u64)>,
    /// The WHOLE extent-allocator working set, captured by value.
    ///
    /// This used to be four LENGTHS, compared on rollback so that any extent
    /// activity in the scope was refused. Refusing was the honest thing while
    /// the state was not snapshotted — but on Linux `extent_threshold` is
    /// 4 KiB, so "a large blob" is an ordinary text column, and `ROLLBACK TO`
    /// failed for every one of them. Django's `TestCase` puts each test in a
    /// savepoint, so one 6 KiB value broke every following test in the class.
    ///
    /// Copying the state is exact BY CONSTRUCTION — no reasoning about which
    /// collections happen to be append-only, which is the kind of premise that
    /// is true until someone adds a compaction pass. Deliberately still EAGER
    /// after the page images went lazy (#N2 fase 4): the working set is small
    /// (roots, run lists, a coalescing buffer), and lazy-capturing it would
    /// mean hooking every allocator mutation for pennies.
    ext: ExtentSnapshot,
}

/// The extent allocator's per-transaction working set (DESIGN-BLOBEXTENT).
/// Everything `alloc_run`/`free_run`/the payload writer touch, so restoring it
/// wholesale returns the allocator to exactly its savepoint state. The page
/// side — `high_water`, `reusable`, `taken`, `freed` — is the BASE savepoint's
/// and is restored by `rollback_to_inner`.
#[derive(Clone)]
struct ExtentSnapshot {
    map_root: u64,
    run_pool: Vec<extent::PoolRun>,
    taken_runs: Vec<extent::TakenRunEntry>,
    freed_runs: Vec<(u64, u32)>,
    allocated_runs: std::collections::HashMap<u64, u32>,
    pending_map_edits: Vec<super::extent::MapEdit>,
    dirty: Vec<(u64, u32)>,
    /// Runs parked by an in-scope free (see [`WriteTxn::parked_runs`]).
    parked: Vec<(u64, u32)>,
    /// The coalescing buffer and its file offset. Bytes buffered BEFORE the
    /// savepoint belong to runs allocated before it, which are still owned, so
    /// re-flushing them writes the same bytes to the same offsets — idempotent.
    /// Bytes buffered after are dropped along with the map edits that would
    /// have referenced them.
    buf: Vec<u8>,
    buf_off: u64,
}

fn table_column_name(eng: &Engine, table_id: u32, col: u16) -> String {
    // Total by construction: an EXPRESSION key part (v13) carries the
    // `INDEX_EXPR_COL` sentinel and names no column, and a violation message is
    // not the place to discover that by panicking. sqlite names the INDEX for
    // such a constraint ("UNIQUE constraint failed: index 'ux'", measured);
    // this says what part it was without threading the index name through every
    // caller.
    eng.bundle()
        .schema
        .table(table_id)
        .and_then(|t| t.columns.get(col as usize).map(|c| c.name.clone()))
        .unwrap_or_else(|| {
            if col == mpedb_types::INDEX_EXPR_COL {
                "<expression>".to_string()
            } else {
                format!("col{col}")
            }
        })
}

/// Constraint name for a UNIQUE-violation error: the indexed column, or the
/// comma-joined list for a composite index.
/// The violated columns in sqlite's OWN spelling — `t.a` / `t.a, t.b`.
/// Built HERE, where the columns are still a structured list: a column name
/// may legally contain `, `, so no later stage may re-split the join.
fn index_constraint_name(eng: &Engine, table_id: u32, cols: &[u16]) -> String {
    let t = table_name(eng, table_id);
    cols.iter()
        .map(|&c| format!("{t}.{}", table_column_name(eng, table_id, c)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The PRIMARY KEY columns, sqlite-spelled — a PK conflict is reported by
/// sqlite as a UNIQUE failure over exactly these.
fn pk_constraint_name(eng: &Engine, table_id: u32) -> String {
    let bundle = eng.bundle();
    let Some(t) = bundle.schema.table(table_id) else {
        return String::new();
    };
    t.primary_key
        .iter()
        .filter_map(|&c| t.columns.get(c as usize).map(|c| format!("{}.{}", t.name, c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn table_name(eng: &Engine, table_id: u32) -> String {
    eng.bundle().schema.table(table_id).map(|t| t.name.clone()).unwrap_or_default()
}

/// The sys-keyspace key holding one table's AUTOINCREMENT high-water mark.
///
/// Keyed by table ID — stable, and dense in this format window — rather than by
/// name, so a rename cannot lose the counter and two tables cannot collide.
/// The value is the last id HANDED OUT, little-endian i64.
pub fn rowid_seq_key(table_id: u32) -> Vec<u8> {
    let mut k = b"rowidseq/".to_vec();
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}
