//! Routing on a real road matrix: exact (mpedb + the kernel's Held-Karp mode)
//! vs the original MPEE solver (brooom), stage M4 of
//! design/DESIGN-MODEL-LANG.md's program.
//!
//! The vecbench frame, applied to sequencing: **exact is the ground truth,
//! the heuristic is scored by gap and time.** For every sub-instance small
//! enough for `mpedb_sql::sequence` (N ≤ 18), the optimum is KNOWN — so
//! brooom's answer gets a gap-to-optimum, not a shrug. Past the cap the exact
//! side DECLINES (never silently degrades) and brooom's regime is reported as
//! such.
//!
//! Agreement before timing, both directions: brooom's claimed route cost is
//! RECOMPUTED from our matrix and must match its own summary; the exact
//! order's cost must equal the solver's claim (already asserted inside the
//! solver's tests). A solver whose claimed cost disagrees with its route is a
//! bug report, not a benchmark row.
//!
//! mpedb's role is the platform: the instance matrix lives in TABLES under
//! `models/routing.toml`, the exact arm reads its submatrix out of the
//! database, and the queries an application would run around a solve
//! (nearest stops) are measured too.

mod json;

use std::fmt::Write as _;
use std::time::Instant;

use mpedb::{Config, Database, ExecResult, Value};
use mpedb_sql::sequence;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut instance = String::new();
    let mut brooom = String::new();
    let mut dir = std::env::temp_dir().join("mpedb-routebench");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--instance" => instance = args.next().ok_or("--instance needs a path")?,
            "--brooom" => brooom = args.next().ok_or("--brooom needs a path")?,
            "--dir" => dir = args.next().ok_or("--dir needs a value")?.into(),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if instance.is_empty() || brooom.is_empty() {
        return Err("usage: mpedb-routebench --instance sf.json --brooom <path-to-brooom>".into());
    }

    // ---- the instance -----------------------------------------------------
    let text = std::fs::read_to_string(&instance)?;
    let doc = json::parse(&text)?;
    let durations = doc
        .get("matrices")
        .and_then(|m| m.get("car"))
        .and_then(|c| c.get("durations"))
        .ok_or("instance has no matrices.car.durations")?;
    let matrix: Vec<Vec<i64>> = durations
        .arr()
        .iter()
        .map(|row| row.arr().iter().filter_map(|v| v.num()).map(|f| f as i64).collect())
        .collect();
    let n = matrix.len();
    if n == 0 || matrix.iter().any(|r| r.len() != n) {
        return Err("instance matrix is not square".into());
    }
    let meta = doc.get("meta");
    println!("# Routing head-to-head: exact (mpedb kernel) vs brooom\n");
    println!(
        "- instance: `{}` — {} locations, matrix `{}`",
        instance.rsplit('/').next().unwrap_or(&instance),
        n,
        meta.and_then(|m| m.get("matrix")).and_then(|v| v.str()).unwrap_or("?"),
    );

    // ---- mpedb as the platform -------------------------------------------
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("route.mpedb");
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 128
max_readers = 8
durability = "none"

[[table]]
name = "stops"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"

[[table]]
name = "matrix"
primary_key = ["src", "dst"]
  [[table.column]]
  name = "src"
  type = "int64"
  [[table.column]]
  name = "dst"
  type = "int64"
  [[table.column]]
  name = "secs"
  type = "int64"
  nullable = false
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml)?)?;
    let ins_stop = db.prepare("INSERT INTO stops (id) VALUES ($1)")?;
    let ins_cell = db.prepare("INSERT INTO matrix (src, dst, secs) VALUES ($1, $2, $3)")?;
    let t0 = Instant::now();
    let mut s = db.begin()?;
    for i in 0..n {
        s.execute(&ins_stop, &[Value::Int(i as i64)])?;
    }
    for (i, row) in matrix.iter().enumerate() {
        for (j, &secs) in row.iter().enumerate() {
            s.execute(&ins_cell, &[Value::Int(i as i64), Value::Int(j as i64), Value::Int(secs)])?;
        }
    }
    s.commit()?;
    let load_s = t0.elapsed().as_secs_f64();
    db.analyze()?;
    // The routing preset must validate against this schema — the model IS the
    // declaration of what this database is for.
    db.set_model(include_str!("../../../models/routing.toml"))?;

    println!("- mpedb: {n} stops + {}-cell matrix loaded in {load_s:.2} s; model `routing` set", n * n);

    // The query an application runs around a solve, measured.
    let near = db.prepare("SELECT dst, secs FROM matrix WHERE src = $1 AND dst <> $1 ORDER BY secs LIMIT 5")?;
    let t = Instant::now();
    let mut probes = 0usize;
    for i in 0..n.min(50) {
        if let ExecResult::Rows { rows, .. } = db.execute(&near, &[Value::Int(i as i64)])? {
            probes += rows.len();
        }
    }
    println!(
        "- nearest-5 stops via SQL: {:.2} ms for {} probes ({} rows)\n",
        t.elapsed().as_secs_f64() * 1000.0,
        n.min(50),
        probes
    );

    // Submatrix reader: the exact arm reads its costs OUT OF the database.
    let row_q = db.prepare("SELECT dst, secs FROM matrix WHERE src = $1")?;
    let read_sub = |nodes: &[usize]| -> Result<(Vec<i64>, f64), Box<dyn std::error::Error>> {
        let k = nodes.len();
        let t = Instant::now();
        let mut sub = vec![0i64; k * k];
        for (a, &ni) in nodes.iter().enumerate() {
            let full = match db.execute(&row_q, &[Value::Int(ni as i64)])? {
                ExecResult::Rows { rows, .. } => rows,
                other => return Err(format!("expected rows, got {other:?}").into()),
            };
            // dst-indexed lookup for this row.
            let mut by_dst = vec![0i64; n];
            for r in &full {
                if let (Value::Int(d), Value::Int(secs)) = (&r[0], &r[1]) {
                    by_dst[*d as usize] = *secs;
                }
            }
            for (b, &nj) in nodes.iter().enumerate() {
                sub[a * k + b] = by_dst[nj];
            }
        }
        Ok((sub, t.elapsed().as_secs_f64() * 1000.0))
    };

    // ---- the sweep --------------------------------------------------------
    println!("## Closed tours from the depot (vehicle end = start, brooom's default)\n");
    println!("| N | exact cost | exact total (read+solve) | brooom cost | brooom wall | gap | agree |");
    println!("|---:|---:|---:|---:|---:|---:|---|");

    let mut rng = Rng(0x5EED_2026);
    // k STOPS plus the depot: total nodes = k+1 must stay within the cap.
    for k in [8usize, 10, 12, 14, 16, 17] {
        if k >= n {
            break;
        }
        // k stops sampled without replacement from 1..n, plus depot 0.
        let mut pool: Vec<usize> = (1..n).collect();
        let mut nodes = vec![0usize];
        for _ in 0..k {
            let i = (rng.next() % pool.len() as u64) as usize;
            nodes.push(pool.swap_remove(i));
        }

        let (sub, read_ms) = read_sub(&nodes)?;
        let kk = nodes.len();
        let cost = |i: u16, j: u16| sub[i as usize * kk + j as usize];

        let t = Instant::now();
        let Some(exact) = sequence::solve_sequence(kk as u16, &cost, true) else {
            println!("| {kk} | declines (cap {}) | — | — | — | — | — |", sequence::MAX_SEQUENCE_N);
            continue;
        };
        let solve_ms = t.elapsed().as_secs_f64() * 1000.0;

        // brooom on the SAME sub-instance, explicit end = start.
        let (bcost, bwall_ms, broute) = run_brooom(&brooom, &dir, &nodes, &sub)?;
        // Agreement: recompute brooom's route cost from OUR matrix.
        let recomputed = route_cost(&broute, &nodes, &sub);
        let agree = recomputed == bcost;
        let gap = (bcost - exact.cost) as f64 / exact.cost as f64 * 100.0;
        println!(
            "| {kk} | {} | {:.1} ms | {} | {:.0} ms | {} | {} |",
            exact.cost,
            read_ms + solve_ms,
            bcost,
            bwall_ms,
            if agree { format!("{gap:+.2}%") } else { "—".into() },
            if agree { "yes" } else { "**NO**" }
        );
    }

    // ---- the full instance: the heuristic's regime ------------------------
    println!();
    println!("## The full instance ({n} locations) — past the exact cap\n");
    let nodes: Vec<usize> = (0..n).collect();
    let (sub, read_ms) = read_sub(&nodes)?;
    match sequence::solve_sequence(n as u16, &|i, j| sub[i as usize * n + j as usize], true) {
        Some(_) => println!("(unexpected: exact accepted n={n})"),
        None => println!(
            "- exact: **declines** (cap {} — beyond it the answer would stop being exact)",
            sequence::MAX_SEQUENCE_N
        ),
    }
    let (bcost, bwall_ms, broute) = run_brooom(&brooom, &dir, &nodes, &sub)?;
    let recomputed = route_cost(&broute, &nodes, &sub);
    println!(
        "- brooom: cost {bcost} in {:.1} s (route recomputes to {recomputed} on our matrix: {}); \
         matrix read from mpedb in {read_ms:.0} ms",
        bwall_ms / 1000.0,
        if recomputed == bcost { "agree" } else { "**DISAGREE**" }
    );
    Ok(())
}

/// One brooom subprocess run over a sub-instance. Returns (claimed cost,
/// wall ms, visited location-index order including depot start).
fn run_brooom(
    brooom: &str,
    dir: &std::path::Path,
    nodes: &[usize],
    sub: &[i64],
) -> Result<(i64, f64, Vec<usize>), Box<dyn std::error::Error>> {
    let k = nodes.len();
    let mut body = String::from("{\"vehicles\":[{\"id\":0,\"start_index\":0,\"end_index\":0}],\"jobs\":[");
    for i in 1..k {
        if i > 1 {
            body.push(',');
        }
        let _ = write!(body, "{{\"id\":{i},\"location_index\":{i}}}");
    }
    body.push_str("],\"matrices\":{\"car\":{\"durations\":[");
    for i in 0..k {
        if i > 0 {
            body.push(',');
        }
        body.push('[');
        for j in 0..k {
            if j > 0 {
                body.push(',');
            }
            let _ = write!(body, "{}", sub[i * k + j]);
        }
        body.push(']');
    }
    body.push_str("]}}}");
    let pfile = dir.join("problem.json");
    std::fs::write(&pfile, &body)?;

    let t = Instant::now();
    let out = std::process::Command::new(brooom)
        .arg("-i")
        .arg(&pfile)
        .output()?;
    let wall_ms = t.elapsed().as_secs_f64() * 1000.0;
    if !out.status.success() {
        return Err(format!(
            "brooom failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("?")
        )
        .into());
    }
    let sol = json::parse(&String::from_utf8_lossy(&out.stdout))?;
    let cost = sol
        .get("summary")
        .and_then(|s| s.get("cost"))
        .and_then(|v| v.num())
        .ok_or("brooom output has no summary.cost")? as i64;
    let mut route = Vec::new();
    if let Some(steps) = sol
        .get("routes")
        .and_then(|r| r.arr().first())
        .and_then(|r0| r0.get("steps"))
    {
        for st in steps.arr() {
            if let Some(li) = st.get("location_index").and_then(|v| v.num()) {
                route.push(li as usize);
            }
        }
    }
    Ok((cost, wall_ms, route))
}

/// A route's cost on OUR submatrix. brooom's steps list includes start and
/// end (both the depot); consecutive-leg sum is the whole tour.
fn route_cost(route: &[usize], nodes: &[usize], sub: &[i64]) -> i64 {
    let k = nodes.len();
    route.windows(2).map(|w| sub[w[0] * k + w[1]]).sum()
}
