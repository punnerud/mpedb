# OLAP: mpedb vs DuckDB vs SQLite

Analytics is not what mpedb is for. It is an embedded row store built for
multi-process OLTP, and DuckDB is a vectorised column store built for exactly
this workload. **The point of running it anyway is that the losses are not
uniform, and the shape of the unevenness is the result.**

Harness: [`crates/mpedb-olapbench`](crates/mpedb-olapbench). Machine: Apple M3
Pro, 11 cores, 36 GiB, macOS 26.6. All three engines in-process, DuckDB and
SQLite in memory, mpedb on APFS with `durability = "none"`. Measured 2026-07-22.

    cargo run --release --manifest-path crates/mpedb-olapbench/Cargo.toml -- \
      --facts 2000000 --reps 5

Every engine runs **the same SQL text**, and the harness compares canonically
rendered results across all three before it believes any timing. Every row
below is marked `agree: yes`; a disagreement would have struck the row out.
A fast wrong answer is a bug report, not a benchmark result.

## The dataset

A star: `fact` (2,000,000 rows) joined to `customer` (20,000), `product`
(5,000), `store` (200) and `day` (1,461). Deterministic xorshift generator,
identical rows in identical order for every engine. Customer ids are drawn from
a squared distribution so the group-by has a heavy head, which is what real
data does and what a uniform generator hides.

**The index asymmetry is deliberate.** mpedb and SQLite get an index on every
join key and every filtered dimension column, plus `amount`. DuckDB gets none,
because its own documentation tells you not to build ART indexes for analytics —
benchmarking it in a configuration its authors advise against would be
manufacturing a loss. mpedb pays for those trees at load time, and that price
is in the load table rather than hidden.

| engine | load | note |
|---|---:|---|
| mpedb | 9.6 s | five index trees on `fact`, maintained row by row, no bulk-build path |
| duckdb | 0.5 s | Appender, no indexes |
| sqlite | 3.4 s | same indexes as mpedb, built after the rows |

## Results

Milliseconds, median of 5 plus an untimed warm-up.

| query | probes | mpedb | duckdb | sqlite | mpedb vs duckdb | mpedb vs sqlite |
|---|---|---:|---:|---:|---:|---:|
| `scan-sum` | scan | 41.2 | 0.479 | 34.8 | 86× slower | 1.2× slower |
| `scan-filter-sum` | scan | **126.0** | 0.744 | 60.9 | 169× slower | 2.1× slower |
| `scan-range-sum` | scan | **184.2** | 0.660 | 250.1 | 279× slower | **1.4× faster** |
| `scan-multi-agg` | scan | 173.4 | 0.843 | 93.3 | 206× slower | 1.9× slower |
| `count-star` | precompute | ~~3.0~~ **1.5** | 0.215 | 0.141 | 7.2× slower | 10.6× slower |
| `min-max-indexed` | precompute | **0.008** | 1.9 | 59.9 | **252× faster** | **7,488× faster** |
| `count-filtered` | precompute | 0.151 | 0.367 | 0.007 | **2.4× faster** | 22× slower |
| `group-small` | group by | 251.1 | 1.2 | 685.9 | 212× slower | **2.7× faster** |
| `group-large` | group by | 365.5 | 7.9 | 911.9 | 46× slower | **2.5× faster** |
| `join-star-2` | join order | ~~1198.0~~ **194.3** | 1.9 | 110.4 | 104× slower | 1.8× slower |
| `join-star-4` | join order | ~~1325.0~~ **319.9** | 3.8 | 172.8 | 85× slower | 1.9× slower |
| `join-bad-order` | join order | ~~1098.5~~ **185.9** | 2.7 | 89.6 | 69× slower | 2.1× slower |

Struck-through numbers are the original 2026-07-22 run; the table is the
2026-07-23 re-measure after **stage A** (per-index NDV statistics —
`Database::analyze()`, 0.17 s for nine indexes) and **stage B** (the schema
declares NOT NULL, below). Same binary lineage, same machine, same data.

Prepared and parameterised, 20,000 executions, total milliseconds:

| query | mpedb | duckdb | sqlite |
|---|---:|---:|---:|
| `prepared-point` — `WHERE id = ?` | 13 | 2,729 | 4 |
| `prepared-range` — `sum(amount) WHERE customer_id = ?` | 1,431 | 6,093 | 927 |

## Reading it

**The DuckDB column is the boring one.** A vectorised column store beats a row
store on scan-and-aggregate by two orders of magnitude, and it does. Nobody
should be surprised, and nothing here is an argument that they should not be.
Its 2,729 ms on 20,000 point lookups is the mirror image and equally
unsurprising: an OLAP engine paying per-query setup 20,000 times.

**The SQLite column is the one that matters.** Same architecture, same indexes,
same machine. Where mpedb loses to SQLite, the loss is mpedb's, not the row
store's — and there are two of those.

### Win: the extremum, by 252× over DuckDB and 7,675× over SQLite

`SELECT min(amount), max(amount)` is two O(log n) boundary probes in mpedb
(PLAN_FORMAT 59/60) — descend the `amount` tree to each end and read the value
back from the row bit-exactly. Eight microseconds on two million rows.

SQLite has a min/max index optimisation too, and does not use it here: it
applies only to a query with a *single* aggregate, so asking for both in one
statement falls back to a full scan. DuckDB's zone maps let it skip most row
groups, which is why it lands at 1.9 ms rather than SQLite's 61 — skipping
beats scanning, and descending beats skipping.

### Win: GROUP BY, 2.7× over SQLite

262 ms against SQLite's 699 on 200 groups over two million rows, and 389 against
937 on 20,000 groups. Both engines are doing the same thing; mpedb's fold is
simply tighter. It is still 219× off DuckDB, which is what vectorised
aggregation buys.

### Loss 1, closed by a schema line — and a second correction

**Re-measured 2026-07-23:** 3.0 → **1.5 ms** once the benchmark schema declares
its columns NOT NULL. The plan now reads *"aggregate via index 1 — index-tree
scan (narrow entries, NULL-skip free)"*.

This finding now carries two corrections, and the second is worth stating as
plainly as the first. Yesterday's text claimed the narrow-tree machinery "does
not take it at all", citing a decline at `aggregate.rs:989`. That line is in
`try_fused_fold` — a different helper. The truth: the machinery **exists, is
tested** (`agg_over_index.rs` asserts `count(*)` rides the narrowest
all-NOT-NULL tree, PLAN_FORMAT 59), and did not engage here because this
benchmark's own schema declared nothing NOT NULL — under which the PK-range
count was the only correct behaviour available, exactly as the first correction
suspected. The 17× was 100% schema, 0% engine.

The original analysis of the mechanism follows, kept for the part that remains
true and load-bearing: WHY NOT NULL is the admission guard.

The plans, verbatim:

```
mpedb:   Select fact
           access: FullScan
           aggregate: count(*)

sqlite:  SCAN fact USING COVERING INDEX fact_amount
```

SQLite counts entries in the **narrowest** tree it has. `fact_amount` is a float
plus a rowid, roughly 16 bytes per entry, so counting two million of them means
reading a cell count out of the header of about 8,000 leaf pages. mpedb counts
the **PK tree**, whose entries are whole seven-column rows — perhaps 50 to a
leaf, so around 40,000 pages for the same answer.

**But mpedb cannot simply copy that, and the reason is a correctness rule, not
an oversight.** mpedb follows the SQL membership rule: a row with a NULL in any
indexed column has *no index entry* (`engine/mod.rs:426`). So an index's entry
count is **not** the table's row count in general — it is the count of rows with
no NULL in that index's columns. SQLite stores NULLs in its indexes, so for it
the two are always equal and the shortcut is unconditional. For mpedb it is
available only when every column of the index is declared `NOT NULL`.

The code today does not take it at all: `try_agg_index` explicitly declines
every all-`count(*)` query (`aggregate.rs:989`, *"try_count_only's leaf-wholesale
territory"*) and hands it to `try_count_only`, which counts the **PK range**.
That is leaf-wholesale, and correct, and over the widest tree in the table.

So the fix is narrower than "pick the narrowest index" and has a guard:

> when every aggregate is a bare `count(*)` over an unfiltered scan, and some
> secondary index has **all** its columns `NOT NULL`, count the narrowest such
> index instead of the PK tree.

The benchmark's own schema does not declare any column `NOT NULL`, so on this
dataset today's behaviour is the only *correct* one available — part of this
17× is the schema, not the engine. A real star schema declares its keys NOT
NULL, and the re-measure after the fix has to declare them to mean anything.
Recorded that way rather than as a clean defect, because a "fast" count that
silently skipped NULL-bearing rows would be the exact failure this benchmark
exists to catch.

### Loss 2, FIXED the same day: per-index NDV flips the star — 5.9× recovered

**Re-measure after commit `2f4c7b7`** (CostSource seam + NDV bucket +
`Database::analyze()`): `join-star-2` 1198 → 203 ms, `join-star-4` 1325 → 336,
`join-bad-order` 1099 → 196. The plans now read
`product [index] -> fact [index]` — the dimension drives and the fact is entered
through its join-key index, which is exactly SQLite's plan. Every row still
agrees. The residual 1.7–2.1× against SQLite is per-probe execution cost, not
plan choice, and belongs to the same family as the prepared-point gap below.

The paragraphs that follow are the original analysis of the loss, kept because
the mechanism — and why the fix is a cost *input*, not a solver change — is the
finding.

```
mpedb:   join order: fact [scan] -> product [pk]
         access: FullScan
         inner join product … PkPoint(id = fact.product_id)
           on: true AND (product.category = 'tools')

sqlite:  SEARCH p USING COVERING INDEX prod_cat (category=?)
         SEARCH f USING INDEX fact_product (product_id=?)
```

SQLite enters the **dimension** first: 556 `tools` products through a covering
index, then a probe into `fact_product` per product — touching roughly 200,000
fact rows. mpedb scans all 2,000,000 fact rows, probes `product` by primary key
for each, and discards 89% of them on `category = 'tools'` afterwards.

The join order is not a bug in the solver. It is the documented cost model
working as specified: MPEE prices BOUNDED and UNKNOWN predicates *identically,
at full row count*, because it optimises the worst case rather than the expected
one. `product.category = 'tools'` therefore cannot be seen as selective at all.
That discipline is load-bearing — plan bytes are content-hashed, and a cost that
moved with data would churn plan hashes — but on a star schema, which is the
canonical analytics shape, it loses every time.

**MPEE is doing real work even so.** The kill switch (`MPEDB_NO_MPEE=1`) makes
the A/B measurable:

| query | MPEE on | MPEE off | |
|---|---:|---:|---|
| `join-star-2` | 1198 | 1181 | nothing to reorder, two tables |
| `join-star-4` | **1325** | 2467 | **1.9× — it pulls the filtered dimensions earlier** |
| `join-bad-order` | **1099** | 1227 | 1.1× |

So the solver reorders correctly *within* the plan space it is given. What it
cannot do is leave that space: entering the dimension first requires driving
`fact` through its `product_id` index instead of scanning it, and the cost model
has no reason to prefer that when the dimension's filter is priced at 100%.

The fix is a cost input, not a solver change — which is the interesting part,
because MPEE's cost function is a pluggable component and always was. A
selectivity estimate for equality on a low-cardinality indexed column would do
it, and to keep plans content-hash-stable it has to be as coarse and
deterministic as today's `row_count`: bucket the index's distinct-key count in
log2, the same way table row counts are bucketed, so the estimate cannot move
until the data doubles.

## What moved, and what a graph-shaped win did NOT move

**Re-verified 2026-07-23** (DuckDB and SQLite unchanged, same M3): `count-star`
holds at **1.5 ms** (the NOT NULL schema fix), `min-max-indexed` at 0.009 ms,
`count-filtered` 2.1× ahead, and the group and join cells reproduce — every row
still agrees.

**`scan-filter-sum` improved 172.3 → 126.0 ms** (`dca70b1`): a filtered
aggregate used to abandon the fused fold the moment a `WHERE` existed and fall
back to the generic gather — one `Vec<Value>` and a whole-row decode per row,
two million times. It now carries the predicate: `Instr::PushCol` is the only
instruction that reads the row, so the program's read set is COMPLETE, and the
fold decodes exactly the predicate's columns plus the aggregate's into one
reused buffer. Against SQLite — the row-store control group, which is the
comparison this cell can actually be judged by — that closes 2.9× to **2.1×**.

**The covering-index win that halved the graph bench did nothing here, and the
reason is the point.** That optimization rebuilds a row from an index ENTRY,
which requires the index's columns plus the primary key to be ALL of the
table's columns — true of an edge table `(id PK, src, dst)` under a
`(src, dst)` index, false of a six-column fact table under single-column
indexes. It is a junction-table win, not an analytics win, and measuring it
here was how that got established rather than assumed.

**An index range now prices its own selectivity** (`daacf20`). It used to be
taken on SHAPE — a range predicate over an indexed column got the index, at any
selectivity — and that path fetches ONE ROW PER ENTRY, a PK-tree descent and a
row decode each. With no histograms the fraction cannot be predicted, so the
engine measures it: walk the range's KEYS (no descent, no decode) and stop the
moment the count passes `rows / 8`; under the line fetch per entry as before,
over it scan and keep the rows the index would have held, by rebuilding each
row's index key and testing it against the same raw bounds. The switch point is
measured — ~1.24 µs per matched row fetched against ~0.11 µs per row scanned,
level near 1/11 of the table — and deliberately conservative, so a selective
range cannot regress (measured unchanged at 1 % and at 0.07 %).

`scan-range-sum` is the cell that shape deserved, added with the fix so it is
measured rather than asserted: **410.9 → 184.2 ms**, which turns a 1.7× loss to
SQLite into a **1.4× win**. This was invisible on this page until the cell
existed, which is the argument for adding it.

**The residual on every scan, group and join cell is per-row executor cost** —
a `Vec<Value>`, an expression program per projection — the same constant the
graph page's remaining 2.9× is made of. That one is shared, and it is the
honest next target. A second, narrower hole is measured and open: the index
range does not push a `LIMIT` down, so `… WHERE day_id >= 1000 LIMIT 5` pays
for the whole range (612 ms on the 2M-row table) instead of stopping at five.

## What the extension layer adds here

Deliberately NOT operator sugar: the star queries read naturally as SQL, and a
macro would rename them, not speed them. OLAP's lever in the extension layer
is the **cost layer** (stage M5): `mpedb tune set ndv_discount=…` and the
stored cost-policy spell move the stage-A star flip coherently across every
attached process (tested in `crates/mpedb/tests/cost_layer.rs`), and
`mpedb stats` shows the row/NDV buckets these plans price by. Where graph and
vector got a language, OLAP got a pricing console — each domain's actual gap.

## What this does not measure

- **Concurrency.** Single client throughout. mpedb's multi-process writers, its
  reason to exist, do not appear in a single-threaded analytics benchmark at
  all.
- **Larger than memory.** Two million rows fit in page cache on every engine
  here. Spill behaviour, compression ratios and out-of-core joins are untested.
- **Write amplification and durability.** `durability = "none"`. See
  [BENCHMARKS.md](BENCHMARKS.md) for the durable head-to-head.
- **TPC-H.** Deliberately not run: its value is its published, audited numbers,
  and producing comparable ones needs the full qualification kit. This is a
  star schema chosen to separate three mechanisms, not a standard benchmark.

## Reproducing

```sh
cargo run --release --manifest-path crates/mpedb-olapbench/Cargo.toml -- \
  --facts 2000000 --reps 5

# the A/B that shows what the join-order solver is worth
MPEDB_NO_MPEE=1 cargo run --release \
  --manifest-path crates/mpedb-olapbench/Cargo.toml -- --facts 2000000 --reps 5
```

The crate is **excluded from the workspace**: it links a bundled DuckDB, whose
C++ amalgamation is a multi-minute compile, and as a member it would sit in
front of every `cargo test --workspace` to build a benchmark nobody is running.
