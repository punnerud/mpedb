use mpedb::{params, Config, Database, Value};
use std::time::Instant;

fn open(path: &str) -> Database {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(r#"
[database]
path = "{path}"
size_mb = 256
max_readers = 8
durability = "wal"
[[table]]
name = "users"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true
  [[table.column]]
  name = "age"
  type = "int64"
"#);
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn main() {
    let disk = std::env::args().nth(1).unwrap_or_else(|| "/mnt/xfs".into());
    let path = format!("{disk}/mpedb-batch-ab.mpedb");
    let batch = 100i64;
    let reps = 50i64;

    for (label, mode) in [("sql-exec", 0), ("typed-insert", 1)] {
        let db = open(&path);
        let ins = db.prepare("INSERT INTO users (id, email, age) VALUES ($1,$2,$3)").unwrap();
        let mut next = 0i64;
        for _ in 0..5 {
            let mut s = db.begin().unwrap();
            for i in 0..batch {
                let id = next+i;
                if mode == 0 {
                    s.execute(&ins, &params![id, format!("u{id}@e.com"), id%100]).unwrap();
                } else {
                    s.insert_row(0, &[Value::Int(id), Value::Text(format!("u{id}@e.com")), Value::Int(id%100)]).unwrap();
                }
            }
            s.commit().unwrap();
            next += batch;
        }
        let t0 = Instant::now();
        for _ in 0..reps {
            let mut s = db.begin().unwrap();
            for i in 0..batch {
                let id = next+i;
                if mode == 0 {
                    s.execute(&ins, &params![id, format!("u{id}@e.com"), id%100]).unwrap();
                } else {
                    s.insert_row(0, &[Value::Int(id), Value::Text(format!("u{id}@e.com")), Value::Int(id%100)]).unwrap();
                }
            }
            s.commit().unwrap();
            next += batch;
        }
        let e = t0.elapsed();
        let rows = reps * batch;
        println!("{label}: {:.0} rows/s  ({:?})", rows as f64 / e.as_secs_f64(), e);
    }
}
