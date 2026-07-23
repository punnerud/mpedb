//! DuckDB, in-process and in-memory.
//!
//! No indexes, on purpose: DuckDB is a column store with per-row-group zone
//! maps, and its own documentation tells you not to build ART indexes for
//! analytics. Benchmarking it with a configuration its authors advise against
//! would be manufacturing a loss.
//!
//! Loaded through the Appender, which is DuckDB's bulk path — an INSERT loop
//! would report a load time that says more about the binding than the engine.

use std::time::Instant;

use duckdb::{params, Connection};

use crate::schema::*;

pub struct Duck {
    pub conn: Connection,
}

const DDL: &str = "
CREATE TABLE customer (id BIGINT, name VARCHAR NOT NULL, nation_segment VARCHAR NOT NULL,
                       age BIGINT NOT NULL);
CREATE TABLE product  (id BIGINT, name VARCHAR NOT NULL, category VARCHAR NOT NULL,
                       price DOUBLE NOT NULL);
CREATE TABLE store    (id BIGINT, name VARCHAR NOT NULL, nation VARCHAR NOT NULL);
CREATE TABLE day      (id BIGINT, year BIGINT NOT NULL, month BIGINT NOT NULL, dom BIGINT NOT NULL);
CREATE TABLE fact     (id BIGINT, day_id BIGINT NOT NULL, customer_id BIGINT NOT NULL,
                       product_id BIGINT NOT NULL, store_id BIGINT NOT NULL,
                       qty BIGINT NOT NULL, amount DOUBLE NOT NULL);
";

impl Duck {
    pub fn load(facts: i64) -> Result<(Duck, f64), Box<dyn std::error::Error>> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(DDL)?;

        let t0 = Instant::now();

        let mut rng = Rng::new(0x5EED_0001);
        {
            let mut app = conn.appender("customer")?;
            for id in 0..DIM_CUSTOMER as i64 {
                let (id, name, ns, age) = customer_row(id, &mut rng);
                app.append_row(params![id, name, ns, age])?;
            }
        }
        {
            let mut app = conn.appender("product")?;
            for id in 0..DIM_PRODUCT as i64 {
                let (id, name, cat, price) = product_row(id, &mut rng);
                app.append_row(params![id, name, cat, price])?;
            }
        }
        {
            let mut app = conn.appender("store")?;
            for id in 0..DIM_STORE as i64 {
                let (id, name, nation) = store_row(id, &mut rng);
                app.append_row(params![id, name, nation])?;
            }
        }
        {
            let mut app = conn.appender("day")?;
            for id in 0..DIM_DAY as i64 {
                let (id, y, m, d) = day_row(id);
                app.append_row(params![id, y, m, d])?;
            }
        }
        {
            let mut app = conn.appender("fact")?;
            let mut rng = Rng::new(0x5EED_FAC7);
            for id in 0..facts {
                let f = fact_row(id, &mut rng);
                app.append_row(params![
                    f.id, f.day_id, f.customer_id, f.product_id, f.store_id, f.qty, f.amount
                ])?;
            }
        }

        let load_s = t0.elapsed().as_secs_f64();
        Ok((Duck { conn }, load_s))
    }

    /// DuckDB's own plan, unedited.
    pub fn explain(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN {sql}"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let n = row.as_ref().column_count();
            out.push(crate::cell::render_duck(row, n - 1));
        }
        Ok(out.join("\n"))
    }

    pub fn run(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        // The column count is only available once the statement has run —
        // asking the prepared statement first panics inside the binding.
        while let Some(row) = rows.next()? {
            let ncols = row.as_ref().column_count();
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                cells.push(crate::cell::render_duck(row, i));
            }
            out.push(cells.join("|"));
        }
        out.sort();
        Ok(out.join("\n"))
    }
}
