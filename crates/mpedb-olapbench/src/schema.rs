//! The dataset: a star schema, generated identically for every engine.
//!
//! Why a star and not TPC-H: TPC-H's value is its *published* numbers, and we
//! cannot publish comparable ones without the full qualification kit. What we
//! need instead is a shape that separates the three things this benchmark is
//! actually about — scan-heavy aggregation (where a column store should win),
//! precomputed access paths (where mpedb's index-served aggregates should), and
//! join ordering over enough tables that the order matters (MPEE). A star with
//! four dimensions does all three and fits in a laptop's page cache, which
//! keeps the measurement about engines rather than about disks.
//!
//! The generator is a deterministic xorshift — the same rows in the same order
//! for every engine, every run, every machine. A benchmark whose data differs
//! between the engines it compares is measuring the generator.

pub const DIM_CUSTOMER: usize = 20_000;
pub const DIM_PRODUCT: usize = 5_000;
pub const DIM_STORE: usize = 200;
pub const DIM_DAY: usize = 1_461; // four years

/// Deterministic, seedable, no `rand` dependency — the repo's convention.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

pub struct Fact {
    pub id: i64,
    pub day_id: i64,
    pub customer_id: i64,
    pub product_id: i64,
    pub store_id: i64,
    pub qty: i64,
    pub amount: f64,
}

/// One fact row. Skew is deliberate and mild: customers are drawn from a
/// squared distribution so a group-by has a heavy head, which is what real
/// aggregation hits and what a uniform generator hides.
pub fn fact_row(id: i64, rng: &mut Rng) -> Fact {
    let c = rng.below(DIM_CUSTOMER as u64);
    let c = (c * c) / DIM_CUSTOMER as u64;
    Fact {
        id,
        day_id: rng.below(DIM_DAY as u64) as i64,
        customer_id: c as i64,
        product_id: rng.below(DIM_PRODUCT as u64) as i64,
        store_id: rng.below(DIM_STORE as u64) as i64,
        qty: 1 + rng.below(20) as i64,
        // Two decimals, so float equality across engines is not luck.
        amount: (rng.below(1_000_00) as f64) / 100.0,
    }
}

pub fn customer_row(id: i64, rng: &mut Rng) -> (i64, String, String, i64) {
    let nation = NATIONS[rng.below(NATIONS.len() as u64) as usize];
    let segment = SEGMENTS[rng.below(SEGMENTS.len() as u64) as usize];
    (id, format!("customer#{id:06}"), format!("{nation}|{segment}"), rng.below(80) as i64)
}

pub fn product_row(id: i64, rng: &mut Rng) -> (i64, String, String, f64) {
    let cat = CATEGORIES[rng.below(CATEGORIES.len() as u64) as usize];
    (id, format!("product#{id:05}"), cat.to_string(), (rng.below(50_000) as f64) / 100.0)
}

pub fn store_row(id: i64, rng: &mut Rng) -> (i64, String, String) {
    let nation = NATIONS[rng.below(NATIONS.len() as u64) as usize];
    (id, format!("store#{id:03}"), nation.to_string())
}

/// Days are a dimension rather than a date column on the fact, because that is
/// what forces the four-table join the join-order tests need.
pub fn day_row(id: i64) -> (i64, i64, i64, i64) {
    let year = 2022 + id / 365;
    let doy = id % 365;
    (id, year, 1 + doy / 31, 1 + doy % 28)
}

pub const NATIONS: [&str; 12] = [
    "NORWAY", "SWEDEN", "DENMARK", "FINLAND", "GERMANY", "FRANCE", "SPAIN", "ITALY", "POLAND",
    "UK", "IRELAND", "NETHERLANDS",
];
pub const SEGMENTS: [&str; 5] = ["RETAIL", "WHOLESALE", "ONLINE", "PARTNER", "INTERNAL"];
pub const CATEGORIES: [&str; 8] = [
    "tools", "grocery", "apparel", "furniture", "media", "garden", "sports", "office",
];
