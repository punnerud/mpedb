import Defs

/-
The statements. Each proof is `sorry` on purpose: `lake comparator` checks
that `Solution.lean` proves these propositions and no others.
-/

theorem wellBehaved_iff_mutual_inverse {X Y C : Type} (L : Lens X Y C) :
    wellBehaved L ↔
      (∀ x, invFun L (toFun L x) = x) ∧
        (∀ y r, toFun L (invFun L (y, r)) = (y, r)) := by
  sorry

theorem mutual_inverse_bijective {X Y C : Type} (L : Lens X Y C)
    (hL : ∀ x, invFun L (toFun L x) = x)
    (hR : ∀ y r, toFun L (invFun L (y, r)) = (y, r)) :
    Bijective (toFun L) := by
  sorry

theorem wellBehaved_bijective {X Y C : Type} (L : Lens X Y C)
    (h : wellBehaved L) : Bijective (toFun L) := by
  sorry

theorem getPut_injective {X Y C : Type} (L : Lens X Y C) (h : getPut L) :
    Injective (toFun L) := by
  sorry

theorem wrongInv_forward_bijective : Bijective (toFun wrongInv) := by
  sorry

theorem wrongInv_not_wellBehaved : ¬ wellBehaved wrongInv := by
  sorry

theorem ofInverse_wellBehaved {X Y : Type} (fwd : X → Y) (inv : Y → X)
    (hL : ∀ x, inv (fwd x) = x) (hR : ∀ y, fwd (inv y) = y) :
    wellBehaved (ofInverse fwd inv) := by
  sorry

theorem drift_getPut : getPut drift := by
  sorry

theorem drift_not_putRes : ¬ putRes drift := by
  sorry

theorem collapse_putRes : putRes collapse := by
  sorry

theorem collapse_not_getPut : ¬ getPut collapse := by
  sorry

theorem identity_canon_is_strict {X Y C : Type} (L : Lens X Y C) :
    getPutCanon L (fun x => x) ↔ getPut L := by
  sorry

theorem vcounter_getPut : aGetPut vcounter := by
  sorry

theorem vcounter_putGet : aPutGet vcounter := by
  sorry

theorem vcounter_not_putPut : ¬ aPutPut vcounter := by
  sorry

theorem comp_wellBehaved {X Y Z C D : Type}
    (L : Lens X Y C) (M : Lens Y Z D)
    (hL : wellBehaved L) (hM : wellBehaved M) :
    wellBehaved (comp L M) := by
  sorry

theorem comp_assoc_view {X Y Z W C D E : Type}
    (L : Lens X Y C) (M : Lens Y Z D) (N : Lens Z W E) (x : X) :
    (toFun (comp (comp L M) N) x).1 = (toFun (comp L (comp M N)) x).1 := by
  sorry

theorem comp_assoc_complement {X Y Z W C D E : Type}
    (L : Lens X Y C) (M : Lens Y Z D) (N : Lens Z W E) (x : X) :
    let left := (toFun (comp (comp L M) N) x).2
    let right := (toFun (comp L (comp M N)) x).2
    left.1.1 = right.1 ∧ left.1.2 = right.2.1 ∧ left.2 = right.2.2 := by
  sorry

theorem celsius_not_getPut : ¬ getPut celsius := by
  sorry

theorem readFramed_framed (a b : List Bool) :
    readFramed (framed a b) = some (a, b) := by
  sorry

theorem framed_injective (a b a' b' : List Bool)
    (h : framed a b = framed a' b') : a = a' ∧ b = b' := by
  sorry

theorem unframed_collides :
    ([true, false] ++ ([] : List Bool)) = [true] ++ [false] := by
  sorry

theorem framed_separates_unframed_collision :
    framed [true, false] [] ≠ framed [true] [false] := by
  sorry

theorem collision_not_both_roundtrip {X Y C : Type} (L : Lens X Y C)
    (x x' : X) (hfwd : L.forward x = L.forward x')
    (hx : invFun L (toFun L x) = x) (hx' : invFun L (toFun L x') = x') :
    x = x' := by
  sorry

theorem roundTrip_first_hides_collision :
    diagnose true true true = Report.roundTripFailed := by
  sorry

theorem collision_first_names_it :
    diagnose false true true = Report.collision := by
  sorry

theorem putback_deleted_stays {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (r : C) :
    putbackRow L (.deleted r) = Result.stayDeleted := by
  sorry

theorem putback_inserted_residual_refused {X Y C : Type}
    [DecidableEq Y] [DecidableEq C] (L : Lens X Y C) (y : Y) :
    putbackRow L (.inserted y) = Result.refuseCreation := by
  sorry

theorem putback_carry_of_putRes {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (h : putRes L) (y : Y) (r : C) :
    putbackRow L (.kept y r) = .carry (invFun L (y, r)) := by
  sorry

theorem putback_refuses_off_image {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (y : Y) (r : C)
    (h : toFun L (invFun L (y, r)) ≠ (y, r)) :
    putbackRow L (.kept y r) = .refusePutRes := by
  sorry

theorem putback_bijective_creation {X Y : Type} [DecidableEq Y]
    (L : Lens X Y Unit) (h : putRes L) (y : Y) :
    putbackBij L (.inserted y) = .carry (L.inverse y ()) := by
  sorry

theorem lossy_in_place_refused : admitInPlace .lossy = false := by
  sorry

theorem echo_guard :
    classify ⟨true, true, false⟩ ⟨true, true, false⟩ true false = .skip := by
  sorry

theorem both_moved_conflict :
    classify ⟨true, true, true⟩ ⟨true, true, true⟩ true false = .conflict := by
  sorry

theorem lossy_b_edit_refused :
    classify ⟨true, true, false⟩ ⟨true, true, true⟩ true true = .refuseLossy := by
  sorry

theorem new_b_creation_refused :
    classify ⟨false, false, false⟩ ⟨false, true, false⟩ false false =
      .refuseCreation := by
  sorry

theorem new_a_pushes :
    classify ⟨false, true, true⟩ ⟨false, false, false⟩ false false = .pushAtoB := by
  sorry

theorem both_deleted_clears :
    classify ⟨true, false, false⟩ ⟨true, false, false⟩ true false = .clear := by
  sorry

theorem deleted_clean_other_deletes :
    classify ⟨true, false, false⟩ ⟨true, true, false⟩ true false = .deleteB := by
  sorry

theorem deleted_while_other_moved_conflicts :
    classify ⟨true, false, false⟩ ⟨true, true, true⟩ true false = .conflict := by
  sorry

theorem signed_zero_distinct : sameFlt .negZero .posZero = false := by
  sorry

theorem nan_payloads_agree (p q : Nat) :
    sameFlt (.nan p) (.nan q) = true := by
  sorry

theorem atParam_shares_parameter {P X Y C : Type} (L : PLens P X Y C) (p : P) :
    (atParam L p).forward = L.forward p ∧ (atParam L p).inverse = L.inverse p := by
  sorry

theorem xorParam_wellBehaved : pWellBehaved xorParam := by
  sorry

theorem ancilla_stays_out_of_residual (b x : Bool) :
    (xorParam.forward b x).2 = () := by
  sorry

theorem wrong_parameter_fails :
    xorParam.inverse false (xorParam.forward true true).1 () ≠ true := by
  sorry

theorem scalar_identity_is_valueBits (v : Scalar) :
    identityPreimage [v] = valueBits v := by
  sorry

theorem unframed_blob_partition_collides :
    valueBits (.blob [5]) ++ valueBits (.blob []) =
      valueBits (.blob []) ++ valueBits (.blob [5]) := by
  sorry

theorem framed_partitions_differ :
    identityPreimage [.blob [5], .blob []] ≠
      identityPreimage [.blob [], .blob [5]] := by
  sorry

theorem text_partitions_differ :
    identityPreimage [.text [97, 98], .text [99]] ≠
      identityPreimage [.text [97], .text [98, 99]] := by
  sorry

theorem identity_roundtrip (vs : List Scalar) (h : fits vs) :
    decodeIdentity (identityPreimage vs) = some vs := by
  sorry

theorem identity_injective (vs vs' : List Scalar) (h : fits vs) (h' : fits vs')
    (heq : identityPreimage vs = identityPreimage vs') : vs = vs' := by
  sorry

theorem compound_not_a_scalar_blob (vs : List Scalar) (h : vs.length ≠ 1) :
    identityPreimage vs ≠ valueBits (.blob (identityPreimage vs)) := by
  sorry
