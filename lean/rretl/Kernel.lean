/-
The rRETL kernel. Closed.

This file is the trusted base of `kernel.json`. It is §§2–4 of DESIGN-RRETL
and the §3 refusal of an in-place Lossy run. A theorem in `kernel.json` is a
theorem about these definitions only.

`Defs.lean` imports this file and adds the stage model: framing, putback, the
§13.2 sync table, the §5.1 float edges, the §6 parameter, and the identity
preimage. Those are checked by `comparator.json`. Adding a definition here
widens the kernel.
-/

/- §2. A pair is an asymmetric lens with complement (Hofmann/Pierce/Wagner):
`forward : X → Y × C` and `inverse : Y → C → X`. -/

structure Lens (X Y C : Type) where
  forward : X → Y × C
  inverse : Y → C → X

def toFun {X Y C : Type} (L : Lens X Y C) : X → Y × C :=
  L.forward

def invFun {X Y C : Type} (L : Lens X Y C) : Y × C → X :=
  fun p => L.inverse p.1 p.2

/- §4. The two laws that are tested, and only these two. -/

def getPut {X Y C : Type} (L : Lens X Y C) : Prop :=
  ∀ x, invFun L (toFun L x) = x

def putRes {X Y C : Type} (L : Lens X Y C) : Prop :=
  ∀ y r, toFun L (invFun L (y, r)) = (y, r)

def wellBehaved {X Y C : Type} (L : Lens X Y C) : Prop :=
  getPut L ∧ putRes L

def Injective {X Y : Type} (f : X → Y) : Prop :=
  ∀ x₁ x₂, f x₁ = f x₂ → x₁ = x₂

def Surjective {X Y : Type} (f : X → Y) : Prop :=
  ∀ y, ∃ x, f x = y

def Bijective {X Y : Type} (f : X → Y) : Prop :=
  Injective f ∧ Surjective f

/- §4. Round-trip equality modulo a declared canonizer. The default is
`fun x => x`. -/

def getPutCanon {X Y C : Type} (L : Lens X Y C) (canonX : X → X) : Prop :=
  ∀ x, canonX (invFun L (toFun L x)) = canonX x

/- §3. Bijective is the residual pair whose complement is `Unit`. -/

def ofInverse {X Y : Type} (fwd : X → Y) (inv : Y → X) : Lens X Y Unit where
  forward x := (fwd x, ())
  inverse y _ := inv y

/- §4 says a registered inverse is checked, not inferred from forward. -/

def wrongInv : Lens Nat Nat Unit where
  forward n := (n, ())
  inverse _ _ := 0

/- §4. GetPut does not see residual drift. PutRes does. -/

def drift : Lens Nat Nat Nat where
  forward x := (x, 0)
  inverse y _ := y

/- §4. An injective forward that is not surjective. GetPut holds, so the
round trip over any corpus is clean. PutRes fails at `0`, which is outside
the image: this is the edit putback has to refuse. -/

def succLens : Lens Nat Nat Unit where
  forward n := (n + 1, ())
  inverse y _ := y - 1

/- PutRes does not imply GetPut. -/

def collapse : Lens Bool Unit Unit where
  forward _ := ((), ())
  inverse _ _ := false

/- §4. PutPut is not a law. The version counter is the witness the design
names: GetPut and PutGet hold, PutPut does not. This is the asymmetric
`put : Y → X → X`, not the complement `inverse`. -/

structure ALens (X Y : Type) where
  get : X → Y
  put : Y → X → X

def aGetPut {X Y : Type} (L : ALens X Y) : Prop :=
  ∀ x, L.put (L.get x) x = x

def aPutGet {X Y : Type} (L : ALens X Y) : Prop :=
  ∀ y x, L.get (L.put y x) = y

def aPutPut {X Y : Type} (L : ALens X Y) : Prop :=
  ∀ y₁ y₂ x, L.put y₂ (L.put y₁ x) = L.put y₂ x

def vcounter : ALens (Nat × Nat) Nat where
  get := Prod.fst
  put y x := (y, if y = x.1 then x.2 else x.2 + 1)

/- §2. Composition threads complements as a tuple, in order. -/

def comp {X Y Z C D : Type} (L : Lens X Y C) (M : Lens Y Z D) :
    Lens X Z (C × D) where
  forward x :=
    ( (M.forward (L.forward x).1).1
    , ( (L.forward x).2
      , (M.forward (L.forward x).1).2 ) )
  inverse z cd :=
    L.inverse (M.inverse z cd.2) cd.1

/- The integer shadow of the named `celsius ⇄ fahrenheit` refusal.
Division is Lean's `Int` division, truncating toward zero. The shipped
pair is float64; this is not that function. -/

def celsius : Lens Int Int Unit where
  forward c := (c * 9 / 5 + 32, ())
  inverse f _ := (f - 32) * 5 / 9

/- §4. Two inputs that forward to one value cannot both round-trip, so a
checker that stops at the first failed round trip never reaches a separate
collision arm. `diagnose` is that order, as a pure function of the two bits.
The bits are free here: `roundTrip_first_never_collision` takes "a collision
implies a failed round trip" as a hypothesis, and
`lens_collision_reports_roundTripFailed` discharges it from a `Lens`. -/

inductive Report
  | ok
  | collision
  | roundTripFailed

def diagnose (roundTripFirst collided roundTripFailed : Bool) : Report :=
  if roundTripFirst then
    if roundTripFailed then .roundTripFailed
    else if collided then .collision
    else .ok
  else if collided then .collision
  else if roundTripFailed then .roundTripFailed
  else .ok

/- §3. In-place apply deletes the source. Lossy is refused. -/

inductive Class
  | bijective
  | residual
  | lossy

def admitInPlace : Class → Bool
  | .lossy => false
  | .bijective => true
  | .residual => true
