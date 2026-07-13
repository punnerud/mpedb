use crate::error::{Error, Result};
use crate::value::{read_value, write_value, ColumnType, Value};
use crate::{MAX_COLUMNS, MAX_TABLES};

/// Default value for a column when an INSERT omits it.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultExpr {
    Const(Value),
    /// `now()` — the commit-time timestamp, filled in by the engine.
    Now,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub unique: bool,
    pub default: Option<DefaultExpr>,
    /// CHECK expression source (SQL expression over this table's columns).
    /// Compiled to expression IR at attach time by the SQL layer; the source
    /// text participates in the schema hash.
    pub check: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Indices into `columns`. Non-empty; PK columns must be NOT NULL.
    pub primary_key: Vec<u16>,
}

impl TableDef {
    pub fn column_index(&self, name: &str) -> Option<u16> {
        self.columns.iter().position(|c| c.name == name).map(|i| i as u16)
    }

    pub fn pk_types(&self) -> Vec<ColumnType> {
        self.primary_key
            .iter()
            .map(|&i| self.columns[i as usize].ty)
            .collect()
    }

    pub fn is_pk_column(&self, col: u16) -> bool {
        self.primary_key.contains(&col)
    }
}

/// A validated schema. Tables are sorted by name; a table's id is its index
/// in `tables` (stable because attach requires an identical schema hash).
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub tables: Vec<TableDef>,
}

fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with("__mpedb")
}

impl Schema {
    /// Build and validate a schema from table definitions (any order; sorted
    /// internally by name).
    pub fn new(mut tables: Vec<TableDef>) -> Result<Schema> {
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    fn validate(&self) -> Result<()> {
        if self.tables.is_empty() {
            return Err(Error::Schema("schema defines no tables".into()));
        }
        if self.tables.len() > MAX_TABLES - 8 {
            return Err(Error::Schema(format!(
                "too many tables ({} > {})",
                self.tables.len(),
                MAX_TABLES - 8 // headroom for system tables
            )));
        }
        for w in self.tables.windows(2) {
            if w[0].name == w[1].name {
                return Err(Error::Schema(format!("duplicate table `{}`", w[0].name)));
            }
        }
        for t in &self.tables {
            if !valid_identifier(&t.name) {
                return Err(Error::Schema(format!("invalid table name `{}`", t.name)));
            }
            if t.columns.is_empty() || t.columns.len() > MAX_COLUMNS {
                return Err(Error::Schema(format!(
                    "table `{}` must have 1..={MAX_COLUMNS} columns",
                    t.name
                )));
            }
            let mut names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
            names.sort_unstable();
            if names.windows(2).any(|w| w[0] == w[1]) {
                return Err(Error::Schema(format!("duplicate column in `{}`", t.name)));
            }
            for c in &t.columns {
                if !valid_identifier(&c.name) {
                    return Err(Error::Schema(format!(
                        "invalid column name `{}.{}`",
                        t.name, c.name
                    )));
                }
                if let Some(DefaultExpr::Const(v)) = &c.default {
                    if !v.fits(c.ty) {
                        return Err(Error::Schema(format!(
                            "default for `{}.{}` has type {}, column is {}",
                            t.name,
                            c.name,
                            v.type_name(),
                            c.ty
                        )));
                    }
                    if v.is_null() && !c.nullable {
                        return Err(Error::Schema(format!(
                            "NULL default on NOT NULL column `{}.{}`",
                            t.name, c.name
                        )));
                    }
                }
                if matches!(&c.default, Some(DefaultExpr::Now)) && c.ty != ColumnType::Timestamp {
                    return Err(Error::Schema(format!(
                        "now() default requires timestamp column, `{}.{}` is {}",
                        t.name, c.name, c.ty
                    )));
                }
            }
            if t.primary_key.is_empty() {
                return Err(Error::Schema(format!(
                    "table `{}` has no primary key",
                    t.name
                )));
            }
            let mut pk = t.primary_key.clone();
            pk.sort_unstable();
            if pk.windows(2).any(|w| w[0] == w[1]) {
                return Err(Error::Schema(format!(
                    "duplicate primary key column in `{}`",
                    t.name
                )));
            }
            for &i in &t.primary_key {
                let c = t.columns.get(i as usize).ok_or_else(|| {
                    Error::Schema(format!("primary key index {i} out of range in `{}`", t.name))
                })?;
                if c.nullable {
                    return Err(Error::Schema(format!(
                        "primary key column `{}.{}` must be NOT NULL",
                        t.name, c.name
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn table_id(&self, name: &str) -> Option<u32> {
        self.tables
            .binary_search_by(|t| t.name.as_str().cmp(name))
            .ok()
            .map(|i| i as u32)
    }

    pub fn table(&self, id: u32) -> Option<&TableDef> {
        self.tables.get(id as usize)
    }

    /// Canonical, deterministic serialization — the schema-hash preimage and
    /// the format stored in the database catalog.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.push(1u8); // schema encoding version
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for t in &self.tables {
            write_str(&mut buf, &t.name);
            buf.extend_from_slice(&(t.columns.len() as u16).to_le_bytes());
            for c in &t.columns {
                write_str(&mut buf, &c.name);
                buf.push(c.ty as u8);
                buf.push((c.nullable as u8) | ((c.unique as u8) << 1));
                match &c.default {
                    None => buf.push(0),
                    Some(DefaultExpr::Const(v)) => {
                        buf.push(1);
                        write_value(&mut buf, v);
                    }
                    Some(DefaultExpr::Now) => buf.push(2),
                }
                match &c.check {
                    None => buf.push(0),
                    Some(src) => {
                        buf.push(1);
                        write_str(&mut buf, src);
                    }
                }
            }
            buf.extend_from_slice(&(t.primary_key.len() as u16).to_le_bytes());
            for &i in &t.primary_key {
                buf.extend_from_slice(&i.to_le_bytes());
            }
        }
        buf
    }

    /// Parse [`canonical_bytes`] output (bounds-checked; used when attaching
    /// to an existing database to recover its schema from the catalog).
    pub fn from_canonical_bytes(buf: &[u8]) -> Result<Schema> {
        let err = || Error::Corrupt("truncated schema".into());
        let mut pos = 0usize;
        let version = *buf.get(pos).ok_or_else(err)?;
        pos += 1;
        if version != 1 {
            return Err(Error::Corrupt(format!("unknown schema version {version}")));
        }
        let ntables = read_u32(buf, &mut pos)? as usize;
        if ntables > MAX_TABLES {
            return Err(Error::Corrupt("table count out of range".into()));
        }
        let mut tables = Vec::with_capacity(ntables);
        for _ in 0..ntables {
            let name = read_str(buf, &mut pos)?;
            let ncols = read_u16(buf, &mut pos)? as usize;
            if ncols > MAX_COLUMNS {
                return Err(Error::Corrupt("column count out of range".into()));
            }
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let cname = read_str(buf, &mut pos)?;
                let ty = ColumnType::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column type".into()))?;
                pos += 1;
                let flags = *buf.get(pos).ok_or_else(err)?;
                pos += 1;
                let default = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(DefaultExpr::Const(read_value(buf, &mut pos)?))
                    }
                    2 => {
                        pos += 1;
                        Some(DefaultExpr::Now)
                    }
                    _ => return Err(Error::Corrupt("bad default tag".into())),
                };
                let check = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad check tag".into())),
                };
                columns.push(ColumnDef {
                    name: cname,
                    ty,
                    nullable: flags & 1 != 0,
                    unique: flags & 2 != 0,
                    default,
                    check,
                });
            }
            let npk = read_u16(buf, &mut pos)? as usize;
            if npk > ncols {
                return Err(Error::Corrupt("pk count out of range".into()));
            }
            let mut primary_key = Vec::with_capacity(npk);
            for _ in 0..npk {
                primary_key.push(read_u16(buf, &mut pos)?);
            }
            tables.push(TableDef {
                name,
                columns,
                primary_key,
            });
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("trailing bytes in schema".into()));
        }
        // Re-validate: canonical bytes from a hostile/corrupt mapping must
        // still produce a schema every other invariant can rely on.
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let raw = buf
        .get(*pos..*pos + 2)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let raw = buf
        .get(*pos..*pos + 4)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32(buf, pos)? as usize;
    if len > 1 << 20 {
        return Err(Error::Corrupt("string too long in schema".into()));
    }
    let raw = buf
        .get(*pos..*pos + len)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += len;
    String::from_utf8(raw.to_vec()).map_err(|_| Error::Corrupt("invalid utf-8 in schema".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Schema {
        Schema::new(vec![TableDef {
            name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: ColumnType::Int64,
                    nullable: false,
                    unique: false,
                    default: None,
                    check: None,
                },
                ColumnDef {
                    name: "email".into(),
                    ty: ColumnType::Text,
                    nullable: false,
                    unique: true,
                    default: None,
                    check: None,
                },
                ColumnDef {
                    name: "age".into(),
                    ty: ColumnType::Int64,
                    nullable: true,
                    unique: false,
                    default: Some(DefaultExpr::Const(Value::Int(0))),
                    check: Some("age >= 0 AND age < 200".into()),
                },
            ],
            primary_key: vec![0],
        }])
        .unwrap()
    }

    #[test]
    fn canonical_roundtrip_and_stable_hash() {
        let s = sample();
        let restored = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, restored);
        assert_eq!(s.hash(), restored.hash());
    }

    #[test]
    fn table_order_is_name_sorted_regardless_of_input_order() {
        let mk = |names: &[&str]| {
            Schema::new(
                names
                    .iter()
                    .map(|n| TableDef {
                        name: n.to_string(),
                        columns: vec![ColumnDef {
                            name: "id".into(),
                            ty: ColumnType::Int64,
                            nullable: false,
                            unique: false,
                            default: None,
                            check: None,
                        }],
                        primary_key: vec![0],
                    })
                    .collect(),
            )
            .unwrap()
        };
        assert_eq!(mk(&["b", "a"]).hash(), mk(&["a", "b"]).hash());
        assert_eq!(mk(&["b", "a"]).table_id("a"), Some(0));
    }

    #[test]
    fn rejects_bad_schemas() {
        // nullable PK
        let bad = Schema::new(vec![TableDef {
            name: "t".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: true,
                unique: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
        }]);
        assert!(bad.is_err());
        // reserved prefix
        let bad = Schema::new(vec![TableDef {
            name: "__mpedb_plans".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
        }]);
        assert!(bad.is_err());
    }
}
