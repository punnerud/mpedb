//! Interactive line input for the repls: rustyline with Tab completion when
//! stdin is a tty, and the plain `BufRead` loop otherwise.
//!
//! The piped path must stay byte-for-byte what it always was — the CLI tests
//! (and every script) feed SQL on stdin and expect no prompt, no editing, no
//! terminal writes at all. So [`LineSource`] only reaches for rustyline when
//! `isatty(0)`; everything else is the old `stdin().lock().lines()`.
//!
//! Completion knows four things: the repl's own dot-commands (at the start of
//! a line), SQL keywords, table names from the LIVE schema, and — after a
//! `<table>.` qualifier — that table's columns. The schema is shared as
//! `Rc<RefCell<Names>>` and refreshed by the repl after every statement, so a
//! `CREATE TABLE` typed in the session completes on the next line.

use std::cell::RefCell;
use std::rc::Rc;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};

/// SQL words offered by Tab. `GROUP BY`/`ORDER BY` are single candidates —
/// completing `ORD` to `ORDER BY ` is what you actually wanted to type.
const KEYWORDS: &[&str] = &[
    "ALTER TABLE", "AND", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASE", "CAST", "CHECK",
    "COLLATE", "COLUMN", "COMMIT", "CREATE INDEX", "CREATE TABLE", "CREATE TRIGGER",
    "CREATE UNIQUE INDEX", "CREATE VIEW", "CREATE VIRTUAL TABLE", "CROSS JOIN", "DEFAULT",
    "DELETE FROM", "DESC", "DISTINCT", "DO NOTHING", "DROP INDEX", "DROP TABLE", "DROP VIEW",
    "ELSE", "END", "EXISTS", "EXPLAIN", "FALSE", "FROM", "GLOB", "GROUP BY", "HAVING",
    "IF NOT EXISTS", "IN", "INNER JOIN", "INSERT INTO", "INTO", "IS", "IS NOT", "JOIN",
    "LEFT JOIN", "LIKE", "LIMIT", "MATCH", "NOT", "NOT NULL", "NULL", "OFFSET", "ON",
    "ON CONFLICT", "OR", "ORDER BY", "PRIMARY KEY", "REGEXP", "RENAME TO", "RETURNING",
    "ROLLBACK", "SELECT", "SET", "THEN", "TRUE", "UNIQUE", "UPDATE", "VALUES", "WHEN",
    "WHERE",
];

/// Table names and their columns, plus the repl's dot-commands. Owned by the
/// repl, borrowed by the completer.
#[derive(Default)]
pub struct Names {
    pub tables: Vec<(String, Vec<String>)>,
    pub dots: Vec<&'static str>,
}

impl Names {
    pub fn new(dots: &[&'static str]) -> Names {
        Names {
            tables: Vec::new(),
            dots: dots.to_vec(),
        }
    }

    /// Replace the table/column snapshot (called after every statement — DDL
    /// changes it).
    pub fn set_schema(&mut self, schema: &mpedb::Schema) {
        self.tables = schema
            .tables
            .iter()
            .filter(|t| !t.dead && !t.name.is_empty())
            .map(|t| {
                (
                    t.name.clone(),
                    t.columns.iter().map(|c| c.name.clone()).collect(),
                )
            })
            .collect();
    }
}

/// Is `c` part of the word Tab completes? `.` is included so `.tables` and
/// `users.id` are each ONE word — the two cases the completer then splits.
fn word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'$'
}

/// The completion itself, as a pure function of (names, line, cursor): the
/// byte offset where the replacement starts, and the candidates. Unit-tested
/// below; rustyline only adapts it.
pub fn complete_at(names: &Names, line: &str, pos: usize) -> (usize, Vec<String>) {
    let head = &line[..pos];
    let bytes = head.as_bytes();
    let mut start = pos;
    while start > 0 && word_char(bytes[start - 1]) {
        start -= 1;
    }
    let word = &head[start..];

    // `.help` — a dot-command, but only as the first word of the line.
    if let Some(rest) = word.strip_prefix('.') {
        if head[..start].trim().is_empty() {
            let hits = names
                .dots
                .iter()
                .filter(|d| d.strip_prefix('.').is_some_and(|n| starts_ci(n, rest)))
                .map(|d| format!("{d} "))
                .collect();
            return (start, hits);
        }
    }

    // `users.ema` — columns of that table.
    if let Some(dot) = word.rfind('.') {
        let (table, prefix) = (&word[..dot], &word[dot + 1..]);
        let hits = names
            .tables
            .iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(table))
            .map(|(_, cols)| {
                cols.iter()
                    .filter(|c| starts_ci(c, prefix))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        return (start + dot + 1, hits);
    }

    // A bare word: table names first (the thing you are most likely typing),
    // then keywords, cased to match what you typed.
    let mut hits: Vec<String> = names
        .tables
        .iter()
        .filter(|(t, _)| starts_ci(t, word))
        .map(|(t, _)| t.clone())
        .collect();
    let lower = !word.is_empty() && word.bytes().all(|b| !b.is_ascii_uppercase());
    hits.extend(
        KEYWORDS
            .iter()
            .filter(|k| starts_ci(k, word))
            .map(|k| format!("{} ", if lower { k.to_ascii_lowercase() } else { (*k).to_owned() })),
    );
    (start, hits)
}

fn starts_ci(hay: &str, prefix: &str) -> bool {
    hay.len() >= prefix.len() && hay.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The rustyline helper: completion only — no hints, no highlighting, no
/// multi-line validation (statements are one line, as they always were).
pub struct SqlHelper {
    names: Rc<RefCell<Names>>,
}

impl Completer for SqlHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(complete_at(&self.names.borrow(), line, pos))
    }
}

impl Hinter for SqlHelper {
    type Hint = String;
}
impl Highlighter for SqlHelper {}
impl Validator for SqlHelper {}
impl Helper for SqlHelper {}

/// Where a repl's lines come from. `Piped` is the historical path, unchanged.
pub enum LineSource {
    Piped(std::io::Lines<std::io::StdinLock<'static>>),
    Interactive {
        editor: Box<Editor<SqlHelper, DefaultHistory>>,
        prompt: String,
    },
}

impl LineSource {
    /// Build the input source for a repl: rustyline (with `names` driving Tab)
    /// on a tty, the plain line reader otherwise. A terminal that rustyline
    /// cannot take over falls back to the plain reader rather than failing.
    pub fn new(prompt: &str, names: Rc<RefCell<Names>>) -> LineSource {
        use std::io::BufRead as _;
        let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
        if interactive {
            let config = Config::builder()
                .completion_type(CompletionType::List)
                .auto_add_history(true)
                .build();
            if let Ok(mut editor) = Editor::<SqlHelper, DefaultHistory>::with_config(config) {
                editor.set_helper(Some(SqlHelper { names }));
                return LineSource::Interactive {
                    editor: Box::new(editor),
                    prompt: prompt.to_owned(),
                };
            }
        }
        LineSource::Piped(std::io::stdin().lock().lines())
    }

    /// The next input line, or `None` at end of input. Ctrl-C abandons the
    /// line and continues (an empty line); Ctrl-D ends the session.
    pub fn next_line(&mut self) -> Option<std::io::Result<String>> {
        match self {
            LineSource::Piped(lines) => lines.next(),
            LineSource::Interactive { editor, prompt } => match editor.readline(prompt.as_str()) {
                Ok(l) => Some(Ok(l)),
                Err(ReadlineError::Interrupted) => Some(Ok(String::new())),
                Err(ReadlineError::Eof) => None,
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            },
        }
    }

    /// True when this source prints its own prompt (rustyline does; the piped
    /// reader never did).
    pub fn prompts(&self) -> bool {
        matches!(self, LineSource::Interactive { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        let mut n = Names::new(&[".help", ".tables", ".schema", ".quit"]);
        n.tables = vec![
            ("users".into(), vec!["id".into(), "email".into()]),
            ("orders".into(), vec!["id".into(), "user_id".into()]),
        ];
        n
    }

    fn hits(line: &str) -> Vec<String> {
        complete_at(&names(), line, line.len()).1
    }

    #[test]
    fn dot_commands_complete_only_at_the_start_of_a_line() {
        assert_eq!(hits(".ta"), vec![".tables ".to_string()]);
        assert_eq!(hits("   .sch"), vec![".schema ".to_string()]);
        // Not a dot-command in statement position: `x.` is a qualifier.
        assert!(hits("SELECT .ta").is_empty());
    }

    #[test]
    fn qualified_prefix_completes_that_tables_columns() {
        let (start, h) = complete_at(&names(), "SELECT users.em", 15);
        assert_eq!(start, "SELECT users.".len());
        assert_eq!(h, vec!["email".to_string()]);
        // Unknown table → nothing invented.
        assert!(hits("SELECT nope.i").is_empty());
        // Bare qualifier lists every column.
        assert_eq!(hits("SELECT orders."), vec!["id".to_string(), "user_id".to_string()]);
    }

    #[test]
    fn bare_words_complete_tables_then_keywords_matching_the_typed_case() {
        assert_eq!(hits("SELECT * FROM us"), vec!["users".to_string()]);
        assert_eq!(hits("SEL"), vec!["SELECT ".to_string()]);
        assert_eq!(hits("sel"), vec!["select ".to_string()]);
        assert!(hits("ORD").contains(&"ORDER BY ".to_string()));
        // A table whose name shares a prefix with a keyword yields both.
        let h = hits("o");
        assert!(h.contains(&"orders".to_string()) && h.contains(&"on ".to_string()));
    }

    #[test]
    fn the_replacement_starts_at_the_word_not_the_line() {
        let (start, _) = complete_at(&names(), "INSERT INTO us", 14);
        assert_eq!(start, "INSERT INTO ".len());
        let (start, h) = complete_at(&names(), "", 0);
        assert_eq!(start, 0);
        assert!(!h.is_empty(), "an empty line offers every name");
    }
}
