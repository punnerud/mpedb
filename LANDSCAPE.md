# The landscape

Where mpedb sits among actively-maintained open-source databases, and — the
point of the exercise — **what they do better.**

Surveyed 2026-07-20. Star counts and activity read from the GitHub REST API on
that date, not scraped; "active" means commits in the last ~3 months and
releases still being cut, **not** star count. Nothing was installed or run: this
is desk research from repos, docs and headers. Vendor blog numbers are labelled
as such. Items that could not be reached are marked UNVERIFIED rather than
guessed.

mpedb has no vector index, so for most of §7 the honest verdict is **"not a
competitor, a possible direction"** rather than a feature-matrix row.

---

## 1. The finding

**Of roughly a hundred engines examined across both halves, the number of
actively-maintained open-source databases that let several unrelated OS
processes hold concurrent write transactions against one shared file, with no
daemon, is three** — and the third arrives by a completely different route.

| | project | note |
|---|---|---|
| live | **YottaDB** | daemonless, shared-memory, OCC. 88 stars — and it runs the US Veterans Affairs' VistA and core banking. Star counts are useless here. |
| live | **Firebird** Classic/SuperClassic | *"each database may be opened by multiple processes (including local ones for embedded access)"*, arbitrated by a shared-memory lock table. |
| live | **Lance** | optimistic concurrency over put-if-not-exists manifests with per-operation conflict rules. Not shared memory — but genuine multi-writer, on a local filesystem, with no daemon. |
| frozen | Berkeley DB (Transactional Data Store) | genuinely multi-process, AGPL since 2013, last release 2020. |

Multi-process **attach** with exactly one writer — architecturally mpedb's
family, one step short: **LMDB, libmdbx, Realm, sanakirja**.

Everything else takes an exclusive OS lock and admits one process, full stop:
RocksDB (*"Can I write to RocksDB using multiple processes? — **No.**"*),
LevelDB, pebble, WiredTiger, bbolt, redb, fjall, sled, canopydb, surrealkv,
Badger, Tkrzw, DuckDB, H2, HSQLDB, Derby, CozoDB, SurrealDB-embedded, GlueSQL.

> **In almost every README, "concurrent writers" means threads in one address
> space.** That is the sentence to keep in mind when reading any comparison.

---

## 2. Where mpedb is *not* differentiated

**The meta double-buffer is convergent evolution, not an invention.** Five
independent COW-B+tree-over-mmap engines arrived at the same commit anchor:

| project | the anchor |
|---|---|
| LMDB | `#define NUM_METAS 2` — *"Transaction N writes meta page #(N % 2)"* |
| Realm | `uint64_t m_top_ref[2]` plus a switch bit in `m_flags` |
| bbolt | two-phase write: dirty pages + `fsync`, then a new meta + `fsync` |
| redb | the "god byte" holding `primary_bit` |
| H2 MVStore | *"two file headers, which normally contain the exact same data"* |

That convergence is good evidence the design is right. It also means
`INNOVATIONS.md` should not present the meta flip as a differentiator — the
**multi-writer ring** is. The flip is table stakes for this family.

Same for COW page discipline, lock-free readers, and the freed-page tension.
bbolt states mpedb's freelist problem verbatim: *"Bolt uses copy-on-write so old
pages cannot be reclaimed while an old transaction is using them."* LMDB
documents the stale-reader problem mpedb's reader table exists to solve —
*"Stale reader transactions left behind by an aborted program cause further
writes to grow the database quickly."* We are answering known questions, and we
should say so.

---

## 3. Where mpedb's posture is genuinely stronger

**Almost everyone concedes, in their own documentation, that they can lose
committed transactions.**

| project | their words |
|---|---|
| H2 | *"In H2, after a power failure, a bit more than one second of committed transactions may be lost."* |
| HSQLDB | *"only the last transactions committed in the time interval may be lost. The default time interval is 0.5 second."* |
| RocksDB | default `sync=false`: *"the WAL write is not crash safe"* |
| fjall | default: *"flush to OS buffers, but **not** to disk"* |
| surrealkv | default is **Eventual** — *"not fsynced before returning from `commit()`"* |
| ClickHouse | *"written to the **filesystem**"* — page cache; `fsync_after_insert` is off by default |
| Redis | *"**Snapshotting is not very durable.**"* |
| Dragonfly | *"**Currently, Dragonfly does not support AOF.**"* — snapshots only, off by default |

mpedb's `wal` class is durable-on-ack by default when selected, and each class
has a stated contract. That is a stronger default posture than most of the field
ships — and `INNOVATIONS.md` §2.4's refusal to call `async` durable is the
discipline this table exists to justify.

**Torn writes: only four projects discuss them seriously.** PostgreSQL
(full-page writes, with the sector arithmetic spelled out), InnoDB (doublewrite
buffer), SQLite (the linear-sector-write assumption, stated *as* an assumption),
and Berkeley DB — whose answer is the sharpest contrast available:

> *"**Berkeley DB assumes pages are written atomically.** … if the operating
> system writes the first 16KB of the database page successfully, but crashes
> before being able to write the second 16KB, **the database has been corrupted
> and this corruption may or may not be detected during recovery.**"*

BDB assumes page atomicity and offers checksums as **detection**. Under COW a
torn page is never a page anyone reads. LMDB, redb, bbolt, Realm and H2 take the
same third way — but **assert it rather than test it**. Everyone else is silent.

---

## 4. Where we are behind — read this section twice

Published crash *testing* is the axis mpedb claims. Several projects are ahead
of us on it, and pretending otherwise would make the rest of this document
worthless.

| project | what they actually run |
|---|---|
| **RocksDB** | `db_crashtest.py`: blackbox `kill -9`, whitebox crash points, plus `sync_fault_injection`, `write_fault_one_in`, `metadata_write_fault_one_in`, and `open_*_fault_one_in` for the open path. Continuously, in CI. |
| **pebble** | strict-MemFS crash at the *k*-th write, then reset-to-synced-state and re-run without crashing; `errorfs` IO-error injection; a metamorphic suite across configurations. |
| **WiredTiger** | SIGKILL csuite (`random_abort`, `truncated_log`, `timestamp_abort`) **integrated with LazyFS** — real power loss, not process death. |
| **bbolt** | `dm-flakey` power-failure tests on **both ext4 and xfs**, in CI, then `bbolt check`. |
| **redb** | IO-error-injection fuzzer keeping **separate `reference` and `non_durable_reference` oracles**, so it asserts *which* commits must survive *at each durability class*, plus a direct assertion on the recovery bit. |
| **SQLite** | a crash VFS that *"randomly reorders and corrupts the unsynchronized write operations"*; I/O-error injection; and `mptest/` — *"testing the ability of independent processes to access the same SQLite database concurrently"*. |
| **DuckDB** | SIGKILL plus **LazyFS**: 4000 TPC-H refresh sets in a randomly-killed subprocess, restart, replay. No issues found. |
| **Turso** | own DST simulator with I/O fault injection and `--differential` against SQLite, plus Antithesis, plus TLA+/Quint models that found 12+ bugs *in SQLite itself*. |
| **PostgreSQL** | injection points (`--enable-injection-points`) for deterministic mid-path stalls. |

**The gap, stated plainly.** mpedb's `crash` harness SIGKILLs processes; the new
`powerloss --durability commit` replays the engine's own captured durability
trace and drops subsets. Both are process-level or model-level. **Neither drops
writes the operating system believes it made.** That is the difference between
"survives a kill" and "survives a power cut", and four projects above close it
with tooling we could adopt in days.

### The two techniques to steal, in priority order

1. **LazyFS or `dm-flakey`.** Both drop unsynced writes at the filesystem layer.
   LazyFS (FUSE, `clear-cache` / `torn-seq` / `torn-op`, VLDB 2024, laptop-
   runnable) is already used by DuckDB and WiredTiger. `dm-flakey` is cheaper to
   adopt and bbolt's `tests/dmflakey/` is a working reference. This targets
   exactly the `wal` path's fsync and torn-tail claims.
2. **redb's fuzzer shape.** Injected IO errors plus *durability-aware* oracles.
   mpedb already has the differential-oracle habit; wiring it to durability
   classes — asserting which commits must survive under `none` vs `commit` vs
   `wal` — is the missing piece, and it is a small piece.

**And the external-audit template**: Jepsen's TigerBeetle 0.16.11 analysis is
the bar for what "we test crashes" should mean — SIGKILL and SIGSTOP *plus* a
file-corruption nemesis doing bitflips, misdirected writes, lost writes, and
zone-targeted corruption of WAL headers and superblock.

---

## 5. The case study for why the harness is the product

**UnQLite issue #137.** Two processes writing one file, crashing inside
`unqliteFinalizeJournal`, on macOS and Windows. **Shipped from 2014 until it was
fixed in 1.2.1 on 2026-05-01 — and it was found by a user, not a test.** Their
own release note admits the tracking method: *"No known data corruption bug had
been reported since December 2017."*

Twelve years of multi-process journal corruption in a shipping engine, detected
by report. That is precisely the failure class `crash`, `powerloss` and
`mirror-collide` exist to catch.

---

## 6. Things to know before quoting anything

**The near-twin, disclosed before a reviewer finds it: `canopydb`.** Its feature
list reads almost word-for-word like mpedb's — MVCC, OCC, snapshot isolation,
lock-free reads, B+tree, WAL with async durability, bounded recovery. Two real
differences: it is **in-process only** (*"Only one instance of each Database can
be active at a given time"*) and **dormant since November 2025**. Its own README
says *"Do not trust it with production data."*

**Stars are not health.** `google/leveldb` 39.3k stars, zero commits in three
months, last release 2021. `spacejam/sled` 9.1k, dead since 2021, and its
ALICE-style write permuter (#1077) has been open and unbuilt since 2020.
`Snapchat/KeyDB` 12.5k, dead since April 2024. `boltdb/bolt` archived 2018.
`cozodb/cozo` 4.1k, no commit since December 2024. `apache/derby` formally
retired 2025-10-10. `realm/realm-core` vendor-deprecated, ~1 commit/year, and
still shipping on hundreds of millions of phones. Meanwhile Firebird (1.4k
stars) cut 234 commits in three months, and libmdbx's live mirror is the most
actively developed engine in the survey.

**Benchmark honesty, best to worst.** SQLite's `cpu.html` — cachegrind cycle
counts, *"repeatable to 7 or more significant digits"*, explicitly refusing
cross-engine claims — is the most honest posture in the field, and their old
`speed.html` is retired with *"The numbers here have become meaningless."*
Then pgbench (in-tree, self-measurement) and Datalevin (**cross-engine at
matched durability settings**, with a stated fairness rule). Then DuckDB's
`benchmark_runner` and SurrealDB's `crud-bench` — real packaged harnesses,
vendor-run numbers. Then ClickBench: reproducible, authored by the vendor being
measured. At the bottom, numbers in blog posts with no runnable harness — fjall,
libmdbx (charts from 2015), Speedb, sled — and ObjectBox, whose benchmark
**requires a physical Android phone**.

**Valkey's own documentation says the thing everyone should**: *"It is
absolutely pointless to compare the result of valkey-benchmark to the result of
another benchmark program."*

---

## 7. Vector, RAG and search

**The hypothesis I went in with was that this category has essentially no
crash-safety story. It is wrong for search and right for vectors**, and the
split is the useful part.

**Refuting it outright**: Lucene (`MockDirectoryWrapper.crash()` — *"simulates a
crash of OS or machine by overwriting unsynced files"* — plus a documented
physical mains-power-cut rig: **80 power cycles, zero corruption**, with an
fsync-disabled control run that *did* corrupt). Xapian (*"the database should
always be left in a valid state… even if the power is cut unexpectedly"*).
Vespa (*"Writes are guaranteed to be synced to durable storage prior to sending
a successful response"*). Elasticsearch and OpenSearch (`durability: request`
**by default** — fsync before ack). ParadeDB, which moved its Tantivy index into
Postgres block storage specifically to get WAL and MVCC, and runs its stress
suite under **Antithesis**. Manticore, whose binlog entries are checksummed so a
corrupt record halts replay rather than injecting garbage.

**Confirming it**: every ANN *library* — FAISS, hnswlib, Annoy, ScaNN, Voyager —
plus Chroma, Vald and Sonic. `Annoy::save()` opens with **`unlink(filename)`**:
the previous good index is deleted before the new one is written. hnswlib's
`saveIndex()` has no magic number, no version, no checksum, no rename, no fsync.
These are not databases and were never trying to be — but they are what half the
"vector databases" are built on.

### The differentiator is not crash safety

**It is multi-process writers, and only one project competes.** Every vector
server is single-writer-per-index; every library is single-writer-per-index. The
exception is **Lance**, which gets there by optimistic concurrency over
put-if-not-exists / rename-if-not-exists manifests, with per-operation conflict
rules and rebase — a completely different route, and it works on a local
filesystem. It is the entry that needs the most careful answer if mpedb ever
writes a "why not just use X" section.

So the claim to make is narrow and defensible: **crash-safe under SIGKILL *and*
multiple writer processes on one mapped file.** The first half has plenty of
company. The second half has essentially none.

### If mpedb ever grows a vector index, the design is already decided

**Copy sqlite-vec's storage decision without reservation.** Everything —
including its new DiskANN graph — lives in ordinary SQLite shadow tables, so
durability, atomicity and rollback are inherited entirely. There is no bespoke
durability path to get wrong. Put the vector index in ordinary mpedb trees and
you inherit COW, MVCC, group commit, multi-process writers and SIGKILL survival
for free.

**Copy pgvector's visibility model.** It does *not* make the HNSW graph
MVCC-correct. Aborted transactions' entries physically remain until VACUUM;
every scan re-checks visibility against the heap. That converts "keep an
approximate graph transactionally consistent" (very hard) into "keep it
conservatively over-inclusive" (easy). The index is an accelerator over
candidate row ids, never an authority.

**Do not copy pgvector's post-index filtering.** *"With approximate indexes,
filtering is applied after the index is scanned. If a condition matches 10% of
rows, with HNSW and the default `hnsw.ef_search` of 40, only 4 rows will match
on average."* Their `iterative_scan` is a retrofit. mpedb owns the planner and
the footprint — push the predicate into traversal from day one.

**And look hard at DiskANN3 before writing any graph code.** microsoft/DiskANN
is now *"a composable library"* whose premise is that the host database supplies
storage: implement the `DataProvider` trait and it uses your store for vectors
and adjacency lists. Shipped examples include a KV store and *"a Bf-tree
provider as an illustration of how to connect to a B-tree in your database."*
That is mpedb's shape exactly, and it also solves HNSW's delete problem —
`RepairGraph`-style full scans are where pgvector bleeds both cost and
corruption bugs (0.8.3, 0.8.2 and 0.3.1 all shipped index-corruption fixes).

### Two findings worth keeping

**Weaviate deliberately removed fsync from its HNSW commit log**, and documented
why: *"the index is rebuilt from the object store on any unclean shutdown…
Per-batch fsync was the dominant contributor to the ~15-30% import time
regression on slow disks."* Their object store is the system of record; the
index is an explicitly derived cache. That is an honest trade, and it is the
opposite of what their marketing implies.

**Chroma's is not honest.** The index directory is overwritten **in place** every
~1000 records — no fsync, no temp-file-plus-rename — while the sqlite watermark
is a separate write with no spanning transaction. A kill inside `index.save()`
can leave the index partially overwritten with the previous good copy already
destroyed. Recovery is "delete the index and replay the WAL" — except WAL
auto-purge has been on by default since v0.5.6, which their own cookbook admits
*"makes it hard or impossible to restore Chroma."* The docs say *"Chroma durably
persists each collection to disk."*

**And nobody is watching.** Jepsen has never analysed a single vector database —
not Qdrant, Milvus, Weaviate, Chroma, Vespa, Vald, Infinity or Lance. The only
search analysis is Elasticsearch 1.5.0 from 2015, never redone, and Elastic's
own resiliency page still lists Jepsen testing as work in progress.

### The uncomfortable comparison

**Xapian.** ~875 stars on a mirror, one maintainer for 25 years, and it has a
better durability contract than most of this document: one writer and many
readers enforced by `fcntl` locks — *"if a writer exits without being given a
chance to clean up… any fcntl() locks held will be automatically released by the
operating system so **stale locks can't happen**"* — plus a stated power-loss
guarantee. It publishes no performance marketing at all, so there is no vendor
number to discount. It will have better retrieval quality than mpedb's FTS for a
long time, and a comparable durability posture. It is the fairest comparison
available and the least flattering.

---

## 8. What each does better

The actual output of this exercise. One line each, for the engines worth
learning from.

| project | what it does that mpedb does not |
|---|---|
| SQLite | the most-tested software most people will run — 590× as much test code as library code, 100% MC/DC coverage, an unchanged file format since 2004 |
| PostgreSQL | three decades of optimizer maturity, extensions, replication/PITR, and full-page writes as a *stated* torn-write answer |
| DuckDB | analytical scan throughput: vectorized columnar execution, larger-than-memory queries, direct Parquet/CSV/JSON reading |
| RocksDB | sustained multi-terabyte random-write ingest with tunable compaction, and the crash-test rig to match |
| pebble | metamorphic testing — logically equivalent operations must agree across configurations |
| WiredTiger | independently tunable durability/logging/checkpoint policy, with an explicit application-failure vs system-failure matrix |
| bbolt | production mileage under etcd, Consul and Vault — plus dm-flakey power cuts in CI |
| redb | a polished typed zero-copy API, pluggable backends including WASI/wasm, and that fuzzer |
| LMDB | ~10k lines of C people bet an LDAP server on. Zero config, zero background threads |
| libmdbx | explicit DB geometry, LIFO page reclaim, an offline verifier, and a four-level sync ladder with per-mode corruption semantics spelled out |
| Turso | a whole modern testing apparatus already built and public — DST, Antithesis, TLA+/Quint, differential, fuzz — plus io_uring |
| YottaDB | decades of production at tens of thousands of concurrent processes, with replication and a real DBA toolchain |
| Firebird | four decades of enterprise SQL — PSQL, roles, cross-database queries, online incremental backup — in a database that can *also* be embedded |
| Datalevin | the best benchmark *methodology* in the survey: cross-engine at matched durability settings, with the fairness rule written down |
| ClickHouse | scan-heavy analytics — billions of rows per second on one box |
| Neo4j | native graph traversal: index-free adjacency, multi-hop patterns a relational plan would butcher |
| sanakirja | O(1)-ish forkable tables — branchy versioned data models that mpedb's single-lineage MVCC cannot express |
| Realm | shipped cross-process MVCC to hundreds of millions of phones, with inter-process change notifications |
| Lucene | 25 years of IR correctness under randomized adversarial testing — and a physical power-cut rig |
| Xapian | probabilistic relevance with 25 years of craft, `fcntl` locks the kernel releases on death, and no performance marketing at all |
| Tantivy | Lucene-class inverted index in a linkable Rust crate — the engine to benchmark mpedb's FTS against |
| ParadeDB | the correct answer to "how do you put a Lucene-shaped index inside a transactional engine", plus Antithesis |
| Lance | multi-writer without a daemon, columnar+vector in one format, time travel |
| DiskANN3 | a composable ANN library whose premise is that the *host database* supplies the storage |
| pgvector | vectors that participate in real SQL, with index pages under the host's WAL |

---

## Phase 2 — running them

One engine at a time, per `#129`: record free disk before and after, scale the
dataset down deliberately and **say that the scale is not theirs**, clean up
completely, verify the disk returned, then the next. The tier that is actually
runnable on one machine without a cluster: sqlite, DuckDB, RocksDB, LMDB, redb,
sled, libSQL — plus whatever §7 nominates.

The output is not a leaderboard. **A cell where we lose and the reason is
understood is worth more than ten cells where we win.**
