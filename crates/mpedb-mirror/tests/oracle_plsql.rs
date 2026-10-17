#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_types::Value;
mod oracle_support;

#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_application_dynamic_dml_runs_inside_mpedb_with_exact_binds() {
    let path = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&path)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_SQL_LEDGER PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute("CREATE TABLE MPEDB_SQL_LEDGER (id NUMBER PRIMARY KEY, amount NUMBER, note VARCHAR2(20 CHAR))",&[]).unwrap();
    c.execute("CREATE OR REPLACE PROCEDURE MPEDB_SQL_PUT(p_id NUMBER,p_amount NUMBER) IS BEGIN EXECUTE IMMEDIATE q'[INSERT INTO MPEDB_SQL_LEDGER (id,amount,note) VALUES(:x,:x,q'{:literal}')]' USING p_id,p_amount; END;",&[]).unwrap();
    c.execute("CREATE OR REPLACE FUNCTION MPEDB_SQL_GET(p_id NUMBER) RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT amount FROM MPEDB_SQL_LEDGER WHERE id=:key' INTO n USING p_id; RETURN n; END;",&[]).unwrap();
    c.execute("CREATE OR REPLACE FUNCTION MPEDB_SQL_ALL RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT amount FROM MPEDB_SQL_LEDGER' INTO n; RETURN n; END;",&[]).unwrap();
    c.execute("BEGIN MPEDB_SQL_PUT(1,9007199254740992.125); END;", &[])
        .unwrap();
    c.commit().unwrap();
    let source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_SQL_LEDGER".into()]), &[])
        .unwrap();
    let (_, mut images) = oracle_support::initial_snapshot(&source, &tables);
    let expected = images.remove(0);
    assert_eq!(
        expected[0][1],
        Value::Numeric("9007199254740992.125".into())
    );
    let routine = source
        .routines()
        .unwrap()
        .into_iter()
        .find(|r| r.name == "MPEDB_SQL_PUT" && r.kind == "PROCEDURE")
        .unwrap();
    let db_path = mpedb_testkit::scratch_base()
        .join(format!("oracle-dynamic-dml-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let mut config = mpedb_types::Config::from_toml_str(&format!(
        "[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n",
        mpedb_testkit::toml_path(&db_path)
    ))
    .unwrap();
    config.schema = oracle::build_schema(&tables).unwrap();
    let db = mpedb::Database::open_with_config(config).unwrap();
    let engine = mpedb_proc::ProcEngine::new(&db);
    let hash = engine
        .define(&routine.create_source(), mpedb_proc::Lang::PlSql)
        .unwrap();
    assert!(engine.info(&hash.to_string()).unwrap().writes);
    engine
        .call(
            "MPEDB_SQL_PUT",
            &[
                Value::Numeric("1".into()),
                Value::Numeric("9007199254740992.125".into()),
            ],
        )
        .unwrap();
    let actual = match db.query("SELECT * FROM MPEDB_SQL_LEDGER", &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    };
    assert_eq!(
        actual, expected,
        "same application source executes through MPEdb's native IR and SQL engine"
    );
    for routine in source
        .routines()
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.name.as_str(), "MPEDB_SQL_GET" | "MPEDB_SQL_ALL"))
    {
        engine
            .define(&routine.create_source(), mpedb_proc::Lang::PlSql)
            .unwrap();
    }
    let native: String = c
        .query_row_as(
            "SELECT TO_CHAR(MPEDB_SQL_GET(1),'TM9','NLS_NUMERIC_CHARACTERS=''.,''') FROM dual",
            &[],
        )
        .unwrap();
    assert_eq!(
        engine
            .call("MPEDB_SQL_GET", &[Value::Numeric("1".into())])
            .unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric(native))
    );
    let error = c
        .execute("DECLARE n NUMBER; BEGIN n:=MPEDB_SQL_GET(999); END;", &[])
        .unwrap_err();
    assert!(error.to_string().contains("ORA-01403"));
    assert!(matches!(
        engine.call("MPEDB_SQL_GET", &[Value::Numeric("999".into())]),
        Err(mpedb_types::Error::Oracle { code: 1403, .. })
    ));
    c.execute("BEGIN MPEDB_SQL_PUT(2,3); END;", &[]).unwrap();
    c.commit().unwrap();
    engine
        .call(
            "MPEDB_SQL_PUT",
            &[Value::Numeric("2".into()), Value::Numeric("3".into())],
        )
        .unwrap();
    let error = c
        .execute("DECLARE n NUMBER; BEGIN n:=MPEDB_SQL_ALL; END;", &[])
        .unwrap_err();
    assert!(error.to_string().contains("ORA-01422"));
    assert!(matches!(
        engine.call("MPEDB_SQL_ALL", &[]),
        Err(mpedb_types::Error::Oracle { code: 1422, .. })
    ));
    drop(engine);
    drop(db);
    let _ = std::fs::remove_file(db_path);
    c.execute("DROP PROCEDURE MPEDB_SQL_PUT", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_SQL_GET", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_SQL_ALL", &[]).unwrap();
    c.execute("DROP TABLE MPEDB_SQL_LEDGER PURGE", &[]).unwrap();
}
