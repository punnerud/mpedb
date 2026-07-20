//! Result and schema rendering: tab-separated rows with a header line, and a
//! TOML-shaped schema dump (matches the config file's `[[table]]` layout).

use mpedb::{ExecResult, Schema, Value};
use mpedb_types::DefaultExpr;

/// Raw cell rendering for tab-separated output: Text is printed unquoted,
/// blobs as `0x…` hex, NULL as `NULL`, timestamps as µs since the epoch.
pub fn value_str(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Bool(b) => b.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => {
            let mut out = String::with_capacity(2 + b.len() * 2);
            out.push_str("0x");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out
        }
        Value::Timestamp(t) => t.to_string(),
        // Param-only (§2.6), so no result cell holds one — but render it rather
        // than panicking if one ever surfaces (e.g. an EXPLAIN of a bound param).
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(value_str).collect();
            format!("({})", inner.join(", "))
        }
    }
}

pub fn row_line(row: &[Value]) -> String {
    row.iter().map(value_str).collect::<Vec<_>>().join("\t")
}

pub fn print_result(res: &ExecResult) {
    match res {
        ExecResult::Rows { columns, rows } => {
            println!("{}", columns.join("\t"));
            for row in rows {
                println!("{}", row_line(row));
            }
        }
        ExecResult::Affected(n) => println!("affected: {n}"),
        ExecResult::Explain(text) => println!("{text}"),
    }
}

/// Emit `s` as a TOML **basic string**, escaping what TOML requires.
///
/// `schema_toml` used to interpolate names raw (`name = "{}"`). That was fine
/// while identifiers were `[A-Za-z_][A-Za-z0-9_]*`; now that a quoted
/// identifier may contain spaces, punctuation and `"` (design/DESIGN-TABLE-CAP
/// §7), a raw interpolation would emit TOML that re-parses as a DIFFERENT name
/// — or not at all. Control characters cannot appear in an identifier, so `\`
/// and `"` are the whole escape set; the `\u` arm is belt-and-braces for any
/// non-identifier string that reaches here.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render a schema in the shape of the config file's `[[table]]` blocks.
pub fn schema_toml(schema: &Schema) -> String {
    let mut out = String::new();
    for table in &schema.tables {
        out.push_str("[[table]]\n");
        out.push_str(&format!("name = {}\n", toml_str(&table.name)));
        let pk: Vec<String> = table
            .primary_key
            .iter()
            .map(|&i| toml_str(&table.columns[i as usize].name))
            .collect();
        out.push_str(&format!("primary_key = [{}]\n", pk.join(", ")));
        for col in &table.columns {
            out.push_str("\n  [[table.column]]\n");
            out.push_str(&format!("  name = {}\n", toml_str(&col.name)));
            out.push_str(&format!("  type = \"{}\"\n", col.ty));
            out.push_str(&format!("  nullable = {}\n", col.nullable));
            if col.unique {
                out.push_str("  unique = true\n");
            }
            match &col.default {
                None => {}
                Some(DefaultExpr::Now) => out.push_str("  default = \"now()\"\n"),
                Some(DefaultExpr::Const(v)) => {
                    out.push_str(&format!("  default = {v}\n"));
                }
            }
            if let Some(check) = &col.check {
                out.push_str(&format!("  check = \"{check}\"\n"));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb_types::{Affinity, Collation, ColumnDef, ColumnType, Config, TableDef, TableKind};

    /// A dumped schema must be re-readable as a config. Identifiers may now
    /// contain spaces, punctuation and `"` (design/DESIGN-TABLE-CAP.md §7), so
    /// the raw `name = "{}"` interpolation this used to do would emit TOML that
    /// re-parses as a DIFFERENT name — or not at all.
    #[test]
    fn odd_identifiers_round_trip_through_the_toml_dump() {
        let names = [
            "weird tbl",
            "tbl-with.punct!",
            "1st table",
            "tabell_æøå",
            "a\"b",
            "  padded  ",
            "back\\slash",
        ];
        let col = |n: &str| ColumnDef { generated: None, decl: None,
            name: n.into(),
            ty: ColumnType::Int64,
            nullable: false,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        };
        let tables: Vec<TableDef> = names
            .iter()
            .enumerate()
            .map(|(i, n)| TableDef {
                id: i as u32,
                name: (*n).into(),
                columns: vec![col(n), col(&format!("c {n}"))],
                primary_key: vec![0],
                indexes: vec![],
                dead: false,
                kind: TableKind::Standard,
                implicit_rowid: false,
            })
            .collect();
        let schema = Schema::new(tables).unwrap();

        let toml = format!(
            "[database]\npath = \"/dev/shm/render-roundtrip.mpedb\"\nsize_mb = 16\n\n{}",
            schema_toml(&schema)
        );
        let cfg = Config::from_toml_str(&toml)
            .unwrap_or_else(|e| panic!("dumped schema is not re-readable TOML: {e}\n{toml}"));
        // Byte-for-byte: same names, same order, same columns.
        assert_eq!(cfg.schema, schema, "TOML round-trip changed the schema");
        for n in names {
            assert!(cfg.schema.table_id(n).is_some(), "lost table {n:?}");
        }
    }
}
