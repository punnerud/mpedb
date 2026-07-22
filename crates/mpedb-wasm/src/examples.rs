//! The playground's example queries — **the single source of truth**.
//!
//! They live in Rust, not in the page's JavaScript, so that
//! `tests/examples.rs` can run every one of them against a native in-memory
//! database and assert it still does what its button claims. A page that
//! advertises "CHECK violation" on a statement the engine has since started
//! accepting would be misinformation, and this is what stops it silently
//! happening.
//!
//! The page fetches these through `mpedb_examples()` and only renders them.

/// What a statement is expected to do. The native test asserts it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expect {
    /// Compiles and executes.
    Runs,
    /// The engine refuses it — at bind time or at execution. Which one is not
    /// pinned here: that is a property of the engine the page reports, not a
    /// promise the page makes.
    Refuses,
}

pub struct Example {
    pub label: &'static str,
    /// The one-line reason this query is worth clicking.
    pub why: &'static str,
    pub sql: &'static str,
    pub expect: Expect,
}

pub struct Group {
    pub name: &'static str,
    pub items: &'static [Example],
}

use Expect::{Refuses, Runs};

pub const GROUPS: &[Group] = &[
    Group {
        name: "Reads",
        items: &[
            Example {
                label: "count over a table",
                why: "400 rows, counted from the tree",
                sql: "SELECT count(*) FROM orders",
                expect: Runs,
            },
            Example {
                label: "indexed min / max",
                why: "a boundary probe, not a scan",
                sql: "\
-- `products_price` indexes price_cents, so min/max descend to one edge of
-- that tree instead of reading every row. On a table of millions this is
-- the gap the README measures at ~4000x against sqlite.
SELECT min(price_cents), max(price_cents) FROM products",
                expect: Runs,
            },
            Example {
                label: "primary-key point lookup",
                why: "footprint says Point, not Full",
                sql: "SELECT id, name, email, country, age FROM users WHERE id = 7",
                expect: Runs,
            },
            Example {
                label: "GROUP BY + ORDER BY",
                why: "grouped aggregate",
                sql: "\
SELECT country, count(*) AS n, avg(age) AS mean_age
FROM users
GROUP BY country
ORDER BY count(*) DESC, country",
                expect: Runs,
            },
            Example {
                label: "HAVING over groups",
                why: "filter applied after grouping",
                sql: "\
SELECT p.category, count(*) AS orders, sum(o.total_cents) AS cents
FROM orders o JOIN products p ON p.id = o.product_id
GROUP BY p.category
HAVING count(*) > 60
ORDER BY sum(o.total_cents) DESC",
                expect: Runs,
            },
            Example {
                label: "correlated subquery",
                why: "re-evaluated per outer row",
                sql: "\
SELECT u.name, u.country,
       (SELECT count(*) FROM orders o WHERE o.user_id = u.id) AS orders
FROM users u
ORDER BY orders DESC, u.id
LIMIT 10",
                expect: Runs,
            },
        ],
    },
    Group {
        name: "MPEE join solver",
        items: &[
            Example {
                label: "reorder that kills a cartesian step",
                why: "as written costs 1 step, chosen costs 0",
                sql: "\
-- Written the order a person thinks of it: users, products, and only then
-- the orders table that actually connects them. Taken literally that joins
-- users to products with no predicate between them -- a cartesian step.
-- Open the MPEE tab to see both orders side by side.
SELECT u.name, p.name, o.qty
FROM users u, products p, orders o
WHERE o.user_id = u.id AND o.product_id = p.id
  AND u.email = 'kari.dahl1@example.com'",
                expect: Runs,
            },
            Example {
                label: "reorder driven by selectivity",
                why: "starts from the one selective table",
                sql: "\
SELECT u.name, o.qty, p.name
FROM users u, orders o, products p
WHERE o.user_id = u.id AND o.product_id = p.id
  AND p.sku = 'AUD-0030'",
                expect: Runs,
            },
            Example {
                label: "3-table join, already optimal",
                why: "solver finds nothing better -- and says so",
                sql: "\
SELECT u.name, p.name, o.qty
FROM orders o
JOIN users u ON u.id = o.user_id
JOIN products p ON p.id = o.product_id
ORDER BY o.id
LIMIT 10",
                expect: Runs,
            },
        ],
    },
    Group {
        name: "Refusals \u{2014} the product",
        items: &[
            Example {
                label: "CHECK violation",
                why: "the constraint is enforced, not decoration",
                sql: "\
-- users.age carries CHECK (age >= 13 AND age < 130).
INSERT INTO users (id, name, email, country, age)
VALUES (900, 'Too Young', 'young@example.com', 'NO', 7)",
                expect: Refuses,
            },
            Example {
                label: "wrong type into a column",
                why: "sqlite would store the string",
                sql: "\
-- age is int64. sqlite without STRICT stores 'forty' happily, and you find
-- out in production. mpedb refuses before the statement becomes a plan.
INSERT INTO users (id, name, email, country, age)
VALUES (901, 'Wrong Type', 'wrong@example.com', 'NO', 'forty')",
                expect: Refuses,
            },
            Example {
                label: "UNIQUE violation",
                why: "email is UNIQUE",
                sql: "\
INSERT INTO users (id, name, email, country, age)
VALUES (902, 'Duplicate', 'kari.dahl1@example.com', 'NO', 33)",
                expect: Refuses,
            },
            Example {
                label: "NOT NULL violation",
                why: "a required column left empty",
                sql: "\
INSERT INTO users (id, name, email, country, age)
VALUES (903, NULL, 'nameless@example.com', 'NO', 33)",
                expect: Refuses,
            },
            Example {
                label: "unknown column",
                why: "caught before anything runs",
                sql: "SELECT nosuchcolumn FROM users",
                expect: Refuses,
            },
        ],
    },
    Group {
        name: "Writes & plans",
        items: &[
            Example {
                label: "INSERT that succeeds",
                why: "then query products back",
                sql: "\
INSERT INTO products (id, sku, name, category, price_cents)
VALUES (500, 'NEW-0500', 'Playground cable', 'cable', 1995)",
                expect: Runs,
            },
            Example {
                label: "UPDATE with a footprint",
                why: "tables_written, read_only=false",
                sql: "UPDATE orders SET status = 'shipped' WHERE status = 'placed' AND qty > 3",
                expect: Runs,
            },
            Example {
                label: "same plan, different spelling",
                why: "keyword case and whitespace do not change the hash",
                sql: "\
-- Compile this and note the plan hash. Then reformat it -- lowercase the
-- keywords, respell count() as Count(), move the newlines around -- and run
-- it again. The hash is IDENTICAL: plans are content-addressed, which is
-- what lets execute(hash, params) parse nothing at all.
--
-- Change an IDENTIFIER's case (country -> COUNTRY) and the hash does change.
-- Identifiers and literals are case- and value-sensitive; only keyword case,
-- function-name case and whitespace are normalised away.
SELECT   country ,  count(*)
FROM users
WHERE age >= 30
GROUP BY country",
                expect: Runs,
            },
            Example {
                label: "the seed table",
                why: "what the file was born with",
                sql: "SELECT k, v FROM playground ORDER BY k",
                expect: Runs,
            },
        ],
    },
];
