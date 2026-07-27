//! What does one `schema_gen` bump cost, now that a plan memo depends on it?
//! (#167's premise, measured before building anything.)
//!
//! `schema_gen` is ONE counter for the whole database. Eighteen call sites bump
//! it: DDL and triggers (table-scoped in principle), ANALYZE (index-scoped),
//! and tunables / cost policy / model (genuinely global). Every one of them
//! invalidates EVERY entry of the #166 SQL-text plan memo, in every attached
//! process — including entries for tables the change could not possibly affect.
//!
//! #167 proposes splitting it per table so a change to table A leaves table B's
//! plans alone. That is only worth building if the wipe costs something, so:
//! fill the memo with `n` distinct statements, bump the generation once, and
//! time the refill against a steady-state pass over the same statements.
//!
//! The refill delta is the whole prize a per-table generation could win, and
//! only for the table-scoped half of the bump sites — a tunable change has to
//! re-cost every plan no matter how the counter is shaped.
//!
//! ```text
//! cargo run --release -p mpedb --example gen_bump_cost -- --stmts 512
//! ```

use std::path::PathBuf;
use std::time::Instant;

use mpedb::{Config, Database, Value};

/// Two tables so the sweep can also show what a per-table generation would
/// have to distinguish: statements over `a` and statements over `b`.
const SCHEMA: &str = "\n[[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n\
     \n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
     \n  [[table.column]]\n  name = \"v\"\n  type = \"int64\"\n\
     \n[[table]]\nname = \"b\"\nprimary_key = [\"id\"]\n\
     \n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
     \n  [[table.column]]\n  name = \"v\"\n  type = \"int64\"\n";

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Default matches MEMO_CAP, so the measured wipe is a full one.
    let stmts: usize = arg(&args, "--stmts", "512").parse().expect("--stmts");
    let dir = PathBuf::from(arg(&args, "--dir", "."));
    std::fs::create_dir_all(&dir).expect("--dir");

    let path = dir.join("genbump.mpedb");
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 256\nmax_readers = 16\n\
         durability = \"none\"\n{SCHEMA}",
        mpedb::toml_escape(&path.display().to_string())
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();

    for i in 0..64i64 {
        db.query("INSERT INTO a (id, v) VALUES (?, ?)", &[Value::Int(i), Value::Int(i)])
            .unwrap();
        db.query("INSERT INTO b (id, v) VALUES (?, ?)", &[Value::Int(i), Value::Int(i)])
            .unwrap();
    }

    // Distinct TEXTS, half over `a` and half over `b` — a per-table generation
    // would keep one half alive across a bump that only touched the other.
    let sql: Vec<String> = (0..stmts)
        .map(|i| {
            let t = if i % 2 == 0 { "a" } else { "b" };
            format!("SELECT v FROM {t} WHERE id = {i} OR v > {i}")
        })
        .collect();

    let run = |label: &str| -> f64 {
        let t = Instant::now();
        for s in &sql {
            db.query(s, &[]).unwrap();
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / stmts as f64;
        println!("  {label:<38} {us:>8.2} us/stmt");
        us
    };

    println!("one schema_gen bump against a {stmts}-entry memo\n");
    let cold = run("cold — first sight, all compile");
    let warm = run("warm — every statement memoized");
    // ANALYZE is a real bump site and needs no schema change to trigger; it is
    // the cheapest honest way to move the counter (see stats.rs's #166 note).
    db.analyze().unwrap();
    let refill = run("after one bump — full wipe, refill");
    let warm2 = run("warm again");

    println!(
        "\n  wipe cost = refill − warm = {:.2} us/stmt over {stmts} statements\n  \
         = {:.2} ms paid ONCE per bump; steady state {:.2} vs {:.2} us\n\n  \
         A per-table generation could only win this back for the TABLE-SCOPED\n  \
         bump sites (DDL, triggers, ANALYZE). Tunables, cost policy and the\n  \
         model change how every plan is priced, so they must wipe everything\n  \
         whatever shape the counter has.",
        refill - warm,
        (refill - warm) * stmts as f64 / 1000.0,
        warm2,
        cold
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
}
