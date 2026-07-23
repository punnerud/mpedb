//! Model-declared DERIVED structures (`[[model.derived]]`,
//! DESIGN-MODEL-LANG §3's maintenance rung): tables the ENGINE builds and
//! keeps current with GENERATED triggers on the source table's writes.
//!
//! The declaration is the contract; [`Database::sync_model_derived`] makes the
//! database match it: a declared structure that is missing (or whose
//! declaration changed — detected by a stored fingerprint) is (re)installed,
//! and an installed structure the model no longer claims is dropped, triggers
//! and table both. Installation is ATOMIC — one write transaction creates the
//! table, creates the maintenance triggers, and backfills from the source's
//! current rows, so no concurrent writer can slip a row between "triggers
//! exist" and "backfill computed" (single-writer makes the ordering argument
//! trivial: we hold the lock for all three).
//!
//! NULL discipline mirrors index membership (CLAUDE.md): a source row with a
//! NULL in any derived-relevant column has NO entry — the generated triggers
//! gate on `IS NOT NULL` (`WHEN` clauses; the UPDATE direction needs two
//! triggers because a body statement cannot carry its own gate), and the
//! backfill filters the same way. Everything generated is ordinary SQL through
//! the ordinary DDL/trigger machinery — compiled, gen-gated, fired,
//! backtestable (`mpedb trigger backtest __drv_<name>_i`) like anything
//! hand-written; being generated only means it is REGENERABLE.

use mpedb_types::model::{DerivedKind, DerivedModel, WorkloadModel};
use mpedb_types::ColumnType;

use crate::{Database, Error, Result};

/// Sys-record namespace marking a table as engine-owned derived state:
/// `drvtab/<table-name>` → the declaration fingerprint. The marker is what
/// makes `sync` safe: it only ever drops tables IT installed.
pub const NS_DRVTAB: &str = "drvtab";

/// What one [`Database::sync_model_derived`] pass did.
#[derive(Debug, Clone, Default)]
pub struct DerivedSync {
    /// Installed this pass (missing, or re-installed after a changed
    /// declaration).
    pub installed: Vec<String>,
    /// Already present with an unchanged declaration — untouched.
    pub kept: Vec<String>,
    /// No longer claimed by the model — triggers and table dropped.
    pub dropped: Vec<String>,
}

/// Declaration fingerprint: changes exactly when the maintenance would have
/// to be regenerated.
fn fingerprint(d: &DerivedModel) -> [u8; 32] {
    let mut b = Vec::new();
    b.extend_from_slice(
        match d.kind {
            DerivedKind::ReverseEdge => "reverse-edge",
            DerivedKind::Counter => "counter",
        }
        .as_bytes(),
    );
    b.push(0);
    b.extend_from_slice(d.source.as_bytes());
    b.push(0);
    for c in &d.group_by {
        b.extend_from_slice(c.as_bytes());
        b.push(1);
    }
    *blake3::hash(&b).as_bytes()
}

/// The SQL type name for a derived column, in `ColumnType::parse` vocabulary.
fn decl_for(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Int64 => "BIGINT",
        ColumnType::Float64 => "DOUBLE",
        ColumnType::Text => "TEXT",
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Blob => "BLOB",
        ColumnType::Any => "",
    }
}

/// The four possible generated trigger names for a derived structure. `_un`
/// exists only for kinds with an UPDATE direction that must handle
/// became-NULL keys (see the module doc).
fn trigger_names(name: &str) -> [String; 4] {
    [
        format!("__drv_{name}_i"),
        format!("__drv_{name}_d"),
        format!("__drv_{name}_u"),
        format!("__drv_{name}_un"),
    ]
}

impl Database {
    /// Make the database's derived structures match the stored model's
    /// `[[model.derived]]` declarations. Idempotent; see the module doc.
    pub fn sync_model_derived(&self) -> Result<DerivedSync> {
        let model = self.model()?.ok_or_else(|| {
            Error::Unsupported(
                "no model stored — `mpedb model set` first; the model's \
                 [[model.derived]] blocks are what declare derived structures"
                    .into(),
            )
        })?;
        let mut out = DerivedSync::default();

        // Drop first: an unclaimed structure's triggers stop firing before
        // any (re)install below adds write traffic of its own.
        let installed_names: Vec<String> = self
            .sys_record_scan(NS_DRVTAB)?
            .into_iter()
            .map(|(k, _)| String::from_utf8_lossy(&k).into_owned())
            .collect();
        for name in &installed_names {
            let claimed = model.derived.iter().any(|d| d.name == *name);
            let changed = model
                .derived
                .iter()
                .find(|d| d.name == *name)
                .map(|d| {
                    self.sys_record_get(NS_DRVTAB, name.as_bytes())
                        .ok()
                        .flatten()
                        .as_deref()
                        != Some(&fingerprint(d)[..])
                })
                .unwrap_or(false);
            if !claimed || changed {
                self.drop_derived(name)?;
                if !claimed {
                    out.dropped.push(name.clone());
                }
                // changed: re-installed below, reported as installed.
            }
        }

        for d in &model.derived {
            let current = self.sys_record_get(NS_DRVTAB, d.name.as_bytes())?;
            if current.as_deref() == Some(&fingerprint(d)[..]) {
                out.kept.push(d.name.clone());
                continue;
            }
            self.install_derived(&model, d)?;
            out.installed.push(d.name.clone());
        }
        Ok(out)
    }

    /// Create table + triggers + backfill + marker, in ONE write transaction.
    fn install_derived(&self, model: &WorkloadModel, d: &DerivedModel) -> Result<()> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let schema = &bundle.schema;
        let src_t = schema
            .tables
            .iter()
            .find(|t| !t.dead && mpedb_types::ident_eq(&t.name, &d.source))
            .ok_or_else(|| {
                Error::Bind(format!(
                    "derived `{}`: source table `{}` does not exist",
                    d.name, d.source
                ))
            })?;
        if schema.table_id(&d.name).is_some() {
            return Err(Error::Bind(format!(
                "derived `{}`: a table with that name already exists and is not \
                 marked as engine-derived — refusing to touch it",
                d.name
            )));
        }
        let col_ty = |cname: &str| -> Result<ColumnType> {
            src_t
                .columns
                .iter()
                .find(|c| mpedb_types::ident_eq(&c.name, cname))
                .map(|c| c.ty)
                .ok_or_else(|| {
                    Error::Bind(format!(
                        "derived `{}`: source `{}` has no column `{cname}`",
                        d.name, d.source
                    ))
                })
        };

        // Generate (create-table, triggers, backfill) per kind.
        let (create, triggers, backfill_sql) = match d.kind {
            DerivedKind::ReverseEdge => {
                // Endpoints from the source's traverse declaration — the same
                // convention `:->:` installs from (DESIGN-MODEL-LANG §2).
                let tm = model
                    .tables
                    .iter()
                    .find(|t| mpedb_types::ident_eq(&t.name, &d.source));
                let tr = tm
                    .and_then(|t| {
                        t.access
                            .iter()
                            .find(|a| a.kind == mpedb_types::model::AccessKind::Traverse)
                    })
                    .filter(|a| a.columns.len() == 2)
                    .ok_or_else(|| {
                        Error::Unsupported(format!(
                            "derived reverse-edge `{}`: source `{}` needs a traverse \
                             access declaring [source, destination] columns",
                            d.name, d.source
                        ))
                    })?;
                let (s, dst) = (&tr.columns[0], &tr.columns[1]);
                let (sty, dty) = (col_ty(s)?, col_ty(dst)?);
                // The mirror is PK-keyed on (dst, src): the pair must be
                // unique on the source or the mirror would need multiset
                // semantics. PK or a UNIQUE index over exactly the pair.
                let si = src_t.column_index(s).expect("checked");
                let di = src_t.column_index(dst).expect("checked");
                let mut pair = [si, di];
                pair.sort_unstable();
                let mut pk = src_t.primary_key.clone();
                pk.sort_unstable();
                let unique_pair = pk == pair
                    || src_t.indexes.iter().any(|ix| {
                        let mut c = ix.columns.clone();
                        c.sort_unstable();
                        ix.unique && ix.predicate.is_none() && c == pair
                    });
                if !unique_pair {
                    return Err(Error::Unsupported(format!(
                        "derived reverse-edge `{}`: ({s}, {dst}) must be UNIQUE on \
                         `{}` (its PK, or a unique index over exactly the pair) — \
                         the mirror is keyed on it",
                        d.name, d.source
                    )));
                }
                let n = &d.name;
                let t = &d.source;
                let create = format!(
                    "CREATE TABLE \"{n}\" (\"{dst}\" {} NOT NULL, \"{s}\" {} NOT NULL, \
                     PRIMARY KEY (\"{dst}\", \"{s}\"))",
                    decl_for(dty),
                    decl_for(sty)
                );
                let [ti, td, tu, tun] = trigger_names(n);
                let triggers = vec![
                    format!(
                        "CREATE TRIGGER \"{ti}\" AFTER INSERT ON \"{t}\" FOR EACH ROW \
                         WHEN (NEW.\"{s}\" IS NOT NULL AND NEW.\"{dst}\" IS NOT NULL) \
                         BEGIN INSERT INTO \"{n}\" (\"{dst}\", \"{s}\") \
                         VALUES (NEW.\"{dst}\", NEW.\"{s}\"); END"
                    ),
                    format!(
                        "CREATE TRIGGER \"{td}\" AFTER DELETE ON \"{t}\" FOR EACH ROW \
                         WHEN (OLD.\"{s}\" IS NOT NULL AND OLD.\"{dst}\" IS NOT NULL) \
                         BEGIN DELETE FROM \"{n}\" WHERE \"{dst}\" = OLD.\"{dst}\" \
                         AND \"{s}\" = OLD.\"{s}\"; END"
                    ),
                    // UPDATE splits on whether the NEW pair is present: a
                    // became-NULL key must still remove the OLD mirror row
                    // (the DELETE's OLD-side predicate matches nothing when
                    // OLD had a NULL — 3VL does the gating there).
                    format!(
                        "CREATE TRIGGER \"{tu}\" AFTER UPDATE OF \"{s}\", \"{dst}\" \
                         ON \"{t}\" FOR EACH ROW \
                         WHEN (NEW.\"{s}\" IS NOT NULL AND NEW.\"{dst}\" IS NOT NULL) \
                         BEGIN DELETE FROM \"{n}\" WHERE \"{dst}\" = OLD.\"{dst}\" \
                         AND \"{s}\" = OLD.\"{s}\"; \
                         INSERT INTO \"{n}\" (\"{dst}\", \"{s}\") \
                         VALUES (NEW.\"{dst}\", NEW.\"{s}\"); END"
                    ),
                    format!(
                        "CREATE TRIGGER \"{tun}\" AFTER UPDATE OF \"{s}\", \"{dst}\" \
                         ON \"{t}\" FOR EACH ROW \
                         WHEN (NEW.\"{s}\" IS NULL OR NEW.\"{dst}\" IS NULL) \
                         BEGIN DELETE FROM \"{n}\" WHERE \"{dst}\" = OLD.\"{dst}\" \
                         AND \"{s}\" = OLD.\"{s}\"; END"
                    ),
                ];
                let backfill = format!(
                    "INSERT INTO \"{n}\" (\"{dst}\", \"{s}\") \
                     SELECT \"{dst}\", \"{s}\" FROM \"{t}\" \
                     WHERE \"{dst}\" IS NOT NULL AND \"{s}\" IS NOT NULL"
                );
                (create, triggers, Some(backfill))
            }
            DerivedKind::Counter => {
                let k = &d.group_by[0];
                let kty = col_ty(k)?;
                if mpedb_types::ident_eq(k, "n") {
                    return Err(Error::Unsupported(format!(
                        "derived counter `{}`: the key column cannot be named `n` \
                         (the count column's name)",
                        d.name
                    )));
                }
                let n = &d.name;
                let t = &d.source;
                let create = format!(
                    "CREATE TABLE \"{n}\" (\"{k}\" {} NOT NULL, n BIGINT NOT NULL, \
                     PRIMARY KEY (\"{k}\"))",
                    decl_for(kty)
                );
                let [ti, td, tu, tun] = trigger_names(n);
                let upsert = format!(
                    "INSERT INTO \"{n}\" (\"{k}\", n) VALUES (NEW.\"{k}\", 1) \
                     ON CONFLICT (\"{k}\") DO UPDATE SET n = n + 1"
                );
                let dec = format!(
                    "UPDATE \"{n}\" SET n = n - 1 WHERE \"{k}\" = OLD.\"{k}\""
                );
                let triggers = vec![
                    format!(
                        "CREATE TRIGGER \"{ti}\" AFTER INSERT ON \"{t}\" FOR EACH ROW \
                         WHEN (NEW.\"{k}\" IS NOT NULL) BEGIN {upsert}; END"
                    ),
                    format!(
                        "CREATE TRIGGER \"{td}\" AFTER DELETE ON \"{t}\" FOR EACH ROW \
                         WHEN (OLD.\"{k}\" IS NOT NULL) BEGIN {dec}; END"
                    ),
                    // Key moved (possibly from NULL): decrement the old count
                    // (matches nothing when OLD was NULL), count the new. An
                    // update that keeps the key nets to zero through the pair.
                    format!(
                        "CREATE TRIGGER \"{tu}\" AFTER UPDATE OF \"{k}\" ON \"{t}\" \
                         FOR EACH ROW WHEN (NEW.\"{k}\" IS NOT NULL) \
                         BEGIN {dec}; {upsert}; END"
                    ),
                    format!(
                        "CREATE TRIGGER \"{tun}\" AFTER UPDATE OF \"{k}\" ON \"{t}\" \
                         FOR EACH ROW WHEN (NEW.\"{k}\" IS NULL) BEGIN {dec}; END"
                    ),
                ];
                // Counter backfill runs host-side below (aggregate INSERT …
                // SELECT is not a supported source shape).
                (create, triggers, None)
            }
        };

        // One transaction: table, triggers, backfill, marker. The writer lock
        // makes the create→backfill window race-free; in-txn triggers start
        // firing after commit, which is exactly when writers can run again.
        let mut s = self.begin()?;
        s.query(&create, &[])?;
        for t in &triggers {
            s.query(t, &[])?;
        }
        match (&d.kind, backfill_sql) {
            (_, Some(sql)) => {
                s.query(&sql, &[])?;
            }
            (DerivedKind::Counter, None) => {
                let k = &d.group_by[0];
                let groups = match s.query(
                    &format!(
                        "SELECT \"{k}\", count(*) FROM \"{}\" \
                         WHERE \"{k}\" IS NOT NULL GROUP BY \"{k}\"",
                        d.source
                    ),
                    &[],
                )? {
                    crate::ExecResult::Rows { rows, .. } => rows,
                    other => {
                        return Err(Error::Internal(format!(
                            "counter backfill returned {other:?}"
                        )))
                    }
                };
                for g in groups {
                    s.query(
                        &format!("INSERT INTO \"{}\" (\"{k}\", n) VALUES ($1, $2)", d.name),
                        &g,
                    )?;
                }
            }
            _ => unreachable!("reverse-edge always has backfill SQL"),
        }
        s.sys_record_put(NS_DRVTAB, d.name.as_bytes(), &fingerprint(d))?;
        s.commit()?;
        Ok(())
    }

    /// Drop one engine-derived structure: its four (at most) maintenance
    /// triggers, the table, and the marker — atomically. Refuses nothing:
    /// `IF EXISTS` throughout, so a half-installed remnant heals.
    fn drop_derived(&self, name: &str) -> Result<()> {
        let mut s = self.begin()?;
        for t in trigger_names(name) {
            s.query(&format!("DROP TRIGGER IF EXISTS \"{t}\""), &[])?;
        }
        s.query(&format!("DROP TABLE IF EXISTS \"{name}\""), &[])?;
        s.sys_record_delete(NS_DRVTAB, name.as_bytes())?;
        s.commit()?;
        Ok(())
    }
}
