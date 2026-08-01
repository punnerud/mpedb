use super::*;

// ---------------------------------------------------------------- the passes

/// One column's freshly built blocks, held between the fold (which reads the
/// snapshot) and the write session (which cannot borrow it).
struct BuiltColumn {
    ci: u16,
    name: String,
    blocks: Vec<Vec<u8>>,
    rows: u64,
    bytes: u64,
}

impl crate::Database {
    /// Build column segments for every segmentable column of every table —
    /// the explicit pass of DESIGN-COLUMNAR §5. Nothing here runs on the write
    /// path; a heavy write workload simply leaves segments stale (and so
    /// unused) until the next pass, which is correct.
    ///
    /// The pass reads ONE snapshot per table and stamps every record with that
    /// snapshot's `mod_gen`. If a writer commits while the pass runs, the
    /// records are stamped with a generation the table no longer reports and
    /// every one of them reads as stale — wasted work, never a wrong answer.
    pub fn compact_columns(&self) -> Result<Vec<ColSegStat>> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let mut out = Vec::new();
        for t in bundle.schema.tables.iter().filter(|x| !x.dead) {
            if !matches!(t.kind, mpedb_types::TableKind::Standard) {
                continue;
            }
            self.compact_table(t, &mut out)?;
        }
        Ok(out)
    }

    /// Build every segmentable column of ONE table from a single read snapshot,
    /// then arm the segment/row-tail split scan (DESIGN-COLUMNAR §7) with a
    /// watermark — the highest PK the snapshot covered — set under a mod_gen CAS.
    ///
    /// All columns share the snapshot's generation `g0`, so the whole table's
    /// segments are internally consistent: a reader either uses all of them (the
    /// watermark is present and every block decodes at `g0`) or none. The
    /// watermark is published only if the table is STILL at `g0` when the write
    /// commits — no user write raced the build — which is what makes appending
    /// above the watermark safe afterwards. Returns the number of columns built.
    fn compact_table(
        &self,
        t: &mpedb_types::TableDef,
        out: &mut Vec<ColSegStat>,
    ) -> Result<u32> {
        let r = self.engine.begin_read()?;
        let g0 = r.mod_gen(t.id)?;
        let covered_rows = r.row_count(t.id)?;
        let wm_pk = r.max_pk_key(t.id)?;
        // Fold each column from the ONE snapshot, holding only the encoded
        // blocks (≈12× smaller than the rows) between here and the write below —
        // the per-block Value buffer is the actual memory peak, one block wide.
        let mut built: Vec<BuiltColumn> = Vec::new();
        let mut hard_err: Option<Error> = None;
        'cols: for (ci, col) in t.columns.iter().enumerate() {
            if !segmentable(col.ty) {
                continue;
            }
            let mut blocks: Vec<Vec<u8>> = Vec::new();
            let mut buf: Vec<Value> = Vec::with_capacity(BLOCK_ROWS);
            let mut rows = 0u64;
            let fold = r.fold_range_column(
                t.id,
                None,
                None,
                ci as u16,
                mpedb_core::FoldOpts::SERIAL,
                &mut |v: &Value| {
                    buf.push(v.clone());
                    rows += 1;
                    if buf.len() == BLOCK_ROWS {
                        blocks.push(encode_block(g0, col.ty, &buf)?);
                        buf.clear();
                    }
                    Ok(())
                },
            );
            let fold = fold.and_then(|_| {
                if !buf.is_empty() {
                    blocks.push(encode_block(g0, col.ty, &buf)?);
                }
                Ok(())
            });
            match fold {
                Ok(()) => {}
                Err(Error::Unsupported(_)) => continue 'cols, // column not foldable
                Err(e) => {
                    hard_err = Some(e);
                    break 'cols;
                }
            }
            let bytes = blocks.iter().map(|b| b.len() as u64).sum();
            built.push(BuiltColumn { ci: ci as u16, name: col.name.clone(), blocks, rows, bytes });
        }
        r.finish()?;
        if let Some(e) = hard_err {
            return Err(e);
        }
        if built.is_empty() {
            return Ok(0);
        }

        // Write every column's blocks, then CAS the watermark — all in ONE
        // commit, so a reader never sees a mix of two builds. Each column's old
        // blocks are deleted first (a shortened table would otherwise leave a
        // trailing stale block).
        let mut sess = self.begin()?;
        for b in &built {
            let lo = record_key(t.id, b.ci, 0);
            let mut hi = record_key(t.id, b.ci, u32::MAX);
            for x in hi.iter_mut().skip(6) {
                *x = 0xFF;
            }
            // Replace: delete every old block record first (a shortened table
            // would otherwise leave a trailing stale block). Deleting the record
            // frees its extent through the one btree free-old-val path — the
            // old run stays readable for a reader still pinned on the pre-replace
            // snapshot (the row-value extent MVCC discipline, inherited), no
            // manual free bookkeeping. `_` is the resolved old block (unread).
            for k in sess.sys_record_scan_range_keys(NS, &lo, &hi)? {
                sess.sys_record_delete(NS, &k)?;
            }
            for (bi, blk) in b.blocks.iter().enumerate() {
                sess.sys_record_put_extent(NS, &record_key(t.id, b.ci, bi as u32), blk)?;
            }
        }
        // A non-empty table gets a watermark iff nothing raced the build; an
        // empty one clears any stale watermark by leaving none (the CAS with
        // covered_rows 0 is a no-op, and the segments cover nothing anyway).
        match &wm_pk {
            Some(pk) => {
                sess.set_columnar_watermark_if_gen(t.id, g0, covered_rows, pk)?;
            }
            None => {
                sess.clear_columnar_watermark(t.id)?;
            }
        }
        sess.commit()?;

        let n = built.len() as u32;
        for b in built {
            out.push(ColSegStat {
                table: t.name.clone(),
                column: b.name,
                blocks: b.blocks.len() as u32,
                rows: b.rows,
                bytes: b.bytes,
            });
        }
        Ok(n)
    }

    /// Delete every column segment of one table (by id) AND its watermark.
    /// Returns how many segment records went. Clearing the watermark disarms the
    /// tail scan; without segments it would decline anyway, but leaving a stale
    /// watermark record is untidy.
    fn drop_table_segments(&self, table_id: u32) -> Result<usize> {
        let prefix = table_id.to_be_bytes();
        // Deleting each block record frees its extent through the btree's one
        // free-old-val path — no manual run bookkeeping. Scan KEYS only, or a
        // drop would resolve every 512 KiB block just to learn its key.
        let keys: Vec<Vec<u8>> = self
            .sys_record_scan_keys(NS)?
            .into_iter()
            .filter(|k| k.starts_with(&prefix))
            .collect();
        let n = keys.len();
        let mut s = self.begin()?;
        for k in keys {
            s.sys_record_delete(NS, &k)?;
        }
        s.clear_columnar_watermark(table_id)?;
        s.commit()?;
        Ok(n)
    }

    /// Make the stored column segments match the workload MODEL (DESIGN-COLUMNAR
    /// §2, stage 4) — the "automatic + sparse + dynamic via MPEE" half of the
    /// ask. A table is columnar-eligible when the model marks it scan-heavy:
    /// role `fact`, or a scan archetype (`star-olap`) unless the table is a
    /// `dimension`. A point-oriented table gains nothing from a segment, so its
    /// segments are DROPPED; an eligible table gets one per segmentable column.
    /// Regenerable — safe to run whenever. `mpedb model sync-columnar`.
    pub fn sync_columnar(&self) -> Result<ColumnarSync> {
        let model = self.require_model()?;
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let scan_archetype =
            matches!(model.archetype, Some(mpedb_types::model::Archetype::StarOlap));

        let mut out = ColumnarSync::default();
        for t in bundle.schema.tables.iter().filter(|x| !x.dead) {
            if !matches!(t.kind, mpedb_types::TableKind::Standard) {
                continue;
            }
            if !columnar_eligible(&model, scan_archetype, &t.name) {
                if self.drop_table_segments(t.id)? > 0 {
                    out.dropped.push(t.name.clone());
                }
                continue;
            }
            let mut stats = Vec::new();
            let built = self.compact_table(t, &mut stats)?;
            if built > 0 {
                out.columnarized.push((t.name.clone(), built));
            }
        }
        Ok(out)
    }

    fn require_model(&self) -> Result<mpedb_types::model::WorkloadModel> {
        self.model()?.ok_or_else(|| {
            Error::Unsupported(
                "no model stored — `mpedb model set` first; the model's roles are \
                 what say which tables are scanned (columnar) vs pointed at (row)"
                    .into(),
            )
        })
    }

    /// The cheap, bounded LIVE CHECK (stage B): read-only, O(model tables), it
    /// says what `maintain_columnar` WOULD do without doing any of it. Per
    /// eligible table it reports whether the segments are absent (Build), or
    /// present but the row tail above the stage-5 watermark has grown past
    /// `tail_fraction` of the covered rows (Rebuild); an ineligible table that
    /// still carries a watermark is a Drop. `Fresh` tables are omitted.
    ///
    /// One read snapshot, one watermark read and one row-count per table — safe
    /// to call often (a query-path hook can consult it and record the need
    /// without ever paying for a build).
    pub fn columnar_maintenance_plan(
        &self,
        tail_fraction: f64,
    ) -> Result<Vec<ColumnarMaintenance>> {
        let model = self.require_model()?;
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let scan_archetype =
            matches!(model.archetype, Some(mpedb_types::model::Archetype::StarOlap));
        let r = self.engine.begin_read()?;
        let mut out = Vec::new();
        for t in bundle.schema.tables.iter().filter(|x| !x.dead) {
            if !matches!(t.kind, mpedb_types::TableKind::Standard) {
                continue;
            }
            let eligible = columnar_eligible(&model, scan_archetype, &t.name);
            // A live watermark == usable segments (compact_table publishes it
            // under a CAS, and any covered write deletes it), so its covered-row
            // count is the exact "are the segments there and how much do they
            // cover" signal.
            let covered = r.columnar_watermark(t.id)?.map(|(_, w, _)| w);
            let action = if eligible {
                match covered {
                    None => MaintainAction::Build,
                    Some(w) => {
                        let n = r.row_count(t.id)?;
                        let tail = n.saturating_sub(w);
                        if w > 0 && tail as f64 > tail_fraction.max(0.0) * w as f64 {
                            MaintainAction::Rebuild { covered: w, tail }
                        } else {
                            MaintainAction::Fresh
                        }
                    }
                }
            } else if covered.is_some() {
                MaintainAction::Drop
            } else {
                MaintainAction::Fresh
            };
            if action != MaintainAction::Fresh {
                out.push(ColumnarMaintenance { table: t.name.clone(), action });
            }
        }
        r.finish()?;
        Ok(out)
    }

    /// Apply the maintenance plan (stage B): build absent segments, rebuild the
    /// ones whose tail grew past `tail_fraction`, drop the ones the model no
    /// longer wants — but at most `max_rebuilds` builds/rebuilds this call
    /// (0 = unbounded), so a single maintenance pass has a bounded cost even
    /// when many tables went stale at once. Drops are cheap and never capped.
    ///
    /// The adaptive counterpart to `sync_columnar` (which rebuilds every
    /// eligible table unconditionally): this touches only what the live check
    /// found stale, so re-running it on a settled database does nothing.
    pub fn maintain_columnar(
        &self,
        tail_fraction: f64,
        max_rebuilds: usize,
    ) -> Result<ColumnarSync> {
        let plan = self.columnar_maintenance_plan(tail_fraction)?;
        let bundle = self.schema();
        let find = |name: &str| {
            bundle
                .schema
                .tables
                .iter()
                .find(|t| !t.dead && mpedb_types::ident_eq(&t.name, name))
        };
        let mut out = ColumnarSync::default();
        let mut rebuilds = 0usize;
        for m in plan {
            match m.action {
                MaintainAction::Build | MaintainAction::Rebuild { .. } => {
                    if max_rebuilds > 0 && rebuilds >= max_rebuilds {
                        continue; // bounded — the rest wait for the next pass
                    }
                    let Some(t) = find(&m.table) else { continue };
                    let mut stats = Vec::new();
                    let built = self.compact_table(t, &mut stats)?;
                    rebuilds += 1;
                    if built > 0 {
                        out.columnarized.push((m.table.clone(), built));
                    }
                }
                MaintainAction::Drop => {
                    if let Some(t) = find(&m.table) {
                        if self.drop_table_segments(t.id)? > 0 {
                            out.dropped.push(m.table.clone());
                        }
                    }
                }
                MaintainAction::Fresh => {}
            }
        }
        Ok(out)
    }

    /// Drop every stored column segment and every columnar watermark. Segments
    /// are regenerable, so this is always safe — it only costs the next scan its
    /// speed.
    pub fn drop_column_segments(&self) -> Result<usize> {
        // Only the KEYS: deleting each block record frees its extent through the
        // btree's one free-old-val path (resolving values would materialize
        // every block).
        let keys: Vec<Vec<u8>> = self.sys_record_scan_keys(NS)?;
        let n = keys.len();
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let mut s = self.begin()?;
        for k in keys {
            s.sys_record_delete(NS, &k)?;
        }
        for t in bundle.schema.tables.iter().filter(|x| !x.dead) {
            s.clear_columnar_watermark(t.id)?;
        }
        s.commit()?;
        Ok(n)
    }

    /// Test/diagnostic observability: the covered-row count of a table's live
    /// columnar watermark, or `None` if it has none (never compacted, or a
    /// covered-row write or column DDL invalidated it). Lets a test assert the
    /// split scan is actually armed rather than only that the answer is right.
    #[doc(hidden)]
    pub fn columnar_watermark_covered(&self, table: &str) -> Result<Option<u64>> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let Some(t) = bundle
            .schema
            .tables
            .iter()
            .find(|t| !t.dead && mpedb_types::ident_eq(&t.name, table))
        else {
            return Ok(None);
        };
        let id = t.id;
        let r = self.engine.begin_read()?;
        let wm = r.columnar_watermark(id)?;
        r.finish()?;
        Ok(wm.map(|(_, w, _)| w))
    }
}

