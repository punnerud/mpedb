# Expression evaluation: where a residual filter's time goes

`mpedb-bench --expr <rows>`. No database, no disk — `ExprProgram` alone.

## Why

`olap.md` records the gap: on a 2M-row fact table `scan-sum` costs mpedb
30.9 ms and `scan-filter-sum` 120.0 ms. The filter alone is 89 ms, against
SQLite's 25 and DuckDB's 1.6. mpedb is otherwise within 1–2× of SQLite, so
that ratio is the outlier.

Two ways to close it were on the table — copy-and-patch JIT (malisper's 5 µs
stencils, 11–18× over an interpreter) and vectorised kernels (DuckDB's answer)
— and both accelerate a loop. Neither helps if the loop's cost is somewhere
else, so this measures where it is before anything is built.

## The decomposition

Three arms, the SAME predicate (`lon >= a AND lon <= b AND ts >= c AND
ts <= d`, 15 opcodes) with the SAME opcode sequence, varying only what a stack
slot holds. Dispatch is identical in the first two, so the difference between
them is boxing and nothing else.

| arm | stack slot | models |
|---|---|---|
| `value-stack` | `Value` — 32 B, `needs_drop` | the engine today |
| `f64-stack` | `f64` — 8 B, no drop | type specialisation |
| `closure` | none; operands in registers | the JIT / hand-written ceiling |

## Result

200 000 rows, best of 7, one host (2-core, ext4):

| arm | ns/row | vs ceiling |
|---|---:|---:|
| `value-stack` (the engine) | 186.7–187.8 | ~290× |
| `f64-stack` (typed) | 18.3–22.9 | ~28–37× |
| `closure` (ceiling) | 0.6–0.7 | 1.0× |

Ranges, not single numbers, because that is what three runs gave. The
`value-stack` arm is steady to a few tenths of a nanosecond; the `f64` arm
moves more, being small enough that host noise is a visible fraction of it.
Neither wobble touches the conclusion.

**Boxing in `Value` is ~90 % of the distance to the ceiling** (164–170 ns/row
across runs). The interpreter's own remainder — dispatch, the loop, the stack
bookkeeping — is the other ~10 %.

Per opcode: **~11.3 ns** in the `Value` interpreter against **~1.2 ns** in the
`f64` one. Boxing costs roughly ten nanoseconds per opcode; everything else
costs one.

## The bench, measured against itself

A decomposition is worth nothing if the harness does not track what it claims
to. The same arm over programs of growing length:

| opcodes | ns/row | marginal ns/opcode |
|---:|---:|---:|
| 3 | 49.1 | — |
| 7 | 93.7 | 11.2 |
| 11 | 140.2 | 11.6 |
| 15 | 185.5 | 11.3 |

Linear, with a ~15 ns fixed cost per call. This also reconciles the bench with
`olap.md`: the filter there costs ~44.5 ns/row, which is a three-opcode
program at 49 ns — not a different result, a shorter predicate.

## What it bought, end to end

The decomposition above says what expression evaluation costs. It does not say
what a QUERY costs, and the two are not the same thing — so the fast path was
measured through the C API on a 945 234-row table, one binary, with
`MPEDB_NO_FASTPATH=1` as the only difference:

| workload | fast path off | on | |
|---|---:|---:|---|
| full scan + one predicate | 198.7 ms | **170.0 ms** | −14.5 % |
| index range + residual filter | 233.9 | 233.7 | unchanged |
| the microbench above, 15 opcodes | 187 ns/row | 48.6 | −74 % |

Both results are the expected ones. A full scan spends its time deciding rows,
so a cheaper decision shows. An index range spends it fetching rows the index
pointed at, and its filter is a rounding error — which is what an earlier
measurement had already found, when adding four comparisons per row to that
query cost nothing at all.

**A methodological note worth more than the numbers.** The first attempt
compared two BUILDS of the OLAP suite, with and without the change, and showed
exactly nothing: `scan-filter-sum` 21.6 ms against 21.8. The same change under
a runtime switch, one binary, shows 14.5 %. A two-build comparison cannot rule
out the build, and here it was wrong. `MPEDB_NO_FASTPATH` stays in the tree for
that reason — not as scaffolding, but because without it the wrong answer was
unfalsifiable.

An earlier version of this document also read the `value − f64` gap as pure
boxing. It is boxing plus whatever else differs between the two arms, and only
a fraction of it reaches a real query: 74 % off the interpreter is 14.5 % off
the scan that contains it.

## What follows

**Copy-and-patch is cancelled by this measurement, not by opinion.** It removes
dispatch, which is inside the ~10 % — and it costs stencils per architecture
(CI builds x86-64, aarch64, armv7, wasm32 and Windows; wasm32 cannot map
executable memory at all, so the interpreter would have to stay anyway) plus
W^X handling on macOS and Windows. A large permanent surface for a fraction of
a tenth.

**Type specialisation is where the win is**, and it is now in the tree
(`expr::numeric_filter`): a 16-byte `Copy` slot with no drop glue, on a fixed
frame with no per-row allocation, tried only for programs whose opcodes qualify
— settled once at construction, because deciding it per row cost ~14 ns/row on
programs that do not. It is ordinary Rust. mpedb's
plans are already statically typed — the binder enforces rigid typing,
`CompiledPlan.param_types` pins parameters, `SchemaBundle.col_types` knows
every column — so the types exist at compile time and the evaluator throws
them away. An 8-byte, drop-free slot for the numeric case is a ~10× per-opcode
change with no assembly and no platform dependency.

It is also the prerequisite for the vectorised route: no SIMD kernel can
accelerate a loop that moves 32 bytes and runs drop glue per operand.
