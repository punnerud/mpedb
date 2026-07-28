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

    /// Canonical TOML for this spec — what the programmatic define path
    /// stores, so a dict-built map and a TOML-built map are the same record
    /// shape. Safe to emit with plain quoting: `validate` gates every name
    /// through `ident_ok` (ASCII `[A-Za-z_][A-Za-z0-9_]*`), so no value can
    /// contain a quote, a newline or a backslash.
    pub fn to_toml(&self) -> String {
        let mut out = format!("[map]\nname = \"{}\"\n", self.name);
        for t in &self.tables {
            out.push_str(&format!(
                "\n[[map.table]]\nsource = \"{}\"\ntarget = \"{}\"\n",
                t.source, t.target
            ));
            if let Some(k) = &t.target_key {
                out.push_str(&format!("target_key = \"{k}\"\n"));
            }
            for c in &t.columns {
                out.push_str(&format!(
                    "  [[map.table.column]]\n  source = \"{}\"\n  target = \"{}\"\n",
                    c.source, c.target
                ));
                if let Some(p) = &c.pair {
                    out.push_str(&format!("  pair = \"{p}\"\n"));
                }
            }
        }
        out
    }

    fn validate(&self) -> Result<()> {
        if !ident_ok(&self.name) {
            return Err(Error::Unsupported(format!(
                "map spec: `{}` is not a legal map name",
                self.name
            )));
        }
        // The dict/`define_spec` door validates HERE while every load parses
        // TOML, so anything the parser requires must be required here too —
        // otherwise a stored record exists that the engine's own reader
        // rejects (the schema saboteur's finding).
        if self.tables.is_empty() {
            return Err(Error::Unsupported(
                "map spec: at least one table mapping is required".into(),
            ));
        }
        let mut seen_dst = std::collections::HashSet::new();
        let mut seen_src = std::collections::HashSet::new();
        for t in &self.tables {
            if t.columns.is_empty() {
                return Err(Error::Unsupported(format!(
                    "map spec: table `{}` needs at least one column mapping",
                    t.source
                )));
            }
            for (n, what) in [(&t.source, "source table"), (&t.target, "target table")] {
                if !ident_ok(n) {
                    return Err(Error::Unsupported(format!(
                        "map spec: `{n}` is not a legal {what} name"
                    )));
                }
                // Case-INSENSITIVELY: SQL identifiers are, so a target named
                // `RRETL_MAP_STATE` would otherwise walk past this guard and
                // die later on a raw duplicate-name error.
                if crate::rretl::rretl_bookkeeping_names()
                    .iter()
                    .any(|b| n.eq_ignore_ascii_case(b))
                {
                    return Err(Error::Unsupported(format!(
                        "map spec: `{n}` is rRETL bookkeeping; mapping it is refused"
                    )));
                }
            }
            seen_src.insert(t.source.to_ascii_lowercase());
            if t.source == t.target {
                return Err(Error::Unsupported(format!(
                    "map spec: `{}` maps onto itself — source and target must differ",
                    t.source
                )));
            }
            if !seen_dst.insert(t.target.to_ascii_lowercase()) {
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
                if !dst_cols.insert(c.target.to_ascii_lowercase()) {
                    return Err(Error::Unsupported(format!(
                        "map spec: target column `{}` of `{}` is written twice",
                        c.target, t.target
                    )));
                }
            }
        }
        // CHAINED entries (one entry's target is another's source) break the
        // twin-ness of check and sync: sync applies entry 1's writes before
        // classifying entry 2, while a read-only check classifies both
        // against the same committed state — so check under-counts and can
        // miss conflicts entirely (the schema saboteur's finding). A staging
        // chain is two maps, synced in the order you choose, not one map
        // whose entries feed each other.
        for t in &self.tables {
            if seen_src.contains(&t.target.to_ascii_lowercase()) {
                return Err(Error::Unsupported(format!(
                    "map spec: `{}` is both a target and a source in this map — chained \
                     entries are refused (one entry's writes would be invisible to the \
                     other's classification); use two maps",
                    t.target
                )));
            }
        }
        Ok(())
    }
}

// --------------------------------------------------------------- resolved

pub(crate) struct ResolvedCol {
    pub(crate) src: String,
    pub(crate) dst: String,
    lens: Option<crate::lens::RretlLens>,
}

pub(crate) struct ResolvedTable {
    pub(crate) src: String,
    pub(crate) dst: String,
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

/// One mapped table pair's result from [`Database::rretl_map_check`] — a
/// read-only dry run of the sync classification. `conflicts` collects
/// EVERY named sync-blocker (the sync aborts on the first; the check keeps
/// going and names them all). `diverged` is the invariant the echo guard
/// structurally cannot see: state records both sides clean, yet
/// `forward(source) != target` — a standing, silent divergence (tampered
/// state, a redefine that dodged re-baselining, or an engine bug).
/// `orphan_state` counts state rows whose key is gone from BOTH sides;
/// that is pending cleanup for the next sync's pass 3, not a breach.
#[derive(Debug, Default)]
pub struct MapCheckTable {
    pub src: String,
    pub dst: String,
    pub pending_a2b: u64,
    pub pending_b2a: u64,
    pub would_create_b: u64,
    pub would_create_a: u64,
    pub would_delete_a: u64,
    pub would_delete_b: u64,
    pub would_adopt: u64,
    pub unchanged: u64,
    pub orphan_state: u64,
    pub conflicts: Vec<String>,
    pub diverged: Vec<String>,
}

#[derive(Debug, Default)]
pub struct MapCheckReport {
    pub tables: Vec<MapCheckTable>,
}

impl MapCheckReport {
    /// Rows a sync would move or record (state adoptions included).
    pub fn pending_total(&self) -> u64 {
        self.tables
            .iter()
            .map(|t| {
                t.pending_a2b
                    + t.pending_b2a
                    + t.would_create_b
                    + t.would_create_a
                    + t.would_delete_a
                    + t.would_delete_b
                    + t.would_adopt
            })
            .sum()
    }

    /// Every named breach: conflicts a sync would abort on, plus diverged
    /// rows. Empty = a sync would run through.
    pub fn breaches(&self) -> Vec<&String> {
        self.tables
            .iter()
            .flat_map(|t| t.conflicts.iter().chain(t.diverged.iter()))
            .collect()
    }

    /// Nothing to do and nothing wrong — the post-sync steady state.
    pub fn is_clean(&self) -> bool {
        self.pending_total() == 0
            && self.breaches().is_empty()
            && self.tables.iter().all(|t| t.orphan_state == 0)
    }
}

impl crate::Database {
    /// Store (or replace) a mapping. The spec is validated NOW — sources
    /// exist with a usable row identity, every named pair loads and is
    /// healthy — so `map sync` never discovers a broken definition mid-run.
    pub fn rretl_map_define(&self, toml_text: &str) -> Result<()> {
        let spec = MapSpec::from_toml_str(toml_text)?;
        self.resolve_map(&spec)?;
        self.refuse_overlap_with_other_maps(&spec)?;
        self.store_map_record(&spec.name, toml_text)
    }

    /// A table may be a map SOURCE or a map TARGET, never both, and never a
    /// target twice. One rule, three real failures behind it: two maps
    /// sharing a target silently merge two unrelated masters into each
    /// other; a reverse map (`a→b` plus `b→a`) makes every ordinary edit an
    /// unresolvable conflict, because each map sees the other's push as
    /// "the other side moved too"; and a cross-map chain has the same
    /// check-vs-sync blindness as a chain inside one map. Loop safety is
    /// per map — this keeps the topology inside what that guarantee covers.
    fn refuse_overlap_with_other_maps(&self, spec: &MapSpec) -> Result<()> {
        for other in self.rretl_maps()? {
            if other == spec.name {
                continue;
            }
            let o = match self.load_map(&other) {
                Ok(o) => o,
                Err(_) => continue, // unreadable records are fsck's business
            };
            let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
            for t in &spec.tables {
                for ot in &o.tables {
                    let clash = if same(&t.target, &ot.target) {
                        Some(("target", t.target.as_str(), "target"))
                    } else if same(&t.target, &ot.source) {
                        Some(("target", t.target.as_str(), "source"))
                    } else if same(&t.source, &ot.target) {
                        Some(("source", t.source.as_str(), "target"))
                    } else {
                        None
                    };
                    if let Some((mine, table, theirs)) = clash {
                        return Err(Error::Unsupported(format!(
                            "map `{}`: `{table}` is its {mine}, but map `{other}` already \
                             uses that table as its {theirs} — a table may be a map source \
                             or a map target, never both, and never a target twice (loop \
                             safety is per map)",
                            spec.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// [`rretl_map_define`](Self::rretl_map_define) from a CONSTRUCTED spec —
    /// the programmatic path (Python hands a dict, Rust hands a `MapSpec`),
    /// same validation, same stored form: the record is the canonical TOML
    /// the spec emits, so `map show` and re-parsing behave identically
    /// whichever door the definition came through.
    pub fn rretl_map_define_spec(&self, spec: &MapSpec) -> Result<()> {
        spec.validate()?;
        self.resolve_map(spec)?;
        self.refuse_overlap_with_other_maps(spec)?;
        self.store_map_record(&spec.name, &spec.to_toml())
    }

    fn store_map_record(&self, name: &str, toml_text: &str) -> Result<()> {
        let mut record = vec![MAP_RECORD_V1];
        record.extend_from_slice(toml_text.as_bytes());
        // A CHANGED spec under the same name RE-BASELINES: the state rows'
        // hashes were recorded under the old spec, and against a new one
        // they misclassify — same columns with a swapped pair leaves every
        // chain untouched, so every row reads "both clean" and the target
        // keeps the OLD pair's forward forever, silently. Deleting the
        // map's state (atomically with the record) forces the next sync
        // through the no-state paths: adopt-on-agree re-arbitrates, and
        // disagreement is a named conflict. An UNCHANGED re-define keeps
        // state, so a dropped map's history stays adoptable.
        let have = self.committed_tables()?;
        let has_state = have.iter().any(|(n, _)| n == T_MAP_STATE);
        let mut s = self.begin()?;
        let res = (|| -> Result<()> {
            let prior = s.sys_record_get(NS_MAP, name.as_bytes())?;
            // ONLY a byte-identical re-define keeps state. Anything else —
            // a changed spec, a FIRST define, a define after `map drop` —
            // starts from nothing. The two openings that "keep state unless
            // the record changed" left were both real: a drop plus a changed
            // redefine kept the old spec's hashes (the re-baseline it needed
            // was skipped because there was no prior record), and state
            // planted under an undefined name was ADOPTED by that name's
            // first define, whose first sync then deleted source rows. State
            // is a baseline, never evidence; re-arbitrating from scratch is
            // always safe (agreement adopts, disagreement is a conflict).
            if has_state && prior.as_deref() != Some(record.as_slice()) {
                s.query(
                    "DELETE FROM rretl_map_state WHERE map = $1",
                    &[Value::Text(name.into())],
                )?;
            }
            s.sys_record_put(NS_MAP, name.as_bytes(), &record)?;
            s.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => s.commit()?,
            Err(e) => {
                s.rollback();
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

    /// Drop a mapping AND its sync state, in one transaction. State that
    /// outlives its map is state no oracle scans and no sync re-baselines —
    /// a later map of the same name would inherit a baseline it never
    /// wrote. `true` when the mapping existed.
    pub fn rretl_map_drop(&self, name: &str) -> Result<bool> {
        let have = self.committed_tables()?;
        let has_state = have.iter().any(|(n, _)| n == T_MAP_STATE);
        let mut s = self.begin()?;
        let existed = match s.sys_record_get(NS_MAP, name.as_bytes()) {
            Ok(v) => v.is_some(),
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        };
        let res = (|| -> Result<()> {
            if has_state {
                s.query(
                    "DELETE FROM rretl_map_state WHERE map = $1",
                    &[Value::Text(name.into())],
                )?;
            }
            s.sys_record_delete(NS_MAP, name.as_bytes())?;
            s.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => s.commit()?,
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        }
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(existed)
    }

    pub(crate) fn load_map(&self, name: &str) -> Result<MapSpec> {
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

    pub(crate) fn resolve_map(&self, spec: &MapSpec) -> Result<Vec<ResolvedTable>> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        // SQL identifiers are case-INSENSITIVE, so a spec must resolve the
        // same way `SELECT * FROM SRC` does. Everything downstream then uses
        // the SCHEMA's spelling — interpolated SQL, state keys, messages —
        // so one table is one table however the spec spelled it. Before
        // this, a source in the wrong case was refused ("no source table
        // `SRC`") while a TARGET in the wrong case was accepted and then
        // auto-created into a raw `duplicate table name` at sync, leaving a
        // permanently unsyncable map that `check` reported as ordinary
        // pending creations.
        let find = |name: &str| {
            bundle
                .schema
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name) && !t.dead)
        };
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
            let mut src_col_names = Vec::with_capacity(t.columns.len());
            for c in &t.columns {
                let Some(sc) = src.columns.iter().find(|sc| sc.name.eq_ignore_ascii_case(&c.source))
                else {
                    return Err(Error::Unsupported(format!(
                        "map `{}`: no column `{}` in `{}`",
                        spec.name, c.source, t.source
                    )));
                };
                src_col_names.push(sc.name.clone());
                if sc.name.eq_ignore_ascii_case(&src_key) {
                    return Err(Error::Unsupported(format!(
                        "map `{}`: `{}` is the row identity of `{}` — it maps as the \
                         KEY, not as a value column",
                        spec.name, c.source, t.source
                    )));
                }
            }
            let mut dst_key = t.target_key.clone().unwrap_or_else(|| {
                if rowid_identity { "id".to_string() } else { src_key.clone() }
            });
            if t.columns.iter().any(|c| c.target.eq_ignore_ascii_case(&dst_key)) {
                return Err(Error::Unsupported(format!(
                    "map `{}`: target column `{dst_key}` collides with the target key",
                    spec.name
                )));
            }
            let mut dst_name = t.target.clone();
            let mut dst_col_names: Vec<String> =
                t.columns.iter().map(|c| c.target.clone()).collect();
            let create_dst = match find(&t.target) {
                None => true,
                Some(d) => {
                    dst_name = d.name.clone();
                    let pk_ok = d.primary_key.len() == 1
                        && d.columns[d.primary_key[0] as usize]
                            .name
                            .eq_ignore_ascii_case(&dst_key);
                    if !pk_ok {
                        let have = d
                            .primary_key
                            .iter()
                            .map(|i| d.columns[*i as usize].name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        // Name where `dst_key` CAME FROM: with no explicit
                        // `target_key` it is the source's identity, so a
                        // rename on the SOURCE surfaces here and the old
                        // message blamed the target for it.
                        let origin = match &t.target_key {
                            Some(_) => "the mapping's `target_key`".to_string(),
                            None => format!(
                                "the row identity of source `{}` (no `target_key` given)",
                                t.source
                            ),
                        };
                        return Err(Error::Unsupported(format!(
                            "map `{}`: target `{}` has PRIMARY KEY ({have}), but the \
                             mapping needs `{dst_key}` — which comes from {origin}. \
                             Rename one side, or set `target_key`",
                            spec.name, d.name
                        )));
                    }
                    dst_key = d.columns[d.primary_key[0] as usize].name.clone();
                    // Row identity maps VERBATIM, so a rigid target key of a
                    // different type refuses every point lookup the sync
                    // makes — turning a definition mistake into an
                    // engine-level type error at sync/check/fsck time, which
                    // then took the whole fsck down with it.
                    let dk_ty = d.columns[d.primary_key[0] as usize].ty;
                    if dk_ty != ColumnType::Any
                        && src_key_ty != ColumnType::Any
                        && dk_ty != src_key_ty
                    {
                        return Err(Error::Unsupported(format!(
                            "map `{}`: `{}`.`{dst_key}` is {dk_ty:?} but the row identity \
                             `{}`.`{src_key}` is {src_key_ty:?} — identity maps verbatim, \
                             so the key types must match (or be typeless)",
                            spec.name, t.target, t.source
                        )));
                    }
                    for (i, c) in t.columns.iter().enumerate() {
                        let Some(dc) =
                            d.columns.iter().find(|dc| dc.name.eq_ignore_ascii_case(&c.target))
                        else {
                            return Err(Error::Unsupported(format!(
                                "map `{}`: existing target `{}` has no column `{}`",
                                spec.name, d.name, c.target
                            )));
                        };
                        dst_col_names[i] = dc.name.clone();
                    }
                    false
                }
            };
            let cols = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    Ok(ResolvedCol {
                        src: src_col_names[i].clone(),
                        dst: dst_col_names[i].clone(),
                        lens: c
                            .pair
                            .as_deref()
                            .map(|p| self.load_lens_for_rretl(p))
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            out.push(ResolvedTable {
                src: src.name.clone(),
                dst: dst_name,
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

    /// Read-only dry run of [`rretl_map_sync`](Self::rretl_map_sync):
    /// classify every row exactly as the sync would, without writing —
    /// what WOULD move, every conflict named (not just the first the sync
    /// aborts on), plus the two audits a sync never performs: `diverged`
    /// (state clean on both sides while `forward(source) != target`) and
    /// `orphan_state`. Like `rretl_fsck` it is not one snapshot across
    /// chunks — EXACT WHEN NOTHING WRITES CONCURRENTLY. Under a live sync
    /// any single finding may be a cross-snapshot artifact (source read
    /// from one commit, target from the next), so a churning system should
    /// be quiesced before a finding is believed; the counts stay useful as
    /// a progress signal either way.
    pub fn rretl_map_check(&self, name: &str) -> Result<MapCheckReport> {
        let spec = self.load_map(name)?;
        let resolved = self.resolve_map(&spec)?;
        let have = self.committed_tables()?;
        let has_state = have.iter().any(|(n, _)| n == T_MAP_STATE);
        let mut report = MapCheckReport::default();
        for rt in &resolved {
            let has_dst = have.iter().any(|(n, _)| n == &rt.dst);
            report.tables.push(check_table(self, name, rt, has_state, has_dst)?);
        }
        Ok(report)
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
    prepare_map_tables(s, name, tables, have)?;
    let mut report = MapSyncReport::default();
    for rt in tables {
        sync_table(s, name, rt, &mut report)?;
    }
    Ok(report)
}

/// What both `map sync` and the daemon must do before touching a row:
/// the bookkeeping exists, and a target that is legitimately missing is
/// materialized — while one that VANISHED under standing state is refused.
pub(crate) fn prepare_map_tables(
    s: &mut WriteSession<'_>,
    name: &str,
    tables: &[ResolvedTable],
    have: &[(String, Vec<String>)],
) -> Result<()> {
    ensure_state_table(s, have)?;
    crate::rretl::ensure_lineage_tables(s, have)?;
    for rt in tables {
        if rt.create_dst && !have.iter().any(|(n, _)| n == &rt.dst) {
            // A missing target is "materialize it" ONLY the first time. Once
            // the map has state for that target, missing means the table was
            // DROPPED or renamed away — and reading that as "every row was
            // deleted on the target side" propagates the drop into the
            // source and empties the master (the schema saboteur's finding).
            // One missing-table condition cannot mean two opposite things.
            if state_rows_for(s, name, &rt.dst)? > 0 {
                return Err(Error::Unsupported(format!(
                    "map sync `{name}`: the target table `{}` is GONE but the map has \
                     sync state for it — it was dropped or renamed away. Refusing: \
                     treating this as a target-side delete would empty `{}`. Restore \
                     the table, or `map drop` + define again (which clears the state) \
                     to re-materialize it",
                    rt.dst, rt.src
                )));
            }
            let mut cols = vec![spec_col(&rt.dst_key, rt.src_key_ty)];
            for c in &rt.cols {
                cols.push(spec_col(&c.dst, ColumnType::Any));
            }
            crate::rretl::create_bookkeeping(s, &rt.dst, cols, &[&rt.dst_key])?;
        }
    }
    Ok(())
}

/// Does this state row's `k` (the key VALUE) still hash to its `pk_enc`
/// (the key's bookkeeping reference)? Nothing but tampering or corruption
/// can break it — the sync writes both from one value.
pub(crate) fn state_row_is_consistent(pk_enc: &Value, k: &Value) -> bool {
    matches!(pk_enc, Value::Blob(b) if *b == crate::rretl::pk_ref(k))
}

/// How many state rows this map holds for one target table.
fn state_rows_for(s: &mut WriteSession<'_>, name: &str, dst: &str) -> Result<i64> {
    let r = rows_of(s.query(
        "SELECT count(*) FROM rretl_map_state WHERE map = $1 AND tbl = $2",
        &[Value::Text(name.into()), Value::Text(dst.into())],
    )?)?;
    crate::rretl::as_int(&r[0][0])
}

fn quoted(cols: &[String]) -> String {
    cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
}

fn call1(p: &std::sync::Arc<mpedb_spell::ir::Proc>, x: &Value) -> Result<Value> {
    crate::spellfn::call_spell_fn(p, std::slice::from_ref(x))
}

/// A state hash. The map name, the TARGET table and the row KEY are
/// chained in ahead of the values, so a recorded hash is meaningless
/// anywhere but the row it was written for. Without the binding the hash
/// covered the mapped values alone, which made it PORTABLE: identical
/// values anywhere — a decoy row, a decoy map over the same columns —
/// produced a byte-identical hash, so state could be forged with two
/// ordinary SQL statements and no hashing at all (the bookkeeping
/// saboteur's enabler finding).
fn chain(name: &str, dst: &str, key: &Value, vals: &[Value]) -> String {
    let mut c = CanonChain::new();
    c.push(&Value::Text(name.to_string()));
    c.push(&Value::Text(dst.to_string()));
    c.push(key);
    for v in vals {
        c.push(v);
    }
    c.hex()
}

/// forward per column; lossy is legal here (A→B only ever goes forward).
/// The row is named in the refusal — in a chunked batch the offending
/// value alone does not locate the row (the value saboteur's finding:
/// inverse refusals named the row, forward refusals did not).
fn forward_all(rt: &ResolvedTable, key: &Value, xs: &[Value]) -> Result<Vec<Value>> {
    rt.cols
        .iter()
        .zip(xs)
        .map(|(c, x)| match &c.lens {
            None => Ok(x.clone()),
            Some(l) => call1(&l.forward, x).map_err(|e| {
                Error::Unsupported(format!(
                    "map sync: `{}`.`{}` value {x:?} (row {key:?}) refused by the \
                     pair's forward: {e}",
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

// ------------------------------------------------------- classification
//
// ONE decision function, three consumers: `sync_table` (writes, one txn),
// `check_table` (writes nothing, counts and audits) and the daemon's
// `run_table` (writes, commits per chunk). It performs no I/O — the caller
// hands it what it read and carries out what comes back — so the three can
// never drift apart on what a row MEANS. They differ only in what they do
// about it.

/// What a source-side row (pass 1) means, given its state row and the
/// target row that shares its key.
pub(crate) enum P1 {
    /// New in A: materialize on B.
    CreateB { ys: Vec<Value>, a_hash: String },
    /// Both sides hold it with no recorded sync, and they AGREE: adopt.
    AdoptB { a_hash: String, b_hash: String },
    /// Gone from B while A stood still: delete A.
    DeleteA,
    /// Both sides clean. `diverged` is set only when the caller asked for
    /// the audit (`verify_clean`) and `forward(A) != B` — the breach the
    /// echo guard structurally cannot see.
    Clean { diverged: Option<String> },
    AToB { ys: Vec<Value>, a_hash: String },
    BToA { xs2: Vec<Value>, b_hash: String },
    /// Named, and never acted on. `map sync` aborts the whole run on it;
    /// the daemon counts it, skips the row and keeps going.
    Conflict(String),
}

/// Classify one source row. `st` is its state row (a_hash, b_hash) and `b`
/// its target row's mapped values, both as READ by the caller.
pub(crate) fn classify_p1(
    rt: &ResolvedTable,
    name: &str,
    key: &Value,
    xs: &[Value],
    st: Option<(String, String)>,
    b: Option<Vec<Value>>,
    verify_clean: bool,
) -> Result<P1> {
    let a_now = chain(name, &rt.dst, key, xs);
    let agree = |ys: &[Value], ybs: &[Value]| {
        ys.iter().zip(ybs).all(|(a, b)| crate::lens::same_value(a, b))
    };
    Ok(match (st, b) {
        (None, None) => match forward_all(rt, key, xs) {
            Ok(ys) => P1::CreateB { ys, a_hash: a_now },
            Err(e) => P1::Conflict(e.to_string()),
        },
        (None, Some(ybs)) => match forward_all(rt, key, xs) {
            Ok(ys) if agree(&ys, &ybs) => P1::AdoptB {
                a_hash: a_now,
                b_hash: chain(name, &rt.dst, key, &ybs),
            },
            Ok(_) => P1::Conflict(
                conflict(
                    name,
                    &rt.dst,
                    key,
                    "both sides hold the row and disagree, and no recorded sync can arbitrate",
                )
                .to_string(),
            ),
            Err(e) => P1::Conflict(e.to_string()),
        },
        (Some((st_a, _)), None) => {
            if a_now == st_a {
                P1::DeleteA
            } else {
                P1::Conflict(
                    conflict(
                        name,
                        &rt.dst,
                        key,
                        "deleted on the target side while the source row changed",
                    )
                    .to_string(),
                )
            }
        }
        (Some((st_a, st_b)), Some(ybs)) => {
            let b_now = chain(name, &rt.dst, key, &ybs);
            match (a_now != st_a, b_now != st_b) {
                (false, false) => {
                    let diverged = if !verify_clean {
                        None
                    } else {
                        match forward_all(rt, key, xs) {
                            Ok(ys) if agree(&ys, &ybs) => None,
                            Ok(_) => Some(format!(
                                "`{}` row {key:?}: state records both sides clean, but \
                                 forward(source) != target — a standing divergence the \
                                 echo guard cannot see",
                                rt.dst
                            )),
                            Err(e) => Some(format!(
                                "`{}` row {key:?}: state records both sides clean, but the \
                                 pair's forward refuses the source value ({e})",
                                rt.dst
                            )),
                        }
                    };
                    P1::Clean { diverged }
                }
                (true, false) => match forward_all(rt, key, xs) {
                    Ok(ys) => P1::AToB { ys, a_hash: a_now },
                    Err(e) => P1::Conflict(e.to_string()),
                },
                (false, true) => match inverse_all(rt, key, xs, &ybs) {
                    Ok(xs2) => P1::BToA { xs2, b_hash: b_now },
                    Err(e) => P1::Conflict(e.to_string()),
                },
                (true, true) => P1::Conflict(
                    conflict(name, &rt.dst, key, "both sides changed since the last sync")
                        .to_string(),
                ),
            }
        }
    })
}

/// What a target-side row with NO source row (pass 2) means.
pub(crate) enum P2 {
    /// Gone from A while B stood still: delete B.
    DeleteB,
    /// New in B: the creation path, already checked against §4's rules.
    CreateA { xs2: Vec<Value>, b_hash: String },
    Conflict(String),
}

pub(crate) fn classify_p2(
    rt: &ResolvedTable,
    name: &str,
    key: &Value,
    ybs: &[Value],
    st: Option<(String, String)>,
) -> Result<P2> {
    let b_now = chain(name, &rt.dst, key, ybs);
    if let Some((_, st_b)) = st {
        return Ok(if b_now == st_b {
            P2::DeleteB
        } else {
            P2::Conflict(
                conflict(
                    name,
                    &rt.dst,
                    key,
                    "deleted on the source side while the target row changed",
                )
                .to_string(),
            )
        });
    }
    // The creation path: only when the identity is real (not a
    // source-assigned rowid) and every column inverts without a residual.
    if rt.rowid_identity {
        return Ok(P2::Conflict(format!(
            "map sync `{name}`: row {key:?} was created on `{}` but the source identity is \
             `{}`'s hidden rowid, which only the source can assign — create the row on the \
             source side",
            rt.dst, rt.src
        )));
    }
    if let Some(c) = rt
        .cols
        .iter()
        .find(|c| c.lens.as_ref().is_some_and(|l| l.class != LensClass::Bijective))
    {
        return Ok(P2::Conflict(format!(
            "map sync `{name}`: row {key:?} was created on `{}`, but column `{}` maps through \
             a non-bijective pair — there is no residual to attach (§4's creation path); \
             create the row on `{}` instead",
            rt.dst, c.dst, rt.src
        )));
    }
    let xs2 = rt
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
                        "map sync `{name}`: created row {key:?} on `{}` has `{}` = {y:?}, \
                         outside its pair's image",
                        rt.dst, c.dst
                    )));
                }
                Ok(x)
            }
        })
        .collect::<Result<Vec<_>>>();
    Ok(match xs2 {
        Ok(xs2) => P2::CreateA { xs2, b_hash: b_now },
        Err(e) => P2::Conflict(e.to_string()),
    })
}

/// The per-table SQL a map's passes need, built once.
pub(crate) struct MapSql {
    pub(crate) src_exists: String,
    pub(crate) dst_exists: String,
    pub(crate) src_get: String,
    pub(crate) dst_get: String,
    pub(crate) src_ins: String,
    pub(crate) dst_ins: String,
    pub(crate) src_upd: String,
    pub(crate) dst_upd: String,
    pub(crate) src_del: String,
    pub(crate) dst_del: String,
    pub(crate) p1_first: String,
    pub(crate) p1_next: String,
    pub(crate) p2_first: String,
    pub(crate) p2_next: String,
    pub(crate) p3_first: String,
    pub(crate) p3_next: String,
}

pub(crate) const STATE_GET: &str = "SELECT a_hash, b_hash FROM rretl_map_state \
                                    WHERE map = $1 AND tbl = $2 AND pk_enc = $3";
pub(crate) const STATE_PUT: &str =
    "INSERT INTO rretl_map_state (map, tbl, pk_enc, k, a_hash, b_hash, ts_micros) \
     VALUES ($1, $2, $3, $4, $5, $6, $7)";
pub(crate) const STATE_SET: &str =
    "UPDATE rretl_map_state SET a_hash = $4, b_hash = $5, ts_micros = $6 \
     WHERE map = $1 AND tbl = $2 AND pk_enc = $3";
pub(crate) const STATE_DEL: &str =
    "DELETE FROM rretl_map_state WHERE map = $1 AND tbl = $2 AND pk_enc = $3";

impl MapSql {
    pub(crate) fn new(rt: &ResolvedTable, chunk: usize) -> MapSql {
        let src_cols = quoted(&rt.cols.iter().map(|c| c.src.clone()).collect::<Vec<_>>());
        let dst_cols = quoted(&rt.cols.iter().map(|c| c.dst.clone()).collect::<Vec<_>>());
        let (sk, dk, src, dst) = (&rt.src_key, &rt.dst_key, &rt.src, &rt.dst);
        let sets = |cols: &[ResolvedCol], pick: fn(&ResolvedCol) -> &String| {
            cols.iter()
                .enumerate()
                .map(|(i, c)| format!("\"{}\" = ${}", pick(c), i + 2))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let holes = |n: usize| (0..n).map(|i| format!(", ${}", i + 2)).collect::<String>();
        MapSql {
            src_exists: format!("SELECT \"{sk}\" FROM \"{src}\" WHERE \"{sk}\" = $1"),
            dst_exists: format!("SELECT \"{dk}\" FROM \"{dst}\" WHERE \"{dk}\" = $1"),
            src_get: format!("SELECT {src_cols} FROM \"{src}\" WHERE \"{sk}\" = $1"),
            dst_get: format!("SELECT {dst_cols} FROM \"{dst}\" WHERE \"{dk}\" = $1"),
            src_ins: format!(
                "INSERT INTO \"{src}\" (\"{sk}\", {src_cols}) VALUES ($1{})",
                holes(rt.cols.len())
            ),
            dst_ins: format!(
                "INSERT INTO \"{dst}\" (\"{dk}\", {dst_cols}) VALUES ($1{})",
                holes(rt.cols.len())
            ),
            src_upd: format!(
                "UPDATE \"{src}\" SET {} WHERE \"{sk}\" = $1",
                sets(&rt.cols, |c| &c.src)
            ),
            dst_upd: format!(
                "UPDATE \"{dst}\" SET {} WHERE \"{dk}\" = $1",
                sets(&rt.cols, |c| &c.dst)
            ),
            src_del: format!("DELETE FROM \"{src}\" WHERE \"{sk}\" = $1"),
            dst_del: format!("DELETE FROM \"{dst}\" WHERE \"{dk}\" = $1"),
            p1_first: format!(
                "SELECT \"{sk}\", {src_cols} FROM \"{src}\" ORDER BY \"{sk}\" LIMIT {chunk}"
            ),
            p1_next: format!(
                "SELECT \"{sk}\", {src_cols} FROM \"{src}\" WHERE \"{sk}\" > $1 \
                 ORDER BY \"{sk}\" LIMIT {chunk}"
            ),
            p2_first: format!(
                "SELECT \"{dk}\", {dst_cols} FROM \"{dst}\" ORDER BY \"{dk}\" LIMIT {chunk}"
            ),
            p2_next: format!(
                "SELECT \"{dk}\", {dst_cols} FROM \"{dst}\" WHERE \"{dk}\" > $1 \
                 ORDER BY \"{dk}\" LIMIT {chunk}"
            ),
            p3_first: format!(
                "SELECT pk_enc, k FROM rretl_map_state WHERE map = $1 AND tbl = $2 \
                 ORDER BY pk_enc LIMIT {chunk}"
            ),
            p3_next: format!(
                "SELECT pk_enc, k FROM rretl_map_state WHERE map = $1 AND tbl = $2 \
                 AND pk_enc > $3 ORDER BY pk_enc LIMIT {chunk}"
            ),
        }
    }
}

fn sync_table(
    s: &mut WriteSession<'_>,
    name: &str,
    rt: &ResolvedTable,
    report: &mut MapSyncReport,
) -> Result<()> {
    let chunk = chunk_rows();
    let sql = MapSql::new(rt, chunk);
    let mut w = MapWriter::new(name, rt, &sql);
    let mut last: Option<Value> = None;

    // ---- pass 1: every source row -----------------------------------------
    loop {
        let rows = match &last {
            None => rows_of(s.query(&sql.p1_first, &[])?)?,
            Some(k) => rows_of(s.query(&sql.p1_next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let (key, xs) = (&row[0], &row[1..]);
            let st = w.state_of(s, key)?;
            let b = w.target_row(s, key)?;
            match classify_p1(rt, name, key, xs, st, b, false)? {
                P1::Conflict(msg) => return Err(Error::Unsupported(msg)),
                action => w.apply_p1(s, key, action, report)?,
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    // ---- pass 2: target rows with no source row ---------------------------
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&sql.p2_first, &[])?)?,
            Some(k) => rows_of(s.query(&sql.p2_next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let (key, ybs) = (&row[0], &row[1..]);
            if !rows_of(s.query(&sql.src_exists, std::slice::from_ref(key))?)?.is_empty() {
                continue; // handled in pass 1
            }
            let st = w.state_of(s, key)?;
            match classify_p2(rt, name, key, ybs, st)? {
                P2::Conflict(msg) => return Err(Error::Unsupported(msg)),
                action => w.apply_p2(s, key, action, report)?,
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
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(s.query(&sql.p3_first, &w.map_tbl())?)?,
            Some(k) => {
                let mut p = w.map_tbl();
                p.push(k.clone());
                rows_of(s.query(&sql.p3_next, &p)?)?
            }
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            w.sweep_state_row(s, &row[0], &row[1])?;
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    Ok(())
}

/// Carries out what [`classify_p1`]/[`classify_p2`] decided. Every write
/// path lives here once, so sync and the daemon differ only in their
/// chunking and their answer to a conflict — never in what a push does.
pub(crate) struct MapWriter<'a> {
    name: &'a str,
    rt: &'a ResolvedTable,
    sql: &'a MapSql,
}

impl<'a> MapWriter<'a> {
    pub(crate) fn new(name: &'a str, rt: &'a ResolvedTable, sql: &'a MapSql) -> MapWriter<'a> {
        MapWriter { name, rt, sql }
    }

    pub(crate) fn map_tbl(&self) -> Vec<Value> {
        vec![
            Value::Text(self.name.into()),
            Value::Text(self.rt.dst.clone()),
        ]
    }

    fn state_key(&self, key: &Value) -> Vec<Value> {
        let mut v = self.map_tbl();
        v.push(Value::Blob(crate::rretl::pk_ref(key)));
        v
    }

    pub(crate) fn state_of(
        &self,
        s: &mut WriteSession<'_>,
        key: &Value,
    ) -> Result<Option<(String, String)>> {
        let st = rows_of(s.query(STATE_GET, &self.state_key(key))?)?;
        Ok(st
            .first()
            .map(|r| (crate::rretl::as_text(&r[0]), crate::rretl::as_text(&r[1]))))
    }

    pub(crate) fn target_row(
        &self,
        s: &mut WriteSession<'_>,
        key: &Value,
    ) -> Result<Option<Vec<Value>>> {
        Ok(rows_of(s.query(&self.sql.dst_get, std::slice::from_ref(key))?)?
            .into_iter()
            .next())
    }

    /// Re-read a just-written row and verify its chain: the state hash is
    /// the FUTURE oracle for this row, so it records what is PERSISTED,
    /// never what was intended (the stage-3 finding-14 discipline).
    fn verify_persisted(
        &self,
        s: &mut WriteSession<'_>,
        get: &str,
        key: &Value,
        want: &str,
        side: &str,
    ) -> Result<()> {
        let rows = rows_of(s.query(get, std::slice::from_ref(key))?)?;
        let got = rows
            .first()
            .map(|r| chain(self.name, &self.rt.dst, key, r))
            .ok_or_else(|| Error::Corrupt(format!("map sync: {side} row {key:?} vanished")))?;
        if got != want {
            return Err(Error::Corrupt(format!(
                "map sync: persisted {side} row {key:?} does not hash to what was written"
            )));
        }
        Ok(())
    }

    fn params(&self, key: &Value, rest: &[Value]) -> Vec<Value> {
        let mut v = vec![key.clone()];
        v.extend(rest.iter().cloned());
        v
    }

    fn put_state(
        &self,
        s: &mut WriteSession<'_>,
        key: &Value,
        a: String,
        b: String,
    ) -> Result<()> {
        let mut p = self.state_key(key);
        p.extend([
            key.clone(),
            Value::Text(a),
            Value::Text(b),
            Value::Int(now_micros()),
        ]);
        s.query(STATE_PUT, &p)?;
        Ok(())
    }

    fn set_state(
        &self,
        s: &mut WriteSession<'_>,
        key: &Value,
        a: String,
        b: String,
    ) -> Result<()> {
        let mut p = self.state_key(key);
        p.extend([Value::Text(a), Value::Text(b), Value::Int(now_micros())]);
        s.query(STATE_SET, &p)?;
        Ok(())
    }

    pub(crate) fn apply_p1(
        &mut self,
        s: &mut WriteSession<'_>,
        key: &Value,
        action: P1,
        report: &mut MapSyncReport,
    ) -> Result<()> {
        match action {
            P1::Conflict(msg) => return Err(Error::Unsupported(msg)),
            P1::Clean { .. } => report.unchanged += 1,
            P1::CreateB { ys, a_hash } => {
                s.query(&self.sql.dst_ins, &self.params(key, &ys))?;
                let b_now = chain(self.name, &self.rt.dst, key, &ys);
                self.verify_persisted(s, &self.sql.dst_get, key, &b_now, "target")?;
                self.put_state(s, key, a_hash, b_now)?;
                report.created_b += 1;
            }
            P1::AdoptB { a_hash, b_hash } => {
                self.put_state(s, key, a_hash, b_hash)?;
                report.unchanged += 1;
            }
            P1::DeleteA => {
                s.query(&self.sql.src_del, std::slice::from_ref(key))?;
                s.query(STATE_DEL, &self.state_key(key))?;
                report.deleted_a += 1;
            }
            P1::AToB { ys, a_hash } => {
                s.query(&self.sql.dst_upd, &self.params(key, &ys))?;
                let b_new = chain(self.name, &self.rt.dst, key, &ys);
                self.verify_persisted(s, &self.sql.dst_get, key, &b_new, "target")?;
                self.set_state(s, key, a_hash, b_new)?;
                report.a_to_b += 1;
            }
            P1::BToA { xs2, b_hash } => {
                s.query(&self.sql.src_upd, &self.params(key, &xs2))?;
                let a_new = chain(self.name, &self.rt.dst, key, &xs2);
                self.verify_persisted(s, &self.sql.src_get, key, &a_new, "source")?;
                self.set_state(s, key, a_new, b_hash)?;
                report.b_to_a += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn apply_p2(
        &mut self,
        s: &mut WriteSession<'_>,
        key: &Value,
        action: P2,
        report: &mut MapSyncReport,
    ) -> Result<()> {
        match action {
            P2::Conflict(msg) => return Err(Error::Unsupported(msg)),
            P2::DeleteB => {
                s.query(&self.sql.dst_del, std::slice::from_ref(key))?;
                s.query(STATE_DEL, &self.state_key(key))?;
                report.deleted_b += 1;
            }
            P2::CreateA { xs2, b_hash } => {
                s.query(&self.sql.src_ins, &self.params(key, &xs2))?;
                let a_now = chain(self.name, &self.rt.dst, key, &xs2);
                self.verify_persisted(s, &self.sql.src_get, key, &a_now, "source")?;
                self.put_state(s, key, a_now, b_hash)?;
                report.created_a += 1;
            }
        }
        Ok(())
    }

    /// Pass 3: a state row whose key is gone from BOTH sides is cleared.
    /// An unverified `k` is left alone — see [`state_row_is_consistent`].
    pub(crate) fn sweep_state_row(
        &self,
        s: &mut WriteSession<'_>,
        pk_enc: &Value,
        key: &Value,
    ) -> Result<()> {
        if !state_row_is_consistent(pk_enc, key) {
            return Ok(());
        }
        let in_src = !rows_of(s.query(&self.sql.src_exists, std::slice::from_ref(key))?)?.is_empty();
        let in_dst = !rows_of(s.query(&self.sql.dst_exists, std::slice::from_ref(key))?)?.is_empty();
        if !in_src && !in_dst {
            let mut p = self.map_tbl();
            p.push(pk_enc.clone());
            s.query(STATE_DEL, &p)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ check

/// The read-only twin of [`sync_table`]: the SAME (state, target-row)
/// classification, but nothing is written, no error aborts the walk, and
/// the both-clean arm — which the sync skips untouched by design (that IS
/// the echo guard) — is actually verified: `forward(source)` must equal
/// the target. Any edit here must be mirrored in `sync_table`, and vice
/// versa; the two matches are the same contract read-only vs read-write.
fn check_table(
    db: &crate::Database,
    name: &str,
    rt: &ResolvedTable,
    has_state: bool,
    has_dst: bool,
) -> Result<MapCheckTable> {
    let chunk = chunk_rows();
    let sql = MapSql::new(rt, chunk);
    let mut out = MapCheckTable {
        src: rt.src.clone(),
        dst: rt.dst.clone(),
        ..Default::default()
    };
    let map_tbl = || vec![Value::Text(name.into()), Value::Text(rt.dst.clone())];

    if !has_dst && has_state {
        let n = rows_of(db.query(
            "SELECT count(*) FROM rretl_map_state WHERE map = $1 AND tbl = $2",
            &map_tbl(),
        )?)?;
        if crate::rretl::as_int(&n[0][0])? > 0 {
            out.conflicts.push(format!(
                "map sync `{name}`: the target table `{}` is GONE but the map has sync \
                 state for it — dropped or renamed away; a sync refuses rather than \
                 read it as a target-side delete of every row",
                rt.dst
            ));
            return Ok(out);
        }
    }

    let state_of = |key: &Value| -> Result<Option<(String, String)>> {
        if !has_state {
            return Ok(None);
        }
        let mut p = map_tbl();
        p.push(Value::Blob(crate::rretl::pk_ref(key)));
        let st = rows_of(db.query(STATE_GET, &p)?)?;
        Ok(st
            .first()
            .map(|r| (crate::rretl::as_text(&r[0]), crate::rretl::as_text(&r[1]))))
    };

    // ---- pass 1: every source row -----------------------------------------
    let mut last: Option<Value> = None;
    loop {
        let rows = match &last {
            None => rows_of(db.query(&sql.p1_first, &[])?)?,
            Some(k) => rows_of(db.query(&sql.p1_next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for row in &rows {
            let (key, xs) = (&row[0], &row[1..]);
            let b = if has_dst {
                rows_of(db.query(&sql.dst_get, std::slice::from_ref(key))?)?
                    .into_iter()
                    .next()
            } else {
                None
            };
            // `verify_clean` is what makes this a CHECK and not a rehearsal:
            // the both-clean arm, which a sync skips untouched by design, is
            // the one the audit actually looks at.
            match classify_p1(rt, name, key, xs, state_of(key)?, b, true)? {
                P1::Clean { diverged: None } => out.unchanged += 1,
                P1::Clean { diverged: Some(d) } => out.diverged.push(d),
                P1::CreateB { .. } => out.would_create_b += 1,
                P1::AdoptB { .. } => out.would_adopt += 1,
                P1::DeleteA => out.would_delete_a += 1,
                P1::AToB { .. } => out.pending_a2b += 1,
                P1::BToA { .. } => out.pending_b2a += 1,
                P1::Conflict(c) => out.conflicts.push(c),
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }

    // ---- pass 2: target rows with no source row ---------------------------
    if has_dst {
        let mut last: Option<Value> = None;
        loop {
            let rows = match &last {
                None => rows_of(db.query(&sql.p2_first, &[])?)?,
                Some(k) => rows_of(db.query(&sql.p2_next, std::slice::from_ref(k))?)?,
            };
            let got = rows.len();
            if got == 0 {
                break;
            }
            for row in &rows {
                let (key, ybs) = (&row[0], &row[1..]);
                if !rows_of(db.query(&sql.src_exists, std::slice::from_ref(key))?)?.is_empty() {
                    continue; // classified in pass 1
                }
                match classify_p2(rt, name, key, ybs, state_of(key)?)? {
                    P2::DeleteB => out.would_delete_b += 1,
                    P2::CreateA { .. } => out.would_create_a += 1,
                    P2::Conflict(c) => out.conflicts.push(c),
                }
            }
            last = Some(rows[got - 1][0].clone());
            if got < chunk {
                break;
            }
        }
    }

    // ---- pass 3: state rows with NEITHER side left ------------------------
    if has_state {
        let mut last: Option<Value> = None;
        loop {
            let rows = match &last {
                None => rows_of(db.query(&sql.p3_first, &map_tbl())?)?,
                Some(k) => {
                    let mut p = map_tbl();
                    p.push(k.clone());
                    rows_of(db.query(&sql.p3_next, &p)?)?
                }
            };
            let got = rows.len();
            if got == 0 {
                break;
            }
            for row in &rows {
                let key = &row[1];
                if !state_row_is_consistent(&row[0], key) {
                    out.diverged.push(format!(
                        "`{}`: a sync-state row's key value {key:?} does not match its \
                         recorded key reference — tampered or corrupt; it identifies no \
                         row and no sync will ever clean it",
                        rt.dst
                    ));
                    continue;
                }
                let in_src =
                    !rows_of(db.query(&sql.src_exists, std::slice::from_ref(key))?)?.is_empty();
                let in_dst = has_dst
                    && !rows_of(db.query(&sql.dst_exists, std::slice::from_ref(key))?)?.is_empty();
                if !in_src && !in_dst {
                    out.orphan_state += 1;
                }
            }
            last = Some(rows[got - 1][0].clone());
            if got < chunk {
                break;
            }
        }
    }

    Ok(out)
}

/// The map half of `rretl fsck` (verify-at-rest): every stored map must
/// still parse, resolve and load its pairs, and no synced row may stand
/// DIVERGED — state clean on both sides while `forward(source) != target`,
/// the breach the echo guard structurally cannot see. Pending work,
/// conflicts and orphaned state rows are user state, not integrity
/// findings; they belong to [`Database::rretl_map_check`], not to fsck.
pub(crate) fn fsck_maps(db: &crate::Database, findings: &mut Vec<String>) -> Result<()> {
    let names = db.rretl_maps()?;
    let have = db.committed_tables()?;
    let has_state = have.iter().any(|(n, _)| n == T_MAP_STATE);
    let mut known: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for name in names {
        let spec = match db.load_map(&name) {
            Ok(s) => s,
            Err(e) => {
                findings.push(format!("map `{name}`: its stored record cannot be read ({e})"));
                continue;
            }
        };
        let resolved = match db.resolve_map(&spec) {
            Ok(r) => r,
            Err(e) => {
                findings.push(format!(
                    "map `{name}`: no longer resolves ({e}) — syncing it is impossible \
                     until this is fixed"
                ));
                continue;
            }
        };
        for rt in &resolved {
            let has_dst = have.iter().any(|(n, _)| n == &rt.dst);
            // One unexecutable map must not take the whole fsck down with
            // it: before this, a query-time error here replaced the entire
            // finding list — residual tampering, archive corruption, other
            // maps — with one exception naming nothing.
            match check_table(db, &name, rt, has_state, has_dst) {
                Ok(t) => {
                    for d in t.diverged {
                        findings.push(format!("map `{name}`: {d}"));
                    }
                    for c in t.conflicts {
                        if c.contains("is GONE") {
                            findings.push(format!("map `{name}`: {c}"));
                        }
                    }
                }
                Err(e) => findings.push(format!(
                    "map `{name}`: `{}` → `{}` cannot be checked ({e})",
                    rt.src, rt.dst
                )),
            }
        }
        for rt in &resolved {
            known.insert((name.clone(), rt.dst.clone()));
        }
    }
    // State rows for a (map, target) no defined map claims are scanned by
    // nothing and cleaned by nothing — invisible forever without this.
    if has_state {
        let rows = rows_of(db.query(
            "SELECT DISTINCT map, tbl FROM rretl_map_state ORDER BY map, tbl",
            &[],
        )?)?;
        for r in rows {
            let pair = (crate::rretl::as_text(&r[0]), crate::rretl::as_text(&r[1]));
            if !known.contains(&pair) {
                findings.push(format!(
                    "sync state for map `{}` target `{}` belongs to no defined mapping — \
                     left over from a dropped or redefined map; no sync will clean it",
                    pair.0, pair.1
                ));
            }
        }
    }
    Ok(())
}
