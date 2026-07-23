//! The workload-index advisor — stage E of design/DESIGN-MPEE-GENERAL.md,
//! mode A (recommend-only) of design/DESIGN-WORKLOAD-INDEXES.md §4.
//!
//! The premise (#118 §1): the workload is **enumerable, not sampled** — every
//! statement this database ever compiled is a plan-registry record carrying
//! its full SQL and its full `CompiledPlan` blob, so "which columns does this
//! application filter on" is a scan, not a guess. The candidate extraction
//! here is the engine-side twin of the `--index-census` measurement harness
//! (`mpedb-testkit/src/bin/sqlite_corpus.rs`, which measured 112 candidates
//! over 99,279 real statements); the decomposition rules are carried over
//! verbatim so the advisor recommends exactly what the census counted.
//!
//! **Recommend-only, deliberately.** Auto-create stays blocked on the three
//! prerequisites #118 §7 names — P2 (an index state bit, so a half-built
//! index is never chosen), P3 (`DROP INDEX`, without which auto-create is a
//! ratchet), P5 (per-plan-hash execution counts, without which "how often"
//! is recency, not frequency) — and this module restates them rather than
//! quietly building around them. The ranking axis that IS available today:
//! how many distinct registered plans want the candidate, and the table's
//! row-count magnitude through the stage-A cost seam.

use std::collections::BTreeMap;

use mpedb_sql::{AccessPath, CompiledPlan, OrderOver, PlanStmt, SelectPlan};
use mpedb_types::{CmpKind, Collation, ExprProgram, Instr, Result, Schema};

/// Where the statements come from.
pub enum WorkloadSource {
    /// Every plan in the shared registry — the workload this database has
    /// actually compiled.
    Registry,
    /// An explicit statement list (an offline log, a migration's queries…),
    /// compiled against the CURRENT live schema.
    Statements(Vec<String>),
}

/// One recommended index.
#[derive(Debug, Clone)]
pub struct IndexAdvice {
    pub table: String,
    /// Key columns by name, in recommended order (equalities sorted-canonical,
    /// then at most one range column, then the ORDER BY tail — the census's
    /// candidate shape).
    pub columns: Vec<String>,
    /// Content identity per DESIGN-WORKLOAD-INDEXES §2.2 (names + collation,
    /// never ordinals or statistics), hex — stable across schema reorders and
    /// app versions, so tooling can recognise "the same index" later.
    pub index_id: String,
    /// Distinct compiled statements whose plan would use it.
    pub statements: u64,
    /// log2 bucket of the table's row count (stage-A quantization): the size
    /// axis of the ranking, read through the same seam the planner prices by.
    pub rows_bucket: u32,
    /// One motivating SQL text, for the human reading the advice.
    pub example: String,
}

/// The advice plus the census of what the extraction could not use —
/// the no-silent-caps rule: skipped work is reported, not implied covered.
#[derive(Debug, Default)]
pub struct AdviceReport {
    pub advices: Vec<IndexAdvice>,
    pub compiled: u64,
    pub uncompilable: u64,
    /// Shapes carrying no single-table candidate (joins, compounds, DDL…).
    pub skipped_shape: u64,
    /// Filters with control flow (CASE/COALESCE): conjunct split refused.
    pub opaque_filter: u64,
    /// Single-table shapes that pinned no column.
    pub no_key: u64,
    /// Candidates already served by an existing index / PK prefix.
    pub served: u64,
}

const MAX_KEY: usize = 8;

impl crate::Database {
    /// Derive candidate indexes from the workload and rank them. Read-only:
    /// creates nothing, writes nothing, and says what it skipped.
    pub fn recommend_indexes(&self, source: WorkloadSource) -> Result<AdviceReport> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let schema = &bundle.schema;

        let mut rep = AdviceReport::default();
        // candidate key = (table id, key ordinals) → (count, example sql)
        let mut cands: BTreeMap<(u32, Vec<u16>), (u64, String)> = BTreeMap::new();

        let mut fold_plan = |sql: &str, plan: &CompiledPlan, rep: &mut AdviceReport| {
            rep.compiled += 1;
            match &plan.stmt {
                PlanStmt::Select(sp) => {
                    fold_select(schema, plan, sp, sql, &mut cands, rep);
                }
                PlanStmt::Update { table, access, filter, .. }
                | PlanStmt::Delete { table, access, filter, .. } => {
                    fold_shape(schema, plan, *table, access, filter.as_ref(), &[], sql, &mut cands, rep);
                }
                _ => rep.skipped_shape += 1,
            }
        };

        match source {
            WorkloadSource::Registry => {
                let r = self.engine.begin_read()?;
                let records = r.sys_scan_range(
                    crate::registry::PLAN_PREFIX,
                    crate::registry::PLAN_PREFIX_END,
                )?;
                r.finish()?;
                for (_k, bytes) in records {
                    let Some(rec) = crate::registry::parse_record(&bytes) else {
                        rep.uncompilable += 1;
                        continue;
                    };
                    // Decode re-validates against the LIVE schema; a plan from
                    // before a DDL simply fails validation and is skipped —
                    // stale advice is worse than less advice.
                    match CompiledPlan::decode(rec.blob, schema) {
                        Ok(plan) => fold_plan(rec.sql, &plan, &mut rep),
                        Err(_) => rep.uncompilable += 1,
                    }
                }
            }
            WorkloadSource::Statements(stmts) => {
                for sql in &stmts {
                    match mpedb_sql::prepare(sql, schema) {
                        Ok(plan) => fold_plan(sql, &plan, &mut rep),
                        Err(_) => rep.uncompilable += 1,
                    }
                }
            }
        }

        // Rank: statements desc, then table size (the stage-A bucket — an
        // index on a 2M-row table is worth more than the same index on 200
        // rows), then name for determinism.
        let r = self.engine.begin_read()?;
        let mut advices: Vec<IndexAdvice> = cands
            .into_iter()
            .filter_map(|((tid, key), (n, example))| {
                let t = schema.tables.get(tid as usize)?;
                let columns: Vec<String> = key
                    .iter()
                    .map(|&i| {
                        t.columns
                            .get(i as usize)
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| format!("#{i}"))
                    })
                    .collect();
                let colls: Vec<Collation> = key
                    .iter()
                    .map(|&i| {
                        t.columns
                            .get(i as usize)
                            .map(|c| c.collation)
                            .unwrap_or(Collation::Binary)
                    })
                    .collect();
                Some(IndexAdvice {
                    index_id: index_id_hex(&t.name, &columns, &colls),
                    table: t.name.clone(),
                    columns,
                    statements: n,
                    rows_bucket: crate::stats::bucket(r.row_count(tid).unwrap_or(0)),
                    example,
                })
            })
            .collect();
        r.finish()?;
        advices.sort_by(|a, b| {
            b.statements
                .cmp(&a.statements)
                .then(b.rows_bucket.cmp(&a.rows_bucket))
                .then(a.table.cmp(&b.table))
                .then(a.columns.cmp(&b.columns))
        });
        rep.advices = advices;
        Ok(rep)
    }
}

/// Walk a SELECT (and, when it has joins, each join's unserved inner side is a
/// future refinement — v1 counts the shape as skipped rather than guessing).
fn fold_select(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
    sql: &str,
    cands: &mut BTreeMap<(u32, Vec<u16>), (u64, String)>,
    rep: &mut AdviceReport,
) {
    if !sp.joins.is_empty() {
        rep.skipped_shape += 1;
        return;
    }
    // Base-row ORDER BY columns extend the candidate's tail (a covering sort).
    let order: Vec<u16> = if sp.order_over == OrderOver::BaseRow {
        sp.order_by.iter().map(|(c, _, _)| *c).collect()
    } else {
        Vec::new()
    };
    fold_shape(schema, plan, sp.table, &sp.access, sp.filter.as_ref(), &order, sql, cands, rep);
}

/// The census's `fold`, engine-side: access-pinned columns + residual
/// equality/range conjuncts + ORDER BY tail → one candidate key.
#[allow(clippy::too_many_arguments)]
fn fold_shape(
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    order: &[u16],
    sql: &str,
    cands: &mut BTreeMap<(u32, Vec<u16>), (u64, String)>,
    rep: &mut AdviceReport,
) {
    let Some(t) = schema.tables.get(table as usize) else {
        rep.skipped_shape += 1;
        return;
    };

    // 1. Columns the access path already pinned (their conjuncts were consumed
    //    out of the residual by the planner).
    let mut eq: Vec<u16> = Vec::new();
    let mut range: Option<u16> = None;
    match access {
        AccessPath::PkPoint(_) => eq.extend(t.primary_key.iter().copied()),
        AccessPath::PkRange { .. } => range = t.primary_key.first().copied(),
        AccessPath::IndexPoint { index_no, parts } => {
            if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                eq.extend(ix.columns.iter().take(parts.len()).copied());
            }
        }
        AccessPath::IndexRange { index_no, .. } => {
            if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                range = ix.columns.first().copied();
            }
        }
        AccessPath::FullScan => {}
        AccessPath::FtsScan { .. } => {
            rep.skipped_shape += 1;
            return;
        }
    }

    // 2. Residual conjuncts.
    if let Some(f) = filter {
        let Some((nodes, root)) = decompose(f) else {
            rep.opaque_filter += 1;
            return;
        };
        let mut cs = Vec::new();
        conjuncts(f, &nodes, root, &mut cs);
        for n in cs {
            match classify(f, &nodes, n, plan.n_user_params()) {
                Conj::Eq(col) => {
                    if !eq.contains(&col) {
                        eq.push(col);
                    }
                }
                Conj::Range(col) => {
                    if range.is_none() && !eq.contains(&col) {
                        range = Some(col);
                    }
                }
                Conj::Other => {}
            }
        }
    }

    // 3. The candidate key: equalities sorted-canonical (any permutation
    //    serves an eq set), then ONE range column, then the ORDER BY tail.
    eq.sort_unstable();
    eq.dedup();
    let mut key = eq;
    if let Some(r) = range {
        if !key.contains(&r) {
            key.push(r);
        }
    }
    for &o in order {
        if key.len() >= MAX_KEY {
            break;
        }
        if !key.contains(&o) {
            key.push(o);
        }
    }
    key.truncate(MAX_KEY);
    if key.is_empty() {
        rep.no_key += 1;
        return;
    }

    // 4. Already served? The candidate's key as a PREFIX of the PK or an
    //    existing whole-table index, in order. (A prefix probe of a composite
    //    is real access-path machinery, #55 — served means served.)
    let covers = |cols: &[u16]| key.len() <= cols.len() && key.iter().zip(cols).all(|(a, b)| a == b);
    if covers(&t.primary_key)
        || t.indexes
            .iter()
            .any(|ix| ix.predicate.is_none() && covers(&ix.columns))
    {
        rep.served += 1;
        return;
    }

    let entry = cands.entry((table, key)).or_insert_with(|| (0, sql.to_string()));
    entry.0 += 1;
}

/// The §2.2 identity: names + collation, never ordinals, never statistics.
/// `version ‖ table ‖ unique ‖ n_key ‖ (name ‖ collation ‖ direction)* ‖ pred`.
fn index_id_hex(table: &str, columns: &[String], colls: &[Collation]) -> String {
    let mut b: Vec<u8> = vec![1u8];
    let put_str = |b: &mut Vec<u8>, s: &str| {
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    };
    put_str(&mut b, table);
    b.push(0); // unique = false
    b.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    for (name, coll) in columns.iter().zip(colls) {
        put_str(&mut b, name);
        b.push(match coll {
            Collation::Binary => 0,
            Collation::NoCase => 1,
            Collation::Rtrim => 2,
        });
        b.push(0); // direction: Asc
    }
    b.extend_from_slice(&0u32.to_le_bytes()); // whole-table: empty predicate
    blake3::hash(&b).to_hex().to_string()
}

// ---- expression decomposition (the census's, verbatim rules) ---------------

struct Node {
    instr: usize,
    kids: Vec<usize>,
}

/// Stack slots an instruction pops; `None` = unmodelled (every jump — CASE /
/// COALESCE control flow a linear walk cannot follow) and the whole filter is
/// treated as opaque rather than guessed at.
fn pops(i: &Instr) -> Option<usize> {
    use Instr::*;
    Some(match i {
        PushCol(_) | PushParam(_) | PushConst(_) => 0,
        Neg | Not | IsNull | IsNotNull | ToFloat | Cast(_) | Like(_) | LikeCs(_)
        | LikeEsc(..) | LikeCsEsc(..) | Glob(_) | Regexp(_) | InParam(_) | Affinity(_)
        | BitNot => 1,
        Eq | Ne | Lt | Le | Gt | Ge | Add | Sub | Mul | Div | Mod | And | Or
        | IsNotDistinct | IsDistinct | Concat | BitAnd | BitOr | Shl | Shr | CmpColl(..)
        | CmpClass(..) | LikeDyn | LikeCsDyn | GlobDyn | RegexpDyn | LikeDynEsc(_)
        | LikeCsDynEsc(_) => 2,
        InList(n) | InListColl(n, _) => *n as usize + 1,
        Call(_, argc) => *argc as usize,
        HostCall(_, argc) => *argc as usize,
        _ => return None,
    })
}

fn decompose(p: &ExprProgram) -> Option<(Vec<Node>, usize)> {
    let mut nodes: Vec<Node> = Vec::with_capacity(p.instrs.len());
    let mut stack: Vec<usize> = Vec::new();
    for (pc, ins) in p.instrs.iter().enumerate() {
        let n = pops(ins)?;
        if stack.len() < n {
            return None;
        }
        let kids = stack.split_off(stack.len() - n);
        nodes.push(Node { instr: pc, kids });
        stack.push(nodes.len() - 1);
    }
    let root = *stack.last()?;
    if stack.len() != 1 {
        return None;
    }
    Some((nodes, root))
}

fn conjuncts(p: &ExprProgram, nodes: &[Node], root: usize, out: &mut Vec<usize>) {
    if matches!(p.instrs[nodes[root].instr], Instr::And) {
        let (a, b) = (nodes[root].kids[0], nodes[root].kids[1]);
        conjuncts(p, nodes, a, out);
        conjuncts(p, nodes, b, out);
    } else {
        out.push(root);
    }
}

enum Conj {
    Eq(u16),
    Range(u16),
    Other,
}

fn as_col(p: &ExprProgram, nodes: &[Node], n: usize) -> Option<u16> {
    if !nodes[n].kids.is_empty() {
        return None;
    }
    match p.instrs[nodes[n].instr] {
        Instr::PushCol(c) => Some(c),
        _ => None,
    }
}

fn is_atom(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16) -> bool {
    if !nodes[n].kids.is_empty() {
        return false;
    }
    match p.instrs[nodes[n].instr] {
        Instr::PushConst(_) => true,
        // A slot at/past n_user is a lifted-subquery result, not a caller
        // parameter — not something an index key can be planned around.
        Instr::PushParam(k) => k < n_user,
        _ => false,
    }
}

fn classify(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16) -> Conj {
    use Instr::*;
    let root = &p.instrs[nodes[n].instr];
    let kids = &nodes[n].kids;
    // `col IN (e1..en)`, every element an atom → an equality-class key column.
    if let InList(k) | InListColl(k, _) = root {
        let k = *k as usize;
        if kids.len() == k + 1 {
            if let Some(col) = as_col(p, nodes, kids[0]) {
                if kids[1..].iter().all(|&e| is_atom(p, nodes, e, n_user)) {
                    return Conj::Eq(col);
                }
            }
        }
        return Conj::Other;
    }
    if kids.len() == 2 {
        let eq = matches!(root, Eq | IsNotDistinct)
            || matches!(root, CmpColl(k, _) | CmpClass(k, _) if *k == CmpKind::Eq);
        let rng = matches!(root, Lt | Le | Gt | Ge)
            || matches!(root, CmpColl(k, _) | CmpClass(k, _)
                if matches!(k, CmpKind::Lt | CmpKind::Le | CmpKind::Gt | CmpKind::Ge));
        if eq || rng {
            for (a, b) in [(kids[0], kids[1]), (kids[1], kids[0])] {
                if let Some(col) = as_col(p, nodes, a) {
                    if is_atom(p, nodes, b, n_user) {
                        return if eq { Conj::Eq(col) } else { Conj::Range(col) };
                    }
                }
            }
        }
    }
    Conj::Other
}
