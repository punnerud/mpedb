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

macro_rules! thunk {
    ($name:ident => $imp:ident, $doc:literal) => {
        #[doc = $doc]
        #[unsafe(naked)]
        #[no_mangle]
        pub unsafe extern "C" fn $name() {
            naked_asm!("jmp {}", sym $imp)
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
