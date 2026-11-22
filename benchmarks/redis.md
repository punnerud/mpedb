# mpedb-redis vs Redis

`mpedb-redis` speaks a subset of the Redis protocol over MPEdb files. Redis
keeps the data in memory and optionally logs commands to an append-only file.
This page runs both on one machine, three repetitions, and says where each is
ahead. The harness is [`redis/run.py`](redis/run.py). The raw rows are
[`redis/results-2026-11-22.tsv`](redis/results-2026-11-22.tsv) (Python client),
[`redis/c-client-2026-11-22.tsv`](redis/c-client-2026-11-22.tsv)
(`redis-benchmark`) and
[`redis/fsync-probe-2026-11-22.tsv`](redis/fsync-probe-2026-11-22.tsv).

Every number below is the median of three repetitions. Where the spread
matters, the lowest and highest are in brackets.

## What is compared with what

A variant is named for what the server has done before it replies.

| Redis | mpedb-redis | What survives |
|---|---|---|
| `memory`: no persistence | `none`: mapped file, no log, no sync | Redis loses everything at a restart. mpedb keeps every answered write through a killed process, and loses recent ones at a power cut |
| `aof-no`, `aof-everysec`: command logged, synced later | `async`: log record, synced later | Both keep every answered write through a killed process and lose a recent window at a power cut |
| `aof-always`: synced before the reply | `wal`: one log sync before the reply. `commit`: data and meta flushed | Everything answered |

mpedb is never the weaker side of a pair.

Two more mpedb arms are not durability classes:

- `nocache` is `none` with `cache_mb = 0`. It is the control arm for the
  string cache, which is on by default.
- `behind` is `none` with `write_behind_ms = 1000`. A plain `SET` is
  answered from memory and written within a second. A killed process loses
  that second. That is weaker than every other mpedb arm and every `aof`
  arm, and stronger than `memory`, so it is shown against `memory` only.

## This machine, and its disk

2026-11-22, Linux `dropthepost-test` 6.8.0-149-generic, KVM, 2 vCPU AMD
EPYC-Genoa, 3.7 GiB RAM. Data on a network block volume, XFS. Client and
server share the two cores. Redis 7.0.15 (jemalloc) with `redis-benchmark`
7.0.15. `mpedb-redis` 0.3.1, `cargo build --release`, rustc 1.98.0, one event
loop, one file, at the commit that adds this page.

A probe writes 4 KiB and calls `fdatasync` 200 times before the first
repetition and after each one. It is the null control: nothing in either
server can move it.

| when | p50 | p99 |
|---|---|---|
| before | 4.8 ms | 9.0 ms |
| after repetition 1 | 5.0 ms | 9.4 ms |
| after repetition 2 | 4.8 ms | 9.5 ms |
| after repetition 3 | 4.7 ms | 19.0 ms |

One flush is about 5 ms. An arm that flushes once per write cannot pass about
200 writes a second per writer here, and the arms that sync are measuring how
many writes share a flush.

## C client, 64-byte values

`redis-benchmark`, eight connections, 100 000 keys, `SET` then `GET`. The
`aof-always` and `wal` arms run a tenth of the requests.

### Pipeline 16

| pair | | `SET` /s | `GET` /s |
|---|---|---|---|
| no log | Redis `memory` | 684 697 [611 060–736 648] | 809 389 |
| | mpedb `none` | 105 296 [103 135–107 770] | 759 878 |
| | mpedb `nocache` (control) | 113 135 | 179 453 |
| | mpedb `behind` | 457 143 [434 783–462 214] | 564 812 |
| log, synced later | Redis `aof-no` | 505 050 [429 646–518 538] | 726 744 |
| | mpedb `async` | 45 238 [44 829–45 956] | 700 280 |
| synced before the reply | Redis `aof-always` | 8 079 | 727 273 |
| | mpedb `wal` | 5 656 | 480 769 |

- **Reads are level.** With the cache, mpedb `GET` is 0.94× Redis in the
  `memory` pair and 0.96× in the `aof-no` pair. The control arm shows what
  the cache is worth: 179 453 without it, 759 878 with it, 4.2×.
- **Redis is 6.5× ahead on `SET` without a log and 11× ahead with one.**
  A Redis `SET` is a hash table insert and, with the log, a buffered append
  of the command. An mpedb `SET` is a copy-on-write B-tree update, and under
  `async` it logs about one page image per row. That is the largest gap on
  this page.
- **`behind` closes most of it, for keys that repeat.** 457 143 against
  684 697 is 0.67× Redis `memory`. The next section is what happens when
  they do not repeat.
- **Synced writes: Redis 1.4× ahead.** Both are waiting for the disk. Redis
  puts every command that arrived in one event-loop turn in one flush.
- The `wal` `GET` figure is lower than the other cached arms. That arm's
  read phase is 200 000 requests, under half a second, and it has not been
  explained further.

### Pipeline 16, keys that do not repeat

The same run with a key space of 100 million, so two million `SET`s are two
million keys.

| | `SET` /s | `GET` /s |
|---|---|---|
| Redis `memory` | 566 572 | 758 725 |
| mpedb `none` | 103 167 | 155 836 |
| mpedb `behind` | 163 988 | 129 997 |

`behind` holds at most 64 MiB. When keys repeat, a held `SET` is replaced
before it is written and the file sees a fraction of the traffic. When they
do not, the buffer fills and the rate falls to what the file accepts. Over
this run that is 1.6× `none`, not the 4.3× of the table above. The `GET`s
here ask for random keys and almost all miss, and a miss goes to the file:
Redis is 4.9× ahead.

### No pipeline

| pair | | `SET` /s | `GET` /s |
|---|---|---|---|
| no log | Redis `memory` | 60 096 [51 356–62 259] | 51 905 |
| | mpedb `none` | 41 152 [38 733–43 358] | 54 621 |
| | mpedb `nocache` (control) | 43 354 | 53 740 |
| | mpedb `behind` | 46 283 | 57 320 |
| log, synced later | Redis `aof-no` | 52 263 | 50 803 |
| | mpedb `async` | 24 860 | 50 241 |
| synced before the reply | Redis `aof-always` | 739 [736–777] | 60 024 |
| | mpedb `wal` | 1 108 [1 072–1 143] | 62 735 |

- One request per round trip is bound by the system calls, so reads are
  equal and the cache changes nothing (control 53 740, cached 54 621).
- **mpedb `wal` is 1.5× Redis `aof-always`.** Eight connections share a
  flush in both. This is the one write row mpedb wins.
- Redis is 1.5× ahead on `SET` without a log and 2.1× ahead with one.

## Python client, 64 KiB values

Eight connections, one request per round trip, the 64 KiB JSON value of the
[Blobasaur page](blobasaur.md) (zstd shrinks it 9.1×) and a 2 KiB value that
shrinks to 82 B. 6 000 writes, then five seconds of reads. The Python client
is slower than both servers on reads (12 000–21 000 /s in every arm), so
reads are left out here; they are in the raw rows. Redis stores the value as sent.
mpedb-redis compresses it.

| pair | | writes /s | write p50 | resident memory |
|---|---|---|---|---|
| no log | Redis `memory` | 14 442 [13 468–15 448] | 0.49 ms | 414 MiB |
| | mpedb `none` | 9 202 [9 091–9 659] | 0.73 ms | 117 MiB |
| | mpedb `behind` | 8 610 | 0.43 ms | 195 MiB |
| log, synced later | Redis `aof-no` | 8 495 [8 338–9 034] | 0.82 ms | 514 MiB |
| | Redis `aof-everysec` | 7 206 [6 366–8 532] | 0.79 ms | 503 MiB |
| | mpedb `async` | 8 575 [8 317–9 221] | 0.72 ms | 117 MiB |
| synced before the reply | Redis `aof-always` | 340 [329–347] | 22.6 ms | 288 MiB |
| | mpedb `wal` | 746 [726–756] | 8.1 ms | 118 MiB |
| | mpedb `commit` | 373 [343–393] | 18.1 ms | 100 MiB |

Small value:

| pair | | writes /s | write p50 |
|---|---|---|---|
| log, synced later | Redis `aof-no` | 32 075 | 0.20 ms |
| | mpedb `async` | 20 932 | 0.29 ms |
| synced before the reply | Redis `aof-always` | 516 | 14.7 ms |
| | mpedb `wal` | 1 138 | 6.1 ms |

- **Large values with a log are level**: mpedb `async` 8 575 against Redis
  `aof-no` 8 495. Redis appends 64 KiB to its log per write, mpedb writes
  7 KiB after compression.
- **Synced writes: mpedb `wal` is 2.2× Redis on both values**, and `commit`
  is 1.1× on the large one.
- **Without a log Redis is 1.6× ahead** on the large value, and 1.5× ahead
  on the small one with a log.
- **Memory**: for 6 000 values of 64 KiB, Redis holds 414–514 MiB and
  mpedb-redis 117 MiB. Redis keeps every value in memory. mpedb-redis keeps
  the file's pages it has touched and a 64 MiB cache. `behind` was measured
  while it still held unwritten values.
- `behind` does not help here. The client waits for each reply before it
  sends the next, so the server is not what limits the write rate.

On disk, the mpedb directory is the preallocated 512 MiB file in every arm,
so it says nothing about the data. Redis's append-only file was 254–512 MiB
for the large value.

## Where each is ahead

| | ahead | by |
|---|---|---|
| Pipelined `SET`, no sync | Redis | 6.5× (`memory`), 11× (`aof-no`) |
| Pipelined `SET`, mpedb `behind`, repeating keys | Redis | 1.5× |
| Pipelined `SET`, synced | Redis | 1.4× |
| Pipelined `GET`, keys present | level | mpedb 0.94–0.96× |
| Pipelined `GET`, keys absent | Redis | 4.9× |
| Unpipelined `SET`, no sync | Redis | 1.5–2.1× |
| Unpipelined `SET`, synced | mpedb | 1.5× (64 B), 2.2× (2 KiB and 64 KiB) |
| Unpipelined `GET` | level | |
| 64 KiB values with a log, unpipelined | level | |
| Memory for 375 MiB of values | mpedb | 3.5–4.4× less |

Redis is a hash table in memory, and mpedb-redis is a B-tree in a file with
a cache in front of it. Reads of present keys no longer show the difference.
Writes do, wherever the client sends faster than one request per round trip
and the write does not wait for the disk. What would narrow that gap is a
log that records the change and not the page, which is not built.

## What this page changed in mpedb-redis

The first run of this harness, on 2026-11-21, had `SET` under `async` at
2 450 /s unpipelined and `GET` at 36 000 /s. Between that run and this page:

- The log checkpoints every 256 MiB (`wal_checkpoint_mb`) and not every 16.
- `SET` skips the hash-table probe in a store that has never held a hash,
  and zstd contexts are kept per thread.
- The listener is an event loop, and the `SET`s every connection has ready
  in one round are one transaction.
- `cache_mb` holds strings in memory in front of the files.
- `write_behind_ms` exists, off by default.

## Rerun

```
python3 -B benchmarks/redis/run.py \
    --mpedb <target>/release/mpedb-redis \
    --redis /usr/bin/redis-server --redis-benchmark /usr/bin/redis-benchmark \
    --disk <a directory on the disk to test> --c-requests 500000
```

About half an hour. Port 16379.
