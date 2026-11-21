import Kernel

/-
The kernel statements. Each proof is `sorry` on purpose: `lake comparator
--config kernel.json` checks that `KernelSolution.lean` proves these
propositions from `Kernel.lean` alone.
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

theorem lossy_in_place_refused : admitInPlace .lossy = false := by
  sorry

theorem roundTrip_first_never_collision (c f : Bool)
    (h : c = true → f = true) :
    diagnose true c f ≠ Report.collision := by
  sorry

theorem collision_first_always_names_it (f : Bool) :
    diagnose false true f = Report.collision := by
  sorry

theorem collision_forces_roundTrip_failure {X Y C : Type} (L : Lens X Y C)
    (x x' : X) (hne : x ≠ x') (hfwd : L.forward x = L.forward x') :
    ¬ (invFun L (toFun L x) = x ∧ invFun L (toFun L x') = x') := by
  sorry

theorem lens_collision_reports_roundTripFailed {X Y C : Type} [DecidableEq X]
    (L : Lens X Y C) (x x' : X) (hne : x ≠ x')
    (hfwd : L.forward x = L.forward x') (c : Bool) :
    diagnose true c
        (!(decide (invFun L (toFun L x) = x) &&
            decide (invFun L (toFun L x') = x'))) =
      Report.roundTripFailed := by
  sorry

theorem getPut_putRes_on_image {X Y C : Type} (L : Lens X Y C)
    (h : getPut L) (x : X) :
    toFun L (invFun L (toFun L x)) = toFun L x := by
  sorry

theorem succLens_getPut : getPut succLens := by
  sorry

theorem succLens_not_putRes : ¬ putRes succLens := by
  sorry

theorem succLens_injective_not_surjective :
    Injective (toFun succLens) ∧ ¬ Surjective (toFun succLens) := by
  sorry
