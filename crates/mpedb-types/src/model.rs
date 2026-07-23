//! The workload model — design/DESIGN-MODEL-LANG.md.
//!
//! A TOML document describing how an application USES its data, at whichever
//! resolution the author has: an **archetype** ("this is plain sqlite3-style
//! OLTP, not a graph database" — nothing more), **shapes** (per-table roles
//! and access declarations, the altitude of a Django model), or **statements**
//! (the exact SQL, what the #118 advisor already ingests). One model, three
//! resolutions of the same thing; refining never changes what a consumer
//! means, only how sharply it can act. Most people switch databases when what
//! they actually have is a model switch — this document is where that model
//! lives instead.
//!
//! Parsing is strict the way [`crate::config`] is strict: unknown fields are
//! errors (a typo must not silently describe a different workload), and every
//! enum-like string names its valid values in the refusal.

use serde::Deserialize;

use crate::{Error, Result};

/// The whole model document.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadModel {
    pub name: Option<String>,
    pub archetype: Option<Archetype>,
    pub description: Option<String>,
    pub tables: Vec<TableModel>,
    pub derived: Vec<DerivedModel>,
    pub statements: Vec<StatementModel>,
}

/// Level 0: the coarsest useful statement about a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archetype {
    /// Point reads/writes, short transactions — classic embedded sqlite3 use.
    Oltp,
    /// Fact + dimensions, scan-and-aggregate reads.
    StarOlap,
    /// Edge tables walked by joins / bounded recursion.
    GraphTraversal,
    /// Embedding blobs under kNN, usually filtered.
    VectorRag,
    /// Stops/vehicles/matrix feeding a sequencing solver.
    Routing,
    /// "Ordinary sqlite3 usage" asserted and nothing else — deliberately the
    /// vaguest level: it still tells the engine what NOT to prepare for.
    Sqlite3General,
}

impl Archetype {
    pub const ALL: [(&'static str, Archetype); 6] = [
        ("oltp", Archetype::Oltp),
        ("star-olap", Archetype::StarOlap),
        ("graph-traversal", Archetype::GraphTraversal),
        ("vector-rag", Archetype::VectorRag),
        ("routing", Archetype::Routing),
        ("sqlite3-general", Archetype::Sqlite3General),
    ];

    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, a)| *a == self)
            .map(|(n, _)| *n)
            .unwrap_or("?")
    }

    fn parse(s: &str) -> Result<Archetype> {
        Self::ALL
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, a)| *a)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "unknown model archetype `{s}` — one of: {}",
                    Self::ALL.map(|(n, _)| n).join(", ")
                ))
            })
    }
}

/// Level 1: one table's declared role and traffic.
#[derive(Debug, Clone, PartialEq)]
pub struct TableModel {
    pub name: String,
    pub role: Option<TableRole>,
    pub read_write: Option<ReadWrite>,
    pub access: Vec<AccessModel>,
}

/// What a table IS in the workload — the declaration that later lets an
/// operator like `a :->: b` know which table joins (edge) or which column
/// carries embeddings, without being told at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableRole {
    Fact,
    Dimension,
    Edge,
    Embedding,
    Document,
    Log,
    Queue,
    Generic,
}

const ROLES: [(&str, TableRole); 8] = [
    ("fact", TableRole::Fact),
    ("dimension", TableRole::Dimension),
    ("edge", TableRole::Edge),
    ("embedding", TableRole::Embedding),
    ("document", TableRole::Document),
    ("log", TableRole::Log),
    ("queue", TableRole::Queue),
    ("generic", TableRole::Generic),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadWrite {
    ReadHeavy,
    WriteHeavy,
    Balanced,
}

/// One declared access pattern over a table.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessModel {
    pub kind: AccessKind,
    pub columns: Vec<String>,
    /// Relative weight of this access within the workload. Dimensionless;
    /// consumers only ever compare weights to each other.
    pub weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    /// Equality filter / probe on the named columns.
    FilterEq,
    /// Range predicate on the (single) named column.
    FilterRange,
    /// This table is entered by equality on these columns from a join.
    JoinKey,
    /// `ORDER BY` / grouping on the named columns.
    OrderBy,
    /// Point lookup by primary key.
    Point,
    /// Edge expansion (src → dst walks).
    Traverse,
    /// k-nearest-neighbour over an embedding column.
    Knn,
}

const KINDS: [(&str, AccessKind); 7] = [
    ("filter-eq", AccessKind::FilterEq),
    ("filter-range", AccessKind::FilterRange),
    ("join-key", AccessKind::JoinKey),
    ("order-by", AccessKind::OrderBy),
    ("point", AccessKind::Point),
    ("traverse", AccessKind::Traverse),
    ("knn", AccessKind::Knn),
];

/// A model-declared DERIVED structure (DESIGN-MODEL-LANG §3, the maintenance
/// rung): a table the ENGINE builds and keeps current via generated triggers
/// on the source table's writes. Declared, not hand-wired: the engine can
/// regenerate the maintenance when the declaration changes and drop it when
/// the model stops claiming it (`Database::sync_model_derived`).
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedModel {
    /// The derived TABLE's name (created and owned by the engine).
    pub name: String,
    pub kind: DerivedKind,
    /// The source table whose writes maintain this structure.
    pub source: String,
    /// `counter` only: the single grouping column (v1).
    pub group_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedKind {
    /// `(dst, src)`-keyed mirror of an edge table — backward traversal
    /// ("who points at X") becomes a primary-key range scan. Endpoints come
    /// from the source's `traverse` access declaration, the same convention
    /// `:->:` installs from.
    ReverseEdge,
    /// Row counts per `group_by` key, upsert-maintained (`n` may rest at 0
    /// after deletes — a count, not a membership set).
    Counter,
}

const DERIVED_KINDS: [(&str, DerivedKind); 2] = [
    ("reverse-edge", DerivedKind::ReverseEdge),
    ("counter", DerivedKind::Counter),
];

/// Level 2: an exact statement with an observed/estimated frequency.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementModel {
    pub sql: String,
    /// Executions per unit of workload; 1 when unknown. Integer, because the
    /// advisor counts statements and a weight IS a count.
    pub weight: u64,
}

// ---------------------------------------------------------------------------
// TOML wire form (strict, like config.rs)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDoc {
    model: RawModel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: Option<String>,
    archetype: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "table")]
    tables: Vec<RawTable>,
    #[serde(default, rename = "derived")]
    derived: Vec<RawDerived>,
    #[serde(default, rename = "statement")]
    statements: Vec<RawStatement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    name: String,
    role: Option<String>,
    read_write: Option<String>,
    #[serde(default, rename = "access")]
    access: Vec<RawAccess>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccess {
    kind: String,
    #[serde(default)]
    columns: Vec<String>,
    weight: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDerived {
    name: String,
    kind: String,
    source: String,
    #[serde(default)]
    group_by: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStatement {
    sql: String,
    weight: Option<u64>,
}

/// Look a name up in a `(name, value)` table, refusing with the full list.
fn lookup<T: Copy>(what: &str, table: &[(&'static str, T)], s: &str) -> Result<T> {
    table
        .iter()
        .find(|(n, _)| *n == s)
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "unknown {what} `{s}` — one of: {}",
                table.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            ))
        })
}

impl WorkloadModel {
    /// Parse and structurally validate. Schema-level validation (do the named
    /// tables/columns exist?) belongs to the database that stores the model —
    /// this layer owns only the language.
    pub fn from_toml_str(text: &str) -> Result<WorkloadModel> {
        let raw: RawDoc = toml::from_str(text)
            .map_err(|e| Error::Unsupported(format!("model parse error: {e}")))?;
        let m = raw.model;

        let archetype = m.archetype.as_deref().map(Archetype::parse).transpose()?;

        let mut tables = Vec::with_capacity(m.tables.len());
        for t in m.tables {
            let role = t.role.as_deref().map(|s| lookup("table role", &ROLES, s)).transpose()?;
            let read_write = t
                .read_write
                .as_deref()
                .map(|s| match s {
                    "read-heavy" => Ok(ReadWrite::ReadHeavy),
                    "write-heavy" => Ok(ReadWrite::WriteHeavy),
                    "balanced" => Ok(ReadWrite::Balanced),
                    other => Err(Error::Unsupported(format!(
                        "unknown read_write `{other}` — one of: read-heavy, write-heavy, balanced"
                    ))),
                })
                .transpose()?;
            let mut access = Vec::with_capacity(t.access.len());
            for a in t.access {
                let kind = lookup("access kind", &KINDS, &a.kind)?;
                let needs_cols = !matches!(kind, AccessKind::Point);
                if needs_cols && a.columns.is_empty() {
                    return Err(Error::Unsupported(format!(
                        "access `{}` on table `{}` names no columns",
                        a.kind, t.name
                    )));
                }
                if kind == AccessKind::FilterRange && a.columns.len() != 1 {
                    return Err(Error::Unsupported(format!(
                        "filter-range on `{}` must name exactly one column",
                        t.name
                    )));
                }
                let weight = a.weight.unwrap_or(1.0);
                if !(weight.is_finite() && weight > 0.0) {
                    return Err(Error::Unsupported(format!(
                        "access weight on `{}` must be a positive finite number",
                        t.name
                    )));
                }
                access.push(AccessModel { kind, columns: a.columns, weight });
            }
            tables.push(TableModel { name: t.name, role, read_write, access });
        }

        let mut derived = Vec::with_capacity(m.derived.len());
        for d in m.derived {
            let kind = lookup("derived kind", &DERIVED_KINDS, &d.kind)?;
            if d.name.eq_ignore_ascii_case(&d.source) {
                return Err(Error::Unsupported(format!(
                    "derived `{}` cannot derive from itself",
                    d.name
                )));
            }
            match kind {
                DerivedKind::Counter if d.group_by.len() != 1 => {
                    return Err(Error::Unsupported(format!(
                        "derived counter `{}` needs exactly one group_by column (v1)",
                        d.name
                    )));
                }
                DerivedKind::ReverseEdge if !d.group_by.is_empty() => {
                    return Err(Error::Unsupported(format!(
                        "derived reverse-edge `{}` takes no group_by — its columns \
                         come from the source's traverse declaration",
                        d.name
                    )));
                }
                _ => {}
            }
            derived.push(DerivedModel {
                name: d.name,
                kind,
                source: d.source,
                group_by: d.group_by,
            });
        }

        let statements = m
            .statements
            .into_iter()
            .map(|s| StatementModel { sql: s.sql, weight: s.weight.unwrap_or(1).max(1) })
            .collect();

        if archetype.is_none() && tables.is_empty() {
            // A model with no archetype and no tables says nothing; statements
            // alone are legal (level 2 only) — but a fully empty document is a
            // mistake, not a model.
            let model = WorkloadModel {
                name: m.name,
                archetype,
                description: m.description,
                tables,
                derived,
                statements,
            };
            if model.statements.is_empty() && model.derived.is_empty() {
                return Err(Error::Unsupported(
                    "empty model: declare an archetype, tables, or statements".into(),
                ));
            }
            return Ok(model);
        }

        Ok(WorkloadModel {
            name: m.name,
            archetype,
            description: m.description,
            tables,
            derived,
            statements,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_resolutions_parse() {
        // Level 0 only — the user's "this is plain sqlite3" example.
        let m = WorkloadModel::from_toml_str(
            "[model]\narchetype = \"sqlite3-general\"\ndescription = \"ordinary embedded use\"",
        )
        .unwrap();
        assert_eq!(m.archetype, Some(Archetype::Sqlite3General));
        assert!(m.tables.is_empty());

        // Level 1 shapes.
        let m = WorkloadModel::from_toml_str(
            r#"
[model]
archetype = "star-olap"

[[model.table]]
name = "fact"
role = "fact"
read_write = "read-heavy"

  [[model.table.access]]
  kind = "filter-eq"
  columns = ["product_id"]
  weight = 0.4

  [[model.table.access]]
  kind = "filter-range"
  columns = ["amount"]
"#,
        )
        .unwrap();
        assert_eq!(m.tables.len(), 1);
        assert_eq!(m.tables[0].access[0].kind, AccessKind::FilterEq);
        assert_eq!(m.tables[0].access[1].weight, 1.0);

        // Level 2 statements only.
        let m = WorkloadModel::from_toml_str(
            "[[model.statement]]\nsql = \"SELECT 1\"\nweight = 7\n[model]\n",
        )
        .unwrap();
        assert_eq!(m.statements[0].weight, 7);
    }

    #[test]
    fn refusals_name_the_valid_values() {
        let e = WorkloadModel::from_toml_str("[model]\narchetype = \"webscale\"").unwrap_err();
        assert!(e.to_string().contains("star-olap"), "{e}");
        let e = WorkloadModel::from_toml_str(
            "[model]\n[[model.table]]\nname = \"t\"\nrole = \"blob\"",
        )
        .unwrap_err();
        assert!(e.to_string().contains("edge"), "{e}");
        // Unknown fields are typos, not extensions.
        let e = WorkloadModel::from_toml_str("[model]\narchtype = \"oltp\"").unwrap_err();
        assert!(e.to_string().contains("parse error"), "{e}");
        // Empty says nothing.
        assert!(WorkloadModel::from_toml_str("[model]\n").is_err());
    }
}
