//! The PostgreSQL dialect's own surface — everything sqlite never had.
//!
//! # Why this is a module and not a scattering of `if dialect == Postgres`
//!
//! mpedb's sqlite compatibility is four measured 100 % scores, and the way to
//! keep them is to make the PG surface *addable and removable as one piece*.
//! Everything PG-only lives under this module; the rest of the crate reaches it
//! through a handful of named entry points ([`lex::special`],
//! [`catalog::views`], [`funcs::resolve`], [`types::column_type`]). The build
//! toggle (`--features pg-dialect`) therefore has ONE seam to cut, not fifty.
//!
//! # The invariant the feature must not break
//!
//! The cargo feature decides whether this surface is **compiled in**. It must
//! never change the plan format, the row format, or the schema's canonical
//! bytes — otherwise a file written by one build would be unreadable by
//! another, which is a far worse failure than a missing feature.
//!
//! Concretely: [`mpedb_types::Dialect::Postgres`] and its persisted byte exist
//! in BOTH builds. With the feature off, selecting it is a named refusal at
//! prepare time ([`unavailable`]) — not a missing enum variant.

#[cfg(feature = "pg-dialect")]
pub mod api;
#[cfg(feature = "pg-dialect")]
pub(crate) mod catalog;
#[cfg(feature = "pg-dialect")]
pub(crate) mod funcs;
#[cfg(feature = "pg-dialect")]
pub(crate) mod lex;
#[cfg(feature = "pg-dialect")]
pub(crate) mod types;

/// Stubs for the build without the feature.
///
/// They are **INERT**, not refusals, and that distinction is the whole
/// correctness argument for the toggle.
///
/// `Dialect::Postgres` is older than this feature. It already selected five
/// documented behaviours — bare-column strictness under GROUP BY, case-SENSITIVE
/// `LIKE`, rigid boolean typing, static `CASE`/`COALESCE` arm promotion, and no
/// constant coercion into a column slot (COMPAT.md) — and a PostgreSQL-imported
/// mirror is born under it. None of that is part of the surface this feature
/// gates, and none of it may stop working because the feature is off.
///
/// So a build without `pg-dialect` still HAS the PostgreSQL dialect; what it
/// lacks is the PostgreSQL-only *surface*. `::` goes back to being an
/// unterminated `:sym:`, `pg_class` goes back to being an unknown table, and
/// `version()` goes back to being an unknown function — each with the message it
/// had before this module existed.
///
/// Making these refuse instead was tried and caught immediately by
/// `crates/mpedb/tests/group_by_dialect.rs`, which has asserted the old
/// behaviour since #87: three of its cases turned into "the postgres dialect is
/// not compiled into this build" for statements that had nothing to do with the
/// new surface.
#[cfg(not(feature = "pg-dialect"))]
pub(crate) mod lex {
    use crate::token::Tok;
    use mpedb_types::Result;

    /// Claims no bytes: the sqlite arms below the call site decide everything,
    /// which is exactly what they did before this module existed.
    pub(crate) fn special(_b: &[u8], _i: usize) -> Result<Option<(Tok, usize)>> {
        Ok(None)
    }
}

#[cfg(not(feature = "pg-dialect"))]
pub(crate) mod types {
    /// Recognises nothing, so `serial` falls through to sqlite's affinity rule
    /// exactly as it did before this module existed.
    pub(crate) fn is_serial(_decl: &str) -> bool {
        false
    }

    /// INERT, per the rule above: without the feature there is no PG type
    /// table to resolve against, so the declared name goes back through
    /// sqlite's affinity rule — which is what the DDL did before the fork.
    /// Refusing here would make a build without the feature reject schemas a
    /// build with it accepts, for types that have nothing to do with it.
    pub(crate) fn column_type(decl: &str) -> mpedb_types::Result<mpedb_types::ColumnType> {
        Ok(mpedb_types::ColumnType::declared(decl).0)
    }
}

#[cfg(not(feature = "pg-dialect"))]
pub(crate) mod funcs {
    use mpedb_types::{Result, Value};

    /// Present in both builds so the binder's call site is ordinary code with
    /// no `#[cfg]` around it. Never constructed here — the variants exist so
    /// the `match` in `bind_func` compiles identically either way.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) enum PgFunc {
        Const(Value),
        ConstOfAny(Value),
        FirstArg,
        TypeOf,
        AlwaysTrue,
        Alias(&'static str),
        AliasSwap2(&'static str),
        Scalar(mpedb_types::ScalarFn),
    }

    /// Resolves nothing: every name falls through to the ordinary function
    /// table, so `version()` is an unknown function with the ordinary message
    /// rather than a complaint about a build flag.
    pub(crate) fn resolve(_name: &str, _argc: usize) -> Option<Result<PgFunc>> {
        None
    }
}
