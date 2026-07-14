//! `mpedb` — command-line tool for mpedb databases.
//!
//! User-facing subcommands: exec / prepare / call / repl / dump / bench /
//! stress / crash. `stress-child` and `crash-child` are hidden re-entry
//! points used by the multi-process tests (`current_exe()` respawn).
//!
//! Exit codes: 0 ok, 1 runtime error, 2 usage error. Stress/crash children
//! additionally use 3 (invariant violation — an MVCC/engine bug) and 4
//! (unexpected error inside a child).

mod args;
mod bench;
mod collide;
mod crash;
mod dump;
mod mirror;
mod mirror_collide;
mod powerloss;
mod proc_cmd;
mod render;
mod repl;
mod stress;
mod util;

use std::path::Path;

use mpedb::{Database, Error, PlanHash};
use util::{parse_params, usage, CliResult, Failure};

const USAGE: &str = "\
usage: mpedb <command> [args]

  exec    <config.toml> <SQL> [param ...]   run one statement
  prepare <config.toml> <SQL>               compile + publish, print plan hash
  call    <config.toml> <hash> [param ...]  execute a prepared plan by hash
  proc    define|call|list ...              stored procedures (see `proc`)
  repl    <config.toml>                     interactive session (stdin)
  dump    <file.mpedb> [--data]             config-free schema/row dump
  bench   <config.toml>|--auto [--secs N] [--durability M] [--disk DIR]
  stress  --dir <dir> --workers N --secs S --mode bank|unique|mixed|incr
  crash   --dir <dir> --waves W --children C
  collide --dir <dir> [--writers N] [--total T] [--drop-rate R] [--jitter-us J]
          [--keyspace K] [--detached-pct P] [--durability M]  (writer-collision fuzz)
  powerloss --dir <dir> [--rounds N] [--workers W] [--durability wal|async]
  mirror-collide --dir <dir> [--mode pull|push] [--writers N] [--secs S]
          [--kill-ms M] [--keyspace K]
          (SIGKILL fuzz: pull = source writers vs. a killed pull daemon (source
           is the model); push = mpedb writers vs. a killed push daemon (mpedb
           is the model) — the final drain must converge the pair exactly)

bench --auto accepts --durability none|commit|async|wal (default none); use
  --disk DIR to place the scratch db on real disk (durable modes need it)
stress/crash accept --durability none|commit|async|wal (default none)
stress/crash accept --concurrency serial|optimistic (default serial; Phase-3,
  experimental — see DESIGN-PHASE3.md; `incr` is the autocommit conservation mode)
parameters parse as: null | true | false | integer | float | 0xHEX (blob) | text";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&argv) {
        Ok(()) => 0,
        Err(Failure::Usage(msg)) => {
            eprintln!("mpedb: {msg}\n\n{USAGE}");
            2
        }
        Err(Failure::Runtime(msg)) => {
            eprintln!("mpedb: {msg}");
            1
        }
    };
    std::process::exit(code);
}

fn dispatch(argv: &[String]) -> CliResult {
    let Some(cmd) = argv.first() else {
        return usage("no command given");
    };
    let rest = &argv[1..];
    match cmd.as_str() {
        "exec" => cmd_exec(rest),
        "prepare" => cmd_prepare(rest),
        "call" => cmd_call(rest),
        "proc" => proc_cmd::run(rest),
        "repl" => repl::run(rest),
        "dump" => dump::run(rest),
        "bench" => bench::run(rest),
        "stress" => stress::run_parent(rest),
        "crash" => crash::run_parent(rest),
        "collide" => collide::run_parent(rest),
        "mirror" => mirror::run(rest),
        "mirror-collide" => mirror_collide::run_parent(rest),
        "powerloss" => powerloss::run_parent(rest),
        "stress-child" => stress::run_child(rest),
        "crash-child" => crash::run_child(rest),
        "collide-child" => collide::run_child(rest),
        "mirror-collide-writer" => mirror_collide::run_writer(rest),
        "mirror-collide-mwriter" => mirror_collide::run_mwriter(rest),
        "mirror-collide-daemon" => mirror_collide::run_daemon(rest),
        "mirror-collide-pdaemon" => mirror_collide::run_push_daemon(rest),
        "powerloss-child" => powerloss::run_child(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => usage(format!("unknown command `{other}`")),
    }
}

fn cmd_exec(args: &[String]) -> CliResult {
    let [config, sql, params @ ..] = args else {
        return usage("exec needs <config.toml> <SQL> [param ...]");
    };
    let db = Database::open(Path::new(config))?;
    let res = db.query(sql, &parse_params(params))?;
    render::print_result(&res);
    Ok(())
}

fn cmd_prepare(args: &[String]) -> CliResult {
    let [config, sql] = args else {
        return usage("prepare needs <config.toml> <SQL>");
    };
    let db = Database::open(Path::new(config))?;
    let hash = db.prepare(sql)?;
    println!("{hash}");
    Ok(())
}

fn cmd_call(args: &[String]) -> CliResult {
    let [config, hash, params @ ..] = args else {
        return usage("call needs <config.toml> <hash> [param ...]");
    };
    let hash: PlanHash = hash
        .parse()
        .map_err(|_| Failure::Usage("hash must be 64 hex characters".into()))?;
    let db = Database::open(Path::new(config))?;
    match db.execute(&hash, &parse_params(params)) {
        Ok(res) => {
            render::print_result(&res);
            Ok(())
        }
        Err(Error::UnknownPlan(h)) => Err(Failure::Runtime(format!(
            "plan {h} is not in the shared registry; \
             prepare it first: mpedb prepare <config.toml> '<SQL>'"
        ))),
        Err(e) => Err(e.into()),
    }
}
