//! sqlite_corpus — prototype (task #62): run a subset of SQLite's public
//! **sqllogictest corpus** (canonical format, e.g. the gregrahn/sqllogictest
//! mirror) against mpedb and produce a compatibility report.
//!
//! Unlike the curated `tests/slt/*.test` runner in `src/slt.rs` (which uses an
//! mpedb SLT dialect with a `# schema:` header), this binary consumes the
//! *canonical* corpus files unmodified, bridging the model gap with a shim:
//!
//! - **CREATE TABLE shim**: mpedb has no `CREATE TABLE` (task #47) — pass 1
//!   scans the file for `CREATE TABLE` statements, loosely parses them
//!   (affinity-style type mapping: `*INT*`→int64, `*CHAR*/*CLOB*/TEXT`→text,
//!   `REAL/FLOA/DOUB/NUMERIC/DEC`→float64, typeless→int64) and builds the TOML
//!   schema the database is opened with. At runtime `CREATE TABLE` becomes a
//!   shim success, `DROP TABLE` becomes `DELETE FROM t` + "does not exist"
//!   bookkeeping (so `statement error` on double CREATE/DROP behaves like
//!   sqlite).
//! - **Synthetic PK**: corpus tables have no PK and allow duplicate rows;
//!   every table gets a hidden `rowid_ int64` PK. Every `INSERT ... VALUES`
//!   is rewritten to inject a per-table counter (multi-row VALUES handled).
//! - **`SELECT *` rewrite**: a select list of exactly `*` (or `alias.*`) is
//!   expanded to the declared column list (single table, or an INNER JOIN
//!   chain), so the synthetic column never leaks into results.
//! - **`SELECT ALL`** is rewritten to `SELECT` (mpedb has no ALL quantifier).
//!
//! Result comparison follows the canonical sqllogictest conventions: one value
//! per line, NULL as `NULL`, empty string as `(empty)`, `I` via truncation
//! (atoi semantics), `R` as `%.3f`, nosort/rowsort/valuesort, and — because
//! most corpus expectations are hashed — `N values hashing to <md5>` is
//! verified with a built-in RFC 1321 MD5 (self-tested at startup; no new
//! dependency).
//!
//! Failures are *categorized*, not just counted: an error on a statement the
//! corpus expects to succeed is attributed to the first matching feature
//! (subquery, UNION/…, CAST, `||`, outer/cross join, comma join, view, index
//! DDL, …) so unsupported-surface noise separates cleanly from the interesting
//! signal: **wrong results** (statement accepted, answer differs).
//!
//! Usage: `cargo run -p mpedb-testkit --bin sqlite_corpus -- <file.test>...`
//!
//! Flags: `--as-sqlite` also answers to the `sqlite` engine name in
//! `skipif`/`onlyif` (runs the sqlite-only records, and does NOT take the
//! `skipif sqlite` + `halt` exit that truncates most `evidence/` files);
//! `--samples-all` prints example failing statements for *every* category,
//! not just the uncategorized ones — that is how the ranked blocker table in
//! [`design/CORPUS-STATUS.md`] gets its per-category examples.
//!
//! Known shim limitations (also see the final report):
//! - INSERT ... SELECT is not rewritten (fails as subquery).
//! - `SELECT *` in comma-join FROM clauses is not expanded (comma joins are
//!   unsupported anyway; categorized as comma-join).
//! - Float rendering uses Rust `{:.3}`, which matches C `%.3f` for f64 in
//!   practice but is not bit-for-bit proven; `T`-typed floats use a `%.1f`
//!   approximation of sqlite's text rendering.
//! - Re-creating a table with a *different* schema in one file poisons it
//!   (file-authoritative schema); the sampled corpus never does this.

use mpedb::{Config, Database, ExecResult, Value};
use mpedb_testkit::TempDir;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

// ================================================================= md5

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

fn md5_hex(data: &[u8]) -> String {
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (j, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes(chunk[4 * j..4 * j + 4].try_into().unwrap());
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(MD5_K[i])
                    .wrapping_add(m[g])
                    .rotate_left(MD5_S[i]),
            );
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = String::with_capacity(32);
    for w in [a0, b0, c0, d0] {
        for byte in w.to_le_bytes() {
            let _ = write!(out, "{byte:02x}");
        }
    }
    out
}

fn md5_self_test() {
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5_hex(b"The quick brown fox jumps over the lazy dog"),
        "9e107d9d372bb6826bd81d3542a419d6"
    );
}

// ============================================================ slt parsing

#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    No,
    Row,
    Value,
}

enum Expected {
    Literal(Vec<String>),
    Hash { count: usize, md5: String },
}

enum Kind {
    Statement { expect_error: bool },
    Query { types: String, sort: SortMode, expected: Expected },
}

struct Record {
    line: usize,
    kind: Kind,
    sql: String,
    skip: bool,
}

/// Strip a trailing `# comment` from a directive line (corpus files put
/// comments after `onlyif <db>` etc.). `#` never appears inside directives.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

/// `engines`: the names this run answers to in skipif/onlyif. Default is just
/// `mpedb` (we are "neither sqlite nor mssql"); with `--as-sqlite` we also
/// answer to `sqlite`, running the sqlite-only records whose expected values
/// were generated by sqlite itself (supplementary compat data).
fn parse_slt(text: &str, engines: &[&str]) -> Result<Vec<Record>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut recs = Vec::new();
    let mut i = 0;
    'outer: while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        let lineno = i + 1;
        let mut skip = false;
        let mut head = strip_comment(t).to_string();
        // Conditional prefix lines stack in front of the record they guard.
        loop {
            let mut tk = head.split_whitespace();
            match tk.next() {
                Some("skipif") => {
                    if tk.next().is_some_and(|e| engines.contains(&e)) {
                        skip = true;
                    }
                }
                Some("onlyif") => {
                    if !tk.next().is_some_and(|e| engines.contains(&e)) {
                        skip = true;
                    }
                }
                _ => break,
            }
            i += 1;
            while i < lines.len() && {
                let s = lines[i].trim();
                s.is_empty() || s.starts_with('#')
            } {
                i += 1;
            }
            if i >= lines.len() {
                break 'outer;
            }
            head = strip_comment(lines[i].trim()).to_string();
        }
        let mut toks = head.split_whitespace();
        match toks.next() {
            // A conditional halt only fires when its guard applies to us.
            Some("halt") => {
                if skip {
                    i += 1;
                    continue;
                }
                break;
            }
            Some("hash-threshold") => {
                i += 1;
            }
            Some("statement") => {
                let expect_error = matches!(toks.next(), Some("error"));
                i += 1;
                let mut sql = Vec::new();
                while i < lines.len() && !lines[i].trim().is_empty() {
                    if !lines[i].trim_start().starts_with('#') {
                        sql.push(lines[i].trim_end());
                    }
                    i += 1;
                }
                recs.push(Record {
                    line: lineno,
                    kind: Kind::Statement { expect_error },
                    sql: sql.join("\n"),
                    skip,
                });
            }
            Some("query") => {
                let types = toks.next().unwrap_or("T").to_string();
                let sort = match (toks.next(), toks.next()) {
                    (Some("rowsort"), _) => SortMode::Row,
                    (Some("valuesort"), _) => SortMode::Value,
                    _ => SortMode::No, // nosort, a bare label, or nothing
                };
                i += 1;
                let mut sql = Vec::new();
                let mut saw_sep = false;
                while i < lines.len() {
                    let raw = lines[i].trim_end();
                    i += 1;
                    if raw.trim() == "----" {
                        saw_sep = true;
                        break;
                    }
                    if raw.trim().is_empty() {
                        // Query without expected block (rare); treat as 0 rows.
                        break;
                    }
                    if !raw.trim_start().starts_with('#') {
                        sql.push(raw);
                    }
                }
                let mut expected_lines = Vec::new();
                if saw_sep {
                    while i < lines.len() && !lines[i].trim().is_empty() {
                        expected_lines.push(lines[i].trim_end().to_string());
                        i += 1;
                    }
                }
                let expected = parse_expected(expected_lines);
                recs.push(Record {
                    line: lineno,
                    kind: Kind::Query { types, sort, expected },
                    sql: sql.join("\n"),
                    skip,
                });
            }
            Some(other) => {
                return Err(format!("line {lineno}: unknown directive `{other}`"));
            }
            None => {
                i += 1;
            }
        }
    }
    Ok(recs)
}

fn parse_expected(lines: Vec<String>) -> Expected {
    if lines.len() == 1 {
        let w: Vec<&str> = lines[0].split_whitespace().collect();
        if w.len() == 5 && w[1] == "values" && w[2] == "hashing" && w[3] == "to" {
            if let Ok(count) = w[0].parse::<usize>() {
                return Expected::Hash { count, md5: w[4].to_string() };
            }
        }
    }
    Expected::Literal(lines)
}

// ======================================================== light SQL scanner

#[derive(Clone)]
struct Tok {
    up: String,
    start: usize,
    end: usize,
    depth: i32,
    is_word: bool,
}

fn scan(sql: &str) -> Vec<Tok> {
    let b = sql.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            toks.push(Tok { up: "'".into(), start, end: i, depth, is_word: false });
            continue;
        }
        if c == b'(' {
            toks.push(Tok { up: "(".into(), start: i, end: i + 1, depth, is_word: false });
            depth += 1;
            i += 1;
            continue;
        }
        if c == b')' {
            depth -= 1;
            toks.push(Tok { up: ")".into(), start: i, end: i + 1, depth, is_word: false });
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            toks.push(Tok {
                up: sql[start..i].to_ascii_uppercase(),
                start,
                end: i,
                depth,
                is_word: true,
            });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            toks.push(Tok { up: sql[start..i].into(), start, end: i, depth, is_word: false });
            continue;
        }
        // Multi-char operators we care about (`||` for categorization).
        let two = if i + 1 < b.len() { &sql[i..i + 2] } else { "" };
        if matches!(two, "||" | "<=" | ">=" | "<>" | "!=" | "==") {
            toks.push(Tok { up: two.into(), start: i, end: i + 2, depth, is_word: false });
            i += 2;
            continue;
        }
        toks.push(Tok { up: sql[i..i + 1].into(), start: i, end: i + 1, depth, is_word: false });
        i += 1;
    }
    toks
}

// =========================================================== schema shim

#[derive(Clone)]
struct TableInfo {
    /// Name as written in the corpus file.
    raw_name: String,
    /// Lowercased for lookups.
    name: String,
    /// Declared columns (name, toml type) — NOT including the synthetic PK.
    cols: Vec<(String, &'static str)>,
    /// Normalized column signature for re-create comparison.
    signature: String,
    exists: bool,
    next_rowid: i64,
    poisoned: bool,
}

/// Loosely parse `CREATE TABLE name(coldefs...)` (canonical corpus shapes:
/// possibly multi-line, inline constraints, VARCHAR(n)). Returns None if the
/// statement is not a parseable CREATE TABLE.
fn parse_create_table(sql: &str) -> Option<TableInfo> {
    let toks = scan(sql);
    if toks.len() < 4 || toks[0].up != "CREATE" || toks[1].up != "TABLE" {
        return None;
    }
    let mut j = 2;
    if toks.len() > j + 2 && toks[j].up == "IF" && toks[j + 1].up == "NOT" && toks[j + 2].up == "EXISTS"
    {
        j += 3;
    }
    if !toks[j].is_word {
        return None;
    }
    let raw_name = sql[toks[j].start..toks[j].end].to_string();
    j += 1;
    if j >= toks.len() || toks[j].up != "(" {
        return None;
    }
    let body_open = j;
    // Find the matching close paren (depth of the '(' token itself).
    let open_depth = toks[body_open].depth;
    let mut close = None;
    for (k, t) in toks.iter().enumerate().skip(body_open + 1) {
        if t.up == ")" && t.depth == open_depth {
            close = Some(k);
            break;
        }
    }
    let close = close?;
    // Split the token range into column defs at depth == open_depth + 1 commas.
    let inner = &toks[body_open + 1..close];
    let mut defs: Vec<Vec<&Tok>> = vec![Vec::new()];
    for t in inner {
        if t.up == "," && t.depth == open_depth + 1 {
            defs.push(Vec::new());
        } else {
            defs.last_mut().unwrap().push(t);
        }
    }
    let mut cols = Vec::new();
    for def in &defs {
        if def.is_empty() {
            continue;
        }
        let first = def[0];
        if !first.is_word {
            return None;
        }
        // Table-level constraint, not a column.
        if matches!(
            first.up.as_str(),
            "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "CONSTRAINT"
        ) {
            continue;
        }
        let col_name = sql[first.start..first.end].to_string();
        if col_name.eq_ignore_ascii_case("rowid_") {
            return None; // would collide with the synthetic PK
        }
        let type_text: String = def[1..]
            .iter()
            .take_while(|t| {
                !matches!(
                    t.up.as_str(),
                    "PRIMARY" | "NOT" | "NULL" | "UNIQUE" | "DEFAULT" | "CHECK" | "REFERENCES"
                        | "COLLATE"
                )
            })
            .map(|t| t.up.clone())
            .collect::<Vec<_>>()
            .join(" ");
        // sqlite affinity rules, mapped onto mpedb's rigid types.
        let toml_type = if type_text.contains("INT") {
            "int64"
        } else if type_text.contains("CHAR") || type_text.contains("CLOB") || type_text.contains("TEXT")
        {
            "text"
        } else if type_text.contains("REAL")
            || type_text.contains("FLOA")
            || type_text.contains("DOUB")
            || type_text.contains("NUMERIC")
            || type_text.contains("DEC")
        {
            "float64"
        } else {
            "int64" // typeless / blob: int64 is the least-wrong rigid choice
        };
        cols.push((col_name, toml_type));
    }
    if cols.is_empty() {
        return None;
    }
    let signature = cols
        .iter()
        .map(|(n, t)| format!("{}:{t}", n.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(",");
    Some(TableInfo {
        name: raw_name.to_ascii_lowercase(),
        raw_name,
        cols,
        signature,
        exists: false,
        next_rowid: 1,
        poisoned: false,
    })
}

/// mpedb caps the number of user tables per database at `MAX_TABLES - 8` (128
/// total minus an 8-slot system reserve = 120). Tables beyond the cap are left
/// out of the schema and every statement touching them is counted under
/// `engine-table-cap` instead of polluting the other categories. As of the
/// u128-footprint widen (PLAN_FORMAT 32) this is 120, so `select5.test`'s 64
/// tables are all created and differentially tested — the cap is now only a
/// backstop for pathological files, none of which exist in the corpus.
const ENGINE_TABLE_CAP: usize = 120;

struct Shim {
    tables: Vec<TableInfo>,
    /// Lowercased names of tables dropped from the schema by the cap.
    over_cap: Vec<String>,
}

impl Shim {
    fn find(&self, name: &str) -> Option<usize> {
        let lower = name.to_ascii_lowercase();
        self.tables.iter().position(|t| t.name == lower)
    }

    fn over_cap_referenced(&self, toks: &[Tok]) -> Option<String> {
        for t in toks.iter().filter(|t| t.is_word) {
            let lower = t.up.to_ascii_lowercase();
            if self.over_cap.contains(&lower) {
                return Some(lower);
            }
        }
        None
    }

    /// Any known table that is referenced by `sql` but does not currently
    /// "exist" (dropped, or not yet created) → sqlite would say "no such
    /// table". Word-boundary match on the scanner's word tokens.
    fn missing_table_referenced(&self, toks: &[Tok]) -> Option<String> {
        for t in toks.iter().filter(|t| t.is_word) {
            let lower = t.up.to_ascii_lowercase();
            if let Some(idx) = self.tables.iter().position(|ti| ti.name == lower) {
                if !self.tables[idx].exists {
                    return Some(self.tables[idx].raw_name.clone());
                }
            }
        }
        None
    }
}

enum PreparedSql {
    Run(String),
    /// Handled entirely by the shim; counts as success.
    Done,
    /// Shim-simulated engine error (mirrors sqlite semantics, e.g. "no such
    /// table"). Counts as an error for expect-error purposes.
    SimError(String),
    /// Shim cannot express this statement against a rigid schema.
    Unsupported(&'static str, String),
}

impl Shim {
    fn prepare_statement(&mut self, sql: &str) -> PreparedSql {
        let toks = scan(sql);
        if toks.is_empty() {
            return PreparedSql::Run(sql.to_string());
        }
        if let Some(name) = self.over_cap_referenced(&toks) {
            return PreparedSql::Unsupported(
                "engine-table-cap",
                format!("table {name} exceeds mpedb's {ENGINE_TABLE_CAP}-table cap"),
            );
        }
        let head = toks[0].up.as_str();
        if head == "CREATE" && toks.len() > 1 && toks[1].up == "TABLE" {
            return self.shim_create(sql, &toks);
        }
        if head == "DROP" && toks.len() > 1 && toks[1].up == "TABLE" {
            return self.shim_drop(sql, &toks);
        }
        if let Some(missing) = self.missing_table_referenced(&toks) {
            return PreparedSql::SimError(format!("no such table: {missing}"));
        }
        if head == "INSERT" {
            return self.shim_insert(sql, &toks);
        }
        if head == "SELECT" {
            return PreparedSql::Run(shim_select(sql, self));
        }
        PreparedSql::Run(sql.to_string())
    }

    fn shim_create(&mut self, sql: &str, toks: &[Tok]) -> PreparedSql {
        let Some(parsed) = parse_create_table(sql) else {
            return PreparedSql::Run(sql.to_string()); // engine will reject; categorized
        };
        let if_not_exists = toks.len() > 4 && toks[2].up == "IF";
        match self.find(&parsed.name) {
            Some(idx) => {
                let t = &mut self.tables[idx];
                if t.poisoned {
                    return PreparedSql::Unsupported(
                        "table-recreate",
                        format!("table {} re-created with a different schema", t.raw_name),
                    );
                }
                if t.exists {
                    if if_not_exists {
                        return PreparedSql::Done;
                    }
                    return PreparedSql::SimError(format!("table {} already exists", t.raw_name));
                }
                if t.signature != parsed.signature {
                    t.poisoned = true;
                    return PreparedSql::Unsupported(
                        "table-recreate",
                        format!("table {} re-created with a different schema", t.raw_name),
                    );
                }
                t.exists = true;
                // Re-create after a DROP: make sure the fixed table is empty.
                PreparedSql::Run(format!("DELETE FROM {}", t.raw_name))
            }
            None => PreparedSql::Run(sql.to_string()), // not in the pass-1 schema
        }
    }

    fn shim_drop(&mut self, sql: &str, toks: &[Tok]) -> PreparedSql {
        let mut j = 2;
        let mut if_exists = false;
        if toks.len() > j + 1 && toks[j].up == "IF" && toks[j + 1].up == "EXISTS" {
            if_exists = true;
            j += 2;
        }
        if j >= toks.len() || !toks[j].is_word {
            return PreparedSql::Run(sql.to_string());
        }
        let name = &sql[toks[j].start..toks[j].end];
        match self.find(name) {
            Some(idx) if self.tables[idx].exists => {
                self.tables[idx].exists = false;
                PreparedSql::Run(format!("DELETE FROM {}", self.tables[idx].raw_name))
            }
            _ if if_exists => PreparedSql::Done,
            _ => PreparedSql::SimError(format!("no such table: {name}")),
        }
    }

    /// `INSERT INTO t [(cols)] VALUES (..),(..)` → inject the synthetic
    /// `rowid_` counter into the column list and every tuple.
    fn shim_insert(&mut self, sql: &str, toks: &[Tok]) -> PreparedSql {
        if toks.len() < 4 || toks[1].up != "INTO" || !toks[2].is_word {
            return PreparedSql::Run(sql.to_string());
        }
        let table_raw = sql[toks[2].start..toks[2].end].to_string();
        let Some(idx) = self.find(&table_raw) else {
            return PreparedSql::Run(sql.to_string());
        };
        let mut j = 3;
        let mut col_list: Option<String> = None;
        if toks[j].up == "(" {
            let open_depth = toks[j].depth;
            let open = j;
            j += 1;
            while j < toks.len() && !(toks[j].up == ")" && toks[j].depth == open_depth) {
                j += 1;
            }
            if j >= toks.len() {
                return PreparedSql::Run(sql.to_string());
            }
            col_list = Some(sql[toks[open].end..toks[j].start].to_string());
            j += 1;
        }
        if j >= toks.len() || toks[j].up != "VALUES" {
            return PreparedSql::Run(sql.to_string()); // INSERT ... SELECT etc.
        }
        let values_text = &sql[toks[j].end..];
        let Some(tuples) = split_tuples(values_text) else {
            return PreparedSql::Run(sql.to_string());
        };
        let cols = match col_list {
            Some(c) => c,
            None => self.tables[idx]
                .cols
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(", "),
        };
        let mut out = format!("INSERT INTO {table_raw} (rowid_, {cols}) VALUES ");
        for (k, tup) in tuples.iter().enumerate() {
            let rid = self.tables[idx].next_rowid;
            self.tables[idx].next_rowid += 1;
            if k > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "({rid}, {tup})");
        }
        PreparedSql::Run(out)
    }
}

/// Split `(a,b),(c,d)` into top-level tuple bodies (string-aware).
fn split_tuples(text: &str) -> Option<Vec<String>> {
    let b = text.as_bytes();
    let mut tuples = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] != b'(' {
            return None;
        }
        let start = i + 1;
        let mut depth = 1;
        i += 1;
        while i < b.len() && depth > 0 {
            match b[i] {
                b'\'' => {
                    i += 1;
                    while i < b.len() {
                        if b[i] == b'\'' {
                            if i + 1 < b.len() && b[i + 1] == b'\'' {
                                i += 2;
                                continue;
                            }
                            break;
                        }
                        i += 1;
                    }
                }
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return None;
        }
        tuples.push(text[start..i - 1].to_string());
    }
    if tuples.is_empty() {
        None
    } else {
        Some(tuples)
    }
}

/// SELECT shims: strip the `ALL` quantifier; expand a lone `*` (or `alias.*`)
/// select list to the declared column list so `rowid_` never leaks.
fn shim_select(sql: &str, shim: &Shim) -> String {
    let mut sql = strip_select_all(sql);
    if let Some(expanded) = expand_star(&sql, shim) {
        sql = expanded;
    }
    sql
}

/// Rewrite every `SELECT ALL` to `SELECT` (top level and subqueries — the
/// latter fail anyway, but keep the text consistent).
fn strip_select_all(sql: &str) -> String {
    let toks = scan(sql);
    let mut cut: Vec<(usize, usize)> = Vec::new();
    for w in toks.windows(2) {
        if w[0].up == "SELECT" && w[1].up == "ALL" {
            cut.push((w[1].start, w[1].end));
        }
    }
    if cut.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for (s, e) in cut {
        out.push_str(&sql[pos..s]);
        pos = e;
    }
    out.push_str(&sql[pos..]);
    out
}

const CLAUSE_KEYWORDS: [&str; 7] = ["WHERE", "GROUP", "ORDER", "LIMIT", "HAVING", "OFFSET", "UNION"];

/// If the select list is exactly `*` or `alias.*`, and the FROM clause is a
/// single table or a chain of `[INNER] JOIN ... ON ...`, expand the star to
/// the declared columns (qualified when multiple sources or an alias filter).
fn expand_star(sql: &str, shim: &Shim) -> Option<String> {
    let toks = scan(sql);
    if toks.is_empty() || toks[0].up != "SELECT" {
        return None;
    }
    let mut j = 1;
    if j < toks.len() && toks[j].up == "DISTINCT" {
        j += 1;
    }
    // Select list must be `*` or `word . *`.
    let (star_start, star_end, alias_filter);
    if j < toks.len() && toks[j].up == "*" {
        star_start = toks[j].start;
        star_end = toks[j].end;
        alias_filter = None;
        j += 1;
    } else if j + 2 < toks.len()
        && toks[j].is_word
        && toks[j + 1].up == "."
        && toks[j + 2].up == "*"
    {
        star_start = toks[j].start;
        star_end = toks[j + 2].end;
        alias_filter = Some(sql[toks[j].start..toks[j].end].to_ascii_lowercase());
        j += 3;
    } else {
        return None;
    }
    if j >= toks.len() || toks[j].up != "FROM" || toks[j].depth != 0 {
        return None;
    }

    // FROM-level paren groups — `FROM ( a JOIN b ON … )` — are associativity
    // no-ops the engine accepts since #64, but the depth-based walk below
    // would see the whole group as a subexpression and bail (which is how
    // the shim's rowid_ column leaked as 123 phantom "wrong results" the day
    // paren-FROM landed). Flatten them: drop the group's paren tokens and
    // pull the interior up one level. Repeats for nesting.
    let mut flat: Vec<Tok> = toks.to_vec();
    while let Some(from) = flat.iter().position(|t| t.up == "FROM" && t.depth == 0) {
        let open = from + 1;
        if open >= flat.len() || flat[open].up != "(" {
            break;
        }
        let mut depth = 0i32;
        let mut close = None;
        for (m, t) in flat.iter().enumerate().skip(open) {
            if t.up == "(" {
                depth += 1;
            } else if t.up == ")" {
                depth -= 1;
                if depth == 0 {
                    close = Some(m);
                    break;
                }
            }
        }
        let c = close?;
        for t in &mut flat[open + 1..c] {
            t.depth -= 1;
        }
        flat.remove(c);
        flat.remove(open);
    }
    let toks: &[Tok] = &flat;

    // `ident [AS alias | bare-alias]` at position k → (name, qualifier, next k).
    fn parse_source(sql: &str, toks: &[Tok], mut k: usize) -> Option<(String, String, usize)> {
        const STOP: [&str; 8] = ["INNER", "JOIN", "ON", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL"];
        if k >= toks.len() || !toks[k].is_word || CLAUSE_KEYWORDS.contains(&toks[k].up.as_str()) {
            return None;
        }
        let name_raw = sql[toks[k].start..toks[k].end].to_string();
        let mut qual = name_raw.clone();
        k += 1;
        if k < toks.len() && toks[k].up == "AS" && toks[k].depth == 0 {
            k += 1;
            if k >= toks.len() || !toks[k].is_word {
                return None;
            }
            qual = sql[toks[k].start..toks[k].end].to_string();
            k += 1;
        } else if k < toks.len()
            && toks[k].is_word
            && toks[k].depth == 0
            && !CLAUSE_KEYWORDS.contains(&toks[k].up.as_str())
            && !STOP.contains(&toks[k].up.as_str())
        {
            qual = sql[toks[k].start..toks[k].end].to_string();
            k += 1;
        }
        Some((name_raw, qual, k))
    }

    // Parse FROM sources: ident [AS alias] (INNER? JOIN ident [AS alias] ON ...)*
    let mut sources: Vec<(String, String)> = Vec::new(); // (table lower, qualifier raw)
    let (name_raw, qual, mut k) = parse_source(sql, toks, j + 1)?;
    sources.push((name_raw.to_ascii_lowercase(), qual));
    loop {
        if k >= toks.len() || toks[k].depth != 0 {
            break;
        }
        match toks[k].up.as_str() {
            "INNER" => {
                k += 1;
                if k >= toks.len() || toks[k].up != "JOIN" {
                    return None;
                }
                k += 1;
            }
            // The comma-join executes since #56 — its stars must expand too,
            // or the shim's synthetic rowid_ column leaks into the output
            // (542 phantom "wrong results" the day comma-joins landed).
            "JOIN" | "," => k += 1,
            // CROSS JOIN and LEFT [OUTER] JOIN execute as well — same rule,
            // same phantom otherwise (94 of them the day CROSS landed).
            "CROSS" => {
                k += 1;
                if k >= toks.len() || toks[k].up != "JOIN" {
                    return None;
                }
                k += 1;
            }
            "LEFT" => {
                k += 1;
                if k < toks.len() && toks[k].up == "OUTER" {
                    k += 1;
                }
                if k >= toks.len() || toks[k].up != "JOIN" {
                    return None;
                }
                k += 1;
            }
            "RIGHT" | "FULL" | "NATURAL" => return None,
            _ => break, // WHERE/ORDER/... or end of statement
        }
        let (name_raw, qual, nk) = parse_source(sql, toks, k)?;
        sources.push((name_raw.to_ascii_lowercase(), qual));
        k = nk;
        // Skip any ON condition: advance to the next JOIN/INNER/comma or
        // clause keyword at depth 0 (or end of tokens). Stopping at the comma
        // matters — a comma source has no ON, and skipping past it would
        // swallow the NEXT source.
        while k < toks.len() {
            let t = &toks[k];
            if t.depth == 0
                && (t.up == ","
                    || (t.is_word
                        && (t.up == "JOIN"
                            || t.up == "INNER"
                            || t.up == "CROSS"
                            || t.up == "LEFT"
                            || CLAUSE_KEYWORDS.contains(&t.up.as_str()))))
            {
                break;
            }
            k += 1;
        }
    }
    // Build the expansion.
    let multi = sources.len() > 1;
    let mut cols = Vec::new();
    for (tname, qual) in &sources {
        if let Some(af) = &alias_filter {
            if af != &qual.to_ascii_lowercase() && af != tname {
                continue;
            }
        }
        let idx = shim.tables.iter().position(|t| &t.name == tname)?;
        for (cname, _) in &shim.tables[idx].cols {
            if multi || alias_filter.is_some() {
                cols.push(format!("{qual}.{cname}"));
            } else {
                cols.push(cname.clone());
            }
        }
    }
    if cols.is_empty() {
        return None;
    }
    Some(format!(
        "{}{}{}",
        &sql[..star_start],
        cols.join(", "),
        &sql[star_end..]
    ))
}

// ======================================================== categorization

/// Blank out string literals so their contents never trip keyword matching.
fn strip_strings(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            out.push_str("''");
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn has_word(toks: &[Tok], word: &str) -> bool {
    toks.iter().any(|t| t.is_word && t.up == word)
}

/// FROM clause (depth 0) contains a top-level comma → comma join.
fn has_comma_join(toks: &[Tok]) -> bool {
    let Some(from) = toks
        .iter()
        .position(|t| t.is_word && t.up == "FROM" && t.depth == 0)
    else {
        return false;
    };
    for t in &toks[from + 1..] {
        if t.depth != 0 {
            continue;
        }
        if t.is_word && CLAUSE_KEYWORDS.contains(&t.up.as_str()) {
            break;
        }
        if t.up == "," {
            return true;
        }
    }
    false
}

/// All feature categories present in a failing statement (for the blocker
/// ranking); `primary_category` picks the first by priority for the table.
fn categories(sql: &str, err: &str) -> Vec<&'static str> {
    let clean = strip_strings(sql);
    let toks = scan(&clean);
    let upper = clean.to_ascii_uppercase();
    let squeezed: String = upper.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cats = Vec::new();
    if squeezed.contains("(SELECT") || squeezed.contains("( SELECT") {
        cats.push("subquery");
    }
    if has_word(&toks, "UNION") || has_word(&toks, "INTERSECT") || has_word(&toks, "EXCEPT") {
        cats.push("compound-select");
    }
    if squeezed.contains("CAST(") || squeezed.contains("CAST (") {
        cats.push("cast");
    }
    if toks.iter().any(|t| t.up == "||") {
        cats.push("concat-||");
    }
    if has_word(&toks, "LEFT")
        || has_word(&toks, "RIGHT")
        || has_word(&toks, "FULL")
        || has_word(&toks, "CROSS")
        || has_word(&toks, "NATURAL")
        || has_word(&toks, "OUTER")
    {
        cats.push("outer/cross-join");
    }
    if has_comma_join(&toks) {
        cats.push("comma-join");
    }
    for agg in ["COUNT", "SUM", "AVG", "MIN", "MAX", "TOTAL", "GROUP_CONCAT"] {
        if squeezed.contains(&format!("{agg}(DISTINCT"))
            || squeezed.contains(&format!("{agg}( DISTINCT"))
            || squeezed.contains(&format!("{agg}(ALL"))
            || squeezed.contains(&format!("{agg}( ALL"))
        {
            cats.push("agg-distinct/all");
            break;
        }
    }
    if has_word(&toks, "TOTAL") || has_word(&toks, "GROUP_CONCAT") || has_word(&toks, "GLOB") {
        cats.push("sqlite-func");
    }
    if squeezed.contains("CREATE VIEW")
        || squeezed.contains("CREATE TEMP VIEW")
        || squeezed.contains("DROP VIEW")
    {
        cats.push("view");
    }
    if squeezed.contains("CREATE INDEX")
        || squeezed.contains("CREATE UNIQUE INDEX")
        || squeezed.contains("DROP INDEX")
        || squeezed.starts_with("REINDEX")
    {
        cats.push("index-ddl");
    }
    if has_word(&toks, "TRIGGER") {
        cats.push("trigger-ddl");
    }
    if squeezed.contains("REPLACE INTO") || squeezed.contains("INSERT OR") {
        cats.push("insert-or/replace");
    }
    if squeezed.starts_with("INSERT") && has_word(&toks, "SELECT") {
        cats.push("insert-select");
    }
    if squeezed.starts_with("BEGIN")
        || squeezed.starts_with("COMMIT")
        || squeezed.starts_with("ROLLBACK")
        || squeezed.starts_with("SAVEPOINT")
    {
        cats.push("txn-stmt");
    }
    if squeezed.starts_with("PRAGMA")
        || squeezed.starts_with("VACUUM")
        || squeezed.starts_with("ANALYZE")
        || squeezed.starts_with("ALTER TABLE")
        || squeezed.starts_with("ATTACH")
        || squeezed.starts_with("DETACH")
    {
        cats.push("sqlite-admin");
    }
    if err.contains("ivision") {
        cats.push("div-by-zero-semantics");
    }
    if squeezed.contains("IN ()") || squeezed.contains("IN ( )") {
        cats.push("empty-IN-list");
    }
    if squeezed.starts_with("SELECT") && !has_word(&toks, "FROM") {
        cats.push("select-without-from");
    }
    if squeezed.starts_with("CREATE TABLE") {
        cats.push("create-table");
    }
    // Parser-message buckets (verified empirically): mpedb has no unary `+`
    // ("expected an expression" on `SELECT + col`) and no select-item aliases
    // ("expected FROM" on `SELECT col AS x` / `SELECT col x`).
    if cats.is_empty() && err.contains("expected an expression") {
        cats.push("unary-plus/sign-syntax");
    }
    if cats.is_empty() && err.contains("expected FROM") {
        cats.push("select-item-alias");
    }
    // The deliberate arm-type refusal. It must out-rank the syntactic buckets
    // above: these statements are machine-generated expression soup, so almost
    // every one of them also *contains* a CAST, a `(SELECT`, or no FROM — and
    // attributing them there hid the single largest real blocker behind three
    // categories that are not what the engine actually rejected.
    if err.contains("cannot mix coalesce() argument types")
        || err.contains("cannot mix CASE result types")
        || err.contains("cannot mix nullif() argument types")
    {
        cats.insert(0, "mixed-arm-types");
    }
    // Another *shim* artifact: `shim_select` expands a bare `*` only in the
    // OUTER select list, so `x IN (SELECT * FROM t)` still sees the synthetic
    // `rowid_` column and looks like a 2-column IN subquery. sqlite's `t` has
    // one column there. (The corpus has no row-value INs, so this message is
    // the artifact, not a real arity gap.)
    if err.contains("an IN subquery must select exactly one column") {
        cats.insert(0, "shim-star-arity");
    }
    // A *shim* artifact, not an index-DDL gap: `DROP TABLE` is simulated as
    // `DELETE FROM` (a real DROP would burn one of mpedb's 64 lifetime table
    // ids, and the corpus re-creates its tables hundreds of times per file),
    // so every CREATE INDEX in a re-created table's block piles onto the SAME
    // live table until it trips mpedb's 32-indexes-per-table cap. sqlite drops
    // the indexes with the table and never has more than a handful live.
    if err.contains("indexes (max") {
        cats.insert(0, "shim-index-accumulation");
    }
    if err.contains("ENGINE PANIC") {
        cats.insert(0, "ENGINE-PANIC");
    }
    if cats.is_empty() {
        cats.push("other");
    }
    cats
}

// =========================================================== value rendering

/// Canonical sqllogictest rendering. `I` uses atoi semantics (truncation),
/// `R` is `%.3f`, `T` maps non-printables to `@` and empty to `(empty)`.
fn render_value(v: &Value, tc: u8) -> String {
    if v.is_null() {
        return "NULL".into();
    }
    match tc {
        b'I' => match v {
            Value::Int(x) => x.to_string(),
            Value::Timestamp(x) => x.to_string(),
            Value::Float(x) => format!("{}", x.trunc() as i64),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Text(s) => format!("{}", atoi(s)),
            other => format!("{other}"),
        },
        b'R' => match v {
            Value::Float(x) => format!("{x:.3}"),
            Value::Int(x) => format!("{:.3}", *x as f64),
            Value::Timestamp(x) => format!("{:.3}", *x as f64),
            Value::Text(s) => format!("{:.3}", s.trim().parse::<f64>().unwrap_or(0.0)),
            Value::Bool(b) => format!("{:.3}", if *b { 1.0 } else { 0.0 }),
            other => format!("{other}"),
        },
        _ => match v {
            Value::Text(s) => {
                if s.is_empty() {
                    "(empty)".into()
                } else {
                    s.chars()
                        .map(|c| if (' '..='~').contains(&c) { c } else { '@' })
                        .collect()
                }
            }
            Value::Int(x) => x.to_string(),
            Value::Timestamp(x) => x.to_string(),
            // sqlite renders a REAL's text with a trailing .0 when integral.
            Value::Float(x) => {
                if x.fract() == 0.0 && x.abs() < 1e15 {
                    format!("{x:.1}")
                } else {
                    format!("{x}")
                }
            }
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            other => format!("{other}"),
        },
    }
}

fn atoi(s: &str) -> i64 {
    let s = s.trim_start();
    let mut end = 0;
    let b = s.as_bytes();
    if !b.is_empty() && (b[0] == b'-' || b[0] == b'+') {
        end = 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    s[..end].parse::<i64>().unwrap_or(0)
}

// ================================================================ reporting

#[derive(Default)]
struct FileReport {
    name: String,
    fatal: Option<String>,
    total: usize,
    skipped: usize,
    stmt_pass: usize,
    query_pass: usize,
    hash_verified: usize,
    unsupported: BTreeMap<&'static str, usize>,
    co_counts: BTreeMap<&'static str, usize>,
    /// `(line, sql, error)` samples of failing statements, keyed by primary
    /// category. Only `other` is sampled by default; `--samples-all` samples
    /// every category (how the ranked blocker table gets its examples).
    other_samples: BTreeMap<&'static str, Vec<(usize, String, String)>>,
    wrong: Vec<Wrong>,
    wrong_total: usize,
    errmis: Vec<(usize, String)>,
    errmis_total: usize,
    /// Post-run `Database::verify()` failure — page-accounting corruption
    /// after the file's full statement churn. Always a real engine bug.
    verify_failed: Option<String>,
}

struct Wrong {
    line: usize,
    sql: String,
    detail: String,
    /// Expected-ok write statements that had FAILED before this query ran: a
    /// nonzero count means the database state may already have diverged from
    /// sqlite's, so the mismatch may be a cascade rather than an answer bug.
    failed_writes_before: usize,
}

impl FileReport {
    fn pass(&self) -> usize {
        self.stmt_pass + self.query_pass
    }
    fn unsupported_total(&self) -> usize {
        self.unsupported.values().sum()
    }
    /// Keep up to `MAX_SAMPLES_PER_CAT` failing statements per category, so
    /// the ranked blocker table can quote a real example for each.
    fn sample(&mut self, cat: &'static str, line: usize, sql: &str, err: &str) {
        if !(cat == "other" || SAMPLE_ALL.load(std::sync::atomic::Ordering::Relaxed)) {
            return;
        }
        let slot = self.other_samples.entry(cat).or_default();
        if slot.len() < MAX_SAMPLES_PER_CAT {
            slot.push((line, truncate_sql(sql, 120), truncate_sql(err, 160)));
        }
    }
}

/// Set by `--samples-all`: sample failing statements in every category, not
/// just the uncategorized ones.
static SAMPLE_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const MAX_WRONG_STORED: usize = 12;
const MAX_SAMPLES_PER_CAT: usize = 3;
const MAX_ERRMIS_STORED: usize = 5;

fn truncate_sql(sql: &str, max: usize) -> String {
    let one_line: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= max {
        one_line
    } else {
        format!("{}…", &one_line[..max])
    }
}

// ================================================================= runner

fn run_file(path: &Path, engines: &[&str]) -> FileReport {
    let mut rep = FileReport {
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        ..FileReport::default()
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            rep.fatal = Some(format!("cannot read: {e}"));
            return rep;
        }
    };
    let records = match parse_slt(&text, engines) {
        Ok(r) => r,
        Err(e) => {
            rep.fatal = Some(format!("slt parse: {e}"));
            return rep;
        }
    };

    // Pass 1: gather CREATE TABLE statements we will execute.
    let mut shim = Shim { tables: Vec::new(), over_cap: Vec::new() };
    for rec in records.iter().filter(|r| !r.skip) {
        if let Kind::Statement { .. } = rec.kind {
            if let Some(t) = parse_create_table(&rec.sql) {
                if shim.find(&t.name).is_none() && !shim.over_cap.contains(&t.name) {
                    if shim.tables.len() < ENGINE_TABLE_CAP {
                        shim.tables.push(t);
                    } else {
                        shim.over_cap.push(t.name);
                    }
                }
            }
        }
    }

    // Build the TOML config and open a fresh database.
    let dir = match TempDir::new("corpus") {
        Ok(d) => d,
        Err(e) => {
            rep.fatal = Some(format!("tempdir: {e}"));
            return rep;
        }
    };
    let mut cfg = format!(
        "[database]\npath = \"{}\"\nsize_mb = 128\nmax_readers = 8\n",
        dir.db_path("corpus").display()
    );
    if shim.tables.is_empty() {
        // Config needs at least one table; none of this file's DDL parsed.
        cfg.push_str("\n[[table]]\nname = \"shim_dummy_\"\nprimary_key = [\"rowid_\"]\n  [[table.column]]\n  name = \"rowid_\"\n  type = \"int64\"\n");
    }
    for t in &shim.tables {
        let _ = write!(cfg, "\n[[table]]\nname = \"{}\"\nprimary_key = [\"rowid_\"]\n", t.name);
        cfg.push_str("  [[table.column]]\n  name = \"rowid_\"\n  type = \"int64\"\n");
        for (cn, ct) in &t.cols {
            let _ = write!(
                cfg,
                "  [[table.column]]\n  name = \"{}\"\n  type = \"{ct}\"\n",
                cn.to_ascii_lowercase()
            );
        }
    }
    let db = match Config::from_toml_str(&cfg).and_then(Database::open_with_config) {
        Ok(db) => db,
        Err(e) => {
            rep.fatal = Some(format!("open: {e}"));
            return rep;
        }
    };

    // Expected-ok statements that failed and might have changed state in
    // sqlite (INSERT/UPDATE/DELETE/DDL): once nonzero, later query mismatches
    // may be state-divergence cascades rather than answer bugs.
    let mut failed_writes = 0usize;
    for rec in &records {
        match &rec.kind {
            Kind::Statement { expect_error } => {
                rep.total += 1;
                if rec.skip {
                    rep.skipped += 1;
                    continue;
                }
                let outcome = shim.prepare_statement(&rec.sql);
                let result: Result<(), String> = match outcome {
                    PreparedSql::Done => Ok(()),
                    PreparedSql::SimError(msg) => Err(format!("[shim] {msg}")),
                    PreparedSql::Unsupported(cat, msg) => {
                        if *expect_error {
                            rep.stmt_pass += 1;
                        } else {
                            *rep.unsupported.entry(cat).or_default() += 1;
                            *rep.co_counts.entry(cat).or_default() += 1;
                            failed_writes += 1;
                            let _ = msg;
                        }
                        continue;
                    }
                    PreparedSql::Run(sql) => exec_sql(&db, &sql).map(|_| ()),
                };
                match (result, expect_error) {
                    (Ok(()), false) => rep.stmt_pass += 1,
                    (Err(_), true) => rep.stmt_pass += 1,
                    (Ok(()), true) => {
                        rep.errmis_total += 1;
                        if rep.errmis.len() < MAX_ERRMIS_STORED {
                            rep.errmis.push((rec.line, truncate_sql(&rec.sql, 120)));
                        }
                    }
                    (Err(e), false) => {
                        let cats = categories(&rec.sql, &e);
                        *rep.unsupported.entry(cats[0]).or_default() += 1;
                        for c in &cats {
                            *rep.co_counts.entry(c).or_default() += 1;
                        }
                        failed_writes += 1;
                        rep.sample(cats[0], rec.line, &rec.sql, &e);
                    }
                }
            }
            Kind::Query { types, sort, expected } => {
                rep.total += 1;
                if rec.skip {
                    rep.skipped += 1;
                    continue;
                }
                // Queries never create/drop, but must see dropped tables as
                // missing and get the SELECT shims.
                let toks = scan(&rec.sql);
                if shim.over_cap_referenced(&toks).is_some() {
                    *rep.unsupported.entry("engine-table-cap").or_default() += 1;
                    *rep.co_counts.entry("engine-table-cap").or_default() += 1;
                    continue;
                }
                if let Some(missing) = shim.missing_table_referenced(&toks) {
                    let cats = categories(&rec.sql, "");
                    let _ = missing;
                    *rep.unsupported.entry(cats[0]).or_default() += 1;
                    for c in &cats {
                        *rep.co_counts.entry(c).or_default() += 1;
                    }
                    continue;
                }
                let sql = shim_select(&rec.sql, &shim);
                let rows = match exec_sql(&db, &sql) {
                    Ok(ExecResult::Rows { rows, .. }) => rows,
                    Ok(_) => {
                        *rep.unsupported.entry("other").or_default() += 1;
                        *rep.co_counts.entry("other").or_default() += 1;
                        rep.sample("other", rec.line, &rec.sql, "query returned no row set");
                        continue;
                    }
                    Err(e) => {
                        let cats = categories(&rec.sql, &e);
                        *rep.unsupported.entry(cats[0]).or_default() += 1;
                        for c in &cats {
                            *rep.co_counts.entry(c).or_default() += 1;
                        }
                        rep.sample(cats[0], rec.line, &rec.sql, &e);
                        continue;
                    }
                };
                // Render per typestring.
                let mut arity_note = None;
                let mut rendered: Vec<Vec<String>> = Vec::with_capacity(rows.len());
                for row in &rows {
                    if row.len() != types.len() && arity_note.is_none() {
                        arity_note = Some(format!(
                            "column count differs: typestring {} vs {} returned",
                            types.len(),
                            row.len()
                        ));
                    }
                    let mut r = Vec::with_capacity(row.len());
                    for (i, v) in row.iter().enumerate() {
                        let tc = types.as_bytes().get(i).copied().unwrap_or(b'T');
                        r.push(render_value(v, tc));
                    }
                    rendered.push(r);
                }
                if let Some(note) = arity_note {
                    rep.wrong_total += 1;
                    if rep.wrong.len() < MAX_WRONG_STORED {
                        rep.wrong.push(Wrong {
                            line: rec.line,
                            sql: truncate_sql(&rec.sql, 200),
                            detail: note,
                            failed_writes_before: failed_writes,
                        });
                    }
                    continue;
                }
                let values: Vec<String> = match sort {
                    SortMode::No => rendered.into_iter().flatten().collect(),
                    SortMode::Row => {
                        let mut r = rendered;
                        r.sort();
                        r.into_iter().flatten().collect()
                    }
                    SortMode::Value => {
                        let mut v: Vec<String> =
                            rendered.into_iter().flatten().collect();
                        v.sort();
                        v
                    }
                };
                match expected {
                    Expected::Hash { count, md5 } => {
                        let mut buf = String::new();
                        for v in &values {
                            buf.push_str(v);
                            buf.push('\n');
                        }
                        let got_md5 = md5_hex(buf.as_bytes());
                        if values.len() != *count || got_md5 != *md5 {
                            rep.wrong_total += 1;
                            if rep.wrong.len() < MAX_WRONG_STORED {
                                let preview: Vec<&str> =
                                    values.iter().take(10).map(|s| s.as_str()).collect();
                                rep.wrong.push(Wrong {
                                    line: rec.line,
                                    sql: truncate_sql(&rec.sql, 200),
                                    detail: format!(
                                        "expected {count} values md5={md5}; got {} values md5={got_md5} (first: [{}])",
                                        values.len(),
                                        preview.join(", ")
                                    ),
                                    failed_writes_before: failed_writes,
                                });
                            }
                        } else {
                            rep.query_pass += 1;
                            rep.hash_verified += 1;
                        }
                    }
                    Expected::Literal(exp) => {
                        if &values == exp {
                            rep.query_pass += 1;
                        } else {
                            rep.wrong_total += 1;
                            if rep.wrong.len() < MAX_WRONG_STORED {
                                rep.wrong.push(Wrong {
                                    line: rec.line,
                                    sql: truncate_sql(&rec.sql, 200),
                                    detail: format!(
                                        "expected [{}] got [{}]",
                                        preview_vals(exp),
                                        preview_vals(&values)
                                    ),
                                    failed_writes_before: failed_writes,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    if let Err(e) = db.verify() {
        rep.verify_failed = Some(e.to_string());
    }
    rep
}

fn preview_vals(v: &[String]) -> String {
    let shown: Vec<&str> = v.iter().take(16).map(|s| s.as_str()).collect();
    if v.len() > 16 {
        format!("{} … ({} total)", shown.join(", "), v.len())
    } else {
        shown.join(", ")
    }
}

/// Compile detached (never touches the shared plan registry — the corpus has
/// ~10k distinct statements per file) and execute. Panics inside the engine
/// are caught and surfaced loudly: an engine panic is a real bug finding.
fn exec_sql(db: &Database, sql: &str) -> Result<ExecResult, String> {
    let db = std::panic::AssertUnwindSafe(db);
    let sql_owned = sql.to_string();
    std::panic::catch_unwind(move || {
        // Live DDL (CREATE INDEX, ALTER, …) is not a plannable statement — the
        // facade applies it on the `query()` path, so route it there. The
        // detached prepare/execute path (which compiles to a plan) is used for
        // everything else.
        let first = sql_owned
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(first.as_str(), "CREATE" | "DROP" | "ALTER") {
            return db.query(&sql_owned, &[]).map_err(|e| e.to_string());
        }
        let plan = db.prepare_detached(&sql_owned).map_err(|e| e.to_string())?;
        db.execute_detached(&plan, &[]).map_err(|e| e.to_string())
    })
    .unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        Err(format!("ENGINE PANIC: {msg}"))
    })
}

fn main() {
    md5_self_test();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let as_sqlite = args.iter().any(|a| a == "--as-sqlite");
    let sample_all = args.iter().any(|a| a == "--samples-all");
    args.retain(|a| a != "--as-sqlite" && a != "--samples-all");
    SAMPLE_ALL.store(sample_all, std::sync::atomic::Ordering::Relaxed);
    let engines: &[&str] = if as_sqlite { &["mpedb", "sqlite"] } else { &["mpedb"] };
    if args.is_empty() {
        eprintln!("usage: sqlite_corpus [--as-sqlite] [--samples-all] <file.test> [...]");
        std::process::exit(2);
    }
    let mut reports = Vec::new();
    for a in &args {
        let start = std::time::Instant::now();
        let rep = run_file(Path::new(a), engines);
        eprintln!(
            "ran {} ({} records) in {:.1}s",
            rep.name,
            rep.total,
            start.elapsed().as_secs_f64()
        );
        reports.push(rep);
    }

    // ---------------- per-file table ----------------
    println!();
    println!(
        "{:<28} {:>7} {:>7} {:>6} {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
        "file", "records", "pass", "pass%", "s-pass", "q-pass", "unsupp", "wrong", "errmis", "skipped"
    );
    let (mut t_total, mut t_pass, mut t_unsupp, mut t_wrong, mut t_errmis, mut t_skip) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for r in &reports {
        if let Some(f) = &r.fatal {
            println!("{:<28} FATAL: {f}", r.name);
            continue;
        }
        let run = r.total - r.skipped;
        let pct = if run > 0 {
            100.0 * r.pass() as f64 / run as f64
        } else {
            0.0
        };
        println!(
            "{:<28} {:>7} {:>7} {:>5.1}% {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
            r.name,
            r.total,
            r.pass(),
            pct,
            r.stmt_pass,
            r.query_pass,
            r.unsupported_total(),
            r.wrong_total,
            r.errmis_total,
            r.skipped
        );
        t_total += r.total;
        t_pass += r.pass();
        t_unsupp += r.unsupported_total();
        t_wrong += r.wrong_total;
        t_errmis += r.errmis_total;
        t_skip += r.skipped;
    }
    let t_run = t_total - t_skip;
    let (t_spass, t_qpass) = reports
        .iter()
        .fold((0usize, 0usize), |(s, q), r| (s + r.stmt_pass, q + r.query_pass));
    println!(
        "{:<28} {:>7} {:>7} {:>5.1}% {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
        "TOTAL",
        t_total,
        t_pass,
        if t_run > 0 { 100.0 * t_pass as f64 / t_run as f64 } else { 0.0 },
        t_spass,
        t_qpass,
        t_unsupp,
        t_wrong,
        t_errmis,
        t_skip
    );

    // ---------------- unsupported categories per file ----------------
    println!("\n== unsupported categories (primary attribution) ==");
    for r in &reports {
        if r.fatal.is_some() || r.unsupported.is_empty() {
            continue;
        }
        let cats: Vec<String> = r
            .unsupported
            .iter()
            .map(|(c, n)| format!("{c}={n}"))
            .collect();
        println!("{:<28} {}", r.name, cats.join(" "));
    }

    // ---------------- aggregate blocker ranking ----------------
    let mut agg: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &reports {
        for (c, n) in &r.co_counts {
            *agg.entry(c).or_default() += n;
        }
    }
    let mut ranked: Vec<(&str, usize)> = agg.into_iter().collect();
    ranked.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    println!("\n== blocked-statement counts by feature (co-occurrence, not primary-only) ==");
    for (c, n) in &ranked {
        println!("{n:>7}  {c}");
    }

    // ---------------- hash-verification note ----------------
    let hv: usize = reports.iter().map(|r| r.hash_verified).sum();
    println!("\nqueries verified via md5 hash: {hv}");

    // ---------------- engine verify failures ----------------
    for r in &reports {
        if let Some(v) = &r.verify_failed {
            println!("\n!!! ENGINE VERIFY FAILED after {}: {v}", r.name);
        }
    }

    // ---------------- wrong results ----------------
    println!("\n== WRONG RESULTS (query accepted, answer differs) ==");
    let mut any = false;
    for r in &reports {
        for w in &r.wrong {
            any = true;
            println!("\n--- {}:{}", r.name, w.line);
            println!("  sql: {}", w.sql);
            println!("  {}", w.detail);
            if w.failed_writes_before > 0 {
                println!(
                    "  NOTE: {} expected-ok statement(s) had already failed — state may have \
                     diverged from sqlite (possible cascade, not necessarily an answer bug)",
                    w.failed_writes_before
                );
            }
        }
        if r.wrong_total > r.wrong.len() {
            println!(
                "  ({}: {} further wrong results not shown)",
                r.name,
                r.wrong_total - r.wrong.len()
            );
        }
    }
    if !any {
        println!("(none)");
    }

    // ---------------- error mismatches ----------------
    println!("\n== ERROR MISMATCHES (sqlite expects an error, mpedb succeeds) ==");
    any = false;
    for r in &reports {
        for (line, sql) in &r.errmis {
            any = true;
            println!("{}:{}  {}", r.name, line, sql);
        }
        if r.errmis_total > r.errmis.len() {
            println!(
                "  ({}: {} further error mismatches not shown)",
                r.name,
                r.errmis_total - r.errmis.len()
            );
        }
    }
    if !any {
        println!("(none)");
    }

    // ---------------- other-error samples ----------------
    println!("\n== failing-statement samples ==");
    any = false;
    for r in &reports {
        for (cat, samples) in &r.other_samples {
            for (line, sql, err) in samples {
                any = true;
                println!("[{cat}] {}:{}  {}\n    -> {}", r.name, line, sql, err);
            }
        }
    }
    if !any {
        println!("(none)");
    }
}
