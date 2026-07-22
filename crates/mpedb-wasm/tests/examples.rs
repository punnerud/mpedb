//! The page's claims, checked against the engine.
//!
//! Every button in the playground carries a promise: this one returns rows,
//! that one is REFUSED. The engine is the same code natively and in wasm, so
//! asserting here is asserting about the deployed page — and it means a
//! change in SQL surface that turns a refusal into an acceptance breaks
//! `cargo test --workspace` instead of quietly turning the demo into a
//! misrepresentation.

use mpedb_wasm::examples::{Expect, GROUPS};

#[test]
fn demo_database_builds() {
    let (db, script) = mpedb_wasm::demo::create().expect("demo database");
    assert!(script.contains("CREATE TABLE users"), "seed script is shown to visitors verbatim");
    // The seed schema's table and the DDL-created ones must all be live.
    let bundle = db.schema();
    for want in ["playground", "users", "products", "orders"] {
        assert!(
            bundle.schema.tables.iter().any(|t| t.name == want && !t.dead),
            "table `{want}` missing from the live schema"
        );
    }
}

#[test]
fn every_example_does_what_its_button_says() {
    let (db, _) = mpedb_wasm::demo::create().expect("demo database");
    let mut failures = Vec::new();
    for g in GROUPS {
        for ex in g.items {
            // Mirror `run_one`: the page compiles for the plan panels but
            // lets EXECUTION decide the outcome, because DDL has no plan and
            // a failed `prepare_detached` there is not a refusal.
            let _plan = db.prepare_detached(ex.sql);
            let outcome = db.query(ex.sql, &[]);
            match (ex.expect, &outcome) {
                (Expect::Runs, Err(e)) => {
                    failures.push(format!("[{}] `{}` should run, but: {e}", g.name, ex.label))
                }
                (Expect::Refuses, Ok(_)) => failures.push(format!(
                    "[{}] `{}` is advertised as a REFUSAL but the engine accepted it",
                    g.name, ex.label
                )),
                _ => {}
            }
        }
    }
    assert!(failures.is_empty(), "playground examples drifted:\n  {}", failures.join("\n  "));
}

/// DDL runs through the page's path but has no compiled plan. Both halves
/// matter: if `query` stopped accepting it the demo would break, and if
/// `prepare_detached` started accepting it the page would grow plan panels
/// for something that has no plan.
#[test]
fn ddl_executes_but_has_no_plan() {
    let (db, _) = mpedb_wasm::demo::create().expect("demo database");
    let ddl = "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)";
    assert!(db.prepare_detached(ddl).is_err(), "DDL is not a compiled plan");
    assert!(db.query(ddl, &[]).is_ok(), "DDL must still execute");
    assert!(
        db.query("INSERT INTO notes (id, body) VALUES (1, 'x')", &[]).is_ok(),
        "a table created at run time must be usable"
    );
    assert!(db.query("DROP TABLE notes", &[]).is_ok());
    assert!(
        db.query("SELECT * FROM notes", &[]).is_err(),
        "the dropped table must be gone"
    );
}

/// The "same plan, different spelling" example makes a specific claim. Pin it.
#[test]
fn plan_hash_normalises_keywords_and_whitespace_but_not_identifiers() {
    let (db, _) = mpedb_wasm::demo::create().expect("demo database");
    let h = |sql: &str| db.prepare_detached(sql).expect("compiles").hash;

    let base = h("SELECT country, count(*) FROM users WHERE age >= 30 GROUP BY country");
    assert_eq!(
        base,
        h("select   country ,\n  Count(*)\nfrom  users\nwhere age>=30\ngroup by country"),
        "keyword case, function-name case and whitespace must normalise away"
    );
    assert_ne!(
        base,
        h("SELECT COUNTRY, count(*) FROM users WHERE age >= 30 GROUP BY COUNTRY"),
        "identifiers are case-sensitive; the page says so explicitly"
    );
}

/// The MPEE tab claims the first join example is reordered away from a
/// cartesian step. That is the single most specific claim on the page.
#[test]
fn mpee_removes_the_cartesian_step_the_page_advertises() {
    let (db, _) = mpedb_wasm::demo::create().expect("demo database");
    let sql = "SELECT u.name, p.name, o.qty \
               FROM users u, products p, orders o \
               WHERE o.user_id = u.id AND o.product_id = p.id \
               AND u.email = 'kari.dahl1@example.com'";
    let order = |on: bool| {
        mpedb_sql::set_mpee_enabled(Some(on));
        let d = db.prepare_detached(sql).expect("compiles");
        mpedb_sql::set_mpee_enabled(None);
        let bundle = db.schema();
        let plan = mpedb_sql::CompiledPlan::decode(&d.blob, &bundle.schema).expect("decodes");
        plan.explain(&bundle.schema)
            .lines()
            .find(|l| l.trim_start().starts_with("join order:"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let chosen = order(true);
    let textual = order(false);
    assert_ne!(chosen, textual, "the page shows these two orders as different");
    assert!(
        textual.contains("cartesian") && chosen.contains("0 cartesian steps"),
        "page claims the written order costs a cartesian step and the solver's costs none;\n\
         chosen : {chosen}\n textual: {textual}"
    );
}
