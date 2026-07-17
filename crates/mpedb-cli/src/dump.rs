//! `mpedb dump <file.mpedb> [--data]` — the disaster-recovery path.
//!
//! Deliberately config-free: `mpedb_core::Engine::open_from_file` reads the
//! geometry and the stored schema from the file itself, so a bare `.mpedb`
//! file (copied off a dead host, config lost) is fully inspectable.

use std::path::Path;

use mpedb_core::Engine;

use crate::args;
use crate::render::{row_line, schema_toml};
use crate::util::{usage, CliResult};

pub fn run(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &[], &["data"])?;
    let [file] = p.positional.as_slice() else {
        return usage("dump needs <file.mpedb> [--data]");
    };
    let with_data = p.has("data");

    // A sqlite file dumps through the NATIVE reader (mpedb-sqlitefmt) — no
    // sqlite library, no import: the #69 v1 read path, inspectable exactly
    // like a bare .mpedb. Detected by magic, never extension.
    if is_sqlite_file(Path::new(file)) {
        return dump_sqlite(Path::new(file), with_data);
    }

    let eng = Engine::open_from_file(Path::new(file))?;
    let r = eng.begin_read()?;
    let schema = r.stored_schema()?;

    print!("{}", schema_toml(&schema));
    for (tid, table) in schema.tables.iter().enumerate() {
        println!("# table {}: {} rows", table.name, r.row_count(tid as u32)?);
    }

    if with_data {
        for (tid, table) in schema.tables.iter().enumerate() {
            println!("\n# data: {}", table.name);
            println!(
                "{}",
                table
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join("\t")
            );
            let mut cursor = r.scan(tid as u32, None, None)?;
            while let Some(row) = cursor.next()? {
                println!("{}", row_line(&row));
            }
        }
    }
    r.finish()?;
    Ok(())
}

/// sqlite magic sniff (16 bytes), shared shape with `openpath`.
fn is_sqlite_file(p: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let mut m = [0u8; 16];
    f.read_exact(&mut m).is_ok() && &m == b"SQLite format 3\0"
}

/// Native dump of a sqlite file: tables + row counts, `--data` streams rows.
fn dump_sqlite(path: &Path, with_data: bool) -> CliResult {
    use mpedb_sqlitefmt::{SqliteFile, Value as SV};
    let f = SqliteFile::open(path).map_err(|e| crate::util::Failure::Runtime(e.to_string()))?;
    let tables = f.tables().map_err(|e| crate::util::Failure::Runtime(e.to_string()))?;
    for t in &tables {
        let mut n = 0u64;
        f.scan_table(t, &mut |_, _| {
            n += 1;
            Ok(())
        })
        .map_err(|e| crate::util::Failure::Runtime(e.to_string()))?;
        println!(
            "# table {}: {} rows ({} columns{})",
            t.name,
            n,
            t.columns.len(),
            if t.without_rowid { ", WITHOUT ROWID" } else { "" }
        );
    }
    if with_data {
        for t in &tables {
            println!("\n# data: {}", t.name);
            println!("{}", t.columns.join("\t"));
            f.scan_table(t, &mut |_, vals| {
                let line: Vec<String> = vals
                    .iter()
                    .map(|v| match v {
                        SV::Null => "NULL".to_string(),
                        SV::Int(i) => i.to_string(),
                        SV::Float(x) => format!("{x}"),
                        SV::Text(s) => s.clone(),
                        SV::Blob(b) => format!("<blob {} B>", b.len()),
                    })
                    .collect();
                println!("{}", line.join("\t"));
                Ok(())
            })
            .map_err(|e| crate::util::Failure::Runtime(e.to_string()))?;
        }
    }
    Ok(())
}
