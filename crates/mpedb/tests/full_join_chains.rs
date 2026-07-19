//! FULL [OUTER] JOIN inside N-way join chains, proven against sqlite 3.45.
//!
//! mpedb's join executor is a strictly left-deep nested loop: `A J1 B J2 C`
//! is evaluated as `(A J1 B) J2 C`, exactly SQL's left-associative reading. A
//! FULL join at step `k` computes `acc FULL OUTER JOIN table_k`, where `acc` is
//! the relation accumulated so far — NULL-extending the inner for unmatched
//! accumulated-left rows, then sweeping the held inner for rows no left row
//! matched (NULL-extended on the left, at the current accumulated width). That
//! composition is position-independent, so FULL is allowed at ANY position in a
//! chain — this test PROVES it by running many 3- and 4-table shapes through
//! BOTH mpedb and the BUNDLED sqlite 3.45.0 and requiring byte-identical
//! output.
//!
//! The test is a true differential: sqlite is the oracle — the in-process
//! bundled library (`tests/sqlite_oracle`), so it runs identically on every
//! machine and never skips.
use mpedb::{Config, Database, ExecResult, Value};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

// ---- schema + data, identical on both engines --------------------------------

const TABLES: &[&str] = &["t1", "t2", "t3", "t4"];

/// (id, k, label) rows per table. Keys deliberately overlap only partially so
/// every outer join has matched, left-only and right-only rows; a duplicate key
/// (t2 k=3 twice) exercises fan-out and the unmatched-inner bookkeeping; a NULL
/// key (t3) exercises `k = k` UNKNOWN on both the matching and null-extend side.
fn rows(t: &str) -> Vec<(i64, Option<i64>, &'static str)> {
    match t {
        "t1" => vec![(10, Some(1), "p"), (11, Some(2), "q"), (12, Some(3), "r")],
        "t2" => vec![
            (20, Some(2), "q"),
            (21, Some(3), "r"),
            (22, Some(4), "s"),
            (23, Some(3), "r2"),
        ],
        "t3" => vec![
            (30, Some(3), "r"),
            (31, Some(4), "s"),
            (32, Some(5), "t"),
            (33, None, "z"),
        ],
        "t4" => vec![(40, Some(1), "p"), (41, Some(4), "s"), (42, Some(5), "t")],
        _ => unreachable!(),
    }
}

fn mpedb_config(path: &str) -> String {
    let mut cfg = format!("[database]\npath = \"{path}\"\nsize_mb = 8\n");
    for t in TABLES {
        cfg.push_str(&format!(
            "[[table]]\nname = \"{t}\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
             [[table.column]]\nname = \"k\"\ntype = \"int64\"\n\
             [[table.column]]\nname = \"label\"\ntype = \"text\"\n"
        ));
    }
    cfg
}

fn open_mpedb(path: &str) -> Database {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let db = Database::open_with_config(Config::from_toml_str(&mpedb_config(path)).unwrap()).unwrap();
    for t in TABLES {
        for (id, k, label) in rows(t) {
            let kv = k.map_or("NULL".to_string(), |n| n.to_string());
            db.query(
                &format!("INSERT INTO {t} (id, k, label) VALUES ({id}, {kv}, '{label}')"),
                &[],
            )
            .unwrap();
        }
    }
    db
}

/// The sqlite reference schema+data, replayed on a fresh in-memory bundled
/// connection for every query (the old version materialized a `.sqlite` file
/// for the system binary).
fn build_sqlite_ddl() -> String {
    let mut ddl = String::new();
    for t in TABLES {
        ddl.push_str(&format!(
            "CREATE TABLE {t}(id INTEGER PRIMARY KEY, k INTEGER, label TEXT);\n"
        ));
        for (id, k, label) in rows(t) {
            let kv = k.map_or("NULL".to_string(), |n| n.to_string());
            ddl.push_str(&format!(
                "INSERT INTO {t}(id,k,label) VALUES({id},{kv},'{label}');\n"
            ));
        }
    }
    ddl
}

// ---- running + formatting ----------------------------------------------------

fn fmt_val(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Float(f) => f.to_string(),
        other => format!("{other:?}"),
    }
}

/// mpedb result as `c1|c2|…` rows, or Err(message) for a refusal.
fn run_mpedb(db: &Database, sql: &str) -> Result<Vec<String>, String> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => Ok(rows
            .iter()
            .map(|r| r.iter().map(fmt_val).collect::<Vec<_>>().join("|"))
            .collect()),
        Ok(o) => Err(format!("non-row result: {o:?}")),
        Err(e) => Err(format!("{e}")),
    }
}

fn run_sqlite(ddl: &str, sql: &str) -> Vec<String> {
    let s = sqlite_oracle::script_stdout(&format!("{ddl}{sql};\n"), "NULL");
    s.lines().map(|l| l.to_string()).collect()
}

// ---- query generation --------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum K {
    Inner,
    Left,
    Full,
}
impl K {
    fn sql(self) -> &'static str {
        match self {
            K::Inner => "JOIN",
            K::Left => "LEFT JOIN",
            K::Full => "FULL JOIN",
        }
    }
    fn all() -> [K; 3] {
        [K::Inner, K::Left, K::Full]
    }
}

/// Build one query over `n` tables (3 or 4) with join-kind sequence `kinds`
/// (length n-1), ON pattern `pat`, and optional WHERE. `lead_right` makes the
/// FIRST join a RIGHT (rewritten to a swapped LEFT chain by the planner);
/// `star` selects `*` instead of explicit columns to also check column order.
fn build_query(
    n: usize,
    kinds: &[K],
    pat: &str,
    with_where: bool,
    lead_right: bool,
    star: bool,
) -> String {
    let cols: Vec<String> = (1..=n)
        .flat_map(|i| {
            [
                format!("t{i}.id"),
                format!("t{i}.k"),
                format!("t{i}.label"),
            ]
        })
        .collect();
    let sel = if star { "*".to_string() } else { cols.join(", ") };
    let mut q = format!("SELECT {sel} FROM t1");
    for (j, kind) in kinds.iter().enumerate() {
        let cur = j + 2; // table number being joined (t2, t3, …)
        let prev = j + 1; // immediately previous table
        let on = match pat {
            "chain" => format!("t{cur}.k = t{prev}.k"),
            "star" => format!("t{cur}.k = t1.k"),
            // ON references an EARLIER table (t1) in a way that CHANGES matching
            // once t1 has been null-extended by a prior FULL.
            "onref" => format!("t{cur}.k = t{prev}.k AND t1.id IS NOT NULL"),
            _ => unreachable!(),
        };
        let kw = if j == 0 && lead_right { "RIGHT JOIN" } else { kind.sql() };
        q.push_str(&format!(" {kw} t{cur} ON {on}"));
    }
    if with_where {
        // A predicate over a possibly-null-extended middle table; with any FULL
        // present mpedb keeps the whole WHERE as a post-join filter (no
        // pushdown), so this checks that path against sqlite.
        q.push_str(" WHERE (t2.k IS NULL OR t2.k <> 4)");
    }
    // ORDER BY every column (qualified names, so `SELECT *` sorts too) → a total
    // order both engines agree on (NULLS FIRST ascending, binary text collation).
    q.push_str(&format!(" ORDER BY {}", cols.join(", ")));
    q
}

fn kind_seqs(n_joins: usize) -> Vec<Vec<K>> {
    let mut out = vec![vec![]];
    for _ in 0..n_joins {
        let mut next = Vec::new();
        for prefix in &out {
            for k in K::all() {
                let mut v = prefix.clone();
                v.push(k);
                next.push(v);
            }
        }
        out = next;
    }
    out
}

// ---- the exhaustive differential test ---------------------------------------

#[test]
fn full_join_chains_match_sqlite() {
    let pid = std::process::id();
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let mpath = format!("{dir}/mpedb-fjc-{pid}.mpedb");

    let db = open_mpedb(&mpath);
    let sddl = build_sqlite_ddl();

    let mut checked = 0usize;
    let mut refused_ok = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for n in [3usize, 4] {
        for kinds in kind_seqs(n - 1) {
            for pat in ["chain", "star", "onref"] {
                for with_where in [false, true] {
                    // WHERE only on the plain chain pattern to bound the count.
                    if with_where && pat != "chain" {
                        continue;
                    }
                    // Column-order dimensions: explicit columns always; also a
                    // `SELECT *` pass on the plain chain pattern (proves the
                    // RIGHT-swap column pinning and left-to-right ordering).
                    // And a leading-RIGHT pass whose trailing joins vary — the
                    // leading-RIGHT + FULL case that stays refused.
                    let star_variants: &[bool] =
                        if pat == "chain" && !with_where { &[false, true] } else { &[false] };
                    let right_variants: &[bool] =
                        if !with_where { &[false, true] } else { &[false] };
                    for &lead_right in right_variants {
                        for &star in star_variants {
                            // Contract: a trailing FULL after a leading RIGHT is
                            // the ONE refused shape; every other chain (FULL at
                            // any position, no leading RIGHT) must match sqlite.
                            let expect_refused = lead_right && kinds[1..].contains(&K::Full);
                            let q = build_query(n, &kinds, pat, with_where, lead_right, star);
                            let tag = format!(
                                "n={n} kinds={kinds:?} pat={pat} where={with_where} \
                                 right={lead_right} star={star}"
                            );
                            match run_mpedb(&db, &q) {
                                Ok(got) => {
                                    if expect_refused {
                                        failures.push(format!(
                                            "EXPECTED REFUSAL but answered [{tag}]\n  SQL: {q}\n  \
                                             mpedb: {got:?}"
                                        ));
                                        continue;
                                    }
                                    checked += 1;
                                    let want = run_sqlite(&sddl, &q);
                                    if got != want {
                                        failures.push(format!(
                                            "DIVERGED [{tag}]\n  SQL: {q}\n  mpedb:  {got:?}\n  \
                                             sqlite: {want:?}"
                                        ));
                                    }
                                }
                                Err(e) => {
                                    if expect_refused {
                                        assert!(
                                            e.contains("FULL JOIN following a leading RIGHT"),
                                            "wrong refusal message [{tag}]: {e}"
                                        );
                                        refused_ok += 1;
                                    } else {
                                        failures.push(format!(
                                            "UNEXPECTED REFUSAL [{tag}]: {e}\n  SQL: {q}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(&mpath);
    let _ = std::fs::remove_file(format!("{mpath}-wal"));

    eprintln!("matched {checked} shapes vs sqlite; {refused_ok} leading-RIGHT+FULL cleanly refused");
    assert!(checked > 250, "too few shapes exercised: {checked}");
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("{f}");
        }
        panic!("{} FULL-join chain shape(s) failed the sqlite contract", failures.len());
    }
}
