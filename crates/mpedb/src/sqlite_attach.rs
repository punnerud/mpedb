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
    /// WITHOUT ROWID, single integer PK at this declared index.
    WithoutRowidInt(usize),
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

fn any_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        ty: ColumnType::Any,
        nullable: true,
        unique: false,
        indexed: false,
        default: None,
        check: None,
    }
}

fn int_pk_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        ty: ColumnType::Int64,
        nullable: false,
        unique: false,
        indexed: false,
        default: None,
        check: None,
    }
}

fn int_affinity(decl: &str) -> bool {
    decl.to_ascii_uppercase().contains("INT")
}

impl SqliteAttach {
    pub fn open(path: &Path) -> Result<SqliteAttach> {
        let file = fmtx::SqliteFile::open(path).map_err(ferr)?;
        let src_tables = file.tables().map_err(ferr)?;
        let mut tables = Vec::new();
        let mut defs = Vec::new();
        let mut skipped = Vec::new();
        for t in src_tables {
            if defs.len() >= 64 {
                skipped.push((t.name.clone(), "table-id space (64) exhausted".into()));
                continue;
            }
            let (pk, def) = if let Some(ipk) = t.ipk_column {
                let cols = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| if i == ipk { int_pk_col(c) } else { any_col(c) })
                    .collect();
                (
                    PkKind::Ipk(ipk),
                    TableDef { name: t.name.clone(), columns: cols, primary_key: vec![ipk as u16] },
                )
            } else if t.without_rowid {
                let pk_idx = (t.pk_order.len() == 1)
                    .then(|| {
                        t.columns
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(&t.pk_order[0]))
                    })
                    .flatten()
                    .filter(|i| t.decl_types.get(*i).is_some_and(|d| int_affinity(d)));
                let Some(i) = pk_idx else {
                    skipped.push((
                        t.name.clone(),
                        "WITHOUT ROWID with a non-single-integer PK (v1 shape rule)".into(),
                    ));
                    continue;
                };
                let cols = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(j, c)| if j == i { int_pk_col(c) } else { any_col(c) })
                    .collect();
                (
                    PkKind::WithoutRowidInt(i),
                    TableDef { name: t.name.clone(), columns: cols, primary_key: vec![i as u16] },
                )
            } else {
                if t.columns.iter().any(|c| c.eq_ignore_ascii_case("rowid")) {
                    skipped.push((
                        t.name.clone(),
                        "no INTEGER PRIMARY KEY and a column already named `rowid`".into(),
                    ));
                    continue;
                }
                let mut cols: Vec<ColumnDef> = t.columns.iter().map(|c| any_col(c)).collect();
                cols.push(int_pk_col("rowid"));
                let pk = cols.len() - 1;
                (
                    PkKind::SyntheticRowid,
                    TableDef {
                        name: t.name.clone(),
                        columns: cols,
                        primary_key: vec![pk as u16],
                    },
                )
            };
            defs.push(def);
            tables.push(Attached { src: t, pk });
        }
        // `Schema::table_id` binary-searches by name: the tables MUST be
        // name-sorted, and the attach list must mirror the same order so a
        // plan's table id indexes both consistently.
        let mut both: Vec<(TableDef, Attached)> = defs.into_iter().zip(tables).collect();
        both.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (defs, tables): (Vec<_>, Vec<_>) = both.into_iter().unzip();
        Ok(SqliteAttach { file, tables, schema: Schema { tables: defs }, skipped })
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

    /// Decode a raw keycode bound (single-column int64 PK) back to a rowid.
    fn bound_to_int(b: Option<(&[u8], bool)>) -> Result<Option<(i64, bool)>> {
        match b {
            None => Ok(None),
            Some((raw, incl)) => {
                // Range bounds arrive with prefix semantics (a 0xFF
                // continuation ceiling past the encoded value); decode the
                // first int64 and let the inclusivity flag carry the rest.
                let vals = keycode::decode_key(raw, &[ColumnType::Int64])
                    .or_else(|_| {
                        keycode::decode_key(
                            raw.get(..raw.len().saturating_sub(1)).unwrap_or(raw),
                            &[ColumnType::Int64],
                        )
                    })
                    .map_err(|_| Error::Internal("undecodable PK bound".into()))?;
                match vals.first() {
                    Some(Value::Int(i)) => Ok(Some((*i, incl))),
                    _ => Err(Error::Internal("non-int PK bound".into())),
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
        let lo = Self::bound_to_int(lo)?;
        let hi = Self::bound_to_int(hi)?;
        let ti = id as usize;
        let mut out = Vec::new();
        let in_lo = |k: i64| lo.is_none_or(|(v, incl)| if incl { k >= v } else { k > v });
        let in_hi = |k: i64| hi.is_none_or(|(v, incl)| if incl { k <= v } else { k < v });
        let key_col = match a.pk {
            PkKind::Ipk(i) | PkKind::WithoutRowidInt(i) => Some(i),
            PkKind::SyntheticRowid => None,
        };
        self.at
            .file
            .scan_table(&a.src, &mut |rowid, vals| {
                let k = match key_col {
                    None => rowid,
                    Some(i) => match vals.get(i) {
                        Some(fmtx::Value::Int(x)) => *x,
                        _ => {
                            return Err(fmtx::Error::Corrupt(
                                "integer PK column holds a non-integer".into(),
                            ))
                        }
                    },
                };
                // Scans run in key order for every attached shape, so the
                // upper bound could early-terminate; sqlitefmt's walker has
                // no stop signal yet — correctness first, the valve later.
                if in_lo(k) && in_hi(k) {
                    out.push(self.at.shape_row(ti, rowid, vals));
                }
                Ok(())
            })
            .map_err(ferr)?;
        Ok(out)
    }
}

impl TxnCtx for SqliteCtx<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        let a = self.table(table)?;
        let [Value::Int(k)] = pk else {
            return Ok(None); // NULL or non-int probe: no row can match
        };
        let ti = table as usize;
        match a.pk {
            PkKind::Ipk(_) | PkKind::SyntheticRowid => {
                match self.at.file.seek_rowid(&a.src, *k).map_err(ferr)? {
                    None => Ok(None),
                    Some(vals) => Ok(Some(self.at.shape_row(ti, *k, vals))),
                }
            }
            PkKind::WithoutRowidInt(i) => {
                // v1: linear probe in key order (honest O(n); the index-tree
                // descent for WITHOUT ROWID rides v2's reader work).
                let mut found = None;
                self.at
                    .file
                    .scan_table(&a.src, &mut |_r, vals| {
                        if found.is_none()
                            && matches!(vals.get(i), Some(fmtx::Value::Int(x)) if x == k)
                        {
                            found = Some(vals);
                        }
                        Ok(())
                    })
                    .map_err(ferr)?;
                Ok(found.map(|vals| self.at.shape_row(ti, *k, vals)))
            }
        }
    }

    fn get_by_index(&mut self, _t: u32, _no: u32, _v: &Value) -> Result<Option<Vec<Value>>> {
        Err(Error::Internal("index probe on a sqlite attach (schema has none)".into()))
    }
    fn scan_by_index(&mut self, _t: u32, _no: u32, _v: &Value) -> Result<Vec<Vec<Value>>> {
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
