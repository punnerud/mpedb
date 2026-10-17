#![cfg(feature = "oracle-source")]
//! Native control only: public SQL and our own routine definitions.
use mpedb_mirror::{sourcecfg, SourceSpec};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use mpedb_types::Value;
#[test]
#[ignore = "requires native MPEDB_ORACLE_SOURCE_CONFIG and Oracle client"]
fn native_predefined_names_and_sqlerrm_arguments_match_after_cold_reopen() {
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
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-predefined-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg=mpedb_types::Config::from_toml_str(&format!("[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n[[table]]\nname=\"carrier\"\nprimary_key=[\"id\"]\n[[table.column]]\nname=\"id\"\ntype=\"int64\"\n",mpedb_testkit::toml_path(&path))).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    let names = [
        ("ACCESS_INTO_NULL", -6530),
        ("CASE_NOT_FOUND", -6592),
        ("COLLECTION_IS_NULL", -6531),
        ("CURSOR_ALREADY_OPEN", -6511),
        ("DUP_VAL_ON_INDEX", -1),
        ("INVALID_CURSOR", -1001),
        ("INVALID_NUMBER", -1722),
        ("LOGIN_DENIED", -1017),
        ("NO_DATA_FOUND", 100),
        ("NO_DATA_NEEDED", -6548),
        ("NOT_LOGGED_ON", -1012),
        ("PROGRAM_ERROR", -6501),
        ("ROWTYPE_MISMATCH", -6504),
        ("SELF_IS_NULL", -30625),
        ("STORAGE_ERROR", -6500),
        ("SUBSCRIPT_BEYOND_COUNT", -6533),
        ("SUBSCRIPT_OUTSIDE_LIMIT", -6532),
        ("SYS_INVALID_ROWID", -1410),
        ("TIMEOUT_ON_RESOURCE", -51),
        ("TOO_MANY_ROWS", -1422),
        ("VALUE_ERROR", -6502),
        ("ZERO_DIVIDE", -1476),
    ];
    let mut expected = Vec::new();
    for (index, (exception, code)) in names.iter().enumerate() {
        for text in [false, true] {
            let name = format!(
                "MPEDB_PREDEF_{index}_{}",
                if text { "TEXT" } else { "CODE" }
            );
            let result = if text { "VARCHAR2" } else { "NUMBER" };
            let expr = if text { "SQLERRM" } else { "SQLCODE" };
            let sql=format!("CREATE OR REPLACE FUNCTION {name} RETURN {result} IS BEGIN RAISE {exception};EXCEPTION WHEN {exception} THEN RETURN {expr};END;");
            c.execute(&sql, &[]).unwrap();
            let query = if text {
                format!("SELECT {name}() FROM dual")
            } else {
                format!("SELECT TO_CHAR({name}(),'TM9') FROM dual")
            };
            let native: String = c.query_row_as(&query, &[]).unwrap();
            if !text {
                assert_eq!(native, code.to_string(), "{exception}");
            }
            let value = if text {
                Value::Text(native)
            } else {
                Value::Numeric(native)
            };
            engine.define(&sql, Lang::PlSql).unwrap();
            assert_eq!(
                engine.call(&name, &[]).unwrap(),
                ProcValue::Scalar(value.clone()),
                "{exception}"
            );
            db.create_function(mpedb::spellfn::SpellLang::PlSql, &sql)
                .unwrap();
            assert!(
                matches!(db.query(&format!("SELECT {name}()"),&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![value.clone()]])
            );
            expected.push((name, value));
        }
    }
    let qualified="CREATE OR REPLACE FUNCTION MPEDB_PREDEF_QUALIFIED RETURN NUMBER IS ZERO_DIVIDE EXCEPTION;BEGIN RAISE STANDARD.ZERO_DIVIDE;EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 999;WHEN STANDARD.ZERO_DIVIDE THEN RETURN SQLCODE;END;";
    c.execute(qualified, &[]).unwrap();
    engine.define(qualified, Lang::PlSql).unwrap();
    let native: String = c
        .query_row_as(
            "SELECT TO_CHAR(MPEDB_PREDEF_QUALIFIED(),'TM9') FROM dual",
            &[],
        )
        .unwrap();
    assert_eq!(native, "-1476");
    assert_eq!(
        engine.call("MPEDB_PREDEF_QUALIFIED", &[]).unwrap(),
        ProcValue::Scalar(Value::Numeric(native.clone()))
    );
    expected.push(("MPEDB_PREDEF_QUALIFIED".into(), Value::Numeric(native)));
    let explicit="CREATE OR REPLACE FUNCTION MPEDB_PREDEF_EXPLICIT(p NUMBER) RETURN VARCHAR2 IS BEGIN RETURN SQLERRM(p);END;";
    c.execute(explicit, &[]).unwrap();
    engine.define(explicit, Lang::PlSql).unwrap();
    db.create_function(mpedb::spellfn::SpellLang::PlSql, explicit)
        .unwrap();
    let args = [
        "NULL",
        "0",
        "1",
        "2",
        "100",
        "1403",
        "-1403",
        "-1",
        "-1476",
        "-20001",
        "20001",
        "-65535",
        "-65536",
        "65536",
        "-1000000",
        "-1000001",
        "2147483647",
        "-2147483648",
        "-1.5",
        "1.5",
        "-0.4",
    ];
    let mut explicit_expected = Vec::new();
    for arg in args {
        let input = if arg == "NULL" {
            Value::Null
        } else {
            Value::Numeric(arg.into())
        };
        let native: Option<String> = c
            .query_row_as(
                &format!("SELECT MPEDB_PREDEF_EXPLICIT({arg}) FROM dual"),
                &[],
            )
            .unwrap();
        let value = native.map_or(Value::Null, Value::Text);
        assert_eq!(
            engine
                .call("MPEDB_PREDEF_EXPLICIT", std::slice::from_ref(&input))
                .unwrap(),
            ProcValue::Scalar(value.clone()),
            "{arg}"
        );
        assert!(
            matches!(db.query("SELECT MPEDB_PREDEF_EXPLICIT($1)",std::slice::from_ref(&input)).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![value.clone()]])
        );
        explicit_expected.push((input, value));
    }
    for arg in ["2147483648", "-2147483649"] {
        let native = c
            .query_row_as::<String>(
                &format!("SELECT MPEDB_PREDEF_EXPLICIT({arg}) FROM dual"),
                &[],
            )
            .unwrap_err();
        assert!(native.to_string().contains("ORA-01426"));
        assert!(matches!(
            engine.call("MPEDB_PREDEF_EXPLICIT", &[Value::Numeric(arg.into())]),
            Err(mpedb_types::Error::Oracle { code: 1426, .. })
        ));
    }
    for (index, expr) in [
        "SQLERRM(SQLCODE)",
        "SQLERRM(-20001)",
        "SQLERRM(-1)",
        "SQLERRM(NULL)",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("MPEDB_PREDEF_ACTIVE_{index}");
        let sql=format!("CREATE OR REPLACE FUNCTION {name} RETURN VARCHAR2 IS BEGIN RAISE_APPLICATION_ERROR(-20001,'ø detail');EXCEPTION WHEN OTHERS THEN RETURN {expr};END;");
        c.execute(&sql, &[]).unwrap();
        engine.define(&sql, Lang::PlSql).unwrap();
        let native: Option<String> = c
            .query_row_as(&format!("SELECT {name}() FROM dual"), &[])
            .unwrap();
        let value = native.map_or(Value::Null, Value::Text);
        assert_eq!(
            engine.call(&name, &[]).unwrap(),
            ProcValue::Scalar(value.clone())
        );
        expected.push((name, value));
    }
    drop(engine);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let engine = ProcEngine::new(&db);
    for (name, value) in expected {
        assert_eq!(engine.call(&name, &[]).unwrap(), ProcValue::Scalar(value));
        c.execute(&format!("DROP FUNCTION {name}"), &[]).unwrap();
    }
    for (input, value) in explicit_expected {
        assert_eq!(
            engine
                .call("MPEDB_PREDEF_EXPLICIT", std::slice::from_ref(&input))
                .unwrap(),
            ProcValue::Scalar(value.clone())
        );
        assert!(
            matches!(db.query("SELECT MPEDB_PREDEF_EXPLICIT($1)",std::slice::from_ref(&input)).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![value.clone()]])
        );
    }
    c.execute("DROP FUNCTION MPEDB_PREDEF_EXPLICIT", &[])
        .unwrap();
    drop(engine);
    drop(db);
    std::fs::remove_file(&path).unwrap();
}
