//! The intent ring (design/DESIGN.md §5.3): Phase-2 deterministic batch scheduling.
//!
//! Write requests arrive as `(plan_hash, params)` intents in a fixed table of
//! shared-memory slots. Whoever holds the writer lock is the *leader*: it
//! drains READY intents, executes them all inside one write transaction, and
//! commits them with a single meta flip (group commit — one msync amortized
//! over the whole batch in durable mode). Enqueuers wait on a per-slot futex
//! with a bounded timeout and promote themselves to leader on expiry
//! (wait-or-lead), so a SIGKILLed leader can never strand its waiters.
//!
//! **Flip-atomic consumption.** Before the meta flip the leader stamps every
//! drained slot with `committed_in_txn = N+1` and its result fields. A
//! successor leader compares each stamp against the committed `meta.txn_id`:
//! `stamp <= txn_id` means the batch landed (post the already-written result,
//! wake the waiter); `stamp > txn_id` means the flip never happened (clear the
//! stamp; the intent re-executes from scratch — safe, nothing became
//! visible). This per-slot stamping replaces the design's single
//! `committed_batch_seq` counter: it needs no contiguity, so a slow enqueuer
//! mid-publish never stalls intents behind it.
//!
//! Slot lifecycle (header word = {pid:u32 ‖ gen:30 ‖ state:2}; every
//! transition is a CAS and every transition to EMPTY bumps the generation).
//! The leader posts the RESULT before the READY→DONE transition, and the
//! owner may release from either READY or DONE — so no recovery path ever
//! needs to touch a DONE slot, which is what makes posting incarnation-safe
//! (a READY slot with a nonzero stamp cannot be released or re-reserved
//! until its result is posted).
//!
//! ```text
//! EMPTY ──enqueuer CAS──▶ RESERVED ──owner publishes payload──▶ READY
//!   ▲                        │ (owner died: leader reclaims)      │ stage+flip+post
//!   │                        ▼                                    ▼
//!   └── owner release (from READY or DONE) / leader reclaims dead-owner slots
//! ```
//!
//! Slot layout (1024 B):
//! ```text
//!   0  header        AtomicU64  {pid ‖ gen|state}
//!   8  result_state  AtomicU32  futex word: 0 pending, 1 posted
//!  12  err_code      AtomicU32  0 = ok (see facade codec for kinds)
//!  16  affected      AtomicU64
//!  24  committed_in  AtomicU64  txn stamp (0 = not staged)
//!  32  plan_hash     [u8; 32]   owner-written
//!  64  params_len    u16 LE     owner-written
//!  72  params        [u8; 824]  owner-written
//! 896  err_msg_len   u16 LE     leader-written
//! 898  err_msg       [u8; 126]  leader-written
//! ```

use crate::shm::{Shm, RING_SLOTS, RING_SLOT_SIZE};
use mpedb_types::PlanHash;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

const O_HEADER: usize = 0;
const O_RESULT_STATE: usize = 8;
const O_ERR_CODE: usize = 12;
const O_AFFECTED: usize = 16;
const O_COMMITTED_IN: usize = 24;
const O_PLAN_HASH: usize = 32;
const O_PARAMS_LEN: usize = 64;
const O_PARAMS: usize = 72;
const O_ERR_MSG_LEN: usize = 896;
const O_ERR_MSG: usize = 898;

/// Maximum serialized parameter bytes an intent can carry; larger requests
/// fall back to the direct writer-lock path.
pub const RING_PARAMS_CAP: usize = O_ERR_MSG_LEN - O_PARAMS; // 824
pub const RING_ERR_MSG_CAP: usize = RING_SLOT_SIZE - O_ERR_MSG; // 126

const ST_EMPTY: u64 = 0;
const ST_RESERVED: u64 = 1;
const ST_READY: u64 = 2;
const ST_DONE: u64 = 3;

#[inline]
fn pack(pid: u32, gen: u64, state: u64) -> u64 {
    ((pid as u64) << 32) | ((gen & 0x3FFF_FFFF) << 2) | state
}

#[inline]
fn unpack(word: u64) -> (u32, u64, u64) {
    ((word >> 32) as u32, (word >> 2) & 0x3FFF_FFFF, word & 0b11)
}

/// Raw cross-process futex wait: returns after a wake, a value change, or the
/// timeout — callers always re-check state, so spurious returns are fine.
/// (Platform-specific; macOS polls — see `crate::os`.)
fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    crate::os::futex_wait(word, expected, timeout)
}

#[doc(hidden)]
pub fn ring_debug_pub(msg: String) {
    ring_debug(format_args!("{msg}"));
}

#[doc(hidden)]
pub fn ring_debug(msg: std::fmt::Arguments<'_>) {
    static ENABLED: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_DEBUG_RING").is_ok());
    if !*ENABLED {
        return;
    }
    use std::io::Write;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let line = format!("{t} [{}] {msg}\n", crate::os::process_id());
    let _ = std::io::stderr().write_all(line.as_bytes());
}

fn futex_wake_all(word: &AtomicU32) {
    crate::os::futex_wake_all(word)
}

/// A staged or posted intent outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingResult {
    pub affected: u64,
    /// (code, message) — 0 means success; the facade owns the code mapping.
    pub err_code: u32,
    pub err_msg: Vec<u8>,
}

/// A READY intent collected by the leader. `word` is the slot's exact READY
/// header at collect time: posting is a CAS on it, so a stale leader whose
/// slot moved on (picked up, released, re-used) can never poison the new
/// incarnation with a spurious result.
pub struct PendingIntent {
    pub idx: u32,
    pub word: u64,
    pub hash: PlanHash,
    pub params: Vec<u8>,
}

/// View over the shared intent-slot region. Enqueuer methods are safe from
/// any process; methods marked *leader* must only be called while holding the
/// writer lock (single leader ⇒ no leader-leader races by construction).
pub struct IntentRing<'a> {
    shm: &'a Shm,
}

impl<'a> IntentRing<'a> {
    pub fn new(shm: &'a Shm) -> IntentRing<'a> {
        IntentRing { shm }
    }

    #[inline]
    fn header(&self, idx: u32) -> &AtomicU64 {
        self.shm.atomic_u64_at(self.shm.ring_slot_off(idx) + O_HEADER)
    }

    #[inline]
    fn result_state(&self, idx: u32) -> &AtomicU32 {
        self.shm.atomic_u32_at(self.shm.ring_slot_off(idx) + O_RESULT_STATE)
    }

    #[inline]
    fn err_code(&self, idx: u32) -> &AtomicU32 {
        self.shm.atomic_u32_at(self.shm.ring_slot_off(idx) + O_ERR_CODE)
    }

    #[inline]
    fn affected(&self, idx: u32) -> &AtomicU64 {
        self.shm.atomic_u64_at(self.shm.ring_slot_off(idx) + O_AFFECTED)
    }

    #[inline]
    fn committed_in(&self, idx: u32) -> &AtomicU64 {
        self.shm.atomic_u64_at(self.shm.ring_slot_off(idx) + O_COMMITTED_IN)
    }

    fn payload(&self, idx: u32, field_off: usize, len: usize) -> &mut [u8] {
        self.shm
            .bytes_at_unchecked(self.shm.ring_slot_off(idx) + field_off, len)
    }

    // ------------------------------------------------------ enqueuer side

    /// Reserve a slot, publish the intent, mark it READY. Returns the slot
    /// index and the owned header word, or None when the table is full (or
    /// the params exceed [`RING_PARAMS_CAP`]) — callers then fall back to the
    /// direct writer-lock path.
    pub fn enqueue(&self, hash: &PlanHash, params: &[u8]) -> Option<(u32, u64)> {
        if params.len() > RING_PARAMS_CAP {
            return None;
        }
        let pid = crate::os::process_id();
        // randomized scan start decorrelates concurrent enqueuers
        let start = pid.wrapping_mul(2654435761) % RING_SLOTS;
        for i in 0..RING_SLOTS {
            let idx = (start + i) % RING_SLOTS;
            let w = self.header(idx).load(Ordering::Acquire);
            let (_, gen, state) = unpack(w);
            if state != ST_EMPTY {
                continue;
            }
            let reserved = pack(pid, gen.wrapping_add(1), ST_RESERVED);
            if self
                .header(idx)
                .compare_exchange(w, reserved, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue; // lost the race; move on (plenty of slots)
            }
            // owner-only initialization
            self.result_state(idx).store(0, Ordering::Relaxed);
            self.err_code(idx).store(0, Ordering::Relaxed);
            self.affected(idx).store(0, Ordering::Relaxed);
            self.committed_in(idx).store(0, Ordering::Relaxed);
            self.payload(idx, O_PLAN_HASH, 32).copy_from_slice(&hash.0);
            self.payload(idx, O_PARAMS_LEN, 2)
                .copy_from_slice(&(params.len() as u16).to_le_bytes());
            self.payload(idx, O_PARAMS, params.len()).copy_from_slice(params);
            let ready = pack(pid, gen.wrapping_add(1), ST_READY);
            match self.header(idx).compare_exchange(
                reserved,
                ready,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    ring_debug(format_args!("enqueue idx={idx} word={ready:#x}"));
                    return Some((idx, ready));
                }
                // reclaimed from under us mid-publish (only possible if a
                // sweeper judged us dead — never touch the slot again)
                Err(_) => return None,
            }
        }
        None
    }

    /// Wait for the leader to post our result. Returns None on timeout (the
    /// caller then tries to become leader itself — wait-or-lead).
    pub fn wait_result(&self, idx: u32, timeout: Duration) -> Option<RingResult> {
        if self.result_state(idx).load(Ordering::Acquire) == 0 {
            futex_wait(self.result_state(idx), 0, timeout);
        }
        if self.result_state(idx).load(Ordering::Acquire) == 0 {
            return None;
        }
        ring_debug(format_args!(
            "take idx={idx} header={:#x} aff={} err={}",
            self.header(idx).load(Ordering::Relaxed),
            self.affected(idx).load(Ordering::Relaxed),
            self.err_code(idx).load(Ordering::Relaxed)
        ));
        Some(self.read_result(idx))
    }

    /// Non-blocking result check (used after a leadership round).
    pub fn try_take_result(&self, idx: u32) -> Option<RingResult> {
        if self.result_state(idx).load(Ordering::Acquire) == 0 {
            return None;
        }
        Some(self.read_result(idx))
    }

    fn read_result(&self, idx: u32) -> RingResult {
        let err_code = self.err_code(idx).load(Ordering::Acquire);
        let affected = self.affected(idx).load(Ordering::Acquire);
        let err_msg = if err_code != 0 {
            let raw: [u8; 2] = self.payload(idx, O_ERR_MSG_LEN, 2)[..].try_into().unwrap();
            let len = u16::from_le_bytes(raw) as usize;
            self.payload(idx, O_ERR_MSG, len.min(RING_ERR_MSG_CAP)).to_vec()
        } else {
            Vec::new()
        };
        RingResult {
            affected,
            err_code,
            err_msg,
        }
    }

    /// Release our slot after picking up the result. `owned` is the READY
    /// header returned by enqueue; the poster moves it to DONE with the same
    /// pid+gen, but we may pick the result up in the window before that
    /// transition — release from either state.
    pub fn release(&self, idx: u32, owned: u64) {
        let (pid, gen, _) = unpack(owned);
        let empty = pack(0, gen.wrapping_add(1), ST_EMPTY);
        for state in [ST_DONE, ST_READY] {
            if self
                .header(idx)
                .compare_exchange(
                    pack(pid, gen, state),
                    empty,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return;
            }
        }
        // already reclaimed — nothing to do
    }

    // -------------------------------------------------------- leader side

    /// *Leader.* Resolve orphans from a dead predecessor: slots whose stamp
    /// says they were part of a batch. `committed_txn` is the current
    /// committed `meta.txn_id`.
    pub fn recover_orphans(&self, committed_txn: u64) {
        for idx in 0..RING_SLOTS {
            let w = self.header(idx).load(Ordering::Acquire);
            let (_, _, state) = unpack(w);
            let stamp = self.committed_in(idx).load(Ordering::Acquire);
            if stamp == 0 {
                continue; // fresh incarnation (enqueue resets the stamp)
            }
            if state != ST_READY {
                // DONE slots need no recovery: post_done publishes the result
                // BEFORE the DONE transition, so a DONE slot's waiter already
                // has (or can read) its result. Acting on DONE slots here was
                // a confirmed TOCTOU: a stale header + the NEW incarnation's
                // zeroed result_state let a recover poison a fresh intent.
                continue;
            }
            if stamp <= committed_txn {
                ring_debug(format_args!(
                    "recover-ready idx={idx} word={w:#x} stamp={stamp} committed={committed_txn}"
                ));
                // the batch committed but the leader died before posting:
                // the staged result fields are authoritative. READY+stamp≠0
                // is pinned to its incarnation (the owner cannot release
                // before the result is posted), so this cannot mis-post.
                self.post_done(idx, w);
            } else {
                // the flip never landed: the intent must re-execute
                self.committed_in(idx).store(0, Ordering::Release);
            }
        }
    }

    /// *Leader.* All executable intents, in slot order (deterministic).
    pub fn collect_ready(&self) -> Vec<PendingIntent> {
        let mut out = Vec::new();
        for idx in 0..RING_SLOTS {
            let w = self.header(idx).load(Ordering::Acquire);
            let (_, _, state) = unpack(w);
            if state != ST_READY || self.committed_in(idx).load(Ordering::Acquire) != 0 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(self.payload(idx, O_PLAN_HASH, 32));
            let raw: [u8; 2] = self.payload(idx, O_PARAMS_LEN, 2)[..].try_into().unwrap();
            let len = u16::from_le_bytes(raw) as usize;
            if len > RING_PARAMS_CAP {
                continue; // corrupt publish; leave for dead-owner reclaim
            }
            out.push(PendingIntent {
                idx,
                word: w,
                hash: PlanHash(hash),
                params: self.payload(idx, O_PARAMS, len).to_vec(),
            });
        }
        out
    }

    /// *Leader.* Stage an intent's outcome BEFORE the meta flip. The stamp
    /// `next_txn` makes consumption atomic with the flip.
    pub fn stage_result(
        &self,
        idx: u32,
        affected: u64,
        err_code: u32,
        err_msg: &[u8],
        next_txn: u64,
    ) {
        self.affected(idx).store(affected, Ordering::Relaxed);
        self.err_code(idx).store(err_code, Ordering::Relaxed);
        if err_code != 0 {
            let len = err_msg.len().min(RING_ERR_MSG_CAP);
            self.payload(idx, O_ERR_MSG_LEN, 2)
                .copy_from_slice(&(len as u16).to_le_bytes());
            self.payload(idx, O_ERR_MSG, len).copy_from_slice(&err_msg[..len]);
        }
        // Release: a successor that Acquire-reads the stamp sees the fields
        self.committed_in(idx).store(next_txn, Ordering::Release);
    }

    /// *Leader.* Un-stage after an aborted batch (flip never happened).
    pub fn unstage(&self, idx: u32) {
        self.committed_in(idx).store(0, Ordering::Release);
    }

    /// *Leader (under the writer lock).* Publish the result and wake the
    /// waiter, then transition READY→DONE.
    ///
    /// Safety of the result store: this is only called for slots that are
    /// READY **with a nonzero stamp** (staged by this leader under the
    /// current lock, or verified by `recover_orphans`). Such a slot is pinned
    /// to its incarnation — its owner cannot release before this very store,
    /// and nobody can re-reserve a non-EMPTY slot — so the store can never
    /// land in a newer incarnation. The result store comes FIRST so a leader
    /// dying mid-post leaves a READY slot whose waiter already has its
    /// result (the waiter releases from READY; no recovery arm needed for
    /// DONE slots).
    pub fn post_done(&self, idx: u32, ready_word: u64) {
        let (pid, gen, state) = unpack(ready_word);
        debug_assert_eq!(state, ST_READY);
        ring_debug(format_args!(
            "post idx={idx} word={ready_word:#x} aff={} err={} stamp={}",
            self.affected(idx).load(Ordering::Relaxed),
            self.err_code(idx).load(Ordering::Relaxed),
            self.committed_in(idx).load(Ordering::Relaxed)
        ));
        self.result_state(idx).store(1, Ordering::Release);
        futex_wake_all(self.result_state(idx));
        // best-effort: the owner may already have released from READY
        let _ = self.header(idx).compare_exchange(
            ready_word,
            pack(pid, gen, ST_DONE),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// *Leader (amortized).* Reclaim slots whose owner died: RESERVED slots
    /// stuck mid-publish and DONE slots nobody will ever pick up.
    pub fn reclaim_dead(&self) {
        for idx in 0..RING_SLOTS {
            let w = self.header(idx).load(Ordering::Acquire);
            let (pid, gen, state) = unpack(w);
            if state != ST_RESERVED && state != ST_DONE {
                continue;
            }
            // wasm32: the intent ring only exists for durability commit|wal,
            // which the private path refuses outright — so this loop never
            // sees an occupied slot there. `crate::wasmcompat::kill` still
            // answers honestly (every pid but ours is gone) via its own errno,
            // because wasm has no OS errno for `last_os_error` to read.
            #[cfg(target_arch = "wasm32")]
            let dead = {
                let kill_failed = unsafe { crate::wasmcompat::libc::kill(pid as i32, 0) } != 0;
                kill_failed && crate::wasmcompat::errno() == crate::wasmcompat::libc::ESRCH
            };
            #[cfg(not(target_arch = "wasm32"))]
            let dead = {
                let kill_failed = unsafe { libc::kill(pid as i32, 0) } != 0;
                kill_failed && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            };
            if !dead {
                continue;
            }
            let _ = self.header(idx).compare_exchange(
                w,
                pack(0, gen.wrapping_add(1), ST_EMPTY),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::Shm;
    use mpedb_types::Durability;

    fn open_test(name: &str) -> (Shm, std::path::PathBuf) {
        let dir = std::env::temp_dir().join("mpedb-ring-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[9u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        (shm, p)
    }

    #[test]
    fn enqueue_stage_flip_post_pickup_cycle() {
        let (shm, p) = open_test("cycle");
        let ring = IntentRing::new(&shm);
        let hash = PlanHash([7u8; 32]);
        let (idx, owned) = ring.enqueue(&hash, b"PARAMS").unwrap();

        // leader collects it
        let ready = ring.collect_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].idx, idx);
        assert_eq!(ready[0].hash, hash);
        assert_eq!(ready[0].params, b"PARAMS");

        // stage (pre-flip), then post (post-flip)
        ring.stage_result(idx, 3, 0, &[], 42);
        assert!(ring.try_take_result(idx).is_none(), "not posted yet");
        // staged intents are no longer collectable
        assert!(ring.collect_ready().is_empty());
        ring.post_done(idx, owned);

        // a stale post with an outdated word must be a no-op
        ring.post_done(idx, owned);

        let r = ring.wait_result(idx, Duration::from_millis(1)).unwrap();
        assert_eq!(r.affected, 3);
        assert_eq!(r.err_code, 0);
        ring.release(idx, owned);

        // slot is reusable with a fresh generation
        let (idx2, _) = ring.enqueue(&hash, b"X").unwrap();
        let w = ring.header(idx2).load(Ordering::Acquire);
        let (_, _, state) = unpack(w);
        assert_eq!(state, ST_READY);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn orphan_recovery_committed_vs_uncommitted() {
        let (shm, p) = open_test("orphans");
        let ring = IntentRing::new(&shm);
        let h = PlanHash([1u8; 32]);
        let (i1, _) = ring.enqueue(&h, b"a").unwrap();
        let (i2, _) = ring.enqueue(&h, b"b").unwrap();

        // dead leader staged both for txn 5, but only "committed" up to 5
        ring.stage_result(i1, 1, 0, &[], 5);
        ring.stage_result(i2, 1, 0, &[], 6);

        ring.recover_orphans(5); // current committed txn = 5
        // i1's batch landed: posted
        assert!(ring.try_take_result(i1).is_some());
        // i2's flip never happened: unstaged, executable again
        assert!(ring.try_take_result(i2).is_none());
        let ready = ring.collect_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].idx, i2);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn error_results_roundtrip() {
        let (shm, p) = open_test("errors");
        let ring = IntentRing::new(&shm);
        let (idx, owned) = ring.enqueue(&PlanHash([2u8; 32]), b"").unwrap();
        ring.stage_result(idx, 0, 2, b"users\x1femail", 9);
        ring.post_done(idx, owned);
        let r = ring.try_take_result(idx).unwrap();
        assert_eq!(r.err_code, 2);
        assert_eq!(r.err_msg, b"users\x1femail");
        ring.release(idx, owned);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn dead_owner_reclaim_and_oversized_params() {
        let (shm, p) = open_test("reclaim");
        let ring = IntentRing::new(&shm);
        assert!(
            ring.enqueue(&PlanHash([3u8; 32]), &vec![0u8; RING_PARAMS_CAP + 1]).is_none(),
            "oversized params must be rejected"
        );
        // forge a RESERVED slot owned by a dead pid
        let w = pack(4_000_000, 9, ST_RESERVED);
        ring.header(0).store(w, Ordering::Release);
        ring.reclaim_dead();
        let (_, _, state) = unpack(ring.header(0).load(Ordering::Acquire));
        assert_eq!(state, ST_EMPTY, "dead RESERVED slot must be reclaimed");
        // a RESERVED slot owned by a LIVE pid must survive
        let w = pack(std::process::id(), 3, ST_RESERVED);
        ring.header(1).store(w, Ordering::Release);
        ring.reclaim_dead();
        assert_eq!(ring.header(1).load(Ordering::Acquire), w);
        ring.header(1).store(0, Ordering::Release); // cleanup
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn wait_or_lead_across_threads() {
        let (shm, p) = open_test("threads");
        let shm = std::sync::Arc::new(shm);
        let h = PlanHash([4u8; 32]);
        let (idx, owned) = IntentRing::new(&shm).enqueue(&h, b"payload").unwrap();

        // waiter thread: waits, times out at least once, then gets the result
        let shm2 = shm.clone();
        let waiter = std::thread::spawn(move || {
            let ring = IntentRing::new(&shm2);
            loop {
                if let Some(r) = ring.wait_result(idx, Duration::from_millis(2)) {
                    ring.release(idx, owned);
                    return r;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        // "leader" executes and posts
        let ring = IntentRing::new(&shm);
        let batch = ring.collect_ready();
        assert_eq!(batch.len(), 1);
        let word = batch[0].word;
        ring.stage_result(idx, 7, 0, &[], 1);
        ring.post_done(idx, word);
        let r = waiter.join().unwrap();
        assert_eq!(r.affected, 7);
        std::fs::remove_file(&p).unwrap();
    }
}
