use super::*;

impl Schema {
    /// Evolve this schema by APPENDING one table — `CREATE TABLE` (#47).
    /// Nothing renumbers: existing ids and positions are untouched, the new
    /// table takes the lowest free id (= the current count while ids are
    /// dense), and the vec stays id-sorted (creation order). Flags normalize
    /// and indexes derive exactly as at seed.
    pub fn with_added_table(&self, mut def: TableDef) -> Result<Schema> {
        // `tables.len()` (live + dead) is the monotone id high-water: dead
        // slots are never removed and ids are NEVER reused (DESIGN-DROP-TABLE
        // §0 — reuse would require a crash-atomic distributed purge of every
        // persisted `table_id` record, the exact silent-corruption class mpedb
        // exists to prevent; the bounded-limit + offline `regenerate` compaction
        // is the deliberate trade). Fail closed at MAX_TABLES — now a cost
        // bound (tombstone bloat), not a bitmap width (DESIGN-TABLE-CAP).
        if self.tables.len() >= MAX_TABLES {
            return Err(Error::Schema(
                "table-id space exhausted (MAX_TABLES lifetime creates); rebuild required".into(),
            ));
        }
        def.id = self.tables.len() as u32;
        def.dead = false;
        normalize_and_derive(&mut def);
        let mut tables = self.tables.clone();
        tables.push(def);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by DROPPING one table (#47 stage 4). The slot is
    /// replaced with a tombstone in place — the id is retired, never reused,
    /// `position == id` and every other table's id/data are untouched.
    pub fn with_dropped_table(&self, id: u32) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(id as usize)
            .filter(|t| t.id == id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {id} to drop")))?;
        *slot = TableDef::tombstone(id);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by ADDING a secondary index (CREATE INDEX). The
    /// caller builds the index tree over existing rows. `columns` are ordinals
    /// into the table's columns, in key order. Errors on an unknown column, an
    /// index-count overflow, or an identical index already present (the caller
    /// treats "already exists" as a no-op for idempotency / `IF NOT EXISTS`).
    pub fn with_added_index(&self, table_id: u32, index: IndexDef) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        for &c in &index.columns {
            // An EXPRESSION key part carries the sentinel instead of an ordinal
            // (v13): it reads whatever its source reads, which may be two
            // columns or none, so there is nothing here to range-check.
            if c == INDEX_EXPR_COL {
                continue;
            }
            if c as usize >= slot.columns.len() {
                return Err(Error::Schema(format!(
                    "CREATE INDEX on `{}`: column ordinal {c} out of range",
                    slot.name
                )));
            }
        }
        // Two indexes of the same SHAPE are LEGAL. They are redundant — the
        // second answers no probe the first cannot — but redundant is not
        // illegal, sqlite builds both, and `remove_unique_together` on an
        // already-unique field is Django emitting exactly such a pair.
        //
        // This was a refusal until the shim stopped keying an index's name
        // record by its shape (`index_fingerprint_of`). Under a shape key two
        // same-shape indexes collided onto one record and `PRAGMA index_list`
        // reported a duplicate name — a wrong answer. The records are keyed by
        // NAME now, so the collision has nowhere left to happen.
        //
        // What is still refused is a duplicate NAME, and that is checked by
        // the two `CREATE INDEX` appliers rather than here: this function also
        // serves flag-derived indexes, which have no name at all.
        slot.indexes.push(index);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by REMOVING the index at `pos` in `table_id`'s index
    /// list (`DROP INDEX`).
    ///
    /// The entry is removed, not tombstoned, because `index_no = position + 1`
    /// is the contract every plan and B-tree key on — so the engine renumbers
    /// the catalog tree-roots above `pos` to match, in the same transaction.
    /// A tombstone would keep a dead tree alive forever and still cost the
    /// numbering, which is the worst of both.
    ///
    /// A FLAG-DERIVED index (one with no name, from `unique`/`indexed` in the
    /// config) is refused here rather than silently dropped: its existence is
    /// declared by the config, so removing it would put the live schema at odds
    /// with the file's own declaration on the next attach.
    pub fn with_dropped_index(&self, table_id: u32, pos: usize) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        let ix = slot.indexes.get(pos).ok_or_else(|| {
            Error::Schema(format!(
                "table `{}` has no index at position {pos}",
                slot.name
            ))
        })?;
        if ix.name.is_none() {
            return Err(Error::Schema(format!(
                "the index on `{}` was derived from a column flag and has no name — \
                 drop it by editing the declaration, not with DROP INDEX",
                slot.name
            )));
        }
        slot.indexes.remove(pos);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Find an index by NAME anywhere in the schema, as `(table_id, position)`.
    /// Index names are compared case-insensitively, like every other SQL
    /// identifier here.
    pub fn find_index_by_name(&self, name: &str) -> Option<(u32, usize)> {
        let want = name.to_ascii_lowercase();
        for t in self.tables.iter().filter(|t| !t.dead) {
            for (i, ix) in t.indexes.iter().enumerate() {
                if ix.name.as_deref().map(|n| n.to_ascii_lowercase()) == Some(want.clone()) {
                    return Some((t.id, i));
                }
            }
        }
        None
    }

    /// Evolve this schema by RENAMING a table (#47 stage 5). Pure metadata: the
    /// id, columns, keys, indexes, and all row data are untouched — only the
    /// name changes. `validate` rejects a collision with another live table.
    pub fn with_renamed_table(&self, id: u32, new_name: &str) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(id as usize)
            .filter(|t| t.id == id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {id} to rename")))?;
        let old_name = std::mem::replace(&mut slot.name, new_name.to_string());
        // Every OTHER table's foreign keys that referenced the old name follow
        // the rename, self-references included — `ForeignKeyDef.parent` is a
        // NAME, resolved at check time, so leaving it was not a dangling
        // pointer but something worse: enforcement kept working against
        // whatever table NEXT took the old name, and `PRAGMA foreign_key_list`
        // reported a parent that no longer existed. sqlite rewrites dependent
        // references on RENAME (legacy_alter_table off, the default), and
        // Django's schema editor renames tables as its normal ALTER strategy,
        // so this is the ordinary path.
        //
        // A key that FORWARD-references a table not created yet is untouched
        // unless it named the OLD name — and one that already named the NEW
        // name now resolves to the renamed table, which is exactly sqlite's
        // check-time name resolution too.
        for t in tables.iter_mut().filter(|t| !t.dead) {
            for fk in &mut t.foreign_keys {
                if ident_eq(&fk.parent, &old_name) {
                    fk.parent = new_name.to_string();
                }
            }
        }
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by APPENDING a column to a table (#47 stage 5). The
    /// new column takes the highest index, so existing column/index positions
    /// are untouched; the caller rewrites existing rows with the new column
    /// NULL. Errors on a name collision or an invalid merged schema (e.g. too
    /// many columns).
    pub fn with_added_column(&self, table_id: u32, col: ColumnDef) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        if slot.columns.iter().any(|c| ident_eq(&c.name, &col.name)) {
            return Err(Error::Schema(format!(
                "column `{}` already exists in table `{}`",
                col.name, slot.name
            )));
        }
        // An implicit-rowid table (#94: a `CREATE TABLE` with no declared PK,
        // which is what the C-API shim produces by default) carries a synthetic
        // trailing `rowid` column, and `validate` REQUIRES it to stay last and
        // sole PK. Appending past it produced a schema that fails its own
        // validator — so a migration was impossible on exactly the tables the
        // shim creates. Insert BEFORE the rowid instead and shift the PK index,
        // which keeps both the invariant and the user-visible column order
        // (`SELECT *` hides the rowid, so the new column is still last).
        if slot.implicit_rowid {
            let at = slot.columns.len() - 1;
            slot.columns.insert(at, col);
            // Everything at or past the insertion point moves up — the same
            // three lists as the DROP path, for the same reason.
            slot.renumber_columns(|c: &mut u16| {
                if *c as usize >= at {
                    *c += 1;
                }
            });
        } else {
            slot.columns.push(col);
        }
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by DROPPING one column of a table (#47 stage 5). The
    /// caller rewrites existing rows without the column. Refused when the column
    /// is part of the PK, referenced by any secondary index, or the table's last
    /// column (no online index rebuild, and a table needs its key). Column
    /// indices of surviving columns AFTER the dropped one shift down by one, so
    /// the PK and every index's stored column references are renumbered to match.
    pub fn with_dropped_column(&self, table_id: u32, column: &str) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        let idx = slot
            .columns
            .iter()
            .position(|c| ident_eq(&c.name, column))
            .ok_or_else(|| Error::Schema(format!("no column `{column}` in table `{}`", slot.name)))?;
        let i = idx as u16;
        if slot.primary_key.contains(&i) {
            return Err(Error::Schema(format!(
                "cannot drop column `{column}`: it is part of the PRIMARY KEY of `{}`",
                slot.name
            )));
        }
        if slot.indexes.iter().any(|ix| ix.columns.contains(&i)) {
            return Err(Error::Schema(format!(
                "cannot drop column `{column}`: it is part of an index/UNIQUE on `{}`",
                slot.name
            )));
        }
        if slot.columns.len() == 1 {
            return Err(Error::Schema(format!(
                "cannot drop the last column of table `{}`",
                slot.name
            )));
        }
        // What a drop genuinely cannot survive is a SURVIVING generated column
        // that READS the dropped one: its expression would name a column that
        // is gone. sqlite refuses that too ("error in table t after drop
        // column"), so this is the same line, not a stricter one.
        //
        // It used to be `slot.has_generated()` — table-wide, and checked before
        // the removal. That refused the ordinary case as well: dropping the
        // table's only generated column, where after the removal there is no
        // program left to renumber and the danger cannot arise. Django's
        // `RemoveField` on a `GeneratedField` is exactly that, and so is the
        // cleanup half of `AddField`.
        //
        // Everything else is mechanical: ordinals shift (`renumber_columns`
        // moves the compiled programs with the other three lists) and the
        // `AS (…)` source needs no rewrite, because a drop renames nothing.
        if let Some(reader) = slot.columns.iter().enumerate().find(|(j, c)| {
            *j != idx
                && c.generated.as_ref().is_some_and(|g| {
                    g.program
                        .instrs
                        .iter()
                        .any(|ins| matches!(ins, Instr::PushCol(x) if *x == i))
                })
        }) {
            return Err(Error::Schema(format!(
                "cannot drop column `{}` of `{}`: the generated column `{}` reads it",
                slot.columns[idx].name, slot.name, reader.1.name
            )));
        }
        slot.columns.remove(idx);
        // Renumber references to columns that shifted down (index > i → -1).
        slot.renumber_columns(|c: &mut u16| {
            if *c > i {
                *c -= 1;
            }
        });
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by RENAMING one column of a table (#47 stage 5). Pure
    /// metadata: the column keeps its position and type, so no row image is
    /// touched. Errors if the column is unknown or the new name collides with a
    /// sibling column.
    ///
    /// `generated_srcs` supplies the `AS (…)` SOURCE text for each generated
    /// column of the table, already rewritten to name `new_name`, as
    /// `(column ordinal, source)`. This crate deliberately holds no SQL lexer,
    /// so the rewrite belongs to the caller — and the caller MUST verify it by
    /// re-compiling each new source against the resulting table and checking
    /// the program is unchanged. A rewrite is only a rename if it means the
    /// same thing; anything else is a schema that computes something new
    /// without saying so. `crates/mpedb/src/ddl_apply.rs::rename_generated_srcs`
    /// is the one place that does both halves.
    pub fn with_renamed_column(
        &self,
        table_id: u32,
        column: &str,
        new_name: &str,
        generated_srcs: &[(u16, String)],
        // Rewritten CHECK sources, same contract as `generated_srcs`: a CHECK
        // names its columns in SOURCE text, the compiled program reads
        // ordinals — so enforcement survived a rename while the text (the DDL
        // a dump replays, the message a violation quotes, the constraint
        // Django's get_constraints reads back) still named the OLD column.
        check_srcs: &[(u16, String)],
    ) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        // The duplicate check must SKIP the column being renamed. Identifier
        // comparison is case-insensitive, so without that exclusion
        // `RENAME COLUMN field TO FiElD` — a case-only rename, which sqlite
        // performs and Django's `test_rename_field_case` migration does —
        // collided with the very column it was renaming. So did renaming a
        // column to its own name, which sqlite treats as a no-op.
        let target = slot.columns.iter().position(|c| ident_eq(&c.name, column));
        if slot
            .columns
            .iter()
            .enumerate()
            .any(|(i, c)| Some(i) != target && ident_eq(&c.name, new_name))
        {
            return Err(Error::Schema(format!(
                "column `{new_name}` already exists in table `{}`",
                slot.name
            )));
        }
        // A generated column's EXPRESSION names its inputs in SOURCE text. The
        // compiled program reads ordinals, so evaluation would keep working
        // after a rename — but the source (the DDL a dump replays, the text an
        // error message quotes) would still name the old column, and the
        // replayed dump would fail. sqlite rewrites the text.
        //
        // mpedb has no expression printer and this crate has no SQL lexer, so
        // the rewritten sources arrive as DATA in `generated_srcs`, keyed by
        // column ordinal. A generated column with no entry is still refused —
        // shipping a schema whose declared form and behaviour disagree is the
        // failure this guard exists for, and silence is not consent.
        for (i, c) in slot.columns.iter().enumerate() {
            if c.generated.is_some() && !generated_srcs.iter().any(|(o, _)| *o as usize == i) {
                return Err(Error::Schema(format!(
                    "cannot rename a column of `{}`: the generated column `{}` names its \
                     inputs as text, and no rewritten expression was supplied",
                    slot.name, c.name
                )));
            }
        }
        for (o, src) in generated_srcs {
            let Some(c) = slot.columns.get_mut(*o as usize) else {
                return Err(Error::Schema(format!(
                    "rewritten generated expression for column ordinal {o}, which \
                     table `{}` does not have",
                    slot.name
                )));
            };
            let Some(g) = &mut c.generated else {
                return Err(Error::Schema(format!(
                    "rewritten generated expression for `{}.{}`, which is not generated",
                    slot.name, c.name
                )));
            };
            g.expr = src.clone();
        }
        for (o, src) in check_srcs {
            let Some(c) = slot.columns.get_mut(*o as usize) else {
                return Err(Error::Schema(format!(
                    "rewritten CHECK for column ordinal {o}, which table `{}` does not have",
                    slot.name
                )));
            };
            if c.check.is_none() {
                return Err(Error::Schema(format!(
                    "rewritten CHECK for `{}.{}`, which has none",
                    slot.name, c.name
                )));
            }
            c.check = Some(src.clone());
        }
        let col = slot
            .columns
            .iter_mut()
            .find(|c| ident_eq(&c.name, column))
            .ok_or_else(|| {
                Error::Schema(format!("no column `{column}` in table `{}`", slot.name))
            })?;
        col.name = new_name.to_string();
        let owner = slot.name.clone();
        // Foreign keys in EVERY table that reference the renamed column BY
        // NAME follow it. `parent_columns` holds names (the parent may not
        // even exist when the key is declared), and `renumber_columns`
        // deliberately leaves them alone — they belong to the parent, and no
        // ordinal moved. But a RENAME is precisely the event that changes what
        // those names mean: without this walk the child's key kept naming a
        // column that was gone, `foreign_key_list` reported it, and the next
        // enforcement lookup failed on a schema that was perfectly healthy.
        // Self-references are covered — the walk includes the owner itself.
        for t in tables.iter_mut().filter(|t| !t.dead) {
            for fk in &mut t.foreign_keys {
                if !ident_eq(&fk.parent, &owner) {
                    continue;
                }
                for pc in &mut fk.parent_columns {
                    if ident_eq(pc, column) {
                        *pc = new_name.to_string();
                    }
                }
            }
        }
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }
}
