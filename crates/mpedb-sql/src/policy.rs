//! Per-table RLS policy sets handed to the planner at prepare time
//! (DESIGN-MULTIDB.md §3). Built by the facade from the catalog sys-keyspace;
//! the planner reads it to inject `USING`/`WITH CHECK` predicates.

use mpedb_types::PolicyDef;
use std::collections::{HashMap, HashSet};

/// A table's RLS state: whether row security is enabled and its policies.
#[derive(Debug, Clone, Default)]
pub struct TablePolicies {
    pub rls_enabled: bool,
    /// `FORCE ROW LEVEL SECURITY` (applies RLS even to the table owner; in
    /// mpedb's ownerless model it mainly documents intent — DESIGN-MULTIDB §6.5).
    pub force: bool,
    /// Monotonic per-table policy epoch (bumped on any policy edit). Recorded
    /// on plans and compared against the live value to detect staleness
    /// (Phase-5 plan-cache leak-proofing, DESIGN-MULTIDB.md §4).
    pub epoch: u64,
    pub policies: Vec<PolicyDef>,
}

/// A canonical **content** hash of a table's RLS state (rls flags + policies in
/// a deterministic order), independent of the epoch. `None` (no catalog entry)
/// hashes identically to an explicit empty/disabled state, so a table that
/// never had RLS and one whose policies were dropped agree. Mixed into the
/// plan hash and compared at execute time; a policy edit that produced
/// byte-identical content is therefore *not* a spurious invalidation.
pub fn table_policy_hash(tp: Option<&TablePolicies>) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    // Canonicalize so `None` (no catalog entry) and an explicit disabled/empty
    // state produce the SAME hash — otherwise validation would false-positive.
    let (rls_enabled, force) = tp.map_or((false, false), |t| (t.rls_enabled, t.force));
    h.update(&[u8::from(rls_enabled), u8::from(force)]);
    // Deterministic order: by name (the storage key is unique per name).
    let mut policies: Vec<&PolicyDef> = tp.map(|t| t.policies.iter().collect()).unwrap_or_default();
    policies.sort_by(|a, b| a.name.cmp(&b.name));
    h.update(&(policies.len() as u32).to_le_bytes());
    for p in policies {
        h.update(&(p.name.len() as u32).to_le_bytes());
        h.update(p.name.as_bytes());
        h.update(&p.encode_value());
    }
    *h.finalize().as_bytes()
}

/// All tables' policies for one prepare. Empty ⇒ no RLS anywhere (the planner
/// injects nothing and behaves exactly as before).
#[derive(Debug, Clone, Default)]
pub struct PolicyCatalog {
    tables: HashMap<u32, TablePolicies>,
    /// Tables the deployment declares MUST be policy-protected (§6.3), from
    /// `DbOptions::require_policy`. Kept OUT of `TablePolicies` on purpose: that
    /// struct mirrors the file's sys-keyspace catalog and is content-hashed into
    /// plans, whereas this is per-process config. Mixing them would make one
    /// process's assertion change another's plan hash.
    require: HashSet<u32>,
}

impl PolicyCatalog {
    pub fn empty() -> PolicyCatalog {
        PolicyCatalog::default()
    }

    pub fn set_table(&mut self, table_id: u32, tp: TablePolicies) {
        self.tables.insert(table_id, tp);
    }

    pub fn get(&self, table_id: u32) -> Option<&TablePolicies> {
        self.tables.get(&table_id)
    }

    /// Declare `table_id` tenant-scoped: a prepare touching it must find RLS
    /// enabled and a policy governing the command (§6.3).
    pub fn set_require_policy(&mut self, table_id: u32) {
        self.require.insert(table_id);
    }

    pub fn requires_policy(&self, table_id: u32) -> bool {
        self.require.contains(&table_id)
    }

    /// No RLS state AND no assertions ⇒ the planner injects nothing and behaves
    /// exactly as before. The `require` set must count: a catalog holding only
    /// assertions still has to be consulted, or a table declared tenant-scoped
    /// but never policy-protected would be skipped — the exact §6.3 hole.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.require.is_empty()
    }
}
