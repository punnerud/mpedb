use mpedb::{Config, Database, Value};
use std::time::Instant;

fn main() {
    let path = "/mnt/xfs/wal-size-ab.mpedb";
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
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let batch = 100i64;
    let mut next = 0i64;
    // warm
    for _ in 0..10 {
        let mut s = db.begin().unwrap();
        for i in 0..batch {
            let id = next+i;
            s.insert_row(0, &[Value::Int(id), Value::Text(format!("u{id}@e.com")), Value::Int(id%100)]).unwrap();
        }
        s.commit().unwrap();
        next += batch;
    }
    let mut insert_ns = 0u128;
    let mut commit_ns = 0u128;
    let mut wal_sizes = Vec::new();
    for _ in 0..30 {
        let mut s = db.begin().unwrap();
        let t0 = Instant::now();
        for i in 0..batch {
            let id = next+i;
            s.insert_row(0, &[Value::Int(id), Value::Text(format!("u{id}@e.com")), Value::Int(id%100)]).unwrap();
        }
        insert_ns += t0.elapsed().as_nanos();
        let t1 = Instant::now();
        s.commit().unwrap();
        commit_ns += t1.elapsed().as_nanos();
        next += batch;
        if let Ok(meta) = std::fs::metadata(format!("{path}-wal")) {
            wal_sizes.push(meta.len());
        }
    }
    let n = 30f64;
    println!("insert {:.0} µs/batch  commit {:.0} µs/batch  total {:.0} µs",
        insert_ns as f64 / n / 1000.0,
        commit_ns as f64 / n / 1000.0,
        (insert_ns+commit_ns) as f64 / n / 1000.0);
    if wal_sizes.len() >= 2 {
        let mut deltas: Vec<u64> = wal_sizes.windows(2).map(|w| w[1]-w[0]).collect();
        deltas.sort();
        println!("wal delta median {} bytes (last {})", deltas[deltas.len()/2], wal_sizes.last().unwrap());
    }
}
