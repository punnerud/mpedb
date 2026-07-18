# DESIGN-DURABLE-RUNTIME — mpedb as a durable, parallel, hot-swappable execution substrate

**Status: design (2026-07-18). Phase 7+, the capstone that ties the vision together. Builds on
PySpell (#11), MVCC snapshots, the plan **footprint** (already computed for concurrency), deterministic
content-hashed plans, #74 (runtime budget), #79 (determinism/replication), #80 (live upgrade), and
MPEE. This is the DBOS thesis — a DBMS-backed runtime — realized on mpedb.**

## 0. The idea

Hold **all** of a program's variables and data in mpedb, and the program gains four properties for
free: it becomes **durable** (stop and resume from the exact point, crash-safe), **auto-parallelizable**
(sequential source, parallel execution where safe), **hot-swappable** (change the code with no
downtime while state persists), and **time-travel-debuggable** (inspect and replay any past state).
This is DBOS (Stonebraker/MIT: run the application on the database, state in the database), with
mpedb's specific machinery — footprints, MVCC, deterministic plans — doing the work.

The reason it is credible and not a castle in the air: **all four rest on machinery that already
exists.** The footprint that detects write conflicts IS the parallelization dependency graph. The
determinism that powers #79's replicated state machine and #74's reproducible counter IS what makes
replay, parallel scheduling, and hot-swap safe. One property, reused four ways.

## 1. Durable execution — stop and resume from the exact point

A program is a sequence of **steps**, each a transaction that reads/writes state in mpedb. State is
**fully externalized** — no hidden in-memory variables that would be lost on a stop. So:
- crash/stop → restart loads the persisted state and continues from the next step;
- each step is **exactly-once**: re-running a crashed-mid-step transaction is safe because it is a
  transaction (COW + group-commit: it either committed or it didn't);
- **side effects** (network, disk, sending mail) use the durable-execution *effect* pattern — record
  the effect and its result in mpedb the first time; on replay, read the recorded result instead of
  re-performing it. This is the Temporal/Restate "activity" boundary, and it is the same idea as #79
  §4: externalize non-determinism as recorded data.

## 2. Auto-parallelization of sequential code (MPEE + the footprint)

The programmer writes **sequential** code; the runtime runs it **parallel where safe**. The key: the
**footprint** — which tables/variables a step reads and writes — is already computed by mpedb for
commit-time conflict detection. That footprint **is the dependency graph**: two steps whose footprints
don't overlap (no write-write / read-write conflict) are independent and run in parallel; overlapping
steps serialize. MPEE schedules them (futures/dataflow under sequential syntax). "Continue sequential
and catch up at a block" = a **barrier** where the parallel branch reconverges before a step that
depends on all of it. Bounded by *analyzable* dependencies — a step whose footprint is data-dependent
(which table/row it touches depends on runtime values) must serialize conservatively, the same limit
as the SQL footprint ("table SET static, row VALUES dynamic").

## 3. Hot code swap — no-downtime code upgrade

Because state is **external** to the code, the runtime can swap a function/code unit's implementation
**between steps** (at a transaction boundary — a safe point) and the new code resumes on the persisted
state. This is Erlang/OTP hot code loading, and it is #80's live-upgrade applied to *code* instead of
*schema*:
- **Split the program into functions/code units** → swap one unit at a time; a bad unit is rolled back
  in isolation (per-unit, like #80's per-tenant rollback).
- The state *shape* must stay compatible across a swap, or be migrated — that is exactly #80's
  expand–migrate–contract on the state schema. A swap mid-step is refused; it waits for the step
  boundary (the safe point).
- Content-addressed code (Unison's model) makes "which version ran this step" auditable — and plans
  are already content-hashed, so the substrate is there.

## 4. Time-travel debugging

Every step commits a state snapshot; MVCC lets you **read state as of any past commit**, and
deterministic plans let you **replay a step and get the identical result**. So: set a breakpoint at a
past state, inspect variables, step forward deterministically, or bisect a bug across the state
history. This is DBOS's time-travel debugging, free from the MVCC + determinism mpedb already has.

## 5. The determinism thread (why it all coheres)

Safe parallel scheduling (§2), safe replay/resume (§1), safe hot-swap (§3), and time-travel (§4) ALL
require that a step, given the same input state, produces the same output — **determinism**. This is
the same property behind #79's replicated state machine and #74's reproducible work counter. Non-
deterministic operations (`now()`, `random()`, network) are externalized as recorded effects (§1), so
determinism holds by construction. The whole runtime is one determinism property applied four ways —
which is why it composes instead of being four unrelated features.

## 6. The honest boundary

This is **orchestration-level** durability, not inner-loop. Every state write is a transaction —
cheap at workflow/process-step granularity, ruinous per CPU instruction. So: put a long-running
process's *checkpointable* state (the workflow variables, the step cursor, the accumulated results)
in mpedb; keep a tight numeric loop's counter in registers. This is precisely the Temporal/DBOS
boundary — durable at the step, not the instruction. And **PySpell (#11)** is the lift from Python to
the safe IR that runs against mpedb: not all Python is expressible — hidden in-memory state and
unanalyzable effects fall back to *opaque activities* (durable at their boundary, not parallelized
inside). Say that plainly; the value is in the orchestration layer, not in replacing the interpreter.

## 7. Prior art + staging

Prior art: **DBOS** (the closest — a DBMS-oriented OS/runtime), **Temporal / Restate / Azure Durable
Functions** (durable execution), **Erlang/OTP** (hot code swap + supervision), **Unison**
(content-addressed persistent code), **Linda** tuple spaces (coordination through a shared store),
and time-travel debuggers (rr). Staging:
1. **Durable state + stop/resume** — state in mpedb, step = txn, effect recording, restart-and-continue.
2. **Footprint-driven auto-parallel** of independent steps + barriers.
3. **Hot code swap** at txn boundaries + state-schema expand–contract (#80).
4. **Time-travel debug** (MVCC snapshot inspection + deterministic step replay).

Phase 7+, capture-ahead — built only after the SQL-parity sprint and on top of PySpell #11 / #74 /
#79 / #80 / MPEE. The step/effect/swap protocol gets commit-path-class review (exactly-once under
SIGKILL, swap-at-safe-point, replay determinism) before any line ships.
