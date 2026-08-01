//! sqlite's DOUBLE-QUOTED STRING misfeature (#132), differentially against the
//! bundled sqlite CLI.
//!
//! The rule, MEASURED on 3.45.1 and pinned here row by row: a DOUBLE-quoted
//! identifier in EXPRESSION position that binds to NO column is a string
//! LITERAL. Everything narrower stays an error, exactly as in sqlite: an
//! AMBIGUOUS name, a qualified `"t"."c"`, a backtick/bracket spelling, an
//! INSERT column list, an UPDATE SET target. PostgreSQL-dialect databases
//! never fall back at all.
//!
//! Why mpedb adopted a misfeature sqlite itself regrets: Django's table
//! rebuild emits SQL naming a column it has just renamed away, and it works
//! on stock only because DQS turns the dangler into a literal over an empty
//! table (`schema.test_autofield_to_o2o`). Refusing was more honest but
//! answered with an error where the oracle answers with rows.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

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
        "{dir}/mpedb-dqs-{}-{}.mpedb",
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

/// Run `setup` + `query` in both engines; rows must match exactly.
fn agree(setup: &[&str], query: &str) {
    let t = open();
    let mut script = String::new();
    for s in setup {
        t.db.query(s, &[]).unwrap_or_else(|e| panic!("mpedb `{s}`: {e}"));
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
    let want: Vec<Vec<String>> = sqlite_oracle::try_script_stdout(&script, "")
        .unwrap_or_else(|e| panic!("sqlite refused `{query}`: {e}"))
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want, "`{query}`");
}

/// Both engines refuse.
fn both_refuse(setup: &[&str], stmt: &str) {
    let t = open();
    let mut script = String::new();
    for s in setup {
        t.db.query(s, &[]).unwrap();
        script.push_str(s);
        script.push_str(";\n");
    }
    assert!(t.db.query(stmt, &[]).is_err(), "mpedb accepted `{stmt}`");
    let mut probe = script;
    probe.push_str(stmt);
    probe.push_str(";\n");
    assert!(
        sqlite_oracle::try_script_stdout(&probe, "").is_err(),
        "sqlite accepted `{stmt}`"
    );
}

const SETUP: &[&str] = &[
    "CREATE TABLE t (a TEXT)",
    "CREATE TABLE u (a TEXT, b TEXT)",
    "INSERT INTO t VALUES ('x')",
];

/// The fallback rows: every expression position, resolving vs not.
#[test]
fn a_double_quoted_unknown_is_a_string_literal_where_sqlite_says_so() {
    for q in [
        // Unknown -> literal; resolving -> column; both at once.
        "SELECT \"nope\" FROM t",
        "SELECT \"a\" FROM t",
        "SELECT \"nope\" || \"a\" FROM t",
        // WHERE / CASE / IN / function argument / typeof.
        "SELECT 'lit' WHERE \"boo\" = 'boo'",
        "SELECT CASE WHEN \"cw\" = 'cw' THEN 1 ELSE 0 END FROM t",
        "SELECT \"nope\" IN ('nope') FROM t",
        "SELECT length(\"fn_arg\") FROM t",
        "SELECT typeof(\"nope\") FROM t",
    ] {
        agree(SETUP, q);
    }
    // INSERT VALUES and UPDATE SET rhs are expression positions too.
    agree(
        &[
            "CREATE TABLE t (a TEXT)",
            "INSERT INTO t VALUES (\"dqval\")",
            "UPDATE t SET a = \"unres\"",
        ],
        "SELECT a FROM t",
    );
}

/// The refusals that MUST stay refusals — each one measured in sqlite.
#[test]
fn the_dqs_boundaries_hold() {
    // Ambiguous: an error, never a literal.
    both_refuse(
        &[SETUP[0], SETUP[1], "INSERT INTO u VALUES ('p','q')"],
        "SELECT \"a\" FROM t JOIN u ON 1 = 1",
    );
    // Qualified: no fallback.
    both_refuse(SETUP, "SELECT \"t\".\"nope\" FROM t");
    // Backtick and bracket: identifier spellings without the misfeature.
    both_refuse(SETUP, "SELECT `tick` FROM t");
    both_refuse(SETUP, "SELECT [brack] FROM t");
    // INSERT column list and UPDATE SET target are NOT expression position.
    both_refuse(SETUP, "INSERT INTO t (\"nope\") VALUES ('1')");
    both_refuse(SETUP, "UPDATE t SET \"nope\" = '1'");
}
