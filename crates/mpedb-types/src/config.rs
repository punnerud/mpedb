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
    /// / PostgreSQL `synchronous_commit=off`" class (DESIGN.md §5.4.2). Every
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
    /// (DESIGN.md §5.4).
    Wal,
}

impl Durability {
    /// Modes backed by the companion `<path>-wal` log (`wal` and `async`).
    /// They share the append/checkpoint/recovery machinery; they differ only
    /// in WHEN `fdatasync` runs (`wal`: per commit before ack; `async`:
    /// deferred/coalesced by a background flusher — DESIGN.md §5.4).
    pub fn uses_wal(self) -> bool {
        matches!(self, Durability::Wal | Durability::Async)
    }

    /// True iff a commit is power-loss-durable at the moment it is
    /// acknowledged (`commit` and `wal`). `none` and `async` acknowledge
    /// before power-loss durability (DESIGN.md §5.4).
    pub fn durable_on_ack(self) -> bool {
        matches!(self, Durability::Commit | Durability::Wal)
    }
}

/// Write-path concurrency discipline (DESIGN-PHASE3.md).
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
    /// the flag for reproducibility. See DESIGN-PHASE3.md for the verdict.
    Optimistic,
}

/// Filesystem permissions applied to a freshly-created database file (and its
/// `<path>-wal` companion). This is the ONLY OS-enforced isolation boundary in
/// mpedb's serverless model (DESIGN-MULTIDB.md §1.4, §6): a process that cannot
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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub options: DbOptions,
    pub schema: Schema,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    database: RawDatabase,
    #[serde(default, rename = "table")]
    tables: Vec<RawTable>,
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
        raw_to_config(raw.database, raw.tables)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)?;
        Config::from_toml_str(&text)
    }
}

/// Build a validated single-database `Config` from one `[database]` section and
/// its declared tables. Shared by the single-file path and each `Workspace`
/// member so validation is identical everywhere (DESIGN-MULTIDB.md §1.2).
fn raw_to_config(db: RawDatabase, raw_tables: Vec<RawTable>) -> Result<Config> {
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
            tables.push(TableDef {
                // Assigned by Schema::new (dense, name-sorted); the flags
                // above are the index sugar it derives from.
                id: 0,
                name: t.name,
                columns,
                primary_key,
                indexes: Vec::new(),
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
                require_policy,
            },
            schema: Schema::new(tables)?,
        })
}

/// One attached database inside a [`WorkspaceConfig`]: a routing `alias` and the
/// member's own fully-independent [`Config`] (own file, lock, reader table,
/// catalog — DESIGN-MULTIDB.md §1.1).
#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    pub alias: String,
    pub config: Config,
}

/// A set of independent databases addressed by alias (`alias.table`). Separate
/// files → separate writer locks → linear write parallelism, and the honest
/// hard-isolation boundary (DESIGN-MULTIDB.md §1). A plain single-`[database]`
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
                    let config = raw_to_config(db, tables)?;
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
