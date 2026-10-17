#![cfg(feature = "oracle-source")]
//! Public native Oracle control for exception binding; no engine internals.
use mpedb_mirror::{oracle, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use mpedb_types::Value;
#[test]
#[ignore = "requires native MPEDB_ORACLE_SOURCE_CONFIG and Oracle client"]
fn native_exception_init_codes_sql_errors_aliases_and_boundaries_match() {
    let config = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&config)).unwrap();
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
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_EX_INIT_ROWS PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_EX_INIT_ROWS(id NUMBER PRIMARY KEY)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_EX_INIT_ROWS VALUES(1)", &[])
        .unwrap();
    c.commit().unwrap();
    let source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_EX_INIT_ROWS".into()]), &[])
        .unwrap();
    let path = mpedb_testkit::scratch_base().join(format!(
        "oracle-exception-init-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut cfg = mpedb_types::Config::from_toml_str(&format!(
        "[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n",
        mpedb_testkit::toml_path(&path)
    ))
    .unwrap();
    cfg.schema = oracle::build_schema(&tables).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    db.query(
        "INSERT INTO MPEDB_EX_INIT_ROWS VALUES($1)",
        &[Value::Numeric("1".into())],
    )
    .unwrap();
    let engine = ProcEngine::new(&db);
    let mut cases=vec![
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);BEGIN RAISE_APPLICATION_ERROR(-20001,'x');EXCEPTION WHEN e THEN RETURN SQLCODE;END;".to_string(),"-20001".to_string()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,100);BEGIN RAISE NO_DATA_FOUND;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".to_string(),"100".to_string()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1476);n NUMBER;BEGIN n:=1/0;RETURN 0;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".to_string(),"-1476".to_string()),
        ("e EXCEPTION;f EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);PRAGMA EXCEPTION_INIT(f,-20001);BEGIN RAISE e;EXCEPTION WHEN e OR f THEN RETURN SQLCODE;END;".to_string(),"-20001".to_string()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1);BEGIN RAISE e;EXCEPTION WHEN e OR DUP_VAL_ON_INDEX THEN RETURN SQLCODE;END;".to_string(),"-1".to_string()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);PRAGMA EXCEPTION_INIT(e,-20002);BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".to_string(),"-20002".to_string()),
        ("e EXCEPTION;BEGIN RAISE e;EXCEPTION WHEN e OR e THEN RETURN SQLCODE;END;".to_string(),"1".to_string()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-1);BEGIN INSERT INTO MPEDB_EX_INIT_ROWS(id) VALUES(1);RETURN 0;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".into(),"-1".into()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,100);n NUMBER;BEGIN SELECT t.id INTO n FROM MPEDB_EX_INIT_ROWS t WHERE t.id=99;RETURN 0;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".into(),"100".into()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);BEGIN BEGIN DECLARE e EXCEPTION;BEGIN RAISE e;END;EXCEPTION WHEN e THEN RETURN -99;WHEN OTHERS THEN RAISE;END;EXCEPTION WHEN e THEN RETURN -88;WHEN OTHERS THEN RETURN SQLCODE;END;".into(),"1".into()),
        ("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);BEGIN DECLARE e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-20001);BEGIN RAISE e;END;EXCEPTION WHEN e THEN RETURN SQLCODE;END;".into(),"-20001".into()),
        ("BEGIN RAISE ZERO_DIVIDE;EXCEPTION WHEN ZERO_DIVIDE OR ZERO_DIVIDE THEN RETURN SQLCODE;END;".into(),"-1476".into()),
    ];
    for (token, expected) in [
        ("-1.0", -1),
        ("-1e0", -1),
        ("-1f", -1),
        ("-1d", -1),
        ("-0.9", -1),
        ("-1.5", -2),
        ("-1.499999999f", -2),
        ("-1.499999999d", -1),
        ("-1.499999999", -1),
        ("-999999", -999999),
        ("-1000000", -1000000),
        ("+100", 100),
    ] {
        cases.push((format!("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,{token});BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN SQLCODE;END;"),expected.to_string()));
    }
    for (index, (body, expected)) in cases.iter().enumerate() {
        let name = format!("MPEDB_EX_INIT_{index}");
        let sql = format!("CREATE OR REPLACE FUNCTION {name} RETURN NUMBER IS {body}");
        c.execute(&sql, &[]).unwrap();
        // Anonymous invocation permits DML in the function, unlike SELECT.
        c.execute(&format!("BEGIN IF {name} != {expected} THEN RAISE_APPLICATION_ERROR(-20099,'native mismatch'); END IF; END;"),&[]).unwrap();
        engine.define(&sql, Lang::PlSql).unwrap();
        assert_eq!(
            engine.call(&name, &[]).unwrap(),
            ProcValue::Scalar(Value::Numeric(expected.clone())),
            "{body}"
        );
    }
    for code in [1400, 1403, 1426, 1476, 2290, 20001, 65535, 65536, 1000000] {
        let token = if code == 1403 { 100 } else { -code };
        let name = "MPEDB_EX_INIT_TEXT";
        let sql=format!("CREATE OR REPLACE FUNCTION {name} RETURN VARCHAR2 IS e EXCEPTION;PRAGMA EXCEPTION_INIT(e,{token});BEGIN RAISE e;EXCEPTION WHEN e THEN RETURN SQLERRM;END;");
        c.execute(&sql, &[]).unwrap();
        let native: String = c
            .query_row_as(&format!("SELECT {name}() FROM dual"), &[])
            .unwrap();
        engine.define(&sql, Lang::PlSql).unwrap();
        assert_eq!(
            engine.call(name, &[]).unwrap(),
            ProcValue::Scalar(Value::Text(native)),
            "code {code}"
        );
    }
    for code in [65536, 1000000] {
        let sql=format!("CREATE OR REPLACE FUNCTION MPEDB_EX_INIT_UNHANDLED RETURN NUMBER IS e EXCEPTION;PRAGMA EXCEPTION_INIT(e,-{code});BEGIN RAISE e;END;");
        c.execute(&sql, &[]).unwrap();
        let native = c
            .execute(
                "DECLARE n NUMBER;BEGIN n:=MPEDB_EX_INIT_UNHANDLED;END;",
                &[],
            )
            .unwrap_err();
        assert!(native
            .to_string()
            .contains(&format!("ORA-06515: PL/SQL: unhandled exception {code}")));
        engine.define(&sql, Lang::PlSql).unwrap();
        assert_eq!(
            engine
                .call("MPEDB_EX_INIT_UNHANDLED", &[])
                .unwrap_err()
                .to_string(),
            format!("ORA-06515: PL/SQL: unhandled exception {code}")
        );
    }
    for token in ["0", "1", "-1403", "-1000001", "100.5", "(-1)", "-20000-1"] {
        let declaration = format!("e EXCEPTION;PRAGMA EXCEPTION_INIT(e,{token});");
        assert!(c
            .execute(&format!("DECLARE {declaration} BEGIN NULL;END;"), &[])
            .is_err());
        assert!(engine.define(&format!("CREATE FUNCTION BAD_EX_INIT RETURN NUMBER IS {declaration} BEGIN RETURN 0;END;"),Lang::PlSql).is_err());
    }
    drop(engine);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let engine = ProcEngine::new(&db);
    for (index, (_, expected)) in cases.iter().enumerate() {
        let name = format!("MPEDB_EX_INIT_{index}");
        assert_eq!(
            engine.call(&name, &[]).unwrap(),
            ProcValue::Scalar(Value::Numeric(expected.clone()))
        );
        c.execute(&format!("DROP FUNCTION {name}"), &[]).unwrap();
    }
    assert!(
        matches!(db.query("SELECT * FROM MPEDB_EX_INIT_ROWS",&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric("1".into())]])
    );
    c.execute("DROP FUNCTION MPEDB_EX_INIT_TEXT", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_EX_INIT_UNHANDLED", &[])
        .unwrap();
    c.execute("DROP TABLE MPEDB_EX_INIT_ROWS PURGE", &[])
        .unwrap();
    drop(engine);
    drop(db);
    std::fs::remove_file(&path).unwrap();
}
