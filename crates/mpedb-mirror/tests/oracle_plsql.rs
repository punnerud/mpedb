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
    c.execute("CREATE OR REPLACE FUNCTION MPEDB_SQL_DEFAULT(p_id NUMBER) RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT amount FROM MPEDB_SQL_LEDGER WHERE id=:x' INTO n USING p_id; RETURN n; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN 123; END;",&[]).unwrap();
    c.execute("CREATE OR REPLACE PROCEDURE MPEDB_SQL_HANDLE IS a NUMBER:=3; b NUMBER:=30; original NUMBER:=1; val NUMBER:=999; delta NUMBER:=10; note VARCHAR2(20):='duplicate'; BEGIN EXECUTE IMMEDIATE 'UPDATE MPEDB_SQL_LEDGER SET amount=amount+:x WHERE id=:x' USING delta,original; BEGIN EXECUTE IMMEDIATE 'INSERT INTO MPEDB_SQL_LEDGER(id,amount,note) VALUES(:x,:x,:x),(:x,:x,:x)' USING a,b,note,original,val,note; EXCEPTION WHEN DUP_VAL_ON_INDEX THEN NULL; END; END;",&[]).unwrap();
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
    for routine in source.routines().unwrap().into_iter().filter(|r| {
        matches!(
            r.name.as_str(),
            "MPEDB_SQL_GET" | "MPEDB_SQL_ALL" | "MPEDB_SQL_DEFAULT" | "MPEDB_SQL_HANDLE"
        )
    }) {
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
    let native: String = c
        .query_row_as(
            "SELECT TO_CHAR(MPEDB_SQL_DEFAULT(999),'TM9') FROM dual",
            &[],
        )
        .unwrap();
    assert_eq!(
        engine
            .call("MPEDB_SQL_DEFAULT", &[Value::Numeric("999".into())])
            .unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric(native))
    );
    c.execute("BEGIN MPEDB_SQL_HANDLE; END;", &[]).unwrap();
    c.commit().unwrap();
    engine.call("MPEDB_SQL_HANDLE", &[]).unwrap();
    let native = source
        .read_snapshot(&tables[0], source.current_scn().unwrap())
        .unwrap();
    let actual = match db
        .query("SELECT * FROM MPEDB_SQL_LEDGER ORDER BY id", &[])
        .unwrap()
    {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    };
    assert_eq!(actual, native, "caught multi-row failure preserves prior writes and removes every row of the failed statement");
    assert_eq!(actual.len(), 2);
    drop(engine);
    drop(db);
    let _ = std::fs::remove_file(db_path);
    c.execute("DROP PROCEDURE MPEDB_SQL_PUT", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_SQL_GET", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_SQL_ALL", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_SQL_DEFAULT", &[]).unwrap();
    c.execute("DROP PROCEDURE MPEDB_SQL_HANDLE", &[]).unwrap();
    c.execute("DROP TABLE MPEDB_SQL_LEDGER PURGE", &[]).unwrap();
}

#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_error_reporting_and_application_error_limits_match_mpedb() {
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
    let definitions=[
        ("MPEDB_ERR_CODE","CREATE OR REPLACE FUNCTION MPEDB_ERR_CODE(p PLS_INTEGER) RETURN NUMBER IS BEGIN IF p=0 THEN RETURN SQLCODE; ELSIF p=1 THEN RAISE NO_DATA_FOUND; ELSE RAISE_APPLICATION_ERROR(-20001,'ø'); END IF; EXCEPTION WHEN OTHERS THEN RETURN SQLCODE(); END;"),
        ("MPEDB_ERR_TEXT","CREATE OR REPLACE FUNCTION MPEDB_ERR_TEXT(p PLS_INTEGER) RETURN VARCHAR2 IS n NUMBER; BEGIN IF p=0 THEN RETURN SQLERRM(); ELSIF p=1 THEN RAISE NO_DATA_FOUND; ELSE n:=1/0; END IF; RETURN SQLERRM; EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;"),
        ("MPEDB_ERR_LONG","CREATE OR REPLACE FUNCTION MPEDB_ERR_LONG(p VARCHAR2) RETURN VARCHAR2 IS BEGIN RAISE_APPLICATION_ERROR(-20001,p); EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;"),
        ("MPEDB_ERR_RANGE","CREATE OR REPLACE FUNCTION MPEDB_ERR_RANGE(p NUMBER) RETURN NUMBER IS BEGIN RAISE_APPLICATION_ERROR(p,'test',FALSE); EXCEPTION WHEN OTHERS THEN RETURN SQLCODE; END;"),
        ("MPEDB_ERR_RESTORE","CREATE OR REPLACE FUNCTION MPEDB_ERR_RESTORE RETURN NUMBER IS n NUMBER:=0; BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN BEGIN RAISE NO_DATA_FOUND; EXCEPTION WHEN NO_DATA_FOUND THEN n:=SQLCODE; END; RETURN n+SQLCODE; END;"),
        ("MPEDB_ERR_UNHANDLED","CREATE OR REPLACE FUNCTION MPEDB_ERR_UNHANDLED(p VARCHAR2) RETURN NUMBER IS BEGIN RAISE_APPLICATION_ERROR(-20001,p); RETURN 0; END;"),
    ];
    for (_, sql) in &definitions {
        c.execute(sql, &[]).unwrap();
    }
    let source = OracleSource::connect(&spec).unwrap();
    let path =
        mpedb_testkit::scratch_base().join(format!("oracle-errors-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg=mpedb_types::Config::from_toml_str(&format!("[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n[[table]]\nname=\"carrier\"\nprimary_key=[\"id\"]\n[[table.column]]\nname=\"id\"\ntype=\"int64\"\n",mpedb_testkit::toml_path(&path))).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    let engine = mpedb_proc::ProcEngine::new(&db);
    for routine in source
        .routines()
        .unwrap()
        .into_iter()
        .filter(|r| definitions.iter().any(|(name, _)| *name == r.name))
    {
        engine
            .define(&routine.create_source(), mpedb_proc::Lang::PlSql)
            .unwrap();
    }
    for mode in 0i64..3 {
        let n: String = c
            .query_row_as(
                "SELECT TO_CHAR(MPEDB_ERR_CODE(:1),'TM9') FROM dual",
                &[&mode],
            )
            .unwrap();
        assert_eq!(
            engine.call("MPEDB_ERR_CODE", &[Value::Int(mode)]).unwrap(),
            mpedb_proc::ProcValue::Scalar(Value::Numeric(n))
        );
        let text: String = c
            .query_row_as("SELECT MPEDB_ERR_TEXT(:1) FROM dual", &[&mode])
            .unwrap();
        assert_eq!(
            engine.call("MPEDB_ERR_TEXT", &[Value::Int(mode)]).unwrap(),
            mpedb_proc::ProcValue::Scalar(Value::Text(text))
        );
    }
    for text in [
        "x".repeat(2500),
        "ø".repeat(1500),
        "🙂".repeat(700),
        format!("a{}", "ø".repeat(1500)),
    ] {
        let native: String = c
            .query_row_as("SELECT MPEDB_ERR_LONG(:1) FROM dual", &[&text])
            .unwrap();
        assert_eq!(
            engine.call("MPEDB_ERR_LONG", &[Value::Text(text)]).unwrap(),
            mpedb_proc::ProcValue::Scalar(Value::Text(native))
        );
    }
    for value in [
        Some("-20000"),
        Some("-20999"),
        Some("-20000.5"),
        Some("-19999"),
        Some("-21000"),
        None,
    ] {
        let native: String = c
            .query_row_as(
                "SELECT TO_CHAR(MPEDB_ERR_RANGE(:1),'TM9') FROM dual",
                &[&value],
            )
            .unwrap();
        let value = value.map_or(Value::Null, |v| Value::Numeric(v.into()));
        assert_eq!(
            engine.call("MPEDB_ERR_RANGE", &[value]).unwrap(),
            mpedb_proc::ProcValue::Scalar(Value::Numeric(native))
        );
    }
    let native: String = c
        .query_row_as("SELECT TO_CHAR(MPEDB_ERR_RESTORE,'TM9') FROM dual", &[])
        .unwrap();
    assert_eq!(
        engine.call("MPEDB_ERR_RESTORE", &[]).unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric(native))
    );
    let long = "x".repeat(2500);
    let native = c
        .execute(
            "DECLARE n NUMBER; BEGIN n:=MPEDB_ERR_UNHANDLED(:1); END;",
            &[&long],
        )
        .unwrap_err()
        .to_string();
    let line = native
        .lines()
        .find(|line| line.contains("ORA-20001: "))
        .unwrap();
    let body = line.split_once("ORA-20001: ").unwrap().1;
    let error = engine
        .call("MPEDB_ERR_UNHANDLED", &[Value::Text(long)])
        .unwrap_err();
    assert!(matches!(error,mpedb_types::Error::Oracle{code:20001,ref message} if message==body));
    drop(engine);
    drop(db);
    let _ = std::fs::remove_file(path);
    for (name, _) in definitions {
        c.execute(&format!("DROP FUNCTION {name}"), &[]).unwrap();
    }
}
