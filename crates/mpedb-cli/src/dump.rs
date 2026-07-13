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
