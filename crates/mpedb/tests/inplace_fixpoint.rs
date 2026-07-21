//! Regression: the §4.5 commit fixpoint must converge under the private
//! `:memory:` in-place write mode (`adopt_inplace`).
//!
//! Before the `free()` routing rule in `engine/freelist.rs` (every
//! fixpoint-time free is interred in `freed`, never recycled into
//! `reusable`), delete-heavy `:memory:` databases refused
//! `INSERT INTO .. SELECT` commits with
//! `internal error (bug in mpedb): freelist fixpoint did not converge`:
//! in-place adoption keeps `freed` empty and makes every structurally freed
//! freelist node a *dirty* page, so an unrouted free landed in `reusable` —
//! the pool the fixpoint allocates from — and the plan degenerated into a
//! period-2 cycle (one leftover page consumed to become the tree node that
//! records it, then freed again by deleting the now-empty record). Found by
//! the sqllogictest corpus 2026-07-21 (`index/delete/100/slt_good_0.test`,
//! 145 refused commits); design/DESIGN.md §4.5 has the full argument.
//!
//! The loop below is the smallest deterministic churn that walks a commit
//! into the degenerate state (freelist entries drawn, everything adopted,
//! exactly one leftover page at the own-entry boundary). The unfixed engine
//! refuses the SECOND round's INSERT..SELECT; the fixed engine converges in
//! 2–4 passes. The extra rounds are margin against allocator drift.
//!
//! `db.verify()` after every round is the page-accounting invariant (§4.5:
//! reachable ⊎ freelisted ⊎ [high_water, page_count) partition the data
//! region) — it is what would catch the routing rule leaking a page
//! (listed nowhere) or double-listing one (freed AND reachable).

use mpedb::{Config, Database};

fn memory_db() -> Database {
    let toml = r#"
[database]
path = ":memory:"
size_mb = 32
max_readers = 4

[[table]]
name = "t0"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"

[[table]]
name = "t1"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"
"#;
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

#[test]
fn delete_heavy_insert_select_converges_in_memory() {
    let db = memory_db();
    // Seed rows wide enough that a batch spans multiple pages (varlen text),
    // so each round's DELETE creates real freelist entries and each
    // INSERT..SELECT draws and part-consumes them.
    for i in 0..400i64 {
        db.query(
            &format!("INSERT INTO t0 VALUES ({i}, {}, 'row-{i}-{}')", i * 7, "x".repeat(40)),
            &[],
        )
        .unwrap_or_else(|e| panic!("seed insert {i}: {e}"));
    }
    for round in 0..24 {
        // One txn, many rows: draws freelist entries from the previous
        // round's DELETE, adopts pages in place, ends with leftovers.
        db.query("INSERT INTO t1 SELECT * FROM t0", &[])
            .unwrap_or_else(|e| panic!("round {round}: INSERT..SELECT: {e}"));
        // Delete-heavy: frees a page run back to the freelist every round.
        db.query("DELETE FROM t1", &[])
            .unwrap_or_else(|e| panic!("round {round}: DELETE: {e}"));
        // §4.5 page-accounting partition must hold after every commit.
        db.verify()
            .unwrap_or_else(|e| panic!("round {round}: page accounting: {e}"));
    }
}
