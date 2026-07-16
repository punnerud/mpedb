//! #48: a non-unique secondary index (`indexed = true`) allows duplicate values
//! where `unique = true` would reject them, and the index stays page-consistent.
use mpedb::{params, Config, Database};
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

fn db(tag: &str, col_attr: &str) -> Tmp {
    let path = format!("/dev/shm/mpedb-nuidx-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"orders\"\nprimary_key = [\"oid\"]\n  \
         [[table.column]]\n  name = \"oid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cid\"\n  type = \"int64\"\n  nullable = false\n  {col_attr}"
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    Tmp { db, path }
}

/// The headline: `indexed = true` builds a lookup index that ALLOWS duplicates.
/// Two orders for the same customer both insert; the index is maintained; page
/// accounting (which walks every index tree) stays consistent.
#[test]
fn indexed_allows_duplicates_and_stays_consistent() {
    let d = db("dup", "indexed = true");
    let ins = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    d.execute(&ins, &params![1i64, 100i64]).unwrap();
    d.execute(&ins, &params![2i64, 100i64]).unwrap(); // same cid — must succeed
    d.execute(&ins, &params![3i64, 200i64]).unwrap();
    d.verify().expect("index + page accounting consistent after duplicate inserts");
    // delete one of the duplicates; the other and its index entry survive
    let del = d.prepare("DELETE FROM orders WHERE oid = $1").unwrap();
    d.execute(&del, &params![1i64]).unwrap();
    d.verify().expect("consistent after deleting one of a duplicate pair");
}

/// The guard: `unique = true` still rejects the second duplicate. The two must
/// not have collapsed into one behaviour by the composite-key change.
#[test]
fn unique_still_rejects_duplicates() {
    let d = db("uniq", "unique = true");
    let ins = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    d.execute(&ins, &params![1i64, 100i64]).unwrap();
    let err = d.execute(&ins, &params![2i64, 100i64]).unwrap_err();
    assert!(
        matches!(err, mpedb::Error::UniqueViolation { .. }),
        "a UNIQUE column must still reject a duplicate: {err:?}"
    );
}
