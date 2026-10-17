//! Database query planning, execution and persistent plan registry.
use super::*;

impl Database {
    /// Compile `sql` against an EXPLICIT schema bundle — a [`WriteSession`]'s
    /// own view, which may already reflect DDL applied earlier in the SAME
    /// uncommitted transaction (#95). Unlike [`compile_maybe_explain`], there is
    /// no schema-gen gate: the session holds the single writer lock, so no peer
    /// DDL can move the committed schema underneath it, and its own uncommitted
    /// DDL is exactly what `schema` carries. Policies and views still come from
    /// the COMMITTED catalog (a policy/view created in the same uncommitted
    /// session is not yet visible — the pre-#95 contract, unchanged).
    pub(super) fn compile_maybe_explain_with_schema(
        &self,
        sql: &str,
        schema: &mpedb_core::engine::SchemaBundle,
    ) -> Result<(CompiledPlan, bool)> {
        let catalog = self.load_policy_catalog()?;
        let views = self.load_view_catalog()?;
        // Row counts for the MPEE solver come off an independent read snapshot
        // (the last COMMITTED state), not off this session's uncommitted write
        // txn: they are cost hints quantized to a magnitude bucket, so the
        // session's own in-flight row deltas can never move the chosen plan —
        // which is what keeps an in-session compile and an ordinary compile of
        // the same SQL landing on the same plan hash.
        let r = self.engine.begin_read()?;
        let tune = self.tunables()?;
        let policy = self.load_cost_policy(&r)?;
        if let Some(p) = &policy {
            costlayer::apply_policy(p, "probe", "", 0, 1, 1, "")?;
        }
        let model = self.model()?;
        let archetype: String = model
            .as_ref()
            .and_then(|m| m.archetype)
            .map(|a| a.name().to_string())
            .unwrap_or_default();
        let columnar_tables = self.columnar_cost_tables(model.as_ref());
        let cost = mpedb_sql::CostSource {
            row_count: &|tid| r.row_count(tid).unwrap_or(0),
            columnar: &|tid| columnar_tables.contains(&tid),
            index_ndv_bucket: &|tid, ixno| {
                if !tune.ndv_discount {
                    return None;
                }
                let base = ndv_bucket_from(&r, &schema.schema, tid, ixno)?;
                match &policy {
                    None => Some(base),
                    Some(p) => {
                        let table = schema
                            .schema
                            .tables
                            .get(tid as usize)
                            .map(|t| t.name.as_str())
                            .unwrap_or("");
                        let rows_bucket = stats::bucket(r.row_count(tid).unwrap_or(0));
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
        let mut udfs = self.host_udf_set();
        udfs.spells = self.load_spell_fns(&r)?;
        udfs.ops = self.load_ops(&r)?;
        let out = mpedb_sql::prepare_maybe_explain_with_views(
            sql,
            schema,
            &catalog,
            &views,
            self.dialect,
            &udfs,
            &cost,
        );
        r.finish()?;
        out
    }

    /// Prepare-time worst-case **risk estimate** (#74 layer 1) for an
    /// already-compiled plan, using the catalog's transactionally-exact row
    /// counts. Read-only: opens one read snapshot, executes nothing, and never
    /// touches plan bytes. See [`risk::estimate_plan_risk`].
    pub fn estimate_risk_for_plan(&self, plan: &CompiledPlan) -> Result<RiskEstimate> {
        let bundle = self.schema();
        let r = self.engine.begin_read()?;
        let est = risk::estimate_plan_risk(plan, &bundle.schema, &|tid| {
            r.row_count(tid).unwrap_or(0)
        });
        r.finish()?;
        Ok(est)
    }

    /// Compile `sql` and return its prepare-time risk estimate (#74) — the MPEE
    /// "answer at the start". Does not publish the plan or execute it.
    pub fn estimate_risk_sql(&self, sql: &str) -> Result<RiskEstimate> {
        let plan = self.compile_maybe_explain(sql)?.0;
        self.estimate_risk_for_plan(&plan)
    }

    /// #74 layer-1 surface: for a plan that structurally multiplies, log a
    /// warning when its worst-case estimate exceeds the warn threshold
    /// (`max_work_rows` when finite, else [`RISK_WARN_CEILING`]), naming the
    /// dominant node. Best-effort and advisory — a failure to open the read
    /// snapshot is swallowed, and this never refuses (the hard-refuse hook is
    /// [`RiskEstimate::exceeds`], left opt-in). Cheap: single-table plans skip
    /// the estimate (and its row-count reads) entirely.
    pub(super) fn warn_if_risky(&self, plan: &CompiledPlan) {
        if !RiskEstimate::plan_can_multiply(plan) {
            return;
        }
        let budget = self.engine.work_budget();
        let threshold = if budget != 0 { budget } else { RISK_WARN_CEILING };
        if let Ok(est) = self.estimate_risk_for_plan(plan) {
            if est.work_rows > threshold {
                eprintln!(
                    "mpedb: runtime-budget risk — worst-case ~{} work-rows \
                     (dominant: {}) exceeds {}; this query is likely to hit \
                     [runtime] max_work_rows",
                    est.work_rows, est.dominant, threshold
                );
            }
        }
    }

    /// Compile `sql` to a content-hashed plan and publish it in the shared
    /// registry. Idempotent: statements differing only in whitespace, keyword
    /// case, or `?`/`$n` spelling produce the same hash.
    ///
    /// Read-first (reviewed invariant): local cache, then a registry probe in
    /// a *read* transaction; only a genuine miss opens a short write
    /// transaction to insert the entry — a read-mostly workload never touches
    /// the writer lock here.
    ///
    /// Must NOT be called while a [`WriteSession`] from this handle is open
    /// on the same thread (see the crate-level locking rules).
    pub fn prepare(&self, sql: &str) -> Result<PlanHash> {
        // #51: `main.` qualifiers strip; a cross-file statement compiles into
        // the connection-local cross cache (like host-UDF plans, it is never
        // published — its table ids only mean anything on this handle).
        let routed;
        let sql = match self.resolve_db_refs_hook(sql)? {
            multifile::DbRoute::Passthrough => sql,
            multifile::DbRoute::Main(s) => {
                routed = s;
                &routed
            }
            multifile::DbRoute::Cross { sql, tables, .. } => {
                let guard = self.attached.read().expect(POISON);
                let (cp, _) = self.compile_cross(&guard, &sql, &tables)?;
                let hash = cp.plan.hash();
                self.cross_cache
                    .write()
                    .expect(POISON)
                    .insert(hash, Arc::new(cp));
                self.cross_cache_live
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return Ok(hash);
            }
            multifile::DbRoute::AttachedOnly { db, sql } => {
                // Prepare on the member handle so execute-by-hash stays local.
                let guard = self.attached.read().expect(POISON);
                let i = guard
                    .find(&db)
                    .ok_or_else(|| mpedb_types::Error::Bind(format!("no such database: {db}")))?;
                return guard.members[i].db.prepare(&sql);
            }
        };
        let plan = self.compile_maybe_explain(sql)?.0;
        self.warn_if_risky(&plan);
        let hash = plan.hash();
        self.register(hash, plan, sql)?;
        Ok(hash)
    }

    /// Execute a previously prepared plan by hash. The plan is taken from the
    /// local cache, or loaded (and fully re-validated) from the shared
    /// registry — so a hash prepared by *any* process works here.
    ///
    /// Errors: [`Error::UnknownPlan`] if the hash is in neither place (or the
    /// registry entry is corrupt) — re-`prepare` from SQL;
    /// [`Error::PlanInvalidated`] if the plan predates a schema change.
    pub fn execute(&self, hash: &PlanHash, params: &[Value]) -> Result<ExecResult> {
        self.execute_ctx(&Session::empty(), hash, params)
    }

    /// Like [`execute`](Self::execute) but with a [`Session`] whose values fill
    /// the plan's `current_setting()` references (design/DESIGN-MULTIDB.md §2). `params`
    /// are the caller-facing parameters only; the reserved context slots are
    /// filled from `session` (fail-closed on a missing key / NULL / wrong type).
    pub fn execute_ctx(
        &self,
        session: &Session,
        hash: &PlanHash,
        params: &[Value],
    ) -> Result<ExecResult> {
        // #51: a hash prepared from a cross-file statement lives in the
        // connection-local cross cache and runs on the multi-file read path
        // (epoch- and per-member-schema-gen-checked inside).
        // Most connections never ATTACH; skip the cross-cache lock entirely.
        if self
            .cross_cache_live
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            if let Some(res) = self.execute_cross_cached(session, hash, params)? {
                return Ok(res);
            }
        }
        // Evict a plan whose catalog changed underneath it before serving it
        // from cache (a DROP / re-CREATE / ALTER since prepare). Cheap in the
        // common case: one `newest_meta` read, no clear.
        self.gate_cache_on_schema()?;
        let plan = self.cached_or_load_tls(hash)?;
        let full = session::resolve_params_timed(&plan, params, session)?;
        self.run_plan(Some(hash), &plan, &full)
    }

    /// Prepare a reusable SELECT handle (sqlite `sqlite3_stmt` analogue).
    ///
    /// For `SELECT … FROM t WHERE pk = $1` this captures the PkPoint micro-exec
    /// metadata once. Prefer [`execute_prepared_select`](Self::execute_prepared_select)
    /// over [`execute`](Self::execute) on the hot path.
    pub fn prepare_select(&self, sql: &str) -> Result<PreparedSelect> {
        let hash = self.prepare(sql)?;
        self.gate_cache_on_schema()?;
        let plan = self.cached_or_load_tls(&hash)?;
        let schema_gen = self.engine.schema().schema_gen;
        let pk_hot = match &plan.stmt {
            mpedb_sql::PlanStmt::Select(sp) => {
                exec::try_build_pk_point_hot(&self.schema(), &plan, sp)?
            }
            _ => {
                return Err(Error::Unsupported(
                    "prepare_select requires a SELECT statement".into(),
                ));
            }
        };
        Ok(PreparedSelect {
            hash,
            plan,
            pk_hot,
            schema_gen,
        })
    }

    /// Execute a [`PreparedSelect`] — skips plan-registry lookup; uses the
    /// precomputed PkPoint path when available.
    pub fn execute_prepared_select(
        &self,
        stmt: &PreparedSelect,
        params: &[Value],
    ) -> Result<ExecResult> {
        self.execute_prepared_select_ctx(&Session::empty(), stmt, params)
    }

    /// [`execute_prepared_select`](Self::execute_prepared_select) with session context.
    pub fn execute_prepared_select_ctx(
        &self,
        session: &Session,
        stmt: &PreparedSelect,
        params: &[Value],
    ) -> Result<ExecResult> {
        self.gate_cache_on_schema()?;
        let gen = self.engine.schema().schema_gen;
        if gen != stmt.schema_gen {
            return Err(Error::PlanInvalidated);
        }
        let full = session::resolve_params_timed(&stmt.plan, params, session)?;
        if let Some(ref hot) = stmt.pk_hot {
            // `pk_hot` is Some only when `prepare_select` saw a top-level
            // Select, so this destructure cannot fail; it is how the hot path
            // gets the very SelectPlan the shape was validated against.
            let mpedb_sql::PlanStmt::Select(sp) = &stmt.plan.stmt else {
                return Err(Error::Internal("PreparedSelect is not a Select".into()));
            };
            // Fast path: no host UDFs on pure column projection; skip host table build.
            if !stmt.plan.footprint.read_only {
                return Err(Error::Internal(
                    "PreparedSelect with write footprint".into(),
                ));
            }
            let r = self.engine.begin_read()?;
            self.validate_policy_read(Some(&stmt.hash), &stmt.plan, &r)?;
            let res = {
                let mut ctx = exec::ReadCtx(&r, None, None, None, ChargeMode::PerRow);
                // coerce still needed for Text→Int etc.
                let coerced = exec::coerce_params(&stmt.plan, &full)?;
                exec::exec_pk_point_hot(&mut ctx, &stmt.plan, &coerced, sp, hot)
            };
            match res {
                Ok(out) => {
                    r.finish()?;
                    Ok(out)
                }
                Err(e) => Err(e),
            }
        } else {
            self.run_plan(Some(&stmt.hash), &stmt.plan, &full)
        }
    }

    /// One-shot prepare + execute. `EXPLAIN <stmt>` returns
    /// [`ExecResult::Explain`] without executing (and without publishing the
    /// plan); any other statement is published to the registry exactly like
    /// [`Database::prepare`].
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        self.query_ctx(&Session::empty(), sql, params)
    }

    /// Like [`query`](Self::query) but with a [`Session`] for `current_setting()`
    /// (design/DESIGN-MULTIDB.md §2). `params` are the caller-facing parameters only.
    pub fn query_ctx(
        &self,
        session: &Session,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        // #166: a text seen before, on an unchanged catalog and unchanged table
        // magnitudes, already knows its plan. Checked FIRST so a hit also skips
        // the two routing hooks and the `parse_ddl` probe below, all of which
        // re-inspect the text on every call. Only statements that reached
        // compilation are ever memoized, so a hit is by construction a text
        // those hooks passed through.
        if let Some(res) = self.memoized(session, sql, params)? {
            return Ok(res);
        }
        // sqlite-compat multi-database statements (#51): ATTACH/DETACH mutate
        // the connection-local attach list; database-qualified names resolve
        // before the parser (a `main.`-only statement continues below with
        // the qualifier stripped; a cross-file SELECT runs on its own path).
        if let Some(res) = self.attach_stmt_hook(sql)? {
            return Ok(res);
        }
        if let Some(res) = self.temp_view_stmt_hook(sql)? {
            return Ok(res);
        }
        // Only the `Passthrough` arm may be memoized. The others resolve
        // against the connection's ATTACH list, so their routing is not a
        // function of the text alone: the same SQL routes elsewhere after an
        // ATTACH/DETACH, and a memo keyed on text would answer with the old
        // route. Passthrough is the arm that stays put, and it is the bare-name
        // shadowing rule that makes it so — `dbref.rs` resolves an unqualified
        // name to `main` whenever main has it, before it looks at any attached
        // member. A text only reaches the memo by compiling, which means main
        // did have the table, so no later ATTACH can pull it away; a DROP that
        // could is a DDL, and moves `schema_gen`.
        let memoizable;
        let routed;
        let sql = match self.resolve_db_refs_hook(sql)? {
            multifile::DbRoute::Passthrough => {
                memoizable = true;
                sql
            }
            multifile::DbRoute::Main(s) => {
                memoizable = false;
                routed = s;
                &routed
            }
            multifile::DbRoute::Cross { sql, tables, .. } => {
                return self.query_cross(session, &sql, &tables, params)
            }
            multifile::DbRoute::AttachedOnly { db, sql } => {
                return self.query_attached_only(&db, &sql, params);
            }
        };
        // RLS DDL (CREATE/DROP POLICY, ALTER TABLE … ROW LEVEL SECURITY) mutates
        // the catalog rather than compiling to a plan — apply it directly.
        if let Some(ddl) = self.parse_ddl(sql)? {
            return self.apply_ddl(ddl);
        }
        let (plan, is_explain) = self.compile_maybe_explain(sql)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(&self.schema())));
        }
        self.warn_if_risky(&plan);
        let hash = plan.hash();
        let plan = self.register(hash, plan, sql)?;
        if memoizable {
            let gen = self.cache_gen.load(std::sync::atomic::Ordering::Relaxed);
            self.remember(gen, sql, &plan, hash)?;
        }
        let full = session::resolve_params_timed(&plan, params, session)?;
        self.run_plan(Some(&hash), &plan, &full)
    }

    /// The per-table facts `plan`'s compile depended on, read off one snapshot:
    /// `(table_id, cost magnitude, policy epoch)`.
    ///
    /// The magnitude mirrors `compile_maybe_explain`'s cost wiring exactly —
    /// same [`mpedb_sql::magnitude`], same `unwrap_or(0)` for a table id the
    /// catalog has no entry for (CTE and derived-table sentinels reach the
    /// footprint but never the catalog). If the two ever disagree the memo would
    /// compare buckets the planner never saw, and "unchanged" would stop meaning
    /// "would compile the same".
    pub(super) fn plan_tables(&self, plan: &CompiledPlan) -> Result<Vec<(u32, u32, u64)>> {
        let mut ids: Vec<u32> = plan
            .footprint
            .tables_read
            .iter()
            .chain(plan.footprint.tables_written.iter())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        // Table-free statements (transaction control compiled on the
        // autocommit road, bare VALUES, …) have no facts to probe — skip the
        // read snapshot; an empty vector is what the loop would produce.
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.engine.begin_read()?;
        let out = ids
            .into_iter()
            .map(|t| {
                Ok((
                    t,
                    mpedb_sql::magnitude(r.row_count(t).unwrap_or(0)),
                    policy_store::policy_epoch(&r, t)?,
                ))
            })
            .collect::<Result<Vec<_>>>();
        r.finish()?;
        out
    }

    /// Record what `sql` compiled to, so the next call can skip deriving it.
    /// `gen` is the schema generation the plan was actually COMPILED against,
    /// passed in rather than read here because the two callers learn it
    /// differently: autocommit from `cache_gen` (which `gate_cache_on_schema`
    /// just refreshed), an in-transaction statement from its txn's own schema
    /// bundle (#168). Reading `cache_gen` here would stamp a session's entry
    /// with whatever generation this handle last gated on — possibly older than
    /// the catalog the plan was built from, which is the one direction that
    /// cannot be allowed to be wrong.
    pub(super) fn remember(&self, gen: u64, sql: &str, plan: &CompiledPlan, hash: PlanHash) -> Result<()> {
        // A host-UDF plan is deliberately connection-local and never enters the
        // shared registry; `cached_or_load_tls` would go looking for it there on
        // a fresh handle. Leave those to the compile path, which is where their
        // closures are resolved.
        if plan.contains_host_call() {
            return Ok(());
        }
        let entry = TextMemo {
            gen,
            tables: self.plan_tables(plan)?,
            hash,
            used: std::sync::atomic::AtomicBool::new(true),
        };
        self.text_memo.write().expect(POISON).insert(sql, entry);
        Ok(())
    }

    /// The [`TextMemo`] fast path: `Ok(None)` means "no usable entry, take the
    /// compile road", which is always a legal answer — the memo is an
    /// optimisation and never the only way to reach a plan.
    pub(super) fn memoized(
        &self,
        session: &Session,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<ExecResult>> {
        // Must run even on the fast path: this is where another process's DDL
        // is picked up, and skipping it is exactly how a plan compiled against
        // a dropped table would get executed (#47 stage 4).
        self.gate_cache_on_schema()?;
        let gen = self.cache_gen.load(std::sync::atomic::Ordering::Relaxed);
        let (hash, tables) = {
            let memo = self.text_memo.read().expect(POISON);
            match memo.get(sql) {
                Some(e) if e.gen == gen => (e.hash, e.tables.clone()),
                _ => return Ok(None),
            }
        };
        // Any failure to produce the memoized plan is a MISS, never an error.
        // The memo must not be able to introduce a failure mode the compile
        // path does not have: falling through re-derives everything, so the
        // worst case is doing the work twice. Two ways this legitimately
        // fails today — `register` publishes to the shared registry
        // OPPORTUNISTICALLY (a held writer lock skips the insert, #109), and
        // `validate_policy_read` can `evict` a single plan from the local
        // cache — and the pair leaves a hash nothing can load.
        let Ok(plan) = self.cached_or_load_tls(&hash) else {
            return Ok(None);
        };
        // The two inputs `schema_gen` does not cover. Cheap: only the tables
        // this plan touches, and a row count has to DOUBLE before its bucket
        // moves. A mismatch is a plain miss — fall through and recompile.
        if self.plan_tables(&plan)? != tables {
            return Ok(None);
        }
        // The memo's whole claim, checked rather than argued: a hit must be the
        // plan a recompile would have produced. Deliberately NOT a bespoke test
        // matrix — this turns every existing test, every sqllogictest corpus
        // record and every Django/CPython statement into a memo-invalidation
        // test, which is the only way to cover inputs nobody thought to
        // enumerate. Two such inputs turned up the first time it ran, and
        // neither was on my list: the RLS policy epoch (`schema_gen` looks like
        // it covers every catalog change, and does not) and ANALYZE.
        if verify_plan_memo() {
            let (fresh, _) = self.compile_maybe_explain(sql).map_err(|e| {
                Error::Internal(format!(
                    "#166: memo served a plan for {sql:?} that no longer compiles: {e}"
                ))
            })?;
            assert_eq!(
                fresh.hash(),
                hash,
                "#166: memo served a stale plan for {sql:?} — some compile input \
                 moved without moving `schema_gen` or this plan's table facts"
            );
        }
        // Kept on this path deliberately. `plan_can_multiply` is a pure
        // predicate over the plan — no I/O — so the ordinary statement pays
        // nothing, and a risky one keeps warning as often as it did before.
        self.warn_if_risky(&plan);
        let full = session::resolve_params_timed(&plan, params, session)?;
        self.run_plan(Some(&hash), &plan, &full).map(Some)
    }

    /// One-shot compile + execute **without** publishing to the plan registry
    /// and **without** the detached encode/decode re-validation round-trip.
    ///
    /// Use this for harnesses that fire each SQL text approximately once
    /// (sqllogictest corpus, ad-hoc tooling). Compared to
    /// [`prepare_detached`](Self::prepare_detached) +
    /// [`execute_detached`](Self::execute_detached) it skips `encode` →
    /// `decode` → schema re-check on every statement (those exist for
    /// untrusted client-borne blobs, not same-process one-shots). Compared to
    /// [`query`](Self::query) it never takes the writer lock to insert a plan
    /// into the shared registry.
    ///
    /// DML still autocommits via the writer lock as usual; plans run with no
    /// registry hash so they are not enqueued on the intent ring (same as
    /// detached DML). Correctness is identical to a successful
    /// prepare_detached+execute_detached for well-formed SQL.
    pub fn query_once(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        self.query_once_ctx(&Session::empty(), sql, params)
    }

    /// [`query_once`](Self::query_once) with a [`Session`] for `current_setting()`.
    pub fn query_once_ctx(
        &self,
        session: &Session,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        if let Some(res) = self.attach_stmt_hook(sql)? {
            return Ok(res);
        }
        if let Some(res) = self.temp_view_stmt_hook(sql)? {
            return Ok(res);
        }
        let routed;
        let sql = match self.resolve_db_refs_hook(sql)? {
            multifile::DbRoute::Passthrough => sql,
            multifile::DbRoute::Main(s) => {
                routed = s;
                &routed
            }
            multifile::DbRoute::Cross { sql, tables, .. } => {
                return self.query_cross(session, &sql, &tables, params)
            }
            multifile::DbRoute::AttachedOnly { db, sql } => {
                return self.query_attached_only(&db, &sql, params);
            }
        };
        if let Some(ddl) = self.parse_ddl(sql)? {
            return self.apply_ddl(ddl);
        }
        let (plan, is_explain) = self.compile_maybe_explain(sql)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(&self.schema())));
        }
        self.warn_if_risky(&plan);
        let full = session::resolve_params_timed(&plan, params, session)?;
        // `None` hash: no intent-ring enqueue (same as execute_detached).
        self.run_plan(None, &plan, &full)
    }

    // ---------------- detached (client-borne) plans ----------------

    /// Compile `sql` to a content-hashed plan **without publishing it to the
    /// shared registry**: the caller keeps the returned [`DetachedPlan`] and
    /// ships `(blob + hash + sql)` itself (Morten's detached-plan model). The
    /// returned `hash` is byte-identical to what [`Database::prepare`] would
    /// produce for the same SQL and schema — the two paths differ only in
    /// whether the plan is stored in the database.
    ///
    /// Unlike [`Database::prepare`], this never opens a write transaction and
    /// never takes the writer lock, so it is safe to call under read-mostly
    /// load and (unlike `prepare`) does not need the "no open WriteSession"
    /// caveat.
    pub fn prepare_detached(&self, sql: &str) -> Result<DetachedPlan> {
        // #51: a detached plan is client-borne bytes — a cross-file plan's
        // merged table ids are meaningless outside this handle's attach list,
        // so the combination refuses by name. `main.` qualifiers still strip.
        let routed;
        let sql = match self.resolve_db_refs_hook(sql)? {
            multifile::DbRoute::Passthrough => sql,
            multifile::DbRoute::Main(s) => {
                routed = s;
                &routed
            }
            multifile::DbRoute::Cross { .. } | multifile::DbRoute::AttachedOnly { .. } => {
                return Err(Error::Unsupported(
                    "cross-file statements cannot be prepared as detached \
                     plans; use prepare()/execute() on this handle"
                        .into(),
                ))
            }
        };
        let plan = self.compile_maybe_explain(sql)?.0;
        Ok(DetachedPlan {
            hash: plan.hash(),
            blob: plan.encode(),
            sql: sql.to_owned(),
        })
    }

    /// Execute a client-borne [`DetachedPlan`] **without ever touching the
    /// shared registry**. The blob is fully re-validated on the way in, in the
    /// same order the registry load path uses:
    ///
    /// 1. [`CompiledPlan::decode`] bounds-checks every field, range-checks all
    ///    indices against the live schema, and recomputes the footprint from
    ///    scratch (a forged footprint is rejected).
    /// 2. Its embedded `schema_hash` must equal the live schema — on mismatch
    ///    decode yields [`Error::PlanInvalidated`], which propagates so the
    ///    caller re-prepares from [`DetachedPlan::sql`]. (A byte flipped inside
    ///    the blob's `schema_hash` region is indistinguishable from a genuine
    ///    schema change and surfaces the same way; re-preparing from `sql`
    ///    heals both.)
    /// 3. **Integrity**: the decoded plan must hash to the carried `hash`.
    ///    `decode` does *not* check this (it validates against the schema, not
    ///    against a caller-supplied hash), so it is verified separately here. A
    ///    blob that decodes cleanly but does not match its advertised hash —
    ///    tampering that survived structural + schema validation, or a
    ///    mismatched (blob, hash) pair — is [`Error::Corrupt`]. It is
    ///    deliberately **not** [`Error::PlanInvalidated`] (that means "schema
    ///    changed, re-prepare") and **not** [`Error::UnknownPlan`] (that is the
    ///    registry's "missing", meaningless here — nothing is stored).
    ///
    /// Execution then reuses the exact same routing as [`Database::execute`]:
    /// `read_only` plans run on a lock-free read snapshot, DML autocommits
    /// through the writer lock. It is passed with no hash, so a detached DML
    /// statement leads its own commit directly rather than being enqueued on
    /// the intent ring (the ring leader loads intents *by hash from the
    /// registry*, and a detached plan is by definition not there).
    pub fn execute_detached(&self, plan: &DetachedPlan, params: &[Value]) -> Result<ExecResult> {
        // (1)+(2): structural + schema re-validation. PlanInvalidated (schema
        // drift) propagates verbatim; any other decode failure is Corrupt.
        let compiled = match CompiledPlan::decode(&plan.blob, &self.schema()) {
            Ok(p) => p,
            Err(Error::PlanInvalidated) => return Err(Error::PlanInvalidated),
            Err(e) => return Err(e),
        };
        // (3): integrity — the blob must match the hash the client carried.
        if compiled.hash() != plan.hash {
            return Err(Error::Corrupt(
                "detached plan blob does not match its carried hash".into(),
            ));
        }
        // Reuse the existing executor entry point (lib.rs-local; exec.rs is
        // untouched). `None` hash keeps DML off the intent ring. A detached
        // plan referencing `current_setting()` fails closed here (empty
        // session ⇒ missing-key error); a context-aware detached path is a
        // later addition.
        let full = session::resolve_params(&compiled, params, &Session::empty())?;
        self.run_plan(None, &compiled, &full)
    }

    /// `PRAGMA foreign_keys` for THIS connection (#194): are foreign keys
    /// enforced on the write path?
    ///
    /// Default OFF, which is sqlite's own default — a `REFERENCES` clause is
    /// parsed, stored and reported by both engines whether or not anything
    /// checks it. `[compat] foreign_keys = true` sets the initial value from
    /// config.
    pub fn fk_enforced(&self) -> bool {
        self.fk_enforce.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set `PRAGMA foreign_keys` for this connection.
    ///
    /// The caller is responsible for sqlite's "no-op inside a transaction"
    /// rule (measured: sqlite silently keeps the old value rather than
    /// erroring) — the pragma handler enforces it, because only it knows
    /// whether a transaction is open.
    pub fn set_fk_enforced(&self, on: bool) {
        self.fk_enforce.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Every foreign-key violation standing in the database right now —
    /// `PRAGMA foreign_key_check` (#194). One row per dangling child:
    /// `(child table, child primary key, parent table name, key index)`, the
    /// same four columns sqlite reports.
    ///
    /// Read-only and independent of [`Database::fk_enforced`]: the whole point
    /// is to audit a database that was filled with enforcement OFF, which is
    /// exactly what Django's migration runner does.
    ///
    /// This audits the last COMMITTED state. A caller holding an open write
    /// session must use [`WriteSession::foreign_key_check`] instead — see the
    /// note there for why the difference is a wrong answer and not a nuance.
    pub fn foreign_key_check(
        &self,
        table: Option<&str>,
    ) -> Result<Vec<FkCheckRow>> {
        let bundle = self.schema();
        let Some(only) = fk::check_scope(&bundle.schema, table) else {
            return Ok(Vec::new());
        };
        let txn = self.engine.begin_read()?;
        let mut ctx = exec::ReadCtx(&txn, None, None, None, exec::ChargeMode::PerRow);
        let out = fk::check_all(&mut ctx, &bundle.schema, only);
        txn.finish()?;
        out
    }

    /// Bound every writer-lock wait on this handle (#109). `None` (the
    /// default) blocks indefinitely — mpedb's historical native behavior.
    /// `Some(t)` makes [`Database::begin`], autocommit DML/DDL, and the other
    /// facade write entries poll the lock for at most `t` and then fail with
    /// [`Error::Busy`] — nothing executed, nothing enqueued; the caller may
    /// retry. `Some(Duration::ZERO)` = one immediate attempt (sqlite's
    /// timeout-0 semantics: immediate BUSY on contention).
    ///
    /// Two side effects of a bounded policy, both deliberate:
    /// - Contended autocommit DML takes the direct lock path instead of the
    ///   §5.3 intent ring: a published intent cannot be withdrawn at the
    ///   deadline without racing the leader's stage/post, so a
    ///   deadline-carrying write never publishes one (see `run_write_plan`).
    /// - Shared-registry plan publication becomes opportunistic: a
    ///   first-compile that finds the writer lock held keeps the plan in the
    ///   local cache and skips the registry insert, so a SELECT never waits
    ///   on (or fails behind) another process's write transaction.
    pub fn set_busy_timeout(&self, timeout: Option<std::time::Duration>) {
        let ms = match timeout {
            None => -1,
            // Round sub-millisecond budgets UP: as_millis() truncates, and
            // Some(500µs) silently becoming timeout-0 (immediate BUSY) is a
            // trap. Zero stays zero — that spelling is the explicit
            // one-attempt request.
            Some(t) if t.is_zero() => 0,
            Some(t) => t.as_millis().max(1).min(i64::MAX as u128) as i64,
        };
        self.busy_timeout_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// The busy policy as a per-call deadline: `None` = block indefinitely.
    /// Computed at each write entry so the whole statement (lock wait
    /// included) shares one budget.
    pub(crate) fn busy_deadline(&self) -> Option<mpedb_core::Instant> {
        let ms = self.busy_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        u64::try_from(ms)
            .ok()
            .map(|ms| mpedb_core::Instant::now() + std::time::Duration::from_millis(ms))
    }

    /// True when a bounded busy policy is in force (`set_busy_timeout(Some)`).
    pub(super) fn busy_bounded(&self) -> bool {
        self.busy_timeout_ms.load(std::sync::atomic::Ordering::Relaxed) >= 0
    }

    /// Start an interactive multi-statement write transaction. Holds the
    /// single writer lock until commit/rollback/drop (drop = rollback).
    /// A second `begin()` from the same thread errors instead of hanging
    /// (ERRORCHECK mutex). Under a [`Database::set_busy_timeout`] policy the
    /// lock wait is bounded and fails with [`Error::Busy`] at the deadline.
    pub fn begin(&self) -> Result<WriteSession<'_>> {
        self.begin_as(&Session::empty())
    }

    /// Begin a write transaction bound to a snapshot of `session` (SET LOCAL
    /// semantics, design/DESIGN-MULTIDB.md §2.5): every statement in the transaction
    /// resolves `current_setting()` against the context as it was *here*, so a
    /// later mutation of the caller's `Session` cannot bleed into an open
    /// transaction. The context is fixed for the transaction's lifetime.
    pub fn begin_as(&self, session: &Session) -> Result<WriteSession<'_>> {
        Ok(WriteSession {
            db: self,
            txn: self.engine.begin_write_deadline(self.busy_deadline())?,
            session: session.clone(),
            poisoned: false,
            savepoints: Vec::new(),
            ddl_applied: false,
            validated: Default::default(),
        })
    }

    /// Verify the engine's page-accounting invariant (design/DESIGN.md §4.5).
    /// Takes the writer lock briefly; do not call with an open session.
    pub fn verify(&self) -> Result<()> {
        self.engine.verify_page_accounting()
    }

    /// Diagnostic counters for the high-water leak
    /// (`crates/mpedb-core/tests/high_water_leak.rs`):
    /// `(txn_id, high_water, oldest_pinned_bound, freelist_entries)`.
    ///
    /// For `examples/leak_probe.rs`. Costs a freelist walk, takes no writer
    /// lock, and pins nothing — perturbing the reader table is exactly what
    /// would corrupt the thing being measured.
    pub fn leak_counters(&self) -> Result<(u64, u64, u64, u64)> {
        self.engine.leak_counters()
    }

    /// Diagnostic (#37): see `Engine::freelist_shape`.
    pub fn freelist_shape(&self) -> Result<mpedb_core::engine::FreelistShape> {
        self.engine.freelist_shape()
    }

    // ---------------- system records: tooling/extension keyspace ----------

    /// Store a raw record in the reserved system keyspace, namespaced by
    /// `ns` (full key = `ns` bytes ++ `0x00` ++ `key`; `ns` must be
    /// non-empty, at most 64 bytes and NUL-free, which makes distinct
    /// namespaces prefix-disjoint from each other and from the plan
    /// registry's `plan/` prefix).
    ///
    /// This is a **tooling/extension API** (used by `mpedb-proc` for stored
    /// procedures, namespaces `proc`/`proch`): records are opaque bytes,
    /// shared across all attached processes, and live in the same catalog
    /// sys-keyspace as the plan registry. Like [`Database::prepare`], the
    /// write happens in its own short write transaction — do NOT call while
    /// a [`WriteSession`] from this handle is open on the same thread.
    pub fn sys_record_put(&self, ns: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let subkey = sys_record_subkey(ns, key)?;
        if value.len() > SYS_RECORD_MAX_VALUE {
            return Err(Error::Unsupported(format!(
                "system record value too large ({} > {SYS_RECORD_MAX_VALUE} bytes)",
                value.len()
            )));
        }
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        match w.sys_put(&subkey, value) {
            Ok(()) => w.commit(),
            Err(e) => {
                w.abort();
                Err(e)
            }
        }
    }

    /// Stream one `text`/`blob` column of one row into `out` in bounded
    /// chunks (≤ 256 KiB each) WITHOUT materializing the value — the chunked
    /// read API of DESIGN-BLOBEXTENT §5. `range` is `(offset, len)` within
    /// the value, `None` for all of it. Returns the byte count written, or
    /// `None` when the row is absent or the column is NULL. A snapshot
    /// eviction between chunks surfaces as an error — never as mixed bytes;
    /// the caller decides whether to retry against a fresh snapshot.
    pub fn blob_to_writer(
        &self,
        table: &str,
        pk_values: &[Value],
        col: &str,
        range: Option<(u64, u64)>,
        out: &mut dyn std::io::Write,
    ) -> Result<Option<u64>> {
        let table_id = self
            .schema()
            .table_id(table)
            .ok_or_else(|| Error::Unsupported(format!("unknown table `{table}`")))?;
        let bundle = self.schema();
        let t = bundle.table(table_id).expect("id from table_id");
        let col_idx = t
            .column_index(col)
            .ok_or_else(|| Error::Unsupported(format!("unknown column `{col}` in `{table}`")))?
            as usize;
        let r = self.engine.begin_read()?;
        let result = (|| -> Result<Option<u64>> {
            let Some(mut br) = r.blob_read(table_id, pk_values, col_idx, range)? else {
                return Ok(None);
            };
            let mut written = 0u64;
            while let Some(chunk) = br.next()? {
                out.write_all(&chunk).map_err(Error::from)?;
                written += chunk.len() as u64;
            }
            Ok(Some(written))
        })();
        r.finish()?;
        result
    }

    /// Read a system record (see [`Database::sys_record_put`]). Runs in a
    /// read transaction; never touches the writer lock.
    pub fn sys_record_get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let subkey = sys_record_subkey(ns, key)?;
        let r = self.engine.begin_read()?;
        let rec = r.sys_get(&subkey)?;
        r.finish()?;
        Ok(rec)
    }

    /// All records in namespace `ns`, as `(key, value)` pairs in key order,
    /// with the namespace prefix stripped. Read transaction only.
    ///
    /// Prefix-bounded (#124): keys are `<ns>\0<key>`, so `[<ns>\0, <ns>\x01)`
    /// is exactly this namespace — no neighbouring family (least of all the
    /// plan registry, which shares this keyspace) is ever materialised.
    /// Every key in the system keyspace, WITH its namespace prefix intact.
    ///
    /// Unlike [`Database::sys_record_scan`], which strips the namespace and
    /// takes one at a time, this is deliberately whole-keyspace and
    /// prefix-preserving: its callers are the ones that must reason about which
    /// stores exist rather than about one they already know. Table-id
    /// compaction (`mpedb_core::compact`) is the reason it exists — it has to
    /// classify EVERY key, including keys belonging to a store it has never
    /// heard of, because an unclassified store is how compaction goes silently
    /// wrong.
    /// What a table-id compaction would do to THIS database, writing nothing.
    ///
    /// Answers the question an operator actually has before taking a database
    /// down: how much of the id budget is tombstones, and what would be
    /// rewritten. Safe on a live database — it reads one snapshot.
    /// Reclaim the table-id budget: dense ids, tombstones gone.
    ///
    /// Requires the database EXCLUSIVE — no other reader, no other writer. That
    /// is not caution, it is the correctness argument: between renumbering the
    /// schema and renumbering the records keyed by the old ids there is an
    /// instant where the two disagree, and the only reason that window is safe
    /// is that nobody can look into it. DESIGN-DROP-TABLE §0 rejected ONLINE id
    /// reuse for exactly the window this closes by being offline.
    ///
    /// Published plans are DROPPED rather than renumbered — each carries table
    /// ids inside its own bytes and inside its footprint, so a renumbered plan
    /// would name a different table and still validate. Callers re-prepare.
    ///
    /// One transaction: a crash leaves the old ids, untouched.
    pub fn compact_ids(&self) -> Result<mpedb_core::engine::Rewritten> {
        self.refresh_schema_if_stale()?;
        // The reader table counts THIS process's reader too if one is open;
        // there is none here, so any live reader is somebody else.
        let others = self.engine.live_readers();
        if others > 0 {
            return Err(Error::Unsupported(format!(
                "compact-ids needs the database to itself: {others} reader(s) are attached.                  Renumbering is safe only while nobody can observe the instant between the                  schema moving and the records keyed by the old ids moving"
            )));
        }
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let done = match w.compact_table_ids() {
            Ok(d) => d,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        };
        w.commit()?;
        // Every cached plan was compiled against the old numbering.
        self.cache.write().expect(POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(done)
    }

    /// This database's lifetime `CREATE TABLE` budget — see
    /// [`mpedb_types::config::DbOptions::max_tables`].
    pub fn max_tables(&self) -> usize {
        self.engine.max_tables()
    }

    pub fn compact_plan(&self) -> Result<mpedb_core::compact::CompactPlan> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let dead: Vec<bool> = bundle.schema.tables.iter().map(|t| t.dead).collect();
        let keys = self.sys_keys()?;
        Ok(mpedb_core::compact::plan(&dead, &keys))
    }

    pub fn sys_keys(&self) -> Result<Vec<Vec<u8>>> {
        let r = self.engine.begin_read()?;
        let all = r.sys_scan();
        r.finish()?;
        Ok(all?.into_iter().map(|(k, _)| k).collect())
    }

    pub fn sys_record_scan(&self, ns: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        check_sys_ns(ns)?;
        let mut prefix = Vec::with_capacity(ns.len() + 1);
        prefix.extend_from_slice(ns.as_bytes());
        prefix.push(0);
        let mut end = prefix.clone();
        *end.last_mut().expect("non-empty") = 1;
        let r = self.engine.begin_read()?;
        let all = r.sys_scan_range(&prefix, &end);
        r.finish()?;
        Ok(all?
            .into_iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k[prefix.len()..].to_vec(), v))
            .collect())
    }

    /// The KEYS of namespace `ns`, prefix-stripped, without resolving the
    /// values — the drop path for a family whose values are extents (columnar
    /// segment blocks), where resolving would materialize every block.
    pub(crate) fn sys_record_scan_keys(&self, ns: &str) -> Result<Vec<Vec<u8>>> {
        check_sys_ns(ns)?;
        let mut prefix = Vec::with_capacity(ns.len() + 1);
        prefix.extend_from_slice(ns.as_bytes());
        prefix.push(0);
        let mut end = prefix.clone();
        *end.last_mut().expect("non-empty") = 1;
        let r = self.engine.begin_read()?;
        let all = r.sys_scan_range_keys(&prefix, &end);
        r.finish()?;
        Ok(all?
            .into_iter()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| k[prefix.len()..].to_vec())
            .collect())
    }

    // ---------------- internal: plan registry protocol ----------------

    /// Local-cache/registry publication for a freshly compiled plan.
    pub(super) fn register(&self, hash: PlanHash, plan: CompiledPlan, sql: &str) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.cache.read().expect(POISON).get(&hash) {
            return Ok(p.clone());
        }
        let plan = Arc::new(plan);
        // A plan that calls a HOST-registered UDF is valid ONLY for this
        // connection (its closures live here), so it MUST NOT enter the shared
        // content-hashed `plan/<hash>` registry (design/DESIGN-UDF.md §2). Keep it
        // in the local cache only — this connection can still `execute(hash)` it,
        // and it is recompiled per connection rather than shared. The local cache
        // is dropped whenever the UDF set changes (`register_host_function`).
        if plan.contains_host_call() {
            self.cache.write().expect(POISON).insert(hash, plan.clone());
            return Ok(plan);
        }
        let blob = plan.encode();
        let subkey = plan_subkey(&hash, self.dialect);

        // READ-FIRST probe: is an identical record already published?
        let published = {
            let r = self.engine.begin_read()?;
            let rec = r.sys_get(&subkey)?;
            r.finish()?;
            rec.as_deref()
                .and_then(registry::parse_record)
                .is_some_and(|r| r.blob == blob)
        };
        if !published {
            // Genuine miss (or corrupt/stale entry): short write txn inserts
            // or overwrites. Re-checked under the lock so a racing process
            // that just published the same plan costs us no write.
            //
            // Under a busy policy (#109 / compat gap E8), publication is
            // OPPORTUNISTIC: one non-blocking attempt, and a held lock skips
            // the insert rather than waiting or failing — the plan still runs
            // from the local cache (the host-UDF plan path has always worked
            // this way), so a first-compile SELECT never answers BUSY behind
            // another process's write transaction. sqlite readers proceed
            // under a writer; so must ours. The registry entry appears the
            // next time this text is compiled cold while the lock is free;
            // an enqueued intent whose plan is not yet published falls back
            // via the leader's `UnknownPlan` answer (handled below).
            //
            // The immediate-deadline probe (rather than raw `try_begin_write`)
            // matters: it also answers `Busy` when the lock is held by THIS
            // thread's sibling connection (the ERRORCHECK reentry case — the
            // engine folds that in), which is exactly CPython's
            // `TransactionTests`: con1 holds an implicit txn, con2's
            // first-compile SELECT must read, not error.
            let w = if self.busy_bounded() {
                match self.engine.begin_write_deadline(Some(mpedb_core::Instant::now())) {
                    Ok(w) => Some(w),
                    Err(Error::Busy) => None, // opportunistic: skip publication
                    Err(e) => return Err(e),
                }
            } else {
                Some(self.engine.begin_write()?)
            };
            if let Some(mut w) = w {
                match registry::insert_plan(&mut w, &hash, sql, &blob, self.dialect) {
                    Ok(true) => w.commit()?,
                    Ok(false) => w.abort(),
                    Err(e) => {
                        w.abort();
                        return Err(e);
                    }
                }
            }
        }
        self.cache
            .write()
            .expect(POISON)
            .insert(hash, plan.clone());
        Ok(plan)
    }

    /// Local cache, else registry load: parse the record, fully re-validate
    /// the blob, verify the content hash, and cache locally.
    ///
    /// Deliberately does NOT refresh the entry's `last_used_txn`: bumping it
    /// needs the single writer lock, and this path serves read-only executes
    /// — a cold-cache SELECT must never block behind (or msync alongside) a
    /// live writer (design/DESIGN.md §7.3: read-only plans route to a read
    /// transaction, the writer lock is never touched). Registry recency is
    /// therefore insert time plus the ride-along bump in
    /// [`WriteSession::plan_by_hash`]; eviction tolerates entries whose
    /// `last_used_txn` was never bumped (see `registry::evict_if_full`).
    pub(super) fn cached_or_load(&self, hash: &PlanHash) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.cache.read().expect(POISON).get(hash) {
            return Ok(p.clone());
        }
        let subkey = plan_subkey(hash, self.dialect);
        let record = {
            let r = self.engine.begin_read()?;
            let rec = r.sys_get(&subkey)?;
            r.finish()?;
            rec.ok_or(Error::UnknownPlan(*hash))?
        };
        let plan = Arc::new(decode_registry_plan(&record, hash, &self.schema(), self.dialect)?);
        self.cache
            .write()
            .expect(POISON)
            .insert(*hash, plan.clone());
        Ok(plan)
    }

    /// [`cached_or_load`](Self::cached_or_load) plus a thread-local one-entry
    /// cache for the common "same plan, many times" pattern. Invalidated when
    /// [`cache_gen`](Self::cache) moves (DDL).
    pub(super) fn cached_or_load_tls(&self, hash: &PlanHash) -> Result<Arc<CompiledPlan>> {
        let gen = self.cache_gen.load(std::sync::atomic::Ordering::Relaxed);
        let hit = TLS_PLAN.with(|c| {
            let g = c.borrow();
            match g.as_ref() {
                Some(h) if h.conn_id == self.conn_id && h.hash == *hash && h.cache_gen == gen => {
                    Some(h.plan.clone())
                }
                _ => None,
            }
        });
        if let Some(p) = hit {
            return Ok(p);
        }
        let plan = self.cached_or_load(hash)?;
        TLS_PLAN.with(|c| {
            *c.borrow_mut() = Some(TlsPlanHit {
                conn_id: self.conn_id,
                hash: *hash,
                plan: plan.clone(),
                cache_gen: gen,
            });
        });
        Ok(plan)
    }

    // ---------------- internal: execution routing ----------------

    /// Route by the (decode-recomputed) footprint: read transaction for
    /// read-only plans, autocommit write transaction for DML.
    pub(super) fn run_plan(
        &self,
        hash: Option<&PlanHash>,
        plan: &CompiledPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        if matches!(
            plan.stmt,
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback
        ) {
            return Err(Error::Unsupported(
                "BEGIN/COMMIT/ROLLBACK do nothing through execute(); \
                 use Database::begin() and WriteSession"
                    .into(),
            ));
        }
        if matches!(
            plan.stmt,
            PlanStmt::Savepoint(_) | PlanStmt::Release(_) | PlanStmt::RollbackTo(_)
        ) {
            return Err(Error::Unsupported(
                "SAVEPOINT/RELEASE/ROLLBACK TO require an open transaction; \
                 use Database::begin() and run them through the WriteSession \
                 (there is no autocommit savepoint)"
                    .into(),
            ));
        }
        if plan.footprint.read_only {
            // Reads never touch the writer lock or the ring.
            let mut partial = false;
            // Host UDF closures for the executor, only for a plan that calls one
            // (design/DESIGN-UDF.md). Snapshot once; kept alive for the whole
            // scan. Scalars and aggregates ride ONE gate — a plan naming a host
            // aggregate reports `contains_host_call` too.
            let tables = self.host_tables(plan);
            let host: Option<&dyn mpedb_types::HostFns> =
                tables.as_ref().map(|(f, _, _)| f as &dyn mpedb_types::HostFns);
            let host_aggs: Option<&dyn mpedb_types::HostAggs> =
                tables.as_ref().map(|(_, a, _)| a as &dyn mpedb_types::HostAggs);
            let host_colls: Option<&dyn mpedb_types::HostColls> =
                tables.as_ref().map(|(_, _, c)| c as &dyn mpedb_types::HostColls);
            let r = self.engine.begin_read()?;
            // Staleness check UNDER THE SAME PIN that scans the rows (§4.3):
            // a policy edit that landed since compile invalidates the plan.
            // On error `r` drops here, releasing the reader slot.
            self.validate_policy_read(hash, plan, &r)?;
            let res = {
                let mut ctx = ReadCtx(&r, host, host_aggs, host_colls, ChargeMode::PerRow);
                exec_stmt(&mut ctx, &self.schema(), plan, params, &mut partial)
            };
            match res {
                Ok(out) => {
                    r.finish()?; // SnapshotEvicted here invalidates the rows
                    Ok(out)
                }
                Err(e) => Err(e), // Drop releases the reader slot
            }
        } else {
            self.run_write_plan(hash, plan, params)
        }
    }

    /// Autocommit DML with Phase-2 group commit (design/DESIGN.md §5.3).
    ///
    /// Uncontended: take the lock immediately and lead (executing our own
    /// statement plus any pending intents in one commit). Contended: publish
    /// the statement as an intent and wait-or-lead — a bounded futex wait,
    /// then a `try_begin_write`, repeated. A SIGKILLed leader can therefore
    /// never strand us, and N contended writers cost one meta flip (and one
    /// msync in durable mode) instead of N.
    pub(super) fn run_write_plan(
        &self,
        hash: Option<&PlanHash>,
        plan: &CompiledPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        // #109: the busy budget for this statement's FIRST acquisition,
        // fixed here. The optimistic-mode retry/serial-fallback sites in
        // ring_exec recompute a fresh budget per acquisition — sqlite's
        // busy handler is likewise per-lock-event, so a statement that
        // needs several acquisitions may wait up to that many budgets.
        let deadline = self.busy_deadline();
        // fast path: uncontended — validate policy staleness under the writer
        // lock we now hold (no policy edit can race it, §4).
        match self.engine.try_begin_write() {
            Ok(Some(mut txn)) => {
                self.validate_policy_write(hash, plan, &mut txn)?;
                return ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                    .map(|r| r.expect("own statement always yields a result"));
            }
            Ok(None) => {}
            // Under a busy policy, a sibling connection on THIS thread
            // holding the lock (ERRORCHECK reentry) is contention, not a
            // bug: fall through — `begin_write_deadline` below answers Busy
            // for it immediately (an unwinnable wait). With no policy the
            // loud EDEADLK diagnostic stands.
            Err(Error::Internal(ref m))
                if deadline.is_some() && m.contains("writer lock re-entered") => {}
            Err(e) => return Err(e),
        }
        // Contended: pre-check staleness on a read snapshot before enqueueing.
        // The leader that executes our intent also holds the writer lock, so a
        // policy edit racing an in-flight *durable* enqueued write has a bounded
        // one-commit residual window (documented limitation; durability=none
        // never rides the ring and is fully covered by the fast path above).
        {
            let r = self.engine.begin_read()?;
            let res = self.validate_policy_read(hash, plan, &r);
            let _ = r.finish();
            res?;
        }
        // Contended. Whether to ride the ring depends on the storage medium:
        // group commit pays for itself when commits are expensive (an msync
        // per commit in durability=commit — hundreds of µs to ms depending on
        // the disk), because N writers share ONE flush. On pure-memory /
        // non-durable databases a commit is microseconds, and the ring's
        // wait/wake round-trips would only add latency — block directly (the
        // lock holder still drains any intents, so mixed deployments stay
        // live).
        //
        // One plan class NEVER rides the ring: a plan that calls a host UDF
        // (design/DESIGN-UDF.md). The ring is a cross-PROCESS queue — a leader
        // loads an intent's plan BY HASH FROM THE SHARED REGISTRY, and a
        // host-call plan is deliberately never published there, so an enqueued
        // one could only come back as `UnknownPlan`. More importantly the
        // closures are connection-local: no other process may run our UDF, and
        // we may not run theirs. Leading our own statement keeps it in the
        // process that owns the closures.
        //
        // A statement carrying a busy DEADLINE (#109) never rides the ring
        // either. A published intent cannot be withdrawn at the deadline:
        // release-from-READY before the result post would race the leader's
        // collect→stage→post sequence (§5.3 pins READY+stamped slots to their
        // incarnation precisely so that can't happen), so an abandoned intent
        // could still COMMIT after we answered Busy — a phantom write — or the
        // leader's post could land in a re-reserved slot. Rather than touch
        // that ordering, a deadline-carrying write polls the lock directly and
        // answers Busy having enqueued nothing; once it does acquire, it still
        // leads and drains everyone else's intents, so ring deployments stay
        // live. (The group-commit amortization forgone is the busy-timeout
        // caller's explicit trade.)
        //
        // Nor does a write carrying `RETURNING`: its result is Rows, and a
        // ring result slot holds only an affected count (§5.3) — a foreign
        // leader executing the intent would have nowhere to post the rows
        // (`execute_prepared` refuses with "write plan returned rows").
        // Direct-path it: the owner executes its own statement and keeps the
        // full result, and still leads + drains others' intents on acquire.
        let use_ring = deadline.is_none()
            && ring_exec::ring_enabled(self)
            && !plan.contains_host_call()
            && !plan.has_returning();
        let ring = self.engine.ring();
        let enqueued = if use_ring {
            hash.and_then(|h| {
                let blob = ring_exec::encode_params(params);
                ring.enqueue(h, &blob)
            })
        } else {
            None
        };
        let Some((idx, owned)) = enqueued else {
            // ring off / full / params too large / hash unavailable / busy
            // deadline set: take the lock directly (bounded iff a deadline).
            let txn = self.engine.begin_write_deadline(deadline)?;
            return ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                .map(|r| r.expect("own statement always yields a result"));
        };
        loop {
            if let Some(r) = ring.wait_result(idx, std::time::Duration::from_millis(2)) {
                ring.release(idx, owned);
                return match ring_exec::decode_ring_result(r) {
                    // the leader had no way to load our plan (e.g. local-only
                    // seeding); we DO have it — run it directly instead
                    Err(Error::UnknownPlan(_)) => {
                        let txn = self.engine.begin_write()?;
                        ring_exec::lead_and_execute(self, txn, Some((plan, params)))
                            .map(|r| r.expect("own statement always yields a result"))
                    }
                    other => other,
                };
            }
            // another leader at work → keep waiting; lock free → we lead,
            // and our own intent is drained like any other
            if let Some(txn) = self.engine.try_begin_write()? {
                ring_exec::lead_and_execute(self, txn, None)?;
                if let Some(r) = ring.try_take_result(idx) {
                    ring.release(idx, owned);
                    return ring_exec::decode_ring_result(r);
                }
                // not drained (should not happen — we were READY); retry
            }
        }
    }
}
