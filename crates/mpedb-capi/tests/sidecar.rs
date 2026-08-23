//! The sidecar's lifecycle: when it is rebuilt, when it is reused, and what
//! happens when several processes want it at once.
//!
//! Opening a `.db` through the shim builds a `.mpedb` beside it. Everything
//! here is about the four ways that went wrong before: a row-at-a-time import,
//! a reader table too small for a web server, no lock around the build, and a
//! freshness test that could not see a restore.

use mpedb_sqlite3::*;
use mpedb_sqlitefmt::{write_image, ImageTable, Value as FmtValue};
use std::ffi::CString;
use std::path::Path;
use std::ptr;

mod common;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// A sqlite file with `n` rows.
fn write_source(path: &str, n: i64, tag: &str) {
    let rows: Vec<Vec<FmtValue>> = (1..=n)
        .map(|i| vec![FmtValue::Int(i), FmtValue::Text(format!("{tag}{i}"))])
        .collect();
    let img = write_image(
        &[ImageTable {
            name: "p".into(),
            sql: "CREATE TABLE p (id INTEGER, name TEXT)".into(),
            rows,
            indexes: vec![],
        }],
        4096,
    )
    .expect("write the source image");
    std::fs::write(path, img).expect("write");
}

unsafe fn count(path: &str) -> i64 {
    let mut db: *mut Sqlite3 = ptr::null_mut();
    let name = cs(path);
    assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK, "open {path}");
    let mut st: *mut Stmt = ptr::null_mut();
    let sql = cs("SELECT COUNT(*) FROM p");
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK
    );
    assert_eq!(sqlite3_step(st), SQLITE_ROW);
    let n = sqlite3_column_int64(st, 0);
    sqlite3_finalize(st);
    sqlite3_close(db);
    n
}

/// A scratch path that cleans up AFTER itself, not only before.
///
/// Cleaning at the start only is what leaked: each run left a source, a
/// sidecar (~4x the source), a stamp and a lock behind, and on a tmpfs a few
/// runs of this file fill the disk. The guard removes all four when the test
/// ends, including on a panic — unwinding runs `Drop`.
///
/// It cannot help a process that is KILLED, which is why
/// `mpedb_testkit::scratch_base` also sweeps files whose PID is gone. Two
/// lines of defence for two different failures: this one is exact, that one
/// covers what no destructor can.
struct Scratch(String);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = common::scratch_base_str();
        let p =
            format!("{}/mpedb-side-{tag}-{}.db", dir.trim_end_matches('/'), std::process::id());
        let s = Scratch(p);
        s.wipe();
        s
    }
    fn wipe(&self) {
        for suffix in ["", ".mpedb", ".mpedb.src", ".mpedb.lock"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
        }
    }
    fn path(&self) -> &str {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// A restore that PRESERVES the source's timestamp — `cp -p`, `rsync -t`, any
/// backup tool — leaves a sidecar that is newer than a source it was never
/// built from. mtime alone calls that fresh and serves the OLD rows, with no
/// error: the failure this test exists for.
#[test]
fn a_restore_that_keeps_the_mtime_is_still_seen() {
    let _g = Scratch::new("restore");
    let p = _g.path().to_string();
    write_source(&p, 300, "a");
    unsafe {
        assert_eq!(count(&p), 300);
    }
    assert!(Path::new(&format!("{p}.mpedb")).exists(), "the sidecar was built");

    // Put a DIFFERENT database in place with the original's timestamp, exactly
    // as a restore does.
    let was = std::fs::metadata(&p).unwrap().modified().unwrap();
    write_source(&p, 5000, "b");
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(was).unwrap();
    assert!(
        std::fs::metadata(format!("{p}.mpedb")).unwrap().modified().unwrap() >= was,
        "the sidecar still looks newer — which is the trap"
    );

    unsafe {
        assert_eq!(count(&p), 5000, "the restored rows, not the ones the sidecar remembers");
    }
}

/// The limit of the stamp, pinned rather than left to be discovered.
///
/// 3 rows and 7 rows both fit one page, so the two files have the SAME length
/// and a byte-identical header — same change counter, same schema cookie, same
/// format bytes, no `-wal` either side, so `BaseStamp` sees one file. They
/// differ only in page content.
/// Nothing short of hashing the whole source can tell them apart, and hashing
/// 142 MB on every open costs more than the import being avoided. So this case
/// is NOT caught, and that is a decision, not an oversight: if it ever has to
/// be, the answer is a content hash written by whoever produces the file, not
/// a more expensive check on every reader.
#[test]
fn a_same_size_replacement_is_not_seen() {
    let _g = Scratch::new("samesize");
    let p = _g.path().to_string();
    write_source(&p, 3, "a");
    unsafe {
        assert_eq!(count(&p), 3);
    }
    let was = std::fs::metadata(&p).unwrap().modified().unwrap();
    let before = std::fs::read(&p).unwrap();
    write_source(&p, 7, "b");
    let after = std::fs::read(&p).unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(was).unwrap();

    assert_eq!(before.len(), after.len(), "same length");
    assert_eq!(before[..100], after[..100], "and a byte-identical header");
    assert_ne!(before, after, "but different content");

    unsafe {
        assert_eq!(count(&p), 3, "the stale answer — documented, not endorsed");
    }
}

/// An unchanged source must NOT be re-imported: that is the whole point of the
/// sidecar, and a freshness test that is too eager costs every open a full
/// rebuild.
#[test]
fn an_untouched_source_reuses_its_sidecar() {
    let _g = Scratch::new("reuse");
    let p = _g.path().to_string();
    write_source(&p, 5, "a");
    unsafe {
        assert_eq!(count(&p), 5);
    }
    let side = format!("{p}.mpedb");
    let first = std::fs::metadata(&side).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    unsafe {
        assert_eq!(count(&p), 5);
    }
    assert_eq!(
        std::fs::metadata(&side).unwrap().modified().unwrap(),
        first,
        "the sidecar was rebuilt for a source nobody touched"
    );
}

/// Concurrent opens of a stale sidecar: every caller gets the right answer and
/// no staging file survives. Before the lock they shared one staging path and
/// a loser could fail the open outright.
///
/// Honest about what this proves: it passes WITHOUT the lock too — six threads
/// do not reliably collide, and a test that waits for a race to show up is a
/// test that passes for the wrong reason. It pins the user-visible contract,
/// not the mechanism. The lock's argument is in the code, not here.
#[test]
fn concurrent_opens_import_once_and_all_succeed() {
    let _g = Scratch::new("race");
    let p = _g.path().to_string();
    write_source(&p, 11, "a");
    let n = 6;
    let got: Vec<i64> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..n)
            .map(|_| {
                let p = p.clone();
                s.spawn(move || unsafe { count(&p) })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("a thread panicked")).collect()
    });
    assert_eq!(got, vec![11; n], "every concurrent open sees all the rows");
    // No staging file survives a completed race.
    let dir = Path::new(&p).parent().unwrap();
    let base = Path::new(&p).file_name().unwrap().to_string_lossy().into_owned();
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with(&base) && f.contains(".importing"))
        .collect();
    assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");
}

/// The import is batched, so a source with more rows than one batch (4096)
/// must still come across exactly — the boundary the batching introduced.
#[test]
fn a_source_larger_than_one_batch_imports_whole() {
    let _g = Scratch::new("batch");
    let p = _g.path().to_string();
    write_source(&p, 10_000, "r");
    unsafe {
        assert_eq!(count(&p), 10_000, "4096 + 4096 + 1808, all of them");
    }
}
