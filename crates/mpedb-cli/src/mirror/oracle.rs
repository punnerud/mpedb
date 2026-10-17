//! Opt-in Oracle CLI import, bounded pull, ledger-pinned push, and sync.
//! Authority cutover remains refused: sync never calls recovery-on-attach.
use super::*;
use mpedb_mirror::{import_oracle, oracle_source::OracleSource, OracleAdapter, PushStats};
use std::path::Path;

pub(super) struct OracleSync {
    pub pulled: u64,
    pub push: PushStats,
    pub authority: Authority,
}
pub(super) fn import(
    spec: &SourceSpec,
    dest: &Path,
    opts: &ImportOptions,
    pin_ledgers: bool,
) -> Result<(Database, mpedb_mirror::ImportReport), Failure> {
    let mut source = OracleSource::connect(spec)?;
    // Ledgers are pinned before the snapshot. Push never creates them later:
    // a reconnect must not adopt a different receipt table as if it were the
    // one this file committed against.
    if pin_ledgers {
        source.prepare_write_ledger()?;
        source.prepare_origin_ledger()?;
    }
    Ok(import_oracle(&mut source, dest, opts)?)
}
fn require_matching_source(
    db: &Database,
    adapter: &mut OracleAdapter,
    verb: &str,
) -> Result<(), Failure> {
    let epoch = read_epoch(db)?;
    let cfg = db
        .sys_record_get(state::MIR_NS, state::KEY_CFG)?
        .ok_or_else(|| Failure::Runtime("Oracle mirror config is missing".into()))?;
    let cfg = state::MirrorConfig::decode(&cfg)?;
    let id = cfg
        .mirror_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if epoch.frozen
        || epoch.authority != Authority::Source
        || adapter.read_source_state(&id)? != Some((epoch.epoch, "source".into()))
    {
        return Err(Failure::Runtime(format!(
            "Oracle {verb} requires matching source authority; automatic Oracle cutover recovery is not enabled"
        )));
    }
    Ok(())
}
fn open_oracle(
    db_path: &str,
    p: &args::Parsed,
) -> Result<Option<(Database, OracleAdapter)>, Failure> {
    let db = Database::open_from_file(Path::new(db_path))?;
    let spec = resolve_spec(&db, p)?;
    if !matches!(spec, SourceSpec::Oracle { .. }) {
        return Ok(None);
    }
    let adapter = OracleAdapter::from_database(OracleSource::connect(&spec)?, &db)?;
    Ok(Some((db, adapter)))
}
fn require_pinned_ledgers(adapter: &OracleAdapter, verb: &str) -> Result<(), Failure> {
    if adapter.push_outbox_identity().is_none() {
        return Err(Failure::Runtime(format!(
            "Oracle {verb} requires receipt and origin ledgers pinned at import; reimport with --pin-ledgers"
        )));
    }
    Ok(())
}
/// Pull through one captured head. Echoes advance the cursor and are not counted.
fn pull_bounded(db: &Database, adapter: &mut OracleAdapter) -> Result<u64, Failure> {
    let through = adapter.head()?;
    let mut applied = 0;
    loop {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)?
            .ok_or_else(|| Failure::Runtime("Oracle cursor is missing".into()))?;
        let Some(batch) = adapter.pull_through_provenanced(&from, &through, 100_000)? else {
            break;
        };
        let stats = mpedb_mirror::apply_provenanced_batch(db, &from, &batch)?;
        applied += stats.upserts + stats.deletes;
    }
    Ok(applied)
}
pub(super) fn pull(db_path: &str, p: &args::Parsed) -> Result<Option<u64>, Failure> {
    let Some((db, mut adapter)) = open_oracle(db_path, p)? else {
        return Ok(None);
    };
    require_matching_source(&db, &mut adapter, "pull")?;
    Ok(Some(pull_bounded(&db, &mut adapter)?))
}
pub(super) fn push(db_path: &str, p: &args::Parsed) -> Result<Option<PushStats>, Failure> {
    let Some((db, mut adapter)) = open_oracle(db_path, p)? else {
        return Ok(None);
    };
    require_matching_source(&db, &mut adapter, "push")?;
    require_pinned_ledgers(&adapter, "push")?;
    // No recovery-on-attach: that path completes a half-finished cutover.
    Ok(Some(drain_push(&db, &mut adapter)?))
}
pub(super) fn sync(db_path: &str, p: &args::Parsed) -> Result<Option<OracleSync>, Failure> {
    let Some((db, mut adapter)) = open_oracle(db_path, p)? else {
        return Ok(None);
    };
    // Both refusals run before pull, so a mismatch or a missing ledger cannot
    // apply source rows and then fail on the way back out.
    require_matching_source(&db, &mut adapter, "sync")?;
    require_pinned_ledgers(&adapter, "sync")?;
    let authority = read_epoch(&db)?.authority;
    let pulled = pull_bounded(&db, &mut adapter)?;
    let push = drain_push(&db, &mut adapter)?;
    Ok(Some(OracleSync {
        pulled,
        push,
        authority,
    }))
}
