//! Compiled plans: the self-contained, deterministically serializable output
//! of `prepare()`. Other processes execute plans straight from these bytes,
//! so `decode` treats its input as hostile: every read is bounds-checked and
//! the decoded plan is re-validated against the schema, including a full
//! footprint recomputation.

use crate::planner;
use mpedb_types::value::{read_value, write_value};
use mpedb_types::{
    ColumnType, Error, ExprProgram, Footprint, Instr, KeyBound, KeyPart, PlanHash, Result, Schema,
    TableDef, Value, FORMAT_VERSION, MAX_COLUMNS,
};

/// Leading byte of the plan wire format (independent of [`FORMAT_VERSION`],
/// which is mixed into the hash). Bumped to 2 for the reserved session-context
/// parameter slots (DESIGN-MULTIDB.md §2); an older blob fails `decode` closed.
const PLAN_FORMAT: u8 = 2;

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
    /// The target table's RLS `pol_epoch` at compile time (Phase-5 leak-proofing,
    /// DESIGN-MULTIDB.md §4). Fast-path staleness check: if the live epoch still
    /// equals this, the baked policy is current.
    pub policy_epoch: u64,
    /// Content hash of the target table's applicable policy set at compile time
    /// ([`table_policy_hash`](crate::table_policy_hash)). When the epoch moved,
    /// a matching recomputed hash still proves the plan is current (a no-op edit);
    /// a mismatch is stale ⇒ `PlanInvalidated`.
    pub policy_hash: [u8; 32],
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
        /// Residual predicate after access-path extraction.
        filter: Option<ExprProgram>,
        /// Output columns, in order.
        projection: Vec<Projection>,
        /// (table column index, descending). Empty = scan order.
        order_by: Vec<(u16, bool)>,
        limit: Option<u64>,
        offset: Option<u64>,
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
        /// column index -> value expression, evaluated over the EXISTING row
        /// concatenated with the PROPOSED row (`excluded.<c>` = `Col(n + i)`).
        set: Vec<(u16, ExprProgram)>,
        /// Optional `WHERE` on the update, over the same doubled row.
        filter: Option<ExprProgram>,
    },
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
        buf.extend_from_slice(&self.policy_epoch.to_le_bytes());
        buf.extend_from_slice(&self.policy_hash);
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
        let policy_epoch = r_u64(bytes, &mut pos)?;
        let mut policy_hash = [0u8; 32];
        policy_hash.copy_from_slice(take(bytes, &mut pos, 32)?);
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
            policy_epoch,
            policy_hash,
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
                filter,
                projection,
                order_by,
                ..
            } => {
                let t = get_table(*table)?;
                self.check_access(access, t)?;
                if let Some(f) = filter {
                    self.check_program(f, t)?;
                }
                for p in projection {
                    match p {
                        Projection::Column(i) => {
                            if *i as usize >= t.columns.len() {
                                return Err(corrupt("projection column out of range"));
                            }
                        }
                        Projection::Expr { program, .. } => self.check_program(program, t)?,
                    }
                }
                for (c, _) in order_by {
                    if *c as usize >= t.columns.len() {
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
                    PlanOnConflict::DoUpdate { target, set, filter } => {
                        if target.is_empty() {
                            return Err(corrupt("ON CONFLICT DO UPDATE with no target"));
                        }
                        for c in target {
                            if *c as usize >= t.columns.len() {
                                return Err(corrupt("conflict-target column out of range"));
                            }
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
                filter,
                projection,
                order_by,
                limit,
                offset,
            } => {
                let name = col_namer(*table);
                out.push_str(&format!("Select {}\n", table_name(*table)));
                out.push_str(&format!(
                    "  access: {}\n",
                    self.render_access(access, schema, *table)
                ));
                if let Some(f) = filter {
                    out.push_str(&format!("  filter: {}\n", render_program(f, &name)));
                }
                let cols: Vec<String> = projection
                    .iter()
                    .map(|p| match p {
                        Projection::Column(i) => name(*i),
                        Projection::Expr { name, .. } => name.clone(),
                    })
                    .collect();
                out.push_str(&format!("  project: {}\n", cols.join(", ")));
                if !order_by.is_empty() {
                    let items: Vec<String> = order_by
                        .iter()
                        .map(|(c, desc)| {
                            format!("{}{}", name(*c), if *desc { " DESC" } else { " ASC" })
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
                    PlanOnConflict::DoUpdate { target, set, filter } => {
                        let cols: Vec<String> = target.iter().map(|c| name(*c)).collect();
                        out.push_str(&format!("  on conflict ({}): do update\n", cols.join(", ")));
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
        PlanOnConflict::DoUpdate { target, set, filter } => {
            buf.push(OC_DO_UPDATE);
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
            PlanOnConflict::DoUpdate { target, set, filter }
        }
        other => return Err(corrupt(format!("bad ON CONFLICT tag {other}"))),
    })
}

fn encode_stmt(stmt: &PlanStmt, buf: &mut Vec<u8>) {
    match stmt {
        PlanStmt::Select {
            table,
            access,
            filter,
            projection,
            order_by,
            limit,
            offset,
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
            w_u16(buf, order_by.len() as u16);
            for (c, desc) in order_by {
                w_u16(buf, *c);
                buf.push(*desc as u8);
            }
            encode_opt_u64(*limit, buf);
            encode_opt_u64(*offset, buf);
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
            Ok(PlanStmt::Select {
                table,
                access,
                filter,
                projection,
                order_by,
                limit,
                offset,
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
        // + param tags(n) + context_keys count(2, none here) + policy_epoch(8)
        // + policy_hash(32) + nconsts(2) + consts.
        assert!(p.context_keys.is_empty());
        let mut off = 1 + 32 + 2 + p.param_types.len() + 2 + 8 + 32 + 2;
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
