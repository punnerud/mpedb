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
with transactional guarantees, no server to run. The optional daemon adds only what a file cannot do
alone: *wake-ups* (timers, a doorbell) and *outbound* I/O (webhooks) under a rate budget.

## 1. Service / daemon mode (`mpedb serve <db>`)

- A long-lived process that attaches the `.mpedb` and **hibernates** ("dvale") — no busy polling. It
  blocks on the earliest of: (a) an inbound API request, (b) the next due scheduled task, (c) a
  **doorbell** raised by another process that just enqueued work.
- **Doorbell**: mpedb already has a shared lock area + reader table in the mmap (pages 2–3, CLAUDE.md).
  Add a lightweight cross-process wake primitive there — a futex/eventfd-style counter another
  process bumps on enqueue, so the daemon wakes immediately without polling. Falls back to a bounded
  timer if the platform lacks the primitive. Multi-process identity/robustness reuses the existing
  {pid,seq}+start-time + boot-id recovery (`post_attach`).
- **API surface**: a local Unix-domain socket by default (and optional TCP), speaking a small
  request protocol — `submit(proc, args)`, `enqueue(queue, payload, run_at?)`, `query(sql)`,
  `wake`. "Exposes an API that wakes on request" = the socket accept loop is one of the hibernation
  wake sources. HTTP is a thin adapter over the same protocol (Phase 6b).
- The daemon is **optional and stateless w.r.t. correctness**: everything it does (claim a task, run
  a proc, fire a webhook) is a normal transactional mutation any process could do. Kill the daemon
  and direct attach still works; restart and it recovers from the tables. No daemon = no wake-ups and
  no outbound callouts, but the data model is unchanged.

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

- A `cron` table: `(name, schedule, proc, args, next_run, last_run, enabled, singleton)`. The daemon
  keeps a min-heap on `next_run` and sleeps until the earliest; firing enqueues a task (§2) rather
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
   is correct without wake-ups, just poll-based).
2. **Daemon `mpedb serve`** with hibernation + doorbell + the socket API (turns poll into wake).
3. **Dynamic cron** (table + heap + CLI).
4. **Webhooks + `[callout]` config budgets**.
5. **Log retention modes** (drain/tier) — meets DESIGN-SYNC-TIERING.md.

Each stage is independently useful and independently testable (multi-process claim races go through
the CLI stress harness, like all multi-process behavior — not unit tests). Own design docs, own
adversarial review of the doorbell/claim protocol before it ships (new cross-process primitive =
commit-path-class scrutiny, per the verification discipline).
