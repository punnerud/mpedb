# DESIGN-SERVICE — mpedb as a hibernating service: queues, dynamic cron, webhooks, task runner

**Status: design (2026-07-18). Forward-looking (Phase 6+), sequenced AFTER the SQL-parity sprint.
Built almost entirely on primitives that already exist — triggers, PySpell procs, the multi-process
writer-lock + MVCC, WAL, workspaces. The service is a *coordination convenience on top of* the
embedded model, never a gatekeeper: other processes still attach the `.mpedb` directly (CLAUDE.md's
no-server contract is preserved). Pairs with [DESIGN-SYNC-TIERING.md](DESIGN-SYNC-TIERING.md).**

## 0. The thesis

A queue and a cron schedule are just *tables*. What makes a DB-backed task runner hard is
multi-process-safe claiming and crash-safety — and mpedb already has both (PostgreSQL-grade
concurrency, SIGKILL-safe commit path). So mpedb can be **pg-boss / River / pg_cron, but embedded and
serverless**: many unrelated OS processes enqueue, claim, and schedule work through one `.mpedb` file
with transactional guarantees, no server to run. And the wake-ups + routing can come **entirely from
the OS** (cron/systemd) and **nginx** — so the *default* needs no resident process at all (§1, model
A); a long-lived daemon is only a throughput optimization on top.

## 1. Two ways to run it — and the serverless one is the default

**Model A — serverless / OS-integrated (default; the truest "dvale": zero resident process).** Lean
on infrastructure that already solves wake/route/hibernate; mpedb implements none of it:
- *Scheduled work*: the DB `cron` table is authoritative, and `mpedb cron sync` **projects it onto
  the OS scheduler** — writing exact-time crontab lines / systemd timers / `at` jobs so a task due at
  14:32:05 fires precisely then, not on a poll tick. The OS timer invokes `mpedb run-due <db>`, which
  attaches, claims + runs due tasks, and exits. Nothing resident between fires. Any process edits the
  table; a tiny reconcile (itself timer-driven, or rung by the edit) rewrites the OS units — DB is
  the source of truth, the OS scheduler is just the executor.
  - Exact-time projection is the *tidy* form; a plain frequent `* * * * * mpedb run-due` also works
    fine and is safe by construction — the "is anything due?" check is a **lock-free MVCC read** (N
    idle pollers create zero writer contention; only an actual claim takes the writer lock), attach
    is sub-millisecond, overlapping invocations claim disjoint tasks atomically (§2), and reader-table
    liveness reaps dead claims while leaving a live long-running task's claim alone (so a run that
    outlasts the next tick never double-fires). We project to exact times only to skip the tiny
    "woke, nothing to do" wake — never for correctness or for overhead.
- *On-demand API*: **nginx** routes to mpedb via **systemd socket-activation** or **FastCGI**
  (`fastcgi_pass`). The socket exists; the first request spawns a short-lived mpedb responder that
  serves and exits after idle. That *is* "expose an API that wakes on request and goes back to
  sleep" — the OS + nginx are the doorbell and the hibernation.
- *Streaming ingest — zero-buffer writes* (the primitive already exists): a large POST body streams
  **straight into the DB** as it arrives — the responder pipes nginx's body chunks into the
  incremental blob API (#43, the `sqlite3_blob_open`-form, built precisely because the memory ceiling
  is the strongest reason) instead of buffering the whole upload. **Constant memory regardless of
  size** (#42's scatter-gather keeps even the commit from materializing it), natural TCP backpressure
  if the disk is slow (you never OOM on a huge upload), on the fast blob-write path (#40/#50, ~GiB/s
  warm). The HTTP **200 is sent only after the txn durably commits** (group-commit fsync, `durability
  = commit|wal`) — so an ack is a durability guarantee, stronger than app-memory-buffered stacks. The
  ack waits on *local* durability; async replication (DESIGN-DISTRIBUTED §2a) fans it out after.
- *State lives in the file, not in memory*: the queue, the schedule, and the callout token-bucket are
  all tables, so short-lived processes read/CAS them transactionally. The rate budget works **better**
  here than in a daemon — there is no resident memory to lose it in; it is durable and multi-process
  by construction.
- This is the CGI/inetd/serverless model, and it fits the no-server contract most purely: genuinely
  no server, just a file + processes the OS spawns on a timer or a request. Many spawned workers
  attaching one file is exactly mpedb's design point (writer-lock + MVCC) — safe by construction, no
  new race.

**Model B — resident daemon `mpedb serve <db>` (optional; a latency optimization).** Attach is cheap
by design (mmap + flock + meta read, no server handshake) and the due-check is a lock-free read, so
frequent cron/spawn is fine on its own — the daemon earns its keep only when you need **sub-tick
latency** (cron's floor is ~1 min): a long-lived process holds the attach warm and adds an in-process
**doorbell** — a futex/eventfd-style counter in the shm lock area (pages 2–3, CLAUDE.md) another
process bumps on enqueue — so an enqueue runs within milliseconds without a round-trip through nginx. It blocks on the earliest of an inbound request, the next due task, or the
doorbell; multi-process identity reuses the existing {pid,seq}+start-time + boot-id recovery
(`post_attach`). Same tables, same semantics — it just holds the attach warm and skips cold starts.
**Correctness never depends on it**: everything it does is a normal transactional mutation any
process could do; kill it and Model A still runs.

**Choosing**: almost everything → A (no daemon) — frequent cron/spawn is cheap (sub-ms attach) and
safe (lock-free due-check, atomic claim, liveness reaping). Reach for B *only* when you need sub-tick
enqueue→run latency, below cron's ~1-min floor. They compose — A for scheduled work, B for a hot
low-latency API path.
The API request protocol (`submit`/`enqueue`/`query`/`wake`) is identical whether it arrives over
FastCGI (A) or the daemon's socket (B); HTTP is a thin adapter either way.

## 2. Queues + task runner

- A queue is a table: `(id, queue, payload, state, priority, run_at, attempts, max_attempts,
  claimed_by, claimed_at, result, error)`. `state ∈ {pending, claimed, done, failed, dead}`.
- **Claim** is where the concurrency model pays off. mpedb serializes writers (writer-lock + group
  commit), so the classic Postgres `FOR UPDATE SKIP LOCKED` race does not exist — a claim is
  `UPDATE q SET state='claimed', claimed_by=:me WHERE id = (SELECT id FROM q WHERE state='pending'
  AND run_at<=now ORDER BY priority,id LIMIT 1) RETURNING *`, atomic by construction. Workers are any
  process (or the daemon's worker pool). A dead worker's stale `claimed` rows are reaped by a
  built-in cron task (claim lease = `claimed_at + visibility_timeout`); reader-table liveness (is the
  `claimed_by` pid still alive?) makes reaping precise, not just timeout-based.
- **Retry/backoff/dead-letter**: `attempts`/`max_attempts` + `run_at` reschedule on failure; exceed
  → `dead`. Standard, and every transition is one txn.
- The runner executes a **PySpell proc** (the task body) — this is the existing proc machinery, not
  new execution semantics. `enqueue → claim → run proc → write result → done`.

## 3. Dynamic cron — better than crontab because it is a table

- A `cron` table: `(name, schedule, proc, args, next_run, last_run, enabled, singleton)`. Model A
  projects `next_run` onto the OS scheduler (§1); Model B keeps an in-process min-heap and sleeps
  until the earliest. Either way, firing enqueues a task (§2) rather
  than running inline, so cron and queue share one execution path.
- **Dynamic, multi-process, transactional**: any process adds/removes/toggles cron rows via CLI
  (`mpedb cron add <name> "<sched>" <proc>`, `cron rm`, `cron list`, `cron pause`) or plain SQL —
  atomic, queryable, versioned, with history. That is the "better than crontab" claim: crontab is a
  per-user flat file with no concurrency story, no audit, no query; this is a shared transactional
  schedule many processes edit safely, and the daemon picks up changes on the next wake (the CLI edit
  rings the doorbell so schedule changes take effect immediately).
- `singleton` prevents overlapping runs of the same job across processes (a claim on the cron row).

## 4. Webhooks + callout limits (`.mpedb` config)

- A **webhook** is an outbound callout: a proc/trigger produces a result and hands it onward — "process
  A submits, a PySpell trigger passes the result to process B or an HTTP endpoint." Modeled as an
  outbound-queue kind: the trigger/proc `enqueue`s a callout row `(url|target, method, body, headers,
  attempts)`, and the daemon's callout worker delivers it (with retry/backoff, §2).
- **Callout limits live in the `.mpedb` config** (the TOML Config, extended with a `[callout]`
  section): `max_per_sec`, `max_concurrent`, per-endpoint budgets, `timeout`. The daemon enforces a
  token-bucket per endpoint before delivering — so a runaway trigger cannot melt a downstream service,
  and the budget is *durable config that travels with the file*. Over-budget callouts queue (bounded)
  and shed with a logged `callout_dropped` event past the high-water. This is the deterministic-cap
  ethos (count/rate budgets, not hope) applied to egress, and it composes with #74's runtime budget
  (ingress compute) — two sides of the same "bounded by declared limits" principle.
- Delivery target can be another `.mpedb` (via the daemon's socket API) or the mirror — the bridge to
  §5 and the sync doc: logs/results stream to a remote file instead of an HTTP endpoint.

## 5. Log tables + retention

- Log/audit/event tables are ordinary tables written by triggers/procs. **Retention** is just a cron
  task: `DELETE FROM log WHERE ts < now - :ttl` on a schedule — but two richer modes exist:
  1. **Drain-then-delete**: before deleting, a callout/mirror streams the stale rows to a *remote*
     `.mpedb` (a webhook target, §4), so history is preserved off-box while the hot file stays small.
  2. **Tiering**: hand old rows to [DESIGN-SYNC-TIERING.md](DESIGN-SYNC-TIERING.md)'s cold store with
     transparent read-back, instead of hard-deleting.
- The freelist/high-water discipline (CLAUDE.md §4.5) means reclaimed log pages are genuinely reused
  — a churning log table under retention does not leak space (there is already a regression test for
  the bound; a retention workload is a natural stress case to add).

## 6. Prior art (keep us honest)

- Queues: pg-boss, River, Que, Sidekiq, Celery — all need a server (PG/Redis). mpedb's differentiator
  is *embedded + multi-process + crash-safe*, no broker. Reuse their hard-won semantics (visibility
  timeout, backoff, dead-letter, singleton) — do not reinvent them.
- Cron: pg_cron (needs PG), systemd timers, dkron. The table-as-schedule + dynamic multi-process edit
  is the pg_cron idea without the server.
- Rate limiting egress: token bucket (envoy/nginx). Durable-config token buckets are the twist.
- Caveat from the mirror work: interactively-authenticated targets may be absent in headless/cron
  runs (already noted for MCP) — callout auth/config must be file-resident, not session-bound.

## 7. Staging

1. **Queue + claim + PySpell-proc runner** (no daemon required — any process claims/runs; the model
   is correct without wake-ups, just poll-based). **SHIPPED (2026-07-20) — this stage is the v1
   spine**; see §8 for what landed and where the code drifted from this doc.
2. **Daemon `mpedb serve`** with hibernation + doorbell + the socket API (turns poll into wake).
3. **Dynamic cron** (table + heap + CLI).
4. **Webhooks + `[callout]` config budgets**.
5. **Log retention modes** (drain/tier) — meets DESIGN-SYNC-TIERING.md.

Each stage is independently useful and independently testable (multi-process claim races go through
the CLI stress harness, like all multi-process behavior — not unit tests). Own design docs, own
adversarial review of the doorbell/claim protocol before it ships (new cross-process primitive =
commit-path-class scrutiny, per the verification discipline).

## 8. Stage 1 as shipped (v1), and drift from this doc

`mpedb queue init|enqueue|run|list` + the `queue-collide` SIGKILL fuzz
(`crates/mpedb-cli/src/queue.rs`, `queue_collide.rs`). The table is `mq_task` — §2's column list
**plus a `proc` column** (§2 says the runner executes a proc but gives it no column; it lives in the
row; `payload` is the proc's args as newline-joined CLI literals). `id INTEGER PRIMARY KEY` rides
the #94 rowid alias for auto-assignment. No new cross-process primitive was introduced — the claim
is exactly §2's single statement, now expressible verbatim since #97 (subquery-in-UPDATE) and
RETURNING landed:
`UPDATE … SET state='claimed', claimed_by=$pid, claimed_at=$now, attempts=attempts+1 WHERE
state='pending' AND id = (SELECT … ORDER BY priority, id LIMIT 1) RETURNING …` — atomic under the
writer lock, so overlapping runners claim disjoint tasks by construction (fuzz-verified: thousands
of kills, `hits ≤ attempts` everywhere, zero double-runs).

Drift found while building against the landed code:

- **RETURNING writes and the intent ring**: under `durability = commit|wal` a contended write used
  to be published as a §5.3 intent, whose result slot carries only an affected count — the claim
  statement surfaced "write plan returned rows". Fixed facade-side: `CompiledPlan::has_returning`
  keeps such plans on the direct writer-lock path (same carve-out class as host-call and
  #109 deadline-carrying plans).
- **Procs vs. live DDL**: any `CREATE TABLE` (including the queue's own lazy one) permanently
  invalidates plans embedded in already-defined write procs ("built against a different schema") —
  pre-existing limitation, surfaced by the queue; `queue init` first is the documented order.
- **Reaping**: §2's "built-in cron task" does not exist yet (stage 3); `queue run` reaps expired
  leases inline when the queue looks idle. v1's reap rule is the lease timestamp ONLY — the
  reader-table pid-liveness refinement is deferred (a naive `/proc` check would let a recycled pid
  block reclaim forever; it needs the {pid,seq}+start-time identity). Set `--lease` above the
  longest task runtime.
- **States**: `failed` = ran and errored with retries exhausted (`error` has the message); `dead` =
  lease expired with retries exhausted (a crash loop that never completed). §2 listed both without
  assigning meanings.
- Deferred to their stages: daemon/doorbell (§1B), cron table + OS projection (§3), webhooks +
  `[callout]` budgets (§4), retention (§5), per-queue workers, and a claim-path index on
  `(state, run_at)` (v1 claims via full scan — fine at queue sizes where a scan is µs).
