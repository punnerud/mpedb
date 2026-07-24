//! One rendering of a result cell, per binding.
//!
//! Every engine's answer is reduced to the same text before comparison. This
//! is the only place a per-engine difference is allowed, and it is deliberately
//! about FORMATTING only — never about the query, never about which rows come
//! back. Floats go to two decimals because the generator produces two-decimal
//! amounts, and comparing sums at full precision would compare summation ORDER
//! (a column store adds in a different order than a row store, and both are
//! right) rather than results.

pub fn render_duck(row: &duckdb::Row<'_>, i: usize) -> String {
    use duckdb::types::ValueRef;
    match row.get_ref(i) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Boolean(b)) => b.to_string(),
        Ok(ValueRef::TinyInt(v)) => v.to_string(),
        Ok(ValueRef::SmallInt(v)) => v.to_string(),
        Ok(ValueRef::Int(v)) => v.to_string(),
        Ok(ValueRef::BigInt(v)) => v.to_string(),
        Ok(ValueRef::HugeInt(v)) => v.to_string(),
        Ok(ValueRef::UTinyInt(v)) => v.to_string(),
        Ok(ValueRef::USmallInt(v)) => v.to_string(),
        Ok(ValueRef::UInt(v)) => v.to_string(),
        Ok(ValueRef::UBigInt(v)) => v.to_string(),
        Ok(ValueRef::Float(v)) => format!("{v:.2}"),
        Ok(ValueRef::Double(v)) => format!("{v:.2}"),
        Ok(ValueRef::Decimal(d)) => format!("{d:.2}"),
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("<err {e}>"),
    }
}

/// PostgreSQL rows come back statically typed; switch on the column's SQL type
/// and pull the matching Rust type. Every value is `Option<_>` so a NULL renders
/// as `NULL` rather than panicking. Floats to two decimals, as everywhere else.
pub fn render_pg(row: &postgres::Row, i: usize) -> String {
    let ty = row.columns()[i].type_().name().to_string();
    match ty.as_str() {
        "int2" => row.get::<_, Option<i16>>(i).map_or_else(|| "NULL".into(), |v| v.to_string()),
        "int4" => row.get::<_, Option<i32>>(i).map_or_else(|| "NULL".into(), |v| v.to_string()),
        "int8" => row.get::<_, Option<i64>>(i).map_or_else(|| "NULL".into(), |v| v.to_string()),
        "float4" => row
            .get::<_, Option<f32>>(i)
            .map_or_else(|| "NULL".into(), |v| format!("{v:.2}")),
        "float8" => row
            .get::<_, Option<f64>>(i)
            .map_or_else(|| "NULL".into(), |v| format!("{v:.2}")),
        "text" | "varchar" | "bpchar" | "name" => {
            row.get::<_, Option<String>>(i).unwrap_or_else(|| "NULL".into())
        }
        other => format!("<{other}>"),
    }
}

/// MariaDB values. The text protocol (`query`) hands EVERYTHING back as `Bytes`
/// — including floats, at full precision (`1000139779.560016`) — so the value's
/// own variant is not enough; the COLUMN type decides. A FLOAT/DOUBLE column is
/// parsed and re-formatted to two decimals to match the other engines (which is
/// also why `min(amount)=0` must render `0.00`, not the raw `0`). Integer and
/// DECIMAL columns (the latter is what `sum()` over an INT returns, scale 0) are
/// already integer-shaped, so they — and the text columns — render verbatim.
pub fn render_mysql(row: &mysql::Row, i: usize) -> String {
    use mysql::consts::ColumnType::{MYSQL_TYPE_DOUBLE, MYSQL_TYPE_FLOAT};
    use mysql::Value;
    let is_float = matches!(
        row.columns_ref()[i].column_type(),
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE
    );
    match row.as_ref(i) {
        None | Some(Value::NULL) => "NULL".into(),
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::UInt(n)) => n.to_string(),
        Some(Value::Float(f)) => format!("{f:.2}"),
        Some(Value::Double(f)) => format!("{f:.2}"),
        Some(Value::Bytes(b)) => {
            let s = String::from_utf8_lossy(b).into_owned();
            if is_float {
                s.parse::<f64>().map(|f| format!("{f:.2}")).unwrap_or(s)
            } else {
                s
            }
        }
        Some(other) => format!("{other:?}"),
    }
}

pub fn render_sqlite(row: &rusqlite::Row<'_>, i: usize) -> String {
    use rusqlite::types::ValueRef;
    match row.get_ref(i) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Integer(v)) => v.to_string(),
        Ok(ValueRef::Real(v)) => format!("{v:.2}"),
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Blob(b)) => format!("blob:{}", b.len()),
        Err(e) => format!("<err {e}>"),
    }
}
