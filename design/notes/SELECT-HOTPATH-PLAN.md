# SELECT/INSERT hot-path plan (SQLite-inspired)

Goal: close the gap to stock SQLite on `prepare+bind` point SELECT (~0.4 µs)
and autocommit INSERT (~0.9 µs) on `:memory:`.

Baseline (Linux, after private pin + PkPoint micro-exec + stack key):  
~1.50 M SELECT ops/s (0.7 µs) · ~270 k INSERT · batch-100 wins.

## Sequence (measure after each)

| # | Change | Why (from sqlite3.c) | Risk |
|---|--------|----------------------|------|
| **P1** | Cache SELECT output column names (`Arc<[String]>`) once per plan | `OP_ResultRow` never rebuilds names | Low |
| **P2** | TLS last-plan hit: same `PlanHash` → skip `RwLock` | stmt keeps VDBE program in hand | Low |
| **P3** | `PreparedSelect` handle: hold `Arc<plan>` + col idxs + names; `query(params)` | `sqlite3_stmt` state machine | Med |
| **P4** | (if needed) tighten Int-PK only path further | `SeekRowid` intkey | Low |
| **P5** | Private `:memory:` in-place leaf mutate when no pins | in-place B-tree, no COW freelist | Med–High |

Harness: `/tmp/imem-bench` `imem_bench 50000 50000`, log `opt-seq-NN-*.log`.
After P3, extend harness with a `PreparedSelect` cell.

Do **not** reorder multiproc pin fences on file DBs; private-only shortcuts only.

## Measured results (Linux x86_64, n=50k)

| Step | prepare+bind SELECT | PreparedSelect | prepare+bind INSERT | batch-100 | vs sqlite SELECT |
|------|--------------------:|---------------:|--------------------:|----------:|-----------------:|
| **00 baseline** (pin+PkPoint+stack key) | 1.45–1.50 M | — | ~274 k | ~900 k | ~0.60× |
| **01** TLS plan + PreparedSelect | 1.39 M | **1.77 M** | ~278 k | ~892 k | **0.75×** (PS) |
| **02** + private in-place write | 1.40 M | **1.73 M** | **474 k** | ~893 k | 0.72× (PS) |
| sqlite3 same harness | **2.40 M** | =bind | **1.09 M** | ~635 k string | 1.00× |

### Takeaways

- **PreparedSelect** is the right API analogue of `sqlite3_stmt` for reads (~1.75 M vs 2.4 M).
- **In-place private writes** nearly doubled autocommit INSERT (274k → 474k); still ~0.44× sqlite (undo-buffer + freelist/catalog remain).
- **Batch-100** still **wins** vs sqlite string batch.
- Residual SELECT gap: owned `Value`/`String` rows + pin/txn per execute vs VDBE registers.
