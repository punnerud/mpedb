//! Copy a raw file into mpedb as a blob, in one call, streamed so the file is
//! never resident.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

/// `/dev/shm` when present (fast tmpfs, mpedb's habitat), else the platform
/// temp dir — keeps the scratch path portable to macOS, where `/dev/shm` does
/// not exist (#66).
fn scratch_path(name: String) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    dir.join(name).to_string_lossy().into_owned()
}

fn db() -> Tmp {
    let path = scratch_path(format!("mpedb-insfile-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\n\
         [[table]]\nname = \"files\"\nprimary_key = [\"id\"]\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"data\"\n  type = \"blob\"\n  nullable = false"
    );
    Tmp { db: Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap(), path }
}

fn anon_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("RssAnon:"))
            .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok())))
        .unwrap_or(0)
}

#[test]
fn copy_a_file_into_mpedb_and_read_it_back() {
    // an 8 MiB file with a checkable pattern
    let n = 8 * 1024 * 1024usize;
    let src_path = scratch_path(format!("mpedb-src-{}.bin", std::process::id()));
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        let chunk: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let mut w = 0;
        while w < n { let take = (n - w).min(chunk.len()); f.write_all(&chunk[..take]).unwrap(); w += take; }
    }
    let expected = |i: usize| (i % 4096) as u8;

    let d = db();
    let before = anon_kib();
    {
        let mut s = d.begin().unwrap();
        s.insert_file("files", &params![1i64, &[][..]], 1, &src_path).unwrap();
        s.commit().unwrap();
    }
    let after = anon_kib();

    // read it back, every byte
    let sel = d.prepare("SELECT data FROM files WHERE id = $1").unwrap();
    match d.execute(&sel, &params![1i64]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Blob(b) => {
                assert_eq!(b.len(), n, "length");
                for (i, &byte) in b.iter().enumerate() {
                    assert_eq!(byte, expected(i), "byte {i}");
                }
            }
            v => panic!("{v:?}"),
        },
        o => panic!("{o:?}"),
    }
    // the file was streamed: anonymous memory grew by far less than its size
    assert!(after.saturating_sub(before) < (n as u64 / 1024) / 4,
        "insert_file should not hold the file resident: +{} KiB for an {} KiB file",
        after.saturating_sub(before), n / 1024);
    let _ = std::fs::remove_file(&src_path);
}
