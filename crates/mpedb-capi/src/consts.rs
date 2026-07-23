//! sqlite3 result codes and datatype constants. The integer values are part of
//! sqlite's ABI — consumers `switch` on them — so they are fixed here verbatim
//! (see `/usr/include/sqlite3.h`).
#![allow(dead_code)]

use std::os::raw::c_int;

// Primary result codes.
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_INTERNAL: c_int = 2;
pub const SQLITE_PERM: c_int = 3;
pub const SQLITE_ABORT: c_int = 4;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_LOCKED: c_int = 6;
pub const SQLITE_NOMEM: c_int = 7;
pub const SQLITE_READONLY: c_int = 8;
pub const SQLITE_INTERRUPT: c_int = 9;
pub const SQLITE_IOERR: c_int = 10;
pub const SQLITE_CORRUPT: c_int = 11;
pub const SQLITE_NOTFOUND: c_int = 12;
pub const SQLITE_FULL: c_int = 13;
pub const SQLITE_CANTOPEN: c_int = 14;
pub const SQLITE_PROTOCOL: c_int = 15;
pub const SQLITE_EMPTY: c_int = 16;
pub const SQLITE_SCHEMA: c_int = 17;
pub const SQLITE_TOOBIG: c_int = 18;
pub const SQLITE_CONSTRAINT: c_int = 19;
pub const SQLITE_MISMATCH: c_int = 20;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_NOLFS: c_int = 22;
pub const SQLITE_AUTH: c_int = 23;
pub const SQLITE_FORMAT: c_int = 24;
pub const SQLITE_RANGE: c_int = 25;
pub const SQLITE_NOTADB: c_int = 26;
pub const SQLITE_NOTICE: c_int = 27;
pub const SQLITE_WARNING: c_int = 28;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

// Extended result codes (primary | (sub << 8)) used by consumers that call
// sqlite3_extended_errcode() — Django's backend inspects these.
pub const SQLITE_CONSTRAINT_CHECK: c_int = SQLITE_CONSTRAINT | (1 << 8);
pub const SQLITE_CONSTRAINT_FOREIGNKEY: c_int = SQLITE_CONSTRAINT | (3 << 8);
pub const SQLITE_CONSTRAINT_NOTNULL: c_int = SQLITE_CONSTRAINT | (5 << 8);
pub const SQLITE_CONSTRAINT_PRIMARYKEY: c_int = SQLITE_CONSTRAINT | (6 << 8);
pub const SQLITE_CONSTRAINT_TRIGGER: c_int = SQLITE_CONSTRAINT | (7 << 8);
pub const SQLITE_CONSTRAINT_UNIQUE: c_int = SQLITE_CONSTRAINT | (8 << 8);

// Fundamental datatypes (sqlite3_column_type).
pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

// sqlite3_open_v2 flags (only the ones the shim reacts to).
pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_URI: c_int = 0x0000_0040;
pub const SQLITE_OPEN_MEMORY: c_int = 0x0000_0080;

// Text-destructor sentinels for sqlite3_bind_text/blob.
pub const SQLITE_STATIC: isize = 0;
pub const SQLITE_TRANSIENT: isize = -1;

// Text encodings / function flags for the `eTextRep` argument of
// `sqlite3_create_function[_v2]`. mpedb `Text` is always UTF-8, so the shim
// accepts any of these and ignores the distinction; they exist so a caller can
// pass the constant it would pass to real sqlite (CPython ORs
// `SQLITE_DETERMINISTIC` in). See design/DESIGN-UDF.md §1.
pub const SQLITE_UTF8: c_int = 1;
pub const SQLITE_UTF16LE: c_int = 2;
pub const SQLITE_UTF16BE: c_int = 3;
pub const SQLITE_UTF16: c_int = 4;
pub const SQLITE_ANY: c_int = 5;
pub const SQLITE_UTF16_ALIGNED: c_int = 8;
/// The function returns the same answer for the same inputs within one
/// statement — an optimizer hint in sqlite; accepted and ignored here.
pub const SQLITE_DETERMINISTIC: c_int = 0x0000_0800;
pub const SQLITE_DIRECTONLY: c_int = 0x0008_0000;
pub const SQLITE_INNOCUOUS: c_int = 0x0020_0000;

// Run-time limit categories (`sqlite3_limit`), 0-based and dense.
pub const SQLITE_LIMIT_LENGTH: c_int = 0;
pub const SQLITE_LIMIT_SQL_LENGTH: c_int = 1;
pub const SQLITE_LIMIT_COLUMN: c_int = 2;
pub const SQLITE_LIMIT_EXPR_DEPTH: c_int = 3;
pub const SQLITE_LIMIT_COMPOUND_SELECT: c_int = 4;
pub const SQLITE_LIMIT_VDBE_OP: c_int = 5;
pub const SQLITE_LIMIT_FUNCTION_ARG: c_int = 6;
pub const SQLITE_LIMIT_ATTACHED: c_int = 7;
pub const SQLITE_LIMIT_LIKE_PATTERN_LENGTH: c_int = 8;
pub const SQLITE_LIMIT_VARIABLE_NUMBER: c_int = 9;
pub const SQLITE_LIMIT_TRIGGER_DEPTH: c_int = 10;
pub const SQLITE_LIMIT_WORKER_THREADS: c_int = 11;
/// Number of limit categories (`SQLITE_N_LIMIT` in sqlite's internals).
pub const SQLITE_N_LIMIT: usize = 12;

// `sqlite3_trace_v2` event mask bits. The shim emits only SQLITE_TRACE_STMT.
pub const SQLITE_TRACE_STMT: u32 = 0x01;
pub const SQLITE_TRACE_PROFILE: u32 = 0x02;
pub const SQLITE_TRACE_ROW: u32 = 0x04;
pub const SQLITE_TRACE_CLOSE: u32 = 0x08;
