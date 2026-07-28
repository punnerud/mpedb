//! Smoke run of `mpedb map-collide` (the rRETL map-sync SIGKILL fuzz)
//! against the built binary, per the convention that multi-process
//! behavior is tested through the CLI harnesses. Small parameters — the
//! point here is that the harness runs end-to-end and converges; long
//! tortures are run by hand.

use std::path::PathBuf;
use std::process::Command;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> TestDir {
        let dir = mpedb_testkit::scratch_base()
            .join(format!("mpedb-map-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writers churn both sides while the syncer is killed at every instant;
/// the run's own final drain asserts convergence (echo == 0, check clean,
/// fsck clean, counts equal) and exits nonzero on any divergence.
#[test]
fn map_collide_sigkill_fuzz() {
    let td = TestDir::new();
    let o = Command::new(env!("CARGO_BIN_EXE_mpedb"))
        .args([
            "map-collide",
            "--dir",
            td.0.to_str().unwrap(),
            "--writers",
            "2",
            "--secs",
            "4",
            "--kill-ms",
            "30",
        ])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "stdout: {out}\nstderr: {err}");
    assert!(out.contains("map-collide:"), "stdout: {out}\nstderr: {err}");
    assert!(
        out.contains("check=clean fsck=clean"),
        "stdout: {out}\nstderr: {err}"
    );
}
