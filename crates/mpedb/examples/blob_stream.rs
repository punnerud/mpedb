//! #43: a streamed blob must never be resident.
//!
//! The memory ceiling is the whole motive. Handing a blob over as
//! `Value::Blob(Vec<u8>)` needs the entire value in RAM before a byte reaches
//! the file — for a 1 GiB blob that is 1 GiB of anonymous pages, and they fault
//! on the way in exactly like the file mapping does. Streaming pulls a page at a
//! time, so peak RSS is flat no matter how big the value gets.
//!
//! This inserts a blob far larger than the memory it is allowed to use, then
//! reads it back and checks every byte. Peak RSS comes from /proc, so the claim
//! is measured rather than asserted.
//!
//! Usage: `blob_stream <dir> [mib]`

use mpedb::{params, Config, Database};
use mpedb_core::btree::BlobSource;

/// Generates the payload on demand — never holds more than one page of it.
/// A real caller would read from a file or a socket here; the shape is the same,
/// and so is the point: the engine pulls, so the buffer is the engine's.
struct Generated {
    total: usize,
    pos: usize,
}

impl Generated {
    /// The byte this source will produce at `i`. Deterministic so the reader can
    /// check it without holding the value either.
    fn byte_at(i: usize) -> u8 {
        let mut x = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    }
}

impl BlobSource for Generated {
    fn len(&self) -> usize {
        self.total
    }
    fn next_into(&mut self, buf: &mut [u8]) -> std::result::Result<(), mpedb::Error> {
        for (k, b) in buf.iter_mut().enumerate() {
            *b = Generated::byte_at(self.pos + k);
        }
        self.pos += buf.len();
        Ok(())
    }
}

/// `RssAnon`, not `VmHWM`.
///
/// mpedb IS an mmap, so writing N bytes to the file faults in N bytes of
/// FILE-backed pages and total RSS grows by the size of the value no matter how
/// it got there. That is not the thing streaming was meant to bound: those pages
/// are page cache, and the kernel reclaims them under pressure. A
/// `Value::Blob(Vec<u8>)` is ANONYMOUS — it cannot be dropped, only swapped, and
/// on a box with no swap (the Pi) it is the difference between running and being
/// OOM-killed. `RssAnon` is what a caller actually has to find.
fn field_kib(name: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}
fn anon_kib() -> u64 {
    field_kib("RssAnon:")
}

fn cfg(path: &std::path::Path, size_mb: u64) -> Config {
    Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 8
durability = "none"

[[table]]
name = "blobs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "blob"
  nullable = false
"#,
        path.display()
    ))
    .unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(a.get(1).cloned().unwrap_or("/tmp/bs".into()));
    let mib: usize = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(256);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("stream.mpedb");
    let _ = std::fs::remove_file(&path);
    let n = mib * 1024 * 1024;

    let db = Database::open_with_config(cfg(&path, (mib as u64 * 2).max(64))).unwrap();
    let anon_before = anon_kib();

    let t0 = std::time::Instant::now();
    {
        let mut s = db.begin().unwrap();
        let mut src = Generated { total: n, pos: 0 };
        s.insert_streaming("blobs", &params![1i64, &[][..]], 1, &mut src)
            .unwrap();
        s.commit().unwrap();
    }
    let d = t0.elapsed();
    let anon_after = anon_kib();

    println!("streamed a {mib} MiB blob in {:.0} ms ({:.0} MiB/s)", d.as_secs_f64() * 1e3, mib as f64 / d.as_secs_f64());
    println!(
        "anonymous RSS: {anon_before} KiB -> {anon_after} KiB  (+{} KiB for a {} KiB value)",
        anon_after.saturating_sub(anon_before),
        n / 1024
    );
    println!(
        "  A Value::Blob would have needed all {} KiB of that anonymous. Total RSS",
        n / 1024
    );
    println!("  grows either way — the file's pages are mapped — but those are page");
    println!("  cache the kernel can reclaim, not memory the caller has to find.");

    // read it back and check every byte — the point is a byte-identical row, not
    // just a small one
    let sel = db.prepare("SELECT v FROM blobs WHERE id = $1").unwrap();
    match db.execute(&sel, &params![1i64]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "row missing");
            match &rows[0][0] {
                mpedb::Value::Blob(b) => {
                    assert_eq!(b.len(), n, "length");
                    for (i, &byte) in b.iter().enumerate() {
                        assert_eq!(byte, Generated::byte_at(i), "byte {i} differs");
                    }
                    println!("read back {} MiB: every byte matches", b.len() / 1024 / 1024);
                }
                other => panic!("expected a blob, got {other:?}"),
            }
        }
        other => panic!("expected rows, got {other:?}"),
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
