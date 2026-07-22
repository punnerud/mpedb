//! MIN/MAX under the ARGUMENT'S collation — differential against the bundled
//! sqlite oracle (3.45.0).
//!
//! sqlite's rule (probed, not recalled — every expectation here comes from the
//! oracle at runtime):
//!
//! * `min(x)`/`max(x)` compare under the collating sequence OF THE ARGUMENT:
//!   an explicit `COLLATE` on the argument, else the declared collation of the
//!   column it names, else BINARY. An expression argument (`x||''`) has no
//!   collation and compares BINARY.
//! * Ties (collation-equal values, `'a'` vs `'A'` under NOCASE): the FIRST row
//!   in scan order wins — `minmaxStep` replaces only on a STRICT beat — and
//!   the bare-column witness follows that same row.
//! * sqlite's OWN min/max-via-index optimization disagrees with its scan path
//!   on MAX ties (the index probe takes the highest-rowid entry of the maximal
//!   run, the scan keeps the first row). Where the two sqlite paths disagree,
//!   mpedb matches the NO-INDEX scan path — see `agg_over_index.rs` for the
//!   pinned divergence.
//! * NOCASE folds ASCII `A-Z` ONLY: `'Æ'`/`'æ'` stay distinct.
//!
//! The row fold compared min/max under BINARY before this battery existed, so
//! a NOCASE column holding `'a'` and `'B'` answered `min = 'B'` — a live wrong
//! answer, caught here.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;


/// Scratch directory for a throwaway test database: tmpfs where it exists
/// (Linux `/dev/shm` — the whole suite's convention, and much faster), the
/// platform temp dir otherwise. macOS has no `/dev/shm`, and hardcoding it
/// failed the entire file there with `Io(NotFound)` the first time CI ran on
/// macOS.
fn scratch_dir() -> String {
    if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".to_string()
    } else {
        std::env::temp_dir().to_string_lossy().trim_end_matches('/').to_string()
    }
}

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open_db(path: &str) -> Database {
    // A throwaway seed table satisfies the config schema; every test table is
    // CREATEd at runtime so its COLLATE clauses are parsed and applied live.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        Value::Float(f) => {
            // sqlite CLI prints reals with a trailing .0; mirror the common case.
            if f == f.trunc() && f.abs() < 1e15 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        other => panic!("unexpected value: {other:?}"),
    }
}

/// Run `setup` + `queries` on BOTH engines and require identical output.
fn diff(setup: &[&str], queries: &[&str]) {
    let path = format!(
        "{}/mpedb-agg-coll-{}-{}.mpedb",
        scratch_dir(),
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let db = open_db(&path);
    let mut script = String::new();
    for s in setup {
        db.query(s, &[]).unwrap();
        script.push_str(s);
        script.push_str(";\n");
    }
    for q in queries {
        let mine: Vec<Vec<String>> = match db.query(q, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| r.into_iter().map(render).collect())
                .collect(),
            other => panic!("expected rows from `{q}`, got {other:?}"),
        };
        let script_q = format!("{script}{q};\n");
        let theirs: Vec<Vec<String>> = sqlite_oracle::script_stdout(&script_q, "")
            .lines()
            .map(|l| l.split('|').map(str::to_string).collect())
            .collect();
        assert_eq!(mine, theirs, "mpedb vs sqlite {} on `{q}`", sqlite_oracle::version());
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

const NOCASE_T: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT COLLATE NOCASE, y INT)";

/// The headline wrong answer: binary min of {'a','B'} is 'B' (0x42 < 0x61),
/// NOCASE min is 'a'.
#[test]
fn min_max_use_the_declared_collation() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,'B',10),(2,'a',20)"],
        &[
            "SELECT min(x) FROM t",
            "SELECT max(x) FROM t",
            "SELECT min(x), max(x), count(*) FROM t",
        ],
    );
}

/// Ties: the FIRST scan row's value wins, in both directions and both orders,
/// and the bare-column witness rides the same row.
#[test]
fn ties_keep_the_first_scan_row_and_its_witness() {
    for rows in [
        "(1,'a',10),(2,'A',20),(3,'b',30)",
        "(1,'A',10),(2,'a',20),(3,'b',30)",
        "(1,'B',10),(2,'b',20),(3,'a',30)",
        "(1,'b',10),(2,'B',20),(3,'a',30)",
    ] {
        diff(
            &[NOCASE_T, &format!("INSERT INTO t (id, x, y) VALUES {rows}")],
            &[
                "SELECT min(x) FROM t",
                "SELECT max(x) FROM t",
                "SELECT y, min(x) FROM t",
                "SELECT y, max(x) FROM t",
            ],
        );
    }
}

/// RTRIM: values equal up to trailing spaces are a TIE, so the first row's
/// spelling comes back where BINARY would have ordered them apart — the max
/// run has the SHORT spelling first (BINARY max is the padded one, len 3),
/// the min run has the PADDED spelling first (BINARY min is the short one).
#[test]
fn rtrim_ties_return_the_first_spelling() {
    diff(
        &[
            "CREATE TABLE r (id INTEGER PRIMARY KEY, c TEXT COLLATE RTRIM)",
            "INSERT INTO r (id, c) VALUES (1,'x'),(2,'x  '),(3,'a '),(4,'a')",
        ],
        &[
            "SELECT min(c), length(min(c)) FROM r",
            "SELECT max(c), length(max(c)) FROM r",
        ],
    );
}

/// An explicit COLLATE on the argument overrides (rung 1), on a column with no
/// declared collation.
#[test]
fn explicit_collate_on_the_argument() {
    diff(
        &[
            "CREATE TABLE u (id INTEGER PRIMARY KEY, x TEXT)",
            "INSERT INTO u (id, x) VALUES (1,'B'),(2,'a')",
        ],
        &[
            "SELECT min(x), max(x) FROM u",
            "SELECT min(x COLLATE NOCASE), max(x COLLATE NOCASE) FROM u",
            "SELECT min(x COLLATE BINARY), max(x COLLATE BINARY) FROM u",
            "SELECT count(DISTINCT x), count(DISTINCT x COLLATE NOCASE) FROM u",
        ],
    );
}

/// An EXPRESSION argument carries no collation: `x||''` compares BINARY even
/// though `x` is NOCASE (probed: sqlite answers B|a here).
#[test]
fn expression_arguments_fall_back_to_binary() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,'B',10),(2,'a',20)"],
        &["SELECT min(x||''), max(x||'') FROM t"],
    );
}

/// GROUP BY: the same collation-aware extremum per group, through the grouped
/// fold, with the group key folding independently.
#[test]
fn grouped_min_max_use_the_collation_per_group() {
    diff(
        &[
            "CREATE TABLE g (id INTEGER PRIMARY KEY, grp INT, x TEXT COLLATE NOCASE)",
            "INSERT INTO g (id, grp, x) VALUES \
             (1,1,'B'),(2,1,'a'),(3,2,'D'),(4,2,'c'),(5,2,'C'),(6,3,NULL)",
        ],
        &[
            "SELECT grp, min(x), max(x) FROM g GROUP BY grp ORDER BY grp",
            "SELECT grp, min(x), max(x), count(x) FROM g GROUP BY grp ORDER BY grp",
        ],
    );
}

/// NULLs are skipped, an all-NULL group answers NULL, an empty input answers
/// NULL — none of that moved with the collation change.
#[test]
fn nulls_empty_and_all_null_groups() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,NULL,1),(2,'B',2),(3,'a',3),(4,NULL,4)"],
        &[
            "SELECT min(x), max(x) FROM t",
            "SELECT min(x), max(x) FROM t WHERE id > 99",
            "SELECT min(x), max(x) FROM t WHERE x IS NULL",
        ],
    );
}

/// NOCASE folds ASCII letters ONLY: 'Æ' and 'æ' are distinct, 'héllo' and
/// 'HÉLLO' fold their h but not their é.
#[test]
fn nocase_is_ascii_only() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,'\u{e6}',1),(2,'\u{c6}',2)"],
        &["SELECT min(x), max(x) FROM t"],
    );
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,'h\u{e9}llo',1),(2,'H\u{c9}LLO',2)"],
        &["SELECT min(x), max(x) FROM t"],
    );
}

/// DISTINCT (a no-op for min/max, but it must not change the compare) and
/// FILTER (restricts the input, same compare).
#[test]
fn distinct_and_filter_share_the_compare() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (1,'B',1),(2,'a',2),(3,'c',3)"],
        &[
            "SELECT min(DISTINCT x), max(DISTINCT x) FROM t",
            "SELECT min(x) FILTER (WHERE x <> 'c'), max(x) FILTER (WHERE y < 3) FROM t",
        ],
    );
}

/// The SCALAR max(a,b)/min(a,b): sqlite searches the arguments left-to-right
/// for the first that DEFINES a collating sequence (a bare column defines its
/// declared one — BINARY counts as defined) and compares under it.
#[test]
fn scalar_min_max_take_the_first_arguments_collation() {
    diff(
        &[NOCASE_T, "INSERT INTO t (id, x, y) VALUES (2,'a',1)"],
        &[
            "SELECT max(x,'B'), min(x,'B') FROM t",
            "SELECT max('B',x), min('B',x) FROM t",
            "SELECT max(x,'B','c'), min(x,'B','c') FROM t",
            // Control: no column anywhere means BINARY.
            "SELECT max('a','B'), min('a','B')",
        ],
    );
    // A BINARY-declared column DEFINES binary and stops the search even when a
    // NOCASE column follows it.
    diff(
        &[
            "CREATE TABLE b (id INTEGER PRIMARY KEY, p TEXT, n TEXT COLLATE NOCASE)",
            "INSERT INTO b (id, p, n) VALUES (1,'a','a')",
        ],
        &[
            "SELECT max(p,'B'), min(p,'B') FROM b",
            "SELECT max(p, n, 'B') FROM b",
            "SELECT max('B', n, p) FROM b",
        ],
    );
}

/// Window min/max fold under the argument's collation too (sqlite: probed).
#[test]
fn window_min_max_use_the_collation() {
    diff(
        &[
            "CREATE TABLE w (id INTEGER PRIMARY KEY, grp INT, x TEXT COLLATE NOCASE)",
            "INSERT INTO w (id, grp, x) VALUES (1,1,'B'),(2,1,'a'),(3,2,'D'),(4,2,'c')",
        ],
        &[
            "SELECT id, min(x) OVER (), max(x) OVER () FROM w ORDER BY id",
            "SELECT id, min(x) OVER (PARTITION BY grp) FROM w ORDER BY id",
            "SELECT id, max(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
             FROM w ORDER BY id",
        ],
    );
}
