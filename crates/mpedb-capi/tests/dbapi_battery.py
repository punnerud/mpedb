# DB-API 2.0 compliance battery for the mpedb libsqlite3 shim.
# Run against the shim:  LD_PRELOAD=<cdylib> python3 dbapi_battery.py
# Run against stock sqlite3 for a baseline:  python3 dbapi_battery.py
# Exits non-zero if any check fails; prints per-check failures.
# Runs whatever libsqlite3 is loaded (stock, or the mpedb shim via LD_PRELOAD).
import sqlite3, sys
P=F=0; fails=[]
def ok(name, cond, detail=""):
    global P,F
    if cond: P+=1
    else: F+=1; fails.append(f"{name}: {detail}")
def trap(name, fn):
    try: fn()
    except Exception as e: ok(name, False, f"raised {type(e).__name__}: {e}")

# --- module ---
ok("module.version", hasattr(sqlite3,"sqlite_version"), )
ok("module.Error", issubclass(sqlite3.IntegrityError, sqlite3.Error))
ok("module.paramstyle", sqlite3.paramstyle=="qmark", sqlite3.paramstyle)

c=sqlite3.connect(":memory:")
cur=c.cursor()

# --- DDL + basic CRUD ---
def ddl():
    cur.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, val REAL, data BLOB)")
trap("ddl.create", ddl)
def crud():
    cur.execute("INSERT INTO t(name,val) VALUES(?,?)", ("alice", 1.5))
    ok("cursor.lastrowid", cur.lastrowid==1, cur.lastrowid)
    ok("cursor.rowcount.insert", cur.rowcount==1, cur.rowcount)
trap("crud.insert", crud)

# --- executemany ---
def em():
    cur.executemany("INSERT INTO t(name,val) VALUES(?,?)", [("bob",2.0),("carol",3.0)])
    ok("executemany.rowcount", cur.rowcount==2, cur.rowcount)
trap("executemany", em)

# --- fetch variants ---
def fetches():
    cur.execute("SELECT id,name FROM t ORDER BY id")
    ok("fetchone", cur.fetchone()==(1,"alice"))
    ok("fetchmany", cur.fetchmany(1)==[(2,"bob")])
    ok("fetchall", cur.fetchall()==[(3,"carol")])
trap("fetch", fetches)

# --- description ---
def desc():
    cur.execute("SELECT id, name FROM t")
    d=cur.description
    ok("description.len", len(d)==2, d)
    ok("description.names", d[0][0]=="id" and d[1][0]=="name", [x[0] for x in d])
trap("description", desc)

# --- type round-trip ---
def types():
    cur.execute("DELETE FROM t")
    cur.execute("INSERT INTO t(id,name,val,data) VALUES(?,?,?,?)", (10,"x",2.25,b"\x00\x01\xff"))
    r=cur.execute("SELECT id,name,val,data FROM t WHERE id=10").fetchone()
    ok("type.int", r[0]==10 and isinstance(r[0],int))
    ok("type.text", r[1]=="x" and isinstance(r[1],str))
    ok("type.real", abs(r[2]-2.25)<1e-9 and isinstance(r[2],float))
    ok("type.blob", bytes(r[3])==b"\x00\x01\xff", repr(r[3]))
    r2=cur.execute("SELECT val FROM t WHERE name IS NULL").fetchall()
    ok("type.null_query", r2==[], r2)
trap("types", types)

# --- named params ---
def named():
    cur.execute("SELECT id FROM t WHERE name=:n", {"n":"x"})
    ok("params.named", cur.fetchone()==(10,))
trap("params.named", named)

# --- transactions ---
def txn():
    c2=sqlite3.connect(":memory:")
    c2.execute("CREATE TABLE q(a)")
    c2.execute("INSERT INTO q VALUES(1)")
    c2.rollback()
    n=c2.execute("SELECT count(*) FROM q").fetchone()[0]
    ok("txn.rollback", n==0, n)
    c2.execute("INSERT INTO q VALUES(2)"); c2.commit()
    c2.execute("INSERT INTO q VALUES(3)"); c2.rollback()
    n=c2.execute("SELECT count(*) FROM q").fetchone()[0]
    ok("txn.commit_then_rollback", n==1, n)
    c2.close()
trap("txn", txn)

# --- executescript ---
def escript():
    c3=sqlite3.connect(":memory:")
    c3.executescript("CREATE TABLE s(a); INSERT INTO s VALUES(1); INSERT INTO s VALUES(2);")
    ok("executescript", c3.execute("SELECT count(*) FROM s").fetchone()[0]==2)
    c3.close()
trap("executescript", escript)

# --- total_changes / connection.execute shortcut ---
def misc():
    c4=sqlite3.connect(":memory:")
    c4.execute("CREATE TABLE m(a)")
    c4.execute("INSERT INTO m VALUES(1)")
    ok("connection.execute_shortcut", c4.execute("SELECT a FROM m").fetchone()==(1,))
    ok("total_changes", c4.total_changes>=1, c4.total_changes)
    c4.close()
trap("misc", misc)

# --- integrity error surfaces ---
def integ():
    c5=sqlite3.connect(":memory:")
    c5.execute("CREATE TABLE u(id INTEGER PRIMARY KEY)")
    c5.execute("INSERT INTO u VALUES(1)")
    try:
        c5.execute("INSERT INTO u VALUES(1)")
        ok("integrity_error", False, "no error on dup PK")
    except sqlite3.IntegrityError:
        ok("integrity_error", True)
    except sqlite3.Error as e:
        ok("integrity_error", False, f"wrong class {type(e).__name__}")
    c5.close()
trap("integrity", integ)

print(f"sqlite_version={sqlite3.sqlite_version}  PASS={P} FAIL={F}")
for f in fails: print("  FAIL", f)
sys.exit(1 if F else 0)
