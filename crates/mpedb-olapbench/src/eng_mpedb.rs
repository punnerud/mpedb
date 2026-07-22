//! mpedb, in-process.
//!
//! Indexed the way a user of a row store would index a star schema: every join
//! key and every filtered dimension column gets one, plus `amount` so the
//! extremum queries have a tree to descend. DuckDB gets none, because a DuckDB
//! user builds none — it is a column store with zone maps, and telling it to
//! build ART indexes on a fact table would be benchmarking a configuration
//! nobody ships. **That asymmetry is the honest one, and it has a price that
//! this harness reports rather than hides: mpedb pays for those trees at load
//! time, in the load column.**

use std::path::{Path, PathBuf};
use std::time::Instant;

use mpedb::{params, Config, Database, ExecResult, PlanHash, Value};

use crate::schema::*;

pub struct Mpedb {
    pub db: Database,
    pub path: PathBuf,
}

/// The whole star, as the seed schema. mpedb is file-authoritative: the config
/// that creates the file also freezes its hash, so the schema lives here rather
/// than in a pile of `CREATE TABLE`s.
fn config_toml(path: &Path, size_mb: u64) -> String {
    format!(
        r#"
[database]
path = "{path}"
size_mb = {size_mb}
max_readers = 64
durability = "none"

[[table]]
name = "fact"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "day_id"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "customer_id"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "product_id"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "store_id"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "qty"
  type = "int64"
  [[table.column]]
  name = "amount"
  type = "float64"
  indexed = true

[[table]]
name = "customer"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "name"
  type = "text"
  [[table.column]]
  name = "nation_segment"
  type = "text"
  indexed = true
  [[table.column]]
  name = "age"
  type = "int64"

[[table]]
name = "product"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "name"
  type = "text"
  [[table.column]]
  name = "category"
  type = "text"
  indexed = true
  [[table.column]]
  name = "price"
  type = "float64"

[[table]]
name = "store"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "name"
  type = "text"
  [[table.column]]
  name = "nation"
  type = "text"
  indexed = true

[[table]]
name = "day"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "year"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "month"
  type = "int64"
  [[table.column]]
  name = "dom"
  type = "int64"
"#,
        path = path.display(),
        size_mb = size_mb
    )
}

impl Mpedb {
    /// Create and fill. Returns the load wall time.
    pub fn load(dir: &Path, facts: i64) -> Result<(Mpedb, f64), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("olap.mpedb");
        let _ = std::fs::remove_file(&path);

        // A fact row is seven columns in the base tree plus an entry in each of
        // five secondary trees, and COW churn during the load needs room on top
        // of the steady state. 200 B/row was measured to be too little — 2M
        // rows hit DbFull — so this reserves 700 B/row and 1 GiB of floor. The
        // file is pre-reserved, so over-reserving costs disk, not memory, while
        // under-reserving ends the run.
        let size_mb = 1024 + (facts as u64 * 700) / (1024 * 1024);

        let db = Database::open_with_config(Config::from_toml_str(&config_toml(&path, size_mb))?)?;

        // Prepared before the write session — the facade's locking rule.
        let ins_fact = db.prepare(
            "INSERT INTO fact (id, day_id, customer_id, product_id, store_id, qty, amount) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )?;
        let ins_customer =
            db.prepare("INSERT INTO customer (id, name, nation_segment, age) VALUES ($1,$2,$3,$4)")?;
        let ins_product =
            db.prepare("INSERT INTO product (id, name, category, price) VALUES ($1,$2,$3,$4)")?;
        let ins_store = db.prepare("INSERT INTO store (id, name, nation) VALUES ($1,$2,$3)")?;
        let ins_day = db.prepare("INSERT INTO day (id, year, month, dom) VALUES ($1,$2,$3,$4)")?;

        let t0 = Instant::now();

        let mut rng = Rng::new(0x5EED_0001);
        let mut s = db.begin()?;
        for id in 0..DIM_CUSTOMER as i64 {
            let (id, name, ns, age) = customer_row(id, &mut rng);
            s.execute(&ins_customer, &params![id, name, ns, age])?;
        }
        for id in 0..DIM_PRODUCT as i64 {
            let (id, name, cat, price) = product_row(id, &mut rng);
            s.execute(&ins_product, &params![id, name, cat, price])?;
        }
        for id in 0..DIM_STORE as i64 {
            let (id, name, nation) = store_row(id, &mut rng);
            s.execute(&ins_store, &params![id, name, nation])?;
        }
        for id in 0..DIM_DAY as i64 {
            let (id, y, m, d) = day_row(id);
            s.execute(&ins_day, &params![id, y, m, d])?;
        }
        s.commit()?;

        // Facts in batches: one commit per batch, so the commit-path fixpoint
        // is exercised repeatedly rather than once over a giant write set.
        const BATCH: i64 = 50_000;
        let mut rng = Rng::new(0x5EED_FAC7);
        let mut id = 0i64;
        while id < facts {
            let end = (id + BATCH).min(facts);
            let mut s = db.begin()?;
            while id < end {
                let f = fact_row(id, &mut rng);
                s.execute(
                    &ins_fact,
                    &params![f.id, f.day_id, f.customer_id, f.product_id, f.store_id, f.qty, f.amount],
                )?;
                id += 1;
            }
            s.commit()?;
        }

        let load_s = t0.elapsed().as_secs_f64();
        Ok((Mpedb { db, path }, load_s))
    }

    pub fn file_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Run once, returning a canonical rendering of the result so the harness
    /// can check every engine answered the SAME thing before believing a time.
    pub fn run(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        Ok(render(self.db.query(sql, &[])?))
    }

    /// The engine's own EXPLAIN, unedited.
    pub fn explain(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        match self.db.query(&format!("EXPLAIN {sql}"), &[])? {
            ExecResult::Explain(text) => Ok(text),
            ExecResult::Rows { rows, .. } => Ok(rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Text(t) => t.to_string(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n")),
            other => Ok(format!("{other:?}")),
        }
    }

    pub fn prepare(&self, sql: &str) -> Result<PlanHash, Box<dyn std::error::Error>> {
        Ok(self.db.prepare(sql)?)
    }

    pub fn exec_param(&self, h: &PlanHash, p: i64) -> Result<(), Box<dyn std::error::Error>> {
        self.db.execute(h, &params![p])?;
        Ok(())
    }
}

/// Canonical result rendering, shared shape with the other adapters: sorted
/// rows, floats to 2 decimals. Sorting is the point — the query set has no
/// ORDER BY, so row order is an engine's business and must not decide equality.
pub fn render(r: ExecResult) -> String {
    let ExecResult::Rows { rows, .. } = r else {
        return String::from("(no rows)");
    };
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
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
    out.join("\n")
}
