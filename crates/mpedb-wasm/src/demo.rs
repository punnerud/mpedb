//! The demo database.
//!
//! Built entirely from SQL that the page displays verbatim: the DDL and the
//! INSERTs below are generated once, executed, and handed to the browser as
//! `seed_sql`. A visitor can therefore check every result against the data
//! that produced it — a demo whose rows appeared from nowhere can only be
//! taken on faith, which is the opposite of what this page is for.
//!
//! Row generation is a deterministic xorshift (the workspace convention — no
//! `rand` dependency), so the database is byte-identical on every load and the
//! example queries always have the same answers.

use mpedb::{Config, Database};
use mpedb_types::Error;

/// The seed schema — the TOML config a native mpedb file is born from.
///
/// It contains ONE table, and none of the demo tables: those are created by
/// real `CREATE TABLE` at run time. That split is itself a thing worth
/// showing. `M_SCHEMA_HASH` freezes the SEED forever, while the LIVE schema is
/// read back from the catalog and may have grown past it via DDL (#47) — so
/// `playground` is what the file was created with, and `users`/`products`/
/// `orders` are what it grew into.
pub const CONFIG_TOML: &str = "\
[database]
path = \":memory:\"
size_mb = 64
max_readers = 8

[[table]]
name = \"playground\"
primary_key = [\"k\"]

  [[table.column]]
  name = \"k\"
  type = \"text\"

  [[table.column]]
  name = \"v\"
  type = \"text\"
";

/// Deterministic xorshift64*, the workspace RNG convention.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

const COUNTRIES: [&str; 8] = ["NO", "SE", "DK", "FI", "DE", "FR", "GB", "US"];
const CATEGORIES: [&str; 5] = ["keyboard", "display", "audio", "storage", "cable"];
const STATUSES: [&str; 4] = ["placed", "shipped", "delivered", "returned"];
const FIRST: [&str; 12] = [
    "Morten", "Ingrid", "Lars", "Astrid", "Jonas", "Kari", "Erik", "Sofie", "Nils", "Maja",
    "Anders", "Live",
];
const LAST: [&str; 8] = [
    "Berg", "Dahl", "Haugen", "Lie", "Moen", "Ness", "Ruud", "Vik",
];
const NOUNS: [&str; 10] = [
    "Compact", "Studio", "Field", "Marine", "Alpine", "Harbour", "Fjord", "Nordic", "Atlas",
    "Vector",
];

const N_USERS: u64 = 60;
const N_PRODUCTS: u64 = 40;
const N_ORDERS: u64 = 400;

/// The DDL. Every constraint here is load-bearing for the examples: the
/// `CHECK`s and `NOT NULL`s are what mpedb REFUSES on, and the indexes are
/// what the `min`/`max` boundary probe and the join access paths ride.
fn ddl() -> String {
    "\
-- `playground` came from the TOML seed schema (the config a native .mpedb
-- file is born from). Everything below is real DDL, executed at load: the
-- LIVE schema grows past the frozen seed.
INSERT INTO playground (k, v) VALUES
  ('build', 'mpedb compiled to wasm32-unknown-unknown'),
  ('engine', 'the real one -- COW B+tree, MVCC, rigid validation'),
  ('durability', 'none (in-memory; the browser build refuses any other)');

-- Rigid schema: typed columns, NOT NULL, UNIQUE, CHECK.
-- These are enforced, not decoration -- see the 'refusals' examples.
CREATE TABLE users (
  id      INTEGER PRIMARY KEY,
  name    TEXT    NOT NULL,
  email   TEXT    NOT NULL UNIQUE,
  country TEXT    NOT NULL,
  age     INTEGER NOT NULL CHECK (age >= 13 AND age < 130)
);

CREATE TABLE products (
  id          INTEGER PRIMARY KEY,
  sku         TEXT    NOT NULL UNIQUE,
  name        TEXT    NOT NULL,
  category    TEXT    NOT NULL,
  price_cents INTEGER NOT NULL CHECK (price_cents > 0)
);

CREATE TABLE orders (
  id          INTEGER PRIMARY KEY,
  user_id     INTEGER NOT NULL,
  product_id  INTEGER NOT NULL,
  qty         INTEGER NOT NULL CHECK (qty > 0),
  total_cents INTEGER NOT NULL,
  status      TEXT    NOT NULL
);

-- Secondary indexes. `price_cents` is what makes min()/max() a boundary
-- probe (descend to one edge of the tree) instead of a scan.
CREATE INDEX users_country  ON users (country);
CREATE INDEX products_cat   ON products (category);
CREATE INDEX products_price ON products (price_cents);
CREATE INDEX orders_user    ON orders (user_id);
CREATE INDEX orders_product ON orders (product_id);
"
    .to_string()
}

fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Generate the INSERTs. Multi-row VALUES, chunked so no single statement is
/// unreasonably large.
fn seed_rows() -> String {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut out = String::new();

    out.push_str("\n-- 60 users\n");
    let mut vals = Vec::new();
    for id in 1..=N_USERS {
        let first = *rng.pick(&FIRST);
        let last = *rng.pick(&LAST);
        let name = format!("{first} {last}");
        let email = format!("{}.{}{}@example.com", first.to_lowercase(), last.to_lowercase(), id);
        let country = *rng.pick(&COUNTRIES);
        let age = 18 + rng.below(50);
        vals.push(format!(
            "({id},{},{},{},{age})",
            sql_quote(&name),
            sql_quote(&email),
            sql_quote(country)
        ));
    }
    push_inserts(&mut out, "users (id, name, email, country, age)", &vals, 20);

    out.push_str("\n-- 40 products\n");
    let mut vals = Vec::new();
    for id in 1..=N_PRODUCTS {
        let cat = *rng.pick(&CATEGORIES);
        let noun = *rng.pick(&NOUNS);
        let name = format!("{noun} {cat}");
        let sku = format!("{}-{:04}", cat[..3].to_uppercase(), id);
        // Spread prices widely so min()/max() are visibly not the first row.
        let price = 500 + rng.below(48_000);
        vals.push(format!(
            "({id},{},{},{},{price})",
            sql_quote(&sku),
            sql_quote(&name),
            sql_quote(cat)
        ));
    }
    push_inserts(&mut out, "products (id, sku, name, category, price_cents)", &vals, 20);

    out.push_str("\n-- 400 orders\n");
    let mut vals = Vec::new();
    for id in 1..=N_ORDERS {
        let user = 1 + rng.below(N_USERS);
        let product = 1 + rng.below(N_PRODUCTS);
        let qty = 1 + rng.below(5);
        // total_cents is stored, not derived -- so a query can check it.
        let total = qty * (500 + rng.below(48_000));
        let status = *rng.pick(&STATUSES);
        vals.push(format!(
            "({id},{user},{product},{qty},{total},{})",
            sql_quote(status)
        ));
    }
    push_inserts(
        &mut out,
        "orders (id, user_id, product_id, qty, total_cents, status)",
        &vals,
        50,
    );
    out
}

fn push_inserts(out: &mut String, target: &str, vals: &[String], chunk: usize) {
    for group in vals.chunks(chunk) {
        out.push_str("INSERT INTO ");
        out.push_str(target);
        out.push_str(" VALUES\n  ");
        out.push_str(&group.join(",\n  "));
        out.push_str(";\n");
    }
}

/// Build the demo database by executing real DDL and INSERTs, returning the
/// database and the **exact script that produced it** for the page to display.
pub fn create() -> Result<(Database, String), Error> {
    let db = Database::open_with_config(Config::from_toml_str(CONFIG_TOML)?)?;
    let script = format!("{}{}", ddl(), seed_rows());
    for stmt in split_statements(&script) {
        db.query(&stmt, &[])?;
    }
    Ok((db, script))
}

/// Split a script on `;` at statement level. The generated script contains no
/// semicolons inside string literals (emails and names are alphanumeric, and
/// `sql_quote` only ever doubles quotes), but the scan honours quoting anyway
/// so a future edit cannot silently split a literal in half.
pub fn split_statements(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if in_str && chars.peek() == Some(&'\'') => {
                cur.push('\'');
                cur.push(chars.next().unwrap_or('\''));
            }
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '-' if !in_str && chars.peek() == Some(&'-') => {
                // line comment: drop to end of line
                for c2 in chars.by_ref() {
                    if c2 == '\n' {
                        break;
                    }
                }
                cur.push('\n');
            }
            ';' if !in_str => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}
