# Your .db, with mpedb as its WAL

The mental model this page exists for: **keep your sqlite `.db` as the
durable, canonical home — and let the `.mpedb` beside it play the role a WAL
plays for a database.** Writes and MVCC reads take the fast path through
mpedb; a *checkpoint* folds them back into the `.db`, where every sqlite tool
on earth can read them. One idea, applied end to end — the same relationship
`app.db-wal` has to `app.db`, lifted one level up and given mpedb's engine.

```
        writes, MVCC reads, many processes
                     │
                     ▼
   app.db.mpedb   ──────────  the "WAL": fast, crash-safe, mpedb's world
        │
        │  mpedb checkpoint app.db        (like a WAL checkpoint)
        ▼
      app.db      ──────────  the home: plain sqlite, every tool reads it
```

## What works today

**Open it like sqlite3 does** — a bare path, repl or one-shot:

```console
$ mpedb app.db                          # repl; first open creates the sidecar
$ mpedb app.db "SELECT count(*) FROM users"
$ mpedb app.db "INSERT INTO users (id, name) VALUES (7, 'ada')"
$ mpedb checkpoint app.db               # fold writes back into the .db
$ sqlite3 app.db "SELECT name FROM users WHERE id = 7"
ada
```

The first open imports schema + data into `app.db.mpedb` and installs
mirror's change tracking in the base; every later open pulls incrementally,
so foreign sqlite writes flow in, and `checkpoint` pushes yours back in one
sqlite transaction with mirror's conflict rules (DESIGN-MIRROR §8 — parked,
reported, never silently dropped).

**Read a `.db` directly, zero import** — the native reader
(`mpedb-sqlitefmt`: no sqlite library in the path, differentially verified
row-for-row against the real one):

```console
$ mpedb app.db --direct "SELECT name FROM users WHERE id = 7"   # b-tree seek
$ mpedb dump app.db --data                                      # inspect any .db
```

`--direct` is read-only and takes no locks yet — run it against a database
nothing is writing. In Rust the same thing is `mpedb::SqliteAttach`, and the
full mpedb planner/executor runs the SQL (joins, aggregates, EXPLAIN
included).

## The WAL metaphor, honestly

The metaphor is load-bearing, so its edges are too:

| WAL property | here |
|---|---|
| readers of the base see the last checkpoint | ✅ sqlite tools see `app.db` as of the last `mpedb checkpoint` — exactly as sqlite readers of another process see WAL-committed-but-uncheckpointed data: not yet |
| the log absorbs writes fast | ✅ mpedb's engine: MVCC snapshots, lock-free readers, group commit, SIGKILL-safe recovery |
| checkpoint folds the log into the base | ✅ one sqlite transaction; idempotent against itself; conflicts with foreign writes park and report |
| the log stays bounded | ✅ today the sidecar is a fixed-size mpedb file; in the designed next stage it holds only DELTAS and truncates at checkpoint |
| anyone can read the base at any time | ✅ — with one caveat: while mpedb holds its (designed) base lock, a *crashed foreign writer's* hot journal blocks other sqlite readers until mpedb mediates; documented in the design |

What it is **not**: two engines writing both files simultaneously with mutual
visibility. Foreign sqlite writers run either between checkpoints (today's
sidecar flow pulls them in on the next open) or in explicit unlocked windows
(next stage), and divergence is *reconciled*, never guessed away.

## Where this is going

The full design — reviewed hard (20 adversarial findings folded) — is
[DESIGN-SQLITE-BACKED.md](DESIGN-SQLITE-BACKED.md). The staging:

- **v0 (shipped)**: the sidecar flow above — full-copy mirror, one-command
  UX, checkpoint = push.
- **v1 (shipped)**: the native reader — `dump`/`--direct`/`SqliteAttach`,
  zero import, both b-tree layouts, refusals by name (WAL-mode files,
  non-UTF8).
- **v2 (in flight — the overlay itself is live)**: the true delta overlay.
  The `.mpedb` stops being a copy and holds only what changed since the last
  checkpoint (row images + tombstones); untouched data is read straight from
  the `.db` through the native reader, merged per PK at read time.
  `SqliteOverlay::open("app.db")` in the facade runs this today in **LOCKED**
  mode (mpedb holds sqlite's own SHARED lock, so the fast path needs zero
  validation and foreign writers get their normal `SQLITE_BUSY` — verified
  against the real sqlite library in-tree), with the settled base-stamp
  stored in the overlay and divergence refused by name at reopen. Still to
  land: the checkpoint (push deltas into the base + truncate the overlay),
  and the other two lock modes — **OPTIMISTIC** (no standing lock — a
  transient SHARED plus a hot-journal check per statement, µs-class,
  self-healing on divergence) and **UNLOCKED-OFFLINE** (overlay-only, for
  bulk foreign rewrites). The settled-stamp trick makes "did anything touch
  the base?" one `stat()` after minutes or days unlocked.
- v3 (measured, maybe never): sqlite index probes for cold reads, hot-row
  promotion, WAL-mode bases.

A one-file variant (mpedb living *inside* the `.db` as blob regions) was
explored seriously and set aside — the analysis with per-approach verdicts
and the experiments that would revive it is
[DESIGN-ONEFILE-EXPLORATION.md](DESIGN-ONEFILE-EXPLORATION.md).

## Honest limits, today

- The v0 sidecar is a **full copy**, not a delta — right for working sets
  that fit comfortably, wasteful for a 50 GB archive (that is exactly what
  v2 removes).
- v0 installs mirror's tracking tables + triggers in the base (visible to
  sqlite tools; that is how incremental pull works without re-reading
  everything).
- `--direct` trusts you about quiescence until v2's lock modes land.
- mpedb's SQL is a strict, measured subset — [COMPAT.md](COMPAT.md) row for
  row, 99.8% of sqlite's own 5.3M-record corpus passing with zero wrong
  answers.
