# RRETL-BIDI — reversible ETL for PySpell (design pre-work)

**Status: prior-art research (2026-07-16). The design itself is not written — this document is
the pre-work task #52 requires before design + adversarial review, the same discipline as DESIGN-DDL.**

The idea (#52): mpedb already stores enough provenance that PG→mpedb→sqlite3→mpedb→PG is a round
trip (mirror + type provenance). Generalize that to user-defined ETL via PySpell: register
function PAIRS that go both ways, compose them into pipelines, and let the database store what is
needed to reverse — so that "loss" of data can be undone by running the pipeline backwards.

The form is a *lens pair with residual*:

```
forward(x) → (y, residual)        inverse(y, residual) → x
```

Lossless steps (rotate image, add annotation) have residual = ∅. Lossy steps
(color→grayscale: residual = chroma; PNG→JPEG: residual ≈ original, so there source retention wins)
declare what is stored. The round trip works because what was lost is STORED — the same mechanism as
the mirror round trip, never because the mapping is magically invertible.

**Example that binds #50 and #52 together (Morten, 2026-07-16) — version storage as base + diff:**
two header files can reflink-point at the same blob (#50), but an edit that rewrites the whole file
costs 2× disk space — and on ext4/macOS reflink does not exist at all. With bidirectional ETL,
version 2 is instead stored as base + diff: `forward(v1, edit) → v2` with *the diff as residual*,
`inverse(v2, diff) → v1`. The DB verifies `apply(v1, diff) == v2` byte-identically at ingest
(commitment 4), and the lineage table knows which base a diff belongs to (commitment 8).
Delta-engine candidates: `zstd --patch-from`, xdelta/VCDIFF (RFC 3284), bsdiff. The precedent for
the chains is git packfiles: delta chains with a *depth limit* and periodic full snapshots — exactly
the Bennett-pebbling knob from commitment 6, in production for twenty years. Logical delta compression is
FS-independent — it works where reflink does not, and the two compose: reflink for identical
blocks, diff residual for near-identical files.

**And container formats as presentation (Morten, 2026-07-16):** a logical file can *appear* as
.gz/.zip via mpedbfs (#54) while mpedb stores the content in its own format (delta chains, extents,
dedup). Two modes with different prices: (a) **presentation** — synthesize a fresh, deterministic
gz/zip on read (fixed parameters, mtime=0); no residual, but the bytes are not the original's.
(b) **round trip** — an ingested .gz/.zip is unpacked into content + container residual (gzip header:
mtime/OS/FNAME; deflate block structure; zip: central directory), and can be recreated BYTE-IDENTICALLY.
The same data has many valid deflate encodings — that is *why* the residual is needed when the original's
hash/signature matters. The precedent for (b) is shipped and old: Debian's **pristine-tar/pristine-gz**
regenerates exact original tarballs from unpacked content + a small delta — the jbrd pattern
applied to gzip, in production in the Debian infrastructure for ~20 years. (`precomp`/`xtool` do the same
for arbitrary deflate streams.) The zip variant gives a bonus: the members live as logical
rows/blobs in mpedb (queryable, deduped, delta-compressed), and the archive is just a view.

The research question was: **what did everyone who built this before us learn?** Three sweeps:
academic BX/lens theory, reversible programming languages, and industrial systems
(incl. artifact correlation in practice). The chapters below are the findings; first the distillate.

## Design commitments (distilled from all the prior art)

These bind the #52 design. Each of them is paid for by a concrete failure or a concrete
success in the chapters below.

1. **The form is formally grounded.** `forward(x) → (y, residual)` / `inverse(y, residual) → x` is
   exactly the symmetric-lens complement (Hofmann/Pierce/Wagner), and the composite residual for a
   pipeline is *the tuple of per-step residuals, applied in reverse* — composition is mechanical,
   no global analysis (the Janus property). But residual representations are only equal up to
   equivalence: compare pipelines on observable round-trip behavior, never on residual bytes.

2. **Classify every pair explicitly**: bijective / invertible-with-residual / lossy-with-
   source-retention. Cambria died in the undeclared gray zone; the JPEG XL encoder REFUSES input
   it cannot round-trip (`JXL_ENC_ERR_JBRD`) instead of emitting a pair that does not
   invert. Fallback on refusal = full source retention.

3. **The laws that are tested**: `inverse(forward(x)) == x` (the GetPut analogue) and residual stability
   `forward(inverse(y, r)) == (y, r)` (PutRes — catches residual drift). NOT PutPut — every
   practical system discarded it. The round trip is tested *modulo a declared canonizer* (the quotient-
   lens lesson: exact byte equality breaks on trivialities; byte-identical is the default canonizer).
   The `inverse(y, ∅)` semantics (the creation path) is declared per step or explicitly refused.

4. **Verification scales with what you dare delete.** If the source is kept: sampled property testing at
   ingest. If the source is deleted: 100 % decode-and-bit-compare BEFORE commit — Dropbox Lepton did
   this on 16 billion images and caught a non-deterministic buffer overrun that release qualification
   would otherwise have let through. The evidence from testing is statistical, not universal — say so in the docs.

5. **The residual is explicit and typed per step.** Implicit garbage accumulates through
   composition (RFun). The ancilla distinction is copied: read-only step parameters (config, models)
   are declared unchanged and kept outside the residual accounting (RFun v2/CoreFun, Sparcl's ordinary
   arguments). Branch choice is residual data: either a branch tag in the residual, or guaranteed
   disjoint output domains (first-match) — an explicit choice in the step API.

6. **Never promise "minimal residual".** Minimality is uncomputable (Glück/Yokoyama 2023); the hard
   lower bound is conditional entropy H(X|Y) — a stage that throws away k bits must store ≥ k bits.
   Offer Bennett pebbling as a configurable knob instead: checkpoints every k-th stage traded against
   recomputation time at reverse (the O(S·log T) point is the theoretically favorable one).

7. **Sparcl's pin rule as an invariant:** everything a lossy stage *reads* of pipeline data must be
   recoverable from (output, residual). No third way exists.

8. **Artifact correlation: content hash + lineage table, never file names.** All the mature tools
   (DVC, Pachyderm, lakeFS, MLflow) landed there; positional alignment was "a show-stopper"
   already in Boomerang. Lineage row: `(source_hash, output_hash, plan_hash, residual ref,
   run ID)` — blake3 and content-hashed plans already exist in mpedb. One stable
   run ID per pipeline run (Pachyderm's global-ID move) makes multi-step lineage a single
   lookup. A generated filename prefix is only a UX view of the lineage lookup; embed a
   DerivedFrom-like ID (the xmpMM pattern) in the output where the format allows. Hash mismatch at
   reverse = explicit error ("artifact changed outside the pipeline"), never silently wrong input.

9. **The residual format is a promise for eternity.** Version it from day one; decoder minimal,
   bounds-checked, `Corrupt`-never-panic (the mpedb rule applies doubly). A new version of a
   pair that cannot consume old residuals is a NEW pair with a new hash (the upcasting rule
   from event sourcing). Lepton is archived, but 16 billion files must be decoded forever.
   The pipeline's own definition format is versioned too — Cambria's prototype became unusable as a
   system of record because *the tool's* format drifted.

10. **Do not build inverses where replay is free.** The mirror layer's CDC is already source retention;
    reversible pairs earn their place when the source is to be deleted (the Lepton case) or
    reverse must be O(1) per artifact instead of O(replay). Run-cache idempotence follows
    for free: `(input-hash, plan-hash) → output-hash` (the DVC move).

11. **The adoption pattern:** the only deployed lens system (Augeas) won through a narrow domain and
    dynamic enforcement; everything with stronger static ambitions remained a research language
    (Boomerang, Links, Sparcl). mpedb's choice — a narrow contract enforced by property testing on
    real data — sits on the right side of that divide, and even Sparcl ended up with runtime assertions.
## Prior art: academic BX/lenses

### Asymmetric lenses (Foster, Pierce et al., UPenn) and Boomerang

**Mechanism.** A lens `l : C ⇌ A` is a pair of partial functions: `get : C → A` (projection from concrete source to abstract view) and `put : A × C → C` (merges an updated view back — with the *original source* as second argument, since `get` throws away information that `put` must restore). The framework ([Foster et al., TOPLAS 2007](https://www.cis.upenn.edu/~bcpierce/papers/lenses-toplas-final.pdf)) is a combinator DSL where each expression denotes both directions simultaneously, and typed combinators guarantee the laws compositionally. Creation (put without an original) is handled with an explicit "missing" element Ω with defaults; Boomerang turns this into a third component `create : A → C` ([Bohannon et al., POPL 2008](https://www.cis.upenn.edu/~bcpierce/papers/boomerang.pdf)).

**Laws and what got relaxed.**
- **GetPut**: `put (get c) c = c` (unchanged view → unchanged source). **PutGet**: `get (put a c) = a` (put must preserve *all* information in the view). These two = *well-behaved*, and the authors say explicitly that removing either one "significantly weakens the semantic foundation".
- **PutPut**: `put a' (put a c) = put a' c` (well-behaved + PutPut = *very well behaved*). **This one was dropped** because `map`, `flatten`, `merge` and conditionals break it "for reasons that seem pragmatically unavoidable". Canonical counter-example: a version-counter lens that increments on change satisfies GetPut/PutGet but not PutPut. The Boomerang paper is even clearer: requiring very-well-behavedness "would prevent writing many useful transformations" — delete a composer and add him back, and the birth dates come back as defaults, "unfortunate, but the alternative is disallowing deletions!". Very-well-behaved ≙ Bancilhon–Spyratos' "constant complement" (TODS 1981) — the classic, too restrictive DB regime.
- **Quotient lenses** ([Foster/Pilkiewicz/Pierce, ICFP 2008](https://repository.upenn.edu/cis_papers/390/)): the laws are in practice too strong "on the nose" — they should hold *modulo declared equivalences* (whitespace, field order, escaping). Mechanism: *canonizers* and operators `lquot`/`rquot` that quotient the domain/codomain; the type tracks which equivalences each lens respects. Implemented as an extension of Boomerang.
- **Totality is half the guarantee**: without totality, well-behavedness is nearly trivial (any `get` can be paired with a `put` defined only on `(get c, c)` — TOPLAS footnote 3). Boomerang's type checker proves totality via regular expressions and unambiguity checks (unambiguous concatenation/iteration) — possible only because the domains are regular languages.

**Alignment relaxation (dictionary/matching lenses).** Basic string lenses align positionally in `put`; on reordering in the view, data gets mixed between rows (the Copland/Britten years get swapped) — "a show-stopper for many of the applications we want to write". Solution: *dictionary lenses* — the user marks *chunks* (`match`) and a *key* per chunk; `put` parses the source into a skeleton + a key-indexed dictionary and fetches chunks by key, not position. Keys need not be unique (collisions are matched positionally). The new law is **EquivPut**: `c ~ c' ⟹ put a c = put a c'` where `~` is key-respecting reordering (*quasi-obliviousness*). [Matching lenses (ICFP 2010)](https://www.cis.upenn.edu/~bcpierce/papers/alignment.pdf) generalizes: the alignment phase is separated from the weaving phase, and *arbitrary heuristics* can be plugged in (positional, minimal edit distance, non-crossing/LCS, or derived from the operation itself) — the only requirement for well-behavedness is that identical arguments yield the identity alignment.

**Adoption.** The Harmony synchronization tool (bookmarks, address books, calendars) was the driving force; Boomerang shipped real lenses for vCard, CSV, BibTeX/RIS, LaTeX, iTunes libraries and SwissProt/UniProt. The only broad deployment of the lens ideas is **[Augeas](http://augeas.net/)** (Lutterkort, Red Hat 2007): asymmetric get/put lenses for Linux config files, deployed via RHEL tooling, Puppet's `augeas` resource and libguestfs — the symmetric lenses paper calls it a "commercial application". Augeas succeeded by narrowing the domain (config files), keeping an original-preserving `put` (unchanged text is written back verbatim) and *not* proving the laws formally per lens.

### Relational lenses (Bohannon/Pierce/Vaughan, PODS 2006; Links, Edinburgh)

**Mechanism.** A bidirectional relational-algebra language ([PODS 2006](https://www.cis.upenn.edu/~bcpierce/papers/dblenses-pods.pdf)): each expression reads left-to-right as a view definition and right-to-left as an update policy. Primitives: `select from R where P`, `join_dl R S` (the suffix is a *policy annotation*: delete from the left table on view deletion), `drop A determined by (X, default)`. The type system carries functional dependencies (FDs, in "tree form") and record predicates, and the typing rules are theorems guaranteeing the lens is well-behaved **and total** on the declared schema domain.

**Laws and relaxation.** Only GetPut and PutGet are required (their Def. 3.2); PutPut is absent. The paper shows concretely how naive, symmetrically-pretty putback definitions break the laws (their `v⋈` example breaks both GetPut and PutGet), and that the repairs yield documented *counter-intuitive* behavior forced by PutGet: `select`'s putback **deletes** existing source rows that would collide with the FDs; `join_dl` requires shared attributes to be a key in the right table; `drop` requires the column to be FD-determined and have a declared default. They admit that "totality imposes a stringent constraint on our lens design" — the main contribution was finding usable total domains via FDs.

**Alignment.** The core is *relational revision* `M ←_F L`: rows are correlated **key-based via FDs** (C-Match: matches `m[X] = n[X]` → overwrite the Y fields; otherwise unchanged) — never positionally. Deletion vs. absence is distinguished by simulating putback-get and removing rows that would resurrect (their `L` set) — the "deleted vs. never-there" problem is thus solved by computing what the view *should* have contained. Join ambiguity is resolved not heuristically but by the user choosing a policy variant (`join_dl`, `join_dr`, …) in the syntax.

**Adoption.** The PODS paper promised a prototype; it never came. [Horn/Perera/Cheney (ICFP 2018)](https://arxiv.org/abs/1807.01948) note that the semantics "has not been implemented or evaluated to date" and that relational lenses "have seldom actually been used" — the state-based `put` reconstructs the whole source state per update and is impractical. The first implementation came 12 years later in the research language **Links** (Edinburgh), and only by making the semantics *incremental* (delta propagation, orders-of-magnitude speedup over naive put, without requiring updatable views in the underlying Postgres). Continued in [Language-Integrated Updatable Views (2020)](https://arxiv.org/abs/2003.02191) and [Horn's PhD (2022)](https://era.ed.ac.uk/handle/1842/39676). [Links itself](https://links-lang.org/) is and remains a research language (academic case studies like Covid-19 data curation and database wikis; no known industrial use).

### Symmetric lenses and edit lenses (Hofmann/Pierce/Wagner, POPL 2011/2012)

**Mechanism.** A [symmetric lens](https://www.cis.upenn.edu/~bcpierce/papers/symmetric.pdf) `ℓ : X ↔ Y` consists of a complement set `C`, a designated `missing ∈ C`, and `putr : X×C → Y×C`, `putl : Y×C → X×C`. Neither side is primary: the complement is conceptually `C_X × C_Y` — "private information" thrown away in each direction. The laws PutRL/PutLR say that `putr(x,c) = (y,c')` implies `putl(y,c') = (x,c')` (consistent steady-state triples `(x,y,c)`). The intermediate step in the paper is exactly the mpedb signature: an asymmetric lens with complement has `get : X → Y×C`, `put : Y×C → X`, `create(y) = put(y, missing)` — i.e. `forward(x) → (y, residual)` and `inverse(y, residual) → x` is formally identical to the complement formulation. Barbosa et al.'s reformulation also yields a third law, **PutRes**: `res(put(v,c)) = c` (cf. [McKinna, Bx 2016](https://groups.inf.ed.ac.uk/bx/2016-Bx-CWC.pdf), which shows that complement elements are *witnesses* to the consistency relation — the laws are precisely the "hygiene check" that the operations produce valid witnesses).

**Composition and complements.** Yes — the composition (Def. 4.2) is explicit: `(k;ℓ).C = k.C × ℓ.C`, the composite complement **is** the tuple of per-step complements, and the puts are threaded through step by step. But the crucial caveat: algebraic laws (associativity, identity) hold **only up to lens equivalence** — `(j;k);ℓ` and `j;(k;ℓ)` are *not* the same lens "because their complements are structured differently"; equivalence is defined behaviorally (same observable put sequences, complements related via a relation R). Symmetric PutPut variants are considered and rejected: "these laws appear too strong to be desirable in practice". Alignment remains the weakness: their list mapping is positional, and inserting at the front of the list gives "surprising (and probably distressing) results" — they point to matching lenses or deltas as the way out.

**Edit lenses** ([POPL 2012](https://dl.acm.org/doi/10.1145/2103621.2103715)) are the delta way out: edit structures = a monoid of edits `∂X` with a partial action on `X`; the lens is stateful monoid homomorphisms `∂X × C → ∂Y × C` plus a consistency relation `K ⊆ X × C × Y`; the law is that propagating a defined edit preserves `K`. Container edits (insert/delete/modify/**rearrange**) carry the alignment information *in the edit itself* — retrospective diffing of states falls away. Composition again combines the complements as a product.

**Adoption.** Pure theory: no implementation beyond prototypes (Wagner's [dissertation, 2014](https://repository.upenn.edu/edissertations/1488/)); the influence went into the delta-lenses/model-transformation community (Diskin, the QVT discussions), not into production systems.

### Lessons for mpedb

- **Verify the laws with property testing, not statically.** All the systems that proved the laws statically managed it only in narrow domains (Boomerang: regular languages with an unambiguity check; relational lenses: FDs in tree form — and remained unimplemented for 12 years). For arbitrary user-registered function pairs, static guarantees are out of reach; mpedb's round-trip test on real data at ingest is moreover precisely the *totality* check the literature shows the laws are worthless without (well-behavedness is nearly trivial for partial lenses).
- **Test GetPut + PutGet; do not require PutPut.** PutPut was discarded by every practical system ("pragmatically unavoidable" violations in map/merge/conditionals; ≙ the too-rigid constant-complement regime). Do however consider testing **PutRes** as a third law: `forward(inverse(y, r)) == (y, r)` — that re-forward regenerates *the same residual* catches residual drift that plain `inverse(forward(x)) == x` does not see.
- **The round trip must be tested modulo declared equivalence** (the quotient-lenses lesson): exact byte equality breaks on whitespace/field-order/encoding trivialities. Let each function pair declare an optional canonizer, and test `canon(inverse(forward(x))) == canon(x)`; track which equivalence each step respects in the plan metadata.
- **Residual = complement, formally** — and the composite residual for a pipeline is the tuple of per-step residuals (the symmetric-lens composition theorem). Store residuals per step and compose mechanically. But: equality between composite complements holds only up to equivalence — compare pipelines on *observable behavior* (round-trip result), never on the residual representation's bytes/hash.
- **The creation path must be explicit.** Every framework needed `Ω`/`missing`/`create` with defaults for "put without an original". mpedb must define `inverse(y, ∅)` semantics per step (declared defaults), not leave it undefined.
- **Artifact correlation: key-based, never positional.** Positional alignment was "a show-stopper" (Boomerang) and gave "distressing results" (symmetric lenses). Require user-declared keys per step (dictionary lenses); relational lenses show that FDs/keys can be carried in the types and that ambiguous policies (join variants) should be explicit annotations, not heuristics. Matching lenses show that the match heuristic itself can safely be pluggable — the only law requirement is identity alignment on identical input.
- **Propagate edits, not states.** State-based put does not scale (relational lenses remained paper until the delta semantics arrived; edit lenses show that edits carry alignment for free). mpedb already has CDC in the mirror layer — reversible ETL should consume the change stream, not diff snapshots.
- **The adoption pattern is a warning and a confirmation:** the only deployed lens system (Augeas) won by narrowing the domain, keeping an original-preserving put and trading formal proofs for practical text preservation; everything with stronger static ambitions stayed in research languages (Boomerang, Links). mpedb's choice — a narrow contract (function pair + residual) enforced dynamically on real data — sits on the right side of that divide.
## Prior art: reversible languages + artifact correlation

### 1. Janus — reversible imperative (Lutz/Derby 1986; Yokoyama/Glück 2007)

**Mechanism.** Every statement is locally invertible by construction: assignment does not exist, only *reversible updates* `+=`, `-=`, `^=` where the left-hand variable may not occur on the right-hand side (so the update is a bijection on the variable). Control flow carries path information explicitly: `if` has both an entry test and an **exit assertion** (`fi`), loops have an **entry assertion** (`from`) + exit test (`until`); backwards execution swaps the roles. Information destruction is handled by forbidding it without proof: global variables are zero-initialized, and `local x = e … delocal x = e'` requires the programmer to *state the value* the variable holds at deallocation — an explicit "I can reconstruct this" commitment, checked as a runtime assertion. The inverse is syntactic and local (statement-for-statement, blocks in reverse), no history; the language is r-Turing-complete (simulates any reversible TM). Sources: [Janus (Wikipedia)](https://en.wikipedia.org/wiki/Janus_(time-reversible_computing_programming_language)), [PIRC/DIKU](https://topps.diku.dk/pirc/?id=janus), [Yokoyama & Glück 2007](https://dl.acm.org/doi/10.1145/1366230.1366239).

**Adoption.** Caltech student project 1982/86, rediscovered and formalized by Yokoyama/Glück 2007; playground interpreters, teaching at DIKU, demo programs (FFT, RTM simulation). No production use. It stayed academic because (i) the main motivation (the Landauer bound, reversible/adiabatic hardware) never materialized commercially, (ii) the annotation burden is high — every deletion requires a delocal assertion, every branch an exit assertion, and (iii) algorithms must be *redesigned* reversibly, not merely translated ([overview, Wikipedia](https://en.wikipedia.org/wiki/Reversible_programming_language)).

**Lessons for mpedb.** (1) `delocal x = e'` is exactly our residual contract in imperative form: destruction is legal only when the value can be stated/reconstructed — make destruction explicit per stage, never implicit. (2) Local invertibility gives free composition: the pipeline inverse is the stage inverses in reverse order — no global analysis needed; that is the property mpedb should preserve. (3) Janus' assertions are runtime checks, not types — our property testing of round trips is the same design point, and Janus shows it suffices for correctness but costs runtime checks on every branch. (4) Full reversibility for the *whole* language was too restrictive for real programs — that is why Sparcl and Eel exist; do not repeat the mistake by requiring the whole ETL pipeline to be reversible.

### 2. RFun / CoreFun — functional reversibility, first-match, garbage

**Mechanism.** RFun ([Yokoyama, Axelsen, Glück, RC 2011](https://link.springer.com/chapter/10.1007/978-3-642-29517-1_2)) is first-order and linear: variables are used exactly once (relevance — discard is destruction), constructors are bijective. Branching is made injective via a **first-match policy**: forward, the first matching clause wins; for backwards execution to be deterministic, the result of clause *i* must not match the result patterns of earlier clauses — the output itself encodes which branch was taken, so no branch tag has to be stored. Duplication is an explicit operator whose inverse is an equality check (dup ⇄ eq). Non-injective functions are made total and injective by **extra output** ("garbage"): `plus :: Nat -> Nat <-> Nat` returns (preserved first argument, sum) — this is exactly our residual. RFun v2/[rfun-interp](https://github.com/kirkedal/rfun-interp) added **ancillae**: parameters guaranteed unchanged across the call and therefore not counted as residual. [CoreFun (Jacobsen, Kaarsgaard, Thomsen, RC 2018)](https://link.springer.com/chapter/10.1007/978-3-319-99498-7_21) provided a type system with an unrestricted (ancilla) + relevant fragment that **statically** checks first-match/injectivity in many cases; the rest remain runtime checks.

**Adoption.** Interpreters on GitHub, used in teaching (20+ students, which forced v2 with sugar and higher-order — [the IFL'15 paper](https://dl.acm.org/doi/10.1145/2897336.2897345) documents the implementation lessons). No use outside research.

**Lessons for mpedb.** (1) **Garbage grows through composition**: naive composition g∘f accumulates both f's and g's garbage; the RFun literature's answer is Bennett's trick in functional form — compute, copy the result, *uncompute* (run f backwards) to delete intermediate garbage — trading residual space for recomputation. mpedb should make the residual **explicit and typed per stage**, and let the pipeline choose: keep all stage residuals (fast inverse) or drop intermediates and recompute (small residual). (2) The ancilla distinction is worth copying: config/parameters passing unchanged through a stage should be declared read-only and not end up in the residual. (3) Branch discrimination is a residual question: if the stage output domains are disjoint (the first-match property) no branch tag is needed in the residual; otherwise the tag *must* go in. Make this an explicit part of the residual design per stage. (4) CoreFun shows that static verification of injectivity only covers "many cases" — runtime checking/property testing remains necessary even with a type system.

### 3. Sparcl — partial invertibility as a type-level property (Matsuda/Wang, ICFP 2020 / JFP 2024)

**Mechanism.** Sparcl ([ICFP'20 paper](https://mengwangoxf.github.io/Papers/ICFP20.pdf), [JFP 2024](https://www.cambridge.org/core/journals/journal-of-functional-programming/article/sparcl-a-language-for-partially-invertible-computation/809BDECF87B3748ED960FEFD42498BBE)) is linearly typed (λq-based, like Linear Haskell) with the type constructor **A• ("invertible")**: only •-data is subject to invertible computation; invertible functions have type A• ⊸ B•. **Partial invertibility is expressed via ordinary arguments**: `Int → Int• ⊸ Int•` is a family of bijections indexed by an ordinary (static) argument. The interface between the worlds is disciplined: **pin : A• ⊸ (A → B•) ⊸ (A ⊗ B)•** lets an invertible value be used as a static snapshot in irreversible code, but *only* by keeping the snapshot in the output (the information is never thrown away); **lift : (A→B) → (B→A) → (A• ⊸ B•)** imports a user-supplied (f, f⁻¹) pair — *unverified*, Sparcl trusts that the pair are mutual inverses. Branching is inverted via **with clauses** (postconditions per branch) checked at runtime as *exclusive* assertions: forward fails if the branch's with does not hold or another's also holds; backwards, the withs select the branch. Linearity forbids both discard and duplication (dup explicit; its inverse is an equality check). The guarantee: `fwd e v = u` if and only if `bwd e u = v` — a total bijection on the actual domain/range.

**Adoption.** Prototype interpreter ([github.com/kztk-m/sparcl](https://github.com/kztk-m/sparcl)), Agda formalization; motivated by serializers, Huffman/arithmetic coding, LZ77, tree reconstruction. A research language without industrial use.

**Lessons for mpedb.** (1) Sparcl's core observation is ours: **"partial-invertibility is the norm and bijectivity is a special case"** — realistic pipelines have invertible parts parameterized by irreversible parts (the model in adaptive compression is built identically in both directions). The mpedb design should let stages take static parameters that do not enter the residual accounting. (2) The pin rule is the important boundary discipline: a value crossing from reversible to irreversible code must either be static or **be preserved in output/residual** — phrase this as a rule in RRETL-BIDI: "everything a lossy stage reads of pipeline data must be recoverable from (output, residual)". (3) Sparcl's lift is identical to mpedb's user-declared forward/inverse pairs, and Sparcl cannot verify the pair statically either — they *trust*; mpedb property-tests instead, which is operationally stronger. Even Sparcl chose runtime assertions (with exclusivity) over refinement types "to keep the types simple" — runtime-declared residuals + property testing is thus a legitimate design point, not a pauper's solution; the price is that errors are found at run/test time, not at compile time. (4) The type-level approach bought them one thing testing does not give: *totality* of the guarantee (all inputs, not just tested ones) — be honest in the document that property testing gives statistical, not universal, evidence.

### 4. Bennett — theory's limits for the residual

**Mechanism/result.** [Bennett 1973](https://scispace.com/papers/logical-reversibility-of-computation-1kqufou0dk): any computation can be made reversible via *compute–copy–uncompute* — run with a history tape, copy the result, run backwards so the history disappears; garbage is handled by uncomputation, not deletion ([overview](https://en.wikipedia.org/wiki/Reversible_computing)). **Minimum residual**: for a total function f over a finite domain, the minimal number of garbage bits is ⌈log₂ m⌉ where m = the largest preimage size max_y |f⁻¹(y)| ([Embedding of Large Boolean Functions, arXiv:1408.3586](https://arxiv.org/pdf/1408.3586)) — i.e. worst-case conditional information; with variable-length coding the expected residual length is bounded below by H(X|Y). For programs over infinite domains, *minimal* garbage is not computable — it is not even decidable whether a function is injective (residual 0) ([Glück/Yokoyama, "Making Programs Reversible with Minimal Extra Data", New Generation Computing 2023](https://link.springer.com/article/10.1007/s00354-022-00169-z)). **Time/space trade-off**: full history is O(T) space; Bennett's 1989 checkpointing (pebble game) gives O(S·log T) space against O(T^{1+ε}) time; one can go all the way to the same space as irreversible, but then with exponential time ([Bennett 1989](https://mathweb.ucsd.edu/~sbuss/CourseWeb/Math268_2013W/Bennett_Tradeoffs.pdf), [Li/Vitányi pebble analysis](https://arxiv.org/pdf/quant-ph/9703009)).

**Lessons for mpedb.** (1) **Do not promise "minimal residual"** — it is uncomputable in the general case; promise instead a *declared, typed residual per stage* + tested round trip. (2) The lower bound is hard: a stage that destroys k bits of information must store ≥ k bits of residual — no clever coding gets around H(X|Y); document this as expectation management (a `DROP COLUMN` stage has residual = the column, period). (3) Bennett pebbling is directly applicable to pipelines: store checkpoints (full intermediate results) every k-th stage and recompute the rest at inverse — residual space O(S log T) against recomputation time; make checkpoint density a configurable knob. (4) Compute–copy–uncompute is the pattern for preventing residuals from internal helper steps from accumulating: internal intermediate values should be uncomputed (dropped and re-derived); only the *outer* stage residual is persisted.

### 5. Correlation of derived artifacts in practice

**Mechanisms.** Four conventions in use: (1) **Deterministic naming** — `foo.c → foo.o`, sidecar `foo.jpg`+`foo.xmp` (same basename, different extension), ffmpeg batches with suffix/`%d` patterns. Simple, but the OS/file managers do not know the link, so a rename/move of either file breaks it silently ([Sidecar file, Wikipedia](https://en.wikipedia.org/wiki/Sidecar_file)). (2) **External manifest/depfile** — gcc `-MD` .d files, provenance.json; path-keyed and goes stale when files move or the plan changes; ninja compresses them into an internal database (`.ninja_deps`) precisely because parsing tens of thousands of .d files dominated (make spends ~98 % of incremental time on .d parsing), but the deps log too gets stale/incorrect edges when the build plan changes ([ninja manual](https://ninja-build.org/manual.html), [Fuchsia: How Ninja works](https://fuchsia.dev/fuchsia-src/development/build/ninja_how)). (3) **Embedded metadata** — XMP Media Management: `xmpMM:DocumentID` (stable across versions), `InstanceID` (new per save), `OriginalDocumentID` (the root), `xmpMM:DerivedFrom` (points at the parent's IDs) and `History` (event array) ([Adobe xmpMM](https://developer.adobe.com/xmp/docs/xmp-namespaces/xmp-mm/), [exiv2 xmpMM](https://exiv2.org/tags-xmp-xmpMM.html)). Survives rename/move because the identity travels *inside* the file; fails when the format cannot embed or tools strip metadata. (4) **Content addressing** — identity = content hash (Bazel/Nix-style CAS, ninja's hash-based deps DB): completely robust against rename, but requires an external database and re-hashing on change.

**Robust pattern** (consensus across DAM and build systems): names are *hints*, never identity; identity is an embedded stable ID where the format allows it **plus** a content hash in an external database; provenance is recorded in *both directions* (a DerivedFrom pointer in the derivative + a manifest in the DB), so that one surviving side can reconstruct the link.

**Lessons for mpedb.** (1) Key residuals on **content hash of the artifact + a stable run/stage ID**, never on filename/path — path-keyed depfiles are the documented fragile variant. (2) mpedb has a luxury DAM tools lack: the database *is* the external manifest, transactionally updated together with the residual — exploit that, but still embed a `DerivedFrom`-like ID in output artifacts where the format allows (à la xmpMM), so the link survives files leaving mpedb's control. (3) Plan for stale correlation as the normal case, not the exception (the ninja experience): an inverse that meets an artifact whose hash does not match the manifest must fail explicitly ("artifact changed outside the pipeline"), not produce wrong input.

### Lessons for mpedb (collected)

- **Make the residual explicit and typed per stage** — implicit garbage accumulates through composition (the RFun experience); the composition rule is that (residual₁, …, residualₙ) in reverse order inverts the pipeline, as in Janus' local inversion.
- **Destruction requires declaration**: a stage that throws away information must either state how it is reconstructed (Janus' `delocal e`) or put it in the residual (RFun's extra output, Sparcl's pin) — no third way exists, per Bennett/H(X|Y).
- **Do not promise minimal residuals** — minimality is uncomputable (Glück/Yokoyama 2023); promise declared size + tested round trip. The lower bound per stage is conditional entropy; say it plainly in the docs.
- **Separate ancilla from residual**: read-only stage parameters (config, models known in both directions) should be type-marked as unchanged and kept outside the residual accounting (RFun v2/CoreFun, Sparcl's ordinary arguments).
- **Branch choice is residual data**: if a stage has multiple code paths, the residual must either contain a branch tag, or the stage must guarantee disjoint output domains (first-match/with-clause exclusivity) — make the choice explicit in the stage API, and let the property tester check disjointness.
- **Runtime checks + testing is a legitimate design point**: even Sparcl enforces branch exclusivity at runtime and blindly trusts lift pairs; mpedb's property testing of forward/inverse pairs is operationally stronger than Sparcl's trust, but weaker than the type guarantee — document that the evidence is statistical.
- **Offer the checkpoint knob**: Bennett pebbling → a configurable trade-off between residual space (store all intermediate results) and inverse time (recompute from checkpoints); the O(S log T) point is the theoretically favorable middle.
- **Artifact correlation: hash + embedded ID, never file names** — path/name-based links (sidecars, depfiles) break on rename/move; the robust pattern is a content hash in the mpedb manifest plus an xmpMM-like `DerivedFrom` ID embedded in the artifact where possible, and an explicit error on hash mismatch.
## Prior art: industrial systems

### Cambria (Ink & Switch) — composable lenses for schema evolution

**Mechanism.** Cambria ([inkandswitch.com/cambria](https://www.inkandswitch.com/cambria/)) expresses the relation between two JSON schema versions as an *edit lens*: one specification translates data in both directions. Lenses are built from small composable operators (`rename`, `convert`, `wrap`/`head`, `add`/`remove`, `hoist`, `in`) and are composed into a graph whose nodes are schema versions; translation between distant versions happens via the shortest path through the graph. A central choice: lenses transform *patches* (JSON Patch), not whole documents — which suits systems exchanging incremental changes. Early prototypes translated at write time; that proved fragile (new schemas added after writes were made, concurrent writes during schema registration), so they landed on storing raw writes in the *writer schema* and translating at read time.

**What shipped vs. not.** Nothing reached production. The authors are explicit: "we do not pretend to have delivered a fully formed, production-quality solution", performance was never measured, and their prototype issue tracker was "too unstable to be their only system of record" — primarily because Cambria's own storage format changed along the way (the lens format itself needed lensing). Documented open problems: recursive schemas, cross-document migration, and the fact that the `convert` operator breaks the lens formalism (the developer supplies independent forward/backward mappings with no guaranteed consistency relation).

**The most important insight** is their trilemma: *consistency* (both sides see a meaningfully equivalent world), *conservation* (no side operates on data it cannot see) and *predictability* (local intent is preserved) cannot all hold when schemas diverge — "there is no perfect option". And what they lacked is exactly the residual idea: `{firstName, lastName}` → `{fullName}` cannot be reversed reliably, and "Cambria needs a way to express dependencies on other data sources" to fetch missing data. Read-time defaults (`add` fills in empty arrays), on the other hand, worked well and moved defensive checks out of the application code.

**Lessons for mpedb:**
- Do not promise total bidirectionality. Classify each function pair explicitly: bijective / invertible-with-residual / lossy. Cambria died in the borderland where they pretended everything was invertible.
- Our residual mechanism solves precisely the hole Cambria documented — but only if the residual is captured *at forward execution*, not reconstructed after the fact.
- Store in writer format and transform on read where possible; write-time transformation commits you to re-migrating on every new schema version.
- The pipeline's own format (the lens/plan definitions) needs versioning from day one — Cambria's prototype became unusable as a system of record because *the tool's* format drifted.

### JPEG XL jbrd + Dropbox Lepton — shipped residual in production

**Mechanism (JPEG XL).** JPEG XL recompresses existing JPEG files ~20 % smaller with byte-for-byte exact reconstruction ([Wikipedia](https://en.wikipedia.org/wiki/JPEG_XL), [arXiv 2506.05987](https://arxiv.org/html/2506.05987)). The image data itself is re-coded with better entropy coding; since the same image data can be coded in many ways as a JPEG file, a separate **`jbrd` box** (JPEG bitstream reconstruction data) is stored with everything needed to distinguish the encodings: entropy coder, progressive scan script, restart markers, values of padding bits and remaining app markers ([libjxl format_overview](https://github.com/libjxl/libjxl/blob/main/doc/format_overview.md)). Reconstruction = codestream + jbrd + metadata boxes (Exif/XMP/JUMBF). The residual is "typically relatively small" — hundreds of bytes (a documented example: 489 bytes) against ~20 % savings; and it is *optional*: the image can be displayed without it; it is only needed for exact file reconstruction. Important: the encoder **refuses** input it cannot round-trip (`JXL_ENC_ERR_JBRD`, [libjxl #2693](https://github.com/libjxl/libjxl/issues/2693)) instead of producing a pair that does not invert.

**Mechanism (Lepton).** Dropbox's Lepton ([dropbox.tech](https://dropbox.tech/infrastructure/lepton-image-compression-saving-22-losslessly-from-images-at-15mbs), [NSDI paper via acolyer](https://blog.acolyer.org/2017/05/01/the-design-implementation-and-deployment-of-a-system-to-transparently-compress-hundreds-of-petabytes-of-image-files-for-a-file-storage-service/)) pulled the same trick (22 % via coefficient prediction + an arithmetic coder) on 16 billion images / multiple petabytes, and *deleted the originals*. Their verification discipline is the gold standard: "All of our compression algorithms, including Lepton, decode every compressed file at least once and compare the result to the input, bit-for-bit, before persisting that file" — a 100 % round-trip check, not sampling, with the compressed file in "kernel-protected, read-only memory" during the comparison. In addition: every release is *qualified* by round-tripping >1 billion random images before deploy — this caught a non-deterministic buffer overrun "after just a few million images". The determinism threats were compiler UB and uninitialized heap; the defenses were static linking and zero-initialization of all heap. The decoder ran under seccomp (only read/write on open fds). A production incident with format-version drift required scanning billions of files to find 18 that had to be re-encoded.

**What shipped.** Both shipped for real: Lepton in Dropbox production from 2016 (later [deprecated/archived](https://github.com/dropbox/lepton) — but the files must be decoded forever); JPEG XL is ISO/IEC 18181, in Safari 17 (2023), and back in Chrome 145 (Feb 2026) via the Rust decoder jxl-rs behind a flag ([The Register](https://www.theregister.com/2026/01/14/google_rekindles_relationship_with_jilted/)).

**Lessons for mpedb:**
- Byte identity is a *verification discipline*, not a property of the function pair. If the source is to be deleted after conversion, verification must be 100 % at ingest (cheap: both x and y are in memory). Sampled property testing is sufficient only when the source is kept.
- Determinism across versions is the hidden cost: `inverse` must yield the same x three years later, on a different CPU, after recompilation. Lock the transformation identity to the residual — mpedb already content-hashes plans; the plan hash belongs in the lineage row.
- The residual becomes small when forward preserves most of the information (hundreds of bytes against a 20 % gain), but the encoder must *detect* non-invertible input and fall back to full source retention — never emit a pair that does not round-trip.
- A residual format is a promise for eternity: Lepton is archived, but 16 billion files must still be decoded. Version the residual format and keep the decoder minimal and bounds-checked (mpedb's "Corrupt, never panic" rule applies doubly here).

### DVC / Pachyderm / lakeFS / MLflow — source retention + lineage instead of inverses

**Mechanism.** All four chose to *keep the source* and correlate via identities, not to invert transformations. **DVC** ([internal files doc](https://doc.dvc.org/user-guide/project-structure/internal-files)) uses a content-addressable cache: the MD5 hash of the content gives the path (`.dvc/cache/files/md5/ec/1d29…`), which dedupes identical files regardless of name; small `.dvc` pointer files and `dvc.lock` in git bind outputs to dependencies via *their hashes*, and the run cache keys runs on (exact dependency content + command). **Pachyderm** ([Global ID doc](https://docs.pachyderm.com/latest/concepts/advanced-concepts/globalid/)) gives every new commit an ID that *all* downstream commits and jobs in the DAG share — the output-commit ↔ input-commit correlation is thus true by construction, exposed in the pipeline as `PACH_OUTPUT_COMMIT_ID` / `<input>_COMMIT`. **lakeFS** ([versioning internals](https://docs.lakefs.io/v1.66/understand/how/versioning-internals/)) builds a two-layer Merkle tree of content-addressed SSTable "ranges" (Graveler); a diff between commits costs proportionally to the change, and lineage across repos is done via commit metadata linking code version to data version. **MLflow** ([tracking doc](https://mlflow.org/docs/latest/ml/tracking/)) separates the backend store (metadata DB) from the artifact store (blobs); artifacts are keyed `run_id` + artifact path, source code via the tag `mlflow.source.git.commit`, and input datasets via `mlflow.log_input` with a Dataset digest/hash.

**What shipped.** All four are in broad production use (Pachyderm bought by HPE; DVC the de facto standard in ML repos; lakeFS on petabyte lakes). What does *not* exist in any of them: inverse transformations. Reproduction is always done by re-running forward from the retained source — inversion was never even attempted as a product feature.

**The convention that won the correlation question:** content hash + lineage table, everywhere. Filename conventions are *nowhere* the primary key — they break on rename, copy and dedup. Sidecar files exist (DVC's `.dvc`), but they are only the *carrier* of the content hash, versioned in git; the key is the hash. Pachyderm and MLflow show the addition: a shared, stable run ID (global commit ID / run_id) that binds the *whole* set of inputs, transformation version and outputs in one lineage row.

**Lessons for mpedb:**
- Correlate output↔source via a lineage table keyed on content hash (blake3 already exists in mpedb), with row `(source_hash, output_hash, plan_hash, residual ref, run ID)`. Generated filename prefixes are out; a sidecar only as a transport form for the hash.
- One stable ID per pipeline run (à la Pachyderm's global ID) makes multi-step lineage trivial to query — do not reconstruct the chain from pairwise rows.
- DVC's run-cache idea transfers directly: (input-hash + plan-hash) → output-hash gives free idempotence on re-ingest of the same file.
- Separate the metadata store from the blob store (the MLflow split): lineage rows and residuals in mpedb tables; large source blobs can live externally, referenced by hash.

### Event sourcing / CDC — reversibility by replay

**Mechanism.** In event sourcing ([Fowler](https://martinfowler.com/eaaDev/EventSourcing.html)) the log is the source retention itself: "We can discard the application state completely and rebuild it by re-running the events from the event log on an empty application." Reversal is done either by snapshot + forward replay, or by each event carrying enough to be reversed — "storing the previous values on any value that is changed, or by calculating and storing differences on the event". The latter *is* the residual idea, stated by Fowler: explicit reversal pays off only "when reversing a few events is much more efficient than using forward play on a lot of events". Documented pain: external side effects during replay, and event versioning — Greg Young's rules ([Versioning in an Event Sourced System](https://github.com/luque/Notes--Versioning-Event-Sourced-System)) are that events are immutable facts, a new version must be convertible from the old (otherwise it is a *new* event), upgrading happens by read-time *upcasting*, and rewrites are done by transforming whole streams into new streams — never editing in place.

**Lessons for mpedb:**
- Replay-from-source beats inverses+residuals when the source is kept anyway — and mpedb's mirror layer already *is* a retained CDC log. Inverses+residuals earn their place in exactly two situations: the source is to be deleted (the Lepton case), or reversal must be cheap and local (one row, not O(log length) replay).
- Fowler's "store previous values on the event" shows that the residual should be captured *at the commit moment* of the forward transformation, in the same transaction — not computed on demand.
- The upcasting rule transferred: a new version of a function pair must be able to consume old residuals, otherwise it is a *new* pair with a new plan hash — and old residuals must keep their old decoder (cf. the Lepton eternity promise).

### Lessons for mpedb (collected)

- **Verification scales with what you dare delete.** If the source is kept: sampled round-trip testing at ingest is enough (à la Lepton's release qualification over 1 billion images). If the source is deleted: 100 % decode-and-bit-compare before commit, like Lepton — sampling is not defensible.
- **Content hash + lineage table won the artifact correlation in every mature tool** (DVC MD5-CAS, Pachyderm global commit ID, lakeFS Merkle ranges, MLflow run_id + dataset digest); filename conventions are nowhere the primary mechanism because they break on rename/copy/dedup. Sidecars are only transport for the hash.
- **Classify the function pairs explicitly** (bijective / invertible-with-residual / lossy) and let the encoder refuse non-invertible input with fallback to full source retention (the jbrd model) — Cambria's undeclared gray zone is where the lens approach broke down.
- **Bind the residual to the transformation identity.** Store the plan hash (mpedb has content-hashed plans) in the lineage row; determinism across compilation/architecture is the hidden cost of byte identity, and version drift happened even at Dropbox.
- **The residual format is a promise for eternity:** version it, keep the decoder minimal, bounds-checked and `Corrupt`-never-panic, and plan for old residuals having to be decoded long after the pair is retired.
- **Do not build inverses where replay is free:** the mirror layer's CDC is already source retention; reversible pairs belong in the conversion flow where the original is deleted or reversal must be O(1) per row.
## Prior art: invertible combinator DSLs and static verification

### 1. Invertible syntax descriptions and partial isomorphisms (Rendel & Ostermann 2010 + the Haskell ecosystem)

**Mechanism/API.** The core is `data Iso a b = Iso (a -> Maybe b) (b -> Maybe a)` — a *pair* of partial functions, composed via three type classes: `IsoFunctor` (`<$> :: Iso a b -> d a -> d b`), `ProductFunctor` (`<*> :: d a -> d b -> d (a,b)` — **tupling, not currying**; partial isos curry poorly) and `<|>`, plus `pure :: Eq a => a -> d a` and `token` ([paper PDF](https://www.informatik.uni-marburg.de/~rendel/unparse/rendel10invertible.pdf), [Hackage invertible-syntax](https://hackage.haskell.org/package/invertible-syntax)). The iso algebra that proved *sufficient* in practice is small: `id`/`(.)` (Kleisli composition over `Maybe`), `×` (product bimap), `associate`/`commute`/`unit` (tuple reshaping), `element x` (constant/default; requires `Eq`), `subset p` (identity narrowed to a predicate — filtering), `inverse`, constructor isos machine-generated with Template Haskell (`defineIsomorphisms`), and `iterate`/`foldl` (fold rewritten into a small-step abstract machine that can run backwards) — hence `many`, `between`, `chainl1`. Two details are especially instructive: (1) `pure`/`element` require `Eq` because *the printer must check that the value being discarded actually equals the constant*; (2) the `p <* q` variants require the ignored part to have type `d ()` — discarding non-trivial information is forbidden, otherwise printing cannot reconstruct it. `subset` on operator precedences gives automatic correct parenthesis insertion when printing ("correct round trip behavior is automatically guaranteed").

**Adoption.** Research-grade: the reference implementation has exponential backtracking and leaks memory (the paper's §4.4 says so itself); [invertible](https://hackage.haskell.org/package/invertible) (total bijections, `<->`/`BiArrow`) has ~5 reverse deps; [codec](https://hackage.haskell.org/package/codec) (applicative field-by-field codec `Codec r w a` = deserializer + serializer) is small. The idea first *shipped* via swift-parsing (see §4).

**The walls.** (a) *Primitive isos are unchecked*: the paper admits that hand-written `Iso` pairs "is neither safe nor convenient … it is not checked that the two directions are really inverse" — exactly the divide the PySpell ladder formalizes. (b) *Context sensitivity*: applicative structure cannot let serialization depend on a parsed value (length prefix!); the solution is monadic profunctors, but then the round-trip properties must be checked separately ([Lysxia: Towards monadic bidirectional serialization](https://blog.poisson.chat/posts/2016-10-12-bidirectional-serialization.html)). (c) *The laws hold modulo equivalence classes* (arbitrary whitespace ↔ one space) — strict `print(parse(x)) == x` does not hold, only up to normalization; they introduced three explicit combinators `skipSpace`/`optSpace`/`sepSpace` to control this. (d) *The type-class interface* gave poor ergonomics; the lesson from [Lysxia: Better invertible syntax descriptions](https://blog.poisson.chat/posts/2016-10-18-typeclass-interface.html) is to derive everything from standard abstractions and keep the bidirectional surface minimal (only `token` was genuinely direction-specific), and that requiring `Alternative`/backtracking of all instances was a mistake.

**Lessons for PySpell:** a small, closed iso algebra (composition, product, sum-per-constructor, `subset` filtering, `element` constants, fold/unfold) covers surprisingly much; constants that are discarded must be *compared*, not assumed, on the way back; information can never be dropped without a `unit` proof; and "user-written Iso pair" is the uncovered rung that must be property-tested.

### 2. Binary-format combinators that actually shipped

**binrw (Rust).** `#[binrw]` / `#[derive(BinRead, BinWrite)]` generate a reader *and* a writer from one specification ([binrw.rs](https://binrw.rs/), [docs.rs attributes](https://docs.rs/binrw/latest/binrw/docs/attribute/index.html)). Directives split sharply: *symmetric from a single annotation* — `magic`, endianness, `pad_before/after`, `align`, `assert` (validated after read / before write), `args`/`import`; *require both directions explicitly* — `map`/`try_map` (read direction and write direction are two lambdas), `parse_with`/`write_with`. Derived fields are handled with `#[bw(calc = expr)]` + `#[br(temp)]`: the field exists on the wire, **is computed from other fields on write and discarded on read** — binrw's answer to residual fields:

```rust
#[binrw]
struct Msg {
    #[bw(calc = data.len() as u32)] #[br(temp)]
    len: u32,
    #[br(count = len)]
    data: Vec<u8>,
}
```

Widely used in reverse-engineering/modding communities ([GitHub](https://github.com/jam1garner/binrw)). *Lesson:* separate combinators into "symmetric by nature" (structure, constants, padding, assertions) and "dual-spec" (arbitrary mappings) — and make the dual-spec variant syntactically heavier, so the user notices they are leaving proven territory.

**Kaitai Struct — parse-only for nine years.** Serialization was requested in [issue #27](https://github.com/kaitai-io/kaitai_struct/issues/27) (opened Sep 13, 2016) and landed only in [v0.11, Sep 2025](https://kaitai.io/news/2025/09/07/kaitai-struct-v0.11-released.html), for Java/Python only, NLnet-funded ([project](https://nlnet.nl/project/Kaitai-Serialization/)). Why did it lag? The spec language allows *arbitrary one-way expressions* (`size: len_field * 4 - 2`, `if: not _io.eof`) that the compiler cannot invert; the design notes ([generalmimon gist](https://gist.github.com/generalmimon/fc22e97faf1fe4b4edc8279b0caa152d), [Serialization Guide](https://doc.kaitai.io/serialization.html)) show two hard findings: (1) lazy/cached *value instances* go stale when a dependency is set — they had to introduce explicit invalidation; (2) sizes cannot be derived automatically — "`if: not _io.eof` doesn't tell us anything – we can set it both ways and it always satisfies itself". Chosen design: the user sets *all* fields (including derived ones) themselves, and generated `_check()` methods validate consistency before `_write()`, on fixed-size streams. *Lesson:* a spec language with non-invertible expressions makes serialization a retrofitting nightmare; "user supplies, system *checks*" is the retreat when you cannot invert — PySpell should rather refuse non-invertible expressions in BIDI mode than inherit this.

**construct (Python).** "Declarative and symmetrical parser and builder" ([GitHub](https://github.com/construct/construct)) — probably the most-used bidirectional DSL in the wild, with operator overloading (`"count" / Byte`) and a context dict flowing through both directions ([context docs](https://construct.readthedocs.io/en/latest/meta.html)). The important three-way distinction: `Computed` (value only in the context, never on the wire), `Rebuild` (on the wire; at *build* the user's value is **overwritten** with one recomputed from the context; at *parse* it is read), `Default` (the user's value wins if supplied), `Check` (assertion both ways):

```python
Struct(
    "count" / Rebuild(Byte, len_(this.items)),
    "items" / Byte[this.count],
)
```

*Lessons:* `Rebuild` is the residual/ancilla pattern in mature form — derived fields should be **recomputed forward and validated/read backward, never stored as truth**; a context identical in both directions is what makes the symmetry possible; and construct shows that a *dynamic* language gets by with runtime errors as long as the error message carries the full path into the structure.

**flat/store vs. codec (Haskell).** [flat](https://hackage.haskell.org/package/flat) and [store](https://hackage.haskell.org/package/store) derive encoder+decoder *generically from the type* — a total bijection by construction, but zero control over the format; [codec](https://github.com/chpatrick/codec) gives format control field by field, but does not check that the field pairing is consistent. *Lesson:* "derive from the type" gives free symmetry; combinator-specified formats buy flexibility at the cost that consistency must be proven or tested — the PySpell ladder should make both levels explicit.

### 3. Static verification of invertibility without dependent types

Three shipped/real systems mark what is actually achievable, sorted by machinery:

**Regular domain → decidable type check.** [Boomerang](https://www.seas.upenn.edu/~harmony/) types string-lens combinators (concatenation, union, Kleene star) with regular expressions and *decides* statically that splitting is unambiguous (used on SwissProt among others; [POPL'08](https://www.cis.upenn.edu/~bcpierce/papers/boomerang.pdf)). [Augeas](https://augeas.net/docs/lenses.html) shipped the same idea industrially (config editing under Puppet et al.): each lens carries four regexes (ctype/atype/ktype/vtype), and the type checker uses the automata library libfa to uncover "vertical/horizontal ambiguities" ([wiki](https://github.com/hercules-team/augeas/wiki/Ambiguities-or-what-do-those-error-messages-from-the-typechecker-mean-%3F)) — but the check is so expensive that it is off by default and runs as a separate test step (`augparse`). *Lesson:* narrowing the language (a regular fragment) is what makes the verification decidable; the wall is exactly context sensitivity (length fields, indentation). And: ambiguity error messages from automata analysis are notoriously cryptic — budget for error-message work.

**SMT on first-order combinator VCs.** [EverParse/LowParse](https://project-everest.github.io/everparse/) proves that generated code is "correct (parsing is the inverse of serialization) and non-malleable (each message has a unique binary representation)" — non-malleability *is* parser injectivity — via F* + Z3, and is used in Microsoft products and for TLS/QUIC ([USENIX'19 paper](https://project-everest.github.io/assets/everparse.pdf)). The architecture point for PySpell: each combinator carries its own injectivity lemma, and composition *preserves* the property — the spec author (the 3D frontend looks like C type definitions) never sees dependent types. The injectivity proofs are in practice first-order conditions (prefix freedom, unambiguous lengths) that SMT crushes automatically.

**The proof-assistant end.** [Narcissus](https://dl.acm.org/doi/10.1145/3341686) (Coq/Fiat, ICFP'19) derives decoder+encoder from one format spec with tactics "in a way that guarantees that they form inverses of each other", demonstrated by swapping out parsing in mirage-tcpip ([arXiv](https://arxiv.org/abs/1803.04870)). Powerful, but expert-cost; no practical path for a Python surface.

**The practical boundary.** By-construction covers in practice: composition of primitives with bundled inverse proofs, product/sum structure, domain narrowing (`subset`), linear/relevant variable use (CoreFun relevance), and linear arithmetic (injectivity of `x*k + c`, `k != 0`, is trivially SMT-checkable). None of the shipped systems do abstract interpretation for injectivity on *general* code — they all narrow the language instead. Everything outside (user-declared forward/inverse pairs, non-linear arithmetic, table lookups) is handled empirically in practice: lens-law testing with QuickCheck/property tests is accepted industry practice where formal proofs are too expensive ([Bidirectionalization for the Common People, 2025](https://arxiv.org/pdf/2502.18954), [Oleg's lens-law notes](http://oleg.fi/gists/posts/2018-12-12-find-correct-laws.html)). This *is* the PySpell ladder: structural proof where the language allows it, property tests as an explicitly marked lower rung.

### 4. Chaining and composition ergonomics

**The Haskell style** (operators `<$>`/`<*>`/`<|>`) yields right-associated nested tuples and type-class error messages that scared users away; Lysxia's two concrete moves — a minimal direction-specific surface and reuse of standard abstractions — are what made the style livable ([blog](https://blog.poisson.chat/posts/2016-10-18-typeclass-interface.html)).

**swift-parsing (Point-Free)** is the mature heir and explicitly inspired by Rendel–Ostermann ([GitHub](https://github.com/pointfreeco/swift-parsing)): parsers compose in result-builder blocks, and a *one-line change* — `Parse(User.init)` → `ParsePrint(.memberwise(User.init(id:name:isAdmin:)))` — upgrades a parser to a parser-printer; the `Conversion` protocol (`apply`/`unapply`, both may throw) is their partial isomorphism. The round-trip contract is documented precisely and *two-way*: "For every `input` for which `p.parse(input)` does not throw, `p.print(p.parse(input))` is equal to `input`" and symmetrically for print-parse ([Roundtripping.md](https://github.com/pointfreeco/swift-parsing/blob/main/Sources/Parsing/Documentation.docc/Articles/Roundtripping.md)). *Lessons:* the migration ergonomics ("same pipeline, swap the constructor") is gold for adoption; the round-trip laws must stand as an explicit contract per combinator; and compile errors in deeply generic builder chains are the weak point — a runtime-verified Python surface can actually give *better* errors by pointing at exactly which link in the chain broke invertibility.

**Python prior art for invertible pipelines** exists, but in the ML camp: [TFP bijectors](https://www.tensorflow.org/probability/api_docs/python/tfp/bijectors/Chain) is a large-scale-shipped combinator DSL where each `Bijector` has `forward`/`inverse` (+ Jacobian), `Chain`/`JointMap`/`Invert` compose them, composition is bijective by construction — and an input/output *cache* makes `inverse(forward(x))` return `x` exactly instead of computing a floating-point inverse ([Composition](https://www.tensorflow.org/probability/api_docs/python/tfp/bijectors/Composition)). PyTorch's [`torch.distributions.transforms`](https://github.com/pytorch/pytorch/blob/main/torch/distributions/transforms.py) is the same shape: `Transform.inv` is a *first-class value*, `ComposeTransform` inverts by reversing and inverting the links, the `bijective` flag and `cache_size=1` are part of the API. [python-lenses](https://github.com/ingolemo/python-lenses) shows optics composition with the `&` operator and prisms built from `wrap`/`unwrap` pairs; [bidict](https://bidict.readthedocs.io/) is the trivial endpoint (an `.inverse` attribute). *Lessons:* in Python, object-with-`.inverse`-property + explicit `Chain`/`|` composition wins over operator acrobatics; the inverse should be a value you can pass around, not a mode; and the caching trick (remember the forward result, do not recompute the inverse) is the same idea as residual storage — exactness at round trip is a *design* responsibility, not a numerical prayer.

### Lessons for PySpell strict-BIDI

- **The iso algebra can be kept small**: composition, product, sum-per-constructor, `subset` filtering, `element` constants and fold/unfold covered everything in Rendel–Ostermann's case study; do not design for more until someone needs it.
- **Discarded information must be proven trivial or compared**: `pure`/constants require an equality check on the way back (the Eq requirement); dropping non-`unit` values should be a type error in BIDI mode.
- **Residual/ancilla has a mature model**: construct's `Rebuild` and binrw's `calc`+`temp` — derived fields are recomputed forward and validated/consumed backward; they are never stored as authoritative state. Kaitai's `_check()` design is the fallback when expressions cannot be inverted — avoid ending up there by refusing non-invertible expressions up front.
- **The Kaitai lesson in one sentence**: a spec language that allows arbitrary one-way expressions makes inversion a nine-year retrofitting project; the strict-BIDI dialect must be restrictive from day one.
- **The verification ladder matches shipped practice**: (1) by-construction for combinator composition with per-primitive inverse proofs (the EverParse model: each link carries its injectivity lemma, composition preserves it); (2) decidable checking only in narrowed domains (Boomerang/Augeas' regular fragment; linear arithmetic via a simple solver); (3) user-declared forward/inverse pairs are property-tested — accepted industry practice, but it must be a *visible* lower rung in the API.
- **Context sensitivity is the wall for applicative structure**: length prefixes/dependent fields require monadic (sequential) form, and then the round-trip property must be re-established per construction — plan for `let`-bound intermediate values in the IR that are relevance-checked (CoreFun) instead of free monadic bind.
- **Ergonomics provably working in Python**: objects with `forward()`/`inverse` where the inverse is a first-class value, pipeline composition (`Chain`/`|`), one-line upgrade from one-way to two-way (swift-parsing's `Parse` → `ParsePrint`), and runtime errors that point at exactly which chain link broke the contract (construct path errors > Haskell type errors).
- **Document the round-trip contract two-way and per combinator** (swift-parsing's formulation is the template), and consider the TFP/PyTorch trick: exactness at round trip is ensured by carrying the residual along, not by computing your way back.
## Prior art: incremental view maintenance and stream processing in DBs

### 1. IVM engines: DBSP/Feldera, Materialize, Noria

**Mechanism.** [DBSP (Budiu et al., VLDB 2023, best paper)](https://www.vldb.org/pvldb/vol16/p1601-budiu.pdf) models the database as a stream of snapshots `DB[t]` and changes as **Z-sets**: functions from rows to integer weights with finite support, where negative weight = deletion; `ΔDB[t] = DB[t] − DB[t−1]`. Every query `Q` gets an incremental version `Q^Δ = D ∘ ↑Q ∘ I` (differentiation ∘ lifted query ∘ integration) with the central guarantee that **the accumulated output of the incremental circuit is identical to running `Q` from scratch on the accumulated input** — and the chain rule says you incrementalize a composite query by incrementalizing each subquery independently. Linear operators (filter, map, projection) are their own incremental version — cost ∝ |Δ|; bilinear ones (join) need integrated state: `Δ(a⋈b) = Δa⋈Δb + a⋈Δb + Δa⋈b`. [Materialize](https://materialize.com/blog/strong-consistency-in-materialize/) builds on differential dataflow with **virtual time**: every update is a triple `(data, time, diff)`, all updates in one source transaction get the same timestamp, and [all views are always consistent with the input at one specific virtual point in time](https://materialize.com/blog/virtual-time-consistency-scalability/). [Noria (OSDI 2018)](https://pdos.csail.mit.edu/papers/noria:osdi18.pdf) is the dataflow DAG with **partial materialization**: operators hold only partial state; evicted/missing state is reconstructed on demand with **upqueries** back toward the base tables — the lazy alternative in pure form.

**Pain/success.** DBSP is machine-checked in Lean and requires only that changes form a commutative group (updates are expressed as retract+insert). [Feldera achieves exactly-once via periodic checkpoints + input/output journaling: recovery loads the last checkpoint, replays input and discards duplicated output](https://docs.feldera.com/pipelines/fault-tolerance/). Materialize argues that eventual consistency is in practice "no guarantee" when the changes never stop — snapshot consistency is the essential thing, freshness is the negotiable one. [Noria](https://blog.acolyer.org/2018/10/29/noria-dynamic-partially-stateful-data-flow-for-high-performance-web-applications/) chose the opposite: only eventual consistency ("if the writes quiesce, views converge"), and paid instead with complexity in the upquery protocol — upquery responses do not commute with in-flight updates, so they had to guarantee that no updates were in flight between upstream state and a join during an upquery, require **deterministic operators** and **commutative merges** in multi-ancestor operators. Result: 5× MySQL on Lobsters, ~3× memory overhead.

**Lessons for mpedb.** (1) The delta model should be retract/insert with weights — an UPDATE in A is delete+insert in the CDC stream; that makes transforms composable (B→C) and reversible. (2) The DBSP theorem is a free fuzz oracle: **the result must be independent of batch boundaries** — fuzz arbitrary partitionings of the CDC stream into batches and require identical B. (3) Keep the Materialize guarantee "B = F(A @ watermark snapshot)" and sacrifice freshness; not the Noria guarantee "correct at some point in the future". (4) Noria shows that lazy moves all race complexity into the upquery protocol; deterministic user functions are a hard prerequisite in all three systems.

### 2. Classic in-DB triggers: PostgreSQL, SQLite sessions, Service Broker

**Mechanism.** PG triggers run **synchronously in the writer's transaction**, row-level triggers fire per row (10 000 inserts = 10 000 calls), and [cascades arise when trigger SQL fires new triggers](https://www.postgresql.org/docs/current/trigger-definition.html). [LISTEN/NOTIFY](https://neon.com/guides/pub-sub-listen-notify) is transient signaling: no persistence, lost if nobody is listening (at-most-once), 8000 bytes payload. Queue-in-table is canonically solved with [`FOR UPDATE SKIP LOCKED`](https://www.dbpro.app/blog/postgresql-skip-locked) — atomic, race-free job claiming (Que, Oban et al. build on this). [SQL Server Service Broker](https://learn.microsoft.com/en-us/sql/database-engine/configure-windows/sql-server-service-broker?view=sql-server-ver17) is transactional queues in the DB with "activation" (spawns a stored procedure when messages arrive) — an in-DB daemon precedent.

**Pain/success.** [GitGuardian's production story](https://blog.gitguardian.com/love-death-triggers/): triggers that precomputed aggregates gave invisible cascades, almost no trace in logs, and unpredictable locking problems after a year — they migrated to asynchronous Celery workers with second-level lag and concluded that triggers must be trivial and code-reviewed like ordinary code. [Cybertec's benchmarks](https://www.cybertec-postgresql.com/en/are-triggers-really-that-slow-in-postgres/) show that the trigger *mechanism* is cheap — the cost is the work performed synchronously in the write path. The strongest data point against in-commit side work: [recall.ai found that NOTIFY takes a global exclusive lock in the commit phase that serializes ALL commits](https://www.recall.ai/blog/postgres-listen-notify-does-not-scale) — three outages, CPU/IO plummeted while the database sat waiting on one mutex. [SKIP LOCKED queues at scale](https://richyen.com/postgres/2026/05/04/postgres_job_queue.html) meet an MVCC mismatch: every claim/complete is a WAL-logged transaction, dead tuples, vacuum pressure. The Service Broker experience is [consistently "too many concepts, impossible to debug"](https://www.sqlservercentral.com/forums/topic/in-praise-of-service-broker).

**Lessons for mpedb.** (1) **Evidence against in-commit transforms**: in a single-writer DB, every µs of transform extends the hold time on the global writer lock for *all* processes; the PG world fled from even cheap synchronous side work (recall.ai, GitGuardian). In-commit is defensible only for validation/CHECK-level work and cheap CDC *capture*. (2) The pattern everyone converges on is "durable queue in the same DB + worker claims atomically + result and cursor committed together" — mpedb's CDC log + one daemon is degenerate (simpler) SKIP LOCKED: one consumer, zero claim contention. (3) Notify-like wakeup should be a pure optimization (a hint), never correctness-bearing: the daemon must poll the cursor; an at-most-once signal + durable log is sufficient. (4) Cascades (A→B→C) must be an explicit, ordered, depth-bounded DAG with runnable introspection (cursor, per-function errors as queryable tables) — not implicit triggers-on-triggers, which is the documented debugging nightmare.

### 3. Embedded/serverless precedents: sqlite hooks, DuckDB, litestream, cr-sqlite

**Mechanism.** [`sqlite3_update_hook`](https://www.sqlite.org/c3ref/update_hook.html) fires only **in the same process, on the same connection** that makes the change, misses WITHOUT ROWID/truncate optimization/REPLACE conflict deletion, and the callback may not modify the database — i.e. the application must drive the processing itself after commit; cross-process notification does not exist. [The sessions extension](https://sqlite.org/sessionintro.html) captures changesets on the writer connection and applies them with a conflict handler on the receiving side. [DuckDB has no triggers](https://github.com/duckdb/duckdb/discussions/12562) — no official rationale is published, but the community consensus is that it does not belong in the OLAP design. SQLite's only "background work", [WAL auto-checkpoint, is cooperative: it is performed by the committing connection that crosses the threshold](https://sqlite.org/wal.html) — with documented **checkpoint starvation** when there are always active readers (the WAL grows without bound). [Litestream](https://litestream.io/how-it-works/) is an explicit sidecar daemon that takes over checkpointing via a long-lived read txn + a shadow WAL; asynchronous replication with [a ~1 s loss window, and a hard requirement of exactly one replicating process per DB — two instances yield corruption](https://litestream.io/tips/). [cr-sqlite](https://github.com/vlcn-io/cr-sqlite) splits: *capture* happens in-commit (triggers + clock metadata tables are written inside the writer's txn), while *merge/sync* is application-driven pull with Lamport clocks and CRDT convergence.

**Pain/success.** The embedded ecosystem's answer to "who owns background work" is thus three-way and maps exactly onto our options: **in-commit** (sqlite triggers, cr-sqlite capture — works because capture is cheap and deterministic), **explicit daemon** (litestream, mpedb's `mirror pull` — operationally proven, but progress requires the daemon to be running), and **cooperative-on-commit** (WAL checkpoint — requires no extra process, but steals latency from a random writer and starves under continuous load).

**Lessons for mpedb.** (1) In a no-server model, hooks cannot carry cross-process ETL — **the durable CDC log in shared storage is the only reliable channel**; mpedb already has it. (2) The cr-sqlite split is right: in-commit does only cheap, deterministic capture (the CDC entry); the transform is deferred. (3) If a daemon is chosen: enforce single-instance with a lease/lock in shm (litestream's corruption warning; mpedb has the robust mutex + pid identity already) and **fuzz two daemons racing**. (4) Spawn-on-attach is fragile (unclear ownership, dies with the process); an explicit command à la `mpedb rretl run` — the same model as `mirror pull` and litestream — is the normalized operating model. Cooperative-on-commit can at most be an opportunistic "top-up", never the primary progress mechanism (the starvation precedent).

### 4. Cursor/watermark + idempotence: Kafka, Debezium, Flink

**Mechanism.** [Kafka's exactly-once](https://www.confluent.io/blog/exactly-once-semantics-are-possible-heres-how-apache-kafka-does-it/) rests on an idempotent producer + transactions, where the key trick is to **commit consumer offsets in the same transaction as the output**; `read_committed` hides open transactions. [Debezium](https://debezium.io/documentation/faq/) persists its own offset bookmark into the source's change log; crash ⇒ resume from the last stored offset ⇒ **at-least-once, duplicates are documented as normal and visible downstream**; [incremental snapshots (the DBLog pattern) write low/high watermarks into the log around chunk reads to reconcile snapshot against stream, resumable per chunk](https://debezium.io/blog/2021/10/07/incremental-snapshots/). [Flink](https://nightlies.apache.org/flink/flink-docs-stable/docs/concepts/stateful-stream-processing/) checkpoints operator state **and source offsets atomically** via Chandy-Lamport barriers; recovery = restore state + replay from offsets; watermarks are an orthogonal event-time completeness signal. The shared law: **exactly-once effect = at-least-once delivery + idempotent/transactional apply** — nobody delivers "exactly-once delivery".

**Pain/success.** [Offset loss or corruption yields either replay (duplicates) or gaps](https://risingwave.com/blog/debezium-offset-management-guide/); true 2PC sinks are so expensive that everyone in practice prefers idempotent sinks. [Feldera's exactly-once](https://docs.feldera.com/pipelines/fault-tolerance/) is the same recipe: checkpoint + input replay + dedup of already-produced output.

**Minimal robust arrangement for mpedb**: (1) The daemon does everything in **one destination write txn**: apply the transform output for the CDC interval `(T_from, T_to]` + set `cursor = T_to`. SIGKILL anywhere ⇒ either committed (cursor advanced) or not (redo from the same cursor); redo is safe because apply is key-idempotent and the run cache `(input-hash, fn-hash) → output-hash` skips recomputation. This is Kafka's offsets-in-the-output-transaction — and mpedb gets it for free because B and the cursor live in the same DB. (2) Expose the watermark as metadata: "B is consistent as of txn T" (the Flink-watermark/Materialize-frontier analogue) — that makes lazy-like reading possible as pure policy ("wait until watermark ≥ my snapshot" or "read what is there"). (3) Determinism must be enforced, not assumed: re-run the function on the same input hash and require the same output hash — the run cache is worthless (and convergence impossible) with non-deterministic functions. (4) The fuzz list beyond mirror-collide SIGKILL: **batch-boundary randomization** (DBSP: the result must be batch-independent), **two competing daemons** (lease violation), **delete/update-heavy workloads** (the retraction path, not just inserts), **kill between cascade steps B→C**, and as the final oracle: drain and compare B against from-scratch `F(A)` — identical to Feldera's guarantee and the mirror-collide oracle.

### Lessons for mpedb triggers

- **Choose the daemon as the primary model**: all production systems (Kafka, Debezium, Flink, Feldera, litestream, GitGuardian's retreat from triggers) converge on "durable log + cursor + idempotent replay". The mirror daemon is the precedent; the ETL daemon is the same machine with a transform in the middle.
- **In-commit only for capture, never transform**: in the single-writer model, transform work extends the writer-lock hold time for all processes; the PG evidence (recall.ai's NOTIFY global lock, the GitGuardian cascades) shows that even "cheap" synchronous side work in the commit path becomes the serialization point.
- **Lazy as a complement, not a foundation**: Noria proves it works, but all the race complexity moves into the upquery protocol — and in mpedb a lock-free reader cannot persist results without taking the writer lock. Lazy = policy over the watermark ("compute-on-read without persist", or read-triggered prioritization of the daemon), not a separate write path.
- **Atomic cursor+output in one txn is the whole crash-safety story** — free when B and the cursor share a DB. Do not build anything more complicated.
- **The guarantee that is kept: B = F(A @ watermark)** (snapshot consistency, Materialize); what is sacrificed: freshness. Never Noria-style "eventually correct" without a defined snapshot.
- **Delta = retract+insert with sign** (the Z-set model): an UPDATE in A is delete+insert in the stream; that makes B→C chains composable and reversal well-defined.
- **Determinism is contractual**: the run-cache hashes give both enforcement (re-run, compare output hash) and free dedup at replay.
- **Wakeup is a hint, the cursor is the truth**: polling against the cursor carries the correctness; any signaling is only latency optimization.
- **New fuzz surface beyond SIGKILL-collide**: batch-boundary randomization, double daemon (lease), retraction-heavy workloads, kill between cascade steps, determinism re-check — the final oracle is always drain + from-scratch `F(A)` comparison.
## Prior art: code stored in the database

### 1. Stored procedures (PL/pgSQL, T-SQL, PL/SQL)

**Mechanism.** Procedures are stored as schema objects in the database catalog and deployed with `CREATE OR REPLACE` — the database keeps only the *latest* version, with no history. Calls happen by name; the logic runs in the server process with transactional access to data. One named procedure = one mutable slot; changing it changes the behavior for all callers immediately.

**Adoption reality and documented pain.** The core criticism after 40 years is strikingly consistent: (1) *no version history* — "if business logic spans across multiple stored procedures then it can be very difficult to establish the exact combination of different versions of different stored procedures at a given point in time" ([dusted.codes](https://dusted.codes/drawbacks-of-stored-procedures)); (2) *untestability* — logic cannot be separated from data, mocking is impossible, unit-test attempts were largely abandoned ([nkdagility](https://nkdagility.com/resources/blog/stop-writing-business-logic-in-stored-procedures/)); (3) *hidden business logic* outside code review/CI, and (4) *vendor lock-in* ([Medium/Binary Notes](https://medium.com/binary-notes/why-using-stored-procedures-is-not-recommended-for-modern-applications-41a8b9c17ba4)). The classic drift failure mode: a hotfix `CREATE OR REPLACE` straight in prod makes the environments diverge from the migration history, with deploy failures and bugs that only reproduce in one environment ([Liquibase on drift](https://www.liquibase.com/blog/database-drift), [danielnolan.io](https://danielnolan.io/database-drift-and-migrations/), [odetocode 2008](https://odetocode.com/blogs/scott/archive/2008/02/02/versioning-databases-views-stored-procedures-and-the-like.aspx)). But they *remained* the right tool for data-near transformations: set-based processing of millions of rows where the data does not need to leave the database, integrity rules that must apply no matter which application writes, and short transactions under the engine's control ([yugabyte/dev.to](https://dev.to/yugabyte/triggers-stored-procedures-for-pure-data-integrity-logic-and-performance-1eh8), [codidact](https://software.codidact.com/posts/285745/285752)). That is exactly PySpell's niche.

**How modern teams actually handle versioning — and the convergence on content hashing.** Flyway solves the sproc problem with *repeatable migrations*: the procedure's text lives in one file in VCS and is re-applied every time the **checksum** changes; filename + checksum are stored in `flyway_schema_history` ([Redgate doc](https://documentation.red-gate.com/fd/repeatable-migrations-273973335.html)). Liquibase does the same with `runOnChange="true"` and an MD5 checksum in `DATABASECHANGELOG` ([Liquibase doc](https://docs.liquibase.com/concepts/changelogs/attributes/runonchange.html)). The industry has thus already *converged on checksum-identified procedure code* — but as an external layer glued onto a mutable name slot. mpedb's `etl/<hash>` registry builds the same principle into the storage model itself, which removes the entire drift class: there is no mutable slot to drift.

**Lessons for mpedb.**
- Never `CREATE OR REPLACE` semantics: an edit = a new hash, old versions are kept. The drift failure class (hotfix in prod ≠ VCS) cannot arise when the code is content-addressed.
- Offer `mpedb` CLI export/import of PySpell source as *text files* so the code can live in git and be reviewed — the Flyway/Liquibase lesson is that DB-stored code dies without a frictionless bridge to VCS and code review.
- Make the call site explicitly versioned: pipelines pin a hash (as planned), interactive callers resolve name→hash at call time. That recreates "which combination of versions ran back then?" — something sprocs never could answer.
- Testability must be designed in: PySpell functions should be runnable against synthetic rows without a full database around them (a pure IR evaluator), otherwise we inherit sproc untestability.

### 2. Immutable/content-addressed code registries (Unison, Nix, Docker digests)

**Mechanism.** Unison identifies each definition by a 512-bit SHA3 hash of the syntax tree, after named arguments are replaced with positional references and all dependencies with *their* hashes; "names are just separately stored metadata that don't affect the function's hash" ([unison-lang.org/docs/the-big-idea](https://www.unison-lang.org/docs/the-big-idea/)). Names are pointers into an immutable address space; an edit yields a new hash, and the old definition persists (only the label moves) ([SoftwareMill experience report](https://softwaremill.com/trying-out-unison-part-1-code-as-hashes/)). Nix builds store paths as hash-of-all-inputs **+ a human name** (`/nix/store/lz9g…-hello-2.12.1`); Docker separates mutable *tags* from immutable *digests*.

**Adoption reality and documented pain.** Unison's gains are documented as real: rename is a pure metadata operation, the diamond-dependency problem disappears (two versions coexist as two hashes), typecheck results and test results on pure functions are cached permanently per hash ([the-big-idea](https://www.unison-lang.org/docs/the-big-idea/)). The pain is documented too: (1) when a definition changes, *dependents* must be updated — UCM generates a "todo" list and a dedicated update branch, and this `update` flow is what they have spent the most years polishing ([update doc](https://www.unison-lang.org/docs/ucm-commands/update/), [workflow doc](https://www.unison-lang.org/docs/usage-topics/workflow-how-tos/update-code/)); (2) raw hashes leak into the user surface ("#hmt4gnn927" in scratch files) and feel cryptic — matching old hash references against new names is an admitted pain point ([SoftwareMill](https://softwaremill.com/trying-out-unison-part-1-code-as-hashes/), [Unison releases](https://github.com/unisonweb/unison/releases)). The Nix lesson: the hash alone is meaningless to humans, so every store path carries a name suffix as a disambiguator ([Tweag on derivation outputs](https://www.tweag.io/blog/2021-02-17-derivation-outputs-and-output-paths/)). The Docker lesson: mutable tags gave non-deterministic deploys and supply-chain holes (unchanged `node:9` tag, suddenly broken yarn); digest pinning is now recommended practice everywhere ([candrews](https://candrews.integralblue.com/2023/09/always-use-docker-image-digests/), [Mend](https://www.mend.io/blog/overcoming-dockers-mutable-image-tags/)) — but *nobody* references images by digest alone; everyone keeps tag+digest together.

**Lessons for mpedb.**
- Store name→hash bindings as mutable metadata over immutable hashed code, Unison-style; never mutate code in place. This is already the design — prior art confirms it is the right core.
- Never show raw hashes as the primary identity: all CLI/SDK surfaces show `name@shortversion` (or `name#short-hash` à la Nix's `hash-name`), with the full hash available for pinning. Unison's biggest ergonomics problem was hashes in the user's face.
- Content addressing gives free caching: validation results, compiled IR and (for pure functions) test results can be cached per hash and never invalidated — the same property mpedb already exploits for query plans.
- Have a `todo` equivalent: when someone publishes a new version of a function other pipelines depend on, the system must be able to *list* dependents that still pin the old hash — it is a query, not a crisis, precisely because old hashes never stop working.

### 3. Sandboxed user code in modern databases

**Mechanism.** Postgres divides procedural languages into *trusted* (can be given to ordinary users; no filesystem/network access, e.g. plv8) and *untrusted* (superuser only; PL/Python exists *only* as `plpython3u` because Python cannot be sandboxed defensibly in-process) ([postgresql.org/docs/current/plpython](https://www.postgresql.org/docs/current/plpython.html)). SQLite runs application-defined functions in-process without a sandbox, but requires an explicit `SQLITE_DETERMINISTIC` flag for a function to be usable in indexes, generated columns and partial-index WHERE ([sqlite.org/deterministic](https://sqlite.org/deterministic.html)). WASM-in-DB (libSQL, SingleStore, ScyllaDB) compiles UDFs to WASM and runs them in a runtime (typically Wasmtime): a linear memory model with a hard cap, no ambient I/O (no syscalls, files, network, threads), and CPU limiting via *fuel* (a deterministic instruction budget, ~2x overhead) or *epochs* (cheap timer-based interruption, non-deterministic) ([Wasmtime Config](https://docs.wasmtime.dev/api/wasmtime/struct.Config.html), [Bytecode Alliance on performance](https://bytecodealliance.org/articles/wasmtime-10-performance), [ScyllaDB on the Wasmtime choice](https://www.scylladb.com/2022/04/14/wasmtime/)).

**Adoption reality and documented pain.** The trusted/untrusted divide is the most important organizational lesson: without a trusted language, user code becomes a superuser privilege and dies in practice. SingleStore sets 16 MB of memory per Wasm sandbox as the default, extensible per function with `GROW TO` ([SingleStore doc](https://docs.singlestore.com/cloud/reference/code-engine-powered-by-wasm/)). The Wasmtime documentation is explicit that the sandbox prevents *escape* but not *resource consumption* — CPU and memory caps must be added separately ([systemshardening on epoch interruption](https://www.systemshardening.com/articles/wasm/wasmtime-epoch-interruption-security/)). The determinism side has two documented traps: Postgres functions mislabeled `IMMUTABLE` cause index corruption and stale constant folding in cached plans ("if you lie to the planner, it will get its revenge") ([xfunc-volatility](https://www.postgresql.org/docs/current/xfunc-volatility.html), [pgsql-hackers](https://www.postgresql.org/message-id/469536.1633969664@sss.pgh.pa.us)); and the SQLite planner only dares optimize functions that themselves declare determinism ([create_function](https://www.sqlite.org/c3ref/create_function.html)).

**Lessons for mpedb.**
- PySpell must be mpedb's *trusted language*: a bounded IR without ambient I/O is exactly the property that lets anyone — not just an admin — store and run functions. Do not build any "untrusted escape hatch"; that is what forced PL/Python into the superuser ghetto.
- Mirror the WASM-UDF limits explicitly in the IR evaluator: an instruction budget (the fuel model — deterministic, and the overhead is acceptable for ETL granularity), a hard memory cap per invocation with a default + per-function override (à la SingleStore's 16 MB/`GROW TO`), and no implicit access to clock/randomness/filesystem. Sandbox ≠ resource control; both are needed.
- Determinism must be a *declared and enforced* property per function, not an assumption. Everything that goes into indexes, CHECK expressions or cacheable results requires the determinism flag — and since the IR is bounded, mpedb can *verify* the flag statically instead of trusting the author, which is a notch better than both SQLite and Postgres.
- Because the function is identified by hash, (hash, params) → result is a valid cache key *only* for deterministic functions — tie cacheability to the verified flag.

### 4. Multi-user editing of DB-stored code

**Mechanism.** Systems that work expose three things: *who/when/why* per change, a *diff surface* between versions, and a *suggestion/merge mechanism*. Postgres itself has nothing built in (a `CREATE OR REPLACE` erases the history); pgAudit bolts on logging of DDL/FUNCTION classes with user, timestamp and statement ([pgAudit at Supabase](https://supabase.com/docs/guides/database/extensions/pgaudit)). The migration tools store a minimal but sufficient metadata set per change: id/description, checksum, author, installed_by, installed_on ([Flyway](https://documentation.red-gate.com/fd/repeatable-migrations-273973335.html), [Liquibase](https://docs.liquibase.com/concepts/changelogs/attributes/runonchange.html)). Observable gives each notebook a history pane with rollback, and collaboration via "Fork, Suggest, Merge": suggestions are sent with a short *note*, and the owner can accept per cell by toggling between the Parent and Fork versions ([Observable collaboration](https://observablehq.com/documentation/collaboration/), [history](https://observablehq.com/documentation/notebooks/history)).

**Adoption reality and documented pain.** Smalltalk is the 40-year warning: code lived in an image with a changeset log that was "barely structured text", an error-prone sharing medium; the ecosystem ended up building real VCS (ENVY, Monticello), and modern practice is "commit source, rebuild images from source" ([HN/Lobsters threads on On Learning Smalltalk](https://news.ycombinator.com/item?id=29890205), [Bracha: An Image Problem](https://gbracha.blogspot.com/2009/10/image-problem.html)). Databricks went the same way: built-in automatic revision history still exists, but the legacy notebook-Git integration was removed in January 2024 in favor of Git folders — the internal history became a convenience, git became the truth ([Databricks doc](https://docs.databricks.com/aws/en/notebooks/notebook-version-history), [Microsoft Learn](https://learn.microsoft.com/en-us/azure/databricks/archive/repos/git-version-control-legacy)). Observable's lesson is that users got by with surprisingly little: full version history + fork + a free-text note per suggestion + per-cell diff — not galaxies of branching.

**Lessons for mpedb.**
- A minimal metadata set per version, immutable and stored next to the hash: `author, timestamp, comment, prev_hash`. The `prev_hash` link makes the history a chain that can be diffed and audited — that is exactly the field set Flyway/Liquibase converged on, plus the link that content addressing makes free.
- The diff surface is source-against-source, not hash-against-hash: `mpedb rretl diff <name>@v1 <name>@v2` must show a PySpell text diff. Smalltalk changesets failed because the change log was not readable as a diff.
- DB history is convenience, git is truth: both Smalltalk and Databricks ended up there. Keep the in-DB version chain for audit and rollback, but make text export to git first-class (cf. lesson 1).
- Editing someone else's function should be a suggestion, not an overwrite: since an edit creates a new hash anyway, "suggest" is just *a new hash + note + prev_hash pointer without moving the name binding* — the owner moves the binding on acceptance. Observable shows that this + a comment field is the entire mechanism users actually need.

### Lessons for mpedb (collected)

- **Immutability is the main win**: the entire sproc drift failure class (hotfix ≠ VCS, environments diverge, "which version ran?") exists only because `CREATE OR REPLACE` mutates a name slot. `etl/<hash>` + pin-by-hash eliminates the class structurally — do not weaken this with any "replace in place" shortcut.
- **Name→hash binding as mutable metadata over immutable code** (the Unison model), and never show a raw hash as the primary identity — always `name@version/short-hash` (Nix/Docker: hash *and* human name, always together).
- **Checksum-keyed deploy is already the industry standard** (Flyway repeatable migrations, Liquibase `runOnChange`) — mpedb merely internalizes the mechanism. Keep the bridge they built it for: frictionless text export/import against git and code review.
- **Sandbox ≠ resource control**: a bounded IR additionally needs an instruction budget (fuel), a hard memory cap with per-function override, and zero ambient I/O (clock, randomness, filesystem) — the limits all the WASM-UDF systems ended up at.
- **Determinism as a verified flag, not a promise**: everything that is cached, indexed or used in CHECK requires it, and mislabeling is documented index corruption in Postgres. mpedb can verify the flag statically in the IR — do it.
- **Per version: `author, timestamp, comment, prev_hash`** — none of the systems needed more metadata; less made the history useless.
- **Be the trusted language for all users**: no untrusted escape hatch. Untrusted languages become admin privileges and die (the PL/Python lesson).
- **The dependents query**: a new version of a function must be able to list which pipelines still pin the old hash (Unison's `todo`) — safe precisely because old hashes never stop working.
## Addendum 2026-07-28 — sweep after stage 1 (Jeopardy, Hermes, CRIL, RC 2023–2025, Sparcl JFP, wire formats, lineage practice)

Run as pre-work for stage 2 (residual + lineage), six targeted sweeps over what
the main sweep of 16/7 did not cover. The cross-cutting main finding, and the most
important one: **none of the reversible languages — not even the newest — has a
story for PERSISTING the complement.** They keep it inline in the output (Sparcl),
derive it from context at the inversion point (Jeopardy), or forbid it from
existing (Hermes). The residual-as-stored-data is uncontested territory; commitments
5–9 compete with no one, and stage 2 builds something the literature does not have.

### Jeopardy (Kristensen/Kaarsgaard/Thomsen; RC 2024, LNCS 14680; arXiv 2209.02422, 2212.03161)

Global-not-local invertibility: locally non-injective (even
non-deterministic) operations are allowed when the context can decide the inversion.
The mechanism is the "available implicit arguments" analysis — available-expressions
dataflow run bidirectionally (seeded both from `main{input}` and
`invert main{output}`), fixpoint over call configurations. The complement unit
is the BRANCH CHOICE (which case arm fired), and Bennett history is explicitly rejected
as "extra unwanted data": instead, the function is specialized at compile time to
copy an already-available input into the output. The complement never exists
as its own runtime object — consequently zero serialization, zero format, zero
versioning. Two lessons: (1) the analysis is conservative and partial (one
recursion-step lookback; undecidable in general — Rice), which REINFORCES
our runtime verification-with-named-counter-example over static trust; (2) the idea
is a **residual elision analysis**: a residual component that provably can be
derived from columns already present in the output row need not be stored — for
mpedb a corpus-verified (not static) stage-3+ optimization, never a
contract change.

### Hermes (Mogensen; RC 2020, SCP 2022) and CRIL (Oguchi & Yuen, EPTCS 387, 2023)

Hermes is reversibility by construction (only `+=`/`-=`/`^=`/rotations/swaps,
static anti-aliasing), and the copyable part is the ancilla discipline: local
variables are born zero and are RUNTIME-CHECKED to be zero at deallocation
(`disposelocation_z`) — uncomputation is enforced, not assumed. Transferred: a
`Bijective`-declared pair gets a cheap runtime assert on an empty residual at
every forward (stage 2, defense in depth against corpus-blind holes). CRIL
(Concurrent Reversible Intermediate Language — "Concurrent", not "Clean") shows
the warning's shape: with shared mutable state, a causality DAG over
updates must be accumulated at runtime — THE EXECUTION SCHEDULE ITSELF becomes residual data.
Per-row residuals remain order-free ONLY when spells are row-independent; PySpell
without shared state between calls gives us that for free today, and `rretl apply` must never
introduce a shared accumulator without knowing that the schedule then has to go into the residual.

### RC 2023–2025 otherwise

The programming-language share is small (~4–5 papers/year). "Towards Clean
Reversible Lossless Compression" (RC 2024, Glück/Yokoyama et al.): garbage-free
reversible LZW in Θ(n), but clean BWT pays ~n³ against the irreversible one — **the price
of zero complement can be polynomial, and that is exactly what storing a
residual buys us out of.** Hybrid SSA (RC 2024) makes non-reversibility
first-class in a reversible IR — the same partially-reversible pipeline shape as
the lens design (only verified-bijective steps drop the residual). No
theoretical successor to the minimal-garbage undecidability; the result stands.
The pattern across the years: garbage is avoided by RECOMPUTATION (time against space), never
for free.

### Sparcl JFP 2024 (Matsuda & Wang; + Kalpis ESOP 2024, Bifrons 2025)

The journal version changes nothing semantically (its own delta list: Agda formalization,
arithmetic coding/LZ77 examples, updated related work). `lift` is still
unverified ("by nature unsafe"); the best offer is `safeLift`'s dynamic
round-trip check — **weaker than our corpus verification with a named counter-example at
declaration time.** Complements ride inline in invertible output (the Huffman table
travels in the pair) and residuals are CLOSURES — by construction unserializable.
Successors: Kalpis (ESOP 2024; partial invertibility as an effect,
Agda-mechanized), Bifrons (2025; a symmetric-lens library + lens TESTING against
heterogeneous databases — closest to us in spirit, still without a persisted complement).

### Wire formats: pristine-tar, Lepton, JPEG XL jbrd — converged rules

1. **Magic + version EARLY, and the version IS the algorithm choice** (pristine-tar
   v2=xdelta/v3=xdelta3; Lepton 1-byte version), with a named refusal on newer
   ("delta is version N, newer than maximum supported M"). One living version at a
   time is compatible with our no-backward-compat — but the byte must exist from day one.
2. **The hash of the ORIGINAL is stored for verify-on-reconstruct** (pristine-tar added
   the sha256sum of the original artifact; Lepton verifies bit-exactness on the
   WRITE side). Our `source_hash` in lineage is the same move.
3. **Structured + opaque split** (jbrd: bit-packed fields with hard
   value ranges — invalid range = reject — plus ONE opaque byte stream; Lepton:
   metadata fields + one zlib blob). For scalar residuals the row codec carries
   the structure; the rule activates for stage-3 composites.
4. Residuals COMPOSE by nesting (pristine-tar's `wrapper` member is a
   complete encapsulated delta with its own type+version) — the stage-3 envelope's shape.

### Lineage practice (OpenLineage, Cui/Widom, ProvSQL, DVC/MLflow)

Converged minimum per row: *(run id, step identity, artifact identity
in/out, code identity, timestamp, outcome)*. Three findings that change the §7 schema:
(1) **failed runs are first-class lineage** (START/FAIL/ABORT +
errorMessage everywhere) — the table gets `outcome` + `error`; (2) the run id is
NEVER a content hash (two runs can produce identical bytes and must be distinguished) —
run_id remains counter/time, the hashes identify artifacts and code; (3) for
per-row transforms without join/aggregate, lineage = why = where collapses —
the residual IS the per-row annotation (the ProvSQL pattern: per-tuple id into a shared
structure), and Cui/Widom's lazy-reverse-queries observation says that for a
verified invertible pair the INVERSE is the reverse query itself — materialized
row-level lineage is redundant for us.
