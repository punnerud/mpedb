//! mpedb vs DuckDB vs SQLite on an analytics workload.
//!
//! Two rules, inherited from benchmarks/head-to-head.md and non-negotiable here:
//!
//! 1. **Every engine answers the same question, and the harness checks.** Each
//!    query's result is rendered canonically and compared across engines before
//!    any timing is believed. An engine that disagrees is reported as
//!    DISAGREES and no ratio is printed — a fast wrong answer is not a
//!    benchmark result, it is a bug report.
//!
//! 2. **The instrument is measured before the change.** Every cell is repeated,
//!    the median reported, and the spread flagged when it is wide. A number
//!    without its spread is a number you cannot act on.
//!
//! What this benchmark is NOT: a claim that mpedb is an analytics engine. It is
//! a row store. On a scan-and-aggregate query a vectorised column store should
//! beat it by a wide margin, and the interesting question is not whether it
//! does but WHERE the margin goes — which cells close, which invert, and what
//! that says about precomputed access paths versus raw scan throughput.

mod cell;
mod eng_duckdb;
mod eng_mpedb;
mod eng_mysql;
mod eng_postgres;
mod eng_sqlite;
mod queries;
mod schema;

use std::time::Instant;

use queries::{Probes, QUERIES};

const DEFAULT_FACTS: i64 = 2_000_000;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Timing {
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    /// Coefficient of variation, percent — the instrument's own noise.
    cv_pct: f64,
}

fn timing_of(ms: &mut [f64]) -> Timing {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = ms[ms.len() / 2];
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let var = ms.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / ms.len() as f64;
    Timing {
        median_ms: median,
        min_ms: ms[0],
        max_ms: ms[ms.len() - 1],
        cv_pct: if mean > 0.0 { 100.0 * var.sqrt() / mean } else { 0.0 },
    }
}

/// One untimed warm-up per engine, then `reps` timed rounds. Each round runs
/// every engine once, and the engine that starts the round rotates, so a warm
/// core does not always land on whoever was listed last.
fn time_rotated<'a>(
    reps: usize,
    mut engines: Vec<(&'a str, Box<dyn FnMut() -> Res<String> + 'a>)>,
) -> Vec<(&'a str, Res<(Timing, String)>)> {
    let n = engines.len();
    let mut names = Vec::with_capacity(n);
    let mut answers: Vec<Option<String>> = Vec::with_capacity(n);
    let mut errs: Vec<Option<String>> = Vec::with_capacity(n);
    let mut samples: Vec<Vec<f64>> = Vec::with_capacity(n);
    for (name, run) in engines.iter_mut() {
        names.push(*name);
        samples.push(Vec::with_capacity(reps));
        match run() {
            Ok(a) => {
                answers.push(Some(a));
                errs.push(None);
            }
            Err(e) => {
                answers.push(None);
                errs.push(Some(e.to_string()));
            }
        }
    }
    for round in 0..reps {
        let start = round % n;
        for step in 0..n {
            let i = (start + step) % n;
            if errs[i].is_some() {
                continue;
            }
            let t0 = Instant::now();
            match engines[i].1() {
                Ok(got) if answers[i].as_ref() == Some(&got) => {
                    samples[i].push(t0.elapsed().as_secs_f64() * 1000.0);
                }
                Ok(_) => {
                    errs[i] = Some("engine gave two different answers to the same query".into());
                }
                Err(e) => errs[i] = Some(e.to_string()),
            }
        }
    }
    names
        .into_iter()
        .zip(errs)
        .zip(answers)
        .zip(samples)
        .map(|(((name, err), answer), mut ms)| {
            let result = if let Some(e) = err {
                Err(e.into())
            } else if ms.is_empty() {
                Err("no timed sample".into())
            } else {
                Ok((timing_of(&mut ms), answer.unwrap()))
            };
            (name, result)
        })
        .collect()
}

/// Sub-millisecond cells are where the precompute paths live, and one decimal
/// renders every one of them as "0.0" — which reads as "too fast to matter"
/// when it is the whole point of the row.
fn cell_ms(r: &Res<(Timing, String)>) -> String {
    match r {
        Ok((t, _)) if t.median_ms < 1.0 => format!("{:.3}", t.median_ms),
        Ok((t, _)) => format!("{:.1}", t.median_ms),
        Err(_) => "refused".to_string(),
    }
}

fn main() -> Res<()> {
    let mut facts = DEFAULT_FACTS;
    let mut reps = 5usize;
    let mut dir = std::env::temp_dir().join("mpedb-olapbench");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--facts" => facts = args.next().ok_or("--facts needs a value")?.parse()?,
            "--reps" => reps = args.next().ok_or("--reps needs a value")?.parse()?,
            "--dir" => dir = args.next().ok_or("--dir needs a value")?.into(),
            "--help" | "-h" => {
                println!("mpedb-olapbench [--facts N] [--reps N] [--dir PATH]");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    println!("# OLAP head-to-head: mpedb vs DuckDB vs SQLite vs PostgreSQL vs MySQL\n");
    println!("- fact rows: {facts}");
    println!(
        "- dimensions: customer {}, product {}, store {}, day {}",
        schema::DIM_CUSTOMER,
        schema::DIM_PRODUCT,
        schema::DIM_STORE,
        schema::DIM_DAY
    );
    println!(
        "- repetitions per cell: {reps} timed rounds, plus one untimed warm-up; \
         each round runs every engine once and the starting engine rotates; median reported"
    );
    println!(
        "- MPEE: {}",
        if std::env::var("MPEDB_NO_MPEE").is_ok() { "**OFF** (kill switch set)" } else { "on" }
    );
    println!();

    std::fs::create_dir_all(&dir)?;
    eprintln!("loading mpedb…");
    let (mpedb, mpedb_load) = eng_mpedb::Mpedb::load(&dir, facts)?;
    eprintln!("loading duckdb…");
    let (duck, duck_load) = eng_duckdb::Duck::load(facts)?;
    eprintln!("loading sqlite…");
    let (sqlite, sqlite_load) = eng_sqlite::Sqlite::load(facts)?;
    eprintln!("loading postgres…");
    let (pg, pg_load) = eng_postgres::Postgres::load(facts, &dir)?;
    eprintln!("loading mysql (mariadb)…");
    let (my, my_load) = eng_mysql::Mysql::load(facts, &dir)?;
    eprintln!("running queries…");

    println!("## Load\n");
    println!("| engine | load | note |");
    println!("|---|---:|---|");
    println!(
        "| mpedb | {mpedb_load:.1} s | {:.0} MiB file *reserved* (not used — mpedb pre-allocates); \
         five index trees on `fact`, maintained row by row |",
        mpedb.file_bytes() as f64 / (1024.0 * 1024.0)
    );
    println!("| duckdb | {duck_load:.1} s | in-memory, Appender, no indexes (its authors advise against them here) |");
    println!("| sqlite | {sqlite_load:.1} s | in-memory, same indexes as mpedb, built after the rows |");
    println!(
        "| postgres | {pg_load:.1} s | private throwaway cluster on disk, COPY load, same indexes, then ANALYZE |"
    );
    println!(
        "| mysql | {my_load:.1} s | private throwaway MariaDB, batched INSERT, same indexes, then ANALYZE |"
    );
    println!();

    println!("## Queries\n");
    println!("Times are milliseconds, median of {reps}.\n");
    println!("| query | probes | mpedb | duckdb | sqlite | postgres | mysql | mpedb vs duckdb | agree |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---|");

    let mut notes: Vec<String> = Vec::new();

    for q in QUERIES {
        if q.probes == Probes::Prepared {
            continue; // parameterised; measured in its own section below
        }
        let timed = time_rotated(
            reps,
            vec![
                ("mpedb", Box::new(|| mpedb.run(q.sql))),
                ("duckdb", Box::new(|| duck.run(q.sql))),
                ("sqlite", Box::new(|| sqlite.run(q.sql))),
                ("postgres", Box::new(|| pg.run(q.sql))),
                ("mysql", Box::new(|| my.run(q.sql))),
            ],
        );
        let m = &timed[0].1;
        let d = &timed[1].1;
        let s = &timed[2].1;
        let p = &timed[3].1;
        let y = &timed[4].1;

        // Agreement is established BEFORE any ratio is printed.
        let answers: Vec<&String> = timed
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok().map(|(_, a)| a))
            .collect();
        let agree = answers.windows(2).all(|w| w[0] == w[1]);

        let ratio = match (m, d) {
            (Ok((mt, _)), Ok((dt, _))) if agree && mt.median_ms > 0.0 => {
                let r = dt.median_ms / mt.median_ms;
                if r >= 1.0 {
                    format!("**{r:.1}× faster**")
                } else {
                    format!("{:.1}× slower", 1.0 / r)
                }
            }
            _ => "—".to_string(),
        };

        println!(
            "| `{}` | {:?} | {} | {} | {} | {} | {} | {} | {} |",
            q.name,
            q.probes,
            cell_ms(m),
            cell_ms(d),
            cell_ms(s),
            cell_ms(p),
            cell_ms(y),
            ratio,
            if agree { "yes" } else { "**NO**" }
        );

        for (name, r) in &timed {
            if let Err(e) = r {
                notes.push(format!("- `{}` refused by {name}: {e}", q.name));
            }
        }
        if !agree {
            notes.push(format!(
                "- `{}` — **the engines disagree.** Every timing on this row is meaningless \
                 until that is explained; a fast wrong answer is a bug report, not a result.",
                q.name
            ));
            for (name, r) in &timed {
                if let Ok((_, a)) = r {
                    let head: String = a.lines().take(3).collect::<Vec<_>>().join(" ⏎ ");
                    notes.push(format!("  - {name}: `{head}`"));
                }
            }
        }
        if let Ok((t, _)) = m {
            if t.cv_pct > 15.0 {
                notes.push(format!(
                    "- `{}` on mpedb is noisy: CV {:.0}%, {:.1}–{:.1} ms. Read the median as a range.",
                    q.name, t.cv_pct, t.min_ms, t.max_ms
                ));
            }
        }
    }
    println!();

    // ------------------------------------------------------------ the plans
    // A join-order benchmark that does not show the orders is asking to be
    // believed. mpedb's EXPLAIN is the engine's own, printed unedited.
    println!("## The plans each engine chose\n");
    for q in QUERIES
        .iter()
        .filter(|q| matches!(q.probes, Probes::JoinOrder | Probes::Precompute))
    {
        println!("### `{}`\n", q.name);
        for (engine, plan) in [
            ("mpedb", mpedb.explain(q.sql)),
            ("sqlite", sqlite.explain(q.sql)),
            ("duckdb", duck.explain(q.sql)),
            ("postgres", pg.explain(q.sql)),
            ("mysql", my.explain(q.sql)),
        ] {
            println!("{engine}:");
            println!("```");
            match plan {
                Ok(x) => println!("{}", x.trim_end()),
                Err(e) => println!("(EXPLAIN refused: {e})"),
            }
            println!("```\n");
        }
    }

    // ------------------------------------------------------------- prepared
    println!("## Prepared, parameterised, repeated\n");
    println!(
        "The one shape where an embedded row store is supposed to win: the same plan \
         executed many times with different parameters. mpedb runs `execute(hash, params)` \
         against a content-hashed plan — no parsing, no planning, no lookup by SQL text.\n"
    );
    println!("| query | iterations | mpedb | duckdb | sqlite | postgres | mysql | mpedb vs duckdb |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    const ITERS: i64 = 20_000;
    for q in QUERIES.iter().filter(|q| q.probes == Probes::Prepared) {
        // Same 20 000 executions, split into `reps` slices. Each slice runs
        // every engine once, and the engine that starts the slice rotates.
        let rounds = reps.max(1);
        let mut totals = [0.0_f64; 5];
        let mut failed: [Option<String>; 5] = Default::default();
        for round in 0..rounds {
            let begin = (ITERS * round as i64) / rounds as i64;
            let end = (ITERS * (round as i64 + 1)) / rounds as i64;
            let start = round % 5;
            for step in 0..5 {
                let eng = (start + step) % 5;
                if failed[eng].is_some() {
                    continue;
                }
                let part = match eng {
                    0 => prepared_mpedb(&mpedb, q.sql, begin, end, facts),
                    1 => prepared_duck(&duck, q.sql, begin, end, facts),
                    2 => prepared_sqlite(&sqlite, q.sql, begin, end, facts),
                    3 => pg.prepared_ms(q.sql, begin, end, facts),
                    _ => my.prepared_ms(q.sql, begin, end, facts),
                };
                match part {
                    Ok(ms) => totals[eng] += ms,
                    Err(e) => failed[eng] = Some(e.to_string()),
                }
            }
        }
        let finish = |i: usize| -> Res<f64> {
            match &failed[i] {
                Some(e) => Err(e.clone().into()),
                None => Ok(totals[i]),
            }
        };
        let mp = finish(0);
        let dp = finish(1);
        let sp = finish(2);
        let pp = finish(3);
        let yp = finish(4);
        let fmt = |r: &Res<f64>| match r {
            Ok(ms) => format!("{ms:.0}"),
            Err(_) => "refused".into(),
        };
        let ratio = match (&mp, &dp) {
            (Ok(m), Ok(d)) if *m > 0.0 => {
                let r = d / m;
                if r >= 1.0 {
                    format!("**{r:.1}× faster**")
                } else {
                    format!("{:.1}× slower", 1.0 / r)
                }
            }
            _ => "—".into(),
        };
        println!(
            "| `{}` | {ITERS} | {} | {} | {} | {} | {} | {ratio} |",
            q.name,
            fmt(&mp),
            fmt(&dp),
            fmt(&sp),
            fmt(&pp),
            fmt(&yp)
        );
        for (name, r) in [("mpedb", &mp), ("duckdb", &dp), ("sqlite", &sp), ("postgres", &pp), ("mysql", &yp)] {
            if let Err(e) = r {
                notes.push(format!("- prepared `{}` on {name}: {e}", q.name));
            }
        }
    }
    println!();

    if !notes.is_empty() {
        println!("## Notes\n");
        for n in &notes {
            println!("{n}");
        }
        println!();
    }

    println!("## What each query is for\n");
    for q in QUERIES {
        println!("- **`{}`** ({:?}) — {}", q.name, q.probes, q.about);
    }

    Ok(())
}

/// Total milliseconds for `iters` executions with a varying parameter.
fn prepared_mpedb(e: &eng_mpedb::Mpedb, sql: &str, start: i64, end: i64, facts: i64) -> Res<f64> {
    // mpedb's parameter marker is $1; the query set is written with `?`.
    let h = e.prepare(&sql.replace('?', "$1"))?;
    let t0 = Instant::now();
    for i in start..end {
        e.exec_param(&h, i % facts)?;
    }
    Ok(t0.elapsed().as_secs_f64() * 1000.0)
}

fn prepared_duck(e: &eng_duckdb::Duck, sql: &str, start: i64, end: i64, facts: i64) -> Res<f64> {
    let mut st = e.conn.prepare(sql)?;
    let t0 = Instant::now();
    for i in start..end {
        let mut rows = st.query(duckdb::params![i % facts])?;
        while rows.next()?.is_some() {}
    }
    Ok(t0.elapsed().as_secs_f64() * 1000.0)
}

fn prepared_sqlite(e: &eng_sqlite::Sqlite, sql: &str, start: i64, end: i64, facts: i64) -> Res<f64> {
    let mut st = e.conn.prepare(sql)?;
    let t0 = Instant::now();
    for i in start..end {
        let mut rows = st.query(rusqlite::params![i % facts])?;
        while rows.next()?.is_some() {}
    }
    Ok(t0.elapsed().as_secs_f64() * 1000.0)
}
