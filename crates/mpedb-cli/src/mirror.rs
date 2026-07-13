//! `mpedb mirror …` — bidirectional sqlite/PostgreSQL ⇄ mpedb mirroring
//! (DESIGN-MIRROR.md). This stage (M2.4) wires the sqlite import and a status
//! read; pull/push/switch land in later milestones.

use std::path::PathBuf;

use mpedb_core::Engine;
use mpedb_mirror::state;
use mpedb_mirror::{import_sqlite, ImportOptions};
use mpedb_types::Durability;
use rusqlite::Connection;

use crate::args;
use crate::util::{usage, CliResult, Failure};

pub fn run(argv: &[String]) -> CliResult {
    let (sub, rest) = argv
        .split_first()
        .ok_or_else(|| Failure::Usage(HELP.into()))?;
    match sub.as_str() {
        "import" => cmd_import(rest),
        "status" => cmd_status(rest),
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        other => usage(format!("unknown mirror subcommand `{other}`\n\n{HELP}")),
    }
}

const HELP: &str = "\
mpedb mirror <subcommand>

  import  --source <sqlite-file> --dest <new-mpedb-file>
          [--include t1,t2] [--exclude t3] [--size_mb N] [--durability none|commit|wal]
      Import a sqlite database into a NEW .mpedb mirror file.

  status  --db <mpedb-file>
      Show a mirror's config and authority state.";

fn cmd_import(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["source", "dest"],
        &["include", "exclude", "size_mb", "durability"],
    )?;
    let source = p.require("source")?;
    let dest = PathBuf::from(p.require("dest")?);

    let include = p
        .value("include")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>());
    let exclude = p
        .value("exclude")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let size_mb = match p.value("size_mb") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| Failure::Usage("--size_mb must be an integer".into()))?,
        None => 256,
    };
    let durability = match p.value("durability") {
        None | Some("none") => Durability::None,
        Some("commit") => Durability::Commit,
        Some("wal") => Durability::Wal,
        Some(other) => return usage(format!("--durability must be none|commit|wal, got `{other}`")),
    };

    let mut conn = Connection::open(source)
        .map_err(|e| Failure::Runtime(format!("open sqlite source `{source}`: {e}")))?;
    let opts = ImportOptions {
        size_bytes: size_mb * 1024 * 1024,
        durability,
        include,
        exclude,
        batch_rows: 8192,
    };
    let (_db, report) = import_sqlite(&mut conn, &dest, &opts)?;

    println!("imported {} into {}", source, dest.display());
    for t in &report.tables {
        println!("  {:<24} {:>10} rows  (table {})", t.name, t.rows, t.table_id);
    }
    println!("  total: {} rows across {} tables", report.total_rows(), report.tables.len());
    println!("mirror is source-authoritative (epoch 1); capture enabled.");
    Ok(())
}

fn cmd_status(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db"], &[])?;
    let path = PathBuf::from(p.require("db")?);
    let eng = Engine::open_from_file(&path)?;
    let r = eng.begin_read()?;
    let cfg_bytes = r.sys_get(&state::sys_subkey(state::KEY_CFG))?;
    let epoch_bytes = r.sys_get(&state::sys_subkey(state::KEY_EPOCH))?;
    r.finish()?;

    let Some(cfg_bytes) = cfg_bytes else {
        return Err(Failure::Runtime(format!(
            "{} is not a mirror (no mir/cfg record)",
            path.display()
        )));
    };
    let cfg = state::MirrorConfig::decode(&cfg_bytes)?;
    let epoch = epoch_bytes
        .as_deref()
        .map(state::Epoch::decode)
        .transpose()?;

    println!("mirror: {}", path.display());
    println!("  source:   {:?}", cfg.source_kind);
    println!("  mode:     {:?}", cfg.mode);
    println!("  tables:   {} mirrored", cfg.scope.len());
    match epoch {
        Some(e) => {
            println!("  epoch:    {}", e.epoch);
            println!("  authority:{:?}", e.authority);
            println!("  state:    {:?}", e.state);
            println!("  frozen:   {}", e.frozen);
        }
        None => println!("  epoch:    (missing)"),
    }
    Ok(())
}
