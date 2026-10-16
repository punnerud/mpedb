use super::*;

#[test]
fn roundtrip_all_variants() {
    let values = vec![
        Value::Null,
        Value::Int(-42),
        Value::Int(i64::MIN),
        Value::Float(3.75),
        Value::Float(f64::NEG_INFINITY),
        Value::Bool(true),
        Value::Text("hløl \0 zero".into()),
        Value::Blob(vec![0, 255, 0, 1]),
        Value::Timestamp(1_720_000_000_000_000),
    ];
    let mut buf = Vec::new();
    for v in &values {
        write_value(&mut buf, v);
    }
    let mut pos = 0;
    for v in &values {
        assert_eq!(&read_value(&buf, &mut pos).unwrap(), v);
    }
    assert_eq!(pos, buf.len());
}

#[test]
fn truncated_input_is_error_not_panic() {
    let mut buf = Vec::new();
    write_value(&mut buf, &Value::Text("hello".into()));
    for cut in 0..buf.len() {
        assert!(read_value(&buf[..cut], &mut 0).is_err());
    }
}

/// The declared-name → column-type table, spelled out. Every row here is
/// cross-checked against the real `sqlite3` binary's affinity in
/// `crates/mpedb/tests/django_parse_gaps.rs`.
#[test]
fn declared_type_names_map_by_affinity() {
    for (decl, want) in [
        // mpedb's own vocabulary, where sqlite's affinity rule agrees.
        ("int64", ColumnType::Int64),
        ("INTEGER", ColumnType::Int64),
        ("real", ColumnType::Float64),
        ("text", ColumnType::Text),
        // The two carve-outs.
        ("bool", ColumnType::Bool),
        ("BOOLEAN", ColumnType::Bool),
        ("any", ColumnType::Any),
        // …and the names where sqlite's rule now wins (task #113): a
        // BLOB-affinity column converts nothing and a `timestamp` is
        // NUMERIC, so both are the per-value column.
        ("timestamp", ColumnType::Any),
        ("blob", ColumnType::Any),
        ("bytes", ColumnType::Any),
        ("string", ColumnType::Any),
        // INTEGER affinity (step 2).
        ("bigint", ColumnType::Int64),
        ("smallint", ColumnType::Int64),
        ("tinyint", ColumnType::Int64),
        ("int(8)", ColumnType::Int64),
        ("integer unsigned", ColumnType::Int64),
        ("unsigned big int", ColumnType::Int64),
        ("int2", ColumnType::Int64),
        // `INT` wins wherever it appears, even in the second word — this is
        // sqlite's rule, and `floating point` really is INTEGER there.
        ("floating point", ColumnType::Int64),
        // REAL affinity.
        ("double precision", ColumnType::Float64),
        ("float", ColumnType::Float64),
        // TEXT affinity.
        ("varchar(100)", ColumnType::Text),
        ("VARCHAR", ColumnType::Text),
        ("char(1)", ColumnType::Text),
        ("nchar(55)", ColumnType::Text),
        ("clob", ColumnType::Text),
        ("native character(70)", ColumnType::Text),
        // BLOB affinity — only a name literally containing `blob`; the
        // typeless column, which converts nothing.
        ("longblob", ColumnType::Any),
        ("cblob", ColumnType::Any),
        // NUMERIC affinity → the per-value column. (`varbinary` really is
        // NUMERIC in sqlite: the BLOB rule is a `blob` substring, nothing
        // else.)
        ("varbinary", ColumnType::Any),
        ("decimal(10,2)", ColumnType::Any),
        ("numeric", ColumnType::Any),
        ("date", ColumnType::Any),
        ("datetime", ColumnType::Any),
        ("nosuchtype", ColumnType::Any),
        // No declared type at all is sqlite's no-affinity column.
        ("", ColumnType::Any),
        ("   ", ColumnType::Any),
    ] {
        assert_eq!(ColumnType::from_declared(decl), want, "declared `{decl}`");
    }
}

/// The DDL path and the CONFIG path are deliberately different vocabularies
/// (task #113). `parse` is the rigid product; `from_declared` is sqlite.
/// They agree everywhere sqlite's affinity rule agrees, and the exact set
/// where they differ is pinned here so a future widening cannot soften a
/// config-declared column by accident.
#[test]
fn from_declared_differs_from_parse_only_on_the_named_sqlite_affinity_words() {
    for name in [
        "int64", "int", "integer", "float64", "float", "real", "double", "bool", "boolean",
        "text", "any",
    ] {
        assert_eq!(
            ColumnType::from_declared(name),
            ColumnType::parse(name).unwrap(),
            "`{name}` must mean the same thing in a config and in a CREATE TABLE"
        );
    }
    // sqlite's rule wins for these: BLOB affinity converts nothing and is
    // per-value; `timestamp`/`string`/`bytes` are NUMERIC affinity.
    for name in ["blob", "bytes", "timestamp", "string"] {
        assert_eq!(ColumnType::from_declared(name), ColumnType::Any, "`{name}`");
        assert_ne!(ColumnType::parse(name).unwrap(), ColumnType::Any, "`{name}`");
    }
}

#[test]
fn float_order_semantics() {
    assert_eq!(float_total_cmp(0.0, -0.0), Ordering::Equal);
    assert_eq!(float_total_cmp(f64::NAN, f64::NAN), Ordering::Equal);
    assert_eq!(float_total_cmp(f64::INFINITY, f64::NAN), Ordering::Less);
    assert_eq!(float_total_cmp(-1.0, 1.0), Ordering::Less);
    assert_eq!(
        float_total_cmp(f64::NEG_INFINITY, f64::MIN),
        Ordering::Less
    );
}
