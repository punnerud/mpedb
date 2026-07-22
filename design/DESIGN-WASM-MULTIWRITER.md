# DESIGN-WASM-MULTIWRITER — concurrent writers in a browser tab

**Status: DESIGN ONLY. Nothing here is built.** The shipped playground
(`web/`, `crates/mpedb-wasm`) is single-writer: one WebAssembly instance, one
thread, one private byte array. This document records what a multi-writer
browser demo would take, and — more importantly — the one thing that would have
to be *proven* before it could claim what it would appear to claim.

## 0. Why bother

mpedb's headline property is the one the playground cannot currently show:
several OS processes writing one database concurrently, any of which may be
`SIGKILL`ed mid-commit without corrupting it. Today a visitor has to take that
on faith from the README, or run `mpedb crash --waves 6` themselves.

A browser could do better than a benchmark table. `worker.terminate()` **is**
the `SIGKILL` case. A page with a *"kill a writer"* button — where the visitor
murders a worker mid-commit and then watches the database verify clean — would
be a better demonstration of the property than anything currently shipped,
because it is adversarial, interactive and impossible to fake.

Persistence is irrelevant to this. The tab dying takes the database with it;
that costs nothing, because what is being demonstrated is *consistency under
concurrent writers and abrupt death*, not durability.

## 1. What the browser actually provides

The primitives are better than the shipped page's honesty box originally
claimed, and the correction matters:

| need | browser primitive | verdict |
|---|---|---|
| one byte range visible to several contexts | `SharedArrayBuffer` | **real.** Each worker instantiates its own wasm module over the same SAB. |
| independent execution contexts | Web Workers | **real.** Each runs its own mpedb instance; they are as independent as processes for this purpose. |
| the reader-slot generation-CAS | `Atomics.compareExchange` | **real CAS**, same semantics the reader table needs. |
| the writer wait queue / futex | `Atomics.wait` / `Atomics.notify` | **a real futex.** `crate::os::futex_wait`/`futex_wake_all` map onto it directly. |
| memory ordering | wasm atomics are seq-cst | **stronger than needed**; DESIGN.md §4.3's `SeqCst` fence pairs are expressible. |
| **owner-death detection** | — | **MISSING. This is the whole problem.** |

## 2. The blocker: nobody tells you a writer died

Every mpedb writer-lock construction so far is built on a death oracle the
*kernel* provides:

- **Linux** — a `PTHREAD_MUTEX_ROBUST` `PROCESS_SHARED` mutex. The kernel hands
  the next acquirer `EOWNERDEAD`, which is the signal to run recovery and then
  `pthread_mutex_consistent`.
- **macOS (FLD-2, `design/DESIGN-MACOS-LOCK.md`)** — no robust mutex, so the
  oracle is a sidecar `flock`: the kernel releases it when the holder's fd
  closes, i.e. when the holder dies. The shared tri-state DIRTY word carries
  "a holder was in its critical section" across that release.

A browser has neither. `Atomics` give mutual exclusion but no notion of an
owner, so a worker terminated between "set DIRTY" and "publish meta" leaves the
lock word held by a context that will never release it, with no event that says
so. **Without a death oracle, the writer lock wedges and the demo's central
claim — that killing a writer is safe — is exactly the claim that is unproven.**

### 2.1 The available answer: the main thread is the supervisor

The browser's substitute is that a supervisor genuinely exists. The main thread
owns every `Worker` handle, it is the one calling `terminate()`, and it observes
`onerror`/exit. So it can play the role the kernel plays on macOS: the party
that knows, authoritatively, that a holder is gone.

That makes this construction **structurally closer to FLD-2 than to Linux's
robust mutex** — an external authority declaring death, plus a shared tri-state
word carrying whether the dead holder was mid-mutation. It is nevertheless a
**third lock construction**, not a port of either. Per this repo's practice
(DESIGN.md's protocols survived a 37-finding adversarial review, and the
ordering rules are load-bearing) that earns its own design pass and adversarial
review *before* any page claims it is sound.

### 2.2 What a review would have to settle

Non-exhaustive, and each is a place where a plausible-looking design is wrong:

1. **The supervisor is not trusted, only believed about death.** It must not be
   able to declare a *live* worker dead and thereby hand out a lock that is
   still held. What makes "terminated" one-way and non-forgeable?
2. **`terminate()` is not instantaneous** in the way `SIGKILL` is, and it is
   not specified to be atomic with respect to shared-memory writes in flight.
   What is the actual guarantee about a store that was executing?
3. **Recovery ordering.** The macOS path's correctness comes from a specific
   sequence (DIRTY set before mutation, cleared after publication, POISONED
   observed by the next acquirer). The equivalent here has to be re-derived,
   not assumed by analogy.
4. **The main thread can die too** — the tab is closed, or it is itself a
   worker. What happens to a database whose supervisor is gone?
5. **Reader identity.** The native protocol pairs `(pid, /proc start-time)` to
   survive PID reuse. Worker ids are reused within a page; the equivalent
   `(id, incarnation)` needs to be as strong.
6. **`Shm::is_private` must be turned OFF for this** — a shared SAB database is
   not the private path, so the `priv_pins`/`priv_meta` shortcuts and the
   `exclusive_write` in-place mode all become wrong. That is a much larger
   surface than the port that shipped.

## 3. The seam that exists today

The shipped port was written so this stays reachable. `crates/mpedb-core/src/wasmcompat.rs`
holds the entire backing store behind one accessor:

- A "file" is a `Box<[u8]>` in a virtual fd table; `mmap` hands back a pointer
  into it, and `pread`/`pwrite` address the same bytes. Nothing outside that
  module knows the storage is a `Box<[u8]>`.
- Replacing it with a view over a `SharedArrayBuffer` is therefore a change to
  `wasmcompat` alone — `shm.rs`, the B+tree, the freelist and the commit path
  are untouched, exactly as they were untouched by the single-writer port.
- `crate::os::futex_wait`/`futex_wake_all` already have wasm arms (currently
  no-ops, because a single-threaded module has nobody to wait for). They are
  the natural landing points for `Atomics.wait`/`notify`.
- `crate::os::WriterLock` already has a third arm (`wasm_lock`) that is a
  single-threaded flag. A shared construction slots in there, next to
  `macos_lock`, which is the right neighbourhood for it.

What does **not** exist yet: the wasm build assumes one thread throughout
(`wasmcompat`'s fd table is a plain `Mutex`, and `Shm`'s `Send`/`Sync` are
asserted on that basis), and `wasm32-unknown-unknown` needs the atomics +
bulk-memory features and a shared memory to have real `Atomics` at all — a
different build configuration, not a flag.

## 4. Deployment blocker: COOP/COEP

`SharedArrayBuffer` is gated behind cross-origin isolation, which requires two
response headers:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

**GitHub Pages cannot set response headers.** So the current deployment target
cannot host this at all as-is. The known workaround is `coi-serviceworker` — a
service worker that re-serves the page with the headers attached — which works
but adds a layer that itself has to be understood (first load is uncontrolled,
the worker must be same-origin, and it changes the caching story). Alternatives
are a host that allows headers (Cloudflare Pages, Netlify) or accepting that
this demo lives somewhere other than the docs site.

Noting it here rather than discovering it after the engine work: the hard part
is §2, but the *deployment* is a separate hard no.

## 5. Recommended order, if this is ever picked up

1. **Design + adversarial review of the lock first**, on paper, against §2.2.
   No code. If the death oracle cannot be made sound, everything below is
   wasted and the demo must not exist.
2. Build configuration: atomics/bulk-memory, shared memory, worker bootstrap.
3. `wasmcompat` backing → `SharedArrayBuffer`; `futex_*` → `Atomics`.
4. Turn `is_private` off for the shared path and re-derive what that re-enables
   (the real reader table, the checksummed dual metas).
5. Only then the kill button — and it should verify, visibly, rather than
   assert: run the page-accounting verifier after each kill and show the
   result, including if it ever fails.

Until step 1 concludes, the playground says plainly that it does not
demonstrate multi-process writers. A demo that *appears* to show crash-safe
concurrency without the protocol that makes it true would be the single most
damaging thing this repo could publish.
