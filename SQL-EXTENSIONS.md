# SQL-EXTENSIONS — stored functions and `:sym:` custom operators

mpedb's SQL surface is user-extensible, and the extensions live **in the
database file**, shared by every attached process. Two mechanisms, both backed
by PySpell (a sandboxed Python/Rust subset compiled to budgeted IR at define
time — the runtime never parses source):

| mechanism | call shape | what it is |
|---|---|---|
| stored function | `f(x, y)` | a value computed per row |
| custom operator | `a :sym: b` | a **macro**: rewrites to SQL at compile time |

If you are an LLM working with an mpedb database: `mpedb fn list <target>` and
`mpedb op list <target>` show what is defined; this file is the contract.

## Stored functions

```sh
echo 'def double(x):
    return x * 2' > double.py
mpedb fn define app.toml double.py
mpedb exec app.toml 'SELECT double(amount) FROM orders'
```

- Name and arity come from the `def` itself. Full procedure subset: `while`,
  `for`, locals, `if`/`else`. **No SQL inside** (that is what stored
  procedures, `mpedb proc`, are for) and no I/O — a function sees its
  arguments and nothing else.
- Stored content-addressed; plans calling it carry the definition's **hash**,
  so they are valid in every attached process and live in the shared plan
  registry. Redefining bumps the schema generation: every process re-binds on
  its next statement.
- Execution is budgeted: a runaway body is a deterministic error at the same
  instruction count everywhere.

### PL/pgSQL — the same mechanism, PostgreSQL's language

`pg_dump` writes functions as `CREATE FUNCTION … AS $$ … $$ LANGUAGE plpgsql;`,
and mpedb compiles that statement whole — through the CLI, or straight over the
wire protocol, which is what makes a dump replayable:

```sh
mpedb fn define app.toml add_tax.sql          # a .sql file is plpgsql
psql -h /run/mpedb -f dump-fragment.sql       # or feed the dump directly
mpedb exec app.toml 'SELECT add_tax(amount, 25) FROM orders'
```

It is a third FRONTEND, not a third runtime: PL/pgSQL is parsed on the host at
define time and emits the same IR the Python and Rust subsets do, so the
security boundary ("the parser stays on the host; the runtime only ever sees
IR") is unchanged, and so are the budget and the content hash.

The unit is the whole statement rather than the bare body, because the HEADER
is where the parameter names live — a frontend fed only the body would need a
second channel to learn them, and the two could disagree.

**Accepted:** `DECLARE`, assignment (`:=` and PostgreSQL's `=`),
`IF`/`ELSIF`/`ELSE`, `WHILE`, bare `LOOP`, `FOR v IN [REVERSE] lo..hi [BY n]`,
`EXIT`/`CONTINUE` (with `WHEN`), `RETURN`, arithmetic and comparisons,
`AND`/`OR`/`NOT`, `IS [NOT] NULL`, and `::int` casts as identities.

**Refused by name**, in three groups: a different FEATURE (`RETURNS SETOF` /
`RETURNS TABLE` are row sources, `RETURNS trigger` is a trigger body,
`CREATE PROCEDURE` is invoked with `CALL`); the stored-function CONTRACT
(`SELECT … INTO`, `PERFORM`, `INSERT`/`UPDATE`/`DELETE`, `FOR … IN SELECT` all
touch the database, and a function is evaluated per row inside a statement that
is already scanning — those belong in `mpedb proc`); and what the IR cannot say
without inventing an answer (`RAISE`, `||`, a converting `::text`, dynamic
`EXECUTE`, an `EXCEPTION` block).

Two semantics decisions worth knowing, both measured against PostgreSQL 16
rather than assumed:

- **`x = NULL` is a compile error**, pointing at `IS NULL`. In PostgreSQL that
  comparison is NULL — never true, whatever `x` is — while the interpreter's
  equality says `None == None` is true. Refusing it is the only way not to
  quietly disagree, and in PostgreSQL the comparison was already a bug.
  (`IF <null> THEN` needs no such care: it does not take the branch in either.)
- **`/` and `%` follow PostgreSQL, not Python** — truncate toward zero, and the
  remainder takes the dividend's sign. `(-7)/2` is `-3` and `(-7)%2` is `-1`.

## `splice()` — sub-edits inside one cell

```sql
UPDATE doc SET body = splice(body, $at, $remove, $insert) WHERE id = $1
```

Replace `$remove` bytes of `body` at byte offset `$at` with `$insert`. Text or
blob; the result has the type of the first argument.

Not a sqlite function, and not sugar for `substr() || … || substr()`. The point
is **when** it evaluates: an UPDATE expression reads the row at write time, so
two editors splicing disjoint ranges of the same cell both land, where two
whole-value writes each computed from the version its author read would lose
one. That is the difference between a sub-edit and a value
(design/DESIGN-COLLAB.md §3).

**Strict about offsets.** A range past the end, or a cut inside a multi-byte
character of a TEXT value, is refused rather than clamped: a stale offset is a
wrong question, and clamping answers it with silently mangled text.

## The cost layer (tune / cost-policy / stats)

The cost calculator itself is adjustable, and the adjustments live in the
file too: `mpedb tune set <target> ndv_discount=false` (stored switches),
`mpedb cost-policy set <target> policy.py` (a stored PySpell
`def policy(kind, table, index_no, bucket, rows_bucket, archetype):` that
adjusts every pricing decision — it sees statistics and the workload model's
archetype), and `mpedb stats <target>` (what the engine believes: rows, NDV,
analyze state). All schema-generation-gated: a change re-prices every attached
process's next statement.

## Custom operators — `:sym:`

An operator is a **compile-time macro over operand source text**. The parser
captures the operands' TEXT (they are parsed for extent, never bound), hands
it to your macro, and splices the returned SQL fragment in place. The
expansion then binds like hand-written SQL — every type rule and refusal
applies to it — and the compiled plan contains only the expansion:
sugar and expansion produce **identical plan hashes**.

### Fixity: the two-bit registration

| bits | name | shape | macro signature |
|---|---|---|---|
| `11` | infix | `a :op: b` | `def m(left, right):` |
| `10` | postfix | `a :op:` | `def m(left):` |
| `01` | prefix | `:op: a` | `def m(right):` |
| `00` | niladic | `:op:` | `def m():` |
| `100` | statement | `:op: <anything…>` | `def m(rest):` |

Expression operators sit at comparison precedence, apply once (no chaining —
parenthesize), and expansion nests at most 8 levels (a self-expanding
operator refuses deterministically).

**The statement bit (`100`) is a language front.** A statement operator must
be the FIRST token; its macro receives the ENTIRE remaining source as one raw
string and returns a complete statement. In principle you define one
`:graph:` operator and build your own graph language behind it — and because
the expansion re-enters the same pipeline, that language's output may itself
use further `:op:` forms:

```python
def graphlang(rest):
    if rest == "count":
        return "SELECT count(*) FROM edge"
    return "SELECT id FROM orders WHERE id :->: (" + rest + ") ORDER BY id"
```

```sql
:graph: count        -- the language's own vocabulary
:graph: 3            -- expands THROUGH the inner :->: operator
```

A statement operator in expression position refuses by name, and an
expression operator cannot begin a statement — the two spaces stay disjoint.

### The founding example

`SELECT * FROM orders WHERE TIME :>: now` — neither `TIME` nor `now` exists.
The macro receives the raw texts `"TIME"` and `"now"` and DECIDES what they
mean:

```python
def timecmp(l, r):
    lhs = "(" + l + ")"
    if l == "TIME":
        lhs = "t"
    rhs = "(" + r + ")"
    if r == "now":
        rhs = "datetime('now')"
    return lhs + " > " + rhs
```

```sh
mpedb op define app.toml '>' infix timecmp.py "TIME/now vocabulary"
```

Outside an operator's operands, an undefined identifier is still the ordinary
bind error — the vocabulary is contained to where you invoked it.

### Model-driven operators

The workload model's **roles** (design/DESIGN-MODEL-LANG.md) are what tell
generic sugar which tables it means. `mpedb op install-model <target>`
installs, from the stored model:

- `role = "edge"` + `traverse = [src, dst]` → **`:->:`** — `a :->: b` expands
  to `EXISTS (SELECT 1 FROM <edge> WHERE src = a AND dst = b)`.
- `role = "embedding"` + `knn` → **`:~:`** — `emb :~: $q` expands to
  `vec_l2(emb, $q)`, so `ORDER BY emb :~: $q LIMIT 10` IS the exact-kNN fast
  path (benchmarks/vector.md).

### Guarantees and limits

- **Deterministic**: macros are pure, budgeted spells; same input text → same
  expansion → same plan hash. Definitions are schema-generation-gated —
  redefinition re-binds every process's next prepare.
- **Contained**: a macro cannot smuggle anything past the binder; its output
  is parsed and bound like anything you could have typed.
- **Introspectable**: `mpedb op list` / `Database::list_operators()`. (A
  SQL-queryable `mpedb_operators` table is planned once the synthetic-table
  seam exists.)
- v1: one fixity per symbol; expression-level expansions only (an operator
  cannot emit a whole `WITH … SELECT` — statement templates are a later
  rung); operand exchange is source TEXT (AST-as-data may come later).
