//! End-to-end `mpedb proc` tests against the built binary. Every invocation
//! is a separate process, so define→call also exercises cross-process
//! resolution of proc blobs and their embedded plan hashes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        let dir = base_dir().join(format!("mpedb-proc-cli-{name}-{}", std::process::id()));
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

fn write_accounts_config(dir: &Path) -> String {
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
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false
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

/// define (.py) → call by name → call by hash from a fresh process → list.
#[test]
fn proc_define_call_list_roundtrip() {
    let td = TestDir::new("roundtrip");
    let cfg = write_accounts_config(td.path());
    for (id, bal) in [(1, 100), (2, 10)] {
        assert_ok(&run(&[
            "exec", &cfg,
            "INSERT INTO accounts (id, balance) VALUES ($1, $2)",
            &id.to_string(), &bal.to_string(),
        ]));
    }

    let proc_py = td.path().join("transfer.py");
    std::fs::write(
        &proc_py,
        r#"
def transfer(src, dst, amount):
    rows = db.query("SELECT balance FROM accounts WHERE id = $1", [src])
    if len(rows) == 0 or rows[0][0] < amount:
        return -1
    db.execute("UPDATE accounts SET balance = balance - $2 WHERE id = $1", [src, amount])
    db.execute("UPDATE accounts SET balance = balance + $2 WHERE id = $1", [dst, amount])
    return rows[0][0] - amount
"#,
    )
    .unwrap();

    // define prints "name\thash".
    let o = run(&["proc", "define", &cfg, proc_py.to_str().unwrap()]);
    assert_ok(&o);
    let line = out_str(&o).trim().to_owned();
    let (name, hash) = line.split_once('\t').expect("name\\thash");
    assert_eq!(name, "transfer");
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));

    // Call by name (new process).
    let o = run(&["proc", "call", &cfg, "transfer", "1", "2", "30"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "70");

    // Call by hash (another new process, never saw the source).
    let o = run(&["proc", "call", &cfg, hash, "2", "1", "5"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "35");

    // Balances via plain SQL: 100-30+5=75, 10+30-5=35.
    let o = run(&["exec", &cfg, "SELECT id, balance FROM accounts"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "id\tbalance\n1\t75\n2\t35\n");

    // Insufficient balance returns -1 and changes nothing.
    let o = run(&["proc", "call", &cfg, "transfer", "1", "2", "9999"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "-1");
    let o = run(&["exec", &cfg, "SELECT balance FROM accounts WHERE id = $1", "1"]);
    assert_eq!(out_str(&o), "balance\n75\n");

    // list shows the proc with its hash.
    let o = run(&["proc", "list", &cfg]);
    assert_ok(&o);
    let s = out_str(&o);
    assert!(s.contains("transfer"), "list output: {s}");
    assert!(s.contains(hash), "list output: {s}");
    assert!(s.contains("write"), "list output: {s}");
}

/// `proc call` accepts a git-style short hash prefix, and a defined name
/// still wins over a prefix-looking string.
#[test]
fn proc_call_by_hash_prefix() {
    let td = TestDir::new("prefix");
    let cfg = write_accounts_config(td.path());
    assert_ok(&run(&[
        "exec", &cfg,
        "INSERT INTO accounts (id, balance) VALUES (1, 500)",
    ]));

    let proc_py = td.path().join("bal.py");
    std::fs::write(
        &proc_py,
        r#"
def bal(id):
    rows = db.query("SELECT balance FROM accounts WHERE id = $1", [id])
    return rows[0][0]
"#,
    )
    .unwrap();
    let o = run(&["proc", "define", &cfg, proc_py.to_str().unwrap()]);
    assert_ok(&o);
    let (name, hash) = out_str(&o).trim().split_once('\t').map(|(n, h)| (n.to_owned(), h.to_owned())).unwrap();
    assert_eq!(name, "bal");

    // Call by the first 8 hex chars of the hash (a new process each time).
    let o = run(&["proc", "call", &cfg, &hash[..8], "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "500");

    // Call by a longer unique prefix — same result.
    let o = run(&["proc", "call", &cfg, &hash[..20], "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "500");

    // Full hash still works (unchanged).
    let o = run(&["proc", "call", &cfg, &hash, "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "500");

    // Call by name still works and takes precedence.
    let o = run(&["proc", "call", &cfg, "bal", "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "500");

    // A too-short / non-matching hex prefix is a clean runtime error (exit 1).
    let o = run(&["proc", "call", &cfg, "dead", "1"]);
    assert_eq!(o.status.code(), Some(1), "stdout: {}", out_str(&o));
    assert!(err_str(&o).contains("unknown procedure"), "{}", err_str(&o));
}

/// Two procs sharing a 4-hex-char hash prefix make that prefix ambiguous;
/// `proc call <prefix>` fails (exit 1) with a clear, candidate-listing error.
#[test]
fn proc_call_ambiguous_prefix_errors() {
    let td = TestDir::new("ambig");
    let cfg = write_accounts_config(td.path());

    // Define distinct pure procs (non-hex names) until two hashes collide on
    // their first 4 hex chars. Deterministic (blake3), so not flaky.
    use std::collections::HashMap;
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut ambiguous: Option<(String, String, String)> = None;
    for i in 0..5000u32 {
        let name = format!("pp{i}");
        let src_path = td.path().join(format!("{name}.py"));
        std::fs::write(&src_path, format!("def {name}():\n    return {i}\n")).unwrap();
        let o = run(&["proc", "define", &cfg, src_path.to_str().unwrap()]);
        assert_ok(&o);
        let hash = out_str(&o).trim().split_once('\t').unwrap().1.to_owned();
        let _ = std::fs::remove_file(&src_path);
        let p4 = hash[..4].to_owned();
        if let Some(prev) = seen.get(&p4) {
            ambiguous = Some((p4, prev.clone(), hash));
            break;
        }
        seen.insert(p4, hash);
    }
    let (prefix, h1, h2) = ambiguous.expect("no 4-hex collision within 5000 procs");

    let o = run(&["proc", "call", &cfg, &prefix]);
    assert_eq!(o.status.code(), Some(1), "stdout: {}", out_str(&o));
    let err = err_str(&o);
    assert!(err.contains("ambiguous"), "stderr: {err}");
    assert!(err.contains(&h1[..12]) && err.contains(&h2[..12]), "stderr: {err}");
}

/// A Rust-source proc through the same pipeline, plus row output and the
/// usage/error exit codes.
#[test]
fn proc_rust_source_rows_and_errors() {
    let td = TestDir::new("rust");
    let cfg = write_accounts_config(td.path());
    assert_ok(&run(&[
        "exec", &cfg,
        "INSERT INTO accounts (id, balance) VALUES (7, 700)",
    ]));

    let proc_rs = td.path().join("snapshot.rs");
    std::fs::write(
        &proc_rs,
        r#"
fn snapshot() -> i64 {
    let rows = db.query("SELECT id, balance FROM accounts");
    return rows;
}
"#,
    )
    .unwrap();
    let o = run(&["proc", "define", &cfg, proc_rs.to_str().unwrap()]);
    assert_ok(&o);
    assert!(out_str(&o).starts_with("snapshot\t"));

    // Rows print like a result set.
    let o = run(&["proc", "call", &cfg, "snapshot"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "7\t700\n");

    // Unknown proc → runtime error (exit 1).
    let o = run(&["proc", "call", &cfg, "nosuch"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("unknown procedure"), "{}", err_str(&o));

    // Wrong extension → usage error (exit 2).
    let bad = td.path().join("p.txt");
    std::fs::write(&bad, "def f(): return 1").unwrap();
    let o = run(&["proc", "define", &cfg, bad.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));

    // Rejected construct → compile error (exit 1) with location.
    let evil = td.path().join("evil.py");
    std::fs::write(&evil, "def f():\n    import os\n    return 1").unwrap();
    let o = run(&["proc", "define", &cfg, evil.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(err_str(&o).contains("line 2"), "stderr: {}", err_str(&o));

    // Bad subcommand → usage (exit 2).
    let o = run(&["proc", "frobnicate", &cfg]);
    assert_eq!(o.status.code(), Some(2));
}

/// `proc call --budget` wires all three dimensions through to the engine:
/// a streaming cursor proc dies on a tiny row budget, completes on a
/// raised one, and malformed specs are usage errors (exit 2).
#[test]
fn proc_call_budget_flag() {
    let td = TestDir::new("budget");
    let cfg = write_accounts_config(td.path());
    for id in 0..20 {
        assert_ok(&run(&[
            "exec", &cfg,
            "INSERT INTO accounts (id, balance) VALUES ($1, $2)",
            &id.to_string(), &(id * 2).to_string(),
        ]));
    }
    let proc_py = td.path().join("scan.py");
    std::fs::write(
        &proc_py,
        r#"
def scan():
    s = 0
    for row in db.rows("SELECT id, balance FROM accounts"):
        s = s + row[1]
    return s
"#,
    )
    .unwrap();
    assert_ok(&run(&["proc", "define", &cfg, proc_py.to_str().unwrap()]));

    // Default budget: fine.
    let o = run(&["proc", "call", &cfg, "scan"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "380"); // 2 * sum(0..19)

    // Tiny row budget: the third component kills the scan (exit 1).
    let o = run(&["proc", "call", &cfg, "scan", "--budget", "1000000,10000,5"]);
    assert_eq!(o.status.code(), Some(1), "stdout: {}", out_str(&o));
    assert!(err_str(&o).contains("row budget"), "{}", err_str(&o));

    // Partial spec keeps defaults for the rest; tiny instruction budget.
    let o = run(&["proc", "call", &cfg, "scan", "--budget", "10"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(err_str(&o).contains("instruction budget"), "{}", err_str(&o));

    // A raised spec completes.
    let o = run(&["proc", "call", &cfg, "scan", "--budget", "2000000,20000,20000000"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "380");

    // Malformed specs are usage errors (exit 2).
    for bad in ["abc", "1,2,3,4", "1,,3"] {
        let o = run(&["proc", "call", &cfg, "scan", "--budget", bad]);
        assert_eq!(o.status.code(), Some(2), "spec {bad:?}: {}", err_str(&o));
    }
}
