# Using mpedb

A practical guide: getting a database open, writing queries, and moving data in
from sqlite3.

**Every Rust snippet here is compiled and run** by
[`crates/mpedb/tests/guide.rs`](crates/mpedb/tests/guide.rs), and every shell
transcript is pasted from a real run. Documentation that is not executed rots
quietly — it keeps working in the reader's head and nowhere else. (This project
has already shipped a README describing a surface the binary did not have, which
is why the rule exists.)

If you want to know *why* mpedb exists, read the [README](README.md). This is the
how.

---

## Contents

- [Quickstart](#quickstart)
- [The config file is the schema](#the-config-file-is-the-schema)
- [What the schema buys you](#what-the-schema-buys-you)
- [Querying](#querying)
- [Transactions](#transactions)
- [Upsert](#upsert)
- [Reading the plan](#reading-the-plan)
- [Aggregates and joins](#aggregates-and-joins)
- [Durability: pick one on purpose](#durability-pick-one-on-purpose)
- [Coming from sqlite3](#coming-from-sqlite3)
- [Migrating a sqlite3 database](#migrating-a-sqlite3-database)
- [Many processes](#many-processes)
- [The CLI](#the-cli)
- [Python](#python)

---

## Quickstart

```toml
# app.toml
[database]
path = "app.mpedb"
size_mb = 64
max_readers = 128
durability = "wal"

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
  nullable = true
  check = "age >= 0 AND age < 150"

  # Composite secondary indexes (#55): a column LIST, declaration order =
  # key order; `unique = true` enforces uniqueness over the SET (NULLs never
  # conflict). The planner uses full-width equality, any equality prefix,
  # and ranges on the first column.
  [[table.index]]
  columns = ["age", "email"]
```

```rust
use mpedb::{params, Config, Database, ExecResult};

let db = Database::open(std::path::Path::new("app.toml"))?;

// Write.
db.query(
    "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
    &params![1, "ada@example.com", 36],
)?;

// Read.
let r = db.query("SELECT email, age FROM users WHERE id = $1", &params![1])?;

// The hot path: compile once, execute by hash forever.
let h = db.prepare("SELECT email FROM users WHERE id = $1")?;
let r = db.execute(&h, &params![1])?;
```

**Use `$n` parameters, never string interpolation.** This is not only about
injection: every distinct SQL *text* compiles to a distinct plan, so
`format!("… WHERE id = {id}")` mints one plan per query and floods the shared
registry. `$1` gives you one plan for all of them.

There is no server to start, no connection string, and no daemon running while
you are not using it. `Database::open` mmaps a file.

## The config file is the schema

`type` is one of `int64`, `float64`, `text`, `blob`, `bool`, `timestamp` — or
`any`, the deliberate per-column opt-out that accepts any scalar (sqlite-style
flexibility where you asked for it; an `any` column cannot be a key or UNIQUE).
Per column: `nullable` (default true), `unique` (an enforced UNIQUE index),
`indexed` (a non-unique lookup index — the planner uses it for `WHERE col = …`
and ranges), `check` (an SQL expression over the row), `default`.

The schema lives **inside the file**. The config is how you create it the first
time; after that the file is authoritative, and attaching with a config that
disagrees is a hard error rather than a silent migration. That is deliberate:
schema drift you find out about at startup beats schema drift you find out about
from a customer.

Two consequences worth knowing up front:

- **There is no `CREATE TABLE` or `ALTER TABLE`.** A table's id is its index in
  the name-sorted table list, and that id keys the catalog's B+tree roots, the
  change-capture bitmap, and the mirror's per-table state. Adding `accounts` to a
  database holding `orders` and `users` would renumber both and point `accounts`
  at `orders`' rows. Schema change is a *rebuild* (`mpedb mirror regenerate`).
- **A `.mpedb` is one self-describing file.** `cp` is a complete snapshot:

  ```sh
  cp app.mpedb app.snap    # snapshot
  pytest                   # let the suite do its worst
  cp app.snap app.mpedb    # roll back, instantly
  ```

  Copy while no process is attached and writing — a live mmapped file can be
  caught mid-commit, exactly as with sqlite. In `wal` durability the `-wal`
  sidecar is part of the database: copy both, or neither.

## What the schema buys you

Each of these is an error at the moment you write the bad row, and each is
something sqlite3 (without `STRICT`) accepts:

```rust
// A string in an integer column.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
         &params![2, "b@example.com", "not a number"]).unwrap_err();

// NOT NULL.
db.query("INSERT INTO users (id, age) VALUES ($1, $2)", &params![3, 20]).unwrap_err();

// UNIQUE.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
         &params![4, "ada@example.com", 20]).unwrap_err();

// CHECK.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
         &params![5, "c@example.com", 200]).unwrap_err();
```

Every one of those leaves the table exactly as it was. A failed statement is a
failed statement, not a half-applied one.

## Querying

`query` takes SQL text; `prepare` + `execute` takes it once and never parses
again. Both return `ExecResult`:

```rust
pub enum ExecResult {
    Rows { columns: Vec<String>, rows: Vec<Vec<Value>> },
    Affected(u64),
    Explain(String),
}
```

The supported surface is deliberately narrow, and
[the README's table](README.md#sql-support) is the exact list, measured against
the binary. In short: `SELECT`/`INSERT`/`UPDATE`/`DELETE`, `WHERE`, `ORDER BY`,
`LIMIT`/`OFFSET`, `GROUP BY`/`HAVING`, `DISTINCT`, aggregates, N-way
`INNER JOIN` chains (aliases and self-joins included), `LEFT JOIN`,
`ON CONFLICT`, `RETURNING`, `IN`/`BETWEEN`/`CASE`/`LIKE`, and a
handful of scalar functions.

## Transactions

```rust
let mut tx = db.begin()?;
tx.query("INSERT INTO users (id, email) VALUES ($1, $2)", &params![1, "a@example.com"])?;
tx.query("INSERT INTO users (id, email) VALUES ($1, $2)", &params![2, "b@example.com"])?;
tx.commit()?;
```

Dropping a session without committing rolls it back. So does `tx.rollback()`,
which says it out loud.

A transaction spanning several tables commits atomically — one writer lock, one
meta flip. Single statements are their own transaction.

## Upsert

```rust
// On the primary key.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3) \
          ON CONFLICT (id) DO UPDATE SET age = excluded.age",
         &params![1, "ada@example.com", 37])?;

// On a UNIQUE column. The proposed id 99 never enters — the row that owns the
// email is the one updated, and RETURNING reports which row that was.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3) \
          ON CONFLICT (email) DO UPDATE SET age = users.age + 1 RETURNING id, age",
         &params![99, "ada@example.com", 0])?;   // -> id 1, age 38

// DO NOTHING.
db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
         &params![1, "z@example.com", 1])?;
```

`excluded.<col>` is the row you proposed; `users.<col>` is the row already there.

The conflict target must be a key that can be **probed**: the primary key, or one
`UNIQUE` column. And only the conflict you *named* is handled — if you say
`ON CONFLICT (email)` and it is the primary key that collides, that is an error,
not a silent upsert of some other row. PostgreSQL does the same.

## Reading the plan

```sql
EXPLAIN SELECT email FROM users WHERE id = $1
```

```text
Select users
  access: PkPoint(id = $1)
  project: email
  footprint: read_only=true tables_read=0x1 tables_written=0x0 indexes_used=0x1 key=Point
```

`access` is the thing to look at: `PkPoint` is a point lookup, `PkRange` a range
scan, `IndexPoint` a unique-index probe, `IndexScan`/`IndexRange` equality and
range over a non-unique index, `FullScan` everything. The `footprint` is what
the engine claims before running — which tables, which keys — and it is how the
commit path decides whether two statements conflict.

Plans do not lie by omission on purpose: a join says whether it is an index
nested loop (the `ON` equality was consumed into the inner fetch) or a held
full scan with its honest `O(n*m)` label, and sort-only columns print as
`sort-only:` rather than as output.

## Aggregates and joins

```rust
db.query("SELECT count(*), sum(qty), avg(qty) FROM items", &[])?;

db.query("SELECT oid, count(*) FROM items GROUP BY oid HAVING count(*) > 1 ORDER BY oid", &[])?;

db.query("SELECT orders.customer, sum(items.qty) FROM items \
          JOIN orders ON items.oid = orders.oid \
          GROUP BY orders.customer ORDER BY orders.customer", &[])?;

db.query("SELECT DISTINCT oid FROM items ORDER BY oid", &[])?;

// LEFT JOIN: the order with no items comes back NULL-extended, not dropped.
db.query("SELECT orders.customer, items.qty FROM orders \
          LEFT JOIN items ON items.oid = orders.oid \
          WHERE orders.oid = 3", &[])?;

// A chain with aliases — the third table is the same table again (self-join).
db.query("SELECT a.iid, b.iid FROM items a \
          JOIN orders o ON a.oid = o.oid \
          JOIN items b ON b.oid = o.oid AND b.iid > a.iid", &[])?;
```

The NULL rules match sqlite3, and they are the ones people state confidently and
get wrong:

| | |
|---|---|
| `SUM` over zero rows | **NULL**, not 0 |
| `COUNT` over zero rows | **0**, not NULL |
| `SUM(x)` where all x are NULL | **NULL** — never seeing a value is not summing nothing |
| `AVG(x)` | divides by the **non-NULL** count |
| `GROUP BY x` where x is NULL | all NULLs are **one group** |
| `count(DISTINCT x)` over all-NULL | **0** — NULL is skipped before the dedup |

An `INNER JOIN` emits a row only where both sides match, so a row whose join key
is NULL never appears (NULL equals nothing, including NULL), and a row on the
other side with no partner does not either. A `LEFT JOIN` keeps that partnerless
row and NULL-extends the inner side — and `WHERE inner.col IS NULL` on top of it
is the anti-join ("which orders have no items").

**Join cost, honestly.** When a join's `ON` contains a plain equality
(`ON items.oid = orders.oid`), the planner consumes it into the inner fetch:
each outer row does one PK get or index probe — the index nested loop. An `ON`
with no equality falls back to reading the inner side once and holding it, with
every pair evaluated: fine for a few thousand rows, not a few million. Your
`WHERE` still waits for the joined row. `EXPLAIN` says which form you got.

## Durability: pick one on purpose

| mode | what a commit means | use it for |
|---|---|---|
| `none` | in memory; survives process death, not power loss | `/dev/shm`, tests, scratch |
| `commit` | `msync` of data and meta before ack | simple durable |
| `wal` | one sequential append + one `fdatasync` before ack | **durable, and much cheaper per commit** |
| `async` | appended and meta-flipped, `fdatasync` on an interval | throughput, when losing a bounded recent window is acceptable |

`wal` is the default recommendation: same guarantee as `commit`, far less work.

`async` is the one to be careful with — a commit is acknowledged **before** it is
power-loss-durable. You may lose a bounded window of recent commits. You will not
get a torn database.

## Large values: stream them

A blob column takes a `Value::Blob(Vec<u8>)` like any other value — but for a
value measured in megabytes, materializing that `Vec` *is* the cost. Stream it
instead:

```rust
// "Put this file in the database", one call: the bytes are pulled in a page
// at a time and are never resident. A 256 MiB file costs ~132 KiB of
// anonymous RSS, not 256 MiB. The `&[][..]` placeholder marks the streamed
// column (index 1 here); it is replaced by the file's bytes.
let mut s = db.begin()?;
s.insert_file("files", &params![1i64, &[][..]], 1, "/path/to/big.bin")?;
s.commit()?;
```

`WriteSession::insert_streaming` is the same thing over any `std::io::Read`
with a known length. Two honest constraints: the streamed column must be the
table's **last** variable-length column, and the table cannot carry a secondary
`UNIQUE` index (uniqueness would need the whole value in hand before it
streams). It pulls rather than handing you a writer on purpose — a
`write_all(chunk)` API would hold the writer lock across your code.

## Coming from sqlite3

| you know | here |
|---|---|
| `sqlite3.connect("app.db")` | `Database::open("app.toml")` — file, no server, same idea |
| `CREATE TABLE` | the config file (or `mirror import`) — live DDL is designed ([DESIGN-DDL.md](DESIGN-DDL.md)), not built |
| `?` placeholders | `$1`, `$2`, … |
| `PRAGMA journal_mode=WAL` | `durability = "wal"` |
| `cp app.db app.snap` | `cp app.mpedb app.snap` (plus `-wal` if you use it) |
| `EXPLAIN QUERY PLAN` | `EXPLAIN` |
| `INSERT … ON CONFLICT DO UPDATE` | the same, and the target may be the PK or a UNIQUE column |
| dynamic typing | rigid columns; a wrong type is an error at write time |

Differences that will bite, each one exercised in `tests/guide.rs`:

1. **No `CREATE TABLE`.** See above — the table-id numbering makes it a rebuild.
2. **Division by zero raises.** sqlite yields NULL. So does overflow: mpedb
   errors where sqlite silently promotes to REAL.
3. **`RIGHT`/`FULL` only join two tables.** The two-table forms work (a
   `RIGHT` plans as a `LEFT` with the sides swapped; `FULL` NULL-extends both
   sides); putting either inside a multi-join CHAIN is refused with a message
   saying the manual fix — a left-deep plan cannot hold the right side as a
   subtree. Everything else joins freely: N-way `INNER`/`LEFT` chains,
   self-joins, aliases, `CROSS JOIN`, compound `UNION [ALL]`/`EXCEPT`/
   `INTERSECT` chains, and scalar/`EXISTS` subqueries (correlated included).
   A scalar subquery returning more than one row is an ERROR (PostgreSQL's
   rule) — sqlite silently takes the first row.
4. **`ORDER BY` must name something the query outputs** — a column of the table,
   an output position (`ORDER BY 1`), or a selected expression. `SELECT c FROM t
   ORDER BY a + 1` works (a hidden sort column is added); under `SELECT DISTINCT`
   the key must be in the `SELECT` list, as in PostgreSQL, because once
   duplicates collapse it is *which duplicate survived* that decides the order —
   and the query never said.
5. **`ORDER BY 1 + 1` is refused.** Only a bare integer is an ordinal. sqlite
   sorts by the constant, which is to say not at all.
6. **`CASE`/`COALESCE` arms cannot mix `int64` and `float64`.** sqlite types
   the winning arm per row — `COALESCE(30, avg(x)) / 35` divides an INTEGER
   when arm 1 wins — and rigid typing cannot express "the type of whichever
   arm wins". Widening 30 to 30.0 silently changes that division (measured:
   82 wrong answers in the sqllogictest expr tree), so the mix is a compile
   error instead; an explicit `CAST` on the arms makes it legal.

And the difference that is the entire point: **sqlite's `STRICT` is not this.**
STRICT rejects what cannot convert *losslessly*; it still stores `'42'` in an
`INT` column as `42`, and `2.0` as `2`. mpedb rejects anything that is not the
declared type, with `int64 → float64` as the one exception.
[The full measured matrix](crates/mpedb-testkit/README.md) is in the testkit
README.

## Migrating a sqlite3 database

This is what mpedb is for. Point it at the sqlite file your tests already use:

```console
$ sqlite3 shop.sqlite "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, age INTEGER);
                       INSERT INTO users VALUES (1,'ada@example.com',36),(2,'bob@example.com','oops');"

$ sqlite3 shop.sqlite "SELECT id, age, typeof(age) FROM users;"
1|36|integer
2|oops|text          <-- a string, in a column declared INTEGER

$ mpedb mirror import --source shop.sqlite --dest shop.mpedb
mpedb: type mismatch: sqlite `users.age`: text in a non-text column (import is strict-reject)
```

That is the bill arriving early. PostgreSQL would have rejected that row too —
on deploy day, in production, at 2am. Here it costs you a shell command and you
are still looking at the data that caused it.

Fix the row and it imports:

```console
$ sqlite3 shop.sqlite "UPDATE users SET age = 41 WHERE id = 2;"
$ mpedb mirror import --source shop.sqlite --dest shop.mpedb
imported sqlite:shop.sqlite into shop.mpedb
  users                             2 rows  (table 0)
  total: 2 rows across 1 tables
mirror is source-authoritative (epoch 1); capture enabled.
```

The result is config-free — the schema came from the source and lives in the
file:

```console
$ mpedb dump shop.mpedb
[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
  nullable = false
  ...

$ mpedb exec shop.mpedb "SELECT id, email, age FROM users ORDER BY id"
id	email	age
1	ada@example.com	36
2	bob@example.com	41
```

The mirror runs in both directions and keeps going: `pull` picks up source
changes incrementally while the source is being written to, `push` writes back,
`switch` moves authority with an epoch fence, and `conflicts`/`resolve` handle
divergence. `mirror regenerate` is how you change a schema. PostgreSQL is
supported at the library level; the CLI drives sqlite today.

## Many processes

Any number of processes may `open` the same file at once. Readers never block
writers and never block each other — they take an MVCC snapshot. Writers
serialize through one writer lock.

Any process may be `SIGKILL`ed at any instant without corrupting the database.
That is not a hope; it is fuzzed:

```sh
mpedb stress --dir /tmp/x --workers 8 --secs 30 --mode mixed
mpedb crash  --dir /tmp/x --waves 20 --children 8
mpedb powerloss --dir /tmp/x --rounds 50 --durability wal
```

A process that prepared a plan publishes it in the shared registry, so **another
process can `execute(hash, params)` for a plan it never compiled**.

If you need write parallelism, use more files: separate files are separate writer
locks, and a `Workspace` addresses several as `alias.table`.

## The CLI

```sh
mpedb exec <target> "<SQL>" [params…]   # one statement; target = config.toml or a .mpedb
mpedb repl <target>                     # interactive
mpedb prepare <target> "<SQL>"          # compile + publish, print the hash
mpedb call <target> <hash> [params…]    # execute a published plan
mpedb proc define|call|list …           # stored procedures
mpedb dump <file.mpedb> [--data]        # config-free schema/row dump
mpedb bench <config.toml> | --auto
mpedb mirror import|export|pull|push|sync|switch|conflicts|resolve|regenerate

# and the fuzzers, which are how the crash-safety claim is kept honest:
mpedb stress|crash|collide|powerloss|mirror-collide …
```

Run `mpedb` with no arguments for the full list.

`<target>` is a config file **or** a `.mpedb` directly — the file knows its own
schema, so a mirror needs no config at all.

## Python

```sh
cargo build --release -p mpedb-py
cp target/release/libmpedb_py.so mpedb.so
```

There are two APIs. The **DB-API 2.0** one (PEP 249) is what `sqlite3`-shaped
code already expects:

```python
import mpedb                       # apilevel '2.0', paramstyle 'qmark'

conn = mpedb.connect("app.toml")
cur = conn.cursor()
cur.execute("INSERT INTO users (id, email) VALUES (?, ?)", [1, "ada@example.com"])
conn.commit()

cur.execute("SELECT id, email FROM users WHERE id = ?", [1])
[d[0] for d in cur.description]    # -> ['id', 'email']
cur.fetchall()                     # -> [(1, 'ada@example.com')]
```

`connect` / `cursor` / `execute` / `executemany` / `fetchone` / `fetchmany` /
`fetchall` / `description` / `rowcount` / `commit` / `rollback` / `close`,
iteration over a cursor, and the connection as a context manager (commit on a
clean exit, roll back on an exception — sqlite3's semantics).

Both `?` and `$1` work, and mixing them in one statement is refused rather than
guessed at. That is the engine's parser, not a driver rewrite, so a `?` inside a
string literal is a question mark:

```python
conn.execute("SELECT ?, 'why?' FROM users WHERE id = ?", [42, 1]).fetchone()
# -> (42, 'why?')
```

**Two things it does not pretend.** There is no `CREATE TABLE`, so a program
that runs DDL raises `ProgrammingError` here — the schema comes from the config
or `mirror import`. And a connection's buffered writes are not visible to its own
reads until `commit()`: mpedb has one exclusive writer lock, so the driver
buffers rather than holding it open across an idle `input()`.

The direct API is the other one — no cursors, no buffering:

```python
db = mpedb.Database("app.toml")
db.query("INSERT INTO users (id, email) VALUES ($1, $2)", [1, "ada@example.com"])
db.query("SELECT email FROM users WHERE id = $1", [1])
# -> [('ada@example.com',)]

with db.begin() as tx:             # a real transaction, holds the writer lock
    tx.query("INSERT INTO users (id, email) VALUES ($1, $2)", [2, "b@example.com"])
```

Either way, rows come back as a list of tuples and errors are **DB-API 2.0
exceptions**:
`IntegrityError` for a constraint (NOT NULL / UNIQUE / CHECK / PK),
`ProgrammingError` for a bad statement or a type that does not fit the column,
`OperationalError` for the engine. So `except IntegrityError` does what you
expect it to.

```python
db.query("INSERT INTO users (id, email) VALUES ($1, $2)", [2, 42])
# ProgrammingError: value of type int64 cannot be inserted into column `email`
```

Built as `abi3-py312`, with the GIL released around engine calls — so threads
doing database work actually run concurrently.

---

## Where to go next

- [README](README.md) — what this is and why, plus the exact SQL surface
- [BENCHMARKS.md](BENCHMARKS.md) — head-to-head against SQLite and PostgreSQL,
  with the methodology and every machine's numbers
- [DESIGN.md](DESIGN.md) — the concurrency, locking and commit protocols. Read
  this before touching that code; every protocol in it survived a 37-finding
  adversarial review.
- [DESIGN-MIRROR.md](DESIGN-MIRROR.md), [DESIGN-MULTIDB.md](DESIGN-MULTIDB.md) —
  mirroring, and multi-database workspaces + row-level security
