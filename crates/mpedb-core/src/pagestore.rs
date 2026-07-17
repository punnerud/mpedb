//! Abstraction over a pool of 4 KiB pages with copy-on-write discipline.
//!
//! The B+tree is written purely against this trait. In production the store
//! is the shared memory mapping (pages allocated from the freelist/high-water
//! mark inside a write transaction); in tests it is a plain in-memory vector,
//! which lets the tree be model-tested without any shared-memory machinery.
//!
//! Rules:
//! - `page` may read any live page.
//! - `page_mut` is only valid for *dirty* pages (allocated by this store
//!   instance, i.e. within the current write transaction). Committed pages
//!   are immutable — mutating one would corrupt concurrent readers' snapshots.
//! - `free` schedules a page for reclamation; it must not be reused while any
//!   reader might still reference it (the engine's freelist handles that).

use mpedb_types::{Error, Result, PAGE_SIZE};

pub trait PageStore {
    fn page(&self, id: u64) -> Result<&[u8]>;
    /// Mutable access to a dirty page. Implementations must reject (or panic
    /// in debug) attempts to mutate non-dirty pages.
    fn page_mut(&mut self, id: u64) -> Result<&mut [u8]>;
    /// Allocate a zeroed page, marked dirty. Never returns 0.
    fn alloc(&mut self) -> Result<u64>;

    /// Allocate a page **without zeroing it**, marked dirty. Never returns 0.
    ///
    /// For a caller that is about to define every byte it cares about anyway,
    /// [`alloc`](Self::alloc)'s full-page `fill(0)` is redundant work: a 4 KiB
    /// memset per page, on the hot path of every blob write. `write_overflow`
    /// overwrites the header and payload immediately and then zeroes only its
    /// own tail, producing **byte-identical pages** for strictly less work.
    ///
    /// The default forwards to `alloc`, so an implementation that has no cheap
    /// un-zeroed path stays correct by doing the safe thing.
    ///
    /// # Contract
    /// The caller MUST leave no byte undefined that anything can observe.
    /// Skipping the memset means the page arrives holding whatever the last
    /// tenant left there.
    fn alloc_raw(&mut self) -> Result<u64> {
        self.alloc()
    }
    fn free(&mut self, id: u64) -> Result<()>;
    fn is_dirty(&self, id: u64) -> bool;

    // ---- extents (DESIGN-BLOBEXTENT) ----
    //
    // A store that can place large payloads OUTSIDE the page tree implements
    // these; the defaults refuse, so a `vkind=2` cell in a store without
    // extent support surfaces as an error instead of garbage. Allocation and
    // the payload write are NOT trait methods: the engine pwrites through the
    // file and the TestStore fills an arena — each on its own terms, before
    // the tiny reference ever reaches the btree.

    /// Read `total_len` bytes of the extent starting at `start_page` into
    /// `out` (appended). The store bounds-checks against its own geometry.
    fn read_extent(&self, _start_page: u64, _total_len: u64, _out: &mut Vec<u8>) -> Result<()> {
        Err(Error::Unsupported("this store has no extents".into()))
    }

    /// Schedule the run for freeing (the btree calls this exactly where it
    /// frees an overflow chain: replace and delete).
    fn free_extent(&mut self, _start_page: u64, _npages: u32) -> Result<()> {
        Err(Error::Unsupported("this store has no extents".into()))
    }
}

/// Copy-on-write: dirty pages are modified in place; committed pages are
/// copied to a fresh dirty page and the original is scheduled for freeing.
pub fn cow<S: PageStore + ?Sized>(store: &mut S, id: u64) -> Result<u64> {
    if store.is_dirty(id) {
        return Ok(id);
    }
    let new_id = store.alloc()?;
    let src: [u8; PAGE_SIZE] = store.page(id)?.try_into().map_err(|_| {
        Error::Internal("page store returned wrong page size".into())
    })?;
    store.page_mut(new_id)?.copy_from_slice(&src);
    store.free(id)?;
    Ok(new_id)
}

/// Simple in-memory store for unit tests (also used by the SQL executor's
/// unit tests further up the stack).
#[cfg(any(test, feature = "teststore"))]
pub mod test_store {
    use super::*;
    use std::collections::BTreeSet;

    #[derive(Default)]
    pub struct TestStore {
        pages: Vec<Box<[u8; PAGE_SIZE]>>,
        free: Vec<u64>,
        freed_pending: BTreeSet<u64>,
        dirty: BTreeSet<u64>,
        /// Extent arena (DESIGN-BLOBEXTENT): start id → payload bytes. Ids
        /// live in their own space (they never collide with page ids here —
        /// the model checks OWNERSHIP, the engine checks geometry).
        extents: std::collections::BTreeMap<u64, Vec<u8>>,
        extents_pending_free: BTreeSet<u64>,
        next_extent: u64,
    }

    impl TestStore {
        pub fn new() -> TestStore {
            TestStore::default()
        }

        /// Simulate a commit: pending frees become reusable, nothing is dirty.
        pub fn commit(&mut self) {
            self.free.extend(self.freed_pending.iter().copied());
            self.freed_pending.clear();
            self.dirty.clear();
            for id in std::mem::take(&mut self.extents_pending_free) {
                self.extents.remove(&id);
            }
        }

        /// Number of live (allocated, not freed) pages.
        pub fn live_pages(&self) -> usize {
            self.pages.len() - self.free.len() - self.freed_pending.len()
        }

        /// Place `bytes` in the arena and hand back the reference the leaf
        /// cell will carry — the payload-before-reference order, modeled.
        pub fn put_extent(&mut self, bytes: &[u8]) -> (u64, u64, u32) {
            self.next_extent += 1;
            let start = self.next_extent;
            let npages = bytes.len().div_ceil(PAGE_SIZE).max(1) as u32;
            self.extents.insert(start, bytes.to_vec());
            (start, bytes.len() as u64, npages)
        }

        /// Live (not pending-free) extents, for the model's leak check.
        pub fn live_extents(&self) -> Vec<u64> {
            self.extents
                .keys()
                .copied()
                .filter(|id| !self.extents_pending_free.contains(id))
                .collect()
        }
    }

    impl PageStore for TestStore {
        fn page(&self, id: u64) -> Result<&[u8]> {
            if id == 0 || self.freed_pending.contains(&id) || self.free.contains(&id) {
                return Err(Error::Internal(format!("read of dead page {id}")));
            }
            self.pages
                .get(id as usize - 1)
                .map(|p| &p[..])
                .ok_or_else(|| Error::Internal(format!("read of unallocated page {id}")))
        }

        fn page_mut(&mut self, id: u64) -> Result<&mut [u8]> {
            if !self.dirty.contains(&id) {
                return Err(Error::Internal(format!(
                    "page_mut on non-dirty page {id} (COW violation)"
                )));
            }
            Ok(&mut self.pages[id as usize - 1][..])
        }

        fn alloc(&mut self) -> Result<u64> {
            let id = match self.free.pop() {
                Some(id) => {
                    self.pages[id as usize - 1].fill(0);
                    id
                }
                None => {
                    self.pages.push(Box::new([0u8; PAGE_SIZE]));
                    self.pages.len() as u64
                }
            };
            self.dirty.insert(id);
            Ok(id)
        }

        fn free(&mut self, id: u64) -> Result<()> {
            if self.dirty.remove(&id) {
                // freed within the same txn that allocated it: reusable at once
                self.free.push(id);
                return Ok(());
            }
            if self.free.contains(&id) {
                return Err(Error::Internal(format!(
                    "double free of page {id} (already in the committed free list)"
                )));
            }
            if !self.freed_pending.insert(id) {
                return Err(Error::Internal(format!("double free of page {id}")));
            }
            Ok(())
        }

        fn is_dirty(&self, id: u64) -> bool {
            self.dirty.contains(&id)
        }

        fn read_extent(&self, start_page: u64, total_len: u64, out: &mut Vec<u8>) -> Result<()> {
            let b = self
                .extents
                .get(&start_page)
                .ok_or_else(|| Error::Internal(format!("read of dead extent {start_page}")))?;
            if self.extents_pending_free.contains(&start_page) {
                return Err(Error::Internal(format!(
                    "read of pending-free extent {start_page}"
                )));
            }
            if b.len() as u64 != total_len {
                return Err(Error::Corrupt("extent length mismatch".into()));
            }
            out.extend_from_slice(b);
            Ok(())
        }

        fn free_extent(&mut self, start_page: u64, _npages: u32) -> Result<()> {
            if !self.extents.contains_key(&start_page) {
                return Err(Error::Internal(format!(
                    "free of unknown extent {start_page}"
                )));
            }
            if !self.extents_pending_free.insert(start_page) {
                return Err(Error::Internal(format!(
                    "double free of extent {start_page}"
                )));
            }
            Ok(())
        }
    }
}
