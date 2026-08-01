//! Schema introspection the shim answers itself, because mpedb's SQL has no
//! `PRAGMA` and no `sqlite_master` table but ORMs/tools lean on both. Everything
//! here is a pure function of the live schema (`db.schema()`) plus the query
//! text; nothing touches the engine. Coverage is the common, canonical forms —
//! unsupported shapes fail loud (a clear error) rather than returning wrong
//! metadata.

use mpedb::ColumnType;

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

// ----------------------------------------------------- index identity (#119)

/// System-record namespace holding the shim's `CREATE INDEX` text, keyed by the
/// index's exact name.
///
/// **Why the shim has to own this.** mpedb's [`mpedb::IndexDef`] carries the key
/// columns, the UNIQUE bit and the partial predicate — and NO NAME. The engine
/// never needed one (`index_no` = position + 1 addresses a tree, and mpedb has
/// no `DROP INDEX`), but sqlite's catalog is name-addressed: `sqlite_master`
/// reports `name`, `PRAGMA index_list` reports it, and Django's `get_constraints`
/// round-trips it through `SELECT sql FROM sqlite_master WHERE type='index' AND
/// name=?`. Without a name, an index cannot appear in `sqlite_master` at all,
/// and a dump that loses its indexes replays into a different schema.
///
/// So the shim records `name → (shape fingerprint, verbatim CREATE INDEX)` in
/// the catalog's sys-keyspace, exactly as it already does for `CREATE TABLE`
/// text ([`DDL_NS`]): it rides the DDL's own write transaction, commits and
/// rolls back with it, and every process sees it.
pub(crate) const IDX_NS: &str = "capiidx";

/// The SHAPE of one index, as a string that is equal for two `IndexDef`s iff
/// they are the same index. Column NAMES, not ordinals: `ALTER TABLE … DROP
/// COLUMN` renumbers every ordinal after the dropped one, and a fingerprint that
/// moved under an unrelated drop would orphan the record.
///
/// `apply_create_index` treats an identical `(columns, unique, predicate)` as a
/// no-op, so within one table this really is a key.
pub(crate) fn index_fingerprint_of(t: &mpedb::TableDef, at: usize) -> Option<String> {
    let ix = t.indexes.get(at)?;
    let cols: Vec<&str> = ix
        .columns
        .iter()
        .filter_map(|&i| t.columns.get(i as usize))
        .map(|c| c.name.as_str())
        .collect();
    Some(format!(
        "{}\u{1}{}\u{1}{}\u{1}{}",
        t.name,
        ix.unique as u8,
        cols.join("\u{2}"),
        ix.predicate.as_deref().unwrap_or("")
    ))
}

/// The record's value: `fingerprint ‖ NUL ‖ verbatim CREATE INDEX`.
pub(crate) fn index_record(fingerprint: &str, verbatim: &str) -> Vec<u8> {
    let mut v = fingerprint.as_bytes().to_vec();
    v.push(0);
    v.extend_from_slice(verbatim.as_bytes());
    v
}

/// The fingerprint an index record was filed under (its first field), for the
/// `DROP TABLE` sweep that forgets a table's index names.
pub(crate) fn index_record_fingerprint(rec: &[u8]) -> Option<&str> {
    let cut = rec.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rec[..cut]).ok()
}

/// The table a fingerprint belongs to (its first field).
pub(crate) fn fingerprint_table(fp: &str) -> &str {
    fp.split('\u{1}').next().unwrap_or("")
}

/// The shim's `CREATE INDEX` records, keyed by index NAME — the same key they
/// are STORED and tombstoned under — mapping to the verbatim `CREATE INDEX`.
/// Built once per introspection statement from one scan.
///
/// Keyed by name rather than by shape (`index_fingerprint_of`), which is what
/// this used to do, because the shape is not an identity:
///
/// * `ALTER TABLE … RENAME COLUMN` changes the shape of every index over the
///   renamed column, orphaning a record that is perfectly valid. The index then
///   reads back as a `constraint` row with a NULL `sql` and consumers that need
///   the text — Django's introspection among them — silently skip it.
/// * Two indexes of the same shape are legal (merely redundant) and sqlite
///   builds both; under a shape key they collide onto one record.
///
/// The fingerprint stays in the record's VALUE, where `forget_table_index_records`
/// reads it to attribute a record to its table during the `DROP TABLE` sweep.
pub(crate) type IndexRecords = std::collections::HashMap<String, String>;

/// Fold a raw `IDX_NS` scan (`name → fingerprint ‖ NUL ‖ sql`) into the
/// name-keyed map the readers below use. Empty values are tombstones.
pub(crate) fn index_records(raw: Vec<(Vec<u8>, Vec<u8>)>) -> IndexRecords {
    let mut out = IndexRecords::new();
    for (k, v) in raw {
        if v.is_empty() {
            continue;
        }
        let Ok(name) = String::from_utf8(k) else { continue };
        let Some(cut) = v.iter().position(|&b| b == 0) else { continue };
        let (Ok(fp), Ok(sql)) = (
            std::str::from_utf8(&v[..cut]),
            std::str::from_utf8(&v[cut + 1..]),
        ) else {
            continue;
        };
        // A record with no fingerprint is malformed — the `DROP TABLE` sweep
        // could not attribute it — so it is not trusted for reading either.
        if fp.is_empty() || sql.is_empty() {
            continue;
        }
        out.insert(name, sql.to_string());
    }
    out
}

/// One row of sqlite's index catalog for a table, as this shim can answer it.
#[derive(Clone)]
struct IndexRow {
    name: String,
    unique: bool,
    /// sqlite's `PRAGMA index_list` origin: `c` = `CREATE INDEX`, `u` = a
    /// `UNIQUE` constraint in the `CREATE TABLE`, `pk` = the `PRIMARY KEY`.
    origin: &'static str,
    partial: bool,
    /// `sqlite_master.sql`. `None` for a constraint index, which sqlite reports
    /// with a NULL `sql` — the signal Django's `get_constraints` uses to skip
    /// inline constraints it has already parsed out of the `CREATE TABLE`.
    sql: Option<String>,
    /// Key columns as ordinals into `TableDef::columns`.
    columns: Vec<u16>,
}

/// The columns of the index sqlite would create for `t`'s PRIMARY KEY, or
/// `None` when it would create none.
///
/// sqlite builds no index for a rowid alias — an `INTEGER PRIMARY KEY`, which is
/// exactly what mpedb's hidden rowid (#94) and a single Int64 PK are — and
/// `sqlite_autoindex_<t>_<k>` for every other PK. Probed: `CREATE TABLE o (a
/// INTEGER PRIMARY KEY, …)` yields no `pk` row, `a TEXT PRIMARY KEY` and
/// `PRIMARY KEY (a, b)` each yield one.
fn pk_index_columns(t: &mpedb::TableDef) -> Option<Vec<u16>> {
    if t.implicit_rowid || t.primary_key.is_empty() {
        return None;
    }
    if t.primary_key.len() == 1 {
        let c = t.columns.get(t.primary_key[0] as usize)?;
        if c.ty == ColumnType::Int64 {
            return None;
        }
    }
    Some(t.primary_key.clone())
}

/// Every index row sqlite would report for `t`, in CREATION order (which is
/// `sqlite_master`'s order; `PRAGMA index_list` reports the reverse).
///
/// Constraint indexes come first because mpedb's `TableDef::indexes` puts the
/// flag-derived entries ahead of anything `CREATE INDEX` appended, and the
/// PRIMARY KEY's synthetic entry is slotted among them by first-column ordinal
/// — which reproduces sqlite's `sqlite_autoindex_<t>_<k>` numbering on every
/// shape the oracle was probed with (PK first, PK in the middle, PK after a
/// table-level UNIQUE, INTEGER PK with no index at all).
///
/// It is a RECONSTRUCTION of a declaration order mpedb does not store, so it
/// can differ: a table-level `PRIMARY KEY (b)` written BEFORE a `UNIQUE (a)`
/// swaps the two numbers. Both engines emit synthetic names with a NULL `sql`
/// there, and both are internally consistent across `index_list`/`index_info`/
/// `sqlite_master`, which is what every consumer of them actually depends on.
fn table_index_rows(t: &mpedb::TableDef, recs: &IndexRecords) -> Vec<IndexRow> {
    let mut constraint: Vec<IndexRow> = Vec::new();
    let mut created: Vec<IndexRow> = Vec::new();
    for ix in t.indexes.iter() {
        let row = IndexRow {
            name: String::new(),
            unique: ix.unique,
            origin: "u",
            partial: ix.predicate.is_some(),
            sql: None,
            columns: ix.columns.clone(),
        };
        // An index is `CREATE INDEX`-origin exactly when it carries a name AND
        // the shim holds a record under that name. A flag-derived index has no
        // name, so it lands in `constraint` — which is where it belongs.
        match ix.name.as_deref().and_then(|n| recs.get(n).map(|sql| (n, sql))) {
            Some((name, sql)) => created.push(IndexRow {
                name: name.to_string(),
                origin: "c",
                sql: Some(sql.clone()),
                ..row
            }),
            None => constraint.push(row),
        }
    }
    if let Some(pk) = pk_index_columns(t) {
        let at = constraint
            .iter()
            .position(|r| r.columns.first() > pk.first())
            .unwrap_or(constraint.len());
        constraint.insert(
            at,
            IndexRow {
                name: String::new(),
                unique: true,
                origin: "pk",
                partial: false,
                sql: None,
                columns: pk,
            },
        );
    }
    for (i, r) in constraint.iter_mut().enumerate() {
        r.name = format!("sqlite_autoindex_{}_{}", t.name, i + 1);
    }
    constraint.extend(created);
    constraint
}

/// The `cid` `PRAGMA table_info` gives column `ord` of `t`.
///
/// Not the raw ordinal: `table_info` lists only the visible, non-generated
/// columns and numbers what it lists 0..n (see the `table_info` arm). A
/// consumer joins `index_info.cid` against those rows, so the two have to agree.
/// A column `table_info` does not list falls back to its raw ordinal.
fn table_info_cid(t: &mpedb::TableDef, ord: u16) -> i64 {
    let vis = t.visible_columns();
    if (ord as usize) >= vis.len() || vis[ord as usize].generated.is_some() {
        return ord as i64;
    }
    vis[..ord as usize].iter().filter(|c| c.generated.is_none()).count() as i64
}

/// A minimal word/identifier reader over the head of a DDL statement. Only ever
/// walks the few tokens before the column list, so it does not need to know
/// SQL — just sqlite's four identifier quotings and where a word ends.
struct DdlWords<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> DdlWords<'a> {
    fn new(s: &'a str) -> Self {
        DdlWords { s, at: 0 }
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.s[self.at..].chars().next() {
            if c.is_whitespace() {
                self.at += c.len_utf8();
            } else {
                break;
            }
        }
    }
    /// Is the very next character a `.`? (a `schema.name` qualifier)
    fn peek_dot(&mut self) -> bool {
        self.skip_ws();
        self.s[self.at..].starts_with('.')
    }
    /// The next bare word or quoted identifier, unquoted, with the byte offset
    /// the token STARTS at (sqlite's stored DDL begins at the name token). `.`
    /// is returned as itself so a qualifier can be stepped over.
    fn word(&mut self) -> Option<(String, usize)> {
        self.skip_ws();
        let start = self.at;
        self.word_at().map(|w| (w, start))
    }
    fn word_at(&mut self) -> Option<String> {
        let rest = &self.s[self.at..];
        let first = rest.chars().next()?;
        let (close, esc) = match first {
            '"' => ('"', true),
            '`' => ('`', true),
            '[' => (']', false),
            '.' => {
                self.at += 1;
                return Some(".".into());
            }
            _ => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '(' || c == '.' || c == ',')
                    .unwrap_or(rest.len());
                if end == 0 {
                    return None;
                }
                self.at += end;
                return Some(rest[..end].to_string());
            }
        };
        // Quoted: scan to the closing delimiter, honoring sqlite's doubling.
        let body = &rest[first.len_utf8()..];
        let mut out = String::new();
        let mut i = 0;
        while let Some(c) = body[i..].chars().next() {
            i += c.len_utf8();
            if c == close {
                if esc && body[i..].starts_with(close) {
                    out.push(close);
                    i += close.len_utf8();
                    continue;
                }
                self.at += first.len_utf8() + i;
                return Some(out);
            }
            out.push(c);
        }
        None
    }
}

// --------------------------------------------------------------- split seams
// The old single-file module, split at its section banners (S40x). mod.rs
// keeps the shared helpers and the index-row machinery every sibling uses;
// the re-exports below keep every external path identical.
mod ddl;
mod master;
mod pragma;

pub(crate) use ddl::{
    alter_rename_target, ddl_key, ddl_record, ddl_record_verbatim, ddl_verbatim, exact_table_name,
    fts4_shadow_sql, object_ddl_record, object_ddl_text, schema_ddl_target, table_by_exact_name,
    DdlKind, DdlTarget, DDL_NS,
};
pub use master::{
    master_reference, master_schema, qualified_temp_master, references_sqlite_master,
    references_sqlite_sequence, sqlite_master, sqlite_sequence_query, MasterRef, MasterSource,
};
pub(crate) use pragma::{names_a_table, parse_pragma, pragma_cols};
pub use pragma::{pragma, pragma_schema, EchoPragmas};
