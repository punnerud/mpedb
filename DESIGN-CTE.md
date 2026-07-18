# DESIGN-CTE — non-recursive `WITH` common table expressions

**Status: DESIGN ONLY (not implemented).** Target: lift the `WITH (CTEs) ❌`
row in COMPAT.md to a bounded ✅ for the non-recursive case.

## 0. Key insight — a CTE is a transient view

A non-recursive CTE `WITH c AS (SELECT …) <main>` is a **named inline view whose
scope is one statement**. mpedb already flattens views and derived tables at bind
time in `crate::view` (`inline_views` → `flatten_select`/`flatten_derived`), and
the view catalog is nothing but a `HashMap<String, String>` of *name → SELECT
source text* (`ViewCatalog`). So a CTE needs **no new flattening machinery**:

1. Parse the `WITH` prefix into a list of `(name, body_source_text)` pairs — the
   exact shape a `ViewCatalog` entry already has (this is how `CREATE VIEW`
   stores a body: raw source text, re-parsed at reference time).
2. Merge those pairs into a **transient, cloned** `ViewCatalog` for this one
   statement (a CTE *shadows/extends* the persistent views), and run the existing
   `inline_views` over the main statement. A `FROM c` reference is spliced onto
   `c`'s base table by the identical code path a `FROM viewname` reference uses.

No planner change. No plan-format change. `inline_views` clears `from_derived`
and splices tables **before** `planner::plan_statement` runs, so the planner,
`SelectPlan`, the compiled-plan bytes, and the executor never see a CTE. The CTE
lives and dies in the binder-facing rewrite pass, exactly like a view.

**PLAN_FORMAT change required: NONE.** (This is bind-time flattening; the plan
byte format and its blake3 hashing are untouched.)

---

## 1. Parse — the `WITH` prefix

### 1.1 Grammar

```
WITH [RECURSIVE] cte [, cte]*  <main-statement>
cte  ::=  name [ ( col [, col]* ) ] AS ( <select> )
```

`<main-statement>` is an ordinary `SELECT` / compound `SELECT` / `INSERT … SELECT`
(and, for free, whatever `statement()` already dispatches). The `WITH` prefix
sits **before** the main statement and **after** an optional leading `EXPLAIN`
(`EXPLAIN WITH … SELECT …` is valid; `WITH … EXPLAIN` is not and stays a parse
error).

### 1.2 `WITH` and `RECURSIVE` are positional words, not new keywords

Follow the established house pattern: `UNION`/`EXCEPT`/`INTERSECT`,
`LEFT`/`RIGHT`/`FULL`/`CROSS`/`NATURAL`/`OUTER`, and `ALL` are all recognized
positionally via `Parser::eat_word` (a case-insensitive bare-identifier match),
**not** added to the `Kw` enum / `keyword()` table in `token.rs`. Do the same for
`WITH` and `RECURSIVE`:

- Zero blast radius on existing SQL. A table or column literally named `with`
  keeps working — `SELECT with FROM t`, `FROM with` — because `WITH` is only
  recognized in **one position**: the very start of a statement (or right after
  `EXPLAIN`). A bare identifier there is already a hard parse error today
  (`statement()` accepts only the statement keywords), so recognizing `with`
  positionally steals nothing.
- No `token.rs` edit at all.

### 1.3 Where in the parser

`crates/mpedb-sql/src/parser/mod.rs`.

Add one method and one thin entry point; leave `parse_statement`'s signature
alone (see §2 for the ripple argument).

```rust
/// Parse an optional leading `WITH [RECURSIVE] …` clause. Returns the CTE
/// bodies as (name, body-source-text) pairs in declaration order, or an empty
/// vec when there is no WITH. RECURSIVE and a per-CTE column list are refused.
fn with_prefix(&mut self) -> Result<Vec<(String, String)>> {
    if !self.eat_word("WITH") {
        return Ok(Vec::new());
    }
    if self.eat_word("RECURSIVE") {
        return Err(self.err_here(
            "WITH RECURSIVE is not supported yet (non-recursive CTEs only)",
        ));
    }
    let mut ctes = Vec::new();
    loop {
        let name = self.ident("CTE name after WITH")?;
        // `WITH c(x, y) AS (…)` — column-list aliasing. Refused (see §3.3);
        // mirror `CREATE VIEW v(a,b)`, which is refused the same way.
        if self.peek() == Some(&Tok::LParen) {
            return Err(self.err_here(
                "WITH column-list aliasing `name(col, …)` is not supported yet",
            ));
        }
        self.expect_kw(Kw::As, "AS after the CTE name")?;
        self.expect(&Tok::LParen, "`(` to open the CTE body")?;
        // Capture the body as SOURCE TEXT between the parens, by paren-counting
        // over the token stream — do NOT sub-parse it here (see §1.4).
        let start = self.here();
        let mut depth = 1usize;
        let end;
        loop {
            match self.peek() {
                Some(Tok::LParen) => { depth += 1; self.advance(); }
                Some(Tok::RParen) => {
                    depth -= 1;
                    if depth == 0 { end = self.here(); break; }
                    self.advance();
                }
                None => return Err(self.err_here("unclosed `(` in WITH clause")),
                _ => { self.advance(); }
            }
        }
        let body = self.src[start..end].trim().to_string();
        if body.is_empty() {
            return Err(self.err_here("empty CTE body"));
        }
        self.expect(&Tok::RParen, "`)` to close the CTE body")?;
        ctes.push((name, body));
        if !self.eat(&Tok::Comma) {
            break;
        }
    }
    Ok(ctes)
}
```

### 1.4 Capture the body by paren-counting, NOT by sub-parsing

The body text is recovered by slicing `self.src[start..end]`, where `start` is
the byte offset of the first token after `(` and `end` is the byte offset of the
matching `)` — exactly the technique `parser::ddl::parse_create_view` already
uses to capture a view body (`self.src[start..]`).

Paren-counting walks the **already-tokenized** stream, so a `)` inside a string
literal is a `Tok::Str`, never a `Tok::RParen` — string contents can never
miscount the nesting.

**Why not call `select_stmt()` to consume the body?** Because sub-parsing the
body inside the outer parser would mutate the outer parser's parameter state
(`max_params`, `next_question`, `style`) — a `$n` or `?` inside a CTE body would
pollute the main statement's parameter count and `?` numbering. Paren-counting
touches none of that: it `advance()`s over body tokens without ever routing a
`?`/`$n` through `primary()`, so the main statement's parameters are numbered as
if the CTE body were not there. (CTE bodies must be parameter-free anyway — that
is enforced at reference time, §3.2 — but the counter must stay clean even while
producing that refusal.)

The body is re-parsed later, once, when it is referenced — by the existing
`flatten_select` view path (`parse_statement(view_src)`), which is where all body
validation happens. Capturing raw text keeps a single validation path.

---

## 2. Thread the CTEs into `inline_views` — minimal ripple

### 2.1 The signature problem

`parse_statement` returns `(Stmt, bool, u16)` and has ~30 call sites (mostly unit
tests, plus `view.rs` re-parsing a view body). Widening that tuple ripples to all
of them. Avoid it.

### 2.2 Add a sibling entry point; keep `parse_statement` intact

In `parser/mod.rs`:

```rust
/// Like `parse_statement`, additionally returning any leading `WITH` CTE
/// bodies (name → source text, declaration order). This is the production
/// compile entry (`prepare_maybe_explain_with_views`); a WITH-less statement
/// yields an empty vec.
pub(crate) fn parse_statement_ctes(
    sql: &str,
) -> Result<(Stmt, bool, u16, Vec<(String, String)>)> {
    let toks = tokenize(sql)?;
    let mut p = Parser::new(sql, toks);
    let is_explain = /* existing EXPLAIN / no-nested-EXPLAIN logic, verbatim */;
    let ctes = p.with_prefix()?;          // NEW: after EXPLAIN, before the stmt
    let stmt = p.statement()?;
    p.eat(&Tok::Semicolon);
    p.expect_eof()?;
    let n_params = p.n_params()?;
    Ok((stmt, is_explain, n_params, ctes))
}

/// Unchanged 3-tuple entry, for callers that do not thread a CTE scope
/// (view-body re-parse, tests). A `WITH` here is refused rather than silently
/// dropped — those callers cannot resolve CTE references.
pub(crate) fn parse_statement(sql: &str) -> Result<(Stmt, bool, u16)> {
    let (stmt, is_explain, n_params, ctes) = parse_statement_ctes(sql)?;
    if !ctes.is_empty() {
        return Err(Error::Parse {
            pos: 0,
            msg: "WITH is not supported in this context".into(),
        });
    }
    Ok((stmt, is_explain, n_params))
}
```

Ripple is then exactly:
- `parser/mod.rs`: `+with_prefix`, `+parse_statement_ctes`; `parse_statement`
  delegates. **No test call site changes.**
- `lib.rs`: one call site switches from `parse_statement` to
  `parse_statement_ctes` (§2.3).
- `view.rs`: **no change** to its `parse_statement(view_src)` call — a view body
  can never contain a top-level `WITH` (a `CREATE VIEW … AS WITH … SELECT` is now
  cleanly refused at reference time via the guard above, which is correct: CTE
  bodies inside a stored view are out of scope).

### 2.3 Build the transient scope and reuse `inline_views`

`crates/mpedb-sql/src/lib.rs`, `prepare_maybe_explain_with_views` — today:

```rust
let (mut stmt, is_explain, n_params) = parser::parse_statement(sql)?;
view::inline_views(&mut stmt, views)?;
let plan = planner::plan_statement(&stmt, schema, n_params, catalog)?;
Ok((plan, is_explain))
```

becomes:

```rust
let (mut stmt, is_explain, n_params, ctes) = parser::parse_statement_ctes(sql)?;
if ctes.is_empty() {
    view::inline_views(&mut stmt, views)?;                 // unchanged fast path
} else {
    // A CTE shadows/extends the persistent views for THIS statement only.
    let mut scope = views.clone();
    for (name, body) in ctes {
        scope.insert(name, body);                          // CTE wins on a clash
    }
    view::inline_views(&mut stmt, &scope)?;
}
let plan = planner::plan_statement(&stmt, schema, n_params, catalog)?;
Ok((plan, is_explain))
```

The clone is paid only when a statement actually has a `WITH`, and compilation
happens once per distinct SQL string (results are content-hash cached). A CTE
name equal to a persistent view name shadows the view — standard SQL semantics —
because `insert` overwrites the map entry.

### 2.4 `INSERT … SELECT` from a CTE — one small `inline_views` addition

`inline_views` today recurses into `Stmt::Select` and each `Stmt::Compound` arm,
but for `Stmt::Insert` it only checks the **write target**
(`refuse_view_target(&i.table, …)`) and never descends into `i.select`. So
`WITH c AS (…) INSERT INTO t SELECT … FROM c` would leave `c` unresolved and the
planner would reject `c` as an unknown table. (Note: `INSERT INTO t SELECT … FROM
someview` has this same pre-existing gap.)

Fix, in `view.rs`, the `Stmt::Insert` arm:

```rust
Stmt::Insert(i) => {
    refuse_view_target(&i.table, views, "INSERT")?;
    if let Some(sel) = &mut i.select {
        flatten_select(sel, views, 0)?;   // resolve CTE/view refs in the source
    }
    Ok(())
}
```

This keeps the CTE-as-write-**target** refusal (§4) while allowing a CTE as the
**source** of an `INSERT … SELECT`, and incidentally closes the same gap for
views. `UPDATE`/`DELETE` have no source `SELECT`; a CTE referenced only inside an
`UPDATE`/`DELETE` `WHERE` subquery is out of scope for v1 (their arms may descend
into `where_clause` via `flatten_expr` as an optional follow-up, but the corpus
weight is in `INSERT … SELECT`).

---

## 3. Refusal boundaries (never a wrong answer)

Every refusal below is a clean `Error::Bind`/`Error::Parse` message, not a silent
divergence.

### 3.1 Non-flattenable bodies — inherited from views, for free

A CTE body that is not a simple single-table projection/filter
(aggregate / JOIN / DISTINCT / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET /
renamed-or-computed projection / a compound `UNION …` body / a FROM-less body) is
refused by the **existing** `check_simple` at reference time. Because
`flatten_select` passes the reference name (`tname` = the CTE name) into
`check_simple`, the message already names the CTE:

> `` `c` uses a JOIN, which is not supported yet (only a single-table
> projection/filter source can be flattened) ``

This is the identical boundary the task specifies and the identical one views and
derived tables already enforce — zero new refusal code.

### 3.2 CTE body using parameters — refused

`flatten_select` already refuses a referenced body whose re-parse yields
`n_params != 0` (`view `c` body must not use parameters`). A CTE inherits this.

### 3.3 `RECURSIVE` and column-list aliasing — refused at parse

- `WITH RECURSIVE …` → refused in `with_prefix` (§1.3), early and clearly. A
  recursive self-reference would otherwise resolve against the flat scope and
  recurse until the `MAX_VIEW_DEPTH` guard fires with a vaguer message; refusing
  on the `RECURSIVE` keyword is cleaner.
- `WITH c(x, y) AS (…)` — column-list aliasing → refused in `with_prefix`.
  **Feasibility assessment:** supporting it requires **positional projection
  remapping** — rewriting every outer reference to an exposed name (`x`, `y`)
  back to the body's base column names (`a`, `b`). The view flattener was built
  specifically to *avoid* projection remapping: its correctness argument is
  "exposed column name == base column name, so the outer query needs no
  rewriting" (see `view.rs` header and `check_simple`, which requires bare,
  un-aliased body columns). A positional rename is exactly the class of change
  where getting the mapping wrong yields a **silently wrong column** — the one
  outcome the project forbids. It is deliberately deferred, matching the existing
  `CREATE VIEW v(a,b)` refusal (`parser/ddl.rs`). It becomes feasible only once
  the flattener grows a validated projection-remap pass; that is out of v1 scope.

### 3.4 Qualified references and aliasing a CTE reference — inherited view limits

Because a CTE reference is spliced by the **view** path, it inherits two view
limitations, and these are the most impactful known gaps:

- **Qualified references** (`SELECT c.x FROM c`) do **not** resolve. The view
  splice sets `s.table = <base>` with **no alias**, so the qualifier `c` no
  longer names a table in scope and the binder rejects it. Only **unqualified**
  references work: `WITH c AS (SELECT id, a FROM t) SELECT id FROM c WHERE a > 5`.
  (The derived-table path *does* support `d.col`, because `flatten_derived` keeps
  the alias — see the enhancement in §5.)
- **Aliasing the reference** (`FROM c AS x`) is refused (`aliasing a view (`c`)
  is not supported yet`).

These are consistent with today's view behavior and never produce a wrong answer
(they refuse or fail to bind). §5 describes how to lift them.

### 3.5 Name cycles are bounded, not infinite

A CTE scope is a single flat map, so a reference cycle
(`WITH a AS (SELECT * FROM b), b AS (SELECT * FROM a) …`, or a self-reference
`WITH a AS (SELECT * FROM a) …`) recurses through `flatten_select` until the
existing `MAX_VIEW_DEPTH = 16` guard fires (`view nesting too deep (recursive
view?)`). It is refused, never a hang — the same protection views already have.

---

## 4. Non-goals (v1)

- **Recursive CTEs** (`WITH RECURSIVE`) — refused at parse (§3.3). A fixpoint
  evaluator is a separate, much larger feature.
- **A CTE as a write target** (`WITH c AS (…) INSERT INTO c …` /
  `UPDATE c …` / `DELETE FROM c …`) — refused. `refuse_view_target` already
  rejects a view/derived write target; a CTE name in target position is not in
  the persistent view catalog but is unknown to the schema, so it fails to bind.
  For a clearer message, `refuse_view_target` may additionally consult the
  transient scope, but the default failure is already safe. (A CTE as an
  `INSERT … SELECT` **source** *is* supported — §2.4.)
- **Correlated / `LATERAL` CTEs** — a CTE that references the outer row. Out of
  scope, as correlated derived tables and correlated-subquery-in-aggregate
  already are.
- **`MATERIALIZED` / `NOT MATERIALIZED` hints** — ignored/unsupported; the v1
  model is pure inlining, which for a parameter-free simple projection/filter body
  is semantically identical to materialization (no aggregation, ordering, or side
  effects to make one-vs-many evaluations observable).

---

## 5. Correctness argument

1. **The plan surface is provably unchanged.** `inline_views` runs entirely
   before `planner::plan_statement`; it splices CTE/view/derived references into
   base-table reads and clears `from_derived`. The planner, `SelectPlan`, the
   serialized plan bytes, and the blake3 plan hash never observe a CTE. Therefore
   no `PLAN_FORMAT` bump, and every existing plan/decoder/executor invariant is
   untouched by construction.

2. **A CTE reference reduces to a view reference.** After merging the CTE bodies
   into the transient scope, `FROM c` is indistinguishable from `FROM v` for a
   stored view `v` with the same body text. The rewrite, the `SELECT *` column
   expansion, the `WHERE` merge (`merge_where`), and the refusal boundary
   (`check_simple`) are the *same code*, already differentially tested against
   sqlite 3.45 for views (`crates/mpedb/tests/create_view.rs`) and derived tables
   (`crates/mpedb/tests/derived_table.rs`).

3. **Multiple references are safe.** A CTE referenced N times is flattened N
   times — each `FROM c` independently re-parses and splices the body. For a
   parameter-free simple projection/filter body this is semantically identical to
   evaluating the CTE once and reading it N times: there is no aggregation,
   ordering, randomness, or side effect that could differ between an inlined and a
   materialized reading. (This is the same argument that lets the view path
   re-expand a view at every reference.)

4. **Shadowing is correct.** Inserting CTEs into a *clone* of the persistent
   `ViewCatalog` gives them statement-local scope and lets a CTE shadow a
   same-named view — standard SQL. The persistent catalog is never mutated.

5. **Leniencies are safe (never wrong answers), and documented:**
   - *Forward / unordered references.* The flat scope lets CTE `a` reference a
     later CTE `b`, which strict SQL forbids. This is pure name resolution; it
     accepts more than the standard but can never compute a wrong result.
   - *Unused CTEs are not validated.* A defined-but-never-referenced CTE is never
     re-parsed, so a syntactically broken or non-simple **unused** body is
     silently accepted where PostgreSQL would reject it. Since an unused CTE
     cannot affect the result, this is a safe leniency. (If strictness is later
     wanted, eagerly `parse_statement` each captured body once in an isolated
     parser purely for validation — it does not touch the outer parameter state.)
   - *Non-recursive self-shadowing.* `WITH a AS (SELECT * FROM a)` where the inner
     `a` should mean a base table `a` is instead treated as a self-cycle and
     refused by the depth guard (§3.5), rather than reading the base table. This
     obscure shadowing case is refused, not answered wrongly.

6. **Parameter integrity.** Body capture never routes body `$n`/`?` tokens
   through the parameter machinery (§1.4), so the main statement's parameter
   count and `?` numbering are exactly what they would be without the `WITH`.
   Accepted CTE bodies are parameter-free (§3.2).

---

## 6. Files and functions to change (summary)

| File | Change |
|---|---|
| `crates/mpedb-sql/src/parser/mod.rs` | `+ Parser::with_prefix()`; `+ parse_statement_ctes()`; `parse_statement()` delegates to it and refuses a stray `WITH`. No `token.rs` edit (positional words). |
| `crates/mpedb-sql/src/lib.rs` | `prepare_maybe_explain_with_views`: call `parse_statement_ctes`, build the transient scope (clone views + insert CTEs) only when `!ctes.is_empty()`, then reuse `view::inline_views`. |
| `crates/mpedb-sql/src/view.rs` | `Stmt::Insert` arm of `inline_views`: after the target check, `flatten_select` the `i.select` source (enables `INSERT … SELECT` from a CTE/view). No other view change. |
| `COMPAT.md` | Flip `WITH (CTEs) ❌` → ✅ with the bounded-grammar note (non-recursive, simple bodies, unqualified refs). |
| `crates/mpedb/tests/` | New `cte.rs` differential test against sqlite 3.45 (accepted shapes) + refusal assertions (RECURSIVE, column list, JOIN/aggregate body, qualified ref). Modeled on `create_view.rs` / `derived_table.rs`. |

No change to: `token.rs`, `ast.rs`, `binder.rs`, `planner/*`, `plan/*` (no
`PLAN_FORMAT` bump), the executor, or the facade catalog/DDL paths.

---

## 7. Recommended follow-up — lift the qualified-reference limit (§3.4)

The single most useful enhancement (qualified refs `c.x` and reference aliasing
`FROM c AS x` are very common with CTEs) is to splice a *named* source the way
`flatten_derived` already does — **keep the reference name (or explicit alias) as
the spliced base's alias and `rename_qualifier` the body's own qualifiers onto
it** — instead of the view path's "strip the name" splice. Two ways to reach it,
in increasing scope:

- **A (narrow, CTE-only).** Pass the CTE set to `inline_views` *separately* from
  the persistent `ViewCatalog` (not merged), so `flatten_select` can detect a CTE
  reference and route it through the `flatten_derived`-style keep-alias splice,
  while stored views keep their current strip-name splice. Preserves view
  behavior exactly; costs a second parameter on `inline_views`/`flatten_select`.
- **B (unify).** Make the view-splice branch itself keep the reference name as an
  alias (adopting `flatten_derived`'s rename). This grants qualified refs and
  reference-aliasing to **both** CTEs and views. It lightly *extends* view
  behavior (today `SELECT v.col FROM v` fails and `FROM v x` is refused); the
  existing view tests use only unqualified refs and continue to pass, but this is
  a behavior change to a shipped feature and should be gated on its own review.

Recommendation: ship §1–§6 first (truest to "reuse `inline_views`", lowest risk,
all task refusal boundaries met), then take **A** to make CTEs ergonomic without
disturbing views.
