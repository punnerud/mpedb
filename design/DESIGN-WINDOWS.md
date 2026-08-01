# DESIGN-WINDOWS — porting the engine, and why it is smaller than it looks

**Status: stages 1–4 built and green on REAL Windows (2026-07-27), and the
word is now crash-safe.** The `windows-engine` job builds the engine, runs its
unit tests, the four multi-process properties (mapping coherence, writer
exclusion, cross-process MVCC snapshots, owner death), the two durability arms,
**the facade's own integration suite** — ~1100 tests — and **all six crash
harnesses**, then lints it, all on `windows-latest`. `collide` was the last one
in: it was held out while #160 was open, and it is in at `--durability commit`,
the arm that reproduced. Only stage 5 remains, and it is bookkeeping.

Three attempts were needed, and the first two failed for reasons worth naming:
a committed `:memory:.wlock` made `git checkout` fail before any code ran (§6),
and then three lints inside `cfg(windows)` code that no Linux lint pass
compiles. `linux.yml` now cross-*lints* the engine for Windows and cross-checks
it for wasm32 on every push, so both classes are caught where they are made.

Everything below §0 was written before any of it existed; the parts that turned
out to be wrong are marked rather than deleted, because what a prediction got
wrong is the more useful record.

## 0. The claim this corrects

> "The full engine is Unix-only because of mmap/flock/PROCESS_SHARED//proc.
> Only the portable crates run there."

The first half is accurate. The conclusion people draw from it — that a port is
impractical — does not survive two measurements.

**Measurement 1: the Unix surface is two files.** Across the engine, `libc` and
`std::os::unix` appear 150 times, and **141 of them are in `os.rs` (78) and
`shm.rs` (63)**. The tail is nine hits across four files. `mpedb-cli` has nine
more files, but those are the SIGKILL/fork crash harnesses — test tooling, not
the engine, and not needed for a first port.

**Measurement 2: the seam already exists and already carries three platforms.**
`crates/mpedb-core/src/os.rs` is 727 lines of deliberate platform abstraction
with 35 cfg-arms, written for the macOS port (#18) and extended for wasm32. It
exports about sixteen functions plus `WriterLock`. A Windows arm goes where the
macOS arm went. And `pagestore.rs` exposes a `PageStore` **trait** with an
in-memory `TestStore` impl, so storage is already behind an interface rather
than hardwired to `mmap`.

**Measurement 3: the gap has a number.** `cargo check -p mpedb-core --target
x86_64-pc-windows-gnu` reports **68 errors across 5 files** — 56 in `shm.rs`,
8 in `os.rs`, 2 in `engine/write.rs`, 1 each in `ring.rs` and `lib.rs`.

*(Held up. All 68 were symbol substitution, and the shape of the fix was a
`wincompat` module supplying the Unix-shaped names — `RawFd`, `FileExt`,
`OpenOptionsExt`, and a `libc` module that SHADOWS the real crate for the files
that need it — so `shm.rs`'s 56 call sites compiled unchanged. The same trick
the wasm32 arm already used. Not one of the 68 needed a design change.)*

Every one is the same shape: a missing `libc` function, type or constant, or a
Unix-only method on `std::fs::File` (`as_raw_fd`, `write_all_at`, `mode`). There
is **not one architectural error** in the list — no trait coherence problem, no
lifetime that only works on Unix, nothing saying the design cannot be expressed.
Treat 68 as a lower bound (the compiler stops early on some paths), but treat
the *composition* as the real signal: it is a symbol-substitution job, not a
redesign.

So the shape is not "rewrite the engine". It is "add a third arm to a module
that was built to have arms".

## 1. Every primitive maps, and one maps BETTER

| `os.rs` needs | Linux | macOS (shipped) | Windows |
|---|---|---|---|
| shared file mapping | `mmap(MAP_SHARED)` | same | `CreateFileMapping` + `MapViewOfFile` |
| **robust cross-process lock** | robust `pthread_mutex` | **absent** → FLD-2 sidecar `flock` + DIRTY word | **named mutex; `WAIT_ABANDONED`** |
| futex wait/wake | `futex` | polling park | **`WaitOnAddress` / `WakeByAddressAll`** |
| durability barrier | `fdatasync` | `F_FULLFSYNC` | `FlushViewOfFile` + `FlushFileBuffers` |
| truncate / preallocate | `ftruncate64` / `fallocate` | same | `SetEndOfFile` / `SetFileValidData` |
| punch hole | `FALLOC_FL_PUNCH_HOLE` | — | `FSCTL_SET_ZERO_DATA` |
| process start time (pid identity) | `/proc/<pid>/stat` | `proc_pidinfo` | `OpenProcess` + `GetProcessTimes` |
| pid namespace | `/proc/self/ns/pid` | — | none exist; constant |
| boot id | `/proc/sys/kernel/random/boot_id` | `kern.boottime` | kernel objects vanish on reboot |
| page size | `sysconf` | 16 KiB | `GetSystemInfo` |

Two rows are worth dwelling on, because they are the ones assumed to be
blockers:

**The robust mutex is the hardest primitive, and Windows has it natively.** The
whole of `DESIGN-MACOS-LOCK.md` exists because macOS lacks robust
process-shared mutexes: the FLD-2 design reconstructs owner-death recovery from
a sidecar `flock` plus a shared tri-state DIRTY word. Windows needs none of
that — a named mutex whose owner dies hands the next waiter `WAIT_ABANDONED`,
which *is* the robust semantic, from the OS, with the "state may be
inconsistent" signal included. **On the single hardest primitive, Windows is
better equipped than a platform mpedb already supports.**

**Futexes have a direct equivalent.** `WaitOnAddress`/`WakeByAddressAll`
(Windows 8+) is a 1:1 replacement for `futex_wait`/`futex_wake_all`, not a
degradation. macOS had to fall back to polling; Windows does not.

## 2. What is genuinely different, and must be designed rather than mapped

Three things do not have a clean one-liner, and they are where the real work is:

1. **You cannot delete or replace an open file.** Unix code freely unlinks and
   renames files other processes hold open; Windows refuses without
   `FILE_SHARE_DELETE`, and even then the semantics differ. Every place the
   engine or the WAL replaces a file needs auditing — this is the classic source
   of ported-database bugs, and it is behavioural, not a missing API.
2. **Sharing mode on open is mandatory and unforgiving.** A file opened without
   `FILE_SHARE_READ | FILE_SHARE_WRITE` cannot be opened by a second process at
   all. Getting this wrong turns the headline property — several processes on
   one database — into an `ERROR_SHARING_VIOLATION` that looks like corruption.

   *(Stage 2: a non-issue, and worth recording why. Rust's `OpenOptions`
   defaults `share_mode` to `FILE_SHARE_READ | FILE_SHARE_WRITE |
   FILE_SHARE_DELETE`, so the engine's plain `File::open` already asks for
   everything. The trap is real for code that reaches for `CreateFileW`
   directly, which `wincompat` deliberately does not — it opens through `std`
   and only drops to Win32 for the mapping and the lock.)*
3. **The crash harnesses assume `fork` + signals.** `stress`, `crash`,
   `powerloss`, `collide`, `mirror-collide` — nine CLI files — are how
   crash-safety is *proven*, and they are POSIX to the bone. A Windows port that
   cannot run them is a port whose central claim is untested there. Rewriting
   them on `CreateProcess` + `TerminateProcess` is a project of its own, and it
   is the honest gate on calling Windows "supported" rather than "compiles".

~~Point 3 is the real cost, and it is larger than the `os.rs` work.~~

**Wrong, and by a wide margin — this is the prediction the port got least
right.** "POSIX to the bone" was written from the presence of `SIGKILL` in the
source, not from counting. Counted: `mpedb-cli` contains **no `fork` at all**.
Every harness already spawns with `std::process::Command`; the entire Unix
dependency is two patterns, and both are one function each:

- the child kills ITSELF (`kill(getpid(), SIGKILL)`) to hit a precise point in
  the code — `TerminateProcess` on our own handle is the same thing;
- the parent asks `status.signal() == SIGKILL` — Windows reads an exit code.

Those are `os::hard_kill_self` and `os::died_by_hard_kill`. The only other
Unix cluster in the CLI is `line.rs`'s terminal handling (`isatty`, `poll`,
`TIOCGWINSZ`), which has nothing to do with crash-safety and degrades to the
plain line reader the non-tty path already uses.

The measurement that would have prevented the wrong estimate is the one §0
already performed on the engine and this section did not perform on the CLI:
count the symbols, do not read the prose.

## 3. "Server" is a framing worth correcting

mpedb has no server, on any platform, by design: processes attach the file
directly (CLAUDE.md's no-server contract). Running "a server on Windows" means
one of two things:

- **the embedded engine on a Windows host** — that is this document, and
  applications get the same API they get on Linux; or
- **the service layer** (`DESIGN-SERVICE.md`: queues, cron, webhooks) — which is
  ordinary tables plus a task runner, and needs nothing from Windows that the
  engine does not already need.

So there is no separate "server port". There is an engine port, and everything
else follows it.

## 4. Staging, if this is ever picked up

1. ~~**`os.rs` Windows arm + `Shm` open/map path.**~~ **DONE.** `wincompat.rs`
   (hand-declared `extern "system"` kernel32 bindings — no `windows-sys`
   dependency) plus Windows arms in `os.rs`. 86 engine unit tests pass.
2. ~~**Multi-process attach.**~~ **DONE.** `crates/mpedb-core/tests/multiproc_attach.rs`
   — mapping coherence, writer exclusion, cross-process MVCC snapshots, owner
   death. Fork-free: it re-invokes its own binary via `std::process` and hard-kills
   with `Child::kill` (`TerminateProcess` here, `SIGKILL` on Unix), so the SAME
   test is the gate on both platforms.

   Two things came out differently than planned. The writer lock is
   `LockFileEx`, **not** the named mutex §1 recommends: `not(target_os =
   "linux")` already routes to the macOS FLD-2 shape (a sidecar advisory lock
   plus the shared tri-state DIRTY word), and `LockFileEx` drops into that
   position with owner-death release from the kernel, where a named mutex would
   have needed a second recovery path for the same property. And the reader
   sweep needed a new seam: `shm.rs` asked `kill(pid, 0)` and compared
   `last_os_error()` to `ESRCH`, which on Windows reads the raw Win32 code and
   never matches any errno — so every dead pid answered "alive" and the sweep
   reclaimed nothing. That is now `os::pid_definitely_dead`, one-sided by
   construction, and the Windows arm answers it BETTER than Linux does: it sees
   through a terminated-but-still-referenced process, which on Linux is known
   issue #136.
3. ~~**Durability + recovery.**~~ **DONE.** Two `multiproc_attach` arms kill a
   writer at `durability = commit` and `= wal`, then have a third process
   reopen the file — so the all-or-nothing answer comes from the disk and the
   log, not from a mapping that survived. The facade's ~1100 integration tests
   run too (see the sweep note above).

   The substantive finding was in boot-epoch recovery, and it was a live
   defect: `boot_id()` on Windows derived the boot instant as `wall clock −
   GetTickCount64`, two clocks that move independently, and `post_attach`
   compared it for byte equality. An NTP correction therefore looked like a
   reboot — and boot recovery WIPES the reader table while holding only the
   file lock, which an already-attached reader does not hold, so a live
   reader's pin would be dropped and the writer could reclaim pages under it.
   The question is now a predicate, `os::boot_id_matches`, and Windows answers
   it from the tick counter's reset (exact) plus a bounded epoch tolerance.

   **The dangerous residual is closed, and not by a better clock.** The
   reachable window was spurious recovery — a wall-clock step (VM snapshot
   resume, laptop wake, first NTP correction) making the predicate say "reboot"
   on a machine that never rebooted, and boot recovery then wiping a live
   reader's pin. `post_attach` now refuses to act on a mismatch while any
   reader slot names a live process with matching identity, and on Windows that
   identity carries an absolute process CREATION FILETIME — a stale entry
   surviving a real reboot would need the same pid created at the same
   100-nanosecond instant. The clock can now be wrong at no cost.

   The other window — a missed reboot — needs a whole boot-to-boot cycle under
   ten seconds, an interval in which nothing was durably committed to miss, and
   it stays open deliberately. The registry route (`PrefetchParameters\BootId`)
   was considered and refused: it is the prefetcher's counter, and whether it
   still increments where prefetching is disabled cannot be measured from this
   project's machines or from a CI runner that never reboots. Trading a stated
   window for an unmeasured assumption is not an improvement.
4. **The crash harnesses on `CreateProcess`/`TerminateProcess`.** Two seam
   functions, not a rewrite — see §2's correction. `crash` runs on Windows:
   12 children terminated at their chosen instants, all counted as killed,
   owner-death recovery observed, every invariant held including the index
   probe and the page-accounting verifier. `stress` and `queue-collide` too.

   **And it immediately found something that is not a Windows problem.**
   `collide` — concurrent writer processes at `durability = commit|wal` —
   corrupted the btree and freelist, and it did so on LINUX, on the tagged
   v0.1.4, and on v0.1.3 before it. The engine's own verifier reported it
   (`double free of page N`, `page reachable twice`). Splitting: the DIRECT
   path (`durability = none`) was clean, one writer was clean, process death
   was NOT the trigger, and neither was the detached-plan path — what was left
   was the intent ring.

   **#160, and it is fixed** (DESIGN.md §5.3): the leader's per-intent undo
   trusted the executor's `partial` flag, which is honest about ROWS and says
   nothing about a B-tree split. A failing statement COWs the leaf it descends
   into; from a transaction the batch has already made dirty, that copy stays
   linked while the rollback re-offers it. 0/8 across five previously-failing
   cells, and `collide` is in the Windows job.

   That is the argument for stage 4 stated more sharply than the original text
   managed: the harnesses are not a Windows chore, they are the only thing that
   exercises this, and CI does not run them because they are CLI subcommands
   rather than tests.

   **The one genuinely Windows-shaped defect in stage 4 was in the kill
   itself.** A child can die hard two ways — it kills ITSELF at a chosen
   instant (`crash`, `collide`, `tier`), or the PARENT kills it (`powerloss`,
   `queue-collide`, `mirror-collide`). On Unix both are SIGKILL, so one
   predicate covers them. On Windows they are different exit codes, because
   `Child::kill` is `TerminateProcess(handle, 1)` — and 1 is also the CLI's
   "runtime error" code, so "we killed it" and "it failed on its own" become
   the same observation. `powerloss` reported every worker as having "hit an
   error before the kill", with an empty stderr, which is what that collision
   looks like from the outside.

   The fix is `os::hard_kill_child`, which terminates with the SAME code
   `hard_kill_self` uses, so the harness controls both sides of the
   distinction rather than trying to infer it. **All six harnesses run in CI on
   `windows-latest`.**
5. ~~**`windows.yml` stops being a portable-crates job.**~~ **DONE**, and it
   grew two things stage 4 did not cover, both product surfaces rather than
   engine internals:

   * **The CLI end to end** — create-on-first-write, DDL, insert, aggregate
     read, `prepare`, `dump`, `DROP TABLE`. Building `mpedb-cli` is not the
     same as it working, and every step goes through the path handling that
     #159 found four wrong spellings of.
   * **The Python binding, as a wheel** — `windows-python` builds it with
     maturin, installs it, and runs the suite TWICE against one directory,
     which is its documented mode (that is how it tests persistence across
     process restarts). `pypi.yml` gained the matching `wheel-windows` job;
     the note there saying a Windows port was "an engine project, not a CI
     matrix entry" was true when written and is not any more.

   **And the sqlite interop lock, which was the last gated-out feature.** The
   15 overlay tests were skipped on Windows on the claim that "sqlite's Windows
   VFS uses its own locking protocol, with different byte offsets and a
   different shared/pending/reserved scheme". That claim was **wrong**, and
   checking it against the amalgamation this repo already pins took minutes:
   the lock bytes are defined once in sqlite's core, not per-VFS, and its own
   comment says the range is shared across platforms on purpose so win/unix
   clients *could* interoperate. `winGetReadLock` locks `SHARED_FIRST ..
   +SHARED_SIZE` shared; `winCheckReservedLock` probes `RESERVED_BYTE` with a
   shared try-lock it releases at once — which is exactly `getlk_free`, since
   Windows has no `F_GETLK`. The arm is `LockFileEx`/`UnlockFileEx` over the
   same bytes, and Windows is STRONGER than the POSIX path in two ways: its
   locks belong to the handle (so the [R#5] in-process-close trap cannot reach
   them, and `ofd()` reports `true`), and they are mandatory, which is harmless
   because sqlite's pager never allocates the page those bytes live in.

   The gate was not free. While it was up, a fifth unescaped-path site (#159's
   class) sat in the overlay's own `overlay_toml` — production code, not a
   test — where a Windows path made the config fail to PARSE with an error
   about unicode escapes. The tests that would have caught it were the ones
   the gate switched off.

Stages 1–3 were tractable and mostly mechanical against an abstraction that
exists. Stage 4 was the one to budget for, and it decided whether the claim on
the tin is true on Windows — it does, and the bug it found was ours on every
platform, not Windows'.

**The sweep that was predicted, and the edit that replaced it.** ~200 test
files hand-build config text with an unescaped path — 244 interpolation sites
across five spellings — and the estimate above was that stage 3 had to touch
every one of them. It touched none.

Two facts about `std::path` on Windows, verified there rather than assumed:
it accepts `/` as a separator, and `PathBuf::push` skips adding one when the
base already ends in a separator. So `mpedb_testkit::scratch_base` now hands
back `C:/Users/x/Temp/` — forward slashes, trailing separator — and every
`base.join(name)` downstream keeps that spelling. Without the trailing
separator the same base comes back as `C:/Users/x/Temp\db.mpedb`, one
backslash, and the config no longer parses. That single function took the
facade suite from "the config never parses" to 1364 passing.

What it does NOT cover is a path from anywhere else — a user, a fixture,
`current_exe()`. Those still need `mpedb_testkit::toml_path`, and the four
production sites still need `mpedb_types::toml_escape`. The lesson is narrower
than "no sweep was needed": the sweep was avoidable *because the test paths are
ours to spell*.

## 5. Running the engine tests on REAL Windows

This is what settles it, and it is now wired: `windows.yml` has a
`windows-engine` job separate from `windows-portable-core`, running stage 1
(`cargo test -p mpedb-core --lib`) and stage 2 (`--test multiproc_attach`).

Separate jobs, per stage, was the right call and stays the rule. The portable
crates are a settled guarantee whose red means a regression; the engine job is a
port in progress whose red means an unfinished stage. One job would have made
those indistinguishable, and the workspace-wide `cargo test --workspace` this
section originally proposed would have been red for reasons that have nothing to
do with the engine — see the sweep noted in §4.

## 6. What Wine bought — the first assessment was wrong

`scripts/test-windows-wine.sh` runs the portable crates' tests as Windows `.exe`
under Wine. First run, 2026-07-26: **360 tests, 0 failures.** From that I wrote
that Wine "found nothing", that it would keep finding nothing, and that pointing
it at the engine would produce failures nobody could act on. The first claim was
accurate about the portable crates. The rest did not survive the engine.

**Pointed at the engine, its first run found a production bug.** 28 of 86 tests
failed, all on one cause: a `.mpedb` path interpolated into a TOML `path = "…"`
line unescaped. `C:\Users\…` makes `\U` a TOML unicode escape, so the config is
a *parse error* — and the error names TOML, not paths, so nothing about it
points at the cause. Four production call sites had the same shape and three
were wrong in three different ways:

| Site | Before |
|---|---|
| `mpedb-capi/src/lib.rs` | `\\`+`"` escaped — correct, and the only one |
| `mpedb-cli/src/openpath.rs` | same |
| `mpedb-cli/src/util.rs` | **no escaping at all** |
| `mpedb-py/src/lib.rs` | `\\`→`/`, and quotes **deleted** — silently opens a different file |

All four now call `mpedb_types::toml_escape`. None of this is Windows-specific:
the Python binding would drop a quote out of a Unix path just as happily. It is
Windows-specific only in that no Unix path contains a backslash, so nothing ever
exercised it.

**The corrected reading.** Wine is not a Windows substitute, and the caution
below is still right about *why*:

> Wine is most trustworthy exactly where we need it least, and least trustworthy
> exactly where we would want it most.

What that misses is that a **red** result is cheap and actionable even when a
green one proves nothing. Every failure Wine reported here was ours, none were
Wine's, and the debugging loop — rebuild, rerun, ten seconds — is not something
CI can offer. The right role is a fast local screen that catches the portable
mistakes (path handling, string encoding, missing symbols) before the slow
authoritative run, with real Windows in Actions keeping the verdict.

Stages 1 and 2 were developed against it in an afternoon. That is the measurement.

## 7. What we do instead, today

Two cheaper things already run, and they cover the failure mode that has
actually occurred (twice: `b044c37`, and a nightly on 2026-07-26):

- **Cross-compilation on every push** (`linux.yml`) — the portable crates are
  compiled *for* Windows in ten seconds. This is what catches a portable crate
  acquiring a Unix dependency, which needs a Windows *target*, not a Windows
  *runtime*. Proven to catch what the older `cargo tree` guard misses.
- **`scripts/test-windows-wine.sh`** — §6.

Neither touches the engine, and neither pretends to.
