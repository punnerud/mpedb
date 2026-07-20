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
    /// Set by the optimistic blind-apply path to record a precise
    /// (table, key_hash) point footprint at commit instead of a table-level
    /// one. `None` for every other path.
    pub(super) commit_point: Option<(u32, u64)>,
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
}

impl<'e> WriteTxn<'e> {
    pub(super) fn tree_root(&mut self, table_id: u32, index_no: u32) -> Result<(u64, u64)> {
        if let Some(&e) = self.table_roots.get(&(table_id, index_no)) {
            return Ok(e);
        }
        let e = catalog_entry(self, self.catalog_root, table_id, index_no)?;
        self.table_roots.insert((table_id, index_no), e);
        Ok(e)
    }

    pub(super) fn set_tree_root(&mut self, table_id: u32, index_no: u32, root: u64, count: u64) {
        // `& 63`: deliberate mod-64 fold, unchanged by the sparse-footprint work
        // (DESIGN-TABLE-CAP §5). A given table always folds to the same bit, so
        // a real conflict is never missed; aliasing only ever costs an extra
        // optimistic re-validation.
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
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
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
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::insert(self, root, key, &mut btree::Payload::Flat(payload), InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);
        self.capture_dirty(table_id, key, DirtyOp::Upsert)?;
        Ok(())
    }

    /// Blind DELETE of a PK. Returns whether the row existed.
    pub fn optimistic_delete(&mut self, table_id: u32, key: &[u8]) -> Result<bool> {
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
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
        let mut payload = if as_extent {
            // Same expected-failure rule as insert_row: probe the PK before
            // any payload byte or bookkeeping exists, so a duplicate leaves
            // the txn clean enough to continue.
            if btree::get(self, root, &key)?.is_some() {
                return Err(Error::PrimaryKeyViolation { table: tname });
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
            if let Some(f) = src.as_file() {
                let copied = self
                    .eng
                    .shm
                    .file_copy_range_from(f, 0, off, remaining as u64)?;
                off += copied;
                remaining -= copied as usize;
                if remaining > 0 {
                    use std::os::unix::fs::FileExt;
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
            return Err(Error::PrimaryKeyViolation { table: tname });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);
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
        let __t = std::time::Instant::now();
        self.eng.validate_row_in(&self.bundle, table_id, values)?;
        leakstat::add(&leakstat::INS_NS_VALIDATE, __t.elapsed().as_nanos() as u64);
        let table = self
            .bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let tname = table.name.clone();
        let sec = self.bundle.sec_indexes[table_id as usize].clone();
        let sec_coll = self.bundle.sec_specs[table_id as usize].clone();
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
        let __t = std::time::Instant::now();
        let types = &self.bundle.col_types[table_id as usize];
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
        leakstat::add(&leakstat::INS_NS_ENCODE, __t.elapsed().as_nanos() as u64);

        // UNIQUE pre-check on secondary indexes before mutating anything, so
        // a violation aborts with zero side effects on the dirty state. Only
        // UNIQUE indexes are checked — a plain `indexed` column allows dups.
        let sec_unique = self.bundle.sec_unique[table_id as usize].clone();
        for (i, cols) in sec.iter().enumerate() {
            if !sec_unique[i] {
                continue;
            }
            // A unique key is the values alone; any-NULL means no entry and
            // no conflict (SQL: UNIQUE permits multiple NULLs).
            let Some(ikey) = index_row_key(true, cols, values, &[], &sec_coll[i]) else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, _) = self.tree_root(table_id, ino)?;
            if btree::get(self, iroot, &ikey)?.is_some() {
                return Err(Error::UniqueViolation {
                    table: tname,
                    constraint: index_constraint_name(self.eng, table_id, cols),
                });
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
        // probed here (UNIQUE was pre-checked above), before any payload
        // byte or bookkeeping exists.
        let extent_ref = if as_extent {
            let (root_probe, _) = self.tree_root(table_id, 0)?;
            if btree::get(self, root_probe, &key)?.is_some() {
                return Err(Error::PrimaryKeyViolation { table: tname });
            }
            let total_len = encoded_len as u64;
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
            Some((start_page, total_len, npages))
        } else {
            None
        };
        let mut payload = match (extent_ref, &flat) {
            (Some((start_page, total_len, npages)), _) => {
                btree::Payload::ExtentRef { start_page, total_len, npages }
            }
            (None, Some(b)) => btree::Payload::Flat(b),
            (None, None) => btree::Payload::Parts(&pieces),
        };

        let (root, count) = self.tree_root(table_id, 0)?;
        let __t = std::time::Instant::now();
        let out = btree::insert(self, root, &key, &mut payload, InsertMode::InsertOnly)?;
        leakstat::add(&leakstat::INS_NS_BTREE, __t.elapsed().as_nanos() as u64);
        if out.existed {
            return Err(Error::PrimaryKeyViolation { table: tname });
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);

        for (i, cols) in sec.iter().enumerate() {
            // UNIQUE: key is the values alone (values→pk). Non-unique: the
            // values may repeat, so the key is `(values ‖ pk)` — unique by
            // construction. Any-NULL ⇒ no entry (membership rule). Both
            // store the pk as the payload so a lookup fetches the row.
            let Some(ikey) = index_row_key(sec_unique[i], cols, values, &key, &sec_coll[i]) else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let out = btree::insert(self, iroot, &ikey, &mut btree::Payload::Flat(&key), InsertMode::InsertOnly)?;
            if out.existed {
                // pre-check passed (unique) / composite is unique (non-unique),
                // so a collision is engine inconsistency.
                return Err(Error::Internal("secondary index collision within txn".into()));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount + 1);
        }
        // FTS inverted-index maintenance rides the same txn (design/DESIGN-FTS.md
        // §1): a no-op unless `table_id` is an FTS table.
        self.fts_maybe_index(table_id, values, true)?;
        self.capture_dirty(table_id, &key, DirtyOp::Upsert)?;
        Ok(())
    }

    pub fn get_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        let key = keycode::encode_key_spec(pk_values, self.bundle.pk_coll(table_id));
        let (root, _) = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
            )?)),
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
        match btree::max_key(self, root)? {
            None => Ok(1),
            Some(key) => match keycode::decode_key(&key, &[ColumnType::Int64])?.into_iter().next() {
                Some(Value::Int(m)) => Ok(m.saturating_add(1)),
                _ => Err(Error::Internal(
                    "rowid-alias primary key is not an integer".into(),
                )),
            },
        }
    }

    /// Delete by primary key; returns whether the row existed.
    pub fn delete_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<bool> {
        self.check_write_blocked(table_id)?;
        let key = keycode::encode_key_spec(pk_values, self.bundle.pk_coll(table_id));
        let (root, count) = self.tree_root(table_id, 0)?;
        // fetch old row first: index maintenance needs its column values
        let sec_unique = self.bundle.sec_unique[table_id as usize].clone();
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.bundle.col_types[table_id as usize])?;
        let out = btree::delete(self, root, &key)?;
        debug_assert!(out.existed);
        self.set_tree_root(table_id, 0, out.new_root, count - 1);

        let sec = self.bundle.sec_indexes[table_id as usize].clone();
        let sec_coll = self.bundle.sec_specs[table_id as usize].clone();
        for (i, cols) in sec.iter().enumerate() {
            let Some(ikey) = index_row_key(sec_unique[i], cols, &old, &key, &sec_coll[i]) else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, icount) = self.tree_root(table_id, ino)?;
            let out = btree::delete(self, iroot, &ikey)?;
            if !out.existed {
                return Err(Error::Corrupt("missing index entry on delete".into()));
            }
            self.set_tree_root(table_id, ino, out.new_root, icount - 1);
        }
        // Remove the deleted row's postings (FTS tables only).
        self.fts_maybe_index(table_id, &old, false)?;
        self.capture_dirty(table_id, &key, DirtyOp::Delete)?;
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
        let table = self
            .bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let tname = table.name.clone();
        let key = self.eng.pk_key_in(&self.bundle, table_id, new_values)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let Some(old_bytes) = btree::get(self, root, &key)? else {
            return Ok(false);
        };
        let old = row::decode_row(&old_bytes, &self.bundle.col_types[table_id as usize])?;

        let sec = self.bundle.sec_indexes[table_id as usize].clone();
        let sec_unique = self.bundle.sec_unique[table_id as usize].clone();
        let sec_coll = self.bundle.sec_specs[table_id as usize].clone();
        // pre-check UNIQUE conflicts for changed unique-indexed columns
        for (i, cols) in sec.iter().enumerate() {
            if !sec_unique[i] {
                continue;
            }
            // "changed" is measured AS THE INDEX SEES IT: under a collated index
            // `'Bob' → 'bob'` folds to the same key, so it is NOT a change — else
            // the pre-check below would find the row's OWN entry and raise a
            // phantom self-conflict.
            let changed = cols.iter().enumerate().any(|(j, &c)| {
                !index_value_equal(&old[c as usize], &new_values[c as usize], sec_coll[i][j])
            });
            if !changed {
                continue;
            }
            // Any-NULL in the NEW values ⇒ no entry ⇒ nothing to conflict.
            let Some(ikey) = index_row_key(true, cols, new_values, &[], &sec_coll[i]) else {
                continue;
            };
            let ino = (i + 1) as u32;
            let (iroot, _) = self.tree_root(table_id, ino)?;
            if btree::get(self, iroot, &ikey)?.is_some() {
                return Err(Error::UniqueViolation {
                    table: tname.clone(),
                    constraint: index_constraint_name(self.eng, table_id, cols),
                });
            }
        }

        let payload = row::encode_row(new_values, &self.bundle.col_types[table_id as usize])?;
        // The threshold applies to updates exactly as to inserts; the upsert
        // frees the OLD value's chain/run through `free_old_val` either way.
        let extent_ref = if self.eng.extent_threshold.is_some_and(|t| payload.len() > t) {
            let total_len = payload.len() as u64;
            let npages = u32::try_from(total_len.div_ceil(PAGE_SIZE as u64))
                .map_err(|_| Error::Unsupported("value too large for one extent".into()))?;
            let start_page = self.alloc_run(npages)?;
            leakstat::add(&leakstat::EXTENT_ALLOC_PAGES, u64::from(npages));
            self.write_extent_payload(start_page, npages, &[payload.as_slice()], total_len)?;
            self.pending_map_edits.push(extent::MapEdit::Insert(start_page, npages));
            Some((start_page, total_len, npages))
        } else {
            None
        };
        let mut up = match extent_ref {
            Some((start_page, total_len, npages)) => {
                btree::Payload::ExtentRef { start_page, total_len, npages }
            }
            None => btree::Payload::Flat(&payload),
        };
        let out = btree::insert(self, root, &key, &mut up, InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);

        for (i, cols) in sec.iter().enumerate() {
            let changed = cols.iter().enumerate().any(|(j, &c)| {
                !index_value_equal(&old[c as usize], &new_values[c as usize], sec_coll[i][j])
            });
            if !changed {
                continue;
            }
            let okey = index_row_key(sec_unique[i], cols, &old, &key, &sec_coll[i]);
            let nkey = index_row_key(sec_unique[i], cols, new_values, &key, &sec_coll[i]);
            let ino = (i + 1) as u32;
            let (mut iroot, mut icount) = self.tree_root(table_id, ino)?;
            if let Some(okey) = okey {
                let out = btree::delete(self, iroot, &okey)?;
                if !out.existed {
                    return Err(Error::Corrupt("missing index entry on update".into()));
                }
                iroot = out.new_root;
                icount -= 1;
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

    /// `CREATE TABLE` (#47 stage 2): append `def` to the schema — nothing
    /// renumbers (DESIGN-SCHEMA-V2: the new table takes the lowest free id;
    /// `Schema::with_added_table` validates the merged set) — write the
    /// re-canonicalized bytes to `CAT_SCHEMA_KEY`, seed the new table's
    /// empty tree-root entries (`catalog_entry` hard-errors on a missing
    /// key, exactly like bootstrap), and arm the schema-gen bump so every
    /// other process reloads at its next transaction. One ordinary COW
    /// commit publishes all of it atomically. Returns the new table's id.
    ///
    /// The txn's CAPTURED bundle still holds the old schema — a DDL txn
    /// does nothing else; the caller swaps the engine bundle after commit
    /// (`Engine::reload_schema_from_catalog`).
    pub fn create_table(&mut self, def: mpedb_types::TableDef) -> Result<u32> {
        let new_schema = self.bundle.schema.with_added_table(def)?;
        let new = new_schema.tables.last().expect("just appended");
        let tid = new.id;
        let n_indexes = new.indexes.len();
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;
        let empty = [0u8; 16]; // root 0 (empty tree), count 0
        for ino in 0..=n_indexes as u32 {
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &cat_tree_key(tid, ino),
                &mut btree::Payload::Flat(&empty),
                InsertMode::InsertOnly,
            )?;
            if out.existed {
                return Err(Error::Corrupt(format!(
                    "catalog already has tree entries for new table id {tid}"
                )));
            }
            self.catalog_root = out.new_root;
        }
        // An FTS table owns one extra tree — the inverted index at the reserved
        // FTS_INDEX_NO. Seed its catalog entry (root 0, count 0) so the first
        // `tree_root` finds it; the commit table-root write-back keeps it live.
        if new.kind.is_fts() {
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &cat_tree_key(tid, super::FTS_INDEX_NO),
                &mut btree::Payload::Flat(&empty),
                InsertMode::InsertOnly,
            )?;
            if out.existed {
                return Err(Error::Corrupt(format!(
                    "catalog already has an FTS tree entry for new table id {tid}"
                )));
            }
            self.catalog_root = out.new_root;
        }
        self.schema_gen_bump = true;
        Ok(tid)
    }

    /// DROP a table by id. Frees every reachable data/index page, unlinks the
    /// table's catalog tree-root entries, tombstones its schema slot in place
    /// (the id is NEVER reused — DESIGN-DROP-TABLE §0/§1), purges its CDC dirty
    /// entries and clears its capture/blocked bits, and arms the schema-gen
    /// bump. One ordinary COW commit publishes all of it atomically; other
    /// processes reload the tombstoned schema at their next transaction.
    ///
    /// Bounded to one commit (§2): a table larger than [`MAX_DROP_PAGES`]
    /// refuses — nothing is freed, the schema is untouched — rather than
    /// overflow the `u16` freelist chunk index. Pages committed by earlier txns
    /// are freed under THIS commit's id, so a reader still pinned on a snapshot
    /// that predates the drop keeps them un-reused (identical discipline to a
    /// large DELETE).
    pub fn drop_table(&mut self, table_id: u32) -> Result<()> {
        // Compute the tombstoned schema first: `with_dropped_table` validates
        // the id is live (and not the last live table), so an illegal drop bails
        // here before any page is freed or catalog entry unlinked.
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_dropped_table(table_id)?;
        let live = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let n_trees = 1 + live.indexes.len() as u32; // PK tree + secondaries
        // An FTS table owns one extra tree (the inverted index at FTS_INDEX_NO);
        // fold it into the tree-id list so DROP frees and unlinks it too.
        let fts_ino: Option<u32> = live.kind.is_fts().then_some(super::FTS_INDEX_NO);

        // 1. Collect every reachable page of the table's trees (branch, leaf,
        //    and overflow chains).
        let mut pages: BTreeSet<u64> = BTreeSet::new();
        for ino in (0..n_trees).chain(fts_ino) {
            let (root, _count) = self.tree_root(table_id, ino)?;
            if root != 0 {
                btree::collect_pages(self, root, &mut pages)?;
            }
        }
        if pages.len() as u64 > MAX_DROP_PAGES {
            return Err(Error::Unsupported(format!(
                "table {table_id} spans {} pages; DROP is bounded to {MAX_DROP_PAGES} per commit",
                pages.len()
            )));
        }

        // 2. Free them. `free` keys past-committed pages under this txn's id;
        //    the commit fixpoint records them, reader-pin safety governs reuse.
        for p in pages {
            PageStore::free(self, p)?;
        }

        // 3. Unlink the catalog tree-root entries so neither the page-accounting
        //    verifier nor a later `tree_root` sees the now-freed roots.
        for ino in (0..n_trees).chain(fts_ino) {
            let root = self.catalog_root;
            let out = btree::delete(self, root, &cat_tree_key(table_id, ino))?;
            self.catalog_root = out.new_root;
            self.table_roots.remove(&(table_id, ino));
        }

        // 4. Publish the tombstoned schema.
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;

        // 5. Purge the table's CDC dirty entries — a mirror push must not try to
        //    re-read rows of a table that no longer exists.
        let mut lo = cdc::CDC_DIRTY_PREFIX.to_vec();
        lo.extend_from_slice(&table_id.to_be_bytes());
        let mut hi = cdc::CDC_DIRTY_PREFIX.to_vec();
        hi.extend_from_slice(&(table_id + 1).to_be_bytes());
        for (subkey, _v) in self.sys_scan_range(&lo, &hi)? {
            self.sys_delete(&subkey)?;
        }

        // 6. Clear any capture/blocked bits for the id (harmless if unset — a
        //    dead id is never written again — but keeps the control record
        //    clean and bumps the generation for per-process caches).
        let mut cfg = self.capture_config()?.clone();
        if cfg.is_captured(table_id) || cfg.is_blocked(table_id) {
            cfg.set_captured(table_id, false);
            cfg.set_blocked(table_id, false);
            cfg.generation += 1;
            self.sys_put(cdc::CDC_TABS_KEY, &cfg.encode())?;
            self.capture_cfg = Some(cfg);
        }

        self.schema_gen_bump = true;
        Ok(())
    }

    /// ALTER TABLE ... RENAME TO (#47 stage 5). Pure metadata: computes the
    /// renamed schema from THIS txn's captured bundle (so a concurrent DDL is
    /// caught by the id-liveness check, not silently overwritten) and publishes
    /// it. No tree roots move.
    pub fn alter_rename_table(&mut self, table_id: u32, new_name: &str) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_renamed_table(table_id, new_name)?;
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... RENAME [COLUMN] (#47 stage 5). Pure metadata: the column
    /// keeps its position and type, so no row is touched.
    pub fn alter_rename_column(
        &mut self,
        table_id: u32,
        column: &str,
        new_name: &str,
    ) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_renamed_column(table_id, column, new_name)?;
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... ADD COLUMN (#47 stage 5). Appends a column and rewrites
    /// every existing row with `fill` in the new (trailing) position — mpedb's
    /// row image is schema-driven, not self-describing, so a widen cannot be
    /// lazy: an old short row decoded with the new (longer) type list would
    /// misread. `fill` is `Value::Null` for a plain nullable ADD, or the
    /// (already type-checked) constant of a `DEFAULT <const>` clause; the facade
    /// refuses UNIQUE / PRIMARY KEY on ADD and NOT NULL without a non-NULL
    /// default. The rewrite is one commit (whole table resident once, like
    /// DROP); batching is deferred. No secondary index is touched (the new
    /// column is unindexed).
    pub fn alter_add_column(
        &mut self,
        table_id: u32,
        col: mpedb_types::ColumnDef,
        fill: Value,
    ) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_added_column(table_id, col.clone())?;
        let old_types = bundle.col_types[table_id as usize].clone();
        // New column list = old ++ [new type]; the added column is trailing.
        let mut new_types = old_types.clone();
        new_types.push(col.ty);

        // Pass 1: read every row (decode with OLD types) and re-encode it with
        // the new column set to `fill`. Collect fully before mutating so the
        // cursor's read borrow is released before the write pass.
        let (root, count) = self.tree_root(table_id, 0)?;
        // A GENERATED column is not filled with a constant — every existing row
        // gets the expression's value for THAT row, computed against the widened
        // table. sqlite refuses `ADD COLUMN … STORED` outright once the table has
        // rows (it would have to rewrite them); mirror that refusal so the two
        // engines answer the same, even though mpedb's rewrite could do it.
        let gen_def = col.generated.as_ref().map(|_| {
            new_schema
                .table(table_id)
                .expect("widened table exists")
                .clone()
        });
        if let (Some(g), true) = (col.generated.as_ref(), count > 0) {
            if g.kind == mpedb_types::GeneratedKind::Stored {
                return Err(Error::Schema(
                    "cannot add a STORED generated column to a table that already has rows \
                     (sqlite refuses this too); add it as VIRTUAL, or recreate the table"
                        .into(),
                ));
            }
        }
        let mut rewritten: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, root, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let mut vals = row::decode_row(&v, &old_types)?;
                vals.push(fill.clone());
                if let Some(t) = &gen_def {
                    t.apply_generated(&mut vals, &[])?;
                }
                let payload = row::encode_row(&vals, &new_types)?;
                rewritten.push((k.to_vec(), payload));
            }
        }
        // Pass 2: upsert the widened rows back at their (unchanged) keys.
        let mut cur_root = root;
        for (k, payload) in &rewritten {
            let out = btree::insert(
                self,
                cur_root,
                k,
                &mut btree::Payload::Flat(payload),
                InsertMode::Upsert,
            )?;
            cur_root = out.new_root;
        }
        self.set_tree_root(table_id, 0, cur_root, count);
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... DROP COLUMN (#47 stage 5). Removes a column and rewrites
    /// every existing row without it (decode with old types, drop the value at
    /// the column's index, re-encode with new types). Refusals (PK column,
    /// indexed column, last column) are enforced by `with_dropped_column`, so an
    /// illegal drop bails before any row is touched. No secondary index tree is
    /// rebuilt: the dropped column is unindexed, and the surviving indexed
    /// columns' VALUES are unchanged — only their stored column *indices* shift
    /// (handled in the schema), which future writes honor.
    pub fn alter_drop_column(&mut self, table_id: u32, column: &str) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        // Column index in the OLD layout (needed for the row rewrite). The
        // schema evolver re-checks existence and the refusals below.
        let idx = {
            let table = bundle
                .schema
                .table(table_id)
                .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
            table
                .columns
                .iter()
                .position(|c| c.name == column)
                .ok_or_else(|| {
                    Error::Schema(format!("no column `{column}` in table id {table_id}"))
                })?
        };
        let new_schema = bundle.schema.with_dropped_column(table_id, column)?;
        let old_types = bundle.col_types[table_id as usize].clone();
        let mut new_types = old_types.clone();
        new_types.remove(idx);

        let (root, count) = self.tree_root(table_id, 0)?;
        let mut rewritten: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, root, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let mut vals = row::decode_row(&v, &old_types)?;
                vals.remove(idx);
                let payload = row::encode_row(&vals, &new_types)?;
                rewritten.push((k.to_vec(), payload));
            }
        }
        let mut cur_root = root;
        for (k, payload) in &rewritten {
            let out = btree::insert(
                self,
                cur_root,
                k,
                &mut btree::Payload::Flat(payload),
                InsertMode::Upsert,
            )?;
            cur_root = out.new_root;
        }
        self.set_tree_root(table_id, 0, cur_root, count);
        self.publish_schema(&new_schema)
    }

    /// CREATE INDEX: add a secondary index and BUILD its tree over the existing
    /// rows. Scans the PK tree once, computes each row's index key (skipping
    /// rows with any NULL indexed column — SQL membership), and inserts
    /// `key → pk`. A UNIQUE index whose build hits a duplicate aborts with a
    /// violation (nothing is published). One commit; the new tree's catalog
    /// root is persisted by the commit's table-root write-back.
    pub fn create_index(&mut self, table_id: u32, columns: Vec<u16>, unique: bool) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_added_index(
            table_id,
            mpedb_types::IndexDef { columns: columns.clone(), unique },
        )?;
        let (tname, new_ino, idx_coll) = {
            let table = bundle
                .schema
                .table(table_id)
                .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
            // index 0 is the PK tree; the new secondary is appended after the
            // existing ones. The new index's per-column collation comes straight
            // from the columns it covers (it is not in the bundle's caches yet).
            let coll: Vec<keycode::KeySpec> = columns
                .iter()
                .map(|&c| {
                    let cd = &table.columns[c as usize];
                    keycode::KeySpec::for_column(cd.ty, cd.collation)
                })
                .collect();
            (table.name.clone(), (table.indexes.len() + 1) as u32, coll)
        };
        let col_types = bundle.col_types[table_id as usize].clone();

        // Collect (index key, pk) for every row that has an entry.
        let (pkroot, _) = self.tree_root(table_id, 0)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, pkroot, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let values = row::decode_row(&v, &col_types)?;
                if let Some(ikey) = index_row_key(unique, &columns, &values, &k, &idx_coll) {
                    entries.push((ikey, k));
                }
            }
        }
        // Build the index tree from an empty root.
        let mut iroot = 0u64;
        let mut icount = 0u64;
        for (ikey, pk) in &entries {
            let out = btree::insert(
                self,
                iroot,
                ikey,
                &mut btree::Payload::Flat(pk),
                InsertMode::InsertOnly,
            )?;
            if unique && out.existed {
                return Err(Error::UniqueViolation {
                    table: tname,
                    constraint: index_constraint_name(self.eng, table_id, &columns),
                });
            }
            iroot = out.new_root;
            icount += 1;
        }
        self.set_tree_root(table_id, new_ino, iroot, icount);
        self.publish_schema(&new_schema)
    }

    /// Publish a new schema (already validated by the caller via one of the
    /// `Schema::with_*` evolvers) into the catalog and arm the schema-gen bump.
    /// For a PURE-METADATA change (rename): no tree roots move, so this single
    /// `CAT_SCHEMA_KEY` upsert is the whole commit. The caller swaps the engine
    /// bundle after commit; other processes reload at their next transaction.
    pub fn publish_schema(&mut self, new_schema: &mpedb_types::Schema) -> Result<()> {
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;
        self.schema_gen_bump = true;
        Ok(())
    }

    /// Arm the schema-generation bump for this commit without changing the
    /// catalog schema — used for sys-keyspace-only DDL (e.g. `CREATE VIEW`
    /// stores its source under `view/<name>`) so peer processes drop caches that
    /// baked in the old definition.
    pub fn bump_schema_gen(&mut self) {
        self.schema_gen_bump = true;
    }

    /// This transaction's captured schema view (one cheap Arc clone). For a
    /// writer it starts equal to the engine's committed bundle, and moves ahead
    /// of it only when this txn applies DDL and calls
    /// [`reload_bundle_from_catalog`](Self::reload_bundle_from_catalog) — so a
    /// facade session can compile and execute a statement against the exact
    /// schema its own uncommitted DDL will commit with (#95).
    pub fn schema_bundle(&self) -> Arc<SchemaBundle> {
        Arc::clone(&self.bundle)
    }

    /// Rebuild this transaction's captured schema bundle from its OWN
    /// (uncommitted) catalog pages, so a later statement in the SAME
    /// transaction sees a DDL change this txn just applied (`create_table`,
    /// `drop_table`, `alter_*`, `create_index`). The engine's committed bundle
    /// and every other transaction are untouched — only THIS writer's view
    /// moves, and it moves back automatically on abort (the change lives in
    /// COW pages the abort discards).
    ///
    /// CHECK programs come from the facade's compiler, installed on the engine
    /// ([`Engine::set_check_compiler`]) — the same call
    /// [`Engine::reload_schema_from_catalog`] makes. That matters here more than
    /// anywhere: a table this txn just created with a `CHECK (…)` is inserted
    /// into by the NEXT statement of the SAME transaction, so an empty program
    /// slot would mean the constraint never fires for exactly the rows written
    /// alongside it. Returns the new bundle so the caller can compile/execute
    /// against it.
    pub fn reload_bundle_from_catalog(&mut self) -> Result<Arc<SchemaBundle>> {
        let root = self.catalog_root;
        let bytes = btree::get(self, root, CAT_SCHEMA_KEY)?
            .ok_or_else(|| Error::Corrupt("no schema stored in catalog".into()))?;
        let schema = mpedb_types::Schema::from_canonical_bytes(&bytes)?;
        // The gen this bundle WILL carry once this txn commits (its DDL armed
        // `schema_gen_bump`). It never escapes this txn's lifetime; it only
        // keeps the value distinct from the pre-DDL bundle's gen.
        let gen = self.meta.schema_gen + 1;
        let checks = self.eng.compile_checks(&schema, &self.bundle.checks);
        let bundle = Arc::new(SchemaBundle::new_at(gen, schema, checks));
        self.bundle = Arc::clone(&bundle);
        Ok(bundle)
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
    /// `reusable`, `taken` and `refill_cursor` MUST be restored together —
    /// they are the whole of what a post-savepoint `refill_reusable` changed.
    /// Refill is READ-ONLY (design/DESIGN.md §4.5): it draws an entry's pages
    /// into `reusable`, records the provenance in `taken`, advances
    /// `refill_cursor` past the key — and LEAVES the entry in the tree. So
    /// there is no tree edit to undo here; what must be undone is the private
    /// bookkeeping. The entry still lists the drawn pages, so keeping them in
    /// `reusable` would offer at commit pages the fixpoint also finds listed,
    /// and a stale `taken` would have it strike out a consumption that the
    /// rollback erased. Restoring the three to the snapshot drops exactly what
    /// the refill added while re-offering the pages that were reusable before
    /// the savepoint. (`freelist_root` is restored too, but defensively: only
    /// the commit fixpoint moves it, and `rollback_to` refuses to run inside
    /// one.)
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

    /// True iff the extent allocator (DESIGN-BLOBEXTENT) has not been touched
    /// by this transaction. Extent state lives OUTSIDE every savepoint — no
    /// `rollback_to`/`rollback_to_full` restores it — so a caller that must
    /// undo a statement has to check this to know whether the undo is exact.
    pub fn extents_untouched(&self) -> bool {
        self.extent_map_root == self.meta.extent_map_root
            && self.pending_map_edits.is_empty()
            && self.freed_runs.is_empty()
            && self.taken_runs.is_empty()
            && self.extent_dirty.is_empty()
            && self.allocated_runs.is_empty()
            && self.run_pool.is_empty()
            && self.extent_buf.is_empty()
    }

    /// True iff this transaction has not mutated anything yet: no page is
    /// dirty and the extent allocator is untouched.
    ///
    /// The load-bearing consequence (design/DESIGN.md §5.3): a statement started
    /// from a pristine transaction can only ever mutate pages it COW-allocated
    /// itself, so restoring the root pointers with [`rollback_to`](Self::rollback_to)
    /// undoes it EXACTLY — even a statement that failed half-way through. From
    /// a non-pristine transaction that is no longer true (a page dirtied by an
    /// earlier statement is mutated in place, and the page id never changes).
    pub fn is_pristine(&self) -> bool {
        self.dirty.is_empty() && self.extents_untouched()
    }

    /// Discard EVERYTHING this transaction has done and return it to its
    /// just-begun state, **keeping the writer lock**.
    ///
    /// This is the batch leader's escape hatch (design/DESIGN.md §5.3): when an
    /// intent fails after partially applying itself, no savepoint can undo it
    /// (see [`savepoint_full`](Self::savepoint_full) for why), so the leader
    /// throws the whole round away and replays it with that intent's outcome
    /// pre-decided. Keeping the lock is the point — dropping and re-acquiring
    /// it would let another process lead and execute the very intents this
    /// leader is about to replay.
    ///
    /// Nothing committed is touched: by COW construction every page this txn
    /// wrote was allocated by it, so this is a pure discard. `high_water` is
    /// deliberately NOT rewound, for exactly the reason
    /// [`rollback_to`](Self::rollback_to) does not rewind it: pages minted from
    /// it belong to no committed freelist entry, so they go back to `reusable`
    /// and the commit fixpoint records them as freed, keeping page accounting
    /// exact.
    pub fn restart(&mut self) {
        debug_assert!(!self.finished);
        debug_assert!(!self.in_freelist_op);
        self.catalog_root = self.meta.catalog_root;
        self.freelist_root = self.meta.freelist_root;
        self.extent_map_root = self.meta.extent_map_root;
        self.schema_gen_bump = false;
        // Roots are a lazily-filled cache over the catalog tree, which the
        // restore above rewound; drop it so every entry reloads.
        self.table_roots.clear();
        self.dirty.clear();
        self.freed.clear();
        self.taken.clear();
        self.refill_cursor = None;
        self.reusable.clear();
        // ascending, so `reusable` stays sorted (binary-searched by the fixpoint)
        for id in self.meta.high_water..self.high_water {
            self.reusable.push(id);
        }
        self.run_pool.clear();
        self.taken_runs.clear();
        self.freed_runs.clear();
        self.allocated_runs.clear();
        self.pending_map_edits.clear();
        self.extent_dirty.clear();
        self.extent_buf.clear();
        self.extent_buf_off = 0;
        self.bound_recomputed = false;
        self.written_tables = 0;
        self.commit_point = None;
        self.work = WorkMeter::new(self.eng.work_budget());
    }

    /// A **full** savepoint for the SQL `SAVEPOINT` surface. It captures the
    /// accounting snapshot ([`savepoint`](Self::savepoint)) AND the CONTENTS of
    /// every page dirty at this instant, plus the few working-set scalars
    /// `rollback_to` does not restore.
    ///
    /// The plain [`savepoint`](Self::savepoint)/[`rollback_to`](Self::rollback_to)
    /// pair only reverts changes to pages that were COMMITTED at savepoint time
    /// (the COW allocates a fresh page, which the root-pointer restore drops).
    /// It does NOT revert an in-place mutation of a page that was ALREADY dirty
    /// (txn-local) at savepoint time — the page id never changes, so restoring
    /// root pointers leaves the mutated bytes in place. That is fine for the
    /// mirror, which only rolls back a FAILED op (a constraint violation fails
    /// before mutating, so there is nothing in-place to undo), but a SQL
    /// `ROLLBACK TO` must undo SUCCESSFUL statements, whose in-place mutations
    /// this method captures and [`rollback_to_full`](Self::rollback_to_full)
    /// restores. Heavier (it copies dirty-page bytes), so it is deliberately not
    /// on the mirror's per-row path.
    pub fn savepoint_full(&self) -> Result<TxnSavepointFull> {
        let base = self.savepoint();
        let mut page_images = Vec::with_capacity(self.dirty.len());
        for &id in &self.dirty {
            page_images.push((id, self.eng.shm.page(id)?.to_vec()));
        }
        Ok(TxnSavepointFull {
            base,
            page_images,
            schema_gen_bump: self.schema_gen_bump,
            written_tables: self.written_tables,
            commit_point: self.commit_point,
            extent_map_root: self.extent_map_root,
            ext_edits: self.pending_map_edits.len(),
            ext_freed_runs: self.freed_runs.len(),
            ext_taken_runs: self.taken_runs.len(),
            ext_dirty: self.extent_dirty.len(),
        })
    }

    /// Roll back to a [`savepoint_full`](Self::savepoint_full): restore the
    /// accounting via [`rollback_to`](Self::rollback_to), then the captured
    /// dirty-page contents and working-set scalars.
    ///
    /// Refuses (a clean [`Error::Unsupported`]) when a large-value **extent**
    /// write (the out-of-tree blob allocator, `vkind=2`) happened since the
    /// savepoint: that allocator's state is not snapshotted here, and undoing it
    /// silently would corrupt the extent map / leak pages. Inline values never
    /// touch extents, and btree overflow chains ARE ordinary dirty pages (so
    /// they are covered by the page-image restore) — only genuinely large blob
    /// columns can trip this, and they trip it cleanly rather than wrongly.
    pub fn rollback_to_full(&mut self, sp: TxnSavepointFull) -> Result<()> {
        if self.extent_map_root != sp.extent_map_root
            || self.pending_map_edits.len() != sp.ext_edits
            || self.freed_runs.len() != sp.ext_freed_runs
            || self.taken_runs.len() != sp.ext_taken_runs
            || self.extent_dirty.len() != sp.ext_dirty
        {
            return Err(Error::Unsupported(
                "ROLLBACK TO across a large blob/overflow-extent write is not supported".into(),
            ));
        }
        self.rollback_to(sp.base);
        self.schema_gen_bump = sp.schema_gen_bump;
        self.written_tables = sp.written_tables;
        self.commit_point = sp.commit_point;
        // Restore the bytes of pages that were dirty at the savepoint: these are
        // exactly the pages `rollback_to` leaves in the dirty set, and an
        // in-place mutation after the savepoint changed their contents.
        for (id, bytes) in &sp.page_images {
            self.eng
                .shm
                .page_mut_unchecked(*id)?
                .copy_from_slice(bytes);
        }
        Ok(())
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
    /// `(page id, 4 KiB contents)` for every page dirty at savepoint time.
    page_images: Vec<(u64, Vec<u8>)>,
    schema_gen_bump: bool,
    written_tables: u64,
    commit_point: Option<(u32, u64)>,
    extent_map_root: u64,
    // Append-only extent-activity counters — a change means a large-value
    // extent write happened in the scope, which `rollback_to_full` refuses.
    ext_edits: usize,
    ext_freed_runs: usize,
    ext_taken_runs: usize,
    ext_dirty: usize,
}

fn table_column_name(eng: &Engine, table_id: u32, col: u16) -> String {
    eng.bundle()
        .schema
        .table(table_id)
        .map(|t| t.columns[col as usize].name.clone())
        .unwrap_or_else(|| format!("col{col}"))
}

/// Constraint name for a UNIQUE-violation error: the indexed column, or the
/// comma-joined list for a composite index.
fn index_constraint_name(eng: &Engine, table_id: u32, cols: &[u16]) -> String {
    cols.iter()
        .map(|&c| table_column_name(eng, table_id, c))
        .collect::<Vec<_>>()
        .join(", ")
}
