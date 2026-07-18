//! Drives CPython's built-in `sqlite3` module against the shim via `LD_PRELOAD`
//! (`tests/py_sqlite3_preload.py`) — the headline milestone: an unmodified
//! `_sqlite3` C extension resolves its `sqlite3_*` symbols to mpedb.
//!
//! Skips (does not fail) when `python3` is absent, or when a baseline
//! `import sqlite3` fails without the preload (a Python built without the
//! sqlite module), so the suite stays green in a minimal environment. A macro
//! that only matters on Linux: `LD_PRELOAD` is a no-op elsewhere, so the test
//! also skips on non-Linux hosts.

use std::path::PathBuf;
use std::process::Command;

fn python3() -> Option<String> {
    for p in ["python3", "python"] {
        let ok = Command::new(p)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(p.to_string());
        }
    }
    None
}

/// The directory holding `libmpedb_sqlite3.so`, two levels up from the test
/// binary (`target/<profile>/deps/<bin>` → `target/<profile>`).
fn cdylib_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?;
    let so = profile_dir.join("libmpedb_sqlite3.so");
    so.exists().then_some(so)
}

#[test]
fn py_sqlite3_preload() {
    if !cfg!(target_os = "linux") {
        eprintln!("skip py_sqlite3_preload: LD_PRELOAD path is Linux-only");
        return;
    }
    let Some(py) = python3() else {
        eprintln!("skip py_sqlite3_preload: no python3 found");
        return;
    };

    // Baseline: does this Python even have the sqlite3 module (without preload)?
    let baseline = Command::new(&py)
        .args(["-c", "import sqlite3"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !baseline {
        eprintln!("skip py_sqlite3_preload: python3 has no working sqlite3 module");
        return;
    }

    // Refresh the cdylib (the test harness builds the rlib, not necessarily the
    // cdylib the preload needs). Best-effort — fall back to whatever exists.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let _ = Command::new(&cargo)
        .args(["build", "-p", "mpedb-capi", "--lib"])
        .status();

    let Some(so) = cdylib_path() else {
        eprintln!("skip py_sqlite3_preload: libmpedb_sqlite3.so not found next to the test binary");
        return;
    };

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/py_sqlite3_preload.py");
    let output = Command::new(&py)
        .arg(&script)
        .env("LD_PRELOAD", &so)
        .output()
        .expect("run python3 under LD_PRELOAD");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("py stdout:\n{stdout}");
    if !output.status.success() {
        eprintln!("py stderr:\n{stderr}");
    }
    assert!(output.status.success(), "python sqlite3 preload script failed");
    // The exact target output: `lastrowid` then the fetched row, then OK.
    assert!(stdout.contains('1'), "expected lastrowid 1 in output");
    assert!(stdout.contains("[(1, 'x')]"), "expected fetched row [(1, 'x')]");
    assert!(stdout.trim_end().ends_with("OK"), "expected final OK");
}
