use super::*;

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
        let key = keycode::encode_key(pk_values);
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
        let key = keycode::encode_key(pk_values);
        let root = self.tree_root(table_id, 0)?;
        match btree::get(self, root, &key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::decode_row(
                &bytes,
                &self.bundle.col_types[table_id as usize],
            )?)),
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
        let ikey = keycode::encode_key(values);
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
        let prefix = keycode::encode_key(values);
        let iroot = self.tree_root(table_id, index_no)?;
        let root = self.tree_root(table_id, 0)?;
        let types = &self.bundle.col_types[table_id as usize];
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, Some((&prefix[..], true)), None)?;
        while let Some((k, pk_bytes)) = c.next(self)? {
            if !k.starts_with(&prefix) {
                break; // past every (value, *) entry
            }
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            match btree::get(self, root, &pk_bytes)? {
                Some(bytes) => out.push(row::decode_row(&bytes, types)?),
                None => {
                    return Err(Error::Corrupt("index entry points at a missing row".into()))
                }
            }
        }
        Ok(out)
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
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, lo, hi)?;
        while let Some((_k, pk_bytes)) = c.next(self)? {
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
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
            Some((_k, v)) => {
                // #74: one work-row per row this scan yields. Charged AFTER the
                // cursor produced a row, so an empty/exhausted scan costs nothing.
                self.txn
                    .charge_work(1, || scan_label(&self.txn.bundle.schema, self.table_id))?;
                Ok(Some(row::decode_row(
                    &v,
                    &self.txn.bundle.col_types[self.table_id as usize],
                )?))
            }
        }
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
