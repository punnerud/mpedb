//! Lens pairs — reversible PySpell ETL, stage 1 (design/DESIGN-ETL.md).
//!
//! A *lens pair* binds two stored functions declared to be each other's
//! inverse, plus an explicit CLASS saying how exactly that is meant, plus the
//! engine's own verification of the claim. The form is the symmetric-lens
//! complement (DESIGN-ETL §2):
//!
//! ```text
//! forward(x) → (y, residual)        inverse(y, residual) → x
//! ```
//!
//! Stage 1 builds only the corner where `residual = ∅`: **1→1 scalar
//! bijections**. That is not a placeholder scope — a stored function must
//! return a scalar ([`crate::spellfn::call_spell_fn`]), so `{first, last} →
//! full` cannot even be expressed without a residual, and the residual format
//! is an eternity promise that stage 1 deliberately does not write a byte of
//! (DESIGN-ETL §8.2).
//!
//! What makes the verification mean anything: PySpell has no clock and no
//! randomness — [`mpedb_spell::ir::Op`] has no instruction that can answer
//! differently twice — so `inverse(forward(x)) == x` is a property of the pair
//! and not of the day it ran.
//!
//! The pair is pinned by CONTENT HASH, never by name. That is inherited, not
//! invented (DESIGN-TRIGGERS §5.1): a name binding can diverge between attached
//! processes while an immutable `funch/<hash>` blob cannot. Here it carries
//! extra weight — the verification was performed against two *specific* blobs,
//! so a later `mpedb fn define` of the same name must not silently change what
//! the pair means. Re-register to rebind, and it is re-verified when you do.

use mpedb_spell::ir::Proc;
use mpedb_types::{Error, Result, Value};

pub const NS_LENS: &str = "lens";

/// How a pair is meant to be reversible. Declared by the registrant, verified
/// by the engine — never inferred (DESIGN-ETL §3, commitment 2). Cambria died
/// in the undeclared grey zone; JPEG XL refuses input it cannot round-trip
/// rather than emit a pair that does not invert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LensClass {
    /// `inverse(forward(x)) == x` for every x in the declared domain.
    Bijective = 0,
    /// `forward(x) → (y, r)`, `inverse(y, r) → x`. Stage 2.
    Residual = 1,
    /// Not invertible at all. The contract becomes "the source stays" — this is
    /// the honest escape hatch, not a lens pretending to be one.
    Lossy = 2,
}

impl LensClass {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "bijective" => Ok(Self::Bijective),
            "residual" => Ok(Self::Residual),
            "lossy" => Ok(Self::Lossy),
            other => Err(Error::Unsupported(format!(
                "unknown lens class `{other}` — expected bijective, residual or lossy"
            ))),
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Bijective),
            1 => Some(Self::Residual),
            2 => Some(Self::Lossy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bijective => "bijective",
            Self::Residual => "residual",
            Self::Lossy => "lossy",
        }
    }
}

/// The only probe corpus that exists. Stored in the record so a re-verification
/// runs against the same inputs (DESIGN-ETL §5.1).
pub const PROBE_CORPUS_V1: u8 = 1;
/// Byte-for-byte round-trip equality. Other canonizer ids are reserved.
const CANONIZER_IDENTITY: u8 = 0;
/// `residual_type` when the class has no residual (Bijective, Lossy). Legal
/// `ColumnType` tags are 1..=7, so this can never collide with a declaration.
const RESIDUAL_TYPE_NONE: u8 = 0xff;
/// `branch_policy` 0: the residual carries the branch tag (DESIGN-ETL §6).
/// Policy 1 (verified-disjoint outputs) is reserved — the byte exists so the
/// format does not move when it becomes expressible.
const BRANCH_POLICY_TAG: u8 = 0;

const LENS_RECORD_VERSION: u8 = 2;
/// v2: version + class + fwd + inv + rex + residual_type + branch_policy +
///     canonizer + probe + samples + gen
const LENS_RECORD_LEN: usize = 1 + 1 + 32 + 32 + 32 + 1 + 1 + 1 + 1 + 4 + 8;
/// v1 (stage 1): version + class + fwd + inv + canonizer + probe + samples +
///     gen + residual_ref. Decoded INTO v2 shape — see `decode_lens_record`.
const LENS_RECORD_V1_LEN: usize = 1 + 1 + 32 + 32 + 1 + 1 + 4 + 8 + 1;

#[derive(Debug, Clone)]
struct LensRecord {
    class: LensClass,
    forward: [u8; 32],
    inverse: [u8; 32],
    /// All-zero when the class has no residual extractor.
    rex: [u8; 32],
    /// `RESIDUAL_TYPE_NONE`, or a `ColumnType` tag — the DECLARED residual
    /// type (commitment 5), verified against actual rex outputs at
    /// registration. `Any` is a legal declaration; it must be said.
    residual_type: u8,
    branch_policy: u8,
    canonizer: u8,
    probe_corpus: u8,
    samples: u32,
    verified_gen: u64,
}

fn encode_lens_record(r: &LensRecord) -> Vec<u8> {
    let mut v = Vec::with_capacity(LENS_RECORD_LEN);
    v.push(LENS_RECORD_VERSION);
    v.push(r.class as u8);
    v.extend_from_slice(&r.forward);
    v.extend_from_slice(&r.inverse);
    v.extend_from_slice(&r.rex);
    v.push(r.residual_type);
    v.push(r.branch_policy);
    v.push(r.canonizer);
    v.push(r.probe_corpus);
    v.extend_from_slice(&r.samples.to_le_bytes());
    v.extend_from_slice(&r.verified_gen.to_le_bytes());
    v
}

/// Bounds-checked, `None` on anything unexpected. An advisory catalog: a corrupt
/// record degrades to "that pair is not defined", never a panic — the shape of
/// `decode_func_record` and the project's decoder rule.
///
/// **v1 records decode into v2 shape** (zero rex, no residual type) rather than
/// as `None`: degradation-to-undefined was designed for CORRUPTION, not for
/// deliberate retirement, and stage-1 records exist in this project's own dev
/// and M3 databases (DESIGN-ETL §8.1).
fn decode_lens_record(bytes: &[u8]) -> Option<LensRecord> {
    match bytes.first() {
        Some(2) if bytes.len() == LENS_RECORD_LEN => Some(LensRecord {
            class: LensClass::from_byte(bytes[1])?,
            forward: bytes[2..34].try_into().ok()?,
            inverse: bytes[34..66].try_into().ok()?,
            rex: bytes[66..98].try_into().ok()?,
            residual_type: bytes[98],
            branch_policy: bytes[99],
            canonizer: bytes[100],
            probe_corpus: bytes[101],
            samples: u32::from_le_bytes(bytes[102..106].try_into().ok()?),
            verified_gen: u64::from_le_bytes(bytes[106..114].try_into().ok()?),
        }),
        Some(1) if bytes.len() == LENS_RECORD_V1_LEN => Some(LensRecord {
            class: LensClass::from_byte(bytes[1])?,
            forward: bytes[2..34].try_into().ok()?,
            inverse: bytes[34..66].try_into().ok()?,
            rex: [0; 32],
            residual_type: RESIDUAL_TYPE_NONE,
            branch_policy: BRANCH_POLICY_TAG,
            canonizer: bytes[66],
            probe_corpus: bytes[67],
            samples: u32::from_le_bytes(bytes[68..72].try_into().ok()?),
            verified_gen: u64::from_le_bytes(bytes[72..80].try_into().ok()?),
        }),
        _ => None,
    }
}

/// One pair, as `list_lenses` reports it.
#[derive(Debug, Clone)]
pub struct LensInfo {
    pub name: String,
    pub class: LensClass,
    pub forward_hash: String,
    pub inverse_hash: String,
    /// The residual extractor, when the class has one.
    pub rex_hash: Option<String>,
    /// The DECLARED residual type's name, when the class has one.
    pub residual_type: Option<&'static str>,
    /// How many probe inputs actually round-tripped. Reported rather than a bare
    /// "verified": the evidence is statistical, not universal (DESIGN-ETL §5).
    pub samples: u32,
    pub verified_gen: u64,
    /// Every definition blob the pair names is still readable. False means the
    /// pair points at a blob this database can no longer decode — say so, do
    /// not report health.
    pub healthy: bool,
}

// ---------------------------------------------------------------------------
// Canonical value bytes — the comparison the round-trip test actually uses
// ---------------------------------------------------------------------------

/// Canonical bytes for round-trip comparison AND for collision detection —
/// also the encoding the ETL layer's `source_hash`/`output_hash` chains and
/// `pk_enc` keys are built from (§12.2 attack 2: hashing RAW storage bits would
/// turn legal NaN canonicalisation into a false "artifact changed").
///
/// Floats compare on **bits**, not `==`: `-0.0 == 0.0` is true while their
/// representations differ, so a pair can pass a naive equality check and still
/// not be a bijection on the representation.
///
/// NaN is the documented exception — every NaN canonicalises to one pattern.
/// NaN *payload* propagation is not specified by IEEE-754 and genuinely differs
/// between x86 and ARM; making payload part of the contract would make a pair
/// verify on Linux and fail on the M3, which is a worse outcome than the
/// sliver of strictness it buys.
pub(crate) fn value_bits(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => {
            out.push(1);
            out.push(*b as u8);
        }
        Value::Int(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float(f) => {
            out.push(3);
            let bits = if f.is_nan() { f64::NAN.to_bits() } else { f.to_bits() };
            out.extend_from_slice(&bits.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            out.push(5);
            out.extend_from_slice(b);
        }
        Value::Timestamp(t) => {
            out.push(6);
            out.extend_from_slice(&t.to_le_bytes());
        }
        // Unreachable for a stored function's scalar result and never in the
        // probe corpus; kept total rather than panicking.
        other => {
            out.push(0xff);
            out.extend_from_slice(format!("{other:?}").as_bytes());
        }
    }
    out
}

pub(crate) fn same_value(a: &Value, b: &Value) -> bool {
    value_bits(a) == value_bits(b)
}

/// The v1 probe corpus (DESIGN-ETL §5.1) — the edges that break naive
/// "bijections", then a deterministic xorshift tail for mantissa coverage.
/// No `rand` dependency; the project's existing convention.
pub fn probe_corpus(id: u8) -> Result<Vec<Value>> {
    if id != PROBE_CORPUS_V1 {
        return Err(Error::Unsupported(format!("unknown probe corpus id {id}")));
    }
    let mut v = vec![
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(0),
        Value::Int(1),
        Value::Int(-1),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Int(255),
        Value::Int(256),
        Value::Int(65_535),
        Value::Int(65_536),
        Value::Int(i32::MAX as i64),
        Value::Int(i32::MIN as i64),
        // ±0.0 are equal under `==` and differ in bits — the trap value_bits exists for.
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(1.0),
        Value::Float(-1.0),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        Value::Float(f64::MIN_POSITIVE),
        Value::Float(f64::from_bits(1)), // smallest subnormal
        Value::Float(1e308),
        Value::Float(-1e308),
        Value::Float(0.1),
        Value::Float(37.0),
        Value::Float(-40.0), // the one temperature where C and F agree
        Value::Float(std::f64::consts::PI),
        Value::Text(String::new()),
        Value::Text("a".into()),
        Value::Text("æøå".into()),
        Value::Text("e\u{0301}".into()), // e + combining acute
        Value::Text("\u{00e9}".into()),  // precomposed é — NFC of the line above
        Value::Blob(Vec::new()),
        Value::Blob(vec![0xff]),
        Value::Blob(vec![0, 1, 2]),
        Value::Timestamp(0),
        Value::Timestamp(-1),
        Value::Timestamp(i64::MAX),
    ];
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..64 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let n = s as i64;
        v.push(Value::Int(n));
        // Real-valued, not from_bits: random bit patterns are overwhelmingly
        // NaN, which would probe nothing but the NaN rule above.
        v.push(Value::Float(n as f64));
        v.push(Value::Float(n as f64 / 7.0));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// What a verification found. A failure names a concrete counter-example —
/// "not bijective" without one is a rubber stamp.
#[derive(Debug)]
pub enum LensFault {
    /// The inverse came back with something other than the input.
    RoundTrip { input: Value, forward: Value, back: Value },
    /// Two distinct inputs map to the same output — for a `Bijective` pair the
    /// same `y`, for a `Residual` pair the same `(y, r)` JOINTLY. Reported
    /// separately because "forward is not injective" is a different bug from
    /// "forward loses precision".
    Collision { a: Value, b: Value, shared: Value },
    /// The inverse refused a `(y, r)` the forward side produced. Not a skip:
    /// forward defines the domain, so the inverse must cover forward's image —
    /// an x that forwards but cannot come back has no round trip at all.
    InverseRefused { input: Value, forward: Value, residual: Option<Value> },
    /// `rex` produced a value outside the DECLARED residual type
    /// (commitment 5: declared, then verified — never inferred).
    ResidualType { input: Value, residual: Value, declared: &'static str },
    /// Every probe input was refused, so nothing was proven.
    NothingProven,
}

impl std::fmt::Display for LensFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoundTrip { input, forward, back } => write!(
                f,
                "not bijective: forward({input:?}) = {forward:?}, but inverse of that is \
                 {back:?}, not {input:?}"
            ),
            Self::Collision { a, b, shared } => write!(
                f,
                "not bijective: forward({a:?}) and forward({b:?}) are both {shared:?}, \
                 so at most one of them can be recovered"
            ),
            Self::InverseRefused { input, forward, residual } => match residual {
                Some(r) => write!(
                    f,
                    "no round trip: forward({input:?}) = ({forward:?}, residual {r:?}), \
                     but the inverse refuses that pair"
                ),
                None => write!(
                    f,
                    "no round trip: forward({input:?}) = {forward:?}, but the inverse \
                     refuses it"
                ),
            },
            Self::ResidualType { input, residual, declared } => write!(
                f,
                "rex({input:?}) = {residual:?}, outside the declared residual type \
                 `{declared}` — the declaration is verified, not advisory"
            ),
            Self::NothingProven => write!(
                f,
                "the pair refused every input in the probe corpus, so the round trip was \
                 never exercised — nothing was verified"
            ),
        }
    }
}

/// Run the pair over a corpus. `Ok(samples)` counts the inputs that actually
/// round-tripped; inputs the pair refuses are outside its declared domain and
/// are skipped, not failed.
///
/// The law tested is `inverse(forward(x)) == x` (the GetPut analogue).
/// PutRes — `forward(inverse(y)) == y` — is implied here and not run separately:
/// with `residual = ∅` and a deterministic interpreter, `inverse(forward(x)) ==
/// x` for all x already forces it. **PutPut is not tested and must never be
/// added** (DESIGN-ETL §4): every practical system in the literature dropped it,
/// and a version-counter pair that we want to be able to write violates it.
pub fn verify_bijective(
    forward: &Proc,
    inverse: &Proc,
    corpus: &[Value],
) -> std::result::Result<u32, LensFault> {
    let mut samples = 0u32;
    let mut seen: Vec<(Vec<u8>, Value)> = Vec::new();
    for x in corpus {
        let Ok(y) = crate::spellfn::call_spell_fn(forward, std::slice::from_ref(x)) else {
            continue; // outside the declared domain
        };
        // NOT a skip: forward defines the domain, so the inverse must cover
        // forward's image — an x that forwards but cannot come back has no
        // round trip. (Stage 1 skipped here; tightened in stage 2, and no
        // shipped pair depended on the leniency.)
        let Ok(back) = crate::spellfn::call_spell_fn(inverse, std::slice::from_ref(&y)) else {
            return Err(LensFault::InverseRefused {
                input: x.clone(),
                forward: y,
                residual: None,
            });
        };
        // Collision BEFORE round trip, and the order is load-bearing. With a
        // deterministic inverse a collision always *also* shows up as a failed
        // round trip on the second of the two inputs — so checking the round
        // trip first would make this arm unreachable and throw away the better
        // diagnosis. "These two inputs forward to the same value" says why;
        // "the round trip came back wrong" only says that.
        let key = value_bits(&y);
        if let Some((_, prev)) = seen.iter().find(|(k, _)| *k == key) {
            return Err(LensFault::Collision {
                a: prev.clone(),
                b: x.clone(),
                shared: y,
            });
        }
        if !same_value(x, &back) {
            return Err(LensFault::RoundTrip {
                input: x.clone(),
                forward: y,
                back,
            });
        }
        seen.push((key, x.clone()));
        samples += 1;
    }
    if samples == 0 {
        return Err(LensFault::NothingProven);
    }
    Ok(samples)
}

/// The JOINT collision key for a `Residual` pair: `blake3(len‖bits(y)‖len‖bits(r))`.
///
/// The length framing is load-bearing. `value_bits` has no internal framing, so
/// naive concatenation makes distinct pairs collide —
/// `(Text("a\x04"), Text(""))` and `(Text("a"), Text("\x04"))` concatenate to
/// the same bytes — and a false collision REFUSES a valid pair.
fn joint_key(y: &Value, r: &Value) -> [u8; 32] {
    let (yb, rb) = (value_bits(y), value_bits(r));
    let mut buf = Vec::with_capacity(8 + yb.len() + 8 + rb.len());
    buf.extend_from_slice(&(yb.len() as u64).to_le_bytes());
    buf.extend_from_slice(&yb);
    buf.extend_from_slice(&(rb.len() as u64).to_le_bytes());
    buf.extend_from_slice(&rb);
    *blake3::hash(&buf).as_bytes()
}

/// Verify a `Residual` triple over a corpus. The law is GetPut:
/// `inverse(forward(x), rex(x)) == x`. PutRes is deliberately NOT run — over
/// the corpus it can only be tested on image pairs `(forward(x), rex(x))`,
/// where GetPut plus determinism already force it, and running a tautology and
/// reporting it as an independently verified law is the bare "verified"
/// DESIGN-ETL §5 forbids.
///
/// The collision check keys on `(y, r)` JOINTLY: multiple preimages of `y`
/// alone are the entire point of the class — the residual disambiguates them.
/// Injectivity of `x ↦ (y, r)` is exactly what GetPut needs to be satisfiable.
///
/// Every non-NULL `rex` output is checked against the DECLARED residual type
/// (commitment 5: declared, then verified). NULL is always a legal residual
/// VALUE — it is the missing residual ROW that is an error, and that
/// distinction belongs to the apply/revert layer.
pub fn verify_residual(
    forward: &Proc,
    rex: &Proc,
    inverse: &Proc,
    corpus: &[Value],
    declared: mpedb_types::ColumnType,
) -> std::result::Result<u32, LensFault> {
    let mut samples = 0u32;
    let mut seen: Vec<([u8; 32], Value)> = Vec::new();
    for x in corpus {
        let Ok(y) = crate::spellfn::call_spell_fn(forward, std::slice::from_ref(x)) else {
            continue; // outside the declared domain
        };
        let Ok(r) = crate::spellfn::call_spell_fn(rex, std::slice::from_ref(x)) else {
            continue; // rex narrows the domain the same way forward does
        };
        if let Some(rt) = r.column_type() {
            if declared != mpedb_types::ColumnType::Any && rt != declared {
                return Err(LensFault::ResidualType {
                    input: x.clone(),
                    residual: r,
                    declared: declared.name(),
                });
            }
        }
        // Collision before round trip — same load-bearing order as the
        // bijective verifier, on the joint key.
        let key = joint_key(&y, &r);
        if let Some((_, prev)) = seen.iter().find(|(k, _)| *k == key) {
            return Err(LensFault::Collision {
                a: prev.clone(),
                b: x.clone(),
                shared: y,
            });
        }
        let args = [y.clone(), r.clone()];
        let Ok(back) = crate::spellfn::call_spell_fn(inverse, &args) else {
            return Err(LensFault::InverseRefused {
                input: x.clone(),
                forward: y,
                residual: Some(r),
            });
        };
        if !same_value(x, &back) {
            return Err(LensFault::RoundTrip {
                input: x.clone(),
                forward: y,
                back,
            });
        }
        seen.push((key, x.clone()));
        samples += 1;
    }
    if samples == 0 {
        return Err(LensFault::NothingProven);
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// A function name resolved to the blob the pair will actually be pinned to.
struct Resolved {
    hash: [u8; 32],
    argc: u16,
}

/// What `etl apply`/`etl revert` need from a registered pair (crate::etl).
pub(crate) struct EtlLens {
    pub class: LensClass,
    pub forward: std::sync::Arc<Proc>,
    pub rex: Option<std::sync::Arc<Proc>>,
    pub inverse: std::sync::Arc<Proc>,
    pub forward_hash: String,
    pub rex_hash: Option<String>,
    pub inverse_hash: String,
}

impl crate::Database {
    /// Register a lens pair over two functions that already exist
    /// (`mpedb fn define`). There is deliberately no new compilation path here.
    ///
    /// A `Bijective` declaration is VERIFIED before the record is written, and a
    /// pair that fails is refused with a named counter-example. That refusal is
    /// the whole point of the feature: a verifier that accepts
    /// `celsius ⇄ fahrenheit` — which is not bijective in float64, however much
    /// intuition says otherwise — is a rubber stamp.
    pub fn create_lens(
        &self,
        name: &str,
        forward: &str,
        inverse: &str,
        class: LensClass,
    ) -> Result<u32> {
        if class == LensClass::Residual {
            return Err(Error::Unsupported(
                "a residual pair binds THREE functions and a declared residual type — \
                 use create_residual_lens (CLI: --rex <fn> --residual-type <type>)"
                    .into(),
            ));
        }
        self.register_lens(name, class, forward, None, inverse, None)
    }

    /// Register a `Residual` pair: the triple `forward/1`, `rex/1` (the
    /// residual extractor — Sparcl's complement as a plain stored function),
    /// `inverse/2`, plus the DECLARED residual type (commitment 5: declared,
    /// then verified against actual rex outputs — never inferred).
    ///
    /// The law verified is GetPut: `inverse(forward(x), rex(x)) == x`. The
    /// collision check keys on `(y, r)` JOINTLY — multiple preimages of `y`
    /// alone are the point of the class; the residual disambiguates them.
    pub fn create_residual_lens(
        &self,
        name: &str,
        forward: &str,
        rex: &str,
        inverse: &str,
        residual_type: mpedb_types::ColumnType,
    ) -> Result<u32> {
        self.register_lens(name, LensClass::Residual, forward, Some(rex), inverse, Some(residual_type))
    }

    fn register_lens(
        &self,
        name: &str,
        class: LensClass,
        forward: &str,
        rex: Option<&str>,
        inverse: &str,
        residual_type: Option<mpedb_types::ColumnType>,
    ) -> Result<u32> {
        let key = crate::sys_record_subkey(NS_LENS, name.as_bytes())?;

        let r = self.engine.begin_read()?;
        let resolved = (|| -> Result<(Resolved, Option<Resolved>, Resolved)> {
            let fns = self.scan_func_records(&r)?;
            let find = |want: &str| -> Result<Resolved> {
                fns.iter()
                    .find(|(n, _, _)| n == want)
                    .map(|(_, hash, argc)| Resolved { hash: *hash, argc: *argc })
                    .ok_or_else(|| {
                        Error::Unsupported(format!(
                            "no stored function named `{want}` — define it with \
                             `mpedb fn define` first"
                        ))
                    })
            };
            Ok((find(forward)?, rex.map(&find).transpose()?, find(inverse)?))
        })();
        let (fwd_fn, rex_fn, inv_fn) = match resolved {
            Ok(v) => v,
            Err(e) => {
                r.finish()?;
                return Err(e);
            }
        };

        let verify = (|| -> Result<u32> {
            let want_inv_argc: u16 = if class == LensClass::Residual { 2 } else { 1 };
            if fwd_fn.argc != 1 || inv_fn.argc != want_inv_argc {
                return Err(Error::Unsupported(format!(
                    "a {} pair is forward/1 and inverse/{want_inv_argc}, got \
                     {forward}/{} and {inverse}/{}",
                    class.as_str(),
                    fwd_fn.argc,
                    inv_fn.argc,
                )));
            }
            if let Some(rx) = &rex_fn {
                if rx.argc != 1 {
                    return Err(Error::Unsupported(format!(
                        "the residual extractor is rex/1, got {}/{}",
                        rex.unwrap_or("?"),
                        rx.argc
                    )));
                }
            }
            match class {
                // Nothing to verify: the pair declares it does not invert, and
                // the contract it buys is source retention.
                LensClass::Lossy => Ok(0),
                LensClass::Bijective => {
                    let fwd = self.load_proc_by_hash(&r, &fwd_fn.hash)?.ok_or_else(|| {
                        Error::Corrupt(format!(
                            "`{forward}` names a definition blob that is missing"
                        ))
                    })?;
                    let inv = self.load_proc_by_hash(&r, &inv_fn.hash)?.ok_or_else(|| {
                        Error::Corrupt(format!(
                            "`{inverse}` names a definition blob that is missing"
                        ))
                    })?;
                    let corpus = probe_corpus(PROBE_CORPUS_V1)?;
                    verify_bijective(&fwd, &inv, &corpus).map_err(|fault| {
                        Error::Unsupported(format!("`{name}` was declared bijective but {fault}"))
                    })
                }
                LensClass::Residual => {
                    let rx_fn = rex_fn.as_ref().expect("residual class resolves rex");
                    let declared = residual_type.expect("residual class declares a type");
                    let fwd = self.load_proc_by_hash(&r, &fwd_fn.hash)?.ok_or_else(|| {
                        Error::Corrupt(format!(
                            "`{forward}` names a definition blob that is missing"
                        ))
                    })?;
                    let rx = self.load_proc_by_hash(&r, &rx_fn.hash)?.ok_or_else(|| {
                        Error::Corrupt("the rex function names a missing definition blob".into())
                    })?;
                    let inv = self.load_proc_by_hash(&r, &inv_fn.hash)?.ok_or_else(|| {
                        Error::Corrupt(format!(
                            "`{inverse}` names a definition blob that is missing"
                        ))
                    })?;
                    let corpus = probe_corpus(PROBE_CORPUS_V1)?;
                    verify_residual(&fwd, &rx, &inv, &corpus, declared).map_err(|fault| {
                        Error::Unsupported(format!("`{name}` was declared residual but {fault}"))
                    })
                }
            }
        })();
        r.finish()?;
        let samples = verify?;

        let record = encode_lens_record(&LensRecord {
            class,
            forward: fwd_fn.hash,
            inverse: inv_fn.hash,
            rex: rex_fn.map(|f| f.hash).unwrap_or([0; 32]),
            residual_type: residual_type.map(|t| t as u8).unwrap_or(RESIDUAL_TYPE_NONE),
            branch_policy: BRANCH_POLICY_TAG,
            canonizer: CANONIZER_IDENTITY,
            probe_corpus: PROBE_CORPUS_V1,
            samples,
            verified_gen: self.engine.schema().schema_gen,
        });

        // The view-DDL shape, as `create_function` uses it. The gen bump is not
        // needed while pairs are not a compilation input — it is taken because
        // it becomes needed the moment they are, and the alternative is a silent
        // staleness bug on that day. Rare admin operation.
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&key, &record)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(samples)
    }

    /// Re-run a registered pair's verification against the corpus its record
    /// names. Reads only — it reports, it does not rewrite the record.
    pub fn verify_lens(&self, name: &str) -> Result<u32> {
        let key = crate::sys_record_subkey(NS_LENS, name.as_bytes())?;
        let r = self.engine.begin_read()?;
        let out = (|| -> Result<u32> {
            let rec = r
                .sys_get(&key)?
                .as_deref()
                .and_then(decode_lens_record)
                .ok_or_else(|| Error::Unsupported(format!("no lens pair named `{name}`")))?;
            let fwd = self
                .load_proc_by_hash(&r, &rec.forward)?
                .ok_or_else(|| Error::Corrupt("forward definition blob is missing".into()))?;
            let inv = self
                .load_proc_by_hash(&r, &rec.inverse)?
                .ok_or_else(|| Error::Corrupt("inverse definition blob is missing".into()))?;
            let corpus = probe_corpus(rec.probe_corpus)?;
            match rec.class {
                LensClass::Bijective => verify_bijective(&fwd, &inv, &corpus)
                    .map_err(|fault| Error::Unsupported(format!("`{name}` {fault}"))),
                LensClass::Residual => {
                    let rx = self
                        .load_proc_by_hash(&r, &rec.rex)?
                        .ok_or_else(|| Error::Corrupt("rex definition blob is missing".into()))?;
                    let declared = mpedb_types::ColumnType::from_tag(rec.residual_type)
                        .ok_or_else(|| {
                            Error::Corrupt("lens record declares no valid residual type".into())
                        })?;
                    verify_residual(&fwd, &rx, &inv, &corpus, declared)
                        .map_err(|fault| Error::Unsupported(format!("`{name}` {fault}")))
                }
                LensClass::Lossy => Err(Error::Unsupported(format!(
                    "`{name}` is lossy: it declares it does not invert, so there is \
                     nothing to verify — the contract it buys is source retention"
                ))),
            }
        })();
        r.finish()?;
        out
    }

    /// Every registered pair. `healthy` is computed, not stored: a pair whose
    /// definition blob has become unreadable must be reported as such, not as a
    /// pair in good standing.
    pub fn list_lenses(&self) -> Result<Vec<LensInfo>> {
        let mut lo = NS_LENS.as_bytes().to_vec();
        lo.push(0);
        let mut hi = NS_LENS.as_bytes().to_vec();
        hi.push(1);
        let r = self.engine.begin_read()?;
        let out = (|| -> Result<Vec<LensInfo>> {
            let mut out = Vec::new();
            for (k, v) in r.sys_scan_range(&lo, &hi)? {
                let Some(rec) = decode_lens_record(&v) else {
                    continue; // advisory catalog: corrupt reads as "not defined"
                };
                let has_rex = rec.class == LensClass::Residual;
                let healthy = self.load_proc_by_hash(&r, &rec.forward)?.is_some()
                    && self.load_proc_by_hash(&r, &rec.inverse)?.is_some()
                    && (!has_rex || self.load_proc_by_hash(&r, &rec.rex)?.is_some());
                out.push(LensInfo {
                    name: String::from_utf8_lossy(&k[lo.len()..]).into_owned(),
                    class: rec.class,
                    forward_hash: mpedb_spell::ProcHash(rec.forward).to_string(),
                    inverse_hash: mpedb_spell::ProcHash(rec.inverse).to_string(),
                    rex_hash: has_rex.then(|| mpedb_spell::ProcHash(rec.rex).to_string()),
                    residual_type: mpedb_types::ColumnType::from_tag(rec.residual_type)
                        .map(|t| t.name()),
                    samples: rec.samples,
                    verified_gen: rec.verified_gen,
                    healthy,
                });
            }
            Ok(out)
        })();
        r.finish()?;
        out
    }

    /// The ETL layer's view of a registered pair: class, declared residual
    /// type, and the three decoded procedures — pinned by the hashes the
    /// VERIFICATION ran against, exactly as stored.
    pub(crate) fn load_lens_for_etl(&self, name: &str) -> Result<EtlLens> {
        let key = crate::sys_record_subkey(NS_LENS, name.as_bytes())?;
        let r = self.engine.begin_read()?;
        let out = (|| -> Result<EtlLens> {
            let rec = r
                .sys_get(&key)?
                .as_deref()
                .and_then(decode_lens_record)
                .ok_or_else(|| Error::Unsupported(format!("no lens pair named `{name}`")))?;
            let forward = self
                .load_proc_by_hash(&r, &rec.forward)?
                .ok_or_else(|| Error::Corrupt("forward definition blob is missing".into()))?;
            let inverse = self
                .load_proc_by_hash(&r, &rec.inverse)?
                .ok_or_else(|| Error::Corrupt("inverse definition blob is missing".into()))?;
            let rex = if rec.class == LensClass::Residual {
                Some(
                    self.load_proc_by_hash(&r, &rec.rex)?
                        .ok_or_else(|| Error::Corrupt("rex definition blob is missing".into()))?,
                )
            } else {
                None
            };
            Ok(EtlLens {
                class: rec.class,
                forward,
                rex,
                inverse,
                forward_hash: mpedb_spell::ProcHash(rec.forward).to_string(),
                rex_hash: (rec.class == LensClass::Residual)
                    .then(|| mpedb_spell::ProcHash(rec.rex).to_string()),
                inverse_hash: mpedb_spell::ProcHash(rec.inverse).to_string(),
            })
        })();
        r.finish()?;
        out
    }

    /// Remove a pair's registration. The definition blobs stay — they are
    /// content-addressed and may still be named by a function or another pair.
    pub fn drop_lens(&self, name: &str) -> Result<bool> {
        let key = crate::sys_record_subkey(NS_LENS, name.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let existed = match w.sys_delete(&key) {
            Ok(existed) => existed,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        };
        if !existed {
            w.abort();
            return Ok(false);
        }
        w.bump_schema_gen();
        w.commit()?;
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> LensRecord {
        LensRecord {
            class: LensClass::Residual,
            forward: [7; 32],
            inverse: [9; 32],
            rex: [11; 32],
            residual_type: mpedb_types::ColumnType::Int64 as u8,
            branch_policy: BRANCH_POLICY_TAG,
            canonizer: CANONIZER_IDENTITY,
            probe_corpus: PROBE_CORPUS_V1,
            samples: 123,
            verified_gen: 456,
        }
    }

    #[test]
    fn record_round_trips() {
        let bytes = encode_lens_record(&rec());
        assert_eq!(bytes.len(), LENS_RECORD_LEN);
        let back = decode_lens_record(&bytes).expect("decodes");
        assert_eq!(back.class, LensClass::Residual);
        assert_eq!(back.forward, [7; 32]);
        assert_eq!(back.inverse, [9; 32]);
        assert_eq!(back.rex, [11; 32]);
        assert_eq!(back.residual_type, mpedb_types::ColumnType::Int64 as u8);
        assert_eq!(back.samples, 123);
        assert_eq!(back.verified_gen, 456);
    }

    #[test]
    fn a_v1_record_decodes_as_a_bijective_pair_not_as_undefined() {
        // The stage-1 layout, byte for byte. Degradation-to-undefined was
        // designed for CORRUPTION; deliberate retirement decodes (§8.1).
        let mut v1 = Vec::with_capacity(LENS_RECORD_V1_LEN);
        v1.push(1u8); // version
        v1.push(LensClass::Bijective as u8);
        v1.extend_from_slice(&[7; 32]); // forward
        v1.extend_from_slice(&[9; 32]); // inverse
        v1.push(CANONIZER_IDENTITY);
        v1.push(PROBE_CORPUS_V1);
        v1.extend_from_slice(&123u32.to_le_bytes());
        v1.extend_from_slice(&456u64.to_le_bytes());
        v1.push(0); // the reserved residual_ref byte
        assert_eq!(v1.len(), LENS_RECORD_V1_LEN);

        let back = decode_lens_record(&v1).expect("v1 decodes into v2 shape");
        assert_eq!(back.class, LensClass::Bijective);
        assert_eq!(back.forward, [7; 32]);
        assert_eq!(back.inverse, [9; 32]);
        assert_eq!(back.rex, [0; 32]);
        assert_eq!(back.residual_type, RESIDUAL_TYPE_NONE);
        assert_eq!(back.samples, 123);
        assert_eq!(back.verified_gen, 456);

        // And v1 truncation still degrades safely.
        for n in 0..v1.len() {
            assert!(decode_lens_record(&v1[..n]).is_none(), "v1 prefix of {n} decoded");
        }
    }

    #[test]
    fn truncation_at_every_offset_is_none_not_panic() {
        let bytes = encode_lens_record(&rec());
        for n in 0..bytes.len() {
            assert!(decode_lens_record(&bytes[..n]).is_none(), "prefix of {n} decoded");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_lens_record(&longer).is_none());
    }

    #[test]
    fn unknown_version_and_class_are_rejected() {
        let mut bytes = encode_lens_record(&rec());
        bytes[0] = 3; // future version: named refusal is the CLI's job; here, undefined
        assert!(decode_lens_record(&bytes).is_none());
        let mut bytes = encode_lens_record(&rec());
        bytes[1] = 9;
        assert!(decode_lens_record(&bytes).is_none());
        // A v1 version byte on a v2-length body is a corrupt record, not a v1 record.
        let mut bytes = encode_lens_record(&rec());
        bytes[0] = 1;
        assert!(decode_lens_record(&bytes).is_none());
    }

    #[test]
    fn signed_zero_is_not_the_same_value() {
        let (a, b) = (Value::Float(0.0), Value::Float(-0.0));
        // The trap: `==` says these are the same value and the bits say they
        // are not. A round-trip check built on `==` would miss the difference.
        assert!(matches!((&a, &b), (Value::Float(p), Value::Float(q))
            if p == q && p.to_bits() != q.to_bits()));
        assert!(!same_value(&a, &b));
        assert!(same_value(&a, &Value::Float(0.0)));
    }

    #[test]
    fn every_nan_is_the_same_nan() {
        // Payload propagation differs between x86 and ARM; a payload-sensitive
        // contract would verify on Linux and fail on the M3.
        let a = Value::Float(f64::NAN);
        let b = Value::Float(f64::from_bits(f64::NAN.to_bits() | 0x3));
        assert!(matches!((&a, &b), (Value::Float(p), Value::Float(q))
            if p.to_bits() != q.to_bits()));
        assert!(same_value(&a, &b));
    }

    #[test]
    fn distinct_types_do_not_collide_in_the_key() {
        // Int(1) and Timestamp(1) share their payload bytes and must not share a key.
        assert_ne!(value_bits(&Value::Int(1)), value_bits(&Value::Timestamp(1)));
        assert_ne!(value_bits(&Value::Text("a".into())), value_bits(&Value::Blob(vec![b'a'])));
    }

    #[test]
    fn probe_corpus_is_deterministic_and_covers_the_edges() {
        let a = probe_corpus(PROBE_CORPUS_V1).expect("corpus");
        let b = probe_corpus(PROBE_CORPUS_V1).expect("corpus");
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert!(same_value(x, y), "corpus is not deterministic");
        }
        assert!(a.iter().any(|v| matches!(v, Value::Int(i) if *i == i64::MIN)));
        assert!(a.iter().any(|v| matches!(v, Value::Float(f) if f.is_sign_negative() && *f == 0.0)));
        assert!(a.iter().any(|v| matches!(v, Value::Float(f) if f.is_nan())));
        assert!(probe_corpus(9).is_err());
    }

    fn compile(src: &str) -> Proc {
        let sk = mpedb_spell::py::compile(src).expect("compiles");
        Proc::new(sk.name.clone(), sk.argc, sk.nlocals, Vec::new(), sk.consts, sk.instrs)
            .expect("proc")
    }

    /// The design claims celsius ⇄ fahrenheit fails on float64 precision. Over
    /// the full probe corpus the pair is refused earlier, on the TYPE change
    /// (`Int(0) → Float(32.0) → Float(0.0)`), which is a second and independent
    /// reason it is not a bijection in mpedb's value domain. This test holds the
    /// type change out of the picture — floats only — so precision is the sole
    /// remaining explanation, and the claim is backed rather than asserted.
    #[test]
    fn float64_precision_alone_breaks_the_celsius_round_trip() {
        let fwd = compile("def c_to_f(c):\n    return c * 9 / 5 + 32\n");
        let inv = compile("def f_to_c(x):\n    return (x - 32) * 5 / 9\n");
        let corpus: Vec<Value> =
            [0.1, 37.0, 1.0 / 3.0, 1e-8, 12.345].iter().map(|x| Value::Float(*x)).collect();
        let fault = verify_bijective(&fwd, &inv, &corpus).expect_err("not a float64 bijection");
        match fault {
            LensFault::RoundTrip { input, back, .. } => {
                assert!(matches!(input, Value::Float(_)), "{input:?}");
                assert!(matches!(back, Value::Float(_)), "both sides stayed floats: {back:?}");
                assert!(!same_value(&input, &back));
            }
            other => panic!("expected a precision round-trip failure, got {other}"),
        }
        // And -40 is the fixed point where the two scales agree, so it survives
        // — the pair is not uniformly broken, which is exactly why it is
        // convincing enough to need refusing.
        assert_eq!(verify_bijective(&fwd, &inv, &[Value::Float(-40.0)]).ok(), Some(1));
    }

    #[test]
    fn class_parse_covers_all_three() {
        assert_eq!(LensClass::parse("bijective").unwrap(), LensClass::Bijective);
        assert_eq!(LensClass::parse("residual").unwrap(), LensClass::Residual);
        assert_eq!(LensClass::parse("lossy").unwrap(), LensClass::Lossy);
        assert!(LensClass::parse("nonsense").is_err());
    }

    #[test]
    fn the_joint_key_is_length_framed() {
        // Unframed concatenation makes these two DISTINCT pairs collide —
        // value_bits(Text("a\x04")) ‖ value_bits(Text("")) equals
        // value_bits(Text("a")) ‖ value_bits(Text("\x04")) byte for byte —
        // and a false collision REFUSES a valid pair.
        let a = joint_key(&Value::Text("a\u{4}".into()), &Value::Text(String::new()));
        let b = joint_key(&Value::Text("a".into()), &Value::Text("\u{4}".into()));
        assert_ne!(a, b, "length framing is load-bearing");
        // Sanity: the trap premise holds — the unframed bytes really are equal.
        let unframed_a = [
            value_bits(&Value::Text("a\u{4}".into())),
            value_bits(&Value::Text(String::new())),
        ]
        .concat();
        let unframed_b =
            [value_bits(&Value::Text("a".into())), value_bits(&Value::Text("\u{4}".into()))]
                .concat();
        assert_eq!(unframed_a, unframed_b, "the trap this guards no longer exists?");
    }

    /// Stage 1 proved celsius ⇄ fahrenheit is NOT a float64 bijection. This is
    /// the promised counterpart: with the correction delta as the residual —
    /// rex(c) = c - inverse_raw(forward(c)) — GetPut holds EXACTLY, by
    /// Sterbenz (the round-tripped value is within a few ulps of c, so the
    /// subtraction is exact, and adding the exact difference back reproduces
    /// c's representable value). The corpus here is curated finite floats:
    /// near-overflow values (|c| ~ 1e308) push forward to ±inf, and -0.0 dies
    /// in the residual ADDITION (0.0 + -0.0 = +0.0) — both are domain edges,
    /// not counter-examples to the mechanism.
    #[test]
    fn celsius_with_a_correction_residual_round_trips_floats() {
        let fwd = compile("def c_to_f(c):\n    return c * 9.0 / 5.0 + 32.0\n");
        let rex = compile(
            "def c_rex(c):\n    return c - (c * 9.0 / 5.0 + 32.0 - 32.0) * 5.0 / 9.0\n",
        );
        let inv = compile("def f_to_c(f, r):\n    return (f - 32.0) * 5.0 / 9.0 + r\n");
        let corpus: Vec<Value> = [
            0.0, 0.1, 37.0, -17.5, 1.0 / 3.0, 1e-8, 12.345, -40.0, 1e100, f64::NAN,
        ]
        .iter()
        .map(|x| Value::Float(*x))
        .collect();
        let n = verify_residual(
            &fwd,
            &rex,
            &inv,
            &corpus,
            mpedb_types::ColumnType::Float64,
        )
        .expect("the residual makes the intuitive pair actually reversible");
        assert_eq!(n as usize, corpus.len(), "every curated float round-trips");

        // And the SAME functions, declared with the WRONG residual type, are
        // refused with the declaration named — declared, then verified.
        let fault = verify_residual(&fwd, &rex, &inv, &corpus, mpedb_types::ColumnType::Text)
            .expect_err("a float residual is not `text`");
        let msg = fault.to_string();
        assert!(msg.contains("declared residual type `text`"), "{msg}");
    }
}

