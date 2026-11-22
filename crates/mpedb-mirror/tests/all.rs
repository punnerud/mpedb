//! Every integration test of this crate, as one binary. Each file used to be
//! its own, and each binary links the engine and a bundled SQLite. A file
//! here is a module: `cargo test -p mpedb-mirror --test all <file>::`.
//! `autotests` is off in Cargo.toml, so a new test file needs a line here.

// Shared by many of the files below. Each used to compile its own copy.
#[cfg(feature = "oracle-source")]
mod oracle_support;

mod concurrent_load;
mod echo_loop;
mod oracle_adapter;
mod oracle_adapter_push;
mod oracle_binary_float;
mod oracle_capture;
mod oracle_checked_write;
mod oracle_control_fence;
mod oracle_exception_init;
mod oracle_final_postimages;
mod oracle_import;
mod oracle_mirror_decision;
mod oracle_not_null;
mod oracle_number_constraints;
mod oracle_origin;
mod oracle_origin_cursor;
mod oracle_plsql;
mod oracle_predefined_exceptions;
mod oracle_receipt;
mod oracle_source;
mod oracle_state;
mod oracle_static_sql;
mod oracle_stream_import;
mod oracle_temporal_write;
mod oracle_user_exceptions;
mod push_outbox;
mod push_postimages;
mod sqlite_specials;
