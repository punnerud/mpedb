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
//!
//! Tab on an EMPTY line is different: instead of dumping every name it opens a
//! [`picker`] — a list of the database's tables you walk with the arrow keys and
//! choose from with Enter. That is the one thing `sqlite3` has no answer for
//! ("what is even in this file?"), and an empty prompt is the only place it can
//! be offered without guessing, because there is no half-typed word to complete
//! and therefore no intent to override.
//!
//! It needs no new dependency and no TUI crate. rustyline calls
//! [`Completer::complete`] with the terminal ALREADY in raw mode, so the picker
//! reads keys straight from fd 0 (unbuffered `libc::read`, so nothing is stolen
//! from rustyline's own reader), paints itself with ANSI escapes, erases itself
//! again, and hands back exactly ONE candidate — which rustyline inserts and
//! redraws as if it had been an ordinary completion. An unsupported terminal
//! never reaches the completer at all (rustyline takes `readline_direct`), so
//! there is no dumb-terminal path to guard.

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
    ///
    /// `_mpedb_*` is filtered out: the inert bootstrap table a freshly created
    /// native database is seeded with (a schema with no tables is refused) is an
    /// implementation detail, and offering it as the first entry of the picker —
    /// which is where an empty database's user looks FIRST — would be actively
    /// misleading. Nothing stops you naming it; it is just never suggested.
    pub fn set_schema(&mut self, schema: &mpedb::Schema) {
        self.tables = schema
            .tables
            .iter()
            .filter(|t| !t.dead && !t.name.is_empty() && !t.name.starts_with("_mpedb_"))
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

// ------------------------------------------------------------- table picker

/// One keystroke, as the picker cares about it.
enum Key {
    Up,
    Down,
    Home,
    End,
    Enter,
    Tab,
    Cancel,
    Other,
}

/// Read one byte from stdin, unbuffered. rustyline holds the terminal in raw
/// mode for the whole of `readline`, so this returns a single keypress with no
/// line discipline in the way. `libc::read` rather than `std::io::stdin()`
/// because the latter buffers, and a buffer here would swallow keys rustyline
/// still has to see.
fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return Some(b[0]);
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
}

/// Is another byte available RIGHT NOW? The only way to tell a lone `Esc`
/// (cancel) from the start of an arrow key's `Esc [ A`: a real escape sequence
/// arrives as one burst, a human's Esc does not.
fn byte_waiting(ms: i32) -> bool {
    let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

fn read_key() -> Key {
    match read_byte() {
        None => Key::Cancel,
        Some(b'\r') | Some(b'\n') => Key::Enter,
        Some(b'\t') => Key::Tab,
        // Ctrl-C, Ctrl-G, Ctrl-D, `q`.
        Some(3) | Some(7) | Some(4) | Some(b'q') => Key::Cancel,
        Some(0x1b) => {
            if !byte_waiting(30) {
                return Key::Cancel; // a bare Esc
            }
            match read_byte() {
                Some(b'[') | Some(b'O') => match read_byte() {
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    Some(b'H') => Key::Home,
                    Some(b'F') => Key::End,
                    // `Esc [ 5 ~` and friends: swallow the tilde, ignore.
                    Some(c) if c.is_ascii_digit() => {
                        while byte_waiting(0) {
                            if read_byte() == Some(b'~') {
                                break;
                            }
                        }
                        Key::Other
                    }
                    _ => Key::Other,
                },
                _ => Key::Other,
            }
        }
        Some(b'k') => Key::Up,
        Some(b'j') => Key::Down,
        Some(_) => Key::Other,
    }
}

/// Terminal size, or a conservative 80x24.
fn term_size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
        && ws.ws_col > 0
        && ws.ws_row > 0
    {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 24)
    }
}

fn out(s: &str) {
    use std::io::Write as _;
    let mut o = std::io::stdout();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

/// Cut `s` to `w` display columns (counting chars, which is right for the
/// identifiers and ASCII punctuation this paints) and pad it back out to `w`,
/// so a reverse-video row is a full-width bar.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// The interactive table list. Returns the text to insert, or `None` when the
/// user backed out.
///
/// Contract with the caller: the cursor starts at the end of the (empty) prompt
/// line, and is put back there before returning — the menu is painted BELOW and
/// erased again, so rustyline's own idea of the layout is never disturbed.
fn picker(names: &Names, prompt_cols: usize) -> Option<String> {
    let (cols, rows) = term_size();
    let width = cols.saturating_sub(1).max(20);

    if names.tables.is_empty() {
        out("\r\n\x1b[2m  no tables yet — CREATE TABLE <name> (id INTEGER PRIMARY KEY, …) \
             to make one\x1b[0m\r");
        let _ = read_key();
        restore(1, prompt_cols);
        return None;
    }

    // The list gets whatever vertical room is left after the prompt line and
    // the footer, capped so it never dominates the screen (and, more to the
    // point, never scrolls it — a scroll would move the prompt out from under
    // the cursor arithmetic in `restore`).
    let view = rows.saturating_sub(4).clamp(1, 12).min(names.tables.len());
    let namew = names
        .tables
        .iter()
        .map(|(t, _)| t.chars().count())
        .max()
        .unwrap_or(1)
        .min(28);

    let mut sel = 0usize;
    let mut top = 0usize;
    let mut painted = 0usize;
    loop {
        if sel < top {
            top = sel;
        } else if sel >= top + view {
            top = sel + 1 - view;
        }
        let mut buf = String::new();
        if painted == 0 {
            buf.push_str("\r\n");
        } else {
            // Back to the first painted row.
            buf.push_str(&format!("\x1b[{}A\r", painted - 1));
        }
        let mut lines: Vec<String> = Vec::new();
        for (i, (t, cols_)) in names.tables.iter().enumerate().skip(top).take(view) {
            let more = if i == top && top > 0 {
                "↑"
            } else if i + 1 == top + view && top + view < names.tables.len() {
                "↓"
            } else {
                " "
            };
            let body = format!(
                " {more} {:<namew$}  \x1b[2m{}\x1b[0m",
                t,
                fit(&cols_.join(", "), width.saturating_sub(namew + 6))
            );
            lines.push(if i == sel {
                // Reverse video, with the dim escape stripped so the bar is one
                // solid colour.
                format!("\x1b[7m{}\x1b[0m", fit(&body.replace("\x1b[2m", "").replace("\x1b[0m", ""), width))
            } else {
                body
            });
        }
        lines.push(format!(
            "\x1b[2m   {} table{} · ↑↓ move · Enter SELECT · Tab name · Esc cancel\x1b[0m",
            names.tables.len(),
            if names.tables.len() == 1 { "" } else { "s" }
        ));
        painted = lines.len();
        for (i, l) in lines.iter().enumerate() {
            buf.push_str("\x1b[2K");
            buf.push_str(l);
            buf.push('\r');
            if i + 1 < lines.len() {
                buf.push('\n');
            }
        }
        out(&buf);

        match read_key() {
            Key::Up => sel = sel.saturating_sub(1),
            Key::Down => sel = (sel + 1).min(names.tables.len() - 1),
            Key::Home => sel = 0,
            Key::End => sel = names.tables.len() - 1,
            Key::Enter => {
                restore(painted, prompt_cols);
                // Enter inserts a RUNNABLE statement, not a bare identifier.
                // The picker only ever opens on an EMPTY line, where a lone
                // table name is a syntax error and nothing at all is being
                // overridden — and the statement is left ON the line to edit,
                // not executed. `LIMIT 20` because the question a picker
                // answers is "what is in here", and the honest answer to that
                // is a peek, not a full table scan.
                return Some(format!("SELECT * FROM {} LIMIT 20;", names.tables[sel].0));
            }
            Key::Tab => {
                restore(painted, prompt_cols);
                // The conservative half of the same gesture: just the name, for
                // when the statement you have in mind is not a SELECT.
                return Some(names.tables[sel].0.clone());
            }
            Key::Cancel => {
                restore(painted, prompt_cols);
                return None;
            }
            Key::Other => {}
        }
    }
}

/// Erase `painted` menu rows and put the cursor back where rustyline left it:
/// end of the prompt on the line above. Deliberately does NOT touch the prompt
/// row itself, so a cancelled pick leaves the screen byte-identical to before.
fn restore(painted: usize, prompt_cols: usize) {
    let mut s = String::new();
    if painted > 1 {
        s.push_str(&format!("\x1b[{}A", painted - 1));
    }
    s.push_str("\r\x1b[J\x1b[1A\r");
    if prompt_cols > 0 {
        s.push_str(&format!("\x1b[{prompt_cols}C"));
    }
    out(&s);
}

/// The rustyline helper: completion only — no hints, no highlighting, no
/// multi-line validation (statements are one line, as they always were).
pub struct SqlHelper {
    names: Rc<RefCell<Names>>,
    /// Display width of the prompt, so the picker can put the cursor back.
    prompt_cols: usize,
}

impl Completer for SqlHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        // An EMPTY line has no word to complete, so Tab means "show me what is
        // here" — the browsable picker rather than a wall of every keyword.
        if line[..pos].trim().is_empty() && line.trim().is_empty() {
            let chosen = picker(&self.names.borrow(), self.prompt_cols);
            // No choice → no candidates → rustyline beeps and redraws nothing,
            // which is exactly right: `restore` already put the screen back.
            return Ok((pos, chosen.into_iter().collect()));
        }
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
                editor.set_helper(Some(SqlHelper {
                    names,
                    prompt_cols: prompt.chars().count(),
                }));
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
