//! Oracle PL/SQL host-side frontend. The lexer retains exact numeric text,
//! national-string identity, quoted names, and source byte spans. Parsing and
//! execution use the shared validated, budgeted procedure IR. The first
//! compiler handles NUMBER/BOOLEAN scalar routines and structured control
//! flow; unsupported constructs produce located errors.
//!
//! Specification: Oracle's public PL/SQL Language Reference, "Lexical Units",
//! and SQL Language Reference, "Literals". No Oracle implementation source.

pub mod lex;
mod compile;
pub use compile::compile;

#[cfg(test)]
mod tests;
