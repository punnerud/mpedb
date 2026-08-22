//! Minimal sqlite-format WRITER — the reader's mirror (plan §11). The C-API
//! shim's `sqlite3_backup` geometry and `sqlite3_serialize` hand out REAL
//! sqlite images of logical content through this, not a private dump format.
//!
//! v1 scope is deliberately narrow, and everything outside it is refused BY
//! NAME ([`Error::Unsupported`]) — never an amputated image:
//!
//! - 4096-byte pages only.
//! - Plain rowid tables whose CREATE text carries no `PRIMARY KEY`/`UNIQUE`
//!   clause — stock backs those with auto-index trees this writer does not
//!   emit, so writing the table without them would be a lying image. The
//!   `INTEGER PRIMARY KEY` rowid alias needs NO extra tree and is the
//!   exception to support later; v1 refuses it too, by name.
//! - Record payloads ≤ 4061 bytes — the table-leaf X threshold at usable
//!   4096 (U − 35), i.e. no overflow chains.
//! - Trees span as many pages as they need: leaves are packed full in one
//!   pass and interior levels are packed over them, bottom-up. (v1 wrote a
//!   single leaf per tree and refused anything larger, which capped a table
//!   at one page — about 60 rows of the shape a real table has.)
//!
//! Every header constant is the byte-exact shape stock 3.45.1 writes,
//! verified with xxd: payload fractions 64/32/32 (stock REFUSES other
//! values), schema format 4, and change counter == version-valid-for @92 —
//! the equality the reader's own `root_bound` rule keys on, so an image we
//! write is one our reader trusts the @28 size of.

use crate::{Error, Result, Value};

const PAGE: usize = 4096;
/// Table-leaf X at usable 4096 / 0 reserved: U − 35. One byte more and stock
/// spills to an overflow page, which v1 does not write.
const MAX_PAYLOAD: usize = PAGE - 35;
/// What stock 3.45.1 stamps at @96; the recipe's reference build.
const SQLITE_VERSION_NUMBER: u32 = 3_045_001;

/// One table for the image: the CREATE-TABLE text exactly as the catalog
/// reports it, and the rows in insertion order (rowid = 1..N).
pub struct ImageTable {
    pub name: String,
    pub sql: String,
    pub rows: Vec<Vec<Value>>,
    /// Indexes over this table, written as their own b-trees. Empty is the
    /// common case and costs nothing.
    pub indexes: Vec<ImageIndex>,
}

/// One index to write alongside its table.
///
/// `columns` are ordinals into the table's row values, in key order — the
/// caller resolves names, because it is the one that knows the table's column
/// order. `sql` is the `CREATE INDEX` text that goes into `sqlite_master`; it
/// is also sniffed, since it is what a reader will believe about the index and
/// must not promise an ordering this writer did not produce.
#[derive(Clone, Debug)]
pub struct ImageIndex {
    pub name: String,
    pub sql: String,
    pub columns: Vec<usize>,
}

/// Serialize `tables` as a complete sqlite database image: page 1 =
/// `sqlite_master` (one row per table, rootpage 2, 3, … in table order),
/// then one leaf page per table. Anything the v1 scope cannot represent
/// faithfully is [`Error::Unsupported`] with the offender named.
pub fn write_image(tables: &[ImageTable], page_size: usize) -> Result<Vec<u8>> {
    if page_size != PAGE {
        return Err(Error::Unsupported(format!(
            "page size {page_size} (v1 writes 4096-byte pages only)"
        )));
    }
    for t in tables {
        refuse_indexed_ddl(t)?;
        for ix in &t.indexes {
            refuse_unsupported_index(ix)?;
        }
    }

    // Page 1 is reserved before anything else: sqlite_master's root MUST be
    // page 1, but its rows carry the data roots, which are only known once the
    // data trees are built. So the page is claimed now and filled last.
    let mut pager = Pager::new();

    let mut roots = Vec::with_capacity(tables.len());
    for t in tables {
        let mut cells = Vec::with_capacity(t.rows.len());
        for (j, row) in t.rows.iter().enumerate() {
            let rowid = j as i64 + 1;
            if row.is_empty() {
                // A zero-value record's cell is 3 bytes, under sqlite's
                // 4-byte cell minimum — representable only with padding the
                // free-space accounting would have to explain. No SQL INSERT
                // produces one; refuse rather than approximate.
                return Err(Error::Unsupported(format!(
                    "table `{}` row {rowid}: empty record (v1 writes at least one value per row)",
                    t.name
                )));
            }
            let record = encode_record(row);
            if record.len() > MAX_PAYLOAD {
                return Err(Error::Unsupported(format!(
                    "table `{}` row {rowid}: record payload {} bytes exceeds {MAX_PAYLOAD} \
                     (v1 writes no overflow chains)",
                    t.name,
                    record.len()
                )));
            }
            cells.push((rowid, leaf_cell(rowid, &record)));
        }
        roots.push(build_table_tree(&mut pager, &cells, None, &t.name)?);
    }

    // Index trees after the table trees, so a rootpage is never referenced
    // before it exists. Each carries (table position, index, root) so the
    // master rows below can be emitted in sqlite's order: a table followed by
    // its own indexes.
    let mut index_roots: Vec<(usize, &ImageIndex, u32)> = Vec::new();
    for (ti, t) in tables.iter().enumerate() {
        for ix in &t.indexes {
            let root = build_index_tree(&mut pager, &t.rows, ix)?;
            index_roots.push((ti, ix, root));
        }
    }

    // sqlite_master on page 1: (type, name, tbl_name, rootpage, sql) per
    // table — the same record codec as the data, nothing special-cased.
    let mut master: Vec<[Value; 5]> = Vec::with_capacity(tables.len() + index_roots.len());
    for (i, t) in tables.iter().enumerate() {
        master.push([
            Value::Text("table".into()),
            Value::Text(t.name.clone()),
            Value::Text(t.name.clone()),
            Value::Int(roots[i] as i64),
            Value::Text(t.sql.clone()),
        ]);
        for (ti, ix, root) in &index_roots {
            if *ti == i {
                master.push([
                    Value::Text("index".into()),
                    Value::Text(ix.name.clone()),
                    Value::Text(t.name.clone()), // tbl_name: the table it indexes
                    Value::Int(*root as i64),
                    Value::Text(ix.sql.clone()),
                ]);
            }
        }
    }
    let mut cells = Vec::with_capacity(master.len());
    for (i, row) in master.iter().enumerate() {
        let record = encode_record(row);
        if record.len() > MAX_PAYLOAD {
            let what = match &row[1] {
                Value::Text(n) => n.clone(),
                _ => String::new(),
            };
            return Err(Error::Unsupported(format!(
                "sqlite_master row for `{what}`: record payload {} bytes exceeds {MAX_PAYLOAD} \
                 (v1 writes no overflow chains)",
                record.len()
            )));
        }
        cells.push((i as i64 + 1, leaf_cell(i as i64 + 1, &record)));
    }
    build_table_tree(&mut pager, &cells, Some(1), "sqlite_master")?;

    let n_pages = pager.count();
    let mut img = pager.finish();
    write_header(&mut img[..100], n_pages);
    Ok(img)
}

/// sqlite's storage-class rank, which orders values before any comparison
/// within a class: NULL < numbers < text < blob. INTEGER and REAL share one
/// rank and are compared numerically across the boundary.
fn class_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Int(_) | Value::Float(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    }
}

/// An integer against a float, EXACTLY.
///
/// Casting the i64 to f64 and comparing would be wrong for magnitudes past
/// 2^53, where the cast rounds: two different integers can land on the same
/// float and compare equal, which in an index means two keys sorted as
/// equivalent that sqlite considers ordered. Comparing against the float's
/// floor keeps it exact.
fn cmp_int_float(i: i64, f: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if f.is_nan() {
        // sqlite stores NaN as NULL, so this should not arrive. Order it after
        // every real number rather than returning an inconsistent comparison,
        // which would break the sort itself.
        return Ordering::Less;
    }
    if f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less; // f is above i64::MAX
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let floor = f.floor();
    match i.cmp(&(floor as i64)) {
        Ordering::Equal if f > floor => Ordering::Less, // equal whole part, f has a fraction
        other => other,
    }
}

/// Compare two index key values the way sqlite does with BINARY collation and
/// ASC order — storage class first, then within the class.
///
/// This is the load-bearing function of the whole index path. An index whose
/// entries are not in exactly the order sqlite expects is worse than no index:
/// sqlite trusts the order and binary-searches it, so a wrong one produces
/// missing rows rather than an error. `PRAGMA integrity_check` walks index
/// order and is the test that keeps this honest.
fn cmp_key_value(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ra, rb) = (class_rank(a), class_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => cmp_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => cmp_int_float(*y, *x).reverse(),
        // BINARY collation is a byte compare of the UTF-8, not a locale one.
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.as_slice().cmp(y.as_slice()),
        _ => Ordering::Equal, // unreachable: ranks already matched
    }
}

/// The DDL gate for an index, mirroring [`refuse_indexed_ddl`] for tables.
///
/// Everything here changes the ORDER of the entries, and this writer produces
/// exactly one order: every column ascending under BINARY. An index declared
/// otherwise would be written sorted one way and read as if sorted another —
/// a silent wrong answer, so it is refused by name instead.
fn refuse_unsupported_index(ix: &ImageIndex) -> Result<()> {
    let words = ddl_words(&ix.sql);
    for (word, why) in [
        ("DESC", "descending key order"),
        ("COLLATE", "a collation other than BINARY"),
        ("WHERE", "a partial-index predicate"),
    ] {
        if words.iter().any(|w| w == word) {
            return Err(Error::Unsupported(format!(
                "index `{}`: {why} (v1 writes ascending BINARY keys over whole tables only)",
                ix.name
            )));
        }
    }
    if ix.columns.is_empty() {
        return Err(Error::Unsupported(format!(
            "index `{}`: no key columns (an expression index has none this writer can read)",
            ix.name
        )));
    }
    Ok(())
}

/// Build an index b-tree over `rows` and return its root page.
///
/// An index entry is its key columns followed by the rowid, and the rowid also
/// breaks ties — so no two entries are ever equal and the order is total.
///
/// The tree shape differs from a table's in the way that matters: a table's
/// interior cells are pure separators whose keys ALSO live in the leaves,
/// while an index's interior cells are entries in their own right, LIFTED out
/// of the leaf sequence. Leaving a lifted entry in its leaf as well would
/// duplicate a row in every scan; dropping one without lifting it would lose
/// it. That is why the packer takes one entry as a separator each time it
/// closes a leaf.
fn build_index_tree(
    pager: &mut Pager,
    rows: &[Vec<Value>],
    ix: &ImageIndex,
) -> Result<u32> {
    let mut entries: Vec<(Vec<Value>, i64)> = Vec::with_capacity(rows.len());
    for (j, row) in rows.iter().enumerate() {
        let mut key = Vec::with_capacity(ix.columns.len());
        for &c in &ix.columns {
            let Some(v) = row.get(c) else {
                return Err(Error::Unsupported(format!(
                    "index `{}`: key column {c} is past the end of the row",
                    ix.name
                )));
            };
            key.push(v.clone());
        }
        entries.push((key, j as i64 + 1));
    }
    entries.sort_by(|a, b| {
        for (x, y) in a.0.iter().zip(b.0.iter()) {
            let o = cmp_key_value(x, y);
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.1.cmp(&b.1) // equal keys: the rowid orders them, as sqlite does
    });

    // The payload is the key columns with the rowid appended — the record
    // shape sqlite reads back as (key…, rowid).
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    for (key, rowid) in entries {
        let mut vals = key;
        vals.push(Value::Int(rowid));
        let rec = encode_record(&vals);
        if rec.len() > MAX_PAYLOAD {
            return Err(Error::Unsupported(format!(
                "index `{}`: entry payload {} bytes exceeds {MAX_PAYLOAD} \
                 (v1 writes no overflow chains)",
                ix.name,
                rec.len()
            )));
        }
        payloads.push(rec);
    }

    if payloads.is_empty() {
        let page = pager.alloc();
        build_index_leaf(pager.page_mut(page), &[])?;
        return Ok(page);
    }

    // Level 0: leaves, with one entry lifted between each pair as a separator.
    let mut children: Vec<u32> = Vec::new();
    let mut seps: Vec<Vec<u8>> = Vec::new();
    let mut i = 0usize;
    while i < payloads.len() {
        let mut batch: Vec<&[u8]> = Vec::new();
        let mut used = 0usize;
        while i < payloads.len() {
            let cell = varint_len(payloads[i].len() as u64) + payloads[i].len();
            if !batch.is_empty() && 8 + 2 * (batch.len() + 1) + used + cell > PAGE {
                break;
            }
            used += cell;
            batch.push(&payloads[i]);
            i += 1;
        }
        let page = pager.alloc();
        build_index_leaf(pager.page_mut(page), &batch)?;
        children.push(page);
        if i < payloads.len() {
            seps.push(payloads[i].clone());
            i += 1;
        }
    }

    // Interior levels: children[k] and children[k+1] are separated by seps[k].
    // A group of children packed onto one page consumes the separators BETWEEN
    // them as cells; the one at the group's edge is lifted to the level above.
    loop {
        if children.len() == 1 {
            return Ok(children[0]);
        }
        let mut up_children: Vec<u32> = Vec::new();
        let mut up_seps: Vec<Vec<u8>> = Vec::new();
        let mut j = 0usize;
        while j < children.len() {
            let start = j;
            let mut used = 0usize;
            let mut n_cells = 0usize;
            j += 1; // the first child of a group costs no cell
            while j < children.len() {
                let sep = &seps[j - 1];
                let cell = 4 + varint_len(sep.len() as u64) + sep.len();
                if 12 + 2 * (n_cells + 1) + used + cell > PAGE {
                    break;
                }
                used += cell;
                n_cells += 1;
                j += 1;
            }
            let page = pager.alloc();
            build_index_interior(
                pager.page_mut(page),
                &children[start..j],
                &seps[start..j - 1],
            )?;
            up_children.push(page);
            if j < children.len() {
                // The separator at the boundary belongs to the level above; the
                // next group starts at children[j], which is NOT consumed here.
                up_seps.push(seps[j - 1].clone());
            }
        }
        children = up_children;
        seps = up_seps;
    }
}

/// Lay `cells` (payloads) on one index-leaf page: 8-byte header, pointers
/// forward, content packed backward. An index-leaf cell is
/// `varint(payload length) ‖ payload` — no rowid field, because the rowid is
/// inside the payload as its last column.
fn build_index_leaf(page: &mut [u8], cells: &[&[u8]]) -> Result<()> {
    let ptrs = 8;
    let total: usize = cells.iter().map(|c| varint_len(c.len() as u64) + c.len()).sum();
    if ptrs + 2 * cells.len() + total > PAGE {
        return Err(Error::Unsupported(format!(
            "index leaf overflow with {} cells — the level packer should have split this",
            cells.len()
        )));
    }
    page[0] = 0x0a; // index leaf
    page[3..5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let mut content = PAGE;
    for (i, c) in cells.iter().enumerate() {
        let mut cell = Vec::with_capacity(c.len() + 3);
        put_varint(&mut cell, c.len() as u64);
        cell.extend_from_slice(c);
        content -= cell.len();
        page[content..content + cell.len()].copy_from_slice(&cell);
        page[ptrs + 2 * i..ptrs + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    page[5..7].copy_from_slice(&(content as u16).to_be_bytes());
    Ok(())
}

/// Lay an index level on one index-interior page. `children` has exactly one
/// more entry than `seps`: the last child is the right-most pointer, and
/// `seps[i]` is the entry that sits between `children[i]` and `children[i+1]`.
fn build_index_interior(page: &mut [u8], children: &[u32], seps: &[Vec<u8>]) -> Result<()> {
    debug_assert_eq!(children.len(), seps.len() + 1);
    let ptrs = 12;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(seps.len());
    for (i, sep) in seps.iter().enumerate() {
        let mut cell = Vec::with_capacity(sep.len() + 7);
        cell.extend_from_slice(&children[i].to_be_bytes());
        put_varint(&mut cell, sep.len() as u64);
        cell.extend_from_slice(sep);
        bodies.push(cell);
    }
    let total: usize = bodies.iter().map(Vec::len).sum();
    if ptrs + 2 * bodies.len() + total > PAGE {
        return Err(Error::Unsupported(format!(
            "index interior overflow with {} children — the level packer should have split this",
            children.len()
        )));
    }
    page[0] = 0x02; // index interior
    page[3..5].copy_from_slice(&(bodies.len() as u16).to_be_bytes());
    page[8..12].copy_from_slice(&children[children.len() - 1].to_be_bytes());
    let mut content = PAGE;
    for (i, c) in bodies.iter().enumerate() {
        content -= c.len();
        page[content..content + c.len()].copy_from_slice(c);
        page[ptrs + 2 * i..ptrs + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    page[5..7].copy_from_slice(&(content as u16).to_be_bytes());
    Ok(())
}

/// The image under construction, addressed by sqlite's 1-based page numbers.
///
/// Pages are handed out in the order trees ask for them, which is not the
/// order sqlite would choose — it does not have to be. Nothing in the format
/// requires a tree's pages to be contiguous or ascending; the parent's child
/// pointers are the only structure that matters.
struct Pager {
    buf: Vec<u8>,
}

impl Pager {
    /// Starts with page 1 claimed and blank — sqlite_master's root has to be
    /// page 1, and it is written only after the data trees have taken theirs.
    fn new() -> Self {
        Pager { buf: vec![0u8; PAGE] }
    }

    fn alloc(&mut self) -> u32 {
        self.buf.resize(self.buf.len() + PAGE, 0);
        (self.buf.len() / PAGE) as u32
    }

    fn page_mut(&mut self, n: u32) -> &mut [u8] {
        let off = (n as usize - 1) * PAGE;
        &mut self.buf[off..off + PAGE]
    }

    fn count(&self) -> u32 {
        (self.buf.len() / PAGE) as u32
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Bulk-load `cells` (rowid-ordered) into a table b-tree and return its root.
///
/// Bottom-up, which is what makes it a bulk load rather than N inserts: leaves
/// are packed full in one pass, then each level of interior pages is packed
/// over the level below until one page holds the lot. No page is ever split
/// and none is rebalanced, because the input is already in key order and
/// nothing is inserted after the fact.
///
/// `root` pins the root to an already-allocated page (page 1, for
/// sqlite_master). Everything else takes fresh pages.
fn build_table_tree(
    pager: &mut Pager,
    cells: &[(i64, Vec<u8>)],
    root: Option<u32>,
    tree: &str,
) -> Result<u32> {
    // An empty table is a single empty leaf — the shape stock leaves after
    // CREATE TABLE with no rows.
    if cells.is_empty() {
        let page = root.unwrap_or_else(|| pager.alloc());
        let hdr = if page == 1 { 100 } else { 0 };
        build_leaf(pager.page_mut(page), hdr, &[], tree)?;
        return Ok(page);
    }

    // Level 0: pack the cells into leaves, each filled until the next cell
    // would not fit. Carries (page, highest rowid) up to the parent level.
    let mut level: Vec<(u32, i64)> = Vec::new();
    let mut batch: Vec<Vec<u8>> = Vec::new();
    let mut used = 0usize;
    let mut last_rowid = 0i64;
    let single_page_hdr = if root == Some(1) { 100 } else { 0 };

    for (rowid, cell) in cells {
        // While the tree may still turn out to be one page, the leaf has to be
        // measured against page 1's 100-byte file header. Once it cannot be,
        // the root will be an interior page and the leaves sit on plain pages.
        let hdr = if level.is_empty() { single_page_hdr } else { 0 };
        if !batch.is_empty() && hdr + 8 + 2 * (batch.len() + 1) + used + cell.len() > PAGE {
            let page = pager.alloc();
            build_leaf(pager.page_mut(page), 0, &batch, tree)?;
            level.push((page, last_rowid));
            batch.clear();
            used = 0;
        }
        used += cell.len();
        batch.push(cell.clone());
        last_rowid = *rowid;
    }

    if level.is_empty() {
        // Everything fit on one leaf: that leaf IS the root.
        let page = root.unwrap_or_else(|| pager.alloc());
        build_leaf(pager.page_mut(page), single_page_hdr, &batch, tree)?;
        return Ok(page);
    }
    let page = pager.alloc();
    build_leaf(pager.page_mut(page), 0, &batch, tree)?;
    level.push((page, last_rowid));

    // Interior levels. Each page holds n cells plus a right-most pointer, so
    // n + 1 children; the cell key is the highest rowid in the child beneath
    // it, and the right-most child needs no key because nothing bounds it
    // above.
    loop {
        if level.len() == 1 {
            return Ok(level[0].0);
        }
        let root_here = level.len() <= max_interior_children(root, &level)?;
        let mut up: Vec<(u32, i64)> = Vec::new();
        let mut group: Vec<(u32, i64)> = Vec::new();
        let mut used = 0usize;
        let hdr = if root_here && root == Some(1) { 100 } else { 0 };

        for child in &level {
            let cell_len = 4 + varint_len(child.1 as u64);
            // The group's last child becomes the right pointer, so a group of
            // k children costs k-1 cells.
            if !group.is_empty() && hdr + 12 + 2 * group.len() + used + cell_len > PAGE {
                let page = if root_here && up.is_empty() && group.len() == level.len() {
                    root.unwrap_or_else(|| pager.alloc())
                } else {
                    pager.alloc()
                };
                let top = group.last().expect("non-empty group").1;
                build_interior(pager.page_mut(page), hdr, &group)?;
                up.push((page, top));
                group.clear();
                used = 0;
            }
            used += cell_len;
            group.push(*child);
        }
        let page = if root_here && up.is_empty() {
            root.unwrap_or_else(|| pager.alloc())
        } else {
            pager.alloc()
        };
        let top = group.last().expect("non-empty group").1;
        build_interior(pager.page_mut(page), hdr, &group)?;
        up.push((page, top));
        level = up;
    }
}

/// How many children a single interior page could hold, given this level's
/// key sizes — used only to decide whether the level about to be built is the
/// root (and therefore whether it goes on page 1).
fn max_interior_children(root: Option<u32>, level: &[(u32, i64)]) -> Result<usize> {
    let hdr = if root == Some(1) { 100 } else { 0 };
    let mut used = 0usize;
    for (i, child) in level.iter().enumerate() {
        used += 4 + varint_len(child.1 as u64);
        if hdr + 12 + 2 * (i + 1) + used > PAGE {
            return Ok(i);
        }
    }
    Ok(level.len())
}

/// Lay an interior level's `children` on one table-interior page: the last
/// child becomes the right-most pointer in the header, the rest become cells
/// of `child page ‖ varint(highest rowid below it)`.
fn build_interior(page: &mut [u8], hdr: usize, children: &[(u32, i64)]) -> Result<()> {
    let (right, cells) = children.split_last().expect("interior page needs a child");
    let ptrs = hdr + 12;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(cells.len());
    for (child, key) in cells {
        let mut c = Vec::with_capacity(9);
        c.extend_from_slice(&child.to_be_bytes());
        put_varint(&mut c, *key as u64);
        bodies.push(c);
    }
    let total: usize = bodies.iter().map(Vec::len).sum();
    if ptrs + 2 * bodies.len() + total > PAGE {
        return Err(Error::Unsupported(format!(
            "interior page overflow ({} children) — the level packer should have split this",
            children.len()
        )));
    }
    page[hdr] = 0x05; // table interior
    page[hdr + 3..hdr + 5].copy_from_slice(&(bodies.len() as u16).to_be_bytes());
    page[hdr + 8..hdr + 12].copy_from_slice(&right.0.to_be_bytes());
    let mut content = PAGE;
    for (i, c) in bodies.iter().enumerate() {
        content -= c.len();
        page[content..content + c.len()].copy_from_slice(c);
        page[ptrs + 2 * i..ptrs + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    page[hdr + 5..hdr + 7].copy_from_slice(&(content as u16).to_be_bytes());
    Ok(())
}

/// The 100-byte file header, byte-for-byte what stock 3.45.1 leaves after
/// `CREATE TABLE` (xxd-verified). The load-bearing ones: 64/32/32 at
/// @21/22/23 (stock hard-refuses the file otherwise), schema format 4 @44,
/// and version-valid-for @92 == change counter @24 — the equality that makes
/// the @28 page count authoritative for rootpage bounds (the reader's
/// `root_bound` rule, mirrored here from the writing side).
fn write_header(h: &mut [u8], n_pages: u32) {
    h[..16].copy_from_slice(b"SQLite format 3\0");
    h[16..18].copy_from_slice(&(PAGE as u16).to_be_bytes());
    h[18] = 1; // write format: legacy (rollback journal)
    h[19] = 1; // read format
    h[20] = 0; // reserved bytes per page
    h[21] = 64; // max embedded payload fraction — stock REQUIRES 64
    h[22] = 32; // min embedded payload fraction — stock REQUIRES 32
    h[23] = 32; // leaf payload fraction — stock REQUIRES 32
    h[24..28].copy_from_slice(&2u32.to_be_bytes()); // change counter
    h[28..32].copy_from_slice(&n_pages.to_be_bytes()); // database size in pages
    // @32..40: freelist trunk page / freelist count — none.
    h[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
    h[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format 4
    // @48..52 default page cache size 0; @52..56 largest root page 0
    // (auto-vacuum off).
    h[56..60].copy_from_slice(&1u32.to_be_bytes()); // text encoding: UTF-8
    // @60..72: user version / incremental-vacuum mode / application id = 0.
    // @72..92: 20 reserved bytes, zero.
    h[92..96].copy_from_slice(&2u32.to_be_bytes()); // version-valid-for == change counter
    h[96..100].copy_from_slice(&SQLITE_VERSION_NUMBER.to_be_bytes());
}

/// Lay `cells` out on one table-leaf page: 8-byte leaf header at `hdr`
/// (100 on page 1, 0 elsewhere), pointer array forward from `hdr + 8` in
/// rowid order, cell content packed backward from the page end. Cells that
/// do not all fit are the v1 boundary — refused, never split.
fn build_leaf(page: &mut [u8], hdr: usize, cells: &[Vec<u8>], tree: &str) -> Result<()> {
    let ptrs = hdr + 8;
    let total: usize = cells.iter().map(Vec::len).sum();
    if ptrs + 2 * cells.len() + total > PAGE {
        // The level packer sizes every batch against this same bound, so
        // reaching here means the packer and the layout disagree — a bug in
        // this file, not a limit of the format. Named rather than truncated.
        return Err(Error::Unsupported(format!(
            "table `{tree}`: leaf overflow with {} cells — the level packer should have \
             split this",
            cells.len()
        )));
    }
    page[hdr] = 0x0d; // table leaf
    // @hdr+1..3: first freeblock = 0 (the buffer is zeroed).
    page[hdr + 3..hdr + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let mut content = PAGE;
    for (i, c) in cells.iter().enumerate() {
        content -= c.len();
        page[content..content + c.len()].copy_from_slice(c);
        page[ptrs + 2 * i..ptrs + 2 * i + 2].copy_from_slice(&(content as u16).to_be_bytes());
    }
    // Content-start; 4096 when the page is empty (fits u16 — the 0-means-
    // 65536 convention only matters at the 64 KiB page size v1 refuses).
    page[hdr + 5..hdr + 7].copy_from_slice(&(content as u16).to_be_bytes());
    // @hdr+7: fragmented free bytes = 0 — cells are packed contiguously.
    Ok(())
}

/// One table-leaf cell: varint(payload length) ‖ varint(rowid) ‖ record.
/// Payloads are pre-checked ≤ X, so the record is always fully inline.
fn leaf_cell(rowid: i64, record: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(record.len() + 4);
    put_varint(&mut c, record.len() as u64);
    put_varint(&mut c, rowid as u64);
    c.extend_from_slice(record);
    c
}

/// Encode one record: varint(header length, counting itself) ‖ serial-type
/// varints ‖ value bodies. Serial choice mirrors the reader's
/// `decode_serial` exactly — ints pick the smallest of the 0/1 constants
/// (serials 8/9) or 1/2/3/4/6/8-byte signed big-endian (serials 1..=6),
/// floats are serial 7, text/blob 13+2n / 12+2n.
fn encode_record(vals: &[Value]) -> Vec<u8> {
    let mut serials = Vec::new();
    let mut body = Vec::new();
    for v in vals {
        let serial: u64 = match v {
            Value::Null => 0,
            Value::Int(0) => 8,
            Value::Int(1) => 9,
            Value::Int(x) => {
                let (serial, n) = int_serial(*x);
                body.extend_from_slice(&x.to_be_bytes()[8 - n..]);
                serial
            }
            Value::Float(f) => {
                body.extend_from_slice(&f.to_bits().to_be_bytes());
                7
            }
            Value::Text(s) => {
                body.extend_from_slice(s.as_bytes());
                13 + 2 * s.len() as u64
            }
            Value::Blob(b) => {
                body.extend_from_slice(b);
                12 + 2 * b.len() as u64
            }
        };
        put_varint(&mut serials, serial);
    }
    // The header length varint counts ITSELF — the usual fixpoint (grows the
    // guess until the varint of the total no longer changes the total).
    let mut hdr_len = serials.len() + 1;
    loop {
        let n = varint_len(hdr_len as u64);
        if serials.len() + n == hdr_len {
            break;
        }
        hdr_len = serials.len() + n;
    }
    let mut out = Vec::with_capacity(hdr_len + body.len());
    put_varint(&mut out, hdr_len as u64);
    out.extend_from_slice(&serials);
    out.extend_from_slice(&body);
    out
}

/// Smallest signed big-endian width sqlite stores an integer in — 1/2/3/4/
/// 6/8 bytes as serials 1..=6 (there is no 5- or 7-byte form). Callers
/// handle 0 and 1 (serials 8/9) before this.
fn int_serial(x: i64) -> (u64, usize) {
    if (-0x80..0x80).contains(&x) {
        (1, 1)
    } else if (-0x8000..0x8000).contains(&x) {
        (2, 2)
    } else if (-0x0080_0000..0x0080_0000).contains(&x) {
        (3, 3)
    } else if (-0x8000_0000..0x8000_0000).contains(&x) {
        (4, 4)
    } else if (-0x8000_0000_0000..0x8000_0000_0000).contains(&x) {
        (5, 6)
    } else {
        (6, 8)
    }
}

/// sqlite varint: 1–8 big-endian 7-bit groups, or the 9-byte form whose
/// last byte carries 8 bits. The exact mirror of the reader's `varint`.
fn put_varint(out: &mut Vec<u8>, v: u64) {
    if v >> 56 != 0 {
        // 9-byte form: the LAST byte carries the low 8 bits, the first 8
        // bytes carry bits 63..8 in 7-bit groups (b0 = bits 63..57).
        for shift in [57, 50, 43, 36, 29, 22, 15, 8] {
            out.push(((v >> shift) as u8 & 0x7f) | 0x80);
        }
        out.push(v as u8);
        return;
    }
    let mut groups = [0u8; 8];
    let mut n = 0;
    let mut x = v;
    loop {
        groups[n] = (x & 0x7f) as u8;
        x >>= 7;
        n += 1;
        if x == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(groups[i] | if i != 0 { 0x80 } else { 0 });
    }
}

fn varint_len(v: u64) -> usize {
    if v >> 56 != 0 {
        return 9;
    }
    let mut n = 1;
    let mut x = v >> 7;
    while x != 0 {
        n += 1;
        x >>= 7;
    }
    n
}

/// The v1 DDL gate: a `PRIMARY KEY` or `UNIQUE` clause anywhere in the
/// CREATE text means stock would pair the table with an auto-index tree (or,
/// for the INTEGER PRIMARY KEY alias, a rowid contract) this writer does not
/// emit — refused by name, never written without.
///
/// Token sniff over bare words OUTSIDE every quote form but at EVERY paren
/// depth: a table-level `UNIQUE(a, b)` sits inside the parens and must be
/// seen, while a `DEFAULT 'PRIMARY KEY'` string or a `"unique"` identifier
/// must not trip it. A false refusal is safe; a false pass is a lying image.
fn refuse_indexed_ddl(t: &ImageTable) -> Result<()> {
    let words = ddl_words(&t.sql);
    if words.windows(2).any(|w| w[0] == "PRIMARY" && w[1] == "KEY") {
        return Err(Error::Unsupported(format!(
            "table `{}`: PRIMARY KEY in CREATE text (v1 writes plain rowid tables only; \
             the INTEGER PRIMARY KEY rowid alias is planned, not yet written)",
            t.name
        )));
    }
    if words.iter().any(|w| w == "UNIQUE") {
        return Err(Error::Unsupported(format!(
            "table `{}`: UNIQUE in CREATE text (stock backs it with an auto-index tree \
             v1 does not write)",
            t.name
        )));
    }
    Ok(())
}

/// Bare words of a CREATE text, uppercased, skipping all four quote forms
/// (`'…'`, `"…"`, `` `…` ``, `[…]`) whole — same walk as the reader's
/// `constraint_words`, except paren groups are NOT skipped (the constraints
/// v1 refuses live inside them).
fn ddl_words(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(sql[start..i].to_ascii_uppercase());
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteFile;

    fn foo() -> ImageTable {
        ImageTable {
            name: "foo".into(),
            sql: "CREATE TABLE foo (a, b)".into(),
            rows: vec![
                vec![Value::Int(1), Value::Text("x".into())],
                vec![Value::Int(2), Value::Text("y".into())],
            ],
            indexes: Vec::new(),
        }
    }

    /// Rows enough to need several leaves and an interior level above them.
    fn wide(n: usize) -> ImageTable {
        ImageTable {
            name: "wide".into(),
            sql: "CREATE TABLE wide (hex TEXT, ts INTEGER, lat REAL)".into(),
            rows: (0..n)
                .map(|i| {
                    vec![
                        Value::Text(format!("hex{:04}", i % 7)),
                        Value::Int(i as i64 * 3),
                        Value::Float(59.8 + (i % 500) as f64 / 10000.0),
                    ]
                })
                .collect(),
            indexes: Vec::new(),
        }
    }

    /// A tree past one page must still read back row-for-row through our own
    /// reader — the structural check that the leaves chain and the interior
    /// keys point where they claim.
    #[test]
    fn multi_page_tree_round_trips() {
        for n in [60usize, 61, 500, 20_000] {
            let img = write_image(&[wide(n)], PAGE).expect("write");
            assert!(img.len() > PAGE, "n={n} produced a single page");
            let f = SqliteFile::from_bytes(img).expect("read back");
            let tables = f.tables().expect("tables");
            assert_eq!(tables.len(), 1, "n={n}");
            let mut seen = 0usize;
            let mut last = 0i64;
            f.scan_table(&tables[0], &mut |rowid, vals| {
                assert_eq!(rowid, seen as i64 + 1, "n={n}: rowids out of order");
                assert_eq!(vals.len(), 3, "n={n}");
                assert_eq!(vals[1], Value::Int(seen as i64 * 3), "n={n}: wrong row at {rowid}");
                last = rowid;
                seen += 1;
                Ok(())
            })
            .expect("scan");
            assert_eq!(seen, n, "n={n}: row count");
            assert_eq!(last, n as i64, "n={n}: last rowid");
        }
    }

    /// The one that actually decides it: stock sqlite reads the file. Our own
    /// reader agreeing with our own writer proves only that they share a
    /// misunderstanding.
    #[test]
    fn stock_sqlite_reads_a_multi_page_image() {
        let Ok(out) = std::process::Command::new("sqlite3").arg("-version").output() else {
            eprintln!("sqlite3 not on PATH — skipping the stock cross-check");
            return;
        };
        assert!(out.status.success());

        let n = 20_000usize;
        let img = write_image(&[wide(n)], PAGE).expect("write");
        let path = std::env::temp_dir().join(format!("mpedb_wtest_{}.db", std::process::id()));
        std::fs::write(&path, &img).expect("write file");

        let ask = |sql: &str| -> String {
            let o = std::process::Command::new("sqlite3")
                .arg(&path)
                .arg(sql)
                .output()
                .expect("run sqlite3");
            assert!(
                o.status.success(),
                "sqlite3 failed on `{sql}`: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };

        // integrity_check first: it walks every page and every b-tree link,
        // so a malformed interior level fails HERE rather than surfacing as a
        // plausible-looking wrong answer below.
        assert_eq!(ask("PRAGMA integrity_check"), "ok", "stock integrity_check");
        assert_eq!(ask("SELECT COUNT(*) FROM wide"), n.to_string());
        assert_eq!(ask("SELECT SUM(ts) FROM wide"), (0..n as i64).map(|i| i * 3).sum::<i64>().to_string());
        assert_eq!(ask("SELECT ts FROM wide WHERE rowid = 1"), "0");
        assert_eq!(ask("SELECT ts FROM wide WHERE rowid = 12345"), (12_344i64 * 3).to_string());
        assert_eq!(ask("SELECT ts FROM wide ORDER BY rowid DESC LIMIT 1"), ((n as i64 - 1) * 3).to_string());
        let _ = std::fs::remove_file(&path);
    }

    /// Indexes we wrote, checked by the only judge that counts: stock sqlite
    /// walks index order in `integrity_check`, and answers queries by binary
    /// search over it. A key ordering that disagrees with sqlite's shows up
    /// here as a failed check or a missing row — never as a crash, which is
    /// exactly why it needs a test.
    #[test]
    fn stock_sqlite_accepts_our_indexes() {
        if std::process::Command::new("sqlite3").arg("-version").output().is_err() {
            eprintln!("sqlite3 not on PATH — skipping");
            return;
        }
        // Every storage class in one key, including the NULL/number/text/blob
        // ordering and a mix of INTEGER and REAL in one column.
        let n = 4000usize;
        let rows: Vec<Vec<Value>> = (0..n)
            .map(|i| {
                let a = match i % 5 {
                    0 => Value::Null,
                    1 => Value::Int((i as i64 * 7919) % 10_000 - 5000),
                    2 => Value::Float((i as f64) * 0.5 - 1000.0),
                    3 => Value::Text(format!("t{:05}", (i * 31) % 9973)),
                    _ => Value::Blob(vec![(i % 251) as u8, (i % 97) as u8]),
                };
                vec![a, Value::Int((i as i64 * 13) % 1000), Value::Text(format!("p{i}"))]
            })
            .collect();
        let t = ImageTable {
            name: "mixed".into(),
            sql: "CREATE TABLE mixed (a, b, c)".into(),
            rows,
            indexes: vec![
                ImageIndex {
                    name: "ix_a".into(),
                    sql: "CREATE INDEX ix_a ON mixed (a)".into(),
                    columns: vec![0],
                },
                ImageIndex {
                    name: "ix_ba".into(),
                    sql: "CREATE INDEX ix_ba ON mixed (b, a)".into(),
                    columns: vec![1, 0],
                },
            ],
        };
        let img = write_image(&[t], PAGE).expect("write");
        let path = std::env::temp_dir().join(format!("mpedb_ixtest_{}.db", std::process::id()));
        std::fs::write(&path, &img).expect("write file");

        let ask = |sql: &str| -> String {
            let o = std::process::Command::new("sqlite3")
                .arg(&path)
                .arg(sql)
                .output()
                .expect("run sqlite3");
            assert!(o.status.success(), "sqlite3 failed on `{sql}`: {}",
                    String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };

        // integrity_check verifies every index against its table: right
        // entries, right order, right count.
        assert_eq!(ask("PRAGMA integrity_check"), "ok");
        assert_eq!(ask("SELECT COUNT(*) FROM mixed"), n.to_string());
        assert_eq!(
            ask("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name"),
            "ix_a\nix_ba"
        );

        // The index must ANSWER, not merely exist: each of these plans through
        // it, so a mis-sorted tree returns fewer rows than the scan does.
        for (indexed, scanned) in [
            ("SELECT COUNT(*) FROM mixed WHERE a IS NULL",
             "SELECT COUNT(*) FROM mixed NOT INDEXED WHERE a IS NULL"),
            ("SELECT COUNT(*) FROM mixed WHERE a > 0",
             "SELECT COUNT(*) FROM mixed NOT INDEXED WHERE a > 0"),
            ("SELECT COUNT(*) FROM mixed WHERE a > 't'",
             "SELECT COUNT(*) FROM mixed NOT INDEXED WHERE a > 't'"),
            ("SELECT COUNT(*) FROM mixed WHERE b BETWEEN 100 AND 500",
             "SELECT COUNT(*) FROM mixed NOT INDEXED WHERE b BETWEEN 100 AND 500"),
            ("SELECT COUNT(*) FROM mixed WHERE b = 13 AND a IS NULL",
             "SELECT COUNT(*) FROM mixed NOT INDEXED WHERE b = 13 AND a IS NULL"),
        ] {
            assert_eq!(ask(indexed), ask(scanned), "index and scan disagree on `{indexed}`");
        }
        // And the ORDER the index imposes must match the one a sort produces.
        assert_eq!(
            ask("SELECT c FROM mixed ORDER BY a, rowid LIMIT 5"),
            ask("SELECT c FROM mixed NOT INDEXED ORDER BY a, rowid LIMIT 5")
        );
        let _ = std::fs::remove_file(&path);
    }

    fn be32(img: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([img[at], img[at + 1], img[at + 2], img[at + 3]])
    }

    /// The xxd-verified stock-3.45.1 facit, byte for byte: 2 pages / 8192
    /// bytes, the quoted master cell `29 01 06 17 13 13 01 3b …` on page 1,
    /// pointers 0ffa/0ff3 and cells `04 01 03 09 0f 78` /
    /// `05 02 03 01 0f 02 79` on page 2 — serial 9 (constant 1, zero bytes)
    /// for the value 1, serial 15 for 'x'.
    #[test]
    fn foo_facit_byte_for_byte() {
        let img = write_image(&[foo()], 4096).unwrap();
        assert_eq!(img.len(), 8192);

        // Header fields.
        assert_eq!(&img[..16], b"SQLite format 3\0");
        assert_eq!(u16::from_be_bytes([img[16], img[17]]), 4096);
        assert_eq!([img[18], img[19], img[20]], [1, 1, 0]);
        assert_eq!([img[21], img[22], img[23]], [64, 32, 32]);
        assert_eq!(be32(&img, 24), 2); // change counter
        assert_eq!(be32(&img, 28), 2); // database size in pages
        assert_eq!(be32(&img, 32), 0); // freelist trunk
        assert_eq!(be32(&img, 36), 0); // freelist count
        assert_eq!(be32(&img, 40), 1); // schema cookie
        assert_eq!(be32(&img, 44), 4); // schema format
        assert_eq!(be32(&img, 52), 0); // largest root (auto-vacuum off)
        assert_eq!(be32(&img, 56), 1); // UTF-8
        assert!(img[72..92].iter().all(|b| *b == 0));
        assert_eq!(be32(&img, 92), 2); // version-valid-for == change counter
        assert_eq!(be32(&img, 96), 3_045_001);

        // Page 1: leaf header at 100, one master cell packed to the byte end.
        assert_eq!(&img[100..108], &[0x0d, 0, 0, 0, 1, 0x0f, 0xd5, 0]);
        assert_eq!(&img[108..110], &[0x0f, 0xd5]);
        let mut master = vec![0x29, 0x01, 0x06, 0x17, 0x13, 0x13, 0x01, 0x3b];
        master.extend_from_slice(b"table");
        master.extend_from_slice(b"foo");
        master.extend_from_slice(b"foo");
        master.push(0x02);
        master.extend_from_slice(b"CREATE TABLE foo (a, b)");
        assert_eq!(0x0fd5 + master.len(), 4096);
        assert_eq!(&img[0x0fd5..4096], &master[..]);

        // Page 2: pointers in rowid order, content backward from 4096.
        assert_eq!(&img[4096..4104], &[0x0d, 0, 0, 0, 2, 0x0f, 0xf3, 0]);
        assert_eq!(&img[4104..4108], &[0x0f, 0xfa, 0x0f, 0xf3]);
        assert_eq!(&img[4096 + 0x0ffa..4096 + 0x0ffa + 6], &[0x04, 0x01, 0x03, 0x09, 0x0f, 0x78]);
        assert_eq!(
            &img[4096 + 0x0ff3..4096 + 0x0ff3 + 7],
            &[0x05, 0x02, 0x03, 0x01, 0x0f, 0x02, 0x79]
        );

        // And OUR reader — the mirror — round-trips it exactly.
        let f = SqliteFile::from_bytes(img).unwrap();
        let ts = f.tables().unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].name, "foo");
        assert_eq!(ts[0].root_page, 2);
        assert_eq!(ts[0].columns, ["a", "b"]);
        let mut rows = Vec::new();
        f.scan_table(&ts[0], &mut |rowid, vals| {
            rows.push((rowid, vals));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, vec![Value::Int(1), Value::Text("x".into())]),
                (2, vec![Value::Int(2), Value::Text("y".into())]),
            ]
        );
    }

    /// Every value class through write → read, including the int widths on
    /// both sides of each serial boundary (the writer's `int_serial` must be
    /// the exact mirror of the reader's `decode_serial`).
    #[test]
    fn value_classes_round_trip() {
        let vals: Vec<Vec<Value>> = [
            Value::Null,
            Value::Int(0),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(127),
            Value::Int(128),
            Value::Int(-128),
            Value::Int(-129),
            Value::Int(32767),
            Value::Int(32768),
            Value::Int((1 << 23) - 1),
            Value::Int(1 << 23),
            Value::Int((1 << 31) - 1),
            Value::Int(1 << 31),
            Value::Int((1 << 47) - 1),
            Value::Int(1 << 47),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Float(1.5),
            Value::Float(-2.75e300),
            Value::Text(String::new()),
            Value::Text("æøå".into()),
            Value::Blob(Vec::new()),
            Value::Blob(vec![0, 1, 2, 0xff]),
        ]
        .into_iter()
        .map(|v| vec![v])
        .collect();
        let t = ImageTable {
            name: "v".into(),
            sql: "CREATE TABLE v (x)".into(),
            rows: vals.clone(),
            indexes: Vec::new(),
        };
        let img = write_image(&[t], 4096).unwrap();
        let f = SqliteFile::from_bytes(img).unwrap();
        let ts = f.tables().unwrap();
        let mut back = Vec::new();
        f.scan_table(&ts[0], &mut |_rowid, v| {
            back.push(v);
            Ok(())
        })
        .unwrap();
        assert_eq!(back, vals);
    }

    #[test]
    fn refusals_are_named() {
        let unsupported = |r: Result<Vec<u8>>, needle: &str| {
            match r {
                Err(Error::Unsupported(m)) => {
                    assert!(m.contains(needle), "`{m}` should name `{needle}`")
                }
                other => panic!("expected Unsupported({needle}), got {other:?}"),
            }
        };
        let plain = |sql: &str| ImageTable {
            name: "t".into(),
            sql: sql.into(),
            rows: vec![],
            indexes: Vec::new(),
        };

        unsupported(write_image(&[foo()], 8192), "page size 8192");
        unsupported(
            write_image(&[plain("CREATE TABLE t (id INTEGER PRIMARY KEY, v)")], 4096),
            "PRIMARY KEY",
        );
        unsupported(
            write_image(&[plain("CREATE TABLE t (a, b, PRIMARY KEY (a, b))")], 4096),
            "PRIMARY KEY",
        );
        unsupported(write_image(&[plain("CREATE TABLE t (a UNIQUE)")], 4096), "UNIQUE");
        unsupported(
            write_image(&[plain("CREATE TABLE t (a, b, UNIQUE(a, b))")], 4096),
            "UNIQUE",
        );

        // Quoted occurrences are NOT constraints: a default string and a
        // quoted identifier must pass the sniff.
        let quoted = plain(r#"CREATE TABLE t (a TEXT DEFAULT 'PRIMARY KEY', "unique" INTEGER)"#);
        assert!(write_image(&[quoted], 4096).is_ok());

        // Payload boundary: 4058 text bytes = payload exactly 4061 (X) — in;
        // one more — refused by name.
        let big = |n: usize| ImageTable {
            name: "big".into(),
            sql: "CREATE TABLE big (x)".into(),
            rows: vec![vec![Value::Text("x".repeat(n))]],
            indexes: Vec::new(),
        };
        assert!(write_image(&[big(4058)], 4096).is_ok());
        unsupported(write_image(&[big(4059)], 4096), "overflow");

        // 800 minimal rows overflow a single page. v1 refused that; the tree
        // now grows a level instead, so this is no longer a refusal — the
        // assertion is kept, pointed the other way, so a regression back to
        // the one-page cap fails here.
        let many = ImageTable {
            name: "many".into(),
            sql: "CREATE TABLE many (x)".into(),
            rows: (0..800).map(|_| vec![Value::Int(0)]).collect(),
            indexes: Vec::new(),
        };
        let img = write_image(&[many], 4096).expect("800 rows should span pages, not refuse");
        assert!(img.len() > 2 * PAGE, "800 rows should need more than one leaf");

        let empty_row = ImageTable {
            name: "e".into(),
            sql: "CREATE TABLE e (x)".into(),
            rows: vec![vec![]],
            indexes: Vec::new(),
        };
        unsupported(write_image(&[empty_row], 4096), "empty record");
    }

    /// Zero tables is a legal image (the reader's zero-table seed is legal
    /// too): one page, an empty master leaf.
    #[test]
    fn zero_tables_is_one_empty_page() {
        let img = write_image(&[], 4096).unwrap();
        assert_eq!(img.len(), 4096);
        assert_eq!(be32(&img, 28), 1);
        assert_eq!(&img[100..108], &[0x0d, 0, 0, 0, 0, 0x10, 0x00, 0]);
        let f = SqliteFile::from_bytes(img).unwrap();
        assert!(f.tables().unwrap().is_empty());
    }
}
