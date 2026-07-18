//! Hash-sharded single table across K files (a production form of the measured
//! `shardbench` win: on an 11-core M3 Pro, K=8 gave 3.84x near-linear write
//! throughput — separate files ⇒ separate writer locks + meta roots ⇒ parallel
//! commits). The plan's precomputed **footprint** gives the exact shard for a
//! point operation, so routing is abort-free ("pre-computed locks route to
//! shards"); non-point statements fan out.
//!
//! v1 scope (correct + useful, point-heavy OLTP):
//! - INSERT (single row) / SELECT·UPDATE·DELETE with a `PkPoint` access →
//!   routed to `hash(pk) % K` (the win).
//! - non-point SELECT → fan out + concatenate. A cross-shard SELECT with
//!   ORDER BY / LIMIT / OFFSET is rejected (global merge is v2).
//! - non-point UPDATE / DELETE → applied to every shard, affected counts summed.
//! - multi-row INSERT, transactions, and RLS on shards are out of v1 scope.

use crate::{Database, DetachedPlan, ExecResult};
use mpedb_sql::{AccessPath, CompiledPlan, InsertSource, PlanStmt, SelectPlan};
use mpedb_types::value::write_value;
use mpedb_types::{Config, Error, KeyPart, Result, Schema, Value};
use std::path::PathBuf;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A single logical table sharded over K independent database files.
pub struct ShardSet {
    shards: Vec<Database>,
    schema: Schema,
}

enum Route {
    One(usize),
    All,
}

fn shard_path(base: &std::path::Path, i: usize) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".shard{i}"));
    PathBuf::from(s)
}

fn hash_value(h: &mut u64, v: &Value) {
    let mut buf = Vec::new();
    write_value(&mut buf, v);
    for &b in &buf {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

impl ShardSet {
    /// Open (or create) `k` shards for the table described by `config_path`.
    /// Each shard is `<path>.shard<i>` with the identical schema/geometry.
    pub fn open(config_path: &std::path::Path, k: usize) -> Result<ShardSet> {
        ShardSet::open_config(Config::from_file(config_path)?, k)
    }

    pub fn open_config(base: Config, k: usize) -> Result<ShardSet> {
        if k == 0 {
            return Err(Error::Config("ShardSet needs at least 1 shard".into()));
        }
        let schema = base.schema.clone();
        let mut shards = Vec::with_capacity(k);
        for i in 0..k {
            let mut cfg = base.clone();
            cfg.options.path = shard_path(&base.options.path, i);
            shards.push(Database::open_with_config(cfg)?);
        }
        Ok(ShardSet { shards, schema })
    }

    pub fn shards(&self) -> usize {
        self.shards.len()
    }

    /// Compile once against the shared schema, route by the plan's footprint,
    /// and execute on the routed shard (point ops) or fan out (non-point).
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        let plan = mpedb_sql::prepare(sql, &self.schema)?;
        // The ShardSet is a "client" carrying the plan to each shard — the
        // detached-plan path validates the (identical) schema and needs no
        // per-shard registry write.
        let detached = DetachedPlan {
            hash: plan.hash(),
            blob: plan.encode(),
            sql: sql.to_owned(),
        };
        match self.route(&plan, params)? {
            Route::One(s) => self.shards[s].execute_detached(&detached, params),
            Route::All => self.fan_out(&plan, &detached, params),
        }
    }

    fn route(&self, plan: &CompiledPlan, params: &[Value]) -> Result<Route> {
        let k = self.shards.len() as u64;
        // A subplan-result slot is filled by the EXECUTOR (per shard, at run
        // time); hashing its hole here would route on NULL. Fan out instead.
        if !plan.subplans.is_empty() {
            return Ok(Route::All);
        }
        match &plan.stmt {
            PlanStmt::Insert { table, rows, .. } => {
                if rows.len() != 1 {
                    return Err(Error::Unsupported(
                        "multi-row INSERT across shards is not supported (v1)".into(),
                    ));
                }
                let t = self
                    .schema
                    .table(*table)
                    .ok_or_else(|| Error::Internal("shard: table id out of range".into()))?;
                let mut h = FNV_OFFSET;
                for &pk in &t.primary_key {
                    let v = match &rows[0][pk as usize] {
                        InsertSource::Param(i) => params
                            .get(*i as usize)
                            .ok_or_else(|| Error::Internal("shard: pk param".into()))?,
                        InsertSource::Const(i) => plan
                            .consts
                            .get(*i as usize)
                            .ok_or_else(|| Error::Internal("shard: pk const".into()))?,
                        InsertSource::Default => {
                            return Err(Error::Unsupported(
                                "cannot shard-route an INSERT whose primary key uses DEFAULT".into(),
                            ))
                        }
                    };
                    hash_value(&mut h, v);
                }
                Ok(Route::One((h % k) as usize))
            }
            PlanStmt::Select(SelectPlan { access, .. })
            | PlanStmt::Update { access, .. }
            | PlanStmt::Delete { access, .. } => match access {
                AccessPath::PkPoint(parts) => {
                    let mut h = FNV_OFFSET;
                    for part in parts {
                        let v = self.resolve_part(part, plan, params)?;
                        hash_value(&mut h, &v);
                    }
                    Ok(Route::One((h % k) as usize))
                }
                _ => Ok(Route::All),
            },
            // A compound reads several access paths; per-shard routing has no
            // single key to hash, so it fans out like any non-point read. A
            // recursive CTE reads several tables plus a working table — likewise
            // no single routing key, so it fans out too.
            PlanStmt::Compound(_) | PlanStmt::RecursiveCte(_) => Ok(Route::All),
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Err(Error::Unsupported(
                "explicit transactions are not supported on a ShardSet (v1)".into(),
            )),
        }
    }

    fn resolve_part(&self, part: &KeyPart, plan: &CompiledPlan, params: &[Value]) -> Result<Value> {
        Ok(match part {
            KeyPart::Param(i) => params
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| Error::Internal("shard: key param".into()))?,
            KeyPart::Const(i) => plan
                .consts
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| Error::Internal("shard: key const".into()))?,
            // Routing keys come from statement-level access paths, which can
            // never carry an outer-column part (validate refuses them there).
            KeyPart::OuterCol(_) => {
                return Err(Error::Internal(
                    "shard: outer-column key part outside a join".into(),
                ))
            }
        })
    }

    fn fan_out(
        &self,
        plan: &CompiledPlan,
        detached: &DetachedPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        match &plan.stmt {
            PlanStmt::Select(SelectPlan {
                order_by,
                limit,
                offset,
                ..
            }) => {
                if !order_by.is_empty() || limit.is_some() || offset.is_some() {
                    return Err(Error::Unsupported(
                        "cross-shard SELECT with ORDER BY/LIMIT/OFFSET is not supported (v1); \
                         query by primary key or merge client-side"
                            .into(),
                    ));
                }
                let mut columns = Vec::new();
                let mut rows = Vec::new();
                for s in &self.shards {
                    match s.execute_detached(detached, params)? {
                        ExecResult::Rows { columns: c, rows: r } => {
                            columns = c;
                            rows.extend(r);
                        }
                        other => {
                            return Err(Error::Internal(format!(
                                "shard fan-out expected rows, got {other:?}"
                            )))
                        }
                    }
                }
                Ok(ExecResult::Rows { columns, rows })
            }
            PlanStmt::Update { .. } | PlanStmt::Delete { .. } => {
                let mut total = 0u64;
                for s in &self.shards {
                    if let ExecResult::Affected(n) = s.execute_detached(detached, params)? {
                        total += n;
                    }
                }
                Ok(ExecResult::Affected(total))
            }
            _ => Err(Error::Internal("shard: unexpected fan-out statement".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params;

    fn shards(tag: &str, k: usize) -> crate::testdb::Owned<ShardSet> {
        let path = crate::testdb::scratch_path(format!(
            "mpedb-shardset-{tag}-{}.mpedb",
            std::process::id()
        ));
        // A ShardSet fans out into <path>.shard0..k; every one of them is ours
        // to remove, panic or not.
        let files: Vec<std::path::PathBuf> =
            (0..k).map(|i| format!("{path}.shard{i}").into()).collect();
        for p in &files {
            let _ = std::fs::remove_file(p);
        }
        let cfg = Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"kv\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"v\"\n  type = \"int64\"\n  nullable = false"
        ))
        .unwrap();
        crate::testdb::Owned::new(ShardSet::open_config(cfg, k).unwrap(), files)
    }

    fn nrows(r: ExecResult) -> usize {
        match r {
            ExecResult::Rows { rows, .. } => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn point_ops_route_and_reads_find_their_shard() {
        let ss = shards("route", 4);
        for id in 0..200i64 {
            ss.query("INSERT INTO kv (id, v) VALUES ($1, $2)", &params![id, id * 10])
                .unwrap();
        }
        // Every point read finds its row (routed to the same shard as the insert).
        for id in 0..200i64 {
            let got = ss.query("SELECT v FROM kv WHERE id = $1", &params![id]).unwrap();
            match got {
                ExecResult::Rows { rows, .. } => {
                    assert_eq!(rows.len(), 1, "id {id} not found");
                    assert_eq!(rows[0][0], Value::Int(id * 10));
                }
                other => panic!("{other:?}"),
            }
        }
        // A point UPDATE routes to one shard and affects exactly one row.
        assert_eq!(
            ss.query("UPDATE kv SET v = 999 WHERE id = 5", &[]).unwrap(),
            ExecResult::Affected(1)
        );
        assert_eq!(
            ss.query("DELETE FROM kv WHERE id = 5", &[]).unwrap(),
            ExecResult::Affected(1)
        );
    }

    #[test]
    fn full_scan_fans_out_and_unions_all_shards() {
        let ss = shards("fanout", 4);
        for id in 0..200i64 {
            ss.query("INSERT INTO kv (id, v) VALUES ($1, 1)", &params![id]).unwrap();
        }
        // A non-point SELECT fans out to all shards and unions the rows.
        assert_eq!(nrows(ss.query("SELECT id FROM kv", &[]).unwrap()), 200);
        // A non-point UPDATE/DELETE applies across shards, counts summed.
        assert_eq!(
            ss.query("UPDATE kv SET v = 2 WHERE v = 1", &[]).unwrap(),
            ExecResult::Affected(200)
        );
        // Cross-shard ORDER BY/LIMIT is rejected in v1.
        assert!(matches!(
            ss.query("SELECT id FROM kv ORDER BY id LIMIT 10", &[]),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn distribution_across_shards_is_balanced() {
        let ss = shards("dist", 8);
        for id in 0..800i64 {
            ss.query("INSERT INTO kv (id, v) VALUES ($1, 0)", &params![id]).unwrap();
        }
        // Each shard should hold a nontrivial slice (hash spread), summing to 800.
        let mut total = 0usize;
        for s in &ss.shards {
            let n = nrows(s.query("SELECT id FROM kv", &[]).unwrap());
            assert!(n > 0, "a shard is empty — poor hash spread");
            total += n;
        }
        assert_eq!(total, 800);
    }
}
