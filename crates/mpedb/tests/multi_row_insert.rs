use mpedb::{Config, Database, Value};
use std::sync::atomic::{AtomicU64, Ordering};
static U: AtomicU64 = AtomicU64::new(0);
fn open() -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm".into() } else { std::env::temp_dir().display().to_string() };
    let path = format!("{dir}/mpedb-batch-{}-{}.mpedb", std::process::id(), U.fetch_add(1, Ordering::Relaxed));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [[table]]\nname = \"users\"\nprimary_key = [\"id\"]\n\
           [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
           [[table.column]]\n  name = \"email\"\n  type = \"text\"\n\
           [[table.column]]\n  name = \"age\"\n  type = \"int64\"\n"
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

#[test]
fn multi_row_insert_100() {
    let (db, path) = open();
    let mut parts = Vec::new();
    let mut pi = 1;
    for _ in 0..100 {
        parts.push(format!("(${pi}, ${}, ${})", pi+1, pi+2));
        pi += 3;
    }
    let sql = format!("INSERT INTO users (id, email, age) VALUES {}", parts.join(","));
    let h = db.prepare(&sql).expect("prepare multi");
    let mut vals = Vec::with_capacity(300);
    for i in 0..100i64 {
        vals.push(Value::Int(i));
        vals.push(Value::Text(format!("u{i}@e.com")));
        vals.push(Value::Int(i % 100));
    }
    let mut s = db.begin().unwrap();
    s.execute(&h, &vals).expect("exec");
    s.commit().unwrap();
    let r = db.query("SELECT count(*) FROM users", &[]).unwrap();
    eprintln!("ok count={r:?}");
    let _ = std::fs::remove_file(path);
}
