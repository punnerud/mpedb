//! Byte-level `||` (plan §8, PLAN_FORMAT 70): sqlite's concat operates on
//! BYTES — a blob operand contributes its raw bytes, and the bytes RECOMBINE
//! across the whole chain. mpedb's `Text` is valid-UTF-8 by construction, so
//! the chain evaluates in ONE n-ary opcode's stack frame (`ConcatN`) and only
//! a VALID result ever becomes a `Value`; an invalid one is the decode error
//! whose Display is CPython's own message shape, byte-for-byte (measured
//! against CPython's generator, U+FFFD rendering included).
//!
//! Value rows are differential against the sqlite3 CLI (blob literals X'..'
//! — the CLI cannot bind parameters); error rows pin the measured message.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open() -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-concatblob-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i32).to_string(),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Both engines answer `query` after `setup`; rows must match.
fn agree(setup: &[&str], query: &str) {
    let t = open();
    let mut script = String::new();
    for s in setup {
        t.db.query(s, &[]).unwrap();
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    let got = match t.db.query(query, &[]).unwrap_or_else(|e| panic!("mpedb `{query}`: {e}")) {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    let want: Vec<Vec<String>> = sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want, "`{query}`");
}

/// The recombination rows sqlite answers — matched exactly.
#[test]
fn blob_concat_bytes_recombine_like_sqlite() {
    const T: &[&str] = &["CREATE TABLE t (b BLOB, s TEXT)", "INSERT INTO t VALUES (X'FA', 'ok')"];
    for q in [
        // Split UTF-8 recombines across operands — the reason the chain is ONE opcode.
        "SELECT 7, X'C3' || X'A9'",
        "SELECT 7, 'x' || X'C3' || X'A9'",
        "SELECT 7, typeof('a' || X'C3A9'), 'a' || X'C3A9' IS 'aé'",
        // blob||blob with valid bytes is TEXT.
        "SELECT 7, typeof(X'6162' || X'6364'), X'6162' || X'6364'",
        // NULL propagates before any byte is looked at.
        "SELECT 7, NULL || X'FA' IS NULL",
        // A stored typeless cell carrying text passes through the n-ary path.
        "SELECT 7, s || 'x' FROM t",
    ] {
        agree(T, q);
    }
}

/// The decode error: CPython's message, built from the OUTPUT column's name
/// and the LOSSY rendering of the whole chain's bytes.
#[test]
fn the_decode_error_speaks_cpython() {
    let t = open();
    let e = t
        .db
        .query("SELECT 'xxx' || X'FA' || 'yyy' AS colname", &[])
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "Could not decode to UTF-8 column 'colname' with text 'xxx\u{FFFD}yyy'"
    );
    // Two invalid bytes render as TWO replacement chars (measured).
    let e = t.db.query("SELECT 'ab' || X'FFFE' AS w", &[]).unwrap_err().to_string();
    assert_eq!(e, "Could not decode to UTF-8 column 'w' with text 'ab\u{FFFD}\u{FFFD}'");
    // The named narrowings (stock answers; mpedb refuses — bytes never cross
    // a value boundary): a consumer of the invalid intermediate, and a split
    // over a subquery.
    assert!(t.db.query("SELECT hex('a' || X'FA')", &[]).is_err());
    assert!(t.db.query("SELECT (SELECT 'x' || X'C3') || X'A9'", &[]).is_err());
    // Blob params flow through the chain (the CPython shape) — via the API.
    let e = t
        .db
        .query("SELECT 'a' || $1 AS z", &[Value::Blob(vec![0xFA])])
        .unwrap_err()
        .to_string();
    assert_eq!(e, "Could not decode to UTF-8 column 'z' with text 'a\u{FFFD}'");
    let got = t.db.query("SELECT $1 || $2", &[Value::Blob(vec![0xC3]), Value::Blob(vec![0xA9])]);
    match got.unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Text("é".into())),
        other => panic!("{other:?}"),
    }
}
