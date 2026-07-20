use crate::error::{Error, Result};
use crate::expr::{ExprProgram, Instr};
use crate::value::{read_value, write_value, Affinity, Collation, ColumnType, Value};
use crate::{MAX_COLUMNS, MAX_TABLES};

/// Which of sqlite's two storage modes a GENERATED column was declared with.
///
/// The distinction is a STORAGE promise, not a value promise: a generated
/// expression may reference only other columns of the SAME row and must be
/// deterministic, so computing it at write time and computing it at read time
/// can never disagree. mpedb therefore materializes BOTH kinds into the row and
/// keeps this tag purely as declared metadata — it decides what
/// `PRAGMA table_xinfo.hidden` reports (2 = virtual, 3 = stored) and whether
/// `ALTER TABLE … ADD COLUMN` is allowed on a non-empty table, and nothing else.
/// A `Virtual` column therefore costs mpedb the row bytes sqlite would not
/// spend; it never costs a different answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedKind {
    Virtual = 0,
    Stored = 1,
}

impl GeneratedKind {
    pub fn from_tag(t: u8) -> Option<GeneratedKind> {
        match t {
            0 => Some(GeneratedKind::Virtual),
            1 => Some(GeneratedKind::Stored),
            _ => None,
        }
    }

    /// The word `CREATE TABLE` spells it with, and what `table_xinfo` keys off.
    pub fn keyword(self) -> &'static str {
        match self {
            GeneratedKind::Virtual => "VIRTUAL",
            GeneratedKind::Stored => "STORED",
        }
    }

    /// `PRAGMA table_xinfo`'s `hidden` code for a generated column.
    pub fn xinfo_hidden(self) -> i64 {
        match self {
            GeneratedKind::Virtual => 2,
            GeneratedKind::Stored => 3,
        }
    }
}

/// A `GENERATED ALWAYS AS (<expr>) [STORED|VIRTUAL]` column.
///
/// Unlike [`ColumnDef::check`] — which stores SQL SOURCE and is compiled into a
/// side table by the facade at attach time — the compiled program lives HERE,
/// in the schema, and travels in the canonical bytes. That is deliberate: a
/// generated value has to be computed on every write path there is (the plan
/// executor, the engine's typed row API, `ALTER TABLE ADD COLUMN`'s backfill,
/// the mirror importer), and several of those hold a `&TableDef` and nothing
/// else. Threading a side table to all of them is how a path gets forgotten and
/// silently writes NULL into a generated column — a wrong answer, not a
/// refusal. With the program in the schema there is no path that can miss it.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedCol {
    /// The `AS (…)` expression source, verbatim, for DDL round-trip and error
    /// messages. Participates in the schema hash.
    pub expr: String,
    pub kind: GeneratedKind,
    /// The expression compiled against this table's columns (`Instr::PushCol`
    /// ordinals). Bounds- and acyclicity-checked by [`Schema::validate`], so a
    /// corrupt mapping cannot make evaluation read out of range or loop.
    pub program: ExprProgram,
}

/// Default value for a column when an INSERT omits it.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultExpr {
    Const(Value),
    /// `now()` — the commit-time timestamp, filled in by the engine.
    Now,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub unique: bool,
    /// A non-unique secondary index (duplicates allowed). Distinct from
    /// `unique`, which also builds an index but enforces uniqueness. A column
    /// with either is a secondary index; `unique` decides how it is stored and
    /// whether inserts are checked.
    pub indexed: bool,
    pub default: Option<DefaultExpr>,
    /// CHECK expression source (SQL expression over this table's columns).
    /// Compiled to expression IR at attach time by the SQL layer; the source
    /// text participates in the schema hash.
    pub check: Option<String>,
    /// The column's DECLARED collating sequence (`name TEXT COLLATE NOCASE`),
    /// the DEFAULT for `= <> < <= > >= IN BETWEEN`, `ORDER BY`, `GROUP BY` and
    /// `DISTINCT` on this column (sqlite's precedence rung 2 — an explicit
    /// `COLLATE` on an operand still overrides). [`Collation::Binary`] unless
    /// declared. Only meaningful for TEXT: `validate` refuses a non-BINARY
    /// collation on any other type, and — because mpedb does not yet fold
    /// collated ON-DISK keys — on any PRIMARY KEY / indexed column (a collated
    /// UNIQUE/index is refused, never answered wrong; comparisons and sorts
    /// still honor the collation). Participates in the schema hash (canonical
    /// bytes v6).
    pub collation: Collation,
    /// sqlite's TYPE AFFINITY for this column: what happens to a value on its
    /// way IN, as distinct from `ty`, which is what the column may hold at rest.
    ///
    /// The two are separate because ONE mpedb type hosts TWO sqlite behaviours
    /// that are exact opposites, and collapsing them produced a wrong answer:
    ///
    /// | declared              | affinity | `ty`  | `'1.50'` stores as |
    /// |-----------------------|----------|-------|--------------------|
    /// | `decimal(10,2)`, `numeric`, `datetime`, `date` | `Numeric` | `Any` | `1.5` (real) |
    /// | *(no type at all)*    | `Blob`   | `Any` | `'1.50'` (text)    |
    ///
    /// [`Affinity::Numeric`] is the ONLY value here that mpedb *applies*
    /// ([`crate::expr::store_affinity`]); every other affinity belongs to a
    /// rigid column that REFUSES a mismatched value instead of converting it,
    /// so for those `validate` pins this to [`Affinity::implied_by`] the storage
    /// type — a field that could disagree with `ty` would be a second source of
    /// truth. Set from the declared type name by [`ColumnType::declared`].
    /// Participates in the schema hash (canonical bytes v7).
    pub affinity: Affinity,
    /// The column's declared type text, **verbatim as `CREATE TABLE` spelled
    /// it** (`float`, `unsigned big int`, `number(5)`, `cblob`) — not a
    /// vocabulary, just the bytes.
    ///
    /// It exists because `ty` + `affinity` are LOSSY about the name: every
    /// unrecognized name folds into `(Any, Numeric)`, and `float` folds into
    /// `Float64` whose canonical spelling is `REAL`. That loss is invisible in
    /// SQL but not to a consumer: `sqlite3_column_decltype` is *defined* as the
    /// declared text, and CPython's `PARSE_DECLTYPES` looks its converter up
    /// under exactly that string — so reporting the canonical name silently
    /// skips the converter and hands back a different VALUE with no error.
    ///
    /// `None` = no declared type at all (`CREATE TABLE t(a)`, sqlite's NULL
    /// decltype), or a schema built without DDL text (the TOML config path,
    /// synthetic catalog tables), where [`ColumnType::decltype_name`] remains
    /// the answer. Read it through [`ColumnDef::decltype`], never directly.
    /// Participates in the schema hash (canonical bytes v8).
    pub decl: Option<String>,
    /// `GENERATED ALWAYS AS (<expr>) [STORED|VIRTUAL]` — the column's value is
    /// COMPUTED from the rest of the row, never supplied by the writer, and
    /// `INSERT`/`UPDATE` refuse to name it. `None` for an ordinary column.
    /// Participates in the schema hash (canonical bytes v9).
    pub generated: Option<GeneratedCol>,
}

/// The store-time conversion a column of this shape applies — the SINGLE place
/// the gate lives, for callers that hold a `(type, affinity)` pair before a
/// [`ColumnDef`] exists (the DDL path converting a `DEFAULT`).
///
/// The gate is the point: sqlite's affinity is applied ONLY to an `Any` column,
/// the one that can hold whatever the conversion produces. On a rigid column
/// mpedb REFUSES a mismatched value instead, so converting there would quietly
/// make `TEXT DEFAULT 5` legal and undo the rigidity — which is exactly the bug
/// a first cut of this shipped with.
pub fn store_into(ty: ColumnType, affinity: Affinity, v: Value) -> Value {
    if ty == ColumnType::Any && affinity != Affinity::Blob {
        crate::expr::store_affinity(affinity, v)
    } else {
        v
    }
}

impl ColumnDef {
    /// Whether a value is CONVERTED on the way into this column (sqlite's
    /// store-time affinity) rather than type-checked as it stands.
    ///
    /// True only for an `Any` column with a converting affinity. `Any` is the
    /// only column that can HOLD whatever a conversion produces; every rigid
    /// column refuses a mismatched value instead, which is narrower than
    /// sqlite but never a different answer, and converting there would silently
    /// undo the rigidity this database exists for. `Blob` affinity — the
    /// typeless column — converts nothing by definition.
    pub fn converts_on_store(&self) -> bool {
        self.ty == ColumnType::Any && self.affinity != Affinity::Blob
    }

    /// What `sqlite3_column_decltype` reports for this column: the VERBATIM
    /// declared text where the schema has it, else the canonical spelling of
    /// the storage type, else `None` (sqlite's NULL) for a typeless column.
    pub fn decltype(&self) -> Option<&str> {
        match &self.decl {
            Some(d) => Some(d.as_str()),
            None => self.ty.decltype_name(),
        }
    }
}

/// One secondary index (canonical-bytes v2, DESIGN-SCHEMA-V2). `index_no` in
/// the catalog/plans is `1 + position` in `TableDef::indexes` (0 = PK tree).
/// Column order is significant. This list is the SINGLE source of truth for
/// index numbering — the per-column `unique`/`indexed` flags are input sugar
/// and in-memory convenience, reconstructed from here on decode.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexDef {
    /// Ordinals into `TableDef::columns`, in key order.
    pub columns: Vec<u16>,
    pub unique: bool,
}

/// Distinguishes an ordinary table from a full-text-search virtual table
/// (`CREATE VIRTUAL TABLE … USING fts5(…)`, design/DESIGN-FTS.md §1). An FTS
/// table is stored like any table — an auto `rowid` INTEGER PK plus its declared
/// TEXT columns — but the engine ALSO maintains an inverted-index B+tree over
/// its content (a reserved `index_no`), and `MATCH` compiles to an FtsScan
/// against it. The tokenizer choice is FROZEN here (content-hashed with the
/// schema and every plan) so a query can never tokenize differently than the
/// index was built with — the rigid-schema advantage over sqlite's silently
/// mismatched external tokenizers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// An ordinary user table.
    Standard,
    /// An FTS5 content + inverted-index table, with its frozen tokenizer.
    Fts { tokenizer: crate::fts::Tokenizer },
}

impl TableKind {
    pub fn is_fts(self) -> bool {
        matches!(self, TableKind::Fts { .. })
    }
    pub fn fts_tokenizer(self) -> Option<crate::fts::Tokenizer> {
        match self {
            TableKind::Fts { tokenizer } => Some(tokenizer),
            TableKind::Standard => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    /// Stable table id (DESIGN-SCHEMA-V2): explicit in the canonical bytes,
    /// stable for the table's life, allocated lowest-free (always
    /// `< MAX_TABLES`, which footprint/CDC decode re-checks). In the
    /// current format window ids are DENSE 0..n and equal the position in
    /// `Schema::tables` — enforced by `validate`, relaxed only when DROP
    /// TABLE lands with the positional audit (design §6).
    pub id: u32,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Indices into `columns`. Non-empty; PK columns must be NOT NULL.
    pub primary_key: Vec<u16>,
    /// Secondary indexes in `index_no` order. `Schema::new` fills this from
    /// the column flags (declaration order) and appends explicitly declared
    /// entries; hand-built `TableDef`s normally leave it empty and let
    /// `Schema::new` derive.
    pub indexes: Vec<IndexDef>,
    /// TOMBSTONE marker (#47 stage 4, DROP TABLE). A dead slot keeps its `id`
    /// so `position == id` stays dense (no gap in `Schema::tables`), but holds
    /// no data: empty `name`, `columns`, `primary_key`, `indexes`. Its id is
    /// NEVER reused, so `tables.len()` is a monotone id high-water and every
    /// persisted `table_id` referencing a dropped table stays inert. `validate`
    /// skips the shape rules for a dead slot and enforces it IS empty.
    pub dead: bool,
    /// Ordinary vs. FTS virtual table (design/DESIGN-FTS.md §1). Canonical-bytes
    /// v4 carries this discriminant; a dead slot is always `Standard`.
    pub kind: TableKind,
    /// A `CREATE TABLE` with NO declared PRIMARY KEY (#94, sqlite parity). The
    /// engine synthesizes a HIDDEN auto-increment integer `rowid` column — the
    /// LAST column, the sole PRIMARY KEY — and this flag records that it is
    /// hidden: `SELECT *` and the default INSERT column list skip it, but it is
    /// addressable by the names `rowid` / `_rowid_` / `oid`, exactly as sqlite's
    /// implicit rowid. Storage/MVCC/btree treat it as an ordinary single-integer
    /// PK (it IS a rowid alias for auto-assign), so the whole engine is unchanged
    /// — only the SQL surface hides it. Canonical-bytes v5 carries this bit; a
    /// dead slot and an FTS table are always `false`. NOT derivable from the
    /// shape: an explicit `CREATE TABLE t(rowid INTEGER PRIMARY KEY)` has the
    /// same columns but a VISIBLE rowid, so the flag must be stored.
    pub implicit_rowid: bool,
}

impl TableDef {
    /// The tombstone that replaces a dropped table's slot (#47 stage 4). Keeps
    /// the id, frees the name for re-CREATE, holds no data.
    pub fn tombstone(id: u32) -> TableDef {
        TableDef {
            id,
            name: String::new(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            dead: true,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }
    }
}

impl TableDef {
    /// Whether ANY column of this table converts a value on the way in, i.e.
    /// whether [`TableDef::apply_store_affinity`] can change a row at all.
    /// Checked before touching a row so the zero-copy insert path (#40) keeps
    /// borrowing the caller's values when there is nothing to convert.
    pub fn converts_on_store(&self) -> bool {
        self.columns.iter().any(|c| c.converts_on_store())
    }

    /// Apply each column's store-time affinity to a row about to be written —
    /// sqlite's rule that a value entering a NUMERIC-affinity column becomes an
    /// integer or a real when that is lossless, and stays as it was otherwise.
    ///
    /// This runs BEFORE type checking, NOT NULL, CHECK, uniqueness, index-key
    /// encoding and `RETURNING`, because that is the order sqlite uses: the
    /// converted value is the value, and everything downstream must see it. A
    /// short row (fewer values than columns) is left to the arity check that
    /// follows; this only converts the positions it has.
    ///
    /// Idempotent — converting an already-converted row is a no-op — so a path
    /// that applies it twice is merely wasteful, never wrong.
    pub fn apply_store_affinity(&self, row: &mut [Value]) {
        for (v, c) in row.iter_mut().zip(&self.columns) {
            if c.converts_on_store() {
                let old = std::mem::replace(v, Value::Null);
                *v = store_into(c.ty, c.affinity, old);
            }
        }
    }

    /// Does this table have any `GENERATED ALWAYS AS (…)` column? The guard on
    /// every write path, so a table without one pays a single bool.
    pub fn has_generated(&self) -> bool {
        self.columns.iter().any(|c| c.generated.is_some())
    }

    /// Overwrite every generated column of `row` with its computed value.
    ///
    /// **Declaration order IS a valid evaluation order**: `validate` refuses a
    /// generated column that reads a generated column declared at or after it,
    /// so by the time slot `i` is evaluated every generated slot it can read is
    /// already final. That refusal is what buys a single left-to-right pass
    /// instead of a per-row topological sort, and it makes a dependency cycle
    /// (sqlite's "generated column loop") unrepresentable rather than detected.
    /// mpedb is narrower than sqlite here — sqlite resolves forward references
    /// — and that narrowness is a clean refusal at `CREATE TABLE`, never a
    /// stale value in a row.
    ///
    /// The computed value goes through the column's store-time affinity, the
    /// same gate an INSERTed value passes, so a `decimal(10,2)` generated column
    /// stores what the identical literal would have. The rigid type is enforced
    /// afterwards by the engine's `validate_row`, so an expression whose result
    /// does not fit its column is a clean `TypeMismatch` on the row.
    ///
    /// Idempotent: re-running it recomputes the same values from the same
    /// inputs, which is why both the executor and the engine may apply it.
    pub fn apply_generated(&self, row: &mut [Value], params: &[Value]) -> Result<()> {
        let mut stack = Vec::new();
        for (i, c) in self.columns.iter().enumerate() {
            let Some(g) = &c.generated else { continue };
            if i >= row.len() {
                break;
            }
            let v = g.program.eval_with_stack(&mut stack, row, params)?;
            let v = if c.ty == ColumnType::Int64 && matches!(v, Value::Bool(_)) {
                // The expression IR's comparison/logic result type is `Bool`;
                // an INTEGER generated column declared over one (`b AS (a > 3)`)
                // takes sqlite's 1/0, not a type error.
                Value::Int(matches!(v, Value::Bool(true)) as i64)
            } else if c.ty == ColumnType::Float64 && matches!(v, Value::Int(_)) {
                let Value::Int(n) = v else { unreachable!() };
                Value::Float(n as f64)
            } else {
                v
            };
            row[i] = store_into(c.ty, c.affinity, v);
        }
        Ok(())
    }

    pub fn column_index(&self, name: &str) -> Option<u16> {
        self.columns.iter().position(|c| c.name == name).map(|i| i as u16)
    }

    pub fn pk_types(&self) -> Vec<ColumnType> {
        self.primary_key
            .iter()
            .map(|&i| self.columns[i as usize].ty)
            .collect()
    }

    pub fn is_pk_column(&self, col: u16) -> bool {
        self.primary_key.contains(&col)
    }

    /// The column index of this table's INTEGER PRIMARY KEY *rowid alias*, if
    /// it has one. Per sqlite, a table whose PRIMARY KEY is a SINGLE integer
    /// column makes that column an alias for the rowid: a NULL or omitted value
    /// on INSERT auto-assigns `max(existing rowid) + 1` (the plain,
    /// non-AUTOINCREMENT rule — a freed top id is reusable). A composite PK, or
    /// a non-integer single PK, is NOT a rowid alias — those stay strict, so a
    /// NULL there is the usual NOT-NULL violation. FTS tables keep their own
    /// rowid discipline and are deliberately excluded. Inferred, never stored:
    /// the canonical schema bytes carry no rowid-alias flag, so this adds no
    /// schema-format surface.
    pub fn rowid_alias_col(&self) -> Option<u16> {
        if !matches!(self.kind, TableKind::Standard) {
            return None;
        }
        match self.primary_key.as_slice() {
            [c] if self.columns[*c as usize].ty == ColumnType::Int64 => Some(*c),
            _ => None,
        }
    }

    /// The column ordinal of the HIDDEN implicit `rowid` (#94), or `None` for a
    /// table with an explicit PRIMARY KEY. Synthesized as the LAST column, so
    /// the VISIBLE columns keep their natural declaration ordinals `0..n-1` and
    /// only the trailing one is hidden — which is why every "slot == output
    /// position" assumption in the `SELECT *` path survives unchanged.
    pub fn hidden_rowid_col(&self) -> Option<u16> {
        self.implicit_rowid
            .then(|| (self.columns.len() - 1) as u16)
    }

    /// Count of VISIBLE columns — every column `SELECT *` and the default INSERT
    /// column list expose. Equals `columns.len()` for an explicit-PK table and
    /// one fewer when a hidden rowid is present (it is the trailing column).
    pub fn visible_column_count(&self) -> usize {
        self.columns.len() - self.implicit_rowid as usize
    }

    /// The VISIBLE columns, in declaration order — the trailing hidden rowid (if
    /// any) elided. `SELECT *` / `RETURNING *` / the default INSERT list expand
    /// over exactly these.
    pub fn visible_columns(&self) -> &[ColumnDef] {
        &self.columns[..self.visible_column_count()]
    }

    /// Resolve one of sqlite's three rowid spellings (`rowid`, `_rowid_`, `oid`,
    /// case-insensitively) to the hidden rowid column of an implicit-rowid table.
    /// A REAL column of that name always wins (checked by the caller before this
    /// fallback), matching sqlite, and an explicit-PK table returns `None` so its
    /// name resolution is completely unchanged (#94 requirement 7).
    pub fn rowid_name_col(&self, name: &str) -> Option<u16> {
        let hidden = self.hidden_rowid_col()?;
        let lc = name.to_ascii_lowercase();
        (lc == "rowid" || lc == "_rowid_" || lc == "oid").then_some(hidden)
    }

    /// For an FTS table, the `(column_index, fts_colno)` of every content
    /// column — every non-primary-key column — with `fts_colno` assigned
    /// `0..n` in declaration order. This is the SINGLE colno rule shared by
    /// posting maintenance (engine) and query planning (SQL), so the two can
    /// never disagree about which column is `colno` k (design/DESIGN-FTS.md §7).
    pub fn fts_content_columns(&self) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        let mut colno = 0u16;
        for i in 0..self.columns.len() as u16 {
            if self.primary_key.contains(&i) {
                continue;
            }
            out.push((i, colno));
            colno += 1;
        }
        out
    }

    /// The FTS colno of a content column by its column index, or `None` if the
    /// index names the rowid PK (not a content column).
    pub fn fts_colno(&self, col_index: u16) -> Option<u16> {
        self.fts_content_columns()
            .into_iter()
            .find(|(ci, _)| *ci == col_index)
            .map(|(_, n)| n)
    }
}

/// A validated schema. Tables are sorted by name; a table's id is its index
/// in `tables` (stable because attach requires an identical schema hash).
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub tables: Vec<TableDef>,
}

/// Maximum length of a table / column / index identifier, in BYTES.
///
/// Pure policy: `write_str` length-prefixes with a `u32`, `read_str` bounds at
/// 1 MiB, and no identifier is ever a component of a btree key. It was 128,
/// which independently blocked Django's `backends` label — a generated m2m
/// through-table name comes out at 134 characters (design/DESIGN-TABLE-CAP.md
/// §7). Bytes, not chars: a non-ASCII name is measured by its UTF-8 length.
pub const MAX_IDENTIFIER_LEN: usize = 255;

/// What may be a table / column / index name.
///
/// This used to be `[A-Za-z_][A-Za-z0-9_]*`, which made mpedb's quoted-identifier
/// support ornamental: the tokenizer accepts all three spellings (`"x"`, `[x]`,
/// `` `x` ``, with `""` doubling), and then the schema validator rejected the
/// only names quoting EXISTS for. `CREATE TABLE "weird tbl"(x INT)` — accepted
/// by sqlite — failed with `invalid table name`.
///
/// The rule is now "anything we can represent faithfully", because everything
/// downstream can in fact represent it:
///
/// - **canonical bytes**: `write_str` is a `u32` length + raw UTF-8. No
///   constraint beyond valid UTF-8 (which `&str` guarantees) and the length.
/// - **the keycode ordering**: identifiers are never key components — catalog
///   keys are `[0x01, table_id BE, index_no BE]`, and the CDC/policy/mirror
///   sys-keys all use the numeric `table_id`. Nothing sorts a name.
/// - **the TOML config surface**: a dumped schema is re-readable because
///   `mpedb-cli`'s `schema_toml` now emits names as escaped TOML basic strings
///   (it used to interpolate them raw, which is why this had to move with it).
/// - **SQL text we emit**: the C-API's `sqlite_master.sql` reconstruction now
///   DOUBLES embedded `"` when quoting, as the mirror's exporters already did.
///
/// What is still refused, and why — each is a wrong answer, not a taste:
///
/// - **empty** — has no distinct identity in any surface, and the tokenizer
///   already refuses `""` / `[]` (as sqlite does).
/// - **control characters** (C0, DEL, C1 — `char::is_control`). `NUL` above all:
///   the C-API hands names out as NUL-terminated `const char*`
///   (`sqlite3_column_name`), so an embedded NUL silently TRUNCATES the name a
///   consumer sees — it would read back as a different identifier. The rest
///   (newline, CR, tab) would break the line-oriented surfaces that parse our
///   output — `mpedb dump`, EXPLAIN, the REPL.
/// - **the `__mpedb` prefix** — reserved for internal objects.
///
/// Everything else is allowed and matches sqlite 3.45.1, verified differentially:
/// spaces (interior, leading and trailing), punctuation including `"`, a leading
/// digit, non-ASCII/Unicode, and an all-whitespace name. An all-whitespace name
/// is a footgun but not a hazard — it round-trips byte-exactly through every
/// encoder above — and refusing it would be a divergence bought with nothing.
fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_IDENTIFIER_LEN
        && !s.chars().any(char::is_control)
        && !s.starts_with("__mpedb")
}

/// Upper bound on secondary indexes per table (canonical-bytes v2).
pub const MAX_INDEXES: usize = 32;

/// **Why a typeless (`any`) column MAY be a PRIMARY KEY / index column.**
///
/// `Schema::validate` refused it until now, on a reason that was correct at the
/// time and is no longer: "a key is memcmp-ordered and `any` has no order
/// across types". mpedb now has that order. [`Value::sort_cmp`](crate::Value::sort_cmp)
/// is sqlite's storage-class order (NULL < numbers < TEXT < BLOB), and
/// [`keycode::encode_group_value`](crate::keycode::encode_group_value) is that
/// order AS BYTES, with a pinned two-way contract: the bytes are equal exactly
/// when `sort_cmp` says `Equal`, and their byte order equals `sort_cmp`
/// wherever it answers. That is precisely what a key encoding is, so an `any`
/// key column is encoded with it ([`keycode::KeySpec`](crate::keycode::KeySpec))
/// rather than with the type-keyed encoder.
///
/// The refusal was ALSO covering a real bug, and dropping it without switching
/// the encoder would have reinstated the bug rather than the refusal. The
/// type-keyed encoder is wrong for a typeless column in BOTH directions,
/// verified against sqlite 3.45.1:
///
/// - it SPLITS `1` from `1.0` and `0` from `-0.0` — two PK rows where sqlite
///   raises `UNIQUE constraint failed`;
/// - it ALIASES the text `'1'` with the blob `x'31'` (identical payload bytes;
///   the type is not in the encoding) — ONE row where sqlite has two, i.e. an
///   INSERT silently overwriting an unrelated row.
///
/// **What is still refused, and where.** Allowing the STORAGE is not allowing
/// the ACCESS PATH. `planner::access` and `planner::join` never build a
/// `PkPoint`/`PkRange`/`IndexPoint`/`IndexRange` over an `any` column: such a
/// probe would have to apply the pair's *comparison affinity* to the bound
/// before encoding it (sqlite's rule — the binder's `ClassCmp`), and mpedb's
/// own `Bool`/`Timestamp` have no storage class at all, so `sort_cmp` calls
/// them peers where the key ranks them. Every predicate over an `any` column
/// therefore stays a residual filter over a full scan, which keeps the
/// comparison-affinity work's proof (a `ClassCmp` is never an access path)
/// true word for word. The cost is a scan; the alternative is an index that
/// can disagree with one.
pub const ANY_KEY_COLUMNS: () = ();

/// Normalize the column flag sugar and derive `TableDef::indexes` — shared
/// by seeding (`Schema::new`) and evolution (`Schema::with_added_table`).
/// A column that is both `unique` and `indexed` has ONE unique index (the
/// engine has always treated it so), and flags on the single PK column are
/// meaningless (the PK tree is index 0) — without normalization these
/// spellings round-trip unequally through the wire format, which carries no
/// flags. The `contains` guard keeps this IDEMPOTENT: re-wrapping a table
/// that already went through it must not double-derive into a
/// duplicate-shape refusal.
fn normalize_and_derive(t: &mut TableDef) {
    let single_pk = (t.primary_key.len() == 1).then(|| t.primary_key[0]);
    for (i, c) in t.columns.iter_mut().enumerate() {
        if c.unique {
            c.indexed = false;
        }
        if single_pk == Some(i as u16) {
            c.unique = false;
            c.indexed = false;
        }
    }
    let explicit = std::mem::take(&mut t.indexes);
    let mut list: Vec<IndexDef> = t
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            (c.unique || c.indexed)
                && !(t.primary_key.len() == 1 && t.primary_key[0] == *i as u16)
        })
        .map(|(i, c)| IndexDef { columns: vec![i as u16], unique: c.unique })
        .collect();
    for e in explicit {
        if !list.contains(&e) {
            list.push(e);
        }
    }
    t.indexes = list;
}

impl Schema {
    /// Build and validate a schema from table definitions (any order; sorted
    /// internally by name). Assigns DENSE stable ids 0..n in name-sorted
    /// order — deterministic under input reordering, which is what keeps the
    /// schema hash independent of `[[table]]` declaration order. Normalizes
    /// the column index flags (`unique` implies not separately `indexed` —
    /// they build ONE unique index) and derives `TableDef::indexes` from the
    /// flags in column-declaration order, appending any explicitly declared
    /// entries after the derived ones.
    pub fn new(mut tables: Vec<TableDef>) -> Result<Schema> {
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        for (pos, t) in tables.iter_mut().enumerate() {
            t.id = pos as u32;
            normalize_and_derive(t);
        }
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by APPENDING one table — `CREATE TABLE` (#47).
    /// Nothing renumbers: existing ids and positions are untouched, the new
    /// table takes the lowest free id (= the current count while ids are
    /// dense), and the vec stays id-sorted (creation order). Flags normalize
    /// and indexes derive exactly as at seed.
    pub fn with_added_table(&self, mut def: TableDef) -> Result<Schema> {
        // `tables.len()` (live + dead) is the monotone id high-water: dead
        // slots are never removed and ids are NEVER reused (DESIGN-DROP-TABLE
        // §0 — reuse would require a crash-atomic distributed purge of every
        // persisted `table_id` record, the exact silent-corruption class mpedb
        // exists to prevent; the bounded-limit + offline `regenerate` compaction
        // is the deliberate trade). Fail closed at MAX_TABLES — now a cost
        // bound (tombstone bloat), not a bitmap width (DESIGN-TABLE-CAP).
        if self.tables.len() >= MAX_TABLES {
            return Err(Error::Schema(
                "table-id space exhausted (MAX_TABLES lifetime creates); rebuild required".into(),
            ));
        }
        def.id = self.tables.len() as u32;
        def.dead = false;
        normalize_and_derive(&mut def);
        let mut tables = self.tables.clone();
        tables.push(def);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by DROPPING one table (#47 stage 4). The slot is
    /// replaced with a tombstone in place — the id is retired, never reused,
    /// `position == id` and every other table's id/data are untouched.
    pub fn with_dropped_table(&self, id: u32) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(id as usize)
            .filter(|t| t.id == id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {id} to drop")))?;
        *slot = TableDef::tombstone(id);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by ADDING a secondary index (CREATE INDEX). The
    /// caller builds the index tree over existing rows. `columns` are ordinals
    /// into the table's columns, in key order. Errors on an unknown column, an
    /// index-count overflow, or an identical index already present (the caller
    /// treats "already exists" as a no-op for idempotency / `IF NOT EXISTS`).
    pub fn with_added_index(&self, table_id: u32, index: IndexDef) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        for &c in &index.columns {
            if c as usize >= slot.columns.len() {
                return Err(Error::Schema(format!(
                    "CREATE INDEX on `{}`: column ordinal {c} out of range",
                    slot.name
                )));
            }
        }
        if slot
            .indexes
            .iter()
            .any(|ix| ix.columns == index.columns && ix.unique == index.unique)
        {
            return Err(Error::Schema(format!(
                "an identical index already exists on table `{}`",
                slot.name
            )));
        }
        slot.indexes.push(index);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by RENAMING a table (#47 stage 5). Pure metadata: the
    /// id, columns, keys, indexes, and all row data are untouched — only the
    /// name changes. `validate` rejects a collision with another live table.
    pub fn with_renamed_table(&self, id: u32, new_name: &str) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(id as usize)
            .filter(|t| t.id == id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {id} to rename")))?;
        slot.name = new_name.to_string();
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by APPENDING a column to a table (#47 stage 5). The
    /// new column takes the highest index, so existing column/index positions
    /// are untouched; the caller rewrites existing rows with the new column
    /// NULL. Errors on a name collision or an invalid merged schema (e.g. too
    /// many columns).
    pub fn with_added_column(&self, table_id: u32, col: ColumnDef) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        if slot.columns.iter().any(|c| c.name == col.name) {
            return Err(Error::Schema(format!(
                "column `{}` already exists in table `{}`",
                col.name, slot.name
            )));
        }
        slot.columns.push(col);
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by DROPPING one column of a table (#47 stage 5). The
    /// caller rewrites existing rows without the column. Refused when the column
    /// is part of the PK, referenced by any secondary index, or the table's last
    /// column (no online index rebuild, and a table needs its key). Column
    /// indices of surviving columns AFTER the dropped one shift down by one, so
    /// the PK and every index's stored column references are renumbered to match.
    pub fn with_dropped_column(&self, table_id: u32, column: &str) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        let idx = slot
            .columns
            .iter()
            .position(|c| c.name == column)
            .ok_or_else(|| Error::Schema(format!("no column `{column}` in table `{}`", slot.name)))?;
        let i = idx as u16;
        if slot.primary_key.contains(&i) {
            return Err(Error::Schema(format!(
                "cannot drop column `{column}`: it is part of the PRIMARY KEY of `{}`",
                slot.name
            )));
        }
        if slot.indexes.iter().any(|ix| ix.columns.contains(&i)) {
            return Err(Error::Schema(format!(
                "cannot drop column `{column}`: it is part of an index/UNIQUE on `{}`",
                slot.name
            )));
        }
        if slot.columns.len() == 1 {
            return Err(Error::Schema(format!(
                "cannot drop the last column of table `{}`",
                slot.name
            )));
        }
        // A generated column's compiled program addresses its inputs by ORDINAL,
        // and a drop shifts every ordinal after it — a program renumbered wrong
        // reads a DIFFERENT column and silently stores a different value, which
        // is the one failure mode this feature must not have. Renumbering the
        // program is mechanical but the expression SOURCE text (see
        // `with_renamed_column`) cannot follow, so the whole shape is refused.
        // sqlite refuses the interesting half of this too ("error in table t
        // after drop column"); mpedb refuses the rest rather than answer wrong.
        if slot.has_generated() {
            return Err(Error::Schema(format!(
                "cannot drop a column of `{}`: it has a generated column whose \
                 expression addresses its inputs by position, and a drop shifts them",
                slot.name
            )));
        }
        slot.columns.remove(idx);
        // Renumber references to columns that shifted down (index > i → -1).
        let shift = |c: &mut u16| {
            if *c > i {
                *c -= 1;
            }
        };
        slot.primary_key.iter_mut().for_each(shift);
        for ix in &mut slot.indexes {
            ix.columns.iter_mut().for_each(shift);
        }
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Evolve this schema by RENAMING one column of a table (#47 stage 5). Pure
    /// metadata: the column keeps its position and type, so no row image is
    /// touched. Errors if the column is unknown or the new name collides with a
    /// sibling column.
    pub fn with_renamed_column(
        &self,
        table_id: u32,
        column: &str,
        new_name: &str,
    ) -> Result<Schema> {
        let mut tables = self.tables.clone();
        let slot = tables
            .get_mut(table_id as usize)
            .filter(|t| t.id == table_id && !t.dead)
            .ok_or_else(|| Error::Schema(format!("no live table with id {table_id}")))?;
        if slot.columns.iter().any(|c| c.name == new_name) {
            return Err(Error::Schema(format!(
                "column `{new_name}` already exists in table `{}`",
                slot.name
            )));
        }
        // A generated column's EXPRESSION names its inputs in SOURCE text. The
        // compiled program reads ordinals, so a rename would keep computing the
        // right value — but the source (the DDL a dump replays, the text an
        // error message quotes) would still name the old column, and a replayed
        // dump would then fail. sqlite rewrites the text; mpedb has no
        // expression printer, so it refuses by name instead of shipping a
        // schema whose declared form and behaviour disagree.
        if slot.has_generated() {
            return Err(Error::Schema(format!(
                "cannot rename a column of `{}`: it has a generated column, whose \
                 expression names its inputs as text that mpedb cannot rewrite",
                slot.name
            )));
        }
        let col = slot
            .columns
            .iter_mut()
            .find(|c| c.name == column)
            .ok_or_else(|| {
                Error::Schema(format!("no column `{column}` in table `{}`", slot.name))
            })?;
        col.name = new_name.to_string();
        let schema = Schema { tables };
        schema.validate()?;
        Ok(schema)
    }

    /// Live (non-tombstone) tables — the user-visible set.
    pub fn live_tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.iter().filter(|t| !t.dead)
    }

    fn validate(&self) -> Result<()> {
        // LIVE tables must exist (a schema of only tombstones is meaningless);
        // the LIVE count carries the system-table headroom guard. The total
        // (live + dead) is bounded by MAX_TABLES — dead slots hold an id.
        let live = self.tables.iter().filter(|t| !t.dead).count();
        if live == 0 {
            return Err(Error::Schema("schema defines no live tables".into()));
        }
        if live > MAX_TABLES - 8 {
            return Err(Error::Schema(format!(
                "too many tables ({live} > {})",
                MAX_TABLES - 8 // headroom for system tables
            )));
        }
        if self.tables.len() > MAX_TABLES {
            return Err(Error::Schema("table-id space exhausted".into()));
        }
        // Duplicate LIVE names (dead slots have empty names, excluded). Set-
        // based, NOT windows(2): the vec is id-sorted, not name-sorted.
        let mut names: Vec<&str> = self.tables.iter().filter(|t| !t.dead).map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::Schema("duplicate table name".into()));
        }
        // DENSE ids: position == id is ENFORCED so every positional engine
        // site stays correct. A DROP tombstones IN PLACE (keeps the slot), so
        // this holds under drops too — a genuinely gapped vec is corrupt.
        for (pos, t) in self.tables.iter().enumerate() {
            if t.id != pos as u32 {
                return Err(Error::Schema(format!(
                    "table `{}` has id {} at position {pos}: ids must be dense 0..n",
                    t.name, t.id
                )));
            }
        }
        for t in &self.tables {
            // A tombstone holds no data: it MUST be empty, and its shape rules
            // are skipped (it has no name/columns/pk to validate).
            if t.dead {
                if !t.name.is_empty()
                    || !t.columns.is_empty()
                    || !t.primary_key.is_empty()
                    || !t.indexes.is_empty()
                    || t.kind != TableKind::Standard
                    || t.implicit_rowid
                {
                    return Err(Error::Schema(format!(
                        "tombstone slot id {} must be empty (no name/columns/pk/indexes/kind)",
                        t.id
                    )));
                }
                continue;
            }
            if !valid_identifier(&t.name) {
                return Err(Error::Schema(format!("invalid table name `{}`", t.name)));
            }
            if t.columns.is_empty() || t.columns.len() > MAX_COLUMNS {
                return Err(Error::Schema(format!(
                    "table `{}` must have 1..={MAX_COLUMNS} columns",
                    t.name
                )));
            }
            let mut names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
            names.sort_unstable();
            if names.windows(2).any(|w| w[0] == w[1]) {
                return Err(Error::Schema(format!("duplicate column in `{}`", t.name)));
            }
            for c in &t.columns {
                if !valid_identifier(&c.name) {
                    return Err(Error::Schema(format!(
                        "invalid column name `{}.{}`",
                        t.name, c.name
                    )));
                }
                if let Some(DefaultExpr::Const(v)) = &c.default {
                    if !v.fits(c.ty) {
                        return Err(Error::Schema(format!(
                            "default for `{}.{}` has type {}, column is {}",
                            t.name,
                            c.name,
                            v.type_name(),
                            c.ty
                        )));
                    }
                    if v.is_null() && !c.nullable {
                        return Err(Error::Schema(format!(
                            "NULL default on NOT NULL column `{}.{}`",
                            t.name, c.name
                        )));
                    }
                }
                // An `Any` column may carry ANY affinity — it is the per-value
                // column, so it can hold whatever the conversion produces, and
                // the affinity is the only thing that distinguishes
                // `decimal(10,2)` from a column with no declared type at all.
                // A RIGID column's affinity must be exactly the one its type
                // already enforces: mpedb refuses a mismatched value there
                // rather than converting it, so any other affinity would be a
                // rule nothing applies — a second source of truth about what
                // the column does.
                let implied = Affinity::implied_by(c.ty);
                if c.ty != ColumnType::Any && c.affinity != implied {
                    return Err(Error::Schema(format!(
                        "column `{}.{}` is {} with {} affinity: a rigid column \
                         refuses a mismatched value instead of converting it, \
                         so its affinity can only be {}",
                        t.name, c.name, c.ty, c.affinity, implied
                    )));
                }
                if matches!(&c.default, Some(DefaultExpr::Now)) && c.ty != ColumnType::Timestamp {
                    return Err(Error::Schema(format!(
                        "now() default requires timestamp column, `{}.{}` is {}",
                        t.name, c.name, c.ty
                    )));
                }
            }
            // GENERATED columns. Every rule here is what makes
            // `TableDef::apply_generated`'s single left-to-right pass sound and
            // panic-free on a HOSTILE mapping — the program's column ordinals
            // come off the wire, so they are re-checked here, not trusted.
            for (i, c) in t.columns.iter().enumerate() {
                let Some(g) = &c.generated else { continue };
                if c.default.is_some() {
                    return Err(Error::Schema(format!(
                        "generated column `{}.{}` cannot also have a DEFAULT",
                        t.name, c.name
                    )));
                }
                if t.primary_key.contains(&(i as u16)) {
                    return Err(Error::Schema(format!(
                        "generated column `{}.{}` cannot be part of the PRIMARY KEY",
                        t.name, c.name
                    )));
                }
                if g.program.has_host_call() {
                    return Err(Error::Schema(format!(
                        "generated column `{}.{}` calls a host-registered function: the \
                         expression is stored in the schema and every writer must be able \
                         to evaluate it, so a connection-local UDF cannot appear in one",
                        t.name, c.name
                    )));
                }
                for instr in &g.program.instrs {
                    match *instr {
                        // A generated expression is evaluated per ROW, with no
                        // statement to take parameters from.
                        Instr::PushParam(_) => {
                            return Err(Error::Schema(format!(
                                "generated column `{}.{}` references a parameter",
                                t.name, c.name
                            )))
                        }
                        Instr::PushCol(ci) => {
                            let src = t.columns.get(ci as usize).ok_or_else(|| {
                                Error::Schema(format!(
                                    "generated column `{}.{}` reads column {ci}, out of range",
                                    t.name, c.name
                                ))
                            })?;
                            // Self- and FORWARD references. sqlite resolves
                            // forward ones and only rejects true loops; mpedb
                            // refuses both, because declaration order is then a
                            // topological order and one left-to-right pass is
                            // provably correct. A refusal at CREATE TABLE, never
                            // a stale value in a row.
                            if src.generated.is_some() && ci as usize >= i {
                                return Err(Error::Schema(format!(
                                    "generated column `{}.{}` reads generated column `{}`, \
                                     which is declared at or after it: mpedb evaluates \
                                     generated columns in declaration order, so a generated \
                                     column may only read ones declared before it",
                                    t.name, c.name, src.name
                                )));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if t.primary_key.is_empty() {
                return Err(Error::Schema(format!(
                    "table `{}` has no primary key",
                    t.name
                )));
            }
            let mut pk = t.primary_key.clone();
            pk.sort_unstable();
            if pk.windows(2).any(|w| w[0] == w[1]) {
                return Err(Error::Schema(format!(
                    "duplicate primary key column in `{}`",
                    t.name
                )));
            }
            for &i in &t.primary_key {
                let c = t.columns.get(i as usize).ok_or_else(|| {
                    Error::Schema(format!("primary key index {i} out of range in `{}`", t.name))
                })?;
                if c.nullable {
                    return Err(Error::Schema(format!(
                        "primary key column `{}.{}` must be NOT NULL",
                        t.name, c.name
                    )));
                }
                // `any` IS allowed here. See `ANY_KEY_COLUMNS` below.
            }
            // The authoritative index list (canonical-bytes v2). The flag
            // check above is defense for hand-built defs; THIS is the check
            // every decode path must pass.
            if t.indexes.len() > MAX_INDEXES {
                return Err(Error::Schema(format!(
                    "table `{}` has {} indexes (max {MAX_INDEXES})",
                    t.name,
                    t.indexes.len()
                )));
            }
            for ix in &t.indexes {
                if ix.columns.is_empty() {
                    return Err(Error::Schema(format!(
                        "empty index column list in `{}`",
                        t.name
                    )));
                }
                let mut cols = ix.columns.clone();
                cols.sort_unstable();
                if cols.windows(2).any(|w| w[0] == w[1]) {
                    return Err(Error::Schema(format!(
                        "duplicate column in an index on `{}`",
                        t.name
                    )));
                }
                for &ci in &ix.columns {
                    t.columns.get(ci as usize).ok_or_else(|| {
                        Error::Schema(format!(
                            "index column ordinal {ci} out of range in `{}`",
                            t.name
                        ))
                    })?;
                    // `any` IS allowed here. See `ANY_KEY_COLUMNS` below.
                }
                if ix.columns.len() == 1
                    && t.primary_key.len() == 1
                    && t.primary_key[0] == ix.columns[0]
                {
                    return Err(Error::Schema(format!(
                        "index on `{}` duplicates the primary key tree (index 0)",
                        t.name
                    )));
                }
            }
            for i in 0..t.indexes.len() {
                for j in i + 1..t.indexes.len() {
                    if t.indexes[i] == t.indexes[j] {
                        return Err(Error::Schema(format!(
                            "duplicate index shape on `{}`",
                            t.name
                        )));
                    }
                }
            }
            // A DECLARED collating sequence (`COLLATE NOCASE`/`RTRIM`) is only
            // meaningful for TEXT. On a PRIMARY KEY or indexed column the engine
            // folds the value under the collation before it enters the keycode
            // tree (`encode_key_collated`), so a collated UNIQUE/index/PK is
            // fully supported: two values equal under the collation share one
            // on-disk key, and `=`/prefix probes fold identically. (Inequality
            // RANGE access over a collated key column stays out of the keycode
            // tree — the planner routes it to a scan with a collation-correct
            // residual filter — since a raw bytewise bound could skip a row.)
            // This is the single chokepoint every path funnels through — CREATE
            // TABLE, ALTER, CREATE INDEX, config, and a hostile v6 blob alike.
            for c in &t.columns {
                if c.collation != Collation::Binary && c.ty != ColumnType::Text {
                    return Err(Error::Schema(format!(
                        "column `{}.{}`: COLLATE {} may only be declared on a text column \
                         (collation affects text comparison only)",
                        t.name,
                        c.name,
                        c.collation.name()
                    )));
                }
            }
            // An FTS content table is stored like any table, but its shape is
            // fixed (design/DESIGN-FTS.md §1): a single INTEGER `rowid` primary
            // key, and NO ordinary secondary indexes — the inverted index lives
            // in a reserved tree, not `TableDef.indexes`. Every declared column
            // is FTS content and must be TEXT (the only tokenizable type).
            if t.kind.is_fts() {
                if t.primary_key.len() != 1
                    || t.columns[t.primary_key[0] as usize].ty != ColumnType::Int64
                {
                    return Err(Error::Schema(format!(
                        "FTS table `{}` must have a single INTEGER rowid primary key",
                        t.name
                    )));
                }
                if !t.indexes.is_empty() {
                    return Err(Error::Schema(format!(
                        "FTS table `{}` must not declare secondary indexes",
                        t.name
                    )));
                }
                for (i, c) in t.columns.iter().enumerate() {
                    if i as u16 == t.primary_key[0] {
                        continue;
                    }
                    if c.ty != ColumnType::Text {
                        return Err(Error::Schema(format!(
                            "FTS table `{}` column `{}` must be text (FTS content columns are \
                             tokenized text)",
                            t.name, c.name
                        )));
                    }
                }
            }
            // A hidden implicit rowid (#94) is a well-defined shape: an ordinary
            // (non-FTS) table whose LAST column is the sole PRIMARY KEY, an
            // Int64 named `rowid`, NOT NULL. Enforced here so a hostile/corrupt
            // v5 blob that merely flips the bit cannot fabricate a table whose
            // `SELECT *` would hide an arbitrary column or whose auto-assign
            // would target a non-integer key.
            if t.implicit_rowid {
                if t.kind.is_fts() {
                    return Err(Error::Schema(format!(
                        "table `{}` cannot be both FTS and implicit-rowid",
                        t.name
                    )));
                }
                let last = (t.columns.len() - 1) as u16;
                let c = &t.columns[last as usize];
                if t.primary_key.as_slice() != [last]
                    || c.name != "rowid"
                    || c.ty != ColumnType::Int64
                    || c.nullable
                {
                    return Err(Error::Schema(format!(
                        "table `{}` has implicit_rowid set but its last column is not a \
                         NOT-NULL Int64 `rowid` sole primary key",
                        t.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Resolve a table NAME to its stable id. A LINEAR scan (≤ 64 tables):
    /// `Schema::tables` is sorted by id (creation order), not by name, once
    /// `CREATE TABLE` has appended out of name order — so a name binary
    /// search is wrong. Returns the table's stable `id`, which equals its
    /// position only while ids are dense (this window), but the id is the
    /// correct value to return regardless.
    pub fn table_id(&self, name: &str) -> Option<u32> {
        self.tables.iter().find(|t| t.name == name).map(|t| t.id)
    }

    pub fn table(&self, id: u32) -> Option<&TableDef> {
        // Dense ids in this window ⇒ position == id ⇒ O(1) index. (DROP's
        // audit revisits this for gapped ids.)
        self.tables.get(id as usize)
    }

    /// Canonical, deterministic serialization — the schema-hash preimage and
    /// the format stored in the database catalog (v2, DESIGN-SCHEMA-V2).
    /// The per-column `unique`/`indexed` flags are NOT serialized (bits 1–7
    /// written zero): `indexes` is the single source of truth on the wire,
    /// and decode reconstructs the in-memory convenience flags from it.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.push(9u8); // schema encoding version (v9: ColumnDef.generated)
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for t in &self.tables {
            buf.extend_from_slice(&t.id.to_le_bytes());
            buf.push(t.dead as u8); // tombstone marker; a dead slot's rest is empty
            // Table-kind discriminant (v4): 0 = Standard, 1 = FTS ‖ tokenizer.
            match t.kind {
                TableKind::Standard => buf.push(0),
                TableKind::Fts { tokenizer } => {
                    buf.push(1);
                    buf.push(tokenizer as u8);
                }
            }
            // Hidden implicit-rowid flag (v5, #94). Always 0 for a dead slot or
            // an FTS table (validate enforces it).
            buf.push(t.implicit_rowid as u8);
            write_str(&mut buf, &t.name);
            buf.extend_from_slice(&(t.columns.len() as u16).to_le_bytes());
            for c in &t.columns {
                write_str(&mut buf, &c.name);
                buf.push(c.ty as u8);
                buf.push(c.nullable as u8);
                // Declared collating sequence (v6). BINARY (0) for every column
                // that did not write `COLLATE`, so a plain schema's bytes grow by
                // exactly one zero byte per column.
                buf.push(c.collation as u8);
                // sqlite type affinity (v7). Pinned by `validate` to the one
                // `ty` implies except on an `Any` column, where `Numeric` vs
                // `Blob` is the store-time-conversion bit `ty` cannot carry.
                buf.push(c.affinity as u8);
                // Verbatim declared-type text (v8). Absent (0) for the config
                // path and synthetic tables, where the canonical name is the
                // answer — a plain schema's bytes grow by one zero per column.
                match &c.decl {
                    None => buf.push(0),
                    Some(d) => {
                        buf.push(1);
                        write_str(&mut buf, d);
                    }
                }
                match &c.default {
                    None => buf.push(0),
                    Some(DefaultExpr::Const(v)) => {
                        buf.push(1);
                        write_value(&mut buf, v);
                    }
                    Some(DefaultExpr::Now) => buf.push(2),
                }
                match &c.check {
                    None => buf.push(0),
                    Some(src) => {
                        buf.push(1);
                        write_str(&mut buf, src);
                    }
                }
                // GENERATED ALWAYS AS (…) (v9): source ‖ kind ‖ compiled
                // program. The COMPILED form is on the wire — see `GeneratedCol`
                // for why — so a decoded schema can evaluate the column without
                // the SQL layer.
                match &c.generated {
                    None => buf.push(0),
                    Some(g) => {
                        buf.push(1);
                        write_str(&mut buf, &g.expr);
                        buf.push(g.kind as u8);
                        g.program.encode_into(&mut buf);
                    }
                }
            }
            buf.extend_from_slice(&(t.primary_key.len() as u16).to_le_bytes());
            for &i in &t.primary_key {
                buf.extend_from_slice(&i.to_le_bytes());
            }
            buf.extend_from_slice(&(t.indexes.len() as u16).to_le_bytes());
            for ix in &t.indexes {
                buf.push(ix.unique as u8);
                buf.extend_from_slice(&(ix.columns.len() as u16).to_le_bytes());
                for &ci in &ix.columns {
                    buf.extend_from_slice(&ci.to_le_bytes());
                }
            }
        }
        buf
    }

    /// Parse [`canonical_bytes`] output (bounds-checked; used when attaching
    /// to an existing database to recover its schema from the catalog). Only
    /// version 5 is accepted — older files refuse loudly and are regenerated
    /// (DESIGN-SCHEMA-V2 §5; the project carries no migration burden).
    pub fn from_canonical_bytes(buf: &[u8]) -> Result<Schema> {
        let err = || Error::Corrupt("truncated schema".into());
        let mut pos = 0usize;
        let version = *buf.get(pos).ok_or_else(err)?;
        pos += 1;
        if version != 9 {
            return Err(Error::Corrupt(format!(
                "unknown schema version {version} (v1..v8 predate canonical-bytes v9 — \
                 regenerate or re-import)"
            )));
        }
        let ntables = read_u32(buf, &mut pos)? as usize;
        if ntables > MAX_TABLES {
            return Err(Error::Corrupt("table count out of range".into()));
        }
        // `.min(256)`: `ntables` comes from untrusted bytes and MAX_TABLES is
        // now 4096, so reserving it outright would let a corrupt count drive a
        // half-megabyte speculative allocation before the first field is read.
        let mut tables = Vec::with_capacity(ntables.min(256));
        for _ in 0..ntables {
            let id = read_u32(buf, &mut pos)?;
            let dead = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad table dead flag".into())),
            };
            pos += 1;
            let kind = match *buf.get(pos).ok_or_else(err)? {
                0 => {
                    pos += 1;
                    TableKind::Standard
                }
                1 => {
                    pos += 1;
                    let tok = crate::fts::Tokenizer::from_tag(*buf.get(pos).ok_or_else(err)?)
                        .ok_or_else(|| Error::Corrupt("bad fts tokenizer tag".into()))?;
                    pos += 1;
                    TableKind::Fts { tokenizer: tok }
                }
                _ => return Err(Error::Corrupt("bad table kind tag".into())),
            };
            let implicit_rowid = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad implicit_rowid flag".into())),
            };
            pos += 1;
            let name = read_str(buf, &mut pos)?;
            let ncols = read_u16(buf, &mut pos)? as usize;
            if ncols > MAX_COLUMNS {
                return Err(Error::Corrupt("column count out of range".into()));
            }
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let cname = read_str(buf, &mut pos)?;
                let ty = ColumnType::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column type".into()))?;
                pos += 1;
                // bits 1–7 are reserved-zero on write and IGNORED on read:
                // the index list is the only wire truth (design §1.5).
                let flags = *buf.get(pos).ok_or_else(err)?;
                pos += 1;
                // Declared collating sequence (v6).
                let collation = Collation::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column collation tag".into()))?;
                pos += 1;
                // sqlite type affinity (v7).
                let affinity = Affinity::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column affinity tag".into()))?;
                pos += 1;
                // Verbatim declared-type text (v8).
                let decl = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad column decl tag".into())),
                };
                let default = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(DefaultExpr::Const(read_value(buf, &mut pos)?))
                    }
                    2 => {
                        pos += 1;
                        Some(DefaultExpr::Now)
                    }
                    _ => return Err(Error::Corrupt("bad default tag".into())),
                };
                let check = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad check tag".into())),
                };
                // GENERATED ALWAYS AS (…) (v9).
                let generated = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        let expr = read_str(buf, &mut pos)?;
                        let kind = GeneratedKind::from_tag(*buf.get(pos).ok_or_else(err)?)
                            .ok_or_else(|| Error::Corrupt("bad generated kind tag".into()))?;
                        pos += 1;
                        let program = ExprProgram::decode(buf, &mut pos)?;
                        Some(GeneratedCol { expr, kind, program })
                    }
                    _ => return Err(Error::Corrupt("bad generated tag".into())),
                };
                columns.push(ColumnDef {
                    name: cname,
                    ty,
                    nullable: flags & 1 != 0,
                    unique: false,
                    indexed: false,
                    default,
                    check,
                    collation,
                    affinity,
                    decl,
                    generated,
                });
            }
            let npk = read_u16(buf, &mut pos)? as usize;
            if npk > ncols {
                return Err(Error::Corrupt("pk count out of range".into()));
            }
            let mut primary_key = Vec::with_capacity(npk);
            for _ in 0..npk {
                primary_key.push(read_u16(buf, &mut pos)?);
            }
            let nindexes = read_u16(buf, &mut pos)? as usize;
            if nindexes > MAX_INDEXES {
                return Err(Error::Corrupt("index count out of range".into()));
            }
            let mut indexes = Vec::with_capacity(nindexes);
            for _ in 0..nindexes {
                let unique = match *buf.get(pos).ok_or_else(err)? {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::Corrupt("bad index unique tag".into())),
                };
                pos += 1;
                let nic = read_u16(buf, &mut pos)? as usize;
                if nic > MAX_COLUMNS {
                    return Err(Error::Corrupt("index column count out of range".into()));
                }
                let mut cols = Vec::with_capacity(nic);
                for _ in 0..nic {
                    cols.push(read_u16(buf, &mut pos)?);
                }
                indexes.push(IndexDef { columns: cols, unique });
            }
            // Reconstruct the in-memory convenience flags from the index
            // list, in one place: a single-column index marks its column.
            for ix in &indexes {
                if let [ci] = ix.columns[..] {
                    if let Some(c) = columns.get_mut(ci as usize) {
                        if ix.unique {
                            c.unique = true;
                        } else {
                            c.indexed = true;
                        }
                    }
                }
            }
            tables.push(TableDef {
                id,
                name,
                columns,
                primary_key,
                indexes,
                dead,
                kind,
                implicit_rowid,
            });
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("trailing bytes in schema".into()));
        }
        // Re-validate: canonical bytes from a hostile/corrupt mapping must
        // still produce a schema every other invariant can rely on —
        // including the dense-id rule (position == id) that the engine's
        // positional caches depend on.
        let schema = Schema { tables };
        schema.validate().map_err(|e| match e {
            Error::Schema(m) => Error::Corrupt(format!("schema bytes invalid: {m}")),
            other => other,
        })?;
        Ok(schema)
    }

    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let raw = buf
        .get(*pos..*pos + 2)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let raw = buf
        .get(*pos..*pos + 4)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32(buf, pos)? as usize;
    if len > 1 << 20 {
        return Err(Error::Corrupt("string too long in schema".into()));
    }
    let raw = buf
        .get(*pos..*pos + len)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += len;
    String::from_utf8(raw.to_vec()).map_err(|_| Error::Corrupt("invalid utf-8 in schema".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Schema {
        Schema::new(vec![TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![
                ColumnDef { generated: None, decl: None,
                    name: "id".into(),
                    ty: ColumnType::Int64,
                    nullable: false,
                    unique: false,
                    indexed: false,
                    default: None,
                    check: None, collation: Collation::Binary,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                },
                ColumnDef { generated: None, decl: None,
                    name: "email".into(),
                    ty: ColumnType::Text,
                    nullable: false,
                    unique: true,
                    indexed: false,
                    default: None,
                    check: None, collation: Collation::Binary,
                    affinity: Affinity::implied_by(ColumnType::Text),
                },
                ColumnDef { generated: None, decl: None,
                    name: "age".into(),
                    ty: ColumnType::Int64,
                    nullable: true,
                    unique: false,
                    indexed: false,
                    default: Some(DefaultExpr::Const(Value::Int(0))),
                    check: Some("age >= 0 AND age < 200".into()),
                    collation: Collation::Binary,
                    affinity: Affinity::implied_by(ColumnType::Int64),
                },
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, kind: TableKind::Standard, implicit_rowid: false,
        }])
        .unwrap()
    }

    #[test]
    fn canonical_roundtrip_and_stable_hash() {
        let s = sample();
        let restored = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, restored);
        assert_eq!(s.hash(), restored.hash());
    }

    #[test]
    fn table_order_is_name_sorted_regardless_of_input_order() {
        let mk = |names: &[&str]| {
            Schema::new(
                names
                    .iter()
                    .map(|n| TableDef {
                        id: 0,
                        name: n.to_string(),
                        columns: vec![ColumnDef { generated: None, decl: None,
                            name: "id".into(),
                            ty: ColumnType::Int64,
                            nullable: false,
                            unique: false,
                            indexed: false,
                            default: None,
                            check: None, collation: Collation::Binary,
                            affinity: Affinity::implied_by(ColumnType::Int64),
                        }],
                        primary_key: vec![0],
                        indexes: vec![],
                        dead: false, kind: TableKind::Standard, implicit_rowid: false,
                    })
                    .collect(),
            )
            .unwrap()
        };
        assert_eq!(mk(&["b", "a"]).hash(), mk(&["a", "b"]).hash());
        assert_eq!(mk(&["b", "a"]).table_id("a"), Some(0));
    }

    #[test]
    fn rejects_bad_schemas() {
        // nullable PK
        let bad = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![ColumnDef { generated: None, decl: None,
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: Affinity::implied_by(ColumnType::Int64),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, kind: TableKind::Standard, implicit_rowid: false,
        }]);
        assert!(bad.is_err());
        // reserved prefix
        let bad = Schema::new(vec![TableDef {
            id: 0,
            name: "__mpedb_plans".into(),
            columns: vec![ColumnDef { generated: None, decl: None,
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None, collation: Collation::Binary,
                affinity: Affinity::implied_by(ColumnType::Int64),
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, kind: TableKind::Standard, implicit_rowid: false,
        }]);
        assert!(bad.is_err());
    }
    /// **The constraint that used to block `CREATE TABLE`, and its fix**
    /// (DESIGN-SCHEMA-V2). v1's table id WAS the name-sort position, so
    /// adding a table renumbered every table sorting after it — and the id
    /// is a key (`cat_tree_key`, CDC bitmaps, mirror families, every plan).
    /// v2 makes the id EXPLICIT in the canonical bytes; seeding still
    /// assigns name-sorted (deterministic under config reordering), but
    /// CREATE TABLE will APPEND at the next free id. In this format window
    /// ids must stay dense 0..n (`position == id` is enforced, which is
    /// what keeps every positional engine cache correct until DROP's audit).
    #[test]
    fn ids_are_dense_explicit_and_survive_the_wire() {
        let col = |n: &str| ColumnDef { generated: None, decl: None, name: n.into(), ty: ColumnType::Int64,
                affinity: Affinity::implied_by(ColumnType::Int64),
            nullable: false, unique: false, indexed: false, default: None, check: None, collation: Collation::Binary };
        let tbl = |n: &str| TableDef { id: 0, name: n.into(), columns: vec![col("id")],
            primary_key: vec![0], indexes: vec![], dead: false, kind: TableKind::Standard, implicit_rowid: false };

        let s = Schema::new(vec![tbl("orders"), tbl("users"), tbl("accounts")]).unwrap();
        let got: Vec<(&str, u32)> =
            s.tables.iter().map(|t| (t.name.as_str(), t.id)).collect();
        assert_eq!(got, vec![("accounts", 0), ("orders", 1), ("users", 2)]);

        // Explicit in the bytes: the id round-trips, it is not re-derived.
        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r);

        // Non-dense ids refuse in this window — a gapped file must never
        // reach the positional engine caches.
        let mut gapped = s.clone();
        gapped.tables[1].id = 5;
        gapped.tables[2].id = 6;
        let err = Schema::from_canonical_bytes(&gapped.canonical_bytes()).unwrap_err();
        assert!(format!("{err}").contains("dense"), "{err}");
    }

    #[test]
    fn indexes_derive_normalize_and_roundtrip() {
        let mut t = TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { generated: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    unique: true, indexed: true, default: None, check: None, collation: Collation::Binary },
                ColumnDef { generated: None, decl: None, name: "a".into(), ty: ColumnType::Int64, nullable: true,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    unique: true, indexed: true, default: None, check: None, collation: Collation::Binary },
                ColumnDef { generated: None, decl: None, name: "b".into(), ty: ColumnType::Text, nullable: true,
                        affinity: Affinity::implied_by(ColumnType::Text),
                    unique: false, indexed: true, default: None, check: None, collation: Collation::Binary },
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef { columns: vec![1, 2], unique: false }],
            dead: false, kind: TableKind::Standard, implicit_rowid: false,
        };
        // The single-PK column's flags are noise and must normalize away.
        t.columns[0].unique = true;
        let s = Schema::new(vec![t]).unwrap();
        let t = &s.tables[0];
        // Derived (declaration order) then explicit, with flags normalized:
        // `a` unique+indexed → ONE unique index; PK column contributes none.
        assert_eq!(
            t.indexes,
            vec![
                IndexDef { columns: vec![1], unique: true },
                IndexDef { columns: vec![2], unique: false },
                IndexDef { columns: vec![1, 2], unique: false },
            ]
        );
        assert!(!t.columns[0].unique && !t.columns[0].indexed);
        assert!(t.columns[1].unique && !t.columns[1].indexed);

        // Wire round-trip reconstructs the same flags and the same list.
        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r);
        assert_eq!(s.hash(), r.hash());
    }

    #[test]
    fn hostile_bytes_refuse_cleanly() {
        let col = |n: &str, ty| ColumnDef { generated: None, decl: None, name: n.into(), ty, nullable: true,
                affinity: Affinity::implied_by(ty),
            unique: false, indexed: false, default: None, check: None, collation: Collation::Binary };
        let base = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { generated: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    unique: false, indexed: false, default: None, check: None, collation: Collation::Binary },
                col("v", ColumnType::Any),
                col("w", ColumnType::Int64),
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef { columns: vec![2], unique: false }],
            dead: false, kind: TableKind::Standard, implicit_rowid: false,
        }])
        .unwrap();

        // An index over an `any` column is ACCEPTED now (`ANY_KEY_COLUMNS`):
        // such a tree is keyed by storage class, whose equality and order are
        // sqlite's, and the planner — not the schema — is what keeps it from
        // ever being probed.
        let mut ok = base.clone();
        ok.tables[0].indexes = vec![IndexDef { columns: vec![1], unique: false }];
        Schema::from_canonical_bytes(&ok.canonical_bytes()).unwrap();

        // Duplicate index shapes refuse.
        let mut evil = base.clone();
        evil.tables[0]
            .indexes
            .push(IndexDef { columns: vec![2], unique: false });
        assert!(Schema::from_canonical_bytes(&evil.canonical_bytes()).is_err());

        // An index equal to the whole single-column PK duplicates index 0.
        let mut evil = base.clone();
        evil.tables[0].indexes = vec![IndexDef { columns: vec![0], unique: true }];
        assert!(Schema::from_canonical_bytes(&evil.canonical_bytes()).is_err());

        // v1 bytes (version byte 1) refuse by name — no migration exists.
        let mut v1ish = base.canonical_bytes();
        v1ish[0] = 1;
        let err = Schema::from_canonical_bytes(&v1ish).unwrap_err();
        assert!(format!("{err}").contains("unknown schema version 1"), "{err}");

        // Truncation at EVERY offset yields Corrupt, never a panic.
        let bytes = base.canonical_bytes();
        for i in 0..bytes.len() {
            assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
        }
    }

    fn tbl(n: &str) -> TableDef {
        let col = |n: &str| ColumnDef { generated: None, decl: None,
            name: n.into(), ty: ColumnType::Int64, nullable: false,
            unique: false, indexed: false, default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        };
        TableDef { id: 0, name: n.into(), columns: vec![col("id")],
            primary_key: vec![0], indexes: vec![], dead: false, kind: TableKind::Standard, implicit_rowid: false }
    }

    #[test]
    fn identifier_rule_allows_what_quoting_exists_for() {
        // Everything sqlite 3.45.1 accepts and we can represent faithfully.
        for good in [
            "weird tbl",
            "tbl-with.punct!",
            "1st table",
            "tabell_æøå",
            "a\"b",
            "  padded  ",
            "   ", // whitespace-only: a footgun, not a hazard, and sqlite allows it
            "[bracketish]",
            "`tick`",
            &"a".repeat(MAX_IDENTIFIER_LEN),
            &format!("app_{}", "m".repeat(130)), // 134: Django's m2m through-table
        ] {
            assert!(valid_identifier(good), "should be valid: {good:?}");
            // …and it survives canonical bytes byte-for-byte.
            let sch = Schema::new(vec![tbl(good)]).unwrap();
            let back = Schema::from_canonical_bytes(&sch.canonical_bytes()).unwrap();
            assert_eq!(back.tables[0].name, good);
            assert_eq!(back, sch);
        }
        // What stays refused, and why (see `valid_identifier`).
        for bad in [
            "",                              // no identity in any surface
            "nul\u{0}name",                  // NUL truncates the C-API's const char*
            "line\nbreak",                   // breaks every line-oriented surface
            "carriage\rreturn",
            "tab\tsep",
            "del\u{7f}",
            "c1\u{85}",                       // C1 control
            "__mpedb_internal",              // reserved prefix
            &"a".repeat(MAX_IDENTIFIER_LEN + 1),
        ] {
            assert!(!valid_identifier(bad), "should be invalid: {bad:?}");
        }
        // A non-ASCII name is measured in BYTES, not chars: 128 × 2-byte chars
        // is 256 bytes and over the limit even though it is 128 characters.
        let wide = "æ".repeat(128);
        assert_eq!(wide.chars().count(), 128);
        assert!(!valid_identifier(&wide));
    }

    #[test]
    fn drop_tombstones_in_place_and_never_reuses_the_id() {
        // Seed {a,b,c} → ids {0,1,2} (name-sorted).
        let s = Schema::new(vec![tbl("a"), tbl("b"), tbl("c")]).unwrap();
        assert_eq!(s.table_id("b"), Some(1));

        // Drop the MIDDLE table (id 1). The slot is tombstoned in place:
        // position == id still holds, `b` is gone, `a`/`c` untouched.
        let s = s.with_dropped_table(1).unwrap();
        assert_eq!(s.tables.len(), 3, "vec does not shrink — dead slot stays");
        assert!(s.tables[1].dead && s.tables[1].name.is_empty());
        assert_eq!(s.table_id("b"), None, "dropped name no longer resolves");
        assert_eq!(s.table_id("a"), Some(0));
        assert_eq!(s.table_id("c"), Some(2));
        assert_eq!(s.live_tables().count(), 2);
        // Every slot's id equals its position (dense, dead included).
        for (pos, t) in s.tables.iter().enumerate() {
            assert_eq!(t.id, pos as u32);
        }

        // A new table takes the NEXT id (3 = tables.len()), NEVER the dropped
        // id 1 — the no-reuse guarantee, from the materialized dead slot.
        let s = s.with_added_table(tbl("d")).unwrap();
        assert_eq!(s.table_id("d"), Some(3));
        assert_eq!(s.tables.len(), 4);

        // Re-CREATE the dropped NAME: gets a fresh id (4), not the old 1.
        let s = s.with_added_table(tbl("b")).unwrap();
        assert_eq!(s.table_id("b"), Some(4));

        // Drop the HIGHEST id (4) — len unchanged, so the next mint is still
        // 5, not the just-freed 4.
        let s = s.with_dropped_table(4).unwrap();
        assert_eq!(s.tables.len(), 5);
        let s = s.with_added_table(tbl("e")).unwrap();
        assert_eq!(s.table_id("e"), Some(5));
    }

    #[test]
    fn tombstoned_schema_round_trips_through_v3_bytes() {
        let s = Schema::new(vec![tbl("a"), tbl("b"), tbl("c")]).unwrap();
        let s = s.with_dropped_table(1).unwrap(); // dead slot at 1
        let s = s.with_added_table(tbl("z")).unwrap(); // id 3
        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r, "dead slot + ids survive the wire byte-for-byte");
        assert_eq!(s.hash(), r.hash());
        // The version byte is 9.
        assert_eq!(s.canonical_bytes()[0], 9);
        // A v8 file refuses cleanly (no misread of the new generated bytes).
        let mut v8 = s.canonical_bytes();
        v8[0] = 8;
        let err = Schema::from_canonical_bytes(&v8).unwrap_err();
        assert!(format!("{err}").contains("unknown schema version 8"), "{err}");
    }

    #[test]
    fn hostile_tombstone_bytes_refuse() {
        // A "dead" slot that carries content is corrupt.
        let s = Schema::new(vec![tbl("a"), tbl("b")]).unwrap();
        let mut evil = s.with_dropped_table(1).unwrap();
        evil.tables[1].dead = true;
        evil.tables[1].name = "ghost".into(); // a dead slot must be empty
        let err = Schema::from_canonical_bytes(&evil.canonical_bytes()).unwrap_err();
        assert!(format!("{err}").contains("tombstone"), "{err}");
        // An all-tombstone schema (no live table) refuses.
        let mut none = Schema::new(vec![tbl("a")]).unwrap();
        none.tables[0] = TableDef::tombstone(0);
        assert!(Schema::from_canonical_bytes(&none.canonical_bytes()).is_err());
    }

    #[test]
    fn create_refuses_at_the_id_ceiling() {
        // Fill to MAX_TABLES with live + dead slots; the next create fails
        // closed rather than minting id >= MAX_TABLES. No-reuse
        // means DROP+CREATE churn grows `tables.len()` by one per cycle, so a
        // churny workload is what eventually reaches this bound — the
        // deliberate, bounded, detectable limit (§0), with offline `regenerate`
        // compaction as the escape hatch.
        let tables: Vec<TableDef> = (0..MAX_TABLES - 8).map(|i| tbl(&format!("t{i}"))).collect();
        let mut s = Schema::new(tables).unwrap();
        // Burn ids up to the ceiling via drop+recreate without exceeding the
        // live-count guard (dead slots accumulate; live count stays flat).
        while s.tables.len() < MAX_TABLES {
            let id = s.tables.iter().rposition(|t| !t.dead).unwrap() as u32;
            s = s.with_dropped_table(id).unwrap();
            if s.tables.len() < MAX_TABLES {
                s = s.with_added_table(tbl(&format!("r{}", s.tables.len()))).unwrap();
            }
        }
        assert_eq!(s.tables.len(), MAX_TABLES);
        let err = s.with_added_table(tbl("overflow")).unwrap_err();
        assert!(format!("{err}").contains("exhausted"), "{err}");
    }

    /// An implicit-rowid table (#94): visible columns plus a trailing hidden
    /// Int64 `rowid` sole PK. It round-trips byte-for-byte through v5, the flag
    /// survives, and the helpers report the right visible set.
    #[test]
    fn implicit_rowid_round_trips_and_helpers() {
        let col = |n: &str, ty| ColumnDef { generated: None, decl: None,
            name: n.into(), ty, nullable: true, unique: false, indexed: false,
            default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
        };
        let rowid = ColumnDef { generated: None, decl: None,
            name: "rowid".into(), ty: ColumnType::Int64, nullable: false,
            unique: false, indexed: false, default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        };
        let s = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![col("a", ColumnType::Any), col("b", ColumnType::Text), rowid],
            primary_key: vec![2],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: true,
        }])
        .unwrap();
        let t = &s.tables[0];
        assert_eq!(t.hidden_rowid_col(), Some(2));
        assert_eq!(t.visible_column_count(), 2);
        assert_eq!(
            t.visible_columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(t.rowid_name_col("rowid"), Some(2));
        assert_eq!(t.rowid_name_col("OID"), Some(2));
        assert_eq!(t.rowid_name_col("_rowid_"), Some(2));
        assert_eq!(t.rowid_name_col("a"), None);
        // The auto-assign machinery treats it as a rowid alias.
        assert_eq!(t.rowid_alias_col(), Some(2));

        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r);
        assert_eq!(s.hash(), r.hash());
        assert_eq!(s.canonical_bytes()[0], 9);

        // Truncation at every offset is Corrupt, never a panic.
        let bytes = s.canonical_bytes();
        for i in 0..bytes.len() {
            assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
        }
    }

    /// The VERBATIM declared-type text survives the v8 wire byte-for-byte, is
    /// what `decltype()` reports (canonical name only where there is no text),
    /// contributes to the hash, and truncation at every offset is Corrupt.
    #[test]
    fn column_decl_text_round_trips_and_is_hostile_safe() {
        let col = |n: &str, ty, decl: Option<&str>| ColumnDef { generated: None,
            name: n.into(),
            ty,
            nullable: true,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
            decl: decl.map(str::to_string),
        };
        let s = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { generated: None, nullable: false, ..col("id", ColumnType::Int64, Some("INTEGER")) },
                // `float` is the case the canonical name loses: mpedb stores
                // Float64, whose canonical spelling is REAL.
                col("f", ColumnType::Float64, Some("float")),
                // No declared type at all: sqlite reports NULL, and so must this.
                col("n", ColumnType::Any, None),
                // An unknown name is legal in sqlite and IS the decltype.
                col("x", ColumnType::Any, Some("number(5)")),
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .unwrap();
        let c = &s.tables[0].columns;
        assert_eq!(c[1].decltype(), Some("float"), "verbatim, not REAL");
        assert_eq!(c[2].decltype(), None, "no declared type ⇒ sqlite's NULL");
        assert_eq!(c[3].decltype(), Some("number(5)"));
        // A column with no text falls back to the canonical name.
        let mut plain = s.clone();
        plain.tables[0].columns[1].decl = None;
        assert_eq!(plain.tables[0].columns[1].decltype(), Some("REAL"));

        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r);
        assert_eq!(s.hash(), r.hash());
        assert_eq!(s.canonical_bytes()[0], 9);
        // The text is part of the schema identity: `f float` and `f REAL` are
        // the same storage and DIFFERENT schemas, because a consumer keying
        // converters off the decltype sees two different columns.
        assert_ne!(s.hash(), plain.hash());

        let bytes = s.canonical_bytes();
        for i in 0..bytes.len() {
            assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
        }
        // A hostile blob with an out-of-range decl tag refuses cleanly. Column
        // `n` encodes as `<len> "n" <ty> <flags> <collation> <affinity> <decl>`.
        let mut evil = bytes.clone();
        let np = evil.windows(5).position(|w| w == b"\x01\x00\x00\x00n").unwrap();
        let decl_at = np + 5 + 1 /*ty*/ + 1 /*flags*/ + 1 /*collation*/ + 1 /*affinity*/;
        assert_eq!(evil[decl_at], 0, "located the decl tag byte");
        evil[decl_at] = 0x7f;
        let err = Schema::from_canonical_bytes(&evil).unwrap_err();
        assert!(format!("{err}").contains("decl"), "{err}");
    }

    /// A DECLARED column collation (`COLLATE NOCASE`) survives the v6 wire
    /// byte-for-byte, contributes to the hash, and truncation at every offset is
    /// Corrupt — the roundtrip/truncation contract extended to the new field.
    #[test]
    fn column_collation_round_trips_and_is_hostile_safe() {
        let col = |n: &str, ty, coll| ColumnDef { generated: None, decl: None,
            name: n.into(), ty, nullable: true, unique: false, indexed: false,
            default: None, check: None, collation: coll,
            affinity: Affinity::implied_by(ty),
        };
        let id = ColumnDef { generated: None, decl: None,
            name: "id".into(), ty: ColumnType::Int64, nullable: false, unique: false,
            indexed: false, default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ColumnType::Int64),
        };
        let s = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            // A NOCASE and an RTRIM text column, neither indexed (a collated key
            // is refused — see `collated_key_column_refused`).
            columns: vec![
                id,
                col("name", ColumnType::Text, Collation::NoCase),
                col("code", ColumnType::Text, Collation::Rtrim),
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .unwrap();
        assert_eq!(s.tables[0].columns[1].collation, Collation::NoCase);
        assert_eq!(s.tables[0].columns[2].collation, Collation::Rtrim);

        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r);
        assert_eq!(s.hash(), r.hash());
        assert_eq!(s.canonical_bytes()[0], 9);

        // The collation changes the hash: a BINARY `name` is a different schema.
        let mut plain = s.clone();
        plain.tables[0].columns[1].collation = Collation::Binary;
        assert_ne!(s.hash(), plain.hash());

        // Truncation at every offset is Corrupt, never a panic.
        let bytes = s.canonical_bytes();
        for i in 0..bytes.len() {
            assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
        }

        // A hostile v6 blob with an out-of-range collation tag refuses cleanly.
        // Column `name` encodes as `<len> "name" <ty> <nullable> <collation> …`,
        // so its collation byte is 6 bytes past the start of the literal "name".
        let mut evil = s.canonical_bytes();
        let np = evil.windows(4).position(|w| w == b"name").unwrap();
        let coll_at = np + 4 /*name*/ + 1 /*ty*/ + 1 /*nullable*/;
        assert_eq!(evil[coll_at], Collation::NoCase as u8, "located the collation byte");
        evil[coll_at] = 0x7f;
        let err = Schema::from_canonical_bytes(&evil).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("collation"), "{err}");
    }

    /// A non-BINARY collation on a key column (PRIMARY KEY / UNIQUE / index) is
    /// REFUSED — collated on-disk keys are deferred, never answered wrong. Also
    /// enforced on decode (a hostile v6 blob cannot smuggle one in).
    #[test]
    fn collated_key_column_accepted_but_non_text_refused() {
        let mk = |unique: bool| {
            Schema::new(vec![TableDef {
                id: 0,
                name: "t".into(),
                columns: vec![
                    ColumnDef { generated: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                        unique: false, indexed: false, default: None, check: None,
                            affinity: Affinity::implied_by(ColumnType::Int64),
                        collation: Collation::Binary },
                    ColumnDef { generated: None, decl: None, name: "name".into(), ty: ColumnType::Text, nullable: true,
                        unique, indexed: !unique, default: None, check: None,
                            affinity: Affinity::implied_by(ColumnType::Text),
                        collation: Collation::NoCase },
                ],
                primary_key: vec![0],
                indexes: vec![],
                dead: false,
                kind: TableKind::Standard,
                implicit_rowid: false,
            }])
        };
        // A UNIQUE and a plain index on a NOCASE column are now ACCEPTED (the
        // engine folds the value into the on-disk key).
        assert!(mk(true).is_ok());
        assert!(mk(false).is_ok());

        // A NOCASE PRIMARY KEY column is accepted too.
        assert!(Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![ColumnDef { generated: None, decl: None, name: "k".into(), ty: ColumnType::Text, nullable: false,
                unique: false, indexed: false, default: None, check: None,
                    affinity: Affinity::implied_by(ColumnType::Text),
                collation: Collation::NoCase }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .is_ok());

        // But a non-TEXT collation is still refused (collation affects text only).
        let err = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { generated: None, decl: None, name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    unique: false, indexed: false, default: None, check: None,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    collation: Collation::Binary },
                ColumnDef { generated: None, decl: None, name: "n".into(), ty: ColumnType::Int64, nullable: true,
                    unique: false, indexed: false, default: None, check: None,
                        affinity: Affinity::implied_by(ColumnType::Int64),
                    collation: Collation::NoCase },
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .unwrap_err();
        assert!(format!("{err}").contains("text"), "{err}");
    }

    /// A hostile v5 blob that merely flips `implicit_rowid` on a shape that is
    /// not a trailing NOT-NULL Int64 `rowid` sole PK must refuse — otherwise
    /// `SELECT *` would hide an arbitrary column.
    #[test]
    fn hostile_implicit_rowid_bytes_refuse() {
        let s = sample(); // explicit-PK `users`, last column is `age` (nullable Int64)
        let mut evil = s.clone();
        evil.tables[0].implicit_rowid = true;
        let err = Schema::from_canonical_bytes(&evil.canonical_bytes()).unwrap_err();
        assert!(format!("{err}").contains("implicit_rowid"), "{err}");
    }

    // ------------------------------------------------ generated columns (v9)

    /// A generated column survives the v9 wire — SOURCE, kind and COMPILED
    /// PROGRAM — contributes to the hash, and truncation at every offset is
    /// `Corrupt`, never a panic. The program is on the wire (unlike a CHECK,
    /// which is source-only) precisely so a decoded schema can evaluate the
    /// column with no SQL layer present.
    #[test]
    fn generated_column_round_trips_and_is_hostile_safe() {
        use crate::expr::Instr;
        let plain = |n: &str, ty| ColumnDef { generated: None, decl: None,
            name: n.into(), ty, nullable: true, unique: false, indexed: false,
            default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
        };
        // g = a + a, over column 0.
        let prog = ExprProgram::new(
            vec![Instr::PushCol(0), Instr::PushCol(0), Instr::Add],
            vec![],
        )
        .unwrap();
        let mut g = plain("g", ColumnType::Int64);
        g.generated = Some(GeneratedCol {
            expr: "a + a".into(),
            kind: GeneratedKind::Stored,
            program: prog,
        });
        let s = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { nullable: false, ..plain("a", ColumnType::Int64) },
                g,
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .unwrap();

        let r = Schema::from_canonical_bytes(&s.canonical_bytes()).unwrap();
        assert_eq!(s, r, "the compiled program survives byte-for-byte");
        assert_eq!(s.hash(), r.hash());
        assert_eq!(s.canonical_bytes()[0], 9);

        // It computes, in declaration order, through the decoded schema.
        let mut row = vec![Value::Int(21), Value::Null];
        r.tables[0].apply_generated(&mut row, &[]).unwrap();
        assert_eq!(row[1], Value::Int(42));
        assert!(r.tables[0].has_generated());

        let bytes = s.canonical_bytes();
        for i in 0..bytes.len() {
            assert!(Schema::from_canonical_bytes(&bytes[..i]).is_err(), "offset {i}");
        }
    }

    /// The rules that make one declaration-order pass sound. Each is checked on
    /// a HOSTILE hand-built schema, i.e. on the decode path, not just at DDL.
    #[test]
    fn generated_column_validation_rules() {
        use crate::expr::Instr;
        let plain = |n: &str, ty| ColumnDef { generated: None, decl: None,
            name: n.into(), ty, nullable: true, unique: false, indexed: false,
            default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
        };
        let gen_reading = |n: &str, col: u16, kind| {
            let mut c = plain(n, ColumnType::Int64);
            c.generated = Some(GeneratedCol {
                expr: format!("col{col}"),
                kind,
                program: ExprProgram::new(vec![Instr::PushCol(col)], vec![]).unwrap(),
            });
            c
        };
        let build = |cols: Vec<ColumnDef>, pk: Vec<u16>| {
            Schema::new(vec![TableDef {
                id: 0,
                name: "t".into(),
                columns: cols,
                primary_key: pk,
                indexes: vec![],
                dead: false,
                kind: TableKind::Standard,
                implicit_rowid: false,
            }])
        };
        let a = || ColumnDef { nullable: false, ..plain("a", ColumnType::Int64) };

        // Reading an EARLIER generated column is fine.
        assert!(build(
            vec![a(), gen_reading("g", 0, GeneratedKind::Stored), gen_reading("h", 1, GeneratedKind::Stored)],
            vec![0]
        )
        .is_ok());

        // Reading a LATER one, or ITSELF, is refused — the two shapes that
        // would make a single declaration-order pass read a stale value.
        for (cols, what) in [
            (vec![a(), gen_reading("g", 2, GeneratedKind::Stored), gen_reading("h", 0, GeneratedKind::Stored)], "forward"),
            (vec![a(), gen_reading("g", 1, GeneratedKind::Stored)], "self"),
        ] {
            let err = build(cols, vec![0]).unwrap_err();
            assert!(
                format!("{err}").contains("declared at or after it"),
                "{what}: {err}"
            );
        }

        // Out-of-range ordinal (a corrupt mapping) is refused, not a panic.
        let err = build(vec![a(), gen_reading("g", 99, GeneratedKind::Virtual)], vec![0]).unwrap_err();
        assert!(format!("{err}").contains("out of range"), "{err}");

        // A parameter has nothing to bind to per row.
        let mut p = plain("g", ColumnType::Int64);
        p.generated = Some(GeneratedCol {
            expr: "$1".into(),
            kind: GeneratedKind::Virtual,
            program: ExprProgram::new(vec![Instr::PushParam(0)], vec![]).unwrap(),
        });
        let err = build(vec![a(), p], vec![0]).unwrap_err();
        assert!(format!("{err}").contains("references a parameter"), "{err}");

        // PRIMARY KEY and DEFAULT are both refused on a generated column.
        let err = build(vec![a(), ColumnDef { nullable: false, ..gen_reading("g", 0, GeneratedKind::Stored) }], vec![0, 1])
            .unwrap_err();
        assert!(format!("{err}").contains("PRIMARY KEY"), "{err}");
        let mut d = gen_reading("g", 0, GeneratedKind::Stored);
        d.default = Some(DefaultExpr::Const(Value::Int(1)));
        let err = build(vec![a(), d], vec![0]).unwrap_err();
        assert!(format!("{err}").contains("DEFAULT"), "{err}");
    }

    /// DROP COLUMN shifts ordinals and RENAME COLUMN invalidates the expression
    /// TEXT; a generated column's program addresses inputs by position, so both
    /// are refused on a table that has one. Narrower than sqlite, never wrong.
    #[test]
    fn drop_and_rename_column_refuse_on_a_generated_table() {
        use crate::expr::Instr;
        let plain = |n: &str, ty, nullable| ColumnDef { generated: None, decl: None,
            name: n.into(), ty, nullable, unique: false, indexed: false,
            default: None, check: None, collation: Collation::Binary,
            affinity: Affinity::implied_by(ty),
        };
        let mut g = plain("g", ColumnType::Int64, true);
        g.generated = Some(GeneratedCol {
            expr: "a".into(),
            kind: GeneratedKind::Stored,
            program: ExprProgram::new(vec![Instr::PushCol(0)], vec![]).unwrap(),
        });
        let s = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                plain("a", ColumnType::Int64, false),
                plain("b", ColumnType::Int64, true),
                g,
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
            kind: TableKind::Standard,
            implicit_rowid: false,
        }])
        .unwrap();

        let err = s.with_dropped_column(0, "b").unwrap_err();
        assert!(format!("{err}").contains("by position"), "{err}");
        let err = s.with_renamed_column(0, "a", "z").unwrap_err();
        assert!(format!("{err}").contains("cannot rewrite"), "{err}");
    }
}
