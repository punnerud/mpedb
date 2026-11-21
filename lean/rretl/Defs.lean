import Kernel

/-
Stage model on top of `Kernel.lean`. Framing, putback, the §13.2 sync
table, the §5.1 float edges, the §6 parameter, and the identity preimage.
The kernel does not import this file.
-/

/- § stage 2. Collision framing. `bits` is unary, ending in `false`, so a
length is not a second copy of the payload. Unframed append is the bug
`("a\x04","")` vs `("a","\x04")`. -/

def bits : Nat → List Bool
  | 0 => [false]
  | n + 1 => true :: bits n

def readLen : List Bool → Option (Nat × List Bool)
  | [] => none
  | false :: rest => some (0, rest)
  | true :: rest =>
    match readLen rest with
    | none => none
    | some (n, rest') => some (n + 1, rest')

def framed (a b : List Bool) : List Bool :=
  bits a.length ++ a ++ bits b.length ++ b

def readFramed (bs : List Bool) : Option (List Bool × List Bool) :=
  match readLen bs with
  | none => none
  | some (n, rest) =>
    let a := rest.take n
    let after := rest.drop n
    match readLen after with
    | none => none
    | some (m, rest2) =>
      let b := rest2.take m
      if rest2.drop m = [] then some (a, b) else none

/- § stage 2 putback. A kept row is inverted with its stored residual and
admitted only when both PutRes halves hold. A deleted row stays deleted.
An inserted residual row is the refused creation path. This is one row.
The design rolls the whole run back on the first refusal; there is no run
here. -/

inductive Row (Y C : Type)
  | kept (y : Y) (r : C)
  | deleted (r : C)
  | inserted (y : Y)

inductive Result (X : Type)
  | carry (x : X)
  | stayDeleted
  | refuseCreation
  | refusePutRes

def putbackRow {X Y C : Type} [DecidableEq Y] [DecidableEq C]
    (L : Lens X Y C) : Row Y C → Result X
  | .deleted _ => .stayDeleted
  | .inserted _ => .refuseCreation
  | .kept y r =>
    let x := invFun L (y, r)
    if toFun L x = (y, r) then .carry x else .refusePutRes

inductive BijRow (Y : Type)
  | kept (y : Y)
  | deleted
  | inserted (y : Y)

def putbackBij {X Y : Type} [DecidableEq Y] (L : Lens X Y Unit) :
    BijRow Y → Result X
  | .deleted => .stayDeleted
  | .kept y =>
    let x := L.inverse y ()
    if (L.forward x).1 = y then .carry x else .refusePutRes
  | .inserted y =>
    let x := L.inverse y ()
    if (L.forward x).1 = y then .carry x else .refusePutRes

/- §13.2. One row, two sides. `unspecified` is every combination the
design does not name; the theorems cover only the named ones. This is the
decision table and nothing more: `changed` is an input bit, not a
comparison against a recorded hash, and there is no push and no state row.
A row that is absent ignores its own `changed` bit. -/

structure Side where
  was : Bool
  now : Bool
  changed : Bool

inductive Sync
  | skip
  | pushAtoB
  | pushBtoA
  | deleteB
  | deleteA
  | clear
  | conflict
  | refuseCreation
  | refuseLossy
  | unspecified

def classify (a b : Side) (bNewAllBijective lossyMovedOnB : Bool) : Sync :=
  if !a.was && a.now && !b.was && !b.now then .pushAtoB
  else if !a.was && !a.now && !b.was && b.now then
    if bNewAllBijective then .pushBtoA else .refuseCreation
  else if a.was && !a.now && b.was && !b.now then .clear
  else if a.was && !a.now && b.was && b.now && !b.changed then .deleteB
  else if a.was && !a.now && b.was && b.now && b.changed then .conflict
  else if b.was && !b.now && a.was && a.now && !a.changed then .deleteA
  else if b.was && !b.now && a.was && a.now && a.changed then .conflict
  else if a.was && a.now && b.was && b.now && !a.changed && !b.changed then .skip
  else if a.was && a.now && b.was && b.now && a.changed && !b.changed then .pushAtoB
  else if a.was && a.now && b.was && b.now && !a.changed && b.changed then
    if lossyMovedOnB then .refuseLossy else .pushBtoA
  else if a.was && a.now && b.was && b.now && a.changed && b.changed then .conflict
  else .unspecified

/- §5.1. rRETL equality on the two float edges, not IEEE `==`.
`±0` stay apart. Every `NaN` payload is one value. The table is written by
hand to say that. It is not derived from 64-bit patterns. -/

inductive Flt
  | negZero
  | posZero
  | nan (payload : Nat)
  | finite (bits : Nat)
  | posInf
  | negInf

def sameFlt : Flt → Flt → Bool
  | .negZero, .negZero => true
  | .posZero, .posZero => true
  | .negZero, .posZero => false
  | .posZero, .negZero => false
  | .nan _, .nan _ => true
  | .finite a, .finite b => decide (a = b)
  | .posInf, .posInf => true
  | .negInf, .negInf => true
  | _, _ => false

/- §6. An ancilla is a parameter both directions already know. It is an
argument, not a component of the complement. `atParam` freezes one value
of it and hands back the lens the earlier laws talk about. -/

def bxor (a b : Bool) : Bool :=
  a != b

structure PLens (P X Y C : Type) where
  forward : P → X → Y × C
  inverse : P → Y → C → X

def atParam {P X Y C : Type} (L : PLens P X Y C) (p : P) : Lens X Y C where
  forward := L.forward p
  inverse := L.inverse p

def pWellBehaved {P X Y C : Type} (L : PLens P X Y C) : Prop :=
  ∀ p, wellBehaved (atParam L p)

def xorParam : PLens Bool Bool Bool Unit where
  forward b x := (bxor b x, ())
  inverse b y _ := bxor b y

/- `crates/mpedb/src/rretl/identity.rs`, the part Lean can see.
A one-column key is `valueBits` and nothing else, so it stays the input
`pk_ref` hashes. A compound key is tag `0xff`, then a little-endian
`u64` count, then each `valueBits` with its own little-endian length.
Blake3 is not in this model: the digest distinguishes two keys only
because these byte strings differ, at the same trust as every other
content hash in the engine. `Scalar` is tags `0`–`9` of `value_bits`:
every storable key type, `Date` (7), `Time` (8) and `Numeric` (9)
included. Payloads are arbitrary byte lists; the fixed eight-byte width
of the integer-like ones is not modelled and not needed. The one arm left
out is the `0xff` debug arm, which today is `Value::List` only, a
parameter value that is never stored and never a key. -/

inductive Scalar
  | null
  | bool (b : Bool)
  | int (le : List UInt8)
  | float (le : List UInt8)
  | text (s : List UInt8)
  | blob (b : List UInt8)
  | timestamp (le : List UInt8)
  | date (le : List UInt8)
  | time (le : List UInt8)
  | numeric (s : List UInt8)

def valueBits : Scalar → List UInt8
  | .null => [0]
  | .bool false => [1, 0]
  | .bool true => [1, 1]
  | .int le => 2 :: le
  | .float le => 3 :: le
  | .text s => 4 :: s
  | .blob b => 5 :: b
  | .timestamp le => 6 :: le
  | .date le => 7 :: le
  | .time le => 8 :: le
  | .numeric s => 9 :: s

def pow256 : Nat → Nat
  | 0 => 1
  | k + 1 => 256 * pow256 k

def writeLe : Nat → Nat → List UInt8
  | 0, _ => []
  | k + 1, n => UInt8.ofNat (n % 256) :: writeLe k (n / 256)

def readLe : List UInt8 → Nat
  | [] => 0
  | b :: bs => b.toNat + 256 * readLe bs

def body (vs : List Scalar) : List UInt8 :=
  vs.flatMap fun v =>
    let b := valueBits v
    writeLe 8 b.length ++ b

def compoundPreimage (vs : List Scalar) : List UInt8 :=
  (255 : UInt8) :: writeLe 8 vs.length ++ body vs

def identityPreimage : List Scalar → List UInt8
  | [v] => valueBits v
  | vs => compoundPreimage vs

def fits (vs : List Scalar) : Prop :=
  vs.length < pow256 8 ∧ ∀ v ∈ vs, (valueBits v).length < pow256 8

def parseScalar : List UInt8 → Option Scalar
  | [0] => some .null
  | [1, 0] => some (.bool false)
  | [1, 1] => some (.bool true)
  | 2 :: le => some (.int le)
  | 3 :: le => some (.float le)
  | 4 :: s => some (.text s)
  | 5 :: b => some (.blob b)
  | 6 :: le => some (.timestamp le)
  | 7 :: le => some (.date le)
  | 8 :: le => some (.time le)
  | 9 :: s => some (.numeric s)
  | _ => none

def decodeN : Nat → List UInt8 → Option (List Scalar)
  | 0, bs => if bs = [] then some [] else none
  | n + 1, bs =>
    if bs.length < 8 then none
    else
      let m := readLe (bs.take 8)
      let rest := bs.drop 8
      if rest.length < m then none
      else
        match parseScalar (rest.take m) with
        | none => none
        | some v =>
          match decodeN n (rest.drop m) with
          | none => none
          | some vs => some (v :: vs)

def decodeCompound (bs : List UInt8) : Option (List Scalar) :=
  match bs with
  | 255 :: rest =>
    if rest.length < 8 then none
    else decodeN (readLe (rest.take 8)) (rest.drop 8)
  | _ => none

def decodeIdentity (bs : List UInt8) : Option (List Scalar) :=
  match bs with
  | 255 :: _ => decodeCompound bs
  | _ =>
    match parseScalar bs with
    | some v => some [v]
    | none => none
