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

    /// Mark a committed page dirty for in-place mutation without COW.
    /// Returns `true` if adopted (caller may `page_mut`); `false` to force full
    /// COW. Default always returns `false`.
    fn adopt_inplace(&mut self, _id: u64) -> Result<bool> {
        Ok(false)
    }

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

    // ---- external payloads (`vkind=4`) ----
    //
    // Like extents, but named by their CONTENTS rather than by a place in this
    // file, so the bytes can live outside the arena — as their own file, which
    // is a thing something else can carry to another machine. Allocation and
    // the payload write are again not trait methods: the store places the bytes
    // on its own terms before the tiny reference ever reaches the btree.

    /// Read `total_len` bytes of the payload named `hash` into `out`
    /// (appended). Refusing is right for a store that has none: a read cannot
    /// invent the bytes, and a `vkind=4` cell reached here is either a database
    /// from a build with the feature on, or corruption.
    fn read_external(&self, _hash: u128, _total_len: u64, _out: &mut Vec<u8>) -> Result<()> {
        Err(Error::Unsupported("this store has no external payloads".into()))
    }

    /// Say that one reference to `hash` has gone.
    ///
    /// **Not** the mirror of `free_extent`, and the difference is the whole
    /// point of content addressing. A run of pages has exactly one owner, so
    /// freeing it is safe the moment that owner lets go. A name has as many
    /// owners as there are rows — and devices — holding identical contents, and
    /// this store can only see the rows in front of it. So the default does
    /// nothing at all: a file nobody references any more is wasted space, which
    /// is recoverable at leisure by something that can see every reference,
    /// while a file deleted out from under a reference is data loss, which is
    /// not recoverable by anything. Reclamation belongs to a sweep that knows
    /// the whole set, not to the row that happened to be edited.
    fn release_external(&mut self, _hash: u128) -> Result<()> {
        Ok(())
    }
}

/// Copy-on-write: dirty pages are modified in place; committed pages are
/// copied to a fresh dirty page and the original is scheduled for freeing.
///
/// If the store reports in-place mutation is safe ([`PageStore::adopt_inplace`]),
/// the page is marked dirty without copy/free — private `:memory:` with no
/// concurrent reader pins (sqlite-style uncontended write).
pub fn cow<S: PageStore + ?Sized>(store: &mut S, id: u64) -> Result<u64> {
    if store.is_dirty(id) {
        return Ok(id);
    }
    if store.adopt_inplace(id)? {
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
        /// External payloads (`vkind=4`): content hash → bytes. A map keyed by
        /// the hash is the model of a directory of content-named files, which
        /// is what the real store keeps — including that writing the same
        /// bytes twice is one object, not two.
        external: std::collections::BTreeMap<u128, Vec<u8>>,
        /// How many rows in the tree name each payload, so a test can say what
        /// the real reclaimer will one day have to work out for itself.
        external_refs: std::collections::BTreeMap<u128, usize>,
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

        /// Place `bytes` outside the arena and hand back the name the leaf
        /// cell will carry. Same payload-before-reference order as extents.
        pub fn put_external(&mut self, bytes: &[u8]) -> (u128, u64) {
            let hash = crate::btree::content_hash(bytes);
            self.external.insert(hash, bytes.to_vec());
            *self.external_refs.entry(hash).or_insert(0) += 1;
            (hash, bytes.len() as u64)
        }

        /// Payloads still named by at least one row.
        pub fn live_external(&self) -> Vec<u128> {
            self.external_refs
                .iter()
                .filter(|(_, n)| **n > 0)
                .map(|(h, _)| *h)
                .collect()
        }

        /// Everything held, referenced or not — what a directory listing sees.
        pub fn stored_external(&self) -> usize {
            self.external.len()
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

        fn read_external(&self, hash: u128, total_len: u64, out: &mut Vec<u8>) -> Result<()> {
            let b = self
                .external
                .get(&hash)
                .ok_or_else(|| Error::Corrupt(format!("external payload {hash:#x} not here")))?;
            if b.len() as u64 != total_len {
                return Err(Error::Corrupt("external length mismatch".into()));
            }
            out.extend_from_slice(b);
            Ok(())
        }

        fn release_external(&mut self, hash: u128) -> Result<()> {
            // The model counts references and deletes NOTHING, which is what
            // the real store does too. What it makes visible is the gap: a
            // payload can reach zero references and still be a file on disk.
            if let Some(n) = self.external_refs.get_mut(&hash) {
                *n = n.saturating_sub(1);
            }
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
