//! Lightweight SQL-text scanning the shim does itself: splitting a script into
//! statements (mpedb's `query`/`prepare` take exactly one), counting bound
//! parameters for `sqlite3_bind_parameter_count`, and classifying the leading
//! keyword (transaction control is intercepted; DML vs read decides which
//! metadata/side effects apply). None of this parses SQL — it only skips string
//! literals, quoted identifiers and comments so those structures are counted
//! correctly.

/// One quote/comment-aware pass. Calls `f(byte_index, ch)` for every character
/// that is *not* inside a literal/identifier/comment.
fn scan_code(sql: &str, mut f: impl FnMut(usize, u8)) {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                // String literal / quoted identifier: skip to the matching
                // close quote (doubled quote = escaped, stays inside).
                let q = c;
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'[' => {
                // sqlite bracket-quoted identifier: closes at ']'.
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                f(i, c);
                i += 1;
            }
        }
    }
}

/// Split off the first statement at the first top-level `;`. Returns
/// `(statement_including_no_semicolon, tail)` where `tail` is the remaining
/// script (possibly empty). Trailing `;` is dropped from the statement.
pub fn split_first(sql: &str) -> (&str, &str) {
    let mut cut = None;
    scan_code(sql, |i, c| {
        if cut.is_none() && c == b';' {
            cut = Some(i);
        }
    });
    match cut {
        Some(i) => (&sql[..i], &sql[i + 1..]),
        None => (sql, ""),
    }
}

/// True if the statement is empty once comments and whitespace are stripped —
/// sqlite prepares such input to a NULL statement with `SQLITE_OK`.
pub fn is_blank(sql: &str) -> bool {
    let mut any = false;
    scan_code(sql, |_, c| {
        if !c.is_ascii_whitespace() {
            any = true;
        }
    });
    !any
}

/// Count bound parameters: the number of positional `?` plus the largest
/// explicit `?N`/`$N` index (mpedb refuses to mix the styles, so in practice
/// one term is zero). Named `:name`/`@name` parameters are not supported by
/// mpedb and are not counted.
pub fn param_count(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut auto = 0usize;
    let mut max_num = 0usize;
    scan_code(sql, |i, c| {
        if c == b'?' || c == b'$' {
            // Read following digits.
            let mut j = i + 1;
            let mut num = 0usize;
            let mut has_digits = false;
            while j < b.len() && b[j].is_ascii_digit() {
                has_digits = true;
                num = num.saturating_mul(10).saturating_add((b[j] - b'0') as usize);
                j += 1;
            }
            if has_digits {
                max_num = max_num.max(num);
            } else if c == b'?' {
                auto += 1;
            }
        }
    });
    auto.max(max_num)
}

/// `sqlite3_complete`: true if `sql` forms one or more complete statements —
/// i.e. the last non-blank, non-comment code character is a `;`. Empty or
/// comment-only input is not complete.
pub fn is_complete(sql: &str) -> bool {
    let mut last: Option<u8> = None;
    scan_code(sql, |_, c| {
        if !c.is_ascii_whitespace() {
            last = Some(c);
        }
    });
    last == Some(b';')
}

/// The name of each bound parameter in appearance order, for
/// `sqlite3_bind_parameter_name`. A named parameter (`:name`, `@name`,
/// `$name` where `name` starts with a letter or `_`) yields `Some` of its
/// spelling *including* the leading sigil and a trailing NUL; an anonymous or
/// numbered parameter (`?`, `?12`, `$3`) yields `None`. The vector length
/// matches the sequential-parameter count (`?`/named tokens each advance the
/// index; explicit `?N`/`$N` numbers are not gap-filled — mpedb refuses to mix
/// the styles, so in practice a statement is all-`?`, all-`$N`, or all-named).
pub fn param_names(sql: &str) -> Vec<Option<Vec<u8>>> {
    let b = sql.as_bytes();
    let mut out: Vec<Option<Vec<u8>>> = Vec::new();
    scan_code(sql, |i, c| {
        match c {
            b'?' => {
                // `?` or `?NNN` — anonymous/numbered, no name.
                out.push(None);
            }
            b'$' | b':' | b'@' => {
                let mut j = i + 1;
                // `$3` is numbered (no name); `$a`/`:a`/`@a` is named.
                if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                    let start = i; // include the sigil
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    let mut name = b[start..j].to_vec();
                    name.push(0);
                    out.push(Some(name));
                } else if c == b'$' && j < b.len() && b[j].is_ascii_digit() {
                    // `$NNN` — numbered positional, no name.
                    out.push(None);
                }
                // A lone `:`/`@` (e.g. a cast `::`, or punctuation) is not a
                // parameter and contributes nothing.
            }
            _ => {}
        }
    });
    out
}

/// The leading keyword classification the shim acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Begin,
    Commit,
    Rollback,
    RollbackTo,
    Savepoint,
    Release,
    /// INSERT / UPDATE / DELETE (updates `changes()`); `has_returning` says
    /// whether it also produces result rows.
    Dml { has_returning: bool },
    /// SELECT / VALUES / WITH / EXPLAIN — produces rows, side-effect free, so
    /// column metadata may be resolved eagerly.
    Read,
    /// CREATE / DROP / ALTER / REINDEX — schema DDL. mpedb routes these through
    /// `parse_ddl`/`apply_ddl`, NOT the `compile`d-plan path, so the shim must
    /// NOT validate them with `prepare_detached` (which only compiles queries):
    /// it defers them to execution, where `Database::query` applies the DDL.
    Ddl,
    /// Everything else (PRAGMA, and any unrecognized leading word) — hand
    /// straight to the engine, validating at prepare so typos surface there.
    Other,
}

fn first_word(sql: &str) -> String {
    let t = sql.trim_start();
    t.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn second_word(sql: &str) -> String {
    let t = sql.trim_start();
    let rest = &t[first_word(t).len().min(t.len())..];
    let rest = rest.trim_start();
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn has_returning(sql: &str) -> bool {
    let mut found = false;
    let lower: Vec<u8> = sql.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let needle = b"returning";
    // Only outside literals/comments.
    let mut window: Vec<u8> = Vec::new();
    scan_code(sql, |i, _| {
        window.push(lower[i]);
    });
    if window.windows(needle.len()).any(|w| w == needle) {
        // crude word check: ensure it's a standalone token most of the time
        found = true;
    }
    found
}

pub fn classify(sql: &str) -> Kind {
    match first_word(sql).as_str() {
        "begin" => Kind::Begin,
        "commit" | "end" => Kind::Commit,
        "rollback" => {
            if second_word(sql) == "to" {
                Kind::RollbackTo
            } else {
                Kind::Rollback
            }
        }
        "savepoint" => Kind::Savepoint,
        "release" => Kind::Release,
        "insert" | "update" | "delete" | "replace" => Kind::Dml {
            has_returning: has_returning(sql),
        },
        "select" | "values" | "with" | "explain" => Kind::Read,
        // Exactly what mpedb applies via `parse_ddl`/`apply_ddl`. Others that
        // happen to be "DDL-ish" (VACUUM/ANALYZE/REINDEX) are left to `Other`,
        // so they validate-and-refuse at prepare like any unsupported statement.
        "create" | "drop" | "alter" => Kind::Ddl,
        _ => Kind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_top_level_semicolon() {
        let (a, b) = split_first("SELECT 1; SELECT 2");
        assert_eq!(a, "SELECT 1");
        assert_eq!(b.trim(), "SELECT 2");
        // Semicolon inside a string is not a split point.
        let (a, b) = split_first("SELECT ';'; SELECT 2");
        assert_eq!(a, "SELECT ';'");
        assert_eq!(b.trim(), "SELECT 2");
    }

    #[test]
    fn blank_detects_comments_only() {
        assert!(is_blank("  -- hi\n /* x */  "));
        assert!(is_blank(""));
        assert!(!is_blank("SELECT 1"));
    }

    #[test]
    fn counts_params() {
        assert_eq!(param_count("SELECT ?, ?, ?"), 3);
        assert_eq!(param_count("SELECT $1, $2, $2"), 2);
        assert_eq!(param_count("SELECT '?', ?"), 1);
        assert_eq!(param_count("SELECT 1"), 0);
    }

    #[test]
    fn classifies() {
        assert_eq!(classify("  begin transaction"), Kind::Begin);
        assert_eq!(classify("END"), Kind::Commit);
        assert_eq!(classify("ROLLBACK TO sp"), Kind::RollbackTo);
        assert_eq!(classify("rollback"), Kind::Rollback);
        assert_eq!(classify("SELECT 1"), Kind::Read);
        assert!(matches!(classify("INSERT INTO t VALUES (1) RETURNING id"), Kind::Dml { has_returning: true }));
        assert!(matches!(classify("delete from t"), Kind::Dml { has_returning: false }));
        assert_eq!(classify("CREATE TABLE t (id int)"), Kind::Ddl);
        assert_eq!(classify("DROP TABLE t"), Kind::Ddl);
        assert_eq!(classify("alter table t add column c int"), Kind::Ddl);
        assert_eq!(classify("PRAGMA foreign_keys=ON"), Kind::Other);
        assert_eq!(classify("SELCT typo"), Kind::Other);
    }
}
