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

/// Sys-keyspace prefix for a stored view: `view/<name>` → its SELECT source.
pub(crate) const VIEW_PREFIX: &[u8] = b"view/";
/// Exclusive upper bound for a `sys_scan_range` over the whole view family:
/// `/` is 0x2f, so 0x30 is the first subkey past every `view/…` entry (#124).
pub(crate) const VIEW_PREFIX_END: &[u8] = b"view0";

impl Database {
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
        Ok(cat)
    }

    /// Every stored view as `(name, select_source)` — used by the C-API
    /// `sqlite_master` dump and by [`load_view_catalog`].
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

    /// `DROP VIEW [IF EXISTS] <name>`.
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

/// Type-check + coerce an `ADD COLUMN DEFAULT <const>` value against the
/// column's declared type (rigid schema). The one implicit widening is an
/// integer literal into a `real`/`timestamp` column, matching the config
/// schema's `parse_default`; everything else must match exactly or it is a
/// clean error (never a silent conversion, the whole point of the rigid
/// schema). `NULL` and an `any` column accept anything.
fn coerce_default(
    v: Value,
    ty: mpedb_types::ColumnType,
    table: &str,
    col: &str,
) -> Result<Value> {
    use mpedb_types::ColumnType;
    let v = match (&v, ty) {
        (Value::Int(i), ColumnType::Float64) => Value::Float(*i as f64),
        (Value::Int(i), ColumnType::Timestamp) => Value::Timestamp(*i),
        _ => v,
    };
    if !v.fits(ty) {
        return Err(Error::Bind(format!(
            "{table}.{col}: DEFAULT value of type {} does not match column type {ty}",
            v.type_name()
        )));
    }
    Ok(v)
}

/// Translate a parsed `CREATE TABLE` spec into a [`TableDef`] (resolve the PK
/// form, derive column nullability, build the UNIQUE indexes). Pure — no
/// catalog access — so the autocommit facade and an in-transaction
/// [`WriteSession`](crate::WriteSession) build the identical `TableDef` from one
/// code path (#95). The engine's `create_table` assigns the id and validates
/// the merged schema.
pub(crate) fn table_def_from_spec(
    spec: mpedb_sql::CreateTableSpec,
) -> Result<mpedb_types::TableDef> {
    // Resolve the PK: exactly one declaration form.
    let inline_pk: Vec<&str> = spec
        .columns
        .iter()
        .filter(|c| c.pk)
        .map(|c| c.name.as_str())
        .collect();
    // `implicit_rowid` (#94): a `CREATE TABLE` with NO declared PRIMARY KEY gets
    // sqlite's hidden auto-increment integer `rowid` synthesized as its sole key
    // (built below), rather than the historical "mpedb requires one" refusal.
    let mut implicit_rowid = false;
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
            // No PRIMARY KEY: synthesize the hidden rowid. A visible column that
            // is already spelled like one of sqlite's rowid names would collide
            // with (or silently shadow) the synthesized `rowid` — refuse cleanly
            // rather than risk answering differently than sqlite (#94: refuse the
            // brittle case, never guess).
            for c in &spec.columns {
                let lc = c.name.to_ascii_lowercase();
                if lc == "rowid" || lc == "_rowid_" || lc == "oid" {
                    return Err(Error::Bind(format!(
                        "CREATE TABLE {}: a table without a declared PRIMARY KEY may not \
                         also declare a column named `{}` — it collides with the implicit \
                         rowid; declare an explicit PRIMARY KEY instead",
                        spec.name, c.name
                    )));
                }
            }
            implicit_rowid = true;
            Vec::new()
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
            .position(|c| mpedb_types::ident_eq(&c.name, name))
            .map(|i| i as u16)
            .ok_or_else(|| {
                Error::Bind(format!(
                    "CREATE TABLE {}: unknown column `{name}` in key list",
                    spec.name
                ))
            })
    };
    // The generated-column sources, by ordinal. Held aside until the TableDef is
    // finished — a generated expression is bound against the whole column list,
    // which does not exist yet here. Ordinals stay valid because the hidden
    // rowid, when there is one, is appended LAST.
    let generated: Vec<(usize, (String, mpedb_types::GeneratedKind))> = spec
        .columns
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.generated.clone().map(|g| (i, g)))
        .collect();
    // Visible columns first (declaration order, ordinals `0..n-1`); the uniques
    // and any explicit PK resolve against these, so appending the hidden rowid
    // last never shifts a referenced ordinal.
    let mut columns: Vec<mpedb_types::ColumnDef> = spec
        .columns
        .iter()
        .map(|c| {
            // `DEFAULT <const>` is type-checked against the declared column type
            // NOW, so a mistyped default is a CREATE TABLE error instead of a
            // surprise at the first INSERT. An explicit `DEFAULT NULL` is
            // exactly "no default" and is not persisted — it is what an omitted
            // column already stores.
            let default = match &c.default {
                Some(mpedb_types::DefaultExpr::Const(v)) => {
                    // A DEFAULT lands in the column like any other value, so it
                    // takes the column's store-time affinity FIRST — sqlite
                    // stores `DEFAULT '1.50'` on a NUMERIC column as the real
                    // 1.5, and reports `typeof()` accordingly.
                    let v = mpedb_types::store_into(c.ty, c.affinity, c.decl.is_some(), v.clone());
                    let v = coerce_default(v, c.ty, &spec.name, &c.name)?;
                    if v.is_null() {
                        None
                    } else {
                        Some(mpedb_types::DefaultExpr::Const(v))
                    }
                }
                // The column-default parser only ever emits a Const literal.
                other => other.clone(),
            };
            Ok(mpedb_types::ColumnDef { generated: None,
                // The declared text VERBATIM, so `sqlite3_column_decltype`
                // answers what CREATE TABLE said, not the canonical name.
                decl: c.decl.clone(),
                name: c.name.clone(),
                ty: c.ty,
                // PK columns are implicitly NOT NULL, as in the config path.
                nullable: !c.not_null && !pk_names.iter().any(|p| p == &c.name),
                unique: c.unique,
                indexed: false,
                default,
                check: c.check.clone(),
                // Declared `COLLATE` rides onto the column. A collated UNIQUE/PK is
                // caught later by `Schema::validate` (collated indexes deferred).
                collation: c.collation,
                // What the DECLARED TYPE NAME said about conversion on the way
                // in — the half `ty` cannot carry (`decimal(10,2)` vs no type).
                affinity: c.affinity,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Table-level `CHECK (…)` bodies fold onto the FIRST column, ANDed with
    // whatever CHECK it already carries. The engine evaluates a CHECK program
    // over the WHOLE row — the per-column slot only decides which column a
    // violation names — so a multi-column table CHECK is enforced identically
    // wherever it hangs.
    if !spec.checks.is_empty() {
        let first = columns.first_mut().ok_or_else(|| {
            Error::Bind(format!(
                "CREATE TABLE {}: a table-level CHECK needs at least one column",
                spec.name
            ))
        })?;
        for src in &spec.checks {
            first.check = Some(match first.check.take() {
                Some(prev) => format!("({prev}) AND ({src})"),
                None => src.clone(),
            });
        }
    }
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
                predicate: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let primary_key = if implicit_rowid {
        // Append the hidden rowid as the trailing column and make it the sole PK.
        // It IS a single-Int64-PK rowid alias, so the existing NULL→max(rowid)+1
        // auto-assign machinery (#85) drives it with no engine change.
        columns.push(mpedb_types::ColumnDef { generated: None, decl: None,
            name: "rowid".into(),
            ty: mpedb_types::ColumnType::Int64,
            nullable: false,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: mpedb_types::Collation::Binary,
            affinity: mpedb_types::Affinity::Integer,
        });
        vec![(columns.len() - 1) as u16]
    } else {
        pk_names
            .iter()
            .map(|n| col_index(n))
            .collect::<Result<Vec<u16>>>()?
    };
    let mut def = mpedb_types::TableDef {
        id: 0, // assigned by Schema::with_added_table (lowest free)
        name: spec.name,
        columns,
        primary_key,
        indexes,
        dead: false,
        implicit_rowid,
        kind: mpedb_types::TableKind::Standard,
    };
    // GENERATED ALWAYS AS (…): compile each expression against the FINISHED
    // table and store the PROGRAM on the column (unlike CHECK, whose source is
    // recompiled into a side table on every bundle rebuild — see `GeneratedCol`
    // for why a generated column carries its compiled form instead). Every
    // program is compiled before any is installed, so `Schema::validate`'s
    // forward-reference rule sees the complete picture and a generated column
    // that reads a later generated column fails the CREATE TABLE.
    let gen_srcs: Vec<(usize, String, mpedb_types::GeneratedKind)> = generated
        .into_iter()
        .map(|(i, (src, kind))| (i, src, kind))
        .collect();
    let mut compiled = Vec::with_capacity(gen_srcs.len());
    for (i, src, kind) in &gen_srcs {
        let program = mpedb_sql::compile_generated(src, &def, *i).map_err(|e| {
            Error::Bind(format!(
                "CREATE TABLE {}: generated column `{}` failed to compile: {e}",
                def.name, def.columns[*i].name
            ))
        })?;
        compiled.push((*i, mpedb_types::GeneratedCol { expr: src.clone(), kind: *kind, program }));
    }
    for (i, g) in compiled {
        def.columns[i].generated = Some(g);
    }
    // Compile every CHECK against the FINISHED table before the DDL commits: an
    // expression naming a missing column, using a parameter, or not typing to
    // bool must fail the CREATE TABLE, not sit in the catalog as a constraint
    // that can never be loaded. The programs are recompiled from these same
    // sources whenever a bundle is (re)built, so they are thrown away here —
    // this call IS the validation.
    for col in &def.columns {
        if let Some(src) = &col.check {
            mpedb_sql::compile_check(src, &def).map_err(|e| {
                Error::Bind(format!(
                    "CREATE TABLE {}: CHECK on `{}` failed to compile: {e}",
                    def.name, col.name
                ))
            })?;
        }
    }
    Ok(def)
}

/// Reserved-name checks + [`TableDef`] construction for `CREATE VIRTUAL TABLE …
/// USING fts5(…)`, shared by the autocommit facade and an in-transaction
/// session. The caller does the existence / `IF NOT EXISTS` check against its
/// own schema view first.
pub(crate) fn virtual_table_def_from_spec(
    spec: mpedb_sql::CreateVirtualTableSpec,
) -> Result<mpedb_types::TableDef> {
    let mkcol = |name: &str, ty, nullable| mpedb_types::ColumnDef { generated: None, decl: None,
        name: name.to_string(),
        ty,
        nullable,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: mpedb_types::Collation::Binary,
        affinity: mpedb_types::Affinity::implied_by(ty),
    };
    // `rowid` and `rank` are reserved fts5 column names; a declared column
    // named for the table would shadow the whole-row `MATCH` operand.
    for c in &spec.columns {
        let lc = c.to_ascii_lowercase();
        if lc == "rowid" || lc == "rank" {
            return Err(Error::Bind(format!("`{c}` is a reserved fts5 column name")));
        }
        if c.eq_ignore_ascii_case(&spec.name) {
            return Err(Error::Bind(format!(
                "an fts5 column may not share the table name `{}`",
                spec.name
            )));
        }
    }
    let mut columns = vec![mkcol("rowid", mpedb_types::ColumnType::Int64, false)];
    for c in &spec.columns {
        columns.push(mkcol(c, mpedb_types::ColumnType::Text, true));
    }
    Ok(mpedb_types::TableDef {
        id: 0,
        name: spec.name.clone(),
        columns,
        primary_key: vec![0],
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Fts { tokenizer: spec.tokenizer },
    })
}

/// Type-check an `ALTER TABLE … ADD COLUMN` spec and produce the
/// [`ColumnDef`](mpedb_types::ColumnDef) plus the fill value seeded into every
/// existing row (`Value::Null` when there is no default). Shared by the
/// autocommit facade and an in-transaction session (#95).
pub(crate) fn add_column_from_spec(
    def: &mpedb_types::TableDef,
    spec: mpedb_sql::CreateColumnSpec,
) -> Result<(mpedb_types::ColumnDef, Value)> {
    use mpedb_types::DefaultExpr;
    let table = def.name.as_str();
    if spec.unique || spec.pk {
        return Err(Error::Bind(format!(
            "ALTER TABLE {table} ADD COLUMN {}: UNIQUE / PRIMARY KEY on ADD is not \
             supported (would need an online index build) — sqlite refuses these too",
            spec.name
        )));
    }
    // Resolve + type-check the DEFAULT const against the column type. The
    // fill value seeds every existing row (NULL when there is no default).
    let fill = match spec.default {
        // The store-time affinity applies to the fill value too: it is what
        // lands in every existing row, so it must be the value the column would
        // have held had the rows been inserted with it.
        Some(DefaultExpr::Const(v)) => coerce_default(
            mpedb_types::store_into(spec.ty, spec.affinity, spec.decl.is_some(), v),
            spec.ty,
            table,
            &spec.name,
        )?,
        // The ADD-COLUMN parser only ever emits a Const literal (no now()).
        Some(DefaultExpr::Now) => {
            return Err(Error::Bind(format!(
                "ALTER TABLE {table} ADD COLUMN {}: now() is not a constant default \
                 (sqlite refuses a non-constant ADD-COLUMN default)",
                spec.name
            )))
        }
        None => Value::Null,
    };
    if spec.not_null && fill.is_null() {
        return Err(Error::Bind(format!(
            "ALTER TABLE {table} ADD COLUMN {}: a NOT NULL column needs a non-NULL \
             DEFAULT to fill existing rows (matches sqlite: \"Cannot add a NOT NULL \
             column with default value NULL\")",
            spec.name
        )));
    }
    // A NULL fill is indistinguishable from "no default" for a nullable
    // column — do not persist a redundant NULL default.
    let default = if fill.is_null() {
        None
    } else {
        Some(DefaultExpr::Const(fill.clone()))
    };
    let mut col = mpedb_types::ColumnDef { generated: None,
        decl: spec.decl.clone(),
        name: spec.name,
        ty: spec.ty,
        nullable: !spec.not_null,
        unique: false,
        indexed: false,
        default,
        check: None,
        // ADD COLUMN carries its declared `COLLATE`. UNIQUE on ADD is already
        // refused above, so a collated index cannot arise here.
        collation: spec.collation,
        affinity: spec.affinity,
    };
    // `ADD COLUMN … AS (<expr>)`: compile against the WIDENED table, since the
    // expression's own column is the last one. The engine backfills every
    // existing row by evaluating it (and refuses STORED once the table has rows,
    // as sqlite does).
    if let Some((src, kind)) = spec.generated {
        let mut widened = def.clone();
        widened.columns.push(col.clone());
        let at = widened.columns.len() - 1;
        let program = mpedb_sql::compile_generated(&src, &widened, at).map_err(|e| {
            Error::Bind(format!(
                "ALTER TABLE {table} ADD COLUMN {}: generated expression failed to \
                 compile: {e}",
                col.name
            ))
        })?;
        col.generated = Some(mpedb_types::GeneratedCol { expr: src, kind, program });
    }
    Ok((col, fill))
}

/// Resolve `CREATE INDEX` column names to ordinals against `t`. Shared by the
/// autocommit facade and an in-transaction session (#95).
pub(crate) fn resolve_index_columns(
    t: &mpedb_types::TableDef,
    table: &str,
    columns: &[String],
) -> Result<Vec<u16>> {
    columns
        .iter()
        .map(|name| {
            t.columns
                .iter()
                .position(|c| mpedb_types::ident_eq(&c.name, name))
                .map(|i| i as u16)
                .ok_or_else(|| {
                    Error::Bind(format!("CREATE INDEX on `{table}`: no column `{name}`"))
                })
        })
        .collect()
}

impl Database {
    /// `CREATE TABLE` (#47 stage 2/3): build the [`TableDef`] from the parsed
    /// spec, append it to the schema in one catalog commit (the engine
    /// validates the merged set and seeds the empty tree roots), then swap this
    /// process's schema bundle and drop the local plan cache. Other processes
    /// reload at their next transaction via the schema-gen bump.
    pub(crate) fn apply_create_table(&self, spec: mpedb_sql::CreateTableSpec) -> Result<ExecResult> {
        let def = table_def_from_spec(spec)?;
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
        let def = virtual_table_def_from_spec(spec)?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.create_table(def) {
            Ok(_tid) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
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
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        // Cascade: a dropped table's triggers are dead — remove their records in
        // the same commit (DESIGN-TRIGGERS §3.1).
        let res = crate::trigger::cascade_drop_triggers(&mut w, id).and_then(|()| w.drop_table(id));
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
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.alter_add_column(id, col, fill) {
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
    pub(crate) fn apply_create_index(
        &self,
        table: &str,
        columns: &[String],
        unique: bool,
        predicate: Option<String>,
    ) -> Result<ExecResult> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let id = bundle
            .schema
            .table_id(table)
            .ok_or_else(|| Error::Bind(format!("CREATE INDEX: no such table `{table}`")))?;
        let t = bundle.schema.table(id).expect("table_id resolved");
        let cols = resolve_index_columns(t, table, columns)?;
        // Idempotent by shape: an identical index already present is a no-op.
        if t.indexes.iter().any(|ix| {
            ix.columns == cols && ix.unique == unique && ix.predicate == predicate
        }) {
            return Ok(ExecResult::Affected(0));
        }
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.create_index(id, cols, unique, predicate) {
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
            DdlStmt::CreateVirtualTable(spec) => {
                return self.apply_create_virtual_table(spec);
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
            DdlStmt::CreateIndex {
                table,
                columns,
                unique,
                where_clause,
                ..
            } => {
                return self.apply_create_index(&table, &columns, unique, where_clause);
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
            // `ANALYZE`/`REINDEX` are accepted no-ops. The planner is rule-based
            // (no statistics for ANALYZE to gather) and indexes are maintained
            // eagerly on every write (nothing for REINDEX to rebuild), so both
            // succeed touching nothing — matching sqlite's "it works" so tools
            // and migrations that emit them do not break.
            DdlStmt::Analyze { name: _ } | DdlStmt::Reindex { target: _ } => {}
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
