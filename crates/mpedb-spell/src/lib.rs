//! The PySpell layer, database-free.
//!
//! User logic in a small **Python subset** or **Rust subset**, parsed
//! host-side, compiled to a compact sandboxed content-hashed IR, executed by
//! a budgeted interpreter. The parser stays on the host; the runtime only
//! ever sees IR — that is the security boundary (see `ir`'s module docs).
//!
//! This crate is `mpedb-proc`'s bottom half, split out (stage M2) so the
//! engine facade can compile and evaluate stored SQL FUNCTIONS without a
//! dependency cycle: everything here depends only on `mpedb-types`. Database
//! access is abstracted behind [`interp::DbBridge`]; `mpedb-proc` provides
//! the real bridges (snapshot reads, one-WriteSession transactional procs)
//! and the define/link pipeline that turns embedded SQL into plan hashes.

pub mod emit;
pub mod hash;
pub mod interp;
pub mod ir;
pub mod py;
pub mod rs;

pub use hash::ProcHash;
pub use interp::{Budget, DbBridge, ProcValue};
pub use ir::Proc;

#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod reject_tests;
