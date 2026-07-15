//! Just the bulk blob write, nothing else — so a syscall trace attributes to
//! IT and not to the point-op suite that shares the `--io` run.
//!
//! Usage: bulk_only <dir> <mib> <value_bytes> [durability]

use mpedb::{params, Config, Database, Value};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let mib: usize = a[2].parse().unwrap();
    let vb: usize = a[3].parse().unwrap();
    let dur = a.get(4).cloned().unwrap_or_else(|| "none".into());

    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bulk.mpedb");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("bulk.mpedb-wal"));
    let cfg = Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = {}
max_readers = 64
durability = "{dur}"

[[table]]
name = "blobs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "data"
  type = "blob"
  nullable = false
"#,
        path.display(),
        (mib * 12).max(64)
    ))
    .unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    let ins = db.prepare("INSERT INTO blobs (id, data) VALUES ($1, $2)").unwrap();
    let buf = vec![0xb1u8; vb];
    let rows = (mib * 1024 * 1024 / vb) as i64;

    // `raw` bypasses the SQL layer entirely (no plan lookup, no param
    // validation, no expression IR) and calls the engine's typed row API. The
    // difference between the two arms is what the SQL layer costs per row.
    let raw = a.get(5).map(|s| s == "raw").unwrap_or(false);
    let tid = db.schema().table_id("blobs").unwrap();

    let t0 = std::time::Instant::now();
    let mut id = 0i64;
    while id < rows {
        let n = (256).min(rows - id);
        let mut s = db.begin().unwrap();
        for k in 0..n {
            if raw {
                s.insert_row(tid, &[Value::Int(id + k), Value::Blob(buf.clone())])
                    .unwrap();
            } else {
                s.execute(&ins, &params![id + k, Value::Blob(buf.clone())]).unwrap();
            }
        }
        s.commit().unwrap();
        id += n;
    }
    let el = t0.elapsed().as_secs_f64();
    eprintln!(
        "bulk_only{}: {rows} rows x {vb} B = {:.1} MiB in {el:.3}s = {:.0} MiB/s",
        if raw { " [raw]" } else { " [sql]" },
        (rows as usize * vb) as f64 / 1048576.0,
        (rows as usize * vb) as f64 / 1048576.0 / el
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("bulk.mpedb-wal"));
}
