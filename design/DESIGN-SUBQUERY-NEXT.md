# DESIGN-SUBQUERY-NEXT — the next three subquery optimizations (#73)

Status: design only, no code. Companion to DESIGN-MPEE-OPT.md (the correlation-key
memoization + consumer-cap LIMIT pruning already shipped there) and to
`crates/mpedb-sql/src/planner/subquery.rs` (the #56 lift). This document specifies
three follow-ups, ranked at the end by value/effort for closing sqlite parity and
speed.

The subquery machinery as-built (the ground every design stands on):

- **Lift, not rewrite.** `plan_select` calls `subquery::lift_subqueries` FIRST
  (select.rs:262). Every `(SELECT …)` becomes a `SubPlan` on `CompiledPlan.subplans`
  and is replaced in the outer AST by `Param(slot)`; no stage below the lift knows
  subqueries exist. Slot layout is `[user ‖ subplan results ‖ context]`
  (`CompiledPlan::subplan_base`, plan/mod.rs:272).
- **Correlation = trailing inner params.** `Correlate::rewrite` (subquery.rs:426)
  turns an outer-row reference into `Param(n_user + j)`; `outer_args[j]` names the
  outer base-row slot that fills it. The inner is planned with param space
  `[user ‖ correlation args]` (`inner_n = self.n_params + outer_args.len()`,
  subquery.rs:349).
- **Where slots get filled.** Uncorrelated subplans are evaluated ONCE per execute
  before dispatch (`exec_stmt_impl`, exec/mod.rs:423). Correlated ones are filled
  per outer row in `exec_select_with` (exec/mod.rs:813), which also carries the
  `post_filter` and the shipped per-correlation-tuple memo.
- **The gather/post split.** `subquery::split_correlated` (subquery.rs:512) cuts the
  bound WHERE into a gather-safe conjunct set (`filter`, resolved into the access
  path) and a correlated residual (`post_filter`, run per row after every policy).
- **Validate's slot discipline.** `validate_subplans` (plan/validate.rs:481) forbids
  every GATHER-side program (access parts, `filter`, `joined_filter`, join `on`/
  `policy`) from reading a correlated slot — those slots are holes until the per-row
  phase. `post_filter` is the one program allowed to read them.

Three current refusals are the targets:

1. `plan_join_select` / `plan_select`: **"a correlated subquery in an aggregate
   query is not supported yet"** (join.rs:304, select.rs:358), mirrored by
   validate.rs:516.
2. Memoization gives nothing when every outer correlation key is DISTINCT — the
   all-distinct semi-join case.
3. `plan_one`: **"nested subqueries are not supported yet"** (subquery.rs:255),
   mirrored in `Correlate::rewrite` (subquery.rs:453/469).

---

## 1. Aggregate over a correlated filter

**Target:** `SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g)`
and the grouped form `SELECT a.dept, count(*) FROM a WHERE EXISTS (…) GROUP BY
a.dept`. Both are refused at plan time today.

### 1.1 Why it is refused, and why the refusal is now removable

The refusal predates the `post_filter`/memo machinery. It reads
(select.rs:357, join.rs:303):

```rust
if correlated.iter().any(|&c| c) {
    return Err(bind_err("a correlated subquery in an aggregate query is not supported yet"));
}
return plan_aggregate_select(…);
```

and `plan_aggregate_select` hardcodes `post_filter: None` on both SelectPlan exits
(aggregate.rs:383 and :447). The executor's aggregate path (`exec_aggregate`,
exec/aggregate.rs:28) never fills a correlated slot and never applies a post-filter.

But the non-aggregate path already does exactly the right thing with the SAME two
pieces: `split_correlated` puts the uncorrelated WHERE conjuncts gather-side and the
correlated ones in `post_filter`, and `exec_select_with` gathers, fills correlated
slots per row (memoized), applies `post_filter`, THEN continues. An aggregate query
differs only in what happens to the surviving rows: they are accumulated instead of
projected. The security ordering DESIGN-MULTIDB §4 demands — aggregate only over
rows past `(WHERE ∧ policy)` — is preserved as long as the correlated post-filter
runs BEFORE accumulation, because the correlated conjunct is part of the WHERE.

### 1.2 Planner changes

**a. `split_correlated` in the aggregate path.** In BOTH `plan_select`
(single-table aggregate) and `plan_join_select` (joined aggregate), stop refusing;
instead compute the `(gather, post)` split exactly as the non-aggregate path does and
thread the post half into the aggregate plan.

- `crates/mpedb-sql/src/planner/select.rs`: the split already happens at select.rs:314
  (`let (bound_where, post_where) = subquery::split_correlated(…)`), and
  `post_filter` is already compiled at select.rs:337. Today that `post_filter` is
  simply dropped on the aggregate branch. Change the aggregate branch (select.rs:353)
  to (i) not refuse on `correlated.any`, and (ii) pass `post_filter` into
  `plan_aggregate_select`.
- `crates/mpedb-sql/src/planner/join.rs`: the split is at join.rs:201, `post_filter`
  compiled at join.rs:203. The join aggregate branch (join.rs:302) already HAS
  `post_filter` in scope — it is threaded into the non-aggregate SelectPlan at
  join.rs:415 but discarded on the aggregate branch. Remove the `correlated.any`
  refusal and pass `post_filter` into `plan_aggregate_select`.

**b. `plan_aggregate_select` gains a `post_filter` parameter.**
`crates/mpedb-sql/src/planner/aggregate.rs:206`. Add
`post_filter: Option<ExprProgram>` to the signature and set it on both SelectPlan
constructions (aggregate.rs:383 DISTINCT exit, aggregate.rs:447 normal exit) instead
of the hardcoded `None`. Nothing else in the function moves — the grouped-tuple
machinery, HAVING, ORDER BY are untouched.

**c. The refusal boundary that stays.** The correlated slot may be read ONLY by the
WHERE (→ `post_filter`). It must NOT appear in an aggregate ARGUMENT, a GROUP BY key,
HAVING, or the SELECT list of the aggregate, because those are evaluated over the
grouped tuple AFTER the per-row correlation has been collapsed — "which row's
correlation?" has no answer. This is enforced structurally and needs no new code:

- A subquery in HAVING is already refused by the lift (subquery.rs:142).
- A subquery in GROUP BY cannot plan (keys must be columns; subquery.rs:138).
- A correlated subquery in an aggregate SELECT item or an aggregate argument leaves
  its `Param(slot)` reference in a GATHER-side program (the projection over the
  grouped tuple, or the agg-arg program over the base row). Extend
  `validate_subplans` to run `gather_ok` over the aggregate's programs too
  (`group_by` `GroupKey::Expr`, each `AggCall::arg`, `having`, and every grouped
  `Projection::Expr`) — see 1.4. A correlated slot there is then rejected as corrupt,
  and the planner never emits it because those programs are built by the aggregate
  binder which cannot reach a per-row slot. Net: only `EXISTS`/scalar/`IN` in the
  WHERE is admitted, which is the whole target set.

### 1.3 Executor changes

Route aggregate + (correlated | post_filter) to the aggregate path, and give
`exec_aggregate` the per-row correlated pre-filter.

- `exec_select_top` (exec/mod.rs:496) currently sends anything with correlated
  subplans or a `post_filter` to `exec_select_with`, which errors on
  `aggregate.is_some()` (exec/mod.rs:837). Change the dispatch so that when
  `aggregate.is_some()` AND (correlated non-empty OR `post_filter.is_some()`), it
  calls `exec_aggregate` with the correlated subplans and post-filter in hand.
- `exec_aggregate` (exec/aggregate.rs:28) gains `correlated: &[(usize, &SubPlan)]`
  and `post_filter: Option<&ExprProgram>` and the scratch buffer. Between the gather
  (aggregate.rs:50) and the grouping loop (aggregate.rs:62), insert a per-row phase
  that is a copy of `exec_select_with`'s inner loop (exec/mod.rs:880–912),
  factored into a shared helper `fill_and_filter_rows` so the two paths cannot drift:

  ```text
  fn correlated_survivors(ctx, schema, plan, params, rows,
                          correlated, post_filter) -> Result<Vec<Vec<Value>>>:
      memo = vec![HashMap::new(); correlated.len()]      // per-tuple, the shipped memo
      scratch = params.to_vec()
      out = Vec::new()
      for row in rows:
          for (ci, (i, sub)) in correlated:
              key_vals = sub.outer_args.map(|a| row[a])
              scratch[base+i] = memo-or-run(sub, key_vals)   // MPEDB_NO_SUBPLAN_MEMO honored
          if post_filter.map_or(true, |pf| pf.eval_filter(&scratch, row)):
              out.push(row)
      out
  ```

  `exec_aggregate` then groups over `out` instead of `rows`. Everything downstream
  (the empty-group zero row, HAVING, ORDER BY, LIMIT-bounds-groups) is unchanged.

- `exec_select_with`'s existing per-row loop is the same code; extract the shared
  helper and call it from both, retiring the duplication.

### 1.4 Correctness argument

- **Visibility / raise contract.** `gather_rows`/`gather_joined` apply the outer
  access path and every policy (`filter`, `join.policy`, `joined_filter`) before a
  single row is returned, exactly as in the non-aggregate correlated path. The
  correlated subplan runs only over already-visible rows, and the inner subplan
  carries `b`'s own SELECT policy (it was planned through `plan_select`). So no
  subplan executes against a row the caller could not see — the same guarantee
  `exec_select_with` already relies on (exec/mod.rs:806 doc).
- **§4 aggregate ordering.** Accumulation consumes `correlated_survivors(rows)`,
  which is `rows` (already `(WHERE_uncorrelated ∧ policy)`) further filtered by the
  correlated WHERE conjunct. That is precisely the full `(WHERE ∧ policy)` set §4
  requires aggregation to run after. `count(*)` therefore counts only rows the caller
  may see AND that satisfy `EXISTS(...)`.
- **Empty group.** `SELECT count(*) FROM a WHERE EXISTS(<never true>)` must yield one
  row `0`. When `correlated_survivors` is empty and `agg.group_by` is empty,
  `exec_aggregate` already pushes the zero-accumulator row (aggregate.rs:96). The
  grouped form correctly yields no groups (SQL semantics) when nothing survives.
- **Memo determinism.** The memo is a pure function of `(user params, correlation
  tuple)` over the txn's stable MVCC snapshot; a scalar subplan's >1-row error still
  fires on the first occurrence of a key (miss path) — byte-identical to per-row
  re-execution (the property DESIGN-MPEE-OPT already argues).

### 1.5 Validate

`crates/mpedb-sql/src/plan/validate.rs`:

- **Delete** the outright refusal at validate.rs:516 (`any_correlated &&
  outer.aggregate.is_some()`).
- **Add** `gather_ok` coverage for the aggregate's programs in `validate_subplans`,
  so a correlated slot leaking into `group_by`/`aggs.arg`/`having`/grouped
  `projection` is still `corrupt` (this closes the boundary of 1.2c). The existing
  `gather_ok` closure (validate.rs:520) is reused verbatim.
- The "post-filter without subplans" guard (validate.rs:55) and the per-program
  gather-side checks are unchanged and now also protect the aggregate case.

### 1.6 Footprint

No change. `compute_footprint(stmt, subplans, schema)` already unions the subplan
tables' read bits and degrades `key_access` to `Full` whenever subplans exist
(planner/footprint.rs:86). The aggregate SelectPlan's own footprint is computed by
`select_footprint` regardless of `aggregate`. Validate recomputes and compares.

### 1.7 PLAN_FORMAT

**No bump required.** `encode_select` writes `post_filter` unconditionally
(encode.rs:293), BEFORE the `aggregate` block (encode.rs:296), so an aggregate
SelectPlan carrying a `post_filter` is already a valid, in-layout blob; `decode_select`
reads it symmetrically (decode.rs:450). The only change is that `validate` stops
rejecting the shape. An older binary decoding a new plan fails CLOSED — its
`validate` still carries the old refusal and returns `Corrupt`/`PlanInvalidated`,
never a wrong answer — and plan hashes already fold `FORMAT_VERSION`, so a re-prepare
is the documented recovery. A bump is therefore optional; take one only if you want
mixed binaries sharing a registry to report "unknown plan format" instead of
"corrupt" for the new shape. Recommendation: **skip the bump** (there is no wire
change to gate), and note the relaxed validate rule in the format history comment.

---

## 2. Build-once hash semi-join / de-correlation (the all-distinct case)

**Target:** `WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.c)` (and `a.c IN (SELECT k
FROM b …)`) where the outer keys `a.c` are all DISTINCT, so the shipped
per-correlation-tuple memo never hits. The idea: detect the equi-correlation
`inner.k = outer.c`, build the inner keyed set ONCE, and turn each outer row into an
O(1) probe.

### 2.1 First, verify how much an index already buys — because it buys most of it

Read the inner subplan's access path for `WHERE b.k = $corr`. After `Correlate`
rewrites `a.c` to `Param(n_user+j)`, the inner is planned by `plan_select` →
`extract_access` (planner/access.rs:6) over the conjunct `b.k = Param(...)`:

- **`b.k` is `b`'s PK** → `PkPoint([Param])` (access.rs:35). Per outer row the inner
  is a single `get_by_pk` — O(log M).
- **`b.k` is a single-column UNIQUE/indexed column** → `IndexPoint { parts:[Param] }`
  (access.rs:97). Per outer row a `scan_by_index` — O(log M + matches).
- **`b.k` is unindexed** → `FullScan` with the equality as the residual filter
  (access.rs:196). Per outer row, `exec_select` scans ALL of `b` and filters. And on
  the all-distinct memo-miss, that full scan happens for EVERY outer row: **O(N·M)**.

So the asymptotic win of de-correlation is entirely in the **unindexed** case:
`O(N·M) → O(N + M)`. When `b.k` is indexed, the inner is already an O(log M) probe;
build-once would replace a B+tree descent with a hash lookup and skip re-descending
the root each time — a constant-factor speedup (no new plan needed), not an
asymptotic one. The honest conclusion: **an index on the inner correlation column
already captures the asymptotic win.** In a rigid-schema engine where indexes are
declared, the idiomatic answer to a slow correlated EXISTS is "index `b.k`", and
`EXPLAIN` can be made to say so.

Note also that rewriting EXISTS to a plain join does NOT rescue the unindexed case:
the join executor reads a `FullScan` inner once and HOLDS it, then linear-scans the
held Vec per outer row — still O(N·M) `on` evaluations (gather.rs:16 doc, and the
inner loop at gather.rs:163). The primitive mpedb genuinely lacks is a HASH-keyed
inner, whether reached via a semi-join or via a build-once subplan.

### 2.2 What the optimization actually is: ~half planner, ~half exec

The correlation `inner.k = outer.c` is buried inside the inner's compiled `filter`
program as `Col(k) == Param(corr)`. The executor cannot reliably introspect an opaque
`ExprProgram` to discover the key column. So a PLANNER de-correlation pass must
detect the shape and record a descriptor the executor consumes:

- **Planner (detection + descriptor).** In `plan_one` (subquery.rs:250), after the
  inner is planned, inspect it for the semi-join shape: `kind ∈ {Exists, List}`,
  exactly one `outer_arg`, and the inner's WHERE contains a top-level conjunct
  `inner_col = Param(corr_slot)` where `inner_col`'s type equals the outer column's.
  When found, emit a `SubPlan.semi_join: Option<SemiJoinKey>` with `{ inner_col,
  build_access }`, where `build_access` is the inner's access path with the
  correlated equality REMOVED (so the one-time build scans `b` under only the
  uncorrelated part of the inner WHERE + `b`'s policy). Everything else the inner
  carries (its own uncorrelated filter, its policy) stays, so the built set already
  respects `b`'s RLS.
- **Executor (build-once + probe).** In `exec_select_with` / the shared helper,
  before the outer loop, for each subplan with a `semi_join` descriptor: run the
  inner ONCE under `build_access`, projecting `inner_col`, and insert every value's
  `keycode::encode_key` into a `HashSet<Vec<u8>>` (the "buy once" build). Per outer
  row the correlated slot becomes `Bool(set.contains(enc(outer.c)))` for EXISTS, or
  the membership test for `IN` — an O(1) hash probe, no inner execution. This is the
  same `HashSet<Vec<u8>>` keying the executor already uses for DISTINCT/set-ops
  (exec/mod.rs:521), so NULL semantics (a NULL outer key never matches — SQL's
  `= NULL` is UNKNOWN, and NULL-containing index entries are absent) are handled by
  simply not probing when `outer.c` is NULL, and by the build never inserting rows
  whose `inner_col` is NULL.

Split: the DETECTION and the descriptor are planner work (the harder, correctness-
critical half); the build/probe is a localized exec addition mirroring the existing
memo. The memo and the semi-join are complementary — memo wins on repeated keys,
semi-join wins on distinct keys over an unindexed inner — and both can coexist
(semi-join build supersedes the memo for a subplan that has a descriptor).

### 2.3 Correctness argument / refusal boundary

- **Single equi-conjunct only.** Admit the descriptor only when the sole correlation
  is one `inner_col = outer_c` equality of EXACTLY matching column types (so the
  `keycode` bytes of `outer.c` and stored `inner_col` are comparable — the same
  exact-type rule `extract_join_access` enforces at join.rs:527). A second
  correlated conjunct, an inequality, a cross-type comparison, or a correlated
  reference anywhere but that one equality → no descriptor, fall back to per-row
  execution (still correct, just not accelerated).
- **RLS.** The one-time build runs the inner's own policy-bearing scan; the built
  set is exactly `{ b.k : b visible ∧ inner-WHERE }`. Probing it is semantically
  identical to running the inner per row. No new visibility surface.
- **NULL 3VL.** EXISTS over a set: outer NULL → no probe → `Bool(false)` (an inner
  row with NULL `k` cannot satisfy `k = NULL`). `IN`: outer NULL, or a NULL present
  in the set, must yield UNKNOWN not FALSE — so the `IN` variant must additionally
  track "did the inner produce any NULL key" and fall back to the 3VL rule
  `in_list_3vl` uses, OR restrict the descriptor to EXISTS only in stage 1 (simplest
  and covers the common `WHERE EXISTS`).

### 2.4 PLAN_FORMAT

**Bump required.** `SubPlan` grows a field (`semi_join: Option<SemiJoinKey>`),
encoded after `outer_args` in `encode` (encode.rs:29–37) and read in `decode`
(decode.rs:120–136). A format-N reader would desync on the extra bytes. This rides
the normal additive-bump discipline (plan/mod.rs history block).

### 2.5 Verdict

Value is real but NARROW (unindexed correlation columns only) and largely subsumed by
declaring an index; effort is the highest of the three (new plan field + format bump
+ planner detection + exec hash primitive + `IN`/NULL 3VL care). Stage it behind the
cost/cardinality broker DESIGN-MPEE-OPT §5 anticipates: with the catalog's exact
`row_count`, the planner can emit the descriptor only when `b` is large and `b.k`
unindexed — the exact regime where it pays — and otherwise lean on the index + memo.

---

## 3. Nested subqueries

**Target:** remove the `plan_one` refusal at subquery.rs:255 (and the
`Correlate::rewrite` mirrors at subquery.rs:453/469) so a subquery may contain
subqueries.

### 3.1 The two structural blockers

**a. The subplan tree is FLAT.** `SubPlan { plan: SelectPlan, outer_args, kind }` and
`SelectPlan` has NO `subplans` field — every subplan of a statement lives in one
`CompiledPlan.subplans` Vec (plan/mod.rs:266). A nested subquery has nowhere to be
represented as a child of its parent subplan.

**b. Each level's param space is closed.** A subplan's inner is planned with param
space `[user ‖ correlation args]` only (`inner_n = self.n_params + outer_args.len()`,
subquery.rs:349). It cannot reference any OTHER subplan's result slot. And the
executor reinforces this: a subplan runs via the plain `exec_select`, whose param
buffer is `params[..n_user] ++ correlation args` (exec/mod.rs:894) — it carries the
user params and this subplan's correlation args, but NOT any sibling/child subplan
result. So even if a nested subplan's slot existed, the middle level could not read a
filled value out of it. This is the **reserved-slot layout reconciliation** the task
names, and it is the real work.

`plan_one` already asserts the flat invariant: `debug_assert!(inner_subs.is_empty(),
"nesting refused above")` (subquery.rs:352) and refuses `current_setting()` inside a
subquery for the same reason (subquery.rs:353).

### 3.2 Recommended architecture: make `SubPlan` recursive

Give each subplan its OWN children and its OWN local slot space, mirroring the
top-level design one level down:

- **Structure.** `SubPlan` gains `subplans: Vec<SubPlan>` (its inner lifts) and
  enough to locate their result slots — a `sub_base: u16` (or an `n_params` for the
  inner) so exec can compute where the children's result slots sit in the inner
  param buffer. Inner layout becomes `[user ‖ children results ‖ correlation args]`,
  the same "results between user and the trailing reserved slots" shape the top
  level uses.
- **Planner.** `plan_one` stops refusing `has_subquery(inner)`. Instead it lifts the
  inner's OWN subqueries with a fresh `Lift` scoped to the inner (its outer scope is
  the inner's `[user ‖ correlation args]` plus, for stage 3, the enclosing scopes),
  producing `inner_subs`, which are stored on the `SubPlan` rather than asserted
  empty. The nested `plan_select` already returns `inner_subs` (subquery.rs:350) —
  they are currently discarded; keep them.
- **Executor.** Generalize the fill from two hardcoded levels to a recursion. Today:
  `exec_stmt_impl` fills top uncorrelated slots, `exec_select_top`/`exec_select_with`
  fill top correlated slots. Make `exec_select` (the shared per-arm/per-subplan
  runner, exec/mod.rs:587) itself responsible, before it gathers, for filling ITS
  plan's children: uncorrelated children once, correlated children per row. Each
  level receives a param buffer that already contains the filled slots of its
  ancestors, and extends it with its own children + correlation args. The recursion
  bottoms out at a subplan with no children — today's leaf case.
- **Validate.** `validate_subplans` becomes recursive: the same slot-discipline
  checks (`gather_ok`, `key_parts_ok`) applied at each level against that level's
  `sub_base`, and the outer-arg bounds checked against that level's outer tuple.
- **Footprint.** `compute_footprint` recurses the subplan tree (it already loops
  `subplans` at planner/footprint.rs:86; make it walk children too), unioning all
  tables read at every depth.
- **PLAN_FORMAT.** **Bump required** — `SubPlan` grows `subplans` + `sub_base`, a
  wire-layout change to an existing record.

### 3.3 The hard part, precisely: correlation to a MIDDLE scope

Nesting depth alone is mechanical once `SubPlan` is recursive. The genuinely hard
case is a reference from the INNERMOST query to a MIDDLE (or the outermost) scope,
skipping the level in between:

```sql
SELECT * FROM a
WHERE EXISTS (SELECT 1 FROM b WHERE b.x = a.k          -- inner→outer (skips nothing yet)
              AND EXISTS (SELECT 1 FROM c WHERE c.y = a.k AND c.z = b.w));
--                                              ^^^^ innermost → OUTERMOST (a), skipping b
```

The innermost's `c.y = a.k` correlates to `a` (two levels up), and `c.z = b.w` to `b`
(one level up). `Correlate` today resolves a name against exactly one outer scope
(`self.outer_scope`, subquery.rs:435). Mid-scope correlation needs a STACK of scopes,
and each resolved reference must become a correlation arg threaded down through EVERY
intervening level: `a.k` becomes an `outer_arg` of the middle subplan (b) that the
middle does not itself use except to PASS to its child (c). So the middle's
`outer_args` and its children's `outer_args` must be reconciled: the innermost's arg
that names `a.k` is filled from the middle's row buffer, which in turn received `a.k`
as one of ITS correlation args from the top. That pass-through plumbing across levels
is the reserved-slot reconciliation at its worst — the middle level allocates a
"transit" correlation slot it only forwards.

### 3.4 Staged plan

**Stage 1 — uncorrelated nested (no cross-level correlation at all).** Admit nesting
only when every subplan in the tree is uncorrelated (`outer_args` empty) OR correlates
only to its own immediate parent's row via the existing single-scope `Correlate`.
The simplest slice: an UNCORRELATED subquery nested inside another subquery, e.g.
`WHERE x IN (SELECT id FROM b WHERE b.v = (SELECT max(v) FROM c))`. The innermost
`(SELECT max(v) FROM c)` is uncorrelated → filled once; the middle is then uncorrelated
w.r.t. the top and also filled once. With the recursive `SubPlan` + the recursive
exec fill, both are computed bottom-up before the middle's own execution. This needs
the recursive structure and fill ordering, but NOT the scope stack — `Correlate` is
untouched. It removes the refusal for a large fraction of the corpus (nested
uncorrelated subqueries are common) at moderate effort.

**Stage 2 — nested correlated to the IMMEDIATE parent.** Allow the inner to correlate
to the row of the subquery that directly encloses it (one level). `Correlate`'s outer
scope becomes the immediately-enclosing inner scope instead of the top scope; the
per-row fill recursion already passes the parent's row down. No scope stack yet —
each level sees exactly one outer.

**Stage 3 — correlation to a middle/outer scope (the scope stack).** Give `Correlate`
a Vec of enclosing scopes (innermost-first per SQL's resolution rule) and the
transit-slot plumbing of 3.3. This is the expensive, review-heavy stage; defer it
until stages 1–2 have shipped and the corpus shows the remaining refusals are
mid-scope.

### 3.5 Refusal boundaries that stay throughout

- Subqueries in HAVING (subquery.rs:142), in GROUP BY (structural), and in a JOIN's
  ON (subquery.rs:115) remain refused — nesting does not change where a subquery can
  legally appear, only that one may contain another.
- `current_setting()` inside a subquery (subquery.rs:353) stays refused until the
  context-slot layout is reconciled across levels (the same reason nesting was
  refused; solved by the same recursive-slot work, but scope-crept out of stage 1).
- The `MAX_SUBPLANS = 16` ceiling (plan/mod.rs:222) becomes a bound on the TOTAL tree
  size, checked recursively in decode/validate to keep the DoS bound.

---

## Ranking (value/effort, for sqlite parity + speed)

1. **Aggregate over a correlated filter (§1) — highest.** It is a PARITY gap: mpedb
   REFUSES `SELECT count(*) … WHERE EXISTS(…)`, a common shape sqlite answers.
   Effort is low: the split, the memo, and the per-row filter all already exist; the
   work is threading `post_filter` into `plan_aggregate_select`, extending
   `exec_aggregate` with the (shared) per-row correlated loop, and relaxing one
   validate rule. No format bump. Ship first.

2. **Nested subqueries (§3), stage 1 — medium.** Also a PARITY gap
   (`plan_one` refuses outright), and nested subqueries are frequent in the corpus.
   Stage 1 (uncorrelated nested) is a bounded, high-coverage slice needing the
   recursive `SubPlan` + recursive exec fill (one format bump), without the scope
   stack. Stages 2–3 add correlation depth incrementally. Second.

3. **Build-once hash semi-join (§2) — lowest.** Pure SPEED, no new queries answered;
   the asymptotic win exists only for UNINDEXED inner correlation columns and is
   already captured by declaring an index — the idiomatic mpedb answer. Highest
   effort (new plan field + format bump + planner detection + exec hash primitive +
   IN/NULL 3VL). Defer behind the cost broker (DESIGN-MPEE-OPT §5), which is what
   should decide hash-build vs. index-probe vs. per-row in the first place.
