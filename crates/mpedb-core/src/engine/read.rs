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

    /// The configured join-materialization live-cell budget (`0` = unlimited).
    /// The SQL executor's nested-loop join reads it to bound its intermediate
    /// product.
    pub fn join_cells_budget(&self) -> u64 {
        self.eng.join_cells_budget()
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
        let (ty, _) = self.index_leading_plain(table_id, index_no)?;
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
        let (ty, _) = self.index_leading_plain(table_id, index_no)?;
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

    /// The leading column's `(type, ordinal)` for a PLAIN-keyed index — the
    /// precondition of decoding a value straight out of the key bytes. Errors
    /// for a collated or class-keyed leading column (the planner never emits
    /// such a plan; a forged one fails closed here).
    fn index_leading_plain(&self, table_id: u32, index_no: u32) -> Result<(ColumnType, u16)> {
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
        if !spec.is_plain() {
            return Err(Error::Unsupported(
                "index-tree aggregate over a collated or typeless key column".into(),
            ));
        }
        let ty = self.bundle.col_types[table_id as usize]
            .get(lead as usize)
            .copied()
            .ok_or_else(|| Error::Corrupt("index column out of the row".into()))?;
        Ok((ty, lead))
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
}

impl RowCursor<'_, '_> {
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
        // #74: one work-row per row this scan yields, charged once the cursor
        // has produced a row (an empty/exhausted scan costs nothing) and
        // BEFORE the decode, the order the two-step form always had.
        self.cursor.next_with(txn, &mut self.scratch, |k, v| {
            txn.charge_work(1, || scan_label(&txn.bundle.schema, table_id))?;
            if let Some(out) = key_out {
                out.clear();
                out.extend_from_slice(k);
            }
            row::decode_row_masked(v, types, keep)
        })
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
