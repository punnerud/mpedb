//! Canonical Oracle carriers shared by snapshot and verified write-back.
use super::*;

pub(super) fn decode_row(
    row: &Row,
    columns: &[OracleColumn],
    mut offset: usize,
) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(columns.len());
    for c in columns {
        let mapped = c.mapped().ok_or_else(|| {
            Error::Unsupported(format!("Oracle type {} is not mapped", c.declared_type()))
        })?;
        let value = match mapped {
            ColumnType::Numeric => {
                let text: Option<String> = get(row, offset)?;
                let image: Option<String> = get(row, offset + 1)?;
                offset += 1;
                match image.as_deref() {
                    Some("Typ=2 Len=2: ff,65") => Value::Numeric("Infinity".into()),
                    Some("Typ=2 Len=1: 0") => Value::Numeric("-Infinity".into()),
                    _ => match text {
                        None => Value::Null,
                        Some(text) => {
                            Value::Numeric(mpedb_types::value::parse_numeric(&text).ok_or_else(
                                || Error::Config("Oracle returned an invalid NUMBER image".into()),
                            )?)
                        }
                    },
                }
            }
            ColumnType::Float64 => {
                get::<Option<f64>>(row, offset)?.map_or(Value::Null, Value::Float)
            }
            ColumnType::Bool => {
                get::<Option<i64>>(row, offset)?.map_or(Value::Null, |b| Value::Bool(b != 0))
            }
            ColumnType::Blob => {
                get::<Option<Vec<u8>>>(row, offset)?.map_or(Value::Null, Value::Blob)
            }
            ColumnType::Text => {
                get::<Option<String>>(row, offset)?.map_or(Value::Null, Value::Text)
            }
            _ => {
                return Err(Error::Unsupported(
                    "Oracle carrier is not implemented".into(),
                ))
            }
        };
        offset += 1;
        values.push(value);
    }
    Ok(values)
}

pub(super) fn read_expr(c: &OracleColumn) -> Result<Vec<String>> {
    let name = quote(&c.name);
    if c.mapped().is_none() {
        return Err(Error::Unsupported(format!(
            "Oracle type {} is not mapped",
            c.declared_type()
        )));
    }
    Ok(match c.data_type.as_str() {
        "NUMBER" | "FLOAT" => vec![
            format!("TO_CHAR({name},'TM9','NLS_NUMERIC_CHARACTERS=''.,''')"),
            format!("DUMP({name},16)"),
        ],
        "BOOLEAN" => vec![format!(
            "CASE WHEN {name} THEN 1 WHEN NOT {name} THEN 0 ELSE NULL END"
        )],
        "DATE" => vec![format!(
            "TO_CHAR({name},'SYYYY-MM-DD\"T\"HH24:MI:SS','NLS_DATE_LANGUAGE=English')"
        )],
        t if t.starts_with("TIMESTAMP") => vec![format!(
            "TO_CHAR({name},'SYYYY-MM-DD\"T\"HH24:MI:SS.FF9{}','NLS_DATE_LANGUAGE=English')",
            if t.contains("TIME ZONE") {
                " TZH:TZM TZR TZD"
            } else {
                ""
            }
        )],
        t if t.starts_with("INTERVAL") => vec![format!("TO_CHAR({name})")],
        _ => vec![name],
    })
}
