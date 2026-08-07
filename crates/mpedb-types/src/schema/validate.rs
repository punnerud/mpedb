use super::*;

impl Schema {
    pub(super) fn validate(&self) -> Result<()> {
        // ZERO live tables is a legal seed: live DDL (#47) grows the catalog
        // past the seed, so "open empty, CREATE TABLE as you go" is the
        // simplest setup a config can express. The LIVE count carries the
        // system-table headroom guard. The total (live + dead) is bounded by
        // MAX_TABLES — dead slots hold an id.
        let live = self.tables.iter().filter(|t| !t.dead).count();
        // The CEILING here, not the configured cap. `validate` runs on every
        // schema LOAD, including of a file written by a process configured
        // differently — refusing it against THIS process's `max_tables` would
        // make a legitimate file unreadable because of a setting that is not
        // the file's. The mint is where a config bound belongs, and that is
        // where it is (`with_added_table_capped`).
        if live > crate::MAX_TABLES_CEILING - 8 {
            return Err(Error::Schema(format!(
                "too many tables ({live} > {})",
                MAX_TABLES - 8 // headroom for system tables
            )));
        }
        if self.tables.len() > crate::MAX_TABLES_CEILING {
            return Err(Error::Schema("table-id space exhausted".into()));
        }
        // Duplicate LIVE names (dead slots have empty names, excluded). Set-
        // based, NOT windows(2): the vec is id-sorted, not name-sorted.
        // Compared ASCII-CASE-INSENSITIVELY: `t` and `T` are one name, so
        // creating both is `duplicate table name`, exactly as sqlite says
        // `table T already exists`. Sorting on the FOLDED key (the names
        // themselves stay verbatim — folding is for lookup only).
        let mut names: Vec<String> =
            self.tables.iter().filter(|t| !t.dead).map(|t| fold_ident(&t.name)).collect();
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
            // Also ASCII-case-insensitive: sqlite refuses `CREATE TABLE t(a, A)`
            // with `duplicate column name: A`, and it refuses it for the QUOTED
            // spelling `("a", "A")` too — quoting buys distinct spellings, never
            // distinct case. Folding lookups without folding THIS check would
            // leave two columns that `column_index` cannot tell apart.
            let mut names: Vec<String> = t.columns.iter().map(|c| fold_ident(&c.name)).collect();
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
                // Duplicate COLUMNS are a malformed index. Two EXPRESSION
                // parts are not duplicates just because they share the sentinel
                // — `(LOWER(a), UPPER(a))` is two different keys — so they are
                // compared by SOURCE instead, which is what actually identifies
                // them.
                let mut cols: Vec<u16> = ix
                    .columns
                    .iter()
                    .copied()
                    .filter(|&c| c != INDEX_EXPR_COL)
                    .collect();
                cols.sort_unstable();
                if cols.windows(2).any(|w| w[0] == w[1]) {
                    return Err(Error::Schema(format!(
                        "duplicate column in an index on `{}`",
                        t.name
                    )));
                }
                let mut srcs: Vec<&String> = ix.exprs.iter().flatten().collect();
                srcs.sort_unstable();
                if srcs.windows(2).any(|w| w[0] == w[1]) {
                    return Err(Error::Schema(format!(
                        "duplicate expression in an index on `{}`",
                        t.name
                    )));
                }
                for &ci in &ix.columns {
                    // The expression-part sentinel names no column (v13).
                    if ci == INDEX_EXPR_COL {
                        continue;
                    }
                    t.columns.get(ci as usize).ok_or_else(|| {
                        Error::Schema(format!(
                            "index column ordinal {ci} out of range in `{}`",
                            t.name
                        ))
                    })?;
                    // `any` IS allowed here. See `ANY_KEY_COLUMNS` below.
                }
                // An index whose columns ARE the primary key used to be refused
                // here as a duplicate of the PK tree. It is redundant — the PK
                // tree already answers every probe it could — but redundant is
                // not illegal, and sqlite builds it. Django's
                // `remove_unique_together` on a pk (or unique) field emits
                // exactly this, so the refusal blocked a migration that has
                // nothing wrong with it.
                //
                // Allowed rather than made a no-op ON PURPOSE: an index entry
                // that carries no tree would be a new concept for the write
                // path, the planner, fsck and the verifier to agree about, and
                // that is how a special case becomes a bug. Building the tree
                // costs exactly what sqlite costs and adds no new mechanism.
                //
                // (The PARTIAL case was already carved out for the same
                // reason and stays: `UNIQUE(pk) WHERE …` holds only the rows
                // its predicate admits, so it answers a question the PK tree
                // cannot.)
            }
            for i in 0..t.indexes.len() {
                for j in i + 1..t.indexes.len() {
                    // Full equality, NAME included. Two indexes of the same
                    // shape under different names are legal and redundant; two
                    // entries alike in every field are one index written twice.
                    if t.indexes[i] == t.indexes[j] {
                        return Err(Error::Schema(format!(
                            "duplicate index shape on `{}`",
                            t.name
                        )));
                    }
                    // A duplicate NAME, though, is refused whatever the shapes.
                    // The name is an index's IDENTITY: the C-API shim files a
                    // `CREATE INDEX` record under it and `PRAGMA index_list`
                    // reports it, so two indexes sharing one would resolve to a
                    // single record and report a duplicate row. Both appliers
                    // already refuse it on the way in; it belongs here too,
                    // because this is the chokepoint a decoded blob and a
                    // config-seeded schema also pass through, and they do not
                    // go through an applier.
                    if let (Some(a), Some(b)) = (&t.indexes[i].name, &t.indexes[j].name) {
                        if ident_eq(a, b) {
                            return Err(Error::Schema(format!(
                                "duplicate index name `{a}` on `{}`",
                                t.name
                            )));
                        }
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
                // FTS + implicit-rowid IS legal, and is the shape every fts5
                // table created via `CREATE VIRTUAL TABLE` now takes. It was
                // refused here as an untested combination — and the refusal is
                // what forced the vtab's rowid to be a VISIBLE leading column,
                // which the C-API shim then measured as two divergences from
                // sqlite at once (`SELECT *` shape, INSERT arity). The two
                // blocks compose: this one pins the hidden-rowid shape, the
                // FTS block above pins content-is-text, and the fts machinery
                // itself is ordinal-agnostic (`primary_key[0]`,
                // `fts_content_columns` skip the pk wherever it sits).
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
}
