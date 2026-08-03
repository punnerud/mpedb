//! N2 (0.2.9-sporet): the connect cell's attribution instrument. The dbapi
//! bench says ~0.4 ms per `:memory:` open against stdlib's ~0.013 — this
//! `#[ignore]`d release-only loop is the Rust-level half (what share is the
//! engine's, before pyo3), and the profiling target for `perf record`.
//!
//!     cargo test --release -p mpedb --test connect_cost -- --ignored --nocapture

use mpedb::{Config, Database};
use std::time::Instant;

const TOML: &str = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                    [[table]]\nname = \"users\"\nprimary_key = [\"id\"]\n\
                    [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
                    [[table.column]]\nname = \"email\"\ntype = \"text\"\n";

#[test]
#[ignore]
fn connect_cost() {
    if cfg!(debug_assertions) {
        eprintln!("connect_cost: release only");
        return;
    }
    // Warm-up.
    drop(Database::open_with_config(Config::from_toml_str(TOML).unwrap()).unwrap());
    for round in 0..3 {
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            let db = Database::open_with_config(Config::from_toml_str(TOML).unwrap()).unwrap();
            drop(db);
        }
        let per = t.elapsed().as_micros() / n;
        eprintln!("connect round={round} us_per_open={per}");
        // The config parse alone, for the attribution.
        let t = Instant::now();
        for _ in 0..n {
            let _ = Config::from_toml_str(TOML).unwrap();
        }
        eprintln!("connect round={round} us_per_config_parse={}", t.elapsed().as_micros() / n);
        // The open/close split: what the OPEN costs against what the DROP
        // (munmap of the whole mapping) costs — the drop is part of the
        // consumer's connect+close cell but no open-path work can move it.
        let k = 50;
        let t = Instant::now();
        let dbs: Vec<_> = (0..k)
            .map(|_| Database::open_with_config(Config::from_toml_str(TOML).unwrap()).unwrap())
            .collect();
        let open_us = t.elapsed().as_micros() / k;
        let t = Instant::now();
        drop(dbs);
        eprintln!(
            "connect round={round} split_open={open_us}us split_drop={}us",
            t.elapsed().as_micros() / k
        );
    }
}
