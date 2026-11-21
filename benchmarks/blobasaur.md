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
An earlier run the same day, before the two changes described under "What
this page changed in mpedb", is kept beside them as `…-before.tsv`.

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
| before | 4.6 ms | 7.5 ms |
| after repetition 1 | 4.6 ms | 8.5 ms |
| after repetition 2 | 4.9 ms | 12.4 ms |
| after repetition 3 | 4.7 ms | 10.0 ms |

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
| 1 | 1 445 [1 308–1 490], p50 2.6 ms | 996 [961–1 016], p50 4.0 ms | **3 425** [3 270–3 491], p50 0.8 ms |
| 2 | 1 429 [1 334–1 447], p50 2.2 ms | 1 062 [1 059–1 148], p50 2.9 ms | **3 247** [2 472–3 474], p50 0.9 ms |

mpedb-redis is 2.4× Blobasaur with one file and 2.3× with two. The batch is
worth about 1.45× to Blobasaur (996 → 1 445). The tails differ more than
the medians: Blobasaur's p99 is 86–97 ms and mpedb's is 6–16 ms.

A second file changes nothing for either server. The ranges overlap.

## Writes with a sync before the reply

`SET`, article value, writes per second.

| files | Blobasaur `full` | Blobasaur `full-b1` | mpedb `wal` | mpedb `commit` |
|---|---|---|---|---|
| 1 | 443 [440–462], p50 15.6 ms | 145 [144–154], p50 51 ms | **845** [836–870], p50 6.9 ms | 78 [74–85], p50 85 ms |
| 2 | 461 [442–473], p50 15.7 ms | 215 [212–216], p50 39 ms | **905** [882–912], p50 6.9 ms | 84 [74–89], p50 84 ms |

mpedb `wal` is 1.9× Blobasaur with one file and 2.0× with two. The two p99s
are the same, 59–62 ms. Blobasaur is 5.7× and 5.5× ahead of `commit`, which
flushes twice per batch and whose p99 is over 500 ms.

Both sides batch here. Take Blobasaur's batch away (`full-b1`) and it does
145 writes a second, one fsync per write, so the batch is worth 3× to it.
mpedb's median request is 6.9 ms against a 4.7 ms flush: a write waits for
the batch in flight and then rides the next one.

A second file adds 7 % to mpedb `wal` and nothing measurable to Blobasaur's
batched arm.

## The same pair with a small value

`SET`, small value (82 B compressed), one file.

| | Blobasaur `full` | mpedb `wal` | mpedb `commit` |
|---|---|---|---|
| writes/s | 558 [548–562], p50 13.5 ms | **1 084** [1 050–1 088], p50 5.9 ms | 122 [117–126], p50 56 ms |

mpedb `wal` is 1.9× Blobasaur here too. The ratio is the same at 82 B and
at 7 166 B, which says the value's size is no longer what decides it.

## Reads

`GET` of the keys just written, article value. Median request time, and
requests per second.

| files | Blobasaur | mpedb-redis |
|---|---|---|
| 1 | p50 0.64 ms, 12 169 [10 339–12 444] /s | **p50 0.40 ms**, 19 280 [17 949–19 389] /s |
| 2 | p50 0.77 ms, 10 030 [9 789–12 097] /s | **p50 0.41 ms**, 18 560 [18 184–19 323] /s |

These are the `normal` and `async` arms. Blobasaur's other arms read between
p50 0.64 and 0.81 ms and mpedb's all read at 0.40. The small value reads at
p50 0.40 ms on Blobasaur and 0.18 ms on mpedb-redis. mpedb-redis is 1.6× to
1.9× on the 64 KiB value, and the Python client is a large share of both
figures.

The earlier run the same day read about half as fast on both servers
(Blobasaur p50 1.20 ms, mpedb-redis 0.76 ms) with the same ratio. Nothing on
the read path changed between the two, and the flush probe was the same, so
that difference is the machine and it is not explained here.

## HSET, the routing control

No sync before the reply. Article value, hash name `prefs`.

| files | Blobasaur `normal` | mpedb `async` |
|---|---|---|
| 1 | 964 [823–1 008] | **2 950** [2 535–2 993] |
| 2 | 1 183 [1 182–1 185] | **2 680** [2 583–3 573] |

Blobasaur hashes the field, so two files spread these writes, and it gains
about 1.2× from the second file. mpedb-redis hashes `prefs`, so they all
land in one, and it does not move.

## What this page changed in mpedb

Three defects, each found by a run of this page and fixed in the tree the
tables above measured.

**`async` synced once per write.** The first run, a day earlier, had mpedb
`async` at 75 writes a second against Blobasaur's 1 667. A value over 4 KiB
took the engine's extent path, and an extent's payload is msynced before
the log record that names it. So `async`, which promises no sync before the
reply, did one sync per write for every value over 4 KiB. On one connection
that was 53–79 `SET` a second with the extent path and 3 068–3 996 without.
The extent default was measured on tmpfs, where that sync is free. It is
now off under `async` unless `extent_threshold_kb` asks for it.

**Group commit stopped at 824 bytes.** The run before this one had mpedb
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

Small values belong in the log record and large ones do not, and the two
cross near 120 KB on this disk. `mpedb-redis` now sets the threshold to
64 KiB under `wal`. The engine's default is unchanged. That moved the cell
from 272 to 845.

`commit` went from 18 to 78 on the article value through the second change
alone. It still flushes twice per batch.

## Reading it

Where neither server waits for the disk, mpedb-redis writes 2.3× to 2.4×
and reads 1.6× to 1.9× what Blobasaur does on this machine.

Where both wait for the disk, a write costs about one 5 ms flush and the
question is how many writes share it. Both servers now share it at this
value size, and mpedb `wal` is 1.9× to 2.0× ahead, at 82 bytes and at
7 KiB alike. Above 1 MiB of parameters an mpedb write still syncs alone,
and that size was not measured here.

A second file did not give either server the article's scaling. This box
has two cores and one disk; the article's had sixteen and tmpfs.
