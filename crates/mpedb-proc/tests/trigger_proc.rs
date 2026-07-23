//! DESIGN-TRIGGERS stage 5, end-to-end: `EXECUTE PROCEDURE` trigger bodies —
//! PySpell procedures fired per row on the SAME transaction as the triggering
//! statement, through the executor's `CtxBridge`.
//!
//! Lives in this crate (not `mpedb`'s own tests) because defining a procedure
//! takes [`ProcEngine`], which depends on `mpedb` — the dependency only points
//! this way.

use mpedb::{Config, Database, ExecResult, Value};
use mpedb_proc::{Lang, ProcEngine};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn test_config(name: &str) -> (Config, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-trgproc-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 64

[[table]]
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false

[[table]]
name = "audit"
primary_key = ["seq"]

  [[table.column]]
  name = "seq"
  type = "int64"

  [[table.column]]
  name = "oid"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "tag"
  type = "text"
  nullable = false
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), FileGuard(path))
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// audit(oid, tag): the canonical enrichment body — writes one audit row.
/// `seq` is `INTEGER PRIMARY KEY` semantics via max+1 in SQL to keep the
/// procedure single-statement.
const AUDIT_PY: &str = r#"
def audit(oid, tag):
    db.execute("INSERT INTO audit (seq, oid, tag) VALUES ((SELECT coalesce(max(seq), 0) + 1 FROM audit), $1, $2)", [oid, tag])
    return 0
"#;

fn audit_rows(db: &Database) -> Vec<(i64, String)> {
    match db
        .query("SELECT oid, tag FROM audit ORDER BY seq", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match (&r[0], &r[1]) {
                (Value::Int(o), Value::Text(t)) => (*o, t.clone()),
                other => panic!("unexpected audit row {other:?}"),
            })
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn proc_trigger_fires_after_insert_and_is_process_coherent() {
    let (cfg, g) = test_config("fires");
    let db = Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db).define(AUDIT_PY, Lang::Python).unwrap();
    db.query(
        "CREATE TRIGGER a_ins AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, 'ins')",
        &[],
    )
    .unwrap();

    db.query("INSERT INTO accounts (id, balance) VALUES (1, 100)", &[]).unwrap();
    db.query("INSERT INTO accounts (id, balance) VALUES (2, 50)", &[]).unwrap();
    assert_eq!(audit_rows(&db), vec![(1, "ins".into()), (2, "ins".into())]);

    // A SECOND handle sees the trigger through the schema_gen gate and fires
    // it too — the catalog is the file, not the connection.
    let db2 = Database::open_from_file(&g.0).unwrap();
    db2.query("INSERT INTO accounts (id, balance) VALUES (3, 7)", &[]).unwrap();
    assert_eq!(audit_rows(&db).len(), 3);
    assert_eq!(audit_rows(&db2)[2], (3, "ins".into()));

    // sqlite_master reconstruction spells the procedure form.
    let listed = db.list_triggers().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].2.contains("EXECUTE PROCEDURE audit(NEW.id, 'ins')"),
        "{}",
        listed[0].2
    );
}

#[test]
fn when_gate_update_events_and_old_binding() {
    let (cfg, _g) = test_config("when");
    let db = Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db).define(AUDIT_PY, Lang::Python).unwrap();
    // Fire only when the balance SHRANK; log the OLD id. Argument expressions
    // are full trigger-scope expressions (coalesce over both images works).
    db.query(
        "CREATE TRIGGER a_upd AFTER UPDATE ON accounts WHEN (NEW.balance < OLD.balance) \
         EXECUTE PROCEDURE audit(coalesce(OLD.id, -1), 'shrink')",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO accounts (id, balance) VALUES (1, 100)", &[]).unwrap();

    db.query("UPDATE accounts SET balance = 200 WHERE id = 1", &[]).unwrap();
    assert!(audit_rows(&db).is_empty(), "grow must not fire");
    db.query("UPDATE accounts SET balance = 150 WHERE id = 1", &[]).unwrap();
    assert_eq!(audit_rows(&db), vec![(1, "shrink".into())]);
}

/// A BEFORE-INSERT procedure that errors is the stage-5 validation trigger:
/// the statement aborts atomically — the row is NOT inserted, and nothing the
/// procedure wrote sticks (same-txn semantics, DESIGN-TRIGGERS §5.2).
#[test]
fn failing_before_proc_vetoes_the_write_atomically() {
    let (cfg, _g) = test_config("veto");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            r#"
def veto(v):
    db.execute("INSERT INTO audit (seq, oid, tag) VALUES ((SELECT coalesce(max(seq), 0) + 1 FROM audit), $1, 'pre')", [v])
    if v < 0:
        return 1 // 0
    return 0
"#,
            Lang::Python,
        )
        .unwrap();
    db.query(
        "CREATE TRIGGER chk BEFORE INSERT ON accounts EXECUTE PROCEDURE veto(NEW.balance)",
        &[],
    )
    .unwrap();

    db.query("INSERT INTO accounts (id, balance) VALUES (1, 10)", &[]).unwrap();
    assert_eq!(audit_rows(&db).len(), 1, "accepted row logged");

    let err = db
        .query("INSERT INTO accounts (id, balance) VALUES (2, -5)", &[])
        .unwrap_err();
    assert!(err.to_string().contains("division"), "{err}");
    // The whole autocommit statement unwound: no account row AND no audit row
    // — the procedure's own pre-write rolled back with it.
    match db.query("SELECT count(*) FROM accounts", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(audit_rows(&db).len(), 1, "veto's pre-write unwound");
}

#[test]
fn define_time_refusals_name_the_gap() {
    let (cfg, _g) = test_config("refuse");
    let db = Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db).define(AUDIT_PY, Lang::Python).unwrap();

    let e = db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON accounts EXECUTE PROCEDURE nosuch(NEW.id)",
            &[],
        )
        .unwrap_err();
    assert!(e.to_string().contains("no stored procedure `nosuch`"), "{e}");

    let e = db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id)",
            &[],
        )
        .unwrap_err();
    assert!(e.to_string().contains("takes 2 argument(s)"), "{e}");

    let e = db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON accounts \
             EXECUTE PROCEDURE audit(NEW.id, (SELECT tag FROM audit))",
            &[],
        )
        .unwrap_err();
    assert!(e.to_string().contains("subqueries"), "{e}");

    let e = db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, $1)",
            &[],
        )
        .unwrap_err();
    assert!(e.to_string().contains("procedure argument"), "{e}");

    // OLD is not in scope for INSERT — same rule as SQL bodies.
    let e = db
        .query(
            "CREATE TRIGGER t AFTER INSERT ON accounts EXECUTE PROCEDURE audit(OLD.id, 'x')",
            &[],
        )
        .unwrap_err();
    assert!(e.to_string().contains("OLD"), "{e}");
}

/// The pinning contract (StoredBody::Proc): the trigger binds the procedure's
/// CONTENT HASH at CREATE. Re-defining the name does not re-target the
/// trigger — deliberately, because procedure definition does not bump
/// `schema_gen`, so a name binding could diverge between processes.
#[test]
fn trigger_pins_the_procedure_version_at_create() {
    let (cfg, _g) = test_config("pin");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    engine.define(AUDIT_PY, Lang::Python).unwrap();
    db.query(
        "CREATE TRIGGER a_ins AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, 'v1')",
        &[],
    )
    .unwrap();

    // Redefine `audit` to tag differently (ignores its arg).
    engine
        .define(
            r#"
def audit(oid, tag):
    db.execute("INSERT INTO audit (seq, oid, tag) VALUES ((SELECT coalesce(max(seq), 0) + 1 FROM audit), $1, 'v2')", [oid])
    return 0
"#,
            Lang::Python,
        )
        .unwrap();

    db.query("INSERT INTO accounts (id, balance) VALUES (1, 1)", &[]).unwrap();
    assert_eq!(audit_rows(&db), vec![(1, "v1".into())], "the pinned v1 fired");

    // Re-CREATE rebinds to the new definition.
    db.query("DROP TRIGGER a_ins", &[]).unwrap();
    db.query(
        "CREATE TRIGGER a_ins AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, 'unused')",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO accounts (id, balance) VALUES (2, 1)", &[]).unwrap();
    assert_eq!(audit_rows(&db)[1], (2, "v2".into()));
}

#[test]
fn runaway_procedure_trips_the_budget_deterministically() {
    let (cfg, _g) = test_config("budget");
    let db = Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db)
        .define(
            "def spin(v):\n    x = 0\n    while x >= 0:\n        x = x + 1\n    return x\n",
            Lang::Python,
        )
        .unwrap();
    db.query(
        "CREATE TRIGGER s AFTER INSERT ON accounts EXECUTE PROCEDURE spin(NEW.id)",
        &[],
    )
    .unwrap();
    let e1 = db
        .query("INSERT INTO accounts (id, balance) VALUES (1, 1)", &[])
        .unwrap_err()
        .to_string();
    let e2 = db
        .query("INSERT INTO accounts (id, balance) VALUES (1, 1)", &[])
        .unwrap_err()
        .to_string();
    assert!(e1.contains("instruction budget exhausted"), "{e1}");
    assert_eq!(e1, e2, "the trip point is deterministic");
    // And the statement unwound: no row.
    match db.query("SELECT count(*) FROM accounts", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(0)),
        other => panic!("unexpected {other:?}"),
    }
}

/// A read-only procedure body iterating `db.rows` exercises the CtxBridge
/// cursor path (materialized on open); coexistence: an in-SQL trigger and a
/// procedure trigger on the SAME table both fire, and the procedure's own DML
/// fires the SQL trigger downstream (nested cascade on one txn).
#[test]
fn cursors_coexistence_and_cascade() {
    let (cfg, _g) = test_config("cascade");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    // Cursor-based validation: total balance must stay under 1000.
    engine
        .define(
            r#"
def cap(v):
    total = v
    for r in db.rows("SELECT balance FROM accounts", []):
        total = total + r[0]
    if total > 1000:
        return 1 // 0
    return total
"#,
            Lang::Python,
        )
        .unwrap();
    engine.define(AUDIT_PY, Lang::Python).unwrap();

    db.query(
        "CREATE TRIGGER cap BEFORE INSERT ON accounts EXECUTE PROCEDURE cap(NEW.balance)",
        &[],
    )
    .unwrap();
    // Procedure trigger writing audit…
    db.query(
        "CREATE TRIGGER a_ins AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, 'proc')",
        &[],
    )
    .unwrap();
    // …and an in-SQL trigger on audit itself: the procedure's INSERT must fire
    // it (nested, depth 2), doubling the tag into accounts' balance-0 row? No —
    // keep it observable: count audit inserts in a side row via UPDATE.
    db.query("INSERT INTO accounts (id, balance) VALUES (99, 0)", &[]).unwrap();
    db.query(
        "CREATE TRIGGER a_cnt AFTER INSERT ON audit \
         BEGIN UPDATE accounts SET balance = balance + 1 WHERE id = 99; END",
        &[],
    )
    .unwrap();

    db.query("INSERT INTO accounts (id, balance) VALUES (1, 500)", &[]).unwrap();
    // audit got the proc row for id=1 AND for id=99? id=99 predates a_ins? No:
    // a_ins existed before both inserts? It was created BEFORE the id=99
    // insert, so 99 is audited too (tag 'proc'), but a_cnt did not exist yet
    // at that point. After the id=1 insert: audit has (99),(1); a_cnt fired
    // once (for the id=1 audit insert).
    let audits = audit_rows(&db);
    assert_eq!(audits, vec![(99, "proc".into()), (1, "proc".into())]);
    match db.query("SELECT balance FROM accounts WHERE id = 99", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        other => panic!("unexpected {other:?}"),
    }

    // The cap procedure read the table through the cursor: pushing the total
    // over 1000 refuses the insert.
    let e = db
        .query("INSERT INTO accounts (id, balance) VALUES (2, 600)", &[])
        .unwrap_err();
    assert!(e.to_string().contains("division"), "{e}");
}

/// DDL inside a WriteSession: `CREATE TRIGGER … EXECUTE PROCEDURE` in an open
/// session resolves the procedure through the txn and, per the established
/// in-txn DDL contract (`view_trigger_txn.rs`), becomes VISIBLE — and starts
/// firing — after commit. The session's own writes between CREATE and commit
/// do not fire it (the trigger set is built from committed state).
#[test]
fn create_in_write_session_resolves_and_fires_after_commit() {
    let (cfg, _g) = test_config("session");
    let db = Database::open_with_config(cfg).unwrap();
    ProcEngine::new(&db).define(AUDIT_PY, Lang::Python).unwrap();

    let mut s = db.begin().unwrap();
    s.query(
        "CREATE TRIGGER a_ins AFTER INSERT ON accounts EXECUTE PROCEDURE audit(NEW.id, 's')",
        &[],
    )
    .unwrap();
    s.query("INSERT INTO accounts (id, balance) VALUES (1, 1)", &[]).unwrap();
    s.commit().unwrap();

    // The in-session insert predates visibility; the post-commit one fires.
    assert_eq!(audit_rows(&db), vec![]);
    db.query("INSERT INTO accounts (id, balance) VALUES (2, 2)", &[]).unwrap();
    assert_eq!(audit_rows(&db), vec![(2, "s".into())]);
}

/// Backtest over a PROCEDURE body: the dry-run replays the spell through the
/// CtxBridge per row, vetoes roll back to their savepoints, and the whole
/// replay commits nothing.
#[test]
fn backtest_replays_a_procedure_trigger_without_committing() {
    let (cfg, _g) = test_config("backtest");
    let db = Database::open_with_config(cfg).unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            r#"
def vet(v):
    db.execute("INSERT INTO audit (seq, oid, tag) VALUES ((SELECT coalesce(max(seq), 0) + 1 FROM audit), $1, 'seen')", [v])
    if v < 0:
        return 1 // 0
    return 0
"#,
            Lang::Python,
        )
        .unwrap();
    for (id, bal) in [(1i64, 10i64), (2, -5), (3, 7), (4, -1)] {
        db.query(
            &format!("INSERT INTO accounts (id, balance) VALUES ({id}, {bal})"),
            &[],
        )
        .unwrap();
    }
    // Dry-run the not-yet-created trigger over the 4 existing rows.
    let r = db
        .backtest_trigger(
            "CREATE TRIGGER chk BEFORE INSERT ON accounts EXECUTE PROCEDURE vet(NEW.balance)",
            0,
        )
        .unwrap();
    assert_eq!((r.fired, r.vetoed), (2, 2));
    assert!(r.veto_examples[0].contains("division"), "{:?}", r.veto_examples);
    // Only the two NON-vetoing rows' audit writes survive to the net count
    // (the vetoed rows rolled back to their savepoints) — and then the whole
    // replay was aborted: the real audit table is untouched.
    assert_eq!(r.net_rows, vec![("audit".to_string(), 2)]);
    assert!(audit_rows(&db).is_empty());
    assert!(db.list_triggers().unwrap().is_empty());
}
