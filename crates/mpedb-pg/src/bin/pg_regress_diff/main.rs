//! The PostgreSQL oracle: `src/test/regress`, run DIFFERENTIALLY.
//!
//! # The method, and why it is not pg_regress
//!
//! PostgreSQL ships ~215 `sql/*.sql` files with matching `expected/*.out`, and
//! `pg_regress` runs each and diffs against the checked-in output. Using those
//! `.out` files as the oracle would not work here: they contain PostgreSQL's own
//! error wording, its OID numbering, its `EXPLAIN` output and its internal
//! catalog contents, none of which mpedb can or should reproduce.
//!
//! So the `.out` files are not used at all. Each `.sql` file is sent through
//! **both** a throwaway PostgreSQL cluster and mpedb-pg, and the two transcripts
//! are diffed **against each other**. That is the same method
//! `mpedb-testkit`'s three-way differential already uses against sqlite, and it
//! answers the question that actually matters — *does mpedb say what PostgreSQL
//! says* — rather than *does mpedb reproduce a file*.
//!
//! # Scoring
//!
//! Per statement, not per file, because a file that dies on statement 3 would
//! otherwise score the same as one that dies on statement 300.
//!
//! - **match** — both engines answered, identically.
//! - **both-refused** — both engines errored. Counted as agreement: mpedb's
//!   contract is a named refusal, and refusing what PostgreSQL refuses IS the
//!   right answer. The messages are NOT compared; they never match.
//! - **refused** — PostgreSQL answered, mpedb refused by name. The honest gap.
//! - **DIVERGED** — both answered and the answers differ. This is the only
//!   category that means something is WRONG, and it is the one to drive to zero.
//!
//! A run prints the four counts per file and a total. `--baseline` compares
//! against a checked-in TSV and exits non-zero on ANY movement — improvements
//! included, exactly as `corpus-baseline.tsv` does, because a number that
//! drifts upward unnoticed is how a regression hides next to a win.
//!
//! # Getting the corpus
//!
//! Not vendored (it is PostgreSQL's, and it is large). Out of tree, like the
//! sqllogictest corpus:
//!
//! ```sh
//! apt-get source postgresql-16          # or the tarball from ftp.postgresql.org
//! export MPEDB_PG_REGRESS=$PWD/postgresql-16.14/src/test/regress
//! ```

mod divergence;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }
    let args = match relocate_to_scratch(args) {
        Ok(a) => a,
        Err(m) => {
            eprintln!("pg_regress_diff: {m}");
            std::process::exit(2);
        }
    };
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(m) => {
            eprintln!("pg_regress_diff: {m}");
            std::process::exit(2);
        }
    }
}

/// Move the process out of whatever directory it was launched from, into the
/// scratch base, and make every path argument absolute first.
///
/// The corpus writes files. `\g :g_out_file` in `psql`, `COPY … TO 'name'`, and
/// several `\o` redirections all name RELATIVE paths, and a relative path in
/// mpedb's arm resolves against this process's working directory — which is the
/// repository root when the harness is run the way its own docs show. A full
/// run left `:g_out_file` sitting in the checkout. An oracle that litters the
/// tree it is measuring is one `git add -A` away from committing corpus output
/// as source.
///
/// Every non-flag token in this CLI is a path (the sql files, and the values of
/// `--baseline` / `--write-baseline`), so making them absolute is exact rather
/// than a guess — as is `MPEDB_PG_REGRESS`, which is a corpus root.
///
/// The destination is `multifile::ephemeral_dir()` — the same `MPEDB_TEST_DIR`
/// → `/dev/shm` → `temp_dir()` chain the mpedb arm's own scratch files follow,
/// so one variable still moves the whole run off a full tmpfs.
fn relocate_to_scratch(args: Vec<String>) -> Result<Vec<String>, String> {
    let orig = std::env::current_dir().map_err(|e| format!("current_dir: {e}"))?;
    let abs = |v: &str| -> String {
        let p = Path::new(v);
        if p.is_absolute() {
            v.to_string()
        } else {
            orig.join(p).to_string_lossy().into_owned()
        }
    };
    if let Ok(root) = std::env::var("MPEDB_PG_REGRESS") {
        // SAFETY: single-threaded, before any thread is spawned.
        unsafe { std::env::set_var("MPEDB_PG_REGRESS", abs(&root)) };
    }
    let out: Vec<String> = args
        .iter()
        .map(|a| if a.starts_with("--") { a.clone() } else { abs(a) })
        .collect();

    let cwd = mpedb::ephemeral_dir().join(format!("pgregress-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(|e| format!("create {}: {e}", cwd.display()))?;
    std::env::set_current_dir(&cwd).map_err(|e| format!("chdir {}: {e}", cwd.display()))?;
    Ok(out)
}

const USAGE: &str = "\
usage: pg_regress_diff [--all | <file.sql>...] [--baseline <tsv>] [--write-baseline <tsv>]

  The corpus root comes from MPEDB_PG_REGRESS (a checkout's src/test/regress).
  PostgreSQL's binaries come from MPEDB_PG_BIN, or the usual install roots.

  --all              every sql/*.sql in the corpus
  --show             print both transcripts per statement
  --baseline F       compare per-file counts against F; ANY movement exits 1
  --write-baseline F rewrite F from this run (deliberate, never automatic)";

#[derive(Default, Clone, Copy, PartialEq)]
struct Counts {
    matched: u32,
    /// Same ROWS, different ORDER, on a statement with no top-level `ORDER BY`.
    ///
    /// Counted as agreement, and this is not generosity — it is the only
    /// defensible reading. SQL does not define the order of a result set
    /// without `ORDER BY`, so two engines returning the same multiset are BOTH
    /// right. Counting it as a divergence measures the scan order of two
    /// different storage engines, which is not a compatibility question.
    ///
    /// It is a separate column rather than folded into `matched` because it is
    /// the one number that would let this harness flatter itself, and a reader
    /// deserves to see how much of the agreement rests on it. sqlite's
    /// sqllogictest solves the same problem with an explicit `rowsort` marker
    /// per query; the PostgreSQL corpus has no such declaration, so the
    /// statement text has to be asked instead.
    order_only: u32,
    both_refused: u32,
    refused: u32,
    diverged: u32,
}

impl Counts {
    fn total(&self) -> u32 {
        self.matched + self.order_only + self.both_refused + self.refused + self.diverged
    }

    /// match + order-only + both-refused.
    fn agreed(&self) -> u32 {
        self.matched + self.order_only + self.both_refused
    }
}

fn run(args: &[String]) -> Result<i32, String> {
    let root = std::env::var("MPEDB_PG_REGRESS").map_err(|_| {
        "MPEDB_PG_REGRESS is not set — point it at a PostgreSQL checkout's \
         src/test/regress (see the module docs). The corpus is deliberately not \
         vendored."
            .to_string()
    })?;
    let root = PathBuf::from(root);
    if !root.join("sql").is_dir() {
        return Err(format!(
            "{} has no sql/ directory — MPEDB_PG_REGRESS should be src/test/regress",
            root.display()
        ));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut baseline: Option<PathBuf> = None;
    let mut write_baseline: Option<PathBuf> = None;
    let mut show = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--all" => {
                let mut all: Vec<PathBuf> = std::fs::read_dir(root.join("sql"))
                    .map_err(|e| format!("read sql/: {e}"))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "sql"))
                    .collect();
                all.sort();
                files.extend(all);
            }
            // Investigating a divergence without seeing both sides is guesswork,
            // and the counts alone cannot tell a real disagreement from a
            // harness artefact — which is exactly what the first run of this
            // tool turned out to be.
            "--show" => show = true,
            "--baseline" => baseline = it.next().map(PathBuf::from),
            "--write-baseline" => write_baseline = it.next().map(PathBuf::from),
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        return Err(USAGE.into());
    }

    // The real PostgreSQL arm. Absent PostgreSQL is a LOUD skip, never a silent
    // pass — the same rule `mpedb-testkit`'s `PgCluster` follows, and for the
    // same reason: an unmeasurable engine reported as measured is worse than a
    // failure.
    if which("psql").is_none() {
        return Err("psql not found — the differential needs a real PostgreSQL to \
                    compare against, and answering without one would be a claim \
                    rather than a measurement"
            .into());
    }

    let mut totals = Counts::default();
    let mut per_file: Vec<(String, Counts)> = Vec::new();
    let mut causes: std::collections::HashMap<String, Cause> = std::collections::HashMap::new();
    let mut diffs: std::collections::HashMap<String, Cause> = std::collections::HashMap::new();
    let mut cascade = Cascade::default();
    for f in &files {
        let name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let c = diff_file(f, show, &mut causes, &mut diffs, &mut cascade)?;
        println!(
            "{name:<24} match {:<6} order-only {:<6} both-refused {:<6} refused {:<6} DIVERGED {}",
            c.matched, c.order_only, c.both_refused, c.refused, c.diverged
        );
        totals.matched += c.matched;
        totals.order_only += c.order_only;
        totals.both_refused += c.both_refused;
        totals.refused += c.refused;
        totals.diverged += c.diverged;
        per_file.push((name, c));
    }

    let n = totals.total().max(1);
    println!(
        "\nTOTAL {} statements: {} match, {} order-only, {} both-refused, {} refused, \
         {} DIVERGED\n  agreement {:.1} %  (of which order-only {:.1} pp)   divergence {:.1} %",
        totals.total(),
        totals.matched,
        totals.order_only,
        totals.both_refused,
        totals.refused,
        totals.diverged,
        100.0 * f64::from(totals.agreed()) / f64::from(n),
        100.0 * f64::from(totals.order_only) / f64::from(n),
        100.0 * f64::from(totals.diverged) / f64::from(n),
    );

    if !diffs.is_empty() {
        let mut ranked: Vec<(&String, &Cause)> = diffs.iter().collect();
        ranked.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
        let shown = 20.min(ranked.len());
        // Printed FIRST, and above the refusal list, because it is the column
        // that means something is WRONG. A refusal is a feature mpedb does not
        // have; a divergence is an answer mpedb gets differently, and a reader
        // scrolling one screen should meet that one.
        println!(
            "\nWHERE mpedb ANSWERS DIFFERENTLY — top {shown} of {} shapes, ranked.\n\
             Both engines answered; the answers disagree. This is the only column \
             that means something is wrong.",
            ranked.len()
        );
        for (shape, c) in ranked.iter().take(shown) {
            println!("  {:>6}  {}", c.count, shape);
            if !c.example.trim().is_empty() {
                let ex: String = c.example.chars().take(110).collect();
                println!("          e.g. {ex}");
            }
            print_concentration(c);
        }
        let tail: u32 = ranked.iter().skip(shown).map(|(_, c)| c.count).sum();
        if tail > 0 {
            println!("  {tail:>6}  … in {} further shapes", ranked.len() - shown);
        }
        // The same rollup the refusal list gets, for the same reason: this
        // family is split by PostgreSQL's REASON, so its members scatter below
        // the cutoff and the family's size disappears with them.
        print_families(&ranked, &["mpedb ANSWERED, PostgreSQL: "]);
    }

    if !causes.is_empty() {
        let mut ranked: Vec<(&String, &Cause)> = causes.iter().collect();
        // Descending by count, then by shape so equal counts print stably.
        ranked.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
        let shown = 25.min(ranked.len());
        println!(
            "\nWHY mpedb REFUSED — top {shown} of {} distinct causes, ranked.\n\
             This is the work list: PostgreSQL answered these and mpedb did not.",
            ranked.len()
        );
        for (shape, c) in ranked.iter().take(shown) {
            let s: String = shape.chars().take(88).collect();
            println!("  {:>6}  [{}]  {}", c.count, c.code, s);
            // The SHAPE groups; the EXAMPLE is what makes it actionable. A row
            // reading "expected _" says the parser refused something and
            // nothing more — the example says WHICH token, which is the whole
            // difference between a work list and a shrug.
            let ex: String = c.example.chars().take(110).collect();
            if ex.trim() != s.trim() {
                println!("          e.g. {ex}");
            }
            print_concentration(c);
        }
        let tail: u32 = ranked.iter().skip(shown).map(|(_, c)| c.count).sum();
        if tail > 0 {
            println!("  {tail:>6}  … in {} further causes", ranked.len() - shown);
        }
        print_families(&ranked, &["unknown function `", "table function in FROM `"]);
    }

    // The `unknown table` split, printed whenever there is one to print.
    //
    // It answers the question the ranked list cannot: is that bucket work, or
    // is it the SHADOW of work already listed above it? A consequence is paid
    // for by fixing the CREATE; an independent one is its own job.
    let total = cascade.consequence + cascade.independent;
    if total > 0 {
        println!(
            "\nUNKNOWN TABLE, split: {} of {total} follow a CREATE this run already \
             refused IN THE SAME FILE ({:.0}%); {} do not.\nThe first group is not work — \
             it is the second-order cost of the refusals ranked above, and fixing those \
             removes it. The second group is the one to read.",
            cascade.consequence,
            100.0 * cascade.consequence as f64 / total as f64,
            cascade.independent
        );
    }

    if let Some(path) = write_baseline {
        write_baseline_tsv(&path, &per_file)?;
        println!("baseline written to {}", path.display());
        return Ok(0);
    }
    if let Some(path) = baseline {
        let moved = compare_baseline(&path, &per_file)?;
        if moved > 0 {
            eprintln!(
                "\n{moved} file(s) moved against the baseline. ANY movement is a \
                 failure — an improvement is regenerated deliberately with \
                 --write-baseline, never automatically, because a number that \
                 drifts upward unnoticed is how a regression hides next to a win."
            );
            return Ok(1);
        }
    }
    // A divergence is the only category that means something is WRONG.
    Ok(if totals.diverged > 0 { 1 } else { 0 })
}

/// Statements in a `.sql` file, split on top-level semicolons.
///
/// psql's meta-commands (`\d`, `\set`, …) are DROPPED rather than sent: they are
/// psql's own SQL, not the corpus's, and running them would measure psql instead
/// of mpedb.
fn statements(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut dollar: Option<String> = None;
    let mut copy_data = false;

    for line in sql.lines() {
        let trimmed = line.trim_start();

        // A COPY payload is DATA, not SQL. `COPY t FROM stdin;` is followed by
        // raw rows terminated by a line of `\.`, and reading those as
        // statements is where 1 030 "unexpected character `\`" refusals came
        // from — plus every `;` inside a data row splitting into more bogus
        // statements, which inflated the corpus's own size. eleven files, 1 553
        // data lines.
        if copy_data {
            if trimmed == "\\." {
                copy_data = false;
            }
            continue;
        }

        // psql meta-commands, at the start of a line and outside a string: they
        // are psql's own language, and running them would measure psql.
        if !in_str && dollar.is_none() && trimmed.starts_with('\\') {
            continue;
        }

        // Walk the line, tracking what a `;` means.
        let b: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];

            // Inside a dollar-quoted body only its own closing tag matters.
            if let Some(tag) = &dollar {
                let rest: String = b[i..].iter().collect();
                if rest.starts_with(tag.as_str()) {
                    cur.push_str(tag);
                    i += tag.chars().count();
                    dollar = None;
                    continue;
                }
                cur.push(c);
                i += 1;
                continue;
            }

            if !in_str && c == '-' && b.get(i + 1) == Some(&'-') {
                break; // line comment: the rest of the line is not SQL
            }
            if !in_str && c == '$' {
                // `$tag$` opens a body; `$1` is a parameter and is not one.
                let mut j = i + 1;
                while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                    j += 1;
                }
                let tag_chars: String = b[i..j].iter().collect();
                if b.get(j) == Some(&'$') && !tag_chars[1..].starts_with(|c: char| c.is_ascii_digit())
                {
                    let tag = format!("{tag_chars}$");
                    cur.push_str(&tag);
                    i = j + 1;
                    dollar = Some(tag);
                    continue;
                }
            }
            // A psql meta-command MID-LINE ends the statement, exactly as `;`
            // does — that is what `\g` MEANS ("send the buffer now"), and the
            // family around it all send too. The splitter used to see only `;`,
            // so `SELECT 1 \g SELECT 2 \gx SELECT 3` arrived as ONE blob
            // containing three statements and a backslash, which is where the
            // `unexpected character \` refusals came from.
            //
            // Three behaviours, because these commands do three different
            // things and collapsing them would trade one wrong answer for
            // another:
            //
            // * SEND — `\g` `\gx` `\gset` `\gexec`: the buffer runs. Keep it.
            // * SEND-BUT-NOT-AS-A-QUERY — `\gdesc` (describes the result's
            //   columns without producing them), `\crosstabview` (pivots the
            //   output), `\bind` (re-runs it with parameters). PostgreSQL's
            //   answer is not the row set, so comparing ours against it would
            //   score a difference neither engine disagrees about. Dropped.
            // * DISCARD — `\r` resets the buffer: psql never runs what came
            //   before it, so neither may we.
            if c == '\\' && !in_str && dollar.is_none() {
                let mut j = i + 1;
                while j < b.len() && b[j].is_alphanumeric() {
                    j += 1;
                }
                let cmd: String = b[i + 1..j].iter().collect::<String>().to_ascii_lowercase();
                match meta_kind(&cmd) {
                    Some(MetaKind::Send) => {
                        let t = cur.trim().to_string();
                        if !t.is_empty() {
                            out.push(t);
                        }
                        cur.clear();
                        i = j;
                        continue;
                    }
                    Some(MetaKind::Drop) | Some(MetaKind::Discard) => {
                        cur.clear();
                        i = j;
                        continue;
                    }
                    None => {}
                }
            }
            if c == '\'' {
                // `''` is an ESCAPED quote inside a string, not the end of one.
                // Without this the flag flips on the first half, the string is
                // considered closed, and the next `;` inside it splits the
                // statement into fragments — which is where 1 047 "unsupported
                // statement (" refusals came from: they were the tails of
                // statements torn in half, not SQL anyone wrote.
                if in_str && b.get(i + 1) == Some(&'\'') {
                    cur.push('\'');
                    cur.push('\'');
                    i += 2;
                    continue;
                }
                in_str = !in_str;
            }
            if c == ';' && !in_str {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    if is_copy_from_stdin(&s) {
                        copy_data = true;
                    }
                    out.push(s);
                }
                cur.clear();
                i += 1;
                continue;
            }
            cur.push(c);
            i += 1;
        }
        cur.push('\n');
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// Is this an `EXPLAIN`?
///
/// Excluded from BOTH arms, because comparing two engines' plan text is not a
/// compatibility question — it is a comparison of two different planners. mpedb
/// has its own plan representation (access paths, MPEE join order, precomputed
/// footprints); PostgreSQL prints `Seq Scan on document / Filter: …`. Neither
/// can produce the other's, and neither should.
///
/// Found because `EXPLAIN (COSTS OFF) SELECT …` — PostgreSQL's parenthesised
/// option list — was the single largest refusal in the corpus at 1 047
/// statements, all reported as "unsupported statement (" because mpedb's
/// EXPLAIN takes no option list. Making the parser swallow the options would
/// have converted 1 047 refusals into 1 047 DIVERGENCES and called it progress;
/// the answers would still never have matched.
fn is_explain(stmt: &str) -> bool {
    stmt.trim_start()
        .get(..7)
        .is_some_and(|h| h.eq_ignore_ascii_case("EXPLAIN"))
}

/// Does this statement expect a data payload to follow?
fn is_copy_from_stdin(stmt: &str) -> bool {
    let u = stmt.to_ascii_uppercase();
    u.trim_start().starts_with("COPY") && u.contains("FROM STDIN")
}

/// One statement's outcome on one engine.
#[derive(PartialEq, Debug)]
enum Outcome {
    Rows(String),
    /// The SQLSTATE and message mpedb refused with.
    ///
    /// The message is the point. 17 728 statements are refused, and until now
    /// the harness recorded only THAT they were, not WHY — so working out what
    /// to build next meant grepping the corpus by hand and guessing. Twice that
    /// produced a wrong guess. The session already has this string; throwing it
    /// away was the expensive part.
    ///
    /// PostgreSQL's side carries no message: its wording is its own and would
    /// never group with mpedb's.
    Error(Option<ErrText>),
}

/// One refusal SHAPE, with how often it fired.
#[derive(Default, Clone)]
struct Cause {
    count: u32,
    code: String,
    example: String,
    /// Which corpus files this shape occurs in, and how often.
    ///
    /// The splitting field of last resort, and the one that works when there
    /// is no other. A shape that is 628 statements spread over 90 files is a
    /// general gap; the same 628 concentrated in `jsonb.sql` is one feature
    /// with a name. Those are different jobs and the count alone cannot tell
    /// them apart — which is the same lesson as every other split in this
    /// harness, applied to the buckets that have no message left to split on.
    files: std::collections::BTreeMap<String, u32>,
}

impl Cause {
    /// The file this shape leans on hardest, and how much of it that is.
    fn concentration(&self) -> Option<(&str, u32, usize)> {
        let (f, n) = self.files.iter().max_by_key(|(f, n)| (**n, std::cmp::Reverse(f.as_str())))?;
        Some((f, *n, self.files.len()))
    }
}

/// Record one divergence shape.
///
/// Same container as the refusal causes on purpose: the two lists want the
/// same ranking, the same "top N plus a tail", and the same discipline that a
/// key groups while an example illustrates. A second bespoke struct would have
/// drifted from the first within a round.
/// Where a shape LIVES — the split of last resort.
///
/// Printed for every ranked line, cheap, and it answers the one question the
/// count cannot when there is no message left to group on: is this a general
/// gap or one file's feature? 628 "text values differ" spread over 90 files is
/// a hundred small jobs; the same 628 with 400 in `jsonb.sql` is one.
///
/// The FILE COUNT always, the file NAME only when it is a finding. Naming the
/// top file of an evenly-spread shape would give a reader a name to act on
/// where there is nothing there; withholding the spread entirely would hide
/// that the spread is itself the answer. The first run of this made exactly
/// that mistake — the two largest shapes printed nothing at all, and "nothing"
/// reads as "not measured" rather than as "everywhere".
fn print_concentration(c: &Cause) {
    let Some((file, n, distinct)) = c.concentration() else {
        return;
    };
    if distinct == 1 {
        println!("          all in {file}.sql");
    } else if n * 3 >= c.count {
        println!(
            "          {n} of {} in {file}.sql (over {distinct} files)",
            c.count
        );
    } else {
        // Spread thin: a general gap, not one file's feature. Worth as much as
        // a concentration and easy to mistake for an absence of information.
        println!(
            "          spread over {distinct} files, none holding a third (top: {file}.sql, {n})"
        );
    }
}

/// The FAMILY totals for the buckets that are deliberately split by name.
///
/// Splitting `unknown function` and `table function in FROM` by name is what
/// turned two collapsed lines into work lists — and it threw away the number a
/// collapsed line was good at: how big the family is. Both readings are needed
/// and they answer different questions. The family says how much a whole
/// SUBSYSTEM is worth; the split says whether that total is one item or a tail,
/// and therefore whether the total is reachable at all.
///
/// `table function in FROM` is the case that proves it. Collapsed, it read as
/// ~950 statements of "table functions" — one feature. Split, its LARGEST
/// member is `check_estimated_rows`, a plpgsql helper the corpus defines for
/// its own use and that nothing will ever implement as a builtin. The family
/// total is real; the share of it a table-function planner would collect is
/// not, and only both numbers together say so.
fn print_families(ranked: &[(&String, &Cause)], families: &[&str]) {
    for fam in families {
        let members: Vec<&(&String, &Cause)> =
            ranked.iter().filter(|(k, _)| k.starts_with(fam)).collect();
        if members.is_empty() {
            continue;
        }
        let total: u32 = members.iter().map(|(_, c)| c.count).sum();
        let biggest = members.iter().map(|(_, c)| c.count).max().unwrap_or(0);
        let name = fam.trim_end_matches(": ").trim_end_matches(" `").trim_end_matches('`');
        println!(
            "\n  FAMILY `{name}`: {total} refusals over {} distinct names; \
             biggest single name {biggest} ({:.0}% of the family).",
            members.len(),
            100.0 * f64::from(biggest) / f64::from(total.max(1))
        );
        for (k, c) in members.iter().take(12) {
            println!("      {:>6}  {}", c.count, k);
        }
        if members.len() > 12 {
            let rest: u32 = members.iter().skip(12).map(|(_, c)| c.count).sum();
            println!("      {rest:>6}  … {} further names", members.len() - 12);
        }
    }
}

fn note(map: &mut std::collections::HashMap<String, Cause>, s: divergence::Shape, file: &str) {
    let e = map.entry(s.key).or_insert_with(|| Cause {
        code: "-".into(),
        example: s.example.clone(),
        ..Cause::default()
    });
    e.count += 1;
    *e.files.entry(file.to_string()).or_default() += 1;
    if e.example.is_empty() {
        e.example = s.example;
    }
}

#[derive(PartialEq, Debug, Clone)]
struct ErrText {
    code: String,
    message: String,
}

fn diff_file(
    path: &Path,
    show: bool,
    causes: &mut std::collections::HashMap<String, Cause>,
    diffs: &mut std::collections::HashMap<String, Cause>,
    cascade: &mut Cascade,
) -> Result<Counts, String> {
    // Two of the corpus files are deliberately NOT UTF-8 (`collate.windows.
    // win1252`), and one of them aborting the run means the 200 files after it
    // never get measured. Read lossily and carry on: mpedb's `Value::Text` is
    // UTF-8 by construction, so a win1252 collation file was never going to be
    // a comparison it could win — but that is a result to RECORD, not a reason
    // to stop.
    let fname = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sql = String::from_utf8_lossy(&raw).into_owned();
    let all = statements(&sql);
    // `COPY … FROM stdin` is dropped from BOTH arms, not just one.
    //
    // Its payload was skipped by the splitter (it is data, not SQL), so neither
    // engine can be given it. Sending the statement anyway would leave psql
    // waiting for rows and swallowing the rest of the script — including the
    // `\echo` markers the whole per-statement alignment depends on. One
    // unbalanced COPY corrupts every later statement in that file.
    //
    // Dropped LOUDLY: a silent omission is how a corpus scores well by skipping
    // the hard parts.
    let before = all.len();
    let stmts: Vec<String> = all
        .into_iter()
        .filter(|s| !is_copy_from_stdin(s) && !is_explain(s))
        .collect();
    let skipped = before - stmts.len();
    if skipped > 0 {
        println!(
            "{:<24} note: {skipped} COPY-payload / EXPLAIN statement(s) skipped — see \
             `is_copy_from_stdin` and `is_explain` for why neither is a compatibility question",
            path.file_stem().unwrap_or_default().to_string_lossy()
        );
    }
    if stmts.is_empty() {
        return Ok(Counts::default());
    }
    // One process per FILE, not per statement. A psql invocation per statement
    // would make a 300-statement file 300 forks, which turns a 4-minute corpus
    // run into an hour — the same reason `mpedb-testkit`'s differential batches.
    let pg = pg_transcript(&stmts)?;
    // The mpedb arm runs behind a WATCHDOG.
    //
    // A hang is not hypothetical here: a `CREATE TEMPORARY TABLE` inside a
    // transaction block self-deadlocked on the writer lock, and one such file
    // stalled a 222-file run for twelve minutes before anyone knew which file
    // it was. A measurement tool that can be stopped dead by the thing it is
    // measuring cannot finish a run, and an unfinished run measures nothing.
    //
    // The stuck thread is LEAKED rather than killed — a blocked thread holding
    // an engine lock cannot be unwound safely from outside, and this is a
    // short-lived measurement process. The file is recorded as hung and the run
    // continues, which is the whole point.
    let mp = match run_with_timeout(stmts.clone(), FILE_TIMEOUT) {
        Ok(t) => t,
        Err(Stuck { err: Some(e), .. }) => return Err(e),
        Err(Stuck { at, .. }) => {
            let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let culprit = at
                .and_then(|i| stmts.get(i))
                .map(|s| s.chars().take(140).collect::<String>())
                .unwrap_or_else(|| "<unknown: no query completed>".into());
            println!(
                "{name:<24} HUNG after {}s at statement {} of {}; counted as all-diverged\n\
                 {:24} stuck on: {culprit}",
                FILE_TIMEOUT.as_secs(),
                at.map(|i| i + 1).unwrap_or(0),
                stmts.len(),
                ""
            );
            return Ok(Counts {
                diverged: stmts.len() as u32,
                ..Counts::default()
            });
        }
    };

    let mut c = Counts::default();
    // Table names whose `CREATE` mpedb refused, IN THIS FILE and BEFORE the
    // statement being judged. That is what turns "unknown table" from a claim
    // into a count: a later reference to one of these is a CONSEQUENCE of the
    // earlier refusal, not an independent gap, and the two need very different
    // work. Per file because the harness gives each file its own database.
    let mut failed_creates: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for (i, stmt) in stmts.iter().enumerate() {
        let (a, b) = (pg.get(i), mp.get(i));
        if matches!(b, Some(Outcome::Error(_))) {
            if let Some(t) = created_table_name(stmt) {
                failed_creates.insert(t);
            }
        }
        if show {
            println!("--- [{i}] {}", stmt.replace('\n', " "));
            println!("    pg:    {:?}", a);
            println!("    mpedb: {:?}", b);
        }
        match (a, b) {
            (Some(Outcome::Error(_)), Some(Outcome::Error(_))) => c.both_refused += 1,
            (Some(Outcome::Rows(_)), Some(Outcome::Error(e))) => {
                c.refused += 1;
                // The work list. Only THIS quadrant is recorded: PostgreSQL
                // answered and mpedb did not, so the message names something
                // mpedb would have to gain. A both-refused agrees, and a
                // divergence is a wrong answer rather than a missing feature.
                if let Some(e) = e {
                    if let Some(t) = missing_table_name(&e.message) {
                        if failed_creates.contains(&t) {
                            cascade.consequence += 1;
                        } else {
                            cascade.independent += 1;
                        }
                    }
                    let c = causes
                        .entry(cause_key(&e.message))
                        .or_insert_with(|| Cause {
                            code: e.code.clone(),
                            example: e.message.clone(),
                            ..Cause::default()
                        });
                    c.count += 1;
                    *c.files.entry(fname.clone()).or_default() += 1;
                }
            }
            // PostgreSQL refusing what mpedb ACCEPTS is also a divergence: mpedb
            // answered a question PostgreSQL declined, which is a wrong answer in
            // the direction nobody looks for.
            (Some(Outcome::Error(pe)), Some(Outcome::Rows(y))) => {
                c.diverged += 1;
                note(
                    diffs,
                    divergence::pg_refused_mpedb_answered(
                        pe.as_ref().map(|e| cause_shape(&e.message)).as_deref(),
                        y,
                    ),
                    &fname,
                );
            }
            (Some(Outcome::Rows(x)), Some(Outcome::Rows(y))) => match compare(x, y, stmt) {
                Verdict::Same => c.matched += 1,
                Verdict::OrderOnly => c.order_only += 1,
                Verdict::Different => {
                    c.diverged += 1;
                    // Classify AFTER the verdict, never instead of it — see
                    // `divergence`'s module docs on why that ordering is the
                    // whole safety property.
                    note(
                        diffs,
                        divergence::classify(
                            &rows(x),
                            &rows(y),
                            has_top_level_order_by(stmt),
                        ),
                        &fname,
                    );
                }
            },
            // A transcript that ran short means one engine stopped answering —
            // counted as a divergence rather than skipped, because silently
            // dropping the tail is how a file scores well by dying early.
            (None, Some(_)) => {
                c.diverged += 1;
                note(diffs, divergence::transcript_ended_early("PostgreSQL"), &fname);
            }
            (Some(_), None) => {
                c.diverged += 1;
                note(diffs, divergence::transcript_ended_early("mpedb"), &fname);
            }
            _ => {
                c.diverged += 1;
                note(diffs, divergence::transcript_ended_early("both"), &fname);
            }
        }
    }
    Ok(c)
}

enum Verdict {
    Same,
    OrderOnly,
    Different,
}

/// Compare two transcripts for one statement.
///
/// Ordered equality first. If that fails but the two are the same MULTISET,
/// the answer is right and only the sequence differs — which is a divergence
/// only if the statement ASKED for an order. A statement with a top-level
/// `ORDER BY` that comes back in the wrong order is a real bug; one without is
/// two correct answers.
///
/// A partial `ORDER BY` (ties on the sort key) still counts as ordered here,
/// so `ORDER BY depname, salary` with two rows sharing both values is reported
/// as a divergence even though both engines are right. That is deliberate: the
/// alternative — sorting whenever ties are possible — would hide a real
/// ordering bug behind an "it might have been a tie" excuse, and there is no
/// way to tell the two apart from outside. The over-report is visible in the
/// `DIVERGED` column and named here rather than silently corrected.
fn compare(pg: &str, mp: &str, stmt: &str) -> Verdict {
    let (mut la, mut lb) = (rows(pg), rows(mp));
    if la == lb {
        return Verdict::Same;
    }
    if la.len() != lb.len() {
        return Verdict::Different;
    }
    la.sort_unstable();
    lb.sort_unstable();
    if la != lb {
        return Verdict::Different;
    }
    if has_top_level_order_by(stmt) {
        Verdict::Different
    } else {
        Verdict::OrderOnly
    }
}

/// Does the statement carry an `ORDER BY` at PAREN DEPTH ZERO?
///
/// Depth matters: `SELECT … FROM (SELECT … ORDER BY x) t` orders a SUBQUERY,
/// and the outer result is still unordered. Counting that as ordered would put
/// every such statement in the divergence column for a sequence neither engine
/// promised.
///
/// Deliberately a text scan rather than a parse: this runs once per statement
/// over 45 000 statements, and it only has to decide which of two comparison
/// rules to apply. A window's `OVER (ORDER BY …)` sits inside parentheses and
/// is correctly ignored by the same depth rule.
fn has_top_level_order_by(stmt: &str) -> bool {
    let b = stmt.as_bytes();
    let up = stmt.to_ascii_uppercase();
    let u = up.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => depth -= 1,
            // A whole WORD, not the tail of one: `REORDER` is not `ORDER`.
            b'O' | b'o' if !in_str && depth == 0 && u[i..].starts_with(b"ORDER") => {
                let before_ok = i == 0 || (!u[i - 1].is_ascii_alphanumeric() && u[i - 1] != b'_');
                let after_ok = u[i + 5..].first().is_some_and(u8::is_ascii_whitespace);
                if before_ok && after_ok {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// A transcript as ROWS, with the only difference that is formatting removed.
///
/// Kept to the minimum that is defensible. Trailing whitespace and a trailing
/// newline are psql's layout, not the engine's answer. Nothing else is
/// normalised — in particular, numbers are NOT reformatted, because a float
/// printed differently IS a different answer and hiding it here would be the
/// exact self-deception this harness exists to prevent.
///
/// A `Vec` of rows rather than a rejoined `String`, and that is a FIX rather
/// than a tidy-up. Rejoining collapsed one distinction psql draws precisely:
/// a result of ONE EMPTY ROW is `"\n"` and a result of NO ROWS is `""`, and
/// `lines().join("\n")` maps both to `""`. The harness then called them equal
/// — a false MATCH, in the direction that flatters the score. Splitting into
/// rows and never rejoining keeps the count, which is the whole content of
/// that distinction.
fn rows(s: &str) -> Vec<&str> {
    s.lines().map(str::trim_end).collect()
}

/// The marker psql echoes between statements so one batch can be split back
/// into per-statement transcripts. Chosen to be something no corpus file emits.
const MARK: &str = "@@mpedb-stmt-boundary@@";

/// Run the whole file through a real PostgreSQL in ONE psql session.
///
/// The cluster is the operator's, named by the ordinary `PG*` environment
/// variables. Spinning one up here was the alternative and was rejected: it
/// would need `initdb` and a data directory inside a binary whose job is to
/// compare, and the operator almost always already has the cluster they want
/// measured against.
fn pg_transcript(stmts: &[String]) -> Result<Vec<Outcome>, String> {
    reset_pg_schema()?;
    let mut script = String::new();
    // ON_ERROR_STOP=0: a failing statement must not abandon the rest of the
    // file, or one early refusal would score every later statement as missing.
    script.push_str("\\set ON_ERROR_STOP 0\n");
    for s in stmts {
        script.push_str(s);
        script.push_str(";\n");
        script.push_str(&format!("\\echo {MARK}\n"));
    }
    // ONE merged stream, via `2>&1`.
    //
    // The first version read stdout and stderr separately and split each on the
    // marker. That cannot work: `\echo` writes to STDOUT ONLY, so the stderr
    // side had no markers at all and every statement read as "no error" — which
    // scored a whole corpus as 171 divergences and 0 matches. Merging keeps
    // rows and errors in the order psql produced them, which is the only thing
    // that makes a per-statement chunk meaningful.
    let out = Command::new("sh")
        .arg("-c")
        .arg("psql -X -q -A -t -F'|' -P null=NULL 2>&1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut ch| {
            use std::io::Write as _;
            ch.stdin.as_mut().unwrap().write_all(script.as_bytes())?;
            ch.wait_with_output()
        })
        .map_err(|e| format!("psql: {e}"))?;

    let text = String::from_utf8_lossy(&out.stdout);
    let chunks = split_marked(&text, MARK);
    Ok((0..stmts.len())
        .map(|i| match chunks.get(i) {
            // PostgreSQL's own wording is never grouped with MPEDB's — the two
            // engines phrase the same refusal differently and a shared bucket
            // would be noise. It IS kept, because there is one bucket where it
            // is the only information there is: `PostgreSQL refused, mpedb
            // ANSWERED`, the largest divergence shape. Nothing on mpedb's side
            // says why that statement should have been refused; PostgreSQL's
            // message is the entire content of the finding.
            None => Outcome::Error(None),
            Some(c) if c.contains("ERROR:") || c.contains("FATAL:") => {
                Outcome::Error(pg_error_text(c))
            }
            Some(c) => Outcome::Rows(strip_notices(c)),
        })
        .collect())
}

/// PostgreSQL's own error line out of a psql chunk.
///
/// No SQLSTATE: `psql -q -A -t` prints `ERROR:  <message>` and nothing else,
/// so the code is a placeholder. The message is what matters here — see the
/// call site on why this side is kept at all.
fn pg_error_text(chunk: &str) -> Option<ErrText> {
    let line = chunk
        .lines()
        .find(|l| l.contains("ERROR:") || l.contains("FATAL:"))?;
    let at = line.find("ERROR:").or_else(|| line.find("FATAL:"))?;
    Some(ErrText {
        code: "-".into(),
        message: line[at + 6..].trim().to_string(),
    })
}

/// Empty the PostgreSQL side before a file runs.
///
/// A `.sql` file builds its own tables, so it must start from nothing — exactly
/// the reason [`fresh_db`] gives mpedb a new database per file. Without this the
/// second run of any file hits "relation already exists" on its own CREATE and
/// scores the whole file as errors, which reads as a catastrophic incompatibility
/// and is really just leftover state.
///
/// Run as its OWN psql invocation so the reset does not appear in the
/// transcript the statements are matched against.
fn reset_pg_schema() -> Result<(), String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg("psql -X -q -c 'DROP SCHEMA IF EXISTS public CASCADE' \
              -c 'CREATE SCHEMA public' 2>&1")
        .output()
        .map_err(|e| format!("psql reset: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not reset the PostgreSQL schema: {}",
            String::from_utf8_lossy(&out.stdout)
        ));
    }
    Ok(())
}

/// Drop psql's chatter that is not part of an answer.
///
/// `NOTICE:` and `HINT:` lines are diagnostics PostgreSQL emits alongside a
/// SUCCESSFUL statement (`CREATE TABLE … NOTICE: table "x" does not exist,
/// skipping`). mpedb emits none, so leaving them in would score every such
/// statement as a divergence over text that is not an answer.
fn strip_notices(chunk: &str) -> String {
    chunk
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("NOTICE:")
                || t.starts_with("HINT:")
                || t.starts_with("DETAIL:")
                || t.starts_with("WARNING:")
                || t.starts_with("CONTEXT:")
                || t.starts_with("LINE "))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Split a marked transcript into one chunk per statement.
fn split_marked(text: &str, mark: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim() == mark {
            out.push(cur.trim_end().to_string());
            cur.clear();
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim_end().to_string());
    }
    out
}

/// Pull the SQLSTATE (`C`) and message (`M`) out of an ErrorResponse body.
///
/// The body is a sequence of `<tag><NUL-terminated value>` pairs ended by a
/// lone NUL — see `proto::Out::error`.
fn parse_error_fields(body: &[u8]) -> Option<ErrText> {
    let mut code = None;
    let mut message = None;
    let mut i = 0usize;
    while i < body.len() && body[i] != 0 {
        let tag = body[i];
        i += 1;
        let start = i;
        while i < body.len() && body[i] != 0 {
            i += 1;
        }
        let val = String::from_utf8_lossy(&body[start..i]).into_owned();
        i += 1; // the NUL
        match tag {
            b'C' => code = Some(val),
            b'M' => message = Some(val),
            _ => {}
        }
    }
    Some(ErrText {
        code: code?,
        message: message?,
    })
}

/// Collapse a message to its SHAPE so two refusals of the same kind group.
///
/// `no such table \`users\`` and `no such table \`orders\`` are one cause, not
/// two. Anything quoted (backticks, single or double quotes) becomes `_`, and
/// any run of digits becomes `N`. Deliberately crude: this ranks a work list,
/// it does not have to be a parser, and over-grouping is visible because the
/// table prints one example message per row.
/// The grouping key for one refusal.
///
/// Normally the SHAPE (`cause_shape`), which is what makes 393 causes out of
/// 20 000 refusals. But ONE cause is worth keeping at full resolution:
/// `unknown function _` is the corpus's largest bucket at over two thousand
/// refusals, and shape-grouping collapses every PostgreSQL system function into
/// a single line whose example names one of them arbitrarily. That line says
/// "implement PostgreSQL's function surface", which is not a task.
///
/// Keeping the NAME turns it into one: the ranked list then says which
/// functions, and how many statements each is worth — and it is what shows that
/// a bucket of two thousand can be a long tail nobody should chase rather than
/// a handful worth an afternoon.
/// The split behind the corpus's largest single refusal cause.
///
/// `unknown table` has been described in COMPAT-PG.md as "largely CASCADE from
/// an earlier failed CREATE" since the first run, on the strength of four names
/// spot-checked by hand. This counts it.
#[derive(Debug, Default)]
struct Cascade {
    /// The `CREATE` for this table was refused EARLIER IN THE SAME FILE.
    consequence: u32,
    /// It was not — so mpedb refused a table PostgreSQL had, for some other
    /// reason. These are the ones worth reading.
    independent: u32,
}

/// `CREATE [TEMP|TEMPORARY|UNLOGGED|GLOBAL|LOCAL] TABLE [IF NOT EXISTS] <name>`
/// → `<name>`, lower-cased.
///
/// A text scan, not a parse: the statement already failed to parse in at least
/// one engine, so a parser is the wrong tool. Getting a name wrong here can
/// only misattribute one refusal between two buckets — it cannot make a
/// statement pass or fail — which is why an approximation is honest for this
/// job and would not be for the judging itself.
fn created_table_name(stmt: &str) -> Option<String> {
    let mut w = stmt.split_whitespace();
    if !w.next()?.eq_ignore_ascii_case("create") {
        return None;
    }
    let mut tok = w.next()?;
    while ["temp", "temporary", "unlogged", "global", "local"]
        .iter()
        .any(|k| tok.eq_ignore_ascii_case(k))
    {
        tok = w.next()?;
    }
    if !tok.eq_ignore_ascii_case("table") {
        return None;
    }
    let mut name = w.next()?;
    if name.eq_ignore_ascii_case("if") {
        let _ = w.next(); // NOT
        let _ = w.next(); // EXISTS
        name = w.next()?;
    }
    // `t(a int)` — the name runs up to the paren, and the schema qualifier is
    // dropped because mpedb has one namespace.
    let name = name.split('(').next()?;
    let name = name.rsplit('.').next()?;
    let name: String = name
        .trim_matches('"')
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

/// The table named by an `unknown table` / `no such table` error.
fn missing_table_name(msg: &str) -> Option<String> {
    for mark in ["unknown table `", "no such table `"] {
        if let Some(at) = msg.find(mark) {
            let rest = &msg[at + mark.len()..];
            let name = rest.split('`').next()?;
            return Some(name.to_ascii_lowercase());
        }
    }
    None
}

/// What a psql meta-command does to the query buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetaKind {
    /// Sends the buffer as an ordinary query.
    Send,
    /// Sends it, but PostgreSQL's answer is not the row set — so the statement
    /// is not a comparison either engine would win or lose.
    Drop,
    /// Throws the buffer away without running it.
    Discard,
}

fn meta_kind(cmd: &str) -> Option<MetaKind> {
    match cmd {
        "g" | "gx" | "gset" | "gexec" => Some(MetaKind::Send),
        "gdesc" | "crosstabview" | "bind" => Some(MetaKind::Drop),
        "r" | "reset" => Some(MetaKind::Discard),
        _ => None,
    }
}

fn cause_key(msg: &str) -> String {
    // `find`, not `strip_prefix`. The message is `bind error: unknown function
    // \`f()\`; available: …` — the marker is in the MIDDLE, and a prefix test
    // silently matched nothing, fell through to the shape, and left the bucket
    // exactly as collapsed as before. It looked like the feature did not help;
    // it had never run.
    const MARK: &str = "unknown function `";
    if let Some(at) = msg.find(MARK) {
        let rest = &msg[at + MARK.len()..];
        if let Some(name) = rest.split('`').next() {
            return format!("unknown function `{name}`");
        }
    }
    // The table-function bucket, for the fifth time in this file's history and
    // for the same reason. `\`f(…)\` is a table function in FROM position` is
    // ONE line of ~950 with one example under it, and the example (`unnest`)
    // is one arbitrary member. The NAME is the whole content: `unnest` needs an
    // array type, `generate_series` in a non-first position needs a join-side
    // row source, and `rngfunct` is a corpus-local PL/pgSQL function that will
    // never be implemented. Those are three different jobs and one number.
    const TF: &str = "is a table function in FROM position";
    if let Some(at) = msg.find(TF) {
        // The name is the backticked token before the marker: `f(…)`.
        let before = &msg[..at];
        if let Some(open) = before.rfind('`') {
            if let Some(start) = before[..open].rfind('`') {
                let name = &before[start + 1..open];
                return format!("table function in FROM `{name}`");
            }
        }
    }
    // `expected \`X\`` — the parser's other catch-all. WHAT it expected is the
    // whole content: `expected \`TABLE\`` after CREATE is a DDL form that does
    // not exist, `expected \`)\`` is a syntax the expression grammar cannot
    // reach. The shaped line said neither.
    const EXP: &str = "expected `";
    if let Some(at) = msg.find(EXP) {
        let rest = &msg[at + EXP.len()..];
        if let Some(w) = rest.split('`').next() {
            // The tail after the backtick pair matters too: `expected \`)\`
            // closing the argument list` is a different site from a bare
            // `expected \`)\``, and merging them would hide which.
            let after = &rest[w.len()..];
            let tail: String = after
                .trim_start_matches('`')
                .chars()
                .take(40)
                .collect();
            return format!("expected `{w}`{tail}");
        }
    }
    // …and the character, for the same reason again. `unexpected character _`
    // was read as "psql meta-commands" on the strength of one example showing
    // `\`. The dominant member is `}`.
    const CH: &str = "unexpected character `";
    if let Some(at) = msg.find(CH) {
        let rest = &msg[at + CH.len()..];
        if let Some(c) = rest.split('`').next() {
            return format!("unexpected character `{c}`");
        }
    }
    // The same treatment for the OTHER collapsed bucket, for the same reason.
    // `unexpected trailing input _` is the parser's catch-all: the statement
    // parsed and then something was left over. WHAT was left over is the whole
    // information — `CASCADE` after a DROP is a different job from a `(` that
    // starts an option list — and shape-grouping throws exactly that away.
    const TAIL: &str = "unexpected trailing input `";
    if let Some(at) = msg.find(TAIL) {
        let rest = &msg[at + TAIL.len()..];
        if let Some(tok) = rest.split('`').next() {
            return format!("unexpected trailing input `{tok}`");
        }
    }
    cause_shape(msg)
}

fn cause_shape(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let b: Vec<char> = msg.chars().collect();
    let mut i = 0usize;
    let mut last_digit = false;
    while i < b.len() {
        let c = b[i];
        if c == '`' || c == '\'' || c == '"' {
            out.push('_');
            i += 1;
            while i < b.len() && b[i] != c {
                i += 1;
            }
            i += 1;
            last_digit = false;
            continue;
        }
        if c.is_ascii_digit() {
            if !last_digit {
                out.push('N');
            }
            last_digit = true;
            i += 1;
            continue;
        }
        last_digit = false;
        out.push(c);
        i += 1;
    }
    // The message often carries a long "available: …" tail; the shape is the
    // first sentence.
    let s = out.trim().to_string();
    match s.find(" — ").or_else(|| s.find("; available")) {
        Some(at) => s[..at].to_string(),
        None => s,
    }
}

/// How long one file's mpedb arm may take before it is declared hung.
///
/// Generous: the slowest legitimate file in the corpus finishes well inside it,
/// so crossing this is a bug rather than a big file.
const FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run the mpedb arm on another thread and give up on it after `limit`.
/// Run the file, and on a timeout report WHICH STATEMENT was in flight.
///
/// The old version returned `None` and the caller printed "this file hung".
/// That is a collapsed line with nothing inside it — the same shape this
/// harness has learned five times not to trust — and it is the least useful
/// possible form for the one failure that stops all measurement. The worker
/// publishes a count of completed queries; on a timeout the next statement is
/// the culprit, named.
fn run_with_timeout(
    stmts: Vec<String>,
    limit: std::time::Duration,
) -> Result<Vec<Outcome>, Stuck> {
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = progress.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(mpedb_transcript(&stmts, progress));
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Stuck { at: None, err: Some(e) }),
        Err(_) => Err(Stuck {
            at: Some(seen.load(std::sync::atomic::Ordering::Relaxed)),
            err: None,
        }),
    }
}

/// Why a file produced no transcript.
struct Stuck {
    /// Index of the statement that never returned, if it timed out. The count
    /// is of COMPLETED queries, so the one in flight is at that index —
    /// off-by-one only if the session died between finishing and answering,
    /// which would be an error rather than a timeout.
    at: Option<usize>,
    err: Option<String>,
}

/// Run the whole file through mpedb, IN PROCESS.
///
/// No socket: a `Session` reads and writes anything `Read + Write`, so the
/// transcript comes from driving one over a pipe — the same harness
/// `tests/wire.rs` uses. That removes a listening socket, a spawn and a port
/// from a tool whose only job is to compare answers.
fn mpedb_transcript(
    stmts: &[String],
    progress: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Result<Vec<Outcome>, String> {
    let db = fresh_db()?;
    let mut input = startup_packet();
    for s in stmts {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        input.extend_from_slice(&framed(b'Q', &b));
    }
    input.extend_from_slice(&framed(b'X', &[]));

    let pipe = Pipe {
        input: std::io::Cursor::new(input),
        output: Vec::new(),
        done: progress,
        scanned: 0,
    };
    let mut sess = mpedb_pg::Session::new(
        pipe,
        mpedb_pg::Options {
            db,
            server_version: mpedb_pg::server_version(),
            require_password: false,
        },
    );
    let bytes = sess.run_for_test();
    Ok(transcripts_from_backend(&bytes, stmts.len()))
}

/// A `.sql` file builds its own tables, so every file starts from an EMPTY
/// database. Reusing one across files would let a `CREATE TABLE` in `int4`
/// decide whether `int8` passes.
fn fresh_db() -> Result<mpedb::Database, String> {
    let cfg = mpedb::Config::from_toml_str(
        "[database]\npath = \":memory:\"\nsize_mb = 64\ndurability = \"none\"\n",
    )
    .map_err(|e| format!("config: {e}"))?;
    mpedb::Database::open_in_memory(cfg).map_err(|e| format!("open: {e}"))
}

struct Pipe {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
    /// Completed simple queries, published for the watchdog thread.
    done: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// How far `output` has been walked for `ReadyForQuery` frames. The walk
    /// resumes here rather than restarting, so counting is O(bytes) over the
    /// whole run and not O(bytes²).
    scanned: usize,
}

impl std::io::Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.input, buf)
    }
}

impl std::io::Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buf);
        // A `ReadyForQuery` ends one simple query. Counting them from the
        // WRITE side is what lets the watchdog name the statement that did not
        // finish instead of the file that contains it — see `run_with_timeout`.
        //
        // Counted on frame boundaries, not by scanning for the byte: a `Z` can
        // appear inside a row value, and a data-dependent progress counter
        // would name the wrong statement exactly when it matters most.
        let mut i = self.scanned;
        while i + 5 <= self.output.len() {
            let len =
                i32::from_be_bytes(self.output[i + 1..i + 5].try_into().unwrap()) as usize;
            if len < 4 || i + 1 + len > self.output.len() {
                break;
            }
            if self.output[i] == b'Z' {
                self.done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            i += 1 + len;
        }
        self.scanned = i;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl mpedb_pg::session::AsBytes for Pipe {
    fn written(&self) -> &[u8] {
        &self.output
    }
}

fn startup_packet() -> Vec<u8> {
    let mut body = 196_608i32.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\0regress\0database\0regress\0\0");
    let mut pkt = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    pkt.extend_from_slice(&body);
    pkt
}

fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

/// Turn the backend byte stream into one transcript per statement, rendered the
/// way psql's `-A -t -F'|' -P null=NULL` renders it — so the two sides are
/// compared on the same layout and a difference is a difference in the ANSWER.
fn transcripts_from_backend(bytes: &[u8], want: usize) -> Vec<Outcome> {
    let mut out: Vec<Outcome> = Vec::new();
    let mut cur = String::new();
    let mut failed = false;
    let mut started = false;
    let mut i = 0usize;
    while i + 5 <= bytes.len() {
        let tag = bytes[i];
        let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
        if len < 4 || i + 1 + len > bytes.len() {
            break;
        }
        let body = &bytes[i + 5..i + 1 + len];
        i += 1 + len;
        match tag {
            b'T' => started = true,
            b'D' => {
                started = true;
                let n = i16::from_be_bytes(body[0..2].try_into().unwrap()) as usize;
                let mut at = 2usize;
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    let l = i32::from_be_bytes(body[at..at + 4].try_into().unwrap());
                    at += 4;
                    if l < 0 {
                        fields.push("NULL".to_string());
                    } else {
                        fields.push(
                            String::from_utf8_lossy(&body[at..at + l as usize]).into_owned(),
                        );
                        at += l as usize;
                    }
                }
                cur.push_str(&fields.join("|"));
                cur.push('\n');
            }
            // An ErrorResponse CLOSES the statement. Neither mpedb nor
            // PostgreSQL sends CommandComplete after an error, so waiting for
            // one made the NEXT statement's `C` close this one — swallowing two
            // statements into a single entry and sliding every later transcript
            // onto the wrong statement. That is what turned int4.sql into 91
            // "divergences" whose mpedb column was plainly some other query's
            // answer.
            b'E' => {
                out.push(Outcome::Error(parse_error_fields(body)));
                cur.clear();
                failed = false;
                started = false;
            }
            // CommandComplete (or EmptyQueryResponse) ALWAYS closes a
            // statement — that is what it means. Gating it on having seen a
            // RowDescription first was wrong: a write emits only `C`, so every
            // INSERT and CREATE vanished from the transcript and shifted all
            // the later statements onto the wrong ones. Eight statements scored
            // as seven divergences that way, none of them real.
            b'C' | b'I' => {
                out.push(if failed {
                    Outcome::Error(None)
                } else {
                    Outcome::Rows(std::mem::take(&mut cur))
                });
                cur.clear();
                failed = false;
                started = false;
            }
            _ => {}
        }
    }
    // Anything still buffered is a statement whose terminator never arrived
    // (a stream cut short). Flushed rather than dropped, so a truncated run is
    // visible as a divergence instead of silently scoring nothing.
    if started || !cur.is_empty() {
        out.push(if failed {
            Outcome::Error(None)
        } else {
            Outcome::Rows(cur)
        });
    }
    out.truncate(want);
    out
}

fn write_baseline_tsv(path: &Path, per_file: &[(String, Counts)]) -> Result<(), String> {
    let mut out =
        String::from("# file\tmatch\torder_only\tboth_refused\trefused\tdiverged\n");
    for (name, c) in per_file {
        out.push_str(&format!(
            "{name}\t{}\t{}\t{}\t{}\t{}\n",
            c.matched, c.order_only, c.both_refused, c.refused, c.diverged
        ));
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

fn compare_baseline(path: &Path, per_file: &[(String, Counts)]) -> Result<usize, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut want = std::collections::BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 6 {
            return Err(format!("malformed baseline line: {line}"));
        }
        let n = |i: usize| f[i].parse::<u32>().unwrap_or(u32::MAX);
        want.insert(
            f[0].to_string(),
            Counts {
                matched: n(1),
                order_only: n(2),
                both_refused: n(3),
                refused: n(4),
                diverged: n(5),
            },
        );
    }
    let mut moved = 0usize;
    for (name, got) in per_file {
        match want.get(name) {
            None => {
                eprintln!("{name}: not in the baseline");
                moved += 1;
            }
            Some(w) if w != got => {
                eprintln!(
                    "{name}: baseline {} / {} / {} / {} / {} -> got {} / {} / {} / {} / {}",
                    w.matched,
                    w.order_only,
                    w.both_refused,
                    w.refused,
                    w.diverged,
                    got.matched,
                    got.order_only,
                    got.both_refused,
                    got.refused,
                    got.diverged
                );
                moved += 1;
            }
            _ => {}
        }
    }
    Ok(moved)
}

fn which(prog: &str) -> Option<PathBuf> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {prog}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_top_level_semicolons_and_drop_psql_meta_commands() {
        // A `\d` in a corpus file is psql's SQL, not the corpus's. Sending it
        // would measure psql rather than mpedb.
        let sql = "SELECT 1;\n\\d foo\nSELECT 2;\n-- a comment;\nSELECT 3;";
        assert_eq!(statements(sql), vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn a_semicolon_inside_a_literal_is_not_a_separator() {
        assert_eq!(statements("SELECT 'a;b';"), vec!["SELECT 'a;b'"]);
    }

    #[test]
    fn a_doubled_quote_inside_a_string_does_not_end_it() {
        // The bug this pins: two single-quotes read as "close then open"
        // leave the splitter thinking it is OUTSIDE a string, so the next
        // `;` tears a statement in half and the tail becomes a nonsense
        // "statement". 1 047 refusals were exactly that.
        assert_eq!(
            statements("SELECT 'it''s; fine' AS a;"),
            vec!["SELECT 'it''s; fine' AS a"]
        );
        assert_eq!(
            statements("SELECT 'a''';\nSELECT 2;"),
            vec!["SELECT 'a'''", "SELECT 2"]
        );
    }

    #[test]
    fn a_copy_payload_is_data_and_never_read_as_sql() {
        // `COPY t FROM stdin;` is followed by raw rows ended by `\.`. Reading
        // those as statements produced 1 030 "unexpected character `\`"
        // refusals AND split every `;` inside a data row into another bogus
        // statement — inflating both the corpus size and the failure count with
        // text that was never SQL.
        let sql = "\
CREATE TABLE t (a int, b text);
COPY t FROM stdin;
1\tone; with a semicolon
2\ttwo \\ with a backslash
\\.
SELECT * FROM t;";
        assert_eq!(
            statements(sql),
            vec![
                "CREATE TABLE t (a int, b text)",
                "COPY t FROM stdin",
                "SELECT * FROM t",
            ]
        );
    }

    /// The one cause kept at full resolution, and its neighbours still shaped.
    /// The name extractor decides which of two buckets a refusal lands in, so
    /// the spellings the corpus actually uses have to work — and the ones that
    /// are NOT a table create must not be mistaken for one.
    /// `\g` and friends end a statement the way `;` does — that is what they
    /// mean. Without this, `SELECT 1 \g SELECT 2` arrived as one blob with a
    /// backslash in the middle, and neither engine could run it.
    #[test]
    fn a_psql_send_command_terminates_a_statement_like_a_semicolon() {
        assert_eq!(
            statements("SELECT 1 as one \\g SELECT 2 \\gx SELECT 3;"),
            vec!["SELECT 1 as one", "SELECT 2", "SELECT 3"]
        );
        assert_eq!(statements("select 10 as a \\gset
SELECT 1;"), vec!["select 10 as a", "SELECT 1"]);
    }

    /// The two commands whose answer is NOT the row set drop their buffer:
    /// `\gdesc` describes the columns instead of producing them, so comparing
    /// our rows against PostgreSQL's description scores a difference neither
    /// engine disagrees about.
    #[test]
    fn a_describe_or_pivot_command_drops_the_statement_it_would_have_sent() {
        assert_eq!(statements("SELECT 1 \\gdesc SELECT 2;"), vec!["SELECT 2"]);
        assert_eq!(statements("SELECT 1 \\crosstabview SELECT 2;"), vec!["SELECT 2"]);
    }

    /// `\r` RESETS the buffer: psql never ran what came before it, so the
    /// harness must not either. Running it would compare an answer PostgreSQL
    /// was never asked for.
    #[test]
    fn a_buffer_reset_discards_the_statement_before_it() {
        assert_eq!(statements("SELECT 2 \\r SELECT 3;"), vec!["SELECT 3"]);
    }

    /// A backslash inside a STRING is content, not a command. `\ud83d\ude04`
    /// is a surrogate pair in a JSON value and appears 39 times in the corpus.
    #[test]
    fn a_backslash_inside_a_string_literal_is_not_a_meta_command() {
        assert_eq!(
            statements("SELECT '\\ud83d\\ude04' AS emoji;"),
            vec!["SELECT '\\ud83d\\ude04' AS emoji"]
        );
        // …and one that merely looks like a send command.
        assert_eq!(statements("SELECT 'a \\g b';"), vec!["SELECT 'a \\g b'"]);
    }

    #[test]
    fn a_created_table_name_is_read_out_of_every_spelling_the_corpus_uses() {
        for (sql, want) in [
            ("CREATE TABLE t (a int)", "t"),
            ("create table t(a int)", "t"),
            ("CREATE TEMP TABLE t (a int)", "t"),
            ("CREATE TEMPORARY TABLE IF NOT EXISTS t (a int)", "t"),
            ("CREATE UNLOGGED TABLE public.t (a int)", "t"),
            ("CREATE TABLE \"MixedCase\" (a int)", "mixedcase"),
            ("CREATE GLOBAL TEMPORARY TABLE t (a int)", "t"),
        ] {
            assert_eq!(created_table_name(sql).as_deref(), Some(want), "{sql}");
        }
        // Not a table create — a false positive here would silently move
        // refusals into the "consequence" bucket and flatter the split.
        for sql in [
            "CREATE INDEX i ON t (a)",
            "CREATE VIEW v AS SELECT 1",
            "CREATE FUNCTION f() RETURNS int AS $$ $$ LANGUAGE sql",
            "SELECT 1",
            "",
        ] {
            assert_eq!(created_table_name(sql), None, "{sql}");
        }
    }

    #[test]
    fn the_missing_table_name_is_read_from_both_message_shapes() {
        assert_eq!(
            missing_table_name("bind error: unknown table `MinMaxTest1`").as_deref(),
            Some("minmaxtest1")
        );
        assert_eq!(
            missing_table_name("bind error: DROP TABLE: no such table `t3`").as_deref(),
            Some("t3")
        );
        assert_eq!(missing_table_name("bind error: unknown column `x`"), None);
    }

    #[test]
    fn an_unknown_function_keeps_its_name_while_everything_else_is_shaped() {
        // The REAL message shape, prefix and all — the first version of this
        // test used a stripped message and passed while the code did nothing.
        assert_eq!(
            cause_key(
                "bind error: unknown function `pg_advisory_xact_lock()`; available: lower, upper"
            ),
            "unknown function `pg_advisory_xact_lock()`"
        );
        assert_eq!(
            cause_key("bind error: unknown function `unnest()`; available: lower"),
            "unknown function `unnest()`"
        );
        assert_eq!(
            cause_key("SQL parse error at byte 22: unexpected trailing input `Ident(\"cascade\")`"),
            "unexpected trailing input `Ident(\"cascade\")`"
        );
        // The table-function bucket keeps its name for the same reason — three
        // different jobs (an array type, a join-side row source, a corpus-local
        // function nobody will implement) were one line.
        assert_eq!(
            cause_key(
                "SQL parse error at byte 14: `unnest(…)` is a table function in FROM \
                 position, and mpedb has no table-function planner — it produces rows \
                 from an array or JSON value"
            ),
            "table function in FROM `unnest(…)`"
        );
        assert_eq!(
            cause_key(
                "SQL parse error at byte 14: `generate_series(…)` is a table function in \
                 FROM position, and mpedb has no table-function planner — mpedb generates \
                 it in the FIRST `FROM` position only"
            ),
            "table function in FROM `generate_series(…)`"
        );
        // Everything else still collapses, or the list is 20 000 lines long.
        assert_eq!(
            cause_key("bind error: unknown table `minmaxtest1`"),
            cause_key("bind error: unknown table `other`")
        );
    }

    /// The progress counter counts FRAMES, not bytes that look like frames.
    ///
    /// The claim in `Pipe::write` is that a `Z` inside a row VALUE must not be
    /// counted, because a data-dependent progress counter would name the wrong
    /// statement exactly when a hang makes it matter. Asserted rather than
    /// asserted-in-a-comment.
    #[test]
    fn the_hang_watchdogs_progress_counter_counts_frames_not_bytes() {
        use std::io::Write as _;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut p = Pipe {
            input: std::io::Cursor::new(Vec::new()),
            output: Vec::new(),
            done: done.clone(),
            scanned: 0,
        };
        let frame = |tag: u8, body: &[u8]| {
            let mut v = vec![tag];
            v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
            v.extend_from_slice(body);
            v
        };
        // A DataRow whose single value is the text "Z" — one byte that would
        // fool a scanner and must not fool this one.
        let mut d = 1i16.to_be_bytes().to_vec();
        d.extend_from_slice(&1i32.to_be_bytes());
        d.push(b'Z');
        p.write_all(&frame(b'D', &d)).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 0);

        // Now a real ReadyForQuery.
        p.write_all(&frame(b'Z', b"I")).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 1);

        // And one arriving SPLIT across two writes — the wire does that, and a
        // walker that restarted or gave up on a partial frame would either
        // miscount or count twice.
        let z = frame(b'Z', b"I");
        p.write_all(&z[..3]).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 1);
        p.write_all(&z[3..]).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn explain_is_recognised_and_a_column_called_explain_is_not() {
        assert!(is_explain("EXPLAIN SELECT 1"));
        assert!(is_explain("explain (costs off) select 1"));
        assert!(is_explain("  EXPLAIN ANALYZE SELECT 1"));
        assert!(!is_explain("SELECT explain FROM t"));
        assert!(!is_explain("SELECT 1"));
    }

    #[test]
    fn copy_from_stdin_is_recognised_in_the_spellings_the_corpus_uses() {
        assert!(is_copy_from_stdin("COPY t FROM stdin"));
        assert!(is_copy_from_stdin("copy testnl from stdin csv"));
        assert!(is_copy_from_stdin("COPY t (a, b) FROM STDIN WITH (FORMAT csv)"));
        // A COPY that reads a FILE has no payload to skip.
        assert!(!is_copy_from_stdin("COPY t FROM '/tmp/x.csv'"));
        // …and COPY TO sends data the other way.
        assert!(!is_copy_from_stdin("COPY t TO stdout"));
        assert!(!is_copy_from_stdin("SELECT 1"));
    }

    #[test]
    fn a_dollar_quoted_body_may_contain_semicolons_and_comments() {
        // Function bodies are where both live, and splitting inside one turns a
        // single statement into several syntax errors.
        assert_eq!(
            statements("CREATE FUNCTION f() RETURNS int AS $$BEGIN; -- hi\nEND$$ LANGUAGE x;"),
            vec!["CREATE FUNCTION f() RETURNS int AS $$BEGIN; -- hi\nEND$$ LANGUAGE x"]
        );
    }

    #[test]
    fn an_order_by_at_paren_depth_zero_is_told_apart_from_one_inside() {
        assert!(has_top_level_order_by("SELECT a FROM t ORDER BY a"));
        assert!(has_top_level_order_by("select a from t order by a"));
        // A subquery's ORDER BY does NOT order the outer result — counting it
        // as ordered would put every such statement in the divergence column
        // for a sequence neither engine promised.
        assert!(!has_top_level_order_by(
            "SELECT * FROM (SELECT a FROM t ORDER BY a) s"
        ));
        // A window's OVER (ORDER BY …) is inside parens too.
        assert!(!has_top_level_order_by(
            "SELECT rank() OVER (ORDER BY salary) FROM t"
        ));
        // …but a statement with both keeps the outer one.
        assert!(has_top_level_order_by(
            "SELECT rank() OVER (ORDER BY salary) FROM t ORDER BY empno"
        ));
        assert!(!has_top_level_order_by("SELECT a FROM t"));
        // A string literal containing the words is not a clause.
        assert!(!has_top_level_order_by("SELECT 'order by a' FROM t"));
        // …and `REORDER` is not `ORDER`.
        assert!(!has_top_level_order_by("SELECT reorder FROM t"));
    }

    #[test]
    fn same_rows_in_a_different_order_is_agreement_only_when_no_order_was_asked_for() {
        // The case this exists for: two engines scan a table in different
        // physical orders. SQL promises nothing about that, so both are right.
        let a = "b|2\na|1";
        let b = "a|1\nb|2";
        assert!(matches!(
            compare(a, b, "SELECT x, y FROM t"),
            Verdict::OrderOnly
        ));
        // But if the statement DID ask for an order, getting it wrong is a bug.
        assert!(matches!(
            compare(a, b, "SELECT x, y FROM t ORDER BY x"),
            Verdict::Different
        ));
        // Identical output is a plain match either way.
        assert!(matches!(compare(a, a, "SELECT x FROM t"), Verdict::Same));
        // Different ROWS are different, order rule or not.
        assert!(matches!(
            compare("a|1", "a|2", "SELECT x FROM t"),
            Verdict::Different
        ));
        // A different row COUNT is never an ordering question.
        assert!(matches!(
            compare("a|1\nb|2", "a|1", "SELECT x FROM t"),
            Verdict::Different
        ));
    }

    /// One empty row is not no rows.
    ///
    /// psql spells the difference exactly — `SELECT ''` prints `"\n"` and
    /// `SELECT 1 WHERE false` prints `""` — and the old normaliser rejoined
    /// rows with `"\n"`, which maps both to `""`. The harness then scored
    /// them as a MATCH. Narrow, and in the one direction that matters: it
    /// could only ever turn a wrong answer into agreement, never the reverse.
    #[test]
    fn a_single_empty_row_does_not_match_no_rows_at_all() {
        assert!(matches!(compare("\n", "", "SELECT ''"), Verdict::Different));
        assert!(matches!(compare("", "\n", "SELECT ''"), Verdict::Different));
        // Two empty rows against one is the same question one row further on.
        assert!(matches!(compare("\n\n", "\n", "SELECT ''"), Verdict::Different));
        // …and the trailing newline psql always writes is still not an answer.
        assert!(matches!(compare("a|1\n", "a|1", "SELECT x FROM t"), Verdict::Same));
        assert!(matches!(compare("", "", "SELECT 1 WHERE false"), Verdict::Same));
    }

    #[test]
    fn the_baseline_round_trips_and_any_movement_is_reported() {
        let dir = mpedb_testkit::scratch_base().join(format!("pgregress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.tsv");
        let a = vec![(
            "int4".to_string(),
            Counts {
                matched: 10,
                order_only: 0,
                both_refused: 2,
                refused: 1,
                diverged: 0,
            },
        )];
        write_baseline_tsv(&path, &a).unwrap();
        assert_eq!(compare_baseline(&path, &a).unwrap(), 0);

        // An IMPROVEMENT is movement too, and must be reported. A number that
        // drifts upward unnoticed is how a regression hides next to a win.
        let better = vec![(
            "int4".to_string(),
            Counts {
                matched: 11,
                order_only: 0,
                both_refused: 2,
                refused: 0,
                diverged: 0,
            },
        )];
        assert_eq!(compare_baseline(&path, &better).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_missing_from_the_baseline_counts_as_movement() {
        let dir = mpedb_testkit::scratch_base().join(format!("pgregress2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.tsv");
        write_baseline_tsv(&path, &[]).unwrap();
        let got = vec![("new_file".to_string(), Counts::default())];
        assert_eq!(compare_baseline(&path, &got).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
