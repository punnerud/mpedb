"""complexq-gen.py — shared query/DDL generators for the complex-query cell
(benchmarks/routing.md, "Mot PostgreSQL"). Imported by complexq-mpedb.py and
complexq-pg.py so both engines get byte-identical SQL.

Shape: NT small tables t1..t17, each (id int64 PK, a int64), 1000 rows,
a = id, joined as a 1:1 FK chain  t(k).a = t(k+1).id.
"""

NT = 17          # tables materialized in both engines
ROWS = 1000      # rows per table


def chain_sql(n: int) -> str:
    """N-table inner-join chain, count(*) so the result set is one row."""
    q = "SELECT count(*) FROM t1"
    for k in range(2, n + 1):
        q += f" JOIN t{k} ON t{k-1}.a = t{k}.id"
    return q


def point_chain_sql(n: int) -> str:
    """Same chain, but anchored to ONE outer row — execution is n point
    lookups (~µs), so per-call overhead is visible instead of drowned."""
    return chain_sql(n) + " WHERE t1.id = 500"


def self_chain_sql(n_aliases: int) -> str:
    """N-alias self-join on t1 (aliases are how we probe the join-count cap
    without creating 65 tables; self-joins are legal since #44)."""
    q = "SELECT count(*) FROM t1 a1"
    for k in range(2, n_aliases + 1):
        q += f" JOIN t1 a{k} ON a{k-1}.a = a{k}.id"
    return q


def uncorrelated_sql(depth: int) -> str:
    """depth nested UNcorrelated IN-subqueries; innermost filters a < 500."""
    inner = f"SELECT id FROM t{depth} WHERE a < 500"
    for k in range(depth - 1, 1, -1):
        inner = f"SELECT id FROM t{k} WHERE a IN ({inner})"
    return f"SELECT count(*) FROM t1 WHERE a IN ({inner})"


def correlated_sql(depth: int) -> str:
    """depth nested CORRELATED EXISTS-subqueries; innermost filters a < 500."""
    q = f"SELECT 1 FROM t{depth} WHERE t{depth}.id = t{depth-1}.a AND t{depth}.a < 500"
    for k in range(depth - 1, 1, -1):
        q = f"SELECT 1 FROM t{k} WHERE t{k}.id = t{k-1}.a AND EXISTS ({q})"
    return f"SELECT count(*) FROM t1 WHERE EXISTS ({q})"


def deep_in_sql(depth: int) -> str:
    """IN-nesting of arbitrary depth over t1 alone — the subquery-cap probe."""
    inner = "SELECT id FROM t1 WHERE a < 500"
    for _ in range(depth - 1):
        inner = f"SELECT id FROM t1 WHERE a IN ({inner})"
    return f"SELECT count(*) FROM t1 WHERE a IN ({inner})"


# Named limit probes (claim 4). Expected answers live in benchmarks/routing.md.
LIMIT_PROBES = {
    "chain-17": chain_sql(17),
    "self-64": self_chain_sql(64),
    "self-65": self_chain_sql(65),
    "right-mid": ("SELECT count(*) FROM t1 JOIN t2 ON t1.a = t2.id "
                  "RIGHT JOIN t3 ON t2.a = t3.id"),
    "right-leading": "SELECT count(*) FROM t1 RIGHT JOIN t2 ON t1.a = t2.id",
    "full-mid": ("SELECT count(*) FROM t1 JOIN t2 ON t1.a = t2.id "
                 "FULL JOIN t3 ON t2.a = t3.id"),
    "full-after-right": ("SELECT count(*) FROM t1 RIGHT JOIN t2 ON t1.a = t2.id "
                         "FULL JOIN t3 ON t2.a = t3.id"),
    "in-depth-17": deep_in_sql(17),
}
