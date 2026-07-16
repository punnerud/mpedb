use super::*;

// ---------------------------------------------------------------- ReadTxn

pub struct ReadTxn<'e> {
    pub(super) eng: &'e Engine,
    pub(super) slot: u32,
    pub(super) word: u64,
    pub meta: MetaSnapshot,
    pub(super) released: bool,
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
        let types = &self.eng.col_types[table_id as usize];
        let mut out = Vec::new();
        let mut c = btree::cursor(self, iroot, lo, hi)?;
        while let Some((_k, pk_bytes)) = c.next(self)? {
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
