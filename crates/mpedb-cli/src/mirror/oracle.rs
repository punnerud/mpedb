//! Opt-in Oracle CLI import and bounded pull. Push/cutover remain refused.
use super::*;
use mpedb_mirror::{import_oracle, oracle_source::OracleSource, OracleAdapter};
use std::path::Path;
pub(super) fn import(
    spec: &SourceSpec,
    dest: &Path,
    opts: &ImportOptions,
) -> Result<(Database, mpedb_mirror::ImportReport), Failure> {
    let mut source = OracleSource::connect(spec)?;
    Ok(import_oracle(&mut source, dest, opts)?)
}
pub(super) fn pull(db_path: &str, p: &args::Parsed) -> Result<Option<u64>, Failure> {
    let db = Database::open_from_file(Path::new(db_path))?;
    let spec = resolve_spec(&db, p)?;
    if !matches!(spec, SourceSpec::Oracle { .. }) {
        return Ok(None);
    }
    let mut adapter = OracleAdapter::from_database(OracleSource::connect(&spec)?, &db)?;
    let epoch = read_epoch(&db)?;
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
        return Err(Failure::Runtime("Oracle pull requires matching source authority; automatic Oracle cutover recovery is not enabled".into()));
    }
    let through = adapter.head()?;
    let mut applied = 0;
    loop {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)?
            .ok_or_else(|| Failure::Runtime("Oracle cursor is missing".into()))?;
        let Some(batch) = adapter.pull_through(&from, &through, 100_000)? else {
            break;
        };
        let stats = mpedb_mirror::apply_batch(&db, &from, &batch)?;
        applied += stats.upserts + stats.deletes;
    }
    Ok(Some(applied))
}
