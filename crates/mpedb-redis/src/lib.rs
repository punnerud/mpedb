//! Redis RESP in front of mpedb, for overwrite-heavy blobs.
//!
//! This is a server. The engine is not: processes still mmap a file. The
//! server exists because a blob cache wants many clients, a shared writer
//! queue, and a reaper, and those need a process that stays up.
//!
//! What it is:
//!
//! * One table family per file. Strings in `str_blobs`, hash fields in
//!   `hash_blobs`, primary key the Redis key (and the field). zstd is applied
//!   before the write when it shrinks the bytes.
//! * N files when one writer lock is not enough. The file is the cluster slot
//!   modulo N. Changing N after data exists is refused.
//! * A static slot map and `MOVED`. `CLUSTER SLOTS` answers. There is no
//!   failover and no replica. That work is `design/DESIGN-DISTRIBUTED.md`.
//!
//! The command set is the one a blob client actually sends: `GET` `SET`
//! `DEL` `EXISTS` `EXPIRE` `TTL` `PTTL` `PERSIST` `HGET` `HSET` `HDEL`,
//! `PING`, `QUIT`, `SELECT 0`, and `CLUSTER SLOTS|NODES|INFO`.

mod cache;
mod cluster;
mod cmd;
mod compress;
mod config;
mod crc16;
mod resp;
mod server;
mod store;

pub use cluster::Cluster;
pub use config::ServeConfig;
pub use server::{now_ms, run, serve, State};
pub use store::{BlobStore, StoreOptions};
