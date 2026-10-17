//! Schema programs, index/key caches, covering reads, and guard diagnostics.

use mpedb_types::{keycode, keycode::KeySpec, ColumnType, Error, ExprProgram, Result, Schema};

/// Per-column compiled CHECK programs, one entry per table (indexed like
/// `schema.tables`), one per column. Compiled by the facade (SQL layer);
/// `None` = no CHECK on that column.
pub type CheckPrograms = Vec<Vec<Option<ExprProgram>>>;

/// Per table, per secondary index (`index_no - 1`): the compiled
/// `CREATE INDEX … WHERE <pred>` predicate. `None` = a whole-table index, whose
/// membership is every row.
///
/// The truth rule is **not** CHECK's. A CHECK passes on TRUE *or* NULL; index
/// membership is a `WHERE`, so only TRUE is a member — MEASURED against sqlite
/// 3.45.1, which lets two rows share a partial-unique key when the predicate is
/// FALSE *and* when it is NULL.
pub type IndexPredicates = Vec<Vec<Option<ExprProgram>>>;

/// Everything the SQL layer has to compile out of a schema's stored SOURCE
/// strings, produced in ONE call so the two can never describe different
/// schemas: per-column CHECKs and per-index partial predicates.
/// `Clone` so the facade's seed-schema memo can hand the install-time
/// rebuild the programs the seed compile already produced (#N2 fase 5).
#[derive(Clone)]
pub struct SchemaPrograms {
    pub checks: CheckPrograms,
    pub index_predicates: IndexPredicates,
    /// Per table, per index, per KEY PART: the compiled expression for a part
    /// that is one (`IndexDef::exprs`), `None` for a plain column part. Empty
    /// outer entries mean the index has no expression parts at all.
    pub index_exprs: Vec<Vec<Vec<Option<ExprProgram>>>>,
}

/// Compiles a schema's stored SOURCE strings into [`SchemaPrograms`].
///
/// mpedb-core stores CHECK and partial-index sources but cannot parse SQL, so
/// the facade installs one of these ([`crate::engine::Engine::set_check_compiler`]) and the
/// engine calls it every time it (re)builds a bundle from the catalog. Without
/// it, a table created by `CREATE TABLE … CHECK (…)` carries its constraint in
/// the schema and gets an EMPTY program slot — a constraint that is stored,
/// reported by the catalog, and never enforced. Whether a CHECK fires must not
/// depend on whether the table came from the config or from DDL.
///
/// A missing INDEX predicate is not treated that leniently: an index whose
/// source is present but whose program is not refuses the write (see
/// `WriteTxn::index_member`). "Not compiled" would otherwise mean "every row is
/// a member", which for a UNIQUE partial index is a wrong answer — rows outside
/// the predicate would collide with each other.
pub type CheckCompiler = dyn Fn(&Schema) -> Result<SchemaPrograms> + Send + Sync;

/// How to rebuild a whole row from one secondary-index ENTRY
/// ([`SchemaBundle::covering`]): which columns the key carries, which the
/// primary key carries, and where each lands in the row.
pub struct Covering {
    /// `(column ordinal, type)` for the index's key columns, in key order.
    idx: Vec<(u16, ColumnType)>,
    /// `(column ordinal, type)` for the primary-key columns, in key order.
    pk: Vec<(u16, ColumnType)>,
    /// A unique index keys by the values alone and carries the pk in the tree
    /// VALUE; a non-unique one appends the pk to the key (`index_row_key`).
    unique: bool,
    ncols: usize,
}

impl Covering {
    /// Rebuild the row from one entry. `key` is the B+tree key, `pk_bytes` the
    /// stored value (the encoded primary key, in both layouts).
    pub fn row(&self, key: &[u8], pk_bytes: &[u8]) -> Result<Vec<mpedb_types::Value>> {
        let mut row = vec![mpedb_types::Value::Null; self.ncols];
        let mut pos = 0usize;
        for &(col, ty) in &self.idx {
            row[col as usize] = keycode::decode_value(key, &mut pos, ty)?;
        }
        // The pk comes from the key SUFFIX (non-unique) or the value (unique);
        // both are `encode_key_spec(pk_values, …)` byte-for-byte.
        let (buf, mut ppos) = if self.unique {
            (pk_bytes, 0usize)
        } else {
            (key, pos)
        };
        for &(col, ty) in &self.pk {
            row[col as usize] = keycode::decode_value(buf, &mut ppos, ty)?;
        }
        if ppos != buf.len() {
            return Err(Error::Corrupt(
                "covering index entry: trailing key bytes".into(),
            ));
        }
        Ok(row)
    }
}

/// Counters for how the optimistic guard actually behaves (#142 G1/G0).
///
/// The three non-`Overlap` verdicts are the OPT ring's documented limits, not
/// real contention, and the plan for widening the ring hangs on which one
/// dominates. Counting is what turns "should we widen it?" from an argument
/// into a measurement — the same discipline that closed #24 and #141 N4 as
/// "measured, not worth it".
#[derive(Debug, Default)]
pub struct GuardStats {
    pub cleared: std::sync::atomic::AtomicU64,
    pub overlap: std::sync::atomic::AtomicU64,
    pub snapshot_too_old: std::sync::atomic::AtomicU64,
    pub ring_gap: std::sync::atomic::AtomicU64,
    /// Bounded-feedback actions that ran out of deadline before getting a
    /// definite answer (#150). Not a guard verdict — the guard never saw them —
    /// but it belongs beside the others, because it is the same question:
    /// *why* did this action not commit. A rising count means the editor cap on
    /// some block is set too high, and it is the only place that shows.
    pub deadline_expired: std::sync::atomic::AtomicU64,
}

impl GuardStats {
    pub fn record(&self, v: crate::shm::GuardVerdict) {
        use crate::shm::GuardVerdict as G;
        use std::sync::atomic::Ordering::Relaxed;
        match v {
            G::Clear => &self.cleared,
            G::Overlap => &self.overlap,
            G::SnapshotTooOld => &self.snapshot_too_old,
            G::RingGap => &self.ring_gap,
        }
        .fetch_add(1, Relaxed);
    }

    /// Count one action that never got a definite answer inside its deadline.
    pub fn record_deadline(&self) {
        self.deadline_expired
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `(cleared, overlap, snapshot_too_old, ring_gap, deadline_expired)`.
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.cleared.load(Relaxed),
            self.overlap.load(Relaxed),
            self.snapshot_too_old.load(Relaxed),
            self.ring_gap.load(Relaxed),
            self.deadline_expired.load(Relaxed),
        )
    }
}

/// The schema plus every per-table cache derived from it, immutable as a
/// unit (#47): transactions capture ONE `Arc<SchemaBundle>` at begin, so a
/// txn sees one schema version even while DDL swaps the engine's current
/// bundle underneath. `TableDef.indexes` is the single derivation source.
pub struct SchemaBundle {
    /// The meta `schema_gen` this bundle was loaded at — compared against
    /// the committed meta at txn begin; mismatch ⇒ reload from the catalog.
    pub schema_gen: u64,
    pub schema: Schema,
    pub checks: CheckPrograms,
    /// Compiled partial-index predicates — see [`IndexPredicates`]. EMPTY when
    /// no compiler has run yet; a table/index whose schema carries a predicate
    /// SOURCE but whose slot here is absent refuses the write rather than
    /// admitting every row (`WriteTxn::index_member`).
    pub index_predicates: IndexPredicates,
    /// Compiled index KEY-PART expressions, parallel to `index_predicates`. An
    /// index whose schema carries expression sources but whose slot here is
    /// absent refuses the write, for the same reason a predicate does: a key
    /// built without its expression would be the WRONG key.
    pub index_exprs: Vec<Vec<Vec<Option<ExprProgram>>>>,
    /// Per table, per index (`index_no - 1`): the indexed column ordinals in
    /// key order.
    pub sec_indexes: Vec<Vec<Vec<u16>>>,
    /// Parallel to `sec_indexes`: is index `k` UNIQUE (values→pk, enforced)
    /// or plain non-unique (`(values ‖ pk)` key, duplicates allowed)?
    pub sec_unique: Vec<Vec<bool>>,
    /// Parallel to `sec_indexes`: index `k`'s build state.
    ///
    /// Positional like everything else about index numbering — a `Building`
    /// index KEEPS its slot, because `index_no = position + 1` is what every
    /// B-tree key and every persisted plan rests on. Shortening these arrays
    /// to "only the ready ones" would repoint all of them.
    ///
    /// Read on the write path to tell a legitimate hole (a row ahead of a
    /// build's backfill cursor) from engine corruption.
    pub sec_state: Vec<Vec<mpedb_types::IndexState>>,
    pub col_types: Vec<Vec<ColumnType>>,
    /// Per table: the [`KeySpec`] of each PRIMARY KEY column, in key order —
    /// parallel to `TableDef.primary_key`. Empty-folding when every entry is
    /// plain (the common case).
    pub pk_specs: Vec<Vec<KeySpec>>,
    /// Per table, per secondary index (`index_no - 1`): the [`KeySpec`] of each
    /// indexed column in key order — parallel to `sec_indexes`.
    pub sec_specs: Vec<Vec<Vec<KeySpec>>>,
    /// Per table: does ANY key column (a PK column or a column of any secondary
    /// index) need something other than the plain bytewise encoding — a
    /// non-`Binary` collation, or a typeless (`any`) column, which keys by
    /// STORAGE CLASS? `false` ⇒ every key builder takes the plain path and
    /// never touches the spec vectors, so an ordinary database pays nothing
    /// (and encodes byte-for-byte as before).
    pub any_key_special: Vec<bool>,
}

impl std::ops::Deref for SchemaBundle {
    type Target = Schema;
    fn deref(&self) -> &Schema {
        &self.schema
    }
}

impl SchemaBundle {
    pub fn new_at(schema_gen: u64, schema: Schema, programs: SchemaPrograms) -> SchemaBundle {
        let mut b = SchemaBundle::new_with_programs(schema, programs);
        b.schema_gen = schema_gen;
        b
    }

    /// CHECKs only, no compiled index predicates — the shape every caller that
    /// has no SQL layer to compile with uses (`Engine::open`'s seed, core
    /// tests). A partial index is then unenforceable and says so at the write,
    /// rather than admitting every row.
    pub fn new(schema: Schema, checks: CheckPrograms) -> SchemaBundle {
        SchemaBundle::new_with_programs(
            schema,
            SchemaPrograms {
                checks,
                index_predicates: Vec::new(),
                index_exprs: Vec::new(),
            },
        )
    }

    pub fn new_with_programs(schema: Schema, programs: SchemaPrograms) -> SchemaBundle {
        let SchemaPrograms {
            checks,
            index_predicates,
            index_exprs,
        } = programs;
        let sec_indexes = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.columns.clone()).collect())
            .collect();
        let sec_unique = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.unique).collect())
            .collect();
        let sec_state = schema
            .tables
            .iter()
            .map(|t| t.indexes.iter().map(|ix| ix.state).collect())
            .collect();
        let col_types = schema
            .tables
            .iter()
            .map(|t| t.columns.iter().map(|c| c.ty).collect())
            .collect();
        let spec_of = |c: &mpedb_types::ColumnDef| KeySpec::for_column(c.ty, c.collation);
        let pk_specs: Vec<Vec<KeySpec>> = schema
            .tables
            .iter()
            .map(|t| {
                t.primary_key
                    .iter()
                    .map(|&i| spec_of(&t.columns[i as usize]))
                    .collect()
            })
            .collect();
        let sec_specs: Vec<Vec<Vec<KeySpec>>> = schema
            .tables
            .iter()
            .map(|t| {
                t.indexes
                    .iter()
                    .map(|ix| {
                        ix.columns
                            .iter()
                            .enumerate()
                            .map(|(k, &i)| {
                                // An EXPRESSION part has no column to take a
                                // collation or type from — its value is computed.
                                // The plain spec is what an untyped computed
                                // value keys by, which is also what the
                                // synthetic-row builder in `index_entry_key`
                                // hands over.
                                let base = t
                                    .columns
                                    .get(i as usize)
                                    .map_or_else(KeySpec::default, spec_of);
                                // A per-part `COLLATE` (v14) overrides the
                                // COLUMN's collation for this index's key only.
                                match ix.collations.get(k).copied().flatten() {
                                    None => base,
                                    Some(c) => t
                                        .columns
                                        .get(i as usize)
                                        .map_or_else(KeySpec::default, |cd| {
                                            KeySpec::for_column(cd.ty, c)
                                        }),
                                }
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let any_key_special: Vec<bool> = pk_specs
            .iter()
            .zip(&sec_specs)
            .map(|(pk, sec)| pk.iter().chain(sec.iter().flatten()).any(|s| !s.is_plain()))
            .collect();
        SchemaBundle {
            schema_gen: 0,
            schema,
            checks,
            index_predicates,
            index_exprs,
            sec_indexes,
            sec_unique,
            sec_state,
            col_types,
            pk_specs,
            sec_specs,
            any_key_special,
        }
    }

    /// Key specs for the PK columns of `table_id`, in key order — or `&[]` when
    /// every key column of the table is plain, so the caller uses the bytewise
    /// encoder. `encode_key_spec(v, &[])` == `encode_key(v)`.
    #[inline]
    /// A COVERING read plan for secondary index `index_no`, or `None` when the
    /// entry cannot rebuild the row.
    ///
    /// A secondary entry already carries every indexed column (in its key) and
    /// the row's primary key (key suffix for a non-unique index, tree value for
    /// a unique one). When those together are ALL of the table's columns, the
    /// row can be decoded from the entry — no descent into the PK tree, no row
    /// decode. Same rows, same order, same membership: this changes only HOW
    /// the row is produced, so no plan, no access path and no caller changes.
    ///
    /// Declines (fail-closed) unless every piece round-trips exactly:
    /// - any COLLATED key column — a NOCASE/RTRIM index stores the FOLDED key,
    ///   so the original value is not in there to recover (`any_key_special`);
    /// - any `ColumnType::Any` column — its key form is class-encoded and a
    ///   decode would have to guess the class back onto a typeless column;
    /// - a table whose columns are not exactly (index ∪ pk).
    pub fn covering(&self, table_id: u32, index_no: u32) -> Option<Covering> {
        if index_no == 0
            || self
                .any_key_special
                .get(table_id as usize)
                .copied()
                .unwrap_or(false)
        {
            return None;
        }
        let t = table_id as usize;
        let idx_cols = self.sec_indexes.get(t)?.get(index_no as usize - 1)?;
        let unique = *self.sec_unique.get(t)?.get(index_no as usize - 1)?;
        let table = self.schema.table(table_id)?;
        let types = self.col_types.get(t)?;
        let pk_cols = &table.primary_key;
        // Every column present exactly once across (index ‖ pk), and none of
        // them typeless.
        let mut seen = vec![false; types.len()];
        for &c in idx_cols.iter().chain(pk_cols.iter()) {
            let i = c as usize;
            if i >= types.len() || types[i] == mpedb_types::ColumnType::Any || seen[i] {
                return None;
            }
            seen[i] = true;
        }
        if !seen.iter().all(|&b| b) {
            return None;
        }
        Some(Covering {
            idx: idx_cols.iter().map(|&c| (c, types[c as usize])).collect(),
            pk: pk_cols.iter().map(|&c| (c, types[c as usize])).collect(),
            unique,
            ncols: types.len(),
        })
    }

    /// The declared types of the key columns of `index_no` on `table_id`, in
    /// key order — `index_no = 0` is the primary key.
    ///
    /// A caller that ENCODES a key bound needs these: the tree is keyed by the
    /// column's own domain, and a bound in some other domain does not order
    /// against it. `Text("59.05")` and `Float(59.05)` are eight different
    /// bytes behind the same tag in `keycode`, so a text bound over a REAL
    /// index sorts above every row and the scan matches nothing.
    pub fn key_col_types(&self, table_id: u32, index_no: u32) -> Option<Vec<ColumnType>> {
        let t = table_id as usize;
        let types = self.col_types.get(t)?;
        let cols: &[u16] = if index_no == 0 {
            &self.schema.table(table_id)?.primary_key
        } else {
            self.sec_indexes.get(t)?.get(index_no as usize - 1)?
        };
        cols.iter()
            .map(|c| types.get(*c as usize).copied())
            .collect()
    }

    pub fn pk_coll(&self, table_id: u32) -> &[KeySpec] {
        let t = table_id as usize;
        if self.any_key_special.get(t).copied().unwrap_or(false) {
            &self.pk_specs[t]
        } else {
            &[]
        }
    }

    /// Key specs for secondary index `index_no` (`>= 1`) of `table_id`, in key
    /// order — or `&[]` when every key column of the table is plain.
    #[inline]
    pub fn index_coll(&self, table_id: u32, index_no: u32) -> &[KeySpec] {
        let t = table_id as usize;
        if index_no >= 1 && self.any_key_special.get(t).copied().unwrap_or(false) {
            self.sec_specs
                .get(t)
                .and_then(|v| v.get(index_no as usize - 1))
                .map(Vec::as_slice)
                .unwrap_or(&[])
        } else {
            &[]
        }
    }
}
