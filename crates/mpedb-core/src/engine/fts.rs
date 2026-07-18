//! Native full-text-search index maintenance and posting reads for the storage
//! engine (design/DESIGN-FTS.md §1, §4, §7).
//!
//! An FTS table stores its content rows like any table AND an inverted-index
//! B+tree at the reserved [`FTS_INDEX_NO`]. The maintenance here rides the
//! ordinary row-mutation path (`insert_row`/`update_by_pk`/`delete_by_pk`), so
//! index and content commit in ONE write txn and are never torn under SIGKILL —
//! COW + WAL give the crash-safety for free (§1). Reads (`fts_get`/`fts_prefix`)
//! serve the SQL executor's posting-list set algebra and charge the #74 work
//! meter per posting key visited.

use super::*;
use mpedb_types::fts::{self, Doclist, Tokenizer};
use std::collections::BTreeMap;

/// Reserved `index_no` for an FTS table's inverted-index tree. Far above the
/// secondary-index range (`index_no = position + 1`, bounded by `MAX_INDEXES`),
/// so it can never collide with a real secondary index; persisted in the
/// catalog like any tree root and seeded by `create_table`.
pub(crate) const FTS_INDEX_NO: u32 = 0x7fff_ffff;

impl WriteTxn<'_> {
    /// If `table_id` is an FTS table, add (`add = true`) or remove
    /// (`add = false`) the postings for `values` in this same write txn. A no-op
    /// for ordinary tables. `values` is the full row image (rowid PK + content).
    pub(super) fn fts_maybe_index(
        &mut self,
        table_id: u32,
        values: &[Value],
        add: bool,
    ) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let table = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let Some(tok) = table.kind.fts_tokenizer() else {
            return Ok(());
        };
        let content = table.fts_content_columns();
        let pk_col = table.primary_key[0];
        let docid = match &values[pk_col as usize] {
            Value::Int(i) => *i,
            other => {
                return Err(Error::Internal(format!(
                    "FTS rowid must be INTEGER, got {}",
                    other.type_name()
                )))
            }
        };
        self.fts_apply(table_id, docid, &content, tok, values, add)
    }

    /// Apply the per-column tokenization of `values` to the inverted index:
    /// rewrite each affected `(term, colno)` posting list, deleting a key whose
    /// list becomes empty. A NULL content column contributes no postings (§7).
    fn fts_apply(
        &mut self,
        table_id: u32,
        docid: i64,
        content: &[(u16, u16)],
        tok: Tokenizer,
        values: &[Value],
        add: bool,
    ) -> Result<()> {
        let (mut root, mut count) = self.tree_root(table_id, FTS_INDEX_NO)?;
        for &(col_index, colno) in content {
            let text = match &values[col_index as usize] {
                Value::Text(s) => s.clone(),
                _ => continue, // NULL (or non-text): no postings for this column
            };
            // token -> ascending positions within this column.
            let mut per_term: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            for (term, pos) in fts::tokenize(tok, &text) {
                per_term.entry(term).or_default().push(pos);
            }
            for (term, positions) in per_term {
                let key = fts::posting_key(&term, colno);
                let mut dl = match btree::get(self, root, &key)? {
                    Some(bytes) => Doclist::decode(&bytes)?,
                    None => Doclist::default(),
                };
                let existed = !dl.is_empty();
                if add {
                    dl.upsert_doc(docid, positions);
                } else {
                    dl.remove_doc(docid);
                }
                if dl.is_empty() {
                    if existed {
                        let out = btree::delete(self, root, &key)?;
                        root = out.new_root;
                        count = count.saturating_sub(1);
                    }
                } else {
                    let enc = dl.encode();
                    let out = btree::insert(
                        self,
                        root,
                        &key,
                        &mut btree::Payload::Flat(&enc),
                        InsertMode::Upsert,
                    )?;
                    root = out.new_root;
                    if !out.existed {
                        count += 1;
                    }
                }
            }
        }
        self.set_tree_root(table_id, FTS_INDEX_NO, root, count);
        Ok(())
    }

    /// The posting list for an exact `(term, colno)` key, if present. `key` is
    /// built by [`fts::posting_key`].
    pub fn fts_get(&mut self, table_id: u32, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (root, _) = self.tree_root(table_id, FTS_INDEX_NO)?;
        if root == 0 {
            return Ok(None);
        }
        btree::get(self, root, key)
    }

    /// Every posting entry whose key starts with `prefix`, as `(key, doclist)`
    /// pairs in key order. Charges one work-row per entry visited (#74).
    pub fn fts_prefix(&mut self, table_id: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (root, _) = self.tree_root(table_id, FTS_INDEX_NO)?;
        let mut out = Vec::new();
        if root == 0 {
            return Ok(out);
        }
        let mut c = btree::cursor(self, root, Some((prefix, true)), None)?;
        while let Some((k, v)) = c.next(self)? {
            if !k.starts_with(prefix) {
                break;
            }
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            out.push((k, v));
        }
        Ok(out)
    }
}

impl ReadTxn<'_> {
    /// The posting list for an exact `(term, colno)` key, if present.
    pub fn fts_get(&self, table_id: u32, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let root = catalog_entry(self, self.meta.catalog_root, table_id, FTS_INDEX_NO)?.0;
        if root == 0 {
            return Ok(None);
        }
        btree::get(self, root, key)
    }

    /// Every posting entry whose key starts with `prefix`, as `(key, doclist)`
    /// pairs in key order. Charges one work-row per entry visited (#74).
    pub fn fts_prefix(&self, table_id: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let root = catalog_entry(self, self.meta.catalog_root, table_id, FTS_INDEX_NO)?.0;
        let mut out = Vec::new();
        if root == 0 {
            return Ok(out);
        }
        let mut c = btree::cursor(self, root, Some((prefix, true)), None)?;
        while let Some((k, v)) = c.next(self)? {
            if !k.starts_with(prefix) {
                break;
            }
            self.charge_work(1, || scan_label(&self.bundle.schema, table_id))?;
            out.push((k, v));
        }
        Ok(out)
    }
}
