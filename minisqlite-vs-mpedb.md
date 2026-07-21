# minisqlite vs mpedb (incl. SQLite3 / PostgreSQL)

**Date:** 2026-07-21  
**minisqlite:** [github.com/cursor/minisqlite](https://github.com/cursor/minisqlite) @ `main`  
**mpedb:** this workspace @ `4926536`  
**Machines:**
- **M3 (earlier):** Apple M3 Pro, 11 cores, macOS 26.6 — prior minisqlite unit/micro notes
- **Linux (this round):** AMD EPYC-Milan 2 cores, Linux 6.8 — unit tests, **official sqllogictest**, cargo bench, micro  

**SQLite / PostgreSQL / mpedb mpedb-bench numbers:** reused from `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md` and `RESULTS-linux-amd-epyc-milan-2c.md` (not re-run)

---

## 1. Short conclusion

| | **minisqlite** | **mpedb** |
|---|---|---|
| Goal | Faithful SQLite reimplementation in Rust | Serverless file DB with **better concurrency** + rigid schema + modern features |
| On-disk format | **Full SQLite format 3** (read + write real `.db` / WAL) | Own format (+ overlay read of SQLite via attach/mirror) |
| C-API / drop-in | **No** | **Yes** (`mpedb-capi` / `libmpedb_sqlite3`) |
| CPython / Django | **Cannot be interposed** | CPython **459/467**, Django A **831/831**, queries **493/493** |
| Multi-process writers | **No** (in-process only; no OS locks) | **Yes** (SHM, MVCC, multi-process writers) |
| Own test suite (release) | **5605 passed, 0 failed** (M3 + Linux) | Large own suite + testkit corpus |
| SQLite **sqllogictest** 7.4M | **Run on Linux (this round):** 99.9999% of attempted, **4 wrong / 2 errmis / 1 engine error** | **Run** (mpedb-testkit): 99.9765% attempted, **0 genuine wrong answers** after shim accounting — see [CORPUS-STATUS.md](design/CORPUS-STATUS.md) |
| Speed | Strong SQL engine; no prepare; own bench scales to 1M rows | **Ahead** of SQLite/PG on primary mpedb-bench cells; ahead of minisqlite on prepared/none-class select |

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

## 3. Coverage: tests and corpus

### 3.1 minisqlite unit suite

| Host | Command | Result |
|---|---|---|
| M3 (earlier) | `cargo test --workspace --release` | **5605 passed, 0 failed** |
| Linux EPYC 2c (this round) | same | **5605 passed, 0 failed** |

```text
cargo test --workspace --release
→ passed=5605  failed=0
#[test] markers in tree: ~5650 (README: 5650 / ~90 s)
```

- **~110** `conformance_*.rs` files: expected values **transcribed from sqlite.org docs**, not from the engine itself (methodology to avoid circular testing).
- **Format/durability:** hand-built byte fixtures from the file-format spec; hot-journal / torn WAL.
- **Architecture/seams:** `seams.rs` pins crate boundaries.
- **Not possible without a C-API:**
  - CPython `test_sqlite3` (no LD_PRELOAD/DYLD interpose)
  - Django
  - SQLite **TCL** suite (API-bound to `sqlite3` C interface)

### 3.2 Official SQLite **sqllogictest** corpus against minisqlite (this round)

minisqlite’s README notes that differential testing against real SQLite is *outside* their repo. We ran the public corpus ourselves.

| Field | Value |
|---|---|
| Corpus | [grahn/sqllogictest](https://github.com/grahn/sqllogictest) `test/**/*.test` |
| Files | **621 / 622** (`select5.test` excluded — same exclusion as mpedb [CORPUS-STATUS.md](design/CORPUS-STATUS.md)) |
| Runner | external `minisqlite_corpus` (answers as engine `sqlite` in `skipif`/`onlyif`; MD5 hash blocks; canonical I/R/T rendering) |
| Host | Linux AMD EPYC-Milan 2c, 2026-07-21 |
| Wall clock | **~187 s** end-to-end |

**Headline:**

| metric | minisqlite (Linux, this round) | mpedb (CORPUS-STATUS @ `b41b713`, M3) |
|---|---:|---:|
| records seen | **7 419 277** | 7 419 202 |
| skipped (`onlyif` mysql/mssql, etc.) | **1 480 834** | 1 480 924 |
| **attempted** | **5 938 443** | 5 938 278 |
| **passed** | **5 938 436 — 99.999882 %** | 5 936 882 — 99.9765 % |
| ├ statements | 210 080 | 208 748 |
| └ queries | 5 728 356 | 5 728 134 |
| queries md5-verified | **955 236** | 955 237 |
| **wrong answers** | **4** | 0 genuine (4 flagged cascade / shim) |
| **error mismatches** | **2** | **0** |
| engine errors (expected ok) | **1** | counted under refused/unsupported (shim) |
| refused / unsupported | n/a (native CREATE TABLE) | 1 392 (mostly runner artifacts) |

**minisqlite residual (all 7 failures):**

| file | kind | notes |
|---|---|---|
| `evidence/slt_lang_aggfunc.test` | 3 wrong + 1 engine error | `sum`/`total` on extreme i64 values: float `%.3f` rounding vs SQLite text (`-9223372036854775808` vs `…6000`); one `sum(x)` raises *integer overflow* where the corpus expects a Real |
| `random/…/slt_good_121.test` | 1 wrong | `SELECT DISTINCT *` cross join — 81 values, MD5 mismatch (ordering/distinct surface) |
| errmis ×2 | expected-error vs success | same aggregate edge area (sampled with wrongs) |

**Interpretation:** minisqlite is extremely close to stock SQLite on the portable logic corpus — better raw pass% than mpedb because it has native `CREATE TABLE`/permissive types and needs **no** mpedb schema shim. mpedb’s published corpus story is different: **zero genuine wrong answers** after categorizing shim artifacts, with a small set of deliberate engine gaps (see CORPUS-STATUS). Neither number is a substitute for the other.

TH3 (proprietary) and the TCL suite were **not** run (no license / no C-API).

### 3.3 mpedb (documented / prior measurement)

| Suite | Status |
|---|---|
| CPython `test_sqlite3` under shim | **459/467** pass (~98.3% of stock-passing); residual: progress + non-goals (serialize, AUTOINCREMENT×2, fts4, …) — see [C-API-COMPAT.md](C-API-COMPAT.md) |
| Django frozen A | **831/831** |
| Django `queries` | **493/493** |
| SQLite sqllogictest corpus | **7.4M records**, zero wrong answers after shim accounting ([CORPUS-STATUS.md](design/CORPUS-STATUS.md)) |
| Own engine/SQL/unit | `cargo test --workspace` (continuous) |

### 3.4 C-API (direct)

| | minisqlite | mpedb |
|---|---|---|
| `libsqlite3` ABI | ❌ | ✅ `libmpedb_sqlite3.{so,dylib}` |
| Python `sqlite3` module | ❌ | ✅ interpose |
| Django ORM | ❌ | ✅ (measured A + queries) |
| Result codes / prepare / bind | N/A (Rust-only) | ✅ for drop-in subset |

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

### 5.1 Own `cargo bench` — Linux EPYC 2c (this round)

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

### 5.2 Own `cargo bench` — M3 (earlier, sizes through 100k)

Source: `~/mpedb-measure-results/minisqlite-bench.log`

```text
scalability (wall-clock ms / peak heap KiB), sizes [1000, 10000, 100000]
workload                              1000             10000            100000
point_lookup_indexed            0.6ms/…          0.0ms/…           0.1ms/…
range_scan                      0.1ms            0.1ms             0.8ms
equi_join                       0.4ms            3.0ms            29.9ms
group_by                        0.2ms            1.6ms            15.9ms
correlated_subquery             1.9ms           19.2ms           189.5ms
durability round-trip: ok
```

This is **scalability / plan-shape**, not the same cells as mpedb-bench (no shared harness).

### 5.3 Microbench (same logical schema as mpedb-bench `users` table)

API: string-SQL per call (no prepare) — **unfavorable vs mpedb/SQLite prepared path**.

**Linux EPYC 2c (this round)** — `~/mpedb-measure-results/minisqlite-micro-linux.log`:

| Cell | minisqlite (Linux) | mpedb (RESULTS Linux) | SQLite (RESULTS Linux) | Note |
|---|---:|---:|---:|---|
| In-memory point-insert | ~83 k ops/s (n=50k) | ~187 k (none) | ~42 k (none) | String-SQL; tree grows with n |
| Disk point-select (WAL) | ~160 k ops/s (n=20k) | ~437 k (none) | ~81 k (none) | mpedb ahead (hash-plan + SHM) |
| Disk WAL batch 100/commit FULL | ~22 k rows/s (n=10k) | ~28 k durable | ~12 k durable | mini competitive on this host; still re-parse each INSERT |

**M3 (earlier)** — `~/mpedb-measure-results/minisqlite-micro.log`:

| Cell | minisqlite (M3) | mpedb (RESULTS M3) | SQLite (RESULTS M3) |
|---|---:|---:|---:|
| In-memory point-insert | ~176 k ops/s | ~233 k (none tmpfs) | ~117 k |
| Disk point-select | ~200 k ops/s | ~1.26 M (none) | ~326 k |
| Disk WAL batch 100/commit | ~6.8 k rows/s | ~28 k durable | ~27 k durable |

**Interpretation:** minisqlite is a serious SQLite clone. It **lacks a prepare hot path** and **multi-process**. On Linux batch-disk FULL it lands between stock SQLite and mpedb in this micro; on M3 durable batch it lagged both. Micro is **not** control-group-identical to mpedb-bench.

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
- CPython/Django against minisqlite (impossible without C-API).
- SQLite TCL suite / TH3 (C-API / proprietary).
- Shared mpedb-bench adapter for minisqlite (API too small: no prepare).
- DuckDB / rest of LANDSCAPE.
- `select5.test` (excluded for both engines; very large / historically out of headline).

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

**Linux measure-host (this round):**
- `~/mpedb-measure-results/minisqlite-test-linux.log` — unit suite (5605/0)
- `~/mpedb-measure-results/minisqlite-corpus-linux.log` — full 621-file sqllogictest
- `~/mpedb-measure-results/minisqlite-bench-linux.log` — cargo bench through 1M
- `~/mpedb-measure-results/minisqlite-micro-linux.log` — micro insert/select/batch

**M3 measure-host (earlier):**
- `~/mpedb-measure-results/minisqlite-test.log`
- `~/mpedb-measure-results/minisqlite-micro.log`
- `~/mpedb-measure-results/minisqlite-bench.log`

**mpedb RESULTS (reused):**
- `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md`
- `crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md`
