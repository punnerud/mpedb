import Kernel

theorem wellBehaved_iff_mutual_inverse {X Y C : Type} (L : Lens X Y C) :
    wellBehaved L ↔
      (∀ x, invFun L (toFun L x) = x) ∧
        (∀ y r, toFun L (invFun L (y, r)) = (y, r)) := by
  rfl

theorem mutual_inverse_bijective {X Y C : Type} (L : Lens X Y C)
    (hL : ∀ x, invFun L (toFun L x) = x)
    (hR : ∀ y r, toFun L (invFun L (y, r)) = (y, r)) :
    Bijective (toFun L) := by
  constructor
  · intro x₁ x₂ h
    have := congrArg (invFun L) h
    rw [hL x₁, hL x₂] at this
    exact this
  · intro p
    cases p with
    | mk y r =>
      exact ⟨invFun L (y, r), hR y r⟩

theorem wellBehaved_bijective {X Y C : Type} (L : Lens X Y C)
    (h : wellBehaved L) : Bijective (toFun L) :=
  mutual_inverse_bijective L h.1 h.2

theorem getPut_injective {X Y C : Type} (L : Lens X Y C) (h : getPut L) :
    Injective (toFun L) := by
  intro x₁ x₂ hfeq
  have := congrArg (invFun L) hfeq
  rw [h x₁, h x₂] at this
  exact this

theorem wrongInv_forward_bijective : Bijective (toFun wrongInv) := by
  constructor
  · intro a b h
    simp [toFun, wrongInv] at h
    exact h
  · intro p
    cases p with
    | mk n c =>
      cases c
      exact ⟨n, by simp [toFun, wrongInv]⟩

theorem wrongInv_not_wellBehaved : ¬ wellBehaved wrongInv := by
  intro h
  have := h.1 1
  simp [toFun, invFun, wrongInv] at this

theorem ofInverse_wellBehaved {X Y : Type} (fwd : X → Y) (inv : Y → X)
    (hL : ∀ x, inv (fwd x) = x) (hR : ∀ y, fwd (inv y) = y) :
    wellBehaved (ofInverse fwd inv) := by
  constructor
  · intro x
    simp [toFun, invFun, ofInverse, hL]
  · intro y r
    cases r
    simp [toFun, invFun, ofInverse, hR]

theorem drift_getPut : getPut drift := by
  intro x
  simp [toFun, invFun, drift]

theorem drift_not_putRes : ¬ putRes drift := by
  intro h
  have := h 0 1
  simp [toFun, invFun, drift] at this

theorem collapse_putRes : putRes collapse := by
  intro y r
  cases y
  cases r
  simp [toFun, invFun, collapse]

theorem collapse_not_getPut : ¬ getPut collapse := by
  intro h
  have := h true
  simp [toFun, invFun, collapse] at this

theorem identity_canon_is_strict {X Y C : Type} (L : Lens X Y C) :
    getPutCanon L (fun x => x) ↔ getPut L := by
  rfl

theorem vcounter_getPut : aGetPut vcounter := by
  intro x
  cases x with
  | mk v n =>
    simp [vcounter]

theorem vcounter_putGet : aPutGet vcounter := by
  intro y x
  simp [vcounter]

theorem vcounter_not_putPut : ¬ aPutPut vcounter := by
  intro h
  have := h 1 2 (0, 0)
  simp [vcounter] at this

theorem comp_wellBehaved {X Y Z C D : Type}
    (L : Lens X Y C) (M : Lens Y Z D)
    (hL : wellBehaved L) (hM : wellBehaved M) :
    wellBehaved (comp L M) := by
  constructor
  · intro x
    have hM1 := hM.1 (L.forward x).1
    simp only [invFun, toFun] at hM1
    have hL1 := hL.1 x
    simp only [invFun, toFun] at hL1
    simp only [invFun, toFun, comp]
    rw [hM1]
    exact hL1
  · intro z cd
    have hL2 := hL.2 (M.inverse z cd.2) cd.1
    have hM2 := hM.2 z cd.2
    simp only [toFun, invFun] at hL2 hM2
    simp only [toFun, invFun, comp]
    rw [hL2, hM2]

theorem comp_assoc_view {X Y Z W C D E : Type}
    (L : Lens X Y C) (M : Lens Y Z D) (N : Lens Z W E) (x : X) :
    (toFun (comp (comp L M) N) x).1 = (toFun (comp L (comp M N)) x).1 := by
  rfl

theorem comp_assoc_complement {X Y Z W C D E : Type}
    (L : Lens X Y C) (M : Lens Y Z D) (N : Lens Z W E) (x : X) :
    let left := (toFun (comp (comp L M) N) x).2
    let right := (toFun (comp L (comp M N)) x).2
    left.1.1 = right.1 ∧ left.1.2 = right.2.1 ∧ left.2 = right.2.2 := by
  simp [toFun, comp]

theorem celsius_not_getPut : ¬ getPut celsius := by
  intro h
  have := h 1
  simp [toFun, invFun, celsius] at this

theorem collision_not_both_roundtrip {X Y C : Type} (L : Lens X Y C)
    (x x' : X) (hfwd : L.forward x = L.forward x')
    (hx : invFun L (toFun L x) = x) (hx' : invFun L (toFun L x') = x') :
    x = x' := by
  unfold toFun at hx hx'
  have := congrArg (invFun L) hfwd
  rw [hx, hx'] at this
  exact this

theorem roundTrip_first_hides_collision :
    diagnose true true true = Report.roundTripFailed := rfl

theorem collision_first_names_it :
    diagnose false true true = Report.collision := rfl

theorem lossy_in_place_refused : admitInPlace .lossy = false := rfl
