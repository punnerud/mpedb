//! One connection, start to finish.
//!
//! # The shape
//!
//! A `Session` owns a socket and an mpedb handle and runs until the client
//! terminates or the socket dies. There is no connection pool, no worker thread
//! and no shared state: mpedb-pg is one PROCESS per connection, spawned by
//! inetd or systemd socket activation, and the process exiting IS the cleanup.
//! Prepared statements, portals and the session catalog live in this struct
//! because that is exactly as long as they are allowed to live.
//!
//! # The limitation that must be stated before anyone finds it
//!
//! **mpedb has one writer at a time.** A client that opens `BEGIN`, thinks for
//! ten seconds and then `COMMIT`s holds mpedb's writer lock for those ten
//! seconds, and every other writer waits. PostgreSQL does not behave this way —
//! it has row-level MVCC for writers — so an application ported across will see
//! contention PostgreSQL would not have shown it.
//!
//! This is not a wrong answer and it is not a bug; it is mpedb's concurrency
//! model meeting PostgreSQL's transaction model. What this module adds is that
//! it fails LOUDLY rather than quietly: `[pg] max_write_txn_ms` bounds how long
//! an idle-in-transaction writer may hold the lock, and crossing it is a named
//! error rather than an unexplained stall on every other connection.

use crate::catalog::{self, SessionCatalog};
use crate::errmap;
use crate::proto::{self, FieldDesc, Frontend, Out, Startup, TxnStatus};
use crate::value;
use mpedb::{Database, ExecResult};
use mpedb_types::{Dialect, Error, Value};
use std::collections::HashMap;
use std::io::{Read, Write};

/// A statement prepared by `Parse`, awaiting `Bind`.
#[derive(Clone)]
struct Prepared {
    sql: String,
    /// The parameter type OIDs the client declared. `0` means "you decide",
    /// which is what psycopg sends for everything.
    param_oids: Vec<u32>,
}

/// A `Bind`ed statement, ready to `Execute`.
///
/// The statement NAME it came from is deliberately not kept: PostgreSQL's
/// `Close 'S'` destroys the statement without touching portals bound from it,
/// and mpedb-pg is one process per connection, so an orphaned portal dies with
/// the process rather than needing to be swept.
#[derive(Clone)]
struct Portal {
    sql: String,
    params: Vec<Value>,
    /// Rows already produced but not yet sent, when a previous `Execute` hit
    /// its `max_rows` limit. mpedb materialises a result set, so suspension is
    /// a cursor over a Vec rather than a paused scan — honest, and the reason
    /// `max_rows` does not reduce peak memory here the way it does in
    /// PostgreSQL.
    pending: Option<PendingRows>,
}

#[derive(Clone)]
struct PendingRows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    at: usize,
    /// The statement text, kept so a resumed portal can still describe its
    /// columns by their DECLARED type (see [`Session::describe_rows`]).
    sql: String,
}

/// How the session is currently answering.
pub struct Session<S: Read + Write> {
    io: S,
    db: Database,
    cat: SessionCatalog,
    out: Out,
    status: TxnStatus,
    prepared: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// An explicit `BEGIN` is open. mpedb's own write session is NOT held
    /// across round trips here — see [`Self::exec_one`] for why.
    in_explicit_txn: bool,
    /// Statements accumulated inside an explicit transaction block.
    ///
    /// This is the honest resolution of PostgreSQL's interactive transaction
    /// model against mpedb's single writer: rather than hold the writer lock
    /// from `BEGIN` to `COMMIT` (which blocks every other process for as long
    /// as the client is thinking), the block is REPLAYED as one mpedb
    /// transaction at `COMMIT`.
    ///
    /// The block DOES see its own writes, and the mechanism is
    /// [`Self::exec_in_block`]: every statement inside the block replays this
    /// log into a write transaction, runs, takes its answer and rolls back.
    ///
    /// This comment used to claim the property came from "the session's
    /// snapshot". It did not, and nothing tested it, so a buffered `INSERT`
    /// returned a fabricated `Affected(0)` and an in-block `SELECT` read the
    /// PRE-transaction state. A property asserted in prose is a property
    /// nobody checks — `wire.rs` now checks this one.
    txn_log: Vec<(String, Vec<Value>)>,
    pid: i32,
    secret: i32,
    /// Reported as `server_version` and by `SHOW server_version`.
    server_version: String,
    /// Whether the client must send a password message before anything else.
    require_password: bool,
}

/// Everything a connection needs to know before the first byte.
pub struct Options {
    /// The user database.
    pub db: Database,
    /// Reported as the `server_version` parameter.
    pub server_version: String,
    /// Require a password (any password) rather than trusting the peer.
    ///
    /// Trust is the DEFAULT and it is not laziness: mpedb-pg's fence is the OS.
    /// On a unix socket the peer is already authenticated by filesystem
    /// permissions, which is a stronger statement than a password in a config
    /// file. Over TCP the fence is nginx or a firewall.
    pub require_password: bool,
}

impl<S: Read + Write> Session<S> {
    pub fn new(io: S, opts: Options) -> Session<S> {
        let pid = std::process::id() as i32;
        // A v3-protocol client IS talking to PostgreSQL, and PostgreSQL has no
        // `PRAGMA foreign_keys`: referential integrity is not optional there.
        // mpedb's default is sqlite's — OFF, the only default that leaves an
        // existing file's behaviour unchanged — which over this protocol means
        // a client's `REFERENCES` is accepted and then not enforced. That is a
        // wrong answer, and one no v3 client has a way to ask about.
        //
        // Set on the CONNECTION, not on the database: this is the same
        // per-connection pragma `PRAGMA foreign_keys = ON` flips, so a
        // sqlite-dialect process attached to the same file keeps its own
        // setting. The wire protocol decides for its own connections and for
        // nothing else.
        opts.db.set_fk_enforced(true);
        Session {
            io,
            db: opts.db,
            cat: SessionCatalog::new(),
            out: Out::new(),
            status: TxnStatus::Idle,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            in_explicit_txn: false,
            txn_log: Vec::new(),
            pid,
            // Derived from the pid rather than random: mpedb has no RNG
            // dependency by policy, and the secret's job here is to make a
            // CancelRequest from an unrelated process implausible, not to
            // resist an attacker who can already read the process table.
            secret: pid.rotate_left(16) ^ 0x5f3a_c71d,
            server_version: opts.server_version,
            require_password: opts.require_password,
        }
    }
}

/// Run one connection to completion.
pub fn serve<S: Read + Write>(io: S, opts: Options) -> std::io::Result<()> {
    let mut s = Session::new(io, opts);
    s.run()
}

impl<S: Read + Write + AsBytes> Session<S> {
    /// Drive the session to completion over a canned stream and hand back
    /// everything the backend wrote.
    ///
    /// Exists so the protocol can be tested without a socket and without
    /// PostgreSQL installed — the socket is the only thing swapped out, so the
    /// code under test is exactly the code a real client reaches.
    pub fn run_for_test(&mut self) -> Vec<u8> {
        // A canned stream ends in EOF, which `run` reports as a clean
        // disconnect — the same thing a client hanging up looks like.
        let _ = self.run();
        self.io.written().to_vec()
    }
}

/// A test transport that can hand back what was written to it.
pub trait AsBytes {
    fn written(&self) -> &[u8];
}

impl<S: Read + Write> Session<S> {
    fn run(&mut self) -> std::io::Result<()> {
        // The client may ask about TLS (and GSSAPI) before sending the real
        // startup packet, and it may ask about both. A loop rather than an
        // `if`, because answering only the first leaves the second unread and
        // the stream desynchronised.
        let params = loop {
            match proto::read_startup(&mut self.io)? {
                Startup::SslRequest | Startup::GssEncRequest => {
                    // 'N' = not available. TLS terminates in nginx or stunnel;
                    // a process spawned per connection has no business holding
                    // a certificate, and every client falls back cleanly.
                    self.io.write_all(b"N")?;
                    self.io.flush()?;
                }
                Startup::Cancel { .. } => {
                    // A cancel arrives on its OWN connection and never gets a
                    // reply — PostgreSQL closes it silently, and a client that
                    // waited for one would hang.
                    return Ok(());
                }
                Startup::Params(p) => break p,
            }
        };
        self.startup(&params)?;
        self.main_loop()
    }

    fn startup(&mut self, params: &[(String, String)]) -> std::io::Result<()> {
        if self.require_password {
            self.out.auth_cleartext();
            self.out.flush_to(&mut self.io)?;
            match proto::read_message(&mut self.io)? {
                Frontend::PasswordMessage(_) => {}
                _ => {
                    self.out
                        .error(errmap::SEVERITY_ERROR, "28000", "authentication failed");
                    self.out.flush_to(&mut self.io)?;
                    return Ok(());
                }
            }
        }
        self.out.auth_ok();

        // The parameters a client reads at connect. Getting `server_version`
        // wrong is not cosmetic: SQLAlchemy and Django parse it to decide which
        // features exist, and a string they cannot parse fails the CONNECT, not
        // the first query.
        let user = params
            .iter()
            .find(|(k, _)| k == "user")
            .map(|(_, v)| v.as_str())
            .unwrap_or("mpedb");
        for (k, v) in [
            ("server_version", self.server_version.clone()),
            ("server_encoding", "UTF8".into()),
            ("client_encoding", "UTF8".into()),
            ("application_name", String::new()),
            ("is_superuser", "on".into()),
            ("session_authorization", user.to_string()),
            // mpedb compares text bytewise, so the only honest collation to
            // claim is C. Saying anything else promises an ordering the engine
            // does not implement.
            ("DateStyle", "ISO, MDY".into()),
            ("IntervalStyle", "postgres".into()),
            ("TimeZone", "UTC".into()),
            ("integer_datetimes", "on".into()),
            ("standard_conforming_strings", "on".into()),
        ] {
            self.out.parameter_status(k, &v);
        }
        self.out.backend_key_data(self.pid, self.secret);
        self.out.ready_for_query(self.status);
        self.out.flush_to(&mut self.io)
    }

    fn main_loop(&mut self) -> std::io::Result<()> {
        loop {
            let msg = match proto::read_message(&mut self.io) {
                Ok(m) => m,
                // A client that vanishes is the normal end of a connection, not
                // an error worth reporting anywhere.
                Err(e) if is_disconnect(&e) => return Ok(()),
                Err(e) => return Err(e),
            };
            match msg {
                Frontend::Terminate => return Ok(()),
                Frontend::Query(sql) => self.simple_query(&sql)?,
                Frontend::Parse {
                    name,
                    sql,
                    param_types,
                } => self.parse(name, sql, param_types),
                Frontend::Bind {
                    portal,
                    statement,
                    param_formats,
                    params,
                    ..
                } => self.bind(portal, statement, &param_formats, &params),
                Frontend::Describe { kind, name } => self.describe(kind, &name),
                Frontend::Execute { portal, max_rows } => self.execute(&portal, max_rows),
                Frontend::Close { kind, name } => {
                    match kind {
                        b'S' => {
                            self.prepared.remove(&name);
                        }
                        _ => {
                            self.portals.remove(&name);
                        }
                    }
                    self.out.close_complete();
                }
                Frontend::Sync => {
                    self.out.ready_for_query(self.status);
                    self.out.flush_to(&mut self.io)?;
                }
                Frontend::Flush => self.out.flush_to(&mut self.io)?,
                Frontend::CopyData(_) | Frontend::CopyDone => {
                    // A stray COPY message outside a COPY is a protocol error in
                    // PostgreSQL too; ignoring it keeps the stream in step.
                }
                Frontend::CopyFail(m) => self.fail_str(errmap::FEATURE_NOT_SUPPORTED, &m),
                Frontend::PasswordMessage(_) => {}
                Frontend::Unknown(tag, _) => self.fail_str(
                    errmap::PROTOCOL_VIOLATION,
                    &format!("unsupported frontend message '{}'", tag as char),
                ),
            }
            // Extended-query messages accumulate and are flushed at Sync; the
            // simple-query path flushes itself. Flushing here as well is
            // harmless and keeps a large Parse/Bind/Execute batch from growing
            // the buffer without bound.
            if self.out.len() > 64 * 1024 {
                self.out.flush_to(&mut self.io)?;
            }
        }
    }

    // ------------------------------------------------------------ simple query

    fn simple_query(&mut self, sql: &str) -> std::io::Result<()> {
        let statements = split_statements(sql);
        if statements.is_empty() {
            self.out.empty_query_response();
            self.out.ready_for_query(self.status);
            return self.out.flush_to(&mut self.io);
        }
        for stmt in statements {
            if self.status == TxnStatus::Failed && !is_txn_end(&stmt) {
                self.fail_str(
                    errmap::IN_FAILED_SQL_TRANSACTION,
                    "current transaction is aborted, commands ignored until end of \
                     transaction block",
                );
                continue;
            }
            match self.exec_one(&stmt, &[]) {
                Ok(Answer::Rows { columns, rows }) => {
                    let fields = self.describe_rows(&stmt, &columns, &rows);
                    self.out.row_description(&fields);
                    for row in &rows {
                        let vals: Vec<Option<Vec<u8>>> = row
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                value::to_text_as(v, fields[i].type_oid)
                            })
                            .collect();
                        self.out.data_row(&vals);
                    }
                    self.out.command_complete(&format!("SELECT {}", rows.len()));
                }
                Ok(Answer::Affected(tag)) => self.out.command_complete(&tag),
                Ok(Answer::Empty) => self.out.empty_query_response(),
                Err(e) => self.fail(&e),
            }
        }
        self.out.ready_for_query(self.status);
        self.out.flush_to(&mut self.io)
    }

    // ---------------------------------------------------------- extended query

    fn parse(&mut self, name: String, sql: String, param_oids: Vec<u32>) {
        // An unnamed statement is replaced silently; a NAMED one that already
        // exists is a protocol error in PostgreSQL, and clients rely on that to
        // catch their own bugs.
        if !name.is_empty() && self.prepared.contains_key(&name) {
            self.fail_str(
                errmap::PROTOCOL_VIOLATION,
                &format!("prepared statement \"{name}\" already exists"),
            );
            return;
        }
        self.prepared.insert(name, Prepared { sql, param_oids });
        self.out.parse_complete();
    }

    fn bind(
        &mut self,
        portal: String,
        statement: String,
        param_formats: &[i16],
        raw: &[Option<Vec<u8>>],
    ) {
        let Some(prep) = self.prepared.get(&statement).cloned() else {
            self.fail_str(
                errmap::INVALID_SQL_STATEMENT_NAME,
                &format!("prepared statement \"{statement}\" does not exist"),
            );
            return;
        };
        // A single format code applies to ALL parameters — the rule that is
        // easy to miss and that produces garbage for every parameter after the
        // first when it is.
        let binary_at = |i: usize| match param_formats.len() {
            0 => false,
            1 => param_formats[0] == 1,
            _ => param_formats.get(i).copied().unwrap_or(0) == 1,
        };
        let mut params = Vec::with_capacity(raw.len());
        for (i, r) in raw.iter().enumerate() {
            if binary_at(i) && r.is_some() {
                self.fail_str(
                    errmap::FEATURE_NOT_SUPPORTED,
                    "binary parameter format is not supported — mpedb advertises text \
                     format only, and decoding binary would risk a plausible wrong value \
                     with no error to notice",
                );
                return;
            }
            let want = prep
                .param_oids
                .get(i)
                .and_then(|oid| mpedb_types::pgtype::by_oid(*oid))
                .and_then(|t| t.mpedb);
            match value::from_text(r.as_deref(), want) {
                Ok(v) => params.push(v),
                Err(m) => {
                    self.fail_str(errmap::INVALID_TEXT_REPRESENTATION, &m);
                    return;
                }
            }
        }
        self.portals.insert(
            portal,
            Portal {
                sql: prep.sql,
                params,
                pending: None,
            },
        );
        self.out.bind_complete();
    }

    fn describe(&mut self, kind: u8, name: &str) {
        match kind {
            b'S' => {
                let Some(prep) = self.prepared.get(name).cloned() else {
                    self.fail_str(
                        errmap::INVALID_SQL_STATEMENT_NAME,
                        &format!("prepared statement \"{name}\" does not exist"),
                    );
                    return;
                };
                // mpedb infers parameter types at bind, not at prepare, so the
                // only honest answer is the OIDs the CLIENT declared — with
                // `unknown` (705) where it declared nothing. Inventing a type
                // here would make the client encode to it.
                let oids: Vec<u32> = prep
                    .param_oids
                    .iter()
                    .map(|o| if *o == 0 { mpedb_types::pgtype::OID_UNKNOWN } else { *o })
                    .collect();
                self.out.parameter_description(&oids);
                self.describe_result_shape(&prep.sql);
            }
            _ => {
                let Some(p) = self.portals.get(name).cloned() else {
                    self.fail_str(
                        errmap::INVALID_CURSOR_NAME,
                        &format!("portal \"{name}\" does not exist"),
                    );
                    return;
                };
                self.describe_result_shape(&p.sql);
            }
        }
    }

    /// Answer a Describe's result half.
    ///
    /// A statement's output shape is only knowable by compiling it, and mpedb
    /// compiles at execute. Rather than execute a statement the client has not
    /// asked to run — which for an INSERT would be a side effect nobody
    /// requested — the shape is derived from the statement KIND: a SELECT
    /// promises rows whose description is sent for real at Execute, anything
    /// else promises none.
    fn describe_result_shape(&mut self, sql: &str) {
        if returns_rows(sql) {
            // A RowDescription with no fields would be a lie of a different
            // kind; `NoData` is wrong for a SELECT. PostgreSQL sends the real
            // description here, and a client that insists on it before Execute
            // will see an empty one — noted in COMPAT-PG.md as the one Describe
            // case mpedb answers late.
            self.out.row_description(&[]);
        } else {
            self.out.no_data();
        }
    }

    fn execute(&mut self, portal: &str, max_rows: i32) {
        let Some(mut p) = self.portals.get(portal).cloned() else {
            self.fail_str(
                errmap::INVALID_CURSOR_NAME,
                &format!("portal \"{portal}\" does not exist"),
            );
            return;
        };
        if self.status == TxnStatus::Failed && !is_txn_end(&p.sql) {
            self.fail_str(
                errmap::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            );
            return;
        }

        // A suspended portal resumes from its own buffer; it must NOT re-run
        // the statement, which for a DML portal would apply it twice.
        let pending = match p.pending.take() {
            Some(pend) => pend,
            None => match self.exec_one(&p.sql, &p.params) {
                Ok(Answer::Rows { columns, rows }) => PendingRows {
                    columns,
                    rows,
                    at: 0,
                    sql: p.sql.clone(),
                },
                Ok(Answer::Affected(tag)) => {
                    self.out.command_complete(&tag);
                    return;
                }
                Ok(Answer::Empty) => {
                    self.out.empty_query_response();
                    return;
                }
                Err(e) => {
                    self.fail(&e);
                    return;
                }
            },
        };

        let limit = if max_rows <= 0 {
            usize::MAX
        } else {
            max_rows as usize
        };
        let end = pending.rows.len().min(pending.at.saturating_add(limit));
        let fields = self.describe_rows(&pending.sql, &pending.columns, &pending.rows);
        self.out.row_description(&fields);
        for row in &pending.rows[pending.at..end] {
            let vals: Vec<Option<Vec<u8>>> = row
                .iter()
                .enumerate()
                .map(|(i, v)| value::to_text_as(v, fields[i].type_oid))
                .collect();
            self.out.data_row(&vals);
        }
        let sent = end - pending.at;
        if end < pending.rows.len() {
            p.pending = Some(PendingRows { at: end, ..pending });
            self.portals.insert(portal.to_string(), p);
            self.out.portal_suspended();
        } else {
            p.pending = None;
            self.portals.insert(portal.to_string(), p);
            self.out.command_complete(&format!("SELECT {sent}"));
        }
    }

    // --------------------------------------------------------------- execution

    fn exec_one(&mut self, sql: &str, params: &[Value]) -> Result<Answer, Error> {
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            return Ok(Answer::Empty);
        }

        // Transaction control is handled HERE rather than handed to the engine,
        // because mpedb's write session cannot span round trips without holding
        // the writer lock across them (see the module header).
        match txn_verb(trimmed) {
            Some(TxnVerb::Begin) => {
                if self.in_explicit_txn {
                    // PostgreSQL warns and continues rather than failing.
                    self.out.notice(
                        "WARNING",
                        "25001",
                        "there is already a transaction in progress",
                    );
                    return Ok(Answer::Affected("BEGIN".into()));
                }
                self.in_explicit_txn = true;
                self.txn_log.clear();
                self.status = TxnStatus::InTransaction;
                return Ok(Answer::Affected("BEGIN".into()));
            }
            Some(TxnVerb::Commit) => {
                let r = self.commit_block();
                self.in_explicit_txn = false;
                self.txn_log.clear();
                self.status = TxnStatus::Idle;
                return r.map(|()| Answer::Affected("COMMIT".into()));
            }
            Some(TxnVerb::Rollback) => {
                self.in_explicit_txn = false;
                self.txn_log.clear();
                self.status = TxnStatus::Idle;
                return Ok(Answer::Affected("ROLLBACK".into()));
            }
            None => {}
        }

        // `SET`/`SHOW` are session chatter every client sends at connect. They
        // must succeed — a client whose `SET client_encoding` fails gives up —
        // but mpedb has no GUC table, so they are accepted and, where the
        // answer is knowable, answered truthfully.
        if let Some(a) = self.session_command(trimmed)? {
            return Ok(a);
        }

        // DML is buffered; DDL is NOT.
        //
        // Replaying the block as ONE mpedb transaction at COMMIT is what keeps
        // the writer lock from being held across the client's thinking time —
        // but DDL cannot go in it. mpedb's DDL takes its OWN write transaction
        // (it bumps `schema_gen` and commits), and running it inside an already
        // open `WriteSession` self-deadlocks on the writer lock: the process
        // waits for a lock it is itself holding, forever.
        //
        // MEASURED, not theorised — `BEGIN; CREATE TEMPORARY TABLE t(a int);
        // COMMIT` hangs, while the same DDL outside a block returns at once. It
        // is `stats.sql` in the PostgreSQL corpus, and it stalled a 222-file run
        // for twelve minutes on one statement.
        //
        // So DDL executes IMMEDIATELY. The cost is named in COMPAT-PG.md: a
        // `ROLLBACK` does not undo DDL, where PostgreSQL's transactional DDL
        // would. That is a real difference, and it is the honest one to take —
        // buffering it would not have made it atomic either (mpedb commits the
        // schema change on its own), it would only have hidden the deadlock.
        // Only USER statements take the block path. A catalog query reads the
        // SCHEMA, which a transaction block never changes — DDL is not
        // buffered (see above) — so routing one through the block's replay
        // would send it to the user database, where `pg_type` does not exist.
        // Measured as an INTERNALERROR that killed the whole suite run.
        if self.in_explicit_txn
            && (is_dml(trimmed) || returns_rows(trimmed))
            && matches!(catalog::route(trimmed), catalog::Route::User)
        {
            return self.exec_in_block(trimmed, params);
        }

        let result = match catalog::route(trimmed) {
            catalog::Route::User => {
                self.db.set_dialect(Dialect::Postgres);
                self.db.query(trimmed, params)
            }
            catalog::Route::Catalog(refs) => {
                // A DDL commit — this session's or another process's — bumps
                // `schema_gen`, and the cached bundle has to be re-read or the
                // catalog answers from BEFORE it. Now that a block's DDL
                // commits through the replay, this session is the likeliest
                // one to have just changed the schema it is about to read.
                let _ = self.db.refresh_schema_if_stale();
                // The bundle carries the generation it was loaded at, so one
                // call answers both "what is the schema" and "is my catalog
                // stale" — and answers them CONSISTENTLY, which two calls
                // would not.
                //
                // Inside a block it must be the schema the block has BUILT,
                // not the committed one: now that DDL is buffered, a
                // `CREATE TABLE` earlier in the block exists only in the log,
                // and reflecting on it would otherwise report the table
                // missing. Replaying gives a transaction that HAS the DDL
                // (`WriteSession::schema` includes uncommitted DDL); the
                // catalog is read off that and the transaction is dropped.
                let bundle = self.block_schema().unwrap_or_else(|| self.db.schema());
                let rewritten = catalog::rewrite(trimmed, &refs);
                // The stored comments ride along: `pg_description` is built
                // from them and they are not part of the schema.
                let comments = self.db.list_comments().unwrap_or_default();
                let cat = self
                    .cat
                    .ensure(&bundle.schema, bundle.schema_gen, &comments)
                    .map_err(Error::Internal)?;
                cat.query(&rewritten, params)
            }
        };
        match result {
            Ok(ExecResult::Rows { columns, rows }) => Ok(Answer::Rows { columns, rows }),
            Ok(ExecResult::Affected(n)) => Ok(Answer::Affected(write_tag(trimmed, n))),
            Ok(ExecResult::Explain(text)) => Ok(Answer::Rows {
                columns: vec!["QUERY PLAN".into()],
                rows: text
                    .lines()
                    .map(|l| vec![Value::Text(l.to_string())])
                    .collect(),
            }),
            Err(e) => Err(e),
        }
    }

    /// The schema as the open block has built it, or `None` when there is no
    /// block (or its replay fails, in which case the caller falls back to the
    /// committed schema — a description must never be the thing that errors).
    fn block_schema(&mut self) -> Option<std::sync::Arc<mpedb::SchemaBundle>> {
        if !self.in_explicit_txn || self.txn_log.is_empty() {
            return None;
        }
        self.db.set_dialect(Dialect::Postgres);
        let mut w = self.db.begin().ok()?;
        for (s, p) in &self.txn_log {
            if w.query(s, p).is_err() {
                w.rollback();
                return None;
            }
        }
        let bundle = w.schema();
        w.rollback();
        Some(bundle)
    }

    /// Run one statement of an explicit transaction block, against the state
    /// the block has actually built.
    ///
    /// The block is still REPLAYED at COMMIT rather than held open across the
    /// client's think-time (see `txn_log`), so the effects of the statements
    /// before this one exist nowhere but in that log. To answer this statement
    /// correctly they have to exist SOMEWHERE, so: open a write transaction,
    /// replay the log into it, run this statement, take its answer — and
    /// ABORT. Nothing is published; the log stays the source of truth and
    /// COMMIT replays it once more, for real.
    ///
    /// This replaced two silent wrong answers, which is why it is worth what
    /// it costs:
    ///
    /// * a buffered `INSERT` used to report `Affected(0)` — a row count that
    ///   was never counted, invented so the client had something to read;
    /// * a `SELECT` inside the block ran against the PRE-transaction state, so
    ///   a client lost read-your-own-writes. The doc comment on `txn_log`
    ///   claimed the opposite ("the block still sees its own writes"), which
    ///   is how the gap survived: the property was asserted in prose and never
    ///   in a test, and `psql` — the only client the corpus runs — never
    ///   checks it, because the corpus's transactions do not read back what
    ///   they wrote.
    ///
    /// **The cost, named rather than buried: this is O(n²) over a block.** The
    /// nth statement replays the n−1 before it. For an ORM's transactions
    /// (a handful of statements) that is nothing; for a block of thousands it
    /// is a wall, and the fix for that is to hold one `WriteSession` open
    /// across the block instead — which needs the session restructured, since
    /// a `WriteSession` borrows the `Database` this struct owns.
    ///
    /// A statement that FAILS here fails now, at the statement that caused it,
    /// rather than at COMMIT. That is closer to PostgreSQL than the behaviour
    /// it replaces, and it is why the failed statement is not appended: the
    /// block continues from the last state that worked.
    fn exec_in_block(&mut self, sql: &str, params: &[Value]) -> Result<Answer, Error> {
        self.db.set_dialect(Dialect::Postgres);
        let mut w = self.db.begin()?;
        let replay = (|| -> Result<ExecResult, Error> {
            for (s, p) in &self.txn_log {
                w.query(s, p)?;
            }
            w.query(sql, params)
        })();
        // ABORT either way. On success the log — not the engine — carries the
        // block forward; on failure there is nothing to keep.
        w.rollback();
        let result = replay?;
        if is_dml(sql) {
            self.txn_log.push((sql.to_string(), params.to_vec()));
        }
        Ok(match result {
            ExecResult::Rows { columns, rows } => Answer::Rows { columns, rows },
            ExecResult::Affected(n) => Answer::Affected(write_tag(sql, n)),
            ExecResult::Explain(text) => Answer::Rows {
                columns: vec!["QUERY PLAN".into()],
                rows: text.lines().map(|l| vec![Value::Text(l.to_string())]).collect(),
            },
        })
    }

    /// Replay a buffered transaction block as ONE mpedb write transaction.
    fn commit_block(&mut self) -> Result<(), Error> {
        if self.txn_log.is_empty() {
            return Ok(());
        }
        self.db.set_dialect(Dialect::Postgres);
        let mut w = self.db.begin()?;
        for (sql, params) in &self.txn_log {
            w.query(sql, params)?;
        }
        w.commit()
    }

    /// `SET`, `SHOW`, `RESET`, `DISCARD` and the other things a driver says
    /// before it says anything interesting.
    fn session_command(&mut self, sql: &str) -> Result<Option<Answer>, Error> {
        let head = sql
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match head.as_str() {
            "SET" => Ok(Some(Answer::Affected("SET".into()))),
            "RESET" => Ok(Some(Answer::Affected("RESET".into()))),
            "DISCARD" => {
                self.prepared.clear();
                self.portals.clear();
                Ok(Some(Answer::Affected("DISCARD ALL".into())))
            }
            "SHOW" => {
                let name = sql
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .trim_end_matches(';')
                    .to_ascii_lowercase();
                let val = match name.as_str() {
                    "server_version" => self.server_version.clone(),
                    "server_encoding" | "client_encoding" => "UTF8".into(),
                    "transaction_isolation" => "serializable".into(),
                    "timezone" => "UTC".into(),
                    "standard_conforming_strings" => "on".into(),
                    "search_path" => "public".into(),
                    // An unknown GUC reads as empty rather than erroring: a
                    // driver probing for a setting it might use should learn
                    // "not set", not "your server is broken".
                    _ => String::new(),
                };
                Ok(Some(Answer::Rows {
                    columns: vec![name],
                    rows: vec![vec![Value::Text(val)]],
                }))
            }
            _ => Ok(None),
        }
    }

    /// Build the `RowDescription` for a result.
    ///
    /// Two sources, in this order:
    ///
    /// 1. **The DECLARED type**, where the plan can name one — an output column
    ///    that is a bare reference to a base-table column
    ///    (`CompiledPlan::output_decltypes`). This is the authority, because
    ///    mpedb's STORAGE type is coarser than PostgreSQL's: `uuid` and `bytea`
    ///    are both `Blob`, so a value alone reports `bytea` for a `uuid`
    ///    column and the client hands back bytes where it was promised a UUID.
    ///    It also answers for an EMPTY result, which a value never can.
    /// 2. Otherwise the first non-NULL VALUE in the column — exact for
    ///    anything computed, which has no declared type to read.
    ///
    /// An all-NULL, empty, computed column reports `text`, which is what
    /// PostgreSQL reports for an untyped NULL literal too.
    fn describe_rows(&self, sql: &str, columns: &[String], rows: &[Vec<Value>]) -> Vec<FieldDesc> {
        // A statement the planner cannot compile here (a catalog query is
        // REWRITTEN before execution, so the original text names tables this
        // does not see) simply yields no declared types and falls through to
        // the value — never an error, since this is only a description.
        let declared: Vec<Option<u32>> = self
            .db
            .output_decltypes(sql)
            .map(|v| {
                v.into_iter()
                    .map(|d| {
                        d.as_deref()
                            .and_then(mpedb_types::pgtype::by_name)
                            .map(|t| t.oid)
                    })
                    .collect()
            })
            .unwrap_or_default();
        columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let ty = rows
                    .iter()
                    .find_map(|r| r.get(i).and_then(|v| v.column_type()));
                // An ARRAY has no `ColumnType` (it is not storable), so it
                // cannot come from either source above — the element type
                // decides which array type it is, and a client decodes by it.
                // A DECLARED type still wins where there is one: `indkey` is an
                // int2vector, which is an array PostgreSQL prints unbraced.
                let array = rows.iter().find_map(|r| match r.get(i) {
                    Some(Value::List(items)) => Some(
                        declared
                            .get(i)
                            .copied()
                            .flatten()
                            .unwrap_or_else(|| value::array_oid_for(items)),
                    ),
                    _ => None,
                });
                let oid = array
                    .or_else(|| declared.get(i).copied().flatten())
                    .or_else(|| ty.map(mpedb_types::pgtype::default_oid))
                    .unwrap_or(25);
                let type_len = mpedb_types::pgtype::by_oid(oid)
                    .map(|t| t.typlen)
                    .unwrap_or(-1);
                FieldDesc {
                    name: name.clone(),
                    table_oid: 0,
                    column_id: 0,
                    type_oid: oid,
                    type_len,
                    type_mod: -1,
                    format: 0,
                }
            })
            .collect()
    }

    fn fail(&mut self, e: &Error) {
        let (code, msg) = errmap::sqlstate(e);
        self.fail_str(code, &msg);
    }

    fn fail_str(&mut self, code: &str, msg: &str) {
        self.out.error(errmap::SEVERITY_ERROR, code, msg);
        if self.in_explicit_txn {
            // PostgreSQL puts the whole block in the failed state: everything
            // but ROLLBACK is refused until the block ends. Not reproducing
            // that lets a client commit a block whose middle failed.
            self.status = TxnStatus::Failed;
        }
    }
}

enum Answer {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Affected(String),
    Empty,
}

enum TxnVerb {
    Begin,
    Commit,
    Rollback,
}

fn txn_verb(sql: &str) -> Option<TxnVerb> {
    let head = sql
        .split(|c: char| c.is_whitespace() || c == ';')
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_ascii_uppercase();
    match head.as_str() {
        "BEGIN" | "START" => Some(TxnVerb::Begin),
        "COMMIT" | "END" => Some(TxnVerb::Commit),
        "ROLLBACK" | "ABORT" => Some(TxnVerb::Rollback),
        _ => None,
    }
}

fn is_txn_end(sql: &str) -> bool {
    matches!(txn_verb(sql), Some(TxnVerb::Commit) | Some(TxnVerb::Rollback))
}

/// A write that may be BUFFERED into a transaction block.
///
/// DML only. DDL is deliberately absent — see `exec_one` for the deadlock that
/// buys.
/// **KNOWN DIVERGENCE — DDL is NOT transactional here.**
///
/// PostgreSQL's DDL is transactional: `BEGIN; CREATE TABLE t …; ROLLBACK`
/// leaves no `t`. mpedb-pg applies DDL IMMEDIATELY instead of buffering it
/// into the block, so a rolled-back `CREATE TABLE` survives and the next
/// statement wanting that name gets `duplicate table name` — 52 of them in one
/// SQLAlchemy suite run.
///
/// Buffering it was BUILT and MEASURED, and is not the whole fix:
///
/// * `COMMENT ON` had to become transactional first (it opened its own
///   transaction and committed) — done, and kept: `WriteSession` now carries
///   it, so that half is no longer in the way.
/// * A block's own catalog queries must see the buffered DDL, or reflection
///   inside the block reports the table missing. Also solved, by reading the
///   catalog off a replay of the log rather than the committed schema.
/// * What is NOT solved: `CREATE INDEX` in the reflection fixtures then failed
///   with `duplicate table name` 749 times, and the suite went 324 → 347
///   passing but 82 → 508 regressed. The cause is somewhere in how those
///   fixtures interleave buffered and immediate statements, and it is not
///   understood yet.
///
/// Shipping it in that state would trade one wrong answer for a much larger
/// set, so it stays reverted with the pieces that stand on their own kept.
fn is_dml(sql: &str) -> bool {
    let head = first_word(sql);
    matches!(head.as_str(), "INSERT" | "UPDATE" | "DELETE" | "TRUNCATE")
}

fn returns_rows(sql: &str) -> bool {
    let head = first_word(sql);
    if matches!(head.as_str(), "SELECT" | "WITH" | "VALUES" | "SHOW" | "EXPLAIN") {
        return true;
    }
    // A DML statement with RETURNING produces rows too, and a client that is
    // told NoData for one will not read them.
    sql.to_ascii_uppercase().contains(" RETURNING ")
}

fn first_word(sql: &str) -> String {
    sql.split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// The CommandComplete tag for a write.
///
/// `INSERT` carries an OID field that has been 0 since PostgreSQL 12 but is
/// still POSITIONAL — clients split the tag on spaces, so `INSERT 5` (without
/// the 0) is read as an OID of 5 and a row count of nothing.
fn write_tag(sql: &str, n: u64) -> String {
    match first_word(sql).as_str() {
        "INSERT" => format!("INSERT 0 {n}"),
        "UPDATE" => format!("UPDATE {n}"),
        "DELETE" => format!("DELETE {n}"),
        // A DDL tag names the OBJECT KIND, and PostgreSQL's own clients read
        // it: `psql` echoes it, and a migration tool checks it to know what it
        // just created. `CREATE FUNCTION` reported as `CREATE TABLE` is a wrong
        // answer to a question the protocol asks — cheap to get right, since
        // the kind is the statement's second word.
        "CREATE" | "DROP" | "ALTER" => ddl_tag(sql),
        "TRUNCATE" => "TRUNCATE TABLE".into(),
        other => other.to_string(),
    }
}

/// `CREATE [OR REPLACE] <KIND> …` → `CREATE <KIND>`.
///
/// Falls back to TABLE for a kind mpedb does not implement: those statements
/// are refused before they reach here, so the fallback is unreachable in
/// practice and only has to be a legal tag rather than a correct one.
fn ddl_tag(sql: &str) -> String {
    let mut w = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase());
    let verb = w.next().unwrap_or_else(|| "CREATE".into());
    let mut kind = w.next().unwrap_or_default();
    // `CREATE OR REPLACE VIEW`, and `CREATE UNIQUE INDEX` — the kind is not
    // always the second word.
    if kind == "OR" {
        let _ = w.next(); // REPLACE
        kind = w.next().unwrap_or_default();
    }
    if kind == "UNIQUE" || kind == "TEMPORARY" || kind == "TEMP" || kind == "VIRTUAL" {
        kind = w.next().unwrap_or_default();
    }
    match kind.as_str() {
        "TABLE" | "INDEX" | "VIEW" | "TRIGGER" | "FUNCTION" | "POLICY" | "SCHEMA" | "SEQUENCE" => {
            format!("{verb} {kind}")
        }
        _ => format!("{verb} TABLE"),
    }
}

/// Split a simple-query string into statements on top-level semicolons.
///
/// Quotes and dollar-quoted bodies are respected, because a `;` inside a string
/// literal is not a separator and splitting there produces two statements that
/// are each a syntax error.
fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        // `''` is an escaped quote, not the end.
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'$' => {
                // A dollar-quoted body can contain anything, semicolons above
                // all — this is how every stored function body arrives.
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if b.get(j) == Some(&b'$') && !b[i + 1..j].first().is_some_and(u8::is_ascii_digit) {
                    let tag = &b[i..=j];
                    let mut k = j + 1;
                    while k + tag.len() <= b.len() && &b[k..k + tag.len()] != tag {
                        k += 1;
                    }
                    i = (k + tag.len()).min(b.len());
                } else {
                    i += 1;
                }
            }
            b';' => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    out.push(stmt.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let tail = sql[start.min(sql.len())..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

fn is_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_top_level_semicolons_only() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
        // A semicolon inside a literal is not a separator; splitting there
        // turns one valid statement into two syntax errors.
        assert_eq!(
            split_statements("SELECT 'a;b'"),
            vec!["SELECT 'a;b'"]
        );
        assert_eq!(
            split_statements("SELECT 'it''s; fine'"),
            vec!["SELECT 'it''s; fine'"]
        );
        assert_eq!(
            split_statements("SELECT \"we;ird\" FROM t"),
            vec!["SELECT \"we;ird\" FROM t"]
        );
        assert_eq!(
            split_statements("SELECT 1 -- a; comment\n; SELECT 2"),
            vec!["SELECT 1 -- a; comment", "SELECT 2"]
        );
        // A dollar-quoted body is how every function definition arrives.
        assert_eq!(
            split_statements("SELECT $$a;b$$; SELECT 2"),
            vec!["SELECT $$a;b$$", "SELECT 2"]
        );
        assert!(split_statements("   ;  ; ").is_empty());
        assert!(split_statements("").is_empty());
    }

    #[test]
    fn the_insert_tag_keeps_its_positional_oid_field() {
        // Clients split the tag on spaces. `INSERT 5` is read as an OID of 5
        // and NO row count, which makes an ORM think nothing was written.
        assert_eq!(write_tag("INSERT INTO t VALUES (1)", 5), "INSERT 0 5");
        assert_eq!(write_tag("UPDATE t SET a = 1", 3), "UPDATE 3");
        assert_eq!(write_tag("DELETE FROM t", 0), "DELETE 0");
        assert_eq!(write_tag("create table t (a int)", 0), "CREATE TABLE");
        // The tag names the object KIND — clients read it.
        assert_eq!(write_tag("CREATE FUNCTION f() RETURNS int AS $$ $$", 0), "CREATE FUNCTION");
        assert_eq!(
            write_tag("CREATE OR REPLACE FUNCTION f() RETURNS int AS $$ $$", 0),
            "CREATE FUNCTION"
        );
        assert_eq!(write_tag("CREATE OR REPLACE VIEW v AS SELECT 1", 0), "CREATE VIEW");
        assert_eq!(write_tag("CREATE UNIQUE INDEX i ON t (a)", 0), "CREATE INDEX");
        assert_eq!(write_tag("CREATE TEMPORARY TABLE t (a int)", 0), "CREATE TABLE");
        assert_eq!(write_tag("DROP FUNCTION f", 0), "DROP FUNCTION");
        assert_eq!(write_tag("ALTER TABLE t ADD COLUMN b int", 0), "ALTER TABLE");
    }

    #[test]
    fn transaction_verbs_are_recognised_in_every_spelling_clients_use() {
        assert!(matches!(txn_verb("BEGIN"), Some(TxnVerb::Begin)));
        assert!(matches!(txn_verb("begin transaction"), Some(TxnVerb::Begin)));
        assert!(matches!(txn_verb("START TRANSACTION"), Some(TxnVerb::Begin)));
        assert!(matches!(txn_verb("COMMIT"), Some(TxnVerb::Commit)));
        // `END` is COMMIT in PostgreSQL, which surprises people reading it as
        // the end of a block.
        assert!(matches!(txn_verb("END"), Some(TxnVerb::Commit)));
        assert!(matches!(txn_verb("ROLLBACK"), Some(TxnVerb::Rollback)));
        assert!(matches!(txn_verb("ABORT"), Some(TxnVerb::Rollback)));
        assert!(txn_verb("SELECT 1").is_none());
        // …and `BEGINNING` is not `BEGIN`: the verb is matched as a WHOLE
        // first word, not as a prefix. A prefix match would turn a column
        // called `beginning` into a transaction start.
        assert!(txn_verb("BEGINNING").is_none());
        assert!(txn_verb("COMMITTED").is_none());
    }

    #[test]
    fn a_dml_statement_with_returning_is_known_to_produce_rows() {
        // Describe answers NoData for a write; a write with RETURNING that got
        // NoData would have its rows never read.
        assert!(returns_rows("SELECT 1"));
        assert!(returns_rows("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(returns_rows("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(!returns_rows("INSERT INTO t VALUES (1)"));
        assert!(!returns_rows("UPDATE t SET a = 1"));
    }

    #[test]
    fn only_dml_is_buffered_into_a_transaction_block_never_ddl() {
        // DDL inside the buffered block self-deadlocks: mpedb's DDL takes its
        // own write transaction, and running one inside an already-open
        // WriteSession waits on a lock this process is holding. Measured with
        // `BEGIN; CREATE TEMPORARY TABLE t(a int); COMMIT`, which hung until
        // this split existed.
        assert!(is_dml("INSERT INTO t VALUES (1)"));
        assert!(is_dml("UPDATE t SET a = 1"));
        assert!(is_dml("DELETE FROM t"));
        assert!(is_dml("truncate t"));
        assert!(!is_dml("create table t (a int)"));
        assert!(!is_dml("CREATE TEMPORARY TABLE t (a int)"));
        assert!(!is_dml("DROP TABLE t"));
        assert!(!is_dml("ALTER TABLE t ADD COLUMN b int"));
        assert!(!is_dml("SELECT 1"));
        assert!(!is_dml("WITH x AS (SELECT 1) SELECT * FROM x"));
    }
}
