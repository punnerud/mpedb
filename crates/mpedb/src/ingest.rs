//! Ingest (design/DESIGN-INGEST.md): pull an external system in, catch
//! every change, call as little as possible. This module owns the
//! DECLARATION — the call graph, the budget vector, the policy — and the
//! bookkeeping tables the receipt path and the planner both write.
//!
//! The receipt protocol (dump/delta, diff-apply, cursor verification) is
//! `ingest_run.rs`; the planner is `ingest_plan.rs`.
//!
//! **mpedb never calls out.** A source declares what calls EXIST and what
//! they cost; the user's code makes them and hands the rows back. Nothing
//! here opens a socket.
//!
//! The shape that makes this a planning problem rather than a schedule:
//! a source is a call GRAPH. A root call ("what changed?") returns keys
//! that drive derived calls, whose fan-out is therefore data-dependent and
//! must be OBSERVED, never declared.

use mpedb_types::{ColumnType, Error, Result, Value};

use crate::rretl::{create_bookkeeping, rows_of, shape_gate, spec_col};
use crate::WriteSession;

/// Sys-keyspace namespace for source declarations: `ingest/<name>` → one
/// version byte + canonical TOML. Bounded (one record per source), so the
/// sys keyspace is right; the OBSERVATIONS are unbounded logs and live in
/// ordinary tables (#124).
pub const NS_INGEST: &str = "ingest";
const RECORD_V1: u8 = 1;

pub const T_STATS: &str = "ingest_stats";
pub const T_STATE: &str = "ingest_state";
pub const T_CONFLICTS: &str = "ingest_conflicts";
pub const T_SEEN: &str = "ingest_seen";

const STATS_SHAPE: [&str; 21] = [
    "run_id", "source", "edge", "tbl", "mode", "ts_micros", "rows_in", "inserted",
    "updated", "deleted", "unchanged", "conflicts", "calls", "bytes", "changed",
    "caught", "missed", "watermark", "verdict", "state", "note",
];
const STATE_SHAPE: [&str; 13] = [
    "source", "edge", "fingerprint", "watermark", "cursor_col", "cursor_state",
    "caught", "missed", "fanout", "receipts", "changed_receipts", "parent_calls",
    "ts_micros",
];
const CONFLICTS_SHAPE: [&str; 6] = ["source", "tbl", "pk_ref", "k", "kind", "detail"];
const SEEN_SHAPE: [&str; 4] = ["source", "tbl", "pk_ref", "run_id"];

/// Every table ingest owns — refused as a source table or an rRETL target.
pub(crate) fn ingest_bookkeeping_names() -> [&'static str; 5] {
    [
        T_STATS,
        T_STATE,
        T_CONFLICTS,
        T_SEEN,
        crate::ingest_task::T_TASK,
    ]
}

// ------------------------------------------------------------------ spec

/// What a call is FOR. `Root` calls are scheduled; `Derived` and
/// `Writeback` calls are driven by a parent's keys, so their rate is the
/// parent's rate times an OBSERVED fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Root,
    Derived,
    Writeback,
}

impl EdgeKind {
    fn parse(s: &str) -> Result<EdgeKind> {
        Ok(match s {
            "root" => EdgeKind::Root,
            "derived" => EdgeKind::Derived,
            "writeback" => EdgeKind::Writeback,
            other => {
                return Err(Error::Unsupported(format!(
                    "ingest: `{other}` is not an edge kind (root, derived, writeback)"
                )))
            }
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Root => "root",
            EdgeKind::Derived => "derived",
            EdgeKind::Writeback => "writeback",
        }
    }
}

/// How an edge gets its rows. See DESIGN-INGEST §4 for what each one
/// costs and — more importantly — what each one MISSES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Every row of the table. The only channel through which deletes are
    /// visible, and the only thing that re-verifies a cursor.
    Dump,
    /// Rows changed since the watermark, with overlap. Cannot see deletes.
    Delta,
    /// A cheap "anything changed?" call gating an expensive fetch.
    ProbeFetch,
    /// The source pushes. Requires a `Dump` edge to reconcile drops.
    Webhook,
    /// Paginated full read resumable by page token. Requires a STABLE sort
    /// in the endpoint, or resumption silently drops or duplicates rows.
    PageCursor,
}

impl Strategy {
    fn parse(s: &str) -> Result<Strategy> {
        Ok(match s {
            "dump" => Strategy::Dump,
            "delta" => Strategy::Delta,
            "probe_fetch" => Strategy::ProbeFetch,
            "webhook" => Strategy::Webhook,
            "page_cursor" => Strategy::PageCursor,
            other => {
                return Err(Error::Unsupported(format!(
                    "ingest: `{other}` is not a strategy (dump, delta, probe_fetch, \
                     webhook, page_cursor)"
                )))
            }
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Dump => "dump",
            Strategy::Delta => "delta",
            Strategy::ProbeFetch => "probe_fetch",
            Strategy::Webhook => "webhook",
            Strategy::PageCursor => "page_cursor",
        }
    }
    /// Does a receipt of this strategy present the WHOLE table? Only then
    /// can absence be read as deletion.
    pub fn is_complete(self) -> bool {
        matches!(self, Strategy::Dump | Strategy::PageCursor)
    }
}

/// Who wins when the external row and the local row differ. `Newest` is
/// deliberately absent: it depends on a clock nobody here controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The external system wins — the local row is overwritten.
    Source,
    /// The local row stands, and the difference is RECORDED as a conflict.
    Local,
}

impl Policy {
    fn parse(s: &str) -> Result<Policy> {
        Ok(match s {
            "source" => Policy::Source,
            "local" => Policy::Local,
            other => {
                return Err(Error::Unsupported(format!(
                    "ingest: `{other}` is not a policy (source, local). `newest` is \
                     deliberately not offered — it depends on a clock you do not control"
                )))
            }
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Source => "source",
            Policy::Local => "local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub name: String,
    pub kind: EdgeKind,
    /// For derived/writeback: whose keys drive this call.
    pub parent: Option<String>,
    pub table: String,
    pub strategy: Strategy,
    /// A cursor CANDIDATE. Verified against every dump, never trusted.
    pub cursor: Option<String>,
    pub overlap_secs: i64,
    /// How many parent keys fit in one call. 1 = no batching.
    pub batch: i64,
    pub cost_calls: i64,
    pub cost_bytes: i64,
    /// Importance μ in the allocation. Default 1.
    pub weight: i64,
}

impl EdgeSpec {
    /// Identity for the observation record's staleness guard: change any
    /// of these and past observations describe a different call, so they
    /// must decode to "never observed" rather than to a lie (the
    /// `stats.rs` discipline).
    /// Does a receipt through this edge present the WHOLE table? Only then
    /// can absence be read as deletion. A DERIVED edge never does, whatever
    /// its strategy says about the per-key call: it is scoped to the keys
    /// that drove it, and the rows it was not asked about are not gone
    /// (DESIGN-INGEST §2).
    pub fn presents_whole_table(&self) -> bool {
        self.kind == EdgeKind::Root && self.strategy.is_complete()
    }

    pub fn fingerprint(&self) -> String {
        let mut h = blake3::Hasher::new();
        for part in [
            self.name.as_str(),
            self.kind.as_str(),
            &self.table,
            self.strategy.as_str(),
            self.cursor.as_deref().unwrap_or(""),
            self.parent.as_deref().unwrap_or(""),
        ] {
            h.update(part.as_bytes());
            h.update(&[0]);
        }
        h.update(&self.batch.to_le_bytes());
        h.finalize().to_hex().to_string()
    }
}

#[derive(Debug, Clone)]
pub struct BudgetSpec {
    /// Which time profile this budget applies in.
    pub profile: String,
    pub window_secs: i64,
    pub calls: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone)]
pub struct IngestSpec {
    pub name: String,
    pub policy: Policy,
    pub budget: Vec<BudgetSpec>,
    pub edges: Vec<EdgeSpec>,
    /// Local hours [from, to) counted as the `work` profile.
    pub work_from: i64,
    pub work_to: i64,
}

/// Identifier check for every NAME the spec contributes to formatted SQL.
fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn tstr(v: &toml::Value, what: &str) -> Result<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Unsupported(format!("ingest spec: {what} must be a string")))
}

fn tint(v: &toml::Value, what: &str) -> Result<i64> {
    v.as_integer()
        .ok_or_else(|| Error::Unsupported(format!("ingest spec: {what} must be an integer")))
}

impl IngestSpec {
    pub fn from_toml_str(text: &str) -> Result<IngestSpec> {
        /// A key nobody reads is a setting the operator thinks is in force.
        /// Rows are already refused this way (§3); the declaration is where
        /// it matters more, because a dropped `work_from` is silent forever.
        fn only(v: &toml::Value, allowed: &[&str], what: &str) -> Result<()> {
            let Some(t) = v.as_table() else { return Ok(()) };
            for k in t.keys() {
                if !allowed.contains(&k.as_str()) {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: `{k}` is not a known {what} key. Known: {}",
                        allowed.join(", ")
                    )));
                }
            }
            Ok(())
        }

        let doc: toml::Value = text
            .parse()
            .map_err(|e| Error::Unsupported(format!("ingest spec parse error: {e}")))?;
        let src = doc
            .get("source")
            .ok_or_else(|| Error::Unsupported("ingest spec: missing [source]".into()))?;
        let name = tstr(
            src.get("name")
                .ok_or_else(|| Error::Unsupported("ingest spec: missing source.name".into()))?,
            "source.name",
        )?;
        let policy = match src.get("policy") {
            Some(v) => Policy::parse(&tstr(v, "source.policy")?)?,
            None => Policy::Source,
        };
        only(src, &["name", "policy", "work_from", "work_to", "budget", "edge"], "source")?;
        let work_from = src.get("work_from").map(|v| tint(v, "work_from")).transpose()?.unwrap_or(8);
        let work_to = src.get("work_to").map(|v| tint(v, "work_to")).transpose()?.unwrap_or(17);

        let mut budget = Vec::new();
        if let Some(arr) = src.get("budget").and_then(|b| b.as_array()) {
            for b in arr {
                only(b, &["profile", "window_secs", "calls", "bytes"], "budget")?;
                budget.push(BudgetSpec {
                    profile: b
                        .get("profile")
                        .map(|v| tstr(v, "budget.profile"))
                        .transpose()?
                        .unwrap_or_else(|| "work".into()),
                    window_secs: b
                        .get("window_secs")
                        .map(|v| tint(v, "budget.window_secs"))
                        .transpose()?
                        .unwrap_or(3600),
                    calls: b.get("calls").map(|v| tint(v, "budget.calls")).transpose()?.unwrap_or(0),
                    bytes: b.get("bytes").map(|v| tint(v, "budget.bytes")).transpose()?.unwrap_or(0),
                });
            }
        }

        let raw = src
            .get("edge")
            .and_then(|e| e.as_array())
            .ok_or_else(|| Error::Unsupported("ingest spec: at least one [[source.edge]]".into()))?;
        let mut edges = Vec::with_capacity(raw.len());
        for e in raw {
            only(
                e,
                &[
                    "name", "kind", "table", "strategy", "parent", "cursor",
                    "overlap_secs", "batch", "cost_calls", "cost_bytes", "weight",
                ],
                "edge",
            )?;
            let ename = tstr(
                e.get("name")
                    .ok_or_else(|| Error::Unsupported("ingest spec: edge without `name`".into()))?,
                "edge.name",
            )?;
            let kind = match e.get("kind") {
                Some(v) => EdgeKind::parse(&tstr(v, "edge.kind")?)?,
                None => EdgeKind::Root,
            };
            edges.push(EdgeSpec {
                table: tstr(
                    e.get("table").ok_or_else(|| {
                        Error::Unsupported(format!("ingest spec: edge `{ename}` without `table`"))
                    })?,
                    "edge.table",
                )?,
                strategy: match e.get("strategy") {
                    Some(v) => Strategy::parse(&tstr(v, "edge.strategy")?)?,
                    None => Strategy::Delta,
                },
                parent: e.get("parent").map(|v| tstr(v, "edge.parent")).transpose()?,
                cursor: e.get("cursor").map(|v| tstr(v, "edge.cursor")).transpose()?,
                overlap_secs: e
                    .get("overlap_secs")
                    .map(|v| tint(v, "edge.overlap_secs"))
                    .transpose()?
                    .unwrap_or(0),
                batch: e.get("batch").map(|v| tint(v, "edge.batch")).transpose()?.unwrap_or(1),
                cost_calls: e
                    .get("cost_calls")
                    .map(|v| tint(v, "edge.cost_calls"))
                    .transpose()?
                    .unwrap_or(1),
                cost_bytes: e
                    .get("cost_bytes")
                    .map(|v| tint(v, "edge.cost_bytes"))
                    .transpose()?
                    .unwrap_or(0),
                weight: e.get("weight").map(|v| tint(v, "edge.weight")).transpose()?.unwrap_or(1),
                name: ename,
                kind,
            });
        }
        let spec = IngestSpec { name, policy, budget, edges, work_from, work_to };
        spec.validate()?;
        Ok(spec)
    }

    /// Canonical TOML — what BOTH doors store, so a dict-built source and a
    /// TOML-built source are the same record. Safe to emit with plain
    /// quoting: `validate` gates every name through `ident_ok`.
    pub fn to_toml(&self) -> String {
        let mut out = format!(
            "[source]\nname = \"{}\"\npolicy = \"{}\"\nwork_from = {}\nwork_to = {}\n",
            self.name,
            self.policy.as_str(),
            self.work_from,
            self.work_to
        );
        for b in &self.budget {
            out.push_str(&format!(
                "\n[[source.budget]]\nprofile = \"{}\"\nwindow_secs = {}\ncalls = {}\nbytes = {}\n",
                b.profile, b.window_secs, b.calls, b.bytes
            ));
        }
        for e in &self.edges {
            out.push_str(&format!(
                "\n[[source.edge]]\nname = \"{}\"\nkind = \"{}\"\ntable = \"{}\"\n\
                 strategy = \"{}\"\noverlap_secs = {}\nbatch = {}\ncost_calls = {}\n\
                 cost_bytes = {}\nweight = {}\n",
                e.name,
                e.kind.as_str(),
                e.table,
                e.strategy.as_str(),
                e.overlap_secs,
                e.batch,
                e.cost_calls,
                e.cost_bytes,
                e.weight
            ));
            if let Some(p) = &e.parent {
                out.push_str(&format!("parent = \"{p}\"\n"));
            }
            if let Some(c) = &e.cursor {
                out.push_str(&format!("cursor = \"{c}\"\n"));
            }
        }
        out
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !ident_ok(&self.name) {
            return Err(Error::Unsupported(format!(
                "ingest spec: `{}` is not a legal source name",
                self.name
            )));
        }
        if self.edges.is_empty() {
            return Err(Error::Unsupported(
                "ingest spec: at least one edge is required".into(),
            ));
        }
        if !(0..=23).contains(&self.work_from) || !(1..=24).contains(&self.work_to) {
            return Err(Error::Unsupported(format!(
                "ingest spec: work hours {}..{} are not a legal range",
                self.work_from, self.work_to
            )));
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in &self.edges {
            for (n, what) in [(&e.name, "edge name"), (&e.table, "table name")] {
                if !ident_ok(n) {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: `{n}` is not a legal {what}"
                    )));
                }
            }
            if let Some(c) = &e.cursor {
                if !ident_ok(c) {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: `{c}` is not a legal cursor column name"
                    )));
                }
            }
            if ingest_bookkeeping_names().iter().any(|b| e.table.eq_ignore_ascii_case(b))
                || crate::rretl::rretl_bookkeeping_names()
                    .iter()
                    .any(|b| e.table.eq_ignore_ascii_case(b))
            {
                return Err(Error::Unsupported(format!(
                    "ingest spec: `{}` is bookkeeping; ingesting into it is refused",
                    e.table
                )));
            }
            if !seen.insert(e.name.to_ascii_lowercase()) {
                return Err(Error::Unsupported(format!(
                    "ingest spec: edge `{}` appears twice",
                    e.name
                )));
            }
            if e.batch < 1 {
                return Err(Error::Unsupported(format!(
                    "ingest spec: edge `{}` has batch {} — must be at least 1",
                    e.name, e.batch
                )));
            }
            if e.weight < 1 {
                return Err(Error::Unsupported(format!(
                    "ingest spec: edge `{}` has weight {} — must be at least 1",
                    e.name, e.weight
                )));
            }
            // Only a SCHEDULED edge needs a cursor. A derived or writeback
            // edge does not ask "what changed since X" — it asks for the keys
            // its parent handed it, so a cursor would be a field nobody reads.
            if e.kind == EdgeKind::Root && e.strategy == Strategy::Delta && e.cursor.is_none() {
                return Err(Error::Unsupported(format!(
                    "ingest spec: root edge `{}` is a delta but declares no cursor candidate — \
                     a delta without a cursor cannot say what it asked for",
                    e.name
                )));
            }
            match (e.kind, &e.parent) {
                (EdgeKind::Root, Some(p)) => {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: root edge `{}` declares parent `{p}` — a root is \
                         scheduled, not driven",
                        e.name
                    )))
                }
                (EdgeKind::Derived | EdgeKind::Writeback, None) => {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: `{}` edge `{}` has no parent — its rate IS the \
                         parent's rate times the observed fan-out, so it needs one",
                        e.kind.as_str(),
                        e.name
                    )))
                }
                _ => {}
            }
        }
        // Parents must exist, and the graph must be acyclic — a cycle would
        // make fan-out unbounded and the cost infinite.
        for e in &self.edges {
            if let Some(p) = &e.parent {
                if !self.edges.iter().any(|o| o.name.eq_ignore_ascii_case(p)) {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: edge `{}` names parent `{p}`, which is not an edge",
                        e.name
                    )));
                }
            }
        }
        for e in &self.edges {
            let mut seen_path = vec![e.name.to_ascii_lowercase()];
            let mut cur = e;
            while let Some(p) = &cur.parent {
                let Some(next) = self.edges.iter().find(|o| o.name.eq_ignore_ascii_case(p)) else {
                    break;
                };
                if !seen_path.insert_unique(next.name.to_ascii_lowercase()) {
                    return Err(Error::Unsupported(format!(
                        "ingest spec: the parent chain through `{}` is a CYCLE — fan-out \
                         would be unbounded",
                        next.name
                    )));
                }
                cur = next;
            }
        }
        // §4: deletes are visible through nothing else, and nothing else
        // re-verifies a cursor. A table pulled only by delta is a table
        // whose deletes never arrive.
        // The requirement is per TABLE, not per edge: whatever pulls it, a
        // table no edge presents WHOLE has deletes nobody can see and a
        // cursor nobody re-verifies. Stating it per edge missed the case
        // where the only edge on a table is a derived one (which is scoped,
        // so it never presents the whole thing however its strategy reads).
        for e in &self.edges {
            let reconciled = self
                .edges
                .iter()
                .any(|o| o.table.eq_ignore_ascii_case(&e.table) && o.presents_whole_table());
            if !reconciled {
                return Err(Error::Unsupported(format!(
                    "ingest spec: nothing presents the whole of `{}` — declare a root dump \
                     edge for it. `{}` is a {} {} edge, and deletes are visible through \
                     nothing but a dump, which is also the only thing that re-verifies a \
                     cursor (DESIGN-INGEST §4)",
                    e.table,
                    e.name,
                    e.kind.as_str(),
                    e.strategy.as_str()
                )));
            }
        }
        Ok(())
    }

    pub fn edge(&self, name: &str) -> Option<&EdgeSpec> {
        self.edges.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// The budget in force for a given local hour.
    pub fn budget_for(&self, hour: i64) -> Option<&BudgetSpec> {
        let want = if hour >= self.work_from && hour < self.work_to { "work" } else { "off" };
        self.budget
            .iter()
            .find(|b| b.profile == want)
            .or_else(|| self.budget.first())
    }
}

/// Tiny helper so the cycle walk reads as intent rather than as index
/// bookkeeping.
trait InsertUnique {
    fn insert_unique(&mut self, v: String) -> bool;
}
impl InsertUnique for Vec<String> {
    fn insert_unique(&mut self, v: String) -> bool {
        if self.contains(&v) {
            return false;
        }
        self.push(v);
        true
    }
}

// ------------------------------------------------------------- tables

pub(crate) fn ensure_ingest_tables(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use ColumnType::{Any, Blob, Int64, Text};
    if !shape_gate(have, T_STATS, &STATS_SHAPE)? {
        create_bookkeeping(
            s,
            T_STATS,
            vec![
                spec_col("run_id", Int64),
                spec_col("source", Text),
                spec_col("edge", Text),
                spec_col("tbl", Text),
                spec_col("mode", Text),
                spec_col("ts_micros", Int64),
                spec_col("rows_in", Int64),
                spec_col("inserted", Int64),
                spec_col("updated", Int64),
                spec_col("deleted", Int64),
                spec_col("unchanged", Int64),
                spec_col("conflicts", Int64),
                spec_col("calls", Int64),
                spec_col("bytes", Int64),
                spec_col("changed", Int64),
                // The cursor trial's running tally, and the high-water mark
                // this receipt saw. `watermark` is ANY so a cursor keeps its
                // TYPE across a receipt — a timestamp that came back as text
                // would sort differently from the integer it started as.
                spec_col("caught", Int64),
                spec_col("missed", Int64),
                spec_col("watermark", Any),
                spec_col("verdict", Text),
                spec_col("state", Text),
                spec_col("note", Text),
            ],
            &["run_id"],
        )?;
    }
    if !shape_gate(have, T_STATE, &STATE_SHAPE)? {
        create_bookkeeping(
            s,
            T_STATE,
            vec![
                spec_col("source", Text),
                spec_col("edge", Text),
                // The staleness guard: blake3 over the EDGE IDENTITY, so a
                // redefined edge's observations decode to "never observed"
                // instead of to a lie (the stats.rs discipline).
                spec_col("fingerprint", Text),
                spec_col("watermark", Any),
                spec_col("cursor_col", Text),
                spec_col("cursor_state", Text),
                spec_col("caught", Int64),
                spec_col("missed", Int64),
                spec_col("fanout", Int64),
                spec_col("receipts", Int64),
                spec_col("changed_receipts", Int64),
                spec_col("parent_calls", Int64),
                spec_col("ts_micros", Int64),
            ],
            &["source", "edge"],
        )?;
    }
    if !shape_gate(have, T_CONFLICTS, &CONFLICTS_SHAPE)? {
        create_bookkeeping(
            s,
            T_CONFLICTS,
            vec![
                spec_col("source", Text),
                spec_col("tbl", Text),
                spec_col("pk_ref", Blob),
                spec_col("k", Any),
                spec_col("kind", Text),
                spec_col("detail", Text),
            ],
            &["source", "tbl", "pk_ref"],
        )?;
    }
    if !shape_gate(have, T_SEEN, &SEEN_SHAPE)? {
        create_bookkeeping(
            s,
            T_SEEN,
            vec![
                spec_col("source", Text),
                spec_col("tbl", Text),
                spec_col("pk_ref", Blob),
                spec_col("run_id", Int64),
            ],
            &["source", "tbl", "pk_ref"],
        )?;
    }
    Ok(())
}

// ------------------------------------------------------- declaration API

impl crate::Database {
    /// Store (or replace) a source declaration. Validated NOW — tables
    /// exist with a usable row identity, parents resolve, the graph is
    /// acyclic, every delta has a reconciling dump — so a receipt never
    /// discovers a broken declaration mid-run.
    pub fn ingest_define(&self, toml_text: &str) -> Result<()> {
        let spec = IngestSpec::from_toml_str(toml_text)?;
        self.resolve_ingest(&spec)?;
        self.store_ingest_record(&spec.name, toml_text)
    }

    /// [`ingest_define`](Self::ingest_define) from a CONSTRUCTED spec —
    /// the programmatic door (Python hands a dict). Same validation, and
    /// the record is the canonical TOML either way.
    pub fn ingest_define_spec(&self, spec: &IngestSpec) -> Result<()> {
        spec.validate()?;
        self.resolve_ingest(spec)?;
        self.store_ingest_record(&spec.name, &spec.to_toml())
    }

    fn store_ingest_record(&self, name: &str, toml_text: &str) -> Result<()> {
        let mut record = vec![RECORD_V1];
        record.extend_from_slice(toml_text.as_bytes());
        let have = self.committed_tables()?;
        let has_state = have.iter().any(|(n, _)| n == T_STATE);
        let mut s = self.begin()?;
        let res = (|| -> Result<()> {
            ensure_ingest_tables(&mut s, &have)?;
            // A CHANGED declaration re-baselines: observations recorded for
            // the old edge set describe different calls. Per-edge
            // fingerprints already make a stale row decode to "never
            // observed", so this is belt AND braces — but it also clears
            // watermarks, which fingerprints do not, and a watermark from a
            // different cursor is the one thing that could silently skip
            // rows.
            let prior = s.sys_record_get(NS_INGEST, name.as_bytes())?;
            if has_state && prior.is_some() && prior.as_deref() != Some(record.as_slice()) {
                s.query(
                    "DELETE FROM ingest_state WHERE source = $1",
                    &[Value::Text(name.into())],
                )?;
            }
            s.sys_record_put(NS_INGEST, name.as_bytes(), &record)?;
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

    /// The stored declaration, verbatim.
    pub fn ingest_show(&self, name: &str) -> Result<String> {
        let key = crate::sys_record_subkey(NS_INGEST, name.as_bytes())?;
        let r = self.engine.begin_read()?;
        let rec = r.sys_get(&key)?;
        r.finish()?;
        let rec = rec.ok_or_else(|| Error::Unsupported(format!("no ingest source `{name}`")))?;
        match rec.first() {
            Some(&RECORD_V1) => String::from_utf8(rec[1..].to_vec())
                .map_err(|_| Error::Corrupt(format!("ingest source `{name}`: not UTF-8"))),
            Some(v) => Err(Error::Unsupported(format!(
                "ingest source `{name}` uses record version {v}; this build reads {RECORD_V1}"
            ))),
            None => Err(Error::Corrupt(format!("ingest source `{name}`: empty record"))),
        }
    }

    pub fn ingest_sources(&self) -> Result<Vec<String>> {
        // Namespace framing is `ns ‖ NUL ‖ key`; [ns‖0, ns‖1) is exactly it.
        let mut lo = NS_INGEST.as_bytes().to_vec();
        lo.push(0);
        let mut hi = NS_INGEST.as_bytes().to_vec();
        hi.push(1);
        let r = self.engine.begin_read()?;
        let keys = r.sys_scan_range_keys(&lo, &hi)?;
        r.finish()?;
        Ok(keys
            .into_iter()
            .filter_map(|k| String::from_utf8(k[lo.len()..].to_vec()).ok())
            .collect())
    }

    /// Drop a source AND its observations, in one transaction. State that
    /// outlives its source is state nothing scans and nothing re-baselines.
    pub fn ingest_drop(&self, name: &str) -> Result<bool> {
        let have = self.committed_tables()?;
        let has_state = have.iter().any(|(n, _)| n == T_STATE);
        let mut s = self.begin()?;
        let existed = match s.sys_record_get(NS_INGEST, name.as_bytes()) {
            Ok(v) => v.is_some(),
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        };
        let res = (|| -> Result<()> {
            if has_state {
                let p = [Value::Text(name.into())];
                s.query("DELETE FROM ingest_state WHERE source = $1", &p)?;
                s.query("DELETE FROM ingest_seen WHERE source = $1", &p)?;
            }
            s.sys_record_delete(NS_INGEST, name.as_bytes())?;
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

    pub(crate) fn load_ingest(&self, name: &str) -> Result<IngestSpec> {
        let text = self.ingest_show(name)?;
        let spec = IngestSpec::from_toml_str(&text)?;
        if spec.name != name {
            return Err(Error::Corrupt(format!(
                "ingest record `{name}` contains a source named `{}`",
                spec.name
            )));
        }
        Ok(spec)
    }

    /// Resolve every edge against the LIVE schema: the table exists, has a
    /// single row identity, and the cursor candidate is one of its columns.
    pub(crate) fn resolve_ingest(&self, spec: &IngestSpec) -> Result<Vec<ResolvedEdge>> {
        self.engine.refresh_schema_if_stale()?;
        let bundle = self.engine.schema();
        let mut out = Vec::with_capacity(spec.edges.len());
        for e in &spec.edges {
            let t = bundle
                .schema
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&e.table) && !t.dead)
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "ingest `{}`: edge `{}` fills `{}`, which is not a table",
                        spec.name, e.name, e.table
                    ))
                })?;
            if t.primary_key.len() != 1 {
                return Err(Error::Unsupported(format!(
                    "ingest `{}`: `{}` has no single row identity (one-column PK or implicit \
                     rowid) — composite PKs are refused, because the external id is what makes \
                     a re-read harmless",
                    spec.name, t.name
                )));
            }
            let pk = &t.columns[t.primary_key[0] as usize];
            let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            if let Some(c) = &e.cursor {
                if !cols.iter().any(|n| n.eq_ignore_ascii_case(c)) {
                    return Err(Error::Unsupported(format!(
                        "ingest `{}`: edge `{}` names cursor `{c}`, which is not a column of \
                         `{}`",
                        spec.name, e.name, t.name
                    )));
                }
            }
            out.push(ResolvedEdge {
                spec: e.clone(),
                table: t.name.clone(),
                pk_col: pk.name.clone(),
                cols,
            });
        }
        Ok(out)
    }
}

/// An edge checked against the live schema, carrying the schema's own
/// spelling of every name (SQL identifiers are case-insensitive, so the
/// declaration's spelling must not leak into interpolated SQL).
#[derive(Debug, Clone)]
pub struct ResolvedEdge {
    pub spec: EdgeSpec,
    pub table: String,
    pub pk_col: String,
    pub cols: Vec<String>,
}

impl ResolvedEdge {
    /// The cursor candidate, in the schema's spelling.
    pub fn cursor_col(&self) -> Option<String> {
        let want = self.spec.cursor.as_deref()?;
        self.cols.iter().find(|c| c.eq_ignore_ascii_case(want)).cloned()
    }
}

/// One edge's observed model, as `ingest_state` records it.
#[derive(Debug, Clone)]
pub struct EdgeState {
    /// When this edge last reported a receipt. A cron-style fetcher has no
    /// memory of its own; without this it cannot tell whether it is due.
    pub last_micros: i64,
    pub watermark: Value,
    pub cursor_col: String,
    pub cursor_state: String,
    pub caught: i64,
    pub missed: i64,
    pub fanout: i64,
    pub receipts: i64,
    pub changed_receipts: i64,
    pub parent_calls: i64,
}

impl Default for EdgeState {
    fn default() -> EdgeState {
        EdgeState {
            last_micros: 0,
            watermark: Value::Null,
            cursor_col: String::new(),
            cursor_state: "unknown".into(),
            caught: 0,
            missed: 0,
            fanout: 0,
            receipts: 0,
            changed_receipts: 0,
            parent_calls: 0,
        }
    }
}

impl EdgeState {
    /// The change-rate estimate, per receipt, by the LLN closed form
    /// `Δ̂ = p·Î/(k + α − Î)`.
    ///
    /// NOT the naive `Î/k`, which is biased AND inconsistent: it converges
    /// to pΔ/(Δ+p), saturating at the poll rate itself, so it structurally
    /// cannot see a table that changed ten times between two receipts.
    /// α = 1 keeps the estimate large-but-finite when every receipt found a
    /// change (DESIGN-INGEST §7.3).
    ///
    /// Returned per receipt; the planner multiplies by the actual rate.
    pub fn lambda_per_poll(&self) -> f64 {
        if self.receipts <= 0 {
            return 0.0;
        }
        let k = self.receipts as f64;
        let hit = self.changed_receipts as f64;
        let denom = k + 1.0 - hit;
        if denom <= 0.0 {
            return k; // every receipt changed and then some: cap at k, finite
        }
        hit / denom
    }

    /// Observed fan-out: keys per parent call. 0 = never observed, which
    /// the planner prices as its floor rather than as "free".
    pub fn fanout_per_call(&self) -> f64 {
        if self.parent_calls <= 0 {
            return 0.0;
        }
        self.fanout as f64 / self.parent_calls as f64
    }
}

const STATE_GET: &str =
    "SELECT fingerprint, watermark, cursor_col, cursor_state, caught, missed, fanout, \
     receipts, changed_receipts, parent_calls, ts_micros FROM ingest_state \
     WHERE source = $1 AND edge = $2";

/// Decode a state row, or "never observed". **Fails SOFT on a stale
/// fingerprint**: a redefined edge's observations describe a different
/// call, and pricing them as unobserved is exactly right. An observation
/// read must never fail a run (the `stats.rs` discipline).
fn decode_state(rows: &[Vec<Value>], fingerprint: &str) -> Result<EdgeState> {
    let Some(r) = rows.first() else {
        return Ok(EdgeState::default());
    };
    if crate::rretl::as_text(&r[0]) != fingerprint {
        return Ok(EdgeState::default());
    }
    Ok(EdgeState {
        watermark: r[1].clone(),
        cursor_col: crate::rretl::as_text(&r[2]),
        cursor_state: crate::rretl::as_text(&r[3]),
        caught: crate::rretl::as_int(&r[4])?,
        missed: crate::rretl::as_int(&r[5])?,
        fanout: crate::rretl::as_int(&r[6])?,
        receipts: crate::rretl::as_int(&r[7])?,
        changed_receipts: crate::rretl::as_int(&r[8])?,
        parent_calls: crate::rretl::as_int(&r[9])?,
        last_micros: r.get(10).and_then(|v| crate::rretl::as_int(v).ok()).unwrap_or(0),
    })
}

pub(crate) fn read_state(
    db: &crate::Database,
    source: &str,
    edge: &str,
    fingerprint: &str,
) -> Result<EdgeState> {
    let have = db.committed_tables()?;
    if !have.iter().any(|(n, _)| n == T_STATE) {
        return Ok(EdgeState::default());
    }
    let rows = rows_of(db.query(
        STATE_GET,
        &[Value::Text(source.into()), Value::Text(edge.into())],
    )?)?;
    decode_state(&rows, fingerprint)
}

/// The same read from INSIDE a write session — the receipt path needs its
/// own uncommitted writes to be visible, and cannot lend out the database.
pub(crate) fn read_state_in(
    s: &mut WriteSession<'_>,
    source: &str,
    edge: &str,
    fingerprint: &str,
) -> Result<EdgeState> {
    let rows = rows_of(s.query(
        STATE_GET,
        &[Value::Text(source.into()), Value::Text(edge.into())],
    )?)?;
    decode_state(&rows, fingerprint)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_state(
    s: &mut WriteSession<'_>,
    source: &str,
    edge: &str,
    fingerprint: &str,
    st: &EdgeState,
) -> Result<()> {
    let p = [
        Value::Text(source.into()),
        Value::Text(edge.into()),
        Value::Text(fingerprint.into()),
        st.watermark.clone(),
        Value::Text(st.cursor_col.clone()),
        Value::Text(st.cursor_state.clone()),
        Value::Int(st.caught),
        Value::Int(st.missed),
        Value::Int(st.fanout),
        Value::Int(st.receipts),
        Value::Int(st.changed_receipts),
        Value::Int(st.parent_calls),
        Value::Int(crate::rretl::now_micros()),
    ];
    let hit = matches!(
        s.query(
            "UPDATE ingest_state SET fingerprint = $3, watermark = $4, cursor_col = $5, \
             cursor_state = $6, caught = $7, missed = $8, fanout = $9, receipts = $10, \
             changed_receipts = $11, parent_calls = $12, ts_micros = $13 \
             WHERE source = $1 AND edge = $2",
            &p,
        )?,
        crate::ExecResult::Affected(n) if n > 0
    );
    if !hit {
        s.query(
            "INSERT INTO ingest_state (source, edge, fingerprint, watermark, cursor_col, \
             cursor_state, caught, missed, fanout, receipts, changed_receipts, parent_calls, \
             ts_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            &p,
        )?;
    }
    Ok(())
}
