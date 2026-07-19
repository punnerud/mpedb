//! SQL triggers (DESIGN-TRIGGERS, stages 0-3). Fires `BEFORE`/`AFTER` ×
//! `INSERT`/`UPDATE`/`DELETE FOR EACH ROW` triggers with a multi-statement SQL
//! body (`BEGIN <stmt>; … END`) and an optional `WHEN`, binding `NEW.<col>` (the
//! post-image) and `OLD.<col>` (the pre-image) per event. Cross-checked against
//! sqlite 3.45: an AFTER INSERT audit trigger, an AFTER UPDATE trigger logging
//! OLD+NEW, an AFTER DELETE trigger logging OLD, WHEN-gated ones (over OLD and
//! NEW), INSERT … SELECT bodies, a multi-statement AFTER INSERT body, an
//! `UPDATE OF <col>` trigger that fires/does-not-fire on the SET target, and a
//! BEFORE INSERT trigger that observes the pre-mutation table state — all
//! producing exactly the rows sqlite does. Also covers DROP TRIGGER, IF NOT
//! EXISTS / IF EXISTS, persistence across reopen, the recursion-depth guard, and
//! the named refusals (INSTEAD OF / FOR EACH STATEMENT / EXECUTE PROCEDURE /
//! OLD in INSERT / NEW in DELETE).

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn toml_for(path: &Path) -> String {
    format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    )
}

fn fresh_path(name: &str) -> PathBuf {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-trg-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn open(name: &str) -> (Database, PathBuf) {
    let path = fresh_path(name);
    let db = Database::open_with_config(Config::from_toml_str(&toml_for(&path)).unwrap()).unwrap();
    (db, path)
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn apply(db: &Database, stmts: &[&str]) {
    for s in stmts {
        db.query(s, &[]).unwrap_or_else(|e| panic!("`{s}` failed: {e}"));
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run a full script (schema + triggers + data) then one final query through the
/// `sqlite3` CLI, parsing its default list-mode output into rows.
fn sqlite_rows(setup: &[&str], final_query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(final_query);
    script.push_str(";\n");

    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Statements common to the audit cross-checks. `trigger` is the CREATE TRIGGER
/// text; `final_query` reads the audit table. Both engines must agree.
fn cross_check(trigger: &str, final_query: &str) {
    let setup: Vec<&str> = vec![
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, oid INTEGER, note TEXT)",
        trigger,
        "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
        "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
        "INSERT INTO orders (id, total, tag) VALUES (3, 250, 'c')",
    ];
    let (db, _p) = open("cross");
    apply(&db, &setup);
    let got = mpedb_rows(&db, final_query);
    let want = sqlite_rows(&setup, final_query);
    assert_eq!(got, want, "mpedb vs sqlite disagree on `{final_query}`");
    assert!(!got.is_empty(), "expected some audit rows");
}

#[test]
fn after_insert_audit_matches_sqlite() {
    cross_check(
        "CREATE TRIGGER audit_ins AFTER INSERT ON orders FOR EACH ROW \
         BEGIN INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.id, 'ins'); END",
        "SELECT id, oid, note FROM audit ORDER BY id",
    );
}

#[test]
fn when_gated_trigger_matches_sqlite() {
    // Only orders with total > 100 (ids 2 and 3) produce an audit row.
    cross_check(
        "CREATE TRIGGER audit_big AFTER INSERT ON orders FOR EACH ROW \
         WHEN (NEW.total > 100) \
         BEGIN INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.total, 'big'); END",
        "SELECT id, oid, note FROM audit ORDER BY id",
    );
}

#[test]
fn insert_select_body_matches_sqlite() {
    // A computed NEW expression via an INSERT … SELECT body reading the just-
    // inserted order row back out (NEW.id selects it).
    cross_check(
        "CREATE TRIGGER audit_calc AFTER INSERT ON orders FOR EACH ROW \
         BEGIN INSERT INTO audit (id, oid, note) \
               SELECT id, total * 2, tag FROM orders WHERE id = NEW.id; END",
        "SELECT id, oid, note FROM audit ORDER BY id",
    );
}

#[test]
fn drop_trigger_stops_firing() {
    let setup = &[
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, oid INTEGER, note TEXT)",
        "CREATE TRIGGER audit_ins AFTER INSERT ON orders FOR EACH ROW \
         BEGIN INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.id, 'ins'); END",
    ];
    let (db, _p) = open("drop");
    apply(&db, setup);
    apply(&db, &["INSERT INTO orders (id, total, tag) VALUES (1, 10, 'a')"]);
    assert_eq!(mpedb_rows(&db, "SELECT count(*) FROM audit"), vec![vec!["1"]]);

    apply(&db, &["DROP TRIGGER audit_ins"]);
    apply(&db, &["INSERT INTO orders (id, total, tag) VALUES (2, 20, 'b')"]);
    // The dropped trigger does not fire on the next statement — still one row.
    assert_eq!(mpedb_rows(&db, "SELECT count(*) FROM audit"), vec![vec!["1"]]);

    // DROP TRIGGER of a missing name errors, IF EXISTS is a no-op.
    assert!(db.query("DROP TRIGGER audit_ins", &[]).is_err());
    assert!(db.query("DROP TRIGGER IF EXISTS audit_ins", &[]).is_ok());
}

#[test]
fn if_not_exists_is_idempotent() {
    let (db, _p) = open("ine");
    apply(&db, &["CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)"]);
    let create = "CREATE TRIGGER t AFTER INSERT ON orders FOR EACH ROW \
                  BEGIN INSERT INTO orders (id, total, tag) VALUES (999, 0, 'x'); END";
    apply(&db, &[create]);
    // A second bare CREATE of the same name errors; IF NOT EXISTS is a no-op.
    assert!(db.query(create, &[]).is_err());
    let ine = "CREATE TRIGGER IF NOT EXISTS t AFTER INSERT ON orders FOR EACH ROW \
               BEGIN INSERT INTO orders (id, total, tag) VALUES (999, 0, 'x'); END";
    assert!(db.query(ine, &[]).is_ok());
}

#[test]
fn trigger_survives_reopen() {
    let path = fresh_path("reopen");
    let toml = toml_for(&path);
    {
        let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
        apply(
            &db,
            &[
                "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
                "CREATE TABLE audit (id INTEGER PRIMARY KEY, oid INTEGER, note TEXT)",
                "CREATE TRIGGER audit_ins AFTER INSERT ON orders FOR EACH ROW \
                 BEGIN INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.id, 'ins'); END",
                "INSERT INTO orders (id, total, tag) VALUES (1, 10, 'a')",
            ],
        );
        assert_eq!(mpedb_rows(&db, "SELECT count(*) FROM audit"), vec![vec!["1"]]);
    }
    // Reopen the same file: the trigger is read back from the catalog and still
    // fires on a fresh insert.
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    apply(&db, &["INSERT INTO orders (id, total, tag) VALUES (2, 20, 'b')"]);
    assert_eq!(
        mpedb_rows(&db, "SELECT id, oid FROM audit ORDER BY id"),
        vec![vec!["1", "1"], vec!["2", "2"]]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn recursion_depth_guard_aborts_and_rolls_back() {
    let (db, _p) = open("recur");
    apply(
        &db,
        &[
            "CREATE TABLE chain (id INTEGER PRIMARY KEY)",
            // Each insert fires the trigger, which inserts the NEXT id (read back
            // from the row just written): an unbounded self-cascade of distinct
            // keys, guarded only by the depth cap.
            "CREATE TRIGGER selfref AFTER INSERT ON chain FOR EACH ROW \
             BEGIN INSERT INTO chain (id) SELECT id + 1 FROM chain WHERE id = NEW.id; END",
        ],
    );
    let err = db.query("INSERT INTO chain (id) VALUES (1)", &[]).unwrap_err();
    assert!(
        format!("{err}").contains("recursion too deep"),
        "expected a depth-cap error, got: {err}"
    );
    // The whole autocommit statement (and its cascade) rolled back.
    assert_eq!(mpedb_rows(&db, "SELECT count(*) FROM chain"), vec![vec!["0"]]);
}

#[test]
fn stage3_named_refusals() {
    let (db, _p) = open("refuse");
    apply(&db, &["CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)"]);
    let body = "BEGIN INSERT INTO orders (id, total, tag) VALUES (9, 0, 'x'); END";
    // Grammar-level refusals.
    assert!(db.query("CREATE TRIGGER t INSTEAD OF INSERT ON orders BEGIN DELETE FROM orders; END", &[]).is_err());
    assert!(db
        .query(&format!("CREATE TRIGGER t AFTER INSERT ON orders FOR EACH STATEMENT {body}"), &[])
        .is_err());
    assert!(db.query("CREATE TRIGGER t AFTER INSERT ON orders EXECUTE PROCEDURE p(NEW.id)", &[]).is_err());
    // OLD is unavailable in an AFTER INSERT trigger.
    assert!(db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON orders FOR EACH ROW \
             BEGIN INSERT INTO orders (id, total, tag) VALUES (OLD.id, 0, 'x'); END",
            &[]
        )
        .is_err());
    // NEW is unavailable in an AFTER DELETE trigger.
    assert!(db
        .query(
            "CREATE TRIGGER t AFTER DELETE ON orders FOR EACH ROW \
             BEGIN INSERT INTO orders (id, total, tag) VALUES (NEW.id, 0, 'x'); END",
            &[]
        )
        .is_err());
    // A body referencing a missing NEW column is refused at CREATE (define-time).
    assert!(db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON orders FOR EACH ROW \
             BEGIN INSERT INTO orders (id, total, tag) VALUES (NEW.nope, 0, 'x'); END",
            &[]
        )
        .is_err());
    // No trigger was actually stored by any of the above.
    apply(&db, &["INSERT INTO orders (id, total, tag) VALUES (1, 1, 'a')"]);
    assert_eq!(mpedb_rows(&db, "SELECT count(*) FROM orders"), vec![vec!["1"]]);
}

/// Apply a full setup script (schema + triggers + DML) to a fresh mpedb, then
/// compare one final query against sqlite fed the identical script.
fn cross_check_full(name: &str, setup: &[&str], final_query: &str) {
    let (db, _p) = open(name);
    apply(&db, setup);
    let got = mpedb_rows(&db, final_query);
    let want = sqlite_rows(setup, final_query);
    assert_eq!(got, want, "mpedb vs sqlite disagree on `{final_query}`");
    assert!(!got.is_empty(), "expected some audit rows");
}

#[test]
fn after_update_logs_old_and_new_matches_sqlite() {
    // OLD = pre-image total, NEW = post-image total, both bound in one body.
    cross_check_full(
        "upd",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, old_total INTEGER, new_total INTEGER)",
            "CREATE TRIGGER audit_upd AFTER UPDATE ON orders FOR EACH ROW \
             BEGIN INSERT INTO audit (id, old_total, new_total) \
                   VALUES (NEW.id, OLD.total, NEW.total); END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
            "UPDATE orders SET total = total + 5 WHERE id = 1",
            "UPDATE orders SET total = 999 WHERE id = 2",
        ],
        "SELECT id, old_total, new_total FROM audit ORDER BY id",
    );
}

#[test]
fn after_delete_logs_old_matches_sqlite() {
    // Only OLD is available; the deleted row is logged into audit.
    cross_check_full(
        "del",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, old_total INTEGER, old_tag TEXT)",
            "CREATE TRIGGER audit_del AFTER DELETE ON orders FOR EACH ROW \
             BEGIN INSERT INTO audit (id, old_total, old_tag) \
                   VALUES (OLD.id, OLD.total, OLD.tag); END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
            "INSERT INTO orders (id, total, tag) VALUES (3, 250, 'c')",
            "DELETE FROM orders WHERE total >= 150",
        ],
        "SELECT id, old_total, old_tag FROM audit ORDER BY id",
    );
}

#[test]
fn when_gated_update_over_old_and_new_matches_sqlite() {
    // Fire the audit only when the total strictly increased — a WHEN predicate
    // over BOTH OLD and NEW. id 1 increases (fires), id 2 decreases (skipped).
    cross_check_full(
        "updwhen",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, old_total INTEGER, new_total INTEGER)",
            "CREATE TRIGGER audit_inc AFTER UPDATE ON orders FOR EACH ROW \
             WHEN (NEW.total > OLD.total) \
             BEGIN INSERT INTO audit (id, old_total, new_total) \
                   VALUES (NEW.id, OLD.total, NEW.total); END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
            "UPDATE orders SET total = 90 WHERE id = 1",
            "UPDATE orders SET total = 40 WHERE id = 2",
        ],
        "SELECT id, old_total, new_total FROM audit ORDER BY id",
    );
}

#[test]
fn after_delete_when_gated_matches_sqlite() {
    // AFTER DELETE with a WHEN over OLD: only rows with total > 100 are logged.
    cross_check_full(
        "delwhen",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, old_total INTEGER)",
            "CREATE TRIGGER audit_bigdel AFTER DELETE ON orders FOR EACH ROW \
             WHEN (OLD.total > 100) \
             BEGIN INSERT INTO audit (id, old_total) VALUES (OLD.id, OLD.total); END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
            "INSERT INTO orders (id, total, tag) VALUES (3, 250, 'c')",
            "DELETE FROM orders",
        ],
        "SELECT id, old_total FROM audit ORDER BY id",
    );
}

#[test]
fn multi_statement_after_insert_matches_sqlite() {
    // A BEGIN <stmt>; <stmt>; END body: two inserts run in order on the same txn
    // (DESIGN-TRIGGERS stage 3). Keyed by distinct NEW columns (id vs total) so
    // both rows land — the test proves both body statements fire, in order.
    cross_check_full(
        "multi",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, oid INTEGER, note TEXT)",
            "CREATE TRIGGER audit_multi AFTER INSERT ON orders FOR EACH ROW \
             BEGIN \
               INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.id, 'a'); \
               INSERT INTO audit (id, oid, note) VALUES (NEW.total, NEW.id, 'b'); \
             END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'x')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'y')",
        ],
        "SELECT id, oid, note FROM audit ORDER BY id",
    );
}

#[test]
fn update_of_fires_only_on_named_column_matches_sqlite() {
    // `AFTER UPDATE OF total`: fires when the SET list assigns `total`, and NOT
    // when the UPDATE only touches `tag` (sqlite's SET-target semantics). id 1's
    // total-changing UPDATE logs a row; id 2's tag-only UPDATE logs nothing.
    cross_check_full(
        "updof",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, new_total INTEGER)",
            "CREATE TRIGGER audit_updof AFTER UPDATE OF total ON orders FOR EACH ROW \
             BEGIN INSERT INTO audit (id, new_total) VALUES (NEW.id, NEW.total); END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
            "UPDATE orders SET total = 77 WHERE id = 1",
            "UPDATE orders SET tag = 'z' WHERE id = 2",
        ],
        "SELECT id, new_total FROM audit ORDER BY id",
    );
}

#[test]
fn before_insert_sees_pre_image_matches_sqlite() {
    // A BEFORE INSERT multi-statement body. The first statement logs the row
    // unconditionally; the second is an INSERT … SELECT reading the target table
    // for the row being inserted. At BEFORE time that row is not yet present, so
    // the self-select finds nothing and the second insert is a no-op — the audit
    // holds only the 'pre' rows. (An AFTER trigger would additionally log the
    // 'self' rows.) sqlite, fed the same BEFORE trigger, agrees exactly.
    cross_check_full(
        "before",
        &[
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER, tag TEXT)",
            "CREATE TABLE audit (id INTEGER PRIMARY KEY, oid INTEGER, note TEXT)",
            "CREATE TRIGGER bins BEFORE INSERT ON orders FOR EACH ROW \
             BEGIN \
               INSERT INTO audit (id, oid, note) VALUES (NEW.id, NEW.id, 'pre'); \
               INSERT INTO audit (id, oid, note) \
                 SELECT NEW.id + 100, id, 'self' FROM orders WHERE id = NEW.id; \
             END",
            "INSERT INTO orders (id, total, tag) VALUES (1, 50, 'a')",
            "INSERT INTO orders (id, total, tag) VALUES (2, 150, 'b')",
        ],
        "SELECT id, oid, note FROM audit ORDER BY id",
    );
}
