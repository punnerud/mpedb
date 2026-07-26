//! **mpedb ⇄ mpedb sync: the same library on both ends (#157).**
//!
//! An application talks to *its own* `Database` through the ordinary API —
//! `query`, `execute`, [`WriteSession`], [`submit_batch`](crate::Database::submit_batch).
//! Whether that instance is alone, is a replica of another `.mpedb`, or is the
//! one others replicate from, is a **deployment fact**, not an API difference.
//! That is the whole promise of this module, and [`Role`] is where it is stated.
//!
//! # There is no new protocol here, and that is deliberate
//!
//! The changelog a replica needs already exists: mpedb-core's CDC dirty set
//! (`mpedb_core::cdc`). Each entry is keyed by `(table, pk-hash)` and carries
//! the **committing txn id** that dirtied it. So it is a *coalesced* changelog
//! with a per-entry watermark — exactly the shape a pull wants:
//!
//! * **Coalesced**, so a replica that has been offline for an hour replays one
//!   row image per changed row rather than every intermediate write. That is the
//!   property that makes long-offline catch-up cheap instead of linear in edits.
//! * **Watermarked**, so N replicas each keep their own cursor and never
//!   interfere: [`SyncLink::pull`] only ever *reads* the upstream's dirty set.
//!
//! The one consequence worth stating: because pull does not consume, nothing
//! trims the upstream's dirty set. [`SyncLink::gc_upstream`] does that, and it
//! must be given a watermark at or below the slowest live replica's cursor —
//! the same "oldest pinned" rule the freelist follows, for the same reason.
//!
//! # Two planes, because a row and a cell are different questions
//!
//! * **Row plane** ([`SyncLink::pull`] / [`SyncLink::push`]) — whole-row upsert
//!   and delete by primary key. This is "general sync": tables, records,
//!   ordinary application data.
//! * **Cell plane** — many editors inside *one* value. A whole-row push would
//!   clobber a concurrent sub-edit, so those travel as
//!   [`Submission`](crate::collab::Submission)s through
//!   [`submit_batch`](crate::Database::submit_batch), where `(seq, editor)`
//!   orders them and `splice()` rebases them (#151/#155). [`SyncLink::push_edits`]
//!   is that path.
//!
//! Mixing them on one column would be a bug: the row plane's last-writer-wins
//! would silently discard the sub-edit merge the cell plane just computed.
//! [`SyncLink::push`] therefore takes the tables it owns, and a table under the
//! cell plane must not be listed.
//!
//! # The topology this supports, and the one it does not
//!
//! **A star works**: N replicas around one upstream, in any direction. A change
//! written at replica A reaches replica B in two sync calls, one per link.
//!
//! **A relay chain does not, downstream.** If a hub is itself a replica of A and
//! also the upstream of B, then A's changes reach the hub and stop there. The
//! cause is structural rather than a missed case: [`SyncLink::pull`] applies
//! with capture **off** — it must, or every pulled row would push straight back
//! — so a row the hub *pulled* never enters the hub's own change log, and B's
//! pull reads exactly that log. The other direction relays fine, because
//! [`SyncLink::push`] deliberately applies with capture **on** at its
//! destination.
//!
//! So the relay is one-way, it fails silently (`lag()` reads 0, `pull` reports
//! nothing, no error), and nothing here refuses the topology. Until that is
//! fixed, **give every replica a link to the authority it actually needs data
//! from** rather than chaining links together.

use std::collections::BTreeMap;

use mpedb_core::cdc::{self, DirtyEntry, DirtyOp};
use mpedb_types::{keycode, Value};

use crate::collab::{EditVerdict, Submission};
use crate::{Database, Error, Result};

/// What this instance is in a sync topology.
///
/// **Not file-authoritative.** Schema and geometry hard-error on config drift
/// because they describe the file; a role describes *this process's* job, and
/// the same `.mpedb` may legitimately be opened as a [`Role::Replica`] by one
/// process and [`Role::Standalone`] by another. Putting it anywhere hashed
/// would turn a deployment knob into a property of the bytes on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// No upstream. Every verdict is authoritative because there is nobody else
    /// to ask. This is what every existing database is.
    #[default]
    Standalone,
    /// Has an upstream. A local commit stands locally and is
    /// [`EditVerdict::Provisional`] until the upstream has seen it.
    Replica,
    /// Others replicate from this one.
    ///
    /// Behaviourally identical to [`Role::Standalone`] today, and it exists
    /// anyway: it is a **declaration a service reads**, so an operator can tell
    /// the two apart in config and monitoring without the engine having to
    /// invent a difference it does not have.
    Authority,
}

impl Role {
    pub fn parse(s: &str) -> Result<Role> {
        match s {
            "standalone" => Ok(Role::Standalone),
            "replica" => Ok(Role::Replica),
            "authority" => Ok(Role::Authority),
            other => Err(Error::Config(format!(
                "unknown sync role {other:?} (expected standalone, replica or authority)"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Standalone => "standalone",
            Role::Replica => "replica",
            Role::Authority => "authority",
        }
    }

    /// Does a locally committed edit still need an authority's word?
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Role::Replica)
    }
}

/// **Who wins when two ends changed the same row.** An explicit choice, because
/// the right answer genuinely differs by workload and guessing it for the caller
/// is how a sync layer loses data politely.
///
/// | your situation | pick | why |
/// |---|---|---|
/// | many clients, all online, a server of record | [`Resolve::UpstreamWins`] | the authority is the truth; a stale client should not be able to undo it |
/// | one client offline for a long time, its work is the point | [`Resolve::LocalWins`] | a week of field work must not evaporate because a colleague touched one row |
/// | you want neither to disappear | **neither** — put the column under the cell plane ([`SyncLink::push_edits`]) so the two edits *merge* instead of one winning |
///
/// The third row is the important one. Row-plane resolution is last-writer-wins
/// by construction: it moves whole row images, so somebody's version is
/// discarded either way. If losing an edit is unacceptable, the answer is not a
/// cleverer winner — it is to stop treating the value as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resolve {
    /// The upstream's value stands; the local change is counted in
    /// [`SyncReport::conflicts`] and replaced. The safe default for a
    /// server-of-record topology.
    #[default]
    UpstreamWins,
    /// The local value overwrites the upstream's. Still **counted** as a
    /// conflict — the caller is told what it ran over, because a silent
    /// overwrite is the same thing as data loss with better manners.
    LocalWins,
}

impl Resolve {
    pub fn parse(s: &str) -> Result<Resolve> {
        match s {
            "upstream-wins" => Ok(Resolve::UpstreamWins),
            "local-wins" => Ok(Resolve::LocalWins),
            other => Err(Error::Config(format!(
                "unknown sync resolve policy {other:?} \
                 (expected upstream-wins or local-wins)"
            ))),
        }
    }
}

/// The sys namespace this module owns. Prefix-disjoint from `plan/`, `pol/`,
/// `mir`, `cdc` (facade convention: `ns ++ 0x00 ++ subkey`).
const SYNC_NS: &str = "sync";

/// Cursors are per **(link, table)**, and that is a correctness requirement
/// rather than tidiness.
///
/// With one cursor per link, a `pull(["doc"])` advanced it to the newest txn it
/// saw — past every pending entry of `note`, which the next `pull(["note"])`
/// then skipped forever. Silent, permanent, and `lag()` reported 0 because it
/// asked the same poisoned cursor. Syncing a hot table and a cold table on
/// different cadences is an ordinary thing to want, so the cursor has to be
/// scoped the way the call is.
fn pull_cursor_key(link: u64, table_id: u32) -> Vec<u8> {
    cursor_key(b"pull/", link, table_id)
}

fn push_cursor_key(link: u64, table_id: u32) -> Vec<u8> {
    cursor_key(b"push/", link, table_id)
}

fn cursor_key(tag: &[u8], link: u64, table_id: u32) -> Vec<u8> {
    let mut k = tag.to_vec();
    k.extend_from_slice(&link.to_be_bytes());
    k.push(b'/');
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}

/// Key of the **base** we recorded for one row: the hash of the image this
/// replica and its upstream last agreed on.
///
/// This is the one piece of state a correct sync cannot do without, and the
/// benchmark is what proved it. Without a base there are only two signals — "we
/// changed it" and "they changed it" — and neither can tell **our own change
/// coming back** from **somebody else having moved the row**. Treating the first
/// as a conflict miscounts; treating the second as an echo silently drops an
/// edit. The base separates them: the upstream row still hashing to the base is
/// a row nobody else touched.
fn base_key(link: u64, table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    let mut k = b"b/".to_vec();
    k.extend_from_slice(&link.to_be_bytes());
    k.extend_from_slice(&table_id.to_be_bytes());
    // Hashed rather than carried whole: a PK is unbounded and this is a btree
    // key. 128 bits of blake3 — the same hash the plan registry and the advisor
    // already trust for identity, so no second hash function enters the crate.
    k.extend_from_slice(&blake3::hash(pk_keycode).as_bytes()[..16]);
    k
}

/// Content hash of a row image; `None` for "no row", which must be a *distinct*
/// state from any hash — a deleted row and a row that happens to hash to zero
/// are not the same thing.
fn image_hash(image: Option<&Vec<Value>>) -> Option<u64> {
    let row = image?;
    let mut h = blake3::Hasher::new();
    for v in row {
        h.update(&keycode::encode_key(std::slice::from_ref(v)));
    }
    Some(u64::from_be_bytes(
        h.finalize().as_bytes()[..8].try_into().expect("len 8"),
    ))
}

fn encode_base(h: Option<u64>) -> Vec<u8> {
    match h {
        Some(v) => {
            let mut b = vec![1u8];
            b.extend_from_slice(&v.to_be_bytes());
            b
        }
        None => vec![0u8],
    }
}

fn decode_base(bytes: &[u8]) -> Option<u64> {
    match bytes.first() {
        Some(1) if bytes.len() >= 9 => {
            Some(u64::from_be_bytes(bytes[1..9].try_into().expect("len 8")))
        }
        _ => None,
    }
}

fn read_cursor(db: &Database, key: &[u8]) -> Result<u64> {
    match db.sys_record_get(SYNC_NS, key)? {
        Some(v) if v.len() == 8 => Ok(u64::from_be_bytes(v[..8].try_into().expect("len 8"))),
        // A short record is corruption, not "start over" — restarting from zero
        // would silently re-apply an offline replica's whole history.
        Some(v) => Err(Error::Corrupt(format!(
            "sync cursor is {} bytes (need 8)",
            v.len()
        ))),
        None => Ok(0),
    }
}

/// What one direction of a sync moved.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    /// Rows written locally (pull) or upstream (push).
    pub upserts: u64,
    pub deletes: u64,
    /// Rows the other side had changed too, resolved by the plane's rule rather
    /// than applied blind. Counted separately because "nothing moved" and
    /// "everything collided" are opposite outcomes with the same `upserts`.
    ///
    /// **Its relationship to `upserts` depends on the policy**, so read the two
    /// together: under [`Resolve::UpstreamWins`] a conflicted row is refused, so
    /// the counts are disjoint and sum to the rows offered; under
    /// [`Resolve::LocalWins`] it is applied, so it is counted in *both* and the
    /// sum exceeds the rows offered.
    pub conflicts: u64,
    /// Rows this call rewrote on the **local** database during a push — the
    /// upstream values adopted for rows we lost.
    ///
    /// Without it a push could change local data and report nothing that said
    /// so: `upserts`/`deletes` describe the destination, and for a push the
    /// destination is the other end.
    pub local_writes: u64,
    /// Where the next call resumes. The highest per-table cursor touched.
    pub cursor: u64,
}

/// What one [`SyncLink::sync`] moved, in each direction.
///
/// Named fields rather than `(SyncReport, SyncReport)`: both halves have the
/// same type, so a caller who binds the tuple in the order the doc *describes*
/// (push, then pull) rather than the order it *returned* gets the other
/// direction's numbers, silently, and it compiles. The first person to use this
/// API did exactly that.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    /// What went up. **Conflicts are counted here** — push is where they are
    /// decided.
    pub pushed: SyncReport,
    /// What came down.
    pub pulled: SyncReport,
}

/// One direction's changed rows, read out of a CDC dirty set.
struct Change {
    table_id: u32,
    pk: Vec<Value>,
    op: DirtyOp,
    last_txn: u64,
}

/// Read every dirty entry of `db` past `after`, for the given tables.
///
/// Sorted by `last_txn` so a partial apply always advances the cursor over a
/// **prefix** of the change stream: interrupting halfway leaves a cursor that
/// is behind rather than ahead, and being behind is recoverable.
fn changes_since(db: &Database, tables: &[u32], after: u64) -> Result<Vec<Change>> {
    let bundle = db.schema();
    let mut out = Vec::new();
    for (subkey, value) in db.sys_record_scan("cdc")? {
        // `cdc\0d/` ++ table u32 BE ++ pk-hash u128 BE, minus the `cdc\0`.
        let want = &cdc::CDC_DIRTY_PREFIX[4..];
        if !subkey.starts_with(want) || subkey.len() < want.len() + 4 {
            continue;
        }
        let table_id = u32::from_be_bytes(
            subkey[want.len()..want.len() + 4].try_into().expect("len 4"),
        );
        if !tables.contains(&table_id) {
            continue;
        }
        let entry = DirtyEntry::decode(&value)?;
        if entry.last_txn <= after {
            continue;
        }
        let Some(tdef) = bundle.schema.table(table_id) else {
            continue;
        };
        let pk = keycode::decode_key(&entry.pk_keycode, &tdef.pk_types())?;
        out.push(Change { table_id, pk, op: entry.op, last_txn: entry.last_txn });
    }
    out.sort_by_key(|c| c.last_txn);
    Ok(out)
}

/// A link from a **local** database to an **upstream** one.
///
/// Both ends are ordinary `Database` handles, so the same code runs whether the
/// upstream is another file on this machine, a shared volume, or (once a
/// transport exists) a remote instance. Nothing in this type knows the
/// difference.
pub struct SyncLink<'a> {
    local: &'a Database,
    upstream: &'a Database,
    /// Distinguishes one link's cursors and bases from another's.
    ///
    /// All of that state lives on the **local** database, so two different
    /// replica files of one upstream may safely use the same number — they
    /// never share a file. What must be unique is one replica's links to
    /// *several* upstreams: those do share a file, and a shared id would make
    /// each read the other's progress as its own.
    link: u64,
    resolve: Resolve,
}

impl<'a> SyncLink<'a> {
    /// A link with the safe default policy ([`Resolve::UpstreamWins`]).
    pub fn new(local: &'a Database, upstream: &'a Database, link: u64) -> Self {
        Self { local, upstream, link, resolve: Resolve::default() }
    }

    /// Choose who wins a row-plane conflict. See [`Resolve`] — the table there
    /// is the decision, and the third row (use the cell plane instead) is the
    /// one most callers actually want.
    pub fn with_resolve(mut self, resolve: Resolve) -> Self {
        self.resolve = resolve;
        self
    }

    /// Turn CDC capture on for `tables` on both ends.
    ///
    /// Required before anything else: without it the dirty set stays empty and
    /// every sync is a silent no-op. Enabling is a separate transaction from the
    /// writes it affects, exactly as `mirror import` does it, so the config a
    /// write reads is stable for that write's whole life.
    pub fn enable(&self, tables: &[&str]) -> Result<()> {
        for db in [self.local, self.upstream] {
            let ids = Self::table_ids(db, tables)?;
            let mut cfg = match db.sys_record_get("cdc", &cdc::CDC_TABS_KEY[4..])? {
                Some(b) => cdc::CaptureConfig::decode(&b)?,
                None => cdc::CaptureConfig::default(),
            };
            cfg.generation += 1;
            for id in ids {
                cfg.set_captured(id, true);
            }
            let mut s = db.begin()?;
            s.set_capture(false);
            s.sys_record_put("cdc", &cdc::CDC_TABS_KEY[4..], &cfg.encode())?;
            s.commit()?;
        }
        Ok(())
    }

    /// Copy every row the upstream already holds into the local database, and
    /// record the agreed base for each.
    ///
    /// **[`SyncLink::enable`] is not retroactive**, and both reviewers walked
    /// into it: change capture starts when it is turned on, so rows written
    /// before that are in no change log and replicate never. The failure is
    /// silent — every call returns `Ok`, `lag()` reads 0, and the two ends
    /// simply differ forever. A smoke test on a fresh database passes, so the
    /// gap only shows up when sync is added to an application that already has
    /// users, which is the case that matters.
    ///
    /// This is the missing step: call it once, after `enable`, when attaching a
    /// replica to an upstream that is not empty. It is a full copy, so it is
    /// O(rows) — meant for bootstrap, not for a schedule.
    ///
    /// Existing local rows for the same PKs are replaced: bootstrapping means
    /// adopting the upstream's state, and merging into a replica that has never
    /// synced would be inventing a conflict nobody expressed.
    pub fn seed(&self, tables: &[&str]) -> Result<SyncReport> {
        let ids = Self::table_ids(self.upstream, tables)?;
        let mut rep = SyncReport::default();
        for (&table_id, name) in ids.iter().zip(tables) {
            let rows = match self.upstream.query(&format!("SELECT * FROM {name}"), &[])? {
                crate::ExecResult::Rows { rows, .. } => rows,
                _ => continue,
            };
            let pk_cols: Vec<u16> = {
                let bundle = self.upstream.schema();
                match bundle.schema.table(table_id) {
                    Some(t) => t.primary_key.clone(),
                    None => continue,
                }
            };
            let mut s = self.local.begin()?;
            // Seeding is replication, not local work: capturing it would push
            // the whole copy straight back up on the next call.
            s.set_capture(false);
            // Delete every target PK before inserting any, for the same reason
            // the pull does: a UNIQUE column the seed permutes relative to the
            // local state is legal at both ends and illegal in between.
            let mut pks: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
            for row in &rows {
                let pk: Vec<Value> = pk_cols.iter().map(|&i| row[i as usize].clone()).collect();
                s.delete_by_pk(table_id, &pk)?;
                pks.push(pk);
            }
            for (row, pk) in rows.iter().zip(&pks) {
                s.insert_row(table_id, row)?;
                s.sys_record_put(
                    SYNC_NS,
                    &base_key(self.link, table_id, &keycode::encode_key(pk)),
                    &encode_base(image_hash(Some(row))),
                )?;
                rep.upserts += 1;
            }
            // The seed is as of the upstream's current state, so the cursor
            // starts there: replaying the change log from zero afterwards would
            // re-apply history this copy already includes.
            let head = self.upstream.snapshot_txn();
            s.sys_record_put(
                SYNC_NS,
                &pull_cursor_key(self.link, table_id),
                &head.to_be_bytes(),
            )?;
            s.commit()?;
            rep.cursor = rep.cursor.max(head);
        }
        Ok(rep)
    }

    fn table_ids(db: &Database, tables: &[&str]) -> Result<Vec<u32>> {
        let bundle = db.schema();
        tables
            .iter()
            .map(|t| {
                bundle.schema.table_id(t).ok_or_else(|| {
                    Error::Unsupported(format!("sync: no table named {t:?} in this database"))
                })
            })
            .collect()
    }

    /// Bring the local database up to date with the upstream.
    ///
    /// A row whose upstream image already equals the local one is **skipped
    /// entirely** — no write, no conflict. That is the common case on a link
    /// that has just pushed, because everything it sent comes back on the next
    /// pull, and counting those as divergences would report a conflict rate
    /// equal to the change rate. (It did, in the first run of `G-rows`.)
    ///
    /// Conflict *detection* lives in [`SyncLink::push`], not here, and
    /// [`SyncLink::sync`] therefore pushes first. Detecting it here would mean
    /// clobbering an unpushed local change before the upstream had ever been
    /// offered it.
    ///
    /// # Prefer [`SyncLink::sync`] unless you know why you are not
    ///
    /// **Calling this on a replica that has unpushed local changes destroys
    /// them, uncounted.** The upstream's value replaces the local one, nothing
    /// is reported (`conflicts` stays 0, the call returns `Ok`), and the loss is
    /// not even auditable afterwards — the local row now matches the upstream,
    /// so it leaves the change log and a later `push` cannot re-offer it.
    ///
    /// That is the documented consequence of putting conflict detection in
    /// `push`, not a defect, but the method has an inviting name and the hazard
    /// belongs where the hand lands. `sync` is push-then-pull and is safe by
    /// construction.
    pub fn pull(&self, tables: &[&str]) -> Result<SyncReport> {
        let ids = Self::table_ids(self.upstream, tables)?;
        let mut rep = SyncReport::default();
        // Per TABLE, because the cursor is per table: a shared cursor advanced
        // by one table's pull skips the other's pending entries forever.
        for &table_id in &ids {
            let key = pull_cursor_key(self.link, table_id);
            let from = read_cursor(self.local, &key)?;
            let changes = changes_since(self.upstream, &[table_id], from)?;
            rep.cursor = rep.cursor.max(from);
            if changes.is_empty() {
                continue;
            }

            // Row images are read from ONE upstream snapshot: a value read from
            // a later snapshot than its neighbour would let a replica land in a
            // state the upstream never had.
            let mut images: Vec<Option<Vec<Value>>> = Vec::with_capacity(changes.len());
            {
                let mut us = self.upstream.begin()?;
                for c in &changes {
                    images.push(us.get_by_pk(c.table_id, &c.pk)?);
                }
                us.rollback();
            }

            let mut s = self.local.begin()?;
            // The replication plane must not self-capture, or everything pulled
            // would push straight back on the next call (DESIGN-MIRROR §3.8).
            s.set_capture(false);
            let mut high = from;
            let mut to_insert: Vec<(usize, Vec<Value>)> = Vec::new();
            // **Delete every affected key first, then insert.** A row-at-a-time
            // delete+insert passes through an intermediate state the source
            // never had, and a UNIQUE secondary index rejects it: permuting two
            // rows\' unique values is legal at both endpoints and illegal in
            // between, so the apply failed and the cursor never moved \u2014 wedged
            // forever, not merely slow. Same two-phase shape as the mirror\'s
            // applier, for the same reason.
            for (i, (c, image)) in changes.iter().zip(images.iter()).enumerate() {
                let kc = keycode::encode_key(&c.pk);
                let theirs = image_hash(image.as_ref());
                let mine = image_hash(s.get_by_pk(c.table_id, &c.pk)?.as_ref());
                high = high.max(c.last_txn);
                if theirs == mine {
                    // Already agreed. Usually our own push coming back; either
                    // way there is nothing to write, and writing anyway would
                    // churn the file and (before the base existed) invent a
                    // conflict.
                    s.sys_record_put(
                        SYNC_NS,
                        &base_key(self.link, c.table_id, &kc),
                        &encode_base(theirs),
                    )?;
                    continue;
                }
                if s.delete_by_pk(c.table_id, &c.pk)? && matches!(c.op, DirtyOp::Delete) {
                    rep.deletes += 1;
                } else if matches!(c.op, DirtyOp::Delete) || image.is_none() {
                    // Nothing local to remove; still a delete we have now seen.
                }
                if let Some(row) = image {
                    if !matches!(c.op, DirtyOp::Delete) {
                        to_insert.push((i, row.clone()));
                    }
                }
                // What we now agree on. Recorded even for a delete (as "no
                // row"), because "we agreed it is gone" and "we never saw it"
                // have to stay distinguishable at the next push.
                s.sys_record_put(
                    SYNC_NS,
                    &base_key(self.link, c.table_id, &kc),
                    &encode_base(theirs),
                )?;
            }
            for (i, row) in to_insert {
                s.insert_row(changes[i].table_id, &row)?;
                rep.upserts += 1;
            }
            s.sys_record_put(SYNC_NS, &key, &high.to_be_bytes())?;
            s.commit()?;
            rep.cursor = rep.cursor.max(high);
        }
        Ok(rep)
    }

    /// Send local row changes upstream. **This is where conflicts are decided.**
    ///
    /// For each locally changed row: hash the upstream's current image and
    /// compare it with the base — the image the two ends last agreed on. Equal
    /// means nobody else has touched the row since, so our change applies.
    /// Different means somebody else moved it while we were away, and
    /// [`Resolve`] decides who wins.
    ///
    /// Applied **with capture on** at the upstream, on purpose: a change pushed
    /// by replica A has to become visible to replica B's next pull, and the
    /// upstream's dirty set is what carries it. The cost is one echo per row,
    /// which [`SyncLink::pull`] recognises and skips.
    pub fn push(&self, tables: &[&str]) -> Result<SyncReport> {
        let ids = Self::table_ids(self.local, tables)?;
        let mut rep = SyncReport::default();
        for &table_id in &ids {
            let one = self.push_table(table_id)?;
            rep.upserts += one.upserts;
            rep.deletes += one.deletes;
            rep.conflicts += one.conflicts;
            rep.local_writes += one.local_writes;
            rep.cursor = rep.cursor.max(one.cursor);
        }
        Ok(rep)
    }

    /// One table's half of [`SyncLink::push`]. Split out because the cursor is
    /// per table (see [`pull_cursor_key`]), so the loop body is a whole
    /// transaction pair and reads better with a name.
    fn push_table(&self, table_id: u32) -> Result<SyncReport> {
        let key = push_cursor_key(self.link, table_id);
        let from = read_cursor(self.local, &key)?;
        let changes = changes_since(self.local, &[table_id], from)?;

        let mut rep = SyncReport { cursor: from, ..Default::default() };
        if changes.is_empty() {
            return Ok(rep);
        }

        let mut images: Vec<Option<Vec<Value>>> = Vec::with_capacity(changes.len());
        {
            let mut ls = self.local.begin()?;
            for c in &changes {
                images.push(ls.get_by_pk(c.table_id, &c.pk)?);
            }
            ls.rollback();
        }

        // Rows the upstream won, to be re-based locally after the push commits.
        let mut lost: Vec<usize> = Vec::new();
        let mut agreed: Vec<(usize, Option<u64>)> = Vec::new();
        let mut deferred: Vec<usize> = Vec::new();
        let mut us = self.upstream.begin()?;
        for (i, (c, image)) in changes.iter().zip(images.iter()).enumerate() {
            let kc = keycode::encode_key(&c.pk);
            let base = self
                .local
                .sys_record_get(SYNC_NS, &base_key(self.link, c.table_id, &kc))?
                .and_then(|b| decode_base(&b));
            let theirs = image_hash(us.get_by_pk(c.table_id, &c.pk)?.as_ref());
            let mine = image_hash(image.as_ref());

            if theirs == mine {
                // Already there — a re-push of something the upstream has.
                agreed.push((i, theirs));
                rep.cursor = rep.cursor.max(c.last_txn);
                continue;
            }
            if theirs != base {
                // Somebody else moved this row since we last agreed on it.
                rep.conflicts += 1;
                if self.resolve == Resolve::UpstreamWins {
                    lost.push(i);
                    rep.cursor = rep.cursor.max(c.last_txn);
                    continue;
                }
                // LocalWins falls through and overwrites, which is what the
                // caller asked for by choosing it.
            }
            // Delete now, insert after every delete has been done: permuting a
            // UNIQUE column is legal at both endpoints and illegal in between,
            // and a row-at-a-time delete+insert walks straight through the
            // illegal state. That wedged the link permanently rather than
            // failing once.
            if us.delete_by_pk(c.table_id, &c.pk)? && image.is_none() {
                rep.deletes += 1;
            }
            if image.is_some() {
                deferred.push(i);
            }
            agreed.push((i, mine));
            rep.cursor = rep.cursor.max(c.last_txn);
        }
        for i in deferred {
            if let Some(row) = &images[i] {
                us.insert_row(changes[i].table_id, row)?;
                rep.upserts += 1;
            }
        }
        us.commit()?;

        // Record what we now agree on, and take back the rows we lost. Both in
        // ONE local transaction with the cursor, so an interrupted push cannot
        // leave a cursor that has moved past work whose base was never written.
        let mut s = self.local.begin()?;
        s.set_capture(false);
        for (i, h) in agreed {
            let kc = keycode::encode_key(&changes[i].pk);
            s.sys_record_put(
                SYNC_NS,
                &base_key(self.link, changes[i].table_id, &kc),
                &encode_base(h),
            )?;
        }
        for i in lost {
            // The upstream's value is the one that stands; adopt it now rather
            // than leaving the replica showing a change it has already lost.
            let c = &changes[i];
            let up_image = {
                let mut u = self.upstream.begin()?;
                let img = u.get_by_pk(c.table_id, &c.pk)?;
                u.rollback();
                img
            };
            s.delete_by_pk(c.table_id, &c.pk)?;
            if let Some(row) = &up_image {
                s.insert_row(c.table_id, row)?;
            }
            rep.local_writes += 1;
            let kc = keycode::encode_key(&c.pk);
            s.sys_record_put(
                SYNC_NS,
                &base_key(self.link, c.table_id, &kc),
                &encode_base(image_hash(up_image.as_ref())),
            )?;
        }
        s.sys_record_put(SYNC_NS, &key, &rep.cursor.to_be_bytes())?;
        s.commit()?;
        Ok(rep)
    }

    /// **Push, then pull.** The order is load-bearing and the benchmark is what
    /// settled it.
    ///
    /// Pulling first would clobber an unpushed local change *before the upstream
    /// had ever been offered it* — including, in the common case, replacing the
    /// replica's newest edit with its own older one that had already been
    /// pushed. Pushing first means every local change is offered, judged against
    /// the base, and either accepted or explicitly lost; the pull that follows
    /// then finds its own work already agreed and skips it.
    ///
    /// **This is the call an application should make.** [`SyncLink::pull`] and
    /// [`SyncLink::push`] are public because a service may want to schedule the
    /// two halves differently, but only this order is safe by construction.
    pub fn sync(&self, tables: &[&str]) -> Result<SyncOutcome> {
        // Named fields rather than a tuple of two same-typed reports: the first
        // reviewer to use this bound `(pulled, pushed)` and got the other
        // direction's numbers, silently, because it compiles either way.
        let pushed = self.push(tables)?;
        let pulled = self.pull(tables)?;
        Ok(SyncOutcome { pushed, pulled })
    }

    /// Send buffered sub-edits to the upstream's authoritative merge (#155).
    ///
    /// This is the cell plane. Each submission carries its own `(seq, editor)`,
    /// assigned when the editor acted — **not** when the network delivered it —
    /// so a replica that was offline for an hour contributes edits that order
    /// against each other exactly as they would have live.
    ///
    /// What it cannot do is reorder edits the upstream has already committed:
    /// #151 rebases a late submission forward, so it lands late rather than in
    /// its original place. Nothing is lost and nothing lands on the wrong bytes;
    /// the counter is a total order *within a round*, not over all history.
    pub fn push_edits(
        &self,
        table: &str,
        col: &str,
        subs: &[Submission],
    ) -> Result<Vec<EditVerdict>> {
        self.upstream.submit_batch(table, col, subs)
    }

    /// Drop upstream dirty entries at or below `watermark`.
    ///
    /// Pull never consumes — that is what lets N replicas share one upstream —
    /// so something has to trim, and only the caller knows how far behind the
    /// slowest live replica is. Passing a watermark above any live replica's
    /// cursor silently makes that replica miss changes forever, which is why
    /// this takes the number rather than computing one it cannot know.
    pub fn gc_upstream(&self, tables: &[&str], watermark: u64) -> Result<u64> {
        let ids = Self::table_ids(self.upstream, tables)?;
        let mut dropped = 0u64;
        let mut s = self.upstream.begin()?;
        s.set_capture(false);
        let want = &cdc::CDC_DIRTY_PREFIX[4..];
        let entries = self.upstream.sys_record_scan("cdc")?;
        for (subkey, value) in entries {
            if !subkey.starts_with(want) || subkey.len() < want.len() + 4 {
                continue;
            }
            let table_id = u32::from_be_bytes(
                subkey[want.len()..want.len() + 4].try_into().expect("len 4"),
            );
            if !ids.contains(&table_id) {
                continue;
            }
            if DirtyEntry::decode(&value)?.last_txn <= watermark
                && s.sys_record_delete("cdc", &subkey)?
            {
                dropped += 1;
            }
        }
        s.commit()?;
        Ok(dropped)
    }

    /// How far behind the upstream this replica is, in changed rows.
    ///
    /// The number a UI shows as "syncing…", and the one an operator watches to
    /// size `gc_upstream`.
    pub fn lag(&self, tables: &[&str]) -> Result<u64> {
        let ids = Self::table_ids(self.upstream, tables)?;
        let mut behind = 0u64;
        let mut ls = self.local.begin()?;
        let mut us = self.upstream.begin()?;
        for &table_id in &ids {
            let from = read_cursor(self.local, &pull_cursor_key(self.link, table_id))?;
            for c in changes_since(self.upstream, &[table_id], from)? {
                // Rows we just pushed are past our pull cursor but identical on
                // both sides. Counting them made a write-heavy replica read as
                // permanently behind — "syncing 7…" on a link with nothing to
                // do — which is precisely wrong for the one caller (a UI) this
                // number exists for.
                let theirs = image_hash(us.get_by_pk(c.table_id, &c.pk)?.as_ref());
                let mine = image_hash(ls.get_by_pk(c.table_id, &c.pk)?.as_ref());
                if theirs != mine {
                    behind += 1;
                }
            }
        }
        us.rollback();
        ls.rollback();
        Ok(behind)
    }
}

/// Every table's row count and content hash, for asserting convergence.
///
/// Used by the tests and the benchmark: "did the replicas converge" is not a
/// question a throughput number can answer, and a sync benchmark without it
/// measures how fast we can lose data.
pub fn fingerprint(db: &Database, tables: &[&str]) -> Result<BTreeMap<String, (u64, u64)>> {
    let mut out = BTreeMap::new();
    for t in tables {
        let rows = match db.query(&format!("SELECT * FROM {t}"), &[])? {
            crate::ExecResult::Rows { rows, .. } => rows,
            _ => Vec::new(),
        };
        // Order-independent: sum of per-row hashes, so two databases that agree
        // on content but not on scan order still match.
        let mut acc = 0u64;
        for r in &rows {
            let mut h = blake3::Hasher::new();
            for v in r {
                h.update(&keycode::encode_key(std::slice::from_ref(v)));
            }
            acc = acc.wrapping_add(u64::from_be_bytes(
                h.finalize().as_bytes()[..8].try_into().expect("len 8"),
            ));
        }
        out.insert((*t).to_string(), (rows.len() as u64, acc));
    }
    Ok(out)
}
