//! SQL `IN` three-valued-logic core and the LIKE / GLOB matchers.

use super::*;

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

/// SQL LIKE: `%` matches any run, `_` matches one char. Iterative
/// two-pointer algorithm — O(n·m) worst case, no recursion, no regex dep.
pub(super) fn like_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);
    while ti < t.len() {
        // The wildcard branch MUST precede the literal branch: a literal '%'
        // in the SUBJECT would otherwise consume the pattern's '%' as a
        // one-character match ('a%c' LIKE 'a%' must be TRUE).
        if pi < p.len() && p[pi] == '%' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
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
    while pi < p.len() && p[pi] == '%' {
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
