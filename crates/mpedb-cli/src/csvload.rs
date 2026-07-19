//! `mpedb data.db people.csv` — a CSV named as the second argument is not SQL,
//! so it is offered as a CHOICE: import it into the database as a table, or
//! analyse it in memory and write nothing at all.
//!
//! The whole module is built around one asymmetry: **analysis is free, import
//! is permanent**. So analysis is the default everywhere a decision has to be
//! made without a human present (piped stdin, `2>/dev/null`, a test harness),
//! and import only ever happens because someone said so — interactively at the
//! prompt, or with `--import` on the command line. That also keeps the
//! lazy-create rule that ships next door intact: analysis is a READ and creates
//! nothing, import is a WRITE and materializes the database.
//!
//! mpedb has a RIGID schema, so a CSV cannot be loaded without first committing
//! to column types. Inference is deliberately timid ([`infer`]): a column is
//! `int64`/`float64` only when EVERY non-empty value in it is unambiguously
//! that, and `text` — which can never be wrong — otherwise. Leading zeros
//! (`007`, `01234` zip codes, phone numbers) are text, not integers: that is
//! the single most common way a CSV loader loses data, and it is one `is_int`
//! branch to avoid.

use std::path::Path;

use mpedb::Value;

use crate::util::{runtime, CliResult, Failure};

/// Extensions that make an argument a data file rather than a SQL statement.
/// Deliberately short: `.txt` is not on it, because a `.txt` argument is far
/// more likely to be something else.
const CSV_EXTS: &[&str] = &["csv", "tsv"];

/// Does this argument name an existing CSV/TSV file? Both halves matter — a
/// SQL statement never names an existing file, and a nonexistent path is not
/// data we could load, so it stays a (failing) statement.
pub fn looks_like_csv(arg: &str) -> bool {
    let p = Path::new(arg);
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CSV_EXTS.iter().any(|c| e.eq_ignore_ascii_case(c)))
}

// ---------------------------------------------------------------------------
// RFC4180 reader
// ---------------------------------------------------------------------------

/// Split `src` into records by RFC4180 rules: `"` quotes a field, `""` inside a
/// quoted field is a literal quote, and a quoted field may contain the
/// delimiter and newlines. CR/LF, LF and CRLF all end a record; a final newline
/// does NOT produce a trailing empty record.
///
/// Anything the grammar does not cover is taken literally rather than refused —
/// a bare `"` in the middle of an unquoted field is just a quote character, as
/// every spreadsheet treats it. The only hard error is EOF inside a quoted
/// field, which is genuinely truncated data.
pub fn read_records(src: &str, delim: char) -> Result<Vec<Vec<String>>, String> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = src.chars().peekable();
    // Has anything at all been seen since the last record ended? Distinguishes
    // "a record of one empty field" from "the file ended with a newline".
    let mut started = false;

    while let Some(c) = chars.next() {
        started = true;
        if c == '"' && field.is_empty() {
            // Quoted field: consume to the closing quote, doubling `""`.
            loop {
                match chars.next() {
                    None => return Err("unterminated quoted field at end of file".into()),
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            field.push('"');
                        } else {
                            break;
                        }
                    }
                    Some(ch) => field.push(ch),
                }
            }
            // Anything between the closing quote and the delimiter/newline is
            // junk; append it rather than losing it.
            while let Some(&ch) = chars.peek() {
                if ch == delim || ch == '\n' || ch == '\r' {
                    break;
                }
                field.push(ch);
                chars.next();
            }
            continue;
        }
        if c == delim {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            out.push(std::mem::take(&mut row));
            started = false;
        } else {
            field.push(c);
        }
    }
    if started {
        row.push(field);
        out.push(row);
    }
    // A file of blank lines carries no records.
    out.retain(|r| !(r.len() == 1 && r[0].is_empty()));
    Ok(out)
}

/// Guess the delimiter by parsing the file with each candidate and keeping the
/// one that yields the most CONSISTENT rectangle: every record the same width,
/// and that width greater than one. Ties go to the wider result, then to the
/// earlier candidate — so a plain comma file stays a comma file.
fn sniff_delim(src: &str) -> char {
    // Only the first few records decide; a 200 MB file need not be parsed four
    // times to find its delimiter.
    const HEAD: usize = 64 * 1024;
    let head: String = src.char_indices().take_while(|(i, _)| *i < HEAD).map(|(_, c)| c).collect();
    let whole = head.len() == src.len();
    let mut best = (',', 0usize, false);
    // A delimiter that IS in the file but leaves it unparseable — the classic
    // unterminated quote.
    let mut broken: Option<char> = None;
    for &d in &[',', '\t', ';', '|'] {
        let rows = match read_records(&head, d) {
            Ok(r) => r,
            Err(_) => {
                if broken.is_none() && head.contains(d) {
                    broken = Some(d);
                }
                continue;
            }
        };
        let rows: Vec<_> = rows.into_iter().take(20).collect();
        let Some(w) = rows.first().map(Vec::len) else {
            continue;
        };
        // The last record of a truncated head may be cut in half; ignore it.
        let body = if rows.len() > 1 { &rows[..rows.len() - 1] } else { &rows[..] };
        let uniform = w > 1 && body.iter().all(|r| r.len() == w);
        if (uniform, w) > (best.2, best.1) {
            best = (d, w, uniform);
        }
    }
    // Nothing produced a real table, but something produced a real error: hand
    // back the broken one so the caller REPORTS it. Pretending a comma file
    // with a runaway quote is a one-column tab file would silently glue every
    // row into one cell — the loudest possible way to lose data quietly. Only
    // when the head is the whole file, since a cut-off head can invent the
    // error all by itself.
    if whole && !best.2 && best.1 <= 1 {
        if let Some(d) = broken {
            return d;
        }
    }
    best.0
}

// ---------------------------------------------------------------------------
// Type inference
// ---------------------------------------------------------------------------

/// Is `s` an integer we are willing to STORE as one? Canonical form only:
/// optional `-`, then digits with no leading zero unless the value is exactly
/// `0`. `007`, `+7`, `1 000` and `1e3` are all text — an inferred type that
/// silently rewrites the user's bytes is worse than a text column.
fn is_int(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if d.len() > 1 && d.starts_with('0') {
        return false;
    }
    s.parse::<i64>().is_ok()
}

/// Is `s` a float we are willing to store as one? Same leading-zero rule on the
/// integer part (`0.5` is fine, `00.5` is not), a mandatory digit on each side
/// of the point, optional exponent. `inf`/`NaN` are text.
fn is_float(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || (int.len() > 1 && int.starts_with('0')) {
        return false;
    }
    if let Some(f) = frac {
        if !digits(f) {
            return false;
        }
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if !digits(e) {
            return false;
        }
    }
    // Must have SOMETHING beyond a bare integer, or `is_int` owns it.
    if frac.is_none() && exp.is_none() {
        return false;
    }
    s.parse::<f64>().is_ok_and(f64::is_finite)
}

/// The column type a set of raw fields commits to. Empty fields are NULL and
/// constrain nothing; a column that is entirely empty is `text`.
fn infer<'a>(vals: impl Iterator<Item = &'a str> + Clone) -> mpedb::ColumnType {
    use mpedb::ColumnType::*;
    let mut any = false;
    let mut all_int = true;
    let mut all_num = true;
    for v in vals.clone() {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        any = true;
        if !is_int(v) {
            all_int = false;
            if !is_float(v) {
                all_num = false;
                break;
            }
        }
    }
    if !any {
        Text
    } else if all_int {
        Int64
    } else if all_num {
        Float64
    } else {
        Text
    }
}

// ---------------------------------------------------------------------------
// CSV → table plan
// ---------------------------------------------------------------------------

/// One column of the table a CSV will become.
pub struct CsvColumn {
    pub name: String,
    pub ty: mpedb::ColumnType,
    pub nullable: bool,
}

/// A CSV read, typed, and named — everything needed to create the table and
/// stream the rows in, and nothing about WHERE they go.
pub struct CsvTable {
    pub table: String,
    pub columns: Vec<CsvColumn>,
    /// Ordinal of the primary-key column in `columns`.
    pub pk: usize,
    /// `true` when `columns[pk]` was SYNTHESIZED (1..n) rather than taken from
    /// the file — worth saying out loud, since it appears in `SELECT *`.
    pub pk_synthetic: bool,
    /// Row values, already coerced to `columns`' types and parallel to them.
    pub rows: Vec<Vec<Value>>,
    pub delim: char,
    pub header: bool,
}

/// Would this row of strings be a HEADER? Yes when every field is a non-empty,
/// case-insensitively distinct label that is not itself a number — which is
/// what a header row is and what a data row essentially never is. A file whose
/// first row fails the test keeps all its rows as data and gets `c1..cn` names,
/// so the answer is never "your first row silently vanished".
fn is_header(row: &[String]) -> bool {
    let mut seen: Vec<String> = Vec::new();
    for f in row {
        let f = f.trim();
        if f.is_empty() || is_int(f) || is_float(f) {
            return false;
        }
        let low = f.to_ascii_lowercase();
        if seen.contains(&low) {
            return false;
        }
        seen.push(low);
    }
    !row.is_empty()
}

/// A SQL identifier made out of arbitrary CSV text: ASCII alphanumerics and `_`
/// survive, everything else becomes `_`, a leading digit gets a `c` in front,
/// and an empty result falls back to `c<n>`.
fn ident(raw: &str, n: usize) -> String {
    let mut s: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    while s.ends_with('_') && s.len() > 1 {
        s.pop();
    }
    if s.is_empty() || s == "_" {
        return format!("c{n}");
    }
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, 'c');
    }
    s
}

/// `people.csv` → `people`; `2024 sales.tsv` → `c2024_sales`.
fn table_name_for(path: &Path) -> String {
    ident(
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("csv_import"),
        1,
    )
}

/// Make every name distinct (case-insensitively), appending `_2`, `_3`, … —
/// mpedb refuses duplicate column names, and a CSV with two `id` columns is a
/// real thing.
fn dedupe(names: &mut [String]) {
    for i in 0..names.len() {
        let mut n = 1;
        while names[..i].iter().any(|p| p.eq_ignore_ascii_case(&names[i])) {
            n += 1;
            let base = names[i].trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches('_');
            names[i] = format!("{base}_{n}");
        }
    }
}

/// Read `path` and decide everything about the table it becomes.
pub fn plan(path: &Path, table_override: Option<&str>) -> Result<CsvTable, Failure> {
    let bytes = std::fs::read(path)
        .map_err(|e| Failure::Runtime(format!("cannot read {}: {e}", path.display())))?;
    // Strip a UTF-8 BOM (Excel writes one) and accept lossy bytes rather than
    // refusing a Latin-1 file outright.
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
    let delim = sniff_delim(&text);
    let records = read_records(&text, delim)
        .map_err(|e| Failure::Runtime(format!("{}: {e}", path.display())))?;
    if records.is_empty() {
        return Err(Failure::Runtime(format!(
            "{} is empty — there is no table in it",
            path.display()
        )));
    }

    let header = is_header(&records[0]);
    let width = records[0].len();
    let mut names: Vec<String> = if header {
        records[0].iter().enumerate().map(|(i, f)| ident(f, i + 1)).collect()
    } else {
        (1..=width).map(|i| format!("c{i}")).collect()
    };
    dedupe(&mut names);

    // Ragged rows: a SHORT row is padded (a trailing empty field is the usual
    // cause and is harmless), a LONG one is an error — truncating it would
    // silently drop data, which is the one thing an importer must not do.
    let body = if header { &records[1..] } else { &records[..] };
    let mut raw: Vec<Vec<String>> = Vec::with_capacity(body.len());
    for (i, r) in body.iter().enumerate() {
        if r.len() > width {
            return Err(Failure::Runtime(format!(
                "{}: row {} has {} fields but the table has {width} columns",
                path.display(),
                i + 1 + usize::from(header),
                r.len()
            )));
        }
        let mut r = r.clone();
        r.resize(width, String::new());
        raw.push(r);
    }

    let mut columns: Vec<CsvColumn> = (0..width)
        .map(|c| CsvColumn {
            name: names[c].clone(),
            ty: infer(raw.iter().map(|r| r[c].as_str())),
            nullable: raw.iter().any(|r| r[c].trim().is_empty()),
        })
        .collect();

    // The primary key. mpedb requires one, so there are exactly two outcomes:
    // the file's FIRST column when it can serve (int or text, never empty,
    // no duplicates — the shape of every `id,…` export), or a synthesized
    // 1..n counter. Only the first column is considered: a PK is a claim about
    // what a row IS, and hunting for one in column 7 would be guessing.
    let first_usable = !raw.is_empty()
        && matches!(columns[0].ty, mpedb::ColumnType::Int64 | mpedb::ColumnType::Text)
        && !columns[0].nullable
        && {
            let mut seen: Vec<&str> = raw.iter().map(|r| r[0].trim()).collect();
            seen.sort_unstable();
            let n = seen.len();
            seen.dedup();
            seen.len() == n
        };
    let (pk, pk_synthetic) = if first_usable {
        (0, false)
    } else {
        let mut n = "rowid".to_string();
        while columns.iter().any(|c| c.name.eq_ignore_ascii_case(&n)) {
            n.insert(0, '_');
        }
        columns.insert(
            0,
            CsvColumn { name: n, ty: mpedb::ColumnType::Int64, nullable: false },
        );
        (0, true)
    };

    let rows: Vec<Vec<Value>> = raw
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut v: Vec<Value> = Vec::with_capacity(columns.len());
            if pk_synthetic {
                v.push(Value::Int(i as i64 + 1));
            }
            for (c, cell) in r.iter().enumerate() {
                let col = &columns[c + usize::from(pk_synthetic)];
                v.push(cell_value(cell, col.ty));
            }
            v
        })
        .collect();

    Ok(CsvTable {
        table: table_override.map_or_else(|| table_name_for(path), |t| ident(t, 1)),
        columns,
        pk,
        pk_synthetic,
        rows,
        delim,
        header,
    })
}

/// One CSV field as a typed value. An empty field is NULL in every type — the
/// classic CSV ambiguity, resolved the way every other loader resolves it and
/// stated in the summary so nobody has to guess.
fn cell_value(raw: &str, ty: mpedb::ColumnType) -> Value {
    let t = raw.trim();
    if t.is_empty() {
        return Value::Null;
    }
    match ty {
        mpedb::ColumnType::Int64 => t.parse::<i64>().map_or(Value::Null, Value::Int),
        mpedb::ColumnType::Float64 => t.parse::<f64>().map_or(Value::Null, Value::Float),
        // Only TEXT keeps the field VERBATIM: trimming is a typing decision, and
        // for a text column there is no type to decide.
        _ => Value::Text(raw.to_owned()),
    }
}

impl CsvTable {
    fn sql_type(ty: mpedb::ColumnType) -> &'static str {
        ty.name()
    }

    /// sqlite's three storage classes, for the `CREATE TABLE` that goes into a
    /// sqlite base.
    fn sqlite_type(ty: mpedb::ColumnType) -> &'static str {
        match ty {
            mpedb::ColumnType::Int64 => "INTEGER",
            mpedb::ColumnType::Float64 => "REAL",
            _ => "TEXT",
        }
    }

    /// The `CREATE TABLE` for either engine — the type words are the only
    /// difference, and mpedb accepts sqlite's spellings too, so `sqlite = true`
    /// is really "write it the way a sqlite tool would".
    fn create_sql(&self, sqlite: bool) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let ty = if sqlite { Self::sqlite_type(c.ty) } else { Self::sql_type(c.ty) };
                let mut s = format!("\"{}\" {ty}", c.name.replace('"', "\"\""));
                if i == self.pk {
                    s.push_str(" PRIMARY KEY");
                } else if !c.nullable {
                    s.push_str(" NOT NULL");
                }
                s
            })
            .collect();
        format!("CREATE TABLE \"{}\" ({})", self.table.replace('"', "\"\""), cols.join(", "))
    }

    /// One line saying what was found, so the choice below is an informed one.
    pub fn summary(&self, src: &Path) -> String {
        let named = if self.header { "header row" } else { "no header row — columns named c1.." };
        let d = match self.delim {
            '\t' => "tab".to_string(),
            c => format!("`{c}`"),
        };
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| format!("{} {}", c.name, Self::sql_type(c.ty)))
            .collect();
        let pk = if self.pk_synthetic {
            format!(", synthesized primary key `{}`", self.columns[self.pk].name)
        } else {
            format!(", primary key `{}`", self.columns[self.pk].name)
        };
        format!(
            "{}: {} row{} x {} columns ({d}-delimited, {named}{pk})\n  {} ({})",
            src.display(),
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" },
            self.columns.len(),
            self.table,
            cols.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Create the table in an open mpedb database and stream the rows in through
/// the typed row API — one transaction, no SQL per row.
pub fn load_native(db: &mpedb::Database, t: &CsvTable) -> CliResult {
    if db.schema().tables.iter().any(|x| x.name.eq_ignore_ascii_case(&t.table)) {
        return exists_err(&t.table);
    }
    db.query(&t.create_sql(false), &[])?;
    let tid = db
        .schema()
        .tables
        .iter()
        .position(|x| x.name.eq_ignore_ascii_case(&t.table))
        .ok_or_else(|| Failure::Runtime(format!("table `{}` vanished after CREATE", t.table)))?
        as u32;
    let mut s = db.begin()?;
    for row in &t.rows {
        s.insert_row(tid, row)?;
    }
    s.commit()?;
    Ok(())
}

/// The same for a sqlite base: sqlite's own `CREATE TABLE` plus one prepared
/// `INSERT`, inside one transaction. Importing into the BASE (rather than
/// through the overlay) is the point — the table has to be there for every
/// other sqlite tool, exactly as `sqlite3 .import` leaves it.
pub fn load_sqlite(base: &Path, t: &CsvTable) -> CliResult {
    let mut conn = rusqlite::Connection::open(base)
        .map_err(|e| Failure::Runtime(format!("open {}: {e}", base.display())))?;
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [&t.table],
            |r| r.get(0),
        )
        .map_err(|e| Failure::Runtime(format!("read {}: {e}", base.display())))?;
    if n > 0 {
        return exists_err(&t.table);
    }
    let tx = conn.transaction().map_err(|e| Failure::Runtime(e.to_string()))?;
    tx.execute_batch(&t.create_sql(true))
        .map_err(|e| Failure::Runtime(e.to_string()))?;
    {
        let holes: Vec<String> = (1..=t.columns.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT INTO \"{}\" VALUES ({})",
            t.table.replace('"', "\"\""),
            holes.join(", ")
        );
        let mut st = tx.prepare(&sql).map_err(|e| Failure::Runtime(e.to_string()))?;
        for row in &t.rows {
            let vals: Vec<Box<dyn rusqlite::ToSql>> = row
                .iter()
                .map(|v| -> Box<dyn rusqlite::ToSql> {
                    match v {
                        Value::Int(i) => Box::new(*i),
                        Value::Float(f) => Box::new(*f),
                        Value::Text(s) => Box::new(s.clone()),
                        _ => Box::new(rusqlite::types::Null),
                    }
                })
                .collect();
            st.execute(rusqlite::params_from_iter(vals.iter().map(|b| b.as_ref())))
                .map_err(|e| Failure::Runtime(e.to_string()))?;
        }
    }
    tx.commit().map_err(|e| Failure::Runtime(e.to_string()))?;
    Ok(())
}

/// Refusing to overwrite is not an inconvenience to be worked around — an
/// import that lands on an existing table has no correct behaviour, so it has
/// none. `--table` is the way out, and the message says so.
fn exists_err(name: &str) -> CliResult {
    runtime(format!(
        "table `{name}` already exists — import will not overwrite it; \
         choose another name with `--table <name>` (or drop the table first)"
    ))
}

// ---------------------------------------------------------------------------
// The choice
// ---------------------------------------------------------------------------

/// What to do with the CSV.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Import,
    Analyse,
    Quit,
}

/// Ask, but ONLY when there is someone to ask. `--import`/`--analyse` decide
/// outright; otherwise a tty gets the prompt and everything else gets analysis,
/// because analysis writes nothing and a script that meant to write can say so.
///
/// Note the tty test is on stdin: the analysis session that follows READS stdin,
/// so "is there a human at the keyboard" and "can I ask a question" are the same
/// question. A prompt fired down a pipe would consume the caller's SQL and then
/// hang the session that was supposed to run it.
pub fn choose(forced: Option<Action>, t: &CsvTable, target: &Path, src: &Path) -> Action {
    if let Some(a) = forced {
        return a;
    }
    eprintln!("{}", t.summary(src));
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        eprintln!(
            "no tty: analysing in memory (nothing is written). \
             Use --import to load it into {} instead.",
            target.display()
        );
        return Action::Analyse;
    }
    loop {
        eprint!(
            "  [i] import into {}   [a] analyse in memory (writes nothing)   [q] quit\nchoice [a]: ",
            target.display()
        );
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => return Action::Analyse, // EOF: the safe one
            Ok(_) => {}
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "a" | "analyse" | "analyze" => return Action::Analyse,
            "i" | "import" => return Action::Import,
            "q" | "quit" | "exit" => return Action::Quit,
            other => eprintln!("  `{other}`? answer i, a or q."),
        }
    }
}

/// `--import` / `--analyse` / `--analyze` / `--table NAME`, removed from `args`.
pub struct CsvFlags {
    pub action: Option<Action>,
    pub table: Option<String>,
}

pub fn take_flags(args: &mut Vec<String>) -> Result<CsvFlags, Failure> {
    let mut action = None;
    let mut table = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--import" => {
                args.remove(i);
                action = Some(Action::Import);
            }
            "--analyse" | "--analyze" => {
                args.remove(i);
                action = Some(Action::Analyse);
            }
            "--table" => {
                args.remove(i);
                if i >= args.len() {
                    return Err(Failure::Usage("--table needs a name".into()));
                }
                table = Some(args.remove(i));
            }
            _ => i += 1,
        }
    }
    Ok(CsvFlags { action, table })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(src: &str) -> Vec<Vec<String>> {
        read_records(src, ',').unwrap()
    }

    #[test]
    fn rfc4180_quoting_survives_commas_quotes_and_newlines() {
        assert_eq!(rows("a,b\n1,2\n"), vec![vec!["a", "b"], vec!["1", "2"]]);
        // Embedded delimiter, doubled quote, embedded newline, CRLF.
        let r = rows("a,b\r\n\"x,y\",\"he said \"\"hi\"\"\"\r\n\"two\nlines\",z\r\n");
        assert_eq!(r[1], vec!["x,y", "he said \"hi\""]);
        assert_eq!(r[2], vec!["two\nlines", "z"]);
        // A trailing newline does not invent a record; blank lines are dropped.
        assert_eq!(rows("a\n\n\n").len(), 1);
        // Empty fields are preserved.
        assert_eq!(rows("a,,c"), vec![vec!["a", "", "c"]]);
        // Truncated quote is the one hard error.
        assert!(read_records("a,\"b", ',').is_err());
    }

    #[test]
    fn the_delimiter_is_sniffed_not_assumed() {
        assert_eq!(sniff_delim("a,b,c\n1,2,3\n"), ',');
        assert_eq!(sniff_delim("a\tb\tc\n1\t2\t3\n"), '\t');
        assert_eq!(sniff_delim("a;b;c\n1;2;3\n"), ';');
        // One column, no delimiter anywhere: comma, and it still parses.
        assert_eq!(sniff_delim("name\nx\ny\n"), ',');
    }

    #[test]
    fn inference_is_timid_where_it_should_be() {
        use mpedb::ColumnType::*;
        let t = |v: &[&str]| infer(v.iter().copied());
        assert_eq!(t(&["1", "-2", "0"]), Int64);
        assert_eq!(t(&["1", "2.5"]), Float64);
        assert_eq!(t(&["1e3", "2.5"]), Float64);
        assert_eq!(t(&["1", "x"]), Text);
        // Empty cells constrain nothing; an all-empty column is text.
        assert_eq!(t(&["1", "", "3"]), Int64);
        assert_eq!(t(&["", ""]), Text);
        // The data-losing cases: leading zeros, plus signs, thousands
        // separators, infinities. All text.
        assert_eq!(t(&["007", "010"]), Text);
        assert_eq!(t(&["+7"]), Text);
        assert_eq!(t(&["1 000"]), Text);
        assert_eq!(t(&["inf", "1.0"]), Text);
        assert_eq!(t(&["0.5", "1.25"]), Float64);
    }

    #[test]
    fn a_header_is_recognized_by_being_labels() {
        assert!(is_header(&["id".into(), "name".into()]));
        assert!(!is_header(&["1".into(), "2".into()]));
        assert!(!is_header(&["id".into(), "".into()]));
        assert!(!is_header(&["id".into(), "ID".into()])); // duplicate
    }

    #[test]
    fn identifiers_are_made_safe_and_distinct() {
        assert_eq!(ident("First Name!", 1), "First_Name");
        assert_eq!(ident("2024", 3), "c2024");
        assert_eq!(ident("", 4), "c4");
        let mut n = vec!["id".to_string(), "ID".to_string(), "id".to_string()];
        dedupe(&mut n);
        assert_eq!(n, vec!["id", "ID_2", "id_3"]);
    }
}
