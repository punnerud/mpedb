"""Differential ANSWER probe for the mpedb libsqlite3 shim.

The suite runs (`run_suite.sh`) tell you what mpedb REFUSES. They are weak
evidence about what it answers WRONGLY: a wrong answer only shows up if some
Django test happens to assert on it. This runs a fixed statement script and
prints every result, so the two arms can be diffed directly:

    python3 probe_answers.py > stock.txt
    LD_PRELOAD=…/libmpedb_sqlite3.so python3 probe_answers.py > shim.txt
    diff stock.txt shim.txt

Reading the diff: a line where one arm answered and the other raised is a
REFUSAL (a gap, expected). A line where BOTH arms answered and the answers
differ is a WRONG ANSWER, and outranks every gap. Note that a refusal cascades —
an INSERT that was refused makes the following SELECT return `[]` in that arm
without that being an answer difference.

The script covers the surfaces the 2026-07-19 merges touched: NUMERIC/INTEGER/
REAL/TEXT/BLOB affinity on store, ordering and aggregation over mixed classes,
the int/bool bridge in every boolean position, `quote()`/`strftime()`, W2's
`FILTER` + correlated-subquery shapes, and the CAST/arithmetic/`IN` surface.
"""
import sqlite3

SCRIPT = [
    # --- W1 family: NUMERIC affinity ---------------------------------------
    "CREATE TABLE t (id integer NOT NULL PRIMARY KEY, price decimal(10, 2) NOT NULL)",
    ("INSERT INTO t (id, price) VALUES (1, ?)", ("1000",)),
    ("INSERT INTO t (id, price) VALUES (2, ?)", ("35",)),
    ("INSERT INTO t (id, price) VALUES (3, ?)", ("40.5",)),
    "SELECT id, price, typeof(price) FROM t ORDER BY id",
    ("SELECT id FROM t WHERE price < ? ORDER BY id", ("40.0",)),
    "SELECT id FROM t ORDER BY price",
    "SELECT MAX(price), MIN(price) FROM t",
    "SELECT SUM(price), AVG(price), TOTAL(price) FROM t",
    "SELECT id FROM t WHERE price = 35",
    "SELECT id FROM t WHERE price = '35'",
    "SELECT id, price + 1 FROM t ORDER BY id",
    "SELECT id FROM t WHERE price IN ('35', 1000) ORDER BY id",
    "SELECT COUNT(*), COUNT(price), SUM(id) FROM t",
    "SELECT group_concat(price) FROM t",
    # NUMERIC affinity edge cases: what converts and what stays text
    "CREATE TABLE n (id integer PRIMARY KEY, v numeric)",
    ("INSERT INTO n VALUES (1, ?)", ("abc",)),
    ("INSERT INTO n VALUES (2, ?)", ("0012",)),
    ("INSERT INTO n VALUES (3, ?)", (" 7 ",)),
    ("INSERT INTO n VALUES (4, ?)", ("1e3",)),
    ("INSERT INTO n VALUES (5, ?)", ("3.0",)),
    ("INSERT INTO n VALUES (6, ?)", ("9223372036854775808",)),
    ("INSERT INTO n VALUES (7, ?)", ("0x10",)),
    ("INSERT INTO n VALUES (8, ?)", ("",)),
    ("INSERT INTO n VALUES (9, ?)", (b"\x01\x02",)),
    "INSERT INTO n VALUES (10, 2.0)",
    ("INSERT INTO n VALUES (11, ?)", ("-4",)),
    ("INSERT INTO n VALUES (12, ?)", ("+5",)),
    ("INSERT INTO n VALUES (13, ?)", ("2.50",)),
    "SELECT id, v, typeof(v) FROM n ORDER BY id",
    "SELECT id FROM n ORDER BY v, id",
    "SELECT MAX(v), MIN(v) FROM n",
    "SELECT id FROM n WHERE v > 3 ORDER BY id",
    "SELECT id FROM n WHERE v = 7 ORDER BY id",
    # other affinities
    "CREATE TABLE a (id integer PRIMARY KEY, i integer, r real, s text, b blob)",
    ("INSERT INTO a VALUES (1,?,?,?,?)", ("12", "12", 12, 12)),
    ("INSERT INTO a VALUES (2,?,?,?,?)", ("x", "x", "x", "x")),
    ("INSERT INTO a VALUES (3,?,?,?,?)", (1.5, "1.5", 1.5, 1.5)),
    "SELECT id, typeof(i), typeof(r), typeof(s), typeof(b) FROM a ORDER BY id",
    "SELECT id, i, r, s, b FROM a ORDER BY id",
    # --- gap 5: int/bool bridge --------------------------------------------
    "CREATE TABLE bt (id integer PRIMARY KEY, flag bool NOT NULL)",
    ("INSERT INTO bt VALUES (1,?)", (True,)),
    ("INSERT INTO bt VALUES (2,?)", (False,)),
    "SELECT id, flag, typeof(flag) FROM bt ORDER BY id",
    "SELECT id FROM bt WHERE flag",
    "SELECT id FROM bt WHERE NOT flag",
    "SELECT id FROM bt WHERE flag = 1",
    "SELECT id FROM bt WHERE flag = 2",
    "SELECT id FROM bt WHERE flag + 0 = 1",
    "SELECT id FROM t WHERE id AND price",
    "SELECT id FROM t WHERE CASE WHEN id THEN 1 ELSE 0 END",
    "SELECT 1 WHERE 'abc'",
    "SELECT 1 WHERE '3abc'",
    "SELECT 1 WHERE 0.0",
    "SELECT 1 WHERE '0.0'",
    "SELECT 1 WHERE -2",
    "SELECT 1 WHERE NULL",
    # --- gap 4: quote() / strftime() ---------------------------------------
    "SELECT quote('a''b'), quote(NULL), quote(42), quote(x'0102')",
    "SELECT quote(price) FROM t ORDER BY id",
    "SELECT strftime('%Y-%m-%d', '2020-02-29')",
    "SELECT strftime('%Y %m %d %H %M %S %j %w %W', '2020-02-29 13:14:15')",
    "SELECT strftime('%s', '1970-01-02')",
    "SELECT strftime('%Y', '2020-02-29 13:14:15.678')",
    # --- W2: FILTER + correlated subquery ----------------------------------
    "CREATE TABLE b (id integer PRIMARY KEY, rating real)",
    "CREATE TABLE ba (id integer PRIMARY KEY, book_id integer)",
    "INSERT INTO b VALUES (1, 4.5), (2, 3.0)",
    "INSERT INTO ba VALUES (1, 1)",
    "SELECT COUNT(*) FILTER (WHERE EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b",
    "SELECT COUNT(*) FILTER (WHERE NOT EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b",
    "SELECT b.id, MAX(b.rating) FILTER (WHERE EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b GROUP BY b.id",
    # --- misc ----------------------------------------------------------------
    "SELECT CAST('12abc' AS integer), CAST('abc' AS integer), CAST(3.9 AS integer)",
    "SELECT length('abc'), substr('abcdef',2,3), upper('ab'), abs(-3), round(2.567,2)",
    "SELECT coalesce(NULL, 3), nullif(1,1), ifnull(NULL,'x')",
    "SELECT 7/2, 7%2, -7/2, 7.0/2",
]


def main():
    con = sqlite3.connect(":memory:")
    con.isolation_level = None
    c = con.cursor()
    for item in SCRIPT:
        sql, params = (item, ()) if isinstance(item, str) else item
        try:
            c.execute(sql, params)
            rows = c.fetchall()
            print(f"{sql} | {params!r}\n    -> {rows!r}")
        except Exception as exc:  # noqa: BLE001
            print(f"{sql} | {params!r}\n    !! {type(exc).__name__}: {exc}")


main()
