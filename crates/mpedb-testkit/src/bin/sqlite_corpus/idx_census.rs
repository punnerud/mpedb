// ====================================================== index-candidate census
//
// `--index-census[=out.tsv]` (task #118). The sibling of the #117 footprint
// census, and the same reuse of the same statement stream: the corpus is the
// only REAL SQL workload this repo has at scale, and the question #118 needs
// answered before any of its design is worth building is a single number —
// **how many DISTINCT (table, key column set, predicate) index candidates does
// a real workload generate?** If it is thousands, workload-derived indexing is
// a search problem; if it is dozens, it is an enumeration.
//
// The candidate is derived from the SAME compiled plan `execute` runs, not from
// the SQL text: the plan's `AccessPath` says which columns the planner already
// pinned by equality (those conjuncts are CONSUMED out of `filter` by
// `planner::access::extract_access`, so text analysis would miss exactly the
// columns that matter), and the residual `filter` — a stack-based `ExprProgram`
// — carries every remaining conjunct with its column ordinals intact.
//
// Three candidate FAMILIES are counted, because the corpus is literal-heavy
// (#117 §1: 1.17 statements per distinct plan, i.e. essentially unparameterized)
// while a real ORM binds parameters, and the two disagree about exactly one
// thing — whether a `col = <value>` conjunct is a *predicate* (a constant of the
// application) or an *argument* (a parameter of the query):
//
//   W      (table, key cols)                      — parameterization-INVARIANT.
//   Pnull  W + `IS [NOT] NULL` conjuncts          — survives parameterization:
//                                                   an ORM emits IS NULL as text.
//   Plit   Pnull + `= <const>` / `IN (<consts>)`  — the UPPER bound; a real
//                                                   parameterized app reaches
//                                                   this only for true constants.
//
// OFF by default; one extra compile per statement when on.
use mpedb::Database;
use mpedb_sql::{AccessPath, CompiledPlan, ExprProgram, Instr, OrderOver, PlanStmt, Schema};
use mpedb_types::CmpKind;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

/// Longest key column list a candidate may carry. Beyond this an index is
/// not a real proposal, and the corpus never gets close.
const MAX_KEY: usize = 8;

#[derive(Default)]
pub struct IdxCensus {
    /// statements that compiled through `mpedb_sql::prepare`
    pub compiled: u64,
    /// statements that did not compile in the prepare-only pass
    pub uncompilable: u64,
    /// compiled, but the shape carries no single-table candidate
    /// (join / compound / recursive / derived / INSERT / txn control)
    pub skipped_shape: u64,
    /// single-table shape whose predicate pinned no column at all
    pub no_key: u64,
    /// filter contained a jump (CASE / COALESCE): conjunct split refused
    pub opaque_filter: u64,
    /// occurrences per candidate, per family
    w: HashMap<String, u64>,
    pnull: HashMap<String, u64>,
    plit: HashMap<String, u64>,
    /// distinct predicates seen (non-empty only)
    pnull_preds: BTreeSet<String>,
    plit_preds: BTreeSet<String>,
    /// candidates already served by an existing index / PK prefix
    pub served: u64,
    pub novel: u64,
    /// W candidates keyed by table, for the per-table search-space size
    per_table: BTreeMap<String, BTreeSet<String>>,
    /// how the access path resolved
    pub acc_pk_point: u64,
    pub acc_pk_range: u64,
    pub acc_ix_point: u64,
    pub acc_ix_range: u64,
    pub acc_full: u64,
    /// key-column-count histogram (capped 8+)
    pub key_hist: [u64; 9],
    /// conjuncts whose RHS was a parameter rather than a constant — the
    /// class a partial index can never be probed by without a runtime
    /// access-path choice.
    pub param_rhs: u64,
    pub const_rhs: u64,
}

static CENSUS: Mutex<Option<IdxCensus>> = Mutex::new(None);
static OUT: Mutex<Option<String>> = Mutex::new(None);

pub fn enable(out: Option<String>) {
    *CENSUS.lock().unwrap() = Some(IdxCensus::default());
    *OUT.lock().unwrap() = out;
}

// ---- expression decomposition ----------------------------------------

/// One sub-expression of a program: the instruction that produced it and
/// the argument nodes it consumed.
struct Node {
    instr: usize,
    kids: Vec<usize>,
}

/// Number of stack slots an instruction pops. `None` = an opcode this
/// census does not model (every jump, and anything added later) — the
/// caller then treats the whole filter as opaque rather than guessing.
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

/// Forward symbolic-stack walk producing one [`Node`] per sub-expression.
/// Returns `None` when any instruction is unmodelled (a jump, i.e. CASE /
/// COALESCE, whose control flow a linear walk cannot follow).
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

/// Split a boolean root into top-level `AND` conjuncts.
fn conjuncts(p: &ExprProgram, nodes: &[Node], root: usize, out: &mut Vec<usize>) {
    if matches!(p.instrs[nodes[root].instr], Instr::And) {
        let (a, b) = (nodes[root].kids[0], nodes[root].kids[1]);
        conjuncts(p, nodes, a, out);
        conjuncts(p, nodes, b, out);
    } else {
        out.push(root);
    }
}

enum Rhs {
    Const(usize),
    Param,
    Other,
}

fn leaf(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16) -> Rhs {
    if !nodes[n].kids.is_empty() {
        return Rhs::Other;
    }
    match p.instrs[nodes[n].instr] {
        Instr::PushConst(k) => Rhs::Const(k as usize),
        // A slot at or past `n_user` is a lifted-subquery / session-context
        // result, not a caller parameter: neither a constant nor an
        // argument an advisor may reason about.
        Instr::PushParam(k) if k < n_user => Rhs::Param,
        _ => Rhs::Other,
    }
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

/// What one conjunct contributes to a candidate.
enum Conj {
    /// `col = <const|param>` — an index KEY column.
    Eq(u16, bool),
    /// `col <op> <const|param>` — a trailing key column (one only).
    Range(u16),
    /// `col IS [NOT] NULL` — a partial-index PREDICATE that survives
    /// parameterization.
    Null(u16, bool),
    /// `col IN (<all consts>)` — key column, and a literal predicate.
    InConst(u16),
    Other,
}

fn classify(p: &ExprProgram, nodes: &[Node], n: usize, n_user: u16, c: &mut IdxCensus) -> Conj {
    use Instr::*;
    let root = &p.instrs[nodes[n].instr];
    let kids = &nodes[n].kids;
    // `col IS [NOT] NULL`
    if kids.len() == 1 {
        if let Some(col) = as_col(p, nodes, kids[0]) {
            match root {
                IsNull => return Conj::Null(col, true),
                IsNotNull => return Conj::Null(col, false),
                _ => {}
            }
        }
    }
    // `col IN (e1..en)` with every element a constant
    if let InList(k) | InListColl(k, _) = root {
        let k = *k as usize;
        if kids.len() == k + 1 {
            if let Some(col) = as_col(p, nodes, kids[0]) {
                if kids[1..]
                    .iter()
                    .all(|&e| matches!(leaf(p, nodes, e, n_user), Rhs::Const(_)))
                {
                    return Conj::InConst(col);
                }
                return Conj::Eq(col, false);
            }
        }
        return Conj::Other;
    }
    // `col <cmp> <atom>` in either operand order
    if kids.len() == 2 {
        let eq = matches!(root, Eq | IsNotDistinct)
            || matches!(root, CmpColl(k, _) | CmpClass(k, _) if *k == CmpKind::Eq);
        let rng = matches!(root, Lt | Le | Gt | Ge)
            || matches!(root, CmpColl(k, _) | CmpClass(k, _)
                if matches!(k, CmpKind::Lt | CmpKind::Le
                             | CmpKind::Gt | CmpKind::Ge));
        if eq || rng {
            for (a, b) in [(kids[0], kids[1]), (kids[1], kids[0])] {
                if let Some(col) = as_col(p, nodes, a) {
                    match leaf(p, nodes, b, n_user) {
                        Rhs::Const(_) => {
                            c.const_rhs += 1;
                            return if eq { Conj::Eq(col, true) } else { Conj::Range(col) };
                        }
                        Rhs::Param => {
                            c.param_rhs += 1;
                            return if eq { Conj::Eq(col, false) } else { Conj::Range(col) };
                        }
                        Rhs::Other => {}
                    }
                }
            }
        }
    }
    Conj::Other
}

// ---- the census ------------------------------------------------------

fn render_const(p: &ExprProgram, k: usize) -> String {
    match p.consts.get(k) {
        Some(v) => format!("{v:?}"),
        None => "?".into(),
    }
}

/// Analyse one single-table statement and fold its candidate into the
/// three families.
#[allow(clippy::too_many_arguments)]
fn fold(
    c: &mut IdxCensus,
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    order: &[u16],
) {
    let Some(t) = schema.tables.get(table as usize) else {
        c.skipped_shape += 1;
        return;
    };
    let name = |i: u16| -> String {
        t.columns
            .get(i as usize)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("#{i}"))
    };

    // 1. Columns the ACCESS PATH already pinned (their conjuncts were
    //    consumed out of `filter` by the planner).
    let mut eq: Vec<u16> = Vec::new();
    let mut range: Option<u16> = None;
    match access {
        AccessPath::PkPoint(_) => {
            c.acc_pk_point += 1;
            eq.extend(t.primary_key.iter().copied());
        }
        AccessPath::PkRange { .. } => {
            c.acc_pk_range += 1;
            range = t.primary_key.first().copied();
        }
        AccessPath::IndexPoint { index_no, parts } => {
            c.acc_ix_point += 1;
            if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                eq.extend(ix.columns.iter().take(parts.len()).copied());
            }
        }
        AccessPath::IndexRange { index_no, .. } => {
            c.acc_ix_range += 1;
            if let Some(ix) = t.indexes.get(*index_no as usize - 1) {
                range = ix.columns.first().copied();
            }
        }
        AccessPath::FullScan => c.acc_full += 1,
        AccessPath::FtsScan { .. } => {
            c.skipped_shape += 1;
            return;
        }
    }

    // 2. Residual conjuncts.
    let mut nulls: Vec<(u16, bool)> = Vec::new();
    let mut lits: Vec<(u16, String)> = Vec::new();
    if let Some(f) = filter {
        let Some((nodes, root)) = decompose(f) else {
            c.opaque_filter += 1;
            return;
        };
        let mut cs = Vec::new();
        conjuncts(f, &nodes, root, &mut cs);
        for n in cs {
            match classify(f, &nodes, n, plan.n_user_params(), c) {
                Conj::Eq(col, is_const) => {
                    if !eq.contains(&col) {
                        eq.push(col);
                    }
                    if is_const {
                        if let Some(&k) = nodes[n].kids.iter().find(|&&x| {
                            matches!(leaf(f, &nodes, x, plan.n_user_params()), Rhs::Const(_))
                        }) {
                            if let Rhs::Const(ci) = leaf(f, &nodes, k, plan.n_user_params()) {
                                lits.push((col, format!("={}", render_const(f, ci))));
                            }
                        }
                    }
                }
                Conj::InConst(col) => {
                    if !eq.contains(&col) {
                        eq.push(col);
                    }
                    lits.push((col, format!("IN[{}]", nodes[n].kids.len() - 1)));
                }
                Conj::Range(col) => {
                    if range.is_none() && !eq.contains(&col) {
                        range = Some(col);
                    }
                }
                Conj::Null(col, isnull) => nulls.push((col, isnull)),
                Conj::Other => {}
            }
        }
    }

    // 3. Key column list: equalities (sorted, canonical), then ONE range
    //    column, then the ORDER BY tail.
    eq.sort_unstable();
    eq.dedup();
    let mut key: Vec<u16> = eq.clone();
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
        c.no_key += 1;
        return;
    }
    c.key_hist[key.len().min(8)] += 1;

    // 4. Is it already served? A candidate is served when some existing
    //    index (or the PK) has the candidate's key as a PREFIX of its key
    //    columns in the order the candidate needs, or when the candidate's
    //    equality set is a prefix-cover of one.
    let covers = |cols: &[u16]| -> bool {
        key.len() <= cols.len() && key.iter().zip(cols).all(|(a, b)| a == b)
    };
    let served = covers(&t.primary_key) || t.indexes.iter().any(|ix| covers(&ix.columns));
    if served {
        c.served += 1;
    } else {
        c.novel += 1;
    }

    // 5. Fold into the three families.
    let cols: Vec<String> = key.iter().map(|&i| name(i)).collect();
    let w = format!("{}({})", t.name, cols.join(","));
    *c.w.entry(w.clone()).or_default() += 1;
    c.per_table.entry(t.name.clone()).or_default().insert(w.clone());

    let inkey = |col: u16| key.contains(&col);
    let mut pn: Vec<String> = nulls
        .iter()
        .filter(|(col, _)| !inkey(*col))
        .map(|(col, isnull)| {
            format!("{} IS {}NULL", name(*col), if *isnull { "" } else { "NOT " })
        })
        .collect();
    pn.sort();
    pn.dedup();
    let mut pl = pn.clone();
    for (col, s) in &lits {
        if !inkey(*col) {
            pl.push(format!("{}{}", name(*col), s));
        }
    }
    pl.sort();
    pl.dedup();

    let key_pn = if pn.is_empty() {
        w.clone()
    } else {
        let p = pn.join(" AND ");
        c.pnull_preds.insert(p.clone());
        format!("{w} WHERE {p}")
    };
    let key_pl = if pl.is_empty() {
        w.clone()
    } else {
        let p = pl.join(" AND ");
        c.plit_preds.insert(p.clone());
        format!("{w} WHERE {p}")
    };
    *c.pnull.entry(key_pn).or_default() += 1;
    *c.plit.entry(key_pl).or_default() += 1;
}

/// Compile `sql` prepare-only and fold its index candidate.
pub fn observe(db: &Database, sql: &str) {
    let mut guard = CENSUS.lock().unwrap();
    let Some(c) = guard.as_mut() else { return };
    let bundle = db.schema();
    let plan = match mpedb_sql::prepare(sql, &bundle.schema) {
        Ok(p) => p,
        Err(_) => {
            c.uncompilable += 1;
            return;
        }
    };
    c.compiled += 1;
    match &plan.stmt {
        PlanStmt::Select(sp) if sp.joins.is_empty() && sp.windows.is_empty() => {
            // ORDER BY extends the key ONLY when it sorts the base row and
            // the statement neither aggregates nor dedups — otherwise the
            // ordinals do not name base columns.
            let order: Vec<u16> = if sp.order_over == OrderOver::BaseRow
                && sp.aggregate.is_none()
                && !sp.distinct
            {
                sp.order_by.iter().map(|(i, _, _)| *i).collect()
            } else {
                Vec::new()
            };
            let table = sp.table;
            let access = sp.access.clone();
            let filter = sp.filter.clone();
            fold(c, &bundle.schema, &plan, table, &access, filter.as_ref(), &order);
        }
        PlanStmt::Update {
            table,
            access,
            filter,
            ..
        }
        | PlanStmt::Delete {
            table,
            access,
            filter,
            ..
        } => {
            let (t, a, f) = (*table, access.clone(), filter.clone());
            fold(c, &bundle.schema, &plan, t, &a, f.as_ref(), &[]);
        }
        _ => c.skipped_shape += 1,
    }
}

fn top(m: &HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = m.iter().map(|(k, &o)| (k.clone(), o)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

fn partial_count(m: &HashMap<String, u64>) -> (usize, u64) {
    let mut n = 0;
    let mut occ = 0;
    for (k, &o) in m {
        if k.contains(" WHERE ") {
            n += 1;
            occ += o;
        }
    }
    (n, occ)
}

pub fn report() {
    let guard = CENSUS.lock().unwrap();
    let Some(c) = guard.as_ref() else { return };
    println!("\n=== index-candidate census (task #118) ===");
    println!("compiled statements          {}", c.compiled);
    println!("uncompilable (prepare-only)  {}", c.uncompilable);
    println!("skipped (not single-table)   {}", c.skipped_shape);
    println!("opaque filter (CASE/jump)    {}", c.opaque_filter);
    println!("single-table, no key pinned  {}", c.no_key);
    let cand: u64 = c.w.values().sum();
    println!("statements yielding a candidate {cand}");
    println!(
        "access path: PkPoint {} PkRange {} IndexPoint {} IndexRange {} FullScan {}",
        c.acc_pk_point, c.acc_pk_range, c.acc_ix_point, c.acc_ix_range, c.acc_full
    );
    println!("candidate already served by an existing index/PK  {}", c.served);
    println!("candidate NOVEL (no index covers it)              {}", c.novel);
    println!(
        "comparison RHS: const {}  param {}",
        c.const_rhs, c.param_rhs
    );
    print!("key width histogram:");
    for (i, n) in c.key_hist.iter().enumerate() {
        if *n > 0 {
            print!(" {i}={n}");
        }
    }
    println!();

    for (label, m, preds) in [
        ("W     (table, key cols)", &c.w, None),
        ("Pnull (+ IS [NOT] NULL)", &c.pnull, Some(&c.pnull_preds)),
        ("Plit  (+ = const / IN)", &c.plit, Some(&c.plit_preds)),
    ] {
        let (np, occp) = partial_count(m);
        println!(
            "\n{label}: {} distinct candidates   partial {np} ({:.1}%)  occurrences under a partial {occp}",
            m.len(),
            if m.is_empty() { 0.0 } else { 100.0 * np as f64 / m.len() as f64 }
        );
        if let Some(p) = preds {
            println!("  distinct predicates: {}", p.len());
        }
        // coverage of the top-K candidate set
        let mut v: Vec<u64> = m.values().copied().collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        let total: u64 = v.iter().sum();
        for k in [1usize, 8, 32, 128] {
            let s: u64 = v.iter().take(k).sum();
            if total > 0 && k <= v.len() {
                println!("  top {k:>3} candidates cover {:.1}% of occurrences", 100.0 * s as f64 / total as f64);
            }
        }
        for (name, occ) in top(m, 10) {
            println!("  {occ:>8}  {name}");
        }
    }

    println!("\nper-table candidate counts (distinct W per table):");
    let mut pt: Vec<(&String, usize)> = c.per_table.iter().map(|(k, v)| (k, v.len())).collect();
    pt.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (t, n) in pt.iter().take(15) {
        println!("  {n:>5}  {t}");
    }
    println!("  tables with candidates: {}", c.per_table.len());

    if let Some(path) = OUT.lock().unwrap().as_ref() {
        let mut s = String::new();
        for (k, occ) in top(&c.plit, usize::MAX) {
            s.push_str(&format!("{occ}\t{k}\n"));
        }
        let _ = std::fs::write(path, s);
        println!("wrote {} candidate lines to {path}", c.plit.len());
    }
}
