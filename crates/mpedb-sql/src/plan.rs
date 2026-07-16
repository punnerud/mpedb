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

/// Leading byte of the plan wire format (independent of [`FORMAT_VERSION`],
/// which is mixed into the hash). An older blob fails `decode` closed, at byte
/// 0, with a version error — which is the point: plans are stored persistently
/// in the catalog's sys-keyspace, so a file written by an older build hands
/// this decoder bytes in a layout it no longer speaks.
///
/// 2: reserved session-context parameter slots (DESIGN-MULTIDB.md §2).
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

const PLAN_FORMAT: u8 = 7;

/// One table's RLS state, frozen at compile time. See
/// [`CompiledPlan::policies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyStamp {
    pub table: u32,
    pub epoch: u64,
    pub hash: [u8; 32],
}

/// The inner side of an `INNER JOIN`, driven by a nested loop over the outer.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: u32,
    /// How the inner side is read FOR EACH outer row.
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
    /// One session-context key per reserved parameter slot (DESIGN-MULTIDB.md
    /// §2.1), aligned to the final `context_keys.len()` entries of
    /// `param_types`. Empty for statements with no `current_setting()`. The
    /// values are NEVER stored here — they are filled from the caller's
    /// `Session` at execute time, so one content-hashed plan serves all sessions.
    pub context_keys: Vec<String>,
    /// RLS leak-proofing (DESIGN-MULTIDB.md §4), one entry per table whose
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
    pub footprint: Footprint,
}

/// Statement shape the executor consumes.
// The Select variant is naturally larger than Begin/Commit/Rollback; the
// shape is frozen by the public API, so boxing is not an option here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStmt {
    Select {
        table: u32,
        access: AccessPath,
        /// `INNER JOIN` chain, left-deep. Empty = a single-table read, and then
        /// every tuple below is the base row. Join `k`'s `on` runs over the row
        /// accumulated through join `k` — `[table0 ‖ … ‖ table_{k+1}]`.
        joins: Vec<Join>,
        /// Residual predicate after access-path extraction, over the BASE row
        /// (the outer row, when joined). Always the base row — see
        /// `joined_filter` for the other one.
        filter: Option<ExprProgram>,
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
        joined_filter: Option<ExprProgram>,
        /// Output columns, in order.
        projection: Vec<Projection>,
        /// (column index, descending). Empty = scan order. The index is into
        /// the tuple named by `order_over` — never assume the base row.
        order_by: Vec<(u16, bool)>,
        /// Which tuple `order_by` indexes, and therefore where in the pipeline
        /// the sort runs. Explicit rather than inferred from `distinct` /
        /// `aggregate`, because it is a decision the planner makes and the
        /// decoder must bounds-check against the right width — inferring it
        /// twice is how the two come to disagree.
        order_over: OrderOver,
        limit: Option<u64>,
        offset: Option<u64>,
        /// Grouping, applied **after** `filter`. `None` = no aggregation.
        aggregate: Option<Aggregation>,
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
        order_junk: u16,
        /// `SELECT DISTINCT` — deduplicate the PROJECTED tuples.
        ///
        /// It cannot be pushed into the scan (the projection is what is being
        /// deduplicated, and it may be an expression), and it means `limit`
        /// bounds DISTINCT rows rather than scanned rows — the same trap
        /// `aggregate` has, so the executor must not pass a scan bound down
        /// when this is set.
        distinct: bool,
    },
    Insert {
        table: u32,
        /// `rows[r][col_idx]`: one entry per table column per row.
        rows: Vec<Vec<InsertSource>>,
        /// RLS `WITH CHECK` gate on the new row (DESIGN-MULTIDB.md §3.7).
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
    /// Base-row column indices to group by. **Empty = one group over every
    /// surviving row** — that is `SELECT count(*) FROM t`, and it must still
    /// produce exactly one row when the table is empty.
    pub group_by: Vec<u16>,
    /// The aggregate calls, in output order. Their arguments are evaluated over
    /// the BASE row.
    pub aggs: Vec<AggCall>,
    /// `HAVING`, evaluated over the GROUPED row `[group keys ‖ agg results]` —
    /// a different tuple from the one `filter` sees, which is exactly why SQL
    /// has two clauses rather than one.
    pub having: Option<ExprProgram>,
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
    /// Point probe of secondary unique index `index_no` (1-based; index 0 is
    /// the PK tree — see [`crate::secondary_indexes`]).
    IndexPoint { index_no: u32, part: KeyPart },
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

const ACCESS_FULL: u8 = 0;
const ACCESS_PK_POINT: u8 = 1;
const ACCESS_PK_RANGE: u8 = 2;
const ACCESS_INDEX_POINT: u8 = 3;

const PART_PARAM: u8 = 0;
const PART_CONST: u8 = 1;

const PROJ_COLUMN: u8 = 0;
const PROJ_EXPR: u8 = 1;

const SRC_PARAM: u8 = 0;
const SRC_CONST: u8 = 1;
const SRC_DEFAULT: u8 = 2;

fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

// ---- bounded readers ------------------------------------------------------

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(n)
        .filter(|&e| e <= buf.len())
        .ok_or_else(|| corrupt("truncated plan"))?;
    let s = &buf[*pos..end];
    *pos = end;
    Ok(s)
}

fn r_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    Ok(take(buf, pos, 1)?[0])
}

fn r_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    Ok(u16::from_le_bytes(take(buf, pos, 2)?.try_into().unwrap()))
}

fn r_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()))
}

fn r_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    Ok(u64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap()))
}

impl CompiledPlan {
    /// The table this plan targets (for RLS policy-epoch validation), if any.
    pub fn target_table(&self) -> Option<u32> {
        match &self.stmt {
            PlanStmt::Select { table, .. }
            | PlanStmt::Insert { table, .. }
            | PlanStmt::Update { table, .. }
            | PlanStmt::Delete { table, .. } => Some(*table),
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

    /// Canonical, deterministic serialization (the plan-registry blob and the
    /// first component of the hash preimage).
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.push(PLAN_FORMAT);
        buf.extend_from_slice(&self.schema_hash);
        buf.extend_from_slice(&(self.param_types.len() as u16).to_le_bytes());
        for pt in &self.param_types {
            buf.push(pt.map_or(0, |t| t as u8));
        }
        buf.extend_from_slice(&(self.context_keys.len() as u16).to_le_bytes());
        for k in &self.context_keys {
            buf.extend_from_slice(&(k.len() as u16).to_le_bytes());
            buf.extend_from_slice(k.as_bytes());
        }
        buf.extend_from_slice(&(self.policies.len() as u16).to_le_bytes());
        for p in &self.policies {
            buf.extend_from_slice(&p.table.to_le_bytes());
            buf.extend_from_slice(&p.epoch.to_le_bytes());
            buf.extend_from_slice(&p.hash);
        }
        buf.extend_from_slice(&(self.consts.len() as u16).to_le_bytes());
        for c in &self.consts {
            write_value(&mut buf, c);
        }
        self.footprint.encode_into(&mut buf);
        encode_stmt(&self.stmt, &mut buf);
        buf
    }

    /// Decode and fully re-validate a plan blob against `schema`. The input
    /// may come from a corrupt or hostile shared-memory region: every read is
    /// bounds-checked, all indices are range-checked against the schema, and
    /// the embedded footprint must equal a freshly recomputed one.
    pub fn decode(bytes: &[u8], schema: &Schema) -> Result<CompiledPlan> {
        let mut pos = 0usize;
        let format = r_u8(bytes, &mut pos)?;
        if format != PLAN_FORMAT {
            return Err(corrupt(format!("unknown plan format {format}")));
        }
        let mut schema_hash = [0u8; 32];
        schema_hash.copy_from_slice(take(bytes, &mut pos, 32)?);
        let n_params = r_u16(bytes, &mut pos)?;
        let mut param_types = Vec::with_capacity((n_params as usize).min(1024));
        for _ in 0..n_params {
            let tag = r_u8(bytes, &mut pos)?;
            param_types.push(if tag == 0 {
                None
            } else {
                Some(
                    ColumnType::from_tag(tag)
                        .ok_or_else(|| corrupt(format!("bad param type tag {tag}")))?,
                )
            });
        }
        let n_context = r_u16(bytes, &mut pos)? as usize;
        if n_context > n_params as usize {
            return Err(corrupt("more session-context slots than parameters"));
        }
        let n_user = n_params as usize - n_context;
        let mut context_keys = Vec::with_capacity(n_context.min(1024));
        for p in 0..n_context {
            let klen = r_u16(bytes, &mut pos)? as usize;
            let kb = take(bytes, &mut pos, klen)?;
            let key = std::str::from_utf8(kb)
                .map_err(|_| corrupt("session-context key is not valid utf-8"))?
                .to_string();
            if key.is_empty() {
                return Err(corrupt("empty session-context key"));
            }
            // A reserved context slot must carry a concrete type, or its
            // session value cannot be type-checked at execute time.
            if param_types[n_user + p].is_none() {
                return Err(corrupt("session-context slot has no inferred type"));
            }
            context_keys.push(key);
        }
        let n_pol = r_u16(bytes, &mut pos)? as usize;
        // One per table the statement touches; a join touches two. The cap is a
        // sanity bound on a length read from an untrusted blob.
        if n_pol > MAX_COLUMNS {
            return Err(corrupt("too many policy stamps in plan"));
        }
        let mut policies = Vec::with_capacity(n_pol.min(8));
        for _ in 0..n_pol {
            let table = r_u32(bytes, &mut pos)?;
            let epoch = r_u64(bytes, &mut pos)?;
            let mut hash = [0u8; 32];
            hash.copy_from_slice(take(bytes, &mut pos, 32)?);
            policies.push(PolicyStamp { table, epoch, hash });
        }
        let n_consts = r_u16(bytes, &mut pos)?;
        let mut consts = Vec::with_capacity((n_consts as usize).min(1024));
        for _ in 0..n_consts {
            consts.push(read_value(bytes, &mut pos)?);
        }
        let footprint = Footprint::decode(bytes, &mut pos)?;
        let stmt = decode_stmt(bytes, &mut pos)?;
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes in plan"));
        }
        let plan = CompiledPlan {
            stmt,
            schema_hash,
            n_params,
            param_types,
            context_keys,
            policies,
            consts,
            footprint,
        };
        if plan.schema_hash != schema.hash() {
            return Err(Error::PlanInvalidated);
        }
        plan.validate(schema)?;
        Ok(plan)
    }

    /// Semantic re-validation against the schema: index/column/parameter
    /// bounds, PK shapes, typed constants, and footprint consistency
    /// (recomputed from scratch and compared, so a forged footprint in an
    /// otherwise well-formed blob is rejected).
    pub(crate) fn validate(&self, schema: &Schema) -> Result<()> {
        let get_table = |id: u32| {
            schema
                .table(id)
                .ok_or_else(|| corrupt(format!("table id {id} out of range")))
        };
        match &self.stmt {
            PlanStmt::Select {
                table,
                access,
                joins,
                joined_filter,
                filter,
                projection,
                order_by,
                order_over,
                aggregate,
                distinct,
                order_junk,
                ..
            } => {
                let t = get_table(*table)?;
                // Junk columns are sort-only and get trimmed, so they must not
                // be able to (a) eat the whole output, (b) survive a DISTINCT —
                // where they would dedup on a value the caller never sees — or
                // (c) exist where nothing sorts the projection.
                let junk = *order_junk as usize;
                if junk > 0 {
                    if *order_over != OrderOver::Projection {
                        return Err(corrupt("order-junk columns without a projection sort"));
                    }
                    if *distinct {
                        return Err(corrupt("order-junk columns under DISTINCT"));
                    }
                    if junk >= projection.len() {
                        return Err(corrupt("order-junk columns leave no output"));
                    }
                }
                self.check_access(access, t)?;
                // With a join the "base row" IS the joined row, so every width
                // below moves. Getting this wrong is not cosmetic: a program
                // bounded against the outer's width alone could not name the
                // inner's columns at all, and one bounded against nothing could
                // read past the tuple.
                if joins.len() > MAX_JOINS {
                    return Err(corrupt("too many joins in plan"));
                }
                // Width accumulates left to right: join `k`'s `on` runs over
                // `[table0 ‖ … ‖ table_{k+1}]`, so its bound grows as we go. Each
                // join's POLICY runs over its OWN row alone (the whole point of
                // it being separate), so it is bounded by that one table's width.
                // A self-join (same table id twice) is legal since #44 — tables
                // are addressed by alias, and the plan carries slots, not names.
                let mut acc_width = t.columns.len();
                for j in joins {
                    let jt = get_table(j.table)?;
                    self.check_access(&j.access, jt)?;
                    if let Some(p) = &j.policy {
                        self.check_program(p, jt)?;
                    }
                    acc_width += jt.columns.len();
                    self.check_program_width(&j.on, acc_width)?;
                }
                let base_width = acc_width; // the full joined row
                if let Some(jf) = joined_filter {
                    if joins.is_empty() {
                        return Err(corrupt("joined filter without a join"));
                    }
                    self.check_program_width(jf, base_width)?;
                }
                // The sort key is an index into whichever tuple `order_over`
                // names, and those have different widths. Bounding it against
                // the wrong one is not a style point: too LOOSE lets a hostile
                // plan index past the tuple, and too TIGHT is worse than it
                // sounds — `cmp_rows` skips a key it cannot fetch, so an
                // out-of-range index silently drops the sort rather than
                // failing, and the caller gets an unordered answer to an
                // ORDER BY query.
                let order_width = |projection_len: usize, grouped: Option<usize>| match order_over {
                    OrderOver::BaseRow => base_width,
                    OrderOver::Grouped => grouped.unwrap_or(0),
                    OrderOver::Projection => projection_len,
                };
                if let Some(f) = filter {
                    // The OUTER's policy/residual, over the outer row alone.
                    self.check_program(f, t)?;
                }
                if let Some(a) = aggregate {
                    // GROUP BY columns and aggregate ARGUMENTS index the BASE
                    // row — which for a join is the JOINED row, hence
                    // `base_width` and not the outer table's; HAVING and the
                    // projection index the GROUPED tuple `[keys ‖ aggs]`, which
                    // is a different width again. Checking either against the
                    // wrong one would let a hostile plan read past its row — so
                    // they are bounded separately.
                    for c in &a.group_by {
                        if *c as usize >= base_width {
                            return Err(corrupt("GROUP BY column out of range"));
                        }
                    }
                    for c in &a.aggs {
                        if let Some(p) = &c.arg {
                            self.check_program_width(p, base_width)?;
                        }
                    }
                    let out_width = a.group_by.len() + a.aggs.len();
                    if out_width == 0 {
                        return Err(corrupt("aggregation with no groups and no aggregates"));
                    }
                    if let Some(h) = &a.having {
                        self.check_program_width(h, out_width)?;
                    }
                    for p in projection {
                        match p {
                            Projection::Column(i) => {
                                if *i as usize >= out_width {
                                    return Err(corrupt(
                                        "projection column out of the grouped tuple",
                                    ));
                                }
                            }
                            Projection::Expr { program, .. } => {
                                self.check_program_width(program, out_width)?
                            }
                        }
                    }
                    let w = order_width(projection.len(), Some(out_width));
                    for (c, _) in order_by {
                        if *c as usize >= w {
                            return Err(corrupt("order-by column out of range"));
                        }
                    }
                    return Ok(());
                }
                for p in projection {
                    match p {
                        Projection::Column(i) => {
                            if *i as usize >= base_width {
                                return Err(corrupt("projection column out of range"));
                            }
                        }
                        Projection::Expr { program, .. } => {
                            self.check_program_width(program, base_width)?
                        }
                    }
                }
                // A plain Select has no grouped tuple, so `OrderOver::Grouped`
                // here is itself a malformed plan rather than a width question.
                if *order_over == OrderOver::Grouped {
                    return Err(corrupt("order-over grouped without an aggregate"));
                }
                let w = order_width(projection.len(), None);
                for (c, _) in order_by {
                    if *c as usize >= w {
                        return Err(corrupt("order-by column out of range"));
                    }
                }
            }
            PlanStmt::Insert {
                table,
                rows,
                with_check,
                on_conflict,
                returning,
            } => {
                let t = get_table(*table)?;
                // A DO UPDATE's SET/WHERE run over [existing ‖ proposed], so
                // their column indices legitimately reach 2n-1. check_program
                // only knows about n, hence the dedicated check.
                match on_conflict {
                    PlanOnConflict::Error | PlanOnConflict::DoNothing => {}
                    PlanOnConflict::DoUpdate {
                        target,
                        probe,
                        set,
                        filter,
                    } => {
                        if target.is_empty() {
                            return Err(corrupt("ON CONFLICT DO UPDATE with no target"));
                        }
                        for c in target {
                            if *c as usize >= t.columns.len() {
                                return Err(corrupt("conflict-target column out of range"));
                            }
                        }
                        // Recompute the probe from the target and demand a
                        // match. A blob claiming "target (email), probe pk"
                        // would upsert the WRONG ROW — found by pk, reported as
                        // if found by email — which is a silent wrong answer,
                        // not a crash.
                        if *probe != crate::planner::conflict_probe(t, target) {
                            return Err(corrupt("conflict probe does not match the target"));
                        }
                        for (c, p) in set {
                            if *c as usize >= t.columns.len() {
                                return Err(corrupt("ON CONFLICT SET column out of range"));
                            }
                            self.check_doubled_program(p, t)?;
                        }
                        if let Some(f) = filter {
                            self.check_doubled_program(f, t)?;
                        }
                    }
                }
                if let Some(r) = returning {
                    self.check_projection(r, t)?;
                }
                if rows.is_empty() {
                    return Err(corrupt("INSERT plan with no rows"));
                }
                if let Some(w) = with_check {
                    self.check_program(w, t)?;
                }
                for row in rows {
                    if row.len() != t.columns.len() {
                        return Err(corrupt("INSERT row width mismatch"));
                    }
                    for (ci, src) in row.iter().enumerate() {
                        let col = &t.columns[ci];
                        match src {
                            InsertSource::Param(i) => {
                                if *i >= self.n_params {
                                    return Err(corrupt("param index out of range"));
                                }
                                if self.param_types[*i as usize] != Some(col.ty) {
                                    return Err(corrupt("insert param type mismatch"));
                                }
                            }
                            InsertSource::Const(i) => {
                                let v = self
                                    .consts
                                    .get(*i as usize)
                                    .ok_or_else(|| corrupt("const index out of range"))?;
                                if !v.fits(col.ty) {
                                    return Err(corrupt("insert const type mismatch"));
                                }
                                if v.is_null() && !col.nullable {
                                    return Err(corrupt("NULL insert into NOT NULL column"));
                                }
                            }
                            InsertSource::Default => {
                                if !col.nullable && col.default.is_none() {
                                    return Err(corrupt(
                                        "DEFAULT insert into NOT NULL column without default",
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            PlanStmt::Update {
                table,
                access,
                filter,
                set,
                with_check,
                returning,
            } => {
                let t = get_table(*table)?;
                if let Some(r) = returning {
                    self.check_projection(r, t)?;
                }
                self.check_access(access, t)?;
                if let Some(f) = filter {
                    self.check_program(f, t)?;
                }
                if let Some(w) = with_check {
                    self.check_program(w, t)?;
                }
                if set.is_empty() {
                    return Err(corrupt("UPDATE plan with empty SET"));
                }
                let mut seen = vec![false; t.columns.len()];
                for (c, program) in set {
                    let ci = *c as usize;
                    if ci >= t.columns.len() {
                        return Err(corrupt("SET column out of range"));
                    }
                    if t.is_pk_column(*c) {
                        return Err(corrupt("UPDATE plan sets a primary key column"));
                    }
                    if seen[ci] {
                        return Err(corrupt("duplicate SET column"));
                    }
                    seen[ci] = true;
                    self.check_program(program, t)?;
                }
            }
            PlanStmt::Delete {
                table,
                access,
                filter,
                returning,
            } => {
                let t = get_table(*table)?;
                self.check_access(access, t)?;
                if let Some(r) = returning {
                    self.check_projection(r, t)?;
                }
                if let Some(f) = filter {
                    self.check_program(f, t)?;
                }
            }
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {}
        }
        let recomputed = planner::compute_footprint(&self.stmt, schema)?;
        if recomputed != self.footprint {
            return Err(corrupt("plan footprint does not match its statement"));
        }
        Ok(())
    }

    fn check_program(&self, p: &ExprProgram, t: &TableDef) -> Result<()> {
        // Stack discipline and const-pool indices were proven by
        // ExprProgram::new/decode; column and parameter indices are ours.
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= t.columns.len() => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Validate a `RETURNING` projection: column indices in range, and any
    /// expression's own indices too.
    fn check_projection(&self, proj: &[Projection], t: &TableDef) -> Result<()> {
        for p in proj {
            match p {
                Projection::Column(i) => {
                    if *i as usize >= t.columns.len() {
                        return Err(corrupt("RETURNING column out of range"));
                    }
                }
                Projection::Expr { program, .. } => self.check_program(program, t)?,
            }
        }
        Ok(())
    }

    /// Bound a program's column indices by an arbitrary tuple width — for the
    /// GROUPED tuple `[keys ‖ aggs]`, which is not a table's row.
    fn check_program_width(&self, p: &ExprProgram, width: usize) -> Result<()> {
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= width => {
                    return Err(corrupt("expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A `DO UPDATE` SET/WHERE program runs over the EXISTING row concatenated
    /// with the PROPOSED one, so `Col(n + i)` is `excluded.<col i>` and is
    /// legal. `check_program` would reject those as out of range, so the bound
    /// here is 2n — but still a bound: a hostile plan must not read past the
    /// doubled row either.
    fn check_doubled_program(&self, p: &ExprProgram, t: &TableDef) -> Result<()> {
        let limit = t.columns.len() * 2;
        for i in &p.instrs {
            match *i {
                Instr::PushCol(c) if c as usize >= limit => {
                    return Err(corrupt("ON CONFLICT expression column out of range"));
                }
                Instr::PushParam(pi) if pi >= self.n_params => {
                    return Err(corrupt("expression param out of range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A key part must reference a valid param/const, and a const must be a
    /// non-NULL value of the key column's exact type.
    fn check_key_part(&self, p: &KeyPart, ty: ColumnType) -> Result<()> {
        match p {
            KeyPart::Param(i) => {
                if *i >= self.n_params {
                    return Err(corrupt("key param out of range"));
                }
                if self.param_types[*i as usize] != Some(ty) {
                    return Err(corrupt("key param type mismatch"));
                }
            }
            KeyPart::Const(i) => {
                let v = self
                    .consts
                    .get(*i as usize)
                    .ok_or_else(|| corrupt("key const out of range"))?;
                if v.is_null() || !v.fits(ty) {
                    return Err(corrupt("key const type mismatch"));
                }
            }
        }
        Ok(())
    }

    fn check_access(&self, a: &AccessPath, t: &TableDef) -> Result<()> {
        match a {
            AccessPath::FullScan => Ok(()),
            AccessPath::PkPoint(parts) => {
                if parts.len() != t.primary_key.len() {
                    return Err(corrupt("PkPoint part count != PK column count"));
                }
                for (part, &pk_col) in parts.iter().zip(&t.primary_key) {
                    self.check_key_part(part, t.columns[pk_col as usize].ty)?;
                }
                Ok(())
            }
            AccessPath::PkRange { lo, hi } => {
                if lo.is_none() && hi.is_none() {
                    return Err(corrupt("PkRange with no bounds"));
                }
                let first_ty = t.columns[t.primary_key[0] as usize].ty;
                for bound in [lo, hi].into_iter().flatten() {
                    if bound.parts.len() != 1 {
                        return Err(corrupt("Phase 1 PkRange bound must have exactly one part"));
                    }
                    self.check_key_part(&bound.parts[0], first_ty)?;
                }
                Ok(())
            }
            AccessPath::IndexPoint { index_no, part } => {
                let sec = crate::planner::secondary_indexes(t);
                let no = *index_no as usize;
                if no == 0 || no > sec.len() || no > 63 {
                    return Err(corrupt("index_no out of range"));
                }
                let col = sec[no - 1];
                self.check_key_part(part, t.columns[col as usize].ty)
            }
        }
    }

    /// Human-readable plan rendering for `EXPLAIN`.
    pub fn explain(&self, schema: &Schema) -> String {
        let table_name = |id: u32| {
            schema
                .table(id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| format!("table#{id}"))
        };
        let col_namer = |id: u32| {
            let t = schema.table(id).cloned();
            move |c: u16| match &t {
                Some(t) if (c as usize) < t.columns.len() => t.columns[c as usize].name.clone(),
                _ => format!("col#{c}"),
            }
        };
        let mut out = String::new();
        match &self.stmt {
            PlanStmt::Select {
                table,
                access,
                joins,
                joined_filter,
                filter,
                projection,
                order_by,
                order_over,
                limit,
                offset,
                aggregate,
                distinct,
                order_junk,
            } => {
                // With a join every tuple below is `[outer ‖ inner]`, so the
                // namer has to span both — and it qualifies, because `did` alone
                // would not say which side, and both sides usually have one.
                // The joined tuple is `[table0 ‖ … ‖ tableN]`; the namer walks
                // the widths to find which table a column index lands in, and
                // qualifies with the table name (both sides usually share a
                // column name, so bare would be ambiguous).
                let joined_tables: Vec<_> = std::iter::once(*table)
                    .chain(joins.iter().map(|j| j.table))
                    .filter_map(|id| schema.table(id).cloned())
                    .collect();
                let single = col_namer(*table);
                let base = |c: u16| {
                    // A single-table read names columns bare; only a join needs
                    // the `<table>.<column>` qualification to say which side.
                    if joined_tables.len() < 2 {
                        return single(c);
                    }
                    let mut off = 0usize;
                    for t in &joined_tables {
                        if (c as usize) < off + t.columns.len() {
                            return format!("{}.{}", t.name, t.columns[c as usize - off].name);
                        }
                        off += t.columns.len();
                    }
                    format!("col#{c}")
                };
                out.push_str(&format!(
                    "Select{} {}\n",
                    if *distinct { " DISTINCT" } else { "" },
                    table_name(*table)
                ));
                out.push_str(&format!(
                    "  access: {}\n",
                    self.render_access(access, schema, *table)
                ));
                if let Some(f) = filter {
                    // Over the OUTER row alone, so it uses the outer's namer.
                    out.push_str(&format!("  filter: {}\n", render_program(f, &single)));
                }
                for j in joins {
                    out.push_str(&format!(
                        "  inner join {} (nested loop, O(n*m) — no predicate pushdown)\n",
                        table_name(j.table)
                    ));
                    out.push_str(&format!(
                        "    access: {}\n",
                        self.render_access(&j.access, schema, j.table)
                    ));
                    if let Some(p) = &j.policy {
                        let iname = col_namer(j.table);
                        out.push_str(&format!("    policy: {}\n", render_program(p, &iname)));
                    }
                    out.push_str(&format!("    on: {}\n", render_program(&j.on, &base)));
                }
                if let Some(jf) = joined_filter {
                    out.push_str(&format!("  filter (joined): {}\n", render_program(jf, &base)));
                }
                // Everything below a grouping step indexes the GROUPED tuple
                // `[keys ‖ aggs]`, not the base row — so it needs its own namer.
                // Using the base one here printed the table's first column as the
                // name of `count(*)`, which is the kind of plausible wrong answer
                // EXPLAIN exists to rule out.
                let grouped: Option<Vec<String>> = aggregate.as_ref().map(|a| {
                    a.group_by
                        .iter()
                        .map(|c| base(*c))
                        .chain(a.aggs.iter().map(|c| match &c.arg {
                            None => format!("{}(*)", c.func.name()),
                            Some(p) => format!("{}({})", c.func.name(), render_program(p, &base)),
                        }))
                        .collect()
                });
                let name = |c: u16| match &grouped {
                    Some(g) => g
                        .get(c as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("col#{c}")),
                    None => base(c),
                };
                if let Some(a) = aggregate {
                    if !a.group_by.is_empty() {
                        let keys: Vec<String> = a.group_by.iter().map(|c| base(*c)).collect();
                        out.push_str(&format!("  group by: {}\n", keys.join(", ")));
                    }
                    let calls: Vec<String> = grouped.as_ref().unwrap()[a.group_by.len()..].to_vec();
                    out.push_str(&format!("  aggregate: {}\n", calls.join(", ")));
                    if let Some(h) = &a.having {
                        out.push_str(&format!("  having: {}\n", render_program(h, &name)));
                    }
                }
                let cols: Vec<String> = projection
                    .iter()
                    .map(|p| match p {
                        Projection::Column(i) => name(*i),
                        Projection::Expr { name, .. } => name.clone(),
                    })
                    .collect();
                // The junk columns are trailing and get trimmed before the
                // caller sees a row, so listing them under `project:` would
                // describe an output the query does not have. They are still
                // worth showing — they are work the plan does — just not as
                // output.
                let n_out = cols.len() - *order_junk as usize;
                out.push_str(&format!("  project: {}\n", cols[..n_out].join(", ")));
                if *order_junk > 0 {
                    out.push_str(&format!("  sort-only: {}\n", cols[n_out..].join(", ")));
                }
                if !order_by.is_empty() {
                    // The sort key indexes the tuple `order_over` names, which
                    // is not always the one `name` reads. Naming an output
                    // position with a base-column name is the same lie EXPLAIN
                    // told about `count(*)` before the grouped namer above.
                    let sort_name = |c: u16| match order_over {
                        OrderOver::Projection => cols
                            .get(c as usize)
                            .cloned()
                            .unwrap_or_else(|| format!("col#{c}")),
                        OrderOver::BaseRow => base(c),
                        OrderOver::Grouped => name(c),
                    };
                    let items: Vec<String> = order_by
                        .iter()
                        .map(|(c, desc)| {
                            format!("{}{}", sort_name(*c), if *desc { " DESC" } else { " ASC" })
                        })
                        .collect();
                    out.push_str(&format!("  order by: {}\n", items.join(", ")));
                }
                if let Some(n) = limit {
                    out.push_str(&format!("  limit: {n}\n"));
                }
                if let Some(n) = offset {
                    out.push_str(&format!("  offset: {n}\n"));
                }
            }
            PlanStmt::Insert {
                table,
                rows,
                with_check,
                on_conflict,
                returning,
            } => {
                let name = col_namer(*table);
                out.push_str(&format!("Insert {}\n", table_name(*table)));
                if let Some(w) = with_check {
                    out.push_str(&format!("  with check: {}\n", render_program(w, &name)));
                }
                match on_conflict {
                    PlanOnConflict::Error => {}
                    PlanOnConflict::DoNothing => out.push_str("  on conflict: do nothing\n"),
                    PlanOnConflict::DoUpdate {
                        target,
                        probe,
                        set,
                        filter,
                    } => {
                        let cols: Vec<String> = target.iter().map(|c| name(*c)).collect();
                        out.push_str(&format!(
                            "  on conflict ({}): do update (via {})\n",
                            cols.join(", "),
                            match probe {
                                ConflictProbe::Pk => "pk".to_string(),
                                ConflictProbe::Index(n) => format!("index {n}"),
                            }
                        ));
                        for (c, p) in set {
                            out.push_str(&format!(
                                "    set {} = {}\n",
                                name(*c),
                                render_program(p, &name)
                            ));
                        }
                        if let Some(f) = filter {
                            out.push_str(&format!("    where {}\n", render_program(f, &name)));
                        }
                    }
                }
                if returning.is_some() {
                    out.push_str("  returning: yes\n");
                }
                for (r, row) in rows.iter().enumerate() {
                    let items: Vec<String> = row
                        .iter()
                        .enumerate()
                        .map(|(c, src)| {
                            let v = match src {
                                InsertSource::Param(i) => format!("${}", i + 1),
                                InsertSource::Const(i) => self.render_const(*i),
                                InsertSource::Default => "DEFAULT".into(),
                            };
                            format!("{} = {v}", name(c as u16))
                        })
                        .collect();
                    out.push_str(&format!("  row {}: {}\n", r + 1, items.join(", ")));
                }
            }
            PlanStmt::Update {
                table,
                access,
                filter,
                set,
                with_check,
                returning,
            } => {
                let name = col_namer(*table);
                out.push_str(&format!("Update {}\n", table_name(*table)));
                if returning.is_some() {
                    out.push_str("  returning: yes\n");
                }
                if let Some(w) = with_check {
                    out.push_str(&format!("  with check: {}\n", render_program(w, &name)));
                }
                out.push_str(&format!(
                    "  access: {}\n",
                    self.render_access(access, schema, *table)
                ));
                if let Some(f) = filter {
                    out.push_str(&format!("  filter: {}\n", render_program(f, &name)));
                }
                let items: Vec<String> = set
                    .iter()
                    .map(|(c, p)| format!("{} = {}", name(*c), render_program(p, &name)))
                    .collect();
                out.push_str(&format!("  set: {}\n", items.join(", ")));
            }
            PlanStmt::Delete {
                table,
                access,
                filter,
                returning,
            } => {
                let name = col_namer(*table);
                out.push_str(&format!("Delete {}\n", table_name(*table)));
                if returning.is_some() {
                    out.push_str("  returning: yes\n");
                }
                out.push_str(&format!(
                    "  access: {}\n",
                    self.render_access(access, schema, *table)
                ));
                if let Some(f) = filter {
                    out.push_str(&format!("  filter: {}\n", render_program(f, &name)));
                }
            }
            PlanStmt::Begin => out.push_str("Begin\n"),
            PlanStmt::Commit => out.push_str("Commit\n"),
            PlanStmt::Rollback => out.push_str("Rollback\n"),
        }
        out.push_str(&format!(
            "  footprint: read_only={} tables_read={:#x} tables_written={:#x} indexes_used={:#x} key={}\n",
            self.footprint.read_only,
            self.footprint.tables_read,
            self.footprint.tables_written,
            self.footprint.indexes_used,
            match &self.footprint.key_access {
                mpedb_types::KeyAccess::Point(_) => "Point",
                mpedb_types::KeyAccess::Range { .. } => "Range",
                mpedb_types::KeyAccess::Full => "Full",
            }
        ));
        out
    }

    fn render_const(&self, i: u16) -> String {
        self.consts
            .get(i as usize)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    }

    fn render_part(&self, p: &KeyPart) -> String {
        match p {
            KeyPart::Param(i) => format!("${}", i + 1),
            KeyPart::Const(i) => self.render_const(*i),
        }
    }

    fn render_access(&self, a: &AccessPath, schema: &Schema, table: u32) -> String {
        let col_name = |c: u16| {
            schema
                .table(table)
                .and_then(|t| t.columns.get(c as usize))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col#{c}"))
        };
        match a {
            AccessPath::FullScan => "FullScan".into(),
            AccessPath::PkPoint(parts) => {
                let items: Vec<String> = schema
                    .table(table)
                    .map(|t| t.primary_key.clone())
                    .unwrap_or_default()
                    .iter()
                    .zip(parts)
                    .map(|(&c, p)| format!("{} = {}", col_name(c), self.render_part(p)))
                    .collect();
                format!("PkPoint({})", items.join(", "))
            }
            AccessPath::PkRange { lo, hi } => {
                let first = schema
                    .table(table)
                    .and_then(|t| t.primary_key.first().copied())
                    .unwrap_or(0);
                let mut items = Vec::new();
                if let Some(b) = lo {
                    let op = if b.inclusive { ">=" } else { ">" };
                    items.push(format!("{} {op} {}", col_name(first), self.render_part(&b.parts[0])));
                }
                if let Some(b) = hi {
                    let op = if b.inclusive { "<=" } else { "<" };
                    items.push(format!("{} {op} {}", col_name(first), self.render_part(&b.parts[0])));
                }
                format!("PkRange({})", items.join(", "))
            }
            AccessPath::IndexPoint { index_no, part } => {
                let col = (*index_no as usize)
                    .checked_sub(1)
                    .and_then(|i| {
                        schema
                            .table(table)
                            .map(crate::planner::secondary_indexes)
                            .unwrap_or_default()
                            .get(i)
                            .copied()
                    })
                    .unwrap_or(0);
                format!(
                    "IndexPoint({} = {}) via index {index_no}",
                    col_name(col),
                    self.render_part(part)
                )
            }
        }
    }
}

// ---- statement encode/decode ----------------------------------------------

fn w_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn encode_opt_program(p: Option<&ExprProgram>, buf: &mut Vec<u8>) {
    match p {
        None => buf.push(0),
        Some(p) => {
            buf.push(1);
            p.encode_into(buf);
        }
    }
}

fn decode_opt_program(buf: &[u8], pos: &mut usize) -> Result<Option<ExprProgram>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(ExprProgram::decode(buf, pos)?)),
        t => Err(corrupt(format!("bad optional-program tag {t}"))),
    }
}

fn encode_opt_u64(v: Option<u64>, buf: &mut Vec<u8>) {
    match v {
        None => buf.push(0),
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
}

fn decode_opt_u64(buf: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(r_u64(buf, pos)?)),
        t => Err(corrupt(format!("bad optional-u64 tag {t}"))),
    }
}

fn encode_part(p: &KeyPart, buf: &mut Vec<u8>) {
    match p {
        KeyPart::Param(i) => {
            buf.push(PART_PARAM);
            w_u16(buf, *i);
        }
        KeyPart::Const(i) => {
            buf.push(PART_CONST);
            w_u16(buf, *i);
        }
    }
}

fn decode_part(buf: &[u8], pos: &mut usize) -> Result<KeyPart> {
    let tag = r_u8(buf, pos)?;
    let i = r_u16(buf, pos)?;
    match tag {
        PART_PARAM => Ok(KeyPart::Param(i)),
        PART_CONST => Ok(KeyPart::Const(i)),
        t => Err(corrupt(format!("bad key part tag {t}"))),
    }
}

fn encode_parts(parts: &[KeyPart], buf: &mut Vec<u8>) {
    w_u16(buf, parts.len() as u16);
    for p in parts {
        encode_part(p, buf);
    }
}

fn decode_parts(buf: &[u8], pos: &mut usize) -> Result<Vec<KeyPart>> {
    let n = r_u16(buf, pos)? as usize;
    if n > MAX_COLUMNS {
        return Err(corrupt("too many key parts"));
    }
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        out.push(decode_part(buf, pos)?);
    }
    Ok(out)
}

fn encode_access(a: &AccessPath, buf: &mut Vec<u8>) {
    match a {
        AccessPath::FullScan => buf.push(ACCESS_FULL),
        AccessPath::PkPoint(parts) => {
            buf.push(ACCESS_PK_POINT);
            encode_parts(parts, buf);
        }
        AccessPath::PkRange { lo, hi } => {
            buf.push(ACCESS_PK_RANGE);
            for bound in [lo, hi] {
                match bound {
                    None => buf.push(0),
                    Some(b) => {
                        buf.push(1 | ((b.inclusive as u8) << 1));
                        encode_parts(&b.parts, buf);
                    }
                }
            }
        }
        AccessPath::IndexPoint { index_no, part } => {
            buf.push(ACCESS_INDEX_POINT);
            buf.extend_from_slice(&index_no.to_le_bytes());
            encode_part(part, buf);
        }
    }
}

fn decode_access(buf: &[u8], pos: &mut usize) -> Result<AccessPath> {
    match r_u8(buf, pos)? {
        ACCESS_FULL => Ok(AccessPath::FullScan),
        ACCESS_PK_POINT => Ok(AccessPath::PkPoint(decode_parts(buf, pos)?)),
        ACCESS_PK_RANGE => {
            let mut bounds = [None, None];
            for b in &mut bounds {
                let tag = r_u8(buf, pos)?;
                *b = match tag {
                    0 => None,
                    t if t & 1 == 1 && t & !3 == 0 => Some(KeyBound {
                        inclusive: t & 2 != 0,
                        parts: decode_parts(buf, pos)?,
                    }),
                    t => return Err(corrupt(format!("bad range bound tag {t}"))),
                };
            }
            let [lo, hi] = bounds;
            Ok(AccessPath::PkRange { lo, hi })
        }
        ACCESS_INDEX_POINT => {
            let index_no = r_u32(buf, pos)?;
            let part = decode_part(buf, pos)?;
            Ok(AccessPath::IndexPoint { index_no, part })
        }
        t => Err(corrupt(format!("bad access path tag {t}"))),
    }
}

const OC_ERROR: u8 = 0;
const OC_DO_NOTHING: u8 = 1;
const OC_DO_UPDATE: u8 = 2;

fn encode_projection(proj: &[Projection], buf: &mut Vec<u8>) {
    w_u16(buf, proj.len() as u16);
    for p in proj {
        match p {
            Projection::Column(i) => {
                buf.push(PROJ_COLUMN);
                w_u16(buf, *i);
            }
            Projection::Expr { program, name } => {
                buf.push(PROJ_EXPR);
                program.encode_into(buf);
                buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
                buf.extend_from_slice(name.as_bytes());
            }
        }
    }
}

fn decode_projection(buf: &[u8], pos: &mut usize) -> Result<Vec<Projection>> {
    let n = r_u16(buf, pos)? as usize;
    // Mirror the parse-time caps: a compliant encoder can never exceed them, so
    // a larger count is corruption or forgery. This blob can come from a hostile
    // shared-memory region.
    if n > crate::parser::MAX_SELECT_ITEMS {
        return Err(corrupt("too many RETURNING items in plan"));
    }
    let mut proj = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        proj.push(match r_u8(buf, pos)? {
            PROJ_COLUMN => Projection::Column(r_u16(buf, pos)?),
            PROJ_EXPR => {
                let program = ExprProgram::decode(buf, pos)?;
                let len = r_u32(buf, pos)? as usize;
                if len > 1 << 20 {
                    return Err(corrupt("projection name too long"));
                }
                let name = std::str::from_utf8(take(buf, pos, len)?)
                    .map_err(|_| corrupt("invalid utf-8 in projection name"))?
                    .to_string();
                Projection::Expr { program, name }
            }
            other => return Err(corrupt(format!("bad projection tag {other}"))),
        });
    }
    Ok(proj)
}

fn encode_opt_projection(proj: Option<&[Projection]>, buf: &mut Vec<u8>) {
    match proj {
        None => buf.push(0),
        Some(p) => {
            buf.push(1);
            encode_projection(p, buf);
        }
    }
}

fn decode_opt_projection(buf: &[u8], pos: &mut usize) -> Result<Option<Vec<Projection>>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(decode_projection(buf, pos)?)),
        other => Err(corrupt(format!("bad optional-projection tag {other}"))),
    }
}

fn encode_on_conflict(oc: &PlanOnConflict, buf: &mut Vec<u8>) {
    match oc {
        PlanOnConflict::Error => buf.push(OC_ERROR),
        PlanOnConflict::DoNothing => buf.push(OC_DO_NOTHING),
        PlanOnConflict::DoUpdate {
            target,
            probe,
            set,
            filter,
        } => {
            buf.push(OC_DO_UPDATE);
            match probe {
                ConflictProbe::Pk => buf.push(0),
                ConflictProbe::Index(n) => {
                    buf.push(1);
                    buf.extend_from_slice(&n.to_le_bytes());
                }
            }
            w_u16(buf, target.len() as u16);
            for c in target {
                w_u16(buf, *c);
            }
            w_u16(buf, set.len() as u16);
            for (c, program) in set {
                w_u16(buf, *c);
                program.encode_into(buf);
            }
            encode_opt_program(filter.as_ref(), buf);
        }
    }
}

fn decode_on_conflict(buf: &[u8], pos: &mut usize) -> Result<PlanOnConflict> {
    Ok(match r_u8(buf, pos)? {
        OC_ERROR => PlanOnConflict::Error,
        OC_DO_NOTHING => PlanOnConflict::DoNothing,
        OC_DO_UPDATE => {
            let probe = match r_u8(buf, pos)? {
                0 => ConflictProbe::Pk,
                1 => ConflictProbe::Index(r_u32(buf, pos)?),
                t => return Err(corrupt(format!("bad conflict-probe tag {t}"))),
            };
            let n = r_u16(buf, pos)? as usize;
            if n > crate::parser::MAX_SET_ITEMS {
                return Err(corrupt("too many conflict-target columns in plan"));
            }
            let mut target = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                target.push(r_u16(buf, pos)?);
            }
            let n = r_u16(buf, pos)? as usize;
            if n > crate::parser::MAX_SET_ITEMS {
                return Err(corrupt("too many SET assignments in plan"));
            }
            let mut set = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let c = r_u16(buf, pos)?;
                set.push((c, ExprProgram::decode(buf, pos)?));
            }
            let filter = decode_opt_program(buf, pos)?;
            PlanOnConflict::DoUpdate {
                target,
                probe,
                set,
                filter,
            }
        }
        other => return Err(corrupt(format!("bad ON CONFLICT tag {other}"))),
    })
}

fn encode_stmt(stmt: &PlanStmt, buf: &mut Vec<u8>) {
    match stmt {
        PlanStmt::Select {
            table,
            access,
            joins,
            joined_filter,
            filter,
            projection,
            order_by,
            order_over,
            limit,
            offset,
            aggregate,
            distinct,
            order_junk,
        } => {
            buf.push(STMT_SELECT);
            buf.extend_from_slice(&table.to_le_bytes());
            encode_access(access, buf);
            encode_opt_program(filter.as_ref(), buf);
            w_u16(buf, projection.len() as u16);
            for p in projection {
                match p {
                    Projection::Column(i) => {
                        buf.push(PROJ_COLUMN);
                        w_u16(buf, *i);
                    }
                    Projection::Expr { program, name } => {
                        buf.push(PROJ_EXPR);
                        program.encode_into(buf);
                        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
                        buf.extend_from_slice(name.as_bytes());
                    }
                }
            }
            buf.push(match order_over {
                OrderOver::BaseRow => 0u8,
                OrderOver::Grouped => 1,
                OrderOver::Projection => 2,
            });
            w_u16(buf, order_by.len() as u16);
            for (c, desc) in order_by {
                w_u16(buf, *c);
                buf.push(*desc as u8);
            }
            encode_opt_u64(*limit, buf);
            encode_opt_u64(*offset, buf);
            // A COUNT of joins where v6 wrote a single optional-join tag
            // (PLAN_FORMAT 7). `joined_filter` follows the chain, once, over the
            // full joined row.
            w_u16(buf, joins.len() as u16);
            for j in joins {
                buf.extend_from_slice(&j.table.to_le_bytes());
                encode_access(&j.access, buf);
                j.on.encode_into(buf);
                encode_opt_program(j.policy.as_ref(), buf);
            }
            encode_opt_program(joined_filter.as_ref(), buf);
            buf.push(*distinct as u8);
            w_u16(buf, *order_junk);
            match aggregate {
                None => buf.push(0),
                Some(a) => {
                    buf.push(1);
                    w_u16(buf, a.group_by.len() as u16);
                    for c in &a.group_by {
                        w_u16(buf, *c);
                    }
                    w_u16(buf, a.aggs.len() as u16);
                    for c in &a.aggs {
                        buf.push(c.func as u8);
                        buf.push(c.distinct as u8);
                        match &c.arg {
                            None => buf.push(0),
                            Some(p) => {
                                buf.push(1);
                                p.encode_into(buf);
                            }
                        }
                    }
                    encode_opt_program(a.having.as_ref(), buf);
                }
            }
        }
        PlanStmt::Insert {
            table,
            rows,
            with_check,
            on_conflict,
            returning,
        } => {
            buf.push(STMT_INSERT);
            buf.extend_from_slice(&table.to_le_bytes());
            w_u16(buf, rows.len() as u16);
            let width = rows.first().map_or(0, |r| r.len());
            w_u16(buf, width as u16);
            for row in rows {
                for src in row {
                    match src {
                        InsertSource::Param(i) => {
                            buf.push(SRC_PARAM);
                            w_u16(buf, *i);
                        }
                        InsertSource::Const(i) => {
                            buf.push(SRC_CONST);
                            w_u16(buf, *i);
                        }
                        InsertSource::Default => buf.push(SRC_DEFAULT),
                    }
                }
            }
            encode_opt_program(with_check.as_ref(), buf);
            encode_on_conflict(on_conflict, buf);
            encode_opt_projection(returning.as_deref(), buf);
        }
        PlanStmt::Update {
            table,
            access,
            filter,
            set,
            with_check,
            returning,
        } => {
            buf.push(STMT_UPDATE);
            buf.extend_from_slice(&table.to_le_bytes());
            encode_access(access, buf);
            encode_opt_program(filter.as_ref(), buf);
            w_u16(buf, set.len() as u16);
            for (c, program) in set {
                w_u16(buf, *c);
                program.encode_into(buf);
            }
            encode_opt_program(with_check.as_ref(), buf);
            encode_opt_projection(returning.as_deref(), buf);
        }
        PlanStmt::Delete {
            table,
            access,
            filter,
            returning,
        } => {
            buf.push(STMT_DELETE);
            buf.extend_from_slice(&table.to_le_bytes());
            encode_access(access, buf);
            encode_opt_program(filter.as_ref(), buf);
            encode_opt_projection(returning.as_deref(), buf);
        }
        PlanStmt::Begin => buf.push(STMT_BEGIN),
        PlanStmt::Commit => buf.push(STMT_COMMIT),
        PlanStmt::Rollback => buf.push(STMT_ROLLBACK),
    }
}

fn decode_stmt(buf: &[u8], pos: &mut usize) -> Result<PlanStmt> {
    match r_u8(buf, pos)? {
        STMT_SELECT => {
            let table = r_u32(buf, pos)?;
            let access = decode_access(buf, pos)?;
            let filter = decode_opt_program(buf, pos)?;
            let n_proj = r_u16(buf, pos)? as usize;
            // Mirror the parse-time caps (parser.rs): a compliant encoder can
            // never exceed them, so a larger count is corruption/forgery.
            if n_proj > crate::parser::MAX_SELECT_ITEMS {
                return Err(corrupt("too many projection items in plan"));
            }
            let mut projection = Vec::with_capacity(n_proj.min(1024));
            for _ in 0..n_proj {
                projection.push(match r_u8(buf, pos)? {
                    PROJ_COLUMN => Projection::Column(r_u16(buf, pos)?),
                    PROJ_EXPR => {
                        let program = ExprProgram::decode(buf, pos)?;
                        let len = r_u32(buf, pos)? as usize;
                        if len > 1 << 20 {
                            return Err(corrupt("projection name too long"));
                        }
                        let raw = take(buf, pos, len)?;
                        let name = std::str::from_utf8(raw)
                            .map_err(|_| corrupt("invalid utf-8 in projection name"))?
                            .to_owned();
                        Projection::Expr { program, name }
                    }
                    t => return Err(corrupt(format!("bad projection tag {t}"))),
                });
            }
            let order_over = match r_u8(buf, pos)? {
                0 => OrderOver::BaseRow,
                1 => OrderOver::Grouped,
                2 => OrderOver::Projection,
                t => return Err(corrupt(format!("bad order-over tag {t}"))),
            };
            let n_order = r_u16(buf, pos)? as usize;
            if n_order > crate::parser::MAX_ORDER_BY_ITEMS {
                return Err(corrupt("too many order-by items in plan"));
            }
            let mut order_by = Vec::with_capacity(n_order.min(1024));
            for _ in 0..n_order {
                let c = r_u16(buf, pos)?;
                let desc = match r_u8(buf, pos)? {
                    0 => false,
                    1 => true,
                    t => return Err(corrupt(format!("bad order direction {t}"))),
                };
                order_by.push((c, desc));
            }
            let limit = decode_opt_u64(buf, pos)?;
            let offset = decode_opt_u64(buf, pos)?;
            let njoins = r_u16(buf, pos)? as usize;
            if njoins > MAX_JOINS {
                return Err(corrupt("too many joins in plan"));
            }
            let mut joins = Vec::with_capacity(njoins.min(MAX_JOINS));
            for _ in 0..njoins {
                joins.push(Join {
                    table: r_u32(buf, pos)?,
                    access: decode_access(buf, pos)?,
                    on: ExprProgram::decode(buf, pos)?,
                    policy: decode_opt_program(buf, pos)?,
                });
            }
            let joined_filter = decode_opt_program(buf, pos)?;
            let distinct = match r_u8(buf, pos)? {
                0 => false,
                1 => true,
                t => return Err(corrupt(format!("bad distinct tag {t}"))),
            };
            let order_junk = r_u16(buf, pos)?;
            let aggregate = match r_u8(buf, pos)? {
                0 => None,
                1 => {
                    let n = r_u16(buf, pos)? as usize;
                    if n > crate::parser::MAX_ORDER_BY_ITEMS {
                        return Err(corrupt("too many GROUP BY items in plan"));
                    }
                    let mut group_by = Vec::with_capacity(n.min(64));
                    for _ in 0..n {
                        group_by.push(r_u16(buf, pos)?);
                    }
                    let n = r_u16(buf, pos)? as usize;
                    if n > crate::parser::MAX_SELECT_ITEMS {
                        return Err(corrupt("too many aggregates in plan"));
                    }
                    let mut aggs = Vec::with_capacity(n.min(64));
                    for _ in 0..n {
                        let f = AggFn::from_tag(r_u8(buf, pos)?)
                            .ok_or_else(|| corrupt("unknown aggregate function"))?;
                        let distinct = match r_u8(buf, pos)? {
                            0 => false,
                            1 => true,
                            t => return Err(corrupt(format!("bad aggregate distinct tag {t}"))),
                        };
                        let arg = match r_u8(buf, pos)? {
                            0 => None,
                            1 => Some(ExprProgram::decode(buf, pos)?),
                            t => return Err(corrupt(format!("bad aggregate arg tag {t}"))),
                        };
                        if distinct && arg.is_none() {
                            // count(DISTINCT *) has no meaning and the parser
                            // rejects it; a blob claiming it is corrupt.
                            return Err(corrupt("aggregate is DISTINCT but has no argument"));
                        }
                        aggs.push(AggCall {
                            func: f,
                            arg,
                            distinct,
                        });
                    }
                    let having = decode_opt_program(buf, pos)?;
                    Some(Aggregation {
                        group_by,
                        aggs,
                        having,
                    })
                }
                t => return Err(corrupt(format!("bad aggregate tag {t}"))),
            };
            Ok(PlanStmt::Select {
                table,
                access,
                joins,
                joined_filter,
                filter,
                projection,
                order_by,
                order_over,
                limit,
                offset,
                aggregate,
                distinct,
                order_junk,
            })
        }
        STMT_INSERT => {
            let table = r_u32(buf, pos)?;
            let n_rows = r_u16(buf, pos)? as usize;
            let width = r_u16(buf, pos)? as usize;
            if width > MAX_COLUMNS {
                return Err(corrupt("INSERT row width out of range"));
            }
            let mut rows = Vec::with_capacity(n_rows.min(1024));
            for _ in 0..n_rows {
                let mut row = Vec::with_capacity(width);
                for _ in 0..width {
                    row.push(match r_u8(buf, pos)? {
                        SRC_PARAM => InsertSource::Param(r_u16(buf, pos)?),
                        SRC_CONST => InsertSource::Const(r_u16(buf, pos)?),
                        SRC_DEFAULT => InsertSource::Default,
                        t => return Err(corrupt(format!("bad insert source tag {t}"))),
                    });
                }
                rows.push(row);
            }
            let with_check = decode_opt_program(buf, pos)?;
            let on_conflict = decode_on_conflict(buf, pos)?;
            let returning = decode_opt_projection(buf, pos)?;
            Ok(PlanStmt::Insert {
                table,
                rows,
                with_check,
                on_conflict,
                returning,
            })
        }
        STMT_UPDATE => {
            let table = r_u32(buf, pos)?;
            let access = decode_access(buf, pos)?;
            let filter = decode_opt_program(buf, pos)?;
            let n_set = r_u16(buf, pos)? as usize;
            if n_set > crate::parser::MAX_SET_ITEMS {
                return Err(corrupt("too many SET assignments in plan"));
            }
            let mut set = Vec::with_capacity(n_set.min(1024));
            for _ in 0..n_set {
                let c = r_u16(buf, pos)?;
                let program = ExprProgram::decode(buf, pos)?;
                set.push((c, program));
            }
            let with_check = decode_opt_program(buf, pos)?;
            let returning = decode_opt_projection(buf, pos)?;
            Ok(PlanStmt::Update {
                table,
                access,
                filter,
                set,
                with_check,
                returning,
            })
        }
        STMT_DELETE => {
            let table = r_u32(buf, pos)?;
            let access = decode_access(buf, pos)?;
            let filter = decode_opt_program(buf, pos)?;
            let returning = decode_opt_projection(buf, pos)?;
            Ok(PlanStmt::Delete {
                table,
                access,
                filter,
                returning,
            })
        }
        STMT_BEGIN => Ok(PlanStmt::Begin),
        STMT_COMMIT => Ok(PlanStmt::Commit),
        STMT_ROLLBACK => Ok(PlanStmt::Rollback),
        t => Err(corrupt(format!("bad statement tag {t}"))),
    }
}

// ---- expression decompiler (for EXPLAIN and projection names) -------------

/// Render a compiled expression back to a canonical infix string. Purely
/// cosmetic (EXPLAIN output and computed-column names) but deterministic, so
/// it is safe to embed in hashed plan bytes.
pub(crate) fn render_program(p: &ExprProgram, col: &dyn Fn(u16) -> String) -> String {
    // Control flow (CASE, lazy coalesce) cannot be rendered by walking the stack:
    // reconstructing the source shape from a flat program with jumps is
    // decompilation. Refuse up front rather than produce something plausible.
    //
    // The first attempt at this tracked the stack through the jumps and rendered
    // `coalesce(name, 'd')` as `'d'` — the constant from the last arm, presented
    // as the whole expression. That is worse than useless: EXPLAIN's whole job is
    // to tell you what will run, and a confident wrong answer there is a trap. An
    // honest marker at least sends you to the SQL.
    if p.instrs.iter().any(|i| {
        matches!(
            i,
            Instr::Jump(_) | Instr::JumpIfNotTrue(_) | Instr::JumpIfNotNull(_)
        )
    }) {
        return "<conditional>".to_string();
    }
    struct Item {
        s: String,
        atom: bool,
    }
    fn pop(st: &mut Vec<Item>) -> Item {
        st.pop().unwrap_or(Item {
            s: "?".into(),
            atom: true,
        })
    }
    fn wrap(i: &Item) -> String {
        if i.atom {
            i.s.clone()
        } else {
            format!("({})", i.s)
        }
    }
    let cst = |i: u16| {
        p.consts
            .get(i as usize)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    };
    let mut st: Vec<Item> = Vec::new();
    for &instr in &p.instrs {
        let item = match instr {
            Instr::PushCol(c) => Item {
                s: col(c),
                atom: true,
            },
            Instr::PushParam(i) => Item {
                s: format!("${}", i + 1),
                atom: true,
            },
            Instr::PushConst(i) => Item {
                s: cst(i),
                atom: true,
            },
            Instr::Neg => {
                let a = pop(&mut st);
                Item {
                    s: format!("-{}", wrap(&a)),
                    atom: false,
                }
            }
            Instr::Not => {
                let a = pop(&mut st);
                Item {
                    s: format!("NOT {}", wrap(&a)),
                    atom: false,
                }
            }
            Instr::IsNull => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} IS NULL", wrap(&a)),
                    atom: false,
                }
            }
            Instr::IsNotNull => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} IS NOT NULL", wrap(&a)),
                    atom: false,
                }
            }
            Instr::ToFloat => {
                let a = pop(&mut st);
                Item {
                    s: format!("float({})", a.s),
                    atom: true,
                }
            }
            Instr::Like(i) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} LIKE {}", wrap(&a), cst(i)),
                    atom: false,
                }
            }
            // `x IN ($n)` — a session-context list (§2.6). Pops the probe only.
            Instr::InParam(i) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} IN (${})", wrap(&a), i + 1),
                    atom: false,
                }
            }
            // `x IN (a, b, c)` — pops n elements and the probe beneath them.
            Instr::InList(n) => {
                let mut items: Vec<String> = (0..n).map(|_| pop(&mut st).s).collect();
                items.reverse();
                let a = pop(&mut st);
                Item {
                    s: format!("{} IN ({})", wrap(&a), items.join(", ")),
                    atom: false,
                }
            }
            // `f(a, b)` — pops argc arguments.
            Instr::Call(f, argc) => {
                let mut args: Vec<String> = (0..argc).map(|_| pop(&mut st).s).collect();
                args.reverse();
                Item {
                    s: format!("{}({})", f.name(), args.join(", ")),
                    atom: true,
                }
            }
            // Unreachable: a program containing jumps returned early above.
            Instr::Jump(_) | Instr::JumpIfNotTrue(_) | Instr::JumpIfNotNull(_) => continue,
            Instr::Pop => {
                let _ = pop(&mut st);
                continue;
            }
            _ => {
                let b = pop(&mut st);
                let a = pop(&mut st);
                let op = match instr {
                    Instr::Eq => "=",
                    Instr::Ne => "!=",
                    Instr::Lt => "<",
                    Instr::Le => "<=",
                    Instr::Gt => ">",
                    Instr::Ge => ">=",
                    Instr::Add => "+",
                    Instr::Sub => "-",
                    Instr::Mul => "*",
                    Instr::Div => "/",
                    Instr::Mod => "%",
                    Instr::And => "AND",
                    Instr::Or => "OR",
                    _ => "?",
                };
                Item {
                    s: format!("{} {op} {}", wrap(&a), wrap(&b)),
                    atom: false,
                }
            }
        };
        st.push(item);
    }
    pop(&mut st).s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::tests::test_schema;
    use crate::prepare;

    fn sample_sqls() -> Vec<&'static str> {
        vec![
            "SELECT * FROM users WHERE id = $1",
            "SELECT id, email, age + 1 FROM users WHERE age > 18 AND score < 2.5 ORDER BY email DESC LIMIT 10 OFFSET 5",
            "SELECT * FROM users WHERE id > 1 AND id <= $1",
            "SELECT * FROM users WHERE email = 'a@b' AND active",
            "SELECT * FROM orders WHERE user_id = 1 AND item_no = 2",
            "SELECT * FROM orders WHERE user_id = 7 AND note IS NOT NULL",
            "SELECT * FROM users WHERE email LIKE 'a%' OR NOT active",
            "INSERT INTO users (id, email) VALUES ($1, $2)",
            "INSERT INTO users (id, email, age) VALUES (1, 'a', NULL), (2, 'b', 3)",
            "INSERT INTO events (msg) VALUES (x'00ff')" ,
            "UPDATE users SET age = age + 1, score = 0.5 WHERE id = $1",
            "UPDATE users SET email = $1 WHERE email = $2",
            "DELETE FROM users WHERE id = 4",
            "DELETE FROM orders",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
        ]
    }

    #[test]
    fn roundtrip_every_sample() {
        let s = test_schema();
        for sql in sample_sqls() {
            if sql.contains("x'00ff'") {
                continue; // blob into text column: bind error, skipped here
            }
            let p = prepare(sql, &s).unwrap();
            let bytes = p.encode();
            let q = CompiledPlan::decode(&bytes, &s).expect(sql);
            assert_eq!(p, q, "roundtrip mismatch for {sql}");
            assert_eq!(p.hash(), q.hash(), "hash instability for {sql}");
        }
    }

    #[test]
    fn decode_rejects_wrong_schema() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id = 1", &s).unwrap();
        let bytes = p.encode();
        // A schema with one fewer table has a different hash.
        let other = Schema::new(vec![s.table(2).unwrap().clone()]).unwrap();
        assert!(matches!(
            CompiledPlan::decode(&bytes, &other),
            Err(Error::PlanInvalidated)
        ));
    }

    #[test]
    fn decode_rejects_truncation_everywhere() {
        let s = test_schema();
        let p = prepare(
            "SELECT id, age + 1 FROM users WHERE id > $1 ORDER BY email LIMIT 3",
            &s,
        )
        .unwrap();
        let bytes = p.encode();
        for cut in 0..bytes.len() {
            assert!(
                CompiledPlan::decode(&bytes[..cut], &s).is_err(),
                "truncation at {cut} must fail"
            );
        }
    }

    #[test]
    fn tampered_footprint_byte_is_rejected() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id = $1", &s).unwrap();
        let bytes = p.encode();
        // Footprint starts right after: format(1) + schema(32) + nparams(2)
        // + param tags(n) + context_keys count(2, none here)
        // + npolicies(2) + npolicies * (table 4 + epoch 8 + hash 32)
        // + nconsts(2) + consts.
        assert!(p.context_keys.is_empty());
        let mut off =
            1 + 32 + 2 + p.param_types.len() + 2 + 2 + p.policies.len() * (4 + 8 + 32) + 2;
        for c in &p.consts {
            let mut tmp = Vec::new();
            write_value(&mut tmp, c);
            off += tmp.len();
        }
        // Flip the low bit of tables_read: decode must catch the forgery.
        let mut evil = bytes.clone();
        evil[off] ^= 1;
        match CompiledPlan::decode(&evil, &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("footprint"), "{m}"),
            other => panic!("expected footprint corruption error, got {other:?}"),
        }
        // Flip read_only (offset +24): rejected as inconsistent.
        let mut evil = bytes.clone();
        evil[off + 24] ^= 1;
        assert!(CompiledPlan::decode(&evil, &s).is_err());
    }

    #[test]
    fn tampered_semantics_are_rejected() {
        let s = test_schema();
        // Build a hand-corrupted plan: valid structure, PK-column SET.
        let p = prepare("UPDATE users SET age = 1 WHERE id = 1", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Update { set, .. } => set[0].0 = 0, // id is the PK
            _ => unreachable!(),
        }
        let bytes = evil.encode();
        assert!(CompiledPlan::decode(&bytes, &s).is_err());

        // Out-of-range table id.
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Update { table, .. } => *table = 63,
            _ => unreachable!(),
        }
        assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

        // PkPoint with the wrong arity.
        let p = prepare("SELECT * FROM orders WHERE user_id = 1 AND item_no = 2", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { access, .. } => {
                *access = AccessPath::PkPoint(vec![KeyPart::Const(0)]);
            }
            _ => unreachable!(),
        }
        assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

        // Const index out of range inside a key part.
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { access, .. } => {
                *access = AccessPath::PkPoint(vec![KeyPart::Const(60000), KeyPart::Const(1)]);
            }
            _ => unreachable!(),
        }
        assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());

        // Param index beyond n_params inside a program.
        let p = prepare("SELECT * FROM users WHERE age > $1", &s).unwrap();
        let mut evil = p.clone();
        evil.param_types.clear(); // n_params -> 0 on re-encode
        assert!(CompiledPlan::decode(&evil.encode(), &s).is_err());
    }

    /// A tampered blob whose sort key indexes past the tuple it claims to
    /// order must be rejected at decode.
    ///
    /// This is why `order_over` is a field rather than something inferred from
    /// `distinct`/`aggregate`: the decoder has to know WHICH tuple to bound
    /// against, and the failure is quiet if it guesses wrong — `cmp_rows` skips
    /// a key it cannot fetch, so an out-of-range index does not crash, it drops
    /// the sort and answers an ORDER BY query in arbitrary order.
    #[test]
    fn order_by_index_is_bounded_against_the_tuple_it_orders() {
        let s = test_schema();
        // `SELECT DISTINCT email` projects ONE column but the table has more,
        // so index 1 is in range for the base row and out of range for the
        // projection. Bounding against the wrong one accepts this.
        let p = prepare("SELECT DISTINCT email FROM users ORDER BY email", &s).unwrap();
        match &p.stmt {
            PlanStmt::Select {
                order_over,
                projection,
                ..
            } => {
                assert_eq!(*order_over, OrderOver::Projection);
                assert_eq!(projection.len(), 1);
            }
            _ => unreachable!(),
        }
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { order_by, .. } => order_by[0].0 = 1,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("order-by column"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // And a plain Select cannot claim to order a grouped tuple it has not
        // got.
        let p = prepare("SELECT id FROM users ORDER BY id", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { order_over, .. } => *order_over = OrderOver::Grouped,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("grouped"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    /// Sort-only columns are trimmed by the executor on the strength of a
    /// COUNT in the plan. A tampered count is therefore a way to make the
    /// executor trim real output, or to smuggle junk past a DISTINCT where it
    /// would dedup on a value the caller never sees. Decode must refuse all
    /// three shapes.
    /// A tampered plan claiming "target (email), probe pk" would find a row by
    /// PRIMARY KEY and update it as if it were the email conflict — the wrong
    /// row, no error, no crash. Decode recomputes the probe from the target and
    /// refuses a mismatch.
    /// An aggregate over a join groups the JOINED row, so its GROUP BY slots
    /// and aggregate arguments are bounded by the joined width — not the outer
    /// table's, which is narrower and would reject a legitimate plan, and not
    /// nothing, which would let a hostile one read past the tuple.
    #[test]
    fn aggregate_over_a_join_is_bounded_by_the_joined_width() {
        let s = test_schema();
        let p = prepare(
            "SELECT count(*) FROM orders JOIN users ON orders.user_id = users.id \
             GROUP BY users.email",
            &s,
        )
        .unwrap();
        let (outer_w, joined_w) = match &p.stmt {
            PlanStmt::Select {
                table,
                joins,
                aggregate: Some(a),
                ..
            } if !joins.is_empty() => {
                let j = &joins[0];
                let o = s.table(*table).unwrap().columns.len();
                let i = s.table(j.table).unwrap().columns.len();
                // `users.email` is column 1 of users, which sits after all of
                // orders' columns in the joined row — a slot no single-table
                // bound would accept.
                assert_eq!(a.group_by, vec![(o + 1) as u16]);
                (o, o + i)
            }
            other => panic!("expected a joined aggregate plan, got {other:?}"),
        };
        assert!(joined_w > outer_w, "the join must widen the row");
        // Round-trips.
        CompiledPlan::decode(&p.encode(), &s).unwrap();

        // One past the joined row is out of range.
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select {
                aggregate: Some(a), ..
            } => a.group_by[0] = joined_w as u16,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("GROUP BY column"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn conflict_probe_must_match_its_target() {
        let s = test_schema();
        let p = prepare(
            "INSERT INTO users (id, email) VALUES ($1, $2) \
             ON CONFLICT (email) DO UPDATE SET email = excluded.email",
            &s,
        )
        .unwrap();
        match &p.stmt {
            PlanStmt::Insert {
                on_conflict: PlanOnConflict::DoUpdate { probe, .. },
                ..
            } => assert!(
                matches!(probe, ConflictProbe::Index(_)),
                "email is a secondary unique column, got {probe:?}"
            ),
            other => panic!("expected an upsert plan, got {other:?}"),
        }

        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Insert {
                on_conflict: PlanOnConflict::DoUpdate { probe, .. },
                ..
            } => *probe = ConflictProbe::Pk,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("probe"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // And the reverse: a PK target cannot claim an index probe.
        let p = prepare(
            "INSERT INTO users (id, email) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET email = excluded.email",
            &s,
        )
        .unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Insert {
                on_conflict: PlanOnConflict::DoUpdate { probe, .. },
                ..
            } => *probe = ConflictProbe::Index(1),
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("probe"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn order_junk_count_is_validated() {
        let s = test_schema();
        let p = prepare("SELECT id FROM users ORDER BY email", &s).unwrap();
        match &p.stmt {
            PlanStmt::Select {
                order_junk,
                order_over,
                projection,
                ..
            } => {
                // The key is a plain column, so it sorts the base row and needs
                // no junk column at all.
                assert_eq!(*order_junk, 0);
                assert_eq!(*order_over, OrderOver::BaseRow);
                assert_eq!(projection.len(), 1);
            }
            _ => unreachable!(),
        }

        // (a) junk without a projection sort: nothing would ever trim it.
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { order_junk, .. } => *order_junk = 1,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("projection sort"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // (b) junk that eats the entire output.
        let p2 = prepare("SELECT id FROM users ORDER BY email + 1", &s);
        // `email` is text; if that does not bind, use a numeric key instead.
        let p2 = match p2 {
            Ok(p) => p,
            Err(_) => prepare("SELECT email FROM users ORDER BY id + 1", &s).unwrap(),
        };
        match &p2.stmt {
            PlanStmt::Select {
                order_junk,
                order_over,
                projection,
                ..
            } => {
                assert_eq!(*order_junk, 1, "a computed key needs a sort-only column");
                assert_eq!(*order_over, OrderOver::Projection);
                assert_eq!(projection.len(), 2, "one output + one sort-only");
            }
            _ => unreachable!(),
        }
        let mut evil = p2.clone();
        match &mut evil.stmt {
            PlanStmt::Select { order_junk, .. } => *order_junk = 2,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("no output"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        // (c) junk under DISTINCT.
        let mut evil = p2.clone();
        match &mut evil.stmt {
            PlanStmt::Select { distinct, .. } => *distinct = true,
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("DISTINCT"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn oversized_counts_in_plan_bytes_are_rejected() {
        let s = test_schema();

        // The parse-time caps make prepare() refuse oversized statements, so
        // hand-build oversized plans in memory: their encodings are exactly
        // what a tampered registry blob would look like, and decode must
        // reject the count before trusting it.
        let p = prepare("SELECT id FROM users", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { projection, .. } => {
                let item = projection[0].clone();
                while projection.len() <= crate::parser::MAX_SELECT_ITEMS {
                    projection.push(item.clone());
                }
            }
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("projection items"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        let p = prepare("SELECT id FROM users ORDER BY email", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Select { order_by, .. } => {
                assert!(!order_by.is_empty());
                let item = order_by[0];
                while order_by.len() <= crate::parser::MAX_ORDER_BY_ITEMS {
                    order_by.push(item);
                }
            }
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("order-by"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }

        let p = prepare("UPDATE users SET age = 1 WHERE id = 1", &s).unwrap();
        let mut evil = p.clone();
        match &mut evil.stmt {
            PlanStmt::Update { set, .. } => {
                let item = set[0].clone();
                while set.len() <= crate::parser::MAX_SET_ITEMS {
                    set.push(item.clone());
                }
            }
            _ => unreachable!(),
        }
        match CompiledPlan::decode(&evil.encode(), &s) {
            Err(Error::Corrupt(m)) => assert!(m.contains("SET assignments"), "{m}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn explain_is_informative() {
        let s = test_schema();
        let p = prepare(
            "SELECT id, age + 1 FROM users WHERE id = $1 AND age > 18 LIMIT 3",
            &s,
        )
        .unwrap();
        let e = p.explain(&s);
        assert!(e.contains("Select users"), "{e}");
        assert!(e.contains("PkPoint(id = $1)"), "{e}");
        assert!(e.contains("filter: age > 18"), "{e}");
        assert!(e.contains("project: id, age + 1"), "{e}");
        assert!(e.contains("limit: 3"), "{e}");
        assert!(e.contains("read_only=true"), "{e}");

        let p = prepare("SELECT * FROM users WHERE id > 1 AND id <= $1", &s).unwrap();
        let e = p.explain(&s);
        assert!(e.contains("PkRange(id > 1, id <= $1)"), "{e}");

        let p = prepare("SELECT * FROM users WHERE email = 'x'", &s).unwrap();
        let e = p.explain(&s);
        assert!(e.contains("IndexPoint(email = 'x') via index 1"), "{e}");

        let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a')", &s).unwrap();
        let e = p.explain(&s);
        assert!(e.contains("Insert users"), "{e}");
        assert!(e.contains("id = 1"), "{e}");
        assert!(e.contains("email = 'a'"), "{e}");
        assert!(e.contains("created = DEFAULT"), "{e}");

        let p = prepare("UPDATE users SET age = age + 1 WHERE id = 2", &s).unwrap();
        let e = p.explain(&s);
        assert!(e.contains("Update users"), "{e}");
        assert!(e.contains("set: age = age + 1"), "{e}");

        let p = prepare("DELETE FROM users", &s).unwrap();
        let e = p.explain(&s);
        assert!(e.contains("Delete users"), "{e}");
        assert!(e.contains("FullScan"), "{e}");

        assert!(prepare("BEGIN", &s).unwrap().explain(&s).contains("Begin"));
    }

    #[test]
    fn projection_names_are_canonical() {
        let s = test_schema();
        let a = prepare("SELECT age+1 FROM users", &s).unwrap();
        // Identifiers are case-sensitive: AGE does not exist.
        assert!(matches!(
            prepare("select AGE + 1 from users", &s),
            Err(Error::Bind(_))
        ));
        // Whitespace and keyword case do not affect the plan or the name.
        let b = prepare("select age\n  +\n1 from users", &s).unwrap();
        assert_eq!(a, b);
        match &a.stmt {
            PlanStmt::Select { projection, .. } => match &projection[0] {
                Projection::Expr { name, .. } => assert_eq!(name, "age + 1"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use mpedb_types::{ScalarFn, Value};

    fn r(instrs: Vec<Instr>, consts: Vec<Value>) -> String {
        let p = ExprProgram::new(instrs, consts).unwrap();
        render_program(&p, &|c| format!("c{c}"))
    }

    /// EXPLAIN and SELECT column names both render the COMPILED program, so an
    /// instruction the renderer does not know does not merely look odd — the old
    /// catch-all treated every unknown op as a binary operator and popped TWO
    /// operands, corrupting the stack for everything after it. `lower(name)`
    /// came out as `? ? name`.
    #[test]
    fn every_instruction_renders_without_eating_the_stack() {
        assert_eq!(
            r(vec![Instr::PushCol(0), Instr::Call(ScalarFn::Lower, 1)], vec![]),
            "lower(c0)"
        );
        assert_eq!(
            r(
                vec![
                    Instr::PushCol(0),
                    Instr::PushConst(0),
                    Instr::PushConst(1),
                    Instr::Call(ScalarFn::Substr, 3)
                ],
                vec![Value::Int(1), Value::Int(2)]
            ),
            "substr(c0, 1, 2)"
        );
        assert_eq!(
            r(
                vec![
                    Instr::PushCol(0),
                    Instr::PushConst(0),
                    Instr::PushConst(1),
                    Instr::InList(2)
                ],
                vec![Value::Int(1), Value::Int(2)]
            ),
            "c0 IN (1, 2)"
        );
        assert_eq!(
            r(vec![Instr::PushCol(0), Instr::InParam(0)], vec![]),
            "c0 IN ($1)"
        );
    }

    /// A program with jumps cannot be rendered by walking the stack — that is
    /// decompilation. The first attempt tried, and rendered
    /// `coalesce(name, 'd')` as `'d'`: the last arm's constant, presented as the
    /// whole expression. EXPLAIN exists to tell you what will run, so a
    /// confident wrong answer there is worse than no answer.
    #[test]
    fn control_flow_renders_as_an_honest_marker_not_a_plausible_lie() {
        // coalesce(c0, 'd') exactly as the binder emits it:
        //   0 PushCol          depth 1
        //   1 JumpIfNotNull(4) peeks -> jumps to the END with the value still on
        //   2 Pop              the NULL is discarded
        //   3 PushConst('d')   depth 1 again, so both paths agree at 4
        // Writing JumpIfNotNull(3) instead was rejected outright by the
        // verifier ("stack depth disagrees at instruction 3") — which is the
        // depth analysis earning its keep on a hand-written program.
        let out = r(
            vec![
                Instr::PushCol(0),
                Instr::JumpIfNotNull(4),
                Instr::Pop,
                Instr::PushConst(0),
            ],
            vec![Value::Text("d".into())],
        );
        assert_eq!(out, "<conditional>");
        // The old renderer produced exactly `'d'` here — the last arm's constant,
        // presented as the whole expression.
        assert!(
            !out.contains("'d'"),
            "must not present one arm as the whole expression, got {out}"
        );
    }
}
