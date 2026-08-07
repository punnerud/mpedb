//! What a divergence IS — the same treatment the refusal list already gets.
//!
//! `refused` has had a ranked cause list for several rounds, and it has
//! rewritten the roadmap twice. `diverged` had one number: 3 074. That is the
//! column that means something is WRONG — a refusal is a missing feature, a
//! divergence is a wrong answer — and it was the one nobody could look inside.
//!
//! The failure mode this module exists to prevent is written down five times in
//! COMPAT-PG.md: **a collapsed line's example is not its description.** Every
//! time an aggregate carried one example, the example pointed the wrong way —
//! `unknown function` looked like one gap and was 404 names, `unexpected
//! trailing input` looked like `CASCADE` and was `PARTITION`. There is no
//! reason the divergence column would be different, and one strong reason to
//! expect it is worse: nobody had even picked an example from it.
//!
//! # The rule this module must not break
//!
//! **Classifying never changes a verdict.** `compare()` decides Same /
//! OrderOnly / Different; this runs strictly AFTER that decision and only
//! describes. The temptation is real and would be fatal — a classifier that
//! recognises "trailing spaces" is two lines away from a comparator that
//! forgives them, and forgiving them turns 51 wrong answers into 51 silent
//! ones. `character(20)` padding is a genuine difference in what the two
//! engines store; naming it is progress, hiding it is not.
//!
//! There is a test for exactly that: every classifier input is a pair the
//! comparator already rejected, and no code path here returns a verdict.

/// One divergence, reduced to its shape and one member.
pub struct Shape {
    /// The grouping key — no values in it, so members collapse.
    pub key: String,
    /// One member, kept short. A reader needs to see A case; the key says what
    /// the case is an instance OF.
    pub example: String,
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

/// PostgreSQL refused, mpedb answered.
///
/// Its own shape because it is the direction nobody looks for: every other
/// divergence is mpedb being wrong about a question it took, and this one is
/// mpedb taking a question PostgreSQL declined. A `CHECK` mpedb does not
/// enforce and an out-of-range value mpedb accepts both land here.
/// `why` is PostgreSQL's own error message, SHAPED (quoted text and digit runs
/// removed). It is the grouping key, and it has to be: mpedb produced an
/// ANSWER, so nothing on mpedb's side says what was wrong with the question.
/// Without it this is one line of a thousand whose example — `0` — describes
/// nothing, which is the failure this module exists to stop.
pub fn pg_refused_mpedb_answered(why: Option<&str>, mp: &str) -> Shape {
    let Some(why) = why else {
        return Shape {
            key: "mpedb ANSWERED what PostgreSQL refused (no PostgreSQL message)".into(),
            example: short(mp.lines().next().unwrap_or(""), 90),
        };
    };
    Shape {
        key: format!("mpedb ANSWERED, PostgreSQL: {}", short(why, 78)),
        example: format!("mpedb said `{}`", short(mp.lines().next().unwrap_or(""), 60)),
    }
}

/// One transcript ran short — an engine stopped answering mid-file.
pub fn transcript_ended_early(which: &str) -> Shape {
    Shape {
        key: format!("transcript ended early ({which} stopped answering)"),
        example: String::new(),
    }
}

/// Classify a row-set difference the comparator has already called Different.
///
/// `ordered` is whether the statement carried a top-level `ORDER BY`, and it
/// is load-bearing rather than decorative: with one, a same-multiset different-
/// sequence result is mpedb returning the right rows in the WRONG ORDER, which
/// is a bug in the planner; without one it never reaches this function at all
/// (the comparator calls it OrderOnly). That class was completely invisible
/// before — it was inside the 3 074 with everything else.
pub fn classify(la: &[&str], lb: &[&str], ordered: bool) -> Shape {
    if la.len() != lb.len() {
        // The two ends are worth their own keys. "mpedb returned NOTHING" is a
        // different investigation from "mpedb returned 3 rows where
        // PostgreSQL returned 4" — one is a feature that silently no-ops, the
        // other is a predicate or a join.
        let key = match (la.len(), lb.len()) {
            (_, 0) => "row count: mpedb returned NO rows".to_string(),
            (0, _) => "row count: PostgreSQL returned no rows, mpedb returned some".to_string(),
            (a, b) if b < a => "row count: mpedb returned FEWER rows".to_string(),
            _ => "row count: mpedb returned MORE rows".to_string(),
        };
        return Shape {
            key,
            example: format!("pg {} rows, mpedb {} rows", la.len(), lb.len()),
        };
    }

    let mut sa = la.to_vec();
    let mut sb = lb.to_vec();
    sa.sort_unstable();
    sb.sort_unstable();
    if sa == sb {
        // Same multiset, different sequence, and the statement ASKED for a
        // sequence — otherwise the comparator would have said OrderOnly and we
        // would not be here.
        debug_assert!(ordered, "an unordered same-multiset pair is OrderOnly, not Different");
        return Shape {
            key: "row ORDER differs under an explicit ORDER BY".into(),
            example: format!(
                "pg first row `{}`, mpedb first row `{}`",
                short(la.first().copied().unwrap_or(""), 40),
                short(lb.first().copied().unwrap_or(""), 40)
            ),
        };
    }

    // Compare SORTED, so a pure offset does not report every row as different.
    // The pair we want is the first row that has no partner at all.
    let Some(i) = (0..sa.len()).find(|&i| sa[i] != sb[i]) else {
        return Shape {
            key: "different, but no differing row found".into(),
            example: String::new(),
        };
    };
    let (ra, rb) = (sa[i], sb[i]);
    let ca: Vec<&str> = ra.split('|').collect();
    let cb: Vec<&str> = rb.split('|').collect();
    if ca.len() != cb.len() {
        // FIELDS, not columns, and the wording is the caveat: psql's `-A -F'|'`
        // does not escape a `|` INSIDE a value, so a single text column
        // containing one splits into two here. The first run's example for
        // this line was `'1' | '2'` vs `1` — one column of text against one
        // column, reported as 2 vs 1. Naming it "column count" claimed a
        // certainty the split cannot deliver.
        return Shape {
            key: format!(
                "field count after splitting on `|`: pg {}, mpedb {} \
                 (a value may contain the separator)",
                ca.len(),
                cb.len()
            ),
            example: format!("`{}` vs `{}`", short(ra, 45), short(rb, 45)),
        };
    }
    let Some(j) = (0..ca.len()).find(|&j| ca[j] != cb[j]) else {
        return Shape {
            key: "rows differ but every column matches".into(),
            example: format!("`{}` vs `{}`", short(ra, 45), short(rb, 45)),
        };
    };
    let mut s = cell(ca[j], cb[j]);
    s.example = format!("`{}` vs `{}`", short(ca[j], 40), short(cb[j], 40));
    s
}

/// One cell pair, named by HOW it differs.
///
/// The order of these tests is the claim: the more specific a description is,
/// the earlier it must run, or a general test swallows a specific one and the
/// ranked list loses exactly the item worth building. Trailing-space padding
/// before whitespace-in-general is the case that matters — `character(n)` is a
/// named, fixable gap and "whitespace differs" is not a work item.
fn cell(a: &str, b: &str) -> Shape {
    let key = if a == "NULL" || b == "NULL" {
        // Kept apart from the empty-string case below on purpose: a NULL where
        // a value belongs is an execution bug, and NULL-vs-'' is a protocol
        // question about how the two engines render an empty text value.
        if a.is_empty() || b.is_empty() {
            "NULL vs the empty string"
        } else if a == "NULL" {
            "PostgreSQL NULL, mpedb a value"
        } else {
            "mpedb NULL, PostgreSQL a value"
        }
    } else if a.trim_end() == b.trim_end() && a.trim_end() != a {
        // PostgreSQL padded and mpedb did not — `character(n)`. This is the
        // one COMPAT-PG.md already suspects; the list will now say how much of
        // the column it is instead of asserting it from one file.
        "trailing spaces: PostgreSQL PADS (character(n)), mpedb stores unpadded"
    } else if a.trim_end() == b.trim_end() {
        "trailing spaces: mpedb pads, PostgreSQL does not"
    } else if let (Ok(x), Ok(y)) = (a.trim().parse::<i128>(), b.trim().parse::<i128>()) {
        // INTEGERS are compared as integers, before the float path can see
        // them. `9223372036854775808` vs `9223372036854776000` are two
        // different numbers that parse to the SAME f64 — the first run
        // labelled that pair "same number, different rendering", which is a
        // classifier laundering an i64 overflow into a formatting note.
        if x == y {
            "same integer, different rendering"
        } else {
            "integers that are not equal"
        }
    } else if let (Ok(x), Ok(y)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        // NON-FINITE first, and never through the tolerance test. Rust parses
        // `infinity` and `nan`, and `(inf - 0).abs() <= inf * 1e-12` is TRUE —
        // so the first run reported `infinity` vs `0` as "agree to ~1e-12".
        // That is the worst thing this module can do: give a benign name to
        // the largest possible disagreement.
        if !x.is_finite() || !y.is_finite() {
            "one side is infinity or NaN, the other is not"
        } else if x == y {
            // `1.0` vs `1`, `1e10` vs `10000000000`. The VALUE agrees; the
            // rendering does not. Deliberately not normalised away in
            // `compare` — see the module docs — but worth its own line,
            // because the fix is a formatter and not an executor.
            "same number, different rendering"
        } else if (x - y).abs() <= (x.abs().max(y.abs())) * 1e-12 {
            "float precision: agree to ~1e-12, differ in the last digits"
        } else {
            "numbers that are not equal"
        }
    } else if a.eq_ignore_ascii_case(b) {
        "letter case"
    } else if a.split_whitespace().eq(b.split_whitespace()) {
        "internal whitespace"
    } else if a.is_empty() || b.is_empty() {
        "one side empty, the other not"
    } else {
        "text values differ"
    };
    Shape {
        key: key.to_string(),
        example: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_counts_split_by_direction_because_the_investigations_differ() {
        assert!(classify(&["a", "b"], &[], true).key.contains("NO rows"));
        assert!(classify(&["a", "b"], &["a"], true).key.contains("FEWER"));
        assert!(classify(&["a"], &["a", "b"], true).key.contains("MORE"));
        assert!(classify(&[], &["a"], true).key.contains("PostgreSQL returned no rows"));
    }

    #[test]
    fn order_under_an_explicit_order_by_is_its_own_class() {
        // Same multiset, different sequence. Without the ORDER BY the
        // comparator never sends this here; with one it is a planner bug and
        // was previously indistinguishable from a wrong VALUE.
        let s = classify(&["1", "2", "3"], &["3", "2", "1"], true);
        assert!(s.key.contains("ORDER"), "{}", s.key);
    }

    #[test]
    fn the_padding_hypothesis_gets_a_name_and_a_direction() {
        // `character(20)`: PostgreSQL pads, mpedb stores unpadded.
        let s = classify(&["abc                 |1"], &["abc|1"], true);
        assert!(s.key.contains("PADS"), "{}", s.key);
        assert!(s.key.contains("character(n)"), "{}", s.key);
        // …and the other direction is a different bug, so a different line.
        let s = classify(&["abc|1"], &["abc                 |1"], true);
        assert!(s.key.contains("mpedb pads"), "{}", s.key);
    }

    #[test]
    fn number_rendering_is_separated_from_number_disagreement() {
        assert!(classify(&["1.0"], &["1"], true).key.contains("different rendering"));
        assert!(classify(&["10000000000"], &["1e10"], true).key.contains("different rendering"));
        assert!(classify(&["1"], &["2"], true).key.contains("not equal"));
        // A specific test must not be swallowed by a general one: `1` vs `1.0`
        // is ALSO "text values differ", and that reading would hide a
        // formatter bug inside the corpus's largest catch-all.
        assert!(!classify(&["1.0"], &["1"], true).key.contains("text values"));
    }

    #[test]
    fn nulls_are_split_by_which_side_has_one() {
        assert!(classify(&["NULL"], &["0"], true).key.contains("PostgreSQL NULL"));
        assert!(classify(&["0"], &["NULL"], true).key.contains("mpedb NULL"));
        // A CELL, not a transcript: `""` on its own is zero ROWS, which the
        // row-count branch catches first and correctly. The empty-string case
        // needs a row that has one.
        assert!(classify(&["NULL|x"], &["|x"], true).key.contains("empty string"));
    }

    #[test]
    fn the_answered_bucket_groups_on_postgresqls_reason_because_mpedb_has_none() {
        // Two statements mpedb answered, refused by PostgreSQL for the SAME
        // reason, must land on ONE line — and one refused for a different
        // reason must not join them. Without PostgreSQL's message there is
        // nothing to group on at all: mpedb produced rows, and rows do not say
        // what was wrong with the question.
        let a = pg_refused_mpedb_answered(Some("value out of range for type integer"), "0");
        let b = pg_refused_mpedb_answered(Some("value out of range for type integer"), "7");
        let c = pg_refused_mpedb_answered(Some("division by zero"), "0");
        assert_eq!(a.key, b.key);
        assert_ne!(a.key, c.key);
        // The example still shows what mpedb said, which is the other half of
        // the finding.
        assert!(a.example.contains('0'), "{}", a.example);
        assert!(b.example.contains('7'), "{}", b.example);
        // Missing message is its own line rather than silently merged into a
        // real reason.
        assert!(pg_refused_mpedb_answered(None, "0")
            .key
            .contains("no PostgreSQL message"));
    }

    #[test]
    fn field_count_is_reported_before_any_cell_is_read_and_does_not_overclaim() {
        let s = classify(&["a|b|c"], &["a|b"], true);
        assert!(s.key.contains("field count"), "{}", s.key);
        // Not "column": the separator is unescaped in psql's output, so this
        // count is a split and says so.
        assert!(!s.key.contains("column count"), "{}", s.key);
    }

    /// The two labels that LAUNDERED a real difference in the first run.
    ///
    /// Both were the same mistake in opposite corners of the numeric tower: a
    /// classifier is allowed to be coarse, and is not allowed to give the
    /// largest possible disagreement a reassuring name.
    #[test]
    fn nothing_non_finite_or_out_of_f64_range_gets_a_benign_label() {
        // `(inf - 0).abs() <= inf * 1e-12` is TRUE. The tolerance test must
        // never see a non-finite value.
        let s = classify(&["infinity"], &["0"], true);
        assert!(s.key.contains("infinity or NaN"), "{}", s.key);
        assert!(!s.key.contains("agree"), "{}", s.key);
        assert!(classify(&["NaN"], &["1"], true).key.contains("infinity or NaN"));

        // Two DIFFERENT integers that share one f64.
        let s = classify(&["9223372036854775808"], &["9223372036854776000"], true);
        assert!(s.key.contains("not equal"), "{}", s.key);
        assert!(!s.key.contains("same number"), "{}", s.key);

        // …while genuine integer re-rendering still gets its own line.
        assert!(classify(&["007"], &["7"], true).key.contains("same integer"));
        // and the float path still works for values that are actually floats.
        assert!(classify(&["1.50"], &["1.5"], true).key.contains("same number"));
    }

    #[test]
    fn a_key_carries_no_values_or_it_would_not_group() {
        // The whole point of a shape is that two members share a key. If a
        // value leaked into one, the ranked list would be one line per
        // statement — which is what the raw transcript already is.
        let a = classify(&["alpha                |1"], &["alpha|1"], true);
        let b = classify(&["omega    |9"], &["omega|9"], true);
        assert_eq!(a.key, b.key);
        assert_ne!(a.example, b.example);
    }

    #[test]
    fn classifying_returns_no_verdict() {
        // Stated as a test because the module docs say it is the one rule that
        // must not break: every function here returns a Shape, and a Shape has
        // no way to say "actually these agree". If someone adds a verdict to
        // this module, this test stops compiling — which is the point.
        let s: Shape = classify(&["a"], &["b"], true);
        let _: String = s.key;
    }
}
