//! Task #117 — is the plan **footprint** worth building an index on?
//!
//! The footprint (`tables_read`/`tables_written` as sparse sorted `TableSet`,
//! `indexes_used` bitmap, `key_access`) is "the matrix of what a statement
//! touches", computed at plan time for free. It cannot be the plan identity
//! (content addressing is load-bearing and footprints are not unique), but it
//! *could* be an index over plans with the hash as the final discriminator.
//! This measures the three candidate sites before anyone builds on it:
//!
//! 1. **Conflict detection at scale.** Pairwise `Footprint::conflicts_with`
//!    across a commit window of N concurrent writers is O(N) per commit /
//!    O(N²) per window. An inverted `table_id -> statements` index should make
//!    it O(tables touched). Find the crossover — and, decisively, compare both
//!    against the measured commit critical section (~1.4 µs, DESIGN-PHASE3 §2)
//!    to say whether the crossover is *reachable* inside a real commit window.
//! 2. **Routing.** The footprint is already the shard key. Measure the routing
//!    decision per statement against the cost of the statement itself.
//! 3. **Delta ("polyline") compression of `TableSet`** — the #115 mechanism —
//!    at #88's would-be scale of millions of stored footprints.
//!
//! ```text
//! cargo run --release -p mpedb --example footprint_index [census.tsv]
//! ```
//!
//! With no argument the shapes are synthetic (a sweep). With a `census.tsv`
//! produced by `sqlite_corpus --footprint-census=<path>` the REAL corpus
//! footprint distribution is replayed, so the verdict is not a guess about
//! what statements look like.

use mpedb::{Footprint, KeyAccess, TableSet};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

// ------------------------------------------------------------------ rng
/// The house xorshift (no rand dep, deterministic).
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
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }
}

fn fp(read: &[u32], write: &[u32]) -> Footprint {
    Footprint {
        tables_read: read.iter().copied().collect(),
        tables_written: write.iter().copied().collect(),
        indexes_used: 1,
        key_access: KeyAccess::Full,
        read_only: write.is_empty(),
    }
}

// ============================================== 1. conflict detection at scale

/// The inverted index the task proposes: `table_id -> the in-flight statements
/// that read it / write it`. A commit tests its own footprint against the
/// index instead of against every peer.
///
/// Two realizations, because the choice of map decides the whole verdict at
/// small N: a `HashMap` (unbounded ids) and a flat `Vec` slot per table id
/// (`MAX_TABLES` is 4096, so a direct-indexed table is 32 KiB of pointers and
/// is the form anyone would actually build). Both are REUSED across windows —
/// an in-flight index is cleared and refilled, never reallocated, and charging
/// a cold `HashMap` allocation to it would rig the comparison.
#[derive(Default)]
struct InvertedHash {
    readers: HashMap<u32, Vec<u32>>,
    writers: HashMap<u32, Vec<u32>>,
}

impl InvertedHash {
    fn refill(&mut self, w: &[Footprint]) {
        for v in self.readers.values_mut() {
            v.clear();
        }
        for v in self.writers.values_mut() {
            v.clear();
        }
        for (i, f) in w.iter().enumerate() {
            for t in f.tables_read.iter() {
                self.readers.entry(t).or_default().push(i as u32);
            }
            for t in f.tables_written.iter() {
                self.writers.entry(t).or_default().push(i as u32);
            }
        }
    }

    /// Does `f` conflict with anything in the window other than `me`? Exactly
    /// `Footprint::conflicts_with`'s rule, table-granular: my writes against
    /// their reads and writes, my reads against their writes.
    fn conflicts(&self, me: u32, f: &Footprint) -> bool {
        for t in f.tables_written.iter() {
            for v in [self.readers.get(&t), self.writers.get(&t)].into_iter().flatten() {
                {
                    if v.iter().any(|&o| o != me) {
                        return true;
                    }
                }
            }
        }
        for t in f.tables_read.iter() {
            if let Some(v) = self.writers.get(&t) {
                if v.iter().any(|&o| o != me) {
                    return true;
                }
            }
        }
        false
    }
}

struct InvertedVec {
    readers: Vec<Vec<u32>>,
    writers: Vec<Vec<u32>>,
}

impl InvertedVec {
    fn new(tables: u32) -> InvertedVec {
        InvertedVec {
            readers: vec![Vec::new(); tables as usize],
            writers: vec![Vec::new(); tables as usize],
        }
    }
    fn refill(&mut self, w: &[Footprint]) {
        for v in self.readers.iter_mut().chain(self.writers.iter_mut()) {
            v.clear();
        }
        for (i, f) in w.iter().enumerate() {
            for t in f.tables_read.iter() {
                self.readers[t as usize].push(i as u32);
            }
            for t in f.tables_written.iter() {
                self.writers[t as usize].push(i as u32);
            }
        }
    }
    fn conflicts(&self, me: u32, f: &Footprint) -> bool {
        for t in f.tables_written.iter() {
            for v in [&self.readers[t as usize], &self.writers[t as usize]] {
                if v.iter().any(|&o| o != me) {
                    return true;
                }
            }
        }
        for t in f.tables_read.iter() {
            if self.writers[t as usize].iter().any(|&o| o != me) {
                return true;
            }
        }
        false
    }
}

/// What the engine ACTUALLY runs today (`shm::opt_conflict`, the committed-
/// footprint ring, DESIGN-PHASE3 §3.1): a scan of the exact txn ids committed
/// since our snapshot, each a u64 table bitmap AND plus a key-hash compare.
/// It is O(window), not O(peers), and it is already an index over TIME rather
/// than over tables — which is the baseline any proposal has to beat.
fn ring_scan(ring: &[(u64, u64, u64)], snap: usize, cur: usize, my_bit: u64, key: u64) -> bool {
    for t in snap + 1..=cur {
        let (txn, tbits, khash) = ring[t % ring.len()];
        if txn != t as u64 {
            return true;
        }
        if tbits & my_bit != 0 && khash == key {
            return true;
        }
    }
    false
}

/// Build a window of `n` write footprints over `tables` tables, each touching
/// `fan` tables. `fan == 1` is the corpus-typical shape.
fn window(n: usize, tables: u32, fan: usize, seed: u64) -> Vec<Footprint> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| {
            let mut w = TableSet::new();
            for _ in 0..fan {
                w.insert(rng.below(tables));
            }
            let ids: Vec<u32> = w.iter().collect();
            fp(&ids, &ids)
        })
        .collect()
}

fn bench_conflicts() {
    println!("\n=== 1. conflict detection: pairwise vs inverted index ===");
    println!(
        "ns per COMMIT (one committer tested against a window of N-1 peers),\n\
         index build charged at steady state (clear+refill, amortized over N).\n\
         'ring' = what the engine runs today: shm::opt_conflict's O(window)\n\
         scan over the committed-footprint ring (DESIGN-PHASE3 §3.1).\n\
         Budget: the serial commit critical section measures ~1.4 us and a\n\
         whole serial txn ~4.4 us (DESIGN-PHASE3 §2)."
    );
    const TABLES: u32 = 4096;
    // A ring in the shape shm.rs uses: 64 slots of (txn, table bitmap, key hash).
    let mut rng = Rng(0xA5A5_1234);
    let ring: Vec<(u64, u64, u64)> = (0..64u64)
        .map(|i| (i, 1u64 << (rng.below(64)), rng.next()))
        .collect();

    for &fan in &[1usize, 3, 5] {
        for &tables in &[8u32, TABLES] {
            println!(
                "\n-- {fan} table(s)/stmt, {tables} tables in schema --\n\
                 {:>6} {:>11} {:>11} {:>11} {:>9} {:>13} {:>13}",
                "N", "pairwise", "inv(hash)", "inv(vec)", "best-up", "pw window/us", "inv window/us"
            );
            for &n in &[2usize, 8, 32, 128, 512, 2048] {
                let w = window(n, tables, fan, 0x9E37_79B9_7F4A_7C15 ^ (n as u64));
                let reps = (4_000_000 / n).clamp(50, 200_000);

                // --- pairwise: the committer against every peer.
                let t0 = Instant::now();
                let mut hits = 0u64;
                for r in 0..reps {
                    let me = r % n;
                    let mut c = false;
                    for (j, o) in w.iter().enumerate() {
                        if j != me {
                            c |= w[me].conflicts_with(o);
                        }
                    }
                    hits += black_box(c) as u64;
                }
                let pw = t0.elapsed().as_nanos() as f64 / reps as f64;
                black_box(hits);

                // --- inverted, both map shapes, refill amortized over N commits.
                let mut ih = InvertedHash::default();
                ih.refill(&w);
                let mut iv = InvertedVec::new(tables);
                iv.refill(&w);
                let windows = (reps / n).max(1);

                let t0 = Instant::now();
                for _ in 0..windows {
                    ih.refill(&w);
                    for (me, fp) in w.iter().enumerate() {
                        black_box(ih.conflicts(me as u32, fp));
                    }
                }
                let inv_h = t0.elapsed().as_nanos() as f64 / (windows * n) as f64;

                let t0 = Instant::now();
                for _ in 0..windows {
                    iv.refill(&w);
                    for (me, fp) in w.iter().enumerate() {
                        black_box(iv.conflicts(me as u32, fp));
                    }
                }
                let inv_v = t0.elapsed().as_nanos() as f64 / (windows * n) as f64;

                let best = inv_h.min(inv_v);
                println!(
                    "{n:>6} {pw:>11.1} {inv_h:>11.1} {inv_v:>11.1} {:>8.2}x {:>13.1} {:>13.1}",
                    pw / best,
                    pw * n as f64 / 1000.0,
                    best * n as f64 / 1000.0,
                );
            }
        }
    }

    // The mechanism that is actually in the tree, for scale.
    println!("\n-- today's mechanism: shm::opt_conflict ring scan --");
    println!("{:>10} {:>14}", "window", "ns/commit");
    for &wlen in &[1usize, 4, 16, 63] {
        let reps = 2_000_000usize;
        let t0 = Instant::now();
        let mut hits = 0u64;
        for r in 0..reps {
            let cur = 64 + (r % 32);
            hits += black_box(ring_scan(&ring, cur - wlen, cur, 1 << (r % 64), r as u64)) as u64;
        }
        black_box(hits);
        println!(
            "{wlen:>10} {:>14.1}",
            t0.elapsed().as_nanos() as f64 / reps as f64
        );
    }
}

// ================================================== 2. routing / shard key

fn bench_routing(shapes: &[Footprint]) {
    println!("\n=== 2. routing: footprint -> shard, computed vs memoized ===");
    // Today (shard.rs): route by walking the plan's stmt/footprint per
    // statement. Indexed: memoize the decision under the plan hash.
    let reps = 200_000usize;
    let k = 8u64;

    let t0 = Instant::now();
    let mut acc = 0u64;
    for i in 0..reps {
        let f = &shapes[i % shapes.len()];
        // The decision a footprint router makes: point access -> one shard,
        // anything else -> fan out. `first()` is the O(1) sparse-set read.
        let route = match &f.key_access {
            KeyAccess::Point(parts) => {
                let mut h = 0xcbf2_9ce4_8422_2325u64;
                for p in parts {
                    h ^= match p {
                        mpedb::KeyPart::Param(i) | mpedb::KeyPart::Const(i) => u64::from(*i),
                        mpedb::KeyPart::OuterCol(i) => u64::from(*i) | 1 << 32,
                    };
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
                (h % k) as i64
            }
            _ => -1,
        };
        acc = acc.wrapping_add(route as u64);
    }
    let computed = t0.elapsed().as_nanos() as f64 / reps as f64;
    black_box(acc);

    let memo: HashMap<u64, i64> = (0..shapes.len() as u64).map(|i| (i, (i % k) as i64)).collect();
    let t0 = Instant::now();
    let mut acc = 0i64;
    for i in 0..reps {
        acc += memo[&((i % shapes.len()) as u64)];
    }
    let memoized = t0.elapsed().as_nanos() as f64 / reps as f64;
    black_box(acc);

    println!("footprint-computed route   {computed:>8.2} ns/stmt");
    println!("memoized (hash -> shard)   {memoized:>8.2} ns/stmt");
    println!(
        "delta {:+.2} ns/stmt — against a ~4.4 us serial txn that is {:.4} %",
        memoized - computed,
        100.0 * (computed - memoized).abs() / 4400.0
    );
}

// ======================================= 3. delta compression of TableSet

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// The #115 mechanism applied to a sorted `TableSet`: varint count, first id
/// as a varint, then varint GAPS. A dense run 5,6,7,8 costs 1 byte/element.
fn delta_len(ts: &TableSet) -> usize {
    let ids = ts.as_slice();
    let mut n = varint_len(ids.len() as u64);
    let mut prev = 0u32;
    for (i, &id) in ids.iter().enumerate() {
        n += varint_len(if i == 0 {
            u64::from(id)
        } else {
            u64::from(id - prev - 1)
        });
        prev = id;
    }
    n
}

fn cur_len(ts: &TableSet) -> usize {
    let mut b = Vec::new();
    ts.encode_into(&mut b);
    b.len()
}

fn bench_compression(shapes: &[Footprint], weights: &[u64]) {
    println!("\n=== 3. delta (polyline) compression of TableSet, #115 mechanism ===");
    let (mut cur, mut del, mut occ) = (0u64, 0u64, 0u64);
    let (mut fcur, mut fdel) = (0u64, 0u64);
    for (f, &w) in shapes.iter().zip(weights) {
        let c = (cur_len(&f.tables_read) + cur_len(&f.tables_written)) as u64;
        let d = (delta_len(&f.tables_read) + delta_len(&f.tables_written)) as u64;
        let mut whole = Vec::new();
        f.encode_into(&mut whole);
        cur += c * w;
        del += d * w;
        fcur += whole.len() as u64 * w;
        fdel += (whole.len() as u64 - c + d) * w;
        occ += w;
    }
    println!("{occ} footprint instances, {} distinct shapes", shapes.len());
    println!(
        "table sets only:  current {cur} B  delta {del} B  ({:+.1} %)",
        100.0 * (del as f64 - cur as f64) / cur.max(1) as f64
    );
    println!(
        "whole footprint:  current {fcur} B  delta {fdel} B  ({:+.1} %)",
        100.0 * (fdel as f64 - fcur as f64) / fcur.max(1) as f64
    );
    println!(
        "mean bytes/footprint: {:.2} -> {:.2}",
        fcur as f64 / occ.max(1) as f64,
        fdel as f64 / occ.max(1) as f64
    );
    // #88's would-be scale: what does the delta form save over M stored rows?
    for &m in &[1_000_000u64, 10_000_000, 100_000_000] {
        let per_cur = fcur as f64 / occ.max(1) as f64;
        let per_del = fdel as f64 / occ.max(1) as f64;
        println!(
            "  at {:>11} stored footprints: {:>8.1} MiB -> {:>8.1} MiB (saves {:.1} MiB)",
            m,
            m as f64 * per_cur / (1 << 20) as f64,
            m as f64 * per_del / (1 << 20) as f64,
            m as f64 * (per_cur - per_del) / (1 << 20) as f64
        );
    }
    // The counterfactual the honest expectation needs: WIDE, dense sets —
    // the only regime where the mechanism can pay.
    println!("\nsynthetic wide/dense sets (the regime where delta could pay):");
    for &(w, dense) in &[(8usize, true), (64, true), (64, false), (512, true)] {
        let mut rng = Rng(0xDEAD_BEEF);
        let ts: TableSet = if dense {
            (100..100 + w as u32).collect()
        } else {
            (0..w).map(|_| rng.below(4096)).collect()
        };
        println!(
            "  |set|={:>4} {:<7} current {:>5} B  delta {:>5} B  ({:+.1} %)",
            ts.len(),
            if dense { "dense" } else { "sparse" },
            cur_len(&ts),
            delta_len(&ts),
            100.0 * (delta_len(&ts) as f64 - cur_len(&ts) as f64) / cur_len(&ts) as f64
        );
    }
}

// ============================ 4. plan-variant families (MPEE ping-pong, #88)

/// `DESIGN-MPEE-SOLVER` §9.6's execution-time ping-pong persists a better plan
/// as a NEW content hash — immutable identity. So one SQL statement grows a
/// FAMILY of variants over time, and two questions follow:
///
/// 1. **"list the variants of X"** — today that is either N probes of a 32-byte
///    hash key in the registry, or a scan. If a variant were addressed as
///    `(base plan hash ‖ decision vector)` the family would be a single PREFIX
///    RANGE, and the best variant could be chosen without decoding any plan.
/// 2. **"pick the best variant"** — a decision vector is a permutation of ≤ 16
///    tables plus a few access-path bits, so it is short, structured and
///    ORDERED; a full blake3 is none of those.
///
/// This measures the lookup half against the honest baseline: N probes of a
/// `HashMap<[u8;32], _>` (what the registry is) vs one prefix range over a
/// `BTreeMap<[u8;40], _>` keyed `base_hash ‖ decision`.
fn bench_variants() {
    use std::collections::BTreeMap;
    println!("\n=== 4. plan-variant families: N hash probes vs one prefix scan ===");
    println!(
        "one SQL statement accumulating V learned variants (MPEE ping-pong).\n\
         probe = V lookups of a 32-byte content hash; prefix = one range over\n\
         (base hash ‖ 8-byte decision vector).\n{:>6} {:>14} {:>14} {:>9}",
        "V", "V probes/ns", "prefix/ns", "speedup"
    );
    const STMTS: usize = 4096;
    for &v in &[1usize, 2, 4, 8, 16, 64] {
        let mut rng = Rng(0xC0FF_EE00 ^ v as u64);
        // Content-addressed registry: every variant is its own 32-byte key.
        let mut flat: HashMap<[u8; 32], u32> = HashMap::new();
        // Structured: base hash (32) ‖ decision vector (8), ordered.
        let mut tree: BTreeMap<[u8; 40], u32> = BTreeMap::new();
        let mut bases = Vec::with_capacity(STMTS);
        for _ in 0..STMTS {
            let mut base = [0u8; 32];
            for c in base.chunks_mut(8) {
                c.copy_from_slice(&rng.next().to_le_bytes());
            }
            bases.push(base);
            for d in 0..v {
                let mut k = [0u8; 32];
                for c in k.chunks_mut(8) {
                    c.copy_from_slice(&rng.next().to_le_bytes());
                }
                flat.insert(k, d as u32);
                let mut t = [0u8; 40];
                t[..32].copy_from_slice(&base);
                t[32..].copy_from_slice(&(d as u64).to_be_bytes());
                tree.insert(t, d as u32);
            }
        }
        // Baseline: the caller must probe every variant key it knows of. It
        // knows them because they are stored alongside the statement, so the
        // fair baseline is V successful probes.
        let keys: Vec<[u8; 32]> = flat.keys().copied().collect();
        let reps = 200_000usize;
        let t0 = Instant::now();
        let mut acc = 0u64;
        for r in 0..reps {
            let start = (r * v) % keys.len().max(1);
            for i in 0..v {
                acc += u64::from(flat[&keys[(start + i) % keys.len()]]);
            }
        }
        let probe = t0.elapsed().as_nanos() as f64 / reps as f64;
        black_box(acc);

        let t0 = Instant::now();
        let mut acc = 0u64;
        for r in 0..reps {
            let base = bases[r % bases.len()];
            let mut lo = [0u8; 40];
            lo[..32].copy_from_slice(&base);
            let mut hi = [0xffu8; 40];
            hi[..32].copy_from_slice(&base);
            // "list the variants of X, best first" — one ordered range, no
            // plan decoded, the decision vector readable straight off the key.
            for (_k, d) in tree.range(lo..=hi) {
                acc += u64::from(*d);
            }
        }
        let prefix = t0.elapsed().as_nanos() as f64 / reps as f64;
        black_box(acc);

        println!("{v:>6} {probe:>14.1} {prefix:>14.1} {:>8.2}x", probe / prefix);
    }
    println!(
        "note: this is a pure in-memory lookup model. The registry lives in the\n\
         catalog's sys-keyspace, where a lookup is a B+tree descent per key, so\n\
         the real ratio favours the prefix scan MORE (one descent, not V)."
    );
}

// ============================================================ census replay

/// Read `occurrences \t plans \t hex(encoded footprint)` lines produced by
/// `sqlite_corpus --footprint-census=<path>`.
fn load_census(path: &str) -> Option<(Vec<Footprint>, Vec<u64>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (mut shapes, mut weights) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let mut it = line.split('\t');
        let occ: u64 = it.next()?.parse().ok()?;
        let _plans = it.next()?;
        let hex = it.next()?;
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
            .collect();
        if let Ok(f) = Footprint::decode(&bytes, &mut 0) {
            shapes.push(f);
            weights.push(occ);
        }
    }
    if shapes.is_empty() {
        None
    } else {
        Some((shapes, weights))
    }
}

fn synthetic() -> (Vec<Footprint>, Vec<u64>) {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut shapes = Vec::new();
    for _ in 0..2000 {
        let fan = 1 + (rng.below(3) as usize);
        let ids: Vec<u32> = (0..fan).map(|_| rng.below(64)).collect();
        let mut f = fp(&ids, if rng.below(4) == 0 { &ids } else { &[] });
        if rng.below(2) == 0 {
            f.key_access = KeyAccess::Point(vec![mpedb::KeyPart::Param(0)]);
        }
        shapes.push(f);
    }
    let w = vec![1u64; shapes.len()];
    (shapes, w)
}

fn main() {
    let arg = std::env::args().nth(1);
    let (shapes, weights) = match arg.as_deref().and_then(load_census) {
        Some(v) => {
            println!(
                "replaying {} distinct REAL footprints from {}",
                v.0.len(),
                arg.as_deref().unwrap()
            );
            v
        }
        None => {
            println!("no census file given (or unreadable) — using synthetic shapes");
            synthetic()
        }
    };
    bench_conflicts();
    bench_routing(&shapes);
    bench_variants();
    bench_compression(&shapes, &weights);
}
