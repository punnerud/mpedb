//! **mpedb as a PostgreSQL server, without a server.**
//!
//! This crate speaks PostgreSQL's v3 wire protocol on a socket it did not open.
//! That sentence is the design: a connection arrives on an inherited file
//! descriptor from inetd or systemd socket activation, one process serves it,
//! and the process exits. Nothing is resident between connections, which is the
//! only shape that keeps mpedb's no-server contract intact while still letting
//! `psql`, `psycopg` and JDBC connect.
//!
//! # Why it is its own workspace
//!
//! Being outside the parent's `members` IS the build toggle. `cargo build` and
//! `cargo test --workspace` in the parent compile not one byte of network code,
//! and cargo's feature unification cannot pull `mpedb-sql/pg-dialect` into the
//! sqlite corpus run by accident — a failure mode the root `Cargo.toml`'s own
//! comments record biting once already.
//!
//! # The three named limits
//!
//! Stated here rather than discovered later:
//!
//! 1. **One writer at a time.** A client holding `BEGIN … COMMIT` open across
//!    round trips would hold mpedb's writer lock for the duration. Rather than
//!    do that, a transaction block is BUFFERED and replayed as one mpedb
//!    transaction at `COMMIT`. The visible difference: a constraint violation
//!    surfaces at COMMIT rather than at the offending statement.
//! 2. **Text format only.** `RowDescription` advertises text for every column
//!    and a binary bind parameter is refused by name. Binary is where a wrong
//!    answer hides best — a misencoded `int8` is eight bytes that decode to a
//!    plausible number with no error anywhere.
//! 3. **The catalog lives in its own database.** A statement naming both a
//!    catalog relation and a user table cannot run. See `catalog.rs` for the
//!    measurement that forced this.

pub mod catalog;
pub mod errmap;
pub mod proto;
pub mod session;
pub mod value;

pub use session::{serve, Options, Session};

/// The `server_version` string mpedb reports.
///
/// It leads with a PostgreSQL version because SQLAlchemy, Django and psycopg
/// PARSE this to decide which features the server has — a string they cannot
/// parse fails the CONNECT, not the first query that needed the feature. It
/// names mpedb in the same breath, so it informs rather than impersonates.
pub fn server_version() -> String {
    format!(
        "{} (mpedb {})",
        mpedb_sql::pg::api::compat_pg_version(),
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
pub(crate) mod testutil {
    use mpedb_types::schema::TableKind;
    use mpedb_types::Schema;
    use mpedb_types::value::{Affinity, Collation};
    use mpedb_types::{ColumnDef, ColumnType, TableDef};

    fn c(name: &str, ty: ColumnType, nullable: bool, unique: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            ty,
            nullable,
            unique,
            indexed: false,
            default: None,
            default_text: None,
            decl: None,
            check: None,
            collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
            generated: None,
        }
    }

    fn table(id: u32, name: &str, columns: Vec<ColumnDef>, pk: Vec<u16>) -> TableDef {
        TableDef {
            id,
            name: name.into(),
            columns,
            primary_key: pk,
            indexes: vec![],
            dead: false, pk_name: None,
            kind: TableKind::Standard,
            implicit_rowid: false,
            autoincrement: false,
            foreign_keys: Vec::new(),
        }
    }

    /// The same two-table shape the SQL crate's catalog tests use — a composite
    /// primary key and a unique column, because those are what naive catalog
    /// code gets wrong.
    pub fn sample_schema() -> Schema {
        Schema::new(vec![
            table(
                0,
                "users",
                vec![
                    c("id", ColumnType::Int64, false, false),
                    c("email", ColumnType::Text, false, true),
                    c("nick", ColumnType::Text, true, false),
                ],
                vec![0],
            ),
            table(
                1,
                "memberships",
                vec![
                    c("user_id", ColumnType::Int64, false, false),
                    c("group_id", ColumnType::Int64, false, false),
                ],
                vec![0, 1],
            ),
        ])
        .expect("sample schema is valid")
    }
}
