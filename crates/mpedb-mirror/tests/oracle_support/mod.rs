use mpedb_mirror::{oracle::OracleTable, oracle_source::OracleSource};
use mpedb_types::Value;

/// Start capture only after a successful complete bootstrap snapshot. Oracle
/// can map an immediate post-DDL SCN to a timestamp before that DDL. Retrying
/// bootstrap is safe: there is no established cursor to skip or advance yet.
/// Once established, capture errors must propagate, never trigger this retry.
pub fn initial_snapshot(
    source: &OracleSource,
    tables: &[OracleTable],
) -> (u64, Vec<Vec<Vec<Value>>>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let scn = source.current_scn().unwrap();
        let result = tables
            .iter()
            .map(|table| source.read_snapshot(table, scn))
            .collect::<mpedb_types::Result<Vec<_>>>();
        match result {
            Ok(rows) => return (scn, rows),
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("initial Oracle snapshot failed: {e}"),
        }
    }
}
