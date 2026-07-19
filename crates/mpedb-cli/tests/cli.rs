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

/// Drive a repl with `script` on stdin. Piped stdin is not a tty, so this is
/// the plain (non-rustyline) reader path.
fn run_repl(args: &[&str], script: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
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
    child.wait_with_output().unwrap()
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

/// `mpedb new.db 'SQL'` on a MISSING path CREATES the database, as `sqlite3
/// new.db 'SQL'` does — and the created base stays a real sqlite file: DDL lands
/// in it, deltas live in the overlay, `checkpoint` folds them back, and a
/// foreign sqlite reader sees the result.
#[test]
fn creates_a_sqlite_database_on_the_first_statement() {
    let td = TestDir::new("create-sqlite");
    let db = td.path().join("new.db");
    let dbs = db.to_str().unwrap().to_owned();

    // 1. DDL on a path that does not exist yet.
    assert_ok(&run(&[&dbs, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"]));
    assert!(db.exists(), "the database was not created");
    let head = std::fs::read(&db).unwrap();
    assert_eq!(
        &head[..16],
        b"SQLite format 3\0",
        "the created base is not a sqlite file"
    );

    // 2. Writes go to the overlay, 3. reads merge base + overlay.
    let o = run(&[&dbs, "INSERT INTO t VALUES(1,'x')"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "affected: 1");
    let o = run(&[&dbs, "SELECT * FROM t"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "id\tv\n1\tx\n");

    // 4. checkpoint folds the delta into the base, 5. where foreign sqlite —
    // a library that knows nothing about mpedb — reads it.
    assert_ok(&run(&["checkpoint", &dbs]));
    let conn = rusqlite::Connection::open(&db).unwrap();
    let row: (i64, String) = conn
        .query_row("SELECT id, v FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(row, (1, "x".to_owned()));

    // A repl session over the same base: DDL mid-session folds the pending
    // delta first, and everything converges in the base.
    let script = "\
INSERT INTO t VALUES(2,'y')
CREATE TABLE u(id INTEGER PRIMARY KEY)
INSERT INTO u VALUES(9)
SELECT * FROM u
.checkpoint
.quit
";
    let mut child = Command::new(bin())
        .args([&dbs])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_ok(&o);
    assert!(out_str(&o).contains("9"), "stdout: {}", out_str(&o));
    let conn = rusqlite::Connection::open(&db).unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "the pre-DDL delta was not folded into the base");
    let n: i64 = conn.query_row("SELECT count(*) FROM u", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

/// The `.mpedb` variant: a missing path ending in `.mpedb` is created as a
/// NATIVE database (seeded with one inert bootstrap table, since a schema with
/// no tables is refused), and live `CREATE TABLE` grows it from there.
#[test]
fn creates_a_native_database_on_the_first_statement() {
    let td = TestDir::new("create-native");
    let db = td.path().join("new.mpedb");
    let dbs = db.to_str().unwrap().to_owned();

    assert_ok(&run(&[&dbs, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"]));
    assert!(db.exists(), "the database was not created");
    assert_ok(&run(&[&dbs, "INSERT INTO t VALUES(1,'x')"]));
    let o = run(&[&dbs, "SELECT * FROM t"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "id\tv\n1\tx\n");

    // It is a genuine mpedb file: the config-free dump reads it.
    let o = run(&["dump", &dbs, "--data"]);
    assert_ok(&o);
    assert!(out_str(&o).contains("name = \"t\""), "dump: {}", out_str(&o));
}

/// What creation must NOT do: invent directories, invent a config, or turn a
/// mistyped subcommand into a database.
#[test]
fn creation_refuses_what_it_cannot_invent() {
    let td = TestDir::new("create-refuse");
    let nested = td.path().join("nope").join("x.db");
    let o = run(&[nested.to_str().unwrap(), "SELECT 1"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("no such directory"), "stderr: {}", err_str(&o));
    assert!(!nested.exists());

    let missing_cfg = td.path().join("gone.toml");
    let o = run(&[missing_cfg.to_str().unwrap(), "SELECT 1"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("no such config file"), "stderr: {}", err_str(&o));
    assert!(!missing_cfg.exists());

    // A bare word with no separator and no extension is a command, not a path.
    let o = run(&["exce"]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("unknown command"), "stderr: {}", err_str(&o));
    assert!(!Path::new("exce").exists());
}

/// Creation is LAZY, exactly as `sqlite3`'s is: measured against sqlite3 3.45,
/// `sqlite3 a.db` followed by `.exit` leaves NO FILE behind. Opening a repl and
/// leaving it again — and typing only dot-commands while there — must create
/// nothing at all, for a sqlite base and for a native `.mpedb` alike.
#[test]
fn opening_a_repl_and_exiting_creates_nothing() {
    let td = TestDir::new("lazy-none");
    for ext in ["db", "mpedb"] {
        for (what, script) in [
            ("exit-only", ".exit\n"),
            ("quit-only", ".quit\n"),
            ("eof-only", ""),
            ("dots-only", ".help\n.exit\n"),
            ("blank-lines", "\n\n.exit\n"),
        ] {
            let db = td.path().join(format!("{what}.{ext}"));
            let o = run_repl(&[db.to_str().unwrap()], script);
            assert_ok(&o);
            assert!(
                !db.exists(),
                "`mpedb {}` + {script:?} created a file; sqlite3 leaves nothing\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                db.display(),
                out_str(&o),
                err_str(&o)
            );
            // Nothing else appeared beside it either (no sidecar, no overlay).
            let left: Vec<_> = std::fs::read_dir(td.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(left.is_empty(), "{what}.{ext}: directory not left clean: {left:?}");
        }
    }
}

/// … and the file appears on the FIRST STATEMENT — in a repl as in a one-shot,
/// and (matching sqlite3) even when that statement fails.
#[test]
fn the_first_statement_creates_the_database() {
    for ext in ["db", "mpedb"] {
        let td = TestDir::new(&format!("lazy-first-{ext}"));
        let path = |name: &str| td.path().join(format!("{name}.{ext}"));

        // repl: a statement, then exit → the file exists.
        let db = path("repl");
        let o = run_repl(&[db.to_str().unwrap()], "SELECT 1;\n.exit\n");
        assert_ok(&o);
        assert!(db.exists(), "repl statement did not create the database: {}", err_str(&o));
        assert!(
            err_str(&o).contains("created"),
            "the create notice belongs on the first statement: {}",
            err_str(&o)
        );

        // one-shot SELECT → the file exists (a table-less sqlite base cannot
        // answer it yet — sqlite3 can; the file is what this asserts).
        let db = path("oneshot");
        let o = run(&[db.to_str().unwrap(), "SELECT 1"]);
        assert!(db.exists(), "one-shot SELECT did not create the database: {}", err_str(&o));

        // one-shot DDL → the file exists AND really holds the table.
        let db = path("ddl");
        let dbs = db.to_str().unwrap().to_owned();
        assert_ok(&run(&[&dbs, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"]));
        assert!(db.exists());
        assert_ok(&run(&[&dbs, "INSERT INTO t VALUES(1,'x')"]));
        let o = run(&[&dbs, "SELECT * FROM t"]);
        assert_ok(&o);
        assert_eq!(out_str(&o), "id\tv\n1\tx\n");

        // a statement that ERRORS still creates the file (sqlite3 parity).
        let db = path("boom");
        let o = run(&[db.to_str().unwrap(), "SELECT * FROM nope"]);
        assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
        assert!(
            db.exists(),
            "a failing first statement must still create the database (sqlite3 does)"
        );
    }
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
/// the oldest-pinned/freelist-reclamation machinery (design/DESIGN.md §4.3/§4.5).
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
