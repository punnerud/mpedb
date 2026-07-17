//! The v2 delta overlay — DESIGN-SQLITE-BACKED §1/§3/§6, building block 5.
//! The `.db` stays the durable home; `<base>.overlay.mpedb` holds ONLY what
//! changed since the last checkpoint: upserted row images and TOMBSTONES.
//! Reads merge per PK — overlay shadows base, tombstones suppress — with the
//! base read through the native reader under a held SHARED lock (LOCKED
//! mode: the base provably cannot move, so fall-through needs zero
//! validation).
//!
//! What this block does and does not do, by name:
//! - LOCKED only: the SHARED is held for the overlay's lifetime. OPTIMISTIC
//!   wiring (the per-statement bracket) and UNLOCKED-OFFLINE come with the
//!   mode plumbing (block 7); the primitives already exist in
//!   `sqlitefmt::lock`.
//! - No checkpoint yet (block 6): deltas accumulate until then.
//! - Divergence at reopen (the stored settled stamp no longer matching the
//!   base) is a NAMED refusal — reconcile rides the checkpoint block.
//! - A hot journal at open is a named refusal telling the fix (`sqlite3
//!   base.db 'SELECT 1'` runs sqlite's own recovery); this crate never
//!   rolls journals back.
//!
//! Layout contract: the overlay's physical tables are the attach-derived
//! user tables PLUS a trailing hidden `__dead` bool. The executor never
//! sees `__dead` — plans compile against the USER schema and the merge
//! context strips/interprets the marker at the TxnCtx boundary.

use std::path::{Path, PathBuf};

use mpedb_sqlitefmt as fmtx;
use mpedb_sqlitefmt::lock::{hot_journal, SharedLock};
use mpedb_sqlitefmt::stamp::{settle_and_read, BaseStamp};
use mpedb_types::{ColumnType, Config, Error, Result, Schema, Value};

use crate::exec::{exec_stmt, ReadCtx, TxnCtx};
use crate::sqlite_attach::SqliteAttach;
use crate::{Database, ExecResult};

const STAMP_NS: &str = "ovl";
const STAMP_KEY: &[u8] = b"base-stamp";

pub struct SqliteOverlay {
    attach: SqliteAttach,
    db: Database,
    /// LOCKED mode: held for the overlay's lifetime, so the base provably
    /// cannot move under the merge.
    _lock: SharedLock,
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
                _ => "any",
            };
            let nullable = c.ty != ColumnType::Int64;
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
        let ovl = overlay_path(base);
        let db = if ovl.exists() {
            let db = Database::open_from_file(&ovl)?;
            // The stored settled stamp must still describe the base — else
            // the deltas were captured against a base that has since moved.
            let stored = db
                .sys_record_get(STAMP_NS, STAMP_KEY)?
                .ok_or_else(|| Error::Corrupt("overlay has no stored base-stamp".into()))?;
            let stored = decode_stamp(&stored)?;
            if !stored.matches(base).map_err(oerr)? {
                return Err(Error::Unsupported(format!(
                    "the base {} changed since this overlay's deltas were captured — \
                     reconcile is not built yet: checkpoint or discard {}",
                    base.display(),
                    ovl.display()
                )));
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
            db
        };
        let pk_idx: Vec<usize> = attach
            .schema()
            .tables
            .iter()
            .map(|t| t.primary_key[0] as usize)
            .collect();
        Ok(SqliteOverlay { attach, db, _lock: lock, pk_idx })
    }

    pub fn schema(&self) -> &Schema {
        self.attach.schema()
    }

    /// Compile + run one statement over the MERGED view. Reads run on an
    /// overlay read transaction (lock-free); writes run in one overlay write
    /// transaction, committed only on success.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
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
        let mut partial = false;
        if plan.footprint.read_only {
            let r = self.db.engine.begin_read()?;
            let result = {
                let mut octx = ReadCtx(&r);
                let mut ctx =
                    MergeCtx { ovl: &mut octx, at: &self.attach, pk_idx: &self.pk_idx };
                exec_stmt(&mut ctx, self.attach.schema(), &plan, params, &mut partial)
            };
            r.finish()?;
            result
        } else {
            let mut w = self.db.engine.begin_write()?;
            let result = {
                let mut ctx = MergeCtx { ovl: &mut w, at: &self.attach, pk_idx: &self.pk_idx };
                exec_stmt(&mut ctx, self.attach.schema(), &plan, params, &mut partial)
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
}

fn is_dead(row: &[Value]) -> bool {
    matches!(row.last(), Some(Value::Bool(true)))
}

fn strip(mut row: Vec<Value>) -> Vec<Value> {
    row.pop();
    row
}

fn pk_of(row: &[Value], idx: usize) -> Result<i64> {
    match row.get(idx) {
        Some(Value::Int(i)) => Ok(*i),
        _ => Err(Error::Internal("non-int PK in merged row".into())),
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
        self.at.base_get_by_pk(table, pk)
    }

    fn get_by_index(&mut self, _t: u32, _n: u32, _v: &Value) -> Result<Option<Vec<Value>>> {
        Err(Error::Internal("index probe on an overlay (schema has none)".into()))
    }
    fn scan_by_index(&mut self, _t: u32, _n: u32, _v: &Value) -> Result<Vec<Vec<Value>>> {
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
                (Some(o), Some(b)) => {
                    let (ko, kb) = (pk_of(o, idx)?, pk_of(b, idx)?);
                    if ko == kb {
                        j += 1; // shadowed
                        true
                    } else {
                        ko < kb
                    }
                }
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
        if self.get_by_pk(table, &[pk])?.is_some() {
            let name = self
                .at
                .schema()
                .table(table)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(Error::PrimaryKeyViolation { table: name });
        }
        let mut full = values.to_vec();
        full.push(Value::Bool(false));
        self.ovl_upsert(table, &full)
    }

    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        // The executor only calls this for rows it just read from the merged
        // view, so existence is established; materialize into the overlay.
        let mut full = new_values.to_vec();
        full.push(Value::Bool(false));
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
        let mut full = vec![Value::Null; t.columns.len()];
        full[idx] = pk
            .first()
            .cloned()
            .ok_or_else(|| Error::Internal("empty PK in delete".into()))?;
        full.push(Value::Bool(true));
        self.ovl_upsert(table, &full)?;
        Ok(true)
    }
}
