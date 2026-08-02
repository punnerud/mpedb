//! Quote/comment-aware SQL-script splitting — the ONE home for the logic both
//! drop-in surfaces need (`mpedb-py`'s `executescript` today; `mpedb-capi`
//! carries an older private copy in its own workspace, queued to migrate
//! here). None of this parses SQL — it only skips string literals, quoted
//! identifiers and comments so `;` and keywords are seen where sqlite's
//! parser would see them.

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
/// `(statement_without_the_semicolon, tail)` where `tail` is the remaining
/// script (possibly empty).
///
/// `CREATE TRIGGER … BEGIN <body> END` contains `;` INSIDE it — one per body
/// statement — so a trigger statement is carved out whole first.
pub fn split_first(sql: &str) -> (&str, &str) {
    if let Some(end) = create_trigger_end(sql) {
        let tail = sql[end..].trim_start();
        let tail = tail.strip_prefix(';').unwrap_or(tail);
        return (&sql[..end], tail);
    }
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

/// The alphanumeric words of `sql` OUTSIDE literals, quoted identifiers and
/// comments, as `(start, end)` byte ranges.
fn code_words(sql: &str) -> Vec<(usize, usize)> {
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut prev = usize::MAX;
    scan_code(sql, |i, c| {
        let is_word = c.is_ascii_alphanumeric() || c == b'_';
        match words.last_mut() {
            Some(last) if is_word && prev != usize::MAX && i == prev + 1 && last.1 == i => {
                last.1 = i + 1
            }
            _ if is_word => words.push((i, i + 1)),
            _ => {}
        }
        prev = i;
    });
    words
}

/// For a `CREATE [TEMP] TRIGGER … BEGIN <body> END`, the byte index just past
/// the `END` that closes the body; `None` for any other statement.
///
/// `END` also closes a `CASE` expression, so the scan tracks depth: `CASE`
/// opens, `END` closes, and the `END` that brings the depth back to zero is
/// the trigger's. This is exactly the rule sqlite's own parser applies.
fn create_trigger_end(sql: &str) -> Option<usize> {
    let words = code_words(sql);
    let w = |n: usize| words.get(n).map(|&(a, b)| &sql[a..b]);
    if !w(0)?.eq_ignore_ascii_case("create") {
        return None;
    }
    let head = if w(1)?.eq_ignore_ascii_case("trigger") {
        1
    } else if w(1)?.eq_ignore_ascii_case("temp") || w(1)?.eq_ignore_ascii_case("temporary") {
        if !w(2)?.eq_ignore_ascii_case("trigger") {
            return None;
        }
        2
    } else {
        return None;
    };
    let begin = (head + 1..words.len()).find(|&i| w(i).unwrap().eq_ignore_ascii_case("begin"))?;
    let mut depth = 1usize;
    for &(a, b) in &words[begin + 1..] {
        let word = &sql[a..b];
        if word.eq_ignore_ascii_case("case") {
            depth += 1;
        } else if word.eq_ignore_ascii_case("end") {
            depth -= 1;
            if depth == 0 {
                return Some(b);
            }
        }
    }
    // Unterminated body: leave it whole, so the parser reports the real error
    // instead of the splitter inventing a truncation.
    Some(sql.len())
}

/// True if the input is empty once comments and whitespace are stripped —
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trigger_body_splits_as_one_statement() {
        let script = "CREATE TABLE t (a INT); \
                      CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
                        UPDATE t SET a = CASE WHEN new.a > 0 THEN 1 ELSE 0 END; \
                        DELETE FROM t WHERE a = 9; \
                      END; \
                      INSERT INTO t VALUES (1)";
        let (s1, rest) = split_first(script);
        assert_eq!(s1.trim(), "CREATE TABLE t (a INT)");
        let (s2, rest) = split_first(rest);
        assert!(s2.trim_start().to_ascii_uppercase().starts_with("CREATE TRIGGER"));
        assert!(s2.trim_end().to_ascii_uppercase().ends_with("END"));
        let (s3, rest) = split_first(rest);
        assert_eq!(s3.trim(), "INSERT INTO t VALUES (1)");
        assert!(is_blank(rest));
    }

    #[test]
    fn semicolons_inside_literals_and_comments_do_not_split() {
        let script = "INSERT INTO t VALUES (';'); -- tail; comment\nSELECT 1";
        let (s1, rest) = split_first(script);
        assert_eq!(s1.trim(), "INSERT INTO t VALUES (';')");
        let (s2, rest) = split_first(rest);
        assert!(s2.contains("SELECT 1"));
        assert!(is_blank(rest));
    }
}
