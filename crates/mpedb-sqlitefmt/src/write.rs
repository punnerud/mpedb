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
//! - One leaf page per tree: every cell must fit on the root. A table that
//!   needs an interior page is refused, not split.
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
    }
    let n_pages = 1 + tables.len();
    let mut img = vec![0u8; n_pages * PAGE];

    // Data trees: table i roots on page i+2, cells in rowid order 1..N.
    for (i, t) in tables.iter().enumerate() {
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
            cells.push(leaf_cell(rowid, &record));
        }
        build_leaf(&mut img[(i + 1) * PAGE..(i + 2) * PAGE], 0, &cells, &t.name)?;
    }

    // sqlite_master on page 1: (type, name, tbl_name, rootpage, sql) per
    // table — the same record codec as the data, nothing special-cased.
    let mut cells = Vec::with_capacity(tables.len());
    for (i, t) in tables.iter().enumerate() {
        let row = [
            Value::Text("table".into()),
            Value::Text(t.name.clone()),
            Value::Text(t.name.clone()),
            Value::Int(i as i64 + 2),
            Value::Text(t.sql.clone()),
        ];
        let record = encode_record(&row);
        if record.len() > MAX_PAYLOAD {
            return Err(Error::Unsupported(format!(
                "sqlite_master row for table `{}`: record payload {} bytes exceeds {MAX_PAYLOAD} \
                 (v1 writes no overflow chains)",
                t.name,
                record.len()
            )));
        }
        cells.push(leaf_cell(i as i64 + 1, &record));
    }
    build_leaf(&mut img[..PAGE], 100, &cells, "sqlite_master")?;
    write_header(&mut img[..100], n_pages as u32);
    Ok(img)
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
        return Err(Error::Unsupported(format!(
            "table `{tree}` needs an interior page (v1 writes one leaf per tree)"
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
        }
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
        let t = ImageTable { name: "v".into(), sql: "CREATE TABLE v (x)".into(), rows: vals.clone() };
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
        let plain = |sql: &str| ImageTable { name: "t".into(), sql: sql.into(), rows: vec![] };

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
        };
        assert!(write_image(&[big(4058)], 4096).is_ok());
        unsupported(write_image(&[big(4059)], 4096), "overflow");

        // One leaf per tree: 800 minimal rows cannot fit a single page.
        let many = ImageTable {
            name: "many".into(),
            sql: "CREATE TABLE many (x)".into(),
            rows: (0..800).map(|_| vec![Value::Int(0)]).collect(),
        };
        unsupported(write_image(&[many], 4096), "needs an interior page");

        let empty_row = ImageTable {
            name: "e".into(),
            sql: "CREATE TABLE e (x)".into(),
            rows: vec![vec![]],
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
