//! Native, read-only reader for the sqlite3 file format — DESIGN-SQLITE-BACKED
//! §4. No sqlite library anywhere in the dependency graph: the v2 overlay's
//! fall-through reads run under mpedb's own lock protocol, and this reader is
//! that path. The format is documented and frozen (sqlite.org/fileformat2);
//! every claim here is differentially verified against the real library in
//! this crate's tests (rusqlite, dev-dependency only).
//!
//! Scope (v1): rowid tables and WITHOUT ROWID tables, full scans in b-tree
//! order, varint records, overflow chains, `sqlite_master` traversal with a
//! minimal CREATE TABLE column extractor. Refusals by name: WAL-mode files,
//! non-UTF8 text encodings, page 1 corruption. The house rule applies:
//! corrupt input yields [`Error::Corrupt`], never a panic.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Corrupt(String),
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt sqlite file: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported sqlite file: {m}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn corrupt(m: impl Into<String>) -> Error {
    Error::Corrupt(m.into())
}

/// One decoded sqlite value. Text is validated UTF-8 (the file header is
/// checked for UTF-8 encoding before any record is read).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A table listed in `sqlite_master`.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub root_page: u32,
    pub without_rowid: bool,
    /// Column names in declared order, from the CREATE TABLE text.
    pub columns: Vec<String>,
    /// Declared type text per column (may be empty — sqlite allows it).
    pub decl_types: Vec<String>,
    /// Index of the `INTEGER PRIMARY KEY` rowid-alias column, if any: its
    /// record slot is NULL on disk and its VALUE is the rowid.
    pub ipk_column: Option<usize>,
    /// PRIMARY KEY column names in key order — what WITHOUT ROWID storage
    /// leads with.
    pub pk_order: Vec<String>,
}

impl Table {
    /// sqlite's declared-type → affinity rules (datatype3.html §3.1), needed
    /// for ONE read-side conversion: a REAL-affinity column stores an
    /// integral float as an INTEGER on disk and converts back on read — the
    /// differential test caught exactly that.
    fn real_affinity(decl: &str) -> bool {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            return false;
        }
        if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
            return false;
        }
        if d.contains("BLOB") || d.is_empty() {
            return false;
        }
        d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB")
    }
}

pub struct SqliteFile {
    data: Vec<u8>,
    page_size: usize,
    usable: usize,
    n_pages: usize,
}

const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

impl SqliteFile {
    /// Open and validate. The whole file is read into memory — v1 serves the
    /// CLI and tests; the mmap'd variant rides the v2 lock work where the
    /// quiescence guarantees live.
    pub fn open(path: &Path) -> Result<SqliteFile> {
        let mut f = File::open(path)?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        Self::from_bytes(data)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<SqliteFile> {
        if data.len() < 100 || &data[..16] != HEADER_MAGIC {
            return Err(corrupt("missing header magic"));
        }
        let raw_ps = u16::from_be_bytes([data[16], data[17]]);
        let page_size = if raw_ps == 1 { 65536 } else { raw_ps as usize };
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(corrupt(format!("bad page size {raw_ps}")));
        }
        // Bytes 18/19: file format versions; 2 = WAL. The refusal the design
        // demands by fact, not by lock-behavior assumption.
        if data[18] == 2 || data[19] == 2 {
            return Err(Error::Unsupported(
                "WAL-mode file — checkpoint it first (PRAGMA journal_mode=DELETE)".into(),
            ));
        }
        if data[18] > 2 || data[19] > 2 {
            return Err(corrupt("unknown file format version"));
        }
        let enc = u32::from_be_bytes([data[56], data[57], data[58], data[59]]);
        // 0 appears in freshly created, never-written files; those have no
        // tables either, so treating 0 as UTF-8 is safe.
        if enc != 1 && enc != 0 {
            return Err(Error::Unsupported(format!(
                "text encoding {enc} (only UTF-8 is supported)"
            )));
        }
        let reserved = data[20] as usize;
        let usable = page_size
            .checked_sub(reserved)
            .filter(|u| *u >= 480)
            .ok_or_else(|| corrupt("reserved space leaves no usable page"))?;
        if !data.len().is_multiple_of(page_size) {
            return Err(corrupt("file size is not a page multiple"));
        }
        let n_pages = data.len() / page_size;
        Ok(SqliteFile { data, page_size, usable, n_pages })
    }

    fn page(&self, no: u32) -> Result<&[u8]> {
        if no == 0 || no as usize > self.n_pages {
            return Err(corrupt(format!("page {no} out of range")));
        }
        let start = (no as usize - 1) * self.page_size;
        Ok(&self.data[start..start + self.page_size])
    }

    /// All tables from `sqlite_master` (root b-tree on page 1), views and
    /// indexes skipped. `sqlite_master` rows: (type, name, tbl_name,
    /// rootpage, sql).
    pub fn tables(&self) -> Result<Vec<Table>> {
        let mut out = Vec::new();
        self.scan_rowid_tree(1, &mut |_rowid, vals| {
            let [Value::Text(ty), Value::Text(name), _, root, Value::Text(sql)] = &vals[..]
            else {
                // Views have NULL sql? No: views carry sql; internal
                // auto-indexes carry NULL sql — either way, not a table row
                // shape we consume.
                return Ok(());
            };
            if ty != "table" || name.starts_with("sqlite_") {
                return Ok(());
            }
            let root_page = match root {
                Value::Int(r) if *r > 0 && *r <= u32::MAX as i64 => *r as u32,
                _ => return Err(corrupt(format!("table `{name}` has a bad rootpage"))),
            };
            let parsed = parse_create_table(sql)
                .ok_or_else(|| corrupt(format!("unparseable CREATE TABLE for `{name}`")))?;
            out.push(Table {
                name: name.clone(),
                root_page,
                without_rowid: parsed.without_rowid,
                columns: parsed.columns,
                decl_types: parsed.decl_types,
                ipk_column: parsed.ipk_column,
                pk_order: parsed.pk_cols,
            });
            Ok(())
        })?;
        Ok(out)
    }

    /// Scan a table in b-tree order, invoking `f(rowid, values)` per row.
    /// For a rowid table the order is rowid order; the `INTEGER PRIMARY KEY`
    /// alias column (if any) is materialized from the rowid. For a WITHOUT
    /// ROWID table the record's columns are returned in DECLARED order
    /// (sqlite stores PK columns first; this reorders them back) and `rowid`
    /// is 0.
    pub fn scan_table(
        &self,
        t: &Table,
        f: &mut dyn FnMut(i64, Vec<Value>) -> Result<()>,
    ) -> Result<()> {
        if t.without_rowid {
            let order = without_rowid_order(t)?;
            self.scan_index_tree(t.root_page, &mut |payload| {
                let stored = decode_record(payload, self.usable)?;
                if stored.len() < t.columns.len() {
                    return Err(corrupt(format!(
                        "row in `{}` has {} values for {} columns",
                        t.name,
                        stored.len(),
                        t.columns.len()
                    )));
                }
                let mut vals = vec![Value::Null; t.columns.len()];
                for (stored_i, decl_i) in order.iter().enumerate() {
                    vals[*decl_i] = stored[stored_i].clone();
                }
                apply_real_affinity(t, &mut vals);
                f(0, vals)
            })
        } else {
            self.scan_rowid_tree(t.root_page, &mut |rowid, mut vals| {
                if vals.len() < t.columns.len() {
                    // Legal: columns added by ALTER TABLE default to NULL /
                    // their default; v1 fills NULL and the differential test
                    // keeps us honest about defaults we do not evaluate.
                    vals.resize(t.columns.len(), Value::Null);
                }
                vals.truncate(t.columns.len());
                if let Some(ipk) = t.ipk_column {
                    vals[ipk] = Value::Int(rowid);
                }
                apply_real_affinity(t, &mut vals);
                f(rowid, vals)
            })
        }
    }

    /// Walk a table (rowid) b-tree: interior pages type 5, leaves type 13.
    fn scan_rowid_tree(
        &self,
        root: u32,
        f: &mut dyn FnMut(i64, Vec<Value>) -> Result<()>,
    ) -> Result<()> {
        self.walk(root, 0, &mut |page, is_page1| {
            let hdr = if is_page1 { 100 } else { 0 };
            match page[hdr] {
                5 => Ok(WalkStep::Interior),
                13 => Ok(WalkStep::Leaf),
                k => Err(corrupt(format!("unexpected page kind {k} in table tree"))),
            }
        }, &mut |cell| {
            let (payload_len, n1) = varint(cell)?;
            let (rowid, n2) = varint(&cell[n1..])?;
            let payload = self.cell_payload(&cell[n1 + n2..], payload_len as usize, true)?;
            let vals = decode_record(&payload, self.usable)?;
            f(rowid, vals)
        })
    }

    /// Walk an index b-tree (WITHOUT ROWID storage): interior 2, leaves 10.
    fn scan_index_tree(
        &self,
        root: u32,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        self.walk(root, 0, &mut |page, is_page1| {
            let hdr = if is_page1 { 100 } else { 0 };
            match page[hdr] {
                2 => Ok(WalkStep::Interior),
                10 => Ok(WalkStep::Leaf),
                k => Err(corrupt(format!("unexpected page kind {k} in index tree"))),
            }
        }, &mut |cell| {
            let (payload_len, n) = varint(cell)?;
            let payload = self.cell_payload(&cell[n..], payload_len as usize, false)?;
            f(&payload)
        })
    }

    /// Shared depth-first b-tree walk. `kind` classifies a page; `on_leaf_cell`
    /// receives each leaf cell's bytes (starting at the cell, header stripped
    /// by the caller). Interior index cells also CARRY records; for WITHOUT
    /// ROWID scans those are rows too — handled by visiting them in order.
    fn walk(
        &self,
        page_no: u32,
        depth: u32,
        kind: &mut dyn FnMut(&[u8], bool) -> Result<WalkStep>,
        on_leaf_cell: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if depth > 40 {
            return Err(corrupt("b-tree too deep (cycle?)"));
        }
        let page = self.page(page_no)?;
        let is_page1 = page_no == 1;
        let hdr = if is_page1 { 100 } else { 0 };
        let step = kind(page, is_page1)?;
        let interior = matches!(step, WalkStep::Interior);
        let page_type = page[hdr];
        let n_cells = u16::from_be_bytes([page[hdr + 3], page[hdr + 4]]) as usize;
        let head_len = if interior { 12 } else { 8 };
        let ptrs = hdr + head_len;
        if ptrs + 2 * n_cells > self.page_size {
            return Err(corrupt("cell pointer array past page end"));
        }
        for i in 0..n_cells {
            let off =
                u16::from_be_bytes([page[ptrs + 2 * i], page[ptrs + 2 * i + 1]]) as usize;
            if off < ptrs || off >= self.page_size {
                return Err(corrupt("cell offset out of bounds"));
            }
            let cell = &page[off..];
            if interior {
                if cell.len() < 4 {
                    return Err(corrupt("truncated interior cell"));
                }
                let child = u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]);
                self.walk(child, depth + 1, kind, on_leaf_cell)?;
                // An interior INDEX cell carries a record too — for WITHOUT
                // ROWID tables that record IS a row and must be emitted
                // between its subtrees to keep key order.
                if page_type == 2 {
                    on_leaf_cell(&cell[4..])?;
                }
            } else {
                on_leaf_cell(cell)?;
            }
        }
        if interior {
            let rp = hdr + 8;
            let right =
                u32::from_be_bytes([page[rp], page[rp + 1], page[rp + 2], page[rp + 3]]);
            self.walk(right, depth + 1, kind, on_leaf_cell)?;
        }
        Ok(())
    }

    /// Assemble a cell's payload, following the overflow chain when the
    /// inline portion is short. `table_leaf` selects the X threshold
    /// (fileformat2's U-35 for table leaves, ((U-12)*64/255)-23 for index
    /// pages).
    fn cell_payload(
        &self,
        after_header: &[u8],
        payload_len: usize,
        table_leaf: bool,
    ) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = if table_leaf { u - 35 } else { ((u - 12) * 64 / 255) - 23 };
        if payload_len <= x {
            let inline = after_header
                .get(..payload_len)
                .ok_or_else(|| corrupt("inline payload past page end"))?;
            return Ok(inline.to_vec());
        }
        let m = ((u - 12) * 32 / 255) - 23;
        let k = m + (payload_len - m) % (u - 4);
        let inline_len = if k <= x { k } else { m };
        let inline = after_header
            .get(..inline_len)
            .ok_or_else(|| corrupt("inline payload past page end"))?;
        let ovf_ptr = after_header
            .get(inline_len..inline_len + 4)
            .ok_or_else(|| corrupt("missing overflow pointer"))?;
        let mut next = u32::from_be_bytes(ovf_ptr.try_into().expect("4 bytes"));
        let mut out = Vec::with_capacity(payload_len);
        out.extend_from_slice(inline);
        let mut hops = 0usize;
        while out.len() < payload_len {
            if next == 0 {
                return Err(corrupt("overflow chain ended early"));
            }
            hops += 1;
            if hops > payload_len / (u - 4) + 2 {
                return Err(corrupt("overflow chain too long"));
            }
            let p = self.page(next)?;
            next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            let take = (payload_len - out.len()).min(u - 4);
            out.extend_from_slice(&p[4..4 + take]);
        }
        Ok(out)
    }
}

/// The read-side half of the integral-REAL storage optimization.
fn apply_real_affinity(t: &Table, vals: &mut [Value]) {
    for (i, v) in vals.iter_mut().enumerate() {
        if let Value::Int(x) = *v {
            if t.decl_types.get(i).is_some_and(|d| Table::real_affinity(d)) {
                *v = Value::Float(x as f64);
            }
        }
    }
}

enum WalkStep {
    Interior,
    Leaf,
}

/// sqlite varint: 1–9 bytes, big-endian 7-bit groups, 9th byte carries 8.
fn varint(b: &[u8]) -> Result<(i64, usize)> {
    let mut v: u64 = 0;
    for i in 0..8 {
        let byte = *b.get(i).ok_or_else(|| corrupt("truncated varint"))?;
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok((v as i64, i + 1));
        }
    }
    let last = *b.get(8).ok_or_else(|| corrupt("truncated varint"))?;
    Ok((((v << 8) | last as u64) as i64, 9))
}

/// Decode one record (header of serial types + body). `usable` only bounds
/// sanity — the payload is already assembled.
fn decode_record(payload: &[u8], _usable: usize) -> Result<Vec<Value>> {
    let (hdr_len, n) = varint(payload)?;
    let hdr_len = hdr_len as usize;
    if hdr_len < n || hdr_len > payload.len() {
        return Err(corrupt("record header length out of bounds"));
    }
    let mut types = Vec::new();
    let mut pos = n;
    while pos < hdr_len {
        let (t, tn) = varint(&payload[pos..])?;
        pos += tn;
        types.push(t);
    }
    let mut out = Vec::with_capacity(types.len());
    let mut body = hdr_len;
    for t in types {
        let (val, len) = decode_serial(payload, body, t)?;
        out.push(val);
        body += len;
    }
    Ok(out)
}

fn decode_serial(payload: &[u8], at: usize, t: i64) -> Result<(Value, usize)> {
    let need = |n: usize| -> Result<&[u8]> {
        payload
            .get(at..at + n)
            .ok_or_else(|| corrupt("record body truncated"))
    };
    let int_be = |b: &[u8]| -> i64 {
        // sign-extend big-endian two's complement of 1..=8 bytes
        let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
        for &x in b {
            v = (v << 8) | x as i64;
        }
        v
    };
    Ok(match t {
        0 => (Value::Null, 0),
        1 => (Value::Int(int_be(need(1)?)), 1),
        2 => (Value::Int(int_be(need(2)?)), 2),
        3 => (Value::Int(int_be(need(3)?)), 3),
        4 => (Value::Int(int_be(need(4)?)), 4),
        5 => (Value::Int(int_be(need(6)?)), 6),
        6 => (Value::Int(int_be(need(8)?)), 8),
        7 => {
            let b = need(8)?;
            (
                Value::Float(f64::from_bits(u64::from_be_bytes(
                    b.try_into().expect("8 bytes"),
                ))),
                8,
            )
        }
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        10 | 11 => return Err(corrupt(format!("reserved serial type {t}"))),
        t if t >= 12 && t % 2 == 0 => {
            let len = ((t - 12) / 2) as usize;
            (Value::Blob(need(len)?.to_vec()), len)
        }
        t if t >= 13 => {
            let len = ((t - 13) / 2) as usize;
            let s = std::str::from_utf8(need(len)?)
                .map_err(|_| corrupt("invalid utf-8 in text value"))?;
            (Value::Text(s.to_string()), len)
        }
        t => return Err(corrupt(format!("negative serial type {t}"))),
    })
}

// ------------------------------------------------ CREATE TABLE extraction

struct ParsedCreate {
    columns: Vec<String>,
    decl_types: Vec<String>,
    without_rowid: bool,
    ipk_column: Option<usize>,
    /// PRIMARY KEY column names in key order (declared inline or as a table
    /// constraint) — what WITHOUT ROWID storage leads with.
    pk_cols: Vec<String>,
}

/// The stored-vs-declared column order of a WITHOUT ROWID table: sqlite
/// stores PK columns first (in PK order), then the rest in declared order.
/// Returns `order[stored_index] = declared_index`.
fn without_rowid_order(t: &Table) -> Result<Vec<usize>> {
    let mut order = Vec::with_capacity(t.columns.len());
    let mut used = vec![false; t.columns.len()];
    for pk in &t.pk_order {
        let i = t
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(pk))
            .ok_or_else(|| corrupt(format!("PK column `{pk}` not in column list")))?;
        order.push(i);
        used[i] = true;
    }
    for (i, u) in used.iter().enumerate() {
        if !u {
            order.push(i);
        }
    }
    Ok(order)
}

/// Minimal CREATE TABLE parser: column names + declared types + PK shape.
/// Handles quoting (`"x"`, `` `x` ``, `[x]`), nested parens in types/CHECKs,
/// and table-level constraints. It does NOT evaluate defaults or understand
/// generated columns — the differential test is the fence.
fn parse_create_table(sql: &str) -> Option<ParsedCreate> {
    let open = sql.find('(')?;
    let body_and_tail = &sql[open + 1..];
    // Split the body at depth-0 commas, find the matching close paren.
    let mut depth = 0usize;
    let mut end = None;
    let mut in_quote: Option<char> = None;
    for (i, ch) in body_and_tail.char_indices() {
        match in_quote {
            Some(q) => {
                if ch == q {
                    in_quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => in_quote = Some(ch),
                '[' => in_quote = Some(']'),
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            },
        }
    }
    let end = end?;
    let body = &body_and_tail[..end];
    let tail = &body_and_tail[end + 1..];
    let without_rowid = tail.to_ascii_uppercase().contains("WITHOUT ROWID");

    let mut columns = Vec::new();
    let mut decl_types = Vec::new();
    let mut pk_cols: Vec<String> = Vec::new();
    let mut ipk_column = None;

    for part in split_depth0(body) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let upper = part.to_ascii_uppercase();
        if upper.starts_with("PRIMARY KEY") {
            // table constraint: PRIMARY KEY(a, b DESC)
            if let Some(o) = part.find('(') {
                let inner = part[o + 1..].trim_end_matches(')');
                pk_cols = split_depth0(inner)
                    .into_iter()
                    .map(|c| {
                        unquote(
                            c.split_whitespace()
                                .next()
                                .unwrap_or("")
                                .trim_end_matches(','),
                        )
                    })
                    .collect();
            }
            continue;
        }
        if upper.starts_with("UNIQUE")
            || upper.starts_with("CHECK")
            || upper.starts_with("FOREIGN KEY")
            || upper.starts_with("CONSTRAINT")
        {
            continue;
        }
        // A column: name [type tokens] [constraints]
        let (name, rest) = take_identifier(part)?;
        let rest_upper = rest.to_ascii_uppercase();
        // Declared type = tokens up to the first constraint keyword.
        let ty = {
            let mut ty = String::new();
            for tok in rest.split_whitespace() {
                let tu = tok.to_ascii_uppercase();
                if [
                    "PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE",
                    "REFERENCES", "GENERATED", "AS",
                ]
                .contains(&tu.as_str())
                {
                    break;
                }
                if !ty.is_empty() {
                    ty.push(' ');
                }
                ty.push_str(tok);
            }
            ty
        };
        if rest_upper.contains("PRIMARY KEY") {
            pk_cols = vec![name.clone()];
            if ty.eq_ignore_ascii_case("INTEGER") && !without_rowid {
                ipk_column = Some(columns.len());
            }
        }
        columns.push(name);
        decl_types.push(ty);
    }
    // `INTEGER PRIMARY KEY` detection above ran before knowing WITHOUT ROWID
    // (it trails the body); correct for it.
    if without_rowid {
        ipk_column = None;
    }
    Some(ParsedCreate { columns, decl_types, without_rowid, ipk_column, pk_cols })
}

fn split_depth0(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_quote: Option<char> = None;
    for (i, ch) in s.char_indices() {
        match in_quote {
            Some(q) => {
                if ch == q {
                    in_quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => in_quote = Some(ch),
                '[' => in_quote = Some(']'),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&s[start..]);
    out
}

fn take_identifier(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let (q_open, q_close) = match s.chars().next()? {
        '"' => ('"', '"'),
        '`' => ('`', '`'),
        '[' => ('[', ']'),
        _ => {
            let end = s
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(s.len());
            return Some((s[..end].to_string(), &s[end..]));
        }
    };
    let inner_start = q_open.len_utf8();
    let close = s[inner_start..].find(q_close)?;
    let name = s[inner_start..inner_start + close].to_string();
    Some((name, &s[inner_start + close + q_close.len_utf8()..]))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for (o, c) in [('"', '"'), ('`', '`'), ('[', ']')] {
        if s.starts_with(o) && s.ends_with(c) && s.len() >= 2 {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

impl SqliteFile {
    /// Point lookup by rowid — the b-tree descent (interior table cells hold
    /// (child, K) where the child subtree's rowids are ≤ K; the rightmost
    /// pointer covers the rest). Rowid tables only; a WITHOUT ROWID table
    /// has no rowid to seek.
    pub fn seek_rowid(&self, t: &Table, rowid: i64) -> Result<Option<Vec<Value>>> {
        if t.without_rowid {
            return Err(Error::Unsupported(
                "seek_rowid on a WITHOUT ROWID table".into(),
            ));
        }
        let mut page_no = t.root_page;
        for _ in 0..40 {
            let page = self.page(page_no)?;
            let is_page1 = page_no == 1;
            let hdr = if is_page1 { 100 } else { 0 };
            let n_cells = u16::from_be_bytes([page[hdr + 3], page[hdr + 4]]) as usize;
            match page[hdr] {
                5 => {
                    let ptrs = hdr + 12;
                    if ptrs + 2 * n_cells > self.page_size {
                        return Err(corrupt("cell pointer array past page end"));
                    }
                    let mut next = None;
                    for i in 0..n_cells {
                        let off = u16::from_be_bytes([
                            page[ptrs + 2 * i],
                            page[ptrs + 2 * i + 1],
                        ]) as usize;
                        let cell = page
                            .get(off..)
                            .filter(|c| c.len() >= 5)
                            .ok_or_else(|| corrupt("truncated interior cell"))?;
                        let child =
                            u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]);
                        let (k, _) = varint(&cell[4..])?;
                        if rowid <= k {
                            next = Some(child);
                            break;
                        }
                    }
                    page_no = match next {
                        Some(c) => c,
                        None => {
                            let rp = hdr + 8;
                            u32::from_be_bytes([
                                page[rp],
                                page[rp + 1],
                                page[rp + 2],
                                page[rp + 3],
                            ])
                        }
                    };
                }
                13 => {
                    let ptrs = hdr + 8;
                    if ptrs + 2 * n_cells > self.page_size {
                        return Err(corrupt("cell pointer array past page end"));
                    }
                    for i in 0..n_cells {
                        let off = u16::from_be_bytes([
                            page[ptrs + 2 * i],
                            page[ptrs + 2 * i + 1],
                        ]) as usize;
                        let cell = page
                            .get(off..)
                            .ok_or_else(|| corrupt("cell offset out of bounds"))?;
                        let (payload_len, n1) = varint(cell)?;
                        let (r, n2) = varint(&cell[n1..])?;
                        if r == rowid {
                            let payload = self.cell_payload(
                                &cell[n1 + n2..],
                                payload_len as usize,
                                true,
                            )?;
                            let mut vals = decode_record(&payload, self.usable)?;
                            if vals.len() < t.columns.len() {
                                vals.resize(t.columns.len(), Value::Null);
                            }
                            vals.truncate(t.columns.len());
                            if let Some(ipk) = t.ipk_column {
                                vals[ipk] = Value::Int(rowid);
                            }
                            apply_real_affinity(t, &mut vals);
                            return Ok(Some(vals));
                        }
                    }
                    return Ok(None);
                }
                k => return Err(corrupt(format!("unexpected page kind {k} in seek"))),
            }
        }
        Err(corrupt("b-tree too deep (cycle?)"))
    }
}
