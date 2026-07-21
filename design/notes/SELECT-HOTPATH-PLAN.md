# SELECT/INSERT hot-path plan (SQLite-inspired)

Goal: close the gap to stock SQLite on `prepare+bind` point SELECT and
autocommit INSERT on `:memory:`.

Shipped in `7fb0d53` (2026-07-21).

## Sequence (measured after each on Linux)

| # | Change | Why (from sqlite3.c) |
|---|--------|----------------------|
| **P1** | Cache SELECT output column names (`Arc<[String]>`) in `PkPointHot` | `OP_ResultRow` never rebuilds names |
| **P2** | TLS last-plan hit: same `PlanHash` → skip `RwLock` | stmt keeps VDBE program in hand |
| **P3** | `PreparedSelect` handle | `sqlite3_stmt` state machine |
| **P5** | Private `:memory:` in-place leaf mutate when no pins (+ undo, nested-read TLS) | in-place B-tree, no COW freelist free |

Harness: external `imem_bench 50000 50000` (mpedb + minisqlite + rusqlite bundled).

## Final dual-host results (`7fb0d53`, n=50k)

| Cell | Linux mpedb | Linux sqlite3 | M3 mpedb | M3 sqlite3 |
|------|------------:|--------------:|---------:|-----------:|
| prepare+bind SELECT (`execute(hash)`) | 1.34 M | 2.51 M | 1.94 M | 3.77 M |
| **PreparedSelect** | **1.78 M** | = | **2.55 M** | = |
| prepare+bind INSERT | **460 k** | 1.16 M | **564 k** | 1.51 M |
| batch-100 prepare INSERT | **891 k** | 634 k string | **1.57 M** | 1.24 M string |

mpedb/sqlite ratio (PreparedSelect): **0.71× Linux · 0.68× M3**.  
mpedb/sqlite ratio (autocommit INSERT): **0.40× Linux · 0.37× M3**.  
**batch-100 wins** string-SQL batch on both hosts.

### Linux step ladder (same day)

| Step | SELECT bind | PreparedSelect | INSERT bind |
|------|------------:|---------------:|------------:|
| Pre-work baseline | 463 k | — | 269 k |
| pin + PkPoint + stack key | ~1.45 M | — | ~274 k |
| + TLS + PreparedSelect | 1.39 M | 1.77 M | ~278 k |
| + in-place write (final) | 1.34–1.40 M | 1.73–1.78 M | **460–474 k** |

### Takeaways

- **PreparedSelect** is the read hot-path API (~0.7× sqlite3, not 0.2×).
- **In-place private writes** ~1.7× autocommit INSERT; still ~0.4× sqlite.
- **Batch-100** amortizes commit and beats sqlite string batch.
- Residual: owned `Value`/`String` + pin/txn per execute vs VDBE registers; freelist/catalog on write.

Logs: `~/mpedb-measure-results/imem-bench-linux-post-push.log`,
`imem-bench-m3-7fb0d53.log`.
