//! Differential testing vs the BUNDLED sqlite (rusqlite `bundled`, pinned in
//! Cargo.toml — no `sqlite3` binary involved; STRICT tables) and, three-way,
//! vs a throwaway PostgreSQL 16 cluster. Any divergence fails the test with
//! the seed and a minimized reproduction program.
//!
//! The known semantic differences that are normalized or avoided in
//! generation are documented in `mpedb_testkit::diff` (module docs).

use mpedb_testkit::diff::{run_differential, run_differential_3way, PgDiff};

#[test]
fn differential_200_programs() {
    // 200 programs x ~80 statements, seeds 1000..1200. Deterministic.
    let stats = run_differential(1000, 200, 80).unwrap_or_else(|e| panic!("{e}"));
    println!(
        "differential: {} programs, {} statements, {} SELECTs compared, {} agreed failures",
        stats.programs, stats.statements, stats.selects_compared, stats.agreed_failures
    );
    assert_eq!(stats.programs, 200);
    // The generator must actually exercise both the query and the error
    // paths; a silent generator regression would hollow the test out.
    assert!(stats.selects_compared >= 200, "too few SELECTs compared");
    assert!(stats.agreed_failures > 0, "no constraint failures exercised");
}

/// Long-haul version. Run with:
/// `cargo test -p mpedb-testkit --release -- --ignored differential_2000`
#[test]
#[ignore = "long-haul: ~10x the default battery"]
fn differential_2000_programs() {
    let stats = run_differential(50_000, 2000, 80).unwrap_or_else(|e| panic!("{e}"));
    println!(
        "differential (long): {} programs, {} statements, {} SELECTs compared",
        stats.programs, stats.statements, stats.selects_compared
    );
    assert_eq!(stats.programs, 2000);
}

/// Three-way: every statement must agree in mpedb, sqlite3 AND PostgreSQL
/// 16. Fails soft — a loud skip, never a silent pass — if this environment
/// cannot start a throwaway PG cluster (it can: mpedb-bench uses the same
/// recipe).
#[test]
fn three_way_100_programs() {
    match run_differential_3way(7_000, 100, 60).unwrap_or_else(|e| panic!("{e}")) {
        PgDiff::Ran(stats) => {
            println!(
                "three-way: {} programs, {} statements, {} SELECTs compared, {} agreed failures",
                stats.programs, stats.statements, stats.selects_compared, stats.agreed_failures
            );
            assert_eq!(stats.programs, 100);
            assert!(stats.selects_compared >= 100, "too few SELECTs compared");
            assert!(stats.agreed_failures > 0, "no constraint failures exercised");
        }
        PgDiff::Unavailable(msg) => {
            eprintln!(
                "==============================================================\n\
                 SKIPPED three_way_100_programs: this environment cannot start\n\
                 a throwaway PostgreSQL 16 cluster. NO three-way coverage ran.\n\
                 {msg}\n\
                 =============================================================="
            );
        }
    }
}

/// Long-haul three-way battery. Run with:
/// `cargo test -p mpedb-testkit --release -- --ignored three_way_1000`
#[test]
#[ignore = "long-haul: ~10x the default three-way battery"]
fn three_way_1000_programs() {
    match run_differential_3way(90_000, 1000, 60).unwrap_or_else(|e| panic!("{e}")) {
        PgDiff::Ran(stats) => {
            println!(
                "three-way (long): {} programs, {} statements, {} SELECTs compared",
                stats.programs, stats.statements, stats.selects_compared
            );
            assert_eq!(stats.programs, 1000);
        }
        PgDiff::Unavailable(msg) => {
            eprintln!("SKIPPED three_way_1000_programs (no PostgreSQL): {msg}");
        }
    }
}
