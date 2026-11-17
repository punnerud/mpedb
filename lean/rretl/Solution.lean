import Defs
import KernelSolution

private theorem readLen_bits (n : Nat) (rest : List Bool) :
    readLen (bits n ++ rest) = some (n, rest) := by
  induction n generalizing rest with
  | zero => simp [bits, readLen]
  | succ n ih =>
    rw [bits, List.cons_append]
    simp [readLen, ih]

theorem readFramed_framed (a b : List Bool) :
    readFramed (framed a b) = some (a, b) := by
  simp [framed, readFramed, readLen_bits]

theorem framed_injective (a b a' b' : List Bool)
    (h : framed a b = framed a' b') : a = a' ∧ b = b' := by
  have h1 := readFramed_framed a b
  have h2 := readFramed_framed a' b'
  rw [h] at h1
  have hp : (a, b) = (a', b') := by
    injection h1.symm.trans h2
  exact Prod.ext_iff.mp hp

theorem unframed_collides :
    ([true, false] ++ ([] : List Bool)) = [true] ++ [false] := rfl

theorem framed_separates_unframed_collision :
    framed [true, false] [] ≠ framed [true] [false] := by
  intro h
  have hpair := framed_injective [true, false] [] [true] [false] h
  simp at hpair

theorem putback_deleted_stays {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (r : C) :
    putbackRow L (.deleted r) = Result.stayDeleted := rfl

theorem putback_inserted_residual_refused {X Y C : Type}
    [DecidableEq Y] [DecidableEq C] (L : Lens X Y C) (y : Y) :
    putbackRow L (.inserted y) = Result.refuseCreation := rfl

theorem putback_carry_of_putRes {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (h : putRes L) (y : Y) (r : C) :
    putbackRow L (.kept y r) = .carry (invFun L (y, r)) := by
  simp [putbackRow, h y r]

theorem putback_refuses_off_image {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) (y : Y) (r : C)
    (h : toFun L (invFun L (y, r)) ≠ (y, r)) :
    putbackRow L (.kept y r) = .refusePutRes := by
  simp [putbackRow, h]

theorem putback_bijective_creation {X Y : Type} [DecidableEq Y]
    (L : Lens X Y Unit) (h : putRes L) (y : Y) :
    putbackBij L (.inserted y) = .carry (L.inverse y ()) := by
  have hf := h y ()
  unfold toFun invFun at hf
  have hfst : (L.forward (L.inverse y ())).1 = y := by
    rw [hf]
  simp [putbackBij, hfst]

theorem echo_guard :
    classify ⟨true, true, false⟩ ⟨true, true, false⟩ true false = .skip := rfl

theorem both_moved_conflict :
    classify ⟨true, true, true⟩ ⟨true, true, true⟩ true false = .conflict := rfl

theorem lossy_b_edit_refused :
    classify ⟨true, true, false⟩ ⟨true, true, true⟩ true true = .refuseLossy := rfl

theorem new_b_creation_refused :
    classify ⟨false, false, false⟩ ⟨false, true, false⟩ false false =
      .refuseCreation := rfl

theorem new_a_pushes :
    classify ⟨false, true, true⟩ ⟨false, false, false⟩ false false = .pushAtoB := rfl

theorem both_deleted_clears :
    classify ⟨true, false, false⟩ ⟨true, false, false⟩ true false = .clear := rfl

theorem deleted_clean_other_deletes :
    classify ⟨true, false, false⟩ ⟨true, true, false⟩ true false = .deleteB := rfl

theorem deleted_while_other_moved_conflicts :
    classify ⟨true, false, false⟩ ⟨true, true, true⟩ true false = .conflict := rfl

theorem signed_zero_distinct : sameFlt .negZero .posZero = false := rfl

theorem nan_payloads_agree (p q : Nat) :
    sameFlt (.nan p) (.nan q) = true := rfl

theorem atParam_shares_parameter {P X Y C : Type} (L : PLens P X Y C) (p : P) :
    (atParam L p).forward = L.forward p ∧ (atParam L p).inverse = L.inverse p :=
  ⟨rfl, rfl⟩

private theorem bxor_cancel (b x : Bool) : bxor b (bxor b x) = x := by
  cases b <;> cases x <;> rfl

theorem xorParam_wellBehaved : pWellBehaved xorParam := by
  intro b
  constructor
  · intro x
    simp [toFun, invFun, atParam, xorParam, bxor_cancel]
  · intro y r
    cases r
    simp [toFun, invFun, atParam, xorParam, bxor_cancel]

theorem ancilla_stays_out_of_residual (b x : Bool) :
    (xorParam.forward b x).2 = () := rfl

theorem wrong_parameter_fails :
    xorParam.inverse false (xorParam.forward true true).1 () ≠ true := by
  simp [xorParam, bxor]

private theorem writeLe_length (k n : Nat) : (writeLe k n).length = k := by
  induction k generalizing n with
  | zero => rfl
  | succ k ih => simp [writeLe, ih]

private theorem take_append_len {α : Type} (a b : List α) :
    (a ++ b).take a.length = a := by
  induction a with
  | nil => simp
  | cons hd tl ih => simp [ih]

private theorem drop_append_len {α : Type} (a b : List α) :
    (a ++ b).drop a.length = b := by
  induction a with
  | nil => simp
  | cons hd tl ih => simp [ih]

private theorem read_write (k n : Nat) (h : n < pow256 k) :
    readLe (writeLe k n) = n := by
  induction k generalizing n with
  | zero =>
    simp [pow256] at h
    have hn : n = 0 := by omega
    subst hn
    simp [writeLe, readLe]
  | succ k ih =>
    unfold writeLe
    simp [readLe]
    have hdiv : n / 256 < pow256 k := by
      have : n < 256 * pow256 k := by simpa [pow256] using h
      omega
    rw [ih (n / 256) hdiv]
    omega

private theorem parse_valueBits (v : Scalar) : parseScalar (valueBits v) = some v := by
  cases v with
  | bool b => cases b <;> rfl
  | _ => rfl

theorem scalar_identity_is_valueBits (v : Scalar) :
    identityPreimage [v] = valueBits v := rfl

theorem unframed_blob_partition_collides :
    valueBits (.blob [5]) ++ valueBits (.blob []) =
      valueBits (.blob []) ++ valueBits (.blob [5]) := rfl

theorem framed_partitions_differ :
    identityPreimage [.blob [5], .blob []] ≠
      identityPreimage [.blob [], .blob [5]] := by
  intro h
  simp [identityPreimage, compoundPreimage, body, valueBits, writeLe] at h

theorem text_partitions_differ :
    identityPreimage [.text [97, 98], .text [99]] ≠
      identityPreimage [.text [97], .text [98, 99]] := by
  intro h
  simp [identityPreimage, compoundPreimage, body, valueBits, writeLe] at h

private theorem body_right (v : Scalar) (vs : List Scalar) :
    body (v :: vs) = writeLe 8 (valueBits v).length ++ (valueBits v ++ body vs) := by
  simp [body, List.append_assoc]

private theorem fits_tail {v : Scalar} {vs : List Scalar} (h : fits (v :: vs)) : fits vs := by
  obtain ⟨hlen, hmem⟩ := h
  refine ⟨?_, ?_⟩
  · have : vs.length + 1 < pow256 8 := by simpa using hlen
    omega
  · intro u hu
    exact hmem u (List.Mem.tail v hu)

private theorem decodeN_body (vs : List Scalar) (h : fits vs) :
    decodeN vs.length (body vs) = some vs := by
  induction vs with
  | nil => simp [body, decodeN]
  | cons v vs ih =>
    have hv : (valueBits v).length < pow256 8 := h.2 v (by simp)
    have ih' := ih (fits_tail h)
    have bs := body_right v vs
    have hlen : ¬ (body (v :: vs)).length < 8 := by
      rw [bs]
      simp [writeLe_length, List.length_append]
    have htake : (body (v :: vs)).take 8 = writeLe 8 (valueBits v).length := by
      rw [bs]
      simp [writeLe_length]
    have hdrop : (body (v :: vs)).drop 8 = valueBits v ++ body vs := by
      rw [bs]
      simp [writeLe_length]
    have hm : readLe ((body (v :: vs)).take 8) = (valueBits v).length := by
      rw [htake, read_write 8 _ hv]
    have hrest : ¬ ((body (v :: vs)).drop 8).length < (valueBits v).length := by
      rw [hdrop]
      simp [List.length_append]
    have hp : parseScalar (((body (v :: vs)).drop 8).take (valueBits v).length) = some v := by
      rw [hdrop, take_append_len, parse_valueBits]
    have hd : decodeN vs.length (((body (v :: vs)).drop 8).drop (valueBits v).length) = some vs := by
      rw [hdrop, drop_append_len]
      exact ih'
    rw [show (v :: vs).length = Nat.succ vs.length from rfl]
    rw [decodeN.eq_2, ite_eq_right hlen]
    simp (config := { zeta := true }) only [hm]
    rw [ite_eq_right hrest]
    simp (config := { zeta := true }) only [hp, hd]

theorem identity_roundtrip (vs : List Scalar) (h : fits vs) :
    decodeIdentity (identityPreimage vs) = some vs := by
  cases vs with
  | nil =>
    simp [identityPreimage, compoundPreimage, body, decodeIdentity, decodeCompound, decodeN,
      writeLe, readLe]
  | cons v vs =>
    cases vs with
    | nil =>
      cases v with
      | bool b => cases b <;> simp [identityPreimage, decodeIdentity, valueBits, parseScalar]
      | _ => simp [identityPreimage, decodeIdentity, valueBits, parseScalar]
    | cons w ws =>
      have hnot : ¬ (writeLe 8 (v :: w :: ws).length ++ body (v :: w :: ws)).length < 8 := by
        simp [writeLe_length, List.length_append]
      have hpre : identityPreimage (v :: w :: ws) =
          (255 : UInt8) :: (writeLe 8 (v :: w :: ws).length ++ body (v :: w :: ws)) := rfl
      rw [hpre, decodeIdentity.eq_1, decodeCompound.eq_1, ite_eq_right hnot]
      have htake := take_append_len (writeLe 8 (v :: w :: ws).length) (body (v :: w :: ws))
      have hdrop := drop_append_len (writeLe 8 (v :: w :: ws).length) (body (v :: w :: ws))
      simp only [writeLe_length] at htake hdrop
      rw [htake, hdrop, read_write 8 _ h.1, decodeN_body _ h]

theorem identity_injective (vs vs' : List Scalar) (h : fits vs) (h' : fits vs')
    (heq : identityPreimage vs = identityPreimage vs') : vs = vs' := by
  have hv := identity_roundtrip vs h
  rw [heq] at hv
  rw [identity_roundtrip vs' h'] at hv
  injection hv with hp
  exact hp.symm

theorem compound_not_a_scalar_blob (vs : List Scalar) (h : vs.length ≠ 1) :
    identityPreimage vs ≠ valueBits (.blob (identityPreimage vs)) := by
  intro heq
  have hhead : (identityPreimage vs).head? = some 255 := by
    cases vs with
    | nil => simp [identityPreimage, compoundPreimage]
    | cons v vs =>
      cases vs with
      | nil => exact absurd rfl h
      | cons w ws => simp [identityPreimage, compoundPreimage]
  have hblob : (valueBits (.blob (identityPreimage vs))).head? = some 5 := by
    simp [valueBits]
  rw [heq] at hhead
  rw [hblob] at hhead
  injection hhead with h5
  simp at h5
