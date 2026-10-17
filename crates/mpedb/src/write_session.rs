//! Interactive transaction statements, savepoints and commit.
use super::*;

impl WriteSession<'_> {
    /// Execute one SELECT or DML statement with complete statement rollback.
    /// A caller may handle a row/constraint error and keep earlier writes;
    /// rollback/release failures poison the session and remain fatal.
    pub fn execute_atomic(&mut self, hash: &PlanHash, params: &[Value]) -> Result<ExecResult> {
        if self.poisoned { return Err(poisoned_err()); }
        let plan = self.plan_by_hash(hash)?;
        if !plan.footprint.read_only && !matches!(plan.stmt, PlanStmt::Insert { .. } | PlanStmt::Update { .. } | PlanStmt::Delete { .. }) {
            return Err(Error::Unsupported("atomic execution requires SELECT or DML".into()));
        }
        let snapshot = self.txn.savepoint_full()?;
        match self.execute(hash, params) {
            Ok(value) => {
                self.txn.release_savepoint_full(&snapshot).inspect_err(|_| { self.poisoned = true; })?;
                Ok(value)
            }
            Err(original) => {
                if let Err(error) = self.txn.rollback_to_full(&snapshot).and_then(|()| self.txn.release_savepoint_full(&snapshot)) {
                    self.poisoned = true;
                    return Err(error);
                }
                self.poisoned = false;
                Err(original)
            }
        }
    }
    /// Scalar PL/SQL `RETURNING INTO`. Zero rows and many rows become Oracle
    /// errors before the savepoint is released, so a caught `NO_DATA_FOUND`
    /// or `TOO_MANY_ROWS` undoes this statement and keeps earlier ones.
    /// An affected-count result is returned unchanged. [`Self::execute_atomic`]
    /// stays the path for every other statement.
    pub fn execute_atomic_one_row(
        &mut self,
        hash: &PlanHash,
        params: &[Value],
    ) -> Result<ExecResult> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        let plan = self.plan_by_hash(hash)?;
        if !plan.footprint.read_only
            && !matches!(
                plan.stmt,
                PlanStmt::Insert { .. } | PlanStmt::Update { .. } | PlanStmt::Delete { .. }
            )
        {
            return Err(Error::Unsupported(
                "atomic execution requires SELECT or DML".into(),
            ));
        }
        let snapshot = self.txn.savepoint_full()?;
        let value = match self.execute(hash, params) {
            Ok(ExecResult::Rows { rows, .. }) if rows.len() != 1 => Err(if rows.is_empty() {
                Error::Oracle {
                    code: 1403,
                    message: "no data found".into(),
                }
            } else {
                Error::Oracle {
                    code: 1422,
                    message: "exact fetch returns more than requested number of rows".into(),
                }
            }),
            Ok(value) => Ok(value),
            Err(original) => Err(original),
        };
        match value {
            Ok(value) => {
                self.txn
                    .release_savepoint_full(&snapshot)
                    .inspect_err(|_| {
                        self.poisoned = true;
                    })?;
                Ok(value)
            }
            Err(original) => {
                if let Err(error) = self
                    .txn
                    .rollback_to_full(&snapshot)
                    .and_then(|()| self.txn.release_savepoint_full(&snapshot))
                {
                    self.poisoned = true;
                    return Err(error);
                }
                self.poisoned = false;
                Err(original)
            }
        }
    }
    /// DDL in this transaction can change both triggers and foreign keys
    /// before schema_gen advances. Do not reuse the committed rules cache.
    pub(super) fn write_rules(&mut self) -> Result<Arc<trigger::WriteRules>> {
        if !self.ddl_applied {
            return self.db.write_rules();
        }
        let bundle = self.txn.schema_bundle();
        let rows = self
            .txn
            .sys_scan_range(trigger::TRIGGER_PREFIX, trigger::TRIGGER_PREFIX_END)?;
        let mut rules = self.db.build_write_rules_from_rows(&bundle.schema, rows)?;
        if self.db.fk_enforced() {
            rules.fks = Some(Arc::new(fk::FkGraph::build(&bundle.schema)));
        }
        Ok(Arc::new(rules))
    }

    /// Schema as seen by this open write txn (includes uncommitted DDL).
    pub fn schema(&self) -> std::sync::Arc<mpedb_core::engine::SchemaBundle> {
        self.txn.schema_bundle()
    }

    /// `PRAGMA foreign_key_check` over THIS transaction — the same audit
    /// [`Database::foreign_key_check`] performs, but against the rows the
    /// session has written and not yet committed.
    ///
    /// The distinction is not a nuance, it is the difference between an answer
    /// and a wrong answer. Django's `TestCase` holds one open transaction for a
    /// whole test and calls `check_constraints` inside it; its foreign keys are
    /// DEFERRABLE INITIALLY DEFERRED, so a violating row is accepted at INSERT
    /// (in sqlite too) and settled only at COMMIT. Auditing a fresh read
    /// snapshot cannot see that row, so the pragma reported a clean database
    /// and the test's expected `IntegrityError` never came.
    ///
    /// Deliberately independent of the DEFERRED queue `commit` settles: the
    /// pragma's contract is "what is standing in the data", which includes
    /// violations written with enforcement off and therefore never queued.
    pub fn foreign_key_check(&mut self, table: Option<&str>) -> Result<Vec<FkCheckRow>> {
        let bundle = self.schema();
        let Some(only) = fk::check_scope(&bundle.schema, table) else {
            return Ok(Vec::new());
        };
        let mut ctx = exec::WriteCtx::new(&mut self.txn, None, None, None);
        fk::check_all(&mut ctx, &bundle.schema, only)
    }

    /// Views visible in this txn (committed + uncommitted `CREATE VIEW`).
    pub fn list_views(&mut self) -> Result<Vec<(String, String)>> {
        crate::ddl_apply::list_views_on_txn(&mut self.txn)
    }

    /// Triggers visible in this txn (committed + uncommitted `CREATE TRIGGER`).
    pub fn list_triggers(&mut self) -> Result<Vec<(String, String, String)>> {
        let bundle = self.txn.schema_bundle();
        crate::trigger::list_triggers_on_txn(&mut self.txn, &bundle)
    }

    /// Execute a prepared plan inside this transaction. Plans are resolved
    /// from the local cache or read from the registry *through this
    /// transaction* (no extra locking; the session already holds the writer
    /// lock).
    pub fn execute(&mut self, hash: &PlanHash, params: &[Value]) -> Result<ExecResult> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        let plan = self.plan_by_hash(hash)?;
        self.run(&plan, params)
    }

    /// Compile and run `sql` inside this transaction.
    ///
    /// Unlike [`Database::query`], the compiled plan is cached **only in the
    /// local plan cache and never published to the shared registry**: the
    /// registry insert needs its own write transaction, and this session
    /// already holds the single writer lock (the ERRORCHECK mutex would turn
    /// that into an error). Prepare statements you want shared *before*
    /// opening the session.
    /// [`Database::access_report`] against THIS session's schema view, so a
    /// statement touching a table created earlier in the same uncommitted
    /// transaction can still be described (#95). An authorization gate must be
    /// able to describe every statement it lets through, including those.
    pub fn access_report(&self, sql: &str) -> Result<AccessReport> {
        if let Some(ddl) = self.db.parse_ddl(sql)? {
            return Ok(access::ddl_access(&ddl));
        }
        let schema = self.txn.schema_bundle();
        let (plan, _explain) = self.db.compile_maybe_explain_with_schema(sql, &schema)?;
        Ok(access::plan_access(&plan, &schema))
    }

    /// Run a statement whose offsets were computed against `origin` rather than
    /// against the session's guard snapshot (#154).
    ///
    /// This exists for [`Database::submit_batch`], where one transaction
    /// carries sub-edits from clients that read at **different** versions. The
    /// guard is still taken against the oldest of them — that is what makes the
    /// transaction's refusal honest against everything outside the batch — but
    /// each member's `splice()` offset must be carried forward from the version
    /// *that member* saw. Walking a newer member from the batch minimum replays
    /// deltas it already accounted for, which lands the splice on bytes its
    /// author never saw, with no error.
    ///
    /// An `origin` older than the guard's snapshot is clamped to the guard's:
    /// the walk may not reach back past what the guard validated.
    ///
    /// Passed per statement rather than held on the session on purpose — a
    /// hidden mode is exactly what breaks when two calls get reordered.
    pub fn query_from_snapshot(
        &mut self,
        origin: u64,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        self.query_with_origin(Some(origin), sql, params)
    }

    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        self.query_with_origin(None, sql, params)
    }

    pub(super) fn query_with_origin(
        &mut self,
        origin: Option<u64>,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecResult> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        // #N2: transaction/savepoint control never takes the compile road in a
        // session. Django mints a UNIQUE savepoint name per use, so each op
        // paid a full compile — plus a memo insert whose CLOCK eviction aimed
        // at the hot texts, and an unbounded hash-cache entry. The recognizer
        // is the real grammar (tokenizer + the same statement arms) behind a
        // first-word byte gate, and it is fail-open: anything but a clean
        // exact parse returns None and takes the road below, so every refusal
        // keeps its canonical message. The arms are the same methods
        // `run_from` dispatches to — one body, two roads.
        if let Some(ctl) = mpedb_sql::parse_txn_control(sql) {
            use mpedb_sql::TxnControl as Tc;
            return match ctl {
                Tc::Savepoint(name) => self.open_savepoint(&name),
                Tc::Release(name) => {
                    self.release_savepoint(&name).map(|()| ExecResult::Affected(0))
                }
                Tc::RollbackTo(name) => self
                    .rollback_to_savepoint(&name)
                    .map(|()| ExecResult::Affected(0)),
                Tc::Begin | Tc::Commit | Tc::Rollback => Err(session_txn_verb_err()),
            };
        }
        // #168: the Django path. The C-API shim routes every statement to this
        // method whenever a transaction is open (capi/src/lib.rs:599), so
        // without this an ORM that wraps its work in BEGIN/COMMIT — which is to
        // say, an ORM — pays a full compile per statement and #166 buys it
        // nothing. Checked FIRST, the same ordering rule as `query_ctx` and for
        // the same reason: a hit also skips the ATTACH probe, the db-ref
        // resolver (two full tokenizations for dotted SQL) and the `parse_ddl`
        // probe below, all of which re-inspect the text on every call. The
        // hop is sound because a hit can only be a text that reached
        // compilation as a plain Passthrough statement: ATTACH/DETACH and DDL
        // return before `remember` and are never memoized (their refusals and
        // routing are reached by the same miss as before), a later ATTACH
        // cannot pull a bare name away from main (shadowing — `query_ctx`'s
        // argument, verbatim), and every temp-namespace mutation and DETACH
        // clears the memo via `drop_local_plans` (the multifile.rs S42
        // invariant: "the memo runs before every routing hook" — this call
        // site was the one non-conforming router). `memoized` itself still
        // enforces the `ddl_applied` latch.
        if let Some(res) = self.memoized(origin, sql, params)? {
            return Ok(res);
        }
        // #51: the attach list is connection state, not transaction state —
        // mutating it mid-transaction (or reading a second file from inside
        // this write txn's snapshot) is refused by name in v1.
        if mpedb_sql::parse_attach(sql)?.is_some() {
            return Err(Error::Unsupported(
                "ATTACH/DETACH inside an open transaction is not supported \
                 (run it in autocommit)"
                    .into(),
            ));
        }
        let memoizable;
        let routed;
        let sql = match self.db.resolve_db_refs_hook(sql)? {
            multifile::DbRoute::Passthrough => {
                memoizable = true;
                sql
            }
            multifile::DbRoute::Main(s) => {
                memoizable = false;
                routed = s;
                &routed
            }
            multifile::DbRoute::Cross { sql, tables, main_free } => {
                // A "cross" statement that touches exactly ONE member and
                // nothing on main is not a cross-file question at all — a
                // coordinated snapshot over one file IS that file's snapshot,
                // and a pure single-member WRITE has been forwarded on exactly
                // that reasoning all along. Forwarded HERE rather than in the
                // resolver because the resolver's answer is also the plan's
                // IDENTITY: a prepared cross plan is cached per attach epoch and
                // invalidated by DETACH, and re-routing it there changed what
                // `execute(hash)` resolves to. This is the one place the
                // statement was otherwise a hard refusal, so nothing that works
                // today changes.
                let one = tables
                    .first()
                    .map(|(db, _)| db.clone())
                    .filter(|db| {
                        // `main_free` is load-bearing: `tables` lists only the
                        // ATTACHED references, so without it `FROM t JOIN
                        // other.u` looked single-member and was forwarded to
                        // the member — where a same-named `t` silently joined
                        // the WRONG table and returned no rows.
                        main_free && tables.iter().all(|(d, _)| d.eq_ignore_ascii_case(db))
                    });
                match one {
                    Some(db) => {
                        // The cross rewrite renamed each table to the mangled
                        // `"db.table"`; the member knows it by its own name.
                        // An exact inverse of `mangle_db_table`, over the very
                        // list the resolver produced — not a guess at the text.
                        let mut bare = sql;
                        for (d, t) in &tables {
                            bare = bare.replace(
                                &format!("\"{}\"", mpedb_sql::mangle_db_table(d, t)),
                                &format!("\"{t}\""),
                            );
                        }
                        return self.db.query_attached_only(&db, &bare, params);
                    }
                    None => {
                        return Err(Error::Unsupported(
                            "cross-file SELECT inside an open write transaction is \
                             not supported in v1 (run it in autocommit)"
                                .into(),
                        ))
                    }
                }
            }
            // Pure attached-only work goes to the member handle, not this
            // session's COW. That is correct: the attached file is a separate
            // engine, and CPython (implicit main txn) still does
            // `ATTACH ':memory:'; CREATE TABLE attached.foo` mid-transaction.
            multifile::DbRoute::AttachedOnly { db, sql } => {
                return self.db.query_attached_only(&db, &sql, params);
            }
        };
        // #95: DDL (CREATE/DROP/ALTER TABLE, CREATE INDEX) runs THROUGH this
        // session's transaction so the schema change lives in the txn's COW
        // catalog pages and commits/rolls back atomically with the session's
        // DML — never a mid-transaction commit.
        if let Some(ddl) = self.db.parse_ddl(sql)? {
            return self.apply_ddl(ddl);
        }
        // Compile against THIS session's schema view — which includes any DDL
        // this session already applied (its txn's captured bundle), not just the
        // committed schema. Policies/views are read from the committed catalog
        // (a policy/view created inside this uncommitted session is not yet
        // visible here).
        let schema = self.txn.schema_bundle();
        let (plan, is_explain) = self.db.compile_maybe_explain_with_schema(sql, &schema)?;
        if is_explain {
            return Ok(ExecResult::Explain(plan.explain(&schema)));
        }
        let hash = plan.hash();
        let plan = {
            let mut cache = self.db.cache.write().expect(POISON);
            cache.entry(hash).or_insert_with(|| Arc::new(plan)).clone()
        };
        if memoizable && !self.ddl_applied {
            self.db.remember(schema.schema_gen, sql, &plan, hash)?;
        }
        self.run_from(origin, &plan, params)
    }

    /// The [`TextMemo`] fast path for a statement inside an open transaction
    /// (#168) — the in-session twin of [`Database::memoized`]. `Ok(None)` means
    /// "no usable entry, take the compile road".
    ///
    /// Why an in-session hit is the same answer a recompile would give, input
    /// by input — this is the whole argument, and every clause of it is a
    /// property of `compile_maybe_explain_with_schema`, not a new promise:
    ///
    /// * **schema** — the txn's bundle. Equal to the committed catalog exactly
    ///   while `ddl_applied` is false, which is what gates this method. The
    ///   session holds the single writer lock, so no peer can move the
    ///   committed schema underneath it either.
    /// * **row counts** — that function reads them off an INDEPENDENT read
    ///   snapshot of the last committed state, deliberately, so this session's
    ///   own uncommitted row deltas cannot move a plan (lib.rs, its comment
    ///   says so in as many words). [`Database::plan_tables`] reads the same
    ///   way. They agree by construction rather than by luck.
    /// * **policies and views** — read from the COMMITTED catalog on both
    ///   paths; a policy created inside this uncommitted session is not visible
    ///   to either. Same for the per-table policy epoch in the memo key.
    ///
    /// No `gate_cache_on_schema` here, for the same reason
    /// `compile_maybe_explain_with_schema` skips it: the writer lock is held,
    /// so the committed generation cannot move for the session's lifetime.
    /// The generation compared against is the txn bundle's own, not this
    /// handle's `cache_gen`, which nothing refreshes while a session is open.
    /// The lookup half of [`Self::memoized`], shared with
    /// [`Self::query_many`]: resolve `sql` to its memoized plan WITHOUT
    /// executing — gen compare, once-per-session table-fact revalidation,
    /// and the debug/corpus recompile oracle all live here, so every road
    /// through the memo is verified by the same assert.
    pub(super) fn memo_plan(&mut self, sql: &str) -> Result<Option<Arc<CompiledPlan>>> {
        if self.ddl_applied {
            // Cheap early-out only — the gen compare below would miss anyway.
            // The load-bearing half of this latch is the `remember` gate; the
            // field's doc carries the argument.
            return Ok(None);
        }
        let schema = self.txn.schema_bundle();
        let gen = schema.schema_gen;
        let (hash, tables) = {
            let memo = self.db.text_memo.read().expect(POISON);
            match memo.get(sql) {
                // `tables` = None: this session already revalidated the
                // entry's table facts for this hash, and within a session
                // those facts are frozen (see `validated`'s doc) — skip both
                // the clone and the probe below.
                Some(e) if e.gen == gen => (
                    e.hash,
                    (!self.validated.contains(&e.hash)).then(|| e.tables.clone()),
                ),
                _ => return Ok(None),
            }
        };
        // A miss, never an error — same rule as the autocommit path, with one
        // more way to get here: a plan first compiled INSIDE a session is never
        // published to the shared registry (that would need its own write txn
        // while this one holds the lock), so once it falls out of the local
        // cache the hash resolves to nothing.
        let Ok(plan) = self.plan_by_hash(&hash) else {
            return Ok(None);
        };
        if let Some(tables) = tables {
            if self.db.plan_tables(&plan)? != tables {
                return Ok(None);
            }
            self.validated.insert(hash);
        }
        if verify_plan_memo() {
            let (fresh, _) = self
                .db
                .compile_maybe_explain_with_schema(sql, &schema)
                .map_err(|e| {
                    Error::Internal(format!(
                        "#168: in-txn memo served a plan for {sql:?} that no \
                         longer compiles: {e}"
                    ))
                })?;
            assert_eq!(
                fresh.hash(),
                hash,
                "#168: in-txn memo served a stale plan for {sql:?} — a compile \
                 input moved without moving `schema_gen` or this plan's table facts"
            );
        }
        Ok(Some(plan))
    }

    /// The [`TextMemo`] fast path for a statement inside an open
    /// transaction: a hit executes through the same [`Self::run_from`] the
    /// compile road uses. `Ok(None)` = take the compile road.
    pub(super) fn memoized(
        &mut self,
        origin: Option<u64>,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<ExecResult>> {
        match self.memo_plan(sql)? {
            None => Ok(None),
            Some(plan) => {
                self.db.warn_if_risky(&plan);
                self.run_from(origin, &plan, params).map(Some)
            }
        }
    }

    /// Execute one parameterized DML text against MANY parameter rows (#N2
    /// — executemany's engine half): the plan and the per-statement fixed
    /// facts (trigger rules, schema bundle, host-fn tables, the policy
    /// staleness check) are resolved ONCE and the loop pays only bind +
    /// execute per row. `Ok(None)` means "this text wants the per-row
    /// road" — transaction control, ATTACH, db-qualified routing, DDL and
    /// EXPLAIN all fall back, so their refusals and routing keep canonical
    /// behavior (the parse_txn_control fail-open rule, one level up).
    /// Hoisting the facts is sound for the same reason the per-hit
    /// revalidation cache is: the session holds the writer lock, so no
    /// commit, policy edit or trigger change can land between two rows of
    /// one batch, and the batch itself applies no DDL (DDL texts fell back).
    /// The per-row guard/notify machinery stays per row, inside
    /// [`Self::exec_prepared`].
    pub fn query_many(&mut self, sql: &str, rows: &[Vec<Value>]) -> Result<Option<u64>> {
        if self.poisoned {
            return Err(poisoned_err());
        }
        if mpedb_sql::parse_txn_control(sql).is_some()
            || mpedb_sql::parse_attach(sql)?.is_some()
            || !matches!(
                self.db.resolve_db_refs_hook(sql)?,
                multifile::DbRoute::Passthrough
            )
            || self.db.parse_ddl(sql)?.is_some()
        {
            return Ok(None);
        }
        let plan = match self.memo_plan(sql)? {
            Some(plan) => plan,
            None => {
                // The compile road, exactly as `query_with_origin` walks it.
                let schema = self.txn.schema_bundle();
                let (plan, is_explain) =
                    self.db.compile_maybe_explain_with_schema(sql, &schema)?;
                if is_explain {
                    return Ok(None);
                }
                let hash = plan.hash();
                let plan = {
                    let mut cache = self.db.cache.write().expect(POISON);
                    cache.entry(hash).or_insert_with(|| Arc::new(plan)).clone()
                };
                if !self.ddl_applied {
                    self.db.remember(schema.schema_gen, sql, &plan, hash)?;
                }
                plan
            }
        };
        self.db.warn_if_risky(&plan);
        let schema = self.txn.schema_bundle();
        self.db.validate_policy_write(None, &plan, &mut self.txn)?;
        let triggers = self.write_rules()?;
        let tables = self.db.host_tables(&plan);
        // The whole batch runs inside an anonymous engine savepoint: on ANY
        // row error the batch is rolled back WHOLE and the error returned —
        // so a caller with richer per-row semantics (the dbapi's coercing,
        // stop-at-failing-row road) can rerun the same rows one by one with
        // no double-apply possible. Cheap since the pre-images went lazy.
        let snap = self.txn.savepoint_full()?;
        let mut total = 0u64;
        let mut failed: Option<Error> = None;
        for params in rows {
            // Rows-returning shapes contribute no affected count — the same
            // rule the dbapi's per-row fast road applies.
            match self.exec_prepared(None, &plan, params, &schema, &triggers, &tables) {
                Ok(ExecResult::Affected(k)) => total += k,
                Ok(_) => {}
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            None => {
                self.txn.release_savepoint_full(&snap).inspect_err(|_| {
                    self.poisoned = true;
                })?;
                Ok(Some(total))
            }
            Some(e) => {
                match self
                    .txn
                    .rollback_to_full(&snap)
                    .and_then(|()| self.txn.release_savepoint_full(&snap))
                {
                    // The rollback restored whole-statement state, so a
                    // poison set by the failing row's partial apply is
                    // cleared with it — the caller may rerun the rows on
                    // the per-row road (that is the whole contract).
                    Ok(()) => self.poisoned = false,
                    Err(_) => self.poisoned = true,
                }
                Err(e)
            }
        }
    }

    /// Commit everything written through this session.
    ///
    /// A poisoned session (see the type-level docs) refuses: the transaction
    /// is rolled back and [`Error::Unsupported`] is returned, so a partially
    /// applied statement can never be persisted.
    pub fn commit(mut self) -> Result<()> {
        if self.poisoned {
            exec::take_fk_deferred();
            self.txn.abort();
            return Err(poisoned_err());
        }
        // DEFERRABLE INITIALLY DEFERRED keys are settled HERE, before anything
        // is made durable (#194): a violation the transaction fixed in between
        // is not a violation, and one it did not is a failed COMMIT that takes
        // the whole transaction with it — sqlite's behaviour exactly.
        let pending = exec::take_fk_deferred();
        if !pending.is_empty() {
            let bundle = self.db.schema();
            let mut ctx = exec::WriteCtx::new(&mut self.txn, None, None, None);
            if let Err(e) = fk::settle_deferred(&mut ctx, &bundle.schema, &pending) {
                self.txn.abort();
                return Err(e);
            }
        }
        self.txn.commit()
    }

    /// Discard everything written through this session.
    pub fn rollback(self) {
        // Held-over violations belong to the transaction that made them.
        exec::take_fk_deferred();
        self.txn.abort()
    }

    // ------------- replication / CDC plane (DESIGN-MIRROR §3, §5.4) -------------
    //
    // The mirror applier and importer operate at the engine typed-API level —
    // BELOW RLS — inside a single session so row writes, CDC dirty-set changes,
    // and the mirror cursor all commit atomically in one meta flip. These
    // methods expose exactly that plane; they do not touch policies. They never
    // poison the session (the mirror manages its own savepoints); a failing op
    // leaves whatever partial effect the engine produced, to be unwound by the
    // caller via [`WriteSession::savepoint`]/[`WriteSession::rollback_to`].

    /// Turn CDC dirty-set capture on/off for this session's transaction. The
    /// importer and applier set it `false` so their own writes are not
    /// self-captured (DESIGN-MIRROR §3.8).
    pub fn set_capture(&mut self, on: bool) {
        self.txn.set_capture(on);
    }

    /// Allow this session to draw from the reserved control-page band so a
    /// control-plane commit succeeds even when the data region is full
    /// (DESIGN-MIRROR §3.10). Use only for small control writes.
    pub fn set_reserved_alloc(&mut self, on: bool) {
        self.txn.set_reserved_alloc(on);
    }

    /// Blind typed INSERT of a full row into `table_id`.
    pub fn insert_row(&mut self, table_id: u32, values: &[Value]) -> Result<()> {
        self.txn.insert_row(table_id, values)
    }

    /// INSERT a row whose column `stream_col` is PULLED from `src` a page at a
    /// time, so a large value is never resident (#43).
    ///
    /// `values[stream_col]` is a placeholder for the type check — pass an empty
    /// `Blob`/`Text`; the length comes from `src.len()`. The streamed column must
    /// be the row's last variable-length column.
    ///
    /// **Pull, not push, and it is deliberate.** A `writer.write_all(chunk)` API
    /// would hold the writer lock across YOUR code, so a blob arriving off a
    /// socket would block every other writer for as long as the network took.
    /// Here the engine asks for bytes as fast as it can write them. `src` is
    /// still called with the lock held — keep it cheap.
    ///
    /// Refused on a table with a secondary UNIQUE index: that probe needs the
    /// value, and the point of this call is that nobody has it.
    /// Copy a whole file into `stream_col` as a blob, streamed a page at a time
    /// so the file is never resident (#43). "Put this file in the database" as
    /// one call — the memory ceiling is one overflow page, not the file size, so
    /// a 4 GiB file inserts on a machine that could not hold it in RAM.
    ///
    /// `values[stream_col]` is a placeholder for the type check (pass an empty
    /// `Blob`/`Text`); the length comes from the file. The streamed column must
    /// be the row's last variable-length column, and the table must have no
    /// secondary UNIQUE index — same constraints as [`Self::insert_streaming`].
    pub fn insert_file(
        &mut self,
        table: &str,
        values: &[Value],
        stream_col: usize,
        path: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // FileBlobSource, not ReaderBlobSource: the file handle is what lets
        // the extent path bulk-copy kernel-side (copy_file_range, #50).
        let mut src = FileBlobSource::new(file, len);
        self.insert_streaming(table, values, stream_col, &mut src)
    }

    pub fn insert_streaming(
        &mut self,
        table: &str,
        values: &[Value],
        stream_col: usize,
        src: &mut dyn mpedb_core::btree::BlobSource,
    ) -> Result<()> {
        let table_id = self
            .db
            .engine
            .schema()
            .tables
            .iter()
            .position(|t| !t.dead && mpedb_types::ident_eq(&t.name, table))
            .ok_or_else(|| Error::Config(format!("no such table: {table}")))?
            as u32;
        self.txn
            .insert_row_streaming(table_id, values, stream_col, src)
    }

    /// Replace the full row with the given PK; returns whether it existed.
    pub fn update_by_pk(&mut self, table_id: u32, values: &[Value]) -> Result<bool> {
        self.txn.update_by_pk(table_id, values)
    }

    /// Delete by PK; returns whether the row existed.
    pub fn delete_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<bool> {
        self.txn.delete_by_pk(table_id, pk_values)
    }

    /// Read a full row by PK within this session's view.
    pub fn get_by_pk(&mut self, table_id: u32, pk_values: &[Value]) -> Result<Option<Vec<Value>>> {
        self.txn.get_by_pk(table_id, pk_values)
    }

    /// Statement-level savepoint for the mirror's per-op apply (§5.4/§6). Pairs
    /// with [`WriteSession::rollback_to`].
    pub fn savepoint(&self) -> mpedb_core::TxnSavepoint {
        self.txn.savepoint()
    }

    /// Roll back to a savepoint taken in this session.
    ///
    /// **Check [`undo_is_exact`](Self::undo_is_exact) first.** This is the cheap
    /// undo: it restores roots and page accounting, which does not cover an
    /// in-place mutation of a page that was already dirty. Calling it when the
    /// undo is not exact re-offers a page the tree still links (#160), and the
    /// engine's `debug_assert` will say so in a debug build.
    pub fn rollback_to(&mut self, sp: mpedb_core::TxnSavepoint) {
        self.txn.rollback_to(sp)
    }

    /// Can [`rollback_to`](Self::rollback_to) undo everything since `sp`
    /// exactly? See `mpedb_core::WriteTxn::undo_is_exact` — nothing allocated
    /// since, or the transaction was pristine when the savepoint was taken.
    ///
    /// A caller that rolls back a FAILED operation and keeps going must consult
    /// this: "the operation failed" is not "the operation changed nothing".
    pub fn undo_is_exact(&self, sp: &mpedb_core::TxnSavepoint) -> bool {
        self.txn.undo_is_exact(sp)
    }

    /// Declare that this transaction changed something every attached process
    /// must recompile against, bumping `schema_gen` on commit.
    ///
    /// The DDL paths call the same thing on their own txn. It is `pub(crate)`
    /// for writers that are not DDL but ARE compile inputs — `stats::analyze`
    /// is the one today (#166): NDV records feed `index_ndv_bucket` on every
    /// compile, so a plan cache that outlived an ANALYZE would make the command
    /// do nothing at all.
    pub(crate) fn bump_schema_gen(&mut self) {
        self.txn.bump_schema_gen();
    }

    /// Store a namespaced system record through THIS transaction (atomic with
    /// the session's row writes — unlike [`Database::sys_record_put`], which
    /// opens its own txn and would self-lock under an open session).
    pub fn sys_record_put(&mut self, ns: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let subkey = sys_record_subkey(ns, key)?;
        if value.len() > SYS_RECORD_MAX_VALUE {
            return Err(Error::Unsupported(format!(
                "system record value too large ({} > {SYS_RECORD_MAX_VALUE} bytes)",
                value.len()
            )));
        }
        self.txn.sys_put(&subkey, value)
    }

    /// Read a namespaced system record through this transaction.
    pub fn sys_record_get(&mut self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let subkey = sys_record_subkey(ns, key)?;
        self.txn.sys_get(&subkey)
    }

    /// Delete a namespaced system record through this transaction.
    pub fn sys_record_delete(&mut self, ns: &str, key: &[u8]) -> Result<bool> {
        let subkey = sys_record_subkey(ns, key)?;
        self.txn.sys_delete(&subkey)
    }

    /// Publish a columnar watermark for `table_id`, but only if the table's
    /// mod_gen still equals `expect_gen` (see
    /// [`mpedb_core::engine::WriteTxn::set_columnar_watermark_if_gen`]). Used by
    /// the compaction pass to arm the segment/row-tail split scan atomically with
    /// its own segment writes.
    pub(crate) fn set_columnar_watermark_if_gen(
        &mut self,
        table_id: u32,
        expect_gen: u64,
        covered_rows: u64,
        wm_pk: &[u8],
    ) -> Result<bool> {
        self.txn
            .set_columnar_watermark_if_gen(table_id, expect_gen, covered_rows, wm_pk)
    }

    /// Drop a table's columnar watermark through this transaction (segments are
    /// being dropped or rebuilt).
    pub(crate) fn clear_columnar_watermark(&mut self, table_id: u32) -> Result<()> {
        self.txn.clear_columnar_watermark(table_id)
    }

    /// Store a column-segment block as a contiguous extent under its namespaced
    /// sys-record key (DESIGN-COLUMNAR §7.3). Unlike [`sys_record_put`], the
    /// value is NOT size-capped — the bytes live in an extent, and the catalog
    /// cell is a tiny reference the verifier and the extent-free path both know.
    /// A re-`put` or [`sys_record_delete`] of the same key frees the old run
    /// automatically. See [`mpedb_core::engine::WriteTxn::sys_put_extent`].
    ///
    /// [`sys_record_put`]: Self::sys_record_put
    /// [`sys_record_delete`]: Self::sys_record_delete
    pub(crate) fn sys_record_put_extent(&mut self, ns: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        let subkey = sys_record_subkey(ns, key)?;
        self.txn.sys_put_extent(&subkey, bytes)
    }

    /// Scan namespaced system records whose key is in `[lo, hi)`, prefix-bounded
    /// (the mirror's dirty-set / park scans). Keys returned with the namespace
    /// prefix stripped.
    ///
    /// `lo` may be empty (start of the namespace). A whole-namespace scan is
    /// `lo = []`, `hi = [0xff]` — the exclusive end is the next namespace
    /// (`<ns>\x01`), matching [`Database::sys_record_scan`]. An empty `lo` used
    /// to fail `sys_record_subkey`'s 1..=N key rule; that made in-txn
    /// `sqlite_master` lose every autocommit verbatim-DDL record (CPython
    /// `test_table_dump`).
    pub fn sys_record_scan_range(
        &mut self,
        ns: &str,
        lo: &[u8],
        hi: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        check_sys_ns(ns)?;
        if lo.len() > SYS_RECORD_MAX_KEY || hi.len() > SYS_RECORD_MAX_KEY {
            return Err(Error::Unsupported(format!(
                "system record key must be ≤{SYS_RECORD_MAX_KEY} bytes"
            )));
        }
        let mut klo = Vec::with_capacity(ns.len() + 1 + lo.len());
        klo.extend_from_slice(ns.as_bytes());
        klo.push(0);
        klo.extend_from_slice(lo);
        // Exclusive end: for a whole-namespace probe use `<ns>\x01` (same as
        // `Database::sys_record_scan`); otherwise `<ns>\0‖hi`.
        let khi = if lo.is_empty() && hi == [0xff] {
            let mut e = ns.as_bytes().to_vec();
            e.push(1);
            e
        } else {
            let mut k = Vec::with_capacity(ns.len() + 1 + hi.len());
            k.extend_from_slice(ns.as_bytes());
            k.push(0);
            k.extend_from_slice(hi);
            k
        };
        let strip = ns.len() + 1;
        Ok(self
            .txn
            .sys_scan_range(&klo, &khi)?
            .into_iter()
            .map(|(k, v)| (k[strip..].to_vec(), v))
            .collect())
    }

    /// [`sys_record_scan_range`](Self::sys_record_scan_range) yielding only
    /// KEYS (namespace-stripped), values never resolved. `lo`/`hi` bound one
    /// column's block records; the re-compaction deletes them without pulling
    /// each 512 KiB extent back in. Whole-namespace `lo = []`, `hi = [0xff]`.
    pub(crate) fn sys_record_scan_range_keys(
        &mut self,
        ns: &str,
        lo: &[u8],
        hi: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        check_sys_ns(ns)?;
        if lo.len() > SYS_RECORD_MAX_KEY || hi.len() > SYS_RECORD_MAX_KEY {
            return Err(Error::Unsupported(format!(
                "system record key must be ≤{SYS_RECORD_MAX_KEY} bytes"
            )));
        }
        let mut klo = Vec::with_capacity(ns.len() + 1 + lo.len());
        klo.extend_from_slice(ns.as_bytes());
        klo.push(0);
        klo.extend_from_slice(lo);
        let khi = if lo.is_empty() && hi == [0xff] {
            let mut e = ns.as_bytes().to_vec();
            e.push(1);
            e
        } else {
            let mut k = Vec::with_capacity(ns.len() + 1 + hi.len());
            k.extend_from_slice(ns.as_bytes());
            k.push(0);
            k.extend_from_slice(hi);
            k
        };
        let strip = ns.len() + 1;
        Ok(self
            .txn
            .sys_scan_range_keys(&klo, &khi)?
            .into_iter()
            .map(|k| k[strip..].to_vec())
            .collect())
    }

    pub(super) fn plan_by_hash(&mut self, hash: &PlanHash) -> Result<Arc<CompiledPlan>> {
        if let Some(p) = self.db.cache.read().expect(POISON).get(hash) {
            return Ok(p.clone());
        }
        let subkey = plan_subkey(hash, self.db.dialect);
        let Some(record) = self.txn.sys_get(&subkey)? else {
            return Err(Error::UnknownPlan(*hash));
        };
        let plan = Arc::new(decode_registry_plan(
            &record,
            hash,
            &self.db.schema(),
            self.db.dialect,
        )?);
        // last_used_txn refresh rides on this transaction (commits or rolls
        // back with it — best-effort bookkeeping either way). A failed put
        // (e.g. DbFull) leaves the record as it was and must never fail the
        // user's statement over eviction metadata.
        if let Some(patched) = patched_last_used(&record, self.txn.meta.txn_id + 1) {
            let _ = self.txn.sys_put(&subkey, &patched);
        }
        self.db
            .cache
            .write()
            .expect(POISON)
            .insert(*hash, plan.clone());
        Ok(plan)
    }

    pub(super) fn run(&mut self, plan: &CompiledPlan, params: &[Value]) -> Result<ExecResult> {
        self.run_from(None, plan, params)
    }

    pub(super) fn run_from(
        &mut self,
        origin: Option<u64>,
        plan: &CompiledPlan,
        params: &[Value],
    ) -> Result<ExecResult> {
        // Transaction/savepoint control is handled here against the session's
        // own state, never compiled to an access path. `run` is only reached
        // from `query`/`execute`, which already refuse a poisoned session, so a
        // savepoint op never runs on a torn transaction.
        match &plan.stmt {
            PlanStmt::Savepoint(name) => return self.open_savepoint(name),
            PlanStmt::Release(name) => {
                return self.release_savepoint(name).map(|()| ExecResult::Affected(0));
            }
            PlanStmt::RollbackTo(name) => {
                return self
                    .rollback_to_savepoint(name)
                    .map(|()| ExecResult::Affected(0));
            }
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {
                return Err(session_txn_verb_err());
            }
            _ => {}
        }
        // Staleness check under this session's own write txn (holds the writer
        // lock, so no policy edit can race it). Local-cache plans only, so no
        // shared-registry eviction is needed here.
        self.db.validate_policy_write(None, plan, &mut self.txn)?;
        let triggers = self.write_rules()?;
        // Execute against the session's OWN schema view (== the txn's captured
        // bundle), so a statement touching a table this session created/altered
        // earlier resolves against the shape it will commit with (#95). For a
        // session that has applied no DDL this equals the committed schema.
        let schema = self.txn.schema_bundle();
        // Host UDF closures for THIS statement (design/DESIGN-UDF.md). This is
        // the path CPython's implicit transaction takes: after the first DML,
        // every statement — reads included — arrives here, so a UDF that
        // resolves in autocommit must resolve here too or Django breaks the
        // moment it stops committing between statements. `None` for a plan with
        // no host call, which then runs on the bare `&mut WriteTxn` exactly as
        // before.
        let tables = self.db.host_tables(plan);
        self.exec_prepared(origin, plan, params, &schema, &triggers, &tables)
    }

    /// The per-row execution body shared by [`Self::run_from`] and
    /// [`Self::query_many`] — param resolve, the #139 notify hint, the
    /// #142/#151 guard machinery, the host-closure wiring and the
    /// poison-on-partial contract in ONE place, so the bulk road cannot
    /// drift from the single-statement one.
    pub(super) fn exec_prepared(
        &mut self,
        origin: Option<u64>,
        plan: &CompiledPlan,
        params: &[Value],
        schema: &std::sync::Arc<mpedb_core::engine::SchemaBundle>,
        triggers: &trigger::WriteRules,
        tables: &Option<(HostFnTable, HostAggTable, HostCollTable)>,
    ) -> Result<ExecResult> {
        let full = session::resolve_params(plan, params, &self.session)?;
        // Change-notification key hint (#139 S2). An interactive session runs
        // several statements into ONE commit, so this is the path where the
        // collapse-on-conflicting-keys rule earns its keep — and the path a
        // ring-only wiring would have missed entirely.
        ring_exec::hint_notify_keys(&mut self.txn, plan, &full);
        // Guard safety net (#142 G1): a statement that reaches outside the
        // declared surface widens it rather than escaping it. Declaring is how
        // you get a SMALL guard; this is what stops a wrong declaration from
        // becoming a wrong answer.
        // #151: carry a pending sub-edit's offset across the commits it never
        // saw, BEFORE the statement runs and before the surface is widened —
        // so both the splice and the guard speak about the same bytes.
        let mut full = full;
        ring_exec::rebase_splice_params(&mut self.txn, plan, full.to_mut(), origin)?;
        ring_exec::widen_guard(&mut self.txn, plan, &full);
        let mut partial = false;
        let host: Option<&dyn mpedb_types::HostFns> =
            tables.as_ref().map(|(f, _, _)| f as &dyn mpedb_types::HostFns);
        let aggs: Option<&dyn mpedb_types::HostAggs> =
            tables.as_ref().map(|(_, a, _)| a as &dyn mpedb_types::HostAggs);
        let colls: Option<&dyn mpedb_types::HostColls> =
            tables.as_ref().map(|(_, _, c)| c as &dyn mpedb_types::HostColls);
        let mut ctx = exec::WriteCtx::new(&mut self.txn, host, aggs, colls);
        let res = exec::exec_stmt_triggered(
            &mut ctx,
            schema,
            plan,
            &full,
            &mut partial,
            triggers,
            0,
        );
        if res.is_err() && partial {
            // The failed statement may have applied part of its effects; the
            // transaction no longer reflects whole statements. See the
            // poisoning contract in the WriteSession docs.
            self.poisoned = true;
        }
        res
    }

    /// `SAVEPOINT <name>`: capture the engine snapshot and push the marker.
    /// One body for both roads — `run_from`'s compiled-plan arm (EXECUTE by
    /// hash) and the `parse_txn_control` fast dispatch in `query_with_origin`
    /// — so they cannot drift.
    pub(super) fn open_savepoint(&mut self, name: &str) -> Result<ExecResult> {
        let snap = self.txn.savepoint_full()?;
        self.savepoints.push(NamedSavepoint {
            name: name.to_string(),
            snap,
        });
        Ok(ExecResult::Affected(0))
    }

    /// Innermost (topmost) savepoint matching `name`, case-insensitively —
    /// sqlite's shadowing rule (a `RELEASE`/`ROLLBACK TO` targets the most
    /// recently opened savepoint of that name).
    pub(super) fn find_savepoint(&self, name: &str) -> Option<usize> {
        self.savepoints
            .iter()
            .rposition(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// `RELEASE [SAVEPOINT] <name>`: drop `<name>` and everything above it from
    /// the stack. The changes made since STAY (they merge into the enclosing
    /// savepoint/transaction) — only the markers and their snapshots go. It does
    /// NOT commit to disk (the surrounding `WriteSession` still owns the
    /// transaction); commit is `WriteSession::commit`.
    pub(super) fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let idx = self
            .find_savepoint(name)
            .ok_or_else(|| no_such_savepoint(name))?;
        // Engine first: fold the released layers' pre-images into the
        // enclosing savepoint's journal (a ROLLBACK TO an outer name must
        // still undo everything the released scope touched), THEN drop the
        // name markers — the two stacks move together or not at all. The
        // only error is the desync class: poison rather than run on with an
        // undo channel that no longer lines up.
        if let Err(e) = self.txn.release_savepoint_full(&self.savepoints[idx].snap) {
            self.poisoned = true;
            return Err(e);
        }
        self.savepoints.truncate(idx);
        Ok(())
    }

    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] <name>`: undo every change since
    /// `<name>` was opened, but KEEP `<name>` on the stack (it can be rolled
    /// back to again). Savepoints opened after it are discarded.
    pub(super) fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        let idx = self
            .find_savepoint(name)
            .ok_or_else(|| no_such_savepoint(name))?;
        // Engine first (it pops its own journal layers above the target), the
        // name stack after. By reference: the snapshot stays on the stack for
        // a repeat `ROLLBACK TO` the same name (the engine keeps the target's
        // journal layer alive, emptied). An error here is the Corrupt/desync
        // class and the replay may have applied partially — the review's
        // point: the drained layer cannot be replayed twice, so a retry
        // would silently restore less than it claims. Poison instead.
        if let Err(e) = self.txn.rollback_to_full(&self.savepoints[idx].snap) {
            self.poisoned = true;
            return Err(e);
        }
        // Keep the target; drop everything opened after it.
        self.savepoints.truncate(idx + 1);
        // The rollback restored the catalog PAGES; the txn's captured schema
        // BUNDLE is derived from them and has to be rebuilt, or a DDL undone
        // here would leave the session describing a table the catalog no longer
        // has. That rebuild is the same call the DDL path already makes after a
        // successful mutation — which is why DDL inside a savepoint was refused
        // for want of it, and why it no longer has to be.
        //
        // A failure here means reading back the catalog we just restored did
        // not work: the txn view is stale, so the session is poisoned rather
        // than left to commit something nobody can describe.
        //
        // BOTH the rebuild and the plan-cache clear exist for undone DDL —
        // and `ddl_applied` is a one-way latch, so a session it is false for
        // cannot have moved the schema: the bundle still equals the committed
        // catalog's and every cached plan still describes it. Skipping both
        // is what keeps the savepoint-per-test pattern (Django's TestCase)
        // from paying a catalog decode AND a whole plan-cache eviction per
        // test. (Mid-transaction sys-keyspace writes — registry last-used
        // patches — share the catalog TREE, but neither the bundle nor a
        // compiled plan reads them; their rollback is bookkeeping.)
        if self.ddl_applied {
            if let Err(e) = self.txn.reload_bundle_from_catalog() {
                self.poisoned = true;
                return Err(e);
            }
            // A cached plan may reference the catalog as it was BEFORE the
            // rollback.
            self.db.cache.write().expect(POISON).clear();
        }
        Ok(())
    }

    /// Apply a parsed DDL statement THROUGH this session's transaction (#95).
    ///
    /// Table DDL (CREATE / DROP / ALTER TABLE, CREATE INDEX, CREATE VIRTUAL
    /// TABLE) runs on `self.txn`, so the schema change lives in the txn's COW
    /// catalog pages and commits / rolls back atomically with the session's
    /// DML — no separate transaction, no mid-transaction commit (the forbidden
    /// "commit + autocommit-DDL + re-begin" hack). After a successful mutation
    /// the txn's captured schema bundle is rebuilt so a later statement in the
    /// SAME session sees the change, and the shared plan cache is dropped
    /// (mirroring the autocommit DDL discipline in `ddl_apply.rs`).
    ///
    /// A mid-operation failure (e.g. `DbFull` during an ALTER row rewrite) is
    /// undone with a full savepoint taken before the mutation, leaving the
    /// session usable — scoped to just the DDL, like the autocommit path's
    /// abort-on-error but without discarding the session's other work. If that
    /// undo is itself impossible (a large-blob extent write in the DDL), the
    /// session is poisoned instead (sound: commit then refuses).
    ///
    /// VIEW and TRIGGER DDL ride this session's txn (sys-keyspace + schema_gen
    /// in the same COW commit as DML). POLICY / RLS still refuse in-session.
    /// `ANALYZE` / `REINDEX` are accepted no-ops.
    pub(crate) fn apply_ddl(&mut self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::DdlStmt;
        if self.poisoned {
            return Err(poisoned_err());
        }
        // DDL inside a SAVEPOINT used to be refused here: a `ROLLBACK TO` would
        // have to revert the txn's captured schema bundle too, and the engine's
        // savepoint restores catalog PAGES, not the in-memory bundle. The
        // rollback path now rebuilds the bundle from those pages — the same call
        // this function makes after a successful mutation — so the desync the
        // refusal guarded against cannot happen. Django's migration executor
        // wraps every DDL step in a savepoint, so this was most of its schema
        // surface.
        // No-op DDL never touches the catalog — nothing to snapshot or reload.
        if matches!(ddl, DdlStmt::Analyze { .. } | DdlStmt::Reindex { .. }) {
            return Ok(ExecResult::Affected(0));
        }
        // Snapshot the txn BEFORE the mutation so a partial failure rolls back
        // cleanly. This captures catalog_root, table_roots, dirty-page contents
        // and schema_gen_bump — the bundle is NOT touched here, and we only
        // advance it AFTER the mutation succeeds, so the pre-mutation bundle
        // stays valid for the rollback path.
        let snap = self.txn.savepoint_full()?;
        match self.apply_ddl_inner(ddl) {
            Ok(res) => {
                // The mutation is in the txn's COW catalog pages. Rebuild the
                // txn's captured bundle so a later statement in this session
                // sees it, then drop the shared plan cache (a cached plan may
                // reference the pre-DDL catalog — the autocommit discipline).
                if let Err(e) = self.txn.reload_bundle_from_catalog() {
                    // Reading back what we just wrote failed (Corrupt): the txn
                    // view is now stale — poison so it cannot be committed.
                    self.poisoned = true;
                    return Err(e);
                }
                // This session's view has left the committed catalog behind.
                self.ddl_applied = true;
                // Unreachable while the latch holds (`memoized` early-outs),
                // cleared anyway so a future latch refinement cannot inherit
                // pre-DDL validations by accident.
                self.validated.clear();
                // The anonymous savepoint's journal layer folds into the
                // enclosing named savepoint (if any) — a ROLLBACK TO an outer
                // name must still undo this DDL's page mutations. A failure
                // here is a layer-stack desync: poison rather than run on
                // with an undo channel that no longer lines up.
                if let Err(e) = self.txn.release_savepoint_full(&snap) {
                    self.poisoned = true;
                    return Err(e);
                }
                // `cache` only — and this is the ONE clear site that must not
                // also drop the text memo (#168). The memo is keyed to the
                // COMMITTED generation, and this DDL is not committed: its
                // `schema_gen` bump rides the txn and lands at commit, where
                // the next `gate_cache_on_schema` drops both. Until then the
                // memo's entries still describe the catalog every reader
                // outside this transaction sees, and a ROLLBACK leaves them
                // correct — throwing them away here would discard live entries
                // to invalidate nothing. The hash cache is cleared anyway
                // because plans in it are reachable BY HASH from inside this
                // session, where the uncommitted schema does apply.
                self.db.cache.write().expect(POISON).clear();
                Ok(res)
            }
            Err(e) => {
                // Undo any partial effect. `rollback_to_full` restores the
                // catalog/pages; the bundle was never advanced, so the session
                // view stays consistent and usable. If the scope crossed a
                // large-blob extent write it refuses — the txn is then torn, so
                // poison. Either way the anonymous savepoint's journal layer
                // is disposed of (folded into an enclosing named savepoint if
                // one is open), or the layer stack would drift from the
                // facade's name stack.
                if self
                    .txn
                    .rollback_to_full(&snap)
                    .and_then(|()| self.txn.release_savepoint_full(&snap))
                    .is_err()
                {
                    self.poisoned = true;
                }
                Err(e)
            }
        }
    }

    /// One name of a `DROP TABLE` list, inside this session's transaction.
    ///
    /// Split out so the list is a loop rather than a nested block: everything
    /// here is per-table, and the only thing the list changes is how many
    /// times it runs. `schema` is the session's CAPTURED view, so a name
    /// dropped earlier in the same list still resolves in it — harmless,
    /// because `drop_table` rejects an id it has already freed.
    pub(super) fn drop_one_table(
        &mut self,
        schema: &std::sync::Arc<mpedb_core::engine::SchemaBundle>,
        name: &str,
        if_exists: bool,
    ) -> Result<()> {
        let Some(id) = schema.schema.table_id(name) else {
            if if_exists {
                return Ok(());
            }
            return Err(Error::Bind(format!("DROP TABLE: no such table `{name}`")));
        };
        // An fts4 vtab owns its five shadows — all six drop in this txn (gated
        // on the MODULE tag, never guessed from names).
        let shadow_ids: Vec<u32> = match schema
            .schema
            .tables
            .iter()
            .find(|t| t.id == id && !t.dead)
            .map(|t| t.kind)
        {
            Some(mpedb_types::TableKind::Fts {
                module: mpedb_types::FtsModule::Fts4,
                ..
            }) => ["_content", "_docsize", "_segdir", "_segments", "_stat"]
                .iter()
                .filter_map(|sfx| schema.schema.table_id(&format!("{name}{sfx}")))
                .collect(),
            _ => Vec::new(),
        };
        // Cascade: a dropped table's triggers are dead — remove their records
        // in the same commit (DESIGN-TRIGGERS §3.1).
        crate::trigger::cascade_drop_triggers(&mut self.txn, id)?;
        self.txn.drop_table(id)?;
        for sid in shadow_ids {
            crate::trigger::cascade_drop_triggers(&mut self.txn, sid)?;
            self.txn.drop_table(sid)?;
        }
        Ok(())
    }

    /// The DDL dispatch for [`apply_ddl`](Self::apply_ddl): resolve names against
    /// this session's CURRENT schema view and call the engine DDL primitive on
    /// `self.txn`. Any error (or the rollback / reload) is handled by the caller.
    pub(super) fn apply_ddl_inner(&mut self, ddl: mpedb_sql::DdlStmt) -> Result<ExecResult> {
        use mpedb_sql::DdlStmt;
        // Resolve against the session's own (possibly uncommitted-DDL) view, so
        // e.g. a CREATE INDEX names a table this same session just created.
        let schema = self.txn.schema_bundle();
        let resolve = |name: &str| -> Result<u32> {
            schema
                .schema
                .table_id(name)
                .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{name}`")))
        };
        match ddl {
            DdlStmt::CreateEnum { .. } | DdlStmt::DropEnum { .. } => return Err(Error::Unsupported("enum DDL inside a native WriteSession".into())),
            DdlStmt::CreateTable(spec) => {
                let def = crate::ddl_apply::table_def_from_spec(spec, &self.db.host_udf_set())?;
                self.txn.create_table(def)?;
            }
            DdlStmt::CreateVirtualTable(spec) => {
                if schema.schema.table_id(&spec.name).is_some() {
                    if spec.if_not_exists {
                        return Ok(ExecResult::Affected(0));
                    }
                    return Err(Error::Bind(format!(
                        "CREATE VIRTUAL TABLE: `{}` already exists",
                        spec.name
                    )));
                }
                let shadows = matches!(spec.module, mpedb_types::FtsModule::Fts4)
                    .then(|| crate::ddl_apply::fts4_shadow_defs(&spec.name, &spec.columns));
                let def = crate::ddl_apply::virtual_table_def_from_spec(spec)?;
                self.txn.create_table(def)?;
                // fts4: the five shadows ride the SAME txn (plan §7) — a name
                // collision on any of them fails the whole statement, as stock.
                for sh in shadows.into_iter().flatten() {
                    self.txn.create_table(sh)?;
                }
            }
            // A COMMENT is metadata in the sys keyspace, written through THIS
            // transaction so it commits and rolls back with the rest of the
            // block — PostgreSQL's DDL is transactional, and a `COMMENT ON`
            // that outlived a `ROLLBACK` would be the same wrong answer a
            // surviving `CREATE TABLE` was.
            //
            // It used to be refused here, because the autocommit applier opens
            // its OWN transaction and committing inside an open one is not a
            // thing. The fix is not to route it there: it is to do the two
            // sys-keyspace writes directly, which is all that applier does
            // once its validation has run.
            DdlStmt::Comment { target, text } => {
                let (table, column) = match &target {
                    mpedb_sql::CommentTarget::Table(t) => (t, None),
                    mpedb_sql::CommentTarget::Column { table, column } => (table, Some(column)),
                    mpedb_sql::CommentTarget::Constraint { table, .. } => (table, None),
                };
                // Validated against THIS transaction's schema, so a table
                // created earlier in the same block is visible — which is
                // exactly the case that made this worth doing.
                let bundle = self.txn.schema_bundle();
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
                let key = crate::Database::comment_key(&target);
                match text.as_deref() {
                    // `IS NULL` removes it; deleting what was never there is a
                    // no-op, as in PostgreSQL.
                    None => {
                        self.txn.sys_delete(&key)?;
                    }
                    Some(t) => self.txn.sys_put(&key, t.as_bytes())?,
                }
                self.txn.bump_schema_gen();
                return Ok(ExecResult::Affected(0));
            }
            // `cascade` is not consulted on this path: it is the WriteSession
            // (explicit-transaction) form, and the orphan-row refusal it would
            // suppress lives on the autocommit path in `apply_drop_tables`.
            //
            // The LIST is genuinely atomic here — one transaction — which the
            // autocommit path cannot promise.
            DdlStmt::DropTable { names, if_exists, cascade: _ } => {
                for name in &names {
                    self.drop_one_table(&schema, name, if_exists)?;
                }
            }
            DdlStmt::AlterRenameTable { table, new_name } => {
                let id = resolve(&table)?;
                self.txn.alter_rename_table(id, &new_name)?;
            }
            DdlStmt::AlterRenameColumn { table, column, new_name } => {
                let id = resolve(&table)?;
                let t = schema.schema.table(id).expect("resolve() returned a live id");
                let srcs = crate::ddl_apply::rename_generated_srcs(t, &column, &new_name)?;
                let chk = crate::ddl_apply::rename_check_srcs(t, &column, &new_name)?;
                self.txn.alter_rename_column(id, &column, &new_name, &srcs, &chk)?;
            }
            DdlStmt::AlterAddColumn { table, column } => {
                let id = resolve(&table)?;
                let def = schema
                    .schema
                    .table(id)
                    .ok_or_else(|| Error::Bind(format!("ALTER TABLE: no such table `{table}`")))?;
                let (col, fill) = crate::ddl_apply::add_column_from_spec(def, column)?;
                // The ALTER-time CHECK discipline is the SAME decision pair as
                // the autocommit applier's — compile against the engine's own
                // widening, then test every existing row with the fill in the
                // new slot (sqlite scans too; see the helpers' contract).
                let chk = crate::ddl_apply::compile_added_column_check(&schema.schema, id, &col)?;
                if let Some(c) = &chk {
                    crate::ddl_apply::refuse_check_violating_rows(&mut self.txn, id, c, &fill)?;
                }
                self.txn.alter_add_column(id, col, fill)?;
            }
            DdlStmt::AlterAddForeignKey { table, foreign_key } => {
                let id = resolve(&table)?;
                crate::ddl_apply::add_foreign_key_on_txn(&mut self.txn, id, foreign_key)?;
            }
            DdlStmt::AlterAddUnique { table, columns, name } => {
                let id = resolve(&table)?;
                crate::ddl_apply::add_unique_on_txn(&mut self.txn, id, &columns, name)?;
            }
            DdlStmt::AlterDropColumn { table, column } => {
                let id = resolve(&table)?;
                self.txn.alter_drop_column(id, &column)?;
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
                let id = schema
                    .schema
                    .table_id(&table)
                    .ok_or_else(|| Error::Bind(format!("CREATE INDEX: no such table `{table}`")))?;
                let t = schema.schema.table(id).expect("table_id resolved");
                // Same resolution as the autocommit applier: an expression part
                // takes the sentinel, and its source is compiled against the
                // table here so an unevaluable key is refused at the DDL.
                let mut cols = Vec::with_capacity(columns.len());
                for (i, cname) in columns.iter().enumerate() {
                    match exprs.get(i).and_then(Option::as_ref) {
                        None => cols.push(crate::ddl_apply::resolve_index_columns(
                            t,
                            &table,
                            std::slice::from_ref(cname),
                        )?[0]),
                        Some(src) => {
                            mpedb_sql::compile_value_expr(src, t).map_err(|e| {
                                Error::Schema(format!(
                                    "CREATE INDEX on `{table}`: key expression `{src}` does \
                                     not compile against the table: {e}"
                                ))
                            })?;
                            cols.push(mpedb_types::INDEX_EXPR_COL);
                        }
                    }
                }
                // Identified by NAME, not by shape — the same rule the
                // autocommit applier uses, and it has to be here too because
                // this is the path Django's migrations actually take: they run
                // under `atomic=True`, so every `CREATE INDEX` in a migration
                // arrives in a session. Shape-idempotence reported success and
                // created nothing whenever an index of that shape existed, so
                // the migration's own `DROP INDEX` then failed with "no such
                // index". Every name in this arm is explicit — the parser gives
                // `name: String` — so there is no unnamed case to fall back to.
                if t.indexes.iter().any(|ix| {
                    ix.name.as_deref().is_some_and(|e| mpedb_types::ident_eq(e, &name))
                }) {
                    if if_not_exists {
                        return Ok(ExecResult::Affected(0));
                    }
                    return Err(Error::Bind(format!("index `{name}` already exists")));
                }
                self.txn
                    .create_index(id, cols, exprs, collations, unique, where_clause, Some(name))?;
            }
            // VIEW / TRIGGER ride this session's txn (sys-keyspace + schema_gen
            // bump) so CPython's implicit transaction + iterdump can CREATE them
            // before COMMIT. POLICY / RLS still open their own catalog path and
            // stay out-of-txn for now.
            DdlStmt::CreateView {
                name,
                select_sql,
                if_not_exists,
            } => {
                self.create_view_in_txn(&name, &select_sql, if_not_exists)?;
            }
            // Compiled and written INSIDE this transaction, the same shape
            // CREATE VIEW takes above: two sys records (the content-addressed
            // IR blob and the name binding) plus a schema-generation bump, so
            // the function appears exactly when the transaction commits and not
            // before. Routing it to `Database::create_function` instead would
            // take the writer lock a second time from inside a write session —
            // a self-deadlock, and the whole reason views have an in-txn twin.
            DdlStmt::CreateFunction { source, .. } => {
                let r = crate::spellfn::compile_function_records(
                    crate::spellfn::SpellLang::PlPgSql,
                    &source,
                )?;
                self.txn.sys_put(&r.blob_key, &r.blob)?;
                self.txn.sys_put(&r.name_key, &r.record)?;
                self.txn.bump_schema_gen();
            }
            // The twin, in the same transaction for the same reason: only the
            // NAME binding is removed. The content-addressed blob stays, and
            // that is deliberate — another name, or a registered plan carrying
            // this function's hash, may still reference it, and
            // content-addressed storage never breaks a pinned reference.
            DdlStmt::DropFunction { name, if_exists } => {
                let key = crate::sys_record_subkey(crate::spellfn::NS_FUNC, name.as_bytes())?;
                let existed = self.txn.sys_delete(&key)?;
                if !existed && !if_exists {
                    return Err(Error::Bind(format!(
                        "DROP FUNCTION: no stored function named `{name}`"
                    )));
                }
                if existed {
                    self.txn.bump_schema_gen();
                }
            }
            // DROP INDEX frees a B-tree and renumbers the catalog roots above
            // it — engine work that the in-session DDL path does not carry, so
            // it takes the out-of-txn catalog path like CREATE INDEX above it.
            DdlStmt::DropIndex { name, if_exists } => {
                let found = self.txn.schema_bundle().schema.find_index_by_name(&name);
                match found {
                    Some((table_id, pos)) => self.txn.drop_index(table_id, pos)?,
                    None if if_exists => {}
                    None => {
                        return Err(Error::Bind(format!("DROP INDEX: no such index `{name}`")))
                    }
                }
            }
            DdlStmt::DropView { name, if_exists } => {
                self.drop_view_in_txn(&name, if_exists)?;
            }
            DdlStmt::CreateTrigger(spec) => {
                self.create_trigger_in_txn(spec)?;
            }
            DdlStmt::DropTrigger { name, if_exists } => {
                self.drop_trigger_in_txn(&name, if_exists)?;
            }
            DdlStmt::CreatePolicy(_)
            | DdlStmt::DropPolicy { .. }
            | DdlStmt::AlterRls { .. } => {
                return Err(Error::Unsupported(
                    "CREATE/DROP POLICY and ALTER … ROW LEVEL SECURITY are not \
                     supported inside a transaction; run them in autocommit \
                     (outside BEGIN/COMMIT)"
                        .into(),
                ));
            }
            // Handled before the savepoint in `apply_ddl`; unreachable here.
            DdlStmt::Analyze { .. } | DdlStmt::Reindex { .. } => {}
        }
        Ok(ExecResult::Affected(0))
    }

    /// `CREATE VIEW` on this session's open write txn (no nested commit).
    pub(super) fn create_view_in_txn(
        &mut self,
        name: &str,
        select_sql: &str,
        if_not_exists: bool,
    ) -> Result<()> {
        let schema = self.txn.schema_bundle();
        if schema.schema.table_id(name).is_some() {
            return Err(Error::Bind(format!(
                "CREATE VIEW: `{name}` is already a table"
            )));
        }
        let key = crate::ddl_apply::view_key_public(name);
        if crate::ddl_apply::view_exists_on_txn(&mut self.txn, name)? {
            if if_not_exists {
                return Ok(());
            }
            return Err(Error::Bind(format!(
                "CREATE VIEW: view `{name}` already exists"
            )));
        }
        self.txn.sys_put(&key, select_sql.as_bytes())?;
        self.txn.bump_schema_gen();
        Ok(())
    }

    pub(super) fn drop_view_in_txn(&mut self, name: &str, if_exists: bool) -> Result<()> {
        let key = match crate::ddl_apply::resolve_view_key_on_txn(&mut self.txn, name)? {
            Some(k) => k,
            None => {
                if if_exists {
                    return Ok(());
                }
                return Err(Error::Bind(format!("DROP VIEW: no such view `{name}`")));
            }
        };
        self.txn.sys_delete(&key)?;
        self.txn.bump_schema_gen();
        Ok(())
    }

    pub(super) fn create_trigger_in_txn(&mut self, spec: mpedb_sql::CreateTriggerSpec) -> Result<()> {
        crate::trigger::create_trigger_on_txn(&mut self.txn, self.db, spec)
    }

    pub(super) fn drop_trigger_in_txn(&mut self, name: &str, if_exists: bool) -> Result<()> {
        crate::trigger::drop_trigger_on_txn(&mut self.txn, name, if_exists)
    }
}
