//! #45: 3+ table INNER JOIN. A left-deep chain a→b→c, join order preserved,
//! and the executor folds N tables (not just 2).
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

fn db(tag: &str) -> Tmp {
    let path = format!("/dev/shm/mpedb-3way-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    // customers 1:N orders 1:N lines
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"customers\"\nprimary_key = [\"cid\"]\n  \
         [[table.column]]\n  name = \"cid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cname\"\n  type = \"text\"\n  nullable = false\n\
         [[table]]\nname = \"orders\"\nprimary_key = [\"oid\"]\n  \
         [[table.column]]\n  name = \"oid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cid\"\n  type = \"int64\"\n  nullable = false\n\
         [[table]]\nname = \"lines\"\nprimary_key = [\"lid\"]\n  \
         [[table.column]]\n  name = \"lid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"oid\"\n  type = \"int64\"\n  nullable = false\n  \
         [[table.column]]\n  name = \"amount\"\n  type = \"int64\"\n  nullable = false"
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    Tmp { db, path }
}

fn seed(d: &Database) {
    let c = d.prepare("INSERT INTO customers (cid, cname) VALUES ($1, $2)").unwrap();
    d.execute(&c, &params![1i64, "ada"]).unwrap();
    d.execute(&c, &params![2i64, "bob"]).unwrap();
    let o = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    d.execute(&o, &params![10i64, 1i64]).unwrap(); // ada's order
    d.execute(&o, &params![20i64, 2i64]).unwrap(); // bob's order
    let l = d.prepare("INSERT INTO lines (lid, oid, amount) VALUES ($1, $2, $3)").unwrap();
    d.execute(&l, &params![100i64, 10i64, 5i64]).unwrap();  // ada: 5
    d.execute(&l, &params![101i64, 10i64, 7i64]).unwrap();  // ada: 7
    d.execute(&l, &params![102i64, 20i64, 3i64]).unwrap();  // bob: 3
}

/// A 3-table chain customers→orders→lines: which customer each line belongs to.
#[test]
fn three_table_chain() {
    let d = db("chain");
    seed(&d);
    let rows = match d.query(
        "SELECT customers.cname, lines.amount \
         FROM customers \
         JOIN orders ON orders.cid = customers.cid \
         JOIN lines ON lines.oid = orders.oid \
         ORDER BY lines.amount",
        &[],
    ).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    };
    let got: Vec<(String, i64)> = rows.iter().map(|r| (
        match &r[0] { Value::Text(s) => s.clone(), v => panic!("{v:?}") },
        match &r[1] { Value::Int(n) => *n, v => panic!("{v:?}") },
    )).collect();
    assert_eq!(got, vec![
        ("bob".into(), 3),
        ("ada".into(), 5),
        ("ada".into(), 7),
    ]);
}

/// Aggregate over a 3-table join: total amount per customer.
#[test]
fn three_table_aggregate() {
    let d = db("agg");
    seed(&d);
    let rows = match d.query(
        "SELECT customers.cname, sum(lines.amount) \
         FROM customers \
         JOIN orders ON orders.cid = customers.cid \
         JOIN lines ON lines.oid = orders.oid \
         GROUP BY customers.cname ORDER BY customers.cname",
        &[],
    ).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    };
    let got: Vec<(String, i64)> = rows.iter().map(|r| (
        match &r[0] { Value::Text(s) => s.clone(), v => panic!("{v:?}") },
        match &r[1] { Value::Int(n) => *n, v => panic!("{v:?}") },
    )).collect();
    assert_eq!(got, vec![("ada".into(), 12), ("bob".into(), 3)]);
}
