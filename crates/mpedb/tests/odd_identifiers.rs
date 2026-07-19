//! Quoted identifiers that a BARE identifier could not spell
//! (design/DESIGN-TABLE-CAP.md §7).
//!
//! The tokenizer has always accepted all three quoting spellings (`"x"`, `[x]`,
//! `` `x` ``, with `""` doubling). The schema validator then rejected the names
//! quoting exists FOR — `CREATE TABLE "weird tbl"(x INT)` failed with
//! `invalid table name` — which made the feature ornamental. `valid_identifier`
//! now refuses only what cannot be represented faithfully: the empty name,
//! control characters (NUL above all: the C-API hands names out as
//! NUL-terminated `const char*`), and the reserved `__mpedb` prefix.
//!
//! Everything here is checked against sqlite 3.45.1, which accepts all of it.

use mpedb::{Config, Database, ExecResult, Value};

fn config(tag: &str) -> (Config, std::path::PathBuf) {
    let path = if std::path::Path::new("/dev/shm").is_dir() { std::path::PathBuf::from("/dev/shm") } else { std::env::temp_dir() }
        .join(format!("mpedb-oddident-{tag}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
durability = "none"

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

fn rows_of(db: &Database, sql: &str) -> Vec<String> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Null => "NULL".into(),
                            Value::Int(n) => n.to_string(),
                            Value::Text(s) => s.clone(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            out.sort();
            out
        }
        other => panic!("expected rows, got {other:?} for {sql}"),
    }
}

fn sqlite_rows(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut st = conn.prepare(sql).unwrap();
    let n = st.column_count();
    let mut out: Vec<String> = st
        .query_map([], |row| {
            let mut parts = Vec::with_capacity(n);
            for i in 0..n {
                parts.push(match row.get::<_, rusqlite::types::Value>(i)? {
                    rusqlite::types::Value::Null => "NULL".to_string(),
                    rusqlite::types::Value::Integer(v) => v.to_string(),
                    rusqlite::types::Value::Text(s) => s,
                    other => format!("{other:?}"),
                });
            }
            Ok(parts.join("|"))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out.sort();
    out
}

/// The names that must now work, each with the SQL spelling used to reference
/// it. `""` doubling is the one escape both engines' tokenizers implement.
fn cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // (table DDL name, column DDL name, human label)
        (r#""weird tbl""#, r#""c d""#, "interior spaces"),
        (r#""tbl-with.punct!""#, r#""col+1""#, "punctuation"),
        (r#""1st table""#, r#""2nd col""#, "leading digit"),
        (r#""tabell_æøå""#, r#""kolonne_日本""#, "non-ASCII"),
        (r#""a""b""#, r#""x""y""#, "an embedded double quote, doubled"),
        (r#""  padded  ""#, r#""  c  ""#, "leading and trailing spaces"),
        (r#""   ""#, r#""    ""#, "whitespace-only (sqlite allows it)"),
        (r#""[bracketish]""#, r#""`tick`""#, "the other quote characters"),
    ]
}

#[test]
fn odd_quoted_identifiers_work_and_match_sqlite() {
    let (cfg, path) = config("basic");
    let db = Database::open_with_config(cfg).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();

    for (i, (tname, cname, label)) in cases().into_iter().enumerate() {
        let ddl = format!("CREATE TABLE {tname} (id INTEGER PRIMARY KEY, {cname} TEXT NOT NULL)");
        db.query(&ddl, &[]).unwrap_or_else(|e| panic!("{label}: create: {e}"));
        conn.execute_batch(&format!("{ddl};")).unwrap();

        for r in 1..=2i64 {
            let dml = format!("INSERT INTO {tname} (id, {cname}) VALUES ({r}, 'v{i}_{r}')");
            db.query(&dml, &[]).unwrap_or_else(|e| panic!("{label}: insert: {e}"));
            conn.execute_batch(&format!("{dml};")).unwrap();
        }
        for q in [
            format!("SELECT id, {cname} FROM {tname} ORDER BY id"),
            format!("SELECT {cname} FROM {tname} WHERE id = 2"),
            format!("SELECT count(*) FROM {tname} WHERE {cname} IS NOT NULL"),
        ] {
            assert_eq!(rows_of(&db, &q), sqlite_rows(&conn, &q), "{label}: {q}");
        }
        // An UPDATE and a DELETE through the quoted name touch the right table.
        let upd = format!("UPDATE {tname} SET {cname} = 'changed' WHERE id = 1");
        db.query(&upd, &[]).unwrap();
        conn.execute_batch(&format!("{upd};")).unwrap();
        let q = format!("SELECT id, {cname} FROM {tname} ORDER BY id");
        assert_eq!(rows_of(&db, &q), sqlite_rows(&conn, &q), "{label}: after update");
    }

    // A join between two oddly-named tables, on oddly-named columns.
    let q = r#"SELECT "weird tbl"."c d", "a""b"."x""y" FROM "weird tbl" JOIN "a""b" ON "weird tbl".id = "a""b".id ORDER BY "weird tbl".id"#;
    assert_eq!(rows_of(&db, q), sqlite_rows(&conn, q), "join over odd names");

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn identifiers_that_stay_refused() {
    let (cfg, path) = config("refused");
    let db = Database::open_with_config(cfg).unwrap();

    // Empty: refused at the TOKENIZER, exactly as sqlite refuses `""`.
    assert!(db.query(r#"CREATE TABLE "" (id INTEGER PRIMARY KEY)"#, &[]).is_err());
    assert!(db.query("CREATE TABLE [] (id INTEGER PRIMARY KEY)", &[]).is_err());

    // Control characters. NUL is the one that would be a WRONG ANSWER rather
    // than an eyesore: the C-API returns names as NUL-terminated `const char*`,
    // so `sqlite3_column_name` would hand back a TRUNCATED, different name.
    for bad in ["nul\u{0}name", "line\nbreak", "carriage\rreturn", "tab\tsep", "del\u{7f}"] {
        let sql = format!("CREATE TABLE \"{bad}\" (id INTEGER PRIMARY KEY)");
        let err = db.query(&sql, &[]).unwrap_err().to_string();
        assert!(err.contains("invalid table name"), "for {bad:?}: {err}");
        let sql = format!("CREATE TABLE ok_t (id INTEGER PRIMARY KEY, \"{bad}\" TEXT)");
        assert!(db.query(&sql, &[]).is_err(), "column {bad:?} must be refused");
    }

    // The reserved internal prefix.
    assert!(db.query(r#"CREATE TABLE "__mpedb_x" (id INTEGER PRIMARY KEY)"#, &[]).is_err());

    // Too long: 256 bytes is over MAX_IDENTIFIER_LEN, 255 is exactly at it.
    let at = "a".repeat(255);
    let over = "a".repeat(256);
    db.query(&format!("CREATE TABLE \"{at}\" (id INTEGER PRIMARY KEY)"), &[]).unwrap();
    assert!(db.query(&format!("CREATE TABLE \"{over}\" (id INTEGER PRIMARY KEY)"), &[]).is_err());
    // 134 chars is what Django's generated m2m through-table name needs and the
    // old 128-byte limit refused; it is the reason the limit moved.
    let django = format!("app_{}", "m".repeat(130));
    assert_eq!(django.len(), 134);
    db.query(&format!("CREATE TABLE \"{django}\" (id INTEGER PRIMARY KEY)"), &[]).unwrap();

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
