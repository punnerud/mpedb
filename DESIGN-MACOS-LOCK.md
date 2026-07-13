# macOS crash-safe writer lock — FINAL design (FLD-2: Flock + Local ERRORCHECK mutex + tri-state Dirty word)

Verified against the tree: `LA_MUTEX=64` reserves bytes 64..192 (next field `LA_DURABLE_TXN=192`); the shared pthread-mutex init at `shm.rs:1801-1812` and `1962-1972` is **not** cfg-gated (it writes the macOS mutex signature over offset 64); `writer_lock`/`try_writer_lock`/`writer_unlock`/`recover_after_owner_death` sit at `shm.rs:847/869/918/924` with the exact EOWNERDEAD/EDEADLK arms; `os.rs` `proc_start_time`→`Some(0)` and `boot_id`←`KERN_BOOTTIME`; DB/WAL files open **without** `O_CLOEXEC` (`OpenOptionsExt` already imported at `shm.rs:15`).

The lock is three primitives: a **sidecar `flock`** (kernel crash-release = death oracle + cross-process rendezvous), a **process-private `PTHREAD_MUTEX_ERRORCHECK`** (intra-process exclusion + re-entrancy→EDEADLK), and one **tri-state shared word `DIRTY`** (the recovered signal). It carries **no pid, no start-time, no generation, no CAS, no `__ulock`**. Every adversarial finding is folded in as a concrete fix or a named accepted limitation.

## 1. Shared-state layout (all `cfg(target_os="macos")`)

Placed inside the ex-pthread region (64..192), free on macOS **only after** the pthread-mutex init is cfg-gated to Linux (fix M4 — else `pthread_mutex_init` stamps `_PTHREAD_MUTEX_SIG_init=0x32AAABA7` over offset 64 on every format/boot-reinit and the first writer spuriously reads `DIRTY!=0`):

```
LA_WRITER_DIRTY  AtomicU32 @ 64   // 0=CLEAN, 1=DIRTY (owner in a critical section),
                                  // 2=POISONED (a prior owner-death recovery FAILED = ENOTRECOVERABLE)
LA_WLOCK_DEV     u64      @ 72    // st_dev of the sidecar inode, recorded at format
LA_WLOCK_INO     u64      @ 80    // st_ino of the sidecar inode, recorded at format
// 88..192 spare. Offsets 192.. (durable_txn, boot_id, reader table, opt-ring) UNCHANGED.
```

4/8-byte aligned in a page-aligned MAP_SHARED region ⇒ true cross-process arm64 atomics (LSE `CASAL`/`SWPAL`, inner-shareable domain — same mechanism as the reader `{pid,seq}` words). On-disk geometry byte-identical to Linux; only the *use* of 64..192 differs under cfg.

**Per-process (`crate::os::WriterLock`, one instance per `(dev,ino)` per process — see M5):**
```
wl_fd     : RawFd            // open("<db>.wlock", O_RDWR|O_CREAT|O_CLOEXEC, 0600) + explicit FD_CLOEXEC;
                             //   held for the Shm lifetime; a DEDICATED inode => flock namespace
                             //   DISJOINT from FlockGuard(self.file).
local_mtx : *pthread_mutex_t // PTHREAD_MUTEX_ERRORCHECK, process-private, NOT pshared/robust;
                             //   ONE object shared by all in-process handles to this DB.
```

Core invariant, guaranteed by the acquire/release ordering:
> **`flock(wl_fd)` free AND `DIRTY==1` ⇔ the previous holder died inside its critical section.**

## 2. Acquire (blocking + non-blocking)

```
// post_acquire runs holding BOTH local_mtx AND flock => exclusive owner, before any engine write.
fn post_acquire(&self) -> Result<bool> {
    match DIRTY.load(Acquire) {
        0 => { DIRTY.store(1, Release); Ok(false) }                 // clean acquire, enter my section
        1 => match self.recover_after_owner_death() {               // took over a dead section
                 Ok(())  => { DIRTY.store(1, Release); Ok(true) }   // recovered=true, exactly once
                 Err(e)  => { DIRTY.store(2, Release);              // POISON (fix L1): do NOT clear
                              self.wl_release_exclusion();           // flock UN + local unlock, keep DIRTY=2
                              Err(e) }
             },
        _ => { self.wl_release_exclusion();                         // 2 = poisoned: fail-closed forever
               Err(Internal("writer lock unrecoverable: prior owner-death recovery failed \
                    (ENOTRECOVERABLE); DB wedged pending operator recovery")) }
    }
}

fn writer_lock(&self) -> Result<bool> {                             // BLOCKING
    match pthread_mutex_lock(local_mtx) {                           // level 1 FIRST: re-entrancy pre-flock
        0 => {}
        EDEADLK => return Err(Internal("writer lock re-entered by its owner (nested write transaction)")),
        rc      => return Err(Internal("local_mtx lock: {rc}")),
    }
    loop {                                                          // level 2: kernel rendezvous
        let rc = flock(wl_fd, LOCK_EX);                             // wakes on release OR holder death-teardown
        if rc == 0 { break; }
        if errno == EINTR { continue; }                            // re-blocks; no hot-spin
        pthread_mutex_unlock(local_mtx);
        return Err(Internal("flock LOCK_EX"));
    }
    post_acquire()                                                 // on Err it already released both levels
}

fn try_writer_lock(&self) -> Result<Option<bool>> {                // NON-BLOCKING
    match pthread_mutex_trylock(local_mtx) {
        0 => {}
        EDEADLK => return Err(Internal("writer lock re-entered by its owner (nested write transaction)")),
        EBUSY   => return Ok(None),                                // another thread of this process
        rc      => return Err(Internal("local_mtx trylock: {rc}")),
    }
    let rc = flock(wl_fd, LOCK_EX | LOCK_NB);
    if rc != 0 {
        let e = errno; pthread_mutex_unlock(local_mtx);
        if e == EWOULDBLOCK { return Ok(None); }                   // a LIVE process holds it (also the brief
        return Err(Internal("flock LOCK_EX|NB"));                  // death-teardown window, L5: retry converges)
    }
    post_acquire().map(Some)
}
```

**Ordering rules (load-bearing):** `local_mtx` is taken **before** `flock` (re-entrancy caught before any `flock`, which would otherwise re-grant a held `LOCK_EX` and double-grant a nested `begin_write`). `flock` is taken **before** the section; `DIRTY=1` is stored in `post_acquire` **before** returning to the engine (before any COW write).

## 3. Release

```
fn writer_unlock(&self) {                                          // best-effort, infallible
    let _ = DIRTY.compare_exchange(1, 0, Release, Relaxed);        // clear MY clean section; never resurrect POISON(2)
    self.wl_release_exclusion();
}
fn wl_release_exclusion(&self) {
    loop { if flock(wl_fd, LOCK_UN) == 0 || errno != EINTR { break; } }  // fix L2: retry LOCK_UN on EINTR
    pthread_mutex_unlock(local_mtx);                              // rc ignored
}
```

`DIRTY→0` is stored (CAS 1→0) **while still holding `flock`** and **before** `flock(UN)`. The next grantee observes `DIRTY` only after the kernel grants it the lock, so a **cleanly-released** holder is always seen as `DIRTY==0` ⇒ `recovered=false`; a **dead-in-section** holder as `DIRTY==1`. `flock` is a full barrier; with Release/Acquire + inner-shareable coherence the store is visible to the successor. The engine calls `writer_unlock` only *after* the commit's meta-flip + `msync`/`F_FULLFSYNC` returned, so `DIRTY=0` truthfully means "nothing to recover."

## 4. Takeover = an ordinary acquire

There is **no separate takeover routine** — that is the whole point of a kernel file lock. When holder **H** is SIGKILLed mid-section: (1) the kernel closes H's `wl_fd`, **auto-releasing H's `flock`** (keyed on the open-file-description, immune to pid reuse/EPERM/clock); (2) `DIRTY` is still `1`. A survivor blocked in `flock(LOCK_EX)` is granted the lock (kernel grants exactly one waiter), reads `DIRTY==1`, runs `recover_after_owner_death()` **under exclusion, before any write**, returns `Ok(true)`. If it dies before its clean unlock, `DIRTY` stays 1 and the next grantee re-runs the **idempotent** recover — never lost, never wedged.

## 5. How `recovered` flows (unchanged wiring)

macOS `writer_lock`/`try_writer_lock` run `recover_after_owner_death()` internally at the identical site as the Linux EOWNERDEAD arm and return `Ok(true)`/`Ok(Some(true))`. The bool flows `Engine::begin_write/try_begin_write → make_write_txn(recovered) → WriteTxn.recovered` exactly as on Linux; no caller changes. `recover_after_owner_death`, `sweep_dead_readers`, the meta double-buffer, `wal_recover`, and the ring protocol are untouched — the lock only delivers the bool under exclusion.

## 6. Correctness argument

**Mutual exclusion.** Two levels. Intra-process: `local_mtx` (ERRORCHECK) serializes threads, yields `EDEADLK` on same-thread re-lock. Cross-process: a single sidecar inode's `flock(LOCK_EX)` is granted to exactly one waiter; `try_*` losers get `EWOULDBLOCK`→`Ok(None)`. `DIRTY` is read/written **only while holding `flock`**, so two processes are never simultaneously in `post_acquire`; single-writer (COW `page_mut`, freelist, ring-leader reads) holds through every takeover race. *Threats closed:* split-inode double-grant (M1 — sidecar `(dev,ino)` recorded at format, re-`fstat`ed at attach, hard-error on drift + never-unlink contract); inherited-OFD co-holding (H1/M2 — `O_CLOEXEC`+`FD_CLOEXEC`+`pthread_atfork` child-close; enforced "no fork-without-exec while attached"); non-`flock` substrate (NFS/SMB — documented hard constraint, optional `statfs` refusal).

**Exactly-once recovery.** `DIRTY==1` covers precisely the incomplete-section interval: set in `post_acquire` before any COW write, cleared (CAS 1→0) in `writer_unlock` only after the commit's meta-flip+`msync` returned (verified `engine.rs:1444-1475`). Read only under the single-grant `flock`, so among N contenders exactly one observes the signal per death. A taker that dies mid-recover leaves `DIRTY==1` for the next grantee to re-run the **idempotent** `recover_after_owner_death` (`shm.rs:924-954` = `msync_range(0,2·PAGE)` for Commit / no-op for WAL, then a monotone `durable_txn.fetch_max` — no active rollback; half-applied COW never flipped meta so "newest valid meta = last commit" is stable across repeats). **Death-at-any-instant** (t0 flock-grant … t3 DIRTY=1 … [write+commit+meta-flip+msync] … t4 DIRTY=0 … t5 flock-UN … t6 local-unlock): no instant yields a *missed* recovery; the only deviation is one **benign idempotent extra `recover()`** in the tiny post-durable/pre-`DIRTY=0` (t4) window, and `recovered` is diagnostic-only (`collide.rs:212`, `crash.rs:140`) so a spurious `true` has zero engine effect. **Failed recovery (L1):** poison `DIRTY=2` (ENOTRECOVERABLE) — a deliberate improvement over the Linux path, which make-consistents-then-unlocks and *loses* the signal; here subsequent acquirers hard-error rather than silently proceed on unrecovered durability state; cleared only by boot-reinit or operator recovery.

**Bounded progress (no wedge) — honestly scoped.** For **process death** (SIGKILL at any instant) takeover is bounded: the kernel tears down the fd table synchronously and wakes a `flock` waiter with no lost-wakeup (no dependence on `__ulock`, which is not a cross-process wake channel on macOS). *Not covered, by parity with the Linux robust mutex:* a **live-but-hung/SIGSTOPped** holder (stuck `msync`/`F_FULLFSYNC`, `SIGSTOP`) keeps its OFD open and wedges on **both** platforms — EOWNERDEAD fires only on death, `flock` releases only on last-fd-close. Accepted limitation, not "bounded no-wedge for all states." **Per-writer fairness** is unspecified (`flock` grants unordered); system-wide progress always holds and there is only one writer by design, so a dead holder is still taken over promptly. Optional ticket turnstile if per-caller starvation is ever observed (L4).

**PID reuse.** The lock stores no pid and reads no start-time; takeover is authorized solely by the kernel releasing `flock` on the *real* death of the *real* incarnation (per-OFD), which a reused pid cannot forge. Re-entrancy uses `local_mtx`, a fresh per-incarnation object. (The reader-table sweep and ring reclaim remain pid-reuse-unsafe on macOS until `proc_start_time` is real — M3/L3, orthogonal to the lock but part of a fully crash-safe **port**; see verdict.)

**Reboot / clock.** `flock` kernel state and `local_mtx` do not survive reboot. The `post_attach` boot-id-change branch (`shm.rs:1941`, under `FlockGuard(self.file)`) gets a `cfg(macos)` reset of `DIRTY=0`. **Fix M6:** derive macOS boot identity from `sysctlbyname("kern.bootsessionuuid")` (reboot-stable per-boot UUID) instead of `KERN_BOOTTIME` (= `now-uptime`, re-derived on `settimeofday`/NTP steps) — else a clock step masquerades as a reboot and wipes reader/DIRTY state on a *live* DB; defense-in-depth, gate the wipe behind acquiring `wl_fd` `flock` so it cannot stomp a live writer's `DIRTY`.

## 7. Five-outcome mapping (matches the `shm.rs` contract)

| Outcome | `writer_lock` | `try_writer_lock` | Trigger |
|---|---|---|---|
| clean acquire | `Ok(false)` | `Ok(Some(false))` | flock granted, `DIRTY==0` |
| acquire w/ recovery | `Ok(true)` | `Ok(Some(true))` | flock granted (dead holder auto-released), `DIRTY==1`, recover Ok |
| busy | — | `Ok(None)` | flock `EWOULDBLOCK` (live process) or `local_mtx` `EBUSY` (another thread) |
| re-entrant | `Err(Internal "…re-entered…")` | same | `local_mtx` `EDEADLK` |
| unrecoverable | `Err(Internal "…ENOTRECOVERABLE…")` | same | `DIRTY==2` (prior recover failed) — new, fail-closed |
| release | `writer_unlock()` infallible | same | CAS `DIRTY 1→0`; flock `UN` (EINTR loop); `local_mtx` unlock |

## 8. macOS primitives — used, and the ones that turned out UNusable

**Used:** `flock(LOCK_EX/UN/NB)` on a sidecar inode (mutual exclusion + kernel crash-release death oracle + cross-process rendezvous); process-private `PTHREAD_MUTEX_ERRORCHECK` (intra-process serialization + EDEADLK); shared `AtomicU32` in MAP_SHARED (the `DIRTY` word — confirmed truly cross-process on arm64); `O_CLOEXEC`/`FD_CLOEXEC` + `pthread_atfork` (fd-inheritance defense); `fstat` (sidecar identity); `proc_pidinfo(PROC_PIDTBSDINFO).pbi_start_tvsec` (real start time — for the reader sweep, M3); `sysctlbyname("kern.bootsessionuuid")` (reboot-stable boot id, M6); `fcntl(F_FULLFSYNC)` (platter durability — orthogonal to the lock).

**UNusable → fallback:** (a) **robust pthread mutex / `pthread_mutex_consistent`** — absent on macOS; replaced by `flock` crash-release + the `DIRTY` word synthesizing EOWNERDEAD/ENOTRECOVERABLE. (b) **`__ulock_wait/wake` as a cross-process futex** — XNU keys the wait queue on the caller's (task, VA), not the physical page, so a wake never rendezvous cross-process; replaced by **blocking `flock`** as the kernel wait (correctness never depends on a wakeup channel). (c) **`kill(pid,0)`+start-time as the takeover oracle** — necessary-not-sufficient under pid reuse; **not used** by the lock at all (superseded by `flock`'s per-OFD release). (d) **plain `fsync`** (current `os.rs` path) is not platter-durable; `F_FULLFSYNC` is the durability fallback behind the same cfg.

## 9. Accepted limitations (explicit)

- **Live-but-hung/SIGSTOPped holder wedges** — identical to the Linux robust mutex; out of scope for a robust-mutex-parity lock (needs an orthogonal heartbeat/watchdog).
- **Fork-without-exec** that inherits `wl_fd` without cooperating with the `pthread_atfork` child-close can co-hold the OFD; enforced API contract "no fork while a `WriteSession`/`Database` is live," plus the atfork handler. Strictly no worse than an app-level contract.
- **Local filesystem, single host, single PID namespace, single uid** (sidecar is 0600; `proc_pidinfo` is same-uid-or-root) — a documented hard constraint; the real crash-safe target is Linux, macOS is the single-host bench/dev build.
- **Benign extra idempotent `recover()`** in the t4 window; `recovered` is diagnostic-only so it has no engine effect.
- **`flock` grants are unordered** — per-writer wait is not FIFO-bounded under saturation (system-wide progress always holds).
