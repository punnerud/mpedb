# mpedb-redis vs Blobasaur

[Blobasaur](https://github.com/iamd3vil/blobasaur) is the RESP server behind Zerodha's
[High-performance distributed blob storage](https://zerodha.tech/blog/high-performance-distributed-blob-storage/)
(Sarat Chandra, 19 Nov 2026). It stores blobs in N SQLite files and speaks a
subset of Redis. `mpedb-redis` is the same shape on MPEdb: one process, N
files, RESP in front, static Redis Cluster slots. This page is three
repetitions of that pair on one machine. The harness is
[`blobasaur/run.py`](blobasaur/run.py). The raw rows are
[`blobasaur/results-2026-11-21.tsv`](blobasaur/results-2026-11-21.tsv) and the
flush probe is
[`blobasaur/fsync-probe-2026-11-21.tsv`](blobasaur/fsync-probe-2026-11-21.tsv).
Two earlier runs the same day are kept beside them: `…-run1.tsv` is before
the last three changes under "What this page changed in mpedb", and
`…-run2.tsv` is before the last one.

## What the article published

Their table is a 16-core laptop, client and server on the same box, data on
tmpfs, 100 000 keys, 64 connections, 64 KiB JSON, synchronous writes,
`batch_size = 50`, and 1 600 MB of SQLite page cache in total. One file
against 16:

| | 1 shard | 16 shards |
|---|---|---|
| HSET | 7 740 ops/s, p99 10.7 ms | 25 324 ops/s, p99 8.3 ms |
| 50/50 | 14 227 ops/s, p99 10.5 ms | 29 538 ops/s, p99 8.5 ms |
| HGET | 67 128 ops/s, p99 2.1 ms | 70 884 ops/s, p99 2.3 ms |

That 3.3× is 16 files on 16 cores with the working set in tmpfs. It is the
reference that prompted this run. It is a different machine, a different
disk, and a different shard count, so it is not a row next to the numbers
below.

## This machine, and its disk

2026-11-21, Linux `dropthepost-test` 6.8.0-149-generic, KVM, 2 vCPU AMD
EPYC-Genoa, 3.7 GiB RAM, about 1.4 GiB already in swap. Data on a network
block volume, XFS, at `/mnt/HC_Volume_106406886`. rustc 1.98.0. Both binaries
are `cargo build --release` at the default release profile, no LTO.

- Blobasaur commit `fe58292254191bf7b95f80bf105e97b19cfd2d9a` (v0.8.0), SQLite
  3.46.0 as vendored by sqlx 0.8.6.
- `mpedb-redis` 0.3.1 at the commit that adds this page, which carries the
  changes this run prompted (below).

**The disk is most of the answer for every arm that syncs.** A probe writes
4 KiB and calls `fdatasync`, 200 times, before the first repetition and
after each one:

| when | p50 | p99 |
|---|---|---|
| before | 4.9 ms | 12.4 ms |
| after repetition 1 | 4.9 ms | 10.6 ms |
| after repetition 2 | 4.7 ms | 11.3 ms |
| after repetition 3 | 5.0 ms | 10.9 ms |

One flush is about 5 ms, so one writer that flushes once per write cannot
pass about 200 writes a second here. The probe is also the null control: it
did not move across the 17 minutes, so the machine did not drift under the
servers.

## Method

The same client for both servers: 8 connections, one request in flight per
connection, no pipeline. One server at a time, port 16379, a fresh data
directory per arm, one untimed warmup write. Workers connect, wait on a
barrier, and the clock starts when it opens. A write phase runs for 10 s or
20 000 writes, whichever comes first. The read phase then fetches the keys
just written for 5 s. Every GET and HGET body had to equal the payload; a
mismatch or an error reply fails the arm. None failed.

Every arm ran three times. The order is rotated between repetitions, and
the two systems alternate inside one repetition, so neither always runs
first. Each cell below is the median of three, with the lowest and highest
in brackets.

Two values. The article value is 64 KiB of chart JSON that `zstd -3`
compresses to 7 166 B, 9.1×, which is the compressibility the article
states. The small value is 2 KiB that compresses to 82 B. Both servers
compress with zstd level 3 before the write.

The client is Python and shares the two cores with the server. Read
throughput in particular is a number for this client on this box, and the
per-request median is the steadier figure.

## What each variant does before it replies

| variant | before the reply |
|---|---|
| Blobasaur `normal` | WAL, `synchronous = NORMAL`, `batch_size = 50`. No sync. |
| Blobasaur `normal-b1` | The same with `batch_size = 1`. |
| Blobasaur `full` | `synchronous = FULL`, `batch_size = 50`. One WAL fsync per batch. |
| Blobasaur `full-b1` | The same with `batch_size = 1`. One WAL fsync per write. |
| mpedb `async` | No sync. |
| mpedb `wal` | One log `fdatasync` per group-commit batch. Durable when acknowledged. |
| mpedb `commit` | Data and meta flushed, two flushes per batch. Durable when acknowledged. |

`normal` pairs with `async`: neither is durable when it replies. `full`
pairs with `wal` and with `commit`: all three are. mpedb is never the
weaker side of a pair.

Blobasaur runs with `async_write = false` and `batch_timeout_ms = 0`, so a
batch is whatever is queued when the writer comes round. With 8 connections
and one request each that is at most 8 writes, whatever `batch_size` says.
`cache_size_mb = 64` is Blobasaur's total, split over 18 pool connections
per shard: its logs report 3 MB per connection at one shard and 1 MB at two.

Two commands, because the servers hash different bytes:

- `SET <key>` is the engine comparison. Both shard on the key.
- `HSET prefs <key>` is the article's command and the routing control.
  Blobasaur hashes the field and spreads. mpedb-redis hashes the hash name
  and keeps every field of that hash in one file.

## Writes with no sync before the reply

`SET`, article value, writes per second.

| files | Blobasaur `normal` | Blobasaur `normal-b1` | mpedb `async` |
|---|---|---|---|
| 1 | 1 384 [1 312–1 476], p50 2.4 ms | 807 [677–919], p50 4.5 ms | **3 444** [3 197–3 610], p50 0.9 ms |
| 2 | 1 338 [1 306–1 457], p50 2.2 ms | 1 138 [1 056–1 175], p50 2.8 ms | **3 138** [2 848–3 898], p50 0.9 ms |

mpedb-redis is 2.5× Blobasaur with one file and 2.3× with two. The batch is
worth about 1.7× to Blobasaur with one file (807 → 1 384). The tails differ
more than the medians: Blobasaur's p99 is 89–105 ms and mpedb's is 6–24 ms.

A second file changes nothing for either server. The ranges overlap.

## Writes with a sync before the reply

`SET`, article value, writes per second.

| files | Blobasaur `full` | Blobasaur `full-b1` | mpedb `wal` | mpedb `commit` |
|---|---|---|---|---|
| 1 | 453 [433–458], p50 15.6 ms | 148 [138–154], p50 50 ms | **871** [839–909], p50 7.0 ms | 397 [384–417], p50 17.2 ms |
| 2 | 441 [436–456], p50 16.0 ms | 209 [200–215], p50 40 ms | **896** [792–904], p50 6.7 ms | 319 [308–333], p50 19.2 ms |

mpedb `wal` is 1.9× Blobasaur with one file and 2.0× with two. Its p99 is
47–79 ms against Blobasaur's 56–65 ms. Blobasaur is 1.14× ahead of `commit`
with one file and 1.38× with two. `commit` flushes twice per batch, data and
then meta, and Blobasaur's batch flushes once.

Both sides batch here. Take Blobasaur's batch away (`full-b1`) and it does
148 writes a second, one fsync per write, so the batch is worth 3× to it.
mpedb's median request is 7.0 ms against a 4.9 ms flush: a write waits for
the batch in flight and then rides the next one.

A second file changes nothing for mpedb `wal` or for Blobasaur's batched
arm: the ranges overlap. It costs `commit` a fifth, 397 to 319, and those
ranges do not overlap. Two files there are two pairs of flushes on one
disk.

## The same pair with a small value

`SET`, small value (82 B compressed), one file.

| | Blobasaur `full` | mpedb `wal` | mpedb `commit` |
|---|---|---|---|
| writes/s | 541 [522–546], p50 13.8 ms | **1 088** [1 069–1 109], p50 5.8 ms | 406 [404–416], p50 16.9 ms |

mpedb `wal` is 2.0× Blobasaur here too. The ratio is the same at 82 B and
at 7 166 B, which says the value's size is no longer what decides it.
`commit` does 406 here and 397 on the article value, so the same holds for
it. Blobasaur is 1.33× ahead of it.

## Reads

`GET` of the keys just written, article value. Median request time, and
requests per second.

| files | Blobasaur | mpedb-redis |
|---|---|---|
| 1 | p50 0.83 ms, 9 176 [9 123–10 695] /s | **p50 0.41 ms**, 18 576 [18 175–19 410] /s |
| 2 | p50 0.73 ms, 10 514 [10 468–11 909] /s | **p50 0.42 ms**, 18 380 [18 265–19 520] /s |

These are the `normal` and `async` arms. Blobasaur's other arms read between
p50 0.68 and 0.80 ms and mpedb's all read at 0.41. The small value reads at
p50 0.42 ms on Blobasaur and 0.18 ms on mpedb-redis. mpedb-redis is 1.7× to
2.0× on the 64 KiB value, and the Python client is a large share of both
figures.

The first run the same day read about half as fast on both servers
(Blobasaur p50 1.20 ms, mpedb-redis 0.76 ms) with the same ratio. Nothing on
the read path changed between the two, and the flush probe was the same, so
that difference is the machine and it is not explained here.

## HSET, the routing control

No sync before the reply. Article value, hash name `prefs`.

| files | Blobasaur `normal` | mpedb `async` |
|---|---|---|
| 1 | 969 [963–984] | **3 369** [3 161–3 389] |
| 2 | 1 142 [1 042–1 218] | **3 820** [3 327–4 031] |

Blobasaur hashes the field, so two files spread these writes, and it gains
about 1.2× from the second file. mpedb-redis hashes `prefs`, so they all
land in one. Its two ranges overlap.

## What this page changed in mpedb

Four defects, each found by a run of this page and fixed in the tree the
tables above measured.

**`async` synced once per write.** The first run, a day earlier, had mpedb
`async` at 75 writes a second against Blobasaur's 1 667. A value over 4 KiB
took the engine's extent path, and an extent's payload is msynced before
the log record that names it. So `async`, which promises no sync before the
reply, did one sync per write for every value over 4 KiB. On one connection
that was 53–79 `SET` a second with the extent path and 3 068–3 996 without.
The extent default was measured on tmpfs, where that sync is free. It is
now off under `async` unless `extent_threshold_kb` asks for it.

**Group commit stopped at 824 bytes.** The first run that day had mpedb
`wal` at 94 writes a second [31–173] on the article value, against
Blobasaur's 450, while the small value was already 2.1× ahead. An intent
slot holds 824 bytes of parameters, and anything larger took the writer
lock and synced alone. Parameters up to 1 MiB now spill to a file beside
the database and ride the same ring (`design/DESIGN.md` §5.3). That alone
moved the cell to 272 [204–278].

**A batch of extent values paid for the data file too.** With group commit
running, a batch still cost about 15 ms. A trace showed one msync of the
data file at about 10 ms and one `fdatasync` of the log at about 2.5 ms.
Eight writers, incompressible values, extents at the 4 KiB default against
off:

| value | extents at 4 KiB | extents off |
|---|---|---|
| 7 KB | 211, 212 | 723, 845 |
| 64 KB | 177, 229 | 360, 311 |
| 120 KB | 206, 221 | 232, 212 |
| 250 KB | 173, 163 | 114, 122 |
| 500 KB | 86, 111 | 55, 61 |

Small values belong in the log record and large ones do not. `mpedb-redis`
now sets the threshold to 64 KiB under `wal`. The engine's default is
unchanged. That moved the cell from 272 to 845.

**One changed page was written as a megabyte.** `commit` was at 78 writes a
second after the second change, with a median request of 85 ms. A trace
showed each of its two msyncs taking 11–16 ms, the one that covers a single
4 KiB meta page included, on a disk that flushes in 5 ms. The engine
pre-writes a new file with zeros so later writes are overwrites, and it did
so in 1 MiB writes. Linux gives a buffered write a page-cache folio as large
as the write, a store through the mapping dirties the whole folio, and msync
writes the whole folio. The mapping also asked for huge pages, which does
the same on a fault. A probe on this disk, one page stored and msynced, by
how the file was filled:

| prewrite | one page | three pages |
|---|---|---|
| 1 MiB writes | 12.7 ms | 26.5 ms |
| 64 KiB writes | 2.7 ms | 6.1 ms |
| 4 KiB writes | 2.1 ms | 3.9 ms |
| 1 MiB writes, cache dropped, huge-page advice | | 28.4 ms |
| 1 MiB writes, cache dropped, no advice | 2.2 ms | 4.9 ms |

The prewrite is now one page per write, and the advice is given only to a
mapping nothing syncs. `commit` went from 78 to 397. The table in the third
change, measured again on this build, eight writers:

| value | extents at 4 KiB | extents off |
|---|---|---|
| 7 KB | 401, 427 | 794, 751 |
| 64 KB | 301, 351 | 310, 332 |
| 120 KB | 265, 269 | 215, 213 |
| 250 KB | 194, 172 | 92, 105 |
| 500 KB | 80, 119 | 67, 62 |

The extent path is about twice as fast below 120 KB as it was, and the two
now cross near 64 KB, which is where the threshold already was.

## Reading it

Where neither server waits for the disk, mpedb-redis writes 2.3× to 2.5×
and reads 1.7× to 2.0× what Blobasaur does on this machine.

Where both wait for the disk, a write costs about one 5 ms flush and the
question is how many writes share it. Both servers now share it at this
value size, and mpedb `wal` is 1.9× to 2.0× ahead, at 82 bytes and at
7 KiB alike. mpedb `commit`, which keeps no log and flushes data and then
meta, is behind Blobasaur by 1.14× to 1.38×. Above 1 MiB of parameters an
mpedb write still syncs alone, and that size was not measured here.

A second file did not give either server the article's scaling. This box
has two cores and one disk; the article's had sixteen and tmpfs.
