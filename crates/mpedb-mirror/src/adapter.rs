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
}
