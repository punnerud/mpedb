//! Read-only SQL over a sqlite `.db` with ZERO import — DESIGN-SQLITE-BACKED
//! v1's query-attach. The native reader (`mpedb-sqlitefmt`) is the row
//! source, an mpedb [`Schema`] is derived from it, and the EXISTING
//! planner/executor do all the SQL work: [`SqliteAttach::query`] compiles
//! with an empty policy catalog and executes against a [`TxnCtx`] whose
//! scans and PK probes walk sqlite b-trees.
//!
//! Honest v1 edges, each named at attach or query time:
//! - Read-only: INSERT/UPDATE/DELETE are refused (writes belong to the
//!   sidecar flow / the v2 overlay).
//! - Quiescence is the CALLER's problem in v1: no locks are taken here (the
//!   lock modes are v2); run it against a database nothing is writing.
//! - Every non-PK column types as `any` (sqlite affinity, decided per
//!   value); the PK is `int64` — a rowid table's `INTEGER PRIMARY KEY`, a
//!   synthetic trailing `rowid` column when the table has none, or a
//!   WITHOUT ROWID table's single integer PK. Tables that fit none of those
//!   shapes are skipped, listed in [`SqliteAttach::skipped`].

use std::path::Path;

use mpedb_sqlitefmt as fmtx;
use mpedb_types::{keycode, ColumnDef, ColumnType, Error, Result, Schema, TableDef, Value};

use crate::exec::{exec_stmt, TxnCtx};
use crate::ExecResult;

enum PkKind {
    /// `INTEGER PRIMARY KEY` — the rowid alias at this column index.
    Ipk(usize),
    /// No integer PK: a synthetic trailing `rowid` column carries it.
    SyntheticRowid,
    /// WITHOUT ROWID, single INTEGER- or TEXT-affinity PK at this declared
    /// index. Storage order is the PK's b-tree order — int order for
    /// integers, BINARY (memcmp) for text — which matches mpedb's keycode
    /// order for the same types, so merges and range scans line up.
    WithoutRowidKey(usize, ColumnType),
}

struct Attached {
    src: fmtx::Table,
    pk: PkKind,
}

pub struct SqliteAttach {
    file: fmtx::SqliteFile,
    tables: Vec<Attached>,
    schema: Schema,
    /// (table, reason) for every table the v1 shape rules could not attach.
    skipped: Vec<(String, String)>,
}

fn ferr(e: fmtx::Error) -> Error {
    match e {
        fmtx::Error::Io(e) => Error::Io(e),
        fmtx::Error::Corrupt(m) => Error::Corrupt(format!("sqlite: {m}")),
        fmtx::Error::Unsupported(m) => Error::Unsupported(format!("sqlite: {m}")),
    }
}

fn val(v: fmtx::Value) -> Value {
    match v {
        fmtx::Value::Null => Value::Null,
        fmtx::Value::Int(i) => Value::Int(i),
        fmtx::Value::Float(f) => Value::Float(f),
        fmtx::Value::Text(t) => Value::Text(t),
        fmtx::Value::Blob(b) => Value::Blob(b),
    }
}

/// A non-PK base column: per-value storage (`Any`) carrying the base's DECLARED
/// AFFINITY.
///
/// The storage type stays `Any` on purpose and is not a shortcut. A sqlite file
/// is not rigid — an `int` column may genuinely hold the text `'abc'`, because
/// sqlite stores whatever survives its affinity conversion — so declaring the
/// overlay column `Int64` would make mpedb refuse to READ rows sqlite happily
/// holds. What sqlite does guarantee is the CONVERSION applied on the way in,
/// and that is the affinity: an `int`/`decimal(10,2)`/`datetime` column turns
/// `'1.50'` into the real `1.5`, and a column with no declared type keeps it as
/// text. Dropping the affinity is what made the overlay answer `'1.50'`/text
/// where sqlite answers `1.5`/real — a wrong answer, and the reason this is no
/// longer a blanket `Any` (DESIGN-SQLITE-BACKED §"Overlay schema" [R#17]).
fn any_col(name: &str, decl: &str, not_null: bool, default: Option<Value>) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        ty: ColumnType::Any,
        nullable: !not_null,
        unique: false,
        indexed: false,
        default: default.map(mpedb_types::DefaultExpr::Const),
        check: None,
        collation: mpedb_types::Collation::Binary,
        affinity: mpedb_types::Affinity::declared(decl),
    }
}

/// A base column's `DEFAULT` text → the mpedb constant it stores, or `Err` when
/// mpedb cannot represent it.
///
/// Only LITERALS are representable: sqlite evaluates `CURRENT_TIMESTAMP` and a
/// parenthesized expression at insert time, and mpedb has no machinery to do
/// that for a base's dialect. Guessing is the wrong answer this whole change
/// exists to stop, and silently dropping it is worse still — a column whose
/// default vanished stores NULL where sqlite stores a value — so an
/// unrepresentable default takes the table out of the attach BY NAME.
///
/// The literal takes the column's store-time affinity, exactly as a value
/// written by an INSERT would: sqlite stores `DEFAULT '1.50'` on a NUMERIC
/// column as the real 1.5.
fn default_const(text: &str, affinity: mpedb_types::Affinity) -> std::result::Result<Value, ()> {
    let t = text.trim();
    let v = if t.eq_ignore_ascii_case("NULL") {
        Value::Null
    } else if t.eq_ignore_ascii_case("TRUE") {
        Value::Int(1)
    } else if t.eq_ignore_ascii_case("FALSE") {
        Value::Int(0)
    } else if let Some(rest) = t.strip_prefix('\'') {
        // A string literal; sqlite doubles an embedded quote.
        let body = rest.strip_suffix('\'').ok_or(())?;
        if body.contains('\'') && !body.contains("''") {
            return Err(());
        }
        Value::Text(body.replace("''", "'"))
    } else if let Ok(i) = t.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = t.parse::<f64>() {
        Value::Float(f)
    } else {
        // `CURRENT_TIMESTAMP`, `(expr)`, `x'…'`, anything else.
        return Err(());
    };
    Ok(mpedb_types::store_into(ColumnType::Any, affinity, v))
}

fn pk_col(name: &str, ty: ColumnType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        ty,
        nullable: false,
        unique: false,
        indexed: false,
        default: None,
        check: None,
        collation: mpedb_types::Collation::Binary,
        // A rigid column enforces its type; `validate` pins the affinity.
        affinity: mpedb_types::Affinity::implied_by(ty),
    }
}

fn int_pk_col(name: &str) -> ColumnDef {
    pk_col(name, ColumnType::Int64)
}

/// sqlite's affinity classification, restricted to the two PK shapes the
/// attach serves: INTEGER affinity (rule 1: contains "INT") and TEXT
/// affinity (rule 2: contains "CHAR", "CLOB", or "TEXT"). Everything else
/// (REAL/NUMERIC/BLOB PKs) stays a named skip.
fn pk_affinity(decl: &str) -> Option<ColumnType> {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        Some(ColumnType::Int64)
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Some(ColumnType::Text)
    } else {
        None
    }
}

impl SqliteAttach {
    pub fn open(path: &Path) -> Result<SqliteAttach> {
        let file = fmtx::SqliteFile::open(path).map_err(ferr)?;
        let src_tables = file.tables().map_err(ferr)?;
        let mut tables = Vec::new();
        let mut defs = Vec::new();
        let mut skipped = Vec::new();
        for t in src_tables {
            // `_mpedb_`-prefixed tables are OURS (the overlay's checkpoint
            // marker, mirror's tracking tables) — internal like `sqlite_`,
            // never user-visible, and silently so (not `skipped`).
            if t.name.starts_with("_mpedb_") {
                continue;
            }
            if defs.len() >= 64 {
                skipped.push((t.name.clone(), "table-id space (64) exhausted".into()));
                continue;
            }
            // Constraints mpedb cannot carry are a NAMED SKIP, never a silent
            // drop. A dropped CHECK let an invalid row into the overlay that
            // then failed the base's own constraint at checkpoint time — the
            // error surfacing on a later, unrelated statement — and a dropped
            // NOT NULL/DEFAULT changed what a row STORES. A table nobody can
            // see is a clean refusal; a table that answers differently is not.
            if t.has_check {
                skipped.push((
                    t.name.clone(),
                    "CHECK constraint (mpedb cannot compile a base's CHECK, and not                      enforcing it would let in a row the base itself rejects)"
                        .into(),
                ));
                continue;
            }
            if t.has_generated {
                skipped.push((
                    t.name.clone(),
                    "GENERATED column (its value is computed by the base, not stored)".into(),
                ));
                continue;
            }
            // Resolve every DEFAULT up front: one unrepresentable default takes
            // the whole table, because a column whose default vanished stores
            // NULL where sqlite stores a value.
            let mut col_defaults: Vec<Option<Value>> = Vec::with_capacity(t.columns.len());
            let mut bad_default = None;
            for (i, d) in t.defaults.iter().enumerate() {
                let aff = mpedb_types::Affinity::declared(
                    t.decl_types.get(i).map_or("", String::as_str),
                );
                match d {
                    None => col_defaults.push(None),
                    Some(text) => match default_const(text, aff) {
                        Ok(Value::Null) => col_defaults.push(None), // == no default
                        Ok(v) => col_defaults.push(Some(v)),
                        Err(()) => {
                            bad_default = Some((t.columns[i].clone(), text.clone()));
                            break;
                        }
                    },
                }
            }
            if let Some((col, text)) = bad_default {
                skipped.push((
                    t.name.clone(),
                    format!("DEFAULT on `{col}` is not a literal mpedb can store (`{text}`)"),
                ));
                continue;
            }
            let mkcol = |i: usize| {
                any_col(
                    &t.columns[i],
                    t.decl_types.get(i).map_or("", String::as_str),
                    t.not_null.get(i).copied().unwrap_or(false),
                    col_defaults.get(i).cloned().flatten(),
                )
            };
            let (pk, def) = if let Some(ipk) = t.ipk_column {
                let cols = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| if i == ipk { int_pk_col(c) } else { mkcol(i) })
                    .collect();
                (
                    PkKind::Ipk(ipk),
                    TableDef {
                        id: 0,
                        name: t.name.clone(),
                        columns: cols,
                        primary_key: vec![ipk as u16],
                        indexes: vec![],
                        dead: false,
                        implicit_rowid: false,
                        kind: mpedb_types::TableKind::Standard,
                    },
                )
            } else if t.without_rowid {
                let found = (t.pk_order.len() == 1)
                    .then(|| {
                        t.columns
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(&t.pk_order[0]))
                    })
                    .flatten()
                    .and_then(|i| {
                        t.decl_types
                            .get(i)
                            .and_then(|d| pk_affinity(d))
                            .map(|ty| (i, ty))
                    });
                let Some((i, ty)) = found else {
                    skipped.push((
                        t.name.clone(),
                        "WITHOUT ROWID with a PK that is not one INTEGER- or TEXT-affinity \
                         column (shape rule)"
                            .into(),
                    ));
                    continue;
                };
                let cols = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(j, c)| if j == i { pk_col(c, ty) } else { mkcol(j) })
                    .collect();
                (
                    PkKind::WithoutRowidKey(i, ty),
                    TableDef {
                        id: 0,
                        name: t.name.clone(),
                        columns: cols,
                        primary_key: vec![i as u16],
                        indexes: vec![],
                        dead: false,
                        implicit_rowid: false,
                        kind: mpedb_types::TableKind::Standard,
                    },
                )
            } else {
                if t.columns.iter().any(|c| c.eq_ignore_ascii_case("rowid")) {
                    skipped.push((
                        t.name.clone(),
                        "no INTEGER PRIMARY KEY and a column already named `rowid`".into(),
                    ));
                    continue;
                }
                let mut cols: Vec<ColumnDef> = (0..t.columns.len()).map(mkcol).collect();
                cols.push(int_pk_col("rowid"));
                let pk = cols.len() - 1;
                (
                    PkKind::SyntheticRowid,
                    TableDef {
                        id: 0,
                        name: t.name.clone(),
                        columns: cols,
                        primary_key: vec![pk as u16],
                        indexes: vec![],
                        dead: false,
                        // HIDDEN, exactly as #94 made it on the native path.
                        // The base table has no INTEGER PRIMARY KEY, so this
                        // `rowid` is mpedb's synthesis of sqlite's implicit one
                        // — and sqlite does not return it from `SELECT *`.
                        // Leaving it visible made `SELECT *` yield one column
                        // MORE than sqlite does: wrong result arity, which is a
                        // wrong answer and not a cosmetic difference.
                        implicit_rowid: true,
                        kind: mpedb_types::TableKind::Standard,
                    },
                )
            };
            defs.push(def);
            tables.push(Attached { src: t, pk });
        }
        // When EVERY table was skipped, `Schema::new` would report the generic
        // "schema defines no live tables" — true, but it hides the reasons the
        // caller actually needs (measured: a one-table base with a CHECK
        // constraint said "no live tables" instead of naming the CHECK). The
        // skip list IS the explanation, so lead with it.
        if defs.is_empty() && !skipped.is_empty() {
            return Err(Error::Unsupported(format!(
                "no table in {} is attachable under the v2 shape rules: {:?}",
                path.display(),
                skipped
            )));
        }
        // The attach list must mirror `Schema`'s table order so a plan's
        // table id indexes both consistently. `Schema::new` sorts by name
        // and assigns dense ids in that order (never a struct literal here —
        // a literal would leave every id 0, and id-based lookups would then
        // answer every query from table 0).
        let mut both: Vec<(TableDef, Attached)> = defs.into_iter().zip(tables).collect();
        both.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (defs, tables): (Vec<_>, Vec<_>) = both.into_iter().unzip();
        Ok(SqliteAttach { file, tables, schema: Schema::new(defs)?, skipped })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    /// Compile + run one statement against the attached file. Read-only:
    /// write statements fail at execution with a named error.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<ExecResult> {
        let (plan, is_explain) = mpedb_sql::prepare_maybe_explain(sql, &self.schema)?;
        if is_explain {
            return Ok(ExecResult::Rows {
                columns: vec!["plan".into()],
                rows: plan
                    .explain(&self.schema)
                    .lines()
                    .map(|l| vec![Value::Text(l.to_string())])
                    .collect(),
            });
        }
        let mut ctx = SqliteCtx { at: self };
        let mut partial = false;
        exec_stmt(&mut ctx, &self.schema, &plan, params, &mut partial)
    }

    /// Full row in the DERIVED column layout (synthetic rowid appended).
    fn shape_row(&self, ti: usize, rowid: i64, vals: Vec<fmtx::Value>) -> Vec<Value> {
        let mut out: Vec<Value> = vals.into_iter().map(val).collect();
        if matches!(self.tables[ti].pk, PkKind::SyntheticRowid) {
            out.push(Value::Int(rowid));
        }
        out
    }
}

struct SqliteCtx<'a> {
    at: &'a SqliteAttach,
}

impl SqliteCtx<'_> {
    fn table(&self, id: u32) -> Result<&Attached> {
        self.at
            .tables
            .get(id as usize)
            .ok_or_else(|| Error::Internal("table id out of range".into()))
    }

    /// Decode a raw keycode bound (single-column PK of `ty`) back to a key
    /// plus its EFFECTIVE inclusivity. Bounds arrive normalized with prefix
    /// semantics (`range_bounds`): a clean decode means the bound sits
    /// exactly at `enc(v)` — the flag carries; a 0xFF-suffixed raw sits just
    /// ABOVE every key equal to `v`, so the effective inclusivity FLIPS
    /// (lo-exclusive and hi-inclusive are the suffixed forms).
    fn bound_to_key(b: Option<(&[u8], bool)>, ty: ColumnType) -> Result<Option<(Key, bool)>> {
        match b {
            None => Ok(None),
            Some((raw, incl)) => {
                let (vals, flipped) = match keycode::decode_key(raw, &[ty]) {
                    Ok(v) => (v, false),
                    Err(_) => (
                        keycode::decode_key(
                            raw.get(..raw.len().saturating_sub(1)).unwrap_or(raw),
                            &[ty],
                        )
                        .map_err(|_| Error::Internal("undecodable PK bound".into()))?,
                        true,
                    ),
                };
                match vals.into_iter().next() {
                    Some(Value::Int(i)) => Ok(Some((Key::Int(i), incl != flipped))),
                    Some(Value::Text(s)) => Ok(Some((Key::Text(s), incl != flipped))),
                    _ => Err(Error::Internal("PK bound of an unserved type".into())),
                }
            }
        }
    }

    fn scan_bounded(
        &self,
        id: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let a = self.table(id)?;
        let (key_col, key_ty) = match a.pk {
            PkKind::Ipk(i) => (Some(i), ColumnType::Int64),
            PkKind::WithoutRowidKey(i, ty) => (Some(i), ty),
            PkKind::SyntheticRowid => (None, ColumnType::Int64),
        };
        let lo = Self::bound_to_key(lo, key_ty)?;
        let hi = Self::bound_to_key(hi, key_ty)?;
        let ti = id as usize;
        let mut out = Vec::new();
        let in_lo = |k: &Key| {
            lo.as_ref()
                .is_none_or(|(v, incl)| if *incl { k >= v } else { k > v })
        };
        let in_hi = |k: &Key| {
            hi.as_ref()
                .is_none_or(|(v, incl)| if *incl { k <= v } else { k < v })
        };
        self.at
            .file
            .scan_table(&a.src, &mut |rowid, vals| {
                let k = match key_col {
                    None => Key::Int(rowid),
                    Some(i) => match vals.get(i) {
                        Some(fmtx::Value::Int(x)) => Key::Int(*x),
                        Some(fmtx::Value::Text(s)) => Key::Text(s.clone()),
                        _ => {
                            return Err(fmtx::Error::Corrupt(
                                "PK column holds a value outside its declared affinity".into(),
                            ))
                        }
                    },
                };
                // Scans run in key order for every attached shape (int
                // order / BINARY for text), so the upper bound could
                // early-terminate; sqlitefmt's walker has no stop signal
                // yet — correctness first, the valve later.
                if in_lo(&k) && in_hi(&k) {
                    out.push(self.at.shape_row(ti, rowid, vals));
                }
                Ok(())
            })
            .map_err(ferr)?;
        Ok(out)
    }
}

/// A decoded PK for the served shapes. One table only ever produces one
/// variant; ordering mirrors the storage order (int order / BINARY memcmp),
/// which is also mpedb's keycode order for the same types.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Int(i64),
    Text(String),
}

impl TxnCtx for SqliteCtx<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        let a = self.table(table)?;
        let ti = table as usize;
        match a.pk {
            PkKind::Ipk(_) | PkKind::SyntheticRowid => {
                let [Value::Int(k)] = pk else {
                    return Ok(None); // NULL or non-int probe: no row can match
                };
                match self.at.file.seek_rowid(&a.src, *k).map_err(ferr)? {
                    None => Ok(None),
                    Some(vals) => Ok(Some(self.at.shape_row(ti, *k, vals))),
                }
            }
            PkKind::WithoutRowidKey(i, _) => {
                // A probe of the wrong shape (NULL, or a type the PK cannot
                // hold) matches nothing.
                let want = match pk {
                    [Value::Int(k)] => fmtx::Value::Int(*k),
                    [Value::Text(s)] => fmtx::Value::Text(s.clone()),
                    _ => return Ok(None),
                };
                // Honest O(n): linear probe in key order (the index-tree
                // descent for WITHOUT ROWID is v3 reader work).
                let mut found = None;
                self.at
                    .file
                    .scan_table(&a.src, &mut |_r, vals| {
                        if found.is_none() && vals.get(i) == Some(&want) {
                            found = Some(vals);
                        }
                        Ok(())
                    })
                    .map_err(ferr)?;
                Ok(found.map(|vals| self.at.shape_row(ti, 0, vals)))
            }
        }
    }

    fn get_by_index(&mut self, _t: u32, _no: u32, _v: &[Value]) -> Result<Option<Vec<Value>>> {
        Err(Error::Internal("index probe on a sqlite attach (schema has none)".into()))
    }
    fn scan_by_index(&mut self, _t: u32, _no: u32, _v: &[Value]) -> Result<Vec<Vec<Value>>> {
        Err(Error::Internal("index scan on a sqlite attach (schema has none)".into()))
    }
    fn scan_by_index_range(
        &mut self,
        _t: u32,
        _no: u32,
        _lo: Option<(&[u8], bool)>,
        _hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        Err(Error::Internal("index range on a sqlite attach (schema has none)".into()))
    }

    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        self.scan_bounded(table, lo, hi)
    }

    fn insert_row(&mut self, _t: u32, _v: &[Value]) -> Result<()> {
        Err(Error::Unsupported(
            "a sqlite attach is read-only — open the sidecar flow (`mpedb file.db`) to write"
                .into(),
        ))
    }
    fn update_by_pk(&mut self, _t: u32, _v: &[Value]) -> Result<bool> {
        Err(Error::Unsupported(
            "a sqlite attach is read-only — open the sidecar flow (`mpedb file.db`) to write"
                .into(),
        ))
    }
    fn delete_by_pk(&mut self, _t: u32, _pk: &[Value]) -> Result<bool> {
        Err(Error::Unsupported(
            "a sqlite attach is read-only — open the sidecar flow (`mpedb file.db`) to write"
                .into(),
        ))
    }
}

// ---- base access for the overlay (#69 v2) --------------------------------

impl SqliteAttach {
    /// Point lookup straight against the BASE — the overlay's fall-through.
    pub(crate) fn base_get_by_pk(&self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        SqliteCtx { at: self }.get_by_pk(table, pk)
    }

    /// PK-ordered base scan with keycode bounds — the overlay merge's right-
    /// hand stream.
    pub(crate) fn base_scan(
        &self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        SqliteCtx { at: self }.scan_bounded(table, lo, hi)
    }
}
