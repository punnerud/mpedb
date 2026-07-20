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
`plan/<hash>` registry. This rule is unconditional — it holds for the WRITE path
too (see §The WRITE path). Stage-1 rule: **a compiled plan containing a `HostCall`
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
3. **Write-path dispatch** (Django gap #2) — DONE, §The WRITE path.
4. **`create_collation`** (custom collations).

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

## The WRITE path (Django gap #2 — DONE)

Stages 1 and 2 shipped read-path only: the closures were threaded through
`TxnCtx::host_fns()` / `host_aggs()` for `ReadCtx`, and the write path returned
`None`. That was a bigger hole than it sounds, because **CPython opens an
implicit transaction on the first DML**: from then on every statement — reads
included — runs through `WriteSession`, so in real Django use almost every UDF
call after the first INSERT took the broken path. It worked only with an
intervening `commit()`.

**What closes it.** `exec::WriteCtx` — the write-path twin of `ReadCtx`: a
`&mut WriteTxn` plus the two resolvers. `impl TxnCtx for WriteTxn` could not
carry them (the type lives in `mpedb-core`, which knows nothing about a
connection's UDF registry), so the facade wraps the transaction for the duration
of ONE statement. Every row operation delegates to the transaction unchanged —
the wrapper adds resolution, never behaviour.

`Database::host_tables(plan)` is the SINGLE gate: it snapshots the registries iff
`plan.contains_host_call()`, and every execution path goes through it — the read
path, `WriteSession::run`, and every own-statement site in `ring_exec`
(`lead_and_execute`'s direct and leader branches, and the optimistic-mode serial
fallback). A statement with no host call snapshots nothing and runs byte-for-byte
as before.

Covered from the write side: a UDF in a statement's `WHERE`, in `UPDATE … SET`,
in `ON CONFLICT DO UPDATE`'s SET/WHERE, in a `RETURNING` projection, in the
row-producing side of `INSERT … SELECT`, and in any SELECT (scalar or aggregate)
run inside an open transaction.

**Plan sharing survives untouched.** A plan naming a host UDF is still never
published to the shared `plan/<hash>` registry — `Database::register` gates on
`contains_host_call()` for every path, so a write-path plan is compiled and run
locally exactly like a read-path one. Two consequences are enforced rather than
assumed:
- `run_write_plan` keeps such a plan **off the intent ring**. The ring is a
  cross-PROCESS queue whose leader loads intents BY HASH FROM THE REGISTRY, so an
  enqueued host-call plan could only come back `UnknownPlan` — and worse, the
  closures are connection-local, so no other process may run ours.
- `prepare_intent` refuses a drained foreign intent whose plan contains a host
  call, explicitly, rather than resolving the name against the LEADER's registry
  (same name, different function = a wrong answer). Unreachable in practice; it
  is the belt to the brace above.

`optimistic_eligible` also rejects a host-call plan: `concurrency = "optimistic"`
builds and validates the row off the executor with no resolver, so such a
statement takes the serial path, which has one.

**§5.3 is untouched.** The ring_exec change is *which `dyn TxnCtx` the own
statement executes against*, inside the same savepoint, at the same point in the
round. Posting under the writer lock, result-store-before-READY→DONE, release
from READY, and recovery ignoring DONE slots are all exactly as they were.

### Safety: a UDF is arbitrary caller code inside the write path

- **Panic.** `guard_panic` catches an unwind at the one boundary where caller
  code is invoked — `HostFnTable::call`, and an aggregate's factory / `step` /
  `finish` — and turns it into an ordinary statement error
  (`Unsupported("host function f/1 panicked: …")`). Nothing unwinds through the
  engine, so the executor's own contract handles it: the ring leader rolls its
  per-intent savepoint back, `WriteSession::run` poisons a session whose
  statement was partially applied, and the writer lock is released by the normal
  commit/abort path. (Even an uncaught unwind was survivable — `WriteTxn::drop`
  releases the lock and COW means nothing committed is touched — but a
  `WriteSession` living on a C-API handle is NOT dropped by the shim's
  `catch_unwind`, and would have survived with a torn statement and no poison
  flag.)
- **Slow / blocking.** A UDF called from the write path runs with the single
  writer lock held, so it delays every other writer for as long as it takes. This
  is inherent — sqlite has the same property — and mpedb does not interrupt it;
  the #74 work budget bounds the number of CALLS, not the duration of one. Keep
  write-path UDFs cheap, exactly as `insert_streaming`'s pull source must be.
  Readers are unaffected (MVCC).
- **What it can see.** Only its arguments, from the write side as from the read
  side: the closure receives evaluated `Value`s and returns one `Value`; it is
  never handed the transaction, the snapshot, the schema, or an engine handle. A
  UDF that captures a `Database` of its own and re-enters is REFUSED by the
  ERRORCHECK writer lock ("writer lock re-entered by its owner") rather than
  deadlocking, so it cannot mutate the database behind the executor's back.
- **Intent-ring parameter cap.** No interaction: a `HostCall` is part of the
  plan's expression program, not a parameter, and the plan never rides the ring
  anyway.

### Still refused (and refused cleanly)

- `INSERT … VALUES (<expression>)` — `InsertSource` is `Default | Const | Param`,
  so **any** function call there is refused with "INSERT values must be literals
  or parameters", host UDF or `abs()` alike. This is a general INSERT-surface
  limit, not a UDF one; `INSERT … SELECT` is the working form and carries UDFs.
- Contexts that structurally cannot carry closures: the streaming read path
  (`stream_query`) and the sqlite-backed contexts (`SqliteCtx`, `MergeCtx`).
- A trigger body is compiled from the shared catalog with an EMPTY host set
  (`compile_trigger_body`), so it cannot contain a host call at all — a stored,
  multi-process body must not depend on one connection's registrations. A
  statement's RLS `WITH CHECK` program is likewise evaluated without the
  closures, so a host call there refuses cleanly rather than resolving against
  whichever connection happens to run the write.

All of these refuse with `Unsupported("host function f() is not in scope for this
execution")` / the aggregate twin. That message used to be `Error::Internal`,
which renders as **"internal error (bug in mpedb)"** — telling a user they hit an
engine bug when they hit a documented boundary. It is `Unsupported` now.

## Stage 3: host COLLATING SEQUENCES (`sqlite3_create_collation[_v2]`)

A registered collation is a **comparator** — `(a: &str, b: &str) -> Ordering`,
wrapping sqlite's `xCompare(pArg, nA, pA, nB, pB)` — kept in a per-connection
registry beside the scalar and aggregate ones (`Database::register_host_collation`
/ `unregister_host_collation`). Re-registering under a name REPLACES; a NULL
`xCompare` deletes. `xDestroy` runs when an entry is replaced, deleted, or the
connection closes, and — unlike `create_function_v2` — **never on a failed
registration**, which is sqlite's documented asymmetry and the double-free that
once took CPython's heap down 200 tests later.

### The scope, and why it stops where it does

A host collation orders `ORDER BY <expr> COLLATE <name>`. It reaches nothing
else, because everything else built on a collation turns a value into **key
bytes**:

| position | what it needs | stage 3 |
|---|---|---|
| `ORDER BY … COLLATE <host>` | a comparator | **supported** |
| a column's declared `COLLATE <host>` | the B+tree key encoding | refused |
| `GROUP BY` / `DISTINCT` fold | a canonical fold (`Collation::fold_key`) | refused |
| a comparison's `COLLATE` (rung 1) | an index-probe-compatible order | refused |

An index (and every PRIMARY KEY) is a memcmp-ordered B+tree written under a
BUILT-IN collation. A callback cannot produce sort bytes, so an index built
under BINARY simply cannot answer a host-collated range — and answering it
under BINARY anyway would be a **different row order with no error**, the exact
failure mode this codebase refuses. Every refusal is sqlite's own wording,
`no such collation sequence: <name>`.

The boundary is enforced by TYPE, not by discipline. `Collation` (the three
built-ins) still appears everywhere a key encoding is derived; only the plan's
`ORDER BY` key list carries the new `OrderColl { Native(Collation), Host(String) }`
(PLAN_FORMAT 52, tag 3 + name). There is no way to construct a host collation
for a schema column, a keycode call, or a group-key fold, because those APIs
take a `Collation` and no registration produces one.

### Execution

The name travels IN the plan (not a registry index), so the plan is
self-describing and two connections cannot disagree about which callback a slot
means. A plan whose `ORDER BY` names a host collation reports
`contains_host_call()` and therefore inherits the **no-publish** rule: it stays
in the compiling connection's local cache and never enters the shared registry.

`TxnCtx::host_colls()` threads the snapshot to the sort, exactly as `host_fns`
/`host_aggs` do. Every sort site calls `check_order_colls` FIRST — one pass that
fails with "no such collation sequence" if any named collation is not in scope —
because a `sort_by` comparator has nowhere to report an error, and a silent
fallback there would be a wrong answer. Only after that does the comparator run,
and only for a TEXT-vs-TEXT pair: every other pair is settled by storage class,
as in sqlite. The planner's "ORDER BY is already satisfied by scan order"
elision requires `Native(Binary)`, so a host collation blocks it by shape.
