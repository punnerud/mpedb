use crate::error::{Error, Result};
use crate::value::{read_value, write_value, ColumnType, Value};
use crate::{MAX_COLUMNS, MAX_TABLES};

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

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    /// Stable table id (DESIGN-SCHEMA-V2): explicit in the canonical bytes,
    /// stable for the table's life, allocated lowest-free (always
    /// `< MAX_TABLES`, which the footprint/CDC bitmaps require). In the
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
        }
    }
}

impl TableDef {
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
}

/// A validated schema. Tables are sorted by name; a table's id is its index
/// in `tables` (stable because attach requires an identical schema hash).
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub tables: Vec<TableDef>,
}

fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with("__mpedb")
}

/// Upper bound on secondary indexes per table (canonical-bytes v2).
pub const MAX_INDEXES: usize = 32;

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
        // is the deliberate trade). Fail closed at the bitmap ceiling
        // (footprint/CDC index by raw id).
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
                {
                    return Err(Error::Schema(format!(
                        "tombstone slot id {} must be empty (no name/columns/pk/indexes)",
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
                if matches!(&c.default, Some(DefaultExpr::Now)) && c.ty != ColumnType::Timestamp {
                    return Err(Error::Schema(format!(
                        "now() default requires timestamp column, `{}.{}` is {}",
                        t.name, c.name, c.ty
                    )));
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
                if c.ty == ColumnType::Any {
                    return Err(Error::Schema(format!(
                        "primary key column `{}.{}` cannot be `any`: a key is \
                         memcmp-ordered, and ordering across types would mean \
                         inventing whether 5 sorts before \"a\" — declare the \
                         column's real type",
                        t.name, c.name
                    )));
                }
            }
            // Same reasoning for EVERY secondary index, unique or not: its keys
            // are encoded with `keycode` too, so it needs an order across the
            // column's values — and `any` has none. A non-unique index over
            // `any` slipped through here once, and the adversarial review
            // showed what that means: the memcmp order across mixed runtime
            // types is arbitrary, so an IndexRange returned WRONG rows — and
            // DELETE/UPDATE through it deleted them.
            for c in &t.columns {
                if (c.unique || c.indexed) && c.ty == ColumnType::Any {
                    return Err(Error::Schema(format!(
                        "column `{}.{}` cannot be `any` and carry an index \
                         ({}): the index is memcmp-ordered and `any` has no \
                         order across types",
                        t.name,
                        c.name,
                        if c.unique { "UNIQUE" } else { "indexed" }
                    )));
                }
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
                    let c = t.columns.get(ci as usize).ok_or_else(|| {
                        Error::Schema(format!(
                            "index column ordinal {ci} out of range in `{}`",
                            t.name
                        ))
                    })?;
                    // Same reasoning as the PK/flag rules: index keys are
                    // keycode-encoded, and `any` has no order across types —
                    // a review-built v2 blob with an `any` index would
                    // resurrect the wrong-rows/wrong-DELETE bug.
                    if c.ty == ColumnType::Any {
                        return Err(Error::Schema(format!(
                            "index column `{}.{}` cannot be `any`: the index \
                             is memcmp-ordered and `any` has no order across \
                             types",
                            t.name, c.name
                        )));
                    }
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
        buf.push(3u8); // schema encoding version (v3: TableDef.dead, #47 stage 4)
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for t in &self.tables {
            buf.extend_from_slice(&t.id.to_le_bytes());
            buf.push(t.dead as u8); // tombstone marker; a dead slot's rest is empty
            write_str(&mut buf, &t.name);
            buf.extend_from_slice(&(t.columns.len() as u16).to_le_bytes());
            for c in &t.columns {
                write_str(&mut buf, &c.name);
                buf.push(c.ty as u8);
                buf.push(c.nullable as u8);
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
    /// version 3 is accepted — older files refuse loudly and are regenerated
    /// (DESIGN-SCHEMA-V2 §5; the project carries no migration burden).
    pub fn from_canonical_bytes(buf: &[u8]) -> Result<Schema> {
        let err = || Error::Corrupt("truncated schema".into());
        let mut pos = 0usize;
        let version = *buf.get(pos).ok_or_else(err)?;
        pos += 1;
        if version != 3 {
            return Err(Error::Corrupt(format!(
                "unknown schema version {version} (v1/v2 predate canonical-bytes v3 — \
                 regenerate or re-import)"
            )));
        }
        let ntables = read_u32(buf, &mut pos)? as usize;
        if ntables > MAX_TABLES {
            return Err(Error::Corrupt("table count out of range".into()));
        }
        let mut tables = Vec::with_capacity(ntables);
        for _ in 0..ntables {
            let id = read_u32(buf, &mut pos)?;
            let dead = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad table dead flag".into())),
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
                columns.push(ColumnDef {
                    name: cname,
                    ty,
                    nullable: flags & 1 != 0,
                    unique: false,
                    indexed: false,
                    default,
                    check,
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
                ColumnDef {
                    name: "id".into(),
                    ty: ColumnType::Int64,
                    nullable: false,
                    unique: false,
                    indexed: false,
                    default: None,
                    check: None,
                },
                ColumnDef {
                    name: "email".into(),
                    ty: ColumnType::Text,
                    nullable: false,
                    unique: true,
                    indexed: false,
                    default: None,
                    check: None,
                },
                ColumnDef {
                    name: "age".into(),
                    ty: ColumnType::Int64,
                    nullable: true,
                    unique: false,
                    indexed: false,
                    default: Some(DefaultExpr::Const(Value::Int(0))),
                    check: Some("age >= 0 AND age < 200".into()),
                },
            ],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
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
                        columns: vec![ColumnDef {
                            name: "id".into(),
                            ty: ColumnType::Int64,
                            nullable: false,
                            unique: false,
                            indexed: false,
                            default: None,
                            check: None,
                        }],
                        primary_key: vec![0],
                        indexes: vec![],
                        dead: false,
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
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
        }]);
        assert!(bad.is_err());
        // reserved prefix
        let bad = Schema::new(vec![TableDef {
            id: 0,
            name: "__mpedb_plans".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false,
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
        let col = |n: &str| ColumnDef { name: n.into(), ty: ColumnType::Int64,
            nullable: false, unique: false, indexed: false, default: None, check: None };
        let tbl = |n: &str| TableDef { id: 0, name: n.into(), columns: vec![col("id")],
            primary_key: vec![0], indexes: vec![], dead: false };

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
                ColumnDef { name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    unique: true, indexed: true, default: None, check: None },
                ColumnDef { name: "a".into(), ty: ColumnType::Int64, nullable: true,
                    unique: true, indexed: true, default: None, check: None },
                ColumnDef { name: "b".into(), ty: ColumnType::Text, nullable: true,
                    unique: false, indexed: true, default: None, check: None },
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef { columns: vec![1, 2], unique: false }],
            dead: false,
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
        let col = |n: &str, ty| ColumnDef { name: n.into(), ty, nullable: true,
            unique: false, indexed: false, default: None, check: None };
        let base = Schema::new(vec![TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                ColumnDef { name: "id".into(), ty: ColumnType::Int64, nullable: false,
                    unique: false, indexed: false, default: None, check: None },
                col("v", ColumnType::Any),
                col("w", ColumnType::Int64),
            ],
            primary_key: vec![0],
            indexes: vec![IndexDef { columns: vec![2], unique: false }],
            dead: false,
        }])
        .unwrap();

        // An index over an `any` column would resurrect the documented
        // wrong-rows/wrong-DELETE memcmp bug — decode must refuse it even
        // though the bytes are structurally well-formed.
        let mut evil = base.clone();
        evil.tables[0].indexes = vec![IndexDef { columns: vec![1], unique: false }];
        let err = Schema::from_canonical_bytes(&evil.canonical_bytes()).unwrap_err();
        assert!(format!("{err}").contains("any"), "{err}");

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
        let col = |n: &str| ColumnDef {
            name: n.into(), ty: ColumnType::Int64, nullable: false,
            unique: false, indexed: false, default: None, check: None,
        };
        TableDef { id: 0, name: n.into(), columns: vec![col("id")],
            primary_key: vec![0], indexes: vec![], dead: false }
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
        // The version byte is 3.
        assert_eq!(s.canonical_bytes()[0], 3);
        // A v2 file refuses cleanly (no misread of the new dead flag).
        let mut v2 = s.canonical_bytes();
        v2[0] = 2;
        let err = Schema::from_canonical_bytes(&v2).unwrap_err();
        assert!(format!("{err}").contains("unknown schema version 2"), "{err}");
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
        // closed rather than minting id >= 64 (the bitmap ceiling). No-reuse
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
}
