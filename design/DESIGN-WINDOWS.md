# DESIGN-WINDOWS — porting the engine, and why it is smaller than it looks

**Status: design (2026-07-26). Nothing is built.** Today Windows runs the
portable crates only (`mpedb-types`, `mpedb-sql`); `.github/workflows/windows.yml`
says so and is right about today. This document is about what the rest would
cost, because the usual answer — "the engine is Unix-only, so Windows is out" —
is true as a description and wrong as a prediction.

## 0. The claim this corrects

> "Full engine er Unix-only pga. mmap/flock/PROCESS_SHARED//proc. Kun portable
> crates kjører der."

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
3. **The crash harnesses assume `fork` + signals.** `stress`, `crash`,
   `powerloss`, `collide`, `mirror-collide` — nine CLI files — are how
   crash-safety is *proven*, and they are POSIX to the bone. A Windows port that
   cannot run them is a port whose central claim is untested there. Rewriting
   them on `CreateProcess` + `TerminateProcess` is a project of its own, and it
   is the honest gate on calling Windows "supported" rather than "compiles".

Point 3 is the real cost, and it is larger than the `os.rs` work.

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

1. **`os.rs` Windows arm + `Shm` open/map path.** The mechanical part. Ends at:
   one process opens a `.mpedb` on Windows, reads and writes, single-process.
2. **Multi-process attach.** Sharing modes, the named-mutex writer lock, the
   reader table's pid identity via `GetProcessTimes`. Ends at: two processes,
   one file, MVCC readers against a writer.
3. **Durability + recovery.** `FlushFileBuffers` ordering, the WAL, boot-epoch
   recovery. Ends at: `powerloss` equivalent passes.
4. **The crash harnesses on `CreateProcess`/`TerminateProcess`.** Until this
   passes, Windows is "compiles and runs", not "crash-safe" — and crash-safety
   is the product.
5. Only then does `windows.yml` stop being a portable-crates job.

Stages 1–3 are tractable and mostly mechanical against an abstraction that
exists. Stage 4 is the one to budget for, and the one that decides whether the
claim on the tin is true on Windows.

## 5. Running the engine tests on REAL Windows

This is the only thing that would settle it, and the CI side of it is trivial —
`windows.yml` would change one line, from

```yaml
run: cargo test -p mpedb-types -p mpedb-sql
```

to `cargo test --workspace`. That is the whole CI change. The 68 errors in §0
are what stand between, and after them stage 4's crash harnesses are what decide
whether the result deserves the word "supported".

There is no useful intermediate. A job that attempts the workspace today fails
on line one and teaches nothing, which is exactly what `windows.yml`'s header
rejects and why it is scoped the way it is. What *is* worth doing, if the port
is ever started, is running that job **per stage**: stage 1 turns `cargo build
-p mpedb-core` green, stage 2 adds the multi-process tests, and so on. Each
stage is a real gate rather than a promise.

## 6. What Wine actually bought — measured, and it is not much

`scripts/test-windows-wine.sh` runs the portable crates' tests as Windows `.exe`
under Wine. First run, 2026-07-26: **360 tests, 0 failures.** It found nothing.

That is the honest report, and it generalises. The two crates it can reach are
pure computation — tokenizer, binder, planner, plan codec, keycode, blake3.
There is no filesystem in them, no processes, no locale-dependent formatting on
the tested paths. So the class of bug Wine is *for* barely exists on the surface
Wine can reach.

And there is an inversion worth naming, because it decides how much to invest:

> **Wine is most trustworthy exactly where we need it least, and least
> trustworthy exactly where we would want it most.**

Pure computation under Wine is reliable and uninteresting. The engine — shared
mmap, cross-process locks, owner-death recovery, process identity — is the part
where a Windows port could genuinely go wrong, and it is also the part where
Wine's emulation is thinnest and a red result is most likely to be Wine's fault
rather than ours. Pointing Wine at the engine, once it compiles, would produce
failures nobody could act on with confidence.

So: keep the Wine script as a cheap regression net for the SQL front end, expect
it to keep finding nothing, and do not treat it as a step toward the port. The
port's verification is real Windows in Actions, per stage.

## 7. What we do instead, today

Two cheaper things already run, and they cover the failure mode that has
actually occurred (twice: `b044c37`, and a nightly on 2026-07-26):

- **Cross-compilation on every push** (`linux.yml`) — the portable crates are
  compiled *for* Windows in ten seconds. This is what catches a portable crate
  acquiring a Unix dependency, which needs a Windows *target*, not a Windows
  *runtime*. Proven to catch what the older `cargo tree` guard misses.
- **`scripts/test-windows-wine.sh`** — §6.

Neither touches the engine, and neither pretends to.
