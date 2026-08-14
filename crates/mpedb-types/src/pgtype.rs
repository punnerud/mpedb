//! PostgreSQL's base types: name → OID → [`ColumnType`], in ONE table.
//!
//! Three separate things need this mapping and they must not drift apart:
//!
//! 1. **`mpedb-mirror`** — importing a PG table has to decide what an `int4`
//!    column becomes here (it already did this; the table moved out of
//!    `mirror/src/pg.rs` to become shared rather than copied).
//! 2. **The PG dialect** (`mpedb-sql/src/pg/`) — `CREATE TABLE t (a int4)` and
//!    `x::numeric` both name a PG type and must resolve it the same way the
//!    mirror does, or an imported table and a locally created one would answer
//!    differently.
//! 3. **The wire protocol** (`mpedb-pg`) — `RowDescription` carries an OID per
//!    column, and a client decodes by that OID. Get it wrong and psycopg
//!    silently produces the wrong Python type; there is no error to notice.
//!
//! The OIDs are PostgreSQL's own, from `src/include/catalog/pg_type.dat`. They
//! are ABI, not an implementation detail: every client in existence has them
//! compiled in, so they can never be renumbered.
//!
//! # What mpedb does NOT have
//!
//! mpedb has seven column types. PostgreSQL has dozens. A PG type that has no
//! honest home here maps to `None`, and every caller turns that into a NAMED
//! refusal — never a guess. `numeric` is the one deliberate exception: it is
//! carried as canonical TEXT, which is lossless and is also exactly PG's own
//! wire text format for it.

use crate::value::ColumnType;

/// How faithfully a PG type survives the trip into mpedb's type system.
///
/// This is the mirror's fidelity verdict (DESIGN-MIRROR §2), kept beside the
/// mapping it describes so a type added to one cannot silently miss the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFidelity {
    /// Same width, same semantics, both directions.
    Exact,
    /// mpedb's type is WIDER than PG's (`int4` → `Int64`, `varchar(8)` →
    /// `Text`). The import is lossless; a LOCAL write can now hold a value the
    /// PG column would refuse — which is the class a pre-flight must catch.
    Widened,
    /// Preserved through a canonical text or byte form (`numeric`, `json`,
    /// `jsonb`, `uuid`). Lossless as long as the far side is the same type.
    ViaText,
}

/// One PostgreSQL base type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgType {
    /// PostgreSQL's own `typname` — the spelling `pg_type` stores, which is
    /// what `pg_typeof()` and the catalog views report.
    pub name: &'static str,
    /// `pg_type.oid`. Wire ABI; never renumbered.
    pub oid: u32,
    /// `pg_type.typlen`: byte width, or -1 for a varlena. `RowDescription`
    /// carries it and some clients read it.
    pub typlen: i16,
    /// The mpedb column type this becomes, or `None` when there is no honest
    /// mapping and the caller must refuse by name.
    pub mpedb: Option<ColumnType>,
    /// How much survives the mapping.
    pub fidelity: PgFidelity,
}

const fn t(
    name: &'static str,
    oid: u32,
    typlen: i16,
    mpedb: Option<ColumnType>,
    fidelity: PgFidelity,
) -> PgType {
    PgType {
        name,
        oid,
        typlen,
        mpedb,
        fidelity,
    }
}

use ColumnType as C;
use PgFidelity::{Exact, ViaText, Widened};

/// Every PG base type mpedb knows how to answer for.
///
/// Ordered as `pg_type.dat` orders them (by OID) so a diff against PostgreSQL's
/// own file is readable.
pub const TYPES: &[PgType] = &[
    t("bool", 16, 1, Some(C::Bool), Exact),
    t("bytea", 17, -1, Some(C::Blob), Exact),
    // `char` is PG's internal one-byte type, NOT `character(n)` — that is
    // `bpchar`. Mapping it to Text is a widening, like every other narrow type.
    t("char", 18, 1, Some(C::Text), Widened),
    t("name", 19, 64, Some(C::Text), Widened),
    t("int8", 20, 8, Some(C::Int64), Exact),
    t("int2", 21, 2, Some(C::Int64), Widened),
    t("int4", 23, 4, Some(C::Int64), Widened),
    t("text", 25, -1, Some(C::Text), Exact),
    t("oid", 26, 4, Some(C::Int64), Widened),
    t("json", 114, -1, Some(C::Text), ViaText),
    t("float4", 700, 4, Some(C::Float64), Widened),
    t("float8", 701, 8, Some(C::Float64), Exact),
    t("bpchar", 1042, -1, Some(C::Text), Widened),
    t("varchar", 1043, -1, Some(C::Text), Widened),
    // A DATE is a day, and mpedb's Timestamp is microseconds. Round-tripping a
    // date through it is lossless (midnight UTC); the widening verdict is what
    // says a local write could produce a value PG's `date` cannot hold.
    t("date", 1082, 4, Some(C::Timestamp), Widened),
    // TIME has no date part, so it cannot be a Timestamp without inventing one.
    // Microseconds since midnight is the honest carrier.
    t("time", 1083, 8, Some(C::Int64), Widened),
    t("timestamp", 1114, 8, Some(C::Timestamp), Exact),
    t("timestamptz", 1184, 8, Some(C::Timestamp), Exact),
    // No mapping: an interval is (months, days, micros) and collapsing it to a
    // single number is wrong for every calendar-aware operation.
    t("interval", 1186, 16, None, Exact),
    t("timetz", 1266, 12, None, Exact),
    // Canonical decimal text. Lossless, and identical to PG's own wire text
    // format — which is why this is the one carried type rather than a refusal.
    t("numeric", 1700, -1, Some(C::Text), ViaText),
    t("uuid", 2950, 16, Some(C::Blob), ViaText),
    t("jsonb", 3802, -1, Some(C::Text), ViaText),
];

/// The OID PostgreSQL uses for a value whose type it has not resolved.
///
/// A parameter mpedb could not type gets this in `ParameterDescription`, and PG
/// clients read it as "you decide" rather than as an error.
pub const OID_UNKNOWN: u32 = 705;

/// SQL spellings that are not `typname` but must resolve to the same type.
///
/// The SQL standard's names and PostgreSQL's internal ones differ (`bigint` vs
/// `int8`), and `CREATE TABLE` is written with the former. Multi-word forms are
/// matched after whitespace collapsing, so `double  precision` works.
const ALIASES: &[(&str, &str)] = &[
    ("boolean", "bool"),
    ("smallint", "int2"),
    ("int2", "int2"),
    ("integer", "int4"),
    ("int", "int4"),
    ("bigint", "int8"),
    ("real", "float4"),
    ("double precision", "float8"),
    ("float", "float8"),
    ("decimal", "numeric"),
    ("character varying", "varchar"),
    ("character", "bpchar"),
    ("varchar", "varchar"),
    ("char", "bpchar"),
    ("timestamp without time zone", "timestamp"),
    ("timestamp with time zone", "timestamptz"),
    ("time without time zone", "time"),
    ("time with time zone", "timetz"),
    ("serial", "int4"),
    ("serial4", "int4"),
    ("bigserial", "int8"),
    ("serial8", "int8"),
    ("smallserial", "int2"),
    ("serial2", "int2"),
];

/// The SQL spelling PostgreSQL's `format_type()` prints for a base type.
///
/// The inverse of [`ALIASES`] cannot be derived from it: that table is
/// many-to-one (`integer` and `int` both mean `int4`), so the canonical
/// direction has to be chosen rather than computed. Anything absent here
/// prints its own short name, which is what PostgreSQL does for the types
/// that have no separate SQL spelling (`text`, `bytea`, `uuid`, `json`).
pub fn sql_spelling(short: &str) -> &str {
    match short {
        "bool" => "boolean",
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "varchar" => "character varying",
        "bpchar" => "character",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        "time" => "time without time zone",
        "timetz" => "time with time zone",
        other => other,
    }
}

/// `format_type(oid, typmod)` — the type name a client sees in reflection.
///
/// `typmod` is PostgreSQL's packed modifier, and its encoding is per-type
/// rather than general: for the character types it is `length + 4`, for
/// `numeric` it is `((precision << 16) | scale) + 4`. A `typmod` below 0 (or
/// NULL, which arrives here as `None`) means "no modifier", which is the
/// common case and the reason this is not simply a lookup.
pub fn format_type(oid: u32, typmod: Option<i32>) -> String {
    let Some(t) = by_oid(oid) else {
        // PostgreSQL prints `???` for an oid it cannot resolve. Saying so is
        // better than inventing a plausible type name for a value that came
        // from somewhere unexpected.
        return "???".into();
    };
    let name = sql_spelling(t.name);
    let Some(m) = typmod.filter(|m| *m >= 4) else {
        return name.into();
    };
    match t.name {
        "varchar" | "bpchar" => format!("{name}({})", m - 4),
        "numeric" => {
            let packed = m - 4;
            format!("{name}({},{})", packed >> 16, packed & 0xffff)
        }
        // Every other type either takes no modifier or takes one mpedb never
        // stores; printing the bare name is what PostgreSQL does there too.
        _ => name.into(),
    }
}

/// Strip a type modifier and collapse whitespace: `NUMERIC(10, 2)` →
/// `numeric`, `DOUBLE   PRECISION` → `double precision`.
///
/// The typmod is dropped on purpose. mpedb has no `varchar(8)` — it has `Text`
/// — so keeping the number here would imply an enforcement that does not exist.
/// The mirror records the source spelling separately for exactly that reason.
fn normalize(decl: &str) -> String {
    let head = decl.split('(').next().unwrap_or(decl);
    let mut out = String::with_capacity(head.len());
    let mut sp = false;
    for ch in head.trim().chars() {
        if ch.is_whitespace() {
            sp = true;
            continue;
        }
        if sp && !out.is_empty() {
            out.push(' ');
        }
        sp = false;
        out.extend(ch.to_lowercase());
    }
    out
}

/// Resolve a PG type name — `typname`, a SQL alias, or either with a typmod.
///
/// Returns `None` for a type mpedb has no entry for at all, which every caller
/// turns into a refusal naming the type.
pub fn by_name(decl: &str) -> Option<PgType> {
    let n = normalize(decl);
    // An array type is spelled `_int4` in pg_type and `int4[]` in SQL. mpedb has
    // no storable array (there is deliberately no ColumnType::List), so this is
    // a refusal — but it must be a refusal about ARRAYS, which the caller can
    // only say if we do not silently resolve the element type.
    if n.ends_with("[]") || n.starts_with('_') {
        return None;
    }
    let canon = ALIASES
        .iter()
        .find(|(alias, _)| *alias == n)
        .map(|(_, real)| *real)
        .unwrap_or(n.as_str());
    TYPES.iter().find(|ty| ty.name == canon).copied()
}

/// Look a type up by its OID — the direction the wire protocol reads.
pub fn by_oid(oid: u32) -> Option<PgType> {
    TYPES.iter().find(|ty| ty.oid == oid).copied()
}

/// The OID to report for an mpedb column that carries no declared PG type.
///
/// This is the answer for a database that was never a PostgreSQL one: a client
/// still needs SOME OID per column, and these are the widest honest choices.
/// `Timestamp` reports `timestamptz` because mpedb's is UTC micros by
/// definition, and `Any` reports `text` because that is the only PG type that
/// can receive every value mpedb might put in such a column.
pub fn default_oid(ty: ColumnType) -> u32 {
    match ty {
        C::Int64 => 20,
        C::Float64 => 701,
        C::Bool => 16,
        C::Text => 25,
        C::Blob => 17,
        C::Timestamp => 1184,
        C::Any => 25,
    }
}

#[cfg(test)]
mod format_type_tests {
    use super::*;

    /// `format_type` is what reflection reads a column's type through, so a
    /// wrong answer here is a wrong SCHEMA, not a wrong value.
    #[test]
    fn format_type_prints_what_postgresql_prints() {
        // The SQL spelling, not mpedb's short name — reflection matches on
        // `integer`, never on `int4`.
        assert_eq!(format_type(23, None), "integer");
        assert_eq!(format_type(25, Some(-1)), "text");
        assert_eq!(format_type(1114, Some(-1)), "timestamp without time zone");

        // A NULL/absent modifier means NO modifier, not "no answer". Getting
        // this wrong makes every column without a length report a NULL type,
        // which is most of them.
        assert_eq!(format_type(1043, None), "character varying");

        // typmod encodings, which are per-type rather than general:
        // characters carry `length + 4`…
        assert_eq!(format_type(1043, Some(54)), "character varying(50)");
        // …and numeric packs precision and scale into one int, `+ 4`.
        assert_eq!(format_type(1700, Some((8 << 16 | 4) + 4)), "numeric(8,4)");
        // A type that takes no modifier ignores one rather than inventing
        // syntax for it.
        assert_eq!(format_type(23, Some(8)), "integer");

        // An oid mpedb does not know prints PostgreSQL's own `???`. Naming a
        // plausible type for a value that came from somewhere unexpected is
        // the one answer that could not be checked by the caller.
        assert_eq!(format_type(999_999, None), "???");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oids_are_postgresqls_own_and_unique() {
        // These are ABI. A renumbering here would make every client decode the
        // wrong type with no error anywhere — so pin the ones that matter.
        assert_eq!(by_name("bool").unwrap().oid, 16);
        assert_eq!(by_name("int8").unwrap().oid, 20);
        assert_eq!(by_name("int4").unwrap().oid, 23);
        assert_eq!(by_name("text").unwrap().oid, 25);
        assert_eq!(by_name("float8").unwrap().oid, 701);
        assert_eq!(by_name("timestamptz").unwrap().oid, 1184);
        assert_eq!(by_name("numeric").unwrap().oid, 1700);
        assert_eq!(by_name("uuid").unwrap().oid, 2950);
        assert_eq!(by_name("jsonb").unwrap().oid, 3802);

        let mut seen = std::collections::BTreeSet::new();
        for ty in TYPES {
            assert!(seen.insert(ty.oid), "duplicate OID {}", ty.oid);
        }
    }

    #[test]
    fn sql_spellings_and_typmods_resolve_to_the_same_type() {
        for (spelling, want) in [
            ("BIGINT", "int8"),
            ("integer", "int4"),
            ("Double Precision", "float8"),
            ("double   precision", "float8"),
            ("character varying(255)", "varchar"),
            ("NUMERIC(10,2)", "numeric"),
            ("decimal", "numeric"),
            ("timestamp with time zone", "timestamptz"),
            ("boolean", "bool"),
        ] {
            assert_eq!(
                by_name(spelling).map(|t| t.name),
                Some(want),
                "spelling {spelling}"
            );
        }
    }

    #[test]
    fn serial_is_an_integer_type_here_not_a_sequence() {
        // SERIAL is sugar for "integer + a sequence default". The type half is
        // all this table answers; whether the DEFAULT can be honoured is the
        // DDL binder's decision, and it refuses by name when it cannot.
        assert_eq!(by_name("serial").unwrap().name, "int4");
        assert_eq!(by_name("bigserial").unwrap().name, "int8");
        assert_eq!(by_name("bigserial").unwrap().mpedb, Some(C::Int64));
    }

    #[test]
    fn arrays_do_not_resolve_to_their_element_type() {
        // The failure this prevents: `int4[]` quietly becoming Int64, so a
        // whole array column reads back as one number. There is no
        // ColumnType::List, so the only correct answer is "no mapping".
        assert!(by_name("int4[]").is_none());
        assert!(by_name("_int4").is_none());
        assert!(by_name("text[]").is_none());
    }

    #[test]
    fn types_without_an_honest_home_map_to_none_rather_than_a_guess() {
        assert_eq!(by_name("interval").unwrap().mpedb, None);
        assert_eq!(by_name("timetz").unwrap().mpedb, None);
        // …and a type not in the table at all is also None, but as a lookup
        // miss, so the caller can say "unknown type" rather than "unsupported".
        assert!(by_name("tsvector").is_none());
        assert!(by_name("point").is_none());
    }

    #[test]
    fn numeric_is_carried_as_text_and_says_so() {
        let n = by_name("numeric").unwrap();
        assert_eq!(n.mpedb, Some(C::Text));
        assert_eq!(n.fidelity, ViaText);
    }

    #[test]
    fn every_mpedb_column_type_has_a_default_oid_in_the_table() {
        for ty in [
            C::Int64,
            C::Float64,
            C::Bool,
            C::Text,
            C::Blob,
            C::Timestamp,
            C::Any,
        ] {
            let oid = default_oid(ty);
            assert!(
                by_oid(oid).is_some(),
                "{ty:?} reports OID {oid}, which is not in TYPES"
            );
        }
    }

    #[test]
    fn by_oid_round_trips_every_entry() {
        for ty in TYPES {
            assert_eq!(by_oid(ty.oid).map(|t| t.name), Some(ty.name));
        }
    }
}
