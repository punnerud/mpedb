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
        let mut subplans = Vec::with_capacity(n_sub);
        for _ in 0..n_sub {
            let kind = match SubPlanKind::from_tag(r_u8(bytes, &mut pos)?) {
                Some(k) => k,
                None => return Err(corrupt("bad subplan kind tag")),
            };
            let n_args = r_u16(bytes, &mut pos)? as usize;
            if n_args > MAX_COLUMNS {
                return Err(corrupt("too many subplan correlation args"));
            }
            let mut outer_args = Vec::with_capacity(n_args.min(64));
            for _ in 0..n_args {
                outer_args.push(r_u16(bytes, &mut pos)?);
            }
            let plan = decode_select(bytes, &mut pos)?;
            subplans.push(SubPlan { plan, outer_args, kind });
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
            let part = decode_part(buf, pos)?;
            Ok(AccessPath::IndexPoint { index_no, part })
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
        t => Err(corrupt(format!("bad access path tag {t}"))),
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

fn decode_stmt(buf: &[u8], pos: &mut usize) -> Result<PlanStmt> {
    match r_u8(buf, pos)? {
        STMT_SELECT => Ok(PlanStmt::Select(decode_select(buf, pos)?)),
        STMT_COMPOUND => {
            let n_arms = r_u8(buf, pos)? as usize;
            if !(2..=MAX_COMPOUND_ARMS).contains(&n_arms) {
                return Err(corrupt("compound arm count out of range"));
            }
            let mut ops = Vec::with_capacity(n_arms - 1);
            for _ in 0..n_arms - 1 {
                let t = r_u8(buf, pos)?;
                ops.push(
                    SetOp::from_tag(t)
                        .ok_or_else(|| corrupt(format!("bad set-operator tag {t}")))?,
                );
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
                let desc = match r_u8(buf, pos)? {
                    0 => false,
                    1 => true,
                    t => return Err(corrupt(format!("bad order direction {t}"))),
                };
                order_by.push((c, desc));
            }
            let limit = decode_opt_u64(buf, pos)?;
            let offset = decode_opt_u64(buf, pos)?;
            Ok(PlanStmt::Compound(CompoundPlan {
                arms,
                ops,
                order_by,
                limit,
                offset,
            }))
        }
        other => decode_stmt_rest(other, buf, pos),
    }
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
            })
    }
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
