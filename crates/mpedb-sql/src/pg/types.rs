//! PostgreSQL type names inside SQL — `CREATE TABLE t (a int4)`, `x::numeric`.
//!
//! The table itself lives in [`mpedb_types::pgtype`] because three subsystems
//! need it; this module is only the SQL-facing half: turning a lookup miss or a
//! `None` mapping into the refusal the user should read.

use mpedb_types::pgtype::{self, PgType};
use mpedb_types::{ColumnType, Error, Result};

/// Resolve a declared PG type to an mpedb column type, or refuse by name.
///
/// Three distinct outcomes, and the difference matters to whoever reads the
/// error:
///
/// - resolved → the column type,
/// - known type with no mpedb home (`interval`) → "mpedb has no X",
/// - unknown name → "unknown type X", which is usually a typo or an extension.
pub(crate) fn column_type(decl: &str) -> Result<ColumnType> {
    match pgtype::by_name(decl) {
        Some(PgType {
            mpedb: Some(ty), ..
        }) => Ok(ty),
        Some(PgType { name, .. }) => Err(Error::Unsupported(format!(
            "mpedb has no type that can hold PostgreSQL's `{name}` without \
             losing information — the column must be declared as something else"
        ))),
        None if is_array(decl) => Err(Error::Unsupported(format!(
            "array types are not supported (`{decl}`) — mpedb has no storable \
             array type; use a separate table or a text/json encoding"
        ))),
        None => Err(Error::Unsupported(format!(
            "unknown type `{decl}`"
        ))),
    }
}

fn is_array(decl: &str) -> bool {
    let t = decl.trim();
    t.ends_with("[]") || t.starts_with('_')
}

/// Whether a declared type carries an implicit sequence default — PG's
/// `SERIAL` family.
///
/// mpedb's answer is the rowid-alias rule (#94): a single-column
/// `INTEGER PRIMARY KEY` auto-assigns. That covers `id serial PRIMARY KEY`,
/// which is what the overwhelming majority of real schemas write. A `serial`
/// column that is NOT the primary key has no auto-assignment here, and the DDL
/// binder refuses it by name rather than silently creating a column that never
/// fills itself in.
pub(crate) fn is_serial(decl: &str) -> bool {
    matches!(
        normalize(decl).as_str(),
        "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial"
    )
}

fn normalize(decl: &str) -> String {
    decl.split('(')
        .next()
        .unwrap_or(decl)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolvable_type_resolves() {
        assert_eq!(column_type("bigint").unwrap(), ColumnType::Int64);
        assert_eq!(column_type("varchar(64)").unwrap(), ColumnType::Text);
        assert_eq!(column_type("numeric(10,2)").unwrap(), ColumnType::Numeric);
        assert_eq!(column_type("date").unwrap(), ColumnType::Date);
        assert_eq!(column_type("time").unwrap(), ColumnType::Time);
        assert_eq!(column_type("timestamptz").unwrap(), ColumnType::Timestamp);
    }

    #[test]
    fn the_three_refusals_are_distinguishable_by_their_text() {
        // A user reading these needs to know which problem they have: an
        // unsupported-but-real type, an array, or a typo.
        let no_home = column_type("interval").unwrap_err().to_string();
        assert!(no_home.contains("`interval`"), "{no_home}");
        assert!(no_home.contains("losing information"), "{no_home}");

        let array = column_type("int4[]").unwrap_err().to_string();
        assert!(array.contains("array types"), "{array}");

        let typo = column_type("intger").unwrap_err().to_string();
        assert!(typo.contains("unknown type"), "{typo}");
    }

    #[test]
    fn serial_is_recognised_in_every_spelling() {
        for s in [
            "serial",
            "SERIAL",
            "bigserial",
            "serial8",
            "smallserial",
            "serial2",
        ] {
            assert!(is_serial(s), "{s}");
        }
        assert!(!is_serial("int4"));
        assert!(!is_serial("text"));
    }
}
