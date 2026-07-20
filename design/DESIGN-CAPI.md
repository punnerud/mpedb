# DESIGN-CAPI — a libsqlite3-compatible C-API shim (ABI-level drop-in + broader testing)

**Status: design (2026-07-18). Forward-looking (Phase 6+). The path from SQL-level to **ABI-level**
sqlite drop-in: a cdylib exporting sqlite3's C-API, backed by mpedb, so everything that links
`libsqlite3` — Python's `sqlite3` module, Django, tools, and their test suites — runs against mpedb
unchanged. Complements the existing native `mpedb-py` (PyO3): same engine, two front doors. Prior
art: libSQL (Turso), DuckDB's sqlite3 C-API shim.**

## 0. The idea

A new crate `mpedb-capi` (cdylib) exports the sqlite3 C symbols (`sqlite3_open_v2`, `sqlite3_prepare_v2`,
`sqlite3_step`, `sqlite3_bind_*`, `sqlite3_column_*`, `sqlite3_exec`, `sqlite3_errmsg`, …) as
`extern "C"`, translating each to mpedb's Rust facade. `LD_PRELOAD=libmpedb_sqlite3.so python app.py`
(or linking it as `libsqlite3`) makes an unmodified libsqlite3 consumer talk to mpedb.

## 1. What it unlocks — the testing answer

The point Morten raised: matching the C-API lets us run **far more** tests than the portable
sqllogictest corpus, by borrowing the ecosystem's suites:
- **Python's built-in `sqlite3` module** and its `unittest` suite (CPython links libsqlite3).
- **Django** — its ORM and its own **test suite**, with mpedb as the DB backend (this is what makes
  the retired "a Django test suite will not run against it" line actually false).
- Any ORM / tool / language binding that links libsqlite3 (Rails via the sqlite3 gem, Node
  better-sqlite3, etc.).
- The **SQL-behavior** portions of sqlite's own TCL suite, via a TCL harness linked against the shim.

**Honest scope limit:** sqlite's TCL suite is *heavily* sqlite-internal — it asserts on the file
format, VDBE opcodes, specific `PRAGMA` outputs, byte-exact quirks and error strings that mpedb
deliberately does not reproduce. Those tests stay out of scope. The value is the **SQL-semantics**
tests plus the vast **libsqlite3-consumer** ecosystem, not sqlite's internal regression suite.

## 2. The C-API surface

The sqlite3 C-API is hundreds of functions; a **core ~30 cover the overwhelming majority of usage**:
- open/close: `sqlite3_open[_v2]`, `sqlite3_close[_v2]`, `sqlite3_busy_timeout`.
- prepare/step: `sqlite3_prepare_v2`, `sqlite3_step`, `sqlite3_reset`, `sqlite3_finalize`,
  `sqlite3_exec`.
- bind: `sqlite3_bind_{int64,double,text,blob,null}`, `sqlite3_bind_parameter_count/index`.
- column read: `sqlite3_column_{count,type,int64,double,text,blob,bytes,name,decltype}`.
- status: `sqlite3_errmsg`, `sqlite3_errcode`/`extended_errcode`, `sqlite3_changes`,
  `sqlite3_last_insert_rowid`, `sqlite3_libversion[_number]`.
- **Result codes must match** — `SQLITE_OK/ROW/DONE/BUSY/ERROR/CONSTRAINT/MISUSE/…` (tests check the
  integers). Extended codes where consumers rely on them.

Out of scope (return `SQLITE_ERROR`/a clear "unsupported", documented): loadable extensions
(`sqlite3_load_extension`), VDBE/bytecode introspection, virtual-table modules
(`sqlite3_create_module` — FTS is native, §DESIGN-FTS). Incremental blob
(`sqlite3_blob_open/read/write`) **maps onto mpedb's own #43 incremental blob API** rather than
being stubbed.

⚠ **Three things this list called out of scope have since shipped** — the ecosystem's own test
suites demanded them, which is exactly §1's argument working:

- **User-defined functions** (`crates/mpedb-capi/src/udf.rs`): `sqlite3_create_function[_v2]`
  scalar and aggregate (`xStep`/`xFinal` over a real `sqlite3_aggregate_context`), plus
  `sqlite3_create_window_function` — a genuine sliding window with `xValue`/`xInverse`, not an
  aggregate re-run per frame; supplying only `xStep`/`xFinal` degrades to a plain aggregate.
- **Collations**: `sqlite3_create_collation[_v2]`, including CPython's
  `create_collation(name, None)` deletion form (`xCompare == NULL` removes the entry).
- **The online-backup API** (`crates/mpedb-capi/src/backup.rs`): `sqlite3_backup_init/step/
  finish/remaining/pagecount`, with the page counts mapped honestly onto mpedb's own unit of
  copy rather than faked.

Measured consequences and the remaining gap list live in `C-API-COMPAT.md`.

## 3. Lifecycle mapping

- `sqlite3*` connection handle → an mpedb `Database` + a `Session`. Multi-connection concurrency is
  mpedb's MVCC (a strict improvement over sqlite's single-writer — a libsqlite3 consumer that expected
  `SQLITE_BUSY` under contention gets mpedb's group-commit instead, which is compatible-or-better).
- `sqlite3_stmt*` → a compiled (content-hashed) plan + a bound-parameter set + a row cursor.
  `prepare_v2` = mpedb `prepare`; `step` advances the cursor (`SQLITE_ROW`/`SQLITE_DONE`); `bind_*`
  sets params; `column_*` reads the current row's typed value (mpedb `Value` → sqlite
  type/affinity); `exec` = a prepare→step loop feeding the callback.
- Text is UTF-8 (`sqlite3_column_text`); the shim owns per-statement scratch buffers with sqlite's
  pointer-lifetime rules (valid until the next `step`/`finalize`).

## 4. Fidelity requirements (so the borrowed tests pass)

- `sqlite3_column_type` reports `SQLITE_{INTEGER,FLOAT,TEXT,BLOB,NULL}` — mpedb's typed `Value`s map
  cleanly (and the CAST-affinity work #83 already models sqlite's type rules).
- Error **codes** match; error **strings** match where a test asserts them (else documented
  divergence). `changes()`/`last_insert_rowid()` reflect the last statement (the latter ties to the
  #85 rowid-alias work).
- Statement pointer/lifetime semantics exactly as sqlite documents, or the bindings crash.

## 5. Relationship to `mpedb-py`

mpedb already ships `mpedb-py` (PyO3, abi3) — the **native** path, idiomatic and fast, for code
written *for* mpedb. `mpedb-capi` is the **compat** path: existing sqlite code and test suites run
unchanged. Same engine underneath; the choice is "adopt mpedb's API" vs "keep sqlite's API and swap
the library."

## 6. Staging

1. **Core shim** — open/prepare/step/bind/column/exec + result codes; run a hand-written Python
   `sqlite3` script end to end.
2. **Python `sqlite3` unittest subset** green (the SQL-behavior tests; skip sqlite-internal ones).
3. **Django backend + Django test suite** — the headline "it really is drop-in" milestone.
4. **SQL-behavior TCL tests** via the shim; **incremental-blob** mapped to #43.

Phase 6+, after the SQL-parity sprint. This is the ABI half of the drop-in goal, and the answer to
"can we run more of the tests" — yes, the ecosystem's, by matching the one interface they all speak.

## 7. `busy_timeout` vs group commit — the forfeit, measured, and why it stays (#110)

**The shape.** #109 made `sqlite3_busy_timeout` real end to end: the shim mirrors
the knob into `Database::set_busy_timeout`, so the *engine's* writer-lock wait is
bounded and cross-process contention answers `SQLITE_BUSY` at the deadline
instead of blocking forever (compat gap E1). The shim installs that policy on
every connection at open — sqlite's default is timeout 0, i.e. immediate BUSY,
and a connection with no policy would block forever.

But `Database::run_write_plan` gates Phase-2 group commit on

```rust
let use_ring = deadline.is_none() && ring_exec::ring_enabled(self) && …
```

so **a deadline-carrying write never publishes a ring intent**. Every write
through the shim takes the direct writer-lock path. It still *leads* on acquire
(it drains everyone else's intents, so mixed deployments stay live), but it can
never be a *follower* — it can never share another leader's commit.

**What it costs.** `crates/mpedb-capi/tests/ring_forfeit.rs` measures it: three
paired arms, interleaved, over freshly seeded `durability = commit` files on
/mnt/ext4 (250 rows/writer, 5 reps, median of the slowest writer).

| writers | shim | facade + busy policy | facade, no policy (ring) |
|---------|------|----------------------|--------------------------|
| 1       | 157  | 149                  | 179 rows/s               |
| 4       | 146  | 156                  | **366 rows/s**           |

A second full run on the same box reproduces it: 177 / 182 / 169 rows/s at one
writer, **161 / 167 / 392** at four. Uncontended, the three arms are
indistinguishable — an uncontended write takes the `try_begin_write` fast path
and leads directly; the ring is not on that path.
Contended, the busy policy costs **2.3–2.5×**, and the shape of the loss is
worse than the ratio suggests: *four shim writers deliver the aggregate
throughput of one*. Each pays its own `msync`, so adding writers adds flushes,
not throughput. #111 halved the flushes per durable commit (4.05 → 2.02
`msync`s), which makes each forfeited batch membership worth relatively more,
not less.

**Why "just enable the ring" is unsound — and now measured, not argued.** Delete
the `deadline.is_none()` term and the shim reaches parity (n=4: 146 → 383
rows/s). It also breaks
`ring_forfeit.rs::a_ring_enabled_shim_write_still_answers_busy_at_its_deadline`:
against a foreign transaction holding the writer lock for 1.5 s, a write with a
**200 ms** budget returns `SQLITE_OK` after **1.4996 s**. A published intent
cannot be withdrawn — §5.3 pins a READY+stamped slot to its incarnation so the
leader's collect → stage → post cannot be raced, and releasing from READY before
the result is posted could COMMIT a phantom write after the caller was already
answered `SQLITE_BUSY` — so at the deadline there is nothing to abandon, and the
enqueued wait-or-lead loop makes no progress while the lock is held. That is
gap E1 reopening.

**Why a budget threshold does not rescue it either.** The tempting version is
"ride the ring only when the remaining budget comfortably exceeds the expected
batch latency". The measurement refutes it: the overshoot is not batch latency,
it is *the length of whatever transaction currently holds the writer lock* —
unbounded, unknowable at enqueue time, and independent of the caller's budget. A
60 s budget looks safe against a 1.5 s hold and is not safe against a 90 s one,
and nothing at the enqueue point can tell them apart. There is no defensible
threshold, only a luckier one.

**So the busy policy stays**, and the shim keeps forfeiting group commit. The
statement in `run_write_plan`'s comment — "the group-commit amortization
forgone is the busy-timeout caller's explicit trade" — is the shipped
behaviour, now with a price tag on it.

### 7.1 What would actually close it: claim-on-collect

The forfeit is not intrinsic. It exists because the ring has no way to say *"a
leader has taken responsibility for this intent"* — the enqueuer cannot
distinguish "still only published" (safe to withdraw: nothing will execute)
from "already collected" (must wait: the write is happening). Give it that
distinction and the deadline becomes expressible:

1. **`mpedb-core/src/ring.rs` — add `ST_CLAIMED` to the slot state machine.**
   `collect_ready` becomes claim-on-collect: for each READY slot it CASes the
   header word `(pid, gen, READY) → (pid, gen, CLAIMED)` and takes only the
   slots whose CAS succeeded. `stage_result`/`post_done` are unchanged in shape
   (they CAS the claimed word). `release` gains CLAIMED to its state list — an
   owner can still pick a result up in the window before `post_done` flips the
   header. `recover_orphans` treats CLAIMED exactly as it treats READY today
   (stamp ≤ committed ⇒ post; stamp > committed ⇒ clear and re-execute), plus
   the claimed-but-never-staged case: re-arm to READY. Only one leader exists at
   a time and the recovering process holds the writer lock, so every CLAIMED
   slot it sees is by construction orphaned.
2. **New enqueuer primitive `try_withdraw(idx, owned) -> bool`**: CAS
   `(pid, gen, READY) → (0, gen+1, EMPTY)`. Wins ⇒ no leader ever claimed it ⇒
   the intent never executes ⇒ answering `Busy` is honest. Loses ⇒ a live leader
   holds the writer lock and is committing this batch right now.
3. **`crates/mpedb/src/lib.rs` — the gate drops `deadline.is_none()`**, and the
   enqueued wait-or-lead loop gains one arm: at the deadline, `try_withdraw`;
   on success return `Error::Busy`, on failure keep waiting.

The resulting contract is honest and *bounded*: the busy budget bounds the time
to **acquire the lock or withdraw the intent**; once a leader has claimed the
intent the caller waits for that batch, which is bounded by one commit round
(the leader holds the lock and is running — and if it dies, our own next
`try_begin_write` recovers the orphan and drains us). That is a genuinely
different guarantee from today's "the budget bounds the whole statement", and it
is strictly better than sqlite, where a busy handler can also be beaten by an
arbitrarily long lock hold.

**Why this was not done here.** Two reasons, both about blast radius rather than
difficulty:

- **The header word has no spare bits.** It is `{pid: u32 ‖ gen: 30 ‖ state: 2}`
  = exactly 64. All four state values are taken (EMPTY/RESERVED/READY/DONE), so
  a fifth state costs a generation bit (30 → 29). The generation is the ABA
  defence for slot reuse; narrowing it is a shared-memory wire-format change to
  the structure §5.3's incarnation-safety argument is built on.
- **It is commit-path work.** The claim CAS sits between "collect" and "execute"
  in the exact sequence the 37-finding review hardened, and it adds a state that
  crash recovery must handle in `recover_orphans` *and* `reclaim_dead`. That
  earns a full adversarial review of §5.3's ordering, not a shim-side patch.

Until then the guard test above is the tripwire: it is the one test that fails
the moment someone tries the one-line version of this optimisation.
