//! `mpedb listen` — block until named tables change (#141 S3).
//!
//! The shell-visible half of change notification. What makes it worth having
//! as a command rather than only an API is that it composes with the way this
//! project already expects work to be triggered: a `listen` that exits on the
//! first change is a doorbell for `xargs`, a systemd unit, or a `while` loop,
//! and it hibernates in between rather than polling the file.
//!
//! ```text
//! mpedb listen db.toml orders                 # wake once, print, exit
//! mpedb listen db.toml orders lines --follow  # keep reporting
//! mpedb listen db.toml orders --key 42        # only that primary key
//! ```
//!
//! Exit status is the interface: **0** = something changed, **1** = the
//! timeout expired with nothing to report. A script can therefore write
//! `if mpedb listen … ; then work; fi` without parsing output.

use crate::args;
use crate::util::{CliResult, Failure};

pub fn usage(msg: &str) -> CliResult {
    Err(Failure::Usage(format!(
        "{msg}\n\
         \n\
         mpedb listen <target> <table>... [--timeout <s>] [--follow] [--key <v>]\n\
         \n\
         Blocks until one of the named tables changes, prints the tables that\n\
         moved, and exits 0. Exits 1 if --timeout elapses with no change.\n\
         \n\
           --timeout <s>  give up after s seconds (default: wait forever)\n\
           --follow       keep going instead of exiting on the first change\n\
           --key <v>      only changes that could touch this primary key;\n\
                          applies to every named table, and is honoured only\n\
                          as far as the writer could name its key region --\n\
                          a wide write always wakes you (see benchmarks/notify.md)\n"
    )))
}

pub fn run(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["timeout", "key"], &["follow"])?;
    let (target, tables) = match p.positional.split_first() {
        Some((t, rest)) if !rest.is_empty() => (t, rest),
        _ => return usage("listen needs <target> and at least one table"),
    };
    let follow = p.has("follow");
    // No --timeout means block indefinitely: this command's whole point is to
    // be the thing a hibernating service waits on.
    let timeout = match p.value("timeout") {
        Some(_) => std::time::Duration::from_secs(p.u64_or("timeout", 0)?),
        None => std::time::Duration::from_secs(u32::MAX as u64),
    };

    let db = crate::util::open_target(target)?;
    let bundle = db.schema();

    // Resolve names once, up front: a typo must be an error before we block,
    // not a wait that never returns.
    let mut ids = Vec::with_capacity(tables.len());
    for name in tables {
        let id = bundle
            .schema
            .tables
            .iter()
            .position(|t| t.name == *name)
            .ok_or_else(|| Failure::Usage(format!("no such table: {name}")))?;
        ids.push(id as u32);
    }

    let keys = match p.value("key") {
        Some(k) => vec![Some(vec![crate::util::parse_param(k)]); ids.len()],
        None => vec![None; ids.len()],
    };

    let mut listener = db.listen_keyed(&ids, &keys);
    let mut any = false;
    loop {
        let changed = listener.wait(timeout);
        if changed.is_empty() {
            break;
        }
        any = true;
        for id in changed {
            let name = bundle
                .schema
                .tables
                .get(id as usize)
                .map(|t| t.name.as_str())
                .unwrap_or("?");
            println!("{name}");
        }
        if !follow {
            break;
        }
    }
    if any {
        Ok(())
    } else {
        // Distinguishable from a change without parsing stdout.
        Err(Failure::Runtime("timed out with no change".into()))
    }
}
