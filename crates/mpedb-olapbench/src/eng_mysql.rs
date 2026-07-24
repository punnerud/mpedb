//! MariaDB 10.11 adapter: a THROWAWAY private server (`mariadb-install-db` +
//! `mariadbd --skip-grant-tables --skip-networking` as the current user, on a
//! private socket — the system service is never touched), so the bench needs no
//! sudo, no user grants, and no password. Same star schema, same indexes, same
//! rows as the others, loaded with batched multi-row INSERTs under one
//! transaction.
//!
//! Durability is set to the none-class (`innodb_flush_log_at_trx_commit=0`,
//! doublewrite off) to match the other in-process engines. Like PostgreSQL it
//! still pays client/server round-trips the embedded engines do not — part of
//! its honest shape here.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mysql::prelude::*;
use mysql::{Conn, OptsBuilder};

use crate::schema::*;

fn mariadb_bin(name: &str) -> String {
    for d in ["/usr/sbin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"] {
        let p = format!("{d}/{name}");
        if Path::new(&p).exists() {
            return p;
        }
    }
    name.to_string()
}

const DDL: &str = "
CREATE TABLE customer (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL,
                       nation_segment VARCHAR(64) NOT NULL, age INT NOT NULL);
CREATE TABLE product  (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL,
                       category VARCHAR(64) NOT NULL, price DOUBLE NOT NULL);
CREATE TABLE store    (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL, nation VARCHAR(64) NOT NULL);
CREATE TABLE day      (id INT PRIMARY KEY, year INT NOT NULL, month INT NOT NULL, dom INT NOT NULL);
CREATE TABLE fact     (id INT PRIMARY KEY, day_id INT NOT NULL, customer_id INT NOT NULL,
                       product_id INT NOT NULL, store_id INT NOT NULL, qty INT NOT NULL,
                       amount DOUBLE NOT NULL);
";

const INDEXES: &[&str] = &[
    "CREATE INDEX fact_day      ON fact(day_id)",
    "CREATE INDEX fact_customer ON fact(customer_id)",
    "CREATE INDEX fact_product  ON fact(product_id)",
    "CREATE INDEX fact_store    ON fact(store_id)",
    "CREATE INDEX fact_amount   ON fact(amount)",
    "CREATE INDEX cust_ns       ON customer(nation_segment)",
    "CREATE INDEX prod_cat      ON product(category)",
    "CREATE INDEX store_nation  ON store(nation)",
    "CREATE INDEX day_year      ON day(year)",
];

pub struct Mysql {
    conn: RefCell<Conn>,
    child: Option<Child>,
    datadir: PathBuf,
    sock: PathBuf,
}

impl Mysql {
    pub fn load(facts: i64, base: &Path) -> Result<(Mysql, f64), Box<dyn std::error::Error>> {
        let datadir = base.join("mariadb");
        let sock = base.join("mariadb.sock");
        let _ = std::fs::remove_dir_all(&datadir);
        std::fs::create_dir_all(&datadir)?;

        let install = Command::new(mariadb_bin("mariadb-install-db"))
            .args([
                "--no-defaults",
                &format!("--datadir={}", datadir.display()),
                "--auth-root-authentication-method=normal",
                "--skip-test-db",
                "--skip-name-resolve",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !install.status.success() {
            return Err(format!(
                "mariadb-install-db failed: {}",
                String::from_utf8_lossy(&install.stderr)
            )
            .into());
        }

        let child = Command::new(mariadb_bin("mariadbd"))
            .args([
                "--no-defaults",
                &format!("--datadir={}", datadir.display()),
                &format!("--socket={}", sock.display()),
                &format!("--pid-file={}", datadir.join("mariadb.pid").display()),
                "--skip-networking",
                "--skip-grant-tables",
                "--innodb-buffer-pool-size=512M",
                "--innodb-flush-log-at-trx-commit=0",
                "--innodb-doublewrite=0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        // Poll the socket until the server accepts a connection (≤ 30 s).
        let mut conn = None;
        let t_wait = Instant::now();
        while t_wait.elapsed() < Duration::from_secs(30) {
            if sock.exists() {
                let opts = OptsBuilder::new()
                    .socket(Some(sock.to_string_lossy().into_owned()))
                    .user(Some("root"));
                if let Ok(c) = Conn::new(opts) {
                    conn = Some(c);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut conn = conn.ok_or("mariadbd did not become reachable within 30 s")?;

        conn.query_drop("CREATE DATABASE bench")?;
        conn.query_drop("USE bench")?;
        for stmt in DDL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            conn.query_drop(stmt)?;
        }

        let t0 = Instant::now();
        Self::load_rows(&mut conn, facts)?;
        for ix in INDEXES {
            conn.query_drop(ix)?;
        }
        conn.query_drop("ANALYZE TABLE fact, customer, product, store, day")?;
        let load_s = t0.elapsed().as_secs_f64();

        Ok((
            Mysql {
                conn: RefCell::new(conn),
                child: Some(child),
                datadir,
                sock,
            },
            load_s,
        ))
    }

    fn load_rows(conn: &mut Conn, facts: i64) -> Result<(), Box<dyn std::error::Error>> {
        conn.query_drop("SET autocommit=0")?;
        conn.query_drop("SET unique_checks=0")?;
        conn.query_drop("SET foreign_key_checks=0")?;
        let mut rng = Rng::new(0x5EED_0001);

        let esc = |s: &str| s.replace('\'', "''");

        let mut vals: Vec<String> = Vec::new();
        for id in 0..DIM_CUSTOMER as i64 {
            let (id, name, ns, age) = customer_row(id, &mut rng);
            vals.push(format!("({id},'{}','{}',{age})", esc(&name), esc(&ns)));
        }
        conn.query_drop(format!(
            "INSERT INTO customer (id,name,nation_segment,age) VALUES {}",
            vals.join(",")
        ))?;

        vals.clear();
        for id in 0..DIM_PRODUCT as i64 {
            let (id, name, cat, price) = product_row(id, &mut rng);
            vals.push(format!("({id},'{}','{}',{price})", esc(&name), esc(&cat)));
        }
        conn.query_drop(format!(
            "INSERT INTO product (id,name,category,price) VALUES {}",
            vals.join(",")
        ))?;

        vals.clear();
        for id in 0..DIM_STORE as i64 {
            let (id, name, nation) = store_row(id, &mut rng);
            vals.push(format!("({id},'{}','{}')", esc(&name), esc(&nation)));
        }
        conn.query_drop(format!("INSERT INTO store (id,name,nation) VALUES {}", vals.join(",")))?;

        vals.clear();
        for id in 0..DIM_DAY as i64 {
            let (id, y, m, d) = day_row(id);
            vals.push(format!("({id},{y},{m},{d})"));
        }
        conn.query_drop(format!("INSERT INTO day (id,year,month,dom) VALUES {}", vals.join(",")))?;

        // Facts in batches to keep each statement a sane size.
        let mut frng = Rng::new(0x5EED_FAC7);
        const BATCH: i64 = 5_000;
        let mut batch: Vec<String> = Vec::with_capacity(BATCH as usize);
        for id in 0..facts {
            let f = fact_row(id, &mut frng);
            batch.push(format!(
                "({},{},{},{},{},{},{})",
                f.id, f.day_id, f.customer_id, f.product_id, f.store_id, f.qty, f.amount
            ));
            if batch.len() as i64 == BATCH {
                conn.query_drop(format!(
                    "INSERT INTO fact (id,day_id,customer_id,product_id,store_id,qty,amount) VALUES {}",
                    batch.join(",")
                ))?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            conn.query_drop(format!(
                "INSERT INTO fact (id,day_id,customer_id,product_id,store_id,qty,amount) VALUES {}",
                batch.join(",")
            ))?;
        }
        conn.query_drop("COMMIT")?;
        conn.query_drop("SET autocommit=1")?;
        Ok(())
    }

    pub fn explain(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let rows: Vec<mysql::Row> = self.conn.borrow_mut().query(format!("EXPLAIN {sql}"))?;
        let mut out = Vec::new();
        for row in &rows {
            let mut cells = Vec::new();
            for i in 0..row.len() {
                cells.push(crate::cell::render_mysql(row, i));
            }
            out.push(cells.join(" "));
        }
        Ok(out.join("\n"))
    }

    pub fn run(&self, sql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let rows: Vec<mysql::Row> = self.conn.borrow_mut().query(sql)?;
        let mut out = Vec::new();
        for row in &rows {
            let mut cells = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                cells.push(crate::cell::render_mysql(row, i));
            }
            out.push(cells.join("|"));
        }
        out.sort();
        Ok(out.join("\n"))
    }

    pub fn prepared_ms(&self, sql: &str, iters: i64, facts: i64) -> Result<f64, Box<dyn std::error::Error>> {
        let mut c = self.conn.borrow_mut();
        let st = c.prep(sql)?;
        let t0 = Instant::now();
        for i in 0..iters {
            let _: Vec<mysql::Row> = c.exec(&st, (i % facts,))?;
        }
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    }
}

impl Drop for Mysql {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}
