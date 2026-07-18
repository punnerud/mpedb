//! Compiled plans: the self-contained, deterministically serializable output
//! of `prepare()`. Other processes execute plans straight from these bytes,
//! so `decode` treats its input as hostile: every read is bounds-checked and
//! the decoded plan is re-validated against the schema, including a full
//! footprint recomputation.

use crate::planner;
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{AggFn, 
    ColumnType, Error, ExprProgram, Footprint, Instr, KeyBound, KeyPart, PlanHash, Result, Schema,
    TableDef, Value, FORMAT_VERSION, MAX_COLUMNS,
};

mod decode;
mod encode;
mod explain;
mod validate;

#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod tests;

pub(crate) use explain::render_program;

/// Leading byte of the plan wire format (independent of [`FORMAT_VERSION`],
/// which is mixed into the hash). An older blob fails `decode` closed, at byte
/// 0, with a version error — which is the point: plans are stored persistently
/// in the catalog's sys-keyspace, so a file written by an older build hands
/// this decoder bytes in a layout it no longer speaks.
///
/// 2: reserved session-context parameter slots (design/DESIGN-MULTIDB.md §2).
/// 3: aggregates. Every `Select` now encodes an `aggregate` tag byte after
///    `offset`, and every `AggCall` a `distinct` byte. Both are layout changes
///    to EXISTING records, not just new statement kinds — a v2 `Select` blob
///    read by this decoder would take the byte after `offset` as the aggregate
///    tag and desynchronize from there. It would most likely then fail some
///    later bounds check, but "most likely" is exactly what this byte exists to
///    replace with "always".
/// 4: `Select` encodes `order_over` before the `order_by` count.
/// 5: joins. `Select` encodes a `join` tag and `order_junk`, and the plan
///    header carries a LIST of policy stamps where it carried one epoch+hash.
/// 6: `ColumnType::Any` (tag 7) — see value.rs.
/// 7: N-way INNER join. `Select` encodes a COUNT of joins where it encoded a
///    single optional join, so a v6 reader would misread the byte stream.
/// 6: `ColumnType::Any` (tag 7). No plan FIELD moved — but a plan carries the
///    column types it bound against, and a v5 reader decoding a v6 plan would
///    hit tag 7 in `ColumnType::from_tag`, get `None`, and report the plan as
///    corrupt rather than as "written by a newer mpedb". The bump makes it say
///    the true thing.
/// Self-imposed ceiling on joins in one SELECT, so a corrupt plan cannot make the
/// decoder allocate unboundedly. Far above any hand-written query.
const MAX_JOINS: usize = 16;

// 8: Join grew a kind byte (INNER/LEFT, with RIGHT/FULL tags reserved),
//    KeyPart grew OuterCol (index nested-loop parametrization), and
//    AccessPath grew IndexRange — one bump for the whole window, so mixed
//    binaries against the shared plan registry see a clean "unknown plan
//    format" instead of a confusing "bad tag" mid-decode.
// 9: the #56 SQL window — the instruction set grew Cast/Concat (`CAST`, `||`);
//    UNION/compound statements and uncorrelated subplans ride the same bump.
// 10: FROM-less SELECT (#67) — `SelectPlan.table` may be the DUAL_TABLE
//    sentinel (one synthetic empty row, no table read). New decode contract
//    for a value that format 9 rejected as out of range, hence the bump.
// 11: `IN (SELECT …)` (#70) — the subplan tag byte grew List(2); format 10
//    decoders reject it as a bad exists tag, hence the bump.
// 12: composite indexes (#55) — `IndexPoint` carries a LIST of parts where
//    it carried exactly one, so a format-11 reader would take the count
//    byte as a part tag and desynchronize. Rides the canonical-bytes-v2
//    window (schemas carry explicit `TableDef.indexes`).
// 13: new aggregate tags `total` (6) and `group_concat` (7). Additive, but a
//    format-12 reader hits `AggFn::from_tag` → None and rejects the plan, so
//    the whole-plan version gates it cleanly.
// 14: new scalar fns `replace` (8), `ltrim` (9), `rtrim` (10), `instr` (11) —
//    additive `ScalarFn` tags in the expr bytes, gated the same way.
// 15: math scalar fns `sqrt` (12), `pow`/`power` (13), `sign` (14) — same
//    additive `ScalarFn`-tag gating.
// 16: `ceil`/`ceiling` (15), `floor` (16) — type-preserving, same gating.
// 17: INSERT … SELECT — the Insert stmt carries an optional embedded select
//     plan + column map after its VALUES rows, so a format-16 reader would
//     desync on the extra bytes.
// 18: general `x IS y` / `x IS NOT y` (NULL-safe distinct-from) — the expr
//     bytes grew two additive `Instr` opcodes (IsNotDistinct=32, IsDistinct=33).
//     A format-17 reader hits an unknown opcode in `ExprProgram::decode` and
//     reports the plan as corrupt rather than "written by a newer mpedb", so
//     the whole-plan version gates it cleanly — same additive pattern as the
//     scalar-fn bumps (14-16).
// 19: `GLOB` / `NOT GLOB` (sqlite's case-sensitive `*`/`?`/`[...]` matcher) —
//     one additive `Instr` opcode (Glob=34) in the expr bytes. A format-18
//     reader hits the unknown opcode in `ExprProgram::decode` and rejects the
//     plan as corrupt rather than misreading it — the same additive gating as
//     the LIKE-shaped and scalar-fn bumps above.
// 20: nested subqueries (#73 §3 stage 1) — `SubPlan` becomes RECURSIVE. Each
//     subplan record grows a `sub_base` (u16), a `slot_type` byte, and a
//     trailing COUNT + list of its OWN nested `SubPlan`s (its uncorrelated inner
//     lifts). A format-19 reader would take the new `sub_base` bytes as the old
//     `outer_args` count and desynchronize — so the whole-plan version gates it:
//     a format-19 blob decoded here (or a format-20 blob decoded by a format-19
//     binary) fails CLOSED at byte 0 with `PlanInvalidated`, the documented
//     re-prepare path, never a misread of the new shape.
// 21: batch of scalar fns — `char` (17), `unicode` (18), `hex` (19), `typeof`
//     (20) as additive `ScalarFn` tags, plus `trim(x, y)` (the 2-arg form now
//     passes `arity_ok`). A format-20 reader hits an unknown scalar tag in
//     `ScalarFn::from_tag` (or rejects the new Trim arity) and reports the plan
//     as corrupt rather than misreading it — same additive gating as the
//     scalar-fn bumps 14-16. `iif` rides along with no new tag: it desugars to
//     a CASE, exactly like `nullif`.
// 22: math scalar fns — `exp`/`ln`/`log10`/`log2`/`log`(base)/`sin`/`cos`/`tan`/
//     `asin`/`acos`/`atan`/`atan2`/`sinh`/`cosh`/`tanh`/`radians`/`degrees`/`pi`/
//     `mod`/`trunc` as additive `ScalarFn` tags 21..=40. A format-21 reader hits
//     an unknown scalar tag in `ScalarFn::from_tag` and reports the plan as
//     corrupt rather than misreading it — same additive gating as the scalar-fn
//     bumps 14-16 and 21. `log`/`log10` and `mod`/`pi` add no new opcode: they
//     are ordinary `Instr::Call`s, so only the whole-plan version gates them.
// 23: `REGEXP` / `NOT REGEXP` — the additive `Instr::Regexp` opcode (tag 35).
//     A format-22 reader hits the unknown opcode in `ExprProgram::decode` and
//     reports the plan as corrupt rather than misreading it — same additive
//     gating as the `Glob` opcode at format 19.
// 24: window functions (design/DESIGN-WINDOW.md stage 1) — every `Select` record grows
//     a trailing `windows` LIST after its `aggregate` block (a count + one
//     `WindowSpec` each: func tag, optional arg program, distinct byte, a
//     PARTITION BY program list and an ORDER BY `(program, desc)` list). A
//     format-23 reader would run past the aggregate block and desync on the
//     extra bytes — exactly as every prior additive `Select` change did — so the
//     whole-plan version gates it: a format-23 blob fails CLOSED at byte 0 with
//     `PlanInvalidated` (the documented re-prepare path), never a misread.
const PLAN_FORMAT: u8 = 24;

/// The table id a FROM-less SELECT carries (`SELECT 3+5`): no table at all.
/// The executor yields ONE synthetic zero-column row; the footprint sets no
/// bits. Deliberately `u32::MAX` — a real schema caps table ids far below,
/// so no future table can collide with it.
pub const DUAL_TABLE: u32 = u32::MAX;

/// The zero-column [`TableDef`] that stands in for `DUAL_TABLE` wherever the
/// planner/validator needs "the table's" width or column names. It never
/// reaches the row or key layer (its empty `primary_key` would violate that
/// layer's invariant), and it is never registered in a schema.
pub fn dual_def() -> &'static mpedb_types::TableDef {
    use std::sync::OnceLock;
    static DUAL: OnceLock<mpedb_types::TableDef> = OnceLock::new();
    DUAL.get_or_init(|| mpedb_types::TableDef {
        id: 0,
        name: String::new(),
        columns: Vec::new(),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        dead: false,
    })
}

/// One table's RLS state, frozen at compile time. See
/// [`CompiledPlan::policies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyStamp {
    pub table: u32,
    pub epoch: u64,
    pub hash: [u8; 32],
}

/// What a missing inner match MEANS for one join step. Encoded as one byte
/// with tags 2 (RIGHT) and 3 (FULL) reserved so adding them later needs no
/// new format bump — decode refuses them by name until they execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// No match → no row.
    Inner,
    /// No match → one row with the inner side NULL-extended.
    Left,
    /// Unmatched rows on BOTH sides NULL-extend (#64). Only as a statement's
    /// single join, and only with a FullScan inner access — the matched-set
    /// bookkeeping needs the inner side enumerated and held. (RIGHT has no
    /// plan-level kind at all: the planner rewrites it to a swapped LEFT.)
    Full,
}

/// The inner side of one `JOIN` step, driven by a nested loop over the outer.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: u32,
    pub kind: JoinKind,
    /// How the inner side is read. An access path whose parts are all
    /// `Param`/`Const` is resolved once and the inner side is read once and
    /// held; one carrying `KeyPart::OuterCol` is the index nested-loop form,
    /// re-resolved and fetched PER OUTER ROW.
    pub access: AccessPath,
    /// The `ON` condition, over the JOINED row `[outer ‖ inner]`. Kept separate
    /// from `filter` even though an INNER JOIN's ON and WHERE are
    /// interchangeable, because they are not interchangeable for the reader:
    /// `EXPLAIN` has to be able to say which one the query wrote.
    pub on: ExprProgram,
    /// The inner table's RLS `USING`, over the INNER row alone — applied as the
    /// inner side is read, before `on` ever sees it.
    ///
    /// It cannot be folded into `on` or `filter`: those run over the joined
    /// tuple, and mpedb's expressions can RAISE (division by zero, overflow).
    /// A raise is observable, so an `on` that divides by an inner column would
    /// report the existence of a row the policy hides — without ever returning
    /// it. Filtering first is what makes the policy a filter rather than a
    /// suggestion.
    pub policy: Option<ExprProgram>,
}

/// Which tuple a `Select`'s `order_by` indexes.
///
/// Each stage of the pipeline produces a different tuple, and the sort runs
/// against exactly one of them:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOver {
    /// The BASE row, sorted before projection. The only variant that keeps the
    /// PK-prefix elision and the streaming top-K path, both of which are about
    /// scan order — so the planner prefers it whenever every key is a plain
    /// column.
    BaseRow,
    /// The GROUPED tuple `[keys ‖ aggs]`. Lets a sort key be an aggregate that
    /// is not selected (`ORDER BY count(*)`).
    Grouped,
    /// The PROJECTION, sorted last. Required when the sort must follow a dedup
    /// (`DISTINCT`), and the only place a computed key like `ORDER BY amt * 2`
    /// or an ordinal like `ORDER BY 1` can be named — no base-column index
    /// refers to either.
    Projection,
}

/// One lifted subquery (#56). The inner select's parameter space is
/// `[user params ‖ correlation args]`: outer-row references inside the
/// subquery were rewritten to trailing params, and `outer_args[j]` names the
/// OUTER base-row slot whose value fills inner param `n_user + j` — the same
/// parametrization idea as the index nested loop's `OuterCol`, applied to a
/// whole plan. `outer_args` empty = uncorrelated: evaluated ONCE per execute
/// (before access resolution, so a PK probe may consume its slot), not per row.
///
/// **Recursive (#73 §3 stage 1).** A subquery may CONTAIN subqueries: `subplans`
/// holds this inner's own lifts, with their result slots living in THIS subplan's
/// inner parameter buffer `[user ‖ correlation args ‖ children results]` at
/// `sub_base + i`. For stage 1 every nested child is UNCORRELATED (`outer_args`
/// empty) — a nested subquery that references an enclosing row is stages 2–3 and
/// is refused. The executor fills the uncorrelated children ONCE, bottom-up,
/// before this subplan's own access resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct SubPlan {
    pub plan: SelectPlan,
    pub outer_args: Vec<u16>,
    pub kind: SubPlanKind,
    /// This subplan's OWN lifted subqueries. Empty = a leaf (the pre-#73 shape).
    pub subplans: Vec<SubPlan>,
    /// First reserved child-result slot in this subplan's inner parameter buffer
    /// = `n_user + outer_args.len()`; child `i`'s result occupies `sub_base + i`.
    /// Its value is the `n_params` the inner was planned with, so exec/validate
    /// can locate the children without re-deriving the layout.
    pub sub_base: u16,
    /// The type THIS subplan's result slot carries in its PARENT's buffer
    /// (`Bool` for EXISTS, the projected column's type for a scalar, `None` for
    /// an IN-list). Carried so a parent's `validate` can type its inner param
    /// space — including a child used as a key part — without re-planning.
    pub slot_type: Option<ColumnType>,
}

/// What a subplan's result slot HOLDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SubPlanKind {
    /// One column; 0 rows → NULL; >1 row → runtime error (PostgreSQL's
    /// line — sqlite silently takes the first row).
    Scalar = 0,
    /// `EXISTS (…)`: `Bool(any rows)`.
    Exists = 1,
    /// `x IN (SELECT …)` (#70): the slot holds a LIST of the single output
    /// column's values, consumed by the `InParam` membership instruction —
    /// the same runtime-typed 3VL core session-context lists use.
    /// Uncorrelated only (the planner refuses, the decoder re-refuses).
    List = 2,
}

impl SubPlanKind {
    pub fn from_tag(t: u8) -> Option<SubPlanKind> {
        Some(match t {
            0 => SubPlanKind::Scalar,
            1 => SubPlanKind::Exists,
            2 => SubPlanKind::List,
            _ => return None,
        })
    }
}

/// Ceiling on subplans per statement — decoder DoS bound, far above real SQL.
const MAX_SUBPLANS: usize = 16;

/// A compiled, content-addressed statement plan.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledPlan {
    pub stmt: PlanStmt,
    /// blake3 of the canonical schema the plan was compiled against.
    pub schema_hash: [u8; 32],
    /// Total parameter count = caller params + reserved session-context slots.
    pub n_params: u16,
    /// One entry per parameter; `None` = unconstrained by this statement. The
    /// last `context_keys.len()` entries are the reserved context slots and are
    /// always constrained (a context ref must be usable in a typed comparison).
    pub param_types: Vec<Option<ColumnType>>,
    /// One session-context key per reserved parameter slot (design/DESIGN-MULTIDB.md
    /// §2.1), aligned to the final `context_keys.len()` entries of
    /// `param_types`. Empty for statements with no `current_setting()`. The
    /// values are NEVER stored here — they are filled from the caller's
    /// `Session` at execute time, so one content-hashed plan serves all sessions.
    pub context_keys: Vec<String>,
    /// RLS leak-proofing (design/DESIGN-MULTIDB.md §4), one entry per table whose
    /// policy this plan BAKED IN — which for a join is both of them.
    ///
    /// This is a list rather than a single pair because the check has to cover
    /// every policy the plan froze. Validating only the outer table of a join
    /// would let a cached plan keep serving the inner table's rows under a
    /// policy that has since been tightened: the exact leak §4 exists to close,
    /// reopened by the table it forgot.
    ///
    /// Each entry: the table's `pol_epoch` at compile time (fast path: equal
    /// epoch ⇒ current), and the content hash of its applicable policy set
    /// ([`table_policy_hash`](crate::table_policy_hash)) — so a moved epoch with
    /// a matching hash (a no-op edit) is still current, and a mismatch is stale
    /// ⇒ `PlanInvalidated`.
    pub policies: Vec<PolicyStamp>,
    /// Plan-level constant pool, referenced by [`KeyPart::Const`] and
    /// [`InsertSource::Const`].
    pub consts: Vec<Value>,
    /// Lifted subqueries (#56). Subplan `i`'s RESULT occupies the reserved
    /// parameter slot `subplan_base() + i`; the caller passes only the user
    /// params, the facade leaves these slots NULL, and the executor fills
    /// them (uncorrelated: once up front; correlated: per outer row). The
    /// parameter layout is `[user ‖ subplan results ‖ context]` — context
    /// stays LAST, so the session-fill formula is untouched.
    pub subplans: Vec<SubPlan>,
    pub footprint: Footprint,
}

impl CompiledPlan {
    /// First reserved subplan-result parameter slot.
    pub fn subplan_base(&self) -> u16 {
        self.n_params - self.context_keys.len() as u16 - self.subplans.len() as u16
    }
}

/// One SELECT, compiled. Extracted from the `PlanStmt::Select` variant so a
/// nested select is a VALUE — a compound statement's arms (and later #56's
/// uncorrelated subplans) hold `SelectPlan`s directly, which is what makes
/// "an arm that is an INSERT" unrepresentable instead of a validation case.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectPlan {
    pub table: u32,
    pub access: AccessPath,
    /// `INNER JOIN` chain, left-deep. Empty = a single-table read, and then
    /// every tuple below is the base row. Join `k`'s `on` runs over the row
    /// accumulated through join `k` — `[table0 ‖ … ‖ table_{k+1}]`.
    pub joins: Vec<Join>,
    /// Residual predicate after access-path extraction, over the BASE row
    /// (the outer row, when joined). Always the base row — see
    /// `joined_filter` for the other one.
    pub filter: Option<ExprProgram>,
    /// Predicate over the JOINED row `[outer ‖ inner]`; `None` without a
    /// join.
    ///
    /// The split from `filter` is the security-relevant part, not a
    /// refactor: RLS policies live in `filter` and `join.policy`, which run
    /// over one row each and BEFORE this. mpedb's expressions can raise
    /// (division by zero, overflow), and a raise is observable — so a
    /// predicate over the joined row that divides by a hidden row's column
    /// would report that row's existence without returning it. Everything
    /// that can raise waits until both policies have had their say.
    pub joined_filter: Option<ExprProgram>,
    /// Predicate over the base row that may read CORRELATED subplan slots
    /// (params at `subplan_base()..`), so it cannot run inside the gather —
    /// those slots are filled per outer row, after `filter` and the policies
    /// have had their say. `None` for every plan without correlated
    /// subqueries. Splitting it from `filter` rather than flagging is what
    /// lets validate FORBID gather-side programs from reading unfilled slots.
    pub post_filter: Option<ExprProgram>,
    /// Output columns, in order.
    pub projection: Vec<Projection>,
    /// (column index, descending). Empty = scan order. The index is into
    /// the tuple named by `order_over` — never assume the base row.
    pub order_by: Vec<(u16, bool)>,
    /// Which tuple `order_by` indexes, and therefore where in the pipeline
    /// the sort runs. Explicit rather than inferred from `distinct` /
    /// `aggregate`, because it is a decision the planner makes and the
    /// decoder must bounds-check against the right width — inferring it
    /// twice is how the two come to disagree.
    pub order_over: OrderOver,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// Grouping, applied **after** `filter`. `None` = no aggregation.
    pub aggregate: Option<Aggregation>,
    /// Trailing `projection` entries that exist ONLY to be sorted by, and
    /// must be dropped before the rows reach the caller.
    ///
    /// `SELECT c FROM t ORDER BY a + 1` sorts by something it does not
    /// output. PostgreSQL calls these resjunk columns; the key has to be
    /// computed somewhere, and the projection is the only tuple the sort
    /// can reach. So the planner appends it, the executor sorts, and then
    /// trims — which is why `order_over` is `Projection` whenever this is
    /// nonzero.
    ///
    /// Always 0 under `distinct`: DISTINCT requires every key to be in the
    /// SELECT list already, and a junk column would break the dedup anyway
    /// by making rows distinct on a value the caller never sees.
    pub order_junk: u16,
    /// `SELECT DISTINCT` — deduplicate the PROJECTED tuples.
    ///
    /// It cannot be pushed into the scan (the projection is what is being
    /// deduplicated, and it may be an expression), and it means `limit`
    /// bounds DISTINCT rows rather than scanned rows — the same trap
    /// `aggregate` has, so the executor must not pass a scan bound down
    /// when this is set.
    pub distinct: bool,
    /// Window functions, in output-slot order (design/DESIGN-WINDOW.md). Empty = none.
    ///
    /// Each produces one extra column APPENDED to the base row; the projection
    /// (and any ORDER BY junk) reads window `k`'s result at slot
    /// `base_width + k` via the synthetic windowed tuple. Present only on a plan
    /// the window phase runs over; `aggregate` and `windows` are mutually
    /// exclusive (validate refuses both together — stage 1). Non-empty forces
    /// `order_over = Projection` (the sort must follow the window phase).
    pub windows: Vec<WindowSpec>,
}

/// One window function call, compiled. The `arg`/`partition_by`/`order_by`
/// programs all read the BASE row; the result lands in the synthetic windowed
/// tuple at `base_width + k` (design/DESIGN-WINDOW.md §3.2).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub func: WindowFunc,
    /// Aggregate/value argument, over the base row. `None` for `count(*)` and
    /// the ranking functions.
    pub arg: Option<ExprProgram>,
    /// `DISTINCT` inside a window aggregate — always `false` in stage 1
    /// (decode and validate refuse `true`).
    pub distinct: bool,
    /// PARTITION BY expressions, over the base row. Empty = one partition.
    pub partition_by: Vec<ExprProgram>,
    /// Window ORDER BY: `(program over base row, descending)`. Empty = the whole
    /// partition is one peer group (no cumulative frame).
    pub order_by: Vec<(ExprProgram, bool)>,
}

/// Which window function a [`WindowSpec`] computes. A closed enum with wire tags
/// (like [`AggFn`]/`ScalarFn`): ranking is SQL-only, and the aggregate half
/// reuses [`AggFn`] verbatim so the NULL/overflow/type rules never fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    /// Distinct sequential 1..n within each partition; ties broken by input
    /// (gather) order.
    RowNumber,
    /// Ties share a rank, the next rank SKIPS (1,1,3).
    Rank,
    /// Ties share a rank, no gaps (1,1,2).
    DenseRank,
    /// An aggregate over the default frame — cumulative (`RANGE … CURRENT ROW`)
    /// when the window has ORDER BY, else the whole partition.
    Agg(AggFn),
}

impl WindowFunc {
    /// Wire tag. `Agg` is tag 4 followed by the [`AggFn`] tag byte.
    pub(crate) fn tag(self) -> u8 {
        match self {
            WindowFunc::RowNumber => 1,
            WindowFunc::Rank => 2,
            WindowFunc::DenseRank => 3,
            WindowFunc::Agg(_) => 4,
        }
    }
}

/// Self-imposed ceiling on window functions in one SELECT — a decoder DoS bound,
/// far above any hand-written query.
const MAX_WINDOWS: usize = 64;

/// A compound-statement set operator. sqlite semantics: `UNION`, `EXCEPT`
/// and `INTERSECT` are SET operators (the result is deduplicated); only
/// `UNION ALL` keeps duplicates. Chains apply LEFT-ASSOCIATIVELY with equal
/// precedence — sqlite's rule, and the one the sqllogictest corpus' expected
/// results are computed under. (PostgreSQL instead gives INTERSECT higher
/// precedence; a mixed chain ported from PG may need restructuring.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    UnionAll,
    Except,
    Intersect,
}

impl SetOp {
    pub(crate) fn from_tag(t: u8) -> Option<SetOp> {
        Some(match t {
            0 => SetOp::Union,
            1 => SetOp::UnionAll,
            2 => SetOp::Except,
            3 => SetOp::Intersect,
            _ => return None,
        })
    }
}

/// A compound SELECT: `arm[0] op[0] arm[1] op[1] arm[2] …`, evaluated
/// left-associatively, then the compound-level ORDER BY / LIMIT / OFFSET.
///
/// Invariants (enforced by `validate`, relied on by the executor):
/// - `arms.len() >= 2` and `ops.len() == arms.len() - 1`;
/// - every arm projects the SAME arity (the planner also requires the same
///   output TYPES — rigid engine, no sqlite-style cross-arm coercion);
/// - no arm carries its own `order_by` / `order_junk` / `limit` / `offset`:
///   those clauses belong to the compound, and SQL cannot express them per
///   arm without parentheses (unsupported).
#[derive(Debug, Clone, PartialEq)]
pub struct CompoundPlan {
    pub arms: Vec<SelectPlan>,
    pub ops: Vec<SetOp>,
    /// (output column index, descending) over the compound OUTPUT tuple.
    pub order_by: Vec<(u16, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// Self-imposed ceiling on compound arms, so a corrupt plan cannot make the
/// decoder allocate unboundedly. The corpus' longest chain is 9 arms.
const MAX_COMPOUND_ARMS: usize = 64;

/// Statement shape the executor consumes.
// The Select variant is naturally larger than Begin/Commit/Rollback; the
// shape is frozen by the public API, so boxing is not an option here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStmt {
    Select(SelectPlan),
    /// `SELECT … UNION/EXCEPT/INTERSECT SELECT …` (#56, format 9).
    Compound(CompoundPlan),
    Insert {
        table: u32,
        /// `rows[r][col_idx]`: one entry per table column per row. Empty when
        /// `from_select` is `Some` (INSERT … SELECT).
        rows: Vec<Vec<InsertSource>>,
        /// `INSERT … SELECT` source. Mutually exclusive with a non-empty `rows`.
        from_select: Option<InsertSelect>,
        /// RLS `WITH CHECK` gate on the new row (design/DESIGN-MULTIDB.md §3.7).
        /// Evaluated with `eval_filter` semantics — NULL and FALSE both REJECT
        /// (NOT the CHECK-constraint rule). `None` = no RLS write gate.
        with_check: Option<ExprProgram>,
        on_conflict: PlanOnConflict,
        /// `RETURNING` projection over the written row. `None` = no clause.
        returning: Option<Vec<Projection>>,
    },
    Update {
        table: u32,
        access: AccessPath,
        filter: Option<ExprProgram>,
        /// column index -> value expression
        set: Vec<(u16, ExprProgram)>,
        /// RLS `WITH CHECK` gate on the post-image row (see `Insert::with_check`).
        with_check: Option<ExprProgram>,
        /// `RETURNING` over the POST-image row.
        returning: Option<Vec<Projection>>,
    },
    Delete {
        table: u32,
        access: AccessPath,
        filter: Option<ExprProgram>,
        /// `RETURNING` over the row as it was BEFORE deletion — there is no
        /// post-image to project.
        returning: Option<Vec<Projection>>,
    },
    Begin,
    Commit,
    Rollback,
}

/// Compiled `GROUP BY` / aggregates / `HAVING`.
///
/// **The ordering here is a security property, not a style choice**
/// (DESIGN-MULTIDB §4). Aggregation consumes rows only AFTER the merged
/// `(WHERE ∧ effective-policy)` predicate — which is `filter` plus whatever the
/// access path already excluded. An aggregate fed the pre-filter tuple stream
/// would count and sum rows the caller cannot see, and `count(*)` leaking the
/// existence of hidden rows is a leak whether or not the rows themselves come
/// back. §4 calls that "a natural mistake, since some policy conjuncts land in
/// the residual"; the executor avoids it by aggregating the output of
/// `gather_rows`, which has already applied both.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregation {
    /// Group keys, evaluated over the BASE row. **Empty = one group over
    /// every surviving row** — that is `SELECT count(*) FROM t`, and it must
    /// still produce exactly one row when the table is empty.
    pub group_by: Vec<GroupKey>,
    /// The aggregate calls, in output order. Their arguments are evaluated over
    /// the BASE row.
    pub aggs: Vec<AggCall>,
    /// `HAVING`, evaluated over the GROUPED row `[group keys ‖ agg results]` —
    /// a different tuple from the one `filter` sees, which is exactly why SQL
    /// has two clauses rather than one.
    pub having: Option<ExprProgram>,
}

/// One GROUP BY key (#56). `GROUP BY a` is a base-row column; `GROUP BY a+1`
/// is a computed key — sqlite and PostgreSQL both allow it, and the grouped
/// tuple `[keys ‖ aggs]` carries the computed value like any other key.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupKey {
    Col(u16),
    Expr(ExprProgram),
}

/// One aggregate call.
#[derive(Debug, Clone, PartialEq)]
pub struct AggCall {
    pub func: AggFn,
    /// `count(DISTINCT x)` — deduplicate this aggregate's INPUT values within
    /// each group before accumulating. Meaningless but legal for min/max.
    pub distinct: bool,
    /// `None` = `count(*)`: the argument is the ROW, not a value, so NULL cannot
    /// arise and every row counts. `Some(p)` is evaluated over the base row and
    /// NULLs are skipped. That difference is the whole reason `count(*)` exists.
    pub arg: Option<ExprProgram>,
}

/// The compiled `ON CONFLICT` action.
///
/// **Not available on an RLS-enabled table, by design.** DESIGN-MULTIDB §6.5
/// closes a classification oracle by collapsing PrimaryKey/Unique/Check
/// violations into one opaque `WriteRejected`, so a caller cannot learn WHICH
/// constraint an invisible row tripped. `ON CONFLICT DO NOTHING` reopens exactly
/// that channel — a silent skip means "a unique conflict", an error means
/// "something else" — and `DO UPDATE` is worse: it would overwrite a row the
/// caller cannot see. PostgreSQL permits both and documents the inference;
/// mpedb made the §6.5 promise, so the planner refuses instead of quietly
/// weakening it.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOnConflict {
    Error,
    DoNothing,
    DoUpdate {
        /// Table column indices of the conflict target.
        target: Vec<u16>,
        /// How the executor finds the conflicting row. Carried in the plan
        /// rather than re-derived at execution: the index numbering lives in
        /// exactly one place (`secondary_indexes`), and a second derivation is
        /// how the two come to disagree about which index `email` is.
        probe: ConflictProbe,
        /// column index -> value expression, evaluated over the EXISTING row
        /// concatenated with the PROPOSED row (`excluded.<c>` = `Col(n + i)`).
        set: Vec<(u16, ExprProgram)>,
        /// Optional `WHERE` on the update, over the same doubled row.
        filter: Option<ExprProgram>,
    },
}

/// How `ON CONFLICT … DO UPDATE` locates the row it conflicted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictProbe {
    /// Target is the primary key: `get_by_pk`.
    Pk,
    /// Target is one secondary UNIQUE column, probed by its index number
    /// (1-based over `secondary_indexes`; 0 is the PK tree).
    Index(u32),
}

/// Where an inserted column value comes from. `Default` means "use the
/// column's DEFAULT"; the executor resolves it to the declared default or,
/// for a nullable column without one, to NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertSource {
    Param(u16),
    /// Index into [`CompiledPlan::consts`].
    Const(u16),
    Default,
}

/// `INSERT INTO t [(cols)] SELECT …` source (COMPAT). The source query produces
/// one output tuple per row to insert; `col_map[ci]` says where table column
/// `ci`'s value comes from — an index into that tuple, or `None` for the
/// column's DEFAULT / NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertSelect {
    pub plan: Box<SelectPlan>,
    pub col_map: Vec<Option<u16>>,
}

/// Physical access path over the target table.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessPath {
    /// All PK columns pinned by equality. `parts.len()` == PK column count,
    /// in PK order.
    PkPoint(Vec<KeyPart>),
    /// Range over the FIRST PK column only (Phase 1). `None` = unbounded.
    PkRange {
        lo: Option<KeyBound>,
        hi: Option<KeyBound>,
    },
    /// Equality probe of secondary index `index_no` (1-based; index 0 is the
    /// PK tree). `parts` cover a PREFIX of the index's columns in key order
    /// (#55: 1..=width; a single-column index is the k = 1 case). At most
    /// one row when the index is UNIQUE and the parts cover its full width;
    /// otherwise every row equal on the covered prefix (the executor picks
    /// exact-get vs prefix-scan from the index shape).
    IndexPoint { index_no: u32, parts: Vec<KeyPart> },
    /// Range over secondary index `index_no`'s FIRST column: `WHERE idx_col
    /// \> $1 AND idx_col <= $2` — the same Phase-1 rule as `PkRange`, and
    /// it serves composite indexes unchanged (the first column's encoding
    /// is a key prefix). Bounds carry exactly one part each; prefix
    /// semantics over the `(values ‖ pk)` keys make one construction serve
    /// unique and non-unique indexes.
    IndexRange {
        index_no: u32,
        lo: Option<KeyBound>,
        hi: Option<KeyBound>,
    },
    FullScan,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// A bare table column.
    Column(u16),
    /// A computed output column with its canonical display name.
    Expr { program: ExprProgram, name: String },
}

// ---- wire tags -----------------------------------------------------------

const STMT_SELECT: u8 = 1;
const STMT_INSERT: u8 = 2;
const STMT_UPDATE: u8 = 3;
const STMT_DELETE: u8 = 4;
const STMT_BEGIN: u8 = 5;
const STMT_COMMIT: u8 = 6;
const STMT_ROLLBACK: u8 = 7;
const STMT_COMPOUND: u8 = 8;

const ACCESS_FULL: u8 = 0;
const ACCESS_PK_POINT: u8 = 1;
const ACCESS_PK_RANGE: u8 = 2;
const ACCESS_INDEX_POINT: u8 = 3;
const ACCESS_INDEX_RANGE: u8 = 4;

const PART_PARAM: u8 = 0;
const PART_CONST: u8 = 1;
const PART_OUTER_COL: u8 = 2;

const PROJ_COLUMN: u8 = 0;
const PROJ_EXPR: u8 = 1;

const SRC_PARAM: u8 = 0;
const SRC_CONST: u8 = 1;
const SRC_DEFAULT: u8 = 2;

const OC_ERROR: u8 = 0;
const OC_DO_NOTHING: u8 = 1;
const OC_DO_UPDATE: u8 = 2;

impl CompiledPlan {
    /// The table this plan targets (for RLS policy-epoch validation), if any.
    pub fn target_table(&self) -> Option<u32> {
        match &self.stmt {
            PlanStmt::Select(SelectPlan { table, .. })
            | PlanStmt::Insert { table, .. }
            | PlanStmt::Update { table, .. }
            | PlanStmt::Delete { table, .. } => Some(*table),
            // A compound has no SINGLE target; staleness is covered by the
            // per-arm entries in `policies`, which stamp every table read.
            PlanStmt::Compound(_) => None,
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => None,
        }
    }

    /// Content hash: blake3(canonical bytes ‖ schema_hash ‖ FORMAT_VERSION).
    pub fn hash(&self) -> PlanHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.encode());
        hasher.update(&self.schema_hash);
        hasher.update(&FORMAT_VERSION.to_le_bytes());
        PlanHash(*hasher.finalize().as_bytes())
    }
}
