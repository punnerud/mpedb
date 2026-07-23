use super::*;

/// How a [`ReadTxn::fold_range_column`] run ended.
pub enum FoldStop {
    /// The bounded range was drained to its end.
    Exhausted,
    /// The row cap was reached: the raw storage key of the LAST row folded.
    /// The remainder of the range is `(key, hi)`, exclusive of `key` — a valid
    /// lower bound for a fresh fold, which is how the adaptive parallel fold
    /// hands its tail to the morsel queue.
    Stopped(Vec<u8>),
}

/// Knobs of one [`ReadTxn::fold_range_column`] run. Neither affects WHICH rows
/// are folded, their order, or the values handed to the accumulator.
#[derive(Clone, Copy)]
pub struct FoldOpts {
    /// Stop after this many rows and hand back the resume key. `None` drains
    /// the range.
    pub cap: Option<u64>,
    /// Charge the #74 meter in batches of this many rows (`1` = per row,
    /// before the decode, the serial contract). A batch > 1 may fold up to
    /// `batch - 1` rows past the point the serial loop would have refused, so
    /// it is legal ONLY where an error abandons the whole attempt to a serial
    /// re-run that owns the authentic outcome — i.e. inside a parallel worker.
    pub charge_batch: u32,
}

impl FoldOpts {
    /// Every serial caller: drain the range, charge per row.
    pub const SERIAL: FoldOpts = FoldOpts { cap: None, charge_batch: 1 };

    /// A parallel worker's: drain its morsel, charging the statement's shared
    /// meter in batches. The batching is a measured necessity, not a
    /// micro-optimization — a per-row atomic RMW on one contended cache line
    /// eats most of a two-core fold's gain.
    pub const fn worker() -> FoldOpts {
        FoldOpts { cap: None, charge_batch: 64 }
    }

    /// The adaptive scheduler's leader probe: serial charging (its rows ARE
    /// the statement's first rows, and any error it raises is the serial one),
    /// stopping after `cap` rows.
    pub const fn probe(cap: u64) -> FoldOpts {
        FoldOpts { cap: Some(cap), charge_batch: 1 }
    }
}

// ---------------------------------------------------------------- ReadTxn

pub struct ReadTxn<'e> {
    pub(super) eng: &'e Engine,
    /// Schema view captured at begin (#47): stable for this txn's lifetime
    /// even while DDL swaps the engine's current bundle.
    pub(super) bundle: Arc<SchemaBundle>,
    pub(super) slot: u32,
    pub(super) word: u64,
    pub meta: MetaSnapshot,
    pub(super) released: bool,
    /// Deterministic per-execution work-row meter (#74). Scans charge it here;
    /// the SQL executor charges the same meter via [`ReadTxn::charge_work`].
    pub(super) work: WorkMeter,
}

impl PageStore for ReadTxn<'_> {
    fn read_extent(&self, start_page: u64, total_len: u64, out: &mut Vec<u8>) -> Result<()> {
        super::read_extent_from_shm(&self.eng.shm, start_page, total_len, out)
    }

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

    /// Charge `n` work-rows against this execution's budget (#74) and return
    /// [`Error::RuntimeBudget`] once it is exceeded. Exposed so the SQL executor
    /// can charge the SAME meter its scans do (correlated-subquery /
    /// nested-loop-join loops). `which` is evaluated only on the error path.
    pub fn charge_work(&self, n: u64, which: impl FnOnce() -> String) -> Result<()> {
        self.work.charge(n, which)
    }

    /// Work-rows charged so far this execution (#74).
    pub fn work_used(&self) -> u64 {
        self.work.used()
    }

    /// The configured work-row budget (`0` = unlimited, #74).
    pub fn work_budget(&self) -> u64 {
        self.work.budget()
    }

    /// The configured join-materialization live-cell budget (`0` = unlimited).
    /// The SQL executor's nested-loop join reads it to bound its intermediate
    /// product.
    pub fn join_cells_budget(&self) -> u64 {
        self.eng.join_cells_budget()
    }

    /// The parallel-fold worker ceiling this engine was opened with (`0` =
    /// auto, `1` = serial) — see `[runtime] max_query_threads`.
    pub fn max_query_threads(&self) -> u32 {
        self.eng.max_query_threads()
    }

    /// Reader slots occupied on this file right now, this transaction's own
    /// included — the parallel fold's politeness signal (a greedy analytical
    /// query must not commandeer the cores other PROCESSES' requests want).
    pub fn live_readers(&self) -> u32 {
        self.eng.live_readers()
    }

    /// Checkpoint of the work meter, for [`work_rewind`](Self::work_rewind).
    pub fn work_checkpoint(&self) -> u64 {
        self.work.used()
    }

    /// Rewind the work meter to a [`work_checkpoint`](Self::work_checkpoint)
    /// — **parallel-fold fallback plumbing only** ([`WorkMeter::rewind`]): the
    /// abandoned attempt's rows are re-read and re-charged by the serial
    /// re-run, so the statement's trip point stays the serial contract's.
    pub fn work_rewind(&self, to: u64) {
        self.work.rewind(to);
    }

    /// Cut the PK range `(lo, hi)` of `table_id` into up to `want` contiguous
    /// pieces at the B+tree's own separator keys — the adaptive parallel
    /// fold's morsel boundaries ([`crate::btree::partition_keys`]). The cuts
    /// are strictly inside the bounds, ascending, and possibly fewer than
    /// asked (an empty answer means "this range has no structure to cut at",
    /// and the caller keeps it whole). Deterministic for this snapshot; a
    /// structural descent, so no work-row charge — like any other probe.
    pub fn partition_range(
        &self,
        table_id: u32,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        want: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let root = self.tree_root(table_id, 0)?;
        btree::partition_keys(self, root, lo, hi, want)
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
        catalog_entry(self, self.meta.catalog_root, table_id, index_no).map(|(r, _, _)| r)
    }

    pub fn row_count(&self, table_id: u32) -> Result<u64> {
        catalog_entry(self, self.meta.catalog_root, table_id, 0).map(|(_, c, _)| c)
    }

    /// The table's data-modification generation on THIS snapshot
    /// (design/DESIGN-COLUMNAR.md §6): a monotonic counter bumped once per
    /// committed write transaction that mutated the table's rows.
    ///
    /// It is the exact staleness test a regenerable read-side artifact needs.
    /// A columnar segment stamped with generation `g` is readable only while
    /// this still returns `g`; any write since bumps it, and a bumped counter
    /// never returns to a prior value, so two different table states can never
    /// both look "unchanged". Heuristics like `(row_count, root_page)` cannot
    /// promise that — a delete+insert restores the count, and the freelist can
    /// hand a later commit a root page id equal to a freed earlier one.
    ///
    /// **An `Err` is NOT generation zero.** A caller that writes
    /// `mod_gen(t).unwrap_or(0)` reintroduces the wrong answer this exists to
    /// prevent: a dropped table's entry is unlinked, so the call errors, and a
    /// segment stamped 0 by a legacy 16-byte entry would then match and be
    /// read as fresh. Every consumer must treat `Err` as "no reusable
    /// artifact" (see `colseg::feed_from_segments`).
    ///
    /// **Comparable only within one file lineage, on one snapshot.** The
    /// generation counts commits to THIS file's PK tree. `mirror regenerate`
    /// and restore rebuild a file and renumber tables by name, so generations
    /// restart and an id can mean a different table; a sqlite-backed overlay
    /// can change what a read answers entirely outside this commit path. An
    /// artifact must therefore be read from the SAME snapshot that supplied
    /// the generation — which is also what makes it correct under crash
    /// rollback and `durability = async`, since the catalog root publishes
    /// both atomically.
    ///
    /// **What bumps, measured:** row inserts/updates/deletes, `ADD COLUMN`,
    /// `DROP COLUMN`, `CREATE INDEX` (an over-bump; fail-safe). `RENAME
    /// COLUMN` does NOT bump — safe only because segments are keyed by column
    /// ORDINAL and `DROP COLUMN`, which renumbers the survivors, does bump.
    /// That chain is the argument; changing either half breaks the other.
    pub fn mod_gen(&self, table_id: u32) -> Result<u64> {
        catalog_entry(self, self.meta.catalog_root, table_id, 0).map(|(_, _, g)| g)
    }

    /// Open one `text`/`blob` column of one row for CHUNKED reading — the
    /// eviction valve of DESIGN-BLOBEXTENT §5. `range` clamps to the value
    /// (`None` = the whole value); `Ok(None)` when the row is absent or the
    /// column is NULL. Each chunk is copied out AFTER a pin revalidation, so
    /// eviction between chunks surfaces as `SnapshotEvicted` — never as
    /// mixed bytes. One bounded memcpy per chunk is the honest cost;
    /// zero-copy is deliberately NOT promised for live databases.
    pub fn blob_read(
        &self,
        table_id: u32,
        pk_values: &[Value],
        col: usize,
        range: Option<(u64, u64)>,
    ) -> Result<Option<BlobReader<'_, '_>>> {
        let types = self
            .bundle
            .col_types
            .get(table_id as usize)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        let key = keycode::encode_key_spec(pk_values, self.bundle.pk_coll(table_id));
        let root = self.tree_root(table_id, 0)?;
        let Some(loc) = btree::value_loc(self, root, &key)? else {
            return Ok(None);
        };
        // The row HEAD (bitmap + fixed) is all the window computation needs;
        // it is bounded by the schema, never by the value.
        let head_len = row::head_len(types).min(loc.len() as usize);
        let mut head = vec![0u8; head_len];
        btree::read_value_range(self, &loc, 0, &mut head)?;
        let Some((start, len)) = row::varlen_window(&head, types, col)? else {
            return Ok(None);
        };
        let (off, want) = range.unwrap_or((0, u64::MAX));
        let off = off.min(len);
        let len = want.min(len - off);
        Ok(Some(BlobReader {
            txn: self,
            loc,
            start: start + off,
            len,
            pos: 0,
        }))
    }

    pub fn get_by_pk(&self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        let mut stack = [0u8; 9];
        let mut heap = Vec::new();
        let key = super::encode_probe_key(
            pk_values,
            self.bundle.pk_coll(table_id),
            &mut stack,
            &mut heap,
        );
        let root = self.tree_root(table_id, 0)?;
        match btree::get(self, root, key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
            )?)),
        }
    }

    /// Like [`get_by_pk`](Self::get_by_pk) but decodes only `cols` (0-based
    /// ordinals in projection order). Avoids materializing unrequested columns
    /// on the simple `SELECT a, b FROM t WHERE pk = $1` hot path.
    pub fn get_by_pk_cols(
        &self,
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
        let root = self.tree_root(table_id, 0)?;
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

    /// Point probe of a secondary unique index with its FULL column width;
    /// returns the full row. (#55: `values` in index-column order — the
    /// k = 1 case is the historical single-value probe.)
    pub fn get_by_index(
        &self,
        table_id: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let ikey = keycode::encode_key_spec(values, self.bundle.index_coll(table_id, index_no));
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
                &self.bundle.col_types[table_id as usize],
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
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        if values.iter().any(|v| v.is_null()) {
            return Ok(Vec::new()); // any-NULL rows are never indexed
        }
        // Exact-get fast path only when the probe covers a UNIQUE index's
        // full width — a PREFIX of a unique index is a scan like any other
        // (several rows may share the prefix).
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
        let iroot = self.tree_root(table_id, index_no)?;
        let root = self.tree_root(table_id, 0)?;
        let types = &self.bundle.col_types[table_id as usize];
        // COVERING read: when the entry already carries every column, rebuild
        // the row from it and skip the per-row PK-tree descent entirely.
        let cov = self.bundle.covering(table_id, index_no);
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, Some((&prefix[..], true)), None)?;
        while let Some((k, pk_bytes)) = c.next(self)? {
            if !k.starts_with(&prefix) {
                break; // past every (value, *) entry
            }
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            if let Some(cov) = &cov {
                out.push(cov.row(&k, &pk_bytes)?);
                continue;
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

    /// How many rows must a table hold before an index range is worth pricing
    /// at all? Below this the whole table fits in a handful of pages and the
    /// fetch path cannot lose enough to pay for the counting walk.
    const ADAPTIVE_MIN_ROWS: u64 = 4096;
    /// The switch point, as a divisor of the table's row count: a range that
    /// matches MORE than `rows / 8` is served by a scan instead of by
    /// per-entry fetches.
    ///
    /// Measured (2M-row fact table, 2-core Linux): the index-range path costs
    /// ~1.24 µs per matched row (a PK-tree descent and a row decode EACH),
    /// against ~0.11 µs per row for a sequential scan carrying the predicate —
    /// a ratio of ~11, so the two are level near 1/11 of the table and the
    /// scan is clearly ahead by 1/8. Deliberately conservative: below the
    /// switch the index keeps the work, so a selective range never regresses.
    const ADAPTIVE_DIVISOR: u64 = 8;

    /// [`scan_by_index_range`](Self::scan_by_index_range) that prices its own
    /// SELECTIVITY instead of assuming it.
    ///
    /// The index-range access fetches one row per entry — a descent into the
    /// PK tree and a row decode each — which beats a scan only while the range
    /// stays small. The planner cannot know the fraction (no histograms), so
    /// this measures it: walk the range's KEYS (no descent, no decode) and
    /// stop the moment the count passes `rows / 8`. Under the line, fetch per
    /// entry exactly as before. Over it, scan the table and keep the rows the
    /// index WOULD have held — by rebuilding each row's index key and testing
    /// it against the same raw bounds, so membership is identical by
    /// construction, NULLs included (a NULL indexed column has no entry).
    ///
    /// The counting walk is bounded by `rows / 8` steps, so it is cheap when
    /// the range is selective (it walks the whole small range) and cheap when
    /// it is not (it stops early). Rows come back in PK order rather than
    /// index order on the scan side; nothing may rely on that, and nothing
    /// does — mpedb never elides a sort over an index access
    /// (`planner/select.rs`, guard proven by differential test).
    ///
    /// Declines to the plain path (returning `None`) for anything it cannot
    /// price exactly: a multi-column index, a collated key, a typeless column,
    /// or a table too small to bother.
    pub fn scan_by_index_range_adaptive(
        &self,
        table_id: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let t = table_id as usize;
        if index_no == 0 || self.bundle.any_key_special.get(t).copied().unwrap_or(false) {
            return Ok(None);
        }
        let Some(cols) = self.bundle.sec_indexes.get(t).and_then(|v| v.get(index_no as usize - 1))
        else {
            return Ok(None);
        };
        let [col] = cols[..] else { return Ok(None) };
        let types = self
            .bundle
            .col_types
            .get(t)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        if types.get(col as usize).is_none_or(|ty| *ty == mpedb_types::ColumnType::Any) {
            return Ok(None);
        }
        let rows = self.row_count(table_id)?;
        if rows < Self::ADAPTIVE_MIN_ROWS {
            return Ok(None);
        }
        let limit = rows / Self::ADAPTIVE_DIVISOR;

        // Count the range's entries, stopping one past the switch point.
        let iroot = self.tree_root(table_id, index_no)?;
        let mut c = btree::cursor(self, iroot, lo, hi)?;
        let mut seen = 0u64;
        let mut scratch = Vec::new();
        while seen <= limit {
            if c.next_with(self, &mut scratch, |_, _| Ok(()))?.is_none() {
                return Ok(None); // the whole range fits under the line: fetch it
            }
            seen += 1;
        }

        // Over the line: scan, and keep exactly the rows the index holds.
        let unique = self
            .bundle
            .sec_unique
            .get(t)
            .and_then(|v| v.get(index_no as usize - 1))
            .copied()
            .unwrap_or(false);
        let root = self.tree_root(table_id, 0)?;
        let mut out = Vec::new();
        let mut probe: Vec<u8> = Vec::with_capacity(32);
        let mut c = btree::cursor(self, root, None, None)?;
        let mut scratch = Vec::new();
        while c
            .next_with(self, &mut scratch, |k, v| {
                self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
                let row = row::decode_row(v, types)?;
                let val = &row[col as usize];
                if val.is_null() {
                    return Ok(()); // no index entry: the range never held it
                }
                probe.clear();
                keycode::encode_value(&mut probe, val);
                if !unique {
                    probe.extend_from_slice(k); // the entry's pk suffix
                }
                let pass_lo = match lo {
                    Some((b, inc)) => {
                        let c = probe.as_slice().cmp(b);
                        c == std::cmp::Ordering::Greater
                            || (inc && c == std::cmp::Ordering::Equal)
                    }
                    None => true,
                };
                let pass_hi = match hi {
                    Some((b, inc)) => {
                        let c = probe.as_slice().cmp(b);
                        c == std::cmp::Ordering::Less || (inc && c == std::cmp::Ordering::Equal)
                    }
                    None => true,
                };
                if pass_lo && pass_hi {
                    out.push(row);
                }
                Ok(())
            })?
            .is_some()
        {}
        Ok(Some(out))
    }

    /// Rows whose indexed value falls in the raw-encoded bound range — the
    /// `IndexRange` access. Bounds use the composite-PK prefix construction
    /// (`enc(v)` / `enc(v) ++ 0xFF`), which is exactly right over both the
    /// unique (`value`) and non-unique (`value ‖ pk`) key layouts: both start
    /// with the encoded value, and `0xFF` clears every continuation.
    pub fn scan_by_index_range(
        &self,
        table_id: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let iroot = self.tree_root(table_id, index_no)?;
        let root = self.tree_root(table_id, 0)?;
        let types = &self.bundle.col_types[table_id as usize];
        // COVERING read — see `scan_by_index`.
        let cov = self.bundle.covering(table_id, index_no);
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, lo, hi)?;
        while let Some((_k, pk_bytes)) = c.next(self)? {
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            if let Some(cov) = &cov {
                out.push(cov.row(&_k, &pk_bytes)?);
                continue;
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
        let pkc = self.bundle.pk_coll(table_id);
        let lo_k = lo.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
        let hi_k = hi.map(|(v, inc)| (keycode::encode_key_spec(v, pkc), inc));
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
            scratch: Vec::new(),
            charge_batch: 1,
            pending: 0,
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
            scratch: Vec::new(),
            charge_batch: 1,
            pending: 0,
        })
    }

    /// `count(*)` over a raw-bounded PK range without touching a row: leaves
    /// are counted wholesale via [`btree::Cursor::next_leaf_count`], so only
    /// the boundary leaf pays per-cell work and no value is read or decoded.
    ///
    /// **The #74 charges are the drain-scan's, exactly**: the same total (one
    /// per row the equivalent `RowCursor` loop would have yielded), the same
    /// label, and — via [`WorkMeter::charge_many`]'s trip contract — the same
    /// `used` in a `RuntimeBudget` refusal. The budget is a tested contract;
    /// this path is faster, never cheaper.
    pub fn count_range(
        &self,
        table_id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<u64> {
        let root = self.tree_root(table_id, 0)?;
        let mut cursor = btree::cursor(self, root, lo, hi)?;
        let mut n = 0u64;
        while let Some(k) = cursor.next_leaf_count(self)? {
            // Once per leaf, the pin revalidation a row cursor does every 256
            // rows — eviction surfaces as an error, never as a wrong count.
            if !self.still_pinned() {
                return Err(Error::SnapshotEvicted);
            }
            self.work
                .charge_many(k, || scan_label(&self.bundle.schema, table_id))?;
            n += k;
        }
        Ok(n)
    }

    /// Number of ENTRIES in a secondary index tree, counted leaf-wholesale
    /// ([`btree::Cursor::next_leaf_count`]) — no key or value is ever read.
    /// The membership rule (a row with ANY NULL indexed column has no entry)
    /// makes this `count(col)` for a single-column index, and `count(*)` when
    /// every indexed column is schema-NOT-NULL — the planner's admission rule.
    ///
    /// **#74 charges: one work-row per entry**, `charge_many`'s trip contract.
    /// For an all-NOT-NULL index the entry count IS the row count, so the
    /// charge equals the drain-scan's / [`count_range`](Self::count_range)'s
    /// exactly; for a nullable indexed column it is the non-NULL count — fewer
    /// rows than a table drain, but exactly the rows this access VISITS, and
    /// deterministic for a given snapshot.
    pub fn count_index_entries(&self, table_id: u32, index_no: u32) -> Result<u64> {
        let iroot = self.tree_root(table_id, index_no)?;
        let mut cursor = btree::cursor(self, iroot, None, None)?;
        let mut n = 0u64;
        while let Some(k) = cursor.next_leaf_count(self)? {
            if !self.still_pinned() {
                return Err(Error::SnapshotEvicted);
            }
            self.work
                .charge_many(k, || scan_label(&self.bundle.schema, table_id))?;
            n += k;
        }
        Ok(n)
    }

    /// Visit the FIRST key column of every entry of a secondary index tree, in
    /// key order, decoded from the keycode bytes — the input stream of an
    /// index-tree aggregate fold (`sum(a)`/`avg(a)` over the index on `a`).
    /// The entries a row with a NULL indexed column never got are exactly the
    /// rows those aggregates skip, so the NULL-skip is free here.
    ///
    /// Only a PLAIN-keyed leading column is decodable: a collated key stores
    /// the FOLDED text and a class-keyed (`any`) column stores the canonical
    /// class image, neither of which is the row's value — the planner refuses
    /// those, and this method re-refuses rather than hand back folded bytes.
    /// Float caveat (documented at the call site): keycode canonicalizes
    /// `-0.0` to `0.0` and NaN to one image, so a decoded float is the
    /// canonical member of its key slot, which every SQL comparison calls
    /// equal to the stored one.
    ///
    /// **#74 charges: one work-row per entry visited**, charged BEFORE the
    /// decode — the same order and label a row scan uses. The total is the
    /// index's entry count (= the non-NULL count of the leading column when
    /// the trailing columns are NOT NULL), deterministic for a snapshot.
    pub fn fold_index_leading(
        &self,
        table_id: u32,
        index_no: u32,
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let (ty, spec) = self.index_leading(table_id, index_no)?;
        if !spec.is_plain() {
            // A collated key stores FOLDED text and a class-keyed (`any`)
            // column stores the canonical class image — neither is the row's
            // value, so decoding would feed the fold a spelling that may match
            // no row. The planner's set-level admission never emits this; a
            // forged plan fails closed here.
            return Err(Error::Unsupported(
                "index-tree aggregate over a collated or typeless key column".into(),
            ));
        }
        let iroot = self.tree_root(table_id, index_no)?;
        let mut c = btree::cursor(self, iroot, None, None)?;
        let mut steps = 0u32;
        let mut scratch = Vec::new();
        loop {
            // Borrowed-cell iteration (`next_with`): the key is decoded
            // straight off the leaf page — the per-entry key+value heap
            // copies of the owning `next` were the scan's dominant constant.
            let stepped = c.next_with(self, &mut scratch, |k, _pk| {
                self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
                let mut pos = 0usize;
                f(keycode::decode_value(k, &mut pos, ty)?)
            })?;
            if stepped.is_none() {
                return Ok(());
            }
            steps += 1;
            if steps.is_multiple_of(256) && !self.still_pinned() {
                return Err(Error::SnapshotEvicted);
            }
        }
    }

    /// Fold ONE decoded column of every row in a raw-bounded PK range, in scan
    /// (PK) order, without materializing a row: the column is decoded straight
    /// off the borrowed leaf cell (overflow values assembled into a reused
    /// scratch) and handed to `f` by reference. This is the spine-free input
    /// of an ungrouped single-column aggregate fold — the per-row `Vec<Value>`
    /// a `RowCursor` drain allocates was the dominant slice of that fold's
    /// ~130 ns/row (examples/agg_prof.rs), and a `sum(a)` never needed it.
    ///
    /// **#74 charges: one work-row per row, charged BEFORE the decode** — the
    /// same total, order, and label as the equivalent [`RowCursor`] drain, and
    /// therefore the same `RuntimeBudget` trip point: this path is faster,
    /// never cheaper. Pin revalidation every 256 rows, as every scan does.
    ///
    /// `opts` is [`FoldOpts::SERIAL`] for every serial caller — per-row
    /// charging, no cap, byte-identical to the pre-#131 fold. A parallel
    /// worker passes [`FoldOpts::worker`] (see there), and the adaptive
    /// scheduler's leader passes a `cap` so a long range can be handed off
    /// mid-flight ([`FoldStop::Stopped`]).
    /// Like [`fold_range_column`](Self::fold_range_column) but decodes a SET of
    /// columns into one REUSED full-width buffer and hands the buffer to the
    /// callback — the shape a filtered aggregate needs: decode the predicate's
    /// columns and the aggregate's column, evaluate, fold, and never
    /// materialize the row.
    ///
    /// Only the ordinals in `cols` are written; every other slot stays
    /// `Value::Null` for the whole scan. That is sound exactly because the
    /// caller derived `cols` from [`ExprProgram::read_columns`], which is
    /// complete — a program cannot read a slot the caller did not fill.
    pub fn fold_range_columns(
        &self,
        table_id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        cols: &[u16],
        opts: FoldOpts,
        f: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<FoldStop> {
        let types = self
            .bundle
            .col_types
            .get(table_id as usize)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        if cols.iter().any(|&c| c as usize >= types.len()) {
            return Err(Error::Internal("fold column out of row bounds".into()));
        }
        let root = self.tree_root(table_id, 0)?;
        let mut c = btree::cursor(self, root, lo, hi)?;
        let mut scratch = Vec::new();
        let mut buf = vec![Value::Null; types.len()];
        let mut steps = 0u32;
        let mut pending = 0u32;
        let mut rows = 0u64;
        let charge_batch = opts.charge_batch.max(1);
        loop {
            let stepped = c.next_with(self, &mut scratch, |_k, v| {
                if charge_batch == 1 {
                    self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
                }
                for &col in cols {
                    buf[col as usize] = row::decode_column(v, types, col as usize)?;
                }
                f(&buf)
            })?;
            if stepped.is_none() {
                if pending > 0 {
                    self.work
                        .charge_many(pending as u64, || scan_label(&self.bundle.schema, table_id))?;
                }
                return Ok(FoldStop::Exhausted);
            }
            rows += 1;
            if charge_batch > 1 {
                pending += 1;
                if pending == charge_batch {
                    self.work
                        .charge_many(pending as u64, || scan_label(&self.bundle.schema, table_id))?;
                    pending = 0;
                }
            }
            steps += 1;
            if steps.is_multiple_of(256) && !self.still_pinned() {
                return Err(Error::SnapshotEvicted);
            }
            let _ = rows;
        }
    }

    pub fn fold_range_column(
        &self,
        table_id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        col: u16,
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<FoldStop> {
        let types = self
            .bundle
            .col_types
            .get(table_id as usize)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        let root = self.tree_root(table_id, 0)?;
        let mut c = btree::cursor(self, root, lo, hi)?;
        let mut steps = 0u32;
        let mut pending = 0u32;
        let mut rows = 0u64;
        let mut scratch = Vec::new();
        let mut last_key: Vec<u8> = Vec::new();
        let charge_batch = opts.charge_batch.max(1);
        loop {
            // The resume key is copied for the row that will REACH the cap and
            // for no other: the copy is what §7 removed from this loop, and a
            // capped fold must not put it back on every row.
            let want_key = opts.cap.is_some_and(|c| rows + 1 >= c);
            let stepped = c.next_with(self, &mut scratch, |k, v| {
                if want_key {
                    last_key.clear();
                    last_key.extend_from_slice(k);
                }
                if charge_batch == 1 {
                    self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
                }
                let val = row::decode_column(v, types, col as usize)?;
                f(&val)
            })?;
            if stepped.is_none() {
                if pending > 0 {
                    self.work
                        .charge_many(pending as u64, || scan_label(&self.bundle.schema, table_id))?;
                }
                return Ok(FoldStop::Exhausted);
            }
            rows += 1;
            if charge_batch > 1 {
                pending += 1;
                if pending == charge_batch {
                    self.work
                        .charge_many(pending as u64, || scan_label(&self.bundle.schema, table_id))?;
                    pending = 0;
                }
            }
            steps += 1;
            if steps.is_multiple_of(256) && !self.still_pinned() {
                return Err(Error::SnapshotEvicted);
            }
            if opts.cap.is_some_and(|c| rows >= c) {
                if pending > 0 {
                    self.work
                        .charge_many(pending as u64, || scan_label(&self.bundle.schema, table_id))?;
                }
                return Ok(FoldStop::Stopped(std::mem::take(&mut last_key)));
            }
        }
    }

    /// The ROW behind an index tree's boundary entry: `max = false` → the
    /// tree's first entry, `max = true` → the first entry of the MAXIMAL
    /// leading-value run — i.e. the row `min(col)` / `max(col)` name, O(log n).
    /// `None` for an empty tree (an empty or all-NULL column: the aggregate's
    /// answer is NULL). The value is re-fetched FROM THE ROW, not decoded from
    /// the key, so a stored `-0.0` (whose key image is canonicalized) comes
    /// back bit-exact; and "first entry of the run" reproduces the fold's
    /// first-strict-beat tie rule — both key layouts (`values → pk` unique,
    /// `(values ‖ pk) → pk` non-unique) sort equal values by ascending pk, so
    /// the first entry is the lowest-pk tie, exactly the row a PK-ordered
    /// table fold would have kept.
    ///
    /// **#74 charges: one work-row when a row is found, none for an empty
    /// tree** — the single entry this access yields; deterministic, and
    /// documented as the probe's charge (an O(log n) probe has no equivalent
    /// drain to mirror).
    pub fn index_boundary_row(
        &self,
        table_id: u32,
        index_no: u32,
        max: bool,
    ) -> Result<Option<Vec<Value>>> {
        let (ty, spec) = self.index_leading(table_id, index_no)?;
        if spec.class {
            // A class-keyed (`any`) column's key image is not the fold's text
            // comparison; the planner refuses it (`ty != Any`) and so does the
            // engine. A COLLATED key is admitted: the run's fold-equality IS
            // the argument's collation-equality (format 60 makes the fold
            // compare under it), the run orders by ascending pk (both key
            // layouts), and the value is re-fetched from the ROW — so the
            // probe returns exactly the fold's first-strict-beat witness,
            // spelling included.
            return Err(Error::Unsupported(
                "index-tree aggregate over a typeless key column".into(),
            ));
        }
        let iroot = self.tree_root(table_id, index_no)?;
        let entry = if !max {
            btree::cursor(self, iroot, None, None)?.next(self)?
        } else {
            match btree::max_key(self, iroot)? {
                None => None,
                Some(mk) => {
                    // The maximal leading VALUE's encoded prefix: decode it once
                    // to learn its byte length, then seek the run's first entry.
                    let mut pos = 0usize;
                    let _ = keycode::decode_value(&mk, &mut pos, ty)?;
                    let prefix = &mk[..pos];
                    btree::cursor(self, iroot, Some((prefix, true)), None)?.next(self)?
                }
            }
        };
        let Some((_k, pk_bytes)) = entry else {
            return Ok(None);
        };
        self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
        let root = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &pk_bytes)? {
            None => Err(Error::Corrupt("index entry points at a missing row".into())),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
            )?)),
        }
    }

    /// The leading column's `(type, key spec)` for a secondary index — what a
    /// key-reading aggregate access must know before touching the tree. Each
    /// caller enforces its own precondition on the spec: [`Self::fold_index_leading`]
    /// requires a PLAIN key (the decoded bytes must BE the row's value);
    /// [`Self::index_boundary_row`] admits a collated one (it re-fetches the
    /// value from the row) and refuses only a class-keyed (`any`) column.
    fn index_leading(&self, table_id: u32, index_no: u32) -> Result<(ColumnType, keycode::KeySpec)> {
        let k = index_no
            .checked_sub(1)
            .ok_or_else(|| Error::Internal("index aggregate over the PK tree".into()))?;
        let cols = self
            .bundle
            .sec_indexes
            .get(table_id as usize)
            .and_then(|v| v.get(k as usize))
            .ok_or_else(|| Error::Internal("index number out of range".into()))?;
        let &lead = cols
            .first()
            .ok_or_else(|| Error::Corrupt("index with no columns".into()))?;
        let spec = self
            .bundle
            .index_coll(table_id, index_no)
            .first()
            .copied()
            .unwrap_or_default();
        let ty = self.bundle.col_types[table_id as usize]
            .get(lead as usize)
            .copied()
            .ok_or_else(|| Error::Corrupt("index column out of the row".into()))?;
        Ok((ty, spec))
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
    /// Reused assembly buffer for overflow/extent values — the inline common
    /// case borrows straight from the page and never touches it.
    scratch: Vec<u8>,
    /// #74 charges per meter update: `1` (the default) charges every row
    /// before its decode — the serial contract. A PARALLEL worker sets a
    /// batch ([`batch_charges`](RowCursor::batch_charges)).
    charge_batch: u32,
    /// Rows folded since the last meter update (`charge_batch > 1` only).
    pending: u32,
}

impl RowCursor<'_, '_> {
    /// Charge the #74 meter every `n` rows instead of every row, and hand the
    /// caller responsibility for [`flush_charges`](RowCursor::flush_charges).
    ///
    /// **Legal only inside a parallel fold worker.** Batching lets up to
    /// `n - 1` rows be yielded past the point the serial loop would have
    /// refused, so it may only be used where an error abandons the whole
    /// attempt to a serial re-run that owns the authentic outcome. Its reason
    /// to exist is measured: the workers of one statement share ONE atomic
    /// meter cell, and a per-row read-modify-write on it made an 11-core fold
    /// 1.4× SLOWER than serial (`examples/agg_prof.rs`, `sql:cntw` and its
    /// two siblings all plateauing at exactly the same ns/row — the signature
    /// of a fully serialized cache line). The total charged is unchanged.
    pub fn batch_charges(&mut self, n: u32) {
        self.charge_batch = n.max(1);
    }

    /// Charge the rows folded since the last meter update. Must be called
    /// before dropping a cursor that [`batch_charges`](RowCursor::batch_charges)
    /// was set on — including on the early exit of a capped scan — or those
    /// rows go uncharged.
    pub fn flush_charges(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let n = std::mem::take(&mut self.pending) as u64;
        let txn = self.txn;
        let table_id = self.table_id;
        txn.work
            .charge_many(n, || scan_label(&txn.bundle.schema, table_id))
    }
    // fallible + streaming, so deliberately not std::iter::Iterator
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Vec<Value>>> {
        self.next_masked(None, None)
    }

    /// [`next`](RowCursor::next) with decode-time column pruning and the
    /// row's raw storage key on the side.
    ///
    /// `keep[i]` false yields `Value::Null` without decoding slot `i`, and the
    /// row is truncated to `keep.len()` slots — [`row::decode_row_masked`]'s
    /// contract, threaded from #125's observable-column analysis so a fold
    /// that reads two columns decodes two columns. `None` is the full row,
    /// byte-identical to what [`next`] always produced.
    ///
    /// `key_out`, when given, is overwritten (`clear` + extend) with the
    /// yielded row's ENCODED KEY — the exact bytes the row is stored under,
    /// which is what a resumable batched scan needs for its next lower bound.
    /// Handing it out here costs a bounded memcpy into a caller-reused buffer;
    /// re-deriving it from the decoded row costs a key ENCODE per batch and
    /// requires the PK columns to survive the mask, which for `count(*)` they
    /// would otherwise not.
    pub fn next_masked(
        &mut self,
        keep: Option<&[bool]>,
        key_out: Option<&mut Vec<u8>>,
    ) -> Result<Option<Vec<Value>>> {
        self.steps += 1;
        if self.steps.is_multiple_of(256) && !self.txn.still_pinned() {
            return Err(Error::SnapshotEvicted);
        }
        let txn = self.txn;
        let table_id = self.table_id;
        let types = &txn.bundle.col_types[table_id as usize];
        let per_row = self.charge_batch <= 1;
        // #74: one work-row per row this scan yields, charged once the cursor
        // has produced a row (an empty/exhausted scan costs nothing) and
        // BEFORE the decode, the order the two-step form always had.
        let mut yielded = false;
        let row = self.cursor.next_with(txn, &mut self.scratch, |k, v| {
            if per_row {
                txn.charge_work(1, || scan_label(&txn.bundle.schema, table_id))?;
            } else {
                yielded = true;
            }
            if let Some(out) = key_out {
                out.clear();
                out.extend_from_slice(k);
            }
            row::decode_row_masked(v, types, keep)
        })?;
        if yielded {
            self.pending += 1;
            if self.pending >= self.charge_batch {
                self.flush_charges()?;
            }
        }
        Ok(row)
    }
}

// ------------------------------------------------------------- BlobReader

/// The chunked reads of one column's bytes (#50 B4). The [`ReadTxn`]'s pin is
/// what keeps the pages/extent run unreusable while chunks stream; the
/// per-chunk revalidation is what makes eviction an ERROR instead of a read
/// of recycled bytes.
pub struct BlobReader<'t, 'e> {
    txn: &'t ReadTxn<'e>,
    loc: btree::ValueLoc,
    /// Window start inside the row image (column start + caller offset).
    start: u64,
    /// Window length (clamped to the value).
    len: u64,
    pos: u64,
}

/// Chunk ceiling — DESIGN-BLOBEXTENT §5's default. Small enough that a slow
/// consumer never holds more than this much copied memory per step, big
/// enough that an extent read is a handful of memcpys per MiB.
pub const BLOB_CHUNK: usize = 256 * 1024;

impl BlobReader<'_, '_> {
    /// Total bytes this reader will yield (the clamped window).
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The next chunk (≤ [`BLOB_CHUNK`] bytes), or `None` when done.
    // fallible + stateful, so deliberately not std::iter::Iterator
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Vec<u8>>> {
        if self.pos >= self.len {
            return Ok(None);
        }
        if !self.txn.still_pinned() {
            return Err(Error::SnapshotEvicted);
        }
        let n = (self.len - self.pos).min(BLOB_CHUNK as u64) as usize;
        let mut buf = vec![0u8; n];
        btree::read_value_range(self.txn, &self.loc, self.start + self.pos, &mut buf)?;
        self.pos += n as u64;
        Ok(Some(buf))
    }
}
