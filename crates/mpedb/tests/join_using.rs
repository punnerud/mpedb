//! `JOIN … USING (col, …)` — the explicit, schema-safe form of a natural join.
//!
//! `a JOIN b USING (x)` ≡ `a JOIN b ON a.x = b.x`, EXCEPT that under `SELECT *`
//! the join column `x` is COALESCED — it appears once (from the left side), not
//! once per side. NATURAL JOIN (implicit USING over all common columns) stays
//! refused; only the explicit column list is supported. Every expected result
//! below was cross-checked against the `sqlite3` 3.45 CLI.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

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

fn db(tag: &str) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-using-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    // a(id,x) / b(id,y): a single-column join key `id`.
    // p(k1,k2,x) / q(k1,k2,y): a two-column join key `(k1,k2)`.
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n  nullable = false\n\
         [[table]]\nname = \"b\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"y\"\n  type = \"int64\"\n  nullable = false\n\
         [[table]]\nname = \"p\"\nprimary_key = [\"k1\", \"k2\"]\n  \
         [[table.column]]\n  name = \"k1\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"k2\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n  nullable = false\n\
         [[table]]\nname = \"q\"\nprimary_key = [\"k1\", \"k2\"]\n  \
         [[table.column]]\n  name = \"k1\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"k2\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"y\"\n  type = \"int64\"\n  nullable = false"
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    Tmp { db, path }
}

fn seed(d: &Database) {
    let a = d.prepare("INSERT INTO a (id, x) VALUES ($1, $2)").unwrap();
    for (id, x) in [(1i64, 10i64), (2, 20), (3, 30)] {
        d.execute(&a, &params![id, x]).unwrap();
    }
    let b = d.prepare("INSERT INTO b (id, y) VALUES ($1, $2)").unwrap();
    for (id, y) in [(1i64, 100i64), (2, 200), (4, 400)] {
        d.execute(&b, &params![id, y]).unwrap();
    }
    let p = d.prepare("INSERT INTO p (k1, k2, x) VALUES ($1, $2, $3)").unwrap();
    for (k1, k2, x) in [(1i64, 1i64, 10i64), (1, 2, 20), (2, 2, 30)] {
        d.execute(&p, &params![k1, k2, x]).unwrap();
    }
    let q = d.prepare("INSERT INTO q (k1, k2, y) VALUES ($1, $2, $3)").unwrap();
    for (k1, k2, y) in [(1i64, 1i64, 100i64), (1, 2, 200), (9, 9, 900)] {
        d.execute(&q, &params![k1, k2, y]).unwrap();
    }
}

fn rows(d: &Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match d.query(sql, &[]).unwrap() {
        ExecResult::Rows { columns, rows } => (columns, rows),
        o => panic!("expected rows, got {o:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        v => panic!("expected int, got {v:?}"),
    }
}

/// Explicit projection over a single-column USING join — the plain ON-equality.
/// sqlite: `SELECT a.x, b.y FROM a JOIN b USING(id)` -> (10,100),(20,200).
#[test]
fn explicit_cols_inner() {
    let d = db("explicit");
    seed(&d);
    let (_, rs) = rows(&d, "SELECT a.x, b.y FROM a JOIN b USING (id) ORDER BY a.x");
    let got: Vec<(i64, i64)> = rs.iter().map(|r| (int(&r[0]), int(&r[1]))).collect();
    assert_eq!(got, vec![(10, 100), (20, 200)]);
}

/// A two-column USING desugars to `p.k1 = q.k1 AND p.k2 = q.k2`.
/// sqlite: only (1,1) and (1,2) match -> (10,100),(20,200).
#[test]
fn multi_column_using() {
    let d = db("multi");
    seed(&d);
    let (_, rs) = rows(&d, "SELECT p.x, q.y FROM p JOIN q USING (k1, k2) ORDER BY p.x");
    let got: Vec<(i64, i64)> = rs.iter().map(|r| (int(&r[0]), int(&r[1]))).collect();
    assert_eq!(got, vec![(10, 100), (20, 200)]);
}

/// LEFT JOIN USING: every left row survives; an unmatched right side is NULL,
/// and the coalesced join column is the (always-present) left value.
/// sqlite `SELECT * FROM a LEFT JOIN b USING(id)` -> (1,10,100),(2,20,200),(3,30,NULL).
#[test]
fn left_join_using_star() {
    let d = db("left");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM a LEFT JOIN b USING (id) ORDER BY a.id");
    // `id` is coalesced: it appears ONCE (from `a`), not once per side.
    assert_eq!(cols, ["a.id", "a.x", "b.y"]);
    assert_eq!(rs.len(), 3);
    assert_eq!((int(&rs[0][0]), int(&rs[0][1]), int(&rs[0][2])), (1, 10, 100));
    assert_eq!((int(&rs[1][0]), int(&rs[1][1]), int(&rs[1][2])), (2, 20, 200));
    assert_eq!((int(&rs[2][0]), int(&rs[2][1])), (3, 30));
    assert_eq!(rs[2][2], Value::Null, "unmatched right side is NULL");
}

/// `SELECT *` over an INNER USING join: the join column appears exactly once.
/// sqlite `SELECT * FROM a JOIN b USING(id)` -> columns id,x,y (no duplicate id).
#[test]
fn star_coalesced_once_inner() {
    let d = db("star");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM a JOIN b USING (id) ORDER BY a.id");
    assert_eq!(cols, ["a.id", "a.x", "b.y"]);
    let got: Vec<(i64, i64, i64)> = rs
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]), int(&r[2])))
        .collect();
    assert_eq!(got, vec![(1, 10, 100), (2, 20, 200)]);
}

/// `SELECT *` over a two-column USING join drops BOTH join columns from the
/// right side. sqlite -> columns k1,k2,x,y.
#[test]
fn star_coalesced_multi() {
    let d = db("starmulti");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM p JOIN q USING (k1, k2) ORDER BY p.x");
    assert_eq!(cols, ["p.k1", "p.k2", "p.x", "q.y"]);
    let got: Vec<(i64, i64, i64, i64)> = rs
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]), int(&r[2]), int(&r[3])))
        .collect();
    assert_eq!(got, vec![(1, 1, 10, 100), (1, 2, 20, 200)]);
}

/// NATURAL JOIN stays refused — its condition is implicit in column names,
/// which rigid schemas make a trap. Only explicit USING is supported.
#[test]
fn natural_join_refused() {
    let d = db("natural");
    seed(&d);
    let err = d.query("SELECT * FROM a NATURAL JOIN b", &[]).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("natural"),
        "expected a NATURAL-join refusal, got: {msg}"
    );
}

/// A USING column must exist on BOTH sides — a clean bind error otherwise.
#[test]
fn using_column_missing_refused() {
    let d = db("missing");
    seed(&d);
    // `x` exists in `a` but not in `b`.
    assert!(d.query("SELECT * FROM a JOIN b USING (x)", &[]).is_err());
    // `nope` exists nowhere.
    assert!(d.query("SELECT * FROM a JOIN b USING (nope)", &[]).is_err());
}

/// RIGHT / FULL JOIN USING are refused (v1) — the coalesced column would have to
/// survive the side-swap / both-sides-whole rewrites. INNER and LEFT are the
/// supported forms.
#[test]
fn right_full_using_refused() {
    let d = db("rightfull");
    seed(&d);
    assert!(
        d.query("SELECT * FROM a RIGHT JOIN b USING (id)", &[]).is_err(),
        "RIGHT JOIN USING should be refused"
    );
    assert!(
        d.query("SELECT * FROM a FULL JOIN b USING (id)", &[]).is_err(),
        "FULL JOIN USING should be refused"
    );
}
