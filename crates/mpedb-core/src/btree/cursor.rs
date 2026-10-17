//! Immutable forward scans and disjoint range partitioning.

use super::*;

// ---------- range scans ----------

/// Forward cursor over `[lo, hi)` / `[lo, hi]` depending on inclusivity.
/// Reads only committed/immutable pages; do not use across mutations.
pub struct Cursor {
    /// (page id, next child index) for branches; leaf handled via `leaf`.
    stack: Vec<(u64, usize)>,
    leaf: Option<(u64, usize)>,
    hi: Option<(Vec<u8>, bool)>,
    done: bool,
}

pub fn cursor<S: PageStore + ?Sized>(
    store: &S,
    root: u64,
    lo: Option<(&[u8], bool)>,
    hi: Option<(&[u8], bool)>,
) -> Result<Cursor> {
    let mut c = Cursor {
        stack: Vec::new(),
        leaf: None,
        hi: hi.map(|(k, inc)| (k.to_vec(), inc)),
        done: root == 0,
    };
    if root == 0 {
        return Ok(c);
    }
    // descend to the first leaf position >= lo
    let mut id = root;
    for _ in 0..64 {
        let p = store.page(id)?;
        check_node(p)?;
        match kind(p) {
            KIND_BRANCH => {
                let idx = match lo {
                    None => 0,
                    Some((k, _)) => descent_index(p, k)?,
                };
                c.stack.push((id, idx + 1));
                id = branch_child(p, idx)?;
            }
            KIND_LEAF => {
                let start = match lo {
                    None => 0,
                    Some((k, inclusive)) => match search(p, k)? {
                        Ok(i) => {
                            if inclusive {
                                i
                            } else {
                                i + 1
                            }
                        }
                        Err(i) => i,
                    },
                };
                c.leaf = Some((id, start));
                return Ok(c);
            }
            _ => return Err(corrupt("unexpected page kind in cursor descent")),
        }
    }
    Err(corrupt("tree too deep (cycle?)"))
}

impl Cursor {
    pub fn next<S: PageStore + ?Sized>(
        &mut self,
        store: &S,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut scratch = Vec::new();
        self.next_with(store, &mut scratch, |k, v| Ok((k.to_vec(), v.to_vec())))
    }

    /// [`next`](Cursor::next), with the cell handed to `f` BORROWED — the key
    /// from the leaf page, and the value from the page too when it is inline
    /// (the common row). No allocation and no copy happen on that path, which
    /// is the point: the two `to_vec`s were ~17 ns of every scanned row
    /// (examples/agg_prof.rs), spent on bytes the row decoder immediately
    /// re-copied from. An overflow/extent value is assembled into `scratch`
    /// (reusable across calls) and handed borrowed from there.
    ///
    /// The cursor advances only when `f` succeeds, exactly as [`next`] only
    /// advanced once `read_leaf_val` had.
    pub fn next_with<S: PageStore + ?Sized, R>(
        &mut self,
        store: &S,
        scratch: &mut Vec<u8>,
        f: impl FnOnce(&[u8], &[u8]) -> Result<R>,
    ) -> Result<Option<R>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if let Some((leaf_id, pos)) = self.leaf {
                let p = store.page(leaf_id)?;
                check_node(p)?;
                if pos < nkeys(p) {
                    let (key, val) = leaf_cell(p, pos)?;
                    if let Some((hk, inc)) = &self.hi {
                        let over = match key.cmp(hk.as_slice()) {
                            Ordering::Less => false,
                            Ordering::Equal => !*inc,
                            Ordering::Greater => true,
                        };
                        if over {
                            self.done = true;
                            return Ok(None);
                        }
                    }
                    let r = match val {
                        LeafVal::Inline(v) => f(key, v)?,
                        other => {
                            *scratch = read_leaf_val(store, other)?;
                            f(key, scratch)?
                        }
                    };
                    self.leaf = Some((leaf_id, pos + 1));
                    return Ok(Some(r));
                }
                self.leaf = None;
            }
            self.climb(store)?;
        }
    }

    /// [`next`](Cursor::next) yielding ONLY the key — the value is never
    /// resolved, so an Overflow/Extent cell costs nothing to skip. For callers
    /// that delete or re-key by prefix and never read the old value (columnar
    /// segment drop, DESIGN-COLUMNAR §7.3): resolving a 512 KiB block extent
    /// just to learn its key would materialize the whole segment set.
    pub fn next_key<S: PageStore + ?Sized>(&mut self, store: &S) -> Result<Option<Vec<u8>>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if let Some((leaf_id, pos)) = self.leaf {
                let p = store.page(leaf_id)?;
                check_node(p)?;
                if pos < nkeys(p) {
                    let (key, _val) = leaf_cell(p, pos)?;
                    if let Some((hk, inc)) = &self.hi {
                        let over = match key.cmp(hk.as_slice()) {
                            Ordering::Less => false,
                            Ordering::Equal => !*inc,
                            Ordering::Greater => true,
                        };
                        if over {
                            self.done = true;
                            return Ok(None);
                        }
                    }
                    let k = key.to_vec();
                    self.leaf = Some((leaf_id, pos + 1));
                    return Ok(Some(k));
                }
                self.leaf = None;
            }
            self.climb(store)?;
        }
    }

    /// Climb until a branch has an unvisited child, then descend to that
    /// child's leftmost leaf (`self.leaf`); `self.done` when the stack
    /// empties. The between-leaves step both scan forms share.
    fn climb<S: PageStore + ?Sized>(&mut self, store: &S) -> Result<()> {
        loop {
            match self.stack.pop() {
                None => {
                    self.done = true;
                    return Ok(());
                }
                Some((branch_id, next_idx)) => {
                    let p = store.page(branch_id)?;
                    if next_idx <= nkeys(p) {
                        self.stack.push((branch_id, next_idx + 1));
                        let mut id = branch_child(p, next_idx)?;
                        let mut found_leaf = false;
                        for _ in 0..64 {
                            let q = store.page(id)?;
                            check_node(q)?;
                            match kind(q) {
                                KIND_BRANCH => {
                                    self.stack.push((id, 1));
                                    id = branch_child(q, 0)?;
                                }
                                KIND_LEAF => {
                                    found_leaf = true;
                                    break;
                                }
                                _ => return Err(corrupt("bad page kind in scan")),
                            }
                        }
                        if !found_leaf {
                            return Err(corrupt("tree too deep (cycle?)"));
                        }
                        self.leaf = Some((id, 0));
                        return Ok(());
                    }
                }
            }
        }
    }

    /// The `count(*)` fast path: how many rows a [`next`](Cursor::next) loop
    /// would have yielded from the CURRENT leaf, without parsing a cell of it
    /// unless the upper bound might cut the leaf short — then advance past the
    /// leaf. `None` when the scan is exhausted (a `Some(0)` boundary count is
    /// possible and simply precedes it).
    ///
    /// Keys are in tree order, so one comparison against the leaf's LAST key
    /// answers for the whole leaf: within the bound ⇒ every earlier cell is
    /// too, and the tail counts wholesale. Only the single boundary leaf pays
    /// per-cell work, with the exact over-bound predicate `next` applies.
    pub fn next_leaf_count<S: PageStore + ?Sized>(
        &mut self,
        store: &S,
    ) -> Result<Option<u64>> {
        loop {
            if self.done {
                return Ok(None);
            }
            let Some((leaf_id, pos)) = self.leaf else {
                self.climb(store)?;
                continue;
            };
            let p = store.page(leaf_id)?;
            check_node(p)?;
            let n = nkeys(p);
            if pos >= n {
                self.leaf = None;
                continue;
            }
            let whole = match &self.hi {
                None => true,
                Some((hk, inc)) => {
                    let (last, _) = leaf_cell(p, n - 1)?;
                    match last.cmp(hk.as_slice()) {
                        Ordering::Less => true,
                        Ordering::Equal => *inc,
                        Ordering::Greater => false,
                    }
                }
            };
            if whole {
                self.leaf = None;
                return Ok(Some((n - pos) as u64));
            }
            // The boundary leaf: count cells until the bound cuts off. Keys
            // after this leaf are larger still, so the scan ends here.
            let (hk, inc) = self.hi.as_ref().expect("bounded: !whole");
            let mut count = 0u64;
            for i in pos..n {
                let (key, _) = leaf_cell(p, i)?;
                let over = match key.cmp(hk.as_slice()) {
                    Ordering::Less => false,
                    Ordering::Equal => !*inc,
                    Ordering::Greater => true,
                };
                if over {
                    break;
                }
                count += 1;
            }
            self.done = true;
            return Ok(Some(count));
        }
    }
}

// ---------- structural range partitioning (the parallel fold's morsel cuts) ----------

/// Cut the key range `(lo, hi)` into up to `want` contiguous pieces along the
/// tree's OWN structure — the morsel boundaries of the adaptive parallel fold
/// (design/DESIGN-PARALLEL-READ.md §8).
///
/// The returned points are separator keys read from branch levels, walking one
/// level deeper only while a level holds fewer than `want - 1` in-range
/// separators (bounded by a node-visit cap, so a pathological `want` cannot
/// turn this into a tree scan). Separators at one level subtend subtrees of
/// similar leaf counts, so the pieces are balanced to within the B+tree fill
/// factor (~2×) — deterministic for a given snapshot (a pure function of the
/// tree's pages), machine-independent, and free of any row visit: nothing here
/// charges the work meter, exactly like any other descent.
///
/// Bounds are RAW encoded keys; inclusivity is irrelevant (a separator equal to
/// a bound would only mint an empty piece, and an empty piece folds to the
/// identity). Fewer points than asked — including NONE — is a normal answer: a
/// narrow range, or one living inside a single leaf, has no structure to cut
/// at, and the caller then keeps that range whole.
pub fn partition_keys<S: PageStore + ?Sized>(
    store: &S,
    root: u64,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    want: usize,
) -> Result<Vec<Vec<u8>>> {
    /// Visiting more nodes than this stops the descent with whatever the
    /// current level yielded: 8 workers × 4 morsels need 31 points, and a
    /// level of 256 branches holds thousands — the cap only ever bites on
    /// degenerate asks.
    const NODE_CAP: usize = 256;
    let mut points: Vec<Vec<u8>> = Vec::new();
    if root == 0 || want < 2 {
        return Ok(points);
    }
    let in_range = |k: &[u8]| lo.is_none_or(|l| k > l) && hi.is_none_or(|h| k < h);
    let mut frontier = vec![root];
    for _depth in 0..64 {
        let mut seps: Vec<Vec<u8>> = Vec::new();
        let mut next: Vec<u64> = Vec::new();
        let mut leaf_level = false;
        for &id in &frontier {
            let p = store.page(id)?;
            check_node(p)?;
            if kind(p) != KIND_BRANCH {
                leaf_level = true;
                break;
            }
            let n = nkeys(p);
            // Child `i` covers keys in [sep_{i-1}, sep_i) within this node
            // (open at both ends of the node). It intersects `(lo, hi)` iff
            // its upper separator clears `lo` and its lower clears `hi`.
            for i in 0..=n {
                let above_lo = match (lo, i < n) {
                    (Some(l), true) => branch_cell(p, i)?.0 > l,
                    _ => true, // no lower bound, or the rightmost child
                };
                let below_hi = match (hi, i > 0) {
                    (Some(h), true) => branch_cell(p, i - 1)?.0 < h,
                    _ => true, // no upper bound, or the leftmost child
                };
                if above_lo && below_hi {
                    next.push(branch_child(p, i)?);
                }
                if i < n {
                    let (k, _) = branch_cell(p, i)?;
                    if in_range(k) {
                        seps.push(k.to_vec());
                    }
                }
            }
        }
        if leaf_level {
            break; // the previous level's points stand
        }
        // Concatenated per-node separators are ascending across the frontier:
        // sibling subtrees are ordered, and each node's separators sit
        // strictly inside its subtree's key span.
        if !seps.is_empty() {
            points = seps;
        }
        if points.len() >= want - 1 || next.len() > NODE_CAP || next.is_empty() {
            break;
        }
        frontier = next;
    }
    // Thin an over-dense level to `want - 1` evenly spaced points, keeping the
    // arithmetic integer-only so every machine picks the same cuts.
    if points.len() > want - 1 {
        let s = points.len();
        let mut picked: Vec<Vec<u8>> = (1..want).map(|i| points[i * s / want].clone()).collect();
        picked.dedup();
        points = picked;
    }
    Ok(points)
}

