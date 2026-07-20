//! What a compiled statement TOUCHES, at column granularity — the report an
//! authorization layer, an audit log or a policy gate needs *before* the
//! statement runs.
//!
//! The [`Footprint`](mpedb_types::Footprint) already names the tables a plan
//! reads and writes; this refines that to the columns, and labels each touch
//! with the operation that reaches it (read / insert / update / delete). It is
//! a pure function of an already-compiled [`CompiledPlan`] and the live
//! [`Schema`]: nothing is parsed, nothing is executed, no page is read.
//!
//! # The one approximation, and its direction
//!
//! Column attribution is EXACT for a single-table statement — a plain SELECT
//! with no join and no lifted subquery, and every UPDATE/DELETE (which are
//! single-table by construction). For everything else — joins, compounds,
//! recursive CTEs, materialized derived tables, lifted subqueries — the plan's
//! column indices address a CONCATENATED tuple whose layout depends on the
//! shape of the pipeline, and a mis-mapped index would name the wrong column.
//! Rather than guess, those report **every column of every table the footprint
//! says is read**.
//!
//! That over-reports, never under-reports, which is the only safe direction
//! for the consumers above: a gate fed this list can refuse something it would
//! otherwise have allowed, but can never be told a column was untouched when
//! it was. [`AccessReport::exact_columns`] says which of the two happened, so a
//! caller that cares can tell them apart.

use mpedb_sql::{
    AccessPath, CompiledPlan, DdlStmt, GroupKey, OrderOver, PlanStmt, Projection, SelectPlan,
};
use mpedb_types::{ExprProgram, Instr, Schema};

/// One thing a statement does to one schema object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// A SELECT is being evaluated (one per select node, sqlite's
    /// `SQLITE_SELECT`). Carries no object — the reads below name those.
    Select,
    /// A column's value is read.
    Read { table: String, column: String },
    /// A row is inserted into a table.
    Insert { table: String },
    /// A column is assigned by an UPDATE.
    Update { table: String, column: String },
    /// A row is deleted from a table.
    Delete { table: String },
    /// A schema object is created. `table` is the table an index/trigger/policy
    /// hangs off, `None` for a standalone object.
    Create { kind: ObjectKind, name: String, table: Option<String> },
    /// A schema object is dropped.
    Drop { kind: ObjectKind, name: String, table: Option<String> },
    /// `ALTER TABLE` — rename, add or drop a column.
    Alter { table: String },
    /// Transaction control.
    Transaction { op: TxnOp },
    /// Savepoint control, with the savepoint's name.
    Savepoint { op: TxnOp, name: String },
}

/// The kind of schema object a `Create`/`Drop` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    VirtualTable,
    Index,
    View,
    Trigger,
    Policy,
}

/// Which end of a transaction or savepoint a control statement is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnOp {
    Begin,
    Commit,
    Rollback,
    Release,
}

/// The full set of touches one compiled statement makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessReport {
    /// In a stable order: `Select` first when present, then reads (table then
    /// column, in schema order), then the write actions.
    pub actions: Vec<Access>,
    /// True when every `Read`/`Update` names a column the statement really
    /// touches; false when the columns were widened to "every column of every
    /// table read" (see the module docs).
    pub exact_columns: bool,
}

/// Derive the access report for `plan` against `schema`.
pub fn plan_access(plan: &CompiledPlan, schema: &Schema) -> AccessReport {
    let mut b = Builder { schema, actions: Vec::new(), exact: true };
    match &plan.stmt {
        PlanStmt::Select(sp) => {
            b.actions.push(Access::Select);
            if plan.subplans.is_empty() && sp.joins.is_empty() {
                let mut cols = select_base_cols(sp);
                collect_access_cols(schema, sp.table, &sp.access, &mut cols);
                b.read_cols(sp.table, &cols);
            } else {
                b.widen(plan);
            }
        }
        // Every remaining read-only shape addresses a tuple whose layout is
        // the pipeline's, not one table's: widen.
        PlanStmt::Compound(_) | PlanStmt::RecursiveCte(_) | PlanStmt::Derived(_) => {
            b.actions.push(Access::Select);
            b.widen(plan);
        }
        PlanStmt::Insert { table, from_select, .. } => {
            b.push_write(Access::Insert { table: b.table_name(*table) });
            // `INSERT … SELECT` also READS the source query.
            if from_select.is_some() {
                b.actions.insert(0, Access::Select);
                b.widen_reads_only(plan, *table);
            }
        }
        PlanStmt::Update { table, access, filter, set, .. } => {
            let t = *table;
            if plan.subplans.is_empty() {
                let mut cols = Vec::new();
                collect_access_cols(schema, t, access, &mut cols);
                if let Some(f) = filter {
                    collect_prog_cols(f, &mut cols);
                }
                for (c, p) in set {
                    cols.push(*c);
                    collect_prog_cols(p, &mut cols);
                }
                b.read_cols(t, &cols);
            } else {
                b.widen(plan);
            }
            let name = b.table_name(t);
            let assigned: Vec<u16> = set.iter().map(|(c, _)| *c).collect();
            for c in assigned {
                if let Some(col) = b.column_name(t, c) {
                    b.actions.push(Access::Update { table: name.clone(), column: col });
                }
            }
        }
        PlanStmt::Delete { table, access, filter, .. } => {
            let t = *table;
            if plan.subplans.is_empty() {
                let mut cols = Vec::new();
                collect_access_cols(schema, t, access, &mut cols);
                if let Some(f) = filter {
                    collect_prog_cols(f, &mut cols);
                }
                b.read_cols(t, &cols);
            } else {
                b.widen(plan);
            }
            b.push_write(Access::Delete { table: b.table_name(t) });
        }
        // Transaction control names no table.
        PlanStmt::Begin => b.actions.push(Access::Transaction { op: TxnOp::Begin }),
        PlanStmt::Commit => b.actions.push(Access::Transaction { op: TxnOp::Commit }),
        PlanStmt::Rollback => b.actions.push(Access::Transaction { op: TxnOp::Rollback }),
        PlanStmt::Savepoint(n) => {
            b.actions.push(Access::Savepoint { op: TxnOp::Begin, name: n.clone() })
        }
        PlanStmt::Release(n) => {
            b.actions.push(Access::Savepoint { op: TxnOp::Release, name: n.clone() })
        }
        PlanStmt::RollbackTo(n) => {
            b.actions.push(Access::Savepoint { op: TxnOp::Rollback, name: n.clone() })
        }
    }
    AccessReport { actions: b.actions, exact_columns: b.exact }
}

/// The access report for a DDL statement, which compiles to no plan at all —
/// the facade applies it against the catalog directly, so the parsed
/// [`DdlStmt`] is the only description of it there is.
pub fn ddl_access(ddl: &DdlStmt) -> AccessReport {
    use ObjectKind as K;
    let create = |kind, name: &str, table: Option<&str>| Access::Create {
        kind,
        name: name.to_string(),
        table: table.map(str::to_string),
    };
    let drop = |kind, name: &str, table: Option<&str>| Access::Drop {
        kind,
        name: name.to_string(),
        table: table.map(str::to_string),
    };
    let actions = match ddl {
        DdlStmt::CreateTable(s) => vec![create(K::Table, &s.name, None)],
        DdlStmt::CreateVirtualTable(s) => vec![create(K::VirtualTable, &s.name, None)],
        DdlStmt::DropTable { name, .. } => vec![drop(K::Table, name, None)],
        DdlStmt::CreateIndex { name, table, .. } => {
            vec![create(K::Index, name, Some(table))]
        }
        DdlStmt::CreateView { name, .. } => vec![create(K::View, name, None)],
        DdlStmt::DropView { name, .. } => vec![drop(K::View, name, None)],
        DdlStmt::CreateTrigger(s) => vec![create(K::Trigger, &s.name, Some(&s.table))],
        DdlStmt::DropTrigger { name, .. } => vec![drop(K::Trigger, name, None)],
        DdlStmt::CreatePolicy(s) => vec![create(K::Policy, &s.name, Some(&s.table))],
        DdlStmt::DropPolicy { table, name } => vec![drop(K::Policy, name, Some(table))],
        DdlStmt::AlterRenameTable { table, .. }
        | DdlStmt::AlterRenameColumn { table, .. }
        | DdlStmt::AlterAddColumn { table, .. }
        | DdlStmt::AlterDropColumn { table, .. }
        | DdlStmt::AlterRls { table, .. } => vec![Access::Alter { table: table.clone() }],
        // Maintenance statements mpedb accepts as no-ops: nothing is touched.
        DdlStmt::Analyze { .. } | DdlStmt::Reindex { .. } => Vec::new(),
    };
    AccessReport { actions, exact_columns: true }
}

struct Builder<'a> {
    schema: &'a Schema,
    actions: Vec<Access>,
    exact: bool,
}

impl Builder<'_> {
    fn table_name(&self, id: u32) -> String {
        self.schema.table(id).map_or_else(|| format!("table#{id}"), |t| t.name.clone())
    }

    fn column_name(&self, table: u32, col: u16) -> Option<String> {
        self.schema
            .table(table)
            .and_then(|t| t.columns.get(col as usize))
            .map(|c| c.name.clone())
    }

    /// Emit `Read` for the given base-row column indices of one table, in
    /// schema order and deduplicated.
    fn read_cols(&mut self, table: u32, cols: &[u16]) {
        let name = self.table_name(table);
        let mut ids: Vec<u16> = cols.to_vec();
        ids.sort_unstable();
        ids.dedup();
        for c in ids {
            if let Some(col) = self.column_name(table, c) {
                self.actions.push(Access::Read { table: name.clone(), column: col });
            }
        }
    }

    /// The conservative fallback: every column of every table the footprint
    /// reads. Marks the report inexact.
    fn widen(&mut self, plan: &CompiledPlan) {
        self.exact = false;
        self.widen_tables(plan, None);
    }

    /// Same, but skipping `except` — the INSERT target is a write, not a read.
    fn widen_reads_only(&mut self, plan: &CompiledPlan, except: u32) {
        self.exact = false;
        self.widen_tables(plan, Some(except));
    }

    fn widen_tables(&mut self, plan: &CompiledPlan, except: Option<u32>) {
        for id in plan.footprint.tables_read.iter() {
            if except == Some(id) {
                continue;
            }
            let Some(t) = self.schema.table(id) else { continue };
            if t.dead {
                continue;
            }
            let name = t.name.clone();
            let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            for column in cols {
                self.actions.push(Access::Read { table: name.clone(), column });
            }
        }
    }

    fn push_write(&mut self, a: Access) {
        self.actions.push(a);
    }
}

/// Base-row column indices a single-table SELECT plan reads.
///
/// Every term below is documented (on `SelectPlan`) as being evaluated over
/// the BASE row, which is what makes this exact for a join-free, subquery-free
/// plan: `filter`/`post_filter`, the projection, the group keys, each
/// aggregate's argument and FILTER, the bare columns, and — only when the sort
/// runs over the base row — the sort keys. `having` reads the GROUPED tuple,
/// whose every input is one of the above, so it needs no separate walk.
fn select_base_cols(sp: &SelectPlan) -> Vec<u16> {
    let mut cols = Vec::new();
    if let Some(f) = &sp.filter {
        collect_prog_cols(f, &mut cols);
    }
    if let Some(f) = &sp.post_filter {
        collect_prog_cols(f, &mut cols);
    }
    // The projection is over the BASE row only when there is no aggregation.
    // Under `GROUP BY` it addresses the GROUPED tuple `[keys ‖ aggs ‖ bare]`,
    // where slot 0 is the first group key, not table column 0 — reading those
    // indices as base columns names an unrelated column. The aggregation arm
    // below covers every base-row read a grouped plan makes.
    if sp.aggregate.is_none() {
        for p in &sp.projection {
            match p {
                Projection::Column(i) => cols.push(*i),
                Projection::Expr { program, .. } => collect_prog_cols(program, &mut cols),
            }
        }
    }
    if let Some(agg) = &sp.aggregate {
        for k in &agg.group_by {
            match k {
                GroupKey::Col(i) => cols.push(*i),
                GroupKey::Expr(p) => collect_prog_cols(p, &mut cols),
            }
        }
        for a in &agg.aggs {
            if let Some(p) = &a.arg {
                collect_prog_cols(p, &mut cols);
            }
            if let Some(p) = &a.filter {
                collect_prog_cols(p, &mut cols);
            }
        }
        cols.extend_from_slice(&agg.bare_cols);
    }
    if sp.order_over == OrderOver::BaseRow {
        cols.extend(sp.order_by.iter().map(|(i, _, _)| *i));
    }
    cols
}

fn collect_prog_cols(p: &ExprProgram, out: &mut Vec<u16>) {
    for instr in &p.instrs {
        if let Instr::PushCol(i) = instr {
            out.push(*i);
        }
    }
}

/// The columns an access path pins. A key probe reads the key columns even
/// when no projection or filter mentions them.
fn collect_access_cols(schema: &Schema, table: u32, access: &AccessPath, out: &mut Vec<u16>) {
    let Some(t) = schema.table(table) else { return };
    let index_cols = |no: u32| -> &[u16] {
        no.checked_sub(1)
            .and_then(|i| t.indexes.get(i as usize))
            .map_or(&[][..], |d| &d.columns)
    };
    match access {
        AccessPath::PkPoint(parts) => out.extend(t.primary_key.iter().take(parts.len())),
        AccessPath::PkRange { .. } => out.extend(t.primary_key.first().copied()),
        AccessPath::IndexPoint { index_no, parts } => {
            out.extend(index_cols(*index_no).iter().take(parts.len()))
        }
        AccessPath::IndexRange { index_no, .. } => {
            out.extend(index_cols(*index_no).first().copied())
        }
        AccessPath::FullScan => {}
        // An FTS match consults the inverted index over every indexed column.
        AccessPath::FtsScan { .. } => out.extend(0..t.columns.len() as u16),
    }
}
