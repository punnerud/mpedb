# minisqlite vs mpedb (inkl. SQLite3 / PostgreSQL)

**Dato:** 2026-07-21  
**minisqlite:** [github.com/cursor/minisqlite](https://github.com/cursor/minisqlite) @ `main` (clone på M3)  
**mpedb:** dette workspace @ `4926536`  
**Maskin (minisqlite-måling):** Apple M3 Pro, 11 cores, macOS 26.6 (Darwin 25.6.0)  
**SQLite / PostgreSQL / mpedb-tall:** gjenbrukt fra `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md` og `RESULTS-linux-amd-epyc-milan-2c.md` (ikke re-kjørt)

---

## 1. Kort konklusjon

| | **minisqlite** | **mpedb** |
|---|---|---|
| Mål | Trofast SQLite-reimplementasjon i Rust | Serverless fil-DB med **bedre concurrency** + rigid schema + moderne features |
| On-disk format | **Full SQLite format 3** (les + skriv ekte `.db` / WAL) | Eget format (+ overlay-lesing av SQLite via attach/mirror) |
| C-API / drop-in | **Nei** | **Ja** (`mpedb-capi` / `libmpedb_sqlite3`) |
| CPython / Django | **Kan ikke interposeres** | CPython **459/467**, Django A **831/831**, queries **493/493** |
| Multi-process writers | **Nei** (in-process only; ingen OS-locks) | **Ja** (SHM, MVCC, multi-process writers) |
| Egen testsuite (M3, release) | **5605 passed, 0 failed** | Stor egen suite + testkit corpus |
| SQLite sqllogictest 7.4M | **Ikke kjørt i-repo** (de sier differential skjer utenfor) | **Kjørt** (zero wrong answers, per README) |
| Hastighet (M3, se §4–5) | Sterk SQL-engine; ingen prepare; batch-disk bak | **Foran** SQLite/PG på primary cells; foran minisqlite i micro |

**Velg minisqlite** hvis du vil ha *«SQLite, men i Rust»* med byte-kompatible filer og ren facade.  
**Velg mpedb** hvis du trenger C-API/Python/Django, multi-process writers, eller strengere schema + mirror/RLS.

---

## 2. Produktflate

| Egenskap | minisqlite | mpedb | SQLite 3.45 | PostgreSQL 16 |
|---|---|---|---|---|
| Språk | Rust | Rust | C | C |
| Public API | `Connection::{open, open_in_memory, execute, query}` | Rust facade + CLI + PyO3 + **C-shim** | C-API + bindings | Client/server |
| Prepared statements | **Nei** (parse per `execute`/`query`) | **Ja** (`prepare` → content-hash plan) | Ja | Ja |
| C-API | **Ingen** | `sqlite3_*` subset (~50 drop-in symbols) | Full | libpq / protocol |
| Filformat | SQLite format 3 bidirectional | Eget `.mpedb` (+ SQLite attach/mirror) | format 3 | egen |
| Multi-process write | Nei (explicit non-goal) | Ja | Single writer + WAL readers | Multi (server) |
| Concurrent readers | WAL snapshots **in-process** | Lock-free multi-process readers | WAL multi-process | Multi |
| Schema | SQLite-permissive | Rigid (fail early) | Permissive | Strong |
| FK / AUTOINCREMENT / fts | Full (inkl. sqlite_sequence) | FK/AUTOINCREMENT honesty-refusals; FTS5 native | Full | Full |

---

## 3. Dekning: tester og corpus

### 3.1 minisqlite (målt på M3 2026-07-21)

```text
cargo test --workspace --release
→ passed=5605  failed=0
#[test]-markører i treet: ~5650 (README: 5650 / ~90 s)
```

- **~110** `conformance_*.rs`-filer: forventede verdier **transkribert fra sqlite.org-docs**, ikke fra egen engine (metodikk for å unngå sirkulær testing).
- **Format/durability:** håndbygde byte-fixtures fra fileformat-spec; hot-journal / torn WAL.
- **Architecture/seams:** `seams.rs` pin’er crate-grenser.
- **Ikke i-repo:**
  - CPython `test_sqlite3` (ingen C-API → umulig via LD_PRELOAD/DYLD)
  - Django
  - Offisiell **sqllogictest**-corpus (7.4M) — README: *«differential testing against real SQLite happens outside this repo»*

### 3.2 mpedb (dokumentert / tidligere M3-måling)

| Suite | Status |
|---|---|
| CPython `test_sqlite3` under shim | **459/467** pass (~98,3 % av stock-passing); residual: progress + non-goals (serialize, AUTOINCREMENT×2, fts4, …) — se [C-API-COMPAT.md](C-API-COMPAT.md) |
| Django frozen A | **831/831** |
| Django `queries` | **493/493** |
| SQLite sqllogictest corpus | **7.4M records**, zero wrong answers (README / mpedb-testkit) |
| Egen engine/SQL/unit | `cargo test --workspace` (kontinuerlig) |

### 3.3 C-API (direkte)

| | minisqlite | mpedb |
|---|---|---|
| `libsqlite3` ABI | ❌ | ✅ `libmpedb_sqlite3.{so,dylib}` |
| Python `sqlite3` module | ❌ | ✅ interpose |
| Django ORM | ❌ | ✅ (målt A + queries) |
| Result codes / prepare / bind | N/A (Rust-only) | ✅ for drop-in subset |

---

## 4. Hastighet — gjenbrukte mpedb-bench RESULTS (SQLite + PG + mpedb)

**Ikke re-kjørt** for SQLite/PostgreSQL/mpedb; tall fra committed RESULTS (2026-07-21).

### 4.1 Apple M3 Pro (`RESULTS-macos-apple-m3-pro-11c.md`)

**none-class** (tmpfs/ramdisk, ingen fsync-garanti):

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

## 5. Hastighet — minisqlite målt på M3 (ny micro + egen bench)

### 5.1 Egen `cargo bench` (workloads, sizes 1k / 10k / 100k)

Kilde: `~/mpedb-measure-results/minisqlite-bench.log`

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

Dette er **scalability / plan-shape**, ikke samme celler som mpedb-bench (ingen felles harness).

### 5.2 Microbench (samme logiske schema som mpedb-bench users-tabell)

Kilde: `~/mpedb-measure-results/minisqlite-micro.log`  
API: string-SQL per kall (ingen prepare) — **ufavorabelt vs mpedb/SQLite prepared path**.

| Cell | minisqlite (M3) | mpedb (RESULTS M3) | SQLite (RESULTS M3) | Note |
|---|---:|---:|---:|---|
| In-memory point-insert | ~176 k ops/s | ~233 k (none tmpfs) | ~117 k | Ulike medier/API; retning: mpedb ≥ mini ≥ sqlite på insert |
| Disk point-select | ~200 k ops/s | ~1.26 M (none) | ~326 k | mpedb langt foran (hash-plan + SHM) |
| Disk WAL batch 100/commit | ~6.8 k rows/s | ~28 k durable | ~27 k durable | mini **mye** bak; re-parse + ingen bind; fullfsync-semantikk ikke verifisert som mpedb/SQLite FULL |

**Tolking:** minisqlite er en seriøs SQLite-klone, men **mangler prepare-hot-path** og **multi-process**. For app-lignende insert/select vinner mpedb og stock SQLite klart på batch-disk. Micro er **ikke** kontrollgruppe-identisk med mpedb-bench.

---

## 6. Arkitektur (kort)

### minisqlite
- 14 crates, ~200k LOC, nesten ingen deps (`elsa` for page cache), **no unsafe** i library code (per README).
- Volcano executor, COW pager, rollback + WAL, format 3 codec.
- Concurrency: **in-process** multi-connection; **ikke** multi-process safe.

### mpedb
- SHM multi-process, meta double-buffer, freelist fixpoint, intent-ring group commit, durability `none|commit|wal|async`.
- SQL → content-hashed plans; rigid schema; C-API shim; mirror til SQLite/PG; RLS/UDF/FTS5 m.m.

---

## 7. Hva som *ikke* ble gjort i denne runden

- Re-bench av mpedb/SQLite/PostgreSQL (brukte committed RESULTS + eksisterende M3 multi-run).
- CPython/Django mot minisqlite (umulig uten C-API).
- Offisiell sqllogictest-corpus mot minisqlite (ikke i deres CI; ville kreve ekstern harness).
- Felles mpedb-bench-adapter for minisqlite (API for liten: ingen prepare).
- DuckDB / øvrig LANDSCAPE.

---

## 8. Anbefaling

| Behov | Anbefaling |
|---|---|
| Byte-kompatibel SQLite-fil i ren Rust, single-process | **minisqlite** |
| Drop-in for Python/Django / libsqlite3 | **mpedb** |
| Flere OS-prosesser som skriver samtidig | **mpedb** |
| Maksimal SQL/surface-troskap til stock SQLite (FK, AUTOINCREMENT, …) | **minisqlite** eller **SQLite** |
| Produksjons-throughput (none-class / multi-reader) | **mpedb** (målt) |
| «Bare en fil» + server (sc=on) | **PostgreSQL** bare hvis du trenger server-modellen |

---

## 9. Reproduksjon (minisqlite på M3)

```bash
git clone https://github.com/cursor/minisqlite.git
export CARGO_TARGET_DIR=~/minisqlite-target
cd minisqlite
cargo test --workspace --release   # ~5605 pass
cargo bench --bench workloads      # scalability harness
# micro (se /tmp/ms-micro på measure-host)
```

**Artefakter på M3 measure-host:**
- `~/mpedb-measure-results/minisqlite-test.log`
- `~/mpedb-measure-results/minisqlite-micro.log`
- `~/mpedb-measure-results/minisqlite-bench.log`

**mpedb RESULTS (gjenbrukt):**
- `crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md`
- `crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md`
