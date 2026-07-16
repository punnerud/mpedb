//! `stream_query` must agree with `execute` for every plan shape — pinned
//! because the streaming path once accepted plans with joins/DISTINCT/
//! aggregates and ran them as a bare outer-table scan, silently returning
//! outer rows as if the rest of the plan did not exist (adversarial review
//! find). Those shapes must take the materializing fallback, which runs the
//! real executor.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

fn db() -> Tmp {
    let path = format!("/dev/shm/mpedb-streamjoin-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"dept\"\nprimary_key = [\"did\"]\n  \
         [[table.column]]\n  name = \"did\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"dname\"\n  type = \"text\"\n\
         [[table]]\nname = \"emp\"\nprimary_key = [\"eid\"]\n  \
         [[table.column]]\n  name = \"eid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"did\"\n  type = \"int64\""
    );
    let d = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    let di = d.prepare("INSERT INTO dept (did, dname) VALUES ($1, $2)").unwrap();
    d.execute(&di, &params![1i64, "eng"]).unwrap();
    d.execute(&di, &params![2i64, "sales"]).unwrap();
    let ei = d.prepare("INSERT INTO emp (eid, did) VALUES ($1, $2)").unwrap();
    for (e, dd) in [(10i64, 1i64), (11, 1), (12, 2)] {
        d.execute(&ei, &params![e, dd]).unwrap();
    }
    Tmp { db: d, path }
}

fn drain(d: &Database, hash: &mpedb::PlanHash) -> Vec<Vec<Value>> {
    let mut s = d.stream_query(hash, &[]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = s.next().unwrap() {
        out.push(row);
    }
    out
}

fn executed(d: &Database, hash: &mpedb::PlanHash) -> Vec<Vec<Value>> {
    match d.execute(hash, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Every plan shape the streaming fast path must NOT take bare: the stream
/// and the executor must return identical rows.
#[test]
fn stream_agrees_with_execute_for_every_plan_shape() {
    let d = db();
    for sql in [
        // join: the streaming path once returned emp's 3 rows as-is
        "SELECT emp.eid, dept.dname FROM emp JOIN dept ON emp.did = dept.did ORDER BY emp.eid",
        "SELECT emp.eid, dept.dname FROM emp LEFT JOIN dept ON emp.did = dept.did ORDER BY emp.eid",
        "SELECT count(*) FROM emp JOIN dept ON emp.did = dept.did",
        // DISTINCT: the streaming path once skipped the dedup
        "SELECT DISTINCT did FROM emp",
        // aggregate: the streaming path once returned raw rows for a count
        "SELECT count(*) FROM emp",
        "SELECT did, count(*) FROM emp GROUP BY did",
        // and the shape that SHOULD stream, as the control
        "SELECT eid FROM emp WHERE eid > 10",
    ] {
        let hash = d.prepare(sql).unwrap();
        assert_eq!(drain(&d, &hash), executed(&d, &hash), "stream != execute for: {sql}");
    }
}
