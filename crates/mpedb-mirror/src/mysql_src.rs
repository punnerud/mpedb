//! MySQL-protocol introspection and value mapping.
//!
//! The adapter transports rows. It does not translate a MySQL schema into
//! PostgreSQL, Oracle, or SQLite. Signed integers and text are the types this
//! slice can carry without inventing a rounding rule; anything else is a
//! named refusal at introspect time.

use mpedb_types::{ColumnType, Error, Result, Value};
use mysql::prelude::Queryable;

use crate::sourcecfg::SourceSpec;
use crate::sqlite::{SourceColumn, SourceTable};
use crate::state::MapPolicy;

pub(crate) fn connect(spec: &SourceSpec) -> Result<mysql::Conn> {
    let SourceSpec::Mysql { socket, user, password, database } = spec else {
        return Err(Error::Config("mysql connect requires a mysql source".into()));
    };
    let mut opts = mysql::OptsBuilder::new()
        .socket(Some(socket.clone()))
        .user(Some(user.clone()))
        .db_name(Some(database.clone()));
    if !password.is_empty() {
        opts = opts.pass(Some(password.clone()));
    }
    mysql::Conn::new(opts).map_err(|e| {
        Error::Config(format!("mysql connect: {}", redact(e.to_string(), password)))
    })
}

fn redact(err: String, password: &str) -> String {
    if password.is_empty() {
        err
    } else {
        err.replace(password, "<redacted>")
    }
}

/// Backtick-quote an identifier. Internal backticks are doubled.
pub(crate) fn q(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// Mirrorable tables in Rust name order, which is the order [`Schema::new`]
/// assigns table ids.
pub(crate) fn introspect(
    conn: &mut mysql::Conn,
    include: Option<&[String]>,
    exclude: &[String],
) -> Result<Vec<SourceTable>> {
    let names: Vec<String> = conn
        .exec(
            "SELECT TABLE_NAME FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'",
            (),
        )
        .map_err(|e| Error::Config(format!("mysql introspect: {e}")))?;
    let mut tables = Vec::new();
    for name in names {
        if name.starts_with("_mpedb_") {
            continue;
        }
        if exclude.iter().any(|n| n == &name) {
            continue;
        }
        if let Some(inc) = include {
            if !inc.iter().any(|n| n == &name) {
                continue;
            }
        }
        tables.push(introspect_table(conn, &name)?);
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    if tables.is_empty() {
        return Err(Error::Config("no mirrorable tables in mysql source".into()));
    }
    Ok(tables)
}

fn introspect_table(conn: &mut mysql::Conn, name: &str) -> Result<SourceTable> {
    let raw: Vec<(String, String, String, String, String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, \
                    IFNULL(EXTRA, ''), IFNULL(COLUMN_KEY, '') \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            (name,),
        )
        .map_err(|e| Error::Config(format!("mysql columns `{name}`: {e}")))?;
    if raw.is_empty() {
        return Err(Error::Config(format!("mysql table `{name}` has no columns")));
    }
    let pk_names: Vec<String> = conn
        .exec(
            "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? \
               AND CONSTRAINT_NAME = 'PRIMARY' \
             ORDER BY ORDINAL_POSITION",
            (name,),
        )
        .map_err(|e| Error::Config(format!("mysql primary key `{name}`: {e}")))?;
    let mut columns = Vec::with_capacity(raw.len());
    for (cname, data_type, column_type, nullable, extra, key) in raw {
        if extra.to_ascii_lowercase().contains("generated") {
            return Err(Error::Unsupported(format!(
                "mysql `{name}.{cname}` is generated; this mirror does not write expressions back"
            )));
        }
        if column_type.contains('`') || column_type.contains(';') {
            return Err(Error::Unsupported(format!(
                "mysql `{name}.{cname}` has an unusable declared type"
            )));
        }
        let mapped = map_type(&data_type, &column_type).map_err(|e| match e {
            Error::Unsupported(msg) => Error::Unsupported(format!("mysql `{name}.{cname}`: {msg}")),
            other => other,
        })?;
        columns.push(SourceColumn {
            name: cname,
            declared_type: column_type,
            not_null: nullable.eq_ignore_ascii_case("NO"),
            generated: false,
            mapped,
            unique: key == "UNI",
        });
    }
    let pk = pk_names
        .iter()
        .map(|pk_name| {
            columns
                .iter()
                .position(|c| c.name == *pk_name)
                .ok_or_else(|| Error::Config(format!("mysql `{name}` primary key `{pk_name}` is not a column")))
        })
        .collect::<Result<Vec<_>>>()?;
    if pk.is_empty() {
        return Err(Error::Unsupported(format!(
            "mysql table `{name}` has no primary key"
        )));
    }
    Ok(SourceTable {
        name: name.to_string(),
        without_rowid: false,
        strict: false,
        columns,
        pk,
    })
}

fn map_type(data_type: &str, column_type: &str) -> Result<ColumnType> {
    if column_type.to_ascii_lowercase().contains("unsigned") {
        return Err(Error::Unsupported(format!(
            "unsigned type `{column_type}` is not mirrored"
        )));
    }
    match data_type.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => Ok(ColumnType::Int64),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => Ok(ColumnType::Text),
        other => Err(Error::Unsupported(format!(
            "type `{other}` is not mirrored (signed integers and text only)"
        ))),
    }
}

pub(crate) fn map_policy(data_type: &str) -> MapPolicy {
    match data_type.to_ascii_lowercase().as_str() {
        "bigint" | "tinytext" | "text" | "mediumtext" | "longtext" => MapPolicy::Exact,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "char" | "varchar" => {
            MapPolicy::Widened
        }
        _ => MapPolicy::Exact,
    }
}

/// `COLUMN_TYPE` is `varchar(64)` / `int(11)`. The policy key is the base word.
pub(crate) fn data_type_of(column_type: &str) -> &str {
    column_type.split(['(', ' ']).next().unwrap_or(column_type)
}

pub(crate) fn from_mysql(v: mysql::Value, ct: ColumnType, table: &str, col: &str) -> Result<Value> {
    let mismatch = |got: &str| {
        Error::TypeMismatch(format!(
            "mysql `{table}.{col}`: {got} does not match {ct:?}"
        ))
    };
    match v {
        mysql::Value::NULL => Ok(Value::Null),
        mysql::Value::Int(i) => match ct {
            ColumnType::Int64 => Ok(Value::Int(i)),
            _ => Err(mismatch("integer")),
        },
        mysql::Value::UInt(u) => {
            let i = i64::try_from(u).map_err(|_| mismatch("unsigned integer"))?;
            match ct {
                ColumnType::Int64 => Ok(Value::Int(i)),
                _ => Err(mismatch("integer")),
            }
        }
        mysql::Value::Bytes(b) => match ct {
            ColumnType::Text => {
                let s = String::from_utf8(b).map_err(|_| mismatch("non-utf8 bytes"))?;
                Ok(Value::Text(s))
            }
            ColumnType::Int64 => {
                let s = String::from_utf8(b).map_err(|_| mismatch("non-utf8 integer"))?;
                let i = s.parse::<i64>().map_err(|_| mismatch("text integer"))?;
                Ok(Value::Int(i))
            }
            _ => Err(mismatch("bytes")),
        },
        mysql::Value::Float(_) => Err(mismatch("float")),
        mysql::Value::Double(_) => Err(mismatch("double")),
        mysql::Value::Date(..) => Err(mismatch("date")),
        mysql::Value::Time(..) => Err(mismatch("time")),
    }
}

pub(crate) fn to_mysql(v: &Value) -> Result<mysql::Value> {
    match v {
        Value::Null => Ok(mysql::Value::NULL),
        Value::Int(i) => Ok(mysql::Value::Int(*i)),
        Value::Text(s) => Ok(mysql::Value::Bytes(s.as_bytes().to_vec())),
        Value::Float(_) | Value::Bool(_) | Value::Blob(_) | Value::List(_) | Value::Timestamp(_)
        | Value::Date(_) | Value::Time(_) | Value::Numeric(_) => Err(Error::TypeMismatch(
            "mysql mirror push carries null, integers, and text".into(),
        )),
    }
}

pub(crate) fn row_value(row: &mysql::Row, idx: usize) -> Result<mysql::Value> {
    row.get(idx)
        .ok_or_else(|| Error::Corrupt(format!("mysql result is missing column {idx}")))
}

/// Constraint and data errors are this row's verdict. Connection and server
/// faults propagate so a push does not pretend the row was parked.
pub(crate) fn is_row_rejection(err: &mysql::Error) -> bool {
    let mysql::Error::MySqlError(server) = err else {
        return false;
    };
    matches!(
        server.code,
        1022 | 1048 | 1062 | 1264 | 1265 | 1292 | 1364 | 1366 | 1406 | 1452 | 3819 | 4025
    )
}

pub(crate) fn missing_table(err: &mysql::Error) -> bool {
    matches!(err, mysql::Error::MySqlError(server) if server.code == 1146)
}
