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

Run 4 added the surfaces of the post-run-3 merges: typeless (`any`) PRIMARY
KEY / index keys (including mixed-storage-class content under an index),
subqueries in UPDATE/DELETE WHERE, correlated `IN`, correlated per-row
aggregate positions, bitwise operators, bound REGEXP (a `regexp` UDF is
registered in both arms, as Django does), scalar `max(a,b)`/`min(a,b)`,
int<->float parameter bridging, the JSON function set, and `typeof()`'s
five-storage-class contract.
"""
import re as _re
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
    # --- run 4: typeless (`any`) PRIMARY KEY / index keys --------------------
    "CREATE TABLE dtpk (d datetime NOT NULL PRIMARY KEY)",
    ("INSERT INTO dtpk VALUES (?)", ("2026-07-19 12:00:00",)),
    ("INSERT INTO dtpk VALUES (?)", ("2025-01-01 00:00:00",)),
    "SELECT d FROM dtpk ORDER BY d",
    ("SELECT d FROM dtpk WHERE d = ?", ("2025-01-01 00:00:00",)),
    ("SELECT d FROM dtpk WHERE d > ? ORDER BY d", ("2025-06-01 00:00:00",)),
    # mixed storage classes UNDER an index: sqlite orders NULL<INTEGER/REAL<TEXT<BLOB
    "CREATE TABLE anyix (id integer NOT NULL PRIMARY KEY, v numeric NULL)",
    "CREATE INDEX anyix_v ON anyix (v)",
    ("INSERT INTO anyix VALUES (1, ?)", ("txt",)),
    "INSERT INTO anyix VALUES (2, 5)",
    "INSERT INTO anyix VALUES (3, 4.5)",
    ("INSERT INTO anyix VALUES (4, ?)", (b"\x00",)),
    "INSERT INTO anyix VALUES (5, NULL)",
    ("INSERT INTO anyix VALUES (6, ?)", ("10",)),  # affinity: stores integer 10
    "SELECT id, v, typeof(v) FROM anyix ORDER BY v, id",
    "SELECT id FROM anyix WHERE v = 10",
    "SELECT id FROM anyix WHERE v > 4 ORDER BY id",
    "SELECT MIN(v), MAX(v) FROM anyix",
    # --- run 4: subqueries in UPDATE/DELETE WHERE, correlated IN -------------
    "CREATE TABLE p (id integer NOT NULL PRIMARY KEY, gid integer NOT NULL)",
    "CREATE TABLE g (id integer NOT NULL PRIMARY KEY, ok bool NOT NULL)",
    "INSERT INTO p VALUES (1,1),(2,2),(3,1)",
    "INSERT INTO g VALUES (1,1),(2,0)",
    "UPDATE p SET gid = gid + 10 WHERE gid IN (SELECT id FROM g WHERE ok)",
    "SELECT id, gid FROM p ORDER BY id",
    "DELETE FROM p WHERE EXISTS (SELECT 1 FROM g WHERE g.id = p.gid AND NOT g.ok)",
    "SELECT id, gid FROM p ORDER BY id",
    # correlated IN
    "SELECT id FROM p WHERE id IN (SELECT g.id FROM g WHERE g.id = p.gid - 10) ORDER BY id",
    # correlated subquery in a per-row aggregate position
    "SELECT COUNT(*), SUM((SELECT g.id FROM g WHERE g.id = p.gid - 10)) FROM p",
    # --- run 4: bitwise operators -------------------------------------------
    "SELECT 5 | 2, 5 & 3, 1 << 4, 256 >> 3, ~5, -1 >> 60, 1 << 62",
    "SELECT 5 | NULL, NULL & 3, ~NULL",
    "SELECT '5' | '2', 4.9 & 7",
    # --- run 4: scalar max/min ----------------------------------------------
    "SELECT max(1, 2), min(3, 1, 2), max(1, NULL), max('a', 'b'), max(1, 2.5)",
    # --- run 4: int<->float parameter bridge --------------------------------
    "CREATE TABLE fr (id integer NOT NULL PRIMARY KEY, ratio real NOT NULL)",
    ("INSERT INTO fr VALUES (1, ?)", (1,)),          # int bound to real column
    ("INSERT INTO fr VALUES (2, ?)", (0.5,)),
    ("SELECT id FROM fr WHERE ratio = ?", (1,)),      # int param vs real column
    ("SELECT id FROM fr WHERE id = ?", (1.0,)),       # exact float vs int column
    "SELECT id, ratio, typeof(ratio) FROM fr ORDER BY id",
    # --- run 4: the JSON function set ----------------------------------------
    """SELECT json('{"a": 1}'), json_valid('{"a": 1}'), json_valid('{a: 1}'), json_valid('not json')""",
    """SELECT json_type('{"a": [1, 2]}'), json_type('{"a": [1, 2]}', '$.a'), json_quote('it''s')""",
    """SELECT json_array_length('[1, 2, 3]'), json_extract('{"a": {"b": 7}}', '$.a.b')""",
    """SELECT '{"a": 2}' -> 'a', '{"a": 2}' ->> 'a', '[1, 2]' ->> 1""",
    """SELECT json_array(1, 'a', NULL), json_object('k', 1)""",
    """SELECT json_set('{"a": 1}', '$.b', 2), json_remove('{"a": 1, "b": 2}', '$.b')""",
    """SELECT json_replace('{"a": 1}', '$.a', 9), json_insert('{"a": 1}', '$.a', 9)""",
    """SELECT json_patch('{"a": 1}', '{"b": 2}')""",
    # Django's JSONField CHECK shape
    "CREATE TABLE jf (id integer NOT NULL PRIMARY KEY, data text NULL CHECK ((JSON_VALID(data) OR data IS NULL)))",
    ("INSERT INTO jf VALUES (1, ?)", ('{"k": "v"}',)),
    ("INSERT INTO jf VALUES (2, ?)", (None,)),
    "SELECT id, data ->> 'k' FROM jf ORDER BY id",
    # --- run 4: typeof() five storage classes -------------------------------
    "SELECT typeof(NULL), typeof(1), typeof(1.5), typeof('x'), typeof(x'00')",
    "SELECT flag, typeof(flag) FROM bt ORDER BY id",
    # --- run 4: bound REGEXP (via the regexp UDF both arms register) ---------
    ("SELECT 1 WHERE 'abcd' REGEXP ?", ("bc",)),
    ("SELECT 1 WHERE 'abcd' REGEXP ?", ("^bc",)),
    ("SELECT 1 WHERE 'ABCD' REGEXP ?", ("bc",)),
    # W3 (CLOSED 2026-07-20, #108): mpedb's engine used to intercept REGEXP
    # with its own dialect instead of calling the consumer's registered
    # regexp() UDF, and a pattern outside that dialect matched NOTHING —
    # inline flags and backreferences are valid Python/PCRE patterns Django
    # relies on (`__iregex` prepends `(?i)`), so these two lines were silent
    # wrong answers. The operator now dispatches to the registered host UDF
    # (sqlite's contract), so both lines answer [(1,)] in both arms.
    ("SELECT 1 WHERE 'hey-Foo' REGEXP ?", ("(?i)fo+",)),
    ("SELECT 1 WHERE 'barfoobaz' REGEXP ?", (r"b(.).*b\1",)),
    "SELECT 1 WHERE 'a%b' LIKE 'a\\%b' ESCAPE '\\'",
    # --- LIKE half of #74 item 3: BOUND patterns (run 4's rank-1 gap). ------
    # Django's exact wire shapes for startswith/contains/endswith/icontains —
    # the pattern is always bound and the escape is always the literal '\'.
    ("SELECT 1 WHERE 'A_b' LIKE ? ESCAPE '\\'", (r"A\_b",)),
    ("SELECT 1 WHERE 'Axb' LIKE ? ESCAPE '\\'", (r"A\_b",)),
    ("SELECT 1 WHERE 'xxfooyy' LIKE ? ESCAPE '\\'", ("%foo%",)),
    ("SELECT 1 WHERE 'xx%fooyy' LIKE ? ESCAPE '\\'", (r"%\%foo%",)),
    ("SELECT 1 WHERE '100%' LIKE ? ESCAPE '\\'", (r"100\%",)),
    ("SELECT 1 WHERE 'FOOBAR' LIKE ? ESCAPE '\\'", ("foo%",)),
    ("SELECT 1 WHERE 'abc' NOT LIKE ? ESCAPE '\\'", ("z%",)),
    # NULL pattern: 3VL — the WHERE is not TRUE, both arms answer [].
    ("SELECT 1 WHERE 'a' LIKE ?", (None,)),
    ("SELECT 1 WHERE 'a' NOT LIKE ?", (None,)),
    # A BLOB pattern is BUILD-DEPENDENT in sqlite (SQLITE_LIKE_DOESNT_MATCH_
    # BLOBS: Debian's libsqlite3 answers [], stock amalgamation coerces the
    # bytes as text). mpedb REFUSES the bind by name ('statement requires
    # text') — a refusal line in this diff, never a wrong answer in either
    # world.
    ("SELECT 1 WHERE 'ab' LIKE ?", (b"ab",)),
    # GLOB had the same literal-only restriction; closed in the same style.
    ("SELECT 1 WHERE 'abcd' GLOB ?", ("ab*",)),
    ("SELECT 1 WHERE 'ABCD' GLOB ?", ("ab*",)),
    # --- run 5: the date/time family (#112 B) -------------------------------
    "SELECT date('2020-02-29 13:14:15'), time('2020-02-29 13:14:15')",
    "SELECT datetime('2020-02-29 13:14:15'), julianday('2020-02-29 13:14:15')",
    "SELECT date('1970-01-01'), julianday('1970-01-01'), datetime('1970-01-01')",
    "SELECT date('2020-02-29'), time('2020-02-29'), datetime('2020-02-29')",
    "SELECT typeof(julianday('2020-01-01')), typeof(date('2020-01-01'))",
    "SELECT date(NULL), time(NULL), datetime(NULL), julianday(NULL)",
    # `'now'` is a CLOCK read: its VALUE cannot be diffed, but its SHAPE, its
    # self-consistency inside one statement and its agreement with the other
    # members of the family can. Any of these differing is a real divergence.
    "SELECT length(date('now')), length(time('now')), length(datetime('now'))",
    "SELECT date('now') = date('now'), datetime('now') = datetime('now')",
    "SELECT date('now') = substr(datetime('now'), 1, 10)",
    "SELECT strftime('%Y-%m-%d', 'now') = date('now')",
    "SELECT typeof(julianday('now')), julianday('now') > 2451545.0",
    "SELECT strftime('%s', date('now')) = strftime('%s', date('now'))",
    # --- run 5: derived-table materialization + aggregate over a group body --
    "CREATE TABLE dt (id integer PRIMARY KEY, k text NOT NULL, v integer NOT NULL)",
    "INSERT INTO dt VALUES (1,'a',1),(2,'a',2),(3,'b',5),(4,'c',7),(5,'b',5)",
    "SELECT COUNT(*) FROM (SELECT k, SUM(v) AS s FROM dt GROUP BY k) u",
    "SELECT AVG(s) FROM (SELECT k, SUM(v) AS s FROM dt GROUP BY k) u",
    "SELECT MAX(s), MIN(s) FROM (SELECT k, SUM(v) AS s FROM dt GROUP BY k) u",
    "SELECT u.k, u.s FROM (SELECT k, SUM(v) AS s FROM dt GROUP BY k) u ORDER BY u.k",
    "SELECT COUNT(*) FROM (SELECT DISTINCT k FROM dt) u",
    "SELECT SUM(c) FROM (SELECT k, COUNT(*) AS c FROM dt GROUP BY k HAVING COUNT(*) > 1) u",
    "SELECT u.s FROM (SELECT k, SUM(v) AS s FROM dt GROUP BY k) u WHERE u.s > 2 ORDER BY u.s",
    # A derived body whose column is RENAMED, and one that projects an
    # expression — run 4's rank-2 family.
    "SELECT z.n FROM (SELECT dt.id AS n FROM dt) z ORDER BY z.n",
    "SELECT z.n FROM (SELECT dt.v * 2 AS n FROM dt WHERE dt.k = 'b') z ORDER BY z.n",
    # --- run 5: tokenizer one-offs (`==`, comments at every position) --------
    "SELECT 1 WHERE 1 == 1",
    "SELECT 1 WHERE 'a' == 'a'",
    "SELECT /* c */ 1 -- trailing\n",
    "SELECT 1 -- comment\nWHERE 2 == 2",
]


def _regexp(pattern, string):
    # Django's `_sqlite_regexp`, verbatim in behaviour.
    if pattern is None or string is None:
        return None
    if not isinstance(string, str):
        string = str(string)
    return bool(_re.search(pattern, string))


def main():
    con = sqlite3.connect(":memory:")
    con.isolation_level = None
    con.create_function("regexp", 2, _regexp, deterministic=True)
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
