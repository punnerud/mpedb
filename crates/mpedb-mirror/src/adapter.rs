//! The source-agnostic adapter boundary (DESIGN-MIRROR §5.4 adapter contract).
//! A `SourceAdapter` turns a live sqlite/PostgreSQL source into a stream of
//! coalesced per-PK net changes that the protocol applies to mpedb, and (from
//! M5) accepts local changes to push back. Cursors are opaque bytes the
//! protocol stores and compares but never interprets — sqlite's is a per-table
//! seq vector, PostgreSQL's a consecutive-snapshot record.

use mpedb_types::{Result, Value};

// (Value is used in the trait's read_table_rows return type below.)

/// A monotone, opaque source position. The protocol persists it in `mir\0cur`
/// atomically with the applied rows; only the adapter interprets its bytes.
pub type Cursor = Vec<u8>;

/// The net effect on one PK within a pull batch (state-based: intermediate ops
/// are coalesced away, so at most one entry per PK per batch).
#[derive(Clone, Debug, PartialEq)]
pub enum NetOpKind {
    /// The row's final image (already type-mapped to mpedb values, full row).
    Upsert(Vec<Value>),
    /// The PK no longer exists at the source.
    Delete,
}

/// One coalesced change to apply.
#[derive(Clone, Debug, PartialEq)]
pub struct NetOp {
    /// mpedb table id (the adapter has resolved the source table name).
    pub table_id: u32,
    /// The primary-key values (mpedb types), for keyed apply.
    pub pk: Vec<Value>,
    pub kind: NetOpKind,
}

/// A batch of net changes read from ONE source snapshot, with the cursor to
/// persist on a successful apply. Batch boundaries align to source-commit
/// boundaries (never split a source transaction) so mpedb readers never see a
/// torn source txn (§0).
#[derive(Clone, Debug)]
pub struct PullBatch {
    pub ops: Vec<NetOp>,
    /// Where a successful apply should resume.
    pub end_cursor: Cursor,
    /// The source's authority epoch, read in the same snapshot (fencing). None
    /// until the switch machinery (M6) installs the source-side state row.
    pub source_epoch: Option<u64>,
}

impl PullBatch {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// The pull side of a source adapter (§5.4). Push is added in M5.
pub trait SourceAdapter {
    /// Pull the next batch of net changes strictly after `from`, coalescing up
    /// to `max_ops` distinct PKs, reading every row image from a single source
    /// snapshot. Returns `None` when the source has no changes past `from`.
    fn pull(&mut self, from: &Cursor, max_ops: usize) -> Result<Option<PullBatch>>;

    /// The source's current change-log head — for lag reporting and the switch
    /// drain loop (§7). Comparable to a `PullBatch::end_cursor`.
    fn head(&mut self) -> Result<Cursor>;

    /// The cursor that means "nothing consumed yet" — the starting point for
    /// the first pull after import.
    fn zero_cursor(&self) -> Cursor;

    /// Read every current row of a mirrored table as mpedb values, in PK order.
    /// Used by the merge-diff / anti-entropy reconcile (§5.5) and no-touch mode.
    fn read_table_rows(&mut self, table_id: u32) -> Result<Vec<Vec<Value>>>;

    /// Apply local mpedb changes back to the source (write-back, §6): each
    /// `NetOp` is an upsert (full row image) or delete by PK. The adapter applies
    /// them in ONE source transaction and tags its own writes so they are
    /// filtered out of the next pull (echo suppression). Unconditional
    /// last-writer-wins from mpedb — used when mpedb holds authority (S6/S7,
    /// local-wins). Conflict-aware write-back is [`push_checked`].
    fn push(&mut self, ops: &[NetOp]) -> Result<()>;

    /// Conflict-aware write-back (§6, source-authoritative / source-wins). Within
    /// ONE source transaction, applies each op only if the source has NOT changed
    /// that PK since `from` (excluding this mirror's own echoes); a PK the source
    /// touched in `(from, now]` is a write-write conflict and is left un-applied
    /// (source wins). Uses lock-then-check: sqlite's single writer (BEGIN
    /// IMMEDIATE) makes check-then-write sound; PG takes the row lock, then tests
    /// the xid-window changelog (CONF#11/27).
    ///
    /// Returns a vector index-aligned to `ops`: `true` = applied, `false` =
    /// rejected because the source concurrently won. The default falls back to
    /// unconditional [`push`] (all applied) for adapters without a changelog.
    fn push_checked(&mut self, from: &Cursor, ops: &[NetOp]) -> Result<Vec<bool>> {
        let _ = from;
        self.push(ops)?;
        Ok(vec![true; ops.len()])
    }

    // ---- authority-switch fence (DESIGN-MIRROR §7) ----

    /// Ensure the source-side mirror-state row exists (idempotent), seeded with
    /// `epoch`/`authority` on first creation. The epoch on this row is the
    /// source-side anchor that fences the switch.
    fn ensure_source_state(&mut self, mirror_id: &str, epoch: u64, authority: &str) -> Result<()>;

    /// Read the source-side `(epoch, authority)`, or None if unset.
    fn read_source_state(&mut self, mirror_id: &str) -> Result<Option<(u64, String)>>;

    /// Compare-and-set the source epoch/authority: apply only `WHERE epoch =
    /// expected_epoch`. Returns `true` if it applied, `false` if fenced (a
    /// concurrent switch moved the epoch).
    ///
    /// This deliberately does NOT return a change-log head to re-seed the pull
    /// cursor onto. Re-seeding at cutover would skip every third-party source
    /// write committed between the drain and the cutover (they sit below the
    /// new head) — permanent silent divergence. The cursor stays put and echo
    /// suppression does the job it already does: both adapters filter their own
    /// `origin` tag out of the changelog on pull, so our drain-push rows are
    /// no-ops while foreign writes in that window are still pulled. Keeping the
    /// cursor untouched is also what makes [`crate::switch::recover`] a total
    /// function — there is no captured head to lose to a SIGKILL.
    fn cas_source_state(
        &mut self,
        mirror_id: &str,
        expected_epoch: u64,
        new_epoch: u64,
        new_authority: &str,
    ) -> Result<bool>;
}
