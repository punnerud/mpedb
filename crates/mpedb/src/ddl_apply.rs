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

impl Database {
    /// `CREATE TABLE` (#47 stage 2/3): build the [`TableDef`] from the parsed
    /// spec, append it to the schema in one catalog commit (the engine
    /// validates the merged set and seeds the empty tree roots), then swap this
    /// process's schema bundle and drop the local plan cache. Other processes
    /// reload at their next transaction via the schema-gen bump.
    pub(crate) fn apply_create_table(&self, spec: mpedb_sql::CreateTableSpec) -> Result<ExecResult> {
        // Resolve the PK: exactly one declaration form.
        let inline_pk: Vec<&str> = spec
            .columns
            .iter()
            .filter(|c| c.pk)
            .map(|c| c.name.as_str())
            .collect();
        let pk_names: Vec<String> = match (inline_pk.is_empty(), spec.table_pk.is_empty()) {
            (false, true) => {
                // Multiple inline `PRIMARY KEY` columns is almost always a
                // typo, not an intended composite key — sqlite and
                // PostgreSQL both hard-refuse it. A composite PK must be
                // declared once at table level: `PRIMARY KEY (a, b)`.
                if inline_pk.len() > 1 {
                    return Err(Error::Bind(format!(
                        "CREATE TABLE {}: more than one column marked PRIMARY KEY \
                         ({}) — for a composite key write `PRIMARY KEY ({})` at \
                         table level",
                        spec.name,
                        inline_pk.join(", "),
                        inline_pk.join(", ")
                    )));
                }
                inline_pk.iter().map(|s| s.to_string()).collect()
            }
            (true, false) => spec.table_pk.clone(),
            (true, true) => {
                return Err(Error::Bind(format!(
                    "CREATE TABLE {}: no PRIMARY KEY declared (mpedb requires one)",
                    spec.name
                )))
            }
            (false, false) => {
                return Err(Error::Bind(format!(
                    "CREATE TABLE {}: PRIMARY KEY declared both inline and at table \
                     level — pick one",
                    spec.name
                )))
            }
        };
        let col_index = |name: &str| -> Result<u16> {
            spec.columns
                .iter()
                .position(|c| c.name == name)
                .map(|i| i as u16)
                .ok_or_else(|| {
                    Error::Bind(format!(
                        "CREATE TABLE {}: unknown column `{name}` in key list",
                        spec.name
                    ))
                })
        };
        let primary_key = pk_names
            .iter()
            .map(|n| col_index(n))
            .collect::<Result<Vec<u16>>>()?;
        let columns = spec
            .columns
            .iter()
            .map(|c| mpedb_types::ColumnDef {
                name: c.name.clone(),
                ty: c.ty,
                // PK columns are implicitly NOT NULL, as in the config path.
                nullable: !c.not_null && !pk_names.iter().any(|p| p == &c.name),
                unique: c.unique,
                indexed: false,
                default: None,
                check: None,
            })
            .collect();
        let indexes = spec
            .uniques
            .iter()
            .map(|group| {
                Ok(mpedb_types::IndexDef {
                    columns: group
                        .iter()
                        .map(|n| col_index(n))
                        .collect::<Result<Vec<u16>>>()?,
                    unique: true,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let def = mpedb_types::TableDef {
            id: 0, // assigned by Schema::with_added_table (lowest free)
            name: spec.name,
            columns,
            primary_key,
            indexes,
            dead: false,
        };
        let mut w = self.engine.begin_write()?;
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

    pub(crate) fn apply_drop_table(&self, name: &str, if_exists: bool) -> Result<ExecResult> {
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
        let mut w = self.engine.begin_write()?;
        match w.drop_table(id) {
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
        let mut w = self.engine.begin_write()?;
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

    /// `ALTER TABLE ... ADD COLUMN` (#47 stage 5, v1). Only a NULLABLE column is
    /// accepted: NOT NULL has no DEFAULT to fill existing rows with, and
    /// UNIQUE / PRIMARY KEY would need an online index build — both refused with
    /// a clear message (sqlite/PG also refuse NOT NULL without a default when the
    /// table has rows). The engine rewrites existing rows with the new column
    /// NULL in one commit.
    pub(crate) fn apply_alter_add_column(
        &self,
        table: &str,
        spec: mpedb_sql::CreateColumnSpec,
    ) -> Result<ExecResult> {
        if spec.not_null {
            return Err(Error::Bind(format!(
                "ALTER TABLE {table} ADD COLUMN {}: NOT NULL is not supported on ADD \
                 (no DEFAULT to fill existing rows) — add it nullable",
                spec.name
            )));
        }
        if spec.unique || spec.pk {
            return Err(Error::Bind(format!(
                "ALTER TABLE {table} ADD COLUMN {}: UNIQUE / PRIMARY KEY on ADD is not \
                 supported yet (would need an online index build)",
                spec.name
            )));
        }
        let col = mpedb_types::ColumnDef {
            name: spec.name,
            ty: spec.ty,
            nullable: true,
            unique: false,
            indexed: false,
            default: None,
            check: None,
        };
        self.engine.refresh_schema_if_stale()?;
        let id = self
            .engine
            .schema()
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{table}`")))?;
        let mut w = self.engine.begin_write()?;
        match w.alter_add_column(id, col) {
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
        let mut w = self.engine.begin_write()?;
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
    pub(crate) fn apply_create_index(
        &self,
        table: &str,
        columns: &[String],
        unique: bool,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let id = bundle
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("CREATE INDEX: no such table `{table}`")))?;
        let t = bundle.schema.table(id).expect("table_id resolved");
        let cols: Vec<u16> = columns
            .iter()
            .map(|name| {
                t.columns
                    .iter()
                    .position(|c| &c.name == name)
                    .map(|i| i as u16)
                    .ok_or_else(|| {
                        Error::Bind(format!("CREATE INDEX on `{table}`: no column `{name}`"))
                    })
            })
            .collect::<Result<_>>()?;
        // Idempotent by shape: an identical index already present is a no-op.
        if t.indexes.iter().any(|ix| ix.columns == cols && ix.unique == unique) {
            return Ok(ExecResult::Affected(0));
        }
        let mut w = self.engine.begin_write()?;
        match w.create_index(id, cols, unique) {
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
            DdlStmt::CreateTable(spec) => {
                return self.apply_create_table(spec);
            }
            DdlStmt::DropTable { name, if_exists } => {
                return self.apply_drop_table(&name, if_exists);
            }
            DdlStmt::AlterRenameTable { table, new_name } => {
                return self.apply_alter_rename(&table, |w, id| w.alter_rename_table(id, &new_name));
            }
            DdlStmt::AlterRenameColumn { table, column, new_name } => {
                return self.apply_alter_rename(&table, |w, id| {
                    w.alter_rename_column(id, &column, &new_name)
                });
            }
            DdlStmt::AlterAddColumn { table, column } => {
                return self.apply_alter_add_column(&table, column);
            }
            DdlStmt::AlterDropColumn { table, column } => {
                return self.apply_alter_drop_column(&table, &column);
            }
            DdlStmt::CreateIndex { table, columns, unique, .. } => {
                return self.apply_create_index(&table, &columns, unique);
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
