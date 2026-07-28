//! `mpedb etl` — apply a lens pair to a column IN PLACE, keep what was lost,
//! and be able to put it back (design/DESIGN-ETL.md §7/§11, #52 stage 2).
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
//! - `etl_residual (run_id, pk_enc) → residual` — what was lost, per row, per
//!   run. Keyed by run so two runs can never collide, `pk_enc` is the row's
//!   PK in canonical bytes. A residual VALUE of NULL is legal; a MISSING row
//!   is a hard error — confusing them would smuggle the refused creation path
//!   `inverse(y, ∅)` back in as a silent wrong answer.
//! - `etl_lineage (run_id, step_no) → …` — which pair (by CONTENT HASH, all
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

pub const T_LINEAGE: &str = "etl_lineage";
pub const T_RESIDUAL: &str = "etl_residual";

/// Verification levels, as recorded in `etl_lineage.verified` (§5: report what
/// was verified, never a bare "verified"). Apply always runs `total`.
const VERIFIED_TOTAL: i64 = 2;

/// Pre-flight row cap. One run is one transaction BY DESIGN (total
/// verification requires it), so an oversized apply would be OOM-killed —
/// deterministically, on every retry (§12.2 attack 4). A named refusal with
/// numbers beats an un-completable loop; chunked commits are NOT the fix
/// (they break verify-before-source-deletion).
const MAX_ETL_ROWS: u64 = 1_000_000;

/// What a run did, as `etl apply`/`etl revert` report it.
#[derive(Debug)]
pub struct EtlReport {
    pub run_id: i64,
    pub rows: u64,
    /// Residual rows written (0 for a bijective pair).
    pub residuals: u64,
}

/// One lineage row, as `etl log` reports it.
#[derive(Debug)]
pub struct EtlLogRow {
    pub run_id: i64,
    pub lens: String,
    pub table: String,
    pub column: String,
    pub rows: i64,
    pub outcome: String,
    pub error: String,
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Length-framed canonical hash chain — the ETL sibling of the lens layer's
/// joint collision key, and framed for the same reason.
struct CanonChain(blake3::Hasher);

impl CanonChain {
    fn new() -> Self {
        Self(blake3::Hasher::new())
    }
    fn push(&mut self, v: &Value) {
        let bits = crate::lens::value_bits(v);
        self.0.update(&(bits.len() as u64).to_le_bytes());
        self.0.update(&bits);
    }
    fn hex(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

fn rows_of(r: ExecResult) -> Result<Vec<Vec<Value>>> {
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
        if table == T_LINEAGE || table == T_RESIDUAL {
            return Err(Error::Unsupported(format!(
                "`{table}` is ETL bookkeeping; transforming it is refused"
            )));
        }
        let bundle = self.engine.schema();
        let t = bundle
            .schema
            .tables
            .iter()
            .find(|t| t.name == table && !t.dead)
            .ok_or_else(|| Error::Unsupported(format!("no table named `{table}`")))?;
        // v1: a declared single-column PK. Composite PKs need a composite
        // pk_enc (representable — value_bits framing — but untested territory),
        // and implicit-rowid tables expose no stable column to key residuals
        // on. Both are named refusals, not silent narrowing.
        if t.primary_key.len() != 1 {
            return Err(Error::Unsupported(format!(
                "`{table}` has a {}-column primary key; etl apply supports a declared \
                 single-column PK in stage 2",
                t.primary_key.len()
            )));
        }
        let pk_col = t.columns[t.primary_key[0] as usize].name.clone();
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
    pub fn etl_apply(&self, pair: &str, table: &str, column: &str) -> Result<EtlReport> {
        let lens = self.load_lens_for_etl(pair)?;
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

        let have = self.committed_tables();
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

    fn record_failed_run(&self, pair: &str, table: &str, column: &str, e: &Error) -> Result<()> {
        let mut s = self.begin()?;
        let have = self.committed_tables();
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
    pub fn etl_revert(&self, run_id: i64) -> Result<EtlReport> {
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
    pub fn etl_putback(&self, run_id: i64) -> Result<EtlReport> {
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

    /// Every lineage row, oldest first.
    pub fn etl_log(&self) -> Result<Vec<EtlLogRow>> {
        let bundle = self.engine.schema();
        if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
            return Ok(Vec::new());
        }
        let rows = rows_of(self.query(
            "SELECT run_id, lens, tbl, col, rows, outcome, error FROM etl_lineage \
             ORDER BY run_id",
            &[],
        )?)?;
        rows.into_iter()
            .map(|r| {
                Ok(EtlLogRow {
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

fn as_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Error::Corrupt(format!("lineage: expected int, got {other:?}"))),
    }
}

fn as_text(v: &Value) -> String {
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
fn ensure_tables_from(s: &mut WriteSession<'_>, have: &[String]) -> Result<()> {
    if !have.iter().any(|n| n == T_LINEAGE) {
        s.query(
            "CREATE TABLE etl_lineage (
               run_id INTEGER, step_no INTEGER,
               lens TEXT, forward_hash TEXT, rex_hash TEXT, inverse_hash TEXT,
               tbl TEXT, col TEXT,
               source_hash TEXT, output_hash TEXT,
               rows INTEGER, verified INTEGER,
               outcome TEXT, error TEXT, ts_micros INTEGER,
               PRIMARY KEY (run_id, step_no))",
            &[],
        )?;
    }
    if !have.iter().any(|n| n == T_RESIDUAL) {
        s.query(
            "CREATE TABLE etl_residual (
               run_id INTEGER, pk_enc BLOB, residual,
               PRIMARY KEY (run_id, pk_enc))",
            &[],
        )?;
    }
    Ok(())
}


impl crate::Database {
    fn committed_tables(&self) -> Vec<String> {
        self.engine
            .schema()
            .schema
            .tables
            .iter()
            .filter(|t| !t.dead)
            .map(|t| t.name.clone())
            .collect()
    }
}

/// `max(run_id) + 1` inside the transaction — a counter, deliberately NOT a
/// content hash: two runs can produce identical bytes and must still be
/// distinguishable (§7).
fn next_run_id(s: &mut WriteSession<'_>) -> Result<i64> {
    let rows = rows_of(s.query("SELECT max(run_id) FROM etl_lineage", &[])?)?;
    Ok(match rows.first().and_then(|r| r.first()) {
        Some(Value::Int(m)) => m + 1,
        _ => 1,
    })
}

struct LineageRow {
    run_id: i64,
    lens: String,
    forward_hash: String,
    rex_hash: String,
    inverse_hash: String,
    table: String,
    column: String,
    source_hash: String,
    output_hash: String,
    rows: i64,
    outcome: &'static str,
    error: String,
}

impl LineageRow {
    fn insert(&self, s: &mut WriteSession<'_>) -> Result<()> {
        s.query(
            "INSERT INTO etl_lineage (run_id, step_no, lens, forward_hash, rex_hash, \
             inverse_hash, tbl, col, source_hash, output_hash, rows, verified, outcome, \
             error, ts_micros) VALUES ($1, 1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
             $12, $13, $14)",
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

/// The apply body, inside the caller's transaction. Returns the report and the
/// success lineage row for the caller to insert before commit.
fn apply_in(
    s: &mut WriteSession<'_>,
    pair: &str,
    lens: &crate::lens::EtlLens,
    table: &str,
    column: &str,
    target: &Target,
    committed_tables: &[String],
) -> Result<(EtlReport, LineageRow)> {
    ensure_tables_from(s, committed_tables)?;

    // One unreverted apply per column (v1): a second would need residual
    // STACKING, which the (run_id, pk) key makes representable but nothing yet
    // supports (§12.2 attack 6). Representable is not supported.
    let prior = rows_of(s.query(
        "SELECT run_id FROM etl_lineage WHERE tbl = $1 AND col = $2 AND outcome = 'applied'",
        &[Value::Text(table.into()), Value::Text(column.into())],
    )?)?;
    if let Some(row) = prior.first() {
        return Err(Error::Unsupported(format!(
            "`{table}.{column}` already carries unreverted etl run {} — revert it first",
            as_int(&row[0])?
        )));
    }

    // Pre-flight size guard BEFORE materialising anything (§12.2 attack 4).
    let count = rows_of(s.query(&format!("SELECT count(*) FROM \"{table}\""), &[])?)?;
    let n = as_int(&count[0][0])? as u64;
    if n > MAX_ETL_ROWS {
        return Err(Error::Unsupported(format!(
            "`{table}` has {n} rows; etl apply is one transaction by design (total \
             verification before the source is destroyed) and caps at {MAX_ETL_ROWS} \
             rows — an over-sized run would be OOM-killed deterministically on every retry"
        )));
    }

    let run_id = next_run_id(s)?;
    let pk_col = &target.pk_col;
    let rows = rows_of(s.query(
        &format!("SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\""),
        &[],
    )?)?;

    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut source = CanonChain::new();
    let mut output = CanonChain::new();
    let mut seen: Vec<[u8; 32]> = Vec::with_capacity(rows.len());
    let mut residuals = 0u64;

    for row in &rows {
        let (pk, x) = (&row[0], &row[1]);
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
        // The collision diagnosis on REAL data, same order and same framing as
        // registration (§12.2 attack 1 / finding 10): 32 bytes per row.
        let key = {
            let mut c = CanonChain::new();
            c.push(&y);
            if let Some(r) = &r {
                c.push(r);
            }
            *c.0.finalize().as_bytes()
        };
        if seen.contains(&key) {
            return Err(Error::Unsupported(format!(
                "`{pair}` maps two rows of `{table}.{column}` to the same (value, residual) \
                 — at most one could be recovered; the run is aborted (row {pk:?})"
            )));
        }
        seen.push(key);

        source.push(x);
        output.push(&y);
        s.query(&update, &[y, pk.clone()])?;
        if let Some(r) = r {
            s.query(
                "INSERT INTO etl_residual (run_id, pk_enc, residual) VALUES ($1, $2, $3)",
                &[
                    Value::Int(run_id),
                    Value::Blob(crate::lens::value_bits(pk)),
                    r,
                ],
            )?;
            residuals += 1;
        }
    }
    let source_hash = source.hex();
    let output_hash = output.hex();

    // TOTAL verification before the commit that deletes the source
    // (commitment 4). Re-read what was PERSISTED — the column and the residual
    // rows — inside the same transaction, and hash the inverse stream against
    // the source hash. O(1) memory; Lepton's discipline without holding the
    // originals. This also runs for Bijective pairs: it is the Hermes
    // zero-check in database form — the residual-free claim is asserted on
    // every real row, not only on the probe corpus.
    let back_rows = rows_of(s.query(
        &format!("SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\""),
        &[],
    )?)?;
    let mut back = CanonChain::new();
    for row in &back_rows {
        let (pk, y) = (&row[0], &row[1]);
        let x = match &lens.rex {
            Some(_) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM etl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(crate::lens::value_bits(pk))],
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
    }
    if back.hex() != source_hash {
        return Err(Error::Corrupt(format!(
            "total verification FAILED: inverse of the transformed `{table}.{column}` does \
             not reproduce the source (run {run_id}); the transaction is rolled back and \
             the column is untouched"
        )));
    }

    Ok((
        EtlReport { run_id, rows: rows.len() as u64, residuals },
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
            rows: rows.len() as i64,
            outcome: "applied",
            error: String::new(),
        },
    ))
}

fn revert_in(
    db: &crate::Database,
    s: &mut WriteSession<'_>,
    run_id: i64,
) -> Result<EtlReport> {
    let bundle = db.engine.schema();
    if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
        return Err(Error::Unsupported("no etl lineage in this database".into()));
    }
    // The lineage row is the residuals' meaning (§8.2): missing row = hard
    // error, never a NULL read.
    let lin = rows_of(s.query(
        "SELECT lens, tbl, col, source_hash, output_hash, outcome FROM etl_lineage \
         WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?)?;
    let Some(lin) = lin.into_iter().next() else {
        return Err(Error::Unsupported(format!(
            "no etl run {run_id} in the lineage — without its lineage row the residuals \
             are uninterpretable, and guessing is refused"
        )));
    };
    let (pair, table, column) = (as_text(&lin[0]), as_text(&lin[1]), as_text(&lin[2]));
    let (source_hash, output_hash, outcome) =
        (as_text(&lin[3]), as_text(&lin[4]), as_text(&lin[5]));
    match outcome.as_str() {
        "applied" => {}
        "reverted" => {
            return Err(Error::Unsupported(format!("etl run {run_id} is already reverted")))
        }
        other => {
            return Err(Error::Unsupported(format!(
                "etl run {run_id} has outcome `{other}`; only an applied run can be reverted"
            )))
        }
    }

    let lens = db.load_lens_for_etl(&pair)?;
    let target = db.resolve_target(&table, &column)?;
    let pk_col = &target.pk_col;

    // The hash gate (commitment 8): if the column moved since the apply, the
    // stored residuals belong to values that no longer exist. Explicit error,
    // never silently wrong input.
    let rows = rows_of(s.query(
        &format!("SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\""),
        &[],
    )?)?;
    let mut current = CanonChain::new();
    for row in &rows {
        current.push(&row[1]);
    }
    if current.hex() != output_hash {
        return Err(Error::Unsupported(format!(
            "`{table}.{column}` changed outside the pipeline since run {run_id} — its hash \
             no longer matches the run's output; reverting would corrupt, so it is refused"
        )));
    }

    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut back = CanonChain::new();
    for row in &rows {
        let (pk, y) = (&row[0], &row[1]);
        let x = match &lens.rex {
            Some(_) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM etl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(crate::lens::value_bits(pk))],
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
    }
    if back.hex() != source_hash {
        return Err(Error::Corrupt(format!(
            "revert verification FAILED for run {run_id}: the inverted stream does not \
             match the recorded source hash; rolled back"
        )));
    }

    s.query("DELETE FROM etl_residual WHERE run_id = $1", &[Value::Int(run_id)])?;
    s.query(
        "UPDATE etl_lineage SET outcome = 'reverted' WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?;
    Ok(EtlReport { run_id, rows: rows.len() as u64, residuals: 0 })
}

/// The putback body — see [`crate::Database::etl_putback`] for the contract.
fn putback_in(
    db: &crate::Database,
    s: &mut WriteSession<'_>,
    run_id: i64,
) -> Result<EtlReport> {
    let bundle = db.engine.schema();
    if !bundle.schema.tables.iter().any(|t| t.name == T_LINEAGE && !t.dead) {
        return Err(Error::Unsupported("no etl lineage in this database".into()));
    }
    let lin = rows_of(s.query(
        "SELECT lens, tbl, col, outcome FROM etl_lineage \
         WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?)?;
    let Some(lin) = lin.into_iter().next() else {
        return Err(Error::Unsupported(format!(
            "no etl run {run_id} in the lineage — without its lineage row the residuals \
             are uninterpretable, and guessing is refused"
        )));
    };
    let (pair, table, column, outcome) =
        (as_text(&lin[0]), as_text(&lin[1]), as_text(&lin[2]), as_text(&lin[3]));
    match outcome.as_str() {
        "applied" => {}
        other => {
            return Err(Error::Unsupported(format!(
                "etl run {run_id} has outcome `{other}`; only an applied run can be \
                 putback-inverted"
            )))
        }
    }

    let lens = db.load_lens_for_etl(&pair)?;
    let target = db.resolve_target(&table, &column)?;
    let pk_col = &target.pk_col;

    // Deliberately NO output-hash gate here — an edited column is the entire
    // point of putback. What replaces it: the per-row PutRes check below, and
    // the residual-set bookkeeping (a residual whose row is gone = a kept
    // deletion; a row without a residual = the refused creation path).
    let rows = rows_of(s.query(
        &format!("SELECT \"{pk_col}\", \"{column}\" FROM \"{table}\" ORDER BY \"{pk_col}\""),
        &[],
    )?)?;

    let update = format!("UPDATE \"{table}\" SET \"{column}\" = $1 WHERE \"{pk_col}\" = $2");
    let mut consumed = 0u64;
    for row in &rows {
        let (pk, y) = (&row[0], &row[1]);
        let x = match &lens.rex {
            Some(rex) => {
                let res = rows_of(s.query(
                    "SELECT residual FROM etl_residual WHERE run_id = $1 AND pk_enc = $2",
                    &[Value::Int(run_id), Value::Blob(crate::lens::value_bits(pk))],
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
    }

    // Residuals not consumed belong to rows deleted after the apply. The
    // deletion is an edit, and it survives: the residuals are discarded with
    // the rows they described. (The image story's crop.)
    s.query("DELETE FROM etl_residual WHERE run_id = $1", &[Value::Int(run_id)])?;
    s.query(
        "UPDATE etl_lineage SET outcome = 'putback' WHERE run_id = $1 AND step_no = 1",
        &[Value::Int(run_id)],
    )?;
    Ok(EtlReport { run_id, rows: rows.len() as u64, residuals: consumed })
}

fn putres_err(pk: &Value, y: &Value, e: &Error) -> Error {
    Error::Unsupported(format!(
        "putback verification could not run on row {pk:?} (edited value {y:?}): {e}; \
         rolled back"
    ))
}
