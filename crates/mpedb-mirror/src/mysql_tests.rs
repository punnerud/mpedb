//! MySQL-protocol mirror against a throwaway `mariadbd`. The server is the
//! host binary, on a private socket, never the system instance.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mpedb::ExecResult;
use mpedb_types::Value;
use mysql::prelude::Queryable;

use crate::adapter::{NetOp, NetOpKind, SourceAdapter};
use crate::import::{import_sqlite, ImportOptions};
use crate::mysql_adapter::MysqlAdapter;
use crate::mysql_import::import_mysql;
use crate::sourcecfg::SourceSpec;
use crate::sqlite_adapter::SqliteAdapter;
use crate::state::{Authority, MirrorConfig, SourceKind, KEY_CFG, MIR_NS};
use crate::switch::{drain_pull, drain_push, read_epoch};

static ONE_SERVER: Mutex<()> = Mutex::new(());

struct Maria {
    child: Option<Child>,
    sock: PathBuf,
    dir: PathBuf,
}

impl Drop for Maria {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn mariadb_bin(name: &str) -> String {
    for dir in ["/usr/sbin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"] {
        let path = format!("{dir}/{name}");
        if Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

fn boot(label: &str) -> (Maria, SourceSpec) {
    let n = std::process::id();
    let dir = mpedb_testkit::scratch_base().join(format!("mpedb-mysql-{label}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let datadir = dir.join("data");
    std::fs::create_dir_all(&datadir).unwrap();
    let sock = dir.join("s.sock");
    let err_path = dir.join("mariadbd.err");
    let install = Command::new(mariadb_bin("mariadb-install-db"))
        .args([
            "--no-defaults",
            &format!("--datadir={}", datadir.display()),
            "--auth-root-authentication-method=normal",
            "--skip-test-db",
            "--skip-name-resolve",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("mariadb-install-db: {e}"));
    if !install.status.success() {
        panic!(
            "mariadb-install-db failed: {}",
            String::from_utf8_lossy(&install.stderr)
        );
    }
    let err_file = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(mariadb_bin("mariadbd"))
        .args([
            "--no-defaults",
            &format!("--datadir={}", datadir.display()),
            &format!("--socket={}", sock.display()),
            &format!("--pid-file={}", dir.join("m.pid").display()),
            "--skip-networking",
            "--skip-grant-tables",
            "--innodb-buffer-pool-size=64M",
            "--character-set-server=utf8mb4",
            "--collation-server=utf8mb4_general_ci",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .spawn()
        .unwrap_or_else(|e| panic!("mariadbd spawn: {e}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if sock.exists() {
            let opts = mysql::OptsBuilder::new()
                .socket(Some(sock.display().to_string()))
                .user(Some("root"));
            if mysql::Conn::new(opts).is_ok() {
                break;
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "mariadbd exited {status}: {}",
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "mariadbd did not accept a connection within 30s: {}",
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let spec = SourceSpec::Mysql {
        socket: sock.display().to_string(),
        user: "root".into(),
        password: String::new(),
        database: "mirror".into(),
    };
    (Maria { child: Some(child), sock, dir }, spec)
}

fn cells(db: &mpedb::Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    match db.query(sql, params).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("query returned {other:?}"),
    }
}

fn assert_kind(db: &mpedb::Database, kind: SourceKind) {
    let bytes = db.sys_record_get(MIR_NS, KEY_CFG).unwrap().expect("mir/cfg");
    let cfg = MirrorConfig::decode(&bytes).unwrap();
    assert_eq!(cfg.source_kind, kind);
}

fn seed_migrated(sock: &Path, payload: &str) {
    let opts = mysql::OptsBuilder::new()
        .socket(Some(sock.display().to_string()))
        .user(Some("root"));
    let mut admin = mysql::Conn::new(opts).unwrap();
    admin.query_drop("CREATE DATABASE IF NOT EXISTS mirror").unwrap();
    admin.query_drop("USE mirror").unwrap();
    admin
        .query_drop(
            "CREATE TABLE migrated (\
                 id INT NOT NULL, \
                 part VARCHAR(64) NOT NULL, \
                 payload VARCHAR(64) NULL, \
                 PRIMARY KEY (id, part) \
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        )
        .unwrap();
    admin
        .exec_drop(
            "INSERT INTO migrated (id, part, payload) VALUES (?, ?, ?)",
            (1i64, "wheel", payload),
        )
        .unwrap();
}

#[test]
fn mysql_committed_row_pulls_into_mpedb_and_pushes_back() {
    let _hold = ONE_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let (server, spec) = boot("sync");
    seed_migrated(&server.sock, "seed");
    let dest = server.dir.join("mirror.mpedb");
    let opts = ImportOptions {
        include: Some(vec!["migrated".into()]),
        ..Default::default()
    };
    let db = import_mysql(&spec, &dest, &opts).unwrap().0;
    assert_kind(&db, SourceKind::Mysql);
    let epoch = read_epoch(&db).unwrap();
    assert_eq!(epoch.authority, Authority::Source);
    assert_eq!(epoch.epoch, 1);
    assert_eq!(
        cells(
            &db,
            "SELECT id, part, payload FROM migrated WHERE id=$1 AND part=$2",
            &[Value::Int(1), Value::Text("wheel".into())],
        ),
        vec![vec![Value::Int(1), Value::Text("wheel".into()), Value::Text("seed".into())]]
    );

    let mut adapter = MysqlAdapter::connect(&spec, opts.include.as_deref(), &[]).unwrap();
    adapter
        .conn()
        .exec_drop(
            "UPDATE migrated SET payload = NULL WHERE id = ? AND part = ?",
            (1i64, "wheel"),
        )
        .unwrap();
    let pulled = drain_pull(&db, &mut adapter).unwrap();
    assert!(pulled >= 1, "a committed MariaDB change must land in MPEdb");
    assert_eq!(
        cells(
            &db,
            "SELECT payload FROM migrated WHERE id=$1 AND part=$2",
            &[Value::Int(1), Value::Text("wheel".into())],
        ),
        vec![vec![Value::Null]]
    );

    db.query(
        "UPDATE migrated SET payload=$1 WHERE id=$2 AND part=$3",
        &[
            Value::Text("from-mpedb".into()),
            Value::Int(1),
            Value::Text("wheel".into()),
        ],
    )
    .unwrap();
    let pushed = drain_push(&db, &mut adapter).unwrap();
    assert!(pushed.upserts >= 1, "an MPEdb change must be written back to MariaDB");
    let stored: Option<Option<String>> = adapter
        .conn()
        .exec_first(
            "SELECT payload FROM migrated WHERE id = ? AND part = ?",
            (1i64, "wheel"),
        )
        .unwrap();
    assert_eq!(stored, Some(Some("from-mpedb".into())));
    let again = drain_pull(&db, &mut adapter).unwrap();
    assert_eq!(again, 0, "the push must not echo back into MPEdb");
    assert_eq!(
        cells(
            &db,
            "SELECT payload FROM migrated WHERE id=$1 AND part=$2",
            &[Value::Int(1), Value::Text("wheel".into())],
        ),
        vec![vec![Value::Text("from-mpedb".into())]]
    );
    let epoch = read_epoch(&db).unwrap();
    assert_eq!(epoch.authority, Authority::Source);
    assert_eq!(epoch.epoch, 1);
    assert!(crate::verify(&db, &mut adapter).unwrap());
    drop(server);
    drop(_hold);
}

#[test]
fn mysql_rows_compose_through_sqlite_and_back() {
    let _hold = ONE_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let (server, spec) = boot("compose");
    seed_migrated(&server.sock, "seed");
    let mysql_path = server.dir.join("mysql.mpedb");
    let opts = ImportOptions {
        include: Some(vec!["migrated".into()]),
        ..Default::default()
    };
    let mysql_db = import_mysql(&spec, &mysql_path, &opts).unwrap().0;
    let mut mysql = MysqlAdapter::connect(&spec, opts.include.as_deref(), &[]).unwrap();
    mysql
        .conn()
        .exec_drop(
            "UPDATE migrated SET payload = ? WHERE id = ? AND part = ?",
            ("from-mysql", 1i64, "wheel"),
        )
        .unwrap();
    assert!(drain_pull(&mysql_db, &mut mysql).unwrap() >= 1);
    let carriers = cells(&mysql_db, "SELECT id, part, payload FROM migrated", &[]);
    assert_eq!(
        carriers,
        vec![vec![
            Value::Int(1),
            Value::Text("wheel".into()),
            Value::Text("from-mysql".into())
        ]]
    );
    assert_eq!(read_epoch(&mysql_db).unwrap().authority, Authority::Source);

    let sqlite_path = server.dir.join("composed.sqlite");
    let sqlite_mirror = server.dir.join("sqlite.mpedb");
    let mut conn = rusqlite::Connection::open(&sqlite_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE migrated(id INTEGER NOT NULL, part TEXT NOT NULL, payload TEXT, PRIMARY KEY(id, part))",
    )
    .unwrap();
    {
        let tx = conn.transaction().unwrap();
        for row in &carriers {
            let Value::Int(id) = row[0] else { panic!("id {:?}", row[0]) };
            let Value::Text(part) = &row[1] else { panic!("part {:?}", row[1]) };
            let payload = match &row[2] {
                Value::Text(s) => Some(s.clone()),
                Value::Null => None,
                other => panic!("payload {other:?}"),
            };
            tx.execute(
                "INSERT INTO migrated(id, part, payload) VALUES(?1, ?2, ?3)",
                rusqlite::params![id, part, payload],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let sqlite_db = import_sqlite(&mut conn, &sqlite_mirror, &opts).unwrap().0;
    assert_kind(&sqlite_db, SourceKind::Sqlite);
    assert_eq!(
        cells(
            &sqlite_db,
            "SELECT payload FROM migrated WHERE id=$1 AND part=$2",
            &[Value::Int(1), Value::Text("wheel".into())],
        ),
        vec![vec![Value::Text("from-mysql".into())]]
    );
    conn.execute("UPDATE migrated SET payload='sqlite-commit' WHERE id=1 AND part='wheel'", [])
        .unwrap();
    let mut sqlite = SqliteAdapter::new(conn, opts.include.as_deref(), &[]).unwrap();
    assert!(drain_pull(&sqlite_db, &mut sqlite).unwrap() >= 1);
    assert_eq!(
        cells(
            &sqlite_db,
            "SELECT payload FROM migrated WHERE id=$1 AND part=$2",
            &[Value::Int(1), Value::Text("wheel".into())],
        ),
        vec![vec![Value::Text("sqlite-commit".into())]]
    );
    sqlite_db
        .query(
            "UPDATE migrated SET payload=$1 WHERE id=$2 AND part=$3",
            &[
                Value::Text("mpedb-commit".into()),
                Value::Int(1),
                Value::Text("wheel".into()),
            ],
        )
        .unwrap();
    let pushed = drain_push(&sqlite_db, &mut sqlite).unwrap();
    assert!(pushed.upserts >= 1, "an MPEdb change must be written back to SQLite");
    let stored: String = sqlite
        .conn()
        .query_row(
            "SELECT payload FROM migrated WHERE id=1 AND part='wheel'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, "mpedb-commit");
    assert_eq!(read_epoch(&sqlite_db).unwrap().authority, Authority::Source);
    assert_eq!(read_epoch(&mysql_db).unwrap().authority, Authority::Source);
    sqlite_db.verify().unwrap();
    drop(server);
    drop(_hold);
}

fn open_mirror(sock: &Path) -> mysql::Conn {
    let opts = mysql::OptsBuilder::new()
        .socket(Some(sock.display().to_string()))
        .user(Some("root"))
        .db_name(Some("mirror"));
    mysql::Conn::new(opts).unwrap()
}

fn payload_op(id: i64, part: &str, payload: &str) -> NetOp {
    NetOp {
        table_id: 0,
        pk: vec![Value::Int(id), Value::Text(part.into())],
        kind: NetOpKind::Upsert(vec![
            Value::Int(id),
            Value::Text(part.into()),
            Value::Text(payload.into()),
        ]),
    }
}

fn source_payload(conn: &mut mysql::Conn, id: i64, part: &str) -> Option<String> {
    let stored: Option<Option<String>> = conn
        .exec_first("SELECT payload FROM migrated WHERE id = ? AND part = ?", (id, part))
        .unwrap();
    stored.unwrap()
}

fn log_origin(conn: &mut mysql::Conn, id: i64, part: &str) -> Option<String> {
    let origin: Option<Option<String>> = conn
        .exec_first(
            "SELECT origin FROM `_mpedb_log_migrated` WHERE pk0 = ? AND pk1 = ?",
            (id, part),
        )
        .unwrap();
    origin.expect("changelog row")
}

fn mpedb_payload(db: &mpedb::Database, id: i64, part: &str) -> Vec<Vec<Value>> {
    cells(
        db,
        "SELECT payload FROM migrated WHERE id=$1 AND part=$2",
        &[Value::Int(id), Value::Text(part.into())],
    )
}

/// B holds the row lock, A blocks inside `push_checked`, then B commits.
/// A's check must see that commit and leave the cell. B's other row stays
/// origin NULL and the next pull applies it. A's own write is an echo.
#[test]
fn mysql_push_checked_keeps_foreign_commit_and_pulls_it() {
    let _hold = ONE_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let (server, spec) = boot("race");
    seed_migrated(&server.sock, "seed");
    let mut admin = open_mirror(&server.sock);
    for (id, part) in [(2i64, "other"), (3, "spare")] {
        admin
            .exec_drop(
                "INSERT INTO migrated (id, part, payload) VALUES (?, ?, ?)",
                (id, part, "seed"),
            )
            .unwrap();
    }
    drop(admin);
    let dest = server.dir.join("mirror.mpedb");
    let opts = ImportOptions {
        include: Some(vec!["migrated".into()]),
        ..Default::default()
    };
    let db = import_mysql(&spec, &dest, &opts).unwrap().0;
    assert_kind(&db, SourceKind::Mysql);

    let mut foreign = open_mirror(&server.sock);
    foreign.query_drop("START TRANSACTION").unwrap();
    let locked: Option<Option<String>> = foreign
        .exec_first(
            "SELECT payload FROM migrated WHERE id = ? AND part = ? FOR UPDATE",
            (1i64, "wheel"),
        )
        .unwrap();
    assert_eq!(locked, Some(Some("seed".into())));

    let spec_push = spec.clone();
    let handle = std::thread::spawn(move || {
        let include = vec!["migrated".to_string()];
        let mut adapter = MysqlAdapter::connect(&spec_push, Some(&include), &[]).unwrap();
        let cursor = adapter.zero_cursor();
        adapter
            .push_checked(
                &cursor,
                &[payload_op(1, "wheel", "from-mpedb"), payload_op(3, "spare", "from-mpedb")],
            )
            .unwrap()
    });

    // Look a few times, with a gap. A tight read of innodb_trx kept the
    // prepared FOR UPDATE out of the lock-wait list entirely.
    let mut watch = open_mirror(&server.sock);
    let mut waiting = false;
    let mut states: Vec<String> = Vec::new();
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(150));
        states = watch
            .query("SELECT trx_state FROM information_schema.innodb_trx")
            .unwrap_or_default();
        if states.iter().any(|s| s.to_ascii_uppercase().contains("LOCK WAIT")) {
            waiting = true;
            break;
        }
    }
    if !waiting {
        let _ = foreign.query_drop("ROLLBACK");
        let joined = handle.join();
        panic!("push_checked did not wait on the source row: states={states:?} join={joined:?}");
    }

    foreign
        .exec_drop(
            "UPDATE migrated SET payload = ? WHERE id = ? AND part = ?",
            ("from-b", 1i64, "wheel"),
        )
        .unwrap();
    foreign
        .exec_drop(
            "UPDATE migrated SET payload = ? WHERE id = ? AND part = ?",
            ("from-b", 2i64, "other"),
        )
        .unwrap();
    foreign.query_drop("COMMIT").unwrap();
    let flags = handle.join().expect("push thread");
    assert_eq!(flags, vec![false, true]);
    assert_eq!(source_payload(&mut watch, 1, "wheel").as_deref(), Some("from-b"));
    assert_eq!(source_payload(&mut watch, 2, "other").as_deref(), Some("from-b"));
    assert_eq!(source_payload(&mut watch, 3, "spare").as_deref(), Some("from-mpedb"));
    assert_eq!(log_origin(&mut watch, 1, "wheel"), None);
    assert_eq!(log_origin(&mut watch, 2, "other"), None);
    assert_eq!(log_origin(&mut watch, 3, "spare").as_deref(), Some("mpedb"));

    let include = vec!["migrated".to_string()];
    let mut adapter = MysqlAdapter::connect(&spec, Some(&include), &[]).unwrap();
    let pulled = drain_pull(&db, &mut adapter).unwrap();
    assert_eq!(pulled, 2, "a foreign commit inside the push must still be pulled");
    assert_eq!(mpedb_payload(&db, 1, "wheel"), vec![vec![Value::Text("from-b".into())]]);
    assert_eq!(mpedb_payload(&db, 2, "other"), vec![vec![Value::Text("from-b".into())]]);
    assert_eq!(mpedb_payload(&db, 3, "spare"), vec![vec![Value::Text("seed".into())]]);
    assert_eq!(drain_pull(&db, &mut adapter).unwrap(), 0);
    let epoch = read_epoch(&db).unwrap();
    assert_eq!(epoch.authority, Authority::Source);
    assert_eq!(epoch.epoch, 1);
    drop(server);
    drop(_hold);
}
