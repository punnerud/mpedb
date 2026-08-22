//! Builds the C-variadic half of the shim.
//!
//! `sqlite3_mprintf` / `sqlite3_snprintf` and their `va_list` forms are
//! C-variadic, and defining a C-variadic function is still unstable in Rust
//! (`error[E0658]`, checked on 1.96), so those four entry points live in
//! `src/printf.c`. Everything else in this crate is Rust.
//!
//! The object is handed to the linker DIRECTLY rather than as the static
//! archive `cc::Build::compile` would produce. That is load-bearing: rustc
//! links a cdylib with `--exclude-libs,ALL`, which makes every symbol coming
//! out of a static ARCHIVE local. The symbols would still be in the library
//! (nm shows them as 't') but no consumer could ever bind to them — the exact
//! failure this file exists to fix. `--exclude-libs` does not apply to loose
//! object files, so passing the object straight through keeps them global.

fn main() {
    println!("cargo:rerun-if-changed=src/printf.c");

    let objs = cc::Build::new()
        .file("src/printf.c")
        .warnings(true)
        // No LTO: these entry points have no call site in this crate — C
        // consumers reach them through the dynamic symbol table — so LTO would
        // see them as dead and internalize them.
        .flag_if_supported("-fno-lto")
        .compile_intermediates();

    for o in &objs {
        println!("cargo:rustc-link-arg-cdylib={}", o.display());
    }

    // No --undefined roots are needed: the naked thunks in printf.rs reference
    // each implementation by symbol, which is a real relocation, so the linker
    // keeps them and --gc-sections leaves them alone.
}
