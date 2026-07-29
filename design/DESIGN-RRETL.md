# DESIGN-RRETL — reversible PySpell ETL: lens pairs, residuals, round-trip verification

> **Naming**: the feature is **rRETL** (doubled R), because "RETL"/"rETL" is
> taken twice over — *Reverse ETL* (warehouse→SaaS sync) in modern data
> engineering, and Oracle's *Retail Extract, Transform, and Load*. rRETL is
> neither; the name collision is why the 2026-07-28 rename happened
> (day-old feature, free break).

**Status: DESIGNED; stage 1 (the bijective corner), stage 2 (residual
pairs + `rretl apply|revert|putback|log` + lineage) and stage 3's storage
domains (blob versioning `rretl put|get|versions`, zip splice
`rretl pack-in|pack-out|archives` — §8.2–8.4 codecs + `rretl_store`)
IMPLEMENTED 2026-07-28** —
user-facing contract: `PYSPELL-RRETL.md` (the self-contained Python guide) —
`crates/mpedb/src/{lens,rretl,rretl_codec,rretl_store}.rs`, `mpedb lens`/`mpedb
rretl` in the CLI, tests in `crates/mpedb/tests/{lens,rretl,rretl_store}.rs`. The prior-art groundwork is `design/RRETL-BIDI.md`
(including the 2026-07-28 addendum: Jeopardy, Hermes, CRIL, RC 2023–2025,
Sparcl JFP 2024, wire-format practice, lineage practice), whose eleven design
commitments bind this document; every section below names the commitment it
discharges.

Three things stage-1 implementation changed in this document, each because
building it proved the first draft wrong:

- **NaN canonicalises; it is not compared by payload** (§5.1). The first draft
  had the opposite rule, which would have made verification platform-dependent.
- **Collision is diagnosed before a failed round trip** (§4). The other order
  makes the collision arm unreachable.
- **`celsius ⇄ fahrenheit` fails for two independent reasons**, not one (§12.1),
  and the precision claim needed its own floats-only proof to be honest.

Four more from stage-2 design review (a 6-sweep research pass plus an
adversarial check of the build sketch, 2026-07-28):

- **The residual type is DECLARED in the pair record and verified at
  registration** (§8.1). The first sketch stored residuals in an `Any` column
  with no declaration anywhere — exactly the "free-form bag" commitment 5
  forbids. `Any` remains a legal declaration; it must be said, not defaulted.
- **The v1 record's reserved bytes could not absorb stage 2** (§8.1) — a third
  32-byte hash does not ride a u8. v2 changes the shape under the standing
  no-backward-compat rule; v1 records decode as bijective pairs, not as
  "not defined", because degradation-to-undefined was designed for corruption.
- **Scalar residuals ride the row codec; the §8.2 envelope is reserved for
  stage-3 composites** (§8.2) — with the condition that `revert` refuses hard
  when the lineage row is missing.
- **The lineage table records failed runs, and the verification level that
  actually ran** (§7) — every surveyed lineage system treats failures as
  first-class, and §5 already forbade a bare "verified".

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

> **User probes** (`create_lens_with_probes` / the `probes=` kwarg): the
> corpus cannot know a DOMAIN's edge values, and "corpus verification is not
> proof" is exactly the gap where that bites — a pair whose hole the 232
> values cannot see registers cleanly and aborts the first apply that meets
> the hole (fail-safe, but late). Handing the domain's own values over at
> registration moves the refusal to registration time, counter-example named
> as always. Probes harden REGISTRATION only: the record does not carry
> them, so `lens verify` re-checks the built-in corpus — re-register to
> re-check probes.

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

## 6. The residual is explicit and typed (commitments 5, 6, 7) **[stage 2]**

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

## 7. Lineage: content hash, never filename (commitment 8) **[stage 2]**

Two ordinary tables, created through the normal DDL path inside the first ETL
run's own transaction:

```
rretl_lineage   (run_id, step_no) →
              lens_name, forward_hash, rex_hash, inverse_hash,
              tbl, col, source_hash, output_hash, residual_hash, rows,
              verified, outcome, error, ts_micros

rretl_residual  (run_id, pk_enc BLOB) → residual   -- residual column type Any
```

Both tables are created from SPECS with RIGID column types, never from SQL
text: the sqlite-affinity DDL path maps the name `BLOB` to the TYPELESS
column (correct sqlite semantics), and a typeless key column takes neither
point probes nor range bounds — every per-row residual lookup would be a
filter over the whole run. Rigid `Blob` for `pk_enc` is what makes the
lookup a PkPoint and the chunked resume a composite PkRange (#55 phase 2).

One stable run id per pipeline run (Pachyderm's global-id move) makes multi-step
lineage a single lookup. blake3 already exists in mpedb. Field decisions, each
paid for by the 2026-07-28 survey (RRETL-BIDI addendum):

- **`step_no` is a constant 1 in stage 2** — it exists so stage-3 pipelines
  extend the data, not the shape.
- **`plan_hash` is dropped, with a reason**: the unit of ETL execution here is a
  lens pair, not a SQL plan, and the three function hashes ARE the code
  identity — strictly stronger than OpenLineage's name-based job references.
- **`verified` records the level that actually ran** (§5 requires reporting
  "what was verified and against what", never a bare "verified").
- **`outcome` + `error` make failed runs first-class lineage** — every surveyed
  system (OpenLineage START/FAIL/ABORT + errorMessage) treats them so; a table
  that only logs successes cannot answer "why is this column half-stale".
- **`run_id` is a counter, never a content hash** — two runs can produce
  identical bytes and must still be distinguishable; hashes identify artifacts
  and code, the id identifies the execution.
- **The residual table is keyed `(run_id, pk)`**, so one run's residuals are
  addressable as a set (`residual_ref` from the original tuple = the run id)
  and two runs on the same table can never collide. A residual VALUE of NULL is
  legal and distinct from a MISSING row — reading absence as NULL would smuggle
  the refused creation path `inverse(y, ∅)` (§4) back in as a silent wrong
  answer.
- **`residual_hash` is the residual set's OWN at-rest identity**: a chain over
  the persisted `(pk_enc, residual)` rows in `pk_enc` (table-key) order,
  computed by RE-READING them inside the applying transaction, empty for a
  run that wrote none. It exists because the residual rows are not
  user-editable state — edits happen in the column — yet a tampered residual
  can survive BOTH PutRes halves (mag ⇄ sgn: flip a stored sign and
  `forward(inverse(y, r')) == y ∧ rex(x') == r'` both hold), which made
  putback the one door a residual tamper could walk through silently. Now:
  fsck re-hashes EVERY standing run's residuals against it — buried runs and
  runs whose table was since dropped included, closing "buried runs are
  unverifiable at rest" — and revert/putback refuse up front when it no
  longer matches.

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

### 8.1 `lens/<name>` — the pair record, v2 (stage 2; v1 was stage 1's)

Fixed width, version-prefixed, decoded through a bounds-checked
`decode_lens_record → Option`, where a corrupt record degrades to "that pair is
not defined" and never panics — exactly the shape of `decode_func_record`
(spellfn.rs:64-71), and the project rule that every decoder gets
truncation-at-every-offset tests.

```
version        u8    = 2
class          u8    0 = Bijective, 1 = Residual, 2 = Lossy
forward_hash   [u8; 32]     ProcHash of the forward function's IR blob
inverse_hash   [u8; 32]     ProcHash of the inverse function's IR blob
rex_hash       [u8; 32]     ProcHash of the residual extractor; all-zero when
                            the class has no residual (Bijective, Lossy)
residual_type  u8    ColumnType tag of the DECLARED residual type; 0xff = none.
                     Declared by the registrant and VERIFIED against actual
                     rex outputs during registration (commitment 5 — an Any
                     column alone is the free-form bag the commitment forbids;
                     ColumnType::Any is a legal declaration, but it must be
                     SAID, not defaulted into)
branch_policy  u8    0 = residual carries the branch tag; 1 = outputs verified
                     disjoint (first-match) — the §6 choice, explicit per pair
canonizer      u8    0 = identity (byte-for-byte); other ids reserved
probe_corpus   u8    id of the corpus the verification ran against
samples        u32   how many samples passed
verified_gen   u64   schema_gen at which the verification was recorded
```

**v1 records decode into v2 shape with a zero rex hash and no residual type** —
not as "not defined". Degradation-to-undefined was designed for *corruption*,
not deliberate retirement, and stage-1 records exist in this project's own dev
and M3 databases. The v1 claim that reserved bytes would absorb stage 2
("without changing the record's shape") was undersized — a third 32-byte hash
cannot ride a u8 — and the shape change is taken under the standing
no-backward-compat rule, with this paragraph as the record of why.

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

### 8.2 Residual persistence: scalars ride the row codec; the envelope is stage 3

**Amended in stage 2.** Stage 2's residuals are scalars in an `Any` column of an
ordinary table (`rretl_residual`, §7), and they deliberately do NOT get the
envelope below: the row codec already tags the value's type, it inherits exactly
the durability contract of every other row in the file, and the standing rule
that format breaks fund a free migration of the project's own files means the
codec cannot break in a way that loses residual rows without losing the user's
data too. The envelope's version/kind bytes would be redundant bureaucracy on a
scalar.

The condition that makes this acceptable rather than a breach of commitment 9:
a residual's *meaning* lives in its lineage row (which pair version consumes
it — the pinned `inverse_hash` is the upcasting rule discharged by content
addressing), so **`revert` refuses hard when the lineage row is missing**. A
residual whose lineage row was deleted is an uninterpretable scalar, and an
equally explicit error, never a NULL read.

The envelope, stage 3's composite-residual carrier — designed 2026-07-28,
**BUILT the same day** as `crates/mpedb/src/rretl_codec.rs` (envelope + both
payload codecs; the 23-finding adversarial check of the build sketch is
folded in below), with the storage half in `crates/mpedb/src/rretl_store.rs`:

```
kind      u8      ONE dispatch byte — the value IS both the version and the
                  algorithm (version-as-dispatch taken literally, pristine-tar's
                  model: their version number selects the delta program).
                  1 = raw (payload is the full bytes)
                  2 = delta-v1 (payload: §8.3)
                  3 = zip-splice-v1 (payload: §8.4)
                  An unknown kind is refused WITH ITS NUMBER NAMED. The first
                  draft had separate version and kind bytes — two dispatch
                  channels that could disagree, with the refusal rule defined
                  for only one of them; collapsed on review.
len       u32     byte length of the payload (hard 4 GiB ceiling; oversize
                  inputs are refused at put/pack-in with the number named)
payload   [u8; len]
```

Envelopes NEST — the outer transform wraps the inner (pristine-tar's `wrapper`
member), never envelopes smuggled inside an algorithm payload — with a hard
depth cap of 4: a recursive decoder without one turns an adversarial blob of
nested envelopes into a stack overflow, which is a panic, which the decoder
rule forbids.

### 8.3 delta-v1 — the payload of kind 2

Git's packfile delta, simplified: the same two instructions (COPY from the
base ONLY, INSERT literals) that have been frozen in git for ~20 years, with
fixed-width fields instead of flag-packing — a few bytes of bloat per
instruction is the entire cost, and bloat is the only failure mode the
verification stance leaves open.

```
base_len    u64   length the base MUST have (fail-fast identity check)
target_len  u64   exact output length; the decoder pre-walks the instruction
                  stream, checks Σ lengths == target_len and every
                  base_off+len ≤ base_len, and only then allocates ONCE —
                  a corrupt delta claiming a 2^63 copy must die on a bounds
                  check, not in the allocator
instructions:
  0x01  COPY   { base_off u64, len u64 }     from the base, never the target
  0x00  INSERT { len u32, bytes [u8; len] }
```

Base-only COPY is deliberate (VCDIFF's self-referential COPY plus address
caches and code tables is the complexity that makes its decoders nontrivial;
git dropped it all and never needed it back). No RUN instruction: repeated
bytes absent from the base cost literals, and re-adding self-referential copy
to get RLE back would break the trivial-decoder property.

**Only the DECODER is the eternity promise.** The encoder (greedy first-match
over a 16-byte-block index of the base, fixed multiplicative hash into
array-backed chains — never a HashMap, whose seeded iteration order would make
output nondeterministic — chain cap 64 against adversarial repetitive input,
capped match extension, lowest-offset tie-break) may improve freely: every
delta is verified `apply(base, delta) == target` BYTE-IDENTICALLY before
commit, so an encoder bug can bloat, never corrupt. Tests therefore assert
apply-equality and NEVER delta bytes — a byte-asserting test would silently
revoke the encoder's freedom to improve. **If the encoded delta is not smaller
than the full payload, the full payload is stored** — a pathological-case cap,
not a minimality promise (commitment 6: the lower bound is H(X|Y) and no
encoding beats it; CLI output reports actual bytes, never "savings").

### 8.4 zip-splice-v1 — the payload of kind 3

The archive with its member data segments CUT OUT, plus the ordered splice
list — reconstruction re-inserts the stored member bytes, and byte-identity is
a PARTITION INVARIANT rather than a hope: if the cut ranges are disjoint,
in-bounds, and `residual_len + Σ cut_len == file_len`, re-splicing reproduces
the original by construction, whatever the zip contains. (No known system does
this — pristine-tar stores a heuristic delta against a regenerated artifact
and "hopes one of six strategies works"; the splice is deterministic.)

```
file_len   u64    original archive length
count      u32    splice entries
entries    count × { member_no u32, offset u64, len u64 }
                  sorted by OFFSET; member_no is the CENTRAL-DIRECTORY order
                  (the two orders legally differ — appended or name-sorted
                  archives — and conflating them breaks the offset arithmetic)
gap_bytes  [u8]   the original minus the cut segments, verbatim: local headers,
                  data descriptors, inter-member junk, SFX stubs, the central
                  directory — everything that is not member data
```

Member location follows the one hard rule practice converged on: enumerate
from the central directory ONLY (with the SFX base-offset correction), verify
`PK\x03\x04` at each corrected offset, compute the data start from the LOCAL
header's name/extra lengths — the CD's copies legally differ, and using them
is the classic splicer bug — and take the length from the CD's
`compressed_size`. Named refusals, each the point where data location becomes
ambiguous or the partition breaks: overlapping segments (the zip-bomb shape —
covered-span check over data ranges, headers and the CD region), segments out
of bounds, a zip64 sentinel (0xFFFFFFFF/0xFFFF), multi-disk archives,
masked/central-directory-encrypted headers, and a missing local-header
signature at a CD-claimed offset. Traditionally- or AES-encrypted MEMBERS are
fine — their crypto lives inside `compressed_size`, opaque bytes splice like
any others (the fail-safe-per-consumer distinction: locatable-but-unreadable
is safe for round-trip, useless for content indexing, and those are different
consumers).

The decoder is minimal, bounds-checked, `Corrupt`-never-panic (the mpedb rule
applies double here). **A new version of a pair that cannot consume old
residuals is a NEW pair with a new hash** — the upcasting rule from event
sourcing. Content addressing gives us this for free: the old blobs stay, and
anything pinned to them keeps working.

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

**Stage 2 — residual pairs + apply/revert + lineage + PUTBACK. Built.**

**Putback is the design change stage 2 grew after shipping**, and it is the
half of "reversible" that `revert` deliberately refuses: inverting a run whose
transformed column has been EDITED, carrying the edits back. Each surviving
row's current value `y'` becomes `x' = inverse(y', r)` with the run's stored
residual; deleted rows stay deleted (their residuals are discarded — the
deletion IS an edit); rows inserted after the apply are refused for residual
pairs (the creation path `inverse(y, ∅)`, §4) and simply inverted for
bijective ones (their creation path is total by construction, §4). The
verification flips with the oracle: `source_hash` cannot arbitrate an edited
column, so **PutRes becomes the operative law, per row, before commit** —
`forward(x') == y'` and `rex(x') == r`. At registration PutRes was a corpus
tautology and deliberately not run (§4); at putback the source is gone as an
oracle and PutRes is the ONLY thing that can hold. An edit outside the pair's
image fails it with the row and both values named, and the run rolls back.
This is GetPut/PutGet operating as the lens literature intended — the putback
function finally earns the name.

The Residual class becomes real as a TRIPLE of stored functions —
`forward/1`, `rex/1` (the residual extractor: Sparcl's complement as a plain
function), `inverse/2` — all bound by content hash, zero interpreter changes
(`call_spell_fn` is arity-generic). The law verified over the probe corpus is
GetPut: `inverse(forward(x), rex(x)) == x`. PutRes is NOT run over the corpus —
on image pairs it is a tautology given GetPut plus determinism, and running a
tautology and reporting it as an independent law is the bare "verified" §5
forbids; the argument lives here instead of in a test.

The collision key is class-conditional: `Bijective` keys on `y` alone (a
collision IS the defect); `Residual` keys on `blake3(len‖bits(y)‖len‖bits(r))` —
multiple preimages of `y` are the entire point when `r` disambiguates, and the
length framing is load-bearing because unframed concatenation makes
`("a\x04","")` and `("a","\x04")` collide falsely. The Hermes borrow: for
`Bijective` pairs, every forward run asserts the residual is empty — cheap
defence in depth against corpus-blind non-bijectivity.

`mpedb rretl apply <target> <pair> <table>.<col>` transforms a column IN PLACE in
ONE WriteSession: class-gated (`Lossy` refused by name — in-place is source
deletion, commitment 2; `Bijective` writes lineage only), a row the pair refuses
ABORTS the run with the row named (silently skipping would reintroduce Cambria's
grey zone per-row), output type pre-checked against the column's declared type
(type-changing pairs need an `Any` column; ALTER COLUMN does not exist yet).
Verification before commit is total (the source is being deleted) at bounded
memory: `source_hash` = blake3 over PK-ordered `value_bits`-CANONICAL bytes —
never raw storage bits, or legal NaN canonicalisation produces a false
"artifact changed" — and the `inverse(y, r)` stream is re-read inside the same
txn and hashed against it. `revert <run_id>` gates the residual set against
`residual_hash`, then re-hashes the column against `output_hash` (mismatch =
"artifact changed outside the pipeline", hard error); a new apply on a column
with an unreverted run stacks (LIFO). **Every pass STREAMS in chunks**
(`pk > last ORDER BY pk LIMIT n` — the same globally-sorted stream the hash
chains are defined over, resumable because the PK never changes mid-run), so
heap is O(chunk) whatever the table size and the old 1M-row pre-flight cap is
gone with the OOM it guarded against; the collision DIAGNOSTIC is bounded
separately (past the cap, the total verification reports the mismatch with
degraded naming — fail-safe either way). The remaining bound is file space:
one txn's COW pages live in the file-backed map, and DbFull is a
deterministic, named refusal that rolls back whole. Apply is still ONE
transaction and an offline operation — it holds the writer lock for the
duration, and says so.

**Stage 3 — the domains. [BUILT 2026-07-28, B-block]** Version storage as
base+diff: `rretl put/get/versions` (`rretl_store.rs`) — the newest version
stored FULL, the previous newest rewritten as a reverse delta whose base is
exactly the version above, verified byte-identical AS PERSISTED before the
commit, every `FULL_EVERY = 8`th version a permanent full anchor (the Bennett
knob, §11 finding 11), and nothing deleted except by EXPLICIT prune:
`rretl prune <obj> <keep>` deletes the oldest versions keeping the newest
`keep`, chain-safe BY CONSTRUCTION (every delta bases on the version ABOVE
it and get walks downward from an anchor at-or-above the target, so a
deleted prefix orphans nothing; the dangerous prune — an anchor from the
middle — stays impossible because only a contiguous oldest-first prefix
ever goes). The prune is first-class lineage (outcome `pruned`, deleted
range in the note), and `keep = 0` is refused: keeping nothing is a drop. Its three failure disciplines:
a stored full that fails its recorded hash HARD-errors the put (rewriting
would launder corruption and delete the last good copy — finding 12); a
delta that bloats or fails its own round trip keeps the full and the put
still succeeds (finding 13); verification re-reads the persisted rows in the
same txn, so the row codec is inside the trust boundary (finding 14).
Container round-trip: `rretl pack-in/pack-out/archives` — zip SPLICE per
§8.4, members as queryable rows, reconstruction verified byte-identically
before the ingest commits and hash-gated on every pack-out. Both are lineage
(`lens = builtin:delta-v1` / `builtin:zip-splice-v1`, outcomes `versioned` /
`packed` — never `applied`, so revert/putback/stacking ignore them), and
`rretl fsck` re-materializes every version and re-splices every archive.
Still not built from the stage-3 slate: mpedbfs presentation (#54).
Composite PySpell residuals in the envelope remain future work — the
envelope and codecs exist; the pipeline composition does not.

Parked stage-3+ ideas from the 2026-07-28 survey, deliberately NOT stage 2:

- **Residual elision, Jeopardy's move made dynamic**: a residual component that
  is provably derivable from values already present in the output row need not
  be stored. Jeopardy proves it statically (conservatively, one recursion step
  deep); mpedb's version would be a CORPUS-VERIFIED elision — same verification
  stance as everything else here. A size optimisation, never a contract change.
- **Multi-column / tuple pairs**: `{first, last} ⇄ full` needs either tuple
  returns unlocked for lens functions or per-column triples composed by the
  pipeline layer. Wants the envelope.
- **Stacked runs**: the `(run_id, pk)` key makes them representable; LIFO
  revert order and cross-run hash chaining are the design work.

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
catalog costs nothing per compile; 7 by running the suites; 8 held for stage 1
(the v1 record's `residual_ref` byte was asserted zero in its round-trip test)
and is now discharged differently: stage 2 DOES persist residuals, through the
§7 tables, under §8.2's amended rule.

Attack 3 is worth its own sentence, because the answer is not what the phrasing
suggests: dropping the *function* leaves the pair perfectly healthy.
`drop_function` removes a name binding, and the pair does not hold a name — it
holds the blob's hash, and content-addressed blobs are never deleted. Tested
(`dropping_the_function_name_does_not_break_the_pair`), and it is the same
property from the other side as the redefinition test: what the pair points at
cannot be changed by anything that happens to a name.

### 12.2 The stage-2 attack list

Written before stage 2 is built, from the 2026-07-28 adversarial check of the
build sketch — the three found VIOLATIONS are already folded into §7/§8.1/§11;
these are the residual risks each stage-2 test must discharge:

1. **The joint collision key is unframed.** `value_bits` has no length framing,
   so naive concatenation makes distinct `(y, r)` pairs collide and falsely
   refuses a valid pair. The key is `blake3(len‖bits(y)‖len‖bits(r))`, and a
   test constructs the `("a\x04","")` / `("a","\x04")` trap explicitly.
2. **Hashes over raw bits instead of canonical bits.** A pair that legally
   canonicalises a NaN payload would verify clean and then fail revert with a
   false "artifact changed". `source_hash`/`output_hash` are blake3 over
   `value_bits`-canonical encodings, tested with a NaN-carrying column.
3. **NULL residual read as missing (or vice versa).** `rex(x)` may return NULL;
   a missing row is a hard error. Confusing them smuggles the refused creation
   path back in as a silent wrong answer — the fail-safe-per-consumer trap.
4. **The apply loop that can never complete.** One big txn + a table over the
   memory guard = OOM-kill, deterministic on retry. The pre-flight refusal with
   numbers is the fix; chunked commits are NOT (they break total verification).
5. **Rows the pair refuses, skipped instead of aborting.** Per-row skipping
   leaves transformed and untransformed values indistinguishable in one column —
   Cambria's grey zone, per-row. Abort with the row named.
6. **Stacked runs unwinding out of order.** Stacking is SUPPORTED (the
   chained form the `(run_id, pk)` key was designed for); the discipline is
   LIFO unwind — reverting or putting back a buried run is refused with the
   topmost run named, because a buried run's residuals describe a column
   state later runs transformed away.
7. **SIGKILL mid-apply.** Single-txn atomicity plus FLD-2 recovery must leave
   the file verifying and the column untouched — the crash harness variant is
   the proof, not the argument.
8. **The suites moved.** Same as stage 1: nothing here touches the SQL surface;
   corpus/Django/CPython numbers must not move.

## 13. Stage 4 — table-SET maps: migration mirrored both ways **[BUILT 2026-07-29]**

The migration/sync shape: table set A (this system's schema) is mirrored
into table set B (another system's schema — renamed tables, renamed columns,
converted values), and edits on EITHER side flow to the other through the
registered lens pairs. The division of labour is deliberate: getting data
in and out of the EXTERNAL system (delta-since-timestamp with overlap, full
dump + diff — whatever that system supports) is the caller's logic, applied
to table set B with plain SQL; **rRETL owns the transformation between the
sets, in both directions, and the loop safety of repeating it.**

### 13.1 The insight that collapses the design: both sides exist

`rretl apply` destroys its source, which is why it must store residuals and
verify totally before commit. A map does NOT: A stays. So for a residual
pair the residual never needs storing — at sync time, `rex(x_current)` IS
the residual, computed live from the A side, and the B→A direction is
exactly putback with a live residual:

```
A→B:  y  = forward(x)                          (per mapped column)
B→A:  x' = inverse(y', rex(x))                 (x = A's current value)
      gated per row by PutRes, both halves:
      forward(x') == y'  ∧  rex(x') == rex(x)
```

Class rules follow: `bijective` flows both ways anywhere; `residual` flows
both ways wherever the A row exists (a row CREATED on the B side has no x to
rex — the §4 creation-path refusal, named); `lossy` is forward-only — a
B-side edit to a lossy-mapped column is refused by name, because nothing can
say what it means on the A side.

### 13.2 Loop safety and conflicts: the state table is the third copy

```
rretl_map_state (map TEXT, tbl TEXT, pk_enc BLOB)
              → k ANY, a_hash TEXT, b_hash TEXT, ts_micros INTEGER
```

(rigid types from specs, like every bookkeeping table; `tbl` is the TARGET
table name, unique per map entry; `k` carries the key VALUE — canonical
bits are one-way, and the both-deleted cleanup needs the value to ask both
sides "are you still there?"; hashes are length-framed CanonChains over
the mapped columns in mapping order.) After every successful push, BOTH
sides' current hashes are recorded. `map sync` then classifies each row by
two bits — (A moved since last sync, B moved since last sync):

- (false, false): skip. **This is the echo guard**: a change that arrived BY
  sync leaves both recorded hashes current, so the next sync — either
  direction, any schedule — sees a clean row and does nothing. No epochs, no
  origin tags, no loop.
- (true, false): push A→B (forward per column; lossy allowed).
- (false, true): push B→A (inverse per column; PutRes per row; lossy column
  moved = named refusal).
- (true, true): CONFLICT. The sync ABORTS whole (one txn — the SET moves
  together or not at all), naming the first conflicting (table, pk) and the
  total count. v1 has no resolution flags: fix a side, re-sync. (Mirror's
  conflicts/resolve vocabulary is the eventual home if policy is wanted.)

Creations and deletions ride the same two bits with absence as a state:
new in A → forward-insert into B; new in B → inverse-insert into A only if
every mapped column is bijective (else the creation-path refusal); deleted
in A with B clean → delete B (and vice versa); deleted on one side while
the OTHER side moved → conflict; deleted on both → state row cleared. First
sync against an empty B is just "every row is new in A" — materialization
and sync are one verb.

### 13.3 The map record

`map/<name>` in the sys keyspace (bounded, like lens records — #124 is
about unbounded logs): one version byte, then the mapping TOML verbatim.
Text-as-format with a version-byte dispatch is the same eternity stance as
the envelope; validation happens on every load, hard.

```toml
[map]
name = "crm"

[[map.table]]
source = "customers"          # must exist, single-col PK or implicit rowid
target = "crm_customers"      # auto-created at first sync if missing:
                              # key column copies the source type, mapped
                              # value columns are ANY
  [[map.table.column]]
  source = "name"
  target = "full_name"        # no pair = identity copy
  [[map.table.column]]
  source = "temp_c"
  target = "temp_f"
  pair = "celsius"            # a registered lens pair, resolved and
                              # health-checked at every sync
```

Row identity maps VERBATIM: the source row's PK (or rowid) value is the
target row's key. Every sync is one lineage row (`lens = map:<name>`,
outcome `mapped`, rows = pushed count) — never `applied`, so revert and
stacking ignore maps entirely.

### 13.4 What v1 refuses, by name

Composite source PKs (same rule as apply); a target table whose shape does
not match the mapping; a pair that fails its health check; B-side inserts
into non-bijective mappings; B-side edits to lossy columns; conflicts. The
external system's clock skew, delta overlap and dump-diff strategies are
the CALLER's, on purpose — they act on table set B with ordinary SQL, and
the map keeps A honest.

### 13.5 `map check` — the audit the echo guard structurally cannot do **[hardening round, 2026-07-29]**

The echo guard's whole mechanism is NOT looking: a row whose recorded
hashes match both sides is skipped untouched — `forward(A) == B` is never
re-verified there, by design, because re-verifying is what an echo IS.
The price is that a tampered state row, a definition change that dodged
re-baselining, or an engine bug leaves a STANDING, SILENT divergence that
no number of syncs will ever surface. Two mechanisms close this:

- **`rretl_map_check <name>`** — the read-only twin of the sync: the same
  (state, target-row) classification, nothing written, no abort-on-first.
  It reports what a sync WOULD do (pending both directions, creations,
  deletions, adoptions), EVERY conflict by name, `orphan_state` (state
  rows both of whose sides are gone — pending pass-3 cleanup, not a
  breach), and the one thing the sync never computes: **`diverged`** —
  rows where state says both sides are clean yet `forward(A) != B`.
  Post-sync steady state ≡ `is_clean()`: nothing pending, nothing wrong.
  CLI exits 1 on breaches (conflicts/diverged), so it crons like fsck.
- **fsck coverage**: `rretl fsck` walks every STORED map — before the
  lineage early-return, so a defined-but-never-synced map is covered —
  and reports records that no longer parse, specs that no longer resolve,
  pairs that no longer load, and every diverged row. Pending work,
  conflicts and orphans are user state, not integrity findings; they
  belong to check, not fsck.

**Redefinition re-baselines.** `map define` over an existing name with a
CHANGED spec deletes that map's state rows in the same transaction as the
record write. The state hashes were recorded under the old spec; against
a new one they misclassify — the sharpest case being a swapped pair over
the same columns, where every chain is untouched, every row reads "both
clean", and the target keeps the OLD pair's forward forever. Cleared
state forces the next sync through the no-state paths: adopt-on-agree
re-arbitrates, disagreement is a named conflict. An UNCHANGED redefine
keeps state, so a dropped map's history stays adoptable (`map drop`
still keeps state rows for exactly that reason). A tamper probe is part
of the test suite: two target rows fully swapped WITH their b_hashes —
sync reports zero moved, check reports both rows diverged, fsck names
them.

### 13.6 What the adversarial duel changed (2026-07-29)

Four saboteur agents ran against the built binary — schema/DDL, values,
bookkeeping tampering, and concurrency. The concurrency arm came back
empty (~2100 SIGKILLs of the sync mid-flight, ~3M concurrent writer
statements, ~10k read-only checks; the one-transaction promise and attach
recovery held under every schedule), and no hostile VALUE produced wrong
data. The other two arms did, and the rules below are their residue.

**A table is a map source or a map target — never both, never a target
twice.** Loop safety is per map, and every topology that reaches across
that boundary breaks something: two maps sharing a target silently MERGE
two unrelated masters; a reverse map (`a→b` plus `b→a`) turns every
ordinary edit into a permanent conflict, because each map reads the
other's push as "the other side moved too"; and a chain (one entry's
target is another's source) breaks the twin-ness of check and sync —
sync applies entry 1's writes before classifying entry 2, a read-only
check cannot, so check under-counts and misses conflicts while exiting 0.
Refused at define, inside one map and across maps.

**A vanished target is a refusal, not a mass delete.** A missing target
table means "materialize it" ONLY when the map has no state for it. Once
state exists, missing means dropped or renamed away — and reading that as
"the target deleted every row" propagated the drop into the source and
emptied the master, unrecoverably (map runs carry outcome `mapped` and
are not revertible). Recovery is restore-the-table, or `map drop` +
define again; an identical re-define is idempotent by design and does NOT
re-baseline.

**Bookkeeping keys on a digest, never on raw canonical bits.** The
composite keys `(run_id, pk_enc)` and `(map, tbl, pk_enc)` spent their
budget on the OTHER parts, so a legal ~970-char TEXT pk overflowed the
engine's encoded-key cap — and the syncable key length depended on how
long you named your map. `pk_ref` = blake3 over the canonical bits, fixed
32 bytes; every consumer holds the row (or, for maps, the `k` column), so
nothing needs to invert it.

**State is a baseline, never evidence, and only an identical re-define
inherits it.** A changed spec, a first define, a define after `drop` —
all start from nothing (adopt-on-agree re-arbitrates; disagreement is a
named conflict). The two openings this closed were both real: drop plus a
changed redefine kept the old spec's hashes, and state planted under an
undefined name was adopted by that name's first define.

**The trust boundary, stated plainly.** `rretl_map_state` is ordinary
SQL-writable state, and it is the sync's sole arbiter. Its hashes are now
bound to (map, target, key), so they cannot be minted by planting a decoy
row — but a party that computes a correct hash for a doctored row can
still make the sync overwrite a real edit with no oracle able to see it
afterwards, because the sync then re-records perfectly consistent state.
`k` is cross-checked against `pk_enc` on every path that trusts it, and
orphaned (map, target) state is an fsck finding, so the cheap and the
accidental tampers are caught. The expensive one is not, and cannot be
without signing the arbiter. Writing to rRETL bookkeeping by hand is
writing to the engine's internals: fsck cannot vouch for a doctored
oracle.

**Identifiers resolve case-INSENSITIVELY, to the schema's spelling.** SQL
accepts `SELECT * FROM SRC`, so a spec must too — and everything
downstream (interpolated SQL, state keys, the messages) then uses the
schema's spelling, so one table is one table however the spec wrote it.
Before this the two halves disagreed: a source in the wrong case was
refused by name, while a TARGET in the wrong case was accepted and
auto-created into a raw `duplicate table name` at sync — a permanently
unsyncable map that `check` reported as ordinary pending creations.

**`check` is exact at quiesce, not under churn.** It is not one snapshot
across its reads, so with a sync running concurrently a single finding
may be a cross-snapshot artifact (source read from one commit, target
from the next). The counts remain a useful progress signal; a FINDING
should be re-run at quiesce before it is acted on, and the CLI says so.

**Pair rebinding is the realistic divergence.** `create_lens` rebinds an
existing pair NAME, and a map resolves pairs by name at every sync — so a
mapped column's meaning can change under a map whose state is entirely
current. The sync is structurally blind (both sides read clean); §13.5's
`diverged` is what sees it. This is the canonical reason that check
exists, and the regression test uses it.

### 13.7 `map-collide` — the sync under SIGKILL, or it did not happen

`mpedb map-collide` is the stage-4 member of the collide family
(mirror-collide's shape): N writers churn the SOURCE side with
domain-legal values, N churn the TARGET side with in-image values, and a
syncer process loops `map sync` while the parent SIGKILLs it on a tight
cadence — kills land mid-classification, mid-push, mid-commit. Named
CONFLICTs are the syncer's expected diet (the sync aborts whole, so the
next round sees the same world); anything else it hits fails the run.
The final drain is the contract: quiesce, resolve standing conflicts
deterministically (source wins; the row's state entry dropped so
adopt-on-agree re-arbitrates), then hard-assert `changed_total == 0` on
one more sync, `map check` clean, `rretl fsck` empty, row counts 1:1.
Divergence after any number of kills = a row lost, duplicated or
half-applied, and the run fails with the evidence.

## 14. #53 — the daemon form: `map run`, bounded and resumable **[BUILT 2026-07-29]**

`map sync` is ONE transaction: the set moves together or not at all. That
is the right contract for "migrate this set now" and the wrong one for a
cron line — a set larger than one budget can never make progress, and a
killed run leaves nothing to resume from. `map run` is the other trade,
and it is a SEPARATE VERB precisely so no existing guarantee changes
quietly.

**Commits as it goes, and every commit advances the WHOLE set.** One pass
over the map's tables per transaction — a chunk from each table that
still has work — rather than finishing table 1 before table 2 starts. So
an interrupted run leaves the tables at comparable positions, never
"customers fully mirrored, orders untouched", and a reader between two
commits sees one step of the whole map rather than one table racing
ahead. What is given up is atomicity ACROSS chunks; what is bought is
progress that survives a kill. It is safe for maps in a way it would not
be for `apply`: nothing is destroyed, both sides exist throughout, and
each row's push and its state row are written in one transaction, so
every commit is per-row consistent and the classification is idempotent.

**Where it left off.** `rretl_map_cursor (map, tbl) → phase, k, pk_enc`.
A ROUND is passes 1→2→3 over the whole set; finishing pass 3 everywhere
completes the round, clears the cursors and bumps `round` in
`rretl_map_run`. Rows that change BEHIND the cursor are picked up by the
next round — "eventually consistent per round", which is the honest
description of any incremental mirror, stated rather than implied.

**Four bounds, because a cron line needs all four:**

| bound | what it does |
|---|---|
| `--max-secs` | wall clock, checked BETWEEN transactions — the overshoot is one chunk |
| `--max-rows` | rows classified; clock-free, so tests are exact |
| `--runner` | must match the map's recorded runner (`map runner <name> <id>`) |
| `--lease-secs` | how long this run's lease holds; default `max_secs + 60` |

**Conflicts are counted and SKIPPED, not fatal.** `map sync` aborts whole
on the first, because a migration wants all-or-nothing. A daemon that did
that would let one unresolvable row block every other row's sync forever.
The report carries the count and the first few verbatim (a cron mail
should say something useful); `map check` remains the place to see them
all by name.

**The runner restriction is a policy guard, not a fence.** It stops a
laptop from picking up the server's cron job by accident. It does not
stop anything: mpedb has no auth layer, and whatever can write the file
can claim any runner name. The real boundary is the OS — only the server
has the file and the crontab entry. Said plainly here for the same reason
§13.6's trust boundary is.

**The lease buys wasted work, not correctness.** Two runners at once is
harmless — the work is idempotent, and the second classifies the first's
pushes as clean — so a stale lease EXPIRES rather than wedging the map.

**One operational note.** The daemon's FIRST run creates its two
bookkeeping tables, and that DDL invalidates every other process's
prepared plans (as all DDL does). Deployments take that once; the
`map-collide` harness does it in setup so the fuzz measures the sync path
and not one schema bump.

A crontab line, then, is the whole deployment:

```
* * * * * mpedb rretl map run /etc/app/db.toml crm --max-secs 45 --runner server-1
```

`map-collide --mode run` is the crash evidence: writers churn both sides
while the daemon is SIGKILLed every few milliseconds, and the final drain
must still converge to a clean `map check` and `rretl fsck`. Measured at
473 kills on Linux, converged clean — the cursor and the rows it covers
are written in one transaction, so a kill between commits cannot separate
them.

---

## 15. #53 — the stream form: the map learns what changed **[BUILT 2026-07-29]**

§14's daemon is honest but blunt: every round walks the source, then the
target, then the state table. That is O(rows) per round no matter how few
rows moved, and it is the reason `--max-secs` exists. A map over a million
rows where twelve changed pays for a million.

The stream form removes the scan from the common case. A trigger on each
mapped table appends the key it just touched to a journal, and the daemon
drains the journal before it advances the scan. Latency stops being "how
long is a round" and becomes "how long until the next drain".

### 15.1 What it is NOT: the journal is a fast path, never the truth

This is the same rule ingest states about deltas and dumps (DESIGN-INGEST
§4), and it is load-bearing here for reasons that are specific and named:

- **Triggers fire on the SQL path.** A write that reaches the tables another
  way — `mirror import`, the typed row API, a restore that swaps a file in —
  leaves no journal entry. The rows are still different; nothing recorded
  that they became different.
- **A trigger is a schema object the user can drop.** `DROP TRIGGER
  rrmap_crm_cases_ins` is a legal statement, and it silently turns the fast
  path off.
- **A map defined over tables that already have rows** has no journal
  history for them at all.

So the scan does not go away. `map run` keeps doing rounds; the journal only
means each round finds almost nothing left to do. **A map whose journal is
the only thing that ever ran is a map that will drift, and no amount of
draining detects it — only the round does.**

### 15.2 A JOURNAL, not a set

```
rretl_map_dirty (seq)  →  map, tbl, side, k, ts_micros
```

`seq` is an implicit rowid: the trigger inserts without it. Append-only, and
deliberately so.

The obvious design — a SET keyed `(map, tbl, pk)`, so a hot row costs one
entry rather than one per write — is the one the duel already broke. A raw
pk inside a composite key hits the engine's 976-byte encoded-key cap, and
the rest of rRETL's bookkeeping only escapes it by keying on `pk_ref`
(blake3 of the pk's canonical bits). **A trigger body cannot compute a
blake3.** It can INSERT what it was handed, and nothing else.

So: append the key, tolerate duplicates, collapse them at drain time. This
is also the cheapest thing the write path can be asked to carry — one
insert into a table with one index, no read, no hash.

### 15.3 The echo is bounded at 1

The syncer's own pushes fire the triggers — verified, not assumed: a trigger
fires inside a `WriteSession` exactly as it does for a top-level statement.
So every row the daemon writes re-appears in the journal.

That terminates, and the argument is the state table's (§13.2): the next
drain classifies the re-appeared row against the state written by the push
that caused it, finds both sides clean, and writes NOTHING. No write, no
trigger, no new entry. The amplification is exactly one extra classification
per pushed row — not a loop, and not something the operator has to tune.

### 15.4 Opt in, because it is the write path that pays

`stream = true` on the map, default off. A journal insert on every write to
a mapped table is a real cost on somebody else's latency, and a map that is
synced nightly over a table written a thousand times a second should not pay
it. Turning it on is a decision with a number attached, so it is the
operator's.

`map define` installs the triggers (`rrmap_<map>_<tbl>_<ins|upd|del>`) in the
same transaction as the map record; `map drop` removes them in the same
transaction as the record's deletion. A name that already exists as a user
trigger is refused at define time rather than overwritten.

### 15.5 Drain and scan share one transaction

A chunk is: drain up to `chunk` journal entries, classify their distinct
keys, push what they need, delete the entries, advance the scan cursor by
one chunk — all in ONE transaction. A kill between commits therefore cannot
separate "the row was synced" from "the entry was consumed", which is the
same property §14 relies on for the cursor.

`map sync` (the whole-set form) clears the map's journal on success: it
holds the writer lock for the entire transaction, so when it commits there
is nothing outstanding by construction. Leaving stale entries would only
make the next daemon round re-classify rows it already knows are clean.

### 15.6 What it is worth — LATENCY, not total work

The first claim written here was that the journal does 500× less work. It
does not, and the measurement said so: `map run` still advances the scan
every invocation, so a streaming round classifies the same rows as a
scanning one PLUS the journal's. Total work is unchanged.

What changes is how long a change waits. The daemon's budget bounds each
invocation, and the drain runs FIRST, so a change is synced on the next
tick instead of when the scan's cursor happens to reach it. Measured on the
dev box (`MPEDB_RRETL_CHUNK=200`, `map run --max-rows 200`, five rows
changed at the far end of the scan order):

| rows | scan: invocations before the change is mirrored | journal |
|---|---|---|
| 2 000 | 10 | **1** |
| 4 000 | 20 | **1** |
| 8 000 | 40 | **1** |

The scan's latency is `rows / budget` invocations and grows with the table;
the journal's is 1 and does not. That is the whole trade: the same work per
tick, spent on the rows that actually moved first.

A map that wants the scan to cost less as well has the budget for it —
`--max-secs` and `--max-rows` already bound a round, and with the journal
carrying the changes the round can be run as slowly as the operator's
tolerance for undetected drift (§15.1) allows. That knob is the crontab
line, not a new flag.
