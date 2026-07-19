//! RIGHT JOIN inside an N-way chain (3+ tables). mpedb supports a RIGHT join as
//! the FIRST join of a chain by rewriting `A RIGHT JOIN B [rest]` to the
//! equivalent left-deep `B LEFT JOIN A [rest]` (same row set; the pinned
//! `SELECT *` order undoes the swap). Every expected value below was taken from
//! sqlite 3.45.1 on the identical schema/data — this is a differential test with
//! the reference outputs inlined so the test stays deterministic and offline.
//!
//! Shapes that a left-deep plan cannot express stay REFUSED (never answered
//! wrong): a RIGHT that is not first, a second RIGHT, any FULL in a chain, and a
//! USING/NATURAL join trailing a leading RIGHT.
use mpedb::{Config, Database, ExecResult, Value};
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
        .join(format!("mpedb-jchain-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    // a  1:N  b  1:N  c  1:N  d, with deliberate outer-join holes:
    //   b11 has aid=3 (no a)   -> RIGHT keeps it, a null-extends
    //   c102 has bid=99 (no b) -> unreachable through b here
    //   d1001 has cid=999 (no c)
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"av\"\n  type = \"text\"\n\
         [[table]]\nname = \"b\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"aid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"bv\"\n  type = \"text\"\n\
         [[table]]\nname = \"c\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"bid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cv\"\n  type = \"text\"\n\
         [[table]]\nname = \"d\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"dv\"\n  type = \"text\""
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    let d = &db;
    for (id, av) in [(1i64, "a1"), (2, "a2")] {
        d.query(&format!("INSERT INTO a (id, av) VALUES ({id}, '{av}')"), &[]).unwrap();
    }
    for (id, aid, bv) in [(10i64, 1i64, "b10"), (11, 3, "b11"), (12, 2, "b12")] {
        d.query(&format!("INSERT INTO b (id, aid, bv) VALUES ({id}, {aid}, '{bv}')"), &[])
            .unwrap();
    }
    for (id, bid, cv) in [(100i64, 10i64, "c100"), (101, 12, "c101"), (102, 99, "c102")] {
        d.query(&format!("INSERT INTO c (id, bid, cv) VALUES ({id}, {bid}, '{cv}')"), &[])
            .unwrap();
    }
    for (id, cid, dv) in [(1000i64, 100i64, "d1000"), (1001, 999, "d1001")] {
        d.query(&format!("INSERT INTO d (id, cid, dv) VALUES ({id}, {cid}, '{dv}')"), &[])
            .unwrap();
    }
    Tmp { db, path }
}

fn run(d: &Database, sql: &str) -> Vec<Vec<Value>> {
    match d.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        o => panic!("unexpected result for {sql:?}: {o:?}"),
    }
}

/// Rows as `Option<String>` cells (NULL -> None) for order-sensitive equality.
fn cells(rows: &[Vec<Value>]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Int(n) => Some(n.to_string()),
                    Value::Text(s) => Some(s.clone()),
                    Value::Float(f) => Some(f.to_string()),
                    other => Some(format!("{other:?}")),
                })
                .collect()
        })
        .collect()
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

/// `A RIGHT JOIN B  INNER JOIN C` — only B rows with a C survive; the a-less
/// b11 is dropped by the inner C join. (sqlite: a1|b10|c100 / a2|b12|c101)
#[test]
fn right_then_inner() {
    let d = db("ri");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv, c.cv FROM a \
         RIGHT JOIN b ON a.id = b.aid \
         JOIN c ON c.bid = b.id \
         ORDER BY b.bv, c.cv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("a1"), s("b10"), s("c100")],
            vec![s("a2"), s("b12"), s("c101")],
        ]
    );
}

/// `A RIGHT JOIN B  LEFT JOIN C` — every B survives; b11 keeps a NULL a AND a
/// NULL c. (sqlite: a1|b10|c100 / NULL|b11|NULL / a2|b12|c101)
#[test]
fn right_then_left() {
    let d = db("rl");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv, c.cv FROM a \
         RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id \
         ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("a1"), s("b10"), s("c100")],
            vec![None, s("b11"), None],
            vec![s("a2"), s("b12"), s("c101")],
        ]
    );
}

/// `SELECT *` keeps the ORIGINAL column order a.*, b.*, c.* even though the plan
/// swapped a and b. (sqlite: 1|a1|10|1|b10|100|10|c100 / 2|a2|12|2|b12|101|12|c101)
#[test]
fn right_chain_select_star_column_order() {
    let d = db("star");
    let got = cells(&run(
        &d,
        "SELECT * FROM a RIGHT JOIN b ON a.id = b.aid \
         JOIN c ON c.bid = b.id ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("1"), s("a1"), s("10"), s("1"), s("b10"), s("100"), s("10"), s("c100")],
            vec![s("2"), s("a2"), s("12"), s("2"), s("b12"), s("101"), s("12"), s("c101")],
        ]
    );
}

/// 4-table chain RIGHT, INNER, LEFT. (sqlite: a1|b10|c100|d1000 / a2|b12|c101|NULL)
#[test]
fn right_inner_left_four_tables() {
    let d = db("four");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv, c.cv, d.dv FROM a \
         RIGHT JOIN b ON a.id = b.aid \
         JOIN c ON c.bid = b.id \
         LEFT JOIN d ON d.cid = c.id \
         ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("a1"), s("b10"), s("c100"), s("d1000")],
            vec![s("a2"), s("b12"), s("c101"), None],
        ]
    );
}

/// The null-extended a side is real for WHERE. With the INNER C the a-less row
/// is already gone, so `WHERE a.id IS NULL` is empty; with the LEFT C it
/// survives as b11. (sqlite: empty / b11|NULL)
#[test]
fn right_chain_antijoin_where() {
    let d = db("anti");
    let inner = cells(&run(
        &d,
        "SELECT b.bv, c.cv FROM a RIGHT JOIN b ON a.id = b.aid \
         JOIN c ON c.bid = b.id WHERE a.id IS NULL ORDER BY b.bv",
    ));
    assert!(inner.is_empty(), "{inner:?}");
    let left = cells(&run(
        &d,
        "SELECT b.bv, c.cv FROM a RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id WHERE a.id IS NULL ORDER BY b.bv",
    ));
    assert_eq!(left, vec![vec![s("b11"), None]]);
}

/// Aggregate over a RIGHT-first chain groups the JOINED row.
/// (sqlite: b10|1 / b11|0 / b12|1)
#[test]
fn right_chain_aggregate() {
    let d = db("agg");
    let got = cells(&run(
        &d,
        "SELECT b.bv, count(c.id) FROM a RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id GROUP BY b.bv ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("b10"), s("1")],
            vec![s("b11"), s("0")],
            vec![s("b12"), s("1")],
        ]
    );
}

/// DISTINCT over the chain: a's distinct values include the null-extension.
/// (sqlite: NULL / a1 / a2)
#[test]
fn right_chain_distinct() {
    let d = db("dist");
    let got = cells(&run(
        &d,
        "SELECT DISTINCT a.av FROM a RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id ORDER BY a.av",
    ));
    assert_eq!(got, vec![vec![None], vec![s("a1")], vec![s("a2")]]);
}

/// Regression: the two-table RIGHT form still works and keeps a.*, b.* order.
#[test]
fn two_table_right_still_works() {
    let d = db("two");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv FROM a RIGHT JOIN b ON a.id = b.aid ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("a1"), s("b10")],
            vec![None, s("b11")],
            vec![s("a2"), s("b12")],
        ]
    );
}

/// A trailing join whose ON references the null-extended (swapped) outer `a`:
/// after the rewrite `a` is a normal LEFT-joined inner, so `a.id` reads NULL for
/// the a-less b11 and the `a.id = 1` conjunct fails for it and for a2.
/// (sqlite: a1|b10|c100 / NULL|b11|NULL / a2|b12|NULL)
#[test]
fn trailing_on_references_swapped_outer() {
    let d = db("trail");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv, c.cv FROM a RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id AND a.id = 1 ORDER BY b.bv",
    ));
    assert_eq!(
        got,
        vec![
            vec![s("a1"), s("b10"), s("c100")],
            vec![None, s("b11"), None],
            vec![s("a2"), s("b12"), None],
        ]
    );
}

/// Trailing INNER join with an extra `a.av = 'a1'` conjunct in its ON — only the
/// b10 row (a1) keeps a c. (sqlite: a1|b10|c100)
#[test]
fn trailing_inner_on_references_outer_value() {
    let d = db("trailv");
    let got = cells(&run(
        &d,
        "SELECT a.av, b.bv, c.cv FROM a RIGHT JOIN b ON a.id = b.aid \
         JOIN c ON c.bid = b.id AND a.av = 'a1' ORDER BY b.bv",
    ));
    assert_eq!(got, vec![vec![s("a1"), s("b10"), s("c100")]]);
}

/// ORDER BY DESC over a joined column with LIMIT on a leading-RIGHT chain.
/// (sqlite: b12|a2 / b11|NULL)
#[test]
fn right_chain_order_desc_limit() {
    let d = db("olim");
    let got = cells(&run(
        &d,
        "SELECT b.bv, a.av FROM a RIGHT JOIN b ON a.id = b.aid \
         LEFT JOIN c ON c.bid = b.id ORDER BY b.bv DESC LIMIT 2",
    ));
    assert_eq!(got, vec![vec![s("b12"), s("a2")], vec![s("b11"), None]]);
}

// ---- shapes that stay refused (clean error, never a wrong answer) -----------

fn err(d: &Database, sql: &str) -> String {
    format!("{}", d.query(sql, &[]).unwrap_err())
}

#[test]
fn non_leading_right_is_refused() {
    let d = db("ref1");
    let e = err(
        &d,
        "SELECT a.av FROM a JOIN b ON a.id = b.aid RIGHT JOIN c ON c.bid = b.id",
    );
    assert!(e.contains("multi-join chain"), "{e}");
    assert!(e.contains("FIRST"), "{e}");
}

#[test]
fn second_right_is_refused() {
    let d = db("ref2");
    let e = err(
        &d,
        "SELECT a.av FROM a RIGHT JOIN b ON a.id = b.aid RIGHT JOIN c ON c.bid = b.id",
    );
    assert!(e.contains("second RIGHT JOIN"), "{e}");
}

#[test]
fn full_in_chain_is_refused() {
    let d = db("ref3");
    // FULL trailing a leading RIGHT.
    let e1 = err(
        &d,
        "SELECT a.av FROM a RIGHT JOIN b ON a.id = b.aid FULL JOIN c ON c.bid = b.id",
    );
    assert!(e1.contains("FULL JOIN in a multi-join chain"), "{e1}");
    // FULL as the first join of a longer chain (no RIGHT present).
    let e2 = err(
        &d,
        "SELECT a.av FROM a FULL JOIN b ON a.id = b.aid JOIN c ON c.bid = b.id",
    );
    assert!(e2.contains("multi-join chain"), "{e2}");
}

#[test]
fn trailing_using_after_leading_right_is_refused() {
    let d = db("ref4");
    // c and b both have `id`; USING is refused after a leading RIGHT regardless.
    let e = err(
        &d,
        "SELECT b.bv FROM a RIGHT JOIN b ON a.id = b.aid JOIN c USING (id)",
    );
    assert!(e.contains("USING / NATURAL"), "{e}");
}
