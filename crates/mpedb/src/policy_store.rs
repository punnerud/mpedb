//! RLS policy storage in the catalog sys-keyspace + the facade DDL API
//! (DESIGN-MULTIDB.md §3.2). A policy edit is one ordinary COW commit (writer
//! lock → sys_put → bump the table's `pol_epoch` → meta flip), so it publishes
//! {schema+policy} atomically and never touches the reviewed commit protocol.
//!
//! Key layout under the sys-keyspace (the same tree the plan registry uses):
//! - `pol/<table_id BE4>/<name>`   → [`PolicyDef::encode_value`]
//! - `rlsen/<table_id BE4>`        → 1 byte flags (bit0 = enabled, bit1 = force)
//! - `polep/<table_id BE4>`        → u64 LE monotonically-bumped epoch

use crate::Database;
use mpedb_sql::{CompiledPlan, PolicyCatalog, TablePolicies};
use mpedb_types::{Error, PolicyDef, Result};

const POL_PREFIX: &[u8] = b"pol/";
const RLSEN_PREFIX: &[u8] = b"rlsen/";
const POLEP_PREFIX: &[u8] = b"polep/";

fn with_table_id(prefix: &[u8], table_id: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 4);
    k.extend_from_slice(prefix);
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}

fn pol_subkey(table_id: u32, name: &str) -> Vec<u8> {
    let mut k = with_table_id(POL_PREFIX, table_id);
    k.push(b'/');
    k.extend_from_slice(name.as_bytes());
    k
}

/// Parse `<table_id BE4>/<name>` (the bytes after `pol/`).
fn parse_pol_key(rest: &[u8]) -> Option<(u32, String)> {
    if rest.len() < 5 || rest[4] != b'/' {
        return None;
    }
    let table_id = u32::from_be_bytes(rest[0..4].try_into().ok()?);
    let name = std::str::from_utf8(&rest[5..]).ok()?.to_string();
    Some((table_id, name))
}

impl Database {
    /// The table id for `name`, or a `Bind` error naming the missing table.
    fn require_table_id(&self, name: &str) -> Result<u32> {
        self.schema()
            .table_id(name)
            .ok_or_else(|| Error::Bind(format!("unknown table `{name}`")))
    }

    /// Create (or replace) an RLS policy on `table` (DESIGN-MULTIDB.md §3.1).
    /// The `USING`/`WITH CHECK` sources are validated against the table before
    /// storage. Must not be called while a [`WriteSession`](crate::WriteSession)
    /// from this handle is open (it takes the writer lock).
    pub fn create_policy(&self, table: &str, def: &PolicyDef) -> Result<()> {
        let table_id = self.require_table_id(table)?;
        let t = self
            .schema()
            .table(table_id)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        if def.name.is_empty() || def.name.as_bytes().contains(&b'/') {
            return Err(Error::Bind("policy name must be non-empty and contain no '/'".into()));
        }
        for src in [def.using_src.as_deref(), def.check_src.as_deref()].into_iter().flatten() {
            mpedb_sql::validate_policy_expr(src, t)?;
        }
        let mut w = self.engine.begin_write()?;
        w.sys_put(&pol_subkey(table_id, &def.name), &def.encode_value())?;
        bump_epoch(&mut w, table_id)?;
        w.commit()
    }

    /// Drop a policy by name. Returns whether it existed.
    pub fn drop_policy(&self, table: &str, name: &str) -> Result<bool> {
        let table_id = self.require_table_id(table)?;
        let mut w = self.engine.begin_write()?;
        let existed = w.sys_delete(&pol_subkey(table_id, name))?;
        if existed {
            bump_epoch(&mut w, table_id)?;
        }
        w.commit()?;
        Ok(existed)
    }

    /// `ALTER TABLE <table> ENABLE [FORCE] ROW LEVEL SECURITY`. With RLS enabled
    /// and no permissive policy applicable to a command, that command sees zero
    /// rows (default-deny, §3.5) — the fail-closed posture.
    pub fn enable_rls(&self, table: &str, force: bool) -> Result<()> {
        let table_id = self.require_table_id(table)?;
        let flags: u8 = 0b01 | if force { 0b10 } else { 0 };
        let mut w = self.engine.begin_write()?;
        w.sys_put(&with_table_id(RLSEN_PREFIX, table_id), &[flags])?;
        bump_epoch(&mut w, table_id)?;
        w.commit()
    }

    /// `ALTER TABLE <table> DISABLE ROW LEVEL SECURITY` — removes filtering
    /// (all rows visible again). Policies themselves are left in place.
    pub fn disable_rls(&self, table: &str) -> Result<()> {
        let table_id = self.require_table_id(table)?;
        let mut w = self.engine.begin_write()?;
        w.sys_delete(&with_table_id(RLSEN_PREFIX, table_id))?;
        bump_epoch(&mut w, table_id)?;
        w.commit()
    }

    /// Load every table's RLS state into a [`PolicyCatalog`] for the planner.
    /// Read on a pinned snapshot so the policy set is consistent with the schema
    /// the plan is compiled against.
    pub(crate) fn load_policy_catalog(&self) -> Result<PolicyCatalog> {
        let mut cat = PolicyCatalog::empty();
        let mut per_table: std::collections::HashMap<u32, TablePolicies> =
            std::collections::HashMap::new();
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan();
        r.finish()?;
        for (subkey, value) in scan? {
            if let Some(rest) = subkey.strip_prefix(POL_PREFIX) {
                if let Some((table_id, name)) = parse_pol_key(rest) {
                    let def = PolicyDef::decode_value(name, &value)?;
                    per_table.entry(table_id).or_default().policies.push(def);
                }
            } else if let Some(rest) = subkey.strip_prefix(RLSEN_PREFIX) {
                if let Some(table_id) = table_id_of(rest) {
                    let flags = value.first().copied().unwrap_or(0);
                    let tp = per_table.entry(table_id).or_default();
                    tp.rls_enabled = flags & 0b01 != 0;
                    tp.force = flags & 0b10 != 0;
                }
            } else if let Some(rest) = subkey.strip_prefix(POLEP_PREFIX) {
                if let Some(table_id) = table_id_of(rest) {
                    per_table.entry(table_id).or_default().epoch = epoch_of(&value);
                }
            }
        }
        for (table_id, tp) in per_table {
            // Deterministic policy order so plans are reproducible across
            // processes regardless of btree scan order.
            let mut tp = tp;
            tp.policies.sort_by(|a, b| a.name.cmp(&b.name));
            cat.set_table(table_id, tp);
        }
        Ok(cat)
    }
}

fn bump_epoch(w: &mut mpedb_core::WriteTxn<'_>, table_id: u32) -> Result<()> {
    let key = with_table_id(POLEP_PREFIX, table_id);
    let cur = w
        .sys_get(&key)?
        .and_then(|b| b.try_into().ok().map(u64::from_le_bytes))
        .unwrap_or(0);
    w.sys_put(&key, &cur.wrapping_add(1).to_le_bytes())
}

fn table_id_of(rest: &[u8]) -> Option<u32> {
    (rest.len() == 4).then(|| u32::from_be_bytes(rest.try_into().unwrap()))
}

fn epoch_of(value: &[u8]) -> u64 {
    value.try_into().map(u64::from_le_bytes).unwrap_or(0)
}

/// Build one table's live RLS state from a full sys-keyspace scan (slow path of
/// staleness validation, only reached when the epoch moved).
fn one_table_from_scan(scan: &[(Vec<u8>, Vec<u8>)], table_id: u32) -> Result<TablePolicies> {
    let mut tp = TablePolicies::default();
    for (subkey, value) in scan {
        if let Some(rest) = subkey.strip_prefix(POL_PREFIX) {
            if let Some((tid, name)) = parse_pol_key(rest) {
                if tid == table_id {
                    tp.policies.push(PolicyDef::decode_value(name, value)?);
                }
            }
        } else if let Some(rest) = subkey.strip_prefix(RLSEN_PREFIX) {
            if table_id_of(rest) == Some(table_id) {
                let flags = value.first().copied().unwrap_or(0);
                tp.rls_enabled = flags & 0b01 != 0;
                tp.force = flags & 0b10 != 0;
            }
        } else if let Some(rest) = subkey.strip_prefix(POLEP_PREFIX) {
            if table_id_of(rest) == Some(table_id) {
                tp.epoch = epoch_of(value);
            }
        }
    }
    Ok(tp)
}

/// Decide whether a plan is stale relative to `live_epoch` (already read from
/// the executing pin). Fast path: epochs equal ⇒ current. Slow path: the epoch
/// moved, so recompute the table's live policy content hash from `scan` and
/// compare — a no-op edit still matches, a real edit is stale.
fn is_stale(plan: &CompiledPlan, table_id: u32, live_epoch: u64, scan: &[(Vec<u8>, Vec<u8>)]) -> Result<bool> {
    if live_epoch == plan.policy_epoch {
        return Ok(false);
    }
    let tp = one_table_from_scan(scan, table_id)?;
    Ok(mpedb_sql::table_policy_hash(Some(&tp)) != plan.policy_hash)
}

impl Database {
    fn evict(&self, hash: Option<&mpedb_types::PlanHash>) {
        if let Some(h) = hash {
            if let Ok(mut c) = self.cache.write() {
                c.remove(h);
            }
        }
    }

    /// Validate a plan's baked RLS policy against the live catalog **under the
    /// executing read snapshot** `r` (Phase-5 leak-proofing, §4.2/§4.3). A stale
    /// plan is evicted and reported as `PlanInvalidated` so the caller
    /// re-prepares against the current policy.
    pub(crate) fn validate_policy_read(
        &self,
        hash: Option<&mpedb_types::PlanHash>,
        plan: &CompiledPlan,
        r: &mpedb_core::ReadTxn<'_>,
    ) -> Result<()> {
        let table = match plan.target_table() {
            Some(t) => t,
            None => return Ok(()),
        };
        let live_epoch = r.sys_get(&with_table_id(POLEP_PREFIX, table))?.map_or(0, |b| epoch_of(&b));
        if live_epoch == plan.policy_epoch {
            return Ok(());
        }
        if is_stale(plan, table, live_epoch, &r.sys_scan()?)? {
            self.evict(hash);
            return Err(Error::PlanInvalidated);
        }
        Ok(())
    }

    /// As [`validate_policy_read`], but under a write transaction that already
    /// holds the writer lock (so no policy edit can race the check).
    pub(crate) fn validate_policy_write(
        &self,
        hash: Option<&mpedb_types::PlanHash>,
        plan: &CompiledPlan,
        w: &mut mpedb_core::WriteTxn<'_>,
    ) -> Result<()> {
        let table = match plan.target_table() {
            Some(t) => t,
            None => return Ok(()),
        };
        let live_epoch = w.sys_get(&with_table_id(POLEP_PREFIX, table))?.map_or(0, |b| epoch_of(&b));
        if live_epoch == plan.policy_epoch {
            return Ok(());
        }
        if is_stale(plan, table, live_epoch, &w.sys_scan()?)? {
            self.evict(hash);
            return Err(Error::PlanInvalidated);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Database, ExecResult, PolicyCmd, PolicyDef, Session};
    use mpedb_types::{Config, Value};

    fn db(tag: &str) -> Database {
        let path = format!("/dev/shm/mpedb-rls-{tag}-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"note\"\n  type = \"text\"\n  nullable = true"
        ))
        .unwrap();
        Database::open_with_config(cfg).unwrap()
    }

    fn sess(tenant: i64) -> Session {
        let mut s = Session::empty();
        s.set("app.tenant", Value::Int(tenant));
        s
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

    fn nrows(r: ExecResult) -> usize {
        match r {
            ExecResult::Rows { rows, .. } => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn seed(db: &Database) {
        // Seed BEFORE enabling RLS (INSERT WITH CHECK is Phase-4 Stage C).
        for (id, t) in [(1, 1), (2, 1), (3, 2)] {
            db.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
    }

    #[test]
    fn select_is_filtered_by_policy() {
        let db = db("select");
        seed(&db);
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        // No WHERE — the policy alone restricts visibility per session.
        let sql = "SELECT id FROM orders";
        assert_eq!(nrows(db.query_ctx(&sess(1), sql, &[]).unwrap()), 2);
        assert_eq!(nrows(db.query_ctx(&sess(2), sql, &[]).unwrap()), 1);
        // Fail-closed: no context set ⇒ hard error, not a silent empty set.
        assert!(matches!(
            db.query_ctx(&Session::empty(), sql, &[]),
            Err(mpedb_types::Error::Bind(_))
        ));
    }

    #[test]
    fn default_deny_when_enabled_without_permissive_policy() {
        let db = db("deny");
        seed(&db);
        // RLS enabled, but no policy governs SELECT ⇒ literal FALSE ⇒ 0 rows.
        db.enable_rls("orders", false).unwrap();
        assert_eq!(nrows(db.query("SELECT id FROM orders", &[]).unwrap()), 0);
        // Disabling RLS restores full visibility.
        db.disable_rls("orders").unwrap();
        assert_eq!(nrows(db.query("SELECT id FROM orders", &[]).unwrap()), 3);
    }

    #[test]
    fn delete_only_touches_visible_rows() {
        let db = db("delete");
        seed(&db);
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        // As tenant 2, delete all "my" rows: only id 3 is visible/deletable.
        let affected = db.query_ctx(&sess(2), "DELETE FROM orders", &[]).unwrap();
        assert_eq!(affected, ExecResult::Affected(1));
        // tenant 1's rows survive.
        assert_eq!(nrows(db.query_ctx(&sess(1), "SELECT id FROM orders", &[]).unwrap()), 2);
        db.verify().unwrap();
    }

    #[test]
    fn with_check_gates_inserts() {
        let db = db("wcheck");
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        // My own tenant row: WITH CHECK (falls back to USING) passes.
        db.query_ctx(&sess(5), "INSERT INTO orders (id, tenant, note) VALUES (1, 5, NULL)", &[])
            .unwrap();
        // For another tenant: rejected — RLS is a WRITE/integrity vector (§6.1).
        assert!(matches!(
            db.query_ctx(&sess(5), "INSERT INTO orders (id, tenant, note) VALUES (2, 6, NULL)", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
        // NULL tenant is REJECTED (the §3.7 fix: eval_filter, not the CHECK-loop
        // rule under which NULL would pass and leak a public-row).
        assert!(matches!(
            db.query_ctx(&sess(5), "INSERT INTO orders (id, tenant, note) VALUES (3, NULL, NULL)", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
        assert_eq!(nrows(db.query_ctx(&sess(5), "SELECT id FROM orders", &[]).unwrap()), 1);
        db.verify().unwrap();
    }

    #[test]
    fn with_check_gates_update_post_image() {
        let db = db("wcheckupd");
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        db.query_ctx(&sess(5), "INSERT INTO orders (id, tenant, note) VALUES (1, 5, NULL)", &[])
            .unwrap();
        // In-tenant update: allowed.
        assert_eq!(
            db.query_ctx(&sess(5), "UPDATE orders SET note = 'x' WHERE id = 1", &[]).unwrap(),
            ExecResult::Affected(1)
        );
        // Moving the row to another tenant: the post-image WITH CHECK rejects it.
        assert!(matches!(
            db.query_ctx(&sess(5), "UPDATE orders SET tenant = 6 WHERE id = 1", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
        db.verify().unwrap();
    }

    #[test]
    fn default_deny_blocks_inserts_without_an_insert_policy() {
        let db = db("denyins");
        // A SELECT-only policy governs reads but NOT writes.
        db.create_policy(
            "orders",
            &PolicyDef {
                name: "read_only".into(),
                command: PolicyCmd::Select,
                permissive: true,
                using_src: Some("tenant = current_setting('app.tenant')".into()),
                check_src: None,
            },
        )
        .unwrap();
        db.enable_rls("orders", false).unwrap();
        // No policy governs INSERT ⇒ WITH CHECK is literal FALSE ⇒ all denied.
        assert!(matches!(
            db.query_ctx(&sess(5), "INSERT INTO orders (id, tenant, note) VALUES (1, 5, NULL)", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
    }

    #[test]
    fn cached_plan_goes_stale_after_policy_edit() {
        let db = db("stale");
        seed(&db); // 3 rows, no RLS yet
        // Prepare + execute a by-hash plan with NO policy: sees all 3 rows.
        let h = db.prepare("SELECT id FROM orders").unwrap();
        assert_eq!(nrows(db.execute(&h, &[]).unwrap()), 3);
        // Enable a tenant policy after the plan was cached.
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        // The cached by-hash plan is now stale (compiled pre-RLS) ⇒ PlanInvalidated,
        // NOT a silent leak of all rows.
        assert!(matches!(db.execute(&h, &[]), Err(mpedb_types::Error::PlanInvalidated)));
        // Re-preparing picks up the policy (different plan + hash).
        let h2 = db.prepare("SELECT id FROM orders").unwrap();
        assert_ne!(h, h2);
        assert_eq!(nrows(db.execute_ctx(&sess(1), &h2, &[]).unwrap()), 2);
        // query_ctx always compiles fresh, so it is never stale.
        assert_eq!(nrows(db.query_ctx(&sess(2), "SELECT id FROM orders", &[]).unwrap()), 1);
    }

    #[test]
    fn identical_policy_recreation_is_not_stale() {
        let db = db("noop");
        seed(&db);
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        let h = db.prepare("SELECT id FROM orders").unwrap();
        // Recreate the byte-identical policy: bumps the epoch but not the content
        // hash, so the cached plan is still valid (no spurious invalidation).
        db.create_policy("orders", &tenant_policy()).unwrap();
        assert_eq!(nrows(db.execute_ctx(&sess(1), &h, &[]).unwrap()), 2);
    }

    #[test]
    fn stale_cached_write_plan_is_invalidated() {
        let db = db("stalewrite");
        // Prepare an INSERT plan with no RLS.
        let h = db.prepare("INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)").unwrap();
        db.execute(&h, &[Value::Int(1), Value::Int(1)]).unwrap();
        // Turn on a WITH CHECK-bearing policy; the cached INSERT plan is stale.
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        assert!(matches!(
            db.execute(&h, &[Value::Int(2), Value::Int(2)]),
            Err(mpedb_types::Error::PlanInvalidated)
        ));
        db.verify().unwrap();
    }

    #[test]
    fn rls_via_sql_ddl() {
        let db = db("ddl");
        seed(&db); // 3 rows before RLS
        // Full policy lifecycle expressed as SQL text.
        db.query(
            "CREATE POLICY tenant_iso ON orders FOR ALL \
             USING (tenant = current_setting('app.tenant')) \
             WITH CHECK (tenant = current_setting('app.tenant'))",
            &[],
        )
        .unwrap();
        db.query("ALTER TABLE orders ENABLE ROW LEVEL SECURITY", &[]).unwrap();
        // Reads filter per session.
        assert_eq!(nrows(db.query_ctx(&sess(1), "SELECT id FROM orders", &[]).unwrap()), 2);
        assert_eq!(nrows(db.query_ctx(&sess(2), "SELECT id FROM orders", &[]).unwrap()), 1);
        // WITH CHECK gates writes to another tenant.
        assert!(matches!(
            db.query_ctx(&sess(1), "INSERT INTO orders (id, tenant, note) VALUES (10, 2, NULL)", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
        // DROP POLICY: RLS still enabled with no permissive policy ⇒ default-deny.
        db.query("DROP POLICY tenant_iso ON orders", &[]).unwrap();
        assert_eq!(nrows(db.query("SELECT id FROM orders", &[]).unwrap()), 0);
        // DISABLE restores full visibility.
        db.query("ALTER TABLE orders DISABLE ROW LEVEL SECURITY", &[]).unwrap();
        assert_eq!(nrows(db.query("SELECT id FROM orders", &[]).unwrap()), 3);
        db.verify().unwrap();
    }

    #[test]
    fn ddl_restrictive_and_for_clause() {
        let db = db("ddlvariants");
        seed(&db);
        // A permissive tenant policy + a restrictive "not archived" write gate.
        db.query(
            "CREATE POLICY p_read ON orders FOR SELECT USING (tenant = current_setting('app.tenant'))",
            &[],
        )
        .unwrap();
        db.query(
            "CREATE POLICY p_write ON orders AS RESTRICTIVE FOR INSERT WITH CHECK (id < 100)",
            &[],
        )
        .unwrap();
        db.query("ALTER TABLE orders ENABLE ROW LEVEL SECURITY", &[]).unwrap();
        // SELECT policy filters reads.
        assert_eq!(nrows(db.query_ctx(&sess(1), "SELECT id FROM orders", &[]).unwrap()), 2);
        // No permissive INSERT policy ⇒ default-deny even though id<100 holds.
        assert!(matches!(
            db.query_ctx(&sess(1), "INSERT INTO orders (id, tenant, note) VALUES (5, 1, NULL)", &[]),
            Err(mpedb_types::Error::PolicyViolation { .. })
        ));
    }

    #[test]
    fn policy_predicate_pins_key_access() {
        // A policy on the PK column should still yield a Point/Range access,
        // not degrade to a full scan (footprint only narrows, §3.3).
        let db = db("pin");
        seed(&db);
        db.create_policy(
            "orders",
            &PolicyDef {
                name: "own_id".into(),
                command: PolicyCmd::Select,
                permissive: true,
                using_src: Some("id = current_setting('app.tenant')".into()),
                check_src: None,
            },
        )
        .unwrap();
        db.enable_rls("orders", false).unwrap();
        // session "tenant" reused as an id here; id=3 exists.
        assert_eq!(nrows(db.query_ctx(&sess(3), "SELECT id FROM orders", &[]).unwrap()), 1);
        assert_eq!(nrows(db.query_ctx(&sess(99), "SELECT id FROM orders", &[]).unwrap()), 0);
    }
}
