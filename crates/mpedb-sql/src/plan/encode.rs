use super::*;

impl CompiledPlan {
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
        buf.push(self.subplans.len() as u8);
        for s in &self.subplans {
            encode_subplan(s, &mut buf);
        }
        self.footprint.encode_into(&mut buf);
        encode_stmt(&self.stmt, &mut buf);
        buf
    }
}

// ---- statement encode/decode ----------------------------------------------

/// One lifted subquery, RECURSIVELY (#73 §3). Layout: kind, `sub_base`,
/// `slot_type` tag, the correlation-arg list, a body-discriminant BYTE + the
/// body (a SELECT or a whole compound, #56/format 31), then a COUNT and the
/// inner's own nested subplans — the exact mirror of [`decode_subplan`].
fn encode_subplan(s: &SubPlan, buf: &mut Vec<u8>) {
    buf.push(s.kind as u8);
    w_u16(buf, s.sub_base);
    buf.push(s.slot_type.map_or(0, |t| t as u8));
    w_u16(buf, s.outer_args.len() as u16);
    for a in &s.outer_args {
        buf.extend_from_slice(&a.to_le_bytes());
    }
    match &s.body {
        SubBody::Select(sp) => {
            buf.push(SUBBODY_SELECT);
            encode_select(sp, buf);
        }
        SubBody::Compound(c) => {
            buf.push(SUBBODY_COMPOUND);
            encode_compound(c, buf);
        }
    }
    buf.push(s.subplans.len() as u8);
    for c in &s.subplans {
        encode_subplan(c, buf);
    }
}

/// The body of a `Compound` after its context (the statement tag or the subplan
/// body-discriminant) — shared verbatim between a top-level compound statement
/// and a compound subquery body, so the two encodings can never drift.
fn encode_compound(c: &CompoundPlan, buf: &mut Vec<u8>) {
    buf.push(c.arms.len() as u8);
    for op in &c.ops {
        buf.push(match op {
            SetOp::Union => 0u8,
            SetOp::UnionAll => 1,
            SetOp::Except => 2,
            SetOp::Intersect => 3,
        });
    }
    for arm in &c.arms {
        encode_select(arm, buf);
    }
    w_u16(buf, c.order_by.len() as u16);
    for (col, dir, coll) in &c.order_by {
        w_u16(buf, *col);
        buf.push(dir.to_byte());
        buf.push(*coll as u8);
    }
    encode_opt_u64(c.limit, buf);
    encode_opt_u64(c.offset, buf);
}

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

fn encode_opt_u64(v: Option<u64>, buf: &mut Vec<u8>) {
    match v {
        None => buf.push(0),
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_le_bytes());
        }
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
        KeyPart::OuterCol(i) => {
            buf.push(PART_OUTER_COL);
            w_u16(buf, *i);
        }
    }
}

fn encode_parts(parts: &[KeyPart], buf: &mut Vec<u8>) {
    w_u16(buf, parts.len() as u16);
    for p in parts {
        encode_part(p, buf);
    }
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
        AccessPath::IndexPoint { index_no, parts } => {
            buf.push(ACCESS_INDEX_POINT);
            buf.extend_from_slice(&index_no.to_le_bytes());
            encode_parts(parts, buf);
        }
        AccessPath::IndexRange { index_no, lo, hi } => {
            buf.push(ACCESS_INDEX_RANGE);
            buf.extend_from_slice(&index_no.to_le_bytes());
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
        AccessPath::FtsScan { query } => {
            buf.push(ACCESS_FTS_SCAN);
            encode_fts_query(query, buf);
        }
    }
}

/// One FTS query node, RECURSIVELY (design/DESIGN-FTS.md §3) — the exact mirror
/// of `decode_fts_query`. Layout: a node tag, then for a `Term` its token
/// (len-prefixed), the `prefix`/`initial` flag bytes, and its column list;
/// for a boolean node its two children.
fn encode_fts_query(q: &FtsQuery, buf: &mut Vec<u8>) {
    match q {
        FtsQuery::Term(t) => {
            buf.push(FTS_TERM);
            buf.extend_from_slice(&(t.token.len() as u32).to_le_bytes());
            buf.extend_from_slice(t.token.as_bytes());
            buf.push(t.prefix as u8);
            buf.push(t.initial as u8);
            w_u16(buf, t.columns.len() as u16);
            for &c in &t.columns {
                w_u16(buf, c);
            }
        }
        FtsQuery::And(a, b) => {
            buf.push(FTS_AND);
            encode_fts_query(a, buf);
            encode_fts_query(b, buf);
        }
        FtsQuery::Or(a, b) => {
            buf.push(FTS_OR);
            encode_fts_query(a, buf);
            encode_fts_query(b, buf);
        }
        FtsQuery::AndNot(a, b) => {
            buf.push(FTS_AND_NOT);
            encode_fts_query(a, buf);
            encode_fts_query(b, buf);
        }
    }
}

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

fn encode_opt_projection(proj: Option<&[Projection]>, buf: &mut Vec<u8>) {
    match proj {
        None => buf.push(0),
        Some(p) => {
            buf.push(1);
            encode_projection(p, buf);
        }
    }
}

fn encode_on_conflict(oc: &PlanOnConflict, buf: &mut Vec<u8>) {
    match oc {
        PlanOnConflict::Error => buf.push(OC_ERROR),
        PlanOnConflict::DoNothing => buf.push(OC_DO_NOTHING),
        PlanOnConflict::Replace => buf.push(OC_REPLACE),
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

fn encode_stmt(stmt: &PlanStmt, buf: &mut Vec<u8>) {
    match stmt {
        PlanStmt::Select(sp) => {
            buf.push(STMT_SELECT);
            encode_select(sp, buf);
        }
        PlanStmt::Compound(c) => {
            buf.push(STMT_COMPOUND);
            encode_compound(c, buf);
        }
        PlanStmt::RecursiveCte(rc) => {
            buf.push(STMT_RECURSIVE_CTE);
            w_str(buf, &rc.name);
            // The declared columns paired with their types (equal length).
            w_u16(buf, rc.columns.len() as u16);
            for (name, ty) in rc.columns.iter().zip(&rc.col_types) {
                w_str(buf, name);
                buf.push(*ty as u8);
            }
            buf.push(rc.union_all as u8);
            encode_select(&rc.anchor, buf);
            encode_select(&rc.recursive, buf);
            encode_select(&rc.outer, buf);
        }
        PlanStmt::Derived(dp) => {
            buf.push(STMT_DERIVED);
            w_str(buf, &dp.name);
            // The body's output columns paired with their types (equal length).
            w_u16(buf, dp.columns.len() as u16);
            for (name, ty) in dp.columns.iter().zip(&dp.col_types) {
                w_str(buf, name);
                buf.push(*ty as u8);
            }
            // The body under the format-31 body-discriminant byte, then the
            // outer statement — mirroring `encode_subplan`'s body framing.
            match &dp.body {
                SubBody::Select(sp) => {
                    buf.push(SUBBODY_SELECT);
                    encode_select(sp, buf);
                }
                SubBody::Compound(c) => {
                    buf.push(SUBBODY_COMPOUND);
                    encode_compound(c, buf);
                }
            }
            encode_select(&dp.outer, buf);
        }
        _other => encode_stmt_rest(stmt, buf),
    }
}

/// A u32-length-prefixed UTF-8 string — the same convention projection names
/// use (`encode_projection`).
fn w_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// The body of a `Select` after its statement tag — shared verbatim between a
/// top-level SELECT and each compound arm, so the two can never drift.
fn encode_select(sp: &SelectPlan, buf: &mut Vec<u8>) {
    let SelectPlan {
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
    } = sp;
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
            for (c, dir, coll) in order_by {
                w_u16(buf, *c);
                buf.push(dir.to_byte());
                buf.push(*coll as u8);
            }
            encode_opt_u64(*limit, buf);
            encode_opt_u64(*offset, buf);
            // A COUNT of joins where v6 wrote a single optional-join tag
            // (PLAN_FORMAT 7). `joined_filter` follows the chain, once, over the
            // full joined row.
            w_u16(buf, joins.len() as u16);
            for j in joins {
                buf.extend_from_slice(&j.table.to_le_bytes());
                buf.push(match j.kind {
                    JoinKind::Inner => 0,
                    JoinKind::Left => 1,
                    // 2 (RIGHT) stays reserved: the planner rewrites RIGHT to
                    // a swapped LEFT, so no plan ever carries it.
                    JoinKind::Full => 3,
                });
                encode_access(&j.access, buf);
                j.on.encode_into(buf);
                encode_opt_program(j.policy.as_ref(), buf);
            }
            encode_opt_program(joined_filter.as_ref(), buf);
            encode_opt_program(post_filter.as_ref(), buf);
            buf.push(*distinct as u8);
            w_u16(buf, *order_junk);
            match aggregate {
                None => buf.push(0),
                Some(a) => {
                    buf.push(1);
                    w_u16(buf, a.group_by.len() as u16);
                    for k in &a.group_by {
                        match k {
                            GroupKey::Col(c) => {
                                buf.push(0);
                                w_u16(buf, *c);
                            }
                            GroupKey::Expr(p) => {
                                buf.push(1);
                                p.encode_into(buf);
                            }
                        }
                    }
                    w_u16(buf, a.aggs.len() as u16);
                    for c in &a.aggs {
                        // Format 40: tag 1..=7 is the `AggFn` byte, unchanged;
                        // tag 0 (never a valid `AggFn`) introduces a HOST
                        // aggregate followed by its name. Native plans encode
                        // exactly as in format 39.
                        match &c.func {
                            AggTarget::Native(f) => buf.push(*f as u8),
                            AggTarget::Host(name) => {
                                buf.push(0);
                                w_str(buf, name);
                            }
                        }
                        buf.push(c.distinct as u8);
                        match &c.arg {
                            None => buf.push(0),
                            Some(p) => {
                                buf.push(1);
                                p.encode_into(buf);
                            }
                        }
                        // FILTER (WHERE …) (format 38): optional predicate over
                        // the base row, same optional-program framing as `arg`.
                        encode_opt_program(c.filter.as_ref(), buf);
                    }
                    encode_opt_program(a.having.as_ref(), buf);
                    // sqlite bare columns (format 30): base-row indices carried
                    // from the group's min/max witness row. Empty unless the plan
                    // has live bare columns, so only those plans differ.
                    w_u16(buf, a.bare_cols.len() as u16);
                    for c in &a.bare_cols {
                        w_u16(buf, *c);
                    }
                }
            }
            // Window functions (format 24): a trailing list after the aggregate
            // block. Compound arms / INSERT…SELECT sources encode an empty list
            // (the planner never puts windows there).
            w_u16(buf, windows.len() as u16);
            for w in windows {
                encode_window(w, buf);
            }
}

/// One [`WindowSpec`]: func tag (+ an `AggFn` byte for `Agg`, or an i64
/// offset/n/bucket-count for `Lag`/`Lead`/`NthValue`/`Ntile`), optional arg
/// program, distinct byte, optional `default` program (lag/lead), a PARTITION BY
/// program list, and an ORDER BY `(program, desc)` list — the exact mirror of
/// `decode_window`.
fn encode_window(w: &WindowSpec, buf: &mut Vec<u8>) {
    buf.push(w.func.tag());
    match w.func {
        WindowFunc::Agg(f) => buf.push(f as u8),
        WindowFunc::Lag(o)
        | WindowFunc::Lead(o)
        | WindowFunc::NthValue(o)
        | WindowFunc::Ntile(o) => {
            buf.extend_from_slice(&o.to_le_bytes());
        }
        _ => {}
    }
    encode_opt_program(w.arg.as_ref(), buf);
    buf.push(w.distinct as u8);
    encode_opt_program(w.default.as_ref(), buf);
    w_u16(buf, w.partition_by.len() as u16);
    for p in &w.partition_by {
        p.encode_into(buf);
    }
    w_u16(buf, w.order_by.len() as u16);
    for (p, desc) in &w.order_by {
        p.encode_into(buf);
        buf.push(*desc as u8);
    }
    encode_opt_frame(w.frame.as_ref(), buf);
}

/// An optional explicit frame (format 36): a presence byte, then — when present
/// — a mode byte and two boundaries. Mirror of `decode_opt_frame`. `None` (the
/// default frame) is a single `0` byte, so every pre-frame window shape encodes
/// exactly as before plus that terminator.
fn encode_opt_frame(frame: Option<&Frame>, buf: &mut Vec<u8>) {
    match frame {
        None => buf.push(0),
        Some(f) => {
            buf.push(1);
            buf.push(match f.mode {
                FrameMode::Rows => 1,
                FrameMode::Range => 2,
                FrameMode::Groups => 3,
            });
            encode_frame_bound(f.start, buf);
            encode_frame_bound(f.end, buf);
        }
    }
}

/// One boundary: a tag byte, plus a trailing u64 offset for `Preceding`/
/// `Following`. Mirror of `decode_frame_bound`.
fn encode_frame_bound(b: FrameBound, buf: &mut Vec<u8>) {
    match b {
        FrameBound::UnboundedPreceding => buf.push(1),
        FrameBound::Preceding(n) => {
            buf.push(2);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        FrameBound::CurrentRow => buf.push(3),
        FrameBound::Following(n) => {
            buf.push(4);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        FrameBound::UnboundedFollowing => buf.push(5),
    }
}

fn encode_stmt_rest(stmt: &PlanStmt, buf: &mut Vec<u8>) {
    match stmt {
        PlanStmt::Select(_)
        | PlanStmt::Compound(_)
        | PlanStmt::RecursiveCte(_)
        | PlanStmt::Derived(_) => {
            unreachable!("handled by encode_stmt")
        }
        PlanStmt::Insert {
            table,
            rows,
            from_select,
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
            // INSERT … SELECT source: presence byte, then the embedded select
            // plan and the target-column map.
            match from_select {
                None => buf.push(0),
                Some(sel) => {
                    buf.push(1);
                    encode_select(&sel.plan, buf);
                    w_u16(buf, sel.col_map.len() as u16);
                    for m in &sel.col_map {
                        match m {
                            None => buf.push(0),
                            Some(i) => {
                                buf.push(1);
                                w_u16(buf, *i);
                            }
                        }
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
        PlanStmt::Savepoint(name) => encode_named_stmt(STMT_SAVEPOINT, name, buf),
        PlanStmt::Release(name) => encode_named_stmt(STMT_RELEASE, name, buf),
        PlanStmt::RollbackTo(name) => encode_named_stmt(STMT_ROLLBACK_TO, name, buf),
    }
}

/// A savepoint-control statement: tag byte then a u16-length-prefixed UTF-8
/// name. The name is a SQL identifier, so it fits a u16 length by construction
/// (the parser caps identifier length far below 65535).
fn encode_named_stmt(tag: u8, name: &str, buf: &mut Vec<u8>) {
    buf.push(tag);
    w_u16(buf, name.len() as u16);
    buf.extend_from_slice(name.as_bytes());
}
