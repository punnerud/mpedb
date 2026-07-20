//! `sqlite3_set_authorizer` — the compile-time access gate.
//!
//! sqlite consults the registered callback once per *action* while a statement
//! is being prepared, with an action code and up to two object names, and
//! `SQLITE_DENY` fails the prepare. mpedb has the same information at the same
//! moment, in a better form: the compiled plan's footprint plus its column
//! references (`mpedb::Database::access_report`), derived by the SAME compile
//! the statement will run through — so an authorized statement and the executed
//! statement can never be two different plans.
//!
//! # What is enforced, and what refuses
//!
//! * `SQLITE_OK` and `SQLITE_DENY` are honoured exactly, with sqlite's own two
//!   messages ("access to *t.c* is prohibited" for a denied column read,
//!   "not authorized" for everything else) and `SQLITE_AUTH`.
//! * `SQLITE_IGNORE` is **refused by name**. On a column read it means
//!   "substitute NULL for this column"; mpedb has no plan rewrite for that, and
//!   returning the real value instead would hand out exactly the data the
//!   callback asked to hide. It fails the prepare with a message that says so —
//!   fail-closed, never a silent leak.
//! * Any other return value is sqlite's "authorizer malfunction".
//! * A statement the shim cannot describe in action codes (`ATTACH`/`DETACH`,
//!   and anything that fails to compile) is refused while an authorizer is
//!   registered, rather than being let through unexamined.
//!
//! Column attribution inherits `access_report`'s one approximation: exact for
//! single-table statements, widened to "every column of every table read" for
//! joins/compounds/subqueries. It over-reports, never under-reports — a gate
//! can refuse more than sqlite would, never less (see `mpedb::access`).

use crate::consts::*;
use crate::{introspect, sql, Sqlite3};
use mpedb::{Access, ObjectKind, TxnOp};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

// sqlite's authorizer action codes (sqlite3.h). Only the ones the shim can
// actually raise are named here.
pub const SQLITE_CREATE_INDEX: c_int = 1;
pub const SQLITE_CREATE_TABLE: c_int = 2;
pub const SQLITE_CREATE_TRIGGER: c_int = 7;
pub const SQLITE_CREATE_VIEW: c_int = 8;
pub const SQLITE_DELETE: c_int = 9;
pub const SQLITE_DROP_INDEX: c_int = 10;
pub const SQLITE_DROP_TABLE: c_int = 11;
pub const SQLITE_DROP_TRIGGER: c_int = 16;
pub const SQLITE_DROP_VIEW: c_int = 17;
pub const SQLITE_INSERT: c_int = 18;
pub const SQLITE_PRAGMA: c_int = 19;
pub const SQLITE_READ: c_int = 20;
pub const SQLITE_SELECT: c_int = 21;
pub const SQLITE_TRANSACTION: c_int = 22;
pub const SQLITE_UPDATE: c_int = 23;
pub const SQLITE_ALTER_TABLE: c_int = 26;
pub const SQLITE_DROP_VTABLE: c_int = 30;
pub const SQLITE_CREATE_VTABLE: c_int = 29;
pub const SQLITE_SAVEPOINT: c_int = 32;

/// Authorizer return codes.
pub const SQLITE_DENY: c_int = 1;
pub const SQLITE_IGNORE: c_int = 2;

type AuthFn = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
) -> c_int;

/// One authorizer consultation: the action code plus sqlite's two object-name
/// arguments.
struct Call {
    action: c_int,
    arg1: Option<String>,
    arg2: Option<String>,
}

/// The outcome of the whole authorization pass: `Ok(())` or the exact
/// `(code, message)` the prepare must fail with.
pub type AuthResult = Result<(), (c_int, String)>;

/// Run every action this statement performs past the connection's authorizer.
/// A no-op (and no compile) when none is registered.
pub unsafe fn authorize(c: &mut Sqlite3, stmt_sql: &str) -> AuthResult {
    if c.auth_cb.is_null() {
        return Ok(());
    }
    let calls = describe(c, stmt_sql)?;
    let cb: AuthFn = std::mem::transmute(c.auth_cb);
    for call in calls {
        let a1 = call.arg1.as_deref().map(cstr);
        let a2 = call.arg2.as_deref().map(cstr);
        let db_name = cstr("main");
        let p1 = a1.as_ref().map_or(ptr::null(), |s| s.as_ptr());
        let p2 = a2.as_ref().map_or(ptr::null(), |s| s.as_ptr());
        // The 6th argument names the trigger/view a nested action came from.
        // mpedb flattens views and triggers into the plan, so every action the
        // shim raises is a TOP-LEVEL one: NULL, as sqlite passes for those.
        let rc = cb(c.auth_ctx, call.action, p1, p2, db_name.as_ptr(), ptr::null());
        match rc {
            SQLITE_OK => {}
            SQLITE_DENY => {
                return Err(match (call.action, &call.arg1, &call.arg2) {
                    // sqlite's column-read message names the object; every
                    // other denial is the generic one.
                    (SQLITE_READ, Some(t), Some(col)) => {
                        (SQLITE_AUTH, format!("access to {t}.{col} is prohibited"))
                    }
                    _ => (SQLITE_AUTH, "not authorized".to_string()),
                });
            }
            SQLITE_IGNORE => {
                return Err((
                    SQLITE_ERROR,
                    "authorizer returned SQLITE_IGNORE, which mpedb cannot honour: it \
                     means \"read this column as NULL\", and mpedb has no plan rewrite \
                     that substitutes a NULL for a column reference. Returning the real \
                     value instead would hand out exactly what was asked to be hidden, \
                     so the statement is refused. Use SQLITE_DENY or SQLITE_OK"
                        .to_string(),
                ));
            }
            _ => return Err((SQLITE_ERROR, "authorizer malfunction".to_string())),
        }
    }
    Ok(())
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// The action list for one statement, in the order sqlite raises them:
/// the statement's own action first, then the object touches.
unsafe fn describe(c: &mut Sqlite3, stmt_sql: &str) -> Result<Vec<Call>, (c_int, String)> {
    use sql::Kind;
    let text = sql::strip_leading_trivia(stmt_sql);
    let one = |action, arg1: Option<&str>, arg2: Option<&str>| Call {
        action,
        arg1: arg1.map(str::to_string),
        arg2: arg2.map(str::to_string),
    };
    match sql::classify(text) {
        // `PRAGMA name[=value]` — sqlite passes the pragma name and its
        // argument, and the shim already parses exactly that pair.
        Kind::Pragma => {
            let (name, arg) = introspect::parse_pragma(text);
            return Ok(vec![one(SQLITE_PRAGMA, Some(&name), arg.as_deref())]);
        }
        // Transaction control never reaches the engine's compiler here (the
        // shim intercepts it), so classify it directly.
        Kind::Begin => return Ok(vec![one(SQLITE_TRANSACTION, Some("BEGIN"), None)]),
        Kind::Commit => return Ok(vec![one(SQLITE_TRANSACTION, Some("COMMIT"), None)]),
        Kind::Rollback => return Ok(vec![one(SQLITE_TRANSACTION, Some("ROLLBACK"), None)]),
        Kind::Savepoint => {
            return Ok(vec![one(SQLITE_SAVEPOINT, Some("BEGIN"), Some(&savepoint_name(text)))])
        }
        Kind::Release => {
            return Ok(vec![one(SQLITE_SAVEPOINT, Some("RELEASE"), Some(&savepoint_name(text)))])
        }
        Kind::RollbackTo => {
            return Ok(vec![one(SQLITE_SAVEPOINT, Some("ROLLBACK"), Some(&savepoint_name(text)))])
        }
        // VACUUM / ANALYZE are accepted no-ops: nothing is touched, so there
        // is nothing to authorize.
        Kind::Maintenance => return Ok(Vec::new()),
        // A `sqlite_master` query is answered by the shim, not compiled — it
        // reads sqlite's own catalog table.
        Kind::Read if introspect::references_sqlite_master(text) => {
            let mut calls = vec![one(SQLITE_SELECT, None, None)];
            for col in ["type", "name", "tbl_name", "rootpage", "sql"] {
                calls.push(one(SQLITE_READ, Some("sqlite_master"), Some(col)));
            }
            return Ok(calls);
        }
        _ => {}
    }

    // Everything else is described by the plan the statement will actually
    // compile to — through the SESSION's schema view when a transaction is
    // open, so DDL applied earlier in it is visible (#95).
    let report = match &c.txn {
        Some(s) => s.access_report(text),
        None => c.db.access_report(text),
    };
    let report = report.map_err(|e| {
        (
            SQLITE_AUTH,
            format!(
                "not authorized: this statement cannot be described in authorizer \
                 actions, so it is refused while an authorizer is registered ({e})"
            ),
        )
    })?;

    let mut calls = Vec::new();
    for a in &report.actions {
        calls.push(match a {
            Access::Select => one(SQLITE_SELECT, None, None),
            Access::Read { table, column } => one(SQLITE_READ, Some(table), Some(column)),
            Access::Insert { table } => one(SQLITE_INSERT, Some(table), None),
            Access::Update { table, column } => one(SQLITE_UPDATE, Some(table), Some(column)),
            Access::Delete { table } => one(SQLITE_DELETE, Some(table), None),
            Access::Alter { table } => one(SQLITE_ALTER_TABLE, Some("main"), Some(table)),
            Access::Transaction { op } => one(SQLITE_TRANSACTION, Some(txn_op(*op)), None),
            Access::Savepoint { op, name } => {
                one(SQLITE_SAVEPOINT, Some(txn_op(*op)), Some(name))
            }
            Access::Create { kind, name, table } => match object_codes(*kind) {
                Some((create, _)) => one(create, Some(name), table.as_deref()),
                // An RLS policy is an mpedb concept with no sqlite action
                // code. Refusing beats inventing one or waving it through.
                None => return Err((SQLITE_AUTH, unmappable("CREATE POLICY"))),
            },
            Access::Drop { kind, name, table } => match object_codes(*kind) {
                Some((_, drop)) => one(drop, Some(name), table.as_deref()),
                None => return Err((SQLITE_AUTH, unmappable("DROP POLICY"))),
            },
        });
    }
    Ok(calls)
}

fn unmappable(what: &str) -> String {
    format!(
        "not authorized: {what} has no sqlite authorizer action code, so it cannot be \
         shown to the authorizer and is refused while one is registered"
    )
}

fn object_codes(k: ObjectKind) -> Option<(c_int, c_int)> {
    Some(match k {
        ObjectKind::Table => (SQLITE_CREATE_TABLE, SQLITE_DROP_TABLE),
        ObjectKind::VirtualTable => (SQLITE_CREATE_VTABLE, SQLITE_DROP_VTABLE),
        ObjectKind::Index => (SQLITE_CREATE_INDEX, SQLITE_DROP_INDEX),
        ObjectKind::View => (SQLITE_CREATE_VIEW, SQLITE_DROP_VIEW),
        ObjectKind::Trigger => (SQLITE_CREATE_TRIGGER, SQLITE_DROP_TRIGGER),
        ObjectKind::Policy => return None,
    })
}

fn txn_op(op: TxnOp) -> &'static str {
    match op {
        TxnOp::Begin => "BEGIN",
        TxnOp::Commit => "COMMIT",
        TxnOp::Rollback => "ROLLBACK",
        TxnOp::Release => "RELEASE",
    }
}

/// The savepoint name from `SAVEPOINT x` / `RELEASE [SAVEPOINT] x` /
/// `ROLLBACK [TRANSACTION] TO [SAVEPOINT] x` — the last word of the statement,
/// which is what all three shapes have in common.
fn savepoint_name(sql: &str) -> String {
    sql.split_whitespace()
        .next_back()
        .unwrap_or("")
        .trim_matches(|ch| ch == '"' || ch == '`' || ch == '\'' || ch == ';')
        .to_string()
}
