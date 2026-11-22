//! Strings held in memory, in front of the files.
//!
//! A `GET` through the engine is a snapshot, a tree walk and a decompress.
//! A live string that is here is answered from a hash table instead. The
//! table holds nothing the files do not: it is filled by `SET` after its
//! commit and by a `GET` that had to read the file, and every other write to
//! a string forgets the key. Losing the table loses no data.
//!
//! One process serves a directory, and that is what makes the table right.
//! A second writer on the same files would change rows this table still
//! answers for.
//!
//! The table has the same 64 parts as the store's stripes, split the same
//! way. Every write to a string holds the key's stripe while it changes the
//! file and then the table, so two writers never update one key's entry out
//! of order. A read holds no stripe. Its fill is guarded by the part's
//! change count instead: the count is read before the file is, and the fill
//! is dropped when a writer moved it in between. A writer moves the count
//! after its commit, so a read that saw the file before the commit cannot
//! fill afterwards.
//!
//! Memory is bounded by `cache_mb`, shared evenly by the parts. A part that
//! is over its share makes one pass: entries not read since the previous
//! pass go first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// Bytes charged to an entry beyond its key and value: the table's slot,
/// the two allocations and their headers.
const ENTRY_OVERHEAD: usize = 96;

pub struct Cache {
    parts: Vec<Mutex<Part>>,
    /// Bytes one part may hold. Zero turns the cache off.
    share: usize,
}

#[derive(Default)]
struct Part {
    map: HashMap<Box<[u8]>, Entry>,
    bytes: usize,
    /// Moved by every write to a key of this part.
    changes: u64,
}

struct Entry {
    value: Arc<[u8]>,
    expires: Option<i64>,
    /// Read since the last eviction pass.
    read: bool,
}

/// What a read saw before it went to the file. See [`Cache::fill`].
#[derive(Clone, Copy)]
pub struct Seen(u64);

fn cost(key: &[u8], value: &[u8]) -> usize {
    key.len() + value.len() + ENTRY_OVERHEAD
}

impl Cache {
    pub fn new(bytes: usize, parts: usize) -> Self {
        Cache {
            parts: (0..parts).map(|_| Mutex::new(Part::default())).collect(),
            share: bytes / parts.max(1),
        }
    }

    pub fn enabled(&self) -> bool {
        self.share > 0
    }

    fn part(&self, part: usize) -> MutexGuard<'_, Part> {
        self.parts[part].lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The value and its deadline, or the token a later [`Cache::fill`]
    /// needs. An expired entry is returned as it is: the caller knows the
    /// time.
    pub fn get(&self, part: usize, key: &[u8]) -> Result<(Arc<[u8]>, Option<i64>), Seen> {
        let mut part = self.part(part);
        match part.map.get_mut(key) {
            Some(entry) => {
                entry.read = true;
                Ok((Arc::clone(&entry.value), entry.expires))
            }
            None => Err(Seen(part.changes)),
        }
    }

    /// A reader's fill. Dropped when a writer touched the part since `seen`.
    pub fn fill(&self, part: usize, seen: Seen, key: &[u8], value: &[u8], expires: Option<i64>) {
        if !self.enabled() || cost(key, value) > self.share / 4 {
            return;
        }
        let share = self.share;
        let mut part = self.part(part);
        if part.changes == seen.0 {
            part.insert(key, Arc::from(value), expires, share);
        }
    }

    /// A writer's entry, after its commit and under the key's stripe.
    pub fn put(&self, part: usize, key: &[u8], value: Arc<[u8]>, expires: Option<i64>) {
        let share = self.share;
        let mut part = self.part(part);
        part.changes += 1;
        if cost(key, &value) > share / 4 {
            part.remove(key);
        } else {
            part.insert(key, value, expires, share);
        }
    }

    /// A writer changed or may have changed the key. Under the key's stripe.
    pub fn forget(&self, part: usize, key: &[u8]) {
        let mut part = self.part(part);
        part.changes += 1;
        part.remove(key);
    }
}

impl Part {
    fn remove(&mut self, key: &[u8]) {
        if let Some(old) = self.map.remove(key) {
            self.bytes -= cost(key, &old.value);
        }
    }

    fn insert(&mut self, key: &[u8], value: Arc<[u8]>, expires: Option<i64>, share: usize) {
        let added = cost(key, &value);
        let entry = Entry {
            value,
            expires,
            read: false,
        };
        match self.map.get_mut(key) {
            Some(old) => {
                self.bytes -= cost(key, &old.value);
                *old = entry;
            }
            None => {
                self.map.insert(key.into(), entry);
            }
        }
        self.bytes += added;
        if self.bytes > share {
            self.evict(share - share / 8);
        }
    }

    /// Down to `target` bytes. Unread entries go first, and a read entry
    /// loses its mark, so it goes on the next pass unless it is read again.
    fn evict(&mut self, target: usize) {
        let mut bytes = self.bytes;
        self.map.retain(|key, entry| {
            if bytes <= target || std::mem::take(&mut entry.read) {
                return true;
            }
            bytes -= cost(key, &entry.value);
            false
        });
        if bytes > target {
            self.map.retain(|key, entry| {
                if bytes <= target {
                    return true;
                }
                bytes -= cost(key, &entry.value);
                false
            });
        }
        self.bytes = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fill_that_raced_a_writer_is_dropped_and_the_budget_holds() {
        let cache = Cache::new(4096, 1);
        let seen = cache.get(0, b"k").err().unwrap();
        cache.forget(0, b"k");
        cache.fill(0, seen, b"k", b"old", None);
        assert!(cache.get(0, b"k").is_err());

        let seen = cache.get(0, b"k").err().unwrap();
        cache.fill(0, seen, b"k", b"new", Some(7));
        let (value, expires) = cache.get(0, b"k").ok().unwrap();
        assert_eq!((&*value, expires), (&b"new"[..], Some(7)));

        // One eviction pass. `k` was read and the others were not.
        for i in 0..22u32 {
            cache.put(0, &i.to_be_bytes(), Arc::from(&[0u8; 100][..]), None);
        }
        let part = cache.part(0);
        assert!(part.bytes <= 4096);
        assert!(part.map.contains_key(&b"k"[..]));
        assert!(part.map.len() <= 20);
    }
}
