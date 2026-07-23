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
mod blob;
mod collide;
mod crash;
mod csvload;
mod dump;
mod line;
mod mirror;
mod mirror_collide;
mod powerloss;
mod powerloss_commit;
mod proc_cmd;
mod queue;
mod queue_collide;
mod render;
mod openpath;
mod repl;
mod stress;
mod tier;
mod util;


use mpedb::{Error, PlanHash};
use util::{parse_params, usage, CliResult, Failure};

const USAGE: &str = "\
usage: mpedb <command> [args]
         mpedb <path> [SQL [param ...]]           sqlite3-shaped: open a repl on a
         config.toml / .mpedb file / sqlite .db, or run one statement. A .db
         opens as a delta-WAL overlay by default (changes in <db>.overlay.mpedb,
         zero import); `mpedb checkpoint <db>` folds them back. `--mirror` uses
         the full sidecar import instead; `--direct` is read-only, zero-setup.
         A MISSING path is CREATED by the FIRST WRITE: `.mpedb` → a native mpedb
         database, anything else → an empty sqlite database. Nothing else
         creates it — opening a repl, or only READING (`SELECT 1` is answered
         without touching the directory), leaves no file behind. CREATE TABLE on
         a sqlite base is applied to the base itself.
         mpedb <path> <file.csv> [--import|--analyse] [--table NAME]
         A CSV/TSV where the statement would go is offered rather than parsed:
         IMPORT it as a table, or ANALYSE it in an in-memory database and get a
         repl over it that writes nothing. On a tty you are asked; with piped
         stdin the answer is `analyse` (the one that writes nothing) unless
         --import says otherwise. Types are inferred conservatively
         (int64/float64/text; anything ambiguous is text) and an existing table
         is NEVER overwritten.
         In a repl, Tab on an EMPTY line opens a table picker: arrows to browse,
         Enter for `SELECT * FROM <table> LIMIT 20;`, Tab for the bare name.


  exec    <target> <SQL> [param ...]       run one statement
  prepare <target> <SQL>                   compile + publish, print plan hash
  advise <target> [statements.sql]         recommend indexes from the workload
         [--model <file|stored>]           (registry, a ;-separated file, or a
                                           workload model — DESIGN-MODEL-LANG.md)
  model set <target> <model.toml>          store the workload model
  model show <target>                      print the stored model
  fn define <target> <file.py|file.rs>     store a PySpell SQL function
  fn drop <target> <name>                  drop a stored function
  fn list <target>                         list stored functions
  call    <target> <hash> [param ...]      execute a prepared plan by hash
  proc    define|call|list ...              stored procedures (see `proc`)
  repl    <target>                          interactive session (stdin)
  blob    put <target> <table> <pk> <file>     [--col C]   stream a file into
          get <target> <table> <pk> <out-file> [--col C]   / out of a blob column
          (column: the table's last blob column unless --col names one)

  queue   init|enqueue|run|list ...         durable task queue: enqueue stored-
          proc tasks, `queue run` drains due work and exits when idle (the
          hibernating-service model — no daemon; see `queue`)
  <target> is a config.toml, or a .mpedb file directly (e.g. a mirror, which
  is config-free: its schema lives in the file).
  dump    <file.mpedb> [--data]             config-free schema/row dump
  bench   <config.toml>|--auto [--secs N] [--durability M] [--disk DIR]
  stress  --dir <dir> --workers N --secs S --mode bank|unique|mixed|incr
          [--size_mb M]  (default 64; exit 4 = out of space, NOT a correctness failure)
  crash   --dir <dir> --waves W --children C [--blob-kb N] [--size_mb M]
  collide --dir <dir> [--writers N] [--total T] [--drop-rate R] [--jitter-us J]
          [--keyspace K] [--detached-pct P] [--durability M]  (writer-collision fuzz)
  powerloss --dir <dir> [--rounds N] [--workers W] [--durability wal|async]
  powerloss --dir <dir> --durability commit [--rounds N] [--commits C] [--cuts K]
          [--size-mb M] [--extent-kb N] [--sabotage reorder|drop-data]
          (a DIFFERENT fault shape: `commit` publishes in place, so power loss
           drops an arbitrary SUBSET of dirty pages, not a tail. Captures the
           engine's own msync/barrier/publish trace and replays it with cuts;
           --sabotage rewrites the trace into a broken engine's and REQUIRES a
           violation, so the injector cannot be silently vacuous)
  tier    drain <hot> <cold.mpedb> --table T --where PRED [param ...]
          [--batch N] [--size-mb M] [--durability D]
          (move matching rows to a cold file; cold commits+verifies BEFORE hot
           deletes, so a crash duplicates at worst — re-run the same drain to
           reconcile. A missing <cold.mpedb> is created with the table's exact
           definition. Read back: ATTACH '<cold>' AS cold; SELECT ... UNION ALL
           SELECT ... FROM cold.<T>)
          crash --dir <dir> --waves W [--batch N]   (SIGKILL fuzz on the drain)
  mirror-collide --dir <dir> [--mode pull|push] [--writers N] [--secs S]
          [--kill-ms M] [--keyspace K]
          (SIGKILL fuzz: pull = source writers vs. a killed pull daemon (source
           is the model); push = mpedb writers vs. a killed push daemon (mpedb
           is the model) — the final drain must converge the pair exactly)

bench --auto accepts --durability none|commit|async|wal (default none); use
  --disk DIR to place the scratch db on real disk (durable modes need it)
stress/crash accept --durability none|commit|async|wal (default none)
stress/crash accept --concurrency serial|optimistic (default serial; Phase-3,
  experimental — see design/DESIGN-PHASE3.md; `incr` is the autocommit conservation mode)
crash --blob-kb N mixes ~20% N-KiB blob writes into every wave (suggest 64;
  above 256 one blob write can dominate the 5-60ms kill window and starve the
  small-txn paths); content is deterministic and byte-verified after each wave.
  NOTE: blob params exceed the intent ring's 824 B cap, so with --durability
  commit|wal blob ops take the direct writer-lock fallback, NOT the ring.
parameters parse as: null | true | false | integer | float | 0xHEX (blob) |
  ISO-8601 timestamp (2026-07-16T12:00:00Z; optional .micros and ±HH:MM offset,
  naive = UTC) | text";

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
        "advise" => cmd_advise(rest),
        "model" => cmd_model(rest),
        "fn" => cmd_fn(rest),
        "call" => cmd_call(rest),
        "proc" => proc_cmd::run(rest),
        "queue" => queue::run(rest),
        "queue-collide" => queue_collide::run_parent(rest),
        "repl" => repl::run(rest),
        "blob" => blob::run(rest),
        "dump" => dump::run(rest),
        "bench" => bench::run(rest),
        "stress" => stress::run_parent(rest),
        "crash" => crash::run_parent(rest),
        "collide" => collide::run_parent(rest),
        "mirror" => mirror::run(rest),
        "tier" => tier::run(rest),
        "tier-crash-child" => tier::run_crash_child(rest),
        "open" => match rest.split_first() {
            Some((path, more)) => openpath::run(path, more),
            None => usage("open needs <config.toml|db.mpedb|sqlite.db>"),
        },
        "checkpoint" => openpath::checkpoint(rest),
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
        "powerloss-commit-child" => powerloss_commit::run_child(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        // The sqlite3-shaped entry: a bare path is unambiguous against the
        // command names above (none of them are files), so `mpedb data.db`
        // opens — or, like `sqlite3 data.db`, CREATES — exactly as sqlite3
        // does. A MISSING name only counts as a path when it looks like one
        // (a separator or an extension); a bare misspelled word stays
        // "unknown command" instead of quietly creating a database called
        // `exce`. `mpedb open <name>` is the explicit form for the rest.
        other if looks_like_path(other) => openpath::run(other, rest),
        other => usage(format!("unknown command `{other}`")),
    }
}

/// Is this argument a database path rather than a mistyped command? An
/// existing file always is. A missing one counts when it carries a directory
/// separator or a file extension — the shapes a database name has, and shapes
/// no subcommand name has.
fn looks_like_path(arg: &str) -> bool {
    let p = std::path::Path::new(arg);
    p.exists() || arg.contains(std::path::MAIN_SEPARATOR) || p.extension().is_some()
}

fn cmd_exec(args: &[String]) -> CliResult {
    let [config, sql, params @ ..] = args else {
        return usage("exec needs <config.toml|db.mpedb> <SQL> [param ...]");
    };
    let db = crate::util::open_target(config)?;
    let res = db.query(sql, &parse_params(params))?;
    render::print_result(&res);
    Ok(())
}

fn cmd_prepare(args: &[String]) -> CliResult {
    let [config, sql] = args else {
        return usage("prepare needs <config.toml|db.mpedb> <SQL>");
    };
    let db = crate::util::open_target(config)?;
    let hash = db.prepare(sql)?;
    println!("{hash}");
    Ok(())
}

/// `mpedb model set <target> <model.toml> | show <target>` — the stored
/// workload model (design/DESIGN-MODEL-LANG.md): what this database is FOR,
/// at whatever resolution the author has, shared by every attached process.
fn cmd_model(args: &[String]) -> CliResult {
    match args {
        [sub, config, file] if sub == "set" => {
            let text = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
            let db = crate::util::open_target(config)?;
            db.set_model(&text)?;
            let m = db.model()?.expect("just stored");
            println!(
                "model stored: archetype {}, {} table shape(s), {} statement(s)",
                m.archetype.map(|a| a.name()).unwrap_or("(none)"),
                m.tables.len(),
                m.statements.len()
            );
            Ok(())
        }
        [sub, config] if sub == "show" => {
            let db = crate::util::open_target(config)?;
            match db.model_source()? {
                Some(src) => println!("{src}"),
                None => println!("no model stored — see design/DESIGN-MODEL-LANG.md"),
            }
            Ok(())
        }
        _ => usage("model needs: set <target> <model.toml> | show <target>"),
    }
}

/// `mpedb fn define <target> <file.py|file.rs> | drop <target> <name> |
/// list <target>` — stored SQL functions (stage M2): PySpell compiled at
/// define time, stored content-addressed in the file, callable from any
/// attached process's SQL.
fn cmd_fn(args: &[String]) -> CliResult {
    use mpedb::spellfn::SpellLang;
    match args {
        [sub, config, file] if sub == "define" => {
            let lang = if file.ends_with(".rs") { SpellLang::Rust } else { SpellLang::Python };
            let src = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
            let db = crate::util::open_target(config)?;
            let (name, hash) = db.create_function(lang, &src)?;
            println!("function {name} stored as {hash}");
            Ok(())
        }
        [sub, config, name] if sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.drop_function(name)? {
                println!("function {name} dropped");
            } else {
                println!("no function named {name}");
            }
            Ok(())
        }
        [sub, config] if sub == "list" => {
            let db = crate::util::open_target(config)?;
            let fns = db.list_functions()?;
            if fns.is_empty() {
                println!("no stored functions");
            }
            for f in fns {
                println!("{}/{}  {}", f.name, f.argc, f.hash_hex);
            }
            Ok(())
        }
        _ => usage("fn needs: define <target> <file.py|rs> | drop <target> <name> | list <target>"),
    }
}

/// `mpedb advise <target> [statements.sql | --model <model.toml|stored>]` —
/// the #118 workload-index advisor, recommend-only. With no source the
/// workload is the plan registry: everything this database has ever compiled.
fn cmd_advise(args: &[String]) -> CliResult {
    use mpedb::advisor::WorkloadSource;
    let (config, source) = match args {
        [config] => (config, None),
        [config, flag, spec] if flag == "--model" => (config, Some((true, spec.clone()))),
        [config, file] => (config, Some((false, file.clone()))),
        _ => {
            return usage(
                "advise needs <config.toml|db.mpedb> [statements.sql | --model <file|stored>]",
            )
        }
    };
    let db = crate::util::open_target(config)?;
    let source = match source {
        None => WorkloadSource::Registry,
        Some((true, spec)) => {
            let model = if spec == "stored" {
                db.model()?.ok_or_else(|| {
                    Failure::Runtime("no model stored — `mpedb model set` first".into())
                })?
            } else {
                let text = std::fs::read_to_string(&spec)
                    .map_err(|e| Failure::Runtime(format!("reading {spec}: {e}")))?;
                mpedb::WorkloadModel::from_toml_str(&text)?
            };
            WorkloadSource::Model(model)
        }
        Some((false, file)) => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
            let stmts: Vec<String> = text
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            WorkloadSource::Statements(stmts)
        }
    };
    let rep = db.recommend_indexes(source)?;
    println!(
        "workload: {} compiled, {} uncompilable, {} shapes without a single-table          candidate, {} opaque filters, {} no-key, {} already served",
        rep.compiled, rep.uncompilable, rep.skipped_shape, rep.opaque_filter, rep.no_key,
        rep.served
    );
    if rep.advices.is_empty() {
        println!("no index recommendations.");
        return Ok(());
    }
    println!();
    println!("{:<40} {:>10} {:>6}  id", "candidate", "statements", "rows");
    for a in &rep.advices {
        println!(
            "{:<40} {:>10} {:>6}  {}…",
            format!("{}({})", a.table, a.columns.join(", ")),
            a.statements,
            format!("2^{}", a.rows_bucket),
            &a.index_id[..12]
        );
    }
    println!();
    println!("recommend-only: auto-create stays blocked on #118's P2 (index state               bit), P3 (DROP INDEX), P5 (execution counts).");
    Ok(())
}

fn cmd_call(args: &[String]) -> CliResult {
    let [config, hash, params @ ..] = args else {
        return usage("call needs <config.toml|db.mpedb> <hash> [param ...]");
    };
    let hash: PlanHash = hash
        .parse()
        .map_err(|_| Failure::Usage("hash must be 64 hex characters".into()))?;
    let db = crate::util::open_target(config)?;
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
