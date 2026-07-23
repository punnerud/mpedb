//! The stored workload model — design/DESIGN-MODEL-LANG.md, stage M1.
//!
//! The language lives in [`mpedb_types::model`]; this module is what a
//! DATABASE does with it: validate a model against the live schema (a model
//! that names tables that do not exist describes some other application),
//! store the source in the shared sys-keyspace so every attached process
//! reads the same declaration, and SYNTHESIZE a statement workload from the
//! shape level — the reverse of the advisor's extraction, emitting exactly
//! the statement shapes whose candidates the advisor knows how to derive.
//!
//! The model is advisory metadata: it never enters plan bytes, plan hashes,
//! or the schema hash. Storing one cannot change any query's meaning — only
//! what the advisor/analyze layer recommends. That is why `set_model` does
//! not bump `schema_gen`: there is nothing cached to invalidate, and a second
//! process sees the record on its next read snapshot like any other row.

use mpedb_types::model::{AccessKind, StatementModel, WorkloadModel};
use mpedb_types::{Error, Result};

/// Sys-keyspace namespace; the single current model lives at key `current`.
pub const NS: &str = "model";
const KEY: &[u8] = b"current";

impl crate::Database {
    /// Validate `toml` against the live schema and store it as THE model.
    /// Refusals name what is wrong (unknown table, unknown column) — a typo
    /// must not survive as a description of a different application.
    pub fn set_model(&self, toml: &str) -> Result<()> {
        let model = WorkloadModel::from_toml_str(toml)?;
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        validate_against_schema(&model, &bundle.schema)?;
        self.sys_record_put(NS, KEY, toml.as_bytes())
    }

    /// The stored model's source, verbatim, if one was set.
    pub fn model_source(&self) -> Result<Option<String>> {
        Ok(self
            .sys_record_get(NS, KEY)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// The stored model, parsed. A stored source that no longer parses is a
    /// corrupt record, reported as such rather than silently ignored.
    pub fn model(&self) -> Result<Option<WorkloadModel>> {
        match self.model_source()? {
            None => Ok(None),
            Some(src) => WorkloadModel::from_toml_str(&src)
                .map(Some)
                .map_err(|e| Error::Corrupt(format!("stored model no longer parses: {e}"))),
        }
    }
}

/// Every named table must exist (live, standard) and every named column must
/// exist in its table. The model may cover a SUBSET of the schema — declaring
/// nothing about a table is fine; declaring a wrong name is not.
fn validate_against_schema(model: &WorkloadModel, schema: &mpedb_types::Schema) -> Result<()> {
    for t in &model.tables {
        let Some(td) = schema.tables.iter().find(|td| !td.dead && td.name == t.name) else {
            return Err(Error::Unsupported(format!(
                "model names table `{}`, which does not exist in the schema",
                t.name
            )));
        };
        for a in &t.access {
            for c in &a.columns {
                if !td.columns.iter().any(|col| &col.name == c) {
                    return Err(Error::Unsupported(format!(
                        "model access on `{}` names column `{c}`, which does not exist",
                        t.name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Level-1 shapes → a weighted statement list, in the exact shapes the
/// advisor's extraction understands (fold_shape's grammar, run backwards).
/// Level-2 statements pass through with their weights. Access weights are
/// relative floats; they scale to counts by ×10 (so 0.4 → 4) with a floor of
/// 1 — the advisor compares counts, and only their ratios mean anything.
pub fn synthesize_statements(
    model: &WorkloadModel,
    schema: &mpedb_types::Schema,
) -> Vec<StatementModel> {
    let mut out: Vec<StatementModel> = Vec::new();
    for t in &model.tables {
        let td = schema.tables.iter().find(|td| !td.dead && td.name == t.name);
        for a in &t.access {
            let weight = ((a.weight * 10.0).round() as u64).max(1);
            let sql = match a.kind {
                AccessKind::FilterEq | AccessKind::JoinKey => {
                    // A join key IS probed by equality — same candidate.
                    let preds = a
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| format!("{c} = ${}", i + 1))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!("SELECT * FROM {} WHERE {preds}", t.name)
                }
                AccessKind::FilterRange => {
                    format!("SELECT * FROM {} WHERE {} > $1", t.name, a.columns[0])
                }
                AccessKind::OrderBy => {
                    format!("SELECT * FROM {} ORDER BY {}", t.name, a.columns.join(", "))
                }
                AccessKind::Point => {
                    // The PK probe — synthesized so the advisor's SERVED
                    // count reflects the declared point traffic.
                    let Some(td) = td else { continue };
                    let preds = td
                        .primary_key
                        .iter()
                        .enumerate()
                        .map(|(i, &c)| {
                            format!("{} = ${}", td.columns[c as usize].name, i + 1)
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!("SELECT * FROM {} WHERE {preds}", t.name)
                }
                AccessKind::Traverse => {
                    // Each fixpoint level probes the edge source by equality;
                    // that probe is the traversal's index shape.
                    format!("SELECT * FROM {} WHERE {} = $1", t.name, a.columns[0])
                }
                // kNN has no B-tree candidate to derive (a vector index is a
                // different structure); the declaration still matters to
                // analyze()/cost presets, just not to index advice.
                AccessKind::Knn => continue,
            };
            out.push(StatementModel { sql, weight });
        }
    }
    out.extend(model.statements.iter().cloned());
    out
}
