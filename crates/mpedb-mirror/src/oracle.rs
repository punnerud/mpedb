//! Oracle source schema and lossless carriers. This describes Oracle -> MPEdb;
//! it is not a direct Oracle -> PostgreSQL converter. Original declarations
//! remain available to rRETL and subsequent source write-back checks.
use crate::state::MapPolicy;
use mpedb_types::ColumnType;

/// A carrier schema for a source snapshot. Oracle constraints/default SQL are
/// retained in OracleTable metadata, not reinterpreted as PostgreSQL syntax.
pub fn build_schema(tables: &[OracleTable]) -> mpedb_types::Result<mpedb_types::Schema> {
    use mpedb_types::{Affinity, Collation, ColumnDef, Error, Schema, TableDef, TableKind};
    let mut output = Vec::new();
    for table in tables {
        if table.pk.is_empty()
            || table
                .pk
                .iter()
                .any(|&i| i >= table.columns.len() || u16::try_from(i).is_err())
            || table
                .pk
                .iter()
                .enumerate()
                .any(|(i, key)| table.pk[..i].contains(key))
        {
            return Err(Error::Unsupported(
                "Oracle carrier requires a valid, nonempty primary key".into(),
            ));
        }
        if !table.pk_validated || table.temporary || table.nested || table.external {
            return Err(Error::Unsupported(
                "Oracle carrier schema requires persistent tables with validated primary keys"
                    .into(),
            ));
        }
        let columns = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let ty = c.mapped().ok_or_else(|| {
                    Error::Unsupported(format!("Oracle type {} is not mapped", c.declared_type()))
                })?;
                Ok(ColumnDef {
                    name: c.name.clone(),
                    ty,
                    nullable: !c.not_null && !table.pk.contains(&i),
                    unique: false,
                    indexed: false,
                    default: None,
                    check: None,
                    collation: Collation::Binary,
                    affinity: Affinity::implied_by(ty),
                    pg_enum: None,
                    generated: None,
                    default_text: None,
                    decl: None,
                })
            })
            .collect::<mpedb_types::Result<Vec<_>>>()?;
        output.push(TableDef {
            id: 0,
            name: table.name.clone(),
            columns,
            primary_key: table.pk.iter().map(|&i| i as u16).collect(),
            indexes: vec![],
            dead: false,
            pk_name: None,
            implicit_rowid: false,
            autoincrement: false,
            kind: TableKind::Standard,
            foreign_keys: vec![],
        });
    }
    Schema::new(output)
}

/// Resolve source metadata in the carrier schema's canonical table-id order.
/// Caller slice order is not an identity: Schema::new sorts table names.
pub(crate) fn carrier_tables(tables: &[OracleTable]) -> mpedb_types::Result<Vec<&OracleTable>> {
    let schema = build_schema(tables)?;
    let by_name = tables
        .iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<std::collections::HashMap<_, _>>();
    schema
        .tables
        .iter()
        .map(|table| {
            by_name.get(table.name.as_str()).copied().ok_or_else(|| {
                mpedb_types::Error::Corrupt("Oracle carrier table metadata is missing".into())
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleColumn {
    pub name: String,
    pub data_type: String,
    pub type_owner: Option<String>,
    pub precision: Option<i32>,
    pub scale: Option<i32>,
    pub data_length: u32,
    pub char_length: u32,
    pub char_used: Option<String>,
    pub not_null: bool,
    pub generated: bool,
    pub identity: bool,
    pub default_source: Option<String>,
    pub collation: Option<String>,
}

impl OracleColumn {
    /// Explicit BYTE/CHAR semantics and NUMBER precision/scale are source
    /// schema, not hints to discard after choosing an MPEdb carrier.
    pub fn declared_type(&self) -> String {
        match self.data_type.as_str() {
            "NUMBER" => match (self.precision, self.scale) {
                (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
                (Some(p), None) => format!("NUMBER({p})"),
                (None, Some(s)) => format!("NUMBER(*,{s})"),
                _ => "NUMBER".into(),
            },
            "FLOAT" => self
                .precision
                .map_or_else(|| "FLOAT".into(), |p| format!("FLOAT({p})")),
            "VARCHAR2" | "CHAR" => {
                let characters = self.char_used.as_deref() == Some("C");
                format!(
                    "{}({} {})",
                    self.data_type,
                    if characters {
                        self.char_length
                    } else {
                        self.data_length
                    },
                    if characters { "CHAR" } else { "BYTE" }
                )
            }
            "NVARCHAR2" | "NCHAR" => format!("{}({})", self.data_type, self.char_length),
            "RAW" => format!("RAW({})", self.data_length),
            _ => self.type_owner.as_ref().map_or_else(
                || self.data_type.clone(),
                |owner| format!("{}.{}", quote(owner), quote(&self.data_type)),
            ),
        }
    }

    pub fn mapped(&self) -> Option<ColumnType> {
        if self.type_owner.is_some() {
            return None;
        }
        map_type(&self.data_type)
    }

    pub fn policy(&self) -> Option<MapPolicy> {
        let mapped = self.mapped()?;
        Some(match self.data_type.as_str() {
            "BINARY_DOUBLE" | "BOOLEAN" | "BLOB" | "CLOB" | "NCLOB" | "LONG" | "LONG RAW" => {
                MapPolicy::Exact
            }
            _ if mapped == ColumnType::Text && temporal(&self.data_type) => MapPolicy::ViaText,
            _ => MapPolicy::Widened,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleTable {
    pub owner: String,
    pub name: String,
    pub object_id: u64,
    /// Original application table DDL from Oracle's public metadata API.
    /// Retained separately; never executed as PostgreSQL DDL.
    pub ddl_source: String,
    /// Logical DDL with physical segment attributes omitted by DBMS_METADATA.
    /// Used for schema fences and cursor scope, never reconstructed from SQL text.
    pub schema_ddl: String,
    pub columns: Vec<OracleColumn>,
    /// Column indices in the primary key's declared order.
    pub pk: Vec<usize>,
    pub pk_validated: bool,
    pub temporary: bool,
    pub nested: bool,
    pub external: bool,
}

/// Application-owned source exposed by ALL_SOURCE, not database-engine code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleRoutine {
    pub owner: String,
    pub name: String,
    pub kind: String,
    pub source: String,
}

impl OracleRoutine {
    pub fn create_source(&self) -> String {
        format!("CREATE OR REPLACE {}", self.source)
    }
}

pub fn map_type(data_type: &str) -> Option<ColumnType> {
    Some(match data_type {
        "NUMBER" | "FLOAT" => ColumnType::Numeric,
        "BINARY_DOUBLE" | "BINARY_FLOAT" => ColumnType::Float64,
        "BOOLEAN" => ColumnType::Bool,
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "CLOB" | "NCLOB" | "LONG" => ColumnType::Text,
        "RAW" | "BLOB" | "LONG RAW" => ColumnType::Blob,
        other if temporal(other) => ColumnType::Text,
        _ => return None,
    })
}

pub(crate) fn temporal(t: &str) -> bool {
    t == "DATE" || t.starts_with("TIMESTAMP") || t.starts_with("INTERVAL")
}

pub(crate) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn column(ty: &str) -> OracleColumn {
        OracleColumn {
            name: "C".into(),
            data_type: ty.into(),
            type_owner: None,
            precision: None,
            scale: None,
            data_length: 40,
            char_length: 10,
            char_used: Some("C".into()),
            not_null: false,
            generated: false,
            identity: false,
            default_source: None,
            collation: Some("BINARY".into()),
        }
    }
    #[test]
    fn source_declarations_retain_width_scale_and_units() {
        let mut c = column("NUMBER");
        c.precision = Some(9);
        c.scale = Some(-2);
        assert_eq!(c.declared_type(), "NUMBER(9,-2)");
        c.precision = None;
        assert_eq!(c.declared_type(), "NUMBER(*,-2)");
        let mut c = column("VARCHAR2");
        assert_eq!(c.declared_type(), "VARCHAR2(10 CHAR)");
        c.char_used = Some("B".into());
        assert_eq!(c.declared_type(), "VARCHAR2(40 BYTE)");
    }
    #[test]
    fn numeric_and_temporal_carriers_do_not_discard_source_precision() {
        assert_eq!(column("NUMBER").mapped(), Some(ColumnType::Numeric));
        for ty in [
            "DATE",
            "TIMESTAMP(9)",
            "TIMESTAMP(9) WITH TIME ZONE",
            "INTERVAL DAY(3) TO SECOND(9)",
        ] {
            let c = column(ty);
            assert_eq!(c.mapped(), Some(ColumnType::Text));
            assert_eq!(c.policy(), Some(MapPolicy::ViaText));
        }
        let mut c = column("APP_TYPE");
        c.type_owner = Some("APP".into());
        assert_eq!(c.mapped(), None);
        assert_eq!(c.declared_type(), "\"APP\".\"APP_TYPE\"");
    }

    #[test]
    fn carrier_refuses_missing_duplicate_and_unrepresentable_keys() {
        let mut table = OracleTable {
            owner: "APP".into(),
            name: "T".into(),
            object_id: 1,
            ddl_source: String::new(),
            schema_ddl: String::new(),
            columns: vec![column("NUMBER")],
            pk: vec![0],
            pk_validated: true,
            temporary: false,
            nested: false,
            external: false,
        };
        assert!(build_schema(&[table.clone()]).is_ok());
        for keys in [vec![], vec![0, 0], vec![1], vec![usize::MAX]] {
            table.pk = keys;
            assert!(build_schema(&[table.clone()]).is_err());
        }
    }
}
