# DESIGN-UDF — host scalar UDFs (the C-API `create_function` path)

**Goal.** Let a C-API consumer register a scalar function via
`sqlite3_create_function[_v2]` and have SQL that calls it invoke the callback.
This is the **Django gate**: Django registers ~30 UDFs (`django_date_extract`,
`django_date_trunc`, `regexp`, `django_power`, …) on *every* connection, and
until the shim accepts + dispatches them, no Django connection opens
(measured — see `crates/mpedb-capi/workbench/`).

This is a deliberate departure from the "UDFs via PySpell compiled IR, not
callbacks" plan: a real sqlite drop-in needs the callback path. Approved.

**Stage 1 scope: SCALAR functions only.** Aggregate UDFs (`xStep`/`xFinal`) and
`create_collation` are later stages; register calls for them refuse cleanly
(invoking the caller's `xDestroy(pApp)` so CPython doesn't leak the callable).
Stage 2 (aggregates) has since shipped — see §Stage 2 at the end; the sections
below describe the scalar path, which stage 2 reuses wholesale.

## The four layers

### 1. Shim (`mpedb-capi`)
`sqlite3_create_function_v2(db, zName, nArg, eTextRep, pApp, xFunc, xStep,
xFinal, xDestroy)`:
- `xStep`/`xFinal` set (aggregate) → clean refuse for stage 1 (call `xDestroy`).
- `xFunc` set (scalar) → store `HostFn { name, n_arg, x_func, p_app, x_destroy }`
  on the `Sqlite3` handle, and register a Rust closure with the facade
  (below). Re-registering the same `(name, n_arg)` replaces (calling the old
  `xDestroy`); `xFunc == NULL` deletes.
- Make the already-exported UDF-callback accessors REAL (they are NULL/0 stubs
  today): `sqlite3_value_{int,int64,double,text,bytes,type,blob}` read from a
  shim `sqlite3_value`; `sqlite3_result_{int,int64,double,text,blob,null,error,
  error_code}` write a result cell on a shim `sqlite3_context`;
  `sqlite3_user_data(ctx)` returns `pApp`.
- The registered closure `Fn(&[Value]) -> Result<Value>`: build a `sqlite3_value`
  array from the argument `Value`s, a `sqlite3_context` with an empty result
  cell, call `xFunc(ctx, argc, argv)`, then map the result cell (or a
  `sqlite3_result_error`) back to a `Value`/`Error`. All allocations are
  per-call and freed after; text/blob copy in and out (no aliasing the engine's
  buffers).

### 2. Facade (`mpedb::Database`)
A per-Database UDF registry: `RwLock<HashMap<(String, i32), Arc<dyn Fn(&[Value])
-> Result<Value> + Send + Sync>>>` keyed by name + arg count (`-1` = variadic).
`register_host_function` / `unregister_host_function`. It reaches:
- the **binder** at compile (names + arities only), and
- the **executor** at run (the closures).

**Plan-sharing.** A plan that calls a host UDF is valid ONLY for a connection
that registered that UDF, so it MUST NOT enter the shared content-hashed
`plan/<hash>` registry. Stage-1 rule: **a compiled plan containing a `HostCall`
bypasses the shared registry** — the facade compiles-and-executes it locally and
never publishes it. (Cost: recompile per execute for UDF queries — acceptable;
a later optimization folds a UDF-set fingerprint into the plan hash to restore
caching.) The binder marks a plan as "contains host call" so the facade can gate
publication.

### 3. Binder (`mpedb-sql`)
Function resolution today: an unknown name is a compile error. New rule: the
binder is threaded a `&HostUdfSet` (the registry's names + arities, like
`bare_group_by` is threaded). A call `f(args)` that matches no native
`ScalarFn`/`AggFn` but DOES match a registered `(name, argc)` (or `(name, -1)`)
compiles to `BExpr::HostCall { name, args }`. A host UDF is dynamically typed:
result type `ColumnType::Any`, args passed through unchanged (any type). Still an
error if the name matches nothing (native or host).

### 4. Expr IR + exec (`mpedb-types` + `mpedb`)
New `Instr::HostCall(name_const_idx, argc)` — the plan stores the function NAME
(const pool) + arg count, NOT the closure (closures aren't serializable).
New opcode + **PLAN_FORMAT bump**. The `ExprProgram` evaluator gains an optional
`host_fns: &HostFns` (name → closure), threaded through the exec eval entry
points (the cross-cutting part). At `HostCall`: pop `argc` values, look up the
closure by name, invoke, push the result. No closure at eval → clean error
(defensive; the binder already checked).

**Footprint / determinism / budget.** A host UDF sees only its arguments (never
the DB), so it is footprint-neutral — no table reads to precompute. Non-
determinism is the caller's problem (sqlite has the same property). The #74
work-budget still bounds a runaway query that calls a UDF per row.

## Verification
Extend `crates/mpedb-capi/tests/capi.rs` with an FFI test that registers a C
scalar function (e.g. `plus1(x)=x+1`, a text `upper2`, a 2-arg function) and
checks it runs in `SELECT`/`WHERE`. Then the workbench: Django `migrate` must get
PAST `register_functions` and create its tables. Track Django progress in
`C-API-COMPAT.md`; each subsequent blocker becomes the next item.

## Stages
1. **Scalar dispatch** (this doc) → Django connects, migrates, basic ORM. DONE.
2. **Aggregate UDFs** (`xStep`/`xFinal` + `aggregate_context`). DONE — §Stage 2.
3. **`create_collation`** (custom collations).

## Stage 2 — AGGREGATE UDFs (`xStep`/`xFinal`)

Same four layers, one extra idea: an aggregate has STATE, and the state is
per-GROUP.

### 1. Shim
`create_function_v2` with `xFunc == NULL` and BOTH `xStep`/`xFinal` set registers
an aggregate; half a pair is `SQLITE_MISUSE`; all-NULL deletes the `(name,nArg)`
entry from both registries. The tracked `HostFn` carries an `aggregate` flag so a
replace/close removes the entry from the registry it actually went into.

`sqlite3_aggregate_context(ctx, nBytes)` is real. The memory lives in the
per-group accumulator (`udf::AggMem`): the first call with `nBytes > 0`
allocates that many ZEROED bytes; every later call in the SAME aggregation —
`xFinal` included — returns the SAME pointer; `nBytes <= 0` never allocates and
returns NULL when nothing was allocated, which is how `xFinal` recognizes an
empty group. The buffer is freed when `xFinal` consumes the accumulator. In a
SCALAR callback the context has no aggregation and the call returns NULL, as
sqlite does for that misuse.

### 2. Facade
`register_host_aggregate(name, n_arg, factory)` / `unregister_host_aggregate`,
in a registry beside the scalars. The value is a FACTORY (`Fn() -> Box<dyn
HostAggState>`), called once per group — two groups never share state. The
executor gets a `HostAggs` resolver (the aggregate twin of `HostFns`).

### 3. Parser + binder + plan
Unlike a scalar, a host aggregate is resolved in the **parser**: `myagg(DISTINCT
x) FILTER (WHERE …)` is aggregate GRAMMAR and the branch must be taken before the
argument list is read. `HostUdfSet` therefore carries the aggregate `(name,
n_arg)` pairs too, and the parser emits `ast::Expr::Agg(AggTarget::Host(name),
…)` — which makes `contains_agg` route the whole SELECT to the aggregate planner
with no other change. A built-in name always wins, so `count` can't be redefined.

`AggFn` stays a closed enum; the CALL carries `AggTarget = Native(AggFn) |
Host(String)` (in `mpedb-types`, beside `AggFn`). Every rule that is about a
specific built-in — `count(*)`'s argument shape, the min/max bare-column witness,
the never-NULL typing — goes through `AggTarget::native()`, so a host aggregate
can never be mistaken for one. Its result type is `ColumnType::Any`, exactly as a
host scalar's is.

**Call shape: exactly one argument.** `Expr::Agg` carries a single optional
argument, so a registration for any other arity is refused at the call site with
a message naming the arity (registration itself always succeeds — Django
registers in bulk and must not fail there). `-1` (variadic) is accepted and
called with its one argument. Windowing a host aggregate (`OVER`) is refused:
that needs `xValue`/`xInverse`, i.e. `create_window_function`, which the shim
refuses.

**PLAN_FORMAT 39 → 40.** Each `AggCall`'s leading function byte becomes a
discriminated tag: 1..=7 is the `AggFn` byte, unchanged, and **0** — a value
`AggFn::from_tag` always rejected — introduces a host aggregate followed by its
length-prefixed name. Native aggregate plans encode byte-for-byte as in 39.
`CompiledPlan::contains_host_call` covers `AggTarget::Host`, so a plan naming a
host aggregate is kept out of the shared `plan/<hash>` registry by the same gate
as a `HostCall`.

### 4. Exec
`exec/aggregate.rs` accumulates through an `Acc` wrapper: a built-in `Accum`, or
a host state. Per group the host state is minted on first row, stepped once per
surviving row — after WHERE, the policy predicate, `FILTER (WHERE …)` and the
DISTINCT dedup, identically to a built-in — and finished at the group's end. An
EMPTY group still finishes a FRESH state, so `xFinal` runs on a never-stepped
(NULL) aggregate context: Django's `STDDEV_POP` over no rows is NULL.

**NULL is the one place the two differ, deliberately.** A built-in SKIPS NULL
arguments; sqlite hands a USER aggregate every row, NULLs included, and lets
`xStep` decide — which is what Django's accumulators expect.

Scope is the READ path, as in stage 1: a host aggregate in a write statement
returns a clean "not in scope" error.
