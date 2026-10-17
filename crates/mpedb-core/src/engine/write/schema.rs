//! Write transaction schema operations.
use super::*;

impl<'e> WriteTxn<'e> {
    /// `CREATE TABLE` (#47 stage 2): append `def` to the schema — nothing
    /// renumbers (DESIGN-SCHEMA-V2: the new table takes the lowest free id;
    /// `Schema::with_added_table` validates the merged set) — write the
    /// re-canonicalized bytes to `CAT_SCHEMA_KEY`, seed the new table's
    /// empty tree-root entries (`catalog_entry` hard-errors on a missing
    /// key, exactly like bootstrap), and arm the schema-gen bump so every
    /// other process reloads at its next transaction. One ordinary COW
    /// commit publishes all of it atomically. Returns the new table's id.
    ///
    /// The txn's CAPTURED bundle still holds the old schema — a DDL txn
    /// does nothing else; the caller swaps the engine bundle after commit
    /// (`Engine::reload_schema_from_catalog`).
    fn validate_enum_definition(&mut self, column: &mpedb_types::ColumnDef) -> Result<()> {
        if let Some(e) = &column.pg_enum {
            let key = format!("pg_enum/{}", e.name);
            let bytes = self.sys_get(key.as_bytes())?.ok_or_else(|| Error::Bind(format!("type \"{}\" does not exist", e.name)))?;
            if mpedb_types::pgtype::PgEnum::decode(&bytes)? != *e {
                return Err(Error::Bind("enum definition changed while preparing DDL; retry".into()));
            }
        }
        Ok(())
    }

    pub fn create_table(&mut self, def: mpedb_types::TableDef) -> Result<u32> {
        if self.sys_get(format!("pg_enum/{}", def.name).as_bytes())?.is_some() {
            return Err(Error::Bind(format!("type \"{}\" already exists", def.name)));
        }
        for c in &def.columns { self.validate_enum_definition(c)?; }
        let new_schema = self
            .bundle
            .schema
            .with_added_table_capped(def, self.eng.max_tables)?;
        let new = new_schema.tables.last().expect("just appended");
        let tid = new.id;
        let n_indexes = new.indexes.len();
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;
        let empty = [0u8; 16]; // root 0 (empty tree), count 0
        for ino in 0..=n_indexes as u32 {
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &cat_tree_key(tid, ino),
                &mut btree::Payload::Flat(&empty),
                InsertMode::InsertOnly,
            )?;
            if out.existed {
                return Err(Error::Corrupt(format!(
                    "catalog already has tree entries for new table id {tid}"
                )));
            }
            self.catalog_root = out.new_root;
        }
        // An FTS table owns one extra tree — the inverted index at the reserved
        // FTS_INDEX_NO. Seed its catalog entry (root 0, count 0) so the first
        // `tree_root` finds it; the commit table-root write-back keeps it live.
        if new.kind.is_fts() {
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &cat_tree_key(tid, super::FTS_INDEX_NO),
                &mut btree::Payload::Flat(&empty),
                InsertMode::InsertOnly,
            )?;
            if out.existed {
                return Err(Error::Corrupt(format!(
                    "catalog already has an FTS tree entry for new table id {tid}"
                )));
            }
            self.catalog_root = out.new_root;
        }
        self.schema_gen_bump = true;
        // Refresh the txn's CAPTURED bundle from the pages just written: a
        // SECOND create in the same txn (an fts4 vtab lays down its five
        // shadow tables in one statement — plan §7) must derive the NEXT id,
        // not re-derive this one from the stale capture — the InsertOnly
        // seed above turned that mistake into a loud Corrupt.
        self.reload_bundle_from_catalog()?;
        Ok(tid)
    }

    /// DROP a table by id. Frees every reachable data/index page, unlinks the
    /// table's catalog tree-root entries, tombstones its schema slot in place
    /// (the id is NEVER reused — DESIGN-DROP-TABLE §0/§1), purges its CDC dirty
    /// entries and clears its capture/blocked bits, and arms the schema-gen
    /// bump. One ordinary COW commit publishes all of it atomically; other
    /// processes reload the tombstoned schema at their next transaction.
    ///
    /// Bounded to one commit (§2): a table larger than [`MAX_DROP_PAGES`]
    /// refuses — nothing is freed, the schema is untouched — rather than
    /// overflow the `u16` freelist chunk index. Pages committed by earlier txns
    /// are freed under THIS commit's id, so a reader still pinned on a snapshot
    /// that predates the drop keeps them un-reused (identical discipline to a
    /// large DELETE).
    pub fn drop_table(&mut self, table_id: u32) -> Result<()> {
        // Compute the tombstoned schema first: `with_dropped_table` validates
        // the id is live (and not the last live table), so an illegal drop bails
        // here before any page is freed or catalog entry unlinked.
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_dropped_table(table_id)?;
        let live = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let n_trees = 1 + live.indexes.len() as u32; // PK tree + secondaries
        // An FTS table owns one extra tree (the inverted index at FTS_INDEX_NO);
        // fold it into the tree-id list so DROP frees and unlinks it too.
        let fts_ino: Option<u32> = live.kind.is_fts().then_some(super::FTS_INDEX_NO);

        // 1. Collect every reachable page of the table's trees (branch, leaf,
        //    and overflow chains).
        let mut pages: BTreeSet<u64> = BTreeSet::new();
        for ino in (0..n_trees).chain(fts_ino) {
            let (root, _count) = self.tree_root(table_id, ino)?;
            if root != 0 {
                btree::collect_pages(self, root, &mut pages)?;
            }
        }
        if pages.len() as u64 > MAX_DROP_PAGES {
            return Err(Error::Unsupported(format!(
                "table {table_id} spans {} pages; DROP is bounded to {MAX_DROP_PAGES} per commit",
                pages.len()
            )));
        }

        // 2. Free them. `free` keys past-committed pages under this txn's id;
        //    the commit fixpoint records them, reader-pin safety governs reuse.
        for p in pages {
            PageStore::free(self, p)?;
        }

        // 3. Unlink the catalog tree-root entries so neither the page-accounting
        //    verifier nor a later `tree_root` sees the now-freed roots.
        for ino in (0..n_trees).chain(fts_ino) {
            let root = self.catalog_root;
            let out = btree::delete(self, root, &cat_tree_key(table_id, ino))?;
            self.catalog_root = out.new_root;
            self.table_roots.remove(&(table_id, ino));
        }
        // A dropped table has no generation left to publish, and its directory
        // entry is gone: leaving it in `mutated_tables` would make the commit's
        // bump loop look up a key this loop just deleted and report the
        // database CORRUPT for a perfectly legal `INSERT …; DROP TABLE …` in
        // one transaction (found in review, reproducible through the C API as
        // SQLITE_CORRUPT). Row mutations before the drop are unobservable
        // afterwards — the rows are gone with the table.
        self.mutated_tables.remove(&table_id);

        // 4. Publish the tombstoned schema.
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;

        // 5. Purge the table's CDC dirty entries — a mirror push must not try to
        //    re-read rows of a table that no longer exists.
        let mut lo = cdc::CDC_DIRTY_PREFIX.to_vec();
        lo.extend_from_slice(&table_id.to_be_bytes());
        let mut hi = cdc::CDC_DIRTY_PREFIX.to_vec();
        hi.extend_from_slice(&(table_id + 1).to_be_bytes());
        for (subkey, _v) in self.sys_scan_range(&lo, &hi)? {
            self.sys_delete(&subkey)?;
        }

        // 6. Clear any capture/blocked bits for the id (harmless if unset — a
        //    dead id is never written again — but keeps the control record
        //    clean and bumps the generation for per-process caches).
        let mut cfg = self.capture_config()?.clone();
        if cfg.is_captured(table_id) || cfg.is_blocked(table_id) {
            cfg.set_captured(table_id, false);
            cfg.set_blocked(table_id, false);
            cfg.generation += 1;
            self.sys_put(cdc::CDC_TABS_KEY, &cfg.encode())?;
            self.capture_cfg = Some(cfg);
        }

        // 7. Drop the table's columnar watermark record (the segment BLOCKS are
        //    a facade-owned sys namespace this layer cannot reach, but they too
        //    can never be matched again: table ids are never reused).
        self.clear_columnar_watermark(table_id)?;

        self.schema_gen_bump = true;
        // Same refresh as `create_table`, and MORE load-bearing here: the fts4
        // cascade drops six tables in one txn, each drop publishing the WHOLE
        // canonical schema derived from the captured bundle — without the
        // refresh, drop N+1 would re-publish tables drop N just tombstoned,
        // silently resurrecting them.
        self.reload_bundle_from_catalog()?;
        Ok(())
    }

    /// ALTER TABLE ... RENAME TO (#47 stage 5). Pure metadata: computes the
    /// renamed schema from THIS txn's captured bundle (so a concurrent DDL is
    /// caught by the id-liveness check, not silently overwritten) and publishes
    /// it. No tree roots move.
    pub fn alter_rename_table(&mut self, table_id: u32, new_name: &str) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_renamed_table(table_id, new_name)?;
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... RENAME [COLUMN] (#47 stage 5). Pure metadata: the column
    /// keeps its position and type, so no row is touched.
    /// `generated_srcs` carries the rewritten `AS (…)` sources — see
    /// [`mpedb_types::Schema::with_renamed_column`], which documents the
    /// verification the CALLER owes. This crate only passes them through: it
    /// has no SQL lexer either.
    pub fn alter_rename_column(
        &mut self,
        table_id: u32,
        column: &str,
        new_name: &str,
        generated_srcs: &[(u16, String)],
        check_srcs: &[(u16, String)],
    ) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_renamed_column(
            table_id,
            column,
            new_name,
            generated_srcs,
            check_srcs,
        )?;
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... ADD COLUMN (#47 stage 5). Appends a column and rewrites
    /// every existing row with `fill` in the new (trailing) position — mpedb's
    /// row image is schema-driven, not self-describing, so a widen cannot be
    /// lazy: an old short row decoded with the new (longer) type list would
    /// misread. `fill` is `Value::Null` for a plain nullable ADD, or the
    /// (already type-checked) constant of a `DEFAULT <const>` clause; the facade
    /// refuses UNIQUE / PRIMARY KEY on ADD and NOT NULL without a non-NULL
    /// default. The rewrite is one commit (whole table resident once, like
    /// DROP); batching is deferred. No secondary index is touched (the new
    /// column is unindexed).
    pub fn alter_add_column(
        &mut self,
        table_id: u32,
        col: mpedb_types::ColumnDef,
        fill: Value,
    ) -> Result<()> {
        self.validate_enum_definition(&col)?;
        if let (Some(e), Value::Text(label)) = (&col.pg_enum, &fill) {
            if !e.labels.contains(label) { return Err(Error::Bind(format!("invalid input value for enum {}: {:?}", e.name, label))); }
        }
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_added_column(table_id, col.clone())?;
        let old_types = bundle.col_types[table_id as usize].clone();
        // Where the new column lands. Normally trailing — but an implicit-rowid
        // table (#94) keeps its synthetic `rowid` LAST (`Schema::validate`
        // enforces it), so `with_added_column` inserts before it and so must the
        // row rewrite. Inserting at the wrong index would re-encode every row
        // with the new column where the rowid belongs.
        let implicit_rowid = new_schema
            .table(table_id)
            .is_some_and(|t| t.implicit_rowid);
        let at = if implicit_rowid { old_types.len() - 1 } else { old_types.len() };
        let mut new_types = old_types.clone();
        new_types.insert(at, col.ty);

        // Pass 1: read every row (decode with OLD types) and re-encode it with
        // the new column set to `fill`. Collect fully before mutating so the
        // cursor's read borrow is released before the write pass.
        let (root, count) = self.tree_root(table_id, 0)?;
        // A GENERATED column is not filled with a constant — every existing row
        // gets the expression's value for THAT row, computed against the widened
        // table. sqlite refuses `ADD COLUMN … STORED` outright once the table has
        // rows (it would have to rewrite them); mirror that refusal so the two
        // engines answer the same, even though mpedb's rewrite could do it.
        let gen_def = col.generated.as_ref().map(|_| {
            new_schema
                .table(table_id)
                .expect("widened table exists")
                .clone()
        });
        if let (Some(g), true) = (col.generated.as_ref(), count > 0) {
            if g.kind == mpedb_types::GeneratedKind::Stored {
                return Err(Error::Schema(
                    "cannot add a STORED generated column to a table that already has rows \
                     (sqlite refuses this too); add it as VIRTUAL, or recreate the table"
                        .into(),
                ));
            }
        }
        let mut rewritten: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, root, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let mut vals = row::decode_row(&v, &old_types)?;
                vals.insert(at, fill.clone());
                if let Some(t) = &gen_def {
                    t.apply_generated(&mut vals, &[])?;
                }
                let payload = row::encode_row(&vals, &new_types)?;
                rewritten.push((k.to_vec(), payload));
            }
        }
        // Pass 2: upsert the widened rows back at their (unchanged) keys.
        let mut cur_root = root;
        for (k, payload) in &rewritten {
            let out = btree::insert(
                self,
                cur_root,
                k,
                &mut btree::Payload::Flat(payload),
                InsertMode::Upsert,
            )?;
            cur_root = out.new_root;
        }
        self.set_tree_root(table_id, 0, cur_root, count);
        // The row rewrite went through `btree::insert` directly, not the
        // mutators, so no `note_columnar_mutation` fired — disarm the tail scan
        // explicitly (DESIGN-COLUMNAR §7).
        self.clear_columnar_watermark(table_id)?;
        self.publish_schema(&new_schema)
    }

    /// ALTER TABLE ... DROP COLUMN (#47 stage 5). Removes a column and rewrites
    /// every existing row without it (decode with old types, drop the value at
    /// the column's index, re-encode with new types). Refusals (PK column,
    /// indexed column, last column) are enforced by `with_dropped_column`, so an
    /// illegal drop bails before any row is touched. No secondary index tree is
    /// rebuilt: the dropped column is unindexed, and the surviving indexed
    /// columns' VALUES are unchanged — only their stored column *indices* shift
    /// (handled in the schema), which future writes honor.
    pub fn alter_drop_column(&mut self, table_id: u32, column: &str) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        // Column index in the OLD layout (needed for the row rewrite). The
        // schema evolver re-checks existence and the refusals below.
        let idx = {
            let table = bundle
                .schema
                .table(table_id)
                .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
            table
                .columns
                .iter()
                .position(|c| mpedb_types::ident_eq(&c.name, column))
                .ok_or_else(|| {
                    Error::Schema(format!("no column `{column}` in table id {table_id}"))
                })?
        };
        let new_schema = bundle.schema.with_dropped_column(table_id, column)?;
        let old_types = bundle.col_types[table_id as usize].clone();
        let mut new_types = old_types.clone();
        new_types.remove(idx);

        let (root, count) = self.tree_root(table_id, 0)?;
        let mut rewritten: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, root, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let mut vals = row::decode_row(&v, &old_types)?;
                vals.remove(idx);
                let payload = row::encode_row(&vals, &new_types)?;
                rewritten.push((k.to_vec(), payload));
            }
        }
        let mut cur_root = root;
        for (k, payload) in &rewritten {
            let out = btree::insert(
                self,
                cur_root,
                k,
                &mut btree::Payload::Flat(payload),
                InsertMode::Upsert,
            )?;
            cur_root = out.new_root;
        }
        self.set_tree_root(table_id, 0, cur_root, count);
        // DROP COLUMN renumbers the surviving ordinals the segments are keyed by,
        // and the tail scan reads blocks at their BUILD gen (not the current
        // one), so the whole-table mod_gen guard would not catch it here —
        // disarm the watermark explicitly (DESIGN-COLUMNAR §7).
        self.clear_columnar_watermark(table_id)?;
        self.publish_schema(&new_schema)
    }

    /// DROP INDEX: free the index's tree and remove it from the schema.
    ///
    /// `pos` is the position in `TableDef::indexes`; its tree is `index_no =
    /// pos + 1` (index 0 is the PK tree). Because `index_no` IS the position,
    /// every index above the dropped one must move down by one — catalog
    /// tree-root entry and in-memory cache together, in this transaction. Doing
    /// it any other way leaves a plan pointing at the wrong B-tree, which is a
    /// wrong ANSWER rather than an error.
    ///
    /// Ordering mirrors `drop_table`: validate via the schema first (so an
    /// illegal drop bails before a page is freed), then free, then relink, then
    /// publish.
    pub fn drop_index(&mut self, table_id: u32, pos: usize) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        let new_schema = bundle.schema.with_dropped_index(table_id, pos)?;
        let live = bundle
            .schema
            .table(table_id)
            .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
        let n_idx = live.indexes.len() as u32;
        let dropped_ino = pos as u32 + 1;

        // 1. Free the dropped tree's pages.
        let mut pages: BTreeSet<u64> = BTreeSet::new();
        let (root, _count) = self.tree_root(table_id, dropped_ino)?;
        if root != 0 {
            btree::collect_pages(self, root, &mut pages)?;
        }
        if pages.len() as u64 > MAX_DROP_PAGES {
            return Err(Error::Unsupported(format!(
                "index {dropped_ino} of table {table_id} spans {} pages; DROP is bounded \
                 to {MAX_DROP_PAGES} per commit",
                pages.len()
            )));
        }
        for p in pages {
            PageStore::free(self, p)?;
        }

        // 2. Unlink the dropped entry, then SHIFT every higher index down one.
        //    Read the whole tail first: deleting and re-inserting as we go would
        //    have each step read a key the previous step just moved.
        let mut tail: Vec<(u32, (u64, u64))> = Vec::new();
        for ino in (dropped_ino + 1)..=n_idx {
            tail.push((ino, self.tree_root(table_id, ino)?));
        }
        for ino in dropped_ino..=n_idx {
            let root = self.catalog_root;
            let out = btree::delete(self, root, &cat_tree_key(table_id, ino))?;
            self.catalog_root = out.new_root;
            self.table_roots.remove(&(table_id, ino));
        }
        for (ino, (root, count)) in tail {
            self.set_tree_root(table_id, ino - 1, root, count);
        }

        // 3. Publish.
        self.publish_schema(&new_schema)
    }

    /// CREATE INDEX: add a secondary index and BUILD its tree over the existing
    /// rows. Scans the PK tree once, computes each row's index key (skipping
    /// rows with any NULL indexed column — SQL membership), and inserts
    /// `key → pk`. A UNIQUE index whose build hits a duplicate aborts with a
    /// violation (nothing is published). One commit; the new tree's catalog
    /// root is persisted by the commit's table-root write-back.
    #[allow(clippy::too_many_arguments)]
    pub fn create_index(
        &mut self,
        table_id: u32,
        columns: Vec<u16>,
        // Parallel to `columns` when non-empty; see `IndexDef::exprs`. A part
        // whose entry is `Some` takes its key value from the EXPRESSION, and
        // `columns[i]` is the `INDEX_EXPR_COL` sentinel.
        exprs: Vec<Option<String>>,
        // Parallel to `columns` when non-empty: a per-part `COLLATE` override.
        collations: Vec<Option<mpedb_types::Collation>>,
        unique: bool,
        predicate: Option<String>,
        // `name`: what the user wrote. Carried so `DROP INDEX`/`REINDEX` can
        // name one; the index itself is still identified POSITIONALLY.
        name: Option<String>,
    ) -> Result<()> {
        let bundle = Arc::clone(&self.bundle);
        // A partial index is BUILT over its members only, and every later write
        // re-decides membership through `index_entry_key`. Before that existed,
        // a partial index was built whole and a UNIQUE one had to be refused
        // outright — rows outside the predicate would have collided with each
        // other. (A non-unique partial was allowed because the planner never
        // picked one for access, so a too-large index still answered.)
        let pred_src = predicate.clone();
        let new_schema = bundle.schema.with_added_index(
            table_id,
            mpedb_types::IndexDef {
                state: mpedb_types::IndexState::Ready,
                columns: columns.clone(),
                unique,
                predicate,
                name,
                exprs: exprs.clone(),
                collations: collations.clone(),
                // This is the `CREATE INDEX` path — an index someone asked for
                // by that name. A UNIQUE CONSTRAINT is declared with the table
                // and built by the schema, never through here.
                from_constraint: false,
            },
        )?;
        // The key-part programs for the index being built, from the SAME
        // compiler the bundle rebuild will use — so the rows this build keys
        // and the keys later writes compute cannot disagree.
        let build_exprs: Vec<Option<mpedb_types::ExprProgram>> = if exprs.is_empty() {
            Vec::new()
        } else {
            let progs = self.eng.compile_programs(&new_schema, &bundle.checks);
            let pos = bundle.schema.table(table_id).map_or(0, |t| t.indexes.len());
            progs
                .index_exprs
                .get(table_id as usize)
                .and_then(|v| v.get(pos))
                .cloned()
                .unwrap_or_default()
        };
        if !exprs.is_empty() && build_exprs.len() != exprs.len() {
            return Err(Error::Unsupported(
                "CREATE INDEX with expression key parts needs the SQL layer's \
                 compiler; this handle has none, and building the index without \
                 evaluating them would store the wrong keys"
                    .into(),
            ));
        }
        // The predicate program, likewise from the same compiler; a predicate
        // with no program refuses below, in `index_predicate_admits`.
        let build_pred: Option<Option<mpedb_types::ExprProgram>> = pred_src.as_ref().map(|_| {
            self.eng
                .compile_programs(&new_schema, &bundle.checks)
                .index_predicates
                .get(table_id as usize)
                .and_then(|v| v.get(bundle.schema.table(table_id).map_or(0, |t| t.indexes.len())))
                .cloned()
                .flatten()
        });
        let (tname, new_ino, idx_coll) = {
            let table = bundle
                .schema
                .table(table_id)
                .ok_or_else(|| Error::Internal(format!("no table id {table_id}")))?;
            // index 0 is the PK tree; the new secondary is appended after the
            // existing ones. The new index's per-column collation comes straight
            // from the columns it covers (it is not in the bundle's caches yet).
            // An EXPRESSION part (v13) has no column to take a type or
            // collation from; its value is computed, and the plain spec is what
            // the synthetic-row key builder uses for it too.
            let coll: Vec<keycode::KeySpec> = columns
                .iter()
                .enumerate()
                .map(|(k, &c)| match table.columns.get(c as usize) {
                    None => keycode::KeySpec::default(),
                    Some(cd) => keycode::KeySpec::for_column(
                        cd.ty,
                        // A per-part `COLLATE` (v14) overrides the column's own.
                        collations.get(k).copied().flatten().unwrap_or(cd.collation),
                    ),
                })
                .collect();
            (table.name.clone(), (table.indexes.len() + 1) as u32, coll)
        };
        let col_types = bundle.col_types[table_id as usize].clone();

        // Collect (index key, pk) for every row that has an entry.
        let (pkroot, _) = self.tree_root(table_id, 0)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut c = btree::cursor(self, pkroot, None, None)?;
            while let Some((k, v)) = c.next(self)? {
                let values = row::decode_row(&v, &col_types)?;
                if !super::index_predicate_admits(
                    build_pred.as_ref(),
                    pred_src.as_ref(),
                    &tname,
                    &values,
                )? {
                    continue;
                }
                // Expression parts are evaluated into a synthetic row, exactly
                // as `index_entry_key` does for every later write — the build
                // and the maintenance have to agree on what the key IS.
                let (ords, parts) = if build_exprs.is_empty() {
                    (columns.clone(), values)
                } else {
                    let mut parts = Vec::with_capacity(build_exprs.len());
                    for (j, e) in build_exprs.iter().enumerate() {
                        match e {
                            None => parts.push(values[columns[j] as usize].clone()),
                            Some(prog) => {
                                let mut stack = Vec::new();
                                parts.push(prog.eval_with_stack(&mut stack, &values, &[])?);
                            }
                        }
                    }
                    ((0..parts.len() as u16).collect(), parts)
                };
                if let Some(ikey) = index_row_key(unique, &ords, &parts, &k, &idx_coll) {
                    entries.push((ikey, k));
                }
            }
        }
        // Build the index tree from an empty root.
        let mut iroot = 0u64;
        let mut icount = 0u64;
        for (ikey, pk) in &entries {
            let out = btree::insert(
                self,
                iroot,
                ikey,
                &mut btree::Payload::Flat(pk),
                InsertMode::InsertOnly,
            )?;
            if unique && out.existed {
                return Err(Error::UniqueViolation {
                    table: tname,
                    constraint: index_constraint_name(self.eng, table_id, &columns),
                });
            }
            iroot = out.new_root;
            icount += 1;
        }
        self.set_tree_root(table_id, new_ino, iroot, icount);
        self.publish_schema(&new_schema)
    }

    /// Publish a new schema (already validated by the caller via one of the
    /// `Schema::with_*` evolvers) into the catalog and arm the schema-gen bump.
    /// For a PURE-METADATA change (rename): no tree roots move, so this single
    /// `CAT_SCHEMA_KEY` upsert is the whole commit. The caller swaps the engine
    /// bundle after commit; other processes reload at their next transaction.
    pub fn publish_schema(&mut self, new_schema: &mpedb_types::Schema) -> Result<()> {
        let bytes = new_schema.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;
        self.schema_gen_bump = true;
        Ok(())
    }

    /// Arm the schema-generation bump for this commit without changing the
    /// catalog schema — used for sys-keyspace-only DDL (e.g. `CREATE VIEW`
    /// stores its source under `view/<name>`) so peer processes drop caches that
    /// baked in the old definition.
    pub fn bump_schema_gen(&mut self) {
        self.schema_gen_bump = true;
    }

    /// This transaction's captured schema view (one cheap Arc clone). For a
    /// writer it starts equal to the engine's committed bundle, and moves ahead
    /// of it only when this txn applies DDL and calls
    /// [`reload_bundle_from_catalog`](Self::reload_bundle_from_catalog) — so a
    /// facade session can compile and execute a statement against the exact
    /// schema its own uncommitted DDL will commit with (#95).
    pub fn schema_bundle(&self) -> Arc<SchemaBundle> {
        Arc::clone(&self.bundle)
    }

    /// Rebuild this transaction's captured schema bundle from its OWN
    /// (uncommitted) catalog pages, so a later statement in the SAME
    /// transaction sees a DDL change this txn just applied (`create_table`,
    /// `drop_table`, `alter_*`, `create_index`). The engine's committed bundle
    /// and every other transaction are untouched — only THIS writer's view
    /// moves, and it moves back automatically on abort (the change lives in
    /// COW pages the abort discards).
    ///
    /// CHECK programs come from the facade's compiler, installed on the engine
    /// ([`Engine::set_check_compiler`]) — the same call
    /// [`Engine::reload_schema_from_catalog`] makes. That matters here more than
    /// anywhere: a table this txn just created with a `CHECK (…)` is inserted
    /// into by the NEXT statement of the SAME transaction, so an empty program
    /// slot would mean the constraint never fires for exactly the rows written
    /// alongside it. Returns the new bundle so the caller can compile/execute
    /// against it.
    pub fn reload_bundle_from_catalog(&mut self) -> Result<Arc<SchemaBundle>> {
        let root = self.catalog_root;
        let bytes = btree::get(self, root, CAT_SCHEMA_KEY)?
            .ok_or_else(|| Error::Corrupt("no schema stored in catalog".into()))?;
        let schema = mpedb_types::Schema::from_canonical_bytes(&bytes)?;
        // The gen this bundle WILL carry once this txn commits (its DDL armed
        // `schema_gen_bump`). It never escapes this txn's lifetime; it only
        // keeps the value distinct from the pre-DDL bundle's gen.
        let gen = self.meta.schema_gen + 1;
        let programs = self.eng.compile_programs(&schema, &self.bundle.checks);
        let bundle = Arc::new(SchemaBundle::new_at(gen, schema, programs));
        self.bundle = Arc::clone(&bundle);
        Ok(bundle)
    }


}
