//! SQLite, in-process, bundled — the control group.
//!
//! Not here to win anything. It is here because without it, every gap to
//! DuckDB reads as a fact about mpedb, when most of it is a fact about row
//! stores. SQLite is indexed exactly like mpedb, for the same reason mpedb is:
//! that is what its users do.

use std::time::Instant;

use rusqlite::{params, Connection};

use crate::schema::*;

pub struct Sqlite {
    pub conn: Connection,
}

const DDL: &str = "
PRAGMA journal_mode=OFF;
PRAGMA synchronous=OFF;
CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                       nation_segment TEXT NOT NULL, age INTEGER NOT NULL);
CREATE TABLE product  (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                       category TEXT NOT NULL, price REAL NOT NULL);
CREATE TABLE store    (id INTEGER PRIMARY KEY, name TEXT NOT NULL, nation TEXT NOT NULL);
CREATE TABLE day      (id INTEGER PRIMARY KEY, year INTEGER NOT NULL,
                       month INTEGER NOT NULL, dom INTEGER NOT NULL);
CREATE TABLE fact     (id INTEGER PRIMARY KEY, day_id INTEGER NOT NULL,
                       customer_id INTEGER NOT NULL, product_id INTEGER NOT NULL,
                       store_id INTEGER NOT NULL, qty INTEGER NOT NULL, amount REAL NOT NULL);
";

const INDEXES: &str = "
CREATE INDEX fact_day      ON fact(day_id);
CREATE INDEX fact_customer ON fact(customer_id);
CREATE INDEX fact_product  ON fact(product_id);
CREATE INDEX fact_store    ON fact(store_id);
CREATE INDEX fact_amount   ON fact(amount);
CREATE INDEX cust_ns       ON customer(nation_segment);
CREATE INDEX prod_cat      ON product(category);
CREATE INDEX store_nation  ON store(nation);
CREATE INDEX day_year      ON day(year);
";

impl Sqlite {
    pub fn load(facts: i64) -> Result<(Sqlite, f64), Box<dyn std::error::Error>> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(DDL)?;

        let t0 = Instant::now();

        let mut rng = Rng::new(0x5EED_0001);
        conn.execute_batch("BEGIN")?;
        {
            let mut st =
                conn.prepare("INSERT INTO customer (id,name,nation_segment,age) VALUES (?,?,?,?)")?;
            for id in 0..DIM_CUSTOMER as i64 {
                let (id, name, ns, age) = customer_row(id, &mut rng);
                st.execute(params![id, name, ns, age])?;
            }
        }
        {
            let mut st =
                conn.prepare("INSERT INTO product (id,name,category,price) VALUES (?,?,?,?)")?;
            for id in 0..DIM_PRODUCT as i64 {
                let (id, name, cat, price) = product_row(id, &mut rng);
                st.execute(params![id, name, cat, price])?;
            }
        }
        {
            let mut st = conn.prepare("INSERT INTO store (id,name,nation) VALUES (?,?,?)")?;
            for id in 0..DIM_STORE as i64 {
                let (id, name, nation) = store_row(id, &mut rng);
                st.execute(params![id, name, nation])?;
            }
        }
        {
            let mut st = conn.prepare("INSERT INTO day (id,year,month,dom) VALUES (?,?,?,?)")?;
            for id in 0..DIM_DAY as i64 {
                let (id, y, m, d) = day_row(id);
                st.execute(params![id, y, m, d])?;
            }
        }
        {
            let mut st = conn.prepare(
                "INSERT INTO fact (id,day_id,customer_id,product_id,store_id,qty,amount) \
                 VALUES (?,?,?,?,?,?,?)",
            )?;
            let mut rng = Rng::new(0x5EED_FAC7);
            for id in 0..facts {
                let f = fact_row(id, &mut rng);
                st.execute(params![
                    f.id, f.day_id, f.customer_id, f.product_id, f.store_id, f.qty, f.amount
                ])?;
            }
        }
        conn.execute_batch("COMMIT")?;
        // Indexes built AFTER the rows, which is what a loader does and what
        // makes the comparison to mpedb's load honest — mpedb maintains its
        // trees incrementally and has no bulk-build shortcut.
        conn.execute_batch(INDEXES)?;
        conn.execute_batch("ANALYZE")?;

        let load_s = t0.elapsed().as_secs_f64();
        Ok((Sqlite { conn }, load_s))
    }

    /// SQLite's own plan, unedited.
    pub fn explain(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get::<_, String>(3)?);
        }
        Ok(out.join("\n"))
    }

    pub fn run(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(sql)?;
        let ncols = stmt.column_count();
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                cells.push(crate::cell::render_sqlite(row, i));
            }
            out.push(cells.join("|"));
        }
        out.sort();
        Ok(out.join("\n"))
    }
}
