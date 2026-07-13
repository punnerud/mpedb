//! mpedb storage engine: shared-memory COW B+tree with MVCC snapshots.
//!
//! Module map (see /DESIGN.md for the full architecture):
//! - [`pagestore`] — page pool abstraction (COW discipline)
//! - [`btree`] — copy-on-write B+tree
//! - [`row`] — row payload codec
//! - shm mapping, meta pages, reader table, transactions: in progress

pub mod btree;
pub mod engine;
pub mod pagestore;
pub mod ring;
pub mod row;
pub mod shm;

pub use engine::{CheckPrograms, Engine, ReadTxn, RowCursor, TxnSavepoint, WriteTxn};
pub use ring::{IntentRing, PendingIntent, RingResult, RING_PARAMS_CAP};
