//! The test that makes table-id compaction maintainable.
//!
//! `mpedb_core::compact::ID_KEYS` lists every sys-keyspace prefix whose key
//! embeds a table id; `ID_FREE_KEYS` lists the ones that deliberately do not.
//! A prefix in NEITHER list is a store somebody added without saying whether
//! compaction has to renumber it — and that is the single way this operation
//! goes silently wrong: the record keeps an id that no longer names its table,
//! and now names a different one.
//!
//! DESIGN-DROP-TABLE §0 rejected id REUSE partly because it would impose "a
//! permanent maintenance tax on all future code" — a purge hook every future
//! subsystem must remember. Compaction has the same exposure, and this test is
//! what converts that tax into something a machine collects: adding a store is
//! fine, adding one silently is not.
//!
//! It exercises the database rather than reading the source, so a store that
//! exists but is never written leaves no key and is correctly not demanded.

use mpedb::{Config, Database, ExecResult, Value};

fn cfg(name: &str) -> Config {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/classified-{name}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    Config::from_toml_str(&format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    ))
    .expect("config")
}

fn q(db: &Database, sql: &str) -> ExecResult {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Drive as many distinct sys-keyspace stores as one test reasonably can, then
/// demand that every key left behind is classified.
#[test]
fn every_sys_key_a_working_database_writes_is_classified() {
    let c = cfg("all");
    let db = Database::open_with_config(c).expect("open");

    // Tables, rows, an index — the catalog and stats paths.
    q(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    q(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY)");
    for i in 0..20i64 {
        db.query(
            "INSERT INTO t (id, name) VALUES ($1, $2)",
            &[Value::Int(i), Value::Text(format!("r{i}"))],
        )
        .expect("insert");
    }
    q(&db, "CREATE INDEX t_name ON t (name)");

    // A view, a trigger, a policy — three different name-keyed stores plus the
    // one that is table-id keyed.
    q(&db, "CREATE VIEW v AS SELECT id FROM t");
    q(
        &db,
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO u (id) VALUES (NEW.id); END",
    );
    q(&db, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    q(&db, "CREATE POLICY p ON t USING (id > 0)");

    // A stored function (name-keyed) and its content-addressed blob.
    db.create_function(
        mpedb::spellfn::SpellLang::Python,
        "def dbl(x):\n    return x * 2\n",
    )
    .expect("stored function");

    // A published plan.
    let plan = db.prepare("SELECT id FROM t WHERE id = $1").expect("prepare");
    let _ = plan;

    // Statistics — the one store keyed `<id BE4><index_no BE4>`.
    let _ = db.analyze();

    // Now: every key in the sys keyspace must be classified one way or the
    // other. The failure message lists what is not, because "some key is
    // unclassified" is not something anyone can act on.
    let keys = db.sys_keys().expect("sys keys");
    let unknown = mpedb_core::compact::unclassified(&keys);
    assert!(
        unknown.is_empty(),
        "sys-keyspace prefixes nobody classified for table-id compaction:\n{}\n\n\
         Add each to `mpedb_core::compact::ID_KEYS` (with the byte offset of its \
         table id) or to `ID_FREE_KEYS` (if it carries none). A record that keeps a \
         stale id after compaction names a DIFFERENT table.",
        unknown
            .iter()
            .map(|k| format!("  {}", String::from_utf8_lossy(k).escape_debug()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The test must be able to FAIL. A classification check that passes on
/// anything is worth nothing, so plant a key from a namespace nobody listed
/// and confirm it is reported.
#[test]
fn an_unclassified_key_is_actually_caught() {
    let c = cfg("planted");
    let db = Database::open_with_config(c).expect("open");
    db.sys_record_put("notalisted", b"k", b"v")
        .expect("plant a record from an unlisted namespace");

    let keys = db.sys_keys().expect("sys keys");
    let unknown = mpedb_core::compact::unclassified(&keys);
    assert!(
        unknown.iter().any(|k| k.starts_with(b"notalisted")),
        "the planted key was not reported: {unknown:?}"
    );
}

/// The dry run, against a database that has actually burned id budget.
///
/// This is the measurement the operator wants before deciding: how much of
/// `MAX_TABLES` is tombstones, and what would a compaction touch.
#[test]
fn the_dry_run_reports_what_a_real_database_has_burned() {
    let c = cfg("dryrun");
    let db = Database::open_with_config(c).expect("open");

    // Burn budget the way the workload that hits this wall does: create and
    // drop, over and over, leaving a few tables live.
    for i in 0..30 {
        q(&db, &format!("CREATE TABLE tmp{i} (id INTEGER PRIMARY KEY)"));
        q(&db, &format!("DROP TABLE tmp{i}"));
    }
    q(&db, "CREATE TABLE keep (id INTEGER PRIMARY KEY)");
    q(&db, "ALTER TABLE keep ENABLE ROW LEVEL SECURITY");

    let p = db.compact_plan().expect("dry run");
    assert_eq!(p.dead, 30, "thirty dropped tables should be thirty tombstones");
    assert!(!p.is_noop());
    // seed + keep survive; the id space would shrink from 32 slots to 2.
    assert_eq!(p.live(), 2, "{:?}", p.map);
    // And it names what it would rewrite, not just a count of tables.
    assert!(
        p.records.iter().any(|(owner, n)| owner.contains("row-level-security") && *n > 0),
        "the RLS records were not counted: {:?}",
        p.records
    );

    // A freshly created database has nothing to compact, and must say so
    // rather than propose a rewrite that drops every plan for no gain.
    let fresh = Database::open_with_config(cfg("fresh")).expect("open");
    assert!(fresh.compact_plan().expect("dry run").is_noop());
}

/// The round trip that decides whether compaction is real: a database driven
/// through the id budget, compacted, and still answering every question.
#[test]
fn compaction_reclaims_the_budget_and_everything_survives() {
    let db = Database::open_with_config(cfg("roundtrip")).expect("open");

    // Burn budget, then build something worth keeping on the OTHER side of a
    // renumber: rows, an index, a policy, a view, a trigger.
    for i in 0..40 {
        q(&db, &format!("CREATE TABLE tmp{i} (id INTEGER PRIMARY KEY)"));
        q(&db, &format!("DROP TABLE tmp{i}"));
    }
    q(&db, "CREATE TABLE keep (id INTEGER PRIMARY KEY, name TEXT)");
    q(&db, "CREATE TABLE other (id INTEGER PRIMARY KEY)");
    for i in 0..50i64 {
        db.query(
            "INSERT INTO keep (id, name) VALUES ($1, $2)",
            &[Value::Int(i), Value::Text(format!("r{i}"))],
        )
        .expect("insert");
    }
    q(&db, "CREATE INDEX keep_name ON keep (name)");
    q(&db, "ALTER TABLE keep ENABLE ROW LEVEL SECURITY");
    q(&db, "CREATE POLICY p ON keep USING (id >= 0)");
    q(&db, "CREATE VIEW v AS SELECT id FROM keep");

    let before = db.compact_plan().expect("plan");
    assert_eq!(before.dead, 40);

    let done = db.compact_ids().expect("compact");
    assert!(done.catalog > 0, "no catalog entries moved: {done:?}");

    // The budget is back.
    let after = db.compact_plan().expect("plan after");
    assert_eq!(after.dead, 0, "tombstones survived");
    assert!(after.is_noop(), "a compacted database must be a no-op");
    assert_eq!(after.live(), 3, "seed + keep + other");

    // Every question still answers. This is the part that catches a renumber
    // that moved the schema but not the trees: the rows would be gone, or
    // worse, they would be another table's.
    let rows = match db.query("SELECT count(*) FROM keep", &[]).expect("count") {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(rows[0][0], Value::Int(50), "rows did not survive the renumber");

    // The INDEX — a separate tree, a separate catalog entry, its own id in the
    // key. A renumber that forgot it would still answer, just slowly and from
    // the wrong tree.
    let hit = match db.query("SELECT id FROM keep WHERE name = 'r42'", &[]).expect("index") {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(hit, vec![vec![Value::Int(42)]]);

    // The VIEW and the second table.
    assert!(db.query("SELECT count(*) FROM v", &[]).is_ok(), "view lost");
    assert!(db.query("SELECT count(*) FROM other", &[]).is_ok(), "second table lost");

    // And it is a live database: writes still land, and on the right table.
    db.query(
        "INSERT INTO keep (id, name) VALUES ($1, $2)",
        &[Value::Int(999), Value::Text("after".into())],
    )
    .expect("write after compaction");
    let n = match db.query("SELECT count(*) FROM keep", &[]).expect("count") {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(n, Value::Int(51));
    let empty = match db.query("SELECT count(*) FROM other", &[]).expect("count") {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(empty, Value::Int(0), "the write landed on the wrong table");
}

/// A published plan must not survive a renumber.
///
/// It carries table ids inside its own bytes and inside its footprint's
/// `TableSet`. After a renumber those name a DIFFERENT table — and validate
/// would not object, because the new id is perfectly legal. That is the one
/// failure here that would be silent, so the plan is deleted and the caller
/// re-prepares.
#[test]
fn published_plans_do_not_survive_a_renumber() {
    let db = Database::open_with_config(cfg("plans")).expect("open");
    q(&db, "CREATE TABLE a (id INTEGER PRIMARY KEY)");
    q(&db, "DROP TABLE a");
    q(&db, "CREATE TABLE keep (id INTEGER PRIMARY KEY)");

    let hash = db.prepare("SELECT id FROM keep WHERE id = $1").expect("prepare");
    assert!(db.execute(&hash, &[Value::Int(1)]).is_ok(), "the plan should run before");

    db.compact_ids().expect("compact");

    let e = db.execute(&hash, &[Value::Int(1)]).unwrap_err().to_string();
    assert!(
        e.contains("unknown plan hash") || e.contains("registry"),
        "a plan survived the renumber: {e}"
    );
    // …and re-preparing works, which is the whole cost of dropping them.
    let again = db.prepare("SELECT id FROM keep WHERE id = $1").expect("re-prepare");
    assert!(db.execute(&again, &[Value::Int(1)]).is_ok());
}

/// Compaction with somebody else attached must refuse BY NAME.
///
/// The exclusivity is the correctness argument, not a nicety: it is the only
/// thing that closes the window where the schema has moved and the records
/// keyed by the old ids have not.
#[test]
fn compaction_refuses_while_a_reader_is_attached() {
    let c = cfg("exclusive");
    let db = Database::open_with_config(c.clone()).expect("open");
    q(&db, "CREATE TABLE a (id INTEGER PRIMARY KEY)");
    q(&db, "DROP TABLE a");
    q(&db, "CREATE TABLE keep (id INTEGER PRIMARY KEY)");

    // A second handle on the same file, with a reader pinned.
    let other = Database::open_with_config(c).expect("second handle");
    let h = other.prepare("SELECT id FROM keep").expect("prepare");
    let stream = other.stream_query(&h, &[]).expect("open a reader");

    let e = db.compact_ids().unwrap_err().to_string();
    assert!(e.contains("database to itself"), "{e}");
    assert!(e.contains("reader"), "{e}");

    drop(stream);
    drop(other);
    // With the reader gone it goes through.
    db.compact_ids().expect("compact once alone");
}

/// The two halves together: a budget you can set, and a way to get slots back
/// when it runs out.
///
/// The knob is testable at a cap of 6 precisely because it IS a knob — the
/// same property at the compiled-in 4096 costs 4096 tables to reach, which is
/// why it was never covered before.
#[test]
fn configured_budget_refuses_by_name_and_compaction_returns_the_slots() {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/budget-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\nmax_tables = 6\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");
    assert_eq!(db.max_tables(), 6);

    // Burn the budget the way a real workload does — create and drop, so the
    // LIVE count never approaches the cap and the tombstones do all the
    // spending. This is the shape the user hit at 4096.
    for i in 0..5 {
        q(&db, &format!("CREATE TABLE tmp{i} (id INTEGER PRIMARY KEY)"));
        q(&db, &format!("DROP TABLE tmp{i}"));
    }
    let err = db
        .query("CREATE TABLE one_too_many (id INTEGER PRIMARY KEY)", &[])
        .expect_err("budget is spent");
    let msg = format!("{err}");
    assert!(msg.contains('6'), "{msg}");
    assert!(msg.contains("compact-ids"), "the refusal must name the way out: {msg}");
    assert!(msg.contains("LIFETIME"), "and must say WHY 1 live table is full: {msg}");

    // One live table, five tombstones. Compaction is the whole difference.
    let plan = db.compact_plan().expect("plan");
    assert_eq!(plan.live(), 1);
    assert_eq!(plan.dead, 5);
    db.compact_ids().expect("compact");

    let after = db.compact_plan().expect("plan");
    assert_eq!(after.live(), 1);
    assert_eq!(after.dead, 0);
    q(&db, "CREATE TABLE one_too_many (id INTEGER PRIMARY KEY)");
    q(&db, "INSERT INTO one_too_many (id) VALUES (7)");
    let ExecResult::Rows { rows, .. } = q(&db, "SELECT id FROM one_too_many") else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);

    let _ = std::fs::remove_file(&path);
}

/// A file written under a RAISED budget must open under the DEFAULT one.
///
/// The cap binds what a process MINTS; if it also bound what a process LOADS,
/// then `max_tables` would be a file-format flag day wearing a config's
/// clothes — set it once, and every tool built without it stops reading the
/// database.
#[test]
fn a_raised_budget_does_not_make_the_file_unreadable() {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/budget-portable-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let body = |extra: &str| {
        format!(
            "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n{extra}\n\
             [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
             [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
        )
    };
    let wide = Config::from_toml_str(&body("max_tables = 9000\n")).expect("cfg");
    {
        let db = Database::open_with_config(wide).expect("open wide");
        assert_eq!(db.max_tables(), 9000);
        q(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
        q(&db, "INSERT INTO t (id) VALUES (1)");
    }
    let plain = Config::from_toml_str(&body("")).expect("cfg");
    let db = Database::open_with_config(plain).expect("a default-budget process must open it");
    assert_eq!(db.max_tables(), mpedb_types::MAX_TABLES);
    let ExecResult::Rows { rows, .. } = q(&db, "SELECT id FROM t") else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);

    let _ = std::fs::remove_file(&path);
}

/// `DROP TABLE t CASCADE` drops `t` even though a child still points at it —
/// and `RESTRICT`, like the bare form, still refuses.
///
/// The reason this is worth a test rather than a parser assertion: refusing the
/// KEYWORD is not a small failure. The parse error left the table standing, so
/// the next `CREATE` of that name failed as a duplicate and every statement
/// after it referenced the WRONG table. That is how one unsupported word cost
/// 13 statements in one corpus file.
#[test]
fn drop_table_cascade_overrides_the_orphan_refusal_and_restrict_does_not() {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/fkcascade-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [compat]\nforeign_keys = true\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");
    assert!(db.fk_enforced(), "the test needs enforcement ON to mean anything");

    let setup = |n: u32| {
        q(&db, &format!("CREATE TABLE par{n} (a INTEGER PRIMARY KEY)"));
        q(&db, &format!("CREATE TABLE chi{n} (b INTEGER PRIMARY KEY, a INTEGER REFERENCES par{n})"));
        db.query(&format!("INSERT INTO par{n} (a) VALUES (1)"), &[]).expect("parent row");
        db.query(&format!("INSERT INTO chi{n} (b, a) VALUES (1, 1)"), &[]).expect("child row");
    };

    // Bare: refused, because a child with rows points at it.
    setup(1);
    let err = db.query("DROP TABLE par1", &[]).expect_err("a live child blocks");
    assert!(format!("{err}").contains("FOREIGN KEY"), "{err}");

    // RESTRICT means the same thing spelled out.
    setup(2);
    let err = db.query("DROP TABLE par2 RESTRICT", &[]).expect_err("RESTRICT blocks too");
    assert!(format!("{err}").contains("FOREIGN KEY"), "{err}");

    // CASCADE drops it.
    setup(3);
    q(&db, "DROP TABLE par3 CASCADE");
    // …and the child's rows are still there, which is what PostgreSQL leaves
    // behind too (it drops the CONSTRAINT, not the rows).
    let ExecResult::Rows { rows, .. } = q(&db, "SELECT b FROM chi3") else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    // The key definition is left DANGLING — sqlite's behaviour, and the
    // documented difference from PostgreSQL, which would have dropped it.
    let err = db
        .query("INSERT INTO chi3 (b, a) VALUES (2, 1)", &[])
        .expect_err("a dangling key refuses the child's next write");
    assert!(format!("{err}").contains("no such table"), "{err}");

    // The whole point of accepting the keyword: the name is FREE afterwards.
    q(&db, "CREATE TABLE par3 (a INTEGER PRIMARY KEY, extra TEXT)");

    let _ = std::fs::remove_file(&path);
}

/// `DROP TABLE parent, child` — the list, and the two rules that make it more
/// than a loop.
#[test]
fn a_drop_list_resolves_every_name_first_and_a_child_in_it_does_not_block() {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!("{dir}/droplist-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [compat]\nforeign_keys = true\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");

    q(&db, "CREATE TABLE par (a INTEGER PRIMARY KEY)");
    q(&db, "CREATE TABLE chi (b INTEGER PRIMARY KEY, a INTEGER REFERENCES par)");
    db.query("INSERT INTO par (a) VALUES (1)", &[]).expect("parent row");
    db.query("INSERT INTO chi (b, a) VALUES (1, 1)", &[]).expect("child row");

    // NOTHING STARTS unless every name resolves — a half-applied DROP is the
    // one outcome a caller cannot reason about.
    let err = db
        .query("DROP TABLE par, nosuch", &[])
        .expect_err("a missing name refuses the whole list");
    assert!(format!("{err}").contains("nosuch"), "{err}");
    assert!(
        db.query("SELECT a FROM par", &[]).is_ok(),
        "par must survive a list that never started"
    );

    // A CHILD INSIDE THE LIST does not block its own parent. Checking each
    // name against the whole schema would refuse this on `par`, and
    // PostgreSQL accepts it because the child is going too.
    q(&db, "DROP TABLE par, chi");
    assert!(db.query("SELECT a FROM par", &[]).is_err());
    assert!(db.query("SELECT b FROM chi", &[]).is_err());

    // IF EXISTS makes a missing name a no-op rather than a refusal, and the
    // present ones still go.
    q(&db, "CREATE TABLE keep (a INTEGER PRIMARY KEY)");
    q(&db, "DROP TABLE IF EXISTS keep, alsonosuch");
    assert!(db.query("SELECT a FROM keep", &[]).is_err());

    let _ = std::fs::remove_file(&path);
}
