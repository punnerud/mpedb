//! Database attachment, host registration and guarded sessions.
use super::*;

impl Database {
    /// Open (or create) the database described by a TOML config file.
    pub fn open(config_path: &Path) -> Result<Database> {
        Database::open_with_config(Config::from_file(config_path)?)
    }

    /// Attach an existing database file config-free, reading its stored schema
    /// and geometry (the file is schema-authoritative). Used by tooling — the
    /// mirror daemon/CLI, `dump`, the `mpedb <file.mpedb>` CLI — that must open a
    /// file it did not create a TOML for. Durability = `async` still needs a
    /// config (no background flusher); mirror files do not use it.
    ///
    /// CHECK constraints ARE enforced here. They did not used to be — this
    /// constructor once documented "a file that carries CHECK constraints must be
    /// opened via a config", which held only while a CHECK could arrive from
    /// nowhere but a config. `CREATE TABLE … CHECK (…)` is live DDL now, so a
    /// constraint can be born INSIDE the file, and every config-free attach then
    /// stored it and silently never enforced it — the CLI's own `.mpedb` path
    /// included. The compiler works off the schema, which the file carries, so
    /// there was never a reason to withhold it.
    pub fn open_from_file(path: &Path) -> Result<Database> {
        let engine = Engine::open_from_file(path)?;
        // Installing the compiler also REBUILDS the bundle it just loaded, so
        // this covers both what the file already carries and any later schema
        // the engine loads from the catalog — including one a peer process
        // creates while we are attached.
        engine.set_check_compiler(std::sync::Arc::new(compile_schema_checks))?;
        let db = Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            text_memo: RwLock::new(MemoMap::default()),
            spell_cache: RwLock::new(HashMap::new()),
            cache_gen: std::sync::atomic::AtomicU64::new(0),
            conn_id: next_conn_id(),
            rules_cache: RwLock::new(None),
            // Placeholders — the stored `tune` record decides both, and is read
            // below. They cannot be read before this point because reading a
            // sys record needs the handle.
            fk_enforce: std::sync::atomic::AtomicBool::new(false),
            path: path.to_path_buf(),
            storage: mpedb_types::StorageKind::File,
            // No config, so no §6.3 assertions — consistent with this
            // constructor's contract (it also skips CHECK programs): a
            // config-free attach enforces what the FILE carries, and
            // `require_policy` is a config-declared deployment assertion.
            require_policy: std::collections::HashSet::new(),
            dialect: mpedb_types::Dialect::default(),
            role: crate::sync::Role::Standalone,
            host_udfs: RwLock::new(HashMap::new()),
            host_aggs: RwLock::new(HashMap::new()),
            host_window_aggs: RwLock::new(std::collections::HashSet::new()),
            host_colls: RwLock::new(HashMap::new()),
            busy_timeout_ms: std::sync::atomic::AtomicI64::new(-1),
            attached: RwLock::new(Default::default()),
            temp_views: RwLock::new(Default::default()),
            cross_cache: RwLock::new(HashMap::new()),
            cross_cache_live: std::sync::atomic::AtomicBool::new(false),
        };
        db.with_stored_compat()
    }

    /// Adopt the dialect and FK setting the FILE records (the `tune` record).
    ///
    /// This is the whole point of storing them. A config-free attach — `dump`,
    /// the mirror daemon, the C-API shim, `mpedb <file>` — has no `[compat]`
    /// section, and used to fall to the lenient sqlite default for both. A
    /// PostgreSQL mirror is imported STRICT and then reopened LENIENT by the
    /// very next tool that touched it, with nothing in the output to say so.
    ///
    /// A database with no record reads `Tunables::default()`, which is exactly
    /// the pair of defaults this code used before — so nothing that exists
    /// today changes behaviour.
    ///
    /// A corrupt record is an ERROR, not a fallback: silently opening lenient
    /// because the record would not parse is the same wrong answer by a
    /// different route.
    pub(super) fn with_stored_compat(mut self) -> Result<Database> {
        let t = self.tunables()?;
        self.dialect = t.dialect;
        self.fk_enforce
            .store(t.foreign_keys, std::sync::atomic::Ordering::Relaxed);
        Ok(self)
    }

    /// Open a **process-private in-memory** database (config `path = ":memory:"`).
    ///
    /// Equivalent to [`open_with_config`](Self::open_with_config) after forcing
    /// `path` to `":memory:"` and `durability` to `none`. Backing is a lazy
    /// memfd / unlinked temp (see `Shm::open_memory`) — no multi-process attach.
    ///
    /// For **multi-process RAM sharing**, use
    /// `path = ":memory:shared:<name>"` instead (same name in every process);
    /// see [`StorageKind`](mpedb_types::StorageKind) and
    /// [`Database::multi_process_hint`].
    pub fn open_in_memory(mut config: Config) -> Result<Database> {
        config.options.path = std::path::PathBuf::from(":memory:");
        config.options.durability = mpedb_types::Durability::None;
        Database::open_with_config(config)
    }

    pub fn open_with_config(mut config: Config) -> Result<Database> {
        // MPEDB_OPEN_TRACE=1: per-stage µs on stderr (the SP_TRACE pattern) —
        // the connect cell's attribution instrument (#N2 0.2.9).
        // `Instant::now()` PANICS on wasm32-unknown-unknown, and the browser
        // playground's open runs through here — so the clock is only ever
        // touched when tracing is actually on (env is always empty there).
        let mut t0 = std::env::var_os("MPEDB_OPEN_TRACE")
            .is_some()
            .then(std::time::Instant::now);
        let mut stage = |name: &str| {
            if let Some(prev) = t0 {
                eprintln!("[open] {name}: {}µs", prev.elapsed().as_micros());
                t0 = Some(std::time::Instant::now());
            }
        };
        // Resolve `:memory:` / `:memory:shared:name` before the engine opens.
        let (resolved, storage) = mpedb_types::resolve_storage_path(&config.options.path)?;
        config.options.path = resolved;
        let checks = compile_schema_checks(&config.schema)?;
        stage("checks");
        let path = config.options.path.clone();
        let dialect = config.options.dialect;
        let role = crate::sync::Role::parse(&config.options.sync_role)?;
        // Resolve the §6.3 assertions against the schema now: an unknown name is
        // a config error, not a no-op assertion nobody notices.
        let mut require_policy = std::collections::HashSet::new();
        for name in &config.options.require_policy {
            let id = config
                .schema
                .tables
                .iter()
                .position(|t| &t.name == name)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "require_policy names table `{name}`, which is not in the schema"
                    ))
                })?;
            require_policy.insert(id as u32);
        }
        // The seed bundle takes CHECKs only; `set_check_compiler` below rebuilds
        // it with the index predicates too, from the LIVE catalog schema.
        let seed_hash = config.schema.hash();
        let seed_programs = std::sync::Arc::new(checks);
        let mut engine = Engine::open(&config, seed_programs.checks.clone())?;
        stage("engine_open");
        // DESIGN-BLOBEXTENT §8: per-process knob, like durability. The format
        // self-describes, so this only decides what NEW writes do.
        engine.set_extent_threshold(config.options.extent_threshold);
        engine.set_external_extents(config.options.external_extents);
        // From here on the ENGINE recompiles CHECK programs itself whenever it
        // rebuilds a bundle from the catalog. The config-derived vector above
        // only covers the SEED schema; a table created by `CREATE TABLE …
        // CHECK (…)` — in this process or another — is picked up through this,
        // instead of landing in the catalog as a constraint that is stored and
        // never enforced.
        //
        // #N2 fase 5: the install-time rebuild recompiled the SAME programs
        // the seed compile just produced, on every open whose live catalog
        // still equals the seed — which is every open not racing a peer's
        // DDL. Key on the schema hash, the identity the attach gate already
        // trusts; any other schema (catalog grown past the seed, a later
        // rebuild after DDL) compiles as before.
        engine.set_check_compiler(std::sync::Arc::new(move |schema: &mpedb_types::Schema| {
            if schema.hash() == seed_hash {
                return Ok((*seed_programs).clone());
            }
            compile_schema_checks(schema)
        }))?;
        stage("check_compiler");
        let mut db = Database {
            engine,
            cache: RwLock::new(HashMap::new()),
            text_memo: RwLock::new(MemoMap::default()),
            spell_cache: RwLock::new(HashMap::new()),
            cache_gen: std::sync::atomic::AtomicU64::new(0),
            conn_id: next_conn_id(),
            rules_cache: RwLock::new(None),
            fk_enforce: std::sync::atomic::AtomicBool::new(config.options.foreign_keys),
            path,
            storage,
            require_policy,
            dialect,
            role,
            host_udfs: RwLock::new(HashMap::new()),
            host_aggs: RwLock::new(HashMap::new()),
            host_window_aggs: RwLock::new(std::collections::HashSet::new()),
            host_colls: RwLock::new(HashMap::new()),
            busy_timeout_ms: std::sync::atomic::AtomicI64::new(-1),
            attached: RwLock::new(Default::default()),
            temp_views: RwLock::new(Default::default()),
            cross_cache: RwLock::new(HashMap::new()),
            cross_cache_live: std::sync::atomic::AtomicBool::new(false),
        };
        db.reconcile_stored_compat(&config.options)?;
        stage("reconcile");
        Ok(db)
    }

    /// Settle the config's `[compat]` against the FILE's stored record.
    ///
    /// Three cases, and the shape follows from the file being authoritative:
    ///
    /// * **No record yet, and the config NAMES a non-default value** — seed it.
    ///   Every database is written once here, and from then on `open_from_file`
    ///   answers the same as the config did. This is the one write, and it is
    ///   what carries a PostgreSQL `mirror import`'s strictness into the file.
    /// * **A record exists and the config NAMES something else** — refuse, by
    ///   name, with both values. Silently preferring either one is how the bug
    ///   this fixes was born; a database that means `postgres` must not be
    ///   opened as `sqlite` because a stale TOML said so. `mpedb tune set` is
    ///   the way to change it, and the message says so.
    /// * **Anything else** — the record wins (or the shared defaults do).
    ///
    /// A config that names NOTHING never writes and never refuses. That is
    /// deliberate: most configs have no `[compat]` at all, and treating their
    /// silence as a demand for `sqlite` would make every such attach refuse a
    /// PostgreSQL database.
    pub(super) fn reconcile_stored_compat(&mut self, o: &mpedb_types::DbOptions) -> Result<()> {
        let stored = self.tunables()?;
        let have_record = self.sys_record_get(crate::costlayer::NS_TUNE, b"current")?.is_some();
        let mut want = stored.clone();

        if o.dialect_named {
            if have_record && stored.dialect != o.dialect {
                return Err(Error::Config(format!(
                    "compat.dialect = `{}` but the database records `{}`. The FILE \
                     decides the dialect — it travels with the data, and a config that \
                     silently overrode it is what let a PostgreSQL mirror reopen lenient. \
                     Change it deliberately with `mpedb tune set <db> dialect={}`, \
                     or drop the config key.",
                    crate::costlayer::dialect_name(o.dialect),
                    crate::costlayer::dialect_name(stored.dialect),
                    crate::costlayer::dialect_name(o.dialect),
                )));
            }
            want.dialect = o.dialect;
        }
        if o.foreign_keys_named {
            if have_record && stored.foreign_keys != o.foreign_keys {
                return Err(Error::Config(format!(
                    "compat.foreign_keys = {} but the database records {}. The FILE decides \
                     where enforcement starts; change it with \
                     `mpedb tune set <db> foreign_keys={}`, or drop the config key.",
                    o.foreign_keys, stored.foreign_keys, o.foreign_keys
                )));
            }
            want.foreign_keys = o.foreign_keys;
        }

        // Seed only when there is nothing recorded AND the config asked for
        // something other than the shared default. A default-valued config key
        // writes nothing, so an ordinary sqlite database never grows a record
        // it does not need.
        if !have_record && want != crate::costlayer::Tunables::default() {
            self.put_gen_bumped(crate::costlayer::NS_TUNE, &want.encode_public())?;
        }
        self.dialect = want.dialect;
        self.fk_enforce
            .store(want.foreign_keys, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// The SQL dialect this HANDLE compiles under.
    pub fn dialect(&self) -> mpedb_types::Dialect {
        self.dialect
    }

    /// Compile under a different dialect from here on — the per-SESSION half of
    /// the sqlite-vs-PostgreSQL switch.
    ///
    /// The file still RECORDS a dialect, and that record is still what a
    /// config-free attach starts from: it is the answer to "what does this data
    /// mean", and a `dump` or a mirror daemon must not silently loosen it. What
    /// this method adds is a session that deliberately says otherwise — a
    /// PostgreSQL wire connection into a database that also serves sqlite
    /// callers through the C-API shim, at the same time, over the same file.
    ///
    /// That is safe because the two dialects differ only in what SQL MEANS, not
    /// in what is stored: mpedb's columns are rigidly typed either way, so no
    /// row written under one dialect is unreadable under the other. The one
    /// thing that had to move for this to hold is the plan registry key, which
    /// now carries the dialect (`registry::plan_subkey`) — without that, the two
    /// sessions would evict each other's plans for the same SQL text.
    ///
    /// Selecting `Postgres` in a build without the `pg-dialect` feature is a
    /// named refusal at the first `prepare`, not here: this call only records an
    /// intent.
    /// A consistent raw image of the whole database, as bytes.
    ///
    /// The alternative — dumping to SQL and parsing it back — is what a wasm
    /// build has had to do, because there is no filesystem there to
    /// `VACUUM INTO`. This copies the pages that are already contiguous
    /// instead: no `SELECT` per table, no literal per value, no hex expansion
    /// per blob byte, and no parse on the way back.
    ///
    /// Blocks no writer. See `mpedb_core::backup` for why that is sound rather
    /// than optimistic.
    ///
    /// The result is the DATA region only, up to the snapshot's high water — so
    /// a mostly-empty 64 MiB database backs up to a few pages, not 64 MiB.
    pub fn backup(&self) -> Result<Vec<u8>> {
        self.engine.backup()
    }

    /// Restore a [`Database::backup`] into a database this process just created.
    ///
    /// The target must be empty and must have been created with the same
    /// `max_readers` — the reader table's width decides where the data region
    /// starts, so a mismatch would place every page at the wrong id. Both are
    /// refused by name rather than half-applied.
    pub fn restore_backup(&self, bytes: &[u8]) -> Result<()> {
        self.engine.restore_backup(bytes)?;
        // Every cached plan was compiled against the schema this file had a
        // moment ago, which was the seed's. The catalog is now someone else's.
        self.cache.write().expect(POISON).clear();
        Ok(())
    }

    pub fn set_dialect(&mut self, dialect: mpedb_types::Dialect) {
        if dialect != self.dialect {
            // The plan cache is keyed by hash alone and is per-handle, so it
            // would happily hand a sqlite plan to a PG statement with the same
            // hash. The registry key separates them on disk; this separates
            // them in memory.
            self.cache.write().expect(POISON).clear();
        }
        self.dialect = dialect;
    }

    /// How this handle is backed ([`mpedb_types::StorageKind`]).
    pub fn storage_kind(&self) -> mpedb_types::StorageKind {
        self.storage
    }

    /// Whether another OS process can attach by opening the same path/config.
    pub fn multi_process_attach_ok(&self) -> bool {
        self.storage.multi_process_attach()
    }

    /// Stable English hint for tools/LLMs: how multi-process attach works for
    /// this handle, or how to switch modes if it does not.
    pub fn multi_process_hint(&self) -> &'static str {
        self.storage.multi_process_hint()
    }

    /// Resolved path this handle attached (for shared memory, the tmpfs path).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The CURRENT schema (Arc'd bundle; derefs to [`Schema`]). DDL may swap
    /// it between calls — bind the Arc for a stable view.
    pub fn schema(&self) -> std::sync::Arc<mpedb_core::engine::SchemaBundle> {
        self.engine.schema()
    }

    /// Reload the schema bundle if a DDL commit (this process or another)
    /// bumped `schema_gen`. The C-API's `sqlite_master` path calls this before
    /// answering outside a write session, so a just-committed `CREATE TABLE`
    /// is visible on the next query without an intervening prepare.
    pub fn refresh_schema_if_stale(&self) -> Result<()> {
        self.engine.refresh_schema_if_stale()
    }

    /// Drop this handle's local plan caches. The hash-keyed [`Self::cache`] and
    /// the SQL-text [`Self::text_memo`] must always go together: the memo maps a
    /// TEXT to a hash, so a text whose meaning just changed has to lose both, or
    /// the next call re-derives the same stale hash from the memo and never
    /// notices the plan cache was cleared.
    ///
    /// The two locks are taken in separate statements and never held at once —
    /// `memoized` walks them in the opposite order (memo, then cache), which is
    /// only safe because neither site overlaps its guards.
    pub(super) fn drop_local_plans(&self) {
        self.cache.write().expect(POISON).clear();
        self.text_memo.write().expect(POISON).clear();
    }

    /// Register a HOST scalar UDF on this connection (the C-API
    /// `sqlite3_create_function` path, design/DESIGN-UDF.md). A SQL call
    /// `name(args)` that matches no built-in function then invokes `f` with the
    /// evaluated arguments. `n_arg` is the argument count, or `-1` for a variadic
    /// function that accepts any arity. Re-registering the same `(name, n_arg)`
    /// REPLACES the previous closure.
    ///
    /// A plan that calls a host UDF is valid only for the connection that
    /// registered it, so it is never published to the shared content-hashed plan
    /// registry — it is compiled and executed locally each time. Registering (or
    /// unregistering) therefore drops this connection's local plan cache: a
    /// cached plan may have resolved the name to the previous meaning, or errored
    /// on it as unknown.
    pub fn register_host_function<F>(&self, name: &str, n_arg: i32, f: F)
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        self.host_udfs
            .write()
            .expect(POISON)
            .insert((name.to_string(), n_arg), Arc::new(f));
        self.drop_local_plans();
    }

    /// Remove a host UDF registered with [`register_host_function`]. Returns
    /// whether an entry was present. Drops the local plan cache (see that method).
    pub fn unregister_host_function(&self, name: &str, n_arg: i32) -> bool {
        let removed = self
            .host_udfs
            .write()
            .expect(POISON)
            .remove(&(name.to_string(), n_arg))
            .is_some();
        if removed {
            self.drop_local_plans();
        }
        removed
    }

    /// Register a HOST AGGREGATE on this connection (the C-API `xStep`/`xFinal`
    /// path, design/DESIGN-UDF.md stage 2). A SQL call `name(arg)` then routes
    /// the whole SELECT through the aggregate planner and, per group, mints a
    /// fresh accumulator from `factory`, steps it once per surviving row (after
    /// `WHERE`, the policy predicate and any `FILTER`), and finishes it at the
    /// group's end. An EMPTY group still finishes a fresh accumulator, which is
    /// sqlite's rule (`xFinal` on a never-stepped context ⇒ typically NULL).
    ///
    /// The call shape is exactly ONE argument (`n_arg` 1, or `-1` for variadic);
    /// any other registered arity compiles to a clear error at the call site.
    /// Same plan-sharing rule as [`register_host_function`]: a plan naming a host
    /// aggregate never enters the shared registry, and registering drops this
    /// connection's local plan cache.
    pub fn register_host_aggregate<F>(&self, name: &str, n_arg: i32, factory: F)
    where
        F: Fn() -> Box<dyn mpedb_types::HostAggState> + Send + Sync + 'static,
    {
        self.host_aggs
            .write()
            .expect(POISON)
            .insert((name.to_string(), n_arg), Arc::new(factory));
        self.drop_local_plans();
    }

    /// Register a host aggregate that ALSO carries sqlite's WINDOW protocol —
    /// `create_window_function`, i.e. `xValue`/`xInverse` on top of
    /// `xStep`/`xFinal` (design/DESIGN-UDF.md stage 4).
    ///
    /// It is a strictly stronger registration than
    /// [`register_host_aggregate`]: the state can report a value WITHOUT being
    /// consumed, and can undo a row it was stepped with. That pair is exactly
    /// what a moving window frame needs, and it is why only a function
    /// registered here may take an `OVER` clause — a plain aggregate under a
    /// bounded frame would otherwise be answered with a running total, which is
    /// a wrong answer rather than a refusal.
    ///
    /// The factory's states MUST override [`HostAggState::inverse`] and
    /// [`HostAggState::value`]; the trait's defaults refuse, so a registration
    /// that forgets one fails at the first row rather than silently.
    pub fn register_host_window_aggregate<F>(&self, name: &str, n_arg: i32, factory: F)
    where
        F: Fn() -> Box<dyn mpedb_types::HostAggState> + Send + Sync + 'static,
    {
        self.host_window_aggs
            .write()
            .expect(POISON)
            .insert(name.to_string());
        self.register_host_aggregate(name, n_arg, factory);
    }

    /// Register a host COLLATING SEQUENCE (`sqlite3_create_collation`,
    /// design/DESIGN-UDF.md stage 3): a comparator over two strings, usable as
    /// `ORDER BY <expr> COLLATE <name>`.
    ///
    /// **What it can and cannot do.** It can order a sort. It cannot change a
    /// KEY: an index (or PRIMARY KEY) is stored in memcmp order under a
    /// built-in collation, and a callback cannot produce sort bytes, so a
    /// host-collated index probe, a host-collated `GROUP BY`/`DISTINCT` fold,
    /// and a host collation on a column's declared `COLLATE` are all refused by
    /// name ("no such collation sequence") rather than answered under BINARY.
    /// The refusals are structural — those APIs take a built-in
    /// [`Collation`](mpedb_types::Collation), which no registration produces.
    ///
    /// A built-in name (BINARY/NOCASE/RTRIM) can never be shadowed. Registering
    /// again under the same name REPLACES the comparator (sqlite's rule) and
    /// drops this connection's local plan cache, so the next compile sees it.
    pub fn register_host_collation<F>(&self, name: &str, cmp: F)
    where
        F: Fn(&str, &str) -> std::cmp::Ordering + Send + Sync + 'static,
    {
        self.host_colls
            .write()
            .expect(POISON)
            .insert(name.to_string(), Arc::new(cmp));
        self.drop_local_plans();
    }

    /// Remove a host collation registered with [`register_host_collation`]
    /// (sqlite's `create_collation(name, None)`). Returns whether one was
    /// present. A plan that already named it then fails with sqlite's
    /// "no such collation sequence: <name>" at execution — never a silent
    /// fallback to BINARY, which would be a different row ORDER with no error.
    pub fn unregister_host_collation(&self, name: &str) -> bool {
        let removed = self.host_colls.write().expect(POISON).remove(name).is_some();
        if removed {
            self.drop_local_plans();
        }
        removed
    }

    /// Remove a host aggregate registered with [`register_host_aggregate`].
    /// Returns whether an entry was present. Drops the local plan cache.
    pub fn unregister_host_aggregate(&self, name: &str, n_arg: i32) -> bool {
        let removed = self
            .host_aggs
            .write()
            .expect(POISON)
            .remove(&(name.to_string(), n_arg))
            .is_some();
        // The window flag is per NAME, so it survives only while some arity of
        // that name is still registered.
        if removed && !self.host_aggs.read().expect(POISON).keys().any(|(n, _)| n == name) {
            self.host_window_aggs.write().expect(POISON).remove(name);
        }
        if removed {
            self.drop_local_plans();
        }
        removed
    }

    /// The names + arities of the registered host UDFs, for the binder to resolve
    /// calls against (values never leave the registry — only names/arities reach
    /// compile). Empty when none are registered, so compilation is unchanged.
    /// Aggregates ride along: the PARSER needs them (see `HostUdfSet`).
    pub(super) fn host_udf_set(&self) -> HostUdfSet {
        let f = self.host_udfs.read().expect(POISON);
        let a = self.host_aggs.read().expect(POISON);
        let c = self.host_colls.read().expect(POISON);
        let mut set = HostUdfSet::with_aggs(f.keys().cloned().collect(), a.keys().cloned().collect());
        set.set_colls(c.keys().cloned().collect());
        set.set_window_aggs(self.host_window_aggs.read().expect(POISON).iter().cloned().collect());
        set
    }

    /// Snapshot the registry's closures for one execution (see [`HostFnTable`]).
    pub(super) fn host_fn_table(&self, plan: &CompiledPlan) -> HostFnTable {
        let g = self.host_udfs.read().expect(POISON);
        HostFnTable {
            fns: g.iter().map(|((n, a), f)| (n.clone(), *a, f.clone())).collect(),
            // Loaded only for a spell-bearing plan; an error loading the
            // catalog degrades to an empty table, and the call site then
            // refuses by hash with the re-prepare message — fail closed,
            // never fail open.
            spells: if plan.contains_spell_call() {
                self.spell_table().unwrap_or_default()
            } else {
                Vec::new()
            },
        }
    }

    /// Snapshot the collating sequences for one execution (see [`HostCollTable`]).
    pub(super) fn host_coll_table(&self) -> HostCollTable {
        let g = self.host_colls.read().expect(POISON);
        HostCollTable {
            colls: g.iter().map(|(n, f)| (n.clone(), f.clone())).collect(),
        }
    }

    /// Snapshot the aggregate factories for one execution (see [`HostAggTable`]).
    pub(super) fn host_agg_table(&self) -> HostAggTable {
        let g = self.host_aggs.read().expect(POISON);
        HostAggTable {
            aggs: g.iter().map(|((n, a), f)| (n.clone(), *a, f.clone())).collect(),
        }
    }

    /// The closures ONE statement needs, or `None` when its plan calls no host
    /// UDF — the single gate every execution path (read, write, session, ring
    /// leader) goes through, so "which executions carry host closures" has one
    /// answer in one place.
    ///
    /// Snapshotting only for a `contains_host_call()` plan keeps the registry
    /// locks entirely off the hot path of every ordinary statement: a database
    /// with no UDFs registered, or a statement that calls none, does exactly what
    /// it did before.
    pub(crate) fn host_tables(
        &self,
        plan: &CompiledPlan,
    ) -> Option<(HostFnTable, HostAggTable, HostCollTable)> {
        // Spell-bearing plans need the table too (their gate is the same
        // seam); a plan with neither keeps the registry locks off its path.
        //
        // A plan that WRITES a table whose column DEFAULT calls a host UDF
        // needs it as well, and this is the case the plan alone cannot report:
        // the call is in the SCHEMA's default program, not in the plan bytes,
        // so `contains_host_call()` is false. Missing it did not degrade to a
        // refusal — it made every such INSERT store NULL, which is exactly the
        // wrong answer the eval site's "not in scope" error exists to prevent.
        let default_needs_host = || {
            let bundle = self.schema();
            bundle.schema.tables.iter().any(|t| {
                !t.dead
                    && plan.footprint.writes_table(t.id)
                    && t.columns.iter().any(|c| {
                        matches!(&c.default, Some(mpedb_types::DefaultExpr::Expr(d))
                            if d.program.has_host_call())
                    })
            })
        };
        (plan.contains_host_call() || plan.contains_spell_call() || default_needs_host()).then(
            || (self.host_fn_table(plan), self.host_agg_table(), self.host_coll_table()),
        )
    }

    /// Compile `sql` with this database's RLS policies injected (loaded from the
    /// catalog sys-keyspace on a pinned read snapshot, design/DESIGN-MULTIDB.md §3).
    /// The bool is the `EXPLAIN` flag. An empty policy set behaves exactly as
    /// plain compilation.
    /// Refresh this process's schema bundle if another process bumped the meta
    /// `schema_gen`, and — critically — drop the local plan cache when the gen
    /// actually moved. A cached plan is bound to a specific catalog layout; a
    /// DROP / re-CREATE (id reused in place) / ALTER since it was compiled makes
    /// it stale, and cache HITS never recompile. One `newest_meta` read in the
    /// common (unchanged) case; the cache is cleared only on a real gen change.
    /// The set of table ids the workload MODEL marks columnar (scan-heavy) — the
    /// stable cost signal `CostSource::columnar` reports (stage C / DESIGN-COLUMNAR
    /// §7.1). A `fact` is columnar, a `dimension` is not, an unroled table follows
    /// a `star-olap` archetype (the same rule as `sync_columnar`). Empty without a
    /// model. This governs plan CHOICE, so it must be stable per prepare — the
    /// model is read once from the compile's snapshot, and `set_model` bumps
    /// `schema_gen` so a changed model re-prepares plans.
    pub(super) fn columnar_cost_tables(
        &self,
        model: Option<&mpedb_types::model::WorkloadModel>,
    ) -> std::collections::HashSet<u32> {
        let mut set = std::collections::HashSet::new();
        let Some(model) = model else { return set };
        let scan_archetype =
            matches!(model.archetype, Some(mpedb_types::model::Archetype::StarOlap));
        let bundle = self.schema();
        for t in bundle.schema.tables.iter().filter(|t| !t.dead) {
            let role = model
                .tables
                .iter()
                .find(|m| mpedb_types::ident_eq(&m.name, &t.name))
                .and_then(|m| m.role);
            let eligible = match role {
                Some(mpedb_types::model::TableRole::Fact) => true,
                Some(mpedb_types::model::TableRole::Dimension) => false,
                _ => scan_archetype,
            };
            if eligible {
                set.insert(t.id);
            }
        }
        set
    }

    pub(super) fn gate_cache_on_schema(&self) -> Result<()> {
        self.engine.refresh_schema_if_stale()?;
        let gen = self.engine.schema().schema_gen;
        // Relaxed is fine: DDL is serialized under the writer lock, and a
        // same-instant race resolves at the txn's own captured snapshot. This
        // gate closes the common case — a DDL that committed in a PRIOR
        // statement — which is the one that would otherwise execute a stale plan.
        if self.cache_gen.swap(gen, std::sync::atomic::Ordering::Relaxed) != gen {
            // The text memo stores its own `gen` per entry, so a stale one
            // would MISS rather than be served — dropping it here is about
            // space, not safety: entries compiled against a catalog nobody will
            // ask about again would otherwise occupy the bounded map and evict
            // live ones (#166).
            self.drop_local_plans();
        }
        Ok(())
    }

    pub(super) fn compile_maybe_explain(&self, sql: &str) -> Result<(CompiledPlan, bool)> {
        // #47 stage 3/4: pick up another process's CREATE/DROP/ALTER before we
        // compile — a query against a just-created table must see it, one
        // against a just-dropped table must fail to bind, and a stale cached
        // plan must be evicted (the txn-begin reload happens too late, and a
        // cache hit skips compilation entirely).
        self.gate_cache_on_schema()?;
        let catalog = self.load_policy_catalog()?;
        let views = self.load_view_catalog()?;
        // #114: the MPEE join solver's only statistic — the catalog's exact
        // per-table row count, read on one pinned snapshot and consumed by the
        // planner as a magnitude bucket (design/DESIGN-MPEE-SOLVER.md §2.1).
        // The read pin is held only for the compile, which happens once per
        // statement; `execute(hash, params)` never comes near it.
        let r = self.engine.begin_read()?;
        let bundle = self.schema();
        // Stage M5: the stored cost layer — tunables, the policy spell, and
        // the model's archetype — read off the SAME snapshot as every other
        // cost input, so all attached processes price identically.
        let tune = self.tunables()?;
        let policy = self.load_cost_policy(&r)?;
        if let Some(p) = &policy {
            // The probe: a policy that cannot even run fails the PREPARE with
            // its own name. Per-input errors after this degrade that one
            // decision to no-discount — deterministically, since every
            // process runs the same spell on the same inputs.
            costlayer::apply_policy(p, "probe", "", 0, 1, 1, "")?;
        }
        let model = self.model()?;
        let archetype: String = model
            .as_ref()
            .and_then(|m| m.archetype)
            .map(|a| a.name().to_string())
            .unwrap_or_default();
        let columnar_tables = self.columnar_cost_tables(model.as_ref());
        let schema_for_cost = bundle.clone();
        let cost = mpedb_sql::CostSource {
            row_count: &|tid| r.row_count(tid).unwrap_or(0),
            columnar: &|tid| columnar_tables.contains(&tid),
            // Stage A's per-index NDV bucket, now behind the M5 layer: the
            // `ndv_discount` tunable can switch the channel off, and the
            // policy spell — if one is stored — adjusts every bucket it sees.
            index_ndv_bucket: &|tid, ixno| {
                if !tune.ndv_discount {
                    return None;
                }
                let base = ndv_bucket_from(&r, &bundle.schema, tid, ixno)?;
                match &policy {
                    None => Some(base),
                    Some(p) => {
                        let table = schema_for_cost
                            .schema
                            .tables
                            .get(tid as usize)
                            .map(|t| t.name.as_str())
                            .unwrap_or("");
                        let rows_bucket = stats::bucket(r.row_count(tid).unwrap_or(0));
                        // Per-input errors degrade THIS decision to
                        // no-discount — deterministically (same spell, same
                        // inputs, every process); the probe above already
                        // failed the prepare for a policy that cannot run.
                        match costlayer::apply_policy(
                            p, "ndv", table, ixno, base, rows_bucket, &archetype,
                        ) {
                            Ok(b) => (b > 0).then_some(b),
                            Err(_) => None,
                        }
                    }
                }
            },
        };
        // The function catalogs for this compile: connection-local host UDFs
        // plus the FILE's stored functions, read off the same snapshot as the
        // cost inputs (stage M2).
        let mut udfs = self.host_udf_set();
        udfs.spells = self.load_spell_fns(&r)?;
        udfs.ops = self.load_ops(&r)?;
        let out = mpedb_sql::prepare_maybe_explain_with_views(
            sql,
            &bundle,
            &catalog,
            &views,
            self.dialect,
            &udfs,
            &cost,
        );
        r.finish()?;
        out
    }

    /// Per output column, the `decltype` to report through the C-API shim
    /// (`sqlite3_column_decltype`) / Python `cursor.description[*][1]`. Compiles
    /// `sql` — surfacing any bind/plan error exactly as prepare would — and
    /// derives each column's declared type from the plan's projection (see
    /// [`CompiledPlan::output_decltypes`]): a bare base-table column reports its
    /// type, everything computed reports `None`. A non-SELECT yields an empty
    /// vec (all NULL). This does not execute or publish a plan.
    pub fn output_decltypes(&self, sql: &str) -> Result<Vec<Option<String>>> {
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(plan.output_decltypes(&self.schema()))
    }

    /// PostgreSQL result types derived from the compiled projection, including
    /// explicit casts and write statements with RETURNING. No SQL is executed.
    pub fn output_pg_oids(&self, sql: &str) -> Result<Vec<Option<u32>>> {
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(plan.output_pg_oids(&self.schema()))
    }

    /// PostgreSQL view/query column metadata, without executing the query.
    pub fn output_pg_columns(&self, sql: &str) -> Result<Vec<mpedb_sql::PgOutputColumn>> {
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(plan.output_pg_columns(&self.schema()))
    }

    /// Per output column, the base TABLE to report through the C-API shim
    /// (`sqlite3_column_table_name`). Compiles `sql` — surfacing any bind/plan
    /// error exactly as prepare would — and derives each column's source table
    /// from the plan's projection (see [`CompiledPlan::output_table_names`]):
    /// a bare base-table column reports its table, everything computed reports
    /// `None`. A non-SELECT yields an empty vec (all NULL). This does not
    /// execute or publish a plan.
    pub fn output_table_names(&self, sql: &str) -> Result<Vec<Option<String>>> {
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(plan.output_table_names(&self.schema()))
    }

    /// What `sql` would TOUCH, at column granularity, without running it: the
    /// report an authorization layer, an audit log or a policy gate consults
    /// before a statement is allowed through (see [`plan_access`]).
    ///
    /// Compiles through exactly the same path `prepare`/`execute` do — same
    /// policies, views and host-UDF set — so an authorized statement and the
    /// executed statement can never be two different plans. Any bind/plan error
    /// surfaces here just as it would at prepare. Nothing is executed and no
    /// plan is published.
    /// What an `ATTACH`/`DETACH` names, parsed with this database's dialect and
    /// nothing applied — the shim needs it to raise `SQLITE_ATTACH` /
    /// `SQLITE_DETACH` for an authorizer, which is the gate PHP's `open_basedir`
    /// enforcement runs through. `None` for any other statement.
    pub fn attach_target(&self, sql: &str) -> Result<Option<AttachStmt>> {
        mpedb_sql::parse_attach_dialect(sql, self.dialect)
    }

    pub fn access_report(&self, sql: &str) -> Result<AccessReport> {
        // DDL compiles to no plan — the facade applies it against the catalog
        // directly — so it is described by its parse, not by a footprint.
        if let Some(ddl) = self.parse_ddl(sql)? {
            return Ok(access::ddl_access(&ddl));
        }
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok(access::plan_access(&plan, &self.schema()))
    }

    /// The compiled plan's **identity and footprint**, without executing or
    /// publishing anything: `(plan hash, what the statement touches)`.
    ///
    /// The footprint is the routing / *shape* key — the table sets, the index
    /// bitmap and the key access a plan will use, all known before a single row
    /// moves ([`ShardSet`] already routes on it). It is deliberately NOT an
    /// identity: many distinct plans share one footprint, which is exactly what
    /// makes it usable as an INDEX over plans with the hash as the final
    /// discriminator. Compiles through the same path `prepare`/`execute` do, so
    /// the pair is the one the executed statement will carry. DDL compiles to no
    /// plan and yields `Unsupported` (use [`Database::access_report`] there).
    /// Where `table_id`'s change counter stands, and the key digest of the
    /// last change (0 = "somewhere in this table"). `None` means the
    /// notification slot currently belongs to another table that hashes there,
    /// so nothing can be said — a false wakeup, never a false "unchanged".
    ///
    /// This is the observation half of change notification (#139); the waiting
    /// half is [`Database::wait_for_change`].
    pub fn change_generation(&self, table_id: u32) -> Option<(u64, u64)> {
        self.engine.change_generation(table_id)
    }

    /// Park until one of `tables` changes past the generations in `seen`, or
    /// `timeout` elapses. Returns the tables that moved.
    ///
    /// Unlike Postgres's LISTEN/NOTIFY this needs no global commit lock,
    /// because it makes no cross-table ordering promise: per table the writer
    /// lock already orders commits, and a listener reads what changed from its
    /// own MVCC snapshot rather than from a queued payload.
    pub fn wait_for_change(
        &self,
        tables: &[u32],
        seen: &[u64],
        timeout: std::time::Duration,
    ) -> Vec<u32> {
        self.engine.wait_for_change(tables, seen, timeout)
    }

    /// How many listeners are parked on this database right now. A commit
    /// reads it to decide whether the wake syscalls are worth issuing at all;
    /// a test or benchmark reads it to know its listeners are actually parked
    /// before it starts measuring.
    pub fn notify_waiter_count(&self) -> u32 {
        self.engine.notify_waiter_count()
    }

    /// Begin a write session guarded against everything committed since
    /// `snap` (#142 G1), with the guarded surface **declared up front** as the
    /// union of `may_run`'s footprints.
    ///
    /// The statements are compiled, not executed. Listing them says *these are
    /// the operations this action might perform* — any number of INSERTs,
    /// UPDATEs and DELETEs across any number of tables — and the guard then
    /// covers exactly their union and nothing more. That is what makes the
    /// lock the smallest one that still covers the action.
    ///
    /// Declaring up front rather than growing as statements run matters for
    /// three reasons:
    ///
    /// 1. **Stability.** A branch not taken would otherwise shrink the surface,
    ///    so two workers running the same logical action would guard different
    ///    things. A shard needs an identity that does not depend on which way
    ///    an `if` went — which is also what #142 G2's lease and G3's queue
    ///    dispatch key on.
    /// 2. **Minimality.** The alternative to declaring is guarding a whole
    ///    table set, or everything. Here you pay for precisely the statements
    ///    you might issue.
    /// 3. **It is nearly free.** Compilation is content-hash cached, so
    ///    declaring the same action repeatedly costs a lookup, not a compile.
    ///
    /// Execution still widens (see `WriteSession::query`): declaring wrong
    /// makes the guard bigger, never wrong.
    pub fn begin_guarded_for(&self, snap: u64, may_run: &[&str]) -> Result<WriteSession<'_>> {
        let with: Vec<(&str, &[Value])> = may_run.iter().map(|s| (*s, &[][..])).collect();
        self.begin_guarded_with(snap, &with)
    }

    /// [`Database::begin_guarded_for`] with the parameter values, which is what
    /// makes the declaration precise instead of merely sound.
    ///
    /// A declaration is a promise about what the action MAY do, and the guard
    /// has to be able to keep that promise for a statement that never runs.
    /// `UPDATE doc SET body = $1 WHERE id = $2` with no values names **every**
    /// row of `doc`, so the string-only form must widen to the whole table —
    /// correct, and exactly the granularity the guard had before key regions
    /// (#143) existed. Supply the values and the same statement names one row,
    /// which is what lets a thousand editors on a thousand documents stop
    /// meeting each other.
    ///
    /// **This is not an optimisation, it is the guarantee.** A read taken
    /// outside the session — the whole point of a guard, since the think time
    /// belongs outside any lock — is covered only because it was declared.
    /// Nothing else can cover it: no statement inside the session touches that
    /// row.
    /// This process's guard verdict tally: `(cleared, overlap, snapshot_too_old,
    /// ring_gap, deadline_expired)`.
    ///
    /// A refusal count alone cannot tell a real conflict from a limit of the
    /// machinery, and the two call for opposite fixes — so the counter that
    /// separates them is part of the API rather than a debug aid.
    /// This process's sync role (#157, `[sync] role`).
    ///
    /// A deployment fact, not a file property: the same `.mpedb` may be open as
    /// a replica here and standalone in the next process.
    pub fn role(&self) -> crate::sync::Role {
        self.role
    }

    pub fn guard_stats(&self) -> (u64, u64, u64, u64, u64) {
        self.engine.guard_stats()
    }

    pub fn begin_guarded_with(
        &self,
        snap: u64,
        may_run: &[(&str, &[Value])],
    ) -> Result<WriteSession<'_>> {
        let mut s = self.begin_guarded(snap)?;
        for (sql, params) in may_run {
            let (plan, _) = self.compile_maybe_explain(sql)?;
            for t in plan.footprint.tables_read.iter() {
                s.txn.guard_widen(t);
            }
            for t in plan.footprint.tables_written.iter() {
                s.txn.guard_widen(t);
            }
            // `None` on either of these means "anywhere" / "every column" —
            // the fail-safe direction, and the only honest reading of a
            // statement whose parameters are not known yet.
            s.txn.guard_touch_key(ring_exec::plan_key(&plan, params));
            // #151: the byte range is in the request too — `splice(body, $2,
            // $3, …)` carries it in the same parameters the key comes from —
            // so the finest dimension is decided before any work happens, and
            // a collision is one integer comparison.
            s.txn.guard_touch_range(ring_exec::plan_range(&plan, params));
            match ring_exec::plan_cols(&plan) {
                Some(m) => {
                    for b in 0..64u16 {
                        if m & (1u64 << b) != 0 {
                            s.txn.guard_touch_col(Some(b));
                        }
                    }
                }
                None => s.txn.guard_touch_col(None),
            }
        }
        Ok(s)
    }

    /// [`Database::begin_guarded_for`] with an empty declaration: the surface
    /// starts at nothing and is built only from what actually runs. Correct,
    /// and the right choice when the action is a fixed short sequence — but it
    /// gives no stable shard identity before execution, so `begin_guarded_for`
    /// is what G2/G3 build on.
    /// [`Database::begin_guarded_for`] under a session context (#144).
    ///
    /// When the session carries the key a table's RLS policy is written
    /// against — `app.tenant` and friends — the guard identifies itself by
    /// **that value** rather than by the rows it touched, and two actions under
    /// different values stop conflicting entirely.
    ///
    /// The identity is only taken when it can be PROVEN: every table the
    /// statements touch must carry a policy on that key, because the policy is
    /// what makes "different tenant ⇒ different rows" a fact. One uncovered
    /// table and the guard falls back to the row-level behaviour, which is
    /// correct and simply coarser.
    ///
    /// **The trust model is RLS's, and RLS in one file is cooperative
    /// (design/DESIGN-MULTIDB.md).** A caller that asserts the wrong tenant
    /// already writes another tenant's rows today, so this adds no failure
    /// mode — but it does tie the guard's precision to the policies actually
    /// being configured, which is worth knowing before relying on it.
    pub fn begin_guarded_as(
        &self,
        snap: u64,
        session: &Session,
        may_run: &[&str],
    ) -> Result<WriteSession<'_>> {
        let mut s = self.begin_guarded_for(snap, may_run)?;
        s.session = session.clone();
        Ok(s)
    }

    pub fn begin_guarded(&self, snap: u64) -> Result<WriteSession<'_>> {
        Ok(WriteSession {
            db: self,
            txn: self.engine.begin_write_guarded(snap)?,
            session: Session::empty(),
            poisoned: false,
            savepoints: Vec::new(),
            ddl_applied: false,
            validated: Default::default(),
        })
    }

    /// The newest committed txn id — the token [`Database::begin_guarded_for`]
    /// is taken against. A listener that just woke should use
    /// [`Listener::snapshot`] instead, which is the same value captured at the
    /// moment it observed the change.
    pub fn snapshot_txn(&self) -> u64 {
        self.engine.snapshot_txn()
    }

    /// A resumable [`Listener`] over `tables`, positioned at the present:
    /// only changes committed AFTER this call are reported (#141 S3).
    pub fn listen(&self, tables: &[u32]) -> Listener<'_> {
        self.listen_keyed(tables, &vec![None; tables.len()])
    }

    /// [`Database::listen`], narrowed to one key per table. `keys[i] = None`
    /// takes every change to `tables[i]`.
    pub fn listen_keyed(&self, tables: &[u32], keys: &[Option<Vec<Value>>]) -> Listener<'_> {
        let mut l = Listener {
            db: self,
            snap_txn: 0,
            tables: tables.to_vec(),
            keys: keys.to_vec(),
            epoch: 0,
            seen: vec![0; tables.len()],
        };
        l.advance();
        l
    }

    /// The notification region's incarnation stamp (#140). Exposed for the
    /// same reason [`ChangeCursor`] carries it: a generation is meaningless
    /// without it.
    pub fn notify_epoch(&self) -> u64 {
        self.engine.notify_epoch()
    }

    /// [`Database::wait_for_change`], narrowed to a key per table (#141 N2).
    ///
    /// `keys[i]` is the primary-key value list this listener cares about in
    /// `tables[i]`, or `None` for every change to that table. A table is
    /// reported only when the commit's advertised key region could contain
    /// that key — and a region that cannot be evaluated always could, so the
    /// filter costs spurious wakeups and never a missed one.
    pub fn wait_for_change_keyed(
        &self,
        tables: &[u32],
        seen: &[u64],
        keys: &[Option<Vec<Value>>],
        timeout: std::time::Duration,
    ) -> Vec<u32> {
        let encoded: Vec<Option<Vec<u8>>> =
            keys.iter().map(|k| k.as_ref().map(|v| keycode::encode_key(v))).collect();
        let refs: Vec<Option<&[u8]>> = encoded.iter().map(|e| e.as_deref()).collect();
        self.engine.wait_for_change_keyed(tables, seen, &refs, timeout)
    }

    pub fn plan_footprint(&self, sql: &str) -> Result<(PlanHash, Footprint)> {
        if self.parse_ddl(sql)?.is_some() {
            return Err(Error::Unsupported(
                "DDL compiles to no plan and has no footprint".into(),
            ));
        }
        let (plan, _explain) = self.compile_maybe_explain(sql)?;
        Ok((plan.hash(), plan.footprint.clone()))
    }

}
