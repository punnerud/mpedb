# mpedb-bench

Honest head-to-head benchmark of **mpedb vs SQLite vs PostgreSQL** on this
machine (2 cores, 7.6 GiB RAM, `/dev/shm` tmpfs + ext4 disk).

```
cargo run --release -p mpedb-bench            # full run, writes RESULTS.md
cargo run --release -p mpedb-bench -- --quick # smoke run, no RESULTS.md
cargo run --release -p mpedb-bench -- --only sqlite
cargo run --release -p mpedb-bench -- --io    # + bulk MiB/s vs a raw-Rust baseline
cargo run --release -p mpedb-bench -- --tmpfs /Volumes/ram   # macOS (no /dev/shm)
```

The formatted report goes to stdout; a full run also writes
[`RESULTS.md`](RESULTS.md) (machine info, versions, date, every cell,
caveats). Progress logs go to stderr.

## Engines and modes

| engine | how | none-class | commit-class |
|---|---|---|---|
| mpedb (this workspace) | embedded, ONE shared `Database`, `execute(hash, params)` plans | `.mpedb` file on tmpfs, `durability=none` | file on disk, `durability=commit` (msync before ack; intent-ring group commit under contention) |
| SQLite 3.45.0 | rusqlite `bundled` (system lib not linkable — no dev symlink/header), STRICT table, prepared statements, connection per thread | tmpfs, `synchronous=OFF`, `journal_mode=MEMORY` | disk, `synchronous=FULL`, `journal_mode=WAL` |
| PostgreSQL 16 | throwaway cluster (`initdb --auth=trust --locale=C`, `pg_ctl`, port 54329, unix socket only, system cluster untouched), `postgres` crate, client per thread | data dir on /dev/shm, `fsync=off`, `synchronous_commit=off` | data dir on disk, `fsync=on`, `synchronous_commit=on` |

Identical logical schema everywhere:
`users(id int64/bigint PRIMARY KEY, email text UNIQUE NOT NULL, age int64/bigint)`
(STRICT in SQLite). Every cell starts from a freshly created, freshly seeded
50,000-row table.

## Workloads

1. **point-insert** — single client, autocommit, N sequential-key inserts (prepared).
2. **point-select** — single client, N random PK lookups (prepared, warm).
3. **point-update** — single client, N random PK updates (autocommit).
4. **contended-writes** — 4 threads × autocommit inserts with distinct keys, 5 s.
5. **read-while-write** — 3 reader threads + 1 writer thread, 5 s; read AND write ops/s.

N self-calibrates per cell so each point cell runs roughly 2-10 s.
Reported per cell: ops, ops/s, p50/p99 latency in µs (measured around every call).

## Honesty requirements (also printed in every report header)

- **Class comparisons only.** "none-class" = no fsync guarantees; "commit-class"
  = durable on ack. Never compare across classes.
- **PostgreSQL has no true none-mode** — it always writes WAL; `fsync=off` only
  stops flushing it. Its none-class cells do strictly more work by design.
- **SQLite `journal_mode=MEMORY` loses rollback safety**: a crash mid-write can
  corrupt the file. mpedb `durability=none` stays process-crash-safe (COW +
  atomic meta flip). The none-class cells match on durability, not on crash safety.
- **mpedb/SQLite are embedded; PostgreSQL pays IPC + protocol per call.** A real
  architectural difference — not unfairness — and it dominates point-op latency.
- **Single machine, 2 cores, `--release` only** (never compare debug numbers).
- **No cherry-picking**: all cells are reported, including where mpedb loses.

## Known engine race found by this benchmark

With `durability=commit`, a reader that loads the `durable_txn` gate and is
descheduled while **two** durable commits land gets a spurious
`Corrupt("no valid meta page (both checksums invalid)")` from
`mpedb-core::shm::newest_meta`: both checksum-valid meta slots are newer than
its stale gate, so both get filtered. The database is not corrupt — a fresh
read reloads the monotone gate and succeeds. Reproduces in seconds here
(3 readers + 1 durable writer, 2 cores). The proper fix is a gate-reload
retry inside `newest_meta`; until then this benchmark's mpedb adapter retries
such reads (bounded at 100 attempts), counts them, and includes the retry
time in the measured read latency. The per-run retry count is printed in the
report caveats.

## Operational notes

- Scratch data lives in `/dev/shm/mpedb-bench-<pid>` and
  `target/release/mpedb-bench-scratch-<pid>`; both are removed on exit —
  including on panic — by directory guards, and the PostgreSQL guard runs
  `pg_ctl stop` before deleting its data dir. Only a SIGKILL of the bench
  process itself can leave leftovers (delete those directories by hand).
- The PostgreSQL unix socket for both configs sits on `/dev/shm` (short path;
  the 107-byte `sun_path` limit — sockets carry no data, the data-dir medium
  is what defines the mode).
- The system PostgreSQL cluster is neither used nor touched: the throwaway
  instance listens on no TCP address and a private socket directory.
