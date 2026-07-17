//! `mpedb blob` — whole files in and out of a table's blob column.
//!
//! `put` streams the file through `WriteSession::insert_file`, so the memory
//! ceiling is one overflow page, not the file size; `get` SELECTs the column
//! and writes the bytes to a file. The blob column is picked automatically
//! (the table's LAST blob-typed column — the engine streams only into the last
//! varlen column) and can be overridden with `--col`. Positionals come first
//! and flags after, so a future `--reflink` import slots in without changing
//! the surface.

use crate::util::{open_target, parse_param, runtime, usage, CliResult, Failure};
use mpedb::{ColumnType, TableDef, Value};

const BLOB_USAGE: &str = "blob needs a subcommand:\n  \
    blob put <target> <table> <pk> <file>     [--col C]  stream a file into a blob column\n  \
    blob get <target> <table> <pk> <out-file> [--col C]  write a blob column to a file";

pub fn run(args: &[String]) -> CliResult {
    match args.first().map(String::as_str) {
        Some("put") => put(&args[1..]),
        Some("get") => get(&args[1..]),
        _ => usage(BLOB_USAGE),
    }
}

/// Split `args` into positionals and the `--col` override; unknown flags are
/// usage errors so a typo never becomes a primary-key value.
fn split_flags(args: &[String]) -> Result<(Vec<&String>, Option<String>), Failure> {
    let mut pos = Vec::new();
    let mut col = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--col" {
            match it.next() {
                Some(v) => col = Some(v.clone()),
                None => return usage("--col needs a column name"),
            }
        } else if a.starts_with("--") {
            return usage(format!("unknown flag `{a}` for blob"));
        } else {
            pos.push(a);
        }
    }
    Ok((pos, col))
}

/// Resolve the table and its blob column + single-column primary key.
/// Returns (table def, pk column index, blob column index).
fn resolve<'a>(
    schema: &'a mpedb::Schema,
    table: &str,
    col_override: Option<&str>,
) -> Result<(&'a TableDef, usize, usize), Failure> {
    let Some(t) = schema.tables.iter().find(|t| t.name == table) else {
        return runtime(format!("no such table: {table}"));
    };
    if t.primary_key.len() != 1 {
        return runtime(format!(
            "table `{table}` has a {}-column primary key; blob put/get take a single pk value",
            t.primary_key.len()
        ));
    }
    let pk_idx = t.primary_key[0] as usize;
    let blob_idx = match col_override {
        Some(name) => {
            let Some(i) = t.columns.iter().position(|c| c.name == name) else {
                return runtime(format!("table `{table}` has no column `{name}`"));
            };
            if t.columns[i].ty != ColumnType::Blob {
                return runtime(format!(
                    "column {table}.{name} is {}, not blob",
                    t.columns[i].ty
                ));
            }
            i
        }
        None => match t.columns.iter().rposition(|c| c.ty == ColumnType::Blob) {
            Some(i) => i,
            None => {
                return runtime(format!(
                    "table `{table}` has no blob column (name one with --col if the schema grows one)"
                ))
            }
        },
    };
    Ok((t, pk_idx, blob_idx))
}

fn put(args: &[String]) -> CliResult {
    let (pos, col) = split_flags(args)?;
    let &[target, table, pk, file] = pos.as_slice() else {
        return usage("blob put needs <target> <table> <pk> <file> [--col C]");
    };
    let db = open_target(target)?;
    let bundle = db.schema();
    let (t, pk_idx, blob_idx) = resolve(&bundle, table, col.as_deref())?;
    let blob_col = t.columns[blob_idx].name.clone();

    // Full row: pk from the CLI, an empty-blob placeholder for the streamed
    // column (its length comes from the file), NULL everywhere else — a
    // NOT NULL column other than those two fails with the engine's own error.
    let mut values = vec![Value::Null; t.columns.len()];
    values[pk_idx] = parse_param(pk);
    values[blob_idx] = Value::Blob(Vec::new());

    let len = std::fs::metadata(file)?.len();
    let mut s = db.begin()?;
    s.insert_file(table, &values, blob_idx, file)?;
    s.commit()?;
    println!("put {len} bytes into {table}.{blob_col} (pk {pk})");
    Ok(())
}

fn get(args: &[String]) -> CliResult {
    let (pos, col) = split_flags(args)?;
    let &[target, table, pk, out] = pos.as_slice() else {
        return usage("blob get needs <target> <table> <pk> <out-file> [--col C]");
    };
    let db = open_target(target)?;
    let bundle = db.schema();
    let (t, pk_idx, blob_idx) = resolve(&bundle, table, col.as_deref())?;
    let blob_col = &t.columns[blob_idx].name;
    let pk_col = &t.columns[pk_idx].name;

    // Chunked streaming (#50 B4): the value never materializes — a 16 GiB
    // blob costs one 256 KiB buffer, and eviction mid-read is a clean error
    // instead of mixed bytes.
    let mut f = std::io::BufWriter::new(std::fs::File::create(out)?);
    match db.blob_to_writer(table, &[parse_param(pk)], blob_col, None, &mut f)? {
        Some(n) => {
            use std::io::Write as _;
            f.flush()?;
            println!("got {n} bytes from {table}.{blob_col} (pk {pk}) -> {out}");
            Ok(())
        }
        None => runtime(format!(
            "no row in `{table}` with {pk_col} = {pk}, or {blob_col} is NULL"
        )),
    }
}
