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
| `scan-sum` | scan | 44.3 | 0.485 | 35.8 | 91× slower | 1.2× slower |
| `scan-filter-sum` | scan | 181.8 | 0.697 | 63.3 | 261× slower | 2.9× slower |
| `scan-multi-agg` | scan | 177.7 | 0.938 | 97.7 | 190× slower | 1.8× slower |
| `count-star` | precompute | 3.0 | 0.218 | 0.180 | 14× slower | **17× slower** |
| `min-max-indexed` | precompute | **0.008** | 1.9 | 61.4 | **252× faster** | **7,675× faster** |
| `count-filtered` | precompute | 0.165 | 0.365 | 0.007 | 2.2× faster | 24× slower |
| `group-small` | group by | 262.5 | 1.2 | 698.9 | 219× slower | **2.7× faster** |
| `group-large` | group by | 389.0 | 8.1 | 937.2 | 48× slower | **2.4× faster** |
| `join-star-2` | join order | 1198.0 | 1.9 | 110.3 | 630× slower | **11× slower** |
| `join-star-4` | join order | 1325.0 | 3.8 | 173.8 | 349× slower | **7.6× slower** |
| `join-bad-order` | join order | 1098.5 | 2.7 | 90.1 | 401× slower | 12× slower |

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

### Loss 1: `count(*)` counts the wrong tree — 17× slower than SQLite

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

mpedb has the machinery: `agg_index_choice` exists precisely to pick the
fewest-column index that serves the aggregate set, and `count_index_entries`
counts leaves wholesale without reading a key. For `count(*)` **every** index
serves. The plan above shows it did not engage, and the cheapest-first ordering
in `try_agg_index` is the suspect: the PK-range wholesale count wins before the
narrowest-tree choice is considered. Choosing the widest available tree is
exactly backwards for a count.

This is a fix, not a redesign.

### Loss 2: the star schema defeats worst-case costing — 7.6–12× slower than SQLite

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
