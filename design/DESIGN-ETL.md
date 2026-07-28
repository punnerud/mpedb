# DESIGN-ETL — reversible PySpell ETL: lens pairs, residuals, round-trip verification

**Status: DESIGNED, and stage 1 (the bijective corner) IMPLEMENTED
(2026-07-28)** — `crates/mpedb/src/lens.rs`, `mpedb lens` in the CLI, tests in
`crates/mpedb/tests/lens.rs`. The prior-art groundwork is `design/ETL-BIDI.md`,
whose eleven design commitments bind this document; every section below names the
commitment it discharges. Sections marked **[not built]** are designed here and
built later — the residual format is an eternity promise (commitment 9) and an
eternity promise cannot be designed incrementally, so it is designed now and
written to disk never, until stage 2.

Three things the implementation changed in this document, each because building
it proved the first draft wrong:

- **NaN canonicalises; it is not compared by payload** (§5.1). The first draft
  had the opposite rule, which would have made verification platform-dependent.
- **Collision is diagnosed before a failed round trip** (§4). The other order
  makes the collision arm unreachable.
- **`celsius ⇄ fahrenheit` fails for two independent reasons**, not one (§12.1),
  and the precision claim needed its own floats-only proof to be honest.

The idea (#52): mpedb already stores enough provenance that
PG→mpedb→sqlite3→mpedb→PG is a round trip. Generalise it to user-defined ETL via
PySpell: register function PAIRS that go both ways, compose them into pipelines,
and let the database store what is needed to reverse them — so that "loss" of
data can be undone by running the pipeline backwards.

## 1. Scope, and what is deliberately not here

**In scope.** A *pair* of stored PySpell functions declared to be each other's
inverse, a *classification* saying how exactly that is meant, engine-side
*verification* of the claim, and — from stage 2 — the residual that makes
non-bijective pairs invertible plus the lineage that says which residual belongs
to which artifact.

**Not in scope, on purpose.** No new SQL syntax. The forward function of a pair
is already callable from SQL as an ordinary stored function (`spellfn.rs`); a
pair adds a *contract over* two existing functions, not a new execution path.
Nothing in this document changes how a query is planned or run. #53 (the daemon
model: pairs driven by triggers / stream ETL) is a separate task and is not
designed here.

**The substrate is already shipped and is reused, not rebuilt:**

| exists | where |
|---|---|
| content-addressed IR storage `funch/<hash>`, name binding `func/<name>` | `crates/mpedb/src/spellfn.rs` |
| the whole DDL tail: `bump_schema_gen` → commit → `cache.clear()` → `reload_schema_from_catalog` | spellfn.rs:141-164 |
| bounded, deterministic execution: `FN_BUDGET`, no db calls, `RefuseDb` | spellfn.rs:37,76-91 |
| content hash of a definition, carried in a plan's const pool | `mpedb_spell::ProcHash`, `Op::SpellCall` |
| blake3, content-hashed plans, `PlanHash` | `mpedb-types` |

**Determinism is free here, and it is the precondition for a round-trip test
meaning anything.** The whole of `mpedb-spell` (4 229 lines) contains no clock
and no randomness: `Op` (ir.rs:86) has no instruction that can answer differently
twice. A forward function that could vary would make GetPut untestable, and every
verification result recorded by this design would be a lie with a timestamp. We
get the property for free because PySpell was built bounded.

## 2. The form (commitment 1)

```
forward(x) → (y, residual)        inverse(y, residual) → x
```

This is exactly the symmetric-lens *complement* formulation
(Hofmann/Pierce/Wagner, POPL 2011): an asymmetric lens with complement has
`get : X → Y×C` and `put : Y×C → X`, and our two signatures are that, renamed.
We inherit two things from choosing a formulation that has already been studied:

1. **Composition is mechanical.** The composite complement of a pipeline is the
   *tuple* of per-stage complements, applied in reverse (Def. 4.2 of the
   symmetric-lens paper; the same property Janus gets from local invertibility).
   No global analysis is needed to invert a pipeline — reverse the stage list and
   feed each stage its own residual.
2. **Residual representations are only equal up to equivalence.** `(j;k);ℓ` and
   `j;(k;ℓ)` are not the same lens, because their complements are structured
   differently. **Therefore: compare pipelines on observable round-trip
   behaviour, never on residual bytes or a residual hash.** Any future
   optimisation that wants to say "these two pipelines are the same" must say it
   by running them, not by hashing their residuals.

## 3. Classification is declared, never inferred (commitment 2)

Every pair carries exactly one class, stated by whoever registers it:

| class | contract | residual |
|---|---|---|
| `Bijective` | `inverse(forward(x)) == x` for every `x` in the declared domain | ∅ |
| `Residual` | `forward(x) → (y, r)`, `inverse(y, r) → x` | typed, stored |
| `Lossy` | `x` is **not** recoverable from `y` and anything the pair stores | n/a — source retention is mandatory |

The class is *declared* by the registrant and *verified* by the engine. A pair
declaring `Bijective` that is not bijective is **refused at registration**, with
a counter-example named. This is the JPEG XL rule: the encoder returns
`JXL_ENC_ERR_JBRD` and refuses input it cannot round-trip, rather than emitting a
pair that does not invert. Cambria died in the undeclared grey zone; there is no
grey zone here.

`Lossy` is not a lens and is not pretending to be one. It is the honest escape
hatch: the pair declares that it cannot invert, and the engine's contract becomes
"the source stays". A run that uses a `Lossy` pair and asks to delete its source
is refused. Refusal fallback = full source retention (commitment 2).

## 4. The laws that are tested (commitment 3)

Two, and only two:

- **GetPut analogue:** `inverse(forward(x)) == x`.
- **PutRes:** `forward(inverse(y, r)) == (y, r)` — re-forwarding must regenerate
  the *same* residual. This catches residual drift, which plain
  `inverse(forward(x)) == x` cannot see. For `Bijective` pairs, where `r` is ∅,
  PutRes degenerates to `forward(inverse(y)) == y` and is still worth testing:
  it is the PutGet direction, and it is what catches a pair whose forward is
  injective but not surjective onto the declared output domain.

**A collision is diagnosed before a failed round trip, and the order is
load-bearing.** With a deterministic inverse, two inputs that forward to the same
value *always* also show up as a failed round trip on the second of them — so
checking the round trip first makes the collision arm unreachable dead code and
throws away the better diagnosis. "These two inputs forward to the same value"
says why; "the round trip came back wrong" only says that.

**PutPut is not tested and must never be added.** `put a' (put a c) = put a' c`
was dropped by every practical system in the literature: `map`, `flatten`,
`merge` and conditionals violate it "for reasons that seem pragmatically
unavoidable" (Foster et al., TOPLAS 2007), and requiring it "would prevent
writing many useful transformations" (Boomerang, POPL 2008). Very-well-behaved
is Bancilhon–Spyratos' constant-complement regime, which is the classical
too-restrictive one. A version-counter pair that increments on change satisfies
GetPut and PutGet and violates PutPut — and it is a pair we want to be able to
write.

**Modulo a declared canonizer.** Round-trip equality is tested modulo an optional
per-pair canonizer (the quotient-lens lesson: exact byte equality breaks on
whitespace, field order, encoding trivialities). **The default canonizer is
identity — byte-for-byte.** A pair that needs a weaker equality must say so, and
the canonizer's identity is part of the pair's record, so a re-verification uses
the same one.

**The creation path is declared or refused.** `inverse(y, ∅)` — inverting without
ever having gone forward — needed an explicit `Ω`/`missing`/`create` with
defaults in every framework in the literature. In v1 it is **refused** for
`Residual` pairs and is simply `inverse(y)` for `Bijective` ones, where it is
total by construction. Refusing beats leaving it undefined.

## 5. Verification scales with what you dare delete (commitment 4)

The verification level is a property of the **run**, not of the pair. The same
pair is sampled when the source is kept and exhaustively bit-compared when it is
not.

| level | what runs | when |
|---|---|---|
| `sample` | the pair over a fixed probe corpus (§5.1) | default; registration always does at least this |
| `column` | `sample`, plus every value of a named column | opt-in: `--over table.column` |
| `total` | every artifact round-tripped and compared **before** the commit that deletes the source | **mandatory** when a run deletes its source |

`total` is Dropbox Lepton's discipline: 100 % decode-and-bit-compare before
committing, applied to 16 billion images, and it caught a non-deterministic
buffer overrun that release qualification had let through. Lepton is archived and
those files still have to decode forever. That is the standard for deleting a
source, and it is not negotiable per-pair.

**The evidence from `sample` and `column` is statistical, not universal, and this
document says so in the same breath as it recommends them.** Sparcl bought
totality with a type system and still fell back on runtime assertions; we buy
nothing with types and are honest about the exchange. `mpedb lens list` reports
*what was verified and against what* — sample count and probe-corpus id — never a
bare "verified".

### 5.1 The probe corpus

A fixed, deterministic corpus per `Value` variant, identified by id in the pair's
record so a re-verification is reproducible. Deterministic xorshift, no `rand`
dependency — the project's existing convention. It must contain the edges that
break naive "bijections":

- `int64`: `0`, `±1`, `i64::MIN`, `i64::MAX`, powers of two either side of a
  magnitude boundary
- `float64`: `±0.0` (distinct bit patterns, equal under `==`), NaN, ±∞,
  subnormals, `1e308`, values whose round trip through a scale-and-offset loses
  the low mantissa bits
- `text`: empty, non-ASCII, combining marks, a string that differs from its own
  NFC normalisation
- `blob`: empty, a byte that is not valid UTF-8
- `NULL`

`±0.0` deserves its own sentence: `-0.0 == 0.0` is true and the bits differ, so a
pair can pass a naive equality check and still not be a bijection on the
representation. Round-trip comparison is therefore on **bits for floats**, not on
`==`.

**NaN is the one deliberate exception: every NaN canonicalises to one pattern.**
The first draft of this section said the opposite — that a pair returning a
different NaN payload than it consumed is not bijective — and that is a trap.
NaN payload propagation is not specified by IEEE-754 and genuinely differs
between x86 and ARM, so a payload-sensitive contract would let a pair verify on
Linux and fail on the M3. A verification result that depends on which machine ran
it is worth less than the sliver of strictness it buys. `±0.0` stays exact
because it *is* specified.

The same reasoning rules the random tail of the corpus: it is generated as real
values, not as random bit patterns. Random 64-bit patterns are overwhelmingly
NaN, so a `from_bits` tail would probe the rule above and nothing else.

## 6. The residual is explicit and typed (commitments 5, 6, 7) **[not built in stage 1]**

**Typed per stage.** Implicit garbage accumulates through composition (the RFun
experience). Each `Residual` pair declares the residual's type; it is not a
free-form bag.

**Ancilla is separated from residual.** Read-only stage parameters — configuration,
models, anything known identically in both directions — are declared unchanged
and kept out of the residual accounting (RFun v2/CoreFun, Sparcl's ordinary
arguments). Sparcl's core observation is ours: partial invertibility is the norm
and bijectivity is the special case; realistic pipelines are invertible parts
parameterised by irreversible ones.

**Branch choice is residual data.** A pair with several code paths either puts a
branch tag in the residual, or guarantees disjoint output domains so the output
itself encodes which path ran (RFun's first-match policy). This is an explicit
choice in the pair's declaration, not an inference, and the verifier checks
disjointness when disjointness is claimed.

**Sparcl's pin rule is an invariant (commitment 7):** everything a lossy stage
*reads* of pipeline data must be recoverable from `(output, residual)`. There is
no third place. This is the rule that makes the classification honest rather than
aspirational — it is what a reviewer checks a proposed pair against.

**No minimality is promised anywhere (commitment 6).** Minimal residual is not
computable in general — it is not even decidable whether a function is injective
(Glück/Yokoyama 2023) — and the hard lower bound is the conditional entropy
H(X|Y): a stage that destroys k bits must store ≥ k bits. No clever encoding gets
around that, and a `DROP COLUMN` stage has residual = the column, full stop.
What is offered instead is Bennett pebbling as a **configurable knob**:
checkpoints every k-th stage traded against recomputation time on reverse, with
O(S·log T) as the theoretically favourable midpoint. Git packfiles have shipped
exactly this — delta chains with a depth limit and periodic full snapshots — for
twenty years. It is a knob, never a guarantee.

## 7. Lineage: content hash, never filename (commitment 8) **[not built in stage 1]**

```
(run_id, step_no, source_hash, output_hash, plan_hash, residual_ref, lens_hash)
```

One stable run id per pipeline run (Pachyderm's global-id move) makes multi-step
lineage a single lookup. blake3 and content-hashed plans already exist in mpedb.

**Positional/path-based correlation is not an option we are weighing.** It was "a
show-stopper" already in Boomerang, and every mature tool — DVC, Pachyderm,
lakeFS, MLflow — landed on content hashing. A generated filename prefix is a UX
rendering of a lineage lookup and nothing more. Where an output format allows it,
embed a `DerivedFrom`-style id (the xmpMM pattern) so the link survives the file
leaving mpedb's control. **A reverse that meets an artifact whose hash does not
match the manifest fails explicitly** — "artifact changed outside the pipeline" —
and never silently feeds wrong input.

### 7.1 Lineage is a TABLE, not a sys-keyspace record — and #124 is why

The pair catalog (`lens/<name>`) goes in the sys keyspace because it is *bounded
catalog*: a handful of records, needed at registration and listing time. Lineage
is an *unbounded log*: one row per artifact per step per run, forever.

#124 measured that compilation is O(bytes ever registered in the sys keyspace) —
two full `sys_scan`s per compile. Putting lineage in the sys keyspace would tax
**every SQL compilation in the database** with the entire ETL history, in
perpetuity, and the tax would grow with use. So lineage is an ordinary mpedb
table, created on demand by the first ETL run that needs it, queryable with
ordinary SQL, and indexed on `source_hash` and `run_id`.

This is not a stylistic preference. It is a measured bug in this repository being
paid forward.

## 8. Formats, and the eternity promise (commitment 9)

### 8.1 `lens/<name>` — the pair record, v1, **built in stage 1**

The only format stage 1 writes to disk. Fixed width, version-prefixed, decoded
through a bounds-checked `decode_lens_record → Option`, where a corrupt record
degrades to "that pair is not defined" and never panics — exactly the shape of
`decode_func_record` (spellfn.rs:64-71), and the project rule that every decoder
gets truncation-at-every-offset tests.

```
version        u8    = 1
class          u8    0 = Bijective, 1 = Residual, 2 = Lossy
forward_hash   [u8; 32]     ProcHash of the forward function's IR blob
inverse_hash   [u8; 32]     ProcHash of the inverse function's IR blob
canonizer      u8    0 = identity (byte-for-byte); other ids reserved
probe_corpus   u8    id of the corpus the verification ran against
samples        u32   how many samples passed
verified_gen   u64   schema_gen at which the verification was recorded
residual_ref   u8    = 0 in v1 (no residual); reserved for stage 2
```

`residual_ref` and the reserved canonizer ids are present in v1 **on purpose**:
stage 2 must be able to add residuals without changing the record's shape.

**The pair is pinned by CONTENT HASH, not by name**, and this is inherited, not
invented: DESIGN-TRIGGERS §5.1 planned to pin procedures by name and shipped
pinning by hash, because a name binding can diverge between attached processes
while an immutable `proch/<hash>` blob cannot. The same argument applies here with
an extra edge — the *verification* was performed against two specific blobs.
Redefining `celsius_to_f` with `mpedb fn define` therefore does **not** silently
change what the pair means; the pair still names the blobs it was verified
against. Re-register the pair to rebind, and it is re-verified when you do. A
verification that could be invalidated by an unrelated `fn define` would be
worthless.

### 8.2 The residual envelope, v1 — designed, **not written in stage 1**

```
version   u8
kind      u8      what the residual is (typed per pair)
len       u32     byte length of the payload
payload   [u8; len]
```

Versioned from day one; the decoder is minimal, bounds-checked, `Corrupt`-never-
panic (the mpedb rule applies double here). **A new version of a pair that cannot
consume old residuals is a NEW pair with a new hash** — the upcasting rule from
event sourcing. Content addressing gives us this for free: the old blobs stay,
and anything pinned to them keeps working.

The pipeline's own definition format is versioned for the same reason. Cambria's
prototype became unusable as a system of record because the *tool's* format
drifted, not because the data did.

## 9. When NOT to build an inverse (commitment 10)

A reversible pair earns its disk space when **the source is going to be deleted**
(the Lepton case) or when **reverse must be O(1) per artifact instead of
O(replay)**. Otherwise, do not build one:

- The mirror layer's CDC is *already* source retention. Replay is free there.
- Run-cache idempotence falls out for free from `(input_hash, plan_hash) →
  output_hash` (the DVC move) and needs no inverse at all.

This section exists so that the answer to "should this be a lens pair?" is
usually **no**, and the design says so in its own voice. Everything in §6 and §7
is machinery you should be talked out of needing.

## 10. Narrow contract, dynamic enforcement (commitment 11)

The only deployed lens system is Augeas, and it won by narrowing the domain,
keeping an original-preserving `put`, and *not* proving the laws formally per
lens. Everything with stronger static ambitions stayed a research language —
Boomerang, Links, Sparcl — and Links took twelve years to get a first
implementation of relational lenses at all.

mpedb sits on the correct side of that line: a narrow contract (a function pair
plus a residual) enforced by property testing on real data. We do not build a
type system that proves totality. We check at runtime, we record what we checked,
and we say out loud that the evidence is statistical. Even Sparcl ended up with
runtime assertions, and Sparcl trusts its `lift` pairs blindly — property testing
them is operationally stronger than that, and weaker than a proof. Both halves of
that sentence belong in the user-facing documentation.

## 11. Staging

**Stage 1 — the bijective corner. Built in this round.**

Deliberately the narrowest thing that makes "reversible processes" real:
**1→1 scalar bijections**. No residual, no lineage, no new blob storage, no new
SQL surface. The arity limit is not arbitrary — a stored function *must* return a
scalar (spellfn.rs:105-109), so `{first, last} → full` cannot even be expressed
without a residual. The tuple case belongs to stage 2, and that is correct: it
needs the residual anyway.

- `crates/mpedb/src/lens.rs`, built like `spellfn.rs`: `NS_LENS = "lens"`,
  `create_lens` / `verify_lens` / `list_lenses` / `drop_lens`, the same DDL tail
  including `bump_schema_gen`, `drop_lens` tearing down only the name binding
  (blobs are content-addressed and may be pinned elsewhere).
- Both functions must already exist as stored functions (`mpedb fn define`).
  Zero new compilation path.
- `mpedb lens define|verify|list|drop`, mirrored off `cmd_fn`
  (`crates/mpedb-cli/src/main.rs:390`).

The `schema_gen` bump costs 8.4 ms (measured in #167) and is strictly unnecessary
in stage 1, since pairs are not a compilation input yet. It is taken anyway: it
becomes required the moment they are, and the alternative is a silent staleness
bug on that day. This is a rare admin operation.

**Stage 2 — residual, lineage, tuple pairs. [not built]** §6, §7, §8.2.

**Stage 3 — the domains. [not built]** Version storage as base+diff (binds #50
and #52: `forward(v1, edit) → v2` with the diff as residual, verified
`apply(v1, diff) == v2` byte-identically at ingest); container round-trip
(pristine-tar's pattern for .gz/.zip, which has been in Debian infrastructure for
~20 years); mpedbfs presentation (#54).

**#53 — the daemon model** (pairs driven by triggers / stream ETL) is a separate
task, designed separately, and gated on stage 2 existing.

## 12. Adversarial review — the attack list

The review this document must survive, written before the review so it cannot be
retrofitted. `lens/<name>` is a persisted format, which is what the project's
calibration rule says demands a full pass.

1. **The verifier is a rubber stamp.** If `create_lens` accepts a pair that is
   not bijective, everything else in this document is decoration. The concrete
   test: `celsius ⇄ fahrenheit`, which everyone's intuition calls reversible.
   It fails for **two independent reasons**, and saying which is which matters:

   - *Precision.* `(c*9/5+32-32)*5/9 != c` for a large majority of `c` in
     float64. This is the claim the design rests on, so it is proven on its own
     — a floats-only unit test, with the type change held out of the picture, so
     precision is the sole remaining explanation. `-40.0` survives, because it is
     the fixed point where the two scales meet; the pair is not uniformly broken,
     which is exactly why it is plausible enough to need refusing.
   - *Type.* In mpedb's value domain the pair does not even preserve the type:
     `Int(0) → Float(32.0) → Float(0.0)`. This is what the full probe corpus
     reports first, since integers come before floats in it.

   Stage 1 is not done until the pair is **refused** with a named
   counter-example, and registers only when reclassified.
2. **The probe corpus misses the edge that matters.** `±0.0`, NaN payloads,
   `i64::MIN`, and non-NFC text are the documented traps; comparison is on bits
   for floats, not `==`.
3. **A pair verified, then invalidated behind its back.** Covered by hashing, not
   naming (§8.1) — but check that `drop_function` on a function a pair points to
   leaves the pair pointing at a still-readable blob, and that `list_lenses` says
   so rather than reporting a healthy pair.
4. **Cross-process staleness.** Process A registers a pair; process B must see it.
   The `schema_gen` bump is the mechanism; the test is the proof.
5. **Truncation.** Every offset of `lens/<name>` → `Corrupt` or "not defined",
   never a panic.
6. **Unbounded growth in the sys keyspace.** §7.1 — lineage must not go there,
   and #124 is the measurement that says why.
7. **The suites moved.** Stage 1 adds nothing to the SQL surface, so
   sqllogictest, Django (824/831) and CPython `test_sqlite3` (440/466) must land
   on exactly the same numbers. If they move, something is coupled that should
   not be.
8. **The residual format was written before it was designed.** Stage 1 writes no
   residual at all. If a stage-1 patch persists a residual byte anywhere, it has
   broken commitment 9 by accident.

Where each is discharged, so the list is a checklist and not a wish: 1, 2 and 3
in `crates/mpedb/tests/lens.rs` plus the floats-only unit test in `lens.rs`;
4 and 5 likewise (`another_process_sees_the_pair_through_the_generation_bump`,
`truncation_at_every_offset_is_none_not_panic`); 6 by reading — every production
`sys_scan` is a prefix-bounded range, and the only two unbounded `sys_scan()`
call sites in the tree are both inside `#[cfg(test)]`, so a bounded `lens`
catalog costs nothing per compile; 7 by running the suites; 8 by the record
layout in §8.1, whose `residual_ref` byte is reserved and asserted zero.

Attack 3 is worth its own sentence, because the answer is not what the phrasing
suggests: dropping the *function* leaves the pair perfectly healthy.
`drop_function` removes a name binding, and the pair does not hold a name — it
holds the blob's hash, and content-addressed blobs are never deleted. Tested
(`dropping_the_function_name_does_not_break_the_pair`), and it is the same
property from the other side as the redefinition test: what the pair points at
cannot be changed by anything that happens to a name.
