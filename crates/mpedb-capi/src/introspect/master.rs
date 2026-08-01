use super::ddl::master_sql;
use super::{object_ddl_text, table_index_rows, user_tables, IndexRecords};
use mpedb::{Error as DbError, Value};

// -------------------------------------------------------------- sqlite_master

/// The five sqlite_master columns, in order.
const MASTER_COLS: [&str; 5] = ["type", "name", "tbl_name", "rootpage", "sql"];
const SEQ_COLS: [&str; 2] = ["name", "seq"];

/// Does `sql` read `sqlite_master`/`sqlite_schema`? (identifier match, so a
/// string literal containing the word does not trigger it).
pub fn references_sqlite_master(sql: &str) -> bool {
    master_reference(sql).is_some() || references_sqlite_sequence(sql)
}

/// Does the statement SELECT FROM `sqlite_sequence`?
///
/// sqlite's AUTOINCREMENT counters live in a real internal table; mpedb keeps
/// them in the catalog's sys keyspace, so the table is SYNTHESISED on read the
/// way `sqlite_master` is. CPython's `iterdump` reads it by name after finding
/// it listed in `sqlite_master`, which is the whole of what needs to work.
///
/// The FROM position specifically, and that is not fussiness. Django's table
/// listing is `SELECT name, type FROM sqlite_master WHERE type IN ('table',
/// 'view') AND NOT name='sqlite_sequence'` — the name appears as a string
/// LITERAL. Matching it anywhere (which is what the catalog scan next door
/// does, boundary checks and all) answered that query with the two-column
/// sequence table instead of the master listing, so Django stopped seeing its
/// own `django_migrations` and tried to create it twice. A wrong answer, and
/// the Django gate is what caught it: the SQLAlchemy suite never moved.
pub fn references_sqlite_sequence(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(pos) = lower[from..].find("from") {
        let at = from + pos;
        from = at + 4;
        // A whole word, not the tail of one.
        if lower[..at].chars().last().is_some_and(ident) {
            continue;
        }
        let rest = lower[from..].trim_start();
        // Any of sqlite's quotings, or none.
        let rest = rest
            .strip_prefix('"')
            .or_else(|| rest.strip_prefix('`'))
            .or_else(|| rest.strip_prefix('['))
            .unwrap_or(rest);
        if let Some(tail) = rest.strip_prefix("sqlite_sequence") {
            if !tail.chars().next().is_some_and(ident) {
                return true;
            }
        }
    }
    false
}

/// Answer a `SELECT … FROM sqlite_sequence …` over the synthesised rows —
/// the same mini-evaluator as [`sqlite_master`]: projection of any subset of
/// `name`/`seq` (or `*`, or `count(*)`), an AND-joined WHERE with bound
/// parameters (`WHERE name = ?` is how CPython consumers read one table's
/// counter, and `name == ?` is accepted because sqlite treats the doubled
/// spelling identically), and `ORDER BY name`.
///
/// `seq` projects as the INTEGER it is; in a WHERE it compares as its decimal
/// text against a stringified bound parameter — like for like. `ORDER BY seq`
/// is REFUSED rather than string-sorted (10 before 2 is a wrong answer, and no
/// measured consumer orders by seq).
pub fn sqlite_sequence_query(
    seqs: &[(String, i64)],
    sql: &str,
    params: &[Value],
) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    let lower = sql.to_ascii_lowercase();
    let sel = lower.find("select").ok_or_else(unsupported)?;
    let from = lower.find("from").ok_or_else(unsupported)?;
    if from < sel {
        return Err(unsupported());
    }
    let proj_src = sql[sel + 6..from].trim();

    let rest_lower = &lower[from..];
    let where_at = rest_lower.find("where").map(|p| from + p);
    let order_at = rest_lower.find("order").map(|p| from + p);
    let where_end = order_at.unwrap_or(sql.len());

    let mut rows: Vec<(String, i64)> = seqs.to_vec();
    if let Some(w) = where_at {
        let preds = parse_where(sql[w + 5..where_end].trim(), params, &SEQ_COLS)?;
        rows.retain(|r| {
            let val = |c: &str| match c {
                "name" => r.0.clone(),
                "seq" => r.1.to_string(),
                _ => String::new(),
            };
            preds.iter().all(|p| p.matches_with(&val))
        });
    }
    if let Some(o) = order_at {
        let ol = lower[o + 5..].trim();
        let ol = ol.strip_prefix("by").map(str::trim_start).unwrap_or(ol);
        let key = ol
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']');
        if key != "name" {
            return Err(unsupported());
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        if ol.contains("desc") {
            rows.reverse();
        }
    }

    let proj_lower = proj_src.to_ascii_lowercase();
    if proj_lower.replace(' ', "") == "count(*)" {
        return Ok((vec!["count(*)".into()], vec![vec![Value::Int(rows.len() as i64)]]));
    }
    let out_cols: Vec<String> = if proj_src == "*" {
        SEQ_COLS.iter().map(|s| s.to_string()).collect()
    } else {
        let mut v = Vec::new();
        for item in proj_src.split(',') {
            let name = item.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            let name = name.trim_matches('"');
            if !SEQ_COLS.contains(&name) {
                return Err(unsupported());
            }
            v.push(name.to_string());
        }
        v
    };
    let out_rows = rows
        .iter()
        .map(|(n, s)| {
            out_cols
                .iter()
                .map(|c| match c.as_str() {
                    "seq" => Value::Int(*s),
                    _ => Value::Text(n.clone()),
                })
                .collect()
        })
        .collect();
    Ok((out_cols, out_rows))
}

/// Is this statement a WRITE targeting `sqlite_sequence`? TARGET position
/// only — `UPDATE sqlite_sequence`, `INSERT INTO sqlite_sequence`,
/// `DELETE FROM sqlite_sequence` — for the same reason the read detector
/// pins the FROM position: the name appears as a string LITERAL in Django's
/// own catalog queries, and match-anywhere answered the wrong table once
/// already. Statement-start anchored; sqlite's `OR <conflict>` spellings are
/// carried through.
pub fn sqlite_sequence_write_target(sql: &str) -> bool {
    parse_seq_target(sql).is_some()
}

/// (verb, byte offset just past the table name) when the statement writes
/// `sqlite_sequence`.
fn parse_seq_target(sql: &str) -> Option<(SeqVerb, usize)> {
    let lower = sql.to_ascii_lowercase();
    let s = lower.trim_start();
    let base = lower.len() - s.len();
    let take = |rest: &str, word: &str| -> Option<usize> {
        let r = rest.strip_prefix(word)?;
        (r.starts_with(char::is_whitespace) || r.is_empty()).then_some(word.len())
    };
    let skip_ws = |s: &str| s.len() - s.trim_start().len();
    let (verb, mut at) = if let Some(n) = take(s, "update") {
        (SeqVerb::Update, n)
    } else if let Some(n) = take(s, "insert") {
        (SeqVerb::Insert, n)
    } else {
        (SeqVerb::Delete, take(s, "delete")?)
    };
    at += skip_ws(&s[at..]);
    // `UPDATE OR ROLLBACK …` / `INSERT OR REPLACE INTO …` — conflict clauses
    // change nothing for a counter write, so they parse and are ignored.
    if matches!(verb, SeqVerb::Update | SeqVerb::Insert) {
        if let Some(n) = take(&s[at..], "or") {
            at += n + skip_ws(&s[at + n..]);
            let word_end = s[at..].find(char::is_whitespace).unwrap_or(s.len() - at);
            at += word_end + skip_ws(&s[at + word_end..]);
        }
    }
    match verb {
        SeqVerb::Insert => {
            let n = take(&s[at..], "into")?;
            at += n + skip_ws(&s[at + n..]);
        }
        SeqVerb::Delete => {
            let n = take(&s[at..], "from")?;
            at += n + skip_ws(&s[at + n..]);
        }
        SeqVerb::Update => {}
    }
    let rest = &s[at..];
    let (rest, quoted) = match rest
        .strip_prefix('"')
        .or_else(|| rest.strip_prefix('`'))
        .or_else(|| rest.strip_prefix('['))
    {
        Some(r) => (r, 1),
        None => (rest, 0),
    };
    let tail = rest.strip_prefix("sqlite_sequence")?;
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if quoted == 0 && tail.chars().next().is_some_and(ident) {
        return None;
    }
    let name_end = at + quoted + "sqlite_sequence".len() + quoted;
    Some((verb, base + name_end))
}

#[derive(Clone, Copy, PartialEq)]
enum SeqVerb {
    Update,
    Insert,
    Delete,
}

/// What a `sqlite_sequence` write resolves to over the SYNTHESISED rows: the
/// per-name counter updates (`None` = delete the record), the changes()
/// count, and — for INSERT — a possibly-virgin name the caller must resolve
/// against the live schema (stock would store an orphan row for an unknown
/// table; mpedb's catalog-backed counters have nowhere to keep one, so the
/// caller refuses BY NAME rather than faking success).
pub struct SeqWritePlan {
    pub updates: Vec<(String, Option<i64>)>,
    pub affected: i64,
    pub insert_new: Option<(String, i64)>,
}

/// Parse + evaluate a `sqlite_sequence` write against the visible rows.
/// Supported forms are the CONSUMER forms, measured on stock 3.45.1:
///
///  * `UPDATE sqlite_sequence SET seq = <int> [WHERE …]` — Django's flush
///    reset (`WHERE name IN (…)`), the seek form (`WHERE name = ?`), and the
///    bare no-WHERE sweep. The value is stored VERBATIM (stock keeps a low
///    seq as written; allocation corrects, and so does mpedb's `next_rowid`).
///    A WHERE that matches nothing is a SILENT no-op with 0 changes — stock's
///    "it is just a table" answer, and rowcounts match stock row for row.
///  * `DELETE FROM sqlite_sequence [WHERE …]` — the record goes away, which
///    resets the sequence (recreated at the next allocation), stock's rule.
///  * `INSERT INTO sqlite_sequence [(cols)] VALUES (…)` — pre-seeding a
///    VIRGIN table's counter is honored (next id = seq + 1); a name that
///    already has a row becomes stock's allocation-inert duplicate, which
///    the synthesised table represents as a counted no-op (1 change, counter
///    unchanged — the FIRST row wins allocation in stock, and ours IS the
///    first row).
///
/// Everything else refuses by name rather than approximating: `SET name = …`
/// (stock manufactures duplicate rows), a non-integer or NULL seq (stock
/// stores junk verbatim and treats it as 0 at allocation — mpedb's counters
/// are rigid i64), expressions over `seq`, and multi-row VALUES.
pub fn sqlite_sequence_write(
    seqs: &[(String, i64)],
    sql: &str,
    params: &[Value],
) -> Result<SeqWritePlan, DbError> {
    let (verb, after_name) = parse_seq_target(sql).ok_or_else(unsupported)?;
    let rest = sql[after_name..].trim();
    let lower_rest = rest.to_ascii_lowercase();
    let int_of = |tok: &str| -> Result<i64, DbError> {
        let t = tok.trim();
        if let Some(idx) = t.strip_prefix('?') {
            let i = if idx.is_empty() {
                0
            } else {
                idx.parse::<usize>().map_err(|_| unsupported())?.saturating_sub(1)
            };
            return match params.get(i) {
                Some(Value::Int(v)) => Ok(*v),
                Some(other) => Err(DbError::Unsupported(format!(
                    "sqlite_sequence.seq is a rigid INTEGER counter in mpedb; a bound {} \
                     is refused (sqlite would store it verbatim and read it as 0)",
                    other.type_name()
                ))),
                None => Err(unsupported()),
            };
        }
        if t.eq_ignore_ascii_case("null") {
            return Err(DbError::Unsupported(
                "sqlite_sequence.seq = NULL is refused: mpedb keeps the counter as a rigid \
                 INTEGER (sqlite stores the NULL and treats it as no history)"
                    .into(),
            ));
        }
        t.parse::<i64>().map_err(|_| {
            DbError::Unsupported(format!(
                "sqlite_sequence.seq must be an integer literal or bound parameter \
                 (got `{t}`) — expressions and junk values are refused by name"
            ))
        })
    };
    match verb {
        SeqVerb::Update => {
            let set_tail = lower_rest.strip_prefix("set").ok_or_else(unsupported)?;
            if !set_tail.starts_with(char::is_whitespace) {
                return Err(unsupported());
            }
            let after_set = rest[3..].trim_start();
            let lower_after = after_set.to_ascii_lowercase();
            // The assignment is strictly `seq = <int|param>`, so the first
            // word-bounded `where` splits it — nothing in a legal assignment
            // can contain the word.
            let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
            let mut where_pos = None;
            let mut i = 0;
            while let Some(p) = lower_after[i..].find("where") {
                let at = i + p;
                i = at + 5;
                let before_ok = !lower_after[..at].chars().last().is_some_and(ident);
                let after_ok = !lower_after[at + 5..].chars().next().is_some_and(ident);
                if before_ok && after_ok {
                    where_pos = Some(at);
                    break;
                }
            }
            let (assign_src, where_src) = match where_pos {
                Some(w) => (&after_set[..w], Some(after_set[w + 5..].trim())),
                None => (after_set, None),
            };
            let mut parts = assign_src.split('=');
            let lhs = parts.next().unwrap_or("").trim().trim_matches('"');
            let rhs = parts.next().ok_or_else(unsupported)?.trim();
            if parts.next().is_some() {
                return Err(unsupported());
            }
            if !lhs.eq_ignore_ascii_case("seq") {
                return Err(DbError::Unsupported(format!(
                    "UPDATE sqlite_sequence SET {lhs} is refused by name — only `seq` is \
                     writable (renaming rows manufactures the duplicates stock then \
                     ignores at allocation)"
                )));
            }
            let value = int_of(rhs)?;
            let preds = match where_src {
                Some(w) if !w.is_empty() => parse_where(w, params, &SEQ_COLS)?,
                _ => Vec::new(),
            };
            let matched: Vec<&(String, i64)> = seqs
                .iter()
                .filter(|r| {
                    let val = |c: &str| match c {
                        "name" => r.0.clone(),
                        "seq" => r.1.to_string(),
                        _ => String::new(),
                    };
                    preds.iter().all(|p| p.matches_with(&val))
                })
                .collect();
            Ok(SeqWritePlan {
                affected: matched.len() as i64,
                updates: matched.iter().map(|r| (r.0.clone(), Some(value))).collect(),
                insert_new: None,
            })
        }
        SeqVerb::Delete => {
            let preds = if let Some(w) = lower_rest.strip_prefix("where") {
                if !w.starts_with(char::is_whitespace) && !w.is_empty() {
                    return Err(unsupported());
                }
                parse_where(rest[5..].trim(), params, &SEQ_COLS)?
            } else if rest.is_empty() {
                Vec::new()
            } else {
                return Err(unsupported());
            };
            let matched: Vec<&(String, i64)> = seqs
                .iter()
                .filter(|r| {
                    let val = |c: &str| match c {
                        "name" => r.0.clone(),
                        "seq" => r.1.to_string(),
                        _ => String::new(),
                    };
                    preds.iter().all(|p| p.matches_with(&val))
                })
                .collect();
            Ok(SeqWritePlan {
                affected: matched.len() as i64,
                updates: matched.iter().map(|r| (r.0.clone(), None)).collect(),
                insert_new: None,
            })
        }
        SeqVerb::Insert => {
            // `[(name, seq)] VALUES (a, b)` — explicit column list honored in
            // either order, defaulting to (name, seq). One row.
            let (cols, vals_src) = if let Some(after_paren) = rest.strip_prefix('(') {
                let close = after_paren.find(')').ok_or_else(unsupported)?;
                let cols: Vec<String> = after_paren[..close]
                    .split(',')
                    .map(|c| c.trim().trim_matches('"').to_ascii_lowercase())
                    .collect();
                (cols, after_paren[close + 1..].trim())
            } else {
                (vec!["name".into(), "seq".into()], rest)
            };
            let lower_vals = vals_src.to_ascii_lowercase();
            let body = lower_vals.strip_prefix("values").ok_or_else(unsupported)?;
            if !body.trim_start().starts_with('(') {
                return Err(unsupported());
            }
            let body_src = vals_src["values".len()..].trim_start();
            let inner = body_src
                .strip_prefix('(')
                .and_then(|b| b.strip_suffix(')'))
                .ok_or_else(|| {
                    DbError::Unsupported(
                        "INSERT INTO sqlite_sequence takes ONE (name, seq) row; multi-row \
                         VALUES is refused by name"
                            .into(),
                    )
                })?;
            let items: Vec<&str> = inner.split(',').map(str::trim).collect();
            if items.len() != cols.len() || cols.len() != 2 {
                return Err(unsupported());
            }
            let mut name: Option<String> = None;
            let mut seq: Option<i64> = None;
            for (c, item) in cols.iter().zip(&items) {
                match c.as_str() {
                    "name" => {
                        let v = if let Some(idx) = item.strip_prefix('?') {
                            let i = if idx.is_empty() {
                                0
                            } else {
                                idx.parse::<usize>().map_err(|_| unsupported())?.saturating_sub(1)
                            };
                            match params.get(i) {
                                Some(Value::Text(s)) => s.clone(),
                                _ => return Err(unsupported()),
                            }
                        } else {
                            let t = item.trim();
                            t.strip_prefix('\'')
                                .and_then(|s| s.strip_suffix('\''))
                                .ok_or_else(unsupported)?
                                .replace("''", "'")
                        };
                        name = Some(v);
                    }
                    "seq" => seq = Some(int_of(item)?),
                    _ => return Err(unsupported()),
                }
            }
            let (name, seq) = (name.ok_or_else(unsupported)?, seq.ok_or_else(unsupported)?);
            if seqs.iter().any(|(n, _)| *n == name) {
                // Stock creates a duplicate row that allocation then ignores
                // (the FIRST matching row wins). The synthesised table IS the
                // first row, so the honest equivalent is: change nothing,
                // count one changed row, exactly what stock reports.
                return Ok(SeqWritePlan { updates: Vec::new(), affected: 1, insert_new: None });
            }
            Ok(SeqWritePlan { updates: Vec::new(), affected: 1, insert_new: Some((name, seq)) })
        }
    }
}

/// Which catalog a statement reads: `Some(false)` for the main one,
/// `Some(true)` for the TEMP one, `None` for neither.
///
/// `sqlite_temp_master` is a separate table in sqlite, listing only the temp
/// schema — and it is how SQLAlchemy enumerates temp tables, so answering
/// "unknown table" for it cost 280 tests once temp tables existed to list.
pub fn master_reference(sql: &str) -> Option<MasterRef> {
    let temp = names_a_catalog(sql, &["sqlite_temp_master", "sqlite_temp_schema"]);
    let main = names_a_catalog(sql, &["sqlite_master", "sqlite_schema"]);
    match (temp, main) {
        (true, false) => Some(MasterRef::Temp),
        // A statement naming BOTH — `SELECT sql FROM (SELECT * FROM
        // sqlite_master UNION ALL SELECT * FROM sqlite_temp_master) WHERE
        // name = ?` is how SQLAlchemy asks for an object's DDL without caring
        // which schema holds it — reads BOTH catalogs, main's rows first, as
        // the UNION ALL writes them. Answering from main alone (which is what
        // this did) made every temp object a "no such table" to a reflecting
        // consumer.
        (true, true) => Some(MasterRef::Both),
        (false, true) => Some(MasterRef::Main),
        (false, false) => None,
    }
}

/// Which catalog(s) a statement's `sqlite_master` reference names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MasterRef {
    Main,
    Temp,
    Both,
}

/// The `<schema>.` a catalog reference was qualified with, if any.
///
/// `SELECT name FROM "test_schema".sqlite_master …` is how SQLAlchemy lists an
/// attached database's VIEWS; answering that from main described the wrong
/// file. Quoted or bare, like the pragma qualifier.
pub fn master_schema(sql: &str) -> Option<String> {
    catalog_qualifier(sql, &["sqlite_master", "sqlite_schema"])
}

/// The `<schema>.` a TEMP-catalog reference was qualified with, if any.
///
/// There is no such object: `sqlite_temp_master` names the connection's one
/// temp schema and cannot be qualified with another. sqlite says `no such
/// table: <q>.sqlite_temp_master`, and SQLAlchemy DEPENDS on that error —
/// its `_get_table_sql` tries a `UNION ALL` over both catalogs and falls back
/// to plain `sqlite_master` when the DBAPI raises. Answering an empty result
/// instead let the fallback never run, so every table in an attached schema
/// reflected as "no such table" (measured: `ComponentReflectionTestExtra`).
pub fn qualified_temp_master(sql: &str) -> Option<String> {
    catalog_qualifier(sql, &["sqlite_temp_master", "sqlite_temp_schema"])
        .filter(|q| !q.eq_ignore_ascii_case("temp"))
}

fn catalog_qualifier(sql: &str, kws: &[&str]) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    for kw in kws {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(kw) {
            let at = from + pos;
            from = at + kw.len();
            let before = sql[..at].trim_end();
            let Some(before) = before.strip_suffix('.') else {
                continue;
            };
            let name = match before.chars().last() {
                Some(q @ ('"' | '`' | '\'')) => before[..before.len() - 1]
                    .rfind(q)
                    .map(|i| before[i + 1..before.len() - 1].to_string()),
                Some(']') => before[..before.len() - 1]
                    .rfind('[')
                    .map(|i| before[i + 1..before.len() - 1].to_string()),
                _ => {
                    let n: String = before
                        .chars()
                        .rev()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    Some(n.chars().rev().collect())
                }
            };
            match name {
                Some(n) if !n.is_empty() => return Some(n),
                _ => continue,
            }
        }
    }
    None
}

fn names_a_catalog(sql: &str, kws: &[&str]) -> bool {
    let lower = sql.to_ascii_lowercase();
    for kw in kws {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(kw) {
            let at = from + pos;
            let before = lower[..at].chars().last();
            let after = lower[at + kw.len()..].chars().next();
            let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
            if before.is_none_or(|c| !ident(c)) && after.is_none_or(|c| !ident(c)) {
                return true;
            }
            from = at + kw.len();
        }
    }
    false
}

#[derive(Clone)]
struct MasterRow {
    ty: &'static str,
    name: String,
    tbl_name: String,
    sql: String,
}

fn master_cell(r: &MasterRow, col: &str) -> Value {
    match col {
        "type" => Value::Text(r.ty.into()),
        "name" => Value::Text(r.name.clone()),
        "tbl_name" => Value::Text(r.tbl_name.clone()),
        "rootpage" => Value::Int(0),
        // An empty `sql` is sqlite's NULL, and it has to arrive as NULL, not as
        // `''`: it is exactly what tells a consumer "this is a constraint index,
        // not a statement" — Django's `get_constraints` does `if not sql:
        // continue`, and CPython's iterdump filters `WHERE sql NOT NULL`.
        "sql" if r.sql.is_empty() => Value::Null,
        "sql" => Value::Text(r.sql.clone()),
        _ => Value::Null,
    }
}

/// Answer a `SELECT … FROM sqlite_master …`. Supports projecting any subset of
/// the five columns (or `*`, or `count(*)`), a `WHERE` of AND-joined
/// `col = 'lit'` / `col <> 'lit'` / `col IN ('a','b')` / `col [NOT] LIKE 'p'`
/// predicates, and `ORDER BY name`. Unsupported shapes → a clear error.
///
/// `verbatim` is the caller's own `CREATE TABLE` text per table name, as far as
/// the shim recorded it (see `ddl_record`); a table with no usable record gets
/// the canonical reconstruction instead.
///
/// `views` / `triggers` are `(name, create_sql)` / `(name, tbl_name, create_sql)`
/// from the engine catalog so iterdump can re-emit them. `idx` is the shim's
/// `CREATE INDEX` record set ([`IDX_NS`]), which is what gives an index row a
/// name and a `sql`.
///
/// `params` are the statement's bound values: a `WHERE` operand may be a
/// parameter (`$N` after `scan_params` rewrote `?`/`:name`), which is the ONLY
/// form Django's `get_constraints` ever writes.
/// One catalog the `sqlite_master` mini-evaluator reads rows from.
///
/// A LIST of these, because a statement may name more than one:
/// `SELECT … FROM (SELECT * FROM sqlite_master UNION ALL SELECT * FROM
/// sqlite_temp_master)` is how SQLAlchemy asks for an object's DDL text
/// without caring which schema holds it. Answered from main alone, every
/// TEMP object came back as "no such table" and was dropped from its
/// reflection results.
pub struct MasterSource<'a> {
    pub schema: &'a mpedb::Schema,
    pub verbatim: &'a std::collections::HashMap<String, Vec<u8>>,
    pub idx: &'a IndexRecords,
    pub views: &'a [(String, String)],
    pub triggers: &'a [(String, String, String)],
}

pub fn sqlite_master(
    sources: &[MasterSource],
    sql: &str,
    params: &[Value],
) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    let lower = sql.to_ascii_lowercase();
    let sel = lower
        .find("select")
        .ok_or_else(unsupported)?;
    let from = lower.find("from").ok_or_else(unsupported)?;
    if from < sel {
        return Err(unsupported());
    }
    let proj_src = sql[sel + 6..from].trim();

    // Clause boundaries after FROM.
    let rest_lower = &lower[from..];
    let where_at = rest_lower.find("where").map(|p| from + p);
    let order_at = rest_lower.find("order").map(|p| from + p);

    let where_end = order_at.unwrap_or(sql.len());
    let where_src = where_at.map(|w| sql[w + 5..where_end].trim().to_string());
    let order_src = order_at.map(|o| sql[o + 5..].trim().to_string());

    // User tables — each followed by ITS indexes — then triggers, then views.
    // CPython's `iterdump` second pass is `WHERE type IN ('index','trigger',
    // 'view')` with NO ORDER BY, so the row order is the catalog's insertion
    // order. Emitting views before triggers inverted CPython `test_table_dump`
    // (trigger created, then view), and an index has to follow the table it is
    // built on or a replayed dump creates it against nothing. sqlite's true
    // order is global creation order, which mpedb's schema does not record;
    // grouping by table agrees with it whenever indexes are created with their
    // table (every ORM migration) and replays correctly regardless.
    let mut rows: Vec<MasterRow> = Vec::new();
    for src in sources {
        let (schema, verbatim, idx, views, triggers) =
            (src.schema, src.verbatim, src.idx, src.views, src.triggers);
    for t in user_tables(schema) {
        rows.push(MasterRow {
            ty: "table",
            name: t.name.clone(),
            tbl_name: t.name.clone(),
            sql: master_sql(t, idx, verbatim.get(&t.name)),
        });
        for r in table_index_rows(t, idx) {
            rows.push(MasterRow {
                ty: "index",
                name: r.name,
                tbl_name: t.name.clone(),
                // A constraint index has NO statement text in sqlite either —
                // `master_cell` turns the empty string back into NULL.
                sql: r.sql.unwrap_or_default(),
            });
        }
    }
    // sqlite creates `sqlite_sequence` the moment the first AUTOINCREMENT
    // table exists, and lists it like any other table — which is how
    // `iterdump` finds it. Emitted after the user tables, where sqlite's
    // creation order puts it.
    if sources.iter().any(|src| user_tables(src.schema).iter().any(|t| t.autoincrement)) {
        rows.push(MasterRow {
            ty: "table",
            name: "sqlite_sequence".into(),
            tbl_name: "sqlite_sequence".into(),
            sql: "CREATE TABLE sqlite_sequence(name,seq)".into(),
        });
    }
    for (name, tbl, create_sql) in triggers {
        let sql = object_ddl_text(verbatim.get(name)).unwrap_or_else(|| create_sql.clone());
        rows.push(MasterRow {
            ty: "trigger",
            name: name.clone(),
            tbl_name: tbl.clone(),
            sql,
        });
    }
    for (name, select_sql) in views {
        // Prefer the caller's own CREATE VIEW text when the shim recorded it
        // (CPython `test_table_dump` asserts spelling); fall back to a
        // reconstruction from the stored select body.
        let create = object_ddl_text(verbatim.get(name))
            .unwrap_or_else(|| format!("CREATE VIEW \"{name}\" AS {select_sql}"));
        rows.push(MasterRow {
            ty: "view",
            name: name.clone(),
            tbl_name: name.clone(),
            sql: create,
        });
    }
    }

    // WHERE.
    if let Some(w) = &where_src {
        let preds = parse_where(w, params, &MASTER_COLS)?;
        rows.retain(|r| preds.iter().all(|p| p.matches(r)));
    }

    // ORDER BY name (the only ordering consumers use here). `order_src` is the
    // text after "ORDER", i.e. "BY name [DESC]" — strip the leading "BY" before
    // matching the column.
    if let Some(o) = &order_src {
        let ol = o.to_ascii_lowercase();
        let ol = ol.strip_prefix("by").map(str::trim_start).unwrap_or(ol.as_str());
        let key = ol
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
            .to_string();
        if !MASTER_COLS.contains(&key.as_str()) {
            return Err(unsupported());
        }
        let cell = |r: &MasterRow| match key.as_str() {
            "type" => r.ty.to_string(),
            "tbl_name" => r.tbl_name.clone(),
            "rootpage" => "0".to_string(),
            "sql" => r.sql.clone(),
            _ => r.name.clone(),
        };
        rows.sort_by_key(&cell);
        if ol.contains("desc") {
            rows.reverse();
        }
    }

    // Projection.
    let proj_lower = proj_src.to_ascii_lowercase();
    if proj_lower.replace(' ', "") == "count(*)" {
        return Ok((vec!["count(*)".into()], vec![vec![Value::Int(rows.len() as i64)]]));
    }
    let out_cols: Vec<String> = if proj_src == "*" {
        MASTER_COLS.iter().map(|s| s.to_string()).collect()
    } else {
        let mut v = Vec::new();
        for item in proj_src.split(',') {
            // Strip an optional alias (`col AS x` / `col x`) — first token.
            let name = item.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            let name = name.trim_matches('"');
            if !MASTER_COLS.contains(&name) {
                return Err(unsupported());
            }
            v.push(name.to_string());
        }
        v
    };

    let out_rows = rows
        .iter()
        .map(|r| out_cols.iter().map(|c| master_cell(r, c)).collect())
        .collect();
    Ok((out_cols, out_rows))
}

fn unsupported() -> DbError {
    DbError::Unsupported(
        "this sqlite_master query form is not supported by the mpedb C-API shim; \
         use PRAGMA table_list / table_info instead"
            .into(),
    )
}

enum Pred {
    Eq(String, String),
    Ne(String, String),
    In(String, Vec<String>),
    Like(String, String, bool), // (col, pattern, negated)
    /// `col IS NULL` / `col IS NOT NULL` / sqlite's `col NOTNULL` and the
    /// bare `col NOT NULL` CPython's iterdump writes. `true` = negated
    /// (matches non-NULL).
    Null(String, bool),
    /// A clause-leading `NOT` (Django's introspection writes
    /// `AND NOT name='sqlite_sequence'`).
    Not(Box<Pred>),
    /// A comparison against a bound parameter that is NULL. `col = NULL` is
    /// UNKNOWN in SQL's 3VL, never true — and so is `col <> NULL` and
    /// `col IN (NULL)`, which is why this is one variant and not a value.
    Never,
}

impl Pred {
    fn matches(&self, r: &MasterRow) -> bool {
        self.matches_with(&|c: &str| match c {
            "type" => r.ty.to_string(),
            "name" => r.name.clone(),
            "tbl_name" => r.tbl_name.clone(),
            "rootpage" => "0".to_string(),
            "sql" => r.sql.clone(),
            _ => String::new(),
        })
    }

    /// The row abstracted to a cell-lookup, so the synthesised
    /// `sqlite_sequence` shares this evaluator instead of growing a second
    /// one that would drift (`seq` arrives as its decimal text — a bound
    /// integer parameter is stringified the same way in [`operand`], so
    /// equality still compares like for like).
    fn matches_with(&self, val: &dyn Fn(&str) -> String) -> bool {
        match self {
            Pred::Eq(c, v) => val(c) == *v,
            Pred::Ne(c, v) => val(c) != *v,
            Pred::In(c, vs) => vs.iter().any(|v| *v == val(c)),
            Pred::Like(c, pat, neg) => like_match(&val(c), pat) != *neg,
            // `sql` is the only column that is ever NULL here — a constraint
            // index has no statement text, in sqlite's catalog and in this one.
            Pred::Null(c, negated) => val(c).is_empty() != *negated,
            Pred::Not(inner) => !inner.matches_with(val),
            Pred::Never => false,
        }
    }
}

/// One comparison operand: a `'string literal'`, or a bound parameter.
///
/// `Some(Some(s))` is a value, `Some(None)` is SQL NULL, `None` is a form this
/// evaluator does not recognize (which REFUSES the whole query rather than
/// silently dropping the predicate).
///
/// Parameters arrive as `$N` because `sql::scan_params` has already rewritten
/// `?`, `?N`, `:name`, `@name` and `$name` to mpedb's numbered form — so this
/// one shape covers every binding style a consumer can write. Django's
/// `get_constraints` reaches `sqlite_master` ONLY through bound parameters
/// (`WHERE type='table' and name=%s`), so before this the whole method raised
/// on the shim.
fn operand(s: &str, params: &[Value]) -> Option<Option<String>> {
    let t = s.trim();
    if let Some(digits) = t.strip_prefix('$') {
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let n: usize = digits.parse().ok()?;
            return match params.get(n.checked_sub(1)?)? {
                Value::Null => Some(None),
                Value::Text(v) => Some(Some(v.clone())),
                Value::Int(v) => Some(Some(v.to_string())),
                // A catalog column is TEXT; comparing it to a float or a blob
                // is a shape this evaluator refuses rather than guesses at.
                _ => None,
            };
        }
        return None;
    }
    str_literal(t).map(Some)
}

/// A minimal `LIKE`: `%` = any run, `_` = one char. Case-insensitive, as sqlite.
fn like_match(s: &str, pat: &str) -> bool {
    fn go(s: &[u8], p: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        match p[0] {
            b'%' => go(s, &p[1..]) || (!s.is_empty() && go(&s[1..], p)),
            b'_' => !s.is_empty() && go(&s[1..], &p[1..]),
            c => !s.is_empty() && s[0].eq_ignore_ascii_case(&c) && go(&s[1..], &p[1..]),
        }
    }
    go(s.as_bytes(), pat.as_bytes())
}

fn parse_where(w: &str, params: &[Value], cols: &[&str]) -> Result<Vec<Pred>, DbError> {
    let mut preds = Vec::new();
    // Split on AND (case-insensitive), at top level (no nested parens support).
    for clause in split_and(w) {
        let mut c = clause.trim();
        // A clause-leading `NOT` negates the comparison that follows — Django's
        // `get_table_list` writes `AND NOT name='sqlite_sequence'`. Doubled
        // `NOT`s cancel.
        let mut negate = false;
        while c.len() >= 4
            && c[..3].eq_ignore_ascii_case("not")
            && c.as_bytes()[3].is_ascii_whitespace()
        {
            negate = !negate;
            c = c[3..].trim_start();
        }
        let p = parse_cmp(c, params, cols)?;
        preds.push(if negate { Pred::Not(Box::new(p)) } else { p });
    }
    Ok(preds)
}

/// One comparison of a `sqlite_master` WHERE clause. A shape this does not
/// recognize is REFUSED — including anything containing a top-level `OR`, whose
/// operands this AND-only evaluator would otherwise silently drop and answer
/// wrongly.
fn parse_cmp(c: &str, params: &[Value], cols: &[&str]) -> Result<Pred, DbError> {
    let cl = c.to_ascii_lowercase();
    if cl.starts_with("or ") || cl.contains(" or ") {
        return Err(unsupported());
    }
    let col_of = |c: &str| {
        let t = c
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
            .to_ascii_lowercase();
        if cols.contains(&t.as_str()) {
            Some(t)
        } else {
            None
        }
    };
    // `col IS NOT NULL` / `col NOT NULL` / `col NOTNULL` / `col IS NULL`.
    // Longest first: `is not null` must not be read as `is null`.
    for (suffix, negated) in [
        (" is not null", true),
        (" not null", true),
        (" notnull", true),
        (" is null", false),
        (" isnull", false),
    ] {
        let t = cl.trim_end();
        if let Some(head) = t.strip_suffix(suffix) {
            let col = col_of(&c[..head.len()]).ok_or_else(unsupported)?;
            return Ok(Pred::Null(col, negated));
        }
    }
    if let Some(idx) = cl.find(" not like ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let pat = operand(&c[idx + 10..], params).ok_or_else(unsupported)?;
        Ok(match pat {
            Some(p) => Pred::Like(col, p, true),
            None => Pred::Never,
        })
    } else if let Some(idx) = cl.find(" like ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let pat = operand(&c[idx + 6..], params).ok_or_else(unsupported)?;
        Ok(match pat {
            Some(p) => Pred::Like(col, p, false),
            None => Pred::Never,
        })
    } else if let Some(idx) = cl.find(" in ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let list = &c[idx + 4..];
        let inner = list.trim().trim_start_matches('(').trim_end_matches(')');
        let vals: Option<Vec<Option<String>>> =
            inner.split(',').map(|e| operand(e, params)).collect();
        // A NULL element of an IN list never matches; the rest still can.
        Ok(Pred::In(col, vals.ok_or_else(unsupported)?.into_iter().flatten().collect()))
    } else if let Some(idx) = cl.find("!=").or_else(|| cl.find("<>")) {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let v = operand(&c[idx + 2..], params).ok_or_else(unsupported)?;
        Ok(match v {
            Some(v) => Pred::Ne(col, v),
            None => Pred::Never,
        })
    } else if let Some(idx) = c.find('=') {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        // sqlite spells equality `=` or `==` and treats them identically —
        // `name == ?` is a form CPython consumers write against
        // `sqlite_sequence`. (`!=` was taken above, so a second `=` here can
        // only be the doubled spelling.)
        let rest = &c[idx + 1..];
        let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest);
        let v = operand(rest, params).ok_or_else(unsupported)?;
        Ok(match v {
            Some(v) => Pred::Eq(col, v),
            None => Pred::Never,
        })
    } else {
        Err(unsupported())
    }
}

/// Split on top-level `AND` (case-insensitive), as a WORD — any whitespace on
/// either side, so a clause broken across lines (CPython's iterdump writes its
/// query that way) splits like a single-spaced one. No parenthesized-group
/// support.
fn split_and(w: &str) -> Vec<String> {
    let lower = w.to_ascii_lowercase();
    let b = lower.as_bytes();
    let word_edge = |i: usize| {
        i == 0 || {
            let c = b[i - 1];
            !(c.is_ascii_alphanumeric() || c == b'_')
        }
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 3 <= b.len() {
        let after = b.get(i + 3).copied();
        if &lower[i..i + 3] == "and"
            && i > 0
            && word_edge(i)
            && b[i - 1].is_ascii_whitespace()
            && after.is_some_and(|c| c.is_ascii_whitespace())
        {
            out.push(w[start..i].to_string());
            i += 3;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(w[start..].to_string());
    out
}

/// Extract a single-quoted string literal (the first one) from `s`.
fn str_literal(s: &str) -> Option<String> {
    let s = s.trim();
    let bytes = s.as_bytes();
    let start = s.find('\'')?;
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            return Some(out);
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    None
}
