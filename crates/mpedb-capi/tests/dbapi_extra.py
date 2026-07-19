# Extra DB-API 2.0 probes (companion to dbapi_battery.py) — use EXPLICIT-PK
# tables so the PK-less blocker doesn't mask other behaviors. Reveals gaps
# beyond typeless/PK-less/:params — notably DDL-in-(implicit)-transaction,
# which Python's sqlite3 triggers on any CREATE after a DML.
# Run: LD_PRELOAD=<cdylib> python3 dbapi_extra.py  (or no preload for stock)
# doesn't mask other behaviors. Reveals NEW gaps beyond typeless/PK-less/:params.
import sqlite3, sys
P=F=0; fails=[]
def ok(n,c,d=""):
    global P,F
    (globals().__setitem__('P',P+1) if c else (globals().__setitem__('F',F+1), fails.append(f"{n}: {d}")))
def trap(n,fn):
    try: fn()
    except Exception as e: ok(n, False, f"{type(e).__name__}: {e}")

c=sqlite3.connect(":memory:")
c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, val REAL)")
c.executemany("INSERT INTO t VALUES(?,?,?)", [(1,"a",1.5),(2,"b",2.5),(3,"c",3.5)])

# row_factory = sqlite3.Row (column access by name)
def rowfac():
    c.row_factory = sqlite3.Row
    r = c.execute("SELECT id,name FROM t WHERE id=1").fetchone()
    ok("row_factory.index", r[0]==1)
    ok("row_factory.byname", r["name"]=="a", dict(zip(r.keys(),r)) if hasattr(r,'keys') else r)
    c.row_factory = None
trap("row_factory", rowfac)

# cursor as iterator
def itercur():
    cur=c.execute("SELECT id FROM t ORDER BY id")
    ok("cursor.iter", [row[0] for row in cur]==[1,2,3])
trap("cursor.iter", itercur)

# arraysize + fetchmany default
def arr():
    cur=c.execute("SELECT id FROM t ORDER BY id")
    cur.arraysize=2
    ok("arraysize.fetchmany", cur.fetchmany()==[(1,),(2,)])
trap("arraysize", arr)

# connection context manager (commits on clean exit)
def ctxmgr():
    c2=sqlite3.connect(":memory:")
    c2.execute("CREATE TABLE q(id INTEGER PRIMARY KEY, v INTEGER)")
    with c2:
        c2.execute("INSERT INTO q VALUES(1, 10)")
    ok("ctxmgr.commit", c2.execute("SELECT v FROM q WHERE id=1").fetchone()==(10,))
    try:
        with c2:
            c2.execute("INSERT INTO q VALUES(2, 20)")
            raise ValueError("boom")
    except ValueError: pass
    ok("ctxmgr.rollback", c2.execute("SELECT count(*) FROM q").fetchone()[0]==1)
    c2.close()
trap("ctxmgr", ctxmgr)

# aggregate / expression column names
def colnames():
    cur=c.execute("SELECT count(*) AS n, sum(val) AS s FROM t")
    names=[d[0] for d in cur.description]
    ok("colnames.alias", names==["n","s"], names)
trap("colnames", colnames)

# text with unicode + blob
def uni():
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, s TEXT, b BLOB)")
    c.execute("INSERT INTO u VALUES(1, ?, ?)", ("héllo→", b"\x00\xff\x10"))
    r=c.execute("SELECT s,b FROM u WHERE id=1").fetchone()
    ok("unicode.text", r[0]=="héllo→", repr(r[0]))
    ok("blob.bytes", bytes(r[1])==b"\x00\xff\x10", repr(r[1]))
trap("unicode_blob", uni)

# executescript returns cursor + runs all
def escript():
    c3=sqlite3.connect(":memory:")
    c3.executescript("CREATE TABLE s(id INTEGER PRIMARY KEY, v); INSERT INTO s VALUES(1,1); INSERT INTO s VALUES(2,2);")
    ok("executescript.multi", c3.execute("SELECT count(*) FROM s").fetchone()[0]==2)
    c3.close()
trap("executescript", escript)

# OperationalError on bad SQL (not a crash)
def badsql():
    try:
        c.execute("SELECT * FROM nonexistent_table_xyz")
        ok("bad_sql.error", False, "no error")
    except sqlite3.OperationalError: ok("bad_sql.error", True)
    except sqlite3.Error as e: ok("bad_sql.error", True)  # any DB error class ok
trap("bad_sql", badsql)

print(f"EXTRA sqlite_version={sqlite3.sqlite_version} PASS={P} FAIL={F}")
for f in fails: print("  FAIL", f)
sys.exit(1 if F else 0)
