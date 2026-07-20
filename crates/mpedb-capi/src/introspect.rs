//! Schema introspection the shim answers itself, because mpedb's SQL has no
//! `PRAGMA` and no `sqlite_master` table but ORMs/tools lean on both. Everything
//! here is a pure function of the live schema (`db.schema()`) plus the query
//! text; nothing touches the engine. Coverage is the common, canonical forms —
//! unsupported shapes fail loud (a clear error) rather than returning wrong
//! metadata.

use mpedb::{ColumnType, Error as DbError, Value};

/// Bootstrap/dead tables are hidden from introspection so a consumer sees only
/// the schema it created.
fn user_tables(schema: &mpedb::Schema) -> Vec<&mpedb::TableDef> {
    schema
        .tables
        .iter()
        .filter(|t| !t.dead && !t.name.is_empty() && t.name != crate::SEED_TABLE)
        .collect()
}

fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Int64 => "INTEGER",
        ColumnType::Float64 => "REAL",
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Text => "TEXT",
        ColumnType::Blob => "BLOB",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Any => "",
    }
}

/// Quote an identifier for SQL text, DOUBLING any embedded `"` (sqlite's own
/// rule, and what mpedb's tokenizer un-escapes). Identifiers may contain spaces
/// and punctuation, so the quoting is not optional; without the doubling a name
/// like `a"b` would emit `"a"b"`, which reparses as a DIFFERENT name.
fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Reconstruct a `CREATE TABLE` statement for the `sql` column of sqlite_master.
fn create_ddl(t: &mpedb::TableDef) -> String {
    let mut cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", q(&c.name), type_name(c.ty));
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if c.unique {
                s.push_str(" UNIQUE");
            }
            s.trim_end().to_string()
        })
        .collect();
    if !t.primary_key.is_empty() {
        let pk: Vec<String> = t
            .primary_key
            .iter()
            .filter_map(|&i| t.columns.get(i as usize))
            .map(|c| q(&c.name))
            .collect();
        cols.push(format!("PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {} ({})", q(&t.name), cols.join(", "))
}

// ------------------------------------------------------------------ PRAGMA

/// Parse `PRAGMA <name>[(<arg>)] | <name> = <value>` into `(name, arg)`.
pub(crate) fn parse_pragma(sql: &str) -> (String, Option<String>) {
    // Drop the leading `pragma` keyword.
    let rest = sql.trim_start();
    let rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
    let rest = rest.trim();
    // Name = leading identifier.
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let after = rest[name.len()..].trim_start();
    let arg = if let Some(a) = after.strip_prefix('(') {
        a.split(')').next().map(|s| unquote(s.trim()))
    } else {
        after.strip_prefix('=').map(|a| unquote(a.trim()))
    };
    (name, arg)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (f, l) = (b[0], b[b.len() - 1]);
        if (f == b'\'' && l == b'\'') || (f == b'"' && l == b'"') || (f == b'[' && l == b']') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn find_table<'a>(schema: &'a mpedb::Schema, name: &str) -> Option<&'a mpedb::TableDef> {
    user_tables(schema)
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

fn cols(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Answer a `PRAGMA` statement. Returns `(columns, rows)`; an unknown pragma is
/// a harmless empty result (matching sqlite's silence for no-op pragmas).
///
/// `busy_timeout_ms` is the connection's live busy timeout, passed in by
/// reference because `PRAGMA busy_timeout = N` is the ONE setter pragma the
/// shim can actually honour: it is the same knob `sqlite3_busy_timeout()` sets
/// and the retry loop in `lib.rs` reads. Every other setter stays a no-op *and
/// its getter keeps reporting what mpedb really does* — `synchronous` and
/// `cache_size` are deliberately NOT stored-and-echoed, because answering "3"
/// to a durability probe mpedb does not honour is a different answer rather
/// than an error, which is the one thing this shim must never do.
pub fn pragma(
    schema: &mpedb::Schema,
    sql: &str,
    busy_timeout_ms: &mut i32,
) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
    let (name, arg) = parse_pragma(sql);
    match name.to_ascii_lowercase().as_str() {
        "table_info" | "table_xinfo" => {
            let cols_out = cols(&["cid", "name", "type", "notnull", "dflt_value", "pk"]);
            let Some(t) = arg.as_deref().and_then(|a| find_table(schema, a)) else {
                return Ok((cols_out, vec![]));
            };
            let rows = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let pk = t
                        .primary_key
                        .iter()
                        .position(|&p| p as usize == i)
                        .map(|p| (p + 1) as i64)
                        .unwrap_or(0);
                    vec![
                        Value::Int(i as i64),
                        Value::Text(c.name.clone()),
                        Value::Text(type_name(c.ty).to_string()),
                        Value::Int(if c.nullable { 0 } else { 1 }),
                        Value::Null, // dflt_value: not reconstructed
                        Value::Int(pk),
                    ]
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
        "index_list" => {
            let cols_out = cols(&["seq", "name", "unique", "origin", "partial"]);
            let Some(t) = arg.as_deref().and_then(|a| find_table(schema, a)) else {
                return Ok((cols_out, vec![]));
            };
            let rows = t
                .indexes
                .iter()
                .enumerate()
                .map(|(i, ix)| {
                    vec![
                        Value::Int(i as i64),
                        Value::Text(format!("sqlite_autoindex_{}_{}", t.name, i + 1)),
                        Value::Int(ix.unique as i64),
                        Value::Text("c".into()),
                        Value::Int(0),
                    ]
                })
                .collect();
            Ok((cols_out, rows))
        }
        "foreign_key_list" => Ok((
            cols(&["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"]),
            vec![],
        )),
        "foreign_key_check" => Ok((cols(&["table", "rowid", "parent", "fkid"]), vec![])),
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
        // `foreign_keys` answers 0 — which is BOTH sqlite's own default and the
        // literal truth: mpedb parses `REFERENCES` and discards it, enforcing no
        // foreign key. The setter is a no-op, so `PRAGMA foreign_keys = ON`
        // followed by a read still reports 0. That divergence is deliberate:
        // reporting 1 would tell a consumer its FK violations will be caught
        // when they will not. See C-API-COMPAT gap D11.
        "foreign_keys" if arg.is_none() => Ok((cols(&["foreign_keys"]), vec![vec![Value::Int(0)]])),
        "journal_mode" => Ok((cols(&["journal_mode"]), vec![vec![Value::Text("memory".into())]])),
        "user_version" if arg.is_none() => {
            Ok((cols(&["user_version"]), vec![vec![Value::Int(0)]]))
        }
        "schema_version" if arg.is_none() => {
            Ok((cols(&["schema_version"]), vec![vec![Value::Int(0)]]))
        }
        // Every other pragma (synchronous, cache_size, foreign_keys=on, …) is a
        // no-op with no result — the common database-setup pragmas.
        _ => Ok((Vec::new(), Vec::new())),
    }
}

// -------------------------------------------------------------- sqlite_master

/// The five sqlite_master columns, in order.
const MASTER_COLS: [&str; 5] = ["type", "name", "tbl_name", "rootpage", "sql"];

/// Does `sql` read `sqlite_master`/`sqlite_schema`? (identifier match, so a
/// string literal containing the word does not trigger it).
pub fn references_sqlite_master(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    for kw in ["sqlite_master", "sqlite_schema"] {
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
        "sql" => Value::Text(r.sql.clone()),
        _ => Value::Null,
    }
}

/// Answer a `SELECT … FROM sqlite_master …`. Supports projecting any subset of
/// the five columns (or `*`, or `count(*)`), a `WHERE` of AND-joined
/// `col = 'lit'` / `col <> 'lit'` / `col IN ('a','b')` / `col [NOT] LIKE 'p'`
/// predicates, and `ORDER BY name`. Unsupported shapes → a clear error.
pub fn sqlite_master(
    schema: &mpedb::Schema,
    sql: &str,
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

    // Build the full candidate set (user tables only, for now).
    let mut rows: Vec<MasterRow> = user_tables(schema)
        .iter()
        .map(|t| MasterRow {
            ty: "table",
            name: t.name.clone(),
            tbl_name: t.name.clone(),
            sql: create_ddl(t),
        })
        .collect();

    // WHERE.
    if let Some(w) = &where_src {
        let preds = parse_where(w)?;
        rows.retain(|r| preds.iter().all(|p| p.matches(r)));
    }

    // ORDER BY name (the only ordering consumers use here). `order_src` is the
    // text after "ORDER", i.e. "BY name [DESC]" — strip the leading "BY" before
    // matching the column.
    if let Some(o) = &order_src {
        let ol = o.to_ascii_lowercase();
        let ol = ol.strip_prefix("by").map(str::trim_start).unwrap_or(ol.as_str());
        if ol.starts_with("name") {
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            if ol.contains("desc") {
                rows.reverse();
            }
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
    /// A clause-leading `NOT` (Django's introspection writes
    /// `AND NOT name='sqlite_sequence'`).
    Not(Box<Pred>),
}

impl Pred {
    fn matches(&self, r: &MasterRow) -> bool {
        let val = |c: &str| match c {
            "type" => r.ty.to_string(),
            "name" => r.name.clone(),
            "tbl_name" => r.tbl_name.clone(),
            _ => String::new(),
        };
        match self {
            Pred::Eq(c, v) => val(c) == *v,
            Pred::Ne(c, v) => val(c) != *v,
            Pred::In(c, vs) => vs.iter().any(|v| *v == val(c)),
            Pred::Like(c, pat, neg) => like_match(&val(c), pat) != *neg,
            Pred::Not(inner) => !inner.matches(r),
        }
    }
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

fn parse_where(w: &str) -> Result<Vec<Pred>, DbError> {
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
        let p = parse_cmp(c)?;
        preds.push(if negate { Pred::Not(Box::new(p)) } else { p });
    }
    Ok(preds)
}

/// One comparison of a `sqlite_master` WHERE clause. A shape this does not
/// recognize is REFUSED — including anything containing a top-level `OR`, whose
/// operands this AND-only evaluator would otherwise silently drop and answer
/// wrongly.
fn parse_cmp(c: &str) -> Result<Pred, DbError> {
    let cl = c.to_ascii_lowercase();
    if cl.starts_with("or ") || cl.contains(" or ") {
        return Err(unsupported());
    }
    let col_of = |c: &str| {
        let t = c.trim().trim_matches('"').to_ascii_lowercase();
        if ["type", "name", "tbl_name"].contains(&t.as_str()) {
            Some(t)
        } else {
            None
        }
    };
    if let Some(idx) = cl.find(" not like ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let pat = str_literal(&c[idx + 10..]).ok_or_else(unsupported)?;
        Ok(Pred::Like(col, pat, true))
    } else if let Some(idx) = cl.find(" like ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let pat = str_literal(&c[idx + 6..]).ok_or_else(unsupported)?;
        Ok(Pred::Like(col, pat, false))
    } else if let Some(idx) = cl.find(" in ") {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let list = &c[idx + 4..];
        let inner = list.trim().trim_start_matches('(').trim_end_matches(')');
        let vals: Option<Vec<String>> = inner.split(',').map(str_literal).collect();
        Ok(Pred::In(col, vals.ok_or_else(unsupported)?))
    } else if let Some(idx) = cl.find("!=").or_else(|| cl.find("<>")) {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let v = str_literal(&c[idx + 2..]).ok_or_else(unsupported)?;
        Ok(Pred::Ne(col, v))
    } else if let Some(idx) = c.find('=') {
        let col = col_of(&c[..idx]).ok_or_else(unsupported)?;
        let v = str_literal(&c[idx + 1..]).ok_or_else(unsupported)?;
        Ok(Pred::Eq(col, v))
    } else {
        Err(unsupported())
    }
}

/// Split on top-level ` AND ` (case-insensitive). No parenthesized-group support.
fn split_and(w: &str) -> Vec<String> {
    let lower = w.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let bytes = lower.as_bytes();
    while i + 5 <= bytes.len() {
        if &lower[i..i + 5] == " and " {
            out.push(w[start..i].to_string());
            i += 5;
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
