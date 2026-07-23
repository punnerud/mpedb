# DESIGN-TRIGGERS — `CREATE TRIGGER`, in-SQL and PySpell (design-forarbeid)

**Status: IMPLEMENTED through stage 5** (2026-07-23). Stages 0–4 shipped as
designed (`crates/mpedb/src/trigger.rs`, `crates/mpedb-sql/src/trigger.rs`,
fire points in `crates/mpedb/src/exec/mod.rs`); stage 5 (`EXECUTE PROCEDURE`
PySpell bodies, the `CtxBridge`) and the `RAISE` veto shipped 2026-07-23.
Deviations from the plan, all deliberate:

- **The procedure is pinned by CONTENT HASH at `CREATE TRIGGER`** (§5.1
  said name): procedure re-definition does not bump `schema_gen`, so a name
  binding could diverge between attached processes; the hash cannot (the
  `proch/<hash>` blob is immutable). Re-CREATE the trigger to rebind.
- **`RAISE(FAIL)` is refused** alongside `ROLLBACK` (§4.3 planned FAIL):
  keep-earlier-rows-of-the-statement semantics contradict mpedb's atomic
  statements; refusing beats mis-honouring. `ABORT` carries the user's
  message verbatim (`Error::Raise`, C shim → `SQLITE_CONSTRAINT_TRIGGER`);
  `IGNORE` is `FireOutcome::SkipRow` with sqlite's abandonment scope.
- **`recursive_triggers` shipped as a STORED tunable** (§4.4 planned a
  config option): `tune set recursive_triggers=true|false`, default false =
  sqlite's default. Stored-not-config because cascade behaviour decides what
  a statement DOES — attached processes must agree, and the tunables record
  + gen bump is that machinery. OFF suppresses re-entering an ACTIVE trigger
  (sqlite's rule — covers A→B→A cycles), differential-tested; ON is full
  recursion under the depth cap (`MAX_TRIGGER_DEPTH = 32`). The #74 work
  meter additionally charges one row per (trigger, row) fire, so cascade
  WIDTH trips `RuntimeBudget` deterministically.
- **Backtest** (beyond the plan): `Database::backtest_trigger` /
  `mpedb trigger backtest` replays a trigger — stored or a dry-run
  `CREATE TRIGGER` statement — over the target table's current rows on an
  always-aborted txn and reports fired / WHEN-skipped / ignored / vetoed
  counts and net per-table row effects. No row history exists, so the data
  as it stands is the corpus; UPDATE replays state the identity assumption.
- A spell body's embedded plans are pre-resolved at catalog build; failures
  POISON that one trigger (fire-time error naming the repair), never the
  catalog. A body plan invalidated by DDL re-prepares from the registry's
  stored SQL — the view model, applied to procedures.

The original design rationale follows, kept for the review trail. Triggers
touch the parser, binder, plan/catalog storage, the execute hot path, and the
PySpell (`mpedb-spell`/`mpedb-proc`) interpreter. The plan below is staged so
each increment ships, is testable in isolation, and refuses the
not-yet-supported rest cleanly rather than answering wrongly.

COMPAT.md today: `CREATE TRIGGER | ❌ | planned both as in-SQL triggers and as
PySpell/ETL-layer code`. This document is what makes that entry buildable.

---

## 0. What we are building, and why it fits mpedb's grain

A trigger is a stored rule: *when* rows change in a table (`INSERT`/`UPDATE`/
`DELETE`), `BEFORE` or `AFTER`, `FOR EACH ROW`, optionally gated by a `WHEN`
predicate, run a body. mpedb supports **two body kinds**, chosen at define time
and coexisting on the same table:

1. **In-SQL body** — `BEGIN <stmt>; … END`, a sqlite-subset statement list, with
   `NEW.col` / `OLD.col` row bindings. This is the sqlite trigger model.
2. **PySpell body** — `EXECUTE PROCEDURE <name>(NEW.a, OLD.b, …)`, dispatching
   to a compiled, content-hashed `mpedb-proc` procedure (the ETL/stored-proc
   layer). The proc runs in the *same* transaction as the triggering statement.

Both dispatch from **one** fire point with **one** `NEW`/`OLD` binding
mechanism; only the body executor differs (§4, §5).

### Why the existing machinery already carries most of this

Four mpedb facts make triggers a natural extension rather than a new subsystem:

- **DDL is facade-routed, catalog-stored, never a plan.** `CREATE VIEW`/`CREATE
  POLICY` store their bodies as *source text* in the sys-keyspace
  (`view/<name>`, policy records) and bump `schema_gen` so every attached
  process reloads (`crates/mpedb/src/ddl_apply.rs`,
  `crates/mpedb-sql/src/ddl.rs`). Triggers store `trigger/<name>` the same way.
- **Reserved parameter slots already bind "row-shaped context" filled at exec
  time.** A `CompiledPlan`'s parameter layout is `[user ‖ subplan results ‖
  context]` (`plan/mod.rs`, `subplan_base()`); `current_setting()` context refs
  (`Expr::ContextRef`/`InContext`) and `excluded.<col>` (`Expr::Excluded`, the
  `ON CONFLICT` pseudo-row) are exactly "a name that resolves to a slot filled
  from a row image at execution." `NEW`/`OLD` are two more such pseudo-relations.
- **The executor already recurses in-transaction without re-entering the
  facade.** `exec/mod.rs` runs subplans and `INSERT … SELECT` sources by calling
  `exec_select`/`exec_stmt` against the *same* `ctx: &mut dyn TxnCtx`. A trigger
  body is the same move: recurse on the already-held txn, never call
  `Database::execute` (which would re-take the writer lock / re-enter the ring).
- **PySpell procs already run a whole proc inside one write txn.** A `has_exec`
  proc runs on a `SessionBridge` over one `WriteSession` and commits/rolls back
  atomically (`crates/mpedb-proc/src/engine.rs`). A trigger-dispatched proc is
  the same interpreter with a bridge over the *current* txn instead of a fresh
  session.

### Assessment: no canonical-bytes or PLAN_FORMAT change (§8 in full)

Triggers are **pure sys-keyspace catalog entries**, exactly like views and
policies. They do **not** enter the Schema canonical bytes v2 (which covers only
tables/columns/indexes/PK, DESIGN-SCHEMA-V2) and do **not** need a new
`PLAN_FORMAT`: the triggering statement's own plan (`INSERT …`) is unchanged —
*whether it fires triggers is decided at exec time from the trigger catalog*, not
encoded in the plan. What the feature does need:

- a `schema_gen` bump on `CREATE`/`DROP TRIGGER` so trigger-catalog caches drop
  (identical to views/policies);
- a **binder extension** for the `NEW`/`OLD` pseudo-relations (new reserved-slot
  kind);
- an **exec-path thread-through**: a `TriggerHost` handed into `exec_stmt` so the
  write arms can fire.

The PySpell side reuses the existing `PROC_FORMAT` unchanged (a trigger just
references a proc hash). The only *new* serialized format is the trigger catalog
record itself (§3.3), versioned from day one, `Corrupt`-never-panic.

---

## 1. Grammar (v1 target and refusals)

```
CREATE TRIGGER [IF NOT EXISTS] <name>
    { BEFORE | AFTER } { INSERT | UPDATE [OF <col>,…] | DELETE }
    ON <table>
    [ FOR EACH ROW ]
    [ WHEN ( <bool expr over NEW/OLD> ) ]
    { BEGIN <stmt> ; [ <stmt> ; … ] END          -- in-SQL body
    | EXECUTE PROCEDURE <proc>( <arg>,… )         -- PySpell body
    }

DROP TRIGGER [IF EXISTS] <name>
```

`FOR EACH ROW` is the only granularity (sqlite's only mode). It is accepted and,
if omitted, **assumed** — mpedb has no set-level trigger. `FOR EACH STATEMENT` is
a **named refusal** (Postgres-only), never silently downgraded.

Row-binding availability by event (sqlite's rule, enforced at bind time):

| Event    | `NEW` | `OLD` |
|----------|-------|-------|
| INSERT   | yes   | —     |
| UPDATE   | yes   | yes   |
| DELETE   | —     | yes   |

Referencing an unavailable binding (`OLD` in an INSERT trigger) is a bind-time
error, not a runtime NULL.

### Named refusals in v1 (visible gaps, never wrong answers)

- `INSTEAD OF` triggers (they need updatable views; mpedb has none — DESIGN-VIEW
  §0). Refuse.
- `FOR EACH STATEMENT`. Refuse.
- **`BEFORE` triggers that mutate `NEW`.** In sqlite, `NEW` is *read-only* inside
  a trigger body — you cannot assign `NEW.col := …` (that is Postgres PL/pgSQL).
  v1 follows sqlite exactly: `NEW`/`OLD` are read-only bindings. A body statement
  that tries to write to `NEW` does not parse (there is no syntax for it in the
  sqlite subset). This is the single biggest simplification and it is the
  *correct* line, not a shortcut.
- Multiple statements in the body beyond the v1 executor's staged support (§7):
  refused with "trigger body statement N unsupported", body still stored.
- `WHEN` referencing anything but `NEW`/`OLD`/constants (no subqueries in `WHEN`
  in v1). Refuse.
- Recursion beyond the depth cap (§4.4): runtime refusal, whole statement aborts.

---

## 2. Parsing

`CREATE/DROP TRIGGER` are DDL: they route through `mpedb_sql::parse_ddl`
(`parser/ddl.rs`), not through the ordinary statement compiler, exactly like
`CREATE VIEW`. Add `DdlStmt::CreateTrigger { … }` and `DdlStmt::DropTrigger { … }`
to `crates/mpedb-sql/src/ddl.rs`, and a `parse_create_trigger` arm to
`parse_ddl`'s `"create"` dispatch (after the `VIEW` arm).

Following the `CREATE VIEW` precedent, the **body is captured as source text**,
not parsed into an AST at DDL time:

- The `BEGIN … END` block is captured verbatim between the balanced `BEGIN`/`END`
  keywords (the same balanced-capture idea as `capture_paren_source` in
  `parser/ddl.rs`, generalised to a keyword-delimited region). The `WHEN (…)`
  predicate is captured with the existing `capture_paren_source`.
- The `EXECUTE PROCEDURE name(args…)` form captures the proc name and the
  argument expression list (each arg is `NEW.col`, `OLD.col`, a literal, or a
  simple expression over them).

The parsed spec is pure metadata + text:

```rust
pub struct CreateTriggerSpec {
    pub name: String,
    pub timing: TriggerTiming,      // Before | After
    pub event: TriggerEvent,        // Insert | Update { of: Vec<String> } | Delete
    pub table: String,
    pub when_src: Option<String>,   // captured predicate source
    pub body: TriggerBodySpec,      // Sql(String) | Proc { name, arg_srcs: Vec<String> }
    pub if_not_exists: bool,
}
```

The parser does **not** validate `NEW`/`OLD`/columns/proc existence — that is the
facade's job at apply time (mirrors how `CreateTableSpec` leaves id/PK resolution
to `ddl_apply.rs`, and how policy predicates are re-bound later).

---

## 3. Storage (sys-keyspace, `schema_gen`-gated)

### 3.1 Where

A new sys-keyspace prefix in `ddl_apply.rs`, beside `VIEW_PREFIX`:

```
trigger/<name>  →  canonical trigger record (§3.3)
```

Keyed by trigger name (globally unique across tables, like sqlite — a trigger
name is not table-scoped). `apply_create_trigger` / `apply_drop_trigger` follow
`apply_create_view` / `apply_drop_view` line for line:

1. `refresh_schema_if_stale()`, resolve the target table (must exist and be a
   real table, not a view — refuse a trigger on a view in v1).
2. Bind-check the body at apply time (compile it, §3.4) so a broken trigger is
   rejected at `CREATE`, not discovered at fire time. (This is *stricter* than
   views, which defer to reference time — but a trigger has no "reference," it
   just fires, so define-time validation is the only place to catch errors
   loudly. Same reasoning as `mpedb-proc`'s define-time compile.)
3. One catalog commit: `sys_put(trigger/<name>, record)` + `bump_schema_gen()`.
4. Infallible `cache.clear()` + best-effort `reload_schema_from_catalog()`, the
   established DDL tail.

`DROP TABLE` must also drop that table's triggers (cascade), and reject/orphan
per the DESIGN-DROP-TABLE positional-audit rules — a trigger record naming a
dropped table is dead. `ALTER TABLE … RENAME` must rewrite the stored `table`
name (or triggers store the table **id**, not name — preferable, since ids are
stable across rename; see §3.3).

### 3.2 Loading — `load_trigger_catalog`, gated

A `load_trigger_catalog()` on `Database`, twin of `load_view_catalog()`: scan the
sys-keyspace for `trigger/*`, decode each record, group by `(table_id, event,
timing)`. Cached behind the **`schema_gen` gate** (`gate_cache_on_schema`) — the
catalog is rebuilt only when a DDL commit (here or in another process) moves the
gen. This is the same freshness contract views and policies already rely on, and
it is what keeps `CREATE TRIGGER` visible to every attached process (including a
*different* process acting as the intent-ring leader, §4.5).

### 3.3 Record format (new, versioned, `Corrupt`-never-panic)

A minimal, bounds-checked, self-describing blob (the mpedb decoder discipline —
`mpedb_types::expr`, `mpedb-proc/src/ir.rs`). Fields:

```
MAGIC "MTRG" | u16 TRIGGER_FORMAT=1
table_id: u32                       // stable across RENAME (§3.1)
timing:   u8  (0 Before, 1 After)
event:    u8  (0 Insert, 1 Update, 2 Delete)
update_of_cols: [u16]               // empty = all columns (UPDATE only)
name: len-prefixed utf-8
when_src:  optional len-prefixed utf-8      // predicate source
body:  tag 0 = Sql(len-prefixed utf-8)
       tag 1 = Proc { name, arg_srcs: [utf-8] }
```

Body is stored as **source text** (view model), re-compiled on catalog load and
cached in the (gen-gated) `TriggerCatalog`. Rationale: least new surface, and the
re-compile already re-binds against the *current* schema, so a trigger body stays
correct across compatible schema changes for free — the same reason views store
text. (Alternative "compile to plan hashes at CREATE time, store hashes like a
proc" is stronger against hostile blobs but adds registry-lifetime coupling; the
text model is the v1 recommendation, and the record is versioned so a later
switch is a `TRIGGER_FORMAT=2`, not a migration — see the no-backward-compat
standing rule.)

Every decoder path is truncation-at-every-offset tested and returns
`Error::Corrupt`, never panics — non-negotiable per CLAUDE.md.

### 3.4 Compiling a body: the `NEW`/`OLD` binder extension

This is the one genuinely new piece of SQL machinery. When compiling a trigger
body statement (or `WHEN` predicate, or a proc arg expression), the binder runs
in a **trigger scope** that knows two pseudo-relations over the target table:

- `NEW.<col>` and `OLD.<col>` resolve to the target table's column `col`
  (type-checked against the real column type — rigid typing preserved).
- Each distinct `NEW.col` / `OLD.col` reference binds to a **reserved parameter
  slot**, appended after the user/subplan/context slots. The plan's parameter
  layout becomes `[user ‖ subplan ‖ context ‖ NEW cols ‖ OLD cols]`. This reuses
  the *exact* reserved-slot mechanism `current_setting()` already uses
  (`context_keys` aligned to the tail of `param_types`); we add a parallel
  `row_binding_keys: Vec<RowRef>` where `RowRef = { which: New|Old, col: u16 }`.

Because a trigger body statement is an ordinary `CompiledPlan` with a couple of
extra reserved params, **the whole executor runs it unchanged**. The only new
work at fire time is filling those tail slots from the `NEW`/`OLD` row images
(§4.2). No new plan opcode, no PLAN_FORMAT bump for the *triggering* statement;
the body plans are internal to the trigger catalog and may carry the new
reserved-slot kind under their own compilation (a `plan/` format addition scoped
to trigger-body plans, additive and validate-enforced, not a change to ordinary
statement plans).

Refuse in `WHEN` and body: correlated subqueries referencing `NEW`/`OLD` (v1),
aggregates over `NEW`/`OLD`, `NEW`/`OLD` in a position the binder cannot map to a
scalar slot. Refuse, never guess.

---

## 4. Firing — in-SQL bodies

### 4.1 The fire point

Triggers fire from inside `exec_stmt_rest` in `crates/mpedb/src/exec/mod.rs`, in
the `PlanStmt::Insert` / `Update` / `Delete` arms — the only place that has the
per-row `NEW`/`OLD` images and the live `ctx: &mut dyn TxnCtx`. Concretely, per
matched row:

- **INSERT** (`built_rows` loop): `BEFORE` fires *after* `WITH CHECK`/before
  `ctx.insert_row`; `AFTER` fires *after* a successful `insert_row` (and after
  `ON CONFLICT DO UPDATE`, on the row actually written).
- **UPDATE** (`old_rows` loop): `OLD` = the gathered pre-image, `NEW` = the
  computed post-image. `BEFORE` fires before `ctx.update_by_pk`, `AFTER` after
  success. If `event.update_of` is non-empty, fire only when one of those columns
  actually changes (sqlite's `UPDATE OF` semantics).
- **DELETE** (`old_rows` loop): `OLD` = the row, `NEW` unavailable. `BEFORE`
  before `ctx.delete_by_pk`, `AFTER` after.

Ordering when several triggers match the same `(event, timing)`: sqlite leaves it
"undefined but stable"; v1 fires in **creation order** (the catalog preserves it)
and documents that as the contract.

### 4.2 Threading the catalog in: `TriggerHost`

`exec_stmt` currently takes `(ctx, schema, plan, params, partial)`. Add one
parameter: `triggers: &dyn TriggerHost` (an empty no-op host when the table has
no triggers, so the common path pays a single "is this table trigger-free?"
branch and nothing else). The host is constructed by the facade layers that have
`&Database` — `run_plan`/`run_write_plan`, the `WriteSession::run`, and the ring
leader (`ring_exec::lead_and_execute`) — from the gen-gated `TriggerCatalog`.

The host exposes:

```rust
trait TriggerHost {
    fn fire(&self,
            ctx: &mut dyn TxnCtx, schema: &Schema,
            table: u32, event: Event, timing: Timing,
            new: Option<&[Value]>, old: Option<&[Value]>,
            depth: u32) -> Result<FireOutcome>;
}
```

`fire` looks up matching triggers, and for each: builds the body-param buffer by
copying the trigger's user params (none, for a body — bodies take no caller
params) and filling the `NEW`/`OLD` reserved tail from `new`/`old` per the plan's
`row_binding_keys`; evaluates `WHEN` (an `ExprProgram` over that buffer, 3VL:
NULL/FALSE skip, only TRUE fires); then runs each body statement.

### 4.3 Running a body statement: recurse on the same ctx

A body statement is executed by calling `exec_stmt` **recursively** with the same
`ctx`, the same `schema`, the body's `CompiledPlan`, the filled param buffer, and
`depth + 1`. This is the established in-transaction recursion (subplans,
`INSERT … SELECT`); it never calls `Database::execute` and so never re-enters the
writer lock or the intent ring. The body's own writes may themselves fire
triggers — recursion handles that naturally.

`RAISE(ABORT, 'msg')` / `RAISE(FAIL, 'msg')` in a body maps to returning an
`Error` from `fire`, which propagates out of `exec_stmt` and (per the existing
`partial` contract) poisons the session / aborts the autocommit txn — the whole
triggering statement and every effect it caused unwinds atomically. `RAISE(IGNORE)`
means "silently skip this row's remaining trigger work and the row operation" —
map it to a `FireOutcome::SkipRow` the write arm honours (like `ON CONFLICT DO
NOTHING`'s skip). `RAISE(ROLLBACK)` is a v1 refusal (it means "abort the whole
outer transaction," which through autocommit equals ABORT, but through an
interactive `WriteSession` has different scope — refuse until the semantics are
pinned).

### 4.4 Recursion / depth control

A trigger that writes its own table (or a cycle A→B→A) recurses. Guardrails:

- **Hard depth cap** `MAX_TRIGGER_DEPTH` (sqlite uses 1000; mpedb picks a
  conservative value, e.g. 64, since each level is a full statement execution).
  Exceeding it is `Error::Unsupported("trigger recursion too deep")`, aborting
  the statement. This is the `MAX_VIEW_DEPTH` pattern (`view.rs`) applied to
  execution instead of flattening.
- **Recursive-triggers default OFF** (sqlite's historical default): a trigger's
  body does **not** re-fire triggers on the *same* table by default, only the
  depth-capped cross-table cascade. A `recursive_triggers` knob (config option)
  can enable same-table recursion for callers who want it, still under the depth
  cap. Defaulting off is the safe, sqlite-compatible choice and prevents the most
  common runaway (`AFTER UPDATE ON t … UPDATE t …`).
- The whole cascade shares **one transaction** (the leader's / the session's), so
  atomicity and MVCC snapshot semantics are automatic: an outer `UPDATE`'s
  `old_rows` were gathered up front (collect-then-mutate), so trigger-inserted
  rows never retro-join the outer statement's working set — matching sqlite.

### 4.5 Autocommit / intent-ring interaction

Autocommit DML runs through `run_write_plan` → the group-commit leader
(`ring_exec::lead_and_execute`). The leader may be a **different process** that
loaded our intent by hash from the registry. Two consequences, both handled:

- The leader executes `exec_stmt`; it must construct the `TriggerHost` from *its*
  gen-gated `TriggerCatalog`. Since triggers live in the shared sys-keyspace and
  the gen gate forces a reload after any `CREATE/DROP TRIGGER` commit, the leader
  sees the current trigger set. No plan encoding of triggers is needed — this is
  precisely why triggers must be an exec-time catalog lookup, not a plan field.
- The triggering statement's `footprint.read_only` is already `false` (it is
  DML), so routing is unchanged; the trigger's writes to other tables happen in
  the leader's single write txn and are captured by the engine's normal dirty-set
  / CDC path (so mirror replication of trigger effects is automatic — they are
  just more row writes in the same commit).

---

## 5. Firing — PySpell / ETL bodies

`EXECUTE PROCEDURE <proc>(args…)` dispatches to an `mpedb-proc` procedure. Same
fire point, same `NEW`/`OLD` binding; only the body executor changes.

### 5.1 Argument binding

The proc-arg expressions (`NEW.a`, `OLD.b`, literals) are compiled in the trigger
scope (§3.4) to a small list of `ExprProgram`s over the `NEW`/`OLD` reserved
slots. At fire time, `fire` evaluates them against the row images to produce the
proc's positional `args: &[Value]`. The proc's declared `argc` must equal the arg
count — checked at `CREATE TRIGGER` (define-time loudness).

### 5.2 Running the proc in the current transaction — the key constraint

`ProcEngine::call` opens its **own** `WriteSession` (`self.db.begin()`), which
would re-take the writer lock — a deadlock/re-entrancy inside an in-flight txn.
So a trigger-dispatched proc must **not** go through `ProcEngine::call`. Instead:

- Reuse the proc **interpreter** (`interp::run(&proc, args, &mut bridge,
  budget)`) directly.
- Provide a **new bridge** — call it `CtxBridge` — that implements `DbBridge`
  over the *current* `ctx: &mut dyn TxnCtx`, resolving each embedded plan hash and
  running it against that same ctx (via `exec_stmt`), instead of over a fresh
  `WriteSession` (`SessionBridge`) or a read snapshot (`SnapshotBridge`). Plan
  resolution by hash needs the registry/cache; the `TriggerHost` captures the
  `&Database` handle to do it. The proc thus sees the triggering statement's
  uncommitted writes and commits/rolls back with it — the semantics a trigger
  requires.
- The proc runs under its own instruction/db-call/row **budget** (the existing
  `Budget`), which doubles as the recursion/runaway guard for the PySpell side;
  the trigger depth cap still bounds nested SQL statements the proc issues.
- A read-only trigger proc (no `DbExec`) is allowed and runs against the same ctx
  read-side; a writer proc requires the outer statement to already hold the write
  txn (it always does — triggers only fire on DML). A proc-firing trigger on a
  read is impossible (reads fire no triggers).

### 5.3 How the two models coexist

- The body kind is a **tag in the trigger record** (§3.3). One table may have an
  in-SQL `AFTER INSERT` audit trigger *and* a PySpell `AFTER UPDATE` enrichment
  trigger; they are independent catalog rows fired by the same dispatcher.
- Both bodies see the **same** `NEW`/`OLD` binding and run in the **same**
  transaction with the **same** atomicity/rollback and depth/budget guards.
- Injection safety is preserved on both sides: the in-SQL body is compiled once
  (at `CREATE TRIGGER`) to plans; the PySpell body was compiled once (at proc
  `define`) to IR + plan hashes. **No SQL is ever parsed at fire time** — the
  PySpell security invariant ("the parser stays on the host; the runtime only
  sees IR") extends to triggers unchanged.

---

## 6. Content-hashed-plan correctness (the cache-gate argument, in full)

The worry: a cached `INSERT` plan executes without knowing a trigger was created
after it was compiled. Resolution, entirely within existing invariants:

1. The triggering statement's plan **does not encode triggers**. Whether triggers
   fire is read from the `TriggerCatalog` at exec time by the `TriggerHost`.
2. `CREATE`/`DROP TRIGGER` **bump `schema_gen`** (a catalog commit, §3.1).
3. Every execute path calls `gate_cache_on_schema()` first, which drops the plan
   cache **and** (by the same gen check) invalidates the `TriggerCatalog` cache
   when the gen moved. The next statement rebuilds both.
4. Therefore a plan compiled before a trigger existed still runs correctly: the
   plan is unchanged (it does not need to change — it just does its INSERT), and
   the *executor* now consults an up-to-date trigger set. Conversely, dropping a
   trigger stops it firing on the very next statement in every process.

So triggers ride the existing DDL-staleness machinery with **no new coherence
protocol**. The one thing to verify in review: the `TriggerCatalog` cache is gated
on the *same* gen snapshot as the plan cache, so the two can never disagree within
one statement (a trigger created "between" the plan lookup and the fire). Because
both are read after a single `gate_cache_on_schema()` at the top of the execute
call, and DDL is serialized under the writer lock, they cannot skew — the identical
argument DESIGN-VIEW/policy already rely on.

---

## 7. Staged plan (each stage ships and refuses the rest)

Honest scoping: this is comparable in size to `CREATE VIEW` + a binder feature +
an exec-path change. Stages are ordered so the highest-value, lowest-risk slice
lands first and every not-yet-supported form is a *named refusal*.

**Stage 0 — parse + store + drop (no firing).** `DdlStmt::CreateTrigger/DropTrigger`,
`parser/ddl.rs` grammar, the `trigger/<name>` record (§3.3) with truncation
tests, `apply_create/drop_trigger`, `schema_gen` bump, `DROP TABLE` cascade,
`RENAME` rewrite (or table-id storage). `load_trigger_catalog` gen-gated. Bodies
are stored but **nothing fires yet** — every DML runs as today. Milestone:
`CREATE TRIGGER` / `DROP TRIGGER` round-trip, survive reopen, replicate via
mirror. Ships COMPAT movement from ❌ to "parsed/stored, does not fire."

**Stage 1 — `AFTER INSERT FOR EACH ROW`, single-statement in-SQL body, no
`WHEN`.** The `NEW` binder scope (§3.4, `NEW` only), the `TriggerHost` +
`exec_stmt` thread-through (§4.2), firing after `ctx.insert_row` (§4.1),
depth cap (§4.4). Body limited to one `INSERT`/`UPDATE`/`DELETE`. This is the
canonical audit-log trigger (`AFTER INSERT ON t … INSERT INTO audit …`) and the
smallest end-to-end slice that proves the whole spine. Refuse `BEFORE`,
`UPDATE`/`DELETE` events, `WHEN`, multi-statement bodies, PySpell.

**Stage 2 — `AFTER UPDATE`/`AFTER DELETE`, `OLD` binding, `WHEN`,
`UPDATE OF cols`.** Full `OLD`/`NEW` table (§1), the `WHEN` predicate (§4.2, 3VL),
`UPDATE OF` change-detection. Still `AFTER`, still single-statement bodies.

**Stage 3 — `BEFORE` triggers + `RAISE`.** `BEFORE` fire ordering (§4.1),
`RAISE(ABORT|FAIL|IGNORE)` → error / skip-row (§4.3). `NEW` stays read-only
(refuse mutation). This unlocks validation triggers.

**Stage 4 — multi-statement bodies.** `BEGIN s1; s2; … END` executed in order on
the same ctx, with the depth cap covering the fan-out. Recursive-triggers knob
(§4.4).

**Stage 5 — PySpell bodies.** `EXECUTE PROCEDURE proc(args…)`, the `CtxBridge`
(§5.2), proc-arg binding (§5.1), define-time argc/kind checks. Coexistence tests
(both body kinds on one table, §5.3).

Adversarial review gate: per the verification-calibration standing rule, the
**exec-path change and the trigger record wire-format** get a full adversarial
review (they touch the commit path's atomicity and a new on-disk format); the
grammar/binder stages ride differential + SLT testing against sqlite with at most
one reviewer agent.

---

## 8. Format-impact summary (the explicit assessment asked for)

| Concern | Verdict |
|---|---|
| Schema **canonical bytes v2** (tables/cols/indexes/PK) | **No change.** Triggers are sys-keyspace records, like views/policies — outside canonical bytes entirely. |
| **`M_SCHEMA_HASH`** (frozen seed) | **No change.** Seed is forever; triggers are live catalog like `CREATE TABLE`'s additions. |
| **`schema_gen`** | **Used** (bumped on CREATE/DROP TRIGGER) — the coherence mechanism, no new protocol. |
| Ordinary-statement **`PLAN_FORMAT`** | **No change.** Triggering plans don't encode triggers; firing is an exec-time catalog lookup. |
| Trigger **body** plans | New reserved-slot kind (`NEW`/`OLD`), an additive plan-format extension scoped to body plans, validate-enforced — not a change to user statement plans. |
| **`PROC_FORMAT`** (`mpedb-proc`) | **No change.** A trigger references a proc by name/hash; the proc blob is unchanged. |
| **New** format | The `trigger/<name>` record (`MTRG`, `TRIGGER_FORMAT=1`), versioned, bounds-checked, `Corrupt`-never-panic, truncation-tested. |
| Executor signature | `exec_stmt` gains a `&dyn TriggerHost` (no-op when trigger-free); a new `CtxBridge` for PySpell dispatch in-txn. |
| Facade | `load_trigger_catalog` (gen-gated), `apply_create/drop_trigger`, `DROP TABLE`/`RENAME` cascade. |

Net: a large feature by **surface** (parser + binder + exec + proc bridge +
catalog), but **small by format risk** — one new versioned sys-record and one
additive body-plan slot kind, riding the view/policy DDL machinery and the
subplan/context reserved-slot machinery that already exist. No migration, no
backward-compat burden (standing rule): breaking `TRIGGER_FORMAT` later is a new
version, not a migration.

---

## 9. Open questions for review

1. **Trigger name scope** — global (sqlite) vs table-qualified. Recommendation:
   global unique names (sqlite-compatible), stored `trigger/<name>`.
2. **`recursive_triggers` default** — off (sqlite historical) vs on (Postgres).
   Recommendation: off, config-overridable, always depth-capped.
3. **Body-as-text vs body-as-plan-hashes** — §3.3 recommends text (view model);
   revisit if hostile-blob hardening of the sys-keyspace is tightened.
4. **`RAISE(ROLLBACK)`** semantics under an interactive `WriteSession` (vs
   autocommit) — refuse in v1, pin later.
5. **Interaction with RLS `WITH CHECK`** ordering: a `BEFORE INSERT` trigger runs
   after `WITH CHECK` in §4.1 — confirm this matches the intended
   policy-then-trigger precedence, or swap.
