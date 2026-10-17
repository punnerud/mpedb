//! Native public OCI client boundary. Reads application dictionary metadata
//! and rows at one explicit SCN. No Oracle engine implementation is inspected.
//! Bounded capture, atomic writes and verified bootstrap positions are exposed
//! as low-level APIs. Full SourceAdapter and CLI import remain unfinished.
mod capture;
mod codec;
mod identity;
mod schema;
mod snapshot;
mod state;
pub use capture::OracleChangeBatch;
pub use snapshot::OracleSnapshot;
pub use state::OracleStateIdentity;
mod checked_write;
mod receipt;
mod receipt_codec;
mod write;
use codec::{decode_row, read_expr};
pub use receipt::{OracleLedgerIdentity, OracleWriteImages, OracleWriteRequest};

use crate::oracle::{quote, OracleColumn, OracleRoutine, OracleTable};
use crate::sourcecfg::SourceSpec;
use mpedb_types::{ColumnType, Error, Result, Value};
use oracle_client::{Connection, Row};
use std::time::Duration;

fn err(context: &str, e: oracle_client::Error) -> Error {
    Error::Config(format!("oracle {context}: {e}"))
}

pub struct OracleSource {
    connection: Connection,
    owner: String,
}

impl OracleSource {
    pub fn connect(spec: &SourceSpec) -> Result<Self> {
        let SourceSpec::Oracle {
            connect_string,
            username,
            password,
        } = spec
        else {
            return Err(Error::Config(
                "Oracle source requires kind=oracle credentials".into(),
            ));
        };
        let connection = Connection::connect(username, password, connect_string).map_err(|e| {
            let mut message = e.to_string();
            for secret in [password, connect_string] {
                if !secret.is_empty() {
                    message = message.replace(secret, "<redacted>");
                }
            }
            Error::Config(format!("oracle connect: {message}"))
        })?;
        connection
            .set_call_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| err("call timeout", e))?;
        // Dedicated adapter session only. Canonical temporal carriers must
        // not change with the client's local timezone or calendar settings.
        for sql in [
            "ALTER SESSION SET TIME_ZONE='+00:00'",
            "ALTER SESSION SET NLS_CALENDAR='GREGORIAN'",
            "ALTER SESSION SET NLS_NUMERIC_CHARACTERS='.,'",
        ] {
            connection
                .execute(sql, &[])
                .map_err(|e| err("canonical session", e))?;
        }
        let owner: String = connection
            .query_row_as(
                "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual",
                &[],
            )
            .map_err(|e| err("current schema", e))?;
        let maintained: String = connection
            .query_row_as(
                "SELECT oracle_maintained FROM all_users WHERE username=:1",
                &[&owner],
            )
            .map_err(|e| err("application schema", e))?;
        if maintained == "Y" {
            return Err(Error::Unsupported(
                "Oracle source requires an application-owned schema".into(),
            ));
        }
        Ok(Self { connection, owner })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn current_scn(&self) -> Result<u64> {
        self.connection
            .query_row_as(
                "SELECT DBMS_FLASHBACK.GET_SYSTEM_CHANGE_NUMBER FROM dual",
                &[],
            )
            .map_err(|e| err("SCN", e))
    }

    /// Preserve application routine definitions. Native compilation is a
    /// separate explicit action; unsupported packages are not dropped or
    /// mistaken for executable native routines. Oracle-maintained schemas are
    /// rejected at connect, so this never inspects vendor implementation code.
    pub fn routines(&self) -> Result<Vec<OracleRoutine>> {
        let rows=self.connection.query("SELECT name,type,text FROM all_source WHERE owner=:1 AND type IN ('FUNCTION','PROCEDURE','PACKAGE','PACKAGE BODY','TRIGGER','TYPE','TYPE BODY') ORDER BY name,type,line", &[&self.owner]).map_err(|e|err("application routine metadata",e))?;
        let mut result: Vec<OracleRoutine> = Vec::new();
        for row in rows {
            let row = row.map_err(|e| err("application routine source", e))?;
            let name: String = get(&row, 0)?;
            let kind: String = get(&row, 1)?;
            let text: String = get(&row, 2)?;
            if let Some(last) = result
                .last_mut()
                .filter(|r| r.name == name && r.kind == kind)
            {
                last.source.push_str(&text);
            } else {
                result.push(OracleRoutine {
                    owner: self.owner.clone(),
                    name,
                    kind,
                    source: text,
                });
            }
        }
        Ok(result)
    }

    pub fn introspect(
        &self,
        include: Option<&[String]>,
        exclude: &[String],
    ) -> Result<Vec<OracleTable>> {
        let rows = self.connection.query(
            "SELECT t.table_name,o.object_id,t.temporary,t.nested, CASE WHEN e.table_name IS NULL THEN 0 ELSE 1 END FROM all_tables t JOIN all_objects o ON o.owner=t.owner AND o.object_name=t.table_name AND o.object_type='TABLE' AND o.subobject_name IS NULL LEFT JOIN all_external_tables e ON e.owner=t.owner AND e.table_name=t.table_name WHERE t.owner=:1 ORDER BY t.table_name", &[&self.owner]
        ).map_err(|e|err("table metadata",e))?;
        let mut tables = Vec::new();
        for row in rows {
            let row = row.map_err(|e| err("table metadata row", e))?;
            let name: String = get(&row, 0)?;
            if include.is_some_and(|scope| !scope.contains(&name)) || exclude.contains(&name) {
                continue;
            }
            let columns = self.columns(&name)?;
            let pk_rows=self.connection.query("SELECT cc.column_name,c.status,c.validated FROM all_constraints c JOIN all_cons_columns cc ON cc.owner=c.owner AND cc.constraint_name=c.constraint_name AND cc.table_name=c.table_name WHERE c.owner=:1 AND c.table_name=:2 AND c.constraint_type='P' ORDER BY cc.position", &[&self.owner,&name]).map_err(|e|err("primary key",e))?;
            let mut pk = Vec::new();
            let mut pk_validated = true;
            for row in pk_rows {
                let row = row.map_err(|e| err("primary key row", e))?;
                let col: String = get(&row, 0)?;
                let index = columns.iter().position(|c| c.name == col).ok_or_else(|| {
                    Error::Config("Oracle PK references an unavailable column".into())
                })?;
                pk.push(index);
                pk_validated &=
                    get::<String>(&row, 1)? == "ENABLED" && get::<String>(&row, 2)? == "VALIDATED";
            }
            let ddl_source = self.table_ddl(&name, false)?;
            let schema_ddl = self.table_ddl(&name, true)?;
            tables.push(OracleTable {
                owner: self.owner.clone(),
                name,
                object_id: get(&row, 1)?,
                ddl_source,
                schema_ddl,
                columns,
                pk_validated: pk_validated && !pk.is_empty(),
                pk,
                temporary: get::<String>(&row, 2)? == "Y",
                nested: get::<String>(&row, 3)? == "YES",
                external: get::<i64>(&row, 4)? != 0,
            });
        }
        Ok(tables)
    }

    fn columns(&self, table: &str) -> Result<Vec<OracleColumn>> {
        let rows=self.connection.query("SELECT column_name,data_type,data_type_owner,data_precision,data_scale,data_length,char_length,char_used,nullable,virtual_column,identity_column,data_default,collation FROM all_tab_cols WHERE owner=:1 AND table_name=:2 AND (hidden_column='NO' OR user_generated='YES') ORDER BY internal_column_id", &[&self.owner,&table]).map_err(|e|err("column metadata",e))?;
        rows.map(|row| {
            let row = row.map_err(|e| err("column metadata row", e))?;
            Ok(OracleColumn {
                name: get(&row, 0)?,
                data_type: get(&row, 1)?,
                type_owner: get(&row, 2)?,
                precision: get(&row, 3)?,
                scale: get(&row, 4)?,
                data_length: get(&row, 5)?,
                char_length: get(&row, 6)?,
                char_used: get(&row, 7)?,
                not_null: get::<String>(&row, 8)? == "N",
                generated: get::<String>(&row, 9)? == "YES",
                identity: get::<String>(&row, 10)? == "YES",
                default_source: get(&row, 11)?,
                collation: get(&row, 12)?,
            })
        })
        .collect()
    }

    /// One table at a caller-selected SCN. Use the SAME SCN for every table in
    /// a snapshot. Flashback/DDL/history errors propagate; never silently fall
    /// back to current rows. Temporal text retains nanoseconds and time zones.
    pub fn read_snapshot(&self, table: &OracleTable, scn: u64) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        self.stream_snapshot(table, scn, |row| {
            rows.push(row);
            Ok(())
        })?;
        Ok(rows)
    }
    /// Stream one fixed-SCN table; individual carrier values remain owned.
    /// Callback failure aborts the read without substituting a newer SCN.
    pub fn stream_snapshot(
        &self,
        table: &OracleTable,
        scn: u64,
        mut sink: impl FnMut(Vec<Value>) -> Result<()>,
    ) -> Result<u64> {
        if table.owner != self.owner
            || table.temporary
            || table.nested
            || table.external
            || !table.pk_validated
            || table.pk.is_empty()
            || table.pk.iter().any(|&i| i >= table.columns.len())
        {
            return Err(Error::Unsupported(
                "Oracle snapshot requires an owned persistent table with a validated primary key"
                    .into(),
            ));
        }
        let mut exprs = Vec::new();
        for c in &table.columns {
            exprs.extend(read_expr(c)?);
        }
        let order = table
            .pk
            .iter()
            .map(|&i| quote(&table.columns[i].name))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {} FROM {}.{} AS OF SCN :1 ORDER BY {}",
            exprs.join(","),
            quote(&table.owner),
            quote(&table.name),
            order
        );
        let mut statement = self
            .connection
            .statement(&sql)
            .lob_locator()
            .build()
            .map_err(|e| err("snapshot prepare", e))?;
        let rows = statement.query(&[&scn]).map_err(|e| err("snapshot", e))?;
        let mut count = 0;
        for row in rows {
            let row = row.map_err(|e| err("snapshot row", e))?;
            sink(decode_row(&row, &table.columns, 0)?)?;
            count += 1;
        }
        Ok(count)
    }
}

fn get<T: oracle_client::sql_type::FromSql>(row: &Row, i: usize) -> Result<T> {
    row.get(i).map_err(|e| err("decode", e))
}
