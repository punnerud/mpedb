//! The planner (DESIGN-INGEST §7): given what the receipts observed and a
//! budget vector, how often should each call be made?
//!
//! Route optimisation, and the same shape: the navigator knows the network
//! (the call graph), the traffic (measured change rates and fan-outs) and
//! the fuel (calls and bytes per window), and computes the route. The
//! driver still drives — nothing here makes a call.
//!
//! **The objective is HARMONIC STALENESS, not binary freshness**, and the
//! difference is not academic. Binary freshness's budget-optimum is an
//! inverted U in the change rate: the fastest-changing table is polled
//! EXACTLY ZERO times (Cho & García-Molina TODS'03 Thm 5.5 — "penalize the
//! elements that change too often"). For a mirror that must catch every
//! change that is a catastrophe wearing an optimum's clothes. The harmonic
//! penalty `C(n) = Σ1/i` for n unseen changes starves nothing, and its
//! optimum has a closed form.
//!
//! Uniform allocation is computed alongside as the CONTROL ARM, because
//! the other counterintuitive result is that proportional-to-change-rate
//! loses to uniform under every distribution of λ (Thms 5.1/5.2). If the
//! solver cannot beat uniform on a source, the report says so.

use mpedb_types::{Result, Value};

use crate::ingest::{EdgeKind, EdgeSpec, EdgeState, IngestSpec};
use crate::rretl::rows_of;

/// What one edge should do, and why.
#[derive(Debug, Clone)]
pub struct EdgePlan {
    pub edge: String,
    pub table: String,
    pub strategy: String,
    pub kind: String,
    /// Calls of THIS edge per budget window.
    pub rate_per_window: f64,
    /// Seconds between calls. `f64::INFINITY` when the rate is zero.
    pub interval_secs: f64,
    /// Ready to paste into a crontab.
    pub cron: String,
    /// This edge's own calls plus everything its children cost per call.
    pub effective_calls: f64,
    pub effective_bytes: f64,
    /// Estimated changes per window, from the receipts.
    pub lambda_per_window: f64,
    /// Observed keys per parent call (derived edges only).
    pub fanout: f64,
    pub reason: String,
}

/// One profile's allocation.
#[derive(Debug, Clone)]
pub struct ProfilePlan {
    pub profile: String,
    pub window_secs: i64,
    pub budget_calls: i64,
    pub budget_bytes: i64,
    pub used_calls: f64,
    pub used_bytes: f64,
    pub edges: Vec<EdgePlan>,
    /// What uniform allocation would have achieved, for comparison. The
    /// control arm — uniform is a much stronger baseline than intuition
    /// suggests, and beats proportional-to-change-rate always.
    pub uniform_staleness: f64,
    pub solved_staleness: f64,
}

impl ProfilePlan {
    /// Lower is better. `Σ μ·(−ln(ρ/(Δ+ρ)))`, the harmonic objective.
    pub fn verdict(&self) -> String {
        if self.solved_staleness <= self.uniform_staleness * 1.001 {
            format!(
                "staleness {:.3} vs uniform {:.3} — the allocation earns its keep",
                self.solved_staleness, self.uniform_staleness
            )
        } else {
            format!(
                "staleness {:.3} vs uniform {:.3} — UNIFORM IS BETTER HERE; the estimates \
                 are probably too thin to allocate on",
                self.solved_staleness, self.uniform_staleness
            )
        }
    }
}

#[derive(Debug, Clone)]
pub struct IngestPlan {
    pub source: String,
    pub profiles: Vec<ProfilePlan>,
    /// Everything that could NOT be planned, named. Never a silent drop —
    /// the advisor's census rule.
    pub skipped: Vec<String>,
}

impl IngestPlan {
    /// Crontab lines, ready to paste.
    pub fn cron(&self, command: &str) -> Vec<String> {
        let mut out = vec![format!("# ingest plan for `{}`", self.source)];
        for p in &self.profiles {
            out.push(format!(
                "#   {} profile: {} call(s) / {}s — {}",
                p.profile,
                p.budget_calls,
                p.window_secs,
                p.verdict()
            ));
            for e in &p.edges {
                if e.rate_per_window <= 0.0 {
                    continue;
                }
                out.push(format!(
                    "{}  {command} {} {}   # {}",
                    e.cron, self.source, e.edge, e.reason
                ));
            }
        }
        for s in &self.skipped {
            out.push(format!("# NOT PLANNED: {s}"));
        }
        out
    }
}

/// Floor rates, in calls per DAY. A dump that is scheduled to run never is
/// a source whose deletes never arrive and whose cursor is never re-tried
/// — so a reconcile has a floor no allocation may take below it.
const DUMP_FLOOR_PER_DAY: f64 = 1.0;
/// A never-observed edge is not free. It prices at a floor so it gets
/// scheduled at all, which is what earns it the observations that let the
/// next plan price it properly.
const UNOBSERVED_LAMBDA_PER_WINDOW: f64 = 0.5;

/// Everything the solver needs about one root edge, with its children's
/// cost already folded in.
struct Root {
    idx: usize,
    weight: f64,
    lambda: f64,
    calls: f64,
    bytes: f64,
    floor: f64,
}

/// The harmonic optimum for one root at a given Lagrange scalar:
/// `ρ = (√(Δ² + 4μΔ/(λc)) − Δ)/2`.
///
/// The cost `c` enters through the multiplier because the constraint is
/// `Σ ρ·c ≤ R` rather than `Σ ρ ≤ R` — an expensive call buys less rate
/// for the same budget, which is the whole point of costing the graph.
fn rho_at(r: &Root, lambda: f64) -> f64 {
    if r.lambda <= 0.0 || lambda <= 0.0 || r.calls <= 0.0 {
        return 0.0;
    }
    let d = r.lambda;
    let inner = d * d + 4.0 * r.weight * d / (lambda * r.calls);
    ((inner.max(0.0).sqrt() - d) / 2.0).max(0.0)
}

/// `Σ μ·(−ln(ρ/(Δ+ρ)))` — the objective being minimised. A rate of zero
/// against a nonzero change rate is infinite staleness, which is exactly
/// why the harmonic objective never chooses it.
fn staleness(roots: &[Root], rates: &[f64]) -> f64 {
    roots
        .iter()
        .zip(rates)
        .map(|(r, &rho)| {
            if r.lambda <= 0.0 {
                return 0.0;
            }
            if rho <= 0.0 {
                return f64::INFINITY;
            }
            -r.weight * (rho / (r.lambda + rho)).ln()
        })
        .sum()
}

/// Solve `Σ ρ_i·c_i = budget` by bisection on the single Lagrange scalar.
/// Monotone in λ (a larger λ prices every rate down), so bisection is
/// exact to any tolerance in O(|roots|·log(1/ε)).
fn solve_rates(roots: &[Root], budget: f64) -> Vec<f64> {
    let cost_at = |lambda: f64| -> f64 {
        roots.iter().map(|r| rho_at(r, lambda) * r.calls).sum::<f64>()
    };
    // Bracket: λ→0 spends unboundedly, λ→∞ spends nothing.
    let (mut lo, mut hi) = (1e-12f64, 1e12f64);
    for _ in 0..200 {
        let mid = (lo * hi).sqrt(); // geometric — the scale spans decades
        if cost_at(mid) > budget {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi / lo < 1.0 + 1e-9 {
            break;
        }
    }
    let mut rates: Vec<f64> = roots.iter().map(|r| rho_at(r, hi)).collect();
    // Saturate-remove-recurse for the floors: a rate below its floor is
    // pinned there, its cost taken off the top, and the rest re-solved
    // (SIGIR 2019 Alg. 2, with floors in place of per-host caps).
    let mut pinned = vec![false; roots.len()];
    for _ in 0..roots.len() {
        let mut violated = None;
        for (i, r) in roots.iter().enumerate() {
            if !pinned[i] && rates[i] < r.floor {
                violated = Some(i);
                break;
            }
        }
        let Some(i) = violated else { break };
        pinned[i] = true;
        rates[i] = roots[i].floor;
        let spent: f64 = roots
            .iter()
            .enumerate()
            .filter(|(j, _)| pinned[*j])
            .map(|(j, r)| rates[j] * r.calls)
            .sum();
        let rest: Vec<Root> = roots
            .iter()
            .enumerate()
            .filter(|(j, _)| !pinned[*j])
            .map(|(_, r)| Root {
                idx: r.idx,
                weight: r.weight,
                lambda: r.lambda,
                calls: r.calls,
                bytes: r.bytes,
                floor: r.floor,
            })
            .collect();
        if rest.is_empty() {
            break;
        }
        let sub = solve_rates(&rest, (budget - spent).max(0.0));
        let mut k = 0;
        for (j, _) in roots.iter().enumerate() {
            if !pinned[j] {
                rates[j] = sub[k];
                k += 1;
            }
        }
    }
    rates
}

/// Seconds between calls → a crontab schedule. Cron's floor is one minute;
/// anything faster is emitted as every-minute WITH a note, because
/// pretending otherwise would silently under-deliver the plan.
fn cron_line(interval_secs: f64, profile: &str, work_from: i64, work_to: i64) -> (String, String) {
    let (hours, days) = if profile == "work" {
        (format!("{}-{}", work_from, (work_to - 1).max(work_from)), "1-5".to_string())
    } else {
        ("*".to_string(), "*".to_string())
    };
    if interval_secs <= 60.0 {
        return (
            format!("* {hours} * * {days}"),
            "at cron's one-minute floor — the plan wants it faster, so run a loop instead"
                .into(),
        );
    }
    if interval_secs < 3600.0 {
        let m = (interval_secs / 60.0).round().clamp(1.0, 59.0) as i64;
        return (format!("*/{m} {hours} * * {days}"), format!("every {m} min"));
    }
    if interval_secs < 86400.0 {
        let h = (interval_secs / 3600.0).round().clamp(1.0, 23.0) as i64;
        return (format!("17 */{h} * * {days}"), format!("every {h} h"));
    }
    let d = (interval_secs / 86400.0).round().max(1.0) as i64;
    if d <= 1 {
        (format!("17 3 * * {days}"), "daily".into())
    } else {
        (format!("17 3 */{d} * *"), format!("every {d} days"))
    }
}

impl crate::Database {
    /// The plan: which call, how often, in which profile — plus what could
    /// NOT be planned and why.
    pub fn ingest_advise(&self, source: &str) -> Result<IngestPlan> {
        let spec = self.load_ingest(source)?;
        self.resolve_ingest(&spec)?; // the plan must describe a live source
        let mut skipped = Vec::new();

        // Observed model per edge, and the average interval each edge has
        // actually been called at — the estimator counts receipts, and a
        // rate needs a clock to divide by.
        let mut states: Vec<(EdgeState, f64)> = Vec::with_capacity(spec.edges.len());
        for e in &spec.edges {
            let st = crate::ingest::read_state(self, source, &e.name, &e.fingerprint())?;
            let interval = self.observed_interval_secs(source, &e.name)?;
            states.push((st, interval));
        }

        let mut profiles = Vec::new();
        for b in &spec.budget {
            if b.calls <= 0 {
                skipped.push(format!(
                    "profile `{}` has no call budget — nothing can be scheduled in it",
                    b.profile
                ));
                continue;
            }
            let window = b.window_secs.max(1) as f64;
            let mut roots = Vec::new();
            for (i, e) in spec.edges.iter().enumerate() {
                if e.kind != EdgeKind::Root {
                    continue;
                }
                let (st, interval) = &states[i];
                // Δ per window: the LLN estimate is per receipt, so it is
                // scaled by how often receipts actually happened.
                let lambda = if st.receipts < 2 || *interval <= 0.0 {
                    skipped.push(format!(
                        "edge `{}` has {} receipt(s) — priced at the unobserved floor until \
                         it has a history to estimate from",
                        e.name, st.receipts
                    ));
                    UNOBSERVED_LAMBDA_PER_WINDOW
                } else {
                    (st.lambda_per_poll() / interval) * window
                };
                let (calls, bytes) = effective_cost(&spec, &states, e);
                let floor = if e.presents_whole_table() {
                    DUMP_FLOOR_PER_DAY * window / 86400.0
                } else {
                    0.0
                };
                roots.push(Root {
                    idx: i,
                    weight: e.weight as f64,
                    lambda,
                    calls,
                    bytes,
                    floor,
                });
            }
            if roots.is_empty() {
                skipped.push(format!("profile `{}` has no root edges to schedule", b.profile));
                continue;
            }
            let budget = b.calls as f64;
            let rates = solve_rates(&roots, budget);
            // The control arm: the same budget spread evenly by cost.
            let total_cost: f64 = roots.iter().map(|r| r.calls).sum();
            let uni: Vec<f64> = roots.iter().map(|_| budget / total_cost.max(1e-9)).collect();

            let mut edges = Vec::new();
            let (mut used_calls, mut used_bytes) = (0.0, 0.0);
            for (r, &rho) in roots.iter().zip(&rates) {
                let e = &spec.edges[r.idx];
                let interval = if rho > 0.0 { window / rho } else { f64::INFINITY };
                let (cron, when) = cron_line(interval, &b.profile, spec.work_from, spec.work_to);
                used_calls += rho * r.calls;
                used_bytes += rho * r.bytes;
                let reason = plan_reason(e, &states[r.idx].0, r, rho, &when);
                edges.push(EdgePlan {
                    edge: e.name.clone(),
                    table: e.table.clone(),
                    strategy: e.strategy.as_str().into(),
                    kind: e.kind.as_str().into(),
                    rate_per_window: rho,
                    interval_secs: interval,
                    cron,
                    effective_calls: r.calls,
                    effective_bytes: r.bytes,
                    lambda_per_window: r.lambda,
                    fanout: 0.0,
                    reason,
                });
            }
            // Derived edges are not scheduled: their rate IS the parent's
            // rate times the observed fan-out. Report it so the operator
            // can see where the budget actually goes.
            for (i, e) in spec.edges.iter().enumerate() {
                if e.kind == EdgeKind::Root {
                    continue;
                }
                let fan = states[i].0.fanout_per_call();
                let parent_rho = e
                    .parent
                    .as_ref()
                    .and_then(|p| spec.edges.iter().position(|o| o.name.eq_ignore_ascii_case(p)))
                    .and_then(|pi| roots.iter().position(|r| r.idx == pi).map(|k| rates[k]))
                    .unwrap_or(0.0);
                let per_window = parent_rho * (fan / e.batch.max(1) as f64);
                edges.push(EdgePlan {
                    edge: e.name.clone(),
                    table: e.table.clone(),
                    strategy: e.strategy.as_str().into(),
                    kind: e.kind.as_str().into(),
                    rate_per_window: 0.0,
                    interval_secs: f64::INFINITY,
                    cron: String::new(),
                    effective_calls: e.cost_calls as f64,
                    effective_bytes: e.cost_bytes as f64,
                    lambda_per_window: 0.0,
                    fanout: fan,
                    reason: format!(
                        "driven by `{}` — not scheduled; ~{:.1} call(s)/window at the \
                         observed fan-out of {:.1} key(s) per parent call, batched {}",
                        e.parent.as_deref().unwrap_or("?"),
                        per_window,
                        fan,
                        e.batch
                    ),
                });
                if fan <= 0.0 {
                    skipped.push(format!(
                        "edge `{}`'s fan-out has never been observed — its cost is priced at \
                         zero until a receipt reports keys for it",
                        e.name
                    ));
                }
            }
            profiles.push(ProfilePlan {
                profile: b.profile.clone(),
                window_secs: b.window_secs,
                budget_calls: b.calls,
                budget_bytes: b.bytes,
                used_calls,
                used_bytes,
                edges,
                uniform_staleness: staleness(&roots, &uni),
                solved_staleness: staleness(&roots, &rates),
            });
            if b.bytes > 0 && used_bytes > b.bytes as f64 {
                skipped.push(format!(
                    "profile `{}` fits the CALL budget but wants {:.0} bytes/window against a \
                     {} byte budget — raise the byte budget or lower the dump cadence",
                    b.profile, used_bytes, b.bytes
                ));
            }
        }
        if profiles.is_empty() {
            skipped.push(
                "no profile could be planned — declare at least one [[source.budget]]".into(),
            );
        }
        Ok(IngestPlan { source: source.into(), profiles, skipped })
    }

    /// How many seconds have actually elapsed between this edge's receipts.
    /// Zero when there are fewer than two.
    fn observed_interval_secs(&self, source: &str, edge: &str) -> Result<f64> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == crate::ingest::T_STATS) {
            return Ok(0.0);
        }
        let rows = rows_of(self.query(
            "SELECT min(ts_micros), max(ts_micros), count(*) FROM ingest_stats \
             WHERE source = $1 AND edge = $2 AND state = 'closed'",
            &[Value::Text(source.into()), Value::Text(edge.into())],
        )?)?;
        let Some(r) = rows.first() else { return Ok(0.0) };
        let (lo, hi, n) = (
            crate::rretl::as_int(&r[0]).unwrap_or(0),
            crate::rretl::as_int(&r[1]).unwrap_or(0),
            crate::rretl::as_int(&r[2]).unwrap_or(0),
        );
        if n < 2 || hi <= lo {
            return Ok(0.0);
        }
        Ok((hi - lo) as f64 / 1e6 / (n - 1) as f64)
    }
}

/// One root's cost per call, with every descendant's expected cost folded
/// in. Fan-out multiplies along the chain: a root that returns F keys
/// drives F/batch child calls, each of which may drive its own.
fn effective_cost(spec: &IngestSpec, states: &[(EdgeState, f64)], root: &EdgeSpec) -> (f64, f64) {
    fn walk(
        spec: &IngestSpec,
        states: &[(EdgeState, f64)],
        name: &str,
        depth: usize,
    ) -> (f64, f64) {
        if depth > 8 {
            return (0.0, 0.0); // the declaration is acyclic; this is belt
        }
        let mut calls = 0.0;
        let mut bytes = 0.0;
        for (i, e) in spec.edges.iter().enumerate() {
            if !e.parent.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(name)) {
                continue;
            }
            let fan = states[i].0.fanout_per_call();
            let per_parent = fan / e.batch.max(1) as f64;
            calls += per_parent * e.cost_calls as f64;
            bytes += per_parent * e.cost_bytes as f64;
            let (c, b) = walk(spec, states, &e.name, depth + 1);
            calls += per_parent * c;
            bytes += per_parent * b;
        }
        (calls, bytes)
    }
    let (kids_c, kids_b) = walk(spec, states, &root.name, 0);
    (
        (root.cost_calls as f64 + kids_c).max(1e-9),
        root.cost_bytes as f64 + kids_b,
    )
}

fn plan_reason(e: &EdgeSpec, st: &EdgeState, r: &Root, rho: f64, when: &str) -> String {
    let mut why = format!("{}, {when}", e.strategy.as_str());
    if e.presents_whole_table() && rho <= r.floor * 1.001 {
        why.push_str(" — at the reconcile floor: deletes and the cursor trial depend on it");
    }
    match st.cursor_state.as_str() {
        "unsafe" => why.push_str(&format!(
            " — cursor `{}` is UNSAFE ({} missed), so the dump carries the load",
            st.cursor_col, st.missed
        )),
        "safe" => why.push_str(" — cursor verified safe so far"),
        _ => {}
    }
    if e.overlap_secs > 0 {
        why.push_str(&format!("; re-read {}s of overlap", e.overlap_secs));
    }
    if r.lambda > 0.0 {
        why.push_str(&format!("; ~{:.2} change(s)/window observed", r.lambda));
    }
    why
}

/// Every plan the caller might want as one line, for the CLI.
pub fn render(plan: &IngestPlan) -> Vec<String> {
    let mut out = Vec::new();
    for p in &plan.profiles {
        out.push(format!(
            "profile {} — {} call(s)/{}s budget, {:.1} used, {:.0} byte(s) used; {}",
            p.profile,
            p.budget_calls,
            p.window_secs,
            p.used_calls,
            p.used_bytes,
            p.verdict()
        ));
        for e in &p.edges {
            if e.kind == "root" {
                out.push(format!(
                    "  {:<16} {:>8.2}/window  every {:>9}  {}",
                    e.edge,
                    e.rate_per_window,
                    if e.interval_secs.is_finite() {
                        format!("{:.0}s", e.interval_secs)
                    } else {
                        "never".into()
                    },
                    e.reason
                ));
            } else {
                out.push(format!("  {:<16} {:>8}            {}", e.edge, "driven", e.reason));
            }
        }
    }
    for s in &plan.skipped {
        out.push(format!("NOT PLANNED: {s}"));
    }
    out
}
