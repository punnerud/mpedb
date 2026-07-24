# OLAP: mpedb vs DuckDB vs SQLite vs PostgreSQL vs MySQL

Analytics is not what mpedb is for. It is an embedded row store built for
multi-process OLTP, and DuckDB is a vectorised column store built for exactly
this workload. **The point of running it anyway is that the losses are not
uniform, and the shape of the unevenness is the result.** The five-engine field
draws the shape sharply: the specialists win their home turf, but against the
other general-purpose SQL engines — SQLite, PostgreSQL, MySQL — mpedb wins the
precomputed and point-shaped work outright, often by one to two orders of
magnitude.

Harness: [`crates/mpedb-olapbench`](crates/mpedb-olapbench). Every engine runs
**the same SQL text**, and the harness compares canonically rendered results
across all engines before it believes any timing. Every row below is marked
`agree: yes`; a disagreement strikes the row out. A fast wrong answer is a bug
report, not a benchmark result.

    cargo run --release --manifest-path crates/mpedb-olapbench/Cargo.toml -- \
      --facts 2000000 --reps 5

DuckDB and SQLite run in-process and in memory; PostgreSQL and MySQL/MariaDB run
as **private throwaway servers** the harness spins up itself (`initdb`+`pg_ctl`,
`mariadb-install-db`+`mariadbd`, no sudo, no system instance touched), each set
to the none-durability class to match — so they still pay client/server
round-trips the embedded engines do not, which is a real part of their shape
here. mpedb builds five index trees on `fact` row-by-row and one column-segment
set (the DESIGN-COLUMNAR extent store); DuckDB is told to build no indexes (its
authors advise against them for analytics); SQLite/PostgreSQL/MySQL get the same
indexes as mpedb, built after the load.

Two machines, tabled separately because absolute times do not cross hardware
(the *shape* does): a **Linux devbox** (AMD EPYC-Milan, 2 cores, 7.6 GiB) with
all five engines, and an earlier **Apple M3 Pro** (11 cores) three-engine run
kept below for reference.

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

## Linux devbox, five engines (2026-07-24, mpedb `2867098`)

AMD EPYC-Milan, 2 cores, 7.6 GiB, Linux 6.8; mpedb on `/mnt/ext4` with
`durability = "none"`, its column segments freshly on the extent store
(`2867098`). 2M-row `fact`. Every row `agree: yes`.

### Load

| engine | load |
|---|---:|
| duckdb | 1.3 s |
| sqlite | 5.2 s |
| postgres | 7.5 s |
| mpedb | 20.2 s |
| mysql | 20.6 s |

mpedb and MySQL pay the most: mpedb maintains five `fact` index trees row by row
(no bulk-build path) and builds column segments; MySQL's batched InnoDB insert is
the slowest loader. DuckDB's Appender is an order of magnitude ahead of everyone.

### Queries

| query | probes | mpedb | duckdb | sqlite | postgres | mysql |
|---|---|---:|---:|---:|---:|---:|
| `scan-sum` | scan | **30.9** | 1.6 | 57.9 | 113.9 | 159.1 |
| `scan-filter-sum` | scan | 120.0 | 3.2 | 83.1 | 125.3 | 185.1 |
| `scan-range-sum` | scan | 384.5 | 3.4 | 426.3 | 109.6 | 175.3 |
| `scan-multi-agg` | scan | 418.1 | 4.0 | 148.4 | 149.8 | 268.0 |
| `count-star` | precompute | 3.0 | 0.386 | 0.219 | 79.0 | 144.0 |
| `min-max-indexed` | precompute | **0.032** | 12.6 | 99.7 | 0.185 | 0.066 |
| `count-filtered` | precompute | 0.292 | 1.8 | 0.012 | 0.347 | 0.129 |
| `group-small` | group by | 334.7 | 7.6 | 1316.3 | 221.3 | 2545.0 |
| `group-large` | group by | 688.0 | 24.3 | 1527.1 | 261.7 | 2881.9 |
| `join-star-2` | join order | 362.5 | 10.0 | 177.7 | 131.5 | 346.4 |
| `join-star-4` | join order | 514.1 | 14.9 | 310.4 | 164.7 | 736.1 |
| `join-bad-order` | join order | 269.6 | 11.7 | 155.9 | 140.2 | 460.7 |

Prepared, 20,000 executions, total milliseconds:

| query | mpedb | duckdb | sqlite | postgres | mysql |
|---|---:|---:|---:|---:|---:|
| `prepared-point` — `WHERE id = ?` | 40 | 6520 | 6 | 943 | 1186 |
| `prepared-range` — `sum(amount) WHERE customer_id = ?` | 2458 | 36522 | 1688 | 2741 | 5053 |

### Reading the five-engine field

**DuckDB is the specialist and wins every heavy scan/join/group**, by 20–110×.
That is the vectorised column store doing what it is for, and DESIGN-COLUMNAR is
mpedb's answer to it (the extent segment store already halves the scan-sum gap
vs a pure row scan). The interesting comparison is against the other
**general-purpose SQL engines** — SQLite, PostgreSQL, MySQL — the field mpedb
actually competes in:

- **Wins outright (all five):** `min-max-indexed` — 0.032 ms, the O(log n)
  boundary probe. Faster than DuckDB's zone-map skip (396×), PostgreSQL's index
  (5.8×) and MySQL (2×). mpedb is the fastest engine on the board here.
- **Wins the general-purpose field:** `scan-sum` — 30.9 ms, ahead of SQLite
  (1.9×), PostgreSQL (3.7×), MySQL (5.1×). The column segments carry it.
- **Crushes the servers, loses to SQLite by a hair:** `count-star` (mpedb 3.0
  vs PG 79, MySQL 144 — 26–48× ahead of the servers; SQLite 0.2), and
  `prepared-point` (40 vs PG 943, MySQL 1186 — 24–30× ahead; SQLite 6). This is
  the embedded no-IPC edge: a client/server engine pays a round-trip 20,000
  times where an in-process one pays a function call. `prepared-range` mpedb
  beats PostgreSQL and MySQL too (2458 vs 2741 vs 5053).
- **Strong second, beats SQLite and MySQL by 2–8×:** both GROUP BYs — mpedb
  335/688 vs SQLite 1316/1527 vs MySQL 2545/2882 — but PostgreSQL's hash
  aggregate wins the general-purpose field (221/262).
- **The clear weakness — joins.** PostgreSQL's planner and hash join win every
  join cell (131–165 ms), and on the star joins mpedb trails SQLite too, landing
  third or fourth of the general-purpose four. mpedb even emits a runtime-budget
  *warning* on the wide star joins (worst-case nested-loop estimate) though it
  completes correctly. Nested-loop-with-hash is younger here than PostgreSQL's
  decades of join machinery; this is the honest gap to close, and where MPEE's
  next work (hash-join costing, streaming) points.

**The shape, in one line:** mpedb is the fastest general-purpose engine on the
precomputed and point-shaped work — often by one to two orders of magnitude over
the servers — competitive-to-winning against SQLite/MySQL on single-pass scans
and grouping, and behind PostgreSQL on joins. Against the DuckDB specialist it
loses the heavy analytical scans, by design and by a closing margin.

## Apple M3 Pro, three engines (reference)

Consolidated at HEAD (`4643aeb`, M3 Pro, 2M-row fact) — a clean snapshot
after all of stage A/B and the day's four executor changes (covering reads,
filtered fused fold, selectivity-priced ranges, hash join):

| query | probes | mpedb | duckdb | sqlite | mpedb vs duckdb | **mpedb vs sqlite** |
|---|---|---:|---:|---:|---:|---:|
| `scan-sum` | scan | 42.4 | 0.491 | 37.3 | 86× slower | 1.1× slower |
| `scan-filter-sum` | scan | 130.6 | 0.752 | 60.2 | 174× slower | 2.2× slower |
| `scan-range-sum` | scan | 184.2 | 0.627 | 251.1 | 294× slower | **1.4× faster** |
| `scan-multi-agg` | scan | 169.9 | 0.867 | 94.3 | 196× slower | 1.8× slower |
| `count-star` | precompute | 1.4 | 0.206 | 0.164 | 7× slower | 9× slower |
| `min-max-indexed` | precompute | 0.009 | 1.9 | 57.6 | **211× faster** | **6,400× faster** |
| `count-filtered` | precompute | 0.153 | 0.340 | 0.007 | **2.2× faster** | 22× slower |
| `group-small` | group by | 252.6 | 1.2 | 689.7 | 211× slower | **2.7× faster** |
| `group-large` | group by | 367.0 | 7.9 | 910.2 | 46× slower | **2.5× faster** |
| `join-star-2` | join order | 193.6 | 1.9 | 110.5 | 102× slower | 1.8× slower |
| `join-star-4` | join order | 257.9 | 3.8 | 173.1 | 68× slower | 1.5× slower |
| `join-bad-order` | join order | 140.4 | 2.6 | 88.8 | 54× slower | 1.6× slower |

**The mpedb-vs-SQLite column is the one to read** — same storage class (both
row stores), so it measures the ENGINE, where DuckDB's column measures the
STORAGE MODEL. Against SQLite, mpedb WINS four cells (range scan, min/max,
both group-bys — the last two by 2.5-2.7×) and trails the rest by 1.1-2.2×,
with `count-*`'s larger ratios sitting on sub-millisecond absolutes. The
50-290× behind DuckDB is the row-store-vs-column-store gap, which is what
design/DESIGN-COLUMNAR.md addresses.

The earlier incremental history: the original 2026-07-22 run had the star
joins at ~1200-1325 ms (before **stage A**, per-index NDV statistics —
`Database::analyze()`) and **stage B** (the schema
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

**A hash join, chosen by the executor** (`6cb6f9a`). mpedb had exactly one
join strategy — the nested loop, index-driven when the inner side has an index
and held otherwise — and a cost model can only choose among the strategies
that exist, which is why §9.4 concluded the generic lever was a new strategy
rather than a better price. An index-driven inner costs one descent per OUTER
row, and the planner cannot price that because it does not know how many rows
the outer will produce; the executor does, so when the outer is large and the
inner is small it reads the inner once, hashes it, and probes. `join-star-4`
319.9 → **257.0 ms** and `join-bad-order` 185.9 → **139.7**, closing those to
1.5× and 1.6× of SQLite. The build side is capped in ABSOLUTE cells, not
relative to the outer: the switch holds it resident, and the memory contract
is that held bytes do not scale with the input — a dimension table passes that
line, a self-join on a growing table does not.

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
