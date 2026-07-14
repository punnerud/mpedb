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

/// Render a schema in the shape of the config file's `[[table]]` blocks.
pub fn schema_toml(schema: &Schema) -> String {
    let mut out = String::new();
    for table in &schema.tables {
        out.push_str("[[table]]\n");
        out.push_str(&format!("name = \"{}\"\n", table.name));
        let pk: Vec<String> = table
            .primary_key
            .iter()
            .map(|&i| format!("\"{}\"", table.columns[i as usize].name))
            .collect();
        out.push_str(&format!("primary_key = [{}]\n", pk.join(", ")));
        for col in &table.columns {
            out.push_str("\n  [[table.column]]\n");
            out.push_str(&format!("  name = \"{}\"\n", col.name));
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
