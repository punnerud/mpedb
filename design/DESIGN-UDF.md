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
1. **Scalar dispatch** (this doc) → Django connects, migrates, basic ORM.
2. **Aggregate UDFs** (`xStep`/`xFinal` + `aggregate_context`).
3. **`create_collation`** (custom collations).
