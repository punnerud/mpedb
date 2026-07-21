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
    // `CREATE TRIGGER … BEGIN <body> END` contains `;` INSIDE it — one per
    // body statement. Splitting on the first one hands the caller a truncated
    // statement and a tail, which is why `conn.execute("CREATE TRIGGER …")`
    // used to answer "you can only execute one statement at a time": a
    // consumer could not create a trigger through this API at all.
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
    // `CREATE [TEMP|TEMPORARY] TRIGGER`.
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

/// `EXPLAIN QUERY PLAN <stmt>` — sqlite's plan-description statement. mpedb
/// spells the same request `EXPLAIN <stmt>` and answers with its own plan text,
/// so the shim strips the `QUERY PLAN` words and reshapes the answer into
/// sqlite's four-column `(id, parent, notused, detail)` result.
///
/// Returns the `<stmt>` body, or `None` when this is not an `EXPLAIN QUERY
/// PLAN`. A bare `EXPLAIN <stmt>` is NOT taken here: sqlite answers that with a
/// VDBE opcode listing, which mpedb has no equivalent of — it stays mpedb's own
/// single-column plan text rather than a fabricated opcode table.
pub fn explain_query_plan_body(sql: &str) -> Option<&str> {
    let mut rest = strip_leading_trivia(sql).trim_start();
    for word in ["explain", "query", "plan"] {
        let n = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphabetic())
            .count();
        if !rest[..n].eq_ignore_ascii_case(word) {
            return None;
        }
        rest = &rest[n..];
        // The words must be separated: `EXPLAINQUERY` is one identifier.
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            return None;
        }
        rest = rest.trim_start();
    }
    Some(rest)
}

/// Per-constraint `… ON CONFLICT ROLLBACK` on a CREATE TABLE clause.
///
/// Same ownership story as [`rewrite_insert_or_rollback`]: mpedb's schema
/// carries no per-constraint conflict action (ACCEPT would silently mean
/// ABORT — a wrong answer), so the engine refuses `ON CONFLICT ROLLBACK` by
/// name. The shim owns the connection's transaction, so it rewrites every
/// such clause to `ON CONFLICT ABORT` (same width — offsets preserved) and
/// returns whether any rewrite happened. The caller records the CREATE's
/// table name and, on a later `SQLITE_CONSTRAINT`, rolls the transaction
/// back — sqlite's definition of the action.
///
/// Only the spelling `ON CONFLICT ROLLBACK` is rewritten (quote/comment
/// aware). `IGNORE`/`REPLACE`/`FAIL` stay refused by the engine.
pub fn rewrite_on_conflict_rollback(sql: &str) -> (std::borrow::Cow<'_, str>, bool) {
    // Mark every code-region byte (not inside a string/quoted-ident/comment).
    let mut is_code = vec![false; sql.len()];
    scan_code(sql, |i, _| {
        if i < is_code.len() {
            is_code[i] = true;
        }
    });
    let lower = sql.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut hits: Vec<usize> = Vec::new(); // byte offset of 'r' in rollback
    let mut i = 0;
    while i + 2 <= lb.len() {
        if !is_code[i] {
            i += 1;
            continue;
        }
        // Word-boundary "on"
        if &lb[i..i + 2] == b"on"
            && (i == 0 || !lb[i - 1].is_ascii_alphanumeric())
            && (i + 2 == lb.len() || !lb[i + 2].is_ascii_alphanumeric())
        {
            let mut j = i + 2;
            while j < lb.len() && is_code[j] && lb[j].is_ascii_whitespace() {
                j += 1;
            }
            if j + 8 <= lb.len()
                && is_code[j]
                && &lb[j..j + 8] == b"conflict"
                && (j + 8 == lb.len() || !lb[j + 8].is_ascii_alphanumeric())
            {
                let mut k = j + 8;
                while k < lb.len() && is_code[k] && lb[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k + 8 <= lb.len()
                    && is_code[k]
                    && &lb[k..k + 8] == b"rollback"
                    && (k + 8 == lb.len() || !lb[k + 8].is_ascii_alphanumeric())
                {
                    hits.push(k);
                    i = k + 8;
                    continue;
                }
            }
        }
        i += 1;
    }
    if hits.is_empty() {
        return (std::borrow::Cow::Borrowed(sql), false);
    }
    let mut out = sql.to_string();
    for &at in hits.iter().rev() {
        // "rollback" and "ABORT   " are both 8 bytes.
        out.replace_range(at..at + 8, "ABORT   ");
    }
    (std::borrow::Cow::Owned(out), true)
}

/// Table name of a leading `CREATE TABLE [IF NOT EXISTS] <name>` statement,
/// or `None` when the text is not that shape. Used with
/// [`rewrite_on_conflict_rollback`] to remember which tables carry a
/// ROLLBACK conflict action on this connection.
pub fn create_table_name(sql: &str) -> Option<String> {
    let head = strip_leading_trivia(sql).trim_start();
    let mut rest = head;
    // CREATE TABLE [IF NOT EXISTS] name
    for expect in ["create", "table"] {
        let trimmed = rest.trim_start();
        let word: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if word != expect {
            return None;
        }
        rest = &trimmed[word.len()..];
    }
    rest = rest.trim_start();
    // Optional IF NOT EXISTS
    {
        let t = rest;
        let w1: String = t
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if w1 == "if" {
            let mut r = &t[w1.len()..];
            r = r.trim_start();
            let w2: String = r
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .flat_map(|c| c.to_lowercase())
                .collect();
            if w2 == "not" {
                r = &r[w2.len()..];
                r = r.trim_start();
                let w3: String = r
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .flat_map(|c| c.to_lowercase())
                    .collect();
                if w3 == "exists" {
                    rest = r[w3.len()..].trim_start();
                }
            }
        }
    }
    // name: bare, "quoted", [bracket], `tick`
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (name, _) = match bytes[0] {
        b'"' | b'`' => {
            let q = bytes[0];
            let end = rest[1..].find(q as char)? + 1;
            (rest[1..end].to_string(), end + 1)
        }
        b'[' => {
            let end = rest.find(']')?;
            (rest[1..end].to_string(), end + 1)
        }
        _ => {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (rest[..end].to_string(), end)
        }
    };
    // Drop a schema qualifier `main.t` → `t` (sqlite's create-table text rule).
    let name = name
        .rsplit_once('.')
        .map(|(_, n)| n.to_string())
        .unwrap_or(name);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// `INSERT OR ROLLBACK …` / `REPLACE`-style conflict prefix handling.
///
/// sqlite's five conflict actions differ only in what survives a constraint
/// violation, and four of them are statement-local — mpedb's parser takes
/// those (`IGNORE`, `REPLACE`, `ABORT`, and `FAIL` on a single-row source).
/// `ROLLBACK` is the odd one out: it aborts the enclosing TRANSACTION, which
/// no statement can reach, so mpedb's parser refuses it by name.
///
/// The shim is the layer that *does* own the transaction (`Sqlite3::txn`), so
/// it implements `OR ROLLBACK` itself: the statement runs as `OR ABORT`, and a
/// constraint failure rolls the connection's open transaction back before the
/// error is returned. That is exactly sqlite's definition of the action.
///
/// Returns the rewritten text (`ROLLBACK` → `ABORT`, same byte length so every
/// downstream offset is unchanged) and whether the prefix was present. Only a
/// leading `INSERT OR ROLLBACK` is recognized — the conflict prefix has no
/// other legal position.
pub fn rewrite_insert_or_rollback(sql: &str) -> (std::borrow::Cow<'_, str>, bool) {
    let head = strip_leading_trivia(sql).trim_start();
    let off = sql.len() - head.len();
    let mut end = 0usize;
    let mut rest = head;
    let mut consumed = 0usize;
    // The prefix is exactly the first three words: INSERT, OR, ROLLBACK.
    for expect in ["insert", "or", "rollback"] {
        let trimmed = rest.trim_start();
        consumed += rest.len() - trimmed.len();
        let word: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if word != expect {
            return (std::borrow::Cow::Borrowed(sql), false);
        }
        consumed += word.len();
        end = consumed;
        rest = &trimmed[word.len()..];
    }
    let start = off + end - "rollback".len();
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..start]);
    out.push_str("ABORT   "); // same width as "rollback": offsets are preserved
    out.push_str(&sql[start + "rollback".len()..]);
    (std::borrow::Cow::Owned(out), true)
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

/// The widest argument count of any FUNCTION CALL in `sql`, with the name that
/// carried it — `None` when the statement calls no function this shim can
/// recognize.
///
/// This exists for one job: `SQLITE_LIMIT_FUNCTION_ARG`, which sqlite enforces
/// in its parser ("too many arguments on function <name>"). The shim has no
/// parse tree at that point, so it counts from the text — and the danger of
/// counting from text is calling something a function that is not one:
/// `VALUES (1,2)`, `IN (a,b)` and `CREATE TABLE t(a,b)` all read as
/// `<name>(<list>)`. Two guards make that structurally impossible rather than
/// merely unlikely:
///
/// 1. **`known` decides.** Only a name the connection can actually CALL — a
///    built-in or a registered host function/aggregate — is counted. `values`,
///    `in`, a table name and a column list are not functions, so they are never
///    counted, whatever they look like.
/// 2. **A preceding `INTO`/`TABLE`/`FROM`/`JOIN`/`UPDATE` disqualifies.** That
///    covers the one residual shape — a TABLE named like a function
///    (`insert into max(a,b) …`).
///
/// Nesting and strings fall out of the scan: commas are counted only at the
/// call's own paren depth, and `scan_code` never reports a byte inside a string,
/// a quoted identifier or a comment, so `f('a,b')` is one argument.
pub fn max_function_args(sql: &str, known: impl Fn(&str) -> bool) -> Option<(String, usize)> {
    // Code bytes only, in order: strings/comments are already gone.
    let mut code: Vec<(usize, u8)> = Vec::new();
    scan_code(sql, |i, c| code.push((i, c)));
    let mut worst: Option<(String, usize)> = None;
    let mut prev_word: Option<String> = None;
    let mut k = 0usize;
    while k < code.len() {
        let (off, c) = code[k];
        if !(c.is_ascii_alphabetic() || c == b'_') {
            k += 1;
            continue;
        }
        // One identifier run: contiguous in the ORIGINAL text, so a comment
        // cannot glue two names into one.
        let start = k;
        let mut end = k + 1;
        while end < code.len()
            && code[end].0 == code[end - 1].0 + 1
            && (code[end].1.is_ascii_alphanumeric() || code[end].1 == b'_')
        {
            end += 1;
        }
        let name: String = code[start..end].iter().map(|&(_, b)| b as char).collect();
        let lower = name.to_ascii_lowercase();
        k = end;
        let _ = off;
        // The next code byte, whitespace skipped, decides whether this is a call.
        let mut j = k;
        while j < code.len() && code[j].1.is_ascii_whitespace() {
            j += 1;
        }
        let opens = j < code.len() && code[j].1 == b'(';
        let after_name_kw = matches!(
            prev_word.as_deref(),
            Some("into") | Some("table") | Some("from") | Some("join") | Some("update")
        );
        prev_word = Some(lower.clone());
        if !opens || after_name_kw || !known(&lower) {
            continue;
        }
        // Count arguments: 0 for `f()`, otherwise 1 + commas at depth 1.
        let mut depth = 0i32;
        let mut commas = 0usize;
        let mut empty = true;
        let mut p = j;
        while p < code.len() {
            match code[p].1 {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                b',' if depth == 1 => commas += 1,
                b if !b.is_ascii_whitespace() => empty = false,
                _ => {}
            }
            p += 1;
        }
        let args = if empty { 0 } else { commas + 1 };
        if worst.as_ref().is_none_or(|(_, w)| args > *w) {
            worst = Some((name, args));
        }
        k = j + 1;
        prev_word = None;
    }
    worst
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

    /// A `CREATE TRIGGER` body's `;`s are INSIDE the statement — splitting on
    /// the first one made `conn.execute("CREATE TRIGGER …")` answer "you can
    /// only execute one statement at a time", so a consumer could not create a
    /// trigger through this API at all. `END` also closes a `CASE`, so the
    /// scan has to count depth rather than stop at the first `END`.
    #[test]
    fn a_trigger_body_is_one_statement() {
        let t = "CREATE TRIGGER tr UPDATE OF b ON t1 BEGIN \
                 UPDATE t2 SET b = new.b WHERE a = old.a; END";
        let src = format!("{t}; SELECT 1");
        let (a, b) = split_first(&src);
        assert_eq!(a, t);
        assert_eq!(b.trim(), "SELECT 1");
        // No trailing semicolon at all.
        assert_eq!(split_first(t), (t, ""));
        // Two body statements, and a CASE whose END is not the trigger's.
        let t2 = "CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN \
                  UPDATE u SET v = CASE WHEN new.x > 0 THEN 1 ELSE 2 END; \
                  DELETE FROM w; END";
        let src2 = format!("{t2}; SELECT 2");
        let (a, b) = split_first(&src2);
        assert_eq!(a, t2);
        assert_eq!(b.trim(), "SELECT 2");
        // A `;` in a comment or a literal inside the body is still not a split.
        let t3 = "CREATE TRIGGER tr BEFORE DELETE ON t BEGIN \
                  INSERT INTO log VALUES ('a;b'); -- ; not here\n END";
        assert_eq!(split_first(t3), (t3, ""));
        // Not a trigger: the ordinary rule still applies.
        assert_eq!(split_first("CREATE TABLE t (x); SELECT 1").0, "CREATE TABLE t (x)");
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

    /// The zeroblob rewrite must fire ONLY in code position: a `zeroblob(...)`
    /// spelled inside a string literal, a quoted identifier or a comment is
    /// data or prose, and rewriting it would corrupt the statement. It must
    /// also respect identifier boundaries (`myzeroblob(3)` is a user function).
    #[test]
    fn zeroblob_rewrite_only_touches_code_position() {
        // Code position: rewritten to the byte-identical blob literal.
        assert_eq!(rewrite_zeroblob("select zeroblob(2)"), "select x'0000'");
        assert_eq!(rewrite_zeroblob("insert into t values (zeroblob(1))"), "insert into t values (x'00')");
        // Case-insensitive, whitespace-tolerant, and repeated.
        assert_eq!(rewrite_zeroblob("select ZeroBlob( 1 ), zeroblob(2)"), "select x'00', x'0000'");
        // sqlite's argument coercions: NULL/negative/non-numeric text are empty,
        // a numeric-prefix string and a float truncate toward zero.
        for (sql, want) in [
            ("select zeroblob(0)", "select x''"),
            ("select zeroblob(-5)", "select x''"),
            ("select zeroblob(NULL)", "select x''"),
            ("select zeroblob('x')", "select x''"),
            ("select zeroblob('2')", "select x'0000'"),
            ("select zeroblob(2.9)", "select x'0000'"),
        ] {
            assert_eq!(rewrite_zeroblob(sql), want, "{sql}");
        }

        // NOT code position — every one must come back byte-identical.
        for sql in [
            "select 'zeroblob(5) in a string'",
            "select \"zeroblob(5)\" from t",
            "select `zeroblob(5)` from t",
            "select [zeroblob(5)] from t",
            "select 1 -- zeroblob(5) in a line comment",
            "select /* zeroblob(5) in a block comment */ 1",
            "update t set s = 'zeroblob(9)' where x = 1",
        ] {
            assert_eq!(rewrite_zeroblob(sql), sql, "must not rewrite: {sql}");
        }

        // Identifier boundaries and shapes the rewrite must decline, leaving
        // them for the host UDF (a non-constant argument) or the parser.
        for sql in [
            "select myzeroblob(3)",
            "select zeroblobx(3)",
            "select zeroblob_(3)",
            "select zeroblob(1+1)",
            "select zeroblob($1)",
            "select zeroblob(a)",
            "select zeroblob()",
            "select zeroblob",
        ] {
            assert_eq!(rewrite_zeroblob(sql), sql, "must decline: {sql}");
        }

        // A statement with no zeroblob at all is returned borrowed, untouched.
        assert!(matches!(
            rewrite_zeroblob("select * from t where a = 1"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// The rewrite runs BEFORE `scan_params`, so it must never disturb
    /// parameter numbering around it.
    #[test]
    fn zeroblob_rewrite_preserves_parameter_numbering() {
        let sql = "insert into t (a, b, c) values (?, zeroblob(2), ?)";
        let rewritten = rewrite_zeroblob(sql);
        let scan = scan_params(&rewritten);
        assert_eq!(scan.rewritten, "insert into t (a, b, c) values ($1, x'0000', $2)");
        assert_eq!(scan.count, 2);
    }
}
