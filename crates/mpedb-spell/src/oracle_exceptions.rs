//! Public predefined names and English messages measured with Oracle 23.26.3.
//! This is an interoperability table, not a complete/localized error catalog.
//! Names/codes: https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/predefined-exceptions.html

pub(crate) const PREDEFINED: &[(&str, u32, &str)] = &[
    (
        "ACCESS_INTO_NULL",
        6530,
        "Reference to uninitialized composite",
    ),
    (
        "CASE_NOT_FOUND",
        6592,
        "CASE not found while executing CASE statement",
    ),
    (
        "COLLECTION_IS_NULL",
        6531,
        "Reference to uninitialized collection",
    ),
    ("CURSOR_ALREADY_OPEN", 6511, "PL/SQL: cursor already open"),
    (
        "DUP_VAL_ON_INDEX",
        1,
        "unique constraint (.) violated on table . columns ()",
    ),
    (
        "INVALID_CURSOR",
        1001,
        "cursor number is invalid or does not exist",
    ),
    (
        "INVALID_NUMBER",
        1722,
        "unable to convert string value containing  to a number: ",
    ),
    (
        "LOGIN_DENIED",
        1017,
        "invalid credential or not authorized; logon denied",
    ),
    ("NO_DATA_FOUND", 1403, "no data found"),
    ("NO_DATA_NEEDED", 6548, "no more rows needed"),
    ("NOT_LOGGED_ON", 1012, "not logged on"),
    ("PROGRAM_ERROR", 6501, "PL/SQL: program error"),
    (
        "ROWTYPE_MISMATCH",
        6504,
        "PL/SQL: Return types of Result Set variables or query do not match",
    ),
    (
        "SELF_IS_NULL",
        30625,
        "method dispatch on NULL SELF argument is disallowed",
    ),
    ("STORAGE_ERROR", 6500, "PL/SQL: storage error"),
    ("SUBSCRIPT_BEYOND_COUNT", 6533, "Subscript beyond count"),
    (
        "SUBSCRIPT_OUTSIDE_LIMIT",
        6532,
        "subscript outside of limit",
    ),
    ("SYS_INVALID_ROWID", 1410, "The ROWID is invalid."),
    (
        "TIMEOUT_ON_RESOURCE",
        51,
        "timeout occurred while waiting for a resource",
    ),
    (
        "TOO_MANY_ROWS",
        1422,
        "exact fetch returned more than the requested number of rows ",
    ),
    ("VALUE_ERROR", 6502, "PL/SQL: value or conversion error"),
    ("ZERO_DIVIDE", 1476, "divisor is equal to zero"),
];

pub(crate) fn code(name: &str) -> Option<u32> {
    PREDEFINED
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, c, _)| *c)
}

pub(crate) fn message(code: u32) -> Option<&'static str> {
    match code {
        2 => Some("Message 2 not found;  product=RDBMS; facility=ORA"),
        1400 => Some("cannot insert NULL into ()"),
        1426 => Some("numeric overflow"),
        2290 => Some("check constraint (.) violated"),
        65535 => Some("Oracle Database client does not support error number exceeding 65535."),
        20000..=20999 => Some(""),
        _ => PREDEFINED
            .iter()
            .find(|(_, c, _)| *c == code)
            .map(|(_, _, m)| *m),
    }
}
