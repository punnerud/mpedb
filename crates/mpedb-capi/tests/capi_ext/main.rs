//! mpedb-capi FFI tests (capi_ext): drive the exported sqlite3 C-API exactly as a C caller
//! would (raw pointers, 1-based binds, 0-based columns) and assert both the
//! result-code integers and the returned values.

#![allow(dead_code)] // each of the three capi test binaries uses the subset of
// these shared FFI helpers its own cases need (split out of capi.rs in 62c8f20).

use mpedb_sqlite3::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

#[path = "../common/mod.rs"]
mod common;

mod helpers;
mod api_basics;
mod locking;
mod blob_io;
mod stmt_semantics;
mod introspection;
mod index_catalog;
