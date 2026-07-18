//! Compiles the C smoke test (`tests/smoke.c`) against the built cdylib and
//! runs it — proof that a plain C libsqlite3 consumer, including the shim's
//! `sqlite3.h` and linking `libmpedb_sqlite3`, drives mpedb end to end.
//!
//! Skips (does not fail) if a C compiler or the cdylib cannot be located, so
//! the suite stays green in a minimal environment.

use std::path::{Path, PathBuf};
use std::process::Command;

fn cc() -> Option<String> {
    for c in ["cc", "gcc", "clang"] {
        if Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c.to_string());
        }
    }
    None
}

/// The directory containing `libmpedb_sqlite3.{so,dylib}` — the profile dir two
/// levels up from the test binary (`target/<profile>/deps/<bin>`).
fn cdylib_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // .../target/<profile>
    for name in ["libmpedb_sqlite3.so", "libmpedb_sqlite3.dylib"] {
        if profile_dir.join(name).exists() {
            return Some(profile_dir.to_path_buf());
        }
    }
    None
}

#[test]
fn c_consumer_drives_mpedb() {
    let Some(cc) = cc() else {
        eprintln!("skip c_smoke: no C compiler found");
        return;
    };
    // `cargo test` builds the lib as an rlib for the test harness but does not
    // necessarily refresh the cdylib the C test links against. Rebuild it now
    // (the build lock is free during the test-run phase). Best-effort: if cargo
    // is absent we fall back to whatever cdylib already exists.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let _ = Command::new(&cargo)
        .args(["build", "-p", "mpedb-capi", "--lib"])
        .status();

    let Some(libdir) = cdylib_dir() else {
        eprintln!("skip c_smoke: libmpedb_sqlite3 not found next to the test binary");
        return;
    };

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("tests/smoke.c");
    let include = manifest.join("include");
    let out = std::env::temp_dir().join(format!("mpedb_capi_smoke_{}", std::process::id()));

    let status = Command::new(&cc)
        .arg(&src)
        .arg("-I")
        .arg(&include)
        .arg("-L")
        .arg(&libdir)
        .arg("-lmpedb_sqlite3")
        .arg(format!("-Wl,-rpath,{}", libdir.display()))
        .arg("-o")
        .arg(&out)
        .status()
        .expect("run C compiler");
    assert!(status.success(), "compile smoke.c");

    let output = Command::new(&out)
        .env("LD_LIBRARY_PATH", &libdir)
        .env("DYLD_LIBRARY_PATH", &libdir)
        .output()
        .expect("run smoke binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("smoke stdout:\n{stdout}");
    if !output.status.success() {
        eprintln!("smoke stderr:\n{stderr}");
    }
    let _ = std::fs::remove_file(&out);

    assert!(output.status.success(), "smoke.c exited non-zero");
    assert!(stdout.contains("row: id=1 name=ada"), "expected row output");
    assert!(stdout.contains("row: id=3 name=linus"), "expected row output");
    assert!(stdout.trim_end().ends_with("OK"), "expected final OK");
    let _ = Path::new(".");
}
