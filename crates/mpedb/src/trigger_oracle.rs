//! Oracle PL/SQL row triggers. The body is recompiled from its stored source
//! against the live schema. `:NEW` is the first image and `:OLD` the second;
//! a side this event does not have is NULL, and a NULL column stays NULL.
//! The body observes those values. It does not change the row buffer, and it
//! does not take a lock of its own: embedded SQL runs on the firing transaction
//! through the same bridge as a PySpell trigger.

use crate::{ColumnType, Error, Result, Schema, TableDef, Value};
use mpedb_spell::emit::CallKind;
use mpedb_spell::ir::{PlanKind, PlanRef, Proc};
use mpedb_spell::plsql::{TriggerImage, TriggerImageType};
use mpedb_sql::PlanStmt;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct OracleBody {
    pub proc: Arc<Proc>,
    pub plans: HashMap<[u8; 32], Arc<mpedb_sql::CompiledPlan>>,
}

pub(crate) fn compile_body(source: &str, table: &TableDef, schema: &Schema) -> Result<OracleBody> {
    let images = trigger_images(table)?;
    let skel = mpedb_spell::plsql::compile_trigger(source, &images).map_err(|err| {
        Error::Bind(format!("CREATE TRIGGER: {err}"))
    })?;
    let mut plans = HashMap::with_capacity(skel.calls.len());
    let mut refs = Vec::with_capacity(skel.calls.len());
    for call in &skel.calls {
        let plan = mpedb_sql::prepare(&call.sql, schema).map_err(|err| {
            Error::Bind(format!("CREATE TRIGGER: embedded SQL failed to compile: {err}"))
        })?;
        if matches!(
            plan.stmt,
            PlanStmt::Begin
                | PlanStmt::Commit
                | PlanStmt::Rollback
                | PlanStmt::Savepoint(_)
                | PlanStmt::Release(_)
                | PlanStmt::RollbackTo(_)
        ) {
            return Err(Error::Bind(
                "CREATE TRIGGER: BEGIN/COMMIT/ROLLBACK/SAVEPOINT cannot appear inside a trigger"
                    .into(),
            ));
        }
        let kind = match call.kind {
            CallKind::Query if plan.footprint.read_only => PlanKind::Query,
            CallKind::Exec if !plan.footprint.read_only => PlanKind::Exec,
            CallKind::Query | CallKind::Rows => {
                return Err(Error::Bind(
                    "CREATE TRIGGER: a PL/SQL trigger SELECT must be read-only".into(),
                ))
            }
            CallKind::Exec => {
                return Err(Error::Bind(
                    "CREATE TRIGGER: a PL/SQL trigger DML statement was read-only".into(),
                ))
            }
        };
        if let Some(expected) = call.expected_columns {
            let actual = plan.output_decltypes(schema).len();
            if actual != expected as usize {
                return Err(Error::Bind(format!(
                    "CREATE TRIGGER: query returns {actual} columns but INTO has {expected} targets"
                )));
            }
        }
        if plan.n_user_params() as usize != call.argc as usize {
            return Err(Error::Bind(format!(
                "CREATE TRIGGER: SQL takes {} parameter(s) but {} argument(s) were passed",
                plan.n_user_params(),
                call.argc
            )));
        }
        if let Some(key) = plan
            .context_keys
            .iter()
            .find(|key| key.as_str() != mpedb_sql::STATEMENT_INSTANT_KEY)
        {
            return Err(Error::Bind(format!(
                "CREATE TRIGGER: embedded statement uses current_setting('{key}'), \
                 which needs a session and is not available inside a trigger"
            )));
        }
        let hash = plan.hash();
        plans.insert(hash.0, Arc::new(plan));
        refs.push(PlanRef {
            hash,
            kind,
            argc: call.argc,
        });
    }
    let proc = Proc::new(
        skel.name,
        skel.argc,
        skel.nlocals,
        refs,
        skel.consts,
        skel.instrs,
    )?;
    Ok(OracleBody {
        proc: Arc::new(proc),
        plans,
    })
}

/// `argc` is two images in table order. `None` for an image means every
/// column of that side is NULL. A present image that is short is an internal
/// bug, same as the SQL trigger path.
pub(crate) fn image_args(
    argc: usize,
    new_row: Option<&[Value]>,
    old_row: Option<&[Value]>,
) -> Result<Vec<Value>> {
    if !argc.is_multiple_of(2) {
        return Err(Error::Internal(
            "oracle trigger image arity is not even".into(),
        ));
    }
    let width = argc / 2;
    let mut args = Vec::with_capacity(argc);
    push_image(&mut args, new_row, width)?;
    push_image(&mut args, old_row, width)?;
    Ok(args)
}

fn push_image(args: &mut Vec<Value>, row: Option<&[Value]>, width: usize) -> Result<()> {
    match row {
        None => {
            args.resize(args.len() + width, Value::Null);
            Ok(())
        }
        Some(row) => {
            if row.len() < width {
                return Err(Error::Internal(
                    "trigger NEW/OLD column out of row bounds".into(),
                ));
            }
            args.extend_from_slice(&row[..width]);
            Ok(())
        }
    }
}

fn trigger_images(table: &TableDef) -> Result<Vec<TriggerImage>> {
    let mut out = Vec::with_capacity(table.columns.len());
    for column in &table.columns {
        let ty = match column.ty {
            ColumnType::Text => TriggerImageType::Text,
            ColumnType::Int64 => TriggerImageType::Integer,
            other => {
                return Err(Error::Bind(format!(
                    "CREATE TRIGGER: a PL/SQL trigger supports INTEGER and TEXT columns only; \
                     `{}.{}` is {other:?}",
                    table.name, column.name
                )))
            }
        };
        out.push(TriggerImage {
            name: column.name.clone(),
            ty,
        });
    }
    Ok(out)
}
