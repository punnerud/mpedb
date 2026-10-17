//! Opt-in Oracle CLI import, bounded pull, and ledger-pinned push.
//! Sync and authority cutover remain refused.
use super::*;
use mpedb_mirror::{import_oracle, oracle_source::OracleSource, OracleAdapter, PushStats};
use std::path::Path;
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
pub(super) fn pull(db_path: &str, p: &args::Parsed) -> Result<Option<u64>, Failure> {
    let db = Database::open_from_file(Path::new(db_path))?;
    let spec = resolve_spec(&db, p)?;
    if !matches!(spec, SourceSpec::Oracle { .. }) {
        return Ok(None);
    }
    let mut adapter = OracleAdapter::from_database(OracleSource::connect(&spec)?, &db)?;
    require_matching_source(&db, &mut adapter, "pull")?;
    let through = adapter.head()?;
    let mut applied = 0;
    loop {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)?
            .ok_or_else(|| Failure::Runtime("Oracle cursor is missing".into()))?;
        let Some(batch) = adapter.pull_through_provenanced(&from, &through, 100_000)? else {
            break;
        };
        let stats = mpedb_mirror::apply_provenanced_batch(&db, &from, &batch)?;
        applied += stats.upserts + stats.deletes;
    }
    Ok(Some(applied))
}
pub(super) fn push(db_path: &str, p: &args::Parsed) -> Result<Option<PushStats>, Failure> {
    let db = Database::open_from_file(Path::new(db_path))?;
    let spec = resolve_spec(&db, p)?;
    if !matches!(spec, SourceSpec::Oracle { .. }) {
        return Ok(None);
    }
    let mut adapter = OracleAdapter::from_database(OracleSource::connect(&spec)?, &db)?;
    require_matching_source(&db, &mut adapter, "push")?;
    if adapter.push_outbox_identity().is_none() {
        return Err(Failure::Runtime(
            "Oracle push requires receipt and origin ledgers pinned at import; reimport with --pin-ledgers"
                .into(),
        ));
    }
    // No recovery-on-attach: that path completes a half-finished cutover.
    Ok(Some(drain_push(&db, &mut adapter)?))
}
