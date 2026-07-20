//! `GENERATED ALWAYS AS (<expr>) [STORED|VIRTUAL]` — differential against the
//! BUNDLED sqlite oracle (3.45.0).
//!
//! mpedb materializes BOTH kinds into the row; the STORED/VIRTUAL tag is
//! declared metadata that only `PRAGMA table_xinfo.hidden` and the
//! `ALTER TABLE ADD COLUMN` rule read. That is a storage divergence, never a
//! value one — a generated expression may reference only same-row columns and
//! must be deterministic, so computing it at write time and computing it at read
//! time cannot disagree. Every test here checks the VALUE (and `typeof()`, so a
//! coincidentally-equal rendering of a different type cannot pass).
//!
//! The deliberate narrowings, each a refusal and each pinned below:
//! - a generated column may not reference a generated column declared at or
//!   after it (sqlite resolves forward references; mpedb refuses, which is what
//!   makes its single declaration-order evaluation pass provably correct);
//! - a UDF-calling generated expression is refused (the program lives in the
//!   schema; a connection-local function is not available to every writer).

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
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-generated-{}-{}.mpedb",
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
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Blob(b) => String::from_utf8_lossy(b).to_string(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let t = open();
    for s in setup {
        t.db.query(s, &[])
            .unwrap_or_else(|e| panic!("mpedb setup `{s}` failed: {e}"));
    }
    match t.db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{query}`, got {other:?}"),
    }
}

fn sqlite_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Both engines run the script and produce the SAME rows.
fn assert_same(setup: &[&str], query: &str) {
    let m = mpedb_state(setup, query);
    let s = sqlite_state(setup, query);
    assert_eq!(m, s, "mpedb vs sqlite diverged for:\n{setup:?}\n{query}");
}

/// Both engines REFUSE `stmt` after the same setup. Neither message is compared
/// — only that the statement fails on both, which is what "matching shape"
/// means for a refusal.
fn assert_both_refuse(setup: &[&str], stmt: &str) {
    let t = open();
    for s in setup {
        t.db.query(s, &[])
            .unwrap_or_else(|e| panic!("mpedb setup `{s}` failed: {e}"));
    }
    let m = t.db.query(stmt, &[]);
    assert!(
        m.is_err(),
        "mpedb ACCEPTED `{stmt}` which sqlite refuses; got {:?}",
        m.map(|_| ())
    );

    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(stmt);
    script.push_str(";\n");
    let s = sqlite_oracle::try_script_stdout(&script, "");
    assert!(
        s.is_err(),
        "sqlite ACCEPTED `{stmt}` which mpedb refuses — the refusal is a real \
         divergence, not agreement:\n{s:?}"
    );
}

const STORED: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, \
     s INTEGER GENERATED ALWAYS AS (a + b) STORED)";
const VIRT: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, \
     s INTEGER GENERATED ALWAYS AS (a + b) VIRTUAL)";
/// No `GENERATED ALWAYS`, no storage word: sqlite's short spelling, VIRTUAL.
const SHORT: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, s INTEGER AS (a + b))";

// ------------------------------------------------------------------ values

#[test]
fn stored_value_is_computed_on_insert() {
    assert_same(
        &[STORED, "INSERT INTO t VALUES (1, 10, 20)", "INSERT INTO t VALUES (2, 3, 4)"],
        "SELECT id, a, b, s, typeof(s) FROM t ORDER BY id",
    );
}

#[test]
fn virtual_value_matches_stored_value() {
    assert_same(
        &[VIRT, "INSERT INTO t VALUES (1, 10, 20)", "INSERT INTO t VALUES (2, 3, 4)"],
        "SELECT id, a, b, s, typeof(s) FROM t ORDER BY id",
    );
}

#[test]
fn short_spelling_without_generated_always() {
    assert_same(
        &[SHORT, "INSERT INTO t VALUES (1, 10, 20)"],
        "SELECT id, a, b, s, typeof(s) FROM t",
    );
}

#[test]
fn select_star_includes_the_generated_column() {
    assert_same(
        &[STORED, "INSERT INTO t VALUES (1, 10, 20)"],
        "SELECT * FROM t",
    );
}

#[test]
fn insert_with_a_column_list_still_computes_it() {
    assert_same(
        &[STORED, "INSERT INTO t(id, a, b) VALUES (1, 7, 8)"],
        "SELECT id, a, b, s FROM t",
    );
}

#[test]
fn an_omitted_input_leaves_the_generated_value_null() {
    // `a + NULL` is NULL in both engines — 3VL through the generated expression.
    assert_same(
        &[STORED, "INSERT INTO t(id, a) VALUES (1, 7)"],
        "SELECT id, a, b, s, typeof(s) FROM t",
    );
}

#[test]
fn update_of_an_input_recomputes_the_generated_column() {
    assert_same(
        &[
            STORED,
            "INSERT INTO t VALUES (1, 10, 20)",
            "INSERT INTO t VALUES (2, 1, 1)",
            "UPDATE t SET a = 100 WHERE id = 1",
        ],
        "SELECT id, a, b, s FROM t ORDER BY id",
    );
}

#[test]
fn update_of_an_unrelated_row_leaves_others_alone() {
    assert_same(
        &[
            STORED,
            "INSERT INTO t VALUES (1, 10, 20)",
            "INSERT INTO t VALUES (2, 1, 1)",
            "UPDATE t SET b = b + 5 WHERE id = 2",
        ],
        "SELECT id, a, b, s FROM t ORDER BY id",
    );
}

#[test]
fn delete_by_generated_value() {
    assert_same(
        &[
            STORED,
            "INSERT INTO t VALUES (1, 10, 20)",
            "INSERT INTO t VALUES (2, 1, 1)",
            "DELETE FROM t WHERE s = 30",
        ],
        "SELECT id, s FROM t ORDER BY id",
    );
}

#[test]
fn text_expression_and_collation_free_functions() {
    // Django's actual shapes: `lower(name)` into a CHAR column, and a copy.
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name varchar(10), \
             low varchar(11) GENERATED ALWAYS AS (lower(name)) STORED, \
             copy varchar(10) GENERATED ALWAYS AS (name) STORED)",
            "INSERT INTO t(id, name) VALUES (1, 'MiXeD')",
            "INSERT INTO t(id, name) VALUES (2, NULL)",
        ],
        "SELECT id, name, low, copy, typeof(low), typeof(copy) FROM t ORDER BY id",
    );
}

#[test]
fn constant_expression() {
    // Django's `Value("Constant")` GeneratedField.
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, \
             f varchar(10) GENERATED ALWAYS AS ('Constant') STORED)",
            "INSERT INTO t(id) VALUES (1)",
        ],
        "SELECT id, f, typeof(f) FROM t",
    );
}

#[test]
fn a_generated_column_may_read_an_earlier_generated_column() {
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, \
             b INTEGER GENERATED ALWAYS AS (a * 2) STORED, \
             c INTEGER GENERATED ALWAYS AS (b + 1) STORED)",
            "INSERT INTO t(id, a) VALUES (1, 5)",
        ],
        "SELECT id, a, b, c FROM t",
    );
}

#[test]
fn a_generated_column_may_read_the_integer_primary_key() {
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, \
             d INTEGER GENERATED ALWAYS AS (id * 10) STORED)",
            "INSERT INTO t(id) VALUES (3)",
            "INSERT INTO t(id) VALUES (NULL)",
        ],
        "SELECT id, d FROM t ORDER BY id",
    );
}

#[test]
fn generated_column_participates_in_a_check() {
    // sqlite evaluates a CHECK over the row AFTER the generated columns are
    // materialized, so a CHECK may name one. Both must accept the good row.
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, \
             s INTEGER GENERATED ALWAYS AS (a * 2) STORED CHECK (s < 100))",
            "INSERT INTO t(id, a) VALUES (1, 4)",
        ],
        "SELECT id, a, s FROM t",
    );
}

#[test]
fn a_check_over_a_generated_column_rejects_the_bad_row() {
    assert_both_refuse(
        &["CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, \
           s INTEGER GENERATED ALWAYS AS (a * 2) STORED CHECK (s < 100))"],
        "INSERT INTO t(id, a) VALUES (1, 500)",
    );
}

// ----------------------------------------------------------------- indexes

#[test]
fn index_on_a_generated_column_answers_the_same_rows() {
    assert_same(
        &[
            STORED,
            "CREATE INDEX ix ON t(s)",
            "INSERT INTO t VALUES (1, 10, 20)",
            "INSERT INTO t VALUES (2, 1, 1)",
            "INSERT INTO t VALUES (3, 15, 15)",
        ],
        "SELECT id, s FROM t WHERE s = 30 ORDER BY id",
    );
}

#[test]
fn unique_on_a_generated_column_refuses_the_duplicate() {
    assert_both_refuse(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, \
             s INTEGER GENERATED ALWAYS AS (a * 2) STORED UNIQUE)",
            "INSERT INTO t(id, a) VALUES (1, 5)",
        ],
        "INSERT INTO t(id, a) VALUES (2, 5)",
    );
}

// ---------------------------------------------------------- write refusals

#[test]
fn insert_naming_a_generated_column_is_refused() {
    assert_both_refuse(&[STORED], "INSERT INTO t(id, a, b, s) VALUES (1, 1, 2, 99)");
}

#[test]
fn insert_naming_a_virtual_generated_column_is_refused() {
    assert_both_refuse(&[VIRT], "INSERT INTO t(id, a, b, s) VALUES (1, 1, 2, 99)");
}

#[test]
fn a_values_list_counts_only_the_non_generated_columns() {
    // Four columns, three values: correct in both. The complementary case —
    // four values — must be refused by both.
    assert_same(
        &[STORED, "INSERT INTO t VALUES (1, 2, 3)"],
        "SELECT id, a, b, s FROM t",
    );
    assert_both_refuse(&[STORED], "INSERT INTO t VALUES (1, 2, 3, 4)");
}

#[test]
fn update_setting_a_generated_column_is_refused() {
    assert_both_refuse(
        &[STORED, "INSERT INTO t VALUES (1, 1, 2)"],
        "UPDATE t SET s = 99 WHERE id = 1",
    );
}

#[test]
fn a_generated_column_cannot_be_the_primary_key() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (a INTEGER, k INTEGER GENERATED ALWAYS AS (a) STORED PRIMARY KEY)",
    );
}

#[test]
fn a_generated_column_cannot_carry_a_default() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
         s INTEGER GENERATED ALWAYS AS (a) STORED DEFAULT 5)",
    );
}

#[test]
fn a_generated_expression_may_not_contain_a_subquery() {
    assert_both_refuse(
        &["CREATE TABLE other (id INTEGER PRIMARY KEY)"],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, \
         s INTEGER GENERATED ALWAYS AS ((SELECT count(*) FROM other)) STORED)",
    );
}

#[test]
fn a_generated_expression_may_not_be_an_aggregate() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
         s INTEGER GENERATED ALWAYS AS (sum(a)) STORED)",
    );
}

#[test]
fn a_generated_expression_may_not_name_an_unknown_column() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, \
         s INTEGER GENERATED ALWAYS AS (nope) STORED)",
    );
}

#[test]
fn stored_and_virtual_together_is_a_syntax_error() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
         s INTEGER GENERATED ALWAYS AS (a) STORED VIRTUAL)",
    );
}

#[test]
fn an_unparenthesized_generated_expression_is_a_syntax_error() {
    assert_both_refuse(
        &[],
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, s INTEGER AS a)",
    );
}

/// Both engines refuse a self-referencing generated column — mpedb at
/// `CREATE TABLE` (it is the degenerate case of the forward-reference rule),
/// sqlite only at the first INSERT ("generated column loop"). Same outcome, and
/// mpedb's is strictly earlier, so the only thing to pin is that neither ever
/// stores a row.
#[test]
fn a_self_referencing_generated_column_is_refused() {
    let ddl = "CREATE TABLE g (id INTEGER PRIMARY KEY, s INTEGER GENERATED ALWAYS AS (s) STORED)";
    let t = open();
    assert!(
        t.db.query(ddl, &[]).is_err(),
        "mpedb must refuse a self-referencing generated column at CREATE TABLE"
    );
    let s = sqlite_oracle::try_script_stdout(
        &format!("{ddl};\nINSERT INTO g(id) VALUES (1);\n"),
        "",
    );
    assert!(s.is_err(), "sqlite must refuse the insert: {s:?}");
}

// ------------------------------------------------------- ALTER ADD COLUMN

#[test]
fn add_column_virtual_on_a_populated_table() {
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)",
            "INSERT INTO t VALUES (1, 5)",
            "INSERT INTO t VALUES (2, 6)",
            "ALTER TABLE t ADD COLUMN d INTEGER GENERATED ALWAYS AS (a * 3) VIRTUAL",
        ],
        "SELECT id, a, d FROM t ORDER BY id",
    );
}

#[test]
fn add_column_stored_on_a_populated_table_is_refused() {
    // sqlite: "cannot add a STORED column". mpedb could rewrite the rows, but
    // agreeing with sqlite is worth more than the capability.
    assert_both_refuse(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)",
            "INSERT INTO t VALUES (1, 5)",
        ],
        "ALTER TABLE t ADD COLUMN d INTEGER GENERATED ALWAYS AS (a * 3) STORED",
    );
}

// ----------------------------------------- mpedb's declared narrowings

/// A FORWARD reference is where mpedb is deliberately narrower than sqlite:
/// sqlite resolves it, mpedb refuses it by name so its single
/// declaration-order evaluation pass stays provably correct. Asserted directly
/// (not through `assert_both_refuse`) precisely because the two disagree.
#[test]
fn a_forward_reference_between_generated_columns_is_refused_by_name() {
    let t = open();
    let e = t
        .db
        .query(
            "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
             c INTEGER GENERATED ALWAYS AS (b) STORED, \
             b INTEGER GENERATED ALWAYS AS (a) STORED)",
            &[],
        )
        .unwrap_err();
    let msg = format!("{e}");
    assert!(
        msg.contains("declared at or after it"),
        "expected the forward-reference refusal, got: {msg}"
    );

    // sqlite ACCEPTS this one — recorded, so the divergence is deliberate and
    // visible rather than discovered.
    let ok = sqlite_oracle::try_script_stdout(
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
         c INTEGER GENERATED ALWAYS AS (b) STORED, \
         b INTEGER GENERATED ALWAYS AS (a) STORED);\n",
        "",
    );
    assert!(ok.is_ok(), "sqlite is expected to accept a forward reference");
}

/// The generated program lives in the SCHEMA, so every writer must be able to
/// evaluate it — a connection-local UDF cannot appear in one.
#[test]
fn a_generated_expression_may_not_call_a_host_udf() {
    let t = open();
    t.db.register_host_function("myfn", 1, |args: &[Value]| Ok(args[0].clone()));
    let e = t
        .db
        .query(
            "CREATE TABLE g (id INTEGER PRIMARY KEY, a INTEGER, \
             s INTEGER GENERATED ALWAYS AS (myfn(a)) STORED)",
            &[],
        )
        .unwrap_err();
    let msg = format!("{e}");
    // The binder that compiles a generated expression is built WITHOUT the
    // connection's UDF registry, so the refusal lands as "unknown function"
    // before `Schema::validate`'s `has_host_call` guard is even reached. Either
    // wording is the same contract: the schema never stores a program only one
    // connection could evaluate.
    assert!(
        msg.contains("unknown function") || msg.contains("host-registered function"),
        "expected the host-UDF refusal, got: {msg}"
    );
}

// ------------------------------------------------------------- durability

/// The value survives a close/reopen: the schema (including the compiled
/// program) round-trips through the catalog's canonical bytes, and a write made
/// by the REOPENED handle computes the same column.
#[test]
fn generated_columns_survive_a_reopen() {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-gen-reopen-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        db.query(STORED, &[]).unwrap();
        db.query("INSERT INTO t VALUES (1, 10, 20)", &[]).unwrap();
    }
    {
        let db = Database::open_with_config(cfg).unwrap();
        db.query("INSERT INTO t VALUES (2, 3, 4)", &[]).unwrap();
        let rows = match db.query("SELECT id, s FROM t ORDER BY id", &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Int(30));
        assert_eq!(rows[1][1], Value::Int(7));
    }
    let _ = std::fs::remove_file(&path);
}
