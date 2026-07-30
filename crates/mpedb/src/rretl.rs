//! `mpedb rretl` — apply a lens pair to a column IN PLACE, keep what was lost,
//! and be able to put it back (design/DESIGN-RRETL.md §7/§11, #52 stage 2).
//!
//! One run is ONE WriteSession: read, transform, persist residuals, verify,
//! commit — atomically. In-place transformation IS source deletion, so the
//! verification level is always `total` (commitment 4, Lepton's discipline):
//! before the commit that destroys the source, every transformed row is read
//! back INSIDE the same transaction and the `inverse(y, r)` stream is hashed
//! against the source hash. At O(1) memory — the originals are not held; the
//! hash chain over canonical bytes is the comparison.
//!
//! What the database keeps, and how it is found again:
//! - `rretl_residual (run_id, pk_enc) → residual` — what was lost, per row, per
//!   run. Keyed by run so two runs can never collide, `pk_enc` is the row's
//!   PK in canonical bytes. A residual VALUE of NULL is legal; a MISSING row
//!   is a hard error — confusing them would smuggle the refused creation path
//!   `inverse(y, ∅)` back in as a silent wrong answer.
//! - `rretl_lineage (run_id, step_no) → …` — which pair (by CONTENT HASH, all
//!   three functions), which table.column, the source and output hashes, how
//!   many rows, which verification level ran, and the outcome. Failed runs are
//!   first-class lineage: an aborted apply leaves an `outcome='failed'` row
//!   (in its own small transaction — the failed run's txn rolled back).
//!
//! Ordinary TABLES, not sys-keyspace records: #124 measured compilation as
//! O(bytes in the sys keyspace), and lineage is an unbounded log (§7.1).
//!
//! The hashes are blake3 over `value_bits`-CANONICAL bytes, never raw storage
//! bits — a pair that legally canonicalises a NaN payload must not produce a
//! false "artifact changed" (§12.2 attack 2). Each value is length-framed in
//! the chain for the same reason the joint collision key is (§12.2 attack 1).

use std::sync::Arc;

use mpedb_spell::ir::Proc;
use mpedb_types::{ColumnType, Error, Result, Value};

use crate::lens::LensClass;
use crate::{ExecResult, WriteSession};

pub const T_LINEAGE: &str = "rretl_lineage";
pub const T_RESIDUAL: &str = "rretl_residual";

/// Every table name rRETL owns — refused as a transform or map target.
pub(crate) fn rretl_bookkeeping_names() -> [&'static str; 8] {
    [
        T_LINEAGE,
        T_RESIDUAL,
        crate::rretl_store::T_VERSIONS,
        crate::rretl_store::T_ARCHIVES,
        crate::rretl_store::T_MEMBERS,
        crate::rretl_map::T_MAP_STATE,
        crate::rretl_map_run::T_MAP_CURSOR,
        crate::rretl_map_run::T_MAP_RUN,
    ]
}

/// Verification levels, as recorded in `rretl_lineage.verified` (§5: report what
/// was verified, never a bare "verified"). Apply always runs `total`.
const VERIFIED_TOTAL: i64 = 2;

/// Rows per streaming chunk. One run is still ONE transaction BY DESIGN
/// (total verification before the source dies requires it — chunked COMMITS
/// are not a fix), but no pass materialises more than one chunk: scans
/// resume with `pk > last ORDER BY pk LIMIT n`, which yields the exact same
/// globally-sorted stream the hash chains were defined over. Heap is O(chunk)
/// regardless of table size; the dirtied pages live in the file-backed map,
/// not the heap. The old 1M-row pre-flight cap (§12.2 attack 4) is GONE with
/// the OOM it guarded against — the remaining bound is file space, and
/// DbFull is already a deterministic, named refusal that rolls back whole.
///
/// The env override is a TEST hook: chunk-boundary resume logic must be
/// exercised without million-row fixtures.
const RRETL_CHUNK_DEFAULT: usize = 4096;

thread_local! {
    /// Per-THREAD chunk override — see [`ChunkGuard`].
    static CHUNK_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Force the chunk size for the current THREAD until dropped.
///
/// The env var below is the out-of-process hook (a CLI run, a cron daemon). It
/// is the wrong hook for a test: `cargo test` runs a file's tests as THREADS in
/// one process, so two tests that each set and clear `MPEDB_RRETL_CHUNK` race,
/// and the loser silently runs at the 4096 default. That is exactly what made
/// `rretl_map::the_daemon_advances_both_tables_together_and_resumes` fail about
/// one run in three under a full parallel workspace test while passing alone —
/// its budget of 4 rows met a chunk of 4096 and moved the whole table. (It is
/// also why `std::env::set_var` is `unsafe` from edition 2024.)
///
/// Thread-local is sound here for the same reason `FK_DEFERRED` is: every rRETL
/// pass runs synchronously on the caller's thread.
#[doc(hidden)]
pub struct ChunkGuard(Option<usize>);

impl ChunkGuard {
    #[doc(hidden)]
    pub fn new(rows: usize) -> ChunkGuard {
        ChunkGuard(CHUNK_OVERRIDE.with(|c| c.replace(Some(rows))))
    }
}

impl Drop for ChunkGuard {
    fn drop(&mut self) {
        CHUNK_OVERRIDE.with(|c| c.set(self.0));
    }
}

/// [`chunk_rows`] for the test that pins the guard's thread scoping.
#[doc(hidden)]
pub fn chunk_rows_for_tests() -> usize {
    chunk_rows()
}

pub(crate) fn chunk_rows() -> usize {
    if let Some(n) = CHUNK_OVERRIDE.with(|c| c.get()) {
        return n;
    }
    std::env::var("MPEDB_RRETL_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(RRETL_CHUNK_DEFAULT)
}

/// The collision DIAGNOSTIC is bounded: past this many distinct images the
/// map stops growing and a later mismatch is reported by the total
/// verification instead (fail-safe either way — this only trades message
/// quality for bounded memory on huge tables).
const COLLISION_DIAG_CAP: usize = 1 << 20;

/// What a run did, as `rretl apply`/`rretl revert` report it.
#[derive(Debug)]
pub struct RretlReport {
    pub run_id: i64,
    pub rows: u64,
    /// Residual rows written (0 for a bijective pair).
    pub residuals: u64,
}

/// One lineage row, as `rretl log` reports it.
#[derive(Debug)]
pub struct RretlLogRow {
    pub run_id: i64,
    pub lens: String,
    pub table: String,
    pub column: String,
    pub rows: i64,
    pub outcome: String,
    pub error: String,
}

pub(crate) fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Length-framed canonical hash chain — the rRETL sibling of the lens layer's
/// joint collision key, and framed for the same reason.
pub(crate) struct CanonChain(pub(crate) blake3::Hasher);

impl CanonChain {
    pub(crate) fn new() -> Self {
        Self(blake3::Hasher::new())
    }
    pub(crate) fn push(&mut self, v: &Value) {
        let bits = crate::lens::value_bits(v);
        self.0.update(&(bits.len() as u64).to_le_bytes());
        self.0.update(&bits);
    }
    pub(crate) fn hex(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// Bookkeeping key for a row identity: blake3 over the pk's canonical
/// bits, FIXED 32 bytes. The raw bits used to ride inside composite
/// bookkeeping keys (`(run_id, pk_enc)`, `(map, tbl, pk_enc)`) whose
/// OTHER parts eat into the engine's encoded-key cap — a legal ~970-char
/// TEXT pk then wedged apply and map sync behind an unnamed engine
/// refusal, and how long you named your map decided which keys were
/// syncable (the value saboteur's finding). A digest is injective at the
/// same trust level as every other content hash here, and every consumer
/// has the ROW in hand (or, for maps, the `k` value column), so nothing
/// ever needs to invert it.
pub(crate) fn pk_ref(v: &Value) -> Vec<u8> {
    blake3::hash(&crate::lens::value_bits(v)).as_bytes().to_vec()
}

pub(crate) fn rows_of(r: ExecResult) -> Result<Vec<Vec<Value>>> {
    match r {
        ExecResult::Rows { rows, .. } => Ok(rows),
        other => Err(Error::Unsupported(format!("expected rows, got {other:?}"))),
    }
}

/// Resolved facts about the target column, checked against the LIVE schema.
struct Target {
    pk_col: String,
    col_type: ColumnType,
}

impl crate::Database {
    fn resolve_target(&self, table: &str, column: &str) -> Result<Target> {
        // Same freshness rule as committed_tables(): this runs BEFORE any SQL
        // compilation or txn begin has refreshed the cached bundle, and a
        // stale read here would miss a table this very handle just created.
        self.engine.refresh_schema_if_stale()?;
        if rretl_bookkeeping_names().contains(&table) {
            return Err(Error::Unsupported(format!(
                "`{table}` is rRETL bookkeeping; transforming it is refused"
            )));
        }
        let bundle = self.engine.schema();
        let t = bundle
            .schema
            .tables
            .iter()
            .find(|t| t.name == table && !t.dead)
            .ok_or_else(|| Error::Unsupported(format!("no table named `{table}`")))?;
        // Row identity: a declared single-column PK, or the hidden rowid of
        // an implicit-rowid table — `rowid` is queryable, point-updatable and
        // range-scannable exactly like a declared INTEGER PK (probed:
        // PkRange + elided sort + PkPoint), and it is stable for the life of
        // the row. Identity semantics are the PK contract either way: delete
        // a row and re-use its identity (an explicit pk value, or a re-issued
        // rowid) and putback treats the newcomer as an EDIT of that row,
        // PutRes-gated like any other. Composite PKs stay a named refusal —
        // their chunk resume needs a tuple comparison the planner does not
        // have.
        // #94's implicit rowid materializes IN the schema as a real column
        // named `rowid` carrying the single-column PK — so the one-column
        // branch covers it, and there is nothing special to detect here.
        let pk_col = if t.primary_key.len() == 1 {
            t.columns[t.primary_key[0] as usize].name.clone()
        } else {
            return Err(Error::Unsupported(format!(
                "`{table}` has a {}-column primary key; rretl needs a single row \
                 identity — a one-column PK or an implicit rowid",
                t.primary_key.len()
            )));
        };
        let col = t
            .columns
            .iter()
            .find(|c| c.name == column)
            .ok_or_else(|| {
                Error::Unsupported(format!("no column `{column}` in `{table}`"))
            })?;
        if col.name == pk_col {
            return Err(Error::Unsupported(
                "transforming the PRIMARY KEY in place would change row identity; refused"
                    .into(),
            ));
        }
        Ok(Target { pk_col, col_type: col.ty })
    }

    /// Apply `pair` to every value of `table.column`, in place, in one
    /// transaction. See the module doc for the contract.
    pub fn rretl_apply(&self, pair: &str, table: &str, column: &str) -> Result<RretlReport> {
        let lens = self.load_lens_for_rretl(pair)?;
        // Class gate (commitment 2): in-place transformation deletes the
        // source, and a Lossy pair declares exactly that it cannot bring it
        // back. Refused by name; the fallback IS keeping the source.
        if lens.class == LensClass::Lossy {
            return Err(Error::Unsupported(format!(
                "`{pair}` is lossy: applying it in place would delete a source it \
                 declares it cannot recover — keep the source, or reclassify the pair"
            )));
        }
        let target = self.resolve_target(table, column)?;

        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = apply_in(&mut s, pair, &lens, table, column, &target, &have);
        match out {
            Ok((report, lineage)) => {
                lineage.insert(&mut s)?;
                s.commit()?;
                Ok(report)
            }
            Err(e) => {
                s.rollback();
                // Failed runs are first-class lineage (§7): record the failure
                // in its OWN small transaction, best-effort — the error the
                // caller sees is the apply's, never the bookkeeper's.
                let _ = self.record_failed_run(pair, table, column, &e);
                Err(e)
            }
        }
    }

    pub(crate) fn record_failed_run(&self, pair: &str, table: &str, column: &str, e: &Error) -> Result<()> {
        let mut s = self.begin()?;
        let have = self.committed_tables()?;
        ensure_tables_from(&mut s, &have)?;
        let run_id = next_run_id(&mut s)?;
        LineageRow {
            run_id,
            lens: pair.into(),
            forward_hash: String::new(),
            rex_hash: String::new(),
            inverse_hash: String::new(),
            table: table.into(),
            column: column.into(),
            source_hash: String::new(),
            output_hash: String::new(),
            residual_hash: String::new(),
            rows: 0,
            outcome: "failed",
            error: e.to_string(),
        }
        .insert(&mut s)?;
        s.commit()
    }

    /// Undo run `run_id`: hash-gate the column against the run's output hash,
    /// invert every row with its stored residual, verify against the source
    /// hash, drop the run's residuals, and mark the lineage row reverted.
    pub fn rretl_revert(&self, run_id: i64) -> Result<RretlReport> {
        let mut s = self.begin()?;
        let out = revert_in(self, &mut s, run_id);
        match out {
            Ok(report) => {
                s.commit()?;
                Ok(report)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Invert run `run_id` while KEEPING edits made to the transformed column —
    /// the lens putback, and the half of "reversible" that `revert` refuses.
    ///
    /// Where `revert` hash-gates the column (any edit = refusal), `putback`
    /// exists FOR the edited column: each surviving row's current value `y'` is
    /// combined with the run's stored residual — `x' = inverse(y', r)` — so the
    /// edit flows back into the source domain and what was lost is still
    /// re-attached. Deleted rows stay deleted (their residuals are discarded:
    /// the deletion IS an edit, and it survives). The image story: apply strips
    /// colour, the user retouches and crops the grayscale, putback re-attaches
    /// the colour to the retouched pixels and the cropped ones stay gone.
    ///
    /// Verification cannot compare against `source_hash` — edits are the point.
    /// The operative law is PutRes, per row, before commit:
    /// `forward(x') == y'` and (for residual pairs) `rex(x') == r`. At
    /// registration that law was a corpus tautology and deliberately not run
    /// (§4); here the source is no longer the oracle, and PutRes is the ONLY
    /// thing that can hold. A `y'` outside the pair's image for `r` — an edit
    /// the pair cannot carry back — fails it and aborts with the row named.
    ///
    /// New rows (no residual): refused for residual pairs — that is the
    /// creation path `inverse(y, ∅)`, refused by design (§4). For bijective
    /// pairs the creation path is total by construction, so new rows simply
    /// invert like every other row.
    pub fn rretl_putback(&self, run_id: i64) -> Result<RretlReport> {
        let mut s = self.begin()?;
        let out = putback_in(self, &mut s, run_id);
        match out {
            Ok(report) => {
                s.commit()?;
                Ok(report)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Verify-at-rest, the second half of the Lepton discipline: the write
    /// side verified once, but nothing re-checks between apply and unwind —
    /// and for a tampered column, unwind time is exactly too late to want
    /// the news. Re-verifies every STANDING (`outcome = 'applied'`) run,
    /// read-only: the TOP run per column re-hashes against its recorded
    /// output (buried runs' outputs were legitimately transformed away by
    /// later runs — that is the designed state, not a finding), every row of
    /// a residual run still has its residual, and the run's pair still
    /// loads. Returns findings, one string per problem, empty = clean. It
    /// reports and never repairs — a repair would need to know which side is
    /// right, and fsck does not.
    pub fn rretl_fsck(&self) -> Result<Vec<String>> {
        let mut findings = Vec::new();
        // Maps are checked FIRST and unconditionally: a defined-but-never-
        // synced map has a record and no lineage tables, and the early
        // return below would silently skip it.
        crate::rretl_map::fsck_maps(self, &mut findings)?;
        let bundle = self.engine.schema();
        if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
            return Ok(findings);
        }
        let runs = rows_of(self.query(
            "SELECT run_id, lens, tbl, col, output_hash, residual_hash FROM rretl_lineage \
             WHERE outcome = 'applied' ORDER BY run_id",
            &[],
        )?)?;
        let mut top_for: std::collections::HashMap<(String, String), i64> =
            std::collections::HashMap::new();
        for r in &runs {
            top_for.insert((as_text(&r[2]), as_text(&r[3])), as_int(&r[0])?);
        }
        for r in &runs {
            let (run_id, pair) = (as_int(&r[0])?, as_text(&r[1]));
            let (table, column, want_hash, want_residuals) =
                (as_text(&r[2]), as_text(&r[3]), as_text(&r[4]), as_text(&r[5]));
            // The residual set is checked FIRST and against the run's OWN
            // recorded chain — it needs neither the pair nor the target, so
            // BURIED runs and runs whose table was since dropped are covered
            // at rest, not first at unwind.
            let (got_residuals, _) = residual_chain_ro(self, run_id)?;
            if got_residuals != want_residuals {
                findings.push(format!(
                    "run {run_id}: its stored residuals no longer hash to what the apply \
                     wrote — tampered or deleted at rest; unwinding would fabricate data"
                ));
            }
            let lens = match self.load_lens_for_rretl(&pair) {
                Ok(l) => Some(l),
                Err(e) => {
                    findings.push(format!(
                        "run {run_id}: its pair `{pair}` cannot be loaded ({e}) — \
                         unwinding this run is currently impossible"
                    ));
                    None
                }
            };
            let target = match self.resolve_target(&table, &column) {
                Ok(t) => t,
                Err(e) => {
                    findings.push(format!(
                        "run {run_id}: its target `{table}.{column}` is gone ({e})"
                    ));
                    continue;
                }
            };
            let pk_col = &target.pk_col;
            if top_for.get(&(table.clone(), column.clone())) == Some(&run_id) {
                let mut c = CanonChain::new();
                scan_pairs_ro(self, &table, pk_col, &column, |_, y| {
                    c.push(y);
                    Ok(())
                })?;
                if c.hex() != want_hash {
                    findings.push(format!(
                        "run {run_id}: `{table}.{column}` no longer hashes to the run's \
                         output — edited outside the pipeline (revert will refuse; \
                         putback remains available)"
                    ));
                }
            }
            if lens.as_ref().map(|l| l.rex.is_some()).unwrap_or(false) {
                scan_pairs_ro(self, &table, pk_col, &column, |pk, _| {
                    let res = rows_of(self.query(
                        "SELECT count(*) FROM rretl_residual \
                         WHERE run_id = $1 AND pk_enc = $2",
                        &[Value::Int(run_id), Value::Blob(pk_ref(pk))],
                    )?)?;
                    if res[0][0] == Value::Int(0) {
                        findings.push(format!(
                            "run {run_id}: row {pk:?} of `{table}.{column}` has NO \
                             residual — what was lost is gone, and unwinding that row \
                             is impossible"
                        ));
                    }
                    Ok(())
                })?;
            }
        }
        crate::rretl_store::fsck_stores(self, &mut findings)?;
        Ok(findings)
    }

    /// Every lineage row, oldest first.
    pub fn rretl_log(&self) -> Result<Vec<RretlLogRow>> {
        let bundle = self.engine.schema();
        if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
            return Ok(Vec::new());
        }
        let rows = rows_of(self.query(
            "SELECT run_id, lens, tbl, col, rows, outcome, error FROM rretl_lineage \
             ORDER BY run_id",
            &[],
        )?)?;
        rows.into_iter()
            .map(|r| {
                Ok(RretlLogRow {
                    run_id: as_int(&r[0])?,
                    lens: as_text(&r[1]),
                    table: as_text(&r[2]),
                    column: as_text(&r[3]),
                    rows: as_int(&r[4])?,
                    outcome: as_text(&r[5]),
                    error: as_text(&r[6]),
                })
            })
            .collect()
    }
}

pub(crate) fn as_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Error::Corrupt(format!("lineage: expected int, got {other:?}"))),
    }
}

pub(crate) fn as_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Both bookkeeping tables, created through the ordinary DDL path INSIDE the
/// run's own transaction (#95: DDL in a txn never mid-commits), so a first run
/// that fails leaves neither table nor debris.
///
/// Existence comes from the COMMITTED schema bundle, passed in by the caller —
/// `sqlite_master` is a shim-side view, not a native table, and each caller
/// opens a fresh transaction, so the committed view is the correct one at
/// every call site (this runs at most once per transaction, before any DDL).
/// The column lists the bookkeeping tables MUST have. A pre-existing user
/// table that merely shares the NAME is refused by name here, up front —
/// the alternative (adversarial-check finding 20) is a mid-run bind error at
/// best and inserts into compatible-but-wrong columns at worst.
const LINEAGE_SHAPE: [&str; 16] = [
    "run_id", "step_no", "lens", "forward_hash", "rex_hash", "inverse_hash", "tbl", "col",
    "source_hash", "output_hash", "residual_hash", "rows", "verified", "outcome", "error",
    "ts_micros",
];
const RESIDUAL_SHAPE: [&str; 3] = ["run_id", "pk_enc", "residual"];

pub(crate) fn shape_gate(have: &[(String, Vec<String>)], name: &str, want: &[&str]) -> Result<bool> {
    match have.iter().find(|(n, _)| n == name) {
        None => Ok(false),
        Some((_, cols)) => {
            if cols.iter().map(String::as_str).eq(want.iter().copied()) {
                Ok(true)
            } else {
                Err(Error::Unsupported(format!(
                    "a table named `{name}` already exists but is NOT rretl's bookkeeping \
                     table (columns {cols:?}, expected {want:?}) — rename or drop it; \
                     rretl will not write into a shape it does not own"
                )))
            }
        }
    }
}

pub(crate) fn ensure_lineage_tables(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    ensure_tables_from(s, have)
}

/// One RIGIDLY-typed column for a bookkeeping table. The tables are created
/// from SPECS, not SQL text, because the sqlite-affinity DDL path maps the
/// name `BLOB` to the TYPELESS column (sqlite semantics, correctly) — and a
/// typeless key column takes neither point probes nor range bounds (the
/// planner's `typeless` guard, for good reasons that do not apply to a
/// column only this module ever writes). Rigid `Blob` for `pk_enc` is what
/// turns every per-row residual lookup into a PkPoint and the chunk resume
/// into a composite PkRange.
pub(crate) fn spec_col(name: &str, ty: ColumnType) -> mpedb_sql::CreateColumnSpec {
    mpedb_sql::CreateColumnSpec {
        default_src: None,
        name: name.into(),
        ty,
        affinity: mpedb_types::Affinity::implied_by(ty),
        decl: if ty == ColumnType::Any { None } else { Some(ty.name().into()) },
        not_null: false,
        unique: false,
        pk: false,
        collation: mpedb_types::Collation::Binary,
        default: None,
        check: None,
        generated: None,
        references: None,
    }
}

pub(crate) fn create_bookkeeping(
    s: &mut WriteSession<'_>,
    name: &str,
    columns: Vec<mpedb_sql::CreateColumnSpec>,
    pk: &[&str],
) -> Result<()> {
    s.apply_ddl(mpedb_sql::DdlStmt::CreateTable(mpedb_sql::CreateTableSpec {
        if_not_exists: false,
        name: name.into(),
        columns,
        table_pk: pk.iter().map(|c| (*c).to_string()).collect(),
        uniques: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
    }))?;
    Ok(())
}

fn ensure_tables_from(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use ColumnType::{Any, Int64, Text};
    if !shape_gate(have, T_LINEAGE, &LINEAGE_SHAPE)? {
        create_bookkeeping(
            s,
            T_LINEAGE,
            vec![
                spec_col("run_id", Int64),
                spec_col("step_no", Int64),
                spec_col("lens", Text),
                spec_col("forward_hash", Text),
                spec_col("rex_hash", Text),
                spec_col("inverse_hash", Text),
                spec_col("tbl", Text),
                spec_col("col", Text),
                spec_col("source_hash", Text),
                spec_col("output_hash", Text),
                spec_col("residual_hash", Text),
                spec_col("rows", Int64),
                spec_col("verified", Int64),
                spec_col("outcome", Text),
                spec_col("error", Text),
                spec_col("ts_micros", Int64),
            ],
            &["run_id", "step_no"],
        )?;
    }
    if !shape_gate(have, T_RESIDUAL, &RESIDUAL_SHAPE)? {
        create_bookkeeping(
            s,
            T_RESIDUAL,
            vec![
                spec_col("run_id", Int64),
                spec_col("pk_enc", ColumnType::Blob),
                spec_col("residual", Any),
            ],
            &["run_id", "pk_enc"],
        )?;
    }
    Ok(())
}


impl crate::Database {
    pub(crate) fn committed_tables(&self) -> Result<Vec<(String, Vec<String>)>> {
        // The cached bundle refreshes on SQL compilation and txn begin, but
        // NEITHER has happened yet when a run starts by asking what exists —
        // a snapshot from before this handle's own last DDL commit would
        // re-CREATE the bookkeeping tables. Refresh explicitly.
        self.engine.refresh_schema_if_stale()?;
        Ok(self
            .engine
            .schema()
            .schema
            .tables
            .iter()
            .filter(|t| !t.dead)
            .map(|t| {
                (t.name.clone(), t.columns.iter().map(|c| c.name.clone()).collect())
            })
            .collect())
    }
}

/// `max(run_id) + 1` inside the transaction — a counter, deliberately NOT a
/// content hash: two runs can produce identical bytes and must still be
/// distinguishable (§7).
pub(crate) fn next_run_id(s: &mut WriteSession<'_>) -> Result<i64> {
    let rows = rows_of(s.query("SELECT max(run_id) FROM rretl_lineage", &[])?)?;
    Ok(match rows.first().and_then(|r| r.first()) {
        Some(Value::Int(m)) => m + 1,
        _ => 1,
    })
}

pub(crate) struct LineageRow {
    pub(crate) run_id: i64,
    pub(crate) lens: String,
    pub(crate) forward_hash: String,
    pub(crate) rex_hash: String,
    pub(crate) inverse_hash: String,
    pub(crate) table: String,
    pub(crate) column: String,
    pub(crate) source_hash: String,
    pub(crate) output_hash: String,
    /// Chain over the run's persisted `(pk_enc, residual)` rows in `pk_enc`
    /// order; empty for runs that wrote none. The at-rest oracle for what
    /// apply stored — fsck checks it for EVERY standing run, buried included,
    /// and revert/putback refuse when it no longer matches.
    pub(crate) residual_hash: String,
    pub(crate) rows: i64,
    pub(crate) outcome: &'static str,
    pub(crate) error: String,
}

impl LineageRow {
    pub(crate) fn insert(&self, s: &mut WriteSession<'_>) -> Result<()> {
        s.query(
            "INSERT INTO rretl_lineage (run_id, step_no, lens, forward_hash, rex_hash, \
             inverse_hash, tbl, col, source_hash, output_hash, residual_hash, rows, \
             verified, outcome, error, ts_micros) VALUES ($1, 1, $2, $3, $4, $5, $6, $7, \
             $8, $9, $10, $11, $12, $13, $14, $15)",
            &[
                Value::Int(self.run_id),
                Value::Text(self.lens.clone()),
                Value::Text(self.forward_hash.clone()),
                Value::Text(self.rex_hash.clone()),
                Value::Text(self.inverse_hash.clone()),
                Value::Text(self.table.clone()),
                Value::Text(self.column.clone()),
                Value::Text(self.source_hash.clone()),
                Value::Text(self.output_hash.clone()),
                Value::Text(self.residual_hash.clone()),
                Value::Int(self.rows),
                Value::Int(VERIFIED_TOTAL),
                Value::Text(self.outcome.into()),
                Value::Text(self.error.clone()),
                Value::Int(now_micros()),
            ],
        )?;
        Ok(())
    }
}

fn call1(p: &Arc<Proc>, x: &Value) -> Result<Value> {
    crate::spellfn::call_spell_fn(p, std::slice::from_ref(x))
}

/// Stream `(pk, column)` over `table` in PK order, one bounded chunk at a
/// time, calling `f` per row. `f` gets the session back so it can write —
/// updates land BEHIND the scan position (the PK never changes; transforming
/// it is refused), so resume-by-`pk > last` never revisits or skips a row.
fn scan_pairs(
    s: &mut WriteSession<'_>,
    table: &str,
    pk_col: &str,
    column: &str,
    mut f: impl FnMut(&mut WriteSession<'_>, &Value, &Value) -> Result<()>,
) -> Result<u64> {
    let chunk = chunk_rows();
    let first = format!(
        "SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\" LIMIT {chunk}"
    );
    let next = format!(
        "SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" WHERE \"{pk_col}\" > $1 \
         ORDER BY \"{pk_col}\" LIMIT {chunk}"
    );
    let mut last: Option<Value> = None;
    let mut n = 0u64;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &[])?)?,
            Some(pk) => rows_of(s.query(&next, std::slice::from_ref(pk))?)?,
        };
        let got = rows.len();
        if got == 0 {
            return Ok(n);
        }
        for row in &rows {
            f(s, &row[0], &row[1])?;
            n += 1;
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            return Ok(n);
        }
    }
}

/// Stream `(pk_enc, residual)` for one run out of `rretl_residual`, in
/// `pk_enc` (memcmp) order — the table's OWN key order, which is what the
/// lineage `residual_hash` chain is defined over. `pk_enc` order and pk
/// VALUE order genuinely differ (value_bits are not keycode), which is why
/// every producer and consumer of the chain reads the TABLE, never re-sorts.
fn scan_residuals(
    s: &mut WriteSession<'_>,
    run_id: i64,
    mut f: impl FnMut(&Value, &Value) -> Result<()>,
) -> Result<u64> {
    let chunk = chunk_rows();
    let first = format!(
        "SELECT pk_enc, residual FROM rretl_residual WHERE run_id = $1 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let next = format!(
        "SELECT pk_enc, residual FROM rretl_residual WHERE run_id = $1 AND pk_enc > $2 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let mut last: Option<Value> = None;
    let mut n = 0u64;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &[Value::Int(run_id)])?)?,
            Some(pk) => rows_of(s.query(&next, &[Value::Int(run_id), pk.clone()])?)?,
        };
        let got = rows.len();
        if got == 0 {
            return Ok(n);
        }
        for row in &rows {
            f(&row[0], &row[1])?;
            n += 1;
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            return Ok(n);
        }
    }
}

/// The `residual_hash` chain for `run_id`, from the PERSISTED rows (the row
/// codec is inside the trust boundary, so it is inside the hash). Empty
/// string when the run wrote no residuals is the bijective convention.
fn residual_chain(s: &mut WriteSession<'_>, run_id: i64) -> Result<(String, u64)> {
    let mut chain = CanonChain::new();
    let n = scan_residuals(s, run_id, |pk_enc, r| {
        chain.push(pk_enc);
        chain.push(r);
        Ok(())
    })?;
    Ok(if n == 0 { (String::new(), 0) } else { (chain.hex(), n) })
}

/// Read-only sibling of [`scan_pairs`] for fsck, which must not take the
/// writer lock.
fn scan_pairs_ro(
    db: &crate::Database,
    table: &str,
    pk_col: &str,
    column: &str,
    mut f: impl FnMut(&Value, &Value) -> Result<()>,
) -> Result<u64> {
    let chunk = chunk_rows();
    let first = format!(
        "SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\" LIMIT {chunk}"
    );
    let next = format!(
        "SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" WHERE \"{pk_col}\" > $1 \
         ORDER BY \"{pk_col}\" LIMIT {chunk}"
    );
    let mut last: Option<Value> = None;
    let mut n = 0u64;
    loop {
        let rows = match &last {
            None => rows_of(db.query(&first, &[])?)?,
            Some(pk) => rows_of(db.query(&next, std::slice::from_ref(pk))?)?,
        };
        let got = rows.len();
        if got == 0 {
            return Ok(n);
        }
        for row in &rows {
            f(&row[0], &row[1])?;
            n += 1;
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            return Ok(n);
        }
    }
}

/// Read-only sibling of [`residual_chain`] for fsck.
fn residual_chain_ro(db: &crate::Database, run_id: i64) -> Result<(String, u64)> {
    let chunk = chunk_rows();
    let first = format!(
        "SELECT pk_enc, residual FROM rretl_residual WHERE run_id = $1 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let next = format!(
        "SELECT pk_enc, residual FROM rretl_residual WHERE run_id = $1 AND pk_enc > $2 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let mut chain = CanonChain::new();
    let mut last: Option<Value> = None;
    let mut n = 0u64;
    loop {
        let rows = match &last {
            None => rows_of(db.query(&first, &[Value::Int(run_id)])?)?,
            Some(pk) => rows_of(db.query(&next, &[Value::Int(run_id), pk.clone()])?)?,
        };
        let got = rows.len();
        if got == 0 {
            return Ok(if n == 0 { (String::new(), 0) } else { (chain.hex(), n) });
        }
        for row in &rows {
            chain.push(&row[0]);
            chain.push(&row[1]);
            n += 1;
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            return Ok((chain.hex(), n));
        }
    }
}

/// The at-rest residual integrity gate shared by revert and putback: the
/// residual rows are NOT user-editable state — edits happen in the column —
/// so the set must hash to exactly what the apply wrote. Without this, a
/// tampered residual can survive BOTH PutRes halves (mag/sgn: flip the
/// stored sign bit and forward(inverse(y, r')) == y ∧ rex(x') == r' both
/// hold) and putback silently restores a value the user never had.
fn residual_gate(s: &mut WriteSession<'_>, run_id: i64, want: &str, verb: &str) -> Result<()> {
    let (got, _) = residual_chain(s, run_id)?;
    if got != want {
        return Err(Error::Corrupt(format!(
            "run {run_id}'s residuals no longer hash to what the apply wrote — \
             tampered or deleted at rest; {verb} would fabricate data, refused"
        )));
    }
    Ok(())
}

/// The apply body, inside the caller's transaction. Returns the report and the
/// success lineage row for the caller to insert before commit.
fn apply_in(
    s: &mut WriteSession<'_>,
    pair: &str,
    lens: &crate::lens::RretlLens,
    table: &str,
    column: &str,
    target: &Target,
    committed_tables: &[(String, Vec<String>)],
) -> Result<(RretlReport, LineageRow)> {
    ensure_tables_from(s, committed_tables)?;

    // Runs STACK: applying pair B on top of pair A's output is the chained
    // form the (run_id, pk) residual key was designed for, and run N+1's
    // source hash is automatically run N's output domain (possibly edited).
    // The discipline lives on the way DOWN instead: revert/putback unwind
    // strictly LIFO — see `lifo_gate`.

    let run_id = next_run_id(s)?;
    let pk_col = &target.pk_col;

    // Pass 1, streaming (O(chunk) heap regardless of table size): transform,
    // persist, and chain — the chains see the same pk-ordered stream a
    // one-shot scan would produce, because chunk resume is `pk > last` on a
    // key the run never changes.
    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut source = CanonChain::new();
    let mut output = CanonChain::new();
    let mut seen: std::collections::HashMap<[u8; 32], Vec<u8>> = std::collections::HashMap::new();
    let mut diag_capped = false;
    let mut residuals = 0u64;

    let n_rows = scan_pairs(s, table, pk_col, column, |s, pk, x| {
        // A row the pair refuses ABORTS the whole run, with the row named.
        // Skipping it would leave transformed and untransformed values
        // indistinguishable in one column — Cambria's grey zone, per-row.
        let y = call1(&lens.forward, x).map_err(|e| {
            Error::Unsupported(format!(
                "`{pair}` refuses row {pk:?} of `{table}.{column}` (value {x:?}: {e}); \
                 the run is aborted — a partially transformed column is worse than none"
            ))
        })?;
        // Type gate against the rigid column (a type-changing pair needs an
        // Any column; ALTER COLUMN does not exist yet).
        if let Some(yt) = y.column_type() {
            if target.col_type != ColumnType::Any && yt != target.col_type {
                return Err(Error::Unsupported(format!(
                    "`{pair}` maps {x:?} to {y:?}, which does not fit `{table}.{column}` \
                     (declared {}); a type-changing pair needs an `any` column",
                    target.col_type.name()
                )));
            }
        }
        let r = match &lens.rex {
            Some(rex) => Some(call1(rex, x).map_err(|e| {
                Error::Unsupported(format!(
                    "`{pair}`'s rex refuses row {pk:?} (value {x:?}: {e}); the run is aborted"
                ))
            })?),
            None => None,
        };
        // The collision diagnosis on REAL data, same framing as registration
        // (§12.2 attack 1 / finding 10) — with one apply-only refinement the
        // randomized chain test caught as a WRONG REFUSAL: two rows holding
        // the SAME source value legitimately map to the same (y, r), and
        // recovery is per row via (run_id, pk). A collision is only real when
        // DIFFERENT sources land on one image — that is the unrecoverable
        // case. So the key maps to the source bits, and only a source
        // MISMATCH aborts. (Without this, any column with duplicate values
        // could never be applied at all.) The map is bounded: past the cap
        // the DIAGNOSTIC stops growing and the total verification below is
        // what reports a real collision — fail-safe either way.
        let key = {
            let mut c = CanonChain::new();
            c.push(&y);
            if let Some(r) = &r {
                c.push(r);
            }
            *c.0.finalize().as_bytes()
        };
        let x_bits = crate::lens::value_bits(x);
        if let Some(prev_x) = seen.get(&key) {
            if *prev_x != x_bits {
                return Err(Error::Unsupported(format!(
                    "`{pair}` maps two DIFFERENT source values of `{table}.{column}` to \
                     the same (value, residual) — at most one could be recovered; the \
                     run is aborted (row {pk:?})"
                )));
            }
        } else if seen.len() < COLLISION_DIAG_CAP {
            seen.insert(key, x_bits);
        } else {
            diag_capped = true;
        }

        source.push(x);
        output.push(&y);
        s.query(&update, &[y, pk.clone()])?;
        if let Some(r) = r {
            s.query(
                "INSERT INTO rretl_residual (run_id, pk_enc, residual) VALUES ($1, $2, $3)",
                &[
                    Value::Int(run_id),
                    Value::Blob(pk_ref(pk)),
                    r,
                ],
            )?;
            residuals += 1;
        }
        Ok(())
    })?;
    let source_hash = source.hex();
    let output_hash = output.hex();

    // Pass 2 — TOTAL verification before the commit that deletes the source
    // (commitment 4). Re-read what was PERSISTED — the column and the residual
    // rows — inside the same transaction, and hash the inverse stream against
    // the source hash. O(chunk) memory; Lepton's discipline without holding
    // the originals. This also runs for Bijective pairs: it is the Hermes
    // zero-check in database form — the residual-free claim is asserted on
    // every real row, not only on the probe corpus.
    let mut back = CanonChain::new();
    scan_pairs(s, table, pk_col, column, |s, pk, y| {
        let x = match &lens.rex {
            Some(_) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM rretl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(pk_ref(pk))],
                )?)?;
                let Some(r) = res.into_iter().next().and_then(|mut r| {
                    if r.is_empty() { None } else { Some(r.remove(0)) }
                }) else {
                    return Err(Error::Corrupt(format!(
                        "verification: residual row missing for {pk:?} inside the writing \
                         transaction"
                    )));
                };
                crate::spellfn::call_spell_fn(&lens.inverse, &[y.clone(), r])?
            }
            None => call1(&lens.inverse, y)?,
        };
        back.push(&x);
        Ok(())
    })?;
    if back.hex() != source_hash {
        let hint = if diag_capped {
            " (the collision diagnostic was capped on this table; a many-to-one \
             image is one possible cause)"
        } else {
            ""
        };
        return Err(Error::Corrupt(format!(
            "total verification FAILED: inverse of the transformed `{table}.{column}` does \
             not reproduce the source (run {run_id}){hint}; the transaction is rolled back \
             and the column is untouched"
        )));
    }

    // Pass 3 — the residual set's OWN identity, from the persisted rows in
    // pk_enc order (the residual table's key order). It is what fsck verifies
    // at rest for buried runs, and what revert/putback gate on before
    // trusting a single residual — see `residual_gate` for the attack it
    // closes.
    let (residual_hash, _) = residual_chain(s, run_id)?;

    Ok((
        RretlReport { run_id, rows: n_rows, residuals },
        LineageRow {
            run_id,
            lens: pair.into(),
            forward_hash: lens.forward_hash.clone(),
            rex_hash: lens.rex_hash.clone().unwrap_or_default(),
            inverse_hash: lens.inverse_hash.clone(),
            table: table.into(),
            column: column.into(),
            source_hash,
            output_hash,
            residual_hash,
            rows: n_rows as i64,
            outcome: "applied",
            error: String::new(),
        },
    ))
}

/// Stacked runs unwind strictly LIFO: only the TOPMOST run still standing
/// (`outcome = 'applied'`) on a column may be reverted or putback. A buried
/// run's residuals describe a column state that later runs have transformed
/// away — unwinding it in place would feed the inverse values from the wrong
/// domain, and the hash gate would only catch that for revert, not putback.
fn lifo_gate(
    s: &mut WriteSession<'_>,
    run_id: i64,
    table: &str,
    column: &str,
) -> Result<()> {
    let top = rows_of(s.query(
        "SELECT max(run_id) FROM rretl_lineage WHERE tbl = $1 AND col = $2 \
         AND outcome = 'applied'",
        &[Value::Text(table.into()), Value::Text(column.into())],
    )?)?;
    match top.first().and_then(|r| r.first()) {
        Some(Value::Int(t)) if *t == run_id => Ok(()),
        Some(Value::Int(t)) => Err(Error::Unsupported(format!(
            "rretl run {run_id} is buried under run {t} on `{table}.{column}` — runs \
             unwind LIFO; revert or putback run {t} first"
        ))),
        _ => Err(Error::Corrupt(format!(
            "rretl run {run_id} claims outcome 'applied' but no applied run tops \
             `{table}.{column}`"
        ))),
    }
}

fn revert_in(
    db: &crate::Database,
    s: &mut WriteSession<'_>,
    run_id: i64,
) -> Result<RretlReport> {
    let bundle = db.engine.schema();
    if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
        return Err(Error::Unsupported("no rretl lineage in this database".into()));
    }
    // The lineage row is the residuals' meaning (§8.2): missing row = hard
    // error, never a NULL read.
    let lin = rows_of(s.query(
        "SELECT lens, tbl, col, source_hash, output_hash, outcome, residual_hash \
         FROM rretl_lineage WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?)?;
    let Some(lin) = lin.into_iter().next() else {
        return Err(Error::Unsupported(format!(
            "no rretl run {run_id} in the lineage — without its lineage row the residuals \
             are uninterpretable, and guessing is refused"
        )));
    };
    let (pair, table, column) = (as_text(&lin[0]), as_text(&lin[1]), as_text(&lin[2]));
    let (source_hash, output_hash, outcome) =
        (as_text(&lin[3]), as_text(&lin[4]), as_text(&lin[5]));
    let residual_hash = as_text(&lin[6]);
    match outcome.as_str() {
        "applied" => {}
        "reverted" => {
            return Err(Error::Unsupported(format!("rretl run {run_id} is already reverted")))
        }
        other => {
            return Err(Error::Unsupported(format!(
                "rretl run {run_id} has outcome `{other}`; only an applied run can be reverted"
            )))
        }
    }

    lifo_gate(s, run_id, &table, &column)?;
    let lens = db.load_lens_for_rretl(&pair)?;
    let target = db.resolve_target(&table, &column)?;
    let pk_col = &target.pk_col;

    // The residuals themselves are gated FIRST (they are not user-editable
    // state, and the final source-hash check would catch a tampered one only
    // with a far worse diagnosis).
    residual_gate(s, run_id, &residual_hash, "reverting")?;

    // The hash gate (commitment 8): if the column moved since the apply, the
    // stored residuals belong to values that no longer exist. Explicit error,
    // never silently wrong input.
    let mut current = CanonChain::new();
    scan_pairs(s, &table, pk_col, &column, |_, _, y| {
        current.push(y);
        Ok(())
    })?;
    if current.hex() != output_hash {
        return Err(Error::Unsupported(format!(
            "`{table}.{column}` changed outside the pipeline since run {run_id} — its hash \
             no longer matches the run's output; reverting would corrupt, so it is refused"
        )));
    }

    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut back = CanonChain::new();
    let n_rows = scan_pairs(s, &table, pk_col, &column, |s, pk, y| {
        let x = match &lens.rex {
            Some(_) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM rretl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(pk_ref(pk))],
                )?)?;
                // NULL as a residual VALUE would arrive here as Value::Null in
                // a present row and feed inverse(y, NULL); an ABSENT row is
                // the hard error. The distinction is the point.
                let Some(r) = res.into_iter().next().and_then(|mut r| {
                    if r.is_empty() { None } else { Some(r.remove(0)) }
                }) else {
                    return Err(Error::Corrupt(format!(
                        "residual row missing for run {run_id}, pk {pk:?} — what was lost \
                         is gone, and reverting without it would fabricate data"
                    )));
                };
                crate::spellfn::call_spell_fn(&lens.inverse, &[y.clone(), r])?
            }
            None => call1(&lens.inverse, y)?,
        };
        back.push(&x);
        s.query(&update, &[x, pk.clone()])?;
        Ok(())
    })?;
    if back.hex() != source_hash {
        return Err(Error::Corrupt(format!(
            "revert verification FAILED for run {run_id}: the inverted stream does not \
             match the recorded source hash; rolled back"
        )));
    }

    s.query("DELETE FROM rretl_residual WHERE run_id = $1", &[Value::Int(run_id)])?;
    s.query(
        "UPDATE rretl_lineage SET outcome = 'reverted' WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?;
    Ok(RretlReport { run_id, rows: n_rows, residuals: 0 })
}

/// The putback body — see [`crate::Database::rretl_putback`] for the contract.
fn putback_in(
    db: &crate::Database,
    s: &mut WriteSession<'_>,
    run_id: i64,
) -> Result<RretlReport> {
    let bundle = db.engine.schema();
    if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
        return Err(Error::Unsupported("no rretl lineage in this database".into()));
    }
    let lin = rows_of(s.query(
        "SELECT lens, tbl, col, outcome, residual_hash FROM rretl_lineage \
         WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?)?;
    let Some(lin) = lin.into_iter().next() else {
        return Err(Error::Unsupported(format!(
            "no rretl run {run_id} in the lineage — without its lineage row the residuals \
             are uninterpretable, and guessing is refused"
        )));
    };
    let (pair, table, column, outcome) =
        (as_text(&lin[0]), as_text(&lin[1]), as_text(&lin[2]), as_text(&lin[3]));
    let residual_hash = as_text(&lin[4]);
    match outcome.as_str() {
        "applied" => {}
        other => {
            return Err(Error::Unsupported(format!(
                "rretl run {run_id} has outcome `{other}`; only an applied run can be \
                 putback-inverted"
            )))
        }
    }

    lifo_gate(s, run_id, &table, &column)?;
    let lens = db.load_lens_for_rretl(&pair)?;
    let target = db.resolve_target(&table, &column)?;
    let pk_col = &target.pk_col;

    // Deliberately NO output-hash gate here — an edited column is the entire
    // point of putback. What replaces it: the RESIDUAL gate (the residual
    // table is not where edits happen, and a tampered residual can survive
    // both PutRes halves — see `residual_gate`), the per-row PutRes check
    // below, and the residual-set bookkeeping (a residual whose row is gone
    // = a kept deletion; a row without a residual = the refused creation
    // path).
    residual_gate(s, run_id, &residual_hash, "putback")?;

    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut consumed = 0u64;
    let n_rows = scan_pairs(s, &table, pk_col, &column, |s, pk, y| {
        let x = match &lens.rex {
            Some(rex) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM rretl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(pk_ref(pk))],
                )?)?;
                let Some(r) = res.into_iter().next().and_then(|mut r| {
                    if r.is_empty() { None } else { Some(r.remove(0)) }
                }) else {
                    // A row with no residual was INSERTED after the apply:
                    // inverting it would be the creation path inverse(y, ∅),
                    // which the design refuses (§4) — there is nothing true to
                    // re-attach. Named, with the fix.
                    return Err(Error::Unsupported(format!(
                        "row {pk:?} of `{table}.{column}` has no residual in run {run_id} — \
                         it was inserted after the apply, and inverting it without a \
                         residual would fabricate what was never lost; delete the row or \
                         revert it by hand, then retry"
                    )));
                };
                let x = crate::spellfn::call_spell_fn(&lens.inverse, &[y.clone(), r.clone()])
                    .map_err(|e| {
                        Error::Unsupported(format!(
                            "putback: inverse refuses row {pk:?} (edited value {y:?}: {e}); \
                             the run is aborted"
                        ))
                    })?;
                // PutRes, both halves, on the EDITED value. rex(x') == r is not
                // decoration: a pair whose residual does not survive the edit
                // would silently re-attach the WRONG lost half next time.
                let fwd = call1(&lens.forward, &x).map_err(|e| putres_err(pk, y, &e))?;
                if !crate::lens::same_value(&fwd, y) {
                    return Err(Error::Unsupported(format!(
                        "putback verification FAILED on row {pk:?}: the edit {y:?} is \
                         outside the pair's image — forward(inverse({y:?}, r)) = {fwd:?}, \
                         not {y:?}; the pair cannot carry this edit back, rolled back"
                    )));
                }
                let rx = call1(rex, &x).map_err(|e| putres_err(pk, y, &e))?;
                if !crate::lens::same_value(&rx, &r) {
                    return Err(Error::Unsupported(format!(
                        "putback verification FAILED on row {pk:?}: the residual does not \
                         survive the edit {y:?} — rex(x') = {rx:?} but the stored residual \
                         is {r:?}; rolled back"
                    )));
                }
                consumed += 1;
                x
            }
            None => {
                // Bijective: the creation path is total by construction (§4),
                // so a row inserted after the apply inverts like any other.
                let x = call1(&lens.inverse, y).map_err(|e| {
                    Error::Unsupported(format!(
                        "putback: inverse refuses row {pk:?} (edited value {y:?}: {e}); \
                         the run is aborted"
                    ))
                })?;
                let fwd = call1(&lens.forward, &x).map_err(|e| putres_err(pk, y, &e))?;
                if !crate::lens::same_value(&fwd, y) {
                    return Err(Error::Unsupported(format!(
                        "putback verification FAILED on row {pk:?}: \
                         forward(inverse({y:?})) = {fwd:?}, not {y:?}; rolled back"
                    )));
                }
                x
            }
        };
        s.query(&update, &[x, pk.clone()])?;
        Ok(())
    })?;

    // Residuals not consumed belong to rows deleted after the apply. The
    // deletion is an edit, and it survives: the residuals are discarded with
    // the rows they described. (The image story's crop.)
    s.query("DELETE FROM rretl_residual WHERE run_id = $1", &[Value::Int(run_id)])?;
    s.query(
        "UPDATE rretl_lineage SET outcome = 'putback' WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?;
    Ok(RretlReport { run_id, rows: n_rows, residuals: consumed })
}

fn putres_err(pk: &Value, y: &Value, e: &Error) -> Error {
    Error::Unsupported(format!(
        "putback verification could not run on row {pk:?} (edited value {y:?}): {e}; \
         rolled back"
    ))
}
