//! mpedb (SQL over edge tables) vs Neo4j (Cypher) on graph workloads.
//!
//! The harness rules are olapbench's, unchanged: every workload runs on both
//! engines, results are rendered canonically and compared BEFORE any timing is
//! believed, and a disagreement strikes the row. What is deliberately
//! different here: the two engines do not even share a data model — mpedb
//! stores an edge TABLE and answers in joins and recursive CTEs, Neo4j stores
//! a property graph and answers in Cypher patterns — so this benchmark is
//! honest only about QUESTIONS both can answer exactly. Unbounded
//! variable-length traversal is excluded: Cypher's `[*]` walks trails (no
//! repeated relationship per path), which is a different object than a
//! recursive CTE's reachable set, and timing two different questions teaches
//! nothing.
//!
//! Neo4j is measured over its HTTP transactional endpoint on localhost —
//! that is a real client's path, but it IS a protocol tax mpedb (in-process)
//! does not pay. The report says so next to the numbers.

mod neo4j;

use std::fmt::Write as _;
use std::time::Instant;

use mpedb::{Config, Database, ExecResult, Value};
use neo4j::Neo4j;

const NODES: i64 = 50_000;
const EDGES: i64 = 250_000;
/// Node 0 is the hub: sources are drawn from a squared distribution, so the
/// low ids have most of the out-edges — the shape a real follower graph has.
const HOT: i64 = 0;

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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Deterministic edge list — identical for both engines, self-loops excluded.
fn edges() -> Vec<(i64, i64)> {
    let mut rng = Rng(0x9E37_79B9 | 1);
    let mut out = Vec::with_capacity(EDGES as usize);
    while out.len() < EDGES as usize {
        let r = rng.below(NODES as u64);
        let src = ((r * r) / NODES as u64) as i64;
        let dst = rng.below(NODES as u64) as i64;
        if src != dst {
            out.push((src, dst));
        }
    }
    out
}

struct Workload {
    name: &'static str,
    about: &'static str,
    sql: String,
    cypher: String,
}

fn workloads() -> Vec<Workload> {
    // Parameters are inlined into the SQL text: a recursive CTE component may
    // not carry parameters (a documented refusal), and one rule for all six
    // beats two mechanisms.
    let h = HOT;
    vec![
        Workload {
            name: "degree",
            about: "out-degree of the hub — one index range count",
            sql: format!("SELECT count(*) FROM edge WHERE src = {h}"),
            cypher: format!("MATCH (:N {{id: {h}}})-[:E]->() RETURN count(*)"),
        },
        Workload {
            name: "hop2",
            about: "distinct nodes exactly 2 hops out — one self-join",
            sql: format!(
                "SELECT count(DISTINCT e2.dst) FROM edge e1, edge e2 \
                 WHERE e1.src = {h} AND e2.src = e1.dst"
            ),
            cypher: format!(
                "MATCH (:N {{id: {h}}})-[:E]->()-[:E]->(b) RETURN count(DISTINCT b.id)"
            ),
        },
        Workload {
            name: "hop3",
            about: "distinct nodes exactly 3 hops out — two self-joins",
            sql: format!(
                "SELECT count(DISTINCT e3.dst) FROM edge e1, edge e2, edge e3 \
                 WHERE e1.src = {h} AND e2.src = e1.dst AND e3.src = e2.dst"
            ),
            cypher: format!(
                "MATCH (:N {{id: {h}}})-[:E]->()-[:E]->()-[:E]->(b) RETURN count(DISTINCT b.id)"
            ),
        },
        Workload {
            name: "reach4",
            about: "distinct nodes within 4 hops (start included) — the \
                    depth-guarded recursive CTE against Cypher [*0..4]",
            sql: format!(
                "WITH RECURSIVE r(node, d) AS (\
                   SELECT {h}, 0 \
                   UNION \
                   SELECT e.dst, r.d + 1 FROM r JOIN edge e ON e.src = r.node WHERE r.d < 4\
                 ) SELECT count(DISTINCT node) FROM r"
            ),
            cypher: format!(
                "MATCH (:N {{id: {h}}})-[:E*0..4]->(b) RETURN count(DISTINCT b.id)"
            ),
        },
        Workload {
            name: "tri-hub",
            about: "directed triangles through the hub — a 3-cycle join anchored one end",
            sql: format!(
                "SELECT count(*) FROM edge a, edge b, edge c \
                 WHERE a.src = {h} AND b.src = a.dst AND c.src = b.dst AND c.dst = {h}"
            ),
            cypher: format!(
                "MATCH (a:N {{id: {h}}})-[:E]->()-[:E]->()-[:E]->(a) RETURN count(*)"
            ),
        },
        Workload {
            name: "tri-global",
            about: "every directed 3-cycle in the graph (each counted 3×, both engines)",
            sql: "SELECT count(*) FROM edge a, edge b, edge c \
                  WHERE b.src = a.dst AND c.src = b.dst AND c.dst = a.src"
                .to_string(),
            cypher: "MATCH (x)-[:E]->(y)-[:E]->(z)-[:E]->(x) RETURN count(*)".to_string(),
        },
    ]
}

// ---------------------------------------------------------------------------
// mpedb side
// ---------------------------------------------------------------------------

fn mpedb_load(dir: &std::path::Path) -> Result<(Database, f64), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("graph.mpedb");
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 192
max_readers = 16
durability = "none"

[[table]]
name = "edge"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "src"
  type = "int64"
  nullable = false
  indexed = true
  [[table.column]]
  name = "dst"
  type = "int64"
  nullable = false
  indexed = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml)?)?;
    let ins = db.prepare("INSERT INTO edge (id, src, dst) VALUES ($1, $2, $3)")?;
    let t0 = Instant::now();
    let all = edges();
    let mut id = 0i64;
    for chunk in all.chunks(50_000) {
        let mut s = db.begin()?;
        for (src, dst) in chunk {
            s.execute(&ins, &[Value::Int(id), Value::Int(*src), Value::Int(*dst)])?;
            id += 1;
        }
        s.commit()?;
    }
    let load = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    db.analyze()?;
    eprintln!("  mpedb analyze in {:.2} s", t1.elapsed().as_secs_f64());
    Ok((db, load))
}

fn mpedb_rows(db: &Database, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
    match db.query(sql, &[])? {
        ExecResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Null => "NULL".to_string(),
                            Value::Int(i) => i.to_string(),
                            Value::Float(f) => format!("{f:.2}"),
                            Value::Text(t) => t.to_string(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            out.sort();
            Ok(out.join("\n"))
        }
        other => Ok(format!("{other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Neo4j side
// ---------------------------------------------------------------------------

fn neo4j_load(n: &Neo4j) -> Result<f64, Box<dyn std::error::Error>> {
    // Wipe, index, await — setup, untimed.
    n.call("MATCH (x) DETACH DELETE x", "{}")?;
    n.call("CREATE INDEX node_id IF NOT EXISTS FOR (x:N) ON (x.id)", "{}")?;
    n.call("CALL db.awaitIndexes()", "{}")?;

    let t0 = Instant::now();
    for lo in (0..NODES).step_by(10_000) {
        let hi = (lo + 10_000).min(NODES);
        n.call(
            "UNWIND range($lo, $hi - 1) AS i CREATE (:N {id: i})",
            &format!("{{\"lo\": {lo}, \"hi\": {hi}}}"),
        )?;
    }
    let all = edges();
    for chunk in all.chunks(10_000) {
        let mut rows = String::from("[");
        for (i, (s, d)) in chunk.iter().enumerate() {
            if i > 0 {
                rows.push(',');
            }
            let _ = write!(rows, "[{s},{d}]");
        }
        rows.push(']');
        n.call(
            "UNWIND $rows AS r MATCH (a:N {id: r[0]}) MATCH (b:N {id: r[1]}) CREATE (a)-[:E]->(b)",
            &format!("{{\"rows\": {rows}}}"),
        )?;
    }
    Ok(t0.elapsed().as_secs_f64())
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Cell {
    cold_ms: f64,
    warm_ms: f64,
    answer: String,
}

fn run_side(
    reps: usize,
    mut f: impl FnMut() -> Result<String, Box<dyn std::error::Error>>,
) -> Result<Cell, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let answer = f()?;
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut ms = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        let got = f()?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if got != answer {
            return Err("two runs of the same query disagreed".into());
        }
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(Cell { cold_ms, warm_ms: ms[ms.len() / 2], answer })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut url = "127.0.0.1:7474".to_string();
    let mut user = "neo4j".to_string();
    let mut pass = "benchpass".to_string();
    let mut reps = 5usize;
    let mut dir = std::env::temp_dir().join("mpedb-graphbench");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--neo4j" => url = args.next().ok_or("--neo4j needs host:port")?,
            "--user" => user = args.next().ok_or("--user needs a value")?,
            "--pass" => pass = args.next().ok_or("--pass needs a value")?,
            "--reps" => reps = args.next().ok_or("--reps needs a value")?.parse()?,
            "--dir" => dir = args.next().ok_or("--dir needs a value")?.into(),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    let neo = Neo4j::new(&url, &user, &pass);
    neo.call("RETURN 1", "{}")
        .map_err(|e| format!("Neo4j not reachable at {url}: {e}"))?;

    println!("# Graph head-to-head: mpedb vs Neo4j\n");
    println!("- graph: {NODES} nodes, {EDGES} directed edges, squared-skew sources (node {HOT} is the hub)");
    println!("- repetitions: {reps} warm (median) after 1 cold run");
    println!();

    eprintln!("loading mpedb…");
    let (db, mpedb_load_s) = mpedb_load(&dir)?;
    eprintln!("loading neo4j…");
    let neo4j_load_s = neo4j_load(&neo)?;
    eprintln!("running…");

    println!("## Load\n");
    println!("| engine | load | note |");
    println!("|---|---:|---|");
    println!("| mpedb | {mpedb_load_s:.1} s | in-process, edge table, src+dst indexes, NDV analyzed |");
    println!("| neo4j | {neo4j_load_s:.1} s | HTTP tx endpoint, UNWIND batches of 10k, id index |");
    println!();

    println!("## Workloads\n");
    println!("Milliseconds; cold = first run, warm = median of {reps}.\n");
    println!("| workload | mpedb cold | mpedb warm | neo4j cold | neo4j warm | warm ratio | agree |");
    println!("|---|---:|---:|---:|---:|---:|---|");

    let mut notes = Vec::new();
    for w in workloads() {
        let m = run_side(reps, || mpedb_rows(&db, &w.sql));
        let n = run_side(reps, || neo.rows(&w.cypher, "{}").map_err(|e| e.into()));

        let (magree, answer) = match (&m, &n) {
            (Ok(a), Ok(b)) => (a.answer == b.answer, Some((a.answer.clone(), b.answer.clone()))),
            _ => (false, None),
        };
        let fmt = |r: &Result<Cell, _>, f: fn(&Cell) -> f64| match r {
            Ok(c) => format!("{:.1}", f(c)),
            Err(_) => "refused".into(),
        };
        let ratio = match (&m, &n) {
            (Ok(a), Ok(b)) if magree && a.warm_ms > 0.0 => {
                let r = b.warm_ms / a.warm_ms;
                if r >= 1.0 {
                    format!("**mpedb {r:.1}× faster**")
                } else {
                    format!("neo4j {:.1}× faster", 1.0 / r)
                }
            }
            _ => "—".into(),
        };
        println!(
            "| `{}` | {} | {} | {} | {} | {} | {} |",
            w.name,
            fmt(&m, |c| c.cold_ms),
            fmt(&m, |c| c.warm_ms),
            fmt(&n, |c| c.cold_ms),
            fmt(&n, |c| c.warm_ms),
            ratio,
            if magree { "yes" } else { "**NO**" }
        );
        for (side, r) in [("mpedb", &m), ("neo4j", &n)] {
            if let Err(e) = r {
                notes.push(format!("- `{}` on {side}: {e}", w.name));
            }
        }
        if let Some((a, b)) = answer {
            if a != b {
                notes.push(format!(
                    "- `{}` DISAGREES: mpedb `{}` vs neo4j `{}` — timings above are void.",
                    w.name,
                    a.lines().next().unwrap_or(""),
                    b.lines().next().unwrap_or("")
                ));
            }
        }
    }
    println!();
    if !notes.is_empty() {
        println!("## Notes\n");
        for n in &notes {
            println!("{n}");
        }
        println!();
    }
    println!("## What each workload is\n");
    for w in workloads() {
        println!("- **`{}`** — {}", w.name, w.about);
        println!("  - SQL: `{}`", w.sql);
        println!("  - Cypher: `{}`", w.cypher);
    }
    Ok(())
}
