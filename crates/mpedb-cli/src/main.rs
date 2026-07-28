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
mod map_collide;
mod mirror_collide;
mod powerloss;
mod powerloss_commit;
mod proc_cmd;
mod listen;
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
         [--columnar [--emit-model]]       …or column-vs-row storage advice;
                                           --emit-model prints a proposed [model]
  model set <target> <model.toml>          store the workload model
  model show <target>                      print the stored model
  model sync-columnar <target>             build column segments for the tables
                                           the model marks scan-heavy (fact /
                                           star-olap); drop them for row-oriented
                                           ones — automatic + sparse via MPEE
  model maintain <target>                  adaptive columnar upkeep: build/rebuild
         [--tail-fraction F] [--max N]     only what went stale (absent, or tail
         [--plan]                          grown past F); --plan = dry-run
  fn define <target> <file.py|file.rs>     store a PySpell SQL function
  fn drop <target> <name>                  drop a stored function
  fn list <target>                         list stored functions
  lens define <target> <name> <fwd> <inv>  register a reversible pair over stored
         [--class bijective|residual|lossy]  functions; bijective AND residual are
         [--rex <fn> --residual-type <ty>]   VERIFIED (GetPut over a probe corpus)
                                           and refused with a counter-example;
                                           residual = the triple fwd/1 rex/1 inv/2
  lens verify|list|drop <target> [name]    re-run a pair's round trip, or manage
  rretl apply <target> <pair> <tbl>.<col>    transform a column IN PLACE; what was
                                           lost is kept per row (rretl_residual),
                                           the run is lineage (rretl_lineage), and
                                           100% of rows verify against the source
                                           hash BEFORE the destroying commit
  rretl revert <target> <run_id>             put it back exactly (hash-gated)
  rretl putback <target> <run_id>            invert KEEPING edits made to the
                                           transformed column (lens putback,
                                           PutRes-verified per row); deleted
                                           rows stay deleted
  rretl log <target>                         every run, failed runs included
  rretl put <target> <obj> <file>            store the next VERSION of a blob:
                                           newest stays full, the previous one
                                           is rewritten as a reverse delta
                                           (verified byte-identical before the
                                           commit); every 8th stays full
  rretl get <target> <obj> <ver> <out>       materialize any version, every step
                                           hash-verified — never silent rot
  rretl versions <target> <obj>              list versions and how each is stored
  rretl prune <target> <obj> <keep>          delete the OLDEST versions, keep the
                                           newest <keep> — chain-safe (deltas
                                           base upward), recorded as lineage
  rretl pack-in <target> <name> <zip>        splice a zip into rows + residual;
                                           reconstruction verified byte-identical
                                           BEFORE the ingest commits
  rretl pack-out <target> <id> <out>         rebuild the zip, hash-gated
  rretl archives <target>                    list spliced archives
  rretl map define <target> <map.toml>       store a table-SET map: source
                                           tables mirrored into a different
                                           target shape through lens pairs
                                           (design/DESIGN-RRETL.md §13)
  rretl map sync <target> <name>             sync BOTH directions in one txn:
                                           edits flow through the pairs,
                                           repeating is a no-op (state-hash
                                           echo guard), both-sides-moved is
                                           a named CONFLICT that aborts whole
  rretl map check <target> <name>            READ-ONLY dry run: what a sync
                                           would move, EVERY conflict named,
                                           plus the audit the echo guard
                                           cannot do (state says clean but
                                           forward(A) != B); exit 1 on breach
  rretl map run <target> <name>              the DAEMON form (cron): commits as
         [--max-secs S] [--max-rows N]       it goes, so a bounded run makes
         [--runner ID] [--lease-secs S]      real progress and the next one
                                           RESUMES where it stopped. Every
                                           commit advances the WHOLE set (a
                                           chunk from each table), conflicts
                                           are counted and skipped, and a
                                           recorded runner may be required
  rretl map runner <target> <name> <id>      restrict who may `map run` it
                                           (empty clears) — a guard against
                                           mistakes, not an auth boundary
  rretl map status <target> <name>           round, runner, live lease, and
                                           which tables are mid-round
  rretl map show|list|drop <target> [name]   manage stored maps
  ingest define <target> <source.toml>     declare an external source: the CALL
                                           GRAPH, the budget vector and the
                                           conflict policy (DESIGN-INGEST.md)
  ingest show|list|drop <target> [name]    manage declared sources
  ingest state <target> <source>           per edge: watermark, CURSOR VERDICT
                                           (safe/unsafe — a dump verifies it and
                                           names it when it lies), change rate
  ingest advise <target> <source>          the plan: which call, how often, in
         [--emit-cron] [--cmd <script>]    which profile — plus what it could
                                           NOT plan and why
  ingest conflicts <target> <source>       what the policy would not decide;
                                           exit 1 when non-empty (cron mails it)
  ingest resolve <target> <source> --take local   clear them, having decided
  op define <target> <sym> <fixity> <f.py> define a custom :sym: operator
  op drop|list|install-model <target> ...  manage custom operators
  tune set <target> name=value | show      stored engine switches (ndv_discount,
                                           recursive_triggers) — coherent everywhere
  trigger backtest <target> <name|SQL> [n]  replay a trigger (stored, or a full
                                           CREATE TRIGGER dry-run) over current
                                           rows, ALWAYS rolled back: what would
                                           it have done? | trigger list <target>
  cost-policy set <target> <f.py> | drop   the programmable cost adjustment
  stats <target>                           what the engine believes (rows/NDV)
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
  map-collide --dir <dir> [--mode sync|run] [--writers N] [--secs S]
              [--kill-ms M] [--keyspace K]
                                           SIGKILL fuzz for `rretl map sync`:
                                           writers churn BOTH sides while the
                                           syncer is killed at every instant;
                                           final drain must converge to a
                                           clean check/fsck — nothing lost,
                                           duplicated or half-synced
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
        // What a packaged binary is expected to answer (`brew test` runs it).
        "--version" | "-V" | "version" => {
            println!("mpedb {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "exec" => cmd_exec(rest),
        "prepare" => cmd_prepare(rest),
        "advise" => cmd_advise(rest),
        "model" => cmd_model(rest),
        "fn" => cmd_fn(rest),
        "lens" => cmd_lens(rest),
        "rretl" => cmd_rretl(rest),
        "ingest" => cmd_ingest(rest),
        "op" => cmd_op(rest),
        "tune" => cmd_tune(rest),
        "trigger" => cmd_trigger(rest),
        "cost-policy" => cmd_cost_policy(rest),
        "stats" => cmd_stats(rest),
        "call" => cmd_call(rest),
        "proc" => proc_cmd::run(rest),
        "queue" => queue::run(rest),
        "listen" => listen::run(rest),
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
        "map-collide" => map_collide::run_parent(rest),
        "map-collide-awriter" => map_collide::run_awriter(rest),
        "map-collide-bwriter" => map_collide::run_bwriter(rest),
        "map-collide-syncer" => map_collide::run_syncer(rest),
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
        [sub, config] if sub == "sync-derived" => {
            let db = crate::util::open_target(config)?;
            let r = db.sync_model_derived()?;
            for n in &r.installed {
                println!("installed {n}");
            }
            for n in &r.kept {
                println!("kept {n}");
            }
            for n in &r.dropped {
                println!("dropped {n}");
            }
            if r.installed.is_empty() && r.kept.is_empty() && r.dropped.is_empty() {
                println!("model declares no derived structures");
            }
            Ok(())
        }
        [sub, config] if sub == "sync-columnar" => {
            let db = crate::util::open_target(config)?;
            let r = db.sync_columnar()?;
            for (t, n) in &r.columnarized {
                println!("columnarized {t} ({n} columns)");
            }
            for t in &r.dropped {
                println!("dropped segments for {t} (row-oriented in the model)");
            }
            if r.columnarized.is_empty() && r.dropped.is_empty() {
                println!("the model marks no scan-heavy tables");
            }
            Ok(())
        }
        [sub, rest @ ..] if sub == "maintain" => {
            // model maintain <target> [--tail-fraction F] [--max N] [--plan]
            let plan_only = rest.iter().any(|a| a == "--plan");
            let mut fraction = 0.25f64;
            let mut max_rebuilds = 0usize;
            let mut config: Option<&str> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--plan" => {}
                    "--tail-fraction" => {
                        i += 1;
                        fraction = rest.get(i).and_then(|s| s.parse().ok()).ok_or_else(|| {
                            Failure::Usage("--tail-fraction needs a number".into())
                        })?;
                    }
                    "--max" => {
                        i += 1;
                        max_rebuilds = rest.get(i).and_then(|s| s.parse().ok()).ok_or_else(|| {
                            Failure::Usage("--max needs an integer".into())
                        })?;
                    }
                    other if config.is_none() => config = Some(other),
                    _ => return usage("model maintain <target> [--tail-fraction F] [--max N] [--plan]"),
                }
                i += 1;
            }
            let Some(config) = config else {
                return usage("model maintain <target> [--tail-fraction F] [--max N] [--plan]");
            };
            let db = crate::util::open_target(config)?;
            if plan_only {
                use mpedb::colseg::MaintainAction;
                let plan = db.columnar_maintenance_plan(fraction)?;
                if plan.is_empty() {
                    println!("nothing to do — every model-eligible table is fresh");
                }
                for m in &plan {
                    match &m.action {
                        MaintainAction::Build => println!("build   {}", m.table),
                        MaintainAction::Rebuild { covered, tail } => {
                            println!("rebuild {} (tail {tail} over {covered} covered)", m.table)
                        }
                        MaintainAction::Drop => println!("drop    {}", m.table),
                        MaintainAction::Fresh => {}
                    }
                }
                return Ok(());
            }
            let r = db.maintain_columnar(fraction, max_rebuilds)?;
            for (t, n) in &r.columnarized {
                println!("built {t} ({n} columns)");
            }
            for t in &r.dropped {
                println!("dropped segments for {t}");
            }
            if r.columnarized.is_empty() && r.dropped.is_empty() {
                println!("nothing to do — every model-eligible table is fresh");
            }
            Ok(())
        }
        _ => usage(
            "model needs: set <target> <model.toml> | show <target> | \
             sync-derived <target> | sync-columnar <target> | \
             maintain <target> [--tail-fraction F] [--max N] [--plan]",
        ),
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

/// `mpedb lens …` — reversible pairs over stored functions (DESIGN-RRETL, #52).
/// `define` VERIFIES a `bijective` declaration against the probe corpus before
/// anything is written, and refuses it with a named counter-example otherwise;
/// that refusal is the feature. The sample count is reported rather than a bare
/// "verified" because the evidence is statistical, not universal.
fn cmd_lens(args: &[String]) -> CliResult {
    use mpedb::lens::LensClass;
    match args {
        [sub, config, name, fwd, inv, rest @ ..] if sub == "define" => {
            let mut class = LensClass::Bijective;
            let mut rex: Option<&str> = None;
            let mut residual_type: Option<mpedb::ColumnType> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--class" => {
                        let v = it.next().ok_or_else(|| {
                            Failure::Usage("--class needs bijective|residual|lossy".into())
                        })?;
                        class = LensClass::parse(v)?;
                    }
                    "--rex" => {
                        rex = Some(it.next().ok_or_else(|| {
                            Failure::Usage("--rex needs a stored function name".into())
                        })?);
                    }
                    "--residual-type" => {
                        let v = it.next().ok_or_else(|| {
                            Failure::Usage(
                                "--residual-type needs a column type (int64, float64, text, \
                                 blob, bool, timestamp, any)"
                                    .into(),
                            )
                        })?;
                        residual_type = Some(mpedb::ColumnType::parse(v).ok_or_else(|| {
                            Failure::Usage(format!("unknown residual type `{v}`"))
                        })?);
                    }
                    other => return usage(format!("lens define: unexpected argument {other}")),
                }
            }
            let db = crate::util::open_target(config)?;
            match class {
                LensClass::Residual => {
                    let (Some(rex), Some(rt)) = (rex, residual_type) else {
                        return usage(
                            "class residual binds a TRIPLE and declares its residual: \
                             lens define <target> <name> <fwd> <inv> --class residual \
                             --rex <fn> --residual-type <type>",
                        );
                    };
                    let samples = db.create_residual_lens(name, fwd, rex, inv, rt)?;
                    println!(
                        "lens {name} registered as residual ({}): {fwd} \u{21c4} {inv} with \
                         rex {rex}, round trip held on {samples} probe inputs",
                        rt.name()
                    );
                }
                other_class => {
                    if rex.is_some() || residual_type.is_some() {
                        return usage(format!(
                            "--rex/--residual-type belong to --class residual, not {}",
                            other_class.as_str()
                        ));
                    }
                    let samples = db.create_lens(name, fwd, inv, other_class)?;
                    match other_class {
                        LensClass::Lossy => println!(
                            "lens {name} registered as lossy: NOT invertible, so the source \
                             must be kept"
                        ),
                        _ => println!(
                            "lens {name} registered as {}: {fwd} \u{21c4} {inv}, round trip \
                             held on {samples} probe inputs",
                            other_class.as_str()
                        ),
                    }
                }
            }
            Ok(())
        }
        [sub, config, name] if sub == "verify" => {
            let db = crate::util::open_target(config)?;
            let samples = db.verify_lens(name)?;
            println!("lens {name}: round trip held on {samples} probe inputs");
            Ok(())
        }
        [sub, config, name] if sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.drop_lens(name)? {
                println!("lens {name} dropped");
            } else {
                println!("no lens named {name}");
            }
            Ok(())
        }
        [sub, config] if sub == "list" => {
            let db = crate::util::open_target(config)?;
            let lenses = db.list_lenses()?;
            if lenses.is_empty() {
                println!("no lens pairs");
            }
            for l in lenses {
                let class = match l.residual_type {
                    Some(t) => format!("{} ({t})", l.class.as_str()),
                    None => l.class.as_str().to_string(),
                };
                print!(
                    "{}  {class}  {} samples{}\n    forward {}\n    inverse {}\n",
                    l.name,
                    l.samples,
                    if l.healthy { "" } else { "  [DEFINITION BLOB MISSING]" },
                    l.forward_hash,
                    l.inverse_hash,
                );
                if let Some(rex) = &l.rex_hash {
                    println!("    rex     {rex}");
                }
            }
            Ok(())
        }
        _ => usage(
            "lens needs: define <target> <name> <forward> <inverse> [--class bijective|lossy] \
             | verify <target> <name> | drop <target> <name> | list <target>",
        ),
    }
}

/// `mpedb rretl …` — apply a lens pair to a column in place, with the residuals
/// and lineage kept in the database (DESIGN-RRETL §7/§11). Apply verifies 100%
/// of rows against the source hash BEFORE the commit that destroys the source,
/// holds the writer lock for the whole run, and is an offline operation.
fn cmd_rretl(args: &[String]) -> CliResult {
    match args {
        [sub, config, pair, target] if sub == "apply" => {
            let Some((table, column)) = target.split_once('.') else {
                return usage("rretl apply needs <table>.<column>");
            };
            let db = crate::util::open_target(config)?;
            let r = db.rretl_apply(pair, table, column)?;
            println!(
                "rretl run {}: {} row(s) of {table}.{column} transformed in place, \
                 {} residual row(s) kept, 100% verified against the source hash \
                 before commit",
                r.run_id, r.rows, r.residuals
            );
            Ok(())
        }
        [sub, config, run] if sub == "revert" => {
            let run_id: i64 = run
                .parse()
                .map_err(|_| Failure::Usage(format!("rretl revert needs a run id, got `{run}`")))?;
            let db = crate::util::open_target(config)?;
            let r = db.rretl_revert(run_id)?;
            println!("rretl run {run_id} reverted: {} row(s) restored exactly", r.rows);
            Ok(())
        }
        [sub, config, run] if sub == "putback" => {
            let run_id: i64 = run
                .parse()
                .map_err(|_| Failure::Usage(format!("rretl putback needs a run id, got `{run}`")))?;
            let db = crate::util::open_target(config)?;
            let r = db.rretl_putback(run_id)?;
            println!(
                "rretl run {run_id} putback: {} row(s) inverted WITH their edits kept,                  {} residual(s) re-attached; deleted rows stayed deleted",
                r.rows, r.residuals
            );
            Ok(())
        }
        [sub, config] if sub == "fsck" => {
            let db = crate::util::open_target(config)?;
            let findings = db.rretl_fsck()?;
            if findings.is_empty() {
                println!("rretl fsck: clean — every standing run verifies");
            } else {
                for f in &findings {
                    println!("FINDING: {f}");
                }
                return Err(Failure::Runtime(format!(
                    "rretl fsck: {} finding(s)",
                    findings.len()
                )));
            }
            Ok(())
        }
        [sub, config] if sub == "log" => {
            let db = crate::util::open_target(config)?;
            let log = db.rretl_log()?;
            if log.is_empty() {
                println!("no rretl runs");
            }
            for l in log {
                let err = if l.error.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", l.error)
                };
                println!(
                    "run {}  {}  {}.{}  {} row(s)  {}{err}",
                    l.run_id, l.lens, l.table, l.column, l.rows, l.outcome
                );
            }
            Ok(())
        }
        [sub, config, obj, file] if sub == "put" => {
            let bytes = std::fs::read(file)
                .map_err(|e| Failure::Runtime(format!("cannot read `{file}`: {e}")))?;
            let db = crate::util::open_target(config)?;
            let ver = db.rretl_put_version(obj, &bytes)?;
            println!("rretl put: `{obj}` is now at version {ver} ({} bytes)", bytes.len());
            Ok(())
        }
        [sub, config, obj, ver, out] if sub == "get" => {
            let ver: i64 = ver
                .parse()
                .map_err(|_| Failure::Usage(format!("rretl get needs a version, got `{ver}`")))?;
            let db = crate::util::open_target(config)?;
            let bytes = db.rretl_get_version(obj, ver)?;
            std::fs::write(out, &bytes)
                .map_err(|e| Failure::Runtime(format!("cannot write `{out}`: {e}")))?;
            println!("rretl get: `{obj}` version {ver} → {out} ({} bytes, hash-verified)", bytes.len());
            Ok(())
        }
        [sub, config, obj, keep] if sub == "prune" => {
            let keep: u64 = keep.parse().map_err(|_| {
                Failure::Usage(format!("rretl prune needs a keep-count, got `{keep}`"))
            })?;
            let db = crate::util::open_target(config)?;
            let n = db.rretl_prune_versions(obj, keep)?;
            println!(
                "rretl prune: {n} old version(s) of `{obj}` deleted, newest {keep} kept \
                 (recorded in the lineage)"
            );
            Ok(())
        }
        [sub, config, obj] if sub == "versions" => {
            let db = crate::util::open_target(config)?;
            let vers = db.rretl_versions(obj)?;
            if vers.is_empty() {
                println!("no versions of `{obj}`");
            }
            for v in vers {
                println!("ver {}  {}  {} byte(s)  {}", v.ver, v.stored_as, v.bytes, v.content_hash);
            }
            Ok(())
        }
        [sub, config, name, file] if sub == "pack-in" => {
            let bytes = std::fs::read(file)
                .map_err(|e| Failure::Runtime(format!("cannot read `{file}`: {e}")))?;
            let db = crate::util::open_target(config)?;
            let id = db.rretl_pack_in(name, &bytes)?;
            println!(
                "rretl pack-in: `{name}` is archive {id}; reconstruction verified \
                 byte-identical before commit"
            );
            Ok(())
        }
        [sub, config, id, out] if sub == "pack-out" => {
            let id: i64 = id
                .parse()
                .map_err(|_| Failure::Usage(format!("rretl pack-out needs an archive id, got `{id}`")))?;
            let db = crate::util::open_target(config)?;
            let bytes = db.rretl_pack_out(id)?;
            std::fs::write(out, &bytes)
                .map_err(|e| Failure::Runtime(format!("cannot write `{out}`: {e}")))?;
            println!("rretl pack-out: archive {id} → {out} ({} bytes, hash-verified)", bytes.len());
            Ok(())
        }
        [m, sub, config, file] if m == "map" && sub == "define" => {
            let toml_text = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("cannot read `{file}`: {e}")))?;
            let db = crate::util::open_target(config)?;
            db.rretl_map_define(&toml_text)?;
            println!("rretl map defined (sources, identities and pairs all validated)");
            Ok(())
        }
        [m, sub, config, name] if m == "map" && sub == "sync" => {
            let db = crate::util::open_target(config)?;
            let r = db.rretl_map_sync(name)?;
            println!(
                "rretl map `{name}` synced (run {}): a→b {}, b→a {}, +b {}, +a {}, \
                 -a {}, -b {}, clean {}",
                r.run_id,
                r.a_to_b,
                r.b_to_a,
                r.created_b,
                r.created_a,
                r.deleted_a,
                r.deleted_b,
                r.unchanged
            );
            Ok(())
        }
        [m, sub, config, name, rest @ ..] if m == "map" && sub == "run" => {
            let p = crate::args::parse(rest, &["max-secs", "max-rows", "runner", "lease-secs"], &[])?;
            let opts = mpedb::rretl_map_run::RunOptions {
                max_secs: p.u64_opt("max-secs")?,
                max_rows: p.u64_opt("max-rows")?,
                runner: p.value("runner").map(str::to_string),
                lease_secs: p.u64_opt("lease-secs")?,
            };
            let db = crate::util::open_target(config)?;
            let r = db.rretl_map_run(name, &opts)?;
            println!("rretl map `{name}` run: {}", r.note());
            for c in &r.conflict_notes {
                println!("blocker: {c}");
            }
            if r.conflicts as usize > r.conflict_notes.len() {
                println!(
                    "…and {} more conflict(s) — `map check` names them all",
                    r.conflicts as usize - r.conflict_notes.len()
                );
            }
            Ok(())
        }
        [m, sub, config, name, runner] if m == "map" && sub == "runner" => {
            let db = crate::util::open_target(config)?;
            db.rretl_map_set_runner(name, runner)?;
            if runner.is_empty() {
                println!("rretl map `{name}`: runner restriction cleared — any process may run it");
            } else {
                println!("rretl map `{name}`: only runner `{runner}` may run it from now on");
            }
            Ok(())
        }
        [m, sub, config, name] if m == "map" && sub == "status" => {
            let db = crate::util::open_target(config)?;
            let st = db.rretl_map_status(name)?;
            println!(
                "round {}  runner {}  {}",
                st.round,
                if st.runner.is_empty() { "<any>" } else { &st.runner },
                if st.note.is_empty() { "-" } else { &st.note }
            );
            if !st.lease_owner.is_empty() {
                println!("lease held by `{}` until {} (micros)", st.lease_owner, st.lease_until);
            }
            for (tbl, phase) in &st.in_progress {
                println!("mid-round: {tbl} in pass {phase}");
            }
            if st.in_progress.is_empty() {
                println!("no round in progress — the next run starts a fresh one");
            }
            Ok(())
        }
        [m, sub, config, name] if m == "map" && sub == "check" => {
            let db = crate::util::open_target(config)?;
            let r = db.rretl_map_check(name)?;
            for t in &r.tables {
                println!(
                    "{} -> {}: pending a→b {}, b→a {}, +b {}, +a {}, -a {}, -b {}, \
                     adopt {}, clean {}, orphans {}",
                    t.src,
                    t.dst,
                    t.pending_a2b,
                    t.pending_b2a,
                    t.would_create_b,
                    t.would_create_a,
                    t.would_delete_a,
                    t.would_delete_b,
                    t.would_adopt,
                    t.unchanged,
                    t.orphan_state
                );
                for c in &t.conflicts {
                    println!("blocker: {c}");
                }
                for d in &t.diverged {
                    println!("DIVERGED: {d}");
                }
            }
            let breaches = r.breaches().len();
            if breaches > 0 {
                // The check is not one snapshot across its reads, so under a
                // live sync a breach can be a cross-snapshot artifact rather
                // than a standing fact. Say so: at quiesce it is exact, and
                // that is when the exit code should be believed.
                return Err(Failure::Runtime(format!(
                    "rretl map check `{name}`: {breaches} breach(es) — a sync would abort, \
                     or divergence is standing. Exact when nothing is writing; re-run at \
                     quiesce before acting on this"
                )));
            }
            Ok(())
        }
        [m, sub, config, name] if m == "map" && sub == "show" => {
            let db = crate::util::open_target(config)?;
            print!("{}", db.rretl_map_show(name)?);
            Ok(())
        }
        [m, sub, config, name] if m == "map" && sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.rretl_map_drop(name)? {
                println!("rretl map `{name}` dropped (its sync state rows remain)");
            } else {
                println!("no rretl map named `{name}`");
            }
            Ok(())
        }
        [m, sub, config] if m == "map" && sub == "list" => {
            let db = crate::util::open_target(config)?;
            let maps = db.rretl_maps()?;
            if maps.is_empty() {
                println!("no rretl maps");
            }
            for m in maps {
                println!("{m}");
            }
            Ok(())
        }
        [sub, config] if sub == "archives" => {
            let db = crate::util::open_target(config)?;
            let arches = db.rretl_archives()?;
            if arches.is_empty() {
                println!("no archives");
            }
            for a in arches {
                println!(
                    "archive {}  {}  {} member(s)  {}",
                    a.archive_id, a.name, a.members, a.content_hash
                );
            }
            Ok(())
        }
        _ => usage(
            "rretl needs: apply <target> <pair> <table>.<column> | revert <target> <run_id> \
             | putback <target> <run_id> | fsck <target> | log <target> \
             | put <target> <obj> <file> | get <target> <obj> <ver> <out-file> \
             | versions <target> <obj> | prune <target> <obj> <keep> \
             | pack-in <target> <name> <zip-file> \
             | pack-out <target> <archive_id> <out-file> | archives <target> \
             | map define <target> <map.toml> | map sync|check|show|drop <target> <name> \
             | map run <target> <name> [--max-secs S] [--max-rows N] [--runner ID] \
             | map runner <target> <name> <id> | map status <target> <name> \
             | map list <target>",
        ),
    }
}

/// `mpedb tune set <target> name=value | show <target>` — the cost
/// calculator's stored switches (stage M5). Stored IN the file so every
/// attached process prices identically; changes bump the schema generation.
fn cmd_tune(args: &[String]) -> CliResult {
    match args {
        [sub, config, assignment] if sub == "set" => {
            let db = crate::util::open_target(config)?;
            let t = db.set_tunable(assignment)?;
            println!(
                "tunables: ndv_discount={} recursive_triggers={}",
                t.ndv_discount, t.recursive_triggers
            );
            Ok(())
        }
        [sub, config] if sub == "show" => {
            let db = crate::util::open_target(config)?;
            let t = db.tunables()?;
            println!("ndv_discount={}", t.ndv_discount);
            println!("recursive_triggers={}", t.recursive_triggers);
            Ok(())
        }
        _ => usage("tune needs: set <target> name=value | show <target>"),
    }
}

/// `mpedb trigger backtest <target> <name|CREATE TRIGGER …> [limit]` — replay
/// a trigger (stored, or a not-yet-created CREATE TRIGGER statement) against
/// the current rows in an always-rolled-back transaction and report what it
/// would have done; `list` shows the stored triggers.
fn cmd_trigger(args: &[String]) -> CliResult {
    match args {
        [sub, config, what, rest @ ..] if sub == "backtest" && rest.len() <= 1 => {
            let limit = match rest {
                [l] => l
                    .parse::<u64>()
                    .map_err(|_| Failure::Runtime(format!("limit must be a number, got `{l}`")))?,
                _ => 0,
            };
            let db = crate::util::open_target(config)?;
            let report = db.backtest_trigger(what, limit)?;
            println!("{report}");
            Ok(())
        }
        [sub, config] if sub == "list" => {
            let db = crate::util::open_target(config)?;
            let trgs = db.list_triggers()?;
            if trgs.is_empty() {
                println!("no triggers");
            }
            for (name, table, sql) in trgs {
                println!("{name} ON {table}: {sql}");
            }
            Ok(())
        }
        _ => usage(
            "trigger needs: backtest <target> <name|'CREATE TRIGGER …'> [limit] | list <target>",
        ),
    }
}

/// `mpedb cost-policy set <target> <file.py|rs> | drop <target>` — the
/// PROGRAMMABLE cost adjustment (stage M5): a stored PySpell
/// `def policy(kind, table, index_no, bucket, rows_bucket, archetype):`
/// running at prepare inside the cost seam, identical in every process.
fn cmd_cost_policy(args: &[String]) -> CliResult {
    use mpedb::spellfn::SpellLang;
    match args {
        [sub, config, file] if sub == "set" => {
            let lang = if file.ends_with(".rs") { SpellLang::Rust } else { SpellLang::Python };
            let src = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
            let db = crate::util::open_target(config)?;
            let hash = db.set_cost_policy(lang, &src)?;
            println!("cost policy stored as {hash}");
            Ok(())
        }
        [sub, config] if sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.drop_cost_policy()? {
                println!("cost policy dropped");
            } else {
                println!("no cost policy set");
            }
            Ok(())
        }
        _ => usage("cost-policy needs: set <target> <file.py|rs> | drop <target>"),
    }
}

/// `mpedb stats <target>` — the READ side of the cost layer: what the engine
/// believes (rows, buckets, NDV/analyze state) per index.
fn cmd_stats(args: &[String]) -> CliResult {
    let [config] = args else { return usage("stats needs <config.toml|db.mpedb>") };
    let db = crate::util::open_target(config)?;
    let lines = db.stats_report()?;
    if lines.is_empty() {
        println!("no secondary indexes — nothing to report");
        return Ok(());
    }
    println!("{:<28} {:>4} {:>12} {:>6} {:>6}", "index", "no", "rows", "2^", "ndv2^");
    for l in lines {
        println!(
            "{:<28} {:>4} {:>12} {:>6} {:>6}",
            format!("{}({})", l.table, l.columns.join(",")),
            l.index_no,
            l.rows,
            l.rows_bucket,
            l.ndv_bucket.map(|b| b.to_string()).unwrap_or_else(|| "—".into())
        );
    }
    println!("
`ndv2^ = —` means analyze() has not run (or DDL made it stale): `mpedb exec <t> 'ANALYZE'`-equivalent is `Database::analyze()`.");
    Ok(())
}

/// `mpedb op define <target> <sym> <infix|postfix|prefix|niladic> <file.py|rs> [doc]
/// | drop <target> <sym> | list <target> | install-model <target>` — custom
/// `:sym:` operators (stage M3, SQL-EXTENSIONS.md).
fn cmd_op(args: &[String]) -> CliResult {
    use mpedb::opdef::OpFixity;
    use mpedb::spellfn::SpellLang;
    let fixity_of = |s: &str| -> Result<OpFixity, Failure> {
        Ok(match s {
            "infix" | "11" => OpFixity::Infix,
            "postfix" | "10" => OpFixity::Postfix,
            "prefix" | "01" => OpFixity::Prefix,
            "niladic" | "00" => OpFixity::Niladic,
            "statement" | "100" => OpFixity::Statement,
            other => {
                return Err(Failure::Usage(format!(
                    "unknown fixity `{other}` — infix (11), postfix (10), prefix (01), niladic (00), statement (100)"
                )))
            }
        })
    };
    match args {
        [sub, config, sym, fixity, file, doc @ ..] if sub == "define" => {
            let fixity = fixity_of(fixity)?;
            let lang = if file.ends_with(".rs") { SpellLang::Rust } else { SpellLang::Python };
            let src = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
            let db = crate::util::open_target(config)?;
            let hash = db.create_operator(sym, fixity, lang, &src, &doc.join(" "))?;
            println!("operator :{sym}: ({}) stored as {hash}", fixity.name());
            Ok(())
        }
        [sub, config, sym] if sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.drop_operator(sym)? {
                println!("operator :{sym}: dropped");
            } else {
                println!("no operator :{sym}:");
            }
            Ok(())
        }
        [sub, config] if sub == "list" => {
            let db = crate::util::open_target(config)?;
            let ops = db.list_operators()?;
            if ops.is_empty() {
                println!("no custom operators — see SQL-EXTENSIONS.md");
            }
            for o in ops {
                println!(":{}:  {:<8} {}  {}", o.symbol, o.fixity.name(), &o.spell_hash_hex[..12], o.doc);
            }
            Ok(())
        }
        [sub, config] if sub == "install-model" => {
            let db = crate::util::open_target(config)?;
            let installed = db.install_model_operators()?;
            println!(
                "installed from the model: {}",
                installed.iter().map(|s| format!(":{s}:")).collect::<Vec<_>>().join(", ")
            );
            Ok(())
        }
        _ => usage(
            "op needs: define <target> <sym> <fixity> <file.py|rs> [doc] | drop <target> <sym>              | list <target> | install-model <target>",
        ),
    }
}

/// `mpedb advise <target> [statements.sql | --model <model.toml|stored>]` —
/// the #118 workload-index advisor, recommend-only. With no source the
/// workload is the plan registry: everything this database has ever compiled.
fn cmd_advise(args: &[String]) -> CliResult {
    use mpedb::advisor::WorkloadSource;
    // Split boolean flags from positional args so `--columnar`/`--emit-model`
    // may appear anywhere.
    let columnar = args.iter().any(|a| a == "--columnar");
    let emit_model = args.iter().any(|a| a == "--emit-model");
    let pos: Vec<&String> = args
        .iter()
        .filter(|a| *a != "--columnar" && *a != "--emit-model")
        .collect();
    let (config, source) = match pos.as_slice() {
        [config] => (*config, None),
        [config, flag, spec] if *flag == "--model" => (*config, Some((true, (*spec).clone()))),
        [config, file] => (*config, Some((false, (*file).clone()))),
        _ => {
            return usage(
                "advise needs <config.toml|db.mpedb> [statements.sql | --model <file|stored>] \
                 [--columnar [--emit-model]]",
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
    if columnar {
        let rep = db.recommend_columnar(source)?;
        if emit_model {
            print!("{}", rep.to_model_toml());
            return Ok(());
        }
        println!(
            "workload: {} compiled, {} uncompilable, {} shapes without a single-table storage signal",
            rep.compiled, rep.uncompilable, rep.skipped_shape
        );
        if rep.advices.is_empty() {
            println!("no columnar recommendations.");
            return Ok(());
        }
        println!();
        println!("{:<24} {:>7} {:>6} {:>6}  columns", "table", "orient", "scan", "point");
        for a in &rep.advices {
            println!(
                "{:<24} {:>7} {:>6} {:>6}  {}",
                a.table,
                match a.orient {
                    mpedb::advisor::Orient::Column => "column",
                    mpedb::advisor::Orient::Row => "row",
                },
                a.scan_weight,
                a.point_weight,
                if a.scan_columns.is_empty() { "*".into() } else { a.scan_columns.join(", ") },
            );
        }
        println!();
        println!(
            "apply with `--emit-model` → `mpedb model set`, then `mpedb model sync-columnar`."
        );
        return Ok(());
    }
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

/// `mpedb ingest …` — declare an external source, see what the receipts
/// learned about it, and get the plan. The FETCHING is the user's code;
/// mpedb plans, receives and verifies.
fn cmd_ingest(args: &[String]) -> CliResult {
    match args {
        [sub, config, file] if sub == "define" => {
            let toml_text = std::fs::read_to_string(file)
                .map_err(|e| Failure::Runtime(format!("cannot read `{file}`: {e}")))?;
            let db = crate::util::open_target(config)?;
            db.ingest_define(&toml_text)?;
            println!("ingest source defined (tables, edges, parents and budgets all validated)");
            Ok(())
        }
        [sub, config, name] if sub == "show" => {
            let db = crate::util::open_target(config)?;
            print!("{}", db.ingest_show(name)?);
            Ok(())
        }
        [sub, config] if sub == "list" => {
            let db = crate::util::open_target(config)?;
            let names = db.ingest_sources()?;
            if names.is_empty() {
                println!("no ingest sources");
            }
            for n in names {
                println!("{n}");
            }
            Ok(())
        }
        [sub, config, name] if sub == "drop" => {
            let db = crate::util::open_target(config)?;
            if db.ingest_drop(name)? {
                println!("ingest source `{name}` dropped, with its observations");
            } else {
                println!("no ingest source `{name}`");
            }
            Ok(())
        }
        [sub, config, name] if sub == "state" => {
            let db = crate::util::open_target(config)?;
            for (edge, st, overlap) in db.ingest_state(name)? {
                println!(
                    "{edge:<20} cursor {:<8} {:<7} caught {:<5} missed {:<5} receipts {}/{} \
                     fan-out {:.1} overlap {overlap}s",
                    if st.cursor_col.is_empty() { "-" } else { &st.cursor_col },
                    st.cursor_state,
                    st.caught,
                    st.missed,
                    st.changed_receipts,
                    st.receipts,
                    st.fanout_per_call(),
                );
                if st.missed > 0 {
                    println!(
                        "  ^ that cursor would have LOST {} changed row(s) — the dump is \
                         carrying this table",
                        st.missed
                    );
                }
            }
            Ok(())
        }
        [sub, config, name, rest @ ..] if sub == "advise" => {
            let emit = rest.iter().any(|a| a == "--emit-cron");
            let flags: Vec<String> =
                rest.iter().filter(|a| *a != "--emit-cron").cloned().collect();
            let p = crate::args::parse(&flags, &["cmd"], &[])?;
            let db = crate::util::open_target(config)?;
            let plan = db.ingest_advise(name)?;
            if emit {
                for line in plan.cron(p.value("cmd").unwrap_or("./fetch.py")) {
                    println!("{line}");
                }
            } else {
                for line in mpedb::ingest_plan::render(&plan) {
                    println!("{line}");
                }
            }
            Ok(())
        }
        [sub, config, name] if sub == "next" => {
            let db = crate::util::open_target(config)?;
            match db.ingest_next(name)? {
                // One line per key, lease first: a shell fetcher can cut it.
                Some(t) => {
                    for k in &t.keys {
                        println!("{}\t{}\t{}\t{}", t.lease, t.edge, t.table, crate::render::value_str(k));
                    }
                }
                None => {
                    let b = db.ingest_budget_left(name)?;
                    println!(
                        "no derived call to make: {} call(s) left in profile `{}` over {} s",
                        b.calls, b.profile, b.window_secs
                    );
                }
            }
            Ok(())
        }
        [sub, config, name, rest @ ..] if sub == "done" || sub == "release" => {
            let p = crate::args::parse(rest, &["lease"], &[])?;
            let lease: i64 = p
                .require("lease")?
                .parse()
                .map_err(|_| Failure::Usage("--lease takes the number `ingest next` printed".into()))?;
            let db = crate::util::open_target(config)?;
            let n = if sub == "done" {
                db.ingest_done(name, lease)?
            } else {
                db.ingest_release(name, lease)?
            };
            println!("ingest `{name}`: {n} key(s) {}", if sub == "done" { "retired" } else { "returned to the queue" });
            Ok(())
        }
        [sub, config, name, rest @ ..] if sub == "reap" => {
            let p = crate::args::parse(rest, &["older-than"], &[])?;
            let secs: i64 = p.value("older-than").unwrap_or("900").parse().map_err(|_| {
                Failure::Usage("--older-than takes seconds".into())
            })?;
            let db = crate::util::open_target(config)?;
            let n = mpedb::ingest_task::reap_leases(&db, name, secs)?;
            println!("ingest `{name}`: {n} lease(s) reclaimed");
            Ok(())
        }
        [sub, config, name] if sub == "pending" => {
            let db = crate::util::open_target(config)?;
            let ps = db.ingest_pending(name)?;
            if ps.is_empty() {
                println!("no derived work waiting");
            }
            for (edge, n, leased) in ps {
                println!("{edge:<20} {n:>6} waiting{}", if leased > 0 { ", some leased" } else { "" });
            }
            let b = db.ingest_budget_left(name)?;
            println!(
                "budget: {} call(s) left in profile `{}` over {} s",
                b.calls, b.profile, b.window_secs
            );
            Ok(())
        }
        [sub, config, name] if sub == "conflicts" => {
            let db = crate::util::open_target(config)?;
            let cs = db.ingest_conflicts(name)?;
            for c in &cs {
                println!("{:<16} {:?}  {}  {}", c.table, c.key, c.kind, c.detail);
            }
            if cs.is_empty() {
                println!("no ingest conflicts");
                return Ok(());
            }
            Err(Failure::Runtime(format!(
                "ingest `{name}`: {} unresolved conflict(s) — the policy would not decide them",
                cs.len()
            )))
        }
        [sub, config, name, rest @ ..] if sub == "resolve" => {
            let p = crate::args::parse(rest, &["take"], &[])?;
            let db = crate::util::open_target(config)?;
            let n = db.ingest_resolve(name, p.require("take")?)?;
            println!("ingest `{name}`: {n} conflict(s) cleared");
            Ok(())
        }
        _ => usage(
            "ingest needs: define <target> <source.toml> | show|drop <target> <name> \
             | list <target> | state <target> <source> \
             | advise <target> <source> [--emit-cron] [--cmd <script>] \
             | conflicts <target> <source> | resolve <target> <source> --take local \
             | next|pending <target> <source> | done|release <target> <source> --lease <n> \
             | reap <target> <source> [--older-than 900]",
        ),
    }
}
