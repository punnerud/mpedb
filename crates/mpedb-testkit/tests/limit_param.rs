//! Parameterized `LIMIT ?` / `OFFSET ?` (PLAN_FORMAT 62), differentially
//! against the BUNDLED sqlite oracle. The clause is one shared resolve
//! function in the executor, so the battery covers the resolve semantics
//! exhaustively (every sign/NULL combination) on the plain path, then touches
//! each distinct plan shape that carries its own LIMIT tail (top-K,
//! compound, aggregate) once with a parameter.

use mpedb::{Config, Database, ExecResult, Value};
use mpedb_testkit::{toml_path as crate_toml_path, TempDir};
use rusqlite::Connection;

const SCHEMA: &str = r#"
[[table]]
name = "t"
primary_key = ["pk"]

  [[table.column]]
  name = "pk"
  type = "int64"

  [[table.column]]
  name = "x"
  type = "int64"
"#;

fn open_pair(dir: &TempDir) -> (Database, Connection) {
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 8\n{}",
        // ESCAPED: `TempDir::path()` is protected against backslashes, but
        // JOINING onto it is not — a Windows build spells that separator `\\`
        // and TOML reads it as an escape, so the config fails to parse with a
        // message about unicode escapes (#159). `scratch_base` guards the base;
        // everything built from it still needs this.
        crate_toml_path(dir.db_path("limit")),
        SCHEMA
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (pk INTEGER PRIMARY KEY, x INTEGER) STRICT")
        .unwrap();
    for pk in 0..10i64 {
        db.query("INSERT INTO t (pk, x) VALUES ($1, $2)", &[Value::Int(pk), Value::Int(pk % 3)])
            .unwrap();
        conn.execute("INSERT INTO t (pk, x) VALUES (?1, ?2)", (pk, pk % 3)).unwrap();
    }
    (db, conn)
}

/// One statement, one param set, both engines: identical single-column rows,
/// or BOTH refusing. The refusal texts differ by design; agreement is on the
/// outcome class, never on the message.
fn check(db: &Database, conn: &Connection, sql: &str, params: &[Value]) {
    let m = match db.query(sql, params) {
        Ok(ExecResult::Rows { rows, .. }) => {
            Ok(rows.into_iter().map(|r| r.into_iter().collect::<Vec<_>>()).collect::<Vec<_>>())
        }
        Ok(other) => panic!("non-rows result for {sql}: {other:?}"),
        Err(e) => Err(e.to_string()),
    };
    let s: Result<Vec<Vec<Value>>, String> = (|| {
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        for (i, p) in params.iter().enumerate() {
            match p {
                Value::Int(v) => stmt.raw_bind_parameter(i + 1, v).map_err(|e| e.to_string())?,
                Value::Null => stmt
                    .raw_bind_parameter(i + 1, rusqlite::types::Null)
                    .map_err(|e| e.to_string())?,
                other => panic!("battery binds ints and NULL only, got {other:?}"),
            }
        }
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut r = Vec::new();
            let mut i = 0;
            while let Ok(cell) = row.get_ref(i) {
                r.push(match cell {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(v) => Value::Int(v),
                    other => panic!("int-only battery, got {other:?}"),
                });
                i += 1;
            }
            out.push(r);
        }
        Ok(out)
    })();
    match (&m, &s) {
        (Ok(mr), Ok(sr)) => {
            assert_eq!(mr, sr, "row divergence on {sql} with {params:?}");
        }
        (Err(_), Err(_)) => {} // both refuse: agreement (texts differ by design)
        _ => panic!("outcome divergence on {sql} with {params:?}:\n  mpedb: {m:?}\n  sqlite: {s:?}"),
    }
}

#[test]
fn limit_offset_param_resolve_semantics() {
    let dir = TempDir::new("limit-param").unwrap();
    let (db, conn) = open_pair(&dir);
    // Every sign/NULL combination through the ONE resolve function. The
    // interesting entries: negative LIMIT = no bound, negative OFFSET = skip
    // nothing, NULL = a loud refusal in both engines, and bounds past the
    // table = clean saturation.
    let vals = [
        Value::Int(-5),
        Value::Int(-1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(3),
        Value::Int(9),
        Value::Int(15),
        Value::Int(i64::MAX),
        Value::Null,
    ];
    for l in &vals {
        check(&db, &conn, "SELECT pk FROM t ORDER BY pk LIMIT ?", std::slice::from_ref(l));
        for o in &vals {
            check(
                &db,
                &conn,
                "SELECT pk FROM t ORDER BY pk LIMIT ? OFFSET ?",
                &[l.clone(), o.clone()],
            );
        }
    }
    // OFFSET alone is only reachable with a literal LIMIT in the grammar;
    // pin the pairing both ways round.
    for o in &vals {
        check(&db, &conn, "SELECT pk FROM t ORDER BY pk LIMIT 4 OFFSET ?", std::slice::from_ref(o));
        check(&db, &conn, "SELECT pk FROM t ORDER BY pk LIMIT ? OFFSET 2", std::slice::from_ref(o));
    }
}

#[test]
fn limit_param_reaches_every_tail_shape() {
    let dir = TempDir::new("limit-shapes").unwrap();
    let (db, conn) = open_pair(&dir);
    // Top-K (ORDER BY non-PK + LIMIT), unsorted stream-and-stop, compound
    // tail, aggregate/GROUP BY tail, and DISTINCT — each is a separate
    // skip/take site in the executor, all funneling into the same resolve.
    for (sql, n) in [
        ("SELECT pk FROM t ORDER BY x, pk LIMIT ?", 2i64),
        ("SELECT pk FROM t LIMIT ?", 4),
        ("SELECT pk FROM t WHERE pk < 5 UNION SELECT pk FROM t WHERE pk > 7 ORDER BY pk LIMIT ?", 3),
        ("SELECT x, count(*) FROM t GROUP BY x ORDER BY x LIMIT ?", 2),
        ("SELECT DISTINCT x FROM t ORDER BY x LIMIT ?", 2),
    ] {
        check(&db, &conn, sql, &[Value::Int(n)]);
        check(&db, &conn, sql, &[Value::Int(-1)]);
        check(&db, &conn, sql, &[Value::Int(0)]);
        // Hostile bound: i64::MAX through the top-K/compound/aggregate tails
        // once sized a preallocation (capacity-overflow panic — review
        // finding); the heap capacity is clamped, the semantics unbounded.
        check(&db, &conn, sql, &[Value::Int(i64::MAX)]);
    }
    // The same prepared plan, executed with different bound values: the plan
    // must not bake the first execution's bound in.
    let sql = "SELECT pk FROM t ORDER BY pk LIMIT ?";
    for n in [1i64, 7, 0, 10] {
        check(&db, &conn, sql, &[Value::Int(n)]);
    }
}
