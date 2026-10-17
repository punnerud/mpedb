#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle_source::OracleSource, sourcecfg, state, SourceSpec};
use mpedb_types::Value;
use std::{
    path::Path,
    process::{Command, Output},
};
fn invoke(args: &[&str], password: &str, connect: &str) -> (Output, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mpedb"))
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains(password),
        "CLI must not disclose Oracle credentials"
    );
    let safe = text
        .replace(password, "<redacted>")
        .replace(connect, "<redacted>");
    (out, safe)
}
#[test]
#[ignore = "requires native Oracle application test owner and private config"]
fn native_oracle_cli_import_pull_resume_and_epoch_guard() {
    let config = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(Path::new(&config)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    assert_eq!(username, "MPEDB_ORACLE_TEST");
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_CLI_ORACLE PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_CLI_ORACLE(id NUMBER PRIMARY KEY,flag BOOLEAN,payload CLOB)",
        &[],
    )
    .unwrap();
    c.execute(
        "INSERT INTO MPEDB_CLI_ORACLE VALUES(1,TRUE,TO_CLOB('initial ø🙂'))",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    let dir = mpedb_testkit::scratch_base().join(format!("oracle-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("live.mpedb");
    let dest = path.to_str().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (out, safe) = invoke(
            &[
                "mirror",
                "import",
                "--source-config",
                &config,
                "--dest",
                dest,
                "--include",
                "MPEDB_CLI_ORACLE",
                "--size_mb",
                "64",
                "--durability",
                "commit",
            ],
            password,
            connect_string,
        );
        if out.status.success() {
            break;
        }
        if safe.contains("ORA-01466") && !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
            continue;
        }
        panic!("Oracle CLI import failed: {safe}");
    }
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let metadata = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_SRC)
            .unwrap()
            .unwrap(),
        std::fs::canonicalize(&config)
            .unwrap()
            .to_str()
            .unwrap()
            .as_bytes()
    );
    assert_ne!(
        mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap(),
        [0; 16]
    );
    db.verify().unwrap();
    drop(db);
    c.execute(
        "UPDATE MPEDB_CLI_ORACLE SET id=2,flag=FALSE,payload=TO_CLOB('committed ø🙂') WHERE id=1",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    // Each invocation cold-opens the file and resolves its recorded config path.
    let (out, safe) = invoke(&["mirror", "pull", "--db", dest], password, connect_string);
    assert!(out.status.success(), "Oracle CLI pull failed: {safe}");
    let db = mpedb::Database::open_from_file(&path).unwrap();
    assert!(
        matches!(db.query("SELECT * FROM MPEDB_CLI_ORACLE",&[]).unwrap(),mpedb::ExecResult::Rows{rows,..} if rows==vec![vec![Value::Numeric("2".into()),Value::Bool(false),Value::Text("committed ø🙂".into())]])
    );
    drop(db);
    let (out, safe) = invoke(&["mirror", "pull", "--db", dest], password, connect_string);
    assert!(
        out.status.success(),
        "empty Oracle drain must finish: {safe}"
    );
    assert!(safe.contains("0 Oracle row change"), "{safe}");
    let (out, safe) = invoke(
        &["mirror", "status", "--db", dest],
        password,
        connect_string,
    );
    assert!(out.status.success(), "{safe}");
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let before = db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap();
    drop(db);
    let mut source = OracleSource::connect(&spec).unwrap();
    assert!(source
        .cas_source_state_fenced(
            &metadata.mirror_id,
            1,
            2,
            "source",
            &metadata.state_identity
        )
        .unwrap());
    let (out, safe) = invoke(&["mirror", "pull", "--db", dest], password, connect_string);
    assert!(!out.status.success());
    assert!(safe.contains("matching source authority"), "{safe}");
    let db = mpedb::Database::open_from_file(&path).unwrap();
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap(),
        before
    );
    db.verify().unwrap();
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&metadata.mirror_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    c.execute("DROP TABLE MPEDB_CLI_ORACLE PURGE", &[]).unwrap();
}
fn import_retry(args: &[&str], password: &str, connect: &str, dest: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (out, safe) = invoke(args, password, connect);
        if out.status.success() {
            return;
        }
        if safe.contains("ORA-01466") && !dest.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
            continue;
        }
        panic!("Oracle CLI import failed: {safe}");
    }
}
fn number(n: i64) -> Value {
    Value::Numeric(n.to_string())
}
#[test]
#[ignore = "requires isolated native MPEDB_ORACLE_FENCE_SOURCE_CONFIG"]
fn native_oracle_cli_push_requires_pinned_ledgers_and_refuses_cutover() {
    let config = std::env::var("MPEDB_ORACLE_FENCE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(Path::new(&config)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    assert_eq!(username, "MPEDB_ORACLE_FENCE");
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_CLI_PUSH PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_CLI_PUSH(id NUMBER PRIMARY KEY, amount NUMBER(4,1))",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_CLI_PUSH(id,amount) VALUES(1,1)", &[])
        .unwrap();
    c.commit().unwrap();
    let dir = mpedb_testkit::scratch_base().join(format!("oracle-cli-push-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let plain = dir.join("plain.mpedb");
    let pinned = dir.join("pinned.mpedb");
    let plain_s = plain.to_str().unwrap();
    let pinned_s = pinned.to_str().unwrap();
    import_retry(
        &[
            "mirror",
            "import",
            "--source-config",
            &config,
            "--dest",
            plain_s,
            "--include",
            "MPEDB_CLI_PUSH",
            "--size_mb",
            "64",
            "--durability",
            "commit",
        ],
        password,
        connect_string,
        &plain,
    );
    let db = mpedb::Database::open_from_file(&plain).unwrap();
    let plain_meta = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    assert!(plain_meta.receipt_ledger.is_none());
    assert!(plain_meta.origin_ledger.is_none());
    db.query(
        "UPDATE MPEDB_CLI_PUSH SET AMOUNT=$1 WHERE ID=$2",
        &[number(2), number(1)],
    )
    .unwrap();
    drop(db);
    let (out, safe) = invoke(
        &["mirror", "push", "--db", plain_s],
        password,
        connect_string,
    );
    assert!(!out.status.success(), "{safe}");
    assert!(safe.contains("--pin-ledgers"), "{safe}");
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_CLI_PUSH WHERE id=1", &[])
            .unwrap(),
        1
    );
    let db = mpedb::Database::open_from_file(&plain).unwrap();
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_none());
    drop(db);
    import_retry(
        &[
            "mirror",
            "import",
            "--source-config",
            &config,
            "--dest",
            pinned_s,
            "--include",
            "MPEDB_CLI_PUSH",
            "--size_mb",
            "64",
            "--durability",
            "commit",
            "--pin-ledgers",
        ],
        password,
        connect_string,
        &pinned,
    );
    let db = mpedb::Database::open_from_file(&pinned).unwrap();
    let pinned_meta = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    assert!(pinned_meta.receipt_ledger.is_some());
    assert!(pinned_meta.origin_ledger.is_some());
    db.query(
        "UPDATE MPEDB_CLI_PUSH SET AMOUNT=$1 WHERE ID=$2",
        &[number(2), number(1)],
    )
    .unwrap();
    drop(db);
    let (out, safe) = invoke(
        &["mirror", "push", "--db", pinned_s],
        password,
        connect_string,
    );
    assert!(out.status.success(), "{safe}");
    assert!(safe.contains("pushed 1 row change"), "{safe}");
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_CLI_PUSH WHERE id=1", &[])
            .unwrap(),
        2
    );
    let db = mpedb::Database::open_from_file(&pinned).unwrap();
    assert!(matches!(
        db.query("SELECT AMOUNT FROM MPEDB_CLI_PUSH WHERE ID=$1", &[number(1)])
            .unwrap(),
        mpedb::ExecResult::Rows { rows, .. } if rows == vec![vec![number(2)]]
    ));
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_none());
    drop(db);
    let (out, safe) = invoke(
        &["mirror", "push", "--db", pinned_s],
        password,
        connect_string,
    );
    assert!(out.status.success(), "{safe}");
    assert!(safe.contains("pushed 0 row change"), "{safe}");
    for cmd in [
        &["mirror", "sync", "--db", pinned_s][..],
        &["mirror", "switch", "--db", pinned_s, "--to", "mpedb"][..],
    ] {
        let (out, safe) = invoke(cmd, password, connect_string);
        assert!(!out.status.success(), "{cmd:?}: {safe}");
        assert!(
            safe.contains("cutover remain unavailable"),
            "{cmd:?}: {safe}"
        );
    }
    let db = mpedb::Database::open_from_file(&pinned).unwrap();
    db.query(
        "UPDATE MPEDB_CLI_PUSH SET AMOUNT=$1 WHERE ID=$2",
        &[number(3), number(1)],
    )
    .unwrap();
    let before = db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap();
    drop(db);
    let mut source = OracleSource::connect(&spec).unwrap();
    assert!(source
        .cas_source_state_fenced(
            &pinned_meta.mirror_id,
            1,
            2,
            "source",
            &pinned_meta.state_identity
        )
        .unwrap());
    let (out, safe) = invoke(
        &["mirror", "push", "--db", pinned_s],
        password,
        connect_string,
    );
    assert!(!out.status.success(), "{safe}");
    assert!(safe.contains("matching source authority"), "{safe}");
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_CLI_PUSH WHERE id=1", &[])
            .unwrap(),
        2,
        "a refused push must not write the newer local value"
    );
    let db = mpedb::Database::open_from_file(&pinned).unwrap();
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap(),
        before
    );
    assert!(matches!(
        db.query("SELECT AMOUNT FROM MPEDB_CLI_PUSH WHERE ID=$1", &[number(1)])
            .unwrap(),
        mpedb::ExecResult::Rows { rows, .. } if rows == vec![vec![number(3)]]
    ));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    for id in [&plain_meta.mirror_id, &pinned_meta.mirror_id] {
        c.execute(
            "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
            &[&id.as_bytes()],
        )
        .unwrap();
    }
    c.commit().unwrap();
    c.execute("DROP TABLE MPEDB_CLI_PUSH PURGE", &[]).unwrap();
}
