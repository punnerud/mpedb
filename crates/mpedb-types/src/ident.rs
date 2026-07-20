//! SQL identifier folding — sqlite's rule, measured, not remembered.
//!
//! Everything measured below is against the BUNDLED oracle (sqlite 3.45.0,
//! `crates/mpedb/tests/sqlite_oracle/mod.rs`); the differential battery lives in
//! `crates/mpedb/tests/ident_case.rs`.
//!
//! # The rule
//!
//! An identifier **comparison** in SQL is case-insensitive over **ASCII only**.
//! Three parts, each of which the oracle contradicts a plausible guess about:
//!
//! 1. **Quoting does not matter.** `"T"`, `[T]`, `` `T` `` and bare `T` all
//!    resolve to the same name as `t`. Quoting in sqlite buys you spellings a
//!    bare word cannot have (spaces, keywords, punctuation) — it does *not* buy
//!    case sensitivity. Measured: `CREATE TABLE "t"(a); SELECT * FROM "T";`
//!    succeeds, and `CREATE TABLE t("a" INT, "A" INT)` is
//!    `duplicate column name: A`.
//!
//! 2. **ASCII only.** `Æ` and `æ` are *different* identifiers; so are `k` and
//!    U+212A KELVIN SIGN, and `i` and `İ`. Reaching for Rust's Unicode-aware
//!    [`str::to_lowercase`] here would silently merge names sqlite keeps apart —
//!    a wrong answer, not an error. Hence [`fold_ident`] is spelled with
//!    `to_ascii_lowercase` and [`ident_eq`] with `eq_ignore_ascii_case`, and
//!    there is a regression test for each of those three characters.
//!
//! 3. **Folding is for LOOKUP ONLY.** Names are *stored and reported verbatim*,
//!    in the spelling they were declared with. `CREATE TABLE MiXeD(Abc INT)`
//!    then `SELECT * FROM MIXED` reports the column as `Abc`, and
//!    `sqlite_master.name` is `MiXeD`. Folding a stored name would relabel every
//!    result column — a wrong answer wearing a compatibility fix's clothes.
//!    So: never write `fold_ident(...)` into a [`crate::schema::Schema`], a plan,
//!    or an error message. Fold on the *comparison*, not on the way in.
//!
//! # Not this
//!
//! `COLLATE NOCASE` is about **values**, not identifiers, and is a different
//! mechanism entirely ([`crate::schema::Collation`]). It happens to be ASCII-only
//! too (`'æ' = 'Æ' COLLATE NOCASE` is 0), but nothing here routes through it and
//! nothing there routes through here.

/// Fold an identifier to its lookup key: ASCII-lowercase, everything else
/// verbatim. Use for hashing/sorting identifiers; use [`ident_eq`] to compare
/// two directly (no allocation).
///
/// **Never store the result.** See the module docs: declared spelling is what
/// gets reported back.
#[inline]
#[must_use]
pub fn fold_ident(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Do these two identifiers name the same thing? sqlite's rule: ASCII-only
/// case-insensitive comparison, regardless of how either was quoted.
#[inline]
#[must_use]
pub fn ident_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_folds() {
        assert!(ident_eq("MyTab", "mytab"));
        assert!(ident_eq("T", "t"));
        assert!(ident_eq("t_1", "T_1"));
        assert_eq!(fold_ident("MiXeD"), "mixed");
    }

    #[test]
    fn non_ascii_does_not_fold() {
        // Every one of these is a name sqlite 3.45.0 keeps DISTINCT. A
        // Unicode-aware fold would merge them and answer the wrong row.
        assert!(!ident_eq("\u{e6}", "\u{c6}"), "æ/Æ must stay distinct");
        assert!(!ident_eq("k", "\u{212a}"), "k/KELVIN SIGN must stay distinct");
        assert!(!ident_eq("i", "\u{130}"), "i/İ must stay distinct");
        assert!(!ident_eq("stra\u{df}e", "STRASSE"), "ß must not expand");
        assert_eq!(fold_ident("\u{c6}x"), "\u{c6}x", "Æ survives folding as-is");
        // The trap, stated as an assertion: Rust's Unicode fold disagrees.
        assert_ne!(fold_ident("\u{c6}"), "\u{c6}".to_lowercase());
    }
}
