//! The corpus baseline — a checked-in expected-counts file, and the diff that
//! turns a silently shifting category into an EXIT CODE.
//!
//! # Why this exists
//!
//! Until now the corpus baseline was carried three ways, all of them human: a
//! table in `design/CORPUS-STATUS.md`, a restated number in each confirming
//! commit message, and log artifacts outside the repo. "Byte-identical against
//! baseline" meant an operator diffing a TOTAL line by eye.
//!
//! That catches a number moving. It does not catch a number staying still while
//! its MEANING moves — which is exactly what happened: `REINDEX <name>` became
//! an accepted no-op, so one record left the `index-ddl` refusal bucket and
//! arrived in the error-mismatch bucket, and the headline "zero error
//! mismatches" went on being quoted. Nothing failed. Nobody was reading that
//! column.
//!
//! So the baseline is per-file and per-CLASS, and any movement — including an
//! improvement — is a nonzero exit. An unrecorded improvement is not harmless:
//! it means the file no longer describes the tree, and the next real regression
//! gets measured against a number that was already wrong.
//!
//! # The key is the path, not the file name — and the ROOT is recorded
//!
//! 127 of the corpus's 622 files share a basename (`slt_good_0.test` lives in
//! `index/between/1/`, `index/delete/1/`, and eleven other directories). Keying
//! on the name would silently merge them — and it is why the report's own
//! per-file table has 127 ambiguous row labels today. So the key is the path
//! relative to the corpus root, which is also stable across machines (the M3
//! keeps its corpus somewhere else).
//!
//! The root is **written into the baseline** rather than inferred per run. The
//! first version of this file inferred it as the longest common prefix of
//! whatever paths the run was given, which is correct for a full run and wrong
//! for every subset: two files under `evidence/` infer the root as
//! `…/test/evidence`, key themselves `slt_lang_reindex.test`, and match nothing
//! in a baseline keyed `evidence/slt_lang_reindex.test`. The 125-file
//! every-fifth arm is the main consumer of this gate, so that bug made the gate
//! useless exactly where it is used. Recorded root, re-keyed run, exact match.

use std::collections::BTreeMap;
use std::path::Path;

/// One file's counts. Everything the diff compares, and nothing else — a
/// baseline that recorded timings would fail on a busy machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub total: usize,
    pub pass: usize,
    pub unsupported: usize,
    pub wrong: usize,
    pub errmis: usize,
    pub skipped: usize,
    /// A file that could not run at all. Recorded so that "it FATALs" is a
    /// stated expectation rather than a zero row that looks like a clean pass.
    pub fatal: bool,
}

pub type Table = BTreeMap<String, Row>;

/// A baseline: the recorded corpus root plus one row per file.
#[derive(Debug)]
pub struct Baseline {
    /// The directory the keys are relative to, as an absolute path. Recorded so
    /// a SUBSET run keys itself the same way a full run did.
    pub root: String,
    pub files: Table,
}

/// Infer the corpus root as the longest common DIRECTORY prefix of the paths.
///
/// Only used when writing a baseline (or when none is being compared). Reading
/// one uses its recorded root instead — see the module docs for why inferring
/// per run is wrong for subsets.
pub fn infer_root(paths: &[String]) -> String {
    let split: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split('/').filter(|s| !s.is_empty()).collect())
        .collect();
    let leading_slash = paths.first().map(|p| p.starts_with('/')).unwrap_or(false);
    // The prefix stops before the last component: a file name is not a directory.
    let bound = split.iter().map(|c| c.len().saturating_sub(1)).min().unwrap_or(0);
    let mut common = 0;
    while common < bound {
        let seg = split[0][common];
        if split.iter().all(|c| c[common] == seg) {
            common += 1;
        } else {
            break;
        }
    }
    let joined = split.first().map(|c| c[..common].join("/")).unwrap_or_default();
    if leading_slash { format!("/{joined}") } else { joined }
}

/// Key each path relative to `root`.
///
/// A path outside the root is a named error rather than a silently odd key: it
/// means the run and the baseline are describing different corpora, and every
/// comparison after that would be meaningless.
pub fn keys_for(paths: &[String], root: &str) -> Result<Vec<String>, String> {
    let root = root.trim_end_matches('/');
    paths
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .map(|r| r.trim_start_matches('/').to_string())
                .filter(|r| !r.is_empty())
                .ok_or_else(|| format!("{p} is not under the corpus root {root}"))
        })
        .collect()
}

pub fn write(path: &Path, base: &Baseline) -> std::io::Result<()> {
    let mut out = String::new();
    out.push_str("# mpedb sqllogictest corpus baseline.\n");
    out.push_str("# Regenerate deliberately with --write-baseline; never by hand.\n");
    out.push_str(&format!("# root {}\n", base.root));
    out.push_str("# key\ttotal\tpass\tunsupported\twrong\terrmis\tskipped\tfatal\n");
    for (k, r) in &base.files {
        out.push_str(&format!(
            "{k}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.total,
            r.pass,
            r.unsupported,
            r.wrong,
            r.errmis,
            r.skipped,
            u8::from(r.fatal)
        ));
    }
    std::fs::write(path, out)
}

pub fn read(path: &Path) -> Result<Baseline, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut table = Table::new();
    let mut root: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        if let Some(r) = line.strip_prefix("# root ") {
            root = Some(r.trim().to_string());
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 8 {
            return Err(format!(
                "{}:{}: expected 8 tab-separated fields, got {}",
                path.display(),
                i + 1,
                f.len()
            ));
        }
        let num = |j: usize| -> Result<usize, String> {
            f[j].parse::<usize>()
                .map_err(|e| format!("{}:{}: field {}: {e}", path.display(), i + 1, j + 1))
        };
        table.insert(
            f[0].to_string(),
            Row {
                total: num(1)?,
                pass: num(2)?,
                unsupported: num(3)?,
                wrong: num(4)?,
                errmis: num(5)?,
                skipped: num(6)?,
                fatal: num(7)? != 0,
            },
        );
    }
    let root = root.ok_or_else(|| {
        format!(
            "{}: no `# root <path>` line — a baseline without its root cannot key a subset run",
            path.display()
        )
    })?;
    Ok(Baseline { root, files: table })
}

/// What the comparison found. The three outcomes are deliberately distinct exit
/// codes: CI must fail on a regression, and a human must be told when the
/// baseline has stopped describing the tree even though everything got better.
pub enum Verdict {
    Match,
    /// Something got worse, or comparability was lost.
    Regression,
    /// Only improvements and/or new files — the baseline needs updating.
    Stale,
}

impl Verdict {
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::Match => 0,
            Verdict::Regression => 1,
            Verdict::Stale => 3,
        }
    }
}

/// Compare a run against a baseline and print every difference.
///
/// Files in the baseline but absent from this run are reported as a count, not
/// as failures: running a subset is a supported and frequent thing (the 125-file
/// every-fifth arm). Files in this run but absent from the baseline are STALE —
/// an unrecorded file is an unmeasured one.
pub fn compare(base: &Table, run: &Table) -> Verdict {
    let mut regressions: Vec<String> = Vec::new();
    let mut improvements: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();

    for (key, now) in run {
        let Some(was) = base.get(key) else {
            unknown.push(key.clone());
            continue;
        };
        if was == now {
            continue;
        }
        // Comparability first: if the record count moved, the corpus file or the
        // parser changed underneath us and every other column is comparing two
        // different things.
        if was.total != now.total {
            regressions.push(format!(
                "{key}: RECORD COUNT moved {} -> {} — the corpus file or the parser changed; \
                 the other columns are not comparable",
                was.total, now.total
            ));
            continue;
        }
        if was.fatal != now.fatal {
            let line = format!("{key}: fatal {} -> {}", was.fatal, now.fatal);
            if now.fatal { regressions.push(line) } else { improvements.push(line) }
        }
        // A skip is a test that did not run. More of them is a silent loss of
        // coverage — the same failure mode as Django's arm-asymmetric skips.
        delta(key, "skipped", was.skipped, now.skipped, true, &mut regressions, &mut improvements);
        delta(key, "pass", was.pass, now.pass, false, &mut regressions, &mut improvements);
        delta(key, "unsupported", was.unsupported, now.unsupported, true, &mut regressions, &mut improvements);
        delta(key, "wrong", was.wrong, now.wrong, true, &mut regressions, &mut improvements);
        delta(key, "errmis", was.errmis, now.errmis, true, &mut regressions, &mut improvements);
    }

    let not_run = base.keys().filter(|k| !run.contains_key(*k)).count();

    println!("\n== baseline ==");
    println!(
        "compared {} file(s) against {} in the baseline{}",
        run.len(),
        base.len(),
        if not_run > 0 { format!("; {not_run} not run (subset)") } else { String::new() }
    );

    if !regressions.is_empty() {
        println!("\n-- REGRESSIONS ({}) --", regressions.len());
        for r in &regressions {
            println!("  {r}");
        }
    }
    if !improvements.is_empty() {
        println!("\n-- improvements ({}) --", improvements.len());
        for r in &improvements {
            println!("  {r}");
        }
    }
    if !unknown.is_empty() {
        println!("\n-- not in the baseline ({}) --", unknown.len());
        for r in unknown.iter().take(20) {
            println!("  {r}");
        }
        if unknown.len() > 20 {
            println!("  … and {} more", unknown.len() - 20);
        }
    }

    if !regressions.is_empty() {
        println!("\nBASELINE: REGRESSION — exit 1");
        Verdict::Regression
    } else if !improvements.is_empty() || !unknown.is_empty() {
        println!(
            "\nBASELINE: STALE — everything moved the right way, but the file no longer \
             describes the tree. Re-run with --write-baseline. Exit 3"
        );
        Verdict::Stale
    } else {
        println!("\nBASELINE: exact match — exit 0");
        Verdict::Match
    }
}

/// One column's movement. `up_is_bad` says which direction is a regression:
/// `pass` wants to go up, everything else wants to go down.
fn delta(
    key: &str,
    col: &str,
    was: usize,
    now: usize,
    up_is_bad: bool,
    regressions: &mut Vec<String>,
    improvements: &mut Vec<String>,
) {
    if was == now {
        return;
    }
    let line = format!("{key}: {col} {was} -> {now}");
    if (now > was) == up_is_bad {
        regressions.push(line);
    } else {
        improvements.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pass: usize, wrong: usize) -> Row {
        Row { total: 100, pass, unsupported: 0, wrong, errmis: 0, skipped: 0, fatal: false }
    }

    fn full_run() -> Vec<String> {
        [
            "/home/m/sqllogictest/test/index/delete/1/slt_good_0.test",
            "/home/m/sqllogictest/test/index/between/1/slt_good_0.test",
            "/home/m/sqllogictest/test/evidence/slt_lang_reindex.test",
            "/home/m/sqllogictest/test/select1.test",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn keys_are_relative_to_the_root_and_keep_the_disambiguating_directory() {
        let paths = full_run();
        let root = infer_root(&paths);
        assert_eq!(root, "/home/m/sqllogictest/test");
        let keys = keys_for(&paths, &root).unwrap();
        // The two same-named files must stay distinct — this is the whole point.
        assert_eq!(keys[0], "index/delete/1/slt_good_0.test");
        assert_eq!(keys[1], "index/between/1/slt_good_0.test");
        assert_eq!(keys[3], "select1.test");
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn a_subset_run_keys_itself_exactly_as_the_full_run_did() {
        // THE regression. Inferring the root per run made a two-file subset
        // under `evidence/` key itself `slt_lang_reindex.test` while the full
        // run's baseline said `evidence/slt_lang_reindex.test` — so the subset
        // matched nothing and the gate silently reported everything as new.
        let full = full_run();
        let root = infer_root(&full);
        let full_keys = keys_for(&full, &root).unwrap();

        let subset = vec!["/home/m/sqllogictest/test/evidence/slt_lang_reindex.test".to_string()];
        // The subset would infer a DIFFERENT root; using the recorded one is
        // what makes it agree.
        assert_ne!(infer_root(&subset), root);
        let subset_keys = keys_for(&subset, &root).unwrap();
        assert_eq!(subset_keys[0], "evidence/slt_lang_reindex.test");
        assert_eq!(subset_keys[0], full_keys[2]);
    }

    #[test]
    fn a_path_outside_the_root_is_a_named_error_not_an_odd_key() {
        let e = keys_for(&["/somewhere/else/x.test".to_string()], "/home/m/sqllogictest/test")
            .unwrap_err();
        assert!(e.contains("not under the corpus root"), "got: {e}");
    }

    #[test]
    fn fewer_passes_is_a_regression_and_more_is_stale() {
        let mut base = Table::new();
        base.insert("a".into(), row(90, 0));

        let mut worse = Table::new();
        worse.insert("a".into(), row(89, 0));
        assert_eq!(compare(&base, &worse).exit_code(), 1);

        let mut better = Table::new();
        better.insert("a".into(), row(91, 0));
        assert_eq!(compare(&base, &better).exit_code(), 3);

        assert_eq!(compare(&base, &base.clone()).exit_code(), 0);
    }

    #[test]
    fn a_wrong_answer_appearing_is_a_regression_even_at_the_same_pass_count() {
        // The REINDEX shape: the pass count does not move, one record just
        // changes which failure bucket it is in. This is the case the eyeball
        // diff of a TOTAL line could not catch.
        let mut base = Table::new();
        base.insert("a".into(), Row { total: 100, pass: 90, unsupported: 10, wrong: 0, errmis: 0, skipped: 0, fatal: false });
        let mut run = Table::new();
        run.insert("a".into(), Row { total: 100, pass: 90, unsupported: 9, wrong: 0, errmis: 1, skipped: 0, fatal: false });
        assert_eq!(compare(&base, &run).exit_code(), 1);
    }

    #[test]
    fn a_moved_record_count_is_a_regression_because_comparability_is_gone() {
        let mut base = Table::new();
        base.insert("a".into(), row(90, 0));
        let mut run = Table::new();
        run.insert("a".into(), Row { total: 101, pass: 91, ..row(91, 0) });
        assert_eq!(compare(&base, &run).exit_code(), 1);
    }

    #[test]
    fn a_subset_run_does_not_fail_on_the_files_it_did_not_run() {
        let mut base = Table::new();
        base.insert("a".into(), row(90, 0));
        base.insert("b".into(), row(80, 0));
        let mut run = Table::new();
        run.insert("a".into(), row(90, 0));
        assert_eq!(compare(&base, &run).exit_code(), 0);
    }

    #[test]
    fn a_file_the_baseline_has_never_seen_is_stale_not_silent() {
        let base = Table::new();
        let mut run = Table::new();
        run.insert("a".into(), row(90, 0));
        assert_eq!(compare(&base, &run).exit_code(), 3);
    }

    #[test]
    fn more_skips_is_a_regression_because_a_skip_is_a_test_that_did_not_run() {
        let mut base = Table::new();
        base.insert("a".into(), Row { skipped: 1, ..row(90, 0) });
        let mut run = Table::new();
        run.insert("a".into(), Row { skipped: 2, ..row(90, 0) });
        assert_eq!(compare(&base, &run).exit_code(), 1);
    }

    #[test]
    fn round_trip_through_the_file_format_carries_the_root() {
        let dir = std::env::temp_dir().join(format!("mpedb-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("baseline.tsv");
        let mut t = Table::new();
        t.insert("index/delete/1/slt_good_0.test".into(), row(90, 2));
        t.insert("select1.test".into(), Row { fatal: true, ..row(0, 0) });
        let base = Baseline { root: "/home/m/sqllogictest/test".into(), files: t.clone() };
        write(&p, &base).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.root, "/home/m/sqllogictest/test");
        assert!(back.files == t, "round trip must be exact");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_truncated_line_is_a_named_error_not_a_zero_row() {
        let dir = std::env::temp_dir().join(format!("mpedb-baseline-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.tsv");
        std::fs::write(&p, "# root /r\na\t1\t2\n").unwrap();
        let e = read(&p).unwrap_err();
        assert!(e.contains("expected 8"), "got: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_baseline_without_a_root_is_refused_by_name() {
        // A rootless baseline cannot key a subset run, so accepting one would
        // reintroduce exactly the bug the root exists to prevent.
        let dir = std::env::temp_dir().join(format!("mpedb-baseline-noroot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("noroot.tsv");
        std::fs::write(&p, "a\t1\t1\t0\t0\t0\t0\t0\n").unwrap();
        let e = read(&p).unwrap_err();
        assert!(e.contains("no `# root"), "got: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
