//! rRETL stage 4 (design/DESIGN-RRETL.md §13): table-SET maps — a source
//! table set mirrored into a differently-shaped target set, with edits on
//! EITHER side flowing to the other through registered lens pairs. Getting
//! data in and out of an EXTERNAL system (delta-since-timestamp, dump+diff)
//! is the caller's logic, applied to the target set with plain SQL; this
//! module owns the transformation between the sets, in both directions, and
//! the loop safety of repeating it.
//!
//! The insight that collapses the design (§13.1): unlike `rretl apply`, a
//! map does not destroy its source — both sides exist. So a residual pair
//! never stores a residual: at sync time `rex(x_current)` IS the residual,
//! computed live from the source side, and the B→A direction is putback
//! with a live residual, PutRes-gated per row.
//!
//! Loop safety (§13.2): `rretl_map_state` records BOTH sides' canonical
//! hashes after every successful push. A change that arrived BY sync leaves
//! both recorded hashes current, so the next sync sees a clean row and does
//! nothing — no epochs, no origin tags, no echo. Both sides moved since the
//! last sync = a CONFLICT, and the sync aborts whole (one transaction; the
//! set moves together or not at all).

use mpedb_types::{ColumnType, Error, Result, Value};

use crate::lens::LensClass;
use crate::rretl::{
    chunk_rows, next_run_id, now_micros, rows_of, shape_gate, spec_col, CanonChain, LineageRow,
};
use crate::WriteSession;

/// Sys-keyspace namespace for map records: `rrmap/<name>` → one version
/// byte + the mapping TOML verbatim. Bounded like lens records (#124 is
/// about unbounded logs); text-as-format behind a version-byte dispatch is
/// the same eternity stance as the stage-3 envelope.
pub const NS_MAP: &str = "rrmap";
const MAP_RECORD_V1: u8 = 1;

pub const T_MAP_STATE: &str = "rretl_map_state";
const MAP_STATE_SHAPE: [&str; 7] =
    ["map", "tbl", "pk_enc", "k", "a_hash", "b_hash", "ts_micros"];

// ------------------------------------------------------------------- spec

#[derive(Debug, Clone)]
pub struct MapColumn {
    pub source: String,
    pub target: String,
    /// A registered lens pair name; `None` = identity copy.
    pub pair: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MapTable {
    pub source: String,
    pub target: String,
    /// Target key column name; defaults to the source identity's name
    /// (`id` when the source identity is the hidden rowid).
    pub target_key: Option<String>,
    pub columns: Vec<MapColumn>,
}

#[derive(Debug, Clone)]
pub struct MapSpec {
    pub name: String,
    pub tables: Vec<MapTable>,
}

/// Identifier check for every NAME the spec contributes to formatted SQL.
/// The spec is user input; a name that fails this never reaches a query
/// string.
fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn toml_str(v: &toml::Value, what: &str) -> Result<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Unsupported(format!("map spec: {what} must be a string")))
}

impl MapSpec {
    /// Parse and VALIDATE a mapping document. Every refusal is named; a
    /// spec that parses is safe to interpolate into SQL (`ident_ok` gates
    /// every identifier).
    pub fn from_toml_str(text: &str) -> Result<MapSpec> {
        let doc: toml::Value = text
            .parse()
            .map_err(|e| Error::Unsupported(format!("map spec parse error: {e}")))?;
        let map = doc
            .get("map")
            .ok_or_else(|| Error::Unsupported("map spec: missing [map] section".into()))?;
        let name = toml_str(
            map.get("name")
                .ok_or_else(|| Error::Unsupported("map spec: missing map.name".into()))?,
            "map.name",
        )?;
        let raw_tables = map
            .get("table")
            .and_then(|t| t.as_array())
            .ok_or_else(|| Error::Unsupported("map spec: at least one [[map.table]]".into()))?;
        let mut tables = Vec::with_capacity(raw_tables.len());
        for t in raw_tables {
            let source = toml_str(
                t.get("source").ok_or_else(|| {
                    Error::Unsupported("map spec: table without `source`".into())
                })?,
                "table.source",
            )?;
            let target = toml_str(
                t.get("target").ok_or_else(|| {
                    Error::Unsupported("map spec: table without `target`".into())
                })?,
                "table.target",
            )?;
            let target_key = t
                .get("target_key")
                .map(|v| toml_str(v, "table.target_key"))
                .transpose()?;
            let raw_cols = t
                .get("column")
                .and_then(|c| c.as_array())
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "map spec: table `{source}` needs at least one [[map.table.column]]"
                    ))
                })?;
            let mut columns = Vec::with_capacity(raw_cols.len());
            for c in raw_cols {
                columns.push(MapColumn {
                    source: toml_str(
                        c.get("source").ok_or_else(|| {
                            Error::Unsupported("map spec: column without `source`".into())
                        })?,
                        "column.source",
                    )?,
                    target: toml_str(
                        c.get("target").ok_or_else(|| {
                            Error::Unsupported("map spec: column without `target`".into())
                        })?,
                        "column.target",
                    )?,
                    pair: c.get("pair").map(|v| toml_str(v, "column.pair")).transpose()?,
                });
            }
            tables.push(MapTable { source, target, target_key, columns });
        }
        let spec = MapSpec { name, tables };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<()> {
        if !ident_ok(&self.name) {
            return Err(Error::Unsupported(format!(
                "map spec: `{}` is not a legal map name",
                self.name
            )));
        }
        let mut seen_dst = std::collections::HashSet::new();
        for t in &self.tables {
            for (n, what) in [(&t.source, "source table"), (&t.target, "target table")] {
                if !ident_ok(n) {
                    return Err(Error::Unsupported(format!(
                        "map spec: `{n}` is not a legal {what} name"
                    )));
                }
                if crate::rretl::rretl_bookkeeping_names().contains(&n.as_str()) {
                    return Err(Error::Unsupported(format!(
                        "map spec: `{n}` is rRETL bookkeeping; mapping it is refused"
                    )));
                }
            }
            if t.source == t.target {
                return Err(Error::Unsupported(format!(
                    "map spec: `{}` maps onto itself — source and target must differ",
                    t.source
                )));
            }
            if !seen_dst.insert(t.target.clone()) {
                return Err(Error::Unsupported(format!(
                    "map spec: target `{}` appears twice",
                    t.target
                )));
            }
            if let Some(k) = &t.target_key {
                if !ident_ok(k) {
                    return Err(Error::Unsupported(format!(
                        "map spec: `{k}` is not a legal target_key name"
                    )));
                }
            }
            let mut src_cols = std::collections::HashSet::new();
            let mut dst_cols = std::collections::HashSet::new();
            for c in &t.columns {
                for (n, what) in [(&c.source, "source column"), (&c.target, "target column")] {
                    if !ident_ok(n) {
                        return Err(Error::Unsupported(format!(
                            "map spec: `{n}` is not a legal {what} name"
                        )));
                    }
                }
                // A source column mapped TWICE would make the B→A direction
                // two competing claims about one value; a target column
                // written twice is a plain collision.
                if !src_cols.insert(c.source.clone()) {
                    return Err(Error::Unsupported(format!(
                        "map spec: source column `{}` of `{}` is mapped twice — the \
                         reverse direction would have two claims about one value",
                        c.source, t.source
                    )));
                }
                if !dst_cols.insert(c.target.clone()) {
                    return Err(Error::Unsupported(format!(
                        "map spec: target column `{}` of `{}` is written twice",
                        c.target, t.target
                    )));
                }
            }
        }
        Ok(())
    }
}

// --------------------------------------------------------------- resolved

struct ResolvedCol {
    src: String,
    dst: String,
    lens: Option<crate::lens::RretlLens>,
}

struct ResolvedTable {
    src: String,
    dst: String,
    /// Source row identity: a declared single-column PK, or `rowid`.
    src_key: String,
    src_key_ty: ColumnType,
    /// Whether the identity is the hidden rowid — creation on the target
    /// side is refused then (the source assigns rowids, nothing else can).
    rowid_identity: bool,
    dst_key: String,
    /// Target table missing entirely: auto-create at sync time.
    create_dst: bool,
    cols: Vec<ResolvedCol>,
}

/// What one sync did.
#[derive(Debug, Default)]
pub struct MapSyncReport {
    pub run_id: i64,
    pub a_to_b: u64,
    pub b_to_a: u64,
    pub created_b: u64,
    pub created_a: u64,
    pub deleted_a: u64,
    pub deleted_b: u64,
    pub unchanged: u64,
}

impl MapSyncReport {
    /// Total rows any direction moved (creations/deletions included).
    pub fn changed_total(&self) -> u64 {
        self.a_to_b
            + self.b_to_a
            + self.created_b
            + self.created_a
            + self.deleted_a
            + self.deleted_b
    }
    fn note(&self) -> String {
        format!(
            "a→b {}, b→a {}, +b {}, +a {}, -a {}, -b {}, clean {}",
            self.a_to_b,
            self.b_to_a,
            self.created_b,
            self.created_a,
            self.deleted_a,
            self.deleted_b,
            self.unchanged
        )
    }
}

impl crate::Database {
    /// Store (or replace) a mapping. The spec is validated NOW — sources
    /// exist with a usable row identity, every named pair loads and is
    /// healthy — so `map sync` never discovers a broken definition mid-run.
    pub fn rretl_map_define(&self, toml_text: &str) -> Result<()> {
        let spec = MapSpec::from_toml_str(toml_text)?;
        self.resolve_map(&spec)?;
        let key = crate::sys_record_subkey(NS_MAP, spec.name.as_bytes())?;
        let mut record = vec![MAP_RECORD_V1];
        record.extend_from_slice(toml_text.as_bytes());
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&key, &record)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(())
    }

    /// The stored mapping TOML, verbatim.
    pub fn rretl_map_show(&self, name: &str) -> Result<String> {
        let key = crate::sys_record_subkey(NS_MAP, name.as_bytes())?;
        let r = self.engine.begin_read()?;
        let rec = r.sys_get(&key)?;
        r.finish()?;
        let rec = rec.ok_or_else(|| Error::Unsupported(format!("no map named `{name}`")))?;
        match rec.first() {
            Some(&MAP_RECORD_V1) => String::from_utf8(rec[1..].to_vec())
                .map_err(|_| Error::Corrupt(format!("map `{name}`: record is not UTF-8"))),
            Some(v) => Err(Error::Unsupported(format!(
                "map `{name}` uses record version {v}; this build reads version {MAP_RECORD_V1}"
            ))),
            None => Err(Error::Corrupt(format!("map `{name}`: empty record"))),
        }
    }

    /// Every stored map name.
    pub fn rretl_maps(&self) -> Result<Vec<String>> {
        // Namespace framing is `ns ‖ NUL ‖ key` (sys_record_subkey); the
        // range [ns‖0, ns‖1) is exactly this namespace.
        let mut lo = NS_MAP.as_bytes().to_vec();
        lo.push(0);
        let mut hi = NS_MAP.as_bytes().to_vec();
        hi.push(1);
        let r = self.engine.begin_read()?;
        let keys = r.sys_scan_range_keys(&lo, &hi)?;
        r.finish()?;
        Ok(keys
            .into_iter()
            .filter_map(|k| String::from_utf8(k[lo.len()..].to_vec()).ok())
            .collect())
    }

    /// Drop a mapping. Its state rows stay (they are history a re-defined
    /// map with the same name can pick up); `true` when it existed.
    pub fn rretl_map_drop(&self, name: &str) -> Result<bool> {
        let key = crate::sys_record_subkey(NS_MAP, name.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let existed = match w.sys_get(&key) {
            Ok(v) => v.is_some(),
            Err(e) => {
                w.abort();
                return Err(e);
            }
        };
        let res = (|| {
            w.sys_delete(&key)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(existed)
    }

    fn load_map(&self, name: &str) -> Result<MapSpec> {
        let text = self.rretl_map_show(name)?;
        let spec = MapSpec::from_toml_str(&text)?;
        if spec.name != name {
            return Err(Error::Corrupt(format!(
                "map record `{name}` contains a spec named `{}`",
                spec.name
            )));
        }
        Ok(spec)
    }

    fn resolve_map(&self, spec: &MapSpec) -> Result<Vec<ResolvedTable>> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let find = |name: &str| bundle.schema.tables.iter().find(|t| t.name == name && !t.dead);
        let mut out = Vec::with_capacity(spec.tables.len());
        for t in &spec.tables {
            let src = find(&t.source).ok_or_else(|| {
                Error::Unsupported(format!("map `{}`: no source table `{}`", spec.name, t.source))
            })?;
            // #94's implicit rowid materializes IN the schema: a real column
            // named `rowid` carrying the single-column PK, plus the flag —
            // so the detection is "the pk column IS the implicit rowid", not
            // "there is no pk".
            let (src_key, src_key_ty, rowid_identity) = if src.primary_key.len() == 1 {
                let c = &src.columns[src.primary_key[0] as usize];
                let is_rowid =
                    src.implicit_rowid && c.name.eq_ignore_ascii_case("rowid");
                (c.name.clone(), c.ty, is_rowid)
            } else {
                return Err(Error::Unsupported(format!(
                    "map `{}`: `{}` has no single row identity (one-column PK or \
                     implicit rowid) — composite PKs are refused",
                    spec.name, t.source
                )));
            };
            for c in &t.columns {
                if !src.columns.iter().any(|sc| sc.name == c.source) {
                    return Err(Error::Unsupported(format!(
                        "map `{}`: no column `{}` in `{}`",
                        spec.name, c.source, t.source
                    )));
                }
                if c.source == src_key {
                    return Err(Error::Unsupported(format!(
                        "map `{}`: `{}` is the row identity of `{}` — it maps as the \
                         KEY, not as a value column",
                        spec.name, c.source, t.source
                    )));
                }
            }
            let dst_key = t.target_key.clone().unwrap_or_else(|| {
                if rowid_identity { "id".to_string() } else { src_key.clone() }
            });
            if t.columns.iter().any(|c| c.target == dst_key) {
                return Err(Error::Unsupported(format!(
                    "map `{}`: target column `{dst_key}` collides with the target key",
                    spec.name
                )));
            }
            let create_dst = match find(&t.target) {
                None => true,
                Some(d) => {
                    let pk_ok = d.primary_key.len() == 1
                        && d.columns[d.primary_key[0] as usize].name == dst_key;
                    if !pk_ok {
                        return Err(Error::Unsupported(format!(
                            "map `{}`: existing target `{}` must have `{dst_key}` as its \
                             single-column PRIMARY KEY",
                            spec.name, t.target
                        )));
                    }
                    for c in &t.columns {
                        if !d.columns.iter().any(|dc| dc.name == c.target) {
                            return Err(Error::Unsupported(format!(
                                "map `{}`: existing target `{}` has no column `{}`",
                                spec.name, t.target, c.target
                            )));
                        }
                    }
                    false
                }
            };
            let cols = t
                .columns
                .iter()
                .map(|c| {
                    Ok(ResolvedCol {
                        src: c.source.clone(),
                        dst: c.target.clone(),
                        lens: c
                            .pair
                            .as_deref()
                            .map(|p| self.load_lens_for_rretl(p))
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            out.push(ResolvedTable {
                src: t.source.clone(),
                dst: t.target.clone(),
                src_key,
                src_key_ty,
                rowid_identity,
                dst_key,
                create_dst,
                cols,
            });
        }
        Ok(out)
    }

    /// Run one sync of `name`, both directions, in ONE transaction. See the
    /// module doc for the row classification; a conflict aborts the whole
    /// sync with the row named. Returns what moved.
    pub fn rretl_map_sync(&self, name: &str) -> Result<MapSyncReport> {
        let spec = self.load_map(name)?;
        let resolved = self.resolve_map(&spec)?;
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = map_sync_in(&mut s, name, &resolved, &have);
        match out {
            Ok(mut report) => {
                let run_id = next_run_id(&mut s)?;
                report.run_id = run_id;
                LineageRow {
                    run_id,
                    lens: format!("map:{name}"),
                    forward_hash: format!("map:{name}"),
                    rex_hash: String::new(),
                    inverse_hash: format!("map:{name}"),
                    table: String::new(),
                    column: name.into(),
                    source_hash: String::new(),
                    output_hash: String::new(),
                    residual_hash: String::new(),
                    rows: report.changed_total() as i64,
                    outcome: "mapped",
                    error: report.note(),
                }
                .insert(&mut s)?;
                s.commit()?;
                Ok(report)
            }
            Err(e) => {
                s.rollback();
                let _ = self.record_failed_run(&format!("map:{name}"), "", name, &e);
                Err(e)
            }
        }
    }
}

// ------------------------------------------------------------------ sync

fn ensure_state_table(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use ColumnType::{Any, Blob, Int64, Text};
    if !shape_gate(have, T_MAP_STATE, &MAP_STATE_SHAPE)? {
        // `pk_enc` (canonical bits of the key) is the KEY — rigid Blob, so
        // point ops and the pass-3 resume both plan on the composite PK.
        // `k` carries the key VALUE (Any): canonical bits are one-way, and
        // pass 3 needs the value to ask both sides "are you still there?".
        crate::rretl::create_bookkeeping(
            s,
            T_MAP_STATE,
            vec![
                spec_col("map", Text),
                spec_col("tbl", Text),
                spec_col("pk_enc", Blob),
                spec_col("k", Any),
                spec_col("a_hash", Text),
                spec_col("b_hash", Text),
                spec_col("ts_micros", Int64),
            ],
            &["map", "tbl", "pk_enc"],
        )?;
    }
    Ok(())
}

fn map_sync_in(
    s: &mut WriteSession<'_>,
    name: &str,
    tables: &[ResolvedTable],
    have: &[(String, Vec<String>)],
) -> Result<MapSyncReport> {
    ensure_state_table(s, have)?;
    crate::rretl::ensure_lineage_tables(s, have)?;
    for rt in tables {
        if rt.create_dst && !have.iter().any(|(n, _)| n == &rt.dst) {
            let mut cols = vec![spec_col(&rt.dst_key, rt.src_key_ty)];
            for c in &rt.cols {
                cols.push(spec_col(&c.dst, ColumnType::Any));
            }
            crate::rretl::create_bookkeeping(s, &rt.dst, cols, &[&rt.dst_key])?;
        }
    }
    let mut report = MapSyncReport::default();
    for rt in tables {
        sync_table(s, name, rt, &mut report)?;
    }
    Ok(report)
}

fn quoted(cols: &[String]) -> String {
    cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
}

fn call1(p: &std::sync::Arc<mpedb_spell::ir::Proc>, x: &Value) -> Result<Value> {
    crate::spellfn::call_spell_fn(p, std::slice::from_ref(x))
}

fn chain(vals: &[Value]) -> String {
    let mut c = CanonChain::new();
    for v in vals {
        c.push(v);
    }
    c.hex()
}

/// forward per column; lossy is legal here (A→B only ever goes forward).
fn forward_all(rt: &ResolvedTable, xs: &[Value]) -> Result<Vec<Value>> {
    rt.cols
        .iter()
        .zip(xs)
        .map(|(c, x)| match &c.lens {
            None => Ok(x.clone()),
            Some(l) => call1(&l.forward, x).map_err(|e| {
                Error::Unsupported(format!(
                    "map sync: `{}`.`{}` value {x:?} refused by the pair's forward: {e}",
                    rt.src, c.src
                ))
            }),
        })
        .collect()
}

/// B→A for one row: per-column inverse with the LIVE residual (`rex` of the
/// source's current value), PutRes-gated; a lossy column that moved is a
/// named refusal — nothing can say what the edit means on the source side.
fn inverse_all(rt: &ResolvedTable, key: &Value, xs: &[Value], ys: &[Value]) -> Result<Vec<Value>> {
    rt.cols
        .iter()
        .zip(xs.iter().zip(ys))
        .map(|(c, (x, y))| {
            let fail = |msg: String| Error::Unsupported(msg);
            match &c.lens {
                None => Ok(y.clone()),
                Some(l) => match l.class {
                    LensClass::Lossy => {
                        let fwd = call1(&l.forward, x)?;
                        if crate::lens::same_value(&fwd, y) {
                            Ok(x.clone())
                        } else {
                            Err(fail(format!(
                                "map sync: `{}`.`{}` (row {key:?}) was edited on the target \
                                 side, but its pair is LOSSY — the edit has no meaning on \
                                 `{}`; revert the target edit or change the pair",
                                rt.dst, c.dst, rt.src
                            )))
                        }
                    }
                    LensClass::Bijective => {
                        let x2 = call1(&l.inverse, y).map_err(|e| {
                            fail(format!(
                                "map sync: inverse refuses `{}`.`{}` value {y:?} \
                                 (row {key:?}): {e}",
                                rt.dst, c.dst
                            ))
                        })?;
                        let fwd = call1(&l.forward, &x2)?;
                        if !crate::lens::same_value(&fwd, y) {
                            return Err(fail(format!(
                                "map sync: edit {y:?} on `{}`.`{}` (row {key:?}) is outside \
                                 the pair's image — forward(inverse(y)) = {fwd:?}",
                                rt.dst, c.dst
                            )));
                        }
                        Ok(x2)
                    }
                    LensClass::Residual => {
                        let rex = l.rex.as_ref().expect("residual pair has rex");
                        let r = call1(rex, x)?;
                        let x2 =
                            crate::spellfn::call_spell_fn(&l.inverse, &[y.clone(), r.clone()])
                                .map_err(|e| {
                                    fail(format!(
                                        "map sync: inverse refuses `{}`.`{}` value {y:?} \
                                         (row {key:?}): {e}",
                                        rt.dst, c.dst
                                    ))
                                })?;
                        let fwd = call1(&l.forward, &x2)?;
                        if !crate::lens::same_value(&fwd, y) {
                            return Err(fail(format!(
                                "map sync: edit {y:?} on `{}`.`{}` (row {key:?}) is outside \
                                 the pair's image — forward(inverse(y, rex(x))) = {fwd:?}",
                                rt.dst, c.dst
                            )));
                        }
                        let r2 = call1(rex, &x2)?;
                        if !crate::lens::same_value(&r2, &r) {
                            return Err(fail(format!(
                                "map sync: the residual does not survive the edit {y:?} on \
                                 `{}`.`{}` (row {key:?}) — rex(x') = {r2:?}, rex(x) = {r:?}",
                                rt.dst, c.dst
                            )));
                        }
                        Ok(x2)
                    }
                },
            }
        })
        .collect()
}

fn conflict(name_ctx: &str, dst: &str, key: &Value, why: &str) -> Error {
    Error::Unsupported(format!(
        "map sync `{name_ctx}`: CONFLICT on `{dst}` row {key:?} — {why}; the sync is \
         rolled back whole. Fix one side and re-sync"
    ))
}

fn sync_table(
    s: &mut WriteSession<'_>,
    name: &str,
    rt: &ResolvedTable,
    report: &mut MapSyncReport,
) -> Result<()> {
    let chunk = chunk_rows();
    let src_cols: Vec<String> = rt.cols.iter().map(|c| c.src.clone()).collect();
    let dst_cols: Vec<String> = rt.cols.iter().map(|c| c.dst.clone()).collect();
    let (sk, dk) = (&rt.src_key, &rt.dst_key);

    let state_get = "SELECT a_hash, b_hash FROM rretl_map_state \
                     WHERE map = $1 AND tbl = $2 AND pk_enc = $3";
    let state_put = "INSERT INTO rretl_map_state (map, tbl, pk_enc, k, a_hash, b_hash, \
                     ts_micros) VALUES ($1, $2, $3, $4, $5, $6, $7)";
    let state_set = "UPDATE rretl_map_state SET a_hash = $4, b_hash = $5, ts_micros = $6 \
                     WHERE map = $1 AND tbl = $2 AND pk_enc = $3";
    let state_del = "DELETE FROM rretl_map_state WHERE map = $1 AND tbl = $2 AND pk_enc = $3";
    let src_exists = format!("SELECT \"{sk}\" FROM \"{}\" WHERE \"{sk}\" = $1", rt.src);
    let dst_get = format!(
        "SELECT {} FROM \"{}\" WHERE \"{dk}\" = $1",
        quoted(&dst_cols),
        rt.dst
    );
    let src_get = format!(
        "SELECT {} FROM \"{}\" WHERE \"{sk}\" = $1",
        quoted(&src_cols),
        rt.src
    );
    let dst_ins = format!(
        "INSERT INTO \"{}\" (\"{dk}\", {}) VALUES ($1{})",
        rt.dst,
        quoted(&dst_cols),
        (0..dst_cols.len()).map(|i| format!(", ${}", i + 2)).collect::<String>()
    );
    let src_ins = format!(
        "INSERT INTO \"{}\" (\"{sk}\", {}) VALUES ($1{})",
        rt.src,
        quoted(&src_cols),
        (0..src_cols.len()).map(|i| format!(", ${}", i + 2)).collect::<String>()
    );
    let dst_upd = format!(
        "UPDATE \"{}\" SET {} WHERE \"{dk}\" = $1",
        rt.dst,
        dst_cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("\"{c}\" = ${}", i + 2))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let src_upd = format!(
        "UPDATE \"{}\" SET {} WHERE \"{sk}\" = $1",
        rt.src,
        src_cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("\"{c}\" = ${}", i + 2))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let state_key = |pk_enc: Vec<u8>| {
        vec![
            Value::Text(name.into()),
            Value::Text(rt.dst.clone()),
            Value::Blob(pk_enc),
        ]
    };
    let with =
        |mut base: Vec<Value>, rest: &[Value]| {
            base.extend(rest.iter().cloned());
            base
        };

    // Re-read a just-written row and verify its chain: the state hash is the
    // FUTURE oracle for this row, so it records what is persisted, never
    // what was intended (the stage-3 finding-14 discipline).
    let verify_persisted = |s: &mut WriteSession<'_>,
                            get: &str,
                            key: &Value,
                            want: &str,
                            side: &str|
     -> Result<()> {
        let rows = rows_of(s.query(get, std::slice::from_ref(key))?)?;
        let got = rows
            .first()
            .map(|r| chain(r))
            .ok_or_else(|| Error::Corrupt(format!("map sync: {side} row {key:?} vanished")))?;
        if got != want {
            return Err(Error::Corrupt(format!(
                "map sync: persisted {side} row {key:?} does not hash to what was written"
            )));
        }
        Ok(())
    };

    // ---- pass 1: every source row -----------------------------------------
    let first = format!(
        "SELECT \"{sk}\", {} FROM \"{}\" ORDER BY \"{sk}\" LIMIT {chunk}",
        quoted(&src_cols),
        rt.src
    );
    let next = format!(
        "SELECT \"{sk}\", {} FROM \"{}\" WHERE \"{sk}\" > $1 ORDER BY \"{sk}\" LIMIT {chunk}",
        quoted(&src_cols),
        rt.src
    );
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &[])?)?,
            Some(k) => rows_of(s.query(&next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let key = &row[0];
            let xs = &row[1..];
            let pk_enc = crate::lens::value_bits(key);
            let a_now = chain(xs);
            let st = rows_of(s.query(state_get, &state_key(pk_enc.clone()))?)?;
            let st = st.first().map(|r| {
                (
                    crate::rretl::as_text(&r[0]),
                    crate::rretl::as_text(&r[1]),
                )
            });
            let b = rows_of(s.query(&dst_get, std::slice::from_ref(key))?)?;
            let b = b.into_iter().next();
            let ts = Value::Int(now_micros());
            match (st, b) {
                (None, None) => {
                    // New in A: forward-materialize.
                    let ys = forward_all(rt, xs)?;
                    s.query(&dst_ins, &with(vec![key.clone()], &ys))?;
                    let b_now = chain(&ys);
                    verify_persisted(s, &dst_get, key, &b_now, "target")?;
                    s.query(
                        state_put,
                        &with(
                            state_key(pk_enc),
                            &[key.clone(), Value::Text(a_now), Value::Text(b_now), ts],
                        ),
                    )?;
                    report.created_b += 1;
                }
                (None, Some(ybs)) => {
                    // Both exist with no recorded sync: adopt only agreement.
                    let ys = forward_all(rt, xs)?;
                    let agree = ys
                        .iter()
                        .zip(&ybs)
                        .all(|(a, b)| crate::lens::same_value(a, b));
                    if !agree {
                        return Err(conflict(
                            name,
                            &rt.dst,
                            key,
                            "both sides hold the row and disagree, and no recorded sync \
                             can arbitrate",
                        ));
                    }
                    let b_now = chain(&ybs);
                    s.query(
                        state_put,
                        &with(
                            state_key(pk_enc),
                            &[key.clone(), Value::Text(a_now), Value::Text(b_now), ts],
                        ),
                    )?;
                    report.unchanged += 1;
                }
                (Some((st_a, _)), None) => {
                    // Deleted on B.
                    if a_now != st_a {
                        return Err(conflict(
                            name,
                            &rt.dst,
                            key,
                            "deleted on the target side while the source row changed",
                        ));
                    }
                    s.query(
                        &format!("DELETE FROM \"{}\" WHERE \"{sk}\" = $1", rt.src),
                        std::slice::from_ref(key),
                    )?;
                    s.query(state_del, &state_key(pk_enc))?;
                    report.deleted_a += 1;
                }
                (Some((st_a, st_b)), Some(ybs)) => {
                    let b_now = chain(&ybs);
                    match (a_now != st_a, b_now != st_b) {
                        (false, false) => report.unchanged += 1,
                        (true, false) => {
                            let ys = forward_all(rt, xs)?;
                            s.query(&dst_upd, &with(vec![key.clone()], &ys))?;
                            let b_new = chain(&ys);
                            verify_persisted(s, &dst_get, key, &b_new, "target")?;
                            s.query(
                                state_set,
                                &with(
                                    state_key(pk_enc),
                                    &[Value::Text(a_now), Value::Text(b_new), ts],
                                ),
                            )?;
                            report.a_to_b += 1;
                        }
                        (false, true) => {
                            let xs2 = inverse_all(rt, key, xs, &ybs)?;
                            s.query(&src_upd, &with(vec![key.clone()], &xs2))?;
                            let a_new = chain(&xs2);
                            verify_persisted(s, &src_get, key, &a_new, "source")?;
                            s.query(
                                state_set,
                                &with(
                                    state_key(pk_enc),
                                    &[Value::Text(a_new), Value::Text(b_now), ts],
                                ),
                            )?;
                            report.b_to_a += 1;
                        }
                        (true, true) => {
                            return Err(conflict(
                                name,
                                &rt.dst,
                                key,
                                "both sides changed since the last sync",
                            ));
                        }
                    }
                }
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    // ---- pass 2: target rows with no source row ---------------------------
    let first = format!(
        "SELECT \"{dk}\", {} FROM \"{}\" ORDER BY \"{dk}\" LIMIT {chunk}",
        quoted(&dst_cols),
        rt.dst
    );
    let next = format!(
        "SELECT \"{dk}\", {} FROM \"{}\" WHERE \"{dk}\" > $1 ORDER BY \"{dk}\" LIMIT {chunk}",
        quoted(&dst_cols),
        rt.dst
    );
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &[])?)?,
            Some(k) => rows_of(s.query(&next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let key = &row[0];
            let ybs = &row[1..];
            if !rows_of(s.query(&src_exists, std::slice::from_ref(key))?)?.is_empty() {
                continue; // handled in pass 1
            }
            let pk_enc = crate::lens::value_bits(key);
            let st = rows_of(s.query(state_get, &state_key(pk_enc.clone()))?)?;
            let ts = Value::Int(now_micros());
            match st.first() {
                Some(r) => {
                    // Deleted on A.
                    let st_b = crate::rretl::as_text(&r[1]);
                    if chain(ybs) != st_b {
                        return Err(conflict(
                            name,
                            &rt.dst,
                            key,
                            "deleted on the source side while the target row changed",
                        ));
                    }
                    s.query(
                        &format!("DELETE FROM \"{}\" WHERE \"{dk}\" = $1", rt.dst),
                        std::slice::from_ref(key),
                    )?;
                    s.query(state_del, &state_key(pk_enc))?;
                    report.deleted_b += 1;
                }
                None => {
                    // New in B: the creation path. Only when the identity is
                    // real (not a source-assigned rowid) and every column
                    // inverts without a residual.
                    if rt.rowid_identity {
                        return Err(Error::Unsupported(format!(
                            "map sync `{name}`: row {key:?} was created on `{}` but the \
                             source identity is `{}`'s hidden rowid, which only the \
                             source can assign — create the row on the source side",
                            rt.dst, rt.src
                        )));
                    }
                    if let Some(c) = rt.cols.iter().find(|c| {
                        c.lens
                            .as_ref()
                            .is_some_and(|l| l.class != LensClass::Bijective)
                    }) {
                        return Err(Error::Unsupported(format!(
                            "map sync `{name}`: row {key:?} was created on `{}`, but \
                             column `{}` maps through a non-bijective pair — there is no \
                             residual to attach (§4's creation path); create the row on \
                             `{}` instead",
                            rt.dst, c.dst, rt.src
                        )));
                    }
                    let xs2: Vec<Value> = rt
                        .cols
                        .iter()
                        .zip(ybs)
                        .map(|(c, y)| match &c.lens {
                            None => Ok(y.clone()),
                            Some(l) => {
                                let x = call1(&l.inverse, y)?;
                                let fwd = call1(&l.forward, &x)?;
                                if !crate::lens::same_value(&fwd, y) {
                                    return Err(Error::Unsupported(format!(
                                        "map sync `{name}`: created row {key:?} on `{}` has \
                                         `{}` = {y:?}, outside its pair's image",
                                        rt.dst, c.dst
                                    )));
                                }
                                Ok(x)
                            }
                        })
                        .collect::<Result<_>>()?;
                    s.query(&src_ins, &with(vec![key.clone()], &xs2))?;
                    let a_now = chain(&xs2);
                    verify_persisted(s, &src_get, key, &a_now, "source")?;
                    s.query(
                        state_put,
                        &with(
                            state_key(pk_enc),
                            &[key.clone(), Value::Text(a_now), Value::Text(chain(ybs)), ts],
                        ),
                    )?;
                    report.created_a += 1;
                }
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    // ---- pass 3: state rows with NEITHER side left ------------------------
    // Both-sides-deleted keys leave a state row passes 1/2 never visit (no
    // row on either side). Left behind, a later RE-CREATION of the key would
    // read as "deleted on the other side while this side changed" — a false
    // conflict. The stored key VALUE makes the check exact: a state row
    // whose key misses BOTH point lookups is cleared.
    let first = format!(
        "SELECT pk_enc, k FROM rretl_map_state WHERE map = $1 AND tbl = $2 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let next = format!(
        "SELECT pk_enc, k FROM rretl_map_state WHERE map = $1 AND tbl = $2 AND pk_enc > $3 \
         ORDER BY pk_enc LIMIT {chunk}"
    );
    let dst_exists = format!("SELECT \"{dk}\" FROM \"{}\" WHERE \"{dk}\" = $1", rt.dst);
    let base = vec![Value::Text(name.into()), Value::Text(rt.dst.clone())];
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &base)?)?,
            Some(k) => rows_of(s.query(&next, &with(base.clone(), std::slice::from_ref(k)))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let key = &row[1];
            let in_src = !rows_of(s.query(&src_exists, std::slice::from_ref(key))?)?.is_empty();
            let in_dst = !rows_of(s.query(&dst_exists, std::slice::from_ref(key))?)?.is_empty();
            if !in_src && !in_dst {
                let pk_enc = match &row[0] {
                    Value::Blob(b) => b.clone(),
                    other => {
                        return Err(Error::Corrupt(format!(
                            "map state pk_enc is not a blob: {other:?}"
                        )))
                    }
                };
                s.query(state_del, &state_key(pk_enc))?;
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    Ok(())
}
