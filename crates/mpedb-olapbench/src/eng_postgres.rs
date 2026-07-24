//! PostgreSQL 16 adapter: a THROWAWAY private cluster (initdb + pg_ctl as the
//! current user, on a private socket — the system cluster is never touched), so
//! the bench needs no sudo, no role setup, and no password. Same star schema,
//! same indexes, same rows as mpedb/SQLite, loaded via COPY.
//!
//! Durability is set to the none-class (`fsync=off, synchronous_commit=off`) to
//! match the other in-process engines, which run in memory with no barrier —
//! this is a read-latency comparison, not a durability one. Stated plainly:
//! PostgreSQL still pays client/server IPC + protocol round-trips that the
//! embedded engines do not, which is a real part of its shape here.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use postgres::{Client, NoTls};

use crate::schema::*;

const PORT: u16 = 54331; // distinct from mpedb-bench's 54329

fn pg_bin() -> String {
    if let Ok(d) = std::env::var("MPEDB_PG_BIN") {
        return d;
    }
    for d in [
        "/usr/lib/postgresql/16/bin",
        "/usr/lib/postgresql/17/bin",
        "/usr/lib/postgresql/15/bin",
        "/opt/homebrew/opt/postgresql@16/bin",
    ] {
        if Path::new(&format!("{d}/initdb")).exists() {
            return d.to_string();
        }
    }
    String::new() // fall back to $PATH
}

fn bin(name: &str) -> String {
    let d = pg_bin();
    if d.is_empty() {
        name.to_string()
    } else {
        format!("{d}/{name}")
    }
}

fn run_cmd(mut cmd: Command, what: &str) -> Result<(), Box<dyn std::error::Error>> {
    let out = cmd.output().map_err(|e| format!("{what}: spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what}: exit {:?}\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

const DDL: &str = "
CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                       nation_segment TEXT NOT NULL, age INTEGER NOT NULL);
CREATE TABLE product  (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                       category TEXT NOT NULL, price DOUBLE PRECISION NOT NULL);
CREATE TABLE store    (id INTEGER PRIMARY KEY, name TEXT NOT NULL, nation TEXT NOT NULL);
CREATE TABLE day      (id INTEGER PRIMARY KEY, year INTEGER NOT NULL,
                       month INTEGER NOT NULL, dom INTEGER NOT NULL);
CREATE TABLE fact     (id INTEGER PRIMARY KEY, day_id INTEGER NOT NULL,
                       customer_id INTEGER NOT NULL, product_id INTEGER NOT NULL,
                       store_id INTEGER NOT NULL, qty INTEGER NOT NULL,
                       amount DOUBLE PRECISION NOT NULL);
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

pub struct Postgres {
    client: RefCell<Client>,
    datadir: PathBuf,
    sockdir: PathBuf,
    running: bool,
}

impl Postgres {
    pub fn load(facts: i64, base: &Path) -> Result<(Postgres, f64), Box<dyn std::error::Error>> {
        let datadir = base.join("pgdata");
        let sockdir = base.join("pgsock");
        let _ = std::fs::remove_dir_all(&datadir);
        let _ = std::fs::remove_dir_all(&sockdir);
        std::fs::create_dir_all(&sockdir)?;
        std::fs::create_dir_all(datadir.parent().unwrap_or(Path::new("/")))?;

        let mut initdb = Command::new(bin("initdb"));
        initdb
            .arg("-D")
            .arg(&datadir)
            .args(["--auth=trust", "-U", "bench", "-E", "UTF8"])
            .args(["--locale=C", "--no-instructions", "--no-sync"]);
        run_cmd(initdb, "initdb")?;

        let opts = format!(
            "-c port={PORT} -c unix_socket_directories={} -c listen_addresses= \
             -c fsync=off -c synchronous_commit=off -c full_page_writes=off \
             -c shared_buffers=512MB -c work_mem=256MB -c max_connections=8",
            sockdir.display(),
        );
        let mut start = Command::new(bin("pg_ctl"));
        start
            .arg("-D")
            .arg(&datadir)
            .arg("-l")
            .arg(datadir.join("server.log"))
            .args(["-w", "-t", "60", "-o", &opts, "start"]);
        run_cmd(start, "pg_ctl start")?;

        let mut me = Postgres {
            client: RefCell::new(Self::connect(&sockdir)?),
            datadir,
            sockdir,
            running: true,
        };
        me.client.borrow_mut().batch_execute(DDL)?;

        let t0 = Instant::now();
        me.load_rows(facts)?;
        {
            let mut c = me.client.borrow_mut();
            c.batch_execute(INDEXES)?;
            c.batch_execute("ANALYZE")?;
        }
        let load_s = t0.elapsed().as_secs_f64();
        Ok((me, load_s))
    }

    fn connect(sockdir: &Path) -> Result<Client, Box<dyn std::error::Error>> {
        let conn = format!("host={} port={PORT} user=bench dbname=postgres", sockdir.display());
        Ok(Client::connect(&conn, NoTls)?)
    }

    fn load_rows(&self, facts: i64) -> Result<(), Box<dyn std::error::Error>> {
        let mut c = self.client.borrow_mut();
        let mut rng = Rng::new(0x5EED_0001);

        {
            let mut w = c.copy_in("COPY customer (id,name,nation_segment,age) FROM STDIN")?;
            let mut buf = String::new();
            for id in 0..DIM_CUSTOMER as i64 {
                let (id, name, ns, age) = customer_row(id, &mut rng);
                buf.clear();
                buf.push_str(&format!("{id}\t{name}\t{ns}\t{age}\n"));
                w.write_all(buf.as_bytes())?;
            }
            w.finish()?;
        }
        {
            let mut w = c.copy_in("COPY product (id,name,category,price) FROM STDIN")?;
            for id in 0..DIM_PRODUCT as i64 {
                let (id, name, cat, price) = product_row(id, &mut rng);
                w.write_all(format!("{id}\t{name}\t{cat}\t{price}\n").as_bytes())?;
            }
            w.finish()?;
        }
        {
            let mut w = c.copy_in("COPY store (id,name,nation) FROM STDIN")?;
            for id in 0..DIM_STORE as i64 {
                let (id, name, nation) = store_row(id, &mut rng);
                w.write_all(format!("{id}\t{name}\t{nation}\n").as_bytes())?;
            }
            w.finish()?;
        }
        {
            let mut w = c.copy_in("COPY day (id,year,month,dom) FROM STDIN")?;
            for id in 0..DIM_DAY as i64 {
                let (id, y, m, d) = day_row(id);
                w.write_all(format!("{id}\t{y}\t{m}\t{d}\n").as_bytes())?;
            }
            w.finish()?;
        }
        {
            let mut w = c.copy_in(
                "COPY fact (id,day_id,customer_id,product_id,store_id,qty,amount) FROM STDIN",
            )?;
            let mut frng = Rng::new(0x5EED_FAC7);
            for id in 0..facts {
                let f = fact_row(id, &mut frng);
                w.write_all(
                    format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                        f.id, f.day_id, f.customer_id, f.product_id, f.store_id, f.qty, f.amount
                    )
                    .as_bytes(),
                )?;
            }
            w.finish()?;
        }
        Ok(())
    }

    pub fn explain(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let rows = self.client.borrow_mut().query(&format!("EXPLAIN {sql}"), &[])?;
        let mut out = Vec::new();
        for row in &rows {
            let line: String = row.get(0);
            out.push(line);
        }
        Ok(out.join("\n"))
    }

    pub fn run(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let rows = self.client.borrow_mut().query(sql, &[])?;
        let mut out = Vec::new();
        for row in &rows {
            let mut cells = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                cells.push(crate::cell::render_pg(row, i));
            }
            out.push(cells.join("|"));
        }
        out.sort();
        Ok(out.join("\n"))
    }

    /// A prepared, parameterised loop: `sql` uses `?`, rewritten to `$1`.
    pub fn prepared_ms(&self, sql: &str, iters: i64, facts: i64) -> Result<f64, Box<dyn std::error::Error>> {
        let pgsql = sql.replace('?', "$1");
        let mut c = self.client.borrow_mut();
        let st = c.prepare(&pgsql)?;
        let t0 = Instant::now();
        for i in 0..iters {
            let _ = c.query(&st, &[&((i % facts) as i32)])?;
        }
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        if self.running {
            let _ = Command::new(bin("pg_ctl"))
                .arg("-D")
                .arg(&self.datadir)
                .args(["-m", "immediate", "-w", "-t", "20", "stop"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.datadir);
        let _ = std::fs::remove_dir_all(&self.sockdir);
    }
}
