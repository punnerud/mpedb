//! `mpedb backup` / `mpedb restore` — the whole database as one file.
//!
//! Distinct from `mpedb dump`, and the difference is worth stating because both
//! produce "the database as a file":
//!
//! | | `dump` | `backup` |
//! |---|---|---|
//! | contents | schema and rows, as text | the B+trees, verbatim |
//! | restores with | a SQL parse per statement | a page copy |
//! | reads on another engine | yes — it is SQL | no |
//! | indexes | rebuilt from the rows | carried |
//!
//! So `dump` is for reading and for moving data somewhere else, and `backup` is
//! for getting the same database back quickly. A dump of a project full of
//! images pays a hex expansion per blob byte and a parse on the way in; a
//! backup does not.
//!
//! Neither blocks a writer, but for different reasons: `dump` reads rows
//! through an ordinary snapshot, and `backup` copies pages under one (see
//! `mpedb_core::backup` for why the COW discipline makes that sound).

use crate::util::{open_target, usage, CliResult, Failure};

pub fn run(verb: &str, args: &[String]) -> CliResult {
    match (verb, args) {
        ("backup", [target, out]) => backup(target, out),
        ("restore", [file, target]) => restore(file, target),
        _ => usage(
            "backup needs: backup <target> <out.mpebak> | restore <in.mpebak> <target>\n  \
             <target> is a config.toml or a .mpedb file. RESTORE's target must be a database \
             this command just created (no DDL has run on it) — a half-restored file cannot \
             be told from a corrupt one.",
        ),
    }
}

/// Should `restore` create this target itself?
///
/// Only for a path that does not exist AND is not a config: a `.toml` names a
/// geometry its author chose, and silently substituting the backup's would
/// ignore a decision someone wrote down.
fn target_needs_creating(target: &str) -> bool {
    let p = std::path::Path::new(target);
    !p.exists() && p.extension().is_none_or(|e| e != "toml")
}

/// Enough megabytes for the backup, plus room to write afterwards.
///
/// The restored database is a WORKING one, not an archive: sizing it to exactly
/// the backup would produce a file that opens and then refuses the first
/// insert. A quarter again, and never below the source's own minimum.
fn size_mb_for(info: &mpedb_core::backup::BackupInfo) -> u64 {
    let bytes = info.high_water * info.page_size as u64;
    let mb = bytes.div_ceil(1024 * 1024);
    (mb + mb / 4 + 4).max(16)
}

fn backup(target: &str, out: &str) -> CliResult {
    let db = open_target(target)?;
    let bytes = db.backup()?;
    std::fs::write(out, &bytes)
        .map_err(|e| Failure::Runtime(format!("writing {out}: {e}")))?;
    // The SIZE is the interesting number, because it is the claim: a backup is
    // proportional to the data, not to the arena the data sits in.
    println!(
        "backed up {} to {out} ({:.1} MiB)",
        target,
        bytes.len() as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn restore(file: &str, target: &str) -> CliResult {
    let bytes = std::fs::read(file)
        .map_err(|e| Failure::Runtime(format!("reading {file}: {e}")))?;
    // Read the header BEFORE opening the target: a wrong file should say so
    // without having created anything.
    let info = mpedb_core::backup::read_header(&bytes)?;
    let db = match target_needs_creating(target) {
        // A `.mpedb` path that is not there yet is CREATED from the backup's
        // own geometry, and that is the difference between a feature and a
        // puzzle. `max_readers` must match or every page lands at the wrong id
        // — but the backup KNOWS what it was, so making the operator find out
        // and write a config would be asking them for something already in
        // their hand. Given a config.toml, that config wins and a mismatch is
        // the named refusal below: an explicit geometry is a decision.
        true => {
            let mb = size_mb_for(&info);
            let toml = format!(
                "[database]\npath = \"{}\"\nsize_mb = {mb}\nmax_readers = {}\n",
                target.replace('\\', "\\\\").replace('"', "\\\""),
                info.max_readers
            );
            let cfg = mpedb::Config::from_toml_str(&toml)?;
            println!(
                "creating {target} from the backup's geometry ({} reader slots, {mb} MiB)",
                info.max_readers
            );
            mpedb::Database::open_with_config(cfg)?
        }
        false => open_target(target)?,
    };
    db.restore_backup(&bytes)?;
    println!(
        "restored {file} into {target} ({} pages, {} reader slots, txn {})",
        info.n_pages, info.max_readers, info.txn_id
    );
    Ok(())
}
