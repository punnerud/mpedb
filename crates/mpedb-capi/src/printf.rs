//! The exported names for the C-variadic four in `printf.c`.
//!
//! `sqlite3_mprintf`, `sqlite3_snprintf` and their `va_list` forms cannot be
//! written in Rust: defining a C-variadic function is still unstable
//! (`error[E0658]`, checked on 1.96). They cannot simply be exported from C
//! either — rustc builds a cdylib's export list from the `#[no_mangle]`
//! symbols it knows about, and binds everything else local, so a name defined
//! only in the C object lands as `t` in `nm`: linked in, but invisible to any
//! consumer that tries to bind it.
//!
//! So Rust owns the names and C owns the bodies. Each export is a naked thunk
//! that tail-jumps to its implementation. Naked is what makes this ABI-exact:
//! no prologue runs, so every argument register, the stack, and `al` — the
//! vector-register count that a variadic callee's own prologue reads to build
//! its register save area — reach the implementation exactly as the caller
//! left them. A normal `extern "C"` wrapper could not forward `...` at all.

use std::arch::naked_asm;

extern "C" {
    fn mpedb_capi_mprintf();
    fn mpedb_capi_vmprintf();
    fn mpedb_capi_snprintf();
    fn mpedb_capi_vsnprintf();
}

// The tail branch is spelled differently per architecture — x86-64 `jmp`,
// AArch64 `b` — and an aarch64 assembler answers "unrecognized instruction
// mnemonic, did you mean: cmp?" to the x86 one. Both instructions do the same
// thing that matters here: transfer control WITHOUT touching a register or the
// stack, so the callee sees precisely what the caller set up. On x86-64 that
// includes `al`; on AArch64 it includes the variadic area the caller has
// already laid out.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "the printf thunks need a tail-branch mnemonic for this architecture; \
     add an arm to `thunk!` rather than letting the naked body come out empty"
);

macro_rules! thunk {
    ($name:ident => $imp:ident, $doc:literal) => {
        #[doc = $doc]
        #[unsafe(naked)]
        #[no_mangle]
        pub unsafe extern "C" fn $name() {
            #[cfg(target_arch = "x86_64")]
            naked_asm!("jmp {}", sym $imp);
            #[cfg(target_arch = "aarch64")]
            naked_asm!("b {}", sym $imp);
        }
    };
}

thunk!(sqlite3_mprintf => mpedb_capi_mprintf,
    "`sqlite3_mprintf(fmt, ...)`: format into a fresh buffer the caller frees \
     with `sqlite3_free`. Supports sqlite's `%q`/`%Q`/`%w`/`%z` on top of the \
     platform conversions, and renders a NULL `%s` as empty rather than \
     `(null)`.");
thunk!(sqlite3_vmprintf => mpedb_capi_vmprintf,
    "`sqlite3_vmprintf(fmt, ap)`: the `va_list` form of [`sqlite3_mprintf`].");
thunk!(sqlite3_snprintf => mpedb_capi_snprintf,
    "`sqlite3_snprintf(n, buf, fmt, ...)`: format into a caller-supplied \
     buffer. Note sqlite's argument order — size first — and that it returns \
     the buffer, not a length.");
thunk!(sqlite3_vsnprintf => mpedb_capi_vsnprintf,
    "`sqlite3_vsnprintf(n, buf, fmt, ap)`: the `va_list` form of \
     [`sqlite3_snprintf`].");
