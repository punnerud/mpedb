//! sqlite_corpus — prototype (task #62): run a subset of SQLite's public
//! **sqllogictest corpus** (canonical format, e.g. the gregrahn/sqllogictest
//! mirror) against mpedb and produce a compatibility report.
//!
//! Unlike the curated `tests/slt/*.test` runner in `src/slt.rs` (which uses an
//! mpedb SLT dialect with a `# schema:` header), this binary consumes the
//! *canonical* corpus files unmodified — and, since 2026-07-29, executes them
//! unmodified too. The database is opened with a ZERO-table seed and the
//! corpus's own `CREATE TABLE` / `CREATE INDEX` / `DROP TABLE` run for real.
//!
//! It did not always. Until #47 (live DDL) and #94 (implicit rowid) shipped,
//! the runner had to manufacture a TOML schema by pre-scanning the file, give
//! every table a hidden `rowid_` primary key, rewrite every `INSERT` to skip
//! that key, expand `SELECT *` so it never leaked, and simulate `DROP TABLE`
//! as `DELETE FROM`. That shim was the single largest source of "failures" in
//! the whole corpus: **1 365 of 1 391 recorded refusals were its own doing**,
//! along with the 4 records flagged as wrong answers, which were cascades from
//! four `REPLACE INTO` statements its INSERT rewriter did not recognise.
//! Deleting it moved the corpus from 99,9765 % to **99,99955 %** with **zero**
//! wrong answers, and every remaining failure is now a named engine gap.
//!
//! One rewrite survives: **`SELECT ALL`** → `SELECT` (mpedb has no ALL
//! quantifier). That is a real surface gap, not a schema workaround.
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
//! `--size-mb N` sets pre-reserved file size (default **32**; was 128). Each
//! `.test` opens a fresh DB and `fallocate`s `size_mb`. 16 MiB is too small for
//! some `index/*` files (`database is out of space`); 32 covers the 621-file
//! flist. Override upward for outliers (e.g. `select5`).
//! `--verify` runs `Database::verify()` after each file (off by default —
//! full page-accounting walk; correctness is already the SLT expect/md5 path).
//! `--jobs N` runs up to N files in parallel (default 1). Corpus files are
//! independent; this is the main wall-clock lever vs sqlite/minisqlite (each
//! file is a full mpedb open + unique-SQL compile, which is slower per stmt
//! than C sqlite but parallelizes cleanly).
//!
//! Known runner limitations (also see the final report). The first four of the
//! old six were the shim's and went with it; what is left is about RENDERING,
//! not about the schema:
//! - Float rendering uses Rust `{:.3}`, which matches C `%.3f` for f64 in
//!   practice but is not bit-for-bit proven; `T`-typed floats use a `%.1f`
//!   approximation of sqlite's text rendering.

use mpedb_testkit::corpus_baseline as baseline;

use mpedb::{Config, Database, ExecResult, Value};


use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

mod census;
mod idx_census;
mod runner;

use runner::run_file;

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

// ====================================================== statement handling

/// The corpus statement, as it will be handed to the engine.
///
/// This used to be a whole schema shim: a pre-scanned table list, a hidden
/// `rowid_` primary key per table, an `exists` flag so `DROP TABLE` could be
/// faked as `DELETE FROM`, an `INSERT` rewriter that named the declared columns
/// so the synthetic key was skipped, a `SELECT *` expander so it never leaked,
/// and a `PreparedSql` enum with three non-engine outcomes (`Done`, `SimError`,
/// `Unsupported`) for the statements none of that could express.
///
/// All of it existed because mpedb had no `CREATE TABLE` and no implicit rowid
/// when the runner was written. Both shipped (#47, #94), and the lifetime table
/// cap went 64 → 4096 while the heaviest corpus file declares 955. So the
/// corpus's own DDL is now simply executed, and the measured failures the shim
/// was itself causing — 1 289 index accumulations (a faked `DROP TABLE` left
/// every `CREATE INDEX` piled on one live table until the 32-index cap), 72
/// star-arity refusals (`SELECT *` expansion could not reach into a subquery),
/// 4 unrecognised `REPLACE INTO`, and the 4 "wrong answers" that cascaded from
/// those four — are gone, because the thing causing them is gone.
///
/// One rewrite survives: `SELECT ALL` → `SELECT`. That is a real (tiny) surface
/// gap, not a schema workaround.
fn prepare_statement(sql: &str) -> String {
    let toks = scan(sql);
    if !toks.is_empty() && toks[0].up == "SELECT" {
        return strip_select_all(sql);
    }
    sql.to_string()
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

/// Words that end a FROM clause at depth 0.
const CLAUSE_KEYWORDS: [&str; 7] =
    ["WHERE", "GROUP", "ORDER", "LIMIT", "HAVING", "OFFSET", "UNION"];

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
    // `shim-star-arity` and `shim-index-accumulation` used to live here. Both
    // named the RUNNER's own damage — a `SELECT *` expander that could not see
    // into a subquery, and a faked `DROP TABLE` that let indexes pile onto one
    // live table until the 32-index cap. The runner now executes the corpus's
    // DDL, so neither error can be produced and neither category can occur.
    // They are gone rather than kept at zero: a category that cannot fire is a
    // reader's trap, not a measurement.
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
/// Pre-reserved DB size per corpus file. Default 16 MiB (not 128): open cost is
/// paid once per `.test` and is pure harness overhead, not an engine regression
/// signal. See module docs for `--size-mb`.
static SIZE_MB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(32);
/// When true, run `Database::verify()` after each file (expensive; off by default).
static DO_VERIFY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Compile once and execute without registry publish and without the
/// detached encode→decode re-validation tax ([`Database::query_once`]).
/// Panics inside the engine are caught and surfaced loudly.
fn exec_sql(db: &Database, sql: &str) -> Result<ExecResult, String> {
    let db = std::panic::AssertUnwindSafe(db);
    let sql_owned = sql.to_string();
    std::panic::catch_unwind(move || {
        let ckey = census::observe(&db, &sql_owned);
        idx_census::observe(&db, &sql_owned);
        let t0 = std::time::Instant::now();
        // query_once: one compile + run_plan(None). Avoids prepare_detached's
        // encode + execute_detached's full decode/schema re-check (meant for
        // untrusted client-borne blobs, not same-process one-shots). DDL is
        // handled inside query_once the same way as query().
        let out = db.query_once(&sql_owned, &[]).map_err(|e| e.to_string());
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
    let do_verify = args.iter().any(|a| a == "--verify");
    args.retain(|a| a != "--as-sqlite" && a != "--samples-all" && a != "--verify");
    DO_VERIFY.store(do_verify, std::sync::atomic::Ordering::Relaxed);
    // `--size-mb N`: pre-reserved file size (default 32).
    if let Some(i) = args.iter().position(|a| a == "--size-mb") {
        let v = args
            .get(i + 1)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                eprintln!("--size-mb needs an integer argument (MiB, >=1)");
                std::process::exit(2);
            });
        if v < 1 {
            eprintln!("--size-mb must be >= 1");
            std::process::exit(2);
        }
        SIZE_MB.store(v, std::sync::atomic::Ordering::Relaxed);
        args.drain(i..=i + 1);
    }
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
    // `--baseline <path>` / `--write-baseline <path>`: the expected-counts gate.
    // Reading it makes a silently shifting category an EXIT CODE; writing it is
    // the deliberate act of saying "this is what the tree does now".
    let mut baseline_path: Option<String> = None;
    let mut write_baseline: Option<String> = None;
    let mut corpus_root: Option<String> = None;
    for (flag, slot) in [
        ("--baseline", &mut baseline_path),
        ("--write-baseline", &mut write_baseline),
        ("--corpus-root", &mut corpus_root),
    ] {
        if let Some(i) = args.iter().position(|a| a == flag) {
            let v = args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("{flag} needs a path argument");
                std::process::exit(2);
            });
            *slot = Some(v);
            args.drain(i..=i + 1);
        }
    }
    SAMPLE_ALL.store(sample_all, std::sync::atomic::Ordering::Relaxed);
    let mut jobs: usize = 1;
    if let Some(i) = args.iter().position(|a| a == "--jobs") {
        jobs = args
            .get(i + 1)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                eprintln!("--jobs needs an integer argument (>=1)");
                std::process::exit(2);
            });
        if jobs < 1 {
            eprintln!("--jobs must be >= 1");
            std::process::exit(2);
        }
        args.drain(i..=i + 1);
    }
    let engines: &[&str] = if as_sqlite { &["mpedb", "sqlite"] } else { &["mpedb"] };
    if args.is_empty() {
        eprintln!(
            "usage: sqlite_corpus [--as-sqlite] [--samples-all] [--verify] [--size-mb N] \
             [--jobs N] [--join-cells N] [--footprint-census[=out.tsv]] \
             [--index-census[=out.tsv]] [--baseline <t.tsv>] [--write-baseline <t.tsv>] \
             [--corpus-root <dir>] <file.test> [...]\n\
             \n\
             --baseline exits 0 on an exact match, 1 on a regression, 3 when the \
             baseline is stale (only improvements)."
        );
        std::process::exit(2);
    }
    let wall0 = std::time::Instant::now();
    let reports = if jobs <= 1 {
        let mut reports = Vec::with_capacity(args.len());
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
        reports
    } else {
        // Independent files → parallel workers. Preserve input order in output.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;
        let paths = &args;
        let next = AtomicUsize::new(0);
        let out: Mutex<Vec<(usize, FileReport)>> = Mutex::new(Vec::with_capacity(paths.len()));
        let n_workers = jobs.min(paths.len());
        std::thread::scope(|scope| {
            for _ in 0..n_workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= paths.len() {
                            break;
                        }
                        let start = std::time::Instant::now();
                        let rep = run_file(Path::new(&paths[i]), engines);
                        eprintln!(
                            "ran {} ({} records) in {:.1}s",
                            rep.name,
                            rep.total,
                            start.elapsed().as_secs_f64()
                        );
                        out.lock().unwrap().push((i, rep));
                    }
                });
            }
        });
        let mut pairs = out.into_inner().unwrap();
        pairs.sort_by_key(|(i, _)| *i);
        pairs.into_iter().map(|(_, r)| r).collect()
    };
    eprintln!(
        "wall_clock {:.1}s jobs={jobs} files={}",
        wall0.elapsed().as_secs_f64(),
        args.len()
    );

    // ---------------- per-file table ----------------
    //
    // Rows are labelled by KEY (the path with the run's common root stripped),
    // not by file name: 127 of the corpus's files share a basename — there are
    // thirteen `slt_good_0.test` — so a name-labelled table attributed a failure
    // to whichever of them the reader guessed.
    // The root the keys are relative to. A baseline being COMPARED supplies it
    // — a subset run must key itself the way the full run that wrote the
    // baseline did, and inferring it from this run's own paths does not.
    let recorded = baseline_path
        .as_ref()
        .map(|p| baseline::read(Path::new(p)))
        .transpose()
        .unwrap_or_else(|e| {
            eprintln!("cannot read baseline: {e}");
            std::process::exit(2);
        });
    let root = corpus_root
        .clone()
        .or_else(|| recorded.as_ref().map(|b| b.root.clone()))
        .unwrap_or_else(|| baseline::infer_root(&args));
    let keys = baseline::keys_for(&args, &root).unwrap_or_else(|e| {
        eprintln!("{e}\n(pass --corpus-root to say which directory the keys are relative to)");
        std::process::exit(2);
    });
    let mut base_table = baseline::Table::new();
    println!();
    println!(
        "{:<44} {:>7} {:>7} {:>6} {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
        "file", "records", "pass", "pass%", "s-pass", "q-pass", "unsupp", "wrong", "errmis", "skipped"
    );
    let (mut t_total, mut t_pass, mut t_unsupp, mut t_wrong, mut t_errmis, mut t_skip) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for (i, r) in reports.iter().enumerate() {
        let key = keys.get(i).cloned().unwrap_or_else(|| r.name.clone());
        if let Some(f) = &r.fatal {
            println!("{:<44} FATAL: {f}", key);
            base_table.insert(
                key,
                baseline::Row {
                    total: r.total,
                    pass: 0,
                    unsupported: 0,
                    wrong: 0,
                    errmis: 0,
                    skipped: 0,
                    fatal: true,
                },
            );
            continue;
        }
        let run = r.total - r.skipped;
        let pct = if run > 0 {
            100.0 * r.pass() as f64 / run as f64
        } else {
            0.0
        };
        println!(
            "{:<44} {:>7} {:>7} {:>5.1}% {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
            key,
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
        base_table.insert(
            key,
            baseline::Row {
                total: r.total,
                pass: r.pass(),
                unsupported: r.unsupported_total(),
                wrong: r.wrong_total,
                errmis: r.errmis_total,
                skipped: r.skipped,
                fatal: false,
            },
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
        "{:<44} {:>7} {:>7} {:>5.1}% {:>7} {:>7} {:>7} {:>6} {:>7} {:>7}",
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

    // ---------------- the baseline gate ----------------
    //
    // Last, so the whole report is still printed on a regression — the diff
    // says WHICH file moved, and the reader wants the categories next to it.
    if let Some(p) = &write_baseline {
        let out = baseline::Baseline { root: root.clone(), files: base_table.clone() };
        match baseline::write(Path::new(p), &out) {
            Ok(()) => println!(
                "\nbaseline written: {p} ({} files, root {root})",
                base_table.len()
            ),
            Err(e) => {
                eprintln!("cannot write baseline {p}: {e}");
                std::process::exit(2);
            }
        }
    }
    if let Some(base) = &recorded {
        let code = baseline::compare(&base.files, &base_table).exit_code();
        if code != 0 {
            std::process::exit(code);
        }
    }
}
