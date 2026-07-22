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
