//! Exact kNN in mpedb vs approximate kNN in Qdrant — stage D of
//! design/DESIGN-MPEE-GENERAL.md.
//!
//! The comparison is exact-vs-approximate ON PURPOSE, and the report keeps
//! that visible: mpedb's brute-force answer is the ground truth Qdrant's
//! recall@k is computed against, and its latency is the price of exactness.
//! A single "X× faster" headline would be a category error — HNSW answers a
//! different question (probably-nearest) than a scan (nearest). The honest
//! frame is the pair (latency, recall), side by side.
//!
//! mpedb runs the SAME query twice, in two shapes: the exact-kNN heap path
//! with per-dimension early abandonment, and the generic materialize-sort
//! path (forced by a second sort key) — so the abandonment's worth is an A/B
//! inside the report, not a claim.

mod qdrant;

use std::fmt::Write as _;
use std::time::Instant;

use mpedb::{Config, Database, ExecResult, PlanHash, Value};
use qdrant::Qdrant;

const N: usize = 100_000;
const DIMS: usize = 128;
const CENTROIDS: usize = 64;
const QUERIES: usize = 100;
const K: usize = 10;
const CATS: usize = 8;

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
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() % (1 << 24)) as f32 / (1 << 24) as f32
    }
}

/// Clustered data — the shape HNSW is built for. Uniform noise would be an
/// adversarial dataset for the approximate side, and benchmarking an engine
/// on data its authors tell you it is not for is manufacturing a loss (the
/// same rule the OLAP bench applied to DuckDB's indexes).
fn dataset() -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let mut rng = Rng(0x7EC5_EED1);
    let centroids: Vec<Vec<f32>> = (0..CENTROIDS)
        .map(|_| (0..DIMS).map(|_| rng.unit() * 10.0).collect())
        .collect();
    let point = |rng: &mut Rng| {
        let c = &centroids[(rng.next() % CENTROIDS as u64) as usize];
        c.iter().map(|v| v + rng.unit() - 0.5).collect::<Vec<f32>>()
    };
    let vectors: Vec<Vec<f32>> = (0..N).map(|_| point(&mut rng)).collect();
    let queries: Vec<Vec<f32>> = (0..QUERIES).map(|_| point(&mut rng)).collect();
    (vectors, queries)
}

fn blob(fs: &[f32]) -> Value {
    Value::Blob(fs.iter().flat_map(|f| f.to_le_bytes()).collect())
}

fn cat_of(id: usize) -> usize {
    id % CATS
}

// ---------------------------------------------------------------------------

fn mpedb_load(
    dir: &std::path::Path,
    vectors: &[Vec<f32>],
) -> Result<(Database, f64), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("vec.mpedb");
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 512
max_readers = 16
durability = "none"

[[table]]
name = "v"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "cat"
  type = "text"
  nullable = false
  indexed = true
  [[table.column]]
  name = "emb"
  type = "blob"
  nullable = false
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml)?)?;
    let ins = db.prepare("INSERT INTO v (id, cat, emb) VALUES ($1, $2, $3)")?;
    let t0 = Instant::now();
    for chunk in vectors.chunks(10_000).enumerate() {
        let (ci, chunk) = chunk;
        let mut s = db.begin()?;
        for (i, v) in chunk.iter().enumerate() {
            let id = ci * 10_000 + i;
            s.execute(
                &ins,
                &[
                    Value::Int(id as i64),
                    Value::Text(format!("c{}", cat_of(id))),
                    blob(v),
                ],
            )?;
        }
        s.commit()?;
    }
    let load = t0.elapsed().as_secs_f64();
    db.analyze()?;
    Ok((db, load))
}

fn mpedb_ids(db: &Database, h: &PlanHash, q: &[f32]) -> Result<Vec<i64>, Box<dyn std::error::Error>> {
    match db.execute(h, &[blob(q)])? {
        ExecResult::Rows { rows, .. } => Ok(rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(i) => *i,
                other => panic!("expected id, got {other:?}"),
            })
            .collect()),
        other => Err(format!("expected rows, got {other:?}").into()),
    }
}

fn qdrant_load(qd: &Qdrant, vectors: &[Vec<f32>]) -> Result<f64, Box<dyn std::error::Error>> {
    let _ = qd.call("DELETE", "/collections/bench", "");
    qd.call(
        "PUT",
        "/collections/bench",
        &format!("{{\"vectors\":{{\"size\":{DIMS},\"distance\":\"Euclid\"}}}}"),
    )?;
    let t0 = Instant::now();
    for (ci, chunk) in vectors.chunks(1_000).enumerate() {
        let mut body = String::from("{\"points\":[");
        for (i, v) in chunk.iter().enumerate() {
            let id = ci * 1_000 + i;
            if i > 0 {
                body.push(',');
            }
            let _ = write!(body, "{{\"id\":{id},\"vector\":[");
            for (j, x) in v.iter().enumerate() {
                if j > 0 {
                    body.push(',');
                }
                let _ = write!(body, "{x}");
            }
            let _ = write!(body, "],\"payload\":{{\"cat\":\"c{}\"}}}}", cat_of(id));
        }
        body.push_str("]}");
        qd.call("PUT", "/collections/bench/points?wait=true", &body)?;
    }
    Ok(t0.elapsed().as_secs_f64())
}

fn vec_json(q: &[f32]) -> String {
    let mut s = String::from("[");
    for (j, x) in q.iter().enumerate() {
        if j > 0 {
            s.push(',');
        }
        let _ = write!(s, "{x}");
    }
    s.push(']');
    s
}

// ---------------------------------------------------------------------------

struct Lat {
    median_ms: f64,
    p99_ms: f64,
}

fn lat(mut ms: Vec<f64>) -> Lat {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Lat {
        median_ms: ms[ms.len() / 2],
        p99_ms: ms[(ms.len() * 99 / 100).min(ms.len() - 1)],
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut url = "127.0.0.1:6333".to_string();
    let mut dir = std::env::temp_dir().join("mpedb-vecbench");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--qdrant" => url = args.next().ok_or("--qdrant needs host:port")?,
            "--dir" => dir = args.next().ok_or("--dir needs a value")?.into(),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let qd = Qdrant::new(&url);
    qd.call("GET", "/collections", "")
        .map_err(|e| format!("Qdrant not reachable at {url}: {e}"))?;

    println!("# Vector head-to-head: mpedb exact vs Qdrant HNSW\n");
    println!("- {N} vectors × {DIMS} dims (f32), {CENTROIDS} clusters, {QUERIES} queries, k = {K}");
    println!("- mpedb answers EXACT kNN (its result is the ground truth); Qdrant answers");
    println!("  approximate kNN (default HNSW) and is scored by recall@{K} against it");
    println!();

    eprintln!("generating…");
    let (vectors, queries) = dataset();
    eprintln!("loading mpedb…");
    let (db, mpedb_load_s) = mpedb_load(&dir, &vectors)?;
    eprintln!("loading qdrant…");
    let qdrant_load_s = qdrant_load(&qd, &vectors)?;

    // Prepared once; executed per query with a fresh parameter — the
    // content-hashed hot path.
    let knn = db.prepare(&format!("SELECT id FROM v ORDER BY vec_l2(emb, $1) LIMIT {K}"))?;
    // The same question in a shape the heap path declines (second sort key):
    // the generic full-compute-and-sort arm of the A/B.
    let knn_generic =
        db.prepare(&format!("SELECT id FROM v ORDER BY vec_l2(emb, $1), id LIMIT {K}"))?;
    let knn_filtered = db.prepare(&format!(
        "SELECT id FROM v WHERE cat = 'c3' ORDER BY vec_l2(emb, $1) LIMIT {K}"
    ))?;

    println!("## Load\n");
    println!("| engine | load | note |");
    println!("|---|---:|---|");
    println!("| mpedb | {mpedb_load_s:.1} s | blob rows, cat index, NDV analyzed |");
    println!("| qdrant | {qdrant_load_s:.1} s | REST upsert (wait=true), HNSW built incrementally |");
    println!();

    eprintln!("running…");

    // --- exact, heap + abandonment ---
    let mut exact: Vec<Vec<i64>> = Vec::with_capacity(QUERIES);
    let mut ms = Vec::with_capacity(QUERIES);
    for q in &queries {
        let t = Instant::now();
        exact.push(mpedb_ids(&db, &knn, q)?);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let l_fast = lat(ms);

    // --- exact, generic sort (the abandonment A/B) ---
    let mut ms = Vec::with_capacity(QUERIES);
    for (i, q) in queries.iter().enumerate() {
        let t = Instant::now();
        let ids = mpedb_ids(&db, &knn_generic, q)?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        // The two mpedb paths must agree exactly (modulo distance ties, which
        // the second sort key breaks differently only WITHIN a tie).
        if ids != exact[i] {
            let same: usize = ids.iter().filter(|x| exact[i].contains(x)).count();
            if same != K {
                return Err(format!("mpedb paths disagree on query {i}").into());
            }
        }
    }
    let l_generic = lat(ms);

    // --- qdrant, approximate ---
    let mut ms = Vec::with_capacity(QUERIES);
    let mut hits = 0usize;
    for (i, q) in queries.iter().enumerate() {
        let body = format!("{{\"vector\":{},\"limit\":{K}}}", vec_json(q));
        let t = Instant::now();
        let ids = qd.search("bench", &body)?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        hits += ids.iter().filter(|x| exact[i].contains(x)).count();
    }
    let l_qd = lat(ms);
    let recall = hits as f64 / (QUERIES * K) as f64;

    // --- filtered: exact vs qdrant payload filter ---
    let mut f_exact: Vec<Vec<i64>> = Vec::with_capacity(QUERIES);
    let mut ms = Vec::with_capacity(QUERIES);
    for q in &queries {
        let t = Instant::now();
        f_exact.push(mpedb_ids(&db, &knn_filtered, q)?);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let l_ffast = lat(ms);
    let mut ms = Vec::with_capacity(QUERIES);
    let mut fhits = 0usize;
    for (i, q) in queries.iter().enumerate() {
        let body = format!(
            "{{\"vector\":{},\"limit\":{K},\"filter\":{{\"must\":[{{\"key\":\"cat\",\"match\":{{\"value\":\"c3\"}}}}]}}}}",
            vec_json(q)
        );
        let t = Instant::now();
        let ids = qd.search("bench", &body)?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        fhits += ids.iter().filter(|x| f_exact[i].contains(x)).count();
    }
    let l_fqd = lat(ms);
    let frecall = fhits as f64 / (QUERIES * K) as f64;

    println!("## Results\n");
    println!("Per-query latency over {QUERIES} queries.\n");
    println!("| side | median | p99 | recall@{K} |");
    println!("|---|---:|---:|---:|");
    println!(
        "| mpedb exact, heap + early abandonment | {:.1} ms | {:.1} ms | 1.000 (ground truth) |",
        l_fast.median_ms, l_fast.p99_ms
    );
    println!(
        "| mpedb exact, generic sort (A/B arm) | {:.1} ms | {:.1} ms | 1.000 |",
        l_generic.median_ms, l_generic.p99_ms
    );
    println!(
        "| qdrant HNSW (default params) | {:.1} ms | {:.1} ms | {recall:.3} |",
        l_qd.median_ms, l_qd.p99_ms
    );
    println!();
    println!("Filtered (`cat = 'c3'`, 1/{CATS} of the data):\n");
    println!("| side | median | p99 | recall@{K} |");
    println!("|---|---:|---:|---:|");
    println!(
        "| mpedb exact, filter before heap | {:.1} ms | {:.1} ms | 1.000 (ground truth) |",
        l_ffast.median_ms, l_ffast.p99_ms
    );
    println!(
        "| qdrant HNSW, payload filter | {:.1} ms | {:.1} ms | {frecall:.3} |",
        l_fqd.median_ms, l_fqd.p99_ms
    );
    println!();
    println!(
        "Early abandonment bought {:.1}× on the unfiltered scan ({:.1} → {:.1} ms median), \
         exactness untouched — the same monotone-lower-bound argument as the solver's \
         UNBOUGHT pricing, applied per dimension.",
        l_generic.median_ms / l_fast.median_ms,
        l_generic.median_ms,
        l_fast.median_ms
    );
    Ok(())
}
