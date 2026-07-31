//! FOREIGN KEY enforcement, differentially against the BUNDLED sqlite (#194).
//!
//! Every case here runs the SAME script through both engines with enforcement
//! ON and compares the surviving rows — or, when the statement must fail,
//! compares WHICH statement failed. sqlite is the specification; nothing in
//! this file asserts against a second copy of mpedb's own reasoning.
//!
//! The default-OFF behaviour is pinned separately, in
//! `django_parse_gaps.rs::references_is_parsed_and_not_enforced_like_sqlite_default`.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

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
    }
}

/// A fresh database with `PRAGMA foreign_keys = ON` already in force.
fn open() -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-fk-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [compat]\nforeign_keys = true\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    assert!(db.fk_enforced(), "[compat] foreign_keys must reach the handle");
    Tmp { db, path }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i32).to_string(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb `{sql}` failed: {e}"),
    }
}

/// Run `setup` statement by statement, stopping at the FIRST failure. Returns
/// the index of the statement that failed (and its message), or `None` when the
/// whole script ran. This is the comparable outcome: sqlite's CLI in `.bail on`
/// mode reports exactly the same thing.
fn mpedb_run(db: &Database, setup: &[&str]) -> Option<(usize, String)> {
    for (i, s) in setup.iter().enumerate() {
        if let Err(e) = db.query(s, &[]) {
            return Some((i, e.to_string()));
        }
    }
    None
}

fn sqlite_run(setup: &[&str]) -> Option<(usize, String)> {
    // Each statement on its own, so a failure names the statement rather than
    // the script — `try_script_stdout` bails at the first error but does not
    // say which one it was.
    let mut script = String::from("PRAGMA foreign_keys = ON;\n");
    for (i, s) in setup.iter().enumerate() {
        let mut probe = script.clone();
        probe.push_str(s);
        probe.push_str(";\n");
        if let Err(e) = sqlite_oracle::try_script_stdout(&probe, "") {
            return Some((i, e));
        }
        script.push_str(s);
        script.push_str(";\n");
    }
    None
}

fn sqlite_rows(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut script = String::from("PRAGMA foreign_keys = ON;\n");
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    let out = sqlite_oracle::try_script_stdout(&script, "")
        .unwrap_or_else(|e| panic!("sqlite3 failed: {e}\nscript:\n{script}"));
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// The whole script must run in BOTH engines, and the query must agree.
fn assert_same(setup: &[&str], query: &str) {
    let t = open();
    if let Some((i, e)) = mpedb_run(&t.db, setup) {
        panic!("mpedb refused `{}`: {e}", setup[i]);
    }
    assert_eq!(sqlite_run(setup), None, "sqlite refused a statement it should accept");
    assert_eq!(
        mpedb_rows(&t.db, query),
        sqlite_rows(setup, query),
        "mpedb vs sqlite diverged for:\n{setup:?}\n{query}"
    );
}

/// Both engines must REFUSE, and refuse at the same statement.
fn assert_both_refuse(setup: &[&str]) {
    let t = open();
    let m = mpedb_run(&t.db, setup);
    let s = sqlite_run(setup);
    let (mi, me) = m.unwrap_or_else(|| panic!("mpedb accepted the whole script:\n{setup:?}"));
    let (si, _) = s.unwrap_or_else(|| panic!("sqlite accepted the whole script:\n{setup:?}"));
    assert_eq!(
        mi, si,
        "the two engines failed at DIFFERENT statements: mpedb at `{}` ({me}), sqlite at `{}`",
        setup[mi], setup[si]
    );
}

// -------------------------------------------------------------- the child side

#[test]
fn a_dangling_child_is_refused() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
        "INSERT INTO p (id) VALUES (1)",
        "INSERT INTO c (id, p) VALUES (1, 99)",
    ]);
}

#[test]
fn a_child_naming_a_live_parent_goes_in() {
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
            "INSERT INTO p (id) VALUES (1)",
            "INSERT INTO c (id, p) VALUES (1, 1)",
        ],
        "SELECT id, p FROM c ORDER BY id",
    );
}

/// MATCH SIMPLE: a NULL key member means the row is not checked at all. This is
/// the rule a reasonable person gets wrong — a PARTIALLY null composite key is
/// also unchecked.
#[test]
fn a_null_key_member_is_never_checked() {
    assert_same(
        &[
            "CREATE TABLE p (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, \
              FOREIGN KEY (x, y) REFERENCES p (a, b))",
            "INSERT INTO c (id, x, y) VALUES (1, 1, NULL)",
            "INSERT INTO c (id, x, y) VALUES (2, NULL, NULL)",
        ],
        "SELECT id, x, y FROM c ORDER BY id",
    );
    // …but a FULLY non-NULL key with no parent is refused.
    assert_both_refuse(&[
        "CREATE TABLE p (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, \
          FOREIGN KEY (x, y) REFERENCES p (a, b))",
        "INSERT INTO c (id, x, y) VALUES (3, 1, 2)",
    ]);
}

#[test]
fn an_update_that_breaks_the_key_is_refused_and_null_is_allowed() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
        "INSERT INTO p (id) VALUES (1)",
        "INSERT INTO c (id, p) VALUES (1, 1)",
        "UPDATE c SET p = 9",
    ]);
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
            "INSERT INTO p (id) VALUES (1)",
            "INSERT INTO c (id, p) VALUES (1, 1)",
            "UPDATE c SET p = NULL",
        ],
        "SELECT id, p FROM c",
    );
}

/// A parent key that is neither the PRIMARY KEY nor a UNIQUE index is a
/// WRITE-time error in sqlite, not a `CREATE TABLE` one.
#[test]
fn a_non_unique_parent_key_fails_at_the_write_not_at_the_ddl() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY, x INTEGER)",
        // Accepted by both: the DDL is fine, the key is not.
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (x))",
        "INSERT INTO p (id, x) VALUES (1, 7)",
        "INSERT INTO c (id, p) VALUES (1, 7)",
    ]);
}

#[test]
fn a_unique_index_is_a_legal_parent_key() {
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT REFERENCES p (code))",
            "INSERT INTO p (id, code) VALUES (1, 'a')",
            "INSERT INTO c (id, code) VALUES (1, 'a')",
        ],
        "SELECT id, code FROM c",
    );
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT REFERENCES p (code))",
        "INSERT INTO p (id, code) VALUES (1, 'a')",
        "INSERT INTO c (id, code) VALUES (2, 'b')",
    ]);
}

/// A forward reference is legal DDL, and the write is what fails.
#[test]
fn a_forward_reference_is_legal_ddl() {
    assert_same(
        &[
            "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES par (id))",
            "CREATE TABLE par (id INTEGER PRIMARY KEY)",
            "INSERT INTO par (id) VALUES (5)",
            "INSERT INTO c (id, p) VALUES (1, 5)",
        ],
        "SELECT id, p FROM c",
    );
}

// ------------------------------------------------------------- the parent side

#[test]
fn deleting_a_referenced_parent_is_refused() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
        "INSERT INTO p (id) VALUES (1)",
        "INSERT INTO c (id, p) VALUES (1, 1)",
        "DELETE FROM p WHERE id = 1",
    ]);
}

#[test]
fn on_delete_cascade_and_set_null_do_what_they_say() {
    let setup: &[&str] = &[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id) ON DELETE CASCADE)",
        "CREATE TABLE d (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id) ON DELETE SET NULL)",
        "INSERT INTO p (id) VALUES (1)",
        "INSERT INTO p (id) VALUES (2)",
        "INSERT INTO c (id, p) VALUES (10, 1)",
        "INSERT INTO c (id, p) VALUES (11, 2)",
        "INSERT INTO d (id, p) VALUES (20, 1)",
        "INSERT INTO d (id, p) VALUES (21, 2)",
        "DELETE FROM p WHERE id = 1",
    ];
    assert_same(setup, "SELECT id, p FROM c ORDER BY id");
    assert_same(setup, "SELECT id, p FROM d ORDER BY id");
}

/// `ON UPDATE CASCADE` over a UNIQUE parent key. The key is NOT the primary
/// key here, and that is not incidental: mpedb refuses `UPDATE t SET <pk> = …`
/// outright ("cannot update primary key column"), a limitation that predates
/// foreign keys and is tracked separately — so a PK-keyed cascade is
/// unreachable rather than wrong, and this is the shape that exercises the
/// carry.
#[test]
fn on_update_cascade_carries_the_new_key() {
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, \
              code TEXT REFERENCES p (code) ON UPDATE CASCADE)",
            "INSERT INTO p (id, code) VALUES (1, 'a')",
            "INSERT INTO c (id, code) VALUES (10, 'a')",
            "UPDATE p SET code = 'b' WHERE id = 1",
        ],
        "SELECT id, code FROM c",
    );
}

/// A self-referencing cascade walks the whole chain — the case that proves the
/// recursion is real and not one level deep.
#[test]
fn a_self_referencing_cascade_empties_the_chain() {
    assert_same(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, up INTEGER REFERENCES t (id) ON DELETE CASCADE)",
            "INSERT INTO t (id, up) VALUES (1, NULL)",
            "INSERT INTO t (id, up) VALUES (2, 1)",
            "INSERT INTO t (id, up) VALUES (3, 2)",
            "DELETE FROM t WHERE id = 1",
        ],
        "SELECT id, up FROM t ORDER BY id",
    );
}

#[test]
fn a_self_reference_refuses_a_missing_parent_and_a_still_referenced_one() {
    assert_both_refuse(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, boss INTEGER REFERENCES t (id))",
        "INSERT INTO t (id, boss) VALUES (1, NULL)",
        "INSERT INTO t (id, boss) VALUES (3, 99)",
    ]);
    assert_both_refuse(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, boss INTEGER REFERENCES t (id))",
        "INSERT INTO t (id, boss) VALUES (1, NULL)",
        "INSERT INTO t (id, boss) VALUES (2, 1)",
        "DELETE FROM t WHERE id = 1",
    ]);
}

/// An UPDATE that leaves the key alone is not a foreign-key event: the children
/// must not be touched, and a RESTRICT parent must not refuse.
#[test]
fn updating_a_non_key_column_of_a_parent_is_not_a_key_event() {
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY, note TEXT)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
            "INSERT INTO p (id, note) VALUES (1, 'a')",
            "INSERT INTO c (id, p) VALUES (10, 1)",
            "UPDATE p SET note = 'b' WHERE id = 1",
        ],
        "SELECT id, p FROM c",
    );
}

// -------------------------------------------------------------------- deferred

#[test]
fn a_deferred_key_is_satisfied_before_commit() {
    let t = open();
    let mut s = t.db.begin().unwrap();
    s.query(
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        &[],
    )
    .unwrap();
    s.query(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, \
          p INTEGER REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED)",
        &[],
    )
    .unwrap();
    s.commit().unwrap();

    let mut s = t.db.begin().unwrap();
    // The child lands FIRST — an immediate key would have refused it here.
    s.query("INSERT INTO c (id, p) VALUES (1, 7)", &[]).unwrap();
    s.query("INSERT INTO p (id) VALUES (7)", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(mpedb_rows(&t.db, "SELECT id, p FROM c"), vec![vec!["1", "7"]]);

    // Same shape in sqlite.
    assert_eq!(
        sqlite_rows(
            &[
                "CREATE TABLE p (id INTEGER PRIMARY KEY)",
                "CREATE TABLE c (id INTEGER PRIMARY KEY, \
                  p INTEGER REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED)",
                "BEGIN",
                "INSERT INTO c (id, p) VALUES (1, 7)",
                "INSERT INTO p (id) VALUES (7)",
                "COMMIT",
            ],
            "SELECT id, p FROM c"
        ),
        vec![vec!["1", "7"]]
    );
}

#[test]
fn a_deferred_key_never_satisfied_fails_the_commit() {
    let t = open();
    let mut s = t.db.begin().unwrap();
    s.query("CREATE TABLE p (id INTEGER PRIMARY KEY)", &[]).unwrap();
    s.query(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, \
          p INTEGER REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED)",
        &[],
    )
    .unwrap();
    s.commit().unwrap();

    let mut s = t.db.begin().unwrap();
    s.query("INSERT INTO c (id, p) VALUES (1, 7)", &[]).unwrap();
    let e = s.commit().unwrap_err().to_string();
    assert!(e.contains("FOREIGN KEY"), "commit must name the key: {e}");
    // Nothing landed — the whole transaction rolled back.
    assert_eq!(mpedb_rows(&t.db, "SELECT COUNT(*) FROM c"), vec![vec!["0"]]);
}

/// Autocommit is a transaction too. A statement outside `BEGIN` has an IMPLICIT
/// commit, and a deferred key is settled there — Django's
/// `TestAsPrimaryKeyTransactionTests.test_unsaved_fk` is exactly this shape, and
/// it is a WRONG ANSWER when it is missed: the row is accepted, not refused.
#[test]
fn a_deferred_key_is_settled_by_the_implicit_commit_too() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, \
          p INTEGER REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED)",
        "INSERT INTO c (id, p) VALUES (1, 7)",
    ]);
}

// ------------------------------------------------------------- foreign_key_check

/// The audit a database filled with enforcement OFF needs — and the one Django's
/// migration runner calls.
#[test]
fn foreign_key_check_finds_what_was_written_with_enforcement_off() {
    let t = open();
    t.db.set_fk_enforced(false);
    t.db.query("CREATE TABLE p (id INTEGER PRIMARY KEY)", &[]).unwrap();
    t.db.query(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))",
        &[],
    )
    .unwrap();
    t.db.query("INSERT INTO c (id, p) VALUES (1, 42)", &[]).unwrap();
    let bad = t.db.foreign_key_check(None).unwrap();
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert_eq!(bad[0].0, "c");
    assert_eq!(bad[0].2, "p");
    // Naming the table narrows it; naming the OTHER table finds nothing.
    assert_eq!(t.db.foreign_key_check(Some("c")).unwrap().len(), 1);
    assert!(t.db.foreign_key_check(Some("p")).unwrap().is_empty());
    // Supplying the parent clears it.
    t.db.query("INSERT INTO p (id) VALUES (42)", &[]).unwrap();
    assert!(t.db.foreign_key_check(None).unwrap().is_empty());
}

/// A key-free schema must not pay for the machinery, and the flag must not
/// change any answer where there is no key.
#[test]
fn a_key_free_schema_behaves_identically_with_enforcement_on() {
    let setup: &[&str] = &[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO a (id, v) VALUES (1, 'x')",
        "INSERT INTO a (id, v) VALUES (2, 'y')",
        "DELETE FROM a WHERE id = 1",
        "UPDATE a SET v = 'z' WHERE id = 2",
    ];
    assert_same(setup, "SELECT id, v FROM a ORDER BY id");
}

// ------------------------------------------------- ordinals after a DDL -----

/// `ALTER TABLE … DROP COLUMN` must renumber the FOREIGN KEY's column ordinals,
/// not just the primary key's and the indexes'.
///
/// It did not, and the consequence was not a cosmetic pragma: `fk.rs` addresses
/// a row BY ORDINAL (`key_of(row, &fk.columns)`), so after a drop the check read
/// a DIFFERENT column — and when the stale ordinal fell past the end of the row,
/// enforcement stopped rejecting orphans ENTIRELY, with nothing in the output to
/// say so. A row with no parent went in and stayed.
#[test]
fn dropping_a_column_keeps_the_foreign_key_pointing_at_its_own_column() {
    // The FK column sits AFTER the dropped one, so its ordinal must move.
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, filler TEXT, pid INTEGER REFERENCES p(id))",
        "INSERT INTO p (id) VALUES (1)",
        "ALTER TABLE c DROP COLUMN filler",
        // No parent 999 — this must still be refused, on the RIGHT column.
        "INSERT INTO c (id, pid) VALUES (2, 999)",
    ]);
    // …and a live parent still goes in, i.e. the renumbering did not simply
    // break the check in the other direction.
    assert_same(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, filler TEXT, pid INTEGER REFERENCES p(id))",
            "INSERT INTO p (id) VALUES (1)",
            "ALTER TABLE c DROP COLUMN filler",
            "INSERT INTO c (id, pid) VALUES (2, 1)",
        ],
        "SELECT id, pid FROM c ORDER BY id",
    );
}

/// The same, for a COMPOSITE key across TWO drops — and with an index behind the
/// dropped columns, so all three ordinal lists move together.
#[test]
fn two_drops_keep_a_composite_key_and_its_indexes_aligned() {
    let base: &[&str] = &[
        "CREATE TABLE p (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, junk TEXT, more TEXT, \
         ca INTEGER, cb INTEGER, tag TEXT, FOREIGN KEY (ca, cb) REFERENCES p(a, b))",
        "CREATE INDEX c_tag ON c(tag)",
        "INSERT INTO p (a, b) VALUES (1, 2)",
        "ALTER TABLE c DROP COLUMN junk",
        "ALTER TABLE c DROP COLUMN more",
    ];
    let mut refuse = base.to_vec();
    refuse.push("INSERT INTO c (id, ca, cb, tag) VALUES (2, 9, 9, 'z')");
    assert_both_refuse(&refuse);

    let mut ok = base.to_vec();
    ok.push("INSERT INTO c (id, ca, cb, tag) VALUES (3, 1, 2, 'z')");
    // The index moved too: this probe reads `tag`, whose ordinal shifted by 2.
    assert_same(&ok, "SELECT id, ca, cb FROM c WHERE tag = 'z' ORDER BY id");
}

/// A column ADDED to an implicit-rowid table shifts the trailing rowid, and the
/// same three lists have to follow. The add path renumbered only the primary
/// key — an index or foreign key naming that rowid would have gone stale the
/// same way.
#[test]
fn adding_a_column_to_an_implicit_rowid_table_keeps_the_key_aligned() {
    assert_both_refuse(&[
        "CREATE TABLE p (id INTEGER PRIMARY KEY)",
        // No declared PK: #94's implicit rowid, which ADD COLUMN inserts before.
        "CREATE TABLE c (pid INTEGER REFERENCES p(id), note TEXT)",
        "INSERT INTO p (id) VALUES (1)",
        "ALTER TABLE c ADD COLUMN extra TEXT",
        "INSERT INTO c (pid, note) VALUES (999, 'x')",
    ]);
}
