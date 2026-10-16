//! Facade DDL application (#47 live DDL). `CREATE TABLE`, `DROP TABLE`, and
//! `ALTER TABLE ... RENAME` do not compile to a [`CompiledPlan`] — they mutate
//! the catalog under the writer lock — so [`Database::query`] intercepts them
//! (via `mpedb_sql::parse_ddl`) and routes here. RLS DDL (CREATE/DROP POLICY,
//! ALTER TABLE ... ROW LEVEL SECURITY) is applied through the policy-store API
//! from [`Database::apply_ddl`].
//!
//! Every path here is one catalog commit (durable + globally visible via the
//! `schema_gen` bump) followed by a best-effort local refresh: the plan-cache
//! clear is infallible and mandatory, and a transient reload failure self-heals
//! at the next statement's `refresh_schema_if_stale` / `gate_cache_on_schema`.

use super::*;
use mpedb_sql::CommentTarget;

mod table_spec;
pub(crate) use table_spec::{
    add_column_from_spec,
    compile_added_column_check,
    fts4_shadow_defs,
    refuse_check_violating_rows,
    rename_check_srcs,
    rename_generated_srcs,
    resolve_index_columns,
    table_def_from_spec,
    virtual_table_def_from_spec,
};

pub(crate) fn add_unique_on_txn(
    txn: &mut mpedb_core::engine::WriteTxn,
    table_id: u32,
    columns: &[String],
    name: Option<String>,
) -> Result<()> {
    let bundle = txn.schema_bundle();
    let table = bundle
        .schema
        .table(table_id)
        .ok_or_else(|| Error::Bind("ALTER TABLE: table no longer exists".into()))?;
    if let Some(name) = &name {
        if table.pk_name.as_ref() == Some(name)
            || table
                .foreign_keys
                .iter()
                .any(|fk| fk.name.as_ref() == Some(name))
            || bundle
                .schema
                .tables
                .iter()
                .any(|t| !t.dead && t.indexes.iter().any(|ix| ix.name.as_ref() == Some(name)))
        {
            return Err(Error::Bind(format!(
                "constraint or index \"{name}\" already exists"
            )));
        }
    }
    let cols = resolve_index_columns(table, &table.name, columns)?;
    let schema = bundle.schema.with_added_index(
        table_id,
        mpedb_types::IndexDef {
            state: mpedb_types::IndexState::Ready,
            columns: cols.clone(),
            unique: true,
            predicate: None,
            name: name.clone(),
            exprs: Vec::new(),
            collations: Vec::new(),
            from_constraint: true,
        },
    )?;
    txn.create_index(table_id, cols, Vec::new(), Vec::new(), true, None, name)?;
    txn.publish_schema(&schema)
}

/// Resolve and validate under the writer lock before publishing any metadata.
/// Existing rows are checked even when ordinary FK enforcement is disabled.
pub(crate) fn add_foreign_key_on_txn(
    txn: &mut mpedb_core::engine::WriteTxn,
    table_id: u32,
    spec: mpedb_sql::ForeignKeySpec,
) -> Result<()> {
    let bundle = txn.schema_bundle();
    let table = bundle
        .schema
        .table(table_id)
        .ok_or_else(|| Error::Bind("ALTER TABLE: table no longer exists".into()))?;
    let columns = spec
        .columns
        .iter()
        .map(|name| {
            table
                .columns
                .iter()
                .position(|c| mpedb_types::ident_eq(&c.name, name))
                .map(|at| at as u16)
                .ok_or_else(|| {
                    Error::Bind(format!(
                        "unknown column \"{name}\" in foreign key definition"
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let foreign_key = mpedb_types::ForeignKeyDef {
        columns,
        parent: spec.parent,
        parent_columns: spec.parent_columns,
        on_delete: spec.on_delete,
        on_update: spec.on_update,
        deferred: spec.deferred,
        name: spec.name,
    };
    let schema = bundle
        .schema
        .with_added_foreign_key(table_id, foreign_key)?;
    let table = schema.table(table_id).expect("validated table");
    let foreign_key = table.foreign_keys.last().expect("added foreign key");
    let resolved = crate::fk::resolve(&schema, table, foreign_key)?;
    let parent = schema.table(resolved.parent_id).expect("resolved parent");
    if foreign_key
        .columns
        .iter()
        .zip(&resolved.parent_cols)
        .any(|(&child, &parent_col)| {
            table.columns[child as usize].ty != parent.columns[parent_col as usize].ty
        })
    {
        return Err(Error::Unsupported(
            "foreign keys between different storage types are not supported".into(),
        ));
    }
    let mut validation_schema = schema.clone();
    validation_schema.tables[table_id as usize].foreign_keys = vec![foreign_key.clone()];
    let violations = {
        let mut ctx = crate::exec::WriteCtx::new(txn, None, None, None);
        crate::fk::check_all(&mut ctx, &validation_schema, Some(table_id))?
    };
    if !violations.is_empty() {
        return Err(crate::fk::violation(&table.name, foreign_key));
    }
    txn.publish_schema(&schema)
}

/// Sys-keyspace prefix for a stored COMMENT.
///
/// The subkey spells the target so the three kinds cannot collide:
/// `cmnt/t/<table>`, `cmnt/c/<table>.<column>`, `cmnt/k/<table>.<name>`. A
/// comment is pure metadata — nothing plans or executes differently for one —
/// so it lives beside views and triggers rather than in the schema's canonical
/// bytes, where it would move `schema_hash` and make documenting a table a
/// format change.
pub(crate) const COMMENT_PREFIX: &[u8] = b"cmnt/";
/// Exclusive upper bound for a `sys_scan_range` over every comment (#124):
/// `/` is 0x2f, so `0` (0x30) is the first subkey past the family.
pub(crate) const COMMENT_PREFIX_END: &[u8] = b"cmnt0";

/// Sys-keyspace prefix for a stored view: `view/<name>` → its SELECT source.
pub(crate) const VIEW_PREFIX: &[u8] = b"view/";
/// Exclusive upper bound for a `sys_scan_range` over the whole view family:
/// `/` is 0x2f, so 0x30 is the first subkey past every `view/…` entry (#124).
pub(crate) const VIEW_PREFIX_END: &[u8] = b"view0";

impl Database {
    /// Persisted PostgreSQL enum definitions, including types with no columns.
    pub fn list_pg_enums(&self) -> Result<Vec<mpedb_types::pgtype::PgEnum>> {
        let r = self.engine.begin_read()?;
        r.sys_scan_range(b"pg_enum/", b"pg_enum0")?
            .into_iter()
            .map(|(_, bytes)| mpedb_types::pgtype::PgEnum::decode(&bytes))
            .collect()
    }

    pub(crate) fn parse_ddl(&self, sql: &str) -> Result<Option<mpedb_sql::DdlStmt>> {
        // Ordinary statements and built-in DDL need no enum registry scan.
        // Only a DDL parse that could name a custom type retries with the
        // persisted definitions (unknown names remain explicit errors).
        match mpedb_sql::parse_ddl_dialect(sql, self.dialect) {
            Err(original) if self.dialect == mpedb_types::Dialect::Postgres => {
                let enums = self.list_pg_enums()?;
                if enums.is_empty() { return Err(original); }
                mpedb_sql::parse_ddl_with_enums(sql, self.dialect, &enums)
            }
            result => result,
        }
    }

    fn apply_enum(
        &self,
        name: &str,
        labels: Option<Vec<String>>,
        if_exists: bool,
    ) -> Result<ExecResult> {
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let key = format!("pg_enum/{name}");
        let old = w.sys_get(key.as_bytes())?;
        if let Some(labels) = labels {
            if old.is_some() {
                return Err(Error::Bind(format!("type \"{name}\" already exists")));
            }
            if mpedb_types::pgtype::by_name(name).is_some()
                || w.schema_bundle().schema.table_id(name).is_some()
            {
                return Err(Error::Bind(format!("type \"{name}\" already exists")));
            }
            let next = match w.sys_get(b"pg_enum_oid_next")? {
                Some(bytes) => u64::from_le_bytes(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Corrupt("invalid enum OID counter".into()))?,
                ),
                None => {
                    let mut next = 1u64 << 31;
                    for (_, bytes) in w.sys_scan_range(b"pg_enum/", b"pg_enum0")? {
                        let existing = mpedb_types::pgtype::PgEnum::decode(&bytes)?;
                        next = next.max(u64::from(existing.oid) + 4096);
                    }
                    next
                }
            };
            if !((1u64 << 31)..=(1u64 << 32)).contains(&next) || next & 4095 != 0 {
                return Err(Error::Corrupt("invalid enum OID counter".into()));
            }
            if next == (1u64 << 32) {
                return Err(Error::Unsupported("enum OID space exhausted".into()));
            }
            let e = mpedb_types::pgtype::PgEnum {
                name: name.into(),
                oid: next as u32,
                labels,
            };
            e.validate()?;
            w.sys_put(key.as_bytes(), &e.encode())?;
            w.sys_put(b"pg_enum_oid_next", &(next + 4096).to_le_bytes())?;
        } else {
            if old.is_none() {
                if if_exists {
                    w.abort();
                    return Ok(ExecResult::Affected(0));
                }
                return Err(Error::Bind(format!("type \"{name}\" does not exist")));
            }
            if w.schema_bundle().schema.live_tables().any(|t| {
                t.columns
                    .iter()
                    .any(|c| c.pg_enum.as_ref().is_some_and(|e| e.name == name))
            }) {
                return Err(Error::Bind(format!(
                    "cannot drop type \"{name}\" because columns depend on it"
                )));
            }
            w.sys_delete(key.as_bytes())?;
        }
        w.bump_schema_gen();
        w.commit()?;
        self.cache.write().expect(POISON).clear();
        self.engine.reload_schema_from_catalog()?;
        Ok(ExecResult::Affected(0))
    }

    /// Reserve a key from the owned SERIAL generator. This is a short,
    /// independently committed allocation: the caller's later rollback does
    /// not return the key. It shares the AUTOINCREMENT high-water with direct
    /// inserts, preventing a reserved id from being assigned a second time.
    ///
    /// This is the rowid-alias model, not a standalone PostgreSQL sequence:
    /// explicit ids also advance the high-water. Never call while holding a
    /// WriteSession on this database.
    pub fn next_serial_value(&self, sequence: &str, namespace: Option<&str>) -> Result<i64> {
        if !matches!(
            namespace,
            None | Some("public") | Some("pg_temp") | Some("pg_temp_1")
        ) {
            return Err(Error::Bind(format!(
                "sequence \"{sequence}\" does not exist"
            )));
        }
        if namespace != Some("public") {
            let guard = self.attached.read().expect(POISON);
            if let Some(i) = guard.find("temp") {
                let db = &guard.members[i].db;
                db.engine.refresh_schema_if_stale()?;
                if owned_serial_table(&db.schema().schema, sequence).is_some() {
                    return db.next_serial_value(sequence, Some("public"));
                }
            }
            if namespace.is_some() {
                return Err(Error::Bind(format!(
                    "sequence \"{sequence}\" does not exist"
                )));
            }
        }
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let bundle = w.schema_bundle();
        let id = owned_serial_table(&bundle.schema, sequence)
            .ok_or_else(|| Error::Bind(format!("sequence \"{sequence}\" does not exist")))?;
        if !bundle.schema.table(id).is_some_and(|t| t.autoincrement) {
            return Err(Error::Unsupported("nextval on a legacy SERIAL table needs a monotonic owned generator; recreate/migrate the table".into()));
        }
        let key = mpedb_core::engine::rowid_seq_key(id);
        if let Some(bytes) = w.sys_get(&key)? {
            let stored = i64::from_le_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid SERIAL high-water".into()))?,
            );
            if stored == i64::MAX {
                return Err(Error::ArithmeticOverflow);
            }
        }
        let next = w.next_rowid(id)?;
        w.commit()?;
        Ok(next)
    }

    /// Load every stored view (`view/<name>` → SELECT source) into a catalog.
    /// Cheap when there are none. Cached by the facade behind the schema-gen
    /// gate — views change only on a DDL commit, which bumps `schema_gen`.
    ///
    /// **Prefix-bounded, never a whole-keyspace scan (#124).** This runs on
    /// EVERY compile, and the sys keyspace it shares is where the plan registry
    /// lives — up to `MAX_REGISTRY_PLANS` records carrying full SQL text plus an
    /// encoded plan blob each. Scanning the whole region and filtering made
    /// compilation cost O(bytes ever registered): 297 B held and 0.24 µs per
    /// previously-registered plan, i.e. a 1.2 MB / 1.0 ms compile on a database
    /// with a full registry, for a statement that touches none of it.
    pub(crate) fn load_view_catalog(&self) -> Result<mpedb_sql::ViewCatalog> {
        let mut cat = mpedb_sql::ViewCatalog::new();
        for (name, src) in self.list_views()? {
            cat.insert(name, src);
        }
        // Connection-local temp views shadow main's, which is why they are
        // merged last.
        self.merge_temp_views(&mut cat);
        Ok(cat)
    }

    /// Every stored view as `(name, select_source)` — used by the C-API
    /// `sqlite_master` dump and by [`load_view_catalog`].
    /// Every AUTOINCREMENT table's high-water mark as `(table name, last id)`
    /// — sqlite's `sqlite_sequence` contents.
    ///
    /// Only tables that have HANDED OUT an id appear, which is sqlite's rule
    /// too: the row exists once the first id is assigned, not when the table is
    /// created.
    pub fn rowid_sequences(&self) -> Result<Vec<(String, i64)>> {
        let bundle = self.schema();
        let r = self.engine.begin_read()?;
        let mut out = Vec::new();
        for t in bundle.tables.iter().filter(|t| !t.dead && t.autoincrement) {
            let key = mpedb_core::engine::rowid_seq_key(t.id);
            if let Ok(Some(b)) = r.sys_get(&key) {
                if b.len() == 8 {
                    let v = i64::from_le_bytes(b[..8].try_into().expect("len 8"));
                    out.push((t.name.clone(), v));
                }
            }
        }
        r.finish()?;
        Ok(out)
    }

    /// The autocommit twin of [`WriteSession::set_rowid_sequences`]: all N
    /// counter writes in ONE transaction — never one commit per name, which
    /// would let a crash land a flush half-reset.
    pub fn set_rowid_sequences(&self, updates: &[(u32, Option<i64>)]) -> Result<()> {
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        for (id, v) in updates {
            let key = mpedb_core::engine::rowid_seq_key(*id);
            let r = match v {
                Some(v) => w.sys_put(&key, &v.to_le_bytes()),
                None => w.sys_delete(&key).map(|_| ()),
            };
            if let Err(e) = r {
                w.abort();
                return Err(e);
            }
        }
        w.commit()?;
        Ok(())
    }

    pub fn list_views(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan_range(VIEW_PREFIX, VIEW_PREFIX_END);
        r.finish()?;
        for (subkey, value) in scan? {
            if let Some(name) = subkey.strip_prefix(VIEW_PREFIX) {
                let name = String::from_utf8_lossy(name).into_owned();
                let src = String::from_utf8_lossy(&value).into_owned();
                out.push((name, src));
            }
        }
        Ok(out)
    }

    /// `CREATE VIEW [IF NOT EXISTS] <name> AS <select>`. Stores the SELECT
    /// source under `view/<name>` and bumps the schema gen so peers reload.
    /// Refuses a name already taken by a table or (unless IF NOT EXISTS) a view.
    pub(crate) fn apply_create_view(
        &self,
        name: &str,
        select_sql: &str,
        if_not_exists: bool,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        if self.engine.schema().schema.table_id(name).is_some() {
            return Err(Error::Bind(format!(
                "CREATE VIEW: `{name}` is already a table"
            )));
        }
        let key = view_key(name);
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let exists = resolve_view_key(&mut w, name)?.is_some();
        if exists {
            w.abort();
            if if_not_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!("CREATE VIEW: view `{name}` already exists")));
        }
        let res = (|| {
            w.sys_put(&key, select_sql.as_bytes())?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// The sys-keyspace subkey a comment target is stored under.
    ///
    /// Lower-cased, because every other name lookup in mpedb is
    /// case-insensitive and a comment that could only be read back with the
    /// same capitalisation would be a trap rather than a feature.
    pub(crate) fn comment_key(target: &CommentTarget) -> Vec<u8> {
        let mut k = COMMENT_PREFIX.to_vec();
        match target {
            CommentTarget::Table(t) => {
                k.extend_from_slice(b"t/");
                k.extend_from_slice(t.to_lowercase().as_bytes());
            }
            CommentTarget::Column { table, column } => {
                k.extend_from_slice(b"c/");
                k.extend_from_slice(table.to_lowercase().as_bytes());
                k.push(b'.');
                k.extend_from_slice(column.to_lowercase().as_bytes());
            }
            CommentTarget::Constraint { table, name } => {
                k.extend_from_slice(b"k/");
                k.extend_from_slice(table.to_lowercase().as_bytes());
                k.push(b'.');
                k.extend_from_slice(name.to_lowercase().as_bytes());
            }
        }
        k
    }

    /// `COMMENT ON <target> IS { 'text' | NULL }`.
    ///
    /// The TABLE must exist, for both the table and the column form: a comment
    /// on nothing is storage no reflection could ever return, and accepting it
    /// would make `COMMENT ON TABLE typo IS '…'` a silent success. The COLUMN
    /// is checked too, for the same reason.
    ///
    /// A CONSTRAINT name is NOT checked. mpedb does not carry a named-check
    /// catalog that could answer "does this constraint exist", and refusing
    /// every constraint comment because of that would reject the one form ORMs
    /// emit most. The comment is stored against `(table, name)` and read back
    /// by the same key — write-and-read-back is honest even when the name
    /// cannot be validated, and the table it hangs off IS checked.
    pub(crate) fn apply_comment(
        &self,
        target: &CommentTarget,
        text: Option<&str>,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        {
            let bundle = self.engine.schema();
            let (table, column) = match target {
                CommentTarget::Table(t) => (t, None),
                CommentTarget::Column { table, column } => (table, Some(column)),
                CommentTarget::Constraint { table, .. } => (table, None),
            };
            let Some(id) = bundle.schema.table_id(table) else {
                return Err(Error::Bind(format!("COMMENT ON: no such table `{table}`")));
            };
            if let Some(col) = column {
                let t = bundle.schema.table(id).expect("table_id returned a live id");
                if !t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(col)) {
                    return Err(Error::Bind(format!(
                        "COMMENT ON COLUMN: no such column `{col}` in table `{table}`"
                    )));
                }
            }
        }
        let key = Self::comment_key(target);
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            match text {
                // `IS NULL` is PostgreSQL's spelling of "remove the comment";
                // storing the absence would make an empty comment and a
                // removed one indistinguishable on read. Deleting something
                // that was never there is a no-op, not an error — PostgreSQL
                // accepts it too.
                None => {
                    w.sys_delete(&key)?;
                }
                Some(t) => w.sys_put(&key, t.as_bytes())?,
            };
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// Every stored comment as `(subkey-after-the-prefix, text)`.
    ///
    /// Prefix-bounded like `list_views`, and for the same reason (#124): this
    /// shares the sys keyspace with the plan registry, so an unbounded scan
    /// would cost O(bytes ever registered).
    pub fn list_comments(&self) -> Result<Vec<(String, String)>> {
        let r = self.engine.begin_read()?;
        let scan = r.sys_scan_range(COMMENT_PREFIX, COMMENT_PREFIX_END);
        r.finish()?;
        Ok(scan?
            .into_iter()
            .filter_map(|(subkey, value)| {
                subkey.strip_prefix(COMMENT_PREFIX).map(|rest| {
                    (
                        String::from_utf8_lossy(rest).into_owned(),
                        String::from_utf8_lossy(&value).into_owned(),
                    )
                })
            })
            .collect())
    }

    /// `DROP VIEW [IF EXISTS] <name>`.
    /// `DROP INDEX [IF EXISTS] <name>` — resolved by the index's stored name
    /// (canonical-bytes v11), which is the only handle a user has: mpedb
    /// indexes are POSITIONAL, so before names were persisted this statement
    /// could not even be parsed.
    pub(crate) fn apply_drop_index(&self, name: &str, if_exists: bool) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let found = self.engine.schema().schema.find_index_by_name(name);
        let Some((table_id, pos)) = found else {
            if if_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!("DROP INDEX: no such index `{name}`")));
        };
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.drop_index(table_id, pos) {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        Ok(ExecResult::Affected(0))
    }

    pub(crate) fn apply_drop_view(&self, name: &str, if_exists: bool) -> Result<ExecResult> {
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let found = resolve_view_key(&mut w, name)?;
        let key = found.clone().unwrap_or_else(|| view_key(name));
        let existed = found.is_some();
        if !existed {
            w.abort();
            if if_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!("DROP VIEW: no such view `{name}`")));
        }
        let res = (|| {
            w.sys_delete(&key)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }
}

fn owned_serial_table(schema: &mpedb_types::Schema, sequence: &str) -> Option<u32> {
    schema.tables.iter().filter(|t| !t.dead).find_map(|t| {
        let col = t.rowid_alias_col()?;
        let column = t.columns.get(col as usize)?;
        if !column.decl.as_deref().is_some_and(mpedb_types::pgtype::is_serial) { return None; }
        (format!("{}_{}_seq", t.name, column.name) == sequence).then_some(t.id)
    })
}

fn view_key(name: &str) -> Vec<u8> {
    view_key_public(name)
}

/// Sys-key for a stored view — shared by autocommit and in-txn CREATE VIEW.
pub(crate) fn view_key_public(name: &str) -> Vec<u8> {
    let mut k = VIEW_PREFIX.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

/// Does a view named `name` exist on this write txn (ASCII-case-insensitive)?
pub(crate) fn view_exists_on_txn(
    w: &mut mpedb_core::WriteTxn,
    name: &str,
) -> Result<bool> {
    Ok(resolve_view_key(w, name)?.is_some())
}

impl crate::WriteSession<'_> {
    /// The AUTOINCREMENT high-waters visible in THIS txn — committed rows plus
    /// ids this still-open transaction has handed out. The C-API's
    /// `sqlite_sequence` read must come through here when a txn is open: a
    /// fresh read snapshot cannot see the open txn's inserts (the same cure as
    /// the FK pragma's `foreign_key_check`).
    pub fn rowid_sequences(&mut self) -> crate::Result<Vec<(String, i64)>> {
        let bundle = self.txn.schema_bundle();
        let mut out = Vec::new();
        for t in bundle.schema.tables.iter().filter(|t| !t.dead && t.autoincrement) {
            let key = mpedb_core::engine::rowid_seq_key(t.id);
            if let Some(b) = self.txn.sys_get(&key)? {
                if b.len() == 8 {
                    let v = i64::from_le_bytes(b[..8].try_into().expect("len 8"));
                    out.push((t.name.clone(), v));
                }
            }
        }
        Ok(out)
    }

    /// Write the AUTOINCREMENT counters the C-API's `sqlite_sequence` write
    /// arm resolved — N tables in THIS one open txn, atomic with whatever
    /// else the txn holds (a Django flush runs its resets inside the same
    /// transaction as its deletes). `Some(v)` stores v VERBATIM — sqlite
    /// keeps a low seq as written and silently corrects at the next
    /// allocation, and so does mpedb: `next_rowid` takes
    /// max(counter, tree max) + 1 already, so a lowered counter can never
    /// hand out a live id. `None` deletes the record, which IS sqlite's
    /// `DELETE FROM sqlite_sequence` (record existence = row existence).
    pub fn set_rowid_sequences(&mut self, updates: &[(u32, Option<i64>)]) -> crate::Result<()> {
        for (id, v) in updates {
            let key = mpedb_core::engine::rowid_seq_key(*id);
            match v {
                Some(v) => self.txn.sys_put(&key, &v.to_le_bytes())?,
                None => {
                    self.txn.sys_delete(&key)?;
                }
            }
        }
        Ok(())
    }
}

/// Every view visible through this write txn (for mid-transaction iterdump).
pub(crate) fn list_views_on_txn(
    w: &mut mpedb_core::WriteTxn,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (subkey, value) in w.sys_scan_range(VIEW_PREFIX, VIEW_PREFIX_END)? {
        if let Some(name) = subkey.strip_prefix(VIEW_PREFIX) {
            let name = String::from_utf8_lossy(name).into_owned();
            let src = String::from_utf8_lossy(&value).into_owned();
            out.push((name, src));
        }
    }
    Ok(out)
}

/// The sys-key of an existing view, if any (ASCII-case-insensitive).
pub(crate) fn resolve_view_key_on_txn(
    w: &mut mpedb_core::WriteTxn,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    resolve_view_key(w, name)
}

/// The sys-key of the stored view that `name` names, matched
/// ASCII-case-insensitively — `DROP VIEW v` finds `CREATE VIEW V`.
///
/// The key keeps the DECLARED spelling (`view/V`), so the name a view reports
/// back is the one it was created with; only the *matching* folds. Resolved
/// from inside the caller's write txn, so the existence test and the
/// put/delete that follows it are one atomic decision.
fn resolve_view_key(w: &mut mpedb_core::WriteTxn<'_>, name: &str) -> Result<Option<Vec<u8>>> {
    for (subkey, _) in w.sys_scan_range(VIEW_PREFIX, VIEW_PREFIX_END)? {
        let Some(stored) = subkey.strip_prefix(VIEW_PREFIX) else { continue };
        if mpedb_types::ident_eq(&String::from_utf8_lossy(stored), name) {
            return Ok(Some(subkey));
        }
    }
    Ok(None)
}

impl Database {
    /// `CREATE TABLE` (#47 stage 2/3): build the [`TableDef`] from the parsed
    /// spec, append it to the schema in one catalog commit (the engine
    /// validates the merged set and seeds the empty tree roots), then swap this
    /// process's schema bundle and drop the local plan cache. Other processes
    /// reload at their next transaction via the schema-gen bump.
    pub(crate) fn apply_create_table(&self, spec: mpedb_sql::CreateTableSpec) -> Result<ExecResult> {
        // `IF NOT EXISTS`: an existing table of this name makes the statement a
        // no-op. Checked on the LIVE catalog before the write txn, the same way
        // `CREATE INDEX`'s idempotence is.
        if spec.if_not_exists
            && self
                .schema()
                .tables
                .iter()
                .any(|t| !t.dead && mpedb_types::ident_eq(&t.name, &spec.name))
        {
            return Ok(ExecResult::Affected(0));
        }
        let def = table_def_from_spec(spec, &self.host_udf_set())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.create_table(def) {
            Ok(_tid) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        // The table is now DURABLE and visible to every process (the
        // schema_gen bump). Refreshing THIS process's view is best-effort:
        // dropping the plan cache is infallible and must always happen, but
        // a transient reload failure must NOT report the durable CREATE as
        // failed — the next statement's `refresh_schema_if_stale` (in
        // `compile_maybe_explain`) self-heals the bundle (review finding).
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `CREATE VIRTUAL TABLE … USING fts5(cols [, tokenize=…])` (design/DESIGN-FTS.md
    /// §1). Builds a `TableKind::Fts` table — an auto `rowid` INTEGER primary key
    /// plus the declared columns as tokenized TEXT content — and appends it to
    /// the schema in one catalog commit, exactly like `CREATE TABLE`. The engine
    /// seeds the extra inverted-index tree; row-level maintenance keeps it live.
    pub(crate) fn apply_create_virtual_table(
        &self,
        spec: mpedb_sql::CreateVirtualTableSpec,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        if self.engine.schema().schema.table_id(&spec.name).is_some() {
            if spec.if_not_exists {
                return Ok(ExecResult::Affected(0));
            }
            return Err(Error::Bind(format!(
                "CREATE VIRTUAL TABLE: `{}` already exists",
                spec.name
            )));
        }
        let shadows = matches!(spec.module, mpedb_types::FtsModule::Fts4)
            .then(|| fts4_shadow_defs(&spec.name, &spec.columns));
        let def = virtual_table_def_from_spec(spec)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        // One txn for the vtab AND (fts4) its five shadows: a name collision
        // on ANY of them aborts the whole create — sqlite refuses the same
        // way, and half a shadow set is not a state the dump may ever see.
        let res = (|| {
            w.create_table(def)?;
            for sh in shadows.into_iter().flatten() {
                w.create_table(sh)?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `DROP TABLE a, b, c [CASCADE|RESTRICT]`.
    ///
    /// Names are RESOLVED FIRST, all of them, before anything is dropped. A
    /// missing name in the middle would otherwise leave the ones before it
    /// gone and the ones after it standing, and a half-applied DROP is the one
    /// outcome a caller cannot reason about. (Each name still commits on its
    /// own — this is the autocommit path — so the guarantee is "nothing starts
    /// unless every name resolves", not full atomicity. The `WriteSession`
    /// path IS atomic; see `Database::drop_one_table`.)
    ///
    /// The list is also what the orphan check has to see: `DROP TABLE parent,
    /// child` is legal in PostgreSQL because the child is going too, and
    /// checking each name against the whole schema would refuse it on the
    /// parent. The set is passed down so a child inside it does not block.
    pub(crate) fn apply_drop_tables(
        &self,
        names: &[String],
        if_exists: bool,
        cascade: bool,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let mut ids = Vec::with_capacity(names.len());
        {
            let bundle = self.engine.schema();
            for n in names {
                match bundle.schema.table_id(n) {
                    Some(id) => ids.push(id),
                    None if if_exists => {}
                    None => {
                        return Err(Error::Bind(format!("DROP TABLE: no such table `{n}`")))
                    }
                }
            }
        }
        for n in names {
            // Re-checked per name: an earlier drop in this same list bumped
            // the schema gen, so the view taken above is stale by now for
            // everything except the question it answered.
            if self.engine.schema().schema.table_id(n).is_none() {
                continue;
            }
            self.apply_drop_table(n, if_exists, cascade, &ids)?;
        }
        Ok(ExecResult::Affected(0))
    }

    pub(crate) fn apply_drop_table(
        &self,
        name: &str,
        if_exists: bool,
        cascade: bool,
        also_dropping: &[u32],
    ) -> Result<ExecResult> {
        // Resolve the name against a fresh schema view (another process may have
        // created/dropped since our last statement). The write txn re-checks the
        // gen and `drop_table` re-validates the id against its own captured
        // bundle, so a lost race surfaces as a clean error, never corruption.
        self.engine.refresh_schema_if_stale()?;
        let id = match self.engine.schema().schema.table_id(name) {
            Some(id) => id,
            None => {
                if if_exists {
                    return Ok(ExecResult::Affected(0));
                }
                return Err(Error::Bind(format!("DROP TABLE: no such table `{name}`")));
            }
        };
        // With enforcement on, sqlite treats DROP TABLE as deleting every row
        // (measured, 3.45.1: dropping a parent with live children fails), so a
        // table something still points at cannot go. Checked here rather than
        // per row because the answer is the same for all of them.
        // `CASCADE` says drop it anyway. PostgreSQL means "drop the dependent
        // CONSTRAINT too"; mpedb leaves the child's key definition dangling,
        // which is sqlite's answer and the one that already happens when
        // enforcement is off — the child's next write says `no such table`.
        if self.fk_enforced() && !cascade {
            let bundle = self.engine.schema();
            let sc = &bundle.schema;
            if let Some(t) = sc.tables.iter().find(|t| t.id == id && !t.dead) {
                for other in sc
                    .tables
                    .iter()
                    .filter(|o| !o.dead && o.id != id && !also_dropping.contains(&o.id))
                {
                    for fk in &other.foreign_keys {
                        if !fk.parent.eq_ignore_ascii_case(&t.name) {
                            continue;
                        }
                        // Only a table with ROWS in it blocks — an empty child
                        // has nothing dangling, which is what sqlite reports too.
                        let r = self.engine.begin_read()?;
                        let rows = r.row_count(other.id);
                        r.finish()?;
                        if rows? > 0 {
                            return Err(crate::fk::violation(&other.name, fk));
                        }
                    }
                }
            }
        }
        // An fts4 vtab owns its five shadow tables: DROP takes all six in ONE
        // commit (gated on the MODULE tag — never guessed from names for
        // Standard/fts5 tables). Resolved before the write txn opens.
        let shadow_ids: Vec<u32> = {
            let bundle = self.engine.schema();
            let sc = &bundle.schema;
            match sc.tables.iter().find(|t| t.id == id && !t.dead).map(|t| t.kind) {
                Some(mpedb_types::TableKind::Fts {
                    module: mpedb_types::FtsModule::Fts4,
                    ..
                }) => ["_content", "_docsize", "_segdir", "_segments", "_stat"]
                    .iter()
                    .filter_map(|sfx| sc.table_id(&format!("{name}{sfx}")))
                    .collect(),
                _ => Vec::new(),
            }
        };
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        // Cascade: a dropped table's triggers are dead — remove their records in
        // the same commit (DESIGN-TRIGGERS §3.1).
        let res = (|| {
            crate::trigger::cascade_drop_triggers(&mut w, id)?;
            w.drop_table(id)?;
            for sid in shadow_ids {
                crate::trigger::cascade_drop_triggers(&mut w, sid)?;
                w.drop_table(sid)?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        // Durable and globally visible via the schema_gen bump. Refreshing this
        // process is best-effort for the same reason as CREATE: the plan cache
        // clear is infallible and mandatory (cached plans reference the dropped
        // table's id), and a transient reload failure self-heals at the next
        // statement's `refresh_schema_if_stale`.
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `ALTER TABLE ... RENAME` (#47 stage 5). Pure schema metadata — resolve
    /// the table id against a fresh view, apply the rename in one commit, then
    /// (best-effort, like CREATE/DROP) clear the plan cache and reload. `rename`
    /// runs the txn method that computes+publishes from the txn's own bundle.
    pub(crate) fn apply_alter_rename(
        &self,
        table: &str,
        rename: impl FnOnce(&mut mpedb_core::engine::WriteTxn, u32) -> Result<()>,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let id = self
            .engine
            .schema()
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{table}`")))?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match rename(&mut w, id) {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `ALTER TABLE ... ADD COLUMN` (#47 stage 5). A NULLABLE column fills
    /// existing rows with NULL; `DEFAULT <const>` fills them with the constant,
    /// which also makes `NOT NULL DEFAULT <const>` legal (the fill value is
    /// non-NULL) and is persisted so later INSERTs omitting the column get it.
    /// Still refused, matching sqlite: NOT NULL *without* a non-NULL default
    /// (no value for existing rows), and UNIQUE / PRIMARY KEY on ADD (would need
    /// an online index build; sqlite refuses these outright). The DEFAULT const
    /// is type-checked against the column type (rigid schema). The engine
    /// rewrites existing rows in one commit.
    pub(crate) fn apply_alter_add_column(
        &self,
        table: &str,
        spec: mpedb_sql::CreateColumnSpec,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let (id, def) = bundle
            .schema
            .table_id(table)
            .and_then(|id| bundle.schema.table(id).map(|t| (id, t)))
            .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{table}`")))?;
        let (col, fill) = add_column_from_spec(def, spec)?;
        let chk = compile_added_column_check(&bundle.schema, id, &col)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let applied = match &chk {
            Some(c) => refuse_check_violating_rows(&mut w, id, c, &fill)
                .and_then(|()| w.alter_add_column(id, col, fill)),
            None => w.alter_add_column(id, col, fill),
        };
        match applied {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `ALTER TABLE ... DROP COLUMN` (#47 stage 5). The engine refuses dropping a
    /// PK / indexed / last column and rewrites existing rows without the column
    /// in one commit.
    pub(crate) fn apply_alter_drop_column(
        &self,
        table: &str,
        column: &str,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let id = self
            .engine
            .schema()
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{table}`")))?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.alter_drop_column(id, column) {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// `CREATE [UNIQUE] INDEX … ON t (cols)`. Resolves the columns, treats an
    /// identical existing index as a no-op (idempotent — covers `IF NOT
    /// EXISTS`), then builds the index over the existing rows in one commit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_create_index(
        &self,
        table: &str,
        columns: &[String],
        // Parallel to `columns` when non-empty; see `IndexDef::exprs`.
        exprs: &[Option<String>],
        // Parallel to `columns` when non-empty: a per-part `COLLATE` override.
        collations: &[Option<mpedb_types::Collation>],
        unique: bool,
        predicate: Option<String>,
        name: Option<String>,
        if_not_exists: bool,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let id = bundle
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("CREATE INDEX: no such table `{table}`")))?;
        let t = bundle.schema.table(id).expect("table_id resolved");
        // An expression part has no column to resolve; it takes the sentinel,
        // and the source is compiled against the table before anything is
        // written, so a key that cannot be evaluated is refused at the DDL
        // rather than at the first INSERT.
        let mut cols = Vec::with_capacity(columns.len());
        for (i, name) in columns.iter().enumerate() {
            match exprs.get(i).and_then(Option::as_ref) {
                None => cols.push(resolve_index_columns(t, table, std::slice::from_ref(name))?[0]),
                Some(src) => {
                    mpedb_sql::compile_value_expr(src, t).map_err(|e| {
                        Error::Schema(format!(
                            "CREATE INDEX on `{table}`: key expression `{src}` does not \
                             compile against the table: {e}"
                        ))
                    })?;
                    cols.push(mpedb_types::INDEX_EXPR_COL);
                }
            }
        }
        // A NAMED index is identified by its NAME, not by its shape.
        //
        // This used to be "idempotent by shape": any index with the same
        // columns/uniqueness/predicate/expressions was treated as already
        // present and the statement became a silent no-op. That made
        // `CREATE INDEX` LIE in two directions at once, both of which Django's
        // migrations walk into:
        //
        //   * `CREATE UNIQUE INDEX u ON t(c)` where `c` is already UNIQUE
        //     reported success and created nothing, so the following
        //     `DROP INDEX u` failed with "no such index" — which is exactly
        //     `remove_unique_together` on a unique field;
        //   * a duplicate NAME with a different shape was accepted and
        //     ignored, where sqlite says "index u already exists".
        //
        // Shape-based idempotence survives only for an UNNAMED index, which
        // has no name to collide on.
        match &name {
            Some(n) => {
                if t.indexes.iter().any(|ix| {
                    ix.name.as_deref().is_some_and(|e| mpedb_types::ident_eq(e, n))
                }) {
                    if if_not_exists {
                        return Ok(ExecResult::Affected(0));
                    }
                    return Err(Error::Bind(format!("index `{n}` already exists")));
                }
            }
            None => {
                if t.indexes.iter().any(|ix| {
                    ix.columns == cols && ix.unique == unique && ix.predicate == predicate
                        && ix.exprs == exprs && ix.collations == collations
                }) {
                    return Ok(ExecResult::Affected(0));
                }
            }
        }
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.create_index(id, cols, exprs.to_vec(), collations.to_vec(), unique, predicate, name) {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(ExecResult::Affected(0))
    }

    /// Apply a parsed DDL statement. Table DDL routes to the dedicated appliers
    /// above; RLS DDL (CREATE/DROP POLICY, ALTER TABLE ... ROW LEVEL SECURITY)
    /// takes the writer lock once and bumps the table's policy epoch. Returns
    /// `Affected(0)` (RLS DDL touches no user rows; a policy lint may return
    /// warning rows).
    pub(crate) fn apply_ddl(&self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::{DdlStmt, RlsAction};
        match ddl {
            DdlStmt::CreateEnum { name, labels } => return self.apply_enum(&name, Some(labels), false),
            DdlStmt::DropEnum { name, if_exists } => return self.apply_enum(&name, None, if_exists),
            DdlStmt::CreateTable(spec) => {
                return self.apply_create_table(spec);
            }
            DdlStmt::CreateVirtualTable(spec) => {
                return self.apply_create_virtual_table(spec);
            }
            // The SAME path `mpedb fn define <target> f.sql` takes: compile the
            // whole statement through the plpgsql frontend, store it
            // content-addressed, bump the schema generation. Routing it here
            // rather than duplicating the store is what makes a `pg_dump`
            // replayed as SQL and a file handed to the CLI produce byte-
            // identical stored functions.
            DdlStmt::DropFunction { name, if_exists } => {
                let found = self.drop_function(&name)?;
                if !found && !if_exists {
                    return Err(Error::Bind(format!(
                        "DROP FUNCTION: no stored function named `{name}`"
                    )));
                }
                return Ok(ExecResult::Affected(0));
            }
            DdlStmt::CreateFunction { source, .. } => {
                let (_name, _hash) =
                    self.create_function(crate::spellfn::SpellLang::PlPgSql, &source)?;
                return Ok(ExecResult::Affected(0));
            }
            DdlStmt::DropTable { names, if_exists, cascade } => {
                return self.apply_drop_tables(&names, if_exists, cascade);
            }
            DdlStmt::AlterRenameTable { table, new_name } => {
                return self.apply_alter_rename(&table, |w, id| w.alter_rename_table(id, &new_name));
            }
            DdlStmt::AlterRenameColumn { table, column, new_name } => {
                let (srcs, chk_srcs) = {
                    let bundle = self.schema();
                    let id = bundle.schema.table_id(&table).ok_or_else(|| {
                        Error::Bind(format!("ALTER TABLE: no such table `{table}`"))
                    })?;
                    let t = bundle.schema.table(id).expect("table_id resolved");
                    (
                        rename_generated_srcs(t, &column, &new_name)?,
                        rename_check_srcs(t, &column, &new_name)?,
                    )
                };
                return self.apply_alter_rename(&table, |w, id| {
                    w.alter_rename_column(id, &column, &new_name, &srcs, &chk_srcs)
                });
            }
            DdlStmt::AlterAddColumn { table, column } => {
                return self.apply_alter_add_column(&table, column);
            }
            DdlStmt::AlterAddForeignKey { table, foreign_key } => {
                return self.apply_alter_rename(&table, |txn, id| {
                    add_foreign_key_on_txn(txn, id, foreign_key)
                });
            }
            DdlStmt::AlterAddUnique { table, columns, name } => {
                return self.apply_alter_rename(&table, |txn, id| {
                    add_unique_on_txn(txn, id, &columns, name)
                });
            }
            DdlStmt::AlterDropColumn { table, column } => {
                return self.apply_alter_drop_column(&table, &column);
            }
            DdlStmt::CreateIndex {
                name,
                table,
                columns,
                exprs,
                collations,
                unique,
                where_clause,
                if_not_exists,
                ..
            } => {
                return self.apply_create_index(
                    &table, &columns, &exprs, &collations, unique, where_clause, Some(name),
                    if_not_exists,
                );
            }
            DdlStmt::DropIndex { name, if_exists } => {
                return self.apply_drop_index(&name, if_exists);
            }
            DdlStmt::Comment { target, text } => {
                return self.apply_comment(&target, text.as_deref());
            }
            DdlStmt::CreateView { name, select_sql, if_not_exists } => {
                return self.apply_create_view(&name, &select_sql, if_not_exists);
            }
            DdlStmt::DropView { name, if_exists } => {
                return self.apply_drop_view(&name, if_exists);
            }
            DdlStmt::CreatePolicy(spec) => {
                let def = mpedb_types::PolicyDef {
                    name: spec.name,
                    command: spec.command,
                    permissive: spec.permissive,
                    using_src: spec.using_src,
                    check_src: spec.check_src,
                };
                // Lint BEFORE creating, but never block on it (§6.4): a leaky
                // unique key is a design smell the author may have accepted, not
                // something the database gets to veto. Findings come back as rows
                // so they print through the ordinary result path — a lint nobody
                // sees is worthless, and a library must not print for its caller.
                let findings = self.lint_policy(&spec.table, &def)?;
                self.create_policy(&spec.table, &def)?;
                if !findings.is_empty() {
                    return Ok(ExecResult::Rows {
                        columns: vec!["warning".into()],
                        rows: findings.into_iter().map(|w| vec![Value::Text(w)]).collect(),
                    });
                }
            }
            DdlStmt::CreateTrigger(spec) => {
                return self.apply_create_trigger(spec);
            }
            DdlStmt::DropTrigger { name, if_exists } => {
                return self.apply_drop_trigger(&name, if_exists);
            }
            // `ANALYZE` is an accepted no-op: the planner is rule-based, so
            // there are no statistics to gather.
            DdlStmt::Analyze { name: _ } => {}
            // `REINDEX` is a no-op too — indexes are maintained eagerly on
            // every write, so there is nothing to rebuild — but a NAMED target
            // must still EXIST. sqlite errors on a name that is neither a
            // table, an index, nor a collation, and accepting it was a real
            // divergence: it was the corpus's only error mismatch
            // (`evidence/slt_lang_reindex.test:40  REINDEX tXiX`), and it hid
            // behind a "zero error mismatches" headline because a lenient
            // no-op looks exactly like success.
            DdlStmt::Reindex { target } => {
                if let Some(name) = &target {
                    let bundle = self.engine.schema();
                    let known_collation = matches!(
                        name.to_ascii_uppercase().as_str(),
                        "BINARY" | "NOCASE" | "RTRIM"
                    );
                    let exists = known_collation
                        || bundle.schema.table_id(name).is_some()
                        || bundle.schema.find_index_by_name(name).is_some();
                    if !exists {
                        return Err(Error::Bind(format!(
                            "unable to identify the object to be reindexed: `{name}` is \
                             neither a table, an index, nor a collation"
                        )));
                    }
                }
            }
            DdlStmt::DropPolicy { table, name } => {
                self.drop_policy(&table, &name)?;
            }
            DdlStmt::AlterRls { table, action } => match action {
                RlsAction::Enable { force } => self.enable_rls(&table, force)?,
                RlsAction::Disable => self.disable_rls(&table)?,
            },
        }
        Ok(ExecResult::Affected(0))
    }
}
