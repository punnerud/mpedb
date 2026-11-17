# rRETL, checked in Lean

This is a Lean 4 check of the **laws** in `design/DESIGN-RRETL.md`. It is not a proof that `crates/mpedb/src/rretl.rs` implements them.

## The kernel

`Kernel.lean` is the closed core: §§2–4, plus the §3 refusal of an in-place `Lossy` run. `kernel.json` names the 23 theorems about that file. `KernelSolution.lean` imports `Kernel.lean` and nothing else, so a stage definition cannot appear in a kernel proof.

A green kernel run is `lake comparator --config kernel.json`. That is the sentence "the rRETL kernel is green." Widening it means editing `Kernel.lean`.

`Defs.lean` imports the kernel and adds the stage model. `comparator.json` checks that model too: framing, putback, the §13.2 sync rows, the §5.1 float edges, the §6 parameter, and the identity preimage. A green full run is a larger claim. It does not make those definitions part of the kernel.

Buzzard's four questions (Xena, 12 Nov 2026), answered for this tree:

1. **Does the statement match the prose?** The kernel model is `Kernel.lean`. The stage model is `Defs.lean`, which imports the kernel. The propositions are `KernelChallenge.lean` and `Challenge.lean`, each `sorry`. The map is below. Read those files. If a definition is the wrong reading of the design, every green theorem underneath it is about the wrong thing.
2. **Does it compile?** `lake build`.
3. **Does the proof prove those statements, and only the axioms of Lean?** `lake comparator --config kernel.json` for the kernel, and `lake comparator` for the whole model. `KernelSolution.lean` must not import `KernelChallenge.lean`. `Solution.lean` must not import `Challenge.lean`.
4. **A bug in one kernel?** Add `--paranoid`, when the external checkers shipped with the toolchain are on `PATH`.

`Kernel.lean` is imported by `KernelChallenge.lean`, so it is part of the trusted kernel statement. `Defs.lean` is imported by `Challenge.lean`, so the stage definitions are part of the trusted full statement. A proof that redefines `getPut` does not pass the comparator.

## What a green run does and does not say

A green `lake comparator --config kernel.json` says: the 23 propositions in `KernelChallenge.lean` follow from `Kernel.lean` alone, using only `propext`, `Quot.sound`, and `Classical.choice`.

A green `lake comparator` says the same for every name in `comparator.json`, from `Kernel.lean` plus `Defs.lean`.

It does not say:

- The Rust functions `forward` / `rex` / `inverse` satisfy `Lens`.
- A probe corpus is a proof. DESIGN-RRETL §5 says the opposite, and this tree does not withdraw that.
- User PySpell is total, deterministic, or free of `RefuseDb`. §10 says arbitrary pairs are checked by running them. Nothing here replaces that.
- `residual_hash`, blake3, the row codec, zip splice, version chains, or SIGKILL recovery. `identity_roundtrip` proves the byte-string preimage is injective under `fits`. It does not prove anything about the blake3 digest `RowKey::reference` stores.
- That the Lean `Scalar` tags are the Rust `value_bits` function. Rust's debug arm for every other `Value` starts with `0xff`, and that arm is not in `Scalar`. `fits` is the `as u64` range: list length and each payload length are `< 256^8`.
- Commitment 6 (a minimal residual). The design refuses to promise it.

The formalization tracks `design/DESIGN-RRETL.md`. Check the blob before trusting a worktree: `git rev-parse HEAD:design/DESIGN-RRETL.md`. The statements above were read against blob `e84e20e3600704fd2047d3e9c764f18dd8c5996d` at `1b33fbed`.

## Statement map

| Lean | DESIGN-RRETL |
|---|---|
| `Lens`, `getPut`, `putRes`, `wellBehaved` | §2, §4. The two tested laws. |
| `wellBehaved_iff_mutual_inverse`, `mutual_inverse_bijective`, `wellBehaved_bijective` | §2 and §4 together: the laws are "forward and inverse are mutual inverses `X ≃ Y × C`", so forward is bijective onto pairs. |
| `getPut_injective`, `collision_not_both_roundtrip` | §4. A collision is a failed round trip for at least one of the two inputs. |
| `roundTrip_first_hides_collision`, `collision_first_names_it` | §4. Checking the round trip first reports the failure and never the collision. |
| `drift_getPut`, `drift_not_putRes` | §4. GetPut misses residual drift. PutRes is the other law. |
| `collapse_putRes`, `collapse_not_getPut` | The two laws are independent in the other direction. |
| `ofInverse_wellBehaved` | §3. A `Bijective` pair is a residual pair with complement `Unit`. |
| `wrongInv_forward_bijective`, `wrongInv_not_wellBehaved` | §3. A bijective forward with the wrong registered inverse is not well-behaved. The class is checked, not inferred. |
| `vcounter_getPut`, `vcounter_putGet`, `vcounter_not_putPut` | §4. PutPut is not a law. The version counter is the witness named in the design. |
| `identity_canon_is_strict` | §4. The default canonizer is identity, and under it the canonized law is GetPut. |
| `comp_wellBehaved` | §2. Composition keeps both laws. The complement of a pipeline is the tuple of complements. |
| `comp_assoc_view`, `comp_assoc_complement` | §2. `(L;M);N` and `L;(M;N)` agree on the view and on the flattened complement, and their complement types are nested differently. |
| `celsius_not_getPut` | The integer shadow of the named `celsius ⇄ fahrenheit` refusal. Truncating `Int` division. Not the float64 pair. |
| `readFramed_framed`, `framed_injective`, `unframed_collides`, `framed_separates_unframed_collision` | Stage 2. Length-framed collision keys. Unframed append identifies `[true,false]++[]` with `[true]++[false]`, the shape of `("a\x04","")` versus `("a","\x04")`. |
| `putback_deleted_stays`, `putback_inserted_residual_refused`, `putback_carry_of_putRes`, `putback_refuses_off_image`, `putback_bijective_creation` | Stage 2 putback. Deleted rows stay deleted. A residual row with no residual is the refused creation path. A kept row is carried iff both PutRes halves hold. A bijective insert is the creation path, and PutRes still gates it. |
| `lossy_in_place_refused` | §3. In-place apply is source deletion. `Lossy` is refused. |
| `echo_guard`, `both_moved_conflict`, `lossy_b_edit_refused`, `new_b_creation_refused`, `new_a_pushes`, `both_deleted_clears`, `deleted_clean_other_deletes`, `deleted_while_other_moved_conflicts` | §13.2, the named rows of the sync table only. `classify` returns `unspecified` for every combination the section does not list. |
| `signed_zero_distinct`, `nan_payloads_agree` | §5.1. `−0` and `+0` are not the same bits. Every `NaN` payload compares equal. This is not IEEE `==`. |
| `atParam_shares_parameter`, `xorParam_wellBehaved`, `ancilla_stays_out_of_residual`, `wrong_parameter_fails` | §6. A stage parameter is an argument of both directions, the same value on the way back, and it is not stored in the complement. `xorParam` is the witness: the parameter stays `()`, and the other parameter does not invert. |
| `scalar_identity_is_valueBits`, `unframed_blob_partition_collides`, `framed_partitions_differ`, `text_partitions_differ`, `identity_roundtrip`, `identity_injective`, `compound_not_a_scalar_blob` | `identity.rs`. One key is `value_bits`. Two or more keys are length-framed under a leading `0xff`. Unframed blob bytes identify `[5] ++ []` with `[] ++ [5]`. The framed preimages of that pair, and of `["ab","c"]` versus `["a","bc"]`, differ. The preimage round-trips, so it is injective, for every list inside `fits`. A compound preimage is not a scalar blob of itself. |

## Check

From this directory, with [elan](https://github.com/leanprover/elan) and the toolchain in `lean-toolchain`:

```sh
lake update
lake build
lake comparator --config kernel.json --paranoid
lake comparator --paranoid
```

`lake comparator` sandboxes the build with `bwrap` and rejects a solution that uses `sorry` or any axiom outside the three above. `--paranoid` adds the kernels shipped with Lean: the paranoid kernel, `lean4lean`, `nanoda`, `con-leche`, and `con-ron`. Exit `0` accepts, `1` rejects, `2` means the judge did not run.

On this machine `bwrap` is not on `PATH`. Pointed at a local `bubblewrap` 0.9.0 binary it still fails with `setting up uid map: Permission denied`, so the sandboxed judge does not run. What did run, and accept, after the fast-forward to `1b33fbed`, was:

```sh
lake comparator --config kernel.json --paranoid --inadvisably-no-sandbox
lake comparator --config comparator.json --paranoid --inadvisably-no-sandbox
```

The kernel judge accepted the 23 names in `kernel.json`. `lean4lean` checked 605 declarations; `con-leche` and `con-ron` accepted 604 (`--verified`). The full judge then accepted every name in `comparator.json`. `lean4lean` checked 1718 declarations; `con-leche` and `con-ron` accepted 1717. `identity_roundtrip` and `identity_injective` use `propext` and `Quot.sound`. No checked theorem uses `sorry` or `Classical.choice`.

That still checks that `Solution` proves the `Challenge` statements and that those kernels accept the export. It does not isolate the build. Re-run without `--inadvisably-no-sandbox` on a host that allows user namespaces. The toolchain's own warning applies: a run with the sandbox off is not the gold-standard check.
