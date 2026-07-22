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
///
/// The HIDDEN implicit rowid (#94) is elided — column AND primary key. It is
/// not part of the statement the caller wrote, `SELECT *` does not expose it,
/// and emitting it here makes the dump replay as a DIFFERENT table (one with
/// an explicit `rowid` column and an explicit PK).
///
/// This is a RECONSTRUCTION from the live schema, not the caller's original
/// text: mpedb's schema stores the resolved types and constraints, not the
/// bytes of the `CREATE TABLE`. It round-trips semantically, but a consumer
/// diffing it against what it wrote sees the canonical spelling. It is the
/// FALLBACK — `master_sql` prefers the caller's own text when the shim
/// recorded it (`DDL_NS`) and it still describes this exact shape.
///
/// A column-level `UNIQUE` is emitted only for a constraint the `CREATE TABLE`
/// itself declared. mpedb's canonical-bytes decode rebuilds `ColumnDef::unique`
/// from the index list ("a single-column index marks its column"), so after
/// `CREATE UNIQUE INDEX ux ON t(a)` the flag is set on a column the caller
/// declared plain — and a reconstruction that echoed it would replay as a table
/// with a constraint index the original never had, ON TOP of the named index the
/// dump also re-creates. `idx` is what separates the two: a unique index the
/// shim has a `CREATE UNIQUE INDEX` record for is not a column constraint.
fn create_ddl(t: &mpedb::TableDef, idx: &IndexRecords) -> String {
    let hidden_pk = t.hidden_rowid_col();
    let by_statement: Vec<u16> = table_index_rows(t, idx)
        .into_iter()
        .filter(|r| r.origin == "c" && r.unique && r.columns.len() == 1)
        .map(|r| r.columns[0])
        .collect();
    let mut cols: Vec<String> = t
        .visible_columns()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut s = format!("{} {}", q(&c.name), type_name(c.ty));
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if c.unique && !by_statement.contains(&(i as u16)) {
                s.push_str(" UNIQUE");
            }
            // A GENERATED column MUST carry its clause: without it the replayed
            // statement makes an ordinary column, and the dump's INSERTs — which
            // take their column list from `table_info`, where a generated column
            // is correctly absent — would then leave it permanently NULL.
            if let Some(g) = &c.generated {
                s.push_str(&format!(
                    " GENERATED ALWAYS AS ({}) {}",
                    g.expr,
                    g.kind.keyword()
                ));
            }
            s.trim_end().to_string()
        })
        .collect();
    if !t.primary_key.is_empty() && t.primary_key != [hidden_pk.unwrap_or(u16::MAX)] {
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

// ------------------------------------------- verbatim CREATE TABLE text (#118)

/// System-record namespace holding the shim's verbatim `CREATE …` text,
/// keyed by the object's exact name. Written by `lib::record_object_ddl` when a
/// `CREATE TABLE`/`VIEW`/`TRIGGER` succeeds; read back below.
///
/// **Why store it at all.** sqlite's `sqlite_master.sql` is the caller's own
/// statement, byte for byte, and consumers diff against it: CPython's
/// `test_dump_custom_row_factory` asserts `iterdump()` re-emits
/// `CREATE TABLE test(t);` exactly. mpedb's catalog keeps the *resolved* schema,
/// not the bytes, so `create_ddl` can only produce a canonical spelling
/// (`CREATE TABLE "test" ("t")`) — semantically identical, textually different.
/// Keeping the original text in the catalog's sys-keyspace closes that gap
/// without any engine change: it rides the same write transaction as the DDL
/// (so it commits and rolls back with it) and is visible to every process.
pub(crate) const DDL_NS: &str = "capiddl";

/// The record's value: the reconstruction as it stood WHEN THE TEXT WAS
/// RECORDED, a NUL, then the verbatim statement.
///
/// The reconstruction is the staleness fingerprint. `sqlite_master` re-derives
/// `create_ddl` from the live table and uses the verbatim text ONLY when the
/// two still agree; anything that changed the table's shape (`ALTER TABLE ADD`/
/// `DROP`/`RENAME COLUMN`, a drop-and-recreate that the record outlived) makes
/// them differ and falls back to the reconstruction. That ordering matters: an
/// *almost* right `CREATE TABLE` replays as a DIFFERENT table, which is worse
/// than a canonical one, so the verbatim text is used only where it is
/// provably still the text that produced this exact shape.
pub(crate) fn ddl_record(t: &mpedb::TableDef, idx: &IndexRecords, verbatim: &str) -> Vec<u8> {
    let mut v = create_ddl(t, idx).into_bytes();
    v.push(0);
    v.extend_from_slice(verbatim.as_bytes());
    v
}

/// The namespace + key a table's verbatim-DDL record lives under.
pub(crate) fn ddl_key(table: &str) -> (&'static str, Vec<u8>) {
    (DDL_NS, table.as_bytes().to_vec())
}

/// The `sql` text for a table: the caller's own statement when a record for it
/// is present AND still describes this exact shape, else the reconstruction.
fn master_sql(t: &mpedb::TableDef, idx: &IndexRecords, rec: Option<&Vec<u8>>) -> String {
    let recon = create_ddl(t, idx);
    let Some(rec) = rec else { return recon };
    let Some(cut) = rec.iter().position(|&b| b == 0) else {
        return recon;
    };
    if rec[..cut] != *recon.as_bytes() {
        return recon;
    }
    match std::str::from_utf8(&rec[cut + 1..]) {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => recon,
    }
}

/// The name a table is STORED under, given any spelling of it. Records are
/// keyed by the stored name so `sqlite_master` can look them up by the name it
/// reports, whatever case the `CREATE` used.
pub(crate) fn exact_table_name(schema: &mpedb::Schema, name: &str) -> Option<String> {
    find_table(schema, name).map(|t| t.name.clone())
}

/// The table stored under exactly `name` (no case folding).
pub(crate) fn table_by_exact_name<'a>(
    schema: &'a mpedb::Schema,
    name: &str,
) -> Option<&'a mpedb::TableDef> {
    user_tables(schema).into_iter().find(|t| t.name == name)
}

/// Which catalog object a DDL statement creates or drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DdlKind {
    Table,
    View,
    Trigger,
    /// `CREATE [UNIQUE] INDEX` — the `UNIQUE` is part of the head sqlite
    /// rebuilds, so it has to travel with the kind.
    Index { unique: bool },
}

impl DdlKind {
    fn head(self) -> &'static str {
        match self {
            DdlKind::Table => "CREATE TABLE ",
            DdlKind::View => "CREATE VIEW ",
            DdlKind::Trigger => "CREATE TRIGGER ",
            DdlKind::Index { unique: false } => "CREATE INDEX ",
            DdlKind::Index { unique: true } => "CREATE UNIQUE INDEX ",
        }
    }
}

/// What a `CREATE`/`DROP` statement targets, as [`schema_ddl_target`] read it.
#[derive(Clone, Debug)]
pub(crate) struct DdlTarget {
    pub kind: DdlKind,
    /// `true` for `CREATE …`, `false` for `DROP …`.
    pub create: bool,
    /// The object's name, unquoted, without any `schema.` qualifier.
    pub name: String,
    /// Byte offset of the name token within the trivia-stripped statement —
    /// where sqlite's stored `sql` text begins.
    pub name_at: usize,
    /// `CREATE INDEX … ON <table>`: the table the index is built over. `None`
    /// for every other kind (and for `DROP INDEX`, which does not name one).
    pub on_table: Option<String>,
}

/// The text sqlite would store in `sqlite_master.sql` for a CREATE.
///
/// **Not the raw bytes** — sqlite reconstructs the head and keeps only the
/// tail. `sqlite3EndTable` builds `"CREATE %s %.*s"` from the *name token*
/// onwards, so everything before the name is normalized away and everything
/// from it is verbatim. Verified against sqlite 3.45, which is the only way to
/// get this right; four of these were not guessable:
///
/// | written | stored |
/// |---|---|
/// | `create table t3(a)` | `CREATE TABLE t3(a)` — the head is UPPERCASED |
/// | `CREATE  TABLE  t2 ( a )` | `CREATE TABLE t2 ( a )` — head spacing normalized, tail kept |
/// | `CREATE TABLE IF NOT EXISTS t4(a)` | `CREATE TABLE t4(a)` — `IF NOT EXISTS` is GONE |
/// | `CREATE TABLE main.t5(a)` | `CREATE TABLE t5(a)` — the qualifier is GONE |
/// | `CREATE TABLE t9(a) -- c` | `CREATE TABLE t9(a)` — the tail ends at the last TOKEN |
///
/// The same rule applies to `VIEW` and `TRIGGER` (CPython `test_table_dump`
/// asserts the caller's spelling of both). `name_at` is the byte offset of
/// the name token within the trivia-stripped statement.
///
/// **`CREATE INDEX` ends differently**, and the difference is not a detail:
/// `sqlite3CreateIndex` measures to the end of the LAST TOKEN and then drops a
/// single trailing `;`, so whitespace and comments sitting between the last real
/// token and the `;` are KEPT — where `sqlite3EndTable` measures to the closing
/// `)` and keeps neither. Probed against the bundled 3.45.0 oracle:
///
/// | written | stored |
/// |---|---|
/// | `create   table   spaced ( a  int ) ;` | `CREATE TABLE spaced ( a  int )` |
/// | `create index   ixs   on spaced ( a )   ;  -- trail` | `CREATE INDEX ixs   on spaced ( a )   ` — three trailing spaces |
/// | `CREATE INDEX ix9 ON t9(a) /* mid */ ;` | `CREATE INDEX ix9 ON t9(a) /* mid */ ` — comment kept |
/// | `create unique index u on t(a)` | `CREATE UNIQUE INDEX u on t(a)` — `UNIQUE` is part of the rebuilt head |
///
/// The shim is handed statements with the `;` already removed by
/// [`crate::sql::split_first`], so "to the end of the text" IS sqlite's answer
/// for every `;`-terminated `CREATE INDEX`. The one shape it cannot match is a
/// `CREATE INDEX` with trailing whitespace/comments and NO terminator at all,
/// where sqlite stops at the last token and this keeps the trivia.
pub(crate) fn ddl_verbatim(sql: &str, name_at: usize, kind: DdlKind) -> String {
    let s = crate::sql::strip_leading_trivia(sql);
    let end = match kind {
        DdlKind::Index { .. } => s.len(),
        _ => stmt_text_end(s),
    };
    if name_at >= end || !s.is_char_boundary(name_at) || !s.is_char_boundary(end) {
        return String::new();
    }
    format!("{}{}", kind.head(), &s[name_at..end])
}

/// The byte offset just past the LAST token of `s` — where sqlite's stored
/// statement text ends. Trailing whitespace, `;`, and `--`/`/* */` comments are
/// not tokens; a `;` or comment marker inside a quoted string or identifier is
/// not one either, which is the whole reason this is a scan and not a `trim`.
fn stmt_text_end(s: &str) -> usize {
    let b = s.as_bytes();
    let (mut i, mut end) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c | b';' => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // A doubled delimiter is an escaped one, not the close.
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                end = i;
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                end = i;
            }
            _ => {
                i += 1;
                end = i;
            }
        }
    }
    end
}

/// The object a `CREATE`/`DROP` `TABLE`/`VIEW`/`TRIGGER`/`INDEX` names, if it
/// is one.
///
/// Handles `CREATE [TEMP|TEMPORARY] [UNIQUE] {TABLE|VIEW|TRIGGER|INDEX}
/// [IF NOT EXISTS] [schema.]name` and the matching `DROP … [IF EXISTS]`, with
/// the name in any of sqlite's quotings, plus the `ON <table>` an index carries.
/// A `VIRTUAL TABLE` or an `ALTER` → `None`.
pub(crate) fn schema_ddl_target(sql: &str) -> Option<DdlTarget> {
    let mut w = DdlWords::new(crate::sql::strip_leading_trivia(sql));
    let create = match w.word()?.0.to_ascii_lowercase().as_str() {
        "create" => true,
        "drop" => false,
        _ => return None,
    };
    let mut kw = w.word()?.0.to_ascii_lowercase();
    if create && (kw == "temp" || kw == "temporary") {
        kw = w.word()?.0.to_ascii_lowercase();
    }
    // `CREATE VIRTUAL TABLE` is not an ordinary table (and has no reconstruction).
    if create && kw == "virtual" {
        return None;
    }
    let mut unique = false;
    if create && kw == "unique" {
        unique = true;
        kw = w.word()?.0.to_ascii_lowercase();
    }
    let kind = match kw.as_str() {
        "table" => DdlKind::Table,
        "view" => DdlKind::View,
        "trigger" => DdlKind::Trigger,
        "index" => DdlKind::Index { unique },
        _ => return None,
    };
    // `IF NOT EXISTS` / `IF EXISTS`.
    let (mut name, mut at) = w.word()?;
    if name.eq_ignore_ascii_case("if") {
        let mut nx = w.word()?.0;
        if nx.eq_ignore_ascii_case("not") {
            nx = w.word()?.0;
        }
        if !nx.eq_ignore_ascii_case("exists") {
            return None;
        }
        (name, at) = w.word()?;
    }
    // A `schema.name` qualifier: the name is the component AFTER the dot, and
    // sqlite's stored text starts there too.
    if w.peek_dot() {
        let _ = w.word(); // the '.' itself
        (name, at) = w.word()?;
    }
    // `CREATE INDEX <name> ON <table> (…)`: the table the index belongs to.
    // Read here rather than re-derived later because a `DROP TABLE` has to be
    // able to forget the index records that named it.
    let on_table = if matches!(kind, DdlKind::Index { .. }) && create {
        let (on, _) = w.word()?;
        if !on.eq_ignore_ascii_case("on") {
            return None;
        }
        let (mut t, _) = w.word()?;
        if w.peek_dot() {
            let _ = w.word();
            t = w.word()?.0;
        }
        Some(t)
    } else {
        None
    };
    Some(DdlTarget { kind, create, name, name_at: at, on_table })
}

/// A view/trigger verbatim record: no shape fingerprint (there is no
/// reconstruction that could stale the same way as `ALTER TABLE`). Value is
/// `b"\0" ‖ verbatim` so it still round-trips through the table reader as
/// "empty fingerprint → always use the text".
pub(crate) fn object_ddl_record(verbatim: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + verbatim.len());
    v.push(0);
    v.extend_from_slice(verbatim.as_bytes());
    v
}

/// The `sql` text from a view/trigger verbatim record, if present.
pub(crate) fn object_ddl_text(rec: Option<&Vec<u8>>) -> Option<String> {
    let rec = rec?;
    let cut = rec.iter().position(|&b| b == 0)?;
    // Empty fingerprint (views/triggers) or a matching one: use the tail.
    match std::str::from_utf8(&rec[cut + 1..]) {
        Ok(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
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

/// The shim's `CREATE INDEX` records, keyed by SHAPE so a live `IndexDef` can
/// find its own name. Built once per introspection statement from one scan.
pub(crate) type IndexRecords = std::collections::HashMap<String, (String, String)>;

/// Fold a raw `IDX_NS` scan (`name → fingerprint ‖ NUL ‖ sql`) into the
/// shape-keyed map the readers below use. Empty values are tombstones.
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
        if fp.is_empty() || sql.is_empty() {
            continue;
        }
        out.insert(fp.to_string(), (name, sql.to_string()));
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
    for (at, ix) in t.indexes.iter().enumerate() {
        let row = IndexRow {
            name: String::new(),
            unique: ix.unique,
            origin: "u",
            partial: ix.predicate.is_some(),
            sql: None,
            columns: ix.columns.clone(),
        };
        match index_fingerprint_of(t, at).and_then(|fp| recs.get(&fp)) {
            Some((name, sql)) => created.push(IndexRow {
                name: name.clone(),
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
    idx: &IndexRecords,
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
                        Value::Text(type_name(c.ty).to_string()),
                        Value::Int(if c.nullable { 0 } else { 1 }),
                        Value::Null, // dflt_value: not reconstructed
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
pub fn sqlite_master(
    schema: &mpedb::Schema,
    sql: &str,
    params: &[Value],
    verbatim: &std::collections::HashMap<String, Vec<u8>>,
    idx: &IndexRecords,
    views: &[(String, String)],
    triggers: &[(String, String, String)],
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

    // WHERE.
    if let Some(w) = &where_src {
        let preds = parse_where(w, params)?;
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
        let val = |c: &str| match c {
            "type" => r.ty.to_string(),
            "name" => r.name.clone(),
            "tbl_name" => r.tbl_name.clone(),
            "rootpage" => "0".to_string(),
            "sql" => r.sql.clone(),
            _ => String::new(),
        };
        match self {
            Pred::Eq(c, v) => val(c) == *v,
            Pred::Ne(c, v) => val(c) != *v,
            Pred::In(c, vs) => vs.iter().any(|v| *v == val(c)),
            Pred::Like(c, pat, neg) => like_match(&val(c), pat) != *neg,
            // `sql` is the only column that is ever NULL here — a constraint
            // index has no statement text, in sqlite's catalog and in this one.
            Pred::Null(c, negated) => val(c).is_empty() != *negated,
            Pred::Not(inner) => !inner.matches(r),
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

fn parse_where(w: &str, params: &[Value]) -> Result<Vec<Pred>, DbError> {
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
        let p = parse_cmp(c, params)?;
        preds.push(if negate { Pred::Not(Box::new(p)) } else { p });
    }
    Ok(preds)
}

/// One comparison of a `sqlite_master` WHERE clause. A shape this does not
/// recognize is REFUSED — including anything containing a top-level `OR`, whose
/// operands this AND-only evaluator would otherwise silently drop and answer
/// wrongly.
fn parse_cmp(c: &str, params: &[Value]) -> Result<Pred, DbError> {
    let cl = c.to_ascii_lowercase();
    if cl.starts_with("or ") || cl.contains(" or ") {
        return Err(unsupported());
    }
    let col_of = |c: &str| {
        let t = c
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
            .to_ascii_lowercase();
        if MASTER_COLS.contains(&t.as_str()) {
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
        let v = operand(&c[idx + 1..], params).ok_or_else(unsupported)?;
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
