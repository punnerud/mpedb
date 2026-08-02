//! N2 fase 1: the in-session text memo is checked FIRST in
//! `WriteSession::query_with_origin` — the same ordering rule as the
//! autocommit path (#166/#168), so a hit skips the per-statement text
//! re-inspections. These pins hold the two edges that ordering must never
//! cut: the temp-namespace S42 class (name resolution moved without moving
//! `schema_gen`) and the `ddl_applied` latch (an in-session DDL must defeat
//! the memo in BOTH directions). Debug builds double-check every hit via
//! `verify_plan_memo`, so running this suite in debug is itself the oracle
//! for the reordered path.

use mpedb::{Config, Database, ExecResult, Value};

fn twodb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                [[table]]\nname = \"t\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n\n\
                [[table]]\nname = \"u\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn int_at(res: &ExecResult) -> i64 {
    match res {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => panic!("expected int"),
        },
        _ => panic!("expected rows"),
    }
}

fn width(res: &ExecResult) -> usize {
    match res {
        ExecResult::Rows { rows, .. } => rows[0].len(),
        _ => panic!("expected rows"),
    }
}

/// (i) The session twin of the multifile.rs S42 lesson: a text memoized
/// IN-SESSION must stop answering with the old resolution once a temp view
/// shadows a name it uses. `CREATE TEMP VIEW` clears the local caches
/// (`drop_local_plans`); with the memo now checked before the routing hooks,
/// this pin is what notices if that invalidation ever loosens.
#[test]
fn temp_view_shadowing_defeats_a_session_memoized_text() {
    let db = twodb();
    let mut s = db.begin().unwrap();
    for i in 0..3 {
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(i)]).unwrap();
    }
    s.query("INSERT INTO u VALUES ($1)", &[Value::Int(100)]).unwrap();
    // Memoize the text inside the session, then commit.
    let n = s.query("SELECT count(*) FROM t", &[]).unwrap();
    assert_eq!(int_at(&n), 3);
    s.commit().unwrap();

    // Shadow `t` with a temp view over `u` (autocommit, same handle).
    db.query("CREATE TEMP VIEW t AS SELECT v FROM u", &[]).unwrap();

    // Same text, new session: must answer THROUGH the view.
    let mut s = db.begin().unwrap();
    let n = s.query("SELECT count(*) FROM t", &[]).unwrap();
    assert_eq!(int_at(&n), 1, "memoized text must re-resolve through the temp view");
    s.rollback();

    // And unshadowing restores the table.
    db.query("DROP VIEW t", &[]).unwrap();
    let mut s = db.begin().unwrap();
    let n = s.query("SELECT count(*) FROM t", &[]).unwrap();
    assert_eq!(int_at(&n), 3);
    s.rollback();
}

/// (ii) The `ddl_applied` latch, both directions, past the memo-first hop:
/// after an in-session ALTER the SAME text must see the new shape (the memo
/// may not serve the pre-DDL plan), and after ROLLBACK the surviving memo
/// entry must keep answering the OLD shape on the autocommit path — the
/// entry's generation still names the committed catalog, which the rollback
/// left untouched.
#[test]
fn in_session_ddl_defeats_the_memo_and_rollback_keeps_the_entry_honest() {
    let db = twodb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES ($1)", &[Value::Int(1)]).unwrap();
    s.commit().unwrap();

    // Memoize on the autocommit path AND in a session.
    let r = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(width(&r), 1);
    let mut s = db.begin().unwrap();
    let r = s.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(width(&r), 1);

    // In-session DDL: the same text must now answer with the new shape.
    s.query("ALTER TABLE t ADD COLUMN c INTEGER", &[]).unwrap();
    let r = s.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(width(&r), 2, "post-DDL the memo may not serve the one-column plan");
    s.rollback();

    // The rollback discarded the DDL; the committed catalog never moved, so
    // the memo entry (stamped with the committed gen) is still the truth.
    let r = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(width(&r), 1, "after rollback the committed shape answers again");
}

/// (iii) The frozen-facts claim behind skipping per-hit revalidation work
/// in-session: a session's OWN inserts move no committed fact, so the
/// memoized text keeps hitting and keeps answering correctly across a row
/// count that grows three decades; after COMMIT the committed facts HAVE
/// moved and the autocommit path still answers correctly (a stale-tables
/// entry is a miss, never a wrong answer).
#[test]
fn own_writes_never_stale_a_session_hit_and_commit_moves_the_facts() {
    let db = twodb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES ($1)", &[Value::Int(-1)]).unwrap();
    s.commit().unwrap();

    let mut s = db.begin().unwrap();
    for i in 0..5000 {
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(i)]).unwrap();
        if i % 1000 == 0 {
            let n = s.query("SELECT count(*) FROM t", &[]).unwrap();
            assert_eq!(int_at(&n), i + 2, "in-session hit must see own writes");
        }
    }
    s.commit().unwrap();

    let n = db.query("SELECT count(*) FROM t", &[]).unwrap();
    assert_eq!(int_at(&n), 5001, "post-commit the autocommit path sees the new facts");
}

/// ATTACH/DETACH stay refused by name inside a transaction — the refusal
/// lives BELOW the memo check now, and this pin proves no memo state can
/// shortcut past it (an ATTACH text is never remembered).
#[test]
fn attach_refusal_survives_memo_first() {
    let db = twodb();
    let mut s = db.begin().unwrap();
    let e = s.query("ATTACH DATABASE ':memory:' AS x", &[]).unwrap_err();
    let msg = format!("{e}");
    assert!(msg.contains("ATTACH"), "refusal must stay by-name: {msg}");
    s.rollback();
}
