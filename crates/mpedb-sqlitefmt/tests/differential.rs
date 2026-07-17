//! The fence: every row of every table, read natively and through the real
//! sqlite library, must match value-for-value. The generated database is
//! deliberately nasty: multi-page trees, overflow payloads, NULLs, negative
//! ints, floats, quoted identifiers, an ALTER-added column, a TEXT-PK rowid
//! table (PK in an index we ignore — scan order is rowid), and a composite
//! WITHOUT ROWID table (PK-first storage reordered back).

use mpedb_sqlitefmt::{SqliteFile, Value};
use rusqlite::Connection;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-sqlitefmt-tests")
        .join(format!("{name}-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&p);
    p
}

fn lib_value(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => Value::Int(i),
        V::Real(f) => Value::Float(f),
        V::Text(t) => Value::Text(std::str::from_utf8(t).unwrap().to_string()),
        V::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Read a whole table through the LIBRARY, in the same order the native
/// reader scans (rowid order / WITHOUT ROWID PK order = b-tree order).
fn lib_rows(conn: &Connection, table: &str, cols: &[String], order: &str) -> Vec<Vec<Value>> {
    let collist = cols
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn
        .prepare(&format!("SELECT {collist} FROM \"{table}\" ORDER BY {order}"))
        .unwrap();
    let n = cols.len();
    let rows = stmt
        .query_map([], |r| {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(lib_value(r.get_ref(i).unwrap()));
            }
            Ok(out)
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn every_row_of_every_table_matches_the_library() {
    let path = scratch("diff");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = DELETE;
        CREATE TABLE plain (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, note TEXT);
        CREATE TABLE "quoted name" ("weird col" TEXT, [bracketed] INTEGER, `ticked` REAL);
        CREATE TABLE textpk (k TEXT PRIMARY KEY, v INTEGER);
        CREATE TABLE worow (a INTEGER, b TEXT, c BLOB, PRIMARY KEY (b, a)) WITHOUT ROWID;
        CREATE INDEX idx_plain_name ON plain(name);
        "#,
    )
    .unwrap();

    // Nasty data: enough rows for multi-page trees, overflow-sized payloads,
    // every value class, negative rowids.
    let mut ins = conn
        .prepare("INSERT INTO plain (id, name, score, data, note) VALUES (?,?,?,?,?)")
        .unwrap();
    for i in 0..3000i64 {
        let name: Option<String> = if i % 7 == 0 {
            None
        } else {
            Some(format!("navn-{i}-{}", "x".repeat((i % 50) as usize)))
        };
        let score: Option<f64> = if i % 11 == 0 { None } else { Some(i as f64 * 0.25 - 100.0) };
        let data: Option<Vec<u8>> = if i % 13 == 0 {
            None
        } else {
            Some((0..(i % 300) as usize * 40).map(|b| (b * 31 + i as usize) as u8).collect())
        };
        let note = if i % 100 == 0 {
            // Overflow-sized text: far past one page.
            Some(format!("stor-{}", "æøå-".repeat(3000)))
        } else {
            None
        };
        ins.execute(rusqlite::params![i - 500, name, score, data, note])
            .unwrap();
    }
    drop(ins);
    conn.execute(
        r#"INSERT INTO "quoted name" VALUES ('a', 1, 1.5), (NULL, -2, NULL), ('b', 9223372036854775807, -0.0)"#,
        [],
    )
    .unwrap();
    let mut ins = conn.prepare("INSERT INTO textpk VALUES (?, ?)").unwrap();
    for i in 0..500i64 {
        ins.execute(rusqlite::params![format!("key-{i:04}"), i * 3]).unwrap();
    }
    drop(ins);
    let mut ins = conn.prepare("INSERT INTO worow VALUES (?, ?, ?)").unwrap();
    for i in 0..800i64 {
        let blob: Vec<u8> = (0..(i % 90) as usize * 60).map(|b| (b ^ i as usize) as u8).collect();
        ins.execute(rusqlite::params![i, format!("wk-{:03}", i % 400), blob])
            .unwrap();
    }
    drop(ins);
    // ALTER-added column: old rows have short records (native fills NULL).
    conn.execute("ALTER TABLE textpk ADD COLUMN added TEXT", []).unwrap();
    conn.execute("UPDATE textpk SET added = 'ny' WHERE v % 30 = 0", []).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;").unwrap();

    // ---- native vs library, every table -------------------------------
    let f = SqliteFile::open(&path).unwrap();
    let tables = f.tables().unwrap();
    assert_eq!(tables.len(), 4, "views/indexes skipped, all tables seen");

    for t in &tables {
        let order = if t.without_rowid {
            t.pk_order
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "rowid".to_string()
        };
        let expect = lib_rows(&conn, &t.name, &t.columns, &order);
        let mut got = Vec::new();
        f.scan_table(t, &mut |_rowid, vals| {
            got.push(vals);
            Ok(())
        })
        .unwrap();
        assert_eq!(got.len(), expect.len(), "row count for `{}`", t.name);
        for (i, (g, e)) in got.iter().zip(&expect).enumerate() {
            assert_eq!(g, e, "table `{}` row {i}", t.name);
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn wal_mode_is_refused_by_name() {
    let path = scratch("walmode");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (x);").unwrap();
    drop(conn);
    let err = match SqliteFile::open(&path) {
        Err(e) => e,
        Ok(_) => panic!("WAL-mode file was accepted"),
    };
    assert!(format!("{err}").contains("WAL-mode"), "{err}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn truncation_never_panics() {
    let path = scratch("trunc");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = DELETE; CREATE TABLE t (a, b); \
         INSERT INTO t VALUES (1, 'x'), (2, zeroblob(10000));",
    )
    .unwrap();
    drop(conn);
    let whole = std::fs::read(&path).unwrap();
    // Every prefix must either open+scan cleanly or error — never panic.
    for cut in (0..whole.len()).step_by(97) {
        if let Ok(f) = SqliteFile::from_bytes(whole[..cut].to_vec()) {
            if let Ok(tables) = f.tables() {
                for t in &tables {
                    let _ = f.scan_table(t, &mut |_, _| Ok(()));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}
