//! Shared TOML configuration: every process opens the same config file and
//! derives from it both the runtime options and the schema (whose hash must
//! match the database it attaches to).

use crate::error::{Error, Result};
use std::collections::BTreeSet;
use crate::schema::{ColumnDef, DefaultExpr, Schema, TableDef};
use crate::value::{ColumnType, Value};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Never msync; crash-safe against process death, not against power loss
    /// or reboot. The right choice for /dev/shm.
    None,
    /// msync data and meta before a commit is acknowledged.
    Commit,
    /// **WAL with deferred (coalesced) fsync** — the "sqlite `synchronous=NORMAL`
    /// / PostgreSQL `synchronous_commit=off`" class (design/DESIGN.md §5.4.2). Every
    /// commit still APPENDS its record to `<path>-wal` and flips the meta, so
    /// the on-disk log is always a crash-consistent prefix; a background
    /// flusher issues `fdatasync` on a bounded interval rather than per commit.
    /// **Weaker than `commit`/`wal`: a commit is acknowledged BEFORE it is
    /// power-loss-durable, so a power failure may lose a bounded recent window
    /// of commits — but never yields a torn/partial database.** NOT
    /// durable-on-ack.
    Async,
    /// Write-ahead log: every commit appends one sequential record to
    /// `<path>-wal` and issues a single fdatasync before it is acknowledged.
    /// Same durability guarantee as `commit`, much cheaper per commit
    /// (design/DESIGN.md §5.4).
    Wal,
}

impl Durability {
    /// Modes backed by the companion `<path>-wal` log (`wal` and `async`).
    /// They share the append/checkpoint/recovery machinery; they differ only
    /// in WHEN `fdatasync` runs (`wal`: per commit before ack; `async`:
    /// deferred/coalesced by a background flusher — design/DESIGN.md §5.4).
    pub fn uses_wal(self) -> bool {
        matches!(self, Durability::Wal | Durability::Async)
    }

    /// True iff a commit is power-loss-durable at the moment it is
    /// acknowledged (`commit` and `wal`). `none` and `async` acknowledge
    /// before power-loss durability (design/DESIGN.md §5.4).
    pub fn durable_on_ack(self) -> bool {
        matches!(self, Durability::Commit | Durability::Wal)
    }
}

/// Write-path concurrency discipline (design/DESIGN-PHASE3.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Concurrency {
    /// Today's shipped behavior: one writer at a time under the global writer
    /// lock (with the Phase-2 intent ring for group commit). Default.
    #[default]
    Serial,
    /// EXPERIMENTAL (default OFF): optimistic per-writer execution — a write
    /// prepares its row against a pinned snapshot off-lock, then takes a short
    /// critical section to validate its footprint (first-committer-wins,
    /// `Error::WriteConflict` on conflict) and blind-apply. Measured on this
    /// engine's COW B+tree and found NOT to beat the serial path; kept behind
    /// the flag for reproducibility. See design/DESIGN-PHASE3.md for the verdict.
    Optimistic,
}

/// GROUP BY column-strictness dialect (COMPAT.md). Governs whether a **bare**
/// column — one that is neither an aggregate nor a GROUP BY key — is accepted in
/// a grouped (or otherwise aggregated) SELECT.
///
/// The mode travels with the data's ORIGIN: a database imported from PostgreSQL
/// (`mirror import` from PG) is born [`Postgres`](BareGroupBy::Postgres); every
/// other database defaults to [`Sqlite`](BareGroupBy::Sqlite). It is a
/// per-process compilation option, like [`Durability`] — it decides what
/// `prepare` accepts, never what a stored plan means (a plan is self-describing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BareGroupBy {
    /// sqlite's rule: a bare column is accepted **only when its value is
    /// deterministic**, so the answer still matches sqlite exactly (mpedb's core
    /// guarantee — never a wrong answer). Two cases qualify: the column is
    /// provably never evaluated (a dead `COALESCE`/`CASE` branch that constant
    /// folding removes), or the query has exactly one `min()`/`max()` and no
    /// other aggregate (the bare column takes its value from the extremum's
    /// row). A genuinely arbitrary bare column (any other shape) is REFUSED with
    /// a clean bind error rather than guessed. The default.
    #[default]
    Sqlite,
    /// PostgreSQL / SQL-standard strictness: a bare column is ALWAYS an error
    /// (`must appear in GROUP BY …`). mpedb's original behavior; the mode a
    /// PostgreSQL-imported database is born with, so a query that PG refused
    /// keeps being refused here.
    Postgres,
}

impl BareGroupBy {
    /// The configured strictness as its config-string (`"sqlite"` / `"postgres"`).
    pub fn as_str(self) -> &'static str {
        match self {
            BareGroupBy::Sqlite => "sqlite",
            BareGroupBy::Postgres => "postgres",
        }
    }
}

/// Filesystem permissions applied to a freshly-created database file (and its
/// `<path>-wal` companion). This is the ONLY OS-enforced isolation boundary in
/// mpedb's serverless model (design/DESIGN-MULTIDB.md §1.4, §6): a process that cannot
/// `open()` the file touches zero bytes. Files are always *born* owner-only
/// (0o600) and then widened to `mode`; leaving `mode` unset keeps them 0o600.
#[derive(Debug, Clone, Default)]
pub struct FilePerms {
    /// Permission bits (<= 0o777) applied after born-restrictive creation.
    /// `None` ⇒ the file stays 0o600 (owner-only, the secure default).
    pub mode: Option<u32>,
    /// Owner to `chown` to — a username or a numeric uid string. Requires
    /// privilege; a configured owner that cannot be applied is a hard error.
    pub owner: Option<String>,
    /// Group to `chown` to — a group name or a numeric gid string.
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DbOptions {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub max_readers: u32,
    pub durability: Durability,
    pub concurrency: Concurrency,
    pub perms: FilePerms,
    /// Extent threshold in BYTES; `None` = the extent path is off
    /// (DESIGN-BLOBEXTENT §8). Per-process like `durability` — the on-disk
    /// format self-describes (`vkind=2` cells), so processes with different
    /// thresholds only differ in what NEW writes do.
    pub extent_threshold: Option<usize>,
    /// Per-statement-execution runtime budget in "work rows" (#74,
    /// design/DESIGN-RUNTIME-BUDGET.md): rows yielded by scans, nested-loop join
    /// candidates, and correlated-subquery re-evaluations. `0` = unlimited. A
    /// per-process execution option like `durability`, NOT a file-frozen
    /// property, so it lives here rather than in the schema. Absent in config ⇒
    /// [`DEFAULT_MAX_WORK_ROWS`].
    pub max_work_rows: u64,
    /// Names of tables declared `require_policy = true` (DESIGN-MULTIDB §6.3).
    /// A prepare touching one of these fails closed unless RLS is enabled AND a
    /// policy governs the command being compiled — the answer to "one forgotten
    /// `ENABLE ROW LEVEL SECURITY` silently exposes every row".
    ///
    /// This is a **per-process deployment assertion, not a file-wide guarantee**:
    /// it lives in config (like `durability`), so a process that does not declare
    /// it is not bound by it. That is consistent with cooperative RLS — any
    /// attached process can read raw pages anyway (§6 Honesty Box) — and it
    /// catches the mistake it is aimed at: the developer's own forgotten DDL, in
    /// their own build, at prepare time.
    pub require_policy: BTreeSet<String>,
    /// GROUP BY column-strictness dialect ([`BareGroupBy`], COMPAT.md). Set from
    /// `[compat] bare_group_by` (default [`BareGroupBy::Sqlite`]); a PostgreSQL
    /// `mirror import` overrides it to [`BareGroupBy::Postgres`] so the strictness
    /// travels with the data's origin. A per-process compilation option like
    /// `durability`, so it lives here rather than in the file-frozen schema.
    pub bare_group_by: BareGroupBy,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub options: DbOptions,
    pub schema: Schema,
}

/// The deterministic per-statement-execution work budget default (#74). One
/// billion work-rows is far above any legitimate query on an embedded database,
/// yet a genuine runaway (an accidental cross join, an unbounded correlated
/// subquery) crosses it long before it exhausts memory — a backstop, not a
/// quota. `0` in config means unlimited; the finite default is what makes a
/// runaway caught-by-default (see design/DESIGN-RUNTIME-BUDGET.md).
pub const DEFAULT_MAX_WORK_ROWS: u64 = 1_000_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    database: RawDatabase,
    #[serde(default, rename = "table")]
    tables: Vec<RawTable>,
    /// Optional `[runtime]` section (#74). Applies to this single database.
    #[serde(default)]
    runtime: Option<RawRuntime>,
    /// Optional `[compat]` section (COMPAT.md). Applies to this single database.
    #[serde(default)]
    compat: Option<RawCompat>,
}

/// The `[runtime]` TOML section (#74): per-process execution limits.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    /// Deterministic work-row budget per statement execution; `0` = unlimited.
    /// Absent ⇒ [`DEFAULT_MAX_WORK_ROWS`].
    #[serde(default)]
    max_work_rows: Option<u64>,
}

impl RawRuntime {
    fn resolve(this: Option<&RawRuntime>) -> u64 {
        this.and_then(|r| r.max_work_rows)
            .unwrap_or(DEFAULT_MAX_WORK_ROWS)
    }
}

/// The `[compat]` TOML section (COMPAT.md): per-process SQL-dialect toggles.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompat {
    /// `"sqlite"` (lenient bare columns, the default) or `"postgres"` (strict).
    #[serde(default)]
    bare_group_by: Option<String>,
}

impl RawCompat {
    fn resolve(this: Option<&RawCompat>) -> Result<BareGroupBy> {
        match this.and_then(|c| c.bare_group_by.as_deref()) {
            None | Some("sqlite") => Ok(BareGroupBy::Sqlite),
            Some("postgres") => Ok(BareGroupBy::Postgres),
            Some(other) => Err(Error::Config(format!(
                "compat.bare_group_by must be sqlite|postgres, got `{other}`"
            ))),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDatabase {
    path: String,
    #[serde(default = "default_size_mb")]
    size_mb: u64,
    #[serde(default = "default_max_readers")]
    max_readers: u32,
    #[serde(default)]
    durability: Option<String>,
    #[serde(default)]
    concurrency: Option<String>,
    /// File permission bits (e.g. `mode = 0o640`); TOML octal is supported.
    #[serde(default)]
    mode: Option<u32>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    group: Option<String>,
    /// Values whose encoded payload exceeds this many KiB take an extent run
    /// instead of an overflow chain (DESIGN-BLOBEXTENT §8). `0` = explicitly
    /// off. Absent = the PLATFORM default: 4 on Linux (coalesced
    /// pwrite made the 4 KiB cell win 1.7×), 32 on macOS (crossover ~16 KiB) (the sparse preallocation
    /// makes per-value pwrites lose at every measured size until the B4
    /// coalescing levers land).
    #[serde(default)]
    extent_threshold_kb: Option<u64>,
}

/// The measured per-platform default (DESIGN-BLOBEXTENT §8; blob_bulk_ab,
/// 2026-07-17): Linux crosses over at ~2 pages and is monotonic above —
/// 16 KiB is conservative-side of clear wins. macOS loses at every measured
/// size (sparse preallocation: each payload pwrite allocates APFS blocks),
/// so its default stays OFF until the per-commit pwritev coalescing and
/// F_PREALLOCATE levers land.
pub fn default_extent_threshold() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        // With the coalesced pwrite the 4 KiB cell wins 1.7× — the default
        // moves down to one page (values > 4 KiB), the benchmark cell's own
        // size. Below that, inline/overflow stays unmeasured and untouched.
        Some(4 * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        // M3, coalesced pwrite, paired: 4 KiB 0.85×, 16 KiB 1.09×, 32 KiB
        // 1.16×, 64 KiB 1.32×, 1 MiB 1.39× (12.3 GB/s). Crossover ~16 KiB;
        // 32 is the conservative side of it (the sparse-file allocation tax
        // is amortized but not gone — F_PREALLOCATE density is still queued).
        Some(32 * 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn default_size_mb() -> u64 {
    64
}
fn default_max_readers() -> u32 {
    1024
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    name: String,
    primary_key: Vec<String>,
    /// Deployment assertion (DESIGN-MULTIDB §6.3): this table is tenant-scoped
    /// and MUST be policy-protected — `prepare` fails closed if it is not.
    /// Deliberately collected into `DbOptions`, NOT into `TableDef`: `TableDef`
    /// feeds `Schema::canonical_bytes()` and thus the file-frozen `schema_hash`,
    /// so putting it there would make adding one assertion a flag-day that
    /// invalidates every existing file.
    #[serde(default)]
    require_policy: bool,
    #[serde(rename = "column")]
    columns: Vec<RawColumn>,
    /// Explicit (possibly composite) secondary indexes — `[[table.index]]`.
    /// Appended after the flag-derived single-column ones by `Schema::new`;
    /// declaration order is significant (it is the index numbering).
    #[serde(default, rename = "index")]
    indexes: Vec<RawIndex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    /// Column NAMES in key order.
    columns: Vec<String>,
    #[serde(default)]
    unique: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawColumn {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default = "default_true")]
    nullable: bool,
    #[serde(default)]
    unique: bool,
    /// A non-unique secondary index — a lookup index that allows duplicates.
    /// `unique = true` already builds an index (and enforces uniqueness); this
    /// builds one WITHOUT the uniqueness constraint, for `WHERE col = ?` and
    /// join lookups on a column that repeats.
    #[serde(default)]
    indexed: bool,
    #[serde(default)]
    default: Option<toml::Value>,
    #[serde(default)]
    check: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let max_work_rows = RawRuntime::resolve(raw.runtime.as_ref());
        let bare_group_by = RawCompat::resolve(raw.compat.as_ref())?;
        raw_to_config(raw.database, raw.tables, max_work_rows, bare_group_by)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)?;
        Config::from_toml_str(&text)
    }
}

/// Build a validated single-database `Config` from one `[database]` section and
/// its declared tables. Shared by the single-file path and each `Workspace`
/// member so validation is identical everywhere (design/DESIGN-MULTIDB.md §1.2).
fn raw_to_config(
    db: RawDatabase,
    raw_tables: Vec<RawTable>,
    max_work_rows: u64,
    bare_group_by: BareGroupBy,
) -> Result<Config> {
        if db.path.is_empty() {
            return Err(Error::Config("database.path must be set".into()));
        }
        if db.size_mb < 1 || db.size_mb > 1 << 20 {
            return Err(Error::Config("database.size_mb must be in 1..=1048576".into()));
        }
        if db.max_readers < 1 || db.max_readers > 65_536 {
            return Err(Error::Config("database.max_readers must be in 1..=65536".into()));
        }
        if let Some(m) = db.mode {
            if m > 0o777 {
                return Err(Error::Config(format!(
                    "database.mode must be permission bits <= 0o777, got 0o{m:o}"
                )));
            }
        }
        let durability = match db.durability.as_deref() {
            None | Some("none") => Durability::None,
            Some("commit") => Durability::Commit,
            Some("async") => Durability::Async,
            Some("wal") => Durability::Wal,
            Some(other) => {
                return Err(Error::Config(format!(
                    "durability must be none|commit|async|wal, got `{other}`"
                )))
            }
        };
        let concurrency = match db.concurrency.as_deref() {
            None | Some("serial") => Concurrency::Serial,
            Some("optimistic") => Concurrency::Optimistic,
            Some(other) => {
                return Err(Error::Config(format!(
                    "concurrency must be serial|optimistic, got `{other}`"
                )))
            }
        };

        let mut tables = Vec::with_capacity(raw_tables.len());
        let mut require_policy = BTreeSet::new();
        for t in raw_tables {
            let mut columns = Vec::with_capacity(t.columns.len());
            for c in &t.columns {
                let ty = ColumnType::parse(&c.ty).ok_or_else(|| {
                    Error::Config(format!("unknown type `{}` for {}.{}", c.ty, t.name, c.name))
                })?;
                let default = match &c.default {
                    None => None,
                    Some(v) => Some(parse_default(v, ty).map_err(|m| {
                        Error::Config(format!("bad default for {}.{}: {m}", t.name, c.name))
                    })?),
                };
                columns.push(ColumnDef {
                    name: c.name.clone(),
                    ty,
                    nullable: c.nullable,
                    unique: c.unique,
                    indexed: c.indexed,
                    default,
                    check: c.check.clone(),
                });
            }
            let primary_key = t
                .primary_key
                .iter()
                .map(|pk| {
                    columns
                        .iter()
                        .position(|c| &c.name == pk)
                        .map(|i| i as u16)
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "primary_key column `{pk}` not found in table `{}`",
                                t.name
                            ))
                        })
                })
                .collect::<Result<Vec<u16>>>()?;
            // PK columns are implicitly NOT NULL.
            for &i in &primary_key {
                columns[i as usize].nullable = false;
            }
            if t.require_policy {
                require_policy.insert(t.name.clone());
            }
            let indexes = t
                .indexes
                .iter()
                .map(|ix| {
                    let cols = ix
                        .columns
                        .iter()
                        .map(|name| {
                            columns
                                .iter()
                                .position(|c| &c.name == name)
                                .map(|i| i as u16)
                                .ok_or_else(|| {
                                    Error::Config(format!(
                                        "index column `{name}` not found in table `{}`",
                                        t.name
                                    ))
                                })
                        })
                        .collect::<Result<Vec<u16>>>()?;
                    Ok(crate::schema::IndexDef { columns: cols, unique: ix.unique })
                })
                .collect::<Result<Vec<_>>>()?;
            tables.push(TableDef {
                // Assigned by Schema::new (dense, name-sorted); the flags
                // above are the single-column index sugar it derives from,
                // and these explicit entries append after the derived ones.
                id: 0,
                name: t.name,
                columns,
                primary_key,
                indexes,
                dead: false,
                // Config-defined tables are always ordinary; FTS tables are
                // created live via `CREATE VIRTUAL TABLE` (design/DESIGN-FTS.md).
                kind: crate::schema::TableKind::Standard,
            });
        }

        Ok(Config {
            options: DbOptions {
                path: PathBuf::from(db.path),
                size_bytes: db.size_mb * 1024 * 1024,
                max_readers: db.max_readers,
                durability,
                concurrency,
                perms: FilePerms {
                    mode: db.mode,
                    owner: db.owner,
                    group: db.group,
                },
                extent_threshold: match db.extent_threshold_kb {
                    Some(0) => None,
                    Some(kb) => Some(kb as usize * 1024),
                    None => default_extent_threshold(),
                },
                max_work_rows,
                require_policy,
                bare_group_by,
            },
            schema: Schema::new(tables)?,
        })
}

/// One attached database inside a [`WorkspaceConfig`]: a routing `alias` and the
/// member's own fully-independent [`Config`] (own file, lock, reader table,
/// catalog — design/DESIGN-MULTIDB.md §1.1).
#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    pub alias: String,
    pub config: Config,
}

/// A set of independent databases addressed by alias (`alias.table`). Separate
/// files → separate writer locks → linear write parallelism, and the honest
/// hard-isolation boundary (design/DESIGN-MULTIDB.md §1). A plain single-`[database]`
/// config parses as a one-member workspace, so every existing config still
/// opens as a workspace with no change.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub members: Vec<WorkspaceMember>,
}

/// A `[[database]]` member in the multi-database TOML form: the single-database
/// `[database]` fields plus a required `alias` and its own nested `[[database.table]]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMember {
    alias: String,
    path: String,
    #[serde(default = "default_size_mb")]
    size_mb: u64,
    #[serde(default = "default_max_readers")]
    max_readers: u32,
    #[serde(default)]
    durability: Option<String>,
    #[serde(default)]
    concurrency: Option<String>,
    #[serde(default)]
    mode: Option<u32>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default, rename = "table")]
    tables: Vec<RawTable>,
}

impl RawMember {
    fn into_parts(self) -> (String, RawDatabase, Vec<RawTable>) {
        (
            self.alias,
            RawDatabase {
                path: self.path,
                size_mb: self.size_mb,
                max_readers: self.max_readers,
                durability: self.durability,
                concurrency: self.concurrency,
                mode: self.mode,
                owner: self.owner,
                group: self.group,
                extent_threshold_kb: None,
            },
            self.tables,
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    #[serde(rename = "database")]
    databases: Vec<RawMember>,
    /// Workspace-wide `[runtime]` section (#74): the same work budget applies to
    /// every member (a per-process execution option, not a per-file property).
    #[serde(default)]
    runtime: Option<RawRuntime>,
    /// Workspace-wide `[compat]` section (COMPAT.md): the same SQL-dialect
    /// strictness applies to every member (a per-process compilation option).
    #[serde(default)]
    compat: Option<RawCompat>,
}

impl WorkspaceConfig {
    /// Parse a workspace. Accepts BOTH forms:
    /// - a single `[database]` + top-level `[[table]]` (legacy; one member,
    ///   alias derived from the db file stem), and
    /// - a `[[database]]` array, each member carrying its own `[[database.table]]`.
    pub fn from_toml_str(text: &str) -> Result<WorkspaceConfig> {
        let val: toml::Value =
            toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        match val.get("database") {
            Some(toml::Value::Array(_)) => {
                let raw: RawWorkspace =
                    toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
                if raw.databases.is_empty() {
                    return Err(Error::Config(
                        "workspace must declare at least one [[database]] member".into(),
                    ));
                }
                let max_work_rows = RawRuntime::resolve(raw.runtime.as_ref());
                let bare_group_by = RawCompat::resolve(raw.compat.as_ref())?;
                let mut members = Vec::with_capacity(raw.databases.len());
                let mut seen_alias = std::collections::HashSet::new();
                let mut seen_path = std::collections::HashSet::new();
                for m in raw.databases {
                    let (alias, db, tables) = m.into_parts();
                    if alias.is_empty() {
                        return Err(Error::Config(
                            "each [[database]] must set a non-empty alias".into(),
                        ));
                    }
                    if alias.contains('.') {
                        return Err(Error::Config(format!(
                            "database alias `{alias}` must not contain '.'"
                        )));
                    }
                    if !seen_alias.insert(alias.clone()) {
                        return Err(Error::Config(format!("duplicate database alias `{alias}`")));
                    }
                    let config = raw_to_config(db, tables, max_work_rows, bare_group_by)?;
                    if !seen_path.insert(config.options.path.clone()) {
                        return Err(Error::Config(format!(
                            "two workspace members map to the same file `{}`",
                            config.options.path.display()
                        )));
                    }
                    members.push(WorkspaceMember { alias, config });
                }
                Ok(WorkspaceConfig { members })
            }
            Some(toml::Value::Table(_)) => {
                let config = Config::from_toml_str(text)?;
                let alias = default_member_alias(&config.options.path);
                Ok(WorkspaceConfig {
                    members: vec![WorkspaceMember { alias, config }],
                })
            }
            _ => Err(Error::Config(
                "config must contain a [database] table or a [[database]] array".into(),
            )),
        }
    }

    pub fn from_file(path: &std::path::Path) -> Result<WorkspaceConfig> {
        let text = std::fs::read_to_string(path)?;
        WorkspaceConfig::from_toml_str(&text)
    }

    /// Look up a member by alias.
    pub fn member(&self, alias: &str) -> Option<&WorkspaceMember> {
        self.members.iter().find(|m| m.alias == alias)
    }

    /// The default (unqualified) member: only defined when there is exactly one.
    pub fn default_alias(&self) -> Option<&str> {
        match self.members.as_slice() {
            [only] => Some(only.alias.as_str()),
            _ => None,
        }
    }
}

/// Derive a stable alias for a lone `[database]` config from its file stem
/// (e.g. `/var/lib/billing.mpedb` → `billing`), falling back to `main`.
fn default_member_alias(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && !s.contains('.'))
        .unwrap_or("main")
        .to_string()
}

fn parse_default(v: &toml::Value, ty: ColumnType) -> std::result::Result<DefaultExpr, String> {
    if let toml::Value::String(s) = v {
        if s == "now()" {
            return if ty == ColumnType::Timestamp {
                Ok(DefaultExpr::Now)
            } else {
                Err("now() only valid for timestamp columns".into())
            };
        }
    }
    let val = match (v, ty) {
        (toml::Value::Integer(x), ColumnType::Int64) => Value::Int(*x),
        (toml::Value::Integer(x), ColumnType::Float64) => Value::Float(*x as f64),
        (toml::Value::Integer(x), ColumnType::Timestamp) => Value::Timestamp(*x),
        (toml::Value::Float(x), ColumnType::Float64) => Value::Float(*x),
        (toml::Value::Boolean(x), ColumnType::Bool) => Value::Bool(*x),
        (toml::Value::String(s), ColumnType::Text) => Value::Text(s.clone()),
        _ => return Err(format!("cannot use `{v}` as default for {ty} column")),
    };
    Ok(DefaultExpr::Const(val))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[database]
path = "/dev/shm/test.mpedb"
size_mb = 16
durability = "none"

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "created"
  type = "timestamp"
  default = "now()"
"#;

    #[test]
    fn parses_sample() {
        let cfg = Config::from_toml_str(SAMPLE).unwrap();
        assert_eq!(cfg.options.size_bytes, 16 * 1024 * 1024);
        assert_eq!(cfg.options.max_readers, 1024);
        assert_eq!(cfg.options.durability, Durability::None);
        let t = &cfg.schema.tables[0];
        assert_eq!(t.name, "users");
        // PK column forced NOT NULL even though nullable defaulted to true.
        assert!(!t.columns[0].nullable);
        assert_eq!(t.columns[2].default, Some(DefaultExpr::Now));
    }

    #[test]
    fn rejects_unknown_fields_and_types() {
        assert!(Config::from_toml_str("[database]\npath='x'\nbogus=1").is_err());
        let bad = SAMPLE.replace("type = \"int64\"", "type = \"varchar\"");
        assert!(Config::from_toml_str(&bad).is_err());
    }

    #[test]
    fn single_database_is_one_member_workspace() {
        // Every legacy [database] config opens as a one-member workspace,
        // alias derived from the file stem.
        let ws = WorkspaceConfig::from_toml_str(SAMPLE).unwrap();
        assert_eq!(ws.members.len(), 1);
        assert_eq!(ws.members[0].alias, "test"); // /dev/shm/test.mpedb
        assert_eq!(ws.default_alias(), Some("test"));
        assert_eq!(ws.members[0].config.schema.tables[0].name, "users");
    }

    const WORKSPACE: &str = r#"
[[database]]
alias = "billing"
path = "/dev/shm/billing.mpedb"
size_mb = 8
durability = "wal"
mode = 0o640
  [[database.table]]
  name = "orders"
  primary_key = ["id"]
    [[database.table.column]]
    name = "id"
    type = "int64"

[[database]]
alias = "shared"
path = "/dev/shm/shared.mpedb"
  [[database.table]]
  name = "tenants"
  primary_key = ["id"]
    [[database.table.column]]
    name = "id"
    type = "int64"
"#;

    #[test]
    fn parses_multi_database_workspace() {
        let ws = WorkspaceConfig::from_toml_str(WORKSPACE).unwrap();
        assert_eq!(ws.members.len(), 2);
        assert_eq!(ws.default_alias(), None); // >1 member ⇒ no unqualified default
        let billing = ws.member("billing").unwrap();
        assert_eq!(billing.config.options.durability, Durability::Wal);
        assert_eq!(billing.config.options.perms.mode, Some(0o640));
        assert_eq!(billing.config.schema.tables[0].name, "orders");
        let shared = ws.member("shared").unwrap();
        assert_eq!(shared.config.schema.tables[0].name, "tenants");
    }

    #[test]
    fn workspace_rejects_dup_alias_dup_path_and_dotted_alias() {
        let dup_alias = WORKSPACE.replace("alias = \"shared\"", "alias = \"billing\"");
        assert!(matches!(
            WorkspaceConfig::from_toml_str(&dup_alias),
            Err(Error::Config(_))
        ));
        let dup_path =
            WORKSPACE.replace("path = \"/dev/shm/shared.mpedb\"", "path = \"/dev/shm/billing.mpedb\"");
        assert!(matches!(
            WorkspaceConfig::from_toml_str(&dup_path),
            Err(Error::Config(_))
        ));
        let dotted = WORKSPACE.replace("alias = \"billing\"", "alias = \"a.b\"");
        assert!(matches!(
            WorkspaceConfig::from_toml_str(&dotted),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn parses_file_perms() {
        // TOML octal literal for the mode; owner/group optional.
        let cfg = Config::from_toml_str(
            &SAMPLE.replace(
                "durability = \"none\"",
                "durability = \"none\"\nmode = 0o640\nowner = \"nobody\"\ngroup = \"42\"",
            ),
        )
        .unwrap();
        assert_eq!(cfg.options.perms.mode, Some(0o640));
        assert_eq!(cfg.options.perms.owner.as_deref(), Some("nobody"));
        assert_eq!(cfg.options.perms.group.as_deref(), Some("42"));
        // unset ⇒ defaults (born-restrictive 0o600 applied at create time)
        let plain = Config::from_toml_str(SAMPLE).unwrap();
        assert_eq!(plain.options.perms.mode, None);
        // out-of-range mode rejected
        let bad = SAMPLE.replace("durability = \"none\"", "durability = \"none\"\nmode = 0o4000");
        assert!(matches!(Config::from_toml_str(&bad), Err(Error::Config(_))));
    }

    #[test]
    fn parses_all_durability_modes() {
        for (text, want) in [
            ("none", Durability::None),
            ("commit", Durability::Commit),
            ("async", Durability::Async),
            ("wal", Durability::Wal),
        ] {
            let toml = SAMPLE.replace("durability = \"none\"", &format!("durability = \"{text}\""));
            assert_eq!(Config::from_toml_str(&toml).unwrap().options.durability, want);
        }
        let bad = SAMPLE.replace("durability = \"none\"", "durability = \"walrus\"");
        assert!(Config::from_toml_str(&bad).is_err());
    }

    #[test]
    fn parses_runtime_max_work_rows() {
        // absent [runtime] ⇒ the finite default (caught-by-default guard)
        assert_eq!(
            Config::from_toml_str(SAMPLE).unwrap().options.max_work_rows,
            DEFAULT_MAX_WORK_ROWS
        );
        // explicit value
        let cfg = Config::from_toml_str(&format!("{SAMPLE}\n[runtime]\nmax_work_rows = 42"))
            .unwrap();
        assert_eq!(cfg.options.max_work_rows, 42);
        // 0 = unlimited sentinel, preserved verbatim
        let cfg0 = Config::from_toml_str(&format!("{SAMPLE}\n[runtime]\nmax_work_rows = 0"))
            .unwrap();
        assert_eq!(cfg0.options.max_work_rows, 0);
        // unknown key in [runtime] is rejected (deny_unknown_fields)
        assert!(
            Config::from_toml_str(&format!("{SAMPLE}\n[runtime]\nmax_time_ms = 5")).is_err()
        );
    }

    #[test]
    fn parses_compat_bare_group_by() {
        // absent [compat] ⇒ the sqlite (lenient) default
        assert_eq!(
            Config::from_toml_str(SAMPLE).unwrap().options.bare_group_by,
            BareGroupBy::Sqlite
        );
        // explicit sqlite / postgres
        let cfg = Config::from_toml_str(&format!(
            "{SAMPLE}\n[compat]\nbare_group_by = \"postgres\""
        ))
        .unwrap();
        assert_eq!(cfg.options.bare_group_by, BareGroupBy::Postgres);
        let cfg = Config::from_toml_str(&format!(
            "{SAMPLE}\n[compat]\nbare_group_by = \"sqlite\""
        ))
        .unwrap();
        assert_eq!(cfg.options.bare_group_by, BareGroupBy::Sqlite);
        // unknown value rejected
        assert!(Config::from_toml_str(&format!(
            "{SAMPLE}\n[compat]\nbare_group_by = \"mysql\""
        ))
        .is_err());
        // unknown key in [compat] rejected (deny_unknown_fields)
        assert!(
            Config::from_toml_str(&format!("{SAMPLE}\n[compat]\nstrict = true")).is_err()
        );
    }

    #[test]
    fn parses_concurrency_modes() {
        // default (key absent) is serial
        assert_eq!(
            Config::from_toml_str(SAMPLE).unwrap().options.concurrency,
            Concurrency::Serial
        );
        for (text, want) in [
            ("serial", Concurrency::Serial),
            ("optimistic", Concurrency::Optimistic),
        ] {
            let toml = SAMPLE.replace(
                "durability = \"none\"",
                &format!("durability = \"none\"\nconcurrency = \"{text}\""),
            );
            assert_eq!(Config::from_toml_str(&toml).unwrap().options.concurrency, want);
        }
        let bad = SAMPLE.replace(
            "durability = \"none\"",
            "durability = \"none\"\nconcurrency = \"yolo\"",
        );
        assert!(Config::from_toml_str(&bad).is_err());
    }
}
