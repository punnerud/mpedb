#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_types::Value;
mod oracle_support;
#[test]
#[ignore = "requires Oracle application test owner and native client libraries"]
fn native_oracle_static_sql_procedures_execute_through_mpedb_shared_plans() {
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
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_STATIC_LEDGER PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute("CREATE TABLE MPEDB_STATIC_LEDGER(id NUMBER PRIMARY KEY,amount NUMBER,note VARCHAR2(20 CHAR))", &[]).unwrap();
    let definitions = [
        ("PROCEDURE", "MPEDB_STATIC_PUT", "CREATE OR REPLACE PROCEDURE MPEDB_STATIC_PUT(p_id NUMBER,p_amount NUMBER,p_note VARCHAR2) IS BEGIN DECLARE p_id NUMBER:=999; BEGIN INSERT INTO MPEDB_STATIC_LEDGER(id,amount,note) VALUES(MPEDB_STATIC_PUT.p_id,MPEDB_STATIC_PUT.p_amount,MPEDB_STATIC_PUT.p_note); END; END;"),
        ("PROCEDURE", "MPEDB_STATIC_BUMP", "CREATE OR REPLACE PROCEDURE MPEDB_STATIC_BUMP(p_id NUMBER,p_amount NUMBER) IS BEGIN UPDATE MPEDB_STATIC_LEDGER SET amount=amount+MPEDB_STATIC_BUMP.p_amount WHERE id=MPEDB_STATIC_BUMP.p_id; END;"),
        ("PROCEDURE", "MPEDB_STATIC_DELETE", "CREATE OR REPLACE PROCEDURE MPEDB_STATIC_DELETE(p_id NUMBER) IS BEGIN DELETE FROM MPEDB_STATIC_LEDGER WHERE id=MPEDB_STATIC_DELETE.p_id; END;"),
        ("FUNCTION", "MPEDB_STATIC_GET", "CREATE OR REPLACE FUNCTION MPEDB_STATIC_GET(p_id NUMBER) RETURN NUMBER IS n NUMBER; BEGIN SELECT t.amount INTO n FROM MPEDB_STATIC_LEDGER t WHERE t.id=MPEDB_STATIC_GET.p_id; RETURN n; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN NULL; END;"),
        ("FUNCTION", "MPEDB_STATIC_ROUND", "CREATE OR REPLACE FUNCTION MPEDB_STATIC_ROUND(p_id NUMBER) RETURN NUMBER IS n NUMBER(4,2); BEGIN SELECT t.amount INTO n FROM MPEDB_STATIC_LEDGER t WHERE t.id=MPEDB_STATIC_ROUND.p_id; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN -6502; END;"),
        ("FUNCTION", "MPEDB_STATIC_ALL", "CREATE OR REPLACE FUNCTION MPEDB_STATIC_ALL RETURN NUMBER IS n NUMBER; BEGIN SELECT t.amount INTO n FROM MPEDB_STATIC_LEDGER t; RETURN n; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN -1422; WHEN NO_DATA_FOUND THEN RETURN -1403; END;"),
        ("PROCEDURE", "MPEDB_STATIC_HANDLE", "CREATE OR REPLACE PROCEDURE MPEDB_STATIC_HANDLE IS BEGIN INSERT INTO MPEDB_STATIC_LEDGER VALUES(3,30,q'[own :literal]'); INSERT INTO MPEDB_STATIC_LEDGER VALUES(1,999,'duplicate'); EXCEPTION WHEN DUP_VAL_ON_INDEX THEN NULL; END;"),
    ];
    for (_, _, sql) in definitions {
        c.execute(sql, &[]).unwrap();
    }
    let source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_STATIC_LEDGER".into()]), &[])
        .unwrap();
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-static-sql-{}.mpedb", std::process::id()));
    let mut cfg = mpedb_types::Config::from_toml_str(&format!(
        "[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n",
        mpedb_testkit::toml_path(&path)
    ))
    .unwrap();
    cfg.schema = oracle::build_schema(&tables).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    let engine = mpedb_proc::ProcEngine::new(&db);
    let mut count = 0;
    for routine in source
        .routines()
        .unwrap()
        .into_iter()
        .filter(|r| definitions.iter().any(|(_, name, _)| *name == r.name))
    {
        engine
            .define(&routine.create_source(), mpedb_proc::Lang::PlSql)
            .unwrap();
        count += 1;
    }
    assert_eq!(count, definitions.len());
    for (id, amount, note) in [(1i64, "9007199254740992.125", "ø🙂"), (2, "1.235", "")] {
        c.execute(
            "BEGIN MPEDB_STATIC_PUT(:1,CAST(:2 AS NUMBER),:3); END;",
            &[&id, &amount, &note],
        )
        .unwrap();
        engine
            .call(
                "MPEDB_STATIC_PUT",
                &[
                    Value::Int(id),
                    Value::Numeric(amount.into()),
                    Value::Text(note.into()),
                ],
            )
            .unwrap();
    }
    c.execute(
        "BEGIN MPEDB_STATIC_BUMP(1,0.000000001); MPEDB_STATIC_HANDLE; END;",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    engine
        .call(
            "MPEDB_STATIC_BUMP",
            &[Value::Int(1), Value::Numeric("0.000000001".into())],
        )
        .unwrap();
    engine.call("MPEDB_STATIC_HANDLE", &[]).unwrap();
    for (name, id) in [
        ("MPEDB_STATIC_GET", 1),
        ("MPEDB_STATIC_GET", 999),
        ("MPEDB_STATIC_ROUND", 1),
        ("MPEDB_STATIC_ROUND", 2),
    ] {
        let expected: Option<String> = c
            .query_row_as(
                &format!(
                    "SELECT TO_CHAR({name}(:1),'TM9','NLS_NUMERIC_CHARACTERS=''.,''') FROM dual"
                ),
                &[&id],
            )
            .unwrap();
        let expected = expected.map_or(Value::Null, |s| {
            Value::Numeric(mpedb_types::value::parse_numeric(&s).unwrap())
        });
        assert_eq!(
            engine.call(name, &[Value::Int(id)]).unwrap(),
            mpedb_proc::ProcValue::Scalar(expected)
        );
    }
    assert_eq!(
        engine.call("MPEDB_STATIC_ALL", &[]).unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric("-1422".into()))
    );
    let (_, mut snapshots) = oracle_support::initial_snapshot(&source, &tables);
    let expected = snapshots.remove(0);
    let actual = match db
        .query("SELECT * FROM MPEDB_STATIC_LEDGER ORDER BY id", &[])
        .unwrap()
    {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    };
    assert_eq!(actual, expected);
    c.execute(
        "BEGIN MPEDB_STATIC_DELETE(1); MPEDB_STATIC_DELETE(2); MPEDB_STATIC_DELETE(3); END;",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    for id in 1..=3 {
        engine
            .call("MPEDB_STATIC_DELETE", &[Value::Int(id)])
            .unwrap();
    }
    assert_eq!(
        engine.call("MPEDB_STATIC_ALL", &[]).unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric("-1403".into()))
    );
    db.verify().unwrap();
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    assert_eq!(
        mpedb_proc::ProcEngine::new(&db)
            .call("MPEDB_STATIC_ALL", &[])
            .unwrap(),
        mpedb_proc::ProcValue::Scalar(Value::Numeric("-1403".into()))
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
    for (kind, name, _) in definitions {
        c.execute(&format!("DROP {kind} {name}"), &[]).unwrap();
    }
    c.execute("DROP TABLE MPEDB_STATIC_LEDGER PURGE", &[])
        .unwrap();
}
