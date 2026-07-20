use super::*;

pub(super) fn corrupt(msg: impl Into<String>) -> Error {
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

/// Decode an ORDER-BY collating sequence: tags 0..=2 are the built-ins,
/// exactly as before format 52; tag 3 introduces a HOST collation's name.
fn r_collation(buf: &[u8], pos: &mut usize) -> Result<OrderColl> {
    match r_u8(buf, pos)? {
        3 => {
            let name = r_str(buf, pos)?;
            if name.is_empty() {
                return Err(corrupt("host collation with an empty name"));
            }
            Ok(OrderColl::Host(name))
        }
        t => Collation::from_tag(t)
            .map(OrderColl::Native)
            .ok_or_else(|| corrupt("bad collation tag")),
    }
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
    /// Decode and fully re-validate a plan blob against `schema`. The input
    /// may come from a corrupt or hostile shared-memory region: every read is
    /// bounds-checked, all indices are range-checked against the schema, and
    /// the embedded footprint must equal a freshly recomputed one.
    pub fn decode(bytes: &[u8], schema: &Schema) -> Result<CompiledPlan> {
        let mut pos = 0usize;
        let format = r_u8(bytes, &mut pos)?;
        if format != PLAN_FORMAT {
            // A plausible other VERSION (either direction — an old client blob
            // against a newer binary, or an old binary against a newer shared
            // registry) is DRIFT, not tampering: the caller should re-prepare
            // (the documented PlanInvalidated path), not be told its blob is
            // corrupt. A byte outside the version window is not a plan at all.
            return Err(if (1..64).contains(&format) {
                Error::PlanInvalidated
            } else {
                corrupt(format!("unknown plan format {format}"))
            });
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
        // Layout: [user ‖ subplan results ‖ context] — context slots are the
        // LAST n_context entries whatever sits before them.
        let ctx_base = n_params as usize - n_context;
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
            if param_types[ctx_base + p].is_none() {
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
        let n_sub = r_u8(bytes, &mut pos)? as usize;
        if n_sub > MAX_SUBPLANS {
            return Err(corrupt("too many subplans in plan"));
        }
        if n_sub + n_context > n_params as usize {
            return Err(corrupt("more reserved slots than parameters"));
        }
        // `MAX_SUBPLANS` bounds the WHOLE tree (top + every nesting level), so a
        // forged blob cannot make the decoder allocate an unbounded subplan
        // forest — the DoS bound the flat format had, kept under recursion.
        let mut budget = MAX_SUBPLANS;
        let mut subplans = Vec::with_capacity(n_sub);
        for _ in 0..n_sub {
            subplans.push(decode_subplan(bytes, &mut pos, &mut budget)?);
        }
        let footprint = Footprint::decode(bytes, &mut pos)?;
        // ONE tree budget for the WHOLE plan: statement-level lifts, a derived
        // body's owned lifts (format 52) and a compound's arm-owned lifts
        // (format 56) all draw from it, so moving lifts from the statement list
        // onto a component can never buy a bigger forest than the flat format
        // allowed.
        let stmt = decode_stmt(bytes, &mut pos, &mut budget)?;
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes in plan"));
        }
        let plan = CompiledPlan {
            stmt,
            schema_hash,
            n_params,
            param_types,
            subplans,
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
}

/// One lifted subquery, RECURSIVELY (#73 §3) — the exact mirror of
/// [`encode_subplan`]. `budget` is decremented per subplan (top AND nested) and
/// bounds the whole tree at `MAX_SUBPLANS`, so a hostile blob cannot balloon the
/// subplan forest.
fn decode_subplan(buf: &[u8], pos: &mut usize, budget: &mut usize) -> Result<SubPlan> {
    if *budget == 0 {
        return Err(corrupt("too many subplans in plan"));
    }
    *budget -= 1;
    let kind =
        SubPlanKind::from_tag(r_u8(buf, pos)?).ok_or_else(|| corrupt("bad subplan kind tag"))?;
    let sub_base = r_u16(buf, pos)?;
    let slot_type = match r_u8(buf, pos)? {
        0 => None,
        tag => Some(
            ColumnType::from_tag(tag)
                .ok_or_else(|| corrupt(format!("bad subplan slot type tag {tag}")))?,
        ),
    };
    let n_args = r_u16(buf, pos)? as usize;
    if n_args > MAX_COLUMNS {
        return Err(corrupt("too many subplan correlation args"));
    }
    let mut outer_args = Vec::with_capacity(n_args.min(64));
    for _ in 0..n_args {
        outer_args.push(r_u16(buf, pos)?);
    }
    // The body-discriminant byte (format 31): a plain SELECT or a whole compound.
    let body = match r_u8(buf, pos)? {
        SUBBODY_SELECT => SubBody::Select(decode_select(buf, pos)?),
        SUBBODY_COMPOUND => SubBody::Compound(decode_compound(buf, pos, budget)?),
        t => return Err(corrupt(format!("bad subplan body tag {t}"))),
    };
    let n_children = r_u8(buf, pos)? as usize;
    let mut subplans = Vec::with_capacity(n_children.min(MAX_SUBPLANS));
    for _ in 0..n_children {
        subplans.push(decode_subplan(buf, pos, budget)?);
    }
    Ok(SubPlan {
        body,
        outer_args,
        kind,
        subplans,
        sub_base,
        slot_type,
    })
}

fn decode_opt_program(buf: &[u8], pos: &mut usize) -> Result<Option<ExprProgram>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(ExprProgram::decode(buf, pos)?)),
        t => Err(corrupt(format!("bad optional-program tag {t}"))),
    }
}

fn decode_opt_u64(buf: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(r_u64(buf, pos)?)),
        t => Err(corrupt(format!("bad optional-u64 tag {t}"))),
    }
}

fn decode_part(buf: &[u8], pos: &mut usize) -> Result<KeyPart> {
    let tag = r_u8(buf, pos)?;
    let i = r_u16(buf, pos)?;
    match tag {
        PART_PARAM => Ok(KeyPart::Param(i)),
        PART_CONST => Ok(KeyPart::Const(i)),
        PART_OUTER_COL => Ok(KeyPart::OuterCol(i)),
        t => Err(corrupt(format!("bad key part tag {t}"))),
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
            let parts = decode_parts(buf, pos)?;
            Ok(AccessPath::IndexPoint { index_no, parts })
        }
        ACCESS_INDEX_RANGE => {
            let index_no = r_u32(buf, pos)?;
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
            Ok(AccessPath::IndexRange { index_no, lo, hi })
        }
        ACCESS_FTS_SCAN => {
            let mut budget = MAX_FTS_DEPTH;
            Ok(AccessPath::FtsScan { query: decode_fts_query(buf, pos, &mut budget)? })
        }
        t => Err(corrupt(format!("bad access path tag {t}"))),
    }
}

/// One FTS query node, RECURSIVELY (design/DESIGN-FTS.md §3) — the exact mirror
/// of `encode_fts_query`. `budget` bounds the tree depth at [`MAX_FTS_DEPTH`], so
/// a hostile blob cannot overflow the stack. Every read is bounds-checked; a bad
/// tag or a truncated token is [`Error::Corrupt`], never a panic.
fn decode_fts_query(buf: &[u8], pos: &mut usize, budget: &mut usize) -> Result<FtsQuery> {
    if *budget == 0 {
        return Err(corrupt("FTS query tree too deep"));
    }
    *budget -= 1;
    match r_u8(buf, pos)? {
        FTS_TERM => {
            let len = r_u32(buf, pos)? as usize;
            if len > 1 << 20 {
                return Err(corrupt("FTS term too long"));
            }
            let token = std::str::from_utf8(take(buf, pos, len)?)
                .map_err(|_| corrupt("invalid utf-8 in FTS term"))?
                .to_string();
            let prefix = match r_u8(buf, pos)? {
                0 => false,
                1 => true,
                t => return Err(corrupt(format!("bad FTS prefix flag {t}"))),
            };
            let initial = match r_u8(buf, pos)? {
                0 => false,
                1 => true,
                t => return Err(corrupt(format!("bad FTS initial flag {t}"))),
            };
            let n = r_u16(buf, pos)? as usize;
            if n > MAX_COLUMNS {
                return Err(corrupt("too many FTS term columns"));
            }
            let mut columns = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                columns.push(r_u16(buf, pos)?);
            }
            // A bare (empty-token) term is never emitted; reject it so the
            // executor never scans an all-columns empty prefix.
            if token.is_empty() {
                return Err(corrupt("empty FTS term"));
            }
            Ok(FtsQuery::Term(FtsTerm { token, prefix, initial, columns }))
        }
        FTS_AND => {
            let a = decode_fts_query(buf, pos, budget)?;
            let b = decode_fts_query(buf, pos, budget)?;
            Ok(FtsQuery::And(Box::new(a), Box::new(b)))
        }
        FTS_OR => {
            let a = decode_fts_query(buf, pos, budget)?;
            let b = decode_fts_query(buf, pos, budget)?;
            Ok(FtsQuery::Or(Box::new(a), Box::new(b)))
        }
        FTS_AND_NOT => {
            let a = decode_fts_query(buf, pos, budget)?;
            let b = decode_fts_query(buf, pos, budget)?;
            Ok(FtsQuery::AndNot(Box::new(a), Box::new(b)))
        }
        t => Err(corrupt(format!("bad FTS query node tag {t}"))),
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

fn decode_opt_projection(buf: &[u8], pos: &mut usize) -> Result<Option<Vec<Projection>>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => Ok(Some(decode_projection(buf, pos)?)),
        other => Err(corrupt(format!("bad optional-projection tag {other}"))),
    }
}

fn decode_on_conflict(buf: &[u8], pos: &mut usize) -> Result<PlanOnConflict> {
    Ok(match r_u8(buf, pos)? {
        OC_ERROR => PlanOnConflict::Error,
        OC_DO_NOTHING => PlanOnConflict::DoNothing,
        OC_REPLACE => PlanOnConflict::Replace,
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

fn decode_stmt(buf: &[u8], pos: &mut usize, budget: &mut usize) -> Result<PlanStmt> {
    match r_u8(buf, pos)? {
        STMT_SELECT => Ok(PlanStmt::Select(decode_select(buf, pos)?)),
        STMT_COMPOUND => Ok(PlanStmt::Compound(decode_compound(buf, pos, budget)?)),
        STMT_RECURSIVE_CTE => {
            let name = r_str(buf, pos)?;
            let n_cols = r_u16(buf, pos)? as usize;
            // At least one column (the required `t(c1, …)` list); capped like
            // any projection so a forged count cannot balloon the allocation.
            if n_cols == 0 || n_cols > MAX_COLUMNS {
                return Err(corrupt("recursive CTE column count out of range"));
            }
            let mut columns = Vec::with_capacity(n_cols.min(1024));
            let mut col_types = Vec::with_capacity(n_cols.min(1024));
            for _ in 0..n_cols {
                columns.push(r_str(buf, pos)?);
                let tag = r_u8(buf, pos)?;
                col_types.push(
                    ColumnType::from_tag(tag)
                        .ok_or_else(|| corrupt(format!("bad recursive CTE column type tag {tag}")))?,
                );
            }
            let union_all = match r_u8(buf, pos)? {
                0 => false,
                1 => true,
                t => return Err(corrupt(format!("bad recursive CTE union_all byte {t}"))),
            };
            let anchor = decode_select(buf, pos)?;
            let recursive = decode_select(buf, pos)?;
            let outer = decode_select(buf, pos)?;
            Ok(PlanStmt::RecursiveCte(RecursiveCtePlan {
                name,
                columns,
                col_types,
                union_all,
                anchor,
                recursive,
                outer,
            }))
        }
        STMT_DERIVED => {
            let name = r_str(buf, pos)?;
            let n_cols = r_u16(buf, pos)? as usize;
            // A SELECT projects at least one column; capped like any projection
            // so a forged count cannot balloon the allocation.
            if n_cols == 0 || n_cols > MAX_COLUMNS {
                return Err(corrupt("derived-table column count out of range"));
            }
            let mut columns = Vec::with_capacity(n_cols.min(1024));
            let mut col_types = Vec::with_capacity(n_cols.min(1024));
            for _ in 0..n_cols {
                columns.push(r_str(buf, pos)?);
                let tag = r_u8(buf, pos)?;
                col_types.push(ColumnType::from_tag(tag).ok_or_else(|| {
                    corrupt(format!("bad derived-table column type tag {tag}"))
                })?);
            }
            // The body under the format-31 body-discriminant byte, then the
            // outer statement (the mirror of the encoder).
            let body = match r_u8(buf, pos)? {
                SUBBODY_SELECT => SubBody::Select(decode_select(buf, pos)?),
                SUBBODY_COMPOUND => SubBody::Compound(decode_compound(buf, pos, budget)?),
                t => return Err(corrupt(format!("bad derived-table body tag {t}"))),
            };
            let outer = decode_select(buf, pos)?;
            // The BODY's own lifts (format 52) — the mirror of the encoder,
            // decoded under the SAME `MAX_SUBPLANS` tree budget the
            // statement-level list gets, so a forged count cannot balloon the
            // forest here either.
            let body_sub_base = r_u16(buf, pos)?;
            let n_body_sub = r_u8(buf, pos)? as usize;
            if n_body_sub > MAX_SUBPLANS {
                return Err(corrupt("too many derived-table body subplans"));
            }
            let mut body_subplans = Vec::with_capacity(n_body_sub.min(MAX_SUBPLANS));
            for _ in 0..n_body_sub {
                body_subplans.push(decode_subplan(buf, pos, budget)?);
            }
            Ok(PlanStmt::Derived(DerivedPlan {
                name,
                columns,
                col_types,
                body,
                body_subplans,
                body_sub_base,
                outer,
            }))
        }
        other => decode_stmt_rest(other, buf, pos),
    }
}

/// A compound `SELECT … UNION/EXCEPT/INTERSECT …` body — the exact mirror of
/// `encode_compound`, shared between a top-level compound statement and a
/// compound subquery body (format 31).
fn decode_compound(buf: &[u8], pos: &mut usize, budget: &mut usize) -> Result<CompoundPlan> {
    let n_arms = r_u8(buf, pos)? as usize;
    if !(2..=MAX_COMPOUND_ARMS).contains(&n_arms) {
        return Err(corrupt("compound arm count out of range"));
    }
    let mut ops = Vec::with_capacity(n_arms - 1);
    for _ in 0..n_arms - 1 {
        let t = r_u8(buf, pos)?;
        ops.push(SetOp::from_tag(t).ok_or_else(|| corrupt(format!("bad set-operator tag {t}")))?);
    }
    let mut arms = Vec::with_capacity(n_arms);
    for _ in 0..n_arms {
        arms.push(decode_select(buf, pos)?);
    }
    let n_order = r_u16(buf, pos)? as usize;
    if n_order > crate::parser::MAX_ORDER_BY_ITEMS {
        return Err(corrupt("too many order-by items in plan"));
    }
    let mut order_by = Vec::with_capacity(n_order.min(1024));
    for _ in 0..n_order {
        let c = r_u16(buf, pos)?;
        let dir = SortDir::from_byte(r_u8(buf, pos)?)
            .ok_or_else(|| corrupt("bad order direction"))?;
        let coll = r_collation(buf, pos)?;
        order_by.push((c, dir, coll));
    }
    let limit = decode_opt_u64(buf, pos)?;
    let offset = decode_opt_u64(buf, pos)?;
    // Format 56: the arms' OWNED lifts. Either NO list at all (a lift-free
    // compound) or exactly one per arm; drawn from the plan-wide tree budget so
    // the arm-ownership move cannot enlarge the DoS bound.
    let arm_sub_base = r_u16(buf, pos)?;
    let n_lists = r_u8(buf, pos)? as usize;
    if n_lists != 0 && n_lists != n_arms {
        return Err(corrupt("compound arm-subplan list count does not match arm count"));
    }
    let mut arm_subplans = Vec::with_capacity(n_lists);
    for _ in 0..n_lists {
        let n = r_u8(buf, pos)? as usize;
        if n > MAX_SUBPLANS {
            return Err(corrupt("too many subplans in one compound arm"));
        }
        let mut arm = Vec::with_capacity(n.min(MAX_SUBPLANS));
        for _ in 0..n {
            arm.push(decode_subplan(buf, pos, budget)?);
        }
        arm_subplans.push(arm);
    }
    Ok(CompoundPlan { arms, ops, order_by, limit, offset, arm_subplans, arm_sub_base })
}

/// A u32-length-prefixed UTF-8 string (the mirror of `w_str`), bounded at 1 MiB
/// like every other string this decoder reads.
fn r_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = r_u32(buf, pos)? as usize;
    if len > 1 << 20 {
        return Err(corrupt("string too long in plan"));
    }
    Ok(std::str::from_utf8(take(buf, pos, len)?)
        .map_err(|_| corrupt("invalid UTF-8 in plan string"))?
        .to_string())
}

/// The body of a `Select` after its statement tag — the exact mirror of
/// `encode_select`, shared between a top-level SELECT and each compound arm.
fn decode_select(buf: &[u8], pos: &mut usize) -> Result<SelectPlan> {
    {
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
                let dir = SortDir::from_byte(r_u8(buf, pos)?)
                    .ok_or_else(|| corrupt("bad order direction"))?;
                let coll = r_collation(buf, pos)?;
                order_by.push((c, dir, coll));
            }
            let limit = decode_opt_u64(buf, pos)?;
            let offset = decode_opt_u64(buf, pos)?;
            let njoins = r_u16(buf, pos)? as usize;
            if njoins > MAX_JOINS {
                return Err(corrupt("too many joins in plan"));
            }
            let mut joins = Vec::with_capacity(njoins.min(MAX_JOINS));
            for _ in 0..njoins {
                let table = r_u32(buf, pos)?;
                let kind = match r_u8(buf, pos)? {
                    0 => JoinKind::Inner,
                    1 => JoinKind::Left,
                    // RIGHT never reaches plan bytes (the planner rewrites it
                    // to a swapped LEFT), so its tag stays reserved — refused
                    // by NAME so the reader learns what the blob wanted.
                    2 => return Err(corrupt("join kind RIGHT is reserved — plans carry a swapped LEFT")),
                    3 => JoinKind::Full,
                    t => return Err(corrupt(format!("bad join kind tag {t}"))),
                };
                joins.push(Join {
                    table,
                    kind,
                    access: decode_access(buf, pos)?,
                    on: ExprProgram::decode(buf, pos)?,
                    policy: decode_opt_program(buf, pos)?,
                });
            }
            let joined_filter = decode_opt_program(buf, pos)?;
            let post_filter = decode_opt_program(buf, pos)?;
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
                        group_by.push(match r_u8(buf, pos)? {
                            0 => GroupKey::Col(r_u16(buf, pos)?),
                            1 => GroupKey::Expr(ExprProgram::decode(buf, pos)?),
                            t => return Err(corrupt(format!("bad group-key tag {t}"))),
                        });
                    }
                    let n = r_u16(buf, pos)? as usize;
                    if n > crate::parser::MAX_SELECT_ITEMS {
                        return Err(corrupt("too many aggregates in plan"));
                    }
                    let mut aggs = Vec::with_capacity(n.min(64));
                    for _ in 0..n {
                        // Format 40: tag 0 = HOST aggregate (name follows);
                        // 1..=7 = the `AggFn` byte, exactly as before.
                        let f = match r_u8(buf, pos)? {
                            0 => {
                                let name = r_str(buf, pos)?;
                                if name.is_empty() {
                                    return Err(corrupt("host aggregate with an empty name"));
                                }
                                AggTarget::Host(name)
                            }
                            t => AggTarget::Native(
                                AggFn::from_tag(t)
                                    .ok_or_else(|| corrupt("unknown aggregate function"))?,
                            ),
                        };
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
                        if f.host().is_some() && arg.is_none() {
                            // Only `count(*)` takes the row itself; a host
                            // aggregate always carries its one argument (the
                            // parser guarantees it), so the executor never has
                            // to invent an argument list for `xStep`.
                            return Err(corrupt("host aggregate with no argument"));
                        }
                        // FILTER (WHERE …) (format 38): optional predicate over
                        // the base row; `validate` re-checks its width and params.
                        let filter = decode_opt_program(buf, pos)?;
                        // Host-aggregate arguments after the first (format 51).
                        // Only a host call may carry them: a built-in aggregate
                        // takes exactly one argument and the executor would have
                        // nowhere to put a second.
                        let n_extra = r_u16(buf, pos)? as usize;
                        if n_extra > MAX_COLUMNS {
                            return Err(corrupt("too many host aggregate arguments"));
                        }
                        if n_extra > 0 && f.host().is_none() {
                            return Err(corrupt(
                                "built-in aggregate with more than one argument",
                            ));
                        }
                        let mut extra_args = Vec::with_capacity(n_extra.min(MAX_COLUMNS));
                        for _ in 0..n_extra {
                            extra_args.push(ExprProgram::decode(buf, pos)?);
                        }
                        aggs.push(AggCall {
                            func: f,
                            arg,
                            distinct,
                            filter,
                            extra_args,
                        });
                    }
                    let having = decode_opt_program(buf, pos)?;
                    // sqlite bare columns (format 30): base-row indices carried
                    // from the group's min/max witness row. Bounded like any
                    // column list; `validate` re-checks each index against the
                    // base width and enforces the single-min/max invariant.
                    let n = r_u16(buf, pos)? as usize;
                    if n > MAX_COLUMNS {
                        return Err(corrupt("too many bare columns in plan"));
                    }
                    let mut bare_cols = Vec::with_capacity(n.min(MAX_COLUMNS));
                    for _ in 0..n {
                        bare_cols.push(r_u16(buf, pos)?);
                    }
                    Some(Aggregation {
                        group_by,
                        aggs,
                        having,
                        bare_cols,
                    })
                }
                t => return Err(corrupt(format!("bad aggregate tag {t}"))),
            };
            // Window functions (format 24): the trailing list after aggregate.
            let n_win = r_u16(buf, pos)? as usize;
            if n_win > MAX_WINDOWS {
                return Err(corrupt("too many windows in plan"));
            }
            let mut windows = Vec::with_capacity(n_win.min(MAX_WINDOWS));
            for _ in 0..n_win {
                windows.push(decode_window(buf, pos)?);
            }
            Ok(SelectPlan {
                table,
                access,
                joins,
                joined_filter,
                post_filter,
                filter,
                projection,
                order_by,
                order_over,
                limit,
                offset,
                aggregate,
                distinct,
                order_junk,
                windows,
            })
    }
}

/// One [`WindowSpec`] — the exact mirror of `encode_window`. A closed func tag
/// (unknown ⇒ `Corrupt`, like `AggFn::from_tag`), and `distinct` is rejected
/// (stage 1 does not support it, and neither does the planner that emits this).
fn decode_window(buf: &[u8], pos: &mut usize) -> Result<WindowSpec> {
    let mut host: Option<String> = None;
    let func = match r_u8(buf, pos)? {
        1 => WindowFunc::RowNumber,
        2 => WindowFunc::Rank,
        3 => WindowFunc::DenseRank,
        4 => {
            let f = AggFn::from_tag(r_u8(buf, pos)?)
                .ok_or_else(|| corrupt("unknown window aggregate function"))?;
            WindowFunc::Agg(f)
        }
        // Value/offset functions (format 34). Lag/Lead/NthValue carry a trailing
        // i64 (constant offset / n); FirstValue/LastValue carry nothing extra.
        5 => WindowFunc::Lag(r_u64(buf, pos)? as i64),
        6 => WindowFunc::Lead(r_u64(buf, pos)? as i64),
        7 => WindowFunc::FirstValue,
        8 => WindowFunc::LastValue,
        9 => {
            let n = r_u64(buf, pos)? as i64;
            // nth_value's n is a POSITIVE integer (the planner refuses n < 1, and
            // sqlite errors on it at runtime); a blob claiming otherwise is
            // corrupt rather than a source of a wrong answer.
            if n < 1 {
                return Err(corrupt("nth_value n must be a positive integer"));
            }
            WindowFunc::NthValue(n)
        }
        // Rank/distribution functions (format 35). Ntile carries a trailing i64
        // (the constant bucket count ≥ 1); PercentRank/CumeDist carry nothing.
        10 => {
            let n = r_u64(buf, pos)? as i64;
            if n < 1 {
                return Err(corrupt("ntile bucket count must be a positive integer"));
            }
            WindowFunc::Ntile(n)
        }
        11 => WindowFunc::PercentRank,
        12 => WindowFunc::CumeDist,
        // Format 55: a HOST window aggregate — the tag is followed by its NAME.
        13 => {
            host = Some(r_str(buf, pos)?);
            if host.as_deref().is_some_and(str::is_empty) {
                return Err(corrupt("host window aggregate with an empty name"));
            }
            WindowFunc::Host
        }
        t => return Err(corrupt(format!("bad window function tag {t}"))),
    };
    let arg = decode_opt_program(buf, pos)?;
    let distinct = match r_u8(buf, pos)? {
        0 => false,
        // Refused in stage 1 — a blob claiming it is corrupt (matches the
        // planner, which never emits it).
        1 => return Err(corrupt("DISTINCT window aggregate is not supported")),
        t => return Err(corrupt(format!("bad window distinct tag {t}"))),
    };
    let default = decode_opt_program(buf, pos)?;
    // A ranking/distribution function takes no argument (the row itself is the
    // input; ntile's bucket count rides in its tag); a value or aggregate
    // function does. The `default` program is a lag/lead-only field.
    let is_ranking = matches!(
        func,
        WindowFunc::RowNumber
            | WindowFunc::Rank
            | WindowFunc::DenseRank
            | WindowFunc::Ntile(_)
            | WindowFunc::PercentRank
            | WindowFunc::CumeDist
    );
    let is_value = matches!(
        func,
        WindowFunc::Lag(_)
            | WindowFunc::Lead(_)
            | WindowFunc::FirstValue
            | WindowFunc::LastValue
            | WindowFunc::NthValue(_)
    );
    if arg.is_some() && is_ranking {
        return Err(corrupt("ranking window function carries an argument"));
    }
    if arg.is_none() && is_value {
        return Err(corrupt("value window function requires an argument"));
    }
    // A host window aggregate is the single-argument shape the sliding protocol
    // can express (the same arguments go to `xStep` and `xInverse`).
    if matches!(func, WindowFunc::Host) && arg.is_none() {
        return Err(corrupt("host window aggregate requires an argument"));
    }
    if default.is_some() && !matches!(func, WindowFunc::Lag(_) | WindowFunc::Lead(_)) {
        return Err(corrupt("only lag/lead carry a default expression"));
    }
    let n_part = r_u16(buf, pos)? as usize;
    if n_part > crate::parser::MAX_ORDER_BY_ITEMS {
        return Err(corrupt("too many PARTITION BY items in plan"));
    }
    let mut partition_by = Vec::with_capacity(n_part.min(64));
    for _ in 0..n_part {
        partition_by.push(ExprProgram::decode(buf, pos)?);
    }
    let n_ord = r_u16(buf, pos)? as usize;
    if n_ord > crate::parser::MAX_ORDER_BY_ITEMS {
        return Err(corrupt("too many window ORDER BY items in plan"));
    }
    let mut order_by = Vec::with_capacity(n_ord.min(64));
    for _ in 0..n_ord {
        let program = ExprProgram::decode(buf, pos)?;
        let desc = match r_u8(buf, pos)? {
            0 => false,
            1 => true,
            t => return Err(corrupt(format!("bad window order direction {t}"))),
        };
        order_by.push((program, desc));
    }
    let frame = decode_opt_frame(buf, pos)?;
    // The frame's structural legality (which functions accept one, boundary
    // ordering, RANGE-offset refusal, the ORDER BY requirement) is exactly the
    // planner's rule set, applied here so a hostile blob cannot smuggle a shape
    // `prepare` would never emit.
    if let Some(f) = &frame {
        f.check(func, !order_by.is_empty()).map_err(corrupt)?;
    }
    Ok(WindowSpec {
        func,
        arg,
        distinct,
        partition_by,
        order_by,
        default,
        frame,
        host,
    })
}

/// One optional explicit frame — the exact mirror of `encode_opt_frame`. A `0`
/// presence byte is the default frame; `1` is followed by a mode byte and two
/// boundaries. Unknown mode/boundary tags are `Corrupt` (a closed enum).
fn decode_opt_frame(buf: &[u8], pos: &mut usize) -> Result<Option<Frame>> {
    match r_u8(buf, pos)? {
        0 => Ok(None),
        1 => {
            let mode = match r_u8(buf, pos)? {
                1 => FrameMode::Rows,
                2 => FrameMode::Range,
                3 => FrameMode::Groups,
                t => return Err(corrupt(format!("bad window frame mode {t}"))),
            };
            let start = decode_frame_bound(buf, pos)?;
            let end = decode_frame_bound(buf, pos)?;
            Ok(Some(Frame { mode, start, end }))
        }
        t => Err(corrupt(format!("bad window frame presence tag {t}"))),
    }
}

/// One frame boundary — the mirror of `encode_frame_bound`.
fn decode_frame_bound(buf: &[u8], pos: &mut usize) -> Result<FrameBound> {
    Ok(match r_u8(buf, pos)? {
        1 => FrameBound::UnboundedPreceding,
        2 => FrameBound::Preceding(r_u64(buf, pos)?),
        3 => FrameBound::CurrentRow,
        4 => FrameBound::Following(r_u64(buf, pos)?),
        5 => FrameBound::UnboundedFollowing,
        t => return Err(corrupt(format!("bad window frame boundary tag {t}"))),
    })
}

fn decode_stmt_rest(tag: u8, buf: &[u8], pos: &mut usize) -> Result<PlanStmt> {
    match tag {
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
            let from_select = match r_u8(buf, pos)? {
                0 => None,
                1 => {
                    let plan = Box::new(decode_select(buf, pos)?);
                    let n = r_u16(buf, pos)? as usize;
                    if n > MAX_COLUMNS {
                        return Err(corrupt("INSERT … SELECT col_map out of range"));
                    }
                    let mut col_map = Vec::with_capacity(n.min(MAX_COLUMNS));
                    for _ in 0..n {
                        col_map.push(match r_u8(buf, pos)? {
                            0 => None,
                            1 => Some(r_u16(buf, pos)?),
                            t => return Err(corrupt(format!("bad col_map tag {t}"))),
                        });
                    }
                    Some(crate::plan::InsertSelect { plan, col_map })
                }
                t => return Err(corrupt(format!("bad from_select tag {t}"))),
            };
            let with_check = decode_opt_program(buf, pos)?;
            let on_conflict = decode_on_conflict(buf, pos)?;
            let returning = decode_opt_projection(buf, pos)?;
            Ok(PlanStmt::Insert {
                table,
                rows,
                from_select,
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
        STMT_SAVEPOINT => Ok(PlanStmt::Savepoint(decode_savepoint_name(buf, pos)?)),
        STMT_RELEASE => Ok(PlanStmt::Release(decode_savepoint_name(buf, pos)?)),
        STMT_ROLLBACK_TO => Ok(PlanStmt::RollbackTo(decode_savepoint_name(buf, pos)?)),
        t => Err(corrupt(format!("bad statement tag {t}"))),
    }
}

/// A u16-length-prefixed, non-empty, valid-UTF-8 savepoint name from an
/// untrusted blob. Every read is bounds-checked (via [`take`]).
fn decode_savepoint_name(buf: &[u8], pos: &mut usize) -> Result<String> {
    let n = r_u16(buf, pos)? as usize;
    let b = take(buf, pos, n)?;
    let name = std::str::from_utf8(b)
        .map_err(|_| corrupt("savepoint name is not valid utf-8"))?
        .to_string();
    if name.is_empty() {
        return Err(corrupt("empty savepoint name"));
    }
    Ok(name)
}
