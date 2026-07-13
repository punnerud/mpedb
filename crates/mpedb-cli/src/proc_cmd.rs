//! `mpedb proc` — stored procedures (mpedb-proc): define from a .py/.rs
//! source file, call by name or hash, list the catalog.

use std::path::Path;

use crate::args;
use crate::render::value_str;
use crate::util::{parse_params, usage, CliResult, Failure};
use mpedb::Database;
use mpedb_proc::{Budget, Lang, ProcEngine, ProcValue};

const USAGE: &str = "\
usage: mpedb proc <subcommand>

  proc define <config.toml> <file.py|file.rs>       compile + store, print name & hash
  proc call   <config.toml> <name|hash|prefix> [param ...] run a stored procedure
       --budget <instrs>[,<dbcalls>[,<rows>]]       raise/lower the call budgets
                                                    (defaults 1000000,10000,10000000)
  proc list   <config.toml>                         list stored procedures

A <hash> may be abbreviated to any unique hex prefix (>= 4 chars), git-style;
a defined name always takes precedence over a hash prefix.";

pub fn run(args: &[String]) -> CliResult {
    let Some(sub) = args.first() else {
        return usage(format!("proc needs a subcommand\n\n{USAGE}"));
    };
    let rest = &args[1..];
    match sub.as_str() {
        "define" => cmd_define(rest),
        "call" => cmd_call(rest),
        "list" => cmd_list(rest),
        other => usage(format!("unknown proc subcommand `{other}`\n\n{USAGE}")),
    }
}

fn cmd_define(args: &[String]) -> CliResult {
    let [config, file] = args else {
        return usage("proc define needs <config.toml> <file.py|file.rs>");
    };
    let path = Path::new(file);
    let lang = match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Lang::Python,
        Some("rs") => Lang::Rust,
        _ => return usage("procedure source must be a .py or .rs file"),
    };
    let source = std::fs::read_to_string(path)
        .map_err(|e| Failure::Runtime(format!("cannot read {file}: {e}")))?;
    let db = Database::open(Path::new(config))?;
    let engine = ProcEngine::new(&db);
    let hash = engine.define(&source, lang)?;
    let info = engine.info(&hash.to_string())?;
    println!("{}\t{hash}", info.name);
    Ok(())
}

fn cmd_call(argv: &[String]) -> CliResult {
    let parsed = args::parse(argv, &["budget"], &[])?;
    let [config, name_or_hash, params @ ..] = &parsed.positional[..] else {
        return usage(
            "proc call needs <config.toml> <name|hash> [param ...] \
             [--budget <instrs>[,<dbcalls>[,<rows>]]]",
        );
    };
    let db = Database::open(Path::new(config))?;
    let mut engine = ProcEngine::new(&db);
    if let Some(spec) = parsed.value("budget") {
        let (instrs, db_calls, rows) = parse_budget(spec)?;
        engine.set_budget(instrs, db_calls, rows);
    }
    let result = engine.call(name_or_hash, &parse_params(params))?;
    print_proc_value(&result);
    Ok(())
}

/// `--budget <instructions>[,<dbcalls>[,<rows>]]`; omitted trailing parts
/// keep their defaults.
fn parse_budget(spec: &str) -> Result<(u64, u64, u64), Failure> {
    let mut parts = spec.split(',');
    let mut next = |what: &str, default: u64| -> Result<u64, Failure> {
        match parts.next() {
            None => Ok(default),
            Some(s) => s.trim().parse().map_err(|_| {
                Failure::Usage(format!(
                    "--budget {what} must be an unsigned integer, got `{s}` \
                     (form: <instrs>[,<dbcalls>[,<rows>]])"
                ))
            }),
        }
    };
    let instrs = next("instructions", Budget::DEFAULT_INSTRS)?;
    let db_calls = next("db-calls", Budget::DEFAULT_DB_CALLS)?;
    let rows = next("rows", Budget::DEFAULT_ROWS)?;
    if parts.next().is_some() {
        return Err(Failure::Usage(
            "--budget takes at most three comma-separated values: \
             <instrs>[,<dbcalls>[,<rows>]]"
                .into(),
        ));
    }
    Ok((instrs, db_calls, rows))
}

fn cmd_list(args: &[String]) -> CliResult {
    let [config] = args else {
        return usage("proc list needs <config.toml>");
    };
    let db = Database::open(Path::new(config))?;
    let engine = ProcEngine::new(&db);
    println!("name\targc\tkind\thash");
    for p in engine.list()? {
        println!(
            "{}\t{}\t{}\t{}",
            p.name,
            p.argc,
            if p.writes { "write" } else { "read" },
            p.hash
        );
    }
    Ok(())
}

/// Scalars print like `exec` cells; a list of tuples prints like a result
/// set (one tab-separated line per row); anything else nests brackets.
fn print_proc_value(v: &ProcValue) {
    match v {
        ProcValue::Scalar(s) => println!("{}", value_str(s)),
        ProcValue::List(rows) if rows.iter().all(|r| matches!(r, ProcValue::Tuple(_))) => {
            for row in rows {
                let ProcValue::Tuple(cells) = row else {
                    unreachable!("checked above");
                };
                let line: Vec<String> = cells.iter().map(render_cell).collect();
                println!("{}", line.join("\t"));
            }
        }
        other => println!("{other}"),
    }
}

fn render_cell(v: &ProcValue) -> String {
    match v {
        ProcValue::Scalar(s) => value_str(s),
        other => other.to_string(),
    }
}
