# mpedb-redis

A resident Redis RESP server for overwrite-heavy blobs. The engine underneath
is still embedded: each shard is one `.mpedb` file, one writer lock, lock-free
readers. This crate is its own workspace, so `cargo test --workspace` in the
parent does not compile a listener.

```
cargo test --manifest-path crates/mpedb-redis/Cargo.toml
mpedb-redis serve --listen 127.0.0.1:6379 --data-dir /var/lib/mpedb-redis
```

There is no `AUTH`. Bind an address the operating system can keep private.
Point test scratch at a real volume with `MPEDB_TEST_DIR`; the suite opens
real files.

## What a file holds

Each shard file has two tables.

`str_blobs` is a string value. The primary key is the Redis key (`BLOB`).
`hash_blobs` is one hash field. The primary key is `(hkey, field)`, both
`BLOB`. A key is one or the other: `SET` of a live hash, or `HSET` of a live
string, is `WRONGTYPE`.

The payload columns are the same on both: `data`, `codec` (`0` raw, `1`
zstd), `created_at`, `updated_at`, `expires_at` (nullable, indexed), and
`version`. zstd runs before the write. Values under 64 bytes stay raw, and a
frame that does not shrink the bytes stays raw. A read refuses a frame that
would expand past the configured maximum.

`expires_at` is NULL when the key has no deadline. A NULL indexed column has
no index entry, which is the partial index the reaper scans. A read hides an
expired row immediately. The reaper deletes in bounded chunks, and the delete
is conditional on the deadline it observed, so a replacement that cleared the
deadline is left alone.

A hash shares one deadline. Every live field stores that same `expires_at`.
`HSET` of a new field copies a live sibling's deadline. An expired field that
is written again does not keep the old deadline.

Keys and fields are binary. The encoded primary key has to fit in 976 bytes
(`mpedb_core::btree::MAX_KEY`). A longer key is refused by name.

## Shards

The default is one file. `shards = N` creates `shard-0000.mpedb` through
`shard-NNNN.mpedb`. The file is the Redis Cluster slot modulo N, and the slot
is `CRC-16/XMODEM(hash tag) % 16384`. The hash tag is the inside of the first
`{...}` when that inside is non-empty, otherwise the whole key. Every field
of a hash therefore lands on the same file.

The directory writes a `META` file (`shards`, `size_mb`) before the database
files. A later start with a different count or file size is refused by name.
Shard files without `META` are refused too: the process does not guess.
`max_readers` is pinned at 1024 in the generated config, because that number
is frozen in the file header and decides where the data region starts.

N files are N writer locks. On `durability = none` that is what gives write
parallelism. On `commit` and `wal` the engine's group commit runs inside each
file, for writes of up to 1 MiB after compression. A larger value takes
the writer lock and syncs alone. `async` (the default) and `none` acknowledge before the sync and skip
the ring. One file with `commit` has not been measured against N files on the
same payload, durability, and hardware. The default stays 1 until that
measurement exists. This README does not claim a throughput.

## Commands

`GET` `SET` `DEL` `EXISTS` `EXPIRE` `PERSIST` `TTL` `PTTL` `HGET` `HSET`
`HDEL` `PING` `QUIT` `SELECT` `CLUSTER`.

`SET` accepts `NX`, `XX`, `EX`, and `PX`. An unknown option is named in the
error. `NX` and `XX` together are a syntax error. `EX` and `PX` must be
greater than zero. `NX` and `XX` that do not write return a null bulk, which
is the Redis answer, not an error. `SET` replaces the deadline.

`EXPIRE` with a non-positive number of seconds deletes a live key. `TTL`
returns `-2` when the key is missing, `-1` when it has no deadline, and
otherwise the remaining seconds rounded to the nearest, as Redis does. `PTTL` returns milliseconds.
`HSET` returns how many fields were added. An expired field counts as new.
`DEL` of a hash returns `1` when any field was live, and removes every
physical field of that key, expired leftovers included.

`SELECT` accepts database `0` only. `PING` with no argument answers `PONG`;
with one argument it echoes the bulk. `QUIT` closes the connection.

`CLUSTER SLOTS`, `CLUSTER NODES`, and `CLUSTER INFO` answer. A single-node
process owns slots `0-16383` and still answers `CLUSTER SLOTS`. A multi-node
map is static. A key this process does not own is `MOVED <slot> <announce>`.
On a multi-node map, a multi-key command whose keys hash to different slots
is `CROSSSLOT`, and that answer wins over `MOVED`. A single node owns every
slot and takes `DEL a b` as a standalone Redis would. There is no live reshard, no replica, and no
failover. Replication, leader election, and fencing stay in
[design/DESIGN-DISTRIBUTED.md](../../design/DESIGN-DISTRIBUTED.md).

Anything else, including an unknown command, is a named error.

## Concurrency

Sixty-four stripes, one per `slot % 64`. A check-then-write holds its stripe
across the statements and releases it before the next command. Compression
happens before the stripe is taken. A stripe is taken before a writer lock,
never after.

A command that writes one row is one autocommit statement. `HSET` with
several fields is one transaction: all of them land or none do. `DEL` with
several keys checks every key first, takes the stripes it needs in ascending
order, and runs one transaction per file. Keys in different files are
different databases and do not commit together.

A connection has no idle timeout, as Redis ships. A reply that a client has
stopped reading is abandoned after 60 seconds. Replies to a pipeline go out
in one write. A malformed request is answered with `-ERR Protocol error` and
the connection closes. An inline line such as `PING` is accepted.

Reads do not take the stripe. A `GET` that races a type change can answer nil
for that one call. It does not return the other type's bytes.

One process per data directory. The server holds `server.lock` with an
exclusive non-blocking `flock` for its whole life. A second process is
refused by name. On a non-Unix build the lock is a no-op, so that refusal
does not exist there. The Windows CI job does not build this crate.

The reaper runs on `cleanup_interval_ms` (default 1000) and deletes at most
`cleanup_chunk` string rows and that many hash fields per shard per pass
(default 200). An interval of `0` turns the reaper off.

## Config

[`example.toml`](example.toml) is a complete file. `--config FILE` stands
alone. The flags are the other way to start:

```
mpedb-redis serve --listen 127.0.0.1:6379 --data-dir DIR \
    [--shards N] [--durability none|commit|async|wal]
```

`listen` defaults to whatever you pass; the sample binds the loopback.
`shards` defaults to 1 (maximum 256). `durability` defaults to `async`.
`size_mb` defaults to 64. `zstd_level` is `1..=19`, default 3.
`max_value_mb` defaults to 16. `max_connections` defaults to 256.

A cluster section lists every node and the slot ranges it owns. Ranges are
`start-end`, inclusive, and must cover `0-16383` with no overlap. This
process's `cluster.announce` must appear on exactly one node. Addresses are
`host:port` or `[ipv6]:port`.

Durability and the listen address may change between starts. The shard count
and `size_mb` may not.
