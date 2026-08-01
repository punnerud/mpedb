use super::pragma::find_table;
use super::{q, table_index_rows, type_name, user_tables, DdlWords, IndexRecords};

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
    let rows = table_index_rows(t, idx);
    let single_unique = |origin: &str| -> Vec<u16> {
        rows.iter()
            .filter(|r| r.origin == origin && r.unique && r.columns.len() == 1)
            .map(|r| r.columns[0])
            .collect()
    };
    let by_statement = single_unique("c");
    // …unless the column ALSO carries an independent constraint index. Two
    // same-shape indexes are legal now, so `UNIQUE` in the declaration and a
    // named `CREATE UNIQUE INDEX` over the same column can both exist — and
    // suppressing the inline word then drops a real constraint from the
    // reconstruction, which replays as a table missing an index it had.
    let by_constraint = single_unique("u");
    let mut cols: Vec<String> = t
        .visible_columns()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut s = format!("{} {}", q(&c.name), type_name(c.ty));
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            let i = i as u16;
            if c.unique && (!by_statement.contains(&i) || by_constraint.contains(&i)) {
                s.push_str(" UNIQUE");
            }
            // A declared collation is part of the column, not decoration: it
            // decides comparison and index ordering, so a reconstruction that
            // dropped it replays as a BINARY column. It is also the only thing
            // a caller can read the collation back from — `PRAGMA table_info`
            // has no column for it, which is why Django looks for the token in
            // `sqlite_master.sql`.
            if c.collation != mpedb::Collation::Binary {
                s.push_str(&format!(" COLLATE {}", c.collation.name()));
            }
            // A CHECK is a real constraint the replay must re-create — and it
            // is what Django's `get_constraints` reads back out of
            // `sqlite_master.sql`. Omitting it made every rebuilt/renamed
            // table LOOK unconstrained while the compiled program still
            // enforced it (the rename test measured exactly that: 0 checks
            // reported, `x > 10` still firing).
            if let Some(chk) = &c.check {
                s.push_str(&format!(" CHECK ({chk})"));
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

/// sqlite's EXACT shadow-table DDL for an fts4 virtual table (measured on
/// 3.45.1, byte for byte — single-quoted names, typeless content columns,
/// `PRIMARY KEY(level, idx)` spacing included). These are the `sql` texts
/// iterdump re-emits, stored as the VERBATIM half of each shadow's record.
pub(crate) fn fts4_shadow_sql(vtab: &str, content_cols: &[String]) -> Vec<(String, String)> {
    let cols: String = content_cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!(", 'c{i}{c}'"))
        .collect();
    vec![
        (
            format!("{vtab}_content"),
            format!("CREATE TABLE '{vtab}_content'(docid INTEGER PRIMARY KEY{cols})"),
        ),
        (
            format!("{vtab}_docsize"),
            format!("CREATE TABLE '{vtab}_docsize'(docid INTEGER PRIMARY KEY, size BLOB)"),
        ),
        (
            format!("{vtab}_segdir"),
            format!(
                "CREATE TABLE '{vtab}_segdir'(level INTEGER,idx INTEGER,start_block INTEGER,\
leaves_end_block INTEGER,end_block INTEGER,root BLOB,PRIMARY KEY(level, idx))"
            ),
        ),
        (
            format!("{vtab}_segments"),
            format!("CREATE TABLE '{vtab}_segments'(blockid INTEGER PRIMARY KEY, block BLOB)"),
        ),
        (
            format!("{vtab}_stat"),
            format!("CREATE TABLE '{vtab}_stat'(id INTEGER PRIMARY KEY, value BLOB)"),
        ),
    ]
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

/// The VERBATIM half of a DDL record (everything after the fingerprint's NUL),
/// or `None` when the record is malformed or a tombstone. The fingerprint half
/// is never portable — it is re-derived from the live table — so a caller
/// moving a record must take only this and rebuild the rest.
pub(crate) fn ddl_record_verbatim(rec: &[u8]) -> Option<&str> {
    let cut = rec.iter().position(|&b| b == 0)?;
    let text = std::str::from_utf8(&rec[cut + 1..]).ok()?;
    (!text.is_empty() && cut > 0).then_some(text)
}

/// `ALTER TABLE [<schema>.]<old> RENAME TO <new>` → `(schema, old, new)`.
///
/// Only the whole-table rename; `RENAME COLUMN` keeps the table's name and so
/// keeps its record. Word-level rather than a parse, like the other detectors
/// in this file: the statement has already been executed successfully by the
/// time this runs, so the shape is known-good and the only job is to read the
/// two names back out of it.
pub(crate) fn alter_rename_target(sql: &str) -> Option<(Option<String>, String, String)> {
    let mut w = DdlWords::new(crate::sql::strip_leading_trivia(sql));
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    if !eq(&w.word()?.0, "alter") || !eq(&w.word()?.0, "table") {
        return None;
    }
    let mut schema = None;
    let mut old = w.word()?.0;
    if w.peek_dot() {
        w.word()?; // the `.`
        schema = Some(old);
        old = w.word()?.0;
    }
    // `RENAME TO <new>` and nothing else — `RENAME COLUMN` keeps the table's
    // name, so it keeps its record and must not land here.
    if !eq(&w.word()?.0, "rename") || !eq(&w.word()?.0, "to") {
        return None;
    }
    let new = w.word()?.0;
    (!old.is_empty() && !new.is_empty()).then_some((schema, old, new))
}

/// The `sql` text for a table: the caller's own statement when a record for it
/// is present AND still describes this exact shape, else the reconstruction.
pub(super) fn master_sql(t: &mpedb::TableDef, idx: &IndexRecords, rec: Option<&Vec<u8>>) -> String {
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
    /// The `schema.` qualifier as written, unquoted — `None` when the statement
    /// gave none. Which schema an object lives in decides where its verbatim
    /// DDL record is filed: two schemas may hold a table of ONE name (
    /// SQLAlchemy's reflection suite builds `users` in both), and a single
    /// name-keyed record in main meant the second CREATE overwrote the first's
    /// text — so `main.users` reported the ATTACHED table's constraints.
    pub schema: Option<String>,
    /// Byte offset of the name token within the trivia-stripped statement —
    /// where sqlite's stored `sql` text begins.
    pub name_at: usize,
    /// `CREATE INDEX … ON <table>`: the table the index is built over. `None`
    /// for every other kind (and for `DROP INDEX`, which does not name one).
    pub on_table: Option<String>,
    /// `CREATE VIRTUAL TABLE …` (plan §7): the stored `sql` is the WHOLE
    /// statement (sqlite keeps `CREATE VIRTUAL TABLE t USING …` verbatim),
    /// and — for fts4 — five shadow-table records ride along.
    pub virtual_table: bool,
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
    // `CREATE VIRTUAL TABLE`: a real target since plan §7 — the record is
    // the whole statement, and the sqlite_master row is what iterdump's
    // vtab branch replays. `kw` advances onto `table`, the shared tail
    // then reads the name.
    let mut virtual_table = false;
    if create && kw == "virtual" {
        virtual_table = true;
        kw = w.word()?.0.to_ascii_lowercase();
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
    let mut schema = None;
    if w.peek_dot() {
        let _ = w.word(); // the '.' itself
        schema = Some(std::mem::take(&mut name));
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
    Some(DdlTarget { kind, create, name, schema, name_at: at, on_table, virtual_table })
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
