//! `mpedb mirror …` — bidirectional sqlite/PostgreSQL ⇄ mpedb mirroring
//! (design/DESIGN-MIRROR.md). This stage (M2.4) wires the sqlite import and a status
//! read; pull/push/switch land in later milestones.

use std::path::PathBuf;

use mpedb::Database;
use mpedb_core::Engine;
use mpedb_mirror::state::{self, Authority};
use mpedb_mirror::switch::{
    drain_pull, drain_push, read_epoch, recover, switch_to_mpedb, switch_to_source, Recovered,
};
use mpedb_mirror::sourcecfg::{self, SourceSpec};
use mpedb_mirror::{
    diff_sqlite_data, export_sqlite, import_pg, import_sqlite, reconcile, verify, ImportOptions,
    PgAdapter, SourceAdapter, SqliteAdapter,
};
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
        "export" => cmd_export(rest),
        "roundtrip" => cmd_roundtrip(rest),
        "status" => cmd_status(rest),
        "preflight" => cmd_preflight(rest),
        "pull" => cmd_pull(rest),
        "push" => cmd_push(rest),
        "sync" => cmd_sync(rest),
        "verify" => cmd_verify(rest),
        "reconcile" => cmd_reconcile(rest),
        "switch" => cmd_switch(rest),
        "regenerate" => cmd_regenerate(rest),
        "unfreeze" => cmd_unfreeze(rest),
        "conflicts" => cmd_conflicts(rest),
        "resolve" => cmd_resolve(rest),
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        other => usage(format!("unknown mirror subcommand `{other}`\n\n{HELP}")),
    }
}

/// Work out which source a command should talk to, in precedence order:
///
/// 1. `--source-config <path>` — the §12 channel: a 0600 file holding the DSN.
/// 2. the `mir/src` record — the path recorded at import, so day-to-day
///    commands need only `--db`.
/// 3. `--source <sqlite-file>` — a sqlite path is not a secret, so it stays a
///    plain flag; this is what every existing sqlite invocation uses.
///
/// There is deliberately no `--dsn`: `ps` shows every process's argv to every
/// user on the host, so a DSN flag would publish the source password (§12).
fn resolve_spec(db: &Database, p: &args::Parsed) -> Result<SourceSpec, Failure> {
    if let Some(cfg) = p.value("source-config") {
        return Ok(sourcecfg::load(std::path::Path::new(cfg))?);
    }
    if let Some(bytes) = db.sys_record_get(state::MIR_NS, state::KEY_SRC)? {
        let path = state::decode_src_path(&bytes)?;
        return sourcecfg::load(std::path::Path::new(&path)).map_err(|e| {
            Failure::Runtime(format!(
                "this mirror's source-config is `{path}` (recorded at import) but it \
                 could not be read: {e}\nPass --source-config <path> if it moved."
            ))
        });
    }
    if let Some(src) = p.value("source") {
        return Ok(SourceSpec::Sqlite { path: src.into() });
    }
    Err(Failure::Usage(
        "no source: pass --source <sqlite-file>, or --source-config <0600-file> for \
         PostgreSQL (a DSN must never be a CLI arg — it would be visible in `ps`)"
            .into(),
    ))
}

/// Open an existing mirror `.mpedb` (config-free) and its source adapter with
/// tracking installed (idempotent).
///
/// This is every both-sides command's single entry point, so it is also where
/// **recovery-on-attach** (§7) runs: a switch is two fenced commits — the source
/// CAS and the mpedb commit — and a SIGKILL between them leaves a half-cutover
/// that only the pair `(mpedb epoch, source epoch)` can disambiguate. Doing it
/// here means no command can act on a half-switched mirror — and, since the PG
/// path now comes through here too, PG gets that same guarantee rather than a
/// second copy of the rule that can drift out of step.
fn open_source(
    db_path: &str,
    p: &args::Parsed,
) -> Result<(Database, Box<dyn SourceAdapter>), Failure> {
    let db = Database::open_from_file(std::path::Path::new(db_path))?;
    let spec = resolve_spec(&db, p)?;
    let mut adapter: Box<dyn SourceAdapter> = match &spec {
        SourceSpec::Sqlite { path } => {
            let conn = Connection::open(path)
                .map_err(|e| Failure::Runtime(format!("open sqlite `{path}`: {e}")))?;
            let a = SqliteAdapter::new(conn, None, &[])?;
            a.install_triggers()?;
            Box::new(a)
        }
        SourceSpec::Postgres { dsn } => {
            let mut a = PgAdapter::connect(dsn, None, &[])?;
            a.install_triggers()?;
            Box::new(a)
        }
    };
    match recover(&db, adapter.as_mut())? {
        Recovered::Steady => {}
        r => println!("recovery-on-attach: an interrupted switch was completed ({r:?})"),
    }
    Ok((db, adapter))
}

const HELP: &str = "\
mpedb mirror <subcommand>

  import  (--source <sqlite-file> | --source-config <file>) --dest <new-mpedb-file>
          [--include t1,t2] [--exclude t3] [--size_mb N] [--durability none|commit|wal]
          [--adapt exact|lossy]
      Import a sqlite or PostgreSQL database into a NEW .mpedb mirror file.
      sqlite's declared types are affinities, not constraints, so a column may
      hold off-type values. By DEFAULT one of those fails the import (loudly,
      while you are watching). --adapt exact coerces only what parses WHOLLY and
      losslessly ('yes' -> true, ISO-8601 -> timestamp); --adapt lossy also
      allows coercions that discard something (truncation, rounding). Neither
      ever prefix-parses: '007abc' is refused, never turned into 7. Every
      coercion applied is printed.

  export  --db <mpedb-file> --dest <new-sqlite-file>
          --db <mpedb-file> --to postgres --source-config <file> [--pg-schema S]
      Export a mirror out to a fresh sqlite database, or into PostgreSQL.
      The PostgreSQL export recreates the ORIGINAL declared types recorded at
      import (int4 stays int4, varchar(20) stays varchar(20)) rather than the
      widened mpedb types, and runs preflight first: if any value would be
      rejected by the target schema it refuses to start, instead of failing
      halfway through the load.



SOURCE SELECTION
  A sqlite source is a path: --source app.sqlite.
  A PostgreSQL source needs a DSN, which is a password -- so it is NEVER a flag
  (`ps` shows every process's argv to every user on the host). Put it in a 0600
  file and name the file:

      $ install -m600 /dev/null pg.toml     # born 0600, before a secret is in it
      $ $EDITOR pg.toml
            kind = \"postgres\"
            dsn  = \"host=db.internal dbname=app user=app password=s3cr3t\"
      $ mpedb mirror import --source-config pg.toml --dest app.mpedb

  import records that file's PATH in the mirror (never the DSN), so afterwards
  --db alone is enough:

      $ mpedb mirror sync --db app.mpedb

  The file's mode and owner are re-checked on every read: if it ever becomes
  readable by group or other, commands refuse to run rather than use it.

  roundtrip --source <sqlite-file> [--size_mb N]
      Differential self-test: source -> mpedb -> sqlite, then diff the data
      against the source. Reports any rows that did not survive the mapping.

  status  --db <mpedb-file>
      Show a mirror's config and authority state.

  preflight --db <mpedb-file>
      Check the data against the source schema recorded at import, WITHOUT
      contacting the source: reports every value a strict target would reject
      (int4 overflow, varchar length, numeric precision, NULL in NOT NULL, NUL
      in text, loose-source type drift) and every column that already lost
      information at import. Read-only, no round-trips.

  pull    --db <mpedb-file> [--source <sqlite-file> | --source-config <file>]
      Pull + apply source changes into mpedb until caught up.

  push    --db <mpedb-file> [--source <sqlite-file> | --source-config <file>]
      Push local mpedb changes back to the source.

  sync    --db <mpedb-file> [--source <sqlite-file> | --source-config <file>]
      Pull then push (respecting which side is authoritative).

  verify  --source <sqlite-file> --db <mpedb-file>
      Report whether mpedb and the source are byte-identical.

  reconcile --db <mpedb-file> [--source <sqlite-file> | --source-config <file>]
      Full merge-diff: converge mpedb to the source (source-wins).

  switch  --db <mpedb-file> --to mpedb|source [--source <f> | --source-config <f>]
      Move authority to the given side (epoch-fenced).

  regenerate --db <mpedb-file> [--size_mb N] [--source <f> | --source-config <f>]
             [--no-drain]
      Rebuild the mirror into a fresh file of --size_mb, carrying its whole
      identity across: rows, cursor, epoch, type provenance, parked conflicts,
      and the UN-PUSHED local changes still waiting to reach the source.
      This is the only way out of a full mirror (the geometry is fixed at
      create time, so a full file cannot grow), and the only way to follow a
      source that gained a table or drifted its schema -- mpedb has no ALTER
      or CREATE TABLE by design (DESIGN-MIRROR §7).
      Pushes to the source first when one is reachable; --no-drain skips that.
      Crash-safe: the old file is untouched until an atomic rename, and a
      half-done rebuild leaves the mirror FROZEN rather than writable -- run
      `unfreeze` after checking it.

  unfreeze --db <mpedb-file>
      Clear a stuck freeze (e.g. after a switch-to-source verify failure);
      reconcile first, then retry the switch.

  conflicts --db <mpedb-file> [--clear]
      List parked conflicts (rows that could not be applied), or --clear them.

  resolve --db <mpedb-file> --take source|local [--source <f> | --source-config <f>]
      Override every parked conflict: `source` converges mpedb to the current
      source row; `local` forces mpedb's row onto the source.";

fn cmd_conflicts(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db"], &["clear"])?;
    let db = Database::open_from_file(std::path::Path::new(p.require("db")?))?;
    if p.has("clear") {
        let n = mpedb_mirror::conflicts::clear(&db)?;
        println!("cleared {n} parked conflict(s)");
        return Ok(());
    }
    let parked = mpedb_mirror::conflicts::list(&db)?;
    if parked.is_empty() {
        println!("no parked conflicts");
        return Ok(());
    }
    println!("{} parked conflict(s):", parked.len());
    for c in &parked {
        println!("  {:<14?} {}.pk={:?}", c.record.kind, c.table, c.pk);
    }
    Ok(())
}

fn cmd_resolve(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "take", "source-config"], &[])?;
    let take = match p.require("take")? {
        "source" => mpedb_mirror::Take::Source,
        "local" => mpedb_mirror::Take::Local,
        other => return usage(format!("resolve --take must be source|local, not `{other}`")),
    };
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    let s = mpedb_mirror::resolve(&db, a.as_mut(), take)?;
    println!(
        "resolved: {} took-source, {} took-local, {} still parked",
        s.took_source, s.took_local, s.still_parked
    );
    Ok(())
}

fn cmd_regenerate(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db", "size_mb", "source", "source-config"], &["no-drain"])?;
    let db_path = PathBuf::from(p.require("db")?);
    let size_mb = p.value("size_mb").unwrap_or("256");
    let size_mb: u64 = size_mb
        .parse()
        .map_err(|_| Failure::Usage("--size_mb must be an integer".into()))?;

    // §7 step 2: drain-push first WHEN REACHABLE. It is not required — the
    // un-pushed dirty set travels either way — but pushing first means the
    // source is already current if the rebuild goes wrong, so it is the
    // default. A db_full escape must not depend on the source being up, hence
    // --no-drain and the tolerated failure below.
    if !p.has("no-drain") && (p.value("source").is_some() || p.value("source-config").is_some()) {
        match open_source(p.require("db")?, &p) {
            Ok((db, mut a)) => match drain_push(&db, a.as_mut()) {
                Ok(s) => println!("drained {} change(s) to the source first", s.upserts + s.deletes),
                Err(e) => println!("could not drain to the source ({e}); regenerating anyway — \
                                    the un-pushed changes travel with the rebuild"),
            },
            Err(e) => println!("could not reach the source ({e:?}); regenerating anyway"),
        }
    }

    let rep = mpedb_mirror::regenerate(&db_path, size_mb * 1024 * 1024)?;
    println!(
        "regenerated {} at {} MB",
        db_path.display(),
        rep.new_size_bytes / (1024 * 1024)
    );
    for (name, n) in &rep.tables {
        println!("  {name:<24} {n:>10} rows");
    }
    println!(
        "  total: {} rows, {} mirror record(s), {} un-pushed local change(s) carried",
        rep.total_rows(),
        rep.mir_records,
        rep.dirty_entries
    );
    Ok(())
}

fn cmd_unfreeze(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db"], &[])?;
    let db = Database::open_from_file(std::path::Path::new(p.require("db")?))?;
    mpedb_mirror::switch::set_frozen(&db, false)?;
    println!("unfrozen — writes to mirrored tables are allowed again");
    Ok(())
}

fn cmd_pull(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    let n = drain_pull(&db, a.as_mut())?;
    println!("pulled + applied {n} row change(s)");
    Ok(())
}

fn cmd_push(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    let s = drain_push(&db, a.as_mut())?;
    println!("pushed {} row change(s) to the source", s.upserts + s.deletes);
    if s.conflicts > 0 {
        println!(
            "{} change(s) parked (the source concurrently won) — run `pull` to \
             resolve source-wins, or `conflicts` to inspect",
            s.conflicts
        );
    }
    Ok(())
}

fn cmd_sync(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    let auth = read_epoch(&db)?.authority;
    let pulled = if auth == Authority::Source {
        drain_pull(&db, a.as_mut())?
    } else {
        0 // mpedb-authoritative: the source is a stale replica, do not pull
    };
    let s = drain_push(&db, a.as_mut())?;
    println!(
        "sync: pulled {pulled}, pushed {}, parked {} (authority: {auth:?})",
        s.upserts + s.deletes,
        s.conflicts
    );
    Ok(())
}

fn cmd_verify(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    if verify(&db, a.as_mut())? {
        println!("OK — mpedb and the source are identical");
        Ok(())
    } else {
        Err(Failure::Runtime("mpedb and the source DIVERGE (run reconcile)".into()))
    }
}

fn cmd_reconcile(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    let s = reconcile(&db, a.as_mut())?;
    println!(
        "reconciled: {} upserts, {} deletes across {} table(s)",
        s.upserts, s.deletes, s.tables_changed
    );
    Ok(())
}

fn cmd_switch(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "db", "to", "source-config"], &[])?;
    let (db, mut a) = open_source(p.require("db")?, &p)?;
    match p.require("to")? {
        "mpedb" => {
            switch_to_mpedb(&db, a.as_mut())?;
            println!("authority switched to mpedb (epoch {})", read_epoch(&db)?.epoch);
        }
        "source" => {
            switch_to_source(&db, a.as_mut())?;
            println!("authority switched to source (epoch {})", read_epoch(&db)?.epoch);
        }
        other => return usage(format!("--to must be mpedb|source, got `{other}`")),
    }
    Ok(())
}

fn cmd_import(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &[
            "source", "dest", "include", "exclude", "size_mb", "durability", "adapt",
            "source-config",
        ],
        &[],
    )?;
    let dest = PathBuf::from(p.require("dest")?);
    // Import is the one command with no mirror to read `mir/src` from, so the
    // spec comes from the flags alone.
    let spec = match (p.value("source-config"), p.value("source")) {
        (Some(cfg), _) => sourcecfg::load(std::path::Path::new(cfg))?,
        (None, Some(src)) => SourceSpec::Sqlite { path: src.into() },
        (None, None) => {
            return Err(Failure::Usage(
                "import needs --source <sqlite-file> or --source-config <0600-file>".into(),
            ))
        }
    };

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

    // Strict-reject stays the default (DESIGN-MIRROR §4.5): an off-type value
    // should fail loudly while a human is watching, not be silently coerced.
    let adapt = match p.value("adapt") {
        None => None,
        Some("exact") => Some(mpedb_mirror::AdaptMode::ExactOnly),
        Some("lossy") => Some(mpedb_mirror::AdaptMode::AllowLossy),
        Some(other) => {
            return usage(format!("--adapt must be exact|lossy, got `{other}`"))
        }
    };
    let opts = ImportOptions {
        size_bytes: size_mb * 1024 * 1024,
        durability,
        include,
        exclude,
        batch_rows: 8192,
        adapt,
    };

    let (db, report) = match &spec {
        SourceSpec::Sqlite { path } => {
            let mut conn = Connection::open(path)
                .map_err(|e| Failure::Runtime(format!("open sqlite source `{path}`: {e}")))?;
            import_sqlite(&mut conn, &dest, &opts)?
        }
        SourceSpec::Postgres { dsn } => {
            let mut client = PgAdapter::connect(dsn, opts.include.as_deref(), &opts.exclude)?;
            import_pg(client.client(), &dest, &opts)?
        }
    };

    // Record WHERE the credentials live (never the DSN itself — §12), so every
    // later command needs only --db. Written after the import commits: a
    // mir/src pointing into a mirror that does not exist is worse than none.
    if let Some(cfg) = p.value("source-config") {
        let abs = std::fs::canonicalize(cfg)
            .map_err(|e| Failure::Runtime(format!("canonicalize --source-config: {e}")))?;
        let mut s = db.begin()?;
        s.set_capture(false);
        s.sys_record_put(
            state::MIR_NS,
            state::KEY_SRC,
            abs.to_string_lossy().as_bytes(),
        )?;
        s.commit()?;
    }

    println!("imported {} into {}", spec.redacted(), dest.display());
    if !report.adapted.is_empty() {
        // An adapted import must say exactly what it changed: a count alone is a
        // summary of unreviewable edits.
        println!("  adapted {} value(s) on the way in:", report.adapted.len());
        for a in report.adapted.iter().take(20) {
            println!("    {a}");
        }
        if report.adapted.len() > 20 {
            println!("    … and {} more", report.adapted.len() - 20);
        }
    }
    for t in &report.tables {
        println!("  {:<24} {:>10} rows  (table {})", t.name, t.rows, t.table_id);
    }
    println!("  total: {} rows across {} tables", report.total_rows(), report.tables.len());
    println!("mirror is source-authoritative (epoch 1); capture enabled.");
    Ok(())
}

fn cmd_export(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["db", "dest", "to", "source-config", "pg-schema"],
        &[],
    )?;
    let db = PathBuf::from(p.require("db")?);

    // --to postgres is the other half of the migration: sqlite -> mpedb -> PG.
    if p.value("to") == Some("postgres") || p.value("source-config").is_some() {
        let cfg = p.value("source-config").ok_or_else(|| {
            Failure::Usage(
                "exporting to PostgreSQL needs --source-config <0600-file> (a DSN must \
                 never be a CLI arg -- it would be visible in `ps`)"
                    .into(),
            )
        })?;
        let spec = sourcecfg::load(std::path::Path::new(cfg))?;
        let SourceSpec::Postgres { dsn } = &spec else {
            return usage("--source-config for export --to postgres must be kind = \"postgres\"");
        };
        let mut client = postgres::Client::connect(dsn, postgres::NoTls)
            .map_err(|e| Failure::Runtime(format!("connect to {}: {e}", spec.redacted())))?;
        let opts = mpedb_mirror::PgExportOptions {
            schema: p.value("pg-schema").unwrap_or("public").to_string(),
            skip_preflight: false,
        };
        let report = mpedb_mirror::export_pg(&db, &mut client, &opts)?;
        println!("exported {} to {}", db.display(), spec.redacted());
        for t in &report.tables {
            println!("  {:<24} {:>10} rows", t.name, t.rows);
        }
        println!("  total: {} rows", report.total_rows());
        if !report.widened.is_empty() {
            // Silence here would read as "the schema round-tripped exactly".
            println!(
                "  NOTE: {} table(s) got generic (widened) column types rather than the \
                 source's own: {}\n        (a sqlite source declares affinities, not \
                 types -- its INTEGER is 64-bit and its REAL is a double, so copying \
                 those words into PostgreSQL, where they mean int4 and float4, would \
                 truncate your data rather than preserve it)",
                report.widened.len(),
                report.widened.join(", ")
            );
        }
        return Ok(());
    }

    let dest = PathBuf::from(p.require("dest")?);
    let report = export_sqlite(&db, &dest)?;
    println!("exported {} to {}", db.display(), dest.display());
    for t in &report.tables {
        println!("  {:<24} {:>10} rows", t.name, t.rows);
    }
    println!("  total: {} rows", report.total_rows());
    Ok(())
}

fn cmd_roundtrip(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "size_mb"], &[])?;
    let source = p.require("source")?;
    let size_mb = match p.value("size_mb") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| Failure::Usage("--size_mb must be an integer".into()))?,
        None => 256,
    };

    // temp mpedb + temp sqlite next to a unique name; cleaned up at the end
    let stamp = std::process::id();
    let mid = std::env::temp_dir().join(format!("mpedb-roundtrip-{stamp}.mpedb"));
    let out = std::env::temp_dir().join(format!("mpedb-roundtrip-{stamp}.db"));
    let _ = std::fs::remove_file(&mid);
    let _ = std::fs::remove_file(&out);

    let opts = ImportOptions {
        size_bytes: size_mb * 1024 * 1024,
        ..ImportOptions::default()
    };
    let imported = {
        let mut conn = Connection::open(source)
            .map_err(|e| Failure::Runtime(format!("open sqlite `{source}`: {e}")))?;
        let (_db, report) = import_sqlite(&mut conn, &mid, &opts)?;
        report.total_rows()
    };
    let exported = export_sqlite(&mid, &out)?.total_rows();

    let a = Connection::open(source)
        .map_err(|e| Failure::Runtime(format!("open source: {e}")))?;
    let b = Connection::open(&out).map_err(|e| Failure::Runtime(format!("open round-trip: {e}")))?;
    let diffs = diff_sqlite_data(&a, &b)?;
    drop((a, b));
    let _ = std::fs::remove_file(&mid);
    let _ = std::fs::remove_file(&out);

    println!("roundtrip {source}:  imported {imported} rows -> exported {exported} rows");
    if diffs.is_empty() {
        println!("  OK — data is byte-identical after sqlite -> mpedb -> sqlite");
        Ok(())
    } else {
        println!("  {} difference(s) (data that did not survive the mapping):", diffs.len());
        for d in diffs.iter().take(50) {
            println!("    - {d}");
        }
        Err(Failure::Runtime(format!("{} round-trip difference(s)", diffs.len())))
    }
}

/// `mirror preflight --db <file>` — check the data against the source schema
/// recorded at import, WITHOUT touching the source. Reads mpedb only, so it runs
/// with the source offline and costs no round-trips.
fn cmd_preflight(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db"], &[])?;
    let db = Database::open_from_file(std::path::Path::new(p.require("db")?))?;
    let r = mpedb_mirror::preflight(&db)?;

    if r.findings.is_empty() {
        println!("preflight: {} row(s) checked — nothing would be rejected", r.rows_checked);
        return Ok(());
    }
    // Column-level verdicts first: they are about the migration, not a row, and
    // burying them under 10k row findings hides the thing you most need to know.
    let (col_level, row_level): (Vec<_>, Vec<_>) = r
        .findings
        .iter()
        .partition(|f| f.kind == mpedb_mirror::FindingKind::LossyColumn);
    for f in &col_level {
        println!("LOSSY  {}.{}: {}", f.table, f.column, f.detail);
    }
    for f in row_level.iter().take(50) {
        let pk: Vec<String> = f.pk.iter().map(|v| format!("{v}")).collect();
        println!("REJECT {}.{} [pk {}]: {}", f.table, f.column, pk.join(","), f.detail);
    }
    if row_level.len() > 50 {
        println!("… and {} more", row_level.len() - 50);
    }
    println!(
        "\npreflight: {} row(s) checked, {} would be rejected, {} lossy column(s)",
        r.rows_checked,
        row_level.len(),
        col_level.len()
    );
    if r.would_fail() {
        return Err(Failure::Runtime(
            "preflight found values the source schema will reject — fix or accept them before \
             writing".into(),
        ));
    }
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
