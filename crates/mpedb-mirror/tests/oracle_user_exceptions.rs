#![cfg(feature = "oracle-source")]
//! Black-box comparison using only our own source and Oracle's public SQL API.
use mpedb_mirror::{sourcecfg, SourceSpec};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use mpedb_types::Value;
#[test]
#[ignore = "requires native MPEDB_ORACLE_SOURCE_CONFIG and Oracle client"]
fn native_user_exceptions_match_scopes_reraise_and_cold_catalog() {
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
    let path = mpedb_testkit::scratch_base().join(format!(
        "oracle-user-exceptions-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let cfg=mpedb_types::Config::from_toml_str(&format!("[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n[[table]]\nname=\"carrier\"\nprimary_key=[\"id\"]\n[[table.column]]\nname=\"id\"\ntype=\"int64\"\n",mpedb_testkit::toml_path(&path))).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    let cases=[
        ("e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN DUP_VAL_ON_INDEX THEN RETURN -99; WHEN e THEN RETURN SQLCODE; END;","1"),
        ("e EXCEPTION; BEGIN RAISE DUP_VAL_ON_INDEX; EXCEPTION WHEN e THEN RETURN -99; WHEN DUP_VAL_ON_INDEX THEN RETURN SQLCODE; END;","-1"),
        ("e EXCEPTION; BEGIN DECLARE e EXCEPTION; BEGIN RAISE e; END; EXCEPTION WHEN e THEN RETURN -99; WHEN OTHERS THEN RETURN SQLCODE; END;","1"),
        ("e EXCEPTION; BEGIN BEGIN RAISE e; EXCEPTION WHEN OTHERS THEN DECLARE e EXCEPTION; BEGIN RAISE; EXCEPTION WHEN e THEN RETURN -99; WHEN OTHERS THEN RETURN 2; END; END; END;","2"),
        ("e NUMBER:=7; BEGIN DECLARE e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN e THEN NULL; END; RETURN e; END;","7"),
        ("ZERO_DIVIDE EXCEPTION; BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN SQLCODE; END;","1"),
        ("e EXCEPTION; BEGIN BEGIN RAISE e; EXCEPTION WHEN e THEN RAISE; END; EXCEPTION WHEN e THEN RETURN 9; END;","9"),
    ];
    for (index, (body, expected)) in cases.iter().enumerate() {
        let name = format!("MPEDB_USER_EX_{index}");
        let sql = format!("CREATE OR REPLACE FUNCTION {name} RETURN NUMBER IS {body}");
        c.execute(&sql, &[]).unwrap();
        let native: String = c
            .query_row_as(&format!("SELECT TO_CHAR({name}(),'TM9') FROM dual"), &[])
            .unwrap();
        assert_eq!(&native, expected, "{body}");
        engine.define(&sql, Lang::PlSql).unwrap();
        assert_eq!(
            engine.call(&name, &[]).unwrap(),
            ProcValue::Scalar(Value::Numeric(native.clone())),
            "{body}"
        );
        db.create_function(mpedb::spellfn::SpellLang::PlSql, &sql)
            .unwrap();
        assert!(
            matches!(db.query(&format!("SELECT {name}()"),&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric(native)]])
        );
    }
    let text="CREATE OR REPLACE FUNCTION MPEDB_USER_EX_TEXT RETURN VARCHAR2 IS e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN e THEN RETURN SQLERRM; END;";
    c.execute(text, &[]).unwrap();
    let native: String = c
        .query_row_as("SELECT MPEDB_USER_EX_TEXT() FROM dual", &[])
        .unwrap();
    assert_eq!(native, "User-Defined Exception");
    engine.define(text, Lang::PlSql).unwrap();
    assert_eq!(
        engine.call("MPEDB_USER_EX_TEXT", &[]).unwrap(),
        ProcValue::Scalar(Value::Text(native))
    );
    let unhandled="CREATE OR REPLACE FUNCTION MPEDB_USER_EX_UNHANDLED RETURN NUMBER IS e EXCEPTION; BEGIN RAISE e; END;";
    c.execute(unhandled, &[]).unwrap();
    let native = c
        .query_row_as::<String>("SELECT TO_CHAR(MPEDB_USER_EX_UNHANDLED()) FROM dual", &[])
        .unwrap_err();
    assert!(native.to_string().contains("ORA-06510"));
    engine.define(unhandled, Lang::PlSql).unwrap();
    assert!(matches!(
        engine.call("MPEDB_USER_EX_UNHANDLED", &[]),
        Err(mpedb_types::Error::OracleUserException { .. })
    ));
    drop(engine);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let engine = ProcEngine::new(&db);
    for (index, (_, expected)) in cases.iter().enumerate() {
        let name = format!("MPEDB_USER_EX_{index}");
        assert_eq!(
            engine.call(&name, &[]).unwrap(),
            ProcValue::Scalar(Value::Numeric((*expected).into()))
        );
        assert!(
            matches!(db.query(&format!("SELECT {name}()"),&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric((*expected).into())]])
        );
        c.execute(&format!("DROP FUNCTION {name}"), &[]).unwrap();
    }
    c.execute("DROP FUNCTION MPEDB_USER_EX_TEXT", &[]).unwrap();
    c.execute("DROP FUNCTION MPEDB_USER_EX_UNHANDLED", &[])
        .unwrap();
    drop(engine);
    drop(db);
    std::fs::remove_file(&path).unwrap();
}
