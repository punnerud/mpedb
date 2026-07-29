//! FOREIGN KEY enforcement (#194).
//!
//! # Where this sits
//!
//! The declaration lives in the schema ([`mpedb_types::ForeignKeyDef`], canonical
//! bytes v12); the enforcement lives HERE, next to triggers, and for the same
//! reason triggers do: a foreign key is not a per-row predicate over one row's
//! own columns — it reads ANOTHER table, and its actions WRITE other tables. The
//! engine's `validate_row_in` cannot do either, and it is not called on delete
//! at all, which is exactly the half a foreign key needs most.
//!
//! # sqlite parity, measured (3.45.1) rather than assumed
//!
//! Every rule below was probed against the bundled sqlite before it was
//! written. The ones that are NOT what a reasonable person would guess:
//!
//! * **A forward reference is legal.** `CREATE TABLE c (p REFERENCES par(id))`
//!   succeeds with no `par` in the schema; the parent is resolved at WRITE
//!   time, and `no such table: main.par` is what an INSERT then says. This is
//!   why [`mpedb_types::ForeignKeyDef::parent`] is a name.
//! * **A parent key that is neither the PK nor a UNIQUE index is a write-time
//!   error**, not a DDL error: `foreign key mismatch - "c" referencing "par"`.
//! * **MATCH SIMPLE, always.** A composite key with ANY NULL member is not
//!   checked. `MATCH FULL`/`PARTIAL` parse and behave as SIMPLE — sqlite
//!   implements no other mode, so following sqlite means ignoring the word.
//! * **Enforcement is OFF by default**, in sqlite and here. `PRAGMA
//!   foreign_keys = ON` turns it on per connection, and is a NO-OP inside a
//!   transaction (measured: it silently keeps the old value).
//! * **`DROP TABLE` counts as deleting every row**, so dropping a parent with
//!   live children fails.
//!
//! # The cost when there is no key
//!
//! Nothing is built at all unless the connection asked for it: `WriteRules.fks`
//! is `None` under the default `PRAGMA foreign_keys = OFF`, and the write path's
//! first question is that `Option`. With it on, a table with no outgoing key and
//! no incoming one costs two `HashMap` misses per STATEMENT — not per row: the
//! graph is built once per `schema_gen` and cached on the
//! [`Database`](crate::Database) exactly like the trigger set.

use mpedb_types::{Error, FkAction, ForeignKeyDef, Result, Schema, TableDef, Value};
use std::collections::HashMap;

/// Maximum depth of a cascade chain (`A` deletes `B` deletes `C` …). sqlite's
/// own limit is `SQLITE_MAX_TRIGGER_DEPTH` (1000) because it implements FK
/// actions AS triggers; mpedb charges the #74 work meter per cascaded row on
/// top, so a wide fan-out trips a budget rather than running to exhaustion.
pub(crate) const MAX_FK_DEPTH: u32 = 64;

/// Which tables have foreign keys, in both directions, for one schema
/// generation.
///
/// Both maps are keyed by TABLE ID for the child side and by lowercased NAME
/// for the parent side, because that is how each side is known: a child's key
/// is stored on its own `TableDef`, and its parent is a name that may not
/// resolve yet.
#[derive(Debug, Default)]
pub(crate) struct FkGraph {
    /// Child table id → nothing (presence is the whole answer; the keys
    /// themselves are read off the schema, which the caller already holds).
    children: HashMap<u32, ()>,
    /// Lowercased parent NAME → `(child table id, index into that table's
    /// `foreign_keys`)`. A table that nothing references is absent.
    incoming: HashMap<String, Vec<(u32, usize)>>,
}

impl FkGraph {
    /// Build from a live schema. Dead (tombstoned) tables contribute nothing.
    pub(crate) fn build(schema: &Schema) -> FkGraph {
        let mut g = FkGraph::default();
        for t in &schema.tables {
            if t.dead || t.foreign_keys.is_empty() {
                continue;
            }
            g.children.insert(t.id, ());
            for (i, fk) in t.foreign_keys.iter().enumerate() {
                g.incoming
                    .entry(fk.parent.to_ascii_lowercase())
                    .or_default()
                    .push((t.id, i));
            }
        }
        g
    }

    /// Does `table` declare a key of its own (so an INSERT/UPDATE of it must
    /// probe parents)?
    pub(crate) fn has_outgoing(&self, table: u32) -> bool {
        self.children.contains_key(&table)
    }

    /// The keys pointing AT `name` — what a DELETE or a key-changing UPDATE of
    /// that table has to act on.
    pub(crate) fn incoming(&self, name: &str) -> &[(u32, usize)] {
        static NONE: &[(u32, usize)] = &[];
        self.incoming
            .get(&name.to_ascii_lowercase())
            .map_or(NONE, |v| v.as_slice())
    }

    /// Is `name` referenced by anything? Cheaper than [`Self::incoming`] when
    /// the answer only gates work.
    pub(crate) fn has_incoming(&self, name: &str) -> bool {
        self.incoming.contains_key(&name.to_ascii_lowercase())
    }
}

/// How a parent row is found from a key value.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ParentProbe {
    /// The parent columns ARE the primary key, in primary-key order.
    Pk,
    /// A UNIQUE index (`index_no`, i.e. position + 1) over exactly those
    /// columns in exactly that order.
    Index(u32),
}

/// A foreign key with its parent resolved against the live schema.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub parent_id: u32,
    /// Parent column ordinals in KEY order (the same order as the child's).
    pub parent_cols: Vec<u16>,
    pub probe: ParentProbe,
}

/// Resolve one key's parent side. This is where sqlite's two write-time errors
/// are raised, and they are raised HERE rather than at `CREATE TABLE` because
/// sqlite raises them here (measured — see the module docs).
pub(crate) fn resolve(schema: &Schema, child: &TableDef, fk: &ForeignKeyDef) -> Result<Resolved> {
    let parent = schema
        .tables
        .iter()
        .find(|t| !t.dead && t.name.eq_ignore_ascii_case(&fk.parent))
        .ok_or_else(|| Error::Bind(format!("no such table: main.{}", fk.parent)))?;
    let mismatch = || {
        Error::Bind(format!(
            "foreign key mismatch - \"{}\" referencing \"{}\"",
            child.name, parent.name
        ))
    };
    // No column list means "the parent's PRIMARY KEY" — and an implicit-rowid
    // parent has one, the hidden `rowid` column (#94), which is what sqlite
    // resolves a bare `REFERENCES t` to as well.
    let parent_cols: Vec<u16> = if fk.parent_columns.is_empty() {
        parent.primary_key.clone()
    } else {
        fk.parent_columns
            .iter()
            .map(|n| {
                parent
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(n))
                    .map(|p| p as u16)
                    .ok_or_else(mismatch)
            })
            .collect::<Result<Vec<u16>>>()?
    };
    if parent_cols.len() != fk.columns.len() || parent_cols.is_empty() {
        return Err(mismatch());
    }
    // The parent key must be UNIQUE, or the "does a parent exist" question has
    // no single answer and `ON UPDATE CASCADE` no single row to carry. sqlite
    // calls this a mismatch too.
    let probe = if parent_cols == parent.primary_key {
        ParentProbe::Pk
    } else {
        let ix = parent
            .indexes
            .iter()
            .position(|ix| ix.unique && ix.predicate.is_none() && ix.columns == parent_cols)
            .ok_or_else(mismatch)?;
        ParentProbe::Index(ix as u32 + 1)
    };
    Ok(Resolved {
        parent_id: parent.id,
        parent_cols,
        probe,
    })
}

/// The key value a row carries for one side of a foreign key, or `None` when
/// ANY member is NULL — MATCH SIMPLE, which means "not checked".
pub(crate) fn key_of(row: &[Value], cols: &[u16]) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(cols.len());
    for &c in cols {
        let v = row.get(c as usize)?;
        if v.is_null() {
            return None;
        }
        out.push(v.clone());
    }
    Some(out)
}

/// The key value with NULLs allowed through — used on the PARENT side, where a
/// NULL key member means no child can be pointing at it either, and on the
/// child side when comparing an old key to a new one.
pub(crate) fn key_of_raw(row: &[Value], cols: &[u16]) -> Option<Vec<Value>> {
    cols.iter()
        .map(|&c| row.get(c as usize).cloned())
        .collect::<Option<Vec<Value>>>()
}

/// The violation, shaped for the error. `constraint` is the declared
/// `CONSTRAINT <name>`, kept for the native surface only — the C-API flattens
/// every foreign-key failure to sqlite's bare string.
pub(crate) fn violation(table: &str, fk: &ForeignKeyDef) -> Error {
    Error::ForeignKeyViolation {
        table: table.to_string(),
        constraint: fk.name.clone(),
    }
}

/// Does this action refuse rather than mutate?
pub(crate) fn refuses(a: FkAction) -> bool {
    matches!(a, FkAction::NoAction | FkAction::Restrict)
}

// ---------------------------------------------------------------------------
// Enforcement
// ---------------------------------------------------------------------------

use crate::exec::TxnCtx;

/// One violation held over to COMMIT, for a `DEFERRABLE INITIALLY DEFERRED`
/// key. The row is kept BY KEY, not by value: the point of deferring is that
/// the statement in between may fix it, and the fix is a row that did not exist
/// when the violation was recorded.
#[derive(Debug, Clone)]
pub(crate) struct Deferred {
    /// The CHILD table whose row is (still?) dangling.
    pub child: u32,
    /// Which of that table's keys.
    pub fk_index: usize,
    /// The key value the child row carried. Re-probed at commit.
    pub key: Vec<Value>,
}

/// Check every key `table`'s row declares — the INSERT/UPDATE side.
///
/// `deferred` collects the violations a `DEFERRABLE INITIALLY DEFERRED` key
/// produces instead of raising them; an immediate key raises here.
pub(crate) fn check_child(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    table: u32,
    row: &[Value],
    deferred: &mut Vec<Deferred>,
) -> Result<()> {
    let t = table_of(schema, table)?;
    for (i, fk) in t.foreign_keys.iter().enumerate() {
        // MATCH SIMPLE: any NULL in the key and the row is unconstrained.
        let Some(key) = key_of(row, &fk.columns) else {
            continue;
        };
        let r = resolve(schema, t, fk)?;
        if parent_exists(ctx, &r, &key)? {
            continue;
        }
        if fk.deferred {
            deferred.push(Deferred {
                child: table,
                fk_index: i,
                key,
            });
        } else {
            return Err(violation(&t.name, fk));
        }
    }
    Ok(())
}

/// Is there a parent row for this key? One point probe — PK or unique index.
fn parent_exists(ctx: &mut dyn TxnCtx, r: &Resolved, key: &[Value]) -> Result<bool> {
    Ok(match r.probe {
        ParentProbe::Pk => ctx.get_by_pk(r.parent_id, key)?.is_some(),
        ParentProbe::Index(no) => ctx.get_by_index(r.parent_id, no, key)?.is_some(),
    })
}

/// The two halves of acting on a parent row that is going away (`new = None`)
/// or being re-keyed (`new = Some`).
///
/// They are separate because they run on OPPOSITE SIDES of the write, and that
/// is not a stylistic choice:
///
/// * [`Phase::Guard`] refuses (`NO ACTION`/`RESTRICT`) and runs BEFORE the row
///   is touched, so a refusal leaves the database exactly as it was. sqlite
///   implements these as AFTER triggers and unwinds the statement instead; the
///   outcome is the same and the intermediate state is cleaner.
/// * [`Phase::Act`] mutates (`CASCADE`/`SET NULL`/`SET DEFAULT`) and must run
///   AFTER, because a cascaded child is re-checked against the parent — and
///   under `ON UPDATE CASCADE` the key it is carried to only exists once the
///   parent has been rewritten. Acting first made a legal cascade refuse
///   itself, which is how this split was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Guard,
    Act,
}

/// Act on every key pointing AT `table` because one of its rows is going away
/// (`new = None`) or being re-keyed (`new = Some`).
///
/// This is the half `validate_row_in` structurally cannot do: it runs on
/// DELETE, and it WRITES other tables.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_parent_change(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    graph: &FkGraph,
    table: u32,
    old: &[Value],
    new: Option<&[Value]>,
    deferred: &mut Vec<Deferred>,
    phase: Phase,
    depth: u32,
) -> Result<()> {
    if depth > MAX_FK_DEPTH {
        return Err(Error::Unsupported(format!(
            "foreign key cascade nested deeper than {MAX_FK_DEPTH} levels — \
             the chain of ON DELETE/UPDATE actions does not terminate"
        )));
    }
    let parent_def = table_of(schema, table)?;
    let refs: Vec<(u32, usize)> = graph.incoming(&parent_def.name).to_vec();
    for (child_id, fk_ix) in refs {
        let child = table_of(schema, child_id)?;
        let Some(fk) = child.foreign_keys.get(fk_ix) else {
            continue;
        };
        let r = resolve(schema, child, fk)?;
        // A NULL member on the parent side means nothing could be referencing
        // it (a child with a NULL key member is unchecked, one with a non-NULL
        // key never matched a NULL parent value).
        let Some(old_key) = key_of(old, &r.parent_cols) else {
            continue;
        };
        let new_key = match new {
            None => None,
            Some(n) => {
                let nk = key_of_raw(n, &r.parent_cols);
                // Not re-keyed: an UPDATE that leaves the key alone is not a
                // foreign-key event at all.
                if nk.as_deref() == Some(old_key.as_slice()) {
                    continue;
                }
                nk
            }
        };
        let action = if new.is_none() {
            fk.on_delete
        } else {
            fk.on_update
        };
        // Each phase owns exactly one class of action, so neither runs the
        // other's work on the wrong side of the write.
        if refuses(action) != (phase == Phase::Guard) {
            continue;
        }
        let victims = children_with_key(ctx, child, &fk.columns, &old_key)?;
        if victims.is_empty() {
            continue;
        }
        if phase == Phase::Guard {
            if fk.deferred {
                // The parent may come back before COMMIT. Record the CHILD's
                // key, which is what gets re-probed.
                for _ in &victims {
                    deferred.push(Deferred {
                        child: child_id,
                        fk_index: fk_ix,
                        key: old_key.clone(),
                    });
                }
                continue;
            }
            return Err(violation(&child.name, fk));
        }
        for victim in victims {
            ctx.charge_work(1, &|| format!("foreign key cascade on \"{}\"", child.name))?;
            let recurses = graph.has_incoming(&child.name);
            match action {
                FkAction::Cascade if new.is_none() => {
                    // Deleting the child is itself a parent-delete for ITS
                    // children: guard before it goes, act after.
                    let pk: Vec<Value> = child
                        .primary_key
                        .iter()
                        .map(|&c| victim[c as usize].clone())
                        .collect();
                    if recurses {
                        on_parent_change(
                            ctx, schema, graph, child_id, &victim, None, deferred,
                            Phase::Guard, depth + 1,
                        )?;
                    }
                    ctx.delete_by_pk(child_id, &pk)?;
                    if recurses {
                        on_parent_change(
                            ctx, schema, graph, child_id, &victim, None, deferred,
                            Phase::Act, depth + 1,
                        )?;
                    }
                }
                FkAction::Cascade | FkAction::SetNull | FkAction::SetDefault => {
                    let mut updated = victim.clone();
                    for (n, &c) in fk.columns.iter().enumerate() {
                        updated[c as usize] = match action {
                            FkAction::Cascade => new_key
                                .as_ref()
                                .and_then(|k| k.get(n).cloned())
                                .unwrap_or(Value::Null),
                            FkAction::SetNull => Value::Null,
                            // sqlite's SET DEFAULT is the column's DEFAULT, and
                            // NULL when it has none. A non-constant default is
                            // not reachable here (the DDL only accepts
                            // constants), so no expression evaluation is owed.
                            _ => default_of(&child.columns[c as usize]),
                        };
                    }
                    // The child's own key may itself be a parent key, so the
                    // re-keying cascades on down.
                    if recurses {
                        on_parent_change(
                            ctx, schema, graph, child_id, &victim, Some(&updated), deferred,
                            Phase::Guard, depth + 1,
                        )?;
                    }
                    ctx.update_by_pk(child_id, &updated)?;
                    if recurses {
                        on_parent_change(
                            ctx, schema, graph, child_id, &victim, Some(&updated), deferred,
                            Phase::Act, depth + 1,
                        )?;
                    }
                    // SET NULL/SET DEFAULT may have written a value that
                    // satisfies nothing — sqlite checks the result, so a
                    // `SET DEFAULT 99` with no parent 99 still fails.
                    check_child(ctx, schema, child_id, &updated, deferred)?;
                }
                FkAction::NoAction | FkAction::Restrict => unreachable!("the phase gate above"),
            }
        }
    }
    Ok(())
}

/// The child rows whose key equals `key`. Uses an index over the key columns
/// when the schema has one (the common case — an FK column is nearly always
/// indexed), and falls back to a scan, which is what sqlite does too when the
/// child side is unindexed.
fn children_with_key(
    ctx: &mut dyn TxnCtx,
    child: &TableDef,
    cols: &[u16],
    key: &[Value],
) -> Result<Vec<Vec<Value>>> {
    if child.primary_key == cols {
        return Ok(ctx.get_by_pk(child.id, key)?.into_iter().collect());
    }
    if let Some(pos) = child
        .indexes
        .iter()
        .position(|ix| ix.predicate.is_none() && ix.columns == cols)
    {
        return ctx.scan_by_index(child.id, pos as u32 + 1, key);
    }
    let rows = ctx.scan_rows_raw(child.id, None, None)?;
    ctx.charge_work(rows.len() as u64, &|| {
        format!("unindexed foreign key sweep of \"{}\"", child.name)
    })?;
    Ok(rows
        .into_iter()
        .filter(|r| key_of(r, cols).as_deref() == Some(key))
        .collect())
}

/// A column's DEFAULT as a value, or NULL when it has none. Only constants are
/// reachable — the DDL refuses a non-constant column default.
fn default_of(c: &mpedb_types::ColumnDef) -> Value {
    match &c.default {
        Some(mpedb_types::DefaultExpr::Const(v)) => v.clone(),
        _ => Value::Null,
    }
}

/// Re-probe every held-over violation. Called at COMMIT: what is still
/// dangling now is a real failure, what the transaction fixed in between is
/// not.
pub(crate) fn settle_deferred(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    pending: &[Deferred],
) -> Result<()> {
    for d in pending {
        let child = table_of(schema, d.child)?;
        let Some(fk) = child.foreign_keys.get(d.fk_index) else {
            continue;
        };
        let r = resolve(schema, child, fk)?;
        if !parent_exists(ctx, &r, &d.key)? {
            // The child row itself may since have been deleted or re-keyed, in
            // which case there is nothing dangling. Ask.
            if children_with_key(ctx, child, &fk.columns, &d.key)?.is_empty() {
                continue;
            }
            return Err(violation(&child.name, fk));
        }
    }
    Ok(())
}

/// One row of `PRAGMA foreign_key_check`: `(child table, child primary key,
/// parent table name, index of the key on the child)` — sqlite's four columns.
pub type FkCheckRow = (String, Vec<Value>, String, usize);

/// Every foreign-key violation standing in the database right now — `PRAGMA
/// foreign_key_check`.
pub(crate) fn check_all(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    only: Option<u32>,
) -> Result<Vec<FkCheckRow>> {
    let mut out = Vec::new();
    let tables: Vec<u32> = schema
        .tables
        .iter()
        .filter(|t| !t.dead && !t.foreign_keys.is_empty() && only.is_none_or(|o| o == t.id))
        .map(|t| t.id)
        .collect();
    for id in tables {
        let t = table_of(schema, id)?;
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            // A key whose parent is missing or non-unique is reported for every
            // row, exactly as an INSERT would have failed for every row.
            let r = match resolve(schema, t, fk) {
                Ok(r) => r,
                Err(_) => {
                    for row in ctx.scan_rows_raw(id, None, None)? {
                        if key_of(&row, &fk.columns).is_some() {
                            out.push((t.name.clone(), pk_of(t, &row), fk.parent.clone(), i));
                        }
                    }
                    continue;
                }
            };
            for row in ctx.scan_rows_raw(id, None, None)? {
                let Some(key) = key_of(&row, &fk.columns) else {
                    continue;
                };
                if !parent_exists(ctx, &r, &key)? {
                    out.push((t.name.clone(), pk_of(t, &row), fk.parent.clone(), i));
                }
            }
        }
    }
    Ok(out)
}

fn pk_of(t: &TableDef, row: &[Value]) -> Vec<Value> {
    t.primary_key
        .iter()
        .filter_map(|&c| row.get(c as usize).cloned())
        .collect()
}

fn table_of(schema: &Schema, id: u32) -> Result<&TableDef> {
    schema
        .tables
        .iter()
        .find(|t| t.id == id && !t.dead)
        .ok_or_else(|| Error::Internal(format!("foreign key: unknown table id {id}")))
}
