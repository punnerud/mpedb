# DESIGN-INGEST — the call graph into mpedb: catch every change, fetch the least

**Status: DESIGNED 2026-07-29.** Stages flip to BUILT as they land.
User-facing contract: `INGEST-GUIDE.md` (the self-contained guide).
Transformation is NOT here — once rows are in mpedb, `design/DESIGN-RRETL.md`
§13/§14 owns reshaping them and syncing them onward.

## 0. What this is, in one analogy

Route optimisation for a car. The navigator knows the road network, the
traffic, and the fuel in the tank; it computes *which way and when*. The
driver still drives. Here: mpedb observes what an external system actually
does, learns each table's change behaviour and each call's fan-out, and
computes **which calls to make, how often, in what order, under a call and
byte budget**. The user's code still makes the HTTP request.

**mpedb never calls out.** It plans, receives, diffs, verifies, records, and
carries derived parameters forward on a queue. There is no HTTP client in
this repo and this design does not add one. (`postgres` in `mpedb-mirror` is
the one existing outbound client, and it is not reused here.)

## 1. The problem everyone hits

An external system is a moving target, and every mechanism for tracking it
lies in a different way:

- Many databases stamp `created_at` on insert and **nothing on update**, so
  a cursor over it misses every edit.
- `updated_at` exists but is set by application code that forgets, or by a
  bulk backfill that touches every row at once.
- **Deletes are invisible to every cursor scheme.** A row that is gone
  cannot appear in "rows changed since X".
- APIs cap you: calls per minute, bytes per window, and the caps differ by
  endpoint and by time of day.
- The interesting calls are not independent. "List cases changed since X"
  returns keys; each key drives a detail call; each detail may drive a
  contract call; a result may drive a write **back** into another system.

The last point is the structural one, and it is why this is a planning
problem rather than a scheduling loop.

## 2. A source is a call GRAPH, not a list of tables

```
        cases_changed (root)          cheap, returns N keys
              │  fan-out = N
              ├──────────────► case_detail (derived)      N calls, or N/batch
              │                      │
              │                      └──► contract_get (derived)
              └──────────────► case_close (writeback)
```

Every node is an **edge** in the declaration (the name is deliberate: what
is planned is the *call*, and a call is an edge from a parent's result to a
child's parameters). An edge carries:

| field | meaning |
|---|---|
| `name` | stable identity within the source |
| `kind` | `root` \| `derived` \| `writeback` |
| `parent` | for `derived`/`writeback`: whose keys drive it |
| `table` | which mpedb table it fills (roots and deriveds) |
| `strategy` | `dump` \| `delta` \| `probe_fetch` \| `webhook` |
| `cursor` | candidate column name — **verified, never trusted** (§5) |
| `overlap_secs` | how far back before the watermark to re-read (§4, P2) |
| `batch` | how many parent keys fit in one child call (1 = no batching) |
| `cost_calls`, `cost_bytes` | the cost vector for one invocation |
| `weight` | importance μ; default 1 |

**A derived edge is SCOPED, and therefore never complete.** Whatever its
`strategy` says about the per-key call, a derived receipt presents only the
keys that drove it — so absence proves nothing and a delete can never be
inferred from it. A `dump` receipt through a derived edge is refused by
name; the rows go in through the delta door, which upserts. (A derived
table whose rows can vanish independently of its parent therefore needs its
own root dump edge, exactly like any other table.)

**The queue is what carries the parameters.** `ingest_derive` writes one
task row per key IN THE SAME TRANSACTION as the receipt that produced it,
so the window between "I have the keys" and "the follow-ups are recorded"
does not exist. Tasks are handed out under a lease (`ingest_next`), retired
by `ingest_done`, returned by `ingest_release`, and reclaimed after a
worker dies by `reap_leases` — a duplicate fetch is the worst case, and it
is harmless because every receipt is idempotent. Re-deriving a key that is
already waiting is a no-op, which is what makes the mandatory overlap (P2)
free on this path.

**Fan-out is data-dependent and therefore observed, never declared.** The
number of `case_detail` calls is the number of keys `cases_changed`
returned, which is a function of the change rate. That is a cardinality
estimate feeding a cost estimate — the same shape a query planner solves,
which is why §7 borrows MPEE's contracts.

## 3. The budget is a VECTOR, with windows and time profiles

A single "calls per hour" number cannot express what real APIs enforce.
The budget is:

```
{ calls_per_window, bytes_per_window, window_secs }   per PROFILE
```

with profiles selected by time of day (`work` 08–17 weekdays, `off`
otherwise, by default), plus an optional `reserved` share per edge
priority so a critical stream cannot be starved by a chatty one.

Rules taken from practice, all of which the planner must respect:

- **Honour `Retry-After` and do not add your own back-off on top.** Parse
  both RFC 9110 forms — delta-seconds *and* HTTP-date. (`parseInt` on the
  date form yields NaN; this has bitten many clients.)
- **Read `X-RateLimit-*` on SUCCESSFUL responses** and steer proactively.
  Discovering your limit via 429s is discovering it too late.
- Rate (traffic shape, burst) and quota (consumption over a long window)
  are different constraints and both are expressible above.
- N workers each enforcing limit L collectively emit N×L. The budget is
  **consumed by the planner and recorded in the database**, not enforced at
  individual call sites.

The user's fetch code reports what a call actually cost
(`ingest_rows(..., calls=1, bytes=N)`); the recorded consumption is what
the next plan is computed against. Reported cost is trusted — mpedb cannot
see the wire.

## 4. The five strategies

| strategy | when | what it costs | what it misses |
|---|---|---|---|
| `delta` | a cursor exists AND verified safe (§5) | small | rows committed late (P1); deletes; sub-granularity edits |
| `dump` | no safe cursor, or the periodic reconcile | large | nothing — this is the ground truth |
| `probe_fetch` | a cheap "anything changed?" endpoint exists | probe + P(changed)·fetch | whatever the probe's recall misses (§6) |
| `webhook` | the source pushes | ~0 | dropped pushes — REQUIRES a periodic `dump` to reconcile |
| `page_cursor` | no cursor at all, paginated list | large | requires a **stable sort** in the endpoint, else resumption drops or duplicates rows |

**Every source needs a `dump` cadence**, even when a cursor is verified
safe. It is the only channel through which deletes are visible, the only
correction for P1/P4, and the only re-verification of the cursor. A source
whose declaration has no `dump` edge is refused at define time with that
reason.

## 5. Cursor verification — measured, never assumed

This is the piece that makes the rest honest. Every `dump` re-derives, for
each candidate cursor column, whether it would have **caught** each row the
dump found changed:

```
for each row the dump found INSERTED or UPDATED:
    for each candidate column c:
        if row[c] > watermark_at_last_delta:   caught += 1
        else:                                  missed += 1   ← the cursor would have LOST this row
```

A candidate is `SAFE` only while `missed == 0` over the observed window, and
the verdict carries the counts. The first miss flips it to `UNSAFE` **and
names the row** — that is the moment a lying `updated_at` becomes a fact in
the database rather than a suspicion. A `SAFE` verdict is never permanent:
it holds until the next dump re-tests it, which is a second reason §4
requires a dump cadence.

Deletes are counted separately (`delete_rate`) because no cursor can see
them; the planner uses that rate to argue for dump frequency.

## 6. Probes: a cheap signal is worth "extra elapsed time"

A probe endpoint ("which tables changed?", or a conditional GET returning
304) is modelled as a Poisson signal with **recall** λ_rec (P a real change
emits a signal) and false-signal rate ν, so γ = λ_rec·Δ + ν and the
unobserved rate is α = (1−λ_rec)·Δ. Then

```
P(fresh) = exp(−α·τ_elapsed) · (ν/γ)^n_signals
```

collapses to a single scalar:

```
τ_eff = τ_elapsed + β·n_probe_hits ,      β = −log(ν/γ) / α
```

**Each probe hit is worth exactly β units of extra elapsed time.** A perfect
probe (ν=0) gives β=∞ → fetch immediately on any hit. A useless probe
(precision→0) gives β→0 → ignore it. The policy is then "fetch edge i when
τ_eff,i ≥ threshold", one global scalar setting the thresholds.

**Probe answers are not ground truth and their precision/recall are
MEASURED per edge**, from dumps that follow probe misses. Google measures
importance-weighted precision below 0.2 and recall below 0.5 on real
`lastmod` signals, and dropped self-declared `<changefreq>` entirely while
keeping `lastmod` — self-reported rates are worthless, observed ones are
not. Conditional GET (`ETag`/`If-None-Match`, `304`) is the degenerate case
with precision ≈ recall ≈ 1 at near-zero cost; if the API supports it, use
it everywhere.

## 7. The objective, and the trap in it

Two objectives look interchangeable and behave oppositely. This section
exists so no future "improvement" silently swaps them.

**Binary freshness** ("is the table current right now?") has the
time-averaged form `F̄(λ,f) = (1−e^{−λ/f})/(λ/f)`, and its budget-optimal
allocation is an **inverted U in λ: the fastest-changing table is polled
exactly zero times.** Cho & García-Molina state it plainly — "to improve
freshness, penalize the elements that change too often" (TODS 28(4) 2003,
Thm 5.5; worked example λ = 1..5/day, budget 5/day → f = 1.15, 1.36, 1.35,
1.14, **0.00**). For a mirror that must catch every change, that is a
catastrophe dressed as an optimum.

**Harmonic staleness** penalises `C(n) = Σ_{i≤n} 1/i` for n unseen changes
— strictly increasing (every missed change counts) and discrete-concave
(the first hurts most). Its optimum under `Σρ = R` has a closed form:

```
ρ_w = ( √( Δ_w² + 4·μ_w·Δ_w / λ ) − Δ_w ) / 2
```

with a single Lagrange scalar λ found by bisection, ε-optimal in
O(|E|·log(1/ε)). **No source that changes is ever starved** (ρ_w > 0
always). (Kolobov, Peres, Lu, Horvitz, NeurIPS 2019.)

**We optimise harmonic staleness.** "Catch every change" is a
staleness-count objective, not a binary-freshness one.

### 7.1 Two more results that are not intuitive

- **Uniform beats proportional-to-change-rate, under every distribution of
  λ** (Thms 5.1/5.2; the proof is one line of Jensen). Measured on web data:
  proportional 0.12 freshness / 400 days age, uniform 0.57 / 5.6 days,
  optimal 0.62 / 4.3. The instinct "poll the fast-changing table more" is
  the single biggest trap here. **Uniform is therefore built first and kept
  as the control arm** — the house rule that anything claimed to cost
  something needs a control.
- **When μ/Δ is roughly constant across edges, importance-proportional is
  provably optimal** and the change-rate estimate does not matter at all
  (Kolobov et al., SIGIR 2019). The advisor says so when it sees it, rather
  than pretending its estimator earned the plan.

### 7.2 Spacing, and per-source caps

Same budget, three schedulings, freshness at λ/f = 1: **evenly spaced
0.632**, random order 0.599, Poisson-rate 0.500. A work queue that
randomises when an edge runs throws away a fifth of the freshness the
budget paid for. Plans emit **evenly spaced** cron lines.

Per-source caps (your min/max per window) are applied by
**saturate-remove-recurse**: solve unconstrained; for each violated cap,
re-solve restricted to that source with the cap as an equality, fix those
rates, subtract from the global budget, recurse. Proven optimal (SIGIR
2019, Alg. 2).

### 7.3 Estimating Δ from binary observations

Each receipt records only "did anything change?". **Never use the naive
`Δ̂ = X/(n·I)`** — it is biased *and* inconsistent: it converges to
pΔ/(Δ+p), saturating at your own poll rate, so it structurally cannot see a
table that changes ten times between polls. Switching estimators is worth
~35% freshness on real data. Use the LLN closed form:

```
Δ̂_k = p · Î_k / (k + α_k − Î_k)
```

k = receipts, Î_k = receipts that found a change, p = poll rate, α_k = a
small stabiliser (1) so a table that changes every single time yields a
large-but-finite estimate rather than ∞. O(1) per update, provably
consistent. (Singh et al. 2020, Thms 1–2.)

Two honest limits, stated because they bound what any plan can promise: the
estimate is **capped by poll granularity** (a table polled daily can never
be measured above 1/day), and bursty tables are mis-estimated by any
homogeneous-Poisson estimator.

## 8. The graph cost, and borrowed MPEE contracts

Cost along a path multiplies: a root at rate ρ with fan-out F costs
`ρ·(c_root + F/batch · c_child)` per window, in each of `{calls, bytes}`.
Fan-out is observed per edge (§9), so a plan's cost rests on estimates.

MPEE's solver is **not** reusable here — it is SQL-AST-typed and searches
left-deep permutations over a bitmask; an ingest plan is a rate assignment
over a tree under a budget. What transfers, and is reimplemented, are its
three contracts (`design/DESIGN-MPEE-GENERAL.md` §1–3):

1. **Every cost dimension is monotone**, so a partial cost is a sound lower
   bound and pruning is exact.
2. **Unbought inputs price at their lower bound**, so the search may run on
   floors and only "buy" (read the measured fan-out for) the edges the
   winning proposal actually rests on — MPEE's ping-pong, ~20 lines, with
   the same soundness argument.
3. **Quantise with log2 buckets** (`mpedb_sql::magnitude`, the solver's own
   function, reused so the two cannot drift) for stability against noise.

## 9. Bookkeeping — four tables, rigid types, never sys-keyspace

Like rRETL: ordinary TABLES built from `CreateTableSpec` with rigid column
types (#124 measured compilation as O(bytes in the sys keyspace), and these
are unbounded logs). The **declaration** is small and bounded, so it rides
the sys keyspace as versioned TOML at `ingest/<name>`, exactly like
`rrmap/<name>`.

```
ingest_stats (source, edge, run_id)
    → ts_micros, mode, rows_in, inserted, updated, deleted, unchanged,
      calls, bytes, changed, cursor_verdict, note
        -- one row per receipt; Δ̂ and fan-out are derived FROM this log

ingest_state (source, edge)
    → fingerprint, watermark, cursor_col, cursor_state, caught, missed,
      fanout, probe_hits, probe_misses, ts_micros
        -- the observed model. `fingerprint` guards staleness the way
           stats.rs does: it is blake3 over the EDGE IDENTITY (name ‖ kind ‖
           table ‖ strategy ‖ cursor ‖ batch), so a redefined edge's
           observations decode to None rather than to a lie. Decode fails
           SOFT — a stale record must never fail a run, it must price as
           "never observed".

ingest_conflicts (source, tbl, pk_ref)
    → k, kind, detail, ts_micros
        -- everything the policy would not decide, queryable = the alert
```

ingest_task (source, edge, pk_ref)
    → k, state, lease, ts_micros
        -- the cascade's queue (§2). `k` is the key VALUE, because the
           worker needs it to make the call and a digest is one-way; the
           pk_ref digest is what keys it, because raw values do not fit a
           bounded composite key. `state` is pending|claimed, `lease` the
           claim stamp a worker passes back to `ingest_done`.
```

Plus one internal set used only inside a dump:

```
ingest_seen (source, tbl, pk_ref)      -- keys this dump has presented
```

## 10. The dump protocol — streamed, no per-table DDL

A full dump must find deletes, which needs the whole key set, but must not
materialise the dump. The protocol:

1. `begin(source, table, mode=dump)` → run id.
2. For each chunk the user pushes: per row, point-read the target,
   classify (`insert` / `update` / `unchanged`), apply under policy, and
   record the key in `ingest_seen`. Commit per chunk.
3. `finish`: stream the target; every key **not** in `ingest_seen` is a
   delete (policy-gated). Clear `ingest_seen`. Write `ingest_stats` and the
   cursor verdicts.

Two properties this buys: memory is O(chunk) regardless of dump size, and
**no DDL runs per dump** — which matters because DDL bumps the schema
generation and invalidates every other process's prepared plans (learned
the hard way in #53; the daemon's first run does exactly that, once).

A `delta` run is the same loop without step 3: deletes cannot appear in a
delta by construction, and `ingest_stats` records that fact rather than
implying coverage.

**Visibility semantics, stated:** a dump commits per chunk, so a reader
between two chunks sees a partially-updated table (Fivetran has the same
property and recommends pausing readers; Airbyte instead swaps at
completion). Ingest chooses per-chunk commit for the same reason `rretl map
run` does — progress must survive a kill — and says so here rather than
letting a reader discover it.

## 11. Conflicts: three separate requirements

Confusing these three is the classic two-way-sync failure, so they are
named separately:

1. **Echo suppression** — a change that arrived BY sync must not be pushed
   back as if it were local. For the mpedb→external direction this is
   rRETL §13.2's recorded state hashes; for external→mpedb it is the
   compare-before-write in §10 step 2 (an identical row is `unchanged` and
   writes nothing at all).
2. **Field-level change detection** — compare before write, so an untouched
   field is not "updated".
3. **Idempotent writes** — every apply is an upsert keyed on the external
   identity, so a re-read (P2's overlap) is free and a duplicate delivery
   is harmless.

Policy menu (what products expose): `source` wins · `local` wins · `newest`
wins · per-field owner · custom function. **v1 implements `source` and
`local`** — mirror's `resolve.rs` spelling, chosen deliberately because
three spellings already exist in this repo (`upstream/local`,
`source/local`, `ours/theirs`). `newest` is deliberately NOT in v1: it
depends on a clock you do not control, and version counters are the correct
mechanism where the source offers them.

**Anything the policy will not decide lands in `ingest_conflicts`. Never a
silent overwrite.** The table is the alert channel: `ingest conflicts`
exits nonzero when it is non-empty, so cron mails it.

## 12. Two-way, and gradual migration

```
external ──ingest──► source set ──rRETL map──► working set ──► users
    ▲                                                │
    └────────── user's writeback code ◄──────────────┘
```

Ingest owns getting rows in and detecting what changed. rRETL §13 owns
reshaping and the loop safety of syncing both ways. The writeback call is
an edge of kind `writeback` in the same graph, so it is budgeted with
everything else.

This is the **strangler fig** pattern with a database under it: transform →
coexist → eliminate. During coexistence both systems are live and both are
written to, which is exactly where §11's three requirements become
load-bearing rather than optional. It is also what lets different user
groups keep their own specialist system while the sets stay in sync.

**Bootstrap rule** (from DMS's bidirectional guidance, generalised): full
load one direction while the other side is frozen, record the position at
the instant writes are unfrozen, and start the reverse direction from that
position. Otherwise the reverse direction has a gap exactly the width of the
initial load.

## 13. The named pitfalls, with sources

Rules the design must not lose. Each is a real, documented failure.

- **P1 — a cursor orders ASSIGNMENT, not COMMIT.** A long transaction
  stamps `updated_at = T0`, the sync advances past T0, then the transaction
  commits. `WHERE cursor > watermark` never sees that row again. Cursor
  incremental is lossy **by construction**; only the dump corrects it.
  ([Airbyte #9668](https://github.com/airbytehq/airbyte/issues/9668),
  [dlt #2269](https://github.com/dlt-hub/dlt/issues/2269))
- **P2 — overlap + idempotent upsert is mandatory, not an optimisation**,
  and the lower bound is INCLUSIVE. Two independent causes stack: late
  commits, and clock skew between the source's clock and your watermark.
  ([dlt lag](https://dlthub.com/docs/general-usage/incremental/lag))
- **P3 — never advance a watermark past a value whose completeness you
  cannot prove** (Singer's "signpost"), never checkpoint before the batch is
  durably written, and a partial run over an unsorted source must leave NO
  usable watermark.
  ([Meltano SDK state](https://sdk.meltano.com/en/latest/implementation/state.html),
  [Airbyte #12821](https://github.com/airbytehq/airbyte/issues/12821))
- **P4 — the cursor does not fire on every mutation, and a bulk backfill
  fires it on everything.** Both directions have no cursor-side fix; the
  periodic dump is the answer to both.
- **P5 — system cursors have cliffs.** Postgres `xmin` is 32-bit and stops
  being monotone on wraparound; log positions expire with retention.
  ([Fivetran xmin wraparound](https://fivetran.com/docs/connectors/databases/postgresql/troubleshooting/xmin-wraparound-causing-excess-mar))
- **P7 — deletes need an explicit channel**, and the reconcile pass must
  state its visibility semantics (§10).
  ([Fivetran soft delete](https://fivetran.com/docs/core-concepts/sync-modes/soft-delete))
- **P8 — a dump running concurrently with a delta needs a dedupe rule.**
  Debezium's DDD-3 answer: mark the window in the log and drop from the
  chunk buffer any key that appeared inside it. Ingest's v1 answer is
  simpler and stated: **a source runs one receipt at a time** (the lease),
  so the window cannot open.
  ([DDD-3](https://github.com/debezium/debezium-design-documents/blob/main/DDD-3.md))
- **P12 — rate limits are budget allocation, not retry policy** (§3).
  ([Jira](https://developer.atlassian.com/cloud/jira/platform/rate-limiting/),
  [Salesforce rolling limits](https://help.salesforce.com/s/articleView?id=002888831))

## 14. Theory sources

- Cho & García-Molina, *Effective Page Refresh Policies for Web Crawlers*,
  ACM TODS 28(4), 2003 — Poisson model, F̄/Ā closed forms, Thm 5.1/5.2
  (uniform > proportional), Thm 5.5 (the inverted U), spacing comparison.
- Cho & García-Molina, *Estimating Frequency of Change*, ACM TOIT 3(3),
  2003 — why the naive estimator is inconsistent.
- Singh et al., *Change Rate Estimation and Optimal Freshness in Web Page
  Crawling*, 2020 — the LLN estimator in §7.3, Thms 1–2.
- Kolobov, Peres, Lu, Horvitz, *Staying up to Date with Online Content
  Changes Using RL for Scheduling*, NeurIPS 2019 — harmonic objective, the
  closed form in §7.
- Kolobov, Peres, Lubetzky, Horvitz, *Optimal Freshness Crawl Under
  Politeness Constraints*, SIGIR 2019 — per-source caps (Alg. 2), the
  importance-proportional optimality result.
- Azar, Horvitz, Lubetzky, Peres, Shahaf, *Tractable near-optimal policies
  for crawling*, PNAS 2018 — the binary-freshness exact solution.
- Busa-Fekete, Zimmert, György et al. (Google), *A Scalable Crawling
  Algorithm Utilizing Noisy Change-Indicating Signals*, 2025 — §6's β.

## 15. Staging

| stage | contents | status |
|---|---|---|
| B1 | this document + `INGEST-GUIDE.md`; formats frozen | **BUILT** |
| B2 | `ingest.rs`: declaration, dump/delta protocol, diff-apply, cursor verification, conflicts | |
| B3 | `ingest advise`: uniform control arm, harmonic solver, caps, profiles, cron emission | |
| B4 | cascade: queue-driven derived calls, budget accounting per edge | |
| B5 | `workbench/ingest-lab` + the agent trial against the GUIDE | |

Out of scope, deliberately: an HTTP client; new SQL syntax; changes to
`mpee.rs`/`CostSource` (contracts borrowed, code untouched); Merkle /
range-digest diffing for very large tables (the natural later optimisation
of §10 — cite `pt-table-checksum`'s adaptive chunk sizing when it is built);
`newest`-wins conflict policy.
