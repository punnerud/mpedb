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

/// Type-check + coerce an `ADD COLUMN DEFAULT <const>` value against the
/// column's declared type (rigid schema). The one implicit widening is an
/// integer literal into a `real`/`timestamp` column, matching the config
/// schema's `parse_default`; everything else must match exactly or it is a
/// clean error (never a silent conversion, the whole point of the rigid
/// schema). `NULL` and an `any` column accept anything.
/// Resolve `DEFAULT ( <expr> )`: fold it to the literal it always evaluates to,
/// or — when it reads the STATEMENT INSTANT — keep it as a program to evaluate
/// per INSERT.
///
/// The expression is CLOSED by sqlite's own rule — it may not read a column
/// ("default value of column [b] is not constant", MEASURED at 3.45.1) — so it
/// is a constant written as arithmetic, and evaluating it once at DDL time is
/// the same answer as evaluating it per row, forever. That is what lets the
/// stored schema keeps a plain `DefaultExpr::Const` for that case.
///
/// `'NOW'` is the exception, and the ONE the refusal here used to catch:
/// `DEFAULT (STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))` — Django's `auto_now_add`
/// — has no single value to fold to, and it is not supposed to. A DEFAULT is
/// EVALUATED per INSERT, so "when this row was inserted" is exactly its
/// meaning; the reasoning that bars `'now'` from a CHECK or an index
/// expression (stored, re-evaluated later, silently changing under something
/// already written) does not apply to it.
///
/// Compiled against a table with NO columns, which is what enforces the closure:
/// a column reference has nothing to bind to and is refused, in the same breath
/// as anything else that will not fold (a parameter, a subquery, a
/// non-deterministic call). The refusal names the column, because at DDL time
/// that is the only thing the user can act on.
fn fold_default_expr(
    src: &str,
    table: &str,
    column: &str,
    host_udfs: &mpedb_sql::HostUdfSet,
) -> Result<mpedb_types::DefaultExpr> {
    let closed = mpedb_types::TableDef {
        id: 0,
        name: table.to_string(),
        columns: Vec::new(),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        dead: false,
        kind: mpedb_types::TableKind::Standard,
        implicit_rowid: false, autoincrement: false,
        foreign_keys: Vec::new(),
    };
    let bad = |why: String| {
        Error::Schema(format!(
            "DEFAULT ({src}) on `{table}`.`{column}` is not a constant: {why}"
        ))
    };
    let (prog, uses_instant) = mpedb_sql::compile_default_expr_with_udfs(src, &closed, host_udfs)
        .map_err(|e| bad(e.to_string()))?;
    // A DEFAULT that calls a host-registered UDF is DEFERRED for the same
    // reason `'now'` is: there is no single value to fold to. It is evaluated
    // per INSERT, against the inserting connection's function set — which is
    // exactly what sqlite does (measured: it accepts the DDL for a function it
    // has never heard of, errors on INSERT while the name is missing, and works
    // the moment `create_function` supplies it).
    //
    // Django's `DEFAULT (django_datetime_extract(…))` is this shape, and the
    // refusal took not just its own 17 tests but the eleven labels that share
    // its test group.
    //
    // This puts a program that depends on a PER-CONNECTION function into the
    // shared schema, which is the opposite of the rule `has_host_call` was
    // written for: a PLAN carrying one is kept out of the shared registry,
    // because another process would decode it and be unable to run it. The
    // difference is what "unable to run it" costs. A registry plan is looked up
    // by hash and must execute; a DEFAULT is only reached by a connection
    // inserting into this table, and one without the function gets a named
    // error on its own INSERT while every row already written keeps its value.
    // sqlite makes precisely this trade, and Django is built on it.
    if uses_instant || prog.has_host_call() {
        return Ok(mpedb_types::DefaultExpr::Expr(Box::new(
            mpedb_types::DefaultProgram { src: src.to_string(), program: prog },
        )));
    }
    let mut stack = Vec::new();
    let v = prog
        .eval_with_stack(&mut stack, &[], &[])
        .map_err(|e| bad(e.to_string()))?;
    Ok(mpedb_types::DefaultExpr::Const(v))
}

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
        // `BOOLEAN DEFAULT 1`. sqlite has no boolean type, so an ORM declaring
        // one writes an integer default for it — and the INSERT path already
        // takes the same value on the same column (`INSERT INTO t (b) VALUES
        // (1)` stores `1`/`integer`, byte-identical to sqlite). Refusing it
        // HERE and accepting it there was the two paths disagreeing about one
        // column, not a rule.
        (Value::Int(i), ColumnType::Bool) if *i == 0 || *i == 1 => Value::Bool(*i == 1),
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
    host_udfs: &mpedb_sql::HostUdfSet,
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
            // `DEFAULT ( <expr> )` folds to a literal HERE, where the
            // expression compiler is reachable. It is sound to fold because the
            // expression is CLOSED — sqlite refuses one that reads a column
            // ("default value of column [b] is not constant", measured) — so it
            // has the same value for every row that will ever be inserted.
            let folded = match &c.default_src {
                None => None,
                Some(src) => Some(fold_default_expr(src, &spec.name, &c.name, host_udfs)?),
            };
            let default = match folded.as_ref().or(c.default.as_ref()) {
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
                // The column-default parser emits a Const literal or `Now`.
                other => other.cloned(),
            };
            Ok(mpedb_types::ColumnDef { generated: None,
                // The DEFAULT's DDL text verbatim, for `dflt_value` — sqlite
                // reports what was WRITTEN, which the folded value cannot say.
                default_text: c.default_text.clone(),
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
                collations: Vec::new(),
                columns: group
                    .iter()
                    .map(|n| col_index(n))
                    .collect::<Result<Vec<u16>>>()?,
                unique: true,
                predicate: None,
                // An inline/table UNIQUE constraint. sqlite gives these an
                // auto name (`sqlite_autoindex_…`) that DROP INDEX refuses to
                // touch, so carrying none is the same reachable surface.
                exprs: Vec::new(),
                name: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let primary_key = if implicit_rowid {
        // Append the hidden rowid as the trailing column and make it the sole PK.
        // It IS a single-Int64-PK rowid alias, so the existing NULL→max(rowid)+1
        // auto-assign machinery (#85) drives it with no engine change.
        columns.push(mpedb_types::ColumnDef { generated: None, default_text: None, decl: None,
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
    // FOREIGN KEYs. Only the CHILD side resolves here — the parent stays in
    // names because a forward reference is legal (see `ForeignKeyDef`). The two
    // things decidable without a catalog are decided now, both of them errors
    // sqlite also raises at CREATE time:
    //   * a child column that does not exist,
    //   * a declared parent list of a different width than the child list.
    let foreign_keys = spec
        .foreign_keys
        .iter()
        .map(|fk| {
            let columns = fk
                .columns
                .iter()
                .map(|n| {
                    col_index(n).map_err(|_| {
                        Error::Bind(format!(
                            "unknown column \"{n}\" in foreign key definition"
                        ))
                    })
                })
                .collect::<Result<Vec<u16>>>()?;
            if !fk.parent_columns.is_empty() && fk.parent_columns.len() != columns.len() {
                return Err(Error::Bind(
                    "number of columns in foreign key does not match the number of \
                     columns in the referenced table"
                        .into(),
                ));
            }
            Ok(mpedb_types::ForeignKeyDef {
                columns,
                parent: fk.parent.clone(),
                parent_columns: fk.parent_columns.clone(),
                on_delete: fk.on_delete,
                on_update: fk.on_update,
                deferred: fk.deferred,
                name: fk.name.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut def = mpedb_types::TableDef {
        id: 0, // assigned by Schema::with_added_table (lowest free)
        name: spec.name,
        columns,
        primary_key,
        indexes,
        dead: false,
        implicit_rowid,
        autoincrement: spec.autoincrement,
        kind: mpedb_types::TableKind::Standard,
        foreign_keys,
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
    let mkcol = |name: &str, ty, nullable| mpedb_types::ColumnDef { generated: None, default_text: None, decl: None,
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
    // The rowid is the TRAILING column and HIDDEN (#94's implicit-rowid shape),
    // not a leading visible one. That is not cosmetic: sqlite hides an fts5
    // vtab's rowid from `SELECT *`, from `PRAGMA table_info` and from the
    // default INSERT column list, and exposing it did both damages at once —
    // `SELECT *` answered (rowid, content) where stock answers (content), a
    // wrong answer; and `INSERT INTO t VALUES('a')` was refused with
    // "expected 2" where stock auto-assigns. The trailing position is what
    // lets every piece of the #94 machinery (visible_columns, the default
    // insert list, table_info, rowid-name resolution, auto-assign) apply
    // unchanged. `rowid` stays addressable BY NAME, exactly as in sqlite.
    //
    // Existing fts tables keep their stored (leading, visible) shape — the
    // schema is file-authoritative and both shapes validate.
    let mut columns: Vec<_> = spec
        .columns
        .iter()
        .map(|c| mkcol(c, mpedb_types::ColumnType::Text, true))
        .collect();
    columns.push(mkcol("rowid", mpedb_types::ColumnType::Int64, false));
    let pk = (columns.len() - 1) as u16;
    Ok(mpedb_types::TableDef {
        id: 0,
        name: spec.name.clone(),
        columns,
        primary_key: vec![pk],
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: true, autoincrement: false,
        kind: mpedb_types::TableKind::Fts { tokenizer: spec.tokenizer, module: spec.module },
        // An FTS shadow table is engine-owned; user DDL never attaches a key
        // to it.
        foreign_keys: Vec::new(),
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
        // sqlite refuses a NON-CONSTANT default on ADD COLUMN — there is no
        // one value to seed the existing rows with. `now()` and the three time
        // keywords are all that shape.
        Some(ref d @ (DefaultExpr::Now
        | DefaultExpr::CurrentTimestamp
        | DefaultExpr::CurrentDate
        | DefaultExpr::CurrentTime
        | DefaultExpr::Expr(_))) => {
            let what = match d {
                DefaultExpr::Expr(d) => d.src.as_str(),
                other => other.time_keyword().unwrap_or("now()"),
            };
            return Err(Error::Bind(format!(
                "ALTER TABLE {table} ADD COLUMN {}: {what} is not a constant default \
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
        // ADD COLUMN's default is a CONSTANT (a non-constant one is refused
        // above), so its DDL text is what the column keeps.
        default_text: spec.default_text.clone(),
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
    // `ADD COLUMN … CHECK (<expr>)`: store the SOURCE — bundle (re)builds
    // compile the program from it like any CREATE TABLE check, so future
    // writes enforce it for free. Compilation AND the existing-row test live
    // in [`compile_added_column_check`] / [`refuse_check_violating_rows`],
    // which both appliers call with the schema in hand: the widening must be
    // `with_added_column`'s (a hand-widened def that APPENDS puts the
    // program's slots one off on a #94 implicit-rowid table, where the new
    // column lands BEFORE the trailing rowid), and sqlite DOES test existing
    // rows at ALTER time — measured on 3.45.1: `DEFAULT -5 CHECK (x >= 0)`
    // on a populated table refuses the whole ALTER ("CHECK constraint
    // failed"), and `DEFAULT 5 CHECK (x > id)` over rows {1, 9} refuses too,
    // so the verdict is PER ROW, not per fill constant. A NULL fill passes
    // (3VL), an empty table accepts anything — both fall out of the one rule
    // "refuse iff any existing row evaluates to FALSE".
    if let Some(src) = &spec.check {
        col.check = Some(src.clone());
    }
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

/// The compiled half of `ADD COLUMN … CHECK`, built where the caller has the
/// SCHEMA in hand: the table is widened by [`Schema::with_added_column`] —
/// the engine's own rule, which places the new column BEFORE a #94
/// implicit-rowid's trailing `rowid` — so the program's slots match the rows
/// the scan will feed it. `insert_at` is where the fill value goes in an
/// existing row to reproduce exactly the row the engine is about to write.
pub(crate) struct AddedColumnCheck {
    program: mpedb_types::expr::ExprProgram,
    insert_at: usize,
}

pub(crate) fn compile_added_column_check(
    schema: &mpedb_types::Schema,
    table_id: u32,
    col: &mpedb_types::ColumnDef,
) -> Result<Option<AddedColumnCheck>> {
    let Some(src) = col.check.as_deref() else {
        return Ok(None);
    };
    let widened = schema.with_added_column(table_id, col.clone())?;
    let t = widened.table(table_id).expect("with_added_column keeps the id live");
    let program = mpedb_sql::compile_check(src, t).map_err(|e| {
        Error::Bind(format!(
            "ALTER TABLE {} ADD COLUMN {}: CHECK failed to compile: {e}",
            t.name, col.name
        ))
    })?;
    let insert_at = t
        .columns
        .iter()
        .position(|c| c.name == col.name)
        .expect("the added column is in the widened table");
    Ok(Some(AddedColumnCheck { program, insert_at }))
}

/// sqlite tests EXISTING rows against an `ADD COLUMN … CHECK` at ALTER time
/// (measured on 3.45.1, per row and not per fill constant: `DEFAULT 5
/// CHECK (x > id)` over rows {1, 9} refuses because the id=9 row fails). One
/// rule reproduces every measured edge: refuse iff any existing row, with the
/// fill value in the new column's slot, evaluates to FALSE — a NULL verdict
/// passes (3VL, so the no-DEFAULT NULL fill sails through) and an empty table
/// never enters the loop (stock accepts even a violating default there). The
/// error is sqlite's ALTER-time message VERBATIM — bare, no expression
/// suffix, unlike the INSERT-time form. Materializing the scan matches the
/// engine's own `alter_add_column`, which buffers every rewritten row of the
/// same table in the same transaction anyway.
pub(crate) fn refuse_check_violating_rows(
    w: &mut mpedb_core::engine::WriteTxn,
    table_id: u32,
    chk: &AddedColumnCheck,
    fill: &Value,
) -> Result<()> {
    for mut row in w.scan_rows(table_id, None, None)? {
        row.insert(chk.insert_at, fill.clone());
        let verdict = chk.program.eval(&row, &[])?;
        if mpedb_types::expr::truthy3(&verdict) == Some(false) {
            return Err(Error::Bind("CHECK constraint failed".into()));
        }
    }
    Ok(())
}

/// The rewritten `AS (…)` sources an `ALTER TABLE … RENAME COLUMN` needs, each
/// one PROVEN to mean what it meant before.
///
/// A generated column names its inputs in source text. The compiled program
/// reads ordinals and a rename does not move them, so evaluation would survive
/// untouched — but the stored source would keep naming a column that no longer
/// exists, and a dump replayed from it would fail. sqlite rewrites the text, so
/// mpedb must too, and this used to be a flat refusal for lack of an
/// expression printer.
///
/// Two halves, and the second is what makes the first safe:
///
///  1. [`mpedb_sql::rename_identifier`] replaces the identifier TOKEN, so
///     `'pink'` in a string literal and `pinkish` as a longer name are left
///     alone. It is still only lexical: it does not know that a token in
///     function position is not a column reference.
///  2. Both sources are then COMPILED — the old one against the old table, the
///     new one against the table as the rename leaves it — and the two
///     programs must be equal. That is the whole safety argument: a rewrite is
///     a rename only if it means the same thing, and anything that changes the
///     meaning, for a reason anticipated here or not, is refused instead of
///     silently altering what the column computes.
///
/// Comparing two FRESH compilations rather than one against the stored program
/// is deliberate: it asks exactly "did the rewrite change the meaning", and
/// does not also depend on the stored program having been built by today's
/// binder.
pub(crate) fn rename_generated_srcs(
    t: &mpedb_types::TableDef,
    column: &str,
    new_name: &str,
) -> Result<Vec<(u16, String)>> {
    if !t.has_generated() {
        return Ok(Vec::new());
    }
    // The table as the rename leaves it. Only a name changes, so the ordinals
    // both sides address are identical and the comparison is meaningful.
    let mut after = t.clone();
    match after
        .columns
        .iter_mut()
        .find(|c| mpedb_types::ident_eq(&c.name, column))
    {
        Some(c) => c.name = new_name.to_string(),
        // Unknown column: let the schema evolver report it, in its words.
        None => return Ok(Vec::new()),
    }
    let mut out = Vec::new();
    for (i, c) in t.columns.iter().enumerate() {
        let Some(g) = &c.generated else { continue };
        let src = mpedb_sql::rename_identifier(&g.expr, column, new_name)?;
        let refuse = |why: String| {
            Error::Schema(format!(
                "cannot rename column `{column}` of `{}`: the generated column `{}` \
                 would become `{src}`, which {why}",
                t.name, c.name
            ))
        };
        let before = mpedb_sql::compile_value_expr(&g.expr, t)
            .map_err(|e| refuse(format!("cannot be checked — `{}` does not compile: {e}", g.expr)))?;
        let after_prog = mpedb_sql::compile_value_expr(&src, &after)
            .map_err(|e| refuse(format!("does not compile: {e}")))?;
        if before != after_prog {
            return Err(refuse("computes something different".into()));
        }
        out.push((i as u16, src));
    }
    Ok(out)
}

/// sqlite's five fts4 SHADOW tables (plan §7), as REAL Standard tables —
/// created and dropped WITH the virtual table so the catalog lists exactly
/// what sqlite would (`test_content/_docsize/_segdir/_segments/_stat`).
/// Shapes mirror sqlite's own shadow DDL (measured, 3.45.1): typeless
/// payload columns are `Any` (mpedb's typeless), `docid`/`blockid`/`id` are
/// INTEGER-PK rowid aliases, `_segdir` has the composite PK(level, idx).
/// Their CONTENT stays empty when the vtab holds data — mpedb indexes in
/// its own inverted tree; a dump with data replays correctly through the
/// vtab's own INSERT rows (a documented narrowing, never a wrong answer).
pub(crate) fn fts4_shadow_defs(
    vtab: &str,
    content_cols: &[String],
) -> Vec<mpedb_types::TableDef> {
    let col = |name: &str, ty, nullable| mpedb_types::ColumnDef {
        generated: None,
        default_text: None,
        decl: None,
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
    use mpedb_types::ColumnType::{Any, Int64};
    let table = |suffix: &str, columns: Vec<mpedb_types::ColumnDef>, pk: Vec<u16>| {
        mpedb_types::TableDef {
            id: 0,
            name: format!("{vtab}{suffix}"),
            columns,
            primary_key: pk,
            indexes: Vec::new(),
            dead: false,
            implicit_rowid: false,
            autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        }
    };
    vec![
        table("_content", {
            // `c0<name>, c1<name>, …` — sqlite's own content-column spelling.
            let mut cs = vec![col("docid", Int64, false)];
            for (i, c) in content_cols.iter().enumerate() {
                cs.push(col(&format!("c{i}{c}"), Any, true));
            }
            cs
        }, vec![0]),
        table("_docsize", vec![col("docid", Int64, false), col("size", Any, true)], vec![0]),
        table(
            "_segdir",
            vec![
                col("level", Int64, false),
                col("idx", Int64, false),
                col("start_block", Int64, true),
                col("leaves_end_block", Int64, true),
                col("end_block", Int64, true),
                col("root", Any, true),
            ],
            vec![0, 1],
        ),
        table("_segments", vec![col("blockid", Int64, false), col("block", Any, true)], vec![0]),
        table("_stat", vec![col("id", Int64, false), col("value", Any, true)], vec![0]),
    ]
}

/// Rewritten CHECK sources for a column rename — `rename_generated_srcs`'s
/// twin, same contract: the rename arrives as DATA (ordinal, new source),
/// verified by compiling BOTH spellings against before/after tables and
/// requiring the identical program. A CHECK that stops compiling refuses the
/// rename by name — shipping a schema whose text and behaviour disagree is
/// exactly the failure this exists to prevent.
pub(crate) fn rename_check_srcs(
    t: &mpedb_types::TableDef,
    column: &str,
    new_name: &str,
) -> Result<Vec<(u16, String)>> {
    if t.columns.iter().all(|c| c.check.is_none()) {
        return Ok(Vec::new());
    }
    let mut after = t.clone();
    match after.columns.iter_mut().find(|c| mpedb_types::ident_eq(&c.name, column)) {
        Some(c) => c.name = new_name.to_string(),
        None => return Ok(Vec::new()),
    }
    let mut out = Vec::new();
    for (i, c) in t.columns.iter().enumerate() {
        let Some(src0) = &c.check else { continue };
        let src = mpedb_sql::rename_identifier(src0, column, new_name)?;
        let refuse = |why: String| {
            Error::Schema(format!(
                "cannot rename column `{column}` of `{}`: the CHECK on `{}`                  would become `{src}`, which {why}",
                t.name, c.name
            ))
        };
        let before = mpedb_sql::compile_check(src0, t)
            .map_err(|e| refuse(format!("cannot be checked — `{src0}` does not compile: {e}")))?;
        let after_prog = mpedb_sql::compile_check(&src, &after)
            .map_err(|e| refuse(format!("does not compile: {e}")))?;
        if before != after_prog {
            return Err(refuse("checks something different".into()));
        }
        out.push((i as u16, src));
    }
    Ok(out)
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

    pub(crate) fn apply_drop_table(
        &self,
        name: &str,
        if_exists: bool,
        cascade: bool,
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
                for other in sc.tables.iter().filter(|o| !o.dead && o.id != id) {
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
            DdlStmt::DropTable { name, if_exists, cascade } => {
                return self.apply_drop_table(&name, if_exists, cascade);
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
