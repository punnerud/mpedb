# DESIGN-MULTIDB.md — Parallel databases, per-database separation, and cooperative in-file RLS

> Companion to `DESIGN.md`. This document adds two features and one honesty rule. It changes **nothing** in the reviewed concurrency/commit protocol (`DESIGN.md` §4–§5): every mechanism here is a handle-layer or query-layer overlay. Read `DESIGN.md` before touching lock/commit/meta code; read **§6 of this file before selling any of it as a security boundary.**

---

## What this gives you

Two asks arrived together and they pull in opposite directions:

1. **Easy hard separation per database** — a real, OS-enforced wall so one team/tenant/service cannot touch another's bytes.
2. **Shared data across that wall with PostgreSQL-style row-level security (RLS).**

They are answered by **two orthogonal primitives**, because in a serverless, directly-`mmap`'d engine *no single mechanism can be both a hard wall and a shared surface*:

| | **Workspace (separate files)** | **In-file RLS (one file, one trust domain)** |
|---|---|---|
| Isolation kind | **Hard, OS-enforced** by filesystem permissions | **Cooperative**, library-enforced, defense-in-depth |
| Boundary against hostile/untrusted code | Yes (a process that cannot `open()` the file touches zero bytes) | **No** — any attached process reads/writes any page directly |
| Write parallelism | Independent writer locks → linear scaling across files | Shared writer lock (all writers in one file serialize) |
| Shared data / joins across the wall | No (separate catalogs) | Yes (same catalog, MVCC snapshot) |
| Cross-table atomic commit | No (independent commit protocols) | Yes (one writer lock, one meta flip) |

The headline, stated once up front because it shapes everything: **RLS that spans a hard-separation boundary is structurally impossible in this model.** RLS is enforced by *your library code on the SQL path*; a hard boundary means *a process that does not run your code still cannot see the bytes*. With no server mediating and `MAP_SHARED|PROT_WRITE` (`shm.rs:1700-1701`) handed to every opener, the only actor who can enforce a predicate is one already inside your trust domain. So "shared data with RLS" necessarily means **shared *within* one trust domain**, never *across* the hard wall.

In-file *namespaces* are **explicitly rejected** (see §5): they buy no id domain, no parallelism, and no isolation — cosmetic machinery over the same 56-table / `1<<(table_id&63)` footprint word. The scaling path past one file's table budget is *more files*, i.e. more Workspace members.

> ### ⚠️ TRUST-BOUNDARY HONESTY BOX (canonical; mirrored in `DESIGN.md` §7.3 lineage and in config docs)
>
> **There are exactly two isolation primitives in mpedb, and only one is a security boundary.**
>
> **(1) SEPARATE FILES are a HARD, OS-ENFORCED boundary.** Each Workspace member is its own `mmap`'d file *plus its `<path>-wal` companion*, with its own writer lock, reader table, and catalog. The boundary is filesystem permissions: a process that cannot `open()` the file (and its `-wal`) cannot read or write a byte. Set `mode = 0o600` (and `owner`/`group`) per member and this is real, kernel-enforced isolation, with independent writer locks as a free parallelism bonus. **The caveat that must never be hidden:** this boundary is only *real* if the deployment actually runs each database's processes under **distinct OS uid/gid** matching the file's owner/group/mode. In a single-service-account / one-container deployment where every process is the same OS principal, filesystem permissions grant that principal access to *every* member and the wall collapses to zero. mpedb cannot enforce or verify uid separation; the operator must provide it.
>
> **(2) IN-FILE ROW-LEVEL SECURITY is COOPERATIVE, defense-in-depth ONLY — NOT a security boundary.** Every process attaches `MAP_SHARED|PROT_WRITE` and can read or write ANY page directly — walk `catalog_root`, scan the B+tree, read the `<path>-wal`, or read deleted/RLS-hidden rows out of freed or high-water pages (COW leaves byte residue; there is **no scrub-on-free** — `shm.rs` zero-fills only on *alloc*) — **never invoking the binder or any policy.** The AND-folded policy predicate exists only on the library's SQL path; a patched build, a hand-crafted detached plan carrying the current `policy_hash`, or a direct page walk omits it entirely. In-file policies are **configuration, not a protected control plane**: any writer can `page_mut` the `pol/*` records to disable them. The session principal is **asserted by the caller and bound to no OS uid** — even a cooperating-but-buggy caller that passes the wrong `app.tenant` reads (and *writes*) another tenant's rows, with no hostility. And even on the sanctioned SQL path, RLS **does not hide the *existence* of rows** from PK/UNIQUE constraint errors (§6.4).
>
> **Rule of thumb, stated up front:**
> - Mutually-untrusted tenants · a compliance/security boundary · any code you did not write · one tenant that must not be able to DoS/corrupt others (a shared writer lock is *shared-fate*: one tenant dying under the robust mutex forces `EOWNERDEAD` recovery for all) → **SEPARATE FILES.**
> - One application's own trusted code preventing *its own bugs* from leaking rows between *its* users → **one file + RLS is fine.**
> - Need atomicity across two tables → they belong in **one file.** Need isolation between them → **two files.**
> - Want RLS enforced *across* a hard boundary → **not possible serverless**; you need a server process (out of scope) or acceptance that the sharing side is one cooperative trust domain.

---

## 1. Parallel databases & per-database separation — the `Workspace` handle

### 1.1 Model: nothing in the engine changes

Each logical database stays exactly what it is today: one TOML → one `.mpedb` file → one writer lock + one reader-slot table + one MVCC meta double-buffer + one `catalog_root` + its own 56-user-table budget + its own `u64` footprint domain. A **`Workspace` is a thin handle that owns N independent `Database` engines.** The 64-table / `&63` ceiling and the entire 37-finding commit/lock surface are never stressed because table ids stay *per file*.

Because members are separate files they have **separate writer locks → genuinely parallel writers at zero protocol cost.** This is the intended, honest way both to scale write throughput and to exceed 56 tables.

### 1.2 Config surface (pure superset, back-compat)

`config.rs` today is `RawConfig { database: RawDatabase }` (one file). Add a `[[database]]` array; a lone `[database]` remains a valid one-member workspace.

```toml
[[database]]
alias       = "billing"
path        = "/var/lib/app/billing.mpedb"
size_mb     = 256
durability  = "wal"
concurrency = "optimistic"
mode        = 0o600         # NEW — file permission (see §1.4); the ONLY hard boundary
owner       = "billing"     # NEW — optional; chown after create
group       = "app"         # NEW — optional
  [[database.table]]        # this member's own tables

[[database]]
alias   = "shared"
path    = "/var/lib/app/shared.mpedb"
mode    = 0o660
  [[database.table]]        # tables carrying RLS policies (§3)
```

`alias` is required and is the routing name. `mode`/`owner`/`group` are the load-bearing addition — file permissions are the *only* hard boundary and today are left to ambient umask.

### 1.3 API — sqlite-`ATTACH` ergonomics, `db.table` addressing

- `Workspace::open("workspace.toml") -> Workspace` — attaches every member.
- `Workspace::attach(alias, Config)` / `Workspace::detach(alias)` — runtime ATTACH/DETACH.
- `ws.db("billing") -> &Database` — per-member handle (own registry, own `schema_hash`, own parallel writer lock).
- `ws.prepare("SELECT * FROM billing.orders WHERE id=$1") -> WsPlan { alias, hash }`.
- `ws.execute(&WsPlan, &session, &params)` / `ws.query_ctx(&session, sql, &params)`.

**Routing is done at the tokenizer/AST layer, never by string surgery.** The router runs the existing tokenizer over the statement, locates the single table reference (the binder is single-table, so there is exactly one), reads its optional leading `alias.` qualifier from the **parsed** identifier — which correctly ignores the same token inside string literals, comments, or quoted identifiers — strips the qualifier, and dispatches the de-qualified statement to that member `Database`. If no qualifier is present, an optional workspace default alias (or an error) applies.

**The hot path stays per-member and hash-addressed.** `WsPlan` carries `{alias, hash}` so `ws.execute` knows the member without re-parsing; each member keeps its own content-hashed registry (`plan/<hash>`), and hash domains never need to be globally unique because the alias disambiguates.

**Cross-member JOIN is out of scope.** The binder binds one `&TableDef`; the router handles single-alias statements only. That is the honest scaling story: more files, not a wider footprint word.

### 1.4 File permissions — born-restrictive, TOCTOU-safe, `-wal` covered

Today both creation sites use bare `OpenOptions::create(true)` with no `.mode()` — the main file at **`shm.rs:1566`** and the WAL companion at **`shm.rs:944`** (the WAL is created lazily on the first wal-mode attach; it is a *second* site, easy to miss). Ambient umask leaves a latent world/group-readable window, and a naive "create then `fchmod`" is TOCTOU-racy.

**Rule: born-restrictive, then widen — never create-loose-then-restrict.**

1. Create with `OpenOptions::mode(0o600)` (via `std::os::unix::fs::OpenOptionsExt`) on the create path so the file is *born* owner-only. umask can only tighten this further, never loosen above 0600, so there is no readable instant and any concurrent `open()` in the window still cannot read it.
2. `fchmod` to the configured `mode` *afterward*. Widening 0600 → 0640/0660 has a worst-case window of 0600 (safe). `fchown` to `owner`/`group` if configured.
3. Apply the **identical** mode/owner to the **`<path>-wal` companion** at `shm.rs:944` — it holds full recent-commit page images and is a *first-class isolation asset*: a tenant with read on `-wal` reads recent rows directly, bypassing every engine control. Born-restrictive must land at **both** sites, not only `open()`.

A process that cannot `open()` the file (and its `-wal`) touches zero bytes. That is the whole hard-isolation mechanism.

### 1.5 Cross-file transactions — offered only as loudly non-atomic

There is **no atomic cross-file commit**, and none should be added: separate files have entirely independent meta/ring/WAL, and a shared commit protocol would destroy the independent-writer-lock parallelism that is the point. Offered:

- **Per-member atomicity** (each member's `WriteSession` commits atomically on its own engine), and
- an optional **`WorkspaceTxn::commit_sequential_nonatomic()`** that commits member txns sequentially, with non-atomicity loud in the name and docs.

**Honest failure envelope (corrected):** it is *not* a clean committed prefix. Members are separate files with independent meta/ring/WAL and independent `fsync`; there is no cross-file barrier at any durability level. **After a crash, an *arbitrary subset* of the member commits may survive — a later member may be durable while an earlier one is lost. There is no prefix or ordering guarantee.** Documented cliff: *if you need ACID across two tables they belong in ONE file; if you need isolation between them they belong in TWO files.*

---

## 2. Session context — the serverless "principal" (asserted, not authenticated)

There is no server to authenticate a principal, so the principal is a **caller-asserted context bag, authenticated against nothing.** That asymmetry is the honesty core, stated in the same breath the feature is introduced (§6).

```rust
let s = db.session();                 // or ws.session()
s.set("app.tenant", Value::I64(42));  // mirrors PG `SET app.tenant = 42`
```

`Session { ctx: BTreeMap<String, Value> }`. New context-taking entry points thread an **immutable context vector** down through `run_plan`/`run_write_plan` (`lib.rs:474`/`518`) into `exec_stmt` (`exec.rs:302`) alongside `params`:

- `db.execute_ctx(&Session, &hash, &params)`
- `db.query_ctx(&Session, sql, &params)`
- `db.begin_as(&Session) -> WriteSession`

Existing `execute`/`query`/`begin` become `Session::empty()` shims (full back-compat; public param count unchanged). The `ERRORCHECK`-relock rule is preserved: context threading takes no new lock, and read-only plans stay on the read path.

### 2.1 Context enters the plan as RESERVED PARAMS — never a new eval channel, never a const

`current_setting('app.tenant')` (and the declared bareword form, §3.4) binds to `BExpr::Param(n_user_params + ctx_pos)` with the context key's declared type. This is the standout mechanism: it reuses `PushParam` / `KeyPart::Param` / footprint key-refinement / exec binding **unchanged** — no new `expr.rs` opcode, no `KeyPart::Context`, no `footprint.rs` variant — and a tenant-keyed policy still yields a precise `KeyAccess::Point/Range` rather than degrading to `FullScan`.

`CompiledPlan.n_params` (`plan.rs:24`) **splits** into caller-facing `n_user_params` and engine-filled `n_context_params` (context slots appended *after* user params). At exec the Session's values are resolved against the plan's declared context slots and bound into the reserved tail; **exec refuses any caller-supplied value addressing the reserved tail** (the public API exposes only `n_user_params`), so a client cannot spoof a slot through the params array.

### 2.2 One ordered context-slot table per plan (bind-by-key, not by position)

A single plan can reference the same or different context keys from three places: the access-path `KeyPart::Param`, the residual filter, and the `WITH CHECK` program (§3). **All three must index into ONE ordered, deduped-by-key context-slot table produced by a single binder/slot allocator.** Exec fills each reserved slot **strictly from its recorded `(key, type)`** — resolution is **by declared key name, never by slot position** — so a plan whose `USING` and `WITH CHECK` reference different keys can never cross-bind one context value into another's slot. (Regression test: a plan with `USING (a = current_setting('app.x'))` and `WITH CHECK (b = current_setting('app.y'))` resolves each program's `Param` tail to the correct Session value.)

### 2.3 Missing context = HARD ERROR (fail closed, loudly)

If a plan references a context key the Session did not set, exec returns an error — `"session context 'app.tenant' required by policy on table orders is not set"`. It does **not** bind NULL and silently return zero rows. Silent-empty-set is a footgun that masks real misconfiguration; loud failure is specified.

### 2.4 Wrong TYPE = HARD ERROR (same class as missing)

If a Session value's type does not match the policy's declared `(key, type)` — e.g. `Value::Text` where the key is declared `I64`, or `Value::Null` — exec raises a hard error `"session context 'app.tenant' has type Text, policy requires I64"`. It must **never** silently coerce, and **never** bind `Null` (which would fold to `UNKNOWN` under 3VL and quietly return zero rows, masking a bug as "no data"). Type mismatch is treated identically to missing context. (Note: even without this check the engine's `sql_cmp` returns `TypeMismatch` rather than coercing, so the failure mode is a self-inflicted DoS, not a leak — but the explicit, well-worded error is required so the caller learns *why*.)

### 2.5 Lifecycle — the pooling-bleed footgun, specified

`Session` context mirrors PG `SET` semantics: **it persists** until overwritten or cleared. This is the classic `SET`-vs-`SET LOCAL` footgun in a connection-pool / thread-reuse deployment: if the app sets `app.tenant = A`, serves a request, then reuses the *same* `Session` for principal B but forgets (or partially fails) to overwrite the key, **B silently reads and writes A's rows with zero hostility** — exactly the "cooperating-but-buggy caller" this feature claims to guard.

Mandated mitigations:
- **Document SET-persistence and the bleed explicitly** in the API docs.
- Provide `Session::reset()` / `clear()` and **recommend a fresh `Session` (or `reset()`) per principal**, never a pooled long-lived bag.
- Provide a **txn-scoped `SET LOCAL` mode**: `begin_as(&Session)` snapshots the context at `begin` so a later mutation of the bag cannot bleed into an open txn, and a v2 `execute_local(...)` form that takes an ephemeral one-shot context. Prefer snapshot-at-`begin_as` as the default recommendation.

### 2.6 Multi-valued / membership context

The recommended spoof-resistant pattern (§3.6) is "resolve membership in the app, pass it through context." A *single-valued* tenant check (`tenant_id = current_setting('app.tenant')`) is fully supported in v1 and binds to one reserved slot.

A *variable-length* `org_id IN (<context list>)` **cannot** bind to a fixed reserved slot without either baking arity into the plan (per-session hash explosion, breaking §4.1) or an undefined encoding. Resolution:
- **v1:** single-valued equality only; the `org_id IN (...)` example is **not** claimed for v1.
- **v2:** introduce a single **array-typed context `Value`** bound to one reserved slot and a scalar **set-membership operator** (`col ∈ ctx_list`) in `expr.rs`, evaluated per-row against the one bound list Value. Arity lives in the *data*, not the plan bytes, so the plan hash stays context-independent and one plan still serves all sessions.

---

## 3. Row-level security — `CREATE POLICY`, transparent injection, invisible-rows-behave-as-absent

RLS is a pure query-layer overlay for tables that legitimately live in **one file, one trust domain**. No commit-path, meta-page, or footprint-type change.

### 3.1 Declaration — PostgreSQL-shaped

```sql
CREATE POLICY <name> ON <table>
  [AS { PERMISSIVE | RESTRICTIVE }]          -- default PERMISSIVE
  FOR { ALL | SELECT | INSERT | UPDATE | DELETE }
  USING (<visibility predicate>)             -- SELECT / UPDATE / DELETE
  [WITH CHECK (<write predicate>)];          -- INSERT / UPDATE
ALTER TABLE <t> ENABLE ROW LEVEL SECURITY;
ALTER TABLE <t> FORCE ROW LEVEL SECURITY;    -- optional; §6.5
DROP POLICY <name> ON <t>;
```

A config-file twin (`[[table.policy]]` with `name`/`for`/`as`/`using`/`check`/declared `context` name-type list) is accepted for file-authoritative declaration, so policies can be born with the file.

### 3.2 Storage — the sys-keyspace, **not** the schema bytes

Policies live under the existing reserved sys-keyspace (`SYS_PREFIX = 0x02`, `sys_put`/`sys_get`/`sys_scan`, `engine.rs`) inside the catalog tree rooted at `catalog_root` — the same place `plan/<hash>` already lives:

- `pol/<table_id BE>/<name>` → `{ cmd bits, permissive|restrictive, USING source text, WITH CHECK source text, declared context (key,type) list }`.
- `rls_enabled/<table_id BE>` → bool (from `ENABLE/FORCE ROW LEVEL SECURITY`).
- `pol_epoch/<table_id BE>` → `u64`, bumped on any policy commit **for that table** (per-table so churn is confined; §4.3).

**Store SOURCE, not precompiled `ExprProgram`.** The planner re-binds the policy source against *each statement's* param space at prepare time (cold path), so its context references get the correct reserved-param indices for that statement's `n_user_params` — avoiding an ExprProgram relocation pass.

**Why sys-keyspace and NOT `Schema::canonical_bytes`:** folding policy into `schema_hash` would make every policy edit register as file-authoritative *config drift*, and since attach hard-errors on drift, **every other attached process would fail to reopen on a `CREATE POLICY`.** The sys-keyspace decouples policy versioning from schema drift and gives **online** `CREATE/DROP POLICY`. (This is the fatal flaw that sinks the "policies in schema bytes" alternative — do not revive it.)

**Publication is automatic and snapshot-consistent.** A policy edit is an ordinary COW commit through the catalog tree: writer lock → `sys_put` the `pol/*` record(s) → bump `pol_epoch` → freelist fixpoint → **single meta flip.** Because both `CAT_SCHEMA_KEY` and `pol/*`+`pol_epoch` hang off `catalog_root`, that one meta flip **publishes {schema+policy} atomically.** A reader pinned on an older `MetaSnapshot` keeps the old `catalog_root` and therefore the old policy set — **snapshot isolation for policy is free, no new concurrency machinery, and the reviewed fence/publication ordering is untouched.**

### 3.3 Compilation & injection — one helper, AND-fold **before** `extract_access`

In the planner, a single helper builds the effective policy `BExpr` (§3.5) with the **same binder** used for CHECK constraints and folds it into the read path *before* access extraction (`plan_select` `planner.rs:63`, `plan_update` `:250`, `plan_delete` `:298`):

```
bound_where = binder.bind_predicate(user_where);
eff_policy  = apply_policy(&mut binder, table, cmd);          // (perm ∨ …) ∧ restr ∧ …
merged      = BExpr::Binary(And, bound_where, eff_policy);    // BEFORE extract_access
access      = extract_access(merged, table, consts);
```

`split_and`/`extract_access`/`rebuild_residual` (`planner.rs:385/398/505`) decompose the merged conjunction exactly like a user `AND`: a policy conjunct that pins the PK/unique column (e.g. via a context param) becomes a legitimate `KeyAccess::Point/Range`; otherwise it lands in the residual filter, which exec evaluates via `eval_filter` (`exec.rs:614`, fail-closed on NULL). No new decomposition logic. `rebuild_residual` keeps *every* unconsumed conjunct AND-ed, so a policy conjunct is never dropped even when a user predicate pins the same column first.

### 3.4 Context-name resolution (bind-by-key; collision rule)

Context references bind **by declared key name**, into the ordered per-plan slot table (§2.2). Two forms are accepted:
- `current_setting('app.tenant')` — always resolves to a context slot (canonical, unambiguous).
- a declared bareword (only for keys declared in the policy's `context` list).

**Collision precedence:** if a bareword context key collides with a table column name, the binder must **error at bind time** (`"ambiguous name 'x': matches both a column and a declared context key; use current_setting('x')"`) rather than silently pick one. Recommendation to policy authors: always use the `current_setting()` form.

### 3.5 Combination semantics (PostgreSQL-compatible; default-deny as a literal FALSE)

For a given command with RLS enabled:

```
effective = (perm1 ∨ perm2 ∨ …) ∧ restr1 ∧ restr2 ∧ …
```

Permissive policies OR-combine; restrictive policies AND-combine; the groups AND together.

**If RLS is enabled and no permissive policy applies to the command, the effective predicate is a literal `FALSE` (deny).** This is a *construction* requirement, not just a semantic note: `apply_policy` must **emit `BExpr::Const(Bool(false))`** when the applicable-permissive set is empty (including "RLS enabled, zero policies for this command"), **never `None`/omit**. Omitting it would make `merged = user_where AND (nothing) = user_where` and expose the whole table. `const-fold` of `user_where AND FALSE` collapses to `FALSE`, which `extract_access` must route as an all-rows-filtered residual (verify it is not dropped). Regression test: RLS enabled + no permissive policy for the command ⇒ zero rows.

### 3.6 Per-command semantics — invisible rows behave as ABSENT

- **SELECT:** `USING` AND-folded into WHERE; a row is visible iff `USING` is true. An invisible row is indistinguishable from a non-existent one *on the read path* (but see the constraint-error and timing caveats, §6.4/§6.7).
- **DELETE:** `USING` AND-folded into WHERE; only visible rows are deletable.
- **UPDATE:** `USING` restricts the target set (AND-folded into WHERE) **and** `WITH CHECK` gates the post-image. If `WITH CHECK` is omitted, `USING` is reused as the check (PG rule).
- **INSERT:** no WHERE, so `USING` does not apply; `WITH CHECK` is the sole gate on the new row. If `WITH CHECK` is omitted, the policy's `USING` is used as the check (PG rule).

**Read/write asymmetry — PG's "SELECT policies also apply to rows read by a write" rule (correctness fix).** An UPDATE/DELETE always reads the old row (`gather_rows`), and the affected-row count leaks. A table with a narrow `FOR SELECT USING(tenant=ctx)` plus a broad write policy (e.g. `FOR UPDATE USING(true)`) would otherwise let a caller mutate — and, via the count, *infer the existence of* — rows it can never SELECT. **Therefore: when planning UPDATE/DELETE and the statement reads columns (has a WHERE/residual, RETURNING, or a self-referential SET), AND-fold the applicable SELECT permissive/restrictive set into the target predicate *in addition to* the command's own `USING`.** This matches PostgreSQL and closes the read-via-write inference channel within the cooperative domain.

### 3.7 `WITH CHECK` NULL semantics — must REJECT on NULL (**not** "like a CHECK constraint")

This is a load-bearing correctness fix. The engine's existing CHECK-constraint evaluator (`engine.rs:505`) treats `Value::Bool(true) | Value::Null => {}` — i.e. **NULL passes** (SQL "violated only when FALSE"). `eval_filter` (`expr.rs:235-236`) does the opposite: `Value::Null => Ok(false)` — **NULL rejects.** The correct PG `WITH CHECK` rule is "row rejected unless the predicate is exactly TRUE," i.e. **`eval_filter` semantics.**

**`WITH CHECK` MUST compile to an `ExprProgram` evaluated with `eval_filter` (NULL and FALSE both reject), and MUST NOT be wired into the `validate_row` CHECK loop.** Strike the phrase "exactly like a CHECK constraint" wherever it appears in earlier drafts. Concrete leak this closes: `orders(tenant_id INT null)`, `FOR INSERT WITH CHECK (tenant_id = current_setting('app.tenant'))`; inserting `tenant_id = NULL` (or `SET app.tenant = Null`) makes `NULL = 42` → `NULL`; under the CHECK loop it would *pass* and write a forbidden row, and if any read policy uses `... OR tenant_id IS NULL` (public-row pattern) that row becomes visible to **every** tenant. Required tests: a NULL-valued `WITH CHECK` (both null column and null context) rejects the write, on INSERT and on UPDATE-to-NULL.

`WITH CHECK` is **footprint-neutral** (touches no additional table or key), but its context-param references must be counted in the plan's param total so `validate()`'s param-bounds check passes. It is carried as `with_check: Option<ExprProgram>` on `PlanStmt::Insert` and `PlanStmt::Update`.

### 3.8 Scope limits — single-table scalar (honest expressiveness cost)

**Phase 1 policies are single-table scalar.** They may reference only the row's own columns, session context, constants, and existing scalar ops/LIKE. **Subqueries / JOIN / EXISTS / cross-table references are rejected at bind time**, because `ExprProgram` is single-row scalar and the footprint describes exactly one table — a second-table read would silently under-claim `tables_read` and corrupt conflict grouping (§5).

This is a *real* limit, and it must not be oversold as "full PostgreSQL RLS." Classic PG RLS often does a membership/role lookup against a second table. **The idiomatic mpedb pattern — and the recommended one — is to resolve membership in the application and pass it through session context:** the app computes "which tenant/org/roles does this caller have" once and `SET`s it (`app.tenant`, or a v2 list value, §2.6); the policy is then a single-table predicate like `tenant_id = current_setting('app.tenant')`. This keeps policies single-table and the footprint precise while covering the overwhelmingly common multi-tenant case. Controlled second-table lookups are out of scope Phase 1 precisely because doing them wrong reopens the reviewed commit surface.

---

## 4. Plan-cache leak-proofing (holds across processes AND pinned snapshots)

Plans are content-addressed and shared via `plan/<hash>` (registry, `MAX_REGISTRY_PLANS = 4096`) plus a local `HashMap<PlanHash>`. Cross-**session** row leakage and cross-**policy-version** leakage are each closed, but the second requires a live check the earlier drafts omitted. **Scoped claim:** everything in this section is leak-proof *on the cooperative library SQL path, within one trust domain*; it is not, and cannot be, a boundary against a process that walks raw pages (§6). Do not quote "leak-proof" out of that scope.

### 4.1 Session context is never baked, never hashed (verified sound)

`current_setting()` compiles to a **reserved param**, resolved from context at eval time — never a const. This is safe against const-folding because `fold()` (`binder.rs:273-288`) only folds when all operands are `Const` and returns the expression unchanged for `BExpr::Param`. So one compiled plan filters differently per session; **no tenant value ever enters the shared plan bytes or the `plan/<hash>` key**, there is no per-tenant blake3 explosion, and one content-hashed plan is safely shared by all sessions.

### 4.2 Policy version in the plan hash — with a LIVE, snapshot-scoped check (the core fix)

`CompiledPlan` gains **`policy_hash: [u8;32]`** = `blake3(deterministic serialization of the plan's target-table applicable policy set ‖ pol_epoch[table])`, mixed into `hash()`:

```
plan.hash() = blake3(canonical bytes ‖ schema_hash ‖ policy_hash ‖ FORMAT_VERSION)
```

Storing and round-tripping `policy_hash` through `encode`/`decode` **is not sufficient by itself** — and this is the hole earlier drafts missed. `plan.hash()` recomputes purely from encoded stored fields, and the registry loader only asserts `plan.hash() == requested_hash`, so an internally-consistent plan whose embedded `policy_hash` reflects epoch *e0* hashes back to its own *e0* hash and **passes regardless of the live `pol_epoch`.** `decode(bytes, &Schema)` compares only `schema_hash` against the *process-fixed* attach-time `Schema` (`plan.rs:238`) — it has no snapshot and no `pol_epoch`. `schema_hash` validation works precisely *because* schema is attach-fixed and hard-errors on drift; `policy_hash` is the **opposite** — online-mutable, living under `catalog_root`. So a stored/self-referential `policy_hash` is never actually checked against live state.

**The fix — a NEW validation step, distinct from `decode()`, threaded with the executing snapshot:**

1. On **every** execute — both the local cache-hit path and the registry-load path (`cached_or_load` `lib.rs:451`, `plan_by_hash` `lib.rs:705`, and `execute_detached` `lib.rs:303`) — call `validate_policy_epoch(plan, &read_txn)` **after** the plan is obtained and **under the same pin that will scan the rows.**
2. Inside it, read the current `pol_epoch[table]` from the **executing pinned snapshot** (`sys_get`). Fast path: if it equals the `pol_epoch` the plan recorded, the plan is valid (one point-get, no scan). Else recompute `policy_hash` from the live `pol/*` source for the plan's table under the same pin; if it matches the plan's embedded `policy_hash`, refresh the in-process `pol_epoch` marker and proceed (an edit to a *different* table bumped nothing here; an edit that produced identical policy bytes is a genuine match); otherwise **evict the local-cache entry and return `PlanInvalidated`** so the caller re-prepares.

`policy_hash` is thus **validated against live snapshot state, never merely round-tripped like `schema_hash`.** Fail-closed in both directions: a reader on an old snapshot uses old policies *and* old plans (consistent); when it advances its snapshot it sees the new epoch, its stale plans fail the compare, and it re-prepares.

### 4.3 One pin for validation AND execution (close the two-snapshot TOCTOU)

Today `cached_or_load` opens `begin_read()` (`lib.rs:457`), reads the registry, then **`finish()`es that snapshot**, and `run_plan` opens a *separate* `begin_read()` (`lib.rs:493`) for the scan. A policy edit committing in that window would let a plan validated at *e0* execute against an *e1* snapshot. **Required refactor:** fold the registry read, the `pol_epoch`/`pol/*` read, the `validate_policy_epoch` check, and the row scan into a **single `begin_read()` pin** (or the `WriteSession`'s own txn for DML). Compute the expected `policy_hash` from the *same* snapshot that scans the rows, immediately before `exec_stmt`. `validate_policy_epoch` must **not** reuse `self.schema()` (process-fixed) for the policy check; it takes the `ReadTxn`.

### 4.4 Detached plans — accidental staleness closed cheaply; hostile crafting explicitly *not* closed

- **Accidental staleness IS closed, cheaply.** `execute_detached` recomputes the expected `policy_hash` (and `schema_hash`) from **its own execution pin** and compares against the blob's embedded values — a 32-byte compare, **no re-parse** (preserving the zero-parse reason detached plans exist). A client that cached a pre-RLS or old-policy blob is rejected with `PlanInvalidated`. `execute_detached` routes the **identical context path** (reserved-param tail, refusal of caller-supplied context values, §2.1) as `execute_ctx`, so a detached plan cannot bypass context handling.
- **A maliciously hand-crafted, self-consistent, policy-omitting blob carrying the *current* `policy_hash` is NOT closed — and does not need to be.** Such a client already runs code you did not write and could equally walk `catalog_root` and read raw pages; it is the **same trust class as direct page access** (§6). If you want the *engine*, not the client, to control policy injection, use the **registry path** (`prepare`/`execute(hash)`): there the plan bytes are produced by the trusted engine's own compilation and the client only supplies a hash to look up.

### 4.5 Registry churn (bounded, per-table)

Per-table `pol_epoch` confines invalidation to plans on the *edited* table (a `CREATE POLICY` on `orders` does not re-mint every plan on every table). Old-epoch `plan/<hash>` records orphan and age out passively via the existing 4096-cap eviction; optionally a targeted sweep of `plan/*` entries whose embedded `policy_hash` no longer matches any live table policy set may run on policy-edit commits. Policy edits are DDL-frequency, so this is a monitored cost, not a hot-path concern.

---

## 5. Footprint & concurrency correctness (the reviewed protocol is untouched)

RLS and Workspaces are query-layer / handle-layer overlays. By construction:

- **No meta-page field.** Policies ride `catalog_root`; the single existing meta flip already publishes {schema+policy} atomically. The `M_CHECKSUM`-covered 0..96 body is unchanged.
- **No footprint-type widening, no `&63` change, no opt-ring change.** All tables stay in the one `u64` domain, ids stay <64 per file. **AND-before-`extract_access` guarantees footprints only *narrow*:** AND-ing a policy conjunct can turn `FullScan → Range/Point` or land in the residual, but can **never widen** the access path, so `compute_footprint` re-derived from the merged `PlanStmt` can never *under-claim* `tables_read/tables_written`. This is the single invariant that keeps pre-computed locks and opt-ring conflict-grouping sound. Verified against the real code: (a) `tables_written` and the delete/insert/update index bitmaps are set *unconditionally from the statement shape*, never from the predicate; (b) `split_and` only decomposes top-level ANDs, so a policy AND-combined with the user WHERE can only add pins; (c) `IndexPoint` conservatively reports `KeyAccess::Full` (`planner.rs:531`); (d) `plan_update` rejects PK-column SET (`planner.rs:267`), so a Point/Range PK claim stays valid across the write. `WITH CHECK` is footprint-neutral.
- **No fence/publication-ordering change.** The SeqCst reader-pin/writer-scan pair, the `fence(Release)→body→checksum(Release)` meta publication, intent-ring incarnation-safety, and the oldest-pinned reuse bound are all as reviewed.
- **Policy edits are ordinary COW commits** (writer lock → `sys_put` → freelist fixpoint → meta flip). No new commit machinery.
- **Read-only plans stay on the read path.** Applying a `USING` predicate to a SELECT never forces a write txn or the writer lock; a cold-cache SELECT still cannot block behind a writer.
- **In-file namespaces are rejected.** They would share this one 56-table / `u64`-footprint / single-writer-lock domain — buying no id space, no parallelism, no isolation. Per-namespace 64-table domains would require widening `Footprint` beyond `u64` and touching `opt_record`/`written_tables`/conflict grouping — exactly the reviewed 37-finding surface. **Scale with more files (Workspace members), not namespaces.**
- **Forward invariant for aggregates.** mpedb-sql has no aggregates today. When they are added, any aggregation **must** consume rows only *after* the merged `(WHERE ∧ effective-policy)` predicate — including residual-filter conjuncts — has excluded hidden rows. An aggregate reading the pre-filter tuple stream (a natural mistake, since some policy conjuncts land in the residual) would count/sum hidden rows and leak. State this at the injection point so aggregates are understood to bind here too.

---

## 6. Security limitations & when to use files vs RLS (adversarial findings folded in)

The Honesty Box up top is the canonical statement. This section catalogs the specific leaks a well-behaved developer must know about. **Info-severity page-bypass classes** (direct `page()`/B+tree walk, `<path>-wal` read in the shared case, freed/high-water residue, hostile writer `page_mut`ing `pol/*`) are all inherent to `MAP_SHARED|PROT_WRITE` and are covered by the Box; they have no fix and are the reason "in-file RLS is not a boundary." Below are the ones that bite even *cooperative* code, or that need a stated construction.

### 6.1 The asserted principal is a WRITE/integrity vector, not just a read leak

A caller that sets a wrong/spoofed `app.tenant` does not merely *read* another tenant's rows. With `app.tenant = victim` it can **INSERT rows attributed to the victim** (`WITH CHECK` passes), and **UPDATE/DELETE the victim's rows** (`USING` restricts the target set to the spoofed tenant). Scope your threat model to **data poisoning and destructive writes**, not just read disclosure.

### 6.2 Context must come from a server-verified identity — never client input

mpedb treats every context value as ground truth and **cannot distinguish a server-verified identity from attacker-controlled input.** A developer who does `s.set("app.tenant", request.header("X-Tenant-Id"))` or reads an *unverified* JWT claim has a full authorization bypass that *reads to them as "using RLS correctly."* **Context MUST be derived from a server-side-verified authenticated session or a cryptographically verified claim.**

### 6.3 Missing `ENABLE ROW LEVEL SECURITY` = silent full exposure

A table without `ENABLE RLS` (or RLS-enabled with no policy for a given command) performs **zero** filtering and exposes all rows to every context — PG-compatible, but the whole point (accidental-leak prevention) is defeated by one forgotten DDL line, silently, and no context value can trip it. Mitigation: `FORCE ROW LEVEL SECURITY` and a **file-authoritative "require policy" assertion** — a config flag that makes `prepare` **fail closed** if a table declared as tenant-scoped lacks an applicable policy for the command being compiled.

### 6.4 UNIQUE/PK constraint violations disclose hidden rows (existence oracle)

The uniqueness pre-checks run over the **entire** B+tree with no RLS awareness (`insert_row` `engine.rs:966-986`, `update_by_pk` `:1064-1074`), and `WITH CHECK` runs *before* them (`:957`). So a caller inserting a row valid under its own policy but whose PK/unique column collides with a *hidden* row gets `PrimaryKeyViolation`/`UniqueViolation` **iff a hidden colliding row exists** — a boolean existence oracle (and the classic PostgreSQL RLS caveat). This **cannot be closed while a single global unique domain is preserved.**

**Prescribed mitigation (by construction):** make the policy discriminator (e.g. `tenant_id`) a **leading part of every UNIQUE/PRIMARY KEY** on an RLS table, so a collision can only occur *within the caller's own visible partition* and the violation is non-leaking. Policy-authoring docs must pair §3.5 combination semantics with "unique keys that span the policy column leak; put the tenant column first." State plainly in DESIGN that PK/UNIQUE are enforced over all rows regardless of visibility.

### 6.5 Write-failure error taxonomy is a classification oracle

Distinct variants — `CheckViolation` vs `PrimaryKeyViolation` vs `UniqueViolation{constraint: <colname>}` vs success — let a probe learn not just *that* a hidden row exists but *which unique attribute* matches a probed value (the error even names the column), enabling attribute-by-attribute reconstruction. **When RLS is enabled on a table, normalize write-path constraint errors to a single indistinguishable failure** (no variant, no column name), *or* document loudly that the taxonomy is an oracle. Combined with tenant-leading keys (§6.4) the classification becomes harmless; without either, it must be called out.

### 6.6 Policy source in error text

`CheckViolation` today carries the constraint **source text** (`Error::CheckViolation{expr}`). Policy (`USING`/`WITH CHECK`) violations **must not** reuse that echo — emit a policy-specific error that names the *policy* but **not its predicate text** (which may embed thresholds/allow-list constants), or explicitly decide and document that policy source is non-secret.

### 6.7 RLS filters POST-fetch (timing + in-process materialization)

For a PK-point / unique-index-point query the policy conjunct lands in the residual filter, so the engine **fetches the row by key and then discards it**. This gives a timing distinction between "key absent" (B+tree miss) and "key present but hidden" (hit + decode + filter reject), and means **hidden-row bytes are decoded into the querying process's address space before being dropped** — an incidental log/panic/debug-dump leaks a hidden row even for non-hostile code. Acknowledged in the Box: in-file RLS provides no timing indistinguishability and hidden rows *do* transit the reader's memory. No code fix is mandated by the cooperative model, but the design must not imply hidden rows never reach the process.

### 6.8 Permissive OR weakens stricter write gates (PG-compatible, must be documented)

The effective write gate is `(perm_check1 ∨ perm_check2 ∨ …) ∧ restr…`. Any single permissive check being TRUE admits the write; adding a broad permissive policy — especially a `FOR ALL` policy whose omitted `WITH CHECK` reuses a lax `USING` (e.g. `USING(is_public=true)`) — silently opens writes a stricter tenant policy would reject. This is PG behavior, but policy-authoring docs must call it out and **recommend `RESTRICTIVE` for tenant-pinning write gates.**

---

## 7. Phased implementation plan

Six phases, each independently shippable and testable. **Phases 1–2 touch nothing in the concurrency/commit path.** Phases 3–6 are query-layer/handle-layer only; the sole commit-path interaction is that a `CREATE/DROP POLICY` rides the existing writer-lock → `sys_put` → freelist-fixpoint → meta-flip path **as an ordinary COW commit** — it does not modify the protocol.

**Wire-format flag-day:** Phases 4 and 5 both change the plan wire format (`with_check`, the `n_params → {n_user_params, n_context_params}` split, and `policy_hash`). They **share a single `FORMAT_VERSION` bump and must be released together.** An old-format or stale plan then fails safely (`PlanInvalidated` → re-prepare), composing with the existing "attach hard-errors on geometry/schema drift" behavior. Development can proceed in two phases; the release is one flag-day.

### Phase 1 — Born-restrictive file permissions (v1, independent, no protocol touch)
- `crates/mpedb-core/src/shm.rs`: `OpenOptions::mode(0o600)` on both create sites (main file ~`:1566`, WAL ~`:944`) via `OpenOptionsExt`; `fchmod`/`fchown` to configured `mode`/`owner`/`group` after create; apply identically to the `-wal` companion.
- `crates/mpedb-types/src/config.rs`: optional `mode`/`owner`/`group` per database.
- Tests: created file is never group/world-readable at any instant; `-wal` inherits mode; widen-only ordering.
- *Ships a real security fix for single-file deployments today; no dependency on later phases.*

### Phase 2 — `Workspace` multi-file handle (v1, independent, no protocol touch)
- `crates/mpedb-types/src/config.rs`: `[[database]]` array with required `alias`; lone `[database]` = one-member workspace; produce `Workspace { members: Vec<(String, Config)> }`.
- `crates/mpedb/src/lib.rs`: `Workspace` handle — `open`/`attach`/`detach`/`db(alias)`; AST-level `alias.table` router over the existing tokenizer; `WsPlan { alias, hash }` hot path; `ws.prepare`/`execute`/`query_ctx`.
- `crates/mpedb-sql/src/{token,ast}.rs`: expose the parsed leading-qualifier so the router reads it from the AST, not the raw string.
- `crates/mpedb-cli`: workspace-aware `repl`/`exec`.
- Tests: routing ignores `alias.` inside string literals/comments; per-member parallel writers; partial-attach policy.

### Phase 3 — Session context plumbing (v1, no protocol touch; no policies yet)
- `crates/mpedb/src/lib.rs`: `Session { ctx: BTreeMap<String,Value> }`, `db.session()`, `set`/`reset`/`clear`; `execute_ctx`/`query_ctx`/`begin_as`; `execute`/`query`/`begin` become `Session::empty()` shims; snapshot-at-`begin_as` (`SET LOCAL`) mode.
- `crates/mpedb-sql/src/{binder,planner,plan}.rs`: `current_setting('k')` and declared-bareword binding to reserved `BExpr::Param(n_user_params + ctx_pos)`; split `n_params → {n_user_params, n_context_params}`; one ordered, dedup-by-key context-slot table; bind-by-key resolution; column/bareword-collision error.
- `crates/mpedb/src/exec.rs`: thread the resolved context vector into `exec_stmt`; fill only the reserved tail; **refuse** caller-supplied values there; **hard error** on missing key (§2.3) and on type mismatch (§2.4).
- Tests: cross-program slot correctness (`USING` vs `WITH CHECK` different keys); missing/wrong-type errors; caller cannot address the reserved tail; pooling-bleed doc example.

### Phase 4 — RLS policies (v1; shares the FORMAT_VERSION flag-day with Phase 5)
- `crates/mpedb-sql/src/{token,parser,ast}.rs`: `CREATE POLICY … AS PERMISSIVE|RESTRICTIVE FOR … USING … WITH CHECK …`, `ALTER TABLE … ENABLE|FORCE ROW LEVEL SECURITY`, `DROP POLICY`.
- `crates/mpedb-core/src/engine.rs`: sys-keyspace storage `pol/<table_id>/<name>`, `rls_enabled/<table_id>`, per-table `pol_epoch/<table_id>`; policy edits as ordinary COW commits bumping the table's epoch.
- `crates/mpedb-sql/src/planner.rs`: `apply_policy(binder, table, cmd)` building `(perm ∨ …) ∧ restr` with **literal `FALSE` on empty permissive set**; AND-fold **before** `extract_access`; UPDATE/DELETE also fold the SELECT policy when they read columns (§3.6).
- `crates/mpedb-sql/src/plan.rs`: `with_check: Option<ExprProgram>` on `Insert`/`Update`, evaluated with **`eval_filter` (NULL rejects)** — **not** the CHECK loop; encode/decode/`validate`/`explain`; single `FORMAT_VERSION` bump (with Phase 5).
- `crates/mpedb-types/src/config.rs`: `[[table.policy]]` twin with declared context `(key,type)` list; optional "require policy" assertion (§6.3).
- Tests: default-deny ⇒ zero rows; NULL `WITH CHECK` rejects (insert + update-to-NULL); permissive OR / restrictive AND; read/write-asymmetry inference closed; subquery/cross-table policy rejected at bind; footprint round-trips (policy-narrowed Point and policy-as-residual).

### Phase 5 — Plan-cache leak-proofing (v1; shares the flag-day with Phase 4)
- `crates/mpedb-sql/src/plan.rs`: `policy_hash: [u8;32]` field, mixed into `hash()`, round-tripped in encode/decode.
- `crates/mpedb/src/lib.rs` + `crates/mpedb/src/registry.rs`: `validate_policy_epoch(plan, &read_txn)` called on **every** execute (cache-hit, registry-load, detached) **under the single execution pin**; fold registry read + `pol_epoch`/`pol/*` read + scan into one `begin_read()`/write txn (fixes the two-snapshot TOCTOU); evict + `PlanInvalidated` on mismatch; detached path does the 32-byte compare and routes context.
- `crates/mpedb/src/exec.rs`: (optional, with §6.5) normalize constraint-error variants when RLS is enabled.
- Tests: process-B policy edit ⇒ process-A cache-hit re-prepares; old-snapshot reader keeps old policy+old plan; detached stale blob rejected; hostile-blob-with-current-hash *documented* as not closed.

### Phase 6 — Hardening & completeness (v2; no protocol touch)
- `crates/mpedb/src/lib.rs`: `WorkspaceTxn::commit_sequential_nonatomic()` with the corrected "arbitrary subset, no prefix/ordering guarantee" docs (§1.5).
- `crates/mpedb-types/src/expr.rs` + planner: array-typed context `Value` + scalar **set-membership** op for `col ∈ ctx_list` (§2.6) so IN-list policies work without per-session plan explosion.
- Tenant-leading-key **lint** at `CREATE POLICY` time (warn when a UNIQUE/PK does not lead with the policy discriminator, §6.4); constraint-error normalization + policy-named (non-source-echoing) policy-violation error (§6.5/§6.6).
- `FORCE ROW LEVEL SECURITY` + "require policy" fail-closed enforcement (§6.3).
- Optional stale `plan/*` GC sweep on policy-edit commits (§4.5).
- Tests: sequential-workspace-txn crash leaves arbitrary subset; IN-list policy one-plan-serves-all-sessions; lint fires on tenant-spanning unique key.