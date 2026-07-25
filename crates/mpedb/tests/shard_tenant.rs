//! **Per-user shards: conflict that depends on WHO, not on HOW MANY (#144).**
//!
//! #143 gave the guard row-level keys, and arm E measured the limit: a 64-bit
//! Bloom with one bit per key saturates, so an action touching six keys
//! collides with another better than half the time. That is the wrong shape
//! for a per-user workload — whether two actions conflict should depend on
//! *whether it is the same user*, never on how many users exist.
//!
//! So the identity is one value compared for equality: the session context the
//! action ran under. It is taken ONLY when every touched table carries a policy
//! on that key, because the policy is what makes "different tenant ⇒ different
//! rows" a fact rather than a hope.
//!
//! **What a shard means, stated because it is a narrowing:** the shard replaces
//! the row-level filter, so two actions on the SAME user serialize even on
//! different rows. That is what a per-user lock *is*; it is not a regression to
//! be surprised by.

use mpedb::{Config, Database, Error, PolicyCmd, PolicyDef, Session, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-shard-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "orders"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "tenant"
  type = "int64"

  [[table.column]]
  name = "note"
  type = "int64"

[[table]]
name = "shared"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        path.display()
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (d, path)
}

fn tenant_policy() -> PolicyDef {
    PolicyDef {
        name: "tenant_iso".into(),
        command: PolicyCmd::All,
        permissive: true,
        using_src: Some("tenant = current_setting('app.tenant')".into()),
        check_src: None,
    }
}

fn sess(t: i64) -> Session {
    let mut s = Session::empty();
    s.set("app.tenant", Value::Int(t));
    s
}

/// Rows for tenants 1 and 2, then RLS on. Seeding happens first because
/// INSERT WITH CHECK is a later stage.
fn seeded(tag: &str) -> (Database, std::path::PathBuf) {
    let (d, path) = db(tag);
    for (id, t) in [(1i64, 1i64), (2, 1), (3, 2), (4, 2)] {
        d.query(
            "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, 0)",
            &[Value::Int(id), Value::Int(t)],
        )
        .unwrap();
    }
    d.query("INSERT INTO shared (id, v) VALUES (1, 0)", &[]).unwrap();
    d.create_policy("orders", &tenant_policy()).unwrap();
    d.enable_rls("orders", false).unwrap();
    (d, path)
}

const SQL: [&str; 1] = ["UPDATE orders SET note = $1 WHERE id = $2"];

/// **The requirement, as a test.** Two actions under DIFFERENT tenants against
/// the same table must both commit — that is "a shard is not affected by other
/// users' data", and it is the whole point of #144.
#[test]
fn two_tenants_on_one_table_both_commit() {
    let (d, path) = seeded("two");
    let snap = d.snapshot_txn();

    let mut a = d.begin_guarded_as(snap, &sess(1), &SQL).unwrap();
    a.query(SQL[0], &[Value::Int(7), Value::Int(1)]).unwrap();
    a.commit().expect("tenant 1's action should commit");

    let mut b = d.begin_guarded_as(snap, &sess(2), &SQL).unwrap();
    b.query(SQL[0], &[Value::Int(9), Value::Int(3)]).unwrap();
    b.commit().expect(
        "tenant 2's action was refused because tenant 1 wrote — the shard is \
         not isolating users, which is exactly what #144 exists to do",
    );

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same tenant still conflicts. Without this the shard filter could be
/// "always allow" and the test above would not notice.
#[test]
fn the_same_tenant_still_conflicts() {
    let (d, path) = seeded("same");
    let snap = d.snapshot_txn();

    let mut a = d.begin_guarded_as(snap, &sess(1), &SQL).unwrap();
    a.query(SQL[0], &[Value::Int(7), Value::Int(1)]).unwrap();
    a.commit().unwrap();

    let mut b = d.begin_guarded_as(snap, &sess(1), &SQL).unwrap();
    b.query(SQL[0], &[Value::Int(8), Value::Int(2)]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "two actions in the SAME shard both committed ({other:?}) — a shard \
             is the unit of serialization and this is a lost update"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The coverage requirement biting.** Touch a table with no policy and the
/// proof that different tenants touch different rows is gone — so the shard
/// must be discarded and the action must conflict with everything. Without
/// this the guard would trust a proof it does not have.
#[test]
fn touching_an_unpoliced_table_discards_the_shard() {
    let (d, path) = seeded("uncovered");
    let snap = d.snapshot_txn();

    // Tenant 1 writes the SHARED table, which carries no policy.
    let shared_sql = ["UPDATE shared SET v = $1 WHERE id = $2"];
    let mut a = d.begin_guarded_as(snap, &sess(1), &shared_sql).unwrap();
    a.query(shared_sql[0], &[Value::Int(7), Value::Int(1)]).unwrap();
    a.commit().unwrap();

    // Tenant 2 touching the same unpoliced table must NOT be let through by
    // the shard, because nothing keeps the two apart there.
    let mut b = d.begin_guarded_as(snap, &sess(2), &shared_sql).unwrap();
    b.query(shared_sql[0], &[Value::Int(9), Value::Int(1)]).unwrap();
    match b.commit() {
        Err(Error::WriteConflict) => {}
        other => panic!(
            "two tenants writing an UNPOLICED table did not conflict ({other:?}) — \
             the guard used a tenant identity it cannot justify there"
        ),
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// No session context ⇒ no shard ⇒ the row-level behaviour of #143, unchanged.
#[test]
fn without_a_session_the_guard_falls_back() {
    let (d, path) = db("nosess");
    d.query("INSERT INTO shared (id, v) VALUES (1, 0)", &[]).unwrap();
    d.query("INSERT INTO shared (id, v) VALUES (2, 0)", &[]).unwrap();
    let snap = d.snapshot_txn();
    let sql = "UPDATE shared SET v = $1 WHERE id = $2";

    // Different rows, no tenant anywhere: #143's key filter should still let
    // both through. Declared WITH the values, because that is the form in
    // which a statement names one row rather than the whole table.
    let pa = [Value::Int(7), Value::Int(1)];
    let mut a = d.begin_guarded_with(snap, &[(sql, &pa[..])]).unwrap();
    a.query(sql, &pa).unwrap();
    a.commit().unwrap();

    let pb = [Value::Int(9), Value::Int(2)];
    let mut b = d.begin_guarded_with(snap, &[(sql, &pb[..])]).unwrap();
    b.query(sql, &pb).unwrap();
    b.commit().expect("the pre-#144 row-level path stopped working");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **Scale independence, measured rather than asserted.** The number of
/// tenants must not change how often two DIFFERENT tenants conflict — that is
/// the literal claim "10 or 10 million users should not mean more locking",
/// and it is only meaningful if it is falsifiable.
#[test]
fn conflict_rate_does_not_depend_on_tenant_count() {
    for tenants in [2i64, 64, 4096] {
        let (d, path) = db(&format!("scale{tenants}"));
        for id in 0..tenants {
            d.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $1, 0)",
                &[Value::Int(id)],
            )
            .unwrap();
        }
        d.create_policy("orders", &tenant_policy()).unwrap();
        d.enable_rls("orders", false).unwrap();

        // Every action is a different tenant, all against one stale snapshot:
        // none may conflict, whatever the population is.
        let snap = d.snapshot_txn();
        let mut refused = 0;
        for t in 0..tenants.min(32) {
            let mut s = d.begin_guarded_as(snap, &sess(t), &SQL).unwrap();
            s.query(SQL[0], &[Value::Int(t + 1), Value::Int(t)]).unwrap();
            if s.commit().is_err() {
                refused += 1;
            }
        }
        assert_eq!(
            refused, 0,
            "{tenants} tenants: {refused} distinct-tenant actions were refused — \
             conflict is depending on the tenant COUNT, which is the thing #144 \
             exists to remove"
        );
        drop(d);
        let _ = std::fs::remove_file(&path);
    }
}
