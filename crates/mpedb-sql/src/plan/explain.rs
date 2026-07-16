use super::*;

impl CompiledPlan {
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
            PlanStmt::Select(SelectPlan {
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
            }) => {
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
                    // The cost note is the honest one for THIS join: a
                    // FullScan inner side is read once and paired with every
                    // outer row (O(n*m) ON evaluations); a keyed access is
                    // the index nested loop — the ON equality was consumed
                    // into the inner fetch and runs per outer row.
                    let kind = match j.kind {
                        JoinKind::Inner => "inner",
                        JoinKind::Left => "left",
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
                // A unique probe returns at most one row (IndexPoint); a
                // non-unique index returns every equal row (IndexScan) — the
                // label is the honest cost statement.
                let unique = schema
                    .table(table)
                    .and_then(|t| t.columns.get(col as usize))
                    .is_none_or(|c| c.unique);
                let label = if unique { "IndexPoint" } else { "IndexScan" };
                format!(
                    "{label}({} = {}) via index {index_no}",
                    col_name(col),
                    self.render_part_outer(part, outer)
                )
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
        }
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
                    Instr::Concat => "||",
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
