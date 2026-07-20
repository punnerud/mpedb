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
//!   without a column list is rewritten to name the declared columns, so the
//!   PK is omitted and the engine's rowid-alias auto-assign (max+1, sqlite's
//!   own rule) numbers the rows — a shim-side counter cannot stay in sync
//!   once a passthrough `INSERT ... SELECT` copies real rowids past it.
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
        poisoned: false,
    })
}

/// mpedb caps the number of user tables per database at `MAX_TABLES - 8` (4096
/// total minus an 8-slot system reserve = 4088). Tables beyond the cap are left
/// out of the schema and every statement touching them is counted under
/// `engine-table-cap` instead of polluting the other categories. As of the
/// sparse-footprint change (PLAN_FORMAT 42, design/DESIGN-TABLE-CAP.md) this is
/// 4088 — no corpus file comes near it, so the category is now a pure backstop.
const ENGINE_TABLE_CAP: usize = 4088;

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

    /// `INSERT INTO t VALUES (..),(..)` → name the declared columns so the
    /// synthetic `rowid_` PK is OMITTED and the engine's rowid-alias
    /// auto-assign (max(rowid)+1 per row, sqlite's own non-AUTOINCREMENT rule)
    /// numbers the rows. The shim used to simulate rowids with a per-table
    /// counter, but a passthrough `INSERT … SELECT` copies REAL rowids the
    /// counter never saw — the next VALUES insert then collided on the PK
    /// (in1.test t4n/t7n). Statements that already name a column list cannot
    /// name `rowid_` (`parse_create_table` refuses tables that use the name),
    /// so they pass through and auto-assign the same way.
    fn shim_insert(&self, sql: &str, toks: &[Tok]) -> PreparedSql {
        if toks.len() < 4 || toks[1].up != "INTO" || !toks[2].is_word {
            return PreparedSql::Run(sql.to_string());
        }
        let table_raw = sql[toks[2].start..toks[2].end].to_string();
        let Some(idx) = self.find(&table_raw) else {
            return PreparedSql::Run(sql.to_string());
        };
        if toks[3].up != "VALUES" {
            // Explicit column list, INSERT … SELECT, DEFAULT VALUES, …
            return PreparedSql::Run(sql.to_string());
        }
        let values_text = &sql[toks[3].end..];
        let cols = self.tables[idx]
            .cols
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
            .join(", ");
        PreparedSql::Run(format!("INSERT INTO {table_raw} ({cols}) VALUES{values_text}"))
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

/// Set by `--join-cells N`: an explicit `[runtime] max_join_cells` for the
/// generated config (`JOIN_CELLS_SET` distinguishes "flag absent" from an
/// explicit 0 = unlimited).
static JOIN_CELLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static JOIN_CELLS_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    if JOIN_CELLS_SET.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = write!(
            cfg,
            "\n[runtime]\nmax_join_cells = {}\n",
            JOIN_CELLS.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
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

// ============================================================ footprint census
//
// `--footprint-census[=out.tsv]` (task #117). The corpus is the only REAL
// statement stream this repo has at scale, and the question it answers is a
// single number: **how many distinct plans map to how many distinct
// footprints?** A footprint cannot be a plan identity (it is not unique), but
// it can be an INDEX over plans — and the plans-per-footprint ratio is exactly
// the fan-out of that index. Near 1:1 means the shape key buys nothing (a
// cost history keyed by shape is a plan history with extra steps); a high
// ratio means one measured shape covers many plans.
//
// The census recompiles each statement through `Database::plan_footprint` —
// the same compile path `prepare`/`execute` take, so the footprint recorded is
// the one the executed statement carries (including the MPEE solver's row-count
// bucket, which is why this is done live on the running database and not in a
// prepare-only pass). It is OFF by default and costs one extra compile per
// statement when on.
mod census {
    use mpedb::{Database, Footprint, TableSet};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    /// Refuse to grow past this many distinct footprints / plans (a 3 GB
    /// ulimit is the house rule); the overflow is counted and reported so a
    /// truncated census can never be mistaken for a complete one.
    const MAX_TRACKED: usize = 4_000_000;

    #[derive(Default)]
    pub struct Census {
        /// occurrences (statements successfully compiled)
        pub occurrences: u64,
        /// statements that did not compile (unsupported surface; not counted)
        pub uncompilable: u64,
        /// fp key -> (occurrences, distinct plan keys under it)
        fps: HashMap<u64, (u64, HashSet<u64>)>,
        plans: HashSet<u64>,
        overflow: u64,
        /// canonical-encoding bytes summed over OCCURRENCES and over DISTINCT
        /// footprints, current encoding vs the #115 delta/varint form.
        pub bytes_occ: u64,
        pub bytes_occ_delta: u64,
        /// |tables_read| + |tables_written| histogram, capped at 8+.
        pub width_hist: [u64; 9],
        /// One exemplar encoding per distinct footprint, for `--footprint-census=out`.
        exemplars: HashMap<u64, Vec<u8>>,
        /// Per plan: (n, sum ns, sum ns^2) — the WITHIN-plan spread is the
        /// irreducible term (same plan, different data).
        plan_cost: HashMap<u64, (u64, f64, f64)>,
        /// fp key -> the plan keys under it, for the across-plan spread.
        fp_plans: HashMap<u64, HashSet<u64>>,
        /// The REORDER-INVARIANT key: the table sets + read_only, WITHOUT
        /// `indexes_used` / `key_access`. A join reorder never changes which
        /// tables a statement touches, but it does change the access paths
        /// chosen — so `indexes_used` (a bitmap OR'd from the chosen
        /// `AccessPath`s, `planner/footprint.rs:access_key_and_indexes`) moves
        /// under a reorder and the table sets do not. If a plan-variant family
        /// (MPEE ping-pong, DESIGN-MPEE-SOLVER §9.6) is to share one index
        /// bucket, THIS is the key it has to be.
        tsets: HashMap<u64, HashSet<u64>>,
    }

    static CENSUS: Mutex<Option<Census>> = Mutex::new(None);
    static OUT: Mutex<Option<String>> = Mutex::new(None);

    pub fn enable(out: Option<String>) {
        *CENSUS.lock().unwrap() = Some(Census::default());
        *OUT.lock().unwrap() = out;
    }

    fn fnv(bytes: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Length of a `TableSet` under the #115 mechanism: a varint count, the
    /// first id as a varint, then each subsequent id as a varint DELTA (the
    /// gaps are what a sorted set makes small; a dense run 5,6,7,8 costs
    /// 1 byte per element instead of 4).
    fn delta_len(ts: &TableSet) -> usize {
        fn varint(mut v: u64) -> usize {
            let mut n = 1;
            while v >= 0x80 {
                v >>= 7;
                n += 1;
            }
            n
        }
        let ids = ts.as_slice();
        let mut n = varint(ids.len() as u64);
        let mut prev = 0u32;
        for (i, &id) in ids.iter().enumerate() {
            n += varint(if i == 0 { id as u64 } else { (id - prev - 1) as u64 });
            prev = id;
        }
        n
    }

    /// Everything in a footprint EXCEPT the two table sets, under the current
    /// encoding: the u64 index bitmap, the read_only byte and the key access.
    /// (`indexes_used` is a per-table bitmap, not a sorted set — the #115
    /// mechanism does not apply to it, so it is charged unchanged to both
    /// arms and the comparison isolates the table sets.)
    fn tail_len(fp: &Footprint) -> usize {
        let mut a = Vec::new();
        fp.encode_into(&mut a);
        let mut r = Vec::new();
        fp.tables_read.encode_into(&mut r);
        let mut w = Vec::new();
        fp.tables_written.encode_into(&mut w);
        a.len() - r.len() - w.len()
    }

    /// Compile `sql` and record its (plan, footprint) pair. Returns the pair
    /// of interned keys so the caller can attribute the statement's EXECUTION
    /// cost to them (`record_cost`) — the measurement #88 actually needs.
    pub fn observe(db: &Database, sql: &str) -> Option<(u64, u64)> {
        let mut guard = CENSUS.lock().unwrap();
        let c = guard.as_mut()?;
        let (hash, fp) = match db.plan_footprint(sql) {
            Ok(v) => v,
            Err(_) => {
                c.uncompilable += 1;
                return None;
            }
        };
        let mut enc = Vec::new();
        fp.encode_into(&mut enc);
        let fk = fnv(&enc);
        let pk = u64::from_le_bytes(hash.0[..8].try_into().unwrap());

        c.occurrences += 1;
        c.bytes_occ += enc.len() as u64;
        c.bytes_occ_delta +=
            (tail_len(&fp) + delta_len(&fp.tables_read) + delta_len(&fp.tables_written)) as u64;
        let width = (fp.tables_read.len() + fp.tables_written.len()).min(8);
        c.width_hist[width] += 1;

        if c.fps.len() >= MAX_TRACKED && !c.fps.contains_key(&fk) {
            c.overflow += 1;
            return None;
        }
        c.plans.insert(pk);
        let slot = c.fps.entry(fk).or_default();
        slot.0 += 1;
        slot.1.insert(pk);
        if c.exemplars.len() < MAX_TRACKED {
            c.exemplars.entry(fk).or_insert(enc);
        }
        c.fp_plans.entry(fk).or_default().insert(pk);
        let mut tenc = Vec::new();
        fp.tables_read.encode_into(&mut tenc);
        fp.tables_written.encode_into(&mut tenc);
        tenc.push(fp.read_only as u8);
        c.tsets.entry(fnv(&tenc)).or_default().insert(pk);
        Some((fk, pk))
    }

    /// Attribute one execution's wall time to its plan.
    pub fn record_cost(key: Option<(u64, u64)>, ns: f64) {
        let Some((_fk, pk)) = key else { return };
        let mut guard = CENSUS.lock().unwrap();
        let Some(c) = guard.as_mut() else { return };
        let e = c.plan_cost.entry(pk).or_insert((0, 0.0, 0.0));
        e.0 += 1;
        e.1 += ns;
        e.2 += ns * ns;
    }

    /// Print the census and, if `--footprint-census=<path>` was given, write
    /// one line per distinct footprint: `occurrences TAB plans TAB hex`.
    /// That file is the input the microbench replays, so the conflict and
    /// routing measurements run on REAL footprints, not synthetic ones.
    pub fn report() {
        let guard = CENSUS.lock().unwrap();
        let Some(c) = guard.as_ref() else { return };
        println!("\n=== footprint census (task #117) ===");
        println!("compiled statements      {}", c.occurrences);
        println!("uncompilable (skipped)   {}", c.uncompilable);
        println!("distinct plans           {}", c.plans.len());
        println!("distinct footprints      {}", c.fps.len());
        if c.overflow > 0 {
            println!("TRUNCATED: {} occurrences past the tracking cap", c.overflow);
        }
        let fpn = c.fps.len().max(1) as f64;
        println!(
            "plans per footprint      mean {:.2}",
            c.plans.len() as f64 / fpn
        );
        let mut fan: Vec<usize> = c.fps.values().map(|(_, p)| p.len()).collect();
        fan.sort_unstable();
        let pick = |q: f64| fan.get(((fan.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0);
        println!(
            "                         p50 {} p90 {} p99 {} max {}",
            pick(0.50),
            pick(0.90),
            pick(0.99),
            fan.last().copied().unwrap_or(0)
        );
        let singleton = fan.iter().filter(|&&n| n == 1).count();
        println!(
            "footprints with exactly one plan: {singleton} ({:.1} %)",
            100.0 * singleton as f64 / fpn
        );
        // Occurrence-weighted: what a #88 history actually stores.
        let mut occ: Vec<(u64, usize)> = c.fps.values().map(|(o, p)| (*o, p.len())).collect();
        occ.sort_unstable();
        let tot_occ: u64 = occ.iter().map(|(o, _)| o).sum();
        println!(
            "occurrence-weighted mean plans/footprint {:.2}",
            occ.iter().map(|(o, p)| *o as f64 * *p as f64).sum::<f64>() / tot_occ.max(1) as f64
        );
        println!(
            "footprint bytes over occurrences: current {} delta {} ({:+.1} %)",
            c.bytes_occ,
            c.bytes_occ_delta,
            100.0 * (c.bytes_occ_delta as f64 - c.bytes_occ as f64) / c.bytes_occ.max(1) as f64
        );
        print!("|tables_read|+|tables_written| hist:");
        for (i, n) in c.width_hist.iter().enumerate() {
            if *n > 0 {
                print!(" {i}{}={n}", if i == 8 { "+" } else { "" });
            }
        }
        println!();
        let tn = c.tsets.len().max(1) as f64;
        println!(
            "distinct TABLE-SET keys   {}  (plans per table set: mean {:.2})",
            c.tsets.len(),
            c.plans.len() as f64 / tn
        );
        let mut tfan: Vec<usize> = c.tsets.values().map(|p| p.len()).collect();
        tfan.sort_unstable();
        println!(
            "                          p50 {} p90 {} max {}",
            tfan.get(tfan.len() / 2).copied().unwrap_or(0),
            tfan.get(tfan.len() * 9 / 10).copied().unwrap_or(0),
            tfan.last().copied().unwrap_or(0)
        );
        println!(
            "footprint refines the table set by {:.2}x (distinct fps / distinct table sets)",
            c.fps.len() as f64 / tn
        );
        cost_report(c);
        if let Some(path) = OUT.lock().unwrap().as_ref() {
            let mut out = String::new();
            for (fk, (occ, plans)) in &c.fps {
                let hex: String = c
                    .exemplars
                    .get(fk)
                    .map(|e| e.iter().map(|b| format!("{b:02x}")).collect())
                    .unwrap_or_default();
                out.push_str(&format!("{occ}\t{}\t{hex}\n", plans.len()));
            }
            match std::fs::write(path, out) {
                Ok(()) => println!("wrote {} distinct footprints to {path}", c.fps.len()),
                Err(e) => println!("could not write {path}: {e}"),
            }
        }
    }

    /// The question #88 actually has to answer: if a cost history is keyed by
    /// SHAPE, how wrong is a measurement borrowed from another plan in the
    /// same bucket? Two spreads, both as coefficients of variation:
    ///
    /// - **within-plan**: the same plan re-executed. This is irreducible —
    ///   data dependence and timer noise. It is the floor.
    /// - **across-plan, within-footprint**: the per-plan MEAN costs of the
    ///   plans that share one footprint. This is the error a shape key
    ///   introduces on top of the floor.
    ///
    /// If the second is not much larger than the first, the shape key is a
    /// legitimate place to pool measurements. If it is far larger, the bucket
    /// is conflating plans whose costs differ, and #88 must key on something
    /// finer.
    fn cost_report(c: &Census) {
        let cv = |n: u64, sum: f64, sq: f64| -> Option<f64> {
            if n < 2 {
                return None;
            }
            let n = n as f64;
            let mean = sum / n;
            let var = (sq / n - mean * mean).max(0.0);
            if mean <= 0.0 {
                None
            } else {
                Some(var.sqrt() / mean)
            }
        };
        let mut within: Vec<f64> = Vec::new();
        for (n, sum, sq) in c.plan_cost.values() {
            if let Some(v) = cv(*n, *sum, *sq) {
                within.push(v);
            }
        }
        let mut across: Vec<f64> = Vec::new();
        let mut across_ratio: Vec<f64> = Vec::new();
        for plans in c.fp_plans.values() {
            let means: Vec<f64> = plans
                .iter()
                .filter_map(|p| c.plan_cost.get(p))
                .filter(|(n, _, _)| *n > 0)
                .map(|(n, sum, _)| sum / *n as f64)
                .collect();
            if means.len() < 2 {
                continue;
            }
            let n = means.len() as f64;
            let m = means.iter().sum::<f64>() / n;
            let var = means.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n;
            if m > 0.0 {
                across.push(var.sqrt() / m);
            }
            let (lo, hi) = means.iter().fold((f64::MAX, 0.0f64), |(a, b), &x| {
                (a.min(x), b.max(x))
            });
            if lo > 0.0 {
                across_ratio.push(hi / lo);
            }
        }
        let med = |v: &mut Vec<f64>| -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let n_within = within.len();
        let n_across = across.len();
        println!("\n-- cost spread (feeds #88) --");
        println!(
            "within-plan CV (irreducible)          median {:.3}  over {n_within} plans",
            med(&mut within)
        );
        println!(
            "across-plan-within-footprint CV       median {:.3}  over {n_across} footprints",
            med(&mut across)
        );
        println!(
            "worst/best plan mean cost in a bucket median {:.1}x  p90 {:.1}x  max {:.1}x",
            med(&mut across_ratio),
            across_ratio
                .get((across_ratio.len().saturating_sub(1)) * 9 / 10)
                .copied()
                .unwrap_or(f64::NAN),
            across_ratio.last().copied().unwrap_or(f64::NAN)
        );
    }
}


// ====================================================== index-candidate census
//
// `--index-census[=out.tsv]` (task #118). The sibling of the #117 footprint
// census, and the same reuse of the same statement stream: the corpus is the
// only REAL SQL workload this repo has at scale, and the question #118 needs
// answered before any of its design is worth building is a single number —
// **how many DISTINCT (table, key column set, predicate) index candidates does
// a real workload generate?** If it is thousands, workload-derived indexing is
// a search problem; if it is dozens, it is an enumeration.
//
// The candidate is derived from the SAME compiled plan `execute` runs, not from
// the SQL text: the plan's `AccessPath` says which columns the planner already
// pinned by equality (those conjuncts are CONSUMED out of `filter` by
// `planner::access::extract_access`, so text analysis would miss exactly the
// columns that matter), and the residual `filter` — a stack-based `ExprProgram`
// — carries every remaining conjunct with its column ordinals intact.
//
// Three candidate FAMILIES are counted, because the corpus is literal-heavy
// (#117 §1: 1.17 statements per distinct plan, i.e. essentially unparameterized)
// while a real ORM binds parameters, and the two disagree about exactly one
// thing — whether a `col = <value>` conjunct is a *predicate* (a constant of the
// application) or an *argument* (a parameter of the query):
//
//   W      (table, key cols)                      — parameterization-INVARIANT.
//   Pnull  W + `IS [NOT] NULL` conjuncts          — survives parameterization:
//                                                   an ORM emits IS NULL as text.
//   Plit   Pnull + `= <const>` / `IN (<consts>)`  — the UPPER bound; a real
//                                                   parameterized app reaches
//                                                   this only for true constants.
//
// OFF by default; one extra compile per statement when on.
mod idx_census {
    use mpedb::Database;
    use mpedb_sql::{AccessPath, CompiledPlan, ExprProgram, Instr, OrderOver, PlanStmt, Schema};
    use mpedb_types::CmpKind;
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::Mutex;

    /// Longest key column list a candidate may carry. Beyond this an index is
    /// not a real proposal, and the corpus never gets close.
    const MAX_KEY: usize = 8;

    #[derive(Default)]
    pub struct IdxCensus {
        /// statements that compiled through `mpedb_sql::prepare`
        pub compiled: u64,
        /// statements that did not compile in the prepare-only pass
        pub uncompilable: u64,
        /// compiled, but the shape carries no single-table candidate
        /// (join / compound / recursive / derived / INSERT / txn control)
        pub skipped_shape: u64,
        /// single-table shape whose predicate pinned no column at all
        pub no_key: u64,
        /// filter contained a jump (CASE / COALESCE): conjunct split refused
        pub opaque_filter: u64,
        /// occurrences per candidate, per family
        w: HashMap<String, u64>,
        pnull: HashMap<String, u64>,
        plit: HashMap<String, u64>,
        /// distinct predicates seen (non-empty only)
        pnull_preds: BTreeSet<String>,
        plit_preds: BTreeSet<String>,
        /// candidates already served by an existing index / PK prefix
        pub served: u64,
        pub novel: u64,
        /// W candidates keyed by table, for the per-table search-space size
        per_table: BTreeMap<String, BTreeSet<String>>,
        /// how the access path resolved
        pub acc_pk_point: u64,
        pub acc_pk_range: u64,
        pub acc_ix_point: u64,
        pub acc_ix_range: u64,
        pub acc_full: u64,
        /// key-column-count histogram (capped 8+)
        pub key_hist: [u64; 9],
        /// conjuncts whose RHS was a parameter rather than a constant — the
        /// class a partial index can never be probed by without a runtime
        /// access-path choice.
        pub param_rhs: u64,
        pub const_rhs: u64,
    }

    static CENSUS: Mutex<Option<IdxCensus>> = Mutex::new(None);
    static OUT: Mutex<Option<String>> = Mutex::new(None);

    pub fn enable(out: Option<String>) {
        *CENSUS.lock().unwrap() = Some(IdxCensus::default());
        *OUT.lock().unwrap() = out;
    }

    // ---- expression decomposition ----------------------------------------

    /// One sub-expression of a program: the instruction that produced it and
    /// the argument nodes it consumed.
    struct Node {
        instr: usize,
        kids: Vec<usize>,
    }

    /// Number of stack slots an instruction pops. `None` = an opcode this
    /// census does not model (every jump, and anything added later) — the
    /// caller then treats the whole filter as opaque rather than guessing.
    fn pops(i: &Instr) -> Option<usize> {
        use Instr::*;
        Some(match i {
            PushCol(_) | PushParam(_) | PushConst(_) => 0,
            Neg | Not | IsNull | IsNotNull | ToFloat | Cast(_) | Like(_) | LikeCs(_)
            | LikeEsc(..) | LikeCsEsc(..) | Glob(_) | Regexp(_) | InParam(_) | Affinity(_)
            | BitNot => 1,
            Eq | Ne | Lt | Le | Gt | Ge | Add | Sub | Mul | Div | Mod | And | Or
            | IsNotDistinct | IsDistinct | Concat | BitAnd | BitOr | Shl | Shr | CmpColl(..)
            | CmpClass(..) | LikeDyn | LikeCsDyn | GlobDyn | RegexpDyn | LikeDynEsc(_)
            | LikeCsDynEsc(_) => 2,
            InList(n) | InListColl(n, _) => *n as usize + 1,
            Call(_, argc) => *argc as usize,
            HostCall(_, argc) => *argc as usize,
            _ => return None,
        })
    }

    /// Forward symbolic-stack walk producing one [`Node`] per sub-expression.
    /// Returns `None` when any instruction is unmodelled (a jump, i.e. CASE /
    /// COALESCE, whose control flow a linear walk cannot follow).
    fn decompose(p: &ExprProgram) -> Option<(Vec<Node>, usize)> {
        let mut nodes: Vec<Node> = Vec::with_capacity(p.instrs.len());
        let mut stack: Vec<usize> = Vec::new();
        for (pc, ins) in p.instrs.iter().enumerate() {
            let n = pops(ins)?;
            if stack.len() < n {
                return None;
            }
            let kids = stack.split_off(stack.len() - n);
            nodes.push(Node { instr: pc, kids });
            stack.push(nodes.len() - 1);
        }
        let root = *stack.last()?;
        if stack.len() != 1 {
            return None;
        }
        Some((nodes, root))
    }

    /// Split a boolean root into top-level `AND` conjuncts.
    fn conjuncts(p: &ExprProgram, nodes: &[Node], root: usize, out: &mut Vec<usize>) {
        if matches!(p.instrs[nodes[root].instr], Instr::And) {
            let (a, b) = (nodes[root].kids[0], nodes[root].kids[1]);
            conjuncts(p, nodes, a, out);
            conjuncts(p, nodes, b, out);
        } else {
            out.push(root);
        }
    }

    enum Rhs {
        Const(usize),
        Param,
        Other,
    }

    fn leaf(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16) -> Rhs {
        if !nodes[n].kids.is_empty() {
            return Rhs::Other;
        }
        match p.instrs[nodes[n].instr] {
            Instr::PushConst(k) => Rhs::Const(k as usize),
            // A slot at or past `n_user` is a lifted-subquery / session-context
            // result, not a caller parameter: neither a constant nor an
            // argument an advisor may reason about.
            Instr::PushParam(k) if k < n_user => Rhs::Param,
            _ => Rhs::Other,
        }
    }

    fn as_col(p: &ExprProgram, nodes: &[Node], n: usize) -> Option<u16> {
        if !nodes[n].kids.is_empty() {
            return None;
        }
        match p.instrs[nodes[n].instr] {
            Instr::PushCol(c) => Some(c),
            _ => None,
        }
    }

    /// What one conjunct contributes to a candidate.
    enum Conj {
        /// `col = <const|param>` — an index KEY column.
        Eq(u16, bool),
        /// `col <op> <const|param>` — a trailing key column (one only).
        Range(u16),
        /// `col IS [NOT] NULL` — a partial-index PREDICATE that survives
        /// parameterization.
        Null(u16, bool),
        /// `col IN (<all consts>)` — key column, and a literal predicate.
        InConst(u16),
        Other,
    }

    fn classify(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16, c: &mut IdxCensus) -> Conj {
        use Instr::*;
        let root = &p.instrs[nodes[n].instr];
        let kids = &nodes[n].kids;
        // `col IS [NOT] NULL`
        if kids.len() == 1 {
            if let Some(col) = as_col(p, nodes, kids[0]) {
                match root {
                    IsNull => return Conj::Null(col, true),
                    IsNotNull => return Conj::Null(col, false),
                    _ => {}
                }
            }
        }
        // `col IN (e1..en)` with every element a constant
        if let InList(k) | InListColl(k, _) = root {
            let k = *k as usize;
            if kids.len() == k + 1 {
                if let Some(col) = as_col(p, nodes, kids[0]) {
                    if kids[1..]
                        .iter()
                        .all(|&e| matches!(leaf(p, nodes, e, n_user), Rhs::Const(_)))
                    {
                        return Conj::InConst(col);
                    }
                    return Conj::Eq(col, false);
                }
            }
            return Conj::Other;
        }
        // `col <cmp> <atom>` in either operand order
        if kids.len() == 2 {
            let eq = matches!(root, Eq | IsNotDistinct)
                || matches!(root, CmpColl(k, _) | CmpClass(k, _) if *k == CmpKind::Eq);
            let rng = matches!(root, Lt | Le | Gt | Ge)
                || matches!(root, CmpColl(k, _) | CmpClass(k, _)
                    if matches!(k, CmpKind::Lt | CmpKind::Le
                                 | CmpKind::Gt | CmpKind::Ge));
            if eq || rng {
                for (a, b) in [(kids[0], kids[1]), (kids[1], kids[0])] {
                    if let Some(col) = as_col(p, nodes, a) {
                        match leaf(p, nodes, b, n_user) {
                            Rhs::Const(_) => {
                                c.const_rhs += 1;
                                return if eq { Conj::Eq(col, true) } else { Conj::Range(col) };
                            }
                            Rhs::Param => {
                                c.param_rhs += 1;
                                return if eq { Conj::Eq(col, false) } else { Conj::Range(col) };
                            }
                            Rhs::Other => {}
                        }
                    }
                }
            }
        }
        Conj::Other
    }

    // ---- the census ------------------------------------------------------

    fn render_const(p: &ExprProgram, k: usize) -> String {
        match p.consts.get(k) {
            Some(v) => format!("{v:?}"),
            None => "?".into(),
        }
    }

    /// Analyse one single-table statement and fold its candidate into the
    /// three families.
    #[allow(clippy::too_many_arguments)]
    fn fold(
        c: &mut IdxCensus,
        schema: &Schema,
        plan: &CompiledPlan,
        table: u32,
        access: &AccessPath,
        filter: Option<&ExprProgram>,
        order: &[u16],
    ) {
        let Some(t) = schema.tables.get(table as usize) else {
            c.skipped_shape += 1;
            return;
        };
        let name = |i: u16| -> String {
            t.columns
                .get(i as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("#{i}"))
        };

        // 1. Columns the ACCESS PATH already pinned (their conjuncts were
        //    consumed out of `filter` by the planner).
        let mut eq: Vec<u16> = Vec::new();
        let mut range: Option<u16> = None;
        match access {
            AccessPath::PkPoint(_) => {
                c.acc_pk_point += 1;
                eq.extend(t.primary_key.iter().copied());
            }
            AccessPath::PkRange { .. } => {
                c.acc_pk_range += 1;
                range = t.primary_key.first().copied();
            }
            AccessPath::IndexPoint { index_no, parts } => {
                c.acc_ix_point += 1;
                if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                    eq.extend(ix.columns.iter().take(parts.len()).copied());
                }
            }
            AccessPath::IndexRange { index_no, .. } => {
                c.acc_ix_range += 1;
                if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                    range = ix.columns.first().copied();
                }
            }
            AccessPath::FullScan => c.acc_full += 1,
            AccessPath::FtsScan { .. } => {
                c.skipped_shape += 1;
                return;
            }
        }

        // 2. Residual conjuncts.
        let mut nulls: Vec<(u16, bool)> = Vec::new();
        let mut lits: Vec<(u16, String)> = Vec::new();
        if let Some(f) = filter {
            let Some((nodes, root)) = decompose(f) else {
                c.opaque_filter += 1;
                return;
            };
            let mut cs = Vec::new();
            conjuncts(f, &nodes, root, &mut cs);
            for n in cs {
                match classify(f, &nodes, n, plan.n_user_params(), c) {
                    Conj::Eq(col, is_const) => {
                        if !eq.contains(&col) {
                            eq.push(col);
                        }
                        if is_const {
                            if let Some(&k) = nodes[n].kids.iter().find(|&&x| {
                                matches!(leaf(f, &nodes, x, plan.n_user_params()), Rhs::Const(_))
                            }) {
                                if let Rhs::Const(ci) = leaf(f, &nodes, k, plan.n_user_params()) {
                                    lits.push((col, format!("={}", render_const(f, ci))));
                                }
                            }
                        }
                    }
                    Conj::InConst(col) => {
                        if !eq.contains(&col) {
                            eq.push(col);
                        }
                        lits.push((col, format!("IN[{}]", nodes[n].kids.len() - 1)));
                    }
                    Conj::Range(col) => {
                        if range.is_none() && !eq.contains(&col) {
                            range = Some(col);
                        }
                    }
                    Conj::Null(col, isnull) => nulls.push((col, isnull)),
                    Conj::Other => {}
                }
            }
        }

        // 3. Key column list: equalities (sorted, canonical), then ONE range
        //    column, then the ORDER BY tail.
        eq.sort_unstable();
        eq.dedup();
        let mut key: Vec<u16> = eq.clone();
        if let Some(r) = range {
            if !key.contains(&r) {
                key.push(r);
            }
        }
        for &o in order {
            if key.len() >= MAX_KEY {
                break;
            }
            if !key.contains(&o) {
                key.push(o);
            }
        }
        key.truncate(MAX_KEY);
        if key.is_empty() {
            c.no_key += 1;
            return;
        }
        c.key_hist[key.len().min(8)] += 1;

        // 4. Is it already served? A candidate is served when some existing
        //    index (or the PK) has the candidate's key as a PREFIX of its key
        //    columns in the order the candidate needs, or when the candidate's
        //    equality set is a prefix-cover of one.
        let covers = |cols: &[u16]| -> bool {
            key.len() <= cols.len() && key.iter().zip(cols).all(|(a, b)| a == b)
        };
        let served = covers(&t.primary_key) || t.indexes.iter().any(|ix| covers(&ix.columns));
        if served {
            c.served += 1;
        } else {
            c.novel += 1;
        }

        // 5. Fold into the three families.
        let cols: Vec<String> = key.iter().map(|&i| name(i)).collect();
        let w = format!("{}({})", t.name, cols.join(","));
        *c.w.entry(w.clone()).or_default() += 1;
        c.per_table.entry(t.name.clone()).or_default().insert(w.clone());

        let inkey = |col: u16| key.contains(&col);
        let mut pn: Vec<String> = nulls
            .iter()
            .filter(|(col, _)| !inkey(*col))
            .map(|(col, isnull)| {
                format!("{} IS {}NULL", name(*col), if *isnull { "" } else { "NOT " })
            })
            .collect();
        pn.sort();
        pn.dedup();
        let mut pl = pn.clone();
        for (col, s) in &lits {
            if !inkey(*col) {
                pl.push(format!("{}{}", name(*col), s));
            }
        }
        pl.sort();
        pl.dedup();

        let key_pn = if pn.is_empty() {
            w.clone()
        } else {
            let p = pn.join(" AND ");
            c.pnull_preds.insert(p.clone());
            format!("{w} WHERE {p}")
        };
        let key_pl = if pl.is_empty() {
            w.clone()
        } else {
            let p = pl.join(" AND ");
            c.plit_preds.insert(p.clone());
            format!("{w} WHERE {p}")
        };
        *c.pnull.entry(key_pn).or_default() += 1;
        *c.plit.entry(key_pl).or_default() += 1;
    }

    /// Compile `sql` prepare-only and fold its index candidate.
    pub fn observe(db: &Database, sql: &str) {
        let mut guard = CENSUS.lock().unwrap();
        let Some(c) = guard.as_mut() else { return };
        let bundle = db.schema();
        let plan = match mpedb_sql::prepare(sql, &bundle.schema) {
            Ok(p) => p,
            Err(_) => {
                c.uncompilable += 1;
                return;
            }
        };
        c.compiled += 1;
        match &plan.stmt {
            PlanStmt::Select(sp) if sp.joins.is_empty() && sp.windows.is_empty() => {
                // ORDER BY extends the key ONLY when it sorts the base row and
                // the statement neither aggregates nor dedups — otherwise the
                // ordinals do not name base columns.
                let order: Vec<u16> = if sp.order_over == OrderOver::BaseRow
                    && sp.aggregate.is_none()
                    && !sp.distinct
                {
                    sp.order_by.iter().map(|(i, _, _)| *i).collect()
                } else {
                    Vec::new()
                };
                let table = sp.table;
                let access = sp.access.clone();
                let filter = sp.filter.clone();
                fold(c, &bundle.schema, &plan, table, &access, filter.as_ref(), &order);
            }
            PlanStmt::Update {
                table,
                access,
                filter,
                ..
            }
            | PlanStmt::Delete {
                table,
                access,
                filter,
                ..
            } => {
                let (t, a, f) = (*table, access.clone(), filter.clone());
                fold(c, &bundle.schema, &plan, t, &a, f.as_ref(), &[]);
            }
            _ => c.skipped_shape += 1,
        }
    }

    fn top(m: &HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = m.iter().map(|(k, &o)| (k.clone(), o)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    fn partial_count(m: &HashMap<String, u64>) -> (usize, u64) {
        let mut n = 0;
        let mut occ = 0;
        for (k, &o) in m {
            if k.contains(" WHERE ") {
                n += 1;
                occ += o;
            }
        }
        (n, occ)
    }

    pub fn report() {
        let guard = CENSUS.lock().unwrap();
        let Some(c) = guard.as_ref() else { return };
        println!("\n=== index-candidate census (task #118) ===");
        println!("compiled statements          {}", c.compiled);
        println!("uncompilable (prepare-only)  {}", c.uncompilable);
        println!("skipped (not single-table)   {}", c.skipped_shape);
        println!("opaque filter (CASE/jump)    {}", c.opaque_filter);
        println!("single-table, no key pinned  {}", c.no_key);
        let cand: u64 = c.w.values().sum();
        println!("statements yielding a candidate {cand}");
        println!(
            "access path: PkPoint {} PkRange {} IndexPoint {} IndexRange {} FullScan {}",
            c.acc_pk_point, c.acc_pk_range, c.acc_ix_point, c.acc_ix_range, c.acc_full
        );
        println!("candidate already served by an existing index/PK  {}", c.served);
        println!("candidate NOVEL (no index covers it)              {}", c.novel);
        println!(
            "comparison RHS: const {}  param {}",
            c.const_rhs, c.param_rhs
        );
        print!("key width histogram:");
        for (i, n) in c.key_hist.iter().enumerate() {
            if *n > 0 {
                print!(" {i}={n}");
            }
        }
        println!();

        for (label, m, preds) in [
            ("W     (table, key cols)", &c.w, None),
            ("Pnull (+ IS [NOT] NULL)", &c.pnull, Some(&c.pnull_preds)),
            ("Plit  (+ = const / IN)", &c.plit, Some(&c.plit_preds)),
        ] {
            let (np, occp) = partial_count(m);
            println!(
                "\n{label}: {} distinct candidates   partial {np} ({:.1}%)  occurrences under a partial {occp}",
                m.len(),
                if m.is_empty() { 0.0 } else { 100.0 * np as f64 / m.len() as f64 }
            );
            if let Some(p) = preds {
                println!("  distinct predicates: {}", p.len());
            }
            // coverage of the top-K candidate set
            let mut v: Vec<u64> = m.values().copied().collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            let total: u64 = v.iter().sum();
            for k in [1usize, 8, 32, 128] {
                let s: u64 = v.iter().take(k).sum();
                if total > 0 && k <= v.len() {
                    println!("  top {k:>3} candidates cover {:.1}% of occurrences", 100.0 * s as f64 / total as f64);
                }
            }
            for (name, occ) in top(m, 10) {
                println!("  {occ:>8}  {name}");
            }
        }

        println!("\nper-table candidate counts (distinct W per table):");
        let mut pt: Vec<(&String, usize)> = c.per_table.iter().map(|(k, v)| (k, v.len())).collect();
        pt.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (t, n) in pt.iter().take(15) {
            println!("  {n:>5}  {t}");
        }
        println!("  tables with candidates: {}", c.per_table.len());

        if let Some(path) = OUT.lock().unwrap().as_ref() {
            let mut s = String::new();
            for (k, occ) in top(&c.plit, usize::MAX) {
                s.push_str(&format!("{occ}\t{k}\n"));
            }
            let _ = std::fs::write(path, s);
            println!("wrote {} candidate lines to {path}", c.plit.len());
        }
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
        let ckey = census::observe(&db, &sql_owned);
        idx_census::observe(&db, &sql_owned);
        let plan = db.prepare_detached(&sql_owned).map_err(|e| e.to_string())?;
        let t0 = std::time::Instant::now();
        let out = db.execute_detached(&plan, &[]).map_err(|e| e.to_string());
        census::record_cost(ckey, t0.elapsed().as_nanos() as f64);
        out
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
    // `--join-cells N`: set `[runtime] max_join_cells` in the generated
    // config (0 = unlimited) — how the N-way-join battery (`select5.test`)
    // is probed with an explicit budget instead of the default.
    if let Some(i) = args.iter().position(|a| a == "--join-cells") {
        let v = args
            .get(i + 1)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                eprintln!("--join-cells needs an integer argument");
                std::process::exit(2);
            });
        JOIN_CELLS.store(v, std::sync::atomic::Ordering::Relaxed);
        JOIN_CELLS_SET.store(true, std::sync::atomic::Ordering::Relaxed);
        args.drain(i..=i + 1);
    }
    // `--footprint-census[=<out.tsv>]` (task #117): count distinct plans vs
    // distinct footprints over the real statement stream, and optionally dump
    // the distinct footprints so the microbench can replay them.
    if let Some(i) = args
        .iter()
        .position(|a| a == "--footprint-census" || a.starts_with("--footprint-census="))
    {
        let out = args[i].split_once('=').map(|(_, v)| v.to_owned());
        census::enable(out);
        args.remove(i);
    }
    // `--index-census[=<out.tsv>]` (task #118): how many DISTINCT (table, key
    // column set, predicate) index candidates does the real statement stream
    // generate, and how many of them are PARTIAL?
    if let Some(i) = args
        .iter()
        .position(|a| a == "--index-census" || a.starts_with("--index-census="))
    {
        let out = args[i].split_once('=').map(|(_, v)| v.to_owned());
        idx_census::enable(out);
        args.remove(i);
    }
    SAMPLE_ALL.store(sample_all, std::sync::atomic::Ordering::Relaxed);
    let engines: &[&str] = if as_sqlite { &["mpedb", "sqlite"] } else { &["mpedb"] };
    if args.is_empty() {
        eprintln!(
            "usage: sqlite_corpus [--as-sqlite] [--samples-all] [--join-cells N] \
             [--footprint-census[=out.tsv]] [--index-census[=out.tsv]] <file.test> [...]"
        );
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

    census::report();
    idx_census::report();
}
