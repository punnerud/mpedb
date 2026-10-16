//! Oracle PL/SQL host-side frontend. The lexer retains exact numeric text,
//! national-string identity, quoted names, and source byte spans. Parsing and
//! execution are added separately; tokenization alone is not PL/SQL support.
//!
//! Specification: Oracle's public PL/SQL Language Reference, "Lexical Units",
//! and SQL Language Reference, "Literals". No Oracle implementation source.

pub mod lex;
