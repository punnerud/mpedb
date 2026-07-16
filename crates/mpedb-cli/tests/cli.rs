//! End-to-end tests that spawn the built `mpedb` binary. Every invocation is
//! its own process, so these also cover cross-process behavior (shared plan
//! registry, multi-process stress, crash injection).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mpedb")
}

fn base_dir() -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> TestDir {
        let dir = base_dir().join(format!("mpedb-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// users(id int64 pk, email text unique not null)
fn write_users_config(dir: &Path) -> String {
    let cfg = dir.join("config.toml");
    let db = dir.join("db.mpedb");
    std::fs::write(
        &cfg,
        format!(
            r#"[database]
path = "{}"
size_mb = 16
durability = "none"

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
"#,
            db.display()
        ),
    )
    .unwrap();
    cfg.to_str().unwrap().to_owned()
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().unwrap()
}

fn out_str(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err_str(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn assert_ok(o: &Output) {
    assert!(
        o.status.success(),
        "command failed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        o.status,
        out_str(o),
        err_str(o)
    );
}

// --------------------------------------------------------------------------

/// exec → prepare → call round-trip. `call` runs in a NEW process that never
/// saw the SQL: the hash must resolve through the shared plan registry
/// inside the database file.
#[test]
fn exec_prepare_call_roundtrip() {
    let td = TestDir::new("roundtrip");
    let cfg = write_users_config(td.path());

    let o = run(&["exec", &cfg, "INSERT INTO users (id, email) VALUES ($1, $2)", "1", "a@x"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "affected: 1");

    let o = run(&["prepare", &cfg, "SELECT id, email FROM users WHERE id = $1"]);
    assert_ok(&o);
    let hash = out_str(&o).trim().to_owned();
    assert_eq!(hash.len(), 64, "expected a 64-hex plan hash, got {hash:?}");
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));

    // Second process invocation: execute by hash only, no SQL anywhere.
    let o = run(&["call", &cfg, &hash, "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "id\temail\n1\ta@x\n");

    // Unknown hash → runtime error (exit 1) suggesting prepare.
    let zero = "0".repeat(64);
    let o = run(&["call", &cfg, &zero, "1"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("prepare"), "stderr: {}", err_str(&o));

    // Malformed hash → usage error (exit 2).
    let o = run(&["call", &cfg, "not-a-hash"]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", err_str(&o));

    // Bad subcommand usage → exit 2.
    let o = run(&["exec", &cfg]);
    assert_eq!(o.status.code(), Some(2));
}

/// Piped-stdin REPL: BEGIN + INSERT + ROLLBACK leaves nothing; BEGIN +
/// INSERT + COMMIT persists (checked from a separate process); `.tables`
/// prints name + row count.
#[test]
fn repl_transactions_and_tables() {
    let td = TestDir::new("repl");
    let cfg = write_users_config(td.path());

    let script = "\
BEGIN
INSERT INTO users (id, email) VALUES (10, 'gone@x')
ROLLBACK
BEGIN
INSERT INTO users (id, email) VALUES (11, 'kept@x')
COMMIT
.tables
.verify
.quit
";
    let mut child = Command::new(bin())
        .args(["repl", &cfg])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_ok(&o);
    let s = out_str(&o);
    assert!(s.contains("rollback"), "stdout: {s}");
    assert!(s.contains("commit"), "stdout: {s}");
    assert!(s.contains("users\t1"), ".tables output wrong: {s}");
    assert!(s.contains("verify: ok"), "stdout: {s}");

    // Fresh process: only the committed row survived.
    let o = run(&["exec", &cfg, "SELECT id, email FROM users"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "id\temail\n11\tkept@x\n");
}

/// `dump --data` must work from the .mpedb file alone: the file is copied to
/// a directory containing no config at all (disaster-recovery path).
#[test]
fn dump_data_without_config() {
    let src = TestDir::new("dump-src");
    let cfg = write_users_config(src.path());
    assert_ok(&run(&["exec", &cfg, "INSERT INTO users (id, email) VALUES ($1, $2)", "1", "a@x"]));
    assert_ok(&run(&["exec", &cfg, "INSERT INTO users (id, email) VALUES ($1, $2)", "2", "b@x"]));

    let dst = TestDir::new("dump-dst");
    let copied = dst.path().join("orphan.mpedb");
    std::fs::copy(src.path().join("db.mpedb"), &copied).unwrap();
    drop(src); // config and original db are gone

    let o = run(&["dump", copied.to_str().unwrap(), "--data"]);
    assert_ok(&o);
    let s = out_str(&o);
    assert!(s.contains("name = \"users\""), "schema missing: {s}");
    assert!(s.contains("unique = true"), "schema missing detail: {s}");
    assert!(s.contains("# table users: 2 rows"), "row count missing: {s}");
    assert!(s.contains("1\ta@x"), "row 1 missing: {s}");
    assert!(s.contains("2\tb@x"), "row 2 missing: {s}");
}

/// Multi-process bank invariant: concurrent transfer writers + full-scan
/// readers asserting sum conservation on every snapshot.
#[test]
fn stress_bank() {
    let td = TestDir::new("stress-bank");
    let o = run(&[
        "stress", "--dir", td.path().to_str().unwrap(),
        "--workers", "4", "--secs", "2", "--mode", "bank",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("verify: ok"), "stdout: {}", out_str(&o));
}

/// Multi-process UNIQUE race: children insert the same email set; losers get
/// constraint violations, the parent re-verifies uniqueness and totals.
#[test]
fn stress_unique() {
    let td = TestDir::new("stress-unique");
    let o = run(&[
        "stress", "--dir", td.path().to_str().unwrap(),
        "--workers", "4", "--secs", "2", "--mode", "unique",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("verify: ok"), "stdout: {}", out_str(&o));
}

/// SIGKILL crash injection: children die mid-write every wave; the database
/// must recover the writer lock promptly, pass page-accounting verification,
/// and show no torn rows.
///
/// This test used to quarantine the freelist-corruption engine bug's failure
/// signatures behind a retry loop. That bug no longer reproduces (fixed as a
/// side effect of #37/#39 — see the regression guard below), so EVERY failure
/// here fails loudly again: a corruption signature in a crash wave is a
/// crash/recovery regression, not a known flake.
#[test]
fn crash_injection() {
    let td = TestDir::new("crash");
    let o = run(&[
        "crash", "--dir", td.path().to_str().unwrap(),
        "--waves", "2", "--children", "4",
    ]);
    assert_ok(&o);
    let s = out_str(&o);
    assert!(s.contains("all invariants held"), "stdout: {s}");
}

/// REGRESSION GUARD for the historical freelist/page-accounting corruption
/// under concurrent readers + writers (observed 2026-07-12; NO LONGER
/// REPRODUCES as of 2026-07-16 — fixed as a side effect of #37's read-only
/// refill and #39's meta-retry work; no single commit names it). Verified
/// obsolete by 16 consecutive clean 8-worker×5s runs — at the historical
/// ~2/3 failure rate that is ≈ 2e-8 if the bug were still alive. Kept
/// #[ignore]d purely for TIME (~20 s), as the strongest known stressor of
/// the oldest-pinned/freelist-reclamation machinery (DESIGN.md §4.3/§4.5).
///
/// The historical failure signatures, should it ever return:
///   - child:  "internal error (bug in mpedb): double free of page N"
///   - parent: "database corrupt: page N listed twice in freelist"
///   - parent: "page N leaked: neither reachable nor freelisted"
///   - "btree: unexpected page kind in descent/collect" (premature reuse)
///   - escalation to catalog corruption: "missing catalog entry" /
///     "no schema stored in catalog"
///
/// Historical minimization (kept because it locates the machinery): the
/// trigger was autocommit point-SELECTs (reader pins) concurrent with COW
/// write txns, needing >= 3 processes — UPDATE-only and INSERT+DELETE
/// without reads never failed; UPDATE+SELECT failed 3/3 at 8 workers.
#[test]
#[ignore = "slow regression guard (~20 s: 4 stress runs of 8 workers x 5 s); run with --ignored"]
fn engine_bug_freelist_double_free_under_concurrent_readers() {
    // Historically ~2/3 failure rate per run; four clean runs in a row
    // ≈ 1% if the bug were present.
    for attempt in 0..4 {
        let td = TestDir::new(&format!("engine-bug-{attempt}"));
        let o = run(&[
            "stress", "--dir", td.path().to_str().unwrap(),
            "--workers", "8", "--secs", "5", "--mode", "mixed",
        ]);
        if !o.status.success() {
            panic!(
                "the historical freelist bug REGRESSED on attempt {attempt}:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out_str(&o),
                err_str(&o)
            );
        }
    }
}
