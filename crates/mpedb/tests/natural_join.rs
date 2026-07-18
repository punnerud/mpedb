//! `NATURAL [INNER | LEFT] JOIN` — the implicit `USING` over the columns common
//! to the two sides.
//!
//! `a NATURAL JOIN b` ≡ `a JOIN b USING (<all column names common to both>)`.
//! The common set is resolved at PLAN time from the schema — a rigid schema makes
//! it a static fact, so the content-hashed plan pins the exact column set and the
//! match cannot silently drift as it can in a schemaless engine. Everything then
//! flows through the just-shipped USING machinery: the ON-equalities and, under
//! `SELECT *`, the COALESCE of each common column (once, from the left side).
//!
//! Edge cases matched to sqlite 3.45: NO common column ⇒ a cross join (`ON true`);
//! a column common to two already-joined LEFT tables equates the LEFTMOST. RIGHT /
//! FULL / CROSS NATURAL are refused — the coalesced column cannot survive the
//! side-swap / both-sides-whole rewrites, the same reason USING refuses them.
//! Every expected result below was cross-checked against the `sqlite3` 3.45 CLI.
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
        .join(format!("mpedb-natural-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    // a(id,x,name) / b(id,x,y): common column NAMES {id, x}.
    // c(id,cname):              common with `a` is {id} only.
    // p(pid,pv) / q(qid,qv):    NO common column name at all.
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n  nullable = false\n  \
         [[table.column]]\n  name = \"name\"\n  type = \"text\"\n  nullable = false\n\
         [[table]]\nname = \"b\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n  nullable = false\n  \
         [[table.column]]\n  name = \"y\"\n  type = \"int64\"\n  nullable = false\n\
         [[table]]\nname = \"c\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cname\"\n  type = \"text\"\n  nullable = false\n\
         [[table]]\nname = \"p\"\nprimary_key = [\"pid\"]\n  \
         [[table.column]]\n  name = \"pid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"pv\"\n  type = \"text\"\n  nullable = false\n\
         [[table]]\nname = \"q\"\nprimary_key = [\"qid\"]\n  \
         [[table.column]]\n  name = \"qid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"qv\"\n  type = \"text\"\n  nullable = false"
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    Tmp { db, path }
}

fn seed(d: &Database) {
    let a = d.prepare("INSERT INTO a (id, x, name) VALUES ($1, $2, $3)").unwrap();
    for (id, x, name) in [(1i64, 10i64, "a1"), (2, 20, "a2"), (3, 30, "a3")] {
        d.execute(&a, &params![id, x, name]).unwrap();
    }
    let b = d.prepare("INSERT INTO b (id, x, y) VALUES ($1, $2, $3)").unwrap();
    for (id, x, y) in [(1i64, 10i64, 100i64), (2, 99, 200), (4, 40, 400)] {
        d.execute(&b, &params![id, x, y]).unwrap();
    }
    let c = d.prepare("INSERT INTO c (id, cname) VALUES ($1, $2)").unwrap();
    for (id, cname) in [(1i64, "c1"), (2, "c2")] {
        d.execute(&c, &params![id, cname]).unwrap();
    }
    let p = d.prepare("INSERT INTO p (pid, pv) VALUES ($1, $2)").unwrap();
    for (pid, pv) in [(1i64, "p1"), (2, "p2")] {
        d.execute(&p, &params![pid, pv]).unwrap();
    }
    let q = d.prepare("INSERT INTO q (qid, qv) VALUES ($1, $2)").unwrap();
    for (qid, qv) in [(10i64, "q1"), (20, "q2")] {
        d.execute(&q, &params![qid, qv]).unwrap();
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
fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        v => panic!("expected text, got {v:?}"),
    }
}

/// One common column (`id`): `SELECT *` COALESCES it — it shows once (from the
/// left), then the rest of the left, then the right's non-common columns.
/// sqlite `SELECT * FROM a NATURAL JOIN c` -> cols id,x,name,cname;
/// rows (1,10,a1,c1),(2,20,a2,c2).
#[test]
fn one_common_column_star_coalesced() {
    let d = db("one");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM a NATURAL JOIN c ORDER BY a.id");
    assert_eq!(cols, ["a.id", "a.x", "a.name", "c.cname"]);
    let got: Vec<(i64, i64, &str, &str)> = rs
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]), text(&r[2]), text(&r[3])))
        .collect();
    assert_eq!(got, vec![(1, 10, "a1", "c1"), (2, 20, "a2", "c2")]);
}

/// Multi-common columns (`id`, `x`): the implicit condition is
/// `a.id = b.id AND a.x = b.x`, so only rows matching on BOTH survive, and BOTH
/// join columns are coalesced under `SELECT *`.
/// sqlite `SELECT * FROM a NATURAL JOIN b` -> cols id,x,name,y; row (1,10,a1,100).
#[test]
fn multi_common_columns() {
    let d = db("multi");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM a NATURAL JOIN b ORDER BY a.id");
    assert_eq!(cols, ["a.id", "a.x", "a.name", "b.y"]);
    let got: Vec<(i64, i64, &str, i64)> = rs
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]), text(&r[2]), int(&r[3])))
        .collect();
    assert_eq!(got, vec![(1, 10, "a1", 100)]);
}

/// Explicit projection over a natural join reaches the coalesced column by its
/// left name. sqlite `SELECT a.id, name, y FROM a NATURAL JOIN b` -> (1,a1,100).
#[test]
fn explicit_projection() {
    let d = db("proj");
    seed(&d);
    let (_, rs) = rows(&d, "SELECT a.id, a.name, b.y FROM a NATURAL JOIN b");
    let got: Vec<(i64, &str, i64)> =
        rs.iter().map(|r| (int(&r[0]), text(&r[1]), int(&r[2]))).collect();
    assert_eq!(got, vec![(1, "a1", 100)]);
}

/// `NATURAL LEFT JOIN`: every left row survives; an unmatched right side is NULL.
/// sqlite `SELECT * FROM a NATURAL LEFT JOIN b`
///   -> (1,10,a1,100),(2,20,a2,NULL),(3,30,a3,NULL).
#[test]
fn natural_left_join() {
    let d = db("left");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM a NATURAL LEFT JOIN b ORDER BY a.id");
    assert_eq!(cols, ["a.id", "a.x", "a.name", "b.y"]);
    assert_eq!(rs.len(), 3);
    assert_eq!((int(&rs[0][0]), int(&rs[0][1]), text(&rs[0][2]), int(&rs[0][3])), (1, 10, "a1", 100));
    assert_eq!((int(&rs[1][0]), int(&rs[1][1]), text(&rs[1][2])), (2, 20, "a2"));
    assert_eq!(rs[1][3], Value::Null, "unmatched right side is NULL");
    assert_eq!((int(&rs[2][0]), int(&rs[2][1]), text(&rs[2][2])), (3, 30, "a3"));
    assert_eq!(rs[2][3], Value::Null);
}

/// `NATURAL LEFT OUTER JOIN` — the `OUTER` keyword is a no-op, same join.
#[test]
fn natural_left_outer_is_left() {
    let d = db("outer");
    seed(&d);
    let (_, plain) = rows(&d, "SELECT * FROM a NATURAL LEFT JOIN b ORDER BY a.id");
    let (_, outer) = rows(&d, "SELECT * FROM a NATURAL LEFT OUTER JOIN b ORDER BY a.id");
    assert_eq!(plain, outer);
}

/// NO common column ⇒ the natural join is a CROSS join (`ON true`): every pair,
/// all columns of both sides. sqlite `SELECT * FROM p NATURAL JOIN q` -> 4 rows,
/// cols pid,pv,qid,qv.
#[test]
fn no_common_columns_is_cross_join() {
    let d = db("cross");
    seed(&d);
    let (cols, rs) = rows(&d, "SELECT * FROM p NATURAL JOIN q ORDER BY p.pid, q.qid");
    assert_eq!(cols, ["p.pid", "p.pv", "q.qid", "q.qv"]);
    let got: Vec<(i64, &str, i64, &str)> = rs
        .iter()
        .map(|r| (int(&r[0]), text(&r[1]), int(&r[2]), text(&r[3])))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "p1", 10, "q1"),
            (1, "p1", 20, "q2"),
            (2, "p2", 10, "q1"),
            (2, "p2", 20, "q2"),
        ]
    );
    // And the count, the way sqlite reports it: 2 x 2 = 4.
    let (_, cnt) = rows(&d, "SELECT count(*) FROM p NATURAL JOIN q");
    assert_eq!(int(&cnt[0][0]), 4);
}

/// A bare `NATURAL JOIN` with no side word is an INNER join — same result as the
/// explicit `NATURAL INNER JOIN`.
#[test]
fn natural_inner_is_default() {
    let d = db("inner");
    seed(&d);
    let (_, bare) = rows(&d, "SELECT * FROM a NATURAL JOIN c ORDER BY a.id");
    let (_, inner) = rows(&d, "SELECT * FROM a NATURAL INNER JOIN c ORDER BY a.id");
    assert_eq!(bare, inner);
}

/// NATURAL RIGHT JOIN is refused — the coalesced column cannot survive the
/// side-swap (the same reason `RIGHT JOIN USING` is refused).
#[test]
fn natural_right_join_refused() {
    let d = db("right");
    seed(&d);
    let err = d.query("SELECT * FROM a NATURAL RIGHT JOIN b", &[]).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("natural") || msg.contains("right"),
        "expected a NATURAL RIGHT refusal, got: {msg}"
    );
}

/// NATURAL FULL and NATURAL CROSS are likewise refused.
#[test]
fn natural_full_and_cross_refused() {
    let d = db("fullcross");
    seed(&d);
    assert!(
        d.query("SELECT * FROM a NATURAL FULL JOIN b", &[]).is_err(),
        "NATURAL FULL JOIN should be refused"
    );
    assert!(
        d.query("SELECT * FROM a NATURAL CROSS JOIN b", &[]).is_err(),
        "NATURAL CROSS JOIN should be refused"
    );
}
