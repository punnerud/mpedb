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

Operators sit at comparison precedence, apply once (no chaining —
parenthesize), and expansion nests at most 8 levels (a self-expanding
operator refuses deterministically).

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
  path (BENCHMARKS-VECTOR.md).

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
