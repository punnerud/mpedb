//! Durable request preparation/replay across local reopen, restore and races.
use mpedb_mirror::{
    push_batch, state, Cursor, ImportOptions, NetOp, NetOpKind, PullBatch, PushContext, PushReply,
    SourceAdapter,
};
use mpedb_types::{Error, Result, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
const OUTBOX: &[u8] = b"push/outbox";
const PART: &[u8] = b"push/outbox/00000000";
struct Fixture {
    path: PathBuf,
    db: Option<mpedb::Database>,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let path = mpedb_testkit::scratch_base()
            .join(format!("push-outbox-{name}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY,v INTEGER,g INTEGER,payload TEXT);INSERT INTO t VALUES(1,1,0,'initial')").unwrap();
        let (db, _) = mpedb_mirror::import_sqlite(
            &mut c,
            &path,
            &ImportOptions {
                size_bytes: 64 * 1024 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        db.query("UPDATE t SET v=10 WHERE id=1", &[]).unwrap();
        Self { path, db: Some(db) }
    }
    fn db(&self) -> &mpedb::Database {
        self.db.as_ref().unwrap()
    }
    fn reopen(&mut self) {
        drop(self.db.take());
        self.db = Some(mpedb::Database::open_from_file(&self.path).unwrap());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.db.take());
        let _ = std::fs::remove_file(&self.path);
    }
}
#[derive(Clone, Debug)]
struct Call {
    id: [u8; 32],
    ops: Vec<NetOp>,
    generations: Vec<u64>,
    from: Option<Cursor>,
}
#[derive(Clone, Copy)]
enum Mode {
    Fail,
    Good,
    BadKey,
    CorruptAfterRpc,
    UntrackedDelete,
    UntrackedLegacyUpsert,
    RaceNewRequest,
}
struct Adapter<'a> {
    id: Option<[u8; 16]>,
    mode: Mode,
    db: Option<&'a mpedb::Database>,
    log: Arc<Mutex<Vec<Call>>>,
}
impl Adapter<'_> {
    fn new(mode: Mode) -> Self {
        Self {
            id: Some([7; 16]),
            mode,
            db: None,
            log: Arc::default(),
        }
    }
    fn calls(&self) -> Vec<Call> {
        self.log.lock().unwrap().clone()
    }
}
impl SourceAdapter for Adapter<'_> {
    fn pull(&mut self, _: &Cursor, _: usize) -> Result<Option<PullBatch>> {
        Ok(None)
    }
    fn head(&mut self) -> Result<Cursor> {
        Ok(vec![7])
    }
    fn zero_cursor(&self) -> Cursor {
        vec![7]
    }
    fn read_table_rows(&mut self, _: u32) -> Result<Vec<Vec<Value>>> {
        unreachable!()
    }
    fn push(&mut self, _: &[NetOp]) -> Result<Vec<bool>> {
        unreachable!()
    }
    fn ensure_source_state(&mut self, _: &str, _: u64, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_source_state(&mut self, _: &str) -> Result<Option<(u64, String)>> {
        Ok(None)
    }
    fn cas_source_state(&mut self, _: &str, _: u64, _: u64, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn push_outbox_identity(&self) -> Option<[u8; 16]> {
        self.id
    }
    fn push_versioned(
        &mut self,
        from: Option<&Cursor>,
        ops: &[NetOp],
        ctx: &PushContext,
    ) -> Result<PushReply> {
        self.log.lock().unwrap().push(Call {
            id: ctx.request_id.expect("persist BEFORE RPC"),
            ops: ops.to_vec(),
            generations: ctx.generations.clone(),
            from: from.cloned(),
        });
        if matches!(self.mode, Mode::Fail) {
            return Err(Error::Unsupported("injected transport failure".into()));
        }
        let mut images = ops
            .iter()
            .map(|op| match &op.kind {
                NetOpKind::Delete => None,
                NetOpKind::Upsert(row) => {
                    let mut row = row.clone();
                    let Value::Int(v) = row[1] else {
                        panic!("integer fixture")
                    };
                    row[2] = Value::Int(v + 1);
                    Some(row)
                }
            })
            .collect::<Vec<_>>();
        if matches!(self.mode, Mode::BadKey) {
            images[0].as_mut().unwrap()[0] = Value::Int(99);
        }
        if matches!(self.mode, Mode::CorruptAfterRpc) {
            let mut s = self.db.unwrap().begin()?;
            s.set_capture(false);
            let mut bytes = s.sys_record_get(state::MIR_NS, PART)?.unwrap();
            bytes[0] ^= 1;
            s.sys_record_put(state::MIR_NS, PART, &bytes)?;
            s.commit()?;
        }
        if matches!(
            self.mode,
            Mode::UntrackedDelete | Mode::UntrackedLegacyUpsert
        ) {
            let mut s = self.db.unwrap().begin()?;
            s.set_capture(false);
            let values = [
                Value::Int(1),
                Value::Int(77),
                Value::Int(0),
                Value::Text("untracked".into()),
            ];
            if matches!(self.mode, Mode::UntrackedDelete) {
                s.insert_row(0, &values)?;
            } else {
                s.update_by_pk(0, &values)?;
            }
            s.commit()?;
        }
        if matches!(self.mode, Mode::RaceNewRequest) {
            let db = self.db.unwrap();
            let mut winner = Adapter::new(Mode::Good);
            push_batch(db, &mut winner)?;
            db.query("UPDATE t SET v=20,g=0 WHERE id=1", &[])?;
            let mut next = Adapter::new(Mode::Fail);
            assert!(push_batch(db, &mut next).is_err());
        }
        Ok(PushReply {
            applied: vec![true; ops.len()],
            postimages: if matches!(self.mode, Mode::UntrackedLegacyUpsert) {
                None
            } else {
                Some(images)
            },
        })
    }
}
fn rows(db: &mpedb::Database) -> Vec<Vec<Value>> {
    match db.query("SELECT id,v,g FROM t ORDER BY id", &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows"),
    }
}
fn dirty(db: &mpedb::Database) -> usize {
    db.sys_record_scan("cdc")
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(b"d/"))
        .count()
}
fn put(db: &mpedb::Database, key: &[u8], value: &[u8]) {
    let mut s = db.begin().unwrap();
    s.set_capture(false);
    s.sys_record_put(state::MIR_NS, key, value).unwrap();
    s.commit().unwrap();
}
#[test]
fn cold_replay_preserves_original_request_and_newer_local_generation() {
    let mut f = Fixture::new("cold");
    f.db()
        .query(
            "UPDATE t SET payload=$1 WHERE id=1",
            &[Value::Text("ø🙂".repeat(14000))],
        )
        .unwrap();
    let mut adapter = Adapter::new(Mode::Fail);
    assert!(push_batch(f.db(), &mut adapter).is_err());
    assert!(f
        .db()
        .sys_record_get(state::MIR_NS, b"push/outbox/00000001")
        .unwrap()
        .is_some());
    let first = adapter.calls()[0].clone();
    f.reopen();
    f.db().query("UPDATE t SET v=11 WHERE id=1", &[]).unwrap();
    adapter.mode = Mode::Good;
    assert_eq!(push_batch(f.db(), &mut adapter).unwrap().upserts, 1);
    let retry = adapter.calls()[1].clone();
    assert_eq!(first.id, retry.id);
    assert_eq!(first.ops, retry.ops);
    assert_eq!(first.generations, retry.generations);
    assert_eq!(first.from, retry.from);
    assert_eq!(
        rows(f.db()),
        vec![vec![Value::Int(1), Value::Int(11), Value::Int(0)]]
    );
    assert_eq!(dirty(f.db()), 1);
    assert!(f
        .db()
        .sys_record_get(state::MIR_NS, OUTBOX)
        .unwrap()
        .is_none());
    push_batch(f.db(), &mut adapter).unwrap();
    assert_ne!(first.id, adapter.calls()[2].id);
    assert_eq!(
        rows(f.db()),
        vec![vec![Value::Int(1), Value::Int(11), Value::Int(12)]]
    );
    assert_eq!(dirty(f.db()), 0);
    f.db().verify().unwrap();
}
#[test]
fn restoring_an_old_file_without_an_outbox_never_reuses_a_request_identity() {
    let mut f = Fixture::new("restore");
    let backup = f.path.with_extension("backup");
    drop(f.db.take());
    std::fs::copy(&f.path, &backup).unwrap();
    f.db = Some(mpedb::Database::open_from_file(&f.path).unwrap());
    let mut adapter = Adapter::new(Mode::Good);
    push_batch(f.db(), &mut adapter).unwrap();
    drop(f.db.take());
    std::fs::copy(&backup, &f.path).unwrap();
    f.db = Some(mpedb::Database::open_from_file(&f.path).unwrap());
    push_batch(f.db(), &mut adapter).unwrap();
    let calls = adapter.calls();
    assert_eq!(calls[0].ops, calls[1].ops);
    assert_eq!(calls[0].generations, calls[1].generations);
    assert_ne!(
        calls[0].id, calls[1].id,
        "restored transaction counters cannot identify a NEW request"
    );
    std::fs::remove_file(backup).unwrap();
}
#[test]
fn corrupt_or_mismatched_requests_stop_before_any_source_rpc() {
    for case in [
        "identity", "legacy", "zero", "epoch", "config", "schema", "chunk", "missing", "header",
        "size",
    ] {
        let f = Fixture::new(case);
        let mut first = Adapter::new(Mode::Fail);
        assert!(push_batch(f.db(), &mut first).is_err());
        let mut next = Adapter::new(Mode::Good);
        match case {
            "identity" => next.id = Some([8; 16]),
            "legacy" => next.id = None,
            "zero" => next.id = Some([0; 16]),
            "epoch" => {
                let bytes = f
                    .db()
                    .sys_record_get(state::MIR_NS, state::KEY_EPOCH)
                    .unwrap()
                    .unwrap();
                let mut epoch = state::Epoch::decode(&bytes).unwrap();
                epoch.epoch += 1;
                put(f.db(), state::KEY_EPOCH, &epoch.encode());
            }
            "config" => {
                let bytes = f
                    .db()
                    .sys_record_get(state::MIR_NS, state::KEY_CFG)
                    .unwrap()
                    .unwrap();
                let mut cfg = state::MirrorConfig::decode(&bytes).unwrap();
                cfg.mirror_id[0] ^= 1;
                put(f.db(), state::KEY_CFG, &cfg.encode());
            }
            "schema" => {
                f.db()
                    .query("ALTER TABLE t ADD COLUMN extra TEXT", &[])
                    .unwrap();
            }
            "chunk" => {
                let mut bytes = f.db().sys_record_get(state::MIR_NS, PART).unwrap().unwrap();
                bytes[0] ^= 1;
                put(f.db(), PART, &bytes);
            }
            "missing" => {
                let mut s = f.db().begin().unwrap();
                s.sys_record_delete(state::MIR_NS, PART).unwrap();
                s.commit().unwrap();
            }
            "header" | "size" => {
                let mut bytes = f
                    .db()
                    .sys_record_get(state::MIR_NS, OUTBOX)
                    .unwrap()
                    .unwrap();
                if case == "header" {
                    bytes[0] = 2;
                } else {
                    bytes[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
                }
                put(f.db(), OUTBOX, &bytes);
            }
            _ => unreachable!(),
        }
        assert!(push_batch(f.db(), &mut next).is_err(), "{case}");
        assert!(next.calls().is_empty(), "{case}");
        assert_eq!(dirty(f.db()), 1);
        assert!(f
            .db()
            .sys_record_get(state::MIR_NS, OUTBOX)
            .unwrap()
            .is_some());
    }
}
#[test]
fn ack_failure_retains_the_request_and_all_local_pending_intent() {
    for corrupt_after in [false, true] {
        let f = Fixture::new(if corrupt_after {
            "ackchunk"
        } else {
            "badimage"
        });
        let mut first = Adapter::new(Mode::Fail);
        assert!(push_batch(f.db(), &mut first).is_err());
        let original = f.db().sys_record_get(state::MIR_NS, PART).unwrap().unwrap();
        let mut adapter = Adapter::new(if corrupt_after {
            Mode::CorruptAfterRpc
        } else {
            Mode::BadKey
        });
        adapter.db = Some(f.db());
        assert!(push_batch(f.db(), &mut adapter).is_err());
        assert_eq!(dirty(f.db()), 1);
        assert_eq!(rows(f.db())[0][2], Value::Int(0));
        assert!(f
            .db()
            .sys_record_get(state::MIR_NS, OUTBOX)
            .unwrap()
            .is_some());
        if corrupt_after {
            put(f.db(), PART, &original);
        }
        adapter.mode = Mode::Good;
        push_batch(f.db(), &mut adapter).unwrap();
        assert_eq!(first.calls()[0].id, adapter.calls()[1].id);
        assert_eq!(dirty(f.db()), 0);
        assert!(f
            .db()
            .sys_record_get(state::MIR_NS, OUTBOX)
            .unwrap()
            .is_none());
        f.db().verify().unwrap();
    }
}
#[test]
fn unrelated_local_tables_do_not_invalidate_a_prepared_request() {
    let f = Fixture::new("local-table");
    let mut adapter = Adapter::new(Mode::Fail);
    assert!(push_batch(f.db(), &mut adapter).is_err());
    f.db()
        .query("CREATE TABLE extra_local(id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    adapter.mode = Mode::Good;
    push_batch(f.db(), &mut adapter).unwrap();
    assert_eq!(adapter.calls()[0].id, adapter.calls()[1].id);
    assert_eq!(dirty(f.db()), 0);
}

#[test]
fn uncaptured_resurrection_and_legacy_row_changes_cannot_be_acknowledged() {
    for deleting in [false, true] {
        let f = Fixture::new(if deleting {
            "untracked-delete"
        } else {
            "untracked-legacy"
        });
        if deleting {
            f.db().query("DELETE FROM t WHERE id=1", &[]).unwrap();
        }
        let mut adapter = Adapter::new(if deleting {
            Mode::UntrackedDelete
        } else {
            Mode::UntrackedLegacyUpsert
        });
        adapter.db = Some(f.db());
        let error = push_batch(f.db(), &mut adapter).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without a dirty-generation update"),
            "{error}"
        );
        assert_eq!(rows(f.db())[0][1], Value::Int(77));
        assert_eq!(dirty(f.db()), 1);
        assert!(f
            .db()
            .sys_record_get(state::MIR_NS, OUTBOX)
            .unwrap()
            .is_some());
    }
}

#[test]
fn a_stale_ack_cannot_remove_a_new_request_prepared_by_another_pusher() {
    let f = Fixture::new("stale-ack");
    let mut adapter = Adapter::new(Mode::RaceNewRequest);
    adapter.db = Some(f.db());
    let error = push_batch(f.db(), &mut adapter).unwrap_err();
    assert!(error.to_string().contains("request changed"), "{error}");
    let header = f
        .db()
        .sys_record_get(state::MIR_NS, OUTBOX)
        .unwrap()
        .unwrap();
    assert_ne!(&header[9..], &adapter.calls()[0].id);
    assert_eq!(dirty(f.db()), 1);
    assert_eq!(
        rows(f.db()),
        vec![vec![Value::Int(1), Value::Int(20), Value::Int(0)]]
    );
    let mut finish = Adapter::new(Mode::Good);
    push_batch(f.db(), &mut finish).unwrap();
    assert_eq!(&header[9..], &finish.calls()[0].id);
    assert_eq!(dirty(f.db()), 0);
    assert_eq!(rows(f.db())[0][2], Value::Int(21));
}
