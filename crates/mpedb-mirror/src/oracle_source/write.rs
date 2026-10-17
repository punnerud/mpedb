//! Verified atomic source writes from MPEdb carrier values. This is transport;
//! transformations belong to MPEdb/rRETL. Capture, echo suppression, authority
//! fencing and conflict-aware push still require the higher-level adapter.
use super::*;
use crate::adapter::{NetOp, NetOpKind};
use oracle_client::sql_type::ToSql;

enum Bind {
    Text(String),
    Float(f64),
    Int(i64),
    Bytes(Vec<u8>),
    Null(Option<String>),
}
impl Bind {
    fn sql(&self) -> &dyn ToSql {
        match self {
            Self::Text(v) => v,
            Self::Float(v) => v,
            Self::Int(v) => v,
            Self::Bytes(v) => v,
            Self::Null(v) => v,
        }
    }
}

impl OracleSource {
    /// Apply a batch in one Oracle transaction and return its actual postimages.
    /// Table ids are indices into `tables` (the carrier schema's table order).
    /// Any error, including source rounding/truncation or trigger substitution,
    /// rolls the WHOLE batch back. Generated columns are read back rather than
    /// written. This low-level API does not advance a synchronization cursor.
    pub fn write_batch(
        &self,
        tables: &[OracleTable],
        ops: &[NetOp],
    ) -> Result<Vec<Option<Vec<Value>>>> {
        let result = (|| {
            // Fence source DDL for the entire batch. A recreated/altered table
            // requires a new carrier import; never use stale type metadata.
            let used = ops
                .iter()
                .map(|op| op.table_id)
                .collect::<std::collections::BTreeSet<_>>();
            for id in used {
                let table = tables.get(id as usize).ok_or_else(|| {
                    Error::Config("Oracle write references an unknown carrier table".into())
                })?;
                if table.owner != self.owner {
                    return Err(Error::Unsupported(
                        "Oracle write requires the connected application owner".into(),
                    ));
                }
                let target = format!("{}.{}", quote(&table.owner), quote(&table.name));
                self.connection
                    .execute(
                        &format!("LOCK TABLE {target} IN ROW EXCLUSIVE MODE NOWAIT"),
                        &[],
                    )
                    .map_err(|e| err("write schema fence", e))?;
                let object_id: u64 = self.connection.query_row_as("SELECT object_id FROM all_objects WHERE owner=:1 AND object_name=:2 AND object_type='TABLE' AND subobject_name IS NULL", &[&table.owner, &table.name]).map_err(|e| err("write object identity", e))?;
                let ddl: String = self
                    .connection
                    .query_row_as(
                        "SELECT DBMS_METADATA.GET_DDL('TABLE',:1,:2) FROM dual",
                        &[&table.name, &table.owner],
                    )
                    .map_err(|e| err("write schema verification", e))?;
                if object_id != table.object_id || ddl != table.ddl_source {
                    return Err(Error::Unsupported(
                        "Oracle source schema changed; reimport before writing".into(),
                    ));
                }
            }
            let mut images = Vec::with_capacity(ops.len());
            for op in ops {
                let table = tables.get(op.table_id as usize).ok_or_else(|| {
                    Error::Config("Oracle write references an unknown carrier table".into())
                })?;
                if table.owner != self.owner {
                    return Err(Error::Unsupported(
                        "Oracle write requires the connected application owner".into(),
                    ));
                }
                crate::oracle::build_schema(std::slice::from_ref(table))?;
                if table.columns.iter().any(|c| c.identity) {
                    return Err(Error::Unsupported(
                        "Oracle identity write policy is not implemented".into(),
                    ));
                }
                if op.pk.len() != table.pk.len() || op.pk.contains(&Value::Null) {
                    return Err(Error::Config(
                        "Oracle write has an invalid row identity".into(),
                    ));
                }
                let target = format!("{}.{}", quote(&table.owner), quote(&table.name));
                let keys = table
                    .pk
                    .iter()
                    .enumerate()
                    .map(|(i, &k)| {
                        Ok(format!(
                            "{}={}",
                            quote(&table.columns[k].name),
                            expression(&table.columns[k], i + 1)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join(" AND ");
                match &op.kind {
                    NetOpKind::Delete => {
                        let owned = table
                            .pk
                            .iter()
                            .zip(&op.pk)
                            .map(|(&k, v)| bind(&table.columns[k], v))
                            .collect::<Result<Vec<_>>>()?;
                        let refs = owned.iter().map(Bind::sql).collect::<Vec<_>>();
                        self.connection
                            .execute(&format!("DELETE FROM {target} WHERE {keys}"), &refs)
                            .map_err(|e| err("delete", e))?;
                        let image = self.read_current_key(table, &op.pk)?;
                        if image.is_some() {
                            return Err(Error::Unsupported(
                                "Oracle delete did not remove the requested identity".into(),
                            ));
                        }
                        images.push(None);
                    }
                    NetOpKind::Upsert(values) => {
                        if values.len() != table.columns.len()
                            || table.pk.iter().zip(&op.pk).any(|(&k, v)| &values[k] != v)
                        {
                            return Err(Error::Config(
                                "Oracle write image does not match its identity or schema".into(),
                            ));
                        }
                        let writable = table
                            .columns
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| !c.generated)
                            .collect::<Vec<_>>();
                        let owned = writable
                            .iter()
                            .map(|(i, c)| bind(c, &values[*i]))
                            .collect::<Result<Vec<_>>>()?;
                        let projection = writable
                            .iter()
                            .enumerate()
                            .map(|(n, (_, c))| {
                                Ok(format!("{} AS {}", expression(c, n + 1)?, quote(&c.name)))
                            })
                            .collect::<Result<Vec<_>>>()?
                            .join(",");
                        let on = table
                            .pk
                            .iter()
                            .map(|&k| {
                                let q = quote(&table.columns[k].name);
                                format!("d.{q}=s.{q}")
                            })
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        let updates = writable
                            .iter()
                            .filter(|(i, _)| !table.pk.contains(i))
                            .map(|(_, c)| {
                                let q = quote(&c.name);
                                format!("d.{q}=s.{q}")
                            })
                            .collect::<Vec<_>>();
                        let update = if updates.is_empty() {
                            String::new()
                        } else {
                            format!(" WHEN MATCHED THEN UPDATE SET {}", updates.join(","))
                        };
                        let names = writable
                            .iter()
                            .map(|(_, c)| quote(&c.name))
                            .collect::<Vec<_>>();
                        let source = names
                            .iter()
                            .map(|n| format!("s.{n}"))
                            .collect::<Vec<_>>()
                            .join(",");
                        let sql=format!("MERGE INTO {target} d USING (SELECT {projection} FROM dual) s ON ({on}){update} WHEN NOT MATCHED THEN INSERT ({}) VALUES ({source})",names.join(","));
                        let refs = owned.iter().map(Bind::sql).collect::<Vec<_>>();
                        self.connection
                            .execute(&sql, &refs)
                            .map_err(|e| err("upsert", e))?;
                        let image = self.read_current_key(table, &op.pk)?.ok_or_else(|| {
                            Error::Unsupported(
                                "Oracle upsert did not retain the requested identity".into(),
                            )
                        })?;
                        if writable.iter().any(|(i, _)| image[*i] != values[*i]) {
                            return Err(Error::Unsupported("Oracle changed a submitted carrier value; the entire batch was rolled back".into()));
                        }
                        images.push(Some(image));
                    }
                }
            }
            self.connection
                .commit()
                .map_err(|e| err("write commit", e))?;
            Ok(images)
        })();
        if result.is_err() {
            // Rollback failure is material: the caller cannot assume source
            // state and must reconnect/reconcile before retrying.
            self.connection
                .rollback()
                .map_err(|e| err("write rollback", e))?;
        }
        result
    }

    fn read_current_key(&self, table: &OracleTable, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        let owned = table
            .pk
            .iter()
            .zip(pk)
            .map(|(&k, v)| bind(&table.columns[k], v))
            .collect::<Result<Vec<_>>>()?;
        let refs = owned.iter().map(Bind::sql).collect::<Vec<_>>();
        let keys = table
            .pk
            .iter()
            .enumerate()
            .map(|(i, &k)| {
                Ok(format!(
                    "{}={}",
                    quote(&table.columns[k].name),
                    expression(&table.columns[k], i + 1)?
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .join(" AND ");
        let projection = table
            .columns
            .iter()
            .map(read_expr)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {projection} FROM {}.{} WHERE {keys}",
            quote(&table.owner),
            quote(&table.name)
        );
        let mut rows = self
            .connection
            .query(&sql, &refs)
            .map_err(|e| err("write verification", e))?;
        let image = rows
            .next()
            .transpose()
            .map_err(|e| err("write verification row", e))?
            .map(|r| decode_row(&r, &table.columns, 0))
            .transpose()?;
        if rows.next().is_some() {
            return Err(Error::Corrupt(
                "Oracle validated identity returned multiple rows".into(),
            ));
        }
        Ok(image)
    }
}

fn bind(c: &OracleColumn, value: &Value) -> Result<Bind> {
    if matches!(value, Value::Null) {
        return Ok(Bind::Null(None));
    }
    match (c.mapped(), value) {
        (Some(ColumnType::Numeric), Value::Numeric(v))
            if mpedb_types::value::parse_numeric(v).is_some() && !v.contains("Infinity") =>
        {
            Ok(Bind::Text(v.clone()))
        }
        (Some(ColumnType::Float64), Value::Float(v)) => Ok(Bind::Float(*v)),
        (Some(ColumnType::Bool), Value::Bool(v)) => Ok(Bind::Int(i64::from(*v))),
        (Some(ColumnType::Text), Value::Text(v)) if v.len() <= 4000 => Ok(Bind::Text(v.clone())),
        (Some(ColumnType::Blob), Value::Blob(v)) if v.len() <= 2000 => Ok(Bind::Bytes(v.clone())),
        _ => Err(Error::Unsupported(
            "Oracle write carrier type, special NUMBER or large LOB is not supported".into(),
        )),
    }
}
fn expression(c: &OracleColumn, n: usize) -> Result<String> {
    let b = format!(":{n}");
    Ok(match c.data_type.as_str() {
        // The dedicated session pins NLS_NUMERIC_CHARACTERS. An explicit
        // NULL format is NOT equivalent: Oracle returns NULL for that form.
        "NUMBER" | "FLOAT" => format!("TO_NUMBER({b})"),
        "BOOLEAN" => format!("CASE {b} WHEN 1 THEN TRUE WHEN 0 THEN FALSE ELSE NULL END"),
        "DATE" => format!("TO_DATE({b},'SYYYY-MM-DD\"T\"HH24:MI:SS','NLS_DATE_LANGUAGE=English')"),
        "CLOB" => format!("TO_CLOB({b})"),
        "NCLOB" => format!("TO_NCLOB({b})"),
        "BLOB" => format!("TO_BLOB({b})"),
        t if t.starts_with("TIMESTAMP") && !t.contains("TIME ZONE") => format!(
            "TO_TIMESTAMP({b},'SYYYY-MM-DD\"T\"HH24:MI:SS.FF9','NLS_DATE_LANGUAGE=English')"
        ),
        t if t.starts_with("TIMESTAMP")
            || t.starts_with("INTERVAL")
            || t == "LONG"
            || t == "LONG RAW" =>
        {
            return Err(Error::Unsupported(
                "Oracle zoned timestamp, interval and LONG writes are not implemented".into(),
            ))
        }
        _ => b,
    })
}
