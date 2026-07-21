# minisqlite vs mpedb (incl. SQLite3 / PostgreSQL)

**Date:** 2026-07-21  
**minisqlite:** [github.com/cursor/minisqlite](https://github.com/cursor/minisqlite) @ `main`  
**mpedb:** this workspace @ `4926536`  
**Machines (minisqlite tests run on both):**
- **M3:** Apple M3 Pro, 11 cores, macOS 26.6 (Darwin 25.6.0) — unit, **sqllogictest**, cargo bench, micro
- **Linux:** AMD EPYC-Milan 2 cores, Linux 6.8 — unit, **sqllogictest**, cargo bench, micro  

**What is comparable without a C-API:** official **sqllogictest** corpus (SQL text in → results out).  
**What needs a C-API / host binding** (mpedb only here): CPython `test_sqlite3`, Django, SQLite TCL, TH3.  
**Own unit suites** are per-engine (cannot score minisqlite’s 5605 tests on mpedb or vice versa).

**SQLite / PostgreSQL / mpedb mpedb-bench numbers:** reused from `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md` and `RESULTS-linux-amd-epyc-milan-2c.md` (not re-run)

---

## 1. Short conclusion

| | **minisqlite** | **mpedb** | **stock SQLite 3.45** |
|---|---|---|---|
| Goal | Faithful SQLite reimplementation in Rust | Serverless file DB with **better concurrency** + rigid schema + modern features | Reference engine |
| On-disk format | **Full SQLite format 3** | Own format (+ SQLite attach/mirror) | format 3 |
| C-API / drop-in | **No** | **Yes** (`libmpedb_sqlite3`) | Full |
| CPython / Django | **N/A** (no C-API) | **459/467** · A **831/831** · queries **493/493** | stock baseline |
| Multi-process writers | **No** | **Yes** | Single writer + WAL readers |
| Own unit suite | **5605/0** (M3 + Linux) | `cargo test --workspace` | SQLite’s own (C/TCL; not this doc) |
| **sqllogictest** 7.4M (no C-API) | **99.999882 %** attempted (4 wrong / 2 errmis / 1 err) | **99.9765 %** + 0 genuine wrong after shim ([CORPUS-STATUS](design/CORPUS-STATUS.md)) | **99.999933 %** (3 wrong / 0 errmis / 1 err) — same harness |
| Speed | Strong; no prepare | **Ahead** on primary mpedb-bench cells | Reference in RESULTS |

**Choose minisqlite** if you want *“SQLite, but in Rust”* with byte-compatible files and a pure facade.  
**Choose mpedb** if you need C-API/Python/Django, multi-process writers, or stricter schema + mirror/RLS.
**Choose minisqlite** if you want *“SQLite, but in Rust”* with byte-compatible files and a pure facade.  
**Choose mpedb** if you need C-API/Python/Django, multi-process writers, or stricter schema + mirror/RLS.

---

## 2. Product surface

| Property | minisqlite | mpedb | SQLite 3.45 | PostgreSQL 16 |
|---|---|---|---|---|
| Language | Rust | Rust | C | C |
| Public API | `Connection::{open, open_in_memory, execute, query}` | Rust facade + CLI + PyO3 + **C-shim** | C-API + bindings | Client/server |
| Prepared statements | **No** (parse per `execute`/`query`) | **Yes** (`prepare` → content-hash plan) | Yes | Yes |
| C-API | **None** | `sqlite3_*` subset (~50 drop-in symbols) | Full | libpq / protocol |
| File format | SQLite format 3 bidirectional | Own `.mpedb` (+ SQLite attach/mirror) | format 3 | own |
| Multi-process write | No (explicit non-goal) | Yes | Single writer + WAL readers | Multi (server) |
| Concurrent readers | WAL snapshots **in-process** | Lock-free multi-process readers | WAL multi-process | Multi |
| Schema | SQLite-permissive | Rigid (fail early) | Permissive | Strong |
| FK / AUTOINCREMENT / fts | Full (incl. sqlite_sequence) | FK/AUTOINCREMENT honesty-refusals; FTS5 native | Full | Full |

---

## 3. Coverage: portable vs C-API suites

### 3.0 Which suites apply to whom

| Suite | Needs C-API? | stock SQLite | minisqlite | mpedb | PostgreSQL |
|---|---|---|---|---|---|
| Own unit / integration suite | No (per engine) | C/TCL suite (not re-run here) | **5605/0** | `cargo test --workspace` | `make check` (not re-run) |
| Official **sqllogictest** corpus | **No** — pure SQL | **measured** §3.2 | **measured** §3.2 | **measured** ([CORPUS-STATUS](design/CORPUS-STATUS.md)) | dialect-bound; not this corpus |
| CPython `test_sqlite3` | **Yes** | stock | **N/A** | **459/467** | N/A |
| Django frozen A / `queries` | **Yes** | via pysqlite | **N/A** | **831/831** · **493/493** | Django PG backend (not this doc) |
| SQLite TCL / TH3 | Yes / proprietary | yes | **N/A** | **N/A** | N/A |

### 3.1 Own unit suites (not cross-engine)

These are **implementation tests**, not a shared scoreboard. minisqlite’s 5605 tests call its Rust facade (and crate internals); they cannot be pointed at mpedb or libsqlite3 without a rewrite. mpedb’s suite likewise.

| Engine | Command | Result |
|---|---|---|
| **minisqlite** M3 | `cargo test --workspace --release` | **5605 passed, 0 failed** (`minisqlite-test-m3.log`) |
| **minisqlite** Linux | same | **5605 passed, 0 failed** (`minisqlite-test-linux.log`) |
| **mpedb** | `cargo test --workspace` | continuous CI / local (this tree) |
| **stock SQLite** | TCL suite / TH3 | not re-run in this document |

minisqlite suite makeup (for scale): ~2964 facade `conformance_*.rs` tests (SQL vs sqlite.org-transcribed expects) + format/durability fixtures + per-crate unit tests + seams.

### 3.2 Official **sqllogictest** corpus (no C-API — fair multi-engine)

| Field | Value |
|---|---|
| Corpus | [grahn/sqllogictest](https://github.com/grahn/sqllogictest) `test/**/*.test` |
| Files | **621 / 622** (`select5.test` excluded — same as mpedb [CORPUS-STATUS.md](design/CORPUS-STATUS.md)) |
| Harness (this round) | same SLT parser + MD5 + I/R/T rendering; engines answer as `sqlite` in `skipif`/`onlyif` |
| stock SQLite | rusqlite **bundled 3.45.0** (`sqlite_corpus`) — Linux |
| minisqlite | `minisqlite_corpus` — **M3 + Linux** (identical totals) |
| mpedb | `mpedb-testkit` `sqlite_corpus` + schema shim — M3 @ `b41b713` |

**Headline (same 621 files):**

| metric | **stock SQLite 3.45** (Linux) | **minisqlite** (M3 ≡ Linux) | **mpedb** (CORPUS-STATUS M3) |
|---|---:|---:|---:|
| records seen | **7 419 277** | **7 419 277** | 7 419 202 |
| skipped | **1 480 834** | **1 480 834** | 1 480 924 |
| **attempted** | **5 938 443** | **5 938 443** | 5 938 278 |
| **passed** | **5 938 439 — 99.999933 %** | **5 938 436 — 99.999882 %** | 5 936 882 — 99.9765 % |
| ├ statements | 210 082 | 210 080 | 208 748 |
| └ queries | 5 728 357 | 5 728 356 | 5 728 134 |
| md5-verified queries | **955 237** | **955 236** | 955 237 |
| **wrong answers** | **3** | **4** | 0 genuine (4 flagged cascade/shim) |
| **error mismatches** | **0** | **2** | **0** |
| engine errors (expected ok) | **1** | **1** | in refused/unsupported |
| refused / unsupported | 0 | 0 | 1 392 (mostly shim) |
| wall clock | **~70 s** (Linux) | **~86 s** M3 · **~187 s** Linux | longer (shim + mpedb paths) |

**Shared residual (harness / extreme floats):** stock SQLite and minisqlite both miss the same 3 `evidence/slt_lang_aggfunc.test` cases — `sum`/`total` on extreme i64 rendered with `%.3f` vs corpus text (`…5808` vs `…6000`). That is **not** a minisqlite-only bug.

**minisqlite-only extras vs stock SQLite (same harness):**

| delta | detail |
|---|---|
| +1 wrong | `random/…/slt_good_121.test` — `SELECT DISTINCT *` cross join, 81 values, MD5 mismatch |
| +2 errmis | expected-error vs success in the same aggregate edge area |
| same 1 engine error | `sum(x)` integer overflow where corpus expects a Real (also fails on stock with this harness path) |

**mpedb:** lower raw pass% mainly from **schema shim** (no free `CREATE TABLE` in the seed model) and a small set of engine gaps; after discounting runner artifacts, CORPUS-STATUS reports **zero genuine wrong answers**. Not directly comparable to the native-DDL harness used for minisqlite/sqlite3.

**PostgreSQL:** the public sqllogictest corpus is SQLite-dialect; we do **not** claim a PG score here (mpedb uses a separate 3-way differential generator for PG, not this corpus).

### 3.3 C-API host suites (mpedb only)

| Suite | minisqlite | mpedb | stock SQLite |
|---|---|---|---|
| CPython `test_sqlite3` | **N/A** (no C-API) | **459/467** (~98.3% of stock-passing); residual progress + non-goals — [C-API-COMPAT.md](C-API-COMPAT.md) | 467/467 stock |
| Django frozen A | **N/A** | **831/831** | via pysqlite |
| Django `queries` | **N/A** | **493/493** | via pysqlite |
| `libsqlite3` ABI | ❌ | ✅ `libmpedb_sqlite3.{so,dylib}` | full |

---

## 4. Speed — reused mpedb-bench RESULTS (SQLite + PG + mpedb)

**Not re-run** for SQLite/PostgreSQL/mpedb; numbers from committed RESULTS (2026-07-21).

### 4.1 Apple M3 Pro (`RESULTS-macos-apple-m3-pro-11c.md`)

**none-class** (tmpfs/ramdisk, no fsync guarantee):

| Cell | mpedb | SQLite 3.45 | PostgreSQL 16 | Turso 0.7 |
|---|---:|---:|---:|---:|
| point-insert ops/s | **233 477** | 117 077 | 33 470 | 47 023 |
| point-select ops/s | **1 263 331** | 325 857 | 40 773 | 236 345 |
| contended-writes ops/s | **144 620** | 101 173 | 116 121 | 41 029 |

**durable-on-ack batched** (100 rows/commit, disk, fullfsync/WAL):

| Engine | ops/s |
|---|---:|
| **mpedb** (wal) | **28 126** |
| SQLite FULL+WAL | 27 261 |
| PostgreSQL sc=on | 13 789 |

**M3 multi-run (7× interleaved, seed-once, 2026-07-21, mpedb-measure):**  
median **mpedb 29 748** > **SQLite 28 540** > **PG 13 600** (6/7 per-run wins).

### 4.2 Linux 2-core EPYC (`RESULTS-linux-amd-epyc-milan-2c.md`)

| Cell | mpedb | SQLite | PostgreSQL |
|---|---:|---:|---:|
| point-insert (none) | **186 956** | 42 199 | 14 217 |
| point-select (none) | **437 232** | 81 375 | 19 940 |
| contended-writes (none) | **125 397** | 35 033 | 37 574 |
| durable batched | **27 700** | 12 001 | 10 156 |

---

## 5. Speed — minisqlite measured

### 5.1 Own `cargo bench` — M3 (2026-07-21 re-run, through 1M)

Source: `~/mpedb-measure-results/minisqlite-bench-m3.log`

```text
scalability (wall-clock ms / peak heap KiB), sizes [1000, 10000, 100000, 1000000]
workload                              1000             10000            100000           1000000
point_lookup_indexed            0.6ms/123K       0.0ms/4554K      0.1ms/11886K     0.7ms/119426K
range_scan                      0.1ms/125K       0.1ms/4555K      0.8ms/11881K     7.4ms/119364K
equi_join                       0.4ms/558K       2.9ms/6960K     27.9ms/33122K   278.1ms/322757K
group_by                        0.3ms/129K       1.5ms/4559K     15.1ms/11885K   152.4ms/119368K
correlated_subquery             1.8ms/132K      17.9ms/4562K    178.8ms/11889K  1775.8ms/119372K

durability round-trip (commit survives reopen, rollback leaves no trace): ok
```

### 5.2 Own `cargo bench` — Linux EPYC 2c

Source: `~/mpedb-measure-results/minisqlite-bench-linux.log`

```text
scalability (wall-clock ms / peak heap KiB), sizes [1000, 10000, 100000, 1000000]
workload                              1000             10000            100000           1000000
point_lookup_indexed            0.1ms/123K       0.1ms/4554K      0.5ms/11886K     3.2ms/119426K
range_scan                      0.2ms/125K       0.4ms/4555K      2.6ms/11881K    29.7ms/119364K
equi_join                       1.5ms/558K      11.9ms/6960K     98.9ms/33122K  1199.9ms/322757K
group_by                        0.6ms/129K       4.0ms/4559K     45.4ms/11885K   445.6ms/119368K
correlated_subquery             5.1ms/132K      52.7ms/4562K    571.4ms/11888K  4786.7ms/119372K

durability round-trip (commit survives reopen, rollback leaves no trace): ok
process peak RSS: 367664 KiB
```

This is **scalability / plan-shape**, not the same cells as mpedb-bench (no shared harness). M3 is ~3–4× faster than the 2-core Linux host on the large sizes (as expected).

### 5.3 Microbench (same logical schema as mpedb-bench `users` table)

API: string-SQL per call (no prepare) — **unfavorable vs mpedb/SQLite prepared path**.

**M3 (2026-07-21 re-run)** — `minisqlite-micro-m3.log`:

| Cell | minisqlite (M3) | mpedb (RESULTS M3) | SQLite (RESULTS M3) |
|---|---:|---:|---:|
| In-memory point-insert | **~262 k** ops/s (n=50k) | ~233 k (none tmpfs) | ~117 k |
| Disk point-select (WAL) | **~235 k** ops/s (n=20k) | ~1.26 M (none) | ~326 k |
| Disk WAL batch 100/commit FULL | **~5.0 k** rows/s (n=10k) | ~28 k durable | ~27 k durable |

**Linux EPYC 2c** — `minisqlite-micro-linux.log`:

| Cell | minisqlite (Linux) | mpedb (RESULTS Linux) | SQLite (RESULTS Linux) |
|---|---:|---:|---:|
| In-memory point-insert | ~83 k ops/s (n=50k) | ~187 k (none) | ~42 k (none) |
| Disk point-select (WAL) | ~160 k ops/s (n=20k) | ~437 k (none) | ~81 k (none) |
| Disk WAL batch 100/commit FULL | ~22 k rows/s (n=10k) | ~28 k durable | ~12 k durable |

**Interpretation:** minisqlite is a serious SQLite clone. It **lacks a prepare hot path** and **multi-process**. M3 durable batch is fsync-bound (~5k rows/s FULL); Linux batch looks stronger relative to stock SQLite on that host. Micro is **not** control-group-identical to mpedb-bench.

---

## 6. Architecture (brief)

### minisqlite
- 14 crates, ~200k LOC, almost no deps (`elsa` for page cache), **no unsafe** in library code (per README).
- Volcano executor, COW pager, rollback + WAL, format 3 codec.
- Concurrency: **in-process** multi-connection; **not** multi-process safe.

### mpedb
- SHM multi-process, meta double-buffer, freelist fixpoint, intent-ring group commit, durability `none|commit|wal|async`.
- SQL → content-hashed plans; rigid schema; C-API shim; mirror to SQLite/PG; RLS/UDF/FTS5, etc.

---

## 7. What was *not* done this round

- Re-bench of mpedb/SQLite/PostgreSQL (used committed RESULTS + existing M3 multi-run).
- CPython/Django against minisqlite (impossible without C-API) — correctly **N/A**, not a fail.
- Porting minisqlite’s 5605 Rust unit tests onto mpedb/sqlite3/PG (wrong tool; use sqllogictest for shared SQL).
- SQLite TCL suite / TH3 (C-API / proprietary).
- Full sqllogictest on **PostgreSQL** (dialect mismatch).
- Shared mpedb-bench adapter for minisqlite (API too small: no prepare).
- DuckDB / rest of LANDSCAPE.
- `select5.test` (excluded for all engines in this headline).

---

## 8. Recommendation

| Need | Recommendation |
|---|---|
| Byte-compatible SQLite file in pure Rust, single-process | **minisqlite** |
| Drop-in for Python/Django / libsqlite3 | **mpedb** |
| Multiple OS processes writing concurrently | **mpedb** |
| Maximum SQL/surface fidelity to stock SQLite (FK, AUTOINCREMENT, …) | **minisqlite** or **SQLite** |
| Portable logic corpus (sqllogictest) fidelity | **both very high**; minisqlite slightly higher raw % (native DDL) |
| Production throughput (none-class / multi-reader) | **mpedb** (measured) |
| “Just a file” + server (sc=on) | **PostgreSQL** only if you need the server model |

---

## 9. Reproduction

### 9.1 minisqlite unit + own bench

```bash
git clone https://github.com/cursor/minisqlite.git
export CARGO_TARGET_DIR=~/minisqlite-target
cd minisqlite
cargo test --workspace --release   # 5605 pass
cargo bench --bench workloads      # scalability harness (incl. 1M on Linux)
```

### 9.2 Official sqllogictest against minisqlite

```bash
git clone https://github.com/grahn/sqllogictest.git ~/sqllogictest
# external runner used this round: answers as engine `sqlite`, MD5 hash blocks
# (see host path ~/minisqlite-slt — not vendored in mpedb)
find ~/sqllogictest/test -name '*.test' | sort | grep -v '/select5\.test$' > flist-621
# run in chunks; aggregate TOTAL lines → attempted / passed / wrong
```

### 9.3 Artifacts

**M3 (2026-07-21, complete — no leftover processes):**
- `~/mpedb-measure-results/minisqlite-test-m3.log` — unit **5605/0**
- `~/mpedb-measure-results/minisqlite-corpus-m3.log` — 621-file sqllogictest, `DONE 2026-07-21T15:43:24+02:00` (~86 s)
- `~/mpedb-measure-results/minisqlite-bench-m3.log` — cargo bench through 1M
- `~/mpedb-measure-results/minisqlite-micro-m3.log` — micro insert/select/batch

**Linux (same day):**
- `~/mpedb-measure-results/minisqlite-test-linux.log` — unit 5605/0
- `~/mpedb-measure-results/minisqlite-corpus-linux.log` — same residual as M3
- `~/mpedb-measure-results/sqlite-corpus-linux.log` — stock SQLite 3.45.0 same 621-file corpus
- `~/mpedb-measure-results/minisqlite-bench-linux.log`
- `~/mpedb-measure-results/minisqlite-micro-linux.log`

**mpedb RESULTS (reused):**
- `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md`
- `crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md`
