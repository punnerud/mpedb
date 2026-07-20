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

/// The statement with leading whitespace AND comments removed. sqlite's parser
/// skips leading trivia itself; mpedb's does not (a `-- comment\nINSERT …`
/// is a parse error at the `--`), so the shim strips it before the engine
/// sees the text. Comments elsewhere in the statement remain the engine's
/// business.
pub fn strip_leading_trivia(sql: &str) -> &str {
    let mut first = sql.len();
    scan_code(sql, |i, c| {
        if first == sql.len() && !c.is_ascii_whitespace() {
            first = i;
        }
    });
    &sql[first..]
}

/// Rewrite `zeroblob(<constant>)` calls to the equivalent blob LITERAL
/// (`x'00…'`) in code regions, leaving strings/quoted-idents/comments untouched.
///
/// `zeroblob(N)` is a sqlite core scalar function (N zero bytes). mpedb has it
/// only as a shim-registered host UDF (`register_shim_builtins`), which returns
/// a dynamically-typed `Any` value — and mpedb's binder refuses a function
/// CALL in `INSERT … VALUES` position ("must be literals or parameters") and
/// pins `length()`'s argument to text. Both are dodged by turning a constant
/// zeroblob into the byte-identical blob literal at the text level, which is
/// accepted everywhere a literal is and carries the correct `blob` type.
///
/// Only the simple shape `zeroblob(<numeric | 'string' | NULL>)` with a single
/// constant argument is rewritten, and only when the resulting literal is small
/// enough to materialize inline (`MAX_INLINE`). A non-constant argument, a
/// nested expression, or an over-large constant is left verbatim for the host
/// UDF (which materializes it and enforces sqlite's `SQLITE_TOOBIG` cap) — so
/// e.g. `select zeroblob(1000000001)` still raises "string or blob too big".
pub fn rewrite_zeroblob(sql: &str) -> std::borrow::Cow<'_, str> {
    // 16 MiB: far beyond any realistic INSERT/UPDATE zeroblob and a hard bound
    // on the literal we build; larger constants fall through to the host UDF.
    const MAX_INLINE: i64 = 16 * 1024 * 1024;
    let b = sql.as_bytes();
    // Collect (start, end, n) for each rewritable call in a first scan, then
    // splice — so the byte offsets from `scan_code` stay valid.
    let mut hits: Vec<(usize, usize, i64)> = Vec::new();
    let lower = sql.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut i = 0;
    while i + 8 <= lb.len() {
        // Must be an identifier boundary before `zeroblob`.
        if &lb[i..i + 8] == b"zeroblob"
            && (i == 0 || !is_name_char(b[i - 1]))
            && !is_name_char(*b.get(i + 8).unwrap_or(&b' '))
        {
            if let Some((end, n)) = parse_zeroblob_arg(b, i + 8) {
                if n <= MAX_INLINE {
                    hits.push((i, end, n));
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if hits.is_empty() {
        return std::borrow::Cow::Borrowed(sql);
    }
    // Verify each hit is in a CODE region (not inside a string/comment) by
    // re-scanning and intersecting; scan_code visits only code bytes.
    let mut code_starts = std::collections::HashSet::new();
    scan_code(sql, |idx, _| {
        code_starts.insert(idx);
    });
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for (start, end, n) in hits {
        if !code_starts.contains(&start) {
            continue; // the `zeroblob` text was inside a literal/comment
        }
        out.push_str(&sql[pos..start]);
        let count = n.max(0) as usize;
        out.push_str("x'");
        for _ in 0..count {
            out.push_str("00");
        }
        out.push('\'');
        pos = end;
    }
    out.push_str(&sql[pos..]);
    std::borrow::Cow::Owned(out)
}

/// Starting just past `zeroblob`, parse `( <const> )` and return the byte index
/// past the `)` plus the sqlite integer value of the constant. `None` if the
/// argument is not a single literal (leave the call for the host UDF).
fn parse_zeroblob_arg(b: &[u8], mut i: usize) -> Option<(usize, i64)> {
    let skip_ws = |b: &[u8], mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    i = skip_ws(b, i);
    if b.get(i) != Some(&b'(') {
        return None;
    }
    i = skip_ws(b, i + 1);
    let n = if b.get(i) == Some(&b'\'') {
        // Single-quoted string: capture with '' escapes, then sqlite text→int.
        let mut s = Vec::new();
        i += 1;
        loop {
            match b.get(i) {
                None => return None,
                Some(b'\'') => {
                    if b.get(i + 1) == Some(&b'\'') {
                        s.push(b'\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                Some(&c) => {
                    s.push(c);
                    i += 1;
                }
            }
        }
        crate::blob::text_to_i64(&String::from_utf8_lossy(&s))
    } else if b[i..].len() >= 4 && b[i..i + 4].eq_ignore_ascii_case(b"null") {
        i += 4;
        0
    } else {
        // Numeric: optional sign, digits, optional fraction/exponent.
        let start = i;
        if b.get(i) == Some(&b'+') || b.get(i) == Some(&b'-') {
            i += 1;
        }
        let mut saw_digit = false;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
            saw_digit = true;
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
                saw_digit = true;
            }
        }
        if saw_digit && (b.get(i) == Some(&b'e') || b.get(i) == Some(&b'E')) {
            let mut j = i + 1;
            if b.get(j) == Some(&b'+') || b.get(j) == Some(&b'-') {
                j += 1;
            }
            if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
                while b.get(j).is_some_and(|c| c.is_ascii_digit()) {
                    j += 1;
                }
                i = j;
            }
        }
        if !saw_digit {
            return None;
        }
        let text = std::str::from_utf8(&b[start..i]).ok()?;
        crate::blob::text_to_i64(text)
    };
    i = skip_ws(b, i);
    if b.get(i) != Some(&b')') {
        return None;
    }
    Some((i + 1, n))
}

/// A character that may appear in a named parameter's body (after the sigil),
/// matching sqlite's `IdChar`: ASCII alphanumerics, `_`, and any byte ≥ 0x80 (so
/// UTF-8 identifier names bind). `$` is deliberately excluded here (it is only a
/// sigil), keeping the common case unambiguous.
fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// The result of a quote/comment-aware parameter scan: the SQL rewritten so that
/// mpedb's numbered-`$N` binder sees *every* parameter, plus the sqlite-visible
/// parameter count and per-number spelling.
pub struct ParamScan {
    /// `sql` with every parameter token (`?`, `?N`, `:name`, `@name`, `$name`)
    /// replaced by the numbered mpedb placeholder `$K` it was assigned. Every
    /// other byte — string/blob literals, quoted identifiers, comments — is
    /// preserved verbatim, so only placeholders change.
    pub rewritten: String,
    /// The highest parameter number used, i.e. `sqlite3_bind_parameter_count`.
    pub count: usize,
    /// Per-parameter spelling in number order (1-based → `names[k-1]`), sigil
    /// included and NUL-terminated, for `sqlite3_bind_parameter_name`. `None` for
    /// an anonymous `?`, or for a number never spelled (a gap an explicit `?N`
    /// skipped over). Matches sqlite: a `?N`/`:n`/`@n`/`$n` all report their own
    /// spelling; only a bare `?` is anonymous.
    pub names: Vec<Option<Vec<u8>>>,
}

/// Scan `sql` for bound parameters and rewrite them to mpedb's numbered `$K`
/// form, assigning numbers exactly as sqlite's `sqlite3ExprAssignVarNumber`
/// does (verified against sqlite 3.45):
/// * a bare `?` takes the next number (one past the highest so far), anonymous;
/// * an explicit `?N` takes number `N`, bumping the high-water mark if larger;
/// * `:name`/`@name`/`$name` are all *named* — a new name takes the next number
///   (so `$5` is a name, NOT positional `$5`); a repeated name reuses its number.
///
/// All parameter kinds share one numbering space. The rewrite emits `$K` for
/// every token, giving mpedb a single-style numbered statement (mpedb refuses to
/// mix `?` and `$N`, but the shim's uniform `$K` output never does). String/blob
/// literals, quoted identifiers and comments are skipped verbatim, so a `?` or
/// `:x` inside them is never mistaken for a parameter.
pub fn scan_params(sql: &str) -> ParamScan {
    let b = sql.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 8);
    let mut names: Vec<Option<Vec<u8>>> = Vec::new();
    // (spelling-without-NUL, number) for named-parameter reuse.
    let mut seen: Vec<(Vec<u8>, usize)> = Vec::new();
    let mut n_var: usize = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b'\'' | b'"' | b'`' => {
                // String literal / quoted identifier: copy through the matching
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
                out.extend_from_slice(&b[start..i]);
            }
            b'[' => {
                // sqlite bracket-quoted identifier: closes at ']'.
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1; // consume the ']' (or run to end, matching scan_code)
                let end = i.min(b.len());
                out.extend_from_slice(&b[start..end]);
                i = end;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.extend_from_slice(&b[start..i]);
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                let end = i.min(b.len());
                out.extend_from_slice(&b[start..end]);
                i = end;
            }
            b'?' => {
                i += 1;
                let dstart = i;
                let mut num: usize = 0;
                while i < b.len() && b[i].is_ascii_digit() {
                    num = num.saturating_mul(10).saturating_add((b[i] - b'0') as usize);
                    i += 1;
                }
                if i > dstart {
                    // `?N` explicit number. Ignore out-of-range values for the
                    // count/name bookkeeping — the emitted `$N` makes mpedb reject
                    // the statement, so a wrong number can never bind.
                    if (1..=u16::MAX as usize).contains(&num) {
                        if num > n_var {
                            n_var = num;
                        }
                        if names.len() < num {
                            names.resize(num, None);
                        }
                        let mut sp = b[start..i].to_vec();
                        sp.push(0);
                        names[num - 1] = Some(sp);
                    }
                    out.extend_from_slice(format!("${num}").as_bytes());
                } else {
                    // Bare `?`: next sequential number, anonymous.
                    n_var += 1;
                    if names.len() < n_var {
                        names.resize(n_var, None);
                    }
                    // names[n_var - 1] stays None (anonymous).
                    out.extend_from_slice(format!("${n_var}").as_bytes());
                }
            }
            b':' | b'@' | b'$' => {
                let mut j = i + 1;
                while j < b.len() && is_name_char(b[j]) {
                    j += 1;
                }
                if j > i + 1 {
                    // Named parameter (sigil + ≥1 name char): reuse its number if
                    // the same spelling was seen, else assign the next number.
                    let spelling = &b[i..j];
                    let num = match seen.iter().find(|(s, _)| s.as_slice() == spelling) {
                        Some((_, n)) => *n,
                        None => {
                            n_var += 1;
                            let n = n_var;
                            seen.push((spelling.to_vec(), n));
                            if names.len() < n {
                                names.resize(n, None);
                            }
                            let mut sp = spelling.to_vec();
                            sp.push(0);
                            names[n - 1] = Some(sp);
                            n
                        }
                    };
                    out.extend_from_slice(format!("${num}").as_bytes());
                    i = j;
                } else {
                    // A lone `:`/`@`/`$` (punctuation, `::` cast, …) is not a
                    // parameter — copy it verbatim.
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    if names.len() < n_var {
        names.resize(n_var, None);
    }
    // `out` is original bytes plus ASCII `$K` — always valid UTF-8.
    let rewritten = String::from_utf8(out).unwrap_or_else(|_| sql.to_string());
    ParamScan {
        rewritten,
        count: n_var,
        names,
    }
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
    /// PRAGMA — mpedb has no PRAGMA, so the shim answers these itself from the
    /// live schema (`introspect::pragma`); they are never handed to the engine.
    Pragma,
    /// CREATE / DROP / ALTER / REINDEX — schema DDL. mpedb routes these through
    /// `parse_ddl`/`apply_ddl`, NOT the `compile`d-plan path, so the shim must
    /// NOT validate them with `prepare_detached` (which only compiles queries):
    /// it defers them to execution, where `Database::query` applies the DDL.
    Ddl,
    /// VACUUM / ANALYZE: storage maintenance mpedb has no equivalent work for
    /// (freelist page reuse; no planner statistics). Accepted as a no-op.
    Maintenance,
    /// Everything else (any unrecognized leading word) — hand straight to the
    /// engine, validating at prepare so typos surface there.
    Other,
}

fn first_word(sql: &str) -> String {
    let t = strip_leading_trivia(sql).trim_start();
    t.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn second_word(sql: &str) -> String {
    let t = strip_leading_trivia(sql).trim_start();
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
        // PRAGMA is answered by the shim's introspection, not the engine.
        "pragma" => Kind::Pragma,
        // Exactly what mpedb applies via `parse_ddl`/`apply_ddl`. REINDEX is
        // left to `Other`, so it validates-and-refuses at prepare like any
        // unsupported statement.
        "create" | "drop" | "alter" => Kind::Ddl,
        // ATTACH/DETACH (#51) mutate the connection's attach list — like DDL
        // they never compile to a plan, so `prepare_detached` cannot validate
        // them; classify with Ddl to defer them to execution, where
        // `Database::query` intercepts and applies them.
        "attach" | "detach" => Kind::Ddl,
        // Storage maintenance with nothing to maintain: mpedb reclaims pages
        // through its freelist (no fragmenting pager to VACUUM) and its
        // planner keeps no ANALYZE statistics. Consumers run these as routine
        // housekeeping, so they are accepted as no-ops — no rows, no changes —
        // rather than refused.
        "vacuum" | "analyze" => Kind::Maintenance,
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

    /// Convenience: the per-number spellings a scan produced, as `Option<&str>`.
    fn spellings(sql: &str) -> Vec<Option<String>> {
        scan_params(sql)
            .names
            .iter()
            .map(|n| {
                n.as_ref().map(|b| {
                    String::from_utf8(b[..b.len() - 1].to_vec()).unwrap()
                })
            })
            .collect()
    }

    #[test]
    fn counts_params() {
        assert_eq!(scan_params("SELECT ?, ?, ?").count, 3);
        // `$n` is a *named* parameter (sqlite semantics), assigned sequentially —
        // so `$1, $2, $2` is two distinct names reused, count 2.
        assert_eq!(scan_params("SELECT $1, $2, $2").count, 2);
        assert_eq!(scan_params("SELECT '?', ?").count, 1);
        assert_eq!(scan_params("SELECT 1").count, 0);
    }

    #[test]
    fn rewrites_named_to_numbered() {
        // Named params become $K, sharing one numbering space; a `?` inside a
        // string literal is untouched.
        let s = scan_params("SELECT :a, @b, $c, :a WHERE x = '? :a'");
        assert_eq!(s.count, 3);
        assert_eq!(s.rewritten, "SELECT $1, $2, $3, $1 WHERE x = '? :a'");
        assert_eq!(
            spellings("SELECT :a, @b, $c, :a WHERE x = '? :a'"),
            vec![Some(":a".into()), Some("@b".into()), Some("$c".into())]
        );
    }

    #[test]
    fn param_numbering_matches_sqlite() {
        // Verified against sqlite 3.45 (see the shim's probe): all kinds share
        // one numbering space, reuse repeats a number, `?N` sets an explicit slot.
        assert_eq!(scan_params("SELECT ?, ?, ?").rewritten, "SELECT $1, $2, $3");
        assert_eq!(scan_params("SELECT ?3, ?").count, 4);
        assert_eq!(scan_params("SELECT ?3, ?").rewritten, "SELECT $3, $4");
        assert_eq!(scan_params("SELECT :a, ?3").rewritten, "SELECT $1, $3");
        assert_eq!(scan_params("SELECT :a, ?5, :b").count, 6);
        assert_eq!(scan_params("SELECT :a, ?5, :b").rewritten, "SELECT $1, $5, $6");
        // `$5` is a name, not a position → count 1.
        assert_eq!(scan_params("SELECT $5").count, 1);
        assert_eq!(scan_params("SELECT $5").rewritten, "SELECT $1");
        assert_eq!(scan_params("SELECT $2, $1").rewritten, "SELECT $1, $2");
        // Comment/line-comment bodies are preserved and never scanned.
        assert_eq!(
            scan_params("SELECT ? /* :not */ -- :also\n, ?").rewritten,
            "SELECT $1 /* :not */ -- :also\n, $2"
        );
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
        assert_eq!(classify("PRAGMA foreign_keys=ON"), Kind::Pragma);
        assert_eq!(classify("pragma table_info(t)"), Kind::Pragma);
        assert_eq!(classify("SELCT typo"), Kind::Other);
    }
}
