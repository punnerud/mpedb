//! **Column pruning must be invisible** (#125): the differential battery.
//!
//! `mpedb_sql::row_prune` computes, per plan, which slots of the row pipeline
//! any later stage can observe; `exec/gather.rs` then stops carrying the rest.
//! Dropping a column changes nothing observable — that is the entire safety
//! argument for the feature — so the only way it can be wrong is by MISSING a
//! consumer, and a missed consumer is a wrong answer, not a slower one.
//!
//! This file is where that claim is checked, against the BUNDLED sqlite
//! oracle (3.45.0, compiled in), on **value and `typeof()`**. The `typeof()`
//! half is not decoration: a pruned column that was feeding a comparison, a
//! collation or a NULL-propagating expression tends to come back as NULL, and
//! `NULL` compares equal to `NULL` in more places than it should. Asking for
//! the storage class as well catches the shape where the value happens to
//! coincide.
//!
//! The battery is organised by CONSUMER, because a consumer is exactly what
//! the analysis can forget:
//!
//! | consumer | why it is easy to miss |
//! |---|---|
//! | a join's `ON` | reads the tuple accumulated through it, not the base row |
//! | an index nested loop's `KeyPart::OuterCol` | names an outer slot with no expression around it |
//! | `joined_filter` (WHERE over the joined row) | a different program from `filter` |
//! | ORDER BY over the base/joined tuple | `cmp_rows` SKIPS an out-of-range key instead of failing |
//! | ORDER BY junk columns | trailing projection entries nobody selected |
//! | DISTINCT under a declared collation | folds through the OUTPUT column's collation |
//! | `agg(x) FILTER (WHERE …)` | a per-aggregate program, separate from the argument |
//! | sqlite bare columns | a witness row, read positionally |
//! | the bare-column witness's PK | read off the base row by NO expression at all |
//! | a correlated subplan's `outer_args` | filled per row, named by index |
//! | LEFT/FULL NULL-extension | rows that exist because nothing matched |
//!
//! The two shapes #125 exists to fix — an aggregate over a join, and an
//! aggregate under a correlated `FILTER` — are in here twice: once for the
//! answer, and once (in `prune_width_mem.rs`) for the bytes.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

// --------------------------------------------------------------- the fixture

/// Deliberately WIDE, and with the interesting columns spread across the row:
/// pruning is a claim about which SLOTS survive, so a fixture whose every
/// column is read proves nothing. `s` sits between two integers so a hole in
/// the middle of the tuple is exercised as well as a truncated tail; `u` is
/// NOCASE so a dropped collation shows up as a dedup/ordering difference;
/// `pad` is the column nothing ever reads.
const CREATE_T: &str = "CREATE TABLE t (\
     id INTEGER PRIMARY KEY, g INTEGER, x INTEGER, s TEXT, \
     u TEXT COLLATE NOCASE, r REAL, pad TEXT)";
const CREATE_C: &str =
    "CREATE TABLE c (cid INTEGER PRIMARY KEY, ref INTEGER, w INTEGER, lbl TEXT, junk TEXT)";
const CREATE_D: &str = "CREATE TABLE d (did INTEGER PRIMARY KEY, k INTEGER, note TEXT)";
/// An index on `c.ref` is what turns the nested loop into the INDEX nested
/// loop, whose access path names the outer row through `KeyPart::OuterCol` —
/// a base-row read with no expression around it for the analysis to find.
const CREATE_IX: &str = "CREATE INDEX c_ref ON c (ref)";
/// An index on `t.x` gives a scan whose order is NOT the primary key's. That
/// is the only way the bare-column witness's PK read becomes observable: over
/// a PK-ordered scan the group's first row IS its lowest-rowid row, so a
/// dropped primary key coincides with the right answer and proves nothing.
const CREATE_IX_X: &str = "CREATE INDEX t_x ON t (x)";

/// One `t` row: `(id, g, x, s, u, r, pad)`.
type TRow = (i64, i64, Option<i64>, Option<&'static str>, Option<&'static str>, Option<f64>, &'static str);

/// NULLs in `x`, `s` and `u` so 3VL, the NULL-skipping aggregates and the NULL
/// placement of ORDER BY are all live.
const T_ROWS: &[TRow] = &[
    (1, 0, Some(5), Some("alpha"), Some("Ab"), Some(1.5), "p1"),
    (2, 0, Some(9), Some("beta"), Some("aB"), Some(2.5), "p2"),
    (3, 0, None, Some("alpha"), Some("cd"), None, "p3"),
    (4, 1, Some(7), None, Some("CD"), Some(4.0), "p4"),
    (5, 1, Some(2), Some("gamma"), None, Some(-0.5), "p5"),
    (6, 1, Some(8), Some("delta"), Some("ef"), Some(6.25), "p6"),
    (7, 2, Some(100), Some("zeta"), Some("EF"), Some(7.0), "p7"),
];

/// `(cid, ref, w, lbl, junk)`. `ref` 99 is dangling (so an INNER join drops a
/// `c` row and a FULL join keeps it); `t` rows 5, 6 and 7 have no child (so a
/// LEFT join NULL-extends).
const C_ROWS: &[(i64, i64, i64, &str, &str)] = &[
    (1, 1, 10, "one", "j1"),
    (2, 1, 20, "uno", "j2"),
    (3, 2, 30, "two", "j3"),
    (4, 4, 40, "four", "j4"),
    (5, 99, 50, "ghost", "j5"),
];

/// `(did, k, note)` — the third table, so the left-deep loop has a stage whose
/// tuple is neither the first nor the last.
const D_ROWS: &[(i64, i64, &str)] = &[(1, 0, "zero"), (2, 1, "one"), (3, 5, "five")];

fn tlit(v: Option<&str>) -> String {
    v.map_or("NULL".to_string(), |s| format!("'{}'", s.replace('\'', "''")))
}

fn statements() -> Vec<String> {
    let mut out = vec![
        CREATE_T.to_string(),
        CREATE_C.to_string(),
        CREATE_D.to_string(),
        CREATE_IX.to_string(),
        CREATE_IX_X.to_string(),
    ];
    for (id, g, x, s, u, r, pad) in T_ROWS {
        out.push(format!(
            "INSERT INTO t (id, g, x, s, u, r, pad) VALUES ({id}, {g}, {}, {}, {}, {}, '{pad}')",
            x.map_or("NULL".into(), |v| v.to_string()),
            tlit(*s),
            tlit(*u),
            r.map_or("NULL".to_string(), |v| format!("{v:?}")),
        ));
    }
    for (cid, r, w, lbl, junk) in C_ROWS {
        out.push(format!(
            "INSERT INTO c (cid, ref, w, lbl, junk) VALUES ({cid}, {r}, {w}, '{lbl}', '{junk}')"
        ));
    }
    for (did, k, note) in D_ROWS {
        out.push(format!("INSERT INTO d (did, k, note) VALUES ({did}, {k}, '{note}')"));
    }
    out
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-prune-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // Default dialect: sqlite-lenient, so a bare column under GROUP BY is
    // legal — which is what puts the bare-column WITNESS (and therefore the
    // primary-key pin) under test at all.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for s in statements() {
        db.query(&s, &[]).unwrap();
    }
    Tmp { db, path }
}

// ------------------------------------------------------------- the comparison

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for s in statements() {
        script.push_str(&s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

fn cell_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s.parse::<i64>().map(|y| y == *i).unwrap_or(false),
        Value::Float(x) => match s.parse::<f64>() {
            Ok(y) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            Err(_) => false,
        },
        Value::Bool(b) => s == if *b { "1" } else { "0" },
        Value::Text(t) => s == t,
        Value::Blob(b) => s.as_bytes() == &b[..],
        other => panic!("unexpected value type: {other:?}"),
    }
}

/// mpedb and the bundled oracle must agree, row for row and cell for cell.
fn agree(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m.len(), s.len(), "row count differs for `{query}`:\n  mpedb {m:?}\n  sqlite {s:?}");
    for (mr, sr) in m.iter().zip(&s) {
        assert_eq!(
            mr.len(),
            sr.len(),
            "column count differs for `{query}`:\n  mpedb {mr:?}\n  sqlite {sr:?}"
        );
        for (mv, sv) in mr.iter().zip(sr) {
            assert!(
                cell_matches(mv, sv),
                "cell mismatch for `{query}`: mpedb {mv:?} vs sqlite {sv:?}\n  \
                 mpedb row {mr:?}\n  sqlite row {sr:?}"
            );
        }
    }
}

/// Every query, twice: as written, and with every output column wrapped in
/// `typeof(…)`. A dropped column usually returns as NULL rather than as
/// garbage, and NULL is a plausible-looking answer — the storage class is what
/// makes that visible.
fn agree_with_types(db: &Database, projection: &str, rest: &str) {
    agree(db, &format!("SELECT {projection} {rest}"));
    let typed: Vec<String> = split_top_level(projection)
        .into_iter()
        .map(|c| format!("typeof({})", c.trim()))
        .collect();
    agree(db, &format!("SELECT {} {rest}", typed.join(", ")));
}

/// Split a SELECT list on commas that are not inside parentheses or quotes —
/// enough for the expressions in this file, and it keeps every case readable
/// as the SQL it is rather than as two hand-maintained strings.
fn split_top_level(list: &str) -> Vec<&str> {
    let (mut depth, mut quoted, mut start) = (0i32, false, 0usize);
    let mut out = Vec::new();
    for (i, ch) in list.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&list[start..]);
    out
}

// -------------------------------------------------------------- the batteries

/// The headline shape: an aggregate over a join observes NO column of either
/// table beyond the ON, so the retained product is a set of empty rows.
#[test]
fn aggregate_over_a_join_matches_sqlite() {
    let d = db();
    for q in [
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id",
        "SELECT count(*) FROM t, c WHERE c.ref = t.id",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id",
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id JOIN d ON d.k = t.g",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id LEFT JOIN d ON d.k = t.g",
        // A residual WHERE over the JOINED row: `joined_filter`, a program the
        // analysis must walk separately from `filter`. Under a LEFT join the
        // two cannot be conflated — the WHERE runs AFTER the NULL extension,
        // which is what keeps this conjunct out of the ON.
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id WHERE c.w > 15",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id WHERE c.w > 15",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id WHERE c.junk IS NULL",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id WHERE c.lbl > 'p'",
        "SELECT t.id FROM t LEFT JOIN c ON c.ref = t.id WHERE c.w > 15 ORDER BY t.id",
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id WHERE t.s LIKE 'a%'",
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id WHERE t.u = 'AB'",
        // Aggregates that DO name columns, on either side of the join.
        "SELECT sum(c.w), avg(t.x), min(t.s), max(c.lbl) FROM t JOIN c ON c.ref = t.id",
        "SELECT count(t.x), count(c.junk), total(t.r) FROM t LEFT JOIN c ON c.ref = t.id",
        "SELECT count(DISTINCT t.g), count(DISTINCT c.lbl) FROM t JOIN c ON c.ref = t.id",
        // Per-aggregate FILTER, reading a column NOTHING else reads.
        "SELECT count(*) FILTER (WHERE c.junk = 'j1') FROM t JOIN c ON c.ref = t.id",
        "SELECT sum(c.w) FILTER (WHERE t.pad = 'p1') FROM t JOIN c ON c.ref = t.id",
        // GROUP BY + HAVING over the joined tuple.
        "SELECT t.g, count(*) FROM t JOIN c ON c.ref = t.id GROUP BY t.g ORDER BY t.g",
        "SELECT t.g, sum(c.w) FROM t JOIN c ON c.ref = t.id GROUP BY t.g HAVING count(*) > 1 \
         ORDER BY t.g",
        "SELECT t.u, count(*) FROM t JOIN c ON c.ref = t.id GROUP BY t.u ORDER BY t.u",
        // A computed group key, over a column the projection never names.
        "SELECT t.x + c.w, count(*) FROM t JOIN c ON c.ref = t.id GROUP BY t.x + c.w \
         ORDER BY 1",
    ] {
        agree(&d, q);
    }
}

/// The other flat shape: a correlated `FILTER`/subquery keeps a per-row
/// scratch beside every gathered row, and its `outer_args` name base-row slots
/// with no expression around them.
#[test]
fn correlated_shapes_match_sqlite() {
    let d = db();
    for q in [
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) FROM t",
        "SELECT count(*) FILTER (WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) FROM t",
        "SELECT sum(t.x) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.g)) FROM t",
        "SELECT t.g, count(*) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) \
         FROM t GROUP BY t.g ORDER BY t.g",
        // A correlated scalar subquery in the SELECT list: the projection reads
        // the correlated slot, the correlation reads the base row.
        "SELECT t.id, (SELECT max(c.w) FROM c WHERE c.ref = t.id) FROM t ORDER BY t.id",
        "SELECT t.s, (SELECT count(*) FROM c WHERE c.ref = t.id) FROM t ORDER BY t.id",
        // Correlated WHERE residual: the `post_filter`.
        "SELECT t.id FROM t WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id) ORDER BY t.id",
        "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)",
        "SELECT t.s FROM t WHERE t.x > (SELECT avg(c.w) FROM c WHERE c.ref = t.id) \
         ORDER BY t.id",
        // Correlated on a column the projection never mentions.
        "SELECT t.pad FROM t WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.g) ORDER BY t.id",
    ] {
        agree(&d, q);
    }
}

/// sqlite "bare columns": each group carries values from a WITNESS row, and
/// with no `min`/`max` to govern the pick that row is the group's LOWEST
/// ROWID — read off the base row's primary key by no expression at all. It is
/// the one base-row read `row_prune` has to know about rather than discover.
#[test]
fn bare_column_witness_matches_sqlite() {
    let d = db();
    for q in [
        // No min/max ⇒ the lowest-rowid witness ⇒ the PK must survive pruning.
        "SELECT t.s, count(*) FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.pad, t.u, count(*) FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.s FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.s, sum(t.x) FROM t GROUP BY t.g ORDER BY t.g",
        // Exactly one min/max ⇒ that extremum's row governs instead.
        "SELECT t.s, max(t.x) FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.pad, min(t.x), count(*) FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.s, max(t.x) FILTER (WHERE t.x < 8) FROM t GROUP BY t.g ORDER BY t.g",
        // Scalar (one-group) forms.
        "SELECT t.s, max(t.x) FROM t",
        "SELECT t.pad, count(*) FROM t",
    ] {
        agree(&d, q);
    }
}

/// The witness read that a PK-ordered scan HIDES — and the only case in this
/// file the oracle cannot arbitrate.
///
/// `exec/aggregate.rs` states the rule for a group with no `min`/`max` to
/// govern it: the witness is the group's LOWEST-ROWID row, tracked as a
/// running minimum "even when the scan is NOT PK-ordered (an index or
/// descending-range access path)". That makes the answer a function of the
/// data alone, and NOT of which access path the planner picked — so the two
/// spellings below, which select exactly the same rows over the same table,
/// must answer identically although one takes `t_x` and the other a full scan.
///
/// It is also the ONLY assertion in this file that exercises the primary-key
/// pin in `row_prune`: a full scan hands each group its lowest-rowid row
/// FIRST, so with the pin deleted the wrong rule silently agrees with the
/// right one. Fault-injected — removing the `outer_pk` pin makes the indexed
/// spelling answer `p5` where the scan answers `p4`.
///
/// **Not differentialled against sqlite, deliberately.** Under the indexed
/// access sqlite 3.45 answers `p5` here: its "arbitrary" bare-column pick
/// follows the SCAN, not the rowid, so mpedb's documented min-PK rule and
/// sqlite's disagree the moment the scan order stops being the PK's. That
/// divergence predates #125 (verified with pruning disabled entirely) and is
/// a bare-column question, not a width one.
#[test]
fn the_bare_column_witness_does_not_depend_on_the_access_path() {
    let d = db();
    for (indexed, scanned) in [
        (
            "SELECT t.pad, count(*) FROM t WHERE t.x > 0 GROUP BY t.g ORDER BY t.g",
            "SELECT t.pad, count(*) FROM t WHERE t.x + 0 > 0 GROUP BY t.g ORDER BY t.g",
        ),
        (
            "SELECT t.s, t.pad, count(*) FROM t WHERE t.x > 0 GROUP BY t.g ORDER BY t.g",
            "SELECT t.s, t.pad, count(*) FROM t WHERE t.x + 0 > 0 GROUP BY t.g ORDER BY t.g",
        ),
        (
            "SELECT t.pad FROM t WHERE t.x > 0 GROUP BY t.g ORDER BY t.g",
            "SELECT t.pad FROM t WHERE t.x + 0 > 0 GROUP BY t.g ORDER BY t.g",
        ),
    ] {
        assert_eq!(
            mpedb_rows(&d, indexed),
            mpedb_rows(&d, scanned),
            "the bare-column witness moved with the access path:\n  {indexed}\n  {scanned}"
        );
    }
}

/// A join's product is what feeds ORDER BY, DISTINCT and the projection, and
/// each indexes a different tuple. `cmp_rows` silently SKIPS a sort key it
/// cannot find, so a dropped ORDER BY column reorders rather than fails.
#[test]
fn join_projection_ordering_and_distinct_match_sqlite() {
    let d = db();
    for (proj, rest) in [
        ("t.id, c.lbl", "FROM t JOIN c ON c.ref = t.id ORDER BY t.id, c.cid"),
        // Sorted by a column NOTHING projects — the junk-column path.
        ("c.lbl", "FROM t JOIN c ON c.ref = t.id ORDER BY t.s, c.w"),
        ("c.lbl", "FROM t JOIN c ON c.ref = t.id ORDER BY t.r DESC, c.cid"),
        // A collated sort key that is not in the output.
        ("t.id", "FROM t JOIN c ON c.ref = t.id ORDER BY t.u, t.id"),
        // LEFT join: rows that exist because NOTHING matched, so every inner
        // slot is the NULL extension.
        ("t.id, c.lbl, c.junk", "FROM t LEFT JOIN c ON c.ref = t.id ORDER BY t.id, c.cid"),
        ("t.s, c.w", "FROM t LEFT JOIN c ON c.ref = t.id ORDER BY t.id, c.cid"),
        // Three-way, so a middle stage's tuple is narrowed too.
        (
            "t.s, c.lbl, d.note",
            "FROM t JOIN c ON c.ref = t.id JOIN d ON d.k = t.g ORDER BY t.id, c.cid, d.did",
        ),
        (
            "d.note",
            "FROM t JOIN c ON c.ref = t.id JOIN d ON d.k = t.g ORDER BY t.id, c.cid, d.did",
        ),
        // An expression over columns from both sides.
        (
            "t.x * c.w, t.s || '-' || c.lbl",
            "FROM t JOIN c ON c.ref = t.id ORDER BY t.id, c.cid",
        ),
        // LIMIT/OFFSET over a join (they bound JOINED rows).
        ("t.id, c.lbl", "FROM t JOIN c ON c.ref = t.id ORDER BY t.id, c.cid LIMIT 2 OFFSET 1"),
    ] {
        agree_with_types(&d, proj, rest);
    }
    // DISTINCT separately: the keyword is not part of the SELECT list, so it
    // cannot ride the `typeof(…)` wrapper. It deduplicates the PROJECTION,
    // under each output column's DECLARED collation — `t.u` is NOCASE, so
    // `'Ab'`/`'aB'` are one value and a dropped collation would show up as two.
    for q in [
        "SELECT DISTINCT t.g FROM t JOIN c ON c.ref = t.id ORDER BY 1",
        "SELECT DISTINCT t.u FROM t JOIN c ON c.ref = t.id ORDER BY 1",
        "SELECT DISTINCT t.g, c.w > 25 FROM t JOIN c ON c.ref = t.id ORDER BY 1, 2",
        "SELECT DISTINCT typeof(t.x) FROM t LEFT JOIN c ON c.ref = t.id ORDER BY 1",
    ] {
        agree(&d, q);
    }
}

/// The INDEX nested loop: `c_ref` turns the inner access into a per-outer-row
/// probe whose key comes from `KeyPart::OuterCol` — an outer-slot read that no
/// expression names. The queries are the same ones as the held-inner battery,
/// which is the point: the answer may not depend on which access path won.
#[test]
fn index_nested_loop_matches_sqlite() {
    let d = db();
    for q in [
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id",
        "SELECT count(*) FROM t JOIN c ON c.ref = t.g",
        "SELECT t.id, c.lbl FROM t JOIN c ON c.ref = t.id ORDER BY t.id, c.cid",
        "SELECT c.lbl FROM t JOIN c ON c.ref = t.id ORDER BY t.s, c.w",
        "SELECT sum(c.w) FROM t JOIN c ON c.ref = t.id",
        "SELECT count(*) FROM t LEFT JOIN c ON c.ref = t.id",
        "SELECT t.g, count(*) FROM t JOIN c ON c.ref = t.id GROUP BY t.g ORDER BY t.g",
    ] {
        agree(&d, q);
    }
    // …and the plan really is the index nested loop, or this test is a
    // duplicate of the one above wearing a different name.
    let plan = match d.query("EXPLAIN SELECT count(*) FROM t JOIN c ON c.ref = t.id", &[]).unwrap()
    {
        ExecResult::Explain(e) => e,
        other => panic!("{other:?}"),
    };
    assert!(
        plan.contains("c_ref") || plan.to_lowercase().contains("index"),
        "expected an index nested loop over c_ref:\n{plan}"
    );
}

/// Single-table shapes on the pruned paths: the correlated gather and the
/// materialising aggregate. Nothing here has a join, so it is the base row
/// itself that gets narrowed.
#[test]
fn single_table_pruned_paths_match_sqlite() {
    let d = db();
    for q in [
        "SELECT count(*) FROM t",
        "SELECT count(*) FROM t WHERE x > 4",
        "SELECT sum(x), avg(r), min(s), max(u) FROM t",
        "SELECT g, count(*), sum(x) FROM t GROUP BY g ORDER BY g",
        "SELECT u, count(*) FROM t GROUP BY u ORDER BY u",
        "SELECT count(*) FILTER (WHERE s LIKE '%a') FROM t",
        "SELECT count(DISTINCT u) FROM t",
        "SELECT g, count(*) FROM t GROUP BY g HAVING sum(x) > 10 ORDER BY g",
        "SELECT g, count(*) FROM t GROUP BY g ORDER BY count(*) DESC, g",
    ] {
        agree(&d, q);
    }
}

/// The same battery inside a WRITE session, where `TxnCtx::scans_incrementally`
/// is false and every aggregate takes the MATERIALISING path — the path this
/// change narrows. The two must give the same answers as each other and as
/// sqlite, which is what makes the pruning independent of execution context.
#[test]
fn pruned_paths_agree_in_a_write_session() {
    let d = db();
    for q in [
        "SELECT count(*) FROM t JOIN c ON c.ref = t.id",
        "SELECT sum(c.w) FROM t JOIN c ON c.ref = t.id",
        "SELECT count(*) FROM t",
        "SELECT t.s, count(*) FROM t GROUP BY t.g ORDER BY t.g",
        "SELECT t.g, sum(c.w) FROM t JOIN c ON c.ref = t.id GROUP BY t.g ORDER BY t.g",
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM c WHERE c.ref = t.id)) FROM t",
    ] {
        let read = mpedb_rows(&d, q);
        let mut w = d.begin().unwrap();
        let written = match w.query(q, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        };
        w.rollback();
        assert_eq!(read, written, "read and write contexts disagree on `{q}`");
        agree(&d, q);
    }
}

/// Pruning must not change which statements the runtime budget REFUSES.
/// `max_join_cells` is a deterministic tripwire with a tested trip point; it
/// prices the product the join LOGICALLY forms, and a width optimisation that
/// silently raised the ceiling would be an observable change dressed up as an
/// invisible one.
#[test]
fn the_join_budget_still_prices_the_logical_product() {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-prune-budget-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // 7 `t` rows × 7 columns = 49 cells for the outer side alone, so a budget
    // of 20 must refuse even `count(*)`, whose retained rows are EMPTY.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = 20\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for s in statements() {
        db.query(&s, &[]).unwrap();
    }
    let e = db
        .query("SELECT count(*) FROM t JOIN c ON c.ref = t.id", &[])
        .expect_err("a 20-cell budget must still refuse this join");
    let msg = e.to_string();
    assert!(msg.contains("max_join_cells"), "the message must name the knob: {msg}");
    let _ = std::fs::remove_file(&path);
}
