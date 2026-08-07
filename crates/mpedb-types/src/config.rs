//! Shared TOML configuration: every process opens the same config file and
//! derives from it both the runtime options and the schema (whose hash must
//! match the database it attaches to).

use crate::error::{Error, Result};
use std::collections::BTreeSet;
use crate::schema::{ColumnDef, DefaultExpr, Schema, TableDef};
use crate::value::{Affinity, Collation, ColumnType, Value};
use serde::Deserialize;
use std::path::PathBuf;

/// How the database is backed. Chosen by `database.path` spelling.
///
/// | path | kind | multi-process attach |
/// |---|---|---|
/// | `":memory:"` | [`StorageKind::PrivateMemory`] | **no** (default in-memory) |
/// | `":memory:shared:<name>"` | [`StorageKind::SharedMemory`] | **yes** (tmpfs file) |
/// | any filesystem path | [`StorageKind::File`] | **yes** |
///
/// LLM / tooling: prefer `":memory:"` for single-process tests and harnesses;
/// use `":memory:shared:foo"` when two OS processes must share one RAM-backed
/// DB (same schema seed / size_mb / max_readers on every attach). Ordinary
/// paths remain the production multi-process model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// Process-private: memfd / unlinked temp. Not attachable by path.
    PrivateMemory,
    /// Named RAM-backed multi-process DB under `/dev/shm` (or temp dir).
    SharedMemory,
    /// Ordinary filesystem `.mpedb` file.
    File,
}

impl StorageKind {
    /// Whether a second OS process can attach by opening the same config path.
    pub fn multi_process_attach(self) -> bool {
        !matches!(self, StorageKind::PrivateMemory)
    }

    /// Short, stable hint for tools/LLMs (also returned from
    /// [`crate` facade helpers on `Database`]).
    pub fn multi_process_hint(self) -> &'static str {
        match self {
            StorageKind::PrivateMemory => {
                "private in-memory (:memory:) — not multi-process; for shared RAM use \
                 path = \":memory:shared:<name>\" (same name in every process) or a \
                 filesystem path"
            }
            StorageKind::SharedMemory => {
                "shared in-memory (:memory:shared:<name>) — multi-process attach OK via \
                 the same path spelling; backing lives under /dev/shm when available"
            }
            StorageKind::File => {
                "file-backed — multi-process attach OK; every process opens the same path"
            }
        }
    }
}

/// Classify and optionally rewrite `database.path` for open.
///
/// * `":memory:"` → left as-is (private memfd).
/// * `":memory:shared:NAME"` → resolved to `/dev/shm/mpedb-shared-NAME.mpedb`
///   (or `$TMPDIR/...` if `/dev/shm` is missing).
/// * anything else → unchanged filesystem path.
pub fn resolve_storage_path(path: &std::path::Path) -> Result<(PathBuf, StorageKind)> {
    let s = path.as_os_str().to_string_lossy();
    if s == ":memory:" {
        return Ok((PathBuf::from(":memory:"), StorageKind::PrivateMemory));
    }
    if let Some(name) = s.strip_prefix(":memory:shared:") {
        validate_shared_memory_name(name)?;
        let base = if std::path::Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let resolved = base.join(format!("mpedb-shared-{name}.mpedb"));
        return Ok((resolved, StorageKind::SharedMemory));
    }
    Ok((path.to_path_buf(), StorageKind::File))
}

fn validate_shared_memory_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(Error::Config(
            "path = \":memory:shared:<name>\": name must be 1..=64 characters".into(),
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Config(
            "path = \":memory:shared:<name>\": name must be [A-Za-z0-9_-]+ only".into(),
        ));
    }
    Ok(())
}

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
/// (`mirror import` from PG) is born [`Postgres`](Dialect::Postgres); every
/// other database defaults to [`Sqlite`](Dialect::Sqlite). It is a
/// per-process compilation option, like [`Durability`] — it decides what
/// `prepare` accepts, never what a stored plan means (a plan is self-describing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// sqlite's rule: a bare column is accepted **only when mpedb reproduces
    /// sqlite's value exactly** (mpedb's core guarantee — never a wrong answer).
    /// Three cases qualify: the column is provably never evaluated (a dead
    /// `COALESCE`/`CASE` branch that constant folding removes); the query has
    /// exactly one `min()`/`max()` (the bare column takes the extremum's row,
    /// even alongside a `count`/`sum`/`avg`); or the query has NO `min()`/`max()`
    /// and reads a single INTEGER-PK table, where sqlite's "arbitrary" pick is
    /// the group's lowest-rowid row and mpedb carries the minimum-PK row to match
    /// it (#88). A bare column mpedb cannot reproduce — the lowest-rowid case over
    /// a join or a non-rowid PK, or two-or-more `min()`/`max()` — is REFUSED with
    /// a clean bind error rather than guessed. The default.
    #[default]
    Sqlite,
    /// PostgreSQL / SQL-standard strictness: a bare column is ALWAYS an error
    /// (`must appear in GROUP BY …`). mpedb's original behavior; the mode a
    /// PostgreSQL-imported database is born with, so a query that PG refused
    /// keeps being refused here.
    Postgres,
}

impl Dialect {
    /// The configured strictness as its config-string (`"sqlite"` / `"postgres"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgres",
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
    /// Lifetime table-id budget: how many `CREATE TABLE`s this database will
    /// ever accept. LIFETIME, not live — ids are never reused, so a dropped
    /// table keeps its slot (DESIGN-DROP-TABLE §0) and `mpedb compact-ids` is
    /// what reclaims one. Absent ⇒ [`crate::MAX_TABLES`].
    ///
    /// It is not a representation limit — every persisted key already spends a
    /// fixed `u32` on the id, so raising this widens nothing. What it costs is
    /// the schema record: ~17 bytes per tombstone in one catalog entry that is
    /// re-encoded on every DDL. That is the reason for a bound at all, and the
    /// reason compaction beats raising it.
    ///
    /// Checked only on the DDL path, never per row or per query.
    pub max_tables: usize,
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
    /// Per-statement-execution budget on `Value` cells LIVE in a nested-loop
    /// join's materialized intermediate product (`rows × row width`, summed
    /// over the accumulated tuple set, the held inner side, and the next
    /// stage being built). `max_work_rows` bounds how much a query READS;
    /// this bounds how much a join HOLDS — the memory-proportional guard
    /// that turns an N-way cross join's OOM abort into a clean
    /// [`crate::Error::RuntimeBudget`]. `0` = unlimited (the join then still
    /// fails cleanly on allocation pressure via fallible reservation, but a
    /// machine with overcommit may OOM-kill first — the deterministic cap is
    /// the reliable guard). Absent in config ⇒ [`DEFAULT_MAX_JOIN_CELLS`].
    pub max_join_cells: u64,
    /// Worker-thread ceiling for the ADAPTIVE parallel aggregate fold
    /// (design/DESIGN-PARALLEL-READ.md §8). `0` = AUTO: `min(available cores,
    /// 8)`, resolved per engagement against the free cores and the reader
    /// census. `1` = never parallel. A per-process execution option like
    /// `durability` — the answer (values, ties, raises, refusal points) is
    /// identical at every thread count, so this knob is observable only as
    /// wall time. Absent in config ⇒ `0` (auto).
    pub max_query_threads: u32,
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
    /// GROUP BY column-strictness dialect ([`Dialect`], COMPAT.md). Set from
    /// `[compat] dialect` (default [`Dialect::Sqlite`]); a PostgreSQL
    /// `mirror import` overrides it to [`Dialect::Postgres`] so the strictness
    /// travels with the data's origin.
    ///
    /// **The FILE is authoritative** — this is what a config may SEED it with.
    /// It used to be per-process like `durability`, and that was a silent
    /// loosening bug: `Database::open_from_file` (the mirror daemon, `dump`,
    /// the C-API shim, `mpedb <file>`) has no `[compat]` to read, so a
    /// PostgreSQL-born file reopened lenient. The database records it now.
    pub dialect: Dialect,
    /// Initial `PRAGMA foreign_keys` for connections to this database
    /// (`[compat] foreign_keys`, default false — sqlite's default). A
    /// connection may still flip it at runtime, as sqlite's pragma is
    /// per-connection; the FILE decides where it starts. Same authority and
    /// the same reason as `dialect`.
    pub foreign_keys: bool,
    /// Whether the CONFIG named each of the two above, as opposed to taking
    /// the default. See [`CompatOptions`]: a config that names a value the
    /// stored record contradicts is a named refusal, one that names nothing
    /// defers to the file.
    pub dialect_named: bool,
    pub foreign_keys_named: bool,
    /// This process's role in a sync topology (`[sync] role`, #157):
    /// `standalone` (default), `replica` or `authority`.
    ///
    /// **Deliberately not file-authoritative.** Schema and geometry hard-error
    /// on config drift because they describe the bytes on disk; a role describes
    /// what *this process* is doing with them, and the same `.mpedb` may be
    /// opened as a replica by one process and standalone by another at the same
    /// moment. Putting it anywhere hashed would turn a deployment knob into a
    /// property of the file. Kept as a string here so `mpedb-types` stays
    /// dependency-light — `mpedb::sync::Role::parse` is where it becomes an enum
    /// and where an unknown value is refused by name.
    pub sync_role: String,
    /// Path of the upstream `.mpedb` a replica syncs against. Advisory: nothing
    /// opens it implicitly, because an engine that reached out to another file
    /// at `open` would make a config typo into a surprise I/O dependency.
    pub sync_upstream: Option<String>,
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

/// The join-materialization cell budget default (2^24 ≈ 670 MB at the measured
/// ~40 B resident per cell). `0` = unlimited.
///
/// It was 2^28 — 268 M cells, ~11 GB — calibrated against a corpus peak of
/// 68 M cells in `select5.test`. That calibration measured the PLANNER, not the
/// queries: those 68 M cells were cartesian products the join solver had
/// declined to reorder (its state mask was 24 tables wide; `select5` joins up
/// to 64). With the solver covering the full width the same file peaks at 9 MB,
/// and the WHOLE 622-file corpus peaks at 345 MB resident — so the number
/// calibrated against the old peak was 30× larger than anything legitimate
/// needs.
///
/// That mattered because a budget is only a guard if it trips BEFORE the memory
/// wall, and ~11 GB is past the wall on any ordinary machine: MEASURED on a
/// 7.9 GB box, a runaway join was OOM-KILLED by the kernel — taking the whole
/// process group with it — while still far under the budget that was supposed
/// to catch it. Fallible reservation does not cover for that either, since
/// Linux's default heuristic overcommit lets the reservation succeed and kills
/// on first touch.
///
/// 2^24 is ~2× the corpus's measured high-water and small enough to trip on any
/// box, which is what makes the runaway a named [`crate::Error::RuntimeBudget`]
/// rather than a dead host. Deliberately NOT derived from the machine: the trip
/// point is a property of the data and the plan, so it reproduces everywhere.
pub const DEFAULT_MAX_JOIN_CELLS: u64 = 16_777_216;

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
    /// Optional `[sync]` section (#157): this PROCESS's role, not the file's.
    #[serde(default)]
    sync: Option<RawSync>,
}

/// The `[sync]` TOML section (#157).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSync {
    /// `"standalone"` (default), `"replica"` or `"authority"`. Validated where
    /// it becomes an enum (`mpedb::sync::Role::parse`) so the refusal can name
    /// the valid values, which the model language already requires of itself.
    #[serde(default)]
    role: Option<String>,
    /// Path of the upstream `.mpedb`. Advisory — never opened implicitly.
    #[serde(default)]
    upstream: Option<String>,
}

/// The `[runtime]` TOML section (#74): per-process execution limits.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    /// Deterministic work-row budget per statement execution; `0` = unlimited.
    /// Absent ⇒ [`DEFAULT_MAX_WORK_ROWS`].
    #[serde(default)]
    max_work_rows: Option<u64>,
    /// Live-cell budget on join materialization; `0` = unlimited.
    /// Absent ⇒ [`DEFAULT_MAX_JOIN_CELLS`].
    #[serde(default)]
    max_join_cells: Option<u64>,
    /// Worker-thread ceiling for the adaptive parallel aggregate fold; `0` =
    /// auto (`min(cores, 8)`), `1` = serial. Absent ⇒ `0`.
    #[serde(default)]
    max_query_threads: Option<u32>,
}

/// The resolved `[runtime]` limits (#74), one value per knob.
#[derive(Clone, Copy)]
struct RuntimeLimits {
    max_work_rows: u64,
    max_join_cells: u64,
    max_query_threads: u32,
}

impl RawRuntime {
    fn resolve(this: Option<&RawRuntime>) -> RuntimeLimits {
        RuntimeLimits {
            max_work_rows: this
                .and_then(|r| r.max_work_rows)
                .unwrap_or(DEFAULT_MAX_WORK_ROWS),
            max_join_cells: this
                .and_then(|r| r.max_join_cells)
                .unwrap_or(DEFAULT_MAX_JOIN_CELLS),
            max_query_threads: this.and_then(|r| r.max_query_threads).unwrap_or(0),
        }
    }
}

/// The `[compat]` TOML section (COMPAT.md): per-process SQL-dialect toggles.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompat {
    /// `"sqlite"` (the default) or `"postgres"` — which engine mpedb agrees with
    /// wherever the two genuinely disagree (COMPAT.md's dialect table).
    ///
    /// The key was `bare_group_by` while the bare-grouped-column rule was the
    /// only thing it decided. It now reaches the TOKENIZER (`::` casts, `$$`
    /// quoting) as well, so the old name described a fraction of its job. The
    /// alias costs one attribute and saves every config file already written —
    /// the only kind of migration this project keeps.
    #[serde(default, alias = "bare_group_by")]
    dialect: Option<String>,
    /// Initial `PRAGMA foreign_keys` for connections opened from this config
    /// (#194). Default FALSE — sqlite's own default, and the only value that
    /// leaves an existing database's write behaviour unchanged. A connection
    /// can still flip it at runtime.
    #[serde(default)]
    foreign_keys: Option<bool>,
}

/// The resolved `[compat]` section.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompatOptions {
    pub dialect: Dialect,
    pub foreign_keys: bool,
    /// Whether the section NAMED each value, as opposed to taking the default.
    ///
    /// It matters because the FILE now records these two (the `tune` record),
    /// and the file is authoritative. A config that names a value the file
    /// contradicts is a refusal rather than a silent override; a config that
    /// names nothing simply defers. Without this bit the two cases are
    /// indistinguishable, and every config-free-shaped attach would look like
    /// a caller demanding `sqlite`.
    pub dialect_named: bool,
    pub foreign_keys_named: bool,
}

impl RawCompat {
    fn resolve(this: Option<&RawCompat>) -> Result<CompatOptions> {
        let dialect = match this.and_then(|c| c.dialect.as_deref()) {
            None | Some("sqlite") => Dialect::Sqlite,
            Some("postgres") => Dialect::Postgres,
            Some(other) => {
                return Err(Error::Config(format!(
                    "compat.dialect must be sqlite|postgres, got `{other}`"
                )))
            }
        };
        Ok(CompatOptions {
            dialect,
            foreign_keys: this.and_then(|c| c.foreign_keys).unwrap_or(false),
            dialect_named: this.is_some_and(|c| c.dialect.is_some()),
            foreign_keys_named: this.is_some_and(|c| c.foreign_keys.is_some()),
        })
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
    /// Lifetime `CREATE TABLE` budget; absent ⇒ [`crate::MAX_TABLES`].
    #[serde(default)]
    max_tables: Option<usize>,
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

/// Maximum `database.size_mb`. mpedb pre-reserves (`fallocate`) this many MiB up
/// front, so the file size is a deliberate space reservation, not a growth cap.
/// Bounded well within a 64-bit process's mmap address space (page ids are u64);
/// the practical limit is disk, not this. 16 TiB — comfortably past an 800 GiB
/// database.
pub const MAX_DB_SIZE_MB: u64 = 1 << 24;

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

/// Escape a filesystem path for embedding in a TOML **basic string** (`"…"`).
///
/// Found by running the engine on Windows (#159): `C:\Users\bob\app.db`
/// interpolated straight into `path = "…"` contains `\U`, which TOML reads as
/// a unicode escape — so `sqlite3_open` through the C-API shim, `mpedb open`,
/// and the Python `connect()` all failed on an ordinary Windows path with
/// `invalid unicode 4-digit hex code`. It never showed on Unix because Unix
/// paths contain no backslashes.
///
/// Escaping rather than switching to a TOML *literal* string (`'…'`): a literal
/// has no escapes at all, which is tempting, but it also cannot contain a
/// single quote — and `'` is a legal filename character on both Windows and
/// Unix. Trading one unrepresentable path for another is not a fix.
///
/// Handles the full basic-string escape set, not just backslash: a path may
/// legally contain a quote, and control characters are rejected outright by
/// TOML rather than passed through.
pub fn toml_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

impl Config {
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let runtime = RawRuntime::resolve(raw.runtime.as_ref());
        let compat = RawCompat::resolve(raw.compat.as_ref())?;
        let (sync_role, sync_upstream) = match raw.sync {
            Some(s) => (s.role, s.upstream),
            None => (None, None),
        };
        raw_to_config(raw.database, raw.tables, runtime, compat, sync_role, sync_upstream)
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
    runtime: RuntimeLimits,
    compat: CompatOptions,
    sync_role: Option<String>,
    sync_upstream: Option<String>,
) -> Result<Config> {
        if db.path.is_empty() {
            return Err(Error::Config("database.path must be set".into()));
        }
        // Storage path dialects (see [`StorageKind`]):
        //   ":memory:"              — process-private (default in-memory)
        //   ":memory:shared:<name>" — multi-process RAM via /dev/shm (or temp)
        //   any other path          — ordinary file DB
        let is_private_memory = db.path == ":memory:";
        let is_shared_memory = db.path.starts_with(":memory:shared:");
        if is_shared_memory {
            let name = &db.path[":memory:shared:".len()..];
            validate_shared_memory_name(name)?;
        }
        if db.size_mb < 1 || db.size_mb > MAX_DB_SIZE_MB {
            return Err(Error::Config(format!(
                "database.size_mb must be in 1..={MAX_DB_SIZE_MB}"
            )));
        }
        if db.max_readers < 1 || db.max_readers > 65_536 {
            return Err(Error::Config("database.max_readers must be in 1..=65536".into()));
        }
        if let Some(t) = db.max_tables {
            // The upper bound is the DECODE ceiling, and it is a hard one: a
            // config may not authorise minting a schema that a reader — this
            // process included — would refuse to load.
            if !(1..=crate::MAX_TABLES_CEILING).contains(&t) {
                return Err(Error::Config(format!(
                    "database.max_tables must be in 1..={}",
                    crate::MAX_TABLES_CEILING
                )));
            }
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
        // Process-private in-memory DBs have no durable companion file.
        if is_private_memory && durability != Durability::None {
            return Err(Error::Config(
                "path = \":memory:\" requires durability = none (or omit durability). \
                 For multi-process RAM sharing use path = \":memory:shared:<name>\" \
                 (tmpfs file under /dev/shm) instead."
                    .into(),
            ));
        }
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
                // `numeric` is the config spelling of an `any` column carrying
                // sqlite's NUMERIC affinity: it holds an int, a real or a
                // string per value like `any`, but text that is losslessly
                // numeric is CONVERTED on the way in ('1.50' → 1.5). `any`
                // itself stays verbatim — the two are opposites and the config
                // vocabulary has to be able to say which one is meant.
                let (ty, affinity) = if c.ty.eq_ignore_ascii_case("numeric") {
                    (ColumnType::Any, Affinity::Numeric)
                } else {
                    let ty = ColumnType::parse(&c.ty).ok_or_else(|| {
                        Error::Config(format!("unknown type `{}` for {}.{}", c.ty, t.name, c.name))
                    })?;
                    (ty, Affinity::implied_by(ty))
                };
                let default = match &c.default {
                    None => None,
                    Some(v) => Some(parse_default(v, ty).map_err(|m| {
                        Error::Config(format!("bad default for {}.{}: {m}", t.name, c.name))
                    })?),
                };
                columns.push(ColumnDef { generated: None, default_text: None, decl: None,
                    name: c.name.clone(),
                    ty,
                    nullable: c.nullable,
                    unique: c.unique,
                    indexed: c.indexed,
                    default,
                    check: c.check.clone(),
                    // The config (TOML) schema path declares no collation yet —
                    // COLLATE is a CREATE TABLE / ALTER surface (BINARY default).
                    collation: Collation::Binary,
                    affinity,
                });
            }
            let primary_key = t
                .primary_key
                .iter()
                .map(|pk| {
                    columns
                        .iter()
                        .position(|c| crate::ident_eq(&c.name, pk))
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
                                .position(|c| crate::ident_eq(&c.name, name))
                                .map(|i| i as u16)
                                .ok_or_else(|| {
                                    Error::Config(format!(
                                        "index column `{name}` not found in table `{}`",
                                        t.name
                                    ))
                                })
                        })
                        .collect::<Result<Vec<u16>>>()?;
                    // A config `[[table.index]]` entry has no name — naming an index is a
                    // `CREATE INDEX` thing, and the config declares shape, not identity.
                    Ok(crate::schema::IndexDef {
                        collations: Vec::new(),
                        columns: cols,
                        unique: ix.unique,
                        predicate: None,
                        exprs: Vec::new(),
                        name: None,
                    })
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
                implicit_rowid: false, autoincrement: false,
                // Config-defined tables are always ordinary; FTS tables are
                // created live via `CREATE VIRTUAL TABLE` (design/DESIGN-FTS.md).
                kind: crate::schema::TableKind::Standard,
                // The config declares one table's SHAPE. A foreign key is a
                // relationship BETWEEN two, and arrives with `CREATE TABLE`.
                foreign_keys: Vec::new(),
            });
        }

        Ok(Config {
            options: DbOptions {
                path: PathBuf::from(db.path),
                size_bytes: db.size_mb * 1024 * 1024,
                max_readers: db.max_readers,
                max_tables: db.max_tables.unwrap_or(crate::MAX_TABLES),
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
                max_work_rows: runtime.max_work_rows,
                max_join_cells: runtime.max_join_cells,
                max_query_threads: runtime.max_query_threads,
                require_policy,
                dialect: compat.dialect,
                foreign_keys: compat.foreign_keys,
                dialect_named: compat.dialect_named,
                foreign_keys_named: compat.foreign_keys_named,
                sync_role: sync_role.unwrap_or_else(|| "standalone".to_string()),
                sync_upstream,
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
                max_tables: None,
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
                let runtime = RawRuntime::resolve(raw.runtime.as_ref());
                let compat = RawCompat::resolve(raw.compat.as_ref())?;
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
                    // A workspace member is a separate file with its own writer
                    // lock; a sync role is per PROCESS, so it is not inherited
                    // per member — a workspace-wide `[sync]` would have to mean
                    // something different for each member and therefore means
                    // nothing.
                    let config = raw_to_config(db, tables, runtime, compat, None, None)?;
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
        // sqlite's three time keywords, in the spelling `render` writes — so a
        // dumped schema round-trips through TOML.
        match s.to_ascii_uppercase().as_str() {
            "CURRENT_TIMESTAMP" => return Ok(DefaultExpr::CurrentTimestamp),
            "CURRENT_DATE" => return Ok(DefaultExpr::CurrentDate),
            "CURRENT_TIME" => return Ok(DefaultExpr::CurrentTime),
            _ => {}
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

    /// `[database] max_tables`: absent is the compiled default, present is
    /// taken verbatim, and out-of-range is refused against the DECODE ceiling
    /// rather than against the default.
    #[test]
    fn max_tables_knob() {
        let base = "[database]\npath = \"/dev/shm/t.mpedb\"\n";
        let cfg = Config::from_toml_str(base).unwrap();
        assert_eq!(cfg.options.max_tables, crate::MAX_TABLES);

        let cfg = Config::from_toml_str(&format!("{base}max_tables = 65536\n")).unwrap();
        assert_eq!(cfg.options.max_tables, 65_536);

        // (65 536 is above the compiled-in default — raising the budget is
        // what the knob is FOR.)

        let err = Config::from_toml_str(&format!("{base}max_tables = 0\n")).unwrap_err();
        assert!(format!("{err}").contains("max_tables"), "{err}");
        let over = crate::MAX_TABLES_CEILING + 1;
        let err = Config::from_toml_str(&format!("{base}max_tables = {over}\n")).unwrap_err();
        assert!(format!("{err}").contains("max_tables"), "{err}");
    }

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
    fn parses_runtime_max_join_cells() {
        // absent [runtime] ⇒ the finite default (caught-by-default guard)
        assert_eq!(
            Config::from_toml_str(SAMPLE).unwrap().options.max_join_cells,
            DEFAULT_MAX_JOIN_CELLS
        );
        // explicit value; both knobs coexist in one [runtime]
        let cfg = Config::from_toml_str(&format!(
            "{SAMPLE}\n[runtime]\nmax_work_rows = 42\nmax_join_cells = 7000"
        ))
        .unwrap();
        assert_eq!(cfg.options.max_work_rows, 42);
        assert_eq!(cfg.options.max_join_cells, 7000);
        // 0 = unlimited sentinel, preserved verbatim; the other knob keeps
        // its default when absent
        let cfg0 = Config::from_toml_str(&format!("{SAMPLE}\n[runtime]\nmax_join_cells = 0"))
            .unwrap();
        assert_eq!(cfg0.options.max_join_cells, 0);
        assert_eq!(cfg0.options.max_work_rows, DEFAULT_MAX_WORK_ROWS);
    }

    #[test]
    fn parses_compat_bare_group_by() {
        // absent [compat] ⇒ the sqlite (lenient) default
        assert_eq!(
            Config::from_toml_str(SAMPLE).unwrap().options.dialect,
            Dialect::Sqlite
        );
        // explicit sqlite / postgres
        let cfg = Config::from_toml_str(&format!(
            "{SAMPLE}\n[compat]\nbare_group_by = \"postgres\""
        ))
        .unwrap();
        assert_eq!(cfg.options.dialect, Dialect::Postgres);
        let cfg = Config::from_toml_str(&format!(
            "{SAMPLE}\n[compat]\nbare_group_by = \"sqlite\""
        ))
        .unwrap();
        assert_eq!(cfg.options.dialect, Dialect::Sqlite);
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

#[cfg(test)]
mod toml_escape_tests {
    use super::toml_escape;

    /// The exact string that broke on Windows: `\U` in `C:\Users` is a TOML
    /// unicode escape, so an unescaped path is a parse error, not a path.
    #[test]
    fn a_windows_path_survives_the_round_trip() {
        let p = r"C:\Users\bob\AppData\Local\app.mpedb";
        let toml = format!("[database]\npath = \"{}\"\nsize_mb = 8\n", toml_escape(p));
        let v: toml::Value = toml::from_str(&toml).expect("escaped path must parse");
        assert_eq!(v["database"]["path"].as_str().unwrap(), p);
    }

    /// A Unix path must come out byte-identical — the escape may not "fix"
    /// anything that was never broken.
    #[test]
    fn a_unix_path_is_unchanged() {
        let p = "/home/morten/db/app.mpedb";
        assert_eq!(toml_escape(p), p);
    }

    /// Quotes and single quotes are both legal in filenames. The quote is why
    /// this escapes rather than switching to a TOML literal string, which
    /// cannot contain a single quote at all.
    #[test]
    fn quotes_of_both_kinds_survive() {
        for p in [r#"/tmp/it"s.mpedb"#, "/tmp/it's.mpedb", r#"C:\a"b\c's.mpedb"#] {
            let toml = format!("[database]\npath = \"{}\"\n", toml_escape(p));
            let v: toml::Value = toml::from_str(&toml).unwrap_or_else(|e| panic!("{p:?}: {e}"));
            assert_eq!(v["database"]["path"].as_str().unwrap(), p, "round trip failed for {p:?}");
        }
    }
}
