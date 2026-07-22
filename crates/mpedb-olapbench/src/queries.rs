//! The queries, in ONE dialect, run verbatim against every engine.
//!
//! No per-engine rewriting. If an engine cannot run a query as written, that is
//! reported as a refusal rather than papered over with a dialect shim — a
//! benchmark that quietly gives each engine a different query is comparing
//! translations. (The one thing normalised is result *formatting*, never the
//! statement.)
//!
//! Every query is also a claim about WHICH machinery it exercises, and the
//! `probes` field says which. That is the point of the whole file: a single
//! "mpedb is 40x slower at OLAP" number teaches nothing, while "40x slower on
//! the scan, 900x faster on the indexed extremum, and it picks the same join
//! order" is a description of two architectures.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probes {
    /// Raw scan-and-aggregate throughput. A vectorised column store should win
    /// this outright, and by a lot — it is the workload it exists for.
    Scan,
    /// Aggregates a precomputed access path can answer without scanning:
    /// count from index entry counts, min/max from a boundary probe.
    Precompute,
    /// Hash/grouping machinery, where cardinality decides the winner.
    GroupBy,
    /// Join ordering. The FROM clause is deliberately written in a BAD order,
    /// so an engine that respects textual order pays for it and an engine that
    /// reorders does not. This is what MPEE is for.
    JoinOrder,
    /// The prepared, parameterised path — mpedb's `execute(hash, params)` with
    /// no parsing at all, against everyone else's prepared statement.
    Prepared,
}

pub struct Query {
    pub name: &'static str,
    pub probes: Probes,
    pub sql: &'static str,
    /// What the query is meant to demonstrate, printed in the report so a
    /// reader does not have to infer intent from SQL.
    pub about: &'static str,
}

pub const QUERIES: &[Query] = &[
    // ---------------------------------------------------------------- scan
    Query {
        name: "scan-sum",
        probes: Probes::Scan,
        sql: "SELECT sum(amount) FROM fact",
        about: "One column, every row. The column store's home turf: it reads \
                one column's worth of bytes where a row store reads whole rows.",
    },
    Query {
        name: "scan-filter-sum",
        probes: Probes::Scan,
        sql: "SELECT sum(amount) FROM fact WHERE qty > 10",
        about: "Same scan with a predicate that keeps about half the rows — \
                measures filter throughput, not selectivity handling.",
    },
    Query {
        name: "scan-multi-agg",
        probes: Probes::Scan,
        sql: "SELECT count(*), sum(qty), sum(amount), avg(amount) FROM fact",
        about: "Four accumulators in one pass. mpedb folds them through a \
                single decode; a column store touches two columns.",
    },
    // ----------------------------------------------------------- precompute
    Query {
        name: "count-star",
        probes: Probes::Precompute,
        sql: "SELECT count(*) FROM fact",
        about: "mpedb answers from index entry counts — whole leaves at a \
                time, no key ever read (PLAN_FORMAT 59). Most engines keep \
                metadata for this too, which is exactly why it is worth \
                measuring rather than assuming.",
    },
    Query {
        name: "min-max-indexed",
        probes: Probes::Precompute,
        sql: "SELECT min(amount), max(amount) FROM fact",
        about: "With an index on amount this is two O(log n) boundary probes \
                in mpedb. A column store with zone maps can also skip most \
                blocks — the question is whether skipping beats descending.",
    },
    Query {
        name: "count-filtered",
        probes: Probes::Precompute,
        sql: "SELECT count(*) FROM fact WHERE product_id = 42",
        about: "Counting a range of one index rather than the whole tree.",
    },
    // ------------------------------------------------------------- group by
    Query {
        name: "group-small",
        probes: Probes::GroupBy,
        sql: "SELECT store_id, count(*), sum(amount) FROM fact GROUP BY store_id",
        about: "200 groups — the accumulator set stays in cache.",
    },
    Query {
        name: "group-large",
        probes: Probes::GroupBy,
        sql: "SELECT customer_id, count(*), sum(amount) FROM fact GROUP BY customer_id",
        about: "20,000 groups with a skewed head. Cache behaviour of the hash \
                table starts to dominate.",
    },
    // ----------------------------------------------------------- join order
    Query {
        name: "join-star-2",
        probes: Probes::JoinOrder,
        sql: "SELECT p.category, sum(f.amount) \
              FROM fact f, product p \
              WHERE f.product_id = p.id AND p.category = 'tools' \
              GROUP BY p.category",
        about: "Fact joined to one dimension with a selective dimension \
                filter. The good plan enters the dimension first.",
    },
    Query {
        name: "join-star-4",
        probes: Probes::JoinOrder,
        sql: "SELECT c.nation_segment, p.category, sum(f.amount) \
              FROM fact f, customer c, product p, day d \
              WHERE f.customer_id = c.id AND f.product_id = p.id AND f.day_id = d.id \
                AND d.year = 2023 AND p.category = 'tools' \
              GROUP BY c.nation_segment, p.category",
        about: "The classic star: three dimensions, two of them filtered. \
                Every engine here has to decide an order.",
    },
    Query {
        name: "join-bad-order",
        probes: Probes::JoinOrder,
        sql: "SELECT sum(f.amount) \
              FROM fact f, store s, customer c, product p, day d \
              WHERE f.store_id = s.id AND f.customer_id = c.id AND f.product_id = p.id \
                AND f.day_id = d.id AND s.nation = 'NORWAY' AND d.year = 2023 \
              GROUP BY s.nation",
        about: "Five tables with the fact FIRST in the FROM clause — the worst \
                textual order there is. Run this with MPEDB_NO_MPEE=1 to see \
                what the solver is worth.",
    },
    // ------------------------------------------------------------- prepared
    Query {
        name: "prepared-point",
        probes: Probes::Prepared,
        sql: "SELECT amount FROM fact WHERE id = ?",
        about: "The OLTP shape inside an OLAP dataset, run thousands of times: \
                mpedb executes a content-hashed plan with no parsing at all.",
    },
    Query {
        name: "prepared-range",
        probes: Probes::Prepared,
        sql: "SELECT sum(amount) FROM fact WHERE customer_id = ?",
        about: "A small aggregate per parameter — the shape a dashboard runs \
                once per widget per refresh.",
    },
];
