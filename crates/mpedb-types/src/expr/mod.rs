//! Compact stack-based expression IR (PySpell-style: compiled once at
//! prepare/attach, evaluated many times with no parsing or allocation-heavy
//! AST walking).
//!
//! Used for WHERE filters, projections with computed columns, and CHECK
//! constraints. Follows SQL three-valued logic: comparisons and arithmetic
//! with NULL yield NULL; AND/OR/NOT use Kleene logic; a filter passes only if
//! the result is exactly TRUE.

use crate::error::{Error, Result};
use crate::value::{Affinity, Collation, ColumnType, Value};
use std::cmp::Ordering;

mod value_ops;
use value_ops::{arith, cast_value, to_real, float_prefix, concat_bytes_n, concat_value};
pub(crate) use value_ops::{cast_pg, cast_typed, numeric_arith};
pub use value_ops::store_affinity;

mod codec;
mod datetime;
mod json;
mod ops;
mod printf;
mod scalar;

#[cfg(test)]
mod in_list_tests;
#[cfg(test)]
mod jump_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod vec_tests;

pub use datetime::{sqlite_now_parts, sqlite_now_string};
pub use scalar::ScalarFn;

use ops::{
    pg_like_match,
    bit_i64, bitwise, escape_char, glob_match, in_items_3vl, in_list_3vl,
    like_match, like_match_cs, regexp_match, CrossClass,
};
use scalar::{call_scalar, call_scalar_collated};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Instr {
    /// Push column value by index into the row.
    PushCol(u16),
    /// Push statement parameter ($1 = index 0).
    PushParam(u16),
    /// Push constant from the program's const pool.
    PushConst(u16),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// `/` and `%` under the PostgreSQL dialect: a zero divisor RAISES
    /// ([`Error::DivisionByZero`]) instead of yielding NULL.
    ///
    /// Separate opcodes rather than a runtime flag, mirroring
    /// [`Instr::Like`] vs [`Instr::LikeCs`]: the dialect is a COMPILE-time
    /// property, so it belongs in the plan bytes. Two dialects then hash to
    /// two plans, and the shared plan registry cannot serve one session's
    /// semantics to another's — which a flag read at eval time would do the
    /// moment two differently-configured processes attach to one file.
    DivStrict,
    ModStrict,
    Neg,
    And,
    Or,
    Not,
    IsNull,
    IsNotNull,
    /// `a IS b` — NULL-safe equality ("is not distinct from"). Pops 2, pushes a
    /// Bool, and NEVER pushes NULL: TRUE when both operands are NULL, FALSE when
    /// exactly one is, otherwise `a = b`. Two-valued, unlike [`Instr::Eq`], which
    /// yields NULL on a NULL side. This is what sqlite's `IS` operator means.
    IsNotDistinct,
    /// `a IS NOT b` — NULL-safe inequality ("is distinct from"), the exact
    /// negation of [`Instr::IsNotDistinct`]. Also two-valued: never NULL.
    IsDistinct,
    /// Coerce Int -> Float (inserted by the binder for mixed numerics).
    ToFloat,
    /// `CAST(x AS <type>)` — sqlite's permissive, affinity-based conversion
    /// (the type name resolves to one of five [`Affinity`]s in the binder).
    /// NULL casts to NULL; text→number parses a leading numeric prefix
    /// (`'12ab'`→12); real→int truncates toward zero; `NUMERIC` yields an int
    /// when integral else a real. See `cast_value`.
    Cast(Affinity),
    /// `CAST(x AS <type>)` under the **PostgreSQL dialect** — a TYPED
    /// conversion that either produces the type or RAISES.
    ///
    /// A separate opcode from [`Instr::Cast`], not a flag on it, for the same
    /// reason [`Instr::DivStrict`] is separate from [`Instr::Div`]: the plan
    /// hash has to differ, or one compiled plan would serve two dialects that
    /// disagree about the answer.
    ///
    /// The difference is not a nicety. sqlite's rule takes a leading numeric
    /// PREFIX, so `'2020-01-02'` casts to the integer `2020` — and psycopg2
    /// renders every Python `date` parameter as `'…'::date`, which made that
    /// the ORDINARY write path rather than an edge case. `'6f8b…'::uuid` was
    /// the integer `6` for the same reason. This one parses the whole string
    /// or reports `invalid input syntax for type <t>`.
    ///
    /// The payload is the **PostgreSQL type OID**, not an mpedb `ColumnType`,
    /// because the mpedb type cannot say which parse to run: `uuid` and
    /// `bytea` are both `Blob` and read their text completely differently.
    CastPg(u32),
    /// Validate an enum label and replace it with its declaration-order rank.
    EnumRank(u16),
    /// Validate a label without changing its text value.
    EnumValidate(u16),
    /// `a || b` — SQL concatenation. NULL propagates; ints and bools render
    /// as text first (sqlite's rule); floats are refused until someone needs
    /// their formatting pinned down.
    Concat,
    /// SQL LIKE with pattern from the const pool (supports % and _).
    Like(u16),
    /// Case-SENSITIVE LIKE (PostgreSQL dialect); otherwise identical to
    /// [`Instr::Like`]. Emitted for a `dialect = "postgres"` database,
    /// where `'a' LIKE 'A'` is FALSE. See [`like_match_cs`](ops::like_match_cs).
    LikeCs(u16),
    /// `x LIKE <pattern> ESCAPE <c>` — like [`Instr::Like`], with a SECOND
    /// const-pool index holding the one-character escape string. A separate
    /// opcode rather than a wider [`Instr::Like`] so that the overwhelmingly
    /// common escape-less LIKE keeps its exact two-byte operand encoding.
    LikeEsc(u16, u16),
    /// Case-SENSITIVE `LIKE … ESCAPE` (PostgreSQL dialect); [`Instr::LikeEsc`]
    /// is to [`Instr::LikeCsEsc`] what [`Instr::Like`] is to [`Instr::LikeCs`].
    LikeCsEsc(u16, u16),
    /// SQL GLOB with pattern from the const pool. Like [`Instr::Like`] but
    /// case-SENSITIVE, and the wildcards are sqlite's `*` (any run), `?` (one
    /// char) and `[...]` character classes rather than `%`/`_`. Same operand
    /// typing and NULL rules: any NULL operand yields NULL.
    Glob(u16),
    /// SQL REGEXP with pattern from the const pool. Like [`Instr::Glob`] but the
    /// pattern is sqlite's bundled `ext/misc/regexp.c` dialect (`.`, `* + ?`,
    /// `{p,q}`, `[...]`, `^`/`$`, `|`, `(...)`, `\d`/`\w`/`\s`/`\b`, escapes) —
    /// case-SENSITIVE, unanchored substring match. `NOT REGEXP` is a `Not`
    /// wrapped around this by the binder. Same operand typing and NULL rules:
    /// any NULL operand yields NULL. See [`regexp_match`].
    Regexp(u16),
    /// `<scalar> IN (<list param n>)` — set membership against a
    /// [`Value::List`] bound to parameter `n` (design/DESIGN-MULTIDB.md §2.6).
    ///
    /// The list is a PARAM, not a const: that is the whole design. Arity lives
    /// in the data, so the plan bytes — and therefore the plan hash — stay
    /// independent of how many orgs a given session belongs to, and one
    /// compiled plan serves every session (§4.1). Baking the list into the
    /// const pool would mint a plan per distinct membership set.
    InParam(u16),
    /// The collated twin of [`Instr::InParam`] — `<probe> IN (<subquery>)` where
    /// the comparison runs under a COLLATION rather than bytewise.
    ///
    /// Emitted only for a non-Binary collation, so an ordinary `IN (SELECT …)`
    /// keeps the plain opcode and identical bytes. The failure it prevents is a
    /// wrong answer, not a missing feature: with `a TEXT COLLATE NOCASE`
    /// holding 'AB' and the subquery yielding 'ab', `a IN (SELECT x FROM q)`
    /// returned NO rows where sqlite returns the row, and `NOT IN` returned
    /// BOTH rows where sqlite returns one.
    InParamColl(u16, Collation),
    /// n-ary `||` over RAW BYTES (plan §8, format 70): pops `n` operands,
    /// concatenates their byte forms (text/blob as-is; int/bool rendered;
    /// NULL propagates), and pushes TEXT when the result is valid UTF-8 —
    /// which is how `? || ?` fed `C3` + `A9` yields `'é'` exactly as sqlite
    /// (the bytes RECOMBINE across operands; measured). An invalid result is
    /// [`Error::NonUtf8Concat`] — the bytes never enter a `Value`, so
    /// `Value::Text`'s valid-UTF-8 invariant stands. Emitted ONLY when an
    /// operand may be a blob; pure text chains keep [`Instr::Concat`] and
    /// their byte-identical plans.
    ConcatN(u16),
    /// `<scalar> IN (<e1>, …, <en>)` — set membership against `n` values taken
    /// from the STACK (general SQL `IN`, task #21). Pops `n` list elements plus
    /// the probe beneath them, pushes the 3VL verdict.
    ///
    /// The counterpart of [`Instr::InParam`], and the split is deliberate: this
    /// form's arity IS part of the query text, so encoding it in the plan is
    /// correct — `x IN (1,2)` and `x IN (1,2,3)` are different queries and
    /// should hash differently. `InParam` exists precisely because a session's
    /// membership set must NOT reach the plan bytes (§4.1). Same 3VL either way.
    ///
    /// Not desugared into an OR-chain even though the 3VL works out identically:
    /// that duplicates the probe expression `n` times in the plan, and the probe
    /// may be arbitrarily large.
    InList(u16),
    /// Jump to absolute instruction index `t` unless the popped value is
    /// exactly TRUE — i.e. jump on FALSE **and on NULL**.
    ///
    /// Named for what it does rather than `JumpIfFalse`, because the NULL case
    /// is the whole subtlety: `CASE WHEN <null> THEN a ELSE b END` yields `b`,
    /// so an unknown condition must not be taken. A `JumpIfFalse` that treated
    /// NULL as "not false" would silently take the branch.
    JumpIfNotTrue(u16),
    /// Unconditional jump to absolute instruction index `t`.
    Jump(u16),
    /// PEEK the top of the stack: jump to `t` if it is NOT NULL, leaving it in
    /// place; otherwise fall through, still leaving it (a following [`Instr::Pop`]
    /// discards it).
    ///
    /// This is what makes `coalesce` lazy. Eager evaluation would not just be a
    /// nicety here: mpedb raises on arithmetic overflow, so an eager
    /// `coalesce(x, <overflowing expr>)` would ERROR where both sqlite and
    /// PostgreSQL return x. (Division by zero is NOT such a case: like sqlite,
    /// mpedb evaluates `1/0` to NULL rather than raising.)
    JumpIfNotNull(u16),
    /// Discard the top of the stack.
    Pop,
    /// Call a scalar function over the top `argc` values (leftmost deepest),
    /// replacing them with one result.
    Call(ScalarFn, u8),
    /// [`Instr::Call`] under a collating sequence — the collated twin the
    /// binder emits ONLY for the scalar `max(a,b,…)`/`min(a,b,…)` when the
    /// first argument (left to right) that DEFINES a collating sequence
    /// defines a non-`Binary` one (sqlite's `SQLITE_FUNC_NEEDCOLL` rule; a
    /// bare column defines its declared collation, and BINARY counts as
    /// defined, so `max(bincol, nocasecol)` stays a plain [`Instr::Call`]).
    /// Every other function ignores its collation and never gets this opcode.
    CallColl(ScalarFn, u8, Collation),
    /// A comparison under an explicit collating sequence (task: COLLATE). Pops
    /// two values and pushes the 3VL verdict, exactly like the plain
    /// [`Instr::Eq`]..[`Instr::Ge`] family, but TEXT operands are ordered by
    /// `Collation` instead of bytewise. The binder emits it ONLY when the
    /// resolved collation is non-`Binary` and the compared type is text — a
    /// `Binary` or numeric comparison stays a plain nullary opcode, so existing
    /// plan bytes are unchanged.
    CmpColl(CmpKind, Collation),
    /// `<scalar> IN (<e1>, …, <en>)` under an explicit collation — the collated
    /// twin of [`Instr::InList`]. Pops `n` list elements plus the probe beneath
    /// them; text membership is decided under `Collation`. Emitted only for a
    /// non-`Binary` collation on a text probe.
    InListColl(u16, Collation),
    /// Call a HOST-registered scalar UDF (the C-API `create_function` path,
    /// design/DESIGN-UDF.md). The first `u16` is the const-pool index of the
    /// function NAME (a [`Value::Text`]); the second is the argument count. Pops
    /// `argc` values (leftmost deepest), looks the name up in the eval context's
    /// [`HostFns`], invokes it, and pushes the one result.
    ///
    /// The plan stores only the NAME + arity, never the closure — closures are
    /// not serializable, and a plan carrying a host call is valid ONLY for a
    /// connection that registered that UDF, so the facade never publishes it to
    /// the shared plan registry. With no host functions in scope (a plan without
    /// a host call, a CHECK constraint, a test) this opcode is unreachable, so
    /// the change is behavior-neutral for every existing plan.
    HostCall(u16, u16),
    /// Call a STORED PySpell function (stage M2, design/DESIGN-MODEL-LANG.md
    /// program): pops `argc` args, pushes the one result. The u16 indexes the
    /// const pool at a 32-byte [`Value::Blob`] holding the function's CONTENT
    /// HASH — not a name. That is the whole difference from [`Instr::HostCall`]:
    /// the definition lives in the database file, schema_gen-gated, so a plan
    /// carrying the hash is deterministic across every attached process and
    /// MAY enter the shared registry (a host closure may not).
    SpellCall(u16, u16),
    /// **Comparison affinity**: apply `Affinity` to the top of the stack, in
    /// place. sqlite's `OP_Affinity`/`applyAffinity`, the same conversion a
    /// value gets on its way INTO a column ([`store_affinity`]) — which is why
    /// this is that function and not [`Instr::Cast`].
    ///
    /// The two are NOT interchangeable and the difference is the whole reason
    /// this opcode exists: `CAST('abc' AS NUMERIC)` FORCES a number (0), while
    /// affinity CONVERTS ONLY WHEN LOSSLESS and otherwise leaves the value
    /// alone — `'abc'` stays the text `'abc'` and then compares as text. Using
    /// `Cast` here would turn every unconvertible operand into 0 and answer
    /// `price < 'abc'` as `price < 0`, which is not sqlite's answer.
    ///
    /// Emitted only in front of [`Instr::CmpClass`], by the binder rule that
    /// mirrors sqlite's `sqlite3CompareAffinity`.
    Affinity(Affinity),
    /// A comparison by sqlite's **storage-class order** — the comparison sqlite
    /// performs AFTER comparison affinity has been applied to both operands.
    /// Pops two values and pushes the 3VL verdict like [`Instr::Eq`]..
    /// [`Instr::Ge`], but a cross-class pair is ORDERED (NULL < numbers < text
    /// < blob, see [`Value::sort_cmp`]) instead of refused.
    ///
    /// **The binder emits it only where it has established sqlite's affinity
    /// rule for that pair**, and that restriction is load-bearing: ordering a
    /// cross-class pair without first applying affinity is a WRONG answer, not
    /// a conservative one (`price < '40.0'` would compare a number against a
    /// text and say "every number is smaller", where sqlite compares against
    /// 40.0). Every comparison the binder has not vetted keeps the plain
    /// opcodes, which keep refusing.
    ///
    /// mpedb's own `Bool`/`Timestamp` have no sqlite storage class, so a pair
    /// involving one against a different class is REFUSED here rather than
    /// given an invented rank.
    CmpClass(CmpKind, Collation),
    /// `a & b`, `a | b` — sqlite's bitwise AND / OR. Pop 2, push 1.
    ///
    /// Both operands go through [`bit_i64`], which is `sqlite3VdbeIntValue`:
    /// integers pass, reals TRUNCATE toward zero and clamp to the i64 range,
    /// text and blobs take sqlite's integer-PREFIX parse. Any NULL operand
    /// yields NULL. The result is always an integer, in sqlite and here.
    ///
    /// The binder only ever emits these over operands it typed `int64`, `bool`
    /// or `any` — the coercion of a real or a text is reachable only through an
    /// `any` (typeless) value, exactly like [`Instr::CmpClass`]. A statically
    /// typed real/text operand is a bind error naming `CAST`.
    BitAnd,
    BitOr,
    /// `a << b`, `a >> b` — sqlite's shifts, ported from `OP_ShiftLeft` /
    /// `OP_ShiftRight` including the two corners that are easy to get wrong:
    ///
    /// * a NEGATIVE shift count shifts the OTHER WAY (`1 >> -1` is `1 << 1`,
    ///   i.e. 2), with counts at or below -64 clamped to 64;
    /// * a count of 64 or more gives 0 — except `>>` of a NEGATIVE value, which
    ///   gives -1, because the right shift is ARITHMETIC (sign-extending).
    ///
    /// `<<` wraps rather than overflowing (`9223372036854775807 << 1` is -2):
    /// sqlite shifts the u64 bit pattern. This is the one place mpedb does not
    /// raise on integer overflow, and it is deliberate — a bit shift has no
    /// "overflow", only a bit pattern.
    Shl,
    Shr,
    /// `~a` — sqlite's bitwise NOT. Same [`bit_i64`] coercion, NULL propagates,
    /// result is always an integer (`~3.7` is -4: truncate to 3, then invert).
    BitNot,
    /// `x REGEXP y` with the pattern from the STACK rather than the const pool
    /// — the same matcher as [`Instr::Regexp`], for a pattern that is not a
    /// literal (a bound parameter, a column, any computed text). Pops the
    /// PATTERN, then the subject beneath it, and pushes the 3VL verdict.
    ///
    /// A separate opcode rather than a wider `Regexp` so the literal form —
    /// which is every REGEXP mpedb could compile before #74 — keeps its exact
    /// one-operand encoding and its plan bytes.
    ///
    /// Both forms go through [`regexp_match`], which memoizes the LAST compiled
    /// pattern per thread: the pattern is almost always the same on every row
    /// of a scan, whether it came from a literal or a parameter, so neither
    /// form recompiles per row.
    RegexpDyn,
    /// `x LIKE y` with the pattern from the STACK rather than the const pool —
    /// the LIKE counterpart of [`Instr::RegexpDyn`], for a pattern that is not
    /// a literal (a bound parameter — Django's exact wire shape for every
    /// `startswith`/`contains`/`endswith`/`icontains` lookup — a column, any
    /// computed value). Pops the PATTERN, then the subject beneath it, and
    /// pushes the 3VL verdict. Same operand typing and NULL rules as
    /// [`Instr::Like`]: any NULL operand yields NULL, and the operands must
    /// arrive as text — the binder wraps a coercible non-text operand
    /// (pattern AND subject alike) in the same `CAST(… AS TEXT)` it always
    /// wrapped subjects in, which is `sqlite3_value_text`'s conversion, i.e.
    /// what the differential-oracle sqlite itself applies inside `likeFunc`.
    ///
    /// FOUR dyn opcodes rather than one, mirroring the const-pool family
    /// exactly: the DIALECT (case-folding, [`Instr::Like`] vs
    /// [`Instr::LikeCs`]) and the ESCAPE-ness are compile-time properties —
    /// the ESCAPE argument is a literal by deliberate policy — so they select
    /// the opcode; only the pattern is runtime. The escape-less form keeps a
    /// one-byte encoding this way, exactly as [`Instr::Like`] kept its
    /// two-byte one when ESCAPE landed.
    ///
    /// Both pattern forms go through the [`ops`] matcher, which since this
    /// task memoizes the LAST compiled `(pattern, escape)` per thread — so
    /// neither form compiles the pattern per row, and the literal form got
    /// faster too.
    LikeDyn,
    /// Case-SENSITIVE (PostgreSQL dialect) [`Instr::LikeDyn`] —
    /// [`Instr::LikeCs`]'s dyn twin. The PG dialect's binder refuses non-text
    /// operands statically (no CAST bridge), so a non-text runtime value here
    /// is a corrupt-plan-grade [`Error::TypeMismatch`].
    LikeCsDyn,
    /// [`Instr::LikeDyn`] with an `ESCAPE` character at the given const-pool
    /// index (a one-character text, validated at decode exactly like
    /// [`Instr::LikeEsc`]'s). The ESCAPE stays a compile-time const while the
    /// pattern rides the stack — Django's exact shape (`s LIKE ? ESCAPE '\'`).
    LikeDynEsc(u16),
    /// Case-SENSITIVE `LIKE … ESCAPE` with a stack pattern;
    /// [`Instr::LikeDynEsc`] is to this what [`Instr::LikeEsc`] is to
    /// [`Instr::LikeCsEsc`].
    LikeCsDynEsc(u16),
    /// `x GLOB y` with the pattern from the STACK — [`Instr::Glob`]'s dyn
    /// twin, with `Glob`'s operand typing and NULL rules. GLOB has no dialect
    /// split (always case-sensitive) and no ESCAPE clause, so ONE opcode
    /// covers it.
    GlobDyn,
}

/// Resolve a HOST-registered scalar UDF by name at eval time (the C-API
/// `create_function` path, design/DESIGN-UDF.md). Implemented by the facade over
/// its per-connection registry; `None` is threaded wherever no UDF can be in
/// scope (CHECK constraints, tests, any plan without a [`Instr::HostCall`]), so
/// the whole mechanism stays inert for existing plans.
pub trait HostFns {
    /// Invoke the scalar function `name` over already-evaluated `args`
    /// (leftmost first), returning its result. An `Error` when no function of
    /// that name/arity is registered — defensive; the binder already checked the
    /// name at compile time, but a registration can change between compile and
    /// execute.
    fn call(&self, name: &str, args: &[Value]) -> Result<Value>;

    /// Invoke a STORED PySpell function by content hash
    /// ([`Instr::SpellCall`]). Defaulted to a refusal so every existing
    /// implementor (and every context that cannot reach the function catalog)
    /// stays correct by doing nothing: the facade overrides this with its
    /// gen-gated compiled-spell cache.
    fn call_spell(&self, hash: &[u8; 32], args: &[Value]) -> Result<Value> {
        let _ = args;
        Err(Error::Unsupported(format!(
            "stored function {} is not in scope for this execution",
            hash.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>()
        )))
    }
}

/// Which of the six SQL comparison operators a collated [`Instr::CmpColl`]
/// evaluates. A tiny closed enum so one collated opcode covers all six rather
/// than minting six; the plain uncollated comparisons stay their own nullary
/// opcodes for wire stability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CmpKind {
    Eq = 0,
    Ne = 1,
    Lt = 2,
    Le = 3,
    Gt = 4,
    Ge = 5,
}

impl CmpKind {
    pub fn from_tag(t: u8) -> Option<CmpKind> {
        Some(match t {
            0 => CmpKind::Eq,
            1 => CmpKind::Ne,
            2 => CmpKind::Lt,
            3 => CmpKind::Le,
            4 => CmpKind::Gt,
            5 => CmpKind::Ge,
            _ => return None,
        })
    }

    /// Map an ordering verdict to this operator's boolean result.
    fn eval(self, ord: Ordering) -> bool {
        match self {
            CmpKind::Eq => ord == Ordering::Equal,
            CmpKind::Ne => ord != Ordering::Equal,
            CmpKind::Lt => ord == Ordering::Less,
            CmpKind::Le => ord != Ordering::Greater,
            CmpKind::Gt => ord == Ordering::Greater,
            CmpKind::Ge => ord != Ordering::Less,
        }
    }

    /// The operator symbol, for EXPLAIN.
    pub fn symbol(self) -> &'static str {
        match self {
            CmpKind::Eq => "=",
            CmpKind::Ne => "<>",
            CmpKind::Lt => "<",
            CmpKind::Le => "<=",
            CmpKind::Gt => ">",
            CmpKind::Ge => ">=",
        }
    }
}

/// A compiled expression: instruction sequence + constant pool.
#[derive(Debug, Clone, PartialEq)]
pub struct ExprProgram {
    pub instrs: Vec<Instr>,
    pub consts: Vec<Value>,
    /// Maximum stack depth, proven at construction/decode time so `eval`
    /// needs no per-instruction underflow checks to be panic-free.
    max_stack: usize,
    /// Every opcode is one [`numeric_filter`] handles, and the frame fits.
    /// Derived, never encoded — a decoded program recomputes it in
    /// [`Self::new`], so the plan format is unchanged.
    fast_ok: bool,
    has_enum_inputs: bool,
}

impl ExprProgram {
    pub fn with_enum_validation(&self, e: &crate::pgtype::PgEnum) -> Result<Self> {
        let mut constants = self.consts.clone();
        let slot = u16::try_from(constants.len()).map_err(|_| Error::Unsupported("too many enum constants".into()))?;
        let mut labels = vec![Value::Text(e.name.clone())];
        labels.extend(e.labels.iter().cloned().map(Value::Text));
        constants.push(Value::List(labels));
        let mut instrs = self.instrs.clone();
        instrs.push(Instr::EnumValidate(slot));
        Self::new(instrs, constants)
    }

    /// Bound enum coercions are checked even when a scan produces no rows.
    pub fn validate_enum_inputs(&self, params: &[Value]) -> Result<()> {
        if !self.has_enum_inputs { return Ok(()); }
        for pair in self.instrs.windows(2) {
            let slot = match pair[1] { Instr::EnumRank(s) | Instr::EnumValidate(s) => s, _ => continue };
            let value = match pair[0] {
                Instr::PushParam(p) => params.get(p as usize),
                Instr::PushConst(c) => self.consts.get(c as usize),
                _ => None,
            };
            if let Some(Value::Text(label)) = value {
                let Value::List(def) = &self.consts[slot as usize] else { unreachable!("validated") };
                if !def[1..].iter().any(|v| matches!(v, Value::Text(s) if s == label)) {
                    let Value::Text(name) = &def[0] else { unreachable!("validated") };
                    return Err(Error::Bind(format!("invalid input value for enum {}: {:?}", name, label)));
                }
            }
        }
        Ok(())
    }

    /// Every column this expression reads, folded into a `& 63` bitmask
    /// (#146 K1).
    ///
    /// [`Instr::PushCol`] is the only instruction that touches the row, which
    /// is what makes this exact rather than an approximation — an expression
    /// with no `PushCol` provably depends on nothing in the row, so an action
    /// built from such expressions can be guarded on the columns it WRITES and
    /// nothing more. That is what lets a move (`SET ord = $1`) and an edit
    /// (`SET body = $1`) stop conflicting.
    pub fn cols_read_mask(&self) -> u64 {
        let mut m = 0u64;
        for i in &self.instrs {
            if let Instr::PushCol(c) = i {
                m |= 1u64 << (c & 63);
            }
        }
        m
    }
}


impl ExprProgram {
    /// Is this program the single constant `TRUE`? The shape a vacuous
    /// predicate compiles to — a comma join's `ON true`, or an ON whose every
    /// conjunct was consumed into an access path. EXPLAIN uses it to tell a
    /// genuinely unconstrained (cartesian) join step apart from one whose
    /// predicate merely stayed residual (design/DESIGN-MPEE-SOLVER.md §3).
    pub fn is_const_true(&self) -> bool {
        matches!(self.instrs[..], [Instr::PushConst(i)] if self.consts.get(i as usize) == Some(&Value::Bool(true)))
    }

    /// Build a program, verifying stack discipline and const/index bounds.
    pub fn new(instrs: Vec<Instr>, consts: Vec<Value>) -> Result<ExprProgram> {
        let max_stack = codec::validate(&instrs, &consts)?;
        // Whether the numeric filter fast path COULD take this program is a
        // property of the opcodes, so it is settled once here rather than
        // rediscovered per row. Measured why: without this, a program the fast
        // path cannot take paid for the attempt on every row — an interleaved
        // A/B over an affinity-carrying filter showed the attempt costing
        // ~14 ns/row, which turned a win into a 1.3 % loss.
        let fast_ok = max_stack <= NUM_DEPTH
            && instrs.iter().all(|i| {
                matches!(
                    i,
                    Instr::PushCol(_)
                        | Instr::PushParam(_)
                        | Instr::PushConst(_)
                        | Instr::Eq
                        | Instr::Ne
                        | Instr::Lt
                        | Instr::Le
                        | Instr::Gt
                        | Instr::Ge
                        | Instr::And
                        | Instr::Or
                        | Instr::Not
                )
            });
        let has_enum_inputs = instrs.iter().any(|i| matches!(i, Instr::EnumRank(_) | Instr::EnumValidate(_)));
        Ok(ExprProgram {
            has_enum_inputs,
            instrs,
            consts,
            max_stack,
            fast_ok,
        })
    }

    pub fn max_stack(&self) -> usize {
        self.max_stack
    }

    /// Does this program call a host-registered UDF ([`Instr::HostCall`])? The
    /// facade uses this to keep a plan that references a per-connection UDF OUT
    /// of the shared content-hashed plan registry (design/DESIGN-UDF.md): such a
    /// plan is valid only for the connection that registered the function.
    pub fn has_host_call(&self) -> bool {
        self.instrs.iter().any(|i| matches!(i, Instr::HostCall(..)))
    }

    /// Does this program call a STORED function ([`Instr::SpellCall`])? The
    /// opposite gate from [`has_host_call`](Self::has_host_call): a spell plan
    /// MAY be shared (the definition is in the file), but its execution needs
    /// the spell table in scope.
    pub fn has_spell_call(&self) -> bool {
        self.instrs.iter().any(|i| matches!(i, Instr::SpellCall(..)))
    }

    /// Evaluate against a decoded row and statement parameters. No host UDFs in
    /// scope — a program containing [`Instr::HostCall`] errors (defensive; only
    /// the executor's host-aware path reaches such a program).
    pub fn eval(&self, cols: &[Value], params: &[Value]) -> Result<Value> {
        self.eval_host(cols, params, None)
    }

    /// [`eval`](Self::eval) with a [`HostFns`] resolver for host-registered
    /// scalar UDFs (design/DESIGN-UDF.md). `None` behaves exactly like `eval`.
    pub fn eval_host(
        &self,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
    ) -> Result<Value> {
        let mut stack: Vec<Value> = Vec::with_capacity(self.max_stack);
        self.eval_with_stack_host(&mut stack, cols, params, host)
    }

    /// Hot-path variant reusing a scratch stack across rows.
    pub fn eval_with_stack(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
    ) -> Result<Value> {
        self.eval_with_stack_host(stack, cols, params, None)
    }

    /// [`eval_with_stack`](Self::eval_with_stack) with a [`HostFns`] resolver.
    /// `None` is behavior-identical to `eval_with_stack`.
    pub fn eval_with_stack_host(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
    ) -> Result<Value> {
        stack.clear();
        stack.reserve(self.max_stack);
        // A program counter rather than an iterator: jumps (CASE) need to move
        // it. `validate` proved every target is forward and in range, so this
        // terminates and never indexes out of bounds.
        let mut pc = 0usize;
        while pc < self.instrs.len() {
            let instr = self.instrs[pc];
            pc += 1;
            match instr {
                Instr::PushCol(i) => {
                    let v = cols.get(i as usize).ok_or_else(|| {
                        Error::Internal(format!("column index {i} out of row bounds"))
                    })?;
                    stack.push(v.clone());
                }
                Instr::PushParam(i) => {
                    let v = params.get(i as usize).ok_or(Error::WrongParamCount {
                        expected: i as usize + 1,
                        got: params.len(),
                    })?;
                    stack.push(v.clone());
                }
                Instr::PushConst(i) => stack.push(self.consts[i as usize].clone()),
                Instr::Eq | Instr::Ne | Instr::Lt | Instr::Le | Instr::Gt | Instr::Ge => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match a.sql_cmp(&b)? {
                        None => Value::Null,
                        Some(ord) => Value::Bool(match instr {
                            Instr::Eq => ord == Ordering::Equal,
                            Instr::Ne => ord != Ordering::Equal,
                            Instr::Lt => ord == Ordering::Less,
                            Instr::Le => ord != Ordering::Greater,
                            Instr::Gt => ord == Ordering::Greater,
                            Instr::Ge => ord != Ordering::Less,
                            _ => unreachable!(),
                        }),
                    });
                }
                Instr::Add
                | Instr::Sub
                | Instr::Mul
                | Instr::Div
                | Instr::Mod
                | Instr::DivStrict
                | Instr::ModStrict => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(arith(instr, a, b)?);
                }
                Instr::BitAnd | Instr::BitOr | Instr::Shl | Instr::Shr => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(bitwise(instr, &a, &b)?);
                }
                Instr::BitNot => {
                    let a = stack.pop().expect("validated");
                    stack.push(match bit_i64(&a)? {
                        None => Value::Null,
                        Some(x) => Value::Int(!x),
                    });
                }
                Instr::Neg => {
                    let a = stack.pop().expect("validated");
                    stack.push(match a {
                        Value::Null => Value::Null,
                        Value::Int(x) => {
                            Value::Int(x.checked_neg().ok_or(Error::ArithmeticOverflow)?)
                        }
                        Value::Float(x) => Value::Float(-x),
                        v => {
                            return Err(Error::TypeMismatch(format!(
                                "cannot negate {}",
                                v.type_name()
                            )))
                        }
                    });
                }
                Instr::And | Instr::Or => {
                    let b = truthy3(&stack.pop().expect("validated"));
                    let a = truthy3(&stack.pop().expect("validated"));
                    stack.push(match instr {
                        // Kleene 3VL
                        Instr::And => match (a, b) {
                            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                            (Some(true), Some(true)) => Value::Bool(true),
                            _ => Value::Null,
                        },
                        _ => match (a, b) {
                            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
                            (Some(false), Some(false)) => Value::Bool(false),
                            _ => Value::Null,
                        },
                    });
                }
                Instr::Not => {
                    let a = truthy3(&stack.pop().expect("validated"));
                    stack.push(match a {
                        None => Value::Null,
                        Some(x) => Value::Bool(!x),
                    });
                }
                Instr::IsNull | Instr::IsNotNull => {
                    let a = stack.pop().expect("validated");
                    let is_null = a.is_null();
                    stack.push(Value::Bool(if instr == Instr::IsNull {
                        is_null
                    } else {
                        !is_null
                    }));
                }
                Instr::IsNotDistinct | Instr::IsDistinct => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    // NULL-safe: two NULLs MATCH, one NULL does not, otherwise
                    // compare. This NEVER produces NULL — the whole point of IS,
                    // and why it is 2-valued rather than 3VL like `=`.
                    let same = match (a.is_null(), b.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => matches!(a.sql_cmp(&b)?, Some(Ordering::Equal)),
                    };
                    stack.push(Value::Bool(if instr == Instr::IsNotDistinct {
                        same
                    } else {
                        !same
                    }));
                }
                Instr::ToFloat => {
                    let a = stack.pop().expect("validated");
                    stack.push(match a {
                        Value::Null => Value::Null,
                        Value::Int(x) => Value::Float(x as f64),
                        Value::Float(x) => Value::Float(x),
                        // PostgreSQL's own promotion: in `numeric op float8`
                        // the float8 wins, so the exact side widens to it.
                        // Lossy by definition, which is why it happens only
                        // where a float is already in the expression.
                        Value::Numeric(ref n) => Value::Float(float_prefix(n.as_bytes())),
                        v => {
                            return Err(Error::TypeMismatch(format!(
                                "cannot cast {} to float",
                                v.type_name()
                            )))
                        }
                    });
                }
                Instr::Cast(t) => {
                    let a = stack.pop().expect("validated");
                    stack.push(cast_value(a, t)?);
                }
                Instr::EnumRank(slot) | Instr::EnumValidate(slot) => {
                    let Value::List(def) = &self.consts[slot as usize] else { unreachable!("validated") };
                    let Value::Text(name) = &def[0] else { unreachable!("validated") };
                    let a = stack.pop().expect("validated");
                    stack.push(match a {
                        Value::Null => Value::Null,
                        Value::Text(label) => {
                            let rank = def[1..].iter().position(|s| matches!(s, Value::Text(s) if s == &label))
                                .ok_or_else(|| Error::Bind(format!("invalid input value for enum {}: {:?}", name, label)))?;
                            if matches!(instr, Instr::EnumRank(_)) { Value::Int(rank as i64) } else { Value::Text(label) }
                        }
                        _ => return Err(Error::TypeMismatch("enum label must be text".into())),
                    });
                }
                Instr::CastPg(oid) => {
                    let a = stack.pop().expect("validated");
                    stack.push(cast_pg(&a, oid)?);
                }
                Instr::Concat => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(concat_value(a, b)?);
                }
                Instr::ConcatN(n) => {
                    let at = stack.len() - n as usize;
                    let ops = stack.split_off(at);
                    stack.push(concat_bytes_n(ops)?);
                }
                Instr::Like(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(like_match(p, s, None)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::LikeCs(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(like_match_cs(p, s, None)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::LikeEsc(pi, ei) | Instr::LikeCsEsc(pi, ei) => {
                    let ci = matches!(instr, Instr::LikeEsc(_, _));
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    // The escape const is validated to be a ONE-CHARACTER text
                    // at decode; a NULL escape can never reach here (the binder
                    // rejects it), so there is no third NULL arm to consider.
                    let esc = escape_char(&self.consts[ei as usize])?;
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(if ci {
                            like_match(p, s, Some(esc))
                        } else {
                            pg_like_match(p, s, esc)?
                        }),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::Glob(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(glob_match(p, s)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "GLOB requires text operands".into(),
                            ))
                        }
                    });
                }
                // The dyn-pattern LIKE/GLOB family: the pattern comes off the
                // STACK (a parameter, a column, any computed value) instead of
                // the const pool; matcher and NULL rules are the const forms',
                // the escape stays a const. STRICT like them, too: the binder
                // wraps any coercible operand in the same `CAST(… AS TEXT)` it
                // always has (sqlite3_value_text's conversion — what the
                // bundled-oracle sqlite applies inside likeFunc), so a
                // non-text value here is a corrupt plan or a misbound
                // parameter, never a coercion opportunity.
                Instr::LikeDyn | Instr::LikeCsDyn => {
                    let ci = matches!(instr, Instr::LikeDyn);
                    let pattern = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match (&a, &pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(if ci {
                            like_match(p, s, None)
                        } else {
                            like_match_cs(p, s, None)
                        }),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::LikeDynEsc(ei) | Instr::LikeCsDynEsc(ei) => {
                    let ci = matches!(instr, Instr::LikeDynEsc(_));
                    let pattern = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    let esc = escape_char(&self.consts[ei as usize])?;
                    stack.push(match (&a, &pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(if ci {
                            like_match(p, s, Some(esc))
                        } else {
                            pg_like_match(p, s, esc)?
                        }),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "LIKE requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::GlobDyn => {
                    let pattern = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match (&a, &pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(glob_match(p, s)),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "GLOB requires text operands".into(),
                            ))
                        }
                    });
                }
                // The pattern comes off the STACK (a parameter, a column, any
                // computed text) instead of the const pool; the operand rules,
                // the NULL rules and the matcher are the literal form's.
                Instr::RegexpDyn => {
                    let pattern = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match (&a, &pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(regexp_match(p, s)?),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "REGEXP requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::Regexp(pi) => {
                    let a = stack.pop().expect("validated");
                    let pattern = &self.consts[pi as usize];
                    stack.push(match (&a, pattern) {
                        (Value::Null, _) | (_, Value::Null) => Value::Null,
                        (Value::Text(s), Value::Text(p)) => Value::Bool(regexp_match(p, s)?),
                        _ => {
                            return Err(Error::TypeMismatch(
                                "REGEXP requires text operands".into(),
                            ))
                        }
                    });
                }
                Instr::InParam(pi) => {
                    let probe = stack.pop().expect("validated");
                    let list = params.get(pi as usize).ok_or_else(|| {
                        Error::Corrupt("IN list parameter index out of range".into())
                    })?;
                    stack.push(in_list_3vl(&probe, list, Collation::Binary)?);
                }
                Instr::InParamColl(pi, coll) => {
                    let probe = stack.pop().expect("validated");
                    // The bounds check is what keeps a hostile slot index from
                    // panicking — `validate` bounds `PushParam` but not this —
                    // so it is copied verbatim rather than shortened.
                    let list = params.get(pi as usize).ok_or_else(|| {
                        Error::Corrupt("IN list parameter index out of range".into())
                    })?;
                    stack.push(in_list_3vl(&probe, list, coll)?);
                }
                Instr::InList(n) => {
                    // The verifier proved depth >= n+1, so both splits are safe.
                    let at = stack.len() - n as usize;
                    let items: Vec<Value> = stack.split_off(at);
                    let probe = stack.pop().expect("validated");
                    // Items are SQL expressions evaluated per row, so a
                    // cross-class pair is sqlite's clean FALSE, not a refusal.
                    stack.push(in_items_3vl(
                        &probe,
                        &items,
                        CrossClass::NonMatch,
                        Collation::Binary,
                    )?);
                }
                Instr::JumpIfNotTrue(t) => {
                    // Jump on FALSE *and* on NULL: an unknown WHEN must not be
                    // taken, or `CASE WHEN <null> THEN a ELSE b END` yields a.
                    // A non-bool condition is truthy-tested like sqlite's
                    // (`truthy3`); the binder has already desugared every
                    // statically-typed WHEN, so this is the fallback for an
                    // expression whose type could not be pinned at bind time.
                    let c = stack.pop().expect("validated");
                    if truthy3(&c) != Some(true) {
                        pc = t as usize;
                    }
                }
                Instr::Jump(t) => pc = t as usize,
                Instr::JumpIfNotNull(t) => {
                    // PEEK, do not pop: on the taken path the value IS the
                    // result, so popping it would throw away what we jumped for.
                    if !stack.last().expect("validated").is_null() {
                        pc = t as usize;
                    }
                }
                Instr::Pop => {
                    stack.pop().expect("validated");
                }
                Instr::Call(f, argc) => {
                    // validate() proved depth >= argc and that argc is legal for
                    // this function, so the split and the indexing below hold.
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    stack.push(call_scalar(f, &args)?);
                }
                Instr::CallColl(f, argc, coll) => {
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    stack.push(call_scalar_collated(f, &args, coll)?);
                }
                Instr::CmpColl(kind, coll) => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    stack.push(match a.sql_cmp_collated(&b, coll)? {
                        None => Value::Null,
                        Some(ord) => Value::Bool(kind.eval(ord)),
                    });
                }
                Instr::Affinity(aff) => {
                    let v = stack.pop().expect("validated");
                    stack.push(store_affinity(aff, v));
                }
                Instr::CmpClass(kind, coll) => {
                    let b = stack.pop().expect("validated");
                    let a = stack.pop().expect("validated");
                    // NULL first, so the 3VL answer is NULL rather than the
                    // "incomparable" arm below: `sort_cmp` reports both as
                    // `None` and only one of them is a missing order.
                    stack.push(if a.is_null() || b.is_null() {
                        Value::Null
                    } else {
                        match a.sort_cmp(&b, coll) {
                            Some(ord) => Value::Bool(kind.eval(ord)),
                            // A `Bool`/`Timestamp` against another class: mpedb
                            // types with no sqlite class and therefore no place
                            // in this order. Refuse rather than invent one.
                            None => {
                                return Err(Error::TypeMismatch(format!(
                                    "cannot compare {} with {}",
                                    a.type_name(),
                                    b.type_name()
                                )))
                            }
                        }
                    });
                }
                Instr::InListColl(n, coll) => {
                    let at = stack.len() - n as usize;
                    let items: Vec<Value> = stack.split_off(at);
                    let probe = stack.pop().expect("validated");
                    stack.push(in_items_3vl(&probe, &items, CrossClass::NonMatch, coll)?);
                }
                Instr::HostCall(name_idx, argc) => {
                    // The name lives in the const pool; validate proved the index
                    // is in range, and a hostile blob whose const is not text is
                    // Corrupt, not a panic.
                    let name = match &self.consts[name_idx as usize] {
                        Value::Text(s) => s,
                        _ => {
                            return Err(Error::Corrupt(
                                "host-call name constant is not text".into(),
                            ))
                        }
                    };
                    // validate proved depth >= argc, so the split holds.
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    // A context that carries no host closures (the streaming
                    // read path, the sqlite-backed contexts, a CHECK program) is
                    // a KNOWN LIMIT, not an engine bug: refuse with a message
                    // that names it — `Unsupported`, never `Internal`, which
                    // renders as "internal error (bug in mpedb)" and tells the
                    // user to file a bug against a documented scope boundary.
                    // Worded like the host-aggregate twin in
                    // `mpedb::exec::aggregate::new_accum`.
                    let host = host.ok_or_else(|| {
                        Error::Unsupported(format!(
                            "host function {name}() is not in scope for this execution"
                        ))
                    })?;
                    stack.push(host.call(name, &args)?);
                }
                Instr::SpellCall(hash_idx, argc) => {
                    // The hash lives in the const pool; validate proved the
                    // index in range AND the const a 32-byte blob, so a
                    // hostile blob fails decode, not eval.
                    let hash: &[u8; 32] = match &self.consts[hash_idx as usize] {
                        Value::Blob(b) => b.as_slice().try_into().map_err(|_| {
                            Error::Corrupt("spell-call hash constant is not 32 bytes".into())
                        })?,
                        _ => {
                            return Err(Error::Corrupt(
                                "spell-call hash constant is not a blob".into(),
                            ))
                        }
                    };
                    let at = stack.len() - argc as usize;
                    let args: Vec<Value> = stack.split_off(at);
                    let host = host.ok_or_else(|| {
                        Error::Unsupported(
                            "stored function is not in scope for this execution".into(),
                        )
                    })?;
                    stack.push(host.call_spell(hash, &args)?);
                }
            }
        }
        Ok(stack.pop().expect("validated: exactly one result"))
    }

    /// Every column ordinal this program reads, ascending and deduplicated.
    ///
    /// [`Instr::PushCol`] is the ONLY instruction that touches the row (every
    /// other operand reaches the stack through a parameter or a constant — the
    /// eval loop has exactly one `cols.get`), so this set is COMPLETE: a caller
    /// may decode these ordinals and nothing else and still evaluate the
    /// program exactly. That is what lets a filtered aggregate fold decode two
    /// columns of a wide row instead of materializing all of it.
    pub fn read_columns(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::PushCol(c) => Some(*c),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Evaluate as a WHERE/CHECK predicate: passes only on exactly TRUE.
    pub fn eval_filter(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
    ) -> Result<bool> {
        self.eval_filter_host(stack, cols, params, None)
    }

    /// [`eval_filter`](Self::eval_filter) with a [`HostFns`] resolver for
    /// host-registered scalar UDFs (design/DESIGN-UDF.md). `None` is
    /// behavior-identical to `eval_filter`.
    pub fn eval_filter_host(
        &self,
        stack: &mut Vec<Value>,
        cols: &[Value],
        params: &[Value],
        host: Option<&dyn HostFns>,
    ) -> Result<bool> {
        // Passes only on exactly TRUE — NULL and FALSE both skip the row. A
        // non-boolean is truthy-tested the way sqlite does (`truthy3`); the
        // binder has already desugared every statically-typed boolean context,
        // so this only catches expressions whose type could not be pinned.
        // The numeric fast path first. It declines — cheaply, and without
        // touching `stack` — on anything it does not handle, so the general
        // evaluator below remains the only path that can error, call a host
        // function or see text. Declining is free of side effects, which is
        // what makes falling through safe rather than merely convenient.
        if self.fast_ok && !fastpath_off() {
            if let Some(hit) = numeric_filter(self, cols, params) {
                return Ok(hit);
            }
        }
        Ok(truthy3(&self.eval_with_stack_host(stack, cols, params, host)?) == Some(true))
    }
}

/// `MPEDB_NO_FASTPATH=1` takes the numeric filter out of the picture.
///
/// Kept, not scaffolding. An A/B that needs two builds cannot rule out the
/// build, and this one had to: a two-build comparison of the OLAP suite showed
/// the fast path doing NOTHING, while the same binary under this variable
/// showed 14.5 %. The two-build answer was wrong, and without a runtime switch
/// there was no way to find that out.
///
/// Read once through a `OnceLock` — a `getenv` per row would be the thing
/// being measured — and only reached when `fast_ok` already said yes.
/// The fast path's frame depth. A deeper program declines at construction.
const NUM_DEPTH: usize = 32;

fn fastpath_off() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var("MPEDB_NO_FASTPATH").is_ok_and(|v| v == "1"))
}

/// One slot of the numeric filter fast path: `Copy`, no heap, no drop glue.
///
/// The general evaluator's slot is `Value` — 32 bytes with four heap-owning
/// variants and a recursive one, so every push is an eleven-way `clone` call
/// and every pop runs drop glue. Measured (`mpedb-bench --expr`): cloning a
/// `Value::Null` costs 10.7 ns where copying the same 32 bytes as a plain
/// struct costs 0.58. The payload is irrelevant; the enum machinery is the
/// cost, and this type exists to not have it.
#[derive(Clone, Copy)]
enum Num {
    Null,
    I(i64),
    F(f64),
    B(bool),
}

impl Num {
    /// The one conversion in, and the gate: anything not numeric declines the
    /// whole fast path rather than guessing at its semantics.
    fn of(v: &Value) -> Option<Num> {
        match v {
            Value::Null => Some(Num::Null),
            Value::Int(i) => Some(Num::I(*i)),
            Value::Float(f) => Some(Num::F(*f)),
            Value::Bool(b) => Some(Num::B(*b)),
            _ => None,
        }
    }

    /// [`truthy3`] for this slot, arm for arm.
    fn truthy(self) -> Option<bool> {
        match self {
            Num::Null => None,
            Num::B(b) => Some(b),
            Num::I(i) => Some(i != 0),
            Num::F(f) => Some(f != 0.0),
        }
    }

    /// [`Value::sql_cmp`] for this slot, arm for arm — including the arms it
    /// REFUSES. `Bool` against a number is not a comparison mpedb answers, and
    /// answering it here would be a divergence, so it declines instead.
    fn cmp3(self, other: Num) -> Option<Option<Ordering>> {
        use Num::*;
        Some(Some(match (self, other) {
            (Null, _) | (_, Null) => return Some(None),
            (I(a), I(b)) => a.cmp(&b),
            (F(a), F(b)) => crate::value::float_total_cmp(a, b),
            (I(a), F(b)) => crate::value::int_float_cmp(a, b),
            (F(a), I(b)) => crate::value::int_float_cmp(b, a).reverse(),
            (B(a), B(b)) => a.cmp(&b),
            // Mixed bool/number: `sql_cmp` refuses it. Decline, so the general
            // path produces the error and its exact text.
            _ => return None,
        }))
    }
}

/// A filter over numeric columns, evaluated without boxing.
///
/// Returns `None` when the program or the data is outside what this handles —
/// any opcode not in the subset below, any value that is not NULL, an integer,
/// a real or a boolean, any comparison `sql_cmp` refuses. The caller then runs
/// the general evaluator, which is the only path that produces errors, calls
/// host functions, or touches text.
///
/// **Every arm here mirrors one in `eval_with_stack_host`, deliberately.** A
/// fast path that computes something subtly different is not an optimisation,
/// it is a second engine — so `prop_fast_path_matches_general` runs both over
/// random programs and rows and demands the same answer.
fn numeric_filter(prog: &ExprProgram, cols: &[Value], params: &[Value]) -> Option<bool> {
    // A fixed frame, not a `Vec`: this runs once per ROW, and one heap
    // allocation per row would cost more than the boxing it exists to avoid.
    // `max_stack` is proven at construction, so the depth is known before the
    // first instruction and a deeper program simply declines.
    let mut stack = [Num::Null; NUM_DEPTH];
    let mut sp = 0usize;
    macro_rules! push {
        ($v:expr) => {{
            if sp >= NUM_DEPTH {
                return None;
            }
            stack[sp] = $v;
            sp += 1;
        }};
    }
    macro_rules! pop {
        () => {{
            sp = sp.checked_sub(1)?;
            stack[sp]
        }};
    }
    for &instr in &prog.instrs {
        match instr {
            Instr::PushCol(i) => push!(Num::of(cols.get(i as usize)?)?),
            Instr::PushParam(i) => push!(Num::of(params.get(i as usize)?)?),
            Instr::PushConst(i) => push!(Num::of(prog.consts.get(i as usize)?)?),
            Instr::Eq | Instr::Ne | Instr::Lt | Instr::Le | Instr::Gt | Instr::Ge => {
                let b = pop!();
                let a = pop!();
                push!(match a.cmp3(b)? {
                    None => Num::Null,
                    Some(ord) => Num::B(match instr {
                        Instr::Eq => ord == Ordering::Equal,
                        Instr::Ne => ord != Ordering::Equal,
                        Instr::Lt => ord == Ordering::Less,
                        Instr::Le => ord != Ordering::Greater,
                        Instr::Gt => ord == Ordering::Greater,
                        _ => ord != Ordering::Less,
                    }),
                });
            }
            Instr::And | Instr::Or => {
                let b = pop!().truthy();
                let a = pop!().truthy();
                push!(if matches!(instr, Instr::And) {
                    match (a, b) {
                        (Some(false), _) | (_, Some(false)) => Num::B(false),
                        (Some(true), Some(true)) => Num::B(true),
                        _ => Num::Null,
                    }
                } else {
                    match (a, b) {
                        (Some(true), _) | (_, Some(true)) => Num::B(true),
                        (Some(false), Some(false)) => Num::B(false),
                        _ => Num::Null,
                    }
                });
            }
            Instr::Not => {
                let a = pop!().truthy();
                push!(match a {
                    None => Num::Null,
                    Some(x) => Num::B(!x),
                });
            }
            // Everything else — arithmetic, CASE jumps, scalar and host calls,
            // IN, LIKE, collated compares — stays with the general evaluator.
            _ => return None,
        }
    }
    // A program that leaves anything but one value is not one this may judge.
    if sp != 1 {
        return None;
    }
    Some(stack[0].truthy() == Some(true))
}

/// sqlite's `sqlite3VdbeBooleanValue`, as SQL 3VL: `None` for NULL, otherwise
/// the value is TRUE iff its REAL value is non-zero — integers directly, and
/// **everything else** through the leading-float-prefix parse ([`to_real`]),
/// which reads a blob's raw bytes as text. So `2`, `0.5`, `'3abc'`, `'1e3'` and
/// `x'31'` are TRUE while `0`, `-0.0`, `'abc'`, `'0'`, `'0x1'`, `x'30'` and
/// `x'00'` are FALSE.
///
/// The binder desugars this statically wherever a boolean context has a known
/// operand type (`Binder::coerce_bool_ctx`), which is every ordinary statement.
/// This is the runtime fallback for the residue: an expression whose type could
/// NOT be pinned at bind time, e.g. `WHERE $1 + $2` over two unconstrained
/// parameters. Before, that reached the gate as an `Int` and raised
/// `predicate evaluated to int64, expected bool`; sqlite truthy-tests it, so
/// mpedb does too — never a truth value invented for a type sqlite has no rule
/// for, because sqlite's rule covers every storage class.
/// sqlite's three-valued truth test, exposed for consumers that must judge a
/// compiled predicate's verdict OUTSIDE the executor — the ALTER-time
/// `ADD COLUMN … CHECK` scan refuses a row exactly when this says
/// `Some(false)`, the same rule the write path applies on INSERT.
pub fn truthy3(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Int(i) => Some(*i != 0),
        // A context list has no truth value in any dialect; it is param-only
        // and reaches only `IN`. Report FALSE rather than inventing one.
        Value::List(_) => Some(false),
        other => Some(to_real(other) != 0.0),
    }
}

#[cfg(test)]
mod cast_type_tests {
    use super::*;

    fn cast(v: Value, ty: ColumnType) -> Result<Value> {
        cast_typed(&v, ty)
    }

    fn cast_oid(v: Value, oid: u32) -> Result<Value> {
        cast_pg(&v, oid)
    }

    /// `uuid` and `bytea` are BOTH `Blob`, so the mpedb type cannot say which
    /// parse to run — that is why the opcode carries the PostgreSQL OID.
    /// `'6f8b9c1e-…'::uuid` was the integer 6 before it did.
    #[test]
    fn uuid_and_bytea_are_told_apart_by_the_oid() {
        let u = "6f8b9c1e-1234-5678-9abc-def012345678";
        let want: Vec<u8> = vec![
            0x6f, 0x8b, 0x9c, 0x1e, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34,
            0x56, 0x78,
        ];
        assert_eq!(cast_oid(Value::Text(u.into()), 2950).unwrap(), Value::Blob(want.clone()));
        // Dashes are optional, case is free.
        assert_eq!(
            cast_oid(Value::Text(u.replace('-', "").to_uppercase()), 2950).unwrap(),
            Value::Blob(want)
        );
        // bytea reads the SAME text completely differently — as hex bytes.
        assert_eq!(
            cast_oid(Value::Text("\\x0102ff".into()), 17).unwrap(),
            Value::Blob(vec![1, 2, 255])
        );
        assert_eq!(
            cast_oid(Value::Text("ab\\\\c".into()), 17).unwrap(),
            Value::Blob(b"ab\\c".to_vec())
        );
    }

    #[test]
    fn a_bad_uuid_or_bytea_raises_and_never_takes_a_prefix() {
        for (lit, oid, want) in [
            ("6f8b9c1e", 2950u32, "uuid"),
            ("6", 2950, "uuid"),
            ("zzzzzzzz-1234-5678-9abc-def012345678", 2950, "uuid"),
            ("\\xabc", 17, "bytea"),
            ("\\xzz", 17, "bytea"),
        ] {
            let e = cast_oid(Value::Text(lit.into()), oid).unwrap_err().to_string();
            assert!(
                e.contains("invalid input syntax for type") && e.contains(want),
                "{lit} as {want}: {e}"
            );
        }
    }

    /// The types NOT on the strict list keep sqlite's affinity answer, so this
    /// change cannot have moved anything that was already right.
    #[test]
    fn a_non_strict_type_keeps_the_affinity_answer() {
        // int8 (oid 20) still takes the leading-prefix rule.
        assert_eq!(cast_oid(Value::Text("12ab".into()), 20).unwrap(), Value::Int(12));
        // text (oid 25) renders.
        assert_eq!(cast_oid(Value::Int(5), 25).unwrap(), Value::Text("5".into()));
        // NULL casts to NULL for every type.
        assert_eq!(cast_oid(Value::Null, 2950).unwrap(), Value::Null);
    }

    /// THE bug this opcode exists for. sqlite's rule takes the leading number,
    /// so `'2020-01-02'` casts to 2020; psycopg2 renders every Python `date`
    /// parameter as `'…'::date`, which made that the ordinary write path.
    #[test]
    fn a_date_literal_casts_to_the_date_and_not_to_its_year() {
        let d = cast(Value::Text("2020-01-02".into()), ColumnType::Date).unwrap();
        assert_eq!(d, Value::Date(18_263));
        // …and the affinity cast still does the sqlite thing, untouched.
        assert_eq!(
            cast_value(Value::Text("2020-01-02".into()), Affinity::Integer).unwrap(),
            Value::Int(2020)
        );
    }

    #[test]
    fn the_four_typed_casts_parse_their_whole_input() {
        assert_eq!(
            cast(Value::Text("03:04:05".into()), ColumnType::Time).unwrap(),
            Value::Time(11_045_000_000)
        );
        assert_eq!(
            cast(Value::Text("1970-01-01 00:00:01".into()), ColumnType::Timestamp).unwrap(),
            Value::Timestamp(1_000_000)
        );
        assert_eq!(
            cast(Value::Text("1.50".into()), ColumnType::Numeric).unwrap(),
            Value::Numeric("1.50".into())
        );
        // An exponent literal expands, so one value has one stored spelling.
        assert_eq!(
            cast(Value::Text("1e3".into()), ColumnType::Numeric).unwrap(),
            Value::Numeric("1000".into())
        );
    }

    /// The message is PostgreSQL's own — that is what a client matches on.
    #[test]
    fn bad_input_raises_and_names_the_type() {
        for (lit, ty, want) in [
            ("2020", ColumnType::Date, "date"),
            ("2020-13-01", ColumnType::Date, "date"),
            ("3", ColumnType::Time, "time"),
            ("25:00:00", ColumnType::Time, "time"),
            ("2020", ColumnType::Timestamp, "timestamp"),
            ("1.2.3", ColumnType::Numeric, "numeric"),
            ("abc", ColumnType::Numeric, "numeric"),
        ] {
            let e = cast(Value::Text(lit.into()), ty).unwrap_err().to_string();
            assert!(
                e.contains("invalid input syntax for type") && e.contains(want) && e.contains(lit),
                "{lit} as {want}: {e}"
            );
        }
    }

    #[test]
    fn null_casts_to_null_and_a_value_casts_to_itself() {
        for ty in [ColumnType::Date, ColumnType::Time, ColumnType::Timestamp, ColumnType::Numeric] {
            assert_eq!(cast(Value::Null, ty).unwrap(), Value::Null, "{ty:?}");
        }
        // Identity, not a round trip through text: `1.50` keeps its scale.
        assert_eq!(
            cast(Value::Numeric("1.50".into()), ColumnType::Numeric).unwrap(),
            Value::Numeric("1.50".into())
        );
    }

    /// The date/timestamp conversions PostgreSQL defines, in both directions.
    #[test]
    fn dates_and_timestamps_convert_the_way_postgresql_converts_them() {
        // `timestamp::date` truncates toward the past, on both sides of zero.
        assert_eq!(
            cast(Value::Timestamp(18_263 * 86_400_000_000 + 3_600_000_000), ColumnType::Date)
                .unwrap(),
            Value::Date(18_263)
        );
        assert_eq!(cast(Value::Timestamp(-1), ColumnType::Date).unwrap(), Value::Date(-1));
        // `timestamp::time` is the clock part, likewise.
        assert_eq!(
            cast(Value::Timestamp(18_263 * 86_400_000_000 + 3_600_000_000), ColumnType::Time)
                .unwrap(),
            Value::Time(3_600_000_000)
        );
        // `date::timestamp` is midnight UTC.
        assert_eq!(
            cast(Value::Date(18_263), ColumnType::Timestamp).unwrap(),
            Value::Timestamp(18_263 * 86_400_000_000)
        );
        // Numbers into an exact decimal, without a float in the middle.
        assert_eq!(
            cast(Value::Int(42), ColumnType::Numeric).unwrap(),
            Value::Numeric("42".into())
        );
        assert_eq!(
            cast(Value::Float(1.5), ColumnType::Numeric).unwrap(),
            Value::Numeric("1.5".into())
        );
        // A cast PostgreSQL does not define either.
        assert!(cast(Value::Blob(vec![1]), ColumnType::Date).is_err());
        assert!(cast(Value::Date(0), ColumnType::Time).is_err());
    }
}

#[cfg(test)]
mod numeric_arith_tests {
    use super::*;

    fn n(s: &str) -> Value {
        Value::Numeric(s.to_string())
    }
    fn go(op: Instr, a: Value, b: Value) -> Result<Value> {
        arith(op, a, b)
    }

    /// The whole point: no float in the middle. `0.1 + 0.2` is `0.3` exactly,
    /// which the f64 path cannot say.
    #[test]
    fn addition_is_exact_where_a_float_would_not_be() {
        assert_eq!(go(Instr::Add, n("0.1"), n("0.2")).unwrap(), n("0.3"));
        assert_eq!(go(Instr::Sub, n("0.3"), n("0.1")).unwrap(), n("0.2"));
        // Scales align rather than truncate.
        assert_eq!(go(Instr::Add, n("1.5"), n("2.25")).unwrap(), n("3.75"));
        assert_eq!(go(Instr::Sub, n("1"), n("0.001")).unwrap(), n("0.999"));
        // Zero keeps its scale but never a sign.
        assert_eq!(go(Instr::Sub, n("1.50"), n("1.50")).unwrap(), n("0.00"));
    }

    #[test]
    fn multiplication_sums_the_scales_the_way_postgresql_does() {
        assert_eq!(go(Instr::Mul, n("1.5"), n("2.5")).unwrap(), n("3.75"));
        assert_eq!(go(Instr::Mul, n("0.01"), n("0.01")).unwrap(), n("0.0001"));
        assert_eq!(go(Instr::Mul, n("-2.5"), n("4")).unwrap(), n("-10.0"));
    }

    /// An INTEGER joins as a scale-0 decimal — `numeric + 1` stays numeric in
    /// PostgreSQL, and routing it through f64 would lose the digits.
    #[test]
    fn an_integer_operand_keeps_the_pair_exact() {
        assert_eq!(go(Instr::Add, n("0.1"), Value::Int(1)).unwrap(), n("1.1"));
        assert_eq!(go(Instr::Sub, Value::Int(1), n("0.1")).unwrap(), n("0.9"));
        assert_eq!(go(Instr::Mul, n("1.25"), Value::Int(4)).unwrap(), n("5.00"));
    }

    #[test]
    fn division_rounds_the_way_postgresql_rounds() {
        // Exact quotients come out exact (trailing zeros are the scale).
        let q = go(Instr::Div, n("1"), n("4")).unwrap();
        assert_eq!(crate::value::canonical_numeric(&format!("{q}")), "0.25");
        // A repeating quotient is carried to 16 places past the common scale.
        let q = go(Instr::Div, n("1"), n("3")).unwrap();
        let Value::Numeric(t) = q else { panic!() };
        assert!(t.starts_with("0.3333333333333333"), "{t}");
        // Rounding is away from zero on the half, on both signs.
        let q = go(Instr::Div, n("-1"), n("3")).unwrap();
        let Value::Numeric(t) = q else { panic!() };
        assert!(t.starts_with("-0.3333333333333333"), "{t}");
    }

    #[test]
    fn division_by_zero_follows_the_dialect() {
        assert_eq!(go(Instr::Div, n("1"), n("0")).unwrap(), Value::Null);
        assert!(matches!(
            go(Instr::DivStrict, n("1"), n("0")),
            Err(Error::DivisionByZero)
        ));
    }

    /// Beyond an i128 it is a NAMED refusal, never a quiet fall back to f64 —
    /// losing the digits is the wrong answer this type exists to prevent.
    #[test]
    fn too_many_digits_is_refused_by_name() {
        let huge = "9".repeat(39);
        let e = go(Instr::Add, n(&huge), n("1")).unwrap_err().to_string();
        assert!(e.contains("38 significant digits"), "{e}");
        assert!(e.contains("double precision"), "{e}");
    }

    /// A float in the expression widens the decimal, which is PostgreSQL's own
    /// rule — checked here at the IR level, where `ToFloat` does the widening
    /// the binder inserted.
    #[test]
    fn to_float_accepts_a_numeric() {
        let p = ExprProgram::new(vec![Instr::PushParam(0), Instr::ToFloat], vec![]).unwrap();
        assert_eq!(p.eval(&[], &[n("2.4")]).unwrap(), Value::Float(2.4));
    }
}
