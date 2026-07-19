//! The v2 delta overlay — DESIGN-SQLITE-BACKED §1/§2/§3/§5/§6.
//! The `.db` stays the durable home; `<base>.overlay.mpedb` holds ONLY what
//! changed since the last checkpoint: upserted row images and TOMBSTONES.
//! Reads merge per PK — overlay shadows base, tombstones suppress — with the
//! base read through the native reader under the mode's lock discipline
//! (LOCKED / OPTIMISTIC / UNLOCKED-OFFLINE, see [`LockMode`]). The
//! checkpoint (§5, behind the `sqlite-checkpoint` feature) pushes deltas
//! into the base via the sqlite library and empties the overlay.
//!
//! Layout contract: the overlay's physical tables are the attach-derived
//! user tables PLUS two trailing hidden columns — `__dead` (bool tombstone
//! marker) and `__pre` (the BASE's row image for this PK, captured at the
//! FIRST delta write, atomically with it). The executor never sees either:
//! plans compile against the USER schema and the merge context
//! strips/interprets them at the TxnCtx boundary.
//!
//! `__pre` is what makes RECONCILE honest: `Blob([1] ‖ canonical row)` =
//! the base row as it was, `Blob([0])` = no base row existed, `Null` =
//! captured offline (unknown). At reconcile, a delta whose `__pre` still
//! equals the CURRENT base row is provably conflict-free (the foreign
//! writer touched other PKs); anything else is a per-PK conflict resolved
//! by the caller's named policy — ours or theirs, counted and reported,
//! never silently merged.
//!
//! A hot journal anywhere is a named refusal telling the fix (`sqlite3
//! base.db 'SELECT 1'` runs sqlite's own recovery); this crate never rolls
//! journals back.

use std::path::{Path, PathBuf};

use mpedb_sqlitefmt as fmtx;
use mpedb_sqlitefmt::lock::{hot_journal, BracketOutcome, ReadBracket, SharedLock};
use mpedb_sqlitefmt::stamp::{settle_and_read, BaseStamp};
use mpedb_types::{ColumnType, Config, Error, Result, Schema, Value};

use crate::exec::{exec_stmt, ReadCtx, TxnCtx};
use crate::sqlite_attach::SqliteAttach;
use crate::{Database, ExecResult};

const STAMP_NS: &str = "ovl";
const STAMP_KEY: &[u8] = b"base-stamp";
const EPOCH_KEY: &[u8] = b"epoch";
/// The checkpoint marker table in the BASE (design §5 [R#4]: "was epoch E
/// pushed?" must be readable from the base itself, atomically with the push).
const MARKER_TABLE: &str = "_mpedb_overlay_state";
/// Truncation batch size [R#14]: deleting delta rows is COW — each batch
/// commits (and frees) before the next allocates.
const TRUNCATE_BATCH: usize = 512;
/// Trailing hidden columns on every overlay row: `__dead`, `__pre`.
const HIDDEN_COLS: usize = 2;

/// Per-conflict resolution for [`SqliteOverlay::reconcile`]: what wins when
/// a foreign writer changed a base row we hold an unpushed delta for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcilePolicy {
    /// Our delta stays and overwrites theirs at the next checkpoint.
    Ours,
    /// The base wins: our delta is dropped, their row shows through.
    Theirs,
}

/// What one reconcile did, per delta: provably conflict-free (`unchanged`),
/// kept over theirs (`ours`), or dropped in their favor (`theirs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub unchanged: u64,
    pub ours: u64,
    pub theirs: u64,
}

/// How the base is held between statements — design §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Default: this handle holds sqlite's own SHARED for its lifetime.
    /// Foreign writers get `SQLITE_BUSY`; the fast path needs zero
    /// validation; mpedb's full snapshot contract holds.
    Locked,
    /// No standing lock: a transient SHARED bracket per STATEMENT (busy →
    /// bounded backoff, never divergence [R#19]; hot journal → named
    /// refusal), with the settled stamp compared under the bracket. Foreign
    /// writers run freely between our statements; a moved base with an
    /// EMPTY overlay is adopted in place, with unpushed deltas it refuses
    /// by name. Base-side isolation weakens to per-statement [R#12] — an
    /// application needing the full snapshot contract uses `Locked`.
    Optimistic,
    /// Cooperative window for bulk foreign rewrites: no lock, and every
    /// base fall-through refused by name — statements resolvable entirely
    /// in the overlay (point reads/updates of overlay-resident rows) still
    /// work. Checkpoint refuses; reopen in another mode ends the window.
    Offline,
}

pub struct SqliteOverlay {
    attach: SqliteAttach,
    db: Database,
    mode: LockMode,
    /// LOCKED: held for the handle's lifetime, so the base provably cannot
    /// move under the merge (`None` after a checkpoint failed to re-take it
    /// — the handle then refuses queries by name until reopened). Other
    /// modes: always `None` between statements.
    lock: Option<SharedLock>,
    /// The settled stamp the deltas are valid against — mirrors the stored
    /// record; compared per statement in OPTIMISTIC.
    expected: BaseStamp,
    base: PathBuf,
    /// Per user-visible table: the PK column index (single int64 in every
    /// attachable shape; same index in user rows and overlay rows, since
    /// `__dead` trails).
    pk_idx: Vec<usize>,
}

fn overlay_path(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    // `.overlay.mpedb`, NOT `.mpedb`: the v0 sidecar (full-copy mirror) owns
    // that name, and clobbering it would silently orphan its mirror state.
    s.push(".overlay.mpedb");
    PathBuf::from(s)
}

fn scratch_path(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".overlay.probe");
    PathBuf::from(s)
}

/// The overlay's physical schema as config TOML: the attach-derived user
/// tables + the trailing `__dead` marker. Durability `none` for now — the
/// base stays the durable home; losing the overlay to power loss loses
/// recent deltas, never consistency (block 7 makes this configurable).
fn overlay_toml(base: &Path, schema: &Schema, size_mb: u64) -> String {
    use std::fmt::Write as _;
    let mut t = String::new();
    let _ = write!(
        t,
        "[database]\npath = \"{}\"\nsize_mb = {size_mb}\ndurability = \"none\"\n",
        overlay_path(base).display()
    );
    for table in &schema.tables {
        let pk = &table.columns[table.primary_key[0] as usize].name;
        let _ = write!(t, "\n[[table]]\nname = \"{}\"\nprimary_key = [\"{pk}\"]\n", table.name);
        for c in &table.columns {
            let ty = match c.ty {
                ColumnType::Int64 => "int64",
                // Text PKs (WITHOUT ROWID shapes) must be `text`, not `any`:
                // the keycode order of a typed text column is what keeps the
                // overlay's scan order aligned with the base's BINARY order.
                ColumnType::Text => "text",
                _ => "any",
            };
            let nullable = !matches!(c.ty, ColumnType::Int64 | ColumnType::Text);
            let _ = write!(
                t,
                "\n  [[table.column]]\n  name = \"{}\"\n  type = \"{ty}\"\n  nullable = {nullable}\n",
                c.name
            );
        }
        let _ = write!(
            t,
            "\n  [[table.column]]\n  name = \"__dead\"\n  type = \"bool\"\n  nullable = false\n"
        );
        let _ = write!(
            t,
            "\n  [[table.column]]\n  name = \"__pre\"\n  type = \"any\"\n"
        );
    }
    t
}

fn encode_stamp(s: &BaseStamp) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    let d = s
        .mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    out.extend_from_slice(&d.as_secs().to_le_bytes());
    out.extend_from_slice(&d.subsec_nanos().to_le_bytes());
    out.extend_from_slice(&s.size.to_le_bytes());
    out.extend_from_slice(&s.change_counter.to_le_bytes());
    out.extend_from_slice(&s.schema_cookie.to_le_bytes());
    out.extend_from_slice(&s.format_versions);
    match &s.wal {
        None => out.push(0),
        Some((salts, len)) => {
            out.push(1);
            out.extend_from_slice(salts);
            out.extend_from_slice(&len.to_le_bytes());
        }
    }
    out
}

fn decode_stamp(b: &[u8]) -> Result<BaseStamp> {
    let err = || Error::Corrupt("stored base-stamp is malformed".into());
    let g = |r: std::ops::Range<usize>| b.get(r).ok_or_else(err);
    let secs = u64::from_le_bytes(g(0..8)?.try_into().expect("8"));
    let nanos = u32::from_le_bytes(g(8..12)?.try_into().expect("4"));
    let size = u64::from_le_bytes(g(12..20)?.try_into().expect("8"));
    let change_counter = u32::from_le_bytes(g(20..24)?.try_into().expect("4"));
    let schema_cookie = u32::from_le_bytes(g(24..28)?.try_into().expect("4"));
    let format_versions = [*b.get(28).ok_or_else(err)?, *b.get(29).ok_or_else(err)?];
    let wal = match *b.get(30).ok_or_else(err)? {
        0 => None,
        1 => {
            let salts: [u8; 8] = g(31..39)?.try_into().expect("8");
            Some((salts, u64::from_le_bytes(g(39..47)?.try_into().expect("8"))))
        }
        _ => return Err(err()),
    };
    Ok(BaseStamp {
        mtime: std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos),
        size,
        change_counter,
        schema_cookie,
        format_versions,
        wal,
    })
}

fn oerr(e: fmtx::Error) -> Error {
    match e {
        fmtx::Error::Io(e) => Error::Io(e),
        fmtx::Error::Corrupt(m) => Error::Corrupt(format!("sqlite: {m}")),
        fmtx::Error::Unsupported(m) => Error::Unsupported(format!("sqlite: {m}")),
    }
}

impl SqliteOverlay {
    /// Open the base in LOCKED mode with its delta overlay beside it.
    pub fn open(base: &Path) -> Result<SqliteOverlay> {
        Self::open_with_mode(base, LockMode::Locked)
    }

    /// Open with an explicit lock mode. Every mode takes ONE transient
    /// SHARED here — attach snapshot, stamp validation/settling, and
    /// recovery all need the base provably quiescent once — and then keeps
    /// it only in `Locked`.
    pub fn open_with_mode(base: &Path, mode: LockMode) -> Result<SqliteOverlay> {
        Self::open_with_options(base, mode, None)
    }

    /// [`Self::open_with_mode`] plus a divergence policy: when the base
    /// moved under unpushed deltas (the case plain open refuses by name),
    /// `Some(policy)` runs [`Self::reconcile`] with it before returning.
    pub fn open_with_options(
        base: &Path,
        mode: LockMode,
        on_divergence: Option<ReconcilePolicy>,
    ) -> Result<SqliteOverlay> {
        let Some(lock) = SharedLock::acquire(base).map_err(oerr)? else {
            return Err(Error::Unsupported(
                "the sqlite database is busy (a writer is draining readers) — retry".into(),
            ));
        };
        if hot_journal(base).map_err(oerr)? {
            return Err(Error::Unsupported(format!(
                "hot journal beside {} — a crashed sqlite writer left it; run \
                 `sqlite3 {} 'SELECT 1'` once so sqlite's own recovery rolls it back",
                base.display(),
                base.display()
            )));
        }
        let attach = SqliteAttach::open(base)?;
        if !attach.skipped().is_empty() {
            // Strict for now: a write mode with silently unwritable tables is
            // a trap. Block 7 revisits (read-only pass-through for the rest).
            return Err(Error::Unsupported(format!(
                "tables not attachable under the v2 shape rules: {:?}",
                attach.skipped()
            )));
        }
        let pk_idx: Vec<usize> = attach
            .schema()
            .tables
            .iter()
            .map(|t| t.primary_key[0] as usize)
            .collect();
        let ovl = overlay_path(base);
        let mut needs_reconcile = false;
        let db = if ovl.exists() {
            let db = Database::open_from_file(&ovl)?;
            // The stored settled stamp must still describe the base — else
            // the deltas were captured against a base that has since moved.
            let stored = db
                .sys_record_get(STAMP_NS, STAMP_KEY)?
                .ok_or_else(|| Error::Corrupt("overlay has no stored base-stamp".into()))?;
            let stored = decode_stamp(&stored)?;
            if !stored.matches(base).map_err(oerr)? {
                match recover_after_crashed_checkpoint(base, &db, &attach, &pk_idx)? {
                    Recovery::Healed => {}
                    Recovery::Diverged { why } => {
                        if on_divergence.is_none() {
                            return Err(Error::Unsupported(format!(
                                "the base {} changed since this overlay's deltas were \
                                 captured ({why}) — reconcile with a policy \
                                 (ours/theirs) or discard {}",
                                base.display(),
                                ovl.display()
                            )));
                        }
                        needs_reconcile = true;
                    }
                }
            }
            db
        } else {
            let cfg = Config::from_toml_str(&overlay_toml(base, attach.schema(), 128))?;
            let db = Database::open_with_config(cfg)?;
            // Settle UNDER the held SHARED — the §3 trick: the base is
            // provably quiescent while the file clock crosses its tick, so
            // the stamp stays a trustworthy change detector across every
            // later unlocked window.
            let stamp = settle_and_read(base, &scratch_path(base)).map_err(oerr)?;
            let _ = std::fs::remove_file(scratch_path(base));
            db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
            db.sys_record_put(STAMP_NS, EPOCH_KEY, &1u64.to_le_bytes())?;
            db
        };
        let expected = decode_stamp(
            &db.sys_record_get(STAMP_NS, STAMP_KEY)?
                .ok_or_else(|| Error::Corrupt("overlay has no stored base-stamp".into()))?,
        )?;
        // Keep the open lock through a possible reconcile — it needs the
        // quiescence; the mode decides afterwards whether it stays.
        let mut handle = SqliteOverlay {
            attach,
            db,
            mode,
            lock: Some(lock),
            expected,
            base: base.to_path_buf(),
            pk_idx,
        };
        if needs_reconcile {
            handle.reconcile(on_divergence.expect("checked above"))?;
        }
        if mode != LockMode::Locked {
            handle.lock = None;
        }
        Ok(handle)
    }

    pub fn schema(&self) -> &Schema {
        self.attach.schema()
    }

    /// The LOCKED-mode guard: every merged read/write requires the held
    /// SHARED. `None` means a checkpoint left the handle detached (retake
    /// failed, or divergence was detected under the drop window).
    fn ensure_locked(&self) -> Result<()> {
        if self.lock.is_none() {
            return Err(Error::Unsupported(
                "the overlay is no longer holding the base's SHARED lock (a checkpoint \
                 detached it) — reopen to recover"
                    .into(),
            ));
        }
        Ok(())
    }

    #[cfg_attr(not(feature = "sqlite-checkpoint"), allow(dead_code))]
    fn epoch(&self) -> Result<u64> {
        read_epoch(&self.db)
    }

    /// Compile + run one statement over the MERGED view, under the mode's
    /// base discipline. Reads run on an overlay read transaction
    /// (lock-free); writes run in one overlay write transaction, committed
    /// only on success. `&mut self` because OPTIMISTIC may adopt a moved
    /// base in place (fresh attach snapshot + stamp).
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        let (plan, is_explain) = mpedb_sql::prepare_maybe_explain(sql, self.attach.schema())?;
        if is_explain {
            return Ok(ExecResult::Rows {
                columns: vec!["plan".into()],
                rows: plan
                    .explain(self.attach.schema())
                    .lines()
                    .map(|l| vec![Value::Text(l.to_string())])
                    .collect(),
            });
        }
        match self.mode {
            LockMode::Locked => {
                self.ensure_locked()?;
                self.exec_plan(&plan, params, true)
            }
            LockMode::Offline => self.exec_plan(&plan, params, false),
            LockMode::Optimistic => {
                // The §2 bracket: results buffer inside exec and are only
                // returned after the bracket closes — nothing streams out
                // of an unvalidated read.
                let bracket = self.open_bracket()?;
                let result = self.exec_plan(&plan, params, true);
                drop(bracket);
                result
            }
        }
    }

    /// The OPTIMISTIC per-statement bracket: transient SHARED with bounded
    /// busy-backoff (busy is a WRITER, never divergence [R#19]), hot-journal
    /// refusal by name, and the stamp compared UNDER the bracket. A moved
    /// base adopts in place when the overlay is empty; with unpushed deltas
    /// it refuses (reconcile is post-v2).
    fn open_bracket(&mut self) -> Result<ReadBracket> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match ReadBracket::open(&self.base).map_err(oerr)? {
                BracketOutcome::Busy => {
                    if std::time::Instant::now() > deadline {
                        return Err(Error::Unsupported(
                            "base busy: a sqlite writer held the base for the whole 2s \
                             backoff window — retry the statement"
                                .into(),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                BracketOutcome::HotJournal => {
                    return Err(Error::Unsupported(format!(
                        "hot journal beside {} — a crashed sqlite writer left it; run \
                         `sqlite3 {} 'SELECT 1'` once so sqlite's own recovery rolls it \
                         back",
                        self.base.display(),
                        self.base.display()
                    )));
                }
                BracketOutcome::Held(b) => {
                    if b.stamp_matches(&self.expected).map_err(oerr)? {
                        return Ok(b);
                    }
                    // Our in-memory stamp is stale — but a CO-ATTACHED
                    // process may have moved the base legitimately (its
                    // checkpoint/reconcile re-blessed it and stored the
                    // fresh stamp in the shared overlay). Re-read the
                    // stored one before calling this divergence.
                    let stored = decode_stamp(
                        &self
                            .db
                            .sys_record_get(STAMP_NS, STAMP_KEY)?
                            .ok_or_else(|| {
                                Error::Corrupt("overlay has no stored base-stamp".into())
                            })?,
                    )?;
                    if stored != self.expected && b.stamp_matches(&stored).map_err(oerr)? {
                        let fresh = SqliteAttach::open(&self.base)?;
                        if fresh.schema() != self.attach.schema() {
                            return Err(Error::Unsupported(
                                "foreign DDL changed the base's schema — reopen to \
                                 re-derive the attach (plans and table ids shift)"
                                    .into(),
                            ));
                        }
                        self.attach = fresh;
                        self.expected = stored;
                        return Ok(b);
                    }
                    let deltas = snapshot_deltas(&self.db, self.pk_idx.len())?;
                    if !deltas.iter().all(|t| t.is_empty()) {
                        return Err(Error::Unsupported(format!(
                            "the base {} changed under this overlay's unpushed deltas — \
                             run reconcile with a policy (ours/theirs) to resume",
                            self.base.display()
                        )));
                    }
                    // Empty overlay: nothing was captured against the old
                    // base — adopt the new one under this bracket's SHARED.
                    let stamp =
                        settle_and_read(&self.base, &scratch_path(&self.base)).map_err(oerr)?;
                    let _ = std::fs::remove_file(scratch_path(&self.base));
                    let fresh = SqliteAttach::open(&self.base)?;
                    if fresh.schema() != self.attach.schema() {
                        return Err(Error::Unsupported(
                            "foreign DDL changed the base's schema — reopen to re-derive \
                             the attach (plans and table ids shift)"
                                .into(),
                        ));
                    }
                    self.db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
                    self.attach = fresh;
                    self.expected = stamp;
                    return Ok(b);
                }
            }
        }
    }

    /// Row-level reconcile of unpushed deltas against a moved base — design
    /// §2's "pause fall-through, reconcile, resume on the new stamp". Per
    /// delta PK, `__pre` proves whether the base row actually changed since
    /// capture: unchanged → the delta is conflict-free and stays; changed
    /// (or offline-captured unknown) → `policy` decides. `Ours` keeps the
    /// delta and refreshes its `__pre` to the CURRENT base row, so the next
    /// foreign write is judged against the state this reconcile accepted;
    /// `Theirs` drops the delta and their row shows through. Ends by
    /// settling a fresh stamp under the lock and refreshing the attach
    /// snapshot — the handle resumes normally, and a detached LOCKED handle
    /// re-arms.
    pub fn reconcile(&mut self, policy: ReconcilePolicy) -> Result<ReconcileReport> {
        if self.mode == LockMode::Offline {
            return Err(Error::Unsupported(
                "reconcile reads the base — refuse in unlocked-offline mode; reopen in \
                 locked/optimistic mode first"
                    .into(),
            ));
        }
        // Quiescence: the held SHARED, or a transient one for the duration.
        let transient = if self.lock.is_none() {
            let l = retake_shared(&self.base)?;
            if hot_journal(&self.base).map_err(oerr)? {
                return Err(Error::Unsupported(format!(
                    "hot journal beside {} — run `sqlite3 {} 'SELECT 1'` once so \
                     sqlite's own recovery rolls it back",
                    self.base.display(),
                    self.base.display()
                )));
            }
            Some(l)
        } else {
            None
        };
        // Fresh base view FIRST — the reader snapshots at open, and every
        // comparison below must be against the base as it is NOW.
        let fresh = SqliteAttach::open(&self.base)?;
        if fresh.schema() != self.attach.schema() {
            return Err(Error::Unsupported(
                "foreign DDL changed the base's schema — reconcile cannot proceed; \
                 reopen to re-derive the attach"
                    .into(),
            ));
        }
        self.attach = fresh;
        enum Act {
            RefreshPre(u32, Vec<Value>),
            Drop(u32, Value),
        }
        let deltas = snapshot_deltas(&self.db, self.pk_idx.len())?;
        let mut report = ReconcileReport::default();
        let mut acts = Vec::new();
        for (ti, rows) in deltas.iter().enumerate() {
            let idx = self.pk_idx[ti];
            for row in rows {
                let pk = row[idx].clone();
                let cur = self.attach.base_get_by_pk(ti as u32, std::slice::from_ref(&pk))?;
                if pre_matches(&row[row.len() - 1], cur.as_deref()) {
                    report.unchanged += 1;
                    continue;
                }
                match policy {
                    ReconcilePolicy::Ours => {
                        let mut full = row.clone();
                        let n = full.len();
                        full[n - 1] = pre_of(cur.as_deref());
                        acts.push(Act::RefreshPre(ti as u32, full));
                        report.ours += 1;
                    }
                    ReconcilePolicy::Theirs => {
                        acts.push(Act::Drop(ti as u32, pk));
                        report.theirs += 1;
                    }
                }
            }
        }
        for chunk in acts.chunks(TRUNCATE_BATCH) {
            let mut w = self.db.engine.begin_write()?;
            let res = chunk.iter().try_for_each(|a| match a {
                Act::RefreshPre(ti, full) => TxnCtx::update_by_pk(&mut w, *ti, full).map(|_| ()),
                Act::Drop(ti, pk) => {
                    TxnCtx::delete_by_pk(&mut w, *ti, std::slice::from_ref(pk)).map(|_| ())
                }
            });
            match res {
                Ok(()) => w.commit()?,
                Err(e) => {
                    w.abort();
                    return Err(e);
                }
            }
        }
        // Bless the base as it is now: settle + store + expected.
        let stamp = settle_and_read(&self.base, &scratch_path(&self.base)).map_err(oerr)?;
        let _ = std::fs::remove_file(scratch_path(&self.base));
        self.db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
        self.expected = stamp;
        if self.mode == LockMode::Locked {
            if let Some(l) = transient {
                self.lock = Some(l); // a detached LOCKED handle re-arms
            }
        }
        Ok(report)
    }

    fn exec_plan(
        &self,
        plan: &mpedb_sql::CompiledPlan,
        params: &[Value],
        base_ok: bool,
    ) -> Result<ExecResult> {
        let mut partial = false;
        if plan.footprint.read_only {
            let r = self.db.engine.begin_read()?;
            let result = {
                let mut octx = ReadCtx(&r, None, None);
                let mut ctx = MergeCtx {
                    ovl: &mut octx,
                    at: &self.attach,
                    pk_idx: &self.pk_idx,
                    base_ok,
                };
                exec_stmt(&mut ctx, self.attach.schema(), plan, params, &mut partial)
            };
            r.finish()?;
            result
        } else {
            let mut w = self.db.engine.begin_write()?;
            let result = {
                let mut ctx = MergeCtx {
                    ovl: &mut w,
                    at: &self.attach,
                    pk_idx: &self.pk_idx,
                    base_ok,
                };
                exec_stmt(&mut ctx, self.attach.schema(), plan, params, &mut partial)
            };
            match result {
                Ok(r) => {
                    w.commit()?;
                    Ok(r)
                }
                Err(e) => {
                    w.abort();
                    Err(e)
                }
            }
        }
    }
}

/// The merge at the TxnCtx boundary: overlay rows (WITH the trailing
/// `__dead`) shadow base rows per PK; tombstones suppress; the executor sees
/// user-width rows only.
struct MergeCtx<'a> {
    ovl: &'a mut dyn TxnCtx,
    at: &'a SqliteAttach,
    pk_idx: &'a [usize],
    /// UNLOCKED-OFFLINE (§2): `false` refuses every base fall-through by
    /// name — overlay-resident answers still serve.
    base_ok: bool,
}

impl MergeCtx<'_> {
    fn base_gate(&self) -> Result<()> {
        if !self.base_ok {
            return Err(Error::Unsupported(
                "base is unlocked-offline: this statement needs the base (a scan or a \
                 fall-through miss) — reopen in locked/optimistic mode"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The `__pre` for a delta write to this PK: the FIRST write captures
    /// the base's row image (or its absence) atomically with the delta;
    /// later writes carry the original capture forward. Offline (no base
    /// access) captures `Null` = unknown — reconcile treats it as a
    /// conflict, never as proof.
    fn pre_for(&mut self, table: u32, pk: &Value) -> Result<Value> {
        if let Some(existing) = self.ovl.get_by_pk(table, std::slice::from_ref(pk))? {
            return Ok(existing[existing.len() - 1].clone());
        }
        if !self.base_ok {
            return Ok(Value::Null);
        }
        let base = self.at.base_get_by_pk(table, std::slice::from_ref(pk))?;
        Ok(pre_of(base.as_deref()))
    }
}

fn is_dead(row: &[Value]) -> bool {
    matches!(row.get(row.len().wrapping_sub(HIDDEN_COLS)), Some(Value::Bool(true)))
}

fn strip(mut row: Vec<Value>) -> Vec<Value> {
    row.truncate(row.len().saturating_sub(HIDDEN_COLS));
    row
}

/// User-column slice of an overlay row (drops `__dead` + `__pre`).
fn user_cols(row: &[Value]) -> &[Value] {
    &row[..row.len() - HIDDEN_COLS]
}

/// Canonical, private encoding of a base row for `__pre` comparison —
/// deterministic tag + LE bytes per value (floats by bit pattern).
fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * row.len());
    for v in row {
        match v {
            Value::Null => out.push(0),
            Value::Int(i) => {
                out.push(1);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(2);
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Bool(b) => {
                out.push(3);
                out.push(*b as u8);
            }
            Value::Text(s) => {
                out.push(4);
                out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Blob(b) => {
                out.push(5);
                out.extend_from_slice(&(b.len() as u64).to_le_bytes());
                out.extend_from_slice(b);
            }
            Value::Timestamp(t) => {
                out.push(6);
                out.extend_from_slice(&t.to_le_bytes());
            }
            // Never stored (the row codec refuses lists).
            Value::List(_) => out.push(7),
        }
    }
    out
}

/// The `__pre` value for a CURRENT base state: present row, absent, or
/// (offline) unknown.
fn pre_of(base_row: Option<&[Value]>) -> Value {
    match base_row {
        Some(row) => {
            let mut b = vec![1u8];
            b.extend_from_slice(&encode_row(row));
            Value::Blob(b)
        }
        None => Value::Blob(vec![0u8]),
    }
}

/// Does a stored `__pre` still describe the CURRENT base row? `Null`
/// (offline-captured) is never provably unchanged.
fn pre_matches(pre: &Value, cur_base: Option<&[Value]>) -> bool {
    matches!((pre, &pre_of(cur_base)), (Value::Blob(a), Value::Blob(b)) if a == b)
}

/// Order two PK values from the SAME table (int64 or text — the served
/// shapes; a table only ever produces one variant). Int order and BINARY
/// text order both match the storage order on both sides of the merge.
fn pk_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Ok(x.as_bytes().cmp(y.as_bytes())),
        _ => Err(Error::Internal("mixed-type PKs in one merged table".into())),
    }
}

impl MergeCtx<'_> {
    /// Overlay upsert: the engine has no upsert form, so try update first
    /// (an overlay row or tombstone may already hold this PK), else insert.
    fn ovl_upsert(&mut self, table: u32, full: &[Value]) -> Result<()> {
        if self.ovl.update_by_pk(table, full)? {
            return Ok(());
        }
        self.ovl.insert_row(table, full)
    }
}

impl TxnCtx for MergeCtx<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        if let Some(row) = self.ovl.get_by_pk(table, pk)? {
            return Ok(if is_dead(&row) { None } else { Some(strip(row)) });
        }
        self.base_gate()?;
        self.at.base_get_by_pk(table, pk)
    }

    fn get_by_index(&mut self, _t: u32, _n: u32, _v: &[Value]) -> Result<Option<Vec<Value>>> {
        Err(Error::Internal("index probe on an overlay (schema has none)".into()))
    }
    fn scan_by_index(&mut self, _t: u32, _n: u32, _v: &[Value]) -> Result<Vec<Vec<Value>>> {
        Err(Error::Internal("index scan on an overlay (schema has none)".into()))
    }
    fn scan_by_index_range(
        &mut self,
        _t: u32,
        _n: u32,
        _lo: Option<(&[u8], bool)>,
        _hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        Err(Error::Internal("index range on an overlay (schema has none)".into()))
    }

    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        self.base_gate()?;
        // Both streams arrive PK-ascending (mpedb scans are key-ordered; the
        // attach serves every shape in key order): a two-pointer merge where
        // the overlay wins ties and tombstones emit nothing.
        let ovl = self.ovl.scan_rows_raw(table, lo, hi)?;
        let base = self.at.base_scan(table, lo, hi)?;
        let idx = self.pk_idx[table as usize];
        let mut out = Vec::with_capacity(base.len() + ovl.len());
        let (mut i, mut j) = (0usize, 0usize);
        loop {
            let take_ovl = match (ovl.get(i), base.get(j)) {
                (Some(o), Some(b)) => match pk_cmp(&o[idx], &b[idx])? {
                    std::cmp::Ordering::Equal => {
                        j += 1; // shadowed
                        true
                    }
                    ord => ord == std::cmp::Ordering::Less,
                },
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_ovl {
                let row = ovl[i].clone();
                i += 1;
                if !is_dead(&row) {
                    out.push(strip(row));
                }
            } else {
                out.push(base[j].clone());
                j += 1;
            }
        }
        Ok(out)
    }

    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        // INSERT's uniqueness is over the MERGED view: a live base row
        // collides exactly as a live overlay row does; a tombstoned PK is
        // free again.
        let pk = values[self.pk_idx[table as usize]].clone();
        if self.get_by_pk(table, std::slice::from_ref(&pk))?.is_some() {
            let name = self
                .at
                .schema()
                .table(table)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(Error::PrimaryKeyViolation { table: name });
        }
        let pre = self.pre_for(table, &pk)?;
        let mut full = values.to_vec();
        full.push(Value::Bool(false));
        full.push(pre);
        self.ovl_upsert(table, &full)
    }

    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        // The executor only calls this for rows it just read from the merged
        // view, so existence is established; materialize into the overlay.
        let pk = new_values[self.pk_idx[table as usize]].clone();
        let pre = self.pre_for(table, &pk)?;
        let mut full = new_values.to_vec();
        full.push(Value::Bool(false));
        full.push(pre);
        self.ovl_upsert(table, &full)?;
        Ok(true)
    }

    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        // Tombstone: PK + NULLs + __dead=true. Suppresses a base row and
        // shadows/replaces any live overlay row.
        let t = self
            .at
            .schema()
            .table(table)
            .ok_or_else(|| Error::Internal("table id out of range".into()))?;
        let idx = self.pk_idx[table as usize];
        let pkv = pk
            .first()
            .cloned()
            .ok_or_else(|| Error::Internal("empty PK in delete".into()))?;
        let pre = self.pre_for(table, &pkv)?;
        let mut full = vec![Value::Null; t.columns.len()];
        full[idx] = pkv;
        full.push(Value::Bool(true));
        full.push(pre);
        self.ovl_upsert(table, &full)?;
        Ok(true)
    }
}

// ---- epoch + marker + delta plumbing (shared by checkpoint and recovery) --

fn read_epoch(db: &Database) -> Result<u64> {
    Ok(match db.sys_record_get(STAMP_NS, EPOCH_KEY)? {
        Some(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().expect("8")),
        _ => 1,
    })
}

/// Read `checkpointed_epoch` from the base's marker table via the NATIVE
/// reader — recovery must work without the sqlite library.
fn read_marker(base: &Path) -> Result<Option<i64>> {
    let f = fmtx::SqliteFile::open(base).map_err(oerr)?;
    let tables = f.tables().map_err(oerr)?;
    let Some(t) = tables.iter().find(|t| t.name == MARKER_TABLE) else {
        return Ok(None);
    };
    let mut v = None;
    f.scan_table(t, &mut |_r, vals| {
        if matches!(vals.first(), Some(fmtx::Value::Text(k)) if k == "checkpointed_epoch") {
            if let Some(fmtx::Value::Int(e)) = vals.get(1) {
                v = Some(*e);
            }
        }
        Ok(())
    })
    .map_err(oerr)?;
    Ok(v)
}

/// One consistent snapshot of EVERY delta row (including `__dead`), per
/// table id.
fn snapshot_deltas(db: &Database, n_tables: usize) -> Result<Vec<Vec<Vec<Value>>>> {
    let r = db.engine.begin_read()?;
    let mut out = Vec::with_capacity(n_tables);
    let res = {
        let mut ctx = ReadCtx(&r, None, None);
        (0..n_tables).try_for_each(|ti| {
            out.push(TxnCtx::scan_rows_raw(&mut ctx, ti as u32, None, None)?);
            Ok(())
        })
    };
    r.finish()?;
    res.map(|()| out)
}

/// Delete exactly the snapshotted delta rows, in bounded batches [R#14] —
/// each batch its own commit, so freeing keeps pace with the COW allocation
/// deleting requires. A row that no longer equals its snapshot image is
/// KEPT (it changed after the freeze and rides the next checkpoint).
fn truncate_deltas(db: &Database, deltas: &[Vec<Vec<Value>>], pk_idx: &[usize]) -> Result<()> {
    for (ti, rows) in deltas.iter().enumerate() {
        let idx = pk_idx[ti];
        for chunk in rows.chunks(TRUNCATE_BATCH) {
            let mut w = db.engine.begin_write()?;
            let res = chunk.iter().try_for_each(|row| {
                let pk = [row[idx].clone()];
                if TxnCtx::get_by_pk(&mut w, ti as u32, &pk)?.as_deref() == Some(&row[..]) {
                    TxnCtx::delete_by_pk(&mut w, ti as u32, &pk)?;
                }
                Ok(())
            });
            match res {
                Ok(()) => w.commit()?,
                Err(e) => {
                    w.abort();
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// What the divergence recovery at reopen decided.
enum Recovery {
    /// Adopted (empty overlay, or our own crashed checkpoint healed).
    Healed,
    /// Foreign writer interleaved with unpushed deltas — the caller decides
    /// (refuse, or reconcile with a policy).
    Diverged { why: &'static str },
}

/// The reopen path when the stored stamp no longer matches the base.
///
/// Three outcomes, in order:
/// - The overlay is EMPTY: nothing was captured against the old base, so a
///   moved base is not divergence at all — adopt it (fresh settled stamp)
///   and carry on. A cleanly-checkpointed overlay thus acts like no overlay:
///   foreign writers may do anything between our sessions.
/// - The overlay has deltas and the movement is provably our own crashed
///   checkpoint: the base's marker names exactly our epoch [R#4] AND every
///   delta is exactly reflected in the base (live rows equal, tombstoned
///   PKs absent). Truncate the redundant deltas and adopt.
/// - Anything else means a foreign writer interleaved with unpushed deltas —
///   `Diverged`; the caller refuses or reconciles, never blind-replays
///   [R#3].
fn recover_after_crashed_checkpoint(
    base: &Path,
    db: &Database,
    attach: &SqliteAttach,
    pk_idx: &[usize],
) -> Result<Recovery> {
    let deltas = snapshot_deltas(db, pk_idx.len())?;
    if deltas.iter().all(|t| t.is_empty()) {
        let stamp = settle_and_read(base, &scratch_path(base)).map_err(oerr)?;
        let _ = std::fs::remove_file(scratch_path(base));
        db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
        return Ok(Recovery::Healed);
    }
    let epoch = read_epoch(db)?;
    if read_marker(base)? != Some(epoch as i64) {
        return Ok(Recovery::Diverged {
            why: "a foreign writer committed in an unlocked window",
        });
    }
    for (ti, rows) in deltas.iter().enumerate() {
        let idx = pk_idx[ti];
        for row in rows {
            let pk = [row[idx].clone()];
            let in_base = attach.base_get_by_pk(ti as u32, &pk)?;
            let consistent = if is_dead(row) {
                in_base.is_none()
            } else {
                in_base.as_deref() == Some(user_cols(row))
            };
            if !consistent {
                return Ok(Recovery::Diverged {
                    why: "a foreign writer overwrote our crashed checkpoint's push",
                });
            }
        }
    }
    // Adopt: the deltas are redundant shadows of the pushed base.
    truncate_deltas(db, &deltas, pk_idx)?;
    let stamp = settle_and_read(base, &scratch_path(base)).map_err(oerr)?;
    let _ = std::fs::remove_file(scratch_path(base));
    db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
    db.sys_record_put(STAMP_NS, EPOCH_KEY, &(epoch + 1).to_le_bytes())?;
    Ok(Recovery::Healed)
}

// ---- the checkpoint itself (design §5) — needs the sqlite LIBRARY --------

/// What one checkpoint did.
#[cfg(feature = "sqlite-checkpoint")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointReport {
    pub upserts: u64,
    pub deletes: u64,
    pub epoch: u64,
}

#[cfg(feature = "sqlite-checkpoint")]
enum PushOutcome {
    Done { upserts: u64, deletes: u64 },
    /// The in-transaction re-validation [R#5c] found the base moved in the
    /// drop window — nothing was written.
    Diverged,
}

#[cfg(feature = "sqlite-checkpoint")]
impl SqliteOverlay {
    /// Push every delta into the base and truncate the overlay — design §5,
    /// with the [R#5] drop → push-under-BEGIN-IMMEDIATE → re-take → re-stamp
    /// dance spelled out in order. `&mut self` is load-bearing: the freeze
    /// is exactly "no writes through this handle while the checkpoint runs"
    /// (co-attaching the same overlay from another process during a
    /// checkpoint is outside v2's contract).
    pub fn checkpoint(&mut self) -> Result<CheckpointReport> {
        match self.mode {
            LockMode::Locked => self.ensure_locked()?,
            LockMode::Optimistic => {}
            LockMode::Offline => {
                return Err(Error::Unsupported(
                    "checkpoint writes the base — refuse in unlocked-offline mode; \
                     reopen in locked/optimistic mode first"
                        .into(),
                ))
            }
        }
        let epoch = self.epoch()?;
        let deltas = snapshot_deltas(&self.db, self.pk_idx.len())?;
        if deltas.iter().all(|t| t.is_empty()) {
            return Ok(CheckpointReport { upserts: 0, deletes: 0, epoch });
        }
        let stored = decode_stamp(
            &self
                .db
                .sys_record_get(STAMP_NS, STAMP_KEY)?
                .ok_or_else(|| Error::Corrupt("overlay has no stored base-stamp".into()))?,
        )?;
        // Drop our SHARED: we are about to BE the writer, and its EXCLUSIVE
        // at COMMIT must not find our own reader lock in the way.
        self.lock = None;
        let push = push_deltas(&self.base, self.attach.schema(), &deltas, &self.pk_idx, &stored, epoch);
        let retaken = retake_shared(&self.base);
        match (push, retaken) {
            (Ok(PushOutcome::Done { upserts, deletes }), Ok(lock)) => {
                // Settle under the re-taken SHARED. A foreign commit in the
                // re-take gap is FINE: it serialized after our push, the
                // fresh stamp blesses it, and the deltas are truncated
                // (compare-to-snapshot keeps nothing stale).
                let stamp = settle_and_read(&self.base, &scratch_path(&self.base)).map_err(oerr)?;
                let _ = std::fs::remove_file(scratch_path(&self.base));
                self.db.sys_record_put(STAMP_NS, STAMP_KEY, &encode_stamp(&stamp))?;
                // The native reader snapshots the file at open — refresh it
                // so the handle's base view includes what was just pushed.
                let fresh = SqliteAttach::open(&self.base)?;
                if fresh.schema() != self.attach.schema() {
                    return Err(Error::Unsupported(
                        "foreign DDL changed the base's schema during the checkpoint \
                         window — the handle is detached; reopen".into(),
                    ));
                }
                self.attach = fresh;
                truncate_deltas(&self.db, &deltas, &self.pk_idx)?;
                self.db.sys_record_put(STAMP_NS, EPOCH_KEY, &(epoch + 1).to_le_bytes())?;
                self.expected = stamp;
                if self.mode == LockMode::Locked {
                    self.lock = Some(lock);
                }
                Ok(CheckpointReport { upserts, deletes, epoch })
            }
            (Ok(PushOutcome::Done { .. }), Err(e)) => Err(Error::Unsupported(format!(
                "checkpoint pushed epoch {epoch} but could not re-take the base lock ({e}) — \
                 the handle is detached; reopening recovers (the base's marker authorizes it)"
            ))),
            (Ok(PushOutcome::Diverged), _) => {
                // Base moved under us: the deltas are stale. Stay detached
                // (any re-taken lock drops here) — reopening decides.
                Err(Error::Unsupported(
                    "checkpoint refused: the base changed while unlocked — this overlay's \
                     deltas are stale; reopen to decide (reconcile is not built yet)"
                        .into(),
                ))
            }
            (Err(e), Ok(lock)) => {
                // IO-class push failure: BEGIN IMMEDIATE rolled back, the
                // base is untouched, our stamp still matches — re-arm and
                // keep serving.
                if self.mode == LockMode::Locked {
                    self.lock = Some(lock);
                }
                Err(e)
            }
            (Err(e), Err(_)) => Err(e),
        }
    }
}

fn retake_shared(base: &Path) -> Result<SharedLock> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(l) = SharedLock::acquire(base).map_err(oerr)? {
            return Ok(l);
        }
        if std::time::Instant::now() > deadline {
            return Err(Error::Unsupported(
                "could not re-take the base SHARED within 5s".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(feature = "sqlite-checkpoint")]
fn to_sq(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Int(i) => S::Integer(*i),
        Value::Float(f) => S::Real(*f),
        Value::Bool(b) => S::Integer(*b as i64),
        Value::Text(t) => S::Text(t.clone()),
        Value::Blob(b) => S::Blob(b.clone()),
        Value::Timestamp(t) => S::Integer(*t),
        // Never stored: the row codec refuses lists (DESIGN-MULTIDB §2.6).
        Value::List(_) => S::Null,
    }
}

#[cfg(feature = "sqlite-checkpoint")]
fn push_deltas(
    base: &Path,
    schema: &Schema,
    deltas: &[Vec<Vec<Value>>],
    pk_idx: &[usize],
    stored: &BaseStamp,
    epoch: u64,
) -> Result<PushOutcome> {
    let serr = |e: rusqlite::Error| Error::Unsupported(format!("sqlite checkpoint: {e}"));
    let c = rusqlite::Connection::open(base).map_err(serr)?;
    c.busy_timeout(std::time::Duration::from_secs(5)).map_err(serr)?;
    // synchronous=FULL owns durability — no after-the-fact fsync [R#13].
    c.pragma_update(None, "synchronous", "FULL").map_err(serr)?;
    c.execute_batch("BEGIN IMMEDIATE").map_err(serr)?;
    // [R#5c] re-validate UNDER the write lock: RESERVED is ours from here to
    // COMMIT, so no foreign commit can slip between this check and the push.
    if !stored.matches(base).map_err(oerr)? {
        let _ = c.execute_batch("ROLLBACK");
        return Ok(PushOutcome::Diverged);
    }
    let mut upserts = 0u64;
    let mut deletes = 0u64;
    let r = (|| -> rusqlite::Result<()> {
        for (ti, rows) in deltas.iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let t = &schema.tables[ti];
            let cols: Vec<String> =
                t.columns.iter().map(|c| format!("\"{}\"", c.name)).collect();
            let qs = vec!["?"; cols.len()].join(", ");
            // The synthetic-rowid shape's PK column is literally named
            // `rowid`, which sqlite resolves to the real rowid (the attach
            // shape rules guarantee no user column shadows it).
            let mut ins = c.prepare(&format!(
                "INSERT OR REPLACE INTO \"{}\" ({}) VALUES ({qs})",
                t.name,
                cols.join(", ")
            ))?;
            let mut del = c.prepare(&format!(
                "DELETE FROM \"{}\" WHERE \"{}\" = ?",
                t.name, t.columns[pk_idx[ti]].name
            ))?;
            for row in rows {
                if is_dead(row) {
                    del.execute([to_sq(&row[pk_idx[ti]])])?;
                    deletes += 1;
                } else {
                    ins.execute(rusqlite::params_from_iter(user_cols(row).iter().map(to_sq)))?;
                    upserts += 1;
                }
            }
        }
        // [R#4]: the marker commits ATOMICALLY with the push, in the base.
        c.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {MARKER_TABLE} (k TEXT PRIMARY KEY, v INTEGER);"
        ))?;
        c.execute(
            &format!(
                "INSERT OR REPLACE INTO {MARKER_TABLE} (k, v) VALUES ('checkpointed_epoch', ?)"
            ),
            [epoch as i64],
        )?;
        c.execute_batch("COMMIT")?;
        Ok(())
    })();
    if let Err(e) = r {
        let _ = c.execute_batch("ROLLBACK");
        return Err(serr(e));
    }
    Ok(PushOutcome::Done { upserts, deletes })
}
