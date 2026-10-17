#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use mpedb_types::Value;
mod oracle_support;
fn numeric(engine: &ProcEngine<'_>, name: &str, input: Value) -> Value {
    match engine.call(name, &[input]).unwrap() {
        ProcValue::Scalar(v) => v,
        _ => panic!("scalar required"),
    }
}
#[test]
#[ignore = "requires native MPEDB_ORACLE_SOURCE_CONFIG and Oracle client"]
fn native_oracle_not_null_locals_match_initialization_assignments_and_sql_into() {
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
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_NOT_NULL_INPUT PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_NOT_NULL_INPUT(id NUMBER PRIMARY KEY,n NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_NOT_NULL_INPUT VALUES(1,NULL)", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_NOT_NULL_INPUT VALUES(2,2.5)", &[])
        .unwrap();
    c.commit().unwrap();
    let definitions=[
        ("MPEDB_NN_NUM","CREATE OR REPLACE FUNCTION MPEDB_NN_NUM(p NUMBER) RETURN NUMBER IS n NUMBER NOT NULL:=7; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;"),
        ("MPEDB_NN_INT","CREATE OR REPLACE FUNCTION MPEDB_NN_INT(p NUMBER) RETURN NUMBER IS n PLS_INTEGER NOT NULL:=7; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; WHEN OTHERS THEN RETURN SQLCODE; END;"),
        ("MPEDB_NN_TEXT","CREATE OR REPLACE FUNCTION MPEDB_NN_TEXT(p VARCHAR2) RETURN VARCHAR2 IS n VARCHAR2(8 CHAR) NOT NULL DEFAULT 'old'; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;"),
        ("MPEDB_NN_BOOL","CREATE OR REPLACE FUNCTION MPEDB_NN_BOOL(p BOOLEAN) RETURN NUMBER IS n BOOLEAN NOT NULL:=TRUE; BEGIN n:=p; IF n THEN RETURN 1; ELSE RETURN 0; END IF; EXCEPTION WHEN VALUE_ERROR THEN IF n THEN RETURN 7; ELSE RETURN 8; END IF; END;"),
        ("MPEDB_NN_INIT","CREATE OR REPLACE FUNCTION MPEDB_NN_INIT(p NUMBER) RETURN NUMBER IS BEGIN DECLARE n NUMBER NOT NULL:=p; BEGIN RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN 101; END; EXCEPTION WHEN VALUE_ERROR THEN RETURN 202; END;"),
        ("MPEDB_NN_SQL","CREATE OR REPLACE FUNCTION MPEDB_NN_SQL(p NUMBER) RETURN NUMBER IS n NUMBER NOT NULL:=7; BEGIN SELECT t.n INTO n FROM MPEDB_NOT_NULL_INPUT t WHERE t.id=MPEDB_NN_SQL.p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;"),
        ("MPEDB_NN_DYN","CREATE OR REPLACE FUNCTION MPEDB_NN_DYN(p NUMBER) RETURN NUMBER IS n NUMBER NOT NULL:=7; BEGIN EXECUTE IMMEDIATE 'SELECT n FROM MPEDB_NOT_NULL_INPUT WHERE id=:x' INTO n USING p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;"),
        ("MPEDB_NN_EMPTY","CREATE OR REPLACE FUNCTION MPEDB_NN_EMPTY RETURN VARCHAR2 IS n VARCHAR2(4) NOT NULL:=''; BEGIN RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN 'own'; END;"),
        ("MPEDB_NN_MESSAGE","CREATE OR REPLACE FUNCTION MPEDB_NN_MESSAGE(p NUMBER) RETURN VARCHAR2 IS n NUMBER NOT NULL:=1; BEGIN n:=p; RETURN 'ok'; EXCEPTION WHEN VALUE_ERROR THEN RETURN SQLERRM; END;"),
    ];
    for (_, sql) in definitions {
        c.execute(sql, &[]).unwrap();
    }
    let source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_NOT_NULL_INPUT".into()]), &[])
        .unwrap();
    let (_, rows) = oracle_support::initial_snapshot(&source, &tables);
    let path =
        mpedb_testkit::scratch_base().join(format!("oracle-not-null-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut cfg = mpedb_types::Config::from_toml_str(&format!(
        "[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n",
        mpedb_testkit::toml_path(&path)
    ))
    .unwrap();
    cfg.schema = oracle::build_schema(&tables).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    for row in &rows[0] {
        db.query("INSERT INTO MPEDB_NOT_NULL_INPUT VALUES($1,$2)", row)
            .unwrap();
    }
    let engine = ProcEngine::new(&db);
    for routine in source
        .routines()
        .unwrap()
        .into_iter()
        .filter(|r| definitions.iter().any(|(name, _)| *name == r.name))
    {
        engine
            .define(&routine.create_source(), Lang::PlSql)
            .unwrap();
    }
    let mut checked = 0;
    for name in ["MPEDB_NN_NUM", "MPEDB_NN_INT", "MPEDB_NN_INIT"] {
        for input in [
            None,
            Some("0"),
            Some("1.5"),
            Some("-2.5"),
            Some("2147483648"),
            Some("12345678901234567890.125"),
        ] {
            let native:Option<String>=c.query_row_as(&format!("SELECT TO_CHAR({name}(TO_NUMBER(:1)),'TM9','NLS_NUMERIC_CHARACTERS=''.,''') FROM dual"),&[&input]).unwrap();
            let expected = native.map_or(Value::Null, Value::Numeric);
            let input = input.map_or(Value::Null, |v| Value::Numeric(v.into()));
            assert_eq!(numeric(&engine, name, input), expected, "{name}");
            checked += 1;
        }
    }
    for input in [
        None,
        Some(""),
        Some("abc"),
        Some("ø🙂漢"),
        Some("123456789"),
    ] {
        let native: Option<String> = c
            .query_row_as("SELECT MPEDB_NN_TEXT(:1) FROM dual", &[&input])
            .unwrap();
        assert_eq!(
            numeric(
                &engine,
                "MPEDB_NN_TEXT",
                input.map_or(Value::Null, |v| Value::Text(v.into()))
            ),
            native.map_or(Value::Null, Value::Text)
        );
        checked += 1;
    }
    for input in [None, Some(false), Some(true)] {
        let native: String = c
            .query_row_as(
                "SELECT TO_CHAR(MPEDB_NN_BOOL(:1),'TM9') FROM dual",
                &[&input],
            )
            .unwrap();
        assert_eq!(
            numeric(
                &engine,
                "MPEDB_NN_BOOL",
                input.map_or(Value::Null, Value::Bool)
            ),
            Value::Numeric(native)
        );
        checked += 1;
    }
    for name in ["MPEDB_NN_SQL", "MPEDB_NN_DYN"] {
        for input in [1i64, 2] {
            let native: Option<String> = c
                .query_row_as(
                    &format!("SELECT TO_CHAR({name}(:1),'TM9') FROM dual"),
                    &[&input],
                )
                .unwrap();
            assert_eq!(
                numeric(&engine, name, Value::Numeric(input.to_string())),
                native.map_or(Value::Null, Value::Numeric)
            );
            checked += 1;
        }
    }
    let native: String = c
        .query_row_as("SELECT MPEDB_NN_MESSAGE(NULL) FROM dual", &[])
        .unwrap();
    assert_eq!(
        numeric(&engine, "MPEDB_NN_MESSAGE", Value::Null),
        Value::Text(native)
    );
    checked += 1;
    assert!(c
        .execute("DECLARE x VARCHAR2(20); BEGIN x:=MPEDB_NN_EMPTY; END;", &[])
        .unwrap_err()
        .to_string()
        .contains("ORA-06502"));
    assert!(matches!(
        engine.call("MPEDB_NN_EMPTY", &[]),
        Err(mpedb_types::Error::Oracle { code: 6502, .. })
    ));
    checked += 1;
    for body in [
        "n NUMBER NOT NULL; BEGIN RETURN 1; END;",
        "n NUMBER NOT NULL:=NULL; BEGIN RETURN 1; END;",
        "n NUMBER NOT NULL:=1; BEGIN n:=NULL; RETURN n; END;",
        "n NUMBER NOT NULL:=1; BEGIN IF FALSE THEN n:=NULL; END IF; RETURN n; END;",
    ] {
        let anonymous = body
            .replace("RETURN 1;", "NULL;")
            .replace("RETURN n;", "NULL;");
        assert!(c
            .execute(&format!("DECLARE {anonymous}"), &[])
            .unwrap_err()
            .to_string()
            .contains("ORA-06550"));
        assert!(engine
            .define(
                &format!("CREATE FUNCTION BAD_NOT_NULL RETURN NUMBER IS {body}"),
                Lang::PlSql
            )
            .is_err());
        checked += 1;
    }
    db.create_function(mpedb::spellfn::SpellLang::PlSql, definitions[0].1)
        .unwrap();
    assert!(
        matches!(db.query("SELECT MPEDB_NN_NUM(NULL)",&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric("7".into())]])
    );
    drop(engine);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let engine = ProcEngine::new(&db);
    assert_eq!(
        numeric(&engine, "MPEDB_NN_NUM", Value::Null),
        Value::Numeric("7".into())
    );
    assert!(
        matches!(db.query("SELECT MPEDB_NN_NUM(NULL)",&[]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric("7".into())]])
    );
    assert_eq!(checked, 36);
    drop(engine);
    drop(db);
    std::fs::remove_file(&path).unwrap();
    for (name, _) in definitions {
        c.execute(&format!("DROP FUNCTION {name}"), &[]).unwrap();
    }
    c.execute("DROP TABLE MPEDB_NOT_NULL_INPUT PURGE", &[])
        .unwrap();
}
