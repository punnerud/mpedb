use super::*;

/// The declared collation of every slot in the BASE (or joined) row being
/// scanned — the concatenation of the scanned tables' column collations, in slot
/// order. GROUP BY and DISTINCT fold their keys through this so a `NOCASE`/`RTRIM`
/// column groups/deduplicates case-/space-insensitively (the collation is baked
/// into the schema, so this is derived at execution and always agrees with the
/// plan's `schema_hash`). The working-table sentinel (`CTE_TABLE`) resolves
/// through the plan's own node and contributes one BINARY slot per column —
/// PADDED, not skipped, so a joined table's collations stay aligned with the
/// joined row (skipping used to shift a collated join column onto the wrong
/// slot the day a working table joined a `NOCASE` table).
pub(super) fn base_row_collations(
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    joins: &[Join],
) -> Vec<Collation> {
    let mut out = Vec::new();
    for id in std::iter::once(table).chain(joins.iter().map(|j| j.table)) {
        if let Ok(t) = table_def(schema, plan, id) {
            out.extend(t.columns.iter().map(|c| c.collation));
        }
    }
    out
}

/// The declared collation of each PROJECTED output column: a bare column
/// (`Projection::Column`) carries its declared collation; a computed column has
/// none (BINARY), exactly as in sqlite. Used to fold `SELECT DISTINCT` keys.
pub(super) fn output_collations(
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    joins: &[Join],
    projection: &[Projection],
) -> Vec<Collation> {
    let base = base_row_collations(schema, plan, table, joins);
    projection
        .iter()
        .map(|p| match p {
            Projection::Column(i) => base.get(*i as usize).copied().unwrap_or(Collation::Binary),
            Projection::Expr { .. } | Projection::SetReturning { .. } => Collation::Binary,
        })
        .collect()
}

/// The row operations the executor needs, implemented by both transaction
/// kinds. Write operations on a read transaction are unreachable by
/// construction (routing is by the recomputed `footprint.read_only`) and
/// return `Error::Internal` if ever hit.
/// Which shape of index walk [`TxnCtx::scan_by_index_capped`] performs.
///
/// An equality probe and a bounded range are mutually exclusive. As three
/// separate parameters they were not: `prefix` alongside `lo`/`hi` was
/// expressible and meant nothing, and it pushed the signature past what any
/// reader can hold at once.
pub(crate) enum IndexProbe<'a> {
    /// Every entry whose key starts with `encode_key_spec(values, …)` — the
    /// `WHERE col = v` probe. A NULL among the values matches nothing: a row
    /// with a NULL in an indexed column has no entry at all.
    Prefix(&'a [Value]),
    /// Raw-encoded bounds, the same prefix construction composite-PK ranges
    /// use.
    Range { lo: Option<(&'a [u8], bool)>, hi: Option<(&'a [u8], bool)> },
}

pub(crate) trait TxnCtx {
    /// Host-registered scalar UDFs in scope for this execution (design/DESIGN-UDF.md),
    /// or `None` where none are (the default). Both native contexts carry them —
    /// [`ReadCtx`] on the read path and [`WriteCtx`] on the write path — so a UDF
    /// called from DML, or from a read inside an open write transaction, resolves
    /// the same closure the read path would. A context that structurally cannot
    /// carry them (the streaming read path, the sqlite-backed contexts) keeps the
    /// `None` default, and the eval site then refuses with a clean "not in scope"
    /// error rather than silently dropping the call. Every eval site threads this
    /// through [`ExprProgram::eval_filter_host`]/`eval_host`.
    fn host_fns(&self) -> Option<&dyn HostFns> {
        None
    }
    /// Host-registered AGGREGATES in scope for this execution
    /// (design/DESIGN-UDF.md stage 2), or `None`. Same scope rule as
    /// [`host_fns`](Self::host_fns): both native contexts carry them, everything
    /// else refuses cleanly.
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        None
    }
    /// Host-registered COLLATING SEQUENCES in scope for this execution
    /// (design/DESIGN-UDF.md stage 3), or `None`. Same scope rule as
    /// [`host_fns`](Self::host_fns); a plan whose ORDER BY names one is
    /// connection-local, so every other context refuses it by name rather than
    /// sorting under a collation it does not have.
    fn host_colls(&self) -> Option<&dyn HostColls> {
        None
    }
    /// The pinned snapshot under this context, when this context IS a plain
    /// snapshot read — the parallel fold's precondition (`exec/parallel.rs`).
    /// Its workers share the returned transaction's pin, meter and page
    /// access, so only a context that is nothing BUT a `ReadTxn` may answer.
    /// Everything else (write contexts, overlay and streaming reads, the
    /// sqlite-backed contexts) keeps the `None` default and folds serially.
    fn snapshot_txn(&self) -> Option<&ReadTxn<'_>> {
        None
    }

    /// The declared types of `index_no`'s key columns (`0` = the primary key),
    /// for a caller that is about to ENCODE a key bound.
    ///
    /// `None` means "cannot say", and the caller must then encode the value as
    /// it stands — the behaviour before this existed. It is not a safe answer,
    /// only an honest one: a context that wraps another should delegate rather
    /// than accept the default.
    fn key_col_types(&self, _table: u32, _index_no: u32) -> Option<Vec<mpedb_types::ColumnType>> {
        self.snapshot_txn().and_then(|t| t.key_col_types(_table, _index_no))
    }
    /// The PKs of existing rows the proposed `row` collides with on a
    /// SECONDARY UNIQUE constraint — `INSERT OR REPLACE`'s victims beyond the
    /// PK's own (#169).
    ///
    /// A method rather than a loop at the call site because WHERE the unique
    /// constraints live is a property of the store, not of the statement. The
    /// native one keeps them in the schema as unique `IndexDef`s and probes
    /// them; the sqlite-backed ones deliberately do NOT (attach keeps the
    /// base's UNIQUE off the schema, since carrying it would let the planner
    /// choose index access paths this reader cannot serve — #133), so they
    /// answer from their own constraint list instead.
    ///
    /// The default IS the native behaviour, unchanged: probe every unique
    /// index, skipping any whose key has a NULL (no index entry, so no
    /// conflict — UNIQUE and the rowid-alias auto-assign both permit it).
    fn unique_victims(
        &mut self,
        table: u32,
        t: &mpedb_types::TableDef,
        row: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        for (pos, ix) in t.indexes.iter().enumerate() {
            if !ix.unique {
                continue;
            }
            let vals: Vec<Value> = ix.columns.iter().map(|&c| row[c as usize].clone()).collect();
            if vals.iter().any(|v| v.is_null()) {
                continue;
            }
            if let Some(existing) = self.get_by_index(table, (pos + 1) as u32, &vals)? {
                out.push(t.primary_key.iter().map(|&c| existing[c as usize].clone()).collect());
            }
        }
        Ok(out)
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>>;
    /// Decode only the listed column ordinals from a PK hit (projection order).
    /// Default: full row then project — override when the store can decode
    /// individual columns without materializing the rest.
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        match self.get_by_pk(table, pk)? {
            None => Ok(None),
            Some(row) => {
                let mut out = Vec::with_capacity(cols.len());
                for &c in cols {
                    out.push(
                        row.get(c as usize)
                            .cloned()
                            .ok_or_else(|| internal("projection column"))?,
                    );
                }
                Ok(Some(out))
            }
        }
    }
    fn get_by_index(&mut self, table: u32, index_no: u32, values: &[Value])
        -> Result<Option<Vec<Value>>>;
    /// Every row matching an index equality — N rows for a non-unique index,
    /// 0 or 1 for a unique one (the engine takes an exact-get fast path for
    /// those, so routing everything through here costs the unique case
    /// nothing).
    fn scan_by_index(&mut self, table: u32, index_no: u32, values: &[Value])
        -> Result<Vec<Vec<Value>>>;
    /// Every row whose indexed value falls in the raw-encoded bound range —
    /// `AccessPath::IndexRange`. Bounds use the same prefix construction as
    /// composite-PK ranges (see [`range_bounds`]).
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>>;
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>>;
    /// The index paths' [`scan_rows_capped`](Self::scan_rows_capped): the
    /// residual filter per row and a cap on KEPT rows, both applied INSIDE the
    /// walk.
    ///
    /// The materializing forms above build the whole matching range first, so
    /// a probe that matches a third of the table paid for a third of the table
    /// to return one row (measured: 528 MB peak for `WHERE hex = 'x' LIMIT 1`
    /// on 945 234 rows, against 7,5 MB for the same one row by table scan).
    /// The filter has to come down WITH the cap, not after it — capping at `n`
    /// rows the filter has not seen yet would stop early and return fewer rows
    /// than exist.
    ///
    /// `prefix` selects the shape: `Some(values)` is the equality probe,
    /// `None` the `lo`/`hi` range. The default collects and then filters,
    /// which is right for the WriteTxn contexts (collect-then-mutate is their
    /// rule anyway); `ReadCtx` overrides it with the real cursor.
    fn scan_by_index_capped(
        &mut self,
        table: u32,
        index_no: u32,
        probe: IndexProbe<'_>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let rows = match probe {
            IndexProbe::Prefix(v) => self.scan_by_index(table, index_no, v)?,
            IndexProbe::Range { lo, hi } => self.scan_by_index_range(table, index_no, lo, hi)?,
        };
        let host = self.host_fns();
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }

    /// Scan with the residual filter applied per row and an optional cap on
    /// KEPT rows — the LIMIT/OFFSET pushdown (MPEE "stream under a memory
    /// budget" transfer: never materialize what the query will not return).
    /// The default collects the whole range first (used by WriteTxn contexts,
    /// where collect-then-mutate is the rule anyway); ReadCtx overrides it
    /// with true cursor streaming, which is the autocommit SELECT path.
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let host = self.host_fns();
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    /// Streaming top-K for `ORDER BY … LIMIT`: return the `keep` smallest
    /// rows under `order_by` (already sorted), scanning under a bounded
    /// `keep`-sized heap so memory is O(keep) instead of O(matched rows) —
    /// the MPEE "stream under a memory budget" transfer applied to sorted
    /// pagination. The default materializes the whole range then sorts and
    /// truncates (used by WriteTxn contexts); ReadCtx overrides it with a
    /// true streaming heap.
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        gather::check_order_colls(order_by, self.host_colls())?;
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let host = self.host_fns();
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if ok {
                kept.push(row);
            }
        }
        sort_rows(&mut kept, order_by, self.host_colls());
        kept.truncate(keep);
        Ok(kept)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()>;
    /// The next value to auto-assign to an INTEGER PRIMARY KEY rowid alias
    /// (`pk_col` is that column's index): `max(existing pk) + 1`, or 1 for an
    /// empty table — sqlite's plain, non-AUTOINCREMENT rule (a freed top id is
    /// reusable). The default scans the table and takes the maximum, which is
    /// correct for any backing store; `WriteTxn` overrides it with an
    /// O(tree-height) rightmost-key descent.
    fn next_rowid(&mut self, table: u32, pk_col: u16) -> Result<i64> {
        let rows = self.scan_rows_raw(table, None, None)?;
        let mut max: Option<i64> = None;
        for row in &rows {
            if let Some(Value::Int(v)) = row.get(pk_col as usize) {
                max = Some(max.map_or(*v, |m: i64| m.max(*v)));
            }
        }
        Ok(max.map_or(1, |m| m.saturating_add(1)))
    }
    /// An INSERT that SUPPLIES the rowid-alias value still moves an
    /// AUTOINCREMENT table's high-water (sqlite records `max(seq-or-0, id)` in
    /// `sqlite_sequence` on every assigned id, explicit included). Default:
    /// no-op — the same contexts whose `next_rowid` default applies the plain
    /// non-AUTOINCREMENT rule have no high-water to move; the engine contexts
    /// override with [`WriteTxn::note_rowid`].
    fn note_rowid(&mut self, table: u32, id: i64) -> Result<()> {
        let _ = (table, id);
        Ok(())
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool>;
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool>;
    /// Every posting entry whose key starts with `prefix`, as `(key, doclist)`
    /// pairs in key order — the FTS set-algebra primitive (design/DESIGN-FTS.md
    /// §4). Charges the #74 work meter per entry visited. The default errors:
    /// only the native engine contexts (`WriteTxn`, `ReadCtx`) serve FTS; the
    /// sqlite-backed contexts have no inverted index.
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let _ = (table, prefix);
        Err(mpedb_types::Error::Unsupported(
            "full-text search is not available in this context".into(),
        ))
    }
    /// Charge `n` work-rows against this execution's deterministic budget (#74)
    /// and surface [`Error::RuntimeBudget`] once it is exceeded. Routes to the
    /// SAME [`mpedb_core::WorkMeter`] the engine's scans charge, so the
    /// exec-layer bumps (nested-loop join, correlated subquery) and the scan
    /// bumps share one running count. `which` builds the attribution lazily —
    /// evaluated only on the abort path. Object-safe: `&dyn Fn`, not a generic.
    ///
    /// The default is a no-op: the sqlite-backed contexts (`SqliteCtx`,
    /// `MergeCtx`) are a different storage engine with no mpedb `WorkMeter`, so
    /// the #74 budget applies only to the native engine paths that override this
    /// (`ReadCtx`, `WriteTxn`).
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        let _ = (n, which);
        Ok(())
    }
    /// The live-cell budget for join materialization (`0` = unlimited): the
    /// nested-loop join in `gather::gather_joined` bounds the `Value` cells
    /// its intermediate product HOLDS against this — the memory-proportional
    /// twin of the work-row budget, which only bounds what a query READS.
    /// Default `0` for the sqlite-backed contexts (a different storage engine;
    /// their joins run through the same gather, but the mpedb config does not
    /// govern them — mirrors [`charge_work`](Self::charge_work)'s scoping).
    fn join_cells_budget(&self) -> u64 {
        0
    }
    /// Does [`scan_rows_capped`](Self::scan_rows_capped) STOP PULLING at the
    /// cap — i.e. is this context's scan a real cursor rather than a
    /// materialize-then-truncate?
    ///
    /// The distinction is what makes a **resumable batched scan**
    /// ([`gather::BatchScan`], #123 §5.1) worth doing. A cursor context answers
    /// a `cap = C` scan in O(C); the default `TxnCtx::scan_rows_capped`
    /// materializes the whole range and only then truncates, so batching over
    /// it would be O(n) PER BATCH — quadratic, and holding exactly what the
    /// batching exists to avoid. So the default is `false` and every context
    /// that has not proven otherwise keeps the single-pass materializing path
    /// it has today, with byte-identical results either way.
    ///
    /// Only [`ReadCtx`] overrides it: its `scan_rows_capped` breaks out of a
    /// live `RowCursor`, and its scans are keyed by the same memcmp PK the
    /// resume bound is encoded with.
    fn scans_incrementally(&self) -> bool {
        false
    }
    /// One batch of a resumable, DECODE-PRUNED scan — the scan-level half of
    /// #125: up to `cap` KEPT rows, each decoded only at the `keep`-true
    /// ordinals and truncated to `keep.len()` slots (holes read as NULL, the
    /// exact shape `gather::narrow_row` produces), plus the raw storage key
    /// of the last kept row when the cap was reached — the next batch's
    /// resume bound, obtained without re-encoding a PK.
    ///
    /// **`keep` must cover every column `filter` reads**: unlike
    /// [`scan_rows_capped`](Self::scan_rows_capped), the residual runs over
    /// the pruned row here. [`gather::scan_keep`] builds exactly that mask.
    ///
    /// Meaningful only where [`scans_incrementally`](Self::scans_incrementally)
    /// answers true — [`gather::BatchScan`], the sole caller, never opens
    /// elsewhere — so the default is a refusal, not a fallback.
    fn scan_rows_pruned(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: usize,
        keep: Option<&[bool]>,
    ) -> Result<PrunedBatch> {
        let _ = (table, lo, hi, filter, cap, keep);
        Err(internal("decode-pruned scan on a non-incremental context"))
    }
    /// `count(*)` over a raw-bounded PK range without materializing a row, or
    /// `Ok(None)` when this context has no such fast path and the caller must
    /// fold the scan. The #74 work charges of the counting context must be
    /// EXACTLY the drain-scan's — same total, same trip point, same label —
    /// because the budget is a deterministic, test-pinned contract and this
    /// is an optimization, not a discount ([`mpedb_core::WorkMeter`]'s
    /// `charge_many` states the same rule from the meter's side).
    fn count_rows_range(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<u64>> {
        let _ = (table, lo, hi);
        Ok(None)
    }
    /// Can this context serve an aggregate-over-index-tree plan (format 59) —
    /// count entries wholesale, fold leading key values, probe boundary rows?
    /// Default `false`: every non-snapshot context (write transactions, the
    /// sqlite-backed overlays, mirrors) keeps the row fold, which remains the
    /// semantics of record — the plan's `over_index` is then an access
    /// decision those contexts decline, not an obligation.
    fn agg_over_index_supported(&self) -> bool {
        false
    }
    /// Entry count of a secondary index tree, leaf-wholesale (#74 charges one
    /// work-row per entry — see `ReadTxn::count_index_entries`). Only called
    /// where [`agg_over_index_supported`](Self::agg_over_index_supported) is
    /// true; the default is therefore a refusal, not a fallback.
    fn count_index_entries(&mut self, table: u32, index_no: u32) -> Result<u64> {
        let _ = (table, index_no);
        Err(internal("index-entry count on an unsupported context"))
    }
    /// Visit every entry's decoded LEADING key value in key order (#74: one
    /// work-row per entry). Same support gate as above.
    fn fold_index_leading(
        &mut self,
        table: u32,
        index_no: u32,
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let _ = (table, index_no, f);
        Err(internal("index-leading fold on an unsupported context"))
    }
    /// The row behind the index tree's min (`max = false`) or max boundary
    /// entry, `None` for an empty tree (#74: one work-row per found row).
    /// Same support gate as above.
    fn index_boundary_row(
        &mut self,
        table: u32,
        index_no: u32,
        max: bool,
    ) -> Result<Option<Vec<Value>>> {
        let _ = (table, index_no, max);
        Err(internal("index boundary probe on an unsupported context"))
    }
    /// Fold ONE decoded column of every row in a raw-bounded PK range into
    /// `f`, in scan (PK) order, without materializing a row — the
    /// decode-to-accumulator fusion of the ungrouped single-column aggregate
    /// (`ReadTxn::fold_range_column`). `Ok(false)` when this context has no
    /// spine-free path; the caller then runs the batched fold, which stays
    /// the semantics of record. Only the pinned-snapshot read context answers
    /// `true`, and its #74 charges are EXACTLY the drain-scan's.
    fn fold_rows_column(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        col: u16,
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let _ = (table, lo, hi, col, opts, f);
        Ok(None)
    }

    /// How many rows does this table hold? `Ok(None)` = this context cannot
    /// say cheaply, and the caller must not depend on knowing.
    fn row_count(&mut self, table: u32) -> Result<Option<u64>> {
        let _ = table;
        Ok(None)
    }

    /// Selectivity-priced index range: `Ok(None)` = this context has no such
    /// path (or the shape declines) and the caller runs the plain range scan.
    /// Only the pinned-snapshot read context answers.
    fn scan_by_index_range_adaptive(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let _ = (table, index_no, lo, hi);
        Ok(None)
    }

    /// [`fold_rows_column`](Self::fold_rows_column) with a PREDICATE: decode
    /// `need` (the filter's columns plus the aggregate's, from
    /// `ExprProgram::read_columns`) into one reused buffer, evaluate the
    /// filter, and fold `col` only for rows that pass — so a filtered
    /// aggregate over a wide table decodes two columns per row instead of
    /// materializing the whole row. `Ok(None)` = this context has no such
    /// path, and the caller runs the ordinary gather.
    #[allow(clippy::too_many_arguments)]
    fn fold_rows_column_filtered(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        need: &[u16],
        col: u16,
        filter: (&ExprProgram, &[Value]),
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let _ = (table, lo, hi, need, col, filter, opts, f);
        Ok(None)
    }
}

impl TxnCtx for WriteTxn<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self, table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk_cols(self, table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_index(self, table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index(self, table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index_range(self, table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_rows_raw(self, table, lo, hi)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        WriteTxn::insert_row(self, table, values)
    }
    fn next_rowid(&mut self, table: u32, _pk_col: u16) -> Result<i64> {
        // The PK tree key IS the single integer PK, so the rightmost key is the
        // maximum — no need to read `pk_col` out of a full row.
        WriteTxn::next_rowid(self, table)
    }
    fn note_rowid(&mut self, table: u32, id: i64) -> Result<()> {
        WriteTxn::note_rowid(self, table, id)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        WriteTxn::update_by_pk(self, table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        WriteTxn::delete_by_pk(self, table, pk)
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        WriteTxn::fts_prefix(self, table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        WriteTxn::charge_work(self, n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        WriteTxn::join_cells_budget(self)
    }
}

/// A [`WriteTxn`] plus the connection's host-UDF closures — the WRITE-path twin
/// of [`ReadCtx`] (design/DESIGN-UDF.md).
///
/// `impl TxnCtx for WriteTxn` cannot carry them (the type lives in
/// `mpedb-core`, which knows nothing about a connection's UDF registry), so the
/// facade wraps the transaction for the duration of ONE statement whose plan
/// `contains_host_call()`. Every row operation delegates to the transaction
/// unchanged — the wrapper adds resolution, never behaviour: a statement with no
/// host call still runs on the bare `&mut WriteTxn`, byte for byte as before.
///
/// The closures reach the executor by value-passing only: `HostFns::call` gets
/// the already-evaluated argument `Value`s and returns one `Value`, and
/// `HostAggs::create` mints a state stepped with the same. Neither is handed the
/// transaction, the snapshot, the schema, or any engine handle, so a host UDF on
/// the write path sees exactly what it sees on the read path — its arguments.
pub(crate) struct WriteCtx<'a, 'e> {
    pub txn: &'a mut WriteTxn<'e>,
    pub host: Option<&'a dyn HostFns>,
    pub aggs: Option<&'a dyn mpedb_types::HostAggs>,
    /// Host COLLATING SEQUENCES in scope for this write (stage 3), so an
    /// `ORDER BY … COLLATE mycoll` inside DML (`INSERT … SELECT`, `RETURNING`)
    /// sorts through the same callbacks a read would.
    pub colls: Option<&'a dyn HostColls>,
}

impl<'a, 'e> WriteCtx<'a, 'e> {
    pub(crate) fn new(
        txn: &'a mut WriteTxn<'e>,
        host: Option<&'a dyn HostFns>,
        aggs: Option<&'a dyn mpedb_types::HostAggs>,
        colls: Option<&'a dyn HostColls>,
    ) -> WriteCtx<'a, 'e> {
        WriteCtx { txn, host, aggs, colls }
    }
}

impl TxnCtx for WriteCtx<'_, '_> {
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.host
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.aggs
    }
    fn host_colls(&self) -> Option<&dyn HostColls> {
        self.colls
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self.txn, table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk_cols(self.txn, table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_index(self.txn, table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index(self.txn, table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index_range(self.txn, table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_rows_raw(self.txn, table, lo, hi)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        WriteTxn::insert_row(self.txn, table, values)
    }
    fn next_rowid(&mut self, table: u32, _pk_col: u16) -> Result<i64> {
        WriteTxn::next_rowid(self.txn, table)
    }
    fn note_rowid(&mut self, table: u32, id: i64) -> Result<()> {
        WriteTxn::note_rowid(self.txn, table, id)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        WriteTxn::update_by_pk(self.txn, table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        WriteTxn::delete_by_pk(self.txn, table, pk)
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        WriteTxn::fts_prefix(self.txn, table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        WriteTxn::charge_work(self.txn, n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        WriteTxn::join_cells_budget(self.txn)
    }
}

/// One pruned batch ([`TxnCtx::scan_rows_pruned`]): the kept rows and, when
/// the cap was reached, the raw storage key of the last kept row — the
/// resume bound of the next batch.
pub(crate) type PrunedBatch = (Vec<Vec<Value>>, Option<Vec<u8>>);

/// How a [`ReadCtx`]'s scans charge the #74 work meter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChargeMode {
    /// One work-row per row, charged before its decode — the serial contract,
    /// and every context's answer but a parallel fold worker's.
    PerRow,
    /// In batches — see [`mpedb_core::RowCursor::batch_charges`]. The workers
    /// of one statement share ONE atomic meter cell, and a per-row
    /// read-modify-write on it measured 1.4× SLOWER than serial on 11 cores.
    /// Sound only because a worker's every error abandons the attempt to a
    /// serial re-run that owns the authentic refusal.
    Batched,
}

/// Adapter over a pinned read snapshot.
pub(crate) struct ReadCtx<'t, 'e>(
    pub &'t ReadTxn<'e>,
    /// Host-registered scalar UDFs in scope for this read (design/DESIGN-UDF.md),
    /// or `None`. Set by [`crate::Database`] only for a plan that
    /// `contains_host_call`; the streaming and sqlite-overlay read paths pass
    /// `None` (host UDFs there are out of scope for stage 1).
    pub Option<&'t dyn HostFns>,
    /// Host-registered AGGREGATE factories in scope for this read (stage 2),
    /// gated by the same `contains_host_call` test as the scalars above.
    pub Option<&'t dyn mpedb_types::HostAggs>,
    /// Host-registered COLLATING SEQUENCES in scope for this read (stage 3),
    /// gated by the same `contains_host_call` test as the two above.
    pub Option<&'t dyn HostColls>,
    /// #74 charging discipline — [`ChargeMode::PerRow`] everywhere but inside
    /// a parallel fold worker.
    pub ChargeMode,
);

impl TxnCtx for ReadCtx<'_, '_> {
    fn snapshot_txn(&self) -> Option<&ReadTxn<'_>> {
        Some(self.0)
    }
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.1
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.2
    }
    fn host_colls(&self) -> Option<&dyn HostColls> {
        self.3
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk(table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk_cols(table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        self.0.get_by_index(table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        self.0.scan_by_index(table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        self.0.scan_by_index_range(table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut out = Vec::new();
        while let Some(row) = cursor.next()? {
            out.push(row);
        }
        Ok(out)
    }
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        // true streaming: stop pulling from the B+tree cursor the moment the
        // cap is reached — `SELECT ... LIMIT k` does O(offset+k) work
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        while let Some(row) = cursor.next()? {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    fn scan_by_index_capped(
        &mut self,
        table: u32,
        index_no: u32,
        probe: IndexProbe<'_>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        // The index twin of `scan_rows_capped`: stop walking the index the
        // moment the cap is met, and never build a row the filter rejects.
        let host = self.1;
        let mut cursor = match probe {
            IndexProbe::Prefix(vals) => {
                if vals.iter().any(|v| v.is_null()) {
                    return Ok(Vec::new()); // any-NULL rows are never indexed
                }
                self.0.scan_by_index_prefix_raw(table, index_no, vals)?
            }
            IndexProbe::Range { lo, hi } => {
                self.0.scan_by_index_range_raw(table, index_no, lo, hi)?
            }
        };
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        while let Some(row) = cursor.next()? {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    // A real B+tree cursor: the `scan_rows_capped` above stops pulling the
    // moment the cap is reached, which is the precondition a resumable
    // batched scan needs (see the trait default).
    fn scans_incrementally(&self) -> bool {
        true
    }
    fn scan_rows_pruned(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: usize,
        keep: Option<&[bool]>,
    ) -> Result<PrunedBatch> {
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        // The raw key of the row most recently yielded, written into a
        // reused buffer — when the loop breaks at the cap this holds the last
        // KEPT row's key, which is the batch's resume bound.
        let mut key_buf = Vec::new();
        if self.4 == ChargeMode::Batched {
            cursor.batch_charges(64);
        }
        while let Some(row) = cursor.next_masked(keep, Some(&mut key_buf))? {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if ok {
                kept.push(row);
                if kept.len() >= cap {
                    // A batching cursor is abandoned here, mid-range: its
                    // unflushed rows must reach the meter before it dies.
                    cursor.flush_charges()?;
                    return Ok((kept, Some(std::mem::take(&mut key_buf))));
                }
            }
        }
        cursor.flush_charges()?;
        // Exhausted short of the cap: no resume needed, and none is claimed.
        Ok((kept, None))
    }
    fn count_rows_range(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<u64>> {
        self.0.count_range(table, lo, hi).map(Some)
    }
    // The pinned-snapshot context is the one that owns real index trees, so it
    // is the one that serves the aggregate-over-index paths (format 59).
    fn agg_over_index_supported(&self) -> bool {
        true
    }
    fn count_index_entries(&mut self, table: u32, index_no: u32) -> Result<u64> {
        self.0.count_index_entries(table, index_no)
    }
    fn fold_index_leading(
        &mut self,
        table: u32,
        index_no: u32,
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        self.0.fold_index_leading(table, index_no, f)
    }
    fn index_boundary_row(
        &mut self,
        table: u32,
        index_no: u32,
        max: bool,
    ) -> Result<Option<Vec<Value>>> {
        self.0.index_boundary_row(table, index_no, max)
    }
    fn scan_by_index_range_adaptive(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        self.0.scan_by_index_range_adaptive(table, index_no, lo, hi)
    }

    fn row_count(&mut self, table: u32) -> Result<Option<u64>> {
        self.0.row_count(table).map(Some)
    }

    fn fold_rows_column(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        col: u16,
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        self.0.fold_range_column(table, lo, hi, col, opts, f).map(Some)
    }

    fn fold_rows_column_filtered(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        need: &[u16],
        col: u16,
        filter: (&ExprProgram, &[Value]),
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let host = self.1;
        let (prog, params) = filter;
        let mut stack = Vec::new();
        self.0
            .fold_range_columns(table, lo, hi, need, opts, &mut |buf| {
                if prog.eval_filter_host(&mut stack, buf, params, host)? {
                    f(&buf[col as usize])?;
                }
                Ok(())
            })
            .map(Some)
    }
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        gather::check_order_colls(order_by, self.host_colls())?;
        if keep == 0 {
            return Ok(Vec::new());
        }
        // Bounded max-heap of the `keep` smallest rows seen so far: the heap's
        // top is the *worst* kept row, so a newcomer that sorts before it
        // evicts it. Never more than `keep` rows are held, regardless of how
        // many the scan yields.
        // `keep` is runtime data (`LIMIT ?` binds it): it caps what the heap
        // HOLDS, never what gets preallocated — `LIMIT ?` bound to i64::MAX
        // must not size a 9e18-slot allocation (capacity overflow = panic).
        let mut heap: BinaryHeap<Ranked<'_>> =
            BinaryHeap::with_capacity(keep.saturating_add(1).min(65_536));
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut stack = Vec::new();
        // Scan sequence = PK order; used as a stable tiebreaker so equal
        // ORDER BY keys come out exactly as the engine's stable `sort_rows`
        // would order them (scan/PK order), matching the non-top-K path.
        let mut seq: u64 = 0;
        while let Some(row) = cursor.next()? {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if !ok {
                continue;
            }
            let cand = Ranked { row, order_by, colls: self.3, seq };
            seq += 1;
            if heap.len() < keep {
                heap.push(cand);
            } else if cand < *heap.peek().expect("keep >= 1") {
                heap.pop();
                heap.push(cand);
            }
        }
        Ok(heap.into_sorted_vec().into_iter().map(|r| r.row).collect())
    }
    fn insert_row(&mut self, _table: u32, _values: &[Value]) -> Result<()> {
        Err(read_txn_write_bug())
    }
    fn update_by_pk(&mut self, _table: u32, _new_values: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
    fn delete_by_pk(&mut self, _table: u32, _pk: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.0.fts_prefix(table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        self.0.charge_work(n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        self.0.join_cells_budget()
    }
}

/// A row wrapped with its `ORDER BY` spec so a [`BinaryHeap`] (max-heap)
/// keeps the smallest rows: `Ord` follows the sort order, so the heap's max
/// is the row that sorts *last*.
struct Ranked<'a> {
    row: Vec<Value>,
    order_by: &'a [(u16, SortDir, OrderColl)],
    /// The connection's HOST collating sequences, so a `COLLATE mycoll` key
    /// orders through the callback here exactly as it does in `sort_rows`.
    colls: Option<&'a dyn HostColls>,
    seq: u64,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: the ORDER BY spec. Secondary: scan sequence ASCENDING
        // regardless of the ORDER BY direction — a stable sort keeps equal
        // keys in original (scan) order, so the tiebreak is never reversed.
        cmp_rows(&self.row, &other.row, self.order_by, self.colls).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked<'_> {}

fn read_txn_write_bug() -> Error {
    Error::Internal("DML plan routed to a read transaction".into())
}
