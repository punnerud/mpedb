use super::*;

/// EXPLAIN suffix for an ORDER BY key's collation: empty for the default
/// [`Collation::Binary`] so plain sorts render exactly as before, ` COLLATE
/// NOCASE`/` COLLATE RTRIM` otherwise.
fn collate_suffix(coll: Collation) -> String {
    match coll {
        Collation::Binary => String::new(),
        other => format!(" COLLATE {}", other.name()),
    }
}

/// The same, for an ORDER BY key — which may name a HOST collation.
fn order_collate_suffix(coll: &mpedb_types::OrderColl) -> String {
    match coll {
        mpedb_types::OrderColl::Native(c) => collate_suffix(*c),
        h => format!(" COLLATE {}", h.name()),
    }
}

/// EXPLAIN suffix for an ORDER BY key's NULL placement: empty when it is the
/// one the direction implies (sqlite's default: first for ASC, last for DESC),
/// so every sort written without a `NULLS` clause renders exactly as before.
fn nulls_suffix(dir: SortDir) -> &'static str {
    match (dir.default_nulls(), dir.nulls_first) {
        (true, _) => "",
        (false, true) => " NULLS FIRST",
        (false, false) => " NULLS LAST",
    }
}

impl CompiledPlan {
    /// Human-readable plan rendering for `EXPLAIN`.
    pub fn explain(&self, schema: &Schema) -> String {
        let mut out = String::new();
        match &self.stmt {
            PlanStmt::Select(sp) => self.render_select(sp, schema, &mut out),
            PlanStmt::Compound(c) => self.render_compound(c, schema, &mut out),
            PlanStmt::RecursiveCte(rc) => {
                out.push_str(&format!(
                    "RecursiveCte {}({}) {}\n",
                    rc.name,
                    rc.columns.join(", "),
                    if rc.union_all { "UNION ALL" } else { "UNION" },
                ));
                out.push_str("anchor:\n");
                self.render_select(&rc.anchor, schema, &mut out);
                out.push_str("recursive:\n");
                self.render_select(&rc.recursive, schema, &mut out);
                out.push_str("outer:\n");
                self.render_select(&rc.outer, schema, &mut out);
            }
            PlanStmt::Derived(dp) => {
                out.push_str(&format!(
                    "Derived {}({}) — materialized once\n",
                    dp.name,
                    dp.columns.join(", "),
                ));
                out.push_str("body:\n");
                match &dp.body {
                    SubBody::Select(sp) => self.render_select(sp, schema, &mut out),
                    SubBody::Compound(c) => self.render_compound(c, schema, &mut out),
                }
                out.push_str("outer:\n");
                self.render_select(&dp.outer, schema, &mut out);
                // The body's OWN lifts (format 52), rendered under the body
                // they belong to rather than with the statement-level list
                // below — which is empty for a derived plan precisely because
                // these are not the outer's.
                for (i, s) in dp.body_subplans.iter().enumerate() {
                    let label = format!("body ${}", dp.body_sub_base as usize + i + 1);
                    self.render_subplan(s, schema, &mut out, &label);
                }
            }
            _other => self.render_rest(schema, &mut out),
        }
        for (i, s) in self.subplans.iter().enumerate() {
            let label = format!("${}", self.subplan_base() as usize + i + 1);
            self.render_subplan(s, schema, &mut out, &label);
        }
        out.push_str(&format!(
            "  footprint: read_only={} tables_read={} tables_written={} indexes_used={:#x} key={}\n",
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

    /// Render one SELECT — shared between a top-level SELECT and each
    /// compound arm.
    /// Render one lifted subquery and, recursively, its own nested lifts
    /// (#73 §3). `label` is the reserved slot name (`$n`, then `$n.k` for a
    /// child).
    fn render_subplan(&self, s: &SubPlan, schema: &Schema, out: &mut String, label: &str) {
        out.push_str(&format!(
            "subplan {}: {}{}\n",
            label,
            match s.kind {
                SubPlanKind::Exists => "EXISTS, ",
                SubPlanKind::Scalar => "scalar, ",
                SubPlanKind::List => "IN-list, ",
            },
            if s.outer_args.is_empty() {
                "uncorrelated (evaluated once)".to_string()
            } else {
                format!("correlated on outer slots {:?} (per row)", s.outer_args)
            }
        ));
        match &s.body {
            SubBody::Select(sp) => self.render_select(sp, schema, out),
            SubBody::Compound(c) => self.render_compound(c, schema, out),
        }
        for (k, c) in s.subplans.iter().enumerate() {
            self.render_subplan(c, schema, out, &format!("{label}.{}", k + 1));
        }
    }

    /// Render a compound `SELECT … UNION/… …` — shared between a top-level
    /// compound statement and a compound subquery body (format 31).
    fn render_compound(&self, c: &CompoundPlan, schema: &Schema, out: &mut String) {
        out.push_str(&format!("Compound ({} arms)\n", c.arms.len()));
        for (k, arm) in c.arms.iter().enumerate() {
            if k > 0 {
                out.push_str(match c.ops[k - 1] {
                    SetOp::Union => "UNION\n",
                    SetOp::UnionAll => "UNION ALL\n",
                    SetOp::Except => "EXCEPT\n",
                    SetOp::Intersect => "INTERSECT\n",
                });
            }
            self.render_select(arm, schema, out);
        }
        if !c.order_by.is_empty() {
            let items: Vec<String> = c
                .order_by
                .iter()
                .map(|(i, dir, coll)| {
                    format!(
                        "output#{}{}{}{}",
                        i + 1,
                        order_collate_suffix(coll),
                        if dir.desc { " DESC" } else { " ASC" },
                        nulls_suffix(*dir)
                    )
                })
                .collect();
            out.push_str(&format!("order by: {}\n", items.join(", ")));
        }
        if let Some(n) = c.limit {
            out.push_str(&format!("limit: {n}\n"));
        }
        if let Some(n) = c.offset {
            out.push_str(&format!("offset: {n}\n"));
        }
    }

    fn render_select(&self, sp: &SelectPlan, schema: &Schema, out: &mut String) {
        // The working table (CTE_TABLE) is named from THIS plan's node —
        // RecursiveCte or Derived — not the schema.
        let (cte_def, cte_role): (Option<TableDef>, &str) = match &self.stmt {
            PlanStmt::RecursiveCte(rc) => (Some(rc.cte_def()), "recursive working table"),
            PlanStmt::Derived(dp) => (Some(dp.derived_def()), "materialized derived table"),
            _ => (None, "working table"),
        };
        let table_name = |id: u32| {
            if id == super::DUAL_TABLE {
                return "(no FROM — one synthetic row)".to_string();
            }
            if id == super::CTE_TABLE {
                return cte_def
                    .as_ref()
                    .map(|t| format!("{} ({cte_role})", t.name))
                    .unwrap_or_else(|| format!("({cte_role})"));
            }
            schema
                .table(id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| format!("table#{id}"))
        };
        let col_namer = |id: u32| {
            let t = if id == super::CTE_TABLE {
                cte_def.clone()
            } else {
                schema.table(id).cloned()
            };
            move |c: u16| match &t {
                Some(t) if (c as usize) < t.columns.len() => t.columns[c as usize].name.clone(),
                _ => format!("col#{c}"),
            }
        };
        {
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
            {
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
                // #114: the MPEE solver's decision, read back off the plan —
                // the order it chose, WHY each table sits where it does (the
                // access class it was entered with), and how many steps still
                // multiply by a whole table with CERTAINTY. That last count is
                // the term the solver minimises second and the one that turns
                // `select5.test`'s `join-17-4` from a refusal into an answer
                // (design/DESIGN-MPEE-SOLVER.md §3).
                if !joins.is_empty() {
                    let class = |a: &AccessPath, on: Option<&ExprProgram>| -> &'static str {
                        match a {
                            AccessPath::PkPoint(_) => "pk",
                            AccessPath::IndexPoint { .. } => "index",
                            AccessPath::PkRange { .. } | AccessPath::IndexRange { .. } => "range",
                            AccessPath::FtsScan { .. } => "fts",
                            // A held FullScan whose whole residual ON is the
                            // constant TRUE has no predicate linking it to
                            // anything already read: a cartesian step.
                            AccessPath::FullScan => match on {
                                Some(p) if p.is_const_true() => "cartesian",
                                _ => "scan",
                            },
                        }
                    };
                    let mut steps =
                        vec![format!("{} [{}]", table_name(*table), class(access, None))];
                    let mut cart = 0usize;
                    for j in joins {
                        let c = class(&j.access, Some(&j.on));
                        cart += usize::from(c == "cartesian");
                        steps.push(format!("{} [{}]", table_name(j.table), c));
                    }
                    out.push_str(&format!(
                        "  join order: {} (MPEE: {} cartesian step{})\n",
                        steps.join(" -> "),
                        cart,
                        if cart == 1 { "" } else { "s" }
                    ));
                }
                if let Some(f) = filter {
                    // Over the OUTER row alone, so it uses the outer's namer.
                    out.push_str(&format!("  filter: {}\n", render_program(f, &single)));
                }
                if let Some(f) = post_filter {
                    // Runs after the gather, once the correlated subplan
                    // slots are filled for the row.
                    out.push_str(&format!("  post-filter: {}\n", render_program(f, &base)));
                }
                for j in joins {
                    // The cost note is the honest one for THIS join: a
                    // FullScan inner side is read once and paired with every
                    // outer row (O(n*m) ON evaluations); a keyed access is
                    // the index nested loop — the ON equality was consumed
                    // into the inner fetch and runs per outer row.
                    let kind = match j.kind {
                        JoinKind::Inner => "inner",
                        JoinKind::Left => "left",
                        JoinKind::Full => "full",
                    };
                    let cost = match (&j.access, j.kind) {
                        (AccessPath::FullScan, JoinKind::Inner) => {
                            "(nested loop, O(n*m) — no predicate pushdown)".to_string()
                        }
                        (AccessPath::FullScan, JoinKind::Left) => {
                            "(nested loop, NULL-extends on no match — no predicate pushdown)"
                                .to_string()
                        }
                        (_, JoinKind::Inner) => {
                            "(index nested loop — ON equality pushed into the inner fetch)"
                                .to_string()
                        }
                        (_, JoinKind::Left) => {
                            "(index nested loop, NULL-extends on no match — ON equality pushed into the inner fetch)"
                                .to_string()
                        }
                        // FULL always holds the inner side whole: the
                        // unmatched-inner sweep needs it enumerated.
                        (_, JoinKind::Full) => {
                            "(held nested loop, NULL-extends on BOTH sides)".to_string()
                        }
                    };
                    out.push_str(&format!(
                        "  {kind} join {} {cost}\n",
                        table_name(j.table)
                    ));
                    out.push_str(&format!(
                        "    access: {}\n",
                        self.render_access_outer(&j.access, schema, j.table, Some(&base))
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
                let group_key_name = |k: &GroupKey| match k {
                    GroupKey::Col(c) => base(*c),
                    GroupKey::Expr(p) => render_program(p, &base),
                };
                let grouped: Option<Vec<String>> = aggregate.as_ref().map(|a| {
                    a.group_by
                        .iter()
                        .map(group_key_name)
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
                        let keys: Vec<String> =
                            a.group_by.iter().map(group_key_name).collect();
                        out.push_str(&format!("  group by: {}\n", keys.join(", ")));
                    }
                    let calls: Vec<String> = grouped.as_ref().unwrap()[a.group_by.len()..].to_vec();
                    out.push_str(&format!("  aggregate: {}\n", calls.join(", ")));
                    if let Some(h) = &a.having {
                        out.push_str(&format!("  having: {}\n", render_program(h, &name)));
                    }
                }
                // Window functions run over the base row, so their sub-programs
                // use the base namer. Shows the phase EXPLAIN otherwise hides.
                for (k, w) in windows.iter().enumerate() {
                    use crate::plan::WindowFunc as WF;
                    // The value `expr` (present for aggregate and value windows).
                    let argp = || w.arg.as_ref().map_or_else(String::new, |p| render_program(p, &base));
                    let fname = match w.func {
                        WF::RowNumber => "row_number()".to_string(),
                        WF::Rank => "rank()".to_string(),
                        WF::DenseRank => "dense_rank()".to_string(),
                        WF::Agg(f) => match &w.arg {
                            None => format!("{}(*)", f.name()),
                            Some(p) => format!("{}({})", f.name(), render_program(p, &base)),
                        },
                        WF::Lag(o) | WF::Lead(o) => {
                            let name = if matches!(w.func, WF::Lag(_)) { "lag" } else { "lead" };
                            match &w.default {
                                Some(d) => format!("{name}({}, {o}, {})", argp(), render_program(d, &base)),
                                None => format!("{name}({}, {o})", argp()),
                            }
                        }
                        WF::FirstValue => format!("first_value({})", argp()),
                        WF::LastValue => format!("last_value({})", argp()),
                        WF::NthValue(n) => format!("nth_value({}, {n})", argp()),
                        WF::Ntile(n) => format!("ntile({n})"),
                        // A host window aggregate: the NAME lives beside the
                        // tag, so EXPLAIN shows what the caller wrote.
                        WF::Host => format!(
                            "{}({})",
                            w.host.as_deref().unwrap_or("?"),
                            argp()
                        ),
                        WF::PercentRank => "percent_rank()".to_string(),
                        WF::CumeDist => "cume_dist()".to_string(),
                    };
                    let mut spec = String::new();
                    if !w.partition_by.is_empty() {
                        let ps: Vec<String> =
                            w.partition_by.iter().map(|p| render_program(p, &base)).collect();
                        spec.push_str(&format!("PARTITION BY {}", ps.join(", ")));
                    }
                    if !w.order_by.is_empty() {
                        if !spec.is_empty() {
                            spec.push(' ');
                        }
                        let os: Vec<String> = w
                            .order_by
                            .iter()
                            .map(|(p, desc)| {
                                format!(
                                    "{}{}",
                                    render_program(p, &base),
                                    if *desc { " DESC" } else { " ASC" }
                                )
                            })
                            .collect();
                        spec.push_str(&format!("ORDER BY {}", os.join(", ")));
                    }
                    if let Some(f) = &w.frame {
                        if !spec.is_empty() {
                            spec.push(' ');
                        }
                        spec.push_str(&render_frame(f));
                    }
                    out.push_str(&format!("  window __w{k}: {fname} OVER ({spec})\n"));
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
                        .map(|(c, dir, coll)| {
                            format!(
                                "{}{}{}{}",
                                sort_name(*c),
                                order_collate_suffix(coll),
                                if dir.desc { " DESC" } else { " ASC" },
                                nulls_suffix(*dir)
                            )
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
        }
    }

    /// The DML/txn arms of `explain`.
    fn render_rest(&self, schema: &Schema, out: &mut String) {
        let table_name = |id: u32| {
            if id == super::DUAL_TABLE {
                return "(no FROM — one synthetic row)".to_string();
            }
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
        match &self.stmt {
            PlanStmt::Select(_)
            | PlanStmt::Compound(_)
            | PlanStmt::RecursiveCte(_)
            | PlanStmt::Derived(_) => {
                unreachable!("handled by explain")
            }
            PlanStmt::Insert {
                table,
                rows,
                from_select,
                with_check,
                on_conflict,
                returning,
            } => {
                let name = col_namer(*table);
                out.push_str(&format!("Insert {}\n", table_name(*table)));
                if from_select.is_some() {
                    out.push_str("  source: SELECT\n");
                }
                if let Some(w) = with_check {
                    out.push_str(&format!("  with check: {}\n", render_program(w, &name)));
                }
                match on_conflict {
                    PlanOnConflict::Error => {}
                    PlanOnConflict::DoNothing => out.push_str("  on conflict: do nothing\n"),
                    PlanOnConflict::Replace => {
                        out.push_str("  on conflict: replace (delete conflicts, insert)\n")
                    }
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
            PlanStmt::Savepoint(name) => out.push_str(&format!("Savepoint {name}\n")),
            PlanStmt::Release(name) => out.push_str(&format!("Release {name}\n")),
            PlanStmt::RollbackTo(name) => out.push_str(&format!("RollbackTo {name}\n")),
        }
    }

    fn render_const(&self, i: u16) -> String {
        self.consts
            .get(i as usize)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    }

    /// `outer` names the slots of the accumulated outer tuple, for
    /// `OuterCol` parts inside a join's access path.
    fn render_part_outer(&self, p: &KeyPart, outer: Option<&dyn Fn(u16) -> String>) -> String {
        match p {
            KeyPart::Param(i) => format!("${}", i + 1),
            KeyPart::Const(i) => self.render_const(*i),
            KeyPart::OuterCol(i) => match outer {
                Some(name) => name(*i),
                None => format!("outer#{i}"),
            },
        }
    }

    fn render_access(&self, a: &AccessPath, schema: &Schema, table: u32) -> String {
        self.render_access_outer(a, schema, table, None)
    }

    fn render_access_outer(
        &self,
        a: &AccessPath,
        schema: &Schema,
        table: u32,
        outer: Option<&dyn Fn(u16) -> String>,
    ) -> String {
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
                    .map(|(&c, p)| format!("{} = {}", col_name(c), self.render_part_outer(p, outer)))
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
                    items.push(format!("{} {op} {}", col_name(first), self.render_part_outer(&b.parts[0], outer)));
                }
                if let Some(b) = hi {
                    let op = if b.inclusive { "<=" } else { "<" };
                    items.push(format!("{} {op} {}", col_name(first), self.render_part_outer(&b.parts[0], outer)));
                }
                format!("PkRange({})", items.join(", "))
            }
            AccessPath::IndexPoint { index_no, parts } => {
                let ix = (*index_no as usize)
                    .checked_sub(1)
                    .and_then(|i| schema.table(table).and_then(|t| t.indexes.get(i)));
                // At most one row only when a UNIQUE index is covered to its
                // FULL width (IndexPoint); anything else returns every row
                // equal on the covered prefix (IndexScan) — the label is the
                // honest cost statement.
                let unique = ix.is_none_or(|ix| ix.unique && parts.len() == ix.columns.len());
                let label = if unique { "IndexPoint" } else { "IndexScan" };
                let items: Vec<String> = parts
                    .iter()
                    .enumerate()
                    .map(|(k, part)| {
                        let col = ix.and_then(|ix| ix.columns.get(k).copied()).unwrap_or(0);
                        format!("{} = {}", col_name(col), self.render_part_outer(part, outer))
                    })
                    .collect();
                format!("{label}({}) via index {index_no}", items.join(", "))
            }
            AccessPath::IndexRange { index_no, lo, hi } => {
                let col = (*index_no as usize)
                    .checked_sub(1)
                    .and_then(|i| {
                        schema
                            .table(table)
                            .map(crate::planner::secondary_indexes)
                            .unwrap_or_default()
                            .get(i)
                            .copied()
                            .flatten() // composite (#55): falls back below
                    })
                    .unwrap_or(0);
                let mut items = Vec::new();
                if let Some(b) = lo {
                    let op = if b.inclusive { ">=" } else { ">" };
                    items.push(format!(
                        "{} {op} {}",
                        col_name(col),
                        self.render_part_outer(&b.parts[0], outer)
                    ));
                }
                if let Some(b) = hi {
                    let op = if b.inclusive { "<=" } else { "<" };
                    items.push(format!(
                        "{} {op} {}",
                        col_name(col),
                        self.render_part_outer(&b.parts[0], outer)
                    ));
                }
                format!("IndexRange({}) via index {index_no}", items.join(", "))
            }
            AccessPath::FtsScan { query } => {
                format!("FtsScan({})", render_fts_query(query))
            }
        }
    }
}

/// Render a compiled FTS query tree back to a readable `MATCH`-string form (for
/// EXPLAIN). Cosmetic only.
fn render_fts_query(q: &FtsQuery) -> String {
    match q {
        FtsQuery::Term(t) => {
            let mut s = String::new();
            if !t.columns.is_empty() {
                let cols: Vec<String> = t.columns.iter().map(|c| c.to_string()).collect();
                s.push('{');
                s.push_str(&cols.join(" "));
                s.push_str("}:");
            }
            if t.initial {
                s.push('^');
            }
            s.push_str(&t.token);
            if t.prefix {
                s.push('*');
            }
            s
        }
        FtsQuery::And(a, b) => format!("({} AND {})", render_fts_query(a), render_fts_query(b)),
        FtsQuery::Or(a, b) => format!("({} OR {})", render_fts_query(a), render_fts_query(b)),
        FtsQuery::AndNot(a, b) => format!("({} NOT {})", render_fts_query(a), render_fts_query(b)),
    }
}

/// Render an explicit window frame, e.g. `ROWS BETWEEN 1 PRECEDING AND CURRENT
/// ROW`. Always the `BETWEEN … AND …` long form (the shorthand is desugared away
/// at parse time).
fn render_frame(f: &Frame) -> String {
    let unit = match f.mode {
        FrameMode::Rows => "ROWS",
        FrameMode::Range => "RANGE",
        FrameMode::Groups => "GROUPS",
    };
    let bound = |b: FrameBound| match b {
        FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
        FrameBound::Preceding(n) => format!("{n} PRECEDING"),
        FrameBound::CurrentRow => "CURRENT ROW".to_string(),
        FrameBound::Following(n) => format!("{n} FOLLOWING"),
        FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
    };
    format!("{unit} BETWEEN {} AND {}", bound(f.start), bound(f.end))
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
            // Unary, so it must be listed here rather than falling into the
            // two-operand catch-all at the bottom.
            Instr::BitNot => {
                let a = pop(&mut st);
                Item {
                    s: format!("~{}", wrap(&a)),
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
            Instr::Cast(t) => {
                let a = pop(&mut st);
                Item {
                    s: format!("CAST({} AS {t})", a.s),
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
            // Case-sensitive (PostgreSQL dialect) LIKE renders as LIKE — the
            // surface syntax is identical; the opcode carries the dialect.
            Instr::LikeCs(i) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} LIKE {}", wrap(&a), cst(i)),
                    atom: false,
                }
            }
            // `LIKE … ESCAPE c`, both dialects — the escape const renders as the
            // string literal it is, so the EXPLAIN round-trips to the SQL.
            Instr::LikeEsc(i, e) | Instr::LikeCsEsc(i, e) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} LIKE {} ESCAPE {}", wrap(&a), cst(i), cst(e)),
                    atom: false,
                }
            }
            Instr::Glob(i) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} GLOB {}", wrap(&a), cst(i)),
                    atom: false,
                }
            }
            Instr::Regexp(i) => {
                let a = pop(&mut st);
                Item {
                    s: format!("{} REGEXP {}", wrap(&a), cst(i)),
                    atom: false,
                }
            }
            // The dynamic-pattern form: the pattern is on the stack, so it is
            // popped FIRST (it was pushed last).
            Instr::RegexpDyn => {
                let p = pop(&mut st);
                let a = pop(&mut st);
                Item {
                    s: format!("{} REGEXP {}", wrap(&a), wrap(&p)),
                    atom: false,
                }
            }
            // The dyn-pattern LIKE/GLOB family (#74 item 3, LIKE half): both
            // dialects render as their surface syntax — the opcode carries the
            // dialect, exactly as with Like vs LikeCs. The escape const still
            // renders as the string literal it is.
            Instr::LikeDyn | Instr::LikeCsDyn => {
                let p = pop(&mut st);
                let a = pop(&mut st);
                Item {
                    s: format!("{} LIKE {}", wrap(&a), wrap(&p)),
                    atom: false,
                }
            }
            Instr::LikeDynEsc(e) | Instr::LikeCsDynEsc(e) => {
                let p = pop(&mut st);
                let a = pop(&mut st);
                Item {
                    s: format!("{} LIKE {} ESCAPE {}", wrap(&a), wrap(&p), cst(e)),
                    atom: false,
                }
            }
            Instr::GlobDyn => {
                let p = pop(&mut st);
                let a = pop(&mut st);
                Item {
                    s: format!("{} GLOB {}", wrap(&a), wrap(&p)),
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
                // The five value-taking JSON writers carry a binder-computed
                // JSON-subtype BITMASK as a hidden leading argument. It is not
                // something anyone wrote, so it is not shown: rendering it
                // would put `json_array(0, 1)` in a result-set column NAME
                // where sqlite (and the query text) say `json_array(1)`.
                if matches!(
                    f,
                    mpedb_types::ScalarFn::JsonArray
                        | mpedb_types::ScalarFn::JsonObject
                        | mpedb_types::ScalarFn::JsonSet
                        | mpedb_types::ScalarFn::JsonInsert
                        | mpedb_types::ScalarFn::JsonReplace
                ) && !args.is_empty()
                {
                    args.remove(0);
                }
                // `->` and `->>` are OPERATORS in the grammar; render them as
                // written rather than as the calls they lower to.
                if matches!(
                    f,
                    mpedb_types::ScalarFn::JsonArrow | mpedb_types::ScalarFn::JsonArrowText
                ) && args.len() == 2
                {
                    Item {
                        s: format!("{} {} {}", args[0], f.name(), args[1]),
                        atom: false,
                    }
                } else {
                    Item {
                        s: format!("{}({})", f.name(), args.join(", ")),
                        atom: true,
                    }
                }
            }
            // A host-registered scalar UDF `name(a, b)` — the name lives in the
            // const pool; pop `argc` arguments (never the fallback's fixed two).
            Instr::HostCall(name_idx, argc) => {
                let mut args: Vec<String> = (0..argc).map(|_| pop(&mut st).s).collect();
                args.reverse();
                let name = p
                    .consts
                    .get(name_idx as usize)
                    .and_then(|v| match v {
                        mpedb_types::Value::Text(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "?".into());
                Item {
                    s: format!("{name}({})", args.join(", ")),
                    atom: true,
                }
            }
            // Comparison affinity applied to one operand, in place — rendered
            // as a pseudo-call so the operand it belongs to stays visible.
            Instr::Affinity(aff) => {
                let a = pop(&mut st);
                Item {
                    s: format!("affinity({}, {})", a.s, aff.name()),
                    atom: true,
                }
            }
            // A storage-class comparison over a typeless column.
            Instr::CmpClass(kind, coll) => {
                let b = pop(&mut st);
                let a = pop(&mut st);
                let tail = match coll {
                    Collation::Binary => String::new(),
                    c => format!(" COLLATE {}", c.name()),
                };
                Item {
                    s: format!("{} {} {}{tail}", wrap(&a), kind.symbol(), wrap(&b)),
                    atom: false,
                }
            }
            // A collated comparison `a <op> b COLLATE <coll>`.
            Instr::CmpColl(kind, coll) => {
                let b = pop(&mut st);
                let a = pop(&mut st);
                Item {
                    s: format!(
                        "{} {} {} COLLATE {}",
                        wrap(&a),
                        kind.symbol(),
                        wrap(&b),
                        coll.name()
                    ),
                    atom: false,
                }
            }
            // A collated `x IN (a, b, c) COLLATE <coll>`.
            Instr::InListColl(n, coll) => {
                let mut items: Vec<String> = (0..n).map(|_| pop(&mut st).s).collect();
                items.reverse();
                let a = pop(&mut st);
                Item {
                    s: format!(
                        "{} IN ({}) COLLATE {}",
                        wrap(&a),
                        items.join(", "),
                        coll.name()
                    ),
                    atom: false,
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
                    Instr::Concat => "||",
                    Instr::BitAnd => "&",
                    Instr::BitOr => "|",
                    Instr::Shl => "<<",
                    Instr::Shr => ">>",
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
