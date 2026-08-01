use super::{table_index_rows, table_info_cid, user_tables, IndexRecords};
use mpedb::{Error as DbError, Value};

// ------------------------------------------------------------------ PRAGMA

/// Parse `PRAGMA [<schema>.]<name>[(<arg>)] | <name> = <value>` into
/// `(name, arg)`.
///
/// The SCHEMA qualifier is stripped. sqlite allows one and SQLAlchemy always
/// writes it — `PRAGMA main.table_info("t")` is how it asks whether a table
/// exists before creating it. Taking the leading identifier as the pragma NAME
/// read that as a pragma called `main`, answered nothing, and so told it every
/// table was absent: it created each one a second time and mpedb reported
/// "duplicate table name". 241 failures in SQLAlchemy's dialect suite came from
/// this one missing dot.
///
/// A qualifier other than `main` still resolves against the main schema, which
/// is what this did for every unqualified pragma already. That is a narrower
/// gap than the one it replaces, and a visible one: `PRAGMA temp.table_info`
/// answers for main rather than for the temp schema.
/// The `<schema>.` a pragma was qualified with, if any — `PRAGMA
/// test_schema.table_info(t)` answers about that attached database, not main.
pub fn pragma_schema(sql: &str) -> Option<String> {
    split_pragma_schema(pragma_body(sql)).0
}

/// Everything after the `PRAGMA` keyword.
fn pragma_body(sql: &str) -> &str {
    let rest = sql.trim_start();
    let rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
    rest.trim()
}

/// Split a leading `<schema>.` off a pragma body: `(schema, rest)`.
///
/// The qualifier may be QUOTED — SQLAlchemy writes
/// `PRAGMA "test_schema".table_info("users")`, and reading only a bare
/// identifier there sent 264 of its reflection tests to the main database
/// instead of the attached one they were about.
fn split_pragma_schema(body: &str) -> (Option<String>, &str) {
    let (name, after) = match body.chars().next() {
        Some(q @ ('"' | '`' | '\'')) => match body[1..].find(q) {
            Some(end) => (body[1..1 + end].to_string(), &body[end + 2..]),
            None => return (None, body),
        },
        Some('[') => match body.find(']') {
            Some(end) => (body[1..end].to_string(), &body[end + 1..]),
            None => return (None, body),
        },
        _ => {
            let bare: String = body
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let n = bare.len();
            (bare, &body[n..])
        }
    };
    match (name.is_empty(), after.strip_prefix('.')) {
        (false, Some(rest)) => (Some(name), rest.trim_start()),
        _ => (None, body),
    }
}

pub(crate) fn parse_pragma(sql: &str) -> (String, Option<String>) {
    // Drop the leading `pragma` keyword, then the `<schema>.` in front, if any.
    let (_schema, rest) = split_pragma_schema(pragma_body(sql));
    // Name = leading identifier.
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let after = rest[name.len()..].trim_start();
    let arg = if let Some(a) = after.strip_prefix('(') {
        pragma_arg_body(a).map(|s| unquote(s.trim()))
    } else {
        after.strip_prefix('=').map(|a| unquote(a.trim()))
    };
    (name, arg)
}

/// Everything up to the `)` that CLOSES a pragma's argument list.
///
/// Splitting on the first `)` is wrong whenever the argument is a quoted name
/// that contains one — `PRAGMA table_info("(2)")` stopped at the name's own
/// paren and matched nothing, so a table SQLAlchemy legitimately calls `(2)`
/// reflected as having no columns at all. Quoting is tracked (all four of
/// sqlite's forms, with the doubled-delimiter escape) and so is nesting.
fn pragma_arg_body(a: &str) -> Option<&str> {
    let b = a.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // A doubled delimiter is one literal character, not the
                        // end of the quoted name.
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'[' => {
                // Bracket quoting has no escape; `]` ends it.
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(&a[..i]),
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    // No closing paren: the whole tail is the argument, which is what splitting
    // on `)` also produced.
    Some(a)
}

/// Strip one layer of sqlite quoting from a PRAGMA argument and undo the
/// delimiter's escape (a doubled quote). Bare identifiers are returned as-is.
///
/// CPython's `iterdump` builds `PRAGMA table_info("quoted""table")` for a table
/// whose stored name is `quoted"table`. Stripping the outer quotes without
/// collapsing `""` → `"` left `quoted""table`, which matches nothing and made
/// `table_info` return zero columns — so the dump emitted `VALUES()` with no
/// `quote(col)` terms (`test_table_dump`).
fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 2 {
        return s.to_string();
    }
    let (f, l) = (b[0], b[b.len() - 1]);
    if f == b'[' && l == b']' {
        // Bracket quoting has no escape; `]` ends the name.
        return s[1..s.len() - 1].to_string();
    }
    if (f == b'\'' && l == b'\'') || (f == b'"' && l == b'"') || (f == b'`' && l == b'`') {
        let inner = &s[1..s.len() - 1];
        let delim = f as char;
        // `""` / `''` / ```` inside is one literal delimiter.
        return inner.replace(&format!("{delim}{delim}"), &delim.to_string());
    }
    s.to_string()
}

/// Does `name` name a real TABLE in `schema`? The pragma handler's own test,
/// exposed so the view path can defer to it rather than run a second, drifting
/// one.
pub(crate) fn names_a_table(schema: &mpedb::Schema, name: &str) -> bool {
    find_table(schema, name).is_some()
}

pub(super) fn find_table<'a>(schema: &'a mpedb::Schema, name: &str) -> Option<&'a mpedb::TableDef> {
    user_tables(schema)
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

/// sqlite's spelling of an FK action in `foreign_key_list`.
fn fk_action_name(a: mpedb::FkAction) -> &'static str {
    match a {
        mpedb::FkAction::NoAction => "NO ACTION",
        mpedb::FkAction::Restrict => "RESTRICT",
        mpedb::FkAction::Cascade => "CASCADE",
        mpedb::FkAction::SetNull => "SET NULL",
        mpedb::FkAction::SetDefault => "SET DEFAULT",
    }
}

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// [`cols`] for callers outside this module (the FK pragmas, answered in
/// `lib.rs` because they need the connection).
pub(crate) fn pragma_cols(names: &[&str]) -> Vec<String> {
    cols(names)
}

/// Answer a `PRAGMA` statement. Returns `(columns, rows)`; an unknown pragma is
/// a harmless empty result (matching sqlite's silence for no-op pragmas).
///
/// `busy_timeout_ms` is the connection's live busy timeout, passed in by
/// reference because `PRAGMA busy_timeout = N` is the ONE setter pragma the
/// shim actually HONOURS: it is the same knob `sqlite3_busy_timeout()` sets and
/// the retry loop in `lib.rs` reads. `echo` holds the three that are stored and
/// echoed without being honoured — see [`EchoPragmas`] for the line that
/// separates those from the setters that stay silent no-ops.
/// The connection-local pragmas the shim STORES AND ECHOES.
///
/// The rule these three live under is not "echo whatever the caller sets" —
/// that would be the wrong answer this shim exists to avoid. It is: *echo a
/// setting that implies no behaviour the oracle's own default configuration
/// exhibits; refuse anything else by name.*
///
/// `read_uncommitted` qualifies outright. sqlite honours it only under shared
/// cache, and this shim does not export `sqlite3_enable_shared_cache`, so on
/// the stock 3.45.1 the consumers here link against it is already pure
/// store-and-echo (measured). Echoing it claims exactly nothing.
///
/// `synchronous` and `cache_size` are the harder call, and they moved to this
/// side of the line for a concrete reason: NOT answering was never neutral.
/// sqlite always returns one row, so a caller doing `fetchone()[0]` — Django's
/// `test_init_command` does — got `None` and a TypeError. A crash is not a
/// narrower answer than an echo; it is a worse one. Neither value has an
/// observable consequence a caller can catch us on: they name a page-cache size
/// and an fsync policy for a rollback journal mpedb does not have, and mpedb's
/// real durability is set in its own config and reported by its own surfaces.
pub struct EchoPragmas {
    synchronous: i64,
    cache_size: i64,
    read_uncommitted: i64,
}

impl Default for EchoPragmas {
    /// sqlite 3.45.1's own defaults, measured.
    fn default() -> Self {
        Self { synchronous: 2, cache_size: -2000, read_uncommitted: 0 }
    }
}

/// sqlite's `getSafetyLevel`: a bare integer passes through, the named levels
/// map, and anything unrecognised becomes 1 — its default, NOT an error and
/// NOT the previous value (measured: `PRAGMA synchronous = bogus` yields 1).
fn safety_level(a: &str) -> i64 {
    let a = a.trim();
    if let Ok(n) = a.parse::<i64>() {
        return n;
    }
    match a.to_ascii_lowercase().as_str() {
        "off" | "false" | "no" => 0,
        "full" => 2,
        "extra" => 3,
        _ => 1,
    }
}

/// sqlite's `sqlite3GetBoolean`: any non-zero number is 1, the true-words are
/// 1, everything else — unrecognised included — is 0.
fn pragma_boolean(a: &str) -> i64 {
    let a = a.trim();
    if let Ok(n) = a.parse::<i64>() {
        return i64::from(n != 0);
    }
    i64::from(matches!(a.to_ascii_lowercase().as_str(), "yes" | "true" | "on"))
}

pub fn pragma(
    schema: &mpedb::Schema,
    sql: &str,
    busy_timeout_ms: &mut i32,
    fk_on: &bool,
    idx: &IndexRecords,
    echo: &mut EchoPragmas,
) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    let (name, arg) = parse_pragma(sql);
    match name.to_ascii_lowercase().as_str() {
        // `table_info` HIDES generated columns; `table_xinfo` lists them and
        // adds the 7th `hidden` column (0 = ordinary, 2 = VIRTUAL generated,
        // 3 = STORED generated). Sharing one arm made `table_xinfo` return
        // `table_info`'s six columns, which is not a narrower answer — a caller
        // that reads `hidden` off column 6 (Django's sqlite3 introspection
        // does, to decide which fields are generated) reads past the end.
        name_info @ ("table_info" | "table_xinfo") => {
            let xinfo = name_info == "table_xinfo";
            let mut names: Vec<&str> = vec!["cid", "name", "type", "notnull", "dflt_value", "pk"];
            if xinfo {
                names.push("hidden");
            }
            let cols_out = cols(&names);
            let Some(t) = arg.as_deref().and_then(|a| find_table(schema, a)) else {
                return Ok((cols_out, vec![]));
            };
            // The implicit rowid (#94) is HIDDEN: `SELECT *` does not expose it
            // and neither does sqlite's `table_info`. Listing it made every
            // consumer that builds a column list from this pragma — iterdump's
            // per-row INSERT among them — emit a column that does not exist.
            // It is elided from `table_xinfo` too: sqlite's rowid is not a
            // column of the table at all, so it has no `cid` there either.
            let rows = t
                .visible_columns()
                .iter()
                .enumerate()
                .filter(|(_, c)| xinfo || c.generated.is_none())
                // `cid` RENUMBERS: sqlite's `table_info` numbers the columns it
                // lists 0..n, so a table whose second column is generated has
                // its third column at cid 1 there and cid 2 in `table_xinfo`.
                // `pk` still needs the TRUE ordinal, which is why both are in
                // scope here.
                .enumerate()
                .map(|(cid, (i, c))| {
                    let pk = t
                        .primary_key
                        .iter()
                        .position(|&p| p as usize == i)
                        .map(|p| (p + 1) as i64)
                        .unwrap_or(0);
                    let mut row = vec![
                        Value::Int(cid as i64),
                        Value::Text(c.name.clone()),
                        // The DECLARED text, verbatim — `VARCHAR(50)` stays
                        // `VARCHAR(50)`, and a column declared with no type at
                        // all reports the empty string, both measured against
                        // sqlite 3.45.1. Reporting the mapped storage class
                        // instead told every reflecting consumer the wrong
                        // thing: Django read `VARCHAR(50)` back as a TextField,
                        // and SQLAlchemy lost the length entirely. It is the
                        // same source `sqlite3_column_decltype` already
                        // answers from (#112 wave 2); this pragma had simply
                        // never been pointed at it.
                        Value::Text(c.decltype().unwrap_or("").to_string()),
                        Value::Int(if c.nullable { 0 } else { 1 }),
                        // `dflt_value` is the DEFAULT's DDL TEXT, not its
                        // value: sqlite reports `'x'` with quotes, `3+5`
                        // unfolded, `1` on a BOOLEAN column as `1`. The schema
                        // carries that text (v15) precisely because the folded
                        // value cannot reproduce it. A schema built from TOML
                        // has no DDL text and reports NULL, which is also what
                        // sqlite says for a column with no default.
                        match &c.default_text {
                            Some(t) => Value::Text(t.clone()),
                            None => Value::Null,
                        },
                        Value::Int(pk),
                    ];
                    if xinfo {
                        row.push(Value::Int(
                            c.generated.as_ref().map_or(0, |g| g.kind.xinfo_hidden()),
                        ));
                    }
                    row
                })
                .collect();
            Ok((cols_out, rows))
        }
        "table_list" => {
            let cols_out = cols(&["schema", "name", "type", "ncol", "wr", "strict"]);
            let rows = user_tables(schema)
                .iter()
                .map(|t| {
                    vec![
                        Value::Text("main".into()),
                        Value::Text(t.name.clone()),
                        Value::Text("table".into()),
                        Value::Int(t.columns.len() as i64),
                        Value::Int(0),
                        Value::Int(0),
                    ]
                })
                .collect();
            Ok((cols_out, rows))
        }
        // `index_list` reports NEWEST FIRST (sqlite walks the table's Index
        // list, which is built by prepending), so the catalog order from
        // `table_index_rows` is reversed here. Probed on a table carrying a
        // UNIQUE constraint plus two later `CREATE INDEX`es: sqlite answered
        // `0|part`, `1|spaced`, `2|sqlite_autoindex_u_1`.
        //
        // Before this, every entry was reported as `sqlite_autoindex_<t>_<k>`
        // with origin `c` — a fabricated name for a real `CREATE INDEX`, which
        // then resolved to nothing in `sqlite_master` and made Django's
        // `get_constraints` see an index it could not look up.
        "index_list" => {
            let cols_out = cols(&["seq", "name", "unique", "origin", "partial"]);
            let Some(t) = arg.as_deref().and_then(|a| find_table(schema, a)) else {
                return Ok((cols_out, vec![]));
            };
            let mut all = table_index_rows(t, idx);
            all.reverse();
            let rows = all
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    vec![
                        Value::Int(i as i64),
                        Value::Text(r.name.clone()),
                        Value::Int(r.unique as i64),
                        Value::Text(r.origin.into()),
                        Value::Int(r.partial as i64),
                    ]
                })
                .collect();
            Ok((cols_out, rows))
        }
        // `index_info(<name>)` — `(seqno, cid, name)` per key column, which is
        // the third call in Django's `get_constraints` chain. `cid` is the
        // column's ordinal in `table_info`'s numbering, so a consumer can join
        // the two. An unknown name answers zero rows, as sqlite does.
        "index_info" => {
            let cols_out = cols(&["seqno", "cid", "name"]);
            let Some(want) = arg.as_deref() else {
                return Ok((cols_out, vec![]));
            };
            for t in user_tables(schema) {
                let Some(r) = table_index_rows(t, idx)
                    .into_iter()
                    .find(|r| r.name.eq_ignore_ascii_case(want))
                else {
                    continue;
                };
                let rows = r
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(seqno, &ord)| {
                        // An EXPRESSION key part is `cid` -2 and a NULL name in
                        // sqlite (-1 is the rowid; a real column is its index).
                        // mpedb reported its own sentinel and an EMPTY name,
                        // and a consumer that tests the name for NULL to detect
                        // an expression index — SQLAlchemy does, and skips such
                        // an index with a warning — saw a column instead and
                        // reflected an index it cannot use.
                        if ord == mpedb::INDEX_EXPR_COL {
                            return vec![
                                Value::Int(seqno as i64),
                                Value::Int(-2),
                                Value::Null,
                            ];
                        }
                        vec![
                            Value::Int(seqno as i64),
                            Value::Int(table_info_cid(t, ord)),
                            Value::Text(
                                t.columns
                                    .get(ord as usize)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default(),
                            ),
                        ]
                    })
                    .collect();
                return Ok((cols_out, rows));
            }
            Ok((cols_out, vec![]))
        }
        // Column ORDER is sqlite's, and it is not the order the words appear
        // in the DDL: `on_update` comes BEFORE `on_delete` (verified against
        // the 3.45.1 binary). `match` is always "NONE" — MATCH SIMPLE is the
        // only mode either engine implements.
        "foreign_key_list" => {
            let cols_out =
                cols(&["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"]);
            let Some(t) = arg.as_deref().and_then(|a| find_table(schema, a)) else {
                return Ok((cols_out, vec![]));
            };
            let mut rows = Vec::new();
            // sqlite numbers keys from the LAST declared one down to 0, so the
            // first `REFERENCES` in the DDL has the highest id. Reversing keeps
            // a consumer that sorts by id (Django's introspection does) in
            // declaration order.
            let n = t.foreign_keys.len();
            for (i, fk) in t.foreign_keys.iter().enumerate() {
                let id = (n - 1 - i) as i64;
                for (seq, &c) in fk.columns.iter().enumerate() {
                    rows.push(vec![
                        Value::Int(id),
                        Value::Int(seq as i64),
                        Value::Text(fk.parent.clone()),
                        // An out-of-range ordinal here is a CORRUPT SCHEMA, not
                        // a missing name: `fk.columns` addresses the row by
                        // position, and if it points past the row then FK
                        // ENFORCEMENT is reading the wrong column too. This was
                        // `unwrap_or_default()`, and the empty string it
                        // produced is why S14a — a drop that silently disabled
                        // enforcement — surfaced as a confusing dict instead of
                        // an alarm. Fail loudly so the next one cannot hide.
                        Value::Text(match t.columns.get(c as usize) {
                            Some(col) => col.name.clone(),
                            None => {
                                return Err(DbError::Corrupt(format!(
                                    "foreign key on \"{}\" names column ordinal {c}, but the \
                                     table has {} column(s)",
                                    t.name,
                                    t.columns.len()
                                )))
                            }
                        }),
                        // An empty parent list means "the parent's PRIMARY
                        // KEY", which sqlite reports as NULL here rather than
                        // resolving — the parent may not exist yet.
                        match fk.parent_columns.get(seq) {
                            Some(n) => Value::Text(n.clone()),
                            None => Value::Null,
                        },
                        Value::Text(fk_action_name(fk.on_update).into()),
                        Value::Text(fk_action_name(fk.on_delete).into()),
                        Value::Text("NONE".into()),
                    ]);
                }
            }
            Ok((cols_out, rows))
        }
        // `busy_timeout` is REAL on this shim: the same milliseconds
        // `sqlite3_busy_timeout()` sets, honoured by the BUSY retry loop AND —
        // via the caller mirroring it into `Database::set_busy_timeout` (#109)
        // — by the engine's bounded writer-lock wait. Both
        // forms answer one row named `timeout` holding the value in force —
        // sqlite's exact shape, including for the setter (verified against the
        // 3.45.1 binary). Before this, a consumer that set its lock timeout via
        // the pragma rather than the C function was silently left at 0.
        "busy_timeout" => {
            if let Some(a) = arg.as_deref() {
                // sqlite clamps a negative/unparsable value to 0.
                *busy_timeout_ms = a.trim().parse::<i32>().unwrap_or(0).max(0);
            }
            Ok((cols(&["timeout"]), vec![vec![Value::Int(*busy_timeout_ms as i64)]]))
        }
        // Getters that a consumer may read: return a single conventional value.
        // A setter form (`= value`) returns no rows, as sqlite does.
        //
        // `foreign_keys` is REAL since #194 — both directions. The SETTER is
        // handled by the caller (it needs to know whether a transaction is
        // open, which is where sqlite makes it a silent no-op); this arm only
        // reports the connection's live state.
        "foreign_keys" if arg.is_none() => Ok((
            cols(&["foreign_keys"]),
            vec![vec![Value::Int(i64::from(*fk_on))]],
        )),
        "journal_mode" => Ok((cols(&["journal_mode"]), vec![vec![Value::Text("memory".into())]])),
        "user_version" if arg.is_none() => {
            Ok((cols(&["user_version"]), vec![vec![Value::Int(0)]]))
        }
        "schema_version" if arg.is_none() => {
            Ok((cols(&["schema_version"]), vec![vec![Value::Int(0)]]))
        }
        // Stored and echoed per connection — see `EchoPragmas` for why these
        // three, and only these three. Getter: one row named after the pragma.
        // Setter: no rows, and the stored value is the one sqlite's own parser
        // would land on, not the caller's raw text.
        n @ ("synchronous" | "cache_size" | "read_uncommitted") => {
            let slot = match n {
                "synchronous" => &mut echo.synchronous,
                "cache_size" => &mut echo.cache_size,
                _ => &mut echo.read_uncommitted,
            };
            if let Some(a) = arg.as_deref() {
                *slot = match n {
                    "synchronous" => safety_level(a),
                    // `cache_size` takes the integer verbatim, sign and all
                    // (negative = kibibytes rather than pages); an unparsable
                    // one leaves the setting alone.
                    "cache_size" => a.trim().parse::<i64>().unwrap_or(*slot),
                    _ => pragma_boolean(a),
                };
                return Ok((Vec::new(), Vec::new()));
            }
            Ok((cols(&[n]), vec![vec![Value::Int(*slot)]]))
        }
        // Every other pragma (foreign_keys=on, …) is a no-op with no result —
        // the common database-setup pragmas.
        _ => Ok((Vec::new(), Vec::new())),
    }
}
