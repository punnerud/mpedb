//! **The same engine, once per filesystem class.** Run it on each volume:
//!
//! ```text
//! MPEDB_TEST_DIR=/mnt/xfs/mpedb-scratch  cargo test -p mpedb --test filesystem_class
//! MPEDB_TEST_DIR=/mnt/ext4/mpedb-scratch cargo test -p mpedb --test filesystem_class
//! ```
//!
//! Without the variable it runs wherever the suite runs (`/dev/shm`), which
//! covers the tmpfs class. All three are legitimate homes for a database and
//! all three must be byte-exact; what differs is the *storage-level* answer
//! underneath, and this file makes that answer explicit instead of assumed.
//!
//! The difference that matters is **reflink** (`FICLONERANGE`): XFS and btrfs
//! clone a range as pure metadata, ext4 cannot and must copy. `insert_file`'s
//! zero-copy import is designed on exactly that split
//! (design/DESIGN-BLOBEXTENT.md §9) and is **not built yet** — so this file
//! pins the premise now, before the code that will depend on it exists.
//!
//! It does NOT try to assert "nothing is shared today" from the outside, and
//! the reason is worth writing down because it costs an hour to rediscover:
//! mpedb preallocates its whole file at open, so `st_blocks` is flat no
//! matter what the writes do. Growth-on-disk cannot see an 8 MiB value
//! arrive, let alone tell a copy from a clone. When §9 lands, the instrument
//! is `REFLINK_HITS` in the leak ledger — which is exactly why §9 reserves
//! it — asserted here as hits > 0 on a clone-capable volume and 0 on ext4.

// `FICLONERANGE` is an `ioctl` on a raw fd: Unix by construction, and there is
// no Windows analogue to classify (ReFS block cloning is a different API with
// different semantics). An empty test binary is the honest result there, not a
// shimmed one that would report a filesystem class it never probed.
#![cfg(unix)]

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mpedb::{Config, Database, ExecResult, Value};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// `FICLONERANGE`: clone `len` bytes from the start of `src` into `dst`.
/// `Ok(())` means the filesystem cloned it as metadata; `EOPNOTSUPP`/`EXDEV`
/// mean it will not, which is an answer, not a failure.
fn try_clone_range(src: &Path, dst: &Path, len: u64) -> io::Result<()> {
    #[repr(C)]
    struct FileCloneRange {
        src_fd: i64,
        src_offset: u64,
        src_length: u64,
        dest_offset: u64,
    }
    // _IOW(0x94, 13, struct file_clone_range)
    const FICLONERANGE: libc::c_ulong = 0x4020_940d;

    let s = std::fs::File::open(src)?;
    let d = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(dst)?;
    let arg = FileCloneRange {
        src_fd: s.as_raw_fd() as i64,
        src_offset: 0,
        src_length: len,
        dest_offset: 0,
    };
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONERANGE, &arg as *const _) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Does the filesystem holding `dir` support reflink? Answered by doing it,
/// not by parsing a mount table: a filesystem type is not the question, the
/// mkfs options are (XFS only clones when made with `reflink=1`).
fn reflink_supported(dir: &Path) -> bool {
    let tag = std::process::id();
    let src = dir.join(format!("mpedb-fsclass-probe-{tag}.src"));
    let dst = dir.join(format!("mpedb-fsclass-probe-{tag}.dst"));
    // One filesystem block is the smallest thing a clone can be asked about;
    // 64 KiB is comfortably above every block size we might meet.
    let ok = std::fs::write(&src, vec![0xA5u8; 64 * 1024]).is_ok()
        && try_clone_range(&src, &dst, 64 * 1024).is_ok();
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    ok
}

fn open(tag: &str) -> (Database, PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-fsclass-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 64
max_readers = 8

[[table]]
name = "docs"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "blob"
  nullable = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, path)
}

/// Deterministic bytes, so a wrong answer is a wrong answer and not a
/// coincidence: xorshift over the row id.
fn payload(id: i64, len: usize) -> Vec<u8> {
    let mut x = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xFF) as u8
        })
        .collect()
}

/// Big values must survive the round trip byte for byte on whatever storage
/// this is. Overflow chains and extent runs are where a filesystem's block
/// size or allocation behaviour could plausibly leak into the engine, so the
/// sizes straddle the page size in both directions.
#[test]
fn blobs_round_trip_on_this_filesystem() {
    let (db, path) = open("roundtrip");
    let sizes = [1, 4095, 4096, 4097, 64 * 1024, 1024 * 1024];
    for (i, &len) in sizes.iter().enumerate() {
        let id = i as i64 + 1;
        db.query(
            "INSERT INTO docs (id, body) VALUES ($1, $2)",
            &[Value::Int(id), Value::Blob(payload(id, len))],
        )
        .unwrap();
    }
    for (i, &len) in sizes.iter().enumerate() {
        let id = i as i64 + 1;
        let got = db.query("SELECT body FROM docs WHERE id = $1", &[Value::Int(id)]).unwrap();
        let ExecResult::Rows { rows, .. } = got else { panic!("expected rows") };
        assert_eq!(rows.len(), 1, "id {id} missing");
        match &rows[0][0] {
            Value::Blob(b) => assert_eq!(b, &payload(id, len), "id {id} ({len} bytes) came back wrong"),
            other => panic!("id {id}: expected a blob, got {other:?}"),
        }
    }
    // Freeing them must return the space, not just unlink the rows: the
    // engine's own accounting is the check, and it is filesystem-independent
    // by design — this asserts that it stays so.
    for i in 0..sizes.len() as i64 {
        db.query("DELETE FROM docs WHERE id = $1", &[Value::Int(i + 1)]).unwrap();
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The premise `insert_file`'s zero-copy import will be built on
/// (DESIGN-BLOBEXTENT §9), pinned before the code exists: on this volume,
/// does `FICLONERANGE` actually work?
///
/// Deliberately not an assertion about which volume you are on — that would
/// only encode this box's mount table. It asserts the answer is CONSISTENT:
/// a filesystem that clones a probe file must clone a second one, so a
/// future implementation cannot be built on an intermittent capability. The
/// class is printed so a run on each volume shows both arms.
#[test]
fn reflink_capability_is_consistent() {
    let dir = mpedb_testkit::scratch_base();
    let first = reflink_supported(&dir);
    let second = reflink_supported(&dir);
    assert_eq!(
        first, second,
        "{}: FICLONERANGE answered differently on two identical probes — a \
         zero-copy import cannot be built on that",
        dir.display()
    );
    println!(
        "filesystem class: {} — reflink {}",
        dir.display(),
        if first { "SUPPORTED (clone path)" } else { "unsupported (copy fallback)" }
    );
}
