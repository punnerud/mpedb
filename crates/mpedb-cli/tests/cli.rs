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

/// A READ never creates the database — deliberately UNLIKE sqlite3, which
/// materializes the file on any statement at all. There is nothing to store, so
/// there is nothing to create; the answer comes from an empty scratch database
/// and is the same one the created-then-queried file would have given.
#[test]
fn a_read_does_not_create_the_database() {
    for ext in ["db", "mpedb"] {
        let td = TestDir::new(&format!("lazy-read-{ext}"));
        let path = |name: &str| td.path().join(format!("{name}.{ext}"));

        // repl reads: answered, and nothing lands on disk.
        let db = path("repl");
        let o = run_repl(&[db.to_str().unwrap()], "SELECT 1;\nSELECT 2+3;\n.exit\n");
        assert_ok(&o);
        assert!(out_str(&o).contains('1') && out_str(&o).contains('5'), "stdout: {}", out_str(&o));
        assert!(!db.exists(), "a repl SELECT created the database: {}", err_str(&o));

        // one-shot read: same.
        let db = path("oneshot");
        let o = run(&[db.to_str().unwrap(), "SELECT 7*6"]);
        assert_ok(&o);
        assert!(out_str(&o).contains("42"), "stdout: {}", out_str(&o));
        assert!(!db.exists(), "a one-shot SELECT created the database");

        // a read that FAILS creates nothing either — and fails for the right
        // reason (no such table), not for a missing file.
        let db = path("boom");
        let o = run(&[db.to_str().unwrap(), "SELECT * FROM nope"]);
        assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
        assert!(err_str(&o).contains("nope"), "stderr: {}", err_str(&o));
        assert!(!db.exists(), "a failing SELECT created the database");

        // and the directory is still untouched after all of it.
        let left: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{ext}: directory not left clean: {left:?}");
    }
}

/// … and the file appears on the first WRITE — in a repl as in a one-shot.
#[test]
fn the_first_write_creates_the_database() {
    for ext in ["db", "mpedb"] {
        let td = TestDir::new(&format!("lazy-first-{ext}"));
        let path = |name: &str| td.path().join(format!("{name}.{ext}"));

        // repl: reads first (creating nothing), then DDL → the file exists.
        let db = path("repl");
        let o = run_repl(
            &[db.to_str().unwrap()],
            "SELECT 1;\nCREATE TABLE t(id INTEGER PRIMARY KEY);\n.exit\n",
        );
        assert_ok(&o);
        assert!(db.exists(), "repl DDL did not create the database: {}", err_str(&o));
        assert!(
            err_str(&o).contains("created"),
            "the create notice belongs on the first write: {}",
            err_str(&o)
        );

        // one-shot DDL → the file exists AND really holds the table.
        let db = path("ddl");
        let dbs = db.to_str().unwrap().to_owned();
        assert_ok(&run(&[&dbs, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"]));
        assert!(db.exists());
        assert_ok(&run(&[&dbs, "INSERT INTO t VALUES(1,'x')"]));
        let o = run(&[&dbs, "SELECT * FROM t"]);
        assert_ok(&o);
        assert_eq!(out_str(&o), "id\tv\n1\tx\n");

        // a WRITE that then fails still creates the file: the create is the
        // decision to have a database, not a consequence of the write working.
        let db = path("boom");
        let o = run(&[db.to_str().unwrap(), "INSERT INTO nope VALUES(1)"]);
        assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
        assert!(db.exists(), "a failing first write must still create the database");
    }
}

// ------------------------------------------------------- CSV: import/analyse

/// `mpedb <db> <file.csv>` — everything below drives it over a PIPE, which is
/// the whole point of the flag pair: the interactive prompt must never fire
/// where there is nobody to answer it.
fn write_csv(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p.to_str().unwrap().to_owned()
}

/// What is left in a directory, sorted — the assertion that analysis wrote
/// nothing has to be about the WHOLE directory, not just the database name.
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// `--import` really lands the rows in the database — in a sqlite base (where
/// a foreign sqlite reader must see them) and in a native `.mpedb` alike.
#[test]
fn csv_import_puts_queryable_rows_in_the_database() {
    let td = TestDir::new("csv-import");
    let csv = write_csv(
        td.path(),
        "people.csv",
        "id,name,score\n1,Ada,9.5\n2,Grace,7.25\n3,Alan,3\n",
    );

    // 1. A sqlite base that does not exist yet: the import is a WRITE, so it
    //    creates the database (the lazy-create rule, unchanged).
    let db = td.path().join("data.db");
    let dbs = db.to_str().unwrap().to_owned();
    let o = run(&[&dbs, &csv, "--import"]);
    assert_ok(&o);
    assert!(db.exists(), "an import must create the database");
    assert!(out_str(&o).contains("imported 3 rows"), "stdout: {}", out_str(&o));

    let o = run(&[&dbs, "SELECT count(*), sum(score) FROM people"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "count(*)\tsum(score)\n3\t19.75\n");

    // The table is in the BASE, so plain sqlite reads it.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let (n, name): (i64, String) = conn
        .query_row("SELECT id, name FROM people WHERE id = 2", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((n, name), (2, "Grace".to_owned()));

    // 2. The native `.mpedb` target takes the same file through the typed row
    //    API and answers the same question.
    let ndb = td.path().join("native.mpedb");
    let nds = ndb.to_str().unwrap().to_owned();
    assert_ok(&run(&[&nds, &csv, "--import"]));
    let o = run(&[&nds, "SELECT name FROM people WHERE id = 3"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "name\nAlan\n");

    // 3. `--table` puts the same CSV somewhere else in the same database.
    assert_ok(&run(&[&dbs, &csv, "--import", "--table", "people_v2"]));
    let o = run(&[&dbs, "SELECT count(*) FROM people_v2"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "count(*)\n3\n");
}

/// `--analyse` answers questions about the CSV and leaves the directory exactly
/// as it found it — the file it loads into is a scratch database in the temp
/// directory, removed on the way out. This is the CSV face of the rule that a
/// READ creates nothing.
#[test]
fn csv_analysis_writes_nothing_at_all() {
    let td = TestDir::new("csv-analyse");
    let csv = write_csv(
        td.path(),
        "sales.csv",
        "region,amount\nnorth,10\nsouth,32\nnorth,8\n",
    );
    let before = listing(td.path());

    let db = td.path().join("data.db");
    let o = run_repl(
        &[db.to_str().unwrap(), &csv, "--analyse"],
        "SELECT region, sum(amount) FROM sales GROUP BY region;\n.quit\n",
    );
    assert_ok(&o);
    assert!(out_str(&o).contains("north\t18"), "stdout: {}", out_str(&o));
    assert!(out_str(&o).contains("south\t32"), "stdout: {}", out_str(&o));

    assert_eq!(listing(td.path()), before, "analysis wrote to the directory");
    assert!(!db.exists(), "analysis created the database");
}

/// RFC4180 in anger: a delimiter inside quotes, a doubled quote, a newline
/// inside a field, CRLF line ends, and empty fields (which become NULL). The
/// round trip through an import is the assertion — the bytes have to come back.
#[test]
fn csv_quoting_edge_cases_survive_the_round_trip() {
    let td = TestDir::new("csv-quotes");
    let csv = write_csv(
        td.path(),
        "q.csv",
        "id,txt\r\n1,\"Doe, Jane\"\r\n2,\"he said \"\"hi\"\"\"\r\n3,\"two\nlines\"\r\n4,\r\n",
    );
    let db = td.path().join("q.db");
    let dbs = db.to_str().unwrap().to_owned();
    assert_ok(&run(&[&dbs, &csv, "--import"]));

    let conn = rusqlite::Connection::open(&db).unwrap();
    let got: Vec<(i64, Option<String>)> = conn
        .prepare("SELECT id, txt FROM q ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        got,
        vec![
            (1, Some("Doe, Jane".to_owned())),
            (2, Some("he said \"hi\"".to_owned())),
            (3, Some("two\nlines".to_owned())),
            (4, None), // an empty field is NULL
        ]
    );

    // A tab-separated file is recognized without being told.
    let tsv = write_csv(td.path(), "t.tsv", "a\tb\n1\tx\n2\ty\n");
    assert_ok(&run(&[&dbs, &tsv, "--import"]));
    let o = run(&[&dbs, "SELECT b FROM t WHERE a = 2"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "b\ny\n");

    // An unterminated quote is truncated data, not something to guess at.
    let bad = write_csv(td.path(), "bad.csv", "a,b\n1,\"oops\n");
    let o = run(&[&dbs, &bad, "--import"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("unterminated"), "stderr: {}", err_str(&o));
}

/// Inference commits to a type per column, and is timid where being wrong
/// would LOSE data: leading zeros stay text (zip codes, ids), a column that
/// mixes integers and decimals is float, anything else is text.
#[test]
fn csv_type_inference_picks_int_float_and_text() {
    let td = TestDir::new("csv-types");
    let csv = write_csv(
        td.path(),
        "m.csv",
        "n,f,s,zip,blank\n1,1.5,alpha,00501,\n2,3,beta,10001,\n-3,2e2,7up,00000,\n",
    );
    let db = td.path().join("m.db");
    let dbs = db.to_str().unwrap().to_owned();
    let o = run(&[&dbs, &csv, "--import"]);
    assert_ok(&o);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'm'", [], |r| r.get(0))
        .unwrap();
    assert!(sql.contains("\"n\" INTEGER PRIMARY KEY"), "{sql}");
    assert!(sql.contains("\"f\" REAL"), "{sql}");
    assert!(sql.contains("\"s\" TEXT"), "{sql}");
    // The one that matters: `00501` is not the integer 501.
    assert!(sql.contains("\"zip\" TEXT"), "{sql}");
    // An all-empty column has nothing to infer from, so it is text and NULLable.
    assert!(sql.contains("\"blank\" TEXT") && !sql.contains("\"blank\" TEXT NOT NULL"), "{sql}");
    let zip: String = conn
        .query_row("SELECT zip FROM m WHERE n = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(zip, "00501");

    // No header row (every field is a number): the rows are all DATA and the
    // columns are named c1.., with a synthesized primary key since column 1
    // repeats.
    let nh = write_csv(td.path(), "nh.csv", "1,2\n1,3\n");
    assert_ok(&run(&[&dbs, &nh, "--import"]));
    let o = run(&[&dbs, "SELECT count(*) FROM nh"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "count(*)\n2\n", "the first row was eaten as a header");
    let sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'nh'", [], |r| r.get(0))
        .unwrap();
    assert!(sql.contains("\"rowid\" INTEGER PRIMARY KEY") && sql.contains("\"c1\""), "{sql}");
}

/// An import that lands on an existing table has no correct behaviour, so it
/// has none: it refuses, names the flag that gets you out, and changes nothing.
#[test]
fn csv_import_never_overwrites_an_existing_table() {
    let td = TestDir::new("csv-collide");
    let db = td.path().join("c.db");
    let dbs = db.to_str().unwrap().to_owned();
    assert_ok(&run(&[&dbs, "CREATE TABLE users(id INTEGER PRIMARY KEY, keep TEXT)"]));
    assert_ok(&run(&[&dbs, "INSERT INTO users VALUES(1,'original')"]));
    assert_ok(&run(&["checkpoint", &dbs]));

    // The CSV's header names a table that is already there.
    let csv = write_csv(td.path(), "users.csv", "id,other\n9,new\n");
    let o = run(&[&dbs, &csv, "--import"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("already exists"), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("--table"), "the way out must be named: {}", err_str(&o));

    let o = run(&[&dbs, "SELECT keep FROM users"]);
    assert_ok(&o);
    assert_eq!(out_str(&o), "keep\noriginal\n", "the existing table was touched");

    // The same refusal on a native database.
    let ndb = td.path().join("c.mpedb");
    let nds = ndb.to_str().unwrap().to_owned();
    assert_ok(&run(&[&nds, &csv, "--import"]));
    let o = run(&[&nds, &csv, "--import"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("already exists"), "stderr: {}", err_str(&o));
}

/// An empty CSV is not a table. It fails before anything is created — for the
/// import that would have created the database as much as for the analysis.
#[test]
fn an_empty_csv_is_refused_and_creates_nothing() {
    let td = TestDir::new("csv-empty");
    let db = td.path().join("e.db");
    let dbs = db.to_str().unwrap().to_owned();
    for body in ["", "\n\n\n"] {
        let csv = write_csv(td.path(), "empty.csv", body);
        for flag in ["--import", "--analyse"] {
            let o = run(&[&dbs, &csv, flag]);
            assert_eq!(o.status.code(), Some(1), "{flag} {body:?}: {}", err_str(&o));
            assert!(err_str(&o).contains("is empty"), "stderr: {}", err_str(&o));
            assert!(!db.exists(), "{flag} on an empty CSV created the database");
        }
    }
}

/// Piped stdin gets NO prompt: the CSV question is only asked where somebody
/// can answer it, and the unattended answer is the one that writes nothing.
/// A prompt fired down a pipe would eat the caller's first line and then hang.
#[test]
fn a_csv_over_a_pipe_is_never_prompted_for() {
    let td = TestDir::new("csv-nopipe");
    let csv = write_csv(td.path(), "p.csv", "id,v\n1,x\n2,y\n");
    let db = td.path().join("p.db");
    let before = listing(td.path());

    // No flag at all: the default must resolve itself, silently on stdout.
    let o = run_repl(&[db.to_str().unwrap(), &csv], "SELECT count(*) FROM p;\n.quit\n");
    assert_ok(&o);
    assert!(out_str(&o).contains('2'), "stdout: {}", out_str(&o));
    let err = err_str(&o);
    assert!(err.contains("no tty"), "the default must say what it did: {err}");
    assert!(!err.contains("choice ["), "a prompt fired down a pipe: {err}");
    assert_eq!(listing(td.path()), before, "the unattended default wrote something");

    // ... and the flags cannot be used without a CSV to apply them to.
    let o = run(&[db.to_str().unwrap(), "--import", "SELECT 1"]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", err_str(&o));
    // ... nor can SQL trail a CSV, where it would silently never run.
    let o = run(&[db.to_str().unwrap(), &csv, "SELECT 1"]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", err_str(&o));
    assert!(err_str(&o).contains("not a statement"), "stderr: {}", err_str(&o));
}

/// Tab on an empty line opens an interactive table picker — but ONLY on a tty.
/// Over a pipe there is no line editor at all, so a Tab is just a character in
/// a statement and nothing is painted: no menu, no prompt, no escape sequences.
/// (The picker itself is verified on a real pty; it cannot be driven by a pipe,
/// which is exactly what this asserts.)
#[test]
fn piped_stdin_gets_no_picker_and_no_prompt() {
    let td = TestDir::new("no-picker");
    let db = td.path().join("k.db");
    let dbs = db.to_str().unwrap().to_owned();
    assert_ok(&run(&[&dbs, "CREATE TABLE users(id INTEGER PRIMARY KEY)"]));

    // A bare Tab, a Tab-indented line, and an empty line: none of them may
    // produce a menu.
    let o = run_repl(&[&dbs], "\t\n\t\nSELECT count(*) FROM users;\n\n.quit\n");
    assert_ok(&o);
    let all = format!("{}{}", out_str(&o), err_str(&o));
    assert!(!all.contains("Esc cancel"), "a picker was painted over a pipe: {all}");
    assert!(!all.contains("no tables yet"), "picker text over a pipe: {all}");
    assert!(!all.contains('\x1b'), "escape sequences over a pipe: {all:?}");
    assert!(!all.contains("mpedb(ovl)>"), "a prompt was printed over a pipe: {all:?}");
    assert!(!all.contains("mpedb> "), "a prompt was printed over a pipe: {all:?}");

    // The same for a native database's repl.
    let ndb = td.path().join("k.mpedb");
    let nds = ndb.to_str().unwrap().to_owned();
    assert_ok(&run(&[&nds, "CREATE TABLE users(id INTEGER PRIMARY KEY)"]));
    let o = run_repl(&[&nds], "\t\n.tables\n.quit\n");
    assert_ok(&o);
    let all = format!("{}{}", out_str(&o), err_str(&o));
    assert!(!all.contains('\x1b') && !all.contains("Esc cancel"), "{all:?}");
    assert!(all.contains("users"), ".tables still works: {all}");
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
/// #78 `tier drain` end to end: seed a hot db, drain a predicate into a
/// freshly created cold file, and prove the split + read-back through the
/// CLI alone — counts on both sides, idempotent re-run, ATTACH union.
#[test]
fn tier_drain_cli_roundtrip() {
    let td = TestDir::new("tier-drain");
    let hot = td.path().join("hot.mpedb");
    let cold = td.path().join("cold.mpedb");
    let (hs, cs) = (hot.to_str().unwrap(), cold.to_str().unwrap());

    assert_ok(&run(&[hs, "CREATE TABLE ev (id INTEGER PRIMARY KEY, grp INT NOT NULL, msg TEXT)"]));
    for i in 0..30 {
        assert_ok(&run(&[
            hs,
            &format!("INSERT INTO ev (id, grp, msg) VALUES ({i}, {}, 'm{i}')", i % 3),
        ]));
    }

    let o = run(&[
        "tier", "drain", hs, cs,
        "--table", "ev", "--where", "grp = $1", "1",
        "--batch", "4", "--size-mb", "16",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("moved=10"), "stdout: {}", out_str(&o));

    let count = |target: &str, sql: &str| {
        let o = run(&[target, sql]);
        assert_ok(&o);
        out_str(&o).lines().last().unwrap().trim().to_owned()
    };
    assert_eq!(count(hs, "SELECT count(*) FROM ev"), "20");
    assert_eq!(count(cs, "SELECT count(*) FROM ev"), "10");
    assert_eq!(count(cs, "SELECT count(*) FROM ev WHERE grp = 1"), "10");

    // Idempotent: nothing left matching, nothing moves, nothing breaks.
    let o = run(&[
        "tier", "drain", hs, cs, "--table", "ev", "--where", "grp = $1", "1",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("moved=0"), "stdout: {}", out_str(&o));

    // Read-back exactly as documented: ATTACH + cross-file UNION ALL (a repl
    // script — the one-shot form runs a single statement).
    let o = run_repl(
        &[hs],
        &format!(
            "ATTACH DATABASE '{cs}' AS cold;\n\
             SELECT id FROM ev UNION ALL SELECT id FROM cold.ev;\n.quit\n"
        ),
    );
    assert_ok(&o);
    let ids = out_str(&o)
        .lines()
        .filter(|l| l.trim().parse::<i64>().is_ok())
        .count();
    assert_eq!(ids, 30, "attach union stdout: {}", out_str(&o));
}

/// #78 SIGKILL fuzz on the drain protocol: the drainer dies at a random
/// instant every wave; no row may be lost, duplicates must reconcile, and
/// the final split must be exact (see `tier crash` in the CLI).
#[test]
fn tier_crash_injection() {
    let td = TestDir::new("tier-crash");
    let o = run(&[
        "tier", "crash", "--dir", td.path().to_str().unwrap(), "--waves", "3",
    ]);
    assert_ok(&o);
    let s = out_str(&o);
    assert!(s.contains("no row lost, no divergence"), "stdout: {s}");
}

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
