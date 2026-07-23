//! Wire codec (`encode_into` / `decode`) and the static verifier
//! (`validate`) for [`ExprProgram`], plus the opcode constants.

use super::*;
use crate::value::{read_value, write_value};

const OP_PUSH_COL: u8 = 1;
const OP_PUSH_PARAM: u8 = 2;
const OP_PUSH_CONST: u8 = 3;
const OP_EQ: u8 = 4;
const OP_NE: u8 = 5;
const OP_LT: u8 = 6;
const OP_LE: u8 = 7;
const OP_GT: u8 = 8;
const OP_GE: u8 = 9;
const OP_ADD: u8 = 10;
const OP_SUB: u8 = 11;
const OP_MUL: u8 = 12;
const OP_DIV: u8 = 13;
const OP_MOD: u8 = 14;
const OP_NEG: u8 = 15;
const OP_AND: u8 = 16;
const OP_OR: u8 = 17;
const OP_NOT: u8 = 18;
const OP_IS_NULL: u8 = 19;
const OP_IS_NOT_NULL: u8 = 20;
const OP_TO_FLOAT: u8 = 21;
const OP_LIKE: u8 = 22;
const OP_IN_PARAM: u8 = 23;
const OP_IN_LIST: u8 = 24;
const OP_JUMP_IF_NOT_TRUE: u8 = 25;
const OP_JUMP: u8 = 26;
const OP_JUMP_IF_NOT_NULL: u8 = 27;
const OP_POP: u8 = 28;
const OP_CALL: u8 = 29;
const OP_CAST: u8 = 30;
const OP_CONCAT: u8 = 31;
const OP_IS_NOT_DISTINCT: u8 = 32;
const OP_IS_DISTINCT: u8 = 33;
const OP_GLOB: u8 = 34;
const OP_REGEXP: u8 = 35;
const OP_CMP_COLL: u8 = 36;
const OP_IN_LIST_COLL: u8 = 37;
const OP_LIKE_CS: u8 = 38;
const OP_HOST_CALL: u8 = 39;
// `Instr::Affinity` took the free tag 40 as planned. `CmpClass` had to move off
// 41: `LIKE … ESCAPE` landed first and owns 41/42, so the class-aware comparison
// is 43. An opcode hole costs nothing (an unknown tag is already a decode error)
// while a collision would silently reinterpret one opcode as the other.
const OP_LIKE_ESC: u8 = 41;
const OP_LIKE_CS_ESC: u8 = 42;
const OP_AFFINITY: u8 = 40;
const OP_CMP_CLASS: u8 = 43;
// The bitwise family (task #74 item 2) starts at 50 rather than 44: three other
// branches were in flight in the 44..49 window, and an opcode hole costs nothing
// (an unknown tag is already a decode error) while a collision would silently
// reinterpret one opcode as another.
const OP_BIT_AND: u8 = 50;
const OP_BIT_OR: u8 = 51;
const OP_SHL: u8 = 52;
const OP_SHR: u8 = 53;
const OP_BIT_NOT: u8 = 54;
// #74 item 3: REGEXP with the pattern from the STACK instead of the const pool.
const OP_REGEXP_DYN: u8 = 55;
// The LIKE/GLOB halves of the same gap: the pattern from the STACK. Four LIKE
// tags mirroring the const-pool family (dialect × escape-ness are compile-time
// properties, so they select the opcode — the ESCAPE argument itself stays a
// const-pool literal by deliberate policy); GLOB has no dialect and no ESCAPE,
// so one tag covers it.
const OP_LIKE_DYN: u8 = 56;
const OP_LIKE_CS_DYN: u8 = 57;
const OP_LIKE_DYN_ESC: u8 = 58;
const OP_LIKE_CS_DYN_ESC: u8 = 59;
const OP_GLOB_DYN: u8 = 60;
// The collated scalar-call twin (min()/max() under a column's collation).
const OP_CALL_COLL: u8 = 61;
const OP_SPELL_CALL: u8 = 62;

impl ExprProgram {
    /// Deterministic serialization (part of plan blobs and plan hashing).
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.consts.len() as u16).to_le_bytes());
        for c in &self.consts {
            write_value(buf, c);
        }
        buf.extend_from_slice(&(self.instrs.len() as u32).to_le_bytes());
        for &i in &self.instrs {
            match i {
                Instr::PushCol(x) => {
                    buf.push(OP_PUSH_COL);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::PushParam(x) => {
                    buf.push(OP_PUSH_PARAM);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::PushConst(x) => {
                    buf.push(OP_PUSH_CONST);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Like(x) => {
                    buf.push(OP_LIKE);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::LikeCs(x) => {
                    buf.push(OP_LIKE_CS);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::LikeEsc(p, e) => {
                    buf.push(OP_LIKE_ESC);
                    buf.extend_from_slice(&p.to_le_bytes());
                    buf.extend_from_slice(&e.to_le_bytes());
                }
                Instr::LikeCsEsc(p, e) => {
                    buf.push(OP_LIKE_CS_ESC);
                    buf.extend_from_slice(&p.to_le_bytes());
                    buf.extend_from_slice(&e.to_le_bytes());
                }
                Instr::Glob(x) => {
                    buf.push(OP_GLOB);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Regexp(x) => {
                    buf.push(OP_REGEXP);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::InList(x) => {
                    buf.push(OP_IN_LIST);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::JumpIfNotTrue(x) => {
                    buf.push(OP_JUMP_IF_NOT_TRUE);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Jump(x) => {
                    buf.push(OP_JUMP);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::JumpIfNotNull(x) => {
                    buf.push(OP_JUMP_IF_NOT_NULL);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Pop => buf.push(OP_POP),
                Instr::Call(f, argc) => {
                    buf.push(OP_CALL);
                    buf.push(f as u8);
                    buf.push(argc);
                }
                Instr::CallColl(f, argc, coll) => {
                    buf.push(OP_CALL_COLL);
                    buf.push(f as u8);
                    buf.push(argc);
                    buf.push(coll as u8);
                }
                Instr::InParam(x) => {
                    buf.push(OP_IN_PARAM);
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                Instr::Eq => buf.push(OP_EQ),
                Instr::Ne => buf.push(OP_NE),
                Instr::Lt => buf.push(OP_LT),
                Instr::Le => buf.push(OP_LE),
                Instr::Gt => buf.push(OP_GT),
                Instr::Ge => buf.push(OP_GE),
                Instr::Add => buf.push(OP_ADD),
                Instr::Sub => buf.push(OP_SUB),
                Instr::Mul => buf.push(OP_MUL),
                Instr::Div => buf.push(OP_DIV),
                Instr::Mod => buf.push(OP_MOD),
                Instr::Neg => buf.push(OP_NEG),
                Instr::And => buf.push(OP_AND),
                Instr::Or => buf.push(OP_OR),
                Instr::Not => buf.push(OP_NOT),
                Instr::IsNull => buf.push(OP_IS_NULL),
                Instr::IsNotNull => buf.push(OP_IS_NOT_NULL),
                Instr::IsNotDistinct => buf.push(OP_IS_NOT_DISTINCT),
                Instr::IsDistinct => buf.push(OP_IS_DISTINCT),
                Instr::ToFloat => buf.push(OP_TO_FLOAT),
                Instr::Cast(aff) => {
                    buf.push(OP_CAST);
                    buf.push(aff as u8);
                }
                Instr::Concat => buf.push(OP_CONCAT),
                Instr::BitAnd => buf.push(OP_BIT_AND),
                Instr::BitOr => buf.push(OP_BIT_OR),
                Instr::Shl => buf.push(OP_SHL),
                Instr::Shr => buf.push(OP_SHR),
                Instr::BitNot => buf.push(OP_BIT_NOT),
                Instr::RegexpDyn => buf.push(OP_REGEXP_DYN),
                Instr::LikeDyn => buf.push(OP_LIKE_DYN),
                Instr::LikeCsDyn => buf.push(OP_LIKE_CS_DYN),
                Instr::LikeDynEsc(e) => {
                    buf.push(OP_LIKE_DYN_ESC);
                    buf.extend_from_slice(&e.to_le_bytes());
                }
                Instr::LikeCsDynEsc(e) => {
                    buf.push(OP_LIKE_CS_DYN_ESC);
                    buf.extend_from_slice(&e.to_le_bytes());
                }
                Instr::GlobDyn => buf.push(OP_GLOB_DYN),
                Instr::CmpColl(kind, coll) => {
                    buf.push(OP_CMP_COLL);
                    buf.push(kind as u8);
                    buf.push(coll as u8);
                }
                Instr::InListColl(x, coll) => {
                    buf.push(OP_IN_LIST_COLL);
                    buf.extend_from_slice(&x.to_le_bytes());
                    buf.push(coll as u8);
                }
                Instr::Affinity(aff) => {
                    buf.push(OP_AFFINITY);
                    buf.push(aff as u8);
                }
                Instr::CmpClass(kind, coll) => {
                    buf.push(OP_CMP_CLASS);
                    buf.push(kind as u8);
                    buf.push(coll as u8);
                }
                Instr::HostCall(name_idx, argc) => {
                    buf.push(OP_HOST_CALL);
                    buf.extend_from_slice(&name_idx.to_le_bytes());
                    buf.extend_from_slice(&argc.to_le_bytes());
                }
                Instr::SpellCall(hash_idx, argc) => {
                    buf.push(OP_SPELL_CALL);
                    buf.extend_from_slice(&hash_idx.to_le_bytes());
                    buf.extend_from_slice(&argc.to_le_bytes());
                }
            }
        }
    }

    /// Bounds-checked decode; re-validates stack discipline so a corrupt or
    /// hostile plan blob cannot cause memory unsafety or panics at eval time.
    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<ExprProgram> {
        let err = || Error::Corrupt("truncated expression program".into());
        let nconsts = {
            let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
            *pos += 2;
            u16::from_le_bytes(raw.try_into().unwrap()) as usize
        };
        let mut consts = Vec::with_capacity(nconsts.min(1024));
        for _ in 0..nconsts {
            consts.push(read_value(buf, pos)?);
        }
        let ninstrs = {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            u32::from_le_bytes(raw.try_into().unwrap()) as usize
        };
        if ninstrs > 1 << 20 {
            return Err(Error::Corrupt("expression too large".into()));
        }
        let mut instrs = Vec::with_capacity(ninstrs.min(4096));
        for _ in 0..ninstrs {
            let op = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            let mut read_u16_arg = || -> Result<u16> {
                let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
                *pos += 2;
                Ok(u16::from_le_bytes(raw.try_into().unwrap()))
            };
            instrs.push(match op {
                OP_PUSH_COL => Instr::PushCol(read_u16_arg()?),
                OP_PUSH_PARAM => Instr::PushParam(read_u16_arg()?),
                OP_PUSH_CONST => Instr::PushConst(read_u16_arg()?),
                OP_LIKE => Instr::Like(read_u16_arg()?),
                OP_LIKE_CS => Instr::LikeCs(read_u16_arg()?),
                OP_LIKE_ESC => {
                    let p = read_u16_arg()?;
                    let e = read_u16_arg()?;
                    Instr::LikeEsc(p, e)
                }
                OP_LIKE_CS_ESC => {
                    let p = read_u16_arg()?;
                    let e = read_u16_arg()?;
                    Instr::LikeCsEsc(p, e)
                }
                OP_GLOB => Instr::Glob(read_u16_arg()?),
                OP_REGEXP => Instr::Regexp(read_u16_arg()?),
                OP_IN_PARAM => Instr::InParam(read_u16_arg()?),
                OP_IN_LIST => Instr::InList(read_u16_arg()?),
                OP_JUMP_IF_NOT_TRUE => Instr::JumpIfNotTrue(read_u16_arg()?),
                OP_JUMP => Instr::Jump(read_u16_arg()?),
                OP_JUMP_IF_NOT_NULL => Instr::JumpIfNotNull(read_u16_arg()?),
                OP_POP => Instr::Pop,
                OP_CALL => {
                    let f = *buf.get(*pos).ok_or_else(err)?;
                    let argc = *buf.get(*pos + 1).ok_or_else(err)?;
                    *pos += 2;
                    Instr::Call(ScalarFn::from_tag(f)?, argc)
                }
                OP_CALL_COLL => {
                    let f = *buf.get(*pos).ok_or_else(err)?;
                    let argc = *buf.get(*pos + 1).ok_or_else(err)?;
                    let c = *buf.get(*pos + 2).ok_or_else(err)?;
                    *pos += 3;
                    let coll = Collation::from_tag(c)
                        .ok_or_else(|| Error::Corrupt("bad collation tag".into()))?;
                    Instr::CallColl(ScalarFn::from_tag(f)?, argc, coll)
                }
                OP_EQ => Instr::Eq,
                OP_NE => Instr::Ne,
                OP_LT => Instr::Lt,
                OP_LE => Instr::Le,
                OP_GT => Instr::Gt,
                OP_GE => Instr::Ge,
                OP_ADD => Instr::Add,
                OP_SUB => Instr::Sub,
                OP_MUL => Instr::Mul,
                OP_DIV => Instr::Div,
                OP_MOD => Instr::Mod,
                OP_NEG => Instr::Neg,
                OP_AND => Instr::And,
                OP_OR => Instr::Or,
                OP_NOT => Instr::Not,
                OP_IS_NULL => Instr::IsNull,
                OP_IS_NOT_NULL => Instr::IsNotNull,
                OP_IS_NOT_DISTINCT => Instr::IsNotDistinct,
                OP_IS_DISTINCT => Instr::IsDistinct,
                OP_TO_FLOAT => Instr::ToFloat,
                OP_CAST => {
                    let t = *buf.get(*pos).ok_or_else(err)?;
                    *pos += 1;
                    Instr::Cast(
                        Affinity::from_tag(t)
                            .ok_or_else(|| Error::Corrupt("bad CAST affinity tag".into()))?,
                    )
                }
                OP_CONCAT => Instr::Concat,
                OP_BIT_AND => Instr::BitAnd,
                OP_BIT_OR => Instr::BitOr,
                OP_SHL => Instr::Shl,
                OP_SHR => Instr::Shr,
                OP_BIT_NOT => Instr::BitNot,
                OP_REGEXP_DYN => Instr::RegexpDyn,
                OP_LIKE_DYN => Instr::LikeDyn,
                OP_LIKE_CS_DYN => Instr::LikeCsDyn,
                OP_LIKE_DYN_ESC => Instr::LikeDynEsc(read_u16_arg()?),
                OP_LIKE_CS_DYN_ESC => Instr::LikeCsDynEsc(read_u16_arg()?),
                OP_GLOB_DYN => Instr::GlobDyn,
                OP_CMP_COLL => {
                    let k = *buf.get(*pos).ok_or_else(err)?;
                    let c = *buf.get(*pos + 1).ok_or_else(err)?;
                    *pos += 2;
                    let kind = CmpKind::from_tag(k)
                        .ok_or_else(|| Error::Corrupt("bad collated-compare op tag".into()))?;
                    let coll = Collation::from_tag(c)
                        .ok_or_else(|| Error::Corrupt("bad collation tag".into()))?;
                    Instr::CmpColl(kind, coll)
                }
                OP_IN_LIST_COLL => {
                    let x = read_u16_arg()?;
                    let c = *buf.get(*pos).ok_or_else(err)?;
                    *pos += 1;
                    let coll = Collation::from_tag(c)
                        .ok_or_else(|| Error::Corrupt("bad collation tag".into()))?;
                    Instr::InListColl(x, coll)
                }
                OP_AFFINITY => {
                    let t = *buf.get(*pos).ok_or_else(err)?;
                    *pos += 1;
                    Instr::Affinity(
                        Affinity::from_tag(t)
                            .ok_or_else(|| Error::Corrupt("bad affinity tag".into()))?,
                    )
                }
                OP_CMP_CLASS => {
                    let k = *buf.get(*pos).ok_or_else(err)?;
                    let c = *buf.get(*pos + 1).ok_or_else(err)?;
                    *pos += 2;
                    let kind = CmpKind::from_tag(k)
                        .ok_or_else(|| Error::Corrupt("bad class-compare op tag".into()))?;
                    let coll = Collation::from_tag(c)
                        .ok_or_else(|| Error::Corrupt("bad collation tag".into()))?;
                    Instr::CmpClass(kind, coll)
                }
                OP_HOST_CALL => {
                    let name_idx = read_u16_arg()?;
                    let argc = read_u16_arg()?;
                    Instr::HostCall(name_idx, argc)
                }
                OP_SPELL_CALL => {
                    let hash_idx = read_u16_arg()?;
                    let argc = read_u16_arg()?;
                    Instr::SpellCall(hash_idx, argc)
                }
                _ => return Err(Error::Corrupt(format!("invalid opcode {op}"))),
            });
        }
        ExprProgram::new(instrs, consts)
    }
}

/// Static verification, so `eval` needs no per-instruction safety checks:
/// const indices in range, no stack underflow, exactly one value left — and,
/// with jumps in the language, two more properties that used to be free.
///
/// **Termination.** Jump targets must be strictly FORWARD. A backward jump
/// would make an expression able to loop, and `eval` runs per row inside the
/// engine with no fuel counter: a crafted plan could hang a reader forever.
/// Forward-only makes the program a DAG, so it always terminates.
///
/// **Depth agreement at merge points.** With branches, "the depth here" is no
/// longer one number per program: an instruction can be reached from several
/// predecessors, and if they disagree the stack means different things
/// depending on the row's data. This walks a depth-per-index map and refuses
/// any disagreement, which is what keeps `max_stack` a real bound and every
/// `pop().expect("validated")` honest.
///
/// Returns the maximum stack depth over all paths.
pub(super) fn validate(instrs: &[Instr], consts: &[Value]) -> Result<usize> {
    if instrs.is_empty() {
        return Err(Error::Corrupt("empty expression program".into()));
    }
    let n = instrs.len();
    // depth on ENTRY to each index; index n is the program's exit.
    let mut depth_at: Vec<Option<usize>> = vec![None; n + 1];
    depth_at[0] = Some(0);
    let mut max = 0usize;

    // Record the depth on entry to `t`, rejecting a disagreement.
    fn merge(depth_at: &mut [Option<usize>], t: usize, d: usize) -> Result<()> {
        match depth_at[t] {
            None => {
                depth_at[t] = Some(d);
                Ok(())
            }
            Some(prev) if prev == d => Ok(()),
            Some(prev) => Err(Error::Corrupt(format!(
                "stack depth disagrees at instruction {t}: {prev} on one path, {d} on another"
            ))),
        }
    }

    for i in 0..n {
        // Every instruction must be reachable. Dead code after a Jump is not a
        // harmless curiosity here: it means the encoder or a corrupt plan built
        // something whose stack shape was never proven.
        let d = depth_at[i]
            .ok_or_else(|| Error::Corrupt(format!("instruction {i} is unreachable")))?;
        max = max.max(d);

        let check_target = |t: u16| -> Result<usize> {
            let t = t as usize;
            if t <= i {
                return Err(Error::Corrupt(format!(
                    "backward jump {i} -> {t}: expression programs must terminate"
                )));
            }
            if t > n {
                return Err(Error::Corrupt(format!("jump target {t} past end of program")));
            }
            Ok(t)
        };

        match instrs[i] {
            Instr::Jump(t) => {
                let t = check_target(t)?;
                merge(&mut depth_at, t, d)?;
                // no fall-through: depth_at[i+1] is set only if something jumps there
            }
            Instr::JumpIfNotTrue(t) => {
                let t = check_target(t)?;
                if d < 1 {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                let d = d - 1; // pops the condition on BOTH paths
                merge(&mut depth_at, t, d)?;
                merge(&mut depth_at, i + 1, d)?;
            }
            Instr::JumpIfNotNull(t) => {
                let t = check_target(t)?;
                if d < 1 {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                // Peeks: the depth is UNCHANGED on both paths.
                merge(&mut depth_at, t, d)?;
                merge(&mut depth_at, i + 1, d)?;
            }
            instr => {
                let (pops, pushes) = match instr {
                    Instr::PushCol(_) | Instr::PushParam(_) => (0, 1),
                    Instr::PushConst(c) => {
                        if c as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        (0, 1)
                    }
                    Instr::Like(c) | Instr::LikeCs(c) | Instr::Glob(c) | Instr::Regexp(c) => {
                        if c as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        (1, 1)
                    }
                    // Both pool slots must exist, AND the escape slot must hold
                    // a one-character TEXT — proved here so `eval` can never be
                    // handed a plan whose escape is a blob, a number, or the
                    // empty string (sqlite errors on all three).
                    Instr::LikeEsc(p, e) | Instr::LikeCsEsc(p, e) => {
                        if p as usize >= consts.len() || e as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        escape_char(&consts[e as usize])?;
                        (1, 1)
                    }
                    // The dyn-pattern escape forms: the pattern is on the
                    // stack, but the escape slot gets the same one-character-
                    // text proof as the const forms above. (The escape-less
                    // dyn forms — LikeDyn/LikeCsDyn/GlobDyn — are plain (2, 1)
                    // ops and ride the catch-all, like RegexpDyn.)
                    Instr::LikeDynEsc(e) | Instr::LikeCsDynEsc(e) => {
                        if e as usize >= consts.len() {
                            return Err(Error::Corrupt("const index out of range".into()));
                        }
                        escape_char(&consts[e as usize])?;
                        (2, 1)
                    }
                    // Pops the probe scalar, pushes the 3VL result; the list comes
                    // from a param slot, not the stack, so the arity is not here.
                    Instr::InParam(_) => (1, 1),
                    Instr::Cast(_) | Instr::Affinity(_) => (1, 1),
                    Instr::Concat => (2, 1),
                    // n list elements plus the probe beneath them. n == 0 is the
                    // empty set `x IN ()`: eval pops the probe and pushes FALSE
                    // (`in_items_3vl` on an empty slice), so it is a valid (1, 1)
                    // op — NOT a no-op that leaves the probe posing as a bool.
                    Instr::InList(nl) | Instr::InListColl(nl, _) => (nl as usize + 1, 1),
                    Instr::Neg
                    | Instr::Not
                    | Instr::IsNull
                    | Instr::IsNotNull
                    | Instr::ToFloat
                    | Instr::BitNot => (1, 1),
                    Instr::Pop => (1, 0),
                    // Arity is checked HERE, once per program, so eval can index
                    // the args without re-checking per row.
                    Instr::Call(f, argc) | Instr::CallColl(f, argc, _) => {
                        if !f.arity_ok(argc) {
                            return Err(Error::Corrupt(format!(
                                "{}() called with {argc} argument(s)",
                                f.name()
                            )));
                        }
                        (argc as usize, 1)
                    }
                    // A host UDF call pops `argc` and pushes its one result. The
                    // name index must be a real const (checked here, once) so
                    // eval can read it without re-checking; the closure itself is
                    // supplied at eval time, not validated in the plan bytes.
                    Instr::HostCall(name_idx, argc) => {
                        if name_idx as usize >= consts.len() {
                            return Err(Error::Corrupt(
                                "host-call name index out of range".into(),
                            ));
                        }
                        (argc as usize, 1)
                    }
                    // A stored-function call: the const must BE a 32-byte
                    // hash, checked once here so eval reads it unguarded and a
                    // forged plan fails decode, never execution.
                    Instr::SpellCall(hash_idx, argc) => {
                        match consts.get(hash_idx as usize) {
                            Some(Value::Blob(b)) if b.len() == 32 => {}
                            _ => {
                                return Err(Error::Corrupt(
                                    "spell-call constant is not a 32-byte hash".into(),
                                ))
                            }
                        }
                        (argc as usize, 1)
                    }
                    Instr::Jump(_) | Instr::JumpIfNotTrue(_) | Instr::JumpIfNotNull(_) => {
                        unreachable!("handled above")
                    }
                    _ => (2, 1),
                };
                if d < pops {
                    return Err(Error::Corrupt("expression stack underflow".into()));
                }
                let nd = d - pops + pushes;
                max = max.max(nd);
                merge(&mut depth_at, i + 1, nd)?;
            }
        }
    }

    match depth_at[n] {
        Some(1) => Ok(max),
        Some(other) => Err(Error::Corrupt(format!(
            "expression program must leave exactly one value, leaves {other}"
        ))),
        None => Err(Error::Corrupt(
            "expression program never reaches its end".into(),
        )),
    }
}
