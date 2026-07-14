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
        }

        /// Number of live (allocated, not freed) pages.
        pub fn live_pages(&self) -> usize {
            self.pages.len() - self.free.len() - self.freed_pending.len()
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
    }
}
