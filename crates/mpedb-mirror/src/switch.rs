//! Authority-switch machinery (DESIGN-MIRROR §7): freeze, verify, and the
//! epoch-fenced flip between source-authoritative and mpedb-authoritative.
//!
//! Freeze is enforced at the ENGINE, not the facade (§3.9): the mirror sets the
//! write-block bitmap in `cdc\0tabs` (the M1.4 mutator check refuses writes to
//! those tables from every path — typed API, ring leader, raw engine), atomically
//! with `mir\0epoch.frozen`. That makes S7's "mpedb frozen → no writes leak"
//! true by construction.

use mpedb::Database;
use mpedb_core::CaptureConfig;
use mpedb_types::{Error, Result};

use crate::adapter::{Cursor, SourceAdapter};
use crate::apply::apply_batch;
use crate::push::push_batch;
use crate::reconcile::verify;
use crate::state::{self, Authority, MirrorState};

const PULL_BATCH: usize = 5000;

fn read_epoch_via(s: &mut mpedb::WriteSession) -> Result<state::Epoch> {
    let bytes = s
        .sys_record_get(state::MIR_NS, state::KEY_EPOCH)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/epoch)".into()))?;
    state::Epoch::decode(&bytes)
}

/// Read the mirror's current epoch record.
pub fn read_epoch(db: &Database) -> Result<state::Epoch> {
    let bytes = db
        .sys_record_get(state::MIR_NS, state::KEY_EPOCH)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/epoch)".into()))?;
    state::Epoch::decode(&bytes)
}

/// Freeze (or unfreeze) all mirrored tables: block every capture-enabled table
/// at the engine and set `mir\0epoch.frozen` — in one capture-suppressed commit.
/// A bumped generation forces every process to re-read the block.
pub fn set_frozen(db: &Database, frozen: bool) -> Result<()> {
    let mut s = db.begin()?;
    s.set_capture(false);

    let mut cap = match s.sys_record_get("cdc", b"tabs")? {
        Some(b) => CaptureConfig::decode(&b)?,
        None => return Err(Error::Unsupported("mirror has no CDC capture record".into())),
    };
    // block exactly the captured (mirrored) tables while frozen
    cap.blocked = if frozen { cap.captured } else { 0 };
    cap.generation = cap.generation.wrapping_add(1);
    s.sys_record_put("cdc", b"tabs", &cap.encode())?;

    let mut epoch = read_epoch_via(&mut s)?;
    epoch.frozen = frozen;
    s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;

    s.commit()
}

fn mirror_id_hex(db: &Database) -> Result<String> {
    let bytes = db
        .sys_record_get(state::MIR_NS, state::KEY_CFG)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/cfg)".into()))?;
    let cfg = state::MirrorConfig::decode(&bytes)?;
    let mut s = String::with_capacity(32);
    for b in cfg.mirror_id {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

fn read_cur<A: SourceAdapter + ?Sized>(db: &Database, adapter: &A) -> Result<Cursor> {
    Ok(db
        .sys_record_get(state::MIR_NS, state::KEY_CUR)?
        .unwrap_or_else(|| adapter.zero_cursor()))
}

/// Pull + apply until the source is caught up. Returns rows applied.
pub fn drain_pull<A: SourceAdapter + ?Sized>(db: &Database, adapter: &mut A) -> Result<u64> {
    let mut applied = 0u64;
    loop {
        let from = read_cur(db, adapter)?;
        match adapter.pull(&from, PULL_BATCH)? {
            Some(batch) => {
                let s = apply_batch(db, &from, &batch)?;
                applied += s.upserts + s.deletes;
            }
            None => break,
        }
    }
    Ok(applied)
}

/// Push until no further progress is made. Returns the aggregate: `upserts` +
/// `deletes` summed across rounds, and `conflicts` = the stable set left parked
/// (source-won write-write conflicts whose dirty entries persist for the next
/// pull to resolve — so they never advance and the loop stops on them).
pub fn drain_push<A: SourceAdapter + ?Sized>(db: &Database, adapter: &mut A) -> Result<crate::push::PushStats> {
    let mut total = crate::push::PushStats::default();
    loop {
        let s = push_batch(db, adapter)?;
        total.upserts += s.upserts;
        total.deletes += s.deletes;
        total.conflicts = s.conflicts; // persist across rounds → report the final count
        if s.upserts + s.deletes == 0 {
            break;
        }
    }
    Ok(total)
}

fn set_epoch(db: &Database, epoch: state::Epoch) -> Result<()> {
    let mut s = db.begin()?;
    s.set_capture(false);
    s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;
    s.commit()
}

/// Switch authority source → mpedb (§7 S4→S5). Drains any pending source
/// changes, then fences the source epoch (losing side first) and flips mpedb to
/// M_AUTH. After this, local mpedb writes accumulate and pulls should stop.
pub fn switch_to_mpedb<A: SourceAdapter + ?Sized>(db: &Database, adapter: &mut A) -> Result<()> {
    let epoch = read_epoch(db)?;
    if epoch.authority != Authority::Source {
        return Err(Error::Unsupported("switch_to_mpedb: not source-authoritative".into()));
    }
    let mid = mirror_id_hex(db)?;
    adapter.ensure_source_state(&mid, epoch.epoch, "source")?;

    drain_pull(db, adapter)?; // catch up first

    let e = epoch.epoch;
    if !adapter.cas_source_state(&mid, e, e + 1, "mpedb")? {
        return Err(Error::Unsupported(
            "switch_to_mpedb fenced: the source epoch moved".into(),
        ));
    }
    // A SIGKILL here leaves (E_m=e, mpedb-pending) vs (E_s=e+1, mpedb):
    // `recover` completes it. Nothing is at risk in the window — the source is
    // already fenced and mpedb has not yet started accumulating as authority.
    set_epoch(
        db,
        state::Epoch {
            epoch: e + 1,
            authority: Authority::Mpedb,
            state: MirrorState::MAuth,
            frozen: false,
        },
    )
}

/// Switch authority mpedb → source (§7 S7→S8). Freezes mpedb (write-block =
/// fence), pushes all local changes, verifies convergence, then flips the epoch
/// and re-seeds the pull cursor to the head captured at cutover. Leaves the
/// mirror FROZEN on a failed verify so no writes leak before reconciliation.
pub fn switch_to_source<A: SourceAdapter + ?Sized>(db: &Database, adapter: &mut A) -> Result<()> {
    let epoch = read_epoch(db)?;
    if epoch.authority != Authority::Mpedb {
        return Err(Error::Unsupported("switch_to_source: not mpedb-authoritative".into()));
    }
    let mid = mirror_id_hex(db)?;
    let e = epoch.epoch;

    set_frozen(db, true)?; // fence the losing side (mpedb) at the engine
    drain_push(db, adapter)?; // land every local change on the source

    if !verify(db, adapter)? {
        return Err(Error::Unsupported(
            "switch_to_source: verify failed — mpedb and source diverge; run reconcile \
             (the mirror stays frozen)"
                .into(),
        ));
    }

    // fence the source (it takes authority at this instant)
    if !adapter.cas_source_state(&mid, e, e + 1, "source")? {
        return Err(Error::Unsupported(
            "switch_to_source fenced: the source epoch moved".into(),
        ));
    }
    // A SIGKILL here leaves (E_m=e, mpedb, frozen) vs (E_s=e+1, source):
    // `recover` completes it. mpedb stays frozen across the window, so no local
    // write can leak into a database that is no longer authoritative.
    finish_switch_to_source(db, e + 1)
}

/// The final mpedb-side txn of a switch-to-source: unblock the tables and
/// publish (E, source, steady) in ONE commit. Split out so `recover` can drive
/// it after a crash between the source CAS and this commit.
///
/// The pull cursor is deliberately NOT re-seeded to the source's log head here.
/// See [`SourceAdapter::cas_source_state`]: re-seeding would silently skip every
/// third-party source write that landed between the drain and the cutover. Our
/// own drain-push rows above the cursor are already filtered by origin, so the
/// next pull consumes exactly the foreign writes and nothing else.
fn finish_switch_to_source(db: &Database, new_epoch: u64) -> Result<()> {
    let mut s = db.begin()?;
    s.set_capture(false);
    let mut cap = s
        .sys_record_get("cdc", b"tabs")?
        .map(|b| CaptureConfig::decode(&b))
        .transpose()?
        .ok_or_else(|| Error::Unsupported("mirror has no CDC record".into()))?;
    cap.blocked = 0;
    cap.generation = cap.generation.wrapping_add(1);
    s.sys_record_put("cdc", b"tabs", &cap.encode())?;
    s.sys_record_put(
        state::MIR_NS,
        state::KEY_EPOCH,
        &state::Epoch {
            epoch: new_epoch,
            authority: Authority::Source,
            state: MirrorState::SrcAuth,
            frozen: false,
        }
        .encode(),
    )?;
    s.commit()
}

/// What [`recover`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovered {
    /// The pair already agrees — nothing to do.
    Steady,
    /// A switch-to-mpedb was interrupted after the source CAS; completed it.
    CompletedToMpedb,
    /// A switch-to-source was interrupted after the source CAS; completed it.
    CompletedToSource,
}

/// **Recovery-on-attach (§7).** A switch is two fenced commits — one on the
/// source, one in mpedb — and a SIGKILL can land between them. This maps the
/// `(mpedb epoch, source epoch)` pair to the single state-machine position it
/// can be in and drives the missing sub-step forward. It is a total function:
/// every reachable pair is either steady, a completable half-cutover, or an
/// explicit error.
///
/// Safety of each half-state while it lasts: mid switch-to-source mpedb is
/// frozen (no local write can leak); mid switch-to-mpedb the source is already
/// fenced at the new epoch (a stale pull/push aborts on the epoch predicate).
/// So recovery is never racing live divergence — it only finishes the paperwork.
pub fn recover<A: SourceAdapter + ?Sized>(db: &Database, adapter: &mut A) -> Result<Recovered> {
    let m = read_epoch(db)?;
    let mid = mirror_id_hex(db)?;
    let Some((e_s, auth_s)) = adapter.read_source_state(&mid)? else {
        // No source row yet: the mirror has never switched (import-only).
        return Ok(Recovered::Steady);
    };

    if e_s == m.epoch {
        return Ok(Recovered::Steady);
    }
    if e_s != m.epoch + 1 {
        return Err(Error::Unsupported(format!(
            "mirror epochs diverge by more than one (mpedb={}, source={e_s}) — refusing to \
             guess; this needs operator resync",
            m.epoch
        )));
    }

    // e_s == m.epoch + 1: the source CAS landed, the mpedb commit did not.
    match auth_s.as_str() {
        "mpedb" => {
            set_epoch(
                db,
                state::Epoch {
                    epoch: e_s,
                    authority: Authority::Mpedb,
                    state: MirrorState::MAuth,
                    frozen: m.frozen,
                },
            )?;
            Ok(Recovered::CompletedToMpedb)
        }
        "source" => {
            finish_switch_to_source(db, e_s)?;
            Ok(Recovered::CompletedToSource)
        }
        other => Err(Error::Unsupported(format!(
            "source mirror-state has an unknown authority `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};
    use mpedb::ExecResult;
    use mpedb_types::{Error as E, Value};
    use rusqlite::Connection;

    fn tmp(name: &str, ext: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.{ext}", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn freeze_blocks_writes_at_the_engine_and_unfreeze_restores() {
        let src = tmp("freeze-src", "db");
        let mid = tmp("freeze-mid", "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };

        // a local write works before freezing
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(11), Value::Int(1)]).unwrap();

        set_frozen(&db, true).unwrap();
        assert!(read_epoch(&db).unwrap().frozen);
        // now a local write to a mirrored table is refused at the engine
        let err = db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(12), Value::Int(1)]);
        assert!(matches!(err, Err(E::Frozen { .. })), "got {err:?}");
        let ins = db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(2), Value::Int(20)]);
        assert!(matches!(ins, Err(E::Frozen { .. })));

        set_frozen(&db, false).unwrap();
        assert!(!read_epoch(&db).unwrap().frozen);
        // writes work again
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(13), Value::Int(1)]).unwrap();

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn switch_round_trip_moves_authority_and_lands_local_changes() {
        use crate::adapter::SourceAdapter;
        use crate::SqliteAdapter;

        let src = tmp("sw-src", "db");
        let mid = tmp("sw-mid", "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10),(2,20);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };
        let mut adapter = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        // starts source-authoritative (epoch 1)
        assert_eq!(read_epoch(&db).unwrap().authority, Authority::Source);

        // → mpedb-authoritative
        super::switch_to_mpedb(&db, &mut adapter).unwrap();
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch), (Authority::Mpedb, 2));

        // local writes while mpedb is authoritative
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)]).unwrap();
        db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(5), Value::Int(50)]).unwrap();

        // → back to source-authoritative (pushes + verifies + re-seeds cursor)
        super::switch_to_source(&db, &mut adapter).unwrap();
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch, ep.frozen), (Authority::Source, 3, false));

        // the source now holds the local changes
        let v1: i64 = adapter
            .conn()
            .query_row("SELECT v FROM t WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, 99);
        let n5: i64 = adapter
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id=5", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n5, 1);

        // the source-side epoch agrees
        let mid_hex = super::mirror_id_hex(&db).unwrap();
        assert_eq!(adapter.read_source_state(&mid_hex).unwrap(), Some((3, "source".into())));

        // and a fresh pull after switch-back sees nothing (cursor re-seeded)
        let from = read_cur(&db, &adapter).unwrap();
        assert!(adapter.pull(&from, 1000).unwrap().is_none());

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    // ---- M8.1 switch drill (DESIGN-MIRROR §10.9) ----

    /// Deterministic xorshift64* — no rand dep (testing convention).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: i64) -> i64 {
            (self.next() % n as u64) as i64
        }
    }

    use std::collections::BTreeMap;

    fn mpedb_rows(db: &Database) -> BTreeMap<i64, i64> {
        let ExecResult::Rows { rows, .. } = db.query("SELECT id, v FROM t", &[]).unwrap() else {
            panic!("expected rows")
        };
        rows.iter()
            .map(|r| match (&r[0], &r[1]) {
                (Value::Int(id), Value::Int(v)) => (*id, *v),
                (Value::Int(id), Value::Null) => (*id, i64::MIN), // sentinel for NULL v
                other => panic!("bad row {other:?}"),
            })
            .collect()
    }

    fn source_rows(conn: &Connection) -> BTreeMap<i64, i64> {
        let mut stmt = conn.prepare("SELECT id, v FROM t").unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(i64::MIN)))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Apply a random put/delete to the source (sqlite), driven by `model` so we
    /// emit INSERT vs UPDATE correctly without relying on UPSERT support.
    fn src_write(conn: &Connection, model: &mut BTreeMap<i64, i64>, id: i64, v: Option<i64>) {
        match v {
            Some(v) => {
                if model.contains_key(&id) {
                    conn.execute("UPDATE t SET v=?1 WHERE id=?2", rusqlite::params![v, id]).unwrap();
                } else {
                    conn.execute("INSERT INTO t(id,v) VALUES(?1,?2)", rusqlite::params![id, v]).unwrap();
                }
                model.insert(id, v);
            }
            None => {
                conn.execute("DELETE FROM t WHERE id=?1", rusqlite::params![id]).unwrap();
                model.remove(&id);
            }
        }
    }

    fn mpedb_write(db: &Database, model: &mut BTreeMap<i64, i64>, id: i64, v: Option<i64>) {
        match v {
            Some(v) => {
                if model.contains_key(&id) {
                    db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(v), Value::Int(id)]).unwrap();
                } else {
                    db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(id), Value::Int(v)]).unwrap();
                }
                model.insert(id, v);
            }
            None => {
                db.query("DELETE FROM t WHERE id=$1", &[Value::Int(id)]).unwrap();
                model.remove(&id);
            }
        }
    }

    fn run_switch_drill(rounds: usize, seed: u64, rogue_every: usize) {
        use crate::reconcile::{reconcile, verify};
        use crate::SqliteAdapter;

        let src = tmp(&format!("drill-src-{seed}"), "db");
        let mid = tmp(&format!("drill-mid-{seed}"), "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10),(2,20),(3,30);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };
        let mut adapter = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        let mut model: BTreeMap<i64, i64> = source_rows(adapter.conn());
        let mut rng = Rng(seed | 1);
        const K: i64 = 8; // id space for churn

        for round in 0..rounds {
            let start_epoch = read_epoch(&db).unwrap().epoch;

            // --- S2: a burst of source writes, then switch to mpedb (drains pull)
            for _ in 0..3 {
                let id = 1 + rng.below(K);
                let v = if rng.below(4) == 0 { None } else { Some(rng.below(1000)) };
                src_write(adapter.conn(), &mut model, id, v);
            }
            switch_to_mpedb(&db, &mut adapter).unwrap();
            let ep = read_epoch(&db).unwrap();
            assert_eq!(ep.authority, Authority::Mpedb, "round {round}: should be M_AUTH");
            assert_eq!(ep.epoch, start_epoch + 1);

            // --- M_AUTH: a burst of local writes
            for _ in 0..3 {
                let id = 1 + rng.below(K);
                let v = if rng.below(4) == 0 { None } else { Some(rng.below(1000)) };
                mpedb_write(&db, &mut model, id, v);
            }

            // --- optionally inject a ROGUE direct source write (id outside the
            //     churn range so it's a genuine source-only row verify will catch)
            let rogue = rogue_every != 0 && round % rogue_every == rogue_every - 1;
            if rogue {
                let rid = 1000 + round as i64;
                let rv = rng.below(1000);
                src_write(adapter.conn(), &mut model, rid, Some(rv));
            }

            // --- switch back to source. A rogue write makes verify fail (mpedb
            //     lacks the row) → the mirror stays frozen; escape via
            //     unfreeze + reconcile (source-wins folds the rogue in), retry.
            match switch_to_source(&db, &mut adapter) {
                Ok(()) => assert!(!rogue, "round {round}: rogue write should have failed verify"),
                Err(_) => {
                    assert!(rogue, "round {round}: non-rogue switch must not fail");
                    assert!(read_epoch(&db).unwrap().frozen, "failed switch leaves it frozen");
                    set_frozen(&db, false).unwrap();
                    reconcile(&db, &mut adapter).unwrap(); // source-wins → mpedb gets the rogue
                    switch_to_source(&db, &mut adapter).unwrap();
                }
            }

            // --- invariants after a full round
            let ep = read_epoch(&db).unwrap();
            assert_eq!(ep.authority, Authority::Source, "round {round}: back to S_AUTH");
            assert!(!ep.frozen, "round {round}: not frozen after a clean switch");
            assert_eq!(ep.epoch, start_epoch + 2, "round {round}: epoch advances by 2");
            assert!(verify(&db, &mut adapter).unwrap(), "round {round}: sides must be identical");
            assert_eq!(mpedb_rows(&db), model, "round {round}: mpedb matches the model");
            assert_eq!(source_rows(adapter.conn()), model, "round {round}: source matches the model");
        }

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn switch_drill_converges_each_round_with_injected_divergence() {
        // several seeds so the churn hits inserts, updates, deletes, collisions
        for seed in [0x1234_5678u64, 0xdead_beef, 0x0f0f_0f0f] {
            run_switch_drill(12, seed, 4); // rogue write every 4th round
        }
    }

    #[test]
    #[ignore = "slow: 100-round switch drill under load (run with --ignored)"]
    fn switch_drill_x100() {
        run_switch_drill(100, 0xa5a5_5a5a, 5);
    }

    // ---- §10.5 crash waves pinned BETWEEN the switch sub-transactions ----
    //
    // A switch is two fenced commits — the source CAS and the mpedb commit — and
    // a SIGKILL can land between them. These tests build that exact half-state by
    // driving the sub-steps by hand and *stopping*, which is precisely what a kill
    // in the window leaves behind, then assert `recover` completes it and the pair
    // converges. Doing it in-process (rather than with a real SIGKILL) is what
    // lets us pin the kill to the one instant that matters; mirror-collide covers
    // kills at random instants.

    fn mirror_with_source(
        tag: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, Database, crate::SqliteAdapter) {
        use crate::SqliteAdapter;
        let src = tmp(&format!("{tag}-src"), "db");
        let mid = tmp(&format!("{tag}-mid"), "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
                 INSERT INTO t VALUES (1,10);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };
        let adapter = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        (src, mid, db, adapter)
    }

    #[test]
    fn crash_between_switch_to_mpedb_subtxns_recovers() {
        let (src, mid, db, mut adapter) = mirror_with_source("crash-tom");
        let mirror_id = super::mirror_id_hex(&db).unwrap();
        let e = read_epoch(&db).unwrap().epoch;
        adapter.ensure_source_state(&mirror_id, e, "source").unwrap();
        drain_pull(&db, &mut adapter).unwrap();

        // CRASH POINT: the source CAS lands, the mpedb commit never does.
        assert!(adapter.cas_source_state(&mirror_id, e, e + 1, "mpedb").unwrap());
        assert_eq!(read_epoch(&db).unwrap().authority, Authority::Source, "mpedb still stale");
        assert_eq!(adapter.read_source_state(&mirror_id).unwrap(), Some((e + 1, "mpedb".into())));

        // recovery drives the half-cutover forward
        assert_eq!(super::recover(&db, &mut adapter).unwrap(), Recovered::CompletedToMpedb);
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch), (Authority::Mpedb, e + 1));
        // idempotent: a second recovery is a no-op
        assert_eq!(super::recover(&db, &mut adapter).unwrap(), Recovered::Steady);

        // and the mirror is fully usable afterwards: local writes push back
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)]).unwrap();
        drain_push(&db, &mut adapter).unwrap();
        assert!(verify(&db, &mut adapter).unwrap());

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn crash_between_switch_to_source_subtxns_recovers_and_unfreezes() {
        let (src, mid, db, mut adapter) = mirror_with_source("crash-tos");
        let mirror_id = super::mirror_id_hex(&db).unwrap();
        switch_to_mpedb(&db, &mut adapter).unwrap();
        let e = read_epoch(&db).unwrap().epoch;

        // local write, then the switch-back sub-steps by hand
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(42), Value::Int(1)]).unwrap();
        set_frozen(&db, true).unwrap();
        drain_push(&db, &mut adapter).unwrap();
        assert!(verify(&db, &mut adapter).unwrap());

        // CRASH POINT: the source CAS lands, the finalizing mpedb commit never does.
        assert!(adapter.cas_source_state(&mirror_id, e, e + 1, "source").unwrap());
        // the half-state is SAFE while it lasts: mpedb is still frozen, so no
        // local write can leak into a database that is no longer authoritative.
        let leaked = db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(7), Value::Int(1)]);
        assert!(matches!(leaked, Err(E::Frozen { .. })), "got {leaked:?}");

        assert_eq!(super::recover(&db, &mut adapter).unwrap(), Recovered::CompletedToSource);
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch), (Authority::Source, e + 1));
        assert!(!ep.frozen, "recovery must unfreeze — the switch completed");
        assert_eq!(super::recover(&db, &mut adapter).unwrap(), Recovered::Steady);

        // writes flow again and the source is authoritative: a source change pulls in
        adapter.conn().execute("UPDATE t SET v=1234 WHERE id=1", []).unwrap();
        drain_pull(&db, &mut adapter).unwrap();
        assert!(verify(&db, &mut adapter).unwrap());

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn recover_refuses_to_guess_when_epochs_diverge_by_more_than_one() {
        let (src, mid, db, mut adapter) = mirror_with_source("crash-div");
        let mirror_id = super::mirror_id_hex(&db).unwrap();
        let e = read_epoch(&db).unwrap().epoch;
        adapter.ensure_source_state(&mirror_id, e, "source").unwrap();
        // external tampering: the source epoch jumps two ahead
        assert!(adapter.cas_source_state(&mirror_id, e, e + 2, "mpedb").unwrap());
        let err = super::recover(&db, &mut adapter);
        assert!(matches!(err, Err(E::Unsupported(_))), "got {err:?}");

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Regression (review CONF#3/#16): the cutover used to re-seed the pull
    /// cursor to the source's log head, which SKIPPED every third-party source
    /// write committed between the drain and the cutover — permanent silent
    /// divergence. The cursor now stays put and origin-filtering suppresses our
    /// own echoes, so that window is pulled instead of swallowed.
    #[test]
    fn switch_to_source_does_not_swallow_a_racing_source_write() {
        let (src, mid, db, mut adapter) = mirror_with_source("noswallow");
        switch_to_mpedb(&db, &mut adapter).unwrap();

        // a local change to push during the drain
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(50), Value::Int(1)]).unwrap();

        // an unfenced third-party source write lands in the switch window — after
        // our drain/verify would have run, before the cutover. It must survive.
        set_frozen(&db, true).unwrap();
        drain_push(&db, &mut adapter).unwrap();
        adapter
            .conn()
            .execute("INSERT INTO t(id,v) VALUES (777, 7)", [])
            .unwrap();

        // finish the switch by hand (verify would fail on the injected row, which
        // is the honest cost of cooperative fencing — §7 S7½ reconciles it)
        let mirror_id = super::mirror_id_hex(&db).unwrap();
        let e = read_epoch(&db).unwrap().epoch;
        assert!(adapter.cas_source_state(&mirror_id, e, e + 1, "source").unwrap());
        super::finish_switch_to_source(&db, e + 1).unwrap();

        // the racing write is NOT below the cursor: the next pull picks it up.
        drain_pull(&db, &mut adapter).unwrap();
        assert!(
            verify(&db, &mut adapter).unwrap(),
            "the source write that raced the cutover must be pulled, not swallowed"
        );
        let rows = mpedb_rows(&db);
        assert!(rows.contains_key(&777), "racing row 777 missing from mpedb: {rows:?}");

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }
    /// §7 calls `regenerate` "the ONLY arrow out of HALTED(db_full)", and it
    /// STARTS with a freeze — a write. So the escape hatch is only real if a
    /// freeze can commit against a file that just refused an INSERT.
    ///
    /// It can, and this pins it. (The margin comes from the failed insert
    /// releasing its pages back to `reusable`, which the small control write
    /// then finds; §3.10's reserved pool is the belt to that braces.) If this
    /// ever goes red, HALTED(db_full) becomes unrecoverable and `regenerate`
    /// needs `set_reserved_alloc(true)`.
    #[test]
    fn freeze_must_work_when_the_file_is_full() {
        use crate::import::{import_sqlite, ImportOptions};
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join("mpedb-regen-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join(format!("s-{}.db", std::process::id()));
        let dest = dir.join(format!("d-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT NOT NULL);
                 INSERT INTO t VALUES (1,'a');",
            ).unwrap();
        }
        // smallest legal file, so filling it is quick
        let opts = ImportOptions { size_bytes: 1 << 20, ..Default::default() };
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &dest, &opts).unwrap().0
        };
        // fill the data region until the engine says DbFull
        let mut i = 2i64;
        let filled = loop {
            let r = db.query(
                "INSERT INTO t (id, v) VALUES ($1, $2)",
                &[Value::Int(i), Value::Text("x".repeat(400))],
            );
            match r {
                Ok(_) => { i += 1; if i > 100_000 { break false } }
                Err(mpedb_types::Error::DbFull) => break true,
                Err(e) => panic!("unexpected: {e:?}"),
            }
        };
        assert!(filled, "could not fill the file");
        eprintln!("  file full after {} rows", i - 2);

        // THE QUESTION: can the documented recovery even start?
        let r = crate::switch::set_frozen(&db, true);
        eprintln!("  set_frozen on a full file -> {r:?}");
        assert!(
            r.is_ok(),
            "freeze is step 1 of the ONLY escape from HALTED(db_full); if it \
             cannot allocate, the escape is unreachable"
        );
        for p in [src, dest] { let _ = std::fs::remove_file(p); }
    }
}
