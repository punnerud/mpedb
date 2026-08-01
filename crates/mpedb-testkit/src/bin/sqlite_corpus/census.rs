// ============================================================ footprint census
//
// `--footprint-census[=out.tsv]` (task #117). The corpus is the only REAL
// statement stream this repo has at scale, and the question it answers is a
// single number: **how many distinct plans map to how many distinct
// footprints?** A footprint cannot be a plan identity (it is not unique), but
// it can be an INDEX over plans — and the plans-per-footprint ratio is exactly
// the fan-out of that index. Near 1:1 means the shape key buys nothing (a
// cost history keyed by shape is a plan history with extra steps); a high
// ratio means one measured shape covers many plans.
//
// The census recompiles each statement through `Database::plan_footprint` —
// the same compile path `prepare`/`execute` take, so the footprint recorded is
// the one the executed statement carries (including the MPEE solver's row-count
// bucket, which is why this is done live on the running database and not in a
// prepare-only pass). It is OFF by default and costs one extra compile per
// statement when on.
use mpedb::{Database, Footprint, TableSet};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Refuse to grow past this many distinct footprints / plans (a 3 GB
/// ulimit is the house rule); the overflow is counted and reported so a
/// truncated census can never be mistaken for a complete one.
const MAX_TRACKED: usize = 4_000_000;

#[derive(Default)]
pub struct Census {
    /// occurrences (statements successfully compiled)
    pub occurrences: u64,
    /// statements that did not compile (unsupported surface; not counted)
    pub uncompilable: u64,
    /// fp key -> (occurrences, distinct plan keys under it)
    fps: HashMap<u64, (u64, HashSet<u64>)>,
    plans: HashSet<u64>,
    overflow: u64,
    /// canonical-encoding bytes summed over OCCURRENCES and over DISTINCT
    /// footprints, current encoding vs the #115 delta/varint form.
    pub bytes_occ: u64,
    pub bytes_occ_delta: u64,
    /// |tables_read| + |tables_written| histogram, capped at 8+.
    pub width_hist: [u64; 9],
    /// One exemplar encoding per distinct footprint, for `--footprint-census=out`.
    exemplars: HashMap<u64, Vec<u8>>,
    /// Per plan: (n, sum ns, sum ns^2) — the WITHIN-plan spread is the
    /// irreducible term (same plan, different data).
    plan_cost: HashMap<u64, (u64, f64, f64)>,
    /// fp key -> the plan keys under it, for the across-plan spread.
    fp_plans: HashMap<u64, HashSet<u64>>,
    /// The REORDER-INVARIANT key: the table sets + read_only, WITHOUT
    /// `indexes_used` / `key_access`. A join reorder never changes which
    /// tables a statement touches, but it does change the access paths
    /// chosen — so `indexes_used` (a bitmap OR'd from the chosen
    /// `AccessPath`s, `planner/footprint.rs:access_key_and_indexes`) moves
    /// under a reorder and the table sets do not. If a plan-variant family
    /// (MPEE ping-pong, DESIGN-MPEE-SOLVER §9.6) is to share one index
    /// bucket, THIS is the key it has to be.
    tsets: HashMap<u64, HashSet<u64>>,
}

static CENSUS: Mutex<Option<Census>> = Mutex::new(None);
static OUT: Mutex<Option<String>> = Mutex::new(None);

pub fn enable(out: Option<String>) {
    *CENSUS.lock().unwrap() = Some(Census::default());
    *OUT.lock().unwrap() = out;
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Length of a `TableSet` under the #115 mechanism: a varint count, the
/// first id as a varint, then each subsequent id as a varint DELTA (the
/// gaps are what a sorted set makes small; a dense run 5,6,7,8 costs
/// 1 byte per element instead of 4).
fn delta_len(ts: &TableSet) -> usize {
    fn varint(mut v: u64) -> usize {
        let mut n = 1;
        while v >= 0x80 {
            v >>= 7;
            n += 1;
        }
        n
    }
    let ids = ts.as_slice();
    let mut n = varint(ids.len() as u64);
    let mut prev = 0u32;
    for (i, &id) in ids.iter().enumerate() {
        n += varint(if i == 0 { id as u64 } else { (id - prev - 1) as u64 });
        prev = id;
    }
    n
}

/// Everything in a footprint EXCEPT the two table sets, under the current
/// encoding: the u64 index bitmap, the read_only byte and the key access.
/// (`indexes_used` is a per-table bitmap, not a sorted set — the #115
/// mechanism does not apply to it, so it is charged unchanged to both
/// arms and the comparison isolates the table sets.)
fn tail_len(fp: &Footprint) -> usize {
    let mut a = Vec::new();
    fp.encode_into(&mut a);
    let mut r = Vec::new();
    fp.tables_read.encode_into(&mut r);
    let mut w = Vec::new();
    fp.tables_written.encode_into(&mut w);
    a.len() - r.len() - w.len()
}

/// Compile `sql` and record its (plan, footprint) pair. Returns the pair
/// of interned keys so the caller can attribute the statement's EXECUTION
/// cost to them (`record_cost`) — the measurement #88 actually needs.
pub fn observe(db: &Database, sql: &str) -> Option<(u64, u64)> {
    let mut guard = CENSUS.lock().unwrap();
    let c = guard.as_mut()?;
    let (hash, fp) = match db.plan_footprint(sql) {
        Ok(v) => v,
        Err(_) => {
            c.uncompilable += 1;
            return None;
        }
    };
    let mut enc = Vec::new();
    fp.encode_into(&mut enc);
    let fk = fnv(&enc);
    let pk = u64::from_le_bytes(hash.0[..8].try_into().unwrap());

    c.occurrences += 1;
    c.bytes_occ += enc.len() as u64;
    c.bytes_occ_delta +=
        (tail_len(&fp) + delta_len(&fp.tables_read) + delta_len(&fp.tables_written)) as u64;
    let width = (fp.tables_read.len() + fp.tables_written.len()).min(8);
    c.width_hist[width] += 1;

    if c.fps.len() >= MAX_TRACKED && !c.fps.contains_key(&fk) {
        c.overflow += 1;
        return None;
    }
    c.plans.insert(pk);
    let slot = c.fps.entry(fk).or_default();
    slot.0 += 1;
    slot.1.insert(pk);
    if c.exemplars.len() < MAX_TRACKED {
        c.exemplars.entry(fk).or_insert(enc);
    }
    c.fp_plans.entry(fk).or_default().insert(pk);
    let mut tenc = Vec::new();
    fp.tables_read.encode_into(&mut tenc);
    fp.tables_written.encode_into(&mut tenc);
    tenc.push(fp.read_only as u8);
    c.tsets.entry(fnv(&tenc)).or_default().insert(pk);
    Some((fk, pk))
}

/// Attribute one execution's wall time to its plan.
pub fn record_cost(key: Option<(u64, u64)>, ns: f64) {
    let Some((_fk, pk)) = key else { return };
    let mut guard = CENSUS.lock().unwrap();
    let Some(c) = guard.as_mut() else { return };
    let e = c.plan_cost.entry(pk).or_insert((0, 0.0, 0.0));
    e.0 += 1;
    e.1 += ns;
    e.2 += ns * ns;
}

/// Print the census and, if `--footprint-census=<path>` was given, write
/// one line per distinct footprint: `occurrences TAB plans TAB hex`.
/// That file is the input the microbench replays, so the conflict and
/// routing measurements run on REAL footprints, not synthetic ones.
pub fn report() {
    let guard = CENSUS.lock().unwrap();
    let Some(c) = guard.as_ref() else { return };
    println!("\n=== footprint census (task #117) ===");
    println!("compiled statements      {}", c.occurrences);
    println!("uncompilable (skipped)   {}", c.uncompilable);
    println!("distinct plans           {}", c.plans.len());
    println!("distinct footprints      {}", c.fps.len());
    if c.overflow > 0 {
        println!("TRUNCATED: {} occurrences past the tracking cap", c.overflow);
    }
    let fpn = c.fps.len().max(1) as f64;
    println!(
        "plans per footprint      mean {:.2}",
        c.plans.len() as f64 / fpn
    );
    let mut fan: Vec<usize> = c.fps.values().map(|(_, p)| p.len()).collect();
    fan.sort_unstable();
    let pick = |q: f64| fan.get(((fan.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0);
    println!(
        "                         p50 {} p90 {} p99 {} max {}",
        pick(0.50),
        pick(0.90),
        pick(0.99),
        fan.last().copied().unwrap_or(0)
    );
    let singleton = fan.iter().filter(|&&n| n == 1).count();
    println!(
        "footprints with exactly one plan: {singleton} ({:.1} %)",
        100.0 * singleton as f64 / fpn
    );
    // Occurrence-weighted: what a #88 history actually stores.
    let mut occ: Vec<(u64, usize)> = c.fps.values().map(|(o, p)| (*o, p.len())).collect();
    occ.sort_unstable();
    let tot_occ: u64 = occ.iter().map(|(o, _)| o).sum();
    println!(
        "occurrence-weighted mean plans/footprint {:.2}",
        occ.iter().map(|(o, p)| *o as f64 * *p as f64).sum::<f64>() / tot_occ.max(1) as f64
    );
    println!(
        "footprint bytes over occurrences: current {} delta {} ({:+.1} %)",
        c.bytes_occ,
        c.bytes_occ_delta,
        100.0 * (c.bytes_occ_delta as f64 - c.bytes_occ as f64) / c.bytes_occ.max(1) as f64
    );
    print!("|tables_read|+|tables_written| hist:");
    for (i, n) in c.width_hist.iter().enumerate() {
        if *n > 0 {
            print!(" {i}{}={n}", if i == 8 { "+" } else { "" });
        }
    }
    println!();
    let tn = c.tsets.len().max(1) as f64;
    println!(
        "distinct TABLE-SET keys   {}  (plans per table set: mean {:.2})",
        c.tsets.len(),
        c.plans.len() as f64 / tn
    );
    let mut tfan: Vec<usize> = c.tsets.values().map(|p| p.len()).collect();
    tfan.sort_unstable();
    println!(
        "                          p50 {} p90 {} max {}",
        tfan.get(tfan.len() / 2).copied().unwrap_or(0),
        tfan.get(tfan.len() * 9 / 10).copied().unwrap_or(0),
        tfan.last().copied().unwrap_or(0)
    );
    println!(
        "footprint refines the table set by {:.2}x (distinct fps / distinct table sets)",
        c.fps.len() as f64 / tn
    );
    cost_report(c);
    if let Some(path) = OUT.lock().unwrap().as_ref() {
        let mut out = String::new();
        for (fk, (occ, plans)) in &c.fps {
            let hex: String = c
                .exemplars
                .get(fk)
                .map(|e| e.iter().map(|b| format!("{b:02x}")).collect())
                .unwrap_or_default();
            out.push_str(&format!("{occ}\t{}\t{hex}\n", plans.len()));
        }
        match std::fs::write(path, out) {
            Ok(()) => println!("wrote {} distinct footprints to {path}", c.fps.len()),
            Err(e) => println!("could not write {path}: {e}"),
        }
    }
}

/// The question #88 actually has to answer: if a cost history is keyed by
/// SHAPE, how wrong is a measurement borrowed from another plan in the
/// same bucket? Two spreads, both as coefficients of variation:
///
/// - **within-plan**: the same plan re-executed. This is irreducible —
///   data dependence and timer noise. It is the floor.
/// - **across-plan, within-footprint**: the per-plan MEAN costs of the
///   plans that share one footprint. This is the error a shape key
///   introduces on top of the floor.
///
/// If the second is not much larger than the first, the shape key is a
/// legitimate place to pool measurements. If it is far larger, the bucket
/// is conflating plans whose costs differ, and #88 must key on something
/// finer.
fn cost_report(c: &Census) {
    let cv = |n: u64, sum: f64, sq: f64| -> Option<f64> {
        if n < 2 {
            return None;
        }
        let n = n as f64;
        let mean = sum / n;
        let var = (sq / n - mean * mean).max(0.0);
        if mean <= 0.0 {
            None
        } else {
            Some(var.sqrt() / mean)
        }
    };
    let mut within: Vec<f64> = Vec::new();
    for (n, sum, sq) in c.plan_cost.values() {
        if let Some(v) = cv(*n, *sum, *sq) {
            within.push(v);
        }
    }
    let mut across: Vec<f64> = Vec::new();
    let mut across_ratio: Vec<f64> = Vec::new();
    for plans in c.fp_plans.values() {
        let means: Vec<f64> = plans
            .iter()
            .filter_map(|p| c.plan_cost.get(p))
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, sum, _)| sum / *n as f64)
            .collect();
        if means.len() < 2 {
            continue;
        }
        let n = means.len() as f64;
        let m = means.iter().sum::<f64>() / n;
        let var = means.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n;
        if m > 0.0 {
            across.push(var.sqrt() / m);
        }
        let (lo, hi) = means.iter().fold((f64::MAX, 0.0f64), |(a, b), &x| {
            (a.min(x), b.max(x))
        });
        if lo > 0.0 {
            across_ratio.push(hi / lo);
        }
    }
    let med = |v: &mut Vec<f64>| -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let n_within = within.len();
    let n_across = across.len();
    println!("\n-- cost spread (feeds #88) --");
    println!(
        "within-plan CV (irreducible)          median {:.3}  over {n_within} plans",
        med(&mut within)
    );
    println!(
        "across-plan-within-footprint CV       median {:.3}  over {n_across} footprints",
        med(&mut across)
    );
    println!(
        "worst/best plan mean cost in a bucket median {:.1}x  p90 {:.1}x  max {:.1}x",
        med(&mut across_ratio),
        across_ratio
            .get((across_ratio.len().saturating_sub(1)) * 9 / 10)
            .copied()
            .unwrap_or(f64::NAN),
        across_ratio.last().copied().unwrap_or(f64::NAN)
    );
}
