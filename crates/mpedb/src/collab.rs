//! **Bounded feedback on an edit, and admission control (#150).**
//!
//! The guard (#142–#149) already answers *immediately* at commit: no lock is
//! held, nothing waits, and a conflicting action is refused rather than queued.
//! What it does not do is bound the **tail**. A refused editor rejoins the same
//! race it just lost, and optimistic retry has no fairness — arm F measured p50
//! on the think time and p99 at 2.2 seconds on one contended field.
//!
//! The contract this module adds is the one a person actually notices:
//!
//! > Everyone who submits an edit learns within *D* whether it landed.
//!
//! Three answers, and exactly three:
//!
//! * [`EditVerdict::Committed`] — it landed.
//! * [`EditVerdict::Lost`] — someone else got there first. Carries the txn that
//!   won, so the client re-reads *that* state and re-renders without polling.
//!   This is "first wins, and the losers know".
//! * [`EditVerdict::DeadlineExpired`] — no definite answer inside *D*. Not a
//!   conflict; an admission-control signal. Too many of these on one block mean
//!   its editor cap is set too high.
//!
//! Two entry points, because two different edits need different answers.
//! [`Database::submit_within`] takes the snapshot the client decided against
//! and tries **once** — retrying would re-apply a decision made against a value
//! that moved, which is precisely the lost update the guard exists to refuse.
//! [`Database::act_within`] re-reads and retries, for edits that are a function
//! of the current value rather than of what a person was shown.
//!
//! ## Why an editor cap exists at all
//!
//! Because the deadline is a promise about the tail, and the tail is a function
//! of how many editors share a **contention unit** — one block row. Measured on
//! a 2-core Linux box at a 1 s deadline: 32 editors on one block still meet it,
//! 64 do not.
//!
//! **The cap composes with a global limit, and that surprised the measurement.**
//! Splitting a paragraph into 20 blocks and putting 50 editors on each did NOT
//! give 20× the capacity: total throughput stayed flat, and so did the
//! *unguarded* control. The ceiling is the single writer lock, not the guard.
//! So an admission policy needs both halves — per block, and overall:
//!
//! ```text
//! editors on one block  <=  cap          (measured; ~32 at 1 s on that box)
//! editors in total      <=  D x global commit rate
//! ```
//!
//! Splitting removes *conflicts*; it does not raise the ceiling.
//!
//! ## Why the lease is not a lock
//!
//! Holding an edit lease does **not** mean your commit wins — first-committer
//! still decides. The lease caps *how many* editors contend for one block, so
//! the deadline stays meetable. Reading it as mutual exclusion would be reading
//! in the one thing this whole architecture does not have.

use std::time::{Duration, Instant};

use crate::{Database, Error, Result, Value, WriteSession};

/// What happened to an edit, inside its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditVerdict {
    /// The edit landed.
    Committed,
    /// Someone else committed to this surface first. `at_txn` is the newest
    /// committed transaction at the moment we were refused — the client reads
    /// its own snapshot at or after that point to see what won.
    Lost { at_txn: u64 },
    /// No definite answer inside the deadline. The block is hotter than its cap
    /// allows; this is the signal admission control regulates on, counted in
    /// `Database::guard_stats().4`.
    DeadlineExpired,
}

impl Database {
    /// **Submit an edit the client already decided on, and answer within
    /// `deadline`.** This is the shape the contract is about: a person read the
    /// block at `snap`, thought about it, typed, and pressed send. Fifty of them
    /// may do it at once; the first wins and the rest are told.
    ///
    /// One attempt, deliberately. Retrying would re-apply a decision made
    /// against a value that has since moved — the client must see what won and
    /// decide again. That is what [`EditVerdict::Lost`] carries `at_txn` for.
    ///
    /// `may_run` is the declared surface **with parameter values** (#148): bare
    /// SQL names every row of its table, which is sound but coarse enough to
    /// make one edit conflict with the whole document.
    pub fn submit_within<F>(
        &self,
        deadline: Duration,
        snap: u64,
        may_run: &[(&str, &[Value])],
        mut f: F,
    ) -> Result<EditVerdict>
    where
        F: FnMut(&mut WriteSession<'_>) -> Result<()>,
    {
        let start = Instant::now();
        if deadline.is_zero() {
            self.engine.record_deadline_expiry();
            return Ok(EditVerdict::DeadlineExpired);
        }
        let mut s = self.begin_guarded_with(snap, may_run)?;
        f(&mut s)?;
        let outcome = s.commit();
        // The clock is checked AFTER the attempt as well as before: an answer
        // that arrives late is not an answer inside the deadline, and reporting
        // it as one would make the contract untestable.
        if start.elapsed() > deadline {
            self.engine.record_deadline_expiry();
            return Ok(EditVerdict::DeadlineExpired);
        }
        match outcome {
            Ok(()) => Ok(EditVerdict::Committed),
            // The winner is whatever is committed now. Reporting the txn rather
            // than the value keeps this free of any assumption about what the
            // caller was editing — the client re-reads its own snapshot.
            Err(Error::WriteConflict) => Ok(EditVerdict::Lost { at_txn: self.snapshot_txn() }),
            Err(e) => Err(e),
        }
    }

    /// **Apply an edit that can be re-decided, retrying until `deadline`.**
    ///
    /// The closure re-reads on every attempt, so a retry is a fresh decision
    /// rather than a replay — which is what makes retrying safe here and unsafe
    /// in [`Database::submit_within`]. Use this for edits derived from the
    /// current value (a counter, an append), not for edits a person composed
    /// against a version they were shown.
    ///
    /// **An attempt that cannot finish inside the remaining budget is not
    /// started.** Otherwise the bound is broken by construction: the last
    /// attempt would run past the deadline and the caller would be told at
    /// D + one attempt. The estimate is the previous attempt's measured cost,
    /// which is the only honest one available and errs toward answering early.
    pub fn act_within<F>(
        &self,
        deadline: Duration,
        may_run: &[(&str, &[Value])],
        mut f: F,
    ) -> Result<EditVerdict>
    where
        F: FnMut(&mut WriteSession<'_>) -> Result<()>,
    {
        let start = Instant::now();
        // Nothing has been measured yet, so the first attempt always runs: an
        // action refused before it ever tried would report a deadline the
        // caller never actually spent.
        let mut last_attempt = Duration::ZERO;
        loop {
            let elapsed = start.elapsed();
            if elapsed >= deadline
                || (last_attempt > Duration::ZERO && deadline - elapsed < last_attempt)
            {
                self.engine.record_deadline_expiry();
                return Ok(EditVerdict::DeadlineExpired);
            }

            let t0 = Instant::now();
            let snap = self.snapshot_txn();
            let mut s = self.begin_guarded_with(snap, may_run)?;
            f(&mut s)?;
            let outcome = s.commit();
            last_attempt = t0.elapsed();

            match outcome {
                Ok(()) => return Ok(EditVerdict::Committed),
                Err(Error::WriteConflict) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

// ---------------------------------------------------------------- leases

/// The SQL an application needs for [`Lease`]. Kept as a constant rather than
/// created implicitly: the table is the application's, not the engine's, and a
/// schema this module created behind the caller's back would be a surprise in a
/// database whose whole premise is a rigid, declared schema.
pub const LEASE_SCHEMA: &str = "\
[[table]]
name = \"edit_lease\"
primary_key = [\"block\", \"editor\"]

  [[table.column]]
  name = \"block\"
  type = \"int64\"

  [[table.column]]
  name = \"editor\"
  type = \"int64\"

  [[table.column]]
  name = \"pid\"
  type = \"int64\"

  [[table.column]]
  name = \"pid_start\"
  type = \"int64\"

  [[table.column]]
  name = \"beat_at\"
  type = \"int64\"
";

/// Outcome of asking for an editing seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// You hold a seat on this block. Heartbeat to keep it.
    Admitted,
    /// The block is at capacity. You are a viewer for now — and you know it
    /// **immediately**, rather than discovering it as a deadline expiry after
    /// a second of trying.
    AtCapacity { holders: usize },
}

/// Admission control over one block's editing seats.
///
/// Modelled on the task queue's claim/lease/reap (`mpedb-cli/src/queue.rs`):
/// ordinary rows, a guarded update, and reclamation driven by an expiry plus a
/// liveness check. Rows rather than a shared-memory registry because this needs
/// no new cross-process primitive, survives SIGKILL for free, and makes "who is
/// editing right now" an ordinary `SELECT` — which a viewer's UI wants anyway.
pub struct Lease<'d> {
    db: &'d Database,
    /// A seat is stale once its heartbeat is this old.
    pub ttl: Duration,
}

impl<'d> Lease<'d> {
    pub fn new(db: &'d Database, ttl: Duration) -> Self {
        Self { db, ttl }
    }

    /// Take a seat on `block`, if the block is under `cap`.
    ///
    /// Reaping happens here, in the same transaction as the count — the party
    /// that benefits pays, which is why a database with no editors never sweeps
    /// (the same rule the notification registry follows, DESIGN-NOTIFY §2).
    ///
    /// **A seat is not a lock.** It bounds how many editors contend for the
    /// block so the feedback deadline stays meetable; it does not decide who
    /// wins a commit. First-committer still does.
    pub fn acquire(&self, block: i64, editor: i64, cap: usize, now_ms: i64) -> Result<Admission> {
        let cutoff = now_ms - self.ttl.as_millis() as i64;
        let mut s = self.db.begin()?;
        // Expired first: cheap, and it is what keeps a crashed editor from
        // holding a seat forever.
        s.query("DELETE FROM edit_lease WHERE block = $1 AND beat_at < $2", &[
            Value::Int(block),
            Value::Int(cutoff),
        ])?;
        let holders = Self::live_holders(&mut s, block, editor)?;
        if holders >= cap {
            // The reaping above is discarded with the rollback. That is fine
            // and deliberate: a refused admission must not be able to leave the
            // table in a state only it produced, and the next successful
            // acquire reaps again anyway.
            s.rollback();
            return Ok(Admission::AtCapacity { holders });
        }
        let (pid, start) = own_identity();
        s.query(
            "INSERT OR REPLACE INTO edit_lease (block, editor, pid, pid_start, beat_at) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                Value::Int(block),
                Value::Int(editor),
                Value::Int(pid),
                Value::Int(start),
                Value::Int(now_ms),
            ],
        )?;
        s.commit()?;
        Ok(Admission::Admitted)
    }

    /// Keep a seat. Guarded on `(pid, pid_start)` so a **reused pid cannot
    /// inherit someone else's seat** — the same guard shape the queue puts on
    /// `(claimed_by, claimed_at)`.
    ///
    /// Deliberately not on the commit path: a heartbeat is a small write every
    /// 10–15 seconds, and putting liveness work into the commit path is exactly
    /// what #147 had to undo.
    pub fn beat(&self, block: i64, editor: i64, now_ms: i64) -> Result<bool> {
        let (pid, start) = own_identity();
        let mut s = self.db.begin()?;
        s.query(
            "UPDATE edit_lease SET beat_at = $1 \
             WHERE block = $2 AND editor = $3 AND pid = $4 AND pid_start = $5",
            &[
                Value::Int(now_ms),
                Value::Int(block),
                Value::Int(editor),
                Value::Int(pid),
                Value::Int(start),
            ],
        )?;
        s.commit()?;
        self.holds(block, editor, now_ms)
    }

    /// Give the seat up. A client that exits cleanly should call this; a client
    /// that is SIGKILLed is covered by expiry plus the liveness check.
    pub fn release(&self, block: i64, editor: i64) -> Result<()> {
        let (pid, start) = own_identity();
        let mut s = self.db.begin()?;
        s.query(
            "DELETE FROM edit_lease \
             WHERE block = $1 AND editor = $2 AND pid = $3 AND pid_start = $4",
            &[
                Value::Int(block),
                Value::Int(editor),
                Value::Int(pid),
                Value::Int(start),
            ],
        )?;
        s.commit()
    }

    /// Does this process still hold `editor`'s seat on `block`?
    pub fn holds(&self, block: i64, editor: i64, now_ms: i64) -> Result<bool> {
        let (pid, start) = own_identity();
        let cutoff = now_ms - self.ttl.as_millis() as i64;
        match self.db.query(
            "SELECT beat_at FROM edit_lease \
             WHERE block = $1 AND editor = $2 AND pid = $3 AND pid_start = $4",
            &[
                Value::Int(block),
                Value::Int(editor),
                Value::Int(pid),
                Value::Int(start),
            ],
        )? {
            crate::ExecResult::Rows { rows, .. } => Ok(rows
                .first()
                .and_then(|r| match r[0] {
                    Value::Int(b) => Some(b >= cutoff),
                    _ => None,
                })
                .unwrap_or(false)),
            _ => Ok(false),
        }
    }

    /// Seats currently held on `block`, counting only editors whose process is
    /// still alive — and dropping the rows of those that are not.
    ///
    /// **Liveness errs toward ALIVE.** `pid_alive_identity` refuses to declare
    /// a process dead on `EPERM`, so an editor owned by another user is left
    /// alone. Getting this backwards would evict a live editor, which is a
    /// visible failure; leaving a dead one costs a seat until its TTL, which is
    /// not. Same asymmetry as #136/#147.
    fn live_holders(s: &mut WriteSession<'_>, block: i64, skip_editor: i64) -> Result<usize> {
        let rows = match s.query(
            "SELECT editor, pid, pid_start FROM edit_lease WHERE block = $1",
            &[Value::Int(block)],
        )? {
            crate::ExecResult::Rows { rows, .. } => rows,
            _ => return Ok(0),
        };
        let mut live = 0usize;
        let mut dead: Vec<i64> = Vec::new();
        for r in &rows {
            let (ed, pid, start) = match (&r[0], &r[1], &r[2]) {
                (Value::Int(e), Value::Int(p), Value::Int(st)) => (*e, *p, *st),
                _ => continue,
            };
            // Re-taking your own seat is a renewal, not a second editor.
            if ed == skip_editor {
                continue;
            }
            if mpedb_core::shm::pid_is_alive(pid as u32, start as u64) {
                live += 1;
            } else {
                dead.push(ed);
            }
        }
        for ed in dead {
            s.query(
                "DELETE FROM edit_lease WHERE block = $1 AND editor = $2",
                &[Value::Int(block), Value::Int(ed)],
            )?;
        }
        Ok(live)
    }
}

/// This process's `(pid, start-time)` pair — the identity a seat is held under.
/// The start time is what makes pid reuse harmless.
fn own_identity() -> (i64, i64) {
    let pid = std::process::id() as i64;
    let start = mpedb_core::shm::own_process_start_time().unwrap_or(0) as i64;
    (pid, start)
}
