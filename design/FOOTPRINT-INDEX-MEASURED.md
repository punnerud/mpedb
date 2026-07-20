# FOOTPRINT-INDEX-MEASURED — task #117, the four candidate sites, measured

**Status: measurement, 2026-07-20. Four verdicts, three of them "don't build". No index
was built.** This is the results half of the harness committed in `2c21c26`
(`Database::plan_footprint`, `sqlite_corpus --footprint-census`, `mpedb --example
footprint_index`). It is the empirical input to [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md)
(#88) — read §1 before designing the cost catalog's key.

## 0. What was run

- **Census**: `sqlite_corpus --footprint-census=<tsv>` over 14 real sqllogictest files
  (`index/{between,commute,delete,in,orderby,orderby_nosort,random,view}`,
  `random/{aggregates,expr,groupby,select}`, `evidence/slt_lang_update`, `select1`) —
  101,343 records, 95,184 passing, 0 wrong. Every statement is recompiled through
  `Database::plan_footprint`, i.e. the same compile path `prepare`/`execute` take, so the
  footprint counted is the one the executed statement carries. **Run twice; every count and
  byte total below reproduced exactly.** Only the cost-spread block (§1, wall-clock based)
  moved between runs, and both runs are reported.
- **Microbench**: `mpedb --example footprint_index <census.tsv>`, replaying the 119
  distinct REAL footprints. **Run three times; every timing below is the elementwise
  minimum of the three.** The box carried two concurrent `cargo test` jobs on 2 cores;
  runs 2 and 3 were inflated 2–35× but reproduced every ordering, every crossover and
  every sign. Section 5 (byte counts) is deterministic and identical in all three.
- Nothing here changed the engine, the commit path or the plan format.

## 1. The number #88 needs: plans vs footprints vs table sets

| key | distinct | plans per key | distribution |
|---|---|---|---|
| plan hash | **81,036** | 1.00 | 94,689 compiled statements (2 uncompilable) |
| footprint | **119** | **680.97** | p50 7, p90 1,001, p99 8,429, max 24,072 |
| table set (reorder-invariant) | **22** | **3,683.45** | p50 206, p90 11,463, max 24,072 |

- The footprint refines the table set by **5.41×**; the table set is `tables_read ‖
  tables_written ‖ read_only` only — no `indexes_used`, no `key_access`, because those two
  come from the chosen `AccessPath` and move under a join reorder while the table sets do not.
- 35 of 119 footprints (29.4 %) hold exactly one plan. Occurrence-weighted mean
  plans/footprint is **10,943.6** — the shapes that actually recur are the crowded ones.
- Statement width `|tables_read| + |tables_written|`: **0 = 10,000, 1 = 82,004, 2 = 2,331,
  3 = 354**. The real statement touches one table. Everything below that assumes a fan
  above 3 is extrapolation.

**Reading it.** The ratio is nowhere near 1:1 — a shape key pools hundreds to thousands of
plans. But pooling only helps if what is pooled is stable, and the cost spread says it is not:

| spread (median) | quiet run | contended run | over |
|---|---|---|---|
| within-plan CV (irreducible: same plan, different data + timer) | **0.023** | 0.091 | 5,435 plans |
| across-plan-within-footprint CV (the error a shape key adds) | **1.191** | 1.823 | 84 footprints |
| ratio across / within | **52×** | 20× | — |
| worst/best plan mean cost inside one footprint bucket | **217.3×** (p90 2,657×, max 5,484×) | 678.3× (p90 26,338×, max 46,980×) | 84 footprints |

The shape key's error is **20–52× the irreducible floor** depending on how much machine
noise is in the floor, and a measurement borrowed from a bucket peer is wrong by a median
factor of **217× (quiet) to 678× (contended)**. The conclusion does not depend on which run
you believe.

> **Verdict for #88: key the cost catalog on the PLAN HASH.** The footprint is a legitimate
> coarse *index* over plans ("which plans touch table T", "invalidate everything that reads
> T after `CREATE INDEX`") and it is free at plan time, so keep computing it — but it is not
> a cost-sharing key, and the table set (22 keys for 81k plans) is far worse. If #88 wants a
> key coarser than the hash so a new plan inherits a prior, the candidate must be finer than
> the footprint: plan hash first, and only then a shape key that includes the access paths.

*Caveat.* Corpus SQL is literal-heavy and unparameterized: 1.17 statements per distinct
plan. A real application binding parameters would collapse many of those 81,036 plans and
push the plans-per-footprint ratio higher still. It does not touch the 217× number, which is
a comparison *inside* a bucket.

## 2. Conflict detection: inverted index vs pairwise — **DON'T BUILD**

Two facts settle this before the numbers:

1. `Footprint::conflicts_with` has **zero callers in the engine** — only its own unit tests
   (`crates/mpedb-types/src/footprint.rs`). The pairwise cost the proposal wants to remove
   is not being paid.
2. What actually runs is `shm::opt_conflict` (DESIGN-PHASE3 §3.1): an O(window) scan of a
   **64-slot** committed-footprint ring, `table_bits` a u64 bitmap + key hash. It indexes
   over *time*, not over tables, and the window is hard-capped at 63 (`OPT_RING_SLOTS = 64`;
   anything older is reported as a conflict, conservatively).

Measured, today's mechanism, ns per commit: **window 1 = 1.4, 4 = 1.7, 16 = 6.1, 63 = 52.7**
— i.e. **0.1 % … 3.8 %** of the 1.4 µs commit critical section (DESIGN-PHASE3 §2), at the
largest window the ring can even represent.

Crossovers anyway, ns/commit, min-of-3, index build charged at steady state (clear + refill,
amortized over N), fan = 1 table/statement (the corpus-real shape):

| tables in schema | N | pairwise | inv(hash) | inv(vec) | pairwise / best |
|---|---|---|---|---|---|
| 8 | 2 | 19.8 | 63.7 | 22.8 | 0.87× (index loses) |
| 8 | 8 | 31.1 | 43.3 | 7.3 | 4.26× |
| 8 | 32 | 136.0 | 37.9 | 6.1 | 22.3× |
| 8 | 2048 | 9,301.8 | 35.8 | 6.5 | 1,431× |
| 4096 (`MAX_TABLES`) | 2 | 5.5 | 42.5 | 3,246.3 | 0.13× |
| 4096 | 8 | 27.3 | 49.3 | 720.4 | 0.55× (index loses) |
| 4096 | 32 | 139.1 | 40.0 | 180.8 | 3.48× |
| 4096 | 512 | 2,870.5 | 41.3 | 20.0 | 143.5× |

- **Crossover in writers**: N ≈ 3–4 with 8 tables in the schema, **N ≈ 16–20 at the real
  `MAX_TABLES` = 4096**. At fan 3 it moves to N ≈ 2 (8 tables) / N ≈ 8–10 (4096 tables); at
  fan 5, N ≈ 8–10 (4096 tables) — wider statements favour the index, as expected.
- **Crossover in table count**: the flat-`Vec` realization pays O(`MAX_TABLES`) per window
  rebuild (8,192 `Vec::clear`s), so at 4096 tables it only beats the `HashMap` form from
  N ≈ 512. Below that the hash form is the only viable index, and its floor is ~30–180
  ns/commit — *above* the ring's worst case.

> **Verdict: DON'T BUILD.** The crossover is real but unreachable. There is no in-flight
> peer set to intersect against: commits serialize on the writer lock and validate against
> committed history, not against peers, so the engine's only "N" is the ring window ≤ 63 —
> where the existing scan costs 52.7 ns, roughly an order of magnitude *less* than the
> cheapest index arm's flat floor once its rebuild is charged. An inverted `table ->
> statements` index would replace a mechanism that is already ~1 % of the critical section
> with one that is more expensive at every reachable N.

*Honest bias, in the index's favour.* The pairwise arm is measured without early exit
(`c |= …` over all peers) while both index arms return on first hit. A short-circuiting
pairwise would be cheaper and push every crossover further right — which only strengthens
the verdict.

## 3. Plan-variant families (the MPEE ping-pong angle) — **DON'T BUILD YET, break-even V = 16**

DESIGN-MPEE-SOLVER §9.6's execution-time ping-pong would persist a better plan as a *new*
content hash, so one SQL statement grows a family of V variants. "List the variants of X" is
then either V probes of a 32-byte hash (what the registry is) or one prefix range over
`(base hash ‖ decision vector)`.

ns per family listing, min-of-3:

| V | V probes | one prefix range | speedup |
|---|---|---|---|
| **1 (measured today)** | 19.5 | 120.1 | **0.16×** |
| 2 | 36.6 | 140.6 | 0.26× |
| 4 | 68.7 | 170.1 | 0.40× |
| 8 | 166.4 | 210.8 | 0.79× |
| **16** | 274.3 | 252.2 | **1.09× ← break-even** |
| 64 | 1,355.3 | 780.9 | 1.74× |

- **Measured V today = 1** — ping-pong is designed, not built, and the registry stores one
  plan per statement. At V = 1 the prefix form is **6× slower**.
- **Break-even V = 16** in memory; it is a clean win (1.7×) only at V = 64.

> **Verdict: DON'T BUILD NOW.** Revisit when ping-pong ships *and* the median statement
> carries ≥ 8 persisted variants. The trigger is cheap to check (the registry knows V).

*Not measured, and it moves the threshold down.* This is an in-memory `HashMap` vs
`BTreeMap` model. The real registry lives in the catalog sys-keyspace, where a probe is a
B+tree descent per key while a prefix range is one descent plus sequential leaf reads — so
the real break-even is lower than 16, plausibly single-digit. Measuring that needs a
sys-keyspace variant layout, i.e. building the thing; it is deliberately left unmeasured.

## 4. Routing (footprint → shard), computed vs memoized — **DON'T BUILD**

| | ns/stmt |
|---|---|
| footprint-computed route | **1.78** |
| memoized (plan hash → shard) | **12.73** |

Memoizing the decision is **7× slower than recomputing it**: the route is a `first()` on a
sparse `TableSet` plus an FNV over a few `KeyPart`s, which is cheaper than one `HashMap`
probe of a 32-byte hash. Even if the memo were free, the +10.95 ns/stmt it costs is
**0.25 %** of a 4.4 µs serial transaction.

> **Verdict: DON'T BUILD.** There is no cache worth having for a computation cheaper than
> its own cache lookup.

## 5. TableSet delta ("polyline", #115) compression — **DON'T BUILD at today's shapes**

Deterministic; identical in all three runs. 94,689 footprint instances, 119 distinct shapes:

| | current | delta | |
|---|---|---|---|
| table sets only | 729,668 B | 277,106 B | **−62.0 %** |
| whole footprint | 1,676,558 B | 1,223,996 B | **−27.0 %** |
| mean bytes / stored footprint | 17.71 | 12.93 | −4.78 B |

At #88's would-be scale: 1M stored footprints saves **4.6 MiB**; 10M saves **45.6 MiB**;
100M saves **456 MiB**.

- **Threshold**: the saving is 4.78 bytes per stored footprint. If #88 persists **per-plan
  aggregates** — 81,036 rows in this corpus — the entire question is ~380 KiB, of which the
  delta form saves ~100 KiB: nothing. It only becomes a number past ~**10 M** stored
  footprints, i.e. only if #88 persists a *per-execution* history.
- **Where it would pay**, and does not exist here: wide/dense sets — `|set|` 8 dense
  −73.5 %, 64 dense −74.8 %, 64 sparse −71.3 %, 512 dense −74.9 %. Those need statements
  touching dozens of tables; the corpus maximum is **3**.

> **Verdict: DON'T BUILD for footprints.** A footprint is a handful of u32s, exactly as
> expected. If #115 lands for its own reasons, `TableSet` gets the encoding for free — but
> it does not justify #115, and it does not justify a footprint store.

## 6. What was not measured

- The **sys-keyspace (B+tree) form of §3** — the regime that would lower the V break-even.
  Measuring it means building the variant key layout.
- **V > 1 in reality**: MPEE ping-pong is not built, so V = 1 is the only observed value.
- **Real join fans above 3**: the corpus tops out at 3 tables per statement, so the fan-5
  rows in §2 are synthetic extrapolation, not observed workload.
- **Multi-process concurrent writers**: census and microbench are single-process. §2's
  reachability argument rests on the ring's 64-slot cap and the writer lock, not on a
  measured multi-process write window.
- Absolute timings on an **idle** box: all three runs shared 2 cores with two `cargo test`
  jobs. The min-of-3 is a lower bound on each cell; ratios and crossovers were stable across
  runs, absolute ns are not tight.
