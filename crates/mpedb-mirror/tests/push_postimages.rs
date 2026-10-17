//! Source-returned generated values must not clobber newer local intent.
use mpedb_mirror::{
    adapter::{Cursor, NetOp, NetOpKind, PullBatch},
    import_sqlite, push_batch, state, ImportOptions, PushContext, PushReply, SourceAdapter,
};
use mpedb_types::{Error, Result, Value};
#[derive(Clone, Copy)]
enum During {
    None,
    Redirty,
    Delete,
    Epoch,
    Config,
    Schema,
    LocalTable,
    Uncaptured,
    Reject,
    RejectAndRedirty,
    BadKey,
    BadWidth,
    BadType,
    SecondBadType,
    BadCount,
    MissingImage,
}
struct Adapter<'a> {
    db: &'a mpedb::Database,
    during: During,
}
impl SourceAdapter for Adapter<'_> {
    fn pull(&mut self, _: &Cursor, _: usize) -> Result<Option<PullBatch>> {
        Ok(None)
    }
    fn head(&mut self) -> Result<Cursor> {
        Ok(vec![0])
    }
    fn zero_cursor(&self) -> Cursor {
        vec![0]
    }
    fn read_table_rows(&mut self, _: u32) -> Result<Vec<Vec<Value>>> {
        Err(Error::Unsupported("unused".into()))
    }
    fn push(&mut self, _: &[NetOp]) -> Result<Vec<bool>> {
        panic!("versioned boundary required")
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
    fn push_versioned(
        &mut self,
        from: Option<&Cursor>,
        ops: &[NetOp],
        context: &PushContext,
    ) -> Result<PushReply> {
        assert!(from.is_some());
        assert_eq!(context.generations.len(), ops.len());
        assert!(context.generations.iter().all(|&g| g > 0));
        assert!(context.epoch.is_some());
        match self.during {
            During::Redirty | During::RejectAndRedirty => {
                self.db.query("UPDATE t SET v=11,g=99 WHERE id=1", &[])?;
            }
            During::Delete => {
                self.db.query("DELETE FROM t WHERE id=1", &[])?;
            }
            During::Epoch => {
                let mut epoch = context.epoch.unwrap();
                epoch.epoch += 1;
                let mut s = self.db.begin()?;
                s.set_capture(false);
                s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;
                s.commit()?;
            }
            During::Config => {
                let mut config = state::MirrorConfig::decode(
                    &self
                        .db
                        .sys_record_get(state::MIR_NS, state::KEY_CFG)?
                        .unwrap(),
                )?;
                config.mirror_id[0] ^= 1;
                let mut s = self.db.begin()?;
                s.set_capture(false);
                s.sys_record_put(state::MIR_NS, state::KEY_CFG, &config.encode())?;
                s.commit()?;
            }
            During::Schema => {
                self.db.query("ALTER TABLE t ADD COLUMN extra TEXT", &[])?;
            }
            During::LocalTable => {
                self.db
                    .query("CREATE TABLE local_metadata(id INTEGER PRIMARY KEY)", &[])?;
            }
            During::Uncaptured => {
                let mut s = self.db.begin()?;
                s.set_capture(false);
                s.query("UPDATE t SET v=11,g=99 WHERE id=1", &[])?;
                s.commit()?;
            }
            _ => {}
        }
        let reject = matches!(self.during, During::Reject | During::RejectAndRedirty);
        let images = ops
            .iter()
            .map(|op| match &op.kind {
                NetOpKind::Delete => None,
                NetOpKind::Upsert(row) if !reject => {
                    let mut actual = row.clone();
                    let Value::Int(v) = actual[1] else {
                        panic!("int")
                    };
                    actual[2] = if matches!(actual[2], Value::Float(_)) {
                        Value::Float(2.0 * v as f64)
                    } else {
                        Value::Int(2 * v)
                    };
                    match self.during {
                        During::BadKey => actual[0] = Value::Int(99),
                        During::BadWidth => {
                            actual.pop();
                        }
                        During::BadType => actual[2] = Value::Text("invalid".into()),
                        _ => {}
                    }
                    if matches!(self.during, During::SecondBadType) && op.pk == vec![Value::Int(2)]
                    {
                        actual[2] = Value::Text("invalid".into());
                    }
                    if matches!(self.during, During::MissingImage) {
                        None
                    } else {
                        Some(actual)
                    }
                }
                _ => None,
            })
            .collect();
        Ok(PushReply {
            applied: if matches!(self.during, During::BadCount) {
                Vec::new()
            } else {
                vec![!reject; ops.len()]
            },
            postimages: Some(images),
        })
    }
}
fn fixture(name: &str) -> (mpedb::Database, std::path::PathBuf, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let source = dir.join(format!("postimage-{name}-{}.sqlite", std::process::id()));
    let dest = source.with_extension("mpedb");
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&dest);
    let mut c = rusqlite::Connection::open(&source).unwrap();
    let generated_type = if name == "nan" { "REAL" } else { "INTEGER" };
    c.execute_batch(&format!("CREATE TABLE t(id INTEGER PRIMARY KEY,v INTEGER,g {generated_type});INSERT INTO t VALUES(1,0,0.0)"))
    .unwrap();
    let (db, _) = import_sqlite(
        &mut c,
        &dest,
        &ImportOptions {
            size_bytes: 32 * 1024 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    db.query("UPDATE t SET v=10 WHERE id=1", &[]).unwrap();
    (db, source, dest)
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
        .filter(|(k, _)| k.starts_with(b"d/"))
        .count()
}
fn cleanup(db: mpedb::Database, source: std::path::PathBuf, dest: std::path::PathBuf) {
    drop(db);
    std::fs::remove_file(source).unwrap();
    std::fs::remove_file(dest).unwrap();
}
#[test]
fn generated_postimages_ack_atomically_and_clear_old_rejections() {
    let (db, source, dest) = fixture("normal");
    let mut adapter = Adapter {
        db: &db,
        during: During::Reject,
    };
    assert_eq!(push_batch(&db, &mut adapter).unwrap().conflicts, 1);
    assert_eq!(dirty(&db), 1);
    assert_eq!(mpedb_mirror::conflicts::list(&db).unwrap().len(), 1);
    adapter.during = During::None;
    assert_eq!(push_batch(&db, &mut adapter).unwrap().upserts, 1);
    assert_eq!(
        rows(&db),
        vec![vec![Value::Int(1), Value::Int(10), Value::Int(20)]]
    );
    assert_eq!(dirty(&db), 0);
    assert!(mpedb_mirror::conflicts::list(&db).unwrap().is_empty());
    cleanup(db, source, dest);
}
#[test]
fn newer_local_update_delete_and_rejection_are_not_overwritten_or_parked() {
    for (name, mode) in [
        ("redirty", During::Redirty),
        ("delete", During::Delete),
        ("reject-redirty", During::RejectAndRedirty),
    ] {
        let (db, source, dest) = fixture(name);
        let mut adapter = Adapter {
            db: &db,
            during: mode,
        };
        push_batch(&db, &mut adapter).unwrap();
        assert_eq!(dirty(&db), 1);
        assert!(mpedb_mirror::conflicts::list(&db).unwrap().is_empty());
        if matches!(mode, During::Delete) {
            assert!(rows(&db).is_empty());
        } else {
            assert_eq!(
                rows(&db),
                vec![vec![Value::Int(1), Value::Int(11), Value::Int(99)]]
            );
        }
        adapter.during = During::None;
        push_batch(&db, &mut adapter).unwrap();
        assert_eq!(dirty(&db), 0);
        if !matches!(mode, During::Delete) {
            assert_eq!(rows(&db)[0][2], Value::Int(22));
        }
        cleanup(db, source, dest);
    }
}
#[test]
fn authority_schema_and_untracked_row_changes_preserve_pending_intent() {
    for (name, mode) in [
        ("epoch", During::Epoch),
        ("config", During::Config),
        ("schema", During::Schema),
        ("untracked", During::Uncaptured),
    ] {
        let (db, source, dest) = fixture(name);
        let mut adapter = Adapter {
            db: &db,
            during: mode,
        };
        assert!(push_batch(&db, &mut adapter).is_err());
        assert_eq!(dirty(&db), 1);
        assert_eq!(
            rows(&db)[0][2],
            if matches!(mode, During::Uncaptured) {
                Value::Int(99)
            } else {
                Value::Int(0)
            }
        );
        cleanup(db, source, dest);
    }
}
#[test]
fn unrelated_tables_are_allowed_and_malformed_postimages_never_ack() {
    for (name, mode) in [
        ("local-table", During::LocalTable),
        ("bad-key", During::BadKey),
        ("bad-width", During::BadWidth),
        ("bad-type", During::BadType),
        ("bad-count", During::BadCount),
        ("missing-image", During::MissingImage),
    ] {
        let (db, source, dest) = fixture(name);
        let mut adapter = Adapter {
            db: &db,
            during: mode,
        };
        if matches!(mode, During::LocalTable) {
            push_batch(&db, &mut adapter).unwrap();
            assert_eq!(dirty(&db), 0);
            assert_eq!(rows(&db)[0][2], Value::Int(20));
        } else {
            assert!(push_batch(&db, &mut adapter).is_err());
            assert_eq!(dirty(&db), 1);
            assert_eq!(rows(&db)[0][2], Value::Int(0));
        }
        cleanup(db, source, dest);
    }
}

#[test]
fn nan_snapshot_can_receive_a_generated_postimage_without_false_conflict() {
    let (db, source, dest) = fixture("nan");
    db.query("UPDATE t SET g=$1 WHERE id=1", &[Value::Float(f64::NAN)])
        .unwrap();
    let mut adapter = Adapter {
        db: &db,
        during: During::None,
    };
    assert_eq!(push_batch(&db, &mut adapter).unwrap().upserts, 1);
    assert_eq!(dirty(&db), 0);
    assert_eq!(rows(&db)[0][2], Value::Float(20.0));
    cleanup(db, source, dest);
}

#[test]
fn failed_second_postimage_rolls_back_the_whole_local_ack_transaction() {
    let (db, source, dest) = fixture("atomic-ack");
    db.query("INSERT INTO t VALUES(2,7,0)", &[]).unwrap();
    let mut adapter = Adapter {
        db: &db,
        during: During::SecondBadType,
    };
    assert!(push_batch(&db, &mut adapter).is_err());
    assert_eq!(dirty(&db), 2);
    assert_eq!(
        rows(&db),
        vec![
            vec![Value::Int(1), Value::Int(10), Value::Int(0)],
            vec![Value::Int(2), Value::Int(7), Value::Int(0)]
        ]
    );
    adapter.during = During::None;
    assert_eq!(push_batch(&db, &mut adapter).unwrap().upserts, 2);
    assert_eq!(dirty(&db), 0);
    assert_eq!(rows(&db)[0][2], Value::Int(20));
    assert_eq!(rows(&db)[1][2], Value::Int(14));
    cleanup(db, source, dest);
}
