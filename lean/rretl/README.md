# rRETL, checked in Lean

This is a Lean 4 check of the **laws** in `design/DESIGN-RRETL.md`. It is not a proof that `crates/mpedb/src/rretl.rs` implements them.

## The kernel

`Kernel.lean` is the closed core: §§2–4, plus the §3 refusal of an in-place `Lossy` run. `kernel.json` names the 31 theorems about that file. `KernelSolution.lean` imports `Kernel.lean` and nothing else, so a stage definition cannot appear in a kernel proof.

A green kernel run is `lake comparator --config kernel.json`. That is the sentence "the rRETL kernel is green." Widening it means editing `Kernel.lean`.

`Defs.lean` imports the kernel and adds the stage model. `comparator.json` checks that model too: framing, putback, the §13.2 sync rows, the §5.1 float edges, the §6 parameter, and the identity preimage. That is 76 names: the 31 kernel ones again, and 45 about the stage model. A green full run is a larger claim. It does not make those definitions part of the kernel.

Buzzard's four questions (Xena, 12 Nov 2026), answered for this tree:

1. **Does the statement match the prose?** The kernel model is `Kernel.lean`. The stage model is `Defs.lean`, which imports the kernel. The propositions are `KernelChallenge.lean` and `Challenge.lean`, each `sorry`. The map is below. Read those files. If a definition is the wrong reading of the design, every green theorem underneath it is about the wrong thing.
2. **Does it compile?** `lake build`.
3. **Does the proof prove those statements, and only the axioms of Lean?** `lake comparator --config kernel.json` for the kernel, and `lake comparator` for the whole model. `KernelSolution.lean` must not import `KernelChallenge.lean`. `Solution.lean` must not import `Challenge.lean`.
4. **A bug in one kernel?** Add `--paranoid`, when the external checkers shipped with the toolchain are on `PATH`.

`Kernel.lean` is imported by `KernelChallenge.lean`, so it is part of the trusted kernel statement. `Defs.lean` is imported by `Challenge.lean`, so the stage definitions are part of the trusted full statement. A proof that redefines `getPut` does not pass the comparator.

## What a green run does and does not say

A green `lake comparator --config kernel.json` says: the 31 propositions in `KernelChallenge.lean` follow from `Kernel.lean` alone. The judge permits `propext`, `Quot.sound`, and `Classical.choice`. The proofs use less; the measured list is under Check.

A green `lake comparator` says the same for all 76 names in `comparator.json`, from `Kernel.lean` plus `Defs.lean`.

It does not say:

- The Rust functions `forward` / `rex` / `inverse` satisfy `Lens`.
- A probe corpus is a proof. DESIGN-RRETL §5 says the opposite, and this tree does not withdraw that.
- User PySpell is total, deterministic, or free of `RefuseDb`. §10 says arbitrary pairs are checked by running them. Nothing here replaces that.
- `residual_hash`, blake3, the row codec, zip splice, version chains, or SIGKILL recovery. `identity_roundtrip` proves the byte-string preimage is injective under `fits`. It does not prove anything about the blake3 digest `RowKey::reference` stores.
- That the Lean `Scalar` tags are the Rust `value_bits` function. `Scalar` was written to match it by reading `crates/mpedb/src/lens.rs`: tags 0–9, which is every storable key type, `Date` (7), `Time` (8) and `Numeric` (9) included. Nothing checks that correspondence. Payloads are arbitrary byte lists, so the fixed eight-byte width of `Int`, `Float`, `Timestamp`, `Date` and `Time` is not modelled, and neither is the canonical form of a `Numeric` text. The one arm of `value_bits` left out is the `0xff` debug arm. What reaches it today is `Value::List`, a parameter value that is never stored and cannot be a key. `fits` is the `as u64` range: list length and each payload length are `< 256^8`.
- The echo guard as the design states it. §13.2's guard is a transition property: a push records both hashes, so the next sync sees a clean row. `classify` is a decision table over input bits. It has no state row, no push and no recorded hashes. `echo_guard` says the (clean, clean) row answers `skip`. It does not say a sync leaves a row clean.
- Whole-run rollback at putback. The design rolls the run back when one row fails PutRes. `putbackRow` is one row.
- §5's rule that `total` verification is mandatory when a run deletes its source. Verification levels are not modelled. `admitInPlace` is the §3 class gate only.
- IEEE floats. `Flt` and `sameFlt` are a hand-written table of the §5.1 edges. Nothing is derived from 64-bit patterns.
- The Rust collision-key encoding. The stage-2 `framed` uses unary lengths over `List Bool`. The Rust frames with `u64` little-endian lengths over bytes. The injectivity argument is the same one (read the length, take that many, repeat); the encoding is not.
- Commitment 6 (a minimal residual). The design refuses to promise it.

The identity block formalizes `crates/mpedb/src/rretl/identity.rs`. The design text does not describe that code: DESIGN-RRETL §13.3 and §13.4 still say composite source PKs are refused. Those theorems are checked against a reading of the Rust, not against a section of the design.

## Which theorems are evaluations

Some of the listed theorems are closed by `rfl` on a table that was written to give that answer. They pin the definition against a later edit. They are not evidence that the definition is the right one.

- The eight original `classify` rows (`echo_guard`, `both_moved_conflict`, `lossy_b_edit_refused`, `new_b_creation_refused`, `new_a_pushes`, `both_deleted_clears`, `deleted_clean_other_deletes`, `deleted_while_other_moved_conflicts`), each at one point. The thirteen quantified rows are the same kind of fact over every ignored flag: a case split, then `rfl`.
- The two `diagnose` points, `roundTrip_first_hides_collision` and `collision_first_names_it`, and `collision_first_always_names_it`.
- The two `sameFlt` rows, `signed_zero_distinct` and `nan_payloads_agree`.
- `putback_deleted_stays` and `putback_inserted_residual_refused`.
- `lossy_in_place_refused`.
- `atParam_shares_parameter`.
- `scalar_identity_is_valueBits`.
- `wellBehaved_iff_mutual_inverse`, which unfolds `wellBehaved`, `getPut` and `putRes` and stops.

Two are true for a reason that is not the one in the name:

- `ancilla_stays_out_of_residual`. The complement type of `xorParam` is `Unit`, so the equation holds for any function into it. The content of §6 here is `xorParam_wellBehaved` and `wrong_parameter_fails`.
- `compound_not_a_scalar_blob`. It is `l ≠ 5 :: l`, which is true of every list by length. The proof happens to compare head bytes. That a compound key and a scalar key never share a preimage is in `identity_injective`, for `Scalar` under `fits`.

The formalization tracks `design/DESIGN-RRETL.md`. Check the blob before trusting a worktree: `git rev-parse HEAD:design/DESIGN-RRETL.md`. The statements above were read against blob `e84e20e3600704fd2047d3e9c764f18dd8c5996d`, which is still the blob at `fb416a98`.

## Statement map

| Lean | DESIGN-RRETL |
|---|---|
| `Lens`, `getPut`, `putRes`, `wellBehaved` | §2, §4. The two tested laws. |
| `wellBehaved_iff_mutual_inverse`, `mutual_inverse_bijective`, `wellBehaved_bijective` | §2 and §4 together: the laws are "forward and inverse are mutual inverses `X ≃ Y × C`", so forward is bijective onto pairs. |
| `getPut_injective`, `collision_not_both_roundtrip`, `collision_forces_roundTrip_failure` | §4. A collision is a failed round trip for at least one of the two inputs. The third is the same fact in the contrapositive: two different inputs with one forward value do not both round-trip. |
| `roundTrip_first_hides_collision`, `collision_first_names_it` | §4. Two evaluations of `diagnose` at (collided, failed): round trip first answers `roundTripFailed`, collision first answers `collision`. They do not say "never". |
| `roundTrip_first_never_collision`, `lens_collision_reports_roundTripFailed`, `collision_first_always_names_it` | §4, the general claim. With the round trip checked first, `collision` is unreachable for every pair of bits where a collision implies a failed round trip. The second discharges that hypothesis from a `Lens`: for two different inputs with one forward value, the failed bit computed from the two round trips is `true`, and the report is `roundTripFailed` whatever the collision bit is. It needs decidable equality on `X`. The third is the other order: a collision is named whatever the round trip did. |
| `getPut_putRes_on_image` | Stage 2. "On image pairs PutRes is a tautology given GetPut": `forward (inverse (forward x)) = forward x`. Determinism is not a hypothesis because a Lean function is one. |
| `succLens_getPut`, `succLens_not_putRes`, `succLens_injective_not_surjective` | §4. `n ↦ n + 1` with complement `Unit` and inverse `y ↦ y − 1`. Forward is injective and not surjective. GetPut holds. PutRes fails at `0`, which is off the image. |
| `drift_getPut`, `drift_not_putRes` | §4. GetPut misses residual drift. PutRes is the other law. |
| `collapse_putRes`, `collapse_not_getPut` | The two laws are independent in the other direction. |
| `ofInverse_wellBehaved` | §3. A `Bijective` pair is a residual pair with complement `Unit`. |
| `wrongInv_forward_bijective`, `wrongInv_not_wellBehaved` | §4. A bijective forward does not make an arbitrary registered inverse correct. `wrongInv` is the identity forward with the constant-`0` inverse. The inverse has to be checked; it does not follow from forward. |
| `vcounter_getPut`, `vcounter_putGet`, `vcounter_not_putPut` | §4. PutPut is not a law. The version counter is the witness named in the design. |
| `identity_canon_is_strict` | §4. The default canonizer is identity, and under it the canonized law is GetPut. |
| `comp_wellBehaved` | §2. Composition keeps both laws. The complement of a pipeline is the tuple of complements. |
| `comp_assoc_view`, `comp_assoc_complement` | §2. `(L;M);N` and `L;(M;N)` agree on the view and on the flattened complement, and their complement types are nested differently. |
| `celsius_not_getPut` | The integer shadow of the named `celsius ⇄ fahrenheit` refusal. Truncating `Int` division. Not the float64 pair. |
| `readFramed_framed`, `framed_injective`, `unframed_collides`, `framed_separates_unframed_collision` | Stage 2. Length-framed collision keys. Unframed append identifies `[true,false]++[]` with `[true]++[false]`, the shape of `("a\x04","")` versus `("a","\x04")`. |
| `putback_deleted_stays`, `putback_inserted_residual_refused` | Stage 2 putback. Deleted rows stay deleted. A residual row with no residual is the refused creation path. Both are the definition read back. |
| `putback_carry_pointwise`, `putback_carry_iff`, `putback_refuses_off_image`, `putback_carry_of_putRes` | Stage 2 putback, one kept row. If `forward (inverse (y, r)) = (y, r)` for this row, it is carried as `inverse (y, r)`; no global PutRes is assumed. `putback_carry_iff`: the row is carried as some `x` iff that equation holds for it. Off the image it is `refusePutRes`. `putback_carry_of_putRes` is the corollary under global PutRes. The equation on a pair is both halves at once, `forward(x') = y'` and `rex(x') = r`. |
| `putback_bijective_creation` | Stage 2. A bijective insert is the creation path. This direction is proved under global PutRes only. |
| `lossy_in_place_refused` | §3. In-place apply is source deletion. `Lossy` is refused. One evaluation of a three-row table. |
| `echo_guard`, `both_moved_conflict`, `lossy_b_edit_refused`, `new_b_creation_refused`, `new_a_pushes`, `both_deleted_clears`, `deleted_clean_other_deletes`, `deleted_while_other_moved_conflicts` | §13.2. Eight rows of the decision table, each at one point. `classify` returns `unspecified` for every combination the section does not list. |
| `changed_a_pushes`, `changed_b_pushes_back`, `new_b_all_bijective_inserts`, `deleted_b_clean_a_deletes`, `deleted_b_while_a_moved_conflicts` | §13.2, the five rows that had a branch and no theorem: A moved alone pushes A→B; B moved alone with no lossy column moved pushes B→A; new in B with every mapped column bijective inserts into A; deleted in B with A clean deletes A; deleted in B while A moved is a conflict. Each is quantified over every flag `classify` does not read at that row. |
| `echo_guard_all`, `both_moved_conflict_all`, `lossy_b_edit_refused_all`, `new_b_creation_refused_all`, `new_a_pushes_all`, `both_deleted_clears_all`, `deleted_clean_other_deletes_all`, `deleted_while_other_moved_conflicts_all` | §13.2. The eight rows above, quantified the same way. The ignored flags are the two policy bits where the row does not branch on them, and the `changed` bit of a side that is absent. |
| `signed_zero_distinct`, `nan_payloads_agree` | §5.1. `sameFlt` keeps `−0` and `+0` apart and identifies every `NaN` payload. This is not IEEE `==`, and it is two rows of a hand-written table. |
| `atParam_shares_parameter`, `xorParam_wellBehaved`, `ancilla_stays_out_of_residual`, `wrong_parameter_fails` | §6. A stage parameter is an argument of both directions, the same value on the way back, and it is not stored in the complement. `xorParam` is the witness: it is well-behaved at each parameter value, and the other parameter does not invert. `atParam_shares_parameter` is the definition of `atParam`. `ancilla_stays_out_of_residual` holds because the complement is `Unit`. |
| `scalar_identity_is_valueBits`, `unframed_blob_partition_collides`, `framed_partitions_differ`, `text_partitions_differ`, `identity_roundtrip`, `identity_injective`, `compound_not_a_scalar_blob` | `identity.rs`, not the design text. One key is `value_bits`. Two or more keys are length-framed under a leading `0xff`. Unframed blob bytes identify `[5] ++ []` with `[] ++ [5]`. The framed preimages of that pair, and of `["ab","c"]` versus `["a","bc"]`, differ. The preimage round-trips, so it is injective, for every list inside `fits`. `compound_not_a_scalar_blob` is `l ≠ 5 :: l`. |

## Check

From this directory, with [elan](https://github.com/leanprover/elan) and the toolchain in `lean-toolchain`:

```sh
lake update
lake build
lake comparator --config kernel.json --paranoid
lake comparator --paranoid
```

`lake comparator` sandboxes the build with `bwrap` and rejects a solution that uses `sorry` or any axiom outside the three permitted ones. `--paranoid` adds the kernels shipped with Lean: the paranoid kernel, `lean4lean`, `nanoda`, `con-leche`, and `con-ron`. Exit `0` accepts, `1` rejects, `2` means the judge did not run.

On this machine `bwrap` is not on `PATH`. Pointed at a local `bubblewrap` 0.9.0 binary it still fails with `setting up uid map: Permission denied`, so the sandboxed judge does not run. What did run, and accept, on the tree at `fb416a98` plus this directory's edits, was:

```sh
lake comparator --config kernel.json --paranoid --inadvisably-no-sandbox
lake comparator --config comparator.json --paranoid --inadvisably-no-sandbox
```

The kernel judge accepted the 31 names in `kernel.json`. `lean4lean` checked 656 declarations; `con-leche` and `con-ron` accepted 655 (`--verified`). The full judge then accepted the 76 names in `comparator.json`. `lean4lean` checked 1759 declarations; `con-leche` and `con-ron` accepted 1758.

Axioms, from `#print axioms` on each of the 76 names:

| | kernel (31) | all (76) |
|---|---|---|
| no axioms | 15 | 42 |
| `propext` only | 16 | 32 |
| `propext` and `Quot.sound` | 0 | 2 |

The two are `identity_roundtrip` and `identity_injective`. No checked theorem uses `sorry` or `Classical.choice`.

That still checks that `Solution` proves the `Challenge` statements and that those kernels accept the export. It does not isolate the build. Re-run without `--inadvisably-no-sandbox` on a host that allows user namespaces. The toolchain's own warning applies: a run with the sandbox off is not the gold-standard check.
