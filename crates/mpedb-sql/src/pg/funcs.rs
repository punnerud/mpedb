//! PostgreSQL's function names, resolved by REWRITING rather than by new
//! opcodes.
//!
//! # Why rewriting
//!
//! Every scalar mpedb can evaluate is a [`mpedb_types::expr::ScalarFn`] variant
//! encoded into the compiled plan. Adding variants for PG's spellings would
//! change the plan format — and the whole point of the `pg-dialect` build
//! toggle is that it changes what is COMPILED, never what is STORED. A file
//! written by a PG-capable build has to stay byte-identical to one written
//! without it.
//!
//! So the PG names that are *the same function under a different name* become
//! that name here ([`PgFunc::Alias`]), the ones that differ only in argument
//! order get swapped ([`PgFunc::AliasSwap2`]), and the ones that are constants
//! fold to a literal ([`PgFunc::Const`]). What is left over is refused BY NAME,
//! with the workaround in the message where one exists.
//!
//! # The version string, and why it says PostgreSQL
//!
//! `version()` reports `PostgreSQL 16.14 (mpedb x.y.z) on <target>`. Every
//! PG-compatible engine does this (CockroachDB and YugabyteDB both lead with a
//! `PostgreSQL N` token) for one unavoidable reason: SQLAlchemy, Django and
//! psycopg all PARSE this string to decide which features the server has, and a
//! string that does not start with a PostgreSQL version makes them fail before
//! they can ask anything else. It names mpedb in the same breath, so it informs
//! rather than impersonates — a client that wants to know what it is talking to
//! is told.

use mpedb_types::{Error, Result, Value};

/// The PostgreSQL version mpedb reports compatibility with.
///
/// This is a COMPATIBILITY LEVEL, not a claim to be that build: it is the
/// version whose behaviour the differential suite measures against, so raising
/// it means re-measuring. Kept next to the string that reports it.
pub(crate) const COMPAT_PG_VERSION: &str = "16.14";

/// What a PG function name resolves to.
#[derive(Debug)]
pub(crate) enum PgFunc {
    /// Fold to this constant; the arguments (there are none) are discarded.
    Const(Value),
    /// Fold to this constant REGARDLESS of the arguments, which are discarded.
    ///
    /// Separate from [`PgFunc::Const`] because the arity check differs: these
    /// take arguments and ignore them, and reporting "takes 0 arguments" for
    /// `pg_get_userbyid(10)` would be a confusing lie.
    ConstOfAny(Value),
    /// Evaluate to the first argument unchanged.
    FirstArg,
    /// Bind to a mpedb SCALAR that exists only for the PostgreSQL dialect.
    ///
    /// Separate from [`PgFunc::Alias`], which names a function the sqlite
    /// dialect also has. This one is reachable ONLY through this table, so
    /// `format_type` cannot be called from a sqlite-dialect statement — which
    /// is right, because it names PostgreSQL's type system and nothing else.
    Scalar(mpedb_types::ScalarFn),
    /// `pg_typeof(x)` — the PostgreSQL type NAME of the argument's static type.
    ///
    /// Its own variant because it needs something no rewrite can express: the
    /// BINDER's view of the argument. The type is a compile-time fact here
    /// (mpedb's columns are rigidly typed), so the answer is exact rather than
    /// a guess from the runtime value — which is precisely why the earlier
    /// refusal said `typeof()` could not stand in for it.
    TypeOf,
    /// Fold to `true` regardless of the arguments.
    ///
    /// The `has_*_privilege` family. mpedb has ONE role and the OS owns the
    /// file — DESIGN-MULTIDB's trust box says the filesystem is the boundary —
    /// so a caller that can open the database can do everything in it. `true`
    /// is not a convenient fiction; it is what the permission model actually
    /// says. Answering `false` would deny access the engine grants.
    AlwaysTrue,
    /// The same function under mpedb's existing name, arguments unchanged.
    Alias(&'static str),
    /// The same function with its two arguments the other way round.
    AliasSwap2(&'static str),
    /// Bind to ANOTHER COLUMN of the relation the first argument came from.
    ///
    /// For the `pg_get_*def` family, which renders a catalog object as the DDL
    /// text that would recreate it. A [`PgFunc::Scalar`] cannot do that — it
    /// gets a bare OID and has no schema to look it up in — but the catalog
    /// relation that OID indexes is BUILT from the schema, so the answer can be
    /// computed there and carried as a column. The call
    /// `pg_get_constraintdef(c.oid)` then IS `c.condef`, qualifier and all.
    ///
    /// The first argument must be a column reference; anything else is refused
    /// by name, because there is no relation to take the sibling from.
    SiblingColumn(&'static str),
}

/// Resolve a PostgreSQL function name.
///
/// - `None` — not a PG-specific name; the ordinary (sqlite) table decides,
///   which is what keeps `lower`, `abs`, `coalesce` and the rest on exactly one
///   code path in both dialects.
/// - `Some(Err(..))` — a name that IS PostgreSQL's and that mpedb refuses, with
///   the reason and, where one exists, the rewrite that works.
pub(crate) fn resolve(name: &str, argc: usize) -> Option<Result<PgFunc>> {
    let lower = name.to_ascii_lowercase();
    Some(Ok(match lower.as_str() {
        // ---- constants ----------------------------------------------------
        "version" => PgFunc::Const(Value::Text(format!(
            "PostgreSQL {COMPAT_PG_VERSION} (mpedb {}) on {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
        ))),
        // mpedb has one schema. Reporting `public` is not a fiction dressed up
        // as a feature: it is the name every PG client expects the default
        // schema to have, and search_path handling downstream keys on it.
        "current_schema" => PgFunc::Const(Value::Text("public".into())),
        "current_catalog" | "current_database" => PgFunc::Const(Value::Text("mpedb".into())),
        // `pg_get_userbyid(oid)` names the owner of an object. mpedb has one
        // role because the OS owns the file — DESIGN-MULTIDB's trust box says
        // so in as many words — so the answer is that role, whatever oid is
        // asked about. psql's `\d` calls this for every row.
        "pg_get_userbyid" => PgFunc::ConstOfAny(Value::Text("mpedb".into())),
        // `pg_table_is_visible(oid)` asks whether an object is reachable on the
        // current search_path. mpedb has one namespace and no search_path, so
        // everything that exists is visible. Answering `false` (or refusing)
        // makes `\d` list nothing at all, with no error to explain it.
        "pg_table_is_visible" | "pg_type_is_visible" | "pg_function_is_visible" => {
            PgFunc::ConstOfAny(Value::Bool(true))
        }
        // Used by psql to render a DEFAULT or an index predicate. mpedb stores
        // the DDL TEXT rather than a parse tree, so `pg_attrdef.adbin` already
        // IS what this call would have returned — the identity is the honest
        // implementation.
        // `pg_get_expr(indpred, indrelid)` hands back its FIRST argument, which
        // is right: mpedb stores a partial index's predicate as the very text
        // PostgreSQL would render, so `pg_index.indpred` already IS the answer.
        "pg_get_expr" => PgFunc::FirstArg,
        // `pg_get_constraintdef(oid, …)` renders a constraint as its DDL text.
        // It USED to hand back the OID, on the reasoning that the call is only
        // made over DOMAINS — which was wrong: SQLAlchemy reads FOREIGN KEY,
        // UNIQUE and CHECK reflection out of this text with a regex, so an
        // integer here is `TypeError: expected string or bytes-like object, got
        // 'int'` and every foreign key on the database reflects as none.
        // `pg_constraint.condef` holds the rendered text (pg/catalog/pgcat.rs).
        "pg_get_constraintdef" => PgFunc::SiblingColumn("condef"),
        // `pg_get_indexdef(oid, k, pretty)` renders the k-th KEY EXPRESSION of
        // an index, and its first argument is an OID — so returning that made a
        // `CASE` whose other arm is the column NAME mix `int` with `text` and
        // refuse. NULL instead: honest ("mpedb cannot render it here" — a
        // ScalarFn has no schema to read the expression from), typed by the
        // other arm, and reached ONLY for an expression index part, since
        // SQLAlchemy guards the call with `attnum = 0`.
        "pg_get_indexdef" => PgFunc::ConstOfAny(mpedb_types::Value::Null),
        // `pg_get_serial_sequence(table, column)` names the sequence a column
        // draws from, and NULL when it draws from none. mpedb has no sequence
        // objects at all — `CREATE SEQUENCE` and `nextval()` are both refused
        // by name — so NULL is not a stand-in here, it is the true answer for
        // every column in every mpedb database. Reflection asks it per column
        // and treats NULL as "not serial", which is correct.
        "pg_get_serial_sequence" => PgFunc::ConstOfAny(Value::Null),
        "pg_typeof" => PgFunc::TypeOf,
        // The privilege family: one role, and the OS is the fence.
        "has_table_privilege" | "has_column_privilege" | "has_database_privilege"
        | "has_function_privilege" | "has_schema_privilege" | "has_sequence_privilege"
        | "has_type_privilege" | "has_any_column_privilege" | "has_language_privilege"
        | "has_tablespace_privilege" | "has_server_privilege" | "has_foreign_data_wrapper_privilege"
        | "pg_has_role" => PgFunc::AlwaysTrue,
        // The backend's pid. mpedb-pg is one process per connection, so this is
        // the same number `BackendKeyData` sent at startup.
        "pg_backend_pid" => PgFunc::Const(Value::Int(i64::from(std::process::id()))),

        // ---- same function, different name --------------------------------
        // sqlite's length() counts CHARACTERS for text, which is exactly
        // char_length. octet_length is NOT the same function — it counts bytes,
        // and aliasing it here would return a plausible wrong number for every
        // non-ASCII string. It is refused below instead.
        "char_length" | "character_length" => PgFunc::Alias("length"),
        // PG's strpos(haystack, needle) is sqlite's instr(haystack, needle) —
        // identical, including the 1-based result and the 0-for-absent rule.
        "strpos" => PgFunc::Alias("instr"),
        // PostgreSQL's variadic JSON constructors are sqlite's under another
        // name — same argument order, same key/value pairing, same result.
        // `json_build_object('a',1,'b',2)` IS `json_object('a',1,'b',2)`.
        // Aliasing is the whole implementation; writing a second one would be
        // two things to keep agreeing about NULL and about duplicate keys.
        //
        // Worth more than its size: SQLAlchemy's `get_columns` — the query the
        // whole reflection API is built on — calls `json_build_object` to
        // gather a column's identity options.
        // `format_type(oid, typmod)` renders a type the way reflection expects
        // to read it: `integer`, `character varying(50)`, `numeric(8,4)`. It
        // cannot fold at bind time — its first argument is
        // `pg_attribute.atttypid`, a column — so it is a real scalar.
        "format_type" => PgFunc::Scalar(mpedb_types::ScalarFn::FormatType),
        // The internal spelling the PLANNER lowers `generate_subscripts(a, 1)`
        // to once it has checked the dimension. Not reachable from SQL: the
        // leading underscores are not a legal identifier start for a call a
        // client could write, and the public name stays set-returning.
        "__subscripts" => PgFunc::Scalar(mpedb_types::ScalarFn::Subscripts),
        "json_build_object" => PgFunc::Alias("json_object"),
        "json_build_array" => PgFunc::Alias("json_array"),
        // …but position(needle IN haystack) reads the other way round. The
        // parser lowers the `IN` form to a two-argument call in written order,
        // so the swap belongs here, once, rather than in the grammar.
        "position" => PgFunc::AliasSwap2("instr"),

        // ---- refused, by name, with the way out ---------------------------
        "octet_length" => {
            return Some(Err(unsupported(
                "octet_length() counts BYTES and mpedb's length() counts characters \
                 — they differ for any non-ASCII text, so aliasing them would be a \
                 wrong answer; use length(cast(x as blob)) for the byte count",
            )))
        }
        //
        // A set-returning function is not a scalar: it produces ROWS, so it
        // belongs in FROM and needs planner support mpedb does not have. The
        // recursive-CTE rewrite below is exact, which is why the message
        // carries it rather than just saying no.
        "generate_series" => {
            return Some(Err(unsupported(
                "generate_series() is a set-returning function and mpedb has no \
                 table-function planner — write it as `WITH RECURSIVE s(i) AS \
                 (SELECT <start> UNION ALL SELECT i+<step> FROM s WHERE i < <stop>)`",
            )))
        }
        // `array_agg` is no longer here: it is a real aggregate now
        // (`AggFn::ArrayAgg`), producing a `Value::List`. The rest still need
        // an array they can take APART, which is the half mpedb does not have.
        "unnest" | "array_length" | "array_upper" | "array_lower" => {
            return Some(Err(unsupported(&format!(
                "{lower}() needs an array type, and mpedb has none — there is no \
                 storable array column, so an array cannot be produced or consumed"
            ))))
        }
        "nextval" | "currval" | "setval" | "lastval" => {
            return Some(Err(unsupported(&format!(
                "{lower}() needs sequences, which mpedb does not have — a single-column \
                 INTEGER PRIMARY KEY auto-assigns instead (the rowid-alias rule), and \
                 `RETURNING id` reads the value back"
            ))))
        }
        _ => return None,
    }))
    .map(|r: Result<PgFunc>| r.and_then(|f| check_arity(&lower, &f, argc).map(|()| f)))
}

/// Arity is checked here rather than at the call site so a rewritten name
/// cannot reach the sqlite table with the wrong number of arguments and be
/// reported against THAT function's signature — an error naming `instr` for a
/// mistyped `position()` is a small mystery nobody needs.
fn check_arity(name: &str, f: &PgFunc, argc: usize) -> Result<()> {
    let want: Option<usize> = match f {
        PgFunc::Const(_) => Some(0),
        PgFunc::AliasSwap2(_) => Some(2),
        PgFunc::FirstArg => None,
        PgFunc::TypeOf => Some(1),
        PgFunc::Alias(_) | PgFunc::ConstOfAny(_) | PgFunc::AlwaysTrue => None,
        // `pg_get_constraintdef(oid)` and `pg_get_constraintdef(oid, pretty)`
        // are both spelled by clients; only the first argument is read.
        PgFunc::SiblingColumn(_) => None,
        // Arity is the scalar's own business, checked where every other
        // scalar's is.
        PgFunc::Scalar(_) => None,
    };
    match want {
        Some(n) if argc != n => Err(unsupported(&format!(
            "{name}() takes {n} argument(s), got {argc}"
        ))),
        _ => Ok(()),
    }
}

fn unsupported(msg: &str) -> Error {
    Error::Unsupported(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str, argc: usize) -> PgFunc {
        resolve(name, argc).expect("PG-specific name").unwrap()
    }

    #[test]
    fn a_non_pg_name_defers_to_the_ordinary_table() {
        // The load-bearing case: `lower` must not be resolved twice. If this
        // returned Some, the PG dialect would have its own copy of every scalar
        // and they would drift.
        assert!(resolve("lower", 1).is_none());
        assert!(resolve("abs", 1).is_none());
        assert!(resolve("coalesce", 2).is_none());
        assert!(resolve("json_extract", 2).is_none());
    }

    #[test]
    fn version_leads_with_a_postgresql_version_and_still_names_mpedb() {
        let PgFunc::Const(Value::Text(v)) = ok("version", 0) else {
            panic!("version() should fold to a constant")
        };
        // Clients parse the leading token; if this stops holding, SQLAlchemy
        // and Django fail at connect time, not at the failing query.
        assert!(v.starts_with("PostgreSQL 16."), "{v}");
        assert!(v.contains("mpedb"), "{v}");
    }

    #[test]
    fn position_swaps_its_arguments_and_strpos_does_not() {
        // position(needle IN haystack) vs strpos(haystack, needle) — the same
        // function, written both ways round. Getting this backwards returns a
        // plausible number for every input, so it is exactly the bug that
        // survives casual testing.
        assert!(matches!(ok("strpos", 2), PgFunc::Alias("instr")));
        assert!(matches!(ok("position", 2), PgFunc::AliasSwap2("instr")));
    }

    #[test]
    fn the_catalog_helpers_psql_calls_are_answered_rather_than_refused() {
        // `\d` calls pg_get_userbyid() per row and pg_table_is_visible() in its
        // WHERE. Refusing either makes `\d` fail; answering `false` for
        // visibility makes it silently list nothing, which is worse.
        assert!(matches!(
            ok("pg_get_userbyid", 1),
            PgFunc::ConstOfAny(Value::Text(_))
        ));
        assert!(matches!(
            ok("pg_table_is_visible", 1),
            PgFunc::ConstOfAny(Value::Bool(true))
        ));
        // …and they take their argument without complaining about arity.
        assert!(resolve("pg_get_userbyid", 1).unwrap().is_ok());
    }

    #[test]
    fn refusals_name_the_function_and_carry_the_way_out() {
        let e = resolve("generate_series", 2).unwrap().unwrap_err().to_string();
        assert!(e.contains("WITH RECURSIVE"), "{e}");

        let e = resolve("nextval", 1).unwrap().unwrap_err().to_string();
        assert!(e.contains("RETURNING id"), "{e}");

        let e = resolve("array_length", 1).unwrap().unwrap_err().to_string();
        assert!(e.contains("array_length"), "{e}");
    }

    #[test]
    fn arity_is_reported_against_the_written_name_not_the_rewritten_one() {
        let e = resolve("position", 3).unwrap().unwrap_err().to_string();
        assert!(e.contains("position()"), "{e}");
        assert!(!e.contains("instr"), "{e}");

        let e = resolve("version", 1).unwrap().unwrap_err().to_string();
        assert!(e.contains("version()"), "{e}");
    }
}
