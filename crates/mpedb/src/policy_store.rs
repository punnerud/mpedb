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

    /// **Tenant-leading-key lint (DESIGN-MULTIDB.md §6.4).** Returns human-readable
    /// findings for a policy about to be created — never an error, and never
    /// blocking: a leaky key is a design smell the author may have accepted, not a
    /// bug the database gets to veto.
    ///
    /// The leak it looks for: uniqueness is enforced over ALL rows regardless of
    /// visibility, so a UNIQUE/PK that does not lead with the policy's
    /// discriminator can collide with an invisible row and tell the caller it
    /// exists. Leading with the discriminator confines collisions to the caller's
    /// own partition, which makes them non-leaking (and makes §6.5's error
    /// normalization harmless rather than merely opaque).
    ///
    /// An uncomfortable truth this surfaces, which is exactly why it is worth
    /// running: **mpedb's secondary unique indexes are single-column**
    /// (`planner::secondary_indexes`), so a `unique` column on an RLS table can
    /// NEVER be tenant-scoped by key design. There is no `(tenant, code)` unique
    /// to write. The only fixes are dropping the uniqueness or moving the
    /// constraint out of the shared table — so the lint says that plainly instead
    /// of suggesting an impossible remedy.
    pub fn lint_policy(&self, table: &str, def: &PolicyDef) -> Result<Vec<String>> {
        let table_id = self.require_table_id(table)?;
        let bundle = self.schema();
        let t = bundle
            .table(table_id)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        let mut disc: Vec<u16> = Vec::new();
        for src in [def.using_src.as_deref(), def.check_src.as_deref()].into_iter().flatten() {
            disc.extend(mpedb_sql::policy_discriminators(src, t));
        }
        disc.sort_unstable();
        disc.dedup();

        let mut out = Vec::new();
        if disc.is_empty() {
            // Nothing to lead with: not a finding, just nothing to say.
            return Ok(out);
        }
        let names: Vec<&str> = disc.iter().map(|&i| t.columns[i as usize].name.as_str()).collect();

        let pk_lead = t.primary_key.first().copied();
        if !pk_lead.is_some_and(|l| disc.contains(&l)) {
            let pk_names: Vec<&str> = t
                .primary_key
                .iter()
                .map(|&i| t.columns[i as usize].name.as_str())
                .collect();
            out.push(format!(
                "PRIMARY KEY ({}) does not lead with the policy discriminator ({}): a PK \
                 collision with a row this caller cannot see still fails the write, revealing \
                 the hidden row exists (§6.4). Consider PRIMARY KEY ({}, …).",
                pk_names.join(", "),
                names.join(" or "),
                names[0],
            ));
        }
        for ix in &t.indexes {
            // §6.4 generalized over `TableDef.indexes`: an index whose
            // LEADING column is the discriminator cannot act as a
            // cross-tenant existence oracle (every probe supplies the tenant
            // first). A composite unique leading with the discriminator is
            // exactly the fix the old single-column advice called
            // impossible.
            let leading = ix.columns[0];
            if !disc.contains(&leading) {
                let cols: Vec<&str> = ix
                    .columns
                    .iter()
                    .map(|&c| t.columns[c as usize].name.as_str())
                    .collect();
                out.push(format!(
                    "index ({}) spans every tenant: a value colliding with a hidden \
                     row reveals it exists (§6.4). Lead the index with `{}` (a \
                     composite index whose first column is the discriminator), drop \
                     it, or move the keyed data to its own table.",
                    cols.join(", "),
                    names[0],
                ));
            }
        }
        Ok(out)
    }

    /// Create (or replace) an RLS policy on `table` (DESIGN-MULTIDB.md §3.1).
    /// The `USING`/`WITH CHECK` sources are validated against the table before
    /// storage. Must not be called while a [`WriteSession`](crate::WriteSession)
    /// from this handle is open (it takes the writer lock).
    pub fn create_policy(&self, table: &str, def: &PolicyDef) -> Result<()> {
        let table_id = self.require_table_id(table)?;
        let bundle = self.schema();
        let t = bundle
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
        // Fold in this process's `require_policy = true` declarations (§6.3).
        // They come from config, never the file, so they are layered on top of
        // the file's catalog here rather than read out of the sys-keyspace.
        // (Name→id resolution and validation already happened at open.)
        for &table_id in &self.require_policy {
            cat.set_require_policy(table_id);
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

/// Decide whether one stamped table is stale relative to `live_epoch` (already
/// read from the executing pin). Fast path: epochs equal ⇒ current. Slow path:
/// the epoch moved, so recompute that table's live policy content hash from
/// `scan` and compare — a no-op edit still matches, a real edit is stale.
fn is_stale(
    stamp: &mpedb_sql::PolicyStamp,
    live_epoch: u64,
    scan: &[(Vec<u8>, Vec<u8>)],
) -> Result<bool> {
    if live_epoch == stamp.epoch {
        return Ok(false);
    }
    let tp = one_table_from_scan(scan, stamp.table)?;
    Ok(mpedb_sql::table_policy_hash(Some(&tp)) != stamp.hash)
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
        // EVERY table whose policy the plan baked in, not just the first. A
        // join bakes two, and checking only the outer would let a cached plan
        // keep serving the inner table's rows under a policy that has since
        // been tightened — §4's leak, reopened by the table it forgot.
        for stamp in &plan.policies {
            let live_epoch = r
                .sys_get(&with_table_id(POLEP_PREFIX, stamp.table))?
                .map_or(0, |b| epoch_of(&b));
            if live_epoch == stamp.epoch {
                continue;
            }
            if is_stale(stamp, live_epoch, &r.sys_scan()?)? {
                self.evict(hash);
                return Err(Error::PlanInvalidated);
            }
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
        for stamp in &plan.policies {
            let live_epoch = w
                .sys_get(&with_table_id(POLEP_PREFIX, stamp.table))?
                .map_or(0, |b| epoch_of(&b));
            if live_epoch == stamp.epoch {
                continue;
            }
            if is_stale(stamp, live_epoch, &w.sys_scan()?)? {
                self.evict(hash);
                return Err(Error::PlanInvalidated);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Database, ExecResult, PolicyCmd, PolicyDef, Session};
    use mpedb_types::{Config, Error as E, Value};

    fn db(tag: &str) -> crate::testdb::TestDb {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-{tag}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"note\"\n  type = \"text\"\n  nullable = true"
        ))
        .unwrap();
        crate::testdb::TestDb::new_db(Database::open_with_config(cfg).unwrap())
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

    // ---- §6.3 require_policy: the fail-closed deployment assertion ----

    /// Same schema, but `orders` is declared tenant-scoped.
    fn db_requiring(tag: &str) -> crate::testdb::TestDb {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-{tag}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\nrequire_policy = true\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"note\"\n  type = \"text\"\n  nullable = true"
        ))
        .unwrap();
        crate::testdb::TestDb::new_db(Database::open_with_config(cfg).unwrap())
    }

    /// The whole point of §6.3: forgetting `ENABLE ROW LEVEL SECURITY` is
    /// SILENT — the table reads like a working one and hands every row to every
    /// caller. With the assertion, prepare refuses instead.
    #[test]
    fn require_policy_fails_closed_when_rls_was_never_enabled() {
        let db = db_requiring("req-off");
        // Without the assertion this SELECT would happily return all 3 rows.
        let err = db.query("SELECT id FROM orders", &[]);
        assert!(
            matches!(&err, Err(E::Config(m)) if m.contains("require_policy")
                     && m.contains("row-level security is not enabled")),
            "expected a fail-closed config error, got {err:?}"
        );
        // and it is not a read-only quirk — writes are refused too
        let w = db.query(
            "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
            &[Value::Int(9), Value::Int(1)],
        );
        assert!(matches!(w, Err(E::Config(_))), "got {w:?}");

    }

    /// RLS on but no policy governs the command: our empty-permissive rule
    /// already default-denies, so this is SAFE — but "the table is mysteriously
    /// empty" is a worse diagnostic than an error, and it is never what someone
    /// who wrote require_policy meant. Assert it too.
    #[test]
    fn require_policy_fails_closed_when_no_policy_governs_the_command() {
        let db = db_requiring("req-nopol");
        db.enable_rls("orders", false).unwrap();
        let err = db.query("SELECT id FROM orders", &[]);
        assert!(
            matches!(&err, Err(E::Config(m)) if m.contains("no policy governs")),
            "expected a fail-closed config error, got {err:?}"
        );
    }

    /// Properly protected ⇒ the assertion is invisible and filtering is normal.
    ///
    /// Note the ORDER this test is forced into: policy and RLS come first, and
    /// only then can rows be inserted (each under its own tenant context). That
    /// is the assertion working as intended — a table declared tenant-scoped
    /// cannot be seeded through an unprotected window, which is precisely the
    /// window §6.3 is about. The other RLS tests seed first *because* they do not
    /// declare require_policy.
    #[test]
    fn require_policy_is_satisfied_by_a_governing_policy() {
        let db = db_requiring("req-ok");
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        for (id, t) in [(1, 1), (2, 1), (3, 2)] {
            db.query_ctx(
                &sess(t),
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
        assert_eq!(nrows(db.query_ctx(&sess(1), "SELECT id FROM orders", &[]).unwrap()), 2);
        assert_eq!(nrows(db.query_ctx(&sess(2), "SELECT id FROM orders", &[]).unwrap()), 1);
    }

    /// A typo'd/renamed table name must fail at OPEN, not silently assert
    /// nothing forever.
    #[test]
    fn require_policy_naming_an_unknown_table_fails_at_open() {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-req-typo-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
             [[table]]\nname = \"ordrs\"\nprimary_key = [\"id\"]\nrequire_policy = true\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\""
        ))
        .unwrap();
        // `ordrs` IS in this schema, so it opens; rename it away and it must not
        let ok = Database::open_with_config(cfg);
        assert!(ok.is_ok(), "a declared table that exists must open");
        drop(ok);
        let _ = std::fs::remove_file(&path);

        let cfg2 = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\""
        ))
        .unwrap();
        // hand-inject an assertion for a table that is not in the schema
        let mut cfg2 = cfg2;
        cfg2.options.require_policy.insert("ghost".into());
        let err = Database::open_with_config(cfg2).err();
        assert!(
            matches!(&err, Some(E::Config(m)) if m.contains("ghost") && m.contains("not in the schema")),
            "expected an open-time config error, got {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The assertion must not change the plan hash: it is per-process config, so
    /// two processes disagreeing about it must still share the plan registry.
    #[test]
    fn require_policy_does_not_affect_the_plan_hash() {
        let db_plain = db("req-hash-a");
        let db_req = db_requiring("req-hash-b");
        db_req.create_policy("orders", &tenant_policy()).unwrap();
        db_req.enable_rls("orders", false).unwrap();
        db_plain.create_policy("orders", &tenant_policy()).unwrap();
        db_plain.enable_rls("orders", false).unwrap();

        let h1 = db_plain.prepare("SELECT id FROM orders WHERE id = $1").unwrap();
        let h2 = db_req.prepare("SELECT id FROM orders WHERE id = $1").unwrap();
        assert_eq!(h1, h2, "require_policy is config, not policy content — it must not rehash");

        for d in [&db_plain, &db_req] {
            let _ = std::fs::remove_file(d.path());
        }
    }

    // ---- §6.5 the classification oracle ----

    /// The attack §6.5 describes, run for real: tenant 2 probes for tenant 1's
    /// hidden rows. Uniqueness pre-checks span the whole B+tree with no RLS
    /// awareness, so a colliding INSERT is rejected even though the colliding row
    /// is invisible. That much cannot be fixed (§6.4 — it needs tenant-leading
    /// keys). What CAN be fixed is the probe learning *which attribute* matched:
    /// `PrimaryKeyViolation` vs `UniqueViolation{constraint: "email"}` vs
    /// `CheckViolation{column, expr}` reconstructs hidden rows attribute by
    /// attribute. On an RLS table those collapse to one opaque `WriteRejected`.
    #[test]
    fn rls_hides_which_constraint_a_hidden_row_collided_with() {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-oracle-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"code\"\n  type = \"text\"\n  unique = true"
        ))
        .unwrap();
        let db = Database::open_with_config(cfg).unwrap();

        // tenant 1 owns id=1 with code="secret" — invisible to tenant 2.
        db.query(
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(1), Value::Int(1), Value::Text("secret".into())],
        )
        .unwrap();

        // BEFORE RLS: the taxonomy is fully informative (this is the oracle).
        let pk = db.query(
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(1), Value::Int(2), Value::Text("mine".into())],
        );
        assert!(matches!(pk, Err(E::PrimaryKeyViolation { .. })), "got {pk:?}");
        let uq = db.query(
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(9), Value::Int(2), Value::Text("secret".into())],
        );
        // it even names the colliding column
        assert!(
            matches!(&uq, Err(E::UniqueViolation { constraint, .. }) if constraint.contains("code")),
            "got {uq:?}"
        );

        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();

        // AFTER RLS: both probes give the SAME opaque answer. Tenant 2 still
        // learns something collided (the existence oracle, §6.4) but no longer
        // learns whether it was the PK or `code`.
        let pk2 = db.query_ctx(
            &sess(2),
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(1), Value::Int(2), Value::Text("mine2".into())],
        );
        let uq2 = db.query_ctx(
            &sess(2),
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(9), Value::Int(2), Value::Text("secret".into())],
        );
        assert!(matches!(pk2, Err(E::WriteRejected { .. })), "got {pk2:?}");
        assert!(matches!(uq2, Err(E::WriteRejected { .. })), "got {uq2:?}");
        assert_eq!(
            format!("{}", pk2.unwrap_err()),
            format!("{}", uq2.unwrap_err()),
            "the two probes must be textually indistinguishable, or the oracle survives"
        );

        // A row that violates the caller's OWN policy stays distinguishable —
        // that is the caller's own mistake and leaks nothing about hidden rows.
        let pol = db.query_ctx(
            &sess(2),
            "INSERT INTO orders (id, tenant, code) VALUES ($1, $2, $3)",
            &[Value::Int(50), Value::Int(1), Value::Text("other".into())],
        );
        assert!(matches!(pol, Err(E::PolicyViolation { .. })), "got {pol:?}");

        let _ = std::fs::remove_file(&path);
    }

    /// Without RLS the precise variants must survive — they are what makes a
    /// constraint failure debuggable, and there is no hidden row to protect.
    #[test]
    fn without_rls_the_constraint_taxonomy_is_untouched() {
        let db = db("no-rls-taxonomy");
        seed(&db);
        let e = db.query(
            "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
            &[Value::Int(1), Value::Int(1)],
        );
        assert!(matches!(e, Err(E::PrimaryKeyViolation { .. })), "got {e:?}");
    }

    // ---- §6.4 tenant-leading-key lint ----

    /// `orders(id PK, tenant, code UNIQUE)` — the shape the lint exists for:
    /// a PK that does not lead with `tenant`, and a tenant-spanning unique.
    fn db_leaky(tag: &str) -> crate::testdb::TestDb {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-{tag}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"code\"\n  type = \"text\"\n  unique = true"
        ))
        .unwrap();
        crate::testdb::TestDb::new_db(Database::open_with_config(cfg).unwrap())
    }

    #[test]
    fn lint_flags_a_pk_that_does_not_lead_with_the_discriminator() {
        let db = db_leaky("lint-pk");
        let w = db.lint_policy("orders", &tenant_policy()).unwrap();
        assert!(
            w.iter().any(|m| m.contains("PRIMARY KEY") && m.contains("does not lead")),
            "expected a PK finding, got {w:?}"
        );
        // and it names the honest remedy for the tenant-spanning index:
        // lead a composite with the discriminator (possible since
        // DESIGN-SCHEMA-V2 made composite indexes representable).
        assert!(
            w.iter().any(|m| m.contains("index (code)") && m.contains("Lead the index")),
            "expected the index finding with the composite remedy, got {w:?}"
        );
    }

    /// A tenant-leading PK and no tenant-spanning unique ⇒ nothing to say.
    #[test]
    fn lint_is_silent_when_the_key_leads_with_the_discriminator() {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rls-lint-ok-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _guard = crate::testdb::Owned::new((), vec![path.clone().into()]);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"tenant\", \"id\"]\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\""
        ))
        .unwrap();
        let db = Database::open_with_config(cfg).unwrap();
        assert!(db.lint_policy("orders", &tenant_policy()).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// A policy with no equality-to-context conjunct has no discriminator, so
    /// there is nothing to lead with and the lint must stay quiet rather than
    /// invent a finding.
    #[test]
    fn lint_is_silent_without_a_discriminator() {
        let db = db_leaky("lint-nodisc");
        let public = PolicyDef {
            name: "public_rows".into(),
            command: PolicyCmd::Select,
            permissive: true,
            using_src: Some("tenant > 0".into()),
            check_src: None,
        };
        assert!(db.lint_policy("orders", &public).unwrap().is_empty());
    }

    /// A discriminator under OR does not partition the table (the other branch
    /// admits rows anyway), so it must not count as one.
    #[test]
    fn lint_ignores_a_discriminator_under_or() {
        let db = db_leaky("lint-or");
        let loose = PolicyDef {
            name: "loose".into(),
            command: PolicyCmd::Select,
            permissive: true,
            using_src: Some(
                "tenant = current_setting('app.tenant') OR tenant = 0".into(),
            ),
            check_src: None,
        };
        // no top-level equality conjunct ⇒ no discriminator ⇒ silent
        assert!(db.lint_policy("orders", &loose).unwrap().is_empty());
    }

    /// The findings must actually reach a user: `CREATE POLICY` returns them as
    /// rows so they print through the ordinary result path.
    #[test]
    fn create_policy_ddl_surfaces_the_lint() {
        let db = db_leaky("lint-ddl");
        let r = db
            .query(
                "CREATE POLICY tenant_iso ON orders FOR ALL \
                 USING (tenant = current_setting('app.tenant'))",
                &[],
            )
            .unwrap();
        match r {
            ExecResult::Rows { columns, rows } => {
                assert_eq!(columns, vec!["warning".to_string()]);
                assert!(!rows.is_empty(), "expected lint rows");
            }
            other => panic!("expected lint warnings as rows, got {other:?}"),
        }
        // the policy was still created — the lint informs, it does not veto
        db.enable_rls("orders", false).unwrap();
        assert_eq!(nrows(db.query_ctx(&sess(1), "SELECT id FROM orders", &[]).unwrap()), 0);
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

    /// A two-table database for the join tests: `orders` and `lines`, each
    /// tenant-scoped.
    fn joindb(tag: &str) -> crate::testdb::TestDb {
        let path =
            crate::testdb::scratch_path(format!("mpedb-rlsj-{tag}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"lines\"\nprimary_key = [\"lid\"]\n  \
             [[table.column]]\n  name = \"lid\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"oid\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n\
             [[table]]\nname = \"orders\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"tenant\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"note\"\n  type = \"text\"\n  nullable = true"
        ))
        .unwrap();
        crate::testdb::TestDb::new_db(Database::open_with_config(cfg).unwrap())
    }

    /// RLS applies to BOTH sides of a join. The inner table's policy is not
    /// something the outer's visibility implies: tenant 1 can see order 1, and
    /// that must not carry tenant 2's line rows along with it.
    #[test]
    fn rls_filters_both_sides_of_a_join() {
        let db = joindb("both");
        for (id, t) in [(1, 1), (2, 2)] {
            db.query(
                "INSERT INTO orders (id, tenant, note) VALUES ($1, $2, NULL)",
                &[Value::Int(id), Value::Int(t)],
            )
            .unwrap();
        }
        // Line 20 belongs to ORDER 1 (tenant 1) but is itself tenant 2 — the
        // shape that catches a join which only filters the outer side.
        for (lid, oid, t) in [(10, 1, 1), (20, 1, 2), (30, 2, 2)] {
            db.query(
                "INSERT INTO lines (lid, oid, tenant) VALUES ($1, $2, $3)",
                &[Value::Int(lid), Value::Int(oid), Value::Int(t)],
            )
            .unwrap();
        }
        for t in ["orders", "lines"] {
            db.query(
                &format!(
                    "CREATE POLICY tenant_iso ON {t} FOR ALL \
                     USING (tenant = current_setting('app.tenant'))"
                ),
                &[],
            )
            .unwrap();
            db.query(&format!("ALTER TABLE {t} ENABLE ROW LEVEL SECURITY"), &[])
                .unwrap();
        }

        let sql = "SELECT lines.lid FROM lines JOIN orders ON lines.oid = orders.id \
                   ORDER BY lines.lid";
        // Tenant 1 sees line 10 only. Line 20 joins to a VISIBLE order (order 1
        // is tenant 1) — so it is dropped by `lines`' own policy, which is the
        // whole point.
        match db.query_ctx(&sess(1), sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec![Value::Int(10)]], "tenant 1");
            }
            other => panic!("expected rows, got {other:?}"),
        }
        // Tenant 2 sees line 30 (order 2). Line 20 is tenant 2's, but its order
        // is tenant 1's and therefore invisible — so the inner join has nothing
        // to pair it with.
        match db.query_ctx(&sess(2), sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                assert_eq!(rows, vec![vec![Value::Int(30)]], "tenant 2");
            }
            other => panic!("expected rows, got {other:?}"),
        }
        db.verify().unwrap();
    }

    /// The leak-proofing must cover EVERY table the plan baked a policy for.
    ///
    /// A join stamps two. If only the outer were stamped, a cached plan would
    /// keep serving the inner table's rows under the policy frozen at compile
    /// time — so tightening `lines`' policy would change nothing until the
    /// plan happened to be evicted. That is §4's leak, reopened by the table
    /// the check forgot.
    #[test]
    fn tightening_the_inner_tables_policy_invalidates_a_cached_join_plan() {
        let db = joindb("stale");
        db.query(
            "INSERT INTO orders (id, tenant, note) VALUES (1, 1, NULL)",
            &[],
        )
        .unwrap();
        for (lid, t) in [(10, 1), (20, 1)] {
            db.query(
                "INSERT INTO lines (lid, oid, tenant) VALUES ($1, 1, $2)",
                &[Value::Int(lid), Value::Int(t)],
            )
            .unwrap();
        }
        // `lines` is the OUTER table (it is the one in FROM) and is policed
        // from the start. `orders` — the INNER side — is wide open. Getting
        // this the wrong way round makes the test pass on the outer stamp
        // alone and prove nothing.
        db.query(
            "CREATE POLICY line_iso ON lines FOR ALL \
             USING (tenant = current_setting('app.tenant'))",
            &[],
        )
        .unwrap();
        db.query("ALTER TABLE lines ENABLE ROW LEVEL SECURITY", &[])
            .unwrap();

        let sql = "SELECT lines.lid FROM lines JOIN orders ON lines.oid = orders.id \
                   ORDER BY lines.lid";
        let hash = db.prepare(sql).unwrap();
        assert_eq!(
            nrows(db.execute_ctx(&sess(1), &hash, &[]).unwrap()),
            2,
            "both lines visible while `orders` is unpoliced"
        );

        // Now police the INNER table so its only row disappears. The cached
        // plan still carries the OLD `orders` policy (none), and the OUTER
        // table's epoch has not moved — so only the inner stamp can catch this.
        db.query("CREATE POLICY order_iso ON orders FOR ALL USING (id > 99)", &[])
            .unwrap();
        db.query("ALTER TABLE orders ENABLE ROW LEVEL SECURITY", &[])
            .unwrap();

        // The cached plan must be REFUSED, not silently reused.
        assert!(
            matches!(
                db.execute_ctx(&sess(1), &hash, &[]),
                Err(mpedb_types::Error::PlanInvalidated)
            ),
            "a policy edit on the INNER table must invalidate the cached join plan"
        );
        // Re-preparing picks the new policy up: order 1 is now invisible, so
        // the inner join has nothing to pair the lines with.
        let hash = db.prepare(sql).unwrap();
        assert_eq!(nrows(db.execute_ctx(&sess(1), &hash, &[]).unwrap()), 0);
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

    /// Deliberately panics. Run explicitly:
    ///   cargo test -p mpedb --lib panicking_test -- --ignored
    ///
    /// Proves the Drop guard cleans up on the FAILURE path -- the only path
    /// that ever leaked. A cleanup that only runs on success is exactly the bug
    /// this replaced.
    #[test]
    #[ignore]
    fn panicking_test_still_removes_its_file() {
        let db = db("leakprobe");
        let p = db.path().to_path_buf();
        assert!(p.exists(), "the file must exist while the test runs");
        panic!("deliberate panic; {} must not survive it", p.display());
    }

    /// §6.5, extended to ON CONFLICT. The section closes a classification
    /// oracle by collapsing PrimaryKey/Unique/Check into one opaque
    /// `WriteRejected`, so a caller cannot learn WHICH constraint an invisible
    /// row tripped. `ON CONFLICT DO NOTHING` reopens it exactly: a silent skip
    /// means "unique conflict", an error means "something else". `DO UPDATE` is
    /// worse -- it would overwrite a row the caller cannot see.
    ///
    /// PostgreSQL permits both and documents the inference. mpedb made the §6.5
    /// promise, so the planner refuses rather than quietly weakening it. This
    /// test is the guard on that decision.
    #[test]
    fn on_conflict_is_refused_on_an_rls_table() {
        let db = db("oc-rls");
        db.create_policy("orders", &tenant_policy()).unwrap();
        db.enable_rls("orders", false).unwrap();
        for sql in [
            "INSERT INTO orders (id, tenant) VALUES (1, 1) ON CONFLICT DO NOTHING",
            "INSERT INTO orders (id, tenant) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET tenant = 2",
        ] {
            let r = db.prepare(sql);
            assert!(
                matches!(&r, Err(E::Bind(m))
                    if m.contains("row-level security") && m.contains("6.5")),
                "ON CONFLICT must be refused under RLS, got {r:?} for: {sql}"
            );
        }
        // ...and a plain INSERT still works, so the refusal is scoped to the
        // clause rather than breaking writes on RLS tables.
        db.query("INSERT INTO orders (id, tenant) VALUES (1, 1)", &[]).unwrap_err();
    }
}

