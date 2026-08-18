use crate::error::{Error, Result};
use crate::expr::{ExprProgram, Instr};
use crate::ident::{fold_ident, ident_eq};
use crate::value::{read_value, write_value, Affinity, Collation, ColumnType, Value};
use crate::{MAX_COLUMNS, MAX_TABLES};

mod canonical;
mod evolve;
mod validate;

#[cfg(test)]
mod tests;

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
    /// sqlite's three time keywords in DEFAULT position: `CURRENT_TIMESTAMP`,
    /// `CURRENT_DATE`, `CURRENT_TIME`. Filled in by the engine from the same
    /// clock [`DefaultExpr::Now`] uses, rendered as TEXT in sqlite's spelling
    /// (`YYYY-MM-DD HH:MM:SS`, `YYYY-MM-DD`, `HH:MM:SS`, all UTC).
    ///
    /// Distinct from `Now`, which is a `Timestamp` VALUE on a timestamp column
    /// — these are text, on a column of any type, because that is what sqlite
    /// stores and what a consumer reflecting the DDL then reads back.
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    /// `DEFAULT ( <expr> )` whose value depends on the STATEMENT INSTANT —
    /// `DEFAULT (STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))`, which is Django's
    /// `auto_now_add`. Stored and evaluated per INSERT rather than folded,
    /// because there is no one value to fold it to.
    ///
    /// The COMPILED form is on the wire, exactly as a generated column's is, so
    /// a decoded schema can evaluate the default without the SQL layer. The
    /// instant is parameter slot 0: a default takes no user parameters, which
    /// is what lets the executor evaluate it with a one-element array.
    /// BOXED: an `ExprProgram` carries its instruction vector and constant
    /// pool, which would make `DefaultExpr` — and through it every column spec
    /// and DDL statement — large for a variant that most columns never use.
    Expr(Box<DefaultProgram>),
}

/// The body of an instant-dependent expression default.
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultProgram {
    /// The DDL source, kept for diagnostics and for the schema hash.
    pub src: String,
    /// The compiled form, on the wire so a decoded schema can evaluate the
    /// default without the SQL layer — exactly as a generated column's is.
    pub program: crate::ExprProgram,
}

impl DefaultExpr {
    /// The DDL spelling of a time-keyword default, or `None` for the others.
    pub fn time_keyword(&self) -> Option<&'static str> {
        match self {
            DefaultExpr::CurrentTimestamp => Some("CURRENT_TIMESTAMP"),
            DefaultExpr::CurrentDate => Some("CURRENT_DATE"),
            DefaultExpr::CurrentTime => Some("CURRENT_TIME"),
            _ => None,
        }
    }
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
    /// The DEFAULT's DDL TEXT, exactly as written — what sqlite's
    /// `PRAGMA table_info` reports in `dflt_value` (canonical bytes v15).
    ///
    /// Kept rather than derived because the stored VALUE cannot reproduce it:
    /// `DEFAULT 1` and `DEFAULT true` on a BOOLEAN column fold to one value and
    /// sqlite prints them differently, and `DEFAULT (3+5)` prints as `3+5`, not
    /// `8`. A schema built from TOML has no DDL text and leaves this `None`,
    /// which reports as NULL — the same answer sqlite gives for no default.
    pub default_text: Option<String>,
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

/// Does a column of this shape CONVERT a value on the way in (sqlite's
/// store-time affinity) rather than type-check it as it stands? The SINGLE
/// place the gate lives — [`ColumnDef::converts_on_store`] is this function
/// with the fields filled in, and callers holding a `(type, affinity)` pair
/// before a [`ColumnDef`] exists (the DDL path converting a `DEFAULT`) reach it
/// through [`store_into`].
///
/// `declared` is the PROVENANCE bit: true when the column's type came from
/// `CREATE TABLE` text ([`ColumnDef::decl`] is `Some`), false for a
/// config-declared column and for the synthetic catalog tables. It is what
/// keeps the rigid schema rigid where rigidity is the product: `type = "text"`
/// in a TOML config still REFUSES `5`, while `name text` in a shim
/// `CREATE TABLE` stores `'5'` exactly as sqlite does (task #113).
///
/// Three outcomes:
/// * BLOB affinity converts nothing, ever — sqlite's typeless column.
/// * [`ColumnType::Any`] applies its affinity whatever the provenance: it can
///   hold whatever the conversion produces, and the affinity is the only thing
///   that says what the column does.
/// * a RIGID `Int64`/`Float64`/`Text` applies it only when DECLARED, and the
///   type check that follows still refuses whatever the conversion could not
///   land inside the type (`'abc'` into an `int` column). Narrower than sqlite,
///   never a different answer. `Bool`/`Timestamp`/`Blob` never convert: they
///   exist only on the config path, where rigidity is the contract.
pub fn converts_on_store(ty: ColumnType, affinity: Affinity, declared: bool) -> bool {
    if affinity == Affinity::Blob {
        return false;
    }
    match ty {
        ColumnType::Any => true,
        ColumnType::Int64 | ColumnType::Float64 | ColumnType::Text => declared,
        // `Date`/`Time`/`Numeric` convert too, but NOT through affinity — see
        // [`coerce_into`]. A literal in SQL has no type of its own in
        // PostgreSQL either (`'2020-01-02'` is `unknown` until a column claims
        // it), so this is that same coercion and not sqlite's guessing.
        ColumnType::Date | ColumnType::Time | ColumnType::Numeric => declared,
        ColumnType::Bool | ColumnType::Timestamp | ColumnType::Blob => false,
    }
}

/// Can `affinity` change a value of this STORAGE CLASS at all? The per-value
/// half of [`TableDef::needs_store_affinity`] — deliberately class-only, so it
/// cannot drift from [`crate::expr::store_affinity`]'s actual rules by more
/// than a wasted copy: every class the conversion can touch answers `true`.
fn affinity_can_change(affinity: Affinity, v: &Value) -> bool {
    match affinity {
        // The typeless column converts nothing.
        Affinity::Blob => false,
        // Renders a number as text; a text/blob/bool/timestamp is left alone.
        Affinity::Text => matches!(v, Value::Int(_) | Value::Float(_)),
        // `applyNumericAffinity`: parses a fully-numeric string, and demotes a
        // real to an integer when the round trip is lossless.
        Affinity::Integer | Affinity::Numeric => matches!(v, Value::Text(_) | Value::Float(_)),
        // The same, then promotes the integer back to a real — so a real is
        // already fixed and only text/integers move.
        Affinity::Real => matches!(v, Value::Text(_) | Value::Int(_)),
    }
}

/// The per-value guard, dispatching on the column's TYPE first.
///
/// For `Date`/`Time`/`Numeric` the conversion is [`coerce_into`], not affinity,
/// so asking `affinity_can_change` would answer about the wrong rule — and it
/// would answer `false` for the case that matters: a TEXT literal into a
/// `Numeric` column, whose affinity is `Text` and whose `Text` arm only moves
/// numbers. Everything else keeps sqlite's class check unchanged.
fn value_can_change(ty: ColumnType, affinity: Affinity, v: &Value) -> bool {
    match ty {
        ColumnType::Date | ColumnType::Time | ColumnType::Numeric => {
            !v.is_null() && v.column_type() != Some(ty)
        }
        _ => affinity_can_change(affinity, v),
    }
}

/// [`converts_on_store`] applied: the value as this column stores it.
pub fn store_into(ty: ColumnType, affinity: Affinity, declared: bool, v: Value) -> Value {
    if !converts_on_store(ty, affinity, declared) {
        return v;
    }
    match ty {
        ColumnType::Date | ColumnType::Time | ColumnType::Numeric => coerce_into(ty, v),
        _ => crate::expr::store_affinity(affinity, v),
    }
}

/// A value on its way into a `Date`, `Time` or `Numeric` column.
///
/// This is the SAME conversion `CAST` performs under the PostgreSQL dialect —
/// literally [`crate::expr::cast_typed`], not a second copy of its rules —
/// because PostgreSQL treats them as one thing: a literal has no type until
/// something claims it, and a `timestamp` written into a `date` column is
/// truncated on assignment exactly as `::date` truncates it.
///
/// The difference is only what happens on failure. A cast RAISES; this leaves
/// the value untouched for the type check that follows, which refuses it by
/// name ("value of type text cannot be inserted into column `d` of type
/// date"). So this can never turn a refusal into a wrong answer — the worst it
/// does is nothing.
fn coerce_into(ty: ColumnType, v: Value) -> Value {
    crate::expr::cast_typed(&v, ty).unwrap_or(v)
}

impl ColumnDef {
    /// Whether a value is CONVERTED on the way into this column (sqlite's
    /// store-time affinity) rather than type-checked as it stands — see the
    /// free function [`converts_on_store`] for the rule and why `decl` is the
    /// provenance bit that decides it.
    pub fn converts_on_store(&self) -> bool {
        converts_on_store(self.ty, self.affinity, self.decl.is_some())
    }

    /// The value as this column stores it: its store-time affinity applied
    /// where [`ColumnDef::converts_on_store`] says it applies.
    pub fn store(&self, v: Value) -> Value {
        store_into(self.ty, self.affinity, self.decl.is_some(), v)
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
    /// Partial-index predicate source (`CREATE INDEX … WHERE <pred>`, P1 /
    /// design/DESIGN-WORKLOAD-INDEXES.md §5). `None` is a whole-table index.
    /// The text is re-bound at plan/build time against the table's columns;
    /// parameterized predicates are refused until P6 (`AccessPath::Guarded`).
    /// On the wire since canonical-bytes **v10**.
    pub predicate: Option<String>,
    /// The name the index was created with (`CREATE INDEX <name> ON …`), or
    /// `None` for one derived from a column flag (`unique = true` /
    /// `indexed = true` in the config), which never had a name to keep.
    ///
    /// mpedb indexes are POSITIONAL — `index_no` is the position in
    /// `TableDef::indexes` plus one, and that is what plans and B-trees key on.
    /// The name is carried alongside purely so a user can NAME one:
    /// `DROP INDEX <name>` and `REINDEX <name>` are unanswerable without it,
    /// and both were corpus failures for exactly that reason (`DROP INDEX`
    /// did not parse at all; `REINDEX <typo>` was accepted where sqlite
    /// errors, because nothing could tell a real index name from a typo).
    ///
    /// On the wire since canonical-bytes **v11**.
    pub name: Option<String>,
    /// EXPRESSION key parts (`CREATE INDEX i ON t (LOWER(a), b)`), on the wire
    /// since canonical-bytes **v13**. Empty for every plain-column index, which
    /// is every index that existed before v13; otherwise the same length as
    /// `columns`, entry `i` holding the SQL SOURCE of key part `i` or `None`
    /// where that part is the plain column `columns[i]`.
    ///
    /// An expression part sets `columns[i]` to [`INDEX_EXPR_COL`]. It has to
    /// mean something, and no real ordinal can: an expression may read two
    /// columns, or none. So `columns` stops describing that part rather than
    /// lying about it — which also matches how sqlite reports one, as
    /// `PRAGMA index_info` column id `-2` (measured, 3.45.1).
    ///
    /// Membership follows the ordinary rule on the EVALUATED part: a NULL means
    /// no entry. sqlite instead stores the entry and lets NULLs not collide —
    /// the same observable answer for a UNIQUE index, and the difference is
    /// unobservable here because an index with an expression part is never
    /// chosen for ACCESS. Matching a query's expression against a stored one is
    /// a separate problem; the consumer that needs these (Django's schema
    /// editor) creates them, introspects them by name and drops them.
    pub exprs: Vec<Option<String>>,
    /// Per-key-part COLLATE override (`CREATE INDEX i ON t (a COLLATE NOCASE)`),
    /// on the wire since canonical-bytes **v14**. Empty when every part keys by
    /// its column's own collation, which is every index before v14; otherwise
    /// the same length as `columns`, `None` where the part has no override.
    ///
    /// This is NOT an expression part: MEASURED at sqlite 3.45.1,
    /// `a COLLATE NOCASE` reports as `index_xinfo` column id 1 named `a` with
    /// coll NOCASE — a COLUMN whose comparison changed — and a duplicate under
    /// it names the column (`UNIQUE constraint failed: t.a`), not the index.
    /// It changes how the key is ENCODED, not what value it holds.
    pub collations: Vec<Option<crate::value::Collation>>,
    /// Whether this index EXISTS BECAUSE OF A CONSTRAINT — a table-level
    /// `UNIQUE (…)`, a named `CONSTRAINT u UNIQUE (…)`, or the column flag that
    /// spells the same thing — rather than because someone wrote
    /// `CREATE UNIQUE INDEX`. On the wire since canonical-bytes **v20**.
    ///
    /// mpedb enforces both the same way, with one B-tree, so this bit changes
    /// nothing about storage or planning. What it changes is what the catalog
    /// can SAY: PostgreSQL reports a constraint-backed unique index in BOTH
    /// `pg_constraint` and `pg_index`, and a bare `CREATE UNIQUE INDEX` in
    /// `pg_index` ONLY. Without the bit every unique index answered as a
    /// constraint, so a reflecting client re-created `Index(unique=True)` as a
    /// `UniqueConstraint` — a wrong answer that no error reports.
    ///
    /// Always `false` when `unique` is false: a non-unique index is never a
    /// constraint.
    pub from_constraint: bool,
}

/// `IndexDef::columns[i]` for a key part that is an EXPRESSION, not a column.
/// A table can never have this many columns (`MAX_COLUMNS` is far below), so
/// the sentinel cannot collide with a real ordinal.
pub const INDEX_EXPR_COL: u16 = u16::MAX;

impl IndexDef {
    /// Does any key part of this index compute a value rather than read a
    /// column?
    ///
    /// Such an index is **never an access path**: its key holds a computed
    /// value, and matching a query's expression against a stored one is a
    /// problem this planner does not solve. `secondary_indexes` has said so
    /// since v13 — but the planner also enumerates `table.indexes` DIRECTLY in
    /// six places, and each of those indexed `table.columns` with
    /// [`INDEX_EXPR_COL`] and PANICKED.
    ///
    /// Found by the PostgreSQL regress differential, which is full of
    /// expression indexes; the sqlite corpus has none, so nothing had ever
    /// reached those lines with the sentinel. Both spellings are checked —
    /// a non-empty `exprs` and the ordinal sentinel — because a schema decoded
    /// from bytes is untrusted and may carry one without the other.
    pub fn has_expression_part(&self) -> bool {
        !self.exprs.iter().all(Option::is_none)
            || self.columns.contains(&INDEX_EXPR_COL)
    }
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
    Fts { tokenizer: crate::fts::Tokenizer, module: crate::fts::FtsModule },
}

impl TableKind {
    pub fn is_fts(self) -> bool {
        matches!(self, TableKind::Fts { .. })
    }
    pub fn fts_tokenizer(self) -> Option<crate::fts::Tokenizer> {
        match self {
            TableKind::Fts { tokenizer, .. } => Some(tokenizer),
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
    /// `INTEGER PRIMARY KEY AUTOINCREMENT` — an id is NEVER reused, even after
    /// the top row is deleted. Without it, mpedb (like sqlite) assigns
    /// `max(rowid) + 1`, so deleting the highest row hands its id to the next
    /// insert. The promise costs a PERSISTED high-water mark per table, written
    /// in the same transaction as the row it hands an id to (canonical bytes
    /// v17) — which is the whole of what the keyword adds and the whole of what
    /// it costs.
    pub autoincrement: bool,
    /// `CONSTRAINT <name> PRIMARY KEY (…)` — the name the PRIMARY KEY constraint
    /// was declared with, on the wire since canonical-bytes **v20**.
    ///
    /// `None` for every key declared without one, which reports PostgreSQL's
    /// derived `<table>_pkey` at the catalog surface rather than storing a name
    /// nobody wrote. Kept for the same reason a UNIQUE constraint's name is
    /// (`IndexDef::name`): a reflecting client reads it back and re-creates the
    /// table with it, so dropping it renamed the author's constraint.
    pub pk_name: Option<String>,
    /// FOREIGN KEYs declared on this table, in declaration order. Empty for
    /// almost every table — and the write path's first question is
    /// `is_empty()`, so a table without one pays NOTHING for the feature.
    ///
    /// On the wire since canonical-bytes **v12**.
    pub foreign_keys: Vec<ForeignKeyDef>,
}

/// What a FOREIGN KEY does when the row it points at moves or goes away.
///
/// All five of sqlite's names are kept, because a schema that declares
/// `ON DELETE CASCADE` and silently gets `NO ACTION` is worse than one that
/// refuses to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FkAction {
    /// The default. For an IMMEDIATE key this is indistinguishable from
    /// `Restrict` — both refuse — but they are stored apart so the deferred
    /// mode does not have to re-derive which the author wrote.
    #[default]
    NoAction,
    /// Refuse while any child row still references the parent.
    Restrict,
    /// Delete the children too (`ON DELETE`), or carry the new key into them
    /// (`ON UPDATE`).
    Cascade,
    /// Set the child's referencing columns to NULL.
    SetNull,
    /// Set them to their column DEFAULT (NULL when none is declared).
    SetDefault,
}

impl FkAction {
    pub fn tag(self) -> u8 {
        match self {
            FkAction::NoAction => 0,
            FkAction::Restrict => 1,
            FkAction::Cascade => 2,
            FkAction::SetNull => 3,
            FkAction::SetDefault => 4,
        }
    }
    /// `None` (→ `Corrupt`) for an unknown byte, like every other wire tag here.
    pub fn from_tag(t: u8) -> Option<FkAction> {
        Some(match t {
            0 => FkAction::NoAction,
            1 => FkAction::Restrict,
            2 => FkAction::Cascade,
            3 => FkAction::SetNull,
            4 => FkAction::SetDefault,
            _ => return None,
        })
    }
}

/// One FOREIGN KEY: this table's `columns` must match a live row of `parent`.
///
/// `REFERENCES` was parsed and DISCARDED until 2026-07-29 — and that was not a
/// shrug. sqlite's own default is `PRAGMA foreign_keys = OFF`, under which
/// sqlite ALSO parses a foreign key and enforces nothing, so parse-and-drop was
/// sqlite's default behaviour exactly. It was never a wrong answer; it just
/// meant mpedb had no `ON` to offer.
///
/// # Why the parent is a NAME and not a table id
///
/// Everything else the catalog stores is resolved to ordinals at DDL time, and
/// the first draft of this type did the same. It is wrong, and sqlite says so
/// (measured, 3.45.1):
///
/// ```text
/// CREATE TABLE c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES par(id));  -- OK
/// CREATE TABLE par (id INTEGER PRIMARY KEY);                              -- OK
/// INSERT INTO c VALUES (2, 5);                                            -- now checked
/// ```
///
/// A forward reference is LEGAL, and it is not exotic — a schema migration
/// that creates tables in dependency-free order (Django's, for one) relies on
/// it. Resolving at DDL time would mean refusing the first statement. So the
/// parent side stays in names and is resolved at WRITE time, which is also
/// where sqlite reports `no such table` and `foreign key mismatch`. The CHILD
/// columns are ordinals: that table is the one being defined, so a name that
/// does not resolve is a `CREATE TABLE` error there too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    /// Child column ordinals, in key order.
    pub columns: Vec<u16>,
    /// The referenced table's NAME (see the type docs). Case is preserved as
    /// written; resolution is case-insensitive, like every other table lookup.
    pub parent: String,
    /// Parent column NAMES, in the SAME key order. EMPTY means `REFERENCES t`
    /// with no list, which resolves to the parent's PRIMARY KEY — and the
    /// parent may not exist yet, so that resolution cannot happen here.
    pub parent_columns: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
    /// `DEFERRABLE INITIALLY DEFERRED` — checked at COMMIT rather than at the
    /// end of the statement.
    pub deferred: bool,
    /// The constraint's declared name, if it had one (`CONSTRAINT fk_x FOREIGN
    /// KEY …`). Carried for the error message, which is what a user greps.
    pub name: Option<String>,
}

impl TableDef {
    /// Apply `shift` to EVERY column ordinal this table holds.
    ///
    /// There are exactly three such lists — `primary_key`, each index's
    /// `columns`, and each FOREIGN KEY's `columns` — and they must move
    /// together. They did not: `with_dropped_column` renumbered the first two
    /// and left the third, so after an `ALTER TABLE … DROP COLUMN` the FK read
    /// a DIFFERENT column's value. That is not a cosmetic pragma bug —
    /// enforcement silently stopped rejecting orphans (`fk.rs` addresses rows
    /// by ordinal: `key_of(row, &fk.columns)`), which is a wrong answer with
    /// nothing in the output to hint at it.
    ///
    /// `parent_columns` is deliberately absent: it holds NAMES, and they belong
    /// to the parent table, which this evolution does not touch.
    fn renumber_columns(&mut self, shift: impl Fn(&mut u16)) {
        self.primary_key.iter_mut().for_each(&shift);
        for ix in &mut self.indexes {
            // [`INDEX_EXPR_COL`] is NOT an ordinal — it marks a key part that is
            // an expression, so there is no column position for it to move to.
            // Shifting it was an arithmetic overflow on `u16::MAX + 1`, i.e. a
            // PANIC on any ALTER TABLE ADD COLUMN against a table carrying an
            // expression index. Skipping it is the only meaning the sentinel
            // has: it stays exactly what it was.
            for c in ix.columns.iter_mut() {
                if *c != INDEX_EXPR_COL {
                    shift(c);
                }
            }
        }
        for fk in &mut self.foreign_keys {
            fk.columns.iter_mut().for_each(&shift);
        }
        // A generated column's COMPILED program is a fourth ordinal list, and
        // it shifts for exactly the same reason. `Instr::PushCol` is the only
        // instruction that touches the row, which is what makes this complete
        // rather than a best effort. The `AS (…)` SOURCE beside it needs no
        // rewrite: it names its inputs, and neither adding nor dropping a
        // column renames anything.
        for c in &mut self.columns {
            let Some(g) = &mut c.generated else { continue };
            for ins in &mut g.program.instrs {
                if let Instr::PushCol(x) = ins {
                    shift(x);
                }
            }
        }
    }

    /// The tombstone that replaces a dropped table's slot (#47 stage 4). Keeps
    /// the id, frees the name for re-CREATE, holds no data.
    pub fn tombstone(id: u32) -> TableDef {
        TableDef {
            id,
            name: String::new(),
            pk_name: None,
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            dead: true,
            kind: TableKind::Standard,
            implicit_rowid: false,
            autoincrement: false,
            foreign_keys: Vec::new(),
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

    /// Could [`TableDef::apply_store_affinity`] change THIS row — i.e. is any
    /// value in a class its column's affinity actually converts?
    ///
    /// [`TableDef::converts_on_store`] is a property of the schema and, since
    /// task #113, true of nearly every table built by `CREATE TABLE` (a `text`
    /// column carries TEXT affinity). Taking the copy on that alone would cost
    /// every insert into every shim table the zero-copy row (#40) — including
    /// the 16 MiB blob writes that work exists for. This is the per-ROW guard:
    /// a conservative class check (it may answer `true` where the conversion
    /// turns out to be the identity, never `false` where it is not), so the
    /// common case — values already in their column's class — stays borrowed.
    pub fn needs_store_affinity(&self, row: &[Value]) -> bool {
        row.iter()
            .zip(&self.columns)
            .any(|(v, c)| c.converts_on_store() && value_can_change(c.ty, c.affinity, v))
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
                *v = c.store(old);
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
            row[i] = c.store(v);
        }
        Ok(())
    }

    /// Resolve a column NAME to its ordinal. ASCII-case-insensitive, sqlite's
    /// rule and regardless of quoting ([`crate::ident`]) — this is THE column
    /// chokepoint, so `SELECT ABC FROM t` finds the column declared `Abc`.
    /// What comes back is an ordinal, never a name: callers that report a
    /// column label read `columns[i].name`, i.e. the DECLARED spelling, which
    /// is what sqlite reports too.
    pub fn column_index(&self, name: &str) -> Option<u16> {
        self.columns.iter().position(|c| ident_eq(&c.name, name)).map(|i| i as u16)
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
        // Fts is eligible too: an fts5 table's rowid IS a rowid alias in
        // sqlite — `INSERT … (NULL, …)` and an omitted rowid both auto-assign,
        // and `last_insert_rowid()` reports it. Excluding the kind here is why
        // an fts INSERT could never auto-assign. (Virtual stays out: those
        // tables have no storage to assign into.)
        if !matches!(self.kind, TableKind::Standard | TableKind::Fts { .. }) {
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
        let lc = name.to_ascii_lowercase();
        if !(lc == "rowid" || lc == "_rowid_" || lc == "oid") {
            return None;
        }
        if let Some(hidden) = self.hidden_rowid_col() {
            return Some(hidden);
        }
        // sqlite: a declared INTEGER PRIMARY KEY column IS the rowid, and the
        // alias spellings resolve to it (diskcache's `DELETE … WHERE rowid IN`
        // runs against exactly such a table). A single int64 PK is that
        // table shape here; TEXT/composite PKs are the WITHOUT-ROWID analog,
        // where sqlite refuses the name too — so absence stays a refusal.
        if self.primary_key.len() == 1 {
            let pk = self.primary_key[0];
            if self.columns[pk as usize].ty == ColumnType::Int64 {
                return Some(pk);
            }
        }
        None
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
        .map(|(i, c)| IndexDef {
            collations: Vec::new(),
            columns: vec![i as u16],
            unique: c.unique,
            predicate: None,
            // Derived from a column flag: there never was a name to keep.
            exprs: Vec::new(),
            name: None,
            // `x TEXT UNIQUE` IS a constraint — in PostgreSQL and in sqlite
            // both, the column flag is shorthand for a table-level `UNIQUE (x)`
            // — so it answers as one. `indexed = true` is not, and cannot be:
            // it produces a non-unique index.
            from_constraint: c.unique,
        })
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

    /// Live (non-tombstone) tables — the user-visible set.
    pub fn live_tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.iter().filter(|t| !t.dead)
    }

    /// Resolve a table NAME to its stable id. A LINEAR scan (≤ 64 tables):
    /// `Schema::tables` is sorted by id (creation order), not by name, once
    /// `CREATE TABLE` has appended out of name order — so a name binary
    /// search is wrong. Returns the table's stable `id`, which equals its
    /// position only while ids are dense (this window), but the id is the
    /// correct value to return regardless.
    /// ASCII-case-insensitive, sqlite's rule and regardless of quoting
    /// ([`crate::ident`]): this is THE table chokepoint, so `FROM T` finds the
    /// table created as `t`. Dead (tombstoned) slots carry an empty name and so
    /// never match a valid identifier.
    pub fn table_id(&self, name: &str) -> Option<u32> {
        self.tables.iter().find(|t| !t.dead && ident_eq(&t.name, name)).map(|t| t.id)
    }

    pub fn table(&self, id: u32) -> Option<&TableDef> {
        // Dense ids in this window ⇒ position == id ⇒ O(1) index. (DROP's
        // audit revisits this for gapped ids.)
        self.tables.get(id as usize)
    }
}
