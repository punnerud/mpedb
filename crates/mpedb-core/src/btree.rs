//! Copy-on-write B+tree over a [`PageStore`].
//!
//! - Keys are opaque byte strings in memcmp order (already order-encoded by
//!   `mpedb_types::keycode`), at most [`MAX_KEY`] bytes.
//! - Values up to [`MAX_INLINE_VAL`] bytes are stored inline; larger values
//!   spill to overflow page chains.
//! - Every mutation copies the root-to-leaf path (COW): committed pages are
//!   never modified, which is what makes lock-free snapshot readers safe.
//! - Deletion frees emptied nodes and merges adjacent under-filled leaves;
//!   under-filled *branch* nodes are only collapsed at the root (a mid-tree
//!   single-child branch is valid and rare enough not to chase in Phase 1).
//!
//! Page id 0 is a sentinel meaning "empty tree" (page 0 of the file is a meta
//! page, so it can never be a tree node).
//!
//! Node layout (4096-byte pages):
//! ```text
//! 0   u8   kind: 1=branch 2=leaf 3=overflow
//! 1   u8   flags (unused)
//! 2   u16  nkeys
//! 4   u16  cell_start (lowest cell offset; cells grow down from page end)
//! 6   u16  overflow: bytes of payload in this page (unused otherwise)
//! 8   u64  branch: leftmost child ─ overflow: next page id
//! 16  slot array: nkeys × u16 cell offsets, ordered by key
//! ...        free space
//! ...        cells
//! ```
//!
//! Leaf cell:   u16 key_len ‖ u8 vkind ‖ key ‖ (vkind=0: u16 val_len ‖ val)
//!                                          ‖ (vkind=1: u32 total_len ‖ u64 first_overflow_page)
//! Branch cell: u16 key_len ‖ key ‖ u64 child   (separator: child holds keys ≥ separator)

use crate::pagestore::{cow, PageStore};
use mpedb_types::{Error, Result, PAGE_SIZE};
use std::cmp::Ordering;

pub const MAX_KEY: usize = 976;
pub const MAX_INLINE_VAL: usize = 1024;

const HDR: usize = 16;
const KIND_BRANCH: u8 = 1;
const KIND_LEAF: u8 = 2;
const KIND_OVERFLOW: u8 = 3;
const OVERFLOW_CAP: usize = PAGE_SIZE - HDR;

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("btree: {msg}"))
}

// ---------- header accessors ----------

fn kind(p: &[u8]) -> u8 {
    p[0]
}
fn nkeys(p: &[u8]) -> usize {
    u16::from_le_bytes([p[2], p[3]]) as usize
}
fn set_nkeys(p: &mut [u8], n: usize) {
    p[2..4].copy_from_slice(&(n as u16).to_le_bytes());
}
fn cell_start(p: &[u8]) -> usize {
    u16::from_le_bytes([p[4], p[5]]) as usize
}
fn set_cell_start(p: &mut [u8], v: usize) {
    p[4..6].copy_from_slice(&(v as u16).to_le_bytes());
}
fn extra(p: &[u8]) -> u64 {
    u64::from_le_bytes(p[8..16].try_into().unwrap())
}
fn set_extra(p: &mut [u8], v: u64) {
    p[8..16].copy_from_slice(&v.to_le_bytes());
}
fn slot(p: &[u8], i: usize) -> usize {
    let off = HDR + i * 2;
    u16::from_le_bytes([p[off], p[off + 1]]) as usize
}
fn set_slot(p: &mut [u8], i: usize, v: usize) {
    let off = HDR + i * 2;
    p[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn init_node(p: &mut [u8], k: u8) {
    p[..HDR].fill(0);
    p[0] = k;
    set_cell_start(p, PAGE_SIZE);
}

/// Validate a freshly-loaded node's header before any slot arithmetic.
/// Hostile bytes in the shared mapping must yield `Error::Corrupt`, never an
/// out-of-bounds panic: with nkeys and cell_start bounded here, every slot
/// read stays inside the page and cell offsets are checked in `cell_bytes`.
fn check_node(p: &[u8]) -> Result<()> {
    match kind(p) {
        KIND_LEAF | KIND_BRANCH => {}
        KIND_OVERFLOW => return Ok(()), // uses its own validated fields
        _ => return Err(corrupt("bad page kind")),
    }
    let n = nkeys(p);
    let cs = cell_start(p);
    if n > (PAGE_SIZE - HDR) / 2 || !(HDR..=PAGE_SIZE).contains(&cs) || HDR + n * 2 > cs {
        return Err(corrupt("corrupt node header"));
    }
    Ok(())
}

// ---------- cell accessors (bounds-checked: pages may be corrupt) ----------

fn cell_bytes(p: &[u8], i: usize) -> Result<&[u8]> {
    let off = slot(p, i);
    if !(HDR..PAGE_SIZE).contains(&off) {
        return Err(corrupt("cell offset out of range"));
    }
    Ok(&p[off..])
}

/// (key, value-part) of a leaf cell. value-part starts at vkind byte.
fn leaf_cell(p: &[u8], i: usize) -> Result<(&[u8], LeafVal<'_>)> {
    let c = cell_bytes(p, i)?;
    if c.len() < 3 {
        return Err(corrupt("truncated leaf cell"));
    }
    let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
    let vkind = c[2];
    let key = c.get(3..3 + klen).ok_or_else(|| corrupt("truncated key"))?;
    let rest = &c[3 + klen..];
    let val = match vkind {
        0 => {
            if rest.len() < 2 {
                return Err(corrupt("truncated inline len"));
            }
            let vlen = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            LeafVal::Inline(
                rest.get(2..2 + vlen)
                    .ok_or_else(|| corrupt("truncated inline value"))?,
            )
        }
        1 => {
            if rest.len() < 12 {
                return Err(corrupt("truncated overflow ref"));
            }
            LeafVal::Overflow {
                total_len: u32::from_le_bytes(rest[0..4].try_into().unwrap()),
                first_page: u64::from_le_bytes(rest[4..12].try_into().unwrap()),
            }
        }
        2 => {
            if rest.len() < EXTENT_REF_LEN {
                return Err(corrupt("truncated extent ref"));
            }
            let start_page = u64::from_le_bytes(rest[0..8].try_into().unwrap());
            let total_len = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            let npages = u32::from_le_bytes(rest[16..20].try_into().unwrap());
            // The two sizes must agree (DESIGN-BLOBEXTENT §3.1): a corrupt
            // length must not let a reader walk past the run, and checked
            // math keeps hostile values at Corrupt instead of a wrap.
            let expect = total_len
                .checked_add(PAGE_SIZE as u64 - 1)
                .ok_or_else(|| corrupt("extent length overflow"))?
                / PAGE_SIZE as u64;
            if u64::from(npages) != expect || npages == 0 {
                return Err(corrupt("extent length/page-count mismatch"));
            }
            if start_page
                .checked_add(u64::from(npages))
                .is_none()
            {
                return Err(corrupt("extent run overflows the page space"));
            }
            LeafVal::Extent { start_page, total_len, npages }
        }
        // vkind=3 is reserved for base+diff (#52) — refused by name.
        3 => return Err(corrupt("extent kind base+diff is reserved but not yet supported")),
        _ => return Err(corrupt("bad vkind")),
    };
    Ok((key, val))
}

/// Encoded size of a `vkind=2` reference: start_page ‖ total_len ‖ npages.
pub const EXTENT_REF_LEN: usize = 8 + 8 + 4;

#[derive(Clone, Copy)]
enum LeafVal<'a> {
    Inline(&'a [u8]),
    Overflow { total_len: u32, first_page: u64 },
    Extent { start_page: u64, total_len: u64, npages: u32 },
}

fn leaf_cell_len(p: &[u8], i: usize) -> Result<usize> {
    let (key, val) = leaf_cell(p, i)?;
    Ok(match val {
        LeafVal::Inline(v) => 3 + key.len() + 2 + v.len(),
        LeafVal::Overflow { .. } => 3 + key.len() + 12,
        LeafVal::Extent { .. } => 3 + key.len() + EXTENT_REF_LEN,
    })
}

fn branch_cell(p: &[u8], i: usize) -> Result<(&[u8], u64)> {
    let c = cell_bytes(p, i)?;
    if c.len() < 2 {
        return Err(corrupt("truncated branch cell"));
    }
    let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
    let key = c.get(2..2 + klen).ok_or_else(|| corrupt("truncated key"))?;
    let child_raw = c
        .get(2 + klen..2 + klen + 8)
        .ok_or_else(|| corrupt("truncated child"))?;
    Ok((key, u64::from_le_bytes(child_raw.try_into().unwrap())))
}

fn branch_cell_len(p: &[u8], i: usize) -> Result<usize> {
    let (key, _) = branch_cell(p, i)?;
    Ok(2 + key.len() + 8)
}

/// Child page for descent position `idx` (0..=nkeys).
fn branch_child(p: &[u8], idx: usize) -> Result<u64> {
    if idx == 0 {
        Ok(extra(p))
    } else {
        Ok(branch_cell(p, idx - 1)?.1)
    }
}

fn set_branch_child(p: &mut [u8], idx: usize, child: u64) -> Result<()> {
    if idx == 0 {
        set_extra(p, child);
        return Ok(());
    }
    let off = slot(p, idx - 1);
    let klen = u16::from_le_bytes([p[off], p[off + 1]]) as usize;
    let coff = off + 2 + klen;
    if coff + 8 > PAGE_SIZE {
        return Err(corrupt("child pointer out of range"));
    }
    p[coff..coff + 8].copy_from_slice(&child.to_le_bytes());
    Ok(())
}

// ---------- free space management ----------

fn free_space(p: &[u8]) -> usize {
    cell_start(p).saturating_sub(HDR + nkeys(p) * 2)
}

fn used_cell_bytes(p: &[u8]) -> Result<usize> {
    let n = nkeys(p);
    let mut total = 0;
    for i in 0..n {
        total += if kind(p) == KIND_LEAF {
            leaf_cell_len(p, i)?
        } else {
            branch_cell_len(p, i)?
        };
    }
    Ok(total)
}

/// Rewrite all cells packed against the page end (defragmentation).
fn compact(p: &mut [u8]) -> Result<()> {
    let n = nkeys(p);
    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let len = if kind(p) == KIND_LEAF {
            leaf_cell_len(p, i)?
        } else {
            branch_cell_len(p, i)?
        };
        let off = slot(p, i);
        cells.push(p[off..off + len].to_vec());
    }
    let mut pos = PAGE_SIZE;
    for (i, c) in cells.iter().enumerate() {
        pos -= c.len();
        p[pos..pos + c.len()].copy_from_slice(c);
        set_slot(p, i, pos);
    }
    set_cell_start(p, pos);
    Ok(())
}

/// Insert `cell` at slot `i`, compacting first if fragmented. Caller must
/// have verified it fits (`fits`).
fn insert_cell(p: &mut [u8], i: usize, cell: &[u8]) -> Result<()> {
    if free_space(p) < cell.len() + 2 {
        compact(p)?;
        if free_space(p) < cell.len() + 2 {
            return Err(Error::Internal("insert_cell without space".into()));
        }
    }
    let n = nkeys(p);
    let pos = cell_start(p) - cell.len();
    p[pos..pos + cell.len()].copy_from_slice(cell);
    set_cell_start(p, pos);
    // shift slots [i..n) right by one
    for j in (i..n).rev() {
        let v = slot(p, j);
        set_slot(p, j + 1, v);
    }
    set_slot(p, i, pos);
    set_nkeys(p, n + 1);
    Ok(())
}

fn remove_cell(p: &mut [u8], i: usize) {
    let n = nkeys(p);
    for j in i + 1..n {
        let v = slot(p, j);
        set_slot(p, j - 1, v);
    }
    set_nkeys(p, n - 1);
    // freed cell bytes stay as fragmentation until the next compact()
}

/// Would a new cell of `cell_len` bytes fit (possibly after compaction)?
fn fits(p: &[u8], cell_len: usize) -> Result<bool> {
    if free_space(p) >= cell_len + 2 {
        return Ok(true);
    }
    let used = used_cell_bytes(p)?;
    Ok(PAGE_SIZE.saturating_sub(HDR + (nkeys(p) + 1) * 2 + used) >= cell_len)
}

/// The half-open byte range `[prefix_end, suffix_start)` of a node page that
/// NO engine code ever reads back, and therefore is safe to omit from a WAL
/// record and zero-fill on replay ("lean records", DESIGN.md §5.4.1).
/// Reconstructing a page as `[0, prefix_end)` ++ zeros ++ `[suffix_start,
/// PAGE_SIZE)` is *observationally identical* to the live page for every read
/// path — even though the omitted bytes are not guaranteed zero in memory.
///
/// Layout proof (audited against every reader in this file):
/// - LEAF/BRANCH: the live bytes are the header+slot array `[0, HDR+nkeys*2)`
///   and the packed cells `[cell_start, PAGE_SIZE)`. The middle
///   `[HDR+nkeys*2, cell_start)` is free space / dead-cell fragmentation:
///   `cell_bytes` only slices from offsets `>= cell_start`, and
///   `compact`/`free_space`/`used_cell_bytes` touch only header, slots and
///   cells. It is NOT zero (a reused page, or one after `remove_cell`, carries
///   stale bytes there), so zeroing it on replay is a real byte change but
///   never an observable one.
/// - OVERFLOW: live bytes are `[0, HDR+payload_len)` (payload_len at bytes
///   6..8); `read_overflow` reads exactly `[HDR, HDR+len)`. The tail is unused
///   padding.
/// - anything else (corrupt/foreign header, wrong page length): no elision —
///   the whole page is the prefix, so replay stays byte-identical.
///
/// Invariant: `prefix_end <= suffix_start <= PAGE_SIZE`.
pub fn used_span(p: &[u8]) -> (usize, usize) {
    if p.len() != PAGE_SIZE {
        return (PAGE_SIZE, PAGE_SIZE);
    }
    match kind(p) {
        KIND_LEAF | KIND_BRANCH if check_node(p).is_ok() => {
            // check_node guarantees HDR + nkeys*2 <= cell_start <= PAGE_SIZE.
            (HDR + nkeys(p) * 2, cell_start(p))
        }
        KIND_OVERFLOW => {
            let payload = u16::from_le_bytes([p[6], p[7]]) as usize;
            ((HDR + payload).min(PAGE_SIZE), PAGE_SIZE)
        }
        _ => (PAGE_SIZE, PAGE_SIZE),
    }
}

// ---------- key search ----------

fn key_at(p: &[u8], i: usize) -> Result<&[u8]> {
    if kind(p) == KIND_LEAF {
        Ok(leaf_cell(p, i)?.0)
    } else {
        Ok(branch_cell(p, i)?.0)
    }
}

/// Binary search. Ok(i) = exact match at slot i; Err(i) = first slot > key.
fn search(p: &[u8], key: &[u8]) -> Result<std::result::Result<usize, usize>> {
    let (mut lo, mut hi) = (0usize, nkeys(p));
    while lo < hi {
        let mid = (lo + hi) / 2;
        match key_at(p, mid)?.cmp(key) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Ok(mid)),
        }
    }
    Ok(Err(lo))
}

/// Descent position in a branch: number of separators ≤ key.
fn descent_index(p: &[u8], key: &[u8]) -> Result<usize> {
    Ok(match search(p, key)? {
        Ok(i) => i + 1,
        Err(i) => i,
    })
}

// ---------- overflow chains ----------

/// #40: a nanosecond clock, but only when the `leakstat` feature is on — a
/// blob write does this per PAGE, and `Instant::now()` is ~25 ns, so it must
/// vanish entirely in a normal build.
#[cfg(feature = "leakstat")]
macro_rules! ovf_timed {
    ($ctr:expr, $body:expr) => {{
        let __t = std::time::Instant::now();
        let __r = $body;
        crate::engine::leakstat::add(&$ctr, __t.elapsed().as_nanos() as u64);
        __r
    }};
}
#[cfg(not(feature = "leakstat"))]
macro_rules! ovf_timed {
    ($ctr:expr, $body:expr) => {{
        let _ = &$ctr;
        $body
    }};
}

fn write_overflow<S: PageStore + ?Sized>(store: &mut S, data: &mut Payload<'_>) -> Result<u64> {
    use crate::engine::leakstat;
    let total = data.len();
    // Destructure ONCE: a Stream's head and its source both come out of the same
    // `&mut`, and matching twice would borrow it twice. The reader must live
    // across the whole loop — a Stream carries its position in the source, a
    // Slices cursor carries its own.
    let (head, mut reader): (&[u8], PayloadReader) = match data {
        Payload::Stream { head, src } => (head, PayloadReader::Stream(*src)),
        other => (&[], PayloadReader::Slices(PayloadCursor::new_owned(other.pieces()))),
    };
    let mut head_done = 0usize;
    let mut first = 0u64;
    let mut prev = 0u64;
    let mut written = 0usize;
    loop {
        leakstat::bump(&leakstat::OVF_PAGES);
        let take = OVERFLOW_CAP.min(total - written);
        // `alloc_raw`, not `alloc`: this page is about to have every byte it
        // owns defined right here — header, payload, then the tail below — so
        // `alloc`'s full-page fill(0) is a 4 KiB memset thrown away on the hot
        // path of every blob write.
        let id = ovf_timed!(leakstat::OVF_NS_ALLOC, store.alloc_raw())?;
        let p = store.page_mut(id)?;
        init_node(p, KIND_OVERFLOW);
        p[6..8].copy_from_slice(&(take as u16).to_le_bytes());
        // The row head (Stream only) precedes the source's bytes.
        let mut filled = 0usize;
        if head_done < head.len() {
            let n = take.min(head.len() - head_done);
            p[HDR..HDR + n].copy_from_slice(&head[head_done..head_done + n]);
            head_done += n;
            filled = n;
        }
        if filled < take {
            reader.fill(&mut p[HDR + filled..HDR + take])?;
        }
        // The tail. `read_overflow` never looks past HDR+take and there is no
        // per-data-page checksum (DESIGN.md §5.4.1), so leaving the previous
        // tenant's bytes here would be *correct* — and would quietly keep
        // deleted rows readable in a file that gets copied around. Zero it.
        p[HDR + take..].fill(0);
        written += take;
        if first == 0 {
            first = id;
        } else {
            ovf_timed!(leakstat::OVF_NS_CHAIN, {
                let prev_p = store.page_mut(prev)?;
                set_extra(prev_p, id);
                Ok::<(), Error>(())
            })?;
        }
        prev = id;
        if written == total {
            break;
        }
    }
    Ok(first)
}

fn read_overflow<S: PageStore + ?Sized>(
    store: &S,
    first_page: u64,
    total_len: u32,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(total_len as usize);
    let mut id = first_page;
    let mut hops = 0usize;
    while id != 0 {
        if hops > (total_len as usize / OVERFLOW_CAP) + 2 {
            return Err(corrupt("overflow chain too long"));
        }
        hops += 1;
        let p = store.page(id)?;
        if kind(p) != KIND_OVERFLOW {
            return Err(corrupt("bad overflow page kind"));
        }
        let len = u16::from_le_bytes([p[6], p[7]]) as usize;
        if len > OVERFLOW_CAP {
            return Err(corrupt("overflow chunk too large"));
        }
        out.extend_from_slice(&p[HDR..HDR + len]);
        id = extra(p);
    }
    if out.len() != total_len as usize {
        return Err(corrupt("overflow length mismatch"));
    }
    Ok(out)
}

fn free_overflow<S: PageStore + ?Sized>(store: &mut S, first_page: u64) -> Result<()> {
    let mut id = first_page;
    let mut hops = 0usize;
    while id != 0 {
        if hops > PAGE_SIZE * 16 {
            return Err(corrupt("overflow chain cycle"));
        }
        hops += 1;
        let p = store.page(id)?;
        // a corrupt vkind/first_page must not let us free arbitrary live pages
        if kind(p) != KIND_OVERFLOW {
            return Err(corrupt("free_overflow reached a non-overflow page"));
        }
        let next = extra(p);
        store.free(id)?;
        id = next;
    }
    Ok(())
}

/// What a replaced/deleted leaf cell owned outside the page tree.
enum OldVal {
    None,
    Overflow(u64),
    Extent(u64, u32),
}

/// Free it — the ONE place replace and delete route through, so overflow
/// chains and extents cannot drift in ownership handling.
fn free_old_val<S: PageStore + ?Sized>(store: &mut S, old: OldVal) -> Result<()> {
    match old {
        OldVal::None => Ok(()),
        OldVal::Overflow(first) => free_overflow(store, first),
        OldVal::Extent(start, npages) => store.free_extent(start, npages),
    }
}

fn read_leaf_val<S: PageStore + ?Sized>(store: &S, val: LeafVal<'_>) -> Result<Vec<u8>> {
    match val {
        LeafVal::Inline(v) => Ok(v.to_vec()),
        LeafVal::Overflow {
            total_len,
            first_page,
        } => read_overflow(store, first_page, total_len),
        LeafVal::Extent { start_page, total_len, .. } => {
            let mut out = Vec::with_capacity(total_len.min(1 << 20) as usize);
            store.read_extent(start_page, total_len, &mut out)?;
            if out.len() as u64 != total_len {
                return Err(corrupt("extent read returned wrong length"));
            }
            Ok(out)
        }
    }
}

// ---------- public operations ----------

/// Where one stored value's bytes live — the descriptor the chunked blob
/// reader (#50 B4) walks WITHOUT materializing the value. `Inline` copies at
/// open (bounded by a leaf cell, ≤ ~4 KB); the other two hold coordinates.
#[derive(Debug, Clone)]
pub enum ValueLoc {
    Inline(Vec<u8>),
    Overflow { total_len: u32, first_page: u64 },
    Extent { start_page: u64, total_len: u64 },
}

impl ValueLoc {
    pub fn len(&self) -> u64 {
        match self {
            ValueLoc::Inline(v) => v.len() as u64,
            ValueLoc::Overflow { total_len, .. } => *total_len as u64,
            ValueLoc::Extent { total_len, .. } => *total_len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The [`ValueLoc`] for `key`, or `None` when absent — [`get`]'s descent
/// without the read.
pub fn value_loc<S: PageStore + ?Sized>(
    store: &S,
    root: u64,
    key: &[u8],
) -> Result<Option<ValueLoc>> {
    if root == 0 {
        return Ok(None);
    }
    let mut id = root;
    for _ in 0..64 {
        let p = store.page(id)?;
        check_node(p)?;
        match kind(p) {
            KIND_BRANCH => id = branch_child(p, descent_index(p, key)?)?,
            KIND_LEAF => {
                return match search(p, key)? {
                    Ok(i) => Ok(Some(match leaf_cell(p, i)?.1 {
                        LeafVal::Inline(v) => ValueLoc::Inline(v.to_vec()),
                        LeafVal::Overflow { total_len, first_page } => {
                            ValueLoc::Overflow { total_len, first_page }
                        }
                        LeafVal::Extent { start_page, total_len, .. } => {
                            ValueLoc::Extent { start_page, total_len }
                        }
                    })),
                    Err(_) => Ok(None),
                };
            }
            _ => return Err(corrupt("unexpected page kind in descent")),
        }
    }
    Err(corrupt("tree too deep (cycle?)"))
}

/// Copy value bytes `[off, off + out.len())` into `out`. The caller bounds
/// the window (`off + out.len() ≤ loc.len()`); overshooting is an error, not
/// a short read. Overflow chains skip whole pages to the offset; an extent is
/// pure offset math into its contiguous run — one bounded copy either way,
/// which is the honest cost the chunked API promises (zero-copy is NOT
/// offered on live databases; FrozenDb (#22) is where borrowing is sound).
pub fn read_value_range<S: PageStore + ?Sized>(
    store: &S,
    loc: &ValueLoc,
    off: u64,
    out: &mut [u8],
) -> Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    let end = off
        .checked_add(out.len() as u64)
        .ok_or_else(|| corrupt("value range overflows"))?;
    if end > loc.len() {
        return Err(corrupt("value range past the end of the value"));
    }
    match loc {
        ValueLoc::Inline(v) => {
            out.copy_from_slice(&v[off as usize..off as usize + out.len()]);
            Ok(())
        }
        ValueLoc::Overflow { total_len, first_page } => {
            let mut id = *first_page;
            let mut skip = off as usize;
            let mut done = 0usize;
            let mut hops = 0usize;
            while id != 0 && done < out.len() {
                if hops > (*total_len as usize / OVERFLOW_CAP) + 2 {
                    return Err(corrupt("overflow chain too long"));
                }
                hops += 1;
                let p = store.page(id)?;
                if kind(p) != KIND_OVERFLOW {
                    return Err(corrupt("bad overflow page kind"));
                }
                let len = u16::from_le_bytes([p[6], p[7]]) as usize;
                if len > OVERFLOW_CAP {
                    return Err(corrupt("overflow chunk too large"));
                }
                if skip >= len {
                    skip -= len;
                } else {
                    let take = (len - skip).min(out.len() - done);
                    out[done..done + take].copy_from_slice(&p[HDR + skip..HDR + skip + take]);
                    done += take;
                    skip = 0;
                }
                id = extra(p);
            }
            if done < out.len() {
                return Err(corrupt("overflow chain shorter than its total_len"));
            }
            Ok(())
        }
        ValueLoc::Extent { start_page, .. } => {
            // Sub-page reads land on the page holding `off`, then trim the
            // in-page remainder — read_extent has no offset parameter, and
            // growing the PageStore trait for one caller is not worth it.
            let page = start_page + off / PAGE_SIZE as u64;
            let skip = (off % PAGE_SIZE as u64) as usize;
            let mut scratch = Vec::new();
            store.read_extent(page, (skip + out.len()) as u64, &mut scratch)?;
            if scratch.len() != skip + out.len() {
                return Err(corrupt("extent read returned wrong length"));
            }
            out.copy_from_slice(&scratch[skip..]);
            Ok(())
        }
    }
}

pub fn get<S: PageStore + ?Sized>(store: &S, root: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
    if root == 0 {
        return Ok(None);
    }
    let mut id = root;
    for _ in 0..64 {
        let p = store.page(id)?;
        check_node(p)?;
        match kind(p) {
            KIND_BRANCH => id = branch_child(p, descent_index(p, key)?)?,
            KIND_LEAF => {
                return match search(p, key)? {
                    Ok(i) => {
                        let (_, val) = leaf_cell(p, i)?;
                        Ok(Some(read_leaf_val(store, val)?))
                    }
                    Err(_) => Ok(None),
                };
            }
            _ => return Err(corrupt("unexpected page kind in descent")),
        }
    }
    Err(corrupt("tree too deep (cycle?)"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// Fail with `existed = true` if the key is present (engine maps this to
    /// a PK/UNIQUE violation).
    InsertOnly,
    Upsert,
}

pub struct InsertOutcome {
    pub new_root: u64,
    pub existed: bool,
}

enum Ins {
    Done { id: u64, existed: bool },
    Split { left: u64, sep: Vec<u8>, right: u64, existed: bool },
}

/// A row payload that may not be contiguous.
///
/// `Parts` is the concatenation of its slices, and it exists so a large value
/// never has to BE concatenated. `row::encode_row` used to materialise the whole
/// row — a 16 MiB blob included — into a fresh heap `Vec` whose only purpose was
/// to be handed here and copied straight back out into overflow pages. That
/// buffer measured **10.1 ms of a 23.5 ms 16 MiB insert (~42%)**: a fresh malloc
/// faults its anonymous pages exactly like the file mapping does, and then the
/// memcpy runs. The blob crossed memory three times and two of them did no work.
///
/// The inline path (<= `MAX_INLINE_VAL`, i.e. almost every row) still copies —
/// the leaf cell must be contiguous — but at <= 1 KiB that is free. The overflow
/// path consumes the parts straight into the pages.
///
/// This is NOT a format change: the pages and cells come out byte-for-byte what
/// a `Flat` payload of the same bytes produces.
/// A large value produced on demand, so it never has to be resident (#43).
///
/// **Pull, not push.** The engine asks for bytes as fast as it can write them,
/// rather than handing the caller a writer to push into. That is not a style
/// choice: a push writer would hold the WRITER LOCK across caller code, so a
/// blob streamed off a socket would block every other writer in the database for
/// as long as the network took. Pulling keeps the lock's duration a property of
/// the engine.
///
/// `next_into` must fill the WHOLE buffer; it is only ever handed a buffer it
/// has the bytes for (`len` is declared up front, and the engine never asks past
/// it).
pub trait BlobSource {
    /// Total bytes this source will produce. Declared up front because the leaf
    /// cell records the value's length and the row's fixed section needs it —
    /// there is no "until EOF" here.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn next_into(&mut self, buf: &mut [u8]) -> Result<()>;
}

pub enum Payload<'a> {
    Flat(&'a [u8]),
    /// Logically `parts.concat()`, never actually concatenated.
    Parts(&'a [&'a [u8]]),
    /// `head` (the row's bitmap + fixed section) followed by `src`'s bytes.
    /// Nothing large is ever resident: the engine pulls a page at a time.
    Stream {
        head: &'a [u8],
        src: &'a mut dyn BlobSource,
    },
    /// A payload ALREADY written into an extent run (DESIGN-BLOBEXTENT §3.1):
    /// the store placed the bytes before this insert was called, and the leaf
    /// carries only the 20-byte `vkind=2` reference. Payload-before-reference
    /// is the crash argument, so the order is a contract, not a convenience.
    ExtentRef {
        start_page: u64,
        total_len: u64,
        npages: u32,
    },
}

impl<'a> Payload<'a> {
    pub fn len(&self) -> usize {
        match self {
            Payload::Flat(b) => b.len(),
            Payload::Parts(ps) => ps.iter().map(|p| p.len()).sum(),
            Payload::Stream { head, src } => head.len() + src.len(),
            // The VALUE the tree stores is the 20-byte reference; the
            // payload's own size lives behind it and never counts against
            // inline/split thresholds.
            Payload::ExtentRef { .. } => EXTENT_REF_LEN,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The pieces, in order. `Flat` is just a one-piece `Parts`, so every
    /// consumer below has exactly one code path and the two cannot drift.
    fn pieces(&self) -> &[&'a [u8]] {
        match self {
            Payload::Flat(b) => std::slice::from_ref(b),
            Payload::Parts(ps) => ps,
            // Only the inline path calls this, and a Stream is by definition too
            // big to be inline — `insert_rec` checks the length first.
            // An ExtentRef is encoded by the leaf writer itself (vkind=2),
            // never through the byte-copy path.
            Payload::Stream { .. } | Payload::ExtentRef { .. } => &[],
        }
    }
}

/// Reads a `Payload`'s bytes in page-sized bites, whatever form it is in.
/// One code path for all three so they cannot drift.
enum PayloadReader<'a, 'p> {
    Slices(PayloadCursor<'a>),
    Stream(&'p mut dyn BlobSource),
}

impl PayloadReader<'_, '_> {
    /// Fill `buf` completely.
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        match self {
            PayloadReader::Slices(cur) => {
                let mut n = 0;
                while n < buf.len() {
                    let chunk = cur
                        .next_chunk(buf.len() - n)
                        .ok_or_else(|| corrupt("payload shorter than its own length"))?;
                    buf[n..n + chunk.len()].copy_from_slice(chunk);
                    n += chunk.len();
                }
                Ok(())
            }
            PayloadReader::Stream(src) => src.next_into(buf),
        }
    }
}

/// Walks a `Payload`'s bytes, handing out at most `want` at a time without
/// joining the pieces. `write_overflow` fills fixed-size pages from a stream of
/// arbitrarily-sized slices, so the two boundaries do not line up.
struct PayloadCursor<'a> {
    pieces: &'a [&'a [u8]],
    ix: usize,
    off: usize,
}

impl<'a> PayloadCursor<'a> {
    fn new_owned(pieces: &'a [&'a [u8]]) -> Self {
        PayloadCursor { pieces, ix: 0, off: 0 }
    }

    /// Next run of <= `want` contiguous bytes, or `None` when drained.
    fn next_chunk(&mut self, want: usize) -> Option<&'a [u8]> {
        while self.ix < self.pieces.len() {
            let piece = self.pieces[self.ix];
            if self.off >= piece.len() {
                self.ix += 1;
                self.off = 0;
                continue;
            }
            let take = want.min(piece.len() - self.off);
            let out = &piece[self.off..self.off + take];
            self.off += take;
            return Some(out);
        }
        None
    }
}

fn make_leaf_cell(key: &[u8], val: &Payload<'_>, overflow_page: u64) -> Vec<u8> {
    let mut c = Vec::with_capacity(3 + key.len() + 2 + val.len().min(MAX_INLINE_VAL));
    c.extend_from_slice(&(key.len() as u16).to_le_bytes());
    if let Payload::ExtentRef { start_page, total_len, npages } = val {
        c.push(2);
        c.extend_from_slice(key);
        c.extend_from_slice(&start_page.to_le_bytes());
        c.extend_from_slice(&total_len.to_le_bytes());
        c.extend_from_slice(&npages.to_le_bytes());
        return c;
    }
    if overflow_page == 0 {
        c.push(0);
        c.extend_from_slice(key);
        c.extend_from_slice(&(val.len() as u16).to_le_bytes());
        // inline only, so <= MAX_INLINE_VAL bytes however many pieces it is
        for piece in val.pieces() {
            c.extend_from_slice(piece);
        }
    } else {
        c.push(1);
        c.extend_from_slice(key);
        c.extend_from_slice(&(val.len() as u32).to_le_bytes());
        c.extend_from_slice(&overflow_page.to_le_bytes());
    }
    c
}

fn make_branch_cell(key: &[u8], child: u64) -> Vec<u8> {
    let mut c = Vec::with_capacity(2 + key.len() + 8);
    c.extend_from_slice(&(key.len() as u16).to_le_bytes());
    c.extend_from_slice(key);
    c.extend_from_slice(&child.to_le_bytes());
    c
}

/// Split a dirty, over-full node conceptually holding `cells` (all cells of
/// the page plus the pending one, in key order). Writes left cells into
/// `page_id` and right cells into a fresh page. Returns (sep, right_id).
fn split_dirty<S: PageStore + ?Sized>(
    store: &mut S,
    page_id: u64,
    node_kind: u8,
    cells: Vec<Vec<u8>>,
    leftmost: u64,
) -> Result<(Vec<u8>, u64)> {
    // Pick a split point where BOTH resulting nodes fit (a naive half-split
    // can overload one side with legal max-size cells), preferring the most
    // balanced feasible point.
    let sizes: Vec<usize> = cells.iter().map(|c| c.len() + 2).collect();
    let total: usize = sizes.iter().sum();
    let cap = PAGE_SIZE - HDR;
    let mut best: Option<(usize, usize)> = None; // (split_at, imbalance)
    let mut left_acc = 0usize;
    for s in 1..cells.len() {
        left_acc += sizes[s - 1];
        let right = if node_kind == KIND_LEAF {
            total - left_acc
        } else {
            // the separator cell at `s` is promoted, not stored in either side
            total - left_acc - sizes[s]
        };
        if left_acc <= cap && right <= cap {
            let d = left_acc.abs_diff(total / 2);
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((s, d));
            }
        }
    }
    let split_at = best
        .ok_or_else(|| corrupt("no feasible split point (oversized cells)"))?
        .0;

    let right_id = store.alloc()?;
    let (sep, right_leftmost, left_cells, right_cells) = if node_kind == KIND_LEAF {
        let key = {
            let c = &cells[split_at];
            let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
            c[3..3 + klen].to_vec()
        };
        (key, 0u64, &cells[..split_at], &cells[split_at..])
    } else {
        // promote the separator: its child becomes the right node's leftmost
        let c = &cells[split_at];
        let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
        let key = c[2..2 + klen].to_vec();
        let child = u64::from_le_bytes(c[2 + klen..2 + klen + 8].try_into().unwrap());
        (key, child, &cells[..split_at], &cells[split_at + 1..])
    };

    let rebuild = |p: &mut [u8], cells: &[Vec<u8>], lm: u64| -> Result<()> {
        init_node(p, node_kind);
        if node_kind == KIND_BRANCH {
            set_extra(p, lm);
        }
        for (i, c) in cells.iter().enumerate() {
            insert_cell(p, i, c)?;
        }
        Ok(())
    };
    {
        let p = store.page_mut(page_id)?;
        rebuild(p, left_cells, leftmost)?;
    }
    {
        let p = store.page_mut(right_id)?;
        rebuild(p, right_cells, right_leftmost)?;
    }
    Ok((sep, right_id))
}

fn all_cells(p: &[u8]) -> Result<Vec<Vec<u8>>> {
    let n = nkeys(p);
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let len = if kind(p) == KIND_LEAF {
            leaf_cell_len(p, i)?
        } else {
            branch_cell_len(p, i)?
        };
        let off = slot(p, i);
        out.push(p[off..off + len].to_vec());
    }
    Ok(out)
}

fn insert_rec<S: PageStore + ?Sized>(
    store: &mut S,
    page_id: u64,
    key: &[u8],
    val: &mut Payload<'_>,
    mode: InsertMode,
) -> Result<Ins> {
    let node_kind = {
        let p = store.page(page_id)?;
        check_node(p)?;
        kind(p)
    };
    match node_kind {
        KIND_LEAF => {
            let (pos, existing) = match search(store.page(page_id)?, key)? {
                Ok(i) => (i, true),
                Err(i) => (i, false),
            };
            if existing && mode == InsertMode::InsertOnly {
                return Ok(Ins::Done {
                    id: page_id,
                    existed: true,
                });
            }
            let id = cow(store, page_id)?;
            // replace: drop the old cell (and its overflow chain) first
            if existing {
                let old = {
                    let p = store.page(id)?;
                    match leaf_cell(p, pos)?.1 {
                        LeafVal::Overflow { first_page, .. } => OldVal::Overflow(first_page),
                        LeafVal::Extent { start_page, npages, .. } => {
                            OldVal::Extent(start_page, npages)
                        }
                        LeafVal::Inline(_) => OldVal::None,
                    }
                };
                free_old_val(store, old)?;
                remove_cell(store.page_mut(id)?, pos);
            }
            // A Stream small enough to stay INLINE has to be drained into a
            // buffer: the leaf cell must be contiguous, so there is nothing to
            // stream into and `pieces()` has nothing to give. At <=
            // MAX_INLINE_VAL there is also nothing to save — that is the same
            // reasoning that keeps small rows on the flat path in `insert_row`.
            // Without this a small streamed value silently wrote an EMPTY cell;
            // `streamed_payload_is_byte_identical_to_flat` caught it.
            let drained: Option<Vec<u8>> = match val {
                Payload::Stream { head, src } if head.len() + src.len() <= MAX_INLINE_VAL => {
                    let mut buf = head.to_vec();
                    let at = buf.len();
                    buf.resize(at + src.len(), 0);
                    if !src.is_empty() {
                        src.next_into(&mut buf[at..])?;
                    }
                    Some(buf)
                }
                _ => None,
            };
            let (overflow_page, cell) = match &drained {
                Some(b) => (0u64, make_leaf_cell(key, &Payload::Flat(b), 0)),
                // The extent payload was written (and, in durable modes,
                // synced) BEFORE this insert — the cell is only the 20-byte
                // reference and never spills.
                None if matches!(val, Payload::ExtentRef { .. }) => {
                    (0u64, make_leaf_cell(key, val, 0))
                }
                None => {
                    let op = if val.len() > MAX_INLINE_VAL {
                        write_overflow(store, val)?
                    } else {
                        0
                    };
                    (op, make_leaf_cell(key, val, op))
                }
            };
            let _ = overflow_page;
            if fits(store.page(id)?, cell.len())? {
                insert_cell(store.page_mut(id)?, pos, &cell)?;
                return Ok(Ins::Done {
                    id,
                    existed: existing,
                });
            }
            // split: gather cells + pending, rebuild two nodes
            let mut cells = all_cells(store.page(id)?)?;
            cells.insert(pos, cell);
            let (sep, right) = split_dirty(store, id, KIND_LEAF, cells, 0)?;
            Ok(Ins::Split {
                left: id,
                sep,
                right,
                existed: existing,
            })
        }
        KIND_BRANCH => {
            let idx = descent_index(store.page(page_id)?, key)?;
            let child = branch_child(store.page(page_id)?, idx)?;
            let res = insert_rec(store, child, key, val, mode)?;
            match res {
                Ins::Done { id, existed } => {
                    if id == child && !store.is_dirty(page_id) {
                        // child unchanged (InsertOnly hit): nothing to rewrite
                        return Ok(Ins::Done {
                            id: page_id,
                            existed,
                        });
                    }
                    let my_id = cow(store, page_id)?;
                    set_branch_child(store.page_mut(my_id)?, idx, id)?;
                    Ok(Ins::Done {
                        id: my_id,
                        existed,
                    })
                }
                Ins::Split {
                    left,
                    sep,
                    right,
                    existed,
                } => {
                    let my_id = cow(store, page_id)?;
                    set_branch_child(store.page_mut(my_id)?, idx, left)?;
                    let cell = make_branch_cell(&sep, right);
                    if fits(store.page(my_id)?, cell.len())? {
                        insert_cell(store.page_mut(my_id)?, idx, &cell)?;
                        return Ok(Ins::Done {
                            id: my_id,
                            existed,
                        });
                    }
                    let mut cells = all_cells(store.page(my_id)?)?;
                    cells.insert(idx, cell);
                    let leftmost = extra(store.page(my_id)?);
                    let (up_sep, up_right) =
                        split_dirty(store, my_id, KIND_BRANCH, cells, leftmost)?;
                    Ok(Ins::Split {
                        left: my_id,
                        sep: up_sep,
                        right: up_right,
                        existed,
                    })
                }
            }
        }
        _ => Err(corrupt("unexpected page kind in insert")),
    }
}

/// Insert `key` → `val`. `val` is a [`Payload`] so a large value need never be
/// concatenated on the way in (#42) — pass `Payload::Flat(bytes)` when it
/// already is.
pub fn insert<S: PageStore + ?Sized>(
    store: &mut S,
    root: u64,
    key: &[u8],
    val: &mut Payload<'_>,
    mode: InsertMode,
) -> Result<InsertOutcome> {
    if key.len() > MAX_KEY {
        return Err(Error::Unsupported(format!(
            "encoded key is {} bytes (max {MAX_KEY})",
            key.len()
        )));
    }
    if val.len() > u32::MAX as usize {
        return Err(Error::Unsupported("value larger than 4 GiB".into()));
    }
    if root == 0 {
        let id = store.alloc()?;
        init_node(store.page_mut(id)?, KIND_LEAF);
        let overflow_page = if val.len() > MAX_INLINE_VAL {
            write_overflow(store, val)?
        } else {
            0
        };
        let cell = make_leaf_cell(key, val, overflow_page);
        insert_cell(store.page_mut(id)?, 0, &cell)?;
        return Ok(InsertOutcome {
            new_root: id,
            existed: false,
        });
    }
    match insert_rec(store, root, key, val, mode)? {
        Ins::Done { id, existed } => Ok(InsertOutcome {
            new_root: id,
            existed,
        }),
        Ins::Split {
            left,
            sep,
            right,
            existed,
        } => {
            let new_root = store.alloc()?;
            let p = store.page_mut(new_root)?;
            init_node(p, KIND_BRANCH);
            set_extra(p, left);
            let cell = make_branch_cell(&sep, right);
            insert_cell(store.page_mut(new_root)?, 0, &cell)?;
            Ok(InsertOutcome {
                new_root,
                existed,
            })
        }
    }
}

struct Del {
    /// 0 = node became empty and was freed.
    new_id: u64,
    existed: bool,
}

fn delete_rec<S: PageStore + ?Sized>(store: &mut S, page_id: u64, key: &[u8]) -> Result<Del> {
    check_node(store.page(page_id)?)?;
    match kind(store.page(page_id)?) {
        KIND_LEAF => {
            let pos = match search(store.page(page_id)?, key)? {
                Ok(i) => i,
                Err(_) => {
                    return Ok(Del {
                        new_id: page_id,
                        existed: false,
                    })
                }
            };
            let id = cow(store, page_id)?;
            let old = {
                match leaf_cell(store.page(id)?, pos)?.1 {
                    LeafVal::Overflow { first_page, .. } => OldVal::Overflow(first_page),
                    LeafVal::Extent { start_page, npages, .. } => {
                        OldVal::Extent(start_page, npages)
                    }
                    LeafVal::Inline(_) => OldVal::None,
                }
            };
            free_old_val(store, old)?;
            remove_cell(store.page_mut(id)?, pos);
            if nkeys(store.page(id)?) == 0 {
                store.free(id)?;
                return Ok(Del {
                    new_id: 0,
                    existed: true,
                });
            }
            Ok(Del {
                new_id: id,
                existed: true,
            })
        }
        KIND_BRANCH => {
            let idx = descent_index(store.page(page_id)?, key)?;
            let child = branch_child(store.page(page_id)?, idx)?;
            let res = delete_rec(store, child, key)?;
            if !res.existed && res.new_id == child && !store.is_dirty(page_id) {
                return Ok(Del {
                    new_id: page_id,
                    existed: false,
                });
            }
            let my_id = cow(store, page_id)?;
            if res.new_id != 0 {
                set_branch_child(store.page_mut(my_id)?, idx, res.new_id)?;
                try_merge_leaves(store, my_id, idx)?;
            } else {
                // child vanished: drop its separator (or promote for leftmost)
                let n = nkeys(store.page(my_id)?);
                if idx == 0 {
                    if n == 0 {
                        store.free(my_id)?;
                        return Ok(Del {
                            new_id: 0,
                            existed: res.existed,
                        });
                    }
                    let new_leftmost = branch_cell(store.page(my_id)?, 0)?.1;
                    let p = store.page_mut(my_id)?;
                    set_extra(p, new_leftmost);
                    remove_cell(p, 0);
                } else {
                    remove_cell(store.page_mut(my_id)?, idx - 1);
                }
            }
            Ok(Del {
                new_id: my_id,
                existed: res.existed,
            })
        }
        _ => Err(corrupt("unexpected page kind in delete")),
    }
}

/// After a delete descended into child `idx` of dirty branch `branch_id`:
/// if that child is an under-filled leaf, merge it with an adjacent leaf
/// sibling when the combined cells fit in one page.
fn try_merge_leaves<S: PageStore + ?Sized>(
    store: &mut S,
    branch_id: u64,
    idx: usize,
) -> Result<()> {
    let n = nkeys(store.page(branch_id)?);
    let child = branch_child(store.page(branch_id)?, idx)?;
    check_node(store.page(child)?)?;
    if kind(store.page(child)?) != KIND_LEAF {
        return Ok(());
    }
    let child_used = used_cell_bytes(store.page(child)?)? + nkeys(store.page(child)?) * 2;
    if child_used >= (PAGE_SIZE - HDR) / 4 {
        return Ok(());
    }
    // pick a leaf sibling: prefer right, else left
    let (li, ri) = if idx < n {
        (idx, idx + 1)
    } else if idx > 0 {
        (idx - 1, idx)
    } else {
        return Ok(());
    };
    let left = branch_child(store.page(branch_id)?, li)?;
    let right = branch_child(store.page(branch_id)?, ri)?;
    check_node(store.page(left)?)?;
    check_node(store.page(right)?)?;
    if kind(store.page(left)?) != KIND_LEAF || kind(store.page(right)?) != KIND_LEAF {
        return Ok(());
    }
    let combined = used_cell_bytes(store.page(left)?)?
        + used_cell_bytes(store.page(right)?)?
        + (nkeys(store.page(left)?) + nkeys(store.page(right)?)) * 2;
    if combined > PAGE_SIZE - HDR {
        return Ok(());
    }
    // merge right into a COW of left, drop separator li (which points to right)
    let left = cow(store, left)?;
    let right_cells = all_cells(store.page(right)?)?;
    let base = nkeys(store.page(left)?);
    for (i, c) in right_cells.iter().enumerate() {
        if free_space(store.page(left)?) < c.len() + 2 {
            compact(store.page_mut(left)?)?;
        }
        insert_cell(store.page_mut(left)?, base + i, c)?;
    }
    store.free(right)?;
    let p = store.page_mut(branch_id)?;
    set_branch_child(p, li, left)?;
    remove_cell(p, li); // separator li sits between children li and ri
    Ok(())
}

pub struct DeleteOutcome {
    pub new_root: u64,
    pub existed: bool,
}

pub fn delete<S: PageStore + ?Sized>(
    store: &mut S,
    root: u64,
    key: &[u8],
) -> Result<DeleteOutcome> {
    if root == 0 {
        return Ok(DeleteOutcome {
            new_root: 0,
            existed: false,
        });
    }
    let res = delete_rec(store, root, key)?;
    let mut new_root = res.new_id;
    // collapse single-child chain at the root only
    while new_root != 0 {
        let p = store.page(new_root)?;
        if kind(p) == KIND_BRANCH && nkeys(p) == 0 {
            let only_child = extra(p);
            store.free(new_root)?;
            new_root = only_child;
        } else {
            break;
        }
    }
    Ok(DeleteOutcome {
        new_root,
        existed: res.existed,
    })
}

/// Collect every page id reachable from `root` (nodes + overflow chains)
/// into `out`. Used by integrity verification (page-accounting invariant).
/// Every extent referenced by leaves under `root` — the extent-side sibling
/// of [`collect_pages`], for the interval-accounting verifier and the model
/// tests' leak check (DESIGN-BLOBEXTENT §3.2/§6).
pub fn collect_extents<S: PageStore + ?Sized>(
    store: &S,
    root: u64,
    out: &mut Vec<(u64, u32)>,
) -> Result<()> {
    if root == 0 {
        return Ok(());
    }
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let p = store.page(id)?;
        check_node(p)?;
        match kind(p) {
            KIND_BRANCH => {
                stack.push(extra(p));
                for i in 0..nkeys(p) {
                    stack.push(branch_cell(p, i)?.1);
                }
            }
            KIND_LEAF => {
                for i in 0..nkeys(p) {
                    if let (_, LeafVal::Extent { start_page, npages, .. }) = leaf_cell(p, i)? {
                        out.push((start_page, npages));
                    }
                }
            }
            _ => return Err(corrupt("unknown node kind")),
        }
    }
    Ok(())
}

pub fn collect_pages<S: PageStore + ?Sized>(
    store: &S,
    root: u64,
    out: &mut std::collections::BTreeSet<u64>,
) -> Result<()> {
    if root == 0 {
        return Ok(());
    }
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !out.insert(id) {
            return Err(corrupt("page reachable twice (cycle or shared page)"));
        }
        let p = store.page(id)?;
        check_node(p)?;
        match kind(p) {
            KIND_BRANCH => {
                stack.push(extra(p));
                for i in 0..nkeys(p) {
                    stack.push(branch_cell(p, i)?.1);
                }
            }
            KIND_LEAF => {
                for i in 0..nkeys(p) {
                    if let (_, LeafVal::Overflow { first_page, total_len }) = leaf_cell(p, i)? {
                        let mut oid = first_page;
                        let mut hops = 0usize;
                        while oid != 0 {
                            if hops > (total_len as usize / OVERFLOW_CAP) + 2 {
                                return Err(corrupt("overflow chain too long"));
                            }
                            hops += 1;
                            if !out.insert(oid) {
                                return Err(corrupt("overflow page reachable twice"));
                            }
                            oid = extra(store.page(oid)?);
                        }
                    }
                }
            }
            _ => return Err(corrupt("unexpected page kind in collect")),
        }
    }
    Ok(())
}

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
                    let key = key.to_vec();
                    let val = read_leaf_val(store, val)?;
                    self.leaf = Some((leaf_id, pos + 1));
                    return Ok(Some((key, val)));
                }
                self.leaf = None;
            }
            // climb until a branch has an unvisited child, then descend
            loop {
                match self.stack.pop() {
                    None => {
                        self.done = true;
                        return Ok(None);
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
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pagestore::test_store::TestStore;
    use std::collections::BTreeMap;

    /// Extents through the tree, differentially against a model — with the
    /// OWNERSHIP ledger as the real assertion: after every simulated commit,
    /// the arena's live extents must be exactly the refs reachable from the
    /// root. A replace or delete that forgets `free_extent` leaks; one that
    /// frees twice trips the arena's double-free guard; a read of a freed
    /// extent trips the dead-extent guard. (DESIGN-BLOBEXTENT §13.2.)
    #[test]
    fn extent_refs_roundtrip_and_never_leak() {
        let mut store = TestStore::new();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut root = 0u64;
        let mut rng = Rng(0x0B10_BE47);
        for round in 0..2000u32 {
            let k = format!("k{:03}", rng.next() % 200).into_bytes();
            match rng.next() % 4 {
                // 0..=1: upsert an extent-backed value
                0 | 1 => {
                    let len = 1 + (rng.next() as usize % (3 * PAGE_SIZE));
                    let val: Vec<u8> = (0..len).map(|i| (rng.next() ^ i as u64) as u8).collect();
                    // payload BEFORE reference — the arena models the order
                    let (start_page, total_len, npages) = store.put_extent(&val);
                    let out = insert(
                        &mut store,
                        root,
                        &k,
                        &mut Payload::ExtentRef { start_page, total_len, npages },
                        InsertMode::Upsert,
                    )
                    .unwrap();
                    root = out.new_root;
                    model.insert(k, val);
                }
                // 2: upsert a small inline value (kind churn: extent→inline
                // replace must free the extent)
                2 => {
                    let val: Vec<u8> = (0..(rng.next() as usize % 64))
                        .map(|_| rng.next() as u8)
                        .collect();
                    let out = insert(
                        &mut store,
                        root,
                        &k,
                        &mut Payload::Flat(&val),
                        InsertMode::Upsert,
                    )
                    .unwrap();
                    root = out.new_root;
                    model.insert(k, val);
                }
                // 3: delete
                _ => {
                    let out = delete(&mut store, root, &k).unwrap();
                    root = out.new_root;
                    model.remove(&k);
                }
            }
            // read-back parity on a sample key
            let probe = format!("k{:03}", rng.next() % 200).into_bytes();
            assert_eq!(
                get(&store, root, &probe).unwrap(),
                model.get(&probe).cloned(),
                "round {round}"
            );
            if round % 97 == 0 {
                store.commit();
                let mut refs = Vec::new();
                collect_extents(&store, root, &mut refs).unwrap();
                let mut reachable: Vec<u64> = refs.iter().map(|(s, _)| *s).collect();
                reachable.sort_unstable();
                assert_eq!(
                    store.live_extents(),
                    reachable,
                    "extent ledger out of balance at round {round}"
                );
            }
        }
        // full drain: every extent must come back
        store.commit();
        for (k, v) in &model {
            assert_eq!(get(&store, root, k).unwrap().as_deref(), Some(v.as_slice()));
        }
        let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
        for k in keys {
            root = delete(&mut store, root, &k).unwrap().new_root;
        }
        store.commit();
        assert!(store.live_extents().is_empty(), "drained tree leaked extents");
        assert_eq!(root, 0);
    }

    /// The vkind=2 decode is strict: truncation at every offset and a
    /// disagreeing npages must be Corrupt, never a panic or a wild read.
    #[test]
    fn extent_cell_decode_is_strict() {
        let mut store = TestStore::new();
        let (start_page, total_len, npages) = store.put_extent(&[7u8; 5000]);
        let out = insert(
            &mut store,
            0,
            b"k",
            &mut Payload::ExtentRef { start_page, total_len, npages },
            InsertMode::Upsert,
        )
        .unwrap();
        let root = out.new_root;
        // Corrupt the npages field in place: find the cell's trailing 4 bytes.
        let page: Vec<u8> = store.page(root).unwrap().to_vec();
        // (cells live at the tail; the ref is the last EXTENT_REF_LEN bytes of
        // the only cell — locate by the known total_len bytes)
        let needle = total_len.to_le_bytes();
        let at = page
            .windows(8)
            .rposition(|w| w == needle)
            .expect("total_len bytes present");
        let dirty = {
            // TestStore page_mut requires dirty — re-insert to keep it dirty,
            // then poke the byte through the same page id.
            store.page_mut(root).unwrap()
        };
        dirty[at + 8..at + 12].copy_from_slice(&(npages + 1).to_le_bytes());
        let err = get(&store, root, b"k").unwrap_err();
        assert!(format!("{err}").contains("mismatch"), "{err}");
    }

    /// A streamed value must be byte-for-byte the same as one handed over whole.
    ///
    /// The safety claim of #43. Chunked at every awkward size — a source's chunk
    /// boundary, the page boundary, and the row head's end are three unrelated
    /// things, and the head is what makes the first page's split uneven.
    ///
    /// Mutation-tested: dropping the head into the fill loop (writing it after
    /// the source's bytes instead of before) fails this, and so does letting the
    /// stream reader hand back a short buffer.
    #[test]
    fn streamed_payload_is_byte_identical_to_flat() {
        struct Chunked {
            data: Vec<u8>,
            pos: usize,
        }
        impl BlobSource for Chunked {
            fn len(&self) -> usize {
                self.data.len()
            }
            fn next_into(&mut self, buf: &mut [u8]) -> Result<()> {
                buf.copy_from_slice(&self.data[self.pos..self.pos + buf.len()]);
                self.pos += buf.len();
                Ok(())
            }
        }
        let mut rng = Rng(0x5175_2A17);
        for &(head_len, body_len) in &[
            (0usize, OVERFLOW_CAP * 2 + 5usize),
            (1, OVERFLOW_CAP * 2),
            (17, OVERFLOW_CAP - 17), // head + body lands exactly on a page
            (OVERFLOW_CAP - 1, OVERFLOW_CAP + 1),
            (OVERFLOW_CAP, OVERFLOW_CAP * 2),
            (9, 1),
        ] {
            let head: Vec<u8> = (0..head_len).map(|_| rng.next() as u8).collect();
            let body: Vec<u8> = (0..body_len).map(|_| rng.next() as u8).collect();
            let whole: Vec<u8> = head.iter().chain(body.iter()).copied().collect();

            let mut store = TestStore::new();
            let r = insert(&mut store, 0, b"flat", &mut Payload::Flat(&whole), InsertMode::InsertOnly)
                .unwrap()
                .new_root;
            let mut src = Chunked { data: body.clone(), pos: 0 };
            let r = insert(
                &mut store,
                r,
                b"stream",
                &mut Payload::Stream { head: &head, src: &mut src },
                InsertMode::InsertOnly,
            )
            .unwrap()
            .new_root;
            let a = get(&store, r, b"flat").unwrap().unwrap();
            let b = get(&store, r, b"stream").unwrap().unwrap();
            assert_eq!(a, whole, "head={head_len} body={body_len}: flat");
            assert_eq!(b, whole, "head={head_len} body={body_len}: streamed");
            assert_eq!(src.pos, body.len(), "head={head_len}: source not drained");
        }
    }

    /// `Payload::Parts` must be byte-for-byte `Payload::Flat` of the same bytes.
    ///
    /// This is the whole safety claim of #42: the parts form exists so a large
    /// value never has to be concatenated, and it is only sound if the pages it
    /// writes are indistinguishable. Split at every awkward place — a piece
    /// boundary and a page boundary have nothing to do with each other, so the
    /// interesting splits are the ones that land mid-page, mid-header, and on
    /// exactly `OVERFLOW_CAP`.
    ///
    /// Mutation-tested: making `PayloadCursor::next_chunk` return the whole
    /// remaining piece (ignoring `want`) fails this, and so does dropping the
    /// `while filled < take` loop in `write_overflow`.
    #[test]
    fn parts_payload_is_byte_identical_to_flat() {
        let mut rng = Rng(0x2A17_BEEF);
        let big: Vec<u8> = (0..(OVERFLOW_CAP * 3 + 777)).map(|_| rng.next() as u8).collect();
        // splits that land: mid-first-page, exactly on a page edge, one before,
        // one after, in the tail, and degenerate (empty pieces at both ends)
        let splits: &[&[usize]] = &[
            &[1],
            &[OVERFLOW_CAP - 1],
            &[OVERFLOW_CAP],
            &[OVERFLOW_CAP + 1],
            &[OVERFLOW_CAP * 3],
            &[0, big.len()],
            &[7, OVERFLOW_CAP, OVERFLOW_CAP * 2 + 3, big.len() - 1],
        ];
        for (case, cuts) in splits.iter().enumerate() {
            let mut pieces: Vec<&[u8]> = Vec::new();
            let mut at = 0usize;
            for &c in cuts.iter() {
                pieces.push(&big[at..c]);
                at = c;
            }
            pieces.push(&big[at..]);

            let mut store = TestStore::new();
            let flat = insert(&mut store, 0, b"flat", &mut Payload::Flat(&big), InsertMode::InsertOnly)
                .unwrap()
                .new_root;
            let both = insert(
                &mut store,
                flat,
                b"parts",
                &mut Payload::Parts(&pieces),
                InsertMode::InsertOnly,
            )
            .unwrap()
            .new_root;
            let a = get(&store, both, b"flat").unwrap().unwrap();
            let b = get(&store, both, b"parts").unwrap().unwrap();
            assert_eq!(a, big, "case {case}: flat round-trip");
            assert_eq!(b, big, "case {case}: parts round-trip, cuts {cuts:?}");
            assert_eq!(a, b, "case {case}: parts != flat");
        }
    }

    /// The same, for a value small enough to stay INLINE — a different code path
    /// in `make_leaf_cell`, and one it would be easy to leave joining only the
    /// first piece.
    #[test]
    fn parts_payload_is_byte_identical_to_flat_when_inline() {
        let small: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        assert!(small.len() <= MAX_INLINE_VAL);
        let pieces: Vec<&[u8]> = vec![&small[..0], &small[..17], &small[17..298], &small[298..]];
        let mut store = TestStore::new();
        let r = insert(&mut store, 0, b"f", &mut Payload::Flat(&small), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        let r = insert(&mut store, r, b"p", &mut Payload::Parts(&pieces), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        assert_eq!(get(&store, r, b"f").unwrap().unwrap(), small);
        assert_eq!(get(&store, r, b"p").unwrap().unwrap(), small);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn key_of(n: u64) -> Vec<u8> {
        format!("key{n:08}").into_bytes()
    }

    fn val_of(rng: &mut Rng) -> Vec<u8> {
        // mix of small inline values and large overflow values
        let len = match rng.next() % 20 {
            0 => 3000 + (rng.next() % 8000) as usize, // overflow, multi-page
            1..=3 => 1024 + (rng.next() % 512) as usize, // around the threshold
            _ => (rng.next() % 64) as usize,
        };
        (0..len).map(|i| (i as u8).wrapping_mul(31)).collect()
    }

    #[test]
    fn model_test_against_btreemap() {
        let mut store = TestStore::new();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut root = 0u64;
        let mut rng = Rng(42);

        for step in 0..6000 {
            let k = key_of(rng.next() % 700);
            match rng.next() % 10 {
                0..=5 => {
                    let v = val_of(&mut rng);
                    let out = insert(&mut store, root, &k, &mut Payload::Flat(&v), InsertMode::Upsert).unwrap();
                    root = out.new_root;
                    let prev = model.insert(k.clone(), v);
                    assert_eq!(out.existed, prev.is_some(), "step {step}");
                }
                6..=8 => {
                    let out = delete(&mut store, root, &k).unwrap();
                    root = out.new_root;
                    let prev = model.remove(&k);
                    assert_eq!(out.existed, prev.is_some(), "step {step}");
                }
                _ => {
                    let got = get(&store, root, &k).unwrap();
                    assert_eq!(got, model.get(&k).cloned(), "step {step}");
                }
            }
            if step % 500 == 0 {
                // full scan must equal the model exactly
                let mut c = cursor(&store, root, None, None).unwrap();
                let mut items = Vec::new();
                while let Some(kv) = c.next(&store).unwrap() {
                    items.push(kv);
                }
                let expect: Vec<_> =
                    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(items, expect, "scan mismatch at step {step}");
                store.commit();
            }
        }
    }

    #[test]
    fn insert_only_reports_duplicates_without_mutation() {
        let mut store = TestStore::new();
        let mut root = 0;
        root = insert(&mut store, root, b"a", &mut Payload::Flat(b"1"), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        store.commit();
        let live_before = store.live_pages();
        let out = insert(&mut store, root, b"a", &mut Payload::Flat(b"2"), InsertMode::InsertOnly).unwrap();
        assert!(out.existed);
        assert_eq!(out.new_root, root);
        assert_eq!(store.live_pages(), live_before, "no pages may be touched");
        assert_eq!(get(&store, root, b"a").unwrap().unwrap(), b"1");
    }

    #[test]
    fn range_scans_with_bounds() {
        let mut store = TestStore::new();
        let mut root = 0;
        for i in 0..500u64 {
            root = insert(
                &mut store,
                root,
                &key_of(i),
                &mut Payload::Flat(&i.to_le_bytes()),
                InsertMode::InsertOnly,
            )
            .unwrap()
            .new_root;
        }
        let collect = |lo: Option<(&[u8], bool)>, hi: Option<(&[u8], bool)>| {
            let mut c = cursor(&store, root, lo, hi).unwrap();
            let mut out = Vec::new();
            while let Some((k, _)) = c.next(&store).unwrap() {
                out.push(k);
            }
            out
        };
        let lo = key_of(100);
        let hi = key_of(200);
        assert_eq!(collect(Some((&lo, true)), Some((&hi, false))).len(), 100);
        assert_eq!(collect(Some((&lo, false)), Some((&hi, true))).len(), 100);
        assert_eq!(collect(None, Some((&key_of(10), false))).len(), 10);
        assert_eq!(collect(Some((&key_of(490), true)), None).len(), 10);
        // bounds that match nothing
        assert_eq!(
            collect(Some((b"zzz".as_slice(), true)), None).len(),
            0
        );
    }

    #[test]
    fn delete_everything_frees_every_page() {
        let mut store = TestStore::new();
        let mut root = 0;
        let mut rng = Rng(7);
        let n = 2000u64;
        for i in 0..n {
            let v = val_of(&mut rng);
            root = insert(&mut store, root, &key_of(i), &mut Payload::Flat(&v), InsertMode::Upsert)
                .unwrap()
                .new_root;
        }
        store.commit();
        assert!(store.live_pages() > 20);
        // delete in a shuffled-ish order
        for i in 0..n {
            let k = key_of((i * 617) % n);
            root = delete(&mut store, root, &k).unwrap().new_root;
        }
        store.commit();
        assert_eq!(root, 0);
        assert_eq!(
            store.live_pages(),
            0,
            "all pages must be freed when the tree is empty (no leaks)"
        );
    }

    #[test]
    fn overflow_values_roundtrip_and_free() {
        let mut store = TestStore::new();
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let mut root = insert(&mut store, 0, b"big", &mut Payload::Flat(&big), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        assert_eq!(get(&store, root, b"big").unwrap().unwrap(), big);
        store.commit();
        // replace with a small value: chain must be freed
        root = insert(&mut store, root, b"big", &mut Payload::Flat(b"small"), InsertMode::Upsert)
            .unwrap()
            .new_root;
        store.commit();
        assert_eq!(store.live_pages(), 1, "overflow chain must be reclaimed");
        assert_eq!(get(&store, root, b"big").unwrap().unwrap(), b"small");
        root = delete(&mut store, root, b"big").unwrap().new_root;
        store.commit();
        assert_eq!(root, 0);
        assert_eq!(store.live_pages(), 0);
    }

    #[test]
    fn oversized_keys_rejected() {
        let mut store = TestStore::new();
        let k = vec![7u8; MAX_KEY + 1];
        assert!(insert(&mut store, 0, &k, &mut Payload::Flat(b""), InsertMode::Upsert).is_err());
    }

    #[test]
    fn cow_discipline_upheld_across_commits() {
        // after commit, any further mutation must not touch committed pages
        let mut store = TestStore::new();
        let mut root = 0;
        for i in 0..300u64 {
            root = insert(
                &mut store,
                root,
                &key_of(i),
                &mut Payload::Flat(b"v"),
                InsertMode::InsertOnly,
            )
            .unwrap()
            .new_root;
        }
        store.commit(); // clears dirty set: everything is now "committed"
        // TestStore::page_mut errors on non-dirty pages, so any COW violation
        // inside insert/delete would fail these calls
        for i in 0..300u64 {
            root = insert(&mut store, root, &key_of(i), &mut Payload::Flat(b"w"), InsertMode::Upsert)
                .unwrap()
                .new_root;
        }
        for i in 150..300u64 {
            root = delete(&mut store, root, &key_of(i)).unwrap().new_root;
        }
        for i in 0..150u64 {
            assert_eq!(get(&store, root, &key_of(i)).unwrap().unwrap(), b"w");
        }
    }
}
