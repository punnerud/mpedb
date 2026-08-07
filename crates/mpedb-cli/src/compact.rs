//! `mpedb compact-ids` — the table-id budget, and (later) reclaiming it.
//!
//! `MAX_TABLES` is a LIFETIME budget: ids are never reused, so `schema.tables`
//! keeps a dead slot per create, forever. A workload that creates and drops
//! tables — a table per tenant, a nightly staging table — spends the budget on
//! tombstones and eventually cannot create a table with almost nothing live.
//!
//! Today this reports. `--dry-run` is the whole command for now, and that is
//! deliberate rather than half-finished: the rewrite needs the database
//! exclusive, and an operator should be able to find out whether it is worth
//! taking anything down BEFORE they take it down. The report reads one
//! snapshot and is safe on a live database.

use crate::util::{open_target, usage, CliResult};

pub fn run(args: &[String]) -> CliResult {
    match args {
        [target] => report(target, false),
        [target, flag] if flag == "--dry-run" => report(target, false),
        [target, flag] if flag == "--apply" => report(target, true),
        _ => usage("compact-ids needs: compact-ids <target> [--dry-run | --apply]"),
    }
}

fn report(target: &str, apply: bool) -> CliResult {
    let db = open_target(target)?;
    let p = db.compact_plan()?;
    let total = p.live() + p.dead;

    println!("{target}");
    println!(
        "  table-id slots used : {total}  ({} live, {} tombstones)",
        p.live(),
        p.dead
    );
    if total > 0 {
        // The budget of THIS database, not the compiled-in default: with
        // `[database] max_tables` set, reporting the default would say a file
        // was full when it had room, or the reverse.
        let cap = db.max_tables();
        println!("  budget spent        : {:.1}% of {cap}", 100.0 * total as f64 / cap as f64);
    }

    if p.is_noop() {
        println!("\n  Nothing to compact — the id space is already dense.");
        return Ok(());
    }

    println!("\n  Compaction would free {} slots, leaving {}.", p.dead, p.live());
    if !p.records.is_empty() {
        println!("  Records to renumber:");
        for (owner, n) in &p.records {
            println!("    {n:>8}  {owner}");
        }
    }
    if p.plans > 0 {
        // Not a renumber: a published plan carries table ids INSIDE its bytes
        // and inside its footprint, so after a renumber it would name a
        // different table — and validate would not object, because the new id
        // is perfectly valid. Dropping them forces a re-prepare, which is the
        // only correct answer and a cheap one.
        println!(
            "    {:>8}  published plans — DROPPED, not renumbered (each carries table ids \
             in its bytes; a renumbered plan would name a different table and still \
             validate)",
            p.plans
        );
    }
    if !apply {
        println!("\n  Run again with --apply to do it. It needs the database EXCLUSIVE:");
        println!("  between renumbering the schema and renumbering the records keyed by the");
        println!("  old ids there is an instant a concurrent reader must not see.");
        return Ok(());
    }

    let done = db.compact_ids()?;
    println!(
        "\n  Compacted. {} catalog entries and {} system records renumbered; \n  \
         {} published plans dropped (each carried table ids in its bytes — \n  \
         re-prepare, or let the next `prepare` do it).",
        done.catalog, done.sys, done.plans
    );
    let after = db.compact_plan()?;
    println!(
        "  table-id slots used : {}  ({} live, {} tombstones)",
        after.live() + after.dead,
        after.live(),
        after.dead
    );
    Ok(())
}
