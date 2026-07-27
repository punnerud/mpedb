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
//! **The cap does NOT compose with a global editor limit — that was an
//! artefact of the benchmark.** Splitting a paragraph into 20 blocks and
//! putting 50 editors on each did not give 20× the capacity, and neither did
//! the *unguarded* control, so the guard was not what was in the way. The
//! ceiling was one commit per edit. Fold K edits into one commit and the edit
//! rate rises 76× on a two-core box and 106× on an M3, while commits run
//! SLOWER on both:
//!
//! ```text
//! K edits/commit:      1       8      64      256
//! Linux commits/s:   197     171      71       59
//! Linux edits/s:     197    1370    4525    14988     (76x)
//! M3 commits/s:     1178    1072     794      490
//! M3 edits/s:       1178    8577   50828   125333    (106x)
//! ```
//!
//! So there is one rule, `editors on one block <= cap`, plus a design
//! obligation: an edit must not be a commit of its own. The answer an editor
//! waits for is *did my edit win* — a question about conflict, not durability —
//! and those can be answered at different times. Byte-range conflict units and
//! acknowledge-on-claim are filed (see design/DESIGN-COLLAB.md §3), not built.
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
    /// The edit landed, authoritatively.
    Committed,
    /// The edit stands **locally** and no authority has confirmed it (#157).
    ///
    /// Only a [`Role::Replica`](crate::sync::Role::Replica) produces this;
    /// standalone and authority instances never do, because for them a local
    /// commit *is* the authoritative one. It is a third state rather than a
    /// flavour of `Committed` on purpose: folding it into either of the other
    /// two reads as the opposite of what happened — the same mistake
    /// `RangeClaim` made for one compile in #151.
    ///
    /// [`EditVerdictExt::is_committed`] stays false for it, so a caller that
    /// does not care about roles keeps today's meaning without changing a line.
    Provisional { local_txn: u64 },
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

/// One editor's pending sub-edit, as a service collects them before flushing
/// (#153).
///
/// # `snap` and `seq` are different numbers, and confusing them is easy
///
/// `snap` is **the engine's** version: which commit this editor had read when
/// it decided. It drives the guard and the rebase walk — where the bytes were.
///
/// `seq` is **the service's** order: in which order the editors acted. It
/// decides who wins a conflict and how equal offsets interleave. The engine
/// never assigns it; it only sorts by `(seq, editor)`, which is a total order
/// with no ties (#155).
///
/// A batch may mix several `snap`s. It is guarded against the oldest, so
/// nobody's decision is silently treated as newer than it was — but each member
/// is *rebased* from its own, because a member that already saw a commit must
/// not have that commit's delta applied to it twice (#154).
#[derive(Debug, Clone)]
pub struct Submission {
    pub editor: i64,
    pub snap: u64,
    /// The service's ordering counter. See the type docs: this is not `snap`.
    pub seq: u64,
    /// Which row of the block table.
    pub key: i64,
    pub at: u64,
    pub remove: u64,
    pub insert: String,
}

/// A member that has been applied, in ORIGINAL (pre-batch) coordinates.
///
/// Kept as an explicit set rather than a watermark because members are applied
/// in counter order, so the applied set is not a prefix of the offset axis.
struct Applied {
    at: u64,
    end: u64,
    delta: i64,
}

impl Database {
    /// **Apply a batch of sub-edits in ONE transaction, and answer each one.**
    ///
    /// This is the primitive a collaborative service is owed. Measured, a
    /// commit costs ~2 ms against ~35 µs of execution, so one edit per commit
    /// wastes ~98 % of the work — and folding K edits into one transaction was
    /// worth 79× (`benchmarks/documents.md`, F-batch). A guarded session cannot
    /// group-commit with other processes (#152: it holds the writer lock and
    /// never reaches the intent ring), so the batching has to be done by
    /// whoever is collecting the edits. This is that call.
    ///
    /// # Offsets are rebased WITHIN the batch, and that is the whole subtlety
    ///
    /// Every submitter computed its `at` against the block as they were shown
    /// it. Two of them in one transaction would otherwise both splice at
    /// pre-batch coordinates, and the second would land on the wrong bytes —
    /// silently, because it is not an error, it is a wrong answer.
    ///
    /// So each member is shifted by the length delta of the members applied
    /// before it **that lie at a lower original offset** — which is exactly the
    /// condition "did that edit move my bytes". `splice()`'s engine-side rebase
    /// (#151) walks the committed ring and handles everything that landed
    /// *before* this batch; this loop handles the batch itself. The two compose
    /// and must not be conflated — one counts committed transactions, the other
    /// counts members.
    ///
    /// # The order is the counter's, not the network's (#155)
    ///
    /// Members are applied in `(seq, editor)` order. That is a total order the
    /// submitters carried with them, so **the same set of submissions produces
    /// the same text no matter what order they arrived in** — which is what
    /// lets a client predict the result before its round trip completes.
    ///
    /// Sorting by offset instead (as this did before #155) only looks
    /// equivalent: disjoint splices commute, so the text is the same either
    /// way. What changes is *who wins a conflict* and *how equal offsets
    /// interleave*, and deciding those by arrival order means deciding them by
    /// network jitter.
    ///
    /// # Nobody starves
    ///
    /// A member with a high counter still lands — it is *rebased*, not
    /// rejected. Only a genuine overlap loses, and it loses **alone**: a
    /// savepoint per member means one collision does not take the batch with
    /// it. Being slow is not what costs you an edit; wanting the same bytes is.
    pub fn submit_batch(
        &self,
        table: &str,
        col: &str,
        subs: &[Submission],
    ) -> Result<Vec<EditVerdict>> {
        if subs.is_empty() {
            return Ok(Vec::new());
        }
        // The submitters' own order. `(seq, editor)` is total — two members
        // cannot tie — so nothing here depends on when they arrived.
        let mut order: Vec<usize> = (0..subs.len()).collect();
        order.sort_by_key(|&i| (subs[i].seq, subs[i].editor));

        // The oldest snapshot in the set: guarding against a newer one would
        // silently forgive a decision made against a version that had already
        // moved. Note this guards the TRANSACTION; each member is rebased from
        // its own snapshot below, which is a different question (#154).
        let snap = subs.iter().map(|s| s.snap).min().unwrap_or(0);

        let read = format!("SELECT {col} FROM {table} WHERE id = $1");
        let write = format!("UPDATE {table} SET {col} = splice({col}, $1, $2, $3) WHERE id = $4");
        let mut declared: Vec<[Value; 4]> = Vec::with_capacity(subs.len());
        for s in subs {
            declared.push([
                Value::Int(s.at as i64),
                Value::Int(s.remove as i64),
                Value::Text(s.insert.clone()),
                Value::Int(s.key),
            ]);
        }
        let rp = [Value::Int(subs[0].key)];
        let mut may_run: Vec<(&str, &[Value])> = vec![(read.as_str(), &rp[..])];
        for d in &declared {
            may_run.push((write.as_str(), &d[..]));
        }

        let mut out = vec![EditVerdict::Committed; subs.len()];
        let mut session = self.begin_guarded_with(snap, &may_run)?;
        // Every member applied so far, in ORIGINAL (pre-batch) coordinates.
        //
        // This is an explicit set rather than a running sum and a watermark,
        // and that is forced by the counter order (#155): the applied members
        // are no longer a prefix of the offset axis, so a watermark would both
        // miss real overlaps and invent false ones. O(n²) at n ≤ 256 is nothing
        // against one commit, and a Fenwick tree would buy speed we do not need
        // with clarity we do.
        //
        // **Shifting without the overlap test is silently wrong**, and the case
        // that caught it is worth keeping in mind: member A rewrites [0,4) and
        // member B wants [2,4). Shifting B by A's delta relocates it onto A's
        // own inserted text — a perfectly valid splice, on bytes B never saw.
        // No error, wrong answer.
        //
        // Same shape as the engine's committed-path rule (#151): overlap is a
        // collision, everything else is a shift. Half-open, so a zero-width
        // insert exactly at a predecessor's end does NOT overlap — both land,
        // in counter order, which is the answer a collaborative editor wants.
        let mut applied: Vec<Applied> = Vec::with_capacity(subs.len());
        let mut any = false;
        for &i in &order {
            let s = &subs[i];
            let end = s.at.saturating_add(s.remove);
            if applied.iter().any(|a| a.at < end && s.at < a.end) {
                out[i] = EditVerdict::Lost { at_txn: self.snapshot_txn() };
                continue;
            }
            // A member moved my position exactly when the bytes it REMOVED lie
            // at or before it. In offset order this was a running prefix sum;
            // in counter order it is the condition stated directly.
            //
            // The test is on the predecessor's END, not its start, and the
            // difference is the equal-offset case: two people typing at one
            // cursor both have `at == mine`, neither removes anything, and the
            // one that acted first must push the other along. `a.at < s.at`
            // would leave them both at the same point and silently reverse
            // them, so the later counter's text would come out first.
            let shift: i64 = applied.iter().filter(|a| a.end <= s.at).map(|a| a.delta).sum();
            let at = match (s.at as i64).checked_add(shift) {
                Some(v) if v >= 0 => v,
                // A member shifted out of the value by its predecessors is a
                // collision, not a coordinate to repair.
                _ => {
                    out[i] = EditVerdict::Lost { at_txn: self.snapshot_txn() };
                    continue;
                }
            };
            let sp = session.savepoint();
            // #154: rebased from THIS member's snapshot, not the batch's oldest.
            // The guard stays at the oldest — that is what makes the
            // transaction's refusal honest — but a member that already saw a
            // commit must not have that commit's delta applied to it twice.
            let r = session.query_from_snapshot(
                s.snap,
                &write,
                &[
                    Value::Int(at),
                    Value::Int(s.remove as i64),
                    Value::Text(s.insert.clone()),
                    Value::Int(s.key),
                ],
            );
            match r {
                Ok(_) => {
                    applied.push(Applied {
                        at: s.at,
                        end,
                        delta: s.insert.len() as i64 - s.remove as i64,
                    });
                    any = true;
                }
                // A real overlap, or an offset the value cannot hold. One
                // member's problem, rolled back to just before it — WHEN that
                // rollback is exact (#162). From the second member onward the
                // session is dirty, so a failing splice that allocated leaves
                // the copy linked while the rollback re-offers it (#160). The
                // whole batch fails rather than committing a page two trees can
                // reach; the caller resubmits, which is what it already does
                // for a `WriteConflict` on the batch as a whole.
                Err(Error::WriteConflict) | Err(Error::TypeMismatch(_)) => {
                    if !session.undo_is_exact(&sp) {
                        return Err(Error::Unsupported(
                            "submit_batch: a member failed after allocating from \
                             a dirty transaction, so the per-member undo is not \
                             exact (#162). The batch is refused rather than \
                             committed with a shared page — resubmit it."
                                .into(),
                        ));
                    }
                    session.rollback_to(sp);
                    out[i] = EditVerdict::Lost { at_txn: self.snapshot_txn() };
                }
                Err(e) => return Err(e),
            }
        }
        if !any {
            session.rollback();
            return Ok(out);
        }
        match session.commit() {
            Ok(()) => {
                // #157: on a replica the commit that just happened is LOCAL.
                // Saying `Committed` would be claiming an authority's word this
                // instance does not have, and a caller who acted on that would
                // be told later that it was withdrawn — which is a bug report
                // rather than a design.
                if self.role.needs_confirmation() {
                    let local_txn = self.snapshot_txn();
                    for v in out.iter_mut() {
                        if *v == EditVerdict::Committed {
                            *v = EditVerdict::Provisional { local_txn };
                        }
                    }
                }
                Ok(out)
            }
            // The batch as a whole lost to something outside it. Everyone who
            // was still standing learns it at once.
            Err(Error::WriteConflict) => {
                let at_txn = self.snapshot_txn();
                for v in out.iter_mut() {
                    if *v == EditVerdict::Committed {
                        *v = EditVerdict::Lost { at_txn };
                    }
                }
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }
}

/// Reading a verdict without matching on it — a batch's caller usually wants
/// "did it land", not which of the two ways it did not.
pub trait EditVerdictExt {
    fn is_committed(&self) -> bool;
}

impl EditVerdictExt for EditVerdict {
    fn is_committed(&self) -> bool {
        matches!(self, EditVerdict::Committed)
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

  [[table.column]]
  name = \"seq\"
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
            "INSERT OR REPLACE INTO edit_lease (block, editor, pid, pid_start, beat_at, seq) \
             VALUES ($1, $2, $3, $4, $5, 0)",
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
    ///
    /// # The heartbeat carries the counter (#155)
    ///
    /// `seq` is how far this editor has got. Reporting it here is what lets a
    /// service flush on a **quorum** with no deadline at all: an editor with
    /// nothing to say does not need a separate "abstain" message, because its
    /// heartbeat already says where it is. Having nothing to add IS a heartbeat
    /// with an unchanged `seq`.
    ///
    /// That moves the only remaining clock off the commit path and onto
    /// membership, where the deployment already controls it — which is the
    /// shape etcd uses, and the reason the flush needs no timer.
    pub fn beat(&self, block: i64, editor: i64, now_ms: i64, seq: u64) -> Result<bool> {
        let (pid, start) = own_identity();
        let mut s = self.db.begin()?;
        s.query(
            "UPDATE edit_lease SET beat_at = $1, seq = $2 \
             WHERE block = $3 AND editor = $4 AND pid = $5 AND pid_start = $6",
            &[
                Value::Int(now_ms),
                Value::Int(seq as i64),
                Value::Int(block),
                Value::Int(editor),
                Value::Int(pid),
                Value::Int(start),
            ],
        )?;
        s.commit()?;
        self.holds(block, editor, now_ms)
    }

    /// Who currently holds a seat on `block`, and how far each has got.
    ///
    /// This is the quorum input for a timerless flush (#155): the membership is
    /// a **known set** — lease holders — rather than something inferred from
    /// arrivals, which is what makes "a majority has spoken" a condition that
    /// can actually be reached without a clock. Expired seats are excluded, so a
    /// crashed editor stops counting after its TTL rather than blocking the
    /// block forever; that TTL is the only time left in the design.
    ///
    /// It also answers the one question a reconnecting client cannot work out on
    /// its own: `max(seq)` is where the document had got to while it was away.
    pub fn spoken(&self, block: i64, now_ms: i64) -> Result<Vec<(i64, u64)>> {
        let cutoff = now_ms - self.ttl.as_millis() as i64;
        match self.db.query(
            "SELECT editor, seq FROM edit_lease WHERE block = $1 AND beat_at >= $2",
            &[Value::Int(block), Value::Int(cutoff)],
        )? {
            crate::ExecResult::Rows { rows, .. } => Ok(rows
                .iter()
                .filter_map(|r| match (&r[0], &r[1]) {
                    (Value::Int(e), Value::Int(s)) => Some((*e, (*s).max(0) as u64)),
                    _ => None,
                })
                .collect()),
            _ => Ok(Vec::new()),
        }
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
