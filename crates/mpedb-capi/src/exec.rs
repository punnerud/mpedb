use super::*;

// ===========================================================================
// Transaction control + statement execution (shared by step and exec).
// ===========================================================================

fn begin_txn(c: &mut Sqlite3) -> Result<(), DbError> {
    if c.txn.is_some() {
        return Err(DbError::Unsupported(
            "cannot start a transaction within a transaction".into(),
        ));
    }
    // The session borrows `c.db`, which lives at a stable heap address (the
    // Sqlite3 is boxed and never moved) and is declared after `txn`, so the
    // borrow is always dropped before its referent. Same discipline as
    // mpedb-py's PyTransaction.
    let db_ptr: *const Database = &c.db;
    let session = unsafe { (*db_ptr).begin()? };
    let session: WriteSession<'static> =
        unsafe { std::mem::transmute::<WriteSession<'_>, WriteSession<'static>>(session) };
    c.txn = Some(session);
    Ok(())
}

fn commit_txn(c: &mut Sqlite3) -> Result<(), DbError> {
    match c.txn.take() {
        Some(s) => s.commit(),
        None => Ok(()), // lenient: COMMIT with no active transaction is a no-op
    }
}

fn rollback_txn(c: &mut Sqlite3) {
    if let Some(s) = c.txn.take() {
        s.rollback();
    }
}

/// sqlite 3.15+: a partial-index WHERE may not name a non-deterministic
/// function. CPython's `test_func_non_deterministic` asserts the refusal;
/// `test_func_deterministic` asserts the allow path.
pub(super) fn nondeterministic_in_partial_index(c: &Sqlite3, sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    // Cheap shape gate: CREATE … INDEX … WHERE
    if !(lower.contains("create") && lower.contains("index") && lower.contains("where")) {
        return None;
    }
    // Host UDFs registered without SQLITE_DETERMINISTIC.
    for h in &c.host_fns {
        if h.aggregate || h.deterministic {
            continue;
        }
        let needle = format!("{}(", h.name);
        if lower.contains(&needle) {
            return Some(format!(
                "non-deterministic functions prohibited in index expressions: `{}`",
                h.name
            ));
        }
    }
    // Built-ins sqlite also refuses in index expressions.
    for name in ["random", "randomblob"] {
        let needle = format!("{name}(");
        if lower.contains(&needle) {
            return Some(format!(
                "non-deterministic functions prohibited in index expressions: `{name}`"
            ));
        }
    }
    None
}

/// Run one statement against the connection, honoring the current transaction
/// state. Transaction-control statements are intercepted; everything else is
/// routed to the open `WriteSession` (if any) or the autocommit facade.
pub(super) fn exec_one(c: &mut Sqlite3, sqltext: &str, params: &[Value]) -> Result<Outcome, DbError> {
    if let Some(msg) = nondeterministic_in_partial_index(c, sqltext) {
        return Err(DbError::Bind(msg));
    }
    // `INSERT OR ROLLBACK` is the one conflict action a statement cannot carry
    // out on its own — it aborts the enclosing TRANSACTION, and the connection
    // is what owns that. mpedb's parser refuses it by name; the shim runs it as
    // `OR ABORT` and rolls the connection back itself when the conflict fires
    // (sqlite's exact definition of the action). See `sql::rewrite_insert_or_rollback`.
    let (or_rollback_sql, or_rollback) = sql::rewrite_insert_or_rollback(sqltext);
    if or_rollback {
        let res = exec_one_inner(c, &or_rollback_sql, params);
        if let Err(e) = &res {
            if valconv::error_codes(e).0 == SQLITE_CONSTRAINT {
                rollback_txn(c);
            }
        }
        return res;
    }
    // Per-constraint `ON CONFLICT ROLLBACK` on CREATE TABLE: same ownership
    // story. Rewrite the clause to ABORT for the engine, remember the table,
    // and on a later UNIQUE failure against it roll the transaction back.
    let (create_sql, had_oc_rb) = sql::rewrite_on_conflict_rollback(sqltext);
    let create_name = if had_oc_rb {
        sql::create_table_name(&create_sql)
    } else {
        None
    };
    let res = if had_oc_rb {
        exec_one_inner(c, &create_sql, params)
    } else {
        exec_one_inner(c, sqltext, params)
    };
    match &res {
        Ok(_) => {
            if let Some(name) = create_name {
                // Keys are folded: identifier case is insignificant (E13).
                c.unique_rollback_tables
                    .insert(name.to_ascii_lowercase());
            }
        }
        Err(e) if valconv::error_codes(e).0 == SQLITE_CONSTRAINT => {
            if !c.unique_rollback_tables.is_empty() {
                // Engine wording is `UNIQUE violation: <table> (…)` or a PK
                // form; match any recorded table name as a whole word so a
                // short name cannot spuriously hit a longer one.
                let msg = format!("{e}");
                let hit = c.unique_rollback_tables.iter().any(|t| {
                    msg.to_ascii_lowercase()
                        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                        .any(|w| w == t.as_str())
                });
                if hit {
                    rollback_txn(c);
                }
            }
        }
        Err(_) => {}
    }
    res
}

fn exec_one_inner(c: &mut Sqlite3, sqltext: &str, params: &[Value]) -> Result<Outcome, DbError> {
    use sql::Kind;
    // sqlite's parser skips leading comments; mpedb's does not — strip them
    // here so `-- comment\nINSERT …` (a shape CPython's suite and iterdump
    // scripts use) reaches the engine as the statement it is.
    let sqltext = sql::strip_leading_trivia(sqltext);
    // `zeroblob(<const>)` → the byte-identical blob literal, so it is accepted
    // in INSERT-values position where mpedb refuses a function call (blob.rs).
    // Idempotent: the step path already rewrote at prepare, leaving no call.
    let rewritten_zb = sql::rewrite_zeroblob(sqltext);
    let sqltext: &str = &rewritten_zb;
    // `EXPLAIN QUERY PLAN <stmt>` → mpedb's own `EXPLAIN <stmt>`, reshaped by
    // `eqp_outcome` below. The rewrite happens here rather than at prepare so
    // `sqlite3_sql()` still reports the text the consumer wrote.
    let eqp_rewritten;
    let (sqltext, eqp) = match sql::explain_query_plan_body(sqltext) {
        Some(body) => {
            eqp_rewritten = format!("EXPLAIN {body}");
            (eqp_rewritten.as_str(), true)
        }
        None => (sqltext, false),
    };
    // `sqlite_sequence` WRITES (plan §4 step 2): stock keeps the counters in
    // an ordinary table, so UPDATE/DELETE/INSERT against it all work — Django's
    // flush reset (`UPDATE sqlite_sequence SET seq = 0 WHERE name IN (…)`)
    // RAISED "no such table" here before this arm. The consumer forms map onto
    // the catalog's AUTOINCREMENT counters, every name in the ONE transaction
    // (the open one when present — a flush resets inside the txn that holds
    // its deletes); junk/duplicate/rename shapes refuse BY NAME (the matrix in
    // introspect::sqlite_sequence_write's contract). Detection is
    // TARGET-position only, for the read detector's measured Django reason:
    // the table's name appears as a string LITERAL in catalog queries.
    if !eqp && introspect::sqlite_sequence_write_target(sqltext) {
        if c.txn.is_none() {
            let _ = c.db.refresh_schema_if_stale();
        }
        let bundle = match c.txn.as_ref() {
            Some(s) => s.schema(),
            None => c.db.schema(),
        };
        // The table exists the moment an AUTOINCREMENT table does — the same
        // rule as the read arm, so a write can never target a table the
        // listing denies.
        if !bundle.schema.tables.iter().any(|t| !t.dead && t.autoincrement) {
            return Err(DbError::Bind("no such table: sqlite_sequence".into()));
        }
        let seqs = match c.txn.as_mut() {
            Some(s) => s.rowid_sequences()?,
            None => c.db.rowid_sequences()?,
        };
        let plan = introspect::sqlite_sequence_write(&seqs, sqltext, params)?;
        // Names resolve BYTE-EXACT: stock compares `sqlite_sequence.name` as
        // ordinary data, so 'T1' does not touch table t1 — and every name in
        // `plan.updates` came from the synthesised rows, so it resolves.
        let mut updates: Vec<(u32, Option<i64>)> = Vec::new();
        for (name, v) in &plan.updates {
            if let Some(t) = bundle
                .schema
                .tables
                .iter()
                .find(|t| !t.dead && t.autoincrement && t.name == *name)
            {
                updates.push((t.id, *v));
            }
        }
        if let Some((name, seq)) = &plan.insert_new {
            match bundle.schema.tables.iter().find(|t| !t.dead && t.name == *name) {
                Some(t) if t.autoincrement => updates.push((t.id, Some(*seq))),
                Some(t) => {
                    return Err(DbError::Unsupported(format!(
                        "INSERT INTO sqlite_sequence for `{}` is refused by name: the \
                         table is not AUTOINCREMENT, and mpedb's catalog-backed counters \
                         have nowhere to keep the inert row sqlite would store and never \
                         read",
                        t.name
                    )))
                }
                None => {
                    return Err(DbError::Unsupported(format!(
                        "INSERT INTO sqlite_sequence for unknown table `{name}` is \
                         refused by name: sqlite stores an orphan row; mpedb's \
                         catalog-backed counters cannot"
                    )))
                }
            }
        }
        match c.txn.as_mut() {
            Some(s) => s.set_rowid_sequences(&updates)?,
            None => c.db.set_rowid_sequences(&updates)?,
        }
        return Ok(Outcome::Affected(plan.affected as u64));
    }
    match sql::classify(sqltext) {
        // PRAGMA and sqlite_master reads are answered by the shim's schema
        // introspection (mpedb has neither); they never reach the engine.
        Kind::Pragma => {
            // #51: `PRAGMA database_list` needs the connection's attach list,
            // which introspect (schema-only) cannot see — answer it here.
            // Shape derived from sqlite (probe P9): seq 0 = main (path, or ''
            // for an in-memory database), attached start at seq 2 (1 is
            // temp's reserved slot, which mpedb does not have).
            if introspect::parse_pragma(sqltext)
                .0
                .eq_ignore_ascii_case("database_list")
            {
                let main_file = match c.backing {
                    Backing::File => c.path.to_string_lossy().into_owned(),
                    _ => String::new(),
                };
                let mut rows = vec![vec![
                    Value::Int(0),
                    Value::Text("main".into()),
                    Value::Text(main_file),
                ]];
                for (i, (name, path)) in c.db.attached_databases().into_iter().enumerate() {
                    rows.push(vec![
                        Value::Int(i as i64 + 2),
                        Value::Text(name),
                        Value::Text(path.to_string_lossy().into_owned()),
                    ]);
                }
                return Ok(Outcome::Rows {
                    columns: vec!["seq".into(), "name".into(), "file".into()],
                    rows,
                });
            }
            // Prefer the open WriteSession's schema so a mid-transaction
            // `CREATE TABLE` is visible to `PRAGMA table_info` (CPython's
            // default isolation starts a txn on the first INSERT, then
            // `iterdump` runs `table_info` before COMMIT — without this the
            // dump emits `VALUES()` with no columns). Outside a txn, refresh
            // so a just-committed CREATE is not stale.
            // A pragma qualified with an ATTACHED database answers about that
            // database. SQLAlchemy's reflection suite attaches a second file as
            // `test_schema` and reflects through it, so answering from main
            // instead reported the wrong tables for 264 of its tests.
            //
            // A qualifier naming nothing attached yields an EMPTY schema rather
            // than main's: "that database has no such table" is the truthful
            // answer, and silently substituting main is how a reflection tool
            // ends up describing the wrong file.
            let mut qualifier = introspect::pragma_schema(sqltext)
                .filter(|q| !q.eq_ignore_ascii_case("main"));
            // An UNQUALIFIED pragma name resolves against temp first, exactly
            // as a bare table reference does (measured: with the object only in
            // temp, `PRAGMA table_info(x)` answers it; sqlite has no separate
            // rule for pragmas). Without this a temp table or view answered
            // only when the caller wrote `temp.` — and SQLAlchemy's
            // `get_multi_*` reflection never does.
            if qualifier.is_none() && introspect::pragma_schema(sqltext).is_none() {
                if let (_, Some(arg)) = introspect::parse_pragma(sqltext) {
                    if names_temp_object(c, &arg) {
                        qualifier = Some("temp".to_string());
                    }
                }
            }
            let bundle = match qualifier.as_deref() {
                Some(q) => c.db.attached_schema_or_empty(q),
                None => match c.txn.as_ref() {
                    Some(s) => s.schema(),
                    None => {
                        let _ = c.db.refresh_schema_if_stale();
                        c.db.schema()
                    }
                },
            };
            // `PRAGMA foreign_keys [= ON|OFF]` and `PRAGMA foreign_key_check`
            // (#194) are answered HERE rather than in `introspect`: the setter
            // must know whether a transaction is open (sqlite makes it a silent
            // no-op inside one — measured, 3.45.1), and the check must run a
            // read transaction. Everything else the pragma handler answers is a
            // pure function of the schema.
            if let Some((columns, rows)) = fk_pragma(c, sqltext)? {
                return Ok(Outcome::Rows { columns, rows });
            }
            // `PRAGMA wal_checkpoint` on a sqlite-backed database writes the
            // sidecar back over its source file. A whole-image write, so it is
            // asked for explicitly and never implied.
            if let Some((columns, rows)) = crate::filesystem_pragma(c, sqltext) {
                return Ok(Outcome::Rows { columns, rows });
            }
            match checkpoint_pragma(c, sqltext) {
                Ok(Some((columns, rows))) => return Ok(Outcome::Rows { columns, rows }),
                Ok(None) => {}
                Err(msg) => return Err(DbError::Unsupported(msg)),
            }
            // A VIEW answers `table_info` too — with its RESULT columns, which
            // is how SQLAlchemy's `get_multi_*` reflection discovers that a
            // view has columns at all. Returning nothing for one silently
            // dropped every view from those results. It is answered here and
            // not in `introspect` because the columns come from COMPILING the
            // view's SELECT, and that needs the connection.
            if let Some((columns, rows)) = view_table_info(c, &bundle, sqltext, qualifier.as_deref())
            {
                return Ok(Outcome::Rows { columns, rows });
            }
            let idx = sqlite_index_records_of(c, qualifier.as_deref());
            let fk_on = c.db.fk_enforced();
            let (columns, rows) =
                introspect::pragma(
                    &bundle,
                    sqltext,
                    &mut c.busy_timeout_ms,
                    &fk_on,
                    &idx,
                    &mut c.echo_pragmas,
                )?;
            // `PRAGMA busy_timeout = N` may have moved the knob — mirror it
            // into the engine's writer-lock deadline (#109), same as
            // `sqlite3_busy_timeout`. Unconditional: an atomic store, cheap.
            c.db.set_busy_timeout(Some(Duration::from_millis(c.busy_timeout_ms.max(0) as u64)));
            Ok(Outcome::Rows { columns, rows })
        }
        // `EXPLAIN QUERY PLAN SELECT … FROM sqlite_master` is excluded: the
        // mini-evaluator answers ROWS, not a plan, and mpedb has no such table
        // to plan against — it refuses by name instead of answering the wrong
        // shape.
        Kind::Read if !eqp && introspect::references_sqlite_master(sqltext) => {
            // Prefer the open txn's schema so uncommitted CREATE TABLE/VIEW/
            // TRIGGER from this session appear in iterdump mid-transaction.
            // Outside a txn, refresh first: a just-committed CREATE bumps
            // schema_gen, and `Database::schema()` alone does not reload.
            // `sqlite_temp_master` lists the TEMP schema and nothing else, so
            // it reads the temp member's own catalog. A connection that never
            // made a temp table has no such member, and the honest answer there
            // is an EMPTY catalog — not an error, and not main's contents.
            // `<schema>.sqlite_master` reads THAT database's catalog — how
            // SQLAlchemy lists an attached schema's views.
            // `sqlite_sequence` is synthesised from the catalog's AUTOINCREMENT
            // counters, the way `sqlite_master` is synthesised from the schema.
            // Answered before the master arms because it is a different table
            // with its own columns.
            if introspect::references_sqlite_sequence(sqltext) {
                // The table exists the moment an AUTOINCREMENT table does —
                // the SAME rule that lists it in `sqlite_master` below, so a
                // consumer can never read a table the listing denies or vice
                // versa. Before any such table: sqlite's exact refusal
                // (measured — a fresh database says `no such table`, an empty
                // one with the keyword answers zero rows). Stock keeps the
                // table after the LAST autoincrement table is dropped; the
                // schema records only live tables, so that one form stays a
                // named divergence.
                if c.txn.is_none() {
                    let _ = c.db.refresh_schema_if_stale();
                }
                let has_autoinc = {
                    let bundle = match c.txn.as_ref() {
                        Some(s) => s.schema(),
                        None => c.db.schema(),
                    };
                    bundle.schema.tables.iter().any(|t| !t.dead && t.autoincrement)
                };
                if !has_autoinc {
                    return Err(DbError::Bind("no such table: sqlite_sequence".into()));
                }
                // Through the open txn when there is one: a fresh read
                // snapshot cannot see ids the txn has just handed out (the
                // FK pragma needed the same cure).
                let seqs = match c.txn.as_mut() {
                    Some(s) => s.rowid_sequences()?,
                    None => c.db.rowid_sequences()?,
                };
                let (columns, rows) =
                    introspect::sqlite_sequence_query(&seqs, sqltext, params)?;
                return Ok(Outcome::Rows { columns, rows });
            }
            let master_q_name = introspect::master_schema(sqltext)
                .filter(|q| !q.eq_ignore_ascii_case("main"));
            // `<schema>.sqlite_temp_master` does not exist — the temp schema is
            // the connection's own and cannot be qualified with another name.
            // sqlite errors, and a consumer may DEPEND on that error rather
            // than on an empty result (see `qualified_temp_master`).
            if let Some(q) = introspect::qualified_temp_master(sqltext) {
                return Err(DbError::Bind(format!("no such table: {q}.sqlite_temp_master")));
            }
            let reference = introspect::master_reference(sqltext);
            // Which catalog(s) supply rows. A statement naming both reads main
            // first and temp second, as its `UNION ALL` writes them.
            let members: Vec<Option<&str>> = match reference {
                Some(introspect::MasterRef::Temp) => vec![Some("temp")],
                Some(introspect::MasterRef::Both) => vec![None, Some("temp")],
                _ => vec![master_q_name.as_deref()],
            };
            let _ = c.db.refresh_schema_if_stale();
            let mut parts = Vec::with_capacity(members.len());
            for member in members {
                let bundle = match member {
                    Some(m) => c.db.attached_schema_or_empty(m),
                    None => match c.txn.as_ref() {
                        Some(s) => s.schema(),
                        None => c.db.schema(),
                    },
                };
                // The verbatim text comes from the SAME schema the rows do —
                // `record_member` files it there. Reading main for every schema
                // could not work: two schemas may hold a table of one name, and
                // one name-keyed record cannot hold two texts.
                let mut verbatim = sqlite_master_records_of(c, member);
                let idx = sqlite_index_records_of(c, member);
                let (views, triggers) = if member == Some("temp") {
                    // A temp view is connection-local text, not a member
                    // object, so it comes from the handle rather than from any
                    // catalog — and main's views must NOT appear under
                    // `sqlite_temp_master`.
                    let mut views = Vec::new();
                    for (name, select_sql, text) in c.db.temp_views_all() {
                        verbatim.insert(name.clone(), introspect::object_ddl_record(&text));
                        views.push((name, select_sql));
                    }
                    (views, Vec::new())
                } else if let Some(m) = member {
                    // An attached member's views are objects of THAT schema.
                    // Reading main's catalog here reported main's views under
                    // every attached schema — a wrong answer, and it hid the
                    // fact that a qualified `CREATE VIEW` was not landing
                    // anywhere.
                    (c.db.attached_list_views(m), c.db.attached_list_triggers(m))
                } else {
                    // Prefer txn-visible catalog (uncommitted CREATE VIEW/TRIGGER).
                    match c.txn.as_mut() {
                        Some(s) => (
                            s.list_views().unwrap_or_default(),
                            s.list_triggers().unwrap_or_default(),
                        ),
                        None => (
                            c.db.list_views().unwrap_or_default(),
                            c.db.list_triggers().unwrap_or_default(),
                        ),
                    }
                };
                parts.push((bundle, verbatim, idx, views, triggers));
            }
            let sources: Vec<introspect::MasterSource> = parts
                .iter()
                .map(|(bundle, verbatim, idx, views, triggers)| introspect::MasterSource {
                    schema: bundle,
                    verbatim,
                    idx,
                    views,
                    triggers,
                })
                .collect();
            let (columns, rows) = introspect::sqlite_master(&sources, sqltext, params)?;
            Ok(Outcome::Rows { columns, rows })
        }
        Kind::Begin => {
            begin_txn(c)?;
            Ok(Outcome::Control)
        }
        Kind::Commit => {
            commit_txn(c)?;
            Ok(Outcome::Control)
        }
        Kind::Rollback => {
            rollback_txn(c);
            Ok(Outcome::Control)
        }
        Kind::Savepoint => {
            if c.txn.is_none() {
                begin_txn(c)?;
            }
            c.txn.as_mut().unwrap().query(sqltext, params)?;
            Ok(Outcome::Control)
        }
        Kind::Release | Kind::RollbackTo => {
            let Some(s) = c.txn.as_mut() else {
                return Err(DbError::Unsupported(
                    "no active transaction for this savepoint operation".into(),
                ));
            };
            s.query(sqltext, params)?;
            Ok(Outcome::Control)
        }
        // VACUUM / ANALYZE: nothing to do (see `Kind::Maintenance`) — succeed
        // with no rows and no change counters, as sqlite's do on a tidy file.
        Kind::Maintenance => Ok(Outcome::Control),
        // DDL (CREATE/DROP/ALTER) routes like any other statement (#95): to the
        // open WriteSession's txn when one is active — where it commits/rolls
        // back atomically with the transaction's DML — else to the autocommit
        // facade. Python's sqlite3 opens an implicit transaction on the first
        // DML, so a `CREATE TABLE` after an `INSERT` (and every `executescript`)
        // lands here with `c.txn` set.
        _ => {
            // A `CREATE`/`DROP` TABLE/VIEW/TRIGGER's own text, filed under the
            // object's name once it succeeds (`sqlite_master.sql`). Resolved
            // BEFORE execution because a DROP's target is gone afterwards.
            let ddl_target = introspect::schema_ddl_target(sqltext);
            // For a `CREATE INDEX`, how many indexes the target table had
            // BEFORE the statement ran — the only way to tell afterwards which
            // `IndexDef` it appended, since mpedb's schema does not name them.
            let idx_before = ddl_target.as_ref().and_then(|d| match d {
                introspect::DdlTarget {
                    kind: introspect::DdlKind::Index { .. },
                    create: true,
                    on_table: Some(t),
                    ..
                } => table_index_count(c, record_member(c, sqltext, d).as_deref(), t),
                _ => None,
            });
            let res = if let Some(s) = c.txn.as_mut() {
                s.query(sqltext, params)
            } else {
                c.db.query(sqltext, params)
            };
            if res.is_ok() {
                if let Some(t) = &ddl_target {
                    record_object_ddl(c, sqltext, t, idx_before);
                }
                if let Some((sch, old, new)) = introspect::alter_rename_target(sqltext) {
                    move_table_ddl_record(c, sch.as_deref(), &old, &new);
                }
            }
            // Drain the rowid the engine recorded for this statement (facade
            // hook) BEFORE propagating any error, so sqlite3_last_insert_rowid
            // reflects the last row an INSERT actually wrote — even when a
            // later row of the same statement failed — and a stale value can
            // never bleed into a subsequent statement. `take_*` clears the
            // thread-local; a non-insert returns None and leaves the
            // connection's value unchanged, exactly as sqlite does.
            if let Some(rowid) = mpedb::take_last_insert_rowid() {
                c.last_insert_rowid = rowid;
            }
            let res = res?;
            Ok(if eqp { eqp_outcome(res) } else { to_outcome(res) })
        }
    }
}

/// The verbatim-DDL records for every catalog object `sqlite_master` may
/// report (tables, views, triggers), keyed by object name.
///
/// Read through the OPEN transaction when there is one, so records written in
/// it are visible to it; through the facade otherwise. A missing or unreadable
/// record is simply absent — `sqlite_master` then falls back to reconstructing
/// the `CREATE …` text, which is what it did before any of this existed.
///
/// ONE scan, not one lookup per object: a consumer with hundreds of tables
/// (Django's suite has them) queries `sqlite_master` repeatedly, and a read
/// transaction per object per query is quadratic for no reason. `0xff` bounds
/// the scan above every key: keys are object names, and no valid UTF-8 byte
/// sequence starts with `0xff`.
/// The verbatim-DDL records of one schema: `None` for main (this connection's
/// own, txn-visible), or an attached database by name.
fn sqlite_master_records_of(c: &mut Sqlite3, member: Option<&str>) -> HashMap<String, Vec<u8>> {
    let ns = introspect::DDL_NS;
    if let Some(m) = member {
        return c
            .db
            .attached_sys_record_scan(m, ns)
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .filter_map(|(k, v)| String::from_utf8(k).ok().map(|k| (k, v)))
            .collect();
    }
    let all = match c.txn.as_mut() {
        Some(s) => s.sys_record_scan_range(ns, &[], &[0xff]).unwrap_or_default(),
        None => c.db.sys_record_scan(ns).unwrap_or_default(),
    };
    // Keep every non-empty record. Stale keys (dropped objects) are harmless:
    // only names that still appear in the live catalog are consulted.
    all.into_iter()
        .filter_map(|(k, v)| {
            if v.is_empty() {
                return None;
            }
            String::from_utf8(k).ok().map(|k| (k, v))
        })
        .collect()
}

/// The shim's `CREATE INDEX` records, folded into the shape-keyed map
/// `introspect` resolves an `IndexDef`'s name through (`introspect::IDX_NS`).
///
/// Same one-scan discipline as [`sqlite_master_records`], and the same
/// transaction rule: through the OPEN session when there is one, so an index
/// created in an uncommitted transaction can still be named.
/// The index records of one schema — `None` for main (this connection's own,
/// txn-visible), or an attached member by name. Scoped for the same reason the
/// DDL records are: two schemas may hold an index of one name.
fn sqlite_index_records_of(c: &mut Sqlite3, member: Option<&str>) -> introspect::IndexRecords {
    let ns = introspect::IDX_NS;
    let all = match member {
        Some(m) => c.db.attached_sys_record_scan(m, ns),
        None => match c.txn.as_mut() {
            Some(s) => s.sys_record_scan_range(ns, &[], &[0xff]).unwrap_or_default(),
            None => c.db.sys_record_scan(ns).unwrap_or_default(),
        },
    };
    introspect::index_records(all)
}

/// How many secondary indexes `table` has right now — the "before" half of the
/// `CREATE INDEX` fingerprint capture (see [`record_object_ddl`]).
fn table_index_count(c: &Sqlite3, member: Option<&str>, table: &str) -> Option<usize> {
    let schema = match member {
        Some(m) => c.db.attached_schema_or_empty(m),
        None => match c.txn.as_ref() {
            Some(s) => s.schema(),
            None => c.db.schema(),
        },
    };
    let exact = introspect::exact_table_name(&schema, table)?;
    introspect::table_by_exact_name(&schema, &exact).map(|t| t.indexes.len())
}

/// Does `name` name an object in the connection's TEMP schema — a temp table
/// or a temp view? The bare-name shadowing test for pragmas.
fn names_temp_object(c: &Sqlite3, name: &str) -> bool {
    let in_view = c
        .db
        .temp_views_all()
        .iter()
        .any(|(n, _, _)| n.eq_ignore_ascii_case(name));
    // INDEXES count: `PRAGMA index_info(<a temp index>)` has to read the temp
    // member's records, and an index is an object of the schema its table is
    // in like any other.
    let in_index = c
        .db
        .attached_sys_record_scan("temp", introspect::IDX_NS)
        .iter()
        .any(|(k, v)| {
            !v.is_empty()
                && std::str::from_utf8(k).is_ok_and(|n| n.eq_ignore_ascii_case(name))
        });
    in_view || in_index || introspect::names_a_table(&c.db.temp_schema_or_empty(), name)
}

/// `PRAGMA [<schema>.]table_info(<view>)` — a view's RESULT columns.
///
/// sqlite reports the same six columns it does for a table, with `notnull`,
/// `dflt_value` and `pk` all empty: a view has no storage, so it has no
/// constraints of its own even when the column it selects is a NOT NULL primary
/// key (measured against 3.45.1 — `CREATE VIEW v AS SELECT * FROM t` over an
/// `INTEGER PRIMARY KEY` reports `pk` 0).
///
/// `None` means "not a view" and the ordinary pragma handler takes it. Only
/// `table_info`/`table_xinfo` are answered — `index_list` on a view is empty in
/// sqlite too, which the ordinary handler already returns.
fn view_table_info(
    c: &mut Sqlite3,
    bundle: &mpedb::Schema,
    sqltext: &str,
    qualifier: Option<&str>,
) -> Option<(Vec<String>, Vec<Vec<Value>>)> {
    let (name, arg) = introspect::parse_pragma(sqltext);
    let xinfo = match name.to_ascii_lowercase().as_str() {
        "table_info" => false,
        "table_xinfo" => true,
        _ => return None,
    };
    let arg = arg?;
    // A table of that name wins: `find_table` is what the ordinary handler
    // consults, so asking it here keeps the two from disagreeing.
    if introspect::names_a_table(bundle, &arg) {
        return None;
    }
    let temp = qualifier.is_some_and(|q| q.eq_ignore_ascii_case("temp"));
    // A temp view is connection-local text, not an object of any member, so it
    // is looked up on the handle — and its BODY is what gets probed. Probing
    // `SELECT * FROM v` instead would go through the routing inliner, which
    // wraps the body in a derived table, and a derived table has no base column
    // for `output_decltypes` to report: every column came back with an empty
    // type where sqlite reports the underlying one. mpedb refuses `CREATE VIEW
    // v(a, b)`, so the body's output names ARE the view's column names.
    let mut temp_body = None;
    let exact = match qualifier {
        Some(_) if temp => {
            let (n, body, _) = c
                .db
                .temp_views_all()
                .into_iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(&arg))?;
            temp_body = Some(body);
            n
        }
        Some(q) => c
            .db
            .attached_list_views(q)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&arg))
            .map(|(n, _)| n)?,
        None => resolve_object_name(c, introspect::DdlKind::View, &arg)?,
    };
    // Qualified through the same spelling the caller used, so an attached
    // schema's view is compiled against that member.
    // The probe runs UNQUALIFIED on whichever schema owns the view — main
    // here, or the member itself. It cannot be qualified and run on main: a
    // read against an attached member takes the cross-file path, which resolves
    // that member's TABLES and has no view of its own to expand.
    let probe = match temp_body {
        Some(body) => body,
        None => format!("SELECT * FROM \"{}\" LIMIT 0", exact.replace('"', "\"\"")),
    };
    let (cols, decl) = match qualifier {
        Some(_) if temp => {
            let cols = match c.db.query(&probe, &[]) {
                Ok(mpedb::ExecResult::Rows { columns, .. }) => columns,
                _ => return None,
            };
            (cols, c.db.routed_output_decltypes(&probe))
        }
        Some(q) => c.db.attached_probe_columns(q, &probe)?,
        None => {
            let cols = match c.db.query(&probe, &[]) {
                Ok(mpedb::ExecResult::Rows { columns, .. }) => columns,
                _ => return None,
            };
            (cols, c.db.output_decltypes(&probe).unwrap_or_default())
        }
    };
    let mut names: Vec<&str> = vec!["cid", "name", "type", "notnull", "dflt_value", "pk"];
    if xinfo {
        names.push("hidden");
    }
    let rows = cols
        .into_iter()
        .enumerate()
        .map(|(i, col)| {
            let ty = decl.get(i).cloned().flatten().unwrap_or_default();
            let mut row = vec![
                Value::Int(i as i64),
                Value::Text(col),
                Value::Text(ty),
                Value::Int(0),
                Value::Null,
                Value::Int(0),
            ];
            if xinfo {
                row.push(Value::Int(0));
            }
            row
        })
        .collect();
    Some((names.into_iter().map(str::to_string).collect(), rows))
}

/// Which attached member holds this statement's object, or `None` for main.
///
/// `CREATE TEMP …` names no schema but is not main either, so the temp rewrite
/// is what decides that case — the same function the router uses, so the record
/// cannot disagree with where the object actually went.
fn record_member(c: &Sqlite3, sqltext: &str, target: &introspect::DdlTarget) -> Option<String> {
    match target.schema.as_deref() {
        Some(q) if q.eq_ignore_ascii_case("main") => None,
        Some(q) => Some(q.to_string()),
        None => {
            if mpedb::rewrite_temp_ddl(sqltext).ok().flatten().is_some() {
                return Some("temp".to_string());
            }
            // `CREATE INDEX ix ON <temp table>` carries no TEMP keyword and no
            // qualifier — sqlite puts the index wherever its TABLE lives, and
            // the table name is a REFERENCE, so temp shadows main for it the
            // way it does for every other bare name. (A CREATE's own NAME never
            // shadows; that is a different rule and it stays.) Without this the
            // index was built but its record filed in main, where the
            // "how many indexes did this table have" lookup found no table —
            // so the name was dropped and `sqlite_temp_master` reported a
            // synthesised `sqlite_autoindex_…` for an index the caller named.
            let on_temp_table = matches!(target.kind, introspect::DdlKind::Index { .. })
                && target
                    .on_table
                    .as_deref()
                    .is_some_and(|t| names_temp_object(c, t));
            // A `DROP INDEX ix` has no ON clause; the index itself is what may
            // be in temp, and its record is what has to be forgotten there.
            let drops_temp_index = matches!(target.kind, introspect::DdlKind::Index { .. })
                && !target.create
                && c.db
                    .attached_sys_record_scan("temp", introspect::IDX_NS)
                    .iter()
                    .any(|(k, v)| {
                        !v.is_empty()
                            && std::str::from_utf8(k)
                                .is_ok_and(|n| n.eq_ignore_ascii_case(&target.name))
                    });
            (on_temp_table || drops_temp_index).then(|| "temp".to_string())
        }
    }
}

/// File (or forget) a catalog object's own `CREATE …` text after the statement
/// succeeded, so `sqlite_master.sql` can hand back what the caller wrote.
///
/// Best-effort by design: the record is a *fidelity* improvement, never a
/// correctness dependency, so a failed write is swallowed — the reconstruction
/// remains the answer. Writes ride the open transaction when there is one, so
/// the text commits and rolls back atomically with the DDL itself.
///
/// Tables are fingerprinted against the live schema (including uncommitted
/// DDL via [`WriteSession::schema`]) so an `ALTER` that changes the shape
/// falls back to reconstruction. Views and triggers have no equivalent
/// fingerprint; their text is stored as-is until DROP.
fn record_object_ddl(
    c: &mut Sqlite3,
    sqltext: &str,
    target: &introspect::DdlTarget,
    idx_before: Option<usize>,
) {
    if c.readonly {
        return;
    }
    // The `CREATE TABLE` fingerprint is computed with the index records in
    // hand, exactly as `sqlite_master` recomputes it — a fingerprint taken
    // under a different rule would never match itself again.
    // WHICH SCHEMA does this object live in? Its verbatim DDL record is filed
    // there, and read back from there — main for an unqualified statement,
    // otherwise the schema named. Filing every record in main under the bare
    // object name is a WRONG ANSWER once two schemas hold an object of one
    // name: the second CREATE overwrote the first's text, so `main.users`
    // reported the attached `test_schema.users`'s named CHECK constraints.
    let member = record_member(c, sqltext, target);
    let idx = sqlite_index_records_of(c, member.as_deref());
    let (kind, create, name) = (target.kind, target.create, target.name.as_str());
    // `CREATE INDEX` files in its OWN namespace, keyed by the index name, with
    // the shape of the `IndexDef` the statement just appended as the
    // fingerprint. Which `IndexDef` that is comes from the count taken BEFORE
    // execution: mpedb appends exactly one, and treats a duplicate shape as a
    // no-op (in which case nothing was appended and there is nothing to record).
    if let introspect::DdlKind::Index { .. } = kind {
        if !create {
            forget_index_record(c, member.as_deref(), name);
            return;
        }
        let verbatim = introspect::ddl_verbatim(sqltext, target.name_at, kind);
        let (Some(table), Some(before), false) =
            (target.on_table.as_deref(), idx_before, verbatim.is_empty())
        else {
            return;
        };
        let schema = match member.as_deref() {
            Some(m) => c.db.attached_schema_or_empty(m),
            None => match c.txn.as_ref() {
                Some(s) => s.schema(),
                None => c.db.schema(),
            },
        };
        let Some(fp) = introspect::exact_table_name(&schema, table)
            .and_then(|e| introspect::table_by_exact_name(&schema, &e).map(|t| (t, e)))
            .and_then(|(t, _)| {
                (t.indexes.len() == before + 1)
                    .then(|| introspect::index_fingerprint_of(t, before))
            })
            .flatten()
        else {
            return;
        };
        let rec = introspect::index_record(&fp, &verbatim);
        put_record(c, member.as_deref(), introspect::IDX_NS, name.as_bytes(), &rec);
        return;
    }
    let value = if create {
        let verbatim = introspect::ddl_verbatim(sqltext, target.name_at, kind);
        if verbatim.is_empty() {
            return;
        }
        // sqlite stores a temp object's text WITHOUT the `TEMP` keyword
        // (measured: `CREATE TEMP TABLE tt (…)` reads back as
        // `CREATE TABLE tt (…)`), which is exactly the rewrite the temp schema
        // already performs, minus the qualifier the router then strips.
        let verbatim = match mpedb::rewrite_temp_ddl(&verbatim).ok().flatten() {
            Some(r) => r.replacen("temp.", "", 1),
            None => verbatim,
        };
        // A virtual table stores the WHOLE statement (sqlite keeps
        // `CREATE VIRTUAL TABLE t USING …` verbatim); `ddl_verbatim` rebuilt
        // the ordinary-table head, so put the real one back.
        let verbatim = if target.virtual_table {
            match verbatim.strip_prefix("CREATE TABLE ") {
                Some(tail) => format!("CREATE VIRTUAL TABLE {tail}"),
                None => verbatim,
            }
        } else {
            verbatim
        };
        match kind {
            introspect::DdlKind::Table => {
                // Re-resolve against the schema the statement just produced —
                // the open WriteSession's bundle when in a txn, else the
                // committed facade schema. Fingerprint + verbatim ride the
                // same txn.
                //
                // A `CREATE TEMP TABLE` produced its table in the TEMP member,
                // so resolving against main found nothing and returned early:
                // no record was written at all, and `sqlite_temp_master`
                // reported a reconstruction that had dropped the table's named
                // CHECK constraints.
                let schema = match member.as_deref() {
                    Some(m) => c.db.attached_schema_or_empty(m),
                    None => match c.txn.as_ref() {
                        Some(s) => s.schema(),
                        None => c.db.schema(),
                    },
                };
                let Some(exact) = introspect::exact_table_name(&schema, name) else {
                    return;
                };
                let Some(t) = introspect::table_by_exact_name(&schema, &exact) else {
                    return;
                };
                Some((exact, introspect::ddl_record(t, &idx, &verbatim)))
            }
            _ => {
                // Name as written (unquoted). Catalog resolution is case-
                // insensitive on the way in; sqlite_master reports the stored
                // name from list_views/list_triggers, so also store under the
                // live name when we can resolve it.
                let exact = resolve_object_name(c, kind, name).unwrap_or_else(|| name.to_string());
                Some((exact, introspect::object_ddl_record(&verbatim)))
            }
        }
    } else {
        None
    };
    match value {
        Some((exact, rec)) => {
            let (ns, key) = introspect::ddl_key(&exact);
            // A CREATE that reaches here having NOT created anything is an
            // `IF NOT EXISTS` no-op: a plain `CREATE` over an existing object
            // errors, and an error never gets this far. sqlite keeps the text
            // of the statement that ACTUALLY created the object, so the
            // standing record wins. (A DROP leaves an empty tombstone, so a
            // genuine re-CREATE is not blocked by it.)
            let already = sqlite_master_records_of(c, member.as_deref())
                .get(&exact)
                .is_some_and(|v| !v.is_empty());
            if !already {
                put_record(c, member.as_deref(), ns, &key, &rec);
            }
            // fts4 (plan §7): the five shadow tables were created in the same
            // statement — file each one's record with sqlite's EXACT
            // single-quoted, typeless DDL as the verbatim half. Both halves
            // are ours, so the fingerprint mechanism carries them unchanged.
            if target.virtual_table {
                let schema = match member.as_deref() {
                    Some(m) => c.db.attached_schema_or_empty(m),
                    None => match c.txn.as_ref() {
                        Some(s) => s.schema(),
                        None => c.db.schema(),
                    },
                };
                let is_fts4 = introspect::table_by_exact_name(&schema, &exact).is_some_and(|t| {
                    matches!(
                        t.kind,
                        mpedb::TableKind::Fts { module: mpedb::FtsModule::Fts4, .. }
                    )
                });
                if is_fts4 {
                    let content: Vec<String> = introspect::table_by_exact_name(&schema, &exact)
                        .map(|t| t.visible_columns().iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    for (sname, ssql) in introspect::fts4_shadow_sql(&exact, &content) {
                        let Some(st) = introspect::table_by_exact_name(&schema, &sname) else {
                            continue;
                        };
                        let srec = introspect::ddl_record(st, &idx, &ssql);
                        let (sns, skey) = introspect::ddl_key(&sname);
                        put_record(c, member.as_deref(), sns, &skey, &srec);
                    }
                }
            }
        }
        // DROP: forget the text. The facade has no delete outside a session, so
        // autocommit writes an EMPTY record instead — a tombstone, since
        // `master_sql` only trusts a record that carries a fingerprint.
        None => {
            let exact = resolve_object_name(c, kind, name).unwrap_or_else(|| name.to_string());
            let (ns, key) = introspect::ddl_key(&exact);
            // The record lives with the object, so the tombstone does too —
            // dropping `test_schema.users` must not blank main's text.
            put_record(c, member.as_deref(), ns, &key, &[]);
            // A DROPPED TABLE takes its index names with it. Without this, a
            // re-CREATEd table of the same name and shape would inherit the old
            // table's index records — a name (and a `CREATE INDEX` text) for an
            // index nobody created, which is exactly the "almost right metadata"
            // failure the fingerprint exists to prevent.
            if kind == introspect::DdlKind::Table {
                forget_table_index_records(c, member.as_deref(), &exact);
                // A dropped fts4 vtab took its five shadows with it (same
                // txn). Their records tombstone here too — but ONLY when the
                // table is genuinely gone: a real user table that merely
                // shares the suffix keeps its record.
                let schema = match member.as_deref() {
                    Some(m) => c.db.attached_schema_or_empty(m),
                    None => match c.txn.as_ref() {
                        Some(s) => s.schema(),
                        None => c.db.schema(),
                    },
                };
                for sfx in ["_content", "_docsize", "_segdir", "_segments", "_stat"] {
                    let sname = format!("{exact}{sfx}");
                    if introspect::table_by_exact_name(&schema, &sname).is_none() {
                        let has_rec = sqlite_master_records_of(c, member.as_deref())
                            .get(&sname)
                            .is_some_and(|v| !v.is_empty());
                        if has_rec {
                            let (sns, skey) = introspect::ddl_key(&sname);
                            put_record(c, member.as_deref(), sns, &skey, &[]);
                        }
                    }
                }
            }
        }
    }
}

/// File one record in the schema that owns it. An empty value is a TOMBSTONE:
/// the facade has no delete outside a session, and `master_sql` only trusts a
/// record that carries a fingerprint.
fn put_record(c: &mut Sqlite3, member: Option<&str>, ns: &str, key: &[u8], val: &[u8]) {
    match member {
        Some(m) => {
            c.db.attached_sys_record_put(m, ns, key, val);
        }
        None => {
            let _ = match c.txn.as_mut() {
                Some(s) if val.is_empty() => s.sys_record_delete(ns, key).map(|_| ()),
                Some(s) => s.sys_record_put(ns, key, val),
                None => c.db.sys_record_put(ns, key, val),
            };
        }
    }
}

/// Carry a table's verbatim DDL — and its indexes' table attribution — across
/// an `ALTER TABLE … RENAME TO`.
///
/// sqlite keeps the original `CREATE TABLE` text forever and only retargets its
/// name, so everything the reconstruction cannot express survives a rename:
/// a column's declared `COLLATE` spelling, the exact type words, a named CHECK.
/// The records here are keyed by table NAME, so without this the rename simply
/// orphaned them and `sqlite_master` fell back to reconstructing — which is why
/// `COLLATE nocase` came back as `COLLATE NOCASE` and Django's
/// `assertColumnCollation` failed on a string compare.
///
/// It matters beyond that one assertion: Django implements nearly every
/// `AlterField` on sqlite by building `new__<table>` and renaming it into
/// place, so this is the ordinary path, not an exotic one.
///
/// The index records keep their own (name) keys — a table rename does not
/// rename its indexes — but the fingerprint inside each VALUE leads with the
/// table name, and `forget_table_index_records` matches on it. Left stale, the
/// eventual `DROP TABLE` would not forget them and a later table of the old
/// name would inherit index names nobody created.
fn move_table_ddl_record(c: &mut Sqlite3, member: Option<&str>, old: &str, new: &str) {
    if c.readonly {
        return;
    }
    let (ns, old_key) = introspect::ddl_key(old);
    let recs = sqlite_master_records_of(c, member);
    if let Some(rec) = recs.get(old).filter(|v| !v.is_empty()).cloned() {
        // The record is `fingerprint ‖ NUL ‖ verbatim`. Both halves name the
        // table, and the fingerprint is a `create_ddl` reconstruction that
        // `sqlite_master` recomputes from the LIVE table — so it has to be
        // re-derived under the new name, not textually patched, or the record
        // would read as permanently stale and never be used.
        let moved = introspect::ddl_record_verbatim(&rec)
            .and_then(|verbatim| mpedb::rename_table_in_ddl(verbatim, old, new).ok().flatten());
        if let Some(verbatim) = moved {
            let schema = match member {
                Some(m) => c.db.attached_schema_or_empty(m),
                None => match c.txn.as_ref() {
                    Some(s) => s.schema(),
                    None => c.db.schema(),
                },
            };
            let idx = sqlite_index_records_of(c, member);
            if let Some(t) = introspect::exact_table_name(&schema, new)
                .and_then(|e| introspect::table_by_exact_name(&schema, &e))
            {
                let (_, new_key) = introspect::ddl_key(&t.name);
                let rec = introspect::ddl_record(t, &idx, &verbatim);
                put_record(c, member, ns, &new_key, &rec);
            }
        }
    }
    // Whatever happened above, the OLD key must not keep answering: a table of
    // that name created later would otherwise inherit this text.
    put_record(c, member, ns, &old_key, &[]);
    retable_index_records(c, member, old, new);
}

/// Re-attribute every index record whose fingerprint names `old` to `new`.
fn retable_index_records(c: &mut Sqlite3, member: Option<&str>, old: &str, new: &str) {
    let ns = introspect::IDX_NS;
    let all = match member {
        Some(m) => c.db.attached_sys_record_scan(m, ns),
        None => match c.txn.as_mut() {
            Some(s) => s.sys_record_scan_range(ns, &[], &[0xff]).unwrap_or_default(),
            None => c.db.sys_record_scan(ns).unwrap_or_default(),
        },
    };
    for (k, v) in all {
        let Some(fp) = introspect::index_record_fingerprint(&v) else { continue };
        if introspect::fingerprint_table(fp) != old {
            continue;
        }
        let Some(rest) = fp.strip_prefix(old) else { continue };
        // The verbatim `CREATE INDEX … ON <old>` is retargeted too, exactly as
        // sqlite retargets it. If it cannot be (see `rename_table_in_ddl`), the
        // old text is kept: an index record has no reconstruction to fall back
        // to, so stale-but-parseable beats absent.
        let verbatim = introspect::ddl_record_verbatim(&v).unwrap_or_default();
        let verbatim = mpedb::rename_table_in_ddl(verbatim, old, new)
            .ok()
            .flatten()
            .unwrap_or_else(|| verbatim.to_string());
        put_record(c, member, ns, &k, &introspect::index_record(&format!("{new}{rest}"), &verbatim));
    }
}

/// Tombstone one index record by name.
fn forget_index_record(c: &mut Sqlite3, member: Option<&str>, name: &str) {
    put_record(c, member, introspect::IDX_NS, name.as_bytes(), &[]);
}

/// Tombstone every index record whose fingerprint names `table`.
fn forget_table_index_records(c: &mut Sqlite3, member: Option<&str>, table: &str) {
    let ns = introspect::IDX_NS;
    let all = match member {
        Some(m) => c.db.attached_sys_record_scan(m, ns),
        None => match c.txn.as_mut() {
            Some(s) => s.sys_record_scan_range(ns, &[], &[0xff]).unwrap_or_default(),
            None => c.db.sys_record_scan(ns).unwrap_or_default(),
        },
    };
    for (k, v) in all {
        let owns = introspect::index_record_fingerprint(&v)
            .is_some_and(|fp| introspect::fingerprint_table(fp) == table);
        if !owns {
            continue;
        }
        put_record(c, member, ns, &k, &[]);
    }
}

/// Resolve a view/trigger name to the catalog's stored spelling (case fold).
fn resolve_object_name(
    c: &mut Sqlite3,
    kind: introspect::DdlKind,
    name: &str,
) -> Option<String> {
    match kind {
        introspect::DdlKind::Table => {
            let schema = match c.txn.as_ref() {
                Some(s) => s.schema(),
                None => c.db.schema(),
            };
            introspect::exact_table_name(&schema, name)
        }
        introspect::DdlKind::View => {
            let views = match c.txn.as_mut() {
                Some(s) => s.list_views().unwrap_or_default(),
                None => c.db.list_views().unwrap_or_default(),
            };
            views
                .into_iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, _)| n)
        }
        introspect::DdlKind::Trigger => {
            let triggers = match c.txn.as_mut() {
                Some(s) => s.list_triggers().unwrap_or_default(),
                None => c.db.list_triggers().unwrap_or_default(),
            };
            triggers
                .into_iter()
                .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, _, _)| n)
        }
        // An index has no catalog entry to fold a name against: mpedb's
        // `IndexDef` carries no name at all, so the record's key IS the name
        // the caller wrote. Handled entirely in `record_object_ddl`.
        introspect::DdlKind::Index { .. } => None,
    }
}

/// mpedb's plan text in sqlite's `EXPLAIN QUERY PLAN` shape: four columns
/// `(id, parent, notused, detail)`, one row per line of the plan.
///
/// The `detail` strings are mpedb's own (`Select t`, `access: FullScan`, …),
/// not sqlite's (`SCAN t`, `SEARCH t USING INDEX …`) — sqlite documents EQP
/// output as human-facing and explicitly unstable between releases, so the
/// honest answer here is a description of the plan mpedb will actually run.
/// Indentation in mpedb's text nests the plan; it is preserved in `detail` and
/// also expressed structurally, with an indented line's `parent` pointing at
/// the nearest less-indented line above it, as sqlite's tree does.
fn eqp_outcome(res: ExecResult) -> Outcome {
    let ExecResult::Explain(text) = res else {
        return to_outcome(res);
    };
    let columns = ["id", "parent", "notused", "detail"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // (indent, id) of the lines still eligible to be a parent.
    let mut stack: Vec<(usize, i64)> = Vec::new();
    let mut rows = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let indent = line.len() - line.trim_start().len();
        while stack.last().is_some_and(|&(i, _)| i >= indent) {
            stack.pop();
        }
        let id = rows.len() as i64 + 1;
        let parent = stack.last().map_or(0, |&(_, p)| p);
        stack.push((indent, id));
        rows.push(vec![
            Value::Int(id),
            Value::Int(parent),
            Value::Int(0),
            Value::Text(line.trim_end().to_string()),
        ]);
    }
    Outcome::Rows { columns, rows }
}

fn to_outcome(res: ExecResult) -> Outcome {
    match res {
        ExecResult::Rows { columns, rows } => Outcome::Rows { columns, rows },
        ExecResult::Affected(n) => Outcome::Affected(n),
        // mpedb EXPLAIN yields plan text; present it as a single "plan" column
        // so a caller stepping/reading it behaves.
        ExecResult::Explain(text) => Outcome::Rows {
            columns: vec!["plan".to_string()],
            rows: vec![vec![Value::Text(text)]],
        },
    }
}

/// A contention error a RETRY can clear — an optimistic-mode `WriteConflict`
/// (the loser rolled back, nothing applied), a full reader table, or an evicted
/// read snapshot. valconv maps all three to `SQLITE_BUSY`; `busy_timeout` waits
/// on exactly these.
///
/// `DbError::Busy` is deliberately NOT here (#109): it means the ENGINE
/// already waited out this connection's busy timeout at the writer lock
/// (`Database::set_busy_timeout`, wired at open / `sqlite3_busy_timeout` /
/// `PRAGMA busy_timeout`) — retrying it in this loop would double the wait.
/// It maps straight to `SQLITE_BUSY` ("database is locked").
fn is_busy_err(e: &DbError) -> bool {
    matches!(
        e,
        DbError::WriteConflict | DbError::ReadersFull | DbError::SnapshotEvicted
    ) || valconv::is_writer_lock_reentry(e)
}

/// sqlite's own default-busy-handler delay table (ms), then 100 ms steady.
fn busy_backoff(tries: u32) -> Duration {
    const DELAYS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
    Duration::from_millis(DELAYS[(tries as usize).min(DELAYS.len() - 1)])
}

pub(super) fn run_stmt(s: &mut Stmt) -> c_int {
    let Some(c) = (unsafe { conn(s.db) }) else {
        return SQLITE_MISUSE;
    };
    let is_dml = matches!(sql::classify(&s.sql), sql::Kind::Dml { .. });
    let params = s.binds.clone();
    // An interrupt requested before we start aborts this step and is consumed
    // (sqlite clears the flag when the interrupted statement finishes).
    if c.interrupted.swap(false, Ordering::SeqCst) {
        c.set_error(SQLITE_INTERRUPT, SQLITE_INTERRUPT, "interrupted");
        return SQLITE_INTERRUPT;
    }
    // A read-only connection (`file:…?mode=ro`) refuses every statement that
    // could write. Transaction control is allowed (as sqlite does): the write
    // inside it is what gets refused.
    if c.readonly && matches!(sql::classify(&s.sql), sql::Kind::Dml { .. } | sql::Kind::Ddl) {
        c.set_error(SQLITE_READONLY, SQLITE_READONLY, "attempt to write a readonly database");
        return SQLITE_READONLY;
    }
    // The statement is about to run: drain any stale UDF-error stash so an
    // error surfaced by THIS run is attributable to this run alone.
    udf::take_last_udf_error();
    // `busy_timeout(ms)`: on a RETRYABLE contention error (`is_busy_err`),
    // sleep with sqlite's backoff and retry until the deadline, exactly as
    // sqlite's default busy handler does — a transient conflict clears
    // instead of failing the call. Zero timeout (the default) = no retry,
    // immediate BUSY, as sqlite. Writer-LOCK contention never reaches this
    // loop: the engine itself waits out the same timeout at the lock
    // (`Database::set_busy_timeout`, #109) and returns the terminal
    // `DbError::Busy` — retrying that here would double the wait.
    let deadline =
        (c.busy_timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(c.busy_timeout_ms as u64));
    // Execute the parameter-rewritten text (`$K` placeholders) so mpedb binds
    // the caller's values by number; classification/introspection are unaffected
    // by the rewrite (only placeholders change), so they still use `s.sql`.
    let mut tries = 0u32;
    let outcome = loop {
        match catch_unwind(AssertUnwindSafe(|| exec_one(c, &s.exec_sql, &params))) {
            Ok(Err(ref e)) if is_busy_err(e) && deadline.is_some_and(|d| Instant::now() < d) => {
                // sqlite3_interrupt breaks the busy wait instead of sleeping on.
                if c.interrupted.swap(false, Ordering::SeqCst) {
                    c.set_error(SQLITE_INTERRUPT, SQLITE_INTERRUPT, "interrupted");
                    return SQLITE_INTERRUPT;
                }
                std::thread::sleep(busy_backoff(tries));
                tries += 1;
                continue;
            }
            Ok(r) => break r,
            Err(_) => {
                c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
                return SQLITE_ERROR;
            }
        }
    };
    match outcome {
        Ok(Outcome::Rows { columns, rows }) => {
            if is_dml {
                c.changes = rows.len() as c_int; // INSERT/…/RETURNING row count
                c.total_changes = c.total_changes.saturating_add(rows.len() as c_int);
            }
            s.col_name_c = columns
                .iter()
                .map(|n| {
                    let mut v = n.as_bytes().to_vec();
                    v.push(0);
                    v
                })
                .collect();
            s.columns = columns;
            s.rows = rows;
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Outcome::Affected(n)) => {
            if is_dml {
                c.changes = n as c_int;
                c.total_changes = c.total_changes.saturating_add(n as c_int);
            }
            s.columns.clear();
            s.col_name_c.clear();
            s.decltype_c = None;
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Outcome::Control) => {
            s.columns.clear();
            s.col_name_c.clear();
            s.decltype_c = None;
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Err(e) => {
            // A host UDF that called `sqlite3_result_error*` failed this
            // statement: the engine tunnels that as an opaque `Unsupported`
            // wrapper ("user function raised: …"), but the CONSUMER'S contract
            // is the code and text the callback itself set — CPython maps
            // NOMEM -> MemoryError, TOOBIG -> DataError, and asserts the exact
            // message. Present the callback's own error when this statement's
            // failure is that error.
            if let Some((code, msg)) = udf::take_last_udf_error() {
                if e.to_string().ends_with(&msg) {
                    let primary = code & 0xff;
                    c.set_error(primary, code, &msg);
                    return primary;
                }
            }
            c.set_db_error(&e)
        }
    }
}

/// Render the current row (`rows[pos]`) into `cells` and advance.
pub(super) fn load_current_row(s: &mut Stmt) {
    let row = &s.rows[s.pos];
    s.cells = row
        .iter()
        .map(|v| {
            let ty = valconv::sqlite_type(v);
            let is_null = matches!(v, Value::Null);
            let (text_c, len) = match valconv::as_bytes(v) {
                Some(mut payload) => {
                    let len = payload.len() as c_int;
                    payload.push(0);
                    (payload, len)
                }
                None => (vec![0u8], 0),
            };
            Cell {
                ty,
                is_null,
                i64v: valconv::as_i64(v),
                f64v: valconv::as_f64(v),
                text_c,
                len,
            }
        })
        .collect();
    s.pos += 1;
    s.have_row = true;
}

