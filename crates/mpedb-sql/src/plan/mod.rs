//! Compiled plans: the self-contained, deterministically serializable output
//! of `prepare()`. Other processes execute plans straight from these bytes,
//! so `decode` treats its input as hostile: every read is bounds-checked and
//! the decoded plan is re-validated against the schema, including a full
//! footprint recomputation.

use crate::planner;
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{AggFn, Collation,
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
// 25: native full-text search (design/DESIGN-FTS.md stage 1) — a new
//     `AccessPath::FtsScan` (tag `ACCESS_FTS_SCAN = 5`) carrying a recursively
//     encoded FTS5 query tree, produced for `<col-or-table> MATCH 'literal'`
//     against a `TableKind::Fts` table. A format-24 reader hits the unknown
//     access-path tag in `decode_access` and rejects the plan as corrupt rather
//     than misreading it — same additive gating as every prior access-path bump
//     (IndexRange at format 8, IndexPoint parts at 12). The schema canonical
//     bytes also gained a table-kind discriminant (v4), so a plan compiled
//     against a v3 schema fails its `schema_hash` check first — belt and braces.
// 26: recursive CTEs (design/DESIGN-CTE-RECURSIVE.md stage 1) — a new
//     `PlanStmt::RecursiveCte` (`STMT_RECURSIVE_CTE = 9`) carrying a name, the
//     declared column names + types, a `union_all` byte and three nested
//     `SelectPlan`s (anchor / recursive / outer). The recursive term and the
//     outer statement read the working table through the [`CTE_TABLE`] sentinel.
//     A format-25 reader hits the unknown statement tag in `decode_stmt` and
//     rejects the plan as corrupt rather than misreading it — same whole-plan
//     version gating as `STMT_COMPOUND` at format 9.
// 27: `printf` / `format` scalar function — the additive `ScalarFn::Printf` tag
//     (41) in the expr bytes, an ordinary variadic `Instr::Call`. A format-26
//     reader hits the unknown scalar tag in `ScalarFn::from_tag` and reports the
//     plan as corrupt rather than misreading it — the same additive gating as
//     every prior scalar-fn bump (14-16, 21, 22).
// 28: `COLLATE` collating sequences (BINARY/NOCASE/RTRIM). TWO shapes change in
//     one bump: (a) two additive expr opcodes — `Instr::CmpColl` (36, a collated
//     comparison) and `Instr::InListColl` (37, a collated `IN`) — for
//     `x = y COLLATE NOCASE` and friends; (b) every ORDER BY key grows a
//     trailing collation BYTE (`SelectPlan.order_by` and `CompoundPlan.order_by`
//     are now `(u16, bool, Collation)`), so even a plain `ORDER BY a` reserializes
//     one byte wider. A format-27 reader would take that extra order-by byte as
//     the next field and desync — so the whole-plan version gates it: a format-27
//     blob fails CLOSED at byte 0 with `PlanInvalidated` (the documented
//     re-prepare path), never a misread. Explicit `COLLATE` on a column
//     declaration is refused at parse time (stage 1b); a Binary comparison still
//     emits the plain nullary opcode, so only genuinely-collated plans carry the
//     new opcodes.
// 29: `CAST` becomes sqlite's permissive, affinity-based conversion. The
//     `Instr::Cast` payload BYTE changes meaning: it was a `ColumnType`
//     discriminant (1-7); it is now an `Affinity` (1=Integer, 2=Real, 3=Text,
//     4=Blob, 5=Numeric). Same width (one byte), but a format-28 `Cast` byte
//     would decode to the WRONG conversion (e.g. old Bool=3 → new Text=3) and,
//     worse, old strict `Cast(Int64)` plans would now prefix-parse text — so the
//     whole-plan version gates it: a format-28 blob fails CLOSED at byte 0 with
//     `PlanInvalidated` and is re-prepared under the new semantics.
// 30: sqlite "bare columns" in a grouped SELECT (COMPAT.md, `[compat]
//     bare_group_by = "sqlite"`). `Aggregation` grows a trailing `bare_cols`
//     LIST (base-row column indices) after `having`: the columns whose value is
//     carried from each group's single min()/max() witness row into the grouped
//     tuple `[keys ‖ aggs ‖ bare_cols]`. Empty for every strict/postgres plan
//     and every grouped plan without a live bare column, so only genuinely-bare
//     plans differ. A format-29 reader would run past `having` and desync on the
//     extra bytes, so the whole-plan version gates it: a format-29 blob fails
//     CLOSED at byte 0 with `PlanInvalidated` (the documented re-prepare path),
//     never a misread — the same additive `Aggregation` gating as every prior
//     grouped-block change.
// 31: compound bodies in a lifted subquery (`IN (SELECT … UNION …)`, a scalar
//     `(SELECT … UNION … LIMIT 1)`, `EXISTS (SELECT … UNION …)`). A [`SubPlan`]'s
//     body was always a `SelectPlan`; it is now a [`SubBody`] — `Select` OR
//     `Compound` — so `encode_subplan` writes a body-discriminant BYTE (0=Select,
//     1=Compound) where it used to inline the select directly. A format-30 reader
//     would take that tag byte as the select's low table-id byte and desync, so
//     the whole-plan version gates it: a format-30 blob fails CLOSED at byte 0
//     with `PlanInvalidated` (the documented re-prepare path), never a misread.
//     Compound-bodied subplans are UNCORRELATED and carry no nested lifts (a
//     subquery inside a compound arm is still refused), so every other subplan
//     field is unchanged — only genuinely-compound bodies differ.
// 32: table footprint bitmaps widened u64 → u128 (MAX_TABLES 64 → 128). The
//     wire layout gained 16 bytes (two u128s where two u64s were); a format-31
//     reader sees the changed FORMAT byte and re-prepares. No semantic change to
//     the query path — a wider ceiling, nothing more.
// 33: native INSERT OR REPLACE (PlanOnConflict::Replace, OC tag 3). Was
//     desugared to a PK-keyed DO UPDATE and REFUSED on any secondary UNIQUE;
//     now a first-class variant the executor resolves by deleting every
//     conflicting row (PK + each unique index) then inserting — sqlite's real
//     semantics.
// 34: window value/offset functions (design/DESIGN-WINDOW.md stage 2) —
//     `lag`/`lead`/`first_value`/`last_value`/`nth_value`. `WindowFunc` grows
//     five tags (Lag=5, Lead=6, FirstValue=7, LastValue=8, NthValue=9), the
//     Lag/Lead/NthValue tags each carry a trailing i64 (the constant offset /
//     n), and every `WindowSpec` grows an optional `default` program (the
//     lag/lead out-of-range default) encoded right after `distinct`. A format-33
//     reader hits an unknown window func tag (or desyncs on the i64 / default
//     bytes) and rejects the plan as corrupt rather than misreading it — the
//     same additive whole-plan-version gating as the stage-1 window bump (24).
// 35: window rank/distribution functions (design/DESIGN-WINDOW.md stage 2b) —
//     `ntile`/`percent_rank`/`cume_dist`. `WindowFunc` grows three tags
//     (Ntile=10, PercentRank=11, CumeDist=12); the Ntile tag carries a trailing
//     i64 (the constant bucket count), while PercentRank/CumeDist carry nothing
//     extra and take no argument. All three are argument-less at the WindowSpec
//     level (ntile's `n` is baked into the tag, not the `arg` program), so the
//     no-arg validate/decode guard extends to cover them. A format-34 reader
//     hits an unknown window func tag and rejects the plan as corrupt rather
//     than misreading it — the same additive whole-plan-version gating as the
//     earlier window bumps (24, 34).
const PLAN_FORMAT: u8 = 36;

/// The table id a FROM-less SELECT carries (`SELECT 3+5`): no table at all.
/// The executor yields ONE synthetic zero-column row; the footprint sets no
/// bits. Deliberately `u32::MAX` — a real schema caps table ids far below,
/// so no future table can collide with it.
pub const DUAL_TABLE: u32 = u32::MAX;

/// The table id a recursive CTE's WORKING TABLE carries
/// (design/DESIGN-CTE-RECURSIVE.md). Like [`DUAL_TABLE`], it names no catalog
/// table: the executor binds it to an in-memory row set (the fixpoint queue for
/// the recursive term, the full result for the outer statement). The validator,
/// footprint and EXPLAIN special-case it exactly as they do the dual sentinel,
/// and its synthetic [`TableDef`] (columns, no PK, no indexes ⇒ always
/// FullScan) is [`RecursiveCtePlan::cte_def`]. `u32::MAX - 1`, one below the
/// dual sentinel and far above any real table id.
pub const CTE_TABLE: u32 = u32::MAX - 1;

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
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
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
    /// tuple, and mpedb's expressions can RAISE (arithmetic overflow; division
    /// by zero is NULL, not a raise). A raise is observable, so an `on` that
    /// overflows on an inner column would report the existence of a row the
    /// policy hides — without ever returning it. Filtering first is what makes
    /// the policy a filter rather than a suggestion.
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
    /// The subquery's body (#56, format 31). A plain `SELECT` OR — for
    /// `IN (SELECT … UNION …)`, a scalar `(SELECT … UNION … LIMIT 1)`, or
    /// `EXISTS (SELECT … UNION …)` — a whole `Compound`. A compound body is
    /// UNCORRELATED and carries no nested lifts (`outer_args`/`subplans` empty),
    /// so it is executed once, up front, exactly like an uncorrelated select.
    pub body: SubBody,
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

/// A lifted subquery's BODY (#56, format 31): a plain `SELECT` or a whole
/// compound `SELECT … UNION/EXCEPT/INTERSECT …`. Only the lift positions that
/// consume the subquery as a value/list/existence — scalar `(…)`, `x IN (…)`,
/// `EXISTS (…)` — accept a `Compound`; a compound body is always UNCORRELATED
/// and carries no nested lifts, so wherever a subplan reaches back into its
/// enclosing row (`outer_args`, `post_filter`, nested `subplans`) the body is a
/// `Select`.
// `Select` is naturally larger than `Compound` (which is a Vec of arms); like
// [`PlanStmt`], the shape is part of the plan's frozen structure and `as_select`
// hands out a `&SelectPlan`, so boxing to equalize the variants would only add
// indirection to the common (simple-select) subplan.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SubBody {
    Select(SelectPlan),
    Compound(CompoundPlan),
}

impl SubBody {
    /// The caller-visible output arity of this body — a scalar/IN subplan must
    /// output exactly one column. A `Select`'s ORDER BY junk is not output; a
    /// compound's arms carry no junk (its `arms[0]` names the output).
    pub fn output_arity(&self) -> usize {
        match self {
            SubBody::Select(sp) => sp.projection.len() - sp.order_junk as usize,
            SubBody::Compound(c) => c.arms.first().map_or(0, |a| a.projection.len()),
        }
    }

    /// The `Select` body, if this is one (never a `Compound`). Used where only a
    /// plain select is representable — a correlated subplan, a post-filter, a
    /// nested lift — all of which a compound body is guaranteed not to be.
    pub fn as_select(&self) -> Option<&SelectPlan> {
        match self {
            SubBody::Select(sp) => Some(sp),
            SubBody::Compound(_) => None,
        }
    }
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
    /// (arithmetic overflow; division by zero is NULL, not a raise), and a
    /// raise is observable — so a predicate over the joined row that overflows
    /// on a hidden row's column would report that row's existence without
    /// returning it. Everything that can raise waits until both policies have
    /// had their say.
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
    /// (column index, descending, collation). Empty = scan order. The index is
    /// into the tuple named by `order_over` — never assume the base row. The
    /// [`Collation`] is [`Collation::Binary`] unless an explicit `COLLATE` was
    /// written on the ORDER BY term (`ORDER BY name COLLATE NOCASE`); it governs
    /// text ordering at sort time and is ignored for non-text keys.
    pub order_by: Vec<(u16, bool, Collation)>,
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
    /// `lag`/`lead`'s out-of-range DEFAULT expression, over the base row (stage
    /// 2, format 34). Evaluated at the CURRENT row when the offset lands outside
    /// the partition; `None` (⇒ NULL) for every function other than lag/lead and
    /// for a lag/lead whose default was omitted.
    pub default: Option<ExprProgram>,
    /// Explicit frame clause (format 36). `None` = the default frame (the exact
    /// stage-1/2 behaviour: `RANGE UNBOUNDED PRECEDING → CURRENT ROW` with an
    /// ORDER BY, else the whole partition). A frame is only carried on aggregate
    /// and `first_value`/`last_value`/`nth_value` windows — the only functions
    /// whose result depends on it; the planner refuses one on any other function.
    pub frame: Option<Frame>,
}

/// An explicit window frame (format 36): a unit (`ROWS`/`RANGE`/`GROUPS`) plus a
/// start and end boundary. The offsets are constants baked into the plan bytes,
/// so one content-hashed plan reproduces the same frame in every process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub mode: FrameMode,
    pub start: FrameBound,
    pub end: FrameBound,
}

/// Frame unit. `Rows` counts physical rows; `Range` compares ORDER BY values
/// (peer semantics for the supported UNBOUNDED/CURRENT ROW bounds); `Groups`
/// counts peer groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    Rows,
    Range,
    Groups,
}

/// A frame boundary. `Preceding`/`Following` carry a constant non-negative
/// offset (rows for `Rows`, peer-groups for `Groups`; refused for `Range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
}

impl Frame {
    /// Wire tag for a boundary as a FRAME START (`None` ⇒ illegal as a start,
    /// i.e. `UNBOUNDED FOLLOWING`). Also the ordinal used to reject an end that
    /// precedes the start — matching sqlite, which treats every `N PRECEDING`
    /// alike (rank 1) and every `N FOLLOWING` alike (rank 3) regardless of `N`.
    fn start_rank(b: FrameBound) -> Option<u8> {
        match b {
            FrameBound::UnboundedPreceding => Some(0),
            FrameBound::Preceding(_) => Some(1),
            FrameBound::CurrentRow => Some(2),
            FrameBound::Following(_) => Some(3),
            FrameBound::UnboundedFollowing => None,
        }
    }

    /// Ordinal of a boundary as a FRAME END (`None` ⇒ illegal as an end, i.e.
    /// `UNBOUNDED PRECEDING`).
    fn end_rank(b: FrameBound) -> Option<u8> {
        match b {
            FrameBound::UnboundedPreceding => None,
            FrameBound::Preceding(_) => Some(1),
            FrameBound::CurrentRow => Some(2),
            FrameBound::Following(_) => Some(3),
            FrameBound::UnboundedFollowing => Some(4),
        }
    }

    /// Whether this frame yields the same result regardless of the (arbitrary)
    /// row order within a partition — the condition for allowing it with NO
    /// window ORDER BY. `Range`/`Groups` collapse to a single peer group without
    /// an ORDER BY, so every such frame is whole-partition-or-empty; a physical
    /// `Rows` frame is order-dependent unless it spans the whole partition.
    fn order_independent(&self) -> bool {
        match self.mode {
            FrameMode::Range | FrameMode::Groups => true,
            FrameMode::Rows => matches!(
                (self.start, self.end),
                (FrameBound::UnboundedPreceding, FrameBound::UnboundedFollowing)
            ),
        }
    }

    /// Structural legality of the frame for `func`, given whether the window has
    /// an ORDER BY. Returns a human message on failure; the planner maps it to a
    /// `bind_err`, decode/validate to `Corrupt`, so the same rules gate both the
    /// prepare path and a hostile blob. The rules are sqlite's, verified against
    /// 3.45:
    ///  - a frame is meaningful only on aggregate / `first_value` / `last_value`
    ///    / `nth_value` windows (elsewhere sqlite silently ignores it — refused
    ///    here so a frame never quietly changes nothing);
    ///  - the start cannot be `UNBOUNDED FOLLOWING`, the end cannot be
    ///    `UNBOUNDED PRECEDING`, and the end cannot precede the start;
    ///  - `RANGE` with a `PRECEDING`/`FOLLOWING` offset is refused (its value
    ///    arithmetic with DESC/NULL ordering is not reproduced exactly);
    ///  - an order-dependent frame needs an ORDER BY.
    pub(crate) fn check(&self, func: WindowFunc, has_order_by: bool) -> std::result::Result<(), String> {
        if !matches!(
            func,
            WindowFunc::Agg(_)
                | WindowFunc::FirstValue
                | WindowFunc::LastValue
                | WindowFunc::NthValue(_)
        ) {
            return Err(
                "an explicit frame is only supported on aggregate and \
                 first_value/last_value/nth_value window functions"
                    .into(),
            );
        }
        let Some(sr) = Self::start_rank(self.start) else {
            return Err("a window frame cannot START at UNBOUNDED FOLLOWING".into());
        };
        let Some(er) = Self::end_rank(self.end) else {
            return Err("a window frame cannot END at UNBOUNDED PRECEDING".into());
        };
        if sr > er {
            return Err("unsupported frame specification: the end boundary precedes the start".into());
        }
        if matches!(self.mode, FrameMode::Range)
            && (matches!(self.start, FrameBound::Preceding(_) | FrameBound::Following(_))
                || matches!(self.end, FrameBound::Preceding(_) | FrameBound::Following(_)))
        {
            return Err(
                "RANGE with a PRECEDING/FOLLOWING offset is not supported — use ROWS or GROUPS \
                 for an offset frame, or RANGE with UNBOUNDED/CURRENT ROW bounds"
                    .into(),
            );
        }
        if !has_order_by && !self.order_independent() {
            return Err(
                "an explicit ROWS frame with a bounded edge requires an ORDER BY in the OVER clause \
                 (without one the row order, and so the frame, is undefined)"
                    .into(),
            );
        }
        Ok(())
    }
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
    /// `lag(expr, offset, …)` — the value `offset` rows BEFORE the current row in
    /// the partition (window order); out of range ⇒ the spec's `default` (or
    /// NULL). Frame-independent. The `i64` is the CONSTANT offset (folded at
    /// prepare; a non-constant offset is refused).
    Lag(i64),
    /// `lead(expr, offset, …)` — the value `offset` rows AFTER the current row.
    Lead(i64),
    /// `first_value(expr)` — the first row of the frame, i.e. (default frame)
    /// the partition's first row: constant across the partition.
    FirstValue,
    /// `last_value(expr)` — the last row of the frame: the current row's
    /// peer-group end (default RANGE frame with ORDER BY), or the partition's
    /// last row (no ORDER BY).
    LastValue,
    /// `nth_value(expr, n)` — the n-th row (1-based, `i64`) of the frame, or NULL
    /// if the frame has fewer than n rows. `n` is a CONSTANT ≥ 1 (validated).
    NthValue(i64),
    /// `ntile(n)` — the partition's rows distributed into `n` buckets as equally
    /// as possible (bucket number 1..n). sqlite's rule: the first `rows % n`
    /// buckets get `ceil(rows/n)` rows, the rest `floor`. `n` is a CONSTANT ≥ 1
    /// (validated); requires ORDER BY (the planner refuses it otherwise). Result
    /// is `Int64`, never NULL. Takes no per-row value.
    Ntile(i64),
    /// `percent_rank()` — `(rank - 1) / (rows_in_partition - 1)`, or 0.0 for a
    /// one-row partition. Uses `rank()` semantics (ties share). `Float64`, never
    /// NULL, no argument.
    PercentRank,
    /// `cume_dist()` — `(rows whose ORDER BY value is ≤ the current row's, peers
    /// included) / rows_in_partition`. `Float64`, never NULL, no argument.
    CumeDist,
}

impl WindowFunc {
    /// Wire tag. `Agg` is tag 4 followed by the [`AggFn`] tag byte;
    /// `Lag`/`Lead`/`NthValue`/`Ntile` are their tag followed by an i64
    /// (offset / n / bucket count).
    pub(crate) fn tag(self) -> u8 {
        match self {
            WindowFunc::RowNumber => 1,
            WindowFunc::Rank => 2,
            WindowFunc::DenseRank => 3,
            WindowFunc::Agg(_) => 4,
            WindowFunc::Lag(_) => 5,
            WindowFunc::Lead(_) => 6,
            WindowFunc::FirstValue => 7,
            WindowFunc::LastValue => 8,
            WindowFunc::NthValue(_) => 9,
            WindowFunc::Ntile(_) => 10,
            WindowFunc::PercentRank => 11,
            WindowFunc::CumeDist => 12,
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
    /// (output column index, descending, collation) over the compound OUTPUT
    /// tuple. Collation is [`Collation::Binary`] unless an explicit `COLLATE` was
    /// written on the compound ORDER BY term.
    pub order_by: Vec<(u16, bool, Collation)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// Self-imposed ceiling on compound arms, so a corrupt plan cannot make the
/// decoder allocate unboundedly. The corpus' longest chain is 9 arms.
const MAX_COMPOUND_ARMS: usize = 64;

/// A `WITH RECURSIVE <name>(<columns>) AS (<anchor> UNION[ ALL] <recursive>)
/// <outer>` statement (design/DESIGN-CTE-RECURSIVE.md stage 1).
///
/// Unlike a non-recursive CTE — flattened onto its base table at bind time
/// (DESIGN-CTE.md) — this is a genuine **fixpoint** the executor iterates: the
/// anchor seeds a result set and a FIFO queue, the recursive term is
/// re-evaluated with the working table bound to the PREVIOUS step's new rows
/// (semi-naive), and survivors accumulate until a step adds nothing (natural
/// fixpoint), the outer `LIMIT` is satisfied, or #74's work budget trips.
///
/// `anchor`, `recursive` and `outer` are ordinary [`SelectPlan`]s. The recursive
/// term and the outer statement read the working table through the [`CTE_TABLE`]
/// sentinel, whose synthetic [`TableDef`] is [`RecursiveCtePlan::cte_def`]; the
/// executor binds it to the queue (recursive) or the full result (outer). The
/// anchor never references it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveCtePlan {
    /// The CTE name — used for the #74 attribution `recursive CTE "<name>"` and
    /// for EXPLAIN.
    pub name: String,
    /// Declared column names (the REQUIRED `t(c1, …)` list). `columns.len()` is
    /// the CTE's arity; the anchor's projection must match it.
    pub columns: Vec<String>,
    /// The CTE's column types, derived from the anchor's projection and aligned
    /// to `columns`. A rigid engine fixes them here; the recursive term's
    /// projection must agree (arity AND type).
    pub col_types: Vec<ColumnType>,
    /// `UNION ALL` keeps every recursive row; `UNION` deduplicates each step's
    /// output against the full accumulated result (on the whole tuple).
    pub union_all: bool,
    /// Non-recursive seed. Reads real tables (or the dual row); NEVER the
    /// working table.
    pub anchor: SelectPlan,
    /// Recursive term. References the working table exactly once ([`CTE_TABLE`]),
    /// in a FROM/JOIN operand; `validate` re-enforces the §3 restrictions.
    pub recursive: SelectPlan,
    /// The outer statement, reading the CTE's full result via [`CTE_TABLE`].
    pub outer: SelectPlan,
}

impl RecursiveCtePlan {
    /// The synthetic [`TableDef`] the working table presents to the binder,
    /// validator, planner and EXPLAIN — id [`CTE_TABLE`], the declared columns
    /// typed by `col_types`, no PK and no indexes (so every access over it is a
    /// FullScan). Never registered in a schema; never reaches the row/key layer.
    pub fn cte_def(&self) -> TableDef {
        cte_working_table_def(&self.name, &self.columns, &self.col_types)
    }
}

/// Build the synthetic working-table [`TableDef`] for a recursive CTE. The
/// SINGLE source of the working table's shape — used by the planner (at compile
/// time) and by [`RecursiveCtePlan::cte_def`] (validate / footprint / EXPLAIN),
/// so the def a plan is built against can never drift from the def it is
/// re-validated against. Columns are nullable (sqlite treats every value as
/// nullable; the anchor may seed one and the recursion NULL it — the permissive
/// 3VL choice, never a wrong answer); no PK and no indexes ⇒ every access is a
/// FullScan.
pub(crate) fn cte_working_table_def(
    name: &str,
    columns: &[String],
    col_types: &[ColumnType],
) -> TableDef {
    TableDef {
        id: CTE_TABLE,
        name: name.to_string(),
        columns: columns
            .iter()
            .zip(col_types)
            .map(|(name, &ty)| mpedb_types::ColumnDef {
                name: name.clone(),
                ty,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            })
            .collect(),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    }
}

/// Statement shape the executor consumes.
// The Select variant is naturally larger than Begin/Commit/Rollback; the
// shape is frozen by the public API, so boxing is not an option here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStmt {
    Select(SelectPlan),
    /// `SELECT … UNION/EXCEPT/INTERSECT SELECT …` (#56, format 9).
    Compound(CompoundPlan),
    /// `WITH RECURSIVE … ` — a fixpoint over a working table (format 26,
    /// design/DESIGN-CTE-RECURSIVE.md). A read-only statement, like `Select`.
    RecursiveCte(RecursiveCtePlan),
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
    /// `SAVEPOINT <name>` — session/transaction control (no access path). The
    /// name is carried in the plan bytes so a prepared savepoint statement
    /// round-trips through the registry like any other plan.
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] <name>`.
    Release(String),
    /// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] <name>`.
    RollbackTo(String),
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
    /// `HAVING`, evaluated over the GROUPED row `[group keys ‖ agg results ‖
    /// bare_cols]` — a different tuple from the one `filter` sees, which is
    /// exactly why SQL has two clauses rather than one.
    pub having: Option<ExprProgram>,
    /// sqlite "bare columns" (COMPAT.md, `[compat] bare_group_by = "sqlite"`):
    /// base-row column indices whose value each group carries from its **single
    /// `min()`/`max()` witness row** — the one input row that achieved the
    /// extremum. They occupy the grouped tuple AFTER the aggregates, so the
    /// grouped tuple is `[keys ‖ aggs ‖ bare_cols]` and a projection/HAVING/ORDER
    /// BY term reads bare column `j` at slot `group_by.len() + aggs.len() + j`.
    ///
    /// **Empty for every strict/postgres plan, and for a sqlite plan whose bare
    /// columns all constant-folded away** (the `COALESCE(const, col)` case — the
    /// column reference is gone, so nothing is carried). Non-empty ⇒ the planner
    /// and [`validate`] guarantee exactly one aggregate and it is `Min`/`Max`;
    /// the executor reads the witness row's values for these columns. The
    /// deterministic-value contract (never a wrong answer) lives in the planner:
    /// a bare column that is neither folded away nor min/max-determined is
    /// REFUSED, never represented here.
    pub bare_cols: Vec<u16>,
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
    /// `INSERT OR REPLACE`: sqlite's delete-on-ANY-unique semantics. The
    /// executor proactively deletes every existing row the proposed row would
    /// conflict with — on the PK AND on each secondary UNIQUE index — then
    /// inserts. Carries no payload: the executor derives the unique index set
    /// from the live `TableDef` (unambiguous — "all unique indexes", not a
    /// name→index mapping that could be re-derived inconsistently). Matches
    /// default sqlite, which fires no DELETE triggers for these removals
    /// (recursive_triggers off).
    Replace,
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
    /// Full-text search over an FTS table's inverted index (design/DESIGN-FTS.md
    /// §4). The `query` is a compiled FTS5 query tree whose terms are already
    /// normalized by the table's frozen tokenizer; the executor evaluates it by
    /// posting-list set algebra and yields matching rows in rowid order. Only an
    /// FTS table (`TableKind::Fts`) carries this access — `validate` enforces it.
    FtsScan { query: FtsQuery },
}

/// A compiled FTS5 `MATCH` query tree (design/DESIGN-FTS.md §3), carried by
/// [`AccessPath::FtsScan`] and content-hashed into the plan. Terms are already
/// normalized by the table's frozen tokenizer, so execution never re-tokenizes.
/// Precedence (highest first, sqlite fts5): `NOT`, then `AND` (and implicit-AND
/// juxtaposition), then `OR`.
#[derive(Debug, Clone, PartialEq)]
pub enum FtsQuery {
    Term(FtsTerm),
    And(Box<FtsQuery>, Box<FtsQuery>),
    Or(Box<FtsQuery>, Box<FtsQuery>),
    /// `X NOT Y` — documents matching `X` but NOT `Y`.
    AndNot(Box<FtsQuery>, Box<FtsQuery>),
}

/// One FTS query term (a bare word, `word*` prefix, `^word` initial, or a
/// column-filtered `col:word` / `{a b}:word`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsTerm {
    /// The normalized token; for a prefix term this is the prefix.
    pub token: String,
    /// `word*` — match every indexed token starting with `token`.
    pub prefix: bool,
    /// `^word` — the token must occur at position 0 in an allowed column.
    pub initial: bool,
    /// Restrict to these FTS column ordinals (0-based over declared columns);
    /// empty = every column. A whole-row `ft MATCH …` leaves this empty; a
    /// column-scoped `col MATCH …` and `col:`/`{a b}:` filters populate it.
    pub columns: Vec<u16>,
}

/// Self-imposed ceiling on the depth of a compiled FTS query tree, so a corrupt
/// plan cannot make the recursive decoder overflow the stack. Far above any
/// hand-written `MATCH` string.
// Caps the TOTAL node count of an FTS query tree (the decoder spends one budget
// unit per node; the planner enforces the same total at bind — see
// `planner::fts::fts_node_count` — so a bind-accepted query always round-trips
// through decode in another process). Also bounds parser/decoder recursion depth
// (<= node count), kept modest so a left-leaning chain cannot overflow the stack.
pub(crate) const MAX_FTS_DEPTH: usize = 512;

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
const STMT_SAVEPOINT: u8 = 9;
const STMT_RELEASE: u8 = 10;
const STMT_ROLLBACK_TO: u8 = 11;
const STMT_RECURSIVE_CTE: u8 = 12;

// A lifted subquery's body discriminant (format 31): a plain SELECT or a whole
// compound. Written by `encode_subplan` right before the body.
const SUBBODY_SELECT: u8 = 0;
const SUBBODY_COMPOUND: u8 = 1;

const ACCESS_FULL: u8 = 0;
const ACCESS_PK_POINT: u8 = 1;
const ACCESS_PK_RANGE: u8 = 2;
const ACCESS_INDEX_POINT: u8 = 3;
const ACCESS_INDEX_RANGE: u8 = 4;
const ACCESS_FTS_SCAN: u8 = 5;

// FTS query-node wire tags (design/DESIGN-FTS.md §3).
const FTS_TERM: u8 = 0;
const FTS_AND: u8 = 1;
const FTS_OR: u8 = 2;
const FTS_AND_NOT: u8 = 3;

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
const OC_REPLACE: u8 = 3;

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
            // A recursive CTE reads several base tables (across anchor /
            // recursive / outer); like a compound it has no single target, and
            // `policies` stamps each real table read.
            PlanStmt::RecursiveCte(_) => None,
            PlanStmt::Begin
            | PlanStmt::Commit
            | PlanStmt::Rollback
            | PlanStmt::Savepoint(_)
            | PlanStmt::Release(_)
            | PlanStmt::RollbackTo(_) => None,
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
