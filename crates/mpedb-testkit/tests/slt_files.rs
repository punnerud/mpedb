//! Walk `tests/slt/*.test` and run every file through the sqllogictest
//! runner. Any mismatch fails with the file/line/SQL and expected-vs-got.

use mpedb_testkit::run_slt_file;
use std::path::PathBuf;

#[test]
fn corpus_slt_files_pass() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "test"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 12,
        "expected at least 12 corpus files in {}, found {}",
        dir.display(),
        files.len()
    );

    let mut failures = Vec::new();
    let mut total_records = 0;
    for f in &files {
        match run_slt_file(f) {
            Ok(stats) => {
                println!(
                    "{}: {} records ({} statements, {} queries, {} skipped)",
                    f.file_name().unwrap().to_string_lossy(),
                    stats.records,
                    stats.statements,
                    stats.queries,
                    stats.skipped
                );
                assert!(
                    stats.records >= 30,
                    "{}: corpus files must hold 30-100 directives, found {}",
                    f.display(),
                    stats.records
                );
                total_records += stats.records;
            }
            Err(e) => failures.push(e.to_string()),
        }
    }
    println!("total: {} files, {total_records} records", files.len());
    if !failures.is_empty() {
        panic!("{} SLT file(s) failed:\n\n{}", failures.len(), failures.join("\n\n"));
    }
}
