//! Scratch: nested derived-table repros.

use mpedb::{Config, Database, Error, ExecResult, Value};
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

const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "a"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true

[[table]]
name = "u"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "b"
  type = "int64"
  nullable = true
"#;

fn insert_statements() -> Vec<&'static str> {
    vec![
        "INSERT INTO t (id, a, s) VALUES (1,10,'x'),(2,20,'y'),(3,20,'x'),(4,NULL,'z'),(5,30,NULL),(6,10,'y')",
        "INSERT INTO u (id, b) VALUES (1,10),(2,20),(3,NULL),(4,99)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-derived-nested-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

const NULLV: &str = "<NULL>";

fn render(v: Value) -> String {
    match v {
        Value::Null => NULLV.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Result<Vec<Vec<String>>, Error> {
    match db.query(sql, &[])? {
        ExecResult::Rows { rows, .. } => Ok(rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect()),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

fn oracle_script(query: &str) -> String {
    let mut script = String::from(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, s TEXT);\n\
         CREATE TABLE u (id INTEGER PRIMARY KEY, b INTEGER);\n",
    );
    for stmt in insert_statements() {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    script
}

fn sqlite_rows(query: &str) -> Result<Vec<Vec<String>>, String> {
    Ok(sqlite_oracle::try_script_stdout(&oracle_script(query), NULLV)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect())
}

#[test]
fn probe() {
    let d = db();
    let queries = [
        // A — test_union_nested
        "SELECT id FROM t UNION SELECT * FROM (SELECT id FROM t UNION SELECT id FROM u)",
        // B — test_qs_with_subcompound_qs
        "SELECT count(*) FROM (SELECT id FROM t EXCEPT SELECT * FROM (SELECT id FROM t INTERSECT SELECT id FROM u WHERE b > 10)) sub",
        // C — test_distinct_ordered_sliced_subquery
        "SELECT s FROM t WHERE id IN (SELECT sq.id FROM (SELECT DISTINCT id, a FROM t ORDER BY a LIMIT 2) sq) ORDER BY 1",
        // D — test_distinct_ordered_sliced_subquery_aggregation
        "SELECT count(*) FROM (SELECT sq.c1, sq.c2 FROM (SELECT DISTINCT t.id AS c1, t.a AS c2, u.b FROM t LEFT JOIN u ON u.id = t.id ORDER BY u.b LIMIT 3) sq) sq2",
    ];
    for q in queries {
        let ours = mpedb_rows(&d, q);
        let theirs = sqlite_rows(q);
        println!("---- {q}");
        println!("  mpedb : {ours:?}");
        println!("  sqlite: {theirs:?}");
    }
    // E — test_bulk_insert
    let e = d.query("INSERT INTO t (id, s) VALUES (7, lower('A')), (8, lower('B'))", &[]);
    println!("---- multi-row VALUES with expression: {e:?}");
}
