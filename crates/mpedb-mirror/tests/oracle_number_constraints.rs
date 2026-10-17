#![cfg(feature = "oracle-source")]
use mpedb_mirror::{sourcecfg, SourceSpec};
use mpedb_types::Value;
#[test]
#[ignore = "requires Oracle application test owner and native client libraries"]
fn native_oracle_number_constraints_match_native_mpedb_plsql() {
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
    c.execute("ALTER SESSION SET NLS_NUMERIC_CHARACTERS='.,'", &[])
        .unwrap();
    let path = mpedb_testkit::scratch_base().join(format!(
        "oracle-number-constraints-{}.mpedb",
        std::process::id()
    ));
    let cfg=mpedb_types::Config::from_toml_str(&format!("[database]\npath=\"{}\"\nsize_mb=32\ndurability=\"none\"\n[[table]]\nname=\"carrier\"\nprimary_key=[\"id\"]\n[[table.column]]\nname=\"id\"\ntype=\"int64\"\n",mpedb_testkit::toml_path(&path))).unwrap();
    let db = mpedb::Database::open_with_config(cfg).unwrap();
    let engine = mpedb_proc::ProcEngine::new(&db);
    let inputs = [
        None,
        Some("0"),
        Some("1.235"),
        Some("-1.235"),
        Some("99.994"),
        Some("99.995"),
        Some("99949"),
        Some("99950"),
        Some("-150"),
        Some("0.012345"),
        Some("0.099995"),
        Some("1e-130"),
        Some("1e-127"),
        Some("1e125"),
        Some("99999999999999999999999999999999999999"),
        Some("99999999999999999999999999999999999999.5"),
    ];
    for ty in [
        "NUMBER(4,2)",
        "NUMBER(3,-2)",
        "NUMBER(4,5)",
        "NUMBER(38,127)",
        "NUMBER(1,-84)",
        "NUMBER(38)",
        "NUMBER(38,-84)",
        "NUMBER(1,127)",
    ] {
        let source = format!("CREATE OR REPLACE FUNCTION MPEDB_NUM_CONSTRAINT(p NUMBER) RETURN NUMBER IS n {ty}; BEGIN n:=p; RETURN n; EXCEPTION WHEN VALUE_ERROR THEN RETURN -6502; END;");
        c.execute(&source, &[]).unwrap();
        engine.define(&source, mpedb_proc::Lang::PlSql).unwrap();
        for input in inputs {
            let bind = input.map(str::to_string);
            let native: Option<String> = c.query_row_as("SELECT TO_CHAR(MPEDB_NUM_CONSTRAINT(CAST(:1 AS NUMBER)),'TM9','NLS_NUMERIC_CHARACTERS=''.,''') FROM dual", &[&bind]).unwrap();
            let expected = native.map_or(Value::Null, |s| {
                Value::Numeric(mpedb_types::value::parse_numeric(&s).unwrap())
            });
            let input_value = input.map_or(Value::Null, |s| {
                Value::Numeric(mpedb_types::value::parse_numeric(s).unwrap())
            });
            let actual = engine.call("MPEDB_NUM_CONSTRAINT", &[input_value]).unwrap();
            assert_eq!(
                actual,
                mpedb_proc::ProcValue::Scalar(expected),
                "{ty}, {input:?}"
            );
        }
    }
    c.execute("DROP FUNCTION MPEDB_NUM_CONSTRAINT", &[])
        .unwrap();
    drop(engine);
    db.verify().unwrap();
    drop(db);
    std::fs::remove_file(path).unwrap();
}
