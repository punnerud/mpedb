//! Runner for the classic **sqllogictest** file format — the engine-agnostic,
//! public-domain half of SQLite's test methodology (the format and corpus
//! approach of <https://sqlite.org/sqllogictest/>).
//!
//! # Supported directives
//!
//! - `statement ok` — the SQL that follows (until a blank line) must succeed.
//! - `statement error [substring]` — the SQL must fail. The optional
//!   substring (an mpedb extension; classic files leave it empty) must occur
//!   in the error's `Display` text.
//! - `query <typestring> [nosort|rowsort|valuesort] [label]` — the SQL
//!   (until the `----` line) must return rows; the rendered values must
//!   equal the expected lines (one value per line, row-major, until a blank
//!   line or EOF). Same-label queries in one file must agree with each other.
//! - `skipif <engine>` / `onlyif <engine>` — conditional records; this
//!   runner is engine `mpedb`.
//! - `halt` — stop processing the file (debugging aid).
//! - `hash-threshold <n>` — **parsed and ignored** (documented omission):
//!   the hashed expected-result form (`N values hashing to <md5>`) is NOT
//!   supported, only literal expected results. Encountering a hashed
//!   result block is an error, so the omission can never silently pass.
//!
//! # mpedb extensions
//!
//! - **`# schema:` header** — mpedb has no `CREATE TABLE`; the schema comes
//!   from TOML config. Each `.test` file opens with a comment block:
//!
//!   ```text
//!   # schema:
//!   # [[table]]
//!   # name = "t"
//!   # ...
//!   # end schema
//!   ```
//!
//!   The lines between `# schema:` and `# end schema` (with the leading
//!   `#`/`# ` stripped) are TOML `[[table]]` definitions; the runner
//!   supplies the `[database]` section itself (a fresh `.mpedb` file under
//!   /dev/shm, removed afterwards).
//!
//! # Value rendering (sqllogictest conventions)
//!
//! - NULL renders as `NULL` (any column type).
//! - `I` columns: int64/timestamp as decimal.
//! - `R` columns: float64 (or int64) as `%.3f` (three decimals).
//! - `T` columns: text verbatim (the empty string renders as `(empty)`),
//!   bool as `true`/`false`, blob as `x'<lowercase hex>'`.
//!
//! `EXPLAIN <stmt>` under a `query T` directive renders one output line per
//! plan line, so plans can be pinned as executable documentation.

use crate::{Failure, Result, TempDir};
use mpedb::{Config, Database, ExecResult, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// Counts from one file run.
#[derive(Debug, Default, Clone, Copy)]
pub struct SltStats {
    /// Executed records (statements + queries), not counting skipped ones.
    pub records: usize,
    pub statements: usize,
    pub queries: usize,
    /// Records skipped by `skipif`/`onlyif`.
    pub skipped: usize,
}

/// The engine name this runner answers to in `skipif`/`onlyif` lines.
const ENGINE: &str = "mpedb";

/// Run one `.test` file against a fresh mpedb database built from the file's
/// `# schema:` header. Returns statistics, or a self-contained failure
/// message (file, line, SQL, expected vs got) on the first mismatch.
pub fn run_slt_file(path: &Path) -> Result<SltStats> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Failure(format!("{}: cannot read: {e}", path.display())))?;
    let lines: Vec<&str> = text.lines().collect();

    let schema_toml = extract_schema(&lines)
        .ok_or_else(|| Failure(format!("{}: missing `# schema:` header block", path.display())))?;

    let dir = TempDir::new("slt")?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "slt".into());
    let config_toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n\n{}",
        dir.db_path(&stem).display(),
        schema_toml
    );
    let config = Config::from_toml_str(&config_toml)
        .map_err(|e| Failure(format!("{}: schema header rejected: {e}", path.display())))?;
    let db = Database::open_with_config(config)
        .map_err(|e| Failure(format!("{}: open failed: {e}", path.display())))?;

    let mut stats = SltStats::default();
    let mut labels: HashMap<String, Vec<String>> = HashMap::new();
    let mut parser = Lines {
        lines: &lines,
        next: 0,
    };

    while let Some((lineno, first)) = parser.next_directive() {
        let fail = |msg: String| Failure(format!("{}:{}: {msg}", path.display(), lineno));

        let mut skip = false;
        let mut head = first;
        // Conditional prefix lines apply to the record that follows.
        loop {
            let mut toks = head.split_whitespace();
            match toks.next() {
                Some("skipif") => {
                    if toks.next() == Some(ENGINE) {
                        skip = true;
                    }
                }
                Some("onlyif") => {
                    if toks.next() != Some(ENGINE) {
                        skip = true;
                    }
                }
                _ => break,
            }
            head = parser
                .next_directive()
                .ok_or_else(|| fail("dangling skipif/onlyif at end of file".into()))?
                .1;
        }

        let mut toks = head.split_whitespace();
        match toks.next() {
            Some("halt") => break,
            Some("hash-threshold") => {
                // Ignored: hashed results are unsupported (module docs); the
                // threshold only matters when the runner *produces* hashes.
                continue;
            }
            Some("statement") => {
                let mode = toks.next().ok_or_else(|| {
                    fail("statement directive needs `ok` or `error`".into())
                })?;
                let expect_err = match mode {
                    "ok" => None,
                    "error" => {
                        // Everything after the `error` token is the required
                        // error-message substring (mpedb extension).
                        let rest = head
                            .strip_prefix("statement")
                            .map(str::trim_start)
                            .and_then(|r| r.strip_prefix("error"))
                            .map(str::trim)
                            .unwrap_or("");
                        Some(rest.to_string())
                    }
                    other => return Err(fail(format!("unknown statement mode `{other}`"))),
                };
                let sql = parser.take_until_blank();
                if sql.is_empty() {
                    return Err(fail("statement directive with no SQL".into()));
                }
                if skip {
                    stats.skipped += 1;
                    continue;
                }
                run_statement(&db, &sql, expect_err.as_deref()).map_err(|m| {
                    fail(format!("statement failed\n  sql: {sql}\n  {m}"))
                })?;
                stats.statements += 1;
                stats.records += 1;
            }
            Some("query") => {
                let types = toks
                    .next()
                    .ok_or_else(|| fail("query directive needs a typestring".into()))?
                    .to_string();
                if !types.bytes().all(|b| matches!(b, b'I' | b'R' | b'T')) {
                    return Err(fail(format!("bad typestring `{types}` (I/R/T only)")));
                }
                let (sort, label) = match (toks.next(), toks.next()) {
                    (None, _) => (SortMode::No, None),
                    (Some(s), lab) => match s {
                        "nosort" => (SortMode::No, lab.map(str::to_string)),
                        "rowsort" => (SortMode::Row, lab.map(str::to_string)),
                        "valuesort" => (SortMode::Value, lab.map(str::to_string)),
                        // Classic format: a third token that is not a sort
                        // mode is the label.
                        other => (SortMode::No, Some(other.to_string())),
                    },
                };
                let (sql, expected) = parser.take_query_body().map_err(&fail)?;
                if skip {
                    stats.skipped += 1;
                    continue;
                }
                if expected
                    .first()
                    .is_some_and(|l| l.split_whitespace().nth(1) == Some("values")
                        && l.split_whitespace().nth(2) == Some("hashing"))
                {
                    return Err(fail(
                        "hashed expected results (`N values hashing to <md5>`) are not \
                         supported; write literal expected values"
                            .into(),
                    ));
                }
                let got = run_query(&db, &sql, &types, sort).map_err(|m| {
                    fail(format!("query failed\n  sql: {sql}\n  {m}"))
                })?;
                if got != expected {
                    return Err(fail(format!(
                        "query result mismatch\n  sql: {sql}\n{}",
                        render_diff(&expected, &got)
                    )));
                }
                if let Some(lab) = label {
                    // Same-label queries must agree (classic slt semantics).
                    if let Some(prev) = labels.get(&lab) {
                        if *prev != got {
                            return Err(fail(format!(
                                "label `{lab}` mismatch with an earlier query\n  sql: {sql}\n{}",
                                render_diff(prev, &got)
                            )));
                        }
                    } else {
                        labels.insert(lab, got);
                    }
                }
                stats.queries += 1;
                stats.records += 1;
            }
            Some(other) => return Err(fail(format!("unknown directive `{other}`"))),
            None => unreachable!("next_directive never yields blank lines"),
        }
    }

    db.verify()
        .map_err(|e| Failure(format!("{}: post-run verify failed: {e}", path.display())))?;
    Ok(stats)
}

// ---------------------------------------------------------------- execution

fn run_statement(db: &Database, sql: &str, expect_err: Option<&str>) -> Result<(), String> {
    let res = db.query(sql, &[]);
    match (res, expect_err) {
        (Ok(_), None) => Ok(()),
        (Err(e), None) => Err(format!("expected success, got error: {e}")),
        (Ok(r), Some(_)) => Err(format!("expected an error, statement succeeded: {r:?}")),
        (Err(e), Some(sub)) => {
            let msg = e.to_string();
            if sub.is_empty() || msg.contains(sub) {
                Ok(())
            } else {
                Err(format!(
                    "error text mismatch\n  expected substring: {sub}\n  actual error:       {msg}"
                ))
            }
        }
    }
}

/// `nosort` / `rowsort` / `valuesort` from the query directive.
#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    No,
    Row,
    Value,
}

/// Execute a query record; render per the typestring; apply the sort mode;
/// return one string per value (row-major).
fn run_query(
    db: &Database,
    sql: &str,
    types: &str,
    sort: SortMode,
) -> Result<Vec<String>, String> {
    let rows: Vec<Vec<String>> = match db.query(sql, &[]).map_err(|e| e.to_string())? {
        ExecResult::Rows { rows, .. } => {
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                if row.len() != types.len() {
                    return Err(format!(
                        "typestring `{types}` has {} columns, result has {}",
                        types.len(),
                        row.len()
                    ));
                }
                let mut r = Vec::with_capacity(row.len());
                for (v, t) in row.iter().zip(types.bytes()) {
                    r.push(render_value(v, t)?);
                }
                out.push(r);
            }
            out
        }
        // EXPLAIN: one text line per plan line (typestring must be `T`).
        ExecResult::Explain(text) => {
            if types != "T" {
                return Err("EXPLAIN output requires typestring `T`".into());
            }
            text.lines().map(|l| vec![l.to_string()]).collect()
        }
        ExecResult::Affected(n) => {
            return Err(format!(
                "statement affected {n} rows but returned none; use `statement ok`"
            ))
        }
    };

    let mut values: Vec<String> = match sort {
        SortMode::No => rows.into_iter().flatten().collect(),
        SortMode::Row => {
            let mut rows = rows;
            rows.sort();
            rows.into_iter().flatten().collect()
        }
        SortMode::Value => {
            let mut v: Vec<String> = rows.into_iter().flatten().collect();
            v.sort();
            v
        }
    };
    // Values are compared line-by-line; a trailing '\r' from a CRLF-edited
    // file must not fail the comparison.
    for v in &mut values {
        while v.ends_with('\r') {
            v.pop();
        }
    }
    Ok(values)
}

/// sqllogictest value rendering (module docs).
fn render_value(v: &Value, type_char: u8) -> Result<String, String> {
    if v.is_null() {
        return Ok("NULL".into());
    }
    match (type_char, v) {
        (b'I', Value::Int(x)) => Ok(x.to_string()),
        (b'I', Value::Timestamp(x)) => Ok(x.to_string()),
        (b'R', Value::Float(x)) => Ok(format!("{x:.3}")),
        (b'R', Value::Int(x)) => Ok(format!("{:.3}", *x as f64)),
        (b'T', Value::Text(s)) => Ok(if s.is_empty() {
            "(empty)".into()
        } else {
            s.clone()
        }),
        (b'T', Value::Bool(b)) => Ok(if *b { "true" } else { "false" }.into()),
        (b'T', Value::Blob(b)) => {
            let mut s = String::with_capacity(3 + b.len() * 2);
            s.push_str("x'");
            for byte in b {
                let _ = write!(s, "{byte:02x}");
            }
            s.push('\'');
            Ok(s)
        }
        (t, v) => Err(format!(
            "value {v} does not fit typestring column `{}`",
            t as char
        )),
    }
}

fn render_diff(expected: &[String], got: &[String]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "  expected ({} values):", expected.len());
    for e in expected {
        let _ = writeln!(s, "    {e}");
    }
    let _ = writeln!(s, "  got ({} values):", got.len());
    for g in got {
        let _ = writeln!(s, "    {g}");
    }
    s.pop();
    s
}

// ---------------------------------------------------------------- parsing

/// Line cursor over the file.
struct Lines<'a> {
    lines: &'a [&'a str],
    next: usize,
}

impl<'a> Lines<'a> {
    /// Advance to the next non-blank, non-comment line; returns
    /// (1-based line number, trimmed line).
    fn next_directive(&mut self) -> Option<(usize, &'a str)> {
        while self.next < self.lines.len() {
            let raw = self.lines[self.next].trim_end();
            self.next += 1;
            let t = raw.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            return Some((self.next, t));
        }
        None
    }

    /// Collect SQL lines until a blank line or EOF (comments skipped).
    fn take_until_blank(&mut self) -> String {
        let mut sql = Vec::new();
        while self.next < self.lines.len() {
            let raw = self.lines[self.next].trim_end();
            if raw.trim().is_empty() {
                break;
            }
            self.next += 1;
            if raw.trim_start().starts_with('#') {
                continue;
            }
            sql.push(raw);
        }
        sql.join("\n")
    }

    /// Collect a query record body: SQL until the `----` line, then expected
    /// value lines until a blank line or EOF (kept verbatim; a comment
    /// cannot start inside expected results).
    fn take_query_body(&mut self) -> Result<(String, Vec<String>), String> {
        let mut sql = Vec::new();
        let mut saw_sep = false;
        while self.next < self.lines.len() {
            let raw = self.lines[self.next].trim_end();
            self.next += 1;
            if raw.trim() == "----" {
                saw_sep = true;
                break;
            }
            if raw.trim().is_empty() {
                return Err("query record ended before `----` separator".into());
            }
            if raw.trim_start().starts_with('#') {
                continue;
            }
            sql.push(raw);
        }
        if !saw_sep {
            return Err("query record missing `----` separator".into());
        }
        if sql.is_empty() {
            return Err("query directive with no SQL".into());
        }
        let mut expected = Vec::new();
        while self.next < self.lines.len() {
            let raw = self.lines[self.next].trim_end();
            if raw.trim().is_empty() {
                break;
            }
            self.next += 1;
            expected.push(raw.to_string());
        }
        Ok((sql.join("\n"), expected))
    }
}

/// Extract the TOML `[[table]]` block from the `# schema:` header.
fn extract_schema(lines: &[&str]) -> Option<String> {
    let start = lines.iter().position(|l| l.trim() == "# schema:")?;
    let mut toml = String::new();
    for raw in &lines[start + 1..] {
        let t = raw.trim_end();
        if t.trim() == "# end schema" {
            return Some(toml);
        }
        let body = t.strip_prefix('#')?;
        let body = body.strip_prefix(' ').unwrap_or(body);
        toml.push_str(body);
        toml.push('\n');
    }
    None // unterminated block
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_test_file(name: &str, contents: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new("slt-unit").unwrap();
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        (dir, p)
    }

    const HEADER: &str = "\
# schema:
# [[table]]
# name = \"t\"
# primary_key = [\"id\"]
#   [[table.column]]
#   name = \"id\"
#   type = \"int64\"
#   [[table.column]]
#   name = \"v\"
#   type = \"text\"
# end schema
";

    #[test]
    fn minimal_file_runs() {
        let body = format!(
            "{HEADER}
statement ok
INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b')

query IT rowsort
SELECT id, v FROM t
----
1
a
2
b

statement error PRIMARY KEY
INSERT INTO t (id, v) VALUES (1, 'dup')
"
        );
        let (_dir, p) = write_test_file("mini.test", &body);
        let stats = run_slt_file(&p).unwrap();
        assert_eq!(stats.statements, 2);
        assert_eq!(stats.queries, 1);
        assert_eq!(stats.records, 3);
    }

    #[test]
    fn mismatch_is_reported_with_context() {
        let body = format!(
            "{HEADER}
query IT nosort
SELECT id, v FROM t
----
1
ghost
"
        );
        let (_dir, p) = write_test_file("bad.test", &body);
        let err = run_slt_file(&p).unwrap_err().to_string();
        assert!(err.contains("mismatch"), "got: {err}");
        assert!(err.contains("ghost"), "got: {err}");
        assert!(err.contains("bad.test:13"), "got: {err}");
    }

    #[test]
    fn skipif_onlyif_and_halt() {
        let body = format!(
            "{HEADER}
skipif mpedb
statement ok
THIS IS NOT VALID SQL

onlyif sqlite
statement ok
NEITHER IS THIS

onlyif mpedb
statement ok
INSERT INTO t (id, v) VALUES (1, 'a')

halt

statement ok
GARBAGE AFTER HALT IS NEVER REACHED
"
        );
        let (_dir, p) = write_test_file("cond.test", &body);
        let stats = run_slt_file(&p).unwrap();
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.statements, 1);
    }

    #[test]
    fn hashed_results_are_a_documented_error() {
        let body = format!(
            "{HEADER}
hash-threshold 8

query I nosort
SELECT id FROM t
----
30 values hashing to 3c13dee48d9356ae19af2515e05e6b54
"
        );
        let (_dir, p) = write_test_file("hash.test", &body);
        let err = run_slt_file(&p).unwrap_err().to_string();
        assert!(err.contains("not"), "got: {err}");
        assert!(err.contains("hashing"), "got: {err}");
    }

    #[test]
    fn error_substring_must_match() {
        let body = format!(
            "{HEADER}
statement error CHECK
INSERT INTO t (id, v) VALUES (1, 2)
"
        );
        let (_dir, p) = write_test_file("sub.test", &body);
        let err = run_slt_file(&p).unwrap_err().to_string();
        assert!(err.contains("error text mismatch"), "got: {err}");
    }

    #[test]
    fn render_value_conventions() {
        assert_eq!(render_value(&Value::Null, b'I').unwrap(), "NULL");
        assert_eq!(render_value(&Value::Int(-7), b'I').unwrap(), "-7");
        assert_eq!(render_value(&Value::Float(2.0), b'R').unwrap(), "2.000");
        assert_eq!(render_value(&Value::Float(-0.125), b'R').unwrap(), "-0.125");
        assert_eq!(render_value(&Value::Int(3), b'R').unwrap(), "3.000");
        assert_eq!(render_value(&Value::Text("x y".into()), b'T').unwrap(), "x y");
        assert_eq!(render_value(&Value::Text(String::new()), b'T').unwrap(), "(empty)");
        assert_eq!(render_value(&Value::Bool(true), b'T').unwrap(), "true");
        assert_eq!(
            render_value(&Value::Blob(vec![0, 255]), b'T').unwrap(),
            "x'00ff'"
        );
        assert_eq!(render_value(&Value::Timestamp(5), b'I').unwrap(), "5");
        assert!(render_value(&Value::Text("x".into()), b'I').is_err());
    }
}
