//! The procedure IR: a compact, stack-based bytecode with locals, jumps and
//! two database ops. This is the **security boundary** (the PySpell model):
//! source text is parsed exactly once, on the host, at `define` time; what is
//! stored in — and later loaded from — the database is only this IR, and the
//! decoder re-derives every safety property (operand bounds, jump targets,
//! stack discipline, plan-arity agreement) from the bytes themselves.
//! Hostile or bit-rotted blobs yield [`Error::Corrupt`], never a panic.
//!
//! # Semantics (differ deliberately from the SQL expression IR)
//!
//! Unlike `mpedb_types::expr` (SQL three-valued logic), procedure code uses
//! ordinary Python/Rust semantics: `Value::Null` plays the role of Python's
//! `None`, `None == None` is *true*, comparisons never yield "unknown", and
//! `and`/`or`/`not` are plain short-circuit boolean logic compiled to jumps.
//! Integer overflow is an error (no wrapping, no bigints); division by zero
//! is an error. Division comes in per-frontend flavors: [`Op::TrueDiv`] and
//! [`Op::FloorDiv`]/[`Op::PyMod`] carry Python semantics, [`Op::IntDiv`] and
//! [`Op::IntRem`] carry Rust semantics; each frontend emits its own.
//!
//! SQL procedural frontends have separate `Sql*` comparison and BOOLEAN ops:
//! comparisons propagate NULL; AND/OR/NOT use three-valued logic; conditions
//! accept only BOOLEAN/NULL. `JumpIfSqlFalse` does not jump on NULL, which is
//! necessary for short-circuit AND to evaluate its right operand when the left
//! is unknown. Existing opcodes and persisted Python/Rust blobs are unchanged.
//!
//! # Budget
//!
//! Every *executed instruction* costs one unit of the instruction budget (so
//! in particular every backward jump does), and every
//! `DbQuery`/`DbExec`/`CursorOpen` additionally costs one unit of the
//! db-call budget. `CursorAdvance` and `CursorFetch` deliberately do NOT
//! cost a db call — one full-table scan would burn the whole 10k db-call
//! budget — each pull costs one unit of the separate, generous row budget
//! (default 10M rows). `CursorClose` reads nothing and costs no row unit.
//!
//! # Cursors
//!
//! `CursorOpen(p)` opens a streaming scan over `plans[p]` (which must be
//! `Query`-kind — validated statically, decode included) and pushes an
//! opaque cursor handle. `CursorAdvance` pulls ONE row and closes the
//! cursor when the scan is exhausted (the Python/Rust loop path).
//! `CursorFetch` is Oracle FETCH: exhaustion pushes false and leaves the
//! handle open. `CursorClose` releases the stream without reading further
//! rows. `CursorAttr` reads `%ISOPEN`, `%FOUND`, `%NOTFOUND`, or
//! `%ROWCOUNT`. `%FOUND` and `%NOTFOUND` are NULL until FETCH; a cursor
//! that is not open makes `%ISOPEN` false and raises INVALID_CURSOR for
//! the other three. `SqlAttr` is the implicit `SQL%` cursor: no handle,
//! `%ISOPEN` always false, and the other three NULL until a SELECT INTO
//! or DML runs. An explicit cursor does not update it. `CursorRow` pushes
//! the current row as a tuple. Handles are runtime values with
//! slot+generation identity; the runtime bounds live cursors
//! (see `interp::MAX_CURSORS`) and rejects stale handles. **v1 rule:**
//! cursors are allowed only in read-only procedures — a program containing
//! both `CursorOpen` and `DbExec` fails validation, so the question of a
//! cursor observing (or blocking on) the proc's own write session never
//! arises. Write procs keep the materializing `DbQuery`.

use crate::hash::ProcHash;
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{Error, PlanHash, Result, Value};

/// Format version, embedded in every blob and covered by the content hash.
pub const PROC_FORMAT: u16 = 1;

const MAGIC: &[u8; 4] = b"MPRC";

pub const MAX_NAME: usize = 128;
pub const MAX_LOCALS: usize = 1024;
pub const MAX_PLANS: usize = 256;
pub const MAX_CONSTS: usize = 4096;
pub const MAX_INSTRS: usize = 1 << 20;
/// Static bound on operand-stack depth, proven by [`validate`].
pub const MAX_STACK: usize = 256;
/// Most parameters one embedded SQL statement may take.
pub const MAX_DB_ARGS: usize = 64;

/// How an embedded plan is used. `Query` plans must be read-only SELECTs;
/// `Exec` plans must be DML. Enforced at define time against the recomputed
/// plan footprint, and covered by the proc's content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Query,
    Exec,
}

/// One entry of the proc's plan table: the SQL was compiled at define time,
/// published to the shared plan registry, and only its content hash is
/// embedded here. The runtime executes by hash and **never parses SQL**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRef {
    pub hash: PlanHash,
    pub kind: PlanKind,
    /// Exact number of `$n` parameters the plan takes (checked at define
    /// time against the compiled plan, re-checked structurally at decode).
    pub argc: u8,
}

/// One bytecode instruction. Stack effects are fixed per opcode (db ops pop
/// their plan's `argc`), which is what makes static validation possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push `consts[i]`.
    LoadConst(u16),
    /// Push `locals[i]`; using a local before any store is a runtime error.
    LoadLocal(u16),
    /// Pop into `locals[i]`.
    StoreLocal(u16),
    /// Discard the top of stack (expression statements).
    Pop,
    /// Duplicate the top of stack (short-circuit and/or).
    Dup,
    /// Arithmetic negation (int checked, float IEEE).
    Neg,
    /// Boolean not, via truthiness.
    Not,
    Add,
    Sub,
    Mul,
    /// Python `/`: int/int yields float; division by zero errors.
    TrueDiv,
    /// Python `//`: floor division; int overflow / zero divisor error.
    FloorDiv,
    /// Rust `/`: int/int truncates toward zero; float follows IEEE.
    IntDiv,
    /// Python `%`: result takes the divisor's sign.
    PyMod,
    /// Rust `%`: result takes the dividend's sign; float follows IEEE.
    IntRem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// SQL/PLSQL three-valued logic, distinct from Python/Rust operators.
    SqlNot,
    SqlAnd,
    SqlOr,
    SqlEq,
    SqlNe,
    SqlLt,
    SqlLe,
    SqlGt,
    SqlGe,
    SqlBool,
    /// Normalize a finite NUMBER value; integer inputs become exact decimals.
    SqlNumber,
    /// Reference-verified base-100 NUMBER rules. Separate from legacy Sql*
    /// instructions so existing stored hashes never change their meaning.
    OracleNumber,
    OracleAdd,
    OracleSub,
    OracleMul,
    OracleDiv,
    /// PL/SQL division returns zero on underflow; legacy SQL-style op stays unchanged.
    OraclePlSqlDiv,
    /// PL/SQL NUMBER special-value rules, versioned separately from finite ops.
    OraclePlSqlNumber,
    /// Local NUMBER(p,s) assignment; scale rounds half away from zero before
    /// checking precision. Precision 1..38, scale -84..127.
    OracleNumberConstraint { precision: u8, scale: i16 },
    OraclePlSqlAdd,
    OraclePlSqlSub,
    OraclePlSqlMul,
    OraclePlSqlDivide,
    OraclePlSqlNeg,
    OraclePlSqlInteger,
    OraclePlSqlLoopBound,
    OracleNeg,
    /// VARCHAR2 normalization. Low 15 bits: length (0 = unconstrained);
    /// high bit: CHAR semantics rather than BYTE. Absolute maximum 32767 bytes.
    OracleVarchar(u16),
    /// Preserve a scalar value, raising VALUE_ERROR for NULL after coercion.
    OracleNotNull,
    /// Numeric FOR bound: rounded signed 32-bit integer; NULL is an error.
    OracleLoopBound,
    OracleInteger,
    OracleIntAdd,
    OracleIntSub,
    OracleIntMul,
    OracleIntNeg,
    OracleConcat,
    /// Blank-padded comparisons when both operands are character literals.
    OracleCharEq,
    OracleCharNe,
    OracleCharLt,
    OracleCharLe,
    OracleCharGt,
    OracleCharGe,

    /// Exact decimal arithmetic; rounding-dependent results currently refuse.
    SqlAdd,
    SqlSub,
    SqlMul,
    SqlDiv,
    SqlNeg,

    /// A function reached END without returning; always raises.
    MissingReturn,
    /// Pop n scalars in source order and construct a scalar-only tuple.
    Tuple(u16),
    /// Pop a BOOLEAN/NULL and jump only on SQL FALSE (NULL falls through).
    JumpIfSqlFalse(u32),
    /// Pop a BOOLEAN/NULL and jump only on SQL TRUE (NULL falls through).
    JumpIfSqlTrue(u32),
    /// len() of a list/tuple/text/blob.
    Len,
    /// `container[index]`; negative indices wrap Python-style.
    Index,
    /// Unconditional jump to an absolute instruction index.
    Jump(u32),
    /// Pop; jump if falsey.
    JumpIfFalse(u32),
    /// Pop; jump if truthy.
    JumpIfTrue(u32),
    /// Pop `plans[p].argc` scalar args, run the SELECT plan, push the rows
    /// as a list of tuples.
    DbQuery(u16),
    /// Require exactly one query row, returning its tuple. Native PL/SQL
    /// NO_DATA_FOUND / TOO_MANY_ROWS are structured Oracle exceptions.
    OracleSelectOne,
    /// A protected statement block, with an empty evaluation stack at entry.
    /// Scope ranges also unwind frames on EXIT/CONTINUE and normal jumps.
    OracleTry { handler: u32, end: u32, handler_end: u32 },
    OracleExceptionCode,
    OracleReraise,
    OracleRaise(u16),
    /// Raise/match a lexical exception identity stored as a 32-byte constant.
    OracleRaiseUser(u16),
    /// A code bound by EXCEPTION_INIT; the old u16 opcode stays unchanged.
    OracleRaiseBound(u32),
    OracleMatchUser(u16),
    OracleSqlCode,
    OracleSqlErrm,
    /// SQLERRM with one numeric argument; NULL remains NULL.
    OracleSqlErrmCode,
    OracleRaiseApplication,
    /// Pop `plans[p].argc` scalar args, run the DML plan, push the affected
    /// row count as an int.
    DbExec(u16),
    /// Pop `plans[p].argc` scalar args, run the DML plan, and push its
    /// RETURNING rows as a list of tuples. Scalar INTO is one row; zero
    /// rows and many rows stay Oracle exceptions. Distinct from `DbExec`
    /// so an affected-count call never starts returning cells.
    DbReturning(u16),
    /// Pop `plans[p].argc` scalar args, open a streaming scan over the
    /// `Query`-kind plan `plans[p]`, push a cursor handle (module docs).
    CursorOpen(u16),
    /// Pop a cursor handle, pull one row from the engine (storing it as the
    /// cursor's current row), push `true`; or push `false` and close the
    /// cursor when the scan is exhausted. Costs one row-budget unit.
    CursorAdvance,
    /// Oracle FETCH. Same pull as `CursorAdvance`, but exhaustion pushes
    /// `false` and leaves the handle open. A later fetch on that handle
    /// pushes `false` without another pull. Costs one row-budget unit only
    /// when it pulls.
    CursorFetch,
    /// Pop an open cursor handle, including one FETCH has exhausted, and
    /// release its stream without reading the remaining rows.
    CursorClose,
    /// Pop a cursor handle or NULL and push `%ISOPEN` (0), `%FOUND` (1),
    /// `%NOTFOUND` (2), or `%ROWCOUNT` (3).
    CursorAttr(u8),
    /// Push one implicit `SQL%` attribute. Same selectors as `CursorAttr`,
    /// and no handle: `%ISOPEN` is always false, and the other three stay
    /// NULL until a SELECT INTO or DML has run.
    SqlAttr(u8),
    /// Dense `FORALL` counts read by `SQL%BULK_ROWCOUNT`. The byte is
    /// `SQL_BULK_BEGIN`, `SQL_BULK_RECORD`, `SQL_BULK_FINISH`, or
    /// `SQL_BULK_LOAD`. A missing subscript pushes NULL.
    SqlBulk(u8),
    /// Pop a query row list and push one nested table of its first column.
    /// Zero rows is an empty table. Sets implicit `SQL%ROWCOUNT` to the
    /// fetched count.
    BulkCollect,
    /// Pop a nested table and a 1-based index. NULL table raises
    /// `COLLECTION_IS_NULL`. A NULL index raises `VALUE_ERROR`. A missing
    /// element raises `SUBSCRIPT_BEYOND_COUNT`. A stored NULL stays NULL.
    NestedIndex,
    /// Pop a nested table and push `COUNT`. NULL raises `COLLECTION_IS_NULL`.
    /// An empty table pushes 0. A stored NULL element still counts.
    NestedCount,
    /// Pop a nested table and an index and push `EXISTS`. A NULL table or a
    /// NULL index pushes NULL. A missing index pushes false. A stored NULL
    /// element still exists.
    NestedExists,
    /// Pop a nested table and push `FIRST`. NULL raises `COLLECTION_IS_NULL`.
    /// An empty table pushes NULL. A stored NULL element does not move it.
    NestedFirst,
    /// Pop a nested table and push `LAST`. Same NULL rules as `FIRST`.
    NestedLast,
    /// Pop a nested table and an index and push `PRIOR`. NULL raises
    /// `COLLECTION_IS_NULL`. A NULL index, an empty table, or no predecessor
    /// pushes NULL. A stored NULL element is still a step.
    NestedPrior,
    /// Pop a nested table and an index and push `NEXT`. Same NULL rules as `PRIOR`.
    NestedNext,
    /// Pop a nested table and push `LIMIT`. NULL raises `COLLECTION_IS_NULL`.
    /// A nested table has no maximum, so an initialized table pushes NULL.
    NestedLimit,
    /// Pop a nested table and a count, append that many NULL elements, and
    /// push the new table. A NULL table raises `COLLECTION_IS_NULL`. A NULL
    /// count raises `VALUE_ERROR`. A negative count raises
    /// `SUBSCRIPT_OUTSIDE_LIMIT`.
    NestedExtend,
    /// Push the session value of package variable `consts[i]` (a text key).
    /// A variable that has never been stored is NULL.
    PkgGet(u16),
    /// Pop a scalar and store it as package variable `consts[i]`. NULL stays
    /// NULL. The store is kept when the call later raises.
    PkgSet(u16),
    /// Pop a cursor handle, push its current row as a tuple.
    CursorRow,
    /// Pop the return value and finish the procedure.
    Return,
}

const OP_LOAD_CONST: u8 = 1;
const OP_LOAD_LOCAL: u8 = 2;
const OP_STORE_LOCAL: u8 = 3;
const OP_POP: u8 = 4;
const OP_DUP: u8 = 5;
const OP_NEG: u8 = 6;
const OP_NOT: u8 = 7;
const OP_ADD: u8 = 8;
const OP_SUB: u8 = 9;
const OP_MUL: u8 = 10;
const OP_TRUE_DIV: u8 = 11;
const OP_FLOOR_DIV: u8 = 12;
const OP_INT_DIV: u8 = 13;
const OP_PY_MOD: u8 = 14;
const OP_INT_REM: u8 = 15;
const OP_EQ: u8 = 16;
const OP_NE: u8 = 17;
const OP_LT: u8 = 18;
const OP_LE: u8 = 19;
const OP_GT: u8 = 20;
const OP_GE: u8 = 21;
const OP_LEN: u8 = 22;
const OP_INDEX: u8 = 23;
const OP_JUMP: u8 = 24;
const OP_JUMP_IF_FALSE: u8 = 25;
const OP_JUMP_IF_TRUE: u8 = 26;
const OP_DB_QUERY: u8 = 27;
const OP_DB_EXEC: u8 = 28;
const OP_RETURN: u8 = 29;
// Cursor opcodes are an additive extension of format 1: old blobs decode
// unchanged, and a pre-cursor build rejects a cursor-bearing blob with
// `invalid opcode` (Corrupt) instead of misinterpreting it.
const OP_CURSOR_OPEN: u8 = 30;
const OP_CURSOR_ADVANCE: u8 = 31;
const OP_CURSOR_ROW: u8 = 32;
// Additive format-1 extension, like cursors: legacy blobs retain their exact
// semantics and older readers reject unknown instructions rather than guessing.
const OP_SQL_NOT: u8 = 33;
const OP_SQL_AND: u8 = 34;
const OP_SQL_OR: u8 = 35;
const OP_SQL_EQ: u8 = 36;
const OP_SQL_NE: u8 = 37;
const OP_SQL_LT: u8 = 38;
const OP_SQL_LE: u8 = 39;
const OP_SQL_GT: u8 = 40;
const OP_SQL_GE: u8 = 41;
const OP_SQL_BOOL: u8 = 42;
const OP_JUMP_IF_SQL_FALSE: u8 = 43;
const OP_JUMP_IF_SQL_TRUE: u8 = 44;
const OP_SQL_NUMBER: u8 = 45;
const OP_MISSING_RETURN: u8 = 46;
const OP_TUPLE: u8 = 47;
const OP_SQL_ADD: u8 = 48;
const OP_SQL_SUB: u8 = 49;
const OP_SQL_MUL: u8 = 50;
const OP_SQL_DIV: u8 = 51;
const OP_SQL_NEG: u8 = 52;
const OP_ORACLE_NUMBER: u8 = 53;
const OP_ORACLE_ADD: u8 = 54;
const OP_ORACLE_SUB: u8 = 55;
const OP_ORACLE_MUL: u8 = 56;
const OP_ORACLE_DIV: u8 = 57;
const OP_ORACLE_NEG: u8 = 58;
const OP_ORACLE_VARCHAR: u8 = 59;
const OP_ORACLE_CONCAT: u8 = 60;
const OP_ORACLE_CHAR_EQ: u8 = 61;
const OP_ORACLE_CHAR_NE: u8 = 62;
const OP_ORACLE_CHAR_LT: u8 = 63;
const OP_ORACLE_CHAR_LE: u8 = 64;
const OP_ORACLE_CHAR_GT: u8 = 65;
const OP_ORACLE_CHAR_GE: u8 = 66;
const OP_ORACLE_LOOP_BOUND: u8 = 67;
const OP_ORACLE_INTEGER: u8 = 68;
const OP_ORACLE_INT_ADD: u8 = 69;
const OP_ORACLE_INT_SUB: u8 = 70;
const OP_ORACLE_INT_MUL: u8 = 71;
const OP_ORACLE_INT_NEG: u8 = 72;
const OP_ORACLE_PLSQL_DIV: u8 = 73;
const OP_PLSQL_NUMBER: u8 = 74;
const OP_PLSQL_ADD: u8 = 75;
const OP_PLSQL_SUB: u8 = 76;
const OP_PLSQL_MUL: u8 = 77;
const OP_PLSQL_DIV: u8 = 78;
const OP_PLSQL_NEG: u8 = 79;
const OP_PLSQL_INTEGER: u8 = 80;
const OP_PLSQL_LOOP_BOUND: u8 = 81;
const OP_ORACLE_SELECT_ONE: u8 = 82;
const OP_ORACLE_TRY: u8 = 83;
const OP_ORACLE_EXCEPTION_CODE: u8 = 84;
const OP_ORACLE_RERAISE: u8 = 85;
const OP_ORACLE_RAISE: u8 = 86;
const OP_ORACLE_SQLCODE: u8 = 87;
const OP_ORACLE_SQLERRM: u8 = 88;
const OP_ORACLE_RAISE_APPLICATION: u8 = 89;
const OP_ORACLE_NUMBER_CONSTRAINT: u8 = 90;
const OP_ORACLE_NOT_NULL: u8 = 91;
const OP_ORACLE_RAISE_USER: u8 = 92;
const OP_ORACLE_MATCH_USER: u8 = 93;
const OP_ORACLE_RAISE_BOUND: u8 = 94;
const OP_ORACLE_SQLERRM_CODE: u8 = 95;
// Oracle FETCH must not reuse CursorAdvance: that opcode closes on
// exhaustion. These tags stay additive so older blobs still decode.
const OP_CURSOR_FETCH: u8 = 96;
const OP_CURSOR_CLOSE: u8 = 97;
// Explicit cursor attributes. Additive, so FETCH (96) and CLOSE (97) stay put.
const OP_CURSOR_ATTR: u8 = 98;
// Scalar RETURNING INTO. Additive: `DbExec` still pushes an affected count.
const OP_DB_RETURNING: u8 = 99;
// Implicit SQL% attributes. Additive: `CursorAttr` still pops a handle.
const OP_SQL_ATTR: u8 = 100;
// `SQL%BULK_ROWCOUNT`. Additive: `SqlAttr` still pushes one scalar attribute.
const OP_SQL_BULK: u8 = 101;
// Nested-table `BULK COLLECT`. Additive: `SqlBulk` keeps its mode byte.
const OP_BULK_COLLECT: u8 = 102;
const OP_NESTED_INDEX: u8 = 103;
// Package session variables. Additive: `NestedIndex` stays 103.
const OP_PKG_GET: u8 = 104;
const OP_PKG_SET: u8 = 105;
// Nested-table COUNT. Additive: `PkgGet`/`PkgSet` stay 104/105.
const OP_NESTED_COUNT: u8 = 106;
// Nested-table EXISTS. Additive: `NestedCount` stays 106.
const OP_NESTED_EXISTS: u8 = 107;
// Nested-table FIRST and LAST. Additive: `NestedExists` stays 107.
const OP_NESTED_FIRST: u8 = 108;
const OP_NESTED_LAST: u8 = 109;
// Nested-table PRIOR and NEXT. Additive: `NestedFirst`/`NestedLast` stay 108/109.
const OP_NESTED_PRIOR: u8 = 110;
const OP_NESTED_NEXT: u8 = 111;
// Nested-table LIMIT. Additive: `NestedPrior`/`NestedNext` stay 110/111.
const OP_NESTED_LIMIT: u8 = 112;
// Nested-table EXTEND. Additive: `NestedLimit` stays 112.
const OP_NESTED_EXTEND: u8 = 113;

/// `Op::CursorAttr` selector. Any other byte fails validation.
pub const CURSOR_ATTR_ISOPEN: u8 = 0;
pub const CURSOR_ATTR_FOUND: u8 = 1;
pub const CURSOR_ATTR_NOTFOUND: u8 = 2;
pub const CURSOR_ATTR_ROWCOUNT: u8 = 3;

/// `Op::SqlBulk` modes. Any other byte fails validation.
pub const SQL_BULK_BEGIN: u8 = 0;
pub const SQL_BULK_RECORD: u8 = 1;
pub const SQL_BULK_FINISH: u8 = 2;
pub const SQL_BULK_LOAD: u8 = 3;



/// A validated procedure: everything the runtime needs, nothing it has to
/// trust. Constructible only through [`Proc::new`] / [`Proc::decode`], both
/// of which run the full static validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub name: String,
    /// Number of parameters; they occupy `locals[0..argc]`.
    pub argc: u16,
    pub nlocals: u16,
    pub plans: Vec<PlanRef>,
    pub consts: Vec<Value>,
    pub instrs: Vec<Op>,
    /// Proven maximum operand-stack depth (validation artifact).
    max_stack: usize,
    /// Whether any instruction is `DbExec` or `DbReturning` — decides
    /// transactional routing. Recomputed from the instructions, never stored.
    has_exec: bool,
}

impl Proc {
    pub fn new(
        name: String,
        argc: u16,
        nlocals: u16,
        plans: Vec<PlanRef>,
        consts: Vec<Value>,
        instrs: Vec<Op>,
    ) -> Result<Proc> {
        check_name(&name)?;
        if (argc as usize) > nlocals as usize || nlocals as usize > MAX_LOCALS {
            return Err(Error::Corrupt("proc: bad local count".into()));
        }
        if plans.len() > MAX_PLANS || consts.len() > MAX_CONSTS {
            return Err(Error::Corrupt("proc: plan/const table too large".into()));
        }
        for p in &plans {
            if p.argc as usize > MAX_DB_ARGS {
                return Err(Error::Corrupt("proc: plan takes too many parameters".into()));
            }
        }
        let max_stack = validate(&instrs, nlocals, &consts, &plans)?;
        let has_exec = instrs
            .iter()
            .any(|op| matches!(op, Op::DbExec(_) | Op::DbReturning(_)));
        Ok(Proc {
            name,
            argc,
            nlocals,
            plans,
            consts,
            instrs,
            max_stack,
            has_exec,
        })
    }

    pub fn max_stack(&self) -> usize {
        self.max_stack
    }

    /// True if the proc contains any `DbExec` or `DbReturning`: it must run
    /// inside a single write transaction. False: it may run lock-free on
    /// read snapshots.
    pub fn has_exec(&self) -> bool {
        self.has_exec
    }

    /// Any database operation at all — query, exec, or cursor. A stored SQL
    /// FUNCTION (stage M2) must have none; this is the load-time re-check
    /// behind `create_function`'s define-time refusal.
    pub fn has_db_ops(&self) -> bool {
        self.instrs
            .iter()
            .any(|op| {
                matches!(
                    op,
                    Op::DbQuery(_) | Op::DbExec(_) | Op::DbReturning(_) | Op::CursorOpen(_)
                )
            })
            || !self.plans.is_empty()
    }

    /// Canonical serialization; the blob stored in the database and the
    /// preimage of [`Proc::hash`].
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.instrs.len() * 3);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&PROC_FORMAT.to_le_bytes());
        b.push(self.name.len() as u8);
        b.extend_from_slice(self.name.as_bytes());
        b.extend_from_slice(&self.argc.to_le_bytes());
        b.extend_from_slice(&self.nlocals.to_le_bytes());
        b.extend_from_slice(&(self.plans.len() as u16).to_le_bytes());
        for p in &self.plans {
            b.extend_from_slice(&p.hash.0);
            b.push(match p.kind {
                PlanKind::Query => 0,
                PlanKind::Exec => 1,
            });
            b.push(p.argc);
        }
        b.extend_from_slice(&(self.consts.len() as u16).to_le_bytes());
        for c in &self.consts {
            write_value(&mut b, c);
        }
        b.extend_from_slice(&(self.instrs.len() as u32).to_le_bytes());
        for &op in &self.instrs {
            encode_op(&mut b, op);
        }
        b
    }

    /// Content hash of the canonical blob (format version included in the
    /// preimage via the header). Same philosophy as plan hashes (§7): the
    /// hash *is* the identity, across every attached process.
    pub fn hash(&self) -> ProcHash {
        ProcHash(*blake3::hash(&self.encode()).as_bytes())
    }

    /// Fully re-validating decode. Every read is bounds-checked, the whole
    /// buffer must be consumed, and the decoded program passes the same
    /// static validation as freshly compiled code — a hostile blob can be
    /// rejected but can never make the runtime read out of bounds.
    pub fn decode(buf: &[u8]) -> Result<Proc> {
        let err = || Error::Corrupt("proc: truncated blob".into());
        let mut pos = 0usize;
        let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
            let end = pos
                .checked_add(n)
                .filter(|&e| e <= buf.len())
                .ok_or_else(err)?;
            let s = &buf[*pos..end];
            *pos = end;
            Ok(s)
        };
        if take(&mut pos, 4)? != MAGIC {
            return Err(Error::Corrupt("proc: bad magic".into()));
        }
        let version = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        if version != PROC_FORMAT {
            return Err(Error::Corrupt(format!(
                "proc: format version {version} (this build understands {PROC_FORMAT})"
            )));
        }
        let name_len = take(&mut pos, 1)?[0] as usize;
        if name_len > MAX_NAME {
            return Err(Error::Corrupt("proc: name too long".into()));
        }
        let name = std::str::from_utf8(take(&mut pos, name_len)?)
            .map_err(|_| Error::Corrupt("proc: name is not utf-8".into()))?
            .to_owned();
        let argc = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        let nlocals = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap());
        let nplans = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as usize;
        if nplans > MAX_PLANS {
            return Err(Error::Corrupt("proc: too many plans".into()));
        }
        let mut plans = Vec::with_capacity(nplans);
        for _ in 0..nplans {
            let hash = PlanHash(take(&mut pos, 32)?.try_into().unwrap());
            let kind = match take(&mut pos, 1)?[0] {
                0 => PlanKind::Query,
                1 => PlanKind::Exec,
                k => return Err(Error::Corrupt(format!("proc: invalid plan kind {k}"))),
            };
            let argc = take(&mut pos, 1)?[0];
            plans.push(PlanRef { hash, kind, argc });
        }
        let nconsts = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as usize;
        if nconsts > MAX_CONSTS {
            return Err(Error::Corrupt("proc: too many constants".into()));
        }
        let mut consts = Vec::with_capacity(nconsts.min(1024));
        for _ in 0..nconsts {
            consts.push(read_value(buf, &mut pos)?);
        }
        let ninstrs = u32::from_le_bytes(take(&mut pos, 4)?.try_into().unwrap()) as usize;
        if ninstrs > MAX_INSTRS {
            return Err(Error::Corrupt("proc: program too large".into()));
        }
        let mut instrs = Vec::with_capacity(ninstrs.min(4096));
        for _ in 0..ninstrs {
            instrs.push(decode_op(buf, &mut pos)?);
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("proc: trailing bytes after program".into()));
        }
        // Full revalidation: identical to the compile-time path.
        Proc::new(name, argc, nlocals, plans, consts, instrs)
    }
}

fn check_name(name: &str) -> Result<()> {
    let part_ok = |part: &str| {
        let mut chars = part.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let mut parts = name.split('.');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    // A package subprogram is `PACKAGE.ROUTINE`. Any other dot is corrupt.
    if name.len() > MAX_NAME
        || parts.next().is_some()
        || !part_ok(first)
        || second.is_some_and(|part| !part_ok(part))
    {
        return Err(Error::Corrupt(format!(
            "proc: invalid procedure name {name:?}"
        )));
    }
    Ok(())
}

fn package_key_ok(text: &str) -> bool {
    let mut parts = text.split('.');
    let Some(package) = parts.next() else {
        return false;
    };
    let Some(var) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !package.is_empty()
        && !var.is_empty()
        && text.len() <= MAX_NAME
}

fn encode_op(b: &mut Vec<u8>, op: Op) {
    let mut u16arg = |code: u8, x: u16| {
        b.push(code);
        b.extend_from_slice(&x.to_le_bytes());
    };
    match op {
        Op::LoadConst(x) => u16arg(OP_LOAD_CONST, x),
        Op::Tuple(x) => u16arg(OP_TUPLE, x),
        Op::LoadLocal(x) => u16arg(OP_LOAD_LOCAL, x),
        Op::StoreLocal(x) => u16arg(OP_STORE_LOCAL, x),
        Op::DbQuery(x) => u16arg(OP_DB_QUERY, x),
        Op::DbExec(x) => u16arg(OP_DB_EXEC, x),
        Op::DbReturning(x) => u16arg(OP_DB_RETURNING, x),
        Op::CursorOpen(x) => u16arg(OP_CURSOR_OPEN, x),
        Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfSqlFalse(t) | Op::JumpIfSqlTrue(t) => {
            b.push(match op {
                Op::Jump(_) => OP_JUMP,
                Op::JumpIfFalse(_) => OP_JUMP_IF_FALSE,
                Op::JumpIfSqlFalse(_) => OP_JUMP_IF_SQL_FALSE,
                Op::JumpIfSqlTrue(_) => OP_JUMP_IF_SQL_TRUE,
                _ => OP_JUMP_IF_TRUE,
            });
            b.extend_from_slice(&t.to_le_bytes());
        }
        Op::Pop => b.push(OP_POP),
        Op::Dup => b.push(OP_DUP),
        Op::Neg => b.push(OP_NEG),
        Op::Not => b.push(OP_NOT),
        Op::SqlNot => b.push(OP_SQL_NOT),
        Op::SqlAnd => b.push(OP_SQL_AND),
        Op::SqlOr => b.push(OP_SQL_OR),
        Op::SqlEq => b.push(OP_SQL_EQ),
        Op::SqlNe => b.push(OP_SQL_NE),
        Op::SqlLt => b.push(OP_SQL_LT),
        Op::SqlLe => b.push(OP_SQL_LE),
        Op::SqlGt => b.push(OP_SQL_GT),
        Op::SqlGe => b.push(OP_SQL_GE),
        Op::SqlBool => b.push(OP_SQL_BOOL),
        Op::SqlNumber => b.push(OP_SQL_NUMBER),
        Op::OracleNumber => b.push(OP_ORACLE_NUMBER),
        Op::OracleAdd => b.push(OP_ORACLE_ADD),
        Op::OracleSub => b.push(OP_ORACLE_SUB),
        Op::OracleMul => b.push(OP_ORACLE_MUL),
        Op::OracleDiv => b.push(OP_ORACLE_DIV),
        Op::OraclePlSqlDiv => b.push(OP_ORACLE_PLSQL_DIV),
        Op::OraclePlSqlNumber => b.push(OP_PLSQL_NUMBER),
        Op::OracleNumberConstraint { precision, scale } => {
            b.push(OP_ORACLE_NUMBER_CONSTRAINT);
            b.push(precision);
            b.extend_from_slice(&scale.to_le_bytes());
        }
        Op::OraclePlSqlAdd => b.push(OP_PLSQL_ADD),
        Op::OraclePlSqlSub => b.push(OP_PLSQL_SUB),
        Op::OraclePlSqlMul => b.push(OP_PLSQL_MUL),
        Op::OraclePlSqlDivide => b.push(OP_PLSQL_DIV),
        Op::OraclePlSqlNeg => b.push(OP_PLSQL_NEG),
        Op::OraclePlSqlInteger => b.push(OP_PLSQL_INTEGER),
        Op::OraclePlSqlLoopBound => b.push(OP_PLSQL_LOOP_BOUND),
        Op::OracleSelectOne => b.push(OP_ORACLE_SELECT_ONE),
        Op::OracleTry { handler, end, handler_end } => {
            b.push(OP_ORACLE_TRY);
            for target in [handler, end, handler_end] { b.extend_from_slice(&target.to_le_bytes()); }
        }
        Op::OracleExceptionCode => b.push(OP_ORACLE_EXCEPTION_CODE),
        Op::OracleReraise => b.push(OP_ORACLE_RERAISE),
        Op::OracleRaise(code) => u16arg(OP_ORACLE_RAISE, code),
        Op::OracleRaiseBound(code) => { b.push(OP_ORACLE_RAISE_BOUND); b.extend_from_slice(&code.to_le_bytes()); },
        Op::OracleRaiseUser(i) => u16arg(OP_ORACLE_RAISE_USER, i),
        Op::OracleMatchUser(i) => u16arg(OP_ORACLE_MATCH_USER, i),
        Op::OracleSqlCode => b.push(OP_ORACLE_SQLCODE),
        Op::OracleSqlErrm => b.push(OP_ORACLE_SQLERRM),
        Op::OracleSqlErrmCode => b.push(OP_ORACLE_SQLERRM_CODE),
        Op::OracleRaiseApplication => b.push(OP_ORACLE_RAISE_APPLICATION),
        Op::OracleNeg => b.push(OP_ORACLE_NEG),
        Op::OracleVarchar(limit) => u16arg(OP_ORACLE_VARCHAR, limit),
        Op::OracleNotNull => b.push(OP_ORACLE_NOT_NULL),
        Op::OracleLoopBound => b.push(OP_ORACLE_LOOP_BOUND),
        Op::OracleInteger => b.push(OP_ORACLE_INTEGER),
        Op::OracleIntAdd => b.push(OP_ORACLE_INT_ADD),
        Op::OracleIntSub => b.push(OP_ORACLE_INT_SUB),
        Op::OracleIntMul => b.push(OP_ORACLE_INT_MUL),
        Op::OracleIntNeg => b.push(OP_ORACLE_INT_NEG),
        Op::OracleConcat => b.push(OP_ORACLE_CONCAT),
        Op::OracleCharEq => b.push(OP_ORACLE_CHAR_EQ),
        Op::OracleCharNe => b.push(OP_ORACLE_CHAR_NE),
        Op::OracleCharLt => b.push(OP_ORACLE_CHAR_LT),
        Op::OracleCharLe => b.push(OP_ORACLE_CHAR_LE),
        Op::OracleCharGt => b.push(OP_ORACLE_CHAR_GT),
        Op::OracleCharGe => b.push(OP_ORACLE_CHAR_GE),

        Op::SqlAdd => b.push(OP_SQL_ADD),
        Op::SqlSub => b.push(OP_SQL_SUB),
        Op::SqlMul => b.push(OP_SQL_MUL),
        Op::SqlDiv => b.push(OP_SQL_DIV),
        Op::SqlNeg => b.push(OP_SQL_NEG),

        Op::MissingReturn => b.push(OP_MISSING_RETURN),

        Op::Add => b.push(OP_ADD),
        Op::Sub => b.push(OP_SUB),
        Op::Mul => b.push(OP_MUL),
        Op::TrueDiv => b.push(OP_TRUE_DIV),
        Op::FloorDiv => b.push(OP_FLOOR_DIV),
        Op::IntDiv => b.push(OP_INT_DIV),
        Op::PyMod => b.push(OP_PY_MOD),
        Op::IntRem => b.push(OP_INT_REM),
        Op::Eq => b.push(OP_EQ),
        Op::Ne => b.push(OP_NE),
        Op::Lt => b.push(OP_LT),
        Op::Le => b.push(OP_LE),
        Op::Gt => b.push(OP_GT),
        Op::Ge => b.push(OP_GE),
        Op::Len => b.push(OP_LEN),
        Op::Index => b.push(OP_INDEX),
        Op::CursorAdvance => b.push(OP_CURSOR_ADVANCE),
        Op::CursorFetch => b.push(OP_CURSOR_FETCH),
        Op::CursorClose => b.push(OP_CURSOR_CLOSE),
        Op::CursorAttr(code) => {
            b.push(OP_CURSOR_ATTR);
            b.push(code);
        }
        Op::SqlAttr(code) => {
            b.push(OP_SQL_ATTR);
            b.push(code);
        }
        Op::SqlBulk(mode) => {
            b.push(OP_SQL_BULK);
            b.push(mode);
        }
        Op::BulkCollect => b.push(OP_BULK_COLLECT),
        Op::NestedIndex => b.push(OP_NESTED_INDEX),
        Op::NestedCount => b.push(OP_NESTED_COUNT),
        Op::NestedExists => b.push(OP_NESTED_EXISTS),
        Op::NestedFirst => b.push(OP_NESTED_FIRST),
        Op::NestedLast => b.push(OP_NESTED_LAST),
        Op::NestedPrior => b.push(OP_NESTED_PRIOR),
        Op::NestedNext => b.push(OP_NESTED_NEXT),
        Op::NestedLimit => b.push(OP_NESTED_LIMIT),
        Op::NestedExtend => b.push(OP_NESTED_EXTEND),
        Op::PkgGet(x) => u16arg(OP_PKG_GET, x),
        Op::PkgSet(x) => u16arg(OP_PKG_SET, x),
        Op::CursorRow => b.push(OP_CURSOR_ROW),
        Op::Return => b.push(OP_RETURN),
    }
}

fn decode_op(buf: &[u8], pos: &mut usize) -> Result<Op> {
    let err = || Error::Corrupt("proc: truncated instruction".into());
    let op = *buf.get(*pos).ok_or_else(err)?;
    *pos += 1;
    let mut u16arg = || -> Result<u16> {
        let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
        *pos += 2;
        Ok(u16::from_le_bytes(raw.try_into().unwrap()))
    };
    Ok(match op {
        OP_LOAD_CONST => Op::LoadConst(u16arg()?),
        OP_TUPLE => Op::Tuple(u16arg()?),
        OP_LOAD_LOCAL => Op::LoadLocal(u16arg()?),
        OP_STORE_LOCAL => Op::StoreLocal(u16arg()?),
        OP_DB_QUERY => Op::DbQuery(u16arg()?),
        OP_DB_EXEC => Op::DbExec(u16arg()?),
        OP_DB_RETURNING => Op::DbReturning(u16arg()?),
        OP_CURSOR_OPEN => Op::CursorOpen(u16arg()?),
        OP_JUMP | OP_JUMP_IF_FALSE | OP_JUMP_IF_TRUE | OP_JUMP_IF_SQL_FALSE | OP_JUMP_IF_SQL_TRUE => {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            let t = u32::from_le_bytes(raw.try_into().unwrap());
            match op {
                OP_JUMP => Op::Jump(t),
                OP_JUMP_IF_FALSE => Op::JumpIfFalse(t),
                OP_JUMP_IF_SQL_FALSE => Op::JumpIfSqlFalse(t),
                OP_JUMP_IF_SQL_TRUE => Op::JumpIfSqlTrue(t),
                _ => Op::JumpIfTrue(t),
            }
        }
        OP_POP => Op::Pop,
        OP_DUP => Op::Dup,
        OP_NEG => Op::Neg,
        OP_NOT => Op::Not,
        OP_SQL_NOT => Op::SqlNot,
        OP_SQL_AND => Op::SqlAnd,
        OP_SQL_OR => Op::SqlOr,
        OP_SQL_EQ => Op::SqlEq,
        OP_SQL_NE => Op::SqlNe,
        OP_SQL_LT => Op::SqlLt,
        OP_SQL_LE => Op::SqlLe,
        OP_SQL_GT => Op::SqlGt,
        OP_SQL_GE => Op::SqlGe,
        OP_SQL_BOOL => Op::SqlBool,
        OP_SQL_NUMBER => Op::SqlNumber,
        OP_ORACLE_NUMBER => Op::OracleNumber,
        OP_ORACLE_ADD => Op::OracleAdd,
        OP_ORACLE_SUB => Op::OracleSub,
        OP_ORACLE_MUL => Op::OracleMul,
        OP_ORACLE_DIV => Op::OracleDiv,
        OP_ORACLE_PLSQL_DIV => Op::OraclePlSqlDiv,
        OP_PLSQL_NUMBER => Op::OraclePlSqlNumber,
        OP_ORACLE_NUMBER_CONSTRAINT => {
            let raw = buf.get(*pos..*pos + 3).ok_or_else(err)?;
            *pos += 3;
            Op::OracleNumberConstraint { precision: raw[0], scale: i16::from_le_bytes(raw[1..].try_into().unwrap()) }
        }
        OP_PLSQL_ADD => Op::OraclePlSqlAdd,
        OP_PLSQL_SUB => Op::OraclePlSqlSub,
        OP_PLSQL_MUL => Op::OraclePlSqlMul,
        OP_PLSQL_DIV => Op::OraclePlSqlDivide,
        OP_PLSQL_NEG => Op::OraclePlSqlNeg,
        OP_PLSQL_INTEGER => Op::OraclePlSqlInteger,
        OP_PLSQL_LOOP_BOUND => Op::OraclePlSqlLoopBound,
        OP_ORACLE_SELECT_ONE => Op::OracleSelectOne,
        OP_ORACLE_TRY => {
            let raw = buf.get(*pos..*pos + 12).ok_or_else(err)?;
            *pos += 12;
            Op::OracleTry {
                handler: u32::from_le_bytes(raw[..4].try_into().unwrap()),
                end: u32::from_le_bytes(raw[4..8].try_into().unwrap()),
                handler_end: u32::from_le_bytes(raw[8..].try_into().unwrap()),
            }
        }
        OP_ORACLE_EXCEPTION_CODE => Op::OracleExceptionCode,
        OP_ORACLE_RERAISE => Op::OracleReraise,
        OP_ORACLE_RAISE => Op::OracleRaise(u16arg()?),
        OP_ORACLE_RAISE_BOUND => {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            Op::OracleRaiseBound(u32::from_le_bytes(raw.try_into().unwrap()))
        },
        OP_ORACLE_RAISE_USER => Op::OracleRaiseUser(u16arg()?),
        OP_ORACLE_MATCH_USER => Op::OracleMatchUser(u16arg()?),
        OP_ORACLE_SQLCODE => Op::OracleSqlCode,
        OP_ORACLE_SQLERRM => Op::OracleSqlErrm,
        OP_ORACLE_SQLERRM_CODE => Op::OracleSqlErrmCode,
        OP_ORACLE_RAISE_APPLICATION => Op::OracleRaiseApplication,
        OP_ORACLE_NEG => Op::OracleNeg,
        OP_ORACLE_VARCHAR => Op::OracleVarchar(u16arg()?),
        OP_ORACLE_NOT_NULL => Op::OracleNotNull,
        OP_ORACLE_LOOP_BOUND => Op::OracleLoopBound,
        OP_ORACLE_INTEGER => Op::OracleInteger,
        OP_ORACLE_INT_ADD => Op::OracleIntAdd,
        OP_ORACLE_INT_SUB => Op::OracleIntSub,
        OP_ORACLE_INT_MUL => Op::OracleIntMul,
        OP_ORACLE_INT_NEG => Op::OracleIntNeg,
        OP_ORACLE_CONCAT => Op::OracleConcat,
        OP_ORACLE_CHAR_EQ => Op::OracleCharEq,
        OP_ORACLE_CHAR_NE => Op::OracleCharNe,
        OP_ORACLE_CHAR_LT => Op::OracleCharLt,
        OP_ORACLE_CHAR_LE => Op::OracleCharLe,
        OP_ORACLE_CHAR_GT => Op::OracleCharGt,
        OP_ORACLE_CHAR_GE => Op::OracleCharGe,

        OP_SQL_ADD => Op::SqlAdd,
        OP_SQL_SUB => Op::SqlSub,
        OP_SQL_MUL => Op::SqlMul,
        OP_SQL_DIV => Op::SqlDiv,
        OP_SQL_NEG => Op::SqlNeg,

        OP_MISSING_RETURN => Op::MissingReturn,

        OP_ADD => Op::Add,
        OP_SUB => Op::Sub,
        OP_MUL => Op::Mul,
        OP_TRUE_DIV => Op::TrueDiv,
        OP_FLOOR_DIV => Op::FloorDiv,
        OP_INT_DIV => Op::IntDiv,
        OP_PY_MOD => Op::PyMod,
        OP_INT_REM => Op::IntRem,
        OP_EQ => Op::Eq,
        OP_NE => Op::Ne,
        OP_LT => Op::Lt,
        OP_LE => Op::Le,
        OP_GT => Op::Gt,
        OP_GE => Op::Ge,
        OP_LEN => Op::Len,
        OP_INDEX => Op::Index,
        OP_CURSOR_ADVANCE => Op::CursorAdvance,
        OP_CURSOR_FETCH => Op::CursorFetch,
        OP_CURSOR_CLOSE => Op::CursorClose,
        OP_CURSOR_ATTR => {
            let code = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            Op::CursorAttr(code)
        }
        OP_SQL_ATTR => {
            let code = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            Op::SqlAttr(code)
        }
        OP_SQL_BULK => {
            let mode = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            Op::SqlBulk(mode)
        }
        OP_BULK_COLLECT => Op::BulkCollect,
        OP_NESTED_INDEX => Op::NestedIndex,
        OP_NESTED_COUNT => Op::NestedCount,
        OP_NESTED_EXISTS => Op::NestedExists,
        OP_NESTED_FIRST => Op::NestedFirst,
        OP_NESTED_LAST => Op::NestedLast,
        OP_NESTED_PRIOR => Op::NestedPrior,
        OP_NESTED_NEXT => Op::NestedNext,
        OP_NESTED_LIMIT => Op::NestedLimit,
        OP_NESTED_EXTEND => Op::NestedExtend,
        OP_PKG_GET => Op::PkgGet(u16arg()?),
        OP_PKG_SET => Op::PkgSet(u16arg()?),
        OP_CURSOR_ROW => Op::CursorRow,
        OP_RETURN => Op::Return,
        other => return Err(Error::Corrupt(format!("proc: invalid opcode {other}"))),
    })
}

/// Stack effect (pops, pushes) of an instruction; db ops consult the plan
/// table (operand bounds are checked before this is called).
fn stack_effect(op: Op, plans: &[PlanRef]) -> (usize, usize) {
    match op {
        Op::OracleTry { .. } | Op::OracleReraise | Op::OracleRaise(_) | Op::OracleRaiseUser(_) | Op::OracleRaiseBound(_) => (0, 0),
        Op::OracleExceptionCode | Op::OracleSqlCode | Op::OracleSqlErrm | Op::OracleMatchUser(_) => (0, 1),
        Op::OracleRaiseApplication => (2, 0),
        Op::LoadConst(_) | Op::LoadLocal(_) | Op::SqlAttr(_) => (0, 1),
        Op::SqlBulk(SQL_BULK_BEGIN | SQL_BULK_FINISH) => (0, 0),
        Op::SqlBulk(SQL_BULK_RECORD) => (1, 0),
        Op::SqlBulk(SQL_BULK_LOAD) => (1, 1),
        Op::SqlBulk(_) => (0, 0),
        Op::BulkCollect => (1, 1),
        Op::NestedIndex => (2, 1),
        Op::NestedCount => (1, 1),
        Op::NestedExists => (2, 1),
        Op::NestedFirst | Op::NestedLast => (1, 1),
        Op::NestedPrior | Op::NestedNext => (2, 1),
        Op::NestedLimit => (1, 1),
        Op::NestedExtend => (2, 1),
        Op::PkgGet(_) => (0, 1),
        Op::PkgSet(_) => (1, 0),
        Op::Tuple(n) => (n as usize, 1),
        Op::StoreLocal(_) | Op::Pop => (1, 0),
        Op::Dup => (1, 2),
        Op::OracleSqlErrmCode | Op::Neg | Op::Not | Op::Len | Op::SqlNot | Op::SqlBool | Op::SqlNumber | Op::SqlNeg | Op::OracleNumber | Op::OracleNeg | Op::OracleVarchar(_) | Op::OracleNotNull | Op::OracleLoopBound | Op::OracleInteger | Op::OracleIntNeg | Op::OraclePlSqlNumber | Op::OracleNumberConstraint { .. } | Op::OraclePlSqlNeg | Op::OraclePlSqlInteger | Op::OraclePlSqlLoopBound | Op::OracleSelectOne => (1, 1),
        Op::OracleIntAdd | Op::OracleIntSub | Op::OracleIntMul
        | Op::OracleConcat
        | Op::OracleCharEq
        | Op::OracleCharNe
        | Op::OracleCharLt
        | Op::OracleCharLe
        | Op::OracleCharGt
        | Op::OracleCharGe
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::TrueDiv
        | Op::FloorDiv
        | Op::IntDiv
        | Op::PyMod
        | Op::IntRem
        | Op::Eq
        | Op::Ne
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::SqlAdd
        | Op::SqlSub
        | Op::SqlMul
        | Op::SqlDiv
        | Op::OracleAdd
        | Op::OracleSub
        | Op::OracleMul
        | Op::OracleDiv
        | Op::OraclePlSqlDiv
        | Op::OraclePlSqlAdd
        | Op::OraclePlSqlSub
        | Op::OraclePlSqlMul
        | Op::OraclePlSqlDivide
        | Op::SqlAnd
        | Op::SqlOr
        | Op::SqlEq
        | Op::SqlNe
        | Op::SqlLt
        | Op::SqlLe
        | Op::SqlGt
        | Op::SqlGe
        | Op::Index => (2, 1),
        Op::Jump(_) | Op::MissingReturn => (0, 0),
        Op::JumpIfFalse(_) | Op::JumpIfTrue(_) | Op::JumpIfSqlFalse(_) | Op::JumpIfSqlTrue(_) => (1, 0),
        Op::DbQuery(p) | Op::DbExec(p) | Op::DbReturning(p) | Op::CursorOpen(p) => {
            (plans[p as usize].argc as usize, 1)
        }
        Op::CursorAdvance | Op::CursorFetch | Op::CursorRow | Op::CursorAttr(_) => (1, 1),
        Op::CursorClose | Op::Return => (1, 0),
    }
}

/// Static validation (the decode-side sibling of `expr::validate`, extended
/// with a control-flow graph walk):
///
/// 1. every operand index (local, const, plan, jump target) is in range —
///    checked for *all* instructions, reachable or not;
/// 2. a worklist pass over the CFG proves that every reachable instruction
///    is entered at one consistent stack depth, that the stack never
///    underflows or exceeds [`MAX_STACK`], and that execution can never fall
///    off the end of the program (every terminal path ends in `Return`).
///
/// Returns the proven maximum stack depth so the interpreter can
/// preallocate and skip per-instruction underflow checks.
fn validate(instrs: &[Op], nlocals: u16, consts: &[Value], plans: &[PlanRef]) -> Result<usize> {
    let nconsts = consts.len();
    if instrs.is_empty() {
        return Err(Error::Corrupt("proc: empty program".into()));
    }
    let n = instrs.len();
    // Pass 1: operand bounds, including unreachable code.
    let mut any_cursor = false;
    let mut any_exec = false;
    for (pc, &op) in instrs.iter().enumerate() {
        match op {
            Op::OracleTry { handler, end, handler_end } => {
                if end as usize <= pc || handler < end || handler_end <= handler || handler_end as usize > n {
                    return Err(Error::Corrupt("proc: invalid Oracle exception scope".into()));
                }
            }
            Op::OracleRaiseUser(i) | Op::OracleMatchUser(i) => {
                if !matches!(consts.get(i as usize), Some(Value::Blob(b)) if b.len() == 32 && b.iter().any(|v| *v != 0)) {
                    return Err(Error::Corrupt("proc: invalid user exception identity".into()));
                }
            }
            Op::OracleRaiseBound(code) if !(1..=1_000_000).contains(&code) => {
                return Err(Error::Corrupt("proc: invalid EXCEPTION_INIT code".into()));
            }
            Op::OracleRaise(code) if !matches!(code, 1 | 1403 | 1422 | 1476 | 6502) => {
                return Err(Error::Corrupt("proc: unknown named Oracle exception".into()));
            }
            Op::OracleNumberConstraint { precision, scale }
                if !(1..=38).contains(&precision) || !(-84..=127).contains(&scale) => {
                return Err(Error::Corrupt("proc: invalid Oracle NUMBER constraint".into()));
            }
            Op::OracleVarchar(0x8000) => {
                return Err(Error::Corrupt("proc: zero CHAR length constraint".into()));
            }
            Op::Tuple(n) if n as usize > MAX_STACK => {
                return Err(Error::Corrupt("proc: tuple arity exceeds stack bound".into()));
            }
            Op::LoadLocal(i) | Op::StoreLocal(i) => {
                if i >= nlocals {
                    return Err(Error::Corrupt("proc: local index out of range".into()));
                }
            }
            Op::LoadConst(i) => {
                if i as usize >= nconsts {
                    return Err(Error::Corrupt("proc: const index out of range".into()));
                }
            }
            Op::PkgGet(i) | Op::PkgSet(i) => match consts.get(i as usize) {
                Some(Value::Text(text)) if package_key_ok(text) => {}
                _ => {
                    return Err(Error::Corrupt(
                        "proc: package variable key is invalid".into(),
                    ))
                }
            },
            Op::DbQuery(p) | Op::DbExec(p) | Op::DbReturning(p) | Op::CursorOpen(p)
                if p as usize >= plans.len() =>
            {
                return Err(Error::Corrupt("proc: plan index out of range".into()));
            }
            // A cursor scans; only Query-kind (read-only SELECT) plans scan.
            Op::CursorOpen(p) if plans[p as usize].kind != PlanKind::Query => {
                return Err(Error::Corrupt(
                    "proc: cursor over a non-Query plan".into(),
                ));
            }
            Op::DbReturning(p) if plans[p as usize].kind != PlanKind::Exec => {
                return Err(Error::Corrupt(
                    "proc: RETURNING over a non-Exec plan".into(),
                ));
            }
            Op::CursorAttr(code) | Op::SqlAttr(code) if code > CURSOR_ATTR_ROWCOUNT => {
                return Err(Error::Corrupt("proc: invalid cursor attribute".into()));
            }
            Op::SqlBulk(mode) if mode > SQL_BULK_LOAD => {
                return Err(Error::Corrupt(
                    "proc: invalid SQL%BULK_ROWCOUNT operation".into(),
                ));
            }
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfSqlFalse(t) | Op::JumpIfSqlTrue(t) if t as usize >= n => {
                return Err(Error::Corrupt("proc: jump target out of range".into()));
            }
            _ => {}
        }
        match op {
            Op::CursorOpen(_) => any_cursor = true,
            Op::DbExec(_) | Op::DbReturning(_) => any_exec = true,
            _ => {}
        }
    }
    // v1 cursor rule (module docs): cursors only in read-only procedures.
    // Checked structurally so hostile blobs cannot smuggle a cursor into a
    // write session either.
    if any_cursor && any_exec {
        return Err(Error::Corrupt(
            "proc: cursors are allowed only in read-only procedures \
             (no DbExec together with CursorOpen)"
                .into(),
        ));
    }
    // Pass 2: CFG walk with per-entry stack depths.
    let mut entry: Vec<Option<usize>> = vec![None; n];
    let mut work: Vec<(usize, usize)> = vec![(0, 0)];
    let mut max = 0usize;
    let visit = |pc: usize,
                     depth: usize,
                     entry: &mut Vec<Option<usize>>,
                     work: &mut Vec<(usize, usize)>|
     -> Result<()> {
        if pc >= n {
            return Err(Error::Corrupt(
                "proc: control flow falls off the end of the program".into(),
            ));
        }
        match entry[pc] {
            Some(d) if d == depth => {}
            Some(_) => {
                return Err(Error::Corrupt(
                    "proc: inconsistent stack depth at join point".into(),
                ))
            }
            None => {
                entry[pc] = Some(depth);
                work.push((pc, depth));
            }
        }
        Ok(())
    };
    // Seed pc 0.
    entry[0] = Some(0);
    while let Some((pc, depth)) = work.pop() {
        let op = instrs[pc];
        let (pops, pushes) = stack_effect(op, plans);
        if depth < pops {
            return Err(Error::Corrupt("proc: stack underflow".into()));
        }
        let after = depth - pops + pushes;
        if after > MAX_STACK {
            return Err(Error::Corrupt("proc: stack too deep".into()));
        }
        max = max.max(after);
        match op {
            Op::Return | Op::MissingReturn | Op::OracleRaise(_) | Op::OracleRaiseUser(_) | Op::OracleRaiseBound(_) | Op::OracleReraise | Op::OracleRaiseApplication => {}
            Op::OracleTry { handler, .. } => {
                if depth != 0 { return Err(Error::Corrupt("proc: Oracle exception scope requires an empty stack".into())); }
                visit(handler as usize, 0, &mut entry, &mut work)?;
                visit(pc + 1, 0, &mut entry, &mut work)?;
            }
            Op::Jump(t) => visit(t as usize, after, &mut entry, &mut work)?,
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) | Op::JumpIfSqlFalse(t) | Op::JumpIfSqlTrue(t) => {
                visit(t as usize, after, &mut entry, &mut work)?;
                visit(pc + 1, after, &mut entry, &mut work)?;
            }
            _ => visit(pc + 1, after, &mut entry, &mut work)?,
        }
    }
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(kind: PlanKind, argc: u8) -> PlanRef {
        PlanRef {
            hash: PlanHash([7u8; 32]),
            kind,
            argc,
        }
    }

    fn mk(instrs: Vec<Op>, consts: Vec<Value>, plans: Vec<PlanRef>) -> Result<Proc> {
        Proc::new("t".into(), 1, 2, plans, consts, instrs)
    }

    #[test]
    fn roundtrip_and_hash_stability() {
        let p = Proc::new(
            "transfer".into(),
            3,
            5,
            vec![plan(PlanKind::Query, 1), plan(PlanKind::Exec, 2)],
            vec![Value::Int(0), Value::Text("x".into()), Value::Null],
            vec![
                Op::LoadLocal(0),
                Op::DbQuery(0),
                Op::StoreLocal(3),
                Op::LoadLocal(1),
                Op::LoadLocal(2),
                Op::DbExec(1),
                Op::Pop,
                Op::LoadConst(2),
                Op::Return,
            ],
        )
        .unwrap();
        let blob = p.encode();
        let q = Proc::decode(&blob).unwrap();
        assert_eq!(p, q);
        assert_eq!(p.hash(), q.hash());
        assert!(p.has_exec());
        // hash covers the plan kind byte
        let mut r = q.clone();
        r.plans[0].kind = PlanKind::Exec;
        assert_ne!(p.hash(), r.hash());
    }

    #[test]
    fn decode_fuzz_truncation_and_bitflips() {
        let p = Proc::new(
            "f".into(),
            1,
            3,
            vec![plan(PlanKind::Query, 2)],
            vec![Value::Int(41), Value::Float(2.5), Value::Text("s".into())],
            vec![
                Op::LoadLocal(0),
                Op::LoadConst(0),
                Op::Add,
                Op::StoreLocal(1),
                Op::LoadLocal(1),
                Op::LoadConst(1),
                Op::Lt,
                Op::JumpIfFalse(12),
                Op::LoadLocal(1),
                Op::LoadConst(2),
                Op::DbQuery(0),
                Op::Return,
                Op::LoadConst(0),
                Op::Return,
            ],
        )
        .unwrap();
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        // Truncation at every offset: error, never panic.
        for cut in 0..blob.len() {
            assert!(Proc::decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        // Every single-bit flip: decode may succeed only if it still
        // validates; it must never panic. (Exhaustive: 8 * len decodes.)
        for byte in 0..blob.len() {
            for bit in 0..8 {
                let mut evil = blob.clone();
                evil[byte] ^= 1 << bit;
                let _ = Proc::decode(&evil);
            }
        }
        // Trailing garbage is rejected.
        let mut long = blob.clone();
        long.push(0);
        assert!(Proc::decode(&long).is_err());
    }

    #[test]
    fn validation_rejects_bad_programs() {
        // stack underflow
        assert!(mk(vec![Op::Add], vec![], vec![]).is_err());
        // empty
        assert!(mk(vec![], vec![], vec![]).is_err());
        // falls off the end
        assert!(mk(vec![Op::LoadConst(0)], vec![Value::Null], vec![]).is_err());
        // jump out of range
        assert!(mk(
            vec![Op::Jump(9), Op::LoadConst(0), Op::Return],
            vec![Value::Null],
            vec![]
        )
        .is_err());
        // conditional jump at the very end falls through past the program
        assert!(mk(
            vec![Op::LoadConst(0), Op::JumpIfFalse(0)],
            vec![Value::Bool(true)],
            vec![]
        )
        .is_err());
        // inconsistent depth at join: one path pushes 1, the other 2
        assert!(mk(
            vec![
                Op::LoadConst(0),   // 0: cond
                Op::JumpIfFalse(4), // 1
                Op::LoadConst(0),   // 2
                Op::LoadConst(0),   // 3  (depth 2 into pc 4)
                Op::LoadConst(0),   // 4  (depth 0 or 2)
                Op::Return,
            ],
            vec![Value::Bool(true)],
            vec![]
        )
        .is_err());
        // local/const/plan out of range
        assert!(mk(vec![Op::LoadLocal(99), Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::LoadConst(0), Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::DbQuery(0), Op::Return], vec![], vec![]).is_err());
        // Return with empty stack
        assert!(mk(vec![Op::Return], vec![], vec![]).is_err());
        // bad name
        assert!(Proc::new("9x".into(), 0, 0, vec![], vec![], vec![
            Op::LoadConst(0),
            Op::Return
        ])
        .is_err());
        // argc > nlocals
        assert!(Proc::new("f".into(), 3, 1, vec![], vec![Value::Null], vec![
            Op::LoadConst(0),
            Op::Return
        ])
        .is_err());
    }

    #[test]
    fn db_ops_pop_their_plan_arity() {
        // DbQuery with argc 2 but only one value on the stack: underflow.
        assert!(mk(
            vec![Op::LoadConst(0), Op::DbQuery(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .is_err());
        // Correct arity validates.
        let p = mk(
            vec![
                Op::LoadConst(0),
                Op::LoadConst(0),
                Op::DbQuery(0),
                Op::Return,
            ],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .unwrap();
        assert!(!p.has_exec());
        assert_eq!(p.max_stack(), 2);
    }

    #[test]
    fn oracle_exact_fetch_round_trips_with_a_distinct_additive_opcode() {
        let p = Proc::new("fetch".into(), 0, 0, vec![plan(PlanKind::Query, 0)], vec![],
            vec![Op::DbQuery(0), Op::OracleSelectOne, Op::Return]).unwrap();
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        for i in 0..blob.len() { assert!(Proc::decode(&blob[..i]).is_err()); }
        let mut offset = 0;
        assert_eq!(decode_op(&[82], &mut offset).unwrap(), Op::OracleSelectOne);
        assert!(Proc::new("bad".into(), 0, 0, vec![], vec![],
            vec![Op::OracleSelectOne, Op::Return]).is_err());
    }

    #[test]
    fn oracle_exception_scopes_round_trip_and_reject_forged_stack_or_ranges() {
        let s = crate::plsql::compile("CREATE FUNCTION handled RETURN NUMBER IS BEGIN RAISE ZERO_DIVIDE; EXCEPTION WHEN ZERO_DIVIDE THEN RETURN 1; END;").unwrap();
        let p = Proc::new(s.name, s.argc, s.nlocals, vec![], s.consts, s.instrs).unwrap();
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        for i in 0..blob.len() { assert!(Proc::decode(&blob[..i]).is_err()); }
        assert!(Proc::new("stack".into(), 0, 0, vec![], vec![Value::Int(1)], vec![
            Op::LoadConst(0), Op::OracleTry { handler: 4, end: 3, handler_end: 6 },
            Op::OracleRaise(1476), Op::Jump(6), Op::LoadConst(0), Op::Return,
            Op::LoadConst(0), Op::Return,
        ]).unwrap_err().to_string().contains("empty stack"));
        for op in [Op::OracleRaise(0), Op::OracleRaise(99),
            Op::OracleTry { handler: 1, end: 2, handler_end: 3 },
            Op::OracleTry { handler: 1, end: 1, handler_end: u32::MAX }] {
            assert!(Proc::new("range".into(), 0, 0, vec![], vec![], vec![op]).is_err());
        }
    }

    #[test]
    fn oracle_number_constraint_codec_bounds_and_stack_are_validated() {
        let op = Op::OracleNumberConstraint { precision: 4, scale: -2 };
        let p = Proc::new("constraint".into(), 0, 0, vec![], vec![Value::Int(12345)],
            vec![Op::LoadConst(0), op, Op::Return]).unwrap();
        let bytes = p.encode();
        assert_eq!(Proc::decode(&bytes).unwrap(), p);
        for i in 0..bytes.len() { assert!(Proc::decode(&bytes[..i]).is_err()); }
        let encoded = [90, 4, 254, 255];
        assert_eq!(decode_op(&encoded, &mut 0).unwrap(), op);
        for i in 0..encoded.len() { assert!(decode_op(&encoded[..i], &mut 0).is_err()); }
        for (precision, scale) in [(0, 0), (39, 0), (4, -85), (4, 128)] {
            assert!(Proc::new("bad".into(), 0, 0, vec![], vec![Value::Int(1)],
                vec![Op::LoadConst(0), Op::OracleNumberConstraint { precision, scale }, Op::Return]).is_err());
        }
        assert!(Proc::new("stack".into(), 0, 0, vec![], vec![], vec![op, Op::Return]).is_err());
    }

    #[test]
    fn oracle_reporting_and_application_raise_keep_additive_wire_tags() {
        for (op, tag) in [(Op::OracleSqlCode, 87), (Op::OracleSqlErrm, 88), (Op::OracleRaiseApplication, 89)] {
            let mut bytes = Vec::new(); encode_op(&mut bytes, op);
            assert_eq!(bytes, vec![tag]);
            assert_eq!(decode_op(&bytes, &mut 0).unwrap(), op);
        }
        let s = crate::plsql::compile("CREATE FUNCTION app_error RETURN VARCHAR2 IS BEGIN RAISE_APPLICATION_ERROR(-20001,'ø'); EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; END;").unwrap();
        let p = Proc::new(s.name, s.argc, s.nlocals, vec![], s.consts, s.instrs).unwrap();
        assert_eq!(Proc::decode(&p.encode()).unwrap(), p);
        assert!(Proc::new("underflow".into(), 0, 0, vec![], vec![], vec![Op::OracleRaiseApplication]).is_err());
    }

    /// Cursor ops: encode/decode roundtrip, full decode fuzz (truncation at
    /// every offset + every single-bit flip — same bar as the base opcode
    /// set), and the static validation rules specific to cursors.
    #[test]
    fn cursor_ops_roundtrip_and_decode_fuzz() {
        // A realistic streaming loop: open, advance, read, accumulate.
        let p = Proc::new(
            "scan_sum".into(),
            1,
            4,
            vec![plan(PlanKind::Query, 1)],
            vec![Value::Int(0), Value::Int(1)],
            vec![
                Op::LoadConst(0),    // 0: acc = 0
                Op::StoreLocal(1),   // 1
                Op::LoadLocal(0),    // 2: arg for the plan
                Op::CursorOpen(0),   // 3: c = cursor
                Op::StoreLocal(2),   // 4
                Op::LoadLocal(2),    // 5: loop head
                Op::CursorAdvance,   // 6
                Op::JumpIfFalse(16), // 7: -> end
                Op::LoadLocal(2),    // 8
                Op::CursorRow,       // 9
                Op::LoadConst(1),    // 10
                Op::Index,           // 11: row[1]
                Op::LoadLocal(1),    // 12
                Op::Add,             // 13
                Op::StoreLocal(1),   // 14
                Op::Jump(5),         // 15: the backward jump
                Op::LoadLocal(1),    // 16: end: return acc
                Op::Return,          // 17
            ],
        )
        .unwrap();
        assert!(!p.has_exec(), "cursors do not make a proc a writer");
        let blob = p.encode();
        assert_eq!(Proc::decode(&blob).unwrap(), p);
        for cut in 0..blob.len() {
            assert!(Proc::decode(&blob[..cut]).is_err(), "cut at {cut}");
        }
        for byte in 0..blob.len() {
            for bit in 0..8 {
                let mut evil = blob.clone();
                evil[byte] ^= 1 << bit;
                let _ = Proc::decode(&evil); // may reject, must not panic
            }
        }
    }

    #[test]
    fn cursor_validation_rules() {
        // CursorOpen plan index out of range.
        assert!(mk(vec![Op::CursorOpen(0), Op::Return], vec![], vec![]).is_err());
        // CursorOpen over an Exec-kind plan: rejected even though the index
        // is in range (a cursor is a scan; DML does not scan).
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorOpen(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Exec, 1)],
        )
        .is_err());
        // CursorOpen pops its plan's arity (underflow with argc 2, depth 1).
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorOpen(0), Op::Return],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 2)],
        )
        .is_err());
        // v1 rule: CursorOpen + DbExec in one proc is structurally invalid,
        // even when the DbExec is unreachable.
        assert!(mk(
            vec![
                Op::CursorOpen(0),  // query plan, argc 0
                Op::Return,
                Op::LoadConst(0),   // unreachable
                Op::DbExec(1),
                Op::Return,
            ],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 0), plan(PlanKind::Exec, 1)],
        )
        .is_err());
        // Advance/Fetch/Row/Close on an empty stack underflow.
        assert!(mk(vec![Op::CursorAdvance, Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::CursorFetch, Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::CursorClose, Op::Return], vec![], vec![]).is_err());
        assert!(mk(vec![Op::CursorRow, Op::Return], vec![], vec![]).is_err());
        let mut fetch = Vec::new();
        encode_op(&mut fetch, Op::CursorFetch);
        assert_eq!(fetch, vec![96]);
        let mut close = Vec::new();
        encode_op(&mut close, Op::CursorClose);
        assert_eq!(close, vec![97]);
        let mut attr = Vec::new();
        encode_op(&mut attr, Op::CursorAttr(CURSOR_ATTR_FOUND));
        assert_eq!(attr, vec![98, CURSOR_ATTR_FOUND]);
        assert!(decode_op(&attr[..1], &mut 0).is_err());
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorAttr(4), Op::Return],
            vec![Value::Null],
            vec![],
        )
        .is_err());
        assert!(mk(
            vec![Op::LoadConst(0), Op::CursorAttr(CURSOR_ATTR_ISOPEN), Op::Return],
            vec![Value::Null],
            vec![],
        )
        .is_ok());
        let mut implicit = Vec::new();
        encode_op(&mut implicit, Op::SqlAttr(CURSOR_ATTR_ROWCOUNT));
        assert_eq!(implicit, vec![100, CURSOR_ATTR_ROWCOUNT]);
        assert!(decode_op(&implicit[..1], &mut 0).is_err());
        assert!(mk(vec![Op::SqlAttr(4), Op::Return], vec![], vec![]).is_err());
        let implicit_proc = mk(
            vec![Op::SqlAttr(CURSOR_ATTR_ISOPEN), Op::Return],
            vec![],
            vec![],
        )
        .unwrap();
        assert!(!implicit_proc.has_exec());
        assert!(!implicit_proc.has_db_ops());
        assert_eq!(
            Proc::decode(&implicit_proc.encode()).unwrap().instrs,
            implicit_proc.instrs
        );
        let mut bulk = Vec::new();
        encode_op(&mut bulk, Op::SqlBulk(SQL_BULK_LOAD));
        assert_eq!(bulk, vec![101, SQL_BULK_LOAD]);
        assert!(decode_op(&bulk[..1], &mut 0).is_err());
        assert!(mk(vec![Op::SqlBulk(4), Op::Return], vec![], vec![]).is_err());
        let bulk_proc = mk(
            vec![Op::LoadConst(0), Op::SqlBulk(SQL_BULK_LOAD), Op::Return],
            vec![Value::Null],
            vec![],
        )
        .unwrap();
        assert!(!bulk_proc.has_exec());
        assert_eq!(
            Proc::decode(&bulk_proc.encode()).unwrap().instrs,
            bulk_proc.instrs
        );
        let mut collect = Vec::new();
        encode_op(&mut collect, Op::BulkCollect);
        assert_eq!(collect, vec![102]);
        encode_op(&mut collect, Op::NestedIndex);
        assert_eq!(collect, vec![102, 103]);
        encode_op(&mut collect, Op::NestedCount);
        assert_eq!(collect, vec![102, 103, 106]);
        encode_op(&mut collect, Op::NestedExists);
        assert_eq!(collect, vec![102, 103, 106, 107]);
        encode_op(&mut collect, Op::NestedFirst);
        encode_op(&mut collect, Op::NestedLast);
        encode_op(&mut collect, Op::NestedPrior);
        encode_op(&mut collect, Op::NestedNext);
        encode_op(&mut collect, Op::NestedLimit);
        encode_op(&mut collect, Op::NestedExtend);
        assert_eq!(
            collect,
            vec![102, 103, 106, 107, 108, 109, 110, 111, 112, 113]
        );
        let collect_proc = mk(
            vec![
                Op::LoadConst(0),
                Op::BulkCollect,
                Op::LoadConst(0),
                Op::NestedIndex,
                Op::Return,
            ],
            vec![Value::Null],
            vec![],
        )
        .unwrap();
        assert_eq!(
            Proc::decode(&collect_proc.encode()).unwrap().instrs,
            collect_proc.instrs
        );
        let mut pkg = Vec::new();
        encode_op(&mut pkg, Op::PkgGet(0));
        assert_eq!(pkg, vec![104, 0, 0]);
        encode_op(&mut pkg, Op::PkgSet(1));
        assert_eq!(pkg, vec![104, 0, 0, 105, 1, 0]);
        assert!(decode_op(&pkg[..1], &mut 0).is_err());
        assert!(mk(vec![Op::PkgGet(0), Op::Return], vec![Value::Null], vec![]).is_err());
        let pkg_proc = mk(
            vec![Op::LoadConst(0), Op::PkgSet(0), Op::PkgGet(0), Op::Return],
            vec![Value::Text("LEDGER.N".into())],
            vec![],
        )
        .unwrap();
        assert!(!pkg_proc.has_exec());
        assert_eq!(Proc::decode(&pkg_proc.encode()).unwrap().instrs, pkg_proc.instrs);
        let fetched = mk(
            vec![
                Op::CursorOpen(0),
                Op::CursorFetch,
                Op::Pop,
                Op::CursorOpen(0),
                Op::CursorClose,
                Op::LoadConst(0),
                Op::Return,
            ],
            vec![Value::Int(1)],
            vec![plan(PlanKind::Query, 0)],
        )
        .unwrap();
        assert_eq!(Proc::decode(&fetched.encode()).unwrap().instrs, fetched.instrs);
        // The happy shape validates.
        let p = mk(
            vec![
                Op::CursorOpen(0),
                Op::CursorAdvance,
                Op::Return,
            ],
            vec![],
            vec![plan(PlanKind::Query, 0)],
        )
        .unwrap();
        assert_eq!(p.max_stack(), 1);
    }

    #[test]
    fn oracle_not_null_has_a_checked_stack_shape_and_roundtrips() {
        assert!(mk(vec![Op::OracleNotNull, Op::Return],vec![],vec![]).is_err());
        let p=mk(vec![Op::LoadConst(0),Op::OracleNotNull,Op::Return],vec![Value::Null],vec![]).unwrap();
        let loaded=Proc::decode(&p.encode()).unwrap();
        assert_eq!(loaded.instrs,p.instrs);
        assert_eq!(loaded.hash(),p.hash());
        assert_eq!(loaded.max_stack(),1);
    }

    #[test]
    fn version_and_magic_are_checked() {
        let p = mk(
            vec![Op::LoadConst(0), Op::Return],
            vec![Value::Int(1)],
            vec![],
        )
        .unwrap();
        let mut blob = p.encode();
        blob[0] = b'X'; // magic
        assert!(matches!(Proc::decode(&blob), Err(Error::Corrupt(_))));
        let mut blob = p.encode();
        blob[4] = 0xFF; // version
        assert!(matches!(Proc::decode(&blob), Err(Error::Corrupt(_))));
    }
}

#[cfg(test)]
mod user_exception_tests {
    use super::*;
    #[test]
    fn identities_are_validated_even_on_unreachable_instructions() {
        for value in [
            Value::Null,
            Value::Int(1),
            Value::Blob(vec![]),
            Value::Blob(vec![0; 32]),
            Value::Blob(vec![1; 31]),
            Value::Blob(vec![1; 33]),
        ] {
            for op in [Op::OracleRaiseUser(0), Op::OracleMatchUser(0)] {
                assert!(Proc::new(
                    "bad".into(),
                    0,
                    0,
                    vec![],
                    vec![value.clone()],
                    vec![Op::LoadConst(0), Op::Return, op]
                )
                .is_err());
            }
        }
        for op in [Op::OracleRaiseUser(1), Op::OracleMatchUser(1)] {
            assert!(Proc::new(
                "bad".into(),
                0,
                0,
                vec![],
                vec![Value::Blob(vec![1; 32])],
                vec![Op::LoadConst(0), Op::Return, op]
            )
            .is_err());
        }
        let p = Proc::new(
            "valid".into(),
            0,
            0,
            vec![],
            vec![Value::Blob(vec![1; 32])],
            vec![Op::OracleRaiseUser(0)],
        )
        .unwrap();
        assert_eq!(Proc::decode(&p.encode()).unwrap(), p);
        for i in 0..p.encode().len() {
            assert!(Proc::decode(&p.encode()[..i]).is_err());
        }
    }
}

#[cfg(test)]
mod bound_exception_tests {
    use super::*;
    #[test]
    fn bound_codes_keep_wide_operands_and_reject_invalid_ir() {
        for code in [1, 1403, 65535, 65536, 1000000] {
            let p = Proc::new(
                "bound".into(),
                0,
                0,
                vec![],
                vec![],
                vec![Op::OracleRaiseBound(code)],
            )
            .unwrap();
            assert_eq!(Proc::decode(&p.encode()).unwrap(), p);
            for len in 0..5 {
                assert!(decode_op(&[94, 0, 0, 1, 0][..len], &mut 0).is_err());
            }
        }
        for code in [0, 1000001, u32::MAX] {
            assert!(Proc::new(
                "bad".into(),
                0,
                0,
                vec![],
                vec![],
                vec![Op::OracleRaiseBound(code)]
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod sqlerrm_argument_tests {
    use super::*;
    #[test]
    fn argument_opcode_checks_stack_and_round_trips() {
        assert!(Proc::new(
            "bad".into(),
            0,
            0,
            vec![],
            vec![],
            vec![Op::OracleSqlErrmCode, Op::Return]
        )
        .is_err());
        let p = Proc::new(
            "message".into(),
            0,
            0,
            vec![],
            vec![Value::Null],
            vec![Op::LoadConst(0), Op::OracleSqlErrmCode, Op::Return],
        )
        .unwrap();
        assert_eq!(Proc::decode(&p.encode()).unwrap(), p);
        assert_eq!(decode_op(&[95], &mut 0).unwrap(), Op::OracleSqlErrmCode);
        for len in 0..p.encode().len() {
            assert!(Proc::decode(&p.encode()[..len]).is_err());
        }
    }
}
