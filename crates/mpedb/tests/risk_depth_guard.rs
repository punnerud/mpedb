//! Stage C of design/DESIGN-MPEE-GENERAL.md, risk half: a recursive CTE whose
//! recursive term carries an integer counter and guards on it terminates by
//! proof, and the prepare-time risk estimate must say so instead of reporting
//! the halting-problem default. One repro (the exact generator shape the
//! playground probes flagged as a false "runtime-budget risk"), one negative
//! (no guard ⇒ still honestly unbounded).

use mpedb::{Config, Database};

fn db() -> (Database, String) {
    let path = format!(
        "{}/riskdg-{}.mpedb",
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 4
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

#[test]
fn a_depth_guard_bounds_the_estimate_and_its_absence_does_not() {
    let (d, path) = db();
    let schema = d.schema();

    // The classic finite generator: x starts at 1, steps by 1, guarded < 20.
    // 19 iterations by proof — the estimate must be small and say why.
    let plan = mpedb_sql::prepare(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 20) \
         SELECT x FROM c",
        &schema.schema,
    )
    .unwrap();
    let est = d.estimate_risk_for_plan(&plan).unwrap();
    assert!(
        est.work_rows < 1_000,
        "a provably terminating generator must not estimate as a runaway; got {}",
        est.work_rows
    );
    assert!(
        est.dominant.contains("depth guard"),
        "the attribution must say WHY it is bounded; got: {}",
        est.dominant
    );

    // Same generator, no guard, no outer LIMIT: genuinely unbounded, and the
    // estimate must keep saying so — the fix must not manufacture confidence.
    let plan = mpedb_sql::prepare(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c",
        &schema.schema,
    )
    .unwrap();
    let est = d.estimate_risk_for_plan(&plan).unwrap();
    assert_eq!(est.work_rows, u64::MAX, "no guard ⇒ still the honest unbounded");

    // A guard on a column carried UNCHANGED never terminates: x < 20 holds
    // forever when x never moves. The proof must notice the transit, not just
    // the comparison.
    let plan = mpedb_sql::prepare(
        "WITH RECURSIVE c(x, y) AS (SELECT 1, 1 UNION ALL SELECT x, y+1 FROM c WHERE x < 20) \
         SELECT y FROM c",
        &schema.schema,
    )
    .unwrap();
    let est = d.estimate_risk_for_plan(&plan).unwrap();
    assert_eq!(
        est.work_rows,
        u64::MAX,
        "a guard on an unchanging column proves nothing; got {}",
        est.work_rows
    );

    let _ = std::fs::remove_file(&path);
}
