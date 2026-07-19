//! SQL `IN` three-valued-logic core, the LIKE / GLOB matchers, and the
//! bitwise operators.

use super::*;

// ===== Bitwise `& | << >> ~` — sqlite's `OP_BitAnd`..`OP_BitNot` =====
//
// sqlite defines all five in terms of ONE coercion: both operands go through
// `sqlite3VdbeIntValue`, and the result is always an integer. That coercion is
// total — it never errors — which is why the operators have no type rules of
// their own, only NULL propagation. Ported here value-for-value against
// sqlite 3.45.1 rather than approximated, because every one of these corners
// is a silent wrong answer if guessed:
//
// | input | sqlite | why |
// |---|---|---|
// | `3.7 \| 1` | 3 | reals TRUNCATE toward zero, they do not round |
// | `1e300 \| 0` | i64::MAX | and CLAMP, they do not wrap |
// | `'3' \| 1` | 3 | text takes an integer-PREFIX parse … |
// | `'1e3' \| 0` | 1 | … which stops at `e`, unlike `CAST('1e3' AS INTEGER)` |
// | `'abc' \| 1` | 1 | no digits at all is 0, not an error |
// | `1 << 64` | 0 | a count of 64+ clears the value … |
// | `-1 >> 64` | -1 | … except `>>` is ARITHMETIC, so a negative stays -1 |
// | `1 >> -1` | 2 | a NEGATIVE count shifts the other way |
// | `1 << -64` | 0 | counts at or below -64 clamp to 64 |
// | `9223372036854775807 << 1` | -2 | `<<` WRAPS; a bit shift has no overflow |
//
// The binder does not let a statically-typed real, text or blob reach these —
// it refuses with a message naming `CAST` — so the non-integer arms are
// reachable only through an `any` (typeless) value, which is exactly the
// contract `Instr::CmpClass` already has for comparisons.

/// sqlite's `sqlite3VdbeIntValue`: a value as the i64 a bitwise operator sees.
/// `None` means NULL (the caller propagates it); [`Error::TypeMismatch`] is
/// reserved for mpedb's own `Timestamp`/`List`, which have no sqlite storage
/// class and so have no sqlite answer to reproduce.
pub(super) fn bit_i64(v: &Value) -> Result<Option<i64>> {
    Ok(Some(match v {
        Value::Null => return Ok(None),
        Value::Int(x) => *x,
        // sqlite has no boolean type: it IS the integer 0/1, the same mapping
        // the binder already uses for `SET int_col = (a = b)`.
        Value::Bool(b) => *b as i64,
        Value::Float(f) => real_to_i64(*f),
        Value::Text(s) => atoi64(s.as_bytes()),
        Value::Blob(b) => atoi64(b),
        other => {
            return Err(Error::TypeMismatch(format!(
                "a bitwise operator has no sqlite meaning for {}",
                other.type_name()
            )))
        }
    }))
}

/// sqlite's `sqlite3RealToI64`: truncate toward zero, clamping at both ends of
/// the i64 range. The bounds are `>=`/`<=` against ±2^63 because that is the
/// double nearest each limit — `9.3e18` is above i64::MAX and clamps.
///
/// NaN yields 0 (Rust's `as` rule). sqlite's C cast is undefined there, but it
/// is unreachable from SQL: sqlite folds every NaN-producing expression to
/// NULL before an operator sees it.
fn real_to_i64(r: f64) -> i64 {
    if r.is_nan() {
        0
    } else if r <= -9_223_372_036_854_775_808.0 {
        i64::MIN
    } else if r >= 9_223_372_036_854_775_808.0 {
        i64::MAX
    } else {
        r as i64
    }
}

/// sqlite's `sqlite3Atoi64`, as `memIntValue` calls it — the return code is
/// ignored there, so a partial or absent parse is simply the value reached:
/// leading whitespace, one optional sign, then the DIGIT PREFIX. Anything from
/// the first non-digit on is dropped, which is why `'1e3'` is 1 and `'9x'` is 9.
/// Overflow clamps to the end of the i64 range rather than wrapping.
fn atoi64(bytes: &[u8]) -> i64 {
    let mut i = 0;
    // sqlite3Isspace: space, \t, \n, \v, \f, \r.
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')) {
        i += 1;
    }
    let neg = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut u: u64 = 0;
    let mut overflow = false;
    while let Some(c) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
        if !overflow {
            match u.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as u64)) {
                Some(v) => u = v,
                None => overflow = true,
            }
        }
        i += 1;
    }
    if overflow || u > i64::MAX as u64 {
        if neg {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if neg {
        -(u as i64)
    } else {
        u as i64
    }
}

/// `a & b`, `a | b`, `a << b`, `a >> b`. Any NULL operand yields NULL.
pub(super) fn bitwise(op: Instr, a: &Value, b: &Value) -> Result<Value> {
    let (Some(x), Some(y)) = (bit_i64(a)?, bit_i64(b)?) else {
        return Ok(Value::Null);
    };
    Ok(Value::Int(match op {
        Instr::BitAnd => x & y,
        Instr::BitOr => x | y,
        Instr::Shl => shift(x, y, true),
        Instr::Shr => shift(x, y, false),
        _ => unreachable!("bitwise() called with {op:?}"),
    }))
}

/// sqlite's `OP_ShiftLeft` / `OP_ShiftRight` body.
///
/// A NEGATIVE count shifts the other way — sqlite's own comment is "bit shifts
/// by a negative amount are the same as shifts in the opposite direction with
/// a positive amount". `-64` and below clamp to 64 rather than negating (which
/// would overflow at `i64::MIN`).
///
/// At 64 or more the value is cleared, except that `>>` is ARITHMETIC: a
/// negative value shifted right past its width stays `-1`. Rust's `i64 >> n`
/// already sign-extends for `n` in `0..64`, so only the `>= 64` case is
/// explicit. `<<` goes through `u64` so it WRAPS: sqlite shifts the bit
/// pattern, and this is the one place mpedb does not raise on integer
/// overflow — a bit shift has no overflow, only a bit pattern.
fn shift(a: i64, count: i64, left: bool) -> i64 {
    let (n, left) = if count < 0 {
        (if count > -64 { -count } else { 64 }, !left)
    } else {
        (count, left)
    };
    if n >= 64 {
        if left || a >= 0 {
            0
        } else {
            -1
        }
    } else if left {
        ((a as u64) << n) as i64
    } else {
        a >> n
    }
}

/// SQL `x IN (…)` under three-valued logic — the semantics that decide whether
/// a policy admits a row, so they are spelled out rather than approximated:
///
/// | case | result | why |
/// |---|---|---|
/// | `x` is NULL | **NULL** | never TRUE; a filter needs exactly TRUE, so the row stays invisible |
/// | `x` equals some element | **TRUE** | a match wins even if other elements are NULL — which is why the NULL scan cannot come first |
/// | no match, some element NULL | **NULL** | the NULL *might* have been the match; SQL refuses to say FALSE |
/// | no match, no NULL elements | **FALSE** | |
/// | empty list | **FALSE** | nothing to match, and NOT NULL: an empty membership set means "belongs to nothing" and must deny cleanly |
///
/// The `IS NOT DISTINCT FROM` reading (NULL matching NULL) is deliberately NOT
/// used: standard `IN` compares with `=`, and a context list containing NULL
/// must not silently make NULL-keyed rows visible.
pub(super) fn in_list_3vl(probe: &Value, list: &Value) -> Result<Value> {
    let items = match list {
        Value::List(items) => items,
        Value::Null => {
            // The whole set is NULL (e.g. an unset context key bound to NULL):
            // membership in an unknown set is unknown.
            return Ok(Value::Null);
        }
        other => {
            return Err(Error::TypeMismatch(format!(
                "IN expects a context list, got {}",
                other.type_name()
            )))
        }
    };
    in_items_3vl(probe, items)
}

/// The 3VL core, over items from anywhere. Shared by [`Instr::InParam`] (items
/// from a param-bound list) and [`Instr::InList`] (items from the stack) so the
/// two forms cannot drift apart on the NULL rules above — which decide whether
/// a policy admits a row.
pub(super) fn in_items_3vl(probe: &Value, items: &[Value]) -> Result<Value> {
    // `x IN ()` — membership in the EMPTY set — is FALSE for any `x`, NULL
    // included: nothing is a member of nothing (SQL 3VL). This MUST precede the
    // null-probe short-circuit below, or `NULL IN (<empty subquery>)` wrongly
    // yields NULL where sqlite/PostgreSQL give 0. Fires for a literal `IN ()`
    // (a zero-element `InList`), an empty subquery, and an empty context list
    // alike — all three share this core.
    if items.is_empty() {
        return Ok(Value::Bool(false));
    }
    if probe.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for it in items {
        if it.is_null() {
            saw_null = true;
            continue;
        }
        // Type mismatches inside the list are the caller's error, not a silent
        // non-match: `org_id IN (list-of-text)` must not quietly deny every row.
        match probe.sql_cmp(it)? {
            Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
            _ => continue,
        }
    }
    Ok(if saw_null { Value::Null } else { Value::Bool(false) })
}

/// The 3VL core for `x COLLATE <coll> IN (…)` — identical to [`in_items_3vl`]
/// except text membership is decided under an explicit collation. Every NULL /
/// empty-set rule above is preserved verbatim; only the equality test changes.
pub(super) fn in_items_3vl_collated(
    probe: &Value,
    items: &[Value],
    coll: Collation,
) -> Result<Value> {
    if items.is_empty() {
        return Ok(Value::Bool(false));
    }
    if probe.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for it in items {
        if it.is_null() {
            saw_null = true;
            continue;
        }
        match probe.sql_cmp_collated(it, coll)? {
            Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
            _ => continue,
        }
    }
    Ok(if saw_null { Value::Null } else { Value::Bool(false) })
}

/// SQL LIKE: `%` matches any run, `_` matches one char. Iterative
/// two-pointer algorithm — O(n·m) worst case, no recursion, no regex dep.
///
/// **Case-insensitive for ASCII A–Z**, matching sqlite's default (`'a' LIKE 'A'`
/// is true; Unicode is NOT casefolded, exactly like NOCASE and sqlite itself).
/// GLOB stays case-sensitive. (Note: PostgreSQL's LIKE is case-sensitive — this
/// is the canonical sqlite/PG divergence; sqlite is mpedb's default and the
/// semantics the C-API drop-in must present. A `bare_group_by = "postgres"`
/// database instead compiles case-SENSITIVE LIKE via [`like_match_cs`] behind the
/// [`Instr::LikeCs`](super::Instr::LikeCs) opcode.)
///
/// `esc` is the `LIKE … ESCAPE c` character, or `None` for a bare LIKE. See
/// [`compile_pattern`] for the escape rules — they are sqlite's, verbatim.
pub(super) fn like_match(pattern: &str, s: &str, esc: Option<char>) -> bool {
    like_impl(pattern, s, true, esc)
}

/// Case-SENSITIVE LIKE (PostgreSQL dialect): identical to [`like_match`] except
/// literal characters compare exactly (`'a' LIKE 'A'` is FALSE). The `%`/`_`
/// wildcards — and the ESCAPE rules — behave the same.
pub(super) fn like_match_cs(pattern: &str, s: &str, esc: Option<char>) -> bool {
    like_impl(pattern, s, false, esc)
}

/// The single character of a `LIKE … ESCAPE <c>` argument.
///
/// sqlite's `likeFunc` raises `ESCAPE expression must be a single character`
/// for anything else, and mpedb's binder refuses a non-single-character literal
/// at PREPARE time — so this is a decode/validate-level guard against a
/// hand-built plan, not a user-facing path, and it must never panic.
pub(super) fn escape_char(v: &crate::value::Value) -> crate::error::Result<char> {
    match v {
        crate::value::Value::Text(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(c),
                _ => Err(crate::error::Error::Corrupt(
                    "ESCAPE expression must be a single character".into(),
                )),
            }
        }
        _ => Err(crate::error::Error::Corrupt(
            "ESCAPE expression must be a single character".into(),
        )),
    }
}

/// One element of a compiled LIKE pattern. Making the wildcards distinct from a
/// literal `%`/`_` is what lets ESCAPE work at all: after compilation there is
/// no way to confuse an escaped `%` with the any-run wildcard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pat {
    /// `%` — matches any run of characters, including none.
    Any,
    /// `_` — matches exactly one character (never the end of the subject).
    One,
    /// A literal character (possibly one that was escaped).
    Lit(char),
}

/// Compile a LIKE pattern under an optional ESCAPE character, mirroring
/// sqlite's `patternCompare` + `likeFunc` exactly:
///
/// - The escape character is tested BEFORE `%` and `_`, which reproduces
///   sqlite's rule that `ESCAPE '%'` clears `matchAll` and `ESCAPE '_'` clears
///   `matchOne` (`likeFunc`): with `ESCAPE '%'`, `'axb' LIKE 'a%b'` is FALSE
///   because the `%` escapes the `b` instead of matching a run.
/// - The escaped character is a LITERAL whatever it is — sqlite does not
///   restrict it to `%`/`_`/itself, so `'ab' LIKE 'a\b' ESCAPE '\'` is TRUE.
/// - A literal produced by an escape still compares case-INsensitively under
///   the sqlite dialect (`'aB' LIKE '\a\B' ESCAPE '\'` is TRUE).
/// - A DANGLING escape at the end of the pattern makes the comparison fail
///   against every subject (sqlite returns `NOMATCH`/`NOWILDCARDMATCH` the
///   moment it reads past the pattern's end), which is `None` here.
fn compile_pattern(pattern: &str, esc: Option<char>) -> Option<Vec<Pat>> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut it = pattern.chars();
    while let Some(c) = it.next() {
        if Some(c) == esc {
            out.push(Pat::Lit(it.next()?));
        } else if c == '%' {
            out.push(Pat::Any);
        } else if c == '_' {
            out.push(Pat::One);
        } else {
            out.push(Pat::Lit(c));
        }
    }
    Some(out)
}

// The LAST `(pattern, escape)` this thread compiled and the compiled form —
// `None` for a pattern with a DANGLING escape, which matches nothing. That
// failure is CACHED and stays a plain no-match, deliberately unlike REGEXP's
// named error: sqlite's `patternCompare` returns NOMATCH the moment it reads
// past the pattern's end, so "matches nothing" IS the binary's answer here,
// not a swallowed dialect gap (the W3 lesson does not transfer).
//
// One entry, not an LRU, for `RE_MEMO`'s reason: a scan's pattern is the same
// on every row whether it arrived as a literal or — since the LIKE half of
// #74 item 3 — as a bound parameter, so a single slot has an LRU's hit rate
// and none of the bookkeeping. Purely a memo of a deterministic function of
// `(pattern, esc)`, so it cannot change an answer; it exists because
// `like_impl` was recompiling the pattern PER ROW even in the literal case.
// The key does NOT include case-sensitivity: the compiled form is
// dialect-independent (`ci` only changes the literal comparison at match
// time).
type LikeMemoEntry = (String, Option<char>, Option<Vec<Pat>>);
std::thread_local! {
    static LIKE_MEMO: std::cell::RefCell<Option<LikeMemoEntry>> =
        const { std::cell::RefCell::new(None) };
}

/// Shared LIKE matcher. `ci` selects case-INsensitive (ASCII A–Z, sqlite) vs
/// case-sensitive (PostgreSQL) comparison of a literal pattern char; the `%`/`_`
/// wildcards and the two-pointer backtracking are identical either way.
fn like_impl(pattern: &str, s: &str, ci: bool, esc: Option<char>) -> bool {
    LIKE_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if !matches!(&*memo, Some((p, e, _)) if p == pattern && *e == esc) {
            *memo = Some((pattern.to_string(), esc, compile_pattern(pattern, esc)));
        }
        match &memo.as_ref().expect("just filled").2 {
            // A dangling ESCAPE never matches anything — not even the empty
            // subject (worth caching too: it is reached on every row).
            None => false,
            Some(p) => like_match_compiled(p, s, ci),
        }
    })
}

/// The two-pointer match over an already-compiled pattern.
fn like_match_compiled(p: &[Pat], s: &str, ci: bool) -> bool {
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);
    while ti < t.len() {
        // The wildcard branch MUST precede the literal branch: a literal '%'
        // in the SUBJECT would otherwise consume the pattern's '%' as a
        // one-character match ('a%c' LIKE 'a%' must be TRUE).
        if pi < p.len() && p[pi] == Pat::Any {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < p.len()
            && match p[pi] {
                Pat::Any => false,
                Pat::One => true,
                Pat::Lit(c) => {
                    if ci {
                        c.eq_ignore_ascii_case(&t[ti])
                    } else {
                        c == t[ti]
                    }
                }
            }
        {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            star_ti += 1;
            pi = star_pi + 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == Pat::Any {
        pi += 1;
    }
    pi == p.len()
}

/// Result of matching one non-`*` GLOB pattern token against a single string
/// character. A token is a literal, `?`, or a `[...]` set.
enum GlobTok {
    /// Matched; the pattern index just past this token.
    Yes(usize),
    /// A well-formed token that did NOT match this character.
    No,
    /// A `[` set with no closing `]`. sqlite treats that as a whole-match
    /// failure (`patternCompare` returns NOMATCH), so the caller stops.
    Unterminated,
}

/// Match the GLOB `[...]` set at `p[start]` (`p[start] == '['`) against char
/// `c`. Mirrors sqlite `patternCompare`'s set logic:
/// - a leading `^` inverts the class;
/// - a `]` immediately after `[`/`[^` is a LITERAL member, not the terminator;
/// - `a-z` is a range, but a `-` that is first, last-before-`]`, or right after
///   a completed range is a literal `-`;
/// - an unterminated set fails the whole comparison.
fn glob_set(p: &[char], start: usize, c: char) -> GlobTok {
    let mut i = start + 1;
    let mut invert = false;
    let mut seen = false;
    // The previous set member available to start a range. `None` (sqlite's
    // `prior_c == 0`) means no range can start here — which is why a leading
    // literal `]` deliberately leaves it unset.
    let mut prior: Option<char> = None;
    if i < p.len() && p[i] == '^' {
        invert = true;
        i += 1;
    }
    if i < p.len() && p[i] == ']' {
        if c == ']' {
            seen = true;
        }
        i += 1; // leading `]` is literal; prior stays None (sqlite parity)
    }
    while i < p.len() && p[i] != ']' {
        let ch = p[i];
        if ch == '-' && prior.is_some() && i + 1 < p.len() && p[i + 1] != ']' {
            let lo = prior.expect("checked is_some");
            let hi = p[i + 1];
            if c >= lo && c <= hi {
                seen = true;
            }
            prior = None; // a completed range cannot itself start another
            i += 2;
        } else {
            if ch == c {
                seen = true;
            }
            prior = Some(ch);
            i += 1;
        }
    }
    if i >= p.len() {
        return GlobTok::Unterminated; // no closing `]`
    }
    if seen ^ invert {
        GlobTok::Yes(i + 1)
    } else {
        GlobTok::No
    }
}

/// sqlite GLOB: `*` matches any run, `?` matches exactly one char, and `[...]`
/// is a character class (`[^...]`, ranges). Case-SENSITIVE (unlike LIKE, which
/// sqlite also leaves case-sensitive but with `%`/`_`). Iterative two-pointer
/// with `*` backtracking — O(n·m) worst case, no recursion, no regex dep, the
/// same shape as [`like_match`].
pub(super) fn glob_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
            continue;
        }
        // Does the current (non-`*`) token match this character?
        let matched = if pi < p.len() {
            match p[pi] {
                '?' => Some(pi + 1),
                '[' => match glob_set(&p, pi, t[ti]) {
                    GlobTok::Yes(next) => Some(next),
                    GlobTok::No => None,
                    // An unterminated set fails the whole comparison, at every
                    // position — so no amount of `*` backtracking can rescue it.
                    GlobTok::Unterminated => return false,
                },
                c if c == t[ti] => Some(pi + 1),
                _ => None,
            }
        } else {
            None
        };
        if let Some(next_pi) = matched {
            pi = next_pi;
            ti += 1;
        } else if star_pi != usize::MAX {
            // Let the most recent `*` absorb one more character.
            star_ti += 1;
            pi = star_pi + 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ===== REGEXP — sqlite's bundled `ext/misc/regexp.c` dialect =====
//
// `x REGEXP y` is sugar for `regexp(y, x)`: the sqlite CLI ships a compact
// UTF-8 regex engine. Supported here, cross-checked against sqlite 3.45:
//   `.` (any char, incl. newline), quantifiers `*` `+` `?`, counts
//   `{p}`/`{p,}`/`{p,q}`, classes `[...]`/`[^...]` with ranges `[a-z]`,
//   anchors `^`/`$`, alternation `|`, grouping `(...)`, the Perl escapes
//   `\d \D \w \W \s \S` and word-boundary `\b`, the C escapes `\a \f \n \r
//   \t \v`, `\uXXXX` / `\xXX`, and `\` before a metacharacter. Case-SENSITIVE,
//   and matches a SUBSTRING (unanchored) unless `^`/`$` pin an end.
//
// Implemented as a hand-rolled Thompson NFA (no backtracking, no regex crate) —
// the same non-exponential shape sqlite uses, so a hostile pattern cannot hang a
// reader. A pattern this engine cannot compile (unmatched `(`/`{`, unterminated
// `[`, unknown `\` escape, `{m,n}` with n<m or both zero, a quantifier with no
// operand — including one applied to `^`/`$`) is a named ERROR, exactly as
// sqlite's own regexp extension errors on a malformed pattern at runtime.
//
// It used to "match NOTHING" instead — a policy that turned into wrong answer
// W3 the moment bound patterns landed: a consumer whose registered `regexp()`
// UDF speaks a richer dialect (Python's `(?i)…`, backreferences — Django's
// `__iregex` prepends `(?i)` to EVERY pattern) got silent empty results where
// stock sqlite returns rows. An error is honest in both worlds: the dialect
// gap is named instead of swallowed. (The full fix — dispatching `x REGEXP y`
// to a registered host `regexp()` UDF, which is the operator's entire meaning
// in real sqlite — is tracked separately.)

// The LAST pattern this thread compiled, and the program it compiled to
// (`None` for a pattern the engine rejects — that result is worth caching too,
// since it is reached on every row of the scan).
//
// One entry, not an LRU: a REGEXP's pattern is the same on every row of a
// scan, whether it arrived as a literal or — since #74 item 3 — as a bound
// parameter, so a single slot has the hit rate an LRU would and none of the
// bookkeeping. Purely a memo of a deterministic function of `pattern`, so it
// cannot change an answer; it exists because `regexp_match` was recompiling the
// pattern PER ROW even in the literal case, which is what made "the pattern
// must be a literal" look like a performance guard when it never was one.
std::thread_local! {
    static RE_MEMO: std::cell::RefCell<Option<(String, Option<ReProg>)>> =
        const { std::cell::RefCell::new(None) };
}

/// sqlite `x REGEXP y`: does `pattern` (the sqlite regexp dialect) match some
/// substring of `text`? A pattern that fails to compile is a named error.
pub(super) fn regexp_match(pattern: &str, text: &str) -> crate::Result<bool> {
    RE_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if !matches!(&*memo, Some((p, _)) if p == pattern) {
            *memo = Some((pattern.to_string(), ReProg::compile(pattern)));
        }
        match &memo.as_ref().expect("just filled").1 {
            Some(prog) => Ok(prog.is_match(text)),
            None => Err(crate::Error::Unsupported(format!(
                "REGEXP pattern {pattern:?} is not valid in mpedb's regexp \
                 dialect (POSIX-style: classes, anchors, alternation, counts, \
                 Perl escapes; no lookaround, no backreferences, no (?i) flags)"
            ))),
        }
    })
}

/// One member of a `[...]` class: a single char or an inclusive range. A range
/// whose low exceeds its high (`[c-a]`) simply matches nothing, exactly as in
/// sqlite.
#[derive(Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

/// A compiled `[...]` / `[^...]` character class.
#[derive(Clone)]
struct ClassSpec {
    negate: bool,
    items: Vec<ClassItem>,
}

impl ClassSpec {
    fn matches(&self, c: char) -> bool {
        let hit = self.items.iter().any(|it| match it {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        });
        hit ^ self.negate
    }
}

/// The Perl escape classes. `\d`/`\w`/`\s` are the positive forms; the negated
/// `\D`/`\W`/`\S` reuse the same test with the boolean flipped.
#[derive(Clone, Copy)]
enum Perl {
    Digit,
    Word,
    Space,
}

fn perl_test(kind: Perl, c: char) -> bool {
    match kind {
        Perl::Digit => c.is_ascii_digit(),
        Perl::Word => c == '_' || c.is_ascii_alphanumeric(),
        // sqlite's \s: space, tab, newline, CR, vertical tab, form feed.
        Perl::Space => matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0B}' | '\u{0C}'),
    }
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Regex AST. A boolean match needs no capture groups and no leftmost-longest
/// rule, so greedy vs lazy is irrelevant and quantifiers carry no order flag.
enum ReNode {
    Empty,
    Char(char),
    /// `.` — any single character (sqlite's `.` matches newline too).
    Any,
    Class(ClassSpec),
    /// A Perl class and whether it is the positive (`\d`) or negated (`\D`) form.
    Perl(Perl, bool),
    /// `^` — assert absolute start of string.
    Bol,
    /// `$` — assert absolute end of string.
    Eol,
    /// `\b` — assert a word boundary.
    Boundary,
    Concat(Vec<ReNode>),
    Alt(Vec<ReNode>),
    Star(Box<ReNode>),
    Plus(Box<ReNode>),
    Quest(Box<ReNode>),
    /// `{p}` / `{p,}` / `{p,q}` — `max` is `None` for the open-ended `{p,}`.
    Repeat(Box<ReNode>, usize, Option<usize>),
}

/// Pike-VM bytecode. `Char`/`Any`/`Class`/`Perl` consume one character; the rest
/// are zero-width (epsilon or a position assertion).
enum ReOp {
    Char(char),
    Any,
    Class(usize),
    Perl(Perl, bool),
    Bol,
    Eol,
    Boundary,
    /// Epsilon fork: a thread proceeds down BOTH targets.
    Split(usize, usize),
    /// Epsilon jump.
    Jmp(usize),
    /// Accept — a match has completed.
    Accept,
}

/// A compiled regex program.
struct ReProg {
    ops: Vec<ReOp>,
    classes: Vec<ClassSpec>,
}

/// Guards against a pathological pattern building an unbounded program (deeply
/// nested groups, or a huge `{m,n}` count). Far above any real pattern; a
/// pattern that would exceed it is treated as non-matching, which is also
/// roughly what sqlite does with very large counts.
const RE_MAX_OPS: usize = 100_000;
const RE_MAX_DEPTH: usize = 200;

// ---- parser: pattern text -> ReNode AST -----------------------------------

struct ReParser {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn parse_alt(&mut self) -> Option<ReNode> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            branches.pop()
        } else {
            Some(ReNode::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Option<ReNode> {
        let mut pieces = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                // A quantifier with nothing to bind to: sqlite's "'*' without
                // operand" / "unmatched '{'". Refuse (→ non-matching).
                Some('*') | Some('+') | Some('?') | Some('{') => return None,
                _ => {
                    let atom = self.parse_atom()?;
                    pieces.push(self.parse_quantifiers(atom)?);
                }
            }
        }
        match pieces.len() {
            0 => Some(ReNode::Empty),
            1 => pieces.pop(),
            _ => Some(ReNode::Concat(pieces)),
        }
    }

    fn parse_atom(&mut self) -> Option<ReNode> {
        let c = self.peek()?;
        match c {
            '(' => {
                self.pos += 1;
                self.depth += 1;
                if self.depth > RE_MAX_DEPTH {
                    return None;
                }
                let inner = self.parse_alt()?;
                self.depth -= 1;
                if self.peek() != Some(')') {
                    return None; // unmatched '('
                }
                self.pos += 1;
                Some(inner)
            }
            '[' => Some(ReNode::Class(self.parse_class()?)),
            '.' => {
                self.pos += 1;
                Some(ReNode::Any)
            }
            '^' => {
                self.pos += 1;
                Some(ReNode::Bol)
            }
            '$' => {
                self.pos += 1;
                Some(ReNode::Eol)
            }
            '\\' => {
                self.pos += 1;
                self.parse_escape()
            }
            // `]` and `}` are ordinary literals outside their constructs.
            _ => {
                self.pos += 1;
                Some(ReNode::Char(c))
            }
        }
    }

    /// Apply any run of postfix quantifiers to `atom`. sqlite allows stacking
    /// (`a**`), but rejects a quantifier applied to a `^`/`$` anchor.
    fn parse_quantifiers(&mut self, mut atom: ReNode) -> Option<ReNode> {
        loop {
            let quant = matches!(self.peek(), Some('*') | Some('+') | Some('?') | Some('{'));
            if !quant {
                return Some(atom);
            }
            if matches!(atom, ReNode::Bol | ReNode::Eol) {
                return None; // quantifier "without operand" (anchor is zero-width)
            }
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    atom = ReNode::Star(Box::new(atom));
                }
                Some('+') => {
                    self.pos += 1;
                    atom = ReNode::Plus(Box::new(atom));
                }
                Some('?') => {
                    self.pos += 1;
                    atom = ReNode::Quest(Box::new(atom));
                }
                Some('{') => {
                    let (min, max) = self.parse_count()?;
                    atom = ReNode::Repeat(Box::new(atom), min, max);
                }
                _ => unreachable!(),
            }
        }
    }

    /// `{p}` / `{p,}` / `{p,q}` / `{,q}` at the current `{`. Returns
    /// `(min, max)`. Rejects `{0}`/`{0,0}` (both zero) and `{m,n}` with n<m,
    /// exactly as sqlite does; a `{` that is not a well-formed count is an
    /// "unmatched '{'" (→ non-matching).
    fn parse_count(&mut self) -> Option<(usize, Option<usize>)> {
        self.pos += 1; // consume '{'
        let (min, had_min) = self.parse_digits();
        // A count that would expand past the program cap cannot yield a valid
        // program anyway; refuse it here so the expansion loop stays bounded (a
        // huge count over an empty body would otherwise spin without emitting).
        if min > RE_MAX_OPS {
            return None;
        }
        match self.peek() {
            Some('}') => {
                self.pos += 1;
                if !had_min || min == 0 {
                    return None; // `{}` / `{0}`
                }
                Some((min, Some(min)))
            }
            Some(',') => {
                self.pos += 1;
                let (max, had_max) = self.parse_digits();
                if self.peek() != Some('}') {
                    return None;
                }
                self.pos += 1;
                if had_max {
                    if max == 0 || max < min || max > RE_MAX_OPS {
                        return None; // both-zero, n<m, or over the cap
                    }
                    Some((min, Some(max)))
                } else {
                    Some((min, None)) // `{p,}`
                }
            }
            _ => None, // e.g. `{2b}`
        }
    }

    /// Read a run of decimal digits, saturating so a giant literal cannot
    /// overflow (`RE_MAX_OPS` catches the resulting expansion). Returns the
    /// value and whether any digit was present.
    fn parse_digits(&mut self) -> (usize, bool) {
        let mut val: usize = 0;
        let mut any = false;
        while let Some(c) = self.peek() {
            let Some(d) = c.to_digit(10) else { break };
            any = true;
            val = val.saturating_mul(10).saturating_add(d as usize);
            self.pos += 1;
        }
        (val, any)
    }

    /// A `\` escape at the top level (past the backslash).
    fn parse_escape(&mut self) -> Option<ReNode> {
        let Some(c) = self.peek() else {
            // Trailing backslash: sqlite treats `a\` as matching `a` — the
            // dangling escape contributes nothing.
            return Some(ReNode::Empty);
        };
        self.pos += 1;
        Some(match c {
            'd' => ReNode::Perl(Perl::Digit, true),
            'D' => ReNode::Perl(Perl::Digit, false),
            'w' => ReNode::Perl(Perl::Word, true),
            'W' => ReNode::Perl(Perl::Word, false),
            's' => ReNode::Perl(Perl::Space, true),
            'S' => ReNode::Perl(Perl::Space, false),
            'b' => ReNode::Boundary,
            'a' => ReNode::Char('\u{07}'),
            'f' => ReNode::Char('\u{0C}'),
            'n' => ReNode::Char('\n'),
            'r' => ReNode::Char('\r'),
            't' => ReNode::Char('\t'),
            'v' => ReNode::Char('\u{0B}'),
            'u' => ReNode::Char(self.parse_hex(4)?),
            'x' => ReNode::Char(self.parse_hex(2)?),
            '\\' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?' | '.' | '^' | '$' | '|' => {
                ReNode::Char(c)
            }
            _ => return None, // unknown escape
        })
    }

    /// Exactly `k` hex digits → a code point (`\uXXXX`, `\xXX`).
    fn parse_hex(&mut self, k: usize) -> Option<char> {
        let mut val: u32 = 0;
        for _ in 0..k {
            let d = self.peek()?.to_digit(16)?;
            val = val * 16 + d;
            self.pos += 1;
        }
        char::from_u32(val)
    }

    /// A `[...]` class at the current `[`. Mirrors sqlite: a leading `]` (right
    /// after `[` or `[^`) is a literal member; a `-` is a range operator except
    /// as the first member; the `\` escapes are the same as the top level MINUS
    /// the Perl classes (`[\d]` is an error there); an unclosed class fails.
    fn parse_class(&mut self) -> Option<ClassSpec> {
        self.pos += 1; // consume '['
        let negate = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self.peek()?; // None → unterminated class
            if c == ']' && !first {
                self.pos += 1;
                return Some(ClassSpec { negate, items });
            }
            let lo = self.class_member()?;
            first = false;
            // A `-` here starts a range (consuming the next member as its upper
            // bound) — even if that member is `]`, which is exactly why sqlite's
            // `[a-]` is "unterminated" rather than a trailing-dash literal.
            if self.peek() == Some('-') {
                self.pos += 1;
                let hi = self.class_member()?;
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Char(lo));
            }
        }
    }

    /// One literal member of a class (a char or a class `\` escape).
    fn class_member(&mut self) -> Option<char> {
        let c = self.peek()?;
        if c == '\\' {
            self.pos += 1;
            self.class_escape()
        } else {
            self.pos += 1;
            Some(c)
        }
    }

    /// A `\` escape inside a class (past the backslash): the C escapes,
    /// `\uXXXX`/`\xXX`, and escaped metacharacters — but NOT the Perl classes
    /// (`\d`), which sqlite rejects inside `[...]`.
    fn class_escape(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(match c {
            'a' => '\u{07}',
            'f' => '\u{0C}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{0B}',
            'u' => self.parse_hex(4)?,
            'x' => self.parse_hex(2)?,
            '\\' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?' | '.' | '^' | '$' | '|' => c,
            _ => return None,
        })
    }
}

// ---- compiler: ReNode AST -> ReOp program ---------------------------------

struct ReCompiler {
    ops: Vec<ReOp>,
    classes: Vec<ClassSpec>,
}

impl ReCompiler {
    fn emit(&mut self, op: ReOp) -> Option<usize> {
        if self.ops.len() >= RE_MAX_OPS {
            return None;
        }
        let i = self.ops.len();
        self.ops.push(op);
        Some(i)
    }

    fn compile(&mut self, node: &ReNode) -> Option<()> {
        match node {
            ReNode::Empty => {}
            ReNode::Char(c) => {
                self.emit(ReOp::Char(*c))?;
            }
            ReNode::Any => {
                self.emit(ReOp::Any)?;
            }
            ReNode::Class(spec) => {
                let idx = self.classes.len();
                self.classes.push(spec.clone());
                self.emit(ReOp::Class(idx))?;
            }
            ReNode::Perl(k, positive) => {
                self.emit(ReOp::Perl(*k, *positive))?;
            }
            ReNode::Bol => {
                self.emit(ReOp::Bol)?;
            }
            ReNode::Eol => {
                self.emit(ReOp::Eol)?;
            }
            ReNode::Boundary => {
                self.emit(ReOp::Boundary)?;
            }
            ReNode::Concat(v) => {
                for n in v {
                    self.compile(n)?;
                }
            }
            ReNode::Alt(v) => {
                let mut jmps = Vec::new();
                for (i, branch) in v.iter().enumerate() {
                    if i + 1 < v.len() {
                        let split = self.emit(ReOp::Split(0, 0))?;
                        let start = self.ops.len();
                        self.compile(branch)?;
                        jmps.push(self.emit(ReOp::Jmp(0))?);
                        let next = self.ops.len();
                        self.ops[split] = ReOp::Split(start, next);
                    } else {
                        self.compile(branch)?;
                    }
                }
                let end = self.ops.len();
                for j in jmps {
                    self.ops[j] = ReOp::Jmp(end);
                }
            }
            ReNode::Star(child) => {
                let l1 = self.emit(ReOp::Split(0, 0))?;
                let body = self.ops.len();
                self.compile(child)?;
                self.emit(ReOp::Jmp(l1))?;
                let end = self.ops.len();
                self.ops[l1] = ReOp::Split(body, end);
            }
            ReNode::Plus(child) => {
                let body = self.ops.len();
                self.compile(child)?;
                let split = self.emit(ReOp::Split(0, 0))?;
                let end = self.ops.len();
                self.ops[split] = ReOp::Split(body, end);
            }
            ReNode::Quest(child) => {
                let split = self.emit(ReOp::Split(0, 0))?;
                let body = self.ops.len();
                self.compile(child)?;
                let end = self.ops.len();
                self.ops[split] = ReOp::Split(body, end);
            }
            ReNode::Repeat(child, min, max) => {
                for _ in 0..*min {
                    self.compile(child)?;
                }
                match max {
                    None => self.compile(&ReNode::Star(clone_node(child)))?,
                    Some(q) => {
                        for _ in *min..*q {
                            self.compile(&ReNode::Quest(clone_node(child)))?;
                        }
                    }
                }
            }
        }
        Some(())
    }
}

/// Deep-clone a node so `{p,q}` can re-emit its body. Only reached for repeat
/// expansion, so it stays out of the hot compile path.
fn clone_node(n: &ReNode) -> Box<ReNode> {
    Box::new(match n {
        ReNode::Empty => ReNode::Empty,
        ReNode::Char(c) => ReNode::Char(*c),
        ReNode::Any => ReNode::Any,
        ReNode::Class(s) => ReNode::Class(s.clone()),
        ReNode::Perl(k, p) => ReNode::Perl(*k, *p),
        ReNode::Bol => ReNode::Bol,
        ReNode::Eol => ReNode::Eol,
        ReNode::Boundary => ReNode::Boundary,
        ReNode::Concat(v) => ReNode::Concat(v.iter().map(|c| *clone_node(c)).collect()),
        ReNode::Alt(v) => ReNode::Alt(v.iter().map(|c| *clone_node(c)).collect()),
        ReNode::Star(c) => ReNode::Star(clone_node(c)),
        ReNode::Plus(c) => ReNode::Plus(clone_node(c)),
        ReNode::Quest(c) => ReNode::Quest(clone_node(c)),
        ReNode::Repeat(c, lo, hi) => ReNode::Repeat(clone_node(c), *lo, *hi),
    })
}

// ---- Thompson-NFA simulation ----------------------------------------------

impl ReProg {
    fn compile(pattern: &str) -> Option<ReProg> {
        let mut p = ReParser {
            chars: pattern.chars().collect(),
            pos: 0,
            depth: 0,
        };
        let ast = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return None; // leftover input, e.g. an unmatched ')'
        }
        let mut c = ReCompiler {
            ops: Vec::new(),
            classes: Vec::new(),
        };
        c.compile(&ast)?;
        c.emit(ReOp::Accept)?;
        Some(ReProg {
            ops: c.ops,
            classes: c.classes,
        })
    }

    /// A word boundary sits where the "is a word char" status changes across the
    /// gap at `pos` (out-of-range on either side counts as non-word).
    fn is_boundary(chars: &[char], pos: usize, n: usize) -> bool {
        let left = pos > 0 && is_word_char(chars[pos - 1]);
        let right = pos < n && is_word_char(chars[pos]);
        left != right
    }

    /// Add a thread and its entire epsilon-closure to `list`, resolving the
    /// zero-width ops (`^`/`$`/`\b`) against `pos`. Iterative (an explicit stack,
    /// not recursion) so a large program cannot overflow the machine stack.
    /// Returns `true` the moment `Accept` is reachable.
    #[allow(clippy::too_many_arguments)]
    fn add_thread(
        &self,
        start: usize,
        pos: usize,
        n: usize,
        chars: &[char],
        list: &mut Vec<usize>,
        seen: &mut [u32],
        gen: u32,
        stack: &mut Vec<usize>,
    ) -> bool {
        stack.clear();
        stack.push(start);
        while let Some(pc) = stack.pop() {
            if seen[pc] == gen {
                continue;
            }
            seen[pc] = gen;
            match &self.ops[pc] {
                ReOp::Jmp(t) => stack.push(*t),
                ReOp::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                ReOp::Bol => {
                    if pos == 0 {
                        stack.push(pc + 1);
                    }
                }
                ReOp::Eol => {
                    if pos == n {
                        stack.push(pc + 1);
                    }
                }
                ReOp::Boundary => {
                    if Self::is_boundary(chars, pos, n) {
                        stack.push(pc + 1);
                    }
                }
                ReOp::Accept => return true,
                // A consuming op: park it to be tried against the current char.
                _ => list.push(pc),
            }
        }
        false
    }

    fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let nops = self.ops.len();

        let mut clist: Vec<usize> = Vec::new();
        let mut cseen: Vec<u32> = vec![0; nops];
        let mut nlist: Vec<usize> = Vec::new();
        let mut nseen: Vec<u32> = vec![0; nops];
        let mut stack: Vec<usize> = Vec::new();
        let mut gen: u32 = 0;

        // Seed a thread at position 0. This also settles the empty-string case
        // (`^$`, or an empty pattern) via the closure's `Accept` check.
        gen += 1;
        if self.add_thread(0, 0, n, &chars, &mut clist, &mut cseen, gen, &mut stack) {
            return true;
        }

        for pos in 0..n {
            let c = chars[pos];
            gen += 1;
            nlist.clear();
            for &pc in &clist {
                let consume = match &self.ops[pc] {
                    ReOp::Char(ch) => *ch == c,
                    ReOp::Any => true,
                    ReOp::Class(ci) => self.classes[*ci].matches(c),
                    ReOp::Perl(k, positive) => perl_test(*k, c) == *positive,
                    _ => false,
                };
                if consume
                    && self.add_thread(
                        pc + 1,
                        pos + 1,
                        n,
                        &chars,
                        &mut nlist,
                        &mut nseen,
                        gen,
                        &mut stack,
                    )
                {
                    return true;
                }
            }
            // Unanchored: a fresh match may also START at the next position.
            if self.add_thread(0, pos + 1, n, &chars, &mut nlist, &mut nseen, gen, &mut stack) {
                return true;
            }
            std::mem::swap(&mut clist, &mut nlist);
            std::mem::swap(&mut cseen, &mut nseen);
        }
        false
    }
}
