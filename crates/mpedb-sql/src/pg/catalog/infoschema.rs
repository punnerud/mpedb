//! `information_schema` — the SQL standard's catalog, over the same schema.
//!
//! These are the relations Django's introspection and SQLAlchemy's reflection
//! reach for when they are not using `pg_catalog` directly. They report the
//! same facts as `pgcat`, under the standard's names and vocabulary
//! (`bigint` rather than `int8`, `YES`/`NO` rather than booleans) — which is
//! why the type spelling goes through one shared function
//! ([`super::pgcat::sql_type_name`]) rather than a second table that could
//! disagree with the first.
//!
//! The catalog name is `mpedb` throughout, matching `pg_database.datname`. A
//! client that qualifies a name three parts deep has to get the same answer
//! from both.

use super::pgcat::{sql_type_name, OWNER_NAME};
use super::{int, live_tables, text};
use mpedb_types::schema::Schema;
use mpedb_types::{ColumnType as C, Value};

/// The catalog name every three-part name resolves under.
const CATALOG: &str = "mpedb";
const SCHEMA_NAME: &str = "public";

/// SQL's `YES`/`NO`, which the standard uses where PostgreSQL's own catalog
/// uses a boolean. Getting this wrong reads as "nullable" in one direction and
/// crashes a client's `== 'YES'` comparison in the other.
fn yes_no(b: bool) -> Value {
    text(if b { "YES" } else { "NO" })
}

pub(crate) const SCHEMATA_COLUMNS: &[(&str, C)] = &[
    ("catalog_name", C::Text),
    ("schema_name", C::Text),
    ("schema_owner", C::Text),
    ("default_character_set_catalog", C::Text),
    ("default_character_set_schema", C::Text),
    ("default_character_set_name", C::Text),
    ("sql_path", C::Text),
];

pub(crate) fn schemata(_: &Schema) -> Vec<Vec<Value>> {
    ["public", "pg_catalog", "information_schema"]
        .into_iter()
        .map(|n| {
            vec![
                text(CATALOG),
                text(n),
                text(OWNER_NAME),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

pub(crate) const TABLES_COLUMNS: &[(&str, C)] = &[
    ("table_catalog", C::Text),
    ("table_schema", C::Text),
    ("table_name", C::Text),
    ("table_type", C::Text),
    ("self_referencing_column_name", C::Text),
    ("reference_generation", C::Text),
    ("user_defined_type_catalog", C::Text),
    ("user_defined_type_schema", C::Text),
    ("user_defined_type_name", C::Text),
    ("is_insertable_into", C::Text),
    ("is_typed", C::Text),
    ("commit_action", C::Text),
];

pub(crate) fn tables(schema: &Schema) -> Vec<Vec<Value>> {
    live_tables(schema)
        .map(|t| {
            vec![
                text(CATALOG),
                text(SCHEMA_NAME),
                text(&t.name),
                text("BASE TABLE"),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                yes_no(true),
                yes_no(false),
                Value::Null,
            ]
        })
        .collect()
}

pub(crate) const COLUMNS_COLUMNS: &[(&str, C)] = &[
    ("table_catalog", C::Text),
    ("table_schema", C::Text),
    ("table_name", C::Text),
    ("column_name", C::Text),
    ("ordinal_position", C::Int64),
    ("column_default", C::Text),
    ("is_nullable", C::Text),
    ("data_type", C::Text),
    ("character_maximum_length", C::Int64),
    ("character_octet_length", C::Int64),
    ("numeric_precision", C::Int64),
    ("numeric_precision_radix", C::Int64),
    ("numeric_scale", C::Int64),
    ("datetime_precision", C::Int64),
    ("collation_name", C::Text),
    ("udt_catalog", C::Text),
    ("udt_schema", C::Text),
    ("udt_name", C::Text),
    ("is_identity", C::Text),
    ("identity_generation", C::Text),
    ("is_generated", C::Text),
    ("generation_expression", C::Text),
    ("is_updatable", C::Text),
];

pub(crate) fn columns(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (i, c) in t.columns.iter().enumerate() {
            let (prec, radix, scale) = match c.ty {
                C::Int64 => (Some(64), Some(2), Some(0)),
                C::Float64 => (Some(53), Some(2), None),
                _ => (None, None, None),
            };
            out.push(vec![
                text(CATALOG),
                text(SCHEMA_NAME),
                text(&t.name),
                text(&c.name),
                int(i as i64 + 1),
                match &c.default_text {
                    Some(d) => text(d),
                    None => Value::Null,
                },
                yes_no(c.nullable),
                text(sql_type_name(c.ty)),
                // mpedb's Text has no declared length — it is not varchar(n) —
                // so the standard's answer is NULL, meaning "no limit". A
                // number here would promise an enforcement that does not exist.
                Value::Null,
                Value::Null,
                prec.map(Value::Int).unwrap_or(Value::Null),
                radix.map(Value::Int).unwrap_or(Value::Null),
                scale.map(Value::Int).unwrap_or(Value::Null),
                match c.ty {
                    C::Timestamp => Value::Int(6),
                    _ => Value::Null,
                },
                match c.ty {
                    C::Text => text("C"),
                    _ => Value::Null,
                },
                text(CATALOG),
                text("pg_catalog"),
                text(udt_name(c.ty)),
                yes_no(false),
                Value::Null,
                if c.generated.is_some() {
                    text("ALWAYS")
                } else {
                    text("NEVER")
                },
                Value::Null,
                yes_no(true),
            ]);
        }
    }
    out
}

/// `udt_name` is PostgreSQL's INTERNAL type name (`int8`), not the standard's
/// (`bigint`) — the one place in this relation where the two vocabularies meet,
/// and a common source of confusion when reflection code reads the wrong one.
fn udt_name(ty: C) -> &'static str {
    super::column_type_name(ty)
}

pub(crate) const TABLE_CONSTRAINTS_COLUMNS: &[(&str, C)] = &[
    ("constraint_catalog", C::Text),
    ("constraint_schema", C::Text),
    ("constraint_name", C::Text),
    ("table_catalog", C::Text),
    ("table_schema", C::Text),
    ("table_name", C::Text),
    ("constraint_type", C::Text),
    ("is_deferrable", C::Text),
    ("initially_deferred", C::Text),
    ("enforced", C::Text),
];

pub(crate) fn table_constraints(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut push = |name: String, table: &str, kind: &str, deferred: bool| {
        out.push(vec![
            text(CATALOG),
            text(SCHEMA_NAME),
            text(name),
            text(CATALOG),
            text(SCHEMA_NAME),
            text(table),
            text(kind),
            yes_no(deferred),
            yes_no(deferred),
            yes_no(true),
        ]);
    };
    for t in live_tables(schema) {
        if !t.primary_key.is_empty() {
            push(format!("{}_pkey", t.name), &t.name, "PRIMARY KEY", false);
        }
        for (i, idx) in t.indexes.iter().enumerate() {
            if idx.unique {
                push(
                    idx.name
                        .clone()
                        .unwrap_or_else(|| format!("{}_{}_key", t.name, i + 1)),
                    &t.name,
                    "UNIQUE",
                    false,
                );
            }
        }
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            push(
                fk.name
                    .clone()
                    .unwrap_or_else(|| format!("{}_fk_{i}", t.name)),
                &t.name,
                "FOREIGN KEY",
                fk.deferred,
            );
        }
        for c in &t.columns {
            if c.check.is_some() {
                push(
                    format!("{}_{}_check", t.name, c.name),
                    &t.name,
                    "CHECK",
                    false,
                );
            }
        }
    }
    out
}

pub(crate) const KEY_COLUMN_USAGE_COLUMNS: &[(&str, C)] = &[
    ("constraint_catalog", C::Text),
    ("constraint_schema", C::Text),
    ("constraint_name", C::Text),
    ("table_catalog", C::Text),
    ("table_schema", C::Text),
    ("table_name", C::Text),
    ("column_name", C::Text),
    ("ordinal_position", C::Int64),
    ("position_in_unique_constraint", C::Int64),
];

pub(crate) fn key_column_usage(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        let mut push = |name: String, cols: &[u16], in_unique: bool| {
            for (pos, &c) in cols.iter().enumerate() {
                let Some(col) = t.columns.get(c as usize) else {
                    continue;
                };
                out.push(vec![
                    text(CATALOG),
                    text(SCHEMA_NAME),
                    text(name.clone()),
                    text(CATALOG),
                    text(SCHEMA_NAME),
                    text(&t.name),
                    text(&col.name),
                    // ordinal_position is 1-based and is the position WITHIN
                    // THE CONSTRAINT, not within the table — a composite key's
                    // second column is 2 even if it is the table's fifth.
                    int(pos as i64 + 1),
                    if in_unique {
                        Value::Int(pos as i64 + 1)
                    } else {
                        Value::Null
                    },
                ]);
            }
        };
        if !t.primary_key.is_empty() {
            push(format!("{}_pkey", t.name), &t.primary_key, false);
        }
        for (i, idx) in t.indexes.iter().enumerate() {
            if idx.unique {
                push(
                    idx.name
                        .clone()
                        .unwrap_or_else(|| format!("{}_{}_key", t.name, i + 1)),
                    &idx.columns,
                    false,
                );
            }
        }
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            push(
                fk.name
                    .clone()
                    .unwrap_or_else(|| format!("{}_fk_{i}", t.name)),
                &fk.columns,
                true,
            );
        }
    }
    out
}

pub(crate) const REFERENTIAL_CONSTRAINTS_COLUMNS: &[(&str, C)] = &[
    ("constraint_catalog", C::Text),
    ("constraint_schema", C::Text),
    ("constraint_name", C::Text),
    ("unique_constraint_catalog", C::Text),
    ("unique_constraint_schema", C::Text),
    ("unique_constraint_name", C::Text),
    ("match_option", C::Text),
    ("update_rule", C::Text),
    ("delete_rule", C::Text),
];

pub(crate) fn referential_constraints(schema: &Schema) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for t in live_tables(schema) {
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            out.push(vec![
                text(CATALOG),
                text(SCHEMA_NAME),
                text(
                    fk.name
                        .clone()
                        .unwrap_or_else(|| format!("{}_fk_{i}", t.name)),
                ),
                text(CATALOG),
                text(SCHEMA_NAME),
                text(format!("{}_pkey", fk.parent)),
                text("SIMPLE"),
                text(fk_action(fk.on_update)),
                text(fk_action(fk.on_delete)),
            ]);
        }
    }
    out
}

fn fk_action(a: mpedb_types::schema::FkAction) -> &'static str {
    use mpedb_types::schema::FkAction as A;
    match a {
        A::NoAction => "NO ACTION",
        A::Restrict => "RESTRICT",
        A::Cascade => "CASCADE",
        A::SetNull => "SET NULL",
        A::SetDefault => "SET DEFAULT",
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample;
    use super::*;
    use crate::pg::catalog::lookup;

    fn col(name: &str, c: &str, row: &[Value]) -> Value {
        let rel = lookup(name).unwrap();
        let i = rel
            .columns
            .iter()
            .position(|(n, _)| *n == c)
            .unwrap_or_else(|| panic!("{name} has no column {c}"));
        row[i].clone()
    }

    #[test]
    fn is_nullable_is_the_strings_yes_and_no_not_a_boolean() {
        // A client comparing `is_nullable = 'YES'` gets no rows if this is a
        // bool, and no error either — the table just looks all-NOT-NULL.
        let rows = columns(&sample());
        let nick = rows
            .iter()
            .find(|r| col("information_schema.columns", "column_name", r) == Value::Text("nick".into()))
            .unwrap();
        assert_eq!(
            col("information_schema.columns", "is_nullable", nick),
            Value::Text("YES".into())
        );
        let id = rows
            .iter()
            .find(|r| col("information_schema.columns", "column_name", r) == Value::Text("id".into()))
            .unwrap();
        assert_eq!(
            col("information_schema.columns", "is_nullable", id),
            Value::Text("NO".into())
        );
    }

    #[test]
    fn data_type_uses_the_standards_spelling_and_udt_name_uses_postgresqls() {
        // The two columns genuinely differ, and reflection code reads one or
        // the other depending on the library. Reporting `int8` in `data_type`
        // makes SQLAlchemy fall through to a generic type.
        let rows = columns(&sample());
        let id = rows
            .iter()
            .find(|r| col("information_schema.columns", "column_name", r) == Value::Text("id".into()))
            .unwrap();
        assert_eq!(
            col("information_schema.columns", "data_type", id),
            Value::Text("bigint".into())
        );
        assert_eq!(
            col("information_schema.columns", "udt_name", id),
            Value::Text("int8".into())
        );
    }

    #[test]
    fn ordinal_position_in_key_column_usage_counts_within_the_constraint() {
        // Not within the table. A composite PK on the table's 1st and 2nd
        // columns and one on its 4th and 5th must both read 1, 2.
        let rows = key_column_usage(&sample());
        let mut pk: Vec<(String, i64)> = rows
            .iter()
            .filter(|r| {
                col("information_schema.key_column_usage", "constraint_name", r)
                    == Value::Text("memberships_pkey".into())
            })
            .map(|r| {
                let Value::Text(c) = col("information_schema.key_column_usage", "column_name", r)
                else {
                    panic!()
                };
                let Value::Int(p) =
                    col("information_schema.key_column_usage", "ordinal_position", r)
                else {
                    panic!()
                };
                (c, p)
            })
            .collect();
        pk.sort();
        assert_eq!(
            pk,
            vec![("group_id".to_string(), 2), ("user_id".to_string(), 1)]
        );
    }

    #[test]
    fn every_live_table_appears_exactly_once_as_a_base_table() {
        let rows = tables(&sample());
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(
                col("information_schema.tables", "table_type", r),
                Value::Text("BASE TABLE".into())
            );
            assert_eq!(
                col("information_schema.tables", "table_catalog", r),
                Value::Text("mpedb".into())
            );
        }
    }

    #[test]
    fn text_columns_report_no_maximum_length_rather_than_a_made_up_one() {
        // mpedb's Text is not varchar(n). A number here would promise an
        // enforcement that does not exist, and a client would truncate.
        let rows = columns(&sample());
        for r in &rows {
            if col("information_schema.columns", "data_type", r) == Value::Text("text".into()) {
                assert_eq!(
                    col(
                        "information_schema.columns",
                        "character_maximum_length",
                        r
                    ),
                    Value::Null
                );
            }
        }
    }
}
